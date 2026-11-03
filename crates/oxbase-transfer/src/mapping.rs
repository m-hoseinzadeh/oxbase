//! Column and table mapping across engines (`docs/m8-contracts.md` §4.1, §4.3): the pure part
//! of a transfer plan. Owner of the bodies marked `unimplemented!("m8: transfer")`: the
//! **transfer** stream.
//!
//! # Rules
//!
//! - **Same engine** (source and target engine equal): every column keeps its native type
//!   ([`TypeMapper::same_engine`]), fidelity `exact`; the target table is the source
//!   [`TableDef`] relocated to the target container (`oxbase_types::relocate_def`), with
//!   defaults, collations, generated columns, checks, comments and extras kept.
//! - **Across engines**: logical type = the source engine's [`TypeMapper::logical`]; target =
//!   the target engine's [`TypeMapper::map`]. The target `TableDef` keeps names, nullability,
//!   the primary key, unique and plain indexes on plain columns (when
//!   `TransferOptions::create_indexes`), and auto-increment / identity (as the target's
//!   auto-increment); it **drops** engine-native text that would not parse on the target
//!   (defaults other than numeric / string / boolean literals, collations, generated
//!   expressions, checks, expression and partial indexes, extras), each with a plan warning.
//! - **Overrides** ([`ColumnOverride`]) win: `skip`, `target_name`, `data_type` (mapped with
//!   the target's `map` for its native text unless `native_type` is given). An overridden
//!   column's fidelity is computed against its source logical type with [`fidelity_between`].
//! - **Foreign keys** between transferred tables are planned separately
//!   ([`TablePlan::foreign_keys`]) and added after every table is loaded; a foreign key to a
//!   table outside the transfer is dropped with a warning.
//! - **Append / truncate** into an existing table: columns are matched by target name; the
//!   existing column's type wins, and the mapping's fidelity compares the source logical type
//!   with the existing column's logical type.
//! - A table whose plan has an `unsupported` column cannot run until the column is skipped or
//!   overridden (`TransferPlan::runnable` is false and says why).

use oxbase_dialect::{Fidelity, TypeMapper, TypeMapping};
use oxbase_types::{
    CaseRules, ColumnDef, DataType, Engine, Error, Extras, IdentCase, IndexColumn, ObjectRef,
    PrimaryKeyDef, Result, ServerVersion, TableDef,
};
use serde::{Deserialize, Serialize};

use crate::cells::{TargetRules, TextDefaults};
use crate::spec::{ColumnOverride, TransferOptions};

/// The two engines and their mappers.
#[derive(Clone, Copy)]
pub struct MappingContext<'a> {
    pub source_engine: Engine,
    pub source_version: &'a ServerVersion,
    pub source_mapper: &'a dyn TypeMapper,
    pub target_engine: Engine,
    pub target_version: &'a ServerVersion,
    pub target_mapper: &'a dyn TypeMapper,
}

impl MappingContext<'_> {
    /// Source and target are the same engine (module docs, *Same engine*).
    pub fn same_engine(&self) -> bool {
        self.source_engine == self.target_engine
    }

    /// What the target engine accepts beyond its type mapping ([`crate::cells::target_rules`]).
    pub fn target_rules(&self) -> TargetRules {
        crate::cells::target_rules(self.target_engine, self.target_version)
    }
}

/// How one source column is copied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ColumnPlan {
    pub source: String,
    /// The source column's logical type ([`TypeMapper::logical`]).
    pub source_type: DataType,
    /// The source column's native type text (empty for query results).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source_native: String,
    /// The target column name.
    pub target: String,
    /// What the target column is (logical type, native text, fidelity, reason).
    pub mapping: TypeMapping,
    /// A user override decided this column.
    #[serde(default)]
    pub overridden: bool,
    /// Left out of the load (override, or `IdentityMode::Generate` on an identity column).
    #[serde(default)]
    pub skipped: bool,
}

/// Longest text form of a value of `t`, in characters (`None`: no bound).
fn text_width(t: &DataType) -> Option<u32> {
    Some(match t {
        DataType::Bool => 5,
        DataType::Int { bits, .. } => match bits {
            0..=8 => 4,
            9..=16 => 6,
            17..=32 => 11,
            _ => 20,
        },
        DataType::Float { .. } => 24,
        DataType::Decimal {
            precision: Some(p), ..
        } => p.saturating_add(2),
        DataType::Char { length: Some(n) } | DataType::VarChar { length: Some(n) } => *n,
        DataType::Date => 13,
        DataType::Time { .. } => 15,
        DataType::Timestamp { .. } => 29,
        DataType::TimestampTz { .. } => 35,
        DataType::Uuid => 36,
        DataType::Enum { labels } => labels
            .iter()
            .map(|l| u32::try_from(l.chars().count()).unwrap_or(u32::MAX))
            .max()
            .unwrap_or(0),
        _ => return None,
    })
}

/// `(min, max)` of an integer type.
fn int_range(bits: u8, unsigned: bool) -> (i128, i128) {
    let bits = u32::from(bits.clamp(1, 64));
    if unsigned {
        (0, (1_i128 << bits) - 1)
    } else {
        (-(1_i128 << (bits - 1)), (1_i128 << (bits - 1)) - 1)
    }
}

/// Decimal digits of the widest value of an integer type.
fn int_digits(bits: u8, unsigned: bool) -> u32 {
    let (min, max) = int_range(bits, unsigned);
    let widest = max.max(-min);
    u32::try_from(widest.to_string().len()).unwrap_or(u32::MAX)
}

