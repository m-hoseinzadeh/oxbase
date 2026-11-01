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
//! - [`script`]: [`Script`] / [`ScriptStep`], ordered DDL steps with phases, transactionality
//!   and destructive flags (M3), and [`BoundStatement`], SQL with bound parameters.
//! - [`diff`]: [`PropertyDiff`], one difference found by a comparator, and [`Rename`] (M3).
//! - [`edit`]: [`RowEdit`] / [`RowKey`], grid edits for write-back.
//! - [`filter`]: [`Filter`], [`SortKey`] and [`TableDataQuery`], the grid's filter and sort
//!   model (M3).
//! - [`progress`]: [`Progress`], one progress event of a long-running operation.
//! - [`secret`]: [`Secret`], a string that never leaks into `Debug`, logs or documents.
//! - [`net`]: [`Endpoint`], [`TlsOptions`] / [`TlsMode`] and [`SshOptions`] (connection addressing,
//!   TLS and SSH settings shared by drivers, `oxbase-net`, `oxbase-store` and services).
//! - [`error`]: the single [`Error`] type and [`Result`] alias.
//! - [`catalog`] (M6): [`CatalogSnapshot`], every schema-level name of one database for code
//!   insight, and the identifier case rules [`IdentCase`] / [`CaseRules`].
//! - [`snippet`] (M6): editor [`Snippet`]s and their body syntax.

pub mod catalog;
pub mod data_type;
pub mod def;
pub mod diff;
pub mod edit;
pub mod engine;
pub mod error;
pub mod filter;
mod int_text;
pub mod net;
pub mod object;
pub mod progress;
pub mod props;
pub mod script;
pub mod secret;
pub mod snippet;
pub mod value;
pub mod version;

pub use catalog::{
    CaseRules, CatalogColumn, CatalogForeignKey, CatalogRelation, CatalogRoutine, CatalogSchema,
    CatalogSnapshot, IdentCase, RelationKind, RoutineKind,
};
pub use data_type::DataType;
pub use def::{
    ArgMode, CheckDef, ColumnDef, FkAction, ForeignKeyDef, GeneratedColumn, GenericDef,
    IdentityDef, IndexColumn, IndexDef, ObjectDef, PrimaryKeyDef, RoutineArg, RoutineDef,
    SchemaDef, SequenceDef, TableDef, TriggerDef, TriggerEvent, TriggerLevel, TriggerTiming,
    ViewDef,
};
pub use diff::{DiffKind, PropertyDiff, Rename};
pub use edit::{ColumnValue, KeyKind, RowEdit, RowKey};
pub use engine::Engine;
pub use error::{Error, ErrorCategory, Result};
pub use filter::{Filter, FilterOp, NullsOrder, SortKey, TableDataQuery};
pub use net::{
    Endpoint, KeySource, KnownHostsPolicy, PemSource, SshAuth, SshOptions, TlsMode, TlsOptions,
};
pub use object::{ObjectKind, ObjectRef, ObjectSummary};
pub use progress::{Progress, ProgressUnit};
pub use props::{Extras, PropId};
pub use script::{BoundStatement, Script, ScriptStep, StepCheck, StepCondition, StepPhase};
pub use secret::Secret;
pub use snippet::{Snippet, SnippetInput, validate_snippet_body};
pub use value::Value;
pub use version::ServerVersion;
