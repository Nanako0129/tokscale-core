# tokscale-core

`tokscale-core` is the shared Rust core extracted from TokenBar. It owns the
shared session parsers, source scanner, message cache, pricing, and aggregation
used by consumers. App-specific FFI, C ABI, UI, and product wiring remain in
the consuming repositories; this extraction does not claim that consumer
migration is complete.

## Build and test

Rust 1.96.1 is pinned in [`rust-toolchain.toml`](rust-toolchain.toml). Run the
same locked commands locally and in CI on macOS and Windows:

```bash
cargo fetch --locked
cargo build --release --locked --offline
cargo test --locked --offline
cargo clippy --locked --offline --all-targets --all-features -- -D warnings
```

## Upstream and license

The upstream implementation is [junhoyeo/tokscale](https://github.com/junhoyeo/tokscale).
The complete extraction baseline, 111-row audit, and local patch ledger are
authoritatively recorded in [`UPSTREAM.md`](UPSTREAM.md). This project retains
the upstream MIT license and attribution; see [`LICENSE`](LICENSE).