/// The fidelity of storing values of logical type `from` in a column of logical type `to`
/// (for overrides and for appends into existing columns): `exact` when equal; `widened` when
/// every value of `from` fits `to` (wider integers / decimals / varchar lengths, any type into
/// text or JSON); `narrowed` when values can be cut or refused; `unsupported` when there is no
/// conversion at all (binary into a number, JSON into a date). Owner: transfer stream.
pub fn fidelity_between(from: &DataType, to: &DataType) -> (Fidelity, Option<String>) {
    use DataType as T;
    if from == to {
        return (Fidelity::Exact, None);
    }
    let widened = |reason: String| (Fidelity::Widened, Some(reason));
    let narrowed = |reason: String| (Fidelity::Narrowed, Some(reason));
    let unsupported = || {
        (
            Fidelity::Unsupported,
            Some(format!("values of {from} cannot be stored as {to}")),
        )
    };
    match (from, to) {
        // Into unbounded text: every value as its text form.
        (_, T::Text | T::VarChar { length: None }) => {
            widened(format!("{from} values are stored as text"))
        }
        (_, T::Char { length: Some(n) } | T::VarChar { length: Some(n) }) => {
            match text_width(from) {
                Some(w) if w <= *n => widened(format!("{from} values fit {to}")),
                _ => narrowed(format!("texts longer than {n} characters are refused")),
            }
        }
        (_, T::Char { length: None }) => {
            narrowed("texts are cut to the column's fixed length".to_owned())
        }
        (
            T::Int { bits, unsigned },
            T::Int {
                bits: tb,
                unsigned: tu,
            },
        ) => {
            let (fmin, fmax) = int_range(*bits, *unsigned);
            let (tmin, tmax) = int_range(*tb, *tu);
            if tmin <= fmin && fmax <= tmax {
                widened(format!("every {from} value fits {to}"))
            } else {
                narrowed(format!("values outside the range of {to} are refused"))
            }
        }
        (T::Bool, T::Int { .. } | T::Decimal { .. }) => {
            widened("true / false are stored as 1 / 0".to_owned())
        }
        (T::Int { .. } | T::Decimal { .. }, T::Bool) => {
            narrowed("only 0 and 1 are kept (as false / true); other values are refused".to_owned())
        }
        (T::Int { bits, unsigned }, T::Decimal { precision, scale }) => match precision {
            Some(p) if p.saturating_sub(scale.unwrap_or(0)) < int_digits(*bits, *unsigned) => {
                narrowed(format!("values that do not fit {to} are refused"))
            }
            _ => widened(format!("every {from} value fits {to}")),
        },
        (T::Int { bits, .. }, T::Float { bits: fb }) => {
            if (*bits <= 16 && *fb >= 32) || (*bits <= 32 && *fb >= 64) {
                widened(format!("every {from} value fits {to}"))
            } else {
                narrowed("large integers lose precision".to_owned())
            }
        }
        (T::Float { bits }, T::Float { bits: tb }) => {
            if tb >= bits {
                widened(format!("every {from} value fits {to}"))
            } else {
                narrowed(format!("values are rounded to {to}"))
            }
        }
        (
            T::Decimal {
                precision: fp,
                scale: fs,
            },
            T::Decimal {
                precision: tp,
                scale: ts,
            },
        ) => {
            let fits = match (fp, tp) {
                (_, None) => true,
                (None, Some(_)) => false,
                (Some(fp), Some(tp)) => {
                    let (fs, ts) = (fs.unwrap_or(0), ts.unwrap_or(0));
                    ts >= fs && tp.saturating_sub(ts) >= fp.saturating_sub(fs)
                }
            };
            if fits {
                widened(format!("every {from} value fits {to}"))
            } else {
                narrowed(format!(
                    "values that do not fit {to} (too many digits or decimal places) are refused"
                ))
            }
        }
        (T::Decimal { .. }, T::Float { .. }) => narrowed(format!(
            "values are rounded to {to}; values outside its range are refused"
        )),
        (T::Float { .. } | T::Decimal { .. }, T::Int { .. } | T::Decimal { .. }) => narrowed(
            format!("values with a fraction {to} cannot hold, or outside its range, are refused"),
        ),
        (_, T::Json { .. }) => match from {
            T::Json { .. } => (Fidelity::Exact, None),
            T::Char { .. } | T::VarChar { .. } | T::Text | T::Other { .. } => {
                narrowed("texts that are not valid JSON are refused".to_owned())
            }
            _ => widened(format!("{from} values are stored as JSON")),
        },
        (T::Enum { labels: fl }, T::Enum { labels: tl }) => {
            if fl.iter().all(|l| tl.contains(l)) {
                widened(format!("every label is one of {to}"))
            } else {
                narrowed(format!("values that are not labels of {to} are refused"))
            }
        }
        (_, T::Enum { .. }) => narrowed(format!("values that are not labels of {to} are refused")),
        (
            T::Binary { length: fl } | T::VarBinary { length: fl },
            T::Binary { length: tl } | T::VarBinary { length: tl },
        ) => {
            let fits = match (fl, tl) {
                (_, None) => true,
                (Some(f), Some(t)) => f <= t,
                (None, Some(_)) => false,
            };
            if fits {
                widened(format!("every {from} value fits {to}"))
            } else {
                narrowed(format!("values longer than {to} are refused"))
            }
        }
        (T::Uuid, T::Binary { length } | T::VarBinary { length }) => {
            if length.is_none_or(|n| n >= 16) {
                widened("the 16 bytes of the UUID are stored".to_owned())
            } else {
                unsupported()
            }
        }
        (T::Binary { .. } | T::VarBinary { .. }, T::Uuid) => {
            narrowed("values that are not 16 bytes long are refused".to_owned())
        }
        (T::Date, T::Timestamp { .. } | T::TimestampTz { .. }) => {
            widened("dates are stored at midnight".to_owned())
        }
        (T::Timestamp { .. } | T::TimestampTz { .. }, T::Date) => {
            narrowed("the time of day is dropped".to_owned())
        }
        (T::Timestamp { precision: fp }, T::Timestamp { precision: tp })
        | (T::TimestampTz { precision: fp }, T::TimestampTz { precision: tp })
        | (T::Time { precision: fp }, T::Time { precision: tp }) => {
            if tp.unwrap_or(6) >= fp.unwrap_or(6) {
                widened(format!("every {from} value fits {to}"))
            } else {
                narrowed(format!("fractional seconds are cut to {to}"))
            }
        }
        (T::Timestamp { .. }, T::TimestampTz { .. }) => {
            narrowed("timestamps without a time zone are read as UTC".to_owned())
        }
        (T::TimestampTz { .. }, T::Timestamp { .. }) => {
            narrowed("instants are stored in UTC: the offset is dropped".to_owned())
        }
        (T::Timestamp { .. } | T::TimestampTz { .. }, T::Time { .. }) => {
            narrowed("only the time of day is kept".to_owned())
        }
        (T::Array { element: fe }, T::Array { element: te }) => {
            let (fidelity, reason) = fidelity_between(fe, te);
            (fidelity, reason.map(|r| format!("array elements: {r}")))
        }
        (T::Char { .. } | T::VarChar { .. } | T::Text | T::Other { .. } | T::Enum { .. }, _)
        | (_, T::Other { .. }) => narrowed(format!(
            "values are read from their text form as {to}; values that do not parse are refused"
        )),
        _ => unsupported(),
    }
}

/// The plan of one column: same-engine, cross-engine or overridden (module docs).
pub fn map_column(
    ctx: &MappingContext<'_>,
    column: &ColumnDef,
    column_override: Option<&ColumnOverride>,
) -> ColumnPlan {
    let source_type = ctx.source_mapper.logical(column, ctx.source_version);
    let mut mapping = if ctx.same_engine() {
        ctx.target_mapper.same_engine(column, ctx.target_version)
    } else {
        ctx.target_mapper.map(&source_type, ctx.target_version)
    };
    let typed_by_user = column_override.is_some_and(|o| o.data_type.is_some());
    if !ctx.same_engine()
        && column.auto_increment
        && !typed_by_user
        && matches!(source_type, DataType::Int { unsigned: true, .. })
        && !matches!(mapping.data_type, DataType::Int { .. })
    {
        // An unsigned 64-bit id (MySQL `bigint unsigned AUTO_INCREMENT`) has no integer home
        // on most engines; a signed bigint keeps it generated on the target.
        let signed = ctx.target_mapper.map(&DataType::INT64, ctx.target_version);
        if matches!(signed.data_type, DataType::Int { .. }) {
            mapping = TypeMapping {
                data_type: signed.data_type,
                native_type: signed.native_type,
                fidelity: Fidelity::Narrowed,
                reason: Some(
                    "an auto-increment column stays an integer so the target generates its \
                     values: ids above 9223372036854775807 are refused"
                        .to_owned(),
                ),
            };
        }
    }
    let mut overridden = false;
    let mut skipped = false;
    let mut target = column.name.clone();
    if let Some(o) = column_override {
        overridden =
            o.skip || o.target_name.is_some() || o.data_type.is_some() || o.native_type.is_some();
        skipped = o.skip;
        if let Some(name) = o.target_name.as_deref().filter(|n| !n.trim().is_empty()) {
            name.clone_into(&mut target);
        }
        if let Some(data_type) = &o.data_type {
            let mapped = ctx.target_mapper.map(data_type, ctx.target_version);
            let (fidelity, reason) = if mapped.fidelity == Fidelity::Unsupported {
                (Fidelity::Unsupported, mapped.reason)
            } else {
                fidelity_between(&source_type, &mapped.data_type)
            };
            mapping = TypeMapping {
                data_type: mapped.data_type,
                native_type: o.native_type.clone().unwrap_or(mapped.native_type),
                fidelity,
                reason,
            };
        } else if let Some(native) = &o.native_type {
            native.clone_into(&mut mapping.native_type);
        }
    }
    ColumnPlan {
        source: column.name.clone(),
        source_type,
        source_native: column.native_type.clone(),
        target,
        mapping,
        overridden,
        skipped,
    }
}

