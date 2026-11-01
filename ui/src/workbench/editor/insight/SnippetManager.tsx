/**
 * The snippet manager (docs/m6-contracts.md §6): the user's snippets and the built-in ones,
 * with add, edit, duplicate, delete and insert. Built-in snippets are read-only. Snippets show
 * in completion at statement starts by their prefix.
 */
import {
  type FormEvent,
  type ReactNode,
  useCallback,
  useEffect,
  useId,
  useMemo,
  useState,
} from "react";
import {
  type Engine,
  type Snippet,
  type SnippetInput,
  snippetDelete,
  snippetList,
  snippetSave,
} from "../../../api";
import { Modal } from "../../../components/Modal";
import { toCommandError } from "../../../connections/api";
import { ENGINE_INFO } from "../../../connections/types";
import { INSIGHT_ENGINES } from "./controller";

/** The editable fields of a snippet. */
export interface SnippetDraft {
  id: string | null;
  name: string;
  prefix: string;
  description: string;
  body: string;
  engines: Engine[];
}

export type DraftErrors = Partial<Record<"name" | "prefix" | "description" | "body", string>>;

const PREFIX = /^[A-Za-z0-9_-]+$/;
// Monaco snippet syntax shown in the form's hint (`\u007b` is `{`).
const STOP_DEFAULT = "$\u007b1:default}";
const STOP_CHOICE = "$\u007b1|a,b|}";

/** The rules of `SnippetInput::validate` the form can check before saving. */
export function validateDraft(draft: SnippetDraft): DraftErrors {
  const errors: DraftErrors = {};
  const name = draft.name.trim();
  if (name.length === 0) errors.name = "Enter a name.";
  else if (name.length > 100) errors.name = "At most 100 characters.";
  const prefix = draft.prefix.trim();
  if (prefix.length === 0) errors.prefix = "Enter the prefix you type to insert it.";
  else if (prefix.length > 40) errors.prefix = "At most 40 characters.";
  else if (!PREFIX.test(prefix)) errors.prefix = "Letters, digits, _ and - only.";
  if (draft.description.length > 500) errors.description = "At most 500 characters.";
  if (draft.body.trim().length === 0) errors.body = "Enter the SQL to insert.";
  else if (draft.body.length > 10_000) errors.body = "At most 10,000 characters.";
  return errors;
}

/** The `snippet_save` payload of a draft. */
export function draftInput(draft: SnippetDraft): SnippetInput {
  return {
    ...(draft.id ? { id: draft.id } : {}),
    name: draft.name.trim(),
    prefix: draft.prefix.trim(),
    body: draft.body,
    description: draft.description.trim(),
    engines: draft.engines,
  };
}

function draftOf(snippet: Snippet | null, engine: Engine | null): SnippetDraft {
  if (!snippet) {
    return {
      id: null,
      name: "",
      prefix: "",
      description: "",
      body: "",
      engines: engine ? [engine] : [],
    };
  }
  return {
    id: snippet.builtin ? null : snippet.id,
    name: snippet.builtin ? `${snippet.name} (copy)` : snippet.name,
    prefix: snippet.prefix,
    description: snippet.description,
    body: snippet.body,
    engines: [...snippet.engines],
  };
}

function enginesText(engines: Engine[]): string {
  return engines.length === 0 ? "All engines" : engines.map((e) => ENGINE_INFO[e].label).join(", ");
}

type Selection = { type: "snippet"; id: string } | { type: "new" } | null;

export interface SnippetManagerProps {
  /** The editor's engine: new snippets default to it and its snippets are listed first. */
  engine: Engine | null;
  onClose(): void;
  /** Inserts a body at the editor's cursor; the button is hidden without it. */
  onInsert?: ((body: string) => void) | undefined;
}

