//! Demo result page for M0: proves the binary Arrow IPC path from Rust to the webview.
//!
//! Real result pages will come from `oxbase-services` (spilled result store) in M2. This module
//! only builds a tiny fixed `RecordBatch` and serialises it as an Arrow IPC *stream*, which the
//! UI decodes with the `apache-arrow` npm package.

use std::sync::Arc;

use arrow_array::{ArrayRef, Float64Array, Int32Array, RecordBatch, StringArray};
use arrow_ipc::writer::StreamWriter;
use arrow_schema::{ArrowError, DataType, Field, Schema};

/// Builds the fixed demo batch: `id INT32`, `name UTF8`, `score FLOAT64 (nullable)`.
pub fn demo_batch() -> Result<RecordBatch, ArrowError> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int32, false),
        Field::new("name", DataType::Utf8, false),
        Field::new("score", DataType::Float64, true),
    ]));
    let ids: ArrayRef = Arc::new(Int32Array::from(vec![1, 2, 3, 4, 5]));
    let names: ArrayRef = Arc::new(StringArray::from(vec![
        "alpha", "bravo", "charlie", "delta", "echo",
    ]));
    let scores: ArrayRef = Arc::new(Float64Array::from(vec![
        Some(1.5),
        Some(2.25),
        None,
        Some(4.0),
        Some(5.125),
    ]));
    RecordBatch::try_new(schema, vec![ids, names, scores])
}

/// Serialises one batch as a complete Arrow IPC stream (schema, batch, end-of-stream marker).
pub fn encode_ipc_stream(batch: &RecordBatch) -> Result<Vec<u8>, ArrowError> {
    let mut writer = StreamWriter::try_new(Vec::new(), &batch.schema())?;
    writer.write(batch)?;
    writer.finish()?;
    writer.into_inner()
}

/// The demo page as Arrow IPC stream bytes.
pub fn demo_page_bytes() -> Result<Vec<u8>, ArrowError> {
    encode_ipc_stream(&demo_batch()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_ipc::reader::StreamReader;

    /// Rows in the demo page; `ui/src/panels/WelcomePanel.test.tsx` expects the same count.
    const DEMO_ROWS: usize = 5;

    #[test]
    fn demo_page_round_trips_through_ipc_stream() {
        let bytes = demo_page_bytes().expect("encode");
        let reader = StreamReader::try_new(std::io::Cursor::new(bytes), None).expect("reader");
        let schema = reader.schema();
        let batches: Vec<RecordBatch> = reader.collect::<Result<_, _>>().expect("batches");
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].num_rows(), DEMO_ROWS);
        assert_eq!(schema.fields().len(), 3);
        assert_eq!(schema.field(1).name(), "name");
    }

    /// Keeps the UI's Rust -> JS format fixture in sync with [`demo_page_bytes`].
    ///
    /// `ui/src/panels/WelcomePanel.test.tsx` decodes this fixture with `apache-arrow`; if the
    /// bytes drifted from what the command returns, that test would silently stop checking the
    /// real format. Regenerate with `OXBASE_REGEN_UI_FIXTURES=1 cargo test -p app-tauri`.
    #[test]
    fn ui_fixture_matches_demo_page() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../ui/src/test/fixtures/demo-page.arrows"
        );
        let bytes = demo_page_bytes().expect("encode");
        if std::env::var_os("OXBASE_REGEN_UI_FIXTURES").is_some() {
            std::fs::write(path, &bytes).expect("write fixture");
        }
        let fixture = std::fs::read(path).expect("read ui fixture");
        assert!(
            bytes == fixture,
            "{path} is stale; regenerate with `OXBASE_REGEN_UI_FIXTURES=1 cargo test -p app-tauri`"
        );
    }
}
