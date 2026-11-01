//! The SQL dump writer (rules: [`crate::sql_dump`] module docs).
//!
//! Every identifier goes through the target's `Quoting` and every value through
//! `Quoting::literal` (the target dialect's escaping); `CREATE` / `DROP` come from the target's
//! `DdlGenerator`. The only engine knowledge here is what the contract names: SQL Server's
//! `SET IDENTITY_INSERT` around inserts into an identity column, and PostgreSQL's
//! `OVERRIDING SYSTEM VALUE` for `GENERATED ALWAYS` identity columns (without it the server
//! refuses the dump's values).

use std::fmt::Write as _;
use std::io::{BufWriter, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use arrow_array::{Array, RecordBatch};
use arrow_schema::{DataType as ArrowType, Schema};
use chrono::{NaiveDateTime, NaiveTime, Timelike};
use oxbase_dialect::{BulkLimits, DdlContext, DdlGenerator, DialectRegistry, DmlTarget, Quoting};
use oxbase_types::{
    ColumnDef, ColumnValue, DataType, Engine, Error, ErrorCategory, Extras, KeyKind, ObjectDef,
    ObjectKind, ObjectRef, Result, RowEdit, RowKey, Script, ServerVersion, TableDef, Value,
};

use super::cells::{TextForms, normalize, normalized_schema, null_at};
use super::{Count, Tally, already_failed, io_error, plural};
use crate::sql_dump::default_version;
use crate::{SinkFile, SinkReport, SinkWriter, SqlDumpOptions, WriteContext, logical_type};

/// How date-time literals are spelled for the target.
///
/// `Quoting::literal` writes them with chrono's `Display` (up to 9 fractional digits, and an
/// offset on timestamps with a zone), which not every target reads:
/// - **SQL Server** refuses more than 3 fractional digits in a string converted to `datetime`
///   (the driver reads `datetime` ticks as `…00:00.003333`) and more than 7 for any type, and
///   reads `yyyy-mm-dd` per `DATEFORMAT` for `datetime`. Timestamps without a zone are written
///   as `CAST(N'yyyy-mm-ddThh:mm:ss.fffffff' AS datetime2(7))`, which every date-time column
///   type takes (rounded to its own precision); times and timestamps with a zone keep at most
///   7 fractional digits.
/// - **MySQL / MariaDB** do not read an offset in a date-time literal (MariaDB never, MySQL
///   before 8.0.19), and a `TIMESTAMP` column reads the text in the session's time zone.
///   Timestamps with a zone are written as their UTC wall-clock time without an offset, and the
///   dump sets `time_zone = '+00:00'` for its statements; at most 6 fractional digits.
/// - Everything else: `Quoting::literal` as it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Temporal {
    Standard,
    MySql,
    MsSql,
}

impl Temporal {
    fn for_engine(engine: Engine) -> Self {
        match engine {
            Engine::MySql | Engine::MariaDb => Self::MySql,
            Engine::MsSql => Self::MsSql,
            _ => Self::Standard,
        }
    }

    /// Fractional-second digits the target keeps (`None`: no limit applied here).
    fn digits(self) -> Option<u32> {
        match self {
            Self::Standard => None,
            Self::MySql => Some(6),
            Self::MsSql => Some(7),
        }
    }
}

/// Appends `.` and the first `digits` fractional digits of `nanos` without trailing zeros
/// (nothing for a whole second). `true` when non-zero digits were dropped.
fn push_fraction(nanos: u32, digits: u32, out: &mut String) -> bool {
    // A leap second carries nanos >= 10^9; it is written as the last instant of the second.
    let nanos = nanos.min(999_999_999);
    let scale = 10_u32.pow(9 - digits);
    let kept = nanos / scale;
    if kept > 0 {
        let width = usize::try_from(digits).unwrap_or(9);
        let mut text = format!("{kept:0width$}");
        while text.ends_with('0') {
            text.pop();
        }
        out.push('.');
        out.push_str(&text);
    }
    !nanos.is_multiple_of(scale)
}