/// `true` for the logical types of columns an index cannot cover on every engine (MySQL
/// needs a prefix length, SQL Server refuses `(max)` key columns): unbounded text and binary,
/// JSON, arrays and engine-specific types.
fn unbounded(t: &DataType) -> bool {
    matches!(
        t,
        DataType::Text
            | DataType::VarChar { length: None }
            | DataType::Char { length: None }
            | DataType::VarBinary { length: None }
            | DataType::Binary { length: None }
            | DataType::Json { .. }
            | DataType::Array { .. }
            | DataType::Other { .. }
    )
}

/// `true` for unbounded text (a `text` / `longtext` / `nvarchar(max)` column).
fn unbounded_text(t: &DataType) -> bool {
    matches!(
        t,
        DataType::Text | DataType::VarChar { length: None } | DataType::Char { length: None }
    )
}

/// `true` for the types no index of a created table covers on any engine: JSON (PostgreSQL
/// `json` has no B-tree ordering), arrays and engine-specific types.
fn never_indexed(t: &DataType) -> bool {
    matches!(
        t,
        DataType::Json { .. } | DataType::Array { .. } | DataType::Other { .. }
    )
}

/// Bytes one value of `t` takes in an index key, for the target's key-size limit (text at
/// `bytes_per_char` per character; a rough upper bound for the rest).
fn key_bytes(t: &DataType, bytes_per_char: u32) -> u64 {
    let n = match t {
        DataType::Char { length: Some(n) } | DataType::VarChar { length: Some(n) } => {
            n.saturating_mul(bytes_per_char)
        }
        DataType::Binary { length: Some(n) } | DataType::VarBinary { length: Some(n) } => *n,
        DataType::Bool | DataType::Enum { .. } => 2,
        DataType::Int { bits, .. } => u32::from(*bits / 8).max(1),
        DataType::Uuid => 16,
        DataType::Decimal { .. } => 32,
        _ => 8,
    };
    u64::from(n)
}

/// The name of constraint or index `name` of table `source` on table `target` when the name
/// must be new (another engine, or a copy in the source's own schema): a name that starts with
/// the source table's name gets the target table's name in its place, any other is prefixed
/// with it (index and constraint names are per schema on some engines and per table on
/// others); at most 60 characters.
pub(crate) fn constraint_name(source: &str, target: &str, name: &str) -> String {
    let full = match name.strip_prefix(source) {
        Some(rest) => format!("{target}{rest}"),
        None => format!("{target}_{name}"),
    };
    full.chars().take(60).collect()
}

/// `true` when `a` and `b` live in the same database and schema (a copy next to its source:
/// index and constraint names must be new there).
pub(crate) fn same_container(a: &ObjectRef, b: &ObjectRef) -> bool {
    a.catalog == b.catalog && a.schema == b.schema
}

/// A literal default that every SQL engine reads the same way, for a column of `target`
/// type: a plain number (numeric columns) or a simple string literal (text, temporal and
/// UUID columns), or a boolean literal (`true`, `1`, `'t'`, ...) written the target's way
/// ([`TargetRules::bool_literals`]). PostgreSQL casts (`'x'::text`), SQL Server parentheses
/// (`((0))`) and `N` prefixes are taken off. `None` when the default is anything else.
fn portable_default(default: &str, target: &DataType, rules: &TargetRules) -> Option<String> {
    let mut text = default.trim();
    while let Some(inner) = text.strip_prefix('(').and_then(|t| t.strip_suffix(')')) {
        text = inner.trim();
    }
    // `'x'::text`, `0::integer`, `'a'::character varying`.
    if let Some(pos) = text.rfind("::")
        && !text[pos..].contains('\'')
    {
        text = text[..pos].trim();
    }
    let text = text
        .strip_prefix(['N', 'n'])
        .filter(|t| t.starts_with('\''))
        .unwrap_or(text);
    let numeric = {
        let digits = text.strip_prefix('-').unwrap_or(text);
        !digits.is_empty()
            && digits.bytes().next().is_some_and(|b| b.is_ascii_digit())
            && digits.bytes().all(|b| b.is_ascii_digit() || b == b'.')
            && digits.bytes().filter(|b| *b == b'.').count() <= 1
    };
    if matches!(target, DataType::Bool) {
        let truth = match text.to_ascii_lowercase().as_str() {
            "true" | "1" | "'t'" | "'true'" | "'1'" | "b'1'" => true,
            "false" | "0" | "'f'" | "'false'" | "'0'" | "b'0'" => false,
            _ => return None,
        };
        let (yes, no) = rules.bool_literals;
        return Some(if truth { yes } else { no }.to_owned());
    }
    let string = text.len() >= 2
        && text.starts_with('\'')
        && text.ends_with('\'')
        && !text.contains('\\')
        && !text[1..text.len() - 1].replace("''", "").contains('\'');
    match target {
        DataType::Int { .. } | DataType::Float { .. } | DataType::Decimal { .. } if numeric => {
            Some(text.to_owned())
        }
        DataType::Char { .. }
        | DataType::VarChar { .. }
        | DataType::Text
        | DataType::Enum { .. }
        | DataType::Date
        | DataType::Time { .. }
        | DataType::Timestamp { .. }
        | DataType::TimestampTz { .. }
        | DataType::Uuid
            if string =>
        {
            Some(text.to_owned())
        }
        _ => None,
    }
}

/// Bounds the primary-key columns of `source` whose target type is unbounded text or binary
/// when the target cannot key such a column ([`TargetRules::keys_on_unbounded`]): they become
/// `varchar(n)` / `varbinary(n)` (`n` = [`TargetRules::key_text_length`]), narrowed, so the
/// key survives and longer values are refused per row. Across engines only; overridden and
/// skipped columns are left alone ([`target_table`] then leaves the key out with a warning).
pub fn bound_key_columns(ctx: &MappingContext<'_>, source: &TableDef, columns: &mut [ColumnPlan]) {
    let rules = ctx.target_rules();
    if ctx.same_engine() || rules.keys_on_unbounded {
        return;
    }
    let Some(pk) = &source.primary_key else {
        return;
    };
    let n = rules.key_text_length;
    for plan in columns
        .iter_mut()
        .filter(|p| !p.skipped && !p.overridden && pk.columns.contains(&p.source))
    {
        let bounded = match plan.mapping.data_type {
            DataType::Text
            | DataType::VarChar { length: None }
            | DataType::Char { length: None } => DataType::VarChar { length: Some(n) },
            DataType::VarBinary { length: None } | DataType::Binary { length: None } => {
                DataType::VarBinary { length: Some(n) }
            }
            _ => continue,
        };
        let mapped = ctx.target_mapper.map(&bounded, ctx.target_version);
        if mapped.fidelity == Fidelity::Unsupported || unbounded(&mapped.data_type) {
            continue;
        }
        let (fidelity, _) = fidelity_between(&plan.source_type, &mapped.data_type);
        let reason = format!(
            "a primary-key column: {} cannot key unbounded text or binary, so it is created as \
             {}; longer values are refused",
            ctx.target_engine.display_name(),
            mapped.data_type
        );
        plan.mapping = TypeMapping {
            data_type: mapped.data_type,
            native_type: mapped.native_type,
            fidelity: fidelity.worst(Fidelity::Narrowed),
            reason: Some(reason),
        };
    }
}

