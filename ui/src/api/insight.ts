/**
 * M6 code insight: payload types and typed wrappers for the commands of `crates/app-tauri`
 * (src/insight.rs), docs/m6-contracts.md §7.
 *
 * Offsets and ranges are UTF-16 code units (Monaco `model.getOffsetAt`). Completion and
 * diagnostics requests carry the editor id and a growing `seq` per editor and kind
 * (`InsightSequencer`): the core answers an older request with `superseded: true`, which the
 * editor ignores. Debounce rules for the editor are the constants below.
 */
import { call } from "./call";
import type { Engine, ErrorCategory } from "./types";

// --- timing rules (docs/m6-contracts.md §5) ------------------------------------------------

/** Completion is requested on every trigger without a debounce; superseded answers drop. */
export const COMPLETION_DEBOUNCE_MS = 0;
/** Diagnostics run this long after the last edit. */
export const DIAGNOSE_DEBOUNCE_MS = 400;
/** Most completion items asked for (the list is `incomplete` past this). */
export const COMPLETION_LIMIT = 200;

// --- payloads -------------------------------------------------------------------------------

/** `start..end` in UTF-16 code units. */
export interface Utf16Range {
  start: number;
  end: number;
}

/** One editor request (completion, diagnostics, hover). */
export interface InsightRequest {
  editorId: string;
  seq: number;
  engine: Engine;
  /** The open connection for schema names; `null` = keywords, functions and snippets only. */
  sessionId: string | null;
  /** The editor's database; `null` = the session's current one. */
  database: string | null;
  text: string;
  /** The cursor (UTF-16); ignored by diagnostics. */
  offset: number;
}

export type CompletionKind =
  | "keyword"
  | "database"
  | "schema"
  | "table"
  | "view"
  | "column"
  | "function"
  | "procedure"
  | "type"
  | "sequence"
  | "alias"
  | "snippet"
  | "join";

export interface CompletionItem {
  label: string;
  kind: CompletionKind;
  insertText: string;
  /** `insertText` uses snippet syntax (Monaco `InsertAsSnippet`). */
  isSnippet: boolean;
  detail: string | null;
  documentation: string | null;
  sortText: string;
  filterText: string | null;
  range: Utf16Range;
}

/** `oxbase_services::schema_cache::CacheState`. */
export type CacheState = "missing" | "ready" | "stale" | "failed";

export interface CompletionResponse {
  seq: number;
  /** A newer request of the same editor started: ignore this answer. */
  superseded: boolean;
  items: CompletionItem[];
  /** Ask again as the user types (Monaco `incomplete`). */
  incomplete: boolean;
  cache: CacheState;
  elapsedUs: number;
}

export type Severity = "hint" | "warning" | "error";

export type DiagnosticCode =
  | "unterminated-string"
  | "unterminated-identifier"
  | "unterminated-comment"
  | "unbalanced-parenthesis"
  | "unknown-relation"
  | "unknown-column"
  | "ambiguous-column"
  | "unknown-schema"
  | "unknown-function"
  | "wrong-argument-count"
  | "missing-where"
  | "syntax-error"
  | "dialect-mismatch"
  | "missing-if-exists";

export interface DiagnosticItem {
  range: Utf16Range;
  severity: Severity;
  code: DiagnosticCode;
  message: string;
}

export interface DiagnosticsResponse {
  seq: number;
  superseded: boolean;
  diagnostics: DiagnosticItem[];
  cache: CacheState;
  elapsedUs: number;
}

export interface HoverResponse {
  range: Utf16Range;
  /** Markdown. */
  markdown: string;
}

export type KeywordCase = "upper" | "lower" | "preserve";

/** `oxbase_insight::FormatOptions`; every field optional (defaults in Rust). */
export interface FormatOptions {
  indentWidth?: number;
  useTabs?: boolean;
  keywordCase?: KeywordCase;
  maxLineLength?: number;
  leadingCommas?: boolean;
  blankLineBetweenStatements?: boolean;
}

export interface FormatRequest {
  engine: Engine;
  text: string;
  /** Only the statements overlapping this range; `null` = the whole text. */
  range: Utf16Range | null;
  options: FormatOptions | null;
}

/** Replace `range` (UTF-16, of the request's text) with `text`. */
export interface FormatResponse {
  range: Utf16Range;
  text: string;
}

/** A core error as serialized inside payloads (`oxbase_types::Error`). */
export interface CoreError {
  category: ErrorCategory;
  message?: string;
  code?: string | null;
}

export interface CacheKey {
  profileId: string;
  database: string;
}

