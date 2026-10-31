//! Building SQL with bound parameters (M3): [`SqlWriter`], the expression safety check
//! [`check_expression`], `LIKE` escaping, and the grid's table-data query
//! ([`select_table_data`]: `SELECT * FROM t [WHERE filter] [ORDER BY sort] [LIMIT n]`).
//!
//! Everything here is generic over the engine's [`Quoting`]: identifiers are quoted with
//! [`Quoting::quote_ident`] / [`Quoting::qualify`], parameters use [`Quoting::placeholder`], and
//! values only ever become bound parameters ([`SqlWriter::param`]), never SQL text. No engine
//! special cases live here; engines differ only through their `Quoting` hooks.

use oxbase_types::{
    BoundStatement, ColumnDef, DataType, Error, Filter, FilterOp, NullsOrder, ObjectKind,
    ObjectRef, Result, SortKey, TableDataQuery, TableDef, Value,
};

use crate::Quoting;

/// The escape character of literal `LIKE` matches (`... LIKE ? ESCAPE '!'`). Chosen because it
/// needs no escaping in any engine's string literals (unlike `\` on MySQL).
pub const LIKE_ESCAPE: char = '!';

/// Builds one statement: fixed SQL text, quoted identifiers and bound parameters in the engine's
/// placeholder style.
pub struct SqlWriter<'q> {
    q: &'q dyn Quoting,
    sql: String,
    params: Vec<Value>,
}

impl<'q> SqlWriter<'q> {
    pub fn new(q: &'q dyn Quoting) -> Self {
        Self {
            q,
            sql: String::new(),
            params: Vec::new(),
        }
    }

    /// The engine's quoting.
    pub fn quoting(&self) -> &'q dyn Quoting {
        self.q
    }

    /// Appends fixed SQL text. Never pass data or names here.
    pub fn push(&mut self, sql: &str) -> &mut Self {
        self.sql.push_str(sql);
        self
    }

    /// Appends a quoted identifier.
    pub fn ident(&mut self, name: &str) -> &mut Self {
        let quoted = self.q.quote_ident(name);
        self.sql.push_str(&quoted);
        self
    }

    /// Appends the qualified, quoted name of `obj` ([`Quoting::qualify`]).
    pub fn qualified(&mut self, obj: &ObjectRef) -> &mut Self {
        let quoted = self.q.qualify(obj);
        self.sql.push_str(&quoted);
        self
    }

    /// Appends a placeholder and binds `value` to it.
    pub fn param(&mut self, value: Value) -> &mut Self {
        self.params.push(value);
        let placeholder = self.q.placeholder(self.params.len());
        self.sql.push_str(&placeholder);
        self
    }

    /// Number of parameters bound so far.
    pub fn param_count(&self) -> usize {
        self.params.len()
    }

    /// The SQL text so far.
    pub fn sql(&self) -> &str {
        &self.sql
    }

    pub fn finish(self) -> BoundStatement {
        BoundStatement::new(self.sql, self.params)
    }
}

/// Escapes `text` for a literal `LIKE` match with [`LIKE_ESCAPE`]: the escape character itself
/// and every character of [`Quoting::like_specials`] get the escape character in front.
pub fn escape_like(q: &dyn Quoting, text: &str) -> String {
    let specials = q.like_specials();
    let mut out = String::with_capacity(text.len() + 2);
    for c in text.chars() {
        if c == LIKE_ESCAPE || specials.contains(c) {
            out.push(LIKE_ESCAPE);
        }
        out.push(c);
    }
    out
}