/// The table to create on the target for `source` (named `target`), from its column plans
/// (module docs): relocated for the same engine, rebuilt with mapped types across engines,
/// without foreign keys (they are added after the load). Returns the definition and its
/// warnings (dropped defaults, checks, indexes, ...).
///
/// `columns` holds one plan per source column (in any order; matched by source name). A
/// skipped column is left out of the table, except an auto-increment column (skipped because
/// the target generates its values) and, on the same engine, a computed column (skipped
/// because the target computes it), which stay.
///
/// Index and constraint names are made new ([`constraint_name`]; the primary key's name is
/// left to the target) across engines and for a copy in the source's own schema, where the
/// source's names are taken. Across engines, a key or index the target cannot create (an
/// unbounded or JSON column, a key longer than [`TargetRules::index_key_bytes`]) is left out
/// with a warning.
pub fn target_table(
    ctx: &MappingContext<'_>,
    source: &TableDef,
    target: ObjectRef,
    columns: &[ColumnPlan],
    options: &TransferOptions,
) -> Result<(TableDef, Vec<String>)> {
    let mut warnings = Vec::new();
    let plan_of = |name: &str| columns.iter().find(|p| p.source == name);
    // Source column name → target column name, for the columns the table keeps.
    let mut renamed: Vec<(String, String)> = Vec::new();
    let same = ctx.same_engine();
    let rules = ctx.target_rules();
    let engine = ctx.target_engine.display_name();
    // New index and constraint names: another engine, or a copy next to its source.
    let rename = !same || same_container(&source.obj, &target);
    let mut out = TableDef::new(target);
    for column in &source.columns {
        let plan = plan_of(&column.name)
            .ok_or_else(|| Error::internal(format!("no column plan for '{}'", column.name)))?;
        let kept_when_skipped =
            column.auto_increment || (same && column.generated.is_some() && !plan.overridden);
        if plan.skipped && !kept_when_skipped {
            continue;
        }
        renamed.push((column.name.clone(), plan.target.clone()));
        if same {
            let mut c = column.clone();
            c.name.clone_from(&plan.target);
            if plan.overridden {
                c.data_type = plan.mapping.data_type.clone();
                c.native_type.clone_from(&plan.mapping.native_type);
            }
            out.columns.push(c);
            continue;
        }
        let mut c = ColumnDef::new(plan.target.clone(), plan.mapping.data_type.clone());
        c.native_type.clone_from(&plan.mapping.native_type);
        c.nullable = column.nullable;
        c.comment.clone_from(&column.comment);
        if column.auto_increment {
            if matches!(c.data_type, DataType::Int { .. }) {
                c.auto_increment = true;
            } else {
                warnings.push(format!(
                    "column {}: its values are no longer generated (auto-increment needs an \
                     integer column on the target)",
                    plan.target
                ));
            }
        }
        if let Some(default) = &column.default
            && !column.auto_increment
        {
            match portable_default(default, &c.data_type, &rules) {
                Some(literal) if unbounded_text(&c.data_type) => match rules.text_defaults {
                    TextDefaults::Plain => c.default = Some(literal),
                    TextDefaults::Parenthesized => c.default = Some(format!("({literal})")),
                    TextDefaults::Dropped => warnings.push(format!(
                        "column {}: its default ({default}) is not copied ({engine} allows no \
                         default on a text column)",
                        plan.target
                    )),
                },
                Some(literal) => c.default = Some(literal),
                None if default.trim().eq_ignore_ascii_case("null") => {}
                None => warnings.push(format!(
                    "column {}: its default ({default}) is not copied (engine-specific \
                     expression)",
                    plan.target
                )),
            }
        }
        if column.generated.is_some() {
            warnings.push(format!(
                "column {}: a computed column on the source; copied as a plain column holding \
                 the computed values",
                plan.target
            ));
        }
        if column.collation.is_some() {
            warnings.push(format!(
                "column {}: its collation is not copied (the target's default applies)",
                plan.target
            ));
        }
        out.columns.push(c);
    }
    if out.columns.is_empty() {
        return Err(Error::invalid_input(format!(
            "{} has no column left to copy",
            source.obj.display_name()
        )));
    }
    let target_of = |name: &str| -> Option<String> {
        renamed
            .iter()
            .find(|(s, _)| s == name)
            .map(|(_, t)| t.clone())
    };
    // Why the target cannot create a key or index on `names` (target column names), across
    // engines.
    let key_problem = |names: &[&str]| -> Option<String> {
        if same {
            return None;
        }
        let defs: Vec<&ColumnDef> = names
            .iter()
            .filter_map(|n| out.columns.iter().find(|c| c.name == *n))
            .collect();
        if defs.iter().any(|c| {
            never_indexed(&c.data_type) || (!rules.keys_on_unbounded && unbounded(&c.data_type))
        }) {
            return Some(format!(
                "it covers a column of unbounded text, binary or JSON, which {engine} cannot \
                 index"
            ));
        }
        let limit = rules.index_key_bytes?;
        let bytes: u64 = defs
            .iter()
            .map(|c| key_bytes(&c.data_type, rules.key_bytes_per_char))
            .sum();
        (bytes > u64::from(limit)).then(|| {
            format!("its key takes up to {bytes} bytes, more than the {limit} {engine} allows")
        })
    };
    let source_name = source.obj.name.clone();
    let target_name = out.obj.name.clone();
    let new_name = |name: &str| constraint_name(&source_name, &target_name, name);

    let mut primary_key = None;
    let mut indexes = Vec::new();
    let mut checks = Vec::new();
    if options.create_indexes {
        if let Some(pk) = &source.primary_key {
            let mapped: Option<Vec<String>> = pk.columns.iter().map(|c| target_of(c)).collect();
            match mapped {
                Some(cols) => {
                    let names: Vec<&str> = cols.iter().map(String::as_str).collect();
                    match key_problem(&names) {
                        Some(why) => {
                            warnings.push(format!("the primary key is not copied: {why}"));
                        }
                        None => {
                            primary_key = Some(PrimaryKeyDef {
                                name: if rename { None } else { pk.name.clone() },
                                columns: cols,
                            });
                        }
                    }
                }
                None => warnings.push(
                    "the primary key is not copied: one of its columns is left out".to_owned(),
                ),
            }
        }
        for index in &source.indexes {
            let label = &index.name;
            if !same {
                if index.predicate.is_some() {
                    warnings.push(format!("index {label}: a partial index is not copied"));
                    continue;
                }
                if index.columns.iter().any(|c| c.is_expression) {
                    warnings.push(format!("index {label}: an expression index is not copied"));
                    continue;
                }
            }
            let mut parts = Vec::with_capacity(index.columns.len());
            let mut missing = false;
            for part in &index.columns {
                if part.is_expression {
                    parts.push(part.clone());
                    continue;
                }
                match target_of(&part.name) {
                    Some(name) => parts.push(IndexColumn {
                        name,
                        is_expression: false,
                        descending: part.descending,
                        length: if same { part.length } else { None },
                    }),
                    None => missing = true,
                }
            }
            if missing {
                warnings.push(format!(
                    "index {label}: not copied (one of its columns is left out)"
                ));
                continue;
            }
            let names: Vec<&str> = parts.iter().map(|p| p.name.as_str()).collect();
            if let Some(why) = key_problem(&names) {
                warnings.push(format!("index {label}: not copied ({why})"));
                continue;
            }
            let mut copy = index.clone();
            copy.columns = parts;
            if rename {
                copy.name = new_name(&index.name);
            }
            if !same {
                copy.extras = Extras::new();
            }
            indexes.push(copy);
        }
        if same {
            checks.clone_from(&source.checks);
            if rename {
                for check in &mut checks {
                    check.name = check.name.as_deref().map(new_name);
                }
            }
        } else if !source.checks.is_empty() {
            warnings.push(format!(
                "{} check constraint(s) not copied (engine-specific expressions)",
                source.checks.len()
            ));
        }
    }
    out.primary_key = primary_key;
    out.indexes = indexes;
    out.checks = checks;
    out.comment.clone_from(&source.comment);
    if same {
        // The definition lives in the target container already (`target`); foreign keys are
        // planned separately, so no other ref needs moving.
        out.extras = source.extras.clone();
        return Ok((out, warnings));
    }
    if !source.extras.is_empty() {
        warnings.push("engine-specific table options are not copied".to_owned());
    }
    Ok((out, warnings))
}

