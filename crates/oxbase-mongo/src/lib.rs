//! MongoDB driver, dialect cells and mongo-shell subset (M7), built on the official `mongodb`
//! crate (3.9, tokio, BSON 3) with oxbase's own SOCKS5 dialer for SSH and TLS, and `rquickjs`
//! (QuickJS) for the shell.
//!
//! The contracts are in `docs/m7-contracts.md`; the module docs of [`driver`], [`dialect`],
//! [`shell`], [`request`], [`arrow`] and [`ejson`] repeat the parts each stream codes against.
//! This file is owned by the Design step: streams fill the bodies in their own modules and
//! never change the public items declared here.
//!
//! # Layout and owners
//!
//! | Module | What | Owner |
//! |---|---|---|
//! | [`driver`] (`src/driver/**`) | [`MongoDriver`], [`MongoConnection`]: the SOCKS5 dialer (SSH, TLS with `oxbase_net`), client options, auth (SCRAM, X.509, PLAIN), `execute` dispatch (request or shell), cancel (`killOp` by comment, `killCursors`), streaming | mongo-driver stream |
//! | `dialect::{fetch, documents, bulk}`, [`request`], [`arrow`], [`ejson`] | tree fetchers, the document store (find / aggregate sources, `document_get`, optimistic edits), import loader, BSON → Arrow, Extended JSON | mongo stream |
//! | [`shell`] | the shell's `ScriptLanguage` (split, guards, redaction) and the sandboxed QuickJS session | shell stream |
//! | [`dialect`] (`dialect/mod.rs`), [`options`], [`extras`], `ShellLimits` | registration, tree layout, capabilities, option keys and parsing, extra ids | Design (done) |
//!
//! # Registration
//!
//! - Driver: [`register`] adds [`MongoDriver`] for [`Engine::MongoDb`](oxbase_types::Engine)
//!   to a `DriverRegistry`; `oxbase_services::default_drivers` calls it once the mongo-driver
//!   stream uncomments its line.
//! - Dialect: [`dialect::register`] adds the fetchers, tree layout, capabilities, the shell
//!   language, the document store and the import loader; `oxbase_services::default_dialects`
//!   calls it at M7 integration.
//!
//! # Server versions
//!
//! MongoDB 5.0 and later (the driver's minimum wire version is 4.0; `$currentOp` with
//! `ownOps`, `$$ROOT` equality and `listCollections` with views need nothing newer). CI runs
//! 7.0.45 (8.x refuses the CI kernel, SERVER-121912), standalone.

pub mod arrow;
pub mod dialect;
pub mod driver;
pub mod ejson;
pub mod options;
pub mod request;
pub mod shell;

pub use driver::{MongoConnection, MongoDriver, register};

/// MongoDB extras: the ids of the engine-specific properties the fetchers write into `extras`
/// and the UI shows. Generic properties use `oxbase_types::props` (`DOCUMENT_COUNT`,
/// `SIZE_BYTES`, `NATIVE_TYPE`, `PRIMARY_KEY`, `UNIQUE`). JSON values are `Value::Json` with
/// relaxed Extended JSON.
pub mod extras {
    use oxbase_types::PropId;

    /// Collection (bool): capped.
    pub const CAPPED: PropId = PropId::from_static("capped");
    /// Collection (json): the `validator` document.
    pub const VALIDATOR: PropId = PropId::from_static("validator");
    /// Collection / view (json): the whole `options` of `listCollections`.
    pub const OPTIONS: PropId = PropId::from_static("options");
    /// Collection (text): `collection` or `timeseries`.
    pub const COLLECTION_TYPE: PropId = PropId::from_static("collection_type");
    /// View (text): the collection or view it reads (`viewOn`).
    pub const VIEW_ON: PropId = PropId::from_static("view_on");
    /// Index (json): the key pattern (`{"name": 1, "tags": -1}`, `"text"`, `"2dsphere"`, ...).
    pub const INDEX_KEYS: PropId = PropId::from_static("index_keys");
    /// Index (bool): `sparse`.
    pub const SPARSE: PropId = PropId::from_static("sparse");
    /// Index (int): TTL `expireAfterSeconds`.
    pub const EXPIRE_AFTER_SECONDS: PropId = PropId::from_static("expire_after_seconds");
    /// Index (bool): hidden from the planner.
    pub const HIDDEN: PropId = PropId::from_static("hidden");
    /// User / role (json): granted roles (`[{"role": "read", "db": "oxbase_test"}]`).
    pub const ROLES: PropId = PropId::from_static("roles");
    /// Role (json): privileges (`[{"resource": {...}, "actions": [...]}]`).
    pub const PRIVILEGES: PropId = PropId::from_static("privileges");
    /// User (json): `mechanisms` the user can authenticate with (never credentials).
    pub const MECHANISMS: PropId = PropId::from_static("mechanisms");
}
