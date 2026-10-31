//! Native MySQL / MariaDB type names (M3): the type text the DDL generator writes for a column,
//! its canonical form for comparison, and whether a type change can lose data.

use oxbase_dialect::{BacktickQuoting, Quoting};
use oxbase_types::{ColumnDef, DataType, Error, Result, ServerVersion};

/// The type text for `col` in a column definition.
///
/// The column's `native_type` when it is set (engine-native text from the model), else one
/// derived from the neutral [`DataType`]. On MariaDB a `JSON` column is always written `json`,
/// because MariaDB stores it as `longtext` plus an automatic `json_valid` check, and writing
/// `longtext` would lose that check.
pub(crate) fn column_type(col: &ColumnDef, mariadb: bool, v: &ServerVersion) -> Result<String> {
    if mariadb && matches!(col.data_type, DataType::Json { .. }) {
        return Ok("json".to_owned());
    }
    let native = col.native_type.trim();
    if native.is_empty() {
        return from_data_type(&col.data_type, mariadb, v).map_err(|e| in_column(e, &col.name));
    }
    check_type_text(native).map_err(|why| {
        Error::invalid_input(format!(
            "column {}: type '{native}' is not a type name ({why})",
            col.name
        ))
    })?;
    Ok(native.to_owned())
}

/// `e` with "column <name>: " in front of its message (same kind).
fn in_column(e: Error, name: &str) -> Error {
    match e {
        Error::Unsupported { message } => Error::unsupported(format!("column {name}: {message}")),
        Error::InvalidInput { message } => {
            Error::invalid_input(format!("column {name}: {message}"))
        }
        other => other,
    }
}

/// Lexical sanity check of a native type text: words, numbers, spaces, commas, parentheses and
/// single-quoted labels (`enum('a','b')`), nothing that could end the statement.
fn check_type_text(text: &str) -> std::result::Result<(), &'static str> {
    let mut in_string = false;
    let mut depth = 0usize;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if in_string {
            match c {
                '\'' if chars.peek() == Some(&'\'') => {
                    chars.next();
                }
                '\'' => in_string = false,
                '\\' => {
                    chars.next();
                }
                _ => {}
            }
            continue;
        }
        match c {
            '\'' => in_string = true,
            '(' => depth += 1,
            ')' => {
                depth = depth.checked_sub(1).ok_or("unbalanced parentheses")?;
            }
            c if c.is_alphanumeric() || c == '_' || c == ' ' || c == ',' || c == '.' => {}
            _ => return Err("unexpected character"),
        }
    }
    if in_string {
        return Err("unterminated string");
    }
    if depth != 0 {
        return Err("unbalanced parentheses");
    }
    Ok(())
}

/// The native type for a neutral [`DataType`] (when a column has no `native_type`).
pub(crate) fn from_data_type(t: &DataType, mariadb: bool, v: &ServerVersion) -> Result<String> {
    let sized = |name: &str, n: Option<u32>| match n {
        Some(n) => format!("{name}({n})"),
        None => name.to_owned(),
    };
    let fraction = |name: &str, p: Option<u8>| match p {
        Some(p) if p > 0 => format!("{name}({p})"),
        _ => name.to_owned(),
    };
    Ok(match t {
        DataType::Bool => "tinyint(1)".to_owned(),
        DataType::Int { bits, unsigned } => {
            let name = match bits {
                0..=8 => "tinyint",
                9..=16 => "smallint",
                17..=32 => "int",
                _ => "bigint",
            };
            if *unsigned {
                format!("{name} unsigned")
            } else {
                name.to_owned()
            }
        }
        DataType::Float { bits } if *bits <= 32 => "float".to_owned(),
        DataType::Float { .. } => "double".to_owned(),
        DataType::Decimal { precision, scale } => match (precision, scale) {
            (Some(p), Some(s)) => format!("decimal({p},{s})"),
            (Some(p), None) => format!("decimal({p},0)"),
            _ => "decimal(65,30)".to_owned(),
        },
        DataType::Char { length } => sized("char", *length),
        DataType::VarChar { length: Some(n) } => format!("varchar({n})"),
        DataType::VarChar { length: None } => {
            return Err(Error::invalid_input(
                "varchar needs a length on MySQL and MariaDB",
            ));
        }
        DataType::Text => "longtext".to_owned(),
        DataType::Binary { length } => sized("binary", *length),
        DataType::VarBinary { length: Some(n) } => format!("varbinary({n})"),
        DataType::VarBinary { length: None } => "longblob".to_owned(),
        DataType::Date => "date".to_owned(),
        DataType::Time { precision } => fraction("time", *precision),
        DataType::Timestamp { precision } => fraction("datetime", *precision),
        DataType::TimestampTz { precision } => fraction("timestamp", *precision),
        DataType::Json { .. } => "json".to_owned(),
        DataType::Uuid if mariadb && v.at_least(10, 7) => "uuid".to_owned(),
        DataType::Uuid => {
            return Err(Error::unsupported(
                "this server has no uuid type; use char(36) or binary(16)",
            ));
        }
        DataType::Enum { labels } => {
            let list: Vec<String> = labels
                .iter()
                .map(|l| BacktickQuoting.quote_str(l))
                .collect();
            format!("enum({})", list.join(","))
        }
        DataType::Other { name } => {
            if name.is_empty()
                || !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ' ')
            {
                return Err(Error::invalid_input(format!("'{name}' is not a type name")));
            }
            name.clone()
        }
        DataType::Interval => {
            return Err(Error::unsupported(
                "MySQL and MariaDB have no interval type",
            ));
        }
        DataType::Array { .. } => {
            return Err(Error::unsupported("MySQL and MariaDB have no array types"));
        }
    })
}

