import {
  type ChangeEvent,
  type KeyboardEvent,
  type ReactNode,
  useEffect,
  useId,
  useRef,
  useState,
} from "react";
import { Modal } from "../components/Modal";
import {
  cancelRequest,
  type FileFilter,
  newRequestId,
  pickFile,
  saveProfile,
  testConnection,
  toCommandError,
} from "./api";
import {
  type ConnectionForm,
  emptyForm,
  FIELD_TAB,
  type FormErrors,
  type FormField,
  type FormTab,
  firstError,
  formFromProfile,
  type HostKeyPolicy,
  isNetworked,
  profileFromForm,
  type SshAuthMethod,
  secretsFromForm,
  type ValidationPurpose,
  validateForm,
  withEngine,
} from "./form";
import {
  type CommandError,
  type ConnectionInfo,
  type ConnectionProfile,
  DIALOG_ENGINES,
  type DialogEngine,
  ENGINE_INFO,
  type ErrorCategory,
  type TlsMode,
} from "./types";

export const TLS_MODES: Array<{ value: TlsMode; label: string; help: string }> = [
  {
    value: "disable",
    label: "Off",
    help: "Never encrypt. Passwords and data cross the network in plain text. Use only on this computer or a network you fully trust.",
  },
  {
    value: "prefer",
    label: "Prefer",
    help: "Encrypt when the server supports it, otherwise connect unencrypted. The server's identity is not checked, so this does not protect against a fake server.",
  },
  {
    value: "require",
    label: "Require",
    help: "Always encrypt, but do not check the server's certificate. Stops eavesdropping, not a fake server.",
  },
  {
    value: "verify_ca",
    label: "Verify certificate authority",
    help: "Always encrypt and check that the server's certificate was issued by a trusted authority (the CA file below, or your system's trusted list). The host name in the certificate is not checked.",
  },
  {
    value: "verify_full",
    label: "Verify certificate and host name",
    help: "Always encrypt, check the certificate authority, and check that the certificate belongs to this server's host name. The safest choice for remote servers.",
  },
];

const HOST_KEY_POLICIES: Array<{ value: HostKeyPolicy; label: string; help: string }> = [
  {
    value: "strict",
    label: "Known hosts only",
    help: "Connect only if the server's key is already listed in your known_hosts file.",
  },
  {
    value: "accept_new",
    label: "Trust on first use",
    help: "Accept and remember the key the first time; refuse to connect if it changes later.",
  },
  {
    value: "fingerprint",
    label: "Exact fingerprint",
    help: "Accept only a key with this SHA256 fingerprint. No known_hosts file is used.",
  },
];

export const CATEGORY_LABEL: Record<ErrorCategory, string> = {
  connection: "Connection error",
  auth: "Authentication failed",
  timeout: "Timed out",
  cancelled: "Cancelled",
  unsupported: "Not supported",
  sql: "Server error",
  not_found: "Not found",
  invalid_input: "Invalid settings",
  io: "File or network error",
  internal: "Internal error",
};

const PEM_FILTERS: FileFilter[] = [
  { name: "PEM files", extensions: ["pem", "crt", "cer", "key"] },
  { name: "All files", extensions: ["*"] },
];
const SQLITE_FILTERS: FileFilter[] = [
  { name: "SQLite databases", extensions: ["db", "sqlite", "sqlite3", "db3"] },
  { name: "All files", extensions: ["*"] },
];

const TABS: Array<{ id: FormTab; label: string }> = [
  { id: "general", label: "General" },
  { id: "tls", label: "SSL/TLS" },
  { id: "ssh", label: "SSH" },
];

type TestState =
  | { status: "idle" }
  | { status: "running"; requestId: string }
  | { status: "ok"; info: ConnectionInfo }
  | { status: "error"; error: CommandError };

export interface ConnectionDialogProps {
  /** The profile to edit; omitted for a new connection. */
  profile?: ConnectionProfile | undefined;
  onClose: () => void;
  onSaved: (profile: ConnectionProfile) => void;
}

function fieldId(field: FormField): string {
  return `conn-${field}`;
}

