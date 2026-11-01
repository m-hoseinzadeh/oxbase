//! One statement's tokens prepared for the layout: the significant tokens ("items") with the
//! comments attached to them, touching operator characters merged into one item, and every
//! word classified as keyword or identifier.
//!
//! # Comments
//!
//! The comments between two items `P` and `N` are split in two: the ones on `P`'s line (no line
//! break before them) are `P`'s *trailing* comments, the rest are `N`'s *leading* comments.
//! Each remembers whether a line break came before and after it in the input; the layout keeps
//! those line breaks, which is what makes formatting idempotent (a comment on its own line stays
//! on its own line, a comment at the end of a line stays at the end of that line).
//!
//! # Keywords
//!
//! The formatter re-cases a word only where it reads as a keyword (`docs/m6-contracts.md`
//! §4.6): a reserved word unless it is qualified (`t.order`) or a parameter (`:date`), and a
//! non-reserved keyword unless its neighbours say it names something (`SELECT date, user FROM
//! t`, `CREATE TABLE t (data text)`, `x AS year`). Uncertain cases keep the input's spelling.

use oxbase_sql::{SqlDialect, SqlToken, SqlTokenKind, tokenize};

use crate::SqlLanguage;

/// Punctuation that joins touching neighbours of the same set into one operator item (`<=`,
/// `::`, `->>`, `@@`).
const OPERATOR_CHARS: &str = "=<>!+-*/%|^&~@#?:$";

/// Words the layout reads as keywords even when the dialect's language data does not list them
/// (clause words of one engine that the standard core lacks). They never change case unless the
/// language lists them.
const LAYOUT_WORDS: &[&str] = &[
    "ANALYSE",
    "ANALYZE",
    "APPLY",
    "BROWSE",
    "CATCH",
    "CONFLICT",
    "DESCRIBE",
    "DO",
    "DUPLICATE",
    "ELSEIF",
    "ELSIF",
    "EVENT",
    "EXEC",
    "EXECUTE",
    "GOTO",
    "IGNORE",
    "ILIKE",
    "ITERATE",
    "JSON",
    "LATERAL",
    "LEAVE",
    "LOOP",
    "MATCHED",
    "MATERIALIZED",
    "MERGE",
    "MINUS",
    "NEXT",
    "NOTHING",
    "OPTION",
    "OUTPUT",
    "PACKAGE",
    "PRINT",
    "PROC",
    "REPEAT",
    "RETURNING",
    "SHARE",
    "STRAIGHT_JOIN",
    "TOP",
    "TRAN",
    "TRY",
    "UPSERT",
    "USE",
    "VERBOSE",
    "WHILE",
    "WORK",
    "XML",
];

/// Keywords after which a non-reserved keyword names an object or alias (`FROM data`, `AS
/// year`), unless it is one of [`AFTER_INTRO_KEYWORDS`].
const OBJECT_INTRO: &[&str] = &[
    "AS",
    "COLUMN",
    "CONSTRAINT",
    "DATABASE",
    "DOMAIN",
    "EXTENSION",
    "FROM",
    "FUNCTION",
    "INDEX",
    "INTO",
    "JOIN",
    "PROCEDURE",
    "RECURSIVE",
    "REFERENCES",
    "ROLE",
    "SCHEMA",
    "SEQUENCE",
    "TABLE",
    "TRIGGER",
    "TYPE",
    "UPDATE",
    "VIEW",
    "WITH",
];

/// Non-reserved keywords that still read as keywords right after an [`OBJECT_INTRO`] word.
const AFTER_INTRO_KEYWORDS: &[&str] = &[
    "CONCURRENTLY",
    "IF",
    "IGNORE",
    "LATERAL",
    "MATERIALIZED",
    "ONLY",
    "RECURSIVE",
    "TEMP",
    "TEMPORARY",
    "UNLOGGED",
];

/// Keywords after which an expression (often a bare column name) starts.
const EXPR_INTRO: &[&str] = &[
    "ALL",
    "AND",
    "ANY",
    "BETWEEN",
    "BY",
    "CASE",
    "DISTINCT",
    "ELSE",
    "EXISTS",
    "HAVING",
    "ILIKE",
    "IN",
    "IS",
    "LIKE",
    "NOT",
    "ON",
    "OR",
    "OUTPUT",
    "RETURN",
    "RETURNING",
    "SELECT",
    "SET",
    "SOME",
    "THEN",
    "TOP",
    "USING",
    "VALUES",
    "WHEN",
    "WHERE",
];

