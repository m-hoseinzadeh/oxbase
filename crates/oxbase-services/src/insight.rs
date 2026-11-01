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
use std::time::Instant;

use oxbase_dialect::DialectRegistry;
use oxbase_insight::{
    Clause, CompletionKind, CursorContext, DiagnosticCode, Dialect, Expected, FormatOptions,
    Outline, RecentNames, SchemaCache, Severity, StatementClass,
};
use oxbase_types::{Engine, Error, Result, ServerVersion};
use serde::{Deserialize, Serialize};

use crate::schema_cache::{CacheKey, CacheState, SchemaCacheService};
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
    /// Recently used names per cache (relations and routines the edited statements name).
    recent: Mutex<HashMap<CacheKey, Arc<RecentNames>>>,
}

/// Most caches whose recently used names are kept.
const MAX_RECENT_CACHES: usize = 64;
/// Most completion items one request may ask for.
const MAX_COMPLETION_LIMIT: usize = 2000;

/// What a request works with (see [`InsightService::inputs`]).
struct Inputs {
    dialect: Dialect,
    cache: Option<SchemaCache>,
    state: CacheState,
    /// A cache build is pending: the answer may grow.
    pending: bool,
    key: Option<CacheKey>,
}

/// Runs `work` on the blocking pool.
async fn blocking<T, F>(work: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| Error::internal("code insight stopped unexpectedly"))?
}

/// A keywords-only context at byte `offset` (texts too large to parse on every keystroke):
/// the word before the cursor is the prefix.
fn keyword_context(dialect: &Dialect, text: &str, offset: usize) -> CursorContext {
    let extra = dialect.language.ident_chars;
    let is_word = |c: char| c.is_alphanumeric() || c == '_' || extra.contains(c);
    let start = text[..offset]
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_word(*c))
        .last()
        .map_or(offset, |(i, _)| i);
    let end = text[offset..]
        .char_indices()
        .find(|(_, c)| !is_word(*c))
        .map_or(text.len(), |(i, _)| offset + i);
    CursorContext {
        offset,
        statement: offset..offset,
        class: StatementClass::Other,
        clause: Clause::Other,
        word: start..end,
        prefix: text[start..offset].to_owned(),
        quote: None,
        qualifier: Vec::new(),
        expected: vec![Expected::Keyword],
        keywords: Vec::new(),
        scope: Vec::new(),
        ctes: Vec::new(),
        outline: Outline::empty(offset..offset),
    }
}

/// Converts many byte ranges of one text to UTF-16 without rescanning the text for each:
/// conversions continue from the last converted offset when they move forward.
struct Utf16Ranges<'t> {
    text: &'t str,
    byte: usize,
    units: usize,
}

impl<'t> Utf16Ranges<'t> {
    fn new(text: &'t str) -> Self {
        Self {
            text,
            byte: 0,
            units: 0,
        }
    }

    fn offset(&mut self, byte: usize) -> usize {
        let mut target = byte.min(self.text.len());
        while !self.text.is_char_boundary(target) {
            target -= 1;
        }
        if target < self.byte {
            self.byte = 0;
            self.units = 0;
        }
        self.units += self.text[self.byte..target].encode_utf16().count();
        self.byte = target;
        self.units
    }

    fn convert(&mut self, range: &Range<usize>) -> Utf16Range {
        let start = self.offset(range.start);
        let end = self.offset(range.end);
        Utf16Range { start, end }
    }
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
            recent: Mutex::new(HashMap::new()),
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
    ///
    /// A request a newer one of the same editor overtook answers `superseded: true`; one
    /// cancelled through `cancel` fails with `Cancelled`. The list is `incomplete` while the
    /// cache is missing or being rebuilt, so the editor asks again.
    #[allow(clippy::needless_pass_by_value)] // the request's token is handed over (contract)
    pub async fn complete(
        &self,
        request: InsightRequest,
        limit: Option<usize>,
        cancel: CancellationToken,
    ) -> Result<CompletionResponse> {
        let seq = request.seq;
        let Some(ticket) = self
            .gate
            .begin(&request.editor_id, RequestKind::Complete, seq)
        else {
            return Ok(CompletionResponse::superseded(seq));
        };
        let work = ticket.token.child_token();
        let result = tokio::select! {
            biased;
            () = cancel.cancelled() => Err(Error::Cancelled),
            r = self.complete_inner(request, limit, &work) => r,
        };
        work.cancel();
        match result {
            Err(e) if e.is_cancelled() && !cancel.is_cancelled() && ticket.token.is_cancelled() => {
                Ok(CompletionResponse::superseded(seq))
            }
            other => other,
        }
    }

