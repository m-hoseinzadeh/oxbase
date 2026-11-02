//! Console command lines → argument vectors (Design, done). Contract: `docs/m7-contracts.md`
//! §4.3.
//!
//! The quoting rules are the ones `redis-cli` documents for its interactive input:
//!
//! - arguments are separated by spaces, tabs, `\r` or `\n`;
//! - `"..."` holds an argument with escapes: `\n`, `\r`, `\t`, `\b`, `\a`, `\\`, `\"`, and
//!   `\xHH` (two hex digits, any byte); any other escaped character stands for itself;
//! - `'...'` holds an argument verbatim except `\'`;
//! - a closing quote must be followed by a separator or the end of the line;
//! - quotes may start in the middle of an argument (`key"with space"` is one argument);
//! - an unterminated quote is an error.
//!
//! Arguments are bytes: `\xHH` may produce any byte, so binary keys and values can be typed.

use oxbase_types::{Error, Result};

/// The arguments of one command line (empty for a blank line).
pub fn parse_command_line(line: &str) -> Result<Vec<Vec<u8>>> {
    let bytes = line.as_bytes();
    let mut args = Vec::new();
    let mut i = 0;
    loop {
        while i < bytes.len() && is_separator(bytes[i]) {
            i += 1;
        }
        if i >= bytes.len() {
            return Ok(args);
        }
        let mut arg = Vec::new();
        while i < bytes.len() && !is_separator(bytes[i]) {
            match bytes[i] {
                b'"' => i = double_quoted(bytes, i + 1, &mut arg)?,
                b'\'' => i = single_quoted(bytes, i + 1, &mut arg)?,
                b => {
                    arg.push(b);
                    i += 1;
                }
            }
        }
        args.push(arg);
    }
}

fn is_separator(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n')
}

fn unbalanced() -> Error {
    Error::invalid_input("unbalanced quotes in the command line")
}

/// Reads a `"..."` body starting after the opening quote into `arg`; returns the index after
/// the closing quote (which must be followed by a separator or the end).
fn double_quoted(bytes: &[u8], mut i: usize, arg: &mut Vec<u8>) -> Result<usize> {
    while i < bytes.len() {
        match bytes[i] {
            b'"' => return closed(bytes, i + 1),
            b'\\' if i + 1 < bytes.len() => {
                let next = bytes[i + 1];
                if next == b'x'
                    && i + 3 < bytes.len()
                    && let (Some(hi), Some(lo)) = (hex(bytes[i + 2]), hex(bytes[i + 3]))
                {
                    arg.push((hi << 4) | lo);
                    i += 4;
                    continue;
                }
                arg.push(match next {
                    b'n' => b'\n',
                    b'r' => b'\r',
                    b't' => b'\t',
                    b'b' => 0x08,
                    b'a' => 0x07,
                    other => other,
                });
                i += 2;
            }
            b => {
                arg.push(b);
                i += 1;
            }
        }
    }
    Err(unbalanced())
}

/// Reads a `'...'` body starting after the opening quote into `arg` (only `\'` is an escape).
fn single_quoted(bytes: &[u8], mut i: usize, arg: &mut Vec<u8>) -> Result<usize> {
    while i < bytes.len() {
        match bytes[i] {
            b'\'' => return closed(bytes, i + 1),
            b'\\' if bytes.get(i + 1) == Some(&b'\'') => {
                arg.push(b'\'');
                i += 2;
            }
            b => {
                arg.push(b);
                i += 1;
            }
        }
    }
    Err(unbalanced())
}

/// `i` after a closing quote: fine at the end or before a separator.
fn closed(bytes: &[u8], i: usize) -> Result<usize> {
    if i < bytes.len() && !is_separator(bytes[i]) {
        return Err(Error::invalid_input(
            "a closing quote must be followed by a space",
        ));
    }
    Ok(i)
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Vec<Vec<u8>> {
        parse_command_line(line).unwrap()
    }

    fn text(items: &[&str]) -> Vec<Vec<u8>> {
        items.iter().map(|s| s.as_bytes().to_vec()).collect()
    }

    #[test]
    fn splits_plain_arguments() {
        assert_eq!(
            args("  SET  key\tvalue \r\n"),
            text(&["SET", "key", "value"])
        );
        assert!(args("   ").is_empty());
        assert_eq!(args("GET ü"), text(&["GET", "ü"]));
    }

    #[test]
    fn reads_quotes_and_escapes() {
        assert_eq!(
            args(r#"SET "a key" "line\nnext \"q\" \\ \x00\xff\xZZ""#),
            vec![
                b"SET".to_vec(),
                b"a key".to_vec(),
                b"line\nnext \"q\" \\ \x00\xffxZZ".to_vec(),
            ]
        );
        assert_eq!(args(r"SET k 'it\'s \n'"), text(&["SET", "k", "it's \\n"]));
        assert_eq!(
            args(r#"SET key"with space" """#),
            text(&["SET", "keywith space", ""])
        );
        assert_eq!(args("PING ''"), text(&["PING", ""]));
    }

    #[test]
    fn refuses_unbalanced_quotes() {
        for line in [r#"SET "a"#, "SET 'a", r#"SET "a"b"#, r#"SET "a\""#] {
            assert!(parse_command_line(line).is_err(), "{line}");
        }
    }
}