/// The canonical spelling of a native type, so `int(11)` (MariaDB, display width) equals `int`
/// (MySQL 8), `integer` equals `int`, `boolean` equals `tinyint(1)`, `decimal` equals
/// `decimal(10,0)` and `enum('a', 'b')` equals `enum('a','b')`.
pub(crate) fn canonical_type(native: &str) -> String {
    // Lowercase outside quotes, whitespace collapsed and dropped around punctuation.
    let mut text = String::with_capacity(native.len());
    let mut in_string = false;
    let mut pending_space = false;
    for c in native.trim().chars() {
        if in_string {
            text.push(c);
            if c == '\'' {
                in_string = false;
            }
            continue;
        }
        if c.is_whitespace() {
            pending_space = true;
            continue;
        }
        let punct = matches!(c, '(' | ')' | ',');
        if pending_space && !punct && !text.ends_with(['(', ',']) {
            text.push(' ');
        }
        pending_space = false;
        if c == '\'' {
            in_string = true;
        }
        text.extend(c.to_lowercase());
    }

    let (head, rest) = match text.find(['(', ' ']) {
        Some(pos) => text.split_at(pos),
        None => (text.as_str(), ""),
    };
    let (args, tail) = match rest.strip_prefix('(') {
        Some(after) => match after.find(')') {
            Some(end) if !head.starts_with("enum") && !head.starts_with("set") => {
                (Some(&after[..end]), after[end + 1..].trim())
            }
            _ => (None, rest.trim()),
        },
        None => (None, rest.trim()),
    };
    let mut words: Vec<&str> = tail.split_whitespace().collect();
    if words.contains(&"zerofill") && !words.contains(&"unsigned") {
        words.insert(0, "unsigned");
    }
    let tail = words.join(" ");
    let with_tail = |base: String| {
        if tail.is_empty() {
            base
        } else {
            format!("{base} {tail}")
        }
    };

    let head = match head {
        "integer" => "int",
        "bool" | "boolean" => return "tinyint(1)".to_owned(),
        "dec" | "numeric" | "fixed" => "decimal",
        "real" => "double",
        "double" if tail.starts_with("precision") => {
            return with_tail_rest("double", tail.trim_start_matches("precision").trim());
        }
        other => other,
    };
    match (head, args) {
        ("tinyint", Some("1")) => with_tail("tinyint(1)".to_owned()),
        ("tinyint" | "smallint" | "mediumint" | "int" | "bigint", _)
        | ("datetime" | "timestamp" | "time", Some("0")) => with_tail(head.to_owned()),
        ("decimal", None) => with_tail("decimal(10,0)".to_owned()),
        ("decimal", Some(a)) if !a.contains(',') => with_tail(format!("decimal({a},0)")),
        ("char" | "binary" | "bit", None) => with_tail(format!("{head}(1)")),
        ("year", _) => "year".to_owned(),
        ("float", Some(p)) if !p.contains(',') => match p.parse::<u32>() {
            Ok(p) if p <= 24 => with_tail("float".to_owned()),
            Ok(_) => with_tail("double".to_owned()),
            Err(_) => with_tail(format!("float({p})")),
        },
        (_, Some(a)) => with_tail(format!("{head}({a})")),
        (_, None) => {
            if rest.starts_with('(') {
                // enum / set: keep the whole label list.
                with_tail_rest(&format!("{head}{}", rest.trim_end()), "")
            } else {
                with_tail(head.to_owned())
            }
        }
    }
}

