//! The MySQL / MariaDB [`DdlGenerator`] cells (M3) for tables, columns, indexes, foreign keys and
//! checks.
//!
//! # Statements
//!
//! - `create` of a table is one `CREATE TABLE` with every column, the primary key, indexes, checks
//!   and foreign keys inline, then the table options (`ENGINE`, `DEFAULT CHARSET`, `COLLATE`,
//!   `ROW_FORMAT`, `COMMENT`). A child (`Column`, `Index`, `ForeignKey`, `Check`) is one
//!   `ALTER TABLE ... ADD ...`.
//! - `alter` of a table is at most four statements, in the order of the contract:
//!   1. `RENAME TABLE` (a table rename);
//!   2. `ALTER TABLE ... RENAME COLUMN / RENAME INDEX` (designer renames; `CHANGE COLUMN` with
//!      the full definition before MySQL 8.0 / MariaDB 10.5.2, drop + add of the index where
//!      `RENAME INDEX` is missing; renamed foreign keys and checks are dropped and added);
//!   3. `ALTER TABLE ... DROP FOREIGN KEY / DROP CHECK` for foreign keys and checks that change
//!      or go (a separate statement, so a constraint can be re-added under the same name);
//!   4. one `ALTER TABLE` with every other clause: index and primary-key drops, column drops,
//!      `MODIFY COLUMN` (always the **full** column definition: type, character set, collation,
//!      generated expression, nullability, default, `ON UPDATE`, `AUTO_INCREMENT`, visibility,
//!      comment) and `ADD COLUMN` with `FIRST` / `AFTER` where the position changes, the new
//!      primary key, new or changed indexes, checks and foreign keys, then table options with
//!      the comment last.
//!
//!   A child `alter` rebuilds the table's new definition from [`DdlContext::table`] and runs the
//!   table `alter`.
//! - `drop_object`: `DROP TABLE`, or `ALTER TABLE ... DROP COLUMN / INDEX / PRIMARY KEY /
//!   FOREIGN KEY / CHECK` (MariaDB: `DROP CONSTRAINT`).
//!
//! Every step is `transactional = false` (MySQL and MariaDB commit each DDL statement on their
//! own); a script with more than one step carries a warning that a failure leaves the earlier
//! statements applied. Drops of columns and constraints and narrowing type changes are
//! `destructive`.
//!
//! # Literals and names
//!
//! Identifiers go through `BacktickQuoting`; comments and enum labels through its `quote_str`
//! (quotes doubled, backslashes escaped). Defaults, check, generated and `ON UPDATE`
//! expressions are engine-native SQL from the model and are written as they are. Character set,
//! collation, engine and row format names must be plain words.
//!
//! # Not supported (`Error::Unsupported`)
//!
//! Identity columns (use `AUTO_INCREMENT`), partial indexes, the `SET DEFAULT` foreign-key action
//! (InnoDB rejects it), functional index keys before MySQL 8.0.13 and on MariaDB, descending keys
//! before MySQL 8.0 / MariaDB 10.8, invisible columns and indexes on servers without them,
//! `NOT ENFORCED` checks on MariaDB, and changing system versioning.

use oxbase_dialect::compare::{CompareRules, diff_columns, diff_foreign_keys, diff_indexes};
use oxbase_dialect::{BacktickQuoting, DdlContext, DdlGenerator, Quoting};
use oxbase_types::{
    CheckDef, ColumnDef, DataType, Error, Extras, FkAction, ForeignKeyDef, IndexDef, ObjectDef,
    ObjectKind, ObjectRef, PrimaryKeyDef, PropId, Rename, Result, Script, ScriptStep,
    ServerVersion, TableDef, Value, props,
};

use super::caps::Features;
use super::compare::{MySqlRules, is_fk_index, normalize_pair};
use super::native::{column_type, is_narrowing};
use super::table::PRIMARY;
use super::{ENFORCED, INVISIBLE, ON_UPDATE_EXPRESSION};

/// The warning of every script with more than one statement.
const NOT_ATOMIC: &str = "Each statement commits on its own: if one fails, the statements \
                          before it stay applied.";

/// The DDL generator of one designer kind on MySQL or MariaDB.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MySqlDdl {
    pub mariadb: bool,
}

impl DdlGenerator for MySqlDdl {
    fn create(&self, d: &ObjectDef, ctx: &DdlContext<'_>) -> Result<Script> {
        let g = Gen::new(self.mariadb, ctx);
        let mut script = match d {
            ObjectDef::Table(t) => g.create_table(t)?,
            ObjectDef::Column { table, column } => {
                let sql = format!(
                    "ALTER TABLE {} ADD COLUMN {}",
                    g.qualified(table),
                    g.column_def(column)?
                );
                one(step(sql, format!("Add column {}", column.name)))
            }
            ObjectDef::Index { table, index } => {
                let (clause, what) = if index.name == PRIMARY {
                    (
                        format!("PRIMARY KEY ({})", g.idents(index_column_names(index)?)),
                        "Add the primary key".to_owned(),
                    )
                } else {
                    (g.index_clause(index)?, format!("Add index {}", index.name))
                };
                let sql = format!("ALTER TABLE {} ADD {clause}", g.qualified(table));
                one(step(sql, what))
            }
            ObjectDef::ForeignKey { table, foreign_key } => {
                let sql = format!(
                    "ALTER TABLE {} ADD {}",
                    g.qualified(table),
                    g.fk_clause(table, foreign_key)?
                );
                one(step(
                    sql,
                    format!("Add foreign key {}", label(foreign_key.name.as_deref())),
                ))
            }
            ObjectDef::Check { table, check } => {
                let sql = format!(
                    "ALTER TABLE {} ADD {}",
                    g.qualified(table),
                    g.check_clause(check)?
                );
                one(step(
                    sql,
                    format!("Add check {}", label(check.name.as_deref())),
                ))
            }
            other => return Err(not_designer_kind(other.kind())),
        };
        let checks = match d {
            ObjectDef::Table(t) => !t.checks.is_empty(),
            ObjectDef::Check { .. } => true,
            _ => false,
        };
        if checks && !g.f.checks {
            script.warn(g.unenforced_checks());
        }
        Ok(script)
    }

