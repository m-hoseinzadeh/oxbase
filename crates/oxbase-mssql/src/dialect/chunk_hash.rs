//! The SQL Server [`ChunkHasher`] (M8 data sync, `docs/m8-contracts.md` §3.4): one statement
//! that returns the row count and a digest of a key range, so ranges that are equal on both
//! sides are skipped without reading their rows.
//!
//! ```sql
//! SELECT COUNT_BIG(*),
//!        CONCAT(SUM(CAST(CAST(SUBSTRING(h.[oxbase_h], 1, 4) AS int) AS bigint)), N':',
//!               CHECKSUM_AGG(CAST(SUBSTRING(h.[oxbase_h], 5, 4) AS int)))
//!   FROM [s].[t] AS [oxbase_t]
//!  CROSS APPLY (SELECT HASHBYTES('SHA2_256',
//!        (SELECT [oxbase_t].[c1] AS [c1], ... FOR JSON PATH, WITHOUT_ARRAY_WRAPPER,
//!         INCLUDE_NULL_VALUES)) AS [oxbase_h]) AS h
//!  WHERE <range>
//! ```
//!
//! Each row is hashed as its JSON object (`null` for NULL, strings quoted, binary as base64)
//! with SHA-256; the per-row hashes are combined with a sum and `CHECKSUM_AGG`. Both sides of a
//! sync run on the same engine, so the digest only has to be stable there. A column type that
//! `FOR JSON` refuses (CLR types) makes the statement fail; the compare then reads every row.

use oxbase_dialect::sql::SqlWriter;
use oxbase_dialect::{BracketQuoting, ChunkHasher, KeyRange};
use oxbase_types::{BoundStatement, ColumnDef, Error, ObjectRef, Result};

/// See the module docs.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct MsSqlChunkHasher;

impl ChunkHasher for MsSqlChunkHasher {
    fn chunk_hash(
        &self,
        table: &ObjectRef,
        key: &[String],
        columns: &[ColumnDef],
        range: KeyRange<'_>,
    ) -> Result<BoundStatement> {
        if columns.is_empty() || key.is_empty() {
            return Err(Error::invalid_input(
                "a chunk digest needs a key and columns",
            ));
        }
        let mut w = SqlWriter::new(&BracketQuoting);
        w.push(
            "SELECT COUNT_BIG(*), CONCAT(SUM(CAST(CAST(SUBSTRING(h.[oxbase_h], 1, 4) AS int) AS \
             bigint)), N':', CHECKSUM_AGG(CAST(SUBSTRING(h.[oxbase_h], 5, 4) AS int))) FROM ",
        )
        .qualified(table)
        .push(" AS [oxbase_t] CROSS APPLY (SELECT HASHBYTES('SHA2_256', (SELECT ");
        for (pos, column) in columns.iter().enumerate() {
            if pos > 0 {
                w.push(", ");
            }
            w.push("[oxbase_t].")
                .ident(&column.name)
                .push(" AS ")
                .ident(&column.name);
        }
        w.push(
            " FOR JSON PATH, WITHOUT_ARRAY_WRAPPER, INCLUDE_NULL_VALUES)) AS [oxbase_h]) AS h \
             WHERE ",
        );
        range.write(&mut w, key)?;
        Ok(w.finish())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_types::{DataType, Value};
    use pretty_assertions::assert_eq;

    #[test]
    fn digest_statement_binds_the_range() {
        let table = ObjectRef::table(Some("db"), Some("s"), "t");
        let columns = [
            ColumnDef::new("id", DataType::INT64),
            ColumnDef::new("na]me", DataType::Text),
        ];
        let hi = [Value::Int(20)];
        let st = MsSqlChunkHasher
            .chunk_hash(
                &table,
                &["id".into()],
                &columns,
                KeyRange {
                    after: None,
                    upto: Some(&hi),
                },
            )
            .unwrap();
        assert_eq!(
            st.sql,
            "SELECT COUNT_BIG(*), CONCAT(SUM(CAST(CAST(SUBSTRING(h.[oxbase_h], 1, 4) AS int) AS \
             bigint)), N':', CHECKSUM_AGG(CAST(SUBSTRING(h.[oxbase_h], 5, 4) AS int))) FROM \
             [s].[t] AS [oxbase_t] CROSS APPLY (SELECT HASHBYTES('SHA2_256', (SELECT \
             [oxbase_t].[id] AS [id], [oxbase_t].[na]]me] AS [na]]me] FOR JSON PATH, \
             WITHOUT_ARRAY_WRAPPER, INCLUDE_NULL_VALUES)) AS [oxbase_h]) AS h WHERE ([id] < @P1 \
             OR ([id] = @P2))"
        );
        assert_eq!(st.params, [Value::Int(20), Value::Int(20)]);
    }
}