    async fn complete_inner(
        &self,
        request: InsightRequest,
        limit: Option<usize>,
        token: &CancellationToken,
    ) -> Result<CompletionResponse> {
        let limit = limit
            .unwrap_or(self.config.completion_limit)
            .clamp(1, MAX_COMPLETION_LIMIT);
        let inputs = self.inputs(&request).await?;
        let InsightRequest {
            seq,
            engine,
            text,
            offset,
            ..
        } = request;
        let text: Arc<str> = Arc::from(text);
        let offset = to_byte_offset(&text, offset);
        let dialect = Arc::new(inputs.dialect);
        let started = Instant::now();

        let list = if text.len() > self.config.max_text_bytes {
            // Too large to parse on every keystroke: keywords only.
            let ctx = keyword_context(&dialect, &text, offset);
            let d = Arc::clone(&dialect);
            let t = token.clone();
            blocking(move || oxbase_insight::complete(&ctx, &d, None, &[], limit, &t)).await?
        } else {
            let (d, body, t) = (Arc::clone(&dialect), Arc::clone(&text), token.clone());
            let ctx = blocking(move || {
                if t.is_cancelled() {
                    return Err(Error::Cancelled);
                }
                Ok(oxbase_insight::context_at(&d, &body, offset))
            })
            .await?;
            let recent = self.recent_for(inputs.key.as_ref(), &ctx);
            let snippets = if ctx.expects(Expected::Snippet) {
                self.snippets.list(Some(engine)).await.unwrap_or_default()
            } else {
                Vec::new()
            };
            let (d, t, cache) = (Arc::clone(&dialect), token.clone(), inputs.cache.clone());
            blocking(move || {
                oxbase_insight::complete_with_recent(
                    &ctx,
                    &d,
                    cache.as_ref(),
                    &snippets,
                    &recent,
                    limit,
                    &t,
                )
            })
            .await?
        };
        let elapsed_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
        let mut ranges = Utf16Ranges::new(&text);
        let items = list
            .items
            .into_iter()
            .map(|c| CompletionItem {
                range: ranges.convert(&c.replace),
                label: c.label,
                kind: c.kind,
                insert_text: c.insert_text,
                is_snippet: c.is_snippet,
                detail: c.detail,
                documentation: c.documentation,
                sort_text: c.sort_key,
                filter_text: c.filter_text,
            })
            .collect();
        Ok(CompletionResponse {
            seq,
            superseded: false,
            items,
            incomplete: list.incomplete || inputs.pending,
            cache: inputs.state,
            elapsed_us,
        })
    }

    /// Diagnostics of the request's text (same gate and cache rules as `complete`).
    #[allow(clippy::needless_pass_by_value)] // the request's token is handed over (contract)
    pub async fn diagnose(
        &self,
        request: InsightRequest,
        cancel: CancellationToken,
    ) -> Result<DiagnosticsResponse> {
        let seq = request.seq;
        let Some(ticket) = self
            .gate
            .begin(&request.editor_id, RequestKind::Diagnose, seq)
        else {
            return Ok(DiagnosticsResponse::superseded(seq));
        };
        let work = ticket.token.child_token();
        let result = tokio::select! {
            biased;
            () = cancel.cancelled() => Err(Error::Cancelled),
            r = self.diagnose_inner(request, &work) => r,
        };
        work.cancel();
        match result {
            Err(e) if e.is_cancelled() && !cancel.is_cancelled() && ticket.token.is_cancelled() => {
                Ok(DiagnosticsResponse::superseded(seq))
            }
            other => other,
        }
    }

