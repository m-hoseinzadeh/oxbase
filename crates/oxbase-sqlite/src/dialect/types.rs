//! Declared column type → engine-neutral [`DataType`].
//!
//! SQLite stores any value in any column; the declared type only gives an affinity. The model
//! still wants a logical type, so well-known type names map to their neutral equivalent (with
//! length, precision and scale) and anything else follows SQLite's affinity rules. The declared
//! text itself always stays in `ColumnDef::native_type`.

use oxbase_types::DataType;

/// Splits `VARCHAR ( 20 )` into the upper-cased, space-normalized base name and the numeric
/// arguments.
fn split(decl: &str) -> (String, Vec<u32>) {
    let (base, args) = match decl.split_once('(') {
        Some((base, rest)) => (base, rest.split(')').next().unwrap_or("")),
        None => (decl, ""),
    };
    let base = base
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_uppercase();
    let args = args
        .split(',')
        .filter_map(|a| a.trim().parse::<u32>().ok())
        .collect();
    (base, args)
}

/// The logical type of a column declared as `decl` (empty when the column has no type).
pub(crate) fn data_type(decl: &str) -> DataType {
    let decl = decl.trim();
    if decl.is_empty() {
        // No declared type: BLOB affinity, any value.
        return DataType::Other {
            name: String::new(),
        };
    }
    let (base, args) = split(decl);
    let first = args.first().copied();
    let second = args.get(1).copied();
    match base.as_str() {
        "INTEGER" | "INT" | "BIGINT" | "INT8" => DataType::INT64,
        "UNSIGNED BIG INT" => DataType::Int {
            bits: 64,
            unsigned: true,
        },
        "MEDIUMINT" | "INT4" => DataType::INT32,
        "SMALLINT" | "INT2" => DataType::INT16,
        "TINYINT" => DataType::Int {
            bits: 8,
            unsigned: false,
        },
        "BOOLEAN" | "BOOL" => DataType::Bool,
        "REAL" | "DOUBLE" | "DOUBLE PRECISION" | "FLOAT" => DataType::FLOAT64,
        // `DECIMAL_TEXT`: the exact decimal declaration of tables made for imported data
        // (`TEXT` affinity, see `ddl::exact_decimal_type`).
        "NUMERIC" | "DECIMAL" | "DECIMAL_TEXT" => DataType::Decimal {
            precision: first,
            scale: second,
        },
        "VARCHAR" | "CHARACTER VARYING" | "VARYING CHARACTER" | "NVARCHAR" => {
            DataType::VarChar { length: first }
        }
        "CHAR" | "CHARACTER" | "NCHAR" | "NATIVE CHARACTER" => DataType::Char { length: first },
        "TEXT" | "CLOB" => DataType::Text,
        "BLOB" => DataType::VarBinary { length: None },
        "DATE" => DataType::Date,
        "DATETIME" | "TIMESTAMP" => DataType::Timestamp { precision: None },
        "TIME" => DataType::Time { precision: None },
        "UUID" => DataType::Uuid,
        "JSON" => DataType::Json { binary: false },
        "JSONB" => DataType::Json { binary: true },
        _ => affinity(&base, decl),
    }
}

/// SQLite's affinity rules, in its order, for type names without a dedicated mapping.
fn affinity(base: &str, decl: &str) -> DataType {
    if base.contains("INT") {
        DataType::INT64
    } else if base.contains("CHAR") || base.contains("CLOB") || base.contains("TEXT") {
        DataType::Text
    } else if base.contains("BLOB") {
        DataType::VarBinary { length: None }
    } else if base.contains("REAL") || base.contains("FLOA") || base.contains("DOUB") {
        DataType::FLOAT64
    } else {
        DataType::Other {
            name: decl.to_owned(),
        }
    }
}

/// `true` when `decl` makes an `INTEGER PRIMARY KEY` column an alias of the rowid (the type must
/// be exactly `INTEGER`, any case).
pub(crate) fn is_rowid_alias_type(decl: &str) -> bool {
    decl.trim().eq_ignore_ascii_case("INTEGER")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_decimals_read_back_as_decimals() {
        assert_eq!(
            data_type("DECIMAL_TEXT(38,10)"),
            DataType::Decimal {
                precision: Some(38),
                scale: Some(10)
            }
        );
        assert_eq!(
            data_type("decimal_text"),
            DataType::Decimal {
                precision: None,
                scale: None
            }
        );
        assert_eq!(
            crate::convert::affinity(Some("DECIMAL_TEXT(38,10)")),
            crate::convert::Affinity::Text
        );
    }

    #[test]
    fn maps_known_names_and_affinities() {
        assert_eq!(data_type("INTEGER"), DataType::INT64);
        assert_eq!(data_type("int"), DataType::INT64);
        assert_eq!(data_type("smallint"), DataType::INT16);
        assert_eq!(
            data_type("varchar( 20 )"),
            DataType::VarChar { length: Some(20) }
        );
        assert_eq!(
            data_type("character  varying(5)"),
            DataType::VarChar { length: Some(5) }
        );
        assert_eq!(data_type("NCHAR(3)"), DataType::Char { length: Some(3) });
        assert_eq!(
            data_type("decimal(10, 2)"),
            DataType::Decimal {
                precision: Some(10),
                scale: Some(2)
            }
        );
        assert_eq!(data_type("TEXT"), DataType::Text);
        assert_eq!(data_type("double precision"), DataType::FLOAT64);
        assert_eq!(data_type("boolean"), DataType::Bool);
        assert_eq!(
            data_type("datetime"),
            DataType::Timestamp { precision: None }
        );
        // Affinity fallbacks.
        assert_eq!(data_type("UNSIGNED INTEGER"), DataType::INT64);
        assert_eq!(data_type("LONGTEXT"), DataType::Text);
        assert_eq!(data_type("FLOATING"), DataType::FLOAT64);
        assert_eq!(
            data_type("money"),
            DataType::Other {
                name: "money".into()
            }
        );
        assert_eq!(
            data_type(""),
            DataType::Other {
                name: String::new()
            }
        );
        assert!(is_rowid_alias_type(" integer "));
        assert!(!is_rowid_alias_type("INT"));
    }
}
