//! Checks an XLSX zip before `calamine` opens it, so a hostile workbook cannot exhaust
//! memory: `calamine` loads every part it needs except the worksheet into memory on open
//! (the shared-strings table, styles, workbook, relationships) and reserves the shared-strings
//! table from its declared `uniqueCount`, and while streaming a worksheet it buffers one XML
//! event (a text node, a tag) or one cell's text at a time.
//!
//! - At most [`Limits::entries`] zip entries.
//! - Every XML part other than worksheets is decompressed and counted: at most
//!   [`Limits::part_bytes`] each and [`Limits::parts_total`] together (the zip's
//!   declared sizes are not trusted). The shared-strings table's `uniqueCount` must fit its
//!   real size (each string takes at least 8 bytes of XML).
//! - The worksheet to read is decompressed and scanned: no text or tag longer than
//!   [`Limits::xml_run`], no cell (`<c>` ... `</c>`) larger than [`Limits::cell_bytes`].
//!   Preview and import both scan the whole sheet once before reading it (the cost of a
//!   second decompression pass): the reading thread runs ahead of its consumer, so a partial
//!   scan would not bound what it parses.
//! - Worksheets that are not read are skipped when the workbook maps sheet names to parts in
//!   the usual way; otherwise every worksheet is scanned.

use std::borrow::Cow;
use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use oxbase_types::Result;
use zip::ZipArchive;

use super::{invalid, io_error};

/// What a workbook may contain (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Limits {
    pub entries: usize,
    pub part_bytes: u64,
    pub parts_total: u64,
    pub xml_run: u64,
    pub cell_bytes: u64,
}

/// The limits readers use.
pub(crate) const LIMITS: Limits = Limits {
    entries: 10_000,
    part_bytes: 256 * 1024 * 1024,
    parts_total: 512 * 1024 * 1024,
    xml_run: 16 * 1024 * 1024,
    cell_bytes: 16 * 1024 * 1024,
};

/// Where the worksheet's compressed data lies in the file (for progress).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SheetLocation {
    pub data_start: u64,
    pub compressed_size: u64,
}

/// Workbook parts read into memory to map sheet names to parts.
const MAX_MAP_PART: u64 = 16 * 1024 * 1024;

