//! Value conversion of the import: source batches (in the reader's schema, one canonical Arrow
//! type per source logical type) into the *load schema* of the target columns
//! (`oxbase_dialect::bulk::load_arrow_type`), with range, length, label and NOT NULL checks.
//! A value that does not convert is NULL in the output and reported as a [`CellIssue`]; the
//! pipeline applies the error policy to it.

use std::borrow::Cow;
use std::sync::Arc;

use arrow_array::builder::{
    BinaryBuilder, BooleanBuilder, Date32Builder, FixedSizeBinaryBuilder, Float64Builder,
    Int64Builder, StringBuilder, Time64MicrosecondBuilder, TimestampMicrosecondBuilder,
    UInt64Builder,
};
use arrow_array::cast::AsArray;
use arrow_array::types::{
    Date32Type, Float64Type, Int64Type, Time64MicrosecondType, TimestampMicrosecondType, UInt64Type,
};
use arrow_array::{Array, ArrayRef, RecordBatch};
use arrow_schema::{DataType as ArrowType, Field, Schema, SchemaRef, TimeUnit};
use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use oxbase_dialect::LoadColumn;
use oxbase_dialect::bulk::load_arrow_type;
use oxbase_driver::cell_value;
use oxbase_io::ValueFormats;
use oxbase_types::{DataType, Error, Result, Value};

use super::ConversionOptions;

const MICROS_PER_DAY: i64 = 86_400_000_000;
const MICROS_PER_SECOND: i64 = 1_000_000;
/// Longest part of a value quoted in a message.
const SHOWN_CHARS: usize = 200;

/// The conversion family of a logical type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Kind {
    Bool,
    Int {
        bits: u8,
        unsigned: bool,
    },
    Float {
        bits: u8,
    },
    Decimal {
        precision: Option<u32>,
        scale: Option<u32>,
    },
    /// `char`, `varchar`, `text` and the types carried as their engine text (`interval`,
    /// `array`, `other`).
    Text {
        length: Option<u32>,
    },
    Enum {
        labels: Vec<String>,
    },
    Json,
    Binary {
        length: Option<u32>,
    },
    Date,
    Time,
    Timestamp,
    TimestampTz,
    Uuid,
}

impl Kind {
    pub(super) fn of(data_type: &DataType) -> Self {
        match data_type {
            DataType::Bool => Self::Bool,
            DataType::Int { bits, unsigned } => Self::Int {
                bits: *bits,
                unsigned: *unsigned,
            },
            DataType::Float { bits } => Self::Float { bits: *bits },
            DataType::Decimal { precision, scale } => Self::Decimal {
                precision: *precision,
                scale: *scale,
            },
            DataType::Char { length } | DataType::VarChar { length } => {
                Self::Text { length: *length }
            }
            DataType::Text
            | DataType::Interval
            | DataType::Array { .. }
            | DataType::Other { .. } => Self::Text { length: None },
            DataType::Enum { labels } => Self::Enum {
                labels: labels.clone(),
            },
            DataType::Json { .. } => Self::Json,
            DataType::Binary { length } | DataType::VarBinary { length } => {
                Self::Binary { length: *length }
            }
            DataType::Date => Self::Date,
            DataType::Time { .. } => Self::Time,
            DataType::Timestamp { .. } => Self::Timestamp,
            DataType::TimestampTz { .. } => Self::TimestampTz,
            DataType::Uuid => Self::Uuid,
        }
    }

    fn is_text(&self) -> bool {
        matches!(self, Self::Text { .. } | Self::Enum { .. })
    }
}

/// `(min, max)` of an integer type.
fn int_range(bits: u8, unsigned: bool) -> (i128, i128) {
    let bits = u32::from(bits.clamp(1, 64));
    if unsigned {
        (0, (1_i128 << bits) - 1)
    } else {
        (-(1_i128 << (bits - 1)), (1_i128 << (bits - 1)) - 1)
    }
}

/// Decimal digits the largest value of an integer type has.
fn int_digits(bits: u8, unsigned: bool) -> u32 {
    let (min, max) = int_range(bits, unsigned);
    let widest = max.max(-min);
    widest.to_string().len().try_into().unwrap_or(u32::MAX)
}

