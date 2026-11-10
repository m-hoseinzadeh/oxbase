//! M8 structure sync DDL cells (`docs/m8-contracts.md` §5.2) for MySQL and MariaDB views,
//! functions, procedures and triggers, and MariaDB sequences. Tables keep the M3 cells
//! (`ddl.rs`).
//!
//! # Statements
//!
//! - Views: `CREATE [ALGORITHM = ...] [SQL SECURITY ...] VIEW db.v AS <query> [WITH ... CHECK
//!   OPTION]` (no `DEFINER`: the applying user defines it); an alter is `CREATE OR REPLACE`.
//! - Routines: built from the model like `SHOW CREATE` prints them, with the name qualified by
//!   its database and without `DEFINER`; an alter drops and creates (MySQL alters only a
//!   routine's characteristics in place).
//! - Triggers: `CREATE TRIGGER db.t {BEFORE|AFTER} <event> ON db.table FOR EACH ROW <body>` (the
//!   trigger name carries the table's database, as the server requires); an alter drops and
//!   creates.
//! - MariaDB sequences: `CREATE SEQUENCE db.s [AS type] START WITH ... MINVALUE ... MAXVALUE ...
//!   INCREMENT BY ... [CACHE n] [NO]CYCLE [COMMENT = ...]`. An alter keeps the sequence and its
//!   current value: `ALTER SEQUENCE` with every option, `ALTER TABLE db.s COMMENT = ...` for the
//!   comment (a sequence is a table). Only a new value type needs a drop and a create; that
//!   drop stays destructive (the sequence starts again) and the script warns.
//!
//! Every statement commits on its own (`transactional = false`); drops are destructive.
//!
//! # Definers
//!
//! No statement carries a `DEFINER` (setting another account needs `SET_USER_ID` / `SUPER`),
//! so the server records the account that applies the script, also on an alter, where the
//! target's definer is lost. A trigger always runs with its definer's privileges, and so does a
//! view or routine whose `SQL SECURITY` is not `INVOKER`: for each of them whose definition
//! names a definer (`props::DEFINER`), the script warns that the definer is not kept.

use std::fmt::Write as _;

use oxbase_dialect::compare::CompareRules;
use oxbase_dialect::{BacktickQuoting, DdlContext, DdlGenerator, Quoting};
use oxbase_types::{
    ArgMode, Error, Extras, ObjectDef, ObjectKind, ObjectRef, PropId, Result, RoutineDef, Script,
    ScriptStep, SequenceDef, TriggerDef, TriggerEvent, TriggerTiming, Value, ViewDef, props,
};

use super::ACTION_ORDER;

/// The kinds these cells cover on MySQL.
pub(crate) const KINDS: [ObjectKind; 4] = [
    ObjectKind::View,
    ObjectKind::Function,
    ObjectKind::Procedure,
    ObjectKind::Trigger,
];

/// The structure sync DDL generator of MySQL and MariaDB (module docs).
#[derive(Debug, Clone, Copy)]
pub(crate) struct MySqlSyncDdl {
    pub mariadb: bool,
}

/// Comparison rules of the definitions: the order among triggers of the same timing and
/// event (`ACTION_ORDER`) is the server's numbering, not part of a definition.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct MySqlSyncRules;

impl CompareRules for MySqlSyncRules {
    fn compare_extra(&self, _kind: ObjectKind, prop: &PropId) -> bool {
        *prop != ACTION_ORDER
    }
}

const NOT_ATOMIC: &str = "Each statement commits on its own: if one fails, the statements \
                          before it stay applied.";

fn q(r: &ObjectRef) -> String {
    BacktickQuoting.qualify(r)
}

fn ident(s: &str) -> String {
    BacktickQuoting.quote_ident(s)
}

fn lit(s: &str) -> String {
    BacktickQuoting.quote_str(s)
}

fn step(sql: String, description: String) -> ScriptStep {
    ScriptStep::new(sql)
        .described(description)
        .non_transactional()
}

fn not_covered(kind: ObjectKind) -> Error {
    Error::unsupported(format!(
        "MySQL / MariaDB structure sync has no DDL for a {}",
        kind.display_name().to_lowercase()
    ))
}