/// Checks that user-typed SQL is a single expression that cannot reach outside the place it is
/// inserted (a raw filter, a column default, a check, a generated column, an index predicate):
///
/// - not blank;
/// - no `;` outside strings and quoted identifiers;
/// - no comment start (`--`, `/*`, and `#` where [`crate::LexRules::hash_comments`]);
/// - every string and quoted identifier is terminated (`'...'`, `"..."`, `` `...` ``, with
///   doubling, backslash escapes where the engine has them, PostgreSQL `E'...'` and `$tag$`
///   strings);
/// - parentheses are balanced and never close more than they opened.
///
/// It is a lexical check, not a parser: it refuses some valid SQL (a `;` inside an unknown kind
/// of quoting) but never accepts text that ends the statement or comments out what follows.
pub fn check_expression(q: &dyn Quoting, text: &str) -> Result<()> {
    if text.trim().is_empty() {
        return Err(Error::invalid_input("the SQL expression is empty"));
    }
    let rules = q.lex_rules();
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut depth: usize = 0;
    let mut i = 0;
    while i < len {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        match c {
            '\'' => {
                let e_string = rules.e_strings
                    && i > 0
                    && matches!(chars[i - 1], 'e' | 'E')
                    && (i < 2 || !is_ident_char(chars[i - 2]));
                i = skip_quoted(&chars, i, rules.backslash_escapes || e_string)?;
            }
            '"' => i = skip_quoted(&chars, i, rules.backslash_escapes)?,
            '`' => i = skip_quoted(&chars, i, false)?,
            '$' if rules.dollar_quotes && (i == 0 || !is_ident_char(chars[i - 1])) => {
                i = match dollar_tag_end(&chars, i) {
                    Some(tag_end) => skip_dollar_string(&chars, i, tag_end)?,
                    None => i + 1,
                };
            }
            ';' => {
                return Err(Error::invalid_input(
                    "the SQL expression contains ';' (only one expression is allowed)",
                ));
            }
            '-' if next == Some('-') => return Err(comment_error()),
            '/' if next == Some('*') => return Err(comment_error()),
            '#' if rules.hash_comments => return Err(comment_error()),
            '(' => {
                depth += 1;
                i += 1;
            }
            ')' => {
                depth = depth.checked_sub(1).ok_or_else(|| {
                    Error::invalid_input("the SQL expression closes a parenthesis it did not open")
                })?;
                i += 1;
            }
            _ => i += 1,
        }
    }
    if depth != 0 {
        return Err(Error::invalid_input(
            "the SQL expression has an unclosed parenthesis",
        ));
    }
    Ok(())
}

fn comment_error() -> Error {
    Error::invalid_input("the SQL expression contains a comment")
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// Index just past the quoted run that starts at `start` (its quote character), honouring
/// doubled quotes and, if `backslash`, backslash escapes.
fn skip_quoted(chars: &[char], start: usize, backslash: bool) -> Result<usize> {
    let quote = chars[start];
    let mut j = start + 1;
    while j < chars.len() {
        let c = chars[j];
        if backslash && c == '\\' {
            j += 2;
            continue;
        }
        if c == quote {
            if chars.get(j + 1) == Some(&quote) {
                j += 2;
                continue;
            }
            return Ok(j + 1);
        }
        j += 1;
    }
    Err(Error::invalid_input(format!(
        "the SQL expression has an unterminated {quote}...{quote}"
    )))
}

/// If a dollar-quote tag (`$$` or `$tag$`) starts at `start`, the index just past it.
fn dollar_tag_end(chars: &[char], start: usize) -> Option<usize> {
    let mut j = start + 1;
    while let Some(&c) = chars.get(j) {
        if c == '$' {
            return Some(j + 1);
        }
        let ok = if j == start + 1 {
            c.is_alphabetic() || c == '_'
        } else {
            c.is_alphanumeric() || c == '_'
        };
        if !ok {
            return None;
        }
        j += 1;
    }
    None
}

/// Index just past the `$tag$ ... $tag$` string whose opening tag spans `start..tag_end`.
fn skip_dollar_string(chars: &[char], start: usize, tag_end: usize) -> Result<usize> {
    let tag = &chars[start..tag_end];
    let mut j = tag_end;
    while j + tag.len() <= chars.len() {
        if &chars[j..j + tag.len()] == tag {
            return Ok(j + tag.len());
        }
        j += 1;
    }
    Err(Error::invalid_input(
        "the SQL expression has an unterminated dollar-quoted string",
    ))
}

/// The column `name` of `table`, or `InvalidInput` naming it. `None` when no table is given.
fn column<'t>(table: Option<&'t TableDef>, name: &str) -> Result<Option<&'t ColumnDef>> {
    match table {
        None => Ok(None),
        Some(t) => t.column(name).map(Some).ok_or_else(|| {
            Error::invalid_input(format!("{} has no column '{name}'", t.obj.display_name()))
        }),
    }
}