/// The note of [`super::conversion_note`] (see its docs for the rules).
pub(super) fn note(from: &DataType, to: &DataType) -> Result<Option<String>> {
    let (f, t) = (Kind::of(from), Kind::of(to));
    let some = |text: String| Ok(Some(text));
    match (&f, &t) {
        (Kind::Bool, Kind::Bool)
        | (Kind::Date, Kind::Date)
        | (Kind::Time, Kind::Time)
        | (Kind::Timestamp, Kind::Timestamp)
        | (Kind::TimestampTz, Kind::TimestampTz)
        | (Kind::Uuid, Kind::Uuid)
        | (Kind::Json, Kind::Json)
        | (
            Kind::Int { .. },
            Kind::Decimal {
                precision: None, ..
            },
        ) => Ok(None),
        (
            Kind::Int {
                bits: fb,
                unsigned: fu,
            },
            Kind::Int {
                bits: tb,
                unsigned: tu,
            },
        ) => {
            let (fmin, fmax) = int_range(*fb, *fu);
            let (tmin, tmax) = int_range(*tb, *tu);
            if tmin <= fmin && fmax <= tmax {
                Ok(None)
            } else {
                some(format!("values outside the range of {to} are row errors"))
            }
        }
        (Kind::Float { bits: fb }, Kind::Float { bits: tb }) => {
            if tb >= fb {
                Ok(None)
            } else {
                some(format!(
                    "values are rounded to {to}; values outside its range are row errors"
                ))
            }
        }
        (
            Kind::Decimal {
                precision: fp,
                scale: fs,
            },
            Kind::Decimal {
                precision: tp,
                scale: ts,
            },
        ) => {
            let Some(tp) = tp else {
                return Ok(None);
            };
            let ts = ts.unwrap_or(0);
            let fits = fp.is_some_and(|fp| {
                let fs = fs.unwrap_or(0);
                ts >= fs && tp.saturating_sub(ts) >= fp.saturating_sub(fs)
            });
            if fits {
                Ok(None)
            } else {
                some(format!(
                    "values are rounded to {ts} decimal places; values that do not fit {to} \
                     are row errors"
                ))
            }
        }
        (Kind::Text { length: fl }, Kind::Text { length: tl }) => match tl {
            None => Ok(None),
            Some(n) if fl.is_some_and(|fl| fl <= *n) => Ok(None),
            Some(n) => some(format!(
                "texts longer than {n} characters are row errors (or cut with the truncate \
                 option)"
            )),
        },
        (Kind::Enum { labels: fl }, Kind::Enum { labels: tl }) => {
            if fl.iter().all(|l| tl.contains(l)) {
                Ok(None)
            } else {
                some(format!(
                    "values that are not one of the labels of {to} are row errors"
                ))
            }
        }
        (_, Kind::Enum { .. }) => some(format!(
            "values that are not one of the labels of {to} are row errors"
        )),
        (Kind::Binary { length: fl }, Kind::Binary { length: tl }) => match tl {
            None => Ok(None),
            Some(n) if fl.is_some_and(|fl| fl <= *n) => Ok(None),
            Some(n) => some(format!("values longer than {n} bytes are row errors")),
        },
        (Kind::Binary { .. }, Kind::Text { length }) => {
            some(text_note("written as hex text (0x...)", *length))
        }
        (_, Kind::Text { length }) => some(text_note("written as text", *length)),
        (Kind::Text { .. } | Kind::Enum { .. }, Kind::Json) => {
            some("texts must be valid JSON; other values are row errors".to_owned())
        }
        (_, Kind::Json) => some("written as JSON".to_owned()),
        (Kind::Text { .. } | Kind::Enum { .. }, Kind::Binary { .. }) => {
            some("hex text (0x...) is decoded; other values are row errors".to_owned())
        }
        (Kind::Text { .. } | Kind::Enum { .. }, _) => some(format!(
            "texts are read as {to}; values that do not parse are row errors"
        )),
        (Kind::Int { bits, .. }, Kind::Float { bits: tb }) => {
            if *bits >= 64 || (*bits >= 32 && *tb < 64) {
                some("large integers may lose precision".to_owned())
            } else {
                Ok(None)
            }
        }
        (
            Kind::Int { bits, unsigned },
            Kind::Decimal {
                precision: Some(p),
                scale,
            },
        ) => {
            if p.saturating_sub(scale.unwrap_or(0)) >= int_digits(*bits, *unsigned) {
                Ok(None)
            } else {
                some(format!("values that do not fit {to} are row errors"))
            }
        }
        (Kind::Float { .. } | Kind::Decimal { .. }, Kind::Int { .. }) => {
            some("values with a fraction or outside the range are row errors".to_owned())
        }
        (Kind::Float { .. }, Kind::Decimal { scale, .. }) => some(format!(
            "values are rounded to {} decimal places; values that do not fit {to} are row \
             errors",
            scale.unwrap_or(0)
        )),
        (Kind::Decimal { .. }, Kind::Float { .. }) => {
            some("values are rounded to the nearest binary floating-point number".to_owned())
        }
        (Kind::Bool, Kind::Int { .. }) => some("true / false are written as 1 / 0".to_owned()),
        (Kind::Int { .. }, Kind::Bool) => {
            some("0 / 1 are read as false / true; other values are row errors".to_owned())
        }
        (Kind::Date, Kind::Timestamp) => some("dates are written at midnight".to_owned()),
        (Kind::Date, Kind::TimestampTz) => some("dates are written at midnight UTC".to_owned()),
        (Kind::Timestamp, Kind::Date) => some("the time of day is dropped".to_owned()),
        (Kind::TimestampTz, Kind::Date) => {
            some("the UTC date is kept; the time of day is dropped".to_owned())
        }
        (Kind::Timestamp, Kind::TimestampTz) => {
            some("timestamps without a time zone are read as UTC".to_owned())
        }
        (Kind::TimestampTz, Kind::Timestamp) => {
            some("instants are converted to UTC (the offset is dropped)".to_owned())
        }
        (Kind::Uuid, Kind::Binary { length }) if length.is_none_or(|n| n >= 16) => {
            some("the 16 bytes of the UUID are written".to_owned())
        }
        (Kind::Binary { .. }, Kind::Uuid) => {
            some("values that are not 16 bytes long are row errors".to_owned())
        }
        _ => Err(Error::invalid_input(format!(
            "there is no conversion from {from} to {to}"
        ))),
    }
}

fn text_note(what: &str, length: Option<u32>) -> String {
    match length {
        Some(n) => format!(
            "{what}; texts longer than {n} characters are row errors (or cut with the truncate \
             option)"
        ),
        None => what.to_owned(),
    }
}

/// A value that could not be imported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CellIssue {
    /// Row index in the converted batch.
    pub batch_row: usize,
    /// Target column name.
    pub column: String,
    pub message: String,
}

/// One target column and where its values come from.
#[derive(Debug, Clone)]
struct Column {
    /// Index of the source column.
    source: usize,
    from: Kind,
    to: Kind,
    to_type: DataType,
    name: String,
    nullable: bool,
}

/// Converts source batches into load batches. See the module docs.
#[derive(Debug)]
pub(super) struct Converter {
    columns: Vec<Column>,
    options: ConversionOptions,
    formats: ValueFormats,
    /// The load schema with every field nullable (rows with issues are still in the batch).
    nullable_schema: SchemaRef,
    /// Texts cut because of `truncate_text`.
    pub truncated_texts: u64,
}

impl Converter {
    /// `columns`: per target column, the source column index, its source logical type and the
    /// target column.
    pub(super) fn new(
        columns: &[(usize, DataType, LoadColumn)],
        options: ConversionOptions,
        formats: ValueFormats,
    ) -> Self {
        let nullable_schema = Arc::new(Schema::new(
            columns
                .iter()
                .map(|(_, _, c)| Field::new(&c.name, load_arrow_type(&c.data_type), true))
                .collect::<Vec<_>>(),
        ));
        let columns = columns
            .iter()
            .map(|(source, from, target)| Column {
                source: *source,
                from: Kind::of(from),
                to: Kind::of(&target.data_type),
                to_type: target.data_type.clone(),
                name: target.name.clone(),
                nullable: target.nullable,
            })
            .collect();
        Self {
            columns,
            options,
            formats,
            nullable_schema,
            truncated_texts: 0,
        }
    }

    /// Converts `batch` (the reader's schema) into a batch of the load schema with every field
    /// nullable, and the issues found (in row order per column).
    pub(super) fn convert(&mut self, batch: &RecordBatch) -> Result<(RecordBatch, Vec<CellIssue>)> {
        let mut issues = Vec::new();
        let mut arrays = Vec::with_capacity(self.columns.len());
        for index in 0..self.columns.len() {
            let source = self.columns[index].source;
            let array = batch.columns().get(source).ok_or_else(|| {
                Error::internal(format!(
                    "the reader produced {} columns, the import needs column {}",
                    batch.num_columns(),
                    source + 1
                ))
            })?;
            arrays.push(self.convert_column(index, array.as_ref(), &mut issues)?);
        }
        let out = RecordBatch::try_new_with_options(
            Arc::clone(&self.nullable_schema),
            arrays,
            &arrow_array::RecordBatchOptions::new().with_row_count(Some(batch.num_rows())),
        )
        .map_err(|e| Error::internal(format!("building a load batch failed: {e}")))?;
        issues.sort_by_key(|i| i.batch_row);
        Ok((out, issues))
    }

    /// `true` when values of the column can be passed on unchanged.
    fn passes_through(&self, column: &Column, array: &dyn Array) -> bool {
        let same_arrow = array.data_type() == &load_arrow_type(&column.to_type);
        if !same_arrow {
            return false;
        }
        let text_options = self.options.trim_text || self.options.empty_text_as_null;
        match (&column.from, &column.to) {
            (Kind::Bool, Kind::Bool)
            | (Kind::Date, Kind::Date)
            | (Kind::Time, Kind::Time)
            | (Kind::Timestamp, Kind::Timestamp)
            | (Kind::TimestampTz, Kind::TimestampTz)
            | (Kind::Uuid, Kind::Uuid)
            | (Kind::Json, Kind::Json)
            | (Kind::Float { .. }, Kind::Float { bits: 64 })
            | (Kind::Int { .. }, Kind::Int { bits: 64, .. })
            | (Kind::Binary { .. }, Kind::Binary { length: None })
            | (
                Kind::Decimal { .. },
                Kind::Decimal {
                    precision: None, ..
                },
            ) => true,
            (Kind::Text { .. }, Kind::Text { length: None }) => !text_options,
            _ => false,
        }
    }