/// Checks the workbook at `path` for reading `sheet` (`None`: the first). `InvalidInput`
/// naming the part and the limit when it is not a workbook or breaks a limit.
pub(crate) fn check(
    path: &Path,
    sheet: Option<&str>,
    limits: &Limits,
) -> Result<Option<SheetLocation>> {
    let mut file = File::open(path).map_err(|e| io_error(path, &e))?;
    let mut magic = Vec::with_capacity(4);
    (&mut file)
        .take(4)
        .read_to_end(&mut magic)
        .map_err(|e| io_error(path, &e))?;
    if magic.starts_with(&[0xD0, 0xCF, 0x11, 0xE0]) {
        return Err(invalid(
            path,
            "is not an XLSX workbook: it is password-protected or a legacy .xls file",
        ));
    }
    if !magic.starts_with(b"PK") {
        return Err(invalid(path, "is not an XLSX workbook (not a zip archive)"));
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|e| io_error(path, &e))?;
    let mut zip = ZipArchive::new(BufReader::new(file))
        .map_err(|e| invalid(path, format!("is not an XLSX workbook: {e}")))?;
    if zip.len() > limits.entries {
        return Err(invalid(
            path,
            format!(
                "the workbook has {} parts, more than the {} accepted",
                zip.len(),
                limits.entries
            ),
        ));
    }
    let names: Vec<String> = zip
        .file_names()
        .map(|n| n.map(Cow::into_owned))
        .collect::<zip::result::ZipResult<_>>()
        .map_err(|e| invalid(path, format!("is not an XLSX workbook: {e}")))?;
    let target = sheet_part(&mut zip, &names, sheet);
    let mut parts_total = 0_u64;
    for (i, name) in names.iter().enumerate() {
        let lower = name.to_ascii_lowercase();
        if lower.ends_with('/') {
            continue;
        }
        let part_error = |message: String| invalid(path, format!("{name}: {message}"));
        if target == Some(i) || lower.contains("worksheets/") {
            if target.is_some_and(|t| t != i) {
                // Never read.
                continue;
            }
            let mut entry = zip.by_index(i).map_err(|e| part_error(e.to_string()))?;
            let mut scanner = SheetScanner::new(*limits);
            drain(&mut entry, u64::MAX, |chunk| scanner.feed(chunk)).map_err(part_error)?;
            continue;
        }
        let xml = Path::new(&lower)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("xml") || e.eq_ignore_ascii_case("rels"));
        if !xml {
            continue;
        }
        let shared = lower.ends_with("sharedstrings.xml");
        let mut entry = zip.by_index(i).map_err(|e| part_error(e.to_string()))?;
        let mut head = Vec::new();
        let size = drain(&mut entry, limits.part_bytes, |chunk| {
            if shared && head.len() < 4096 {
                head.extend_from_slice(&chunk[..chunk.len().min(4096)]);
            }
            Ok(true)
        })
        .map_err(part_error)?;
        parts_total = parts_total.saturating_add(size);
        if parts_total > limits.parts_total {
            return Err(invalid(
                path,
                format!(
                    "the workbook's parts expand to more than {} bytes",
                    limits.parts_total
                ),
            ));
        }
        if shared
            && let Some(count) = unique_count(&head)
            && count > size / 8 + 16
        {
            return Err(part_error(format!(
                "declares {count} shared strings but holds at most {}",
                size / 8
            )));
        }
    }
    let Some(index) = target else {
        return Ok(None);
    };
    let entry = zip
        .by_index(index)
        .map_err(|e| invalid(path, e.to_string()))?;
    Ok(entry.data_start().map(|data_start| SheetLocation {
        data_start,
        compressed_size: entry.compressed_size(),
    }))
}

/// Reads `reader` to the end in chunks, failing past `cap` bytes; `inspect` sees every chunk
/// and returns `false` to stop early. The bytes read.
fn drain(
    reader: &mut impl Read,
    cap: u64,
    mut inspect: impl FnMut(&[u8]) -> std::result::Result<bool, String>,
) -> std::result::Result<u64, String> {
    let mut buf = vec![0_u8; 64 * 1024];
    let mut total = 0_u64;
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => return Ok(total),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.to_string()),
        };
        total = total.saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
        if total > cap {
            return Err(format!("expands to more than {cap} bytes"));
        }
        if !inspect(&buf[..n])? {
            return Ok(total);
        }
    }
}

/// The `uniqueCount` attribute at the start of a shared-strings part.
fn unique_count(head: &[u8]) -> Option<u64> {
    let text = String::from_utf8_lossy(head);
    let at = text.find("uniqueCount")?;
    let rest = text[at + "uniqueCount".len()..]
        .trim_start()
        .strip_prefix('=')?
        .trim_start();
    let rest = rest.strip_prefix('"').or_else(|| rest.strip_prefix('\''))?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    // More digits than a u64 holds is as hostile as a huge number.
    Some(digits.parse().unwrap_or(u64::MAX))
}

/// Byte-level scan of worksheet XML (see the module docs).
struct SheetScanner {
    limits: Limits,
    run: u64,
    in_tag: bool,
    closing: bool,
    name: [u8; 16],
    name_len: usize,
    name_done: bool,
    name_long: bool,
    prev: u8,
    in_cell: bool,
    cell: u64,
}

impl SheetScanner {
    fn new(limits: Limits) -> Self {
        Self {
            limits,
            run: 0,
            in_tag: false,
            closing: false,
            name: [0; 16],
            name_len: 0,
            name_done: false,
            name_long: false,
            prev: 0,
            in_cell: false,
            cell: 0,
        }
    }

