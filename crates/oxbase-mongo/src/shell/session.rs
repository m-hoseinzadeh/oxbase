//! The QuickJS side of a shell session (module docs, *Sandbox*): one OS thread per session
//! owning the runtime and its context, statements sent to it over a channel.
//!
//! The context has only the language built-ins a script needs (`Date`, `RegExp`, `JSON`,
//! `Proxy`, `Map` / `Set`, `Promise`, `eval`; no job queue runs, so promises never settle),
//! none of QuickJS's host modules and no module loader; the
//! prelude (`prelude.js`) adds the shell API on top of the single host function of
//! [`super::host`]. Limits: `Runtime::set_memory_limit`, `set_max_stack_size`, and an
//! interrupt handler that stops the script when the statement's token fires or its CPU time
//! (wall time minus time spent waiting for the server) passes the limit.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use oxbase_driver::{CancellationToken, Notice};
use oxbase_types::{Error, Result};
use rquickjs::context::EvalOptions;
use rquickjs::context::intrinsic::{
    Date, Eval, Json as JsonIntrinsic, MapSet, Promise, Proxy, RegExp, RegExpCompiler,
};
use rquickjs::{Coerced, Context, Ctx, FromJs, Function, Runtime, Value};

use super::ShellLimits;
use super::host::{self, State, StatementEnv};

/// The prelude: the shell API in JavaScript.
const PRELUDE: &str = include_str!("prelude.js");
/// The file name statements are evaluated under (error positions name it).
const SCRIPT_NAME: &str = "shell";
/// Stack of the session thread (QuickJS's own limit, [`ShellLimits::stack_bytes`], is
/// smaller, so a deep recursion is a JavaScript error, never a crash).
const THREAD_STACK_BYTES: usize = 16 * 1024 * 1024;

/// Why the interrupt handler stopped a script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    Cancelled,
    TimeLimit,
}

/// The CPU clock and stop reason of the running statement, shared with the interrupt handler
/// and the host.
pub(super) struct Watch {
    cancel: RefCell<Option<CancellationToken>>,
    started: Cell<Instant>,
    waited: Cell<Duration>,
    limit: Option<Duration>,
    stop: Cell<Option<Stop>>,
}

impl Watch {
    fn new(limit: Option<Duration>) -> Self {
        Self {
            cancel: RefCell::new(None),
            started: Cell::new(Instant::now()),
            waited: Cell::new(Duration::ZERO),
            limit,
            stop: Cell::new(None),
        }
    }

    fn begin(&self, cancel: CancellationToken) {
        *self.cancel.borrow_mut() = Some(cancel);
        self.started.set(Instant::now());
        self.waited.set(Duration::ZERO);
        self.stop.set(None);
    }

    fn end(&self) {
        *self.cancel.borrow_mut() = None;
        self.stop.set(None);
    }

    /// Time spent waiting for the server (not CPU time).
    pub(super) fn waited(&self, d: Duration) {
        self.waited.set(self.waited.get() + d);
    }

    /// The interrupt handler: `true` stops the script.
    fn should_stop(&self) -> bool {
        if self.stop.get().is_some() {
            return true;
        }
        let Ok(cancel) = self.cancel.try_borrow() else {
            return false;
        };
        let Some(cancel) = cancel.as_ref() else {
            return false;
        };
        if cancel.is_cancelled() {
            self.stop.set(Some(Stop::Cancelled));
            return true;
        }
        if let Some(limit) = self.limit {
            let cpu = self
                .started
                .get()
                .elapsed()
                .saturating_sub(self.waited.get());
            if cpu > limit {
                self.stop.set(Some(Stop::TimeLimit));
                return true;
            }
        }
        false
    }
}

/// What one statement produced.
pub(super) struct Outcome {
    /// `print` / `printjson` output, in order.
    pub(super) notices: Vec<Notice>,
    /// The statement's value as described by the prelude (JSON), or its error.
    pub(super) result: Result<String>,
    /// The database `db = db.getSiblingDB(x)` switched to.
    pub(super) switched: Option<String>,
}

/// One statement for the session thread.
pub(super) struct Job {
    pub(super) source: String,
    pub(super) database: String,
    pub(super) env: StatementEnv,
    pub(super) reply: tokio::sync::oneshot::Sender<Outcome>,
}

/// The channel to a session thread.
pub(super) struct Worker {
    jobs: mpsc::Sender<Job>,
}

impl Worker {
    pub(super) fn start(limits: ShellLimits) -> Result<Self> {
        let (jobs, receiver) = mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("oxbase-mongo-shell".into())
            .stack_size(THREAD_STACK_BYTES)
            .spawn(move || serve(limits, &receiver))
            .map_err(|e| Error::internal(format!("cannot start the shell thread: {e}")))?;
        Ok(Self { jobs })
    }

