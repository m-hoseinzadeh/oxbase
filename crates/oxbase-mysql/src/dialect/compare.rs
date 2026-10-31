//! The MySQL / MariaDB [`Comparator`] cells (M3): the shared structural comparison of
//! `oxbase_dialect::compare` with [`MySqlRules`], after [`normalize_pair`] has removed what these
//! servers do not store.
//!
//! # Rules ([`MySqlRules`])
//!
//! - **Types** compare by [`canonical_type`]: display widths (`int(11)` on MariaDB, `int` on MySQL
//!   8), aliases (`integer`, `boolean`, `numeric`, `real`) and spacing do not count. A column
//!   without a native type is compared by the type the generator would write for it. MariaDB's
//!   `json` is `longtext` plus a check; a `Json` column matches either spelling.
//! - **Defaults** compare by their canonical expression ([`super::expr`]): MySQL's
//!   `CURRENT_TIMESTAMP`, MariaDB's `current_timestamp()` and `now()` are one default; a quoted
//!   number on a numeric column is that number (`'5'` = `5`, `0` = `0.00` on a decimal);
//!   `TRUE` / `FALSE` are `1` / `0`; `NULL` is no default.
//! - **Expressions** (checks, generated columns) compare by canonical text, so the server's
//!   rewrite (`` (`qty` >= 0) ``, `_utf8mb4'x'`) equals what was written.
//! - Extras that are facts rather than definitions (`size_bytes`, the `primary_key` marker of
//!   the `PRIMARY` index) are not compared.
//!
//! # Normalization ([`normalize_pair`])
//!
//! Applied to both sides before the walk, because these are not differences on this engine:
//!
//! - every primary key is named `PRIMARY`, so primary key names are dropped;
//! - a unique index is the same thing as a `UNIQUE` constraint (`constraint` = `unique`);
//! - `RESTRICT` and `NO ACTION` are the same foreign-key action in InnoDB;
//! - the index InnoDB creates for a foreign key that has none (named after the constraint, on
//!   its columns) is left out when only one side has it;
//! - text extras that the server spells in its own case (`INDEX_METHOD`, `ENGINE`, `ROW_FORMAT`)
//!   and `ON UPDATE` expressions compare in canonical form.

use oxbase_dialect::Comparator;
use oxbase_dialect::compare::{CompareRules, diff_objects};
use oxbase_types::{
    ColumnDef, DataType, Extras, FkAction, ForeignKeyDef, IndexDef, ObjectDef, ObjectKind, PropId,
    PropertyDiff, ServerVersion, TableDef, Value, props,
};

use super::ON_UPDATE_EXPRESSION;
use super::expr::{Tok, canonical, canonical_tokens, render};
use super::native::{canonical_type, from_data_type};

/// How MySQL and MariaDB spell types, defaults and expressions (see the module docs).
#[derive(Debug, Clone, Copy)]
pub(crate) struct MySqlRules {
    pub mariadb: bool,
}

impl MySqlRules {
    /// The canonical type of `c`: its native type, else the one the generator derives.
    fn canonical(self, c: &ColumnDef) -> Option<String> {
        if c.native_type.trim().is_empty() {
            // The version only matters for `uuid`, which then exists.
            let v = ServerVersion::new(99, 0, 0);
            from_data_type(&c.data_type, self.mariadb, &v)
                .ok()
                .map(|t| canonical_type(&t))
        } else {
            Some(canonical_type(&c.native_type))
        }
    }
}

impl CompareRules for MySqlRules {
    fn same_type(&self, old: &ColumnDef, new: &ColumnDef) -> bool {
        let json = |c: &ColumnDef| matches!(c.data_type, DataType::Json { .. });
        if json(old) || json(new) {
            let json_text = |c: &ColumnDef| {
                json(c) || matches!(self.canonical(c).as_deref(), Some("json" | "longtext"))
            };
            // MariaDB: `json` is stored as `longtext` (typed Json by the fetcher).
            return if self.mariadb {
                json_text(old) && json_text(new)
            } else {
                self.canonical(old) == self.canonical(new)
            };
        }
        match (self.canonical(old), self.canonical(new)) {
            (Some(a), Some(b)) => a == b,
            _ => old.data_type == new.data_type,
        }
    }

