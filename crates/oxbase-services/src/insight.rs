//! M6: code insight for the editor, as the app calls it (`docs/m6-contracts.md` §4 and §7).
//! Owner of the bodies marked `unimplemented!("m6: complete")`: the **complete** stream.
//!
//! The payloads here are the Tauri command payloads (camelCase): offsets and ranges are in
//! **UTF-16 code units** (Monaco's `model.getOffsetAt`), converted to and from the byte
//! offsets of `oxbase_insight` with [`to_byte_offset`] / [`to_utf16_range`].
//!
//! # Latency and cancellation
//!
//! - Every completion and diagnostics request carries its editor's id and a sequence number
//!   that grows with every request of that editor. [`RequestGate`] lets only the newest one
//!   run: a request that arrives after a newer one returns `superseded: true` at once, and a
//!   running request is cancelled (its token fires) when a newer one of the same editor and
//!   kind starts.
//! - The work runs on the blocking pool (`tokio::task::spawn_blocking`), never on the async
//!   workers, and never waits for the network: the cache comes from
//!   [`SchemaCacheService::cache`]; when it is missing or stale, [`SchemaCacheService::ensure`]
//!   starts a background build and the request answers with what it has (keywords,
//!   functions, snippets), reporting the [`CacheState`].
//! - Budget: completion at most 50 ms at p95 end to end on the exit-check cache in a release
//!   build (§5); diagnostics at most 100 ms for a 1 MiB script.

use std::collections::HashMap;
use std::fmt;
use std::ops::Range;
use std::sync::{Arc, Mutex};

use oxbase_dialect::DialectRegistry;
use oxbase_insight::{CompletionKind, DiagnosticCode, FormatOptions, Severity};
use oxbase_types::{Engine, Result};
use serde::{Deserialize, Serialize};

use crate::schema_cache::{CacheState, SchemaCacheService};
use crate::snippets::SnippetService;
use crate::{CancellationToken, SessionId};

/// A range in UTF-16 code units, `start..end`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Utf16Range {
    pub start: usize,
    pub end: usize,
}

/// The byte offset of UTF-16 `offset` in `text` (clamped; see
/// `oxbase_sql::byte_offset_from_utf16`).
pub fn to_byte_offset(text: &str, offset: usize) -> usize {
    oxbase_sql::byte_offset_from_utf16(text, offset)
}

/// The UTF-16 range of byte `range` in `text` (clamped).
pub fn to_utf16_range(text: &str, range: &Range<usize>) -> Utf16Range {
    Utf16Range {
        start: oxbase_sql::utf16_offset_from_byte(text, range.start),
        end: oxbase_sql::utf16_offset_from_byte(text, range.end),
    }
}

/// The byte range of UTF-16 `range` in `text` (clamped, `start <= end`).
pub fn to_byte_range(text: &str, range: Utf16Range) -> Range<usize> {
    let end = to_byte_offset(text, range.end);
    to_byte_offset(text, range.start).min(end)..end
}

/// One editor request (completion, diagnostics, hover).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InsightRequest {
    /// The editor tab (the same id as `query_run`'s `editorId`).
    pub editor_id: String,
    /// Grows with every request of this editor (per kind); see [`RequestGate`].
    pub seq: u64,
    pub engine: Engine,
    /// The open connection, for schema names; `None` for an editor without one (keywords,
    /// functions and snippets only).
    #[serde(default)]
    pub session_id: Option<SessionId>,
    /// The editor's database; `None` = the session's current database.
    #[serde(default)]
    pub database: Option<String>,
    /// The whole editor text.
    pub text: String,
    /// The cursor, in UTF-16 code units (ignored by diagnostics).
    #[serde(default)]
    pub offset: usize,
}

/// One completion item as the editor needs it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletionItem {
    pub label: String,
    pub kind: CompletionKind,
    pub insert_text: String,
    /// `insert_text` is snippet syntax (Monaco `InsertAsSnippet`).
    pub is_snippet: bool,
    pub detail: Option<String>,
    pub documentation: Option<String>,
    /// Monaco `sortText`.
    pub sort_text: String,
    /// Monaco `filterText`.
    pub filter_text: Option<String>,
    /// What the item replaces, in UTF-16 code units.
    pub range: Utf16Range,
}

