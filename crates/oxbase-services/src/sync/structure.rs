//! The bodies of [`StructureSyncService`] (`docs/m8-contracts.md` §5.5): compares on dedicated
//! read sessions, kept results, scripts, and the apply with its stale check and re-compare.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use oxbase_dialect::{DatabaseScope, DialectRegistry};
use oxbase_diff::{SchemaSnapshot, StructureOptions, item_id, load_snapshot, sync_script};
use oxbase_types::{Error, ObjectDef, ObjectKind, ObjectRef, Result};

use super::{
    DiffStatus, KEPT_COMPARES, StructureApplyReport, StructureApplyRequest,
    StructureCompareRequest, StructureCompareResult, StructureDiff, StructureScriptRequest,
    StructureSyncService, SyncScript,
};
use crate::designer::execute_script;
use crate::runs::OperationId;
use crate::schema_cache::{ChangeSource, SchemaChangeEvent};
use crate::{CancellationToken, SessionHandle, SessionId, check_cancelled, until_cancelled};

/// What a destructive apply without confirmation is refused with.
const CONFIRM_DESTRUCTIVE: &str = "the script drops or rebuilds something and can lose data; \
                                   confirm it to apply";

/// A compare kept for scripts and applies.
pub(super) struct KeptCompare {
    request: StructureCompareRequest,
    source: SchemaSnapshot,
    target: SchemaSnapshot,
    diff: StructureDiff,
}

/// The kept compares of one service, oldest first (at most [`KEPT_COMPARES`]).
#[derive(Default)]
pub(super) struct Kept {
    inner: Mutex<VecDeque<(OperationId, Arc<KeptCompare>)>>,
}

impl Kept {
    fn lock(&self) -> MutexGuard<'_, VecDeque<(OperationId, Arc<KeptCompare>)>> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn insert(&self, id: OperationId, kept: KeptCompare) {
        let mut kept_list = self.lock();
        kept_list.push_back((id, Arc::new(kept)));
        while kept_list.len() > KEPT_COMPARES {
            kept_list.pop_front();
        }
    }

    fn get(&self, id: OperationId) -> Result<Arc<KeptCompare>> {
        self.lock()
            .iter()
            .find(|(kept_id, _)| *kept_id == id)
            .map(|(_, kept)| Arc::clone(kept))
            .ok_or_else(|| {
                Error::not_found(format!("the compare {id} is no longer kept; compare again"))
            })
    }

    /// Drops a kept compare; `false` when it is unknown.
    pub(super) fn remove(&self, id: OperationId) -> bool {
        let mut kept_list = self.lock();
        let before = kept_list.len();
        kept_list.retain(|(kept_id, _)| *kept_id != id);
        kept_list.len() != before
    }
}

fn base(service: &StructureSyncService, id: SessionId) -> Result<SessionHandle> {
    service
        .manager
        .get(id)
        .ok_or_else(|| Error::not_found(format!("no open session {id}")))
}

/// The session that reads or writes `container` for `base`: a new child on the container's
/// database (closed by the caller when the flag says so), or the base itself for a private
/// in-memory database, whose children would not see it.
async fn container_session(
    service: &StructureSyncService,
    base: &SessionHandle,
    container: &ObjectRef,
    cancel: &CancellationToken,
) -> Result<(SessionHandle, bool)> {
    if !service.manager.children_share_data(base.id()) {
        return Ok((base.clone(), false));
    }
    let per_database = service
        .dialects
        .tree_layout(base.summary().engine)
        .is_ok_and(|layout| layout.scope == DatabaseScope::Session);
    let database = if per_database {
        container.database_name()
    } else {
        None
    };
    let child = service
        .manager
        .open_child(base.id(), database, cancel.clone())
        .await?;
    Ok((child, true))
}

async fn close_quietly(service: &StructureSyncService, session: &SessionHandle) {
    if let Err(e) = service.manager.close(session.id()).await {
        tracing::debug!(error = %e, "closing a structure sync session failed");
    }
}