    fn alter(&self, old: &ObjectDef, new: &ObjectDef, ctx: &DdlContext<'_>) -> Result<Script> {
        let g = Gen::new(self.mariadb, ctx);
        match (old, new) {
            (ObjectDef::Table(before), ObjectDef::Table(after)) => {
                g.alter_table(before, after, ctx.renames)
            }
            _ if old.kind() != new.kind() => Err(Error::invalid_input(format!(
                "cannot alter a {} into a {}",
                old.kind().display_name(),
                new.kind().display_name()
            ))),
            _ => {
                let base = ctx.table.ok_or_else(|| {
                    Error::invalid_input(format!(
                        "altering a {} needs its table (DdlContext::table)",
                        old.kind().display_name()
                    ))
                })?;
                let (target, renames) = with_child_changed(base, old, new, ctx.renames)?;
                g.alter_table(base, &target, &renames)
            }
        }
    }

    fn drop_object(&self, r: &ObjectRef, ctx: &DdlContext<'_>) -> Result<Script> {
        let g = Gen::new(self.mariadb, ctx);
        if r.kind == ObjectKind::Table {
            let sql = format!("DROP TABLE {}", g.qualified(r));
            return Ok(one(
                step(sql, format!("Drop table {}", r.name)).destructive()
            ));
        }
        if !matches!(
            r.kind,
            ObjectKind::Column | ObjectKind::Index | ObjectKind::ForeignKey | ObjectKind::Check
        ) {
            return Err(not_designer_kind(r.kind));
        }
        let table = r
            .owner()
            .ok_or_else(|| Error::invalid_input(format!("{r} names no owning table")))?;
        let name = g.ident(&r.name);
        let (clause, what, destructive) = match r.kind {
            ObjectKind::Column => (
                format!("DROP COLUMN {name}"),
                format!("Drop column {}", r.name),
                true,
            ),
            ObjectKind::Index if r.name == PRIMARY => (
                "DROP PRIMARY KEY".to_owned(),
                "Drop the primary key".to_owned(),
                true,
            ),
            ObjectKind::Index => {
                let unique = ctx
                    .table
                    .and_then(|t| t.indexes.iter().find(|i| i.name == r.name))
                    .is_some_and(|i| i.unique);
                (
                    format!("DROP INDEX {name}"),
                    format!("Drop index {}", r.name),
                    unique,
                )
            }
            ObjectKind::ForeignKey => (
                format!("DROP FOREIGN KEY {name}"),
                format!("Drop foreign key {}", r.name),
                true,
            ),
            ObjectKind::Check => (
                g.drop_check(&r.name),
                format!("Drop check {}", r.name),
                true,
            ),
            other => return Err(not_designer_kind(other)),
        };
        let sql = format!("ALTER TABLE {} {clause}", g.qualified(&table));
        let mut step = step(sql, what);
        if destructive {
            step = step.destructive();
        }
        Ok(one(step))
    }
}

fn not_designer_kind(kind: ObjectKind) -> Error {
    Error::unsupported(format!(
        "the MySQL / MariaDB DDL generator does not handle {}",
        kind.display_name()
    ))
}

fn label(name: Option<&str>) -> &str {
    name.unwrap_or("(unnamed)")
}

/// The column names of a plain index (the primary key given as an `Index`).
fn index_column_names(index: &IndexDef) -> Result<Vec<String>> {
    index
        .columns
        .iter()
        .map(|part| {
            if part.is_expression || part.length.is_some() || part.descending {
                Err(Error::invalid_input(
                    "a primary key is made of plain columns",
                ))
            } else {
                Ok(part.name.clone())
            }
        })
        .collect()
}

/// `base` with `old`'s child replaced by `new`'s, plus the implied rename when a column or an
/// index changed its name.
fn with_child_changed(
    base: &TableDef,
    old: &ObjectDef,
    new: &ObjectDef,
    renames: &[Rename],
) -> Result<(TableDef, Vec<Rename>)> {
    let mut renames = renames.to_vec();
    let missing = |what: &str, name: &str| {
        Error::invalid_input(format!("{what} {name} is not on table {}", base.obj.name))
    };
    let mut implied = |kind: ObjectKind, before: &str, after: &str| {
        if before != after && !renames.iter().any(|r| r.kind == kind && r.old == before) {
            renames.push(Rename::new(kind, before, after));
        }
    };
    let target = match (old, new) {
        (ObjectDef::Column { column: a, .. }, ObjectDef::Column { column: b, .. }) => {
            if base.column(&a.name).is_none() {
                return Err(missing("column", &a.name));
            }
            implied(ObjectKind::Column, &a.name, &b.name);
            let mut t = apply_renames(base, &renames, true);
            if let Some(slot) = t.columns.iter_mut().find(|c| c.name == b.name) {
                *slot = b.clone();
            }
            t
        }
        (ObjectDef::Index { index: a, .. }, ObjectDef::Index { index: b, .. }) => {
            if !base.indexes.iter().any(|i| i.name == a.name) {
                return Err(missing("index", &a.name));
            }
            implied(ObjectKind::Index, &a.name, &b.name);
            let mut t = apply_renames(base, &renames, true);
            if let Some(slot) = t.indexes.iter_mut().find(|i| i.name == b.name) {
                *slot = b.clone();
            }
            t
        }
        (
            ObjectDef::ForeignKey { foreign_key: a, .. },
            ObjectDef::ForeignKey { foreign_key: b, .. },
        ) => {
            let mut t = base.clone();
            let slot = t
                .foreign_keys
                .iter_mut()
                .find(|fk| fk.name == a.name)
                .ok_or_else(|| missing("foreign key", label(a.name.as_deref())))?;
            *slot = b.clone();
            t
        }
        (ObjectDef::Check { check: a, .. }, ObjectDef::Check { check: b, .. }) => {
            let mut t = base.clone();
            let slot = t
                .checks
                .iter_mut()
                .find(|c| c.name == a.name)
                .ok_or_else(|| missing("check", label(a.name.as_deref())))?;
            *slot = b.clone();
            t
        }
        _ => return Err(not_designer_kind(old.kind())),
    };
    Ok((target, renames))
}