interface FieldProps {
  field: FormField;
  label: string;
  errors: FormErrors;
  hint?: ReactNode;
  children: (props: {
    id: string;
    "aria-invalid": boolean;
    "aria-describedby": string | undefined;
  }) => ReactNode;
}

function Field({ field, label, errors, hint, children }: FieldProps) {
  const id = fieldId(field);
  const error = errors[field];
  const describedBy = [error ? `${id}-error` : null, hint ? `${id}-hint` : null]
    .filter(Boolean)
    .join(" ");
  return (
    <div className={`field ${error ? "field--invalid" : ""}`}>
      <label htmlFor={id}>{label}</label>
      {children({
        id,
        "aria-invalid": Boolean(error),
        "aria-describedby": describedBy || undefined,
      })}
      {hint ? (
        <p id={`${id}-hint`} className="field__hint">
          {hint}
        </p>
      ) : null}
      {error ? (
        <p id={`${id}-error`} className="field__error">
          {error}
        </p>
      ) : null}
    </div>
  );
}

export function TestResult({ state }: { state: TestState }) {
  switch (state.status) {
    case "idle":
      return null;
    case "running":
      return <p className="test-result">Testing connection…</p>;
    case "ok": {
      const { info } = state;
      return (
        <div className="test-result test-result--ok">
          <p className="test-result__title">Connection succeeded</p>
          <dl className="test-result__details">
            <dt>Server version</dt>
            <dd>{info.server_version.raw}</dd>
            <dt>Encryption (TLS)</dt>
            <dd>{info.tls_in_use ? "In use" : "Not in use"}</dd>
            <dt>SSH tunnel</dt>
            <dd>{info.via_ssh ? "Yes" : "No"}</dd>
            <dt>Connect time</dt>
            <dd>{info.latency_ms} ms</dd>
          </dl>
        </div>
      );
    }
    case "error":
      return (
        <div className="test-result test-result--error">
          <p className="test-result__title">
            Connection failed:{" "}
            <span data-testid="error-category">{CATEGORY_LABEL[state.error.category]}</span>
          </p>
          <p className="test-result__message">{state.error.message}</p>
        </div>
      );
  }
}

