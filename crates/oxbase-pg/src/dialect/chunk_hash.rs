//! The PostgreSQL [`ChunkHasher`] (M8 data sync, `docs/m8-contracts.md` §3.4): one statement
//! that returns the row count and an MD5 digest of a key range, so ranges that are equal on
//! both sides are skipped without reading their rows.
//!
//! ```sql
//! SELECT count(*)::bigint,
//!        md5(string_agg(ROW("c1", "c2")::text, chr(10) ORDER BY "k"))
//!   FROM "s"."t" WHERE <range>
//! ```
//!
//! A row's text form (`ROW(...)::text`) writes NULL as nothing and the empty string as `""`,
//! so the two stay apart; rows go in key order. The digest is NULL for an empty range.

use oxbase_dialect::sql::SqlWriter;
use oxbase_dialect::{ChunkHasher, KeyRange, PgQuoting, write_column_list};
use oxbase_types::{BoundStatement, ColumnDef, Error, ObjectRef, Result};

/// See the module docs.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct PgChunkHasher;

impl ChunkHasher for PgChunkHasher {
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
        let names: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
        let mut w = SqlWriter::new(&PgQuoting);
        w.push("SELECT count(*)::bigint, md5(string_agg(ROW(");
        write_column_list(&mut w, &names);
        w.push(")::text, chr(10) ORDER BY ");
        write_column_list(&mut w, key);
        w.push(")) FROM ").qualified(table).push(" WHERE ");
        range.write(&mut w, key)?;
        Ok(w.finish())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_types::{DataType, Value};

    #[test]
    fn digest_statement_binds_the_range() {
        let table = ObjectRef::table(Some("db"), Some("s"), "t");
        let columns = [
            ColumnDef::new("id", DataType::INT64),
            ColumnDef::new("na\"me", DataType::Text),
        ];
        let lo = [Value::Int(10)];
        let hi = [Value::Int(20)];
        let st = PgChunkHasher
            .chunk_hash(
                &table,
                &["id".into()],
                &columns,
                KeyRange {
                    after: Some(&lo),
                    upto: Some(&hi),
                },
            )
            .unwrap();
        assert_eq!(
            st.sql,
            "SELECT count(*)::bigint, md5(string_agg(ROW(\"id\", \"na\"\"me\")::text, chr(10) \
             ORDER BY \"id\")) FROM \"s\".\"t\" WHERE (\"id\" > $1) AND (\"id\" < $2 OR (\"id\" \
             = $3))"
        );
        assert_eq!(st.params, [Value::Int(10), Value::Int(20), Value::Int(20)]);
        assert!(
            PgChunkHasher
                .chunk_hash(&table, &[], &columns, KeyRange::default())
                .is_err()
        );
    }
}
