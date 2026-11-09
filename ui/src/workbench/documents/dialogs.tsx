/**
 * 1.3: the small dialogs of query files and saved queries, shown one at a time by
 * `DocumentDialogsHost` and asked for with promises: unsaved changes (Save / Don't save /
 * Cancel), the saved query name, rename, delete, and file errors or notices.
 */
import { CircleAlert, FileWarning, Info, TriangleAlert } from "lucide-react";
import { type FormEvent, useEffect, useId, useState } from "react";
import { ICON_SM } from "../../components/icons";
import { Modal } from "../../components/Modal";

export type UnsavedAnswer = "save" | "discard" | "cancel";

/** What the save query dialog ended with. */
export type SaveQueryAnswer = "saved" | "file" | "cancel";

type Request = { id: number } & RequestBody;

type RequestBody =
  | { kind: "message"; title: string; text: string; tone: "error" | "info"; done: () => void }
  | { kind: "unsaved"; names: string[]; done: (answer: UnsavedAnswer) => void }
  | {
      kind: "name";
      title: string;
      label: string;
      initial: string;
      action: string;
      /** A note under the field (where the query is saved). */
      note?: string;
      /** Offer "Save to a file instead…". */
      offerFile?: boolean;
      /** Saves; resolves to an error to show, or `null` when done. */
      submit: (name: string) => Promise<string | null>;
      done: (answer: SaveQueryAnswer) => void;
    }
  | { kind: "confirm"; title: string; text: string; action: string; done: (ok: boolean) => void };

let queue: Request[] = [];
let nextId = 0;
const listeners = new Set<() => void>();

function push(body: RequestBody) {
  nextId += 1;
  const request = { id: nextId, ...body } as Request;
  queue = [...queue, request];
  for (const listener of listeners) listener();
}

function pop() {
  queue = queue.slice(1);
  for (const listener of listeners) listener();
}

/** Shows why a file could not be opened or saved. */
export function showFileError(title: string, text: string): Promise<void> {
  return new Promise((resolve) =>
    push({ kind: "message", title, text, tone: "error", done: resolve }),
  );
}

/** Shows a notice. */
export function showNotice(title: string, text: string): Promise<void> {
  return new Promise((resolve) =>
    push({ kind: "message", title, text, tone: "info", done: resolve }),
  );
}

/** Asks what to do with the unsaved changes of `names`. */
export function askUnsaved(names: string[]): Promise<UnsavedAnswer> {
  return new Promise((resolve) => push({ kind: "unsaved", names, done: resolve }));
}

/** Asks for a name and saves with `submit` until it succeeds or the user cancels. */
export function askName(options: {
  title: string;
  label: string;
  initial: string;
  action: string;
  note?: string;
  offerFile?: boolean;
  submit: (name: string) => Promise<string | null>;
}): Promise<SaveQueryAnswer> {
  return new Promise((resolve) => push({ kind: "name", ...options, done: resolve }));
}

/** Asks to confirm a destructive action. */
export function askConfirm(title: string, text: string, action: string): Promise<boolean> {
  return new Promise((resolve) => push({ kind: "confirm", title, text, action, done: resolve }));
}

function NameDialog({ request }: { request: Extract<Request, { kind: "name" }> }) {
  const [name, setName] = useState(request.initial);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const formId = useId();
  const fieldId = useId();

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (busy) return;
    if (!name.trim()) {
      setError("Enter a name.");
      return;
    }
    setBusy(true);
    const failure = await request.submit(name.trim());
    setBusy(false);
    if (failure) {
      setError(failure);
      return;
    }
    pop();
    request.done("saved");
  }

  function finish(answer: SaveQueryAnswer) {
    pop();
    request.done(answer);
  }

  return (
    <Modal
      title={request.title}
      onClose={() => finish("cancel")}
      footer={
        <>
          {request.offerFile ? (
            <button
              type="button"
              className="button button--ghost modal__footer-start"
              data-testid="save-to-file"
              onClick={() => finish("file")}
            >
              Save to a file instead…
            </button>
          ) : null}
          <button type="button" className="button" onClick={() => finish("cancel")}>
            Cancel
          </button>
          <button
            type="submit"
            form={formId}
            className="button button--primary"
            data-testid="name-submit"
            disabled={busy}
          >
            {request.action}
          </button>
        </>
      }
    >
      <form id={formId} className="form-stack" onSubmit={(e) => void submit(e)}>
        <div className="field">
          <label htmlFor={fieldId}>{request.label}</label>
          <input
            id={fieldId}
            data-autofocus
            data-testid="name-input"
            value={name}
            maxLength={100}
            onChange={(e) => {
              setName(e.target.value);
              setError(null);
            }}
            onFocus={(e) => e.target.select()}
          />
        </div>
        {request.note ? <p className="field__hint">{request.note}</p> : null}
        {error ? (
          <p className="callout callout--danger" role="alert">
            <CircleAlert {...ICON_SM} className="callout__icon" />
            <span className="callout__text">{error}</span>
          </p>
        ) : null}
      </form>
    </Modal>
  );
}

