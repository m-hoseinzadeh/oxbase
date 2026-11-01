//! Format options for reading ([`ReadOptions`]) and writing ([`WriteOptions`]).
//!
//! These cross the Tauri boundary unchanged (JSON, camelCase fields; `format` / `codec` tags),
//! so the TypeScript mirror in `ui/src/api/io.ts` must match. Every struct has serde defaults,
//! so the UI may send only what the user changed. `validate` methods reject combinations that
//! cannot work before any file is touched (`Error::InvalidInput`).

use oxbase_types::{Engine, Error, ObjectRef, Result};
use serde::{Deserialize, Serialize};

/// A file format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileFormat {
    /// Delimited text (CSV, TSV, any single-character delimiter).
    Csv,
    /// JSON: an array of objects, or one object per line (NDJSON).
    Json,
    Xlsx,
    Parquet,
    /// `CREATE TABLE` + `INSERT` statements for a target engine (write only).
    SqlDump,
}

impl FileFormat {
    /// The usual file extensions, first one preferred (for dialog filters and default names).
    pub fn extensions(self) -> &'static [&'static str] {
        match self {
            Self::Csv => &["csv", "tsv", "txt"],
            Self::Json => &["json", "ndjson", "jsonl"],
            Self::Xlsx => &["xlsx"],
            Self::Parquet => &["parquet"],
            Self::SqlDump => &["sql"],
        }
    }

    /// `true` when M5 can import from this format.
    pub fn can_read(self) -> bool {
        !matches!(self, Self::SqlDump)
    }
}

/// Character encoding of a text file (CSV, JSON).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum TextEncoding {
    /// Reading only: a BOM decides (UTF-8, UTF-16LE, UTF-16BE); without one, UTF-8 when the
    /// sample is valid UTF-8, else Windows-1252.
    #[default]
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "utf8")]
    Utf8,
    #[serde(rename = "utf16le")]
    Utf16Le,
    #[serde(rename = "utf16be")]
    Utf16Be,
    /// Windows code page 1252 (what `encoding_rs` also uses for the `latin1` / `ISO-8859-1`
    /// labels, per the WHATWG Encoding Standard).
    #[serde(rename = "windows1252")]
    Windows1252,
    /// True ISO-8859-1: every byte is the code point of the same value (bytes 0x80-0x9F are C1
    /// controls, not the Windows-1252 punctuation). Writing fails on characters above U+00FF.
    #[serde(rename = "latin1")]
    Latin1,
}

/// Line endings. Reading: `Auto` accepts `\n`, `\r\n` (and `\r` with `Cr`). Writing: `Lf` or
/// `CrLf` only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LineEnding {
    #[default]
    Auto,
    Lf,
    CrLf,
    /// Classic Mac OS line ends (reading only).
    Cr,
}

/// How values are written as text and read back from text (CSV cells, JSON strings, XLSX text
/// cells). `None` formats mean ISO 8601: dates `YYYY-MM-DD`, times `HH:MM:SS[.ffffff]`,
/// timestamps `YYYY-MM-DD HH:MM:SS[.ffffff]` (a `T` separator is accepted when reading), with an
/// offset `+HH:MM` / `Z` for timestamps with a time zone. Formats are chrono `strftime`
/// patterns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ValueFormats {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date_format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp_format: Option<String>,
    /// `.` or `,`.
    pub decimal_separator: char,
    /// Grouping character ignored when reading numbers (and never written).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thousands_separator: Option<char>,
    /// Text read as `true` for boolean columns (compared without case). Type inference only
    /// infers `bool` for `true` / `false`; the other tokens apply to columns typed `bool`.
    pub true_values: Vec<String>,
    pub false_values: Vec<String>,
}

impl Default for ValueFormats {
    fn default() -> Self {
        Self {
            date_format: None,
            time_format: None,
            timestamp_format: None,
            decimal_separator: '.',
            thousands_separator: None,
            true_values: ["true", "t", "yes", "y", "1"].map(String::from).to_vec(),
            false_values: ["false", "f", "no", "n", "0"].map(String::from).to_vec(),
        }
    }
}

