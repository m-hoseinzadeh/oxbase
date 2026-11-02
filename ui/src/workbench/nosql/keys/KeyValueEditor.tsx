/**
 * The value editor of one Redis key: its header (type, TTL, size) with TTL / rename / delete,
 * and a typed editor per type. The value streams as an ordinary run (`key_value_open`: rows of
 * `value`, `field` / `value`, `member` / `score`, ...) and pages into the grid, so a list of
 * millions of items stays in bounded memory. Every change is one `key_apply` (atomic, with the
 * type the editor read: a key changed by someone else in between is a conflict).
 */
import {
  CircleAlert,
  CircleCheck,
  Clock,
  LoaderCircle,
  PencilLine,
  Plus,
  RefreshCw,
  Save,
  Trash2,
  TriangleAlert,
} from "lucide-react";
import { useEffect, useReducer, useRef, useState } from "react";
import {
  type BinaryText,
  displayBinary,
  type KeyAddress,
  type KeyApplyReport,
  type KeyEdit,
  type KeyInfo,
  keyApply,
  keyValueOpen,
  queryPage,
  utf8,
} from "../../../api";
import { ICON, ICON_SM } from "../../../components/icons";
import { Modal } from "../../../components/Modal";
import { toCommandError } from "../../../connections/api";
import { ResultPager } from "../../results/pager";
import { timedQueryPage } from "../../results/perf";
import { ResultGrid } from "../../results/ResultGrid";
import { type GridRowSelection, NO_ROWS } from "../../results/rowSelection";
import { IDLE_RUN, isActive, rowCountLabel, runReducer } from "../../results/runState";
import { closeEditorLater, editorReady } from "../editorClose";
import {
  cellBinary,
  cellValue,
  type ElementDraft,
  type ElementRow,
  type ElementType,
  EMPTY_ELEMENT,
  elementDraft,
  elementEdits,
  isElementType,
  KEY_TYPE_BADGE,
  KEY_TYPE_LABEL,
  removeEdit,
  sizeText,
  ttlInput,
  ttlText,
} from "./keyModel";

/** Most bytes of a string value edited in the text box. */
export const MAX_EDIT_BYTES = 1024 * 1024;

export interface KeyValueEditorProps {
  sessionId: string;
  /** The editor session the value reads run on (not the scan's). */
  editorId: string;
  address: KeyAddress;
  /** After an edit that committed. */
  onChanged: (change: KeyChange) => void;
}

/** What an applied edit did to the key. */
export type KeyChange =
  | { kind: "changed"; info: KeyInfo | null }
  | { kind: "renamed"; key: BinaryText; info: KeyInfo | null }
  | { kind: "deleted" };

/** The element in row `row` of a value page (`index` / `id` / `field` / `member` / ...). */
function elementOf(get: (name: string) => unknown): ElementRow {
  const out: ElementRow = {};
  const index = get("index");
  if (index !== null && index !== undefined) out.index = Number(index);
  const id = get("id");
  if (typeof id === "string") out.id = id;
  for (const name of ["field", "member", "value"] as const) {
    const v = cellBinary(get(name));
    if (v) out[name] = v;
  }
  const score = get("score");
  if (typeof score === "number") out.score = score;
  const fields = get("fields");
  if (typeof fields === "string") out.fields = fields;
  return out;
}

type Confirm = { title: string; text: string; action: string; run: () => void };

/** The user's words for a report that did not commit. */
export function keyReportProblem(report: KeyApplyReport): string | null {
  if (report.committed) return null;
  if (report.conflict) {
    return "Someone changed this key (or its type) after it was read, so nothing was written. Reload it and try again.";
  }
  return `Nothing was written: ${report.error ?? "the server refused the change"}`;
}

