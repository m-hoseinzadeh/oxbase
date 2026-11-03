//! SQL Server type mapping (`docs/m8-contracts.md` §4.1).
//!
//! Required facts (transfer stream; public SQL Server documentation): `bool` → `bit`; `int8`
//! → `smallint` (SQL Server `tinyint` is unsigned); unsigned 32-bit → `bigint` (widened),
//! unsigned 64-bit → `decimal(20,0)` (widened); `decimal` up to 38 digits (unconstrained →
//! `decimal(38,10)`, narrowed); text → `nvarchar(n)` / `nvarchar(max)` (Unicode); binary →
//! `varbinary(n)` / `varbinary(max)`; `time(p)` / `datetime2(p)` with p ≤ 7; `timestamp_tz` →
//! `datetimeoffset(p)` (exact); `interval` → `nvarchar(64)` (widened); `uuid` →
//! `uniqueidentifier`; `json` → `nvarchar(max)` with `ISJSON` not enforced (widened; the 2025
//! `json` type is used only when the server version has it: exact); arrays → `nvarchar(max)`
//! JSON text (widened); enums → `nvarchar(n)` (widened).

use oxbase_dialect::{TypeMapper, TypeMapping};
use oxbase_types::{DataType, ServerVersion};

/// The SQL Server [`TypeMapper`].
#[derive(Debug, Clone, Copy, Default)]
pub struct MsSqlTypeMapper;

impl TypeMapper for MsSqlTypeMapper {
    fn map(&self, data_type: &DataType, version: &ServerVersion) -> TypeMapping {
        let _ = (data_type, version);
        unimplemented!("m8: transfer")
    }
}
