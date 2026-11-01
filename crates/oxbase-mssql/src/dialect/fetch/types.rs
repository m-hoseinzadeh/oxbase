//! Native type spellings and logical [`DataType`]s of catalog columns and parameters
//! (docs/m4-contracts.md §3.3-3.4).
//!
//! The catalog gives a type name, the column's `max_length` (bytes, `-1` for `(max)`),
//! `precision` and `scale`; the spelling is rebuilt from them as DDL writes it (`varchar(20)`,
//! `nvarchar(max)`, `decimal(12,2)`, `datetime2(6)`). Alias types are written `schema.name`
//! (unquoted) and take the logical type of their base type.

use oxbase_types::DataType;

use super::rows::Row;
use oxbase_types::Result;

/// The type facts of a column or parameter, as the catalog reports them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TypeInfo {
    /// `sys.types.name` of the declared type.
    pub name: String,
    /// The schema of the declared type.
    pub schema: String,
    pub user_defined: bool,
    /// CLR type (`hierarchyid`, `geometry`, `geography`, user assemblies).
    pub assembly: bool,
    pub table_type: bool,
    /// The system base type of an alias type.
    pub base: Option<String>,
    pub max_length: i64,
    pub precision: i64,
    pub scale: i64,
}

impl TypeInfo {
    /// Reads the columns `type_columns!` and the length columns produce.
    pub(crate) fn from_row(row: Row<'_>) -> Result<Self> {
        Ok(Self {
            name: row.string("type_name")?,
            schema: row.opt_string("type_schema")?.unwrap_or_default(),
            user_defined: row.flag("is_user_defined")?,
            assembly: row.flag("is_assembly_type")?,
            table_type: row.flag("is_table_type")?,
            base: row.opt_string("base_type")?,
            max_length: row.opt_i64("max_length")?.unwrap_or(0),
            precision: row.opt_i64("num_precision")?.unwrap_or(0),
            scale: row.opt_i64("scale")?.unwrap_or(0),
        })
    }

    /// The type as DDL writes it: `schema.name` for user-defined types, else the system
    /// spelling with its length, precision or scale.
    pub(crate) fn native(&self) -> String {
        if self.user_defined {
            format!("{}.{}", self.schema, self.name)
        } else {
            system_spelling(&self.name, self.max_length, self.precision, self.scale)
        }
    }

    /// The logical type: the base type's for alias types.
    pub(crate) fn data_type(&self) -> DataType {
        if self.user_defined {
            if self.table_type || self.assembly {
                return DataType::Other {
                    name: self.native(),
                };
            }
            if let Some(base) = &self.base {
                return logical(base, self.max_length, self.precision, self.scale);
            }
            return DataType::Other {
                name: self.native(),
            };
        }
        logical(&self.name, self.max_length, self.precision, self.scale)
    }
}

/// A length in characters or bytes, or `None` for `(max)`.
fn length(max_length: i64, bytes_per_char: i64) -> Option<u32> {
    if max_length < 0 {
        None
    } else {
        u32::try_from(max_length / bytes_per_char).ok()
    }
}

fn len_text(max_length: i64, bytes_per_char: i64) -> String {
    length(max_length, bytes_per_char).map_or_else(|| "max".to_owned(), |n| n.to_string())
}

/// The spelling of system type `name` (lower case as `sys.types` names it).
pub(crate) fn system_spelling(name: &str, max_length: i64, precision: i64, scale: i64) -> String {
    match name {
        "char" | "varchar" | "binary" | "varbinary" => {
            format!("{name}({})", len_text(max_length, 1))
        }
        "nchar" | "nvarchar" => format!("{name}({})", len_text(max_length, 2)),
        "decimal" | "numeric" => format!("{name}({precision},{scale})"),
        "time" | "datetime2" | "datetimeoffset" => format!("{name}({scale})"),
        "float" if precision != 53 && precision > 0 => format!("float({precision})"),
        "timestamp" => "rowversion".to_owned(),
        // 8-byte header plus 4 bytes per float32 dimension.
        "vector" if max_length > 8 => format!("vector({})", (max_length - 8) / 4),
        other => other.to_owned(),
    }
}

