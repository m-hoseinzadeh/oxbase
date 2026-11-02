//! A tolerant reading of statements for diagnostics and hover: which relations each query
//! block reads (with aliases, CTEs and derived tables), which names are column references and
//! in which clause, which functions are called with how many arguments, and which DDL targets
//! a statement names.
//!
//! It never fails: unknown constructs are skipped, and anything the parser is unsure about
//! makes the statement `uncertain` (its catalog findings are then hints). A splitter statement
//! may hold several SQL statements (a T-SQL batch, a routine body); [`analyze`] cuts it first.

// Token indices are `i`, `j`, `k`, `c` (a closing parenthesis), the token list `t`, the block `b`.
#![allow(clippy::many_single_char_names)]

use std::ops::Range;

use oxbase_dialect::SqlLanguage;
use oxbase_sql::NamePart;

use super::features::Features;
use super::scan::{Kind, Toks};
use super::words::{self, has};
use super::{Diagnostic, DiagnosticCode, Severity};

/// Deepest nesting of parentheses and subqueries the parser follows.
const MAX_DEPTH: u32 = 160;

/// An identifier as written, with its range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Name {
    pub text: String,
    pub quoted: bool,
    pub range: Range<usize>,
}

impl Name {
    pub fn part(&self) -> NamePart {
        NamePart {
            text: self.text.clone(),
            quoted: self.quoted,
        }
    }
}

/// Where the rows of a [`Rel`] come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Source {
    /// A table, view or CTE name.
    Table(Vec<Name>),
    /// `(SELECT ...)`: the block of the subquery.
    Derived(usize),
    /// A table function.
    Function(Vec<Name>),
    /// A table variable, `PIVOT`, ...: columns unknown.
    Unknown,
}

/// One relation of a `FROM` list or a DML target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Rel {
    pub source: Source,
    pub alias: Option<Name>,
    pub col_aliases: Vec<Name>,
    /// Check that the relation exists.
    pub check: bool,
    /// The table an `INSERT` / `UPDATE` / `DELETE` / `MERGE` writes.
    pub target: bool,
    /// An `INSERT` target: its columns are not in scope of `VALUES`.
    pub hidden_target: bool,
    /// An `UPDATE` / `DELETE` target that names a relation of the `FROM` list again: column
    /// lookups use that one.
    pub dup: bool,
}

impl Rel {
    /// The last part of a table name.
    pub fn table_last(&self) -> Option<&Name> {
        match &self.source {
            Source::Table(p) => p.last(),
            _ => None,
        }
    }

    /// The name the relation goes by in the query (alias, else table name).
    pub fn display(&self) -> String {
        if let Some(a) = &self.alias {
            return a.text.clone();
        }
        match &self.source {
            Source::Table(p) | Source::Function(p) => p
                .iter()
                .map(|n| n.text.as_str())
                .collect::<Vec<_>>()
                .join("."),
            Source::Derived(_) | Source::Unknown => "subquery".to_owned(),
        }
    }
}

/// One CTE.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CteDef {
    pub name: Name,
    pub columns: Vec<Name>,
    pub block: Option<usize>,
}

/// One query block (a `SELECT` core, a DML statement, a CTE body, a subquery).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Block {
    pub parent: Option<usize>,
    pub range: Range<usize>,
    pub rels: Vec<Rel>,
    pub ctes: Vec<CteDef>,
    /// Output names in order; `None` for an expression without a name.
    pub outputs: Vec<Option<Name>>,
    /// The select list has a `*` or the outputs cannot be known.
    pub star: bool,
    /// `JOIN ... USING` / `NATURAL JOIN`: same-named columns are merged.
    pub merged: bool,
    /// A join with a condition (rows of an `UPDATE` / `DELETE` are restricted by it).
    pub joined: bool,
    pub has_where: bool,
    /// For a later core of a set operation: the first core (whose outputs name the columns).
    pub set_root: Option<usize>,
}

/// How a column reference is resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefCtx {
    Normal,
    /// `ORDER BY`, `GROUP BY`, `HAVING`: the name may be a select-list alias.
    AliasOk,
    /// A column of the DML target (`INSERT INTO t (a)`, `SET a = ...`).
    TargetOnly,
}

/// A column reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ColRef {
    pub parts: Vec<Name>,
    pub block: usize,
    pub ctx: RefCtx,
    /// May see an `INSERT` target (its `SET`, `RETURNING` and conflict clauses).
    pub hidden: bool,
}

/// A function call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Call {
    pub name: Vec<Name>,
    /// Number of arguments, `None` when the call has syntax words (`CAST(x AS t)`,
    /// `COUNT(DISTINCT a)`).
    pub args: Option<usize>,
    pub range: Range<usize>,
}

/// What a statement is, from its first keyword.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Head {
    Query,
    Insert,
    Update,
    Delete,
    Merge,
    Ddl,
    Other,
}

/// One SQL statement as read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Stmt {
    pub range: Range<usize>,
    pub head: Head,
    pub blocks: Vec<Block>,
    pub cols: Vec<ColRef>,
    pub calls: Vec<Call>,
    /// Relations named by DDL (`REFERENCES t`, `CREATE INDEX ... ON t`, `ALTER TABLE t`).
    pub targets: Vec<Vec<Name>>,
    /// Relations a `DROP` names (often guarded by `IF OBJECT_ID(...)`: hints only).
    pub dropped: Vec<Vec<Name>>,
    /// Tables this statement creates (`SELECT ... INTO t`).
    pub created: Vec<Vec<Name>>,
    /// Findings of the reading itself (syntax, dialect, dangerous statements).
    pub findings: Vec<Diagnostic>,
    /// The parser met something it does not fully read.
    pub uncertain: bool,
    /// Inside a routine or trigger body (names may be parameters or variables).
    pub in_routine: bool,
    /// `USE db`.
    pub use_db: Option<Name>,
    /// `SET search_path` and friends.
    pub sets_path: bool,
    /// SQLite `ATTACH ... AS name`.
    pub attached: Option<Name>,
}

/// Reads one statement of the splitter (its tokens) into the SQL statements it holds.
pub(crate) fn analyze(lang: &SqlLanguage, feat: &Features, toks: &Toks<'_>) -> Vec<Stmt> {
    let mut segs = Vec::new();
    split(toks, feat, 0, toks.len(), false, &mut segs);
    segs.into_iter()
        .map(|(lo, hi, in_routine)| {
            let start = toks.range(lo).start;
            let end = toks.range(hi.saturating_sub(1)).end.max(start);
            let mut p = Parser {
                t: toks,
                lang,
                feat,
                st: Stmt {
                    range: start..end,
                    head: Head::Other,
                    blocks: Vec::new(),
                    cols: Vec::new(),
                    calls: Vec::new(),
                    targets: Vec::new(),
                    dropped: Vec::new(),
                    created: Vec::new(),
                    findings: Vec::new(),
                    uncertain: toks.opaque,
                    in_routine,
                    use_db: None,
                    sets_path: false,
                    attached: None,
                },
                depth: 0,
            };
            p.statement(lo, hi);
            p.st
        })
        .collect()
}

// --- cutting a splitter statement into SQL statements ----------------------------------------

fn split(
    t: &Toks<'_>,
    feat: &Features,
    lo: usize,
    hi: usize,
    in_routine: bool,
    out: &mut Vec<(usize, usize, bool)>,
) {
    let mut start = lo;
    while start < hi {
        if t.is_punct(start, b';') {
            start += 1;
            continue;
        }
        if !in_routine
            && t.word_in(start, &["ALTER", "CREATE"])
            && let Some(body) = routine_body(t, feat, start, hi)
        {
            out.push((start, body, in_routine));
            split(t, feat, body, hi, true, out);
            return;
        }
        let end = segment_end(t, feat, start, hi);
        push_segment(t, start, end, in_routine, out);
        start = end.max(start + 1);
    }
}

/// Where the statement starting at `start` ends: its `;`, the next statement's first word
/// (T-SQL batches), or `hi`.
fn segment_end(t: &Toks<'_>, feat: &Features, start: usize, hi: usize) -> usize {
    // `UPDATE STATISTICS t` is maintenance, not DML: a `SET` after it starts a statement.
    let head = if update_statistics(t, start) {
        "UPDATE STATISTICS".to_owned()
    } else {
        t.upper(start)
    };
    let mut main_seen = false;
    // The first DML word of the statement (`WITH x AS (...) UPDATE ...`).
    let mut main_word = String::new();
    let mut set_seen = false;
    let mut saw_as = false;
    let mut case_depth = 0u32;
    let mut i = start + 1;
    while i < hi {
        match t.kind(i) {
            Some(Kind::Punct(b'(')) => {
                i = t.close_of(i, hi) + 1;
                continue;
            }
            Some(Kind::Punct(b';')) => return i,
            Some(Kind::Word) => {
                let w = t.upper(i);
                if w == "CASE" {
                    case_depth += 1;
                } else if w == "END" && case_depth > 0 {
                    case_depth -= 1;
                    i += 1;
                    continue;
                } else if w == "AS" {
                    saw_as = true;
                }
                if feat.batches && case_depth == 0 {
                    let dml = if head == "WITH" {
                        main_word.as_str()
                    } else {
                        head.as_str()
                    };
                    let state = Batch {
                        head: &head,
                        dml,
                        main_seen,
                        set_seen,
                        saw_as,
                    };
                    if starts_statement(t, &state, i, &w) {
                        return i;
                    }
                    if w == "SET" && dml == "UPDATE" {
                        set_seen = true;
                    }
                }
                if matches!(
                    w.as_str(),
                    "SELECT"
                        | "VALUES"
                        | "EXEC"
                        | "EXECUTE"
                        | "INSERT"
                        | "UPDATE"
                        | "DELETE"
                        | "MERGE"
                ) {
                    if !main_seen {
                        main_word = w;
                    }
                    main_seen = true;
                }
            }
            _ => {}
        }
        i += 1;
    }
    hi
}