fn with_tail_rest(base: &str, tail: &str) -> String {
    if tail.is_empty() {
        base.to_owned()
    } else {
        format!("{base} {tail}")
    }
}

/// Rank of the text and blob families by capacity.
fn lob_rank(native: &str) -> Option<(bool, u8)> {
    let name = canonical_type(native);
    let rank = match name.as_str() {
        "tinytext" => (true, 1),
        "text" => (true, 2),
        "mediumtext" => (true, 3),
        "longtext" | "json" => (true, 4),
        "tinyblob" => (false, 1),
        "blob" => (false, 2),
        "mediumblob" => (false, 3),
        "longblob" => (false, 4),
        _ => return None,
    };
    Some(rank)
}

/// `true` when changing `old`'s type to `new`'s can lose or reject data (a narrowing or a change
/// of kind). Same type, more length, more precision, a wider integer and the like are safe.
pub(crate) fn is_narrowing(old: &ColumnDef, new: &ColumnDef) -> bool {
    use DataType as T;
    if !old.native_type.is_empty()
        && !new.native_type.is_empty()
        && canonical_type(&old.native_type) == canonical_type(&new.native_type)
    {
        return false;
    }
    if let (Some(a), Some(b)) = (lob_rank(&old.native_type), lob_rank(&new.native_type)) {
        return a.0 != b.0 || b.1 < a.1;
    }
    let ge = |a: Option<u32>, b: Option<u32>| match (a, b) {
        (_, None) => true,
        (None, Some(_)) => false,
        (Some(a), Some(b)) => b >= a,
    };
    match (&old.data_type, &new.data_type) {
        (a, b) if a == b => false,
        (
            T::Int {
                bits: b1,
                unsigned: u1,
            },
            T::Int {
                bits: b2,
                unsigned: u2,
            },
        ) => !((u1 == u2 && b2 >= b1) || (*u1 && !*u2 && b2 > b1)),
        (T::Bool, T::Int { .. })
        | (T::Char { .. } | T::VarChar { .. } | T::Enum { .. }, T::Text)
        | (T::Date, T::Timestamp { .. } | T::TimestampTz { .. }) => false,
        (T::Int { bits, .. }, T::Float { bits: 64 }) => *bits > 32,
        (T::Float { bits: b1 }, T::Float { bits: b2 }) => b2 < b1,
        (
            T::Decimal {
                precision: p1,
                scale: s1,
            },
            T::Decimal {
                precision: p2,
                scale: s2,
            },
        ) => {
            let (p1, s1, p2, s2) = (
                p1.unwrap_or(10),
                s1.unwrap_or(0),
                p2.unwrap_or(10),
                s2.unwrap_or(0),
            );
            !(s2 >= s1 && p2.saturating_sub(s2) >= p1.saturating_sub(s1))
        }
        (
            T::Char { length: a } | T::VarChar { length: a },
            T::Char { length: b } | T::VarChar { length: b },
        )
        | (
            T::Binary { length: a } | T::VarBinary { length: a },
            T::Binary { length: b } | T::VarBinary { length: b },
        ) => !ge(*a, *b),
        (
            T::Timestamp { precision: a } | T::TimestampTz { precision: a },
            T::Timestamp { precision: b } | T::TimestampTz { precision: b },
        )
        | (T::Time { precision: a }, T::Time { precision: b }) => b.unwrap_or(0) < a.unwrap_or(0),
        (T::Enum { labels: a }, T::Enum { labels: b }) => !a.iter().all(|l| b.contains(l)),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(native: &str, data_type: DataType) -> ColumnDef {
        let mut c = ColumnDef::new("c", data_type);
        c.native_type = native.into();
        c
    }

    #[test]
    fn canonical_types() {
        for (a, b) in [
            ("int(11)", "int"),
            ("INT", "integer"),
            ("int(10) unsigned", "int unsigned"),
            ("bigint(20)", "bigint"),
            ("tinyint(4)", "tinyint"),
            ("boolean", "tinyint(1)"),
            ("decimal", "decimal(10,0)"),
            ("numeric(12, 2)", "decimal(12,2)"),
            ("decimal(5)", "decimal(5,0)"),
            ("char", "char(1)"),
            ("double precision", "double"),
            ("real", "double"),
            ("year(4)", "year"),
            ("datetime(0)", "datetime"),
            ("float(10)", "float"),
            ("float(30)", "double"),
            ("varchar( 20 )", "varchar(20)"),
            ("enum('a', 'b')", "enum('a','b')"),
            ("int zerofill", "int unsigned zerofill"),
        ] {
            assert_eq!(canonical_type(a), canonical_type(b), "{a} vs {b}");
        }
        for (a, b) in [
            ("tinyint(1)", "tinyint"),
            ("varchar(20)", "varchar(21)"),
            ("int", "int unsigned"),
            ("enum('a','b')", "enum('a','B')"),
            ("datetime(6)", "datetime"),
            ("decimal(12,2)", "decimal(12,3)"),
        ] {
            assert_ne!(canonical_type(a), canonical_type(b), "{a} vs {b}");
        }
        assert_eq!(canonical_type("enum('A b','c')"), "enum('A b','c')");
        assert_eq!(canonical_type("VARCHAR(20)"), "varchar(20)");
    }

    #[test]
    fn derived_types() {
        let v = ServerVersion::new(8, 4, 0);
        let m = ServerVersion::new(11, 8, 0);
        let derive = |t: DataType| from_data_type(&t, false, &v).unwrap();
        assert_eq!(derive(DataType::Bool), "tinyint(1)");
        assert_eq!(derive(DataType::INT64), "bigint");
        assert_eq!(
            derive(DataType::Int {
                bits: 32,
                unsigned: true
            }),
            "int unsigned"
        );
        assert_eq!(
            derive(DataType::Decimal {
                precision: Some(12),
                scale: Some(2)
            }),
            "decimal(12,2)"
        );
        assert_eq!(
            derive(DataType::VarChar { length: Some(20) }),
            "varchar(20)"
        );
        assert_eq!(derive(DataType::Text), "longtext");
        assert_eq!(
            derive(DataType::Timestamp { precision: Some(6) }),
            "datetime(6)"
        );
        assert_eq!(
            derive(DataType::Enum {
                labels: vec!["a".into(), "it's".into()]
            }),
            "enum('a','it''s')"
        );
        assert!(from_data_type(&DataType::VarChar { length: None }, false, &v).is_err());
        assert!(matches!(
            from_data_type(&DataType::Uuid, false, &v),
            Err(Error::Unsupported { .. })
        ));
        assert_eq!(from_data_type(&DataType::Uuid, true, &m).unwrap(), "uuid");
        assert!(matches!(
            from_data_type(&DataType::Interval, true, &m),
            Err(Error::Unsupported { .. })
        ));
        assert!(
            from_data_type(
                &DataType::Other {
                    name: "int; DROP".into()
                },
                false,
                &v
            )
            .is_err()
        );

        // Native types are used as they are, after a lexical check.
        let c = col("int unsigned", DataType::INT32);
        assert_eq!(column_type(&c, false, &v).unwrap(), "int unsigned");
        let bad = col("int); DROP TABLE x; --", DataType::INT32);
        assert!(column_type(&bad, false, &v).is_err());
        let json = col("longtext", DataType::Json { binary: false });
        assert_eq!(column_type(&json, true, &m).unwrap(), "json");
        assert_eq!(column_type(&json, false, &v).unwrap(), "longtext");
    }

    #[test]
    fn narrowing() {
        let int = col("int", DataType::INT32);
        let big = col("bigint", DataType::INT64);
        assert!(!is_narrowing(&int, &big));
        assert!(is_narrowing(&big, &int));
        let v20 = col("varchar(20)", DataType::VarChar { length: Some(20) });
        let v40 = col("varchar(40)", DataType::VarChar { length: Some(40) });
        assert!(!is_narrowing(&v20, &v40));
        assert!(is_narrowing(&v40, &v20));
        let text = col("text", DataType::Text);
        let long = col("longtext", DataType::Text);
        assert!(!is_narrowing(&v20, &text));
        assert!(!is_narrowing(&text, &long));
        assert!(is_narrowing(&long, &text));
        let d1 = col(
            "decimal(10,2)",
            DataType::Decimal {
                precision: Some(10),
                scale: Some(2),
            },
        );
        let d2 = col(
            "decimal(12,2)",
            DataType::Decimal {
                precision: Some(12),
                scale: Some(2),
            },
        );
        assert!(!is_narrowing(&d1, &d2));
        assert!(is_narrowing(&d2, &d1));
        assert!(is_narrowing(&v20, &int));
        assert!(!is_narrowing(&int, &col("int(11)", DataType::INT32)));
    }
}
