//! The per-engine [`TypeMapper`](oxbase_dialect::TypeMapper) cells (`docs/m8-contracts.md`
//! §4.1), one module per SQL engine. Owner of the bodies marked `unimplemented!("m8:
//! transfer")`: the **transfer** stream.
//!
//! Each module documents its table: per logical type, the target spelling and the fidelity
//! with its reason. The facts come from the engines' public documentation (type ranges,
//! precision limits, time-zone handling); the golden tests per engine pair pin
//! [`crate::mapping::pair_report`] for [`crate::mapping::representative_types`].

pub mod mssql;
pub mod mysql;
pub mod postgres;
pub mod sqlite;

/// `precision` capped at `max` fractional-second digits, and whether the cap cut digits.
/// `None` (the engine's default precision) stays `None`.
pub(crate) fn cap_precision(precision: Option<u8>, max: u8) -> (Option<u8>, bool) {
    match precision {
        Some(p) if p > max => (Some(max), true),
        other => (other, false),
    }
}

/// The labels of a list of SQL string literals (`'a', 'b''c'`): every single-quoted string,
/// with doubled quotes (and backslash escapes) undone.
pub(crate) fn quoted_labels(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\'' {
            continue;
        }
        let mut label = String::new();
        while let Some(c) = chars.next() {
            match c {
                '\'' if chars.peek() == Some(&'\'') => {
                    chars.next();
                    label.push('\'');
                }
                '\'' => break,
                '\\' => {
                    if let Some(next) = chars.next() {
                        label.push(next);
                    }
                }
                c => label.push(c),
            }
        }
        out.push(label);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(cap_precision(Some(7), 6), (Some(6), true));
        assert_eq!(cap_precision(Some(3), 6), (Some(3), false));
        assert_eq!(cap_precision(None, 6), (None, false));
        assert_eq!(
            quoted_labels("'a', 'b''c' ,'d'"),
            vec!["a".to_owned(), "b'c".to_owned(), "d".to_owned()]
        );
        assert_eq!(quoted_labels(""), Vec::<String>::new());
    }
}