/// `t` with the renames applied: the table, columns (and the column lists of the primary key,
/// indexes and foreign keys) and, when `indexes`, index names. Foreign keys and checks keep their
/// names (renaming them is a drop plus an add on MySQL / MariaDB).
fn apply_renames(t: &TableDef, renames: &[Rename], indexes: bool) -> TableDef {
    let mut out = t.clone();
    let find = |kind: ObjectKind, name: &str| {
        renames
            .iter()
            .find(|r| r.kind == kind && r.old == name)
            .map(|r| r.new.clone())
    };
    let column = |name: &mut String| {
        if let Some(new) = find(ObjectKind::Column, name) {
            *name = new;
        }
    };
    if let Some(new) = find(ObjectKind::Table, &out.obj.name) {
        out.obj.name = new;
    }
    for c in &mut out.columns {
        column(&mut c.name);
    }
    if let Some(pk) = &mut out.primary_key {
        pk.columns.iter_mut().for_each(column);
    }
    for idx in &mut out.indexes {
        if indexes && let Some(new) = find(ObjectKind::Index, &idx.name) {
            idx.name = new;
        }
        for part in idx.columns.iter_mut().filter(|p| !p.is_expression) {
            column(&mut part.name);
        }
    }
    for fk in &mut out.foreign_keys {
        fk.columns.iter_mut().for_each(column);
    }
    out
}

/// One generation: the server's version and features, and the quoting.
struct Gen {
    f: Features,
    v: ServerVersion,
    q: BacktickQuoting,
}

/// A non-transactional step (MySQL / MariaDB DDL commits implicitly).
fn step(sql: String, description: String) -> ScriptStep {
    ScriptStep::new(sql)
        .described(description)
        .non_transactional()
}

/// A script of one step.
fn one(step: ScriptStep) -> Script {
    let mut script = Script::new();
    script.push_step(step);
    script
}

fn fk_action(fk: &ForeignKeyDef, action: FkAction) -> Result<&'static str> {
    Ok(match action {
        FkAction::NoAction => "NO ACTION",
        FkAction::Restrict => "RESTRICT",
        FkAction::Cascade => "CASCADE",
        FkAction::SetNull => "SET NULL",
        FkAction::SetDefault => {
            return Err(Error::unsupported(format!(
                "foreign key {}: InnoDB has no SET DEFAULT action",
                label(fk.name.as_deref())
            )));
        }
    })
}

impl Gen {
    fn new(mariadb: bool, ctx: &DdlContext<'_>) -> Self {
        Self {
            f: Features::new(mariadb, ctx.version),
            v: ctx.version.clone(),
            q: BacktickQuoting,
        }
    }

    fn rules(&self) -> MySqlRules {
        MySqlRules {
            mariadb: self.f.mariadb,
        }
    }

    fn ident(&self, name: &str) -> String {
        self.q.quote_ident(name)
    }

