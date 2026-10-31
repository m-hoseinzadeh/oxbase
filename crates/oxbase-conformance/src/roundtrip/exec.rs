//! Runs a generated [`Script`] on a connection with the semantics of the `oxbase_types::script`
//! module docs: prepare steps outside any transaction, the main steps in one transaction when
//! `atomic` (else one by one, stopping at the first failure), then the finally steps whatever
//! happened; captures, `only_if` conditions and `expect_no_rows` checks.
//!
//! The harness has its own small executor so the round trip depends only on the dialect roles
//! it checks (generator, fetcher, comparator), not on the designer service. Unlike an
//! interactive apply, a failing finally step is an error here: a round trip must leave the
//! session as it found it.

use std::collections::HashMap;

use arrow_array::{
    Array, BooleanArray, Float32Array, Float64Array, Int8Array, Int16Array, Int32Array, Int64Array,
    LargeStringArray, StringArray, StringViewArray, UInt8Array, UInt16Array, UInt32Array,
    UInt64Array,
};
use oxbase_driver::{CancellationToken, Collected, Connection, ResultStream, Tx};
use oxbase_types::{Result, Script, ScriptStep, StepCheck, StepPhase, Value};

/// Where a step runs: on the connection (auto-commit) or inside the script's transaction.
enum On<'a> {
    Conn(&'a dyn Connection),
    Tx(&'a dyn Tx),
}

impl On<'_> {
    async fn execute(
        &self,
        sql: &str,
        params: &[Value],
        cancel: CancellationToken,
    ) -> Result<ResultStream> {
        match self {
            Self::Conn(c) => c.execute(sql, params, cancel).await,
            Self::Tx(t) => t.execute(sql, params, cancel).await,
        }
    }
}

/// Runs `sql` with `params` on `conn` and drains the result.
pub async fn exec(
    conn: &dyn Connection,
    sql: &str,
    params: &[Value],
    cancel: &CancellationToken,
) -> std::result::Result<Collected, String> {
    let stream = conn
        .execute(sql, params, cancel.clone())
        .await
        .map_err(|e| e.to_string())?;
    oxbase_driver::collect(stream)
        .await
        .map_err(|e| e.to_string())
}

/// Runs `script` on `conn` (see the module docs). `Err` names the failing step.
pub async fn run_script(
    conn: &dyn Connection,
    script: &Script,
    atomic: bool,
    cancel: &CancellationToken,
) -> std::result::Result<(), String> {
    let mut captures: HashMap<String, String> = HashMap::new();
    let phase = |p: StepPhase| script.steps.iter().filter(move |s| s.phase == p);

    let mut result = Ok(());
    for step in phase(StepPhase::Prepare) {
        if let Err(e) = run_step(&On::Conn(conn), step, &mut captures, cancel).await {
            result = Err(e);
            break;
        }
    }
    if result.is_ok() {
        result = if atomic {
            run_atomic(conn, phase(StepPhase::Main), &mut captures, cancel).await
        } else {
            let mut r = Ok(());
            for step in phase(StepPhase::Main) {
                if let Err(e) = run_step(&On::Conn(conn), step, &mut captures, cancel).await {
                    r = Err(e);
                    break;
                }
            }
            r
        };
    }
    // Finally steps run after a failure too; their own failures are reported after the first
    // error.
    let mut finally_errors = Vec::new();
    for step in phase(StepPhase::Finally) {
        if let Err(e) = run_step(&On::Conn(conn), step, &mut captures, cancel).await {
            finally_errors.push(e);
        }
    }
    match (result, finally_errors.is_empty()) {
        (Ok(()), true) => Ok(()),
        (Ok(()), false) => Err(format!("finally: {}", finally_errors.join("; "))),
        (Err(e), true) => Err(e),
        (Err(e), false) => Err(format!("{e} (and finally: {})", finally_errors.join("; "))),
    }
}

/// The main steps in one transaction: committed when every step succeeds, else rolled back.
async fn run_atomic<'s>(
    conn: &dyn Connection,
    steps: impl Iterator<Item = &'s ScriptStep>,
    captures: &mut HashMap<String, String>,
    cancel: &CancellationToken,
) -> std::result::Result<(), String> {
    let tx = conn
        .begin()
        .await
        .map_err(|e| format!("cannot begin the transaction: {e}"))?;
    for step in steps {
        if let Err(e) = run_step(&On::Tx(tx.as_ref()), step, captures, cancel).await {
            let rolled_back = match tx.rollback().await {
                Ok(()) => String::new(),
                Err(re) => format!(" (rollback failed: {re})"),
            };
            return Err(format!("{e}{rolled_back}"));
        }
    }
    tx.commit().await.map_err(|e| format!("commit failed: {e}"))
}

/// Runs one step: honours `only_if`, stores `capture`, enforces `expect_no_rows`.
async fn run_step(
    on: &On<'_>,
    step: &ScriptStep,
    captures: &mut HashMap<String, String>,
    cancel: &CancellationToken,
) -> std::result::Result<(), String> {
    if let Some(cond) = &step.only_if {
        let have = captures.get(&cond.capture).map_or("", String::as_str);
        if have != cond.equals {
            return Ok(());
        }
    }
    let label = describe(step);
    let stream = on
        .execute(&step.sql, &[], cancel.clone())
        .await
        .map_err(|e| format!("{label}: {e}"))?;
    let out = oxbase_driver::collect(stream)
        .await
        .map_err(|e| format!("{label}: {e}"))?;
    if let Some(name) = &step.capture {
        captures.insert(name.clone(), first_text(&out).unwrap_or_default());
    }
    if step.check == StepCheck::ExpectNoRows {
        let rows: usize = out.result_sets.iter().map(|r| r.num_rows()).sum();
        if rows > 0 {
            return Err(format!(
                "{label}: expected no rows, got {rows} (first value: {})",
                first_text(&out).unwrap_or_else(|| "NULL".to_owned())
            ));
        }
    }
    Ok(())
}

/// `description` (SQL) of a step, for errors.
fn describe(step: &ScriptStep) -> String {
    if step.description.is_empty() {
        format!("step `{}`", step.sql)
    } else {
        format!("step {:?} (`{}`)", step.description, step.sql)
    }
}

/// The first column of the first row of the first result set, as text. `None` for no row or
/// NULL.
pub fn first_text(out: &Collected) -> Option<String> {
    let batch = out
        .result_sets
        .iter()
        .flat_map(|r| r.batches.iter())
        .find(|b| b.num_rows() > 0 && b.num_columns() > 0)?;
    cell_text(batch.column(0).as_ref(), 0)
}

/// `Some(value(row).to_string())` from the first of the array types `$ty` that `$any` is.
macro_rules! read_as {
    ($any:expr, $row:expr, $($ty:ty),*) => {{
        let mut text = None;
        $(
            if text.is_none() && let Some(a) = $any.downcast_ref::<$ty>() {
                text = Some(a.value($row).to_string());
            }
        )*
        text
    }};
}

/// One cell as text for the types catalog answers use (integers, floats, booleans, strings).
/// `None` for NULL or a type the harness does not read.
pub fn cell_text(array: &dyn Array, row: usize) -> Option<String> {
    if row >= array.len() || array.is_null(row) {
        return None;
    }
    read_as!(
        array.as_any(),
        row,
        Int64Array,
        Int32Array,
        Int16Array,
        Int8Array,
        UInt64Array,
        UInt32Array,
        UInt16Array,
        UInt8Array,
        Float64Array,
        Float32Array,
        BooleanArray,
        StringArray,
        LargeStringArray,
        StringViewArray
    )
}