fn text_extra<'a>(extras: &'a Extras, prop: &PropId) -> Option<&'a str> {
    match extras.get(prop) {
        Some(Value::Text(t)) if !t.is_empty() => Some(t.as_str()),
        _ => None,
    }
}

impl MySqlSyncDdl {
    fn kinds_ok(self, kind: ObjectKind) -> Result<()> {
        if KINDS.contains(&kind) || (self.mariadb && kind == ObjectKind::Sequence) {
            Ok(())
        } else {
            Err(not_covered(kind))
        }
    }
}

impl DdlGenerator for MySqlSyncDdl {
    fn create(&self, d: &ObjectDef, _ctx: &DdlContext<'_>) -> Result<Script> {
        self.kinds_ok(d.kind())?;
        match d {
            ObjectDef::View(v) => Ok(create_view(v, false)),
            ObjectDef::Routine(r) => create_routine(r),
            ObjectDef::Trigger(t) => create_trigger(t),
            ObjectDef::Sequence(s) => Ok(create_sequence(s)),
            other => Err(not_covered(other.kind())),
        }
    }

    fn alter(&self, old: &ObjectDef, new: &ObjectDef, ctx: &DdlContext<'_>) -> Result<Script> {
        self.kinds_ok(new.kind())?;
        match (old, new) {
            (ObjectDef::View(_), ObjectDef::View(v)) => Ok(create_view(v, true)),
            (ObjectDef::Sequence(a), ObjectDef::Sequence(b))
                if same_text(a.data_type.as_deref(), b.data_type.as_deref()) =>
            {
                let mut script = alter_sequence(a, b);
                not_atomic_warning(&mut script);
                Ok(script)
            }
            (ObjectDef::Sequence(_), ObjectDef::Sequence(b)) => {
                // The drop stays destructive: the sequence starts again.
                let mut script = self.drop_object(&old.object_ref(), ctx)?;
                script.append(self.create(new, ctx)?);
                script.warn(format!(
                    "sequence {} is dropped and created again (its value type changes): it \
                     starts again at {}",
                    b.obj.name, b.start
                ));
                not_atomic_warning(&mut script);
                Ok(script)
            }
            _ if old.kind() != new.kind() => Err(Error::invalid_input(format!(
                "cannot alter a {} into a {}",
                old.kind().display_name(),
                new.kind().display_name()
            ))),
            _ => {
                let mut script = self.drop_object(&old.object_ref(), ctx)?;
                for s in &mut script.steps {
                    // Created again right after.
                    s.destructive = false;
                }
                script.append(self.create(new, ctx)?);
                not_atomic_warning(&mut script);
                Ok(script)
            }
        }
    }

    fn drop_object(&self, r: &ObjectRef, _ctx: &DdlContext<'_>) -> Result<Script> {
        self.kinds_ok(r.kind)?;
        let keyword = match r.kind {
            ObjectKind::View => "VIEW",
            ObjectKind::Function => "FUNCTION",
            ObjectKind::Procedure => "PROCEDURE",
            ObjectKind::Trigger => "TRIGGER",
            ObjectKind::Sequence => "SEQUENCE",
            other => return Err(not_covered(other)),
        };
        let mut script = Script::new();
        script.push_step(
            step(
                format!("DROP {keyword} {}", q(r)),
                format!("Drop {} {}", r.kind.display_name().to_lowercase(), r.name),
            )
            .destructive(),
        );
        Ok(script)
    }
}

fn same_text(a: Option<&str>, b: Option<&str>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => x.trim().eq_ignore_ascii_case(y.trim()),
        _ => false,
    }
}

/// Warns that `what` loses its definer (module docs, "Definers"): it runs with the privileges
/// of the account that applies the script from then on. An `INVOKER` view or routine runs with
/// the caller's privileges, so its definer does not matter.
fn definer_warning(script: &mut Script, what: &str, kind: ObjectKind, extras: &Extras) {
    let Some(definer) = text_extra(extras, &props::DEFINER) else {
        return;
    };
    let invoker = kind != ObjectKind::Trigger
        && text_extra(extras, &props::SECURITY).is_some_and(|s| s.eq_ignore_ascii_case("INVOKER"));
    if !invoker {
        script.warn(format!(
            "{what} is created without its DEFINER {definer}: the account that applies this \
             script becomes its definer, and it runs with that account's privileges"
        ));
    }
}

