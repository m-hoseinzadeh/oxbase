//! Import into collections (mongo stream): the MongoDB [`BulkLoader`]. Contract:
//! `docs/m7-contracts.md` §5.3.
//!
//! - Each row of a load batch becomes one document: one field per load column, in column
//!   order; JSON-typed columns (`DataType::Json`) are parsed and stored as embedded documents
//!   or arrays; NULL cells are stored as BSON null (an empty column is kept, not dropped).
//! - `load` sends `insertMany` (`ordered: true`) with at most [`BulkLimits::rows_per_statement`]
//!   documents and about 16 MiB per command; a failure names the row.
//! - Not transactional ([`BulkLoader::transactional`] is `false`): the import service loads
//!   without a transaction and reports the batches written before a failure or cancel.
//! - Mode *create*: [`BulkLoader::create_target`] runs `create` for the collection (an
//!   existing collection is `InvalidInput`, like a table that exists). *Append* inserts into
//!   an existing collection. *Truncate*, *delete all* and *replace* need statements MongoDB
//!   does not have; the import service refuses them for a loader without a transaction in M7
//!   (`Unsupported` in the plan), so a collection is never emptied by an import.
//!
//! # Values
//!
//! Cells are read with `oxbase_dialect::bind_value` (decimal text as decimals, JSON text as
//! JSON) and become BSON like this:
//!
//! | Load value | BSON |
//! |---|---|
//! | NULL | null |
//! | bool | bool |
//! | integer | int32 for a column of at most 32 bits when it fits, else int64 (an unsigned value past `i64` is a decimal) |
//! | float | double |
//! | decimal | decimal128 (`InvalidInput` past its 34 digits) |
//! | text | string; for a column typed `objectId` (`DataType::Other`) 24 hex digits are an ObjectId |
//! | bytes | binary, subtype 0 |
//! | date | date at 00:00 UTC |
//! | time | string `HH:MM:SS[.f]` (BSON has no time of day) |
//! | timestamp, timestamp with zone | date (UTC milliseconds) |
//! | UUID | binary, subtype 4 |
//! | JSON | Extended JSON (relaxed or canonical), so `{"$oid": ...}` and `{"$date": ...}` keep their types |
//!
//! `insertMany` goes out in commands of at most [`DOCUMENTS_PER_INSERT`] documents (the
//! `mongodb` crate splits a command further at the server's message size). The token is
//! checked before each command; a cancel while one runs kills it by its comment
//! (`Cancelled`). With `ordered: true` the documents before a failing one are written: the
//! error says which document of the call failed.

use arrow_array::RecordBatch;
use async_trait::async_trait;
use bson::oid::ObjectId;
use bson::spec::BinarySubtype;
use bson::{Binary, Bson, DateTime, Decimal128, Document};
use mongodb::error::ErrorKind;
use oxbase_dialect::bulk::bind_value;
use oxbase_dialect::{BulkLimits, BulkLoader, BulkMethod, LoadColumn, LoadSession, LoadTarget};
use oxbase_driver::CancellationToken;
use oxbase_types::{DataType, Error, Result, ServerVersion, Value};

use super::fetch::{check, database_of, guarded, map, mongo};

/// Documents per `insertMany`.
pub const DOCUMENTS_PER_INSERT: usize = 1000;

/// Server error code: the collection already exists.
const NAMESPACE_EXISTS: i32 = 48;

/// The MongoDB import loader.
#[derive(Debug, Clone, Copy, Default)]
pub struct MongoBulkLoader;

#[async_trait]
impl BulkLoader for MongoBulkLoader {
    fn method(&self, version: &ServerVersion) -> BulkMethod {
        let _ = version;
        BulkMethod::DocumentInsert
    }

    fn limits(&self, version: &ServerVersion) -> BulkLimits {
        let _ = version;
        BulkLimits {
            max_params: usize::MAX,
            max_rows_per_statement: DOCUMENTS_PER_INSERT,
            max_statement_bytes: Some(16 * 1024 * 1024),
            rows_per_call: DOCUMENTS_PER_INSERT,
        }
    }

    fn transactional(&self) -> bool {
        false
    }

    async fn create_target(
        &self,
        session: LoadSession<'_>,
        target: &LoadTarget<'_>,
        cancel: CancellationToken,
    ) -> Option<Result<()>> {
        Some(create_collection(session, target, &cancel).await)
    }

