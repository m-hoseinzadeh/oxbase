//! Text encodings at the file boundary: BOM detection, a decoding reader (any supported
//! encoding → UTF-8) for the CSV and JSON readers, an encoding writer (UTF-8 → the chosen
//! encoding) for the CSV writer, and byte counters for progress.
//!
//! Decoding never fails: malformed input becomes U+FFFD and
//! [`DecodingReader::had_replacements`] turns true, so the reader can warn once. Encoding fails
//! (`io::ErrorKind::InvalidData`) on a character the target encoding cannot hold
//! (Windows-1252, Latin-1), naming the character.

use std::io::{self, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use encoding_rs::{Decoder, Encoder, EncoderResult, UTF_8, UTF_16BE, UTF_16LE, WINDOWS_1252};
use oxbase_types::{Error, Result};

use crate::TextEncoding;

/// The UTF-8 byte order mark.
pub const UTF8_BOM: &[u8] = b"\xEF\xBB\xBF";
const UTF16LE_BOM: &[u8] = b"\xFF\xFE";
const UTF16BE_BOM: &[u8] = b"\xFE\xFF";

/// The encoding a byte order mark at the start of `prefix` announces, and the BOM's length.
pub fn detect_bom(prefix: &[u8]) -> Option<(TextEncoding, usize)> {
    if prefix.starts_with(UTF8_BOM) {
        Some((TextEncoding::Utf8, 3))
    } else if prefix.starts_with(UTF16LE_BOM) {
        Some((TextEncoding::Utf16Le, 2))
    } else if prefix.starts_with(UTF16BE_BOM) {
        Some((TextEncoding::Utf16Be, 2))
    } else {
        None
    }
}

/// The byte order mark written for `encoding` (empty for the single-byte encodings).
pub fn bom_bytes(encoding: TextEncoding) -> &'static [u8] {
    match encoding {
        TextEncoding::Utf8 => UTF8_BOM,
        TextEncoding::Utf16Le => UTF16LE_BOM,
        TextEncoding::Utf16Be => UTF16BE_BOM,
        TextEncoding::Auto | TextEncoding::Windows1252 | TextEncoding::Latin1 => &[],
    }
}

/// Resolves [`TextEncoding::Auto`] from the first bytes of a file: a BOM decides; else UTF-8
/// when `sample` is valid UTF-8 (a sequence cut off at the end of the sample is fine), else
/// Windows-1252. Other encodings are returned unchanged. The second value is the length of a
/// BOM of the resolved encoding at the start of `sample` (0 when there is none).
pub fn resolve(encoding: TextEncoding, sample: &[u8]) -> (TextEncoding, usize) {
    let bom = detect_bom(sample);
    let resolved = match (encoding, bom) {
        (TextEncoding::Auto, Some((found, _))) => found,
        (TextEncoding::Auto, None) => {
            // A sequence cut off at the end of the sample (no `error_len`) is still UTF-8.
            let utf8 =
                std::str::from_utf8(sample).map_or_else(|e| e.error_len().is_none(), |_| true);
            if utf8 {
                TextEncoding::Utf8
            } else {
                TextEncoding::Windows1252
            }
        }
        (other, _) => other,
    };
    let bom_len = bom
        .filter(|(found, _)| *found == resolved)
        .map_or(0, |(_, len)| len);
    (resolved, bom_len)
}

enum DecodeKind {
    Decoder(Decoder),
    Latin1,
}

/// Reads `inner` in a fixed encoding and yields UTF-8. A leading BOM of that encoding is
/// removed. Memory: one 64 KiB input buffer and its decoded output.
pub struct DecodingReader<R> {
    inner: R,
    kind: DecodeKind,
    raw: Vec<u8>,
    out: Vec<u8>,
    out_pos: usize,
    finished: bool,
    replaced: bool,
}

impl<R: Read> DecodingReader<R> {
    /// `InvalidInput` for [`TextEncoding::Auto`] (resolve it first with [`resolve`]).
    pub fn new(inner: R, encoding: TextEncoding) -> Result<Self> {
        let kind = match encoding {
            TextEncoding::Auto => {
                return Err(Error::invalid_input("resolve the encoding before decoding"));
            }
            TextEncoding::Utf8 => DecodeKind::Decoder(UTF_8.new_decoder_with_bom_removal()),
            TextEncoding::Utf16Le => DecodeKind::Decoder(UTF_16LE.new_decoder_with_bom_removal()),
            TextEncoding::Utf16Be => DecodeKind::Decoder(UTF_16BE.new_decoder_with_bom_removal()),
            TextEncoding::Windows1252 => {
                DecodeKind::Decoder(WINDOWS_1252.new_decoder_without_bom_handling())
            }
            TextEncoding::Latin1 => DecodeKind::Latin1,
        };
        Ok(Self {
            inner,
            kind,
            raw: vec![0; 64 * 1024],
            out: Vec::new(),
            out_pos: 0,
            finished: false,
            replaced: false,
        })
    }