fn create_view(v: &ViewDef, replace: bool) -> Script {
    let mut sql = String::from("CREATE ");
    if replace {
        sql.push_str("OR REPLACE ");
    }
    if let Some(algorithm) = text_extra(&v.extras, &props::VIEW_ALGORITHM) {
        let _ = write!(sql, "ALGORITHM = {} ", algorithm.to_uppercase());
    }
    if let Some(security) = text_extra(&v.extras, &props::SECURITY) {
        let _ = write!(sql, "SQL SECURITY {} ", security.to_uppercase());
    }
    let _ = write!(sql, "VIEW {}", q(&v.obj));
    if !v.columns.is_empty() {
        let columns: Vec<String> = v.columns.iter().map(|c| ident(c)).collect();
        let _ = write!(sql, " ({})", columns.join(", "));
    }
    let _ = write!(sql, " AS {}", v.definition.trim().trim_end_matches(';'));
    if let Some(option) = text_extra(&v.extras, &props::CHECK_OPTION) {
        let _ = write!(sql, " WITH {} CHECK OPTION", option.to_uppercase());
    }
    let mut script = Script::new();
    script.push_step(step(
        sql,
        format!(
            "{} view {}",
            if replace { "Replace" } else { "Create" },
            v.obj.name
        ),
    ));
    definer_warning(
        &mut script,
        &format!("view {}", v.obj.name),
        ObjectKind::View,
        &v.extras,
    );
    script
}

fn create_routine(r: &RoutineDef) -> Result<Script> {
    let procedure = r.obj.kind == ObjectKind::Procedure;
    let body = r.body.as_deref().ok_or_else(|| {
        Error::unsupported(format!(
            "the body of {} cannot be read (it needs the definer's privileges), so it cannot \
             be created",
            r.obj.name
        ))
    })?;
    let params: Vec<String> = r
        .args
        .iter()
        .map(|a| {
            let mode = match (procedure, a.mode) {
                (false, _) => "",
                (true, ArgMode::Out) => "OUT ",
                (true, ArgMode::InOut) => "INOUT ",
                (true, _) => "IN ",
            };
            let name = a.name.as_deref().map(ident).unwrap_or_default();
            format!("{mode}{name} {}", a.native_type)
        })
        .collect();
    let keyword = if procedure { "PROCEDURE" } else { "FUNCTION" };
    let mut sql = format!("CREATE {keyword} {}({})", q(&r.obj), params.join(", "));
    if !procedure {
        let returns = r.returns.as_deref().ok_or_else(|| {
            Error::invalid_input(format!("function {} has no return type", r.obj.name))
        })?;
        let _ = write!(sql, " RETURNS {returns}");
    }
    if r.extras.get(&props::DETERMINISTIC) == Some(&Value::Bool(true)) {
        sql.push_str("\n    DETERMINISTIC");
    }
    if let Some(access) = text_extra(&r.extras, &props::SQL_DATA_ACCESS) {
        let _ = write!(sql, "\n    {}", access.to_uppercase());
    }
    if let Some(security) = text_extra(&r.extras, &props::SECURITY) {
        let _ = write!(sql, "\n    SQL SECURITY {}", security.to_uppercase());
    }
    if let Some(comment) = r.comment.as_deref().filter(|c| !c.is_empty()) {
        let _ = write!(sql, "\n    COMMENT {}", lit(comment));
    }
    sql.push('\n');
    sql.push_str(body);
    let mut script = Script::new();
    script.push_step(step(
        sql,
        format!(
            "Create {} {}",
            if procedure { "procedure" } else { "function" },
            r.obj.name
        ),
    ));
    definer_warning(
        &mut script,
        &format!(
            "{} {}",
            if procedure { "procedure" } else { "function" },
            r.obj.name
        ),
        r.obj.kind,
        &r.extras,
    );
    Ok(script)
}

