//! Running JSON statements (driver module docs, *Statements*, *Cancel*, *Streaming*): one task
//! per statement reads the cursor and feeds Arrow batches into a bounded channel that backs the
//! [`ResultStream`]; the token stops it, kills the server operation by its comment, and ends
//! the stream with one `Cancelled`.

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

async fn run(ctx: &RunContext, statement: Statement, token: &CancellationToken, tx: &Sender) {
    let mut work = Box::pin(execute(ctx, statement, tx));
    let outcome = tokio::select! {
        biased;
        () = token.cancelled() => None,
        r = &mut work => Some(r),
    };
    match outcome {
        Some(Ok(()) | Err(Halt::Gone)) => {}
        Some(Err(Halt::Failed(e))) => {
            let _ = tx.send(Err(e)).await;
        }
        None => {
            // Kill the server operation while its future (and cursor) is still alive, then let
            // the cursor drop (the driver sends killCursors for an open cursor).
            let filter = Bson::String(ctx.comment.clone());
            if let Err(e) = tokio::time::timeout(KILL_TIMEOUT, kill_ops(&ctx.client, filter)).await
            {
                tracing::debug!(error = %e, "MongoDB cancel: killOp did not finish in time");
            }
            drop(work);
            let _ = tx.send(Err(Error::Cancelled)).await;
        }
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
            if !command.contains_key("comment") {
                command.insert("comment", comment);
            }
            let reply = ctx
                .client
                .database(&database)
                .run_command(command)
                .await
                .map_err(map)?;
            let layout = Layout::infer(std::slice::from_ref(&reply), None);
            send(tx, ResultItem::Schema(layout.schema())).await?;
            let (batch, _) = layout.batch(&[reply])?;
            send(tx, ResultItem::Batch(batch)).await
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
}