    /// Scans `chunk` (always `Ok(true)`: the whole sheet is scanned).
    fn feed(&mut self, chunk: &[u8]) -> std::result::Result<bool, String> {
        for &b in chunk {
            match b {
                b'<' => {
                    self.run = 0;
                    self.in_tag = true;
                    self.closing = false;
                    self.name_len = 0;
                    self.name_done = false;
                    self.name_long = false;
                }
                b'>' if self.in_tag => {
                    self.run = 0;
                    self.in_tag = false;
                    self.end_of_tag();
                }
                _ => {
                    self.run += 1;
                    if self.run > self.limits.xml_run {
                        return Err(format!(
                            "holds a text or tag longer than {} bytes",
                            self.limits.xml_run
                        ));
                    }
                    if self.in_tag && !self.name_done {
                        if b == b'/' && self.name_len == 0 && !self.closing {
                            self.closing = true;
                        } else if b.is_ascii_whitespace() || b == b'/' {
                            self.name_done = true;
                        } else if self.name_len < self.name.len() {
                            self.name[self.name_len] = b;
                            self.name_len += 1;
                        } else {
                            self.name_long = true;
                        }
                    }
                }
            }
            if self.in_cell {
                self.cell += 1;
                if self.cell > self.limits.cell_bytes {
                    return Err(format!(
                        "holds a cell larger than {} bytes",
                        self.limits.cell_bytes
                    ));
                }
            }
            self.prev = b;
        }
        Ok(true)
    }

    /// A tag ended: track whether the scan is inside a cell.
    fn end_of_tag(&mut self) {
        if self.name_long {
            return;
        }
        let name = &self.name[..self.name_len];
        let local = match name.iter().rposition(|&c| c == b':') {
            Some(at) => &name[at + 1..],
            None => name,
        };
        let self_closing = self.prev == b'/';
        match (self.closing, local) {
            (false, b"c") => {
                self.in_cell = !self_closing;
                self.cell = 0;
            }
            (_, b"row") | (true, b"c") => self.in_cell = false,
            _ => {}
        }
    }
}

/// The zip entry of the worksheet `sheet` (`None`: the first), when the workbook maps names
/// to parts the usual way (`xl/workbook.xml` and its relationships).
fn sheet_part<R: Read + Seek>(
    zip: &mut ZipArchive<R>,
    names: &[String],
    sheet: Option<&str>,
) -> Option<usize> {
    let find = |wanted: &str| names.iter().position(|n| n.eq_ignore_ascii_case(wanted));
    let workbook = read_part(zip, find("xl/workbook.xml")?)?;
    let rels = read_part(zip, find("xl/_rels/workbook.xml.rels")?)?;
    let sheets = tags(&workbook, "sheet");
    let chosen = match sheet {
        Some(wanted) => sheets
            .iter()
            .find(|attrs| attribute(attrs, "name") == Some(wanted))?,
        None => sheets.first()?,
    };
    let id = attribute(chosen, "id")?;
    let relation = tags(&rels, "Relationship")
        .into_iter()
        .find(|attrs| attribute(attrs, "Id") == Some(id))?;
    let target = attribute(&relation, "Target")?;
    let resolved = match target.strip_prefix('/') {
        Some(absolute) => absolute.to_owned(),
        None => format!("xl/{}", target.trim_start_matches("./")),
    };
    find(&resolved)
}

/// A small part as text (`None` when missing, unreadable or over 16 MiB).
fn read_part<R: Read + Seek>(zip: &mut ZipArchive<R>, index: usize) -> Option<String> {
    let entry = zip.by_index(index).ok()?;
    let mut bytes = Vec::new();
    entry.take(MAX_MAP_PART + 1).read_to_end(&mut bytes).ok()?;
    if u64::try_from(bytes.len()).ok()? > MAX_MAP_PART {
        return None;
    }
    String::from_utf8(bytes).ok()
}

type Attributes = Vec<(String, String)>;

fn attribute<'a>(attrs: &'a [(String, String)], local: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(k, _)| k == local)
        .map(|(_, v)| v.as_str())
}

