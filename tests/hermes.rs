use rusqlite::{params, Connection};
use tempfile::TempDir;
use tokscale_core::sessions::hermes::parse_hermes_sqlite;

fn create_test_db(dir: &TempDir) -> std::path::PathBuf {
    let db_path = dir.path().join("state.db");
    let conn = Connection::open(&db_path).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE sessions (
            id TEXT PRIMARY KEY,
            source TEXT NOT NULL,
            model TEXT,
            started_at REAL NOT NULL,
            message_count INTEGER DEFAULT 0,
            input_tokens INTEGER DEFAULT 0,
            output_tokens INTEGER DEFAULT 0,
            cache_read_tokens INTEGER DEFAULT 0,
            cache_write_tokens INTEGER DEFAULT 0,
            reasoning_tokens INTEGER DEFAULT 0,
            billing_provider TEXT,
            estimated_cost_usd REAL,
            actual_cost_usd REAL
        );
        "#,
    )
    .unwrap();
    db_path
}

#[test]
fn test_parse_hermes_sqlite_reads_session_rows_and_preserves_message_count() {
    let dir = TempDir::new().unwrap();
    let db_path = create_test_db(&dir);
    let conn = Connection::open(&db_path).unwrap();

    conn.execute(
        r#"
        INSERT INTO sessions (
            id, source, model, started_at, message_count,
            input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens,
            billing_provider, estimated_cost_usd, actual_cost_usd
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
        "#,
        params![
            "session-1",
            "cli",
            "claude-sonnet-4",
            1_750_000_000.25_f64,
            42_i64,
            1200_i64,
            300_i64,
            50_i64,
            20_i64,
            10_i64,
            "anthropic",
            0.12_f64,
            0.34_f64,
        ],
    )
    .unwrap();

    let messages = parse_hermes_sqlite(&db_path);
    assert_eq!(messages.len(), 1);

    let msg = &messages[0];
    assert_eq!(msg.client, "hermes");
    assert_eq!(msg.agent.as_deref(), Some("Hermes Agent"));
    assert_eq!(msg.session_id, "session-1");
    assert_eq!(msg.model_id, "claude-sonnet-4");
    assert_eq!(msg.provider_id, "anthropic");
    assert_eq!(msg.timestamp, 1_750_000_000_250_i64);
    assert_eq!(msg.message_count, 42);
    assert_eq!(msg.tokens.input, 1200);
    // `output_tokens` is reasoning-inclusive in Hermes, so the additive output
    // bucket keeps only the non-reasoning remainder.
    assert_eq!(msg.tokens.output, 290);
    assert_eq!(msg.tokens.cache_read, 50);
    assert_eq!(msg.tokens.cache_write, 20);
    assert_eq!(msg.tokens.reasoning, 10);
    assert_eq!(msg.cost, 0.34);
    assert_eq!(msg.dedup_key.as_deref(), Some("session-1"));
}

/// Hermes stores `reasoning_tokens` inside `output_tokens`, but `TokenBreakdown`
/// buckets are additive and `compute_cost` prices output and reasoning at the
/// same rate, so both fields mapped through unmodified would bill every
/// reasoning token twice. The overlap is split out of the output bucket without
/// changing the row's token total (upstream #1411).
#[test]
fn test_parse_hermes_sqlite_bills_reasoning_once() {
    let dir = TempDir::new().unwrap();
    let db_path = create_test_db(&dir);
    let conn = Connection::open(&db_path).unwrap();

    conn.execute(
        r#"
        INSERT INTO sessions (
            id, source, model, started_at, message_count,
            input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens,
            billing_provider
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
        "#,
        params![
            "session-reasoning",
            "cli",
            "deepseek-flash",
            1_775_002_000.0_f64,
            1_i64,
            1000_i64,
            400_i64,
            50_i64,
            20_i64,
            150_i64,
            "deepseek",
        ],
    )
    .unwrap();

    let messages = parse_hermes_sqlite(&db_path);
    assert_eq!(messages.len(), 1);

    let msg = &messages[0];
    assert_eq!(msg.tokens.output, 250);
    assert_eq!(msg.tokens.reasoning, 150);
    assert_eq!(msg.tokens.total(), 1_000 + 400 + 50 + 20);
}

/// A malformed row claiming more reasoning than output must not drive the
/// additive output bucket negative.
#[test]
fn test_parse_hermes_sqlite_clamps_reasoning_to_output() {
    let dir = TempDir::new().unwrap();
    let db_path = create_test_db(&dir);
    let conn = Connection::open(&db_path).unwrap();

    conn.execute(
        r#"
        INSERT INTO sessions (
            id, source, model, started_at, message_count,
            input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens,
            billing_provider
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
        "#,
        params![
            "session-clamped",
            "cli",
            "deepseek-flash",
            1_775_002_000.0_f64,
            1_i64,
            10_i64,
            30_i64,
            0_i64,
            0_i64,
            90_i64,
            "deepseek",
        ],
    )
    .unwrap();

    let messages = parse_hermes_sqlite(&db_path);
    assert_eq!(messages.len(), 1);

    let msg = &messages[0];
    assert_eq!(msg.tokens.output, 0);
    assert_eq!(msg.tokens.reasoning, 30);
    assert_eq!(msg.tokens.total(), 10 + 30);
}

#[test]
fn test_parse_hermes_sqlite_skips_empty_sessions_and_falls_back_to_estimated_cost_and_provider_inference(
) {
    let dir = TempDir::new().unwrap();
    let db_path = create_test_db(&dir);
    let conn = Connection::open(&db_path).unwrap();

    conn.execute(
        r#"
        INSERT INTO sessions (
            id, source, model, started_at, message_count,
            input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens,
            billing_provider, estimated_cost_usd, actual_cost_usd
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
        "#,
        params![
            "session-valid",
            "telegram",
            "gpt-5.4",
            1_775_001_102.0_f64,
            3_i64,
            100_i64,
            20_i64,
            0_i64,
            0_i64,
            5_i64,
            Option::<String>::None,
            1.25_f64,
            Option::<f64>::None,
        ],
    )
    .unwrap();

    conn.execute(
        r#"
        INSERT INTO sessions (
            id, source, model, started_at, message_count,
            input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens,
            billing_provider, estimated_cost_usd, actual_cost_usd
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
        "#,
        params![
            "session-empty",
            "telegram",
            "gpt-5.4",
            1_775_001_103.0_f64,
            9_i64,
            0_i64,
            0_i64,
            0_i64,
            0_i64,
            0_i64,
            Option::<String>::None,
            Option::<f64>::None,
            Option::<f64>::None,
        ],
    )
    .unwrap();

    conn.execute(
        r#"
        INSERT INTO sessions (
            id, source, model, started_at, message_count,
            input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens,
            billing_provider, estimated_cost_usd, actual_cost_usd
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
        "#,
        params![
            "session-no-model",
            "cli",
            Option::<String>::None,
            1_775_001_104.0_f64,
            1_i64,
            500_i64,
            20_i64,
            0_i64,
            0_i64,
            0_i64,
            Some("openai".to_string()),
            0.2_f64,
            0.2_f64,
        ],
    )
    .unwrap();

    let messages = parse_hermes_sqlite(&db_path);
    assert_eq!(messages.len(), 1);

    let msg = &messages[0];
    assert_eq!(msg.session_id, "session-valid");
    assert_eq!(msg.provider_id, "openai");
    assert_eq!(msg.cost, 1.25);
    assert_eq!(msg.message_count, 3);
}

#[test]
fn test_parse_hermes_sqlite_ignores_unknown_billing_provider_and_falls_back_to_model_inference() {
    let dir = TempDir::new().unwrap();
    let db_path = create_test_db(&dir);
    let conn = Connection::open(&db_path).unwrap();

    conn.execute(
        r#"
        INSERT INTO sessions (
            id, source, model, started_at, message_count,
            input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens,
            billing_provider, estimated_cost_usd, actual_cost_usd
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
        "#,
        params![
            "session-unknown-provider",
            "cli",
            "gpt-5.4",
            1_775_001_105.0_f64,
            2_i64,
            100_i64,
            20_i64,
            0_i64,
            0_i64,
            0_i64,
            Some("unknown".to_string()),
            0.5_f64,
            Option::<f64>::None,
        ],
    )
    .unwrap();

    let messages = parse_hermes_sqlite(&db_path);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].provider_id, "openai");
}
