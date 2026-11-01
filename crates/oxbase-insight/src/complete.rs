//! Schema-aware completion (`docs/m6-contracts.md` §4.3). Owner of [`complete`]: the
//! **complete** stream.
//!
//! # How it works
//!
//! 1. Every kind the cursor expects ([`CursorContext::expected`]) has a *source*
//!    (`sources.rs`): columns of the relations in scope, relations of the search path and of
//!    the other schemas, CTE names and aliases, join conditions from foreign keys, schemas,
//!    databases, the dialect's keywords, functions and types, cached routines, sequences,
//!    types, and snippets.
//! 2. A first pass collects case-insensitive **prefix** matches (inside a quoted identifier:
//!    exact-case prefix matches). When it finds fewer than `limit` items, a second pass fills
//!    the list with **fuzzy** matches ([`oxbase_schema_cache::fuzzy_score`]).
//! 3. Items are ranked (`rank.rs`) by: match tier (exact, then prefix, then fuzzy), group (the
//!    most likely kind first, in [`CursorContext::expected`] order; aliases and CTEs right
//!    after it; names outside the search path or outside the scope after those), recently used
//!    names ([`RecentNames`]), match score, shorter label, label. The returned
//!    [`Completion::sort_key`] encodes that order.
//!
//! The work is bounded: cache searches ask for at most `limit` plus a small slack per source
//! (the index search is a binary search for prefixes), and nothing walks every column of the
//! catalog except a prefix search.

use std::collections::HashMap;
use std::ops::Range;

use oxbase_schema_cache::SchemaCache;
use oxbase_types::{Error, Result, Snippet};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::outline::RefRole;
use crate::{CursorContext, Dialect, Outline};

mod rank;
mod sources;

/// What a completion inserts (the editor picks its icon from this).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionKind {
    Keyword,
    Database,
    Schema,
    Table,
    View,
    Column,
    Function,
    Procedure,
    Type,
    Sequence,
    /// A table alias or CTE name in scope.
    Alias,
    Snippet,
    /// A whole join condition (`JOIN orders o ON o.customer_id = c.id`) from a foreign key.
    Join,
}

/// One completion item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Completion {
    /// What the list shows (the plain name, keyword or snippet prefix).
    pub label: String,
    pub kind: CompletionKind,
    /// What replaces [`Completion::replace`]: the name quoted where needed
    /// ([`Dialect::quote_name`]), a function with `(`, a snippet body.
    pub insert_text: String,
    /// `insert_text` uses snippet syntax (`$1`, `${1:x}`): functions with arguments,
    /// snippets, joins.
    pub is_snippet: bool,
    /// One line next to the label: the column type, the relation kind and schema, a function
    /// signature, the snippet name.
    pub detail: Option<String>,
    /// Longer Markdown text (comments, function summaries, snippet descriptions).
    pub documentation: Option<String>,
    /// The editor sorts by this (byte order); the list is also returned in this order.
    pub sort_key: String,
    /// What the editor filters on when it differs from `label` (a quoted name's plain text).
    pub filter_text: Option<String>,
    /// Byte range of the text the item replaces (normally [`CursorContext::word`]).
    pub replace: Range<usize>,
}

/// The ranked completions for one cursor.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletionList {
    pub items: Vec<Completion>,
    /// More items matched than `limit` allowed: the editor must ask again as the user types
    /// (Monaco `incomplete`).
    pub incomplete: bool,
}

/// The completions for `ctx`, at most `limit`, best first.
///
/// Sources: `cache` names of the kinds in [`CursorContext::expected`] (columns only from
/// [`CursorContext::column_sources`] when the scope resolves, else every column matching the
/// prefix), CTEs and aliases in scope, the dialect's keywords ([`CursorContext::keywords`]
/// first), built-in functions and types (filtered by [`Dialect::version`]), and `snippets`
/// that apply to [`Dialect::engine`] at a statement start. Matching is case-insensitive prefix
/// first, then fuzzy; inserted names follow [`Dialect::quote_name`] with the cache's case
/// rules. Ranking rules: `docs/m6-contracts.md` §4.3.
///
/// Latency: end to end with [`crate::context_at`], at most 50 ms at p95 on the exit-check
/// cache in a release build (§5). Checks `cancel` between sources and every few thousand
/// names, and returns `Error::Cancelled` once it is cancelled.
pub fn complete(
    ctx: &CursorContext,
    dialect: &Dialect,
    cache: Option<&SchemaCache>,
    snippets: &[Snippet],
    limit: usize,
    cancel: &CancellationToken,
) -> Result<CompletionList> {
    complete_with_recent(
        ctx,
        dialect,
        cache,
        snippets,
        &RecentNames::default(),
        limit,
        cancel,
    )
}

