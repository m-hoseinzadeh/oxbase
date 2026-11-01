/**
 * The code-insight logic of one SQL editor (docs/m6-contracts.md §5 and §7), independent of
 * Monaco so it can be tested with fakes; `monacoInsight.ts` wires it to the real editor.
 *
 * - **Completion** has no debounce: Monaco asks on typing, on the trigger characters and on
 *   Ctrl+Space. Every request gets the next `seq` of the editor and a request id; when Monaco
 *   cancels its token the request is cancelled in the core (`cancel_request`), and an answer
 *   that is superseded (by the core or by a newer request) is dropped. An answer that arrives
 *   after the user typed on is kept: its ranges are mapped against the text the request was
 *   sent with, which is how Monaco reads them, and Monaco filters it by what was typed since.
 * - **Diagnostics** run {@link DIAGNOSE_DEBOUNCE_MS} after the last edit and when the target
 *   (engine, connection, database, cache generation) changes; markers cover the whole
 *   document and are replaced by each answer, cleared when there is none.
 * - **Hover** uses Monaco's own delay; **formatting** runs only on command and replaces the
 *   range the core answers as one undoable edit.
 *
 * Failures never reach the user while typing: completion answers nothing, diagnostics clear
 * their markers. A format failure is reported to the caller.
 */
import {
  type CacheState,
  COMPLETION_LIMIT,
  type CompletionResponse,
  DIAGNOSE_DEBOUNCE_MS,
  type DiagnosticsResponse,
  type Engine,
  type FormatRequest,
  type FormatResponse,
  formatSql,
  type HoverResponse,
  type InsightRequest,
  InsightSequencer,
  insightCloseEditor,
  insightComplete,
  insightDiagnose,
  insightHover,
  type Utf16Range,
} from "../../../api";
import { cancelRequest, newRequestId, toCommandError } from "../../../connections/api";
import {
  type MarkerLike,
  type ModelLike,
  type MonacoEnums,
  type PositionLike,
  type RangeLike,
  type SuggestionLike,
  toHover,
  toMarkers,
  toRange,
  toSuggestions,
} from "./mapping";

/** Engines with code insight (the SQL engines). */
export const INSIGHT_ENGINES: readonly Engine[] = [
  "postgres",
  "mysql",
  "mariadb",
  "sqlite",
  "mssql",
];

export function hasInsight(engine: Engine | null | undefined): engine is Engine {
  return !!engine && INSIGHT_ENGINES.includes(engine);
}

/** What the editor's requests are about; `null` = no insight (no SQL engine chosen). */
export interface InsightTarget {
  engine: Engine;
  sessionId: string | null;
  database: string | null;
}

/** The commands the controller calls (the real wrappers unless a test replaces them). */
export interface InsightApi {
  complete(request: InsightRequest, limit: number, requestId: string): Promise<CompletionResponse>;
  diagnose(request: InsightRequest, requestId: string): Promise<DiagnosticsResponse>;
  hover(request: InsightRequest): Promise<HoverResponse | null>;
  format(request: FormatRequest): Promise<FormatResponse>;
  closeEditor(editorId: string): Promise<void>;
  cancel(requestId: string): Promise<unknown>;
  newRequestId(): string;
}

const DEFAULT_API: InsightApi = {
  complete: insightComplete,
  diagnose: insightDiagnose,
  hover: insightHover,
  format: formatSql,
  closeEditor: insightCloseEditor,
  cancel: cancelRequest,
  newRequestId,
};

/** Sequence numbers are shared by every editor of the app (one per editor and kind). */
export const sharedSequencer = new InsightSequencer();

/** Monaco's `CancellationToken`. */
export interface CancellationLike {
  readonly isCancellationRequested: boolean;
  onCancellationRequested(listener: () => void): { dispose(): void };
}

/** Where diagnostics go (Monaco markers of the editor's model). */
export interface MarkerSink {
  set(markers: MarkerLike[]): void;
  clear(): void;
}