/// `true` when two names are the same table or column name under `case` (names are always
/// quoted in generated SQL).
pub(crate) fn same_name(case: IdentCase, a: &str, b: &str) -> bool {
    case.matches(a, true, b)
}

/// The case rules of `engine` (its defaults; see `CaseRules::engine_default`).
pub(crate) fn case_rules(engine: Engine) -> CaseRules {
    CaseRules::engine_default(engine)
}

/// One row of a pair report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairRow {
    pub logical: DataType,
    pub mapping: TypeMapping,
}

/// How every [`representative_types`] logical type maps from `source` to `target` (the
/// per-pair lossiness report; golden-tested per pair by the transfer stream and shown by the
/// wizard's "type mapping" help).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairReport {
    pub source: Engine,
    pub target: Engine,
    pub rows: Vec<PairRow>,
}

/// The pair report of `ctx` over [`representative_types`]. Owner: transfer stream.
///
/// Each row starts from a column the **source** engine would have for the logical type (its
/// own `map`: a PostgreSQL source has no 1-byte integer, so `int8` is a `smallint` column
/// there); a type the source cannot hold at all is `unsupported` ("the source has no such
/// column"). That column then goes to the target: kept as it is between two servers of one
/// engine, else through the target's `map`. The fidelity is the target step's.
pub fn pair_report(ctx: &MappingContext<'_>) -> PairReport {
    let rows = representative_types()
        .into_iter()
        .map(|logical| {
            let held = ctx.source_mapper.map(&logical, ctx.source_version);
            let mapping = if held.fidelity == Fidelity::Unsupported {
                TypeMapping::unsupported(
                    logical.clone(),
                    format!(
                        "{} has no column type for {logical}",
                        ctx.source_engine.display_name()
                    ),
                )
            } else if ctx.same_engine() {
                TypeMapping::exact(held.data_type).native(held.native_type)
            } else {
                ctx.target_mapper.map(&held.data_type, ctx.target_version)
            };
            PairRow { logical, mapping }
        })
        .collect();
    PairReport {
        source: ctx.source_engine,
        target: ctx.target_engine,
        rows,
    }
}

