//! Column facts from `information_schema.COLUMNS`: the engine-neutral [`DataType`] and the
//! default expression.

use oxbase_dialect::{BacktickQuoting, Quoting};
use oxbase_types::DataType;

/// What `information_schema.COLUMNS` says about a column's type.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TypeFacts {
    /// `DATA_TYPE` (`int`, `varchar`, `longtext`, ...).
    pub data_type: String,
    /// `COLUMN_TYPE` (`int unsigned`, `varchar(20)`, `enum('a','b')`, `tinyint(1)`).
    pub column_type: String,
    /// `CHARACTER_MAXIMUM_LENGTH` (characters; bytes for binary types).
    pub char_len: Option<u32>,
    /// `NUMERIC_PRECISION` / `NUMERIC_SCALE`.
    pub precision: Option<u32>,
    pub scale: Option<u32>,
    /// `DATETIME_PRECISION` (fractional-second digits).
    pub dt_precision: Option<u8>,
}

/// The engine-neutral type of a column.
///
/// `json` is MySQL's binary JSON type. MariaDB reports JSON columns as `longtext` (its `JSON` is
/// an alias with a `json_valid` check); the caller passes `mariadb_json` for those.
pub(crate) fn data_type(t: &TypeFacts, mariadb_json: bool) -> DataType {
    let dt = t.data_type.to_ascii_lowercase();
    let ct = t.column_type.to_ascii_lowercase();
    let unsigned = ct.contains("unsigned");
    let int = |bits| DataType::Int { bits, unsigned };
    if mariadb_json {
        return DataType::Json { binary: false };
    }
    // The driver reads TINYINT(1) as a boolean too (MySQL 8 prints no other display widths).
    if dt == "tinyint" && ct.starts_with("tinyint(1)") {
        return DataType::Bool;
    }
    match dt.as_str() {
        "bool" | "boolean" => DataType::Bool,
        "tinyint" => int(8),
        "smallint" => int(16),
        "mediumint" | "int" | "integer" => int(32),
        "bigint" => int(64),
        "decimal" | "numeric" | "dec" | "fixed" => DataType::Decimal {
            precision: t.precision,
            scale: t.scale,
        },
        "float" => DataType::Float { bits: 32 },
        "double" | "real" | "double precision" => DataType::Float { bits: 64 },
        "char" => DataType::Char { length: t.char_len },
        "varchar" => DataType::VarChar { length: t.char_len },
        "tinytext" | "text" | "mediumtext" | "longtext" => DataType::Text,
        "json" => DataType::Json { binary: true },
        "binary" => DataType::Binary { length: t.char_len },
        "varbinary" => DataType::VarBinary { length: t.char_len },
        "tinyblob" | "blob" | "mediumblob" | "longblob" => DataType::VarBinary { length: None },
        "date" => DataType::Date,
        "time" => DataType::Time {
            precision: t.dt_precision,
        },
        "datetime" => DataType::Timestamp {
            precision: t.dt_precision,
        },
        // The driver reads TIMESTAMP as an instant (the session runs in UTC).
        "timestamp" => DataType::TimestampTz {
            precision: t.dt_precision,
        },
        "enum" => DataType::Enum {
            labels: quoted_list(&t.column_type),
        },
        "uuid" => DataType::Uuid,
        other => DataType::Other {
            name: other.to_owned(),
        },
    }
}

