/**
 * The SQL text editor of a query tab. The Monaco implementation is loaded on demand (it is large
 * and needs a browser), behind a small handle the query panel drives.
 */
import { lazy, type RefObject, Suspense } from "react";

/** Monaco language id. */
export type SqlLanguage = "pgsql" | "mysql" | "sql";

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
}

export interface SqlEditorProps {
  initialValue: string;
  language: SqlLanguage;
  /** Filled once the editor is ready; cleared when it goes away. */
  editorRef: RefObject<SqlEditorHandle | null>;
  onCommand?: (command: EditorCommand) => void;
  ariaLabel?: string;
}

const MonacoSqlEditor = lazy(() => import("./MonacoSqlEditor"));

export function SqlEditor(props: SqlEditorProps) {
  return (
    <Suspense fallback={<div className="sql-editor sql-editor--loading">Loading editor…</div>}>
      <MonacoSqlEditor {...props} />
    </Suspense>
  );
}
