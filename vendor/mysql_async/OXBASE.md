# Vendored mysql_async 0.37.1

Source: the `mysql_async` 0.37.1 crate from crates.io (<https://github.com/blackbeam/mysql_async>),
dual-licensed MIT (`LICENSE-MIT`) or Apache-2.0 (`LICENSE-APACHE`). The workspace uses it through
`[patch.crates-io]` in the root `Cargo.toml`, with the same features as before.

## What differs from upstream

- `src/opts/mod.rs`: `OptsBuilder::password_disclosure(bool)` / `Opts::password_disclosure()`
  (default `true`, upstream behaviour) and the `MysqlOpts` field behind them.
- `src/conn/mod.rs` `continue_caching_sha2_password_auth`: when it is `false`, a full
  authentication request (`01 04`) fails with `DriverError::PasswordDisclosureDisabled` before
  anything is written, so neither the cleartext password (TLS / socket) nor the RSA-encrypted one
  (to a key the server sends unauthenticated) leaves. Fast authentication (`01 03`) and every
  other plugin are unchanged; `mysql_clear_password` stays behind `enable_cleartext_plugin`.
- `src/error/mod.rs`: the `DriverError::PasswordDisclosureDisabled` variant.
- `Cargo.toml`: upstream's tests and dev-dependencies are not vendored, so their sections are
  removed. Everything else is the upstream manifest.

`oxbase-mysql` turns the switch off unless the server certificate is verified (`VerifyFull`) or
the server is a loopback host: an unverified peer could otherwise ask for full authentication and
learn the reusable password (audit lead 5).

## Updating

Copy the new upstream release over `src/`, reapply the changes above (search for
`oxbase patch`), and check `oxbase-mysql`'s `password_disclosure` tests. Drop the vendored copy once
upstream can refuse full authentication itself.
