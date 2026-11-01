//! Comparator rules for SQL Server (M4, **ddl** stream): the shared structural walk of
//! `oxbase_dialect::compare` with [`MsSqlRules`], after SQL Server's implicit facts are filled in
//! on both sides ([`MsSqlComparator`]). Contract: `docs/m4-contracts.md` §4.3.
//!
//! What the rules fold so that a fetched definition compares equal to the one it was generated
//! from (the round-trip invariant):
//!
//! - type aliases and spellings ([`canonical_type`]): `int` / `INT` / `integer`,
//!   `nvarchar(max)` / `nvarchar(-1)`, `float` / `float(53)`, `real` / `float(24)`,
//!   `decimal(18, 0)` / `decimal`, `rowversion` / `timestamp`, `sysname` / `nvarchar(128)`,
//!   bracketed names; a column without a native type is compared by the spelling the generator
//!   writes for its neutral type;
//! - default, check, computed-column and filter text as the server stores it
//!   ([`canonical_expression`]): redundant parentheses (`((0))` = `0`, `('x')` = `'x'`,
//!   `([qty]>=(0))` = `qty >= 0`), bracketed identifiers, whitespace, case of keywords, names
//!   and built-in functions (`getdate()` / `GETDATE()`), and the `N` prefix of string literals;
//! - extras compared only when both sides have them ([`crate::extras::DEFAULT_CONSTRAINT`]
//!   names the server generated), except the facts [`MsSqlComparator`] fills in first: the
//!   primary key's `clustered` (absent = clustered unless another index is), an index's
//!   `clustered` / `index_kind` (absent = nonclustered) and `include_columns` (absent = none);
//!   identity columns are always `always = true` and auto-increment, and computed columns are
//!   compared without their type and nullability (the server derives both from the
//!   expression).

use std::sync::Arc;

use oxbase_dialect::Comparator;
use oxbase_dialect::compare::{CompareRules, diff_objects, normalize_ws};
use oxbase_types::{
    ColumnDef, DataType, IdentityDef, IndexDef, ObjectDef, PropertyDiff, ServerVersion, TableDef,
    Value, props,
};

use crate::extras;

use super::ddl::neutral_type;

/// SQL Server's comparison rules.
#[derive(Debug, Clone, Copy, Default)]
pub struct MsSqlRules;

impl CompareRules for MsSqlRules {
    fn same_type(&self, old: &ColumnDef, new: &ColumnDef) -> bool {
        match (spelled_type(old), spelled_type(new)) {
            (Some(a), Some(b)) => {
                a == b
                    || (json_neutral(old) && b == "nvarchar(max)")
                    || (json_neutral(new) && a == "nvarchar(max)")
            }
            _ => old.data_type == new.data_type,
        }
    }

    fn same_expression(&self, old: &str, new: &str) -> bool {
        match (canonical_expression(old), canonical_expression(new)) {
            (Some(a), Some(b)) => a == b,
            _ => normalize_ws(old) == normalize_ws(new),
        }
    }
}

/// A column without a native type whose neutral type is JSON: before SQL Server 2025 the
/// generator writes it as `nvarchar(max)`.
fn json_neutral(c: &ColumnDef) -> bool {
    c.native_type.trim().is_empty() && matches!(c.data_type, DataType::Json { .. })
}

/// The canonical spelling of a column's type: its native type, else the generator's spelling
/// of its neutral type (on a current server). `None` when neither can be written.
fn spelled_type(c: &ColumnDef) -> Option<String> {
    if c.native_type.trim().is_empty() {
        neutral_type(&c.data_type, &ServerVersion::new(0, 0, 0))
            .ok()
            .map(|t| canonical_type(&t))
    } else {
        Some(canonical_type(&c.native_type))
    }
}

/// The comparator registered for every designer kind: [`MsSqlRules`] after
/// [`normalize_table`] / the child normalizations on both sides.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct MsSqlComparator;

