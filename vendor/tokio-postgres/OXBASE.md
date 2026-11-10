# Vendored tokio-postgres 0.7.18

Source: the `tokio-postgres` 0.7.18 crate from crates.io (<https://github.com/rust-postgres/rust-postgres>),
dual-licensed MIT (`LICENSE-MIT`) or Apache-2.0 (`LICENSE-APACHE`). The workspace uses it through
`[patch.crates-io]` in the root `Cargo.toml`, with the same features as before.

## What differs from upstream

- `src/config.rs`: `Config::password_disclosure(bool)` / `get_password_disclosure()` (default
  `true`, upstream behaviour) and the field behind them.
- `src/connect_raw.rs` `authenticate`: when it is `false`, an `AuthenticationCleartextPassword` or
  `AuthenticationMD5Password` request fails with an authentication error ("... which would reveal
  the password and is disabled for this connection") before anything is written. SCRAM (with or
  without channel binding) and `AuthenticationOk` are unchanged.
- `Cargo.toml`: upstream's tests, benches and dev-dependencies are not vendored, so their sections
  are removed. Everything else is the upstream manifest.

`oxbase-pg` turns the switch off unless the server certificate is verified (`VerifyCa` /
`VerifyFull`) or the server is local (a Unix socket or a loopback host): an unverified peer could
otherwise pick an exchange that hands it the reusable password (audit lead 5).

## Updating

Copy the new upstream release over `src/`, reapply the changes above (search for
`oxbase patch`), and check `oxbase-pg`'s `password_disclosure` tests. Drop the vendored copy once
upstream can refuse these methods itself (libpq's `require_auth`).
