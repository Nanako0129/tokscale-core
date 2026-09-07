# Contributing to tokscale-core

This is the shared Rust engine behind [TokenBar](https://github.com/Nanako0129/TokenBar): session parsing, scanning, caching, pricing, and aggregation. It is consumed as a Git submodule, so a change here reaches users through a consumer that pins an exact commit.

Focused fixes and well-supported bug reports are welcome. This guide covers what the code owns, how to verify a change, and the two traps that a clean build will not catch.

## Contents

- [What this repository owns](#what-this-repository-owns)
- [Development environment](#development-environment)
- [Verification](#verification)
- [Cache invalidation](#cache-invalidation)
- [The UPSTREAM.md ledger](#the-upstreammd-ledger)
- [Tests](#tests)
- [Pull requests](#pull-requests)
- [Public repository safety](#public-repository-safety)

---

## What this repository owns

Parser, scanner, cache, pricing, and aggregation code.

App FFI and consumer integration stay with the consuming repositories. If a change needs a new field visible in TokenBar's UI, the engine half lands here and the FFI and Swift halves land there; they are separate pull requests against separate repositories. Semantic changes should not be mixed into ownership or extraction work.

A practical consequence: an issue filed on TokenBar about wrong numbers is usually fixed *here*, because that is where the parsing and pricing live. The consumer repository owns only the pin.

## Development environment

`Cargo.lock` is committed and [`rust-toolchain.toml`](rust-toolchain.toml) pins **Rust 1.96.1**. If you use rustup, the version is selected automatically inside this directory — check with `rustc --version` before assuming it. A different toolchain produces different `rustfmt` and Clippy verdicts, and CI will disagree with you.

## Verification

Every change must pass all four commands, on macOS and on Windows:

```bash
cargo fetch --locked
cargo build --release --locked --offline
cargo test --locked --offline
cargo clippy --locked --offline --all-targets --all-features -- -D warnings
```

CI runs exactly these across `macos-latest`, `ubuntu-latest`, and `windows-latest`, with the toolchain pinned to 1.96.1.

Do not skip the Windows leg on the grounds that the change looks platform-neutral. A recent fixture built `variant.json` by interpolating a path into a hand-written JSON string; on macOS and Linux that is valid JSON, and on Windows every backslash in `C:\Users\...` became an invalid escape, so the file failed to parse and the test went silently inert. Only the Windows runner could see it.

## Cache invalidation

Parsed messages are cached on disk. **A change to parser output that does not invalidate the cache reaches nobody**: existing users keep the old numbers until each source file happens to change on its own. A test that parses a fresh source proves nothing about this.

There are two version counters in [`src/message_cache.rs`](src/message_cache.rs) and they are not interchangeable.

| Counter | Use it when | Effect |
|---|---|---|
| `parser_version(client)` | One client's parse semantics changed | That client's shards re-parse; other clients keep their cache |
| `CACHE_FORMAT_VERSION` | The serialized layout or a cross-client type changed | Every namespace's shards go cold once |

**The trap:** `parser_version(ClientId::Claude)` must not be bumped. Claude retention has shipped, Claude Code rewrites transcripts in place on compact, and retained assistant turns can no longer appear in the compacted file. A bump there does not merely make the cache cold — it silently retires those turns, and re-parsing cannot bring them back. The warning is in the source at the counter's definition; read it before touching that function. For a Claude parser change that needs invalidation, bump `CACHE_FORMAT_VERSION` instead: it costs one cold scan and loses nothing.

Also note that `parser_version` values are local state. They diverged from upstream long ago, so during any re-vendor they are re-applied, never copied across. `UPSTREAM.md` carries the comparison table and the reason.

## The UPSTREAM.md ledger

This repository carries selective patches over upstream [`junhoyeo/tokscale`](https://github.com/junhoyeo/tokscale) rather than a wholesale fork. [`UPSTREAM.md`](UPSTREAM.md) records every local divergence.

If your change diverges from upstream — a fix upstream does not have, or a deliberate difference in behaviour — add a row to the `Local patches` table describing what changed, which files carry it, and its upstream status. Include the re-apply instruction naming the file whose re-vendor would otherwise drop it.

This is not bookkeeping for its own sake. A patch missing from the ledger is a patch that a future re-vendor deletes silently, with a clean build and passing tests.

## Tests

Fixtures are built inline as raw strings against a `TempDir`, not stored as files — see the `#[cfg(test)]` modules in [`src/sessions/`](src/sessions) for the established shape. Build JSON with `serde_json::json!` rather than formatting it by hand, for the reason in [Verification](#verification).

For tests that need an isolated environment, [`tests/filter_parity.rs`](tests/filter_parity.rs) is the reference: an `EnvGuard` that restores every variable on drop, two temporary homes, `TOKSCALE_PRICING_CACHE_ONLY=1` so a network pricing fetch cannot make the run non-deterministic, and `#[serial_test::serial]` because process environment is shared.

A parser-output change also wants a **same-fingerprint stale-cache regression** — a test proving an entry already in the cache picks up the new behaviour. Cold-parse tests cannot show that, and it is the case real users are in.

Prefer an assertion on observed behaviour over a scan of source text. A property that can be moved to another line or another function will be moved around a text scan, and the scan will keep passing.

## Pull requests

One reviewable concern per branch. Write the commit message as a durable record: what the change does, the data flow when it matters, and the reasoning a diff alone would not show.

State how you verified it, and be specific about what the verification does *not* cover. A gap named honestly is useful; a gap papered over costs the next person a debugging session.

Merges use merge commits, preserving each commit on the branch.

## Public repository safety

Never commit credentials, private paths, machine-specific tooling details, or unpublished security work.
