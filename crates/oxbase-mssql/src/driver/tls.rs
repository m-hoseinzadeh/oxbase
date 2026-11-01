//! oxbase's own TLS for TDS (driver stream): PRELOGIN exchange, the TLS handshake wrapped in
//! TDS PRELOGIN packets (TDS 7.4) or directly on TCP (TDS 8.0 strict, ALPN `tds/8.0`), and the
//! stream adapter that replays the server's PRELOGIN response to tiberius. Rules: the
//! [`crate::driver`] module docs (*Transport*) and `docs/m4-contracts.md` §2.2. Written from the
//! public MS-TDS specification (sections 2.2.6.5 PRELOGIN and 3.2.5.2 / 3.3.5.2 TLS
//! negotiation); no code from other clients.
//!
//! Unit tests (no server): PRELOGIN encoding / decoding, the packet wrapping of handshake
//! records, the replay adapter over an in-memory duplex stream, and every `TlsMode` /
//! server-answer combination of the table in the module docs.
