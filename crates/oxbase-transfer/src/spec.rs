//! What a transfer copies and how (`docs/m8-contracts.md` §4.2): the connection-free part of a
//! transfer request. `oxbase_services::transfer::TransferRequest` adds the two sessions.
//!
//! JSON: camelCase fields, `type`-tagged snake_case enums; core models inside (`ObjectRef`,
//! `DataType`, `Filter`) keep their own snake_case form.

use oxbase_types::{DataType, Error, Filter, ObjectKind, ObjectRef, Result};
use serde::{Deserialize, Serialize};

/// One thing to copy. Each becomes one target table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum SourceObject {
    /// A table, with an optional typed row filter (`oxbase_dialect::sql::select_table_data`,
    /// values bound). The target table gets the source name unless `target_name` is set.
    Table {
        table: ObjectRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target_name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filter: Option<Filter>,
    },
    /// A view (or materialized view), copied as a table of its rows (columns from the view's
    /// result; no keys, no indexes).
    View {
        view: ObjectRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target_name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filter: Option<Filter>,
    },
    /// One `SELECT` on the source connection (one statement; checked with the splitter),
    /// copied into a table named `target_name`. Columns come from the result's Arrow schema
    /// (`oxbase_io::logical_type` rules), so their logical types are what the driver reports.
    Query { sql: String, target_name: String },
}

impl SourceObject {
    /// The name of the table it becomes on the target.
    pub fn target_name(&self) -> &str {
        match self {
            Self::Table {
                table, target_name, ..
            } => target_name.as_deref().unwrap_or(&table.name),
            Self::View {
                view, target_name, ..
            } => target_name.as_deref().unwrap_or(&view.name),
            Self::Query { target_name, .. } => target_name,
        }
    }

    /// The source object, for tables and views.
    pub fn source_ref(&self) -> Option<&ObjectRef> {
        match self {
            Self::Table { table, .. } => Some(table),
            Self::View { view, .. } => Some(view),
            Self::Query { .. } => None,
        }
    }

    /// `InvalidInput` for an empty name or query, a `Table` source that is not a table ref, a
    /// `View` source that is not a view or materialized view.
    pub fn validate(&self) -> Result<()> {
        if self.target_name().trim().is_empty() {
            return Err(Error::invalid_input("a transfer target name is empty"));
        }
        match self {
            Self::Table { table, .. } if table.kind != ObjectKind::Table => Err(
                Error::invalid_input(format!("{} is not a table", table.display_name())),
            ),
            Self::View { view, .. }
                if !matches!(view.kind, ObjectKind::View | ObjectKind::MatView) =>
            {
                Err(Error::invalid_input(format!(
                    "{} is not a view",
                    view.display_name()
                )))
            }
            Self::Query { sql, .. } if sql.trim().is_empty() => {
                Err(Error::invalid_input("a transfer query is empty"))
            }
            _ => Ok(()),
        }
    }
}

/// What happens to each target table before rows go in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TableMode {
    /// Create the table (the plan refuses when it exists).
    #[default]
    Create,
    /// Drop the existing table (if any) and create it. Destructive.
    Replace,
    /// Insert into the existing table (the plan refuses when it does not exist; columns are
    /// matched by name, case-sensitively, after renames from overrides).
    Append,
    /// Empty the existing table, then insert. Destructive.
    Truncate,
}

/// When the target commits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransactionScope {
    /// One transaction per table: a failed table rolls back to its state before the load (DDL
    /// included where the engine has transactional DDL).
    #[default]
    PerTable,
    /// Commit after every batch (`TransferOptions::batch_rows`): bounded transactions; a failure
    /// keeps the committed batches.
    PerBatch,
}

/// What a failing table does to the rest.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorMode {
    /// The first failing table (or row issue) stops the transfer.
    #[default]
    Stop,
    /// A failing table is reported and the next one starts; rows with conversion issues are
    /// skipped and reported (server errors still fail their table).
    Continue,
}

/// Values of identity / auto-increment columns.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityMode {
    /// Copy the source values (identity insert where the engine needs it; the target's next
    /// generated value is moved past them, `BulkLoader::identity_resync`).
    #[default]
    Keep,
    /// Leave identity columns out of the load; the target generates new values (foreign keys
    /// that point at them then break: the plan warns).
    Generate,
}

/// Foreign-key enforcement while rows load.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FkChecks {
    /// Leave enforcement as it is: tables load in dependency order, and foreign keys of tables
    /// the transfer creates are added after every table is loaded.
    #[default]
    Keep,
    /// Switch enforcement off for the load session where the engine allows it
    /// (`BulkLoader::foreign_key_checks`; MySQL / MariaDB, SQLite, SQL Server) and back on
    /// after; elsewhere the plan warns and behaves like `Keep`.
    Disable,
}

/// How a transfer runs. Every field has a default (camelCase JSON, all optional).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TransferOptions {
    pub mode: TableMode,
    /// Rows per load batch (also the commit size of `TransactionScope::PerBatch`).
    pub batch_rows: usize,
    pub transaction: TransactionScope,
    pub errors: ErrorMode,
    pub identity: IdentityMode,
    pub fk_checks: FkChecks,
    /// Create (or replace) tables with their primary key, unique and plain indexes, checks
    /// (same engine only) and comments.
    pub create_indexes: bool,
    /// Add the source's foreign keys between transferred tables after every table is loaded.
    pub create_foreign_keys: bool,
    /// Rows with conversion issues kept in the report per table.
    pub max_reported_errors: usize,
}

