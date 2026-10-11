//! Cursor usage events billed to Grok Bot are attributed to the `grok-bot`
//! client, not `cursor`. The invariant pinned here: for every request shape,
//! every report entry point agrees on tokens, messages and cost, and
//! cursor + grok + grok-bot = everything (conservation).
//!
//! Fixtures are synthetic and live under a temp HOME.

use super::*;
use std::collections::BTreeMap;
use std::path::Path;

struct Env(Vec<(&'static str, Option<std::ffi::OsString>)>);

impl Env {
    fn set(vars: &[(&'static str, &std::ffi::OsStr)]) -> Self {
        let saved = vars
            .iter()
            .map(|(k, _)| (*k, std::env::var_os(k)))
            .collect();
        for (k, v) in vars {
            unsafe { std::env::set_var(k, v) };
        }
        Self(saved)
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        for (k, v) in self.0.drain(..) {
            unsafe {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
    }
}

// Three distinct days so hourly/monthly/graph buckets never merge events.
const BOT_TS: i64 = 1_760_000_000_000;
const PLAIN_TS: i64 = BOT_TS + 86_400_000;
const GROK_TS: i64 = BOT_TS + 2 * 86_400_000;

fn event(ts: i64, model: &str, conv: &str, input: i64, output: i64, cents: i64) -> String {
    format!(
        r#"{{"timestamp":"{ts}","model":"{model}","kind":"USAGE_EVENT_KIND_CUSTOM_SUBSCRIPTION","conversationId":"{conv}","tokenUsage":{{"inputTokens":{input},"outputTokens":{output},"totalCents":{cents}}}}}"#
    )
}

/// One `grok-bot-default` event (110 tokens, $2.00) + one plain Cursor event
/// (1,100 tokens, $3.00).
fn usage_json() -> String {
    format!(
        r#"{{"totalUsageEventsCount":2,"usageEventsDisplay":[{},{}]}}"#,
        event(BOT_TS, "grok-bot-default", "bot-1", 100, 10, 200),
        event(PLAIN_TS, "claude-4-sonnet", "plain-1", 1000, 100, 300),
    )
}

fn write_cursor_json(dir: &Path, name: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join(name), usage_json()).unwrap();
}

/// Grok Build local data: one turn, 10 tokens.
fn write_grok_build(home: &Path) {
    let dir = home.join(".grok/sessions/%2Ftmp%2Fproject/session-1");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("updates.jsonl"),
        format!(
            r#"{{"method":"session/update","params":{{"sessionId":"session-1","update":{{"sessionUpdate":"turn_completed","usage":{{"inputTokens":7,"outputTokens":3,"totalTokens":10}}}},"_meta":{{"eventId":"turn-1","agentTimestampMs":{GROK_TS}}}}}}}"#
        ),
    )
    .unwrap();
}

fn cursor_cache(home: &Path) -> std::path::PathBuf {
    home.join(".config/tokscale/cursor-cache")
}

/// What one report says about a request; `None` = the entry point does not
/// expose that quantity.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
struct M {
    tokens: Option<i64>,
    msgs: Option<i64>,
    cost_milli: Option<i64>,
}

fn milli(cost: f64) -> Option<i64> {
    Some((cost * 1000.0).round() as i64)
}

fn m(tokens: i64, msgs: i64, cents: i64) -> M {
    M {
        tokens: Some(tokens),
        msgs: Some(msgs),
        cost_milli: Some(cents * 10),
    }
}

fn plus(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    a.zip(b).map(|(a, b)| a + b)
}

impl std::ops::Add for M {
    type Output = M;
    fn add(self, o: M) -> M {
        M {
            tokens: plus(self.tokens, o.tokens),
            msgs: plus(self.msgs, o.msgs),
            cost_milli: plus(self.cost_milli, o.cost_milli),
        }
    }
}

/// `self` must equal `want` on every quantity the entry point exposes.
fn assert_matches(got: M, want: M, what: &str) {
    for (g, w, q) in [
        (got.tokens, want.tokens, "tokens"),
        (got.msgs, want.msgs, "messages"),
        (got.cost_milli, want.cost_milli, "cost(milli$)"),
    ] {
        if g.is_some() {
            assert_eq!(g, w, "{what}: {q}");
        }
    }
}

const ENTRIES: [&str; 12] = [
    "model",
    "monthly",
    "graph",
    "hourly",
    "agents",
    "window",
    "time",
    "model+ctx",
    "graph+ctx",
    "hourly+ctx",
    "agents+ctx",
    "window+ctx",
];

async fn measure(
    entry: &str,
    home: &Path,
    settings: &scanner::ScannerSettings,
    clients: &Option<Vec<String>>,
) -> M {
    let opts = || ReportOptions {
        home_dir: Some(home.to_string_lossy().into_owned()),
        clients: clients.clone(),
        scanner_settings: settings.clone(),
        ..Default::default()
    };
    let ctx = || {
        ResolvedLocalSourceContext::capture(Some(home.to_path_buf()), false, settings.clone())
            .unwrap()
    };
    let (from, until) = (0, i64::MAX);
    let from_model = |r: ModelReport| M {
        tokens: Some(r.total_input + r.total_output + r.total_cache_read + r.total_cache_write),
        msgs: Some(r.total_messages as i64),
        cost_milli: milli(r.total_cost),
    };
    let from_graph = |g: GraphResult| M {
        tokens: Some(g.summary.total_tokens),
        msgs: None,
        cost_milli: milli(g.summary.total_cost),
    };
    let from_hourly = |r: HourlyReport| M {
        tokens: Some(
            r.entries
                .iter()
                .map(|e| e.input + e.output + e.cache_read + e.cache_write)
                .sum(),
        ),
        msgs: Some(r.entries.iter().map(|e| e.message_count as i64).sum()),
        cost_milli: milli(r.total_cost),
    };
    let from_agents = |r: AgentReport| M {
        tokens: Some(
            r.entries
                .iter()
                .map(|e| e.input + e.output + e.cache_read + e.cache_write)
                .sum(),
        ),
        msgs: Some(r.total_messages as i64),
        cost_milli: milli(r.total_cost),
    };
    let from_window = |w: WindowUsage| M {
        tokens: Some(
            w.messages
                .iter()
                .map(|e| e.input + e.output + e.cache_read + e.cache_write)
                .sum(),
        ),
        msgs: Some(w.messages.len() as i64),
        cost_milli: milli(w.messages.iter().map(|e| e.cost).sum()),
    };
    match entry {
        "model" => from_model(get_model_report(opts()).await.unwrap()),
        "monthly" => {
            let r = get_monthly_report(opts()).await.unwrap();
            M {
                tokens: Some(
                    r.entries
                        .iter()
                        .map(|e| e.input + e.output + e.cache_read + e.cache_write)
                        .sum(),
                ),
                msgs: Some(r.entries.iter().map(|e| e.message_count as i64).sum()),
                cost_milli: milli(r.total_cost),
            }
        }
        "graph" => from_graph(generate_local_graph_report(opts()).await.unwrap()),
        "hourly" => from_hourly(get_hourly_report(opts()).await.unwrap()),
        "agents" => from_agents(get_agents_report(opts()).await.unwrap()),
        "window" => from_window(get_window_usage(opts(), from, until).await.unwrap()),
        // Sessions are the only exposed quantity; every fixture event is its own.
        "time" => M {
            msgs: Some(
                get_time_metrics_report(opts())
                    .await
                    .unwrap()
                    .metrics
                    .session_count as i64,
            ),
            ..Default::default()
        },
        "model+ctx" => from_model(
            get_model_report_with_source_context(&ctx(), opts())
                .await
                .unwrap(),
        ),
        "graph+ctx" => from_graph(
            generate_local_graph_report_with_source_context(&ctx(), opts())
                .await
                .unwrap()
                .into_graph(),
        ),
        "hourly+ctx" => from_hourly(
            get_hourly_report_with_source_context(&ctx(), opts())
                .await
                .unwrap(),
        ),
        "agents+ctx" => from_agents(
            get_agents_report_with_source_context(&ctx(), opts())
                .await
                .unwrap(),
        ),
        "window+ctx" => from_window(
            get_window_usage_with_source_context(&ctx(), opts(), from, until)
                .await
                .unwrap(),
        ),
        other => panic!("unknown entry {other}"),
    }
}

fn req(ids: &[&str]) -> Option<Vec<String>> {
    Some(ids.iter().map(|s| s.to_string()).collect())
}

/// Entry points where `Some([])` already means "all"; on hourly, agents and
/// window it is an exact empty set (existing behaviour, apps send `None`).
fn empty_means_all(entry: &str) -> bool {
    !matches!(
        entry,
        "hourly" | "agents" | "window" | "hourly+ctx" | "agents+ctx" | "window+ctx"
    )
}

/// The consistency invariant over every entry point and request shape.
/// `grok` is whatever the Grok Build fixture measures (10 tokens, 1 message).
async fn assert_invariant(
    home: &Path,
    settings: &scanner::ScannerSettings,
    label: &str,
    entries: &[&str],
) {
    let bot = m(110, 1, 200);
    let plain = m(1100, 1, 300);
    for &entry in entries {
        let at = |what: &str| format!("{label} / {entry} / {what}");
        let run = |c: Option<Vec<String>>| async move { measure(entry, home, settings, &c).await };
        // The time report exposes only a session count; messages are 1 each.
        let (bot, plain) = if entry == "time" {
            (m(0, 1, 0), m(0, 1, 0))
        } else {
            (bot, plain)
        };
        let grok = run(req(&["grok"])).await;
        let grok_expect = if entry == "time" {
            m(0, 1, 0)
        } else if entry.starts_with("graph") {
            M {
                tokens: Some(10),
                msgs: None,
                cost_milli: grok.cost_milli,
            }
        } else {
            M {
                tokens: Some(10),
                msgs: Some(1),
                cost_milli: grok.cost_milli,
            }
        };
        assert_matches(grok, grok_expect, &at("[grok]"));
        let g = grok_expect;

        assert_matches(run(req(&["cursor"])).await, plain, &at("[cursor]"));
        assert_matches(run(req(&["grok-bot"])).await, bot, &at("[grok-bot]"));
        assert_matches(
            run(req(&["grok", "grok-bot"])).await,
            g + bot,
            &at("[grok,grok-bot]"),
        );
        assert_matches(
            run(req(&["cursor", "grok-bot"])).await,
            plain + bot,
            &at("[cursor,grok-bot]"),
        );
        let all = plain + bot + g;
        assert_matches(run(None).await, all, &at("None"));
        if empty_means_all(entry) {
            assert_matches(run(req(&[])).await, all, &at("[]"));
        }
    }
}

fn no_settings() -> scanner::ScannerSettings {
    scanner::ScannerSettings::default()
}

fn offline_env(cache_home: &Path) -> Env {
    Env::set(&[
        ("HOME", cache_home.as_os_str()),
        ("TOKSCALE_CONFIG_DIR", cache_home.as_os_str()),
        ("TOKSCALE_PRICING_CACHE_ONLY", std::ffi::OsStr::new("1")),
    ])
}

/// Acceptance 1. On a head without the Cursor-parser tag, `["cursor"]` counts
/// both events and `["grok-bot"]` counts none.
#[tokio::test]
#[serial_test::serial]
async fn grok_bot_events_are_not_counted_as_cursor() {
    let cache_home = tempfile::TempDir::new().unwrap();
    let _env = offline_env(cache_home.path());
    let home = tempfile::TempDir::new().unwrap();
    write_cursor_json(&cursor_cache(home.path()), "usage.json");

    let cursor = measure("model", home.path(), &no_settings(), &req(&["cursor"])).await;
    assert_matches(
        cursor,
        m(1100, 1, 300),
        "[cursor] counts only the plain event",
    );
    let bot = measure("model", home.path(), &no_settings(), &req(&["grok-bot"])).await;
    assert_matches(
        bot,
        m(110, 1, 200),
        "[grok-bot] counts only the Grok Bot event",
    );
}

/// Acceptance 2: conservation on every entry point, plain (non-takeover) roots,
/// with Grok Build local data present. One test per entry point so a guard
/// that only one path depends on is killed by that path's name.
macro_rules! conservation_test {
    ($($name:ident => $entry:expr),* $(,)?) => {$(
        #[tokio::test]
        #[serial_test::serial]
        async fn $name() {
            let cache_home = tempfile::TempDir::new().unwrap();
            let _env = offline_env(cache_home.path());
            let home = tempfile::TempDir::new().unwrap();
            write_cursor_json(&cursor_cache(home.path()), "usage.json");
            write_grok_build(home.path());
            assert_invariant(home.path(), &no_settings(), "plain", &[$entry]).await;
        }
    )*};
}

conservation_test! {
    grok_bot_conservation_model => "model",
    grok_bot_conservation_monthly => "monthly",
    grok_bot_conservation_graph => "graph",
    grok_bot_conservation_hourly => "hourly",
    grok_bot_conservation_agents => "agents",
    grok_bot_conservation_window => "window",
    grok_bot_conservation_time_metrics => "time",
    grok_bot_conservation_model_ctx => "model+ctx",
    grok_bot_conservation_graph_ctx => "graph+ctx",
    grok_bot_conservation_hourly_ctx => "hourly+ctx",
    grok_bot_conservation_agents_ctx => "agents+ctx",
    grok_bot_conservation_window_ctx => "window+ctx",
}

/// Acceptance 3: the CSV lane splits the same way (and the model name match is
/// ASCII case-insensitive).
#[tokio::test]
#[serial_test::serial]
async fn grok_bot_csv_lane_is_split_from_cursor() {
    let cache_home = tempfile::TempDir::new().unwrap();
    let _env = offline_env(cache_home.path());
    let home = tempfile::TempDir::new().unwrap();
    let cache = cursor_cache(home.path());
    std::fs::create_dir_all(&cache).unwrap();
    let csv = "Date,Kind,Model,Max Mode,Input (w/ Cache Write),Input (w/o Cache Write),Cache Read,Output Tokens,Total Tokens,Cost\n\
\"2025-10-09T08:00:00.000Z\",\"Included\",\"Grok-Bot-Default\",\"No\",\"0\",\"100\",\"0\",\"10\",\"110\",\"2.00\"\n\
\"2025-10-10T08:00:00.000Z\",\"Included\",\"claude-4-sonnet\",\"No\",\"0\",\"1000\",\"0\",\"100\",\"1100\",\"3.00\"";
    std::fs::write(cache.join("usage.csv"), csv).unwrap();

    for entry in ["model", "window", "hourly"] {
        let at = |w: &str| format!("csv / {entry} / {w}");
        let hp = home.path();
        let s = no_settings();
        let run = |c: Option<Vec<String>>| {
            let s = &s;
            async move { measure(entry, hp, s, &c).await }
        };
        assert_matches(
            run(req(&["cursor"])).await,
            m(1100, 1, 300),
            &at("[cursor]"),
        );
        assert_matches(
            run(req(&["grok-bot"])).await,
            m(110, 1, 200),
            &at("[grok-bot]"),
        );
        assert_matches(run(None).await, m(1210, 2, 500), &at("None"));
    }
}

/// Acceptance 4: takeover. The excluded CLI root and the synced extra dir each
/// hold the same two events; nothing is counted from both.
#[tokio::test]
#[serial_test::serial]
async fn grok_bot_takeover_counts_each_event_once() {
    let cache_home = tempfile::TempDir::new().unwrap();
    let _env = offline_env(cache_home.path());
    let home = tempfile::TempDir::new().unwrap();
    let cli = cursor_cache(home.path());
    write_cursor_json(&cli, "usage.json");
    let sync = home.path().join("sync");
    write_cursor_json(&sync, "usage.synced.json");
    write_grok_build(home.path());

    // Control: without the exclusion both roots count, so the fixture is not inert.
    let both = scanner::ScannerSettings {
        extra_scan_paths: BTreeMap::from([("cursor".to_string(), vec![sync.clone()])]),
        ..Default::default()
    };
    let doubled = measure("model", home.path(), &both, &req(&["grok-bot"])).await;
    assert_eq!(doubled.msgs, Some(2), "control: two roots, two copies");

    let taken = scanner::ScannerSettings {
        extra_scan_paths: both.extra_scan_paths.clone(),
        excluded_scan_paths: BTreeMap::from([("cursor".to_string(), vec![cli])]),
        ..Default::default()
    };
    assert_invariant(home.path(), &taken, "takeover", &ENTRIES).await;
}

/// `grok-bot` is a request id for the Cursor lane only; it never matches a
/// plain-Cursor model, and `cursor-grok-4.6-*` style Cursor models stay Cursor.
#[test]
fn cursor_client_is_chosen_by_model_prefix() {
    use sessions::cursor::parse_cursor_file;
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("usage.json");
    let events = [
        ("grok-bot-default", "grok-bot"),
        ("GROK-BOT-future", "grok-bot"),
        ("grok-bot", "grok-bot"),
        ("grok-bo", "cursor"),
        ("grok-4.6", "cursor"),
        ("cursor-grok-bot-x", "cursor"),
        ("claude-4-sonnet", "cursor"),
    ];
    let body: Vec<String> = events
        .iter()
        .enumerate()
        .map(|(i, (model, _))| event(BOT_TS + i as i64, model, &format!("c{i}"), 1, 1, 1))
        .collect();
    std::fs::write(
        &path,
        format!(r#"{{"usageEventsDisplay":[{}]}}"#, body.join(",")),
    )
    .unwrap();
    let got: Vec<(String, String)> = parse_cursor_file(&path)
        .into_iter()
        .map(|m| (m.model_id, m.client))
        .collect();
    let want: Vec<(String, String)> = events
        .iter()
        .map(|(m, c)| (m.to_string(), c.to_string()))
        .collect();
    assert_eq!(got, want);
}
