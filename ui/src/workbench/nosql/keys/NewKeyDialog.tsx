/** Creates a Redis key with its first value (`key_apply` with `create`: the key must be new). */
import { useState } from "react";
import { type KeyEdit, type KeyInfo, type KeyType, keyApply, utf8 } from "../../../api";
import { Modal } from "../../../components/Modal";
import { toCommandError } from "../../../connections/api";
import {
  createEdit,
  type ElementDraft,
  EMPTY_ELEMENT,
  KEY_TYPE_LABEL,
  KEY_TYPES,
  ttlInput,
} from "./keyModel";

type NewType = Exclude<KeyType, "other">;

const VALUE_FIELDS: Record<NewType, { name: keyof ElementDraft; label: string; area?: boolean }[]> =
  {
    string: [{ name: "value", label: "Value", area: true }],
    json: [{ name: "value", label: "JSON value", area: true }],
    hash: [
      { name: "field", label: "First field" },
      { name: "value", label: "Its value", area: true },
    ],
    list: [{ name: "value", label: "First item", area: true }],
    set: [{ name: "member", label: "First member" }],
    zset: [
      { name: "member", label: "First member" },
      { name: "score", label: "Its score" },
    ],
    stream: [{ name: "fields", label: "First entry (JSON object)", area: true }],
  };

export interface NewKeyDialogProps {
  sessionId: string;
  database: string;
  /** Prefilled name (the namespace being browsed). */
  prefix: string;
  onClose: () => void;
  onCreated: (name: string, info: KeyInfo | null) => void;
}

export function NewKeyDialog({
  sessionId,
  database,
  prefix,
  onClose,
  onCreated,
}: NewKeyDialogProps) {
  const [name, setName] = useState(prefix);
  const [type, setType] = useState<NewType>("string");
  const [draft, setDraft] = useState<ElementDraft>({ ...EMPTY_ELEMENT, value: "" });
  const [ttl, setTtl] = useState("");
  const [problem, setProblem] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);

  async function create() {
    if (name === "") {
      setProblem("Enter the key's name.");
      return;
    }
    const edits: KeyEdit[] = [];
    try {
      edits.push(
        createEdit(type, type === "json" && draft.value === "" ? { ...draft, value: "{}" } : draft),
      );
      const ttlMs = ttlInput(ttl);
      if (ttlMs !== undefined) edits.push({ type: "expire", ttlMs });
    } catch (error) {
      setProblem((error as Error).message);
      return;
    }
    setProblem(null);
    setSaving(true);
    try {
      const report = await keyApply(sessionId, {
        address: { database, key: utf8(name) },
        create: true,
        edits,
      });
      if (!report.committed) {
        setProblem(
          report.conflict
            ? "A key with this name exists already: nothing was written."
            : `Nothing was written: ${report.error ?? "the server refused the key"}`,
        );
        return;
      }
      onCreated(name, report.info ?? null);
    } catch (error) {
      setProblem(toCommandError(error).message);
    } finally {
      setSaving(false);
    }
  }

  return (
    <Modal
      title={`New key in database ${database}`}
      onClose={onClose}
      footer={
        <>
          <button type="button" className="button" onClick={onClose}>
            Cancel
          </button>
          <button
            type="button"
            className="button button--primary"
            data-testid="new-key-create"
            disabled={saving}
            onClick={() => void create()}
          >
            Create
          </button>
        </>
      }
    >
      <div className="field-row">
        <div className="field">
          <label htmlFor="new-key-name">Key name</label>
          <input
            id="new-key-name"
            type="text"
            spellCheck={false}
            data-autofocus
            value={name}
            onChange={(e) => setName(e.target.value)}
          />
        </div>
        <div className="field">
          <label htmlFor="new-key-type">Type</label>
          <select
            id="new-key-type"
            value={type}
            onChange={(e) => setType(e.target.value as NewType)}
          >
            {KEY_TYPES.map((t) => (
              <option key={t} value={t}>
                {KEY_TYPE_LABEL[t]}
              </option>
            ))}
          </select>
        </div>
      </div>
      {VALUE_FIELDS[type].map((f) => (
        <div key={f.name} className="field">
          <label htmlFor={`new-key-${f.name}`}>{f.label}</label>
          {f.area ? (
            <textarea
              id={`new-key-${f.name}`}
              rows={3}
              spellCheck={false}
              value={draft[f.name]}
              onChange={(e) => setDraft({ ...draft, [f.name]: e.target.value })}
            />
          ) : (
            <input
              id={`new-key-${f.name}`}
              type="text"
              spellCheck={false}
              value={draft[f.name]}
              onChange={(e) => setDraft({ ...draft, [f.name]: e.target.value })}
            />
          )}
        </div>
      ))}
      <div className="field">
        <label htmlFor="new-key-ttl">Expire after (seconds, optional)</label>
        <input
          id="new-key-ttl"
          type="text"
          inputMode="numeric"
          className="input--port"
          value={ttl}
          onChange={(e) => setTtl(e.target.value)}
        />
      </div>
      {problem ? (
        <p className="field__error" role="alert">
          {problem}
        </p>
      ) : null}
    </Modal>
  );
}