/// [`complete`] with a list of recently used names: among items of the same match tier and
/// group, names in `recent` rank first (the most recent ones before older ones). The app keeps
/// one [`RecentNames`] per schema cache, fed with the relations and routines of the statements
/// the user edits.
pub fn complete_with_recent(
    ctx: &CursorContext,
    dialect: &Dialect,
    cache: Option<&SchemaCache>,
    snippets: &[Snippet],
    recent: &RecentNames,
    limit: usize,
    cancel: &CancellationToken,
) -> Result<CompletionList> {
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    if ctx.expected.is_empty() || limit == 0 {
        return Ok(CompletionList::default());
    }
    let mut collector = rank::Collector::new(ctx, dialect, cache, recent, limit, cancel);
    collector.collect(rank::Pass::Prefix, snippets)?;
    if collector.wants_fuzzy() {
        collector.collect(rank::Pass::Fuzzy, snippets)?;
    }
    Ok(collector.finish())
}

/// Names the user used recently (relations, routines), most recent first, at most a fixed
/// number. Lookups ignore case. Cheap to clone for the sizes involved (hundreds of names).
#[derive(Debug, Clone)]
pub struct RecentNames {
    /// Lower-cased name -> logical time of its last use.
    stamps: HashMap<String, u64>,
    clock: u64,
    capacity: usize,
}

impl Default for RecentNames {
    fn default() -> Self {
        Self::new(Self::DEFAULT_CAPACITY)
    }
}

impl RecentNames {
    /// Names kept by [`RecentNames::default`].
    pub const DEFAULT_CAPACITY: usize = 512;
    /// Names this recent rank before older recent ones.
    const FRESH: u64 = 32;

    /// An empty list keeping at most `capacity` names (at least 1).
    pub fn new(capacity: usize) -> Self {
        Self {
            stamps: HashMap::new(),
            clock: 0,
            capacity: capacity.max(1),
        }
    }

    pub fn len(&self) -> usize {
        self.stamps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.stamps.is_empty()
    }

    /// Marks `name` as just used.
    pub fn touch(&mut self, name: &str) {
        if name.is_empty() {
            return;
        }
        self.clock += 1;
        self.stamps.insert(name.to_lowercase(), self.clock);
        if self.stamps.len() > self.capacity + self.capacity / 4 {
            let mut by_age: Vec<u64> = self.stamps.values().copied().collect();
            by_age.sort_unstable();
            let oldest_kept = by_age[by_age.len() - self.capacity];
            self.stamps.retain(|_, stamp| *stamp >= oldest_kept);
        }
    }

    /// Marks the relations, functions and procedures `outline` references as used, except a
    /// reference containing `skip` (the word being typed).
    pub fn record(&mut self, outline: &Outline, skip: usize) {
        for r in &outline.refs {
            let range = r.range();
            if range.start <= skip && skip <= range.end {
                continue;
            }
            if matches!(
                r.role,
                RefRole::Relation | RefRole::Function | RefRole::Procedure
            ) && let Some(last) = r.parts.last()
            {
                self.touch(&last.text);
            }
        }
    }

    /// `0` for one of the most recently used names, `1` for an older recent one, `2` for a
    /// name not in the list.
    pub fn rank(&self, name: &str) -> u8 {
        if self.stamps.is_empty() {
            return 2;
        }
        match self.stamps.get(&name.to_lowercase()) {
            Some(stamp) if self.clock - stamp < Self::FRESH => 0,
            Some(_) => 1,
            None => 2,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payloads_serialize_for_the_editor() {
        let item = Completion {
            label: "Orders".into(),
            kind: CompletionKind::Table,
            insert_text: "\"Orders\"".into(),
            is_snippet: false,
            detail: Some("table · public".into()),
            documentation: None,
            sort_key: "0100".into(),
            filter_text: Some("Orders".into()),
            replace: 7..9,
        };
        let json = serde_json::to_value(&item).unwrap();
        assert_eq!(json["kind"], "table");
        assert_eq!(json["insertText"], "\"Orders\"");
        assert_eq!(json["replace"], serde_json::json!({ "start": 7, "end": 9 }));
        let list = CompletionList {
            items: vec![item],
            incomplete: true,
        };
        let back: CompletionList =
            serde_json::from_value(serde_json::to_value(&list).unwrap()).unwrap();
        assert_eq!(back, list);
        assert_eq!(
            serde_json::to_value(CompletionKind::Join).unwrap(),
            serde_json::json!("join")
        );
    }
}