/** What the format command reads from and does to the editor (Monaco's code editor). */
export interface FormatEditorLike {
  getModel(): ModelLike | null;
  getSelection(): (RangeLike & { isEmpty(): boolean }) | null;
  executeEdits(
    source: string,
    edits: { range: RangeLike; text: string; forceMoveMarkers?: boolean }[],
  ): boolean;
  pushUndoStop(): boolean;
}

/** How a format command ended. */
export type FormatOutcome =
  | { status: "formatted" }
  | { status: "unchanged" }
  | { status: "unavailable" }
  /** The text changed while the core was formatting: nothing was replaced. */
  | { status: "stale" }
  | { status: "failed"; message: string };

export interface InsightControllerOptions {
  editorId: string;
  target(): InsightTarget | null;
  model(): ModelLike | null;
  enums: MonacoEnums;
  markers: MarkerSink;
  api?: Partial<InsightApi>;
  sequencer?: InsightSequencer;
  /** The cache state the core reported with an answer, when it differs from the last one. */
  onCacheState?: (state: CacheState) => void;
}

/** Counters for tests and diagnostics of the provider itself. */
export interface InsightStats {
  completions: number;
  droppedCompletions: number;
  diagnoseRuns: number;
  droppedDiagnostics: number;
}

const EMPTY_LIST = { suggestions: [] as SuggestionLike[], incomplete: false };

export class InsightController {
  readonly editorId: string;
  readonly stats: InsightStats = {
    completions: 0,
    droppedCompletions: 0,
    diagnoseRuns: 0,
    droppedDiagnostics: 0,
  };
  private readonly options: InsightControllerOptions;
  private readonly api: InsightApi;
  private readonly sequencer: InsightSequencer;
  private timer: ReturnType<typeof setTimeout> | null = null;
  private lastCache: CacheState | null = null;
  private hasMarkers = false;
  private disposed = false;

  constructor(options: InsightControllerOptions) {
    this.options = options;
    this.editorId = options.editorId;
    this.api = { ...DEFAULT_API, ...options.api };
    this.sequencer = options.sequencer ?? sharedSequencer;
  }

  private request(target: InsightTarget, seq: number, text: string, offset: number) {
    return {
      editorId: this.editorId,
      seq,
      engine: target.engine,
      sessionId: target.sessionId,
      database: target.database,
      text,
      offset,
    } satisfies InsightRequest;
  }

  private noteCache(state: CacheState) {
    if (state === this.lastCache) return;
    this.lastCache = state;
    this.options.onCacheState?.(state);
  }

  /** Monaco's `provideCompletionItems`. */
  async complete(
    model: ModelLike,
    position: PositionLike,
    token: CancellationLike,
  ): Promise<{ suggestions: SuggestionLike[]; incomplete: boolean }> {
    const target = this.options.target();
    if (!target || this.disposed || token.isCancellationRequested) return EMPTY_LIST;
    const text = model.getValue();
    const offset = model.getOffsetAt(position);
    const seq = this.sequencer.next(this.editorId, "complete");
    const requestId = this.api.newRequestId();
    let pending = true;
    const subscription = token.onCancellationRequested(() => {
      if (pending) void this.api.cancel(requestId).catch(() => {});
    });
    this.stats.completions += 1;
    try {
      const response = await this.api.complete(
        this.request(target, seq, text, offset),
        COMPLETION_LIMIT,
        requestId,
      );
      pending = false;
      if (!response.superseded) this.noteCache(response.cache);
      if (
        this.disposed ||
        token.isCancellationRequested ||
        this.sequencer.isStale(this.editorId, "complete", response)
      ) {
        this.stats.droppedCompletions += 1;
        // Monaco asks again as the user goes on typing.
        return { suggestions: [], incomplete: true };
      }
      return {
        suggestions: toSuggestions(response.items, text, offset, this.options.enums),
        incomplete: response.incomplete,
      };
    } catch {
      return EMPTY_LIST;
    } finally {
      pending = false;
      subscription.dispose();
    }
  }

  /** Runs diagnostics `delay` ms from now, replacing a run that has not started yet. */
  scheduleDiagnostics(delay: number = DIAGNOSE_DEBOUNCE_MS): void {
    if (this.disposed) return;
    if (this.timer !== null) clearTimeout(this.timer);
    this.timer = setTimeout(() => {
      this.timer = null;
      void this.runDiagnostics();
    }, delay);
  }

