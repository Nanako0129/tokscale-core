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

A shard on disk is a `CachedShardEnvelope`: `format_version`, `parser_namespace`, `parser_version`, and `payload` — the entries as an opaque byte vector. `read_shard_with_limit` decodes the envelope, then checks those three fields in order, and only decodes the payload if all three pass.

That ordering is a deliberate design, stated at the envelope's definition: the envelope is independent of the payload's binary layout **so that a parser version can be checked before the payload is deserialized**. One client's serialized layout change therefore cannot make another client's shards unreadable.

So the question is not "which counter", it is **whose payload can no longer be decoded**:

| What changed | Counter | Why |
|---|---|---|
| One client's parse semantics or its own serialized state (e.g. `CodexParseState` under `codex_incremental`) | `parser_version(client)` | The namespace check rejects that client's shards before its payload is decoded; every other namespace keeps its cache |
| The envelope itself, or a type shared by every namespace's payload (`CachedSourceEntry`, `UnifiedMessage`, `TokenBreakdown`) | `CACHE_FORMAT_VERSION` | No per-client check can help — every payload would be decoded against the new struct |

Reach for `parser_version` first and justify the global bump, not the other way round. `CACHE_FORMAT_VERSION` is the wider blast radius and, for one namespace, it is not recoverable.

### Claude is not merely cold

For most clients an invalidation is cold: the shard is rejected, the source is re-parsed, the same messages come back.

Not for Claude. For a namespace covered by `retained_history_key_filter` — only Claude — the cache holds turns the live file **no longer contains**. Claude Code rewrites transcripts in place on compact, and retention carries the dropped turns forward. The comment on `CachedSourceEntry::messages` says it outright: re-parsing will not reproduce them, the cache is the only copy.

So for Claude, "invalidate" means "delete", and that applies to **both** counters. The format-version gate runs before the namespace is even looked at, so a global bump rejects Claude's shards along with everyone else's — it is not a way around a Claude `parser_version` bump, it is the same loss with a larger radius. The test `test_non_claude_legacy_shard_is_rejected_by_the_format_bump_before_its_payload_is_decoded` pins that ordering.

The practical consequence: a change to a shared payload type has no scoped option. The retained-history cost is forced, and the pull request should say so plainly rather than let it be discovered afterwards.

What to do with a Claude parser change that needs to reach existing users:

1. **Ask whether it does.** A fix that applies as each transcript next changes may be acceptable; active files converge on their own, and nothing is lost. This is the default and it needs no counter bump at all.
2. **If it must reach stale entries, that is an explicit data-loss decision.** Say so in the pull request, in those words, and let the maintainer weigh the correction against the retained history it costs.
3. **Or write a migration** that decodes the old shard, preserves the entries named by `retained_keys`, and rewrites them in the new format. That is real code, not a version bump, and it is the only option that gets both.

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