    fn convert_column(
        &mut self,
        index: usize,
        array: &dyn Array,
        issues: &mut Vec<CellIssue>,
    ) -> Result<ArrayRef> {
        let column = self.columns[index].clone();
        if self.passes_through(&column, array) {
            if !column.nullable && array.null_count() > 0 {
                for row in (0..array.len()).filter(|&row| array.is_null(row)) {
                    issues.push(not_null_issue(row, &column));
                }
            }
            return Ok(arrow_array::make_array(array.to_data()));
        }
        let mut out = Out::new(&load_arrow_type(&column.to_type), array.len());
        for row in 0..array.len() {
            if array.is_null(row) {
                out.append_null();
                if !column.nullable {
                    issues.push(not_null_issue(row, &column));
                }
                continue;
            }
            let value = read(array, row)?;
            let value = match value {
                Val::Str(text) if column.from.is_text() => {
                    let text = if self.options.trim_text {
                        trim_cow(text)
                    } else {
                        text
                    };
                    if self.options.empty_text_as_null && text.is_empty() {
                        out.append_null();
                        if !column.nullable {
                            issues.push(not_null_issue(row, &column));
                        }
                        continue;
                    }
                    Val::Str(text)
                }
                other => other,
            };
            match self.convert_value(&column, value) {
                Ok(value) => out.append(value)?,
                Err(message) => {
                    out.append_null();
                    issues.push(CellIssue {
                        batch_row: row,
                        column: column.name.clone(),
                        message,
                    });
                }
            }
        }
        Ok(out.finish())
    }

    /// One value into the target's load form; `Err(message)` for a row issue.
    fn convert_value<'a>(
        &mut self,
        column: &Column,
        value: Val<'a>,
    ) -> std::result::Result<Val<'a>, String> {
        let to_type = &column.to_type;
        match &column.to {
            Kind::Bool => self.to_bool(&column.from, value, to_type),
            Kind::Int { bits, unsigned } => {
                self.to_int(&column.from, &value, *bits, *unsigned, to_type)
            }
            Kind::Float { bits } => self.to_float(&column.from, &value, *bits, to_type),
            Kind::Decimal { precision, scale } => {
                self.to_decimal(&column.from, &value, *precision, *scale, to_type)
            }
            Kind::Text { length } => {
                let text = text_form(value);
                self.limit_text(text, *length)
            }
            Kind::Enum { labels } => {
                let text = text_form(value);
                if labels.is_empty() || labels.iter().any(|l| *l == *text) {
                    Ok(Val::Str(text))
                } else {
                    Err(format!(
                        "{} is not one of the values of {to_type}",
                        shown(&text)
                    ))
                }
            }
            Kind::Json => to_json(&column.from, value),
            Kind::Binary { length } => to_binary(&column.from, value, *length, to_type),
            Kind::Date => self.to_date(value, to_type),
            Kind::Time => self.to_time(value, to_type),
            Kind::Timestamp | Kind::TimestampTz => self.to_timestamp(value, to_type),
            Kind::Uuid => to_uuid(value, to_type),
        }
    }

    fn limit_text<'a>(
        &mut self,
        text: Cow<'a, str>,
        length: Option<u32>,
    ) -> std::result::Result<Val<'a>, String> {
        let Some(limit) = length.and_then(|n| usize::try_from(n).ok()) else {
            return Ok(Val::Str(text));
        };
        // Bytes bound the characters from above: most texts need no count.
        if text.len() <= limit {
            return Ok(Val::Str(text));
        }
        match text.char_indices().nth(limit) {
            None => Ok(Val::Str(text)),
            Some((cut, _)) if self.options.truncate_text => {
                self.truncated_texts += 1;
                Ok(Val::Str(match text {
                    Cow::Borrowed(t) => Cow::Borrowed(&t[..cut]),
                    Cow::Owned(mut t) => {
                        t.truncate(cut);
                        Cow::Owned(t)
                    }
                }))
            }
            Some(_) => Err(format!(
                "the text has {} characters, more than the column's {limit}",
                text.chars().count()
            )),
        }
    }

    fn to_bool<'a>(
        &self,
        from: &Kind,
        value: Val<'a>,
        to_type: &DataType,
    ) -> std::result::Result<Val<'a>, String> {
        match value {
            Val::Bool(b) => Ok(Val::Bool(b)),
            Val::I64(0) | Val::U64(0) => Ok(Val::Bool(false)),
            Val::I64(1) | Val::U64(1) => Ok(Val::Bool(true)),
            Val::Str(text) if from.is_text() => {
                let t = text.trim();
                if self
                    .formats
                    .true_values
                    .iter()
                    .any(|v| v.eq_ignore_ascii_case(t))
                {
                    Ok(Val::Bool(true))
                } else if self
                    .formats
                    .false_values
                    .iter()
                    .any(|v| v.eq_ignore_ascii_case(t))
                {
                    Ok(Val::Bool(false))
                } else {
                    Err(format!("{} is not a boolean", shown(&text)))
                }
            }
            other => Err(cannot(&other, to_type)),
        }
    }

    fn to_int<'a>(
        &self,
        from: &Kind,
        value: &Val<'a>,
        bits: u8,
        unsigned: bool,
        to_type: &DataType,
    ) -> std::result::Result<Val<'a>, String> {
        let n: i128 = match value {
            Val::I64(v) => i128::from(*v),
            Val::U64(v) => i128::from(*v),
            Val::Bool(b) => i128::from(*b),
            Val::F64(v) => float_to_int(*v)
                .ok_or_else(|| format!("{v} is not a whole number in the range of {to_type}"))?,
            Val::Str(text) => {
                let parsed = if from.is_text() {
                    parse_number(text, &self.formats)
                } else {
                    parse_canonical(text)
                };
                let number = parsed.ok_or_else(|| format!("{} is not a number", shown(text)))?;
                number.to_int().ok_or_else(|| {
                    format!(
                        "{} is not a whole number in the range of {to_type}",
                        shown(text)
                    )
                })?
            }
            other => return Err(cannot(other, to_type)),
        };
        let (min, max) = int_range(bits, unsigned);
        if n < min || n > max {
            return Err(format!("{n} is outside the range of {to_type}"));
        }
        if bits >= 64 && unsigned {
            u64::try_from(n)
                .map(Val::U64)
                .map_err(|_| format!("{n} is outside the range of {to_type}"))
        } else {
            i64::try_from(n)
                .map(Val::I64)
                .map_err(|_| format!("{n} is outside the range of {to_type}"))
        }
    }

    #[allow(
        clippy::cast_precision_loss,
        reason = "integers into a float column round, as the conversion note says"
    )]
    fn to_float<'a>(
        &self,
        from: &Kind,
        value: &Val<'a>,
        bits: u8,
        to_type: &DataType,
    ) -> std::result::Result<Val<'a>, String> {
        let v = match value {
            Val::F64(v) => *v,
            Val::I64(v) => *v as f64,
            Val::U64(v) => *v as f64,
            Val::Str(text) => {
                let normalized = if from.is_text() {
                    normalize_number_text(text, &self.formats)
                } else {
                    Some(text.trim().to_owned())
                };
                normalized
                    .and_then(|t| t.parse::<f64>().ok())
                    .ok_or_else(|| format!("{} is not a number", shown(text)))?
            }
            other => return Err(cannot(other, to_type)),
        };
        if bits < 64 && v.is_finite() && v.abs() > f64::from(f32::MAX) {
            return Err(format!("{v} is outside the range of {to_type}"));
        }
        Ok(Val::F64(v))
    }

    fn to_decimal<'a>(
        &self,
        from: &Kind,
        value: &Val<'a>,
        precision: Option<u32>,
        scale: Option<u32>,
        to_type: &DataType,
    ) -> std::result::Result<Val<'a>, String> {
        let number = match value {
            Val::I64(v) => parse_canonical(&v.to_string()),
            Val::U64(v) => parse_canonical(&v.to_string()),
            Val::F64(v) if v.is_finite() => parse_canonical(&v.to_string()),
            Val::F64(v) => return Err(format!("{v} is not a decimal number")),
            Val::Str(text) if from.is_text() => parse_number(text, &self.formats),
            Val::Str(text) => parse_canonical(text),
            other => return Err(cannot(other, to_type)),
        };
        let shown_value = || match value {
            Val::Str(text) => shown(text),
            other => text_form(other.clone()).into_owned(),
        };
        let mut number = number.ok_or_else(|| format!("{} is not a number", shown_value()))?;
        if let Some(p) = precision {
            let s = scale.unwrap_or(0);
            number.round_to(usize::try_from(s).unwrap_or(usize::MAX));
            let int_digits = number.int.len();
            let allowed = usize::try_from(p.saturating_sub(s)).unwrap_or(usize::MAX);
            if int_digits > allowed {
                return Err(format!("{} does not fit {to_type}", shown_value()));
            }
        }
        Ok(Val::Str(Cow::Owned(number.to_text())))
    }

    fn to_date<'a>(
        &self,
        value: Val<'a>,
        to_type: &DataType,
    ) -> std::result::Result<Val<'a>, String> {
        match value {
            Val::Date(d) => Ok(Val::Date(d)),
            Val::Ts { micros, .. } => i32::try_from(micros.div_euclid(MICROS_PER_DAY))
                .map(Val::Date)
                .map_err(|_| "the date is out of range".to_owned()),
            Val::Str(text) => parse_date(&text, &self.formats)
                .and_then(date_days)
                .map(Val::Date)
                .ok_or_else(|| format!("{} is not a date", shown(&text))),
            other => Err(cannot(&other, to_type)),
        }
    }

    fn to_time<'a>(
        &self,
        value: Val<'a>,
        to_type: &DataType,
    ) -> std::result::Result<Val<'a>, String> {
        match value {
            Val::Time(t) => Ok(Val::Time(t)),
            Val::Str(text) => parse_time(&text, &self.formats)
                .map(|t| Val::Time(time_micros(t)))
                .ok_or_else(|| format!("{} is not a time of day", shown(&text))),
            other => Err(cannot(&other, to_type)),
        }
    }

    fn to_timestamp<'a>(
        &self,
        value: Val<'a>,
        to_type: &DataType,
    ) -> std::result::Result<Val<'a>, String> {
        let utc = matches!(to_type, DataType::TimestampTz { .. });
        match value {
            Val::Ts { micros, .. } => Ok(Val::Ts { micros, utc }),
            Val::Date(days) => Ok(Val::Ts {
                micros: i64::from(days) * MICROS_PER_DAY,
                utc,
            }),
            Val::Str(text) => parse_timestamp(&text, &self.formats)
                .map(|micros| Val::Ts { micros, utc })
                .ok_or_else(|| format!("{} is not a timestamp", shown(&text))),
            other => Err(cannot(&other, to_type)),
        }
    }
}

