//! SQL Server → Arrow type mapping of result sets, and oxbase [`Value`](oxbase_types::Value)s
//! → statement parameters (driver stream). Contract: `docs/m4-contracts.md` §2.5.
//!
//! tiberius describes a result column only by its name and a coarse
//! [`tiberius::ColumnType`] (no length, precision, scale, nullability or width of the nullable
//! integer and float types), and the schema must be known before the first row. The mapping is
//! therefore by `ColumnType`:
//!
//! | SQL Server | `ColumnType` | Arrow | Notes |
//! |---|---|---|---|
//! | `bit` | `Bit`, `Bitn` | `Boolean` | |
//! | `tinyint`, `smallint`, `int`, `bigint` | `Int1`, `Int2`, `Int4`, `Int8`, `Intn` | `Int64` | one type for every integer (`Intn` hides the width) |
//! | `real`, `float` | `Float4`, `Float8`, `Floatn` | `Float64` | `real` widened exactly |
//! | `decimal(p,s)`, `numeric(p,s)` | `Decimaln`, `Numericn` | `Utf8` | exact text (`-12.50`); the scale is not in the metadata |
//! | `money`, `smallmoney` | `Money`, `Money4` | `Decimal128(19, 4)` | exact |
//! | `date` | `Daten` | `Date32` | |
//! | `time(n)` | `Timen` | `Time64(µs)` | 7th fractional digit (100 ns) truncated |
//! | `datetime`, `smalldatetime` | `Datetime`, `Datetime4`, `Datetimen` | `Timestamp(µs, None)` | |
//! | `datetime2(n)` | `Datetime2` | `Timestamp(µs, None)` | 7th fractional digit truncated |
//! | `datetimeoffset(n)` | `DatetimeOffsetn` | `Utf8` | RFC 3339 text with the stored offset and 7 digits (`2026-01-15T10:00:00.1234567+03:30`); an instant type would drop the offset |
//! | `char`, `varchar`, `nchar`, `nvarchar` (incl. `max`), `text`, `ntext` | `BigChar`, `BigVarChar`, `NChar`, `NVarchar`, `Text`, `NText` | `Utf8` | non-Unicode text decoded from the column collation's code page |
//! | `json` (2025) | `NVarchar` | `Utf8` | sent as `nvarchar(max)` to clients that do not announce JSON support (tiberius does not) |
//! | `vector(n)` (2025) | `BigVarChar` | `Utf8` | JSON array text, same reason |
//! | `xml` | `Xml` | `Utf8` | |
//! | `binary`, `varbinary` (incl. `max`), `image`, `rowversion` / `timestamp` | `BigBinary`, `BigVarBin`, `Image` | `Binary` | |
//! | `uniqueidentifier` | `Guid` | `FixedSizeBinary(16)` + `ARROW:extension:name = arrow.uuid` | RFC 4122 byte order (as `uuid::Uuid` prints it) |
//! | `sql_variant` | `SSVariant` | `Utf8` | the inner value's text form |
//! | `hierarchyid`, `geometry`, `geography`, CLR types | `Udt` | `Binary` | the CLR serialization; `CAST(x AS nvarchar(max))` / `.ToString()` in SQL gives text |
//! | untyped `NULL` | `Null` | `Null` | |
//!
//! Every field is nullable and carries [`META_NATIVE_TYPE`]: the lower-case SQL Server family
//! name the `ColumnType` stands for (`int`, `decimal`, `nvarchar`, `datetime2`, `udt`, ...),
//! not the declared type (the catalog fetchers give that).
//!
//! # Parameters
//!
//! | `Value` | Declared as (`@params` of `sp_executesql`) |
//! |---|---|
//! | `Null` | `nvarchar(1)` NULL (no implicit conversion to `varbinary`, `image`, CLR types: the grid writer writes the keyword `NULL` instead of a parameter) |
//! | `Bool` | `bit` |
//! | `Int` | `bigint` |
//! | `UInt` | `bigint` when it fits, else `decimal(20, 0)` |
//! | `Float` | `float` (NaN and infinities: `InvalidInput`, SQL Server has none) |
//! | `Decimal` | `decimal(38, s)` with the text's scale (more than 38 digits: `InvalidInput`) |
//! | `Text`, `Json` | `nvarchar(max)` |
//! | `Bytes` | `varbinary(max)` |
//! | `Date` / `Time` / `Timestamp` / `TimestampTz` | `date` / `time(7)` / `datetime2(7)` / `datetimeoffset(7)` |
//! | `Uuid` | `uniqueidentifier` |

/// Field metadata key: the SQL Server type family of the column (see the module docs). Same key
/// as the MySQL driver's.
pub const META_NATIVE_TYPE: &str = "oxbase.native_type";
/// Arrow canonical extension-name metadata key.
pub const META_EXTENSION_NAME: &str = "ARROW:extension:name";
/// Extension name of `uniqueidentifier` columns (Arrow canonical `arrow.uuid`).
pub const UUID_EXTENSION: &str = "arrow.uuid";
/// Default rows per Arrow batch (option [`crate::options::BATCH_ROWS`]).
pub const BATCH_ROWS: usize = 4096;