impl CompletionItem {
    /// The editor form of a core completion over `text`.
    pub fn from_core(text: &str, c: oxbase_insight::Completion) -> Self {
        Self {
            range: to_utf16_range(text, &c.replace),
            label: c.label,
            kind: c.kind,
            insert_text: c.insert_text,
            is_snippet: c.is_snippet,
            detail: c.detail,
            documentation: c.documentation,
            sort_text: c.sort_key,
            filter_text: c.filter_text,
        }
    }
}

/// The answer to `insight_complete`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletionResponse {
    pub seq: u64,
    /// A newer request of the same editor started: ignore this answer (`items` is empty).
    pub superseded: bool,
    pub items: Vec<CompletionItem>,
    /// Ask again as the user types (Monaco `incomplete`).
    pub incomplete: bool,
    /// The state of the schema cache the items came from (`missing` without a session).
    pub cache: CacheState,
    /// Time spent in the core, in microseconds.
    pub elapsed_us: u64,
}

impl CompletionResponse {
    /// The answer for a superseded request.
    pub fn superseded(seq: u64) -> Self {
        Self {
            seq,
            superseded: true,
            items: Vec::new(),
            incomplete: true,
            cache: CacheState::Missing,
            elapsed_us: 0,
        }
    }
}

/// One diagnostic as the editor needs it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticItem {
    pub range: Utf16Range,
    pub severity: Severity,
    pub code: DiagnosticCode,
    pub message: String,
}

impl DiagnosticItem {
    pub fn from_core(text: &str, d: oxbase_insight::Diagnostic) -> Self {
        Self {
            range: to_utf16_range(text, &d.range),
            severity: d.severity,
            code: d.code,
            message: d.message,
        }
    }
}

/// The answer to `insight_diagnose`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticsResponse {
    pub seq: u64,
    pub superseded: bool,
    pub diagnostics: Vec<DiagnosticItem>,
    pub cache: CacheState,
    pub elapsed_us: u64,
}

impl DiagnosticsResponse {
    pub fn superseded(seq: u64) -> Self {
        Self {
            seq,
            superseded: true,
            diagnostics: Vec::new(),
            cache: CacheState::Missing,
            elapsed_us: 0,
        }
    }
}

/// The answer to `insight_hover` (`null` when there is nothing to show).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HoverResponse {
    pub range: Utf16Range,
    pub markdown: String,
}

/// The `format_sql` request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FormatRequest {
    pub engine: Engine,
    pub text: String,
    /// Format only the statements overlapping this range; `None` = the whole text.
    #[serde(default)]
    pub range: Option<Utf16Range>,
    #[serde(default)]
    pub options: Option<FormatOptions>,
}

/// The answer to `format_sql`: replace `range` (UTF-16, of the request's text) with `text`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FormatResponse {
    pub range: Utf16Range,
    pub text: String,
}

/// Which request stream of an editor a [`RequestGate`] ticket belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RequestKind {
    Complete,
    Diagnose,
}

/// Lets only the newest request of each (editor, kind) run. Complete (Design step).
#[derive(Debug, Default)]
pub struct RequestGate {
    latest: Mutex<HashMap<(String, RequestKind), (u64, CancellationToken)>>,
}

/// A running request admitted by [`RequestGate::begin`].
#[derive(Debug, Clone)]
pub struct GateTicket {
    pub seq: u64,
    /// Fires when a newer request of the same editor and kind begins (or the editor closes).
    pub token: CancellationToken,
}

impl RequestGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Admits request `seq` of (`editor`, `kind`): `None` when a newer one already began
    /// (the caller answers "superseded"). Otherwise cancels the token of the older running
    /// request and returns a ticket whose token is cancelled when a newer one begins. An equal
    /// `seq` (a retry) gets a fresh ticket and cancels the earlier one.
    pub fn begin(&self, editor: &str, kind: RequestKind, seq: u64) -> Option<GateTicket> {
        let mut latest = self.latest.lock().ok()?;
        let key = (editor.to_owned(), kind);
        if let Some((newest, token)) = latest.get(&key) {
            if *newest > seq {
                return None;
            }
            token.cancel();
        }
        let token = CancellationToken::new();
        latest.insert(key, (seq, token.clone()));
        Some(GateTicket { seq, token })
    }

    /// `true` while `seq` is the newest request of (`editor`, `kind`).
    pub fn is_current(&self, editor: &str, kind: RequestKind, seq: u64) -> bool {
        self.latest.lock().is_ok_and(|latest| {
            latest
                .get(&(editor.to_owned(), kind))
                .is_some_and(|(newest, _)| *newest == seq)
        })
    }

    /// Cancels and forgets every request of `editor` (its tab closed).
    pub fn forget(&self, editor: &str) {
        if let Ok(mut latest) = self.latest.lock() {
            latest.retain(|(e, _), (_, token)| {
                let keep = e != editor;
                if !keep {
                    token.cancel();
                }
                keep
            });
        }
    }
}

