//! The PostgreSQL [`ChunkHasher`] (M8 data sync, `docs/m8-contracts.md` §3.4): one statement
//! that returns the row count and an MD5 digest of a key range, so ranges that are equal on
//! both sides are skipped without reading their rows.
//!
//! ```sql
//! SELECT count(*)::bigint,
//!        md5(string_agg(md5(ROW("c1", float8send("f"))::text), '' ORDER BY "k"))
//!   FROM "s"."t" WHERE <range>
//! ```
//!
//! A row's text form (`ROW(...)::text`) writes NULL as nothing and the empty string as `""`,
//! so the two stay apart; rows go in key order. Each row is hashed on its own and only the
//! 32-character row digests are aggregated, so a chunk of large rows never builds one huge
//! string. Floating-point columns are hashed by their binary form (`float4send` /
//! `float8send`): their text depends on `extra_float_digits`, and with fewer digits two
//! different values would print (and hash) the same. The digest is NULL for an empty range.
use oxbase_dialect::sql::SqlWriter;
use oxbase_dialect::{ChunkHasher, KeyRange, PgQuoting, write_column_list};
use oxbase_types::{BoundStatement, ColumnDef, DataType, Error, ObjectRef, Result};

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
        let mut w = SqlWriter::new(&PgQuoting);
        w.push("SELECT count(*)::bigint, md5(string_agg(md5(ROW(");
        for (pos, column) in columns.iter().enumerate() {
            if pos > 0 {
                w.push(", ");
            }
            match column.data_type {
                DataType::Float { bits: 32 } => {
                    w.push("float4send(").ident(&column.name).push(")");
                }
                DataType::Float { .. } => {
                    w.push("float8send(").ident(&column.name).push(")");
                }
                _ => {
                    w.ident(&column.name);
                }
            }
        }
        w.push(")::text), '' ORDER BY ");
        write_column_list(&mut w, key);
        w.push(")) FROM ").qualified(table).push(" WHERE ");
        range.write(&mut w, key)?;
        Ok(w.finish())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxbase_types::Value;

    #[test]
    fn digest_statement_binds_the_range() {
        let table = ObjectRef::table(Some("db"), Some("s"), "t");
        let columns = [
            ColumnDef::new("id", DataType::INT64),
            ColumnDef::new("na\"me", DataType::Text),
            ColumnDef::new("r", DataType::Float { bits: 32 }),
            ColumnDef::new("d", DataType::Float { bits: 64 }),
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
            "SELECT count(*)::bigint, md5(string_agg(md5(ROW(\"id\", \"na\"\"me\", \
             float4send(\"r\"), float8send(\"d\"))::text), '' ORDER BY \"id\")) FROM \"s\".\"t\" \
             WHERE (\"id\" > $1) AND (\"id\" < $2 OR (\"id\" = $3))"
        );
        assert_eq!(st.params, [Value::Int(10), Value::Int(20), Value::Int(20)]);
        assert!(
            PgChunkHasher
                .chunk_hash(&table, &[], &columns, KeyRange::default())
                .is_err()
        );
    }
}