    /// `true` once malformed input was replaced with U+FFFD.
    pub fn had_replacements(&self) -> bool {
        self.replaced
    }

    fn refill(&mut self) -> io::Result<()> {
        self.out.clear();
        self.out_pos = 0;
        let n = loop {
            match self.inner.read(&mut self.raw) {
                Ok(n) => break n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        };
        let last = n == 0;
        let src = &self.raw[..n];
        match &mut self.kind {
            DecodeKind::Latin1 => {
                let mut tmp = [0_u8; 4];
                for &b in src {
                    self.out
                        .extend_from_slice(char::from(b).encode_utf8(&mut tmp).as_bytes());
                }
            }
            DecodeKind::Decoder(decoder) => {
                let capacity = decoder
                    .max_utf8_buffer_length(n)
                    .unwrap_or(n.saturating_mul(3).saturating_add(16));
                self.out.resize(capacity, 0);
                let (_, _, written, replaced) = decoder.decode_to_utf8(src, &mut self.out, last);
                self.out.truncate(written);
                self.replaced |= replaced;
            }
        }
        if last {
            self.finished = true;
        }
        Ok(())
    }
}

impl<R: Read> Read for DecodingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.out_pos < self.out.len() {
                let n = buf.len().min(self.out.len() - self.out_pos);
                buf[..n].copy_from_slice(&self.out[self.out_pos..self.out_pos + n]);
                self.out_pos += n;
                return Ok(n);
            }
            if self.finished || buf.is_empty() {
                return Ok(0);
            }
            self.refill()?;
        }
    }
}

enum EncodeKind {
    Utf8,
    Utf16 { big_endian: bool },
    Latin1,
    Encoder(Encoder),
}

/// Takes UTF-8 bytes and writes them to `inner` in a fixed encoding. Writes no BOM (write
/// [`bom_bytes`] first when wanted). Call [`EncodingWriter::finish`] at the end: it fails when
/// the input ended inside a UTF-8 sequence.
pub struct EncodingWriter<W> {
    inner: W,
    kind: EncodeKind,
    pending: Vec<u8>,
    scratch: Vec<u8>,
}

impl<W: Write> EncodingWriter<W> {
    /// `InvalidInput` for [`TextEncoding::Auto`].
    pub fn new(inner: W, encoding: TextEncoding) -> Result<Self> {
        let kind = match encoding {
            TextEncoding::Auto => {
                return Err(Error::invalid_input("choose an encoding to write"));
            }
            TextEncoding::Utf8 => EncodeKind::Utf8,
            TextEncoding::Utf16Le => EncodeKind::Utf16 { big_endian: false },
            TextEncoding::Utf16Be => EncodeKind::Utf16 { big_endian: true },
            TextEncoding::Latin1 => EncodeKind::Latin1,
            TextEncoding::Windows1252 => EncodeKind::Encoder(WINDOWS_1252.new_encoder()),
        };
        Ok(Self {
            inner,
            kind,
            pending: Vec::new(),
            scratch: Vec::new(),
        })
    }

    /// Flushes and returns the inner writer.
    pub fn finish(mut self) -> io::Result<W> {
        if !self.pending.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the text ended inside a UTF-8 sequence",
            ));
        }
        self.inner.flush()?;
        Ok(self.inner)
    }

    fn encode(&mut self, text: &str) -> io::Result<()> {
        self.scratch.clear();
        match &mut self.kind {
            EncodeKind::Utf8 => self.scratch.extend_from_slice(text.as_bytes()),
            EncodeKind::Utf16 { big_endian } => {
                for unit in text.encode_utf16() {
                    let bytes = if *big_endian {
                        unit.to_be_bytes()
                    } else {
                        unit.to_le_bytes()
                    };
                    self.scratch.extend_from_slice(&bytes);
                }
            }
            EncodeKind::Latin1 => {
                for c in text.chars() {
                    let byte = u8::try_from(u32::from(c)).map_err(|_| unmappable(c, "Latin-1"))?;
                    self.scratch.push(byte);
                }
            }
            EncodeKind::Encoder(encoder) => {
                let capacity = encoder
                    .max_buffer_length_from_utf8_without_replacement(text.len())
                    .unwrap_or(text.len().saturating_add(16));
                self.scratch.resize(capacity, 0);
                let (result, read, written) =
                    encoder.encode_from_utf8_without_replacement(text, &mut self.scratch, false);
                if let EncoderResult::Unmappable(c) = result {
                    return Err(unmappable(c, "Windows-1252"));
                }
                debug_assert_eq!(read, text.len());
                self.scratch.truncate(written);
            }
        }
        self.inner.write_all(&self.scratch)
    }
}

fn unmappable(c: char, encoding: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "the character {c:?} (U+{:04X}) cannot be written in {encoding}",
            u32::from(c)
        ),
    )
}

