//! Running JSON statements (driver module docs, *Statements*, *Cancel*, *Streaming*): one task
//! per statement reads the cursor and feeds Arrow batches into a bounded channel that backs the
//! [`ResultStream`]; the token stops it, kills the server operation by its comment, and ends
//! the stream with one `Cancelled`. A consumer that drops the stream (`maxRows`, the disk cap)
//! stops it the same way, without the `Cancelled`.
//!
//! Every operation of a run carries the run's comment `oxbase:<connection id>:<n>`; a command
//! request's own `comment` is kept after it (`oxbase:<id>:<n> <the user's comment>`), so the
//! kill filter is the run's comment followed by a space or the end. A command that returns a
//! cursor (`listCollections`, `find`, ...) shows its first batch; the driver then closes the
//! server cursor with `killCursors` (sessionless, as the `mongodb` crate closes its own) and
//! says so in a notice.

use std::pin::pin;
use std::time::Duration;

use bson::{Bson, Document, doc};
use futures::{StreamExt, TryStreamExt};
use mongodb::Client;
use oxbase_driver::{CancellationToken, Notice, ResultItem, ResultStream};
use oxbase_types::{DocumentQuery, Error, Result};
use tokio::sync::mpsc;

use super::convert::Layout;
use super::error::{Redactor, map_error};
use super::statement::{self, BATCH_ROWS, Statement};

/// Items buffered between the statement task and the consumer.
const CHANNEL_CAPACITY: usize = 4;
/// Longest the `$currentOp` + `killOp` round trips of a cancel may take.
pub(crate) const KILL_TIMEOUT: Duration = Duration::from_millis(1500);
/// Wait before the second `$currentOp` when the first found nothing (the operation may still
/// be on its way to the server).
const KILL_RETRY_DELAY: Duration = Duration::from_millis(200);

type Sender = mpsc::Sender<Result<ResultItem>>;

/// Why a statement stopped early.
enum Halt {
    /// The consumer dropped the stream.
    Gone,
    Failed(Error),
}

impl From<Error> for Halt {
    fn from(e: Error) -> Self {
        Self::Failed(e)
    }
}

/// What a statement task needs from the connection.
pub(crate) struct RunContext {
    pub(crate) client: Client,
    pub(crate) redactor: Redactor,
    pub(crate) comment: String,
    pub(crate) sample_size: usize,
}

/// Starts `statement` on a task and returns its stream. `on_end` runs when the task ends
/// (the connection forgets the run).
pub(crate) fn spawn(
    ctx: RunContext,
    statement: Statement,
    token: CancellationToken,
    on_end: impl FnOnce() + Send + 'static,
) -> ResultStream {
    let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
    tokio::spawn(async move {
        run(&ctx, statement, &token, &tx).await;
        on_end();
    });
    futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    })
    .boxed()
}

/// How a statement task ended.
enum Outcome {
    Finished(std::result::Result<(), Halt>),
    Cancelled,
    /// The consumer dropped the stream while the statement was still working.
    Abandoned,
}

async fn run(ctx: &RunContext, statement: Statement, token: &CancellationToken, tx: &Sender) {
    let mut work = Box::pin(execute(ctx, statement, tx));
    let outcome = tokio::select! {
        biased;
        () = token.cancelled() => Outcome::Cancelled,
        () = tx.closed() => Outcome::Abandoned,
        r = &mut work => Outcome::Finished(r),
    };
    let cancelled = match outcome {
        Outcome::Finished(Ok(()) | Err(Halt::Gone)) => return,
        Outcome::Finished(Err(Halt::Failed(e))) => {
            let _ = tx.send(Err(e)).await;
            return;
        }
        Outcome::Cancelled => true,
        Outcome::Abandoned => false,
    };
    // Kill the server operation while its future (and cursor) is still alive, then let the
    // cursor drop (the driver sends killCursors for an open cursor).
    // A cancel looks twice (the operation may still be on its way); a dropped stream once.
    let kill = kill_ops_with_retry(&ctx.client, run_comments(&ctx.comment), cancelled);
    match tokio::time::timeout(KILL_TIMEOUT, kill).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => tracing::debug!(error = %e, "MongoDB cancel: killOp failed"),
        Err(_) => tracing::debug!("MongoDB cancel: killOp did not finish in time"),
    }
    drop(work);
    if cancelled {
        let _ = tx.send(Err(Error::Cancelled)).await;
    }
}

