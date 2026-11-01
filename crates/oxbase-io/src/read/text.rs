//! Opening a text file (CSV, JSON): encoding resolved from the first 64 KiB, BOM removed, a
//! shared counter of the file bytes consumed (the progress position).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use oxbase_types::Result;

use super::io_error;
use crate::TextEncoding;
use crate::encoding::{CountingReader, DecodingReader, resolve};

/// Bytes sampled to resolve [`TextEncoding::Auto`].
const SAMPLE_BYTES: u64 = 64 * 1024;

/// A text file ready to read as UTF-8.
pub(crate) struct TextInput {
    pub reader: DecodingReader<CountingReader<File>>,
    /// File bytes consumed so far (runs ahead of the parser by the decoder's buffer).
    pub counter: Arc<AtomicU64>,
    /// The encoding used (`Auto` resolved).
    pub encoding: TextEncoding,
    /// A byte order mark was removed.
    pub bom: bool,
    /// File size.
    pub total_bytes: u64,
}

/// File bytes consumed so far.
pub(crate) fn consumed(counter: &AtomicU64) -> u64 {
    counter.load(Ordering::Relaxed)
}

/// The encoding's name in messages.
pub(crate) fn encoding_name(encoding: TextEncoding) -> &'static str {
    match encoding {
        TextEncoding::Auto => "auto",
        TextEncoding::Utf8 => "UTF-8",
        TextEncoding::Utf16Le => "UTF-16LE",
        TextEncoding::Utf16Be => "UTF-16BE",
        TextEncoding::Windows1252 => "Windows-1252",
        TextEncoding::Latin1 => "Latin-1",
    }
}

/// Opens `path` as text in `encoding` (`Auto`: BOM, else UTF-8 when the first 64 KiB are
/// valid UTF-8, else Windows-1252).
pub(crate) fn open_text(path: &Path, encoding: TextEncoding) -> Result<TextInput> {
    let mut file = File::open(path).map_err(|e| io_error(path, &e))?;
    let total_bytes = file.metadata().map_err(|e| io_error(path, &e))?.len();
    let mut sample = Vec::new();
    (&mut file)
        .take(SAMPLE_BYTES)
        .read_to_end(&mut sample)
        .map_err(|e| io_error(path, &e))?;
    let (encoding, bom_len) = resolve(encoding, &sample);
    file.seek(SeekFrom::Start(0))
        .map_err(|e| io_error(path, &e))?;
    let counting = CountingReader::new(file);
    let counter = counting.counter();
    let reader = DecodingReader::new(counting, encoding)?;
    Ok(TextInput {
        reader,
        counter,
        encoding,
        bom: bom_len > 0,
        total_bytes,
    })
}