    async fn diagnose_inner(
        &self,
        request: InsightRequest,
        token: &CancellationToken,
    ) -> Result<DiagnosticsResponse> {
        let inputs = self.inputs(&request).await?;
        let seq = request.seq;
        let text: Arc<str> = Arc::from(request.text);
        // Larger texts get lexical findings only (no names).
        let cache = inputs
            .cache
            .filter(|_| text.len() <= self.config.max_text_bytes);
        let started = Instant::now();
        let (body, t, dialect) = (Arc::clone(&text), token.clone(), inputs.dialect);
        let found =
            blocking(move || oxbase_insight::diagnose(&dialect, &body, cache.as_ref(), &t)).await?;
        let elapsed_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
        let mut ranges = Utf16Ranges::new(&text);
        let diagnostics = found
            .into_iter()
            .map(|d| DiagnosticItem {
                range: ranges.convert(&d.range),
                severity: d.severity,
                code: d.code,
                message: d.message,
            })
            .collect();
        Ok(DiagnosticsResponse {
            seq,
            superseded: false,
            diagnostics,
            cache: inputs.state,
            elapsed_us,
        })
    }

    /// Hover at the request's cursor (no gate: the editor asks once per hover).
    pub async fn hover(&self, request: InsightRequest) -> Result<Option<HoverResponse>> {
        let inputs = self.inputs(&request).await?;
        let text: Arc<str> = Arc::from(request.text);
        if text.len() > self.config.max_text_bytes {
            return Ok(None);
        }
        let offset = to_byte_offset(&text, request.offset);
        let (body, dialect, cache) = (Arc::clone(&text), inputs.dialect, inputs.cache);
        let found = blocking(move || {
            Ok(oxbase_insight::hover(
                &dialect,
                &body,
                offset,
                cache.as_ref(),
            ))
        })
        .await?;
        Ok(found.map(|h| HoverResponse {
            range: to_utf16_range(&text, &h.range),
            markdown: h.markdown,
        }))
    }

    /// Formats the request's text (or the statements in its range) on the blocking pool.
    pub async fn format(&self, request: FormatRequest) -> Result<FormatResponse> {
        let options = request.options.unwrap_or_default();
        options.validate()?;
        let dialect = self.dialect(request.engine, None)?;
        let text: Arc<str> = Arc::from(request.text);
        let range = request.range.map(|r| to_byte_range(&text, r));
        let body = Arc::clone(&text);
        let edit = blocking(move || match range {
            Some(range) => oxbase_insight::format_range(&dialect, &body, range, &options),
            None => oxbase_insight::format(&dialect, &body, &options).map(|formatted| {
                oxbase_insight::TextEdit {
                    range: 0..body.len(),
                    text: formatted,
                }
            }),
        })
        .await?;
        Ok(FormatResponse {
            range: to_utf16_range(&text, &edit.range),
            text: edit.text,
        })
    }

    /// The insight dialect of `engine`: the registry's language data and quoting (the
    /// standard data and the engine's usual quoting when nothing is registered).
    fn dialect(&self, engine: Engine, version: Option<ServerVersion>) -> Result<Dialect> {
        Dialect::from_registry(&self.dialects, engine, version.clone())
            .or_else(|_| Dialect::builtin(engine).map(|d| Dialect { version, ..d }))
    }

