//! Stored functions and procedures (`information_schema.ROUTINES` + `PARAMETERS`).
//!
//! MySQL and MariaDB have no overloading, so refs carry no `signature`.

use async_trait::async_trait;
use oxbase_dialect::Fetcher;
use oxbase_driver::{CancellationToken, Connection};
use oxbase_types::{
    ArgMode, ObjectDef, ObjectKind, ObjectRef, ObjectSummary, Result, RoutineArg, RoutineDef, props,
};

use super::ddl_text::{RoutineText, ident};
use super::rows::{query, text};
use super::{by_name, database_of, expect_kind, is_system_database, not_found, scoped};

/// Lists the functions or the procedures of a database and loads one as a [`RoutineDef`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct RoutineFetcher {
    kind: ObjectKind,
}

impl RoutineFetcher {
    /// A fetcher for `kind` (`Function` or `Procedure`).
    pub(crate) fn new(kind: ObjectKind) -> Self {
        debug_assert!(matches!(kind, ObjectKind::Function | ObjectKind::Procedure));
        Self { kind }
    }

    /// `ROUTINE_TYPE` of this fetcher's kind.
    fn routine_type(self) -> &'static str {
        if self.kind == ObjectKind::Procedure {
            "PROCEDURE"
        } else {
            "FUNCTION"
        }
    }
}

fn arg_mode(mode: Option<&str>) -> ArgMode {
    match mode.map(str::to_ascii_uppercase).as_deref() {
        Some("OUT") => ArgMode::Out,
        Some("INOUT") => ArgMode::InOut,
        _ => ArgMode::In,
    }
}

#[async_trait]
impl Fetcher for RoutineFetcher {
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
            "SELECT ROUTINE_NAME, ROUTINE_COMMENT, DTD_IDENTIFIER FROM information_schema.ROUTINES \
             WHERE ROUTINE_SCHEMA = ? AND ROUTINE_TYPE = ? ORDER BY ROUTINE_NAME",
            &[text(&db), text(self.routine_type())],
            &cancel,
        )
        .await?;
        let system = is_system_database(&db);
        Ok(by_name(
            rows.iter()
                .map(|row| {
                    let mut s = ObjectSummary::new(parent.child(self.kind, row.string(0)));
                    s.system = system;
                    s.comment = row.non_empty(1);
                    if let Some(returns) = row.non_empty(2) {
                        s.extras.insert(props::NATIVE_TYPE, returns);
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
        expect_kind(obj, self.kind)?;
        if obj.signature.as_deref().is_some_and(|s| !s.is_empty()) {
            // No overloading: a signature can only come from another engine.
            return Err(not_found(obj));
        }
        let db = database_of(c, obj, &cancel).await?;
        let params = [text(&db), text(&obj.name), text(self.routine_type())];
        let rows = query(
            c,
            "SELECT DTD_IDENTIFIER, ROUTINE_BODY, ROUTINE_DEFINITION, IS_DETERMINISTIC, \
             SQL_DATA_ACCESS, SECURITY_TYPE, DEFINER, ROUTINE_COMMENT \
             FROM information_schema.ROUTINES \
             WHERE ROUTINE_SCHEMA = ? AND ROUTINE_NAME = ? AND ROUTINE_TYPE = ?",
            &params,
            &cancel,
        )
        .await?;
        let row = rows.first().ok_or_else(|| not_found(obj))?;
        let args = query(
            c,
            "SELECT PARAMETER_NAME, PARAMETER_MODE, DTD_IDENTIFIER \
             FROM information_schema.PARAMETERS \
             WHERE SPECIFIC_SCHEMA = ? AND SPECIFIC_NAME = ? AND ROUTINE_TYPE = ? \
             AND ORDINAL_POSITION > 0 ORDER BY ORDINAL_POSITION",
            &params,
            &cancel,
        )
        .await?;

        let mut def = RoutineDef::new(scoped(obj, &db));
        def.args = args
            .iter()
            .map(|a| RoutineArg {
                name: a.non_empty(0),
                mode: arg_mode(a.text(1).as_deref()),
                native_type: a.string(2),
                default: None,
            })
            .collect();
        if self.kind == ObjectKind::Function {
            def.returns = row.non_empty(0);
        }
        def.language = Some(row.non_empty(1).unwrap_or_else(|| "SQL".to_owned()));
        def.body = row.text(2);
        def.comment = row.non_empty(7);
        let deterministic = row.flag(3);
        let data_access = row.non_empty(4);
        let security = row.non_empty(5);
        let definer = row.non_empty(6);
        def.extras.insert(props::DETERMINISTIC, deterministic);
        if let Some(a) = &data_access {
            def.extras.insert(props::SQL_DATA_ACCESS, a.clone());
        }
        if let Some(s) = &security {
            def.extras.insert(props::SECURITY, s.clone());
        }
        if let Some(d) = &definer {
            def.extras.insert(props::DEFINER, d.clone());
        }
        // Without the privilege to see the body (not the definer, no SHOW_ROUTINE / global
        // SELECT), ROUTINE_DEFINITION is NULL and there is nothing to assemble.
        def.definition = def.body.as_deref().map(|body| {
            let procedure = self.kind == ObjectKind::Procedure;
            RoutineText {
                definer: definer.as_deref(),
                kind: self.routine_type(),
                name: &obj.name,
                params: def
                    .args
                    .iter()
                    .map(|a| {
                        let name = a.name.as_deref().map(ident).unwrap_or_default();
                        let mode = match (procedure, a.mode) {
                            (false, _) => "",
                            (true, ArgMode::Out) => "OUT ",
                            (true, ArgMode::InOut) => "INOUT ",
                            (true, _) => "IN ",
                        };
                        format!("{mode}{name} {}", a.native_type)
                    })
                    .collect(),
                returns: def.returns.as_deref(),
                deterministic,
                data_access: data_access.as_deref(),
                security: security.as_deref(),
                comment: def.comment.as_deref(),
                body,
            }
            .render()
        });
        Ok(ObjectDef::Routine(def))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_and_types() {
        assert_eq!(arg_mode(None), ArgMode::In);
        assert_eq!(arg_mode(Some("in")), ArgMode::In);
        assert_eq!(arg_mode(Some("OUT")), ArgMode::Out);
        assert_eq!(arg_mode(Some("INOUT")), ArgMode::InOut);
        assert_eq!(
            RoutineFetcher::new(ObjectKind::Procedure).routine_type(),
            "PROCEDURE"
        );
        assert_eq!(
            RoutineFetcher::new(ObjectKind::Function).routine_type(),
            "FUNCTION"
        );
    }
}