/// Settings of the [`InsightService`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InsightConfig {
    /// Most completion items returned (the editor asks again as the user types).
    pub completion_limit: usize,
    /// Larger editor texts get keywords-only completion and lexical diagnostics.
    pub max_text_bytes: usize,
}

impl Default for InsightConfig {
    fn default() -> Self {
        Self {
            completion_limit: 200,
            max_text_bytes: 4 * 1024 * 1024,
        }
    }
}

/// Completion, diagnostics, hover and formatting for the app. See the module docs.
pub struct InsightService {
    dialects: Arc<DialectRegistry>,
    caches: Arc<SchemaCacheService>,
    snippets: Arc<SnippetService>,
    gate: RequestGate,
    config: InsightConfig,
}

impl InsightService {
    pub fn new(
        dialects: Arc<DialectRegistry>,
        caches: Arc<SchemaCacheService>,
        snippets: Arc<SnippetService>,
        config: InsightConfig,
    ) -> Self {
        Self {
            dialects,
            caches,
            snippets,
            gate: RequestGate::new(),
            config,
        }
    }

    pub fn dialects(&self) -> &Arc<DialectRegistry> {
        &self.dialects
    }

    pub fn caches(&self) -> &Arc<SchemaCacheService> {
        &self.caches
    }

    pub fn snippets(&self) -> &Arc<SnippetService> {
        &self.snippets
    }

    pub fn gate(&self) -> &RequestGate {
        &self.gate
    }

    pub fn config(&self) -> &InsightConfig {
        &self.config
    }

    /// Completions at the request's cursor: gate, cache lookup (no network), dialect from the
    /// registry with the session's server version, user + built-in snippets for the engine,
    /// `oxbase_insight::context_at` + `complete` on the blocking pool, items converted to
    /// UTF-16. `cancel` (the app's request id) and the gate's token both stop it.
    #[allow(clippy::unused_async)] // skeleton until the complete stream fills it
    pub async fn complete(
        &self,
        request: InsightRequest,
        limit: Option<usize>,
        cancel: CancellationToken,
    ) -> Result<CompletionResponse> {
        let _ = (self, request, limit, cancel);
        unimplemented!("m6: complete")
    }

    /// Diagnostics of the request's text (same gate and cache rules as `complete`).
    #[allow(clippy::unused_async)] // skeleton until the complete stream fills it
    pub async fn diagnose(
        &self,
        request: InsightRequest,
        cancel: CancellationToken,
    ) -> Result<DiagnosticsResponse> {
        let _ = (self, request, cancel);
        unimplemented!("m6: complete")
    }

    /// Hover at the request's cursor (no gate: the editor asks once per hover).
    #[allow(clippy::unused_async)] // skeleton until the complete stream fills it
    pub async fn hover(&self, request: InsightRequest) -> Result<Option<HoverResponse>> {
        let _ = (self, request);
        unimplemented!("m6: complete")
    }

    /// Formats the request's text (or the statements in its range) on the blocking pool.
    #[allow(clippy::unused_async)] // skeleton until the complete stream fills it
    pub async fn format(&self, request: FormatRequest) -> Result<FormatResponse> {
        let _ = (self, request);
        unimplemented!("m6: complete")
    }

    /// Forgets an editor's requests (its tab closed).
    pub fn close_editor(&self, editor_id: &str) {
        self.gate.forget(editor_id);
    }
}