async fn send(tx: &Sender, item: ResultItem) -> std::result::Result<(), Halt> {
    tx.send(Ok(item)).await.map_err(|_| Halt::Gone)
}

async fn execute(
    ctx: &RunContext,
    statement: Statement,
    tx: &Sender,
) -> std::result::Result<(), Halt> {
    let map = |e: mongodb::error::Error| Halt::Failed(map_error(&e, &ctx.redactor));
    let comment = Bson::String(ctx.comment.clone());
    match statement {
        Statement::Command {
            database,
            mut command,
        } => {
            // Replaces the user's comment in place (key order kept), keeping its text.
            let comment = command_comment(&ctx.comment, command.get("comment"));
            command.insert("comment", comment);
            let reply = ctx
                .client
                .database(&database)
                .run_command(command)
                .await
                .map_err(map)?;
            let cursor = close_reply_cursor(ctx, &reply).await;
            let layout = Layout::infer(std::slice::from_ref(&reply), None);
            send(tx, ResultItem::Schema(layout.schema())).await?;
            let (batch, _) = layout.batch(&[reply])?;
            send(tx, ResultItem::Batch(batch)).await?;
            let notice = match cursor {
                ReplyCursor::None => return Ok(()),
                ReplyCursor::Closed => Notice::info(
                    "the command returned a cursor: only its first batch is shown and the server \
                     cursor was closed (a find or aggregate request streams every document)",
                ),
                ReplyCursor::Open => Notice::warning(
                    "the command returned a cursor: only its first batch is shown; the server \
                     cursor could not be closed and stays open until the server times it out \
                     (a find or aggregate request streams every document)",
                ),
            };
            send(tx, ResultItem::Notice(notice)).await
        }
        Statement::Query(query) => {
            let collection = query.collection();
            let database = collection.database_name().ok_or_else(|| {
                Error::invalid_input("the request names no database (collection.catalog)")
            })?;
            let coll = ctx
                .client
                .database(database)
                .collection::<Document>(&collection.name);
            let batch_size = u32::try_from(BATCH_ROWS).unwrap_or(u32::MAX);
            let (cursor, projection) = match query.as_ref() {
                DocumentQuery::Find(f) => {
                    let (filter, mut options) = statement::find_parts(f)?;
                    options.batch_size = Some(batch_size);
                    options.comment = Some(comment);
                    let projection = options.projection.clone();
                    let cursor = coll.find(filter).with_options(options).await.map_err(map)?;
                    (cursor, projection)
                }
                DocumentQuery::Aggregate(a) => {
                    let (pipeline, mut options) = statement::aggregate_parts(a)?;
                    options.batch_size = Some(batch_size);
                    options.comment = Some(comment);
                    let cursor = coll
                        .aggregate(pipeline)
                        .with_options(options)
                        .await
                        .map_err(map)?;
                    (cursor, None)
                }
            };
            let cursor = cursor.map_err(map);
            stream_documents(cursor, projection.as_ref(), ctx.sample_size, tx).await
        }
    }
}

/// The `comment` of a command request: the run's comment, then the user's own comment (its
/// text, or relaxed Extended JSON) after a space.
fn command_comment(ours: &str, user: Option<&Bson>) -> Bson {
    Bson::String(match user {
        None => ours.to_owned(),
        Some(Bson::String(text)) => format!("{ours} {text}"),
        Some(other) => format!("{ours} {}", other.clone().into_relaxed_extjson()),
    })
}

/// The server cursor a command reply left open.
enum ReplyCursor {
    None,
    Closed,
    /// `killCursors` failed (or the reply named no namespace).
    Open,
}

