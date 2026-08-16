use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serial_test::serial;
use tempfile::TempDir;
use tokscale_core::{
    aggregate_remote_usage_from_source_v1, aggregate_remote_usage_v1, pricing::ModelPricing,
    LocalParseOptions, RemotePricingDiagnostic, RemotePricingSnapshot, RemoteSourceContextError,
    RemoteSourceContextV1, RemoteSourceScopeTokenV1, RemoteSourceUsageError, RemoteUsageQueryV1,
    ScannerSettings,
};

fn query_for_clients(clients: Vec<String>) -> RemoteUsageQueryV1 {
    RemoteUsageQueryV1 {
        clients,
        start_date: "2040-01-01".to_owned(),
        end_date_exclusive: "2040-01-02".to_owned(),
        timezone: "UTC".to_owned(),
        tzdb_revision: "2026c".to_owned(),
    }
}

fn query() -> RemoteUsageQueryV1 {
    query_for_clients(vec!["codex".to_owned()])
}

fn context(
    home: &Path,
    config: &Path,
    data: &Path,
    cache: &Path,
    settings: ScannerSettings,
) -> (RemoteSourceContextV1, RemoteSourceScopeTokenV1) {
    let fingerprint =
        RemoteSourceContextV1::preview_fingerprint(home, config, data, cache, &settings).unwrap();
    let token = RemoteSourceScopeTokenV1::new(fingerprint, 7);
    let context = RemoteSourceContextV1::new(home, config, data, cache, settings, token).unwrap();
    (context, token)
}

fn write_codex_fixture(home: &Path) {
    let path = home.join(".codex/sessions/remote.jsonl");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, include_str!("fixtures/codex_duration_timing.jsonl")).unwrap();
}

fn write_cc_mirror_fixture(home: &Path) {
    let variant = home.join(".cc-mirror/kimi-code");
    let config = variant.join("config");
    let project = config.join("projects/proj");
    fs::create_dir_all(&project).unwrap();
    fs::write(
        variant.join("variant.json"),
        serde_json::json!({
            "name": "kimi-code",
            "provider": "kimi",
            "configDir": config,
        })
        .to_string(),
    )
    .unwrap();
    fs::write(
        project.join("session.jsonl"),
        r#"{"type":"assistant","timestamp":"2040-01-01T11:00:00.000Z","requestId":"req_variant","message":{"id":"msg_variant","model":"claude-3-5-sonnet","usage":{"input_tokens":300,"output_tokens":70}}}"#,
    )
    .unwrap();
}

fn fresh_pricing(path: &Path, input: f64) {
    fs::create_dir_all(path).unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut data = std::collections::HashMap::new();
    data.insert(
        "codex-test-model".to_owned(),
        ModelPricing {
            input_cost_per_token: Some(input),
            output_cost_per_token: Some(input),
            ..Default::default()
        },
    );
    fs::write(
        path.join("pricing-litellm.json"),
        serde_json::to_vec(&serde_json::json!({"timestamp": now, "data": data})).unwrap(),
    )
    .unwrap();
}

#[test]
#[serial]
fn remote_context_ignores_environment_and_cwd() {
    let roots = TempDir::new().unwrap();
    let home = roots.path().join("home");
    let config = roots.path().join("config");
    let data = roots.path().join("data");
    let cache = roots.path().join("cache");
    let settings = ScannerSettings::default();
    let first =
        RemoteSourceContextV1::preview_fingerprint(&home, &config, &data, &cache, &settings)
            .unwrap();
    let old_cwd = env::current_dir().unwrap();
    let old_home = env::var_os("HOME");
    unsafe {
        env::set_var("HOME", roots.path().join("attacker-home"));
        env::set_current_dir(roots.path()).unwrap();
    }
    let second =
        RemoteSourceContextV1::preview_fingerprint(&home, &config, &data, &cache, &settings)
            .unwrap();
    unsafe {
        env::set_current_dir(old_cwd).unwrap();
        match old_home {
            Some(value) => env::set_var("HOME", value),
            None => env::remove_var("HOME"),
        }
    }
    assert_eq!(first, second);
}