/// `true` when a pattern operator must cast the column to text: its type is known and is not
/// textual (unknown engine types such as PostgreSQL `citext` are left alone).
fn needs_text_cast(column: Option<&ColumnDef>) -> bool {
    column.is_some_and(|c| {
        !c.data_type.is_textual() && !matches!(c.data_type, DataType::Other { .. })
    })
}

/// Appends the condition of `filter` (without `WHERE`). Returns `false`, writing nothing, when
/// the filter adds no condition ([`Filter::is_empty`]). With `table`, unknown columns are
/// `InvalidInput` and pattern operators on non-text columns compare [`Quoting::text_cast`] of the
/// column. Groups are parenthesized; raw SQL must pass [`check_expression`].
///
/// Call [`Filter::validate`] first (arity, NULLs, text patterns); this function assumes a valid
/// tree but still refuses a condition whose value count does not fit its operator.
pub fn write_filter(
    w: &mut SqlWriter<'_>,
    filter: &Filter,
    table: Option<&TableDef>,
) -> Result<bool> {
    match filter {
        Filter::And { items } | Filter::Or { items } => {
            let live: Vec<&Filter> = items.iter().filter(|f| !f.is_empty()).collect();
            match live.as_slice() {
                [] => Ok(false),
                [only] => write_filter(w, only, table),
                _ => {
                    let sep = if matches!(filter, Filter::And { .. }) {
                        " AND "
                    } else {
                        " OR "
                    };
                    w.push("(");
                    for (k, item) in live.iter().enumerate() {
                        if k > 0 {
                            w.push(sep);
                        }
                        write_filter(w, item, table)?;
                    }
                    w.push(")");
                    Ok(true)
                }
            }
        }
        Filter::Not { item } => {
            if item.is_empty() {
                return Ok(false);
            }
            w.push("NOT (");
            write_filter(w, item, table)?;
            w.push(")");
            Ok(true)
        }
        Filter::Condition { column, op, values } => {
            write_condition(w, column, *op, values, table)?;
            Ok(true)
        }
        Filter::RawSql { sql } => {
            check_expression(w.quoting(), sql)?;
            w.push("(").push(sql).push(")");
            Ok(true)
        }
    }
}

fn arity_error(column: &str, op: FilterOp) -> Error {
    Error::invalid_input(format!(
        "filter on '{column}': wrong number of values for '{}'",
        op.as_str()
    ))
}