export function KeyValueEditor({ sessionId, editorId, address, onChanged }: KeyValueEditorProps) {
  const [run, dispatch] = useReducer(runReducer, IDLE_RUN);
  const [info, setInfo] = useState<KeyInfo | null>(null);
  const [openError, setOpenError] = useState<string | null>(null);
  const [selection, setSelection] = useState<GridRowSelection>(NO_ROWS);
  const [element, setElement] = useState<{ row: ElementRow | null; draft: ElementDraft } | null>(
    null,
  );
  const [stringValue, setStringValue] = useState<{
    text: string;
    binary: boolean;
    tooLarge: boolean;
  } | null>(null);
  const [problem, setProblem] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [confirm, setConfirm] = useState<Confirm | null>(null);
  const [dialog, setDialog] = useState<"ttl" | "rename" | null>(null);
  const [reload, setReload] = useState(0);
  const seq = useRef(0);
  const pagerRef = useRef<{ runId: string; pager: ResultPager } | null>(null);

  const keyLabel = displayBinary(address.key);

  // Open (and re-open after an edit) the key's value.
  useEffect(() => {
    void reload;
    seq.current += 1;
    const mine = seq.current;
    dispatch({ type: "reset" });
    dispatch({ type: "start", script: "" });
    setSelection(NO_ROWS);
    setElement(null);
    setStringValue(null);
    setOpenError(null);
    editorReady(editorId)
      .then(() => {
        if (mine !== seq.current) throw new Error("superseded");
        return keyValueOpen(sessionId, editorId, address, (event) => {
          if (mine === seq.current) dispatch({ type: "event", event });
        });
      })
      .then(
        (opened) => {
          if (mine !== seq.current) return;
          setInfo(opened.info);
          dispatch({ type: "runId", runId: opened.runId });
        },
        (error) => {
          if (mine !== seq.current) return;
          const err = toCommandError(error);
          setOpenError(err.category === "not_found" ? "This key no longer exists." : err.message);
          dispatch({ type: "startFailed", error: err });
        },
      );
  }, [sessionId, editorId, address, reload]);

  useEffect(
    () => () => {
      seq.current += 1;
      closeEditorLater(editorId);
      pagerRef.current?.pager.dispose();
    },
    [editorId],
  );

  const result = run.results[0];
  const runId = run.runId;
  let pager: ResultPager | null = null;
  if (runId && result) {
    if (pagerRef.current?.runId !== runId) {
      pagerRef.current?.pager.dispose();
      const index = result.resultIndex;
      pagerRef.current = {
        runId,
        pager: new ResultPager((offset, limit) => timedQueryPage(runId, index, offset, limit)),
      };
    }
    pager = pagerRef.current.pager;
  }

  const keyType = info?.keyType ?? "other";
  const done = result?.done === true;

  // A string (or JSON) value is read whole once streamed, up to MAX_EDIT_BYTES.
  useEffect(() => {
    if (!runId || !result || !done || (keyType !== "string" && keyType !== "json")) return;
    let alive = true;
    const rows = result.rowCount;
    queryPage(runId, result.resultIndex, 0, Math.max(rows, 1)).then(
      (table) => {
        if (!alive) return;
        const col = table.getChildAt(table.schema.fields.findIndex((f) => f.name === "value"));
        const parts: Uint8Array[] = [];
        let text = "";
        let total = 0;
        for (let i = 0; i < table.numRows; i += 1) {
          const v = cellValue(col, i);
          if (v instanceof Uint8Array) {
            parts.push(v);
            total += v.length;
          } else if (typeof v === "string") text += v;
        }
        if (parts.length > 0) {
          const bytes = new Uint8Array(total);
          let at = 0;
          for (const p of parts) {
            bytes.set(p, at);
            at += p.length;
          }
          const value = cellBinary(bytes);
          const binary = value?.encoding === "hex";
          setStringValue({
            text: value ? displayBinary(value) : "",
            binary,
            tooLarge: total > MAX_EDIT_BYTES,
          });
        } else {
          setStringValue({ text, binary: false, tooLarge: text.length > MAX_EDIT_BYTES });
        }
      },
      (error) => alive && setProblem(toCommandError(error).message),
    );
    return () => {
      alive = false;
    };
  }, [runId, result, done, keyType]);

  // The selected element: from the grid's loaded page, else read with one page request.
  const [currentRow, setCurrentRow] = useState<ElementRow | null>(null);
  const selectedIndex = selection.current;
  useEffect(() => {
    setCurrentRow(null);
    if (selectedIndex === null || !pager || !result) return;
    const columns = result.columns;
    const col = (name: string) => columns.findIndex((c) => c.name === name);
    const first = pager.cell(selectedIndex, 0);
    if (first) {
      setCurrentRow(
        elementOf((name) => {
          const c = col(name);
          const ref = c < 0 ? null : pager.cell(selectedIndex, c);
          return ref ? cellValue(ref.vector, ref.index) : null;
        }),
      );
      return;
    }
    let alive = true;
    pager.readRows(selectedIndex, 1).then(
      (table) => {
        if (!alive || table.numRows === 0) return;
        setCurrentRow(
          elementOf((name) => {
            const c = table.schema.fields.findIndex((f) => f.name === name);
            return c < 0 ? null : cellValue(table.getChildAt(c), 0);
          }),
        );
      },
      () => {},
    );
    return () => {
      alive = false;
    };
  }, [selectedIndex, pager, result]);

  async function apply(edits: KeyEdit[], done: string, destructive = false): Promise<boolean> {
    setBusy(true);
    setProblem(null);
    setNotice(null);
    try {
      const report = await keyApply(sessionId, {
        address,
        keyType,
        edits,
        ...(destructive ? { confirmDestructive: true } : {}),
      });
      const why = keyReportProblem(report);
      if (why) {
        setProblem(why);
        return false;
      }
      setNotice(done);
      const renamed = edits.find((e) => e.type === "rename");
      if (edits.some((e) => e.type === "delete")) {
        onChanged({ kind: "deleted" });
      } else if (renamed?.type === "rename") {
        onChanged({ kind: "renamed", key: renamed.newKey, info: report.info ?? null });
      } else {
        onChanged({ kind: "changed", info: report.info ?? null });
        setReload((n) => n + 1);
      }
      return true;
    } catch (error) {
      setProblem(toCommandError(error).message);
      return false;
    } finally {
      setBusy(false);
    }
  }

  function saveElement() {
    if (!element || !isElementType(keyType)) return;
    let edits: KeyEdit[];
    try {
      edits = elementEdits(keyType, element.row, element.draft);
    } catch (error) {
      setProblem((error as Error).message);
      return;
    }
    if (edits.length === 0) {
      setNotice("Nothing changed.");
      return;
    }
    void apply(edits, element.row ? "Saved." : "Added.").then((ok) => ok && setElement(null));
  }

  function removeSelected(type: ElementType) {
    const edit = currentRow ? removeEdit(type, currentRow) : null;
    if (!edit) return;
    void apply([edit], "Removed.");
  }

  function deleteKey() {
    setConfirm({
      title: "Delete this key?",
      text: `The key ${keyLabel} and its whole value are deleted (UNLINK). This cannot be undone.`,
      action: "Delete key",
      run: () => void apply([{ type: "delete" }], "Deleted the key.", true),
    });
  }

  const active = isActive(run.status);
  const disabled = busy || active || !info;
  /** TTL, rename and delete do not need the value: only an edit in flight blocks them. */
  const keyDisabled = busy || !info;

  return (
    <section className="key-editor" aria-label={`Key ${keyLabel}`} data-testid="key-editor">
      <header className="key-editor__head">
        <span className={`key-type key-type--${keyType}`} title={KEY_TYPE_LABEL[keyType]}>
          {KEY_TYPE_BADGE[keyType]}
        </span>
        <h3 className="key-editor__name" title={keyLabel}>
          {keyLabel}
        </h3>
        <span className="key-editor__meta" data-testid="key-meta">
          {info ? (
            <>
              <span>{KEY_TYPE_LABEL[info.keyType]}</span>
              <span>
                <Clock {...ICON_SM} aria-hidden="true" /> {ttlText(info.ttlMs)}
              </span>
              {sizeText(info) ? <span>{sizeText(info)}</span> : null}
              {info.encoding ? <span>{info.encoding}</span> : null}
            </>
          ) : null}
        </span>
        <span className="key-editor__actions">
          <button
            type="button"
            className="icon-button"
            aria-label="Reload the key"
            title="Reload the key"
            disabled={busy}
            onClick={() => setReload((n) => n + 1)}
          >
            <RefreshCw {...ICON} />
          </button>
          <button
            type="button"
            className="button button--ghost button--small"
            disabled={keyDisabled}
            onClick={() => setDialog("ttl")}
          >
            <Clock {...ICON_SM} />
            TTL…
          </button>
          <button
            type="button"
            className="button button--ghost button--small"
            disabled={keyDisabled}
            onClick={() => setDialog("rename")}
          >
            <PencilLine {...ICON_SM} />
            Rename…
          </button>
          <button
            type="button"
            className="button button--ghost button--small table-data__delete"
            data-testid="key-delete"
            disabled={keyDisabled}
            onClick={deleteKey}
          >
            <Trash2 {...ICON_SM} />
            Delete
          </button>
        </span>
      </header>
      {notice ? (
        <div className="banner banner--ok" role="status">
          <CircleCheck {...ICON_SM} className="banner__icon" />
          <p>{notice}</p>
        </div>
      ) : null}
      {problem ? (
        <div className="banner banner--error" role="alert" data-testid="key-error">
          <CircleAlert {...ICON_SM} className="banner__icon" />
          <p>{problem}</p>
        </div>
      ) : null}
      <div className="key-editor__body">
        {openError ? (
          <div className="empty-state" role="alert">
            <span className="empty-state__icon">
              <CircleAlert {...ICON} />
            </span>
            <p className="empty-state__title">Could not open the key</p>
            <p className="empty-state__hint">{openError}</p>
          </div>
        ) : !info || (!result && active) ? (
          <div className="table-data__loading" role="status">
            <LoaderCircle {...ICON} className="spin" />
            Loading the value…
          </div>
        ) : keyType === "string" || keyType === "json" ? (
          <TextValue
            key={`${runId}`}
            type={keyType}
            value={stringValue}
            disabled={disabled}
            onSave={(text) => {
              if (keyType === "json") {
                let value: unknown;
                try {
                  value = JSON.parse(text);
                } catch {
                  setProblem("The value is not valid JSON.");
                  return;
                }
                void apply([{ type: "jsonSet", path: "$", value: value as never }], "Saved.");
              } else {
                void apply([{ type: "setString", value: utf8(text), keepTtl: true }], "Saved.");
              }
            }}
          />
        ) : isElementType(keyType) && result && pager ? (
          <div className="key-editor__elements">
            <div className="key-editor__grid-bar">
              <span className="key-editor__count" data-testid="value-count">
                {rowCountLabel(result)}
              </span>
              <button
                type="button"
                className="button button--ghost button--small"
                disabled={disabled}
                onClick={() => setElement({ row: null, draft: EMPTY_ELEMENT })}
              >
                <Plus {...ICON_SM} />
                Add
              </button>
              <button
                type="button"
                className="button button--ghost button--small"
                disabled={disabled || !currentRow || keyType === "stream"}
                onClick={() =>
                  currentRow && setElement({ row: currentRow, draft: elementDraft(currentRow) })
                }
              >
                <PencilLine {...ICON_SM} />
                Edit
              </button>
              <button
                type="button"
                className="button button--ghost button--small table-data__delete"
                disabled={disabled || !currentRow}
                onClick={() => removeSelected(keyType)}
              >
                <Trash2 {...ICON_SM} />
                Remove
              </button>
            </div>
            <div className="results__grid">
              <ResultGrid
                key={runId ?? ""}
                pager={pager}
                columns={result.columns}
                rowCount={result.rowCount}
                onSelectRows={setSelection}
                selectedRows={selection}
              />
            </div>
            {element ? (
              <ElementForm
                type={keyType}
                isNew={element.row === null}
                draft={element.draft}
                disabled={busy}
                onChange={(draft) => setElement({ ...element, draft })}
                onSave={saveElement}
                onCancel={() => setElement(null)}
              />
            ) : null}
          </div>
        ) : (
          <p className="results__empty">Values of this type cannot be shown here.</p>
        )}
      </div>
      {dialog === "ttl" && info ? (
        <TtlDialog
          info={info}
          onCancel={() => setDialog(null)}
          onSave={(ttlMs) =>
            void apply(
              [ttlMs === undefined ? { type: "expire" } : { type: "expire", ttlMs }],
              ttlMs === undefined
                ? "The key no longer expires."
                : `The key expires in ${ttlText(ttlMs)}.`,
            ).then((ok) => ok && setDialog(null))
          }
        />
      ) : null}
      {dialog === "rename" ? (
        <RenameDialog
          current={keyLabel}
          onCancel={() => setDialog(null)}
          onSave={(name, overwrite) => {
            const run = () =>
              void apply(
                [{ type: "rename", newKey: utf8(name), ...(overwrite ? { overwrite: true } : {}) }],
                `Renamed to ${name}.`,
                overwrite,
              ).then((ok) => ok && setDialog(null));
            if (!overwrite) {
              run();
              return;
            }
            setConfirm({
              title: "Replace an existing key?",
              text: `If a key named ${name} exists, it is replaced by this one and its value is lost.`,
              action: "Rename and replace",
              run,
            });
          }}
        />
      ) : null}
      {confirm ? <ConfirmDialog confirm={confirm} onClose={() => setConfirm(null)} /> : null}
    </section>
  );
}

