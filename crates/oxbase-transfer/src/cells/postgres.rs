//! PostgreSQL type mapping (`docs/m8-contracts.md` §4.1).
//!
//! Required facts (transfer stream; public PostgreSQL documentation): every logical type has an
//! exact home (`boolean`, `smallint` / `integer` / `bigint`, unsigned 64-bit → `numeric(20,0)`
//! (widened), `numeric(p,s)` / bare `numeric`, `real` / `double precision`, `char(n)`,
//! `varchar(n)`, `text`, `bytea`, `date`, `time(p)`, `timestamp(p)`, `timestamptz(p)`,
//! `interval`, `uuid`, `json` / `jsonb`, arrays `<element>[]`); enums → `text` with a `CHECK`
//! is **not** generated: `text` (widened; a `CREATE TYPE` in a foreign schema is out of scope);
//! `other` → `unsupported` unless the name is a built-in PostgreSQL type. Source side
//! (`logical`): domains map to their base type, arrays of arrays stay `array`.

use oxbase_dialect::{TypeMapper, TypeMapping};
use oxbase_types::{DataType, ServerVersion};

/// The PostgreSQL [`TypeMapper`].
#[derive(Debug, Clone, Copy, Default)]
pub struct PgTypeMapper;

impl TypeMapper for PgTypeMapper {
    fn map(&self, data_type: &DataType, version: &ServerVersion) -> TypeMapping {
        let _ = (data_type, version);
        unimplemented!("m8: transfer")
    }
}