/// Reads `container` through `base` (on a dedicated session).
async fn snapshot(
    service: &StructureSyncService,
    base: &SessionHandle,
    container: &ObjectRef,
    options: &StructureOptions,
    cancel: &CancellationToken,
) -> Result<SchemaSnapshot> {
    let (reader, dedicated) = container_session(service, base, container, cancel).await?;
    let result = async {
        let _guard = until_cancelled(cancel, async { Ok(reader.exclusive().await) }).await?;
        load_snapshot(
            &service.dialects,
            reader.connection().as_ref(),
            container,
            options,
            &|_: u64, _: u64| {},
            cancel.clone(),
        )
        .await
    }
    .await;
    if dedicated {
        close_quietly(service, &reader).await;
    }
    result
}

fn check_container(container: &ObjectRef) -> Result<()> {
    if matches!(container.kind, ObjectKind::Database | ObjectKind::Schema) {
        Ok(())
    } else {
        Err(Error::invalid_input(format!(
            "{} is not a database or a schema",
            container.display_name()
        )))
    }
}

pub(super) async fn compare(
    service: &StructureSyncService,
    request: &StructureCompareRequest,
    cancel: CancellationToken,
) -> Result<StructureCompareResult> {
    let started = Instant::now();
    check_cancelled(&cancel)?;
    check_container(&request.source)?;
    check_container(&request.target)?;
    let source_base = base(service, request.source_session)?;
    let target_base = base(service, request.target_session)?;
    let engine = target_base.summary().engine;
    if source_base.summary().engine != engine {
        return Err(Error::invalid_input(format!(
            "structure sync compares two connections of the same engine, not {} and {}",
            source_base.summary().engine.display_name(),
            engine.display_name()
        )));
    }
    let ticket = service.runs.register_with(
        &[request.source_session, request.target_session],
        cancel.clone(),
    );
    let token = ticket.token().clone();
    let source = snapshot(
        service,
        &source_base,
        &request.source,
        &request.options,
        &token,
    )
    .await?;
    let target = snapshot(
        service,
        &target_base,
        &request.target,
        &request.options,
        &token,
    )
    .await?;
    check_cancelled(&token)?;
    let diff = oxbase_diff::compare(&service.dialects, &source, &target, &request.options)?;
    let compare_id = ticket.id();
    let mut skipped = source.skipped.clone();
    skipped.extend(target.skipped.iter().cloned());
    let result = StructureCompareResult {
        compare_id,
        counts: diff.counts(),
        diff: diff.clone(),
        skipped,
        elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    };
    service.kept.insert(
        compare_id,
        KeptCompare {
            request: request.clone(),
            source,
            target,
            diff,
        },
    );
    drop(ticket);
    Ok(result)
}

pub(super) fn script(
    service: &StructureSyncService,
    request: &StructureScriptRequest,
) -> Result<SyncScript> {
    let kept = service.kept.get(request.compare_id)?;
    script_of(&service.dialects, &kept, &request.selection)
}

fn script_of(
    dialects: &DialectRegistry,
    kept: &KeptCompare,
    selection: &[String],
) -> Result<SyncScript> {
    sync_script(
        dialects,
        &kept.diff,
        &kept.source,
        &kept.target,
        selection,
        &kept.request.options,
    )
}

/// The first item of `items` whose target object is not what the compare read any more
/// (changed, gone, or there now).
fn stale_item(
    dialects: &DialectRegistry,
    before: &SchemaSnapshot,
    now: &SchemaSnapshot,
    items: &[String],
) -> Option<String> {
    let index = |snapshot: &SchemaSnapshot| -> HashMap<String, ObjectDef> {
        snapshot
            .objects
            .iter()
            .map(|d| (item_id(&d.object_ref()), d.clone()))
            .collect()
    };
    let old = index(before);
    let new = index(now);
    items
        .iter()
        .find(|id| match (old.get(*id), new.get(*id)) {
            (None, None) => false,
            (Some(a), Some(b)) => match dialects.comparator(now.engine, a.kind()) {
                Ok(comparator) => !comparator.diff(a, b).is_empty(),
                Err(_) => a != b,
            },
            _ => true,
        })
        .cloned()
}

