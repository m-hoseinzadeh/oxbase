//! The MySQL / MariaDB [`ChunkHasher`] (M8 data sync, `docs/m8-contracts.md` §3.4): one
//! statement that returns the row count and a digest of a key range, so ranges that are equal
//! on both sides are skipped without reading their rows.
//!
//! ```sql
//! SELECT COUNT(*),
//!        CONCAT(COALESCE(SUM(CRC32(r)), 0), ':',
//!               COALESCE(BIT_XOR(CAST(CONV(SUBSTRING(MD5(r), 1, 16), 16, 10) AS UNSIGNED)), 0))
//!   FROM (SELECT CONCAT_WS(',', IFNULL(MD5(`c1`), 'n'), IFNULL(MD5(`c2`), 'n')) AS r
//!           FROM `db`.`t` WHERE <range>) AS x
//! ```
//!
//! Each value is hashed on its own (`MD5` of its bytes: binary columns never meet a text
//! character set) and NULL is `n`, which no MD5 text equals, so NULL and the empty string
//! differ. The per-row hashes are combined
//! with a sum and an XOR (no `GROUP_CONCAT`, whose length is capped by
//! `group_concat_max_len`); both sides of a sync run on the same engine, so the digest only
//! has to be stable there.

use oxbase_dialect::sql::SqlWriter;
use oxbase_dialect::{BacktickQuoting, ChunkHasher, KeyRange};
use oxbase_types::{BoundStatement, ColumnDef, Error, ObjectRef, Result};

/// See the module docs.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct MySqlChunkHasher;

impl ChunkHasher for MySqlChunkHasher {
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
        let mut w = SqlWriter::new(&BacktickQuoting);
        w.push(
            "SELECT COUNT(*), CONCAT(COALESCE(SUM(CRC32(`oxbase_row`)), 0), ':', \
             COALESCE(BIT_XOR(CAST(CONV(SUBSTRING(MD5(`oxbase_row`), 1, 16), 16, 10) AS \
             UNSIGNED)), 0)) FROM (SELECT CONCAT_WS(',', ",
        );
        for (pos, column) in columns.iter().enumerate() {
            if pos > 0 {
                w.push(", ");
            }
            w.push("IFNULL(MD5(").ident(&column.name).push("), 'n')");
        }
        w.push(") AS `oxbase_row` FROM ")
            .qualified(table)
            .push(" WHERE ");
        range.write(&mut w, key)?;
        w.push(") AS `oxbase_chunk`");
        Ok(w.finish())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_types::{DataType, Value};

    #[test]
    fn digest_statement_binds_the_range() {
        let table = ObjectRef::table(Some("db"), None, "t");
        let columns = [
            ColumnDef::new("id", DataType::INT64),
            ColumnDef::new("na`me", DataType::Text),
        ];
        let lo = [Value::Int(10)];
        let st = MySqlChunkHasher
            .chunk_hash(
                &table,
                &["id".into()],
                &columns,
                KeyRange {
                    after: Some(&lo),
                    upto: None,
                },
            )
            .unwrap();
        assert_eq!(
            st.sql,
            "SELECT COUNT(*), CONCAT(COALESCE(SUM(CRC32(`oxbase_row`)), 0), ':', \
             COALESCE(BIT_XOR(CAST(CONV(SUBSTRING(MD5(`oxbase_row`), 1, 16), 16, 10) AS \
             UNSIGNED)), 0)) FROM (SELECT CONCAT_WS(',', IFNULL(MD5(`id`), 'n'), \
             IFNULL(MD5(`na``me`), 'n')) AS `oxbase_row` FROM `db`.`t` WHERE (`id` > ?)) AS \
             `oxbase_chunk`"
        );
        assert_eq!(st.params, [Value::Int(10)]);
        assert!(
            MySqlChunkHasher
                .chunk_hash(&table, &["id".into()], &[], KeyRange::default())
                .is_err()
        );
    }
}
