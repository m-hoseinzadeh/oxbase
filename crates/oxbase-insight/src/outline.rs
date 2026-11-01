//! The tolerant parse of one statement (`docs/m6-contracts.md` §4.1). Owner of [`outline`]:
//! the **context** stream.

use std::ops::Range;

use crate::{Dialect, Ident};

// The tolerant parser (private files under `src/parse/`, context stream).
#[path = "parse/mod.rs"]
pub(crate) mod parse;

/// What kind of statement it is (from its leading keywords; `Other` when unknown).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StatementClass {
    /// `SELECT`, `WITH ... SELECT`, `VALUES`, `TABLE t`, a parenthesised query.
    Select,
    Insert,
    Update,
    Delete,
    Merge,
    CreateTable,
    CreateView,
    CreateIndex,
    /// `CREATE FUNCTION / PROCEDURE / TRIGGER / EVENT`.
    CreateRoutine,
    CreateOther,
    Alter,
    Drop,
    /// `CALL`, `EXEC` / `EXECUTE`.
    Call,
    /// `USE db`.
    Use,
    /// `SET`, `PRAGMA`, `SHOW`, `DESCRIBE`, `EXPLAIN` prefixes are skipped first.
    Set,
    /// `BEGIN`, `COMMIT`, `ROLLBACK`, `START TRANSACTION`, `SAVEPOINT`.
    Transaction,
    Other,
    /// Only whitespace and comments.
    Empty,
}

/// The clause a position or reference is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Clause {
    /// Before the first keyword of a statement.
    Start,
    /// Between `SELECT` and `FROM` (also `RETURNING` / `OUTPUT` lists).
    SelectList,
    /// A `FROM` list (also `USING` of `DELETE`, `UPDATE ... FROM`).
    From,
    /// After `JOIN`, before `ON` / `USING`.
    Join,
    /// A join condition (`ON ...`).
    JoinOn,
    Where,
    GroupBy,
    Having,
    /// `ORDER BY` (also inside `OVER (...)`).
    OrderBy,
    /// `LIMIT`, `OFFSET`, `FETCH`, `TOP`.
    Limit,
    /// `PARTITION BY` / `WINDOW` / `OVER (...)` other than its `ORDER BY`.
    Window,
    /// The table of `INSERT INTO` / `MERGE INTO`.
    InsertTarget,
    /// The column list of `INSERT INTO t (...)`.
    InsertColumns,
    /// `VALUES (...)` rows.
    Values,
    /// The table of `UPDATE`.
    UpdateTarget,
    /// `SET a = ...` of `UPDATE` (and `ON DUPLICATE KEY UPDATE`, `ON CONFLICT ... DO UPDATE`).
    UpdateSet,
    /// The table of `DELETE FROM`, `TRUNCATE`, `DROP TABLE`, `ALTER TABLE`.
    Target,
    /// The name being created (`CREATE TABLE <here>`): nothing to complete.
    NewName,
    /// Column definitions of `CREATE TABLE (...)` / `ALTER TABLE ... ADD`.
    ColumnDefinitions,
    /// Where a data type is written (`CAST(x AS <here>)`, after a column name in a definition).
    TypeName,
    /// The routine of `CALL` / `EXEC`.
    RoutineName,
    Other,
}

/// Where the rows of a [`ScopeRelation`] come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelationSource {
    /// A table or view name as written (`schema.table`).
    Named(Vec<Ident>),
    /// A CTE of an enclosing `WITH`: index into the owning [`QueryBlock::ctes`] of block
    /// `query`.
    Cte { query: usize, index: usize },
    /// A derived table `(SELECT ...) AS x`: its query block and output columns.
    Derived { query: usize, columns: Vec<Ident> },
    /// A table function (`generate_series(...)`, `json_each(...)`, `OPENJSON(...)`).
    Function(Vec<Ident>),
    /// `(VALUES ...) AS v(a, b)`: the column names from the alias list.
    Values { columns: Vec<Ident> },
}

/// One relation of a `FROM` / `JOIN` / DML target, with its alias.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeRelation {
    pub source: RelationSource,
    /// `AS x` or a bare alias; `None` when the relation has none.
    pub alias: Option<Ident>,
    /// Column aliases `AS x(a, b)`; empty when none.
    pub column_aliases: Vec<Ident>,
    /// The relation's text range (name through alias).
    pub range: Range<usize>,
}

/// One CTE of a `WITH`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cte {
    pub name: Ident,
    /// The explicit column list `name(a, b)`, else the body's output columns (best effort,
    /// empty when the body selects `*` or is incomplete).
    pub columns: Vec<Ident>,
    /// The body's query block, when it has one.
    pub query: Option<usize>,
    pub range: Range<usize>,
}

/// One `SELECT` (or DML) block: the main query, a CTE body or a subquery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryBlock {
    /// Text range of the block (inside its parentheses for a subquery).
    pub range: Range<usize>,
    /// The block this one is nested in (`None` for the statement's top block).
    pub parent: Option<usize>,
    /// CTEs defined by this block's `WITH`.
    pub ctes: Vec<Cte>,
    /// Relations of its `FROM` / `JOIN`s / DML target, in text order.
    pub relations: Vec<ScopeRelation>,
    /// Output column names (aliases or plain column names; best effort).
    pub output: Vec<Ident>,
    /// The select list has a `*` (output names are then incomplete).
    pub star: bool,
    /// Clause ranges, in text order.
    pub clauses: Vec<(Clause, Range<usize>)>,
}