export interface CacheStatus {
  key: CacheKey;
  state: CacheState;
  source: "disk" | "live" | null;
  building: boolean;
  generation: number | null;
  /** RFC 3339. */
  readAt: string | null;
  relationCount: number;
  columnCount: number;
  routineCount: number;
  truncated: boolean;
  lastBuildMs: number | null;
  lastError: CoreError | null;
}

export type RefreshScope =
  | { type: "full" }
  | { type: "schemas"; names: string[] }
  | { type: "relations"; targets: { schema?: string | null; name: string }[] };

export interface Snippet {
  /** `builtin:<slug>` or a UUID. */
  id: string;
  name: string;
  prefix: string;
  /** Monaco snippet syntax (`$1`, `${1:default}`, `${1|a,b|}`, `$0`). */
  body: string;
  description: string;
  /** Empty = every SQL engine. */
  engines: Engine[];
  builtin: boolean;
}

export interface SnippetInput {
  /** Omit to create; a UUID to replace. */
  id?: string | null;
  name: string;
  prefix: string;
  body: string;
  description?: string;
  engines?: Engine[];
}

// --- sequencing -----------------------------------------------------------------------------

export type InsightKind = "complete" | "diagnose";

/** Hands out growing sequence numbers per (editor, kind) and tells stale answers apart. */
export class InsightSequencer {
  private readonly latest = new Map<string, number>();

  private key(editorId: string, kind: InsightKind): string {
    return `${kind}\u0000${editorId}`;
  }

  /** The next sequence number of (editor, kind). */
  next(editorId: string, kind: InsightKind): number {
    const key = this.key(editorId, kind);
    const seq = (this.latest.get(key) ?? 0) + 1;
    this.latest.set(key, seq);
    return seq;
  }

  /** `true` when an answer must be ignored: superseded by the core or by a newer request. */
  isStale(
    editorId: string,
    kind: InsightKind,
    response: { seq: number; superseded: boolean },
  ): boolean {
    return response.superseded || this.latest.get(this.key(editorId, kind)) !== response.seq;
  }

  /** Forgets an editor (its tab closed). */
  forget(editorId: string): void {
    for (const kind of ["complete", "diagnose"] as const)
      this.latest.delete(this.key(editorId, kind));
  }
}

// --- commands -------------------------------------------------------------------------------

/** Completions at the request's cursor. */
export function insightComplete(
  request: InsightRequest,
  limit?: number,
  requestId?: string,
): Promise<CompletionResponse> {
  return call("insight_complete", { request, limit: limit ?? null, requestId: requestId ?? null });
}

/** Diagnostics of the request's text. */
export function insightDiagnose(
  request: InsightRequest,
  requestId?: string,
): Promise<DiagnosticsResponse> {
  return call("insight_diagnose", { request, requestId: requestId ?? null });
}

/** What to show over the identifier at the cursor; `null` when nothing. */
export function insightHover(request: InsightRequest): Promise<HoverResponse | null> {
  return call("insight_hover", { request });
}

/** Forgets an editor's pending insight requests (its tab closed). */
export function insightCloseEditor(editorId: string): Promise<void> {
  return call("insight_close_editor", { editorId });
}

/** The formatted text, or the formatted statements of `request.range`. */
export function formatSql(request: FormatRequest): Promise<FormatResponse> {
  return call("format_sql", { request });
}

/** Reads the session's catalog now and returns the cache status. */
export function schemaCacheRefresh(
  sessionId: string,
  database?: string | null,
  scope?: RefreshScope,
  requestId?: string,
): Promise<CacheStatus> {
  return call("schema_cache_refresh", {
    sessionId,
    database: database ?? null,
    scope: scope ?? null,
    requestId: requestId ?? null,
  });
}

/** The status of the session's schema cache (no network access). */
export function schemaCacheStatus(
  sessionId: string,
  database?: string | null,
): Promise<CacheStatus> {
  return call("schema_cache_status", { sessionId, database: database ?? null });
}

/** User snippets then built-in ones, for `engine` when given. */
export function snippetList(engine?: Engine): Promise<Snippet[]> {
  return call("snippet_list", { engine: engine ?? null });
}

/** Creates or replaces a user snippet. */
export function snippetSave(snippet: SnippetInput): Promise<Snippet> {
  return call("snippet_save", { snippet });
}

/** Deletes a user snippet; `false` when it did not exist. Built-in ones are rejected. */
export function snippetDelete(id: string): Promise<boolean> {
  return call("snippet_delete", { id });
}
