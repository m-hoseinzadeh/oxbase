/**
 * The value dialog (Shift+Enter, or "View value" in the grid's menu): a cell's whole value in
 * a large view (./ValueView: formats, HTML preview, images). An editable cell can be edited
 * here as text, set to NULL, and saved into the pending changes.
 */
import { LoaderCircle, Pencil } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import type { CellInput, ColumnInfo, Engine } from "../../api";
import { ICON_SM } from "../../components/icons";
import { Modal } from "../../components/Modal";
import { SqlEditor, type SqlEditorHandle } from "../editor/SqlEditor";
import { columnTypeLabel } from "./format";
import { ValueView } from "./ValueView";

/** Editing in the dialog: where the edit starts and where it goes. */
export interface ValueDialogEdit {
  /** The cell's input now, or a loader for the stored row's exact text. */
  initial: CellInput | (() => Promise<CellInput>);
  nullable: boolean;
  onSave: (input: CellInput) => void;
}

export interface ValueDialogProps {
  column: ColumnInfo;
  /** The value: text, `null` (SQL NULL) or `undefined` (its row is loading). */
  value: string | null | undefined;
  bytes?: Uint8Array | null | undefined;
  engine?: Engine | null | undefined;
  /** Present when the cell is editable. */
  edit?: ValueDialogEdit | undefined;
  /** Open in edit mode ("Edit value…"). */
  startEditing?: boolean | undefined;
  onClose: () => void;
}

function textOf(input: CellInput): string {
  return input.type === "text" ? input.text : "";
}

export function ValueDialog({
  column,
  value,
  bytes,
  engine,
  edit,
  startEditing,
  onClose,
}: ValueDialogProps) {
  const [editing, setEditing] = useState(startEditing === true && edit !== undefined);
  const [text, setText] = useState<string | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const editorRef = useRef<SqlEditorHandle | null>(null);
  const label = `Value of ${column.name}`;

  // Editing starts from the exact stored text (the grid shows a formatted, shortened one).
  useEffect(() => {
    if (!editing || !edit || text !== null) return;
    const { initial } = edit;
    if (typeof initial !== "function") {
      setText(textOf(initial));
      return;
    }
    let alive = true;
    initial().then(
      (input) => alive && setText(textOf(input)),
      (error: unknown) => {
        if (!alive) return;
        setLoadError((error as { message?: string })?.message ?? String(error));
        setText(typeof value === "string" ? value : "");
      },
    );
    return () => {
      alive = false;
    };
  }, [editing, edit, text, value]);

  function save(input: CellInput) {
    edit?.onSave(input);
    onClose();
  }

  const footer = editing ? (
    <>
      <span className="value-dialog__hint">
        {loadError
          ? `Could not load the stored value: ${loadError}`
          : "Saved into the pending changes; Apply writes them."}
      </span>
      {edit?.nullable ? (
        <button type="button" className="button" onClick={() => save({ type: "null" })}>
          Set NULL
        </button>
      ) : null}
      <button type="button" className="button" onClick={() => setEditing(false)}>
        Cancel
      </button>
      <button
        type="button"
        className="button button--primary"
        disabled={text === null}
        onClick={() => save({ type: "text", text: text ?? "" })}
      >
        Save
      </button>
    </>
  ) : (
    <>
      {edit ? (
        <button type="button" className="button" onClick={() => setEditing(true)}>
          <Pencil {...ICON_SM} />
          Edit
        </button>
      ) : null}
      <button type="button" className="button button--primary" data-autofocus onClick={onClose}>
        Close
      </button>
    </>
  );

  return (
    <Modal
      title={`${column.name} · ${columnTypeLabel(column)}`}
      onClose={onClose}
      footer={footer}
      className="value-dialog"
    >
      <div className="value-dialog__view" data-testid="value-dialog">
        {editing ? (
          text === null ? (
            <p className="value-panel__empty">
              <LoaderCircle {...ICON_SM} className="spin" /> Loading…
            </p>
          ) : (
            <div className="value-dialog__edit">
              <SqlEditor
                initialValue={text}
                language="plaintext"
                editorRef={editorRef}
                onChange={setText}
                wordWrap
                ariaLabel={`Edit ${column.name}`}
                testId="value-dialog-editor"
              />
            </div>
          )
        ) : (
          <ValueView
            value={value}
            bytes={bytes}
            engine={engine}
            empty="Loading…"
            label={label}
            testId="value-dialog-text"
          />
        )}
      </div>
    </Modal>
  );
}
