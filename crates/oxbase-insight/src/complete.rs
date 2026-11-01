//! Schema-aware completion (`docs/m6-contracts.md` §4.3). Owner of [`complete`]: the
//! **complete** stream.

use std::ops::Range;

use oxbase_schema_cache::SchemaCache;
use oxbase_types::{Result, Snippet};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::{CursorContext, Dialect};

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
    let _ = (ctx, dialect, cache, snippets, limit, cancel);
    unimplemented!("m6: complete")
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