fn create_trigger(t: &TriggerDef) -> Result<Script> {
    let table = t
        .obj
        .owner()
        .ok_or_else(|| Error::invalid_input(format!("trigger {} names no table", t.obj.name)))?;
    let body = t.body.as_deref().ok_or_else(|| {
        Error::unsupported(format!(
            "the body of trigger {} cannot be read, so it cannot be created",
            t.obj.name
        ))
    })?;
    let timing = match t.timing {
        TriggerTiming::Before => "BEFORE",
        TriggerTiming::After => "AFTER",
        TriggerTiming::InsteadOf => {
            return Err(Error::unsupported(
                "MySQL and MariaDB have no INSTEAD OF triggers",
            ));
        }
    };
    let events: Vec<&str> = t
        .events
        .iter()
        .map(|e| match e {
            TriggerEvent::Insert => "INSERT",
            TriggerEvent::Update => "UPDATE",
            TriggerEvent::Delete | TriggerEvent::Truncate => "DELETE",
        })
        .collect();
    let mut trigger = t.obj.clone();
    // The trigger lives in its table's database.
    trigger.catalog.clone_from(&table.catalog);
    let sql = format!(
        "CREATE TRIGGER {} {timing} {} ON {} FOR EACH ROW {body}",
        q(&trigger),
        events.join(" OR "),
        q(&table)
    );
    let mut script = Script::new();
    script.push_step(step(
        sql,
        format!("Create trigger {} on {}", t.obj.name, table.name),
    ));
    definer_warning(
        &mut script,
        &format!("trigger {} on {}", t.obj.name, table.name),
        ObjectKind::Trigger,
        &t.extras,
    );
    Ok(script)
}

fn sequence_options(s: &SequenceDef) -> String {
    let mut out = String::new();
    let _ = write!(out, " START WITH {}", s.start);
    if let Some(min) = s.min_value {
        let _ = write!(out, " MINVALUE {min}");
    }
    if let Some(max) = s.max_value {
        let _ = write!(out, " MAXVALUE {max}");
    }
    let _ = write!(out, " INCREMENT BY {}", s.increment);
    if let Some(cache) = s.cache {
        let _ = write!(out, " CACHE {cache}");
    }
    out.push_str(if s.cycle { " CYCLE" } else { " NOCYCLE" });
    out
}

fn create_sequence(s: &SequenceDef) -> Script {
    let mut sql = format!("CREATE SEQUENCE {}", q(&s.obj));
    if let Some(data_type) = s.data_type.as_deref().filter(|t| !t.trim().is_empty()) {
        let _ = write!(sql, " AS {data_type}");
    }
    sql.push_str(&sequence_options(s));
    if let Some(comment) = s.comment.as_deref().filter(|c| !c.is_empty()) {
        let _ = write!(sql, " COMMENT = {}", lit(comment));
    }
    let mut script = Script::new();
    script.push_step(step(sql, format!("Create sequence {}", s.obj.name)));
    script
}

/// The options and the comment changed in place (the current value stays).
fn alter_sequence(old: &SequenceDef, new: &SequenceDef) -> Script {
    let mut script = Script::new();
    if sequence_options(old) != sequence_options(new) {
        script.push_step(step(
            format!("ALTER SEQUENCE {}{}", q(&new.obj), sequence_options(new)),
            format!("Change sequence {}", new.obj.name),
        ));
    }
    let before = old.comment.as_deref().unwrap_or_default();
    let after = new.comment.as_deref().unwrap_or_default();
    if before != after {
        script.push_step(step(
            format!("ALTER TABLE {} COMMENT = {}", q(&new.obj), lit(after)),
            format!("Change the comment of sequence {}", new.obj.name),
        ));
    }
    script
}