fn not_null_issue(row: usize, column: &Column) -> CellIssue {
    CellIssue {
        batch_row: row,
        column: column.name.clone(),
        message: format!("column '{}' does not accept NULL", column.name),
    }
}

fn cannot(value: &Val<'_>, to_type: &DataType) -> String {
    format!(
        "{} cannot be converted to {to_type}",
        shown(&text_form(value.clone()))
    )
}

fn trim_cow(text: Cow<'_, str>) -> Cow<'_, str> {
    match text {
        Cow::Borrowed(t) => Cow::Borrowed(t.trim()),
        Cow::Owned(t) => {
            let trimmed = t.trim();
            if trimmed.len() == t.len() {
                Cow::Owned(t)
            } else {
                Cow::Owned(trimmed.to_owned())
            }
        }
    }
}

/// At most [`SHOWN_CHARS`] characters of `text`, quoted.
pub(super) fn shown(text: &str) -> String {
    match text.char_indices().nth(SHOWN_CHARS) {
        Some((cut, _)) => format!("'{}…'", &text[..cut]),
        None => format!("'{text}'"),
    }
}

fn to_json<'a>(from: &Kind, value: Val<'a>) -> std::result::Result<Val<'a>, String> {
    match value {
        Val::Str(text) if *from == Kind::Json || matches!(from, Kind::Decimal { .. }) => {
            Ok(Val::Str(text))
        }
        Val::Str(text) => {
            if serde_json::from_str::<serde::de::IgnoredAny>(&text).is_ok() {
                Ok(Val::Str(text))
            } else {
                Err(format!("{} is not valid JSON", shown(&text)))
            }
        }
        Val::Bool(b) => Ok(Val::Str(Cow::Borrowed(if b { "true" } else { "false" }))),
        Val::I64(v) => Ok(Val::Str(Cow::Owned(v.to_string()))),
        Val::U64(v) => Ok(Val::Str(Cow::Owned(v.to_string()))),
        Val::F64(v) if v.is_finite() => Ok(Val::Str(Cow::Owned(v.to_string()))),
        Val::F64(v) => Err(format!("{v} cannot be written as JSON")),
        other => {
            let text = text_form(other);
            serde_json::to_string(text.as_ref())
                .map(|t| Val::Str(Cow::Owned(t)))
                .map_err(|e| e.to_string())
        }
    }
}

fn to_binary<'a>(
    from: &Kind,
    value: Val<'a>,
    length: Option<u32>,
    to_type: &DataType,
) -> std::result::Result<Val<'a>, String> {
    let bytes: Cow<'a, [u8]> = match value {
        Val::Bytes(b) => b,
        Val::Uuid(u) => Cow::Owned(u.to_vec()),
        Val::Str(text) if from.is_text() => Cow::Owned(
            parse_hex(&text).ok_or_else(|| format!("{} is not hex text (0x...)", shown(&text)))?,
        ),
        other => return Err(cannot(&other, to_type)),
    };
    if let Some(limit) = length.and_then(|n| usize::try_from(n).ok())
        && bytes.len() > limit
    {
        return Err(format!(
            "the value has {} bytes, more than the column's {limit}",
            bytes.len()
        ));
    }
    Ok(Val::Bytes(bytes))
}