impl ValueFormats {
    pub fn validate(&self) -> Result<()> {
        if !matches!(self.decimal_separator, '.' | ',') {
            return Err(Error::invalid_input(
                "the decimal separator must be '.' or ','",
            ));
        }
        if self.thousands_separator == Some(self.decimal_separator) {
            return Err(Error::invalid_input(
                "the thousands separator must differ from the decimal separator",
            ));
        }
        for format in [&self.date_format, &self.time_format, &self.timestamp_format]
            .into_iter()
            .flatten()
        {
            if format.trim().is_empty() {
                return Err(Error::invalid_input("a date or time format is empty"));
            }
        }
        Ok(())
    }
}

/// Options for reading delimited text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CsvReadOptions {
    /// `,` (CSV), `\t` (TSV), `;`, `|`, ... Must be ASCII.
    pub delimiter: char,
    /// Quote character (ASCII); `None` turns quoting off.
    pub quote: Option<char>,
    /// Escape character inside quotes (ASCII, e.g. `\`); `None` means quotes are doubled.
    pub escape: Option<char>,
    /// The first (non-skipped) row holds column names.
    pub header: bool,
    /// Lines to skip before the header / first row.
    pub skip_rows: u32,
    pub encoding: TextEncoding,
    pub line_ending: LineEnding,
    /// Lines starting with this ASCII character are comments.
    pub comment: Option<char>,
    /// Field texts read as NULL (exact match after `trim`). Default: the empty field. The
    /// `csv` crate does not report whether a field was quoted, so `""` and an empty unquoted
    /// field are the same.
    pub null_tokens: Vec<String>,
    /// Trim spaces around unquoted values.
    pub trim: bool,
    /// Rows with more or fewer fields than the header: `true` pads missing fields with NULL
    /// and reports extra fields as a row issue; `false` makes every ragged row an issue.
    pub allow_ragged: bool,
    pub formats: ValueFormats,
}

impl Default for CsvReadOptions {
    fn default() -> Self {
        Self {
            delimiter: ',',
            quote: Some('"'),
            escape: None,
            header: true,
            skip_rows: 0,
            encoding: TextEncoding::Auto,
            line_ending: LineEnding::Auto,
            comment: None,
            null_tokens: vec![String::new()],
            trim: false,
            allow_ragged: true,
            formats: ValueFormats::default(),
        }
    }
}

impl CsvReadOptions {
    /// Tab-separated defaults.
    pub fn tsv() -> Self {
        Self {
            delimiter: '\t',
            ..Self::default()
        }
    }

    pub fn validate(&self) -> Result<()> {
        check_ascii("delimiter", Some(self.delimiter))?;
        check_ascii("quote", self.quote)?;
        check_ascii("escape", self.escape)?;
        check_ascii("comment", self.comment)?;
        if matches!(self.delimiter, '\n' | '\r') || Some(self.delimiter) == self.quote {
            return Err(Error::invalid_input(
                "the delimiter must differ from the quote and line ends",
            ));
        }
        if self.formats.decimal_separator == self.delimiter {
            return Err(Error::invalid_input(
                "the decimal separator must differ from the delimiter",
            ));
        }
        self.formats.validate()
    }
}

/// How quotes are used when writing delimited text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuoteStyle {
    /// Only fields that contain the delimiter, a quote or a line end.
    #[default]
    Necessary,
    Always,
    /// Every field that is not a number.
    NonNumeric,
    /// Never (fields that would need quotes are an error).
    Never,
}

/// Options for writing delimited text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CsvWriteOptions {
    pub delimiter: char,
    pub quote: char,
    pub quote_style: QuoteStyle,
    /// `None`: quotes inside fields are doubled; `Some(c)`: escaped with `c`.
    pub escape: Option<char>,
    pub header: bool,
    /// Not `Auto`.
    pub encoding: TextEncoding,
    /// Write a byte order mark (UTF-8 / UTF-16 only).
    pub bom: bool,
    /// `Lf` or `CrLf`.
    pub line_ending: LineEnding,
    /// How NULL is written (default: empty field).
    pub null_token: String,
    pub formats: ValueFormats,
    /// Neutralize text that a spreadsheet would run as a formula ([`crate::formula`]).
    pub formula_guard: bool,
}