    /// Queues `job`; gives it back when the thread is gone.
    pub(super) fn send(&self, job: Job) -> std::result::Result<(), Job> {
        self.jobs.send(job).map_err(|e| e.0)
    }
}

/// The session thread: runs jobs until the session (the sender) is dropped.
fn serve(limits: ShellLimits, jobs: &mpsc::Receiver<Job>) {
    let mut engine: Option<Engine> = None;
    while let Ok(job) = jobs.recv() {
        if engine.is_none() {
            match Engine::new(limits) {
                Ok(e) => engine = Some(e),
                Err(e) => {
                    let _ = job.reply.send(Outcome {
                        notices: Vec::new(),
                        result: Err(e),
                        switched: None,
                    });
                    continue;
                }
            }
        }
        let Some(current) = engine.as_ref() else {
            continue;
        };
        let outcome = current.run(&job.source, &job.database, job.env);
        if current.broken.get() {
            // Out of memory: start over with a fresh runtime on the next statement.
            engine = None;
        }
        let _ = job.reply.send(outcome);
    }
}

/// A QuickJS runtime with the prelude loaded.
struct Engine {
    // Field order is drop order: the context before its runtime.
    context: Context,
    runtime: Runtime,
    state: Rc<RefCell<State>>,
    watch: Rc<Watch>,
    limits: ShellLimits,
    /// The runtime ran out of memory and is replaced before the next statement.
    broken: Cell<bool>,
}

impl Engine {
    fn new(limits: ShellLimits) -> Result<Self> {
        let setup = |e: rquickjs::Error| Error::internal(format!("the shell could not start: {e}"));
        let runtime = Runtime::new().map_err(setup)?;
        if limits.memory_bytes > 0 {
            runtime.set_memory_limit(limits.memory_bytes);
        }
        runtime.set_max_stack_size(limits.stack_bytes);
        let watch = Rc::new(Watch::new(limits.time_limit));
        let handler_watch = Rc::clone(&watch);
        runtime.set_interrupt_handler(Some(Box::new(move || handler_watch.should_stop())));
        let context = Context::custom::<(
            Date,
            Eval,
            RegExpCompiler,
            RegExp,
            JsonIntrinsic,
            Proxy,
            MapSet,
            Promise,
        )>(&runtime)
        .map_err(setup)?;
        let state = Rc::new(RefCell::new(State::default()));
        let loaded = context.with(|ctx| -> std::result::Result<(), String> {
            let host_state = Rc::clone(&state);
            let host_watch = Rc::clone(&watch);
            let host = Function::new(ctx.clone(), move |op: String, payload: String| {
                host::call(&host_state, &host_watch, &op, &payload)
            })
            .map_err(|e| e.to_string())?;
            ctx.globals()
                .set("__oxbase_host", host)
                .map_err(|e| e.to_string())?;
            let mut options = EvalOptions::default();
            options.filename = Some("oxbase-prelude".into());
            options.strict = false;
            ctx.eval_with_options::<(), _>(PRELUDE, options)
                .map_err(|e| describe_setup_error(&ctx, e))
        });
        loaded.map_err(|e| Error::internal(format!("the shell prelude failed: {e}")))?;
        Ok(Self {
            context,
            runtime,
            state,
            watch,
            limits,
            broken: Cell::new(false),
        })
    }

    fn run(&self, source: &str, database: &str, env: StatementEnv) -> Outcome {
        self.watch.begin(env.cancel.clone());
        self.state.borrow_mut().begin(env);
        let result = self.context.with(|ctx| self.eval(&ctx, source, database));
        let leftovers = self.state.borrow_mut().end();
        self.watch.end();
        self.runtime.run_gc();
        Outcome {
            notices: leftovers.notices,
            result,
            switched: leftovers.switched,
        }
    }

    fn eval(&self, ctx: &Ctx<'_>, source: &str, database: &str) -> Result<String> {
        let begin: Function<'_> = ctx
            .globals()
            .get("__oxbase_begin")
            .map_err(|e| self.js_error(ctx, e))?;
        begin
            .call::<_, ()>((database,))
            .map_err(|e| self.js_error(ctx, e))?;
        let mut options = EvalOptions::default();
        options.strict = false;
        options.backtrace_barrier = true;
        options.filename = Some(SCRIPT_NAME.into());
        let value: Value<'_> = ctx
            .eval_with_options(source.to_owned(), options)
            .map_err(|e| self.js_error(ctx, e))?;
        let describe: Function<'_> = ctx
            .globals()
            .get("__oxbase_describe")
            .map_err(|e| self.js_error(ctx, e))?;
        describe
            .call::<_, String>((value,))
            .map_err(|e| self.js_error(ctx, e))
    }