    fn same_default(&self, old: &ColumnDef, new: &ColumnDef) -> bool {
        let before = old
            .default
            .as_deref()
            .and_then(|d| canonical_default(d, old));
        let after = new
            .default
            .as_deref()
            .and_then(|d| canonical_default(d, new));
        match (before, after) {
            (None, None) => true,
            (Some(a), Some(b)) => a == b,
            _ => old.auto_increment && new.auto_increment,
        }
    }

    fn same_expression(&self, old: &str, new: &str) -> bool {
        canonical(old) == canonical(new)
    }

    fn compare_extra(&self, kind: ObjectKind, prop: &PropId) -> bool {
        if *prop == props::SIZE_BYTES {
            return false;
        }
        kind != ObjectKind::Index || *prop != props::PRIMARY_KEY
    }
}

/// The canonical text of a column default, `None` for "no default" (`NULL`).
pub(crate) fn canonical_default(default: &str, col: &ColumnDef) -> Option<String> {
    let mut tokens = canonical_tokens(default);
    if tokens.is_empty() {
        return None;
    }
    if let [Tok::Word(w)] = tokens.as_slice()
        && w == "null"
    {
        return None;
    }
    if let Some(ts) = current_timestamp(&tokens) {
        return Some(ts);
    }
    let numeric = col.data_type.is_numeric()
        || matches!(col.data_type, DataType::Bool)
        || canonical_type(&col.native_type).starts_with("bit");
    if numeric {
        // `'5'` on a numeric column is the number 5; TRUE / FALSE are 1 / 0.
        let number = match tokens.as_slice() {
            [Tok::Str(s)] => Some(s.trim().to_owned()),
            [Tok::Word(w)] if w == "true" => Some("1".to_owned()),
            [Tok::Word(w)] if w == "false" => Some("0".to_owned()),
            [Tok::Word(w)] => Some(w.clone()),
            [Tok::Sym(sign), Tok::Word(w)] if sign == "-" || sign == "+" => {
                Some(format!("{sign}{w}"))
            }
            _ => None,
        };
        if let Some(n) = number.as_deref().and_then(canonical_number) {
            tokens = vec![Tok::Word(n)];
        }
    }
    Some(render(&tokens))
}

/// `current_timestamp` for `CURRENT_TIMESTAMP`, `current_timestamp()`, `now()`,
/// `localtimestamp` ...; `current_timestamp(n)` with a precision.
fn current_timestamp(tokens: &[Tok]) -> Option<String> {
    const NAMES: [&str; 4] = ["current_timestamp", "now", "localtimestamp", "localtime"];
    let (Tok::Word(name), rest) = tokens.split_first()? else {
        return None;
    };
    if !NAMES.contains(&name.as_str()) {
        return None;
    }
    match rest {
        [] | [Tok::Open, Tok::Close] => Some("current_timestamp".to_owned()),
        [Tok::Open, Tok::Word(p), Tok::Close] if p.chars().all(|c| c.is_ascii_digit()) => {
            if p.trim_start_matches('0').is_empty() {
                Some("current_timestamp".to_owned())
            } else {
                Some(format!("current_timestamp({})", p.trim_start_matches('0')))
            }
        }
        _ => None,
    }
}

/// A plain decimal number in canonical form (`+05.50` → `5.5`, `0.00` → `0`, `-0` → `0`);
/// `None` when `text` is not one.
fn canonical_number(text: &str) -> Option<String> {
    let (negative, body) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let (int, frac) = body.split_once('.').unwrap_or((body, ""));
    if int.is_empty() && frac.is_empty() {
        return None;
    }
    if !int.chars().chain(frac.chars()).all(|c| c.is_ascii_digit()) {
        return None;
    }
    let int = int.trim_start_matches('0');
    let frac = frac.trim_end_matches('0');
    let mut out = String::new();
    if negative && !(int.is_empty() && frac.is_empty()) {
        out.push('-');
    }
    out.push_str(if int.is_empty() { "0" } else { int });
    if !frac.is_empty() {
        out.push('.');
        out.push_str(frac);
    }
    Some(out)
}

/// The MySQL / MariaDB comparator: [`normalize_pair`], then the shared walk with [`MySqlRules`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct MySqlComparator {
    pub rules: MySqlRules,
}

impl Comparator for MySqlComparator {
    fn diff(&self, a: &ObjectDef, b: &ObjectDef) -> Vec<PropertyDiff> {
        let (a, b) = normalize_defs(a, b);
        diff_objects(&a, &b, &self.rules)
    }
}