/// When `reply` holds an open cursor (`cursor.id` not 0), closes it with `killCursors`.
async fn close_reply_cursor(ctx: &RunContext, reply: &Document) -> ReplyCursor {
    let Ok(cursor) = reply.get_document("cursor") else {
        return ReplyCursor::None;
    };
    let id = match cursor.get("id") {
        Some(Bson::Int64(id)) => *id,
        Some(Bson::Int32(id)) => i64::from(*id),
        _ => 0,
    };
    if id == 0 {
        return ReplyCursor::None;
    }
    let Some((database, collection)) = cursor.get_str("ns").ok().and_then(|ns| ns.split_once('.'))
    else {
        return ReplyCursor::Open;
    };
    // `killCursors` takes no session (the `mongodb` crate refuses one); the server lets the
    // user who opened the cursor close it from any session.
    let kill = doc! {"killCursors": collection, "cursors": [id]};
    match ctx.client.database(database).run_command(kill).await {
        Ok(reply) if cursor_killed(&reply, id) => ReplyCursor::Closed,
        Ok(_) => ReplyCursor::Open,
        Err(e) => {
            tracing::debug!(
                error = %map_error(&e, &ctx.redactor),
                "MongoDB: killCursors after a command failed"
            );
            ReplyCursor::Open
        }
    }
}

/// Whether a `killCursors` reply lists `id` in `cursorsKilled`.
fn cursor_killed(reply: &Document, id: i64) -> bool {
    reply.get_array("cursorsKilled").is_ok_and(|killed| {
        killed.iter().any(|k| match k {
            Bson::Int64(k) => *k == id,
            Bson::Int32(k) => i64::from(*k) == id,
            _ => false,
        })
    })
}

/// Streams documents as one result set: the first `sample_size` infer the layout, then
/// batches of [`BATCH_ROWS`]; a warning notice counts values that did not fit their column.
async fn stream_documents<S>(
    cursor: S,
    projection: Option<&Document>,
    sample_size: usize,
    tx: &Sender,
) -> std::result::Result<(), Halt>
where
    S: futures::Stream<Item = std::result::Result<Document, Halt>>,
{
    let mut cursor = pin!(cursor);
    let mut sample = Vec::new();
    while sample.len() < sample_size.max(1) {
        match cursor.next().await {
            Some(doc) => sample.push(doc?),
            None => break,
        }
    }
    let layout = Layout::infer(&sample, projection);
    send(tx, ResultItem::Schema(layout.schema())).await?;
    let mut misfits = 0u64;
    for chunk in sample.chunks(BATCH_ROWS) {
        let (batch, n) = layout.batch(chunk)?;
        misfits += n;
        send(tx, ResultItem::Batch(batch)).await?;
    }
    drop(sample);
    let mut buffer = Vec::with_capacity(BATCH_ROWS);
    while let Some(doc) = cursor.next().await {
        buffer.push(doc?);
        if buffer.len() == BATCH_ROWS {
            let (batch, n) = layout.batch(&buffer)?;
            misfits += n;
            buffer.clear();
            send(tx, ResultItem::Batch(batch)).await?;
        }
    }
    if !buffer.is_empty() {
        let (batch, n) = layout.batch(&buffer)?;
        misfits += n;
        send(tx, ResultItem::Batch(batch)).await?;
    }
    if misfits > 0 {
        send(
            tx,
            ResultItem::Notice(Notice::warning(format!(
                "{misfits} value(s) did not match their column's type (inferred from the first \
                 {} documents) and are shown as NULL there; the document column has them",
                sample_size.max(1)
            ))),
        )
        .await?;
    }
    Ok(())
}

/// Kills this user's operations whose comment matches `filter` (a string, or a `$regex`
/// document), with `$currentOp` + `killOp` on another pooled connection. Returns how many.
pub(crate) async fn kill_ops(client: &Client, filter: Bson) -> Result<usize> {
    let admin = client.database("admin");
    let pipeline = [
        doc! {"$currentOp": {"allUsers": false, "idleConnections": false}},
        doc! {"$match": {"$or": [
            {"command.comment": filter.clone()},
            {"cursor.originatingCommand.comment": filter.clone()},
            {"originatingCommand.comment": filter},
        ]}},
        doc! {"$project": {"opid": 1}},
    ];
    let redactor = Redactor::default();
    let ops: Vec<Document> = admin
        .aggregate(pipeline)
        .await
        .map_err(|e| map_error(&e, &redactor))?
        .try_collect()
        .await
        .map_err(|e| map_error(&e, &redactor))?;
    let mut killed = 0;
    for op in ops {
        let Some(opid) = op.get("opid").cloned() else {
            continue;
        };
        match admin.run_command(doc! {"killOp": 1, "op": opid}).await {
            Ok(_) => killed += 1,
            Err(e) => tracing::debug!(error = %map_error(&e, &redactor), "MongoDB killOp failed"),
        }
    }
    Ok(killed)
}