export function ConfirmDialog({ confirm, onClose }: { confirm: Confirm; onClose: () => void }) {
  return (
    <Modal
      title={confirm.title}
      onClose={onClose}
      footer={
        <>
          <button type="button" className="button" onClick={onClose}>
            Cancel
          </button>
          <button
            type="button"
            className="button button--danger"
            data-testid="confirm-action"
            data-autofocus
            onClick={() => {
              onClose();
              confirm.run();
            }}
          >
            {confirm.action}
          </button>
        </>
      }
    >
      <p className="confirm__text">
        <TriangleAlert {...ICON_SM} className="confirm__icon" />
        {confirm.text}
      </p>
    </Modal>
  );
}

function TextValue({
  type,
  value,
  disabled,
  onSave,
}: {
  type: "string" | "json";
  value: { text: string; binary: boolean; tooLarge: boolean } | null;
  disabled: boolean;
  onSave: (text: string) => void;
}) {
  const [text, setText] = useState<string | null>(null);
  if (!value) {
    return (
      <div className="table-data__loading" role="status">
        <LoaderCircle {...ICON} className="spin" />
        Loading the value…
      </div>
    );
  }
  const readOnly = value.binary || value.tooLarge;
  const shown = text ?? (type === "json" ? prettyJsonText(value.text) : value.text);
  return (
    <div className="key-editor__text">
      {value.binary ? (
        <p className="field__hint">
          The value is binary: its bytes are shown escaped (\xHH) and it cannot be edited as text.
        </p>
      ) : null}
      {value.tooLarge ? (
        <p className="field__hint">The value is larger than 1 MiB: it is shown read-only.</p>
      ) : null}
      <textarea
        className="key-editor__textarea"
        aria-label="Value"
        data-testid="string-value"
        spellCheck={false}
        readOnly={readOnly}
        value={shown}
        onChange={(e) => setText(e.target.value)}
      />
      <div className="key-editor__text-actions">
        <button
          type="button"
          className="button button--primary button--small"
          data-testid="string-save"
          disabled={disabled || readOnly || text === null}
          onClick={() => text !== null && onSave(text)}
        >
          <Save {...ICON_SM} />
          Save
        </button>
        <button
          type="button"
          className="button button--ghost button--small"
          disabled={text === null}
          onClick={() => setText(null)}
        >
          Revert
        </button>
      </div>
    </div>
  );
}

