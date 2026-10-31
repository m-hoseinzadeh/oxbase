/**
 * The "New / Edit connection" form model: defaults, validation and conversion to and from a
 * {@link ConnectionProfile}. Pure functions, so they are easy to test.
 */
import {
  type ConnectionProfile,
  type DialogEngine,
  ENGINE_INFO,
  type KeyRef,
  type KnownHostsPolicy,
  NEW_PROFILE_ID,
  type PemRef,
  type SecretInput,
  type SecretRef,
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
}

export type FormField = keyof ConnectionForm;
export type FormErrors = Partial<Record<FormField, string>>;
export type FormTab = "general" | "tls" | "ssh";

/** Which tab each validated field lives on (to jump to the first error). */
export const FIELD_TAB: Partial<Record<FormField, FormTab>> = {
  name: "general",
  host: "general",
  port: "general",
  filePath: "general",
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
  "filePath",
  "caCert",
  "clientCert",
  "clientKey",
  "sshHost",
  "sshPort",
  "sshUser",
  "sshKeyFile",
  "fingerprint",
];

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
  };
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
  return engine === "postgres" || engine === "mysql" || engine === "mariadb" || engine === "sqlite";
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

    const cert = form.clientCert.trim();
    const key = form.clientKey.trim();
    if (cert !== "" && key === "") errors.clientKey = "Choose the key for the client certificate.";
    if (key !== "" && cert === "") errors.clientCert = "Choose the certificate for this key.";

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
  const port = form.port.trim();

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

  const tls = profile.tls;
  setOptional(tls, "ca_cert", networked ? pemRef(form.caCert, base?.tls.ca_cert) : undefined);
  setOptional(
    tls,
    "client_cert",
    networked ? pemRef(form.clientCert, base?.tls.client_cert) : undefined,
  );
  setOptional(
    tls,
    "client_key",
    networked ? keyRef(form.clientKey, base?.tls.client_key) : undefined,
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
  return profile;
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