/// [`kill_ops`], tried once more after [`KILL_RETRY_DELAY`] when it found nothing and `retry`.
async fn kill_ops_with_retry(client: &Client, filter: Bson, retry: bool) -> Result<usize> {
    let killed = kill_ops(client, filter.clone()).await?;
    if killed > 0 || !retry {
        return Ok(killed);
    }
    tokio::time::sleep(KILL_RETRY_DELAY).await;
    kill_ops(client, filter).await
}

/// The `$regex` filter that matches the comments of one run: `comment` alone, or followed by
/// a space and a command request's own comment.
pub(crate) fn run_comments(comment: &str) -> Bson {
    Bson::Document(doc! {"$regex": format!("^{}(?: |$)", regex_escape(comment))})
}

/// `text` with the PCRE metacharacters escaped.
fn regex_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if "\\^$.|?*+()[]{}".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The `$regex` filter that matches every comment of the connection `id`.
pub(crate) fn connection_comments(id: &str) -> Bson {
    Bson::Document(doc! {"$regex": format!("^oxbase:{id}:")})
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::cast::AsArray;
    use arrow_array::types::Int32Type;
    use oxbase_driver::collect;

    #[tokio::test]
    async fn documents_stream_in_batches_with_a_misfit_notice() {
        let docs: Vec<std::result::Result<Document, Halt>> = (0..2500)
            .map(|i: i32| {
                Ok(if i < 1500 {
                    doc! {"_id": i, "v": i}
                } else {
                    doc! {"_id": i, "v": "text"}
                })
            })
            .collect();
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let producer = tokio::spawn(async move {
            let ok = stream_documents(futures::stream::iter(docs), None, 1000, &tx)
                .await
                .is_ok();
            assert!(ok);
        });
        let stream = futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        })
        .boxed();
        let out = collect(stream).await.unwrap();
        producer.await.unwrap();
        assert_eq!(out.result_sets.len(), 1);
        let set = &out.result_sets[0];
        assert_eq!(set.num_rows(), 2500);
        assert_eq!(set.batches.len(), 3);
        let v = set.batches[1].column(1).as_primitive::<Int32Type>();
        assert_eq!(v.value(0), 1000);
        assert_eq!(out.notices.len(), 1);
        assert!(
            out.notices[0].message.starts_with("1000 value(s)"),
            "{}",
            out.notices[0]
        );
    }

    #[test]
    fn connection_comment_filter() {
        assert_eq!(
            connection_comments("ab12"),
            Bson::Document(doc! {"$regex": "^oxbase:ab12:"})
        );
    }

    #[test]
    fn run_comment_filter_matches_the_run_and_a_wrapped_user_comment_only() {
        assert_eq!(
            run_comments("oxbase:ab12:1"),
            Bson::Document(doc! {"$regex": "^oxbase:ab12:1(?: |$)"})
        );
        assert_eq!(regex_escape("a.b(c)$"), "a\\.b\\(c\\)\\$");
    }

    #[test]
    fn command_comments_keep_the_users_comment_after_the_runs() {
        assert_eq!(
            command_comment("oxbase:ab:2", None),
            Bson::String("oxbase:ab:2".into())
        );
        assert_eq!(
            command_comment("oxbase:ab:2", Some(&Bson::String("mine".into()))),
            Bson::String("oxbase:ab:2 mine".into())
        );
        assert_eq!(
            command_comment("oxbase:ab:2", Some(&Bson::Document(doc! {"tag": 1_i32}))),
            Bson::String(r#"oxbase:ab:2 {"tag":1}"#.into())
        );
    }

    #[test]
    fn kill_cursors_replies() {
        assert!(cursor_killed(
            &doc! {"cursorsKilled": [7_i64], "ok": 1.0},
            7
        ));
        assert!(!cursor_killed(
            &doc! {"cursorsKilled": [], "cursorsNotFound": [7_i64]},
            7
        ));
        assert!(!cursor_killed(&doc! {"ok": 1.0}, 7));
    }

    #[tokio::test]
    async fn a_dropped_consumer_closes_the_channel() {
        let (tx, rx) = mpsc::channel::<Result<ResultItem>>(CHANNEL_CAPACITY);
        drop(rx);
        // `run` selects on this: it resolves at once when the stream is gone.
        tokio::time::timeout(Duration::from_secs(1), tx.closed())
            .await
            .unwrap();
    }
}
