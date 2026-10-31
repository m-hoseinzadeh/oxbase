# M1 contracts

Status: frozen for M1 on 2026-10-31 (Design step); implemented and integrated on 2026-10-31
(see *M1 as built* at the end for where the implementation differs from or extends this text). M1 exit check (PLAN.md §6): connect through
SSH and TLS to PostgreSQL, MySQL and MariaDB, open SQLite, and cancel a running statement on each.

Changes to these contracts go through the Design owner. Workstreams code against them and must not
change their signatures. Adding a private helper is always fine.

## 1. Who owns what

| Area | Crate / file | Owner | State at hand-off |
|---|---|---|---|
| Shared option types: `Secret`, `Endpoint`, `TlsMode`, `TlsOptions`, `PemSource`, `KeySource`, `SshOptions`, `SshAuth`, `KnownHostsPolicy` | `oxbase-types` (`secret.rs`, `net.rs`) | Design | done |
| `Driver` / `Connection` / `Tx`, `ConnectParams`, `DriverRegistry` | `oxbase-driver` | Design | done |
| TLS: `client_config`, `server_name`, PEM/key loaders, the crypto provider | `oxbase-net::tls` | Design | done, tested |
| SSH: `TunnelProvider`, `TunnelHandle`, `TunnelKey`, `TunnelPool` | `oxbase-net::ssh` | Design (contract + pool) | pool done |
| `RusshTunnelProvider::open` | `oxbase-net::ssh` | **net** | done |
| PostgreSQL driver | `oxbase-pg` | **pg** | done |
| MySQL + MariaDB driver | `oxbase-mysql` | **mysql** | done |
| SQLite driver | `oxbase-sqlite` | **sqlite** | done |
| `SecretStore`, `SecretKey`, `MemorySecretStore`, `ConnectionProfile` and its settings, `ProfileRepository`, `MemoryProfileRepository` | `oxbase-store` | Design | done, tested |
| `KeyringSecretStore` methods, persistent `ProfileRepository` | `oxbase-store` | **store** | done (`FileProfileRepository`) |
| `ConnectionManager::{resolve, test_connection, open}` | `oxbase-services` | **services** | done |
| `ConnectionManager::{new, close, close_all, get, list_open, register}` | `oxbase-services` | Design | done |
| Fixtures: SSH bastion, test PKI, TLS-only users | `tests/docker`, `scripts/` | **fixtures** | done |
| Live SSH/TLS/cancel conformance tests | `oxbase-conformance` (`src/connect.rs`) | **conformance** | done |

## 2. Addressing: server endpoint vs dial endpoint

A connection has two addresses, and mixing them up breaks TLS or cancel through a tunnel.

- **Server endpoint**: `ConnectParams.host` + `effective_port()`, returned by
  `ConnectParams::server_endpoint()`. This is the server the user configured. The SSH tunnel
  forwards to it (resolved *by the SSH server*, so `postgres:5432` works inside a Docker
  network). TLS verifies it. The UI and error messages show it.
- **Dial endpoint**: `ConnectParams::dial_endpoint()`. It is `dial_addr` (the tunnel's
  `TunnelHandle::local_addr()`, such as `127.0.0.1:41234`) when set, and the server endpoint
  otherwise.

Rules for drivers:

1. Open **every** socket at `dial_endpoint()`. This includes out-of-band sockets: the PostgreSQL
   `CancelRequest` and the MySQL/MariaDB `KILL QUERY <id>` side connection. Through SSH, the
   server host may not resolve at all from the client machine.
2. Verify TLS against `ConnectParams::tls_server_name()`. That is `tls.server_name` if set, and
   the server host otherwise. **Never** use the dial address, so `VerifyFull` still works through
   a tunnel. The same name is sent as SNI. IP literals verify against IP SANs and send no SNI.
3. Never open SSH tunnels and never dial `ssh.host`. `ConnectParams::validate()` rejects `ssh`
   without `dial_addr`. The connection manager opens the tunnel and sets `dial_addr`.
4. Library mapping hints:
   - `tokio-postgres`: `Config::host(server_host)` + `hostaddr(dial ip)` + `port(dial port)`.
     `hostaddr` is what gets dialled. `host` is what `MakeTlsConnect` receives as the domain.
     `CancelToken::cancel_query` reuses the dialled socket address.
   - `mysql_async`: `ip_or_hostname(dial host)`, `tcp_port(dial port)`, and
     `SslOpts::with_danger_tls_hostname_override(Some(tls_server_name))`. The cancel side connection
     uses the same `Opts`.

