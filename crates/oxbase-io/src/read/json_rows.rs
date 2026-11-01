//! JSON reading (rules: module docs of [`crate::json_file`]).
//!
//! A byte scanner over the decoded text finds one element at a time: in an array, by tracking
//! strings, escapes and nesting depth; in NDJSON, one non-blank line. Each element is
//! validated with `serde_json` and its top-level members are split by hand, so numbers keep
//! the exact text they were written with (`9007199254740993`, `12.50`) and nested values
//! become compact JSON text. Memory: one batch plus the largest element (at most 64 MiB).

use std::collections::HashMap;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use arrow_array::RecordBatch;
use arrow_schema::SchemaRef;
use oxbase_driver::CancellationToken;
use oxbase_types::{DataType, Error, Result};

use super::column::{Cell, ColumnBuilder};
use super::text::{consumed, encoding_name, open_text};
use super::values::is_json;
use super::{MAX_ROW_BYTES, Progress, check_cancel, display, invalid, io_error, unique_names};
use crate::encoding::{CountingReader, DecodingReader};
use crate::infer::TypeInference;
use crate::{
    ColumnSpec, Detected, JsonLayout, JsonReadOptions, PreviewColumn, PreviewLimits, RowIssue,
    SourceBatch, SourcePreview, SourceReader, ValueFormats,
};

type Decoded = DecodingReader<CountingReader<std::fs::File>>;

fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

/// Buffered bytes with line / column tracking.
struct Scanner {
    input: Decoded,
    buf: Vec<u8>,
    pos: usize,
    len: usize,
    eof: bool,
    line: u64,
    column: u64,
}

impl Scanner {
    fn new(input: Decoded) -> Self {
        Self {
            input,
            buf: vec![0; 64 * 1024],
            pos: 0,
            len: 0,
            eof: false,
            line: 1,
            column: 1,
        }
    }

