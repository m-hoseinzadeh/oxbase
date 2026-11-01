//! Type inference over a sample (read stream).
//!
//! Text formats (CSV, JSON strings, XLSX text cells) have no column types, so [`crate::preview`]
//! feeds every sampled value to a [`TypeInference`] and offers the result as the default
//! [`crate::ColumnSpec`]s; the user can override each one.
//!
//! # Rules (contract)
//!
//! For each column, the narrowest type in this order that **every non-NULL sampled value**
//! parses as (with the reader's [`ValueFormats`]):
//!
//! 1. `bool`: `true` / `false` only (any case; the other boolean tokens are accepted only in
//!    columns the user types `bool`, because `1` / `0` / `y` / `n` are ambiguous);
//! 2. `int(64)`: optional sign and digits, within `i64` (grouping characters removed when a
//!    thousands separator is set). Leading zeros (`007`) make it text: they are identifiers,
//!    not numbers;
//! 3. `decimal(p, s)`: a plain decimal with the decimal separator, at most 38 digits; `p` and
//!    `s` are the largest seen (`s` = most fraction digits, `p` = most integer digits + `s`);
//! 4. `float(64)`: exponent forms, `NaN`, `inf` / `Infinity`, or more than 38 digits;
//! 5. `date` / `timestamp` / `timestamp_tz` / `time`: the formats of [`ValueFormats`] (ISO 8601
//!    when unset); a column mixing dates and timestamps is `timestamp`;
//! 6. `uuid`: canonical hyphenated form;
//! 7. `json`: text starting with `{` or `[` that parses as JSON (JSON reader: nested values
//!    are always `json`);
//! 8. else `text`.
//!
//! A column whose sampled values are all NULL is `text`, nullable. `nullable` is `true` when a
//! NULL was seen (or the sample was not the whole file: a later NULL cannot be ruled out, so
//! only a complete sample may report `false`). Warnings: values that broke a narrower type
//! ("2 of 1000 values are not integers: 'n/a', '12a'; read as text"), and the longest text
//! seen when over 255 characters (helps choose `varchar(n)` vs `text`). Typed sources (XLSX
//! numbers, booleans and dates; JSON numbers and booleans) report their type with
//! [`TypeInference::observe_typed`] and widen the same way (`int` + `float` → `float`, anything +
//! text → text).

use oxbase_types::DataType;

use crate::ValueFormats;
use crate::read::values::{
    is_canonical_uuid, is_json, is_special_float, parse_date, parse_naive_timestamp, parse_time,
    parse_timestamp_tz, scan_number,
};

/// What inference concluded for one column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferredColumn {
    pub data_type: DataType,
    pub nullable: bool,
    pub warnings: Vec<String>,
}

/// Most digits a `decimal` column may have; more is `float`.
const MAX_DECIMAL_DIGITS: usize = 38;

/// Text longer than this many characters is noted (helps choose `varchar(n)` vs `text`).
const LONG_TEXT_CHARS: usize = 255;

/// Example values kept per kind for warnings.
const EXAMPLES: usize = 3;

/// Characters of an example value shown in a warning.
const EXAMPLE_CHARS: usize = 40;

/// What one value looks like, narrowest first (the order of the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Bool,
    Int,
    Decimal,
    Float,
    Date,
    Timestamp,
    TimestampTz,
    Time,
    Uuid,
    Json,
    Text,
}

impl Kind {
    const ALL: [Kind; 11] = [
        Kind::Bool,
        Kind::Int,
        Kind::Decimal,
        Kind::Float,
        Kind::Date,
        Kind::Timestamp,
        Kind::TimestampTz,
        Kind::Time,
        Kind::Uuid,
        Kind::Json,
        Kind::Text,
    ];

    fn index(self) -> usize {
        self as usize
    }

    /// The narrowest kind both values fit (the widening rules of the module docs).
    fn join(self, other: Kind) -> Kind {
        use Kind::{Date, Decimal, Float, Int, Text, Timestamp};
        match (self, other) {
            (a, b) if a == b => a,
            (Int, Decimal) | (Decimal, Int) => Decimal,
            (Int | Decimal, Float) | (Float, Int | Decimal) => Float,
            (Date, Timestamp) | (Timestamp, Date) => Timestamp,
            _ => Text,
        }
    }

