import {
  CircleAlert,
  CircleCheck,
  FolderOpen,
  KeyRound,
  Loader2,
  type LucideIcon,
  PlugZap,
  Server,
  ShieldCheck,
  Square,
} from "lucide-react";
import {
  type ChangeEvent,
  type KeyboardEvent,
  type ReactNode,
  useEffect,
  useId,
  useRef,
  useState,
} from "react";
import { ICON, ICON_SM } from "../components/icons";
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
import { EngineBadge } from "./EngineBadge";
import {
  type ConnectionForm,
  emptyForm,
  engineTraits,
  FIELD_TAB,
  type FormErrors,
  type FormField,
  type FormTab,
  firstError,
  formFromProfile,
  type HostKeyPolicy,
  isNetworked,
  MONGO_AUTH_MECHANISMS,
  MONGO_READ_PREFERENCES,
  type MongoAuthMechanism,
  type MongoReadPreference,
  profileFromForm,
  type RedisMode,
  type SshAuthMethod,
  STRICT_TLS_MODES,
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

const TABS: Array<{ id: FormTab; label: string; Icon: LucideIcon }> = [
  { id: "general", label: "General", Icon: Server },
  { id: "tls", label: "SSL/TLS", Icon: ShieldCheck },
  { id: "ssh", label: "SSH", Icon: KeyRound },
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
      return (
        <div className="test-result test-result--running">
          <Loader2 {...ICON} className="test-result__icon spin" />
          <div className="test-result__body">
            <p className="test-result__title">Testing connection…</p>
          </div>
        </div>
      );
    case "ok": {
      const { info } = state;
      return (
        <div className="test-result test-result--ok">
          <CircleCheck {...ICON} className="test-result__icon" />
          <div className="test-result__body">
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
        </div>
      );
    }
    case "error":
      return (
        <div className="test-result test-result--error">
          <CircleAlert {...ICON} className="test-result__icon" />
          <div className="test-result__body">
            <p className="test-result__title">
              Connection failed:{" "}
              <span data-testid="error-category">{CATEGORY_LABEL[state.error.category]}</span>
            </p>
            <p className="test-result__message">{state.error.message}</p>
          </div>
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
  const sshHintId = useId();

  const networked = isNetworked(form.engine);
  const traits = engineTraits(form.engine);
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

  function check(
    key:
      | "savePassword"
      | "sshEnabled"
      | "saveSshSecret"
      | "strictEncryption"
      | "readOnlyIntent"
      | "srv",
  ) {
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

  const tlsHelp =
    traits.tlsHelp?.[form.tlsMode] ?? TLS_MODES.find((m) => m.value === form.tlsMode)?.help;
  const strictAllowed = STRICT_TLS_MODES.includes(form.tlsMode);
  const hostKeyHelp = HOST_KEY_POLICIES.find((p) => p.value === form.hostKeyPolicy)?.help;
  const testing = test.status === "running";

  const footer = (
    <>
      <div className="modal__footer-start">
        {testing ? (
          <button type="button" className="button" onClick={stopTest}>
            <Square {...ICON_SM} />
            Stop test
          </button>
        ) : (
          <button type="button" className="button" onClick={() => void runTest()}>
            <PlugZap {...ICON} />
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
        className="connection-form"
        noValidate
        onSubmit={(event) => {
          event.preventDefault();
          void save();
        }}
      >
        <fieldset className="engine-picker">
          <legend className="form-section__title">Database type</legend>
          <div className="engine-picker__grid">
            {DIALOG_ENGINES.map((engine) => (
              <label key={engine} className="engine-tile">
                <input
                  type="radio"
                  name="conn-engine"
                  className="engine-tile__input"
                  value={engine}
                  checked={form.engine === engine}
                  {...(form.engine === engine ? { id: fieldId("engine") } : {})}
                  onChange={() => {
                    setForm((f) => withEngine(f, engine));
                    setErrors({});
                    if (engine === "sqlite") setTab("general");
                  }}
                />
                <EngineBadge engine={engine} />
                <span className="engine-tile__label">{ENGINE_INFO[engine].label}</span>
              </label>
            ))}
          </div>
        </fieldset>

        <Field field="name" label="Connection name" errors={errors}>
          {(p) => (
            <input
              {...p}
              data-autofocus
              type="text"
              placeholder="e.g. Production"
              value={form.name}
              onChange={text("name")}
            />
          )}
        </Field>

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
                <t.Icon {...ICON_SM} />
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
          {networked ? (
            <>
              <h3 className="form-section__title">Server</h3>
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
                      disabled={traits.options === "mongodb" && form.srv}
                      value={traits.options === "mongodb" && form.srv ? "" : form.port}
                      onChange={text("port")}
                    />
                  )}
                </Field>
              </div>
              {traits.options === "mongodb" ? (
                <MongoServerFields
                  form={form}
                  errors={errors}
                  set={set}
                  text={text}
                  check={check}
                />
              ) : null}
              {traits.options === "redis" ? (
                <RedisServerFields form={form} errors={errors} set={set} text={text} />
              ) : null}
              {traits.instance ? (
                <Field
                  field="instance"
                  label="Instance name (optional)"
                  errors={errors}
                  hint="For a named instance (like SQLEXPRESS), also enter the port it listens on above: the SQL Server Browser service is not asked for it."
                >
                  {(p) => (
                    <input
                      {...p}
                      type="text"
                      autoComplete="off"
                      spellCheck={false}
                      value={form.instance}
                      onChange={text("instance")}
                    />
                  )}
                </Field>
              ) : null}
              <Field
                field="database"
                label={traits.databaseLabel ?? "Database (optional)"}
                errors={errors}
                hint={traits.databaseHint}
              >
                {(p) => (
                  <input
                    {...p}
                    type="text"
                    placeholder={traits.databasePlaceholder}
                    value={form.database}
                    onChange={text("database")}
                  />
                )}
              </Field>
              <h3 className="form-section__title form-section__title--spaced">Credentials</h3>
              <Field field="user" label="User name" errors={errors} hint={traits.userHint}>
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
              {traits.options === "mongodb" ? (
                <MongoAuthFields form={form} errors={errors} set={set} text={text} />
              ) : null}
              {traits.readOnlyIntent ? (
                <label className="checkbox">
                  <input
                    type="checkbox"
                    checked={form.readOnlyIntent}
                    onChange={check("readOnlyIntent")}
                  />
                  Read-only intent (route to a readable secondary replica)
                </label>
              ) : null}
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
                    <FolderOpen {...ICON_SM} />
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
          {traits.strictEncryption ? (
            <div className={`field ${errors.strictEncryption ? "field--invalid" : ""}`}>
              <label className="checkbox">
                <input
                  id={fieldId("strictEncryption")}
                  type="checkbox"
                  checked={form.strictEncryption}
                  disabled={!strictAllowed && !form.strictEncryption}
                  aria-invalid={Boolean(errors.strictEncryption)}
                  aria-describedby={
                    errors.strictEncryption
                      ? `${fieldId("strictEncryption")}-error`
                      : `${fieldId("strictEncryption")}-hint`
                  }
                  onChange={check("strictEncryption")}
                />
                TDS 8.0 strict encryption
              </label>
              <p id={`${fieldId("strictEncryption")}-hint`} className="field__hint">
                {strictAllowed
                  ? "Encrypt before anything else is sent, as SQL Server 2022 and later and Azure SQL allow. Off: encryption is negotiated first (every version)."
                  : "Needs the mode Require, Verify certificate authority or Verify certificate and host name."}
              </p>
              {errors.strictEncryption ? (
                <p id={`${fieldId("strictEncryption")}-error`} className="field__error">
                  {errors.strictEncryption}
                </p>
              ) : null}
            </div>
          ) : null}
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
          {traits.clientCertificates ? (
            <>
              <FileField
                field="clientCert"
                label="Client certificate (optional)"
                browseLabel="Browse for the client certificate"
                errors={errors}
                value={form.clientCert}
                onChange={text("clientCert")}
                onBrowse={() =>
                  void browse("clientCert", "Choose a client certificate", PEM_FILTERS)
                }
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
            </>
          ) : (
            <p className="field__hint" data-testid="no-client-cert">
              {ENGINE_INFO[form.engine].label} connections do not use client certificates.
            </p>
          )}
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
          <div className="option-card">
            <label className="checkbox">
              <input
                type="checkbox"
                checked={form.sshEnabled}
                aria-describedby={sshHintId}
                onChange={check("sshEnabled")}
              />
              Connect through an SSH tunnel
            </label>
            <p id={sshHintId} className="field__hint">
              Reach a database that only accepts connections from a bastion or jump host.
            </p>
          </div>
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
          <CircleAlert {...ICON} className="test-result__icon" />
          <div className="test-result__body">
            <p className="test-result__title">
              Could not save: {CATEGORY_LABEL[saveError.category]}
            </p>
            <p className="test-result__message">{saveError.message}</p>
          </div>
        </div>
      ) : null}
    </Modal>
  );
}

type SetField = <K extends keyof ConnectionForm>(key: K, value: ConnectionForm[K]) => void;
type TextHandler = (key: FormField) => (event: ChangeEvent<HTMLInputElement>) => void;

interface EngineFieldsProps {
  form: ConnectionForm;
  errors: FormErrors;
  set: SetField;
  text: TextHandler;
}

const READ_PREFERENCE_LABEL: Record<MongoReadPreference, string> = {
  primary: "Primary",
  primaryPreferred: "Primary preferred",
  secondary: "Secondary",
  secondaryPreferred: "Secondary preferred",
  nearest: "Nearest",
};

/** MongoDB: seed hosts, SRV, replica set and read preference (`oxbase_mongo::options`). */
function MongoServerFields({
  form,
  errors,
  set,
  text,
  check,
}: EngineFieldsProps & { check: (key: "srv") => (e: ChangeEvent<HTMLInputElement>) => void }) {
  return (
    <>
      <div className={`field ${errors.srv ? "field--invalid" : ""}`}>
        <label className="checkbox">
          <input
            id={fieldId("srv")}
            type="checkbox"
            checked={form.srv}
            aria-invalid={Boolean(errors.srv)}
            aria-describedby={errors.srv ? `${fieldId("srv")}-error` : `${fieldId("srv")}-hint`}
            onChange={check("srv")}
          />
          The host is an SRV name (mongodb+srv)
        </label>
        <p id={`${fieldId("srv")}-hint`} className="field__hint">
          DNS lists the servers and their ports; not available through an SSH tunnel.
        </p>
        {errors.srv ? (
          <p id={`${fieldId("srv")}-error`} className="field__error">
            {errors.srv}
          </p>
        ) : null}
      </div>
      {form.srv ? null : (
        <Field
          field="hosts"
          label="More hosts (optional)"
          errors={errors}
          hint="Other members of a replica set or sharded cluster, as host:port separated by commas."
        >
          {(p) => (
            <input
              {...p}
              type="text"
              autoComplete="off"
              spellCheck={false}
              placeholder="db2.example.com:27017, db3.example.com:27017"
              value={form.hosts}
              onChange={text("hosts")}
            />
          )}
        </Field>
      )}
      <div className="field-row">
        <Field field="replicaSet" label="Replica set (optional)" errors={errors}>
          {(p) => (
            <input
              {...p}
              type="text"
              autoComplete="off"
              spellCheck={false}
              value={form.replicaSet}
              onChange={text("replicaSet")}
            />
          )}
        </Field>
        <Field field="readPreference" label="Read preference" errors={errors}>
          {(p) => (
            <select
              {...p}
              value={form.readPreference}
              onChange={(e) => set("readPreference", e.target.value as MongoReadPreference)}
            >
              {MONGO_READ_PREFERENCES.map((r) => (
                <option key={r} value={r}>
                  {READ_PREFERENCE_LABEL[r]}
                </option>
              ))}
            </select>
          )}
        </Field>
      </div>
    </>
  );
}

/** MongoDB: authentication database and mechanism. */
function MongoAuthFields({ form, errors, set, text }: EngineFieldsProps) {
  const external = form.authMechanism === "MONGODB-X509" || form.authMechanism === "PLAIN";
  return (
    <div className="field-row">
      <Field
        field="authSource"
        label="Authentication database"
        errors={errors}
        hint={external ? "Blank: $external." : "Blank: admin."}
      >
        {(p) => (
          <input
            {...p}
            type="text"
            autoComplete="off"
            spellCheck={false}
            placeholder={external ? "$external" : "admin"}
            value={form.authSource}
            onChange={text("authSource")}
          />
        )}
      </Field>
      <Field field="authMechanism" label="Authentication mechanism" errors={errors}>
        {(p) => (
          <select
            {...p}
            value={form.authMechanism}
            onChange={(e) => set("authMechanism", e.target.value as MongoAuthMechanism | "")}
          >
            <option value="">Negotiated (SCRAM)</option>
            {MONGO_AUTH_MECHANISMS.map((m) => (
              <option key={m} value={m}>
                {m === "MONGODB-X509" ? "X.509 client certificate" : m}
              </option>
            ))}
          </select>
        )}
      </Field>
    </div>
  );
}

const REDIS_MODE_LABEL: Record<RedisMode, string> = {
  standalone: "Standalone server",
  cluster: "Cluster",
  sentinel: "Sentinel",
};

/** Redis: mode, seeds, sentinel master, protocol and namespace separator. */
function RedisServerFields({ form, errors, set, text }: EngineFieldsProps) {
  return (
    <>
      <Field
        field="redisMode"
        label="Mode"
        errors={errors}
        hint={
          form.redisMode === "cluster"
            ? "The host above and the nodes below are seeds; the client discovers the slots."
            : form.redisMode === "sentinel"
              ? "The host above and the nodes below are sentinels; they name the master to connect to."
              : undefined
        }
      >
        {(p) => (
          <select
            {...p}
            value={form.redisMode}
            onChange={(e) => set("redisMode", e.target.value as RedisMode)}
          >
            {(Object.keys(REDIS_MODE_LABEL) as RedisMode[]).map((m) => (
              <option key={m} value={m}>
                {REDIS_MODE_LABEL[m]}
              </option>
            ))}
          </select>
        )}
      </Field>
      {form.redisMode === "standalone" ? null : (
        <Field
          field="nodes"
          label={form.redisMode === "cluster" ? "More cluster nodes" : "More sentinels"}
          errors={errors}
          hint="host:port separated by commas (optional)."
        >
          {(p) => (
            <input
              {...p}
              type="text"
              autoComplete="off"
              spellCheck={false}
              value={form.nodes}
              onChange={text("nodes")}
            />
          )}
        </Field>
      )}
      {form.redisMode === "sentinel" ? (
        <div className="field-row">
          <Field field="masterName" label="Master name" errors={errors}>
            {(p) => (
              <input
                {...p}
                type="text"
                autoComplete="off"
                spellCheck={false}
                value={form.masterName}
                onChange={text("masterName")}
              />
            )}
          </Field>
          <Field field="sentinelAuth" label="Sentinel login" errors={errors}>
            {(p) => (
              <select
                {...p}
                value={form.sentinelAuth}
                onChange={(e) => set("sentinelAuth", e.target.value as "none" | "same")}
              >
                <option value="none">None</option>
                <option value="same">Same user and password</option>
              </select>
            )}
          </Field>
        </div>
      ) : null}
      <div className="field-row">
        <Field field="protocol" label="Protocol" errors={errors}>
          {(p) => (
            <select
              {...p}
              value={form.protocol}
              onChange={(e) => set("protocol", e.target.value as "auto" | "resp2" | "resp3")}
            >
              <option value="auto">Automatic (RESP3 when available)</option>
              <option value="resp2">RESP2</option>
              <option value="resp3">RESP3</option>
            </select>
          )}
        </Field>
        <Field
          field="namespaceSeparator"
          label="Namespace separator"
          errors={errors}
          hint="Splits key names into tree folders; blank shows every key flat."
        >
          {(p) => (
            <input
              {...p}
              type="text"
              autoComplete="off"
              spellCheck={false}
              className="input--port"
              value={form.namespaceSeparator}
              onChange={text("namespaceSeparator")}
            />
          )}
        </Field>
      </div>
    </>
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
            <FolderOpen {...ICON_SM} />
            Browse…
          </button>
        </div>
      )}
    </Field>
  );
}