export function SnippetManager({ engine, onClose, onInsert }: SnippetManagerProps) {
  const [snippets, setSnippets] = useState<Snippet[] | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [selection, setSelection] = useState<Selection>(null);
  const [draft, setDraft] = useState<SnippetDraft | null>(null);
  const [errors, setErrors] = useState<DraftErrors>({});
  const [saveError, setSaveError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const formId = useId();

  const load = useCallback(async (select?: string) => {
    try {
      const list = await snippetList();
      setSnippets(list);
      setLoadError(null);
      if (select) setSelection({ type: "snippet", id: select });
    } catch (error) {
      setLoadError(toCommandError(error).message);
      setSnippets([]);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const ordered = useMemo(() => {
    const list = snippets ?? [];
    const applies = (s: Snippet) => !engine || s.engines.length === 0 || s.engines.includes(engine);
    const mine = list.filter((s) => !s.builtin);
    const builtin = list.filter((s) => s.builtin && applies(s));
    return { mine, builtin };
  }, [snippets, engine]);

  const selected =
    selection?.type === "snippet" ? (snippets?.find((s) => s.id === selection.id) ?? null) : null;

  function select(next: Selection) {
    setSelection(next);
    setConfirmDelete(false);
    setErrors({});
    setSaveError(null);
    if (next?.type === "new") setDraft(draftOf(null, engine));
    else {
      const snippet = next ? (snippets?.find((s) => s.id === next.id) ?? null) : null;
      setDraft(snippet && !snippet.builtin ? draftOf(snippet, engine) : null);
    }
  }

  function duplicate(snippet: Snippet) {
    setSelection({ type: "new" });
    setDraft(draftOf(snippet, engine));
    setErrors({});
    setSaveError(null);
    setConfirmDelete(false);
  }

  async function save(event: FormEvent) {
    event.preventDefault();
    if (!draft) return;
    const found = validateDraft(draft);
    setErrors(found);
    if (Object.keys(found).length > 0) return;
    setBusy(true);
    setSaveError(null);
    try {
      const saved = await snippetSave(draftInput(draft));
      await load(saved.id);
      setDraft(draftOf(saved, engine));
    } catch (error) {
      setSaveError(toCommandError(error).message);
    } finally {
      setBusy(false);
    }
  }

  async function remove(snippet: Snippet) {
    if (!confirmDelete) {
      setConfirmDelete(true);
      return;
    }
    setBusy(true);
    setSaveError(null);
    try {
      await snippetDelete(snippet.id);
      setSelection(null);
      setDraft(null);
      setConfirmDelete(false);
      await load();
    } catch (error) {
      setSaveError(toCommandError(error).message);
    } finally {
      setBusy(false);
    }
  }

  function edit<K extends keyof SnippetDraft>(key: K, value: SnippetDraft[K]) {
    setDraft((d) => (d ? { ...d, [key]: value } : d));
  }

  function toggleEngine(e: Engine, on: boolean) {
    setDraft((d) =>
      d
        ? {
            ...d,
            engines: on
              ? INSIGHT_ENGINES.filter((x) => x === e || d.engines.includes(x))
              : d.engines.filter((x) => x !== e),
          }
        : d,
    );
  }

  function item(snippet: Snippet) {
    const active = selection?.type === "snippet" && selection.id === snippet.id;
    return (
      <li key={snippet.id}>
        <button
          type="button"
          aria-pressed={active}
          className={`snippets__item${active ? " snippets__item--active" : ""}`}
          onClick={() => select({ type: "snippet", id: snippet.id })}
        >
          <span className="snippets__name">{snippet.name}</span>
          <code className="snippets__prefix">{snippet.prefix}</code>
        </button>
      </li>
    );
  }

  const fieldError = (key: keyof DraftErrors) =>
    errors[key] ? (
      <p className="field__error" id={`${formId}-${key}-error`}>
        {errors[key]}
      </p>
    ) : null;

  let detail: ReactNode;
  if (draft) {
    detail = (
      <form id={formId} className="snippets__form" onSubmit={(e) => void save(e)} noValidate>
        <div className="field-row">
          <div className={`field${errors.name ? " field--invalid" : ""}`}>
            <label htmlFor={`${formId}-name`}>Name</label>
            <input
              id={`${formId}-name`}
              value={draft.name}
              maxLength={100}
              onChange={(e) => edit("name", e.target.value)}
              data-autofocus
            />
            {fieldError("name")}
          </div>
          <div className={`field${errors.prefix ? " field--invalid" : ""}`}>
            <label htmlFor={`${formId}-prefix`}>Prefix</label>
            <input
              id={`${formId}-prefix`}
              value={draft.prefix}
              maxLength={40}
              spellCheck={false}
              onChange={(e) => edit("prefix", e.target.value)}
            />
            {fieldError("prefix")}
          </div>
        </div>
        <div className={`field${errors.description ? " field--invalid" : ""}`}>
          <label htmlFor={`${formId}-description`}>Description</label>
          <input
            id={`${formId}-description`}
            value={draft.description}
            maxLength={500}
            onChange={(e) => edit("description", e.target.value)}
          />
          {fieldError("description")}
        </div>
        <div className={`field${errors.body ? " field--invalid" : ""}`}>
          <label htmlFor={`${formId}-body`}>Body</label>
          <textarea
            id={`${formId}-body`}
            className="snippets__body"
            value={draft.body}
            rows={8}
            spellCheck={false}
            onChange={(e) => edit("body", e.target.value)}
          />
          <p className="field__hint">
            Tab stops: <code>$1</code>, <code>{STOP_DEFAULT}</code>, <code>{STOP_CHOICE}</code>; the
            cursor ends at <code>$0</code>; <code>\$</code> is a literal dollar sign.
          </p>
          {fieldError("body")}
        </div>
        <fieldset className="snippets__engines">
          <legend>Engines (none checked: all)</legend>
          {INSIGHT_ENGINES.map((e) => (
            <label key={e} className="checkbox">
              <input
                type="checkbox"
                checked={draft.engines.includes(e)}
                onChange={(event) => toggleEngine(e, event.target.checked)}
              />
              {ENGINE_INFO[e].label}
            </label>
          ))}
        </fieldset>
        {saveError ? (
          <p className="field__error" role="alert">
            {saveError}
          </p>
        ) : null}
      </form>
    );
  } else if (selected) {
    detail = (
      <div className="snippets__view">
        <h3 className="snippets__title">{selected.name}</h3>
        <p className="field__hint">
          Prefix <code>{selected.prefix}</code> · {enginesText(selected.engines)}
          {selected.builtin ? " · built in" : ""}
        </p>
        {selected.description ? <p>{selected.description}</p> : null}
        <pre className="snippets__preview">{selected.body}</pre>
      </div>
    );
  } else {
    detail = (
      <p className="snippets__empty">
        Choose a snippet, or add your own. Type a snippet's prefix at the start of a statement to
        insert it.
      </p>
    );
  }

  const footer = (
    <>
      <span className="modal__footer-start">
        <button
          type="button"
          className="button"
          onClick={() => select({ type: "new" })}
          disabled={busy}
        >
          New snippet
        </button>
      </span>
      {selected && onInsert ? (
        <button
          type="button"
          className="button"
          onClick={() => {
            onInsert(selected.body);
            onClose();
          }}
        >
          Insert
        </button>
      ) : null}
      {selected?.builtin ? (
        <button type="button" className="button" onClick={() => duplicate(selected)}>
          Duplicate
        </button>
      ) : null}
      {selected && !selected.builtin ? (
        <button
          type="button"
          className="button button--danger"
          onClick={() => void remove(selected)}
          disabled={busy}
        >
          {confirmDelete ? "Confirm delete" : "Delete"}
        </button>
      ) : null}
      {draft ? (
        <button type="submit" form={formId} className="button button--primary" disabled={busy}>
          Save
        </button>
      ) : null}
      <button type="button" className="button" onClick={onClose}>
        Close
      </button>
    </>
  );

  return (
    <Modal title="Snippets" onClose={onClose} className="modal--snippets" footer={footer}>
      <div className="snippets">
        <div className="snippets__lists">
          {loadError ? (
            <p className="field__error" role="alert">
              Could not load snippets: {loadError}
            </p>
          ) : null}
          {snippets === null ? <p className="snippets__empty">Loading…</p> : null}
          <h3 className="snippets__heading">My snippets</h3>
          {ordered.mine.length === 0 && snippets !== null ? (
            <p className="snippets__empty">None yet.</p>
          ) : (
            <ul aria-label="My snippets" className="snippets__list">
              {ordered.mine.map(item)}
            </ul>
          )}
          <h3 className="snippets__heading">Built in</h3>
          <ul aria-label="Built-in snippets" className="snippets__list">
            {ordered.builtin.map(item)}
          </ul>
        </div>
        <div className="snippets__detail">{detail}</div>
      </div>
    </Modal>
  );
}