impl Default for CsvWriteOptions {
    fn default() -> Self {
        Self {
            delimiter: ',',
            quote: '"',
            quote_style: QuoteStyle::Necessary,
            escape: None,
            header: true,
            encoding: TextEncoding::Utf8,
            bom: false,
            line_ending: LineEnding::Lf,
            null_token: String::new(),
            formats: ValueFormats::default(),
            formula_guard: true,
        }
    }
}

impl CsvWriteOptions {
    pub fn validate(&self) -> Result<()> {
        check_ascii("delimiter", Some(self.delimiter))?;
        check_ascii("quote", Some(self.quote))?;
        check_ascii("escape", self.escape)?;
        if self.delimiter == self.quote || matches!(self.delimiter, '\n' | '\r') {
            return Err(Error::invalid_input(
                "the delimiter must differ from the quote and line ends",
            ));
        }
        if self.encoding == TextEncoding::Auto {
            return Err(Error::invalid_input("choose an encoding to write"));
        }
        if !matches!(self.line_ending, LineEnding::Lf | LineEnding::CrLf) {
            return Err(Error::invalid_input("line endings must be LF or CRLF"));
        }
        if self.bom
            && matches!(
                self.encoding,
                TextEncoding::Windows1252 | TextEncoding::Latin1
            )
        {
            return Err(Error::invalid_input(
                "a byte order mark needs UTF-8 or UTF-16",
            ));
        }
        self.formats.validate()
    }
}

/// JSON file layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JsonLayout {
    /// Reading only: `[` as the first non-space character means `Array`, else `Lines`.
    #[default]
    Auto,
    /// One top-level array of objects.
    Array,
    /// One object per line (NDJSON / JSON Lines).
    Lines,
}

/// Options for reading JSON. Rows are objects; keys become columns in first-seen order over the
/// sample (a key first seen after the sample is a row issue). Nested objects and arrays are
/// read as JSON text (logical type `json`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct JsonReadOptions {
    pub layout: JsonLayout,
    /// `Auto`: BOM or UTF-8 (JSON is UTF-8 / UTF-16 / UTF-32 only; UTF-32 is not supported).
    pub encoding: TextEncoding,
    pub formats: ValueFormats,
}

impl JsonReadOptions {
    pub fn validate(&self) -> Result<()> {
        if matches!(
            self.encoding,
            TextEncoding::Windows1252 | TextEncoding::Latin1
        ) {
            return Err(Error::invalid_input("JSON text is UTF-8 or UTF-16"));
        }
        self.formats.validate()
    }
}

/// How binary values are written to JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BinaryEncoding {
    #[default]
    Base64,
    Hex,
}

/// Options for writing JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct JsonWriteOptions {
    /// `Array` or `Lines` (not `Auto`).
    pub layout: JsonLayout,
    /// Indent objects (array layout only).
    pub pretty: bool,
    pub binary: BinaryEncoding,
    /// Write NULL values as `"col": null` (default) instead of leaving the key out.
    pub explicit_nulls: bool,
}

impl Default for JsonWriteOptions {
    fn default() -> Self {
        Self {
            layout: JsonLayout::Array,
            pretty: false,
            binary: BinaryEncoding::Base64,
            explicit_nulls: true,
        }
    }
}

impl JsonWriteOptions {
    pub fn validate(&self) -> Result<()> {
        if self.layout == JsonLayout::Auto {
            return Err(Error::invalid_input("choose array or lines layout"));
        }
        if self.pretty && self.layout == JsonLayout::Lines {
            return Err(Error::invalid_input("JSON lines cannot be indented"));
        }
        Ok(())
    }
}

