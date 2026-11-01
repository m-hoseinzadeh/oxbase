//! Turns a statement's tree into layout commands ([`D`]).
//!
//! A statement (or a query in parentheses) is a sequence of **clauses**, each starting with a
//! clause keyword of its statement kind (`SELECT`, `FROM`, `WHERE`, `GROUP BY`, joins, `SET`,
//! `VALUES`, `RETURNING`, `ON CONFLICT`, ...). Clauses go on lines of their own at the
//! sequence's indent. Inside a clause:
//!
//! - list clauses (`SELECT`, `GROUP BY`, `ORDER BY`, `SET`, `VALUES`, `RETURNING`, ...) stay on
//!   one line when they fit, else put one item per line, indented;
//! - condition clauses (`WHERE`, `HAVING`, `MERGE ... ON`) break before top-level `AND` / `OR`
//!   when too long; a join breaks before its `ON` and the condition's `AND` / `OR`;
//! - `WITH` puts each CTE on its own line, CTE bodies and derived tables are indented on lines
//!   of their own; other subqueries stay inline when they fit;
//! - parenthesised lists (arguments, `IN` lists, column lists) break one item per line when too
//!   long; the column definitions of `CREATE TABLE` always do;
//! - `CASE` stays inline when it fits, else puts each `WHEN` / `ELSE` on its own line.
//!
//! A T-SQL batch (or any statement text holding several statements) is laid out statement by
//! statement: a `;` or a statement keyword at the start of an input line starts a new one.
//!
//! Spacing between neighbours never breaks `oxbase_sql::tokens_equivalent`: where touching
//! matters the input's choice is kept, two string literals stay on one line or on two lines as
//! typed (PostgreSQL concatenates only across a line break), and the input's choice is kept
//! between a word and `(` (MySQL reads `count (x)` and `count(x)` differently).

use std::borrow::Cow;

use oxbase_sql::SqlTokenKind;

use super::doc::D;
use super::prep::{Item, is_any, is_prefix_op};
use super::tree::Node;
use crate::KeywordCase;

/// The separator wanted before a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sep {
    /// Decided from the two neighbours (a space, or nothing).
    Auto,
    /// Already pushed by the caller.
    Already,
    Space,
    Line,
    SoftLine,
    Hard,
}

/// How a clause body is laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Body {
    /// Items after the head on their own indented lines when too long.
    List,
    /// The first item on the head's line, further comma items on their own lines.
    Plain,
    /// Breaks before top-level `AND` / `OR`.
    Cond,
    /// A join: the relation, then `ON` and the condition.
    Join,
    /// `WITH`: one CTE per line.
    With,
}

/// What statement the current clauses belong to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stmt {
    Query,
    Insert,
    Update,
    Delete,
    Merge,
    Other,
}

#[derive(Debug, Clone, Copy)]
struct State {
    stmt: Stmt,
    /// A query part has started (`INSERT ... SELECT`, set operations).
    in_query: bool,
}

/// Which parts of a list get one more indent level for their line breaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Indents {
    /// None (the list is inside an indented parenthesis already).
    None,
    /// All but the first (which stays on the clause's first line).
    Rest,
    All,
}

/// Context for the nodes of one clause body.
#[derive(Debug, Clone, Copy, Default)]
struct Ctx {
    /// A query in parentheses here is a derived table or CTE body: always broken.
    derived: bool,
    /// Parenthesised lists here are table definitions: always broken.
    defs: bool,
}

#[derive(Debug, Clone, Copy)]
struct Clause {
    start: usize,
    /// Nodes of the head phrase.
    head: usize,
    end: usize,
    body: Body,
    derived: bool,
    defs: bool,
    /// Starts a new statement (a line break even inside an inline subquery).
    new_stmt: bool,
    /// A `USING` clause (MERGE's `ON` follows it).
    using: bool,
}

impl Clause {
    fn new(head: usize, body: Body) -> Self {
        Self {
            start: 0,
            head,
            end: 0,
            body,
            derived: false,
            defs: false,
            new_stmt: false,
            using: false,
        }
    }
}

