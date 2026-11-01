/**
 * The grid's cell overlay editor: a typed {@link ValueEditor} for one cell. Enter keeps the
 * value and moves down (Shift+Enter is a new line), Escape drops it, clicking elsewhere keeps
 * it. A stored cell starts from its exact edit text (`row_get_form`), not the shortened grid
 * text; typing a character on a selected cell starts from that character.
 */
import { type KeyboardEvent, useEffect, useRef, useState } from "react";
import type { CellInput, EditColumn } from "../../api";
import { ValueEditor } from "./ValueEditor";
import { sameInput } from "./valueInput";

export interface CellEditorProps {
  column: EditColumn;
  /** The cell's current input, or a loader for the stored row's exact text. */
  initial: CellInput | (() => Promise<CellInput>);
  /** What the user typed to open the editor. */
  initialText?: string | undefined;
  /** New rows may use the column default. */
  allowDefault: boolean;
  onCommit: (input: CellInput) => void;
  finish: (move: boolean) => void;
}

export function CellEditor({
  column,
  initial,
  initialText,
  allowDefault,
  onCommit,
  finish,
}: CellEditorProps) {
  const typed: CellInput | null =
    initialText === undefined ? null : { type: "text", text: initialText };
  const [value, setValue] = useState<CellInput | null>(
    typed ?? (typeof initial === "function" ? null : initial),
  );
  const [loadError, setLoadError] = useState<string | null>(null);
  const start = useRef<CellInput | null>(typeof initial === "function" ? null : initial);
  const latest = useRef<CellInput | null>(value);
  latest.current = value;
  const done = useRef(false);
  const commitRef = useRef(onCommit);
  commitRef.current = onCommit;

  useEffect(() => {
    if (typeof initial !== "function") return;
    let alive = true;
    initial().then(
      (input) => {
        if (!alive) return;
        start.current = input;
        setValue((v) => v ?? input);
      },
      (error: unknown) => {
        if (!alive) return;
        setLoadError(
          error instanceof Error
            ? error.message
            : String((error as { message?: string })?.message ?? error),
        );
        setValue((v) => v ?? { type: "text", text: "" });
      },
    );
    return () => {
      alive = false;
    };
  }, [initial]);

  // Closing the overlay any way but Escape keeps the value.
  useEffect(
    () => () => {
      const v = latest.current;
      if (done.current || v === null) return;
      if (start.current && sameInput(start.current, v)) return;
      commitRef.current(v);
    },
    [],
  );

  function onKeyDown(event: KeyboardEvent<HTMLElement>) {
    if (event.key === "Escape") {
      done.current = true;
      return;
    }
    if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      event.stopPropagation();
      const v = latest.current;
      done.current = true;
      if (v && !(start.current && sameInput(start.current, v))) commitRef.current(v);
      finish(true);
    }
  }

  return (
    <div className="cell-editor" data-testid="cell-editor">
      <div className="cell-editor__head">
        <strong>{column.name}</strong>
        <span className="cell-editor__type">{column.nativeType}</span>
      </div>
      {value === null ? (
        <p className="cell-editor__loading">Loading…</p>
      ) : (
        <ValueEditor
          kind={column.input}
          dataType={column.dataType}
          value={value}
          onChange={setValue}
          label={column.name}
          nullable={column.nullable}
          allowDefault={allowDefault}
          autoFocus
          onKeyDown={onKeyDown}
        />
      )}
      {loadError ? (
        <p className="value-editor__hint">Could not load the value: {loadError}</p>
      ) : null}
      <p className="cell-editor__keys">Enter to keep · Esc to cancel</p>
    </div>
  );
}