/// Options for reading a worksheet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct XlsxReadOptions {
    /// Sheet name; `None` is the first sheet. The preview lists every sheet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sheet: Option<String>,
    /// 1-based row holding column names; `0` means no header (columns are `column_1`, ...).
    /// Rows above it are skipped.
    pub header_row: u32,
    /// Formats for text cells read into typed columns.
    pub formats: ValueFormats,
}

impl Default for XlsxReadOptions {
    fn default() -> Self {
        Self {
            sheet: None,
            header_row: 1,
            formats: ValueFormats::default(),
        }
    }
}

/// Options for writing a workbook.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct XlsxWriteOptions {
    /// Name of the first sheet; later sheets get ` (2)`, ` (3)`, ... (31 characters at most,
    /// none of `[]:*?/\`).
    pub sheet_name: String,
    pub header: bool,
    /// Rows per sheet including the header; more rows continue on a new sheet. At most
    /// 1 048 576 (the format's limit).
    pub max_rows_per_sheet: u32,
    /// Write text that starts with `=`, `+`, `-`, `@` as text with a quote prefix, so it is never
    /// a formula ([`crate::formula`]).
    pub formula_guard: bool,
    /// Excel number format for dates (e.g. `yyyy-mm-dd`).
    pub date_format: String,
    /// Excel number format for timestamps (e.g. `yyyy-mm-dd hh:mm:ss`).
    pub timestamp_format: String,
}

/// The XLSX row limit (rows per sheet, header included).
pub const XLSX_MAX_ROWS: u32 = 1_048_576;

impl Default for XlsxWriteOptions {
    fn default() -> Self {
        Self {
            sheet_name: "Sheet1".to_owned(),
            header: true,
            max_rows_per_sheet: XLSX_MAX_ROWS,
            formula_guard: true,
            date_format: "yyyy-mm-dd".to_owned(),
            timestamp_format: "yyyy-mm-dd hh:mm:ss".to_owned(),
        }
    }
}

impl XlsxWriteOptions {
    pub fn validate(&self) -> Result<()> {
        let name = self.sheet_name.trim();
        if name.is_empty() || name.chars().count() > 31 {
            return Err(Error::invalid_input("a sheet name has 1 to 31 characters"));
        }
        if name.contains(['[', ']', ':', '*', '?', '/', '\\']) {
            return Err(Error::invalid_input(
                "a sheet name cannot contain [ ] : * ? / \\",
            ));
        }
        if self.max_rows_per_sheet < 2 || self.max_rows_per_sheet > XLSX_MAX_ROWS {
            return Err(Error::invalid_input(format!(
                "rows per sheet must be between 2 and {XLSX_MAX_ROWS}"
            )));
        }
        Ok(())
    }
}

/// Options for reading Parquet (none yet; every column is read).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ParquetReadOptions {}

/// Parquet compression codec. Every codec builds on Linux, macOS and Windows: Snappy (`snap`),
/// LZ4 raw (`lz4_flex`), gzip (`flate2`, pure-Rust backend) and Brotli (`brotli`) are pure
/// Rust; zstd is C compiled with `cc`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(tag = "codec", rename_all = "snake_case")]
pub enum ParquetCompression {
    None,
    #[default]
    Snappy,
    Gzip {
        /// 0-9 (default 6).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        level: Option<u32>,
    },
    Brotli {
        /// 0-11 (default 1).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        level: Option<u32>,
    },
    Lz4Raw,
    Zstd {
        /// 1-22 (default 1).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        level: Option<i32>,
    },
}

impl ParquetCompression {
    /// The `parquet` crate's codec; `InvalidInput` for a level out of range.
    pub fn to_parquet(self) -> Result<::parquet::basic::Compression> {
        use ::parquet::basic::{BrotliLevel, Compression, GzipLevel, ZstdLevel};
        let bad = |e: ::parquet::errors::ParquetError| {
            Error::invalid_input(format!("compression level: {e}"))
        };
        Ok(match self {
            Self::None => Compression::UNCOMPRESSED,
            Self::Snappy => Compression::SNAPPY,
            Self::Lz4Raw => Compression::LZ4_RAW,
            Self::Gzip { level } => Compression::GZIP(match level {
                Some(l) => GzipLevel::try_new(l).map_err(bad)?,
                None => GzipLevel::default(),
            }),
            Self::Brotli { level } => Compression::BROTLI(match level {
                Some(l) => BrotliLevel::try_new(l).map_err(bad)?,
                None => BrotliLevel::default(),
            }),
            Self::Zstd { level } => Compression::ZSTD(match level {
                Some(l) => ZstdLevel::try_new(l).map_err(bad)?,
                None => ZstdLevel::default(),
            }),
        })
    }
}

