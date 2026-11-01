//! Microsoft SQL Server driver and dialect cells (M4), built on `tiberius` (TDS 7.4) with the
//! TLS handshake run by oxbase itself (rustls + ring).
//!
//! The contracts are in `docs/m4-contracts.md`; the module docs of [`driver`] and [`dialect`]
//! repeat the parts each stream codes against. This file is owned by the Design step: streams
//! fill the bodies in their own modules and never change the public items declared here.
//!
//! # Layout and owners
//!
//! | Module | What | Owner |
//! |---|---|---|
//! | [`driver`] (`src/driver/**`) | [`MsSqlDriver`], [`MsSqlConnection`], TLS, cancel, Arrow mapping ([`driver::types`]) | driver stream |
//! | `dialect::fetch*` | catalog [`Fetcher`](oxbase_dialect::Fetcher) cells | fetch stream |
//! | `dialect::{ddl*, compare*, caps*, dml*}` | DDL generator, comparator rules, designer capabilities, grid writer | ddl stream |
//! | [`dialect`] (`dialect/mod.rs`), [`options`], [`extras`] | registration, tree layout, option keys, extra ids | Design |
//!
//! # Registration
//!
//! - Driver: [`register`] adds [`MsSqlDriver`] for [`Engine::MsSql`](oxbase_types::Engine::MsSql)
//!   to a `DriverRegistry`; `oxbase_services::default_drivers` calls it.
//! - Dialect: [`dialect::register`] adds the fetchers, tree layout, capabilities, quoting
//!   ([`oxbase_dialect::BracketQuoting`]), DDL generators, comparators and the grid writer;
//!   `oxbase_services::default_dialects` calls it.
//!
//! # Server versions
//!
//! SQL Server 2017 (14.x) and later, on Linux, Windows and Azure SQL Database / Managed Instance
//! with SQL authentication. CI runs SQL Server 2025 (17.x). Version gates the cells must honour:
//! 2019 (15.x) UTF-8 collations; 2022 (16.x) TDS 8.0 strict encryption, `GENERATE_SERIES`,
//! `IS [NOT] DISTINCT FROM`; 2025 (17.x) the native `json` and `vector` types.

pub mod dialect;
pub mod driver;

pub use driver::{MsSqlConnection, MsSqlDriver, register};

/// Keys of [`ConnectParams::options`](oxbase_driver::ConnectParams::options) the SQL Server
/// driver reads (profile `options` in the connection dialog). Unknown keys are refused with
/// `InvalidInput` naming the key, so a typo never silently weakens a connection.
pub mod options {
    /// Named instance (`SQLEXPRESS`). M4 does not query SQL Browser (UDP 1434, which cannot go
    /// through an SSH tunnel): a named instance needs an explicit port. The name is sent in the
    /// PRELOGIN `INSTOPT` field and a server that reports another instance is refused.
    pub const INSTANCE: &str = "instance";
    /// `strict`: TDS 8.0 strict encryption (TLS before PRELOGIN, ALPN `tds/8.0`, SQL Server
    /// 2022+ and Azure SQL); needs `TlsMode::Require`, `VerifyCa` or `VerifyFull`. Absent or
    /// `negotiate` (default): TDS 7.4, TLS negotiated in PRELOGIN (every supported version).
    pub const ENCRYPT: &str = "encrypt";
    /// Value of [`ENCRYPT`] for TDS 8.0 strict encryption.
    pub const ENCRYPT_STRICT: &str = "strict";
    /// Value of [`ENCRYPT`] for TDS 7.4 negotiated encryption (the default).
    pub const ENCRYPT_NEGOTIATE: &str = "negotiate";
    /// `read_only` or `read_write` (default): LOGIN7 `ApplicationIntent` (Always On readable
    /// secondaries).
    pub const APPLICATION_INTENT: &str = "application_intent";
    /// Rows per Arrow batch (default [`crate::driver::types::BATCH_ROWS`]).
    pub const BATCH_ROWS: &str = "batch_rows";
}

/// SQL Server extras: the ids of the engine-specific properties the fetch stream writes into
/// `extras`, the ddl stream reads back and the designer shows. Generic properties use
/// `oxbase_types::props` (`CLUSTERED`, `INCLUDE_COLUMNS`, `FILL_FACTOR`, `COLLATION`,
/// `CHECK_OPTION`, `TABLE_TYPE`, ...); identity, computed columns, filtered indexes and comments
/// use the typed model (`ColumnDef::identity` with `always = true`, `ColumnDef::generated`,
/// `IndexDef::predicate`, `comment` = the `MS_Description` extended property). Values are
/// `Value::Text` unless noted. See `docs/m4-contracts.md` §4.
pub mod extras {
    use oxbase_types::PropId;

