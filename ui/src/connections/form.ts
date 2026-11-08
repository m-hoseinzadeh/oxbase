/**
 * The "New / Edit connection" form model: defaults, validation and conversion to and from a
 * {@link ConnectionProfile}. Pure functions, so they are easy to test.
 */
import { getSettings } from "../settings";
import {
  type ConnectionProfile,
  DIALOG_ENGINES,
  type DialogEngine,
  ENGINE_INFO,
  type KeyRef,
  type KnownHostsPolicy,
  NEW_PROFILE_ID,
  type PemRef,
  type SecretInput,
  type SecretRef,
  type SessionMode,
  type SshAuthSettings,
  type SshSettings,
  type TlsMode,
  type TlsSettings,
} from "./types";

export type SshAuthMethod = "password" | "private_key" | "agent";
export type HostKeyPolicy = "strict" | "accept_new" | "fingerprint";

export interface ConnectionForm {
  name: string;
  engine: DialogEngine;
  host: string;
  port: string;
  database: string;
  user: string;
  password: string;
  savePassword: boolean;
  filePath: string;

  tlsMode: TlsMode;
  caCert: string;
  clientCert: string;
  clientKey: string;
  serverName: string;

  sshEnabled: boolean;
  sshHost: string;
  sshPort: string;
  sshUser: string;
  sshAuth: SshAuthMethod;
  sshPassword: string;
  sshKeyFile: string;
  sshPassphrase: string;
  saveSshSecret: boolean;
  hostKeyPolicy: HostKeyPolicy;
  knownHostsPath: string;
  fingerprint: string;

  /** SQL Server: named instance (`options.instance`). */
  instance: string;
  /** SQL Server: TDS 8.0 strict encryption (`options.encrypt = strict`). */
  strictEncryption: boolean;
  /** SQL Server: read-only application intent (`options.application_intent = read_only`). */
  readOnlyIntent: boolean;

  /** MongoDB: more seed hosts, `host:port` comma-separated (`options.hosts`). */
  hosts: string;
  /** MongoDB: the host is a DNS SRV name (`options.srv`, `mongodb+srv://`). */
  srv: boolean;
  /** MongoDB: replica set name (`options.replica_set`). */
  replicaSet: string;
  /** MongoDB: authentication database (`options.auth_source`; blank = the default). */
  authSource: string;
  /** MongoDB: `options.auth_mechanism` (blank = negotiated). */
  authMechanism: MongoAuthMechanism | "";
  /** MongoDB: `options.read_preference`. */
  readPreference: MongoReadPreference;

  /** Redis: `options.mode`. */
  redisMode: RedisMode;
  /** Redis: more seeds, cluster nodes or sentinels, `host:port` comma-separated. */
  nodes: string;
  /** Redis sentinel: the monitored master's name (`options.master_name`). */
  masterName: string;
  /** Redis sentinel: whether the sentinels get the profile's credentials. */
  sentinelAuth: "none" | "same";
  /** Redis: `options.protocol`. */
  protocol: "auto" | "resp2" | "resp3";
  /** Redis: key namespace separator for the tree (`options.namespace_separator`; "" = none). */
  namespaceSeparator: string;

  /** How long query tabs keep their session (`session_mode`; see {@link offersPerRun}). */
  sessionMode: SessionMode;
}

export type MongoAuthMechanism = "SCRAM-SHA-256" | "SCRAM-SHA-1" | "MONGODB-X509" | "PLAIN";
export type MongoReadPreference =
  | "primary"
  | "primaryPreferred"
  | "secondary"
  | "secondaryPreferred"
  | "nearest";
export type RedisMode = "standalone" | "cluster" | "sentinel";

export type FormField = keyof ConnectionForm;
export type FormErrors = Partial<Record<FormField, string>>;
export type FormTab = "general" | "tls" | "ssh";