## 3. TLS (`oxbase_net::tls`)

`client_config(&TlsOptions) -> Result<Arc<rustls::ClientConfig>>` always uses the **ring**
provider (`tls::crypto_provider()`).

| `TlsMode` | Encrypts | Chain | Name | Notes |
|---|---|---|---|---|
| `Disable` | no | - | - | `client_config` returns `InvalidInput`. Drivers use plaintext. |
| `Prefer` (default) | if offered | no | no | Insecure. The driver falls back to plaintext when the server has no TLS. |
| `Require` | yes | no | no | Insecure (`InsecureNoVerification`). A CA file does **not** upgrade it to `VerifyCa`. |
| `VerifyCa` | yes | yes | no | `ChainOnlyVerification`: WebPKI chain + validity dates, name mismatch accepted. |
| `VerifyFull` | yes | yes | yes | Standard WebPKI. |

- Roots: `ca_cert` if set (then *only* those CAs). Otherwise the platform store
  (`rustls-native-certs`, loaded once per process). An empty store is `InvalidInput`.
- Client identity: `client_cert` (PEM chain) + `client_key` (unencrypted PKCS#8/PKCS#1/SEC1,
  file or inline `Secret`). They must be given together.
- For libraries that build their own rustls config (`mysql_async`), use `load_pem`,
  `load_key_pem`, `load_certificates` and `load_private_key`, and keep the semantics above:
  use `with_disable_built_in_roots(true)` when `ca_cert` is set,
  `with_danger_accept_invalid_certs` for Require/Prefer, and
  `with_danger_skip_domain_validation` for VerifyCa. Without `ca_cert`, `mysql_async` trusts its bundled
  webpki-roots instead of the platform store. That is acceptable for M1, but record it in the
  driver docs. Call `install_default_crypto_provider()` before
  such a library builds a config. It is idempotent.
- ALPN (PostgreSQL direct TLS) is per driver: clone the config and set `alpn_protocols`.
- `Connection::tls_in_use()` (a default method, `false`) must be overridden by every network
  driver to report whether the session is actually encrypted.

## 4. SSH (`oxbase_net::ssh`)

```rust
#[async_trait]
pub trait TunnelProvider: Send + Sync {
    async fn open(&self, ssh: &SshOptions, target: &Endpoint, cancel: CancellationToken)
        -> Result<TunnelHandle>;
}
```

Invariants (the full list is in the module docs):

- The listener binds `127.0.0.1:0` only. It accepts any number of connections while the handle
  lives, one `direct-tcpip` channel each, so cancel side connections work.
- Host keys follow `KnownHostsPolicy`:
  - `Strict { path }`: the key must already be in the file.
  - `AcceptNew { path }`: trust on first use. A *changed* key still fails.
  - `Fingerprint { fingerprint }`: exact `SHA256:` match.

  A failure is `Error::Connection` with a message that starts with `SSH host key` and includes
  the presented fingerprint.
- Errors: auth rejected is `Error::Auth`, deadline is `Error::Timeout`, cancelled is
  `Error::Cancelled`, anything else is `Error::Connection`. No secret appears in any message or
  log.
- `open` does not probe the target.
- Lifetime: dropping the handle stops the listener, closes the channels and disconnects. When
  the session dies, the provider cancels the handle's `closed` token. `TunnelPool` shares
  tunnels by `TunnelKey` (SSH endpoint, SSH user, target) through weak references, never
  hands out closed tunnels, and serialises concurrent opens of one key.

## 5. Secrets and profiles (`oxbase-store`)

- `Secret` (in `oxbase-types`, re-exported everywhere) has no `Display` and no `Serialize`. Its
  `Debug` prints `Secret(***)`, and it is zeroed on drop. Its equality check takes the same
  time wherever the strings differ. Every type that can hold a secret (`ConnectParams`,
  `TlsOptions`, `SshOptions`, `ProvidedSecrets`, ...) inherits redacted `Debug`.
  **Invariant:** no password, passphrase or key text appears in `Debug`/`Display`, logs, errors
  or panics. Tests assert this with marker strings.
- `SecretStore` (async): `get -> Option<Secret>` (missing is `Ok(None)`), `set` (overwrites),
  `delete -> bool`. Keys are `SecretKey`s: `[A-Za-z0-9._:/-]{1,200}`. Profile secrets use
  `connection/<uuid>/<slot>`. `KeyringSecretStore` uses service `dev.oxbase.app` and
  account = key text, runs blocking calls on `spawn_blocking`, and maps an unavailable keychain
  to `Unsupported`.