fn to_uuid<'a>(value: Val<'a>, to_type: &DataType) -> std::result::Result<Val<'a>, String> {
    match value {
        Val::Uuid(u) => Ok(Val::Uuid(u)),
        Val::Str(text) => uuid::Uuid::parse_str(text.trim())
            .map(|u| Val::Uuid(*u.as_bytes()))
            .map_err(|_| format!("{} is not a UUID", shown(&text))),
        Val::Bytes(b) => <[u8; 16]>::try_from(b.as_ref())
            .map(Val::Uuid)
            .map_err(|_| format!("a UUID has 16 bytes, the value has {}", b.len())),
        other => Err(cannot(&other, to_type)),
    }
}

/// One value on its way from the source to the load batch. Text and bytes borrow from the
/// source array when they pass unchanged.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Val<'a> {
    Bool(bool),
    I64(i64),
    U64(u64),
    F64(f64),
    Str(Cow<'a, str>),
    Bytes(Cow<'a, [u8]>),
    /// Days since 1970-01-01.
    Date(i32),
    /// Microseconds since midnight.
    Time(i64),
    /// Microseconds since 1970-01-01 00:00 (UTC for `utc`).
    Ts {
        micros: i64,
        utc: bool,
    },
    Uuid([u8; 16]),
}

/// Row `row` (not NULL) of `array`.
fn read(array: &dyn Array, row: usize) -> Result<Val<'_>> {
    Ok(match array.data_type() {
        ArrowType::Boolean => Val::Bool(array.as_boolean().value(row)),
        ArrowType::Int64 => Val::I64(array.as_primitive::<Int64Type>().value(row)),
        ArrowType::UInt64 => Val::U64(array.as_primitive::<UInt64Type>().value(row)),
        ArrowType::Float64 => Val::F64(array.as_primitive::<Float64Type>().value(row)),
        ArrowType::Utf8 => Val::Str(Cow::Borrowed(array.as_string::<i32>().value(row))),
        ArrowType::LargeUtf8 => Val::Str(Cow::Borrowed(array.as_string::<i64>().value(row))),
        ArrowType::Binary => Val::Bytes(Cow::Borrowed(array.as_binary::<i32>().value(row))),
        ArrowType::Date32 => Val::Date(array.as_primitive::<Date32Type>().value(row)),
        ArrowType::Time64(TimeUnit::Microsecond) => {
            Val::Time(array.as_primitive::<Time64MicrosecondType>().value(row))
        }
        ArrowType::Timestamp(TimeUnit::Microsecond, zone) => Val::Ts {
            micros: array.as_primitive::<TimestampMicrosecondType>().value(row),
            utc: zone.is_some(),
        },
        ArrowType::FixedSizeBinary(16) => {
            let bytes = array.as_fixed_size_binary().value(row);
            Val::Uuid(
                <[u8; 16]>::try_from(bytes)
                    .map_err(|_| Error::internal("a UUID cell is not 16 bytes"))?,
            )
        }
        _ => from_value(cell_value(array, row)?)?,
    })
}

fn from_value(value: Value) -> Result<Val<'static>> {
    Ok(match value {
        Value::Null => return Err(Error::internal("a NULL cell reached the conversion")),
        Value::Bool(b) => Val::Bool(b),
        Value::Int(v) => Val::I64(v),
        Value::UInt(v) => Val::U64(v),
        Value::Float(v) => Val::F64(v),
        Value::Decimal(t) | Value::Text(t) => Val::Str(Cow::Owned(t)),
        Value::Bytes(b) => Val::Bytes(Cow::Owned(b)),
        Value::Date(d) => {
            Val::Date(date_days(d).ok_or_else(|| Error::internal("a date is out of range"))?)
        }
        Value::Time(t) => Val::Time(time_micros(t)),
        Value::Timestamp(t) => Val::Ts {
            micros: t.and_utc().timestamp_micros(),
            utc: false,
        },
        Value::TimestampTz(t) => Val::Ts {
            micros: t.timestamp_micros(),
            utc: true,
        },
        Value::Uuid(u) => Val::Uuid(*u.as_bytes()),
        Value::Json(j) => Val::Str(Cow::Owned(j.to_string())),
    })
}

fn epoch() -> NaiveDate {
    NaiveDate::from_ymd_opt(1970, 1, 1).unwrap_or_default()
}

fn date_days(date: NaiveDate) -> Option<i32> {
    i32::try_from(date.signed_duration_since(epoch()).num_days()).ok()
}

fn time_micros(time: NaiveTime) -> i64 {
    let micros = i64::from(time.nanosecond().min(999_999_999) / 1_000);
    i64::from(time.num_seconds_from_midnight()) * MICROS_PER_SECOND + micros
}

/// The text form of a value (texts as they are; others as the CSV writer writes them).
pub(super) fn text_form(value: Val<'_>) -> Cow<'_, str> {
    match value {
        Val::Str(text) => text,
        Val::Bool(b) => Cow::Borrowed(if b { "true" } else { "false" }),
        Val::I64(v) => Cow::Owned(v.to_string()),
        Val::U64(v) => Cow::Owned(v.to_string()),
        Val::F64(v) => Cow::Owned(v.to_string()),
        Val::Bytes(b) => Cow::Owned(hex(&b)),
        Val::Date(days) => Cow::Owned(
            DateTime::from_timestamp(i64::from(days) * 86_400, 0)
                .map_or_else(|| days.to_string(), |d| d.date_naive().to_string()),
        ),
        Val::Time(micros) => Cow::Owned(
            u32::try_from(micros.div_euclid(MICROS_PER_SECOND))
                .ok()
                .zip(u32::try_from(micros.rem_euclid(MICROS_PER_SECOND) * 1_000).ok())
                .and_then(|(s, n)| NaiveTime::from_num_seconds_from_midnight_opt(s, n))
                .map_or_else(
                    || micros.to_string(),
                    |t| t.format("%H:%M:%S%.f").to_string(),
                ),
        ),
        Val::Ts { micros, utc } => Cow::Owned(DateTime::from_timestamp_micros(micros).map_or_else(
            || micros.to_string(),
            |t| {
                let text = t.naive_utc().format("%Y-%m-%d %H:%M:%S%.f").to_string();
                if utc { format!("{text}Z") } else { text }
            },
        )),
        Val::Uuid(u) => Cow::Owned(uuid::Uuid::from_bytes(u).hyphenated().to_string()),
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(2 + bytes.len() * 2);
    out.push_str("0x");
    for b in bytes {
        out.push(char::from(DIGITS[usize::from(b >> 4)]));
        out.push(char::from(DIGITS[usize::from(b & 0x0f)]));
    }
    out
}

/// Hex text with a `0x` / `\x` prefix (or none) as bytes.
fn parse_hex(text: &str) -> Option<Vec<u8>> {
    let t = text.trim();
    let digits = t
        .strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .or_else(|| t.strip_prefix("\\x"))
        .unwrap_or(t);
    if !digits.len().is_multiple_of(2) {
        return None;
    }
    let nibble = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    digits
        .as_bytes()
        .chunks(2)
        .map(|pair| Some((nibble(pair[0])? << 4) | nibble(pair[1])?))
        .collect()
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "the value is whole and inside the i128 range checked just before"
)]
fn float_to_int(v: f64) -> Option<i128> {
    if !v.is_finite() || v.fract().abs() > 0.0 || v.abs() > 1e30 {
        return None;
    }
    Some(v as i128)
}

/// A decimal number as digit strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Number {
    pub negative: bool,
    /// Integer digits without leading zeros (empty for 0).
    pub int: String,
    /// Fraction digits (trailing zeros kept).
    pub frac: String,
}

