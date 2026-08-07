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

    // Order-sensitive digest of the report's per-day, per-client rows. The
    // ordering inside a day comes from the fold, so a reordering of the
    // emitted-message stream changes this value. Volatile metadata
    // (`generated_at`, `processing_time_ms`) is deliberately excluded — it
    // differs between two runs of the SAME binary, which is why a naive
    // byte comparison cannot be the oracle.
    let mut hasher = DefaultHasher::new();
    // Canonicalize every unordered container before hashing. Per-day client
    // rows come from `HashMap::into_values()`, so their order varies between
    // two runs of the SAME binary — the first version of this digest hashed
    // them as-is and the no-op proof caught it immediately.
    let mut days: Vec<_> = result.contributions.iter().collect();
    days.sort_by(|a, b| a.date.cmp(&b.date));
    for c in days {
        c.date.hash(&mut hasher);
        c.totals.messages.hash(&mut hasher);
        c.totals.tokens.hash(&mut hasher);
        format!("{:.6}", c.totals.cost).hash(&mut hasher);
        let mut rows: Vec<_> = c.clients.iter().collect();
        rows.sort_by(|a, b| (&a.client, &a.model_id).cmp(&(&b.client, &b.model_id)));
        for cl in rows {
            cl.client.hash(&mut hasher);
            cl.model_id.hash(&mut hasher);
            cl.messages.hash(&mut hasher);
            format!("{:.6}", cl.cost).hash(&mut hasher);
        }
    }
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