- `ConnectionProfile` is versioned JSON (`schema_version`, currently 1). Secrets appear only as
  `SecretRef { keychain{key} | prompt }` and `KeyRef { file{path} | keychain{key} }`.
  Certificates may be inline (`PemRef::Inline`). Unknown fields are kept in `extra` (top level,
  `tls` and `ssh`) and written back unchanged. Documents from newer versions load and keep their
  version number.
- `ProfileRepository`: `list` (sorted by name), `get`, `save` (upsert, validates), `delete`.
  Deleting a profile does not delete its secrets. The caller deletes `profile.secret_keys()`.
- `oxbase-services::profiles` (used by the app; the CLI has no connection commands yet but will call the same functions) saves and deletes profiles together
  with their secrets: `save_profile(repo, store, profile, &ProvidedSecrets)`, `delete_profile`,
  `restrict_to_saved_destination` (before testing an edited profile) and `same_destination`.
  Keychain references are normalized to the profile's own keys. **Invariant:** a stored secret
  that is sent to a server (database password, SSH password) only goes to the destination it was
  saved for (same engine, address, user, TLS settings, SSH host, port, user and host-key policy).
  `save_profile` refuses an edit or a new profile that would reuse a stored one for another
  destination (`InvalidInput`: retype it); a test of an edited, unsaved profile uses only typed
  values for those slots. Key passphrases and key material are local and not affected.
- The real keychain backend is covered by `scripts/keychain-tests.sh` (CI job `keychain` on
  Linux, macOS and Windows).

## 6. Connection manager (`oxbase-services`)

```rust
ConnectionManager::new(DriverRegistry, Arc<dyn SecretStore>, Arc<dyn TunnelProvider>)
async fn resolve(&self, &ConnectionProfile, &ProvidedSecrets, CancellationToken) -> Result<ResolvedConnection>
async fn test_connection(&self, &ConnectionProfile, &ProvidedSecrets, CancellationToken) -> Result<ConnectionInfo>
async fn open(&self, &ConnectionProfile, &ProvidedSecrets, CancellationToken) -> Result<SessionHandle>
async fn close(&self, SessionId) -> Result<()>
fn list_open(&self) -> Vec<SessionSummary>
```

`ConnectionInfo { server_version, tls_in_use, via_ssh, latency_ms }`. `latency_ms` is the wall
time of the whole connect. `ProvidedSecrets` answers `SecretRef::Prompt` fields and overrides
stored ones. The flow is: resolve secrets, then `TunnelPool::acquire(ssh, server endpoint)`,
then `ConnectParams` (server endpoint + `dial_addr` = tunnel), then `DriverRegistry::connect`,
then register. `SessionHandle` drops the connection before the tunnel. `close` closes the
connection, then releases the tunnel. Every step honours cancellation. A failure after the
tunnel opened releases it.

`ProvidedSecrets` and the cancellation token are additions to the
`test_connection(&profile)` / `open(&profile)` sketch: prompt-only passwords need the first,
and PLAN rule 4 requires the second.

## 7. Cancellation

- `Connection::execute` honours its token. `Connection::cancel_running` cancels the running
  statement. The stream ends with one `Err(Error::Cancelled)`.
- PostgreSQL: send a `CancelRequest` to the **dial endpoint** (with TLS settings matching the
  session). MySQL/MariaDB: open a side connection to the **dial endpoint** with the same
  credentials and TLS, run `KILL QUERY <connection id>`, then close it. Only send it while the
  targeted statement is still running. SQLite: `sqlite3_interrupt` via rusqlite's
  `InterruptHandle`.
- A cancel never closes the session. The connection stays usable afterwards.

## 8. Dependencies

There is one TLS stack: rustls 0.23 with ring, everywhere. No OpenSSL and no aws-lc.
`cargo tree -i openssl-sys` and `cargo tree -i aws-lc-sys` must be empty. Workspace pins:

- tokio-postgres 0.7.18, tokio-postgres-rustls 0.14.0 (ring)
- mysql_async 0.37.1 (`default-rustls-ring`)
- rusqlite 0.41.1 (bundled)
- russh 0.65.0 (ring, flate2, rsa)
- rustls 0.23.46, rustls-pki-types 1.15.1, tokio-rustls 0.26.6, rustls-native-certs 0.8.5
- keyring 4.2.0 (`v1`: native Apple/Windows stores, zbus Secret Service with Rust crypto)
- rcgen 0.14.10 and tempfile 3.27.0 (dev)

