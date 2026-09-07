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
| The synthetic namespace's parse output (Octofriend SQLite) | the `parser_version` inside `CacheIdentity::synthetic()` | Synthetic is not a `ClientId`, so it is absent from `parser_version(client)` and carries its own hard-coded version instead. Same scoping, different place to edit |
| The envelope itself, or a type shared by every namespace's payload (`CachedSourceEntry`, `UnifiedMessage`, `TokenBreakdown`) | `CACHE_FORMAT_VERSION` | No per-client check can help — every payload would be decoded against the new struct |

Reach for `parser_version` first and justify the global bump, not the other way round. `CACHE_FORMAT_VERSION` is the wider blast radius and, for one namespace, it is not recoverable.

### Claude is not merely cold

For most clients an invalidation is cold: the shard is rejected, the source is re-parsed, the same messages come back.

Not for Claude. For a namespace covered by `retained_history_key_filter` — only Claude — the cache holds turns the live file **no longer contains**. Claude Code rewrites transcripts in place on compact, and retention carries the dropped turns forward. The comment on `CachedSourceEntry::messages` says it outright: re-parsing will not reproduce them, the cache is the only copy.

A `parser_version` bump no longer deletes them. A Claude entry behind the current version is stripped to the messages `retained_keys` names and kept, staying on its old version so a later load does the same instead of serving history as a whole source. `oldest_migratable_parser_version` is the floor: version 1 predates `retained_keys` and its payload has one field fewer than the current struct, so it stays stale. Raise that floor if a future bump makes an older payload uninterpretable the same way.

`CACHE_FORMAT_VERSION` still does delete them. Its gate runs before the namespace is looked at and before the payload is decoded, so nothing is left to migrate. The test `test_non_claude_legacy_shard_is_rejected_by_the_format_bump_before_its_payload_is_decoded` pins that ordering.

The practical consequence: a change to a shared payload type has no scoped option. The retained-history cost is forced, and the pull request should say so plainly rather than let it be discovered afterwards.

What to do with a Claude parser change that needs to reach existing users:

1. **Ask whether it does.** A fix that applies as each transcript next changes may be acceptable; active files converge on their own, and nothing is lost. This is the default and it needs no counter bump at all.
2. **Otherwise bump `parser_version`.** The migration carries the retained history across, so the cost is a cold rescan like any other namespace. Check that the version you are bumping from is at or above `oldest_migratable_parser_version`; below it, the bump is still a data-loss decision and the pull request should say so in those words.

Also note that `parser_version` values are local state. They diverged from upstream long ago, so during any re-vendor they are re-applied, never copied across. `UPSTREAM.md` carries the comparison table and the reason.

## The UPSTREAM.md ledger

This repository carries selective patches over upstream [`junhoyeo/tokscale`](https://github.com/junhoyeo/tokscale) rather than a wholesale fork. [`UPSTREAM.md`](UPSTREAM.md) records every local divergence.

If your change diverges from upstream — a fix upstream does not have, or a deliberate difference in behaviour — add a row to the `Local patches` table describing what changed, which files carry it, and its upstream status. Include the re-apply instruction naming the file whose re-vendor would otherwise drop it.

This is not bookkeeping for its own sake. A patch missing from the ledger is a patch that a future re-vendor deletes silently, with a clean build and passing tests.

## Tests

Most fixtures are built inline as raw strings against a `TempDir` — see the `#[cfg(test)]` modules in [`src/sessions/`](src/sessions) for the established shape. Build JSON with `serde_json::json!` rather than formatting it by hand, for the reason in [Verification](#verification).

A fixture that several test lanes must agree on belongs in [`tests/fixtures/`](tests/fixtures) and is pulled in with `include_str!`. `codex_duration_timing.jsonl` is the example: three consumers share it (`src/sessions/codex.rs`, `src/lib.rs`, `tests/remote_source_report.rs`), and inlining a copy per lane would let them drift apart silently.

For tests that need an isolated environment, [`tests/filter_parity.rs`](tests/filter_parity.rs) is the reference: an `EnvGuard` that restores every variable on drop, two temporary homes, `TOKSCALE_PRICING_CACHE_ONLY=1` so a network pricing fetch cannot make the run non-deterministic, and `#[serial_test::serial]` because process environment is shared.

A parser-output change that **does** invalidate wants a **same-fingerprint stale-cache regression** — a test proving an entry already in the cache picks up the new behaviour. Cold-parse tests cannot show that, and it is the case real users are in.

A change that deliberately does not invalidate — the default for Claude, step 1 above — cannot pass that test and must not be pushed into a lossy bump to satisfy it. A same-fingerprint lookup returns the cached messages without running the parser at all, which is the whole point of deferring. Pin the decision instead: assert that a cached entry keeps its old value at an unchanged fingerprint, and that the new behaviour appears once the fingerprint changes.

Prefer an assertion on observed behaviour over a scan of source text. A property that can be moved to another line or another function will be moved around a text scan, and the scan will keep passing.

## Pull requests

One reviewable concern per branch. Write the commit message as a durable record: what the change does, the data flow when it matters, and the reasoning a diff alone would not show.

State how you verified it, and be specific about what the verification does *not* cover. A gap named honestly is useful; a gap papered over costs the next person a debugging session.

Merges use merge commits, preserving each commit on the branch.

## Public repository safety

Never commit credentials, private paths, machine-specific tooling details, or unpublished security work.
