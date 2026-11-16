# DBStudio patch of tiberius 0.13.0

Source: `tiberius-0.13.0` from the crates.io registry (MIT/Apache-2.0), copied here and wired in
with `[patch.crates-io]` in the workspace `Cargo.toml`. Tests, examples and docs of the upstream
package were left out and their targets removed from `Cargo.toml`. The patch is small and every
changed spot is marked `DBStudio patch`:

1. **Statement events** (`src/tds/stream/query.rs`, `src/client.rs`): `QueryItem::Info(ServerMessage)`
   (`PRINT`, `RAISERROR` below severity 11) and `QueryItem::Done(DoneInfo)` (row count per
   statement, `DONE` / `DONEPROC` / `DONEINPROC`). `Client::simple_query` no longer skips the tokens
   before the first result set. Upstream drops both token kinds.
2. **Cooperative cancel** (`src/tds/context.rs`, `src/tds/stream/token.rs`,
   `src/client/connection.rs`, `src/client.rs`): `Client::cancel_handle() -> CancelHandle`, usable
   from any task while a result stream borrows the client. The running stream sends a TDS attention
   packet, keeps reading until the acknowledging `DONE` (attention bit) and ends normally, so the
   connection stays usable. Upstream's `cancel_query(&mut self)` cannot run while the stream is
   alive and an abandoned mid-response stream cannot be recovered.
3. **Exact `money` / `smallmoney`** (`src/tds/codec/column_data/money.rs`): decoded as a scale-4
   `ColumnData::Numeric` instead of `f64`.
4. **NTLM on Unix** (`Cargo.toml`): the `winauth` feature also enables `sspi-rs`, with `sspi`
   bumped to 0.23 (0.18 pins `crypto-bigint =0.7.0-rc.18`, which conflicts with `russh`).
5. **PLP byte limit** (`src/tds/context.rs`, `src/tds/codec/column_data/{plp,string,binary}.rs`,
   `src/tds/codec/token/token_row.rs`, `src/tds/stream/query.rs`, `src/client.rs`,
   `src/result.rs`): `Client::set_plp_limit(Option<usize>)` caps the bytes kept of every PLP value
   (`varchar(max)`, `nvarchar(max)`, `varbinary(max)`, `xml`). The excess is read off the wire in
   64 KiB windows and dropped, so memory stays bounded; the connection stays in sync. A cut value
   is reported by `QueryItem::Truncated(Truncated { column, total_bytes })`, emitted just before
   its `Row`. UTF-16 cuts drop a dangling high surrogate, code-page cuts decode lossily. UDT
   values and the connection default (`None`) are unaffected. Limitations: `QueryStream` queues
   the `Truncated` notes and their row in a `pending` list; `columns()` (peek-ahead) stops while
   that list is non-empty instead of reading past the queued row, and the limit also applies to
   output parameters and other PLP values read while it is set (callers reset it to `None`).
6. **Runtime TLS backend** (`src/client/tls_stream.rs`, `src/client/tls_stream/native_tls_stream.rs`,
   `src/client/config.rs`, `src/lib.rs`): the `rustls` and `native-tls` features can be enabled
   together. `TlsStream` is then an enum over both and `Config::tls_backend(TlsBackend::Native)`
   picks the OS library (SChannel, Security.framework, OpenSSL); the default is
   `TlsBackend::Rustls`. `TlsBackend` and the setter exist only with both features; single
   backend builds are unchanged. The native backend now sets
   `min_protocol_version(Some(Protocol::Tlsv10))`: SQL Server 2008 / 2008 R2 and servers on
   Windows Server 2012 R2 or older only offer TLS 1.0/1.1 or CBC / static-RSA / DHE suites,
   which rustls (TLS 1.2/1.3, AEAD + ECDHE only) cannot negotiate. Trust (`trust_cert`,
   `trust_cert_ca`, `hostname_in_certificate`) is honoured by both backends as before.
7. **One prelogin packet per handshake flight** (`src/client/tls.rs`, `src/client/connection.rs`):
   `TlsPreloginWrapper` no longer turns every `flush` into a TDS packet during the TLS handshake;
   it sends the buffered bytes when the TLS library next reads (the flight is complete) and once
   more right after the handshake (`flush_pending`). Security.framework (the `native-tls` backend
   on macOS) flushes after each record, so ClientKeyExchange, ChangeCipherSpec and Finished went
   out as three packets and SQL Server 2022 closed the connection (`connection closed via error`).
   rustls flushes once per flight, so its bytes on the wire do not change.

Updating: copy the new upstream `src`, re-apply the marked spots (`grep -rn "DBStudio patch"`),
run `cargo test -p dbstudio-driver-mssql` with `DBSTUDIO_IT=1` against `mssql2022`. Candidates for
an upstream pull request: 1, 2, 5, 6 and 7.