#[test]
fn remote_scope_token_rejects_every_effective_input_change() {
    let roots = TempDir::new().unwrap();
    let home = roots.path().join("home");
    let config = roots.path().join("config");
    let data = roots.path().join("data");
    let cache = roots.path().join("cache");
    let (context, token) = context(&home, &config, &data, &cache, ScannerSettings::default());
    assert_eq!(context.source_scope_generation(), 7);
    for (new_home, new_config, new_data, new_cache, settings) in [
        (
            roots.path().join("home-2"),
            config.clone(),
            data.clone(),
            cache.clone(),
            ScannerSettings::default(),
        ),
        (
            home.clone(),
            roots.path().join("config-2"),
            data.clone(),
            cache.clone(),
            ScannerSettings::default(),
        ),
        (
            home.clone(),
            config.clone(),
            roots.path().join("data-2"),
            cache.clone(),
            ScannerSettings::default(),
        ),
        (
            home.clone(),
            config.clone(),
            data.clone(),
            roots.path().join("cache-2"),
            ScannerSettings::default(),
        ),
    ] {
        assert!(matches!(
            RemoteSourceContextV1::new(new_home, new_config, new_data, new_cache, settings, token),
            Err(RemoteSourceContextError::ScopeNotApproved)
        ));
    }
    let settings = ScannerSettings {
        opencode_db_paths: vec![roots.path().join("opencode.db")],
        ..Default::default()
    };
    assert!(matches!(
        RemoteSourceContextV1::new(home, config, data, cache, settings, token),
        Err(RemoteSourceContextError::ScopeNotApproved)
    ));
}

#[test]
fn remote_context_rejects_relative_roots_and_settings() {
    let settings = ScannerSettings {
        opencode_db_paths: vec![PathBuf::from("relative.db")],
        ..Default::default()
    };
    assert_eq!(
        RemoteSourceContextV1::preview_fingerprint(
            Path::new("/home"),
            Path::new("/config"),
            Path::new("/data"),
            Path::new("/cache"),
            &settings,
        ),
        Err(RemoteSourceContextError::InvalidScannerSettings)
    );
}

#[test]
fn pricing_snapshot_is_injected_deterministic_and_fail_soft() {
    let roots = TempDir::new().unwrap();
    let missing = RemotePricingSnapshot::from_cache_root(roots.path().join("missing"));
    assert_eq!(missing.diagnostic(), Some(RemotePricingDiagnostic::Missing));
    let first = missing.content_digest();
    fresh_pricing(roots.path(), 0.000001);
    let loaded = RemotePricingSnapshot::from_cache_root(roots.path());
    assert_ne!(first, loaded.content_digest());
    fresh_pricing(roots.path(), 0.000002);
    let changed = RemotePricingSnapshot::from_cache_root(roots.path());
    assert_ne!(loaded.content_digest(), changed.content_digest());
}

#[test]
fn source_fold_uses_production_fixture_and_matches_pure_fold() {
    let roots = TempDir::new().unwrap();
    let home = roots.path().join("home");
    let config = roots.path().join("config");
    let data = roots.path().join("data");
    let cache = roots.path().join("source-cache");
    write_codex_fixture(&home);
    let (context, _) = context(&home, &config, &data, &cache, ScannerSettings::default());
    let pricing = RemotePricingSnapshot::from_cache_root(roots.path().join("pricing"));
    let streamed = aggregate_remote_usage_from_source_v1(&context, &query(), &pricing).unwrap();
    assert!(!streamed.graph.is_empty());
    assert!(streamed.graph.iter().all(|row| row.client == "codex"));
    assert!(!serde_json::to_string(&streamed)
        .unwrap()
        .contains("remote.jsonl"));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let rows = runtime
        .block_on(tokscale_core::parse_local_unified_messages_with_pricing(
            tokscale_core::LocalParseOptions {
                home_dir: Some(home.to_string_lossy().into_owned()),
                use_env_roots: false,
                clients: Some(vec!["codex".to_owned()]),
                ..Default::default()
            },
            None,
        ))
        .unwrap();
    let pure = aggregate_remote_usage_v1(&rows, &query()).unwrap();
    assert_eq!(
        serde_json::to_vec(&streamed).unwrap(),
        serde_json::to_vec(&pure).unwrap()
    );
}

