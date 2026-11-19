# Vendored patches

## `ironrdp-connector`

Copy of crates.io `ironrdp-connector` 0.10.0 with dependency pins adjusted so GaleTerm can link **IronRDP** and **russh** in one workspace:

- `picky = "=7.0.0-rc.26"` (was `rc.25`)
- `sspi = "0.22"` (was `0.21`)
- `TsRequest::buffer_len` now returns `Result` under sspi 0.22 — one call site in `credssp.rs` updated

Background: [Devolutions/IronRDP#1363](https://github.com/Devolutions/IronRDP/issues/1363).

Wired via workspace `[patch.crates-io]` in the root `Cargo.toml`. Drop the patch when upstream IronRDP publishes a line that resolves against stable `curve25519-dalek` / current `russh`.
