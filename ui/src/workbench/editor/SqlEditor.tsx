/**
 * The SQL text editor of a query tab. The Monaco implementation is loaded on demand (it is large
 * and needs a browser), behind a small handle the query panel drives.
 */
import { lazy, type RefObject, Suspense } from "react";
import type { CacheState, Engine } from "../../api";
import type { FormatOutcome } from "./insight/controller";
import type { SqlLanguage } from "./language";

export type { FormatOutcome, SqlLanguage };

/** Commands bound to editor keys. */
export type EditorCommand = "runAll" | "runCurrent";

/** What the query panel reads from and does to the editor. Offsets are UTF-16 code units. */
export interface SqlEditorHandle {
  getText(): string;
  /** The selected text and where it starts, or `null` when nothing is selected. */
  getSelection(): { text: string; start: number } | null;
  getCursorOffset(): number;
  setText(text: string): void;
  /** Selects `[start, end)` and scrolls it into view. */
  select(start: number, end: number): void;
  focus(): void;
  /** Formats the selection's statements, or the whole text (code insight editors only). */
  format?(): Promise<FormatOutcome>;
  /** Inserts a snippet body (Monaco snippet syntax) at the cursor. */
  insertSnippet?(body: string): void;
}

/**
 * Code insight of a query editor (docs/m6-contracts.md §7): completion, diagnostics, hover and
 * formatting for `engine` with the names of `sessionId`'s schema cache. Without an engine with
 * insight, the editor offers nothing.
 */
export interface EditorInsight {
  /** The editor tab (the same id as `query_run`'s). */
  editorId: string;
  engine: Engine | null;
  sessionId: string | null;
  database: string | null;
  /** The schema cache's generation: diagnostics run again when it changes. */
  generation: number | null;
  /** The cache state the core reported with an answer, when it changed. */
  onCacheState?: ((state: CacheState) => void) | undefined;
  /** How a format command ended. */
  onFormat?: ((outcome: FormatOutcome) => void) | undefined;
}

export interface SqlEditorProps {
  initialValue: string;
  language: SqlLanguage;
  /** Filled once the editor is ready; cleared when it goes away. */
  editorRef: RefObject<SqlEditorHandle | null>;
  onCommand?: ((command: EditorCommand) => void) | undefined;
  ariaLabel?: string | undefined;
  /** A read-only viewer (DDL preview). */
  readOnly?: boolean | undefined;
  /** Controlled text: the editor shows it whenever it changes. */
  value?: string | undefined;
  testId?: string | undefined;
  /** Turns code insight on (query editors). */
  insight?: EditorInsight | undefined;
}

const MonacoSqlEditor = lazy(() => import("./MonacoSqlEditor"));

export function SqlEditor(props: SqlEditorProps) {
  return (
    <Suspense fallback={<div className="sql-editor sql-editor--loading">Loading editor…</div>}>
      <MonacoSqlEditor {...props} />
    </Suspense>
  );
}