impl Number {
    fn is_zero(&self) -> bool {
        self.int.is_empty() && self.frac.bytes().all(|b| b == b'0')
    }

    /// The integer value, when there is no fraction and it fits `i128`.
    fn to_int(&self) -> Option<i128> {
        if self.frac.bytes().any(|b| b != b'0') || self.int.len() > 38 {
            return None;
        }
        let magnitude: i128 = if self.int.is_empty() {
            0
        } else {
            self.int.parse().ok()?
        };
        Some(if self.negative { -magnitude } else { magnitude })
    }

    /// Rounds half away from zero to `scale` fraction digits.
    fn round_to(&mut self, scale: usize) {
        if self.frac.len() <= scale {
            return;
        }
        let round_up = self.frac.as_bytes()[scale] >= b'5';
        self.frac.truncate(scale);
        if !round_up {
            return;
        }
        let mut digits: Vec<u8> = self.int.bytes().chain(self.frac.bytes()).collect();
        let mut carry = true;
        for d in digits.iter_mut().rev() {
            if *d == b'9' {
                *d = b'0';
            } else {
                *d += 1;
                carry = false;
                break;
            }
        }
        if carry {
            digits.insert(0, b'1');
        }
        let split = digits.len() - self.frac.len();
        let (int, frac) = digits.split_at(split);
        String::from_utf8_lossy(int)
            .trim_start_matches('0')
            .clone_into(&mut self.int);
        self.frac = String::from_utf8_lossy(frac).into_owned();
    }

    /// Canonical text: `-123.45`, `0.5`, `12`.
    pub(super) fn to_text(&self) -> String {
        let mut out = String::with_capacity(self.int.len() + self.frac.len() + 3);
        if self.negative && !self.is_zero() {
            out.push('-');
        }
        if self.int.is_empty() {
            out.push('0');
        } else {
            out.push_str(&self.int);
        }
        if !self.frac.is_empty() {
            out.push('.');
            out.push_str(&self.frac);
        }
        out
    }
}

/// Number text with the user's separators (`1.234,5` with `,` as the decimal separator) in
/// the canonical form `parse_canonical` takes.
fn normalize_number_text(text: &str, formats: &ValueFormats) -> Option<String> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(t.len());
    for c in t.chars() {
        if Some(c) == formats.thousands_separator {
            continue;
        }
        if c == formats.decimal_separator {
            out.push('.');
        } else {
            out.push(c);
        }
    }
    Some(out)
}

/// A number typed by a user (separators of `formats`).
pub(super) fn parse_number(text: &str, formats: &ValueFormats) -> Option<Number> {
    parse_canonical(&normalize_number_text(text, formats)?)
}

/// `[+-]digits[.digits][e[+-]digits]`.
pub(super) fn parse_canonical(text: &str) -> Option<Number> {
    let t = text.trim();
    let (negative, rest) = match t.as_bytes().first()? {
        b'-' => (true, &t[1..]),
        b'+' => (false, &t[1..]),
        _ => (false, t),
    };
    let (mantissa, exponent) = match rest.find(['e', 'E']) {
        Some(at) => (&rest[..at], rest[at + 1..].parse::<i32>().ok()?),
        None => (rest, 0),
    };
    let (int, frac) = match mantissa.find('.') {
        Some(at) => (&mantissa[..at], &mantissa[at + 1..]),
        None => (mantissa, ""),
    };
    if (int.is_empty() && frac.is_empty())
        || !int.bytes().all(|b| b.is_ascii_digit())
        || !frac.bytes().all(|b| b.is_ascii_digit())
        || exponent.unsigned_abs() > 400
    {
        return None;
    }
    let mut digits = String::with_capacity(int.len() + frac.len());
    digits.push_str(int);
    digits.push_str(frac);
    // The decimal point sits after `point` digits.
    let point = i64::try_from(int.len()).ok()? + i64::from(exponent);
    let (int, frac) = if point <= 0 {
        let zeros = usize::try_from(-point).ok()?;
        (String::new(), format!("{}{digits}", "0".repeat(zeros)))
    } else {
        let point = usize::try_from(point).ok()?;
        if point >= digits.len() {
            let zeros = point - digits.len();
            (format!("{digits}{}", "0".repeat(zeros)), String::new())
        } else {
            (digits[..point].to_owned(), digits[point..].to_owned())
        }
    };
    Some(Number {
        negative,
        int: int.trim_start_matches('0').to_owned(),
        frac,
    })
}

fn parse_date(text: &str, formats: &ValueFormats) -> Option<NaiveDate> {
    let t = text.trim();
    match &formats.date_format {
        Some(format) => NaiveDate::parse_from_str(t, format).ok(),
        None => NaiveDate::parse_from_str(t, "%Y-%m-%d").ok().or_else(|| {
            // A timestamp's date part.
            parse_naive_timestamp(t).map(|ts| ts.date())
        }),
    }
}

fn parse_time(text: &str, formats: &ValueFormats) -> Option<NaiveTime> {
    let t = text.trim();
    match &formats.time_format {
        Some(format) => NaiveTime::parse_from_str(t, format).ok(),
        None => NaiveTime::parse_from_str(t, "%H:%M:%S%.f")
            .or_else(|_| NaiveTime::parse_from_str(t, "%H:%M"))
            .ok(),
    }
}

fn parse_naive_timestamp(t: &str) -> Option<NaiveDateTime> {
    [
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%dT%H:%M",
    ]
    .iter()
    .find_map(|f| NaiveDateTime::parse_from_str(t, f).ok())
}

/// Microseconds since the epoch: an instant (UTC) when the text has an offset or `Z`, else the
/// wall time as written.
pub(super) fn parse_timestamp(text: &str, formats: &ValueFormats) -> Option<i64> {
    let t = text.trim();
    if let Some(format) = &formats.timestamp_format {
        if let Ok(instant) = DateTime::parse_from_str(t, format) {
            return Some(instant.timestamp_micros());
        }
        return NaiveDateTime::parse_from_str(t, format)
            .ok()
            .map(|n| n.and_utc().timestamp_micros());
    }
    if let Some(naive) = t.strip_suffix(['Z', 'z']).and_then(parse_naive_timestamp) {
        return Some(naive.and_utc().timestamp_micros());
    }
    for format in ["%Y-%m-%d %H:%M:%S%.f%#z", "%Y-%m-%dT%H:%M:%S%.f%#z"] {
        if let Ok(instant) = DateTime::parse_from_str(t, format) {
            return Some(instant.timestamp_micros());
        }
    }
    if let Some(naive) = parse_naive_timestamp(t) {
        return Some(naive.and_utc().timestamp_micros());
    }
    NaiveDate::parse_from_str(t, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|n| n.and_utc().timestamp_micros())
}