impl PairReport {
    /// The report as text, one line per row: logical type, target type (native spelling, or
    /// the logical type the target's generator spells), fidelity and reason. The golden files
    /// of `tests/golden/<source>_<target>.txt` hold this text.
    pub fn to_text(&self) -> String {
        let mut out = format!(
            "# {} -> {}\n",
            self.source.display_name(),
            self.target.display_name()
        );
        for row in &self.rows {
            let target = if row.mapping.native_type.is_empty() {
                row.mapping.data_type.to_string()
            } else {
                format!("{} [{}]", row.mapping.native_type, row.mapping.data_type)
            };
            let logical = row.logical.to_string();
            let fidelity = row.mapping.fidelity.as_str();
            let reason = row.mapping.reason.as_deref().unwrap_or("");
            let line = format!("{logical:<24} -> {target:<36} {fidelity:<11} {reason}");
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out
    }
}

/// One logical type per family the mapping tables must answer, in a fixed order: the rows of
/// every [`PairReport`] and of the golden mapping tests.
pub fn representative_types() -> Vec<DataType> {
    let int = |bits, unsigned| DataType::Int { bits, unsigned };
    vec![
        DataType::Bool,
        int(8, false),
        int(16, false),
        int(32, false),
        int(64, false),
        int(32, true),
        int(64, true),
        DataType::Float { bits: 32 },
        DataType::Float { bits: 64 },
        DataType::Decimal {
            precision: Some(18),
            scale: Some(4),
        },
        DataType::Decimal {
            precision: Some(38),
            scale: Some(10),
        },
        DataType::Decimal {
            precision: None,
            scale: None,
        },
        DataType::Char { length: Some(10) },
        DataType::VarChar { length: Some(255) },
        DataType::VarChar { length: None },
        DataType::Text,
        DataType::Binary { length: Some(16) },
        DataType::VarBinary { length: None },
        DataType::Date,
        DataType::Time { precision: Some(6) },
        DataType::Timestamp { precision: Some(6) },
        DataType::TimestampTz { precision: Some(6) },
        DataType::Interval,
        DataType::Uuid,
        DataType::Json { binary: false },
        DataType::Json { binary: true },
        DataType::Array {
            element: Box::new(DataType::INT32),
        },
        DataType::Array {
            element: Box::new(DataType::Text),
        },
        DataType::Enum {
            labels: vec!["small".into(), "medium".into(), "large".into()],
        },
        DataType::Other {
            name: "geometry".into(),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn representative_types_cover_every_family_once() {
        let types = representative_types();
        assert_eq!(types.len(), 30);
        for (i, t) in types.iter().enumerate() {
            assert!(!types[..i].contains(t), "{t} twice");
        }
        // Every DataType variant appears.
        let has = |f: fn(&DataType) -> bool| types.iter().any(f);
        assert!(has(|t| matches!(t, DataType::Bool)));
        assert!(has(|t| matches!(t, DataType::Interval)));
        assert!(has(|t| matches!(t, DataType::Uuid)));
        assert!(has(|t| matches!(t, DataType::Array { .. })));
        assert!(has(|t| matches!(t, DataType::Enum { .. })));
        assert!(has(|t| matches!(t, DataType::Other { .. })));
        assert!(has(|t| matches!(t, DataType::Json { binary: true })));
    }

    use crate::cells::mysql::MySqlTypeMapper;
    use crate::cells::postgres::PgTypeMapper;
    use crate::cells::sqlite::SqliteTypeMapper;
    use oxbase_types::{ForeignKeyDef, GeneratedColumn, IndexDef, ObjectKind, PrimaryKeyDef as Pk};
    use pretty_assertions::assert_eq;

    const MYSQL: MySqlTypeMapper = MySqlTypeMapper { mariadb: false };

    fn pg_version() -> ServerVersion {
        ServerVersion::new(18, 6, 0)
    }

    fn my_version() -> ServerVersion {
        ServerVersion::new(8, 4, 13)
    }

    fn pg_to_mysql<'a>(pg: &'a ServerVersion, my: &'a ServerVersion) -> MappingContext<'a> {
        MappingContext {
            source_engine: Engine::Postgres,
            source_version: pg,
            source_mapper: &PgTypeMapper,
            target_engine: Engine::MySql,
            target_version: my,
            target_mapper: &MYSQL,
        }
    }

    #[test]
    fn fidelity_between_types() {
        use Fidelity::{Exact, Narrowed, Unsupported, Widened};
        let int = |bits, unsigned| DataType::Int { bits, unsigned };
        let dec = |p, s| DataType::Decimal {
            precision: Some(p),
            scale: Some(s),
        };
        let cases: Vec<(DataType, DataType, Fidelity)> = vec![
            (DataType::INT32, DataType::INT32, Exact),
            (DataType::INT32, DataType::INT64, Widened),
            (DataType::INT64, DataType::INT32, Narrowed),
            (int(32, true), DataType::INT64, Widened),
            (int(32, true), DataType::INT32, Narrowed),
            (DataType::INT32, dec(10, 0), Widened),
            (DataType::INT32, dec(9, 0), Narrowed),
            (DataType::INT16, DataType::Float { bits: 32 }, Widened),
            (DataType::INT64, DataType::FLOAT64, Narrowed),
            (DataType::Float { bits: 32 }, DataType::FLOAT64, Widened),
            (DataType::FLOAT64, DataType::Float { bits: 32 }, Narrowed),
            (dec(10, 2), dec(12, 2), Widened),
            (dec(10, 2), dec(10, 3), Narrowed),
            (
                dec(10, 2),
                DataType::Decimal {
                    precision: None,
                    scale: None,
                },
                Widened,
            ),
            (
                DataType::Decimal {
                    precision: None,
                    scale: None,
                },
                dec(65, 30),
                Narrowed,
            ),
            (DataType::Uuid, DataType::Text, Widened),
            (DataType::Uuid, DataType::Char { length: Some(36) }, Widened),
            (
                DataType::Uuid,
                DataType::Char { length: Some(20) },
                Narrowed,
            ),
            (
                DataType::VarChar { length: Some(10) },
                DataType::VarChar { length: Some(20) },
                Widened,
            ),
            (
                DataType::VarChar { length: Some(30) },
                DataType::VarChar { length: Some(20) },
                Narrowed,
            ),
            (
                DataType::Text,
                DataType::VarChar { length: Some(20) },
                Narrowed,
            ),
            (DataType::Text, DataType::INT32, Narrowed),
            (DataType::Text, DataType::Json { binary: false }, Narrowed),
            (
                DataType::Array {
                    element: Box::new(DataType::INT32),
                },
                DataType::Json { binary: true },
                Widened,
            ),
            (
                DataType::Json { binary: true },
                DataType::Json { binary: false },
                Exact,
            ),
            (DataType::Json { binary: true }, DataType::Date, Unsupported),
            (
                DataType::VarBinary { length: None },
                DataType::INT32,
                Unsupported,
            ),
            (
                DataType::Binary { length: Some(16) },
                DataType::VarBinary { length: None },
                Widened,
            ),
            (
                DataType::VarBinary { length: None },
                DataType::Binary { length: Some(16) },
                Narrowed,
            ),
            (
                DataType::Uuid,
                DataType::Binary { length: Some(16) },
                Widened,
            ),
            (
                DataType::Date,
                DataType::Timestamp { precision: Some(6) },
                Widened,
            ),
            (
                DataType::Timestamp { precision: Some(6) },
                DataType::Date,
                Narrowed,
            ),
            (
                DataType::TimestampTz { precision: Some(6) },
                DataType::Timestamp { precision: Some(6) },
                Narrowed,
            ),
            (
                DataType::Timestamp { precision: Some(3) },
                DataType::Timestamp { precision: Some(6) },
                Widened,
            ),
            (
                DataType::Time { precision: Some(7) },
                DataType::Time { precision: Some(6) },
                Narrowed,
            ),
            (
                DataType::Bool,
                DataType::Int {
                    bits: 8,
                    unsigned: false,
                },
                Widened,
            ),
            (DataType::INT32, DataType::Bool, Narrowed),
            (
                DataType::Enum {
                    labels: vec!["a".into()],
                },
                DataType::Enum {
                    labels: vec!["a".into(), "b".into()],
                },
                Widened,
            ),
            (
                DataType::Text,
                DataType::Enum {
                    labels: vec!["a".into()],
                },
                Narrowed,
            ),
            (DataType::Date, DataType::INT32, Unsupported),
        ];
        for (from, to, expected) in cases {
            let (fidelity, reason) = fidelity_between(&from, &to);
            assert_eq!(fidelity, expected, "{from} -> {to}");
            assert_eq!(
                reason.is_none(),
                fidelity == Exact,
                "{from} -> {to}: {reason:?}"
            );
        }
    }

    #[test]
    fn map_column_across_engines_and_with_overrides() {
        let (pg, my) = (pg_version(), my_version());
        let ctx = pg_to_mysql(&pg, &my);
        let mut tz = ColumnDef::new("c_tz", DataType::TimestampTz { precision: Some(6) });
        tz.native_type = "timestamp(6) with time zone".into();
        let plan = map_column(&ctx, &tz, None);
        assert_eq!(plan.source, "c_tz");
        assert_eq!(plan.target, "c_tz");
        assert_eq!(plan.source_native, "timestamp(6) with time zone");
        assert_eq!(
            plan.mapping.data_type,
            DataType::Timestamp { precision: Some(6) }
        );
        assert_eq!(plan.mapping.fidelity, Fidelity::Narrowed);
        assert!(!plan.overridden && !plan.skipped);

        let interval = ColumnDef::new("c_interval", DataType::Interval);
        let o = ColumnOverride {
            object: 0,
            column: "c_interval".into(),
            skip: false,
            target_name: Some("c_span".into()),
            data_type: Some(DataType::Text),
            native_type: None,
        };
        let plan = map_column(&ctx, &interval, Some(&o));
        assert!(plan.overridden);
        assert_eq!(plan.target, "c_span");
        assert_eq!(plan.mapping.data_type, DataType::Text);
        assert_eq!(plan.mapping.fidelity, Fidelity::Widened);

        let skip = ColumnOverride {
            skip: true,
            target_name: None,
            data_type: None,
            ..o.clone()
        };
        assert!(map_column(&ctx, &interval, Some(&skip)).skipped);

        let native = ColumnOverride {
            skip: false,
            target_name: None,
            data_type: None,
            native_type: Some("varchar(100)".into()),
            ..o
        };
        let plan = map_column(&ctx, &interval, Some(&native));
        assert_eq!(plan.mapping.native_type, "varchar(100)");
        assert_eq!(
            plan.mapping.data_type,
            DataType::VarChar { length: Some(64) }
        );

        // Same engine: the native type stays.
        let same = MappingContext {
            target_engine: Engine::Postgres,
            target_version: &pg,
            target_mapper: &PgTypeMapper,
            ..ctx
        };
        let plan = map_column(&same, &tz, None);
        assert_eq!(plan.mapping.native_type, "timestamp(6) with time zone");
        assert_eq!(plan.mapping.fidelity, Fidelity::Exact);
    }

    fn source_table() -> TableDef {
        let mut t = TableDef::new(ObjectRef::table(Some("db"), Some("src"), "orders"));
        let mut id = ColumnDef::new("id", DataType::INT32).not_null();
        id.auto_increment = true;
        id.native_type = "integer".into();
        id.default = Some("nextval('src.orders_id_seq'::regclass)".into());
        let mut status = ColumnDef::new("status", DataType::VarChar { length: Some(20) });
        status.default = Some("'new'::character varying".into());
        status.collation = Some("C".into());
        let mut qty = ColumnDef::new("qty", DataType::INT32);
        qty.default = Some("0".into());
        let mut created = ColumnDef::new("created", DataType::TimestampTz { precision: Some(6) });
        created.default = Some("now()".into());
        let mut total = ColumnDef::new(
            "total",
            DataType::Decimal {
                precision: Some(12),
                scale: Some(2),
            },
        );
        total.generated = Some(GeneratedColumn {
            expression: "qty * 2".into(),
            stored: true,
        });
        let note = ColumnDef::new("note", DataType::Text);
        t.columns = vec![id, status, qty, created, total, note];
        t.primary_key = Some(Pk {
            name: Some("orders_pkey".into()),
            columns: vec!["id".into()],
        });
        t.indexes = vec![
            IndexDef {
                name: "orders_status_idx".into(),
                columns: vec![IndexColumn::column("status")],
                unique: false,
                is_constraint: false,
                predicate: None,
                extras: Extras::new(),
            },
            IndexDef {
                name: "by_note".into(),
                columns: vec![IndexColumn::column("note")],
                unique: false,
                is_constraint: false,
                predicate: None,
                extras: Extras::new(),
            },
            IndexDef {
                name: "lower_status".into(),
                columns: vec![IndexColumn {
                    name: "lower(status)".into(),
                    is_expression: true,
                    descending: false,
                    length: None,
                }],
                unique: false,
                is_constraint: false,
                predicate: None,
                extras: Extras::new(),
            },
            IndexDef {
                name: "qty".into(),
                columns: vec![IndexColumn::column("qty")],
                unique: true,
                is_constraint: false,
                predicate: None,
                extras: Extras::new(),
            },
        ];
        t.foreign_keys = vec![ForeignKeyDef {
            name: Some("orders_customer_fk".into()),
            columns: vec!["qty".into()],
            ref_table: ObjectRef::table(Some("db"), Some("src"), "customers"),
            ref_columns: vec!["id".into()],
            on_delete: oxbase_types::FkAction::Cascade,
            on_update: oxbase_types::FkAction::NoAction,
            extras: Extras::new(),
        }];
        t.checks = vec![oxbase_types::CheckDef {
            name: Some("qty_positive".into()),
            expression: "qty >= 0".into(),
            extras: Extras::new(),
        }];
        t
    }

    #[test]
    fn target_table_across_engines_drops_engine_specific_text() {
        let (pg, my) = (pg_version(), my_version());
        let ctx = pg_to_mysql(&pg, &my);
        let source = source_table();
        let plans: Vec<ColumnPlan> = source
            .columns
            .iter()
            .map(|c| map_column(&ctx, c, None))
            .collect();
        let target = ObjectRef::database("dst").child(ObjectKind::Table, "orders");
        let (def, warnings) = target_table(
            &ctx,
            &source,
            target.clone(),
            &plans,
            &TransferOptions::default(),
        )
        .unwrap();
        assert_eq!(def.obj, target);
        let names: Vec<&str> = def.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["id", "status", "qty", "created", "total", "note"]);
        let id = &def.columns[0];
        assert!(id.auto_increment && !id.nullable && id.default.is_none());
        assert_eq!(id.native_type, "");
        assert_eq!(def.columns[1].default.as_deref(), Some("'new'"));
        assert_eq!(def.columns[1].collation, None);
        assert_eq!(def.columns[2].default.as_deref(), Some("0"));
        assert_eq!(def.columns[3].default, None);
        assert_eq!(
            def.columns[3].data_type,
            DataType::Timestamp { precision: Some(6) }
        );
        assert_eq!(def.columns[4].generated, None);
        assert_eq!(def.primary_key.as_ref().unwrap().columns, ["id"]);
        let indexes: Vec<&str> = def.indexes.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(indexes, ["orders_status_idx", "orders_qty"]);
        assert_eq!((def.foreign_keys.len(), def.checks.len()), (0, 0));
        let all = warnings.join("\n");
        for expected in [
            "column created: its default (now()) is not copied",
            "column total: a computed column",
            "column status: its collation is not copied",
            "index by_note: not copied (it covers a column of unbounded text",
            "index lower_status: an expression index is not copied",
            "1 check constraint(s) not copied",
        ] {
            assert!(all.contains(expected), "missing '{expected}' in:\n{all}");
        }

        // Without indexes; a skipped column.
        let mut skipped = plans.clone();
        skipped[5].skipped = true;
        let options = TransferOptions {
            create_indexes: false,
            ..TransferOptions::default()
        };
        let (def, _) = target_table(&ctx, &source, target, &skipped, &options).unwrap();
        assert_eq!(def.columns.len(), 5);
        assert!(def.primary_key.is_none());
        assert_eq!(def.indexes.len(), 0);
    }

    #[test]
    fn target_table_on_the_same_engine_keeps_everything_but_foreign_keys() {
        let pg = pg_version();
        let ctx = MappingContext {
            source_engine: Engine::Postgres,
            source_version: &pg,
            source_mapper: &PgTypeMapper,
            target_engine: Engine::Postgres,
            target_version: &pg,
            target_mapper: &PgTypeMapper,
        };
        let source = source_table();
        let plans: Vec<ColumnPlan> = source
            .columns
            .iter()
            .map(|c| map_column(&ctx, c, None))
            .collect();
        let target = ObjectRef::schema(Some("db"), "dst").child(ObjectKind::Table, "orders");
        let (def, warnings) = target_table(
            &ctx,
            &source,
            target.clone(),
            &plans,
            &TransferOptions::default(),
        )
        .unwrap();
        assert_eq!(warnings, Vec::<String>::new());
        assert_eq!(def.obj, target);
        assert_eq!(def.columns, source.columns);
        assert_eq!(def.indexes.len(), 4);
        assert_eq!(def.checks, source.checks);
        assert_eq!(def.foreign_keys.len(), 0);
    }

    #[test]
    fn same_engine_tables_keep_computed_columns_and_rename_next_to_their_source() {
        let pg = pg_version();
        let ctx = MappingContext {
            source_engine: Engine::Postgres,
            source_version: &pg,
            source_mapper: &PgTypeMapper,
            target_engine: Engine::Postgres,
            target_version: &pg,
            target_mapper: &PgTypeMapper,
        };
        let source = source_table();
        let mut plans: Vec<ColumnPlan> = source
            .columns
            .iter()
            .map(|c| map_column(&ctx, c, None))
            .collect();
        // The plan skips the computed column's values (the target computes them).
        plans[4].skipped = true;
        let elsewhere = ObjectRef::schema(Some("db"), "dst").child(ObjectKind::Table, "orders");
        let (def, _) = target_table(
            &ctx,
            &source,
            elsewhere,
            &plans,
            &TransferOptions::default(),
        )
        .unwrap();
        assert_eq!(def.columns, source.columns);
        assert_eq!(
            def.column("total").unwrap().generated,
            source.columns[4].generated
        );
        assert_eq!(
            def.primary_key.as_ref().unwrap().name.as_deref(),
            Some("orders_pkey")
        );

        // A copy in the source's own schema: index and constraint names are new.
        let beside = ObjectRef::schema(Some("db"), "src").child(ObjectKind::Table, "orders_copy");
        let (def, _) =
            target_table(&ctx, &source, beside, &plans, &TransferOptions::default()).unwrap();
        assert_eq!(def.primary_key.as_ref().unwrap().name, None);
        let names: Vec<&str> = def.indexes.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "orders_copy_status_idx",
                "orders_copy_by_note",
                "orders_copy_lower_status",
                "orders_copy_qty"
            ]
        );
        assert_eq!(
            def.checks[0].name.as_deref(),
            Some("orders_copy_qty_positive")
        );

        // A user's skip still leaves the computed column out.
        plans[4].overridden = true;
        let (def, _) = target_table(
            &ctx,
            &source,
            ObjectRef::schema(Some("db"), "dst").child(ObjectKind::Table, "orders"),
            &plans,
            &TransferOptions::default(),
        )
        .unwrap();
        assert!(def.column("total").is_none());
    }