function Dialog({ request }: { request: Request }) {
  switch (request.kind) {
    case "message": {
      const close = () => {
        pop();
        request.done();
      };
      const Icon = request.tone === "error" ? FileWarning : Info;
      return (
        <Modal
          title={request.title}
          onClose={close}
          footer={
            <button type="button" className="button button--primary" data-autofocus onClick={close}>
              OK
            </button>
          }
        >
          <p
            className={`callout ${request.tone === "error" ? "callout--danger" : "callout--muted"}`}
          >
            <Icon {...ICON_SM} className="callout__icon" />
            <span className="callout__text">{request.text}</span>
          </p>
        </Modal>
      );
    }
    case "unsaved": {
      const answer = (value: UnsavedAnswer) => {
        pop();
        request.done(value);
      };
      const one = request.names.length === 1;
      return (
        <Modal
          title="Save changes?"
          onClose={() => answer("cancel")}
          footer={
            <>
              <button
                type="button"
                className="button button--ghost modal__footer-start"
                data-testid="unsaved-discard"
                onClick={() => answer("discard")}
              >
                Don't save
              </button>
              <button type="button" className="button" onClick={() => answer("cancel")}>
                Cancel
              </button>
              <button
                type="button"
                className="button button--primary"
                data-testid="unsaved-save"
                data-autofocus
                onClick={() => answer("save")}
              >
                {one ? "Save" : "Save all"}
              </button>
            </>
          }
        >
          <p className="confirm__text">
            <TriangleAlert {...ICON_SM} className="confirm__icon" />
            {one ? (
              <span>
                <strong>{request.names[0]}</strong> has unsaved changes. They are lost if you don't
                save them.
              </span>
            ) : (
              <span>These tabs have unsaved changes. They are lost if you don't save them.</span>
            )}
          </p>
          {one ? null : (
            <ul className="unsaved__list">
              {request.names.map((name, i) => (
                // Two tabs can share a name.
                // biome-ignore lint/suspicious/noArrayIndexKey: names are not unique
                <li key={i}>{name}</li>
              ))}
            </ul>
          )}
        </Modal>
      );
    }
    case "name":
      return <NameDialog request={request} />;
    case "confirm": {
      const answer = (ok: boolean) => {
        pop();
        request.done(ok);
      };
      return (
        <Modal
          title={request.title}
          onClose={() => answer(false)}
          footer={
            <>
              <button type="button" className="button" onClick={() => answer(false)}>
                Cancel
              </button>
              <button
                type="button"
                className="button button--danger"
                data-testid="confirm-action"
                data-autofocus
                onClick={() => answer(true)}
              >
                {request.action}
              </button>
            </>
          }
        >
          <p className="confirm__text">
            <TriangleAlert {...ICON_SM} className="confirm__icon" />
            {request.text}
          </p>
        </Modal>
      );
    }
  }
}

/** Shows the first waiting document dialog. Mounted once by the app shell. */
export function DocumentDialogsHost() {
  const [, setTick] = useState(0);
  useEffect(() => {
    const listener = () => setTick((n) => n + 1);
    listeners.add(listener);
    return () => {
      listeners.delete(listener);
    };
  }, []);
  const request = queue[0];
  // A new key per request: a second dialog of the same kind starts fresh.
  return request ? <Dialog key={request.id} request={request} /> : null;
}

/** Tests only. */
export function resetDocumentDialogs(): void {
  queue = [];
  for (const listener of listeners) listener();
}