/// `yyyy-mm-dd<sep>hh:mm:ss[.f…]` with at most `digits` fractional digits; `true` when digits
/// were dropped.
fn push_datetime(t: &NaiveDateTime, sep: char, digits: u32, out: &mut String) -> bool {
    let _ = write!(out, "{}{sep}{}", t.format("%Y-%m-%d"), t.format("%H:%M:%S"));
    push_fraction(t.nanosecond(), digits, out)
}

/// `hh:mm:ss[.f…]` with at most `digits` fractional digits; `true` when digits were dropped.
fn push_time(t: NaiveTime, digits: u32, out: &mut String) -> bool {
    let _ = write!(out, "{}", t.format("%H:%M:%S"));
    push_fraction(t.nanosecond(), digits, out)
}

struct SqlSink {
    out: BufWriter<Tally>,
    bytes: Arc<AtomicU64>,
    quoting: Arc<dyn Quoting>,
    forms: TextForms,
    temporal: Temporal,
    /// Date-time values that had more fractional digits than the target keeps.
    cut_fractions: Count,
    /// Statements written after the inserts (MySQL: the session time zone restored).
    epilogue: String,
    /// Batch columns written, in order (generated columns of a same-engine table are left out).
    written: Vec<usize>,
    columns: usize,
    /// `INSERT INTO <table> (<columns>)[ OVERRIDING SYSTEM VALUE] VALUES`.
    insert: String,
    /// `SET IDENTITY_INSERT <table>` (SQL Server identity columns), else `None`.
    identity_insert: Option<String>,
    identity_on: bool,
    rows_per_insert: usize,
    /// Rows of the open `INSERT` statement.
    open: usize,
    line: String,
    rows: u64,
    failed: bool,
    notes: Vec<String>,
}

/// One `--` comment line (line breaks in names would end the comment).
fn comment(text: &str) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    format!("-- {flat}\n")
}

/// The name the dump uses: the option, else the source table's bare name (no database or
/// schema, so the dump loads into the current one), else `export`.
fn dump_name(options: &SqlDumpOptions, source: Option<&ObjectRef>) -> ObjectRef {
    let mut name = match (&options.table, source) {
        (Some(t), _) => t.clone(),
        (None, Some(s)) => ObjectRef::table(None, None, s.name.clone()),
        (None, None) => ObjectRef::table(None, None, "export"),
    };
    name.kind = ObjectKind::Table;
    name.parent = None;
    name.parent_kind = None;
    name.signature = None;
    name
}

/// A table definition from the rows' Arrow types: nullable columns, no keys.
fn from_schema(schema: &Schema, name: ObjectRef) -> TableDef {
    let mut table = TableDef::new(name);
    table.columns = schema
        .fields()
        .iter()
        .map(|f| ColumnDef::new(f.name().clone(), logical_type(f)))
        .collect();
    table
}

/// Removes what is the source engine's SQL (native types, defaults, collations, generated
/// expressions, extras, check expressions, index predicates, expressions and prefix lengths),
/// identity details (kept as plain auto-increment) and the foreign keys, with notes.
fn strip_engine_parts(table: &mut TableDef, notes: &mut Vec<String>) {
    for c in &mut table.columns {
        c.native_type.clear();
        c.default = None;
        c.collation = None;
        c.generated = None;
        c.extras = Extras::default();
        // Identity details are not portable; the target generates values its own way.
        if c.identity.take().is_some() {
            c.auto_increment = true;
        }
    }
    table.extras = Extras::default();
    if !table.foreign_keys.is_empty() {
        notes.push(format!(
            "{} foreign key(s) left out: they refer to tables this dump does not contain",
            table.foreign_keys.len()
        ));
        table.foreign_keys.clear();
    }
    if !table.checks.is_empty() {
        notes.push(format!(
            "{} check constraint(s) left out: their expressions are the source engine's SQL",
            table.checks.len()
        ));
        table.checks.clear();
    }
    let before = table.indexes.len();
    table.indexes.retain(|i| {
        i.predicate.as_deref().is_none_or(|p| p.trim().is_empty())
            && i.columns.iter().all(|c| !c.is_expression)
    });
    for index in &mut table.indexes {
        index.extras = Extras::default();
        for column in &mut index.columns {
            column.length = None;
        }
    }
    if table.indexes.len() < before {
        notes.push(format!(
            "{} partial or expression index(es) left out: they use the source engine's SQL",
            before - table.indexes.len()
        ));
    }
}