/** The "New connection" / "Edit connection" dialog. */
export function ConnectionDialog({ profile, onClose, onSaved }: ConnectionDialogProps) {
  const [form, setForm] = useState<ConnectionForm>(() =>
    profile ? formFromProfile(profile) : emptyForm(),
  );
  const [errors, setErrors] = useState<FormErrors>({});
  const [tab, setTab] = useState<FormTab>("general");
  const [focusField, setFocusField] = useState<FormField | null>(null);
  const [test, setTest] = useState<TestState>({ status: "idle" });
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<CommandError | null>(null);
  const tabRefs = useRef<Partial<Record<FormTab, HTMLButtonElement | null>>>({});
  const runningRequest = useRef<string | null>(null);
  const tlsHelpId = useId();
  const hostKeyHelpId = useId();

  const networked = isNetworked(form.engine);
  const hasSavedPassword = profile?.password?.source === "keychain";
  const sshAuthPrev = profile?.ssh?.auth;
  const hasSavedSshSecret =
    (sshAuthPrev?.method === "password" && sshAuthPrev.password.source === "keychain") ||
    (sshAuthPrev?.method === "private_key" && sshAuthPrev.passphrase?.source === "keychain");

  useEffect(() => {
    if (focusField) {
      document.getElementById(fieldId(focusField))?.focus();
      setFocusField(null);
    }
  }, [focusField]);

  // Cancel a running test when the dialog goes away.
  useEffect(
    () => () => {
      if (runningRequest.current) void cancelRequest(runningRequest.current).catch(() => {});
    },
    [],
  );

  function set<K extends keyof ConnectionForm>(key: K, value: ConnectionForm[K]) {
    setForm((f) => ({ ...f, [key]: value }));
    if (errors[key]) setErrors((e) => ({ ...e, [key]: undefined }));
  }

  function text(key: FormField) {
    return (event: ChangeEvent<HTMLInputElement>) => set(key, event.target.value as never);
  }

  function check(key: "savePassword" | "sshEnabled" | "saveSshSecret") {
    return (event: ChangeEvent<HTMLInputElement>) => set(key, event.target.checked);
  }

  async function browse(key: FormField, title: string, filters: FileFilter[]) {
    const path = await pickFile(title, filters);
    if (path !== null) set(key, path as never);
  }

  function validate(purpose: ValidationPurpose): boolean {
    const found = validateForm(form, purpose);
    setErrors(found);
    const first = firstError(found);
    if (!first) return true;
    setTab(FIELD_TAB[first] ?? "general");
    setFocusField(first);
    return false;
  }

  async function runTest() {
    if (!validate("test")) return;
    const requestId = newRequestId();
    runningRequest.current = requestId;
    setTest({ status: "running", requestId });
    const fallbackName = form.host.trim() || form.filePath.trim() || "Untitled";
    try {
      const info = await testConnection(
        profileFromForm(form, profile, fallbackName),
        secretsFromForm(form),
        requestId,
      );
      if (runningRequest.current === requestId) setTest({ status: "ok", info });
    } catch (error) {
      if (runningRequest.current === requestId) {
        setTest({ status: "error", error: toCommandError(error) });
      }
    } finally {
      if (runningRequest.current === requestId) runningRequest.current = null;
    }
  }

  function stopTest() {
    if (test.status === "running") void cancelRequest(test.requestId).catch(() => {});
  }

  async function save() {
    if (!validate("save")) return;
    setSaving(true);
    setSaveError(null);
    try {
      const saved = await saveProfile(profileFromForm(form, profile), secretsFromForm(form));
      onSaved(saved);
    } catch (error) {
      setSaveError(toCommandError(error));
    } finally {
      setSaving(false);
    }
  }

  function onTabKeyDown(event: KeyboardEvent<HTMLButtonElement>) {
    const enabled = TABS.filter((t) => t.id === "general" || networked).map((t) => t.id);
    const index = enabled.indexOf(tab);
    let next: FormTab | undefined;
    if (event.key === "ArrowRight") next = enabled[(index + 1) % enabled.length];
    else if (event.key === "ArrowLeft")
      next = enabled[(index - 1 + enabled.length) % enabled.length];
    else if (event.key === "Home") next = enabled[0];
    else if (event.key === "End") next = enabled[enabled.length - 1];
    if (next) {
      event.preventDefault();
      setTab(next);
      tabRefs.current[next]?.focus();
    }
  }

  const tlsHelp = TLS_MODES.find((m) => m.value === form.tlsMode)?.help;
  const hostKeyHelp = HOST_KEY_POLICIES.find((p) => p.value === form.hostKeyPolicy)?.help;
  const testing = test.status === "running";

  const footer = (
    <>
      <div className="modal__footer-start">
        {testing ? (
          <button type="button" className="button" onClick={stopTest}>
            Stop test
          </button>
        ) : (
          <button type="button" className="button" onClick={() => void runTest()}>
            Test connection
          </button>
        )}
      </div>
      <button type="button" className="button" onClick={onClose}>
        Cancel
      </button>
      <button
        type="submit"
        form="connection-form"
        className="button button--primary"
        disabled={saving}
      >
        {saving ? "Saving…" : "Save"}
      </button>
    </>
  );

  return (
    <Modal
      title={profile ? `Edit connection: ${profile.name}` : "New connection"}
      onClose={onClose}
      footer={footer}
      className="modal--wide"
    >
      <form
        id="connection-form"
        noValidate
        onSubmit={(event) => {
          event.preventDefault();
          void save();
        }}
      >
        <div className="tabs" role="tablist" aria-label="Connection settings">
          {TABS.map((t) => {
            const disabled = t.id !== "general" && !networked;
            return (
              <button
                key={t.id}
                ref={(el) => {
                  tabRefs.current[t.id] = el;
                }}
                type="button"
                role="tab"
                id={`conn-tab-${t.id}`}
                aria-selected={tab === t.id}
                aria-controls={`conn-panel-${t.id}`}
                tabIndex={tab === t.id ? 0 : -1}
                disabled={disabled}
                title={disabled ? "Not used by SQLite (a local file)" : undefined}
                className="tabs__tab"
                onClick={() => setTab(t.id)}
                onKeyDown={onTabKeyDown}
              >
                {t.label}
              </button>
            );
          })}
        </div>

        <div
          role="tabpanel"
          id="conn-panel-general"
          aria-labelledby="conn-tab-general"
          hidden={tab !== "general"}
          className="tabs__panel"
        >
          <Field field="name" label="Connection name" errors={errors}>
            {(p) => (
              <input {...p} data-autofocus type="text" value={form.name} onChange={text("name")} />
            )}
          </Field>
          <Field field="engine" label="Database type" errors={errors}>
            {(p) => (
              <select
                {...p}
                value={form.engine}
                onChange={(e) => {
                  setForm((f) => withEngine(f, e.target.value as DialogEngine));
                  setErrors({});
                  if (e.target.value === "sqlite") setTab("general");
                }}
              >
                {DIALOG_ENGINES.map((engine) => (
                  <option key={engine} value={engine}>
                    {ENGINE_INFO[engine].label}
                  </option>
                ))}
              </select>
            )}
          </Field>

          {networked ? (
            <>
              <div className="field-row">
                <Field field="host" label="Host" errors={errors}>
                  {(p) => (
                    <input
                      {...p}
                      type="text"
                      autoComplete="off"
                      spellCheck={false}
                      value={form.host}
                      onChange={text("host")}
                    />
                  )}
                </Field>
                <Field field="port" label="Port" errors={errors}>
                  {(p) => (
                    <input
                      {...p}
                      type="text"
                      inputMode="numeric"
                      className="input--port"
                      placeholder={String(ENGINE_INFO[form.engine].port ?? "")}
                      value={form.port}
                      onChange={text("port")}
                    />
                  )}
                </Field>
              </div>
              <Field field="database" label="Database (optional)" errors={errors}>
                {(p) => (
                  <input {...p} type="text" value={form.database} onChange={text("database")} />
                )}
              </Field>
              <Field field="user" label="User name" errors={errors}>
                {(p) => (
                  <input
                    {...p}
                    type="text"
                    autoComplete="off"
                    value={form.user}
                    onChange={text("user")}
                  />
                )}
              </Field>
              <Field
                field="password"
                label="Password"
                errors={errors}
                hint={
                  form.savePassword
                    ? hasSavedPassword
                      ? "A password is saved. Leave blank to keep it, unless you change where it connects."
                      : "Stored in your system keychain, never in the connection file."
                    : "Not saved: you will be asked for it each time you connect."
                }
              >
                {(p) => (
                  <input
                    {...p}
                    type="password"
                    autoComplete="new-password"
                    placeholder={hasSavedPassword && form.savePassword ? "••••••••" : undefined}
                    value={form.password}
                    onChange={text("password")}
                  />
                )}
              </Field>
              <label className="checkbox">
                <input
                  type="checkbox"
                  checked={form.savePassword}
                  onChange={check("savePassword")}
                />
                Save password
              </label>
            </>
          ) : (
            <Field
              field="filePath"
              label="Database file"
              errors={errors}
              hint="SQLite works on a local file, so SSL/TLS and SSH do not apply."
            >
              {(p) => (
                <div className="file-input">
                  <input {...p} type="text" value={form.filePath} onChange={text("filePath")} />
                  <button
                    type="button"
                    className="button"
                    aria-label="Browse for the database file"
                    onClick={() =>
                      void browse("filePath", "Choose a SQLite database", SQLITE_FILTERS)
                    }
                  >
                    Browse…
                  </button>
                </div>
              )}
            </Field>
          )}
        </div>

        <div
          role="tabpanel"
          id="conn-panel-tls"
          aria-labelledby="conn-tab-tls"
          hidden={tab !== "tls"}
          className="tabs__panel"
        >
          <div className="field">
            <label htmlFor={fieldId("tlsMode")}>Encryption mode</label>
            <select
              id={fieldId("tlsMode")}
              aria-describedby={tlsHelpId}
              value={form.tlsMode}
              onChange={(e) => set("tlsMode", e.target.value as TlsMode)}
            >
              {TLS_MODES.map((m) => (
                <option key={m.value} value={m.value}>
                  {m.label}
                </option>
              ))}
            </select>
            <p id={tlsHelpId} className="field__hint" data-testid="tls-help">
              {tlsHelp}
            </p>
          </div>
          <FileField
            field="caCert"
            label="CA certificate (optional)"
            browseLabel="Browse for the CA certificate"
            errors={errors}
            value={form.caCert}
            onChange={text("caCert")}
            onBrowse={() => void browse("caCert", "Choose a CA certificate", PEM_FILTERS)}
            hint="The authority that signed the server's certificate. Without it, your system's trusted list is used."
          />
          <FileField
            field="clientCert"
            label="Client certificate (optional)"
            browseLabel="Browse for the client certificate"
            errors={errors}
            value={form.clientCert}
            onChange={text("clientCert")}
            onBrowse={() => void browse("clientCert", "Choose a client certificate", PEM_FILTERS)}
          />
          <FileField
            field="clientKey"
            label="Client key (optional)"
            browseLabel="Browse for the client key"
            errors={errors}
            value={form.clientKey}
            onChange={text("clientKey")}
            onBrowse={() => void browse("clientKey", "Choose the client key", PEM_FILTERS)}
            hint="Needed together with the client certificate. Must not be password-protected."
          />
          <Field
            field="serverName"
            label="Certificate host name (optional)"
            errors={errors}
            hint="Check the certificate against this name instead of the host above."
          >
            {(p) => (
              <input
                {...p}
                type="text"
                spellCheck={false}
                value={form.serverName}
                onChange={text("serverName")}
              />
            )}
          </Field>
        </div>

        <div
          role="tabpanel"
          id="conn-panel-ssh"
          aria-labelledby="conn-tab-ssh"
          hidden={tab !== "ssh"}
          className="tabs__panel"
        >
          <label className="checkbox">
            <input type="checkbox" checked={form.sshEnabled} onChange={check("sshEnabled")} />
            Connect through an SSH tunnel
          </label>
          <fieldset className="fieldset" disabled={!form.sshEnabled}>
            <legend className="visually-hidden">SSH tunnel settings</legend>
            <div className="field-row">
              <Field field="sshHost" label="SSH host" errors={errors}>
                {(p) => (
                  <input
                    {...p}
                    type="text"
                    spellCheck={false}
                    value={form.sshHost}
                    onChange={text("sshHost")}
                  />
                )}
              </Field>
              <Field field="sshPort" label="SSH port" errors={errors}>
                {(p) => (
                  <input
                    {...p}
                    type="text"
                    inputMode="numeric"
                    className="input--port"
                    value={form.sshPort}
                    onChange={text("sshPort")}
                  />
                )}
              </Field>
            </div>
            <Field field="sshUser" label="SSH user name" errors={errors}>
              {(p) => (
                <input
                  {...p}
                  type="text"
                  autoComplete="off"
                  value={form.sshUser}
                  onChange={text("sshUser")}
                />
              )}
            </Field>
            <Field field="sshAuth" label="Authentication" errors={errors}>
              {(p) => (
                <select
                  {...p}
                  value={form.sshAuth}
                  onChange={(e) => set("sshAuth", e.target.value as SshAuthMethod)}
                >
                  <option value="password">Password</option>
                  <option value="private_key">Private key file</option>
                  <option value="agent">SSH agent</option>
                </select>
              )}
            </Field>
            {form.sshAuth === "password" ? (
              <Field
                field="sshPassword"
                label="SSH password"
                errors={errors}
                hint={
                  hasSavedSshSecret && form.saveSshSecret
                    ? "A password is saved. Leave blank to keep it, unless you change where it connects."
                    : undefined
                }
              >
                {(p) => (
                  <input
                    {...p}
                    type="password"
                    autoComplete="new-password"
                    value={form.sshPassword}
                    onChange={text("sshPassword")}
                  />
                )}
              </Field>
            ) : null}
            {form.sshAuth === "private_key" ? (
              <>
                <FileField
                  field="sshKeyFile"
                  label="Private key file"
                  browseLabel="Browse for the private key file"
                  errors={errors}
                  value={form.sshKeyFile}
                  onChange={text("sshKeyFile")}
                  onBrowse={() => void browse("sshKeyFile", "Choose an SSH private key", [])}
                />
                <Field
                  field="sshPassphrase"
                  label="Key passphrase (if the key is encrypted)"
                  errors={errors}
                  hint={
                    hasSavedSshSecret && form.saveSshSecret
                      ? "A passphrase is saved. Leave blank to keep it."
                      : form.saveSshSecret
                        ? undefined
                        : "Not saved. If the key is encrypted, type the passphrase to be asked for it on every connect."
                  }
                >
                  {(p) => (
                    <input
                      {...p}
                      type="password"
                      autoComplete="new-password"
                      value={form.sshPassphrase}
                      onChange={text("sshPassphrase")}
                    />
                  )}
                </Field>
              </>
            ) : null}
            {form.sshAuth !== "agent" ? (
              <label className="checkbox">
                <input
                  type="checkbox"
                  checked={form.saveSshSecret}
                  onChange={check("saveSshSecret")}
                />
                {form.sshAuth === "password" ? "Save SSH password" : "Save key passphrase"}
              </label>
            ) : null}
            <div className="field">
              <label htmlFor={fieldId("hostKeyPolicy")}>Server key check</label>
              <select
                id={fieldId("hostKeyPolicy")}
                aria-describedby={hostKeyHelpId}
                value={form.hostKeyPolicy}
                onChange={(e) => set("hostKeyPolicy", e.target.value as HostKeyPolicy)}
              >
                {HOST_KEY_POLICIES.map((p) => (
                  <option key={p.value} value={p.value}>
                    {p.label}
                  </option>
                ))}
              </select>
              <p id={hostKeyHelpId} className="field__hint">
                {hostKeyHelp}
              </p>
            </div>
            {form.hostKeyPolicy === "fingerprint" ? (
              <Field field="fingerprint" label="Key fingerprint" errors={errors}>
                {(p) => (
                  <input
                    {...p}
                    type="text"
                    spellCheck={false}
                    placeholder="SHA256:…"
                    value={form.fingerprint}
                    onChange={text("fingerprint")}
                  />
                )}
              </Field>
            ) : (
              <FileField
                field="knownHostsPath"
                label="known_hosts file (optional)"
                browseLabel="Browse for the known_hosts file"
                errors={errors}
                value={form.knownHostsPath}
                onChange={text("knownHostsPath")}
                onBrowse={() => void browse("knownHostsPath", "Choose a known_hosts file", [])}
                hint="Leave blank to use ~/.ssh/known_hosts."
              />
            )}
          </fieldset>
        </div>
      </form>

      <div className="dialog-status" role="status" aria-live="polite">
        <TestResult state={test} />
      </div>
      {saveError ? (
        <div className="test-result test-result--error" role="alert">
          <p className="test-result__title">Could not save: {CATEGORY_LABEL[saveError.category]}</p>
          <p className="test-result__message">{saveError.message}</p>
        </div>
      ) : null}
    </Modal>
  );
}

interface FileFieldProps {
  field: FormField;
  label: string;
  browseLabel: string;
  errors: FormErrors;
  value: string;
  hint?: string;
  onChange: (event: ChangeEvent<HTMLInputElement>) => void;
  onBrowse: () => void;
}

function FileField({
  field,
  label,
  browseLabel,
  errors,
  value,
  hint,
  onChange,
  onBrowse,
}: FileFieldProps) {
  return (
    <Field field={field} label={label} errors={errors} hint={hint}>
      {(p) => (
        <div className="file-input">
          <input {...p} type="text" spellCheck={false} value={value} onChange={onChange} />
          <button type="button" className="button" aria-label={browseLabel} onClick={onBrowse}>
            Browse…
          </button>
        </div>
      )}
    </Field>
  );
}
