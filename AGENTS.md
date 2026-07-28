# Maintainer routing

Start with [`README.md`](README.md) for scope and commands, then consult
[`UPSTREAM.md`](UPSTREAM.md) before changing shared code or upstream alignment.

> `Cargo.lock` is committed. Every change must use Rust 1.96.1 and pass the
> locked build, test, and strict Clippy commands on both macOS and Windows.

```bash
cargo fetch --locked
cargo build --release --locked --offline
cargo test --locked --offline
cargo clippy --locked --offline --all-targets --all-features -- -D warnings
```

Keep selective upstream and local patches documented in `UPSTREAM.md`; do not
wholesale re-vendor the dependency. This repository owns shared parser,
scanner, cache, pricing, and aggregation code only. App FFI and consumer
integration stay with the consuming repositories, and semantic changes must
not be mixed into ownership or extraction work.

This is a public repository. Never commit credentials, private paths,
machine-specific tooling details, or unpublished security work.