/// Types to try when the target has no type for a column's logical type.
fn fallbacks(t: &DataType) -> Vec<DataType> {
    match t {
        DataType::Uuid => vec![DataType::Char { length: Some(36) }, DataType::Text],
        DataType::Text => Vec::new(),
        _ => vec![DataType::Text],
    }
}

/// The `CREATE` script of `table`. When the target's generator refuses it, the definition is
/// simplified step by step, each step with a note: first every column whose type the target
/// cannot declare gets a fallback type (UUID → `char(36)`, anything → text), then comments are
/// left out. The first error is returned when nothing helps.
fn create_script(
    ddl: &dyn DdlGenerator,
    ctx: &DdlContext<'_>,
    table: &mut TableDef,
    target: Engine,
    notes: &mut Vec<String>,
) -> Result<Script> {
    let create = |t: &TableDef| ddl.create(&ObjectDef::Table(t.clone()), ctx);
    let first = match create(table) {
        Ok(script) => return Ok(script),
        Err(e) => e,
    };
    // Whether the target can declare a column of this type at all (keys, defaults, identity
    // and comments aside).
    let declares = |column: &ColumnDef| {
        let mut bare = column.clone();
        bare.comment = None;
        bare.default = None;
        bare.generated = None;
        bare.identity = None;
        bare.auto_increment = false;
        bare.nullable = true;
        let mut probe = TableDef::new(table.obj.clone());
        probe.columns.push(bare);
        create(&probe).is_ok()
    };
    let mut candidate = table.clone();
    let mut changes = Vec::new();
    for (i, column) in table.columns.iter().enumerate() {
        if declares(column) {
            continue;
        }
        for fallback in fallbacks(&column.data_type) {
            let mut changed = column.clone();
            changed.data_type = fallback;
            changed.native_type.clear();
            if declares(&changed) {
                changes.push(format!(
                    "column {}: {} has no {} type; declared as {}",
                    column.name,
                    column.data_type,
                    target.display_name(),
                    changed.data_type
                ));
                candidate.columns[i] = changed;
                break;
            }
        }
    }
    let script = if let Ok(script) = create(&candidate) {
        script
    } else {
        let commented =
            candidate.comment.is_some() || candidate.columns.iter().any(|c| c.comment.is_some());
        if !commented {
            return Err(first);
        }
        candidate.comment = None;
        for column in &mut candidate.columns {
            column.comment = None;
        }
        let Ok(script) = create(&candidate) else {
            return Err(first);
        };
        changes.push(format!(
            "comments left out: {} refused them",
            target.display_name()
        ));
        script
    };
    notes.extend(changes);
    *table = candidate;
    Ok(script)
}

/// Whether the target's grid writer refuses to write `column` of `table` (a column the server
/// maintains: generated, SQL Server `rowversion` or period, ...). Identity and auto-increment
/// columns are probed as plain columns, because the dump writes them on purpose
/// (`SET IDENTITY_INSERT`, `OVERRIDING SYSTEM VALUE`). Only a refusal (`InvalidInput`) counts;
/// any other error says nothing about the column.
fn refused_by_writer(
    dml: &dyn oxbase_dialect::DmlWriter,
    table: &TableDef,
    column: &ColumnDef,
    version: &ServerVersion,
) -> bool {
    let mut probed = column.clone();
    probed.identity = None;
    probed.auto_increment = false;
    let mut key_name = "oxbase_probe_key".to_owned();
    while key_name == probed.name {
        key_name.push('_');
    }
    let mut key_column = ColumnDef::new(key_name.clone(), DataType::INT64);
    key_column.nullable = false;
    let mut probe = TableDef::new(table.obj.clone());
    probe.columns = vec![key_column, probed];
    let key = RowKey::new(table.obj.clone(), KeyKind::Primary, vec![key_name]);
    let edit = RowEdit::Insert {
        values: vec![ColumnValue::new(column.name.clone(), Value::Null)],
    };
    let target = DmlTarget {
        table: &probe,
        key: &key,
        version,
        read_back: &[],
    };
    dml.plan(&edit, &target)
        .is_err_and(|e| e.category() == ErrorCategory::InvalidInput)
}