impl Comparator for MsSqlComparator {
    fn diff(&self, a: &ObjectDef, b: &ObjectDef) -> Vec<PropertyDiff> {
        let (mut a, mut b) = (a.clone(), b.clone());
        normalize_def(&mut a);
        normalize_def(&mut b);
        diff_objects(&a, &b, &MsSqlRules)
    }
}

/// The comparator registered for every designer kind.
pub(crate) fn comparator() -> Arc<dyn Comparator> {
    Arc::new(MsSqlComparator)
}

fn normalize_def(d: &mut ObjectDef) {
    match d {
        ObjectDef::Table(t) => normalize_table(t),
        ObjectDef::Column { column, .. } => normalize_column(column),
        ObjectDef::Index { index, .. } => normalize_index(index),
        _ => {}
    }
}

/// Fills in what SQL Server implies (see the module docs).
pub(crate) fn normalize_table(t: &mut TableDef) {
    for c in &mut t.columns {
        normalize_column(c);
    }
    for idx in &mut t.indexes {
        normalize_index(idx);
    }
    if t.primary_key.is_some() {
        let clustered = pk_clustered(t);
        t.extras.insert(props::CLUSTERED, clustered);
    } else {
        t.extras.remove(&props::CLUSTERED);
    }
}

fn normalize_column(c: &mut ColumnDef) {
    if let Some(identity) = &mut c.identity {
        identity.always = true;
        c.auto_increment = true;
    } else if c.auto_increment && c.generated.is_none() {
        c.identity = Some(IdentityDef {
            always: true,
            start: None,
            increment: None,
        });
    }
    if c.generated.is_some() {
        // The server derives a computed column's type and nullability from its expression.
        c.nullable = true;
        c.native_type = String::new();
        c.data_type = DataType::Other {
            name: "computed".to_owned(),
        };
    }
}

pub(crate) fn normalize_index(idx: &mut IndexDef) {
    let clustered = index_clustered(idx);
    idx.extras.insert(props::CLUSTERED, clustered);
    if !idx.extras.contains(&extras::INDEX_KIND) {
        idx.extras.insert(
            extras::INDEX_KIND,
            if clustered {
                "clustered"
            } else {
                "nonclustered"
            },
        );
    }
    if matches!(
        idx.extras.get(&props::INCLUDE_COLUMNS),
        None | Some(Value::Null)
    ) {
        idx.extras
            .insert(props::INCLUDE_COLUMNS, Value::Json(serde_json::json!([])));
    }
}

/// Whether an index is clustered: its `clustered` extra, else its `index_kind`, else no.
pub(crate) fn index_clustered(idx: &IndexDef) -> bool {
    if let Some(flag) = idx.extras.get(&props::CLUSTERED).and_then(Value::as_bool) {
        return flag;
    }
    idx.extras
        .get(&extras::INDEX_KIND)
        .and_then(Value::as_str)
        .is_some_and(|kind| kind.eq_ignore_ascii_case("clustered"))
}

/// Whether the table's primary key is clustered: the table's `clustered` extra, else `true`
/// unless another index of the table is clustered (SQL Server's default).
pub(crate) fn pk_clustered(t: &TableDef) -> bool {
    match t.extras.get(&props::CLUSTERED).and_then(Value::as_bool) {
        Some(flag) => flag,
        None => !t.indexes.iter().any(index_clustered),
    }
}

// --- types ------------------------------------------------------------------------------------

/// The base name and arguments of a canonical type (`nvarchar`, `["40"]`).
pub(crate) fn split_type(canonical: &str) -> (&str, Vec<&str>) {
    match canonical.split_once('(') {
        Some((base, rest)) => (
            base,
            rest.trim_end_matches(')')
                .split(',')
                .filter(|a| !a.is_empty())
                .collect(),
        ),
        None => (canonical, Vec::new()),
    }
}

