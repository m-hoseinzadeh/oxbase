//! Views (`information_schema.TABLES` + `VIEWS`).

use async_trait::async_trait;
use oxbase_dialect::Fetcher;
use oxbase_driver::{CancellationToken, Connection};
use oxbase_types::{
    Extras, ObjectDef, ObjectKind, ObjectRef, ObjectSummary, Result, ViewDef, props,
};

use super::rows::{query, text};
use super::{
    UPDATABLE, by_name, database_of, expect_kind, is_mariadb, is_system_database, not_found, scoped,
};

/// `TABLE_TYPE` of the server's own catalog views (`information_schema` tables).
const SYSTEM_VIEW: &str = "SYSTEM VIEW";

/// Lists the views of a database and loads one as a [`ViewDef`].
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ViewFetcher;

#[async_trait]
impl Fetcher for ViewFetcher {
    async fn list(
        &self,
        c: &dyn Connection,
        parent: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<Vec<ObjectSummary>> {
        let db = database_of(c, parent, &cancel).await?;
        let parent = scoped(parent, &db);
        let rows = query(
            c,
            "SELECT TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES \
             WHERE TABLE_SCHEMA = ? AND TABLE_TYPE IN ('VIEW', 'SYSTEM VIEW') ORDER BY TABLE_NAME",
            &[text(&db)],
            &cancel,
        )
        .await?;
        let system_db = is_system_database(&db);
        Ok(by_name(
            rows.iter()
                .map(|row| {
                    let mut s = ObjectSummary::new(parent.child(ObjectKind::View, row.string(0)));
                    s.system = system_db || row.string(1) == SYSTEM_VIEW;
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
        expect_kind(obj, ObjectKind::View)?;
        let db = database_of(c, obj, &cancel).await?;
        let params = [text(&db), text(&obj.name)];
        // MySQL keeps the algorithm out of information_schema (only SHOW CREATE VIEW has it).
        let algorithm = if is_mariadb(c) { "ALGORITHM" } else { "NULL" };
        let sql = format!(
            "SELECT VIEW_DEFINITION, CHECK_OPTION, IS_UPDATABLE, DEFINER, SECURITY_TYPE, \
             {algorithm} FROM information_schema.VIEWS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?"
        );
        let rows = query(c, &sql, &params, &cancel).await?;
        let Some(row) = rows.first() else {
            return system_view(c, obj, &db, &cancel).await;
        };
        let mut extras = Extras::new();
        if let Some(option) = row.non_empty(1).filter(|o| !o.eq_ignore_ascii_case("NONE")) {
            extras.insert(props::CHECK_OPTION, option.to_ascii_lowercase());
        }
        extras.insert(UPDATABLE, row.flag(2));
        if let Some(definer) = row.non_empty(3) {
            extras.insert(props::DEFINER, definer);
        }
        if let Some(security) = row.non_empty(4) {
            extras.insert(props::SECURITY, security);
        }
        if let Some(algorithm) = row.non_empty(5) {
            extras.insert(props::VIEW_ALGORITHM, algorithm);
        }
        Ok(ObjectDef::View(ViewDef {
            obj: scoped(obj, &db),
            definition: row.string(0),
            columns: Vec::new(),
            comment: None,
            extras,
        }))
    }
}

/// A server catalog view (not in `information_schema.VIEWS`): no definition, `TABLE_TYPE` set.
async fn system_view(
    c: &dyn Connection,
    obj: &ObjectRef,
    db: &str,
    cancel: &CancellationToken,
) -> Result<ObjectDef> {
    let rows = query(
        c,
        "SELECT TABLE_NAME FROM information_schema.TABLES \
         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND TABLE_TYPE = 'SYSTEM VIEW'",
        &[text(db), text(&obj.name)],
        cancel,
    )
    .await?;
    if rows.is_empty() {
        return Err(not_found(obj));
    }
    Ok(ObjectDef::View(ViewDef {
        obj: scoped(obj, db),
        definition: String::new(),
        columns: Vec::new(),
        comment: None,
        extras: Extras::new().with(props::TABLE_TYPE, "system view"),
    }))
}