    fn keyed(name: &str, pk_name: &str, key: ColumnDef) -> TableDef {
        let mut t = TableDef::new(ObjectRef::table(Some("db"), None, name));
        let key_name = key.name.clone();
        t.columns = vec![
            key,
            ColumnDef::new("label", DataType::VarChar { length: Some(1000) }),
        ];
        t.primary_key = Some(Pk {
            name: Some(pk_name.into()),
            columns: vec![key_name],
        });
        t.indexes = vec![IndexDef {
            name: "label".into(),
            columns: vec![IndexColumn::column("label")],
            unique: true,
            is_constraint: false,
            predicate: None,
            extras: Extras::new(),
        }];
        t
    }

    fn mapped(ctx: &MappingContext<'_>, source: &TableDef) -> (TableDef, Vec<String>) {
        let mut plans: Vec<ColumnPlan> = source
            .columns
            .iter()
            .map(|c| map_column(ctx, c, None))
            .collect();
        bound_key_columns(ctx, source, &mut plans);
        let target = ObjectRef::database("dst").child(ObjectKind::Table, source.obj.name.clone());
        target_table(ctx, source, target, &plans, &TransferOptions::default()).unwrap()
    }

    #[test]
    fn keys_across_engines_get_new_names_and_bounded_columns() {
        let (pg, my) = (pg_version(), my_version());
        // MySQL names every primary key PRIMARY: PostgreSQL names them per table instead.
        let my_to_pg = MappingContext {
            source_engine: Engine::MySql,
            source_version: &my,
            source_mapper: &MYSQL,
            target_engine: Engine::Postgres,
            target_version: &pg,
            target_mapper: &PgTypeMapper,
        };
        for name in ["a", "b"] {
            let source = keyed(
                name,
                "PRIMARY",
                ColumnDef::new("id", DataType::INT32).not_null(),
            );
            let (def, warnings) = mapped(&my_to_pg, &source);
            assert_eq!(def.primary_key.as_ref().unwrap().name, None, "{warnings:?}");
            // PostgreSQL indexes any text: the index stays, renamed after its table.
            assert_eq!(def.indexes[0].name, format!("{name}_label"));
        }

        // A text key into MySQL becomes varchar(255), narrowed; varchar(1000) is too long a key.
        let ctx = pg_to_mysql(&pg, &my);
        let source = keyed(
            "tags",
            "tags_pkey",
            ColumnDef::new("code", DataType::Text).not_null(),
        );
        let mut plans: Vec<ColumnPlan> = source
            .columns
            .iter()
            .map(|c| map_column(&ctx, c, None))
            .collect();
        bound_key_columns(&ctx, &source, &mut plans);
        assert_eq!(
            plans[0].mapping.data_type,
            DataType::VarChar { length: Some(255) }
        );
        assert_eq!(plans[0].mapping.fidelity, Fidelity::Narrowed);
        assert!(
            plans[0]
                .mapping
                .reason
                .as_deref()
                .unwrap()
                .contains("primary-key column")
        );
        assert_eq!(
            plans[1].mapping.data_type,
            DataType::VarChar { length: Some(1000) }
        );
        let (def, warnings) = mapped(&ctx, &source);
        assert_eq!(def.primary_key.as_ref().unwrap().columns, ["code"]);
        assert_eq!(def.primary_key.as_ref().unwrap().name, None);
        assert_eq!(def.indexes.len(), 0);
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("index label: not copied (its key takes up to 4000 bytes")),
            "{warnings:?}"
        );

        // An overridden key column keeps the user's type: the key is left out.
        let mut plans: Vec<ColumnPlan> = source
            .columns
            .iter()
            .map(|c| map_column(&ctx, c, None))
            .collect();
        plans[0].overridden = true;
        bound_key_columns(&ctx, &source, &mut plans);
        let target = ObjectRef::database("dst").child(ObjectKind::Table, "tags");
        let (def, warnings) =
            target_table(&ctx, &source, target, &plans, &TransferOptions::default()).unwrap();
        assert!(def.primary_key.is_none());
        assert!(
            warnings
                .iter()
                .any(|w| w.starts_with("the primary key is not copied: it covers")),
            "{warnings:?}"
        );

        // SQLite keys any text as it is.
        let lite = ServerVersion::new(3, 50, 4);
        let to_sqlite = MappingContext {
            target_engine: Engine::Sqlite,
            target_version: &lite,
            target_mapper: &SqliteTypeMapper,
            ..ctx
        };
        let (def, _) = mapped(&to_sqlite, &source);
        assert_eq!(def.column("code").unwrap().data_type, DataType::Text);
        assert!(def.primary_key.is_some());
    }

    #[test]
    fn defaults_follow_the_target_rules() {
        let (pg, my) = (pg_version(), my_version());
        let mut source = TableDef::new(ObjectRef::table(Some("db"), Some("src"), "d"));
        let mut note = ColumnDef::new("note", DataType::Text).not_null();
        note.default = Some("''::text".into());
        let mut flag = ColumnDef::new("flag", DataType::Bool);
        flag.default = Some("true".into());
        let mut off = ColumnDef::new("off", DataType::Bool);
        off.default = Some("((0))".into());
        source.columns = vec![note, flag, off];
        let defaults = |ctx: &MappingContext<'_>| {
            let (def, warnings) = mapped(ctx, &source);
            let d: Vec<Option<String>> = def.columns.iter().map(|c| c.default.clone()).collect();
            (d, warnings)
        };
        // MySQL 8.0.13+: a text default is an expression; booleans are 1 / 0.
        let (d, _) = defaults(&pg_to_mysql(&pg, &my));
        let want = |a: &str, b: &str, c: &str| {
            vec![Some(a.to_owned()), Some(b.to_owned()), Some(c.to_owned())]
        };
        assert_eq!(d, want("('')", "1", "0"));
        // Older MySQL refuses a text default: dropped with a warning.
        let old = ServerVersion::new(8, 0, 12);
        let (d, warnings) = defaults(&pg_to_mysql(&pg, &old));
        assert_eq!(d[0], None);
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("allows no default on a text column")),
            "{warnings:?}"
        );
        // MariaDB and PostgreSQL take the literal.
        let maria = MySqlTypeMapper { mariadb: true };
        let mv = ServerVersion::new(11, 8, 9);
        let to_maria = MappingContext {
            target_engine: Engine::MariaDb,
            target_version: &mv,
            target_mapper: &maria,
            ..pg_to_mysql(&pg, &my)
        };
        assert_eq!(defaults(&to_maria).0[0].as_deref(), Some("''"));
        let ms = ServerVersion::new(16, 0, 0);
        let ms_to_pg = MappingContext {
            source_engine: Engine::MsSql,
            source_version: &ms,
            source_mapper: &crate::cells::mssql::MsSqlTypeMapper,
            target_engine: Engine::Postgres,
            target_version: &pg,
            target_mapper: &PgTypeMapper,
        };
        assert_eq!(defaults(&ms_to_pg).0, want("''", "true", "false"));
    }

    #[test]
    fn an_unsigned_bigint_id_stays_generated() {
        let (pg, my) = (pg_version(), my_version());
        let ctx = MappingContext {
            source_engine: Engine::MySql,
            source_version: &my,
            source_mapper: &MYSQL,
            target_engine: Engine::Postgres,
            target_version: &pg,
            target_mapper: &PgTypeMapper,
        };
        let mut id = ColumnDef::new(
            "id",
            DataType::Int {
                bits: 64,
                unsigned: true,
            },
        )
        .not_null();
        id.auto_increment = true;
        let plan = map_column(&ctx, &id, None);
        assert_eq!(plan.mapping.data_type, DataType::INT64);
        assert_eq!(plan.mapping.fidelity, Fidelity::Narrowed);
        // A plain unsigned bigint keeps its exact home.
        id.auto_increment = false;
        let plan = map_column(&ctx, &id, None);
        assert_ne!(plan.mapping.data_type, DataType::INT64);
    }

    #[test]
    fn pair_reports_start_from_what_the_source_holds() {
        let (pg, lite) = (pg_version(), ServerVersion::new(3, 50, 4));
        let ctx = MappingContext {
            source_engine: Engine::Sqlite,
            source_version: &lite,
            source_mapper: &SqliteTypeMapper,
            target_engine: Engine::Postgres,
            target_version: &pg,
            target_mapper: &PgTypeMapper,
        };
        let report = pair_report(&ctx);
        assert_eq!(report.rows.len(), 30);
        let interval = report
            .rows
            .iter()
            .find(|r| r.logical == DataType::Interval)
            .unwrap();
        // SQLite holds an interval as text, which PostgreSQL keeps exactly.
        assert_eq!(interval.mapping, TypeMapping::exact(DataType::Text));
        let text = report.to_text();
        assert!(text.starts_with("# SQLite -> PostgreSQL\n"), "{text}");
        assert_eq!(text.lines().count(), 31);
    }
}
