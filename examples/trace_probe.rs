// Oracle probe for the batched-parallel-parse acceptance run (throwaway,
// not part of the plan's diff — see debug_stream_trace's TEMPORARY marker).
//
// Usage: trace_probe <home_dir>
// Prints: trace_sha256=<hex> report_sha256=<hex> messages=<n> days=<n>
//
// trace_sha256 hashes the ordered scan_messages_streaming emission
// (JSON-serialized, one message per line) — the "ordered emitted-message
// trace" the plan calls the invariant that actually matters.
//
// report_sha256 hashes a normalized GraphResult: volatile fields
// (generated_at, processing_time_ms) stripped, and every unordered
// container canonicalized recursively — per-day `clients` sorted by
// (client, model_id, provider_id), not just the top-level per-day list
// (which is already date-sorted by StreamingAggregator::finalize).

use sha2::{Digest, Sha256};
use tokscale_core::{debug_stream_trace, generate_local_graph_report, ReportOptions};

fn main() {
    let home = std::env::args()
        .nth(1)
        .expect("usage: trace_probe <home_dir>");
    let clients = vec!["claude".to_string(), "codex".to_string()];

    let trace = debug_stream_trace(&home, &clients);
    let mut trace_hasher = Sha256::new();
    for message in &trace {
        trace_hasher.update(serde_json::to_vec(message).unwrap());
        trace_hasher.update(b"\n");
    }
    let trace_hash = trace_hasher.finalize();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let options = ReportOptions {
        home_dir: Some(home),
        use_env_roots: false,
        clients: Some(clients),
        ..Default::default()
    };
    let mut result = runtime
        .block_on(generate_local_graph_report(options))
        .expect("graph report must succeed");

    // Strip volatile fields.
    result.meta.generated_at = String::new();
    result.meta.processing_time_ms = 0;
    // Canonicalize the one unordered container: per-day clients.
    for contribution in &mut result.contributions {
        contribution.clients.sort_by(|a, b| {
            (&a.client, &a.model_id, &a.provider_id).cmp(&(&b.client, &b.model_id, &b.provider_id))
        });
    }

    let report_json = serde_json::to_vec(&result).unwrap();
    let mut report_hasher = Sha256::new();
    report_hasher.update(&report_json);
    let report_hash = report_hasher.finalize();

    println!(
        "trace_sha256={} report_sha256={} messages={} days={}",
        hex(&trace_hash),
        hex(&report_hash),
        trace.len(),
        result.contributions.len(),
    );
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
