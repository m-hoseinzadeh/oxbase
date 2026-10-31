//! Streaming results.

use std::fmt;

use arrow_array::RecordBatch;
use arrow_schema::{ArrowError, SchemaRef};
use futures::StreamExt;
use futures::stream::BoxStream;
use oxbase_types::{Error, Result};

/// Severity of a server notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NoticeSeverity {
    Debug,
    Info,
    Warning,
}

/// A non-fatal message from the server (PostgreSQL `NOTICE`, MySQL warnings, SQL Server `PRINT`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub severity: NoticeSeverity,
    pub message: String,
    /// Engine code, when present (SQLSTATE, warning number).
    pub code: Option<String>,
}

impl Notice {
    pub fn info(message: impl Into<String>) -> Self {
        Self {
            severity: NoticeSeverity::Info,
            message: message.into(),
            code: None,
        }
    }

    pub fn warning(message: impl Into<String>) -> Self {
        Self {
            severity: NoticeSeverity::Warning,
            message: message.into(),
            code: None,
        }
    }
}

impl fmt::Display for Notice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let level = match self.severity {
            NoticeSeverity::Debug => "debug",
            NoticeSeverity::Info => "info",
            NoticeSeverity::Warning => "warning",
        };
        match &self.code {
            Some(code) => write!(f, "{level} [{code}]: {}", self.message),
            None => write!(f, "{level}: {}", self.message),
        }
    }
}

/// One item of a [`ResultStream`].
#[derive(Debug, Clone)]
pub enum ResultItem {
    Notice(Notice),
    /// Start of a new result set; following batches use this schema.
    Schema(SchemaRef),
    /// Rows of the current result set.
    Batch(RecordBatch),
    /// A statement without a result set finished, changing this many rows.
    RowsAffected(u64),
}

/// The stream returned by `Connection::execute`. `'static` so it can move to another task.
pub type ResultStream = BoxStream<'static, Result<ResultItem>>;

/// Converts an Arrow error into the oxbase error type (as an internal error: drivers should never
/// build malformed batches).
#[allow(
    clippy::needless_pass_by_value,
    reason = "by value so it can be passed straight to `map_err`"
)]
pub fn from_arrow(e: ArrowError) -> Error {
    Error::internal(format!("arrow: {e}"))
}

/// One result set gathered from a stream.
#[derive(Debug, Clone)]
pub struct ResultSet {
    pub schema: SchemaRef,
    pub batches: Vec<RecordBatch>,
}

impl ResultSet {
    pub fn num_rows(&self) -> usize {
        self.batches.iter().map(RecordBatch::num_rows).sum()
    }
}

/// Everything a stream produced, gathered by [`collect`].
#[derive(Debug, Clone, Default)]
pub struct Collected {
    pub result_sets: Vec<ResultSet>,
    /// One entry per statement that reported affected rows, in order.
    pub rows_affected: Vec<u64>,
    pub notices: Vec<Notice>,
}

/// Drains a stream into memory. For tests, small metadata queries and the CLI; the grid pages
/// through results instead.
///
/// Fails with the first error the stream yields, or with an internal error if a batch arrives
/// before any schema or does not match the current schema.
pub async fn collect(mut stream: ResultStream) -> Result<Collected> {
    let mut out = Collected::default();
    while let Some(item) = stream.next().await {
        match item? {
            ResultItem::Notice(n) => out.notices.push(n),
            ResultItem::Schema(schema) => out.result_sets.push(ResultSet {
                schema,
                batches: Vec::new(),
            }),
            ResultItem::Batch(batch) => {
                let set = out
                    .result_sets
                    .last_mut()
                    .ok_or_else(|| Error::internal("driver sent a batch before its schema"))?;
                if batch.schema() != set.schema {
                    return Err(Error::internal(
                        "driver sent a batch with a different schema",
                    ));
                }
                set.batches.push(batch);
            }
            ResultItem::RowsAffected(n) => out.rows_affected.push(n),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use arrow_array::Int32Array;
    use arrow_schema::{DataType, Field, Schema};
    use futures::stream;

    fn schema() -> SchemaRef {
        Arc::new(Schema::new(vec![Field::new("n", DataType::Int32, false)]))
    }

    fn batch(s: &SchemaRef, n: &[i32]) -> RecordBatch {
        RecordBatch::try_new(s.clone(), vec![Arc::new(Int32Array::from(n.to_vec()))]).unwrap()
    }

    fn run<F: Future>(f: F) -> F::Output {
        futures::executor::block_on(f)
    }

    #[test]
    fn collects_multiple_result_sets() {
        let s = schema();
        let items: Vec<Result<ResultItem>> = vec![
            Ok(ResultItem::Notice(Notice::info("hello"))),
            Ok(ResultItem::Schema(s.clone())),
            Ok(ResultItem::Batch(batch(&s, &[1, 2]))),
            Ok(ResultItem::Batch(batch(&s, &[3]))),
            Ok(ResultItem::RowsAffected(5)),
            Ok(ResultItem::Schema(s.clone())),
        ];
        let c = run(collect(stream::iter(items).boxed())).unwrap();
        assert_eq!(c.result_sets.len(), 2);
        assert_eq!(c.result_sets[0].num_rows(), 3);
        assert_eq!(c.result_sets[1].num_rows(), 0);
        assert_eq!(c.rows_affected, [5]);
        assert_eq!(c.notices[0].to_string(), "info: hello");
    }

    #[test]
    fn rejects_protocol_violations_and_propagates_errors() {
        let s = schema();
        let orphan = vec![Ok(ResultItem::Batch(batch(&s, &[1])))];
        assert!(run(collect(stream::iter(orphan).boxed())).is_err());

        let failing = vec![Ok(ResultItem::Schema(s)), Err(Error::Cancelled)];
        let err = run(collect(stream::iter(failing).boxed())).unwrap_err();
        assert!(err.is_cancelled());
    }
}