/// Names of the columns of a same-engine `table` the server maintains itself (see
/// [`refused_by_writer`]); generated columns always count, whether or not the target has a
/// grid writer.
fn maintained_columns(
    dialects: &DialectRegistry,
    target: Engine,
    table: &TableDef,
    version: &ServerVersion,
) -> Vec<String> {
    let dml = dialects.dml_writer(target).ok();
    table
        .columns
        .iter()
        .filter(|c| {
            c.generated.is_some()
                || dml
                    .as_deref()
                    .is_some_and(|dml| refused_by_writer(dml, table, c, version))
        })
        .map(|c| c.name.clone())
        .collect()
}

/// The SQL dump writer over `file`.
pub(crate) fn writer(
    file: SinkFile,
    schema: &Schema,
    options: &SqlDumpOptions,
    context: &WriteContext,
) -> Result<Box<dyn SinkWriter>> {
    let target = options.target;
    let engine = target.display_name();
    let dialects = context.dialects.as_ref().ok_or_else(|| {
        Error::unsupported(format!(
            "a {engine} SQL dump needs the {engine} dialect cells"
        ))
    })?;
    let quoting = dialects.quoting(target)?;
    let version = match options.target_version.as_deref().map(str::trim) {
        Some(text) if !text.is_empty() => ServerVersion::parse(text)?,
        _ => default_version(target)
            .ok_or_else(|| Error::unsupported(format!("{engine} has no SQL dump")))?,
    };
    let ctx = DdlContext::new(&version);
    let schema = normalized_schema(schema);

    let mut notes = Vec::new();
    let same_engine = context.source_engine == Some(target);
    let matching = context
        .table
        .as_ref()
        .filter(|t| schema.fields().iter().all(|f| t.column(f.name()).is_some()));
    if context.table.is_some() && matching.is_none() {
        notes.push(
            "the table definition does not match the exported columns; the table was built \
             from the column types"
                .to_owned(),
        );
    }
    let mut table = match matching {
        Some(def) => {
            let mut table = def.clone();
            table.obj = dump_name(options, Some(&def.obj));
            if !same_engine {
                notes.push(format!(
                    "written for {engine} from another engine: native types, defaults, \
                     collations and generated expressions were dropped"
                ));
                strip_engine_parts(&mut table, &mut notes);
            }
            table
        }
        None => from_schema(
            &schema,
            dump_name(options, context.table.as_ref().map(|t| &t.obj)),
        ),
    };

    let mut sql = String::new();
    if options.include_create {
        let ddl = dialects.ddl(target, ObjectKind::Table)?;
        let create = create_script(ddl.as_ref(), &ctx, &mut table, target, &mut notes)?;
        if options.include_drop {
            sql.push_str(&ddl.drop_object(&table.obj, &ctx)?.to_sql(";"));
        }
        sql.push_str(&create.to_sql(";"));
        for warning in &create.warnings {
            notes.push(warning.clone());
        }
    } else if options.include_drop {
        let ddl = dialects.ddl(target, ObjectKind::Table)?;
        sql.push_str(&ddl.drop_object(&table.obj, &ctx)?.to_sql(";"));
    }

    // Columns written: all of the batch's, except the columns of a same-engine table the
    // server maintains (it computes them and refuses values). Across engines the generated
    // expressions and extras were cleared above, so every column is a plain one.
    let maintained = if same_engine && matching.is_some() {
        maintained_columns(dialects, target, &table, &version)
    } else {
        Vec::new()
    };
    let written: Vec<usize> = schema
        .fields()
        .iter()
        .enumerate()
        .filter(|(_, f)| !maintained.iter().any(|m| m == f.name()))
        .map(|(i, _)| i)
        .collect();
    for name in &maintained {
        if schema.field_with_name(name).is_ok() {
            notes.push(format!(
                "column {name} left out of the inserts: the server maintains its values"
            ));
        }
    }
    let written_defs: Vec<&ColumnDef> = written
        .iter()
        .filter_map(|&i| table.column(schema.field(i).name()))
        .collect();
    let qualified = quoting.qualify(&table.obj);
    let mut insert = format!("INSERT INTO {qualified} (");
    for (n, &i) in written.iter().enumerate() {
        if n > 0 {
            insert.push_str(", ");
        }
        insert.push_str(&quoting.quote_ident(schema.field(i).name()));
    }
    insert.push(')');
    if target == Engine::Postgres
        && written_defs
            .iter()
            .any(|c| c.identity.as_ref().is_some_and(|i| i.always))
    {
        insert.push_str(" OVERRIDING SYSTEM VALUE");
    }
    insert.push_str(" VALUES\n");
    let identity_insert = (target == Engine::MsSql
        && written_defs
            .iter()
            .any(|c| c.identity.is_some() || c.auto_increment))
    .then(|| format!("SET IDENTITY_INSERT {qualified}"));

    let temporal = Temporal::for_engine(target);
    let mut head = comment("Oxbase SQL dump");
    head.push_str(&comment(&format!("Target: {engine} {version}")));
    for note in &notes {
        head.push_str(&comment(note));
    }
    head.push('\n');
    // MySQL / MariaDB: timestamps with a zone are written as UTC wall-clock times, which a
    // `TIMESTAMP` column reads in the session's time zone.
    let mut epilogue = String::new();
    let zoned = schema
        .fields()
        .iter()
        .any(|f| matches!(f.data_type(), ArrowType::Timestamp(_, Some(_))));
    if temporal == Temporal::MySql && zoned && !written.is_empty() {
        head.push_str(
            "SET @oxbase_time_zone = @@session.time_zone;\nSET time_zone = '+00:00';\n\n",
        );
        epilogue.push_str("SET time_zone = @oxbase_time_zone;\n");
    }
    if !sql.is_empty() {
        head.push_str(&sql);
        head.push('\n');
    }

    let tally = Tally::new(file);
    let bytes = tally.counter();
    let mut out = BufWriter::with_capacity(64 * 1024, tally);
    out.write_all(head.as_bytes()).map_err(|e| io_error(&e))?;
    let rows_per_insert = options
        .rows_per_insert
        .min(BulkLimits::for_engine(target).max_rows_per_statement)
        .max(1);
    Ok(Box::new(SqlSink {
        out,
        bytes,
        quoting,
        forms: TextForms::default(),
        temporal,
        cut_fractions: Count::default(),
        epilogue,
        written,
        columns: schema.fields().len(),
        insert,
        identity_insert,
        identity_on: false,
        rows_per_insert,
        open: 0,
        line: String::new(),
        rows: 0,
        failed: false,
        notes,
    }))
}