/// Words that open a routine definition (its body is kept verbatim).
const ROUTINE_WORDS: &[&str] = &[
    "EVENT",
    "FUNCTION",
    "PACKAGE",
    "PROC",
    "PROCEDURE",
    "TRIGGER",
];

/// Words after `BEGIN` that make it a transaction statement rather than a block.
const BEGIN_TRANSACTION: &[&str] = &[
    "DEFERRED",
    "DISTRIBUTED",
    "EXCLUSIVE",
    "IMMEDIATE",
    "ISOLATION",
    "READ",
    "TRAN",
    "TRANSACTION",
    "WORK",
];

/// Values, not operators: a `-` after one of these is binary.
const VALUE_WORDS: &[&str] = &[
    "CURRENT_DATE",
    "CURRENT_TIME",
    "CURRENT_TIMESTAMP",
    "CURRENT_USER",
    "DEFAULT",
    "END",
    "FALSE",
    "LOCALTIME",
    "LOCALTIMESTAMP",
    "NULL",
    "SESSION_USER",
    "TRUE",
    "USER",
];

/// `true` when `word` equals one of `list` ignoring ASCII case.
pub(super) fn is_any(word: &str, list: &[&str]) -> bool {
    list.iter().any(|w| w.eq_ignore_ascii_case(word))
}

/// A comment kept with an item.
#[derive(Debug, Clone, Copy)]
pub(super) struct Comment<'a> {
    pub text: &'a str,
    /// A line comment (always ends its line).
    pub line: bool,
    /// A line break came right before it in the input.
    pub nl_before: bool,
    /// A line break came right after it in the input.
    pub nl_after: bool,
}

/// One significant token (or a run of touching operator characters).
#[derive(Debug, Clone)]
pub(super) struct Item<'a> {
    pub kind: SqlTokenKind,
    pub text: &'a str,
    /// Byte offset of `text` in the statement.
    pub start: usize,
    /// A run of operator characters (see [`OPERATOR_CHARS`]).
    pub op: bool,
    /// The previous item ended exactly where this one starts.
    pub touch_prev: bool,
    /// Touching the previous item or not changes the meaning (the rule of
    /// `oxbase_sql::tokens_equivalent`), so the input's choice must be kept.
    pub glue_prev: bool,
    /// The whitespace right before this item holds a line break.
    pub nl_before: bool,
    pub leading: Vec<Comment<'a>>,
    pub trailing: Vec<Comment<'a>>,
    /// The layout reads this word as a keyword.
    pub lw: bool,
    /// ... and the language lists it, so its case may change.
    pub kw: bool,
}

impl Item<'_> {
    /// This item is the punctuation `p` (not part of an operator run).
    pub fn is_punct(&self, p: &str) -> bool {
        !self.op && self.kind == SqlTokenKind::Punct && self.text == p
    }

    /// This item is the keyword `w` (any case).
    pub fn is_word(&self, w: &str) -> bool {
        self.lw && self.text.eq_ignore_ascii_case(w)
    }

    /// This item is a keyword of `list`.
    pub fn is_any_word(&self, list: &[&str]) -> bool {
        self.lw && is_any(self.text, list)
    }

    pub fn is_string(&self) -> bool {
        matches!(self.kind, SqlTokenKind::String { .. })
    }
}