    async fn load(
        &self,
        session: LoadSession<'_>,
        target: &LoadTarget<'_>,
        batch: &RecordBatch,
        cancel: CancellationToken,
    ) -> Result<u64> {
        check(&cancel)?;
        target.check_batch(batch)?;
        let conn = mongo(session.connection)?;
        let db = database_of(conn, target.table)?;
        let coll = conn
            .client()
            .database(&db)
            .collection::<Document>(&target.table.name);
        let rows = batch.num_rows();
        let mut written = 0u64;
        let mut start = 0;
        while start < rows {
            check(&cancel)?;
            let end = (start + DOCUMENTS_PER_INSERT).min(rows);
            let documents = (start..end)
                .map(|row| row_document(batch, target.columns, row))
                .collect::<Result<Vec<_>>>()?;
            let comment = conn.next_comment();
            let tag = comment.clone();
            let call = coll.clone();
            guarded(conn, &comment, &cancel, async move {
                call.insert_many(documents)
                    .ordered(true)
                    .comment(tag)
                    .await
                    .map_err(|e| insert_error(&e, start))
            })
            .await?;
            written += u64::try_from(end - start).unwrap_or(u64::MAX);
            start = end;
        }
        Ok(written)
    }
}

async fn create_collection(
    session: LoadSession<'_>,
    target: &LoadTarget<'_>,
    cancel: &CancellationToken,
) -> Result<()> {
    check(cancel)?;
    let conn = mongo(session.connection)?;
    let db = database_of(conn, target.table)?;
    let name = target.table.name.clone();
    if name.is_empty() {
        return Err(Error::invalid_input("the new collection has no name"));
    }
    let comment = conn.next_comment();
    let database = conn.client().database(&db);
    let shown = format!("{db}.{name}");
    guarded(conn, &comment, cancel, async move {
        database.create_collection(name).await.map_err(|e| {
            if let ErrorKind::Command(c) = e.kind.as_ref()
                && c.code == NAMESPACE_EXISTS
            {
                Error::invalid_input(format!("collection {shown} already exists"))
            } else {
                map(&e)
            }
        })
    })
    .await
}

/// The error of an `insertMany` call whose documents start at row `first` of the load batch.
fn insert_error(e: &mongodb::error::Error, first: usize) -> Error {
    if let ErrorKind::InsertMany(failure) = e.kind.as_ref()
        && let Some(w) = failure.write_errors.as_ref().and_then(|w| w.first())
    {
        let code = w
            .code_name
            .clone()
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| w.code.to_string());
        let row = first + w.index + 1;
        let before = if row > 1 {
            format!(
                "; the {} rows before it in this batch were written",
                row - 1
            )
        } else {
            String::new()
        };
        return Error::sql(
            Some(code),
            format!("row {row} of the batch: {}{before}", w.message),
        );
    }
    map(e)
}

/// Row `row` of `batch` as a document (see the module docs).
fn row_document(batch: &RecordBatch, columns: &[LoadColumn], row: usize) -> Result<Document> {
    let mut doc = Document::new();
    for (index, column) in columns.iter().enumerate() {
        let value = bind_value(batch.column(index).as_ref(), row, column)?;
        doc.insert(column.name.clone(), to_bson(value, column)?);
    }
    Ok(doc)
}

/// One load value as BSON for `column` (see the module docs).
fn to_bson(value: Value, column: &LoadColumn) -> Result<Bson> {
    let invalid = |what: String| Error::invalid_input(format!("column '{}': {what}", column.name));
    Ok(match value {
        Value::Null => Bson::Null,
        Value::Bool(b) => Bson::Boolean(b),
        Value::Int(n) => match column.data_type {
            DataType::Int { bits, .. } if bits <= 32 => {
                i32::try_from(n).map_or(Bson::Int64(n), Bson::Int32)
            }
            _ => Bson::Int64(n),
        },
        Value::UInt(n) => match i64::try_from(n) {
            Ok(n) => Bson::Int64(n),
            Err(_) => Bson::Decimal128(decimal(&n.to_string()).map_err(invalid)?),
        },
        Value::Float(f) => Bson::Double(f),
        Value::Decimal(text) => Bson::Decimal128(decimal(&text).map_err(invalid)?),
        Value::Text(text) => match &column.data_type {
            DataType::Other { name } if name == "objectId" => {
                ObjectId::parse_str(&text).map_or(Bson::String(text), Bson::ObjectId)
            }
            _ => Bson::String(text),
        },
        Value::Bytes(bytes) => Bson::Binary(Binary {
            subtype: BinarySubtype::Generic,
            bytes,
        }),
        Value::Date(d) => Bson::DateTime(DateTime::from_millis(
            d.and_hms_opt(0, 0, 0)
                .unwrap_or_default()
                .and_utc()
                .timestamp_millis(),
        )),
        Value::Time(t) => Bson::String(t.to_string()),
        Value::Timestamp(t) => {
            Bson::DateTime(DateTime::from_millis(t.and_utc().timestamp_millis()))
        }
        Value::TimestampTz(t) => Bson::DateTime(DateTime::from_millis(t.timestamp_millis())),
        Value::Uuid(u) => Bson::Binary(Binary {
            subtype: BinarySubtype::Uuid,
            bytes: u.as_bytes().to_vec(),
        }),
        Value::Json(json) => {
            Bson::try_from(json).map_err(|e| invalid(format!("invalid Extended JSON: {e}")))?
        }
    })
}