/// One spelling per SQL Server type: lowercase, no brackets or blanks, aliases resolved and
/// default arguments written out (`decimal` → `decimal(18,0)`, `nvarchar` → `nvarchar(1)`,
/// `nvarchar(-1)` → `nvarchar(max)`, `float` → `float(53)`, `float(10)` → `real`,
/// `timestamp` → `rowversion`, `sysname` → `nvarchar(128)`, `datetime2` → `datetime2(7)`).
pub(crate) fn canonical_type(native: &str) -> String {
    // Lowercase, without brackets or quotes; blanks kept only between two words.
    let mut text = String::with_capacity(native.len());
    let mut blank = false;
    for c in native.trim().chars() {
        if matches!(c, '[' | ']' | '"') {
            continue;
        }
        if c.is_whitespace() {
            blank = true;
            continue;
        }
        let word = c.is_alphanumeric() || c == '_';
        if blank && word && text.ends_with(|p: char| p.is_alphanumeric() || p == '_') {
            text.push(' ');
        }
        blank = false;
        text.extend(c.to_lowercase());
    }
    let (base, args) = match text.split_once('(') {
        Some((base, rest)) => (
            base.trim().to_owned(),
            rest.trim_end_matches(')')
                .split(',')
                .map(|a| {
                    let a = a.trim();
                    if a == "-1" {
                        "max".to_owned()
                    } else {
                        a.to_owned()
                    }
                })
                .filter(|a| !a.is_empty())
                .collect::<Vec<_>>(),
        ),
        None => (text.trim().to_owned(), Vec::new()),
    };
    let base = match base.as_str() {
        "integer" => "int",
        "dec" => "decimal",
        "double precision" => return "float(53)".to_owned(),
        "character" => "char",
        "character varying" | "char varying" => "varchar",
        "national character" | "national char" => "nchar",
        "national character varying" | "national char varying" | "nchar varying" => "nvarchar",
        "binary varying" => "varbinary",
        "national text" => "ntext",
        "timestamp" | "rowversion" => return "rowversion".to_owned(),
        "sysname" => return "nvarchar(128)".to_owned(),
        other => other,
    }
    .to_owned();
    let join = |base: &str, args: &[String]| {
        if args.is_empty() {
            base.to_owned()
        } else {
            format!("{base}({})", args.join(","))
        }
    };
    match base.as_str() {
        "float" => match args.first().and_then(|a| a.parse::<u32>().ok()) {
            Some(n) if n <= 24 => "real".to_owned(),
            _ => "float(53)".to_owned(),
        },
        "decimal" | "numeric" => match args.as_slice() {
            [] => format!("{base}(18,0)"),
            [p] => format!("{base}({p},0)"),
            _ => join(&base, &args),
        },
        "char" | "varchar" | "nchar" | "nvarchar" | "binary" | "varbinary" if args.is_empty() => {
            format!("{base}(1)")
        }
        "datetime2" | "time" | "datetimeoffset" if args.is_empty() => format!("{base}(7)"),
        _ => join(&base, &args),
    }
}

// --- expressions ------------------------------------------------------------------------------

/// One token of an expression.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    /// A name or keyword (bracketed and quoted identifiers unwrapped), lowercase.
    Name(String),
    /// A number as written (lowercase, so `0X1F` = `0x1f`).
    Number(String),
    /// A string literal's text (doubled quotes kept), without an `N` prefix.
    Str(String),
    /// Any other character.
    Punct(char),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Node {
    Tok(Tok),
    Group(Vec<Node>),
}

/// Keywords after which `(` opens a plain group, not a function call's arguments.
const OPERATOR_WORDS: [&str; 15] = [
    "and", "or", "not", "is", "like", "between", "in", "when", "then", "else", "case", "exists",
    "as", "end", "null",
];