impl fmt::Debug for InsightService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InsightService")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn utf16_conversions() {
        let text = "SELECT 'é😀' FROM t";
        // 'é' is 2 bytes / 1 unit, '😀' 4 bytes / 2 units.
        let from = text.find("FROM").unwrap();
        let r = to_utf16_range(text, &(from..from + 4));
        assert_eq!(r, Utf16Range { start: 13, end: 17 });
        assert_eq!(to_byte_range(text, r), from..from + 4);
        assert_eq!(to_byte_offset(text, 999), text.len());
        assert_eq!(to_byte_range(text, Utf16Range { start: 9, end: 3 }), 3..3);
    }

    #[test]
    fn requests_and_responses_have_their_wire_form() {
        let req: InsightRequest = serde_json::from_value(json!({
            "editorId": "tab-1", "seq": 7, "engine": "postgres",
            "text": "SELECT ", "offset": 7
        }))
        .unwrap();
        assert_eq!(req.session_id, None);
        assert_eq!(req.database, None);
        let item = CompletionItem::from_core(
            "SELECT é",
            oxbase_insight::Completion {
                label: "éa".into(),
                kind: CompletionKind::Column,
                insert_text: "éa".into(),
                is_snippet: false,
                detail: None,
                documentation: None,
                sort_key: "1".into(),
                filter_text: None,
                replace: 7..9,
            },
        );
        assert_eq!(item.range, Utf16Range { start: 7, end: 8 });
        let v = serde_json::to_value(CompletionResponse {
            seq: 7,
            superseded: false,
            items: vec![item],
            incomplete: false,
            cache: CacheState::Ready,
            elapsed_us: 12,
        })
        .unwrap();
        assert_eq!(v["items"][0]["sortText"], "1");
        assert_eq!(v["items"][0]["kind"], "column");
        assert_eq!(v["items"][0]["range"], json!({ "start": 7, "end": 8 }));
        assert_eq!(v["cache"], "ready");
        assert_eq!(v["elapsedUs"], 12);
        let s = serde_json::to_value(DiagnosticsResponse::superseded(3)).unwrap();
        assert_eq!(s["superseded"], true);
        let d = DiagnosticItem::from_core(
            "x",
            oxbase_insight::Diagnostic::new(
                DiagnosticCode::UnknownRelation,
                0..1,
                Severity::Error,
                "unknown",
            ),
        );
        let v = serde_json::to_value(d).unwrap();
        assert_eq!(v["severity"], "warning");
        assert_eq!(v["code"], "unknown-relation");
        let f: FormatRequest = serde_json::from_value(json!({
            "engine": "mssql", "text": "select 1",
            "range": { "start": 0, "end": 3 }, "options": { "keywordCase": "lower" }
        }))
        .unwrap();
        assert_eq!(f.range, Some(Utf16Range { start: 0, end: 3 }));
        assert_eq!(
            f.options.map(|o| o.keyword_case),
            Some(oxbase_insight::KeywordCase::Lower)
        );
        assert_eq!(CompletionResponse::superseded(1).items, Vec::new());
    }

    #[test]
    fn the_gate_admits_only_the_newest_request() {
        let gate = RequestGate::new();
        let t1 = gate.begin("e", RequestKind::Complete, 1).unwrap();
        let d1 = gate.begin("e", RequestKind::Diagnose, 1).unwrap();
        let t3 = gate.begin("e", RequestKind::Complete, 3).unwrap();
        assert!(t1.token.is_cancelled());
        assert!(!t3.token.is_cancelled());
        // Another kind is independent.
        assert!(!d1.token.is_cancelled());
        // Late arrival of an older request.
        assert!(gate.begin("e", RequestKind::Complete, 2).is_none());
        assert!(gate.is_current("e", RequestKind::Complete, 3));
        assert!(!gate.is_current("e", RequestKind::Complete, 1));
        // Another editor is independent.
        let other = gate.begin("f", RequestKind::Complete, 1).unwrap();
        // A retry with the same seq replaces the ticket.
        let t3b = gate.begin("e", RequestKind::Complete, 3).unwrap();
        assert!(t3.token.is_cancelled());
        gate.forget("e");
        assert!(t3b.token.is_cancelled() && d1.token.is_cancelled());
        assert!(!other.token.is_cancelled());
        assert!(!gate.is_current("e", RequestKind::Complete, 3));
        assert!(gate.begin("e", RequestKind::Complete, 1).is_some());
    }
}