/// Options for writing Parquet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ParquetWriteOptions {
    pub compression: ParquetCompression,
    /// Rows per row group (also the writer's memory bound).
    pub row_group_rows: usize,
}

impl Default for ParquetWriteOptions {
    fn default() -> Self {
        Self {
            compression: ParquetCompression::Snappy,
            row_group_rows: 128 * 1024,
        }
    }
}

impl ParquetWriteOptions {
    pub fn validate(&self) -> Result<()> {
        if self.row_group_rows == 0 {
            return Err(Error::invalid_input("a row group needs at least one row"));
        }
        self.compression.to_parquet().map(|_| ())
    }
}

/// Options for writing a SQL dump: `CREATE TABLE` (from the target engine's `DdlGenerator`)
/// and `INSERT` statements with literals escaped by the target engine's `Quoting`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SqlDumpOptions {
    /// The engine whose SQL the dump is written in (may differ from the source).
    pub target: Engine,
    /// Server version the SQL is written for (`"16"`, `"8.0.36"`); `None` is a recent default
    /// per engine (see `sql_dump`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_version: Option<String>,
    /// The table name used in the dump (`None`: the source table's or `export`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table: Option<ObjectRef>,
    /// Write `CREATE TABLE` first (default `true`).
    #[serde(default = "yes")]
    pub include_create: bool,
    /// Write `DROP TABLE` before the create (default `false`).
    #[serde(default)]
    pub include_drop: bool,
    /// Rows per `INSERT` statement (default 100; 1 writes one statement per row).
    #[serde(default = "default_rows_per_insert")]
    pub rows_per_insert: usize,
}

fn yes() -> bool {
    true
}

fn default_rows_per_insert() -> usize {
    100
}

impl SqlDumpOptions {
    pub fn new(target: Engine) -> Self {
        Self {
            target,
            target_version: None,
            table: None,
            include_create: true,
            include_drop: false,
            rows_per_insert: default_rows_per_insert(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.rows_per_insert == 0 || self.rows_per_insert > 10_000 {
            return Err(Error::invalid_input(
                "rows per INSERT must be between 1 and 10000",
            ));
        }
        if !matches!(
            self.target,
            Engine::Postgres | Engine::MySql | Engine::MariaDb | Engine::Sqlite | Engine::MsSql
        ) {
            return Err(Error::unsupported(format!(
                "{} has no SQL dump",
                self.target.display_name()
            )));
        }
        Ok(())
    }
}

/// What to read and how. JSON: `{"format": "csv", ...CsvReadOptions}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "format", rename_all = "snake_case")]
pub enum ReadOptions {
    Csv(CsvReadOptions),
    Json(JsonReadOptions),
    Xlsx(XlsxReadOptions),
    Parquet(ParquetReadOptions),
}

impl ReadOptions {
    pub fn format(&self) -> FileFormat {
        match self {
            Self::Csv(_) => FileFormat::Csv,
            Self::Json(_) => FileFormat::Json,
            Self::Xlsx(_) => FileFormat::Xlsx,
            Self::Parquet(_) => FileFormat::Parquet,
        }
    }

    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Csv(o) => o.validate(),
            Self::Json(o) => o.validate(),
            Self::Xlsx(o) => o.formats.validate(),
            Self::Parquet(_) => Ok(()),
        }
    }
}

/// What to write and how. JSON: `{"format": "parquet", ...ParquetWriteOptions}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "format", rename_all = "snake_case")]
pub enum WriteOptions {
    Csv(CsvWriteOptions),
    Json(JsonWriteOptions),
    Xlsx(XlsxWriteOptions),
    Parquet(ParquetWriteOptions),
    SqlDump(SqlDumpOptions),
}