/// The attributes (local names, unescaped values) of every element named `local`.
fn tags(xml: &str, local: &str) -> Vec<Attributes> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(at) = rest.find('<') {
        rest = &rest[at + 1..];
        let end = rest.find('>').unwrap_or(rest.len());
        let tag = &rest[..end];
        rest = &rest[end..];
        let name_end = tag
            .find(|c: char| c.is_whitespace() || c == '/')
            .unwrap_or(tag.len());
        let name = &tag[..name_end];
        if name.rsplit(':').next() == Some(local) {
            out.push(attributes(&tag[name_end..]));
        }
    }
    out
}

fn attributes(mut rest: &str) -> Attributes {
    let mut out = Vec::new();
    loop {
        rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == '/');
        let Some(eq) = rest.find('=') else {
            break;
        };
        let key = rest[..eq].trim();
        let after = rest[eq + 1..].trim_start();
        let Some(quote) = after.chars().next().filter(|&c| c == '"' || c == '\'') else {
            break;
        };
        let body = &after[1..];
        let Some(close) = body.find(quote) else {
            break;
        };
        let local = key.rsplit(':').next().unwrap_or(key);
        out.push((local.to_owned(), unescape(&body[..close])));
        rest = &body[close + 1..];
    }
    out
}

fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let Some(end) = rest.find(';') else {
            break;
        };
        let entity = &rest[1..end];
        let c = match entity {
            "lt" => Some('<'),
            "gt" => Some('>'),
            "amp" => Some('&'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
                .map_or_else(
                    || entity.strip_prefix('#').and_then(|d| d.parse().ok()),
                    |h| u32::from_str_radix(h, 16).ok(),
                )
                .and_then(char::from_u32),
        };
        if let Some(c) = c {
            out.push(c);
            rest = &rest[end + 1..];
        } else {
            out.push('&');
            rest = &rest[1..];
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};

    use zip::write::SimpleFileOptions;
    use zip::{CompressionMethod, ZipWriter};

    use super::*;

    fn workbook(dir: &Path, parts: &[(&str, &[u8])]) -> std::path::PathBuf {
        let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for (name, data) in parts {
            zip.start_file(*name, options).unwrap();
            zip.write_all(data).unwrap();
        }
        let bytes = zip.finish().unwrap().into_inner();
        let path = dir.join(format!("w{}.xlsx", parts.len()));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    const WORKBOOK: &[u8] = br#"<workbook xmlns:r="r"><sheets><sheet name="A &amp; B" sheetId="1" r:id="rId1"/><sheet name="Two" sheetId="2" r:id="rId2"/></sheets></workbook>"#;
    const RELS: &[u8] = br#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Target="/xl/worksheets/sheet2.xml"/></Relationships>"#;

    fn small() -> Limits {
        Limits {
            entries: 10,
            part_bytes: 100_000,
            parts_total: 150_000,
            xml_run: 1_000,
            cell_bytes: 2_000,
        }
    }

    #[test]
    fn maps_sheets_and_accepts_a_normal_workbook() {
        let dir = tempfile::tempdir().unwrap();
        let sheet = b"<worksheet><sheetData><row r=\"1\"><c r=\"A1\"><v>1</v></c><c r=\"B1\"/></row></sheetData></worksheet>";
        let path = workbook(
            dir.path(),
            &[
                ("xl/workbook.xml", WORKBOOK),
                ("xl/_rels/workbook.xml.rels", RELS),
                ("xl/worksheets/sheet1.xml", &sheet[..]),
                ("xl/worksheets/sheet2.xml", &sheet[..]),
            ],
        );
        let found = check(&path, None, &small()).unwrap();
        assert!(found.is_some_and(|l| l.compressed_size > 0));
        let mut zip = ZipArchive::new(File::open(&path).unwrap()).unwrap();
        let names: Vec<String> = zip.file_names().map(|n| n.unwrap().into_owned()).collect();
        assert_eq!(sheet_part(&mut zip, &names, None), Some(2));
        assert_eq!(sheet_part(&mut zip, &names, Some("A & B")), Some(2));
        assert_eq!(sheet_part(&mut zip, &names, Some("Two")), Some(3));
        assert_eq!(sheet_part(&mut zip, &names, Some("Missing")), None);
    }

    #[test]
    fn rejects_bombs_and_huge_cells() {
        let dir = tempfile::tempdir().unwrap();
        // Shared strings that expand far beyond the part limit (highly compressible).
        let mut bomb = b"<sst uniqueCount=\"1\"><si><t>".to_vec();
        bomb.extend(std::iter::repeat_n(b'a', 200_000));
        bomb.extend_from_slice(b"</t></si></sst>");
        let path = workbook(dir.path(), &[("xl/sharedStrings.xml", &bomb[..])]);
        let err = check(&path, None, &small()).unwrap_err();
        assert_eq!(err.category(), oxbase_types::ErrorCategory::InvalidInput);
        assert!(err.to_string().contains("sharedStrings.xml"), "{err}");

        // A declared count the part cannot hold.
        let lying = b"<sst count=\"1\" uniqueCount=\"4000000000\"><si><t>a</t></si></sst>";
        let path = workbook(
            dir.path(),
            &[
                ("xl/sharedStrings.xml", &lying[..]),
                ("xl/styles.xml", &b"<x/>"[..]),
            ],
        );
        let err = check(&path, None, &small()).unwrap_err();
        assert!(err.to_string().contains("4000000000"), "{err}");

        // One text node over the run limit.
        let mut sheet = b"<worksheet><sheetData><row><c t=\"inlineStr\"><is><t>".to_vec();
        sheet.extend(std::iter::repeat_n(b'x', 1_500));
        sheet.extend_from_slice(b"</t></is></c></row></sheetData></worksheet>");
        let path = workbook(
            dir.path(),
            &[
                ("xl/worksheets/sheet1.xml", &sheet[..]),
                ("a.xml", &b"<a/>"[..]),
                ("b.xml", &b"<b/>"[..]),
            ],
        );
        let err = check(&path, None, &small()).unwrap_err();
        assert!(err.to_string().contains("longer than 1000"), "{err}");

        // Many short runs in one cell.
        let mut sheet = b"<worksheet><sheetData><row><c t=\"inlineStr\"><is>".to_vec();
        for _ in 0..300 {
            sheet.extend_from_slice(b"<r><t>abc</t></r>");
        }
        sheet.extend_from_slice(b"</is></c></row></sheetData></worksheet>");
        let path = workbook(
            dir.path(),
            &[
                ("xl/worksheets/sheet1.xml", &sheet[..]),
                ("a.xml", &b"<a/>"[..]),
                ("b.xml", &b"<b/>"[..]),
                ("c.xml", &b"<c/>"[..]),
            ],
        );
        let err = check(&path, None, &small()).unwrap_err();
        assert!(err.to_string().contains("cell larger"), "{err}");
        // Not a zip at all.
        let path = dir.path().join("plain.xlsx");
        std::fs::write(&path, b"id,name\n1,a\n").unwrap();
        let err = check(&path, None, &small()).unwrap_err();
        assert!(err.to_string().contains("not an XLSX workbook"), "{err}");
        assert!(!err.to_string().contains(&*dir.path().to_string_lossy()));
    }

    #[test]
    fn small_helpers() {
        assert_eq!(
            unescape("a &amp; b &lt;&#65;&#x42;&bogus; c"),
            "a & b <AB&bogus; c"
        );
        assert_eq!(unique_count(b"<sst uniqueCount = '12'>"), Some(12));
        assert_eq!(unique_count(b"<sst count=\"3\">"), None);
        assert_eq!(
            unique_count(b"<sst uniqueCount=\"99999999999999999999999\">"),
            Some(u64::MAX)
        );
        let all = tags(r#"<x:sheet name="a" r:id='rId3'/><sheet/>"#, "sheet");
        let attrs = &all[0];
        assert_eq!(attribute(attrs, "id"), Some("rId3"));
        assert_eq!(attribute(attrs, "name"), Some("a"));
    }
}
