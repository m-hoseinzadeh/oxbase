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
//! - [`def`]: the typed object model [`ObjectDef`] (tables, columns, indexes, ...).
//! - [`script`]: [`Script`], an ordered list of statements plus warnings.
//! - [`diff`]: [`PropertyDiff`], one difference found by a comparator.
//! - [`edit`]: [`RowEdit`] / [`RowKey`], grid edits for write-back.
//! - [`progress`]: [`Progress`], one progress event of a long-running operation.
//! - [`error`]: the single [`Error`] type and [`Result`] alias.

pub mod data_type;
pub mod def;
pub mod diff;
pub mod edit;
pub mod engine;
pub mod error;
pub mod object;
pub mod progress;
pub mod props;
pub mod script;
pub mod value;
pub mod version;

pub use data_type::DataType;
pub use def::{
    CheckDef, ColumnDef, FkAction, ForeignKeyDef, GeneratedColumn, GenericDef, IndexColumn,
    IndexDef, ObjectDef, PrimaryKeyDef, TableDef, ViewDef,
};
pub use diff::{DiffKind, PropertyDiff};
pub use edit::{ColumnValue, KeyKind, RowEdit, RowKey};
pub use engine::Engine;
pub use error::{Error, ErrorCategory, Result};
pub use object::{ObjectKind, ObjectRef, ObjectSummary};
pub use progress::{Progress, ProgressUnit};
pub use props::{Extras, PropId};
pub use script::{Script, Statement};
pub use value::Value;
pub use version::ServerVersion;