fn tokenize(text: &str) -> Option<Vec<Tok>> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    let quoted = |start: usize, close: char| -> Option<(String, usize)> {
        let mut j = start + 1;
        let mut body = String::new();
        while j < chars.len() {
            if chars[j] == close {
                if chars.get(j + 1) == Some(&close) {
                    body.push(close);
                    body.push(close);
                    j += 2;
                    continue;
                }
                return Some((body, j + 1));
            }
            body.push(chars[j]);
            j += 1;
        }
        None
    };
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '\'' {
            let (body, end) = quoted(i, '\'')?;
            out.push(Tok::Str(body));
            i = end;
        } else if (c == 'n' || c == 'N') && chars.get(i + 1) == Some(&'\'') {
            let (body, end) = quoted(i + 1, '\'')?;
            out.push(Tok::Str(body));
            i = end;
        } else if c == '[' {
            let (body, end) = quoted(i, ']')?;
            out.push(Tok::Name(body.replace("]]", "]").to_lowercase()));
            i = end;
        } else if c == '"' {
            let (body, end) = quoted(i, '"')?;
            out.push(Tok::Name(body.replace("\"\"", "\"").to_lowercase()));
            i = end;
        } else if c.is_ascii_digit()
            || (c == '.' && chars.get(i + 1).is_some_and(char::is_ascii_digit))
        {
            let start = i;
            while i < chars.len() {
                let d = chars[i];
                let exponent_sign = matches!(d, '+' | '-')
                    && i > start
                    && matches!(chars[i - 1], 'e' | 'E')
                    && !chars[start..i].iter().any(|x| matches!(x, 'x' | 'X'));
                if d.is_ascii_alphanumeric() || d == '.' || exponent_sign {
                    i += 1;
                } else {
                    break;
                }
            }
            let number: String = chars[start..i].iter().collect();
            out.push(Tok::Number(number.to_lowercase()));
        } else if c.is_alphanumeric() || matches!(c, '_' | '@' | '#' | '$') {
            let start = i;
            while i < chars.len()
                && (chars[i].is_alphanumeric() || matches!(chars[i], '_' | '@' | '#' | '$'))
            {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            out.push(Tok::Name(word.to_lowercase()));
        } else {
            out.push(Tok::Punct(c));
            i += 1;
        }
    }
    Some(out)
}

/// Builds the tree of parenthesized groups; `None` when parentheses do not balance.
fn tree(tokens: Vec<Tok>) -> Option<Vec<Node>> {
    let mut stack: Vec<Vec<Node>> = vec![Vec::new()];
    for t in tokens {
        match t {
            Tok::Punct('(') => stack.push(Vec::new()),
            Tok::Punct(')') => {
                let group = stack.pop()?;
                stack.last_mut()?.push(Node::Group(group));
            }
            other => stack.last_mut()?.push(Node::Tok(other)),
        }
    }
    if stack.len() == 1 { stack.pop() } else { None }
}

/// `true` when the node right before a group makes it a call's argument list.
fn is_call(prev: Option<&Node>) -> bool {
    match prev {
        Some(Node::Tok(Tok::Name(name))) => !OPERATOR_WORDS.contains(&name.as_str()),
        _ => false,
    }
}

/// A group that can lose its parentheses: one token or a signed number.
fn unwrappable(items: &[Node]) -> bool {
    matches!(
        items,
        [Node::Tok(_) | Node::Group(_)]
            | [Node::Tok(Tok::Punct('-' | '+')), Node::Tok(Tok::Number(_))]
    )
}

/// Drops redundant parentheses: around one token or a signed number (unless they are a call's
/// argument list) and around the whole expression.
fn simplify(nodes: Vec<Node>) -> Vec<Node> {
    let mut out: Vec<Node> = Vec::with_capacity(nodes.len());
    for node in nodes {
        match node {
            Node::Group(inner) => {
                // Already without parentheses around the whole group content.
                let inner = simplify(inner);
                if !is_call(out.last()) && unwrappable(&inner) {
                    out.extend(inner);
                } else {
                    out.push(Node::Group(inner));
                }
            }
            tok @ Node::Tok(_) => out.push(tok),
        }
    }
    while let [Node::Group(_)] = out.as_slice() {
        let Some(Node::Group(inner)) = out.pop() else {
            break;
        };
        out = inner;
    }
    out
}