    /// The dialect, cache and cache state for a request. Never waits for the network: the
    /// cache comes from memory or the app database, and a missing or stale one is rebuilt in
    /// the background.
    async fn inputs(&self, request: &InsightRequest) -> Result<Inputs> {
        let mut inputs = Inputs {
            dialect: Dialect::builtin(request.engine)?,
            cache: None,
            state: CacheState::Missing,
            pending: false,
            key: None,
        };
        let mut version = None;
        if let Some(session) = request.session_id
            && let Some(handle) = self.caches.manager().get(session)
        {
            version = Some(handle.info().server_version.clone());
            if let Ok(key) = self.caches.key_for(session, request.database.as_deref()) {
                inputs.cache = self.caches.cache(&key).await;
                match self.caches.ensure(session, request.database.clone()).await {
                    Ok(status) => {
                        inputs.state = status.state;
                        inputs.pending = status.building || inputs.cache.is_none();
                    }
                    Err(_) => {
                        inputs.state = if inputs.cache.is_some() {
                            CacheState::Stale
                        } else {
                            CacheState::Failed
                        };
                    }
                }
                inputs.key = Some(key);
            }
        }
        inputs.dialect = self.dialect(request.engine, version)?;
        if let Some(cache) = &inputs.cache {
            inputs.dialect.case_rules = cache.case_rules();
        }
        Ok(inputs)
    }