    fn idents<S: AsRef<str>>(&self, names: impl IntoIterator<Item = S>) -> String {
        names
            .into_iter()
            .map(|n| self.ident(n.as_ref()))
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn literal(&self, text: &str) -> String {
        self.q.quote_str(text)
    }

    /// `db`.`name` of a table (a schema, which MySQL does not have, is ignored).
    fn qualified(&self, obj: &ObjectRef) -> String {
        self.q.qualify(&ObjectRef::table(
            obj.catalog.as_deref(),
            None,
            obj.name.clone(),
        ))
    }

    fn unenforced_checks(&self) -> String {
        format!(
            "{} before {} parses CHECK constraints but neither stores nor enforces them.",
            self.f.engine_name(),
            if self.f.mariadb { "10.2.1" } else { "8.0.16" }
        )
    }

    fn drop_check(&self, name: &str) -> String {
        if self.f.mariadb {
            format!("DROP CONSTRAINT {}", self.ident(name))
        } else {
            format!("DROP CHECK {}", self.ident(name))
        }
    }

    // ---- pieces ------------------------------------------------------------------------------

    /// The full column definition: name, type, character set, collation, generated expression,
    /// nullability, default, `ON UPDATE`, `AUTO_INCREMENT`, visibility, comment.
    fn column_def(&self, c: &ColumnDef) -> Result<String> {
        if c.name.is_empty() {
            return Err(Error::invalid_input("a column needs a name"));
        }
        if c.identity.is_some() {
            return Err(Error::unsupported(format!(
                "column {}: {} has no identity columns; use AUTO_INCREMENT",
                c.name,
                self.f.engine_name()
            )));
        }
        let mut s = format!(
            "{} {}",
            self.ident(&c.name),
            column_type(c, self.f.mariadb, &self.v)?
        );
        if !matches!(c.data_type, DataType::Json { .. }) {
            if let Some(charset) = text_extra(&c.extras, &props::CHARSET) {
                s.push_str(" CHARACTER SET ");
                s.push_str(word(charset, "character set")?);
            }
            if let Some(collation) = &c.collation {
                s.push_str(" COLLATE ");
                s.push_str(word(collation, "collation")?);
            }
        }
        if let Some(generated) = &c.generated {
            s.push_str(" GENERATED ALWAYS AS (");
            s.push_str(&generated.expression);
            s.push_str(if generated.stored {
                ") STORED"
            } else {
                ") VIRTUAL"
            });
        }
        s.push_str(if c.nullable { " NULL" } else { " NOT NULL" });
        if c.generated.is_none()
            && !c.auto_increment
            && let Some(default) = c.default.as_deref().filter(|d| !d.trim().is_empty())
        {
            s.push_str(" DEFAULT ");
            s.push_str(default);
        }
        if let Some(on_update) = text_extra(&c.extras, &ON_UPDATE_EXPRESSION) {
            s.push_str(" ON UPDATE ");
            s.push_str(on_update);
        }
        if c.auto_increment {
            s.push_str(" AUTO_INCREMENT");
        }
        if c.extras.get(&INVISIBLE) == Some(&Value::Bool(true)) {
            if !self.f.invisible_columns {
                return Err(Error::unsupported(format!(
                    "column {}: invisible columns need MySQL 8.0.23 or MariaDB 10.3.3",
                    c.name
                )));
            }
            s.push_str(" INVISIBLE");
        }
        if let Some(comment) = c.comment.as_deref().filter(|t| !t.is_empty()) {
            s.push_str(" COMMENT ");
            s.push_str(&self.literal(comment));
        }
        Ok(s)
    }

    /// `(key parts)` of an index.
    fn index_parts(&self, idx: &IndexDef) -> Result<String> {
        if idx.columns.is_empty() {
            return Err(Error::invalid_input(format!(
                "index {} has no columns",
                idx.name
            )));
        }
        let mut parts = Vec::with_capacity(idx.columns.len());
        for part in &idx.columns {
            let mut p = if part.is_expression {
                if !self.f.expression_indexes {
                    return Err(Error::unsupported(format!(
                        "index {}: expression keys need MySQL 8.0.13 ({} here)",
                        idx.name,
                        self.f.engine_name()
                    )));
                }
                format!("({})", part.name)
            } else {
                self.ident(&part.name)
            };
            if let Some(len) = part.length {
                if part.is_expression {
                    return Err(Error::invalid_input(format!(
                        "index {}: an expression key has no prefix length",
                        idx.name
                    )));
                }
                p.push_str(&format!("({len})"));
            }
            if part.descending {
                if !self.f.descending_indexes {
                    return Err(Error::unsupported(format!(
                        "index {}: descending keys need MySQL 8.0 or MariaDB 10.8",
                        idx.name
                    )));
                }
                p.push_str(" DESC");
            }
            parts.push(p);
        }
        Ok(format!("({})", parts.join(", ")))
    }

    /// `[UNIQUE | FULLTEXT | SPATIAL] INDEX name (parts) [USING ...] [COMMENT ...] [INVISIBLE]`.
    fn index_clause(&self, idx: &IndexDef) -> Result<String> {
        if idx.name.is_empty() {
            return Err(Error::invalid_input("an index needs a name"));
        }
        if idx.name.eq_ignore_ascii_case(PRIMARY) {
            return Err(Error::invalid_input(
                "PRIMARY is the name of the primary key; give the index another name",
            ));
        }
        if idx.predicate.is_some() {
            return Err(Error::unsupported(format!(
                "index {}: {} has no partial indexes",
                idx.name,
                self.f.engine_name()
            )));
        }
        let method = text_extra(&idx.extras, &props::INDEX_METHOD).map(str::to_ascii_uppercase);
        let (head, using) = match method.as_deref() {
            None => (if idx.unique { "UNIQUE INDEX" } else { "INDEX" }, None),
            Some("FULLTEXT" | "SPATIAL") if idx.unique => {
                return Err(Error::invalid_input(format!(
                    "index {}: a {} index cannot be unique",
                    idx.name,
                    method.as_deref().unwrap_or_default()
                )));
            }
            Some("FULLTEXT") => ("FULLTEXT INDEX", None),
            Some("SPATIAL") => ("SPATIAL INDEX", None),
            Some(m @ ("BTREE" | "HASH")) => {
                (if idx.unique { "UNIQUE INDEX" } else { "INDEX" }, Some(m))
            }
            Some(other) => {
                return Err(Error::invalid_input(format!(
                    "index {}: unknown index method '{other}'",
                    idx.name
                )));
            }
        };
        let mut s = format!(
            "{head} {} {}",
            self.ident(&idx.name),
            self.index_parts(idx)?
        );
        if let Some(using) = using {
            s.push_str(" USING ");
            s.push_str(using);
        }
        if let Some(comment) = text_extra(&idx.extras, &props::COMMENT).filter(|c| !c.is_empty()) {
            s.push_str(" COMMENT ");
            s.push_str(&self.literal(comment));
        }
        if idx.extras.get(&INVISIBLE) == Some(&Value::Bool(true)) {
            if !self.f.invisible_indexes {
                return Err(Error::unsupported(format!(
                    "index {}: invisible indexes need MySQL 8.0 or MariaDB 10.6",
                    idx.name
                )));
            }
            s.push_str(if self.f.mariadb {
                " IGNORED"
            } else {
                " INVISIBLE"
            });
        }
        Ok(s)
    }

    /// `[CONSTRAINT name] FOREIGN KEY (cols) REFERENCES t (cols) ON DELETE .. ON UPDATE ..`.
    /// A target without a database is in `table`'s database.
    fn fk_clause(&self, table: &ObjectRef, fk: &ForeignKeyDef) -> Result<String> {
        let name = label(fk.name.as_deref());
        if fk.columns.is_empty() || fk.columns.len() != fk.ref_columns.len() {
            return Err(Error::invalid_input(format!(
                "foreign key {name}: {} columns reference {} columns",
                fk.columns.len(),
                fk.ref_columns.len()
            )));
        }
        let mut target = fk.ref_table.clone();
        if target.catalog.is_none() {
            target.catalog.clone_from(&table.catalog);
        }
        let mut s = String::new();
        if let Some(name) = fk.name.as_deref().filter(|n| !n.is_empty()) {
            s.push_str("CONSTRAINT ");
            s.push_str(&self.ident(name));
            s.push(' ');
        }
        s.push_str(&format!(
            "FOREIGN KEY ({}) REFERENCES {} ({}) ON DELETE {} ON UPDATE {}",
            self.idents(&fk.columns),
            self.qualified(&target),
            self.idents(&fk.ref_columns),
            fk_action(fk, fk.on_delete)?,
            fk_action(fk, fk.on_update)?
        ));
        Ok(s)
    }

    /// `[CONSTRAINT name] CHECK (expr) [NOT ENFORCED]`.
    fn check_clause(&self, check: &CheckDef) -> Result<String> {
        if check.expression.trim().is_empty() {
            return Err(Error::invalid_input(format!(
                "check {} has no expression",
                label(check.name.as_deref())
            )));
        }
        let mut s = String::new();
        if let Some(name) = check.name.as_deref().filter(|n| !n.is_empty()) {
            s.push_str("CONSTRAINT ");
            s.push_str(&self.ident(name));
            s.push(' ');
        }
        s.push_str("CHECK (");
        s.push_str(&check.expression);
        s.push(')');
        if check.extras.get(&ENFORCED) == Some(&Value::Bool(false)) {
            if self.f.mariadb {
                return Err(Error::unsupported(format!(
                    "check {}: MariaDB has no NOT ENFORCED checks",
                    label(check.name.as_deref())
                )));
            }
            s.push_str(" NOT ENFORCED");
        }
        Ok(s)
    }

    /// Table options for `CREATE TABLE` (all of them) or an `ALTER` (`only_changed` from `old`).
    fn table_options(
        &self,
        new: &TableDef,
        old: Option<&TableDef>,
        script: &mut Script,
    ) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let changed = |prop: &PropId| changed_extra(new, old, prop);
        if let Some(engine) = changed(&props::STORAGE_ENGINE) {
            out.push(format!("ENGINE = {}", word(engine, "storage engine")?));
        }
        let charset = changed(&props::CHARSET);
        let collation = changed(&props::COLLATION);
        if charset.is_some() || collation.is_some() {
            let mut s = String::new();
            if let Some(cs) = text_extra(&new.extras, &props::CHARSET) {
                s.push_str("DEFAULT CHARSET = ");
                s.push_str(word(cs, "character set")?);
            }
            if let Some(coll) = text_extra(&new.extras, &props::COLLATION) {
                if !s.is_empty() {
                    s.push(' ');
                }
                s.push_str("COLLATE = ");
                s.push_str(word(coll, "collation")?);
            }
            out.push(s);
        }
        if let Some(format) = changed(&props::ROW_FORMAT) {
            out.push(format!("ROW_FORMAT = {}", word(format, "row format")?));
        }
        let versioned = |t: &TableDef| {
            text_extra(&t.extras, &props::TABLE_TYPE)
                .is_some_and(|t| t.eq_ignore_ascii_case("system versioned"))
        };
        match old {
            None if versioned(new) => {
                if !self.f.mariadb {
                    return Err(Error::unsupported("MySQL has no system-versioned tables"));
                }
                out.push("WITH SYSTEM VERSIONING".to_owned());
            }
            Some(o)
                if versioned(o) != versioned(new) && new.extras.contains(&props::TABLE_TYPE) =>
            {
                return Err(Error::unsupported(
                    "changing system versioning is not supported by the designer",
                ));
            }
            _ => {}
        }
        for (prop, value) in &new.extras {
            let known = [
                props::STORAGE_ENGINE,
                props::CHARSET,
                props::COLLATION,
                props::ROW_FORMAT,
                props::TABLE_TYPE,
                props::SIZE_BYTES,
            ];
            if known.contains(prop) || *value == Value::Bool(false) || *value == Value::Null {
                continue;
            }
            if old.is_none_or(|o| o.extras.get(prop) != Some(value)) {
                script.warn(format!(
                    "Table property {prop} has no MySQL / MariaDB equivalent and was skipped."
                ));
            }
        }
        let comment = new.comment.as_deref().unwrap_or("");
        let old_comment = old.map(|o| o.comment.as_deref().unwrap_or(""));
        if old_comment.map_or(!comment.is_empty(), |before| before != comment) {
            out.push(format!("COMMENT = {}", self.literal(comment)));
        }
        Ok(out)
    }