impl SqlSink {
    /// The literal of cell `row` of `array`: `Quoting::literal` of `cell_value`, or of the
    /// value's text for Arrow types `cell_value` does not cover; date-time values as
    /// [`Temporal`] says. The flag is `true` when fractional digits were dropped.
    fn literal(&self, array: &dyn Array, row: usize) -> Result<(String, bool)> {
        let value = match oxbase_driver::cell_value(array, row) {
            Ok(v) => v,
            Err(Error::Unsupported { .. }) => {
                let mut text = String::new();
                self.forms.push_text(array, row, &mut text)?;
                Value::Text(text)
            }
            Err(e) => return Err(e),
        };
        Ok(self
            .temporal_literal(&value)
            .unwrap_or_else(|| (self.quoting.literal(&value), false)))
    }

    /// The target's spelling of a date-time `value` ([`Temporal`]) and whether fractional
    /// digits were dropped; `None` when `Quoting::literal` is right.
    fn temporal_literal(&self, value: &Value) -> Option<(String, bool)> {
        let digits = self.temporal.digits()?;
        let mut text = String::new();
        let (cut, cast) = match (self.temporal, value) {
            (Temporal::MsSql, Value::Timestamp(t)) => {
                (push_datetime(t, 'T', digits, &mut text), true)
            }
            (Temporal::MsSql, Value::TimestampTz(t)) => {
                let cut = push_datetime(&t.naive_local(), 'T', digits, &mut text);
                text.push_str(&t.offset().to_string());
                (cut, false)
            }
            (Temporal::MySql, Value::Timestamp(t)) => {
                (push_datetime(t, ' ', digits, &mut text), false)
            }
            // The UTC wall-clock time; the dump runs with `time_zone = '+00:00'`.
            (Temporal::MySql, Value::TimestampTz(t)) => {
                (push_datetime(&t.naive_utc(), ' ', digits, &mut text), false)
            }
            (_, Value::Time(t)) => (push_time(*t, digits, &mut text), false),
            _ => return None,
        };
        let quoted = self.quoting.quote_str(&text);
        let literal = if cast {
            format!("CAST({quoted} AS datetime2(7))")
        } else {
            quoted
        };
        Some((literal, cut))
    }