  private clearMarkers() {
    if (!this.hasMarkers) return;
    this.hasMarkers = false;
    this.options.markers.clear();
  }

  /** Diagnoses the whole document now and replaces the markers with the answer. */
  async runDiagnostics(): Promise<void> {
    if (this.disposed) return;
    const target = this.options.target();
    const model = this.options.model();
    if (!target || !model) {
      this.clearMarkers();
      return;
    }
    const text = model.getValue();
    const version = model.getVersionId();
    const seq = this.sequencer.next(this.editorId, "diagnose");
    this.stats.diagnoseRuns += 1;
    try {
      const response = await this.api.diagnose(
        this.request(target, seq, text, 0),
        this.api.newRequestId(),
      );
      if (!response.superseded) this.noteCache(response.cache);
      if (
        this.disposed ||
        this.sequencer.isStale(this.editorId, "diagnose", response) ||
        model.getVersionId() !== version
      ) {
        // A newer run is scheduled (an edit or a target change scheduled it).
        this.stats.droppedDiagnostics += 1;
        return;
      }
      if (response.diagnostics.length === 0) {
        this.clearMarkers();
        return;
      }
      this.options.markers.set(toMarkers(response.diagnostics, model, this.options.enums));
      this.hasMarkers = true;
    } catch {
      // Markers of an older text would be misleading: none is better.
      if (
        !this.disposed &&
        !this.sequencer.isStale(this.editorId, "diagnose", { seq, superseded: false })
      )
        this.clearMarkers();
    }
  }

  /** Monaco's `provideHover`. */
  async hover(
    model: ModelLike,
    position: PositionLike,
    token: CancellationLike,
  ): Promise<ReturnType<typeof toHover> | null> {
    const target = this.options.target();
    if (!target || this.disposed) return null;
    const text = model.getValue();
    const version = model.getVersionId();
    try {
      const answer = await this.api.hover(
        this.request(target, 0, text, model.getOffsetAt(position)),
      );
      if (!answer || token.isCancellationRequested || model.getVersionId() !== version) return null;
      return toHover(answer, model);
    } catch {
      return null;
    }
  }

  /** Formats the selection's statements, or the whole text without a selection. */
  async format(editor: FormatEditorLike): Promise<FormatOutcome> {
    const target = this.options.target();
    const model = editor.getModel();
    if (!target || !model || this.disposed) return { status: "unavailable" };
    const text = model.getValue();
    const version = model.getVersionId();
    const selection = editor.getSelection();
    let range: Utf16Range | null = null;
    if (selection && !selection.isEmpty()) {
      range = {
        start: model.getOffsetAt({
          lineNumber: selection.startLineNumber,
          column: selection.startColumn,
        }),
        end: model.getOffsetAt({
          lineNumber: selection.endLineNumber,
          column: selection.endColumn,
        }),
      };
    }
    let answer: FormatResponse;
    try {
      answer = await this.api.format({ engine: target.engine, text, range, options: null });
    } catch (error) {
      return { status: "failed", message: toCommandError(error).message };
    }
    if (this.disposed || model.getVersionId() !== version) return { status: "stale" };
    const start = Math.max(0, Math.min(answer.range.start, text.length));
    const end = Math.max(start, Math.min(answer.range.end, text.length));
    if (text.slice(start, end) === answer.text) return { status: "unchanged" };
    editor.pushUndoStop();
    editor.executeEdits("oxbase.format", [
      { range: toRange(model, { start, end }), text: answer.text, forceMoveMarkers: true },
    ]);
    editor.pushUndoStop();
    // The old markers point into the old layout.
    this.scheduleDiagnostics(0);
    return { status: "formatted" };
  }

  /** Stops timers, clears the markers and tells the core the editor is gone. */
  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    if (this.timer !== null) clearTimeout(this.timer);
    this.timer = null;
    this.clearMarkers();
    this.sequencer.forget(this.editorId);
    void this.api.closeEditor(this.editorId).catch(() => {});
  }
}
