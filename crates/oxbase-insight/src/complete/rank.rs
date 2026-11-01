//! Matching, ranking and the item list of one completion request.

use std::collections::HashSet;

use oxbase_schema_cache::{
    MatchMode, NameHit, NameKind, NameQuery, RelationId, SchemaCache, fuzzy_score,
};
use oxbase_types::{CaseRules, Error, IdentCase, Result, Snippet};
use tokio_util::sync::CancellationToken;

use super::{Completion, CompletionKind, CompletionList, RecentNames};
use crate::{CursorContext, Dialect, Expected};

/// How well a name matched what was typed. Ranks before everything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Tier {
    /// The whole name was typed.
    Exact,
    /// The name starts with what was typed (everything matches an empty prefix).
    Prefix,
    Fuzzy,
}

/// Which matching pass is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Pass {
    Prefix,
    Fuzzy,
}

impl Pass {
    fn mode(self) -> MatchMode {
        match self {
            Self::Prefix => MatchMode::Prefix,
            Self::Fuzzy => MatchMode::Fuzzy,
        }
    }
}

/// Sub-positions inside the group of one expected kind.
pub(super) mod sub {
    /// Join conditions (before the relations they join).
    pub const JOIN: u16 = 0;
    /// The kind's own items: in-scope columns, search-path relations, keywords that fit.
    pub const MAIN: u16 = 1;
    /// Aliases and CTE names.
    pub const ALIAS: u16 = 2;
    /// Relations outside the search path, other keywords.
    pub const OTHER: u16 = 3;
    /// Columns of relations that are not in scope.
    pub const LOOSE: u16 = 4;
}

/// What was typed, ready for matching.
#[derive(Debug)]
pub(super) struct Pattern {
    /// As typed (without the opening quote).
    pub(super) raw: String,
    pub(super) lower: String,
    /// Inside a quoted identifier: matches are exact-case prefixes, no fuzzy pass.
    pub(super) quoted: bool,
}

impl Pattern {
    fn new(prefix: &str, quote: Option<u8>) -> Self {
        Self {
            raw: prefix.to_owned(),
            lower: prefix.to_lowercase(),
            quoted: quote.is_some(),
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.raw.is_empty()
    }

    /// The score of a prefix match with `extra` characters after the typed part, like
    /// `SchemaCache::search` scores it.
    fn prefix_score(&self, extra: usize) -> (Tier, u32) {
        if extra == 0 && !self.is_empty() {
            (Tier::Exact, 1000)
        } else {
            let extra = u32::try_from(extra).unwrap_or(u32::MAX);
            (Tier::Prefix, 900_u32.saturating_sub(extra).max(1))
        }
    }

    /// A prefix match of `name`.
    pub(super) fn prefix_match(&self, name: &str) -> Option<(Tier, u32)> {
        if self.quoted {
            let rest = name.strip_prefix(self.raw.as_str())?;
            return Some(self.prefix_score(rest.chars().count()));
        }
        if self.lower.is_empty() {
            return Some(self.prefix_score(name.chars().count()));
        }
        let n = self.lower.len();
        if name.is_ascii() && self.lower.is_ascii() {
            let head = name.as_bytes().get(..n)?;
            return head
                .eq_ignore_ascii_case(self.lower.as_bytes())
                .then(|| self.prefix_score(name.len() - n));
        }
        let lower = name.to_lowercase();
        let rest = lower.strip_prefix(self.lower.as_str())?;
        Some(self.prefix_score(rest.chars().count()))
    }

    /// A fuzzy match of `name` that is not a prefix match (the prefix pass has those).
    pub(super) fn fuzzy_match(&self, name: &str) -> Option<u32> {
        if self.quoted || self.lower.is_empty() {
            return None;
        }
        let lower = name.to_lowercase();
        if lower.starts_with(self.lower.as_str()) {
            return None;
        }
        fuzzy_score(&self.lower, &lower)
    }

    pub(super) fn matches(&self, pass: Pass, name: &str) -> Option<(Tier, u32)> {
        match pass {
            Pass::Prefix => self.prefix_match(name),
            Pass::Fuzzy => self.fuzzy_match(name).map(|s| (Tier::Fuzzy, s)),
        }
    }
}

/// The order of the list (derived `Ord`: field order is the ranking).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SortKey {
    tier: Tier,
    group: u16,
    recent: u8,
    /// `u32::MAX - score`: higher scores first.
    inv_score: u32,
    /// Label length in characters: shorter first.
    len: usize,
    lower: String,
    label: String,
}