    fn write_batch(&mut self, batch: &RecordBatch) -> Result<()> {
        let batch = normalize(batch)?;
        if batch.num_columns() != self.columns {
            return Err(Error::invalid_input(format!(
                "a batch has {} columns, the file {}",
                batch.num_columns(),
                self.columns
            )));
        }
        if self.written.is_empty() {
            self.rows += u64::try_from(batch.num_rows()).unwrap_or(u64::MAX);
            return Ok(());
        }
        for row in 0..batch.num_rows() {
            let mut line = std::mem::take(&mut self.line);
            line.clear();
            if self.open == 0 {
                if let Some(set) = &self.identity_insert
                    && !self.identity_on
                {
                    line.push_str(set);
                    line.push_str(" ON;\n");
                    self.identity_on = true;
                }
                line.push_str(&self.insert);
                line.push('(');
            } else {
                line.push_str(",\n(");
            }
            for (n, &c) in self.written.iter().enumerate() {
                if n > 0 {
                    line.push_str(", ");
                }
                let column = batch.column(c);
                if null_at(column.as_ref(), row) {
                    line.push_str("NULL");
                } else {
                    let (literal, cut) = self.literal(column.as_ref(), row)?;
                    if cut {
                        self.cut_fractions.add();
                    }
                    line.push_str(&literal);
                }
            }
            line.push(')');
            self.open += 1;
            if self.open >= self.rows_per_insert {
                line.push_str(";\n");
                self.open = 0;
            }
            self.out
                .write_all(line.as_bytes())
                .map_err(|e| io_error(&e))?;
            self.line = line;
            self.rows += 1;
        }
        Ok(())
    }
}

impl SinkWriter for SqlSink {
    fn write(&mut self, batch: &RecordBatch) -> Result<()> {
        if self.failed {
            return Err(already_failed());
        }
        let result = self.write_batch(batch);
        self.failed = result.is_err();
        result
    }

    fn rows_written(&self) -> u64 {
        self.rows
    }

    fn bytes_written(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    fn finish(self: Box<Self>) -> Result<SinkReport> {
        let mut this = *self;
        if this.failed {
            return Err(already_failed());
        }
        let mut tail = String::new();
        if this.open > 0 {
            tail.push_str(";\n");
        }
        if let Some(set) = &this.identity_insert
            && this.identity_on
        {
            tail.push_str(set);
            tail.push_str(" OFF;\n");
        }
        tail.push_str(&this.epilogue);
        this.out
            .write_all(tail.as_bytes())
            .map_err(|e| io_error(&e))?;
        let tally = this.out.into_inner().map_err(|e| io_error(e.error()))?;
        let bytes = tally.into_file().commit()?;
        let digits = this.temporal.digits().unwrap_or(9);
        this.cut_fractions.note(&mut this.notes, |n| {
            format!(
                "{n} date-time value{} had more than {digits} fractional second digits; the \
                 target keeps {digits}, so the rest were dropped",
                plural(n)
            )
        });
        Ok(SinkReport {
            rows: this.rows,
            bytes,
            sheets: Vec::new(),
            warnings: this.notes,
        })
    }

    fn abort(self: Box<Self>) {
        let this = *self;
        let (tally, _) = this.out.into_parts();
        tally.into_file().abort();
    }
}
