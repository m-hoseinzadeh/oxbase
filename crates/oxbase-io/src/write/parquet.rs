//! The Parquet writer (rules: [`crate::parquet_file`] module docs, *Writing*).

use ::parquet::arrow::ArrowWriter;
use ::parquet::errors::ParquetError;
use ::parquet::file::properties::WriterProperties;
use arrow_array::RecordBatch;
use arrow_schema::SchemaRef;
use oxbase_types::{Error, Result};

use super::{Tally, already_failed};
use crate::{ParquetWriteOptions, SinkFile, SinkReport, SinkWriter};

struct ParquetSink {
    writer: ArrowWriter<Tally>,
    rows: u64,
    failed: bool,
}

fn parquet_error(e: ParquetError) -> Error {
    match e {
        ParquetError::NYI(m) => Error::unsupported(format!("Parquet: {m}")),
        ParquetError::External(inner) => match inner.downcast::<std::io::Error>() {
            Ok(io) => super::io_error(&io),
            Err(other) => Error::internal(format!("Parquet: {other}")),
        },
        other => Error::internal(format!("Parquet: {other}")),
    }
}

/// The Parquet writer over `file`.
pub(crate) fn writer(
    file: SinkFile,
    schema: SchemaRef,
    options: &ParquetWriteOptions,
) -> Result<Box<dyn SinkWriter>> {
    let properties = WriterProperties::builder()
        .set_compression(options.compression.to_parquet()?)
        .set_max_row_group_row_count(Some(options.row_group_rows.max(1)))
        .set_created_by(format!("oxbase {}", env!("CARGO_PKG_VERSION")))
        .build();
    let writer =
        ArrowWriter::try_new(Tally::new(file), schema, Some(properties)).map_err(parquet_error)?;
    Ok(Box::new(ParquetSink {
        writer,
        rows: 0,
        failed: false,
    }))
}

impl SinkWriter for ParquetSink {
    fn write(&mut self, batch: &RecordBatch) -> Result<()> {
        if self.failed {
            return Err(already_failed());
        }
        let result = self.writer.write(batch).map_err(parquet_error);
        match result {
            Ok(()) => {
                self.rows += u64::try_from(batch.num_rows()).unwrap_or(u64::MAX);
                Ok(())
            }
            Err(e) => {
                self.failed = true;
                Err(e)
            }
        }
    }

    fn rows_written(&self) -> u64 {
        self.rows
    }

    fn bytes_written(&self) -> u64 {
        u64::try_from(self.writer.bytes_written()).unwrap_or(u64::MAX)
    }

    fn finish(self: Box<Self>) -> Result<SinkReport> {
        let this = *self;
        if this.failed {
            return Err(already_failed());
        }
        // Flushes the last row group and writes the footer.
        let tally = this.writer.into_inner().map_err(parquet_error)?;
        let bytes = tally.into_file().commit()?;
        Ok(SinkReport {
            rows: this.rows,
            bytes,
            sheets: Vec::new(),
            warnings: Vec::new(),
        })
    }

    fn abort(self: Box<Self>) {
        drop(self);
    }
}
