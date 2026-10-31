/**
 * TypeScript mirrors of the JSON the connection commands exchange. The shapes follow the serde
 * forms in `oxbase-store` (profiles), `oxbase-services` (sessions) and `app-tauri` (errors).
 * Secret values never appear in these types: profiles only reference them.
 */

export type Engine = "postgres" | "mysql" | "mariadb" | "sqlite" | "mssql" | "mongodb" | "redis";

/** Engines the connection dialog offers at M1. */
export const DIALOG_ENGINES = ["postgres", "mysql", "mariadb", "sqlite"] as const;
export type DialogEngine = (typeof DIALOG_ENGINES)[number];

export type TlsMode = "disable" | "prefer" | "require" | "verify_ca" | "verify_full";

export type SecretRef = { source: "keychain"; key: string } | { source: "prompt" };
export type PemRef = { source: "file"; path: string } | { source: "inline"; pem: string };
export type KeyRef = { source: "file"; path: string } | { source: "keychain"; key: string };

export type ProfileAddress =
  | { kind: "network"; host: string; port?: number }
  | { kind: "file"; path: string };

export interface TlsSettings {
  mode: TlsMode;
  ca_cert?: PemRef;
  client_cert?: PemRef;
  client_key?: KeyRef;
  server_name?: string;
}

export type SshAuthSettings =
  | { method: "password"; password: SecretRef }
  | { method: "private_key"; key: KeyRef; passphrase?: SecretRef }
  | { method: "agent" };

export type KnownHostsPolicy =
  | { policy: "strict"; path?: string }
  | { policy: "accept_new"; path?: string }
  | { policy: "fingerprint"; fingerprint: string };

export interface SshSettings {
  host: string;
  port: number;
  user: string;
  auth: SshAuthSettings;
  known_hosts: KnownHostsPolicy;
  connect_timeout_secs?: number;
  keepalive_secs?: number;
}

/**
 * A saved connection (`oxbase_store::ConnectionProfile`). Fields this build does not know are
 * kept at runtime because the UI edits a copy of the object it received.
 */
export interface ConnectionProfile {
  schema_version: number;
  id: string;
  name: string;
  engine: Engine;
  address: ProfileAddress;
  database?: string;
  user?: string;
  password?: SecretRef;
  tls: TlsSettings;
  ssh?: SshSettings;
  connect_timeout_secs?: number;
  options?: Record<string, string>;
}

/** Secret values typed for one call (`app_tauri::profiles::SecretInput`). */
export interface SecretInput {
  password?: string;
  ssh_password?: string;
  ssh_key_passphrase?: string;
}

export interface ServerVersion {
  major: number;
  minor: number;
  patch: number;
  build: number;
  raw: string;
}

/** `oxbase_services::ConnectionInfo`. */
export interface ConnectionInfo {
  server_version: ServerVersion;
  tls_in_use: boolean;
  via_ssh: boolean;
  latency_ms: number;
}

/** `oxbase_services::SessionSummary`. */
export interface SessionSummary {
  id: string;
  profile_id: string;
  profile_name: string;
  engine: Engine;
  info: ConnectionInfo;
  opened_at: string;
}

export type ErrorCategory =
  | "connection"
  | "auth"
  | "timeout"
  | "cancelled"
  | "unsupported"
  | "sql"
  | "not_found"
  | "invalid_input"
  | "io"
  | "internal";

/** `app_tauri::CommandError`: what every failing command rejects with. */
export interface CommandError {
  category: ErrorCategory;
  message: string;
}

/** The nil UUID: tells `save_profile` to assign a fresh id. */
export const NEW_PROFILE_ID = "00000000-0000-0000-0000-000000000000";

export const ENGINE_INFO: Record<Engine, { label: string; short: string; port?: number }> = {
  postgres: { label: "PostgreSQL", short: "PG", port: 5432 },
  mysql: { label: "MySQL", short: "My", port: 3306 },
  mariadb: { label: "MariaDB", short: "Ma", port: 3306 },
  sqlite: { label: "SQLite", short: "SL" },
  mssql: { label: "SQL Server", short: "MS", port: 1433 },
  mongodb: { label: "MongoDB", short: "Mo", port: 27017 },
  redis: { label: "Redis", short: "Rd", port: 6379 },
};

/** Where a profile points, for lists: `host:port` or the file path. */
export function profileLocation(profile: ConnectionProfile): string {
  const address = profile.address;
  if (address.kind === "file") return address.path;
  const port = address.port ?? ENGINE_INFO[profile.engine].port;
  return port === undefined ? address.host : `${address.host}:${port}`;
}