/// The items of statement `text`, or `None` when the statement holds an unterminated string,
/// identifier or comment (the formatter keeps such a statement as it is).
pub(super) fn prepare<'a>(
    sql: SqlDialect,
    text: &'a str,
    lang: &SqlLanguage,
) -> Option<Vec<Item<'a>>> {
    struct Raw {
        tok: SqlToken,
        nl_before: bool,
        ws_before: bool,
    }
    let is_comment =
        |k: SqlTokenKind| matches!(k, SqlTokenKind::LineComment | SqlTokenKind::BlockComment);

    let mut raws: Vec<Raw> = Vec::new();
    let (mut nl, mut ws) = (false, false);
    for tok in tokenize(sql, text) {
        if !tok.is_terminated(text) {
            return None;
        }
        if tok.kind == SqlTokenKind::Space {
            ws = true;
            nl |= tok.text(text).contains('\n');
            continue;
        }
        raws.push(Raw {
            tok,
            nl_before: nl,
            ws_before: ws,
        });
        nl = false;
        ws = false;
    }

    let mut items: Vec<Item<'a>> = Vec::new();
    let mut pending: Vec<Comment<'a>> = Vec::new();
    for (idx, raw) in raws.iter().enumerate() {
        let tok = raw.tok;
        if is_comment(tok.kind) {
            pending.push(Comment {
                text: tok.text(text),
                line: tok.kind == SqlTokenKind::LineComment,
                nl_before: raw.nl_before,
                nl_after: raws.get(idx + 1).is_some_and(|next| next.nl_before),
            });
            continue;
        }
        let prev_raw = idx
            .checked_sub(1)
            .map(|at| &raws[at])
            .filter(|prev| !is_comment(prev.tok.kind));
        let touch = prev_raw.is_some() && !raw.ws_before;
        let operator = tok.kind == SqlTokenKind::Punct
            && tok.text(text).chars().all(|ch| OPERATOR_CHARS.contains(ch));
        if touch
            && operator
            && let Some(last) = items.last_mut()
            && last.op
        {
            last.text = &text[last.start..tok.end];
            continue;
        }
        let glue = prev_raw.is_some_and(|prev| glue_matters(text, prev.tok, tok));
        let leading = attach(&mut pending, items.last_mut());
        items.push(Item {
            kind: tok.kind,
            text: tok.text(text),
            start: tok.start,
            op: operator,
            touch_prev: touch,
            glue_prev: glue,
            nl_before: raw.nl_before,
            leading,
            trailing: Vec::new(),
            lw: false,
            kw: false,
        });
    }
    let last = items.last_mut()?;
    last.trailing.append(&mut pending);
    classify(&mut items, lang);
    Some(items)
}

/// Splits the comments seen since the last item: the ones on its line become its trailing
/// comments, the rest are returned as the next item's leading comments.
fn attach<'a>(pending: &mut Vec<Comment<'a>>, last: Option<&mut Item<'a>>) -> Vec<Comment<'a>> {
    let Some(last) = last else {
        return std::mem::take(pending);
    };
    let split = pending
        .iter()
        .position(|c| c.nl_before)
        .unwrap_or(pending.len());
    let leading = pending.split_off(split);
    last.trailing.append(pending);
    leading
}

/// Whether touching or not changes the meaning of `prev` followed by `next`: the same rule as
/// `oxbase_sql::tokens_equivalent` (rule 3), which the formatter must never break.
fn glue_matters(text: &str, prev: SqlToken, next: SqlToken) -> bool {
    use SqlTokenKind::{Punct, QuotedIdent, String as Str, Word};
    let pt = prev.text(text);
    let nt = next.text(text);
    let free = |t: &str| matches!(t, "(" | ")" | "," | ";");
    if pt == "." || nt == "." {
        return true;
    }
    match (prev.kind, next.kind) {
        (Punct, Punct) => !free(pt) && !free(nt),
        (Word, Str { .. } | QuotedIdent { .. }) => true,
        (Punct, Word) => matches!(pt, "$" | "@" | ":" | "?" | "#" | "&"),
        (Word, Punct) => {
            matches!(nt, "+" | "-")
                && pt.as_bytes().first().is_some_and(u8::is_ascii_digit)
                && matches!(pt.as_bytes().last(), Some(b'e' | b'E'))
        }
        _ => false,
    }
}