/// A builder of one load-schema column.
enum Out {
    Bool(BooleanBuilder),
    I64(Int64Builder),
    U64(UInt64Builder),
    F64(Float64Builder),
    Str(StringBuilder),
    Bytes(BinaryBuilder),
    Date(Date32Builder),
    Time(Time64MicrosecondBuilder),
    Ts(TimestampMicrosecondBuilder, Option<Arc<str>>),
    Uuid(FixedSizeBinaryBuilder),
}

impl Out {
    fn new(arrow: &ArrowType, rows: usize) -> Self {
        match arrow {
            ArrowType::Boolean => Self::Bool(BooleanBuilder::with_capacity(rows)),
            ArrowType::UInt64 => Self::U64(UInt64Builder::with_capacity(rows)),
            ArrowType::Float64 => Self::F64(Float64Builder::with_capacity(rows)),
            ArrowType::Binary => Self::Bytes(BinaryBuilder::with_capacity(rows, rows * 16)),
            ArrowType::Date32 => Self::Date(Date32Builder::with_capacity(rows)),
            ArrowType::Time64(_) => Self::Time(Time64MicrosecondBuilder::with_capacity(rows)),
            ArrowType::Timestamp(_, zone) => Self::Ts(
                TimestampMicrosecondBuilder::with_capacity(rows),
                zone.clone(),
            ),
            ArrowType::FixedSizeBinary(_) => {
                Self::Uuid(FixedSizeBinaryBuilder::with_capacity(rows, 16))
            }
            ArrowType::Int64 => Self::I64(Int64Builder::with_capacity(rows)),
            // Utf8 and anything unexpected.
            _ => Self::Str(StringBuilder::with_capacity(rows, rows * 16)),
        }
    }

    fn append_null(&mut self) {
        match self {
            Self::Bool(b) => b.append_null(),
            Self::I64(b) => b.append_null(),
            Self::U64(b) => b.append_null(),
            Self::F64(b) => b.append_null(),
            Self::Str(b) => b.append_null(),
            Self::Bytes(b) => b.append_null(),
            Self::Date(b) => b.append_null(),
            Self::Time(b) => b.append_null(),
            Self::Ts(b, _) => b.append_null(),
            Self::Uuid(b) => b.append_null(),
        }
    }

    fn append(&mut self, value: Val<'_>) -> Result<()> {
        match (self, value) {
            (Self::Bool(b), Val::Bool(v)) => b.append_value(v),
            (Self::I64(b), Val::I64(v)) => b.append_value(v),
            (Self::U64(b), Val::U64(v)) => b.append_value(v),
            (Self::F64(b), Val::F64(v)) => b.append_value(v),
            (Self::Str(b), Val::Str(v)) => b.append_value(v),
            (Self::Bytes(b), Val::Bytes(v)) => b.append_value(v),
            (Self::Date(b), Val::Date(v)) => b.append_value(v),
            (Self::Time(b), Val::Time(v)) => b.append_value(v),
            (Self::Ts(b, _), Val::Ts { micros, .. }) => b.append_value(micros),
            (Self::Uuid(b), Val::Uuid(v)) => b
                .append_value(v)
                .map_err(|e| Error::internal(format!("building a UUID column failed: {e}")))?,
            (_, value) => {
                return Err(Error::internal(format!(
                    "the conversion produced {value:?} for a column of another type"
                )));
            }
        }
        Ok(())
    }