`mysql_async`'s rustls feature links `webpki-roots` (CDLA-Permissive-2.0), so `deny.toml` must
allow that license.

## 9. Fixtures (`tests/fixtures.toml`)

- `[tls]`: the PKI under `target/fixtures/tls`. One CA signs a server certificate with the SANs
  localhost, 127.0.0.1, postgres, mysql and mariadb, and a client certificate with
  CN `oxbase_tls`.
- `[engines.<pg|mysql|mariadb>.tls_user]`: user `oxbase_tls`, which the server allows **only**
  over TLS.
- `[ssh]`: an OpenSSH bastion on 127.0.0.1:15422 with user/password, key and encrypted-key
  auth, a fixed host key, `known_hosts` and a fingerprint file. `[ssh.targets]` holds the
  in-network addresses (`postgres:5432`, `mysql:3306`, `mariadb:3306`).

## 10. How conformance checks these contracts

Unit tests that already exist:

- `oxbase-types`: redaction, endpoints, TLS mode semantics.
- `oxbase-driver`: dial vs server endpoint, SSH validation.
- `oxbase-net`: rcgen handshakes for every mode, mTLS, the tunnel name rule, error typing and
  no-leak checks; pool sharing, reopen and cancel.
- `oxbase-store`: round trips, unknown-field preservation, memory stores.

Live tests (`oxbase-conformance`, `#[ignore]`d, they need the fixtures) run on each of
postgres, mysql and mariadb:

1. Connect with `TlsMode::VerifyFull` + the fixture CA as `oxbase_tls`. Assert
   `tls_in_use()`. A wrong `server_name` fails. `Disable` as `oxbase_tls` fails (TLS-only user).
2. Connect through the bastion (password, key and encrypted-key auth) to `[ssh.targets]`.
   `dial_addr` is loopback and `via_ssh` is true. Repeat with `VerifyFull`, where the
   certificate must match the service name, not 127.0.0.1.
3. Cancel: run a long statement (`pg_sleep(30)` / `SLEEP(30)`), call `cancel_running` and the
   token in turn. The stream yields `Cancelled` within 5 s, both directly and through SSH, and
   the next `SELECT 1` on the same connection succeeds.
4. Host keys: `Strict` with the fixture `known_hosts` passes and with an empty file fails;
   `Fingerprint` passes with the right fingerprint and fails with a wrong one.
5. SQLite: open the fixture file and cancel a long recursive CTE.
6. Leak check: run every scenario with a marker password and assert the marker never appears in
   any error string or captured log.

## 11. M1 as built

Where the integrated M1 code differs from or extends the text above:

- **Tunnel sharing (section 4).** `TunnelKey` is still (SSH endpoint, SSH user, target), but the
  key alone no longer decides sharing. `TunnelPool` and `RusshTunnelProvider`'s session cache are
  also scoped by the host-key policy and the credential (HMAC-SHA256 tags under a random
  per-cache key; secrets are never stored). A `Strict` or pinned profile therefore never rides
  on a tunnel checked under `AcceptNew`, and a wrong password always authenticates for real.
  `oxbase-services` additionally pools per scope and gives `test_connection` (and connects with
  prompt-supplied SSH secrets) a dedicated tunnel.
- **SSH error categories (section 4).** Besides `Auth`, `Timeout`, `Cancelled` and
  `Connection`, the provider returns `InvalidInput` for local configuration mistakes
  (unparsable or unsupported key, legacy-encrypted PEM other than RSA/AES-128-CBC, bad target,
  no home directory), `Io` for an unreadable key file and `Internal` for a poisoned lock. "No
  agent" is `Connection`.
- **Fingerprint pins.** `SshOptions::validate` still requires the exact `SHA256:` prefix; the
  provider's own parser is more lenient but is reached only after validation.
- **Conformance (section 10).** The live checks run in `oxbase-conformance` as a second table
  (`connect`, `tls`, `ssh`, `tls_over_ssh`, `cancel`, `stream_200k`) next to the object-kind
  matrix, which stays pending until M2. The cancel limit is 2 s (stricter than the 5 s above).
- **Cancel (section 7).** PostgreSQL reports a server-side `statement_timeout` (SQLSTATE 57014)
  as an SQL error, not `Cancelled`; only cancels the driver itself requested are `Cancelled`.
- **Dependencies (section 8).** `ring` 0.17.14 (HMAC in `oxbase-net`), `dirs` 7.0.0
  (`oxbase-store`) and `tauri-plugin-dialog` 2.8.4 (`app-tauri`) were added to
  `[workspace.dependencies]`.