/** Which tab each validated field lives on (to jump to the first error). */
export const FIELD_TAB: Partial<Record<FormField, FormTab>> = {
  name: "general",
  host: "general",
  port: "general",
  filePath: "general",
  instance: "general",
  database: "general",
  hosts: "general",
  srv: "general",
  authMechanism: "general",
  redisMode: "general",
  nodes: "general",
  masterName: "general",
  strictEncryption: "tls",
  caCert: "tls",
  clientCert: "tls",
  clientKey: "tls",
  sshHost: "ssh",
  sshPort: "ssh",
  sshUser: "ssh",
  sshKeyFile: "ssh",
  fingerprint: "ssh",
};

/** Validation order: the first field with an error gets focus. */
export const FIELD_ORDER: FormField[] = [
  "name",
  "host",
  "port",
  "srv",
  "hosts",
  "redisMode",
  "nodes",
  "masterName",
  "instance",
  "database",
  "authMechanism",
  "filePath",
  "strictEncryption",
  "caCert",
  "clientCert",
  "clientKey",
  "sshHost",
  "sshPort",
  "sshUser",
  "sshKeyFile",
  "fingerprint",
];

/**
 * Why `name` is not a SQL Server instance name (up to 16 characters, none of
 * `\ / : , ; ' & @ #` or white space), or `null`; empty is no instance.
 */