    // ---- create ------------------------------------------------------------------------------

    fn create_table(&self, t: &TableDef) -> Result<Script> {
        if t.obj.name.is_empty() {
            return Err(Error::invalid_input("a table needs a name"));
        }
        if t.columns.is_empty() {
            return Err(Error::invalid_input(format!(
                "table {} has no columns",
                t.obj.name
            )));
        }
        let mut script = Script::new();
        let mut lines = Vec::new();
        for c in &t.columns {
            lines.push(self.column_def(c)?);
        }
        if let Some(pk) = &t.primary_key {
            lines.push(self.pk_clause(pk)?);
        }
        for idx in &t.indexes {
            lines.push(self.index_clause(idx)?);
        }
        for check in &t.checks {
            lines.push(self.check_clause(check)?);
        }
        for fk in &t.foreign_keys {
            lines.push(self.fk_clause(&t.obj, fk)?);
        }
        let options = self.table_options(t, None, &mut script)?;
        let mut sql = format!(
            "CREATE TABLE {} (\n  {}\n)",
            self.qualified(&t.obj),
            lines.join(",\n  ")
        );
        if !options.is_empty() {
            sql.push(' ');
            sql.push_str(&options.join(" "));
        }
        script.push_step(step(sql, format!("Create table {}", t.obj.name)));
        Ok(script)
    }