/// [`normalize_pair`] for any pair of definitions (tables, or one child of each).
fn normalize_defs(a: &ObjectDef, b: &ObjectDef) -> (ObjectDef, ObjectDef) {
    if let (ObjectDef::Table(x), ObjectDef::Table(y)) = (a, b) {
        let (x, y) = normalize_pair(x, y);
        return (ObjectDef::Table(x), ObjectDef::Table(y));
    }
    let mut a = a.clone();
    let mut b = b.clone();
    for def in [&mut a, &mut b] {
        match def {
            ObjectDef::Column { column, .. } => normalize_column(column),
            ObjectDef::Index { index, .. } => normalize_index(index),
            ObjectDef::ForeignKey { foreign_key, .. } => normalize_fk(foreign_key),
            _ => {}
        }
    }
    (a, b)
}

/// Both tables without the differences this engine does not store (see the module docs).
pub(crate) fn normalize_pair(a: &TableDef, b: &TableDef) -> (TableDef, TableDef) {
    let mut a = normalize_table(a);
    let mut b = normalize_table(b);
    let a_only = implied_fk_indexes(&a, &b);
    let b_only = implied_fk_indexes(&b, &a);
    a.indexes.retain(|idx| !a_only.contains(&idx.name));
    b.indexes.retain(|idx| !b_only.contains(&idx.name));
    (a, b)
}

/// Names of indexes of `t` missing from `other` that InnoDB created for a foreign key of `t`
/// (same name as the constraint, exactly its columns).
pub(crate) fn implied_fk_indexes(t: &TableDef, other: &TableDef) -> Vec<String> {
    t.indexes
        .iter()
        .filter(|idx| !other.indexes.iter().any(|o| o.name == idx.name))
        .filter(|idx| {
            t.foreign_keys
                .iter()
                .any(|fk| fk.name.as_deref() == Some(idx.name.as_str()) && is_fk_index(idx, fk))
        })
        .map(|idx| idx.name.clone())
        .collect()
}

/// `true` when `idx` is a plain index on exactly `fk`'s columns.
pub(crate) fn is_fk_index(idx: &IndexDef, fk: &ForeignKeyDef) -> bool {
    !idx.unique
        && idx.predicate.is_none()
        && idx.columns.len() == fk.columns.len()
        && idx.columns.iter().zip(&fk.columns).all(|(part, col)| {
            !part.is_expression && part.length.is_none() && !part.descending && part.name == *col
        })
}

fn normalize_table(t: &TableDef) -> TableDef {
    let mut t = t.clone();
    if let Some(pk) = &mut t.primary_key {
        pk.name = None;
    }
    for prop in [props::STORAGE_ENGINE, props::ROW_FORMAT] {
        upper_extra(&mut t.extras, &prop);
    }
    t.columns.iter_mut().for_each(normalize_column);
    t.indexes.iter_mut().for_each(normalize_index);
    t.foreign_keys.iter_mut().for_each(normalize_fk);
    t
}

fn normalize_column(c: &mut ColumnDef) {
    if let Some(Value::Text(expr)) = c.extras.get(&ON_UPDATE_EXPRESSION) {
        let canon = current_timestamp(&canonical_tokens(expr)).unwrap_or_else(|| canonical(expr));
        c.extras.insert(ON_UPDATE_EXPRESSION, canon);
    }
}

fn normalize_index(idx: &mut IndexDef) {
    idx.is_constraint = idx.unique;
    upper_extra(&mut idx.extras, &props::INDEX_METHOD);
}

fn normalize_fk(fk: &mut ForeignKeyDef) {
    for action in [&mut fk.on_delete, &mut fk.on_update] {
        if *action == FkAction::Restrict {
            *action = FkAction::NoAction;
        }
    }
}

