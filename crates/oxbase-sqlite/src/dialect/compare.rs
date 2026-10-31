//! SQLite [`CompareRules`] (M3): what SQLite spells its own way, for the shared structural
//! comparison of `oxbase_dialect::compare` (registered as `RulesComparator(SqliteRules)` for
//! `Table`, `Column`, `Index`, `ForeignKey` and `Check`).
//!
//! - **Types.** SQLite keeps the declared type text as written. Two declared types are the same
//!   when they are equal ignoring ASCII case and whitespace (`varchar ( 20 )` = `VARCHAR(20)`).
//!   A column built by hand without a native type is compared by its neutral type, as by
//!   default.
//! - **Defaults.** `pragma_table_xinfo` reports the default as written, without the parentheses
//!   of `DEFAULT (expr)`; `DEFAULT NULL` is the same as no default.
//! - **Expressions** (checks, generated columns, index predicates and expression keys): equal
//!   after whitespace normalization and after dropping parentheses that enclose the whole
//!   expression (`(qty >= 0)` = `qty >= 0`).
//! - **Foreign-key targets**: SQLite names are case-insensitive and a foreign key never leaves
//!   its database, so a missing database on one side matches.
//! - **Extras** not compared: [`COLUMN`] (where a check was declared), [`UNNAMED`] and
//!   [`HIDDEN`] (listing facts), [`REBUILD_LOSES`] (a note on the stored text) and the
//!   primary-key mark of an index.

use oxbase_dialect::compare::{CompareRules, normalize_ws};
use oxbase_types::{ColumnDef, ObjectKind, ObjectRef, PropId, props};

use super::{COLUMN, HIDDEN, REBUILD_LOSES, UNNAMED};

/// The SQLite comparison rules. See the module docs.
#[derive(Debug, Clone, Copy, Default)]
pub struct SqliteRules;

/// A declared type in a canonical spelling: upper-case, whitespace collapsed, and no space next
/// to `(`, `)` or `,`.
pub(crate) fn canonical_type(decl: &str) -> String {
    let collapsed = decl.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::with_capacity(collapsed.len());
    let chars: Vec<char> = collapsed.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        if *c == ' ' {
            let prev = i.checked_sub(1).and_then(|p| chars.get(p));
            let next = chars.get(i + 1);
            if [prev, next]
                .into_iter()
                .flatten()
                .any(|n| matches!(*n, '(' | ')' | ','))
            {
                continue;
            }
        }
        out.push(c.to_ascii_uppercase());
    }
    out
}

/// `true` for a default that means "no default": absent, or the literal `NULL`.
fn is_null_default(default: Option<&str>) -> bool {
    default.is_none_or(|d| d.trim().eq_ignore_ascii_case("NULL"))
}

/// `text` without parentheses that enclose all of it (repeatedly), whitespace-normalized.
pub(crate) fn strip_outer_parens(text: &str) -> String {
    let mut current = normalize_ws(text);
    loop {
        let Some(inner) = current
            .strip_prefix('(')
            .and_then(|rest| rest.strip_suffix(')'))
        else {
            return current;
        };
        if !encloses_all(&current) {
            return current;
        }
        current = normalize_ws(inner);
    }
}

/// `true` when the `(` at the start of `text` closes at its very end (quotes respected).
fn encloses_all(text: &str) -> bool {
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let last = text.chars().count().saturating_sub(1);
    for (i, c) in text.chars().enumerate() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '\'' | '"' | '`' => quote = Some(c),
                '[' => quote = Some(']'),
                '(' => depth += 1,
                ')' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 && i != last {
                        return false;
                    }
                }
                _ => {}
            },
        }
    }
    depth == 0
}

impl CompareRules for SqliteRules {
    fn same_type(&self, old: &ColumnDef, new: &ColumnDef) -> bool {
        if old.native_type.trim().is_empty() || new.native_type.trim().is_empty() {
            old.data_type == new.data_type
        } else {
            canonical_type(&old.native_type) == canonical_type(&new.native_type)
        }
    }

    fn same_default(&self, old: &ColumnDef, new: &ColumnDef) -> bool {
        let before = old.default.as_deref();
        let after = new.default.as_deref();
        match (is_null_default(before), is_null_default(after)) {
            (true, true) => true,
            (false, false) => match (before, after) {
                (Some(b), Some(a)) => self.same_expression(b, a),
                _ => false,
            },
            _ => old.auto_increment && new.auto_increment,
        }
    }

    fn same_expression(&self, old: &str, new: &str) -> bool {
        strip_outer_parens(old) == strip_outer_parens(new)
    }

    fn same_table_ref(&self, old: &ObjectRef, new: &ObjectRef) -> bool {
        let same_db = match (old.catalog.as_deref(), new.catalog.as_deref()) {
            (Some(before), Some(after)) => before.eq_ignore_ascii_case(after),
            _ => true,
        };
        same_db && old.name.eq_ignore_ascii_case(&new.name)
    }