/// The role of an identifier reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RefRole {
    /// A table or view (`FROM x`, `JOIN x`, `INSERT INTO x`, `UPDATE x`).
    Relation,
    /// A column (`a`, `t.a`), possibly a select-list alias used later.
    Column,
    /// A function call (`f(...)`).
    Function,
    /// A routine of `CALL` / `EXEC`.
    Procedure,
    /// A data type.
    Type,
    Schema,
    Database,
    /// A name being defined (table alias, column alias, CTE name, new object name).
    Definition,
    /// The parser could not tell.
    Unknown,
}

/// One identifier path in the statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameRef {
    /// The parts as written (`s.t.c` = three parts).
    pub parts: Vec<Ident>,
    /// The byte range of each part (same length as `parts`).
    pub ranges: Vec<Range<usize>>,
    pub role: RefRole,
    /// The innermost query block it is in.
    pub query: Option<usize>,
    pub clause: Clause,
}

impl NameRef {
    /// Range of the whole path.
    pub fn range(&self) -> Range<usize> {
        match (self.ranges.first(), self.ranges.last()) {
            (Some(a), Some(b)) => a.start..b.end,
            _ => 0..0,
        }
    }
}

/// The tolerant parse of one statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outline {
    /// The statement's byte range in the full text.
    pub range: Range<usize>,
    pub class: StatementClass,
    /// Query blocks; index 0 is the top block when the statement has one.
    pub queries: Vec<QueryBlock>,
    /// Every identifier reference, in text order.
    pub refs: Vec<NameRef>,
}

impl Outline {
    /// An outline with nothing in it (an empty statement).
    pub fn empty(range: Range<usize>) -> Self {
        Self {
            range,
            class: StatementClass::Empty,
            queries: Vec::new(),
            refs: Vec::new(),
        }
    }

    /// The innermost query block containing `offset` (ties: the later block, which is the
    /// nested one).
    pub fn query_at(&self, offset: usize) -> Option<usize> {
        self.queries
            .iter()
            .enumerate()
            .filter(|(_, q)| q.range.start <= offset && offset <= q.range.end)
            .min_by_key(|(i, q)| (q.range.end - q.range.start, usize::MAX - i))
            .map(|(i, _)| i)
    }

    /// The reference whose range contains `offset` (end inclusive, so a cursor right after a
    /// name finds it).
    pub fn ref_at(&self, offset: usize) -> Option<&NameRef> {
        self.refs.iter().find(|r| {
            let range = r.range();
            range.start <= offset && offset <= range.end
        })
    }

    /// The relations visible in block `query`: its own, then its ancestors' (correlated
    /// subqueries), innermost first.
    pub fn visible_relations(&self, query: usize) -> Vec<&ScopeRelation> {
        let mut out = Vec::new();
        let mut at = Some(query);
        while let Some(i) = at {
            let Some(block) = self.queries.get(i) else {
                break;
            };
            out.extend(block.relations.iter());
            at = block.parent;
        }
        out
    }

    /// The CTEs visible in block `query` (its own and its ancestors'), innermost first, as
    /// (owning block, index) pairs.
    pub fn visible_ctes(&self, query: usize) -> Vec<(usize, &Cte)> {
        let mut out = Vec::new();
        let mut at = Some(query);
        while let Some(i) = at {
            let Some(block) = self.queries.get(i) else {
                break;
            };
            out.extend(block.ctes.iter().map(|c| (i, c)));
            at = block.parent;
        }
        out
    }
}

/// Parses the statement at `range` of `text` (as cut by `oxbase_sql::split`) into an
/// [`Outline`]. Never fails and never panics, whatever the input: unknown or broken parts are
/// skipped (`docs/m6-contracts.md` §4.1 lists what must be recognised per dialect).
pub fn outline(dialect: &Dialect, text: &str, range: Range<usize>) -> Outline {
    parse::parse(dialect, text, range, None).outline
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(range: Range<usize>, parent: Option<usize>, rel: &str) -> QueryBlock {
        QueryBlock {
            range,
            parent,
            ctes: vec![Cte {
                name: Ident {
                    text: format!("c_{rel}"),
                    quoted: false,
                },
                columns: vec![],
                query: None,
                range: 0..0,
            }],
            relations: vec![ScopeRelation {
                source: RelationSource::Named(vec![Ident {
                    text: rel.to_owned(),
                    quoted: false,
                }]),
                alias: None,
                column_aliases: vec![],
                range: 0..0,
            }],
            output: vec![],
            star: false,
            clauses: vec![],
        }
    }

    #[test]
    fn helpers_walk_nested_blocks() {
        let o = Outline {
            range: 0..100,
            class: StatementClass::Select,
            queries: vec![
                block(0..100, None, "outer"),
                block(20..40, Some(0), "inner"),
            ],
            refs: vec![NameRef {
                parts: vec![
                    Ident {
                        text: "t".into(),
                        quoted: false,
                    },
                    Ident {
                        text: "a".into(),
                        quoted: false,
                    },
                ],
                ranges: vec![50..51, 52..53],
                role: RefRole::Column,
                query: Some(0),
                clause: Clause::Where,
            }],
        };
        assert_eq!(o.query_at(30), Some(1));
        assert_eq!(o.query_at(10), Some(0));
        assert_eq!(o.query_at(200), None);
        let names: Vec<_> = o
            .visible_relations(1)
            .iter()
            .map(|r| match &r.source {
                RelationSource::Named(p) => p[0].text.clone(),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(names, ["inner", "outer"]);
        assert_eq!(o.visible_ctes(1)[1].0, 0);
        assert_eq!(o.ref_at(53).map(NameRef::range), Some(50..53));
        assert_eq!(o.ref_at(54), None);
        assert_eq!(Outline::empty(3..3).class, StatementClass::Empty);
    }
}
