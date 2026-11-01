//! Helpers shared by the reader tests (`read_*.rs`; named apart from other streams' helpers).

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use arrow_array::Array;
use arrow_cast::display::{ArrayFormatter, FormatOptions};
use oxbase_driver::CancellationToken;
use oxbase_io::{ColumnSpec, PreviewLimits, ReadOptions, RowIssue, SourceBatch, SourcePreview};
use oxbase_types::{DataType, Result};

/// `tests/data/m5` of the workspace.
pub fn data_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/data/m5")
}

/// A committed sample file.
pub fn sample(name: &str) -> PathBuf {
    data_dir().join(name)
}

/// `tests/fixtures.toml` (or `$OXBASE_FIXTURES`).
pub fn fixtures() -> toml::Table {
    let path = std::env::var_os("OXBASE_FIXTURES").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures.toml"),
        PathBuf::from,
    );
    let text = std::fs::read_to_string(&path).expect("tests/fixtures.toml is readable");
    text.parse().expect("tests/fixtures.toml parses")
}

/// Preview with the default limits.
pub fn preview(path: &Path, options: &ReadOptions) -> Result<SourcePreview> {
    oxbase_io::preview(
        path,
        options,
        PreviewLimits::default(),
        &CancellationToken::new(),
    )
}

/// Everything a reader produced.
pub struct ReadAll {
    pub batches: Vec<SourceBatch>,
    pub rows: usize,
    pub issues: Vec<RowIssue>,
    /// `bytes_read` after each batch, then the reader's final position.
    pub positions: Vec<u64>,
    pub total_bytes: u64,
}

/// Reads every batch of `path` as `specs`.
pub fn read_all(
    path: &Path,
    options: &ReadOptions,
    specs: &[ColumnSpec],
    batch_rows: usize,
) -> Result<ReadAll> {
    let mut reader = oxbase_io::open(path, options, specs, batch_rows)?;
    let schema = reader.schema();
    let mut out = ReadAll {
        batches: Vec::new(),
        rows: 0,
        issues: Vec::new(),
        positions: Vec::new(),
        total_bytes: reader.total_bytes(),
    };
    while let Some(batch) = reader.next_batch()? {
        assert_eq!(
            batch.batch.schema(),
            schema,
            "every batch is in the reader's schema"
        );
        assert!(batch.batch.num_rows() <= batch_rows.max(1));
        assert!(batch.batch.num_rows() > 0);
        assert_eq!(batch.first_row, u64::try_from(out.rows).unwrap() + 1);
        assert_eq!(batch.bytes_read, reader.bytes_read());
        out.rows += batch.batch.num_rows();
        out.issues.extend(batch.issues.iter().cloned());
        out.positions.push(batch.bytes_read);
        out.batches.push(batch);
    }
    out.positions.push(reader.bytes_read());
    Ok(out)
}

/// Preview, then read everything with the preview's specs.
pub fn preview_and_read(path: &Path, options: &ReadOptions) -> Result<(SourcePreview, ReadAll)> {
    let p = preview(path, options)?;
    let specs = ColumnSpec::from_preview(&p);
    let all = read_all(path, options, &specs, 2)?;
    Ok((p, all))
}

impl ReadAll {
    /// Column `column` of every batch as display text (`None` = NULL).
    pub fn column(&self, column: usize) -> Vec<Option<String>> {
        let options = FormatOptions::default();
        let mut out = Vec::new();
        for batch in &self.batches {
            let array = batch.batch.column(column);
            let formatter = ArrayFormatter::try_new(array.as_ref(), &options).unwrap();
            for i in 0..array.len() {
                out.push((!array.is_null(i)).then(|| formatter.value(i).to_string()));
            }
        }
        out
    }

    /// Every non-NULL cell of every column as text.
    pub fn all_text(&self) -> Vec<String> {
        let columns = self.batches.first().map_or(0, |b| b.batch.num_columns());
        (0..columns)
            .flat_map(|c| self.column(c))
            .flatten()
            .collect()
    }

    /// NULL cells over all columns.
    pub fn nulls(&self) -> usize {
        self.batches
            .iter()
            .flat_map(|b| b.batch.columns().iter().map(Array::null_count))
            .sum()
    }

    /// `bytes_read` never goes down and ends at the file size.
    pub fn assert_progress(&self) {
        assert!(
            self.positions.windows(2).all(|w| w[0] <= w[1]),
            "positions go down: {:?}",
            self.positions
        );
        assert_eq!(self.positions.last().copied(), Some(self.total_bytes));
    }
}

/// The type family name used in `tests/fixtures.toml`.
pub fn kind(t: &DataType) -> &'static str {
    match t {
        DataType::Bool => "bool",
        DataType::Int { .. } => "int",
        DataType::Float { .. } => "float",
        DataType::Decimal { .. } => "decimal",
        DataType::Text | DataType::Char { .. } | DataType::VarChar { .. } => "text",
        DataType::Date => "date",
        DataType::Time { .. } => "time",
        DataType::Timestamp { .. } => "timestamp",
        DataType::TimestampTz { .. } => "timestamp_tz",
        DataType::Uuid => "uuid",
        DataType::Json { .. } => "json",
        DataType::Binary { .. } | DataType::VarBinary { .. } => "binary",
        _ => "other",
    }
}

/// Deterministic pseudo-random numbers (xorshift64*), so failures reproduce.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// A number in `0..n`.
    pub fn below(&mut self, n: usize) -> usize {
        let n = u64::try_from(n.max(1)).unwrap();
        usize::try_from(self.next_u64() % n).unwrap()
    }
}