/// The warning every script with more than one statement carries.
pub(crate) fn not_atomic_warning(script: &mut Script) {
    if script.len() > 1 && !script.warnings.iter().any(|w| w.as_str() == NOT_ATOMIC) {
        script.warn(NOT_ATOMIC);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_types::{RoutineArg, ServerVersion, TriggerLevel};

    fn db() -> ObjectRef {
        ObjectRef::database("m8_tgt")
    }

    fn sql(script: &Script) -> Vec<&str> {
        script.steps.iter().map(|s| s.sql.as_str()).collect()
    }

    const MYSQL: MySqlSyncDdl = MySqlSyncDdl { mariadb: false };
    const MARIADB: MySqlSyncDdl = MySqlSyncDdl { mariadb: true };

    #[test]
    fn views() {
        let version = ServerVersion::new(8, 4, 13);
        let ctx = DdlContext::new(&version);
        let view = ViewDef {
            obj: db().child(ObjectKind::View, "active_customers"),
            definition: "select `m8_tgt`.`customers`.`id` AS `id` from `m8_tgt`.`customers`".into(),
            columns: Vec::new(),
            comment: None,
            extras: Extras::new()
                .with(props::SECURITY, "INVOKER")
                .with(props::CHECK_OPTION, "cascaded")
                .with(props::DEFINER, "oxbase@%"),
        };
        let script = MYSQL.create(&ObjectDef::View(view.clone()), &ctx).unwrap();
        assert_eq!(
            sql(&script),
            [
                "CREATE SQL SECURITY INVOKER VIEW `m8_tgt`.`active_customers` AS select `m8_tgt`.`customers`.`id` AS `id` from `m8_tgt`.`customers` WITH CASCADED CHECK OPTION"
            ]
        );
        assert!(!script.steps[0].transactional);
        assert_eq!(script.warnings, Vec::<String>::new(), "an INVOKER view");
        let script = MYSQL
            .alter(
                &ObjectDef::View(view.clone()),
                &ObjectDef::View(view.clone()),
                &ctx,
            )
            .unwrap();
        assert!(
            script.steps[0]
                .sql
                .starts_with("CREATE OR REPLACE SQL SECURITY")
        );
        assert_eq!(
            sql(&MYSQL.drop_object(&view.obj, &ctx).unwrap()),
            ["DROP VIEW `m8_tgt`.`active_customers`"]
        );
    }

    #[test]
    fn routines_and_triggers() {
        let version = ServerVersion::new(8, 4, 13);
        let ctx = DdlContext::new(&version);
        let mut func = RoutineDef::new(db().child(ObjectKind::Function, "order_total"));
        func.args.push(RoutineArg {
            name: Some("p_id".into()),
            mode: ArgMode::In,
            native_type: "int".into(),
            default: None,
        });
        func.returns = Some("decimal(12,2)".into());
        func.body = Some(
            "RETURN (SELECT SUM(qty) FROM `m8_tgt`.`order_items` WHERE order_id = p_id)".into(),
        );
        func.extras.insert(props::DETERMINISTIC, false);
        func.extras.insert(props::SQL_DATA_ACCESS, "READS SQL DATA");
        func.extras.insert(props::SECURITY, "DEFINER");
        func.comment = Some("it's".into());
        let script = MYSQL
            .create(&ObjectDef::Routine(func.clone()), &ctx)
            .unwrap();
        assert_eq!(
            sql(&script),
            [
                "CREATE FUNCTION `m8_tgt`.`order_total`(`p_id` int) RETURNS decimal(12,2)\n    READS SQL DATA\n    SQL SECURITY DEFINER\n    COMMENT 'it''s'\nRETURN (SELECT SUM(qty) FROM `m8_tgt`.`order_items` WHERE order_id = p_id)"
            ]
        );
        let script = MYSQL
            .alter(
                &ObjectDef::Routine(func.clone()),
                &ObjectDef::Routine(func.clone()),
                &ctx,
            )
            .unwrap();
        assert_eq!(script.steps[0].sql, "DROP FUNCTION `m8_tgt`.`order_total`");
        assert!(!script.is_destructive());

        let table = ObjectRef::table(Some("m8_tgt"), None, "orders");
        let trigger = TriggerDef {
            obj: table.child(ObjectKind::Trigger, "orders_audit"),
            timing: TriggerTiming::After,
            events: vec![TriggerEvent::Insert],
            update_columns: Vec::new(),
            level: TriggerLevel::Row,
            condition: None,
            function: None,
            body: Some("INSERT INTO order_audit (order_id) VALUES (NEW.id)".into()),
            definition: None,
            enabled: true,
            comment: None,
            extras: Extras::new().with(ACTION_ORDER, 1_i64),
        };
        let script = MYSQL
            .create(&ObjectDef::Trigger(trigger.clone()), &ctx)
            .unwrap();
        assert_eq!(
            sql(&script),
            [
                "CREATE TRIGGER `m8_tgt`.`orders_audit` AFTER INSERT ON `m8_tgt`.`orders` FOR EACH ROW INSERT INTO order_audit (order_id) VALUES (NEW.id)"
            ]
        );
        assert_eq!(
            sql(&MYSQL.drop_object(&trigger.obj, &ctx).unwrap()),
            ["DROP TRIGGER `m8_tgt`.`orders_audit`"]
        );
        assert!(!MySqlSyncRules.compare_extra(ObjectKind::Trigger, &ACTION_ORDER));
    }

    #[test]
    fn definers_are_not_kept_and_the_script_warns() {
        let version = ServerVersion::new(8, 4, 13);
        let ctx = DdlContext::new(&version);
        let warned = |script: &Script, what: &str| {
            script.warnings.iter().any(|w| {
                w.starts_with(&format!("{what} is created without its DEFINER low@%"))
                    && w.contains("the account that applies this script becomes its definer")
            })
        };

        // A view: SQL SECURITY DEFINER (also when the security is not known) warns, on create
        // and on the CREATE OR REPLACE alter; INVOKER does not.
        let mut view = ViewDef {
            obj: db().child(ObjectKind::View, "v_orders"),
            definition: "select 1 AS `x`".into(),
            columns: Vec::new(),
            comment: None,
            extras: Extras::new()
                .with(props::SECURITY, "DEFINER")
                .with(props::DEFINER, "low@%"),
        };
        let script = MYSQL.create(&ObjectDef::View(view.clone()), &ctx).unwrap();
        assert!(
            !script.steps[0].sql.contains("DEFINER ="),
            "{:?}",
            sql(&script)
        );
        assert!(warned(&script, "view v_orders"), "{:?}", script.warnings);
        let script = MYSQL
            .alter(
                &ObjectDef::View(view.clone()),
                &ObjectDef::View(view.clone()),
                &ctx,
            )
            .unwrap();
        assert!(warned(&script, "view v_orders"), "{:?}", script.warnings);
        view.extras.remove(&props::SECURITY);
        let script = MYSQL.create(&ObjectDef::View(view.clone()), &ctx).unwrap();
        assert!(warned(&script, "view v_orders"), "{:?}", script.warnings);
        view.extras.insert(props::SECURITY, "invoker");
        let script = MYSQL.create(&ObjectDef::View(view), &ctx).unwrap();
        assert_eq!(script.warnings, Vec::<String>::new());

        // A routine: the same rule; an alter drops and creates, and warns too.
        let mut routine = RoutineDef::new(db().child(ObjectKind::Procedure, "p"));
        routine.body = Some("SELECT 1".into());
        routine.extras.insert(props::SECURITY, "DEFINER");
        let script = MYSQL
            .create(&ObjectDef::Routine(routine.clone()), &ctx)
            .unwrap();
        assert_eq!(script.warnings, Vec::<String>::new(), "no definer known");
        routine.extras.insert(props::DEFINER, "low@%");
        let script = MYSQL
            .alter(
                &ObjectDef::Routine(routine.clone()),
                &ObjectDef::Routine(routine.clone()),
                &ctx,
            )
            .unwrap();
        assert!(warned(&script, "procedure p"), "{:?}", script.warnings);
        assert!(
            script.warnings.iter().any(|w| w.as_str() == NOT_ATOMIC),
            "{:?}",
            script.warnings
        );
        routine.extras.insert(props::SECURITY, "INVOKER");
        let script = MYSQL.create(&ObjectDef::Routine(routine), &ctx).unwrap();
        assert_eq!(script.warnings, Vec::<String>::new());

        // A trigger always runs as its definer.
        let table = ObjectRef::table(Some("m8_tgt"), None, "orders");
        let trigger = TriggerDef {
            obj: table.child(ObjectKind::Trigger, "orders_t"),
            timing: TriggerTiming::After,
            events: vec![TriggerEvent::Insert],
            update_columns: Vec::new(),
            level: TriggerLevel::Row,
            condition: None,
            function: None,
            body: Some("SET @x = 1".into()),
            definition: None,
            enabled: true,
            comment: None,
            extras: Extras::new()
                .with(props::DEFINER, "low@%")
                .with(props::SECURITY, "INVOKER"),
        };
        let script = MYSQL
            .create(&ObjectDef::Trigger(trigger.clone()), &ctx)
            .unwrap();
        assert!(
            warned(&script, "trigger orders_t on orders"),
            "{:?}",
            script.warnings
        );
        let script = MYSQL
            .alter(
                &ObjectDef::Trigger(trigger.clone()),
                &ObjectDef::Trigger(trigger),
                &ctx,
            )
            .unwrap();
        assert!(
            warned(&script, "trigger orders_t on orders"),
            "{:?}",
            script.warnings
        );
    }

    #[test]
    fn mariadb_sequences() {
        let version = ServerVersion::new(11, 8, 9);
        let ctx = DdlContext::new(&version);
        let mut s = SequenceDef::new(db().child(ObjectKind::Sequence, "invoice_seq"));
        s.data_type = Some("bigint".into());
        s.start = 1000;
        s.min_value = Some(1);
        s.max_value = Some(9_999_999);
        let script = MARIADB
            .create(&ObjectDef::Sequence(s.clone()), &ctx)
            .unwrap();
        assert_eq!(
            sql(&script),
            [
                "CREATE SEQUENCE `m8_tgt`.`invoice_seq` AS bigint START WITH 1000 MINVALUE 1 MAXVALUE 9999999 INCREMENT BY 1 NOCYCLE"
            ]
        );
        let mut changed = s.clone();
        changed.increment = 5;
        let script = MARIADB
            .alter(
                &ObjectDef::Sequence(s.clone()),
                &ObjectDef::Sequence(changed.clone()),
                &ctx,
            )
            .unwrap();
        assert_eq!(
            sql(&script),
            [
                "ALTER SEQUENCE `m8_tgt`.`invoice_seq` START WITH 1000 MINVALUE 1 MAXVALUE 9999999 INCREMENT BY 5 NOCYCLE"
            ]
        );
        // A comment changes in place, with the options kept.
        let mut commented = s.clone();
        commented.comment = Some("it's".into());
        let script = MARIADB
            .alter(
                &ObjectDef::Sequence(s.clone()),
                &ObjectDef::Sequence(commented.clone()),
                &ctx,
            )
            .unwrap();
        assert_eq!(
            sql(&script),
            ["ALTER TABLE `m8_tgt`.`invoice_seq` COMMENT = 'it''s'"]
        );
        assert!(!script.is_destructive());
        let script = MARIADB
            .alter(
                &ObjectDef::Sequence(commented),
                &ObjectDef::Sequence(s.clone()),
                &ctx,
            )
            .unwrap();
        assert_eq!(
            sql(&script),
            ["ALTER TABLE `m8_tgt`.`invoice_seq` COMMENT = ''"]
        );
        // A new value type: drop and create, destructive, with a warning.
        changed.data_type = Some("int".into());
        let script = MARIADB
            .alter(
                &ObjectDef::Sequence(s.clone()),
                &ObjectDef::Sequence(changed),
                &ctx,
            )
            .unwrap();
        assert_eq!(script.steps[0].sql, "DROP SEQUENCE `m8_tgt`.`invoice_seq`");
        assert!(script.is_destructive());
        assert!(script.warnings.iter().any(|w| w.contains("starts again")));
        assert!(MYSQL.create(&ObjectDef::Sequence(s), &ctx).is_err());
        let mut many = Script::new();
        many.push("a").push("b");
        not_atomic_warning(&mut many);
        not_atomic_warning(&mut many);
        assert_eq!(many.warnings.len(), 1);
    }

    #[test]
    fn registers_every_structure_sync_role() {
        let mut reg = oxbase_dialect::DialectRegistry::new();
        super::super::register(&mut reg);
        for engine in [oxbase_types::Engine::MySql, oxbase_types::Engine::MariaDb] {
            let missing: Vec<String> = reg
                .missing_m8_roles(engine)
                .into_iter()
                .filter(|m| !m.contains("TypeMapper"))
                .collect();
            assert_eq!(missing, Vec::<String>::new(), "{engine}");
        }
        assert!(
            reg.ddl(oxbase_types::Engine::MariaDb, ObjectKind::Sequence)
                .is_ok()
        );
    }
}
