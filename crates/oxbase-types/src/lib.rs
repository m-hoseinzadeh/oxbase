//! Core shared types: `Value`, `DataType`, `ObjectKind`, `ObjectRef`, `ServerVersion` and the common error type.
//!
//! Every other crate depends on this one; it has no engine-specific code.
//!
//! Module map:
//!
//! - [`engine`]: the [`Engine`] enum (which database product a connection talks to).
//! - [`version`]: [`ServerVersion`], parsed from banner strings and compared with `Ord`.
//! - [`value`] / [`data_type`]: the typed scalar [`Value`] and the logical [`DataType`].
//! - [`object`]: [`ObjectKind`], the engine-agnostic [`ObjectRef`] path and [`ObjectSummary`].
//! - [`props`]: [`PropId`] and the typed [`Extras`] map for database-specific properties.
//! - [`def`]: the typed object model [`ObjectDef`] (databases and schemas, tables, columns,
//!   indexes, views, routines, triggers, sequences, ...).
//! - [`script`]: [`Script`], an ordered list of statements plus warnings.
//! - [`diff`]: [`PropertyDiff`], one difference found by a comparator.
//! - [`edit`]: [`RowEdit`] / [`RowKey`], grid edits for write-back.
//! - [`progress`]: [`Progress`], one progress event of a long-running operation.
//! - [`secret`]: [`Secret`], a string that never leaks into `Debug`, logs or documents.
//! - [`net`]: [`Endpoint`], [`TlsOptions`] / [`TlsMode`] and [`SshOptions`] (connection addressing,
//!   TLS and SSH settings shared by drivers, `oxbase-net`, `oxbase-store` and services).
//! - [`error`]: the single [`Error`] type and [`Result`] alias.

pub mod data_type;
pub mod def;
pub mod diff;
pub mod edit;
pub mod engine;
pub mod error;
mod int_text;
pub mod net;
pub mod object;
pub mod progress;
pub mod props;
pub mod script;
pub mod secret;
pub mod value;
pub mod version;

pub use data_type::DataType;
pub use def::{
    ArgMode, CheckDef, ColumnDef, FkAction, ForeignKeyDef, GeneratedColumn, GenericDef,
    IdentityDef, IndexColumn, IndexDef, ObjectDef, PrimaryKeyDef, RoutineArg, RoutineDef,
    SchemaDef, SequenceDef, TableDef, TriggerDef, TriggerEvent, TriggerLevel, TriggerTiming,
    ViewDef,
};
pub use diff::{DiffKind, PropertyDiff};
pub use edit::{ColumnValue, KeyKind, RowEdit, RowKey};
pub use engine::Engine;
pub use error::{Error, ErrorCategory, Result};
pub use net::{
    Endpoint, KeySource, KnownHostsPolicy, PemSource, SshAuth, SshOptions, TlsMode, TlsOptions,
};
pub use object::{ObjectKind, ObjectRef, ObjectSummary};
pub use progress::{Progress, ProgressUnit};
pub use props::{Extras, PropId};
pub use script::{Script, Statement};
pub use secret::Secret;
pub use value::Value;
pub use version::ServerVersion;