    fn peek(&mut self) -> io::Result<Option<u8>> {
        if self.pos == self.len && !self.eof {
            let n = loop {
                match self.input.read(&mut self.buf) {
                    Ok(n) => break n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            };
            self.pos = 0;
            self.len = n;
            self.eof = n == 0;
        }
        Ok((self.pos < self.len).then(|| self.buf[self.pos]))
    }

    fn bump(&mut self) -> io::Result<Option<u8>> {
        let b = self.peek()?;
        if let Some(b) = b {
            self.pos += 1;
            if b == b'\n' {
                self.line += 1;
                self.column = 1;
            } else {
                self.column += 1;
            }
        }
        Ok(b)
    }

    fn skip_space(&mut self) -> io::Result<()> {
        while let Some(b) = self.peek()? {
            if !is_space(b) {
                break;
            }
            self.bump()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    Array,
    Lines,
}

/// Elements of a JSON file, one at a time.
struct JsonSource {
    scanner: Scanner,
    layout: Layout,
    started: bool,
    first: bool,
    finished: bool,
    elem: Vec<u8>,
    /// Line where the current element starts.
    elem_line: u64,
    /// Elements read so far.
    index: u64,
    counter: Arc<AtomicU64>,
    total_bytes: u64,
    detected: Detected,
    path: PathBuf,
}

impl JsonSource {
    fn open(path: &Path, o: &JsonReadOptions) -> Result<Self> {
        let input = open_text(path, o.encoding)?;
        let mut scanner = Scanner::new(input.reader);
        let layout = match o.layout {
            JsonLayout::Array => Layout::Array,
            JsonLayout::Lines => Layout::Lines,
            JsonLayout::Auto => {
                scanner.skip_space().map_err(|e| io_error(path, &e))?;
                if scanner.peek().map_err(|e| io_error(path, &e))? == Some(b'[') {
                    Layout::Array
                } else {
                    Layout::Lines
                }
            }
        };
        Ok(Self {
            scanner,
            layout,
            started: false,
            first: true,
            finished: false,
            elem: Vec::new(),
            elem_line: 1,
            index: 0,
            counter: input.counter,
            total_bytes: input.total_bytes,
            detected: Detected {
                encoding: Some(input.encoding),
                bom: input.bom,
                json_lines: Some(layout == Layout::Lines),
                ..Detected::default()
            },
            path: path.to_path_buf(),
        })
    }

    fn io(&self, e: &io::Error) -> Error {
        io_error(&self.path, e)
    }

    fn at(&self) -> String {
        format!("line {}, column {}", self.scanner.line, self.scanner.column)
    }

    /// Reads the next element into `elem`; `false` at the end.
    fn next_element(&mut self) -> Result<bool> {
        if self.finished {
            return Ok(false);
        }
        let found = match self.layout {
            Layout::Array => self.next_in_array(),
            Layout::Lines => self.next_line(),
        };
        match found {
            Ok(true) => {
                self.index += 1;
                Ok(true)
            }
            Ok(false) => {
                self.finished = true;
                Ok(false)
            }
            Err(e) => {
                self.finished = true;
                Err(e)
            }
        }
    }

    fn next_in_array(&mut self) -> Result<bool> {
        if !self.started {
            self.scanner.skip_space().map_err(|e| self.io(&e))?;
            match self.scanner.bump().map_err(|e| self.io(&e))? {
                Some(b'[') => self.started = true,
                None => return Ok(false),
                Some(_) => {
                    return Err(invalid(
                        &self.path,
                        format!("{}: expected a JSON array ('[')", self.at()),
                    ));
                }
            }
        }
        self.scanner.skip_space().map_err(|e| self.io(&e))?;
        if self.first {
            if self.scanner.peek().map_err(|e| self.io(&e))? == Some(b']') {
                self.scanner.bump().map_err(|e| self.io(&e))?;
                self.end_of_array()?;
                return Ok(false);
            }
        } else {
            match self.scanner.bump().map_err(|e| self.io(&e))? {
                Some(b',') => self.scanner.skip_space().map_err(|e| self.io(&e))?,
                Some(b']') => {
                    self.end_of_array()?;
                    return Ok(false);
                }
                Some(_) => {
                    return Err(invalid(
                        &self.path,
                        format!(
                            "{}: expected ',' or ']' after element {}",
                            self.at(),
                            self.index
                        ),
                    ));
                }
                None => {
                    return Err(invalid(
                        &self.path,
                        format!(
                            "the file ends inside the array, after element {}",
                            self.index
                        ),
                    ));
                }
            }
        }
        self.first = false;
        self.read_value()?;
        Ok(true)
    }

    fn end_of_array(&mut self) -> Result<()> {
        self.scanner.skip_space().map_err(|e| self.io(&e))?;
        if self.scanner.peek().map_err(|e| self.io(&e))?.is_some() {
            return Err(invalid(
                &self.path,
                format!("{}: unexpected text after the array", self.at()),
            ));
        }
        Ok(())
    }

    fn push(&mut self, b: u8) -> Result<()> {
        self.elem.push(b);
        if u64::try_from(self.elem.len()).unwrap_or(u64::MAX) > MAX_ROW_BYTES {
            return Err(invalid(
                &self.path,
                format!(
                    "element {} (line {}) is larger than {} MiB",
                    self.index + 1,
                    self.elem_line,
                    MAX_ROW_BYTES / (1024 * 1024)
                ),
            ));
        }
        Ok(())
    }

    fn take(&mut self) -> Result<u8> {
        match self.scanner.bump().map_err(|e| self.io(&e))? {
            Some(b) => {
                self.push(b)?;
                Ok(b)
            }
            None => Err(invalid(
                &self.path,
                format!(
                    "the file ends inside element {} (it starts at line {})",
                    self.index + 1,
                    self.elem_line
                ),
            )),
        }
    }

    /// One value of the array into `elem`.
    fn read_value(&mut self) -> Result<()> {
        self.elem.clear();
        self.elem_line = self.scanner.line;
        let first = self.scanner.peek().map_err(|e| self.io(&e))?;
        match first {
            None => {
                return Err(invalid(
                    &self.path,
                    format!(
                        "the file ends inside the array, after element {}",
                        self.index
                    ),
                ));
            }
            Some(b'{' | b'[') => {
                let mut depth = 0_u64;
                let mut in_string = false;
                let mut escaped = false;
                loop {
                    let b = self.take()?;
                    if in_string {
                        if escaped {
                            escaped = false;
                        } else if b == b'\\' {
                            escaped = true;
                        } else if b == b'"' {
                            in_string = false;
                        }
                        continue;
                    }
                    match b {
                        b'"' => in_string = true,
                        b'{' | b'[' => depth += 1,
                        b'}' | b']' => {
                            depth = depth.saturating_sub(1);
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
            }
            Some(b'"') => {
                self.take()?;
                let mut escaped = false;
                loop {
                    let b = self.take()?;
                    if escaped {
                        escaped = false;
                    } else if b == b'\\' {
                        escaped = true;
                    } else if b == b'"' {
                        break;
                    }
                }
            }
            Some(b',' | b']') => {
                return Err(invalid(
                    &self.path,
                    format!("{}: expected a value", self.at()),
                ));
            }
            Some(_) => {
                while let Some(b) = self.scanner.peek().map_err(|e| self.io(&e))? {
                    if is_space(b) || matches!(b, b',' | b']' | b'}') {
                        break;
                    }
                    self.take()?;
                }
            }
        }
        Ok(())
    }

    /// The next non-blank line into `elem`.
    fn next_line(&mut self) -> Result<bool> {
        loop {
            self.elem.clear();
            self.elem_line = self.scanner.line;
            let mut ended = false;
            loop {
                match self.scanner.bump().map_err(|e| self.io(&e))? {
                    None => {
                        ended = true;
                        break;
                    }
                    Some(b'\n') => break,
                    Some(b) => self.push(b)?,
                }
            }
            if self.elem.last() == Some(&b'\r') {
                self.elem.pop();
            }
            if !self.elem.iter().all(|&b| is_space(b)) {
                return Ok(true);
            }
            if ended {
                return Ok(false);
            }
        }
    }

    fn consumed(&self) -> u64 {
        consumed(&self.counter)
    }

    fn had_replacements(&self) -> bool {
        self.scanner.input.had_replacements()
    }
}

/// A top-level member value.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Value {
    Null,
    Bool(bool),
    /// As written.
    Number(String),
    Str(String),
    /// An object or array as compact JSON text.
    Nested(String),
}

impl Value {
    fn cell(&self) -> Cell<'_> {
        match self {
            Value::Null => Cell::Null,
            Value::Bool(v) => Cell::Bool(*v),
            Value::Number(t) => Cell::Number(t),
            Value::Str(t) => Cell::Text(t),
            Value::Nested(t) => Cell::Json(t),
        }
    }

    fn display(&self) -> Option<String> {
        match self {
            Value::Null => None,
            Value::Bool(v) => Some(v.to_string()),
            Value::Number(t) | Value::Str(t) | Value::Nested(t) => Some(display(t)),
        }
    }
}

/// What one element is.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Parsed {
    Object(Vec<(String, Value)>),
    /// Not an object: what it is instead ("an array", "a number", ...).
    NotObject(&'static str),
    /// Not valid JSON: why.
    Invalid(String),
}

fn skip_space(b: &[u8], mut i: usize) -> usize {
    while b.get(i).copied().is_some_and(is_space) {
        i += 1;
    }
    i
}

/// End (exclusive) of the string starting with the quote at `start`.
fn string_end(b: &[u8], start: usize) -> Option<usize> {
    let mut i = start + 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'"' => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

/// End (exclusive) of the object or array starting at `start`.
fn nested_end(b: &[u8], start: usize) -> Option<usize> {
    let mut depth = 0_u64;
    let mut i = start;
    while i < b.len() {
        match b[i] {
            b'"' => {
                i = string_end(b, i)?;
                continue;
            }
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// JSON text without the whitespace between tokens.
fn minify(b: &[u8]) -> String {
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'"' => {
                let end = string_end(b, i).unwrap_or(b.len()).min(b.len());
                out.extend_from_slice(&b[i..end]);
                i = end;
            }
            c if is_space(c) => i += 1,
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// Splits an element into its top-level members (keys in order, values as written).
fn parse_element(b: &[u8]) -> Parsed {
    if let Err(e) = serde_json::from_slice::<serde::de::IgnoredAny>(b) {
        return Parsed::Invalid(e.to_string());
    }
    let start = skip_space(b, 0);
    match b.get(start) {
        Some(b'{') => {}
        Some(b'[') => return Parsed::NotObject("an array"),
        Some(b'"') => return Parsed::NotObject("a string"),
        Some(b't' | b'f') => return Parsed::NotObject("a boolean"),
        Some(b'n') => return Parsed::NotObject("null"),
        _ => return Parsed::NotObject("a number"),
    }
    match members(b, start + 1) {
        Some(members) => Parsed::Object(members),
        None => Parsed::Invalid("malformed object".to_owned()),
    }
}

/// The members of a valid object whose `{` is just before `i`.
fn members(b: &[u8], mut i: usize) -> Option<Vec<(String, Value)>> {
    let mut out = Vec::new();
    loop {
        i = skip_space(b, i);
        match b.get(i)? {
            b'}' => return Some(out),
            b'"' => {}
            _ => return None,
        }
        let end = string_end(b, i)?;
        let key: String = serde_json::from_slice(&b[i..end]).ok()?;
        i = skip_space(b, end);
        if b.get(i)? != &b':' {
            return None;
        }
        i = skip_space(b, i + 1);
        let (value, next) = match b.get(i)? {
            b'"' => {
                let end = string_end(b, i)?;
                (Value::Str(serde_json::from_slice(&b[i..end]).ok()?), end)
            }
            b'{' | b'[' => {
                let end = nested_end(b, i)?;
                (Value::Nested(minify(&b[i..end])), end)
            }
            b't' => (Value::Bool(true), i + 4),
            b'f' => (Value::Bool(false), i + 5),
            b'n' => (Value::Null, i + 4),
            _ => {
                let mut end = i;
                while b
                    .get(end)
                    .is_some_and(|&c| !is_space(c) && c != b',' && c != b'}')
                {
                    end += 1;
                }
                (
                    Value::Number(String::from_utf8_lossy(&b[i..end]).into_owned()),
                    end,
                )
            }
        };
        out.push((key, value));
        i = skip_space(b, next);
        match b.get(i)? {
            b',' => i += 1,
            b'}' => return Some(out),
            _ => return None,
        }
    }
}

/// What sampling found.
struct Sample {
    /// Keys in first-seen order, as written.
    keys: Vec<String>,
    inference: TypeInference,
    rows: Vec<Vec<Option<String>>>,
    sampled: usize,
    complete: bool,
    not_objects: usize,
    invalid_lines: usize,
}

fn sample(
    src: &mut JsonSource,
    limits: PreviewLimits,
    formats: &ValueFormats,
    cancel: &CancellationToken,
) -> Result<Sample> {
    let mut s = Sample {
        keys: Vec::new(),
        inference: TypeInference::new(0, formats.clone()),
        rows: Vec::new(),
        sampled: 0,
        complete: false,
        not_objects: 0,
        invalid_lines: 0,
    };
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut seen: Vec<bool> = Vec::new();
    loop {
        if s.sampled >= limits.sample_rows
            || (s.sampled > 0 && src.consumed() > limits.sample_bytes)
        {
            break;
        }
        check_cancel(cancel)?;
        if !src.next_element()? {
            s.complete = true;
            break;
        }
        s.sampled += 1;
        let mut row = Vec::new();
        seen.fill(false);
        match parse_element(&src.elem) {
            Parsed::Invalid(message) => {
                if src.layout == Layout::Array {
                    return Err(invalid(
                        &src.path,
                        format!(
                            "element {} (line {}) is not valid JSON: {message}",
                            src.index, src.elem_line
                        ),
                    ));
                }
                s.invalid_lines += 1;
            }
            Parsed::NotObject(_) => s.not_objects += 1,
            Parsed::Object(members) => {
                for (key, value) in &members {
                    let known = index.get(key).copied();
                    let column = known.unwrap_or_else(|| {
                        let c = s.keys.len();
                        index.insert(key.clone(), c);
                        s.keys.push(key.clone());
                        seen.push(false);
                        // Earlier rows lacked the key.
                        s.inference
                            .observe_nulls(c, u64::try_from(s.sampled - 1).unwrap_or(0));
                        c
                    });
                    if seen[column] {
                        continue;
                    }
                    seen[column] = true;
                    match value {
                        Value::Null => s.inference.observe(column, None),
                        Value::Bool(_) => s.inference.observe_typed(column, &DataType::Bool),
                        Value::Number(raw) => s.inference.observe_number(column, raw),
                        Value::Str(text) => s.inference.observe(column, Some(text)),
                        Value::Nested(_) => s
                            .inference
                            .observe_typed(column, &DataType::Json { binary: false }),
                    }
                    if s.rows.len() < limits.display_rows {
                        if row.len() <= column {
                            row.resize(column + 1, None);
                        }
                        row[column] = value.display();
                    }
                }
            }
        }
        for (column, _) in seen.iter().enumerate().filter(|(_, v)| !**v) {
            s.inference.observe(column, None);
        }
        if s.rows.len() < limits.display_rows {
            s.rows.push(row);
        }
    }
    for row in &mut s.rows {
        row.resize(s.keys.len(), None);
    }
    Ok(s)
}

/// Samples a JSON file (see [`crate::preview`]).
pub(crate) fn preview(
    path: &Path,
    o: &JsonReadOptions,
    limits: PreviewLimits,
    cancel: &CancellationToken,
) -> Result<SourcePreview> {
    let mut src = JsonSource::open(path, o)?;
    let s = sample(&mut src, limits, &o.formats, cancel)?;
    let inferred = s.inference.finish(s.complete);
    let columns: Vec<PreviewColumn> = unique_names(s.keys)
        .into_iter()
        .enumerate()
        .map(|(index, name)| {
            let column = inferred.get(index);
            PreviewColumn {
                index,
                name,
                data_type: column.map_or(DataType::Text, |c| c.data_type.clone()),
                nullable: column.is_none_or(|c| c.nullable),
                warnings: column.map_or_else(Vec::new, |c| c.warnings.clone()),
            }
        })
        .collect();
    let mut warnings = Vec::new();
    if columns.is_empty() {
        warnings.push("no objects with keys were found".to_owned());
    }
    if src.had_replacements() {
        warnings.push(format!(
            "some bytes are not valid {}; they were replaced with U+FFFD",
            encoding_name(src.detected.encoding.unwrap_or_default())
        ));
    }
    if s.not_objects > 0 {
        warnings.push(format!(
            "{} of {} elements are not objects; they will be reported as row issues",
            s.not_objects, s.sampled
        ));
    }
    if s.invalid_lines > 0 {
        warnings.push(format!(
            "{} of {} lines are not valid JSON; they will be reported as row issues",
            s.invalid_lines, s.sampled
        ));
    }
    Ok(SourcePreview {
        columns,
        rows: s.rows,
        sampled_rows: s.sampled,
        complete: s.complete,
        total_bytes: src.total_bytes,
        detected: src.detected.clone(),
        warnings,
    })
}

/// Opens a JSON file for reading (see [`crate::open`]).
///
/// Columns are matched to keys like the preview found them: the keys of the first 1000
/// objects (the default [`PreviewLimits`]) in first-seen order, by position when the counts
/// agree, else by name (a spec whose name is no sampled key reads the key of that name).
pub(crate) fn open(
    path: &Path,
    o: &JsonReadOptions,
    columns: &[ColumnSpec],
    batch_rows: usize,
) -> Result<Box<dyn SourceReader>> {
    let keys = {
        let mut probe = JsonSource::open(path, o)?;
        sample(
            &mut probe,
            PreviewLimits::default(),
            &o.formats,
            &CancellationToken::new(),
        )?
        .keys
    };
    let names = unique_names(keys.clone());
    let mut lookup = HashMap::new();
    if names.len() == columns.len() {
        for (i, key) in keys.into_iter().enumerate() {
            lookup.insert(key, i);
        }
    } else {
        for (i, spec) in columns.iter().enumerate() {
            let key = names
                .iter()
                .position(|n| n == &spec.name)
                .map_or_else(|| spec.name.clone(), |at| keys[at].clone());
            lookup.entry(key).or_insert(i);
        }
    }
    let src = JsonSource::open(path, o)?;
    let total = src.total_bytes;
    Ok(Box::new(JsonReader {
        src,
        lookup,
        names: columns.iter().map(|c| c.name.clone()).collect(),
        json_columns: columns
            .iter()
            .map(|c| matches!(c.data_type, DataType::Json { .. }))
            .collect(),
        schema: ColumnSpec::schema(columns),
        builders: columns
            .iter()
            .map(|c| ColumnBuilder::new(&c.data_type, batch_rows))
            .collect(),
        batch_rows,
        formats: o.formats.clone(),
        row: 0,
        progress: Progress::new(total),
        done: false,
    }))
}

struct JsonReader {
    src: JsonSource,
    /// Key → column.
    lookup: HashMap<String, usize>,
    names: Vec<String>,
    json_columns: Vec<bool>,
    schema: SchemaRef,
    builders: Vec<ColumnBuilder>,
    batch_rows: usize,
    formats: ValueFormats,
    row: u64,
    progress: Progress,
    done: bool,
}

impl JsonReader {
    fn issue(&self, batch_row: usize, column: Option<String>, message: String) -> RowIssue {
        let line = self.src.elem_line;
        let message = if line == self.row {
            message
        } else {
            format!("{message} (line {line})")
        };
        RowIssue {
            row: self.row,
            batch_row,
            column,
            message,
        }
    }

    fn null_row(&mut self) {
        for builder in &mut self.builders {
            builder.append_null();
        }
    }
}

impl SourceReader for JsonReader {
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    fn total_bytes(&self) -> u64 {
        self.progress.total()
    }

    fn bytes_read(&self) -> u64 {
        self.progress.position()
    }

    fn next_batch(&mut self) -> Result<Option<SourceBatch>> {
        if self.done {
            return Ok(None);
        }
        let n = self.builders.len();
        let first_row = self.row + 1;
        let mut issues = Vec::new();
        let mut rows = 0_usize;
        let mut slots: Vec<Option<usize>> = vec![None; n];
        while rows < self.batch_rows {
            if !self.src.next_element()? {
                self.done = true;
                break;
            }
            self.row += 1;
            match parse_element(&self.src.elem) {
                Parsed::Invalid(message) => {
                    if self.src.layout == Layout::Array {
                        self.done = true;
                        return Err(invalid(
                            &self.src.path,
                            format!(
                                "element {} (line {}) is not valid JSON: {message}",
                                self.src.index, self.src.elem_line
                            ),
                        ));
                    }
                    self.null_row();
                    issues.push(self.issue(rows, None, format!("not valid JSON: {message}")));
                }
                Parsed::NotObject(what) => {
                    self.null_row();
                    issues.push(self.issue(
                        rows,
                        None,
                        format!("the element is {what}, not an object"),
                    ));
                }
                Parsed::Object(members) => {
                    slots.fill(None);
                    for (m, (key, _)) in members.iter().enumerate() {
                        match self.lookup.get(key) {
                            Some(&c) => {
                                if slots[c].is_none() {
                                    slots[c] = Some(m);
                                }
                            }
                            None => issues.push(self.issue(
                                rows,
                                None,
                                format!("unknown key {}; its value was dropped", super::quote(key)),
                            )),
                        }
                    }
                    for (c, slot) in slots.iter().enumerate() {
                        let Some(m) = *slot else {
                            self.builders[c].append_null();
                            continue;
                        };
                        let value = &members[m].1;
                        let encoded;
                        let cell = match value {
                            // A JSON string in a json column is the JSON string itself.
                            Value::Str(t) if self.json_columns[c] && !is_json(t) => {
                                encoded = serde_json::Value::String(t.clone()).to_string();
                                Cell::Json(&encoded)
                            }
                            other => other.cell(),
                        };
                        if let Err(message) = self.builders[c].append(cell, &self.formats) {
                            self.builders[c].append_null();
                            let column = Some(self.names[c].clone());
                            issues.push(self.issue(rows, column, message));
                        }
                    }
                }
            }
            rows += 1;
        }
        if rows == 0 {
            self.progress.finish();
            return Ok(None);
        }
        let columns = self
            .builders
            .iter_mut()
            .map(ColumnBuilder::finish)
            .collect();
        let batch = RecordBatch::try_new(self.schema.clone(), columns)
            .map_err(|e| Error::internal(format!("JSON batch: {e}")))?;
        let bytes_read = if self.done {
            self.progress.finish()
        } else {
            self.progress.advance(self.src.consumed())
        };
        Ok(Some(SourceBatch {
            batch,
            first_row,
            issues,
            bytes_read,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elements_split_into_members() {
        let parsed = parse_element(
            br#" {"id": 9007199254740993, "s": "a\"b", "n": null, "t": true,
                 "nested": {"k": [1, 2, {"x": "y z"}]}, "f": 12.50, "e": -1e3} "#,
        );
        let Parsed::Object(members) = &parsed else {
            panic!("not an object: {parsed:?}")
        };
        let get = |k: &str| members.iter().find(|(key, _)| key == k).unwrap().1.clone();
        assert_eq!(get("id"), Value::Number("9007199254740993".into()));
        assert_eq!(get("s"), Value::Str("a\"b".into()));
        assert_eq!(get("n"), Value::Null);
        assert_eq!(get("t"), Value::Bool(true));
        assert_eq!(
            get("nested"),
            Value::Nested(r#"{"k":[1,2,{"x":"y z"}]}"#.into())
        );
        assert_eq!(get("f"), Value::Number("12.50".into()));
        assert_eq!(get("e"), Value::Number("-1e3".into()));

        assert_eq!(parse_element(b"[1]"), Parsed::NotObject("an array"));
        assert_eq!(parse_element(b"\"x\""), Parsed::NotObject("a string"));
        assert_eq!(parse_element(b"3"), Parsed::NotObject("a number"));
        assert!(matches!(parse_element(b"{\"a\": tru}"), Parsed::Invalid(_)));
        assert_eq!(parse_element(b"{}"), Parsed::Object(Vec::new()));
    }

    #[test]
    fn helpers() {
        assert_eq!(string_end(br#""a\"b" x"#, 0), Some(6));
        assert_eq!(nested_end(br#"[1, "]", [2]] tail"#, 0), Some(13));
        assert_eq!(nested_end(b"[1, [2]", 0), None);
        assert_eq!(minify(b"{ \"a b\" : [ 1 ,\n 2 ] }"), r#"{"a b":[1,2]}"#);
    }
}