    fn js_error(&self, ctx: &Ctx<'_>, e: rquickjs::Error) -> Error {
        match self.watch.stop.get() {
            Some(Stop::Cancelled) => {
                let _ = ctx.catch();
                return Error::Cancelled;
            }
            Some(Stop::TimeLimit) => {
                let _ = ctx.catch();
                let ms = self.limits.time_limit.map_or(0, |d| d.as_millis());
                return Error::timeout(format!(
                    "the script ran longer than the shell's CPU time limit of {ms} ms"
                ));
            }
            None => {}
        }
        match e {
            rquickjs::Error::Exception => {
                let error = exception_error(ctx, &self.state.borrow());
                error.unwrap_or_else(|| self.out_of_memory())
            }
            rquickjs::Error::Allocation => self.out_of_memory(),
            other => Error::internal(format!("shell: {other}")),
        }
    }

    fn out_of_memory(&self) -> Error {
        self.broken.set(true);
        let mib = self.limits.memory_bytes / (1024 * 1024);
        Error::sql(
            Some("OutOfMemory"),
            format!(
                "the script used more than the shell's memory limit of {mib} MiB; the shell \
                 session was reset (its variables are gone)"
            ),
        )
    }
}

/// The error of the pending exception; `None` when it is QuickJS's out-of-memory error.
fn exception_error(ctx: &Ctx<'_>, state: &State) -> Option<Error> {
    let value = ctx.catch();
    let Some(object) = value.as_object() else {
        if value.is_null() || value.is_undefined() {
            // QuickJS throws `null` when it cannot even allocate the error object.
            return None;
        }
        let text = Coerced::<String>::from_js(ctx, value)
            .map(|c| c.0)
            .unwrap_or_default();
        return Some(Error::sql(
            Some("Error"),
            format!("uncaught exception: {text}"),
        ));
    };
    let text = |key: &str| -> Option<String> {
        object
            .get::<_, Option<Coerced<String>>>(key)
            .ok()
            .flatten()
            .map(|c| c.0)
    };
    let message = text("message").unwrap_or_default();
    let name = text("name").unwrap_or_else(|| "Error".to_owned());
    if name == "InternalError" && message.contains("out of memory") {
        return None;
    }
    let line = text("stack").as_deref().and_then(line_of);
    if let Some(id) = object.get::<_, Option<i32>>("oxbaseErrorId").ok().flatten()
        && let Some(original) = usize::try_from(id).ok().and_then(|i| state.error(i))
    {
        return Some(match original.clone() {
            Error::Sql { code, message } => Error::Sql {
                code,
                message: with_line(&message, line),
            },
            other => other,
        });
    }
    let code = text("codeName").filter(|c| !c.is_empty()).unwrap_or(name);
    Some(Error::sql(Some(code), with_line(&message, line)))
}

fn with_line(message: &str, line: Option<u32>) -> String {
    match line {
        Some(n) => format!("{message} (line {n})"),
        None => message.to_owned(),
    }
}

/// The statement line of the first stack frame in the statement (`shell:<line>:<column>`).
fn line_of(stack: &str) -> Option<u32> {
    let mut rest = stack;
    while let Some(at) = rest.find("shell:") {
        let before = rest[..at].chars().last();
        let after = &rest[at + "shell:".len()..];
        if matches!(before, None | Some('(' | ' ' | '\t')) {
            let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
            if let Ok(n) = digits.parse() {
                return Some(n);
            }
        }
        rest = after;
    }
    None
}

/// The message of an exception thrown while loading the prelude.
fn describe_setup_error(ctx: &Ctx<'_>, e: rquickjs::Error) -> String {
    if !matches!(e, rquickjs::Error::Exception) {
        return e.to_string();
    }
    let value = ctx.catch();
    value
        .as_object()
        .map(|o| {
            let get = |k: &str| {
                o.get::<_, Option<Coerced<String>>>(k)
                    .ok()
                    .flatten()
                    .map(|c| c.0)
                    .unwrap_or_default()
            };
            format!("{}: {} {}", get("name"), get("message"), get("stack"))
        })
        .unwrap_or_else(|| "exception".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stack_lines() {
        assert_eq!(line_of("    at <eval> (shell:3:9)\n"), Some(3));
        assert_eq!(line_of("    at shell:12:1\n"), Some(12));
        assert_eq!(
            line_of("    at find (oxbase-prelude:400:5)\n    at <eval> (shell:2:4)\n"),
            Some(2)
        );
        assert_eq!(line_of("    at f (myshell:3:1)\n"), None);
        assert_eq!(line_of(""), None);
    }
}