    fn pk_clause(&self, pk: &PrimaryKeyDef) -> Result<String> {
        if pk.columns.is_empty() {
            return Err(Error::invalid_input("the primary key has no columns"));
        }
        Ok(format!("PRIMARY KEY ({})", self.idents(&pk.columns)))
    }

    // ---- alter -------------------------------------------------------------------------------

    fn alter_table(&self, old: &TableDef, new: &TableDef, renames: &[Rename]) -> Result<Script> {
        if new.columns.is_empty() {
            return Err(Error::invalid_input(format!(
                "table {} would have no columns",
                new.obj.name
            )));
        }
        let rules = self.rules();
        let mut script = Script::new();
        let target = self.qualified(&new.obj);

        // 1. The table's own rename.
        if old.obj.name != new.obj.name
            || (new.obj.catalog.is_some() && old.obj.catalog != new.obj.catalog)
        {
            script.push_step(step(
                format!("RENAME TABLE {} TO {target}", self.qualified(&old.obj)),
                format!("Rename table {} to {}", old.obj.name, new.obj.name),
            ));
        }

        // 2. Column and index renames.
        let mut effective = Vec::new();
        let mut rename_clauses = Vec::new();
        for r in renames.iter().filter(|r| r.old != r.new) {
            match r.kind {
                ObjectKind::Column => {
                    let Some(before) = old.column(&r.old) else {
                        continue;
                    };
                    if new.column(&r.new).is_none() {
                        continue;
                    }
                    rename_clauses.push(if self.f.rename_column {
                        format!(
                            "RENAME COLUMN {} TO {}",
                            self.ident(&r.old),
                            self.ident(&r.new)
                        )
                    } else {
                        let mut renamed = before.clone();
                        renamed.name.clone_from(&r.new);
                        format!(
                            "CHANGE COLUMN {} {}",
                            self.ident(&r.old),
                            self.column_def(&renamed)?
                        )
                    });
                    effective.push(r.clone());
                }
                ObjectKind::Index if self.f.rename_index => {
                    if old.indexes.iter().any(|i| i.name == r.old)
                        && new.indexes.iter().any(|i| i.name == r.new)
                    {
                        rename_clauses.push(format!(
                            "RENAME INDEX {} TO {}",
                            self.ident(&r.old),
                            self.ident(&r.new)
                        ));
                        effective.push(r.clone());
                    }
                }
                _ => {}
            }
        }
        if !rename_clauses.is_empty() {
            script.push_step(step(
                format!("ALTER TABLE {target}\n  {}", rename_clauses.join(",\n  ")),
                format!("Rename columns and indexes of {}", new.obj.name),
            ));
        }
        let mut renamed = apply_renames(old, &effective, true);
        renamed.obj.clone_from(&new.obj);
        let (o, n) = normalize_pair(&renamed, new);

        // 3. Foreign keys and checks that change or go.
        let fk_pairs = pair_fks(&o.foreign_keys, &n.foreign_keys, rules, &new.obj);
        let check_pairs = pair_checks(&o.checks, &n.checks, rules);
        let mut drop_clauses = Vec::new();
        let mut dropped_fks: Vec<&ForeignKeyDef> = Vec::new();
        for fk in fk_pairs.dropped.iter().map(|&i| &o.foreign_keys[i]) {
            let name = fk
                .name
                .as_deref()
                .ok_or_else(|| Error::invalid_input("cannot drop a foreign key without a name"))?;
            drop_clauses.push(format!("DROP FOREIGN KEY {}", self.ident(name)));
            dropped_fks.push(fk);
        }
        for check in check_pairs.dropped.iter().map(|&i| &o.checks[i]) {
            let name = check
                .name
                .as_deref()
                .ok_or_else(|| Error::invalid_input("cannot drop a check without a name"))?;
            drop_clauses.push(self.drop_check(name));
        }
        if !drop_clauses.is_empty() {
            script.push_step(
                step(
                    format!("ALTER TABLE {target}\n  {}", drop_clauses.join(",\n  ")),
                    format!("Drop changed or removed constraints of {}", new.obj.name),
                )
                .destructive(),
            );
        }

        // 4. Everything else in one statement.
        let mut main = Main::default();
        let changed_indexes: Vec<&IndexDef> = n
            .indexes
            .iter()
            .filter(|idx| match o.indexes.iter().find(|i| i.name == idx.name) {
                None => true,
                Some(before) => !diff_indexes(&new.obj, before, idx, &rules).is_empty(),
            })
            .collect();
        for idx in &o.indexes {
            let gone = !n.indexes.iter().any(|i| i.name == idx.name);
            if gone || changed_indexes.iter().any(|c| c.name == idx.name) {
                main.push(
                    format!("DROP INDEX {}", self.ident(&idx.name)),
                    format!("drop index {}", idx.name),
                    gone && idx.unique,
                );
            }
        }
        // The index InnoDB made for a foreign key that goes, when the new table has no such index.
        for fk in &dropped_fks {
            if let Some(name) = fk.name.as_deref()
                && renamed
                    .indexes
                    .iter()
                    .any(|i| i.name == name && is_fk_index(i, fk))
                && !o.indexes.iter().any(|i| i.name == name)
                && !new.indexes.iter().any(|i| i.name == name)
                && !n
                    .foreign_keys
                    .iter()
                    .any(|f| f.name.as_deref() == Some(name))
            {
                main.push(
                    format!("DROP INDEX {}", self.ident(name)),
                    format!("drop index {name}"),
                    false,
                );
            }
        }
        let pk_changed = !same_pk(o.primary_key.as_ref(), n.primary_key.as_ref());
        if pk_changed && o.primary_key.is_some() {
            main.push(
                "DROP PRIMARY KEY".to_owned(),
                "drop the primary key".to_owned(),
                true,
            );
        }
        for c in o.columns.iter().filter(|c| n.column(&c.name).is_none()) {
            main.push(
                format!("DROP COLUMN {}", self.ident(&c.name)),
                format!("drop column {}", c.name),
                true,
            );
        }
        self.column_changes(&o, &n, &mut main)?;
        if pk_changed && let Some(pk) = &n.primary_key {
            main.push(
                format!("ADD {}", self.pk_clause(pk)?),
                "add the primary key".to_owned(),
                false,
            );
        }
        for idx in &changed_indexes {
            main.push(
                format!("ADD {}", self.index_clause(idx)?),
                format!("add index {}", idx.name),
                false,
            );
        }
        for check in check_pairs.added.iter().map(|&i| &n.checks[i]) {
            main.push(
                format!("ADD {}", self.check_clause(check)?),
                format!("add check {}", label(check.name.as_deref())),
                false,
            );
        }
        for fk in fk_pairs.added.iter().map(|&i| &n.foreign_keys[i]) {
            main.push(
                format!("ADD {}", self.fk_clause(&new.obj, fk)?),
                format!("add foreign key {}", label(fk.name.as_deref())),
                false,
            );
        }
        for option in self.table_options(&n, Some(&o), &mut script)? {
            let what = if option.starts_with("COMMENT") {
                "set the table comment"
            } else {
                "set table options"
            };
            main.push(option, what.to_owned(), false);
        }
        if !main.clauses.is_empty() {
            let mut step = step(
                format!("ALTER TABLE {target}\n  {}", main.clauses.join(",\n  ")),
                format!("Alter table {}: {}", new.obj.name, main.summary.join(", ")),
            );
            if main.destructive {
                step = step.destructive();
            }
            script.push_step(step);
        }

        if !check_pairs.added.is_empty() && !self.f.checks {
            script.warn(self.unenforced_checks());
        }
        if script.len() > 1 {
            script.warn(NOT_ATOMIC);
        }
        Ok(script)
    }