fn render(nodes: &[Node], out: &mut String) {
    for node in nodes {
        if !out.is_empty() && !out.ends_with('(') {
            out.push(' ');
        }
        match node {
            Node::Tok(Tok::Name(n) | Tok::Number(n)) => out.push_str(n),
            Node::Tok(Tok::Str(s)) => {
                out.push('\'');
                out.push_str(s);
                out.push('\'');
            }
            Node::Tok(Tok::Punct(c)) => out.push(*c),
            Node::Group(inner) => {
                out.push('(');
                render(inner, out);
                out.push(')');
            }
        }
    }
}

/// The canonical form of a T-SQL expression (see the module docs), or `None` when it does not
/// lex (an unterminated string or unbalanced parentheses).
pub(crate) fn canonical_expression(text: &str) -> Option<String> {
    let nodes = simplify(tree(tokenize(text)?)?);
    let mut out = String::new();
    render(&nodes, &mut out);
    Some(out)
}

/// `true` when expression `text` names column `column` (bracketed, quoted or bare; case is
/// ignored). Unlexable text counts as naming it.
pub(crate) fn mentions_column(text: &str, column: &str) -> bool {
    let wanted = column.to_lowercase();
    tokenize(text).is_none_or(|tokens| {
        tokens
            .iter()
            .any(|t| matches!(t, Tok::Name(n) if *n == wanted))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_types::{CheckDef, Extras, GeneratedColumn, IndexColumn, ObjectRef, PrimaryKeyDef};
    use pretty_assertions::assert_eq;

    fn same(a: &str, b: &str) -> bool {
        MsSqlRules.same_expression(a, b)
    }

    #[test]
    fn expressions_fold_server_parentheses_brackets_and_case() {
        assert!(same("((0))", "0"));
        assert!(same("('x')", "'x'"));
        assert!(same("(N'it''s ü')", "N'it''s ü'"));
        assert!(same("(N'new')", "'new'"));
        assert!(same("([qty]>=(0))", "qty >= 0"));
        assert!(same("(getdate())", "GETDATE()"));
        assert!(same("((-1))", "-1"));
        assert!(same("([code] IS NOT NULL)", "code IS NOT NULL"));
        assert!(same("(len([code])>(0))", "len(code) > 0"));
        assert!(same("([qty]*[price])", "qty * price"));
        assert!(same("(upper([code]))", "UPPER(code)"));
        assert!(same("([a]>(0) AND [b]<(5))", "a > 0 and b < 5"));
        assert!(same("(NEXT VALUE FOR [dbo].[s])", "NEXT VALUE FOR dbo.s"));
        assert!(same("(CONVERT([bit],(0)))", "CONVERT(bit, 0)"));
        assert!(same("([a] IN ((1), (2)))", "a IN (1, 2)"));
        // Real differences stay visible.
        assert!(!same("([qty]>=(0))", "qty >= 1"));
        assert!(!same("((0))", "1"));
        assert!(!same("('x')", "'X'"));
        assert!(!same("(round([a],(2)))", "round(a, 3)"));
        assert!(!same("([a]-[b])", "b - a"));
        // Strings keep their content, case and blanks.
        assert!(!same("('a  b')", "'a b'"));
        assert!(same("('a ( b')", "'a ( b'"));
        // Unlexable text falls back to whitespace folding.
        assert!(same("'open", "'open"));
        assert!(!same("'open", "'other"));
        assert_eq!(
            canonical_expression("([qty]>=(0))").as_deref(),
            Some("qty > = 0")
        );
        assert_eq!(canonical_expression("(a").as_deref(), None);
    }

    #[test]
    fn columns_are_found_in_expressions() {
        assert!(mentions_column("([qty]>=(0))", "qty"));
        assert!(mentions_column("QTY > 0", "qty"));
        assert!(mentions_column("[a]]b] > 0", "a]b"));
        assert!(!mentions_column("([qty2]>=(0))", "qty"));
        assert!(!mentions_column("'qty' = [x]", "qty"));
        assert!(mentions_column("'open", "qty"));
    }

    #[test]
    fn type_spellings_fold() {
        let pairs = [
            ("INT", "int"),
            ("integer", "int"),
            ("[nvarchar](max)", "nvarchar(max)"),
            ("nvarchar(-1)", "nvarchar(max)"),
            ("float", "float(53)"),
            ("float(24)", "real"),
            ("double precision", "float(53)"),
            ("decimal", "decimal(18,0)"),
            ("decimal(12, 2)", "decimal(12,2)"),
            ("numeric(10)", "numeric(10,0)"),
            ("timestamp", "rowversion"),
            ("sysname", "nvarchar(128)"),
            ("varchar", "varchar(1)"),
            ("datetime2", "datetime2(7)"),
            ("character varying(10)", "varchar(10)"),
            ("[dbo].[phone]", "dbo.phone"),
        ];
        for (a, b) in pairs {
            assert_eq!(canonical_type(a), canonical_type(b), "{a} vs {b}");
        }
        assert_ne!(
            canonical_type("nvarchar(40)"),
            canonical_type("nvarchar(41)")
        );
        assert_ne!(
            canonical_type("varchar(10)"),
            canonical_type("nvarchar(10)")
        );
        assert_ne!(
            canonical_type("decimal(12,2)"),
            canonical_type("numeric(12,2)")
        );
        assert_eq!(split_type("decimal(12,2)"), ("decimal", vec!["12", "2"]));
        assert_eq!(split_type("int"), ("int", vec![]));
    }

    fn col(name: &str, data_type: DataType, native: &str) -> ColumnDef {
        let mut c = ColumnDef::new(name, data_type);
        c.native_type = native.to_owned();
        c
    }

    #[test]
    fn neutral_types_compare_by_their_spelling() {
        let r = MsSqlRules;
        let fetched = col("c", DataType::VarChar { length: Some(40) }, "nvarchar(40)");
        let neutral = col("c", DataType::VarChar { length: Some(40) }, "");
        assert!(r.same_type(&fetched, &neutral));
        let text = col("c", DataType::Text, "nvarchar(max)");
        assert!(r.same_type(&text, &col("c", DataType::VarChar { length: None }, "")));
        assert!(r.same_type(&text, &col("c", DataType::Text, "")));
        let wider = col("c", DataType::VarChar { length: Some(41) }, "nvarchar(41)");
        assert!(!r.same_type(&fetched, &wider));
        let ts = col(
            "c",
            DataType::Timestamp { precision: Some(7) },
            "datetime2(7)",
        );
        assert!(r.same_type(&ts, &col("c", DataType::Timestamp { precision: None }, "")));
        let json = col("c", DataType::Json { binary: true }, "json");
        assert!(r.same_type(&json, &col("c", DataType::Json { binary: false }, "")));
        let old_json = col("c", DataType::Text, "nvarchar(max)");
        assert!(r.same_type(&old_json, &col("c", DataType::Json { binary: false }, "")));
        assert!(r.same_type(
            &col("c", DataType::INT64, "bigint"),
            &col("c", DataType::INT64, "")
        ));
        assert!(!r.same_type(
            &col("c", DataType::INT32, "int"),
            &col("c", DataType::INT64, "")
        ));
    }

    fn table() -> TableDef {
        let mut t = TableDef::new(ObjectRef::table(Some("db"), Some("m3"), "t"));
        let mut id = col("id", DataType::INT32, "int").not_null();
        id.identity = Some(IdentityDef {
            always: false,
            start: Some(1),
            increment: Some(1),
        });
        let mut qty = col("qty", DataType::INT32, "int");
        qty.default = Some("0".into());
        let mut total = col("total", DataType::INT32, "");
        total.generated = Some(GeneratedColumn {
            expression: "qty * 2".into(),
            stored: true,
        });
        t.columns = vec![id, qty, total];
        t.primary_key = Some(PrimaryKeyDef {
            name: None,
            columns: vec!["id".into()],
        });
        t.indexes.push(IndexDef {
            name: "ix".into(),
            columns: vec![IndexColumn::column("qty")],
            unique: false,
            is_constraint: false,
            predicate: Some("qty > 0".into()),
            extras: Extras::new(),
        });
        t.checks.push(CheckDef {
            name: Some("ck".into()),
            expression: "qty >= 0".into(),
            extras: Extras::new(),
        });
        t
    }

    /// The table as the fetcher reports it after the generator created it.
    fn fetched() -> TableDef {
        let mut t = table();
        t.extras.insert(props::CLUSTERED, true);
        t.primary_key.as_mut().unwrap().name = Some("PK_t".into());
        let id = &mut t.columns[0];
        id.auto_increment = true;
        id.identity.as_mut().unwrap().always = true;
        let qty = &mut t.columns[1];
        qty.default = Some("((0))".into());
        qty.extras.insert(extras::DEFAULT_CONSTRAINT, "DF_t_qty");
        let total = &mut t.columns[2];
        total.native_type = "int".into();
        total.nullable = false;
        total.generated.as_mut().unwrap().expression = "([qty]*(2))".into();
        let ix = &mut t.indexes[0];
        ix.predicate = Some("([qty]>(0))".into());
        ix.extras.insert(props::CLUSTERED, false);
        ix.extras.insert(extras::INDEX_KIND, "nonclustered");
        ix.extras.insert(extras::DISABLED, false);
        t.checks[0].expression = "([qty]>=(0))".into();
        t.checks[0].extras.insert(extras::NOT_TRUSTED, false);
        t
    }

    #[test]
    fn a_fetched_table_equals_its_design() {
        let cmp = MsSqlComparator;
        let design = ObjectDef::Table(table());
        let server = ObjectDef::Table(fetched());
        assert_eq!(cmp.diff(&design, &server), []);
        assert_eq!(cmp.diff(&server, &design), []);
    }

    #[test]
    fn clustering_and_include_columns_are_compared() {
        let cmp = MsSqlComparator;
        let server = fetched();
        // A nonclustered primary key differs from the default.
        let mut design = table();
        design.extras.insert(props::CLUSTERED, false);
        let diffs = cmp.diff(&ObjectDef::Table(design), &ObjectDef::Table(server.clone()));
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].prop, Some(props::CLUSTERED));
        // A clustered index, and included columns the server does not have.
        let mut design = table();
        design.indexes[0].extras.insert(props::CLUSTERED, true);
        design.indexes[0].extras.insert(
            props::INCLUDE_COLUMNS,
            Value::Json(serde_json::json!(["id"])),
        );
        let props_seen: Vec<String> = cmp
            .diff(&ObjectDef::Table(server), &ObjectDef::Table(design))
            .iter()
            .filter_map(|d| d.prop.as_ref().map(|p| p.as_str().to_owned()))
            .collect();
        assert_eq!(
            props_seen,
            ["clustered", "clustered", "include_columns", "index_kind"]
        );
    }

    #[test]
    fn identity_and_expression_changes_are_seen() {
        let cmp = MsSqlComparator;
        let server = fetched();
        let mut design = table();
        design.columns[0].identity.as_mut().unwrap().increment = Some(5);
        design.columns[1].default = Some("1".into());
        design.columns[2].generated.as_mut().unwrap().expression = "qty * 3".into();
        design.checks[0].expression = "qty >= 1".into();
        let props_seen: Vec<String> = cmp
            .diff(&ObjectDef::Table(server), &ObjectDef::Table(design))
            .iter()
            .filter_map(|d| d.prop.as_ref().map(|p| p.as_str().to_owned()))
            .collect();
        assert_eq!(
            props_seen,
            ["identity", "default", "generated", "expression"]
        );
        // Children compare the same way on their own.
        let c = |column: ColumnDef| ObjectDef::Column {
            table: ObjectRef::table(None, Some("m3"), "t"),
            column,
        };
        let f = fetched();
        assert_eq!(
            cmp.diff(&c(table().columns[1].clone()), &c(f.columns[1].clone())),
            []
        );
        let i = |index: IndexDef| ObjectDef::Index {
            table: ObjectRef::table(None, Some("m3"), "t"),
            index,
        };
        assert_eq!(
            cmp.diff(&i(table().indexes[0].clone()), &i(f.indexes[0].clone())),
            []
        );
    }
}