/// How one item ranks (see [`Collector::push`]).
#[derive(Debug, Clone, Copy)]
pub(super) struct Rank<'n> {
    pub(super) tier: Tier,
    pub(super) group: u16,
    pub(super) score: u32,
    /// The catalog name looked up in [`RecentNames`], when recency applies.
    pub(super) recent: Option<&'n str>,
}

/// Collects the items of one request.
pub(super) struct Collector<'a> {
    pub(super) ctx: &'a CursorContext,
    pub(super) dialect: &'a Dialect,
    pub(super) cache: Option<&'a SchemaCache>,
    pub(super) pattern: Pattern,
    /// The cache's case rules (the dialect's defaults without a cache).
    pub(super) rules: CaseRules,
    /// Schemas of the search path (unqualified names resolve there).
    pub(super) path: Vec<u32>,
    recent: &'a RecentNames,
    limit: usize,
    /// Most hits one cache search returns.
    cap: usize,
    /// A source had more matches than it returned.
    overflow: bool,
    cancel: &'a CancellationToken,
    ticks: usize,
    items: Vec<(SortKey, Completion)>,
}

/// Recently used names beyond `limit` that a cache search still returns, so recency can
/// promote them.
const SLACK: usize = 32;
/// `cancel` is checked every this many names.
const CANCEL_EVERY: usize = 4096;

impl<'a> Collector<'a> {
    pub(super) fn new(
        ctx: &'a CursorContext,
        dialect: &'a Dialect,
        cache: Option<&'a SchemaCache>,
        recent: &'a RecentNames,
        limit: usize,
        cancel: &'a CancellationToken,
    ) -> Self {
        Self {
            ctx,
            dialect,
            cache,
            pattern: Pattern::new(&ctx.prefix, ctx.quote),
            rules: cache.map_or(dialect.case_rules, SchemaCache::case_rules),
            path: cache.map_or_else(Vec::new, SchemaCache::search_schemas),
            recent,
            limit,
            cap: limit.saturating_add(SLACK + 1),
            overflow: false,
            cancel,
            ticks: 0,
            items: Vec::new(),
        }
    }