/// Words that start a statement when they begin an input line (T-SQL batches hold several).
const STARTERS: &[&str] = &[
    "ALTER",
    "ANALYZE",
    "BACKUP",
    "BEGIN",
    "CALL",
    "CHECKPOINT",
    "CLOSE",
    "COMMIT",
    "COPY",
    "CREATE",
    "DBCC",
    "DEALLOCATE",
    "DECLARE",
    "DELETE",
    "DENY",
    "DROP",
    "EXEC",
    "EXECUTE",
    "EXPLAIN",
    "GRANT",
    "INSERT",
    "LOCK",
    "MERGE",
    "OPEN",
    "PRAGMA",
    "PREPARE",
    "PRINT",
    "RAISERROR",
    "RESTORE",
    "RETURN",
    "REVOKE",
    "ROLLBACK",
    "SAVE",
    "SELECT",
    "SET",
    "SHOW",
    "THROW",
    "TRUNCATE",
    "UPDATE",
    "USE",
    "VACUUM",
    "WAITFOR",
    "WITH",
];

/// A statement keyword after one of these continues the statement (`ON DELETE`, `DO UPDATE`,
/// `GRANT SELECT, INSERT`, the first item of a list clause).
const NO_BREAK_AFTER: &[&str] = &[
    "AFTER",
    "ALL",
    "AND",
    "AS",
    "BEFORE",
    "BY",
    "DENY",
    "DISTINCT",
    "DO",
    "ELSE",
    "EXCEPT",
    "EXISTS",
    "FOR",
    "FROM",
    "GRANT",
    "IN",
    "INSTEAD",
    "INTERSECT",
    "INTO",
    "IS",
    "JOIN",
    "NOT",
    "OF",
    "ON",
    "OR",
    "OUTPUT",
    "RETURNING",
    "REVOKE",
    "SELECT",
    "SET",
    "THEN",
    "TO",
    "UNION",
    "UPDATE",
    "VALUES",
    "WHEN",
    "WINDOW",
];

/// The layout commands for a statement's `nodes`.
pub(super) fn build<'a>(
    items: &[Item<'a>],
    nodes: &[Node],
    case: KeywordCase,
    leading_commas: bool,
) -> Vec<D<'a>> {
    let mut b = Builder {
        items,
        case,
        leading_commas,
        docs: Vec::new(),
        last: None,
        led: None,
    };
    b.sequence(nodes, Sep::Already, Sep::Hard);
    b.docs
}

struct Builder<'s, 'a> {
    items: &'s [Item<'a>],
    case: KeywordCase,
    leading_commas: bool,
    docs: Vec<D<'a>>,
    /// The last item emitted.
    last: Option<usize>,
    /// The item whose leading comments were pushed ahead of it (before a group opened).
    led: Option<usize>,
}