impl Default for TransferOptions {
    fn default() -> Self {
        Self {
            mode: TableMode::Create,
            batch_rows: 10_000,
            transaction: TransactionScope::PerTable,
            errors: ErrorMode::Stop,
            identity: IdentityMode::Keep,
            fk_checks: FkChecks::Keep,
            create_indexes: true,
            create_foreign_keys: true,
            max_reported_errors: 1_000,
        }
    }
}

/// A user override of one column's mapping.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ColumnOverride {
    /// Index of the source object in [`TransferSpec::objects`].
    pub object: usize,
    /// The source column name.
    pub column: String,
    /// Leave the column out.
    #[serde(default)]
    pub skip: bool,
    /// The target column name (default: the source name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_name: Option<String>,
    /// The target logical type (default: the mapper's choice).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_type: Option<DataType>,
    /// The target native type text (default: the mapper's, or derived from `data_type`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_type: Option<String>,
}

/// A whole transfer without its sessions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferSpec {
    pub objects: Vec<SourceObject>,
    /// Where the tables go on the target connection: a `Schema` ref (PostgreSQL, SQL Server) or
    /// a `Database` ref (MySQL, MariaDB, SQLite).
    pub target: ObjectRef,
    #[serde(default)]
    pub options: TransferOptions,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overrides: Vec<ColumnOverride>,
}

impl TransferSpec {
    /// Shape checks that need no connection: at least one object, every object valid, target
    /// names unique (exactly; the plan also checks the target engine's case rules), a database
    /// or schema target, overrides pointing at existing objects, `batch_rows` at least 1.
    pub fn validate(&self) -> Result<()> {
        if self.objects.is_empty() {
            return Err(Error::invalid_input("nothing to transfer"));
        }
        if !matches!(self.target.kind, ObjectKind::Database | ObjectKind::Schema) {
            return Err(Error::invalid_input(
                "the transfer target must be a database or a schema",
            ));
        }
        if self.options.batch_rows == 0 {
            return Err(Error::invalid_input(
                "the batch size must be at least 1 row",
            ));
        }
        let mut names: Vec<&str> = Vec::with_capacity(self.objects.len());
        for object in &self.objects {
            object.validate()?;
            let name = object.target_name();
            if names.contains(&name) {
                return Err(Error::invalid_input(format!(
                    "two objects are copied into the same table '{name}'"
                )));
            }
            names.push(name);
        }
        if let Some(o) = self
            .overrides
            .iter()
            .find(|o| o.object >= self.objects.len())
        {
            return Err(Error::invalid_input(format!(
                "a column override points at object {} of {}",
                o.object,
                self.objects.len()
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn spec() -> TransferSpec {
        TransferSpec {
            objects: vec![
                SourceObject::Table {
                    table: ObjectRef::table(Some("oxbase_test"), Some("m8_xfer"), "m8_types"),
                    target_name: None,
                    filter: None,
                },
                SourceObject::Query {
                    sql: "SELECT 1 AS one".into(),
                    target_name: "m8_query".into(),
                },
            ],
            target: ObjectRef::database("m8_xfer_target"),
            options: TransferOptions::default(),
            overrides: vec![ColumnOverride {
                object: 0,
                column: "c_interval".into(),
                skip: false,
                target_name: None,
                data_type: Some(DataType::Text),
                native_type: None,
            }],
        }
    }

    #[test]
    fn specs_validate_their_shape() {
        let s = spec();
        s.validate().unwrap();
        assert_eq!(s.objects[0].target_name(), "m8_types");
        assert_eq!(s.objects[1].target_name(), "m8_query");
        assert!(s.objects[1].source_ref().is_none());

        let mut dup = spec();
        dup.objects.push(SourceObject::View {
            view: ObjectRef::new(ObjectKind::View, "v"),
            target_name: Some("m8_query".into()),
            filter: None,
        });
        assert!(dup.validate().is_err());

        let mut bad = spec();
        bad.target = ObjectRef::table(None, None, "t");
        assert!(bad.validate().is_err());
        let mut bad = spec();
        bad.overrides[0].object = 5;
        assert!(bad.validate().is_err());
        let mut bad = spec();
        bad.options.batch_rows = 0;
        assert!(bad.validate().is_err());
        let not_a_view = SourceObject::View {
            view: ObjectRef::table(None, None, "t"),
            target_name: None,
            filter: None,
        };
        assert!(not_a_view.validate().is_err());
    }

    #[test]
    fn json_is_camel_case_with_defaults() {
        let json = serde_json::to_value(spec()).unwrap();
        assert_eq!(json["objects"][0]["type"], "table");
        assert_eq!(json["objects"][1]["targetName"], "m8_query");
        assert_eq!(json["options"]["mode"], "create");
        assert_eq!(json["options"]["batchRows"], 10_000);
        assert_eq!(json["options"]["fkChecks"], "keep");
        assert_eq!(json["overrides"][0]["dataType"]["type"], "text");
        assert_eq!(
            serde_json::from_value::<TransferSpec>(json).unwrap(),
            spec()
        );

        let minimal: TransferSpec = serde_json::from_str(
            r#"{"objects":[{"type":"query","sql":"SELECT 1","targetName":"t"}],
                "target":{"kind":"schema","catalog":"db","name":"s"},
                "options":{"mode":"truncate"}}"#,
        )
        .unwrap();
        assert_eq!(minimal.options.mode, TableMode::Truncate);
        assert_eq!(minimal.options.batch_rows, 10_000);
        assert_eq!(minimal.overrides, Vec::<ColumnOverride>::new());
    }
}