/// The logical type of system type `name`.
pub(crate) fn logical(name: &str, max_length: i64, precision: i64, scale: i64) -> DataType {
    let p = u32::try_from(precision).ok();
    let s = u32::try_from(scale).ok();
    let frac = u8::try_from(scale).ok();
    match name {
        "bit" => DataType::Bool,
        "tinyint" => DataType::Int {
            bits: 8,
            unsigned: true,
        },
        "smallint" => DataType::Int {
            bits: 16,
            unsigned: false,
        },
        "int" => DataType::Int {
            bits: 32,
            unsigned: false,
        },
        "bigint" => DataType::Int {
            bits: 64,
            unsigned: false,
        },
        "real" => DataType::Float { bits: 32 },
        "float" => DataType::Float {
            bits: if (1..=24).contains(&precision) {
                32
            } else {
                64
            },
        },
        "decimal" | "numeric" => DataType::Decimal {
            precision: p,
            scale: s,
        },
        "money" => DataType::Decimal {
            precision: Some(19),
            scale: Some(4),
        },
        "smallmoney" => DataType::Decimal {
            precision: Some(10),
            scale: Some(4),
        },
        "char" => DataType::Char {
            length: length(max_length, 1),
        },
        "nchar" => DataType::Char {
            length: length(max_length, 2),
        },
        "varchar" | "nvarchar" | "text" | "ntext" => {
            if max_length < 0 || name.ends_with("text") {
                DataType::Text
            } else {
                DataType::VarChar {
                    length: length(max_length, if name == "nvarchar" { 2 } else { 1 }),
                }
            }
        }
        "sysname" => DataType::VarChar { length: Some(128) },
        "binary" => DataType::Binary {
            length: length(max_length, 1),
        },
        "varbinary" => DataType::VarBinary {
            length: length(max_length, 1),
        },
        "image" => DataType::VarBinary { length: None },
        "date" => DataType::Date,
        "time" => DataType::Time { precision: frac },
        "datetime" => DataType::Timestamp { precision: Some(3) },
        "smalldatetime" => DataType::Timestamp { precision: Some(0) },
        "datetime2" => DataType::Timestamp { precision: frac },
        "datetimeoffset" => DataType::TimestampTz { precision: frac },
        "uniqueidentifier" => DataType::Uuid,
        "json" => DataType::Json { binary: true },
        "timestamp" | "rowversion" => DataType::Binary { length: Some(8) },
        other => DataType::Other {
            name: other.to_owned(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn sys(name: &str, max_length: i64, precision: i64, scale: i64) -> TypeInfo {
        TypeInfo {
            name: name.into(),
            schema: "sys".into(),
            user_defined: false,
            assembly: false,
            table_type: false,
            base: None,
            max_length,
            precision,
            scale,
        }
    }

    #[test]
    fn spellings_follow_ddl() {
        let cases = [
            (sys("int", 4, 10, 0), "int"),
            (sys("varchar", 20, 0, 0), "varchar(20)"),
            (sys("varchar", -1, 0, 0), "varchar(max)"),
            (sys("nvarchar", 100, 0, 0), "nvarchar(50)"),
            (sys("nvarchar", -1, 0, 0), "nvarchar(max)"),
            (sys("nchar", 20, 0, 0), "nchar(10)"),
            (sys("binary", 16, 0, 0), "binary(16)"),
            (sys("varbinary", -1, 0, 0), "varbinary(max)"),
            (sys("decimal", 9, 12, 2), "decimal(12,2)"),
            (sys("numeric", 17, 38, 10), "numeric(38,10)"),
            (sys("datetime2", 8, 26, 6), "datetime2(6)"),
            (sys("time", 5, 16, 7), "time(7)"),
            (sys("datetimeoffset", 10, 34, 7), "datetimeoffset(7)"),
            (sys("datetime", 8, 23, 3), "datetime"),
            (sys("float", 8, 53, 0), "float"),
            (sys("real", 4, 24, 0), "real"),
            (sys("timestamp", 8, 0, 0), "rowversion"),
            (sys("vector", 20, 0, 0), "vector(3)"),
            (sys("json", -1, 0, 0), "json"),
            (sys("hierarchyid", 892, 0, 0), "hierarchyid"),
            (sys("sysname", 256, 0, 0), "sysname"),
        ];
        for (info, want) in cases {
            assert_eq!(info.native(), want, "{info:?}");
        }
    }

    #[test]
    fn logical_types_follow_the_contract_table() {
        let cases = [
            (sys("bit", 1, 1, 0), DataType::Bool),
            (
                sys("tinyint", 1, 3, 0),
                DataType::Int {
                    bits: 8,
                    unsigned: true,
                },
            ),
            (
                sys("bigint", 8, 19, 0),
                DataType::Int {
                    bits: 64,
                    unsigned: false,
                },
            ),
            (sys("real", 4, 24, 0), DataType::Float { bits: 32 }),
            (sys("float", 8, 53, 0), DataType::Float { bits: 64 }),
            (
                sys("decimal", 9, 12, 2),
                DataType::Decimal {
                    precision: Some(12),
                    scale: Some(2),
                },
            ),
            (
                sys("money", 8, 19, 4),
                DataType::Decimal {
                    precision: Some(19),
                    scale: Some(4),
                },
            ),
            (
                sys("smallmoney", 4, 10, 4),
                DataType::Decimal {
                    precision: Some(10),
                    scale: Some(4),
                },
            ),
            (sys("char", 10, 0, 0), DataType::Char { length: Some(10) }),
            (sys("nchar", 20, 0, 0), DataType::Char { length: Some(10) }),
            (
                sys("nvarchar", 100, 0, 0),
                DataType::VarChar { length: Some(50) },
            ),
            (sys("nvarchar", -1, 0, 0), DataType::Text),
            (sys("varchar", -1, 0, 0), DataType::Text),
            (sys("ntext", 16, 0, 0), DataType::Text),
            (
                sys("binary", 16, 0, 0),
                DataType::Binary { length: Some(16) },
            ),
            (
                sys("varbinary", 64, 0, 0),
                DataType::VarBinary { length: Some(64) },
            ),
            (
                sys("varbinary", -1, 0, 0),
                DataType::VarBinary { length: None },
            ),
            (sys("image", 16, 0, 0), DataType::VarBinary { length: None }),
            (sys("date", 3, 10, 0), DataType::Date),
            (sys("time", 5, 16, 7), DataType::Time { precision: Some(7) }),
            (
                sys("datetime", 8, 23, 3),
                DataType::Timestamp { precision: Some(3) },
            ),
            (
                sys("smalldatetime", 4, 16, 0),
                DataType::Timestamp { precision: Some(0) },
            ),
            (
                sys("datetime2", 8, 27, 7),
                DataType::Timestamp { precision: Some(7) },
            ),
            (
                sys("datetimeoffset", 10, 34, 7),
                DataType::TimestampTz { precision: Some(7) },
            ),
            (sys("uniqueidentifier", 16, 0, 0), DataType::Uuid),
            (sys("json", -1, 0, 0), DataType::Json { binary: true }),
            (sys("xml", -1, 0, 0), DataType::Other { name: "xml".into() }),
            (
                sys("vector", 20, 0, 0),
                DataType::Other {
                    name: "vector".into(),
                },
            ),
            (
                sys("timestamp", 8, 0, 0),
                DataType::Binary { length: Some(8) },
            ),
            (
                sys("sql_variant", 8016, 0, 0),
                DataType::Other {
                    name: "sql_variant".into(),
                },
            ),
            (
                sys("geography", -1, 0, 0),
                DataType::Other {
                    name: "geography".into(),
                },
            ),
            (
                sys("sysname", 256, 0, 0),
                DataType::VarChar { length: Some(128) },
            ),
        ];
        for (info, want) in cases {
            assert_eq!(info.data_type(), want, "{info:?}");
        }
    }

    #[test]
    fn alias_types_keep_their_name_and_take_the_base_type() {
        let alias = TypeInfo {
            name: "m4_code_t".into(),
            schema: "m4".into(),
            user_defined: true,
            assembly: false,
            table_type: false,
            base: Some("varchar".into()),
            max_length: 20,
            precision: 0,
            scale: 0,
        };
        assert_eq!(alias.native(), "m4.m4_code_t");
        assert_eq!(alias.data_type(), DataType::VarChar { length: Some(20) });
        let table = TypeInfo {
            table_type: true,
            base: None,
            name: "m4_rows_t".into(),
            ..alias
        };
        assert_eq!(
            table.data_type(),
            DataType::Other {
                name: "m4.m4_rows_t".into()
            }
        );
    }
}