impl<W: Write> Write for EncodingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.pending.extend_from_slice(buf);
        let valid = match std::str::from_utf8(&self.pending) {
            Ok(_) => self.pending.len(),
            Err(e) if e.error_len().is_none() => e.valid_up_to(),
            Err(_) => {
                self.pending.clear();
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "the text to write is not UTF-8",
                ));
            }
        };
        let rest = self.pending.split_off(valid);
        let done = std::mem::replace(&mut self.pending, rest);
        // `done` is valid UTF-8 by the check above.
        let text = std::str::from_utf8(&done)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        self.encode(text)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Counts bytes read through it into a shared counter (the progress position of a reader
/// running on another thread).
pub struct CountingReader<R> {
    inner: R,
    count: Arc<AtomicU64>,
}

impl<R: Read> CountingReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            count: Arc::new(AtomicU64::new(0)),
        }
    }

    /// The shared counter (bytes read so far).
    pub fn counter(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.count)
    }
}

impl<R: Read> Read for CountingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.count
            .fetch_add(u64::try_from(n).unwrap_or(u64::MAX), Ordering::Relaxed);
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(bytes: &[u8], encoding: TextEncoding) -> (String, bool) {
        let mut reader = DecodingReader::new(bytes, encoding).unwrap();
        let mut out = String::new();
        reader.read_to_string(&mut out).unwrap();
        (out, reader.had_replacements())
    }

    fn encode(text: &str, encoding: TextEncoding) -> io::Result<Vec<u8>> {
        let mut w = EncodingWriter::new(Vec::new(), encoding).unwrap();
        // Split inside a multi-byte character on purpose.
        let bytes = text.as_bytes();
        let mid = bytes.len() / 2;
        w.write_all(&bytes[..mid])?;
        w.write_all(&bytes[mid..])?;
        w.finish()
    }

    #[test]
    fn boms_and_auto_detection() {
        assert_eq!(detect_bom(b"\xEF\xBB\xBFa"), Some((TextEncoding::Utf8, 3)));
        assert_eq!(
            detect_bom(b"\xFF\xFEa\x00"),
            Some((TextEncoding::Utf16Le, 2))
        );
        assert_eq!(detect_bom(b"a,b"), None);
        assert_eq!(
            resolve(TextEncoding::Auto, b"a,\xC3\xA9"),
            (TextEncoding::Utf8, 0)
        );
        // Cut inside a character at the end of the sample: still UTF-8.
        assert_eq!(
            resolve(TextEncoding::Auto, b"a,\xC3"),
            (TextEncoding::Utf8, 0)
        );
        assert_eq!(
            resolve(TextEncoding::Auto, b"caf\xE9,1"),
            (TextEncoding::Windows1252, 0)
        );
        assert_eq!(
            resolve(TextEncoding::Auto, b"\xFE\xFF\x00a"),
            (TextEncoding::Utf16Be, 2)
        );
        assert_eq!(
            resolve(TextEncoding::Latin1, b"\xEF\xBB\xBF"),
            (TextEncoding::Latin1, 0)
        );
        assert_eq!(bom_bytes(TextEncoding::Utf16Le), b"\xFF\xFE");
        assert!(bom_bytes(TextEncoding::Windows1252).is_empty());
    }

    #[test]
    fn decoding() {
        assert_eq!(
            decode(b"\xEF\xBB\xBFid,na\xC3\xAFve\n", TextEncoding::Utf8),
            ("id,naïve\n".to_owned(), false)
        );
        assert_eq!(
            decode(b"\xFF\xFEa\x00\xe9\x00", TextEncoding::Utf16Le),
            ("aé".to_owned(), false)
        );
        // 0x80 is the euro sign in Windows-1252 and a C1 control in Latin-1.
        assert_eq!(decode(b"\x80", TextEncoding::Windows1252).0, "€");
        assert_eq!(decode(b"\x80\xE9", TextEncoding::Latin1).0, "\u{80}é");
        let (text, replaced) = decode(b"ok\xFF", TextEncoding::Utf8);
        assert_eq!(text, "ok\u{FFFD}");
        assert!(replaced);
        assert!(DecodingReader::new(&b""[..], TextEncoding::Auto).is_err());
    }

    #[test]
    fn encoding() {
        assert_eq!(encode("é€", TextEncoding::Utf8).unwrap(), "é€".as_bytes());
        assert_eq!(
            encode("aé", TextEncoding::Utf16Be).unwrap(),
            b"\x00a\x00\xe9".to_vec()
        );
        assert_eq!(
            encode("é€", TextEncoding::Windows1252).unwrap(),
            b"\xE9\x80".to_vec()
        );
        assert_eq!(encode("é", TextEncoding::Latin1).unwrap(), b"\xE9".to_vec());
        let err = encode("€", TextEncoding::Latin1).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(encode("中", TextEncoding::Windows1252).is_err());

        let mut w = EncodingWriter::new(Vec::new(), TextEncoding::Utf8).unwrap();
        w.write_all(b"\xC3").unwrap();
        assert!(w.finish().is_err());
    }

    #[test]
    fn counting() {
        let mut r = CountingReader::new(&b"hello"[..]);
        let counter = r.counter();
        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();
        assert_eq!(counter.load(Ordering::Relaxed), 5);
    }
}