    fn finish(self) -> ArrayRef {
        match self {
            Self::Bool(mut b) => Arc::new(b.finish()),
            Self::I64(mut b) => Arc::new(b.finish()),
            Self::U64(mut b) => Arc::new(b.finish()),
            Self::F64(mut b) => Arc::new(b.finish()),
            Self::Str(mut b) => Arc::new(b.finish()),
            Self::Bytes(mut b) => Arc::new(b.finish()),
            Self::Date(mut b) => Arc::new(b.finish()),
            Self::Time(mut b) => Arc::new(b.finish()),
            Self::Ts(mut b, zone) => Arc::new(b.finish().with_timezone_opt(zone)),
            Self::Uuid(mut b) => Arc::new(b.finish()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::{BooleanArray, Int64Array, StringArray};
    use pretty_assertions::assert_eq;

    fn dec(precision: u32, scale: u32) -> DataType {
        DataType::Decimal {
            precision: Some(precision),
            scale: Some(scale),
        }
    }

    #[test]
    fn notes_follow_the_rules() {
        // Unchanged.
        assert_eq!(note(&DataType::INT32, &DataType::INT64).unwrap(), None);
        assert_eq!(note(&DataType::Text, &DataType::Text).unwrap(), None);
        assert_eq!(note(&dec(10, 2), &dec(12, 2)).unwrap(), None);
        assert_eq!(note(&DataType::Date, &DataType::Date).unwrap(), None);
        // Notes.
        assert!(note(&DataType::INT64, &DataType::INT32).unwrap().is_some());
        assert!(note(&DataType::Text, &DataType::INT64).unwrap().is_some());
        assert!(note(&DataType::FLOAT64, &dec(10, 2)).unwrap().is_some());
        assert!(
            note(&DataType::Timestamp { precision: None }, &DataType::Date)
                .unwrap()
                .unwrap()
                .contains("time of day")
        );
        assert!(note(&DataType::Bool, &DataType::INT32).unwrap().is_some());
        assert!(note(&DataType::Uuid, &DataType::Text).unwrap().is_some());
        assert!(
            note(&DataType::INT64, &DataType::Json { binary: true })
                .unwrap()
                .is_some()
        );
        assert!(
            note(
                &DataType::VarBinary { length: None },
                &DataType::VarChar { length: Some(10) }
            )
            .unwrap()
            .unwrap()
            .contains("hex")
        );
        // No conversion.
        let err = note(&DataType::VarBinary { length: None }, &DataType::INT64).unwrap_err();
        assert_eq!(err.category(), oxbase_types::ErrorCategory::InvalidInput);
        assert!(note(&DataType::Json { binary: false }, &DataType::Date).is_err());
        assert!(note(&DataType::Date, &DataType::Bool).is_err());
    }

    #[test]
    fn numbers_parse_and_round() {
        let n = parse_canonical("-0012.345").unwrap();
        assert_eq!(n.to_text(), "-12.345");
        let mut r = n.clone();
        r.round_to(2);
        assert_eq!(r.to_text(), "-12.35");
        let mut nines = parse_canonical("99.995").unwrap();
        nines.round_to(2);
        assert_eq!(nines.to_text(), "100.00");
        assert_eq!(parse_canonical("1.5e3").unwrap().to_text(), "1500");
        assert_eq!(parse_canonical("25e-3").unwrap().to_text(), "0.025");
        assert_eq!(parse_canonical("-0.00").unwrap().to_text(), "0.00");
        assert!(parse_canonical("1.2.3").is_none());
        assert!(parse_canonical("abc").is_none());
        assert!(parse_canonical("").is_none());
        let formats = ValueFormats {
            decimal_separator: ',',
            thousands_separator: Some('.'),
            ..ValueFormats::default()
        };
        assert_eq!(
            parse_number("1.234,56", &formats).unwrap().to_text(),
            "1234.56"
        );
        assert_eq!(
            parse_canonical("9007199254740993").unwrap().to_int(),
            Some(9_007_199_254_740_993)
        );
        assert_eq!(parse_canonical("12.00").unwrap().to_int(), Some(12));
        assert_eq!(parse_canonical("12.5").unwrap().to_int(), None);
    }

    #[test]
    fn timestamps_parse_with_and_without_offsets() {
        let f = ValueFormats::default();
        let base = parse_timestamp("2024-01-02 03:04:05.123456", &f).unwrap();
        assert_eq!(
            parse_timestamp("2024-01-02T03:04:05.123456Z", &f),
            Some(base)
        );
        assert_eq!(
            parse_timestamp("2024-01-02 05:04:05.123456+02:00", &f),
            Some(base)
        );
        assert_eq!(
            parse_timestamp("2024-01-02", &f),
            Some(base - (3 * 3600 + 4 * 60 + 5) * MICROS_PER_SECOND - 123_456)
        );
        assert_eq!(parse_timestamp("not a time", &f), None);
        assert_eq!(parse_hex("0x00ff10"), Some(vec![0, 255, 16]));
        assert_eq!(parse_hex("0xf"), None);
        assert_eq!(hex(&[0, 255, 16]), "0x00ff10");
    }

    fn load(name: &str, data_type: DataType, nullable: bool) -> LoadColumn {
        LoadColumn {
            name: name.into(),
            data_type,
            native_type: String::new(),
            nullable,
        }
    }

    #[test]
    fn converter_checks_and_reports_issues() {
        let schema = Arc::new(Schema::new(vec![
            Field::new("a", ArrowType::Utf8, true),
            Field::new("b", ArrowType::Int64, true),
            Field::new("c", ArrowType::Boolean, true),
        ]));
        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(StringArray::from(vec![
                    Some(" 7 "),
                    Some("x"),
                    None,
                    Some(""),
                ])),
                Arc::new(Int64Array::from(vec![Some(1), Some(300), Some(-1), None])),
                Arc::new(BooleanArray::from(vec![
                    Some(true),
                    Some(false),
                    None,
                    Some(true),
                ])),
            ],
        )
        .unwrap();
        let columns = vec![
            (0, DataType::Text, load("a_int", DataType::INT32, true)),
            (
                1,
                DataType::INT64,
                load(
                    "b_small",
                    DataType::Int {
                        bits: 8,
                        unsigned: true,
                    },
                    true,
                ),
            ),
            (2, DataType::Bool, load("c", DataType::Bool, false)),
            (
                0,
                DataType::Text,
                load("a_text", DataType::VarChar { length: Some(2) }, true),
            ),
        ];
        let options = ConversionOptions {
            trim_text: true,
            empty_text_as_null: true,
            truncate_text: false,
        };
        let mut converter = Converter::new(&columns, options, ValueFormats::default());
        let (out, issues) = converter.convert(&batch).unwrap();
        assert_eq!(out.num_rows(), 4);
        let ints = out.column(0).as_primitive::<Int64Type>();
        assert_eq!(ints.value(0), 7);
        assert!(ints.is_null(1) && ints.is_null(2) && ints.is_null(3));
        let small = out.column(1).as_primitive::<Int64Type>();
        assert_eq!(small.value(0), 1);
        assert!(small.is_null(1) && small.is_null(2));
        let rows: Vec<(usize, &str)> = issues
            .iter()
            .map(|i| (i.batch_row, i.column.as_str()))
            .collect();
        assert_eq!(
            rows,
            [(1, "a_int"), (1, "b_small"), (2, "b_small"), (2, "c"),]
        );
        let texts = out.column(3).as_string::<i32>();
        assert_eq!(texts.value(0), "7");
        assert_eq!(texts.value(1), "x");

        // Too long without truncation; cut with it.
        let long = RecordBatch::try_new(
            Arc::new(Schema::new(vec![Field::new("t", ArrowType::Utf8, true)])),
            vec![Arc::new(StringArray::from(vec!["héllo"]))],
        )
        .unwrap();
        let columns = vec![(
            0,
            DataType::Text,
            load("t", DataType::VarChar { length: Some(2) }, true),
        )];
        let mut strict = Converter::new(
            &columns,
            ConversionOptions::default(),
            ValueFormats::default(),
        );
        let (_, issues) = strict.convert(&long).unwrap();
        assert_eq!(issues.len(), 1);
        let cutting = ConversionOptions {
            truncate_text: true,
            ..ConversionOptions::default()
        };
        let mut cut = Converter::new(&columns, cutting, ValueFormats::default());
        let (out, issues) = cut.convert(&long).unwrap();
        assert_eq!(issues.len(), 0, "{issues:?}");
        assert_eq!(out.column(0).as_string::<i32>().value(0), "hé");
        assert_eq!(cut.truncated_texts, 1);
    }

    #[test]
    fn converter_writes_canonical_load_types() {
        let schema = Arc::new(Schema::new(vec![Field::new("t", ArrowType::Utf8, true)]));
        let batch = RecordBatch::try_new(
            schema,
            vec![Arc::new(StringArray::from(vec![
                "2024-05-06 07:08:09.5",
                "12.345",
                "550e8400-e29b-41d4-a716-446655440000",
            ]))],
        )
        .unwrap();
        let columns = vec![
            (
                0,
                DataType::Text,
                load("ts", DataType::TimestampTz { precision: Some(6) }, true),
            ),
            (0, DataType::Text, load("d", dec(5, 2), true)),
            (0, DataType::Text, load("u", DataType::Uuid, true)),
        ];
        let mut converter = Converter::new(
            &columns,
            ConversionOptions::default(),
            ValueFormats::default(),
        );
        let (out, issues) = converter.convert(&batch).unwrap();
        assert_eq!(
            out.column(0).data_type(),
            &ArrowType::Timestamp(TimeUnit::Microsecond, Some("+00:00".into()))
        );
        assert_eq!(out.column(1).as_string::<i32>().value(1), "12.35");
        assert_eq!(out.column(2).data_type(), &ArrowType::FixedSizeBinary(16));
        assert!(!out.column(2).is_null(2));
        // Row 0 is not a decimal / UUID, row 1 not a timestamp, etc.
        assert!(issues.iter().any(|i| i.batch_row == 0 && i.column == "d"));
        assert!(issues.iter().any(|i| i.batch_row == 1 && i.column == "ts"));
        assert!(!issues.iter().any(|i| i.batch_row == 2 && i.column == "u"));
        assert_eq!(text_form(Val::Bytes(Cow::Borrowed(&[1, 2]))), "0x0102");
        assert_eq!(shown(&"x".repeat(300)).chars().count(), 203);
    }
}