fn write_condition(
    w: &mut SqlWriter<'_>,
    column_name: &str,
    op: FilterOp,
    values: &[Value],
    table: Option<&TableDef>,
) -> Result<()> {
    let q = w.quoting();
    let def = column(table, column_name)?;
    let quoted = q.quote_ident(column_name);
    let lhs = if op.is_pattern() && needs_text_cast(def) {
        q.text_cast(&quoted)
    } else {
        quoted
    };
    let one = || -> Result<Value> {
        match values {
            [v] => Ok(v.clone()),
            _ => Err(arity_error(column_name, op)),
        }
    };
    let pattern = |make: fn(&str) -> String| -> Result<Value> {
        match values {
            [Value::Text(text)] => Ok(Value::Text(make(&escape_like(q, text)))),
            _ => Err(Error::invalid_input(format!(
                "filter on '{column_name}': '{}' needs one text value",
                op.as_str()
            ))),
        }
    };
    let comparison = match op {
        FilterOp::Eq => Some("="),
        FilterOp::Ne => Some("<>"),
        FilterOp::Lt => Some("<"),
        FilterOp::Le => Some("<="),
        FilterOp::Gt => Some(">"),
        FilterOp::Ge => Some(">="),
        FilterOp::Like => Some("LIKE"),
        FilterOp::NotLike => Some("NOT LIKE"),
        _ => None,
    };
    if let Some(sym) = comparison {
        let value = one()?;
        w.push(&lhs).push(" ").push(sym).push(" ").param(value);
        return Ok(());
    }
    let escape = q.quote_str(&LIKE_ESCAPE.to_string());
    match op {
        FilterOp::Contains | FilterOp::NotContains | FilterOp::StartsWith | FilterOp::EndsWith => {
            let value = match op {
                FilterOp::StartsWith => pattern(|text| format!("{text}%"))?,
                FilterOp::EndsWith => pattern(|text| format!("%{text}"))?,
                _ => pattern(|text| format!("%{text}%"))?,
            };
            let not = if op == FilterOp::NotContains {
                "NOT "
            } else {
                ""
            };
            w.push(&lhs)
                .push(" ")
                .push(not)
                .push("LIKE ")
                .param(value)
                .push(" ESCAPE ")
                .push(&escape);
        }
        FilterOp::In | FilterOp::NotIn => {
            if values.is_empty() {
                return Err(arity_error(column_name, op));
            }
            let not = if op == FilterOp::NotIn { "NOT " } else { "" };
            w.push(&lhs).push(" ").push(not).push("IN (");
            for (idx, value) in values.iter().enumerate() {
                if idx > 0 {
                    w.push(", ");
                }
                w.param(value.clone());
            }
            w.push(")");
        }
        FilterOp::IsNull => {
            w.push(&lhs).push(" IS NULL");
        }
        FilterOp::IsNotNull => {
            w.push(&lhs).push(" IS NOT NULL");
        }
        FilterOp::Between | FilterOp::NotBetween => {
            let [low, high] = values else {
                return Err(arity_error(column_name, op));
            };
            let not = if op == FilterOp::NotBetween {
                "NOT "
            } else {
                ""
            };
            w.push("(")
                .push(&lhs)
                .push(" ")
                .push(not)
                .push("BETWEEN ")
                .param(low.clone())
                .push(" AND ")
                .param(high.clone())
                .push(")");
        }
        // Handled above.
        FilterOp::Eq
        | FilterOp::Ne
        | FilterOp::Lt
        | FilterOp::Le
        | FilterOp::Gt
        | FilterOp::Ge
        | FilterOp::Like
        | FilterOp::NotLike => {}
    }
    Ok(())
}

/// Appends the `ORDER BY` keys (without `ORDER BY`). Returns `false`, writing nothing, for no
/// keys. `nulls` uses `NULLS FIRST` / `NULLS LAST` where [`Quoting::nulls_ordering`], else an
/// emulation key `CASE WHEN col IS NULL THEN 0 ELSE 1 END` (first) / `... THEN 1 ELSE 0 END`
/// (last) before the column. With `table`, unknown columns are `InvalidInput`.
pub fn write_order_by(
    w: &mut SqlWriter<'_>,
    sort: &[SortKey],
    table: Option<&TableDef>,
) -> Result<bool> {
    for (k, key) in sort.iter().enumerate() {
        column(table, &key.column)?;
        if k > 0 {
            w.push(", ");
        }
        let native = w.quoting().nulls_ordering();
        if let (Some(nulls), false) = (key.nulls, native) {
            let (when_null, otherwise) = match nulls {
                NullsOrder::First => ("0", "1"),
                NullsOrder::Last => ("1", "0"),
            };
            w.push("CASE WHEN ")
                .ident(&key.column)
                .push(" IS NULL THEN ")
                .push(when_null)
                .push(" ELSE ")
                .push(otherwise)
                .push(" END, ");
        }
        w.ident(&key.column)
            .push(if key.descending { " DESC" } else { " ASC" });
        if native {
            match key.nulls {
                Some(NullsOrder::First) => {
                    w.push(" NULLS FIRST");
                }
                Some(NullsOrder::Last) => {
                    w.push(" NULLS LAST");
                }
                None => {}
            }
        }
    }
    Ok(!sort.is_empty())
}