impl<'a> Builder<'_, 'a> {
    // --- items and separators ---------------------------------------------------------------

    /// The keyword at node `k` of `nodes` (an item the layout reads as a keyword).
    fn word_at(&self, nodes: &[Node], k: usize) -> Option<&'a str> {
        match nodes.get(k) {
            Some(Node::Tok(i)) if self.items[*i].lw => Some(self.items[*i].text),
            _ => None,
        }
    }

    fn word_is(&self, nodes: &[Node], k: usize, w: &str) -> bool {
        self.word_at(nodes, k)
            .is_some_and(|x| x.eq_ignore_ascii_case(w))
    }

    fn word_in(&self, nodes: &[Node], k: usize, list: &[&str]) -> bool {
        self.word_at(nodes, k).is_some_and(|x| is_any(x, list))
    }

    fn is_punct_node(&self, node: &Node, p: &str) -> bool {
        matches!(node, Node::Tok(i) if self.items[*i].is_punct(p))
    }

    fn is_word_node(&self, node: &Node, w: &str) -> bool {
        matches!(node, Node::Tok(i) if self.items[*i].is_word(w))
    }

    /// Pushes the separator before item `i`.
    fn sep(&mut self, i: usize, want: Sep) {
        if want == Sep::Already {
            return;
        }
        let Some(last) = self.last else {
            return;
        };
        let items = self.items;
        let (prev, cur) = (&items[last], &items[i]);
        let sep = if prev.is_string() && cur.is_string() {
            if cur.nl_before {
                Sep::Hard
            } else if cur.touch_prev {
                return;
            } else {
                Sep::Space
            }
        } else if cur.glue_prev {
            if cur.touch_prev {
                return;
            }
            match want {
                Sep::Line | Sep::Hard | Sep::Space => want,
                Sep::SoftLine => Sep::Line,
                Sep::Auto | Sep::Already => Sep::Space,
            }
        } else if want == Sep::Auto {
            match self.auto(last, i) {
                Some(sep) => sep,
                None => return,
            }
        } else {
            want
        };
        self.docs.push(match sep {
            Sep::Space | Sep::Auto | Sep::Already => D::Space,
            Sep::Line => D::Line,
            Sep::SoftLine => D::SoftLine,
            Sep::Hard => D::Hard,
        });
    }

    /// The separator between neighbours `p` and `c` inside an expression: `None` to touch.
    fn auto(&self, p: usize, c: usize) -> Option<Sep> {
        let items = self.items;
        let (prev, cur) = (&items[p], &items[c]);
        let keep = if cur.touch_prev {
            None
        } else {
            Some(Sep::Space)
        };
        if cur.is_punct(",") || cur.is_punct(";") || cur.is_punct(")") || prev.is_punct("(") {
            return None;
        }
        if cur.is_punct("(") {
            return if prev.op || prev.is_punct(",") {
                Some(Sep::Space)
            } else {
                keep
            };
        }
        let bracket = |it: &Item<'_>| {
            !it.op && it.kind == SqlTokenKind::Punct && matches!(it.text, "[" | "]" | "{" | "}")
        };
        if bracket(prev) || bracket(cur) {
            return keep;
        }
        if (prev.op && !binary_style(prev.text)) || (cur.op && !binary_style(cur.text)) {
            return keep;
        }
        if is_prefix_op(items, p) {
            return None;
        }
        Some(Sep::Space)
    }

    /// Pushes the leading comments of item `i` now (before a group around it opens, so that
    /// a comment on its own line does not break that group).
    fn lead(&mut self, i: usize) {
        if self.led == Some(i) {
            return;
        }
        let items = self.items;
        for c in &items[i].leading {
            self.docs.push(if c.nl_before { D::Hard } else { D::Space });
            self.docs.push(D::Text(Cow::Borrowed(c.text)));
            self.docs.push(if c.line || c.nl_after {
                D::Hard
            } else {
                D::Space
            });
        }
        self.led = Some(i);
    }

    /// Pushes item `i` with its comments.
    fn token(&mut self, i: usize) {
        self.lead(i);
        self.led = None;
        let items = self.items;
        let it = &items[i];
        let text = if it.kw {
            match self.case {
                KeywordCase::Upper => Cow::Owned(it.text.to_ascii_uppercase()),
                KeywordCase::Lower => Cow::Owned(it.text.to_ascii_lowercase()),
                KeywordCase::Preserve => Cow::Borrowed(it.text),
            }
        } else {
            Cow::Borrowed(it.text)
        };
        self.docs.push(D::Text(text));
        for c in &it.trailing {
            self.docs.push(if c.nl_before { D::Hard } else { D::Space });
            self.docs.push(D::Text(Cow::Borrowed(c.text)));
            if c.line || c.nl_after {
                self.docs.push(D::Hard);
            }
        }
        self.last = Some(i);
    }

    // --- nodes ------------------------------------------------------------------------------

    fn nodes(&mut self, nodes: &[Node], first: Sep, ctx: Ctx) {
        for (k, n) in nodes.iter().enumerate() {
            self.node(n, if k == 0 { first } else { Sep::Auto }, ctx);
        }
    }

    fn node(&mut self, node: &Node, sep: Sep, ctx: Ctx) {
        match node {
            Node::Tok(i) => {
                self.sep(*i, sep);
                self.token(*i);
            }
            Node::Paren { open, close, kids } => self.paren(*open, *close, kids, sep, ctx),
            Node::Case { case, end, kids } => self.case(*case, *end, kids, sep),
        }
    }

    /// `true` when parenthesised `kids` are a query.
    fn is_query(&self, kids: &[Node]) -> bool {
        match kids.first() {
            Some(Node::Paren { kids, .. }) => self.is_query(kids),
            Some(_) => self.word_in(kids, 0, &["SELECT", "WITH", "VALUES"]),
            None => false,
        }
    }

    fn paren(&mut self, open: usize, close: usize, kids: &[Node], sep: Sep, ctx: Ctx) {
        self.sep(open, sep);
        self.lead(open);
        if kids.is_empty() {
            self.token(open);
            self.sep(close, Sep::Auto);
            self.token(close);
            return;
        }
        if self.is_query(kids) {
            if ctx.derived {
                self.token(open);
                self.docs.push(D::Indent);
                self.sequence(kids, Sep::Hard, Sep::Hard);
                self.docs.push(D::Dedent);
                self.sep(close, Sep::Hard);
                self.token(close);
            } else {
                self.docs.push(D::Open);
                self.token(open);
                self.docs.push(D::Indent);
                self.sequence(kids, Sep::SoftLine, Sep::Line);
                self.docs.push(D::Dedent);
                self.sep(close, Sep::SoftLine);
                self.token(close);
                self.docs.push(D::Close);
            }
            return;
        }
        let (parts, commas) = self.split_commas(kids);
        let (first, between, last) = if ctx.defs {
            (Sep::Hard, Sep::Hard, Sep::Hard)
        } else {
            (Sep::SoftLine, Sep::Line, Sep::SoftLine)
        };
        self.docs.push(D::Open);
        self.token(open);
        self.docs.push(D::Indent);
        self.list(
            &parts,
            &commas,
            (first, between),
            Indents::None,
            Ctx::default(),
        );
        self.docs.push(D::Dedent);
        self.sep(close, last);
        self.token(close);
        self.docs.push(D::Close);
    }

    fn case(&mut self, case: usize, end: usize, kids: &[Node], sep: Sep) {
        self.sep(case, sep);
        self.lead(case);
        self.docs.push(D::Open);
        self.token(case);
        let parts = self.case_arms(kids);
        if let Some((operand, arms)) = parts.split_first() {
            self.nodes(operand, Sep::Auto, Ctx::default());
            self.docs.push(D::Indent);
            for arm in arms {
                self.nodes(arm, Sep::Line, Ctx::default());
            }
            self.docs.push(D::Dedent);
        }
        self.sep(end, Sep::Line);
        self.token(end);
        self.docs.push(D::Close);
    }

    /// Comma-separated `parts` (`commas[k]` is the item between part `k` and `k + 1`), the
    /// first after separator `seps.0`, the others after `seps.1`.
    fn list(
        &mut self,
        parts: &[&[Node]],
        commas: &[usize],
        seps: (Sep, Sep),
        indents: Indents,
        ctx: Ctx,
    ) {
        let (first, between) = seps;
        for (k, part) in parts.iter().enumerate() {
            let indent = match indents {
                Indents::None => false,
                Indents::Rest => k > 0,
                Indents::All => true,
            };
            if indent {
                self.docs.push(D::Indent);
            }
            self.list_part(
                part,
                k.checked_sub(1).and_then(|c| commas.get(c)).copied(),
                first,
                between,
                ctx,
            );
            if indent {
                self.docs.push(D::Dedent);
            }
        }
    }

    /// One part of [`Builder::list`], after its comma (`None` for the first part).
    fn list_part(
        &mut self,
        part: &[Node],
        comma: Option<usize>,
        first: Sep,
        between: Sep,
        ctx: Ctx,
    ) {
        let Some(comma) = comma else {
            self.nodes(part, first, ctx);
            return;
        };
        if self.leading_commas {
            let before = if between == Sep::Line {
                Sep::SoftLine
            } else {
                between
            };
            self.sep(comma, before);
            self.token(comma);
            self.nodes(part, Sep::Space, ctx);
        } else {
            self.sep(comma, Sep::Auto);
            self.token(comma);
            self.nodes(part, between, ctx);
        }
    }

    /// `nodes` after `sep`, one indent level deeper.
    fn indented(&mut self, nodes: &[Node], sep: Sep, ctx: Ctx) {
        self.docs.push(D::Indent);
        self.nodes(nodes, sep, ctx);
        self.docs.push(D::Dedent);
    }

    fn split_commas<'n>(&self, nodes: &'n [Node]) -> (Vec<&'n [Node]>, Vec<usize>) {
        let mut parts = Vec::new();
        let mut commas = Vec::new();
        let mut start = 0;
        for (k, n) in nodes.iter().enumerate() {
            if let Node::Tok(i) = n
                && self.items[*i].is_punct(",")
            {
                parts.push(&nodes[start..k]);
                commas.push(*i);
                start = k + 1;
            }
        }
        parts.push(&nodes[start..]);
        (parts, commas)
    }

    /// The inside of a `CASE` cut before every `WHEN` and `ELSE` (the first part, the operand,
    /// may be empty).
    fn case_arms<'n>(&self, nodes: &'n [Node]) -> Vec<&'n [Node]> {
        let mut parts = Vec::new();
        let mut start = 0;
        for (k, n) in nodes.iter().enumerate() {
            if self.is_word_node(n, "WHEN") || self.is_word_node(n, "ELSE") {
                parts.push(&nodes[start..k]);
                start = k;
            }
        }
        parts.push(&nodes[start..]);
        parts
    }

    /// A condition cut before its top-level `AND` / `OR` (not the `AND` of `BETWEEN`).
    fn cond_parts<'n>(&self, nodes: &'n [Node]) -> Vec<&'n [Node]> {
        let mut parts = Vec::new();
        let mut start = 0;
        let mut between = false;
        for (k, n) in nodes.iter().enumerate() {
            if self.is_word_node(n, "BETWEEN") {
                between = true;
                continue;
            }
            let and = self.is_word_node(n, "AND");
            if and && between {
                between = false;
                continue;
            }
            if k > start && (and || self.is_word_node(n, "OR")) {
                parts.push(&nodes[start..k]);
                start = k;
            }
        }
        parts.push(&nodes[start..]);
        parts
    }

    // --- clauses ----------------------------------------------------------------------------

    fn sequence(&mut self, nodes: &[Node], first: Sep, clause_sep: Sep) {
        let clauses = self.segment(nodes);
        for (k, c) in clauses.iter().enumerate() {
            let sep = if k == 0 {
                first
            } else if c.new_stmt {
                Sep::Hard
            } else {
                clause_sep
            };
            self.clause(nodes, c, sep);
        }
    }

    fn clause(&mut self, nodes: &[Node], c: &Clause, sep: Sep) {
        let all = &nodes[c.start..c.end];
        let Some(first_node) = all.first() else {
            return;
        };
        let (head, body) = all.split_at(c.head.min(all.len()));
        let ctx = Ctx {
            derived: c.derived,
            defs: c.defs,
        };
        if c.body == Body::With {
            self.nodes(head, sep, Ctx::default());
            let (parts, commas) = self.split_commas(body);
            let first = if head.is_empty() { sep } else { Sep::Auto };
            let derived = Ctx {
                derived: true,
                defs: false,
            };
            for (k, part) in parts.iter().enumerate() {
                let Some(&comma) = k.checked_sub(1).and_then(|c| commas.get(c)) else {
                    self.nodes(part, first, derived);
                    continue;
                };
                if self.leading_commas {
                    self.sep(comma, Sep::Hard);
                    self.token(comma);
                    self.nodes(part, Sep::Space, derived);
                } else {
                    self.sep(comma, Sep::Auto);
                    self.token(comma);
                    self.nodes(part, Sep::Hard, derived);
                }
            }
            return;
        }
        self.sep(first_node.first(), sep);
        self.lead(first_node.first());
        self.docs.push(D::Open);
        self.nodes(head, Sep::Already, Ctx::default());
        let body_first = match (head.is_empty(), c.body) {
            (true, _) => Sep::Already,
            (false, Body::List) => Sep::Line,
            (false, _) => Sep::Auto,
        };
        match c.body {
            Body::List | Body::Plain | Body::With => {
                let (parts, commas) = self.split_commas(body);
                let indents = if c.body == Body::List && !head.is_empty() {
                    Indents::All
                } else {
                    Indents::Rest
                };
                self.list(&parts, &commas, (body_first, Sep::Line), indents, ctx);
            }
            Body::Cond => {
                for (k, part) in self.cond_parts(body).into_iter().enumerate() {
                    if k == 0 {
                        self.nodes(part, body_first, ctx);
                    } else {
                        self.indented(part, Sep::Line, ctx);
                    }
                }
            }
            Body::Join => {
                let on = body.iter().position(|n| self.is_word_node(n, "ON"));
                let (relation, cond) = body.split_at(on.unwrap_or(body.len()));
                self.nodes(relation, body_first, ctx);
                for (k, part) in self.cond_parts(cond).into_iter().enumerate() {
                    if k == 0 && relation.is_empty() {
                        self.nodes(part, body_first, ctx);
                    } else {
                        self.indented(part, Sep::Line, ctx);
                    }
                }
            }
        }
        self.docs.push(D::Close);
    }

    /// Cuts `nodes` into clauses.
    fn segment(&self, nodes: &[Node]) -> Vec<Clause> {
        let mut out: Vec<Clause> = Vec::new();
        let mut st = State {
            stmt: Stmt::Other,
            in_query: false,
        };
        let mut at_start = true;
        let mut i = 0;
        while i < nodes.len() {
            let found = if at_start {
                Some(self.statement_head(nodes, i, &mut st, i > 0))
            } else {
                self.clause_at(nodes, i, &mut st, out.last())
            };
            at_start = false;
            let Some(mut c) = found else {
                at_start = self.is_punct_node(&nodes[i], ";");
                i += 1;
                continue;
            };
            if let Some(prev) = out.last_mut() {
                prev.end = i;
            }
            c.start = i;
            c.end = nodes.len();
            let skip = c.head.max(1);
            out.push(c);
            if let Some(last) = nodes.get(i + skip - 1) {
                at_start = self.is_punct_node(last, ";");
            }
            i += skip;
        }
        out
    }

    /// Number of consecutive keyword items from node `k`.
    fn keyword_run(&self, nodes: &[Node], k: usize) -> usize {
        (k..nodes.len())
            .take_while(|&j| self.word_at(nodes, j).is_some())
            .count()
    }

    /// The first clause of a statement starting at node `i`; sets the statement kind.
    fn statement_head(&self, nodes: &[Node], i: usize, st: &mut State, new_stmt: bool) -> Clause {
        let mut real = i;
        if self.word_in(nodes, i, &["EXPLAIN", "DESCRIBE", "DESC"]) {
            let found = (i + 1..nodes.len().min(i + 12)).find(|&j| {
                self.word_in(
                    nodes,
                    j,
                    &[
                        "SELECT", "WITH", "INSERT", "UPDATE", "DELETE", "MERGE", "VALUES",
                        "REPLACE",
                    ],
                )
            });
            if let Some(j) = found {
                real = j;
            }
        }
        let word = self.word_at(nodes, real).unwrap_or("");
        let is = |list: &[&str]| is_any(word, list);
        let (stmt, body, head_end) = if is(&["SELECT", "VALUES"]) {
            (Stmt::Query, Body::List, real + 1)
        } else if is(&["WITH"]) {
            let recursive = usize::from(self.word_is(nodes, real + 1, "RECURSIVE"));
            (Stmt::Query, Body::With, real + 1 + recursive)
        } else if is(&["INSERT", "REPLACE", "UPSERT"]) {
            (
                Stmt::Insert,
                Body::Plain,
                real + self.keyword_run(nodes, real),
            )
        } else if is(&["UPDATE"]) {
            (Stmt::Update, Body::Plain, real + 1)
        } else if is(&["DELETE"]) {
            (
                Stmt::Delete,
                Body::Plain,
                real + self.keyword_run(nodes, real),
            )
        } else if is(&["MERGE"]) {
            (
                Stmt::Merge,
                Body::Plain,
                real + self.keyword_run(nodes, real),
            )
        } else if word.is_empty()
            && matches!(&nodes[real], Node::Paren { kids, .. } if self.is_query(kids))
        {
            (Stmt::Query, Body::Plain, real)
        } else {
            (Stmt::Other, Body::Plain, i + self.keyword_run(nodes, i))
        };
        let head = head_end.saturating_sub(i);
        let defs = stmt == Stmt::Other
            && self.word_is(nodes, i, "CREATE")
            && (i..i + head).any(|k| self.word_is(nodes, k, "TABLE"));
        st.stmt = stmt;
        st.in_query = stmt == Stmt::Query;
        Clause {
            defs,
            new_stmt,
            ..Clause::new(head, body)
        }
    }

    /// The length of a join phrase at node `i` (`LEFT OUTER JOIN`, `CROSS APPLY`).
    fn join_head(&self, nodes: &[Node], i: usize) -> Option<usize> {
        let at = |k: usize, w: &str| self.word_is(nodes, k, w);
        if at(i, "STRAIGHT_JOIN") {
            return Some(1);
        }
        if (at(i, "CROSS") || at(i, "OUTER")) && at(i + 1, "APPLY") {
            return Some(2);
        }
        let mut k = i;
        if at(k, "NATURAL") {
            k += 1;
        }
        if at(k, "INNER") || at(k, "CROSS") {
            k += 1;
        } else if at(k, "LEFT") || at(k, "RIGHT") || at(k, "FULL") {
            k += 1;
            if at(k, "OUTER") {
                k += 1;
            }
        }
        at(k, "JOIN").then_some(k + 1 - i)
    }

    /// The clause starting at node `i` of a statement already under way, if one does.
    fn clause_at(
        &self,
        nodes: &[Node],
        i: usize,
        st: &mut State,
        cur: Option<&Clause>,
    ) -> Option<Clause> {
        let word = self.word_at(nodes, i)?;
        // After an operator a keyword is an operand (`a = VALUES(a)`), never a clause (`*` is
        // an operand itself in `SELECT * FROM`).
        if i > 0
            && matches!(&nodes[i - 1], Node::Tok(prev) if self.items[*prev].op && self.items[*prev].text != "*")
        {
            return None;
        }
        let is = |other: &str| word.eq_ignore_ascii_case(other);
        let next = |ahead: usize, other: &str| self.word_is(nodes, i + ahead, other);
        let query = st.stmt == Stmt::Query || st.in_query;
        let dml = matches!(st.stmt, Stmt::Update | Stmt::Delete);
        let insert = st.stmt == Stmt::Insert;
        let cur_body = cur.map(|clause| clause.body);

        if is_any(word, &["UNION", "INTERSECT", "EXCEPT", "MINUS"]) && (query || insert) {
            st.in_query = true;
            let head = if next(1, "ALL") || next(1, "DISTINCT") {
                2
            } else {
                1
            };
            return Some(Clause::new(head, Body::Plain));
        }
        if is("SELECT") && (query || insert) {
            st.in_query = true;
            return Some(Clause::new(1, Body::List));
        }
        if query || dml {
            if is("FROM") {
                return Some(Clause {
                    derived: true,
                    ..Clause::new(1, Body::Plain)
                });
            }
            if is("WHERE") || is("HAVING") {
                return Some(Clause::new(1, Body::Cond));
            }
            if (is("GROUP") || is("ORDER")) && next(1, "BY") {
                return Some(Clause::new(2, Body::List));
            }
            if is("LIMIT") {
                return Some(Clause::new(1, Body::Plain));
            }
            if let Some(head) = self.join_head(nodes, i) {
                return Some(Clause {
                    derived: true,
                    ..Clause::new(head, Body::Join)
                });
            }
        }
        if query {
            if is("WINDOW") {
                return Some(Clause::new(1, Body::List));
            }
            if is("OFFSET") || is("INTO") {
                return Some(Clause::new(1, Body::Plain));
            }
            if is("FETCH") && (next(1, "FIRST") || next(1, "NEXT")) {
                return Some(Clause::new(1, Body::Plain));
            }
            if is("FOR")
                && self.word_in(
                    nodes,
                    i + 1,
                    &[
                        "UPDATE", "SHARE", "NO", "KEY", "XML", "JSON", "BROWSE", "READ",
                    ],
                )
            {
                return Some(Clause::new(1, Body::Plain));
            }
            if is("OPTION") && matches!(nodes.get(i + 1), Some(Node::Paren { .. })) {
                return Some(Clause::new(1, Body::Plain));
            }
        }
        if insert {
            if is("WHERE") {
                return Some(Clause::new(1, Body::Cond));
            }
            if is("DEFAULT") && next(1, "VALUES") {
                return Some(Clause::new(2, Body::Plain));
            }
            if is("VALUES") || is("SET") {
                return Some(Clause::new(1, Body::List));
            }
            if is("ON") && next(1, "CONFLICT") {
                return Some(Clause::new(2, Body::Plain));
            }
            if is("DO") && next(1, "UPDATE") && next(2, "SET") {
                return Some(Clause::new(3, Body::List));
            }
            if is("DO") && next(1, "NOTHING") {
                return Some(Clause::new(2, Body::Plain));
            }
            if is("ON") && next(1, "DUPLICATE") && next(2, "KEY") && next(3, "UPDATE") {
                return Some(Clause::new(4, Body::List));
            }
        }
        if st.stmt == Stmt::Update && is("SET") {
            return Some(Clause::new(1, Body::List));
        }
        if matches!(st.stmt, Stmt::Insert | Stmt::Update | Stmt::Delete) && is("RETURNING") {
            return Some(Clause::new(1, Body::List));
        }
        if matches!(
            st.stmt,
            Stmt::Insert | Stmt::Update | Stmt::Delete | Stmt::Merge
        ) && is("OUTPUT")
        {
            return Some(Clause::new(1, Body::List));
        }
        if matches!(st.stmt, Stmt::Delete | Stmt::Merge)
            && is("USING")
            && cur_body != Some(Body::Join)
        {
            return Some(Clause {
                derived: true,
                using: true,
                ..Clause::new(1, Body::Plain)
            });
        }
        if st.stmt == Stmt::Merge {
            if is("ON") && cur.is_some_and(|clause| clause.using) {
                return Some(Clause::new(1, Body::Cond));
            }
            if is("WHEN") {
                return Some(Clause::new(1, Body::Plain));
            }
        }
        // `WITH ... INSERT / UPDATE / DELETE / MERGE`
        if st.stmt == Stmt::Query
            && cur_body == Some(Body::With)
            && is_any(word, &["INSERT", "UPDATE", "DELETE", "MERGE"])
        {
            return Some(self.statement_head(nodes, i, st, false));
        }
        // `CREATE VIEW v AS SELECT ...`, `CREATE TABLE t AS SELECT ...`
        if st.stmt == Stmt::Other
            && is_any(word, &["SELECT", "WITH", "VALUES"])
            && i > 0
            && self.word_is(nodes, i - 1, "AS")
        {
            return Some(self.statement_head(nodes, i, st, false));
        }
        // A new statement on its own line (T-SQL batches).
        if is_any(word, STARTERS)
            && self.items[nodes[i].first()].nl_before
            && !self.no_break(nodes, i)
        {
            return Some(self.statement_head(nodes, i, st, true));
        }
        None
    }

    /// `true` when the node before `i` binds the statement keyword at `i` to the current
    /// statement.
    fn no_break(&self, nodes: &[Node], i: usize) -> bool {
        let Some(Node::Tok(p)) = i.checked_sub(1).and_then(|p| nodes.get(p)) else {
            return false;
        };
        let prev = &self.items[*p];
        prev.is_punct(",") || prev.op || prev.is_any_word(NO_BREAK_AFTER)
    }
}

/// An operator laid out with spaces around it (`a = b`, `x || y`); others keep the input's
/// spacing (`a::int`, `@var`, `#>`).
fn binary_style(text: &str) -> bool {
    !text.contains("--") && text.chars().all(|c| "=<>!+-*/%|^~".contains(c))
}
