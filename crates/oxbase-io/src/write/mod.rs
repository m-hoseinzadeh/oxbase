//! The writers behind [`crate::create_writer`] (write stream): one module per format, plus the
//! pieces they share ([`cells`]: batch normalization, value text forms, JSON value encoders).
//!
//! Every writer owns a [`SinkFile`] (through [`Tally`], which publishes the byte count for
//! [`crate::SinkWriter::bytes_written`]), so nothing reaches the destination before `finish`
//! and an abort or a drop deletes the temporary file.

pub(crate) mod cells;
pub(crate) mod csv;
pub(crate) mod json;
pub(crate) mod parquet;
pub(crate) mod sql;
pub(crate) mod xlsx;

use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use oxbase_types::Error;

use crate::SinkFile;

/// A [`SinkFile`] that mirrors its byte count into a shared counter, so a writer can report
/// progress while the file sits under layers (CSV writer, encoder, `BufWriter`, zip).
pub(crate) struct Tally {
    file: SinkFile,
    count: Arc<AtomicU64>,
}

impl Tally {
    pub(crate) fn new(file: SinkFile) -> Self {
        Self {
            file,
            count: Arc::new(AtomicU64::new(0)),
        }
    }

    /// The shared byte counter.
    pub(crate) fn counter(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.count)
    }

    pub(crate) fn into_file(self) -> SinkFile {
        self.file
    }
}

impl Write for Tally {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.file.write(buf)?;
        self.count
            .store(self.file.bytes_written(), Ordering::Relaxed);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// An I/O error of a writer: `InvalidData` (a character the encoding cannot hold, text that is
/// not UTF-8) is the data's fault, so `InvalidInput`; anything else is `Io`. The messages never
/// name paths (the OS errors of a write do not).
pub(crate) fn io_error(e: &io::Error) -> Error {
    if e.kind() == io::ErrorKind::InvalidData {
        Error::invalid_input(e.to_string())
    } else {
        Error::Io {
            kind: e.kind().to_string(),
            message: e.to_string(),
        }
    }
}

/// The error a writer returns when it is used again after it failed.
pub(crate) fn already_failed() -> Error {
    Error::internal("the file writer already failed; abort it")
}

/// Counts lossy writes of one kind for the report ([`crate::SinkReport::warnings`]).
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Count(u64);

impl Count {
    pub(crate) fn add(&mut self) {
        self.0 += 1;
    }

    /// `message(n)` when anything was counted.
    pub(crate) fn note(self, warnings: &mut Vec<String>, message: impl FnOnce(u64) -> String) {
        if self.0 > 0 {
            warnings.push(message(self.0));
        }
    }
}

/// `"s"` when `n != 1`.
pub(crate) fn plural(n: u64) -> &'static str {
    if n == 1 { "" } else { "s" }
}