/// The labels of `enum('a','b''c')`: the single-quoted strings after the first `(`.
pub(crate) fn quoted_list(column_type: &str) -> Vec<String> {
    let Some(start) = column_type.find('(') else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut chars = column_type[start + 1..].chars().peekable();
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

/// The column default as an engine-native SQL expression.
///
/// - `expressions` is set for MariaDB 10.2.7+, which already reports an expression (`'new'`,
///   `0`, `current_timestamp()`), and `NULL` for "default NULL"; that one is `None` like a
///   column without a default.
/// - MySQL (and MariaDB before 10.2.7) reports literals unquoted (`new`). MySQL 8.0.13+ marks
///   expressions with `DEFAULT_GENERATED` in `EXTRA`: literals of non-numeric types are quoted
///   here, expressions other than the `CURRENT_TIMESTAMP` family are wrapped in parentheses as in
///   `CREATE TABLE`. Older servers have no marker, but their only expression default is
///   `CURRENT_TIMESTAMP[(n)]` on a temporal column (a string literal of that text is not a valid
///   temporal value), so that one is kept as an expression too.
pub(crate) fn column_default(
    expressions: bool,
    raw: Option<&str>,
    extra: &str,
    ty: &DataType,
) -> Option<String> {
    let raw = raw?;
    if expressions {
        return (raw != "NULL").then(|| raw.to_owned());
    }
    if extra.to_ascii_uppercase().contains("DEFAULT_GENERATED") {
        let upper = raw.trim_start().to_ascii_uppercase();
        let temporal = [
            "CURRENT_TIMESTAMP",
            "NOW(",
            "LOCALTIME",
            "CURRENT_DATE",
            "CURDATE(",
        ]
        .iter()
        .any(|p| upper.starts_with(p));
        return Some(if temporal || raw.starts_with('(') {
            raw.to_owned()
        } else {
            format!("({raw})")
        });
    }
    if is_temporal(ty) && is_current_timestamp(raw) {
        return Some(raw.to_owned());
    }
    let bare = ty.is_numeric()
        || matches!(ty, DataType::Bool)
        || matches!(ty, DataType::Other { name } if name == "bit")
        || raw.starts_with("b'")
        || raw.starts_with("0x");
    Some(if bare {
        raw.to_owned()
    } else {
        BacktickQuoting.quote_str(raw)
    })
}

/// `DATETIME` / `TIMESTAMP` (the types that take a `CURRENT_TIMESTAMP` default).
fn is_temporal(ty: &DataType) -> bool {
    matches!(
        ty,
        DataType::Timestamp { .. } | DataType::TimestampTz { .. }
    )
}

/// `CURRENT_TIMESTAMP`, `current_timestamp()` or `CURRENT_TIMESTAMP(n)`, any case.
fn is_current_timestamp(raw: &str) -> bool {
    let upper = raw.trim().to_ascii_uppercase();
    let Some(rest) = upper.strip_prefix("CURRENT_TIMESTAMP") else {
        return false;
    };
    rest.is_empty()
        || rest
            .strip_prefix('(')
            .and_then(|r| r.strip_suffix(')'))
            .is_some_and(|digits| digits.chars().all(|c| c.is_ascii_digit()))
}

/// `utf8mb4` from `utf8mb4_unicode_ci` (charset names contain no `_`; `binary` is itself).
pub(crate) fn charset_of_collation(collation: &str) -> &str {
    collation.split('_').next().unwrap_or(collation)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(data_type: &str, column_type: &str) -> TypeFacts {
        TypeFacts {
            data_type: data_type.into(),
            column_type: column_type.into(),
            ..TypeFacts::default()
        }
    }

    #[test]
    fn maps_types() {
        assert_eq!(data_type(&facts("int", "int"), false), DataType::INT32);
        assert_eq!(
            data_type(&facts("bigint", "bigint unsigned"), false),
            DataType::Int {
                bits: 64,
                unsigned: true
            }
        );
        assert_eq!(
            data_type(&facts("tinyint", "tinyint(1)"), false),
            DataType::Bool
        );
        assert_eq!(
            data_type(&facts("tinyint", "tinyint(4)"), false),
            DataType::Int {
                bits: 8,
                unsigned: false
            }
        );
        let mut v = facts("varchar", "varchar(20)");
        v.char_len = Some(20);
        assert_eq!(data_type(&v, false), DataType::VarChar { length: Some(20) });
        assert_eq!(data_type(&facts("text", "text"), false), DataType::Text);
        assert_eq!(
            data_type(&facts("json", "json"), false),
            DataType::Json { binary: true }
        );
        assert_eq!(
            data_type(&facts("longtext", "longtext"), true),
            DataType::Json { binary: false }
        );
        assert_eq!(
            data_type(&facts("enum", "enum('a','b''c')"), false),
            DataType::Enum {
                labels: vec!["a".into(), "b'c".into()]
            }
        );
        assert_eq!(
            data_type(&facts("geometry", "geometry"), false),
            DataType::Other {
                name: "geometry".into()
            }
        );
        let mut d = facts("decimal", "decimal(12,2)");
        d.precision = Some(12);
        d.scale = Some(2);
        assert_eq!(
            data_type(&d, false),
            DataType::Decimal {
                precision: Some(12),
                scale: Some(2)
            }
        );
    }

    #[test]
    fn defaults() {
        let int = DataType::INT32;
        let text = DataType::VarChar { length: Some(20) };
        let ts = DataType::Timestamp { precision: None };
        // MySQL
        assert_eq!(column_default(false, None, "", &int), None);
        assert_eq!(
            column_default(false, Some("1"), "", &int).as_deref(),
            Some("1")
        );
        assert_eq!(
            column_default(false, Some("it's"), "", &text).as_deref(),
            Some("'it''s'")
        );
        assert_eq!(
            column_default(false, Some("CURRENT_TIMESTAMP"), "DEFAULT_GENERATED", &ts).as_deref(),
            Some("CURRENT_TIMESTAMP")
        );
        assert_eq!(
            column_default(false, Some("rand()"), "DEFAULT_GENERATED", &int).as_deref(),
            Some("(rand())")
        );
        // MySQL before 8.0.13 (no DEFAULT_GENERATED) and MariaDB before 10.2.7.
        assert_eq!(
            column_default(false, Some("CURRENT_TIMESTAMP"), "", &ts).as_deref(),
            Some("CURRENT_TIMESTAMP")
        );
        assert_eq!(
            column_default(
                false,
                Some("CURRENT_TIMESTAMP(3)"),
                "on update CURRENT_TIMESTAMP(3)",
                &ts
            )
            .as_deref(),
            Some("CURRENT_TIMESTAMP(3)")
        );
        assert_eq!(
            column_default(false, Some("CURRENT_TIMESTAMP"), "", &text).as_deref(),
            Some("'CURRENT_TIMESTAMP'")
        );
        assert_eq!(
            column_default(false, Some("CURRENT_TIMESTAMPX"), "", &ts).as_deref(),
            Some("'CURRENT_TIMESTAMPX'")
        );
        // MariaDB
        assert_eq!(column_default(true, Some("NULL"), "", &int), None);
        assert_eq!(
            column_default(true, Some("'new'"), "", &text).as_deref(),
            Some("'new'")
        );
        assert_eq!(
            column_default(true, Some("current_timestamp()"), "", &ts).as_deref(),
            Some("current_timestamp()")
        );
    }

    #[test]
    fn charsets() {
        assert_eq!(charset_of_collation("utf8mb4_unicode_ci"), "utf8mb4");
        assert_eq!(charset_of_collation("binary"), "binary");
        assert_eq!(quoted_list("set('x','y')"), ["x", "y"]);
        assert_eq!(quoted_list("int"), Vec::<String>::new());
    }
}
