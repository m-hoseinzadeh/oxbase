//! SQLite type mapping (`docs/m8-contracts.md` §4.1).
//!
//! Required facts (transfer stream; public SQLite documentation, type affinity): integers →
//! `INTEGER` (unsigned 64-bit → `TEXT`, widened); floats → `REAL`; decimals → `NUMERIC` up
//! to 15 significant digits (narrowed beyond) or `DECIMAL_TEXT(p,s)` with
//! `DdlContext::exact_values` (widened: kept as text); text, char, varchar → `TEXT` (length not
//! enforced: widened); binary → `BLOB`; dates and times → `TEXT` ISO-8601 (widened);
//! `timestamp_tz` → `TEXT` with offset (widened); `uuid` → `TEXT` (widened); `json` → `TEXT`
//! (widened); arrays, enums, intervals → `TEXT` (widened); `other` → `TEXT` with the value's
//! text form (widened).

use oxbase_dialect::{TypeMapper, TypeMapping};
use oxbase_types::{DataType, ServerVersion};

/// The SQLite [`TypeMapper`].
#[derive(Debug, Clone, Copy, Default)]
pub struct SqliteTypeMapper;

impl TypeMapper for SqliteTypeMapper {
    fn map(&self, data_type: &DataType, version: &ServerVersion) -> TypeMapping {
        let _ = (data_type, version);
        unimplemented!("m8: transfer")
    }
}