/// The grid's table-data statement: `SELECT * FROM <qualified table> [WHERE <filter>]
/// [ORDER BY <sort>] [<limit clause>]`, with every filter value bound.
///
/// `table` (the loaded definition, when the caller has it) lets unknown columns fail early and
/// pattern operators cast non-text columns. The query is validated first
/// ([`TableDataQuery::validate`]); the object must be a table, view or materialized view.
pub fn select_table_data(
    q: &dyn Quoting,
    query: &TableDataQuery,
    table: Option<&TableDef>,
) -> Result<BoundStatement> {
    query.validate()?;
    if !matches!(
        query.table.kind,
        ObjectKind::Table | ObjectKind::View | ObjectKind::MatView
    ) {
        return Err(Error::invalid_input(format!(
            "{} is not a table or view",
            query.table.display_name()
        )));
    }
    let mut w = SqlWriter::new(q);
    w.push("SELECT * FROM ").qualified(&query.table);
    if let Some(filter) = query.filter.as_ref().filter(|f| !f.is_empty()) {
        w.push(" WHERE ");
        write_filter(&mut w, filter, table)?;
    }
    if !query.sort.is_empty() {
        w.push(" ORDER BY ");
        write_order_by(&mut w, &query.sort, table)?;
    }
    if let Some(limit) = query.limit {
        let clause = q.limit_clause(limit);
        w.push(" ").push(&clause);
    }
    Ok(w.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AnsiQuoting, BacktickQuoting, BracketQuoting, PgQuoting};
    use pretty_assertions::assert_eq;

    fn table() -> TableDef {
        let mut t = TableDef::new(ObjectRef::table(Some("db"), Some("public"), "t"));
        t.columns
            .push(ColumnDef::new("id", DataType::INT64).not_null());
        t.columns.push(ColumnDef::new("name", DataType::Text));
        t.columns.push(ColumnDef::new(
            "order",
            DataType::VarChar { length: Some(5) },
        ));
        t
    }

    fn query(filter: Filter) -> TableDataQuery {
        let mut q = TableDataQuery::new(table().obj);
        q.filter = Some(filter);
        q
    }

    #[test]
    fn writer_binds_values_in_placeholder_order() {
        let mut w = SqlWriter::new(&PgQuoting);
        w.push("UPDATE ")
            .qualified(&ObjectRef::table(Some("db"), Some("s"), "t"))
            .push(" SET ")
            .ident("a\"b")
            .push(" = ")
            .param(Value::Text("x'; DROP TABLE t; --".into()))
            .push(" WHERE ")
            .ident("id")
            .push(" = ")
            .param(Value::Int(7));
        assert_eq!(w.param_count(), 2);
        let st = w.finish();
        assert_eq!(
            st.sql,
            "UPDATE \"s\".\"t\" SET \"a\"\"b\" = $1 WHERE \"id\" = $2"
        );
        assert_eq!(
            st.params,
            [Value::Text("x'; DROP TABLE t; --".into()), Value::Int(7)]
        );
        let mut w = SqlWriter::new(&BacktickQuoting);
        w.ident("a")
            .push(" = ")
            .param(1.into())
            .push(" AND ")
            .ident("b")
            .push(" = ")
            .param(2.into());
        assert_eq!(w.sql(), "`a` = ? AND `b` = ?");
    }

    #[test]
    fn like_escaping() {
        assert_eq!(escape_like(&PgQuoting, "50%_off!"), "50!%!_off!!");
        assert_eq!(escape_like(&BacktickQuoting, "a\\b"), "a\\b");
        assert_eq!(escape_like(&BracketQuoting, "[x]"), "![x]");
        assert_eq!(escape_like(&AnsiQuoting, "caf\u{e9}"), "caf\u{e9}");
    }

    #[test]
    fn expression_check_accepts_single_expressions() {
        for ok in [
            "a > 1",
            "name = 'it''s; fine'",
            "\"weird;col\" IS NULL",
            "(a + (b * 2)) <> 3",
            "x = '--not a comment'",
            "nextval('t_id_seq'::regclass)",
            "a - -1",
        ] {
            assert!(check_expression(&AnsiQuoting, ok).is_ok(), "{ok}");
        }
        assert!(check_expression(&PgQuoting, "x = $$a;b$$").is_ok());
        assert!(check_expression(&PgQuoting, "x = $tag$ ' ; $tag$").is_ok());
        assert!(check_expression(&PgQuoting, "x = $1").is_ok());
        assert!(check_expression(&PgQuoting, "x = E'it\\'s;'").is_ok());
        assert!(check_expression(&BacktickQuoting, "x = 'a\\';b'").is_ok());
        assert!(check_expression(&BacktickQuoting, "`a;b` = \"c\\\"d\"").is_ok());
    }

    #[test]
    fn expression_check_refuses_escapes_from_the_expression() {
        for bad in [
            "",
            "   ",
            "1=1; DROP TABLE t",
            "a = 1 -- rest",
            "a = 1 /* rest",
            "a = 'unterminated",
            "a = \"unterminated",
            "a = (1",
            "a = 1) OR (1 = 1",
            "a = 1) --",
        ] {
            assert!(check_expression(&AnsiQuoting, bad).is_err(), "{bad:?}");
        }
        // Engine lexing matters: these hide a ';' from a naive scanner.
        assert!(check_expression(&BacktickQuoting, "x = 'a\\''; DROP TABLE t").is_err());
        assert!(check_expression(&PgQuoting, "x = $$ ' $$; DROP TABLE t").is_err());
        assert!(check_expression(&PgQuoting, "x = E'\\''; DROP TABLE t").is_err());
        assert!(check_expression(&PgQuoting, "x = $q$ never closed").is_err());
        assert!(check_expression(&BacktickQuoting, "a = 1 # comment").is_err());
        assert!(check_expression(&AnsiQuoting, "a = 1 # not a comment here").is_ok());
    }

    #[test]
    fn filters_render_with_bound_parameters() {
        let f = Filter::and(vec![
            Filter::condition("id", FilterOp::Ge, vec![Value::Int(10)]),
            Filter::or(vec![
                Filter::condition("name", FilterOp::Contains, vec!["50%_x".into()]),
                Filter::condition("name", FilterOp::IsNull, vec![]),
            ]),
            Filter::and(vec![]),
            Filter::negate(Filter::condition(
                "order",
                FilterOp::In,
                vec!["a".into(), "b".into()],
            )),
            Filter::condition("id", FilterOp::Between, vec![1.into(), 9.into()]),
            Filter::raw_sql("length(name) > 2"),
        ]);
        let t = table();
        let st = select_table_data(&PgQuoting, &query(f), Some(&t)).unwrap();
        assert_eq!(
            st.sql,
            "SELECT * FROM \"public\".\"t\" WHERE (\"id\" >= $1 AND (\"name\" LIKE $2 ESCAPE '!' \
             OR \"name\" IS NULL) AND NOT (\"order\" IN ($3, $4)) AND (\"id\" BETWEEN $5 AND $6) \
             AND (length(name) > 2))"
        );
        assert_eq!(
            st.params,
            [
                Value::Int(10),
                Value::Text("%50!%!_x%".into()),
                "a".into(),
                "b".into(),
                Value::Int(1),
                Value::Int(9)
            ]
        );
    }

    #[test]
    fn pattern_operators_cast_non_text_columns() {
        let t = table();
        let f = Filter::condition("id", FilterOp::StartsWith, vec!["12".into()]);
        let pg = select_table_data(&PgQuoting, &query(f.clone()), Some(&t)).unwrap();
        assert_eq!(
            pg.sql,
            "SELECT * FROM \"public\".\"t\" WHERE CAST(\"id\" AS TEXT) LIKE $1 ESCAPE '!'"
        );
        assert_eq!(pg.params, [Value::Text("12%".into())]);
        let my = select_table_data(&BacktickQuoting, &query(f.clone()), Some(&t)).unwrap();
        assert_eq!(
            my.sql,
            "SELECT * FROM `db`.`public`.`t` WHERE CAST(`id` AS CHAR) LIKE ? ESCAPE '!'"
        );
        // Without the definition the column is not cast.
        let bare = select_table_data(&AnsiQuoting, &query(f), None).unwrap();
        assert_eq!(
            bare.sql,
            "SELECT * FROM \"db\".\"public\".\"t\" WHERE \"id\" LIKE ? ESCAPE '!'"
        );
        let ends = Filter::condition("name", FilterOp::EndsWith, vec!["x".into()]);
        let st = select_table_data(&AnsiQuoting, &query(ends), Some(&t)).unwrap();
        assert!(st.sql.ends_with("\"name\" LIKE ? ESCAPE '!'"), "{}", st.sql);
        assert_eq!(st.params, [Value::Text("%x".into())]);
        let raw_like = Filter::condition("name", FilterOp::NotLike, vec!["a_%".into()]);
        let st = select_table_data(&AnsiQuoting, &query(raw_like), Some(&t)).unwrap();
        assert!(st.sql.ends_with("\"name\" NOT LIKE ?"), "{}", st.sql);
        assert_eq!(st.params, [Value::Text("a_%".into())]);
    }

    #[test]
    fn sort_and_limit() {
        let t = table();
        let mut q = TableDataQuery::new(t.obj.clone());
        q.sort = vec![
            SortKey::desc("id"),
            SortKey {
                column: "name".into(),
                descending: false,
                nulls: Some(NullsOrder::Last),
            },
        ];
        q.limit = Some(500);
        let pg = select_table_data(&PgQuoting, &q, Some(&t)).unwrap();
        assert_eq!(
            pg.sql,
            "SELECT * FROM \"public\".\"t\" ORDER BY \"id\" DESC, \"name\" ASC NULLS LAST LIMIT 500"
        );
        assert!(pg.params.is_empty());
        let my = select_table_data(&BacktickQuoting, &q, Some(&t)).unwrap();
        assert_eq!(
            my.sql,
            "SELECT * FROM `db`.`public`.`t` ORDER BY `id` DESC, CASE WHEN `name` IS NULL THEN 1 \
             ELSE 0 END, `name` ASC LIMIT 500"
        );
    }

    #[test]
    fn bad_requests_are_refused() {
        let t = table();
        let unknown = Filter::condition("nope", FilterOp::IsNull, vec![]);
        let err = select_table_data(&PgQuoting, &query(unknown), Some(&t)).unwrap_err();
        assert!(err.to_string().contains("'nope'"), "{err}");
        let mut q = TableDataQuery::new(t.obj.clone());
        q.sort = vec![SortKey::asc("nope")];
        assert!(select_table_data(&PgQuoting, &q, Some(&t)).is_err());
        let raw = Filter::raw_sql("1=1; DELETE FROM t");
        assert!(select_table_data(&PgQuoting, &query(raw), Some(&t)).is_err());
        let null = Filter::condition("id", FilterOp::Eq, vec![Value::Null]);
        assert!(select_table_data(&PgQuoting, &query(null), Some(&t)).is_err());
        let func = TableDataQuery::new(ObjectRef::new(ObjectKind::Function, "f"));
        assert!(select_table_data(&PgQuoting, &func, None).is_err());
        // Only empty groups: no WHERE at all.
        let st = select_table_data(&PgQuoting, &query(Filter::or(vec![])), Some(&t)).unwrap();
        assert_eq!(st.sql, "SELECT * FROM \"public\".\"t\"");
    }
}