    /// `Err(Cancelled)` once the request is cancelled.
    pub(super) fn check(&self) -> Result<()> {
        if self.cancel.is_cancelled() {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }

    /// Counts one processed name; checks `cancel` every [`CANCEL_EVERY`] names.
    pub(super) fn tick(&mut self) -> Result<()> {
        self.ticks += 1;
        if self.ticks.is_multiple_of(CANCEL_EVERY) {
            self.check()?;
        }
        Ok(())
    }

    /// Runs every source of the expected kinds for one pass.
    pub(super) fn collect(&mut self, pass: Pass, snippets: &[Snippet]) -> Result<()> {
        let ctx = self.ctx;
        for kind in &ctx.expected {
            self.check()?;
            match kind {
                Expected::Keyword => self.keywords(pass)?,
                Expected::Snippet => self.snippets(pass, snippets),
                Expected::Function => self.functions(pass)?,
                Expected::Procedure => self.procedures(pass)?,
                Expected::Type => self.types(pass)?,
                Expected::Sequence => self.sequences(pass)?,
                Expected::Schema => self.schemas(pass)?,
                Expected::Database => self.databases(pass)?,
                Expected::Relation => self.relations(pass)?,
                Expected::Column => self.columns(pass)?,
            }
        }
        self.check()
    }

    /// `true` when a fuzzy pass should fill the list.
    pub(super) fn wants_fuzzy(&self) -> bool {
        !self.pattern.is_empty() && !self.pattern.quoted && self.items.len() < self.limit
    }

    /// The group number of an item of `kind` at sub-position `sub` (see the `sub` module): kinds the
    /// context expects first come first.
    pub(super) fn group(&self, kind: Expected, sub: u16) -> u16 {
        let expected = &self.ctx.expected;
        let slot = expected
            .iter()
            .position(|e| *e == kind)
            .unwrap_or(expected.len());
        u16::try_from(slot).unwrap_or(u16::MAX / 16) * 10 + sub
    }

    /// A completion of `kind` replacing the word at the cursor; the other fields are set by
    /// the caller.
    pub(super) fn item(
        &self,
        label: String,
        kind: CompletionKind,
        insert_text: String,
    ) -> Completion {
        Completion {
            label,
            kind,
            insert_text,
            is_snippet: false,
            detail: None,
            documentation: None,
            sort_key: String::new(),
            filter_text: None,
            replace: self.ctx.word.clone(),
        }
    }

    /// Adds `item` ranked by `rank`.
    pub(super) fn push(&mut self, item: Completion, rank: Rank<'_>) {
        let key = SortKey {
            tier: rank.tier,
            group: rank.group,
            recent: rank.recent.map_or(2, |n| self.recent.rank(n)),
            inv_score: u32::MAX - rank.score,
            len: item.label.chars().count(),
            lower: item.label.to_lowercase(),
            label: item.label.clone(),
        };
        self.items.push((key, item));
    }

    /// The ranked, de-duplicated list of at most `limit` items.
    pub(super) fn finish(mut self) -> CompletionList {
        self.items.sort_by(|a, b| a.0.cmp(&b.0));
        let mut seen: HashSet<(CompletionKind, String)> = HashSet::new();
        let mut items = Vec::with_capacity(self.limit.min(self.items.len()));
        let mut total = 0_usize;
        for (_, item) in self.items {
            if !seen.insert((item.kind, item.insert_text.clone())) {
                continue;
            }
            total += 1;
            if items.len() < self.limit {
                items.push(item);
            }
        }
        for (i, item) in items.iter_mut().enumerate() {
            item.sort_key = format!("{i:06}");
        }
        CompletionList {
            items,
            incomplete: self.overflow || total > self.limit,
        }
    }

    /// Cache names matching the pattern in `pass`, best first, with their tier. Inside a
    /// quoted identifier only exact-case prefixes are kept.
    pub(super) fn search(
        &mut self,
        pass: Pass,
        kinds: &[NameKind],
        schema: Option<u32>,
        relation: Option<RelationId>,
        extra: usize,
    ) -> Vec<(NameHit, Tier)> {
        let Some(cache) = self.cache else {
            return Vec::new();
        };
        if pass == Pass::Fuzzy && (self.pattern.quoted || self.pattern.is_empty()) {
            return Vec::new();
        }
        let limit = if relation.is_some() {
            usize::MAX
        } else {
            self.cap.saturating_add(extra)
        };
        let hits = cache.search(&NameQuery {
            pattern: &self.pattern.raw,
            mode: pass.mode(),
            kinds,
            schema,
            relation,
            limit,
        });
        if hits.len() >= limit {
            self.overflow = true;
        }
        let pattern = &self.pattern;
        hits.into_iter()
            .filter_map(|hit| {
                let tier = match pass {
                    Pass::Fuzzy => {
                        // Prefix matches came from the prefix pass.
                        if pattern.prefix_match(cache.name_of(hit.target)).is_some() {
                            return None;
                        }
                        Tier::Fuzzy
                    }
                    Pass::Prefix if pattern.quoted => {
                        pattern.prefix_match(cache.name_of(hit.target))?.0
                    }
                    Pass::Prefix if hit.score >= 1000 && !pattern.is_empty() => Tier::Exact,
                    Pass::Prefix => Tier::Prefix,
                };
                Some((hit, tier))
            })
            .collect()
    }

    /// `name` as inserted for a catalog name under `case`: in the quote the user opened, else
    /// quoted only where needed ([`Dialect::quote_name`]).
    pub(super) fn ins_name(&self, name: &str, case: IdentCase) -> String {
        match self.ctx.quote {
            Some(open) => quote_with(open, name),
            None => self.dialect.quote_name(name, case),
        }
    }

    /// What the editor filters on for a name item whose insert text is `insert`.
    pub(super) fn name_filter(&self, name: &str, insert: &str) -> Option<String> {
        match self.ctx.quote {
            Some(open) => Some(format!("{}{name}", char::from(open))),
            None if insert != name => Some(name.to_owned()),
            None => None,
        }
    }

    /// `true` when schema `s` is on the search path.
    pub(super) fn in_path(&self, s: u32) -> bool {
        self.path.contains(&s)
    }
}

/// `name` in the quote character `open` (`[` closes with `]`), the closing character doubled
/// inside.
pub(super) fn quote_with(open: u8, name: &str) -> String {
    let open = char::from(open);
    let close = if open == '[' { ']' } else { open };
    let mut out = String::with_capacity(name.len() + 2);
    out.push(open);
    for c in name.chars() {
        if c == close {
            out.push(close);
        }
        out.push(c);
    }
    out.push(close);
    out
}

/// `text` with the snippet syntax characters (`$`, `}`, `\`) escaped.
pub(super) fn escape_snippet(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '$' | '}' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A keyword, function or type name as inserted: an all-upper-case name in lower case when
/// what was typed is lower case, else unchanged.
pub(super) fn adapt_case(name: &str, typed: &str) -> String {
    let typed_lower = typed.bytes().any(|b| b.is_ascii_lowercase())
        && !typed.bytes().any(|b| b.is_ascii_uppercase());
    let name_upper = !name.bytes().any(|b| b.is_ascii_lowercase());
    if typed_lower && name_upper {
        name.to_ascii_lowercase()
    } else {
        name.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_matching_and_scores() {
        let p = Pattern::new("Ord", None);
        assert_eq!(p.prefix_match("orders"), Some((Tier::Prefix, 897)));
        assert_eq!(p.prefix_match("ORD"), Some((Tier::Exact, 1000)));
        assert_eq!(p.prefix_match("or"), None);
        assert_eq!(p.prefix_match("customers"), None);
        let empty = Pattern::new("", None);
        assert_eq!(empty.prefix_match("ab"), Some((Tier::Prefix, 898)));
        let quoted = Pattern::new("Ord", Some(b'"'));
        assert_eq!(quoted.prefix_match("orders"), None);
        assert_eq!(quoted.prefix_match("Orders"), Some((Tier::Prefix, 897)));
        assert_eq!(quoted.fuzzy_match("Orders"), None);
        let unicode = Pattern::new("Сч", None);
        assert_eq!(unicode.prefix_match("счёт"), Some((Tier::Prefix, 898)));
        let fuzzy = Pattern::new("cstord", None);
        assert!(fuzzy.fuzzy_match("customer_order_1").is_some());
        assert_eq!(fuzzy.fuzzy_match("cstord_x"), None);
        assert_eq!(
            fuzzy.matches(Pass::Prefix, "cstord_x").map(|m| m.0),
            Some(Tier::Prefix)
        );
    }

    #[test]
    fn quoting_escaping_and_case() {
        assert_eq!(quote_with(b'"', "a\"b"), "\"a\"\"b\"");
        assert_eq!(quote_with(b'[', "x]y"), "[x]]y]");
        assert_eq!(quote_with(b'`', "a b"), "`a b`");
        assert_eq!(escape_snippet("a$b}c\\"), "a\\$b\\}c\\\\");
        assert_eq!(adapt_case("SELECT", "sel"), "select");
        assert_eq!(adapt_case("SELECT", "Sel"), "SELECT");
        assert_eq!(adapt_case("SELECT", ""), "SELECT");
        assert_eq!(adapt_case("GETDATE", "get_"), "getdate");
        assert_eq!(adapt_case("character varying", "CHAR"), "character varying");
    }
}
