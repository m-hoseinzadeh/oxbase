//! The shell commands written without parentheses: `use <db>`, `show dbs | databases |
//! collections | tables | users | roles`, and `it` (not offered: results are pageable). They
//! run straight on the connection's client, without the JavaScript thread.

use bson::{Bson, Document, doc};
use futures::StreamExt;
use oxbase_driver::{CancellationToken, Connection, Notice, ResultItem, ResultStream};
use oxbase_types::{Error, Result};

use super::host::{check_database_name, map_error};
use super::rows::{self, ShowColumn};
use super::split::CommandWord;
use crate::MongoConnection;

pub(super) async fn run(
    connection: &MongoConnection,
    word: CommandWord,
    argument: &str,
    cancel: &CancellationToken,
) -> Result<ResultStream> {
    let items = match word {
        CommandWord::It => {
            return Err(Error::unsupported(
                "`it` is not offered: the result grid pages through every document",
            ));
        }
        CommandWord::Use => {
            check_database_name(argument)?;
            connection.set_current_database(argument);
            vec![Ok(ResultItem::Notice(Notice::info(format!(
                "switched to db {argument}"
            ))))]
        }
        CommandWord::Show => show(connection, argument, cancel).await?,
    };
    Ok(futures::stream::iter(items).boxed())
}

async fn show(
    connection: &MongoConnection,
    what: &str,
    cancel: &CancellationToken,
) -> Result<Vec<Result<ResultItem>>> {
    let database = connection
        .current_database()
        .unwrap_or_else(|| "test".to_owned());
    let (schema, batch) = match what {
        "dbs" | "databases" => {
            let reply = command(connection, "admin", doc! {"listDatabases": 1_i32}, cancel).await?;
            let mut rows: Vec<(String, Option<i64>, Option<bool>)> = reply
                .get_array("databases")
                .map(|list| {
                    list.iter()
                        .filter_map(Bson::as_document)
                        .map(|d| {
                            (
                                d.get_str("name").unwrap_or_default().to_owned(),
                                d.get("sizeOnDisk").and_then(as_i64),
                                d.get_bool("empty").ok(),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            rows.sort_by(|a, b| a.0.cmp(&b.0));
            rows::listing(vec![
                ShowColumn::Text("name", rows.iter().map(|r| r.0.clone()).collect()),
                ShowColumn::Int("sizeOnDisk", rows.iter().map(|r| r.1).collect()),
                ShowColumn::Bool("empty", rows.iter().map(|r| r.2).collect()),
            ])?
        }
        "collections" | "tables" => {
            let client = connection.client().clone();
            let names = cancellable(cancel, async move {
                client
                    .database(&database)
                    .list_collection_names()
                    .await
                    .map_err(|e| map_error(&e))
            })
            .await?;
            let mut names = names;
            names.sort();
            rows::listing(vec![ShowColumn::Text("name", names)])?
        }
        "users" => {
            let reply = command(connection, &database, doc! {"usersInfo": 1_i32}, cancel).await?;
            rows::documents(&documents(&reply, "users"))?
        }
        "roles" => {
            let reply = command(
                connection,
                &database,
                doc! {"rolesInfo": 1_i32, "showBuiltinRoles": true},
                cancel,
            )
            .await?;
            rows::documents(&documents(&reply, "roles"))?
        }
        other => {
            return Err(Error::invalid_input(format!(
                "show {other} is not supported: use show dbs, show collections, show users or \
                 show roles"
            )));
        }
    };
    Ok(vec![
        Ok(ResultItem::Schema(schema)),
        Ok(ResultItem::Batch(batch)),
    ])
}

fn as_i64(value: &Bson) -> Option<i64> {
    match value {
        Bson::Int64(n) => Some(*n),
        Bson::Int32(n) => Some(i64::from(*n)),
        #[allow(
            clippy::cast_possible_truncation,
            reason = "sizes reported as doubles are whole numbers far below 2^63"
        )]
        Bson::Double(n) if n.is_finite() => Some(*n as i64),
        _ => None,
    }
}

fn documents(reply: &Document, key: &str) -> Vec<Document> {
    reply
        .get_array(key)
        .map(|list| list.iter().filter_map(Bson::as_document).cloned().collect())
        .unwrap_or_default()
}

/// `cmd` on `database` with the connection's next operation comment; the token ends it.
async fn command(
    connection: &MongoConnection,
    database: &str,
    mut cmd: Document,
    cancel: &CancellationToken,
) -> Result<Document> {
    cmd.insert("comment", connection.next_comment());
    let client = connection.client().clone();
    let database = database.to_owned();
    cancellable(cancel, async move {
        client
            .database(&database)
            .run_command(cmd)
            .await
            .map_err(|e| map_error(&e))
    })
    .await
}

async fn cancellable<T>(
    cancel: &CancellationToken,
    work: impl Future<Output = Result<T>>,
) -> Result<T> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(Error::Cancelled),
        r = work => r,
    }
}