/// Sets [`Item::lw`] and [`Item::kw`] for every item, in order (a word's class may depend on
/// the class of the word before it).
fn classify(items: &mut [Item<'_>], lang: &SqlLanguage) {
    let standard = SqlLanguage::standard();
    for i in 0..items.len() {
        let (lw, kw) = word_class(items, i, lang, &standard);
        items[i].lw = lw;
        items[i].kw = kw;
    }
}

fn word_class(
    items: &[Item<'_>],
    i: usize,
    lang: &SqlLanguage,
    standard: &SqlLanguage,
) -> (bool, bool) {
    let it = &items[i];
    if it.kind != SqlTokenKind::Word {
        return (false, false);
    }
    let word = it.text;
    let listed = lang.is_keyword(word);
    if !listed && !standard.is_keyword(word) && !is_any(word, LAYOUT_WORDS) {
        return (false, false);
    }
    let prev = i.checked_sub(1).map(|at| &items[at]);
    let next = items.get(i + 1);
    // Qualified (`t.order`, `order.x`) or a parameter / variable (`:date`, `@user`).
    if prev.is_some_and(|it| it.is_punct(".")) || next.is_some_and(|it| it.is_punct(".")) {
        return (false, false);
    }
    if it.glue_prev && it.touch_prev && prev.is_some_and(|it| it.op) {
        return (false, false);
    }
    let reserved = lang.is_reserved(word) || standard.is_reserved(word);
    let lw = reserved || non_reserved_is_keyword(items, i, lang, standard);
    (lw, lw && listed)
}

/// Whether the non-reserved keyword at `i` reads as a keyword (see the module docs).
fn non_reserved_is_keyword(
    items: &[Item<'_>],
    i: usize,
    lang: &SqlLanguage,
    standard: &SqlLanguage,
) -> bool {
    let word = items[i].text;
    let Some(prev) = i.checked_sub(1).map(|at| &items[at]) else {
        return true;
    };
    let next = items.get(i + 1);
    // `IF [NOT] EXISTS name`
    if prev.is_word("EXISTS")
        && i >= 2
        && (items[i - 2].is_word("IF") || items[i - 2].is_word("NOT"))
    {
        return false;
    }
    // `INSERT OR REPLACE`, `INSERT OR IGNORE`, `CREATE OR REPLACE`
    if prev.is_word("OR")
        && i >= 2
        && (items[i - 2].is_word("INSERT") || items[i - 2].is_word("CREATE"))
    {
        return true;
    }
    if prev.is_any_word(OBJECT_INTRO) {
        return is_any(word, AFTER_INTRO_KEYWORDS);
    }
    let list_start = prev.is_punct(",") || prev.is_punct("(");
    let prev_intro = list_start || prev.op || prev.is_any_word(EXPR_INTRO);
    let next_ends = next.is_none_or(|it| {
        it.is_punct(",")
            || it.is_punct(")")
            || it.is_punct(";")
            || it.op
            || (it.kind == SqlTokenKind::Word
                && (lang.is_reserved(it.text) || standard.is_reserved(it.text)))
    });
    if prev_intro && next_ends {
        return false;
    }
    if list_start
        && next.is_some_and(|it| {
            matches!(
                it.kind,
                SqlTokenKind::Word | SqlTokenKind::QuotedIdent { .. }
            ) && !is_any(it.text, &["BY", "KEY"])
        })
    {
        return false;
    }
    true
}

/// `true` when the item at `p` is a prefix operator (`-x`, `NOT`-like `!x`, `~x`): an operator
/// that follows nothing, an opening parenthesis, a comma, another operator or a keyword.
pub(super) fn is_prefix_op(items: &[Item<'_>], p: usize) -> bool {
    let it = &items[p];
    if !it.op || !matches!(it.text, "-" | "+" | "~" | "!") {
        return false;
    }
    let Some(prev) = p.checked_sub(1).map(|q| &items[q]) else {
        return true;
    };
    prev.is_punct("(")
        || prev.is_punct(",")
        || prev.is_punct(";")
        || prev.is_punct("[")
        || prev.op
        || (prev.lw && !is_any(prev.text, VALUE_WORDS))
}

/// Statements whose layout the formatter does not attempt and keeps as typed: routine
/// definitions (`CREATE FUNCTION`, `CREATE PROCEDURE`, `CREATE TRIGGER`, `CREATE EVENT`; their
/// bodies are other languages or compound statements) and procedural blocks (`BEGIN ... END`,
/// `IF`, `WHILE`, `LOOP`, `REPEAT`).
pub(super) fn is_procedural(items: &[Item<'_>]) -> bool {
    if items
        .first()
        .is_some_and(|f| f.is_word("CREATE") || f.is_word("ALTER"))
    {
        for it in items.iter().skip(1).take(24) {
            if it.is_punct("(") {
                break;
            }
            if it.is_any_word(ROUTINE_WORDS) {
                return true;
            }
        }
    }
    (0..items.len()).any(|i| procedural_word(items, i))
}

fn procedural_word(items: &[Item<'_>], i: usize) -> bool {
    let it = &items[i];
    if !it.lw {
        return false;
    }
    let next = items.get(i + 1);
    let paren_at = |at: usize| items.get(at).is_some_and(|x| x.is_punct("("));
    let word_at = |at: usize, word: &str| {
        items
            .get(at)
            .is_some_and(|x| x.text.eq_ignore_ascii_case(word))
    };
    let statement_start = i.checked_sub(1).is_none_or(|at| items[at].is_punct(";"));
    if it.is_word("BEGIN") {
        return !next.is_none_or(|x| x.is_punct(";") || is_any(x.text, BEGIN_TRANSACTION));
    }
    if it.is_any_word(&["WHILE", "ELSIF", "ELSEIF", "LEAVE", "ITERATE", "GOTO"]) {
        return true;
    }
    if it.is_any_word(&["LOOP", "REPEAT"]) {
        return !paren_at(i + 1);
    }
    if it.is_word("IF") {
        if paren_at(i + 1) && !statement_start {
            return false; // the `IF(cond, a, b)` function
        }
        if word_at(i + 1, "EXISTS") {
            return paren_at(i + 2);
        }
        if word_at(i + 1, "NOT") && word_at(i + 2, "EXISTS") {
            return paren_at(i + 3);
        }
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(text: &str) -> Vec<Item<'_>> {
        prepare(SqlDialect::Postgres, text, &SqlLanguage::standard()).unwrap()
    }

    fn texts<'a>(items: &[Item<'a>]) -> Vec<&'a str> {
        items.iter().map(|i| i.text).collect()
    }

    #[test]
    fn operators_merge_and_comments_attach() {
        let its = items("select a<=b, c::int -- one\n/* two */ from t");
        assert_eq!(
            texts(&its),
            ["select", "a", "<=", "b", ",", "c", "::", "int", "from", "t"]
        );
        assert_eq!(its[7].trailing.len(), 1);
        assert_eq!(its[8].leading.len(), 1);
        assert!(its[8].leading[0].nl_before);
        assert!(!its[8].leading[0].nl_after);
        assert!(its[7].glue_prev && its[7].touch_prev, "`::` then a word");
    }

    #[test]
    fn keywords_and_identifiers() {
        let its = items("select date, user, x from t where data = 1 limit 5");
        let kw: Vec<(&str, bool)> = its.iter().map(|i| (i.text, i.lw)).collect();
        assert!(kw.contains(&("select", true)));
        assert!(kw.contains(&("from", true)));
        assert!(kw.contains(&("limit", true)));
        assert!(kw.contains(&("data", false)));
        let its = items("select t.order from t");
        assert!(!its[3].lw);
        let its = items("create table if not exists data (data text)");
        assert!(its[2].lw, "IF after TABLE");
        assert!(!its[5].lw, "table name");
        assert!(!its[7].lw, "column name");
    }

    #[test]
    fn unterminated_text_is_not_prepared() {
        assert!(
            prepare(
                SqlDialect::Postgres,
                "select 'abc",
                &SqlLanguage::standard()
            )
            .is_none()
        );
        assert!(
            prepare(
                SqlDialect::Postgres,
                "select 1 /* x",
                &SqlLanguage::standard()
            )
            .is_none()
        );
    }

    #[test]
    fn procedural_detection() {
        assert!(is_procedural(&items(
            "create function f() returns int as $$ select 1 $$ language sql"
        )));
        assert!(is_procedural(&items("begin select 1; end")));
        assert!(!is_procedural(&items("begin transaction")));
        assert!(!is_procedural(&items("begin")));
        assert!(!is_procedural(&items("drop table if exists t")));
        assert!(!is_procedural(&items("select if(a, 1, 2) from t")));
        assert!(is_procedural(&items("if exists (select 1) select 2")));
        assert!(is_procedural(&items("while 1 = 1 select 1")));
    }
}