    /// Plural noun for warnings ("3 of 10 values are not integers").
    fn noun(self) -> &'static str {
        match self {
            Kind::Bool => "booleans",
            Kind::Int => "integers",
            Kind::Decimal => "decimal numbers",
            Kind::Float => "numbers",
            Kind::Date => "dates",
            Kind::Timestamp => "timestamps",
            Kind::TimestampTz => "timestamps with a time zone",
            Kind::Time => "times of day",
            Kind::Uuid => "UUIDs",
            Kind::Json => "JSON documents",
            Kind::Text => "text",
        }
    }
}

/// What one number looks like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NumberKind {
    Int { digits: usize },
    Decimal { int_digits: usize, scale: usize },
    Float,
}

/// Classifies a number written in text (`None`: not a number, or one with leading zeros).
fn classify_number(text: &str, decimal: char, thousands: Option<char>) -> Option<NumberKind> {
    if is_special_float(text) {
        return Some(NumberKind::Float);
    }
    let parts = scan_number(text, decimal, thousands)?;
    if parts.has_leading_zero() {
        return None;
    }
    if parts.exponent.is_some() {
        return Some(NumberKind::Float);
    }
    let int_digits = parts.significant_int_digits();
    if !parts.point {
        let mut digits = String::with_capacity(parts.int.len() + 1);
        if parts.negative {
            digits.push('-');
        }
        digits.push_str(&parts.int);
        return Some(if digits.parse::<i64>().is_ok() {
            NumberKind::Int { digits: int_digits }
        } else if int_digits <= MAX_DECIMAL_DIGITS {
            NumberKind::Decimal {
                int_digits,
                scale: 0,
            }
        } else {
            NumberKind::Float
        });
    }
    let scale = parts.frac.len();
    Some(if int_digits + scale <= MAX_DECIMAL_DIGITS {
        NumberKind::Decimal { int_digits, scale }
    } else {
        NumberKind::Float
    })
}

/// Per-column tallies.
#[derive(Debug, Clone, Default)]
struct ColumnStats {
    nulls: u64,
    counts: [u64; 11],
    examples: [Vec<String>; 11],
    /// Most integer digits of `int` / `decimal` values.
    int_digits: usize,
    /// Most fraction digits of `decimal` values.
    scale: usize,
    /// Longest text value, in characters.
    max_chars: usize,
}

impl ColumnStats {
    fn count(&mut self, kind: Kind, example: Option<&str>) {
        let i = kind.index();
        self.counts[i] += 1;
        if let Some(text) = example
            && self.examples[i].len() < EXAMPLES
        {
            let shown: String = text.chars().take(EXAMPLE_CHARS).collect();
            let cut = if shown.len() < text.len() { "…" } else { "" };
            self.examples[i].push(format!("'{shown}{cut}'"));
        }
    }

    fn number(&mut self, kind: NumberKind, example: Option<&str>) {
        match kind {
            NumberKind::Int { digits } => {
                self.int_digits = self.int_digits.max(digits);
                self.count(Kind::Int, example);
            }
            NumberKind::Decimal { int_digits, scale } => {
                self.int_digits = self.int_digits.max(int_digits);
                self.scale = self.scale.max(scale);
                self.count(Kind::Decimal, example);
            }
            NumberKind::Float => self.count(Kind::Float, example),
        }
    }
}

/// Collects sampled values per column and infers their types (module docs).
#[derive(Debug, Clone)]
pub struct TypeInference {
    formats: ValueFormats,
    columns: usize,
    stats: Vec<ColumnStats>,
}

impl TypeInference {
    /// Inference for `columns` columns read with `formats`.
    pub fn new(columns: usize, formats: ValueFormats) -> Self {
        Self {
            formats,
            columns,
            stats: vec![ColumnStats::default(); columns],
        }
    }

    /// Number of columns.
    pub fn columns(&self) -> usize {
        self.columns
    }

    fn column(&mut self, column: usize) -> &mut ColumnStats {
        if column >= self.stats.len() {
            self.stats.resize(column + 1, ColumnStats::default());
            self.columns = self.stats.len();
        }
        &mut self.stats[column]
    }