    /// `MODIFY COLUMN` and `ADD COLUMN` clauses, in the new column order. Kept columns outside
    /// the longest common subsequence of the old and new orders are moved (`FIRST` / `AFTER`
    /// the new predecessor); a new column gets a position unless it is the last one.
    fn column_changes(&self, o: &TableDef, n: &TableDef, main: &mut Main) -> Result<()> {
        let rules = self.rules();
        let old_kept: Vec<&str> = o
            .columns
            .iter()
            .filter(|c| n.column(&c.name).is_some())
            .map(|c| c.name.as_str())
            .collect();
        let new_kept: Vec<&str> = n
            .columns
            .iter()
            .filter(|c| o.column(&c.name).is_some())
            .map(|c| c.name.as_str())
            .collect();
        let stay = longest_common_subsequence(&old_kept, &new_kept);
        let last = n.columns.len() - 1;
        for (pos, col) in n.columns.iter().enumerate() {
            let position = match pos.checked_sub(1) {
                None => " FIRST".to_owned(),
                Some(p) => format!(" AFTER {}", self.ident(&n.columns[p].name)),
            };
            match o.column(&col.name) {
                None => main.push(
                    format!(
                        "ADD COLUMN {}{}",
                        self.column_def(col)?,
                        if pos == last { "" } else { position.as_str() }
                    ),
                    format!("add column {}", col.name),
                    false,
                ),
                Some(before) => {
                    let moved = !stay.contains(&col.name.as_str());
                    let changed = !diff_columns(&n.obj, before, col, &rules).is_empty();
                    if !moved && !changed {
                        continue;
                    }
                    let merged = keep_attributes(before, col);
                    let narrowing = !rules.same_type(before, col) && is_narrowing(before, col);
                    let mut what = format!("modify column {}", col.name);
                    if moved {
                        what.push_str(" (move)");
                    }
                    main.push(
                        format!(
                            "MODIFY COLUMN {}{}",
                            self.column_def(&merged)?,
                            if moved { position.as_str() } else { "" }
                        ),
                        what,
                        narrowing,
                    );
                }
            }
        }
        Ok(())
    }
}

