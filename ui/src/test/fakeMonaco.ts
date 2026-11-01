/** Stand-ins for the Monaco pieces the insight code uses (tests run without a browser). */
import type { MonacoEnums, PositionLike, RangeLike } from "../workbench/editor/insight/mapping";

export const ENUMS: MonacoEnums = {
  CompletionItemKind: {
    Method: 0,
    Function: 1,
    Field: 3,
    Variable: 4,
    Struct: 6,
    Interface: 7,
    Module: 8,
    Value: 13,
    Keyword: 17,
    Folder: 23,
    Reference: 21,
    TypeParameter: 24,
    Snippet: 28,
  },
  CompletionItemInsertTextRule: { InsertAsSnippet: 4 },
  MarkerSeverity: { Hint: 1, Warning: 4, Error: 8 },
};

/** A text model with Monaco's position rules (1-based, UTF-16 columns, `\n` lines). */
export class FakeModel {
  private text: string;
  private version = 1;

  constructor(text: string) {
    this.text = text;
  }

  getValue(): string {
    return this.text;
  }

  getVersionId(): number {
    return this.version;
  }

  setValue(text: string): void {
    this.text = text;
    this.version += 1;
  }

  getOffsetAt(position: PositionLike): number {
    const lines = this.text.split("\n");
    let offset = 0;
    for (let i = 0; i < position.lineNumber - 1 && i < lines.length; i += 1)
      offset += (lines[i]?.length ?? 0) + 1;
    const line = lines[position.lineNumber - 1] ?? "";
    return Math.min(offset + Math.min(position.column - 1, line.length), this.text.length);
  }

  getPositionAt(offset: number): PositionLike {
    const clamped = Math.max(0, Math.min(offset, this.text.length));
    const before = this.text.slice(0, clamped);
    const lineNumber = before.split("\n").length;
    const column = clamped - (before.lastIndexOf("\n") + 1) + 1;
    return { lineNumber, column };
  }

  /** Applies a Monaco edit. */
  apply(range: RangeLike, text: string): void {
    const start = this.getOffsetAt({
      lineNumber: range.startLineNumber,
      column: range.startColumn,
    });
    const end = this.getOffsetAt({ lineNumber: range.endLineNumber, column: range.endColumn });
    this.setValue(this.text.slice(0, start) + text + this.text.slice(end));
  }
}

/** Monaco's `CancellationToken` with a `cancel()` for tests. */
export class FakeToken {
  isCancellationRequested = false;
  private listeners: (() => void)[] = [];

  onCancellationRequested(listener: () => void) {
    this.listeners.push(listener);
    return {
      dispose: () => {
        this.listeners = this.listeners.filter((l) => l !== listener);
      },
    };
  }

  cancel(): void {
    this.isCancellationRequested = true;
    for (const listener of this.listeners) listener();
  }
}

/** A code editor over a {@link FakeModel} for the format command. */
export class FakeEditor {
  readonly edits: { range: RangeLike; text: string }[] = [];
  undoStops = 0;
  selection: (RangeLike & { isEmpty(): boolean }) | null = null;

  constructor(readonly model: FakeModel) {}

  getModel() {
    return this.model;
  }

  getSelection() {
    return this.selection;
  }

  select(start: number, end: number): void {
    const a = this.model.getPositionAt(start);
    const b = this.model.getPositionAt(end);
    this.selection = {
      startLineNumber: a.lineNumber,
      startColumn: a.column,
      endLineNumber: b.lineNumber,
      endColumn: b.column,
      isEmpty: () => start === end,
    };
  }

  executeEdits(_source: string, edits: { range: RangeLike; text: string }[]): boolean {
    for (const edit of edits) {
      this.edits.push(edit);
      this.model.apply(edit.range, edit.text);
    }
    return true;
  }

  pushUndoStop(): boolean {
    this.undoStops += 1;
    return true;
  }
}