/// T-SQL `UPDATE STATISTICS t [...]` at `i` (the `UPDATE` word), not an `UPDATE` of a table
/// named `statistics`.
fn update_statistics(t: &Toks<'_>, i: usize) -> bool {
    t.is_word(i, "UPDATE")
        && t.is_word(i + 1, "STATISTICS")
        && t.is_name(i + 2)
        && !t.word_in(i + 2, &["AS", "FROM", "OUTPUT", "SET", "WITH"])
}

/// What [`segment_end`] knows of the statement so far.
struct Batch<'s> {
    /// The statement's first word, upper case.
    head: &'s str,
    /// The DML word the statement runs (`head`, or the word after a `WITH` list).
    dml: &'s str,
    /// A `SELECT`, `VALUES`, `EXEC` or DML word was read.
    main_seen: bool,
    /// An `UPDATE`'s `SET` clause was read.
    set_seen: bool,
    saw_as: bool,
}

/// T-SQL: whether word `w` at `i` starts a new statement of the batch.
fn starts_statement(t: &Toks<'_>, b: &Batch<'_>, i: usize, w: &str) -> bool {
    let (head, main_seen, saw_as) = (b.head, b.main_seen, b.saw_as);
    let prev = t.upper(i - 1);
    let prev_comma = t.is_punct(i - 1, b',');
    let ddl_head = matches!(head, "CREATE" | "ALTER");
    let grant_head = matches!(head, "GRANT" | "REVOKE" | "DENY");
    match w {
        "SELECT" => {
            !(has(words::SELECT_CONT_PREV, &prev)
                || (matches!(head, "INSERT" | "WITH") && !main_seen)
                || (ddl_head && saw_as && !main_seen)
                || grant_head
                || prev_comma)
        }
        "INSERT" | "UPDATE" | "DELETE" | "MERGE" => {
            // `SET IDENTITY_INSERT t ON` + `INSERT ...`: `ON` ends the option there.
            !((has(words::DML_CONT_PREV, &prev) && !(head == "SET" && prev == "ON"))
                || prev_comma
                || t.is_punct(i + 1, b'(')
                || (head == "WITH" && !main_seen)
                || grant_head)
        }
        "WITH" => {
            !ddl_head && t.is_name(i + 1) && (t.is_word(i + 2, "AS") || t.is_punct(i + 2, b'('))
        }
        "SET" => {
            // T-SQL `INSERT` has no `SET` clause and an `UPDATE` only one: a `SET` after them
            // is the next statement (`SET NOCOUNT OFF`, `SET IDENTITY_INSERT t OFF`).
            let next_dml = b.dml == "INSERT" || (b.dml == "UPDATE" && b.set_seen);
            next_dml
                || !(matches!(head, "UPDATE" | "MERGE" | "ALTER" | "CREATE" | "WITH")
                    || matches!(prev.as_str(), "UPDATE" | "ON" | "DELETE"))
        }
        // `OFFSET n ROWS FETCH NEXT ...` continues the query; a cursor `FETCH` starts one.
        "FETCH" => !(matches!(prev.as_str(), "ROW" | "ROWS") || grant_head),
        "CREATE" | "ALTER" | "DROP" => !(ddl_head || grant_head || prev == "OR"),
        "IF" => {
            !(matches!(head, "DROP" | "CREATE" | "ALTER") && t.word_in(i + 1, &["EXISTS", "NOT"]))
        }
        _ if has(words::TSQL_SPLIT, w) => {
            let exec = matches!(w, "EXEC" | "EXECUTE");
            !((exec
                && ((head == "INSERT" && !main_seen) || prev == "WITH" || grant_head || ddl_head))
                || (w == "RETURN" && ddl_head)
                || grant_head)
        }
        _ => false,
    }
}