    /// Column: name of its `DEFAULT` constraint (`DF_orders_status`). Absent on a design: the
    /// generator names it `DF_<table>_<column>`. Compared only when both sides have it.
    pub const DEFAULT_CONSTRAINT: PropId = PropId::from_static("default_constraint");
    /// Column (bool): `rowversion` / `timestamp`, maintained by the server on every write. Never
    /// written by the grid writer; the native type is `rowversion`.
    pub const ROW_VERSION: PropId = PropId::from_static("row_version");
    /// Column (bool): `SPARSE`.
    pub const SPARSE: PropId = PropId::from_static("sparse");
    /// Column: `row_start` or `row_end` for the period columns of a system-versioned table
    /// (`GENERATED ALWAYS AS ROW START|END`). Such columns are read-only for the grid.
    pub const PERIOD: PropId = PropId::from_static("period");
    /// Column (bool): `HIDDEN` period column.
    pub const HIDDEN: PropId = PropId::from_static("hidden");
    /// Column, identity, foreign key or check (bool): `NOT FOR REPLICATION`.
    pub const NOT_FOR_REPLICATION: PropId = PropId::from_static("not_for_replication");

    /// Table: `system_versioned` (temporal table) or `history` (its history table). Absent for
    /// ordinary tables.
    pub const TEMPORAL_TYPE: PropId = PropId::from_static("temporal_type");
    /// Table: the history table of a system-versioned table, `schema.name` (unquoted).
    pub const HISTORY_TABLE: PropId = PropId::from_static("history_table");
    /// Table, index (text): `NONE`, `ROW`, `PAGE`, `COLUMNSTORE`, ... (`DATA_COMPRESSION`).
    pub const DATA_COMPRESSION: PropId = PropId::from_static("data_compression");
    /// Table (bool): memory-optimized (In-Memory OLTP). The designer treats it as read-only.
    pub const MEMORY_OPTIMIZED: PropId = PropId::from_static("memory_optimized");

    /// Index: `clustered`, `nonclustered`, `clustered_columnstore`, `nonclustered_columnstore`,
    /// `xml`, `spatial`, `hash` (memory-optimized). `props::CLUSTERED` (bool) is set too. The
    /// designer edits only `clustered` / `nonclustered`; the generator refuses to change the
    /// others with `Unsupported` (it may drop them).
    pub const INDEX_KIND: PropId = PropId::from_static("index_kind");
    /// Index (bool): disabled (`ALTER INDEX ... DISABLE`).
    pub const DISABLED: PropId = PropId::from_static("disabled");
    /// Index (bool): `IGNORE_DUP_KEY = ON`.
    pub const IGNORE_DUP_KEY: PropId = PropId::from_static("ignore_dup_key");

    /// Foreign key or check (bool): created or re-enabled `WITH NOCHECK` (`is_not_trusted`).
    pub const NOT_TRUSTED: PropId = PropId::from_static("not_trusted");

    /// Function: `scalar`, `inline_table`, `table` (multi-statement), `clr_scalar`,
    /// `clr_table`, `aggregate`.
    pub const FUNCTION_KIND: PropId = PropId::from_static("function_kind");
    /// View, function, procedure, trigger (bool): `WITH SCHEMABINDING`.
    pub const SCHEMA_BINDING: PropId = PropId::from_static("schema_binding");
    /// View, function, procedure, trigger (bool): `WITH ENCRYPTION`; the definition is NULL.
    pub const ENCRYPTED: PropId = PropId::from_static("encrypted");
    /// Function, procedure, trigger: `EXECUTE AS` principal (`CALLER`, `OWNER`, `SELF` or a
    /// user name).
    pub const EXECUTE_AS: PropId = PropId::from_static("execute_as");
    /// Routine argument list facts that do not fit `RoutineArg`: JSON array of the names of
    /// `READONLY` table-valued parameters.
    pub const READONLY_ARGS: PropId = PropId::from_static("readonly_args");

    /// Type: `alias`, `table` or `clr`.
    pub const TYPE_KIND: PropId = PropId::from_static("type_kind");
    /// Alias type: the base type as written in DDL (`varchar(20)`, `decimal(12, 2)`).
    pub const BASE_TYPE: PropId = PropId::from_static("base_type");
    /// Alias type (bool): `NOT NULL`.
    pub const NOT_NULL: PropId = PropId::from_static("not_null");

    /// Database: compatibility level (`Value::Int`, 140 .. 170).
    pub const COMPATIBILITY_LEVEL: PropId = PropId::from_static("compatibility_level");
    /// Database: `SIMPLE`, `FULL` or `BULK_LOGGED`.
    pub const RECOVERY_MODEL: PropId = PropId::from_static("recovery_model");
    /// Database: `ONLINE`, `OFFLINE`, `RESTORING`, ... (`state_desc`). Databases that are not
    /// `ONLINE` are still listed, with this extra; opening a session on one fails with the
    /// server's error, which the tree shows on that node.
    pub const STATE: PropId = PropId::from_static("state");
}
