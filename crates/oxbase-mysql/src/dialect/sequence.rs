//! MariaDB sequences (10.3+). Listed from `information_schema.TABLES` (`TABLE_TYPE =
//! 'SEQUENCE'`); their options come from `information_schema.SEQUENCES`, which MariaDB has from
//! 11.5. Older servers keep the options only in the sequence table itself, which cannot be read
//! without putting its name into the SQL, so `load` reports `Unsupported` there.

use async_trait::async_trait;
use oxbase_dialect::Fetcher;
use oxbase_driver::{CancellationToken, Connection};
use oxbase_types::{Error, ObjectDef, ObjectKind, ObjectRef, ObjectSummary, Result, SequenceDef};

use super::rows::{query, text};
use super::{by_name, database_of, expect_kind, is_mariadb, is_system_database, not_found, scoped};

/// Lists the sequences of a MariaDB database and loads one as a [`SequenceDef`].
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SequenceFetcher;

fn check_server(c: &dyn Connection) -> Result<()> {
    if is_mariadb(c) && c.server_version().at_least(10, 3) {
        Ok(())
    } else {
        Err(Error::unsupported(format!(
            "{} {} has no sequences",
            c.engine().display_name(),
            c.server_version()
        )))
    }
}

#[async_trait]
impl Fetcher for SequenceFetcher {
    async fn list(
        &self,
        c: &dyn Connection,
        parent: &ObjectRef,
        cancel: CancellationToken,
    ) -> Result<Vec<ObjectSummary>> {
        check_server(c)?;
        let db = database_of(c, parent, &cancel).await?;
        let parent = scoped(parent, &db);
        let rows = query(
            c,
            "SELECT TABLE_NAME, TABLE_COMMENT FROM information_schema.TABLES \
             WHERE TABLE_SCHEMA = ? AND TABLE_TYPE = 'SEQUENCE' ORDER BY TABLE_NAME",
            &[text(&db)],
            &cancel,
        )
        .await?;
        let system = is_system_database(&db);
        Ok(by_name(
            rows.iter()
                .map(|row| {
                    let mut s =
                        ObjectSummary::new(parent.child(ObjectKind::Sequence, row.string(0)));
                    s.system = system;
                    s.comment = row.non_empty(1);
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
        expect_kind(obj, ObjectKind::Sequence)?;
        check_server(c)?;
        if !c.server_version().at_least(11, 5) {
            return Err(Error::unsupported(format!(
                "reading sequence options needs MariaDB 11.5 or later (information_schema.SEQUENCES); \
                 the server is {}",
                c.server_version()
            )));
        }
        let db = database_of(c, obj, &cancel).await?;
        let rows = query(
            c,
            "SELECT s.DATA_TYPE, s.START_VALUE, s.MINIMUM_VALUE, s.MAXIMUM_VALUE, s.INCREMENT, \
             s.CYCLE_OPTION, t.TABLE_COMMENT FROM information_schema.SEQUENCES s \
             LEFT JOIN information_schema.TABLES t \
               ON t.TABLE_SCHEMA = s.SEQUENCE_SCHEMA AND t.TABLE_NAME = s.SEQUENCE_NAME \
             WHERE s.SEQUENCE_SCHEMA = ? AND s.SEQUENCE_NAME = ?",
            &[text(&db), text(&obj.name)],
            &cancel,
        )
        .await?;
        let row = rows.first().ok_or_else(|| not_found(obj))?;
        let mut def = SequenceDef::new(scoped(obj, &db));
        def.data_type = row.non_empty(0);
        def.start = row.int(1).unwrap_or(1);
        def.increment = row.int(4).unwrap_or(1);
        def.min_value = row.int(2);
        def.max_value = row.int(3);
        def.cycle = row.flag(5);
        def.comment = row.non_empty(6);
        Ok(ObjectDef::Sequence(def))
    }
}