/// Every applied item is equal now, or gone from both sides, or (an object only in the target
/// that the script re-created because it reads something that changed, not one it dropped)
/// still only in the target.
fn applied_in_sync(
    before: &StructureDiff,
    after: &StructureDiff,
    items: &[String],
    dropped: impl Fn(&str) -> bool,
) -> bool {
    items
        .iter()
        .all(|id| match after.item(id).map(|i| i.status) {
            None | Some(DiffStatus::Same) => true,
            Some(DiffStatus::OnlyInTarget) => {
                !dropped(id)
                    && before
                        .item(id)
                        .is_some_and(|i| i.status == DiffStatus::OnlyInTarget)
            }
            Some(DiffStatus::OnlyInSource | DiffStatus::Different) => false,
        })
}

pub(super) async fn apply(
    service: &StructureSyncService,
    request: &StructureApplyRequest,
    cancel: CancellationToken,
) -> Result<StructureApplyReport> {
    check_cancelled(&cancel)?;
    let kept = service.kept.get(request.compare_id)?;
    let script = script_of(&service.dialects, &kept, &request.selection)?;
    if script.is_empty() {
        return Err(Error::invalid_input(
            "nothing to apply: the selected items are equal",
        ));
    }
    if script.destructive && !request.confirm_destructive {
        return Err(Error::invalid_input(CONFIRM_DESTRUCTIVE));
    }
    let compared = &kept.request;
    let target_base = base(service, compared.target_session)?;
    let ticket = service.runs.register_with(
        &[compared.source_session, compared.target_session],
        cancel.clone(),
    );
    let token = ticket.token().clone();

    // The target must still be what the compare read.
    let now = snapshot(
        service,
        &target_base,
        &compared.target,
        &compared.options,
        &token,
    )
    .await?;
    if let Some(id) = stale_item(&service.dialects, &kept.target, &now, &script.items) {
        return Err(Error::invalid_input(format!(
            "the target changed since the compare ({id}); compare again"
        )));
    }

    let (session, dedicated) =
        container_session(service, &target_base, &compared.target, &token).await?;
    let outcome = execute_script(&session, &script.script, script.atomic, token.clone()).await;
    if dedicated {
        close_quietly(service, &session).await;
    }
    let outcome = outcome?;
    if let Some(events) = &service.schema_events {
        events.publish(SchemaChangeEvent {
            session: compared.target_session,
            database: compared.target.database_name().map(str::to_owned),
            source: ChangeSource::Designer,
            // Unknown: the cache refreshes everything.
            changes: Vec::new(),
        });
    }
    drop(ticket);

    // A cancelled apply still re-compares: the token only stopped the script.
    let recompare = compare(service, compared, CancellationToken::new()).await?;
    // Objects only in the target were dropped when the selection (or, without one, the
    // compare's options) asked for drops.
    let dropped = |id: &str| {
        if request.selection.is_empty() {
            compared.options.include_drops
        } else {
            request.selection.iter().any(|s| s.as_str() == id)
        }
    };
    let in_sync =
        outcome.committed && applied_in_sync(&kept.diff, &recompare.diff, &script.items, dropped);
    Ok(StructureApplyReport {
        script,
        outcome,
        recompare,
        in_sync,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_diff::DiffItem;
    use oxbase_types::{Engine, ServerVersion, TableDef};
    use pretty_assertions::assert_eq;

    fn snapshot_of(objects: Vec<ObjectDef>) -> SchemaSnapshot {
        SchemaSnapshot {
            engine: Engine::Sqlite,
            version: ServerVersion::new(3, 50, 0),
            container: ObjectRef::database("main"),
            objects,
            skipped: Vec::new(),
        }
    }

    fn table(name: &str) -> ObjectDef {
        ObjectDef::Table(TableDef::new(ObjectRef::table(Some("main"), None, name)))
    }

    #[test]
    fn kept_compares_are_bounded_and_forgotten() {
        let kept = Kept::default();
        let mut ids = Vec::new();
        for _ in 0..KEPT_COMPARES + 2 {
            let id = OperationId::new();
            ids.push(id);
            kept.insert(
                id,
                KeptCompare {
                    request: StructureCompareRequest {
                        source_session: SessionId::new(),
                        source: ObjectRef::database("main"),
                        target_session: SessionId::new(),
                        target: ObjectRef::database("main"),
                        options: StructureOptions::default(),
                    },
                    source: snapshot_of(Vec::new()),
                    target: snapshot_of(Vec::new()),
                    diff: StructureDiff {
                        engine: Engine::Sqlite,
                        source: ObjectRef::database("main"),
                        target: ObjectRef::database("main"),
                        items: Vec::new(),
                        warnings: Vec::new(),
                    },
                },
            );
        }
        assert!(kept.get(ids[0]).is_err());
        assert!(kept.get(ids[2]).is_ok());
        assert!(kept.remove(ids[2]));
        assert!(!kept.remove(ids[2]));
        assert!(kept.get(ids[2]).is_err());
    }

    #[test]
    fn stale_items_and_sync_state() {
        let dialects = crate::default_dialects();
        let before = snapshot_of(vec![table("a"), table("b")]);
        let mut changed = TableDef::new(ObjectRef::table(Some("main"), None, "b"));
        changed.comment = Some("new".into());
        let now = snapshot_of(vec![table("a"), ObjectDef::Table(changed), table("c")]);
        let items = |ids: &[&str]| ids.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            stale_item(&dialects, &before, &now, &items(&["table:a"])),
            None
        );
        assert_eq!(
            stale_item(&dialects, &before, &now, &items(&["table:a", "table:b"])),
            Some("table:b".to_owned())
        );
        assert_eq!(
            stale_item(&dialects, &before, &now, &items(&["table:c"])),
            Some("table:c".to_owned())
        );
        assert_eq!(
            stale_item(&dialects, &before, &now, &items(&["table:x"])),
            None
        );

        let diff = StructureDiff {
            engine: Engine::Sqlite,
            source: ObjectRef::database("main"),
            target: ObjectRef::database("main"),
            items: vec![DiffItem {
                id: "table:a".into(),
                kind: ObjectKind::Table,
                name: "a".into(),
                parent: None,
                status: DiffStatus::Same,
                source: None,
                target: None,
                properties: Vec::new(),
                source_ddl: None,
                target_ddl: None,
            }],
            warnings: Vec::new(),
        };
        let no_drops = |_: &str| false;
        assert!(applied_in_sync(
            &diff,
            &diff,
            &items(&["table:a", "table:gone"]),
            no_drops
        ));
        let mut different = diff.clone();
        different.items[0].status = DiffStatus::Different;
        assert!(!applied_in_sync(
            &diff,
            &different,
            &items(&["table:a"]),
            no_drops
        ));
        // Only in the target before and after: fine when it was re-created, not when it was
        // to be dropped.
        let mut only_target = diff.clone();
        only_target.items[0].status = DiffStatus::OnlyInTarget;
        let ids = items(&["table:a"]);
        assert!(applied_in_sync(&only_target, &only_target, &ids, no_drops));
        assert!(!applied_in_sync(
            &only_target,
            &only_target,
            &ids,
            |_: &str| true
        ));
        assert!(!applied_in_sync(&diff, &only_target, &ids, no_drops));
    }
}
