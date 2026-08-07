// Benchmark probe for the batched-parallel-parse performance gate (not part
// of the library's public surface; lives only in this worktree for the
// PARSE-BATCH acceptance run). Not wired into any build/test/release path.
//
// Usage: bench_scan <home_dir>
// Env: HOME / TOKSCALE_CONFIG_DIR select the isolated cache directory (set by
// the caller, exactly like the existing `with_isolated_tokscale_cache` test
// helper). TOKSCALE_PRICING_CACHE_ONLY=1 avoids any network pricing fetch so
// runs are offline-deterministic.
//
// Prints one line: `elapsed_ms=<u128> messages=<usize> corpus_token=<String>`
// corpus_token is `latest_source_mtime_ms` equivalent proxy — just the newest
// mtime seen among scanned files under .claude and .codex, so the harness can
// assert the corpus did not change between paired runs.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::Instant;
use tokscale_core::{generate_local_graph_report, ReportOptions};

fn newest_mtime_ms(root: &std::path::Path) -> u128 {
    let mut newest = 0u128;
    for entry in walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
    {
        if let Ok(meta) = entry.metadata() {
            if let Ok(modified) = meta.modified() {
                if let Ok(dur) = modified.duration_since(std::time::UNIX_EPOCH) {
                    newest = newest.max(dur.as_millis());
                }
            }
        }
    }
    newest
}

/// Volatile keys, by name, at any depth: they differ between two runs of the
/// same binary, so leaving them in makes the no-op proof fail for the wrong
/// reason. Nothing else is removed — an exclusion list is safe to hand-write in
/// a way an inclusion list is not, because forgetting an entry here produces a
/// loud false mismatch rather than silent blindness.
const VOLATILE_KEYS: &[&str] = &["generated_at", "processing_time_ms"];

fn strip_volatile(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            map.retain(|k, _| !VOLATILE_KEYS.contains(&k.as_str()));
            for v in map.values_mut() {
                strip_volatile(v);
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(strip_volatile),
        _ => {}
    }
}

/// Arrays whose element order is NOT part of the output, addressed by JSON
/// path. Only these get sorted; every other array keeps the order the report
/// produced it in.
///
/// Order is the default because most of these arrays have defined output
/// semantics: `contributions` is sorted by date and `years` by year
/// (`aggregator.rs:51`, `:193`), consumers index into them, and sorting them
/// here would make the digest blind to a reordering — the exact regression a
/// batching change can cause. `summary.clients` and `summary.models` are
/// likewise sorted by the aggregator, so preserving their order also keeps the
/// digest sensitive to that sort breaking.
///
/// A per-day `clients` list is the one genuine exception: it comes from
/// `HashMap::into_values()` (`aggregator.rs:450`) and varies between two runs
/// of the same binary. The very first version of this digest hashed it as-is
/// and the no-op proof caught it immediately.
///
/// Getting this list wrong fails safe in one direction: a missed unordered
/// array makes `OLD-a != OLD-b` and stops the run loudly, whereas sorting an
/// ordered array is silent. That asymmetry is why the default is to preserve.
const UNORDERED_ARRAY_PATHS: &[&str] = &["contributions[].clients"];

/// Render to a canonical string: object keys sorted, array order preserved
/// except at the paths above.
///
/// Numbers keep serde_json's shortest round-trip form, so an `f64` difference
/// below a micro-dollar still changes the digest; the earlier `{:.6}` string
/// formatting silently discarded that.
fn canonicalize(value: &serde_json::Value, path: &str) -> String {
    match value {
        serde_json::Value::Object(map) => {
            let mut parts: Vec<String> = map
                .iter()
                .map(|(k, v)| {
                    let child = if path.is_empty() {
                        k.clone()
                    } else {
                        format!("{path}.{k}")
                    };
                    format!("{}:{}", k, canonicalize(v, &child))
                })
                .collect();
            parts.sort();
            format!("{{{}}}", parts.join(","))
        }
        serde_json::Value::Array(items) => {
            let child = format!("{path}[]");
            let mut parts: Vec<String> = items.iter().map(|v| canonicalize(v, &child)).collect();
            if UNORDERED_ARRAY_PATHS.contains(&path) {
                parts.sort();
            }
            format!("[{}]", parts.join(","))
        }
        other => other.to_string(),
    }
}

fn main() {
    let home = std::env::args()
        .nth(1)
        .expect("usage: bench_scan <home_dir>");
    let home_path = std::path::PathBuf::from(&home);

    let corpus_token =
        newest_mtime_ms(&home_path.join(".claude")).max(newest_mtime_ms(&home_path.join(".codex")));

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let options = ReportOptions {
        home_dir: Some(home),
        use_env_roots: false,
        clients: Some(vec!["claude".to_string(), "codex".to_string()]),
        ..Default::default()
    };

    let start = Instant::now();
    let result = runtime
        .block_on(generate_local_graph_report(options))
        .expect("graph report must succeed");
    let elapsed = start.elapsed().as_millis();

    // Digest of the ENTIRE serialized report, not a hand-picked field list.
    //
    // The previous two versions enumerated fields by hand and were each found
    // incomplete by review: the first omitted `provider_id` and the token
    // breakdown, the second still hashed only `contributions` — one of
    // `GraphResult`'s five fields — leaving `summary`, `years`, the stable half
    // of `meta`, and `time_metrics` entirely unchecked. `time_metrics` is not
    // derived from `contributions`; it comes from a separate sessionize fold,
    // so it can move on its own.
    //
    // Hand-enumeration is the defect, not any particular omission: it silently
    // stops covering a field the moment one is added. Serializing the whole
    // result and canonicalizing the JSON is exhaustive by construction, and a
    // new field joins the digest without anyone remembering to add it.
    let mut value = serde_json::to_value(&result).expect("GraphResult must serialize");
    // Volatile keys differ between two runs of the SAME binary, which is why a
    // naive byte comparison cannot be the oracle. Everything else stays.
    strip_volatile(&mut value);
    let canonical = canonicalize(&value, "");
    let mut hasher = DefaultHasher::new();
    canonical.hash(&mut hasher);
    let trace_digest = hasher.finish();

    println!(
        "elapsed_ms={} messages={} digest={} corpus_token={}",
        elapsed,
        result
            .contributions
            .iter()
            .map(|c| c.totals.messages)
            .sum::<i32>(),
        trace_digest,
        corpus_token,
    );
}
