//! Network layer: SSH tunnels (`russh`), TLS configuration (`rustls`) and a shared tunnel pool.
//!
//! - [`tls`]: [`tls::client_config`] builds the rustls client configuration every driver uses,
//!   with the ring crypto provider; plus PEM/key loading helpers for client libraries that build
//!   their own config.
//! - [`ssh`]: the [`ssh::TunnelProvider`] contract, [`ssh::TunnelHandle`],
//!   [`ssh::TunnelPool`] and the production provider [`ssh::RusshTunnelProvider`] (russh: password,
//!   key file / inline key, encrypted keys, agent; `Strict` / `AcceptNew` / fingerprint host-key
//!   policies).
//!
//! The option types ([`TlsOptions`], [`SshOptions`], ...) are defined in `oxbase-types` so
//! drivers and storage can use them without depending on rustls or russh; they are re-exported
//! here. See `docs/m1-contracts.md` for the addressing and server-name rules.

pub mod ssh;
pub mod tls;

pub use oxbase_types::{
    Endpoint, KeySource, KnownHostsPolicy, PemSource, Secret, SshAuth, SshOptions, TlsMode,
    TlsOptions,
};
pub use tokio_util::sync::CancellationToken;