fn upper_extra(extras: &mut Extras, prop: &PropId) {
    if let Some(Value::Text(text)) = extras.get(prop) {
        let upper = text.to_ascii_uppercase();
        extras.insert(prop.clone(), upper);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_dialect::compare::diff_tables;
    use oxbase_types::{CheckDef, GeneratedColumn, IndexColumn, ObjectRef, PrimaryKeyDef};

    fn col(name: &str, native: &str, data_type: DataType) -> ColumnDef {
        let mut c = ColumnDef::new(name, data_type);
        c.native_type = native.into();
        c
    }

    const MYSQL: MySqlRules = MySqlRules { mariadb: false };
    const MARIADB: MySqlRules = MySqlRules { mariadb: true };

    #[test]
    fn types() {
        let int = col("a", "int", DataType::INT32);
        let int11 = col("a", "int(11)", DataType::INT32);
        let bigint = col("a", "bigint", DataType::INT64);
        let derived = col("a", "", DataType::INT32);
        assert!(MYSQL.same_type(&int, &int11));
        assert!(MYSQL.same_type(&int, &derived));
        assert!(!MYSQL.same_type(&int, &bigint));
        let json = col("j", "json", DataType::Json { binary: true });
        let maria_json = col("j", "longtext", DataType::Json { binary: false });
        let text = col("j", "longtext", DataType::Text);
        assert!(MARIADB.same_type(&json, &maria_json));
        assert!(!MYSQL.same_type(&json, &maria_json));
        assert!(!MYSQL.same_type(&json, &text));
        let bool1 = col("b", "boolean", DataType::Bool);
        let tiny1 = col("b", "tinyint(1)", DataType::Bool);
        assert!(MARIADB.same_type(&bool1, &tiny1));
    }

    #[test]
    fn defaults() {
        let with = |native: &str, t: DataType, d: Option<&str>| {
            let mut c = col("c", native, t);
            c.default = d.map(str::to_owned);
            c
        };
        let dec = DataType::Decimal {
            precision: Some(12),
            scale: Some(2),
        };
        let pairs_equal = [
            (with("int", DataType::INT32, Some("'5'")), Some("5")),
            (with("decimal(12,2)", dec.clone(), Some("0")), Some("0.00")),
            (
                with("decimal(12,2)", dec.clone(), Some("'1.5'")),
                Some("1.50"),
            ),
            (with("decimal(12,2)", dec, Some("-0.0")), Some("0")),
            (with("tinyint(1)", DataType::Bool, Some("TRUE")), Some("1")),
            (
                with(
                    "datetime",
                    DataType::Timestamp { precision: None },
                    Some("CURRENT_TIMESTAMP"),
                ),
                Some("current_timestamp()"),
            ),
            (
                with(
                    "datetime(3)",
                    DataType::Timestamp { precision: Some(3) },
                    Some("now(3)"),
                ),
                Some("CURRENT_TIMESTAMP(3)"),
            ),
            (
                with(
                    "varchar(20)",
                    DataType::VarChar { length: Some(20) },
                    Some("'it''s'"),
                ),
                Some("'it\\'s'"),
            ),
            (
                with(
                    "varchar(20)",
                    DataType::VarChar { length: Some(20) },
                    Some("'a\\\\b'"),
                ),
                Some("'a\\\\b'"),
            ),
            (with("int", DataType::INT32, Some("NULL")), None),
            (
                with("json", DataType::Json { binary: true }, Some("('{}')")),
                Some("(_utf8mb4\\'{}\\')"),
            ),
        ];
        for (old, fetched) in pairs_equal {
            let mut new = old.clone();
            new.default = fetched.map(str::to_owned);
            assert!(
                MYSQL.same_default(&old, &new),
                "{:?} vs {fetched:?}",
                old.default
            );
        }
        let text = with(
            "varchar(20)",
            DataType::VarChar { length: Some(20) },
            Some("'5'"),
        );
        let mut five = text.clone();
        five.default = Some("'5.0'".into());
        assert!(!MYSQL.same_default(&text, &five));
        let mut other = text.clone();
        other.default = Some("'x'".into());
        assert!(!MYSQL.same_default(&text, &other));
    }

    #[test]
    fn numbers() {
        assert_eq!(canonical_number("+05.50").as_deref(), Some("5.5"));
        assert_eq!(canonical_number("0.00").as_deref(), Some("0"));
        assert_eq!(canonical_number("-0").as_deref(), Some("0"));
        assert_eq!(canonical_number("-12").as_deref(), Some("-12"));
        assert_eq!(canonical_number(".5").as_deref(), Some("0.5"));
        assert_eq!(canonical_number("1e3"), None);
        assert_eq!(canonical_number("abc"), None);
        assert_eq!(canonical_number("."), None);
    }

    fn table() -> TableDef {
        let mut t = TableDef::new(ObjectRef::table(Some("db"), None, "t"));
        let mut id = col("id", "int", DataType::INT32).not_null();
        id.auto_increment = true;
        t.columns.push(id);
        t.columns.push(col("parent", "int", DataType::INT32));
        let mut ts = col("at", "datetime", DataType::Timestamp { precision: None });
        ts.default = Some("CURRENT_TIMESTAMP".into());
        ts.extras.insert(ON_UPDATE_EXPRESSION, "CURRENT_TIMESTAMP");
        t.columns.push(ts);
        let mut total = col("total", "int", DataType::INT32);
        total.generated = Some(GeneratedColumn {
            expression: "parent * 2".into(),
            stored: true,
        });
        t.columns.push(total);
        t.primary_key = Some(PrimaryKeyDef {
            name: Some("pk_t".into()),
            columns: vec!["id".into()],
        });
        t.indexes.push(IndexDef {
            name: "uq_parent".into(),
            columns: vec![IndexColumn::column("parent")],
            unique: true,
            is_constraint: false,
            predicate: None,
            extras: Extras::new().with(props::INDEX_METHOD, "btree"),
        });
        t.foreign_keys.push(ForeignKeyDef {
            name: Some("fk_t_parent".into()),
            columns: vec!["parent".into()],
            ref_table: ObjectRef::table(None, None, "p"),
            ref_columns: vec!["id".into()],
            on_delete: FkAction::Restrict,
            on_update: FkAction::Cascade,
            extras: Extras::new(),
        });
        t.checks.push(CheckDef {
            name: Some("t_chk".into()),
            expression: "parent > 0 AND parent < 100".into(),
            extras: Extras::new(),
        });
        t
    }

    /// The same table as MySQL 8 reports it after `CREATE TABLE`.
    fn fetched() -> TableDef {
        let mut t = table();
        t.primary_key.as_mut().unwrap().name = Some("PRIMARY".into());
        t.columns[2].default = Some("current_timestamp()".into());
        t.columns[2]
            .extras
            .insert(ON_UPDATE_EXPRESSION, "current_timestamp()");
        t.columns[3].generated.as_mut().unwrap().expression = "(`parent` * 2)".into();
        t.indexes[0].is_constraint = true;
        t.indexes[0].extras.insert(props::INDEX_METHOD, "BTREE");
        t.foreign_keys[0].on_delete = FkAction::NoAction;
        t.foreign_keys[0].ref_table = ObjectRef::table(Some("db"), None, "p");
        t.checks[0].expression = "((`parent` > 0) and (`parent` < 100))".into();
        t.checks[0].extras.insert(crate::dialect::ENFORCED, true);
        t.extras.insert(props::STORAGE_ENGINE, "InnoDB");
        t
    }

    #[test]
    fn round_trip_spellings_compare_equal() {
        let cmp = MySqlComparator { rules: MYSQL };
        let source = ObjectDef::Table(table());
        let back = ObjectDef::Table(fetched());
        assert_eq!(cmp.diff(&source, &back), []);
        assert_eq!(cmp.diff(&back, &source), []);

        // InnoDB's own index for a foreign key without one.
        let mut with_index = fetched();
        with_index.foreign_keys[0].columns = vec!["id".into()];
        let mut source2 = table();
        source2.foreign_keys[0].columns = vec!["id".into()];
        with_index.indexes.push(IndexDef {
            name: "fk_t_parent".into(),
            columns: vec![IndexColumn::column("id")],
            unique: false,
            is_constraint: false,
            predicate: None,
            extras: Extras::new(),
        });
        assert_eq!(
            cmp.diff(&ObjectDef::Table(source2), &ObjectDef::Table(with_index)),
            []
        );

        // A real change is still one.
        let mut changed = fetched();
        changed.checks[0].expression = "((`parent` > 0) or (`parent` < 100))".into();
        changed.columns[1].native_type = "bigint".into();
        let diffs = diff_tables(&normalize_pair(&table(), &changed).0, &changed, &MYSQL);
        let ids: Vec<_> = diffs.iter().filter_map(|d| d.prop.clone()).collect();
        assert!(ids.contains(&props::DATA_TYPE), "{diffs:?}");
        assert!(ids.contains(&props::EXPRESSION), "{diffs:?}");
        let diffs = cmp.diff(&source, &ObjectDef::Table(changed));
        assert_eq!(diffs.len(), 2, "{diffs:?}");

        // Child cells normalize too.
        let idx = |is_constraint: bool| ObjectDef::Index {
            table: ObjectRef::table(Some("db"), None, "t"),
            index: IndexDef {
                is_constraint,
                ..table().indexes[0].clone()
            },
        };
        assert_eq!(cmp.diff(&idx(false), &idx(true)), []);
    }
}
