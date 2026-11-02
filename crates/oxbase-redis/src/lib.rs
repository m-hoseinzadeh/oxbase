//! Redis driver, dialect cells and console (M7), built on the `redis` crate (1.8, tokio,
//! RESP2 / RESP3, cluster, sentinel, rustls with the ring provider).
//!
//! The contracts are in `docs/m7-contracts.md`; the module docs of [`driver`], [`dialect`],
//! [`console`] and [`command`] repeat the parts each stream codes against. This file is owned by
//! the Design step: streams fill the bodies in their own modules and never change the public
//! items declared here.
//!
//! # Layout and owners
//!
//! | Module | What | Owner |
//! |---|---|---|
//! | [`driver`] (`src/driver/**`) | [`RedisDriver`], [`RedisConnection`]: connect (standalone, cluster, sentinel), TLS, SSH through the DNS-resolver hook, auth, RESP3, `execute` of console commands and reply → Arrow rendering, cancel | redis-driver stream |
//! | `dialect::{fetch, keys, pubsub}` | tree fetchers, the key-value cell (scans, typed values, atomic edits), pub/sub | redis stream |
//! | [`console`] | the console's `ScriptLanguage`: split, guards, redaction | shell stream |
//! | [`dialect`] (`dialect/mod.rs`), [`options`], [`command`], [`extras`] | registration, tree layout, capabilities, option keys and parsing, the command-line reader, extra ids | Design (done) |
//!
//! # Registration
//!
//! - Driver: [`register`] adds [`RedisDriver`] for [`Engine::Redis`](oxbase_types::Engine) to a
//!   `DriverRegistry`; `oxbase_services::default_drivers` calls it once the redis-driver stream
//!   uncomments its line.
//! - Dialect: [`dialect::register`] adds the fetchers, tree layout, capabilities, the console
//!   language, the key-value store and pub/sub; `oxbase_services::default_dialects` calls it at
//!   M7 integration.
//!
//! # Server versions
//!
//! Redis 6.0 and later (ACL users, RESP3, `CLIENT UNBLOCK`, `SCAN ... TYPE`), and Valkey 7.2+.
//! RESP2-only servers (5.x) connect with fewer features: no ACL users, no pub/sub on the
//! multiplexed connection (a dedicated RESP2 pub/sub connection instead), `SCAN` without
//! `TYPE` (filtered client-side). CI runs Redis 8.10 (standalone, a 3-master cluster, and a
//! sentinel with one master and one replica).

pub mod command;
pub mod console;
pub mod dialect;
pub mod driver;
pub mod options;

pub use driver::{RedisConnection, RedisDriver, register};

/// Redis extras: the ids of the engine-specific properties the fetchers write into `extras`
/// (key and user definitions) and the UI shows. Generic M7 properties use `oxbase_types::props`
/// (`KEY_TYPE`, `TTL_MS`, `KEY_COUNT`, `TRUNCATED`, `KEY_HEX`).
pub mod extras {
    use oxbase_types::PropId;

    /// Key (int): string length, or members / fields / entries of a collection type.
    pub const LENGTH: PropId = PropId::from_static("length");
    /// Key (text): `OBJECT ENCODING` (`listpack`, `quicklist`, `hashtable`, `skiplist`, ...).
    pub const ENCODING: PropId = PropId::from_static("encoding");
    /// Key (int): `MEMORY USAGE` in bytes.
    pub const MEMORY_BYTES: PropId = PropId::from_static("memory_bytes");
    /// User (text): the flags of `ACL GETUSER` (`on`, `nopass`, ...), space-separated.
    pub const ACL_FLAGS: PropId = PropId::from_static("acl_flags");
    /// User (text): the command rules (`+@all -flushall`).
    pub const ACL_COMMANDS: PropId = PropId::from_static("acl_commands");
    /// User (text): the key patterns (`~m7:*`).
    pub const ACL_KEYS: PropId = PropId::from_static("acl_keys");
    /// User (text): the channel patterns (`&*`).
    pub const ACL_CHANNELS: PropId = PropId::from_static("acl_channels");
    /// Database (text): `standalone`, `cluster` or `sentinel`, on the database listing.
    pub const MODE: PropId = PropId::from_static("mode");
}