    fn compare_extra(&self, kind: ObjectKind, prop: &PropId) -> bool {
        let listing_only =
            *prop == COLUMN || *prop == UNNAMED || *prop == HIDDEN || *prop == REBUILD_LOSES;
        let pk_index = kind == ObjectKind::Index && *prop == props::PRIMARY_KEY;
        !(listing_only || pk_index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_dialect::Comparator;
    use oxbase_dialect::compare::RulesComparator;
    use oxbase_types::{CheckDef, DataType, Extras, ObjectDef, PrimaryKeyDef, TableDef, Value};

    fn col(name: &str, native: &str) -> ColumnDef {
        let mut c = ColumnDef::new(name, super::super::types::data_type(native));
        native.clone_into(&mut c.native_type);
        c
    }

    #[test]
    fn types_compare_by_declared_text() {
        let rules = SqliteRules;
        assert!(rules.same_type(&col("a", "varchar ( 20 )"), &col("a", "VARCHAR(20)")));
        assert!(rules.same_type(&col("a", "decimal(10, 2)"), &col("a", "DECIMAL(10,2)")));
        assert!(rules.same_type(
            &col("a", "unsigned  big int"),
            &col("a", "UNSIGNED BIG INT")
        ));
        assert!(!rules.same_type(&col("a", "INT"), &col("a", "INTEGER")));
        assert!(!rules.same_type(&col("a", "TEXT"), &col("a", "VARCHAR(20)")));
        // Without a native type the neutral types decide.
        let hand = ColumnDef::new("a", DataType::INT64);
        assert!(rules.same_type(&hand, &col("a", "INT")));
        assert!(rules.same_type(&hand, &col("a", "INTEGER")));
        assert!(!rules.same_type(&hand, &col("a", "TEXT")));
        assert_eq!(canonical_type(" numeric ( 5 , 1 ) "), "NUMERIC(5,1)");
    }

    #[test]
    fn defaults_and_expressions() {
        let rules = SqliteRules;
        let mut a = col("a", "TEXT");
        let mut b = col("a", "TEXT");
        a.default = Some("NULL".into());
        assert!(rules.same_default(&a, &b));
        b.default = Some("'x'".into());
        assert!(!rules.same_default(&a, &b));
        a.default = Some("('x')".into());
        assert!(rules.same_default(&a, &b));
        a.default = Some("'x '".into());
        assert!(!rules.same_default(&a, &b));

        assert!(rules.same_expression("(qty >= 0)", "qty  >=  0"));
        assert!(rules.same_expression("((a > 1))", "a > 1"));
        assert!(!rules.same_expression("(a > 1) AND (b > 1)", "a > 1) AND (b > 1"));
        assert_eq!(
            strip_outer_parens("(a > 1) AND (b > 1)"),
            "(a > 1) AND (b > 1)"
        );
        assert_eq!(strip_outer_parens("( ')' )"), "')'");
        assert!(!rules.same_expression("x = 'a  b'", "x = 'a b'"));
    }

    #[test]
    fn table_refs_and_extras() {
        let rules = SqliteRules;
        let a = ObjectRef::table(Some("main"), None, "Parent");
        assert!(rules.same_table_ref(&a, &ObjectRef::table(None, None, "parent")));
        assert!(rules.same_table_ref(&a, &ObjectRef::table(Some("MAIN"), None, "PARENT")));
        assert!(!rules.same_table_ref(&a, &ObjectRef::table(Some("aux"), None, "parent")));
        assert!(!rules.compare_extra(ObjectKind::Check, &COLUMN));
        assert!(!rules.compare_extra(ObjectKind::Index, &props::PRIMARY_KEY));
        assert!(rules.compare_extra(ObjectKind::Table, &props::WITHOUT_ROWID));
    }

    #[test]
    fn a_fetched_table_equals_its_spec() {
        let mut fetched = TableDef::new(ObjectRef::table(Some("main"), None, "t"));
        let mut id = col("id", "INTEGER").not_null();
        id.auto_increment = true;
        fetched.columns.push(id);
        let mut qty = col("qty", "integer");
        qty.default = Some("1".into());
        fetched.columns.push(qty);
        fetched.primary_key = Some(PrimaryKeyDef {
            name: None,
            columns: vec!["id".into()],
        });
        fetched.checks.push(CheckDef {
            name: Some("t_qty_chk".into()),
            expression: "qty >= 0".into(),
            extras: Extras::new().with(COLUMN, "qty"),
        });
        let mut spec = fetched.clone();
        spec.columns[1].native_type = "INTEGER".into();
        spec.columns[1].default = Some("(1)".into());
        spec.checks[0].expression = "(qty >= 0)".into();
        spec.checks[0].extras = Extras::new();
        let cmp = RulesComparator(SqliteRules);
        assert_eq!(
            cmp.diff(
                &ObjectDef::Table(spec.clone()),
                &ObjectDef::Table(fetched.clone())
            ),
            []
        );
        spec.columns[1].default = Some("2".into());
        let diffs = cmp.diff(&ObjectDef::Table(spec), &ObjectDef::Table(fetched));
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].prop, Some(props::DEFAULT));
        assert_eq!(diffs[0].new, Some(Value::Text("1".into())));
    }
}