impl WriteOptions {
    pub fn format(&self) -> FileFormat {
        match self {
            Self::Csv(_) => FileFormat::Csv,
            Self::Json(_) => FileFormat::Json,
            Self::Xlsx(_) => FileFormat::Xlsx,
            Self::Parquet(_) => FileFormat::Parquet,
            Self::SqlDump(_) => FileFormat::SqlDump,
        }
    }

    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Csv(o) => o.validate(),
            Self::Json(o) => o.validate(),
            Self::Xlsx(o) => o.validate(),
            Self::Parquet(o) => o.validate(),
            Self::SqlDump(o) => o.validate(),
        }
    }
}

fn check_ascii(what: &str, c: Option<char>) -> Result<()> {
    match c {
        Some(c) if !c.is_ascii() => Err(Error::invalid_input(format!(
            "the {what} must be an ASCII character"
        ))),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn defaults_and_json_shape() {
        let csv: ReadOptions = serde_json::from_str(r#"{"format":"csv","delimiter":";"}"#).unwrap();
        let ReadOptions::Csv(o) = &csv else {
            panic!("not csv: {csv:?}")
        };
        assert_eq!(o.delimiter, ';');
        assert_eq!(o.quote, Some('"'));
        assert!(o.header);
        assert_eq!(o.null_tokens, [""]);
        csv.validate().unwrap();

        let json = serde_json::to_value(WriteOptions::Parquet(ParquetWriteOptions {
            compression: ParquetCompression::Zstd { level: Some(3) },
            row_group_rows: 10,
        }))
        .unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "format": "parquet",
                "compression": {"codec": "zstd", "level": 3},
                "rowGroupRows": 10
            })
        );
        let enc = serde_json::to_value(TextEncoding::Utf16Le).unwrap();
        assert_eq!(enc, "utf16le");

        let dump: WriteOptions =
            serde_json::from_str(r#"{"format":"sql_dump","target":"mysql"}"#).unwrap();
        assert_eq!(
            dump,
            WriteOptions::SqlDump(SqlDumpOptions::new(Engine::MySql))
        );
        assert_eq!(dump.format(), FileFormat::SqlDump);
        assert!(!FileFormat::SqlDump.can_read());
    }

    #[test]
    fn validation() {
        let mut csv = CsvReadOptions::default();
        csv.formats.decimal_separator = ',';
        assert!(csv.validate().is_err());
        let mut tsv = CsvReadOptions::tsv();
        tsv.formats.decimal_separator = ',';
        tsv.validate().unwrap();
        tsv.delimiter = '¦';
        assert!(tsv.validate().is_err());

        let mut out = CsvWriteOptions::default();
        out.validate().unwrap();
        out.encoding = TextEncoding::Auto;
        assert!(out.validate().is_err());
        out.encoding = TextEncoding::Latin1;
        out.bom = true;
        assert!(out.validate().is_err());

        let mut xlsx = XlsxWriteOptions::default();
        xlsx.validate().unwrap();
        xlsx.sheet_name = "a/b".into();
        assert!(xlsx.validate().is_err());
        xlsx.sheet_name = "ok".into();
        xlsx.max_rows_per_sheet = XLSX_MAX_ROWS + 1;
        assert!(xlsx.validate().is_err());

        assert!(
            ParquetCompression::Zstd { level: Some(99) }
                .to_parquet()
                .is_err()
        );
        ParquetWriteOptions::default().validate().unwrap();

        let mut json = JsonWriteOptions::default();
        json.validate().unwrap();
        json.layout = JsonLayout::Lines;
        json.pretty = true;
        assert!(json.validate().is_err());

        let mut dump = SqlDumpOptions::new(Engine::Redis);
        assert!(dump.validate().is_err());
        dump.target = Engine::Postgres;
        dump.rows_per_insert = 0;
        assert!(dump.validate().is_err());
    }
}