    /// Records one sampled text value of `column` (`None` = NULL). A column index past the
    /// end widens the inference (ragged rows add columns).
    pub fn observe(&mut self, column: usize, text: Option<&str>) {
        let Some(text) = text else {
            self.column(column).nulls += 1;
            return;
        };
        let chars = if text.len() > LONG_TEXT_CHARS {
            text.chars().count()
        } else {
            text.len()
        };
        let classified = self.classify(text);
        let stats = self.column(column);
        stats.max_chars = stats.max_chars.max(chars);
        match classified {
            Ok(number) => stats.number(number, Some(text)),
            Err(kind) => stats.count(kind, Some(text)),
        }
    }

    /// Records `n` NULLs of `column` (rows that lacked it).
    pub(crate) fn observe_nulls(&mut self, column: usize, n: u64) {
        self.column(column).nulls += n;
    }

    /// Records a number as written in a typed source (JSON): `int`, `decimal` or `float` by
    /// its text, without the leading-zero rule (JSON has none).
    pub(crate) fn observe_number(&mut self, column: usize, raw: &str) {
        let kind = classify_number(raw, '.', None).unwrap_or(NumberKind::Float);
        self.column(column).number(kind, Some(raw));
    }

    /// Records an integer from a typed source (XLSX numbers), with its digit count.
    pub(crate) fn observe_int(&mut self, column: usize, value: i64) {
        let digits = value
            .unsigned_abs()
            .checked_ilog10()
            .map_or(0, |d| usize::try_from(d).unwrap_or(19) + 1);
        self.column(column).number(NumberKind::Int { digits }, None);
    }

    /// Records one value of `column` whose type the source states (XLSX cell types, JSON
    /// numbers and booleans).
    pub fn observe_typed(&mut self, column: usize, data_type: &DataType) {
        let stats = self.column(column);
        match data_type {
            DataType::Bool => stats.count(Kind::Bool, None),
            // The digit count is unknown: assume the widest integer.
            DataType::Int { .. } => stats.number(NumberKind::Int { digits: 19 }, None),
            DataType::Decimal { precision, scale } => {
                let scale = scale.map_or(0, |s| usize::try_from(s).unwrap_or(0));
                let precision =
                    precision.map_or(MAX_DECIMAL_DIGITS, |p| usize::try_from(p).unwrap_or(0));
                stats.number(
                    NumberKind::Decimal {
                        int_digits: precision.saturating_sub(scale),
                        scale,
                    },
                    None,
                );
            }
            DataType::Float { .. } => stats.count(Kind::Float, None),
            DataType::Date => stats.count(Kind::Date, None),
            DataType::Timestamp { .. } => stats.count(Kind::Timestamp, None),
            DataType::TimestampTz { .. } => stats.count(Kind::TimestampTz, None),
            DataType::Time { .. } => stats.count(Kind::Time, None),
            DataType::Uuid => stats.count(Kind::Uuid, None),
            DataType::Json { .. } => stats.count(Kind::Json, None),
            _ => stats.count(Kind::Text, None),
        }
    }

    /// The kind of one text value: a number (`Ok`) or another kind (`Err`).
    fn classify(&self, text: &str) -> Result<NumberKind, Kind> {
        let f = &self.formats;
        let trimmed = text.trim();
        if trimmed.eq_ignore_ascii_case("true") || trimmed.eq_ignore_ascii_case("false") {
            return Err(Kind::Bool);
        }
        if let Some(number) = classify_number(text, f.decimal_separator, f.thousands_separator) {
            return Ok(number);
        }
        if parse_date(text, f).is_some() {
            return Err(Kind::Date);
        }
        if parse_naive_timestamp(text, f).is_some() {
            return Err(Kind::Timestamp);
        }
        if parse_timestamp_tz(text, f).is_some() {
            return Err(Kind::TimestampTz);
        }
        if parse_time(text, f).is_some() {
            return Err(Kind::Time);
        }
        if is_canonical_uuid(trimmed) {
            return Err(Kind::Uuid);
        }
        if (trimmed.starts_with('{') || trimmed.starts_with('[')) && is_json(trimmed) {
            return Err(Kind::Json);
        }
        Err(Kind::Text)
    }

    /// The inferred columns, in order. `complete` says whether the sample was the whole file
    /// (see the module docs on `nullable`).
    pub fn finish(&self, complete: bool) -> Vec<InferredColumn> {
        self.stats
            .iter()
            .map(|stats| infer_column(stats, complete))
            .collect()
    }
}