fn decimal(text: &str) -> std::result::Result<Decimal128, String> {
    text.trim()
        .parse::<Decimal128>()
        .map_err(|e| format!("'{text}' is not a decimal128: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use arrow_array::{ArrayRef, Int64Array, StringArray};
    use oxbase_types::ObjectRef;

    fn column(name: &str, data_type: DataType) -> LoadColumn {
        LoadColumn::new(name, data_type)
    }

    #[test]
    fn values_become_typed_bson() {
        let int32 = column("n", DataType::INT32);
        let int64 = column("n", DataType::INT64);
        assert_eq!(to_bson(Value::Int(5), &int32).unwrap(), Bson::Int32(5));
        assert_eq!(
            to_bson(Value::Int(5_000_000_000), &int32).unwrap(),
            Bson::Int64(5_000_000_000)
        );
        assert_eq!(to_bson(Value::Int(5), &int64).unwrap(), Bson::Int64(5));
        assert_eq!(
            to_bson(Value::UInt(u64::MAX), &int64).unwrap(),
            Bson::Decimal128("18446744073709551615".parse().unwrap())
        );
        let dec = column(
            "d",
            DataType::Decimal {
                precision: None,
                scale: None,
            },
        );
        assert_eq!(
            to_bson(Value::Decimal("12.50".into()), &dec).unwrap(),
            Bson::Decimal128("12.50".parse().unwrap())
        );
        assert!(to_bson(Value::Decimal("abc".into()), &dec).is_err());
        let oid = column(
            "_id",
            DataType::Other {
                name: "objectId".into(),
            },
        );
        assert_eq!(
            to_bson(Value::Text("65a000000000000000000001".into()), &oid).unwrap(),
            Bson::ObjectId(ObjectId::parse_str("65a000000000000000000001").unwrap())
        );
        assert_eq!(
            to_bson(Value::Text("not hex".into()), &oid).unwrap(),
            Bson::String("not hex".into())
        );
        let json = column("j", DataType::Json { binary: true });
        assert_eq!(
            to_bson(
                Value::Json(serde_json::json!({"a": [1, {"$numberLong": "2"}]})),
                &json
            )
            .unwrap(),
            Bson::Document(bson::doc! {"a": [1_i32, 2_i64]})
        );
        assert_eq!(to_bson(Value::Null, &json).unwrap(), Bson::Null);
        let date = chrono::NaiveDate::from_ymd_opt(2026, 1, 2).unwrap();
        assert_eq!(
            to_bson(Value::Date(date), &column("d", DataType::Date)).unwrap(),
            Bson::DateTime(DateTime::from_millis(1_767_312_000_000))
        );
    }

    #[test]
    fn rows_become_documents_in_column_order() {
        let columns = vec![
            column("z", DataType::INT64),
            column("a", DataType::Json { binary: true }),
        ];
        let table = ObjectRef::table(Some("d"), None, "c");
        let version = ServerVersion::new(7, 0, 45);
        let target = LoadTarget {
            table: &table,
            columns: &columns,
            version: &version,
        };
        let batch = RecordBatch::try_new(
            target.schema(),
            vec![
                Arc::new(Int64Array::from(vec![Some(1), None])) as ArrayRef,
                Arc::new(StringArray::from(vec![Some(r#"{"x": true}"#), None])) as ArrayRef,
            ],
        )
        .unwrap();
        let first = row_document(&batch, &columns, 0).unwrap();
        assert_eq!(first, bson::doc! {"z": 1_i64, "a": {"x": true}});
        let keys: Vec<&String> = first.keys().collect();
        assert_eq!(keys, ["z", "a"]);
        let second = row_document(&batch, &columns, 1).unwrap();
        assert_eq!(
            second,
            bson::doc! {"z": null, "a": null},
            "NULL cells stay as null"
        );
        assert!(!MongoBulkLoader.transactional());
        assert_eq!(
            MongoBulkLoader.limits(&version).rows_per_statement(2),
            DOCUMENTS_PER_INSERT
        );
    }
}