/// For `CREATE ... FUNCTION | PROCEDURE | TRIGGER | EVENT`: the index where its body starts
/// (after `BEGIN`'s position, `RETURN`, `DO`, `FOR EACH ROW`, or T-SQL `AS`).
fn routine_body(t: &Toks<'_>, feat: &Features, start: usize, hi: usize) -> Option<usize> {
    let limit = (start + 16).min(hi);
    let mut j = start + 1;
    let mut found = false;
    while j < limit {
        if t.word_in(j, words::ROUTINES) {
            found = true;
            break;
        }
        if t.kind(j) == Some(Kind::Word) && has(words::CREATE_OBJECTS, &t.upper(j)) {
            return None;
        }
        j += 1;
    }
    if !found {
        return None;
    }
    let mut k = j + 1;
    while k < hi {
        match t.kind(k) {
            Some(Kind::Punct(b'(')) => {
                k = t.close_of(k, hi) + 1;
                continue;
            }
            Some(Kind::Word) => {
                let w = t.upper(k);
                match w.as_str() {
                    "BEGIN" => {
                        let mut s = k + 1;
                        if t.is_word(s, "NOT") && t.is_word(s + 1, "ATOMIC") {
                            s += 2;
                        } else if t.is_word(s, "ATOMIC") {
                            s += 1;
                        }
                        return Some(s);
                    }
                    "RETURN" => return Some(k),
                    "DO" => return Some(k + 1),
                    "AS" if feat.batches && !t.is_word(k - 1, "EXECUTE") => return Some(k + 1),
                    "ROW"
                        if t.is_word(k - 1, "EACH")
                            && !t.word_in(k + 1, &["WHEN", "BEGIN", "FOLLOWS", "PRECEDES"]) =>
                    {
                        return Some(k + 1);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        k += 1;
    }
    None
}

/// Records `lo..hi`; inside a routine body, a statement led by control flow (`IF x THEN
/// UPDATE ...`) is cut where its SQL statement starts.
fn push_segment(
    t: &Toks<'_>,
    lo: usize,
    hi: usize,
    in_routine: bool,
    out: &mut Vec<(usize, usize, bool)>,
) {
    if lo >= hi {
        return;
    }
    let control = t.word_in(lo, words::CONTROL)
        || (t.is_name(lo) && t.is_punct(lo + 1, b':') && !t.is_punct(lo + 2, b'='));
    if in_routine && control {
        let mut k = lo + 1;
        while k < hi {
            if t.is_punct(k, b'(') {
                k = t.close_of(k, hi) + 1;
                continue;
            }
            let lead = (t.kind(k - 1) == Some(Kind::Word)
                && has(words::BODY_LEAD, &t.upper(k - 1)))
                || t.is_punct(k - 1, b':');
            if lead
                && t.word_in(
                    k,
                    &[
                        "DELETE", "INSERT", "MERGE", "REPLACE", "SELECT", "UPDATE", "WITH",
                    ],
                )
            {
                out.push((lo, k, true));
                push_segment(t, k, hi, true, out);
                return;
            }
            k += 1;
        }
    }
    out.push((lo, hi, in_routine));
}

// --- the parser -------------------------------------------------------------------------------

/// The clause being read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cl {
    Head,
    Select,
    Into,
    From,
    On,
    Where,
    GroupBy,
    Having,
    Window,
    OrderBy,
    Limit,
    Set,
    Values,
    Returning,
    Skip,
}

/// Order of the clauses of a `SELECT` core (`None`: not checked).
fn rank(cl: Cl) -> Option<u8> {
    match cl {
        Cl::From => Some(1),
        Cl::Where => Some(2),
        Cl::GroupBy | Cl::Having => Some(3),
        Cl::Window => Some(5),
        Cl::OrderBy => Some(6),
        Cl::Limit => Some(7),
        _ => None,
    }
}

/// What a clause keyword must be followed by (for the "incomplete" hint).
fn needs(cl: Cl) -> Option<&'static str> {
    match cl {
        Cl::From => Some("a table"),
        Cl::On | Cl::Where | Cl::Having => Some("a condition"),
        Cl::GroupBy | Cl::OrderBy => Some("an expression"),
        Cl::Set => Some("an assignment"),
        _ => None,
    }
}

/// Expression state.
#[derive(Debug, Clone)]
struct Ex {
    ctx: RefCtx,
    hidden: bool,
    /// The previous token ended an operand: a bare word now is an alias, a unit or a type.
    after: bool,
    /// At the start of a list item.
    item_start: bool,
    /// After `FOR` (`NEXT VALUE FOR seq`): the name is not a column.
    after_for: bool,
    /// Bare one-part names are not columns (`DATEADD(day, ...)`, window names).
    no_bare: bool,
    /// Names are not recorded at all (`LIMIT`, `INTO`, skipped clauses).
    no_refs: bool,
    /// Inside parentheses: `AS` introduces a type.
    in_group: bool,
    /// The top level of a select list.
    select: bool,
    /// The top level of a `SET` list.
    set_list: bool,
    case_depth: u32,
    atoms: u32,
    chain: Option<Name>,
    alias: Option<Name>,
    star: bool,
}

impl Ex {
    fn new(ctx: RefCtx, hidden: bool) -> Self {
        Self {
            ctx,
            hidden,
            after: false,
            item_start: true,
            after_for: false,
            no_bare: false,
            no_refs: false,
            in_group: false,
            select: false,
            set_list: false,
            case_depth: 0,
            atoms: 0,
            chain: None,
            alias: None,
            star: false,
        }
    }

    /// The state inside parentheses opened in this one.
    fn group(&self) -> Self {
        Self {
            in_group: true,
            no_refs: self.no_refs,
            ..Self::new(self.ctx, self.hidden)
        }
    }
}

/// Clause-level state of one block being read.
struct Walk {
    b: usize,
    cl: Cl,
    expect_table: bool,
    /// Relations read now are the statement's DML targets.
    targets: bool,
    ex: Ex,
    rank: u8,
    rank_name: &'static str,
    saw_select: bool,
    /// A clause keyword still waiting for its content: (range, keyword, what it needs).
    pending: Option<(Range<usize>, &'static str, &'static str)>,
}

/// The end of a path of names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tail {
    Name,
    /// `t.*`
    Star,
    /// `t.` (being typed)
    Dot,
}

struct Parser<'p, 'a> {
    t: &'p Toks<'a>,
    lang: &'p SqlLanguage,
    feat: &'p Features,
    st: Stmt,
    depth: u32,
}

impl Parser<'_, '_> {
    // --- findings ---

    fn finding(&mut self, code: DiagnosticCode, range: Range<usize>, sev: Severity, msg: String) {
        let sev = if self.st.uncertain && code == DiagnosticCode::SyntaxError {
            Severity::Hint
        } else {
            sev
        };
        self.st
            .findings
            .push(Diagnostic::new(code, range, sev, msg));
    }

    fn mismatch(&mut self, range: Range<usize>, sev: Severity, msg: String) {
        self.finding(DiagnosticCode::DialectMismatch, range, sev, msg);
    }

    fn incomplete(&mut self, p: (Range<usize>, &'static str, &'static str)) {
        let (range, kw, what) = p;
        self.finding(
            DiagnosticCode::SyntaxError,
            range,
            Severity::Hint,
            format!("`{kw}` needs {what} after it."),
        );
    }

    // --- helpers ---

    fn name_at(&self, i: usize) -> Option<Name> {
        let t = self.t;
        let tok = t.all.get(i)?;
        let raw = t.text(i);
        match tok.kind {
            Kind::Word => Some(Name {
                text: raw.to_owned(),
                quoted: false,
                range: tok.start..tok.end,
            }),
            Kind::Quoted => Some(Name {
                text: oxbase_sql::ident_value(raw),
                quoted: true,
                range: tok.start..tok.end,
            }),
            _ => None,
        }
    }

    /// Reads `a.b.c` (also `a.*`, `db..t`) starting at `i`; returns the parts, how it ends and
    /// the index after it.
    fn path(&self, i: usize, hi: usize) -> (Vec<Name>, Tail, usize) {
        let t = self.t;
        let mut parts = Vec::new();
        if i >= hi {
            return (parts, Tail::Name, i);
        }
        let Some(first) = self.name_at(i) else {
            return (parts, Tail::Name, i);
        };
        parts.push(first);
        let mut j = i + 1;
        while j < hi && t.is_punct(j, b'.') {
            if j + 1 >= hi {
                return (parts, Tail::Dot, j + 1);
            }
            if t.is_punct(j + 1, b'*') {
                return (parts, Tail::Star, j + 2);
            }
            if t.is_punct(j + 1, b'.') {
                // T-SQL `db..table`: the default schema.
                let r = t.range(j + 1);
                parts.push(Name {
                    text: String::new(),
                    quoted: false,
                    range: r.start..r.start,
                });
                j += 1;
                continue;
            }
            match self.name_at(j + 1) {
                Some(n) => {
                    parts.push(n);
                    j += 2;
                }
                None => return (parts, Tail::Dot, j + 1),
            }
        }
        (parts, Tail::Name, j)
    }

    /// The names of a comma list `(a, b c, d)` (first name of each item).
    fn name_list(&self, lo: usize, hi: usize) -> Vec<Name> {
        let t = self.t;
        let mut out = Vec::new();
        let mut item_start = true;
        let mut i = lo;
        while i < hi {
            if t.is_punct(i, b'(') {
                i = t.close_of(i, hi) + 1;
                continue;
            }
            if t.is_punct(i, b',') {
                item_start = true;
            } else if item_start {
                if let Some(n) = self.name_at(i) {
                    out.push(n);
                }
                item_start = false;
            }
            i += 1;
        }
        out
    }

    fn is_keyword(&self, upper: &str) -> bool {
        self.lang.is_keyword(upper) || has(words::EXPR_WORDS, upper)
    }

    /// A word or quoted name that can be a table alias here.
    fn can_alias(&self, i: usize) -> bool {
        let t = self.t;
        match t.kind(i) {
            Some(Kind::Quoted) => true,
            Some(Kind::Word) => {
                let up = t.upper(i);
                !self.lang.is_reserved(&up)
                    && !has(words::ALIAS_STOP, &up)
                    && !has(words::JOIN_MODS, &up)
            }
            _ => false,
        }
    }

    fn starts_query(&self, lo: usize, hi: usize) -> bool {
        let t = self.t;
        let mut i = lo;
        while i < hi && t.is_punct(i, b'(') {
            i += 1;
        }
        i < hi
            && (t.word_in(i, &["SELECT", "VALUES"]) || (t.is_word(i, "WITH") && t.is_name(i + 1)))
    }

    fn new_block(&mut self, parent: Option<usize>, lo: usize, hi: usize) -> usize {
        let t = self.t;
        let start = t.all.get(lo).map_or(self.st.range.end, |x| x.start);
        let end = if hi > lo {
            t.all.get(hi - 1).map_or(start, |x| x.end).max(start)
        } else {
            start
        };
        self.st.blocks.push(Block {
            parent,
            range: start..end,
            ..Block::default()
        });
        self.st.blocks.len() - 1
    }

    /// Skips `TOP n` / `TOP (n)` [`PERCENT`] [`WITH TIES`] at `i` (the `TOP` word); `None`
    /// when `TOP` is not followed by a count.
    fn top(&mut self, b: usize, i: usize, hi: usize) -> Option<usize> {
        let t = self.t;
        let mut j = i + 1;
        let numeric = matches!(t.kind(j), Some(Kind::Number | Kind::Param));
        let paren = t.is_punct(j, b'(');
        if j >= hi || !(numeric || paren) {
            return None;
        }
        if !self.feat.top {
            let sev = if numeric {
                Severity::Warning
            } else {
                Severity::Hint
            };
            let range = t.range(i).start..t.range(j).end;
            self.mismatch(
                range,
                sev,
                format!("`TOP` is not {} syntax; use `LIMIT`.", self.feat.name),
            );
        }
        if paren {
            let c = t.close_of(j, hi);
            self.group(b, j + 1, c, Ex::new(RefCtx::Normal, true).group());
            j = c + 1;
        } else {
            j += 1;
        }
        if t.is_word(j, "PERCENT") {
            j += 1;
        }
        if t.is_word(j, "WITH") && t.is_word(j + 1, "TIES") {
            j += 2;
        }
        Some(j)
    }

    /// Skips DML modifiers (`LOW_PRIORITY`, `IGNORE`, `INTO`, SQLite `OR REPLACE`, `TOP (n)`).
    fn dml_modifiers(&mut self, b: usize, mut i: usize, hi: usize) -> usize {
        let t = self.t;
        loop {
            if i >= hi {
                return i;
            }
            if t.word_in(i, words::DML_MODS) {
                i += 1;
            } else if t.is_word(i, "OR") && t.kind(i + 1) == Some(Kind::Word) {
                i += 2;
            } else if t.is_word(i, "TOP") {
                match self.top(b, i, hi) {
                    Some(j) => i = j,
                    None => return i,
                }
            } else {
                return i;
            }
        }
    }

    // --- statements ---

    fn statement(&mut self, lo: usize, hi: usize) {
        let t = self.t;
        let b = self.new_block(None, lo, hi);
        let mut i = lo;
        while i < hi && t.word_in(i, &["ANALYZE", "EXPLAIN", "RETURN", "VERBOSE"]) {
            i += 1;
            if t.is_punct(i, b'(') && !self.starts_query(i + 1, t.close_of(i, hi)) {
                i = t.close_of(i, hi) + 1;
            }
        }
        if t.is_word(i, "WITH") && t.is_name(i + 1) {
            i = self.with_clause(b, i + 1, hi);
        }
        let w = t.upper(i);
        match w.as_str() {
            "SELECT" | "VALUES" | "" if !w.is_empty() || t.is_punct(i, b'(') => {
                self.st.head = Head::Query;
                self.walk(b, i, hi, Cl::Head, false);
            }
            "INSERT" | "REPLACE"
                if w == "INSERT" || t.is_word(i + 1, "INTO") || t.is_name(i + 1) =>
            {
                self.insert(b, i + 1, hi);
            }
            "UPDATE" if !(self.feat.batches && update_statistics(t, i)) => self.update(b, i, hi),
            "DELETE" => self.delete(b, i, hi),
            "MERGE" => self.merge(b, i + 1, hi),
            "CREATE" => self.create(i + 1, hi),
            "DROP" => self.drop_table(i, hi),
            "ALTER" => self.alter(i + 1, hi),
            "TRUNCATE" => self.truncate(i + 1, hi),
            "USE" => {
                self.st.use_db = self.name_at(i + 1).filter(|_| i + 1 < hi);
            }
            "SET" => {
                let mut j = i + 1;
                if t.word_in(j, &["LOCAL", "SESSION"]) {
                    j += 1;
                }
                if t.word_in(j, &["SCHEMA", "SEARCH_PATH"]) {
                    self.st.sets_path = true;
                }
            }
            "ATTACH" => {
                let mut j = i + 1;
                while j < hi && !t.is_word(j, "AS") {
                    j += 1;
                }
                self.st.attached = self.name_at(j + 1).filter(|_| j + 1 < hi);
            }
            _ => {}
        }
    }

    /// `WITH [RECURSIVE] name [(cols)] AS [[NOT] MATERIALIZED] (body), ...`; returns the index
    /// after the last CTE.
    fn with_clause(&mut self, b: usize, mut i: usize, hi: usize) -> usize {
        let t = self.t;
        if t.is_word(i, "RECURSIVE") {
            i += 1;
        }
        while i < hi {
            let Some(name) = self.name_at(i) else {
                break;
            };
            i += 1;
            let mut columns = Vec::new();
            if t.is_punct(i, b'(') {
                let c = t.close_of(i, hi);
                columns = self.name_list(i + 1, c);
                i = c + 1;
            }
            if !t.is_word(i, "AS") {
                self.st.uncertain = true;
                break;
            }
            i += 1;
            if t.is_word(i, "NOT") {
                i += 1;
            }
            if t.is_word(i, "MATERIALIZED") {
                i += 1;
            }
            if !t.is_punct(i, b'(') {
                self.st.uncertain = true;
                break;
            }
            let c = t.close_of(i, hi);
            self.st.blocks[b].ctes.push(CteDef {
                name,
                columns,
                block: None,
            });
            let k = self.st.blocks[b].ctes.len() - 1;
            let body = self.query(i + 1, c, Some(b));
            self.st.blocks[b].ctes[k].block = Some(body);
            i = c + 1;
            // PostgreSQL `SEARCH ... SET x` / `CYCLE ... SET x USING y`
            while i < hi && t.word_in(i, &["CYCLE", "SEARCH"]) {
                self.st.uncertain = true;
                while i < hi && !t.is_punct(i, b',') && !t.word_in(i, words::STATEMENT_STARTS) {
                    i += 1;
                }
            }
            if t.is_punct(i, b',') {
                i += 1;
            } else {
                break;
            }
        }
        i
    }

    /// A query in `lo..hi` (`[WITH ...] SELECT ...`, `VALUES`, a parenthesised query).
    fn query(&mut self, lo: usize, hi: usize, parent: Option<usize>) -> usize {
        let t = self.t;
        let b = self.new_block(parent, lo, hi);
        let mut i = lo;
        if t.is_word(i, "WITH") && t.is_name(i + 1) {
            i = self.with_clause(b, i + 1, hi);
        }
        self.walk(b, i, hi, Cl::Head, false);
        b
    }

    fn insert(&mut self, b: usize, i: usize, hi: usize) {
        let t = self.t;
        self.st.head = Head::Insert;
        let mut i = self.dml_modifiers(b, i, hi);
        if t.kind(i) == Some(Kind::Param) {
            i += 1;
        } else if t.is_name(i) {
            let (parts, _, next) = self.path(i, hi);
            i = next;
            let mut alias = None;
            if t.is_word(i, "AS") && t.is_name(i + 1) && i + 1 < hi {
                alias = self.name_at(i + 1);
                i += 2;
            }
            self.st.blocks[b].rels.push(Rel {
                source: Source::Table(parts),
                alias,
                col_aliases: Vec::new(),
                check: true,
                target: true,
                hidden_target: true,
                dup: false,
            });
        }
        if t.is_word(i, "WITH") && t.is_punct(i + 1, b'(') {
            i = t.close_of(i + 1, hi) + 1;
        }
        if t.is_punct(i, b'(') {
            let c = t.close_of(i, hi);
            if !self.starts_query(i + 1, c) {
                self.target_columns(b, i + 1, c);
                i = c + 1;
            }
        }
        self.walk(b, i, hi, Cl::Head, false);
    }

    /// `(a, b)` naming columns of the DML target.
    fn target_columns(&mut self, b: usize, lo: usize, hi: usize) {
        let t = self.t;
        let mut i = lo;
        let mut item_start = true;
        while i < hi {
            if t.is_punct(i, b'(') {
                i = t.close_of(i, hi) + 1;
                item_start = false;
                continue;
            }
            if t.is_punct(i, b',') {
                item_start = true;
                i += 1;
                continue;
            }
            if item_start && t.is_name(i) {
                let (parts, tail, next) = self.path(i, hi);
                if tail == Tail::Name && !parts.is_empty() {
                    self.st.cols.push(ColRef {
                        parts,
                        block: b,
                        ctx: RefCtx::TargetOnly,
                        hidden: true,
                    });
                }
                i = next.max(i + 1);
                item_start = false;
                continue;
            }
            item_start = false;
            i += 1;
        }
    }

    /// The end of the `SELECT` of `INSERT ... SELECT`: before `ON CONFLICT`, `ON DUPLICATE
    /// KEY UPDATE` or `RETURNING`.
    fn insert_select_end(&self, i: usize, hi: usize) -> usize {
        let t = self.t;
        let mut j = i;
        while j < hi {
            if t.is_punct(j, b'(') {
                j = t.close_of(j, hi) + 1;
                continue;
            }
            if (t.is_word(j, "ON") && t.word_in(j + 1, &["CONFLICT", "DUPLICATE"]))
                || t.is_word(j, "RETURNING")
            {
                return j;
            }
            j += 1;
        }
        hi
    }

    fn update(&mut self, b: usize, kw: usize, hi: usize) {
        self.st.head = Head::Update;
        let i = self.dml_modifiers(b, kw + 1, hi);
        self.walk(b, i, hi, Cl::From, true);
        self.fix_targets(b);
        self.missing_where(b, kw, "UPDATE", "changes");
    }

    fn delete(&mut self, b: usize, kw: usize, hi: usize) {
        let t = self.t;
        self.st.head = Head::Delete;
        let mut i = self.dml_modifiers(b, kw + 1, hi);
        if t.is_word(i, "FROM") {
            self.walk(b, i + 1, hi, Cl::From, true);
        } else {
            // `DELETE a, b FROM ...` (MySQL) / `DELETE x FROM t x` (T-SQL) name relations of the
            // FROM list; T-SQL `DELETE t WHERE ...` names the target.
            let mut names = Vec::new();
            while i < hi && t.is_name(i) && !t.word_in(i, &["FROM", "OUTPUT", "WHERE"]) {
                let (parts, tail, next) = self.path(i, hi);
                if tail == Tail::Name && !parts.is_empty() {
                    names.push(parts);
                }
                i = next.max(i + 1);
                if t.is_punct(i, b',') {
                    i += 1;
                } else {
                    break;
                }
            }
            if !t.is_word(i, "FROM") {
                for parts in names {
                    self.st.blocks[b].rels.push(Rel {
                        source: Source::Table(parts),
                        alias: None,
                        col_aliases: Vec::new(),
                        check: true,
                        target: true,
                        hidden_target: false,
                        dup: false,
                    });
                }
            }
            self.walk(b, i, hi, Cl::Head, false);
        }
        self.fix_targets(b);
        self.missing_where(b, kw, "DELETE", "removes");
    }

    fn missing_where(&mut self, b: usize, kw: usize, verb: &str, does: &str) {
        let block = &self.st.blocks[b];
        if block.has_where || block.joined || block.rels.iter().all(|r| !r.target) {
            return;
        }
        let start = self.t.range(kw).start;
        let end = block
            .rels
            .iter()
            .find(|r| r.target)
            .and_then(|r| match &r.source {
                Source::Table(p) => p.last().map(|n| n.range.end),
                _ => None,
            })
            .unwrap_or(self.t.range(kw).end)
            .max(start);
        self.st.findings.push(Diagnostic::new(
            DiagnosticCode::MissingWhere,
            start..end,
            Severity::Warning,
            format!("`{verb}` without `WHERE` {does} every row of the table."),
        ));
    }

    /// `UPDATE x ... FROM t x` / `DELETE FROM t ... USING t`: a target that names a relation
    /// of the FROM list again stands for that relation.
    fn fix_targets(&mut self, b: usize) {
        let block = &self.st.blocks[b];
        let mut marks: Vec<(usize, bool)> = Vec::new();
        for (k, rel) in block.rels.iter().enumerate() {
            if !rel.target || rel.alias.is_some() {
                continue;
            }
            let Some(name) = rel.table_last() else {
                continue;
            };
            let mut alias_hit = false;
            let mut same = false;
            for (m, other) in block.rels.iter().enumerate() {
                if m == k || other.target {
                    continue;
                }
                match &other.alias {
                    Some(a) if a.text.eq_ignore_ascii_case(&name.text) => alias_hit = true,
                    None if other
                        .table_last()
                        .is_some_and(|l| l.text.eq_ignore_ascii_case(&name.text)) =>
                    {
                        same = true;
                    }
                    _ => {}
                }
            }
            if alias_hit || same {
                marks.push((k, alias_hit));
            }
        }
        for (k, alias_hit) in marks {
            let rel = &mut self.st.blocks[b].rels[k];
            rel.dup = true;
            if alias_hit {
                rel.check = false;
            }
        }
    }

    fn merge(&mut self, b: usize, i: usize, hi: usize) {
        let t = self.t;
        self.st.head = Head::Merge;
        self.st.uncertain = true;
        let mut i = self.dml_modifiers(b, i, hi);
        if t.is_name(i) {
            let (parts, _, next) = self.path(i, hi);
            i = self.skip_table_options(next, hi);
            let mut alias = None;
            if t.is_word(i, "AS") && t.is_name(i + 1) {
                alias = self.name_at(i + 1);
                i += 2;
            } else if self.can_alias(i) {
                alias = self.name_at(i);
                i += 1;
            }
            self.st.blocks[b].rels.push(Rel {
                source: Source::Table(parts),
                alias,
                col_aliases: Vec::new(),
                check: true,
                target: true,
                hidden_target: false,
                dup: false,
            });
        }
        self.walk(b, i, hi, Cl::Head, false);
    }

    fn create(&mut self, i: usize, hi: usize) {
        let t = self.t;
        self.st.head = Head::Ddl;
        let mut j = i;
        loop {
            if j >= hi {
                return;
            }
            if t.word_in(j, words::CREATE_MODS) {
                j += 1;
            } else if t.is_word(j, "OR") && t.word_in(j + 1, &["ALTER", "REPLACE"]) {
                j += 2;
            } else if t.word_in(j, &["ALGORITHM", "DEFINER", "SQL"]) {
                // `ALGORITHM = x`, `DEFINER = user@host`, `SQL SECURITY x`
                j += 1;
                let mut guard = 0;
                while j < hi && guard < 8 && !t.word_in(j, words::CREATE_OBJECTS) {
                    j += 1;
                    guard += 1;
                }
            } else {
                break;
            }
        }
        let obj = t.upper(j);
        j += 1;
        if t.is_word(j, "IF") && t.is_word(j + 1, "NOT") && t.is_word(j + 2, "EXISTS") {
            j += 3;
        }
        match obj.as_str() {
            "TABLE" => {
                let (_, _, mut k) = self.path(j, hi);
                if t.is_punct(k, b'(') {
                    let c = t.close_of(k, hi);
                    self.ddl_references(k + 1, c);
                    k = c + 1;
                }
                while k < hi {
                    if t.is_word(k, "LIKE") {
                        let (parts, _, _) = self.path(k + 1, hi);
                        if !parts.is_empty() {
                            self.st.targets.push(parts);
                        }
                        return;
                    }
                    if t.is_word(k, "AS") {
                        k += 1;
                        continue;
                    }
                    if self.starts_query(k, hi) {
                        self.query(k, hi, None);
                        return;
                    }
                    k += 1;
                }
            }
            "VIEW" => {
                let mut k = j;
                while k < hi && !t.is_word(k, "AS") {
                    if t.is_punct(k, b'(') {
                        k = t.close_of(k, hi);
                    }
                    k += 1;
                }
                if k + 1 < hi {
                    self.query(k + 1, hi, None);
                }
            }
            "INDEX" | "TRIGGER" => {
                let mut k = j;
                while k < hi && !t.is_word(k, "ON") {
                    if t.is_punct(k, b'(') {
                        k = t.close_of(k, hi);
                    }
                    k += 1;
                }
                k += 1;
                if t.is_word(k, "ONLY") {
                    k += 1;
                }
                let (parts, tail, _) = self.path(k, hi);
                if tail == Tail::Name && !parts.is_empty() {
                    self.st.targets.push(parts);
                }
            }
            _ => {}
        }
    }

    /// `REFERENCES t` inside DDL.
    fn ddl_references(&mut self, lo: usize, hi: usize) {
        let t = self.t;
        for k in lo..hi {
            if t.is_word(k, "REFERENCES") {
                let (parts, tail, _) = self.path(k + 1, hi);
                if tail == Tail::Name && !parts.is_empty() {
                    self.st.targets.push(parts);
                }
            }
        }
    }

    fn drop_table(&mut self, kw: usize, hi: usize) {
        let t = self.t;
        self.st.head = Head::Ddl;
        let mut j = kw + 1;
        if t.word_in(j, &["FOREIGN", "MATERIALIZED", "TEMPORARY"]) {
            j += 1;
        }
        let obj = t.upper(j);
        let obj_end = t.range(j).end;
        if !matches!(obj.as_str(), "TABLE" | "VIEW") {
            return;
        }
        j += 1;
        if t.is_word(j, "IF") && t.is_word(j + 1, "EXISTS") {
            return;
        }
        let what = obj.to_ascii_lowercase();
        self.finding(
            DiagnosticCode::MissingIfExists,
            t.range(kw).start..obj_end,
            Severity::Hint,
            format!("`DROP {obj}` fails when the {what} does not exist; `IF EXISTS` makes the script safe to run again."),
        );
        let before = self.st.targets.len();
        self.target_list(j, hi);
        let dropped = self.st.targets.split_off(before);
        self.st.dropped.extend(dropped);
    }

    /// A comma list of relation names (DDL targets).
    fn target_list(&mut self, mut j: usize, hi: usize) {
        let t = self.t;
        while j < hi && t.is_name(j) {
            let (parts, tail, next) = self.path(j, hi);
            if tail == Tail::Name && !parts.is_empty() {
                self.st.targets.push(parts);
            }
            j = next.max(j + 1);
            if t.is_punct(j, b',') {
                j += 1;
            } else {
                break;
            }
        }
    }

    fn alter(&mut self, i: usize, hi: usize) {
        let t = self.t;
        self.st.head = Head::Ddl;
        if t.is_word(i, "VIEW") {
            let mut k = i + 1;
            while k < hi && !t.is_word(k, "AS") {
                k += 1;
            }
            if k + 1 < hi {
                self.query(k + 1, hi, None);
            }
            return;
        }
        if !t.is_word(i, "TABLE") {
            return;
        }
        let mut j = i + 1;
        let if_exists = t.is_word(j, "IF") && t.is_word(j + 1, "EXISTS");
        if if_exists {
            j += 2;
        }
        if t.is_word(j, "ONLY") {
            j += 1;
        }
        let (parts, tail, next) = self.path(j, hi);
        if !if_exists && tail == Tail::Name && !parts.is_empty() {
            self.st.targets.push(parts);
        }
        self.ddl_references(next, hi);
    }

    fn truncate(&mut self, i: usize, hi: usize) {
        let t = self.t;
        self.st.head = Head::Ddl;
        let mut j = i;
        if t.is_word(j, "TABLE") {
            j += 1;
        }
        if t.is_word(j, "ONLY") {
            j += 1;
        }
        if t.is_word(j, "IF") && t.is_word(j + 1, "EXISTS") {
            return;
        }
        self.target_list(j, hi);
    }

    // --- query blocks ---

    /// Reads `lo..hi` as clauses of block `b`, starting in clause `start`.
    fn walk(&mut self, b: usize, lo: usize, hi: usize, start: Cl, targets: bool) {
        if self.depth >= MAX_DEPTH {
            self.st.uncertain = true;
            return;
        }
        self.depth += 1;
        let t = self.t;
        let mut w = Walk {
            b,
            cl: start,
            expect_table: start == Cl::From,
            targets,
            ex: Ex::new(RefCtx::Normal, self.hidden_of(start)),
            rank: 0,
            rank_name: "",
            saw_select: false,
            pending: None,
        };
        w.ex.no_refs = start == Cl::Head;
        let mut i = lo;
        while i < hi {
            let Some(tok) = t.all.get(i).copied() else {
                break;
            };
            let next = match tok.kind {
                Kind::Word => {
                    if let Some(next) = self.clause_word(&mut w, i, hi) {
                        next
                    } else {
                        w.pending = None;
                        if w.expect_table {
                            self.table_ref(&mut w, i, hi)
                        } else {
                            self.atom(w.b, i, hi, &mut w.ex)
                        }
                    }
                }
                Kind::Quoted | Kind::Param if w.expect_table => {
                    w.pending = None;
                    self.table_ref(&mut w, i, hi)
                }
                Kind::Punct(b'(') => {
                    w.pending = None;
                    let c = t.close_of(i, hi);
                    if w.expect_table {
                        self.paren_table(&mut w, i, hi)
                    } else if self.starts_query(i + 1, c) {
                        self.query(i + 1, c, Some(w.b));
                        if w.cl == Cl::Head {
                            self.st.blocks[w.b].star = true;
                        }
                        Self::operand(&mut w.ex, None);
                        c + 1
                    } else {
                        self.atom(w.b, i, hi, &mut w.ex)
                    }
                }
                Kind::Punct(b',') => {
                    self.walk_comma(&mut w, i, hi);
                    i + 1
                }
                Kind::Punct(b';') => i + 1,
                _ => {
                    w.pending = None;
                    self.atom(w.b, i, hi, &mut w.ex)
                }
            };
            i = next.max(i + 1);
        }
        if w.cl == Cl::Select {
            self.finish_item(&mut w);
        }
        if let Some(p) = w.pending.take() {
            self.incomplete(p);
        }
        self.depth -= 1;
    }

    /// Whether names read in clause `cl` may see an `INSERT` target.
    fn hidden_of(&self, cl: Cl) -> bool {
        !(self.st.head == Head::Insert && matches!(cl, Cl::Head | Cl::Values))
    }

    fn walk_comma(&mut self, w: &mut Walk, i: usize, hi: usize) {
        let t = self.t;
        let dml = matches!(
            self.st.head,
            Head::Query | Head::Insert | Head::Update | Head::Delete
        );
        if dml {
            if t.is_punct(i + 1, b',') && i + 1 < hi {
                self.finding(
                    DiagnosticCode::SyntaxError,
                    t.range(i).start..t.range(i + 1).end,
                    Severity::Warning,
                    "Two commas in a row.".to_owned(),
                );
            } else if i + 1 < hi
                && t.kind(i + 1) == Some(Kind::Word)
                && has(words::CLAUSE_AFTER_COMMA, &t.upper(i + 1))
            {
                let kw = t.upper(i + 1);
                self.finding(
                    DiagnosticCode::SyntaxError,
                    t.range(i),
                    Severity::Warning,
                    format!("A comma right before `{kw}`: a list item is missing."),
                );
            } else if i + 1 == hi {
                let closing = t.is_punct(hi, b')');
                self.finding(
                    DiagnosticCode::SyntaxError,
                    t.range(i),
                    if closing {
                        Severity::Warning
                    } else {
                        Severity::Hint
                    },
                    "The list ends with a comma: an item is missing.".to_owned(),
                );
            }
        }
        if w.cl == Cl::Select {
            self.finish_item(w);
        }
        if matches!(w.cl, Cl::From | Cl::On) {
            w.cl = Cl::From;
            w.expect_table = true;
            w.ex = Ex::new(RefCtx::Normal, self.hidden_of(Cl::From));
        } else {
            w.ex.after = false;
            w.ex.item_start = true;
            w.ex.after_for = false;
        }
    }

    /// Ends one select-list item: records its output name.
    fn finish_item(&mut self, w: &mut Walk) {
        let ex = &mut w.ex;
        if !ex.select {
            return;
        }
        if !ex.star && (ex.atoms > 0 || ex.alias.is_some()) {
            let name = ex
                .alias
                .take()
                .or_else(|| if ex.atoms == 1 { ex.chain.take() } else { None });
            self.st.blocks[w.b].outputs.push(name);
        }
        ex.atoms = 0;
        ex.chain = None;
        ex.alias = None;
        ex.star = false;
        ex.item_start = true;
        ex.after = false;
    }

    /// Switches block `w` to clause `cl` (keyword `name` at `kw`).
    fn enter(&mut self, w: &mut Walk, cl: Cl, kw: Range<usize>, name: &'static str) {
        if w.cl == Cl::Select {
            self.finish_item(w);
        }
        if let Some(p) = w.pending.take() {
            self.incomplete(p);
        }
        if w.saw_select
            && let Some(r) = rank(cl)
        {
            if r < w.rank {
                let prev = w.rank_name;
                self.finding(
                    DiagnosticCode::SyntaxError,
                    kw.clone(),
                    Severity::Warning,
                    format!("`{name}` cannot come after `{prev}`."),
                );
            } else {
                w.rank = r;
                w.rank_name = name;
            }
        }
        w.cl = cl;
        w.expect_table = false;
        w.ex = Ex::new(
            match cl {
                Cl::GroupBy | Cl::Having | Cl::OrderBy => RefCtx::AliasOk,
                _ => RefCtx::Normal,
            },
            self.hidden_of(cl),
        );
        w.ex.select = cl == Cl::Select;
        w.ex.set_list = cl == Cl::Set;
        w.ex.no_refs = matches!(cl, Cl::Limit | Cl::Skip | Cl::Into | Cl::Head);
        w.ex.no_bare = cl == Cl::Window;
        w.pending = needs(cl).map(|what| (kw, name, what));
    }

    /// Handles a clause keyword at `i`; `None` when the word is not one here.
    #[allow(clippy::too_many_lines)]
    fn clause_word(&mut self, w: &mut Walk, i: usize, hi: usize) -> Option<usize> {
        let t = self.t;
        let up = t.upper(i);
        let r = t.range(i);
        let head = self.st.head;
        let next_paren = t.is_punct(i + 1, b'(');
        // `INSERT ... SELECT` / `INSERT ... VALUES` of the statement's own block (block 0).
        if head == Head::Insert
            && w.b == 0
            && matches!(w.cl, Cl::Head | Cl::Values)
            && self.starts_query(i, hi)
        {
            let end = self.insert_select_end(i, hi);
            self.query(i, end, Some(w.b));
            w.pending = None;
            return Some(end);
        }
        match up.as_str() {
            "SELECT" => {
                if w.saw_select {
                    self.st.uncertain = true;
                }
                self.enter(w, Cl::Select, r, "SELECT");
                w.saw_select = true;
                Some(self.select_modifiers(w, i + 1, hi))
            }
            "FROM" if !t.is_word(i.wrapping_sub(1), "DISTINCT") => {
                self.enter(w, Cl::From, r, "FROM");
                w.expect_table = true;
                w.targets = false;
                Some(i + 1)
            }
            "JOIN" | "STRAIGHT_JOIN" | "APPLY" if !next_paren => {
                self.enter(w, Cl::From, r, "JOIN");
                w.expect_table = true;
                Some(i + 1)
            }
            _ if has(words::JOIN_MODS, &up) && matches!(w.cl, Cl::From | Cl::On) && !next_paren => {
                if up == "NATURAL" {
                    self.st.blocks[w.b].merged = true;
                }
                Some(i + 1)
            }
            "ON" => {
                if t.is_word(i + 1, "CONFLICT") {
                    return Some(self.on_conflict(w, i + 2, hi));
                }
                if t.is_word(i + 1, "DUPLICATE") {
                    let mut j = i + 2;
                    while j < hi && t.word_in(j, &["KEY", "UPDATE"]) {
                        j += 1;
                    }
                    self.enter(w, Cl::Set, r, "ON DUPLICATE KEY UPDATE");
                    return Some(j);
                }
                if w.cl == Cl::From && !w.expect_table {
                    self.enter(w, Cl::On, r, "ON");
                    self.st.blocks[w.b].joined = true;
                    return Some(i + 1);
                }
                None
            }
            "USING" => {
                if w.cl == Cl::From && next_paren && !w.expect_table {
                    let blk = &mut self.st.blocks[w.b];
                    blk.merged = true;
                    blk.joined = true;
                    w.ex.after = true;
                    return Some(t.close_of(i + 1, hi) + 1);
                }
                if matches!(head, Head::Delete | Head::Merge) {
                    self.enter(w, Cl::From, r, "USING");
                    w.expect_table = true;
                    w.targets = false;
                    return Some(i + 1);
                }
                None
            }
            "WHERE" => {
                self.enter(w, Cl::Where, r, "WHERE");
                self.st.blocks[w.b].has_where = true;
                Some(i + 1)
            }
            "GROUP" if t.is_word(i + 1, "BY") => {
                self.enter(w, Cl::GroupBy, r.start..t.range(i + 1).end, "GROUP BY");
                Some(i + 2)
            }
            "HAVING" => {
                self.enter(w, Cl::Having, r, "HAVING");
                Some(i + 1)
            }
            "WINDOW" if t.is_name(i + 1) && t.is_word(i + 2, "AS") => {
                self.enter(w, Cl::Window, r, "WINDOW");
                Some(i + 1)
            }
            "ORDER" if t.is_word(i + 1, "BY") => {
                self.enter(w, Cl::OrderBy, r.start..t.range(i + 1).end, "ORDER BY");
                Some(i + 2)
            }
            "LIMIT" => {
                let count = matches!(t.kind(i + 1), Some(Kind::Number | Kind::Param));
                if !self.feat.limit {
                    if !count || w.cl == Cl::Head {
                        return None;
                    }
                    self.mismatch(
                        r.clone(),
                        Severity::Warning,
                        format!(
                            "`LIMIT` is not {} syntax; use `TOP (n)` or `OFFSET ... FETCH`.",
                            self.feat.name
                        ),
                    );
                } else if !self.feat.limit_comma && count && t.is_punct(i + 2, b',') {
                    self.mismatch(
                        r.start..t.range(i + 2).end,
                        Severity::Warning,
                        format!(
                            "{} does not accept `LIMIT offset, count`; write `LIMIT count OFFSET offset`.",
                            self.feat.name
                        ),
                    );
                }
                self.enter(w, Cl::Limit, r, "LIMIT");
                Some(i + 1)
            }
            // A bare `offset` may be a column name (not reserved in MySQL, SQLite, T-SQL): as a
            // clause it follows a complete operand, as a name it starts one.
            "OFFSET"
                if w.cl == Cl::Limit
                    || (w.cl != Cl::Head
                        && (matches!(t.kind(i + 1), Some(Kind::Number | Kind::Param))
                            || next_paren
                            || (w.ex.after && self.lang.is_reserved("OFFSET")))) =>
            {
                self.enter(w, Cl::Limit, r, "OFFSET");
                Some(i + 1)
            }
            "FETCH" if w.cl != Cl::Head && t.word_in(i + 1, &["FIRST", "NEXT"]) => {
                self.enter(w, Cl::Limit, r, "OFFSET");
                Some(i + 1)
            }
            "UNION" | "INTERSECT" | "EXCEPT" | "MINUS" => Some(self.set_operation(w, i, hi)),
            "INTO" if !matches!(w.cl, Cl::Head | Cl::Values | Cl::Set) => {
                self.enter(w, Cl::Into, r, "INTO");
                Some(self.select_into(i + 1, hi))
            }
            "RETURNING" | "OUTPUT" if head != Head::Query && w.cl != Cl::Select => {
                if up == "RETURNING" && !self.feat.returning {
                    self.mismatch(
                        r.clone(),
                        Severity::Hint,
                        format!("{} has no `RETURNING` clause.", self.feat.name),
                    );
                }
                self.enter(w, Cl::Returning, r, "RETURNING");
                Some(i + 1)
            }
            "SET" if matches!(head, Head::Update | Head::Insert | Head::Merge) => {
                w.targets = false;
                self.enter(w, Cl::Set, r, "SET");
                Some(i + 1)
            }
            "VALUES" if !(w.cl == Cl::Set && next_paren) => {
                if head == Head::Query && w.cl == Cl::Head {
                    self.st.blocks[w.b].star = true;
                }
                self.enter(w, Cl::Values, r, "VALUES");
                Some(i + 1)
            }
            "DEFAULT" if t.is_word(i + 1, "VALUES") => Some(i + 2),
            "FOR" if t.word_in(i + 1, words::FOR_CLAUSE) => {
                self.enter(w, Cl::Skip, r, "FOR");
                Some(i + 1)
            }
            "LOCK" if t.is_word(i + 1, "IN") => {
                self.enter(w, Cl::Skip, r, "LOCK");
                Some(i + 1)
            }
            "OPTION" if next_paren => Some(t.close_of(i + 1, hi) + 1),
            "WHEN" if head == Head::Merge && w.ex.case_depth == 0 => {
                self.enter(w, Cl::On, r, "WHEN");
                w.pending = None;
                Some(i + 1)
            }
            "THEN" if head == Head::Merge && w.ex.case_depth == 0 => {
                let mut j = i + 1;
                if t.is_word(j, "INSERT") {
                    j += 1;
                    if t.is_punct(j, b'(') {
                        j = t.close_of(j, hi) + 1;
                    }
                    self.enter(w, Cl::Values, r, "INSERT");
                } else if t.word_in(j, &["DELETE", "UPDATE"]) {
                    j += 1;
                    self.enter(w, Cl::Skip, r, "THEN");
                }
                Some(j)
            }
            "PIVOT" | "UNPIVOT" if next_paren && w.cl == Cl::From => {
                self.st.uncertain = true;
                let c = t.close_of(i + 1, hi);
                Some(self.relation_tail(w, Source::Unknown, c + 1, hi))
            }
            _ => None,
        }
    }

    fn select_modifiers(&mut self, w: &mut Walk, mut i: usize, hi: usize) -> usize {
        let t = self.t;
        loop {
            if i >= hi {
                return i;
            }
            if t.is_word(i, "DISTINCT") && t.is_word(i + 1, "ON") && t.is_punct(i + 2, b'(') {
                let c = t.close_of(i + 2, hi);
                let ex = w.ex.group();
                self.group(w.b, i + 3, c, ex);
                i = c + 1;
            } else if t.word_in(i, words::SELECT_MODS) {
                i += 1;
            } else if t.is_word(i, "TOP") {
                match self.top(w.b, i, hi) {
                    Some(j) => i = j,
                    None => return i,
                }
            } else {
                return i;
            }
        }
    }

    fn set_operation(&mut self, w: &mut Walk, i: usize, hi: usize) -> usize {
        let t = self.t;
        if w.cl == Cl::Select {
            self.finish_item(w);
        }
        if let Some(p) = w.pending.take() {
            self.incomplete(p);
        }
        let root = self.st.blocks[w.b].set_root.unwrap_or(w.b);
        let sibling = self.new_block(Some(w.b), i, hi);
        self.st.blocks[sibling].set_root = Some(root);
        w.b = sibling;
        w.cl = Cl::Head;
        w.rank = 0;
        w.rank_name = "";
        w.saw_select = false;
        w.expect_table = false;
        w.targets = false;
        w.ex = Ex::new(RefCtx::Normal, true);
        w.ex.no_refs = true;
        let mut j = i + 1;
        if t.word_in(j, &["ALL", "DISTINCT"]) {
            j += 1;
        }
        j
    }

    /// The table of `SELECT ... INTO t` (created by the statement).
    fn select_into(&mut self, i: usize, hi: usize) -> usize {
        let t = self.t;
        let mut j = i;
        while t.word_in(j, &["TABLE", "TEMP", "TEMPORARY", "UNLOGGED"]) {
            j += 1;
        }
        if t.is_name(j) && j < hi {
            let (parts, _, next) = self.path(j, hi);
            if !parts.is_empty() {
                self.st.created.push(parts);
            }
            return next.max(j + 1);
        }
        j
    }

    /// After `ON CONFLICT`: the conflict target, `ON CONSTRAINT name`.
    fn on_conflict(&mut self, w: &mut Walk, i: usize, hi: usize) -> usize {
        let t = self.t;
        if w.cl == Cl::Select {
            self.finish_item(w);
        }
        w.cl = Cl::Skip;
        w.ex = Ex::new(RefCtx::Normal, true);
        w.ex.no_refs = true;
        let mut j = i;
        if t.is_punct(j, b'(') {
            let c = t.close_of(j, hi);
            self.target_columns(w.b, j + 1, c);
            j = c + 1;
        }
        if t.is_word(j, "ON") && t.is_word(j + 1, "CONSTRAINT") {
            j += 3;
        }
        j
    }

    // --- relations ---

    fn table_ref(&mut self, w: &mut Walk, mut i: usize, hi: usize) -> usize {
        let t = self.t;
        while t.word_in(i, &["LATERAL", "ONLY"]) {
            i += 1;
        }
        if t.is_punct(i, b'(') {
            return self.paren_table(w, i, hi);
        }
        if t.kind(i) == Some(Kind::Param) {
            return self.relation_tail(w, Source::Unknown, i + 1, hi);
        }
        let (parts, tail, mut next) = self.path(i, hi);
        if parts.is_empty() {
            w.expect_table = false;
            return i + 1;
        }
        if tail == Tail::Name && t.is_punct(next, b'(') && next < hi {
            let c = t.close_of(next, hi);
            let ex = w.ex.clone();
            self.call(w.b, parts.clone(), next, c, &ex);
            return self.relation_tail(w, Source::Function(parts), c + 1, hi);
        }
        if tail != Tail::Name {
            w.expect_table = false;
            return next;
        }
        if t.is_punct(next, b'*') {
            next += 1;
        }
        self.relation_tail(w, Source::Table(parts), next, hi)
    }

    fn paren_table(&mut self, w: &mut Walk, i: usize, hi: usize) -> usize {
        let t = self.t;
        let c = t.close_of(i, hi);
        if self.starts_query(i + 1, c) {
            let child = self.query(i + 1, c, Some(w.b));
            return self.relation_tail(w, Source::Derived(child), c + 1, hi);
        }
        // `(a JOIN b ON ...)`: its relations join the enclosing FROM list.
        self.walk(w.b, i + 1, c, Cl::From, w.targets);
        let mut j = c + 1;
        if t.is_word(j, "AS") && t.is_name(j + 1) {
            j += 2;
        }
        w.expect_table = false;
        w.ex.after = true;
        j
    }

    /// Reads the alias (and column aliases, table hints) after a relation and records it.
    fn relation_tail(&mut self, w: &mut Walk, source: Source, i: usize, hi: usize) -> usize {
        let t = self.t;
        let mut i = self.skip_table_options(i, hi);
        let mut alias = None;
        if i + 1 < hi && t.is_word(i, "AS") && t.is_name(i + 1) {
            alias = self.name_at(i + 1);
            i += 2;
        } else if i < hi && self.can_alias(i) {
            alias = self.name_at(i);
            i += 1;
        }
        let mut col_aliases = Vec::new();
        if alias.is_some() && t.is_punct(i, b'(') && i < hi {
            let c = t.close_of(i, hi);
            col_aliases = self.name_list(i + 1, c);
            i = c + 1;
        }
        let i = self.skip_table_options(i, hi);
        let check = matches!(source, Source::Table(_));
        self.st.blocks[w.b].rels.push(Rel {
            source,
            alias,
            col_aliases,
            check,
            target: w.targets,
            hidden_target: false,
            dup: false,
        });
        w.expect_table = false;
        w.ex.after = true;
        i
    }

    /// Table hints and options after a relation name or alias.
    fn skip_table_options(&self, mut i: usize, hi: usize) -> usize {
        let t = self.t;
        loop {
            if i >= hi {
                return i;
            }
            if t.is_word(i, "WITH") && t.is_punct(i + 1, b'(') {
                i = t.close_of(i + 1, hi) + 1;
            } else if t.is_word(i, "WITH") && t.is_word(i + 1, "ORDINALITY") {
                i += 2;
            } else if t.is_word(i, "TABLESAMPLE") {
                i += 1;
                if t.kind(i) == Some(Kind::Word) {
                    i += 1;
                }
                if t.is_punct(i, b'(') {
                    i = t.close_of(i, hi) + 1;
                }
                if t.is_word(i, "REPEATABLE") && t.is_punct(i + 1, b'(') {
                    i = t.close_of(i + 1, hi) + 1;
                }
            } else if t.word_in(i, &["FORCE", "IGNORE", "USE"])
                && t.word_in(i + 1, &["INDEX", "KEY"])
            {
                let mut j = i + 2;
                while j < hi && j < i + 8 && !t.is_punct(j, b'(') {
                    j += 1;
                }
                i = if t.is_punct(j, b'(') {
                    t.close_of(j, hi) + 1
                } else {
                    j
                };
            } else if t.is_word(i, "INDEXED") && t.is_word(i + 1, "BY") {
                i += 3;
            } else if t.is_word(i, "NOT") && t.is_word(i + 1, "INDEXED") {
                i += 2;
            } else if t.is_word(i, "PARTITION") && t.is_punct(i + 1, b'(') {
                i = t.close_of(i + 1, hi) + 1;
            } else {
                return i;
            }
        }
    }

    // --- expressions ---

    /// Reads `lo..hi` (inside parentheses) as an expression list.
    fn group(&mut self, b: usize, lo: usize, hi: usize, mut ex: Ex) {
        if self.depth >= MAX_DEPTH {
            self.st.uncertain = true;
            return;
        }
        self.depth += 1;
        let t = self.t;
        let dml = matches!(
            self.st.head,
            Head::Query | Head::Insert | Head::Update | Head::Delete
        );
        let mut i = lo;
        while i < hi {
            if t.is_punct(i, b',') {
                if dml && (t.is_punct(i + 1, b',') || (i + 1 == hi && t.is_punct(hi, b')'))) {
                    let end = t.range(i + 1).end;
                    self.finding(
                        DiagnosticCode::SyntaxError,
                        t.range(i).start..end,
                        Severity::Warning,
                        "A list item is missing between these commas or before `)`.".to_owned(),
                    );
                }
                ex.after = false;
                ex.item_start = true;
                ex.after_for = false;
                i += 1;
                continue;
            }
            i = self.atom(b, i, hi, &mut ex).max(i + 1);
        }
        self.depth -= 1;
    }

    fn operand(ex: &mut Ex, chain: Option<Name>) {
        ex.atoms += 1;
        ex.chain = if ex.atoms == 1 { chain } else { None };
        ex.after = true;
        ex.item_start = false;
        ex.after_for = false;
    }

    /// One token of an expression; returns the index after what it consumed.
    fn atom(&mut self, b: usize, i: usize, hi: usize, ex: &mut Ex) -> usize {
        let t = self.t;
        let Some(tok) = t.all.get(i).copied() else {
            return i + 1;
        };
        match tok.kind {
            Kind::Number | Kind::Param | Kind::Str => {
                Self::operand(ex, None);
                i + 1
            }
            Kind::Punct(b'(') => {
                let c = t.close_of(i, hi);
                if self.starts_query(i + 1, c) {
                    self.query(i + 1, c, Some(b));
                } else {
                    let g = ex.group();
                    self.group(b, i + 1, c, g);
                }
                Self::operand(ex, None);
                c + 1
            }
            Kind::Punct(b'*') => {
                if ex.select && ex.item_start {
                    ex.star = true;
                    self.st.blocks[b].star = true;
                    ex.after = true;
                } else {
                    ex.after = false;
                }
                ex.item_start = false;
                i + 1
            }
            Kind::Punct(b':') if t.is_punct_touching(i + 1, b':') => {
                if !self.feat.double_colon {
                    self.mismatch(
                        tok.start..t.range(i + 1).end,
                        Severity::Warning,
                        format!(
                            "`::` casts are not {} syntax; use `CAST(x AS type)`.",
                            self.feat.name
                        ),
                    );
                }
                ex.after = true;
                ex.item_start = false;
                self.skip_cast_type(i + 2, hi)
            }
            Kind::Punct(b'`') if !self.feat.backticks => {
                let mut k = i + 1;
                while k < hi && k < i + 8 && !t.is_punct(k, b'`') {
                    k += 1;
                }
                let end = if t.is_punct(k, b'`') && k < hi {
                    t.range(k).end
                } else {
                    tok.end
                };
                self.mismatch(
                    tok.start..end,
                    Severity::Warning,
                    format!(
                        "Backticks do not quote names in {}; use double quotes.",
                        self.feat.name
                    ),
                );
                Self::operand(ex, None);
                if end > tok.end { k + 1 } else { i + 1 }
            }
            Kind::Punct(b',') => {
                ex.after = false;
                ex.item_start = true;
                i + 1
            }
            Kind::Punct(_) | Kind::Other => {
                ex.after = false;
                ex.item_start = false;
                i + 1
            }
            Kind::Word | Kind::Quoted => self.name_atom(b, i, hi, ex),
        }
    }

    /// The type after `::` (`int`, `numeric(10, 2)`, `text[]`).
    fn skip_cast_type(&self, i: usize, hi: usize) -> usize {
        let t = self.t;
        let (parts, _, mut j) = self.path(i, hi);
        if parts.is_empty() {
            return i;
        }
        if t.is_punct(j, b'(') && j < hi {
            j = t.close_of(j, hi) + 1;
        }
        while j < hi && t.is_punct(j, b'[') {
            j += 1;
            if matches!(t.kind(j), Some(Kind::Number)) {
                j += 1;
            }
            if t.is_punct(j, b']') {
                j += 1;
            }
        }
        j
    }

    /// The rest of a list item after `AS` inside parentheses (`CAST(x AS numeric(10, 2))`).
    fn skip_item(&self, mut i: usize, hi: usize) -> usize {
        let t = self.t;
        while i < hi && !t.is_punct(i, b',') {
            if t.is_punct(i, b'(') {
                i = t.close_of(i, hi);
            }
            i += 1;
        }
        i
    }

    fn name_atom(&mut self, b: usize, i: usize, hi: usize, ex: &mut Ex) -> usize {
        let t = self.t;
        if i.checked_sub(1).is_some_and(|p| t.is_punct(p, b'.')) {
            return self.field_selector(b, i, hi, ex);
        }
        let quoted = t.kind(i) == Some(Kind::Quoted);
        let next_dot = i + 1 < hi && t.is_punct(i + 1, b'.');
        if !quoted && !next_dot {
            let up = t.upper(i);
            if self.is_keyword(&up) {
                return self.keyword(b, i, hi, ex, &up);
            }
            // `DATE '2020-01-01'`, `_utf8mb4 'x'`, `x'41'`
            if i + 1 < hi && t.kind(i + 1) == Some(Kind::Str) && !ex.after {
                Self::operand(ex, None);
                return i + 2;
            }
        }
        let (parts, tail, next) = self.path(i, hi);
        if parts.is_empty() {
            ex.after = false;
            return i + 1;
        }
        if tail == Tail::Star {
            if ex.select && ex.item_start {
                ex.star = true;
                self.st.blocks[b].star = true;
            }
            ex.after = true;
            ex.item_start = false;
            return next;
        }
        let paren = next < hi && t.is_punct(next, b'(');
        if ex.after && parts.len() == 1 {
            // A bare name right after an operand: an alias, a unit (`INTERVAL 1 DAY`), a type
            // word (`character varying(10)`), an operator class.
            if ex.select && ex.alias.is_none() && !paren {
                ex.alias = parts.into_iter().next();
            }
            ex.after = true;
            return if paren {
                t.close_of(next, hi) + 1
            } else {
                next
            };
        }
        if paren && tail == Tail::Name {
            let c = t.close_of(next, hi);
            self.call(b, parts, next, c, ex);
            Self::operand(ex, None);
            return c + 1;
        }
        // `name => value`
        if t.is_punct(next, b'=') && t.is_punct_touching(next + 1, b'>') {
            ex.after = false;
            ex.item_start = false;
            return next + 2;
        }
        // T-SQL `SELECT alias = expr` (MySQL reads a comparison: the name is then unchecked).
        if ex.select && ex.item_start && parts.len() == 1 && t.is_punct(next, b'=') {
            ex.alias = parts.into_iter().next();
            ex.after = false;
            ex.item_start = false;
            return next + 1;
        }
        let set_target = ex.set_list && ex.item_start && t.is_punct(next, b'=');
        let pseudo =
            parts.len() > 1 && has(words::PSEUDO_TABLES, &parts[0].text.to_ascii_uppercase());
        let skip = ex.no_refs
            || tail != Tail::Name
            || ex.after_for
            || pseudo
            || (ex.no_bare && parts.len() == 1);
        let last = parts.last().cloned();
        if !skip {
            self.st.cols.push(ColRef {
                parts,
                block: b,
                ctx: if set_target {
                    RefCtx::TargetOnly
                } else {
                    ex.ctx
                },
                hidden: ex.hidden,
            });
        }
        Self::operand(ex, last);
        next
    }

    /// A name right after a `.` that no name path took: a field of a composite value
    /// (`(addr).city`, `arr[1].f`) or a method (`@x.value(...)`, `(expr).method()`). Neither
    /// is a column reference nor a function of the catalog.
    fn field_selector(&mut self, b: usize, i: usize, hi: usize, ex: &mut Ex) -> usize {
        let t = self.t;
        let (_, _, mut next) = self.path(i, hi);
        next = next.max(i + 1);
        if next < hi && t.is_punct(next, b'(') {
            let c = t.close_of(next, hi);
            let g = ex.group();
            self.group(b, next + 1, c, g);
            next = c + 1;
        }
        ex.chain = None;
        ex.after = true;
        ex.item_start = false;
        ex.after_for = false;
        next
    }

    fn keyword(&mut self, b: usize, i: usize, hi: usize, ex: &mut Ex, up: &str) -> usize {
        let t = self.t;
        let next_paren = i + 1 < hi && t.is_punct(i + 1, b'(');
        if next_paren && (!self.lang.function(up).is_empty() || has(words::TYPE_ARG_FUNCS, up)) {
            // `LEFT(x, 2)`, `REPLACE(...)`, `EXTRACT(YEAR FROM d)`: keywords that are also
            // functions.
            let c = t.close_of(i + 1, hi);
            let name: Vec<Name> = self.name_at(i).into_iter().collect();
            self.call(b, name, i + 1, c, ex);
            Self::operand(ex, None);
            return c + 1;
        }
        match up {
            "AS" => {
                if ex.in_group {
                    ex.after = true;
                    return self.skip_item(i + 1, hi);
                }
                if i + 1 < hi && t.is_name(i + 1) {
                    if ex.select {
                        ex.alias = self.name_at(i + 1);
                    }
                    ex.after = true;
                    return i + 2;
                }
                if i + 1 < hi && t.kind(i + 1) == Some(Kind::Str) {
                    // `AS 'label'`: the output name is not a plain name.
                    ex.alias = None;
                    ex.atoms += 1;
                    ex.after = true;
                    return i + 2;
                }
                ex.after = false;
                i + 1
            }
            "COLLATE" => {
                let (_, _, next) = self.path(i + 1, hi);
                ex.after = true;
                next.max(i + 1)
            }
            "FOR" => {
                ex.after = false;
                ex.after_for = true;
                i + 1
            }
            "OVER" if !next_paren && i + 1 < hi && t.is_name(i + 1) => {
                ex.after = true;
                i + 2
            }
            "CASE" => {
                ex.case_depth += 1;
                ex.after = false;
                ex.item_start = false;
                i + 1
            }
            "ILIKE" if !self.feat.ilike && ex.after => {
                self.mismatch(
                    t.range(i),
                    Severity::Warning,
                    format!(
                        "`ILIKE` is not {} syntax; compare `LOWER(a) LIKE LOWER(b)` instead.",
                        self.feat.name
                    ),
                );
                ex.after = false;
                i + 1
            }
            _ => {
                if up == "END" {
                    ex.case_depth = ex.case_depth.saturating_sub(1);
                }
                if has(words::VALUE_WORDS, up) {
                    Self::operand(ex, None);
                } else {
                    ex.after = false;
                    ex.item_start = false;
                }
                i + 1
            }
        }
    }

    /// Records a call of `name` whose `(` is at `open` and `)` at `close`, and reads its
    /// arguments.
    fn call(&mut self, b: usize, name: Vec<Name>, open: usize, close: usize, parent: &Ex) {
        let t = self.t;
        let up = name
            .last()
            .map(|n| n.text.to_ascii_uppercase())
            .unwrap_or_default();
        let args = self.count_args(open + 1, close);
        let start = name.first().map_or(t.range(open).start, |n| n.range.start);
        let last = if t.is_punct(close, b')') {
            close
        } else {
            close.saturating_sub(1)
        };
        let end = t.range(last).end.max(start);
        self.st.calls.push(Call {
            name,
            args,
            range: start..end,
        });
        let mut ex = parent.group();
        ex.no_bare = has(words::TYPE_ARG_FUNCS, &up);
        self.group(b, open + 1, close, ex);
    }

    fn count_args(&self, lo: usize, hi: usize) -> Option<usize> {
        let t = self.t;
        if lo >= hi {
            return Some(0);
        }
        let mut n = 1;
        let mut i = lo;
        while i < hi {
            match t.kind(i) {
                Some(Kind::Punct(b'(')) => {
                    i = t.close_of(i, hi) + 1;
                    continue;
                }
                Some(Kind::Punct(b',')) => n += 1,
                Some(Kind::Word) if has(words::SYNTAX_ARG_WORDS, &t.upper(i)) => return None,
                _ => {}
            }
            i += 1;
        }
        Some(n)
    }
}