function prettyJsonText(text: string): string {
  try {
    return JSON.stringify(JSON.parse(text), null, 2);
  } catch {
    return text;
  }
}

const FORM_FIELDS: Record<ElementType, { name: keyof ElementDraft; label: string }[]> = {
  hash: [
    { name: "field", label: "Field" },
    { name: "value", label: "Value" },
  ],
  list: [{ name: "value", label: "Value" }],
  set: [{ name: "member", label: "Member" }],
  zset: [
    { name: "member", label: "Member" },
    { name: "score", label: "Score" },
  ],
  stream: [{ name: "fields", label: "Fields (JSON object)" }],
};

function ElementForm({
  type,
  isNew,
  draft,
  disabled,
  onChange,
  onSave,
  onCancel,
}: {
  type: ElementType;
  isNew: boolean;
  draft: ElementDraft;
  disabled: boolean;
  onChange: (draft: ElementDraft) => void;
  onSave: () => void;
  onCancel: () => void;
}) {
  return (
    <form
      className="element-form"
      aria-label={isNew ? "New element" : "Edit element"}
      data-testid="element-form"
      onSubmit={(e) => {
        e.preventDefault();
        onSave();
      }}
    >
      {FORM_FIELDS[type].map((f) => {
        const id = `element-${f.name}`;
        return (
          <div key={f.name} className="element-form__field">
            <label htmlFor={id}>{f.label}</label>
            {f.name === "value" || f.name === "fields" ? (
              <textarea
                id={id}
                rows={f.name === "fields" ? 3 : 2}
                spellCheck={false}
                value={draft[f.name]}
                onChange={(e) => onChange({ ...draft, [f.name]: e.target.value })}
              />
            ) : (
              <input
                id={id}
                type="text"
                spellCheck={false}
                inputMode={f.name === "score" ? "decimal" : undefined}
                value={draft[f.name]}
                onChange={(e) => onChange({ ...draft, [f.name]: e.target.value })}
              />
            )}
          </div>
        );
      })}
      {type === "list" && isNew ? (
        <label className="element-form__field">
          <span>Add at</span>
          <select
            value={draft.end}
            onChange={(e) => onChange({ ...draft, end: e.target.value as "head" | "tail" })}
          >
            <option value="tail">End (RPUSH)</option>
            <option value="head">Start (LPUSH)</option>
          </select>
        </label>
      ) : null}
      <div className="element-form__actions">
        <button
          type="submit"
          className="button button--primary button--small"
          data-testid="element-save"
          disabled={disabled}
        >
          <Save {...ICON_SM} />
          {isNew ? "Add" : "Save"}
        </button>
        <button type="button" className="button button--ghost button--small" onClick={onCancel}>
          Cancel
        </button>
      </div>
    </form>
  );
}