fn infer_column(stats: &ColumnStats, complete: bool) -> InferredColumn {
    let nullable = stats.nulls > 0 || !complete;
    let total: u64 = stats.counts.iter().sum();
    if total == 0 {
        return InferredColumn {
            data_type: DataType::Text,
            nullable: true,
            warnings: Vec::new(),
        };
    }
    let seen: Vec<Kind> = Kind::ALL
        .into_iter()
        .filter(|k| stats.counts[k.index()] > 0)
        .collect();
    let mut kind = seen
        .iter()
        .copied()
        .reduce(Kind::join)
        .unwrap_or(Kind::Text);
    let mut warnings = Vec::new();
    let precision = stats.int_digits + stats.scale;
    if kind == Kind::Decimal && precision > MAX_DECIMAL_DIGITS {
        kind = Kind::Float;
        warnings.push(format!(
            "values have up to {precision} digits, more than a decimal holds; read as float64"
        ));
    }
    let data_type = match kind {
        Kind::Bool => DataType::Bool,
        Kind::Int => DataType::INT64,
        Kind::Decimal => DataType::Decimal {
            precision: u32::try_from(precision.max(1)).ok(),
            scale: u32::try_from(stats.scale).ok(),
        },
        Kind::Float => DataType::FLOAT64,
        Kind::Date => DataType::Date,
        Kind::Timestamp => DataType::Timestamp { precision: Some(6) },
        Kind::TimestampTz => DataType::TimestampTz { precision: Some(6) },
        Kind::Time => DataType::Time { precision: Some(6) },
        Kind::Uuid => DataType::Uuid,
        Kind::Json => DataType::Json { binary: false },
        Kind::Text => DataType::Text,
    };
    // The most common kind; when the column type differs, say which values broke it.
    let majority = seen
        .iter()
        .copied()
        .max_by_key(|k| (stats.counts[k.index()], std::cmp::Reverse(k.index())))
        .unwrap_or(kind);
    if majority != kind {
        let others = total - stats.counts[majority.index()];
        let examples: Vec<&str> = seen
            .iter()
            .filter(|&&k| k != majority)
            .flat_map(|k| stats.examples[k.index()].iter().map(String::as_str))
            .take(EXAMPLES)
            .collect();
        let shown = if examples.is_empty() {
            String::new()
        } else {
            format!(": {}", examples.join(", "))
        };
        warnings.push(format!(
            "{others} of {total} values are not {}{shown}; read as {data_type}",
            majority.noun()
        ));
    }
    if matches!(kind, Kind::Text | Kind::Json) && stats.max_chars > LONG_TEXT_CHARS {
        warnings.push(format!("longest value has {} characters", stats.max_chars));
    }
    InferredColumn {
        data_type,
        nullable,
        warnings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn infer(values: &[Option<&str>]) -> InferredColumn {
        let mut inference = TypeInference::new(1, ValueFormats::default());
        for v in values {
            inference.observe(0, *v);
        }
        inference.finish(true).remove(0)
    }

    fn infer_type(values: &[&str]) -> DataType {
        infer(&values.iter().map(|v| Some(*v)).collect::<Vec<_>>()).data_type
    }

    fn decimal(p: u32, s: u32) -> DataType {
        DataType::Decimal {
            precision: Some(p),
            scale: Some(s),
        }
    }

    #[test]
    fn the_order_of_the_rules() {
        assert_eq!(infer_type(&["true", "FALSE"]), DataType::Bool);
        // 1 / 0 are integers, never booleans.
        assert_eq!(infer_type(&["1", "0"]), DataType::INT64);
        assert_eq!(infer_type(&["yes", "no"]), DataType::Text);
        assert_eq!(infer_type(&["-12", "+7", "0"]), DataType::INT64);
        // Leading zeros are identifiers.
        assert_eq!(infer_type(&["007", "12"]), DataType::Text);
        assert_eq!(
            infer_type(&["12.50", "-0.75", "1000000.00", "0.00", "3.14"]),
            decimal(9, 2)
        );
        assert_eq!(infer_type(&["1", "2.5"]), decimal(2, 1));
        assert_eq!(infer_type(&["9223372036854775808"]), decimal(19, 0));
        assert_eq!(infer_type(&["1e3", "2"]), DataType::FLOAT64);
        assert_eq!(infer_type(&["NaN", "1.5"]), DataType::FLOAT64);
        assert_eq!(infer_type(&[&"1".repeat(40)]), DataType::FLOAT64);
        assert_eq!(infer_type(&["2001-02-03"]), DataType::Date);
        assert_eq!(
            infer_type(&["2001-02-03", "2001-02-03 04:05:06"]),
            DataType::Timestamp { precision: Some(6) }
        );
        assert_eq!(
            infer_type(&["2001-02-03T04:05:06Z", "2001-02-03 04:05:06+02:00"]),
            DataType::TimestampTz { precision: Some(6) }
        );
        assert_eq!(
            infer_type(&["04:05:06", "23:59"]),
            DataType::Time { precision: Some(6) }
        );
        assert_eq!(
            infer_type(&["67e55044-10b1-426f-9247-bb680e5fe0c8"]),
            DataType::Uuid
        );
        assert_eq!(
            infer_type(&["{\"a\": 1}", "[1, 2]"]),
            DataType::Json { binary: false }
        );
        assert_eq!(infer_type(&["{not json"]), DataType::Text);
        assert_eq!(infer_type(&["1", "abc"]), DataType::Text);
        assert_eq!(infer_type(&["2001-02-03", "1"]), DataType::Text);
    }

    #[test]
    fn nullability_and_warnings() {
        let all_null = infer(&[None, None]);
        assert_eq!(all_null.data_type, DataType::Text);
        assert!(all_null.nullable);

        let ints = infer(&[Some("1"), Some("2")]);
        assert!(!ints.nullable);
        let mut partial = TypeInference::new(1, ValueFormats::default());
        partial.observe(0, Some("1"));
        assert!(partial.finish(false)[0].nullable);

        let broken = infer(&[Some("1"), Some("2"), Some("3"), Some("n/a"), None]);
        assert_eq!(broken.data_type, DataType::Text);
        assert!(broken.nullable);
        assert_eq!(
            broken.warnings,
            ["1 of 4 values are not integers: 'n/a'; read as text"]
        );

        let long = "x".repeat(300);
        let text = infer(&[Some(&long)]);
        assert_eq!(text.warnings, ["longest value has 300 characters"]);
    }

    #[test]
    fn formats_and_typed_sources() {
        let formats = ValueFormats {
            decimal_separator: ',',
            thousands_separator: Some('.'),
            date_format: Some("%d.%m.%Y".into()),
            ..ValueFormats::default()
        };
        let mut inference = TypeInference::new(3, formats);
        for (amount, price, day) in [
            ("1.234,56", "0,5", "03.02.2001"),
            ("-7,25", "10", "04.02.2001"),
        ] {
            inference.observe(0, Some(amount));
            inference.observe(1, Some(price));
            inference.observe(2, Some(day));
        }
        let columns = inference.finish(true);
        assert_eq!(columns[0].data_type, decimal(6, 2));
        assert_eq!(columns[1].data_type, decimal(3, 1));
        assert_eq!(columns[2].data_type, DataType::Date);

        let mut typed = TypeInference::new(1, ValueFormats::default());
        typed.observe_typed(0, &DataType::INT64);
        typed.observe_typed(0, &DataType::FLOAT64);
        assert_eq!(typed.finish(true)[0].data_type, DataType::FLOAT64);
        typed.observe(0, Some("abc"));
        assert_eq!(typed.finish(true)[0].data_type, DataType::Text);

        let mut json = TypeInference::new(1, ValueFormats::default());
        json.observe_number(0, "12.5");
        json.observe_number(0, "-0.75");
        assert_eq!(json.finish(true)[0].data_type, decimal(4, 2));
        json.observe_number(0, "1e3");
        assert_eq!(json.finish(true)[0].data_type, DataType::FLOAT64);
        let mut big = TypeInference::new(1, ValueFormats::default());
        big.observe_number(0, "9007199254740993");
        big.observe_number(0, "0");
        assert_eq!(big.finish(true)[0].data_type, DataType::INT64);

        // Ragged rows widen the inference.
        let mut ragged = TypeInference::new(1, ValueFormats::default());
        ragged.observe(2, Some("x"));
        assert_eq!(ragged.columns(), 3);
        ragged.observe_nulls(1, 2);
        ragged.observe_int(0, -1234);
        let columns = ragged.finish(true);
        assert_eq!(columns.len(), 3);
        assert_eq!(columns[0].data_type, DataType::INT64);
        assert!(columns[1].nullable);
    }
}