#[test]
fn source_fold_expands_dynamic_cc_mirror_client_for_scanner() {
    let roots = TempDir::new().unwrap();
    let home = roots.path().join("home");
    let config = roots.path().join("config");
    let data = roots.path().join("data");
    let cache = roots.path().join("source-cache");
    write_cc_mirror_fixture(&home);
    let (context, _) = context(&home, &config, &data, &cache, ScannerSettings::default());
    let query = query_for_clients(vec!["cc-mirror/kimi-code".to_owned()]);
    let pricing = RemotePricingSnapshot::from_cache_root(roots.path().join("pricing"));
    let streamed = aggregate_remote_usage_from_source_v1(&context, &query, &pricing).unwrap();
    assert_eq!(streamed.graph.len(), 1);
    assert_eq!(streamed.graph[0].client, "cc-mirror/kimi-code");
    assert_eq!(streamed.graph[0].input_tokens, 300);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let rows = runtime
        .block_on(tokscale_core::parse_local_unified_messages_with_pricing(
            LocalParseOptions {
                home_dir: Some(home.to_string_lossy().into_owned()),
                use_env_roots: false,
                clients: Some(vec!["claude".to_owned()]),
                ..Default::default()
            },
            None,
        ))
        .unwrap();
    let pure_rows: Vec<_> = rows
        .into_iter()
        .filter(|message| message.client == "cc-mirror/kimi-code")
        .collect();
    assert_eq!(pure_rows.len(), 1);
    let pure = aggregate_remote_usage_v1(&pure_rows, &query).unwrap();
    assert_eq!(pure.graph.len(), 1);
    assert_eq!(pure.graph[0].input_tokens, 300);
    assert_eq!(
        serde_json::to_vec(&streamed).unwrap(),
        serde_json::to_vec(&pure).unwrap()
    );
}

#[test]
fn source_errors_are_stable_and_payload_free() {
    let roots = TempDir::new().unwrap();
    let (context, _) = context(
        &roots.path().join("home"),
        &roots.path().join("config"),
        &roots.path().join("data"),
        &roots.path().join("cache"),
        ScannerSettings::default(),
    );
    let mut invalid = query();
    invalid.timezone = "not-a-zone-private-path".to_owned();
    let error = aggregate_remote_usage_from_source_v1(
        &context,
        &invalid,
        &RemotePricingSnapshot::from_cache_root(roots.path().join("pricing")),
    )
    .unwrap_err();
    assert_eq!(error, RemoteSourceUsageError::InvalidTimeZone);
    assert!(!error.to_string().contains("private-path"));
    assert!(!format!("{error:?}").contains("private-path"));
}

#[test]
fn public_bundle_schema_remains_allowlisted() {
    let encoded = serde_json::to_value(
        aggregate_remote_usage_v1(
            &[],
            &RemoteUsageQueryV1 {
                clients: vec![],
                start_date: "2040-01-01".into(),
                end_date_exclusive: "2040-01-02".into(),
                timezone: "UTC".into(),
                tzdb_revision: "2026c".into(),
            },
        )
        .unwrap(),
    )
    .unwrap();
    let keys: BTreeSet<&str> = encoded
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        BTreeSet::from([
            "tzdbRevision",
            "graph",
            "models",
            "hourly",
            "agents",
            "saturated"
        ])
    );
}