function TtlDialog({
  info,
  onCancel,
  onSave,
}: {
  info: KeyInfo;
  onCancel: () => void;
  onSave: (ttlMs: number | undefined) => void;
}) {
  const [text, setText] = useState(
    info.ttlMs === undefined ? "" : String(Math.max(1, Math.round(info.ttlMs / 1000))),
  );
  const [problem, setProblem] = useState<string | null>(null);
  function save() {
    try {
      onSave(ttlInput(text));
    } catch (error) {
      setProblem((error as Error).message);
    }
  }
  return (
    <Modal
      title="Time to live"
      onClose={onCancel}
      footer={
        <>
          <button type="button" className="button" onClick={onCancel}>
            Cancel
          </button>
          <button type="button" className="button button--primary" onClick={save}>
            Save
          </button>
        </>
      }
    >
      <div className={`field${problem ? " field--invalid" : ""}`}>
        <label htmlFor="key-ttl">Expire after (seconds)</label>
        <input
          id="key-ttl"
          type="text"
          inputMode="numeric"
          data-autofocus
          value={text}
          aria-invalid={Boolean(problem)}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") save();
          }}
        />
        <p className="field__hint">Leave blank to keep the key forever (PERSIST).</p>
        {problem ? (
          <p className="field__error" role="alert">
            {problem}
          </p>
        ) : null}
      </div>
    </Modal>
  );
}

function RenameDialog({
  current,
  onCancel,
  onSave,
}: {
  current: string;
  onCancel: () => void;
  onSave: (name: string, overwrite: boolean) => void;
}) {
  const [name, setName] = useState(current);
  const [overwrite, setOverwrite] = useState(false);
  const valid = name !== "" && name !== current;
  return (
    <Modal
      title="Rename key"
      onClose={onCancel}
      footer={
        <>
          <button type="button" className="button" onClick={onCancel}>
            Cancel
          </button>
          <button
            type="button"
            className="button button--primary"
            disabled={!valid}
            onClick={() => onSave(name, overwrite)}
          >
            Rename
          </button>
        </>
      }
    >
      <div className="field">
        <label htmlFor="key-rename">New name</label>
        <input
          id="key-rename"
          type="text"
          spellCheck={false}
          data-autofocus
          value={name}
          onChange={(e) => setName(e.target.value)}
        />
      </div>
      <label className="checkbox">
        <input
          type="checkbox"
          checked={overwrite}
          onChange={(e) => setOverwrite(e.target.checked)}
        />
        Replace a key that already has this name
      </label>
      <p className="field__hint">Without it, the rename fails when the name is taken (RENAMENX).</p>
    </Modal>
  );
}