export function instanceProblem(name: string): string | null {
  if (name === "") return null;
  if (/[\\/:]/.test(name)) {
    return "Enter only the instance name (like SQLEXPRESS), without the host.";
  }
  if (name.includes(",")) {
    return "Enter only the instance name (like SQLEXPRESS); the port goes in the Port field.";
  }
  if (/[;'&@#\s]/.test(name) || name.length > 16) {
    return "Enter only the instance name (like SQLEXPRESS): up to 16 letters, digits, _ or $.";
  }
  return null;
}

/**
 * What the dialog offers per engine beyond the common fields. Engines not listed have none of
 * the extras and accept client certificates.
 */
export interface EngineFormTraits {
  /** A named instance field (`options.instance`); the instance needs an explicit port. */
  instance: boolean;
  /** The "strict encryption" switch (`options.encrypt = strict`, TDS 8.0). */
  strictEncryption: boolean;
  /** The "read-only intent" switch (`options.application_intent = read_only`). */
  readOnlyIntent: boolean;
  /** TLS client certificates are supported. */
  clientCertificates: boolean;
  /** Hint under the database field. */
  databaseHint?: string;
  /** Placeholder of the database field. */
  databasePlaceholder?: string;
  /** Engine wording of each TLS mode's help (the common help otherwise). */
  tlsHelp?: Partial<Record<TlsMode, string>>;
  /** Engine options section of the General tab (M7). */
  options?: "mongodb" | "redis";
  /** Label of the database field (the common one otherwise). */
  databaseLabel?: string;
  /** Hint under the user name field. */
  userHint?: string;
}

const COMMON_TRAITS: EngineFormTraits = {
  instance: false,
  strictEncryption: false,
  readOnlyIntent: false,
  clientCertificates: true,
};

/** The SQL Server option keys the dialog manages (`oxbase_mssql::options`). */
export const MSSQL_OPTION = {
  instance: "instance",
  encrypt: "encrypt",
  encryptStrict: "strict",
  applicationIntent: "application_intent",
  readOnly: "read_only",
} as const;

/** The MongoDB option keys the dialog manages (`oxbase_mongo::options`). */
export const MONGO_OPTION = {
  hosts: "hosts",
  srv: "srv",
  replicaSet: "replica_set",
  authSource: "auth_source",
  authMechanism: "auth_mechanism",
  readPreference: "read_preference",
} as const;

/** The Redis option keys the dialog manages (`oxbase_redis::options`). */
export const REDIS_OPTION = {
  mode: "mode",
  nodes: "nodes",
  masterName: "master_name",
  sentinelAuth: "sentinel_auth",
  protocol: "protocol",
  namespaceSeparator: "namespace_separator",
} as const;

/** Every option key the dialog rewrites on save (others are kept for the same engine). */
const MANAGED_OPTIONS: readonly string[] = [
  "instance",
  "encrypt",
  "application_intent",
  ...Object.values(MONGO_OPTION),
  ...Object.values(REDIS_OPTION),
];

export const MONGO_AUTH_MECHANISMS: readonly MongoAuthMechanism[] = [
  "SCRAM-SHA-256",
  "SCRAM-SHA-1",
  "MONGODB-X509",
  "PLAIN",
];
export const MONGO_READ_PREFERENCES: readonly MongoReadPreference[] = [
  "primary",
  "primaryPreferred",
  "secondary",
  "secondaryPreferred",
  "nearest",
];
const REDIS_MODES: readonly RedisMode[] = ["standalone", "cluster", "sentinel"];
const DEFAULT_SEPARATOR = ":";

/** TLS modes that verify the server certificate (MongoDB PLAIN needs one). */
export const VERIFYING_TLS_MODES: readonly TlsMode[] = ["verify_ca", "verify_full"];

/**
 * Why `text` is not a seed list (`host`, `host:port` or `[v6]:port`, comma-separated), or
 * `null`; empty is no extra host.
 */
export function hostListProblem(text: string): string | null {
  const t = text.trim();
  if (t === "") return null;
  for (const raw of t.split(",")) {
    const entry = raw.trim();
    if (entry === "") return "Separate the hosts with single commas.";
    const m = /^(\[[0-9a-fA-F:.]+\]|[^\s:[\]/]+)(?::(\d+))?$/.exec(entry);
    if (!m) return `"${entry}" is not host or host:port.`;
    const port = m[2];
    if (port !== undefined && (Number(port) < 1 || Number(port) > 65535)) {
      return `The port of "${entry}" must be a number from 1 to 65535.`;
    }
  }
  return null;
}

/** TLS modes that check (or at least require) encryption; strict encryption needs one. */
export const STRICT_TLS_MODES: readonly TlsMode[] = ["require", "verify_ca", "verify_full"];

const ENGINE_FORM: Partial<Record<DialogEngine, Partial<EngineFormTraits>>> = {
  mssql: {
    instance: true,
    strictEncryption: true,
    readOnlyIntent: true,
    clientCertificates: false,
    databaseHint: "Leave blank to use the login's default database.",
    databasePlaceholder: "login default",
    tlsHelp: {
      disable:
        "Never encrypt (the client tells the server it cannot encrypt). The login and all data cross the network in plain text; a server that requires encryption refuses the connection.",
      prefer:
        "Encrypt when the server offers it, otherwise connect unencrypted. The server's certificate is trusted without any check (like TrustServerCertificate), so this does not protect against a fake server.",
      require:
        "Always encrypt and trust the server's certificate without checking it (like Encrypt with TrustServerCertificate). Stops eavesdropping, not a fake server.",
      verify_ca:
        "Always encrypt and check that the server's certificate was issued by a trusted authority (the CA file below, or your system's trusted list). The host name in the certificate is not checked.",
      verify_full:
        "Always encrypt, check the certificate authority and that the certificate belongs to this server's host name (TrustServerCertificate off). The safest choice for remote servers.",
    },
  },
  mongodb: {
    options: "mongodb",
    databaseHint: "The database the shell starts in (db). Every database stays browsable.",
    databasePlaceholder: "test",
    userHint: "Leave blank to connect without authentication.",
  },
  redis: {
    options: "redis",
    databaseLabel: "Database index (optional)",
    databaseHint: "The logical database number, 0 when blank. A cluster has only database 0.",
    databasePlaceholder: "0",
    userHint: "The ACL user; leave blank for the default user.",
  },
};

/** The dialog extras of `engine`. */
export function engineTraits(engine: DialogEngine): EngineFormTraits {
  return { ...COMMON_TRAITS, ...ENGINE_FORM[engine] };
}

export function defaultPort(engine: DialogEngine): string {
  const port = ENGINE_INFO[engine].port;
  return port === undefined ? "" : String(port);
}

export function isNetworked(engine: DialogEngine): boolean {
  return engine !== "sqlite";
}

export function emptyForm(engine: DialogEngine = "postgres"): ConnectionForm {
  return {
    name: "",
    engine,
    host: "localhost",
    port: defaultPort(engine),
    database: "",
    user: "",
    password: "",
    savePassword: true,
    filePath: "",
    tlsMode: "prefer",
    caCert: "",
    clientCert: "",
    clientKey: "",
    serverName: "",
    sshEnabled: false,
    sshHost: "",
    sshPort: "22",
    sshUser: "",
    sshAuth: "password",
    sshPassword: "",
    sshKeyFile: "",
    sshPassphrase: "",
    saveSshSecret: true,
    hostKeyPolicy: "strict",
    knownHostsPath: "",
    fingerprint: "",
    instance: "",
    strictEncryption: false,
    readOnlyIntent: false,
    hosts: "",
    srv: false,
    replicaSet: "",
    authSource: "",
    authMechanism: "",
    readPreference: "primary",
    redisMode: "standalone",
    nodes: "",
    masterName: "",
    sentinelAuth: "none",
    protocol: "auto",
    namespaceSeparator: DEFAULT_SEPARATOR,
    // The Settings window's default for new connections (used where the engine offers it).
    sessionMode: getSettings().sessionMode,
  };
}

/**
 * Engines that can connect for each run (`SessionMode::PerRun`): the server SQL engines, whose
 * drivers can tell whether a run left a transaction open.
 */
export function offersPerRun(engine: DialogEngine): boolean {
  return engine === "postgres" || engine === "mysql" || engine === "mariadb" || engine === "mssql";
}

/** Switches the engine, moving the port along when it was still the old engine's default. */
export function withEngine(form: ConnectionForm, engine: DialogEngine): ConnectionForm {
  const keepPort = form.port.trim() !== "" && form.port !== defaultPort(form.engine);
  return { ...form, engine, port: keepPort ? form.port : defaultPort(engine) };
}

function filePath(ref: PemRef | KeyRef | undefined): string {
  return ref?.source === "file" ? ref.path : "";
}

function isDialogEngine(engine: string): engine is DialogEngine {
  return (DIALOG_ENGINES as readonly string[]).includes(engine);
}

/** The form for editing `profile`. Secret fields start empty (the stored values stay put). */
export function formFromProfile(profile: ConnectionProfile): ConnectionForm {
  const engine = isDialogEngine(profile.engine) ? profile.engine : "postgres";
  const form = emptyForm(engine);
  form.name = profile.name;
  if (profile.address.kind === "network") {
    form.host = profile.address.host;
    form.port = profile.address.port === undefined ? "" : String(profile.address.port);
  } else {
    form.host = "";
    form.filePath = profile.address.path;
  }
  form.database = profile.database ?? "";
  form.user = profile.user ?? "";
  form.savePassword = profile.password?.source !== "prompt";
  form.tlsMode = profile.tls.mode;
  form.caCert = filePath(profile.tls.ca_cert);
  form.clientCert = filePath(profile.tls.client_cert);
  form.clientKey = filePath(profile.tls.client_key);
  form.serverName = profile.tls.server_name ?? "";
  form.sessionMode =
    profile.session_mode === "per_run" && offersPerRun(engine) ? "per_run" : "per_tab";
  const options = profile.options ?? {};
  const traits = engineTraits(engine);
  if (traits.instance) form.instance = options[MSSQL_OPTION.instance] ?? "";
  if (traits.strictEncryption) {
    form.strictEncryption = options[MSSQL_OPTION.encrypt] === MSSQL_OPTION.encryptStrict;
  }
  if (traits.readOnlyIntent) {
    form.readOnlyIntent = options[MSSQL_OPTION.applicationIntent] === MSSQL_OPTION.readOnly;
  }
  if (traits.options === "mongodb") {
    form.hosts = options[MONGO_OPTION.hosts] ?? "";
    form.srv = options[MONGO_OPTION.srv] === "true";
    form.replicaSet = options[MONGO_OPTION.replicaSet] ?? "";
    form.authSource = options[MONGO_OPTION.authSource] ?? "";
    const mechanism = options[MONGO_OPTION.authMechanism] ?? "";
    form.authMechanism = (MONGO_AUTH_MECHANISMS as readonly string[]).includes(mechanism)
      ? (mechanism as MongoAuthMechanism)
      : "";
    const preference = options[MONGO_OPTION.readPreference] ?? "primary";
    form.readPreference = (MONGO_READ_PREFERENCES as readonly string[]).includes(preference)
      ? (preference as MongoReadPreference)
      : "primary";
  }
  if (traits.options === "redis") {
    const mode = options[REDIS_OPTION.mode] ?? "standalone";
    form.redisMode = (REDIS_MODES as readonly string[]).includes(mode)
      ? (mode as RedisMode)
      : "standalone";
    form.nodes = options[REDIS_OPTION.nodes] ?? "";
    form.masterName = options[REDIS_OPTION.masterName] ?? "";
    form.sentinelAuth = options[REDIS_OPTION.sentinelAuth] === "same" ? "same" : "none";
    const protocol = options[REDIS_OPTION.protocol];
    form.protocol = protocol === "resp2" || protocol === "resp3" ? protocol : "auto";
    form.namespaceSeparator = options[REDIS_OPTION.namespaceSeparator] ?? DEFAULT_SEPARATOR;
  }
  const ssh = profile.ssh;
  if (ssh) {
    form.sshEnabled = true;
    form.sshHost = ssh.host;
    form.sshPort = String(ssh.port);
    form.sshUser = ssh.user;
    form.sshAuth = ssh.auth.method;
    if (ssh.auth.method === "private_key") {
      form.sshKeyFile = filePath(ssh.auth.key);
      form.saveSshSecret = ssh.auth.passphrase?.source !== "prompt";
    } else if (ssh.auth.method === "password") {
      form.saveSshSecret = ssh.auth.password.source !== "prompt";
    }
    form.hostKeyPolicy = ssh.known_hosts.policy;
    if (ssh.known_hosts.policy === "fingerprint") {
      form.fingerprint = ssh.known_hosts.fingerprint;
    } else {
      form.knownHostsPath = ssh.known_hosts.path ?? "";
    }
  }
  return form;
}

function validPort(text: string, required: boolean): boolean {
  const t = text.trim();
  if (t === "") return !required;
  if (!/^\d+$/.test(t)) return false;
  const n = Number(t);
  return n >= 1 && n <= 65535;
}

export type ValidationPurpose = "save" | "test";

/** Field errors, keyed by field. `test` does not need a name. */
export function validateForm(form: ConnectionForm, purpose: ValidationPurpose): FormErrors {
  const errors: FormErrors = {};
  if (purpose === "save" && form.name.trim() === "") {
    errors.name = "Enter a name for this connection.";
  }
  if (isNetworked(form.engine)) {
    if (form.host.trim() === "") errors.host = "Enter the server's host name or IP address.";
    if (!validPort(form.port, false)) errors.port = "Port must be a number from 1 to 65535.";

    const traits = engineTraits(form.engine);
    if (traits.instance && form.instance.trim() !== "" && form.port.trim() === "") {
      errors.port = "Enter the named instance's port (the SQL Server Browser service is not used).";
    }
    if (traits.instance) {
      const problem = instanceProblem(form.instance.trim());
      if (problem) errors.instance = problem;
    }
    if (traits.options === "mongodb") mongoErrors(form, errors);
    if (traits.options === "redis") redisErrors(form, errors);
    if (
      traits.strictEncryption &&
      form.strictEncryption &&
      !STRICT_TLS_MODES.includes(form.tlsMode)
    ) {
      errors.strictEncryption =
        "Strict encryption needs the encryption mode Require, Verify certificate authority or Verify certificate and host name.";
    }

    if (traits.clientCertificates) {
      const cert = form.clientCert.trim();
      const key = form.clientKey.trim();
      if (cert !== "" && key === "") {
        errors.clientKey = "Choose the key for the client certificate.";
      }
      if (key !== "" && cert === "") errors.clientCert = "Choose the certificate for this key.";
    }

    if (form.sshEnabled) {
      if (form.sshHost.trim() === "") errors.sshHost = "Enter the SSH server's host name.";
      if (!validPort(form.sshPort, true)) errors.sshPort = "Port must be a number from 1 to 65535.";
      if (form.sshUser.trim() === "") errors.sshUser = "Enter the SSH user name.";
      if (form.sshAuth === "private_key" && form.sshKeyFile.trim() === "") {
        errors.sshKeyFile = "Choose the private key file.";
      }
      if (form.hostKeyPolicy === "fingerprint" && !/^SHA256:\S+$/.test(form.fingerprint.trim())) {
        errors.fingerprint = "Enter the fingerprint as SHA256:… (from ssh-keygen -lf).";
      }
    }
  } else if (form.filePath.trim() === "") {
    errors.filePath = "Choose the SQLite database file.";
  }
  return errors;
}

function mongoErrors(form: ConnectionForm, errors: FormErrors) {
  const hostsProblem = hostListProblem(form.hosts);
  if (hostsProblem) errors.hosts = hostsProblem;
  if (form.srv) {
    if (form.hosts.trim() !== "" || form.sshEnabled) {
      errors.srv = "An SRV name stands alone: remove the other hosts and turn off the SSH tunnel.";
    }
    // The port field is not sent with SRV (it is disabled), so it never blocks.
    delete errors.port;
  } else if (form.sshEnabled && form.hosts.trim() !== "" && !errors.hosts) {
    errors.hosts = "An SSH tunnel reaches one server: remove the other hosts.";
  }
  if (form.authMechanism === "PLAIN" && !VERIFYING_TLS_MODES.includes(form.tlsMode)) {
    errors.authMechanism =
      "PLAIN sends the password itself: set the encryption mode to Verify certificate authority or Verify certificate and host name.";
  }
}

function redisErrors(form: ConnectionForm, errors: FormErrors) {
  const nodesProblem = hostListProblem(form.nodes);
  if (nodesProblem) errors.nodes = nodesProblem;
  const db = form.database.trim();
  if (db !== "" && (!/^\d+$/.test(db) || Number(db) > 65535)) {
    errors.database = "Enter the database number (0, 1, 2, …).";
  } else if (form.redisMode === "cluster" && db !== "" && Number(db) !== 0) {
    errors.database = "A Redis cluster has only database 0.";
  }
  if (form.redisMode === "sentinel" && form.masterName.trim() === "") {
    errors.masterName = "Enter the name of the master the sentinels monitor.";
  }
  if (form.redisMode !== "standalone" && form.sshEnabled) {
    errors.redisMode =
      "An SSH tunnel reaches one node: use it with a standalone server only, or turn SSH off.";
  }
}

export function firstError(errors: FormErrors): FormField | undefined {
  return FIELD_ORDER.find((field) => errors[field] !== undefined);
}

const PENDING_KEY = "pending";

/**
 * How a secret slot is referenced. A typed value or a previously saved one with "save" on is
 * kept in the keychain; "save" off means the user is asked on every connect.
 */
function secretRef(
  typed: string,
  save: boolean,
  previous: SecretRef | undefined,
): SecretRef | undefined {
  if (!save) return { source: "prompt" };
  if (typed !== "") return { source: "keychain", key: PENDING_KEY };
  // Nothing typed: keep a saved secret; otherwise there is none (e.g. passwordless login).
  return previous?.source === "keychain" ? previous : undefined;
}

function optionalText(value: string): string | undefined {
  const t = value.trim();
  return t === "" ? undefined : t;
}

function pemRef(path: string, previous: PemRef | undefined): PemRef | undefined {
  const p = path.trim();
  if (p !== "") return { source: "file", path: p };
  return previous?.source === "inline" ? previous : undefined;
}

function keyRef(path: string, previous: KeyRef | undefined): KeyRef | undefined {
  const p = path.trim();
  if (p !== "") return { source: "file", path: p };
  return previous?.source === "keychain" ? previous : undefined;
}

function knownHosts(form: ConnectionForm): KnownHostsPolicy {
  if (form.hostKeyPolicy === "fingerprint") {
    return { policy: "fingerprint", fingerprint: form.fingerprint.trim() };
  }
  const path = optionalText(form.knownHostsPath);
  return path === undefined ? { policy: form.hostKeyPolicy } : { policy: form.hostKeyPolicy, path };
}

function sshAuth(form: ConnectionForm, previous: SshAuthSettings | undefined): SshAuthSettings {
  switch (form.sshAuth) {
    case "agent":
      return { method: "agent" };
    case "password": {
      const prev = previous?.method === "password" ? previous.password : undefined;
      return {
        method: "password",
        password: secretRef(form.sshPassword, form.saveSshSecret, prev) ?? { source: "prompt" },
      };
    }
    case "private_key": {
      const prevAuth = previous?.method === "private_key" ? previous : undefined;
      const key = keyRef(form.sshKeyFile, prevAuth?.key) ?? {
        source: "file",
        path: form.sshKeyFile.trim(),
      };
      // A passphrase only exists for an encrypted key. Nothing typed and nothing saved means an
      // unencrypted key, also with saving off: "ask every time" is chosen by typing the
      // passphrase once with saving off, and stays chosen until saving is turned back on.
      const unencrypted =
        !form.saveSshSecret &&
        form.sshPassphrase === "" &&
        prevAuth?.passphrase?.source !== "prompt";
      const passphrase = unencrypted
        ? undefined
        : secretRef(form.sshPassphrase, form.saveSshSecret, prevAuth?.passphrase);
      return passphrase === undefined
        ? { method: "private_key", key }
        : { method: "private_key", key, passphrase };
    }
  }
}

/**
 * The profile to send to the core. `base` is the profile being edited (its unknown fields and
 * non-file references are preserved); without it a new profile is built.
 */
export function profileFromForm(
  form: ConnectionForm,
  base?: ConnectionProfile,
  fallbackName?: string,
): ConnectionProfile {
  const name = form.name.trim() || fallbackName || "";
  const networked = isNetworked(form.engine);
  const traits = engineTraits(form.engine);
  // An SRV name has no port (DNS gives the hosts and ports).
  const port = traits.options === "mongodb" && form.srv ? "" : form.port.trim();

  const profile: ConnectionProfile = {
    ...(base ?? { schema_version: 1, id: NEW_PROFILE_ID }),
    name,
    engine: form.engine,
    address: networked
      ? port === ""
        ? { kind: "network", host: form.host.trim() }
        : { kind: "network", host: form.host.trim(), port: Number(port) }
      : { kind: "file", path: form.filePath.trim() },
    tls: { ...(base?.tls ?? {}), mode: networked ? form.tlsMode : "disable" } as TlsSettings,
  };

  const database = optionalText(form.database);
  const user = optionalText(form.user);
  const password = secretRef(form.password, form.savePassword, base?.password);
  setOptional(profile, "database", database);
  setOptional(profile, "user", user);
  // SQLite has no login; an "ask every time" password would only get in the way.
  setOptional(profile, "password", networked || form.password !== "" ? password : undefined);

  const clientCerts = networked && traits.clientCertificates;
  const tls = profile.tls;
  setOptional(tls, "ca_cert", networked ? pemRef(form.caCert, base?.tls.ca_cert) : undefined);
  setOptional(
    tls,
    "client_cert",
    clientCerts ? pemRef(form.clientCert, base?.tls.client_cert) : undefined,
  );
  setOptional(
    tls,
    "client_key",
    clientCerts ? keyRef(form.clientKey, base?.tls.client_key) : undefined,
  );
  setOptional(tls, "server_name", networked ? optionalText(form.serverName) : undefined);

  if (networked && form.sshEnabled) {
    const ssh: SshSettings = {
      ...(base?.ssh ?? {}),
      host: form.sshHost.trim(),
      port: Number(form.sshPort.trim()),
      user: form.sshUser.trim(),
      auth: sshAuth(form, base?.ssh?.auth),
      known_hosts: knownHosts(form),
    };
    profile.ssh = ssh;
  } else {
    delete profile.ssh;
  }

  // Engine options: the keys the dialog manages are rewritten, any others are kept while the
  // engine stays the same (they are engine-specific: a driver refuses keys it does not know).
  const options: Record<string, string> =
    base?.engine === form.engine ? { ...(base.options ?? {}) } : {};
  for (const key of MANAGED_OPTIONS) delete options[key];
  const instance = traits.instance ? optionalText(form.instance) : undefined;
  if (instance !== undefined) options[MSSQL_OPTION.instance] = instance;
  if (traits.strictEncryption && form.strictEncryption) {
    options[MSSQL_OPTION.encrypt] = MSSQL_OPTION.encryptStrict;
  }
  if (traits.readOnlyIntent && form.readOnlyIntent) {
    options[MSSQL_OPTION.applicationIntent] = MSSQL_OPTION.readOnly;
  }
  if (traits.options === "mongodb") Object.assign(options, mongoOptions(form));
  if (traits.options === "redis") Object.assign(options, redisOptions(form));
  setOptional(profile, "options", Object.keys(options).length > 0 ? options : undefined);
  // Per tab is the default and is not written out.
  setOptional(
    profile,
    "session_mode",
    offersPerRun(form.engine) && form.sessionMode === "per_run" ? "per_run" : undefined,
  );
  return profile;
}

/** The `options` of a MongoDB profile from the form (defaults left out). */
export function mongoOptions(form: ConnectionForm): Record<string, string> {
  const out: Record<string, string> = {};
  const hosts = form.srv ? undefined : optionalText(form.hosts);
  if (hosts !== undefined) {
    out[MONGO_OPTION.hosts] = hosts
      .split(",")
      .map((h) => h.trim())
      .join(",");
  }
  if (form.srv) out[MONGO_OPTION.srv] = "true";
  const replicaSet = optionalText(form.replicaSet);
  if (replicaSet !== undefined) out[MONGO_OPTION.replicaSet] = replicaSet;
  const authSource = optionalText(form.authSource);
  if (authSource !== undefined) out[MONGO_OPTION.authSource] = authSource;
  if (form.authMechanism !== "") out[MONGO_OPTION.authMechanism] = form.authMechanism;
  if (form.readPreference !== "primary") out[MONGO_OPTION.readPreference] = form.readPreference;
  return out;
}

/** The `options` of a Redis profile from the form (defaults left out). */
export function redisOptions(form: ConnectionForm): Record<string, string> {
  const out: Record<string, string> = {};
  if (form.redisMode !== "standalone") out[REDIS_OPTION.mode] = form.redisMode;
  const nodes = form.redisMode === "standalone" ? undefined : optionalText(form.nodes);
  if (nodes !== undefined) {
    out[REDIS_OPTION.nodes] = nodes
      .split(",")
      .map((h) => h.trim())
      .join(",");
  }
  if (form.redisMode === "sentinel") {
    out[REDIS_OPTION.masterName] = form.masterName.trim();
    if (form.sentinelAuth === "same") out[REDIS_OPTION.sentinelAuth] = "same";
  }
  if (form.protocol !== "auto") out[REDIS_OPTION.protocol] = form.protocol;
  // Kept as typed: an empty separator turns namespaces off.
  if (form.namespaceSeparator !== DEFAULT_SEPARATOR) {
    out[REDIS_OPTION.namespaceSeparator] = form.namespaceSeparator;
  }
  return out;
}

function setOptional<T extends object, K extends keyof T>(
  target: T,
  key: K,
  value: T[K] | undefined,
) {
  if (value === undefined) {
    delete target[key];
  } else {
    target[key] = value;
  }
}

/** The secret values typed in the form that apply to its current settings. */
export function secretsFromForm(form: ConnectionForm): SecretInput {
  const secrets: SecretInput = {};
  if (form.password !== "") secrets.password = form.password;
  if (isNetworked(form.engine) && form.sshEnabled) {
    if (form.sshAuth === "password" && form.sshPassword !== "") {
      secrets.ssh_password = form.sshPassword;
    }
    if (form.sshAuth === "private_key" && form.sshPassphrase !== "") {
      secrets.ssh_key_passphrase = form.sshPassphrase;
    }
  }
  return secrets;
}

/** Which secrets a saved profile asks for on every connect. */
export function promptedSecrets(profile: ConnectionProfile): Array<keyof SecretInput> {
  const out: Array<keyof SecretInput> = [];
  if (profile.password?.source === "prompt") out.push("password");
  const auth = profile.ssh?.auth;
  if (auth?.method === "password" && auth.password.source === "prompt") out.push("ssh_password");
  if (auth?.method === "private_key" && auth.passphrase?.source === "prompt") {
    out.push("ssh_key_passphrase");
  }
  return out;
}