/// The items of a longest common subsequence of `a` and `b`.
fn longest_common_subsequence<'a>(a: &[&'a str], b: &[&str]) -> Vec<&'a str> {
    let mut len = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            len[i][j] = if a[i] == b[j] {
                len[i + 1][j + 1] + 1
            } else {
                len[i + 1][j].max(len[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::with_capacity(len[0][0]);
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            out.push(a[i]);
            i += 1;
            j += 1;
        } else if len[i + 1][j] >= len[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    out
}

/// The clauses of the main `ALTER TABLE`.
#[derive(Default)]
struct Main {
    clauses: Vec<String>,
    summary: Vec<String>,
    destructive: bool,
}

impl Main {
    fn push(&mut self, clause: String, what: String, destructive: bool) {
        self.clauses.push(clause);
        self.summary.push(what);
        self.destructive |= destructive;
    }
}

/// `new` with what MySQL would otherwise reset on `MODIFY` kept from `old` when `new` does not
/// say: the character set and collation of a text column, `ON UPDATE` and visibility.
fn keep_attributes(old: &ColumnDef, new: &ColumnDef) -> ColumnDef {
    let mut out = new.clone();
    let texty =
        |c: &ColumnDef| c.data_type.is_textual() || matches!(c.data_type, DataType::Enum { .. });
    if texty(old) && texty(new) {
        if out.collation.is_none() {
            out.collation.clone_from(&old.collation);
        }
        if !out.extras.contains(&props::CHARSET)
            && let Some(cs) = old.extras.get(&props::CHARSET)
        {
            out.extras.insert(props::CHARSET, cs.clone());
        }
    }
    for prop in [ON_UPDATE_EXPRESSION, INVISIBLE] {
        if !out.extras.contains(&prop)
            && let Some(v) = old.extras.get(&prop)
        {
            out.extras.insert(prop, v.clone());
        }
    }
    out
}

fn same_pk(old: Option<&PrimaryKeyDef>, new: Option<&PrimaryKeyDef>) -> bool {
    match (old, new) {
        (None, None) => true,
        (Some(a), Some(b)) => a.columns == b.columns,
        _ => false,
    }
}

/// How old and new constraints pair up: indexes into the old list to drop, into the new list to
/// add (a changed constraint is in both).
#[derive(Debug, Default)]
struct Pairing {
    dropped: Vec<usize>,
    added: Vec<usize>,
}

/// Pairs items by name (both named), then by content where one side is unnamed.
fn pair_by<T>(
    old: &[T],
    new: &[T],
    name_of: impl Fn(&T) -> Option<&str>,
    same_content: impl Fn(&T, &T) -> bool,
    unchanged: impl Fn(&T, &T) -> bool,
) -> Pairing {
    let mut used = vec![false; old.len()];
    let mut matched: Vec<Option<usize>> = vec![None; new.len()];
    for (pos, item) in new.iter().enumerate() {
        if let Some(wanted) = name_of(item)
            && let Some(found) =
                (0..old.len()).find(|&i| !used[i] && name_of(&old[i]) == Some(wanted))
        {
            used[found] = true;
            matched[pos] = Some(found);
        }
    }
    for (pos, item) in new.iter().enumerate() {
        if matched[pos].is_some() {
            continue;
        }
        if let Some(found) = (0..old.len()).find(|&i| {
            !used[i]
                && (name_of(&old[i]).is_none() || name_of(item).is_none())
                && same_content(&old[i], item)
        }) {
            used[found] = true;
            matched[pos] = Some(found);
        }
    }
    let mut out = Pairing::default();
    for (pos, found) in matched.iter().enumerate() {
        match found {
            Some(i) if unchanged(&old[*i], &new[pos]) => {}
            Some(i) => {
                out.dropped.push(*i);
                out.added.push(pos);
            }
            None => out.added.push(pos),
        }
    }
    for (i, was_used) in used.iter().enumerate() {
        if !was_used {
            out.dropped.push(i);
        }
    }
    out.dropped.sort_unstable();
    out
}

fn pair_fks(
    old: &[ForeignKeyDef],
    new: &[ForeignKeyDef],
    rules: MySqlRules,
    table: &ObjectRef,
) -> Pairing {
    pair_by(
        old,
        new,
        |fk| fk.name.as_deref(),
        |a, b| {
            a.columns == b.columns
                && a.ref_columns == b.ref_columns
                && rules.same_table_ref(&a.ref_table, &b.ref_table)
        },
        |a, b| {
            diff_foreign_keys(table, a, b, &rules)
                .iter()
                .all(|d| d.prop.as_ref() == Some(&props::NAME))
        },
    )
}

fn pair_checks(old: &[CheckDef], new: &[CheckDef], rules: MySqlRules) -> Pairing {
    let enforced = |c: &CheckDef| c.extras.get(&ENFORCED) != Some(&Value::Bool(false));
    pair_by(
        old,
        new,
        |c| c.name.as_deref(),
        |a, b| rules.same_expression(&a.expression, &b.expression),
        |a, b| rules.same_expression(&a.expression, &b.expression) && enforced(a) == enforced(b),
    )
}

/// Text extra `prop` of `new` when `old` is `None` or has another value (case-insensitive).
fn changed_extra<'a>(new: &'a TableDef, old: Option<&TableDef>, prop: &PropId) -> Option<&'a str> {
    let after = text_extra(&new.extras, prop)?;
    match old.and_then(|o| text_extra(&o.extras, prop)) {
        Some(before) if before.eq_ignore_ascii_case(after) => None,
        _ => Some(after),
    }
}

/// A text extra, if set.
fn text_extra<'a>(extras: &'a Extras, prop: &PropId) -> Option<&'a str> {
    match extras.get(prop) {
        Some(Value::Text(t)) if !t.trim().is_empty() => Some(t.as_str()),
        _ => None,
    }
}

/// `text` when it is a plain word (character set, collation, engine, row format names).
fn word<'a>(text: &'a str, what: &str) -> Result<&'a str> {
    if !text.is_empty() && text.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        Ok(text)
    } else {
        Err(Error::invalid_input(format!(
            "'{text}' is not a valid {what} name"
        )))
    }
}

#[cfg(test)]
mod tests;
