//! MySQL and MariaDB type mapping (`docs/m8-contracts.md` §4.1). One mapper serves both engines; the
//! `mariadb` flag selects MariaDB's differences (`uuid` type on 10.7+, `inet6`, JSON stored
//! as `longtext` with a `json_valid` check).
//!
//! Required facts (transfer stream; public MySQL / MariaDB documentation): `bool` →
//! `tinyint(1)`; unsigned integers kept (`bigint unsigned`); `decimal` up to 65 digits and 30
//! scale (unconstrained → `decimal(65,30)`, narrowed); `varchar(n)` up to 16 383 characters
//! in `utf8mb4` else `longtext`; `timestamp_tz` → `datetime(6)` holding the UTC instant
//! (narrowed: the offset is dropped); `time` → `time(6)`; `interval` → `varchar(64)` text
//! (widened); `uuid` → `char(36)` (MariaDB 10.7+: `uuid`, exact); `json` / `jsonb` → `json`
//! (exact as a JSON value; key order and spacing normalized); arrays → `json` (widened); enums
//! → `enum(...)` (exact); binary → `varbinary(n)` / `longblob`; `other` → `unsupported`
//! unless the name is a MySQL type.

use oxbase_dialect::{TypeMapper, TypeMapping};
use oxbase_types::{DataType, ServerVersion};

/// The MySQL / MariaDB [`TypeMapper`].
#[derive(Debug, Clone, Copy, Default)]
pub struct MySqlTypeMapper {
    pub mariadb: bool,
}

impl TypeMapper for MySqlTypeMapper {
    fn map(&self, data_type: &DataType, version: &ServerVersion) -> TypeMapping {
        let _ = (self.mariadb, data_type, version);
        unimplemented!("m8: transfer")
    }
}