    /// Records the names the statement at the cursor uses and returns the recently used
    /// names of the request's cache (empty without one).
    fn recent_for(&self, key: Option<&CacheKey>, ctx: &CursorContext) -> Arc<RecentNames> {
        let Some(key) = key else {
            return Arc::new(RecentNames::default());
        };
        let Ok(mut all) = self.recent.lock() else {
            return Arc::new(RecentNames::default());
        };
        if !all.contains_key(key) && all.len() >= MAX_RECENT_CACHES {
            all.clear();
        }
        let entry = all.entry(key.clone()).or_default();
        Arc::make_mut(entry).record(&ctx.outline, ctx.offset);
        Arc::clone(entry)
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

    fn service(config: InsightConfig) -> InsightService {
        let manager = Arc::new(crate::ConnectionManager::new(
            oxbase_driver::DriverRegistry::new(),
            Arc::new(oxbase_store::MemorySecretStore::new()),
            Arc::new(oxbase_net::ssh::RusshTunnelProvider::new()),
        ));
        let dialects = Arc::new(DialectRegistry::new());
        let caches = Arc::new(SchemaCacheService::new(
            manager,
            Arc::clone(&dialects),
            Arc::new(oxbase_store::MemorySchemaCacheRepository::new()),
            crate::schema_cache::SchemaEvents::new(),
            crate::schema_cache::SchemaCacheConfig::default(),
        ));
        let snippets = Arc::new(SnippetService::new(Arc::new(
            oxbase_store::MemorySnippetRepository::new(),
        )));
        InsightService::new(dialects, caches, snippets, config)
    }

    fn request(editor: &str, seq: u64, text: &str) -> InsightRequest {
        InsightRequest {
            editor_id: editor.into(),
            seq,
            engine: Engine::Postgres,
            session_id: None,
            database: None,
            text: text.into(),
            offset: text.encode_utf16().count(),
        }
    }

    #[tokio::test]
    async fn overtaken_requests_answer_superseded() {
        let svc = service(InsightConfig::default());
        let newer = svc.gate().begin("e", RequestKind::Complete, 5).unwrap();
        let answer = svc
            .complete(request("e", 3, "SEL"), None, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(answer, CompletionResponse::superseded(3));
        // The newer request is not disturbed by the late one.
        assert!(!newer.token.is_cancelled());
        svc.gate().begin("e", RequestKind::Diagnose, 9).unwrap();
        let answer = svc
            .diagnose(request("e", 1, "SELECT"), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(answer, DiagnosticsResponse::superseded(1));
        svc.close_editor("e");
        assert!(newer.token.is_cancelled());
    }

    #[tokio::test]
    async fn a_cancelled_request_fails_with_cancelled() {
        let svc = service(InsightConfig::default());
        let token = CancellationToken::new();
        token.cancel();
        let err = svc
            .complete(request("e", 1, "SEL"), Some(10), token.clone())
            .await
            .unwrap_err();
        assert!(err.is_cancelled());
        let err = svc
            .diagnose(request("e", 1, "SELECT"), token)
            .await
            .unwrap_err();
        assert!(err.is_cancelled());
    }

    #[tokio::test]
    async fn large_texts_get_keywords_only_with_utf16_ranges() {
        let svc = service(InsightConfig {
            completion_limit: 50,
            max_text_bytes: 16,
        });
        let text = "SELECT 'é😀' FROM t; SEL";
        let answer = svc
            .complete(request("e", 1, text), None, CancellationToken::new())
            .await
            .unwrap();
        assert!(!answer.superseded);
        assert_eq!(answer.cache, CacheState::Missing);
        assert!(!answer.incomplete);
        let first = &answer.items[0];
        assert_eq!(first.label, "SELECT");
        assert_eq!(first.kind, CompletionKind::Keyword);
        let end = text.encode_utf16().count();
        assert_eq!(
            first.range,
            Utf16Range {
                start: end - 3,
                end
            }
        );
        assert_eq!(first.sort_text, "000000");
        // MongoDB has no SQL editor.
        let mut mongo = request("m", 1, "x");
        mongo.engine = Engine::MongoDb;
        let err = svc
            .complete(mongo, None, CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(err.category(), oxbase_types::ErrorCategory::Unsupported);
    }

    #[tokio::test]
    async fn format_checks_its_options_first() {
        let svc = service(InsightConfig::default());
        let err = svc
            .format(FormatRequest {
                engine: Engine::Postgres,
                text: "select 1".into(),
                range: None,
                options: Some(FormatOptions {
                    indent_width: 0,
                    ..FormatOptions::default()
                }),
            })
            .await
            .unwrap_err();
        assert_eq!(err.category(), oxbase_types::ErrorCategory::InvalidInput);
    }

    #[test]
    fn utf16_ranges_match_the_one_off_conversion() {
        let text = "a😀b é\nc 😀😀 d";
        let mut ranges = Utf16Ranges::new(text);
        let probes = [0..1, 1..5, 5..9, 3..4, 7..text.len(), 0..0, 2..999];
        for r in probes {
            assert_eq!(ranges.convert(&r), to_utf16_range(text, &r), "{r:?}");
        }
    }

    #[test]
    fn keyword_context_takes_the_word_at_the_cursor() {
        let d = Dialect::builtin(Engine::Postgres).unwrap();
        let text = "x; select more";
        let c = keyword_context(&d, text, 6);
        assert_eq!(c.word, 3..9);
        assert_eq!(c.prefix, "sel");
        assert_eq!(c.expected, [Expected::Keyword]);
        let c = keyword_context(&d, "", 0);
        assert_eq!(c.word, 0..0);
        assert_eq!(c.prefix, "");
    }

    #[test]
    fn recent_names_are_kept_per_cache() {
        let svc = service(InsightConfig::default());
        let d = Dialect::builtin(Engine::Postgres).unwrap();
        let mut ctx = keyword_context(&d, "SELECT * FROM orders", 20);
        ctx.offset = 0;
        ctx.outline.refs = vec![oxbase_insight::NameRef {
            parts: vec![oxbase_insight::Ident {
                text: "orders".into(),
                quoted: false,
            }],
            ranges: Vec::from([14..20]),
            role: oxbase_insight::RefRole::Relation,
            query: Some(0),
            clause: Clause::From,
        }];
        let key = CacheKey {
            profile_id: uuid::Uuid::nil(),
            database: "app".into(),
        };
        let recent = svc.recent_for(Some(&key), &ctx);
        assert_eq!(recent.rank("orders"), 0);
        let other = CacheKey {
            profile_id: uuid::Uuid::nil(),
            database: "other".into(),
        };
        let mut empty = keyword_context(&d, "", 0);
        empty.offset = 0;
        assert_eq!(svc.recent_for(Some(&other), &empty).rank("orders"), 2);
        assert_eq!(svc.recent_for(Some(&key), &empty).rank("orders"), 0);
        assert_eq!(svc.recent_for(None, &ctx).len(), 0);
    }
}
