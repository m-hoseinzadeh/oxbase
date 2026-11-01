# Vendored tiberius 0.13.0

Source: the `tiberius` 0.13.0 crate from crates.io (<https://github.com/prisma/tiberius>),
dual-licensed MIT (`LICENSE-MIT.txt`) or Apache-2.0 (`LICENSE-APACHE.txt`). The workspace uses it
through `[patch.crates-io]` in the root `Cargo.toml`, with the same features as before
(`default-features = false`, `tds73`, `chrono`, so no TLS backend).

## What differs from upstream

- `src/tds/codec/column_data/money.rs` `decode`: `money` and `smallmoney` decode to
  `ColumnData::Numeric` with scale 4, built from the on-wire integer (8 bytes: high `i32` then
  low `u32`; 4 bytes: one `i32`). Upstream returns `ColumnData::F64`, which rounds every amount
  whose scaled value needs more than 53 bits (above about 9.007e11); the largest `money`,
  922337203685477.5807, came back off by up to 2048 units of the fourth decimal.
- `src/tds/codec/token/token_col_metadata.rs` `null_value`: a NULL money column is
  `ColumnData::Numeric(None)` to match.
- The unit tests next to both changes are updated to the new values.
- `Cargo.toml`: upstream's tests, examples and dev-dependencies are not vendored, so their
  sections are removed. Everything else is the upstream manifest.

Encoding (statement parameters and bulk load) is unchanged: `Numeric` and `F64` values still
encode into money columns.

## Updating

Copy the new upstream release over `src/`, reapply the changes above (search for
`oxbase patch`), and check `oxbase-mssql`'s `driver_live` money assertions, which compare the
largest and smallest `money` values exactly. Drop the vendored copy once upstream decodes money
exactly, or when oxbase's own TDS decoder (docs/m4-contracts.md §11) replaces tiberius.
