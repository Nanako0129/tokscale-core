// Acceptance probe for Codex turn counting. Not part of the library's public
// surface and not wired into any build/test/release path — it exists so a
// parser change that claims to alter turn counts can be measured on a real
// corpus through the shipping code path rather than through a reimplementation.
//
// Usage: turns_probe <home_dir>
//
// `home_dir` is the root that CONTAINS `.codex/sessions`, not the sessions
// directory itself: the scan derives from `home_dir` plus the client's declared
// `.codex` / `sessions` relatives, and `use_env_roots` is false so CODEX_HOME
// and friends are ignored. Pointing this at the sessions directory silently
// scans nothing.
//
// Env: TOKSCALE_CONFIG_DIR selects the cache directory. Set it to a scratch
// path — a probe run against the user's real cache both pollutes it and makes
// the measurement depend on whatever was cached before.
// TOKSCALE_PRICING_CACHE_ONLY=1 avoids a network pricing fetch so paired runs
// price identically.
//
// Prints one JSON object per line, sorted by date:
//   {"date":"2026-09-07","turns":78,"tokens":123,"cost":1.5,"messages":9}
// followed by a final {"total":...} line. `turns` is the Codex bucket only;
// `tokens`/`cost`/`messages` are the day's totals across the scanned clients,
// and exist so a caller can prove a turn-only change moved nothing else.

use tokscale_core::{generate_local_graph_report, ReportOptions};

fn main() {
    let home = std::env::args()
        .nth(1)
        .expect("usage: turns_probe <home_dir>");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let options = ReportOptions {
        home_dir: Some(home),
        use_env_roots: false,
        clients: Some(vec!["codex".to_string()]),
        ..Default::default()
    };

    let result = runtime
        .block_on(generate_local_graph_report(options))
        .expect("graph report must succeed");

    let mut days: Vec<_> = result.contributions.iter().collect();
    days.sort_by(|a, b| a.date.cmp(&b.date));

    let mut total_turns: i64 = 0;
    let mut total_tokens: i64 = 0;
    let mut total_messages: i64 = 0;
    let mut total_cost = 0.0f64;

    for day in days {
        let turns = day.turns_by_client.get("codex").copied().unwrap_or(0);
        total_turns += turns;
        total_tokens += day.totals.tokens;
        total_messages += i64::from(day.totals.messages);
        total_cost += day.totals.cost;
        println!(
            r#"{{"date":"{}","turns":{},"tokens":{},"cost":{:.6},"messages":{}}}"#,
            day.date, turns, day.totals.tokens, day.totals.cost, day.totals.messages
        );
    }

    println!(
        r#"{{"total":true,"turns":{},"tokens":{},"cost":{:.6},"messages":{}}}"#,
        total_turns, total_tokens, total_cost, total_messages
    );
}
