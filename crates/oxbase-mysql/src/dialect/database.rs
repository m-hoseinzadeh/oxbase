//! Databases (`information_schema.SCHEMATA`).

use async_trait::async_trait;
use oxbase_dialect::Fetcher;
use oxbase_driver::{CancellationToken, Connection};
use oxbase_types::{
    Error, ObjectDef, ObjectKind, ObjectRef, ObjectSummary, Result, SchemaDef, props,
};

use super::rows::{query, text};
use super::{by_name, expect_kind, is_mariadb, is_system_database, not_found};

/// Lists the databases under the connection root and loads one as a [`SchemaDef`].
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct DatabaseFetcher;

/// `SCHEMA_COMMENT` exists on MariaDB 10.5+ only.
fn comment_column(c: &dyn Connection) -> &'static str {
    if is_mariadb(c) && c.server_version().at_least(10, 5) {
        "SCHEMA_COMMENT"
    } else {
        "NULL"
    }
}

#[async_trait]
impl Fetcher for DatabaseFetcher {
    async fn list(
        &self,
        c: &dyn Connection,
        parent: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<Vec<ObjectSummary>> {
        if !parent.is_root() {
            return Err(Error::invalid_input(format!(
                "databases are listed under the connection root, not under {parent}"
            )));
        }
        let sql = format!(
            "SELECT SCHEMA_NAME, DEFAULT_CHARACTER_SET_NAME, DEFAULT_COLLATION_NAME, DATABASE(), {} \
             FROM information_schema.SCHEMATA ORDER BY SCHEMA_NAME",
            comment_column(c)
        );
        let rows = query(c, &sql, &[], &cancel).await?;
        Ok(by_name(
            rows.iter()
                .map(|row| {
                    let name = row.string(0);
                    let mut s =
                        ObjectSummary::new(parent.child(ObjectKind::Database, name.as_str()));
                    s.system = is_system_database(&name);
                    s.comment = row.non_empty(4);
                    if let Some(cs) = row.non_empty(1) {
                        s.extras.insert(props::CHARSET, cs);
                    }
                    if let Some(co) = row.non_empty(2) {
                        s.extras.insert(props::COLLATION, co);
                    }
                    if row.text(3).as_deref() == Some(name.as_str()) {
                        s.extras.insert(props::CURRENT, true);
                    }
                    s
                })
                .collect(),
        ))
    }

    async fn load(
        &self,
        c: &dyn Connection,
        obj: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<ObjectDef> {
        expect_kind(obj, ObjectKind::Database)?;
        let Some(name) = obj.database_name().filter(|n| !n.is_empty()) else {
            return Err(Error::invalid_input("a database ref needs a name"));
        };
        let sql = format!(
            "SELECT SCHEMA_NAME, DEFAULT_CHARACTER_SET_NAME, DEFAULT_COLLATION_NAME, {} \
             FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = ?",
            comment_column(c)
        );
        let rows = query(c, &sql, &[text(name)], &cancel).await?;
        let row = rows.first().ok_or_else(|| not_found(obj))?;
        let mut def = SchemaDef::new(ObjectRef::database(row.string(0)));
        def.comment = row.non_empty(3);
        if let Some(cs) = row.non_empty(1) {
            def.extras.insert(props::CHARSET, cs);
        }
        if let Some(co) = row.non_empty(2) {
            def.extras.insert(props::COLLATION, co);
        }
        Ok(ObjectDef::Schema(def))
    }
}
