use std::collections::BTreeSet;

use jiff::{tz::TimeZone, Timestamp};
use tokscale_core::{
    aggregate_remote_usage_v1, RemoteUsageError, RemoteUsageQueryV1, TokenBreakdown, UnifiedMessage,
};

fn query(timezone: &str) -> RemoteUsageQueryV1 {
    RemoteUsageQueryV1 {
        clients: Vec::new(),
        start_date: "2024-11-03".to_owned(),
        end_date_exclusive: "2024-11-04".to_owned(),
        timezone: timezone.to_owned(),
        tzdb_revision: "2026c".to_owned(),
    }
}

fn message(client: &str, model: &str, provider: &str, timestamp: i64) -> UnifiedMessage {
    UnifiedMessage::new(
        client,
        model,
        provider,
        "private-session-sentinel",
        timestamp,
        TokenBreakdown {
            input: 2,
            output: 3,
            cache_read: 5,
            cache_write: 7,
            reasoning: 11,
            cache_write_1h: 0,
        },
        0.0000000015,
    )
}

#[test]
fn remote_report_utc_fold_emits_all_four_sorted_pages_and_allowlisted_fields() {
    let mut first = message("alpha", "GPT-4o", "openai", 1_730_613_600_000);
    first.is_turn_start = true;
    first.duration_ms = Some(10);
    first.message_count = 2;
    first.agent = Some("worker-a".to_owned());
    let bundle = aggregate_remote_usage_v1(
        &[first],
        &RemoteUsageQueryV1 {
            clients: vec!["alpha".to_owned()],
            start_date: "2024-11-03".to_owned(),
            end_date_exclusive: "2024-11-04".to_owned(),
            timezone: "UTC".to_owned(),
            tzdb_revision: "2026c".to_owned(),
        },
    )
    .expect("valid remote fold");

    assert_eq!(bundle.graph.len(), 1);
    assert_eq!(bundle.models.len(), 1);
    assert_eq!(bundle.hourly.len(), 1);
    assert_eq!(bundle.agents.len(), 1);
    assert_eq!(bundle.graph[0].total_tokens, 28);
    assert_eq!(bundle.graph[0].message_count, 2);
    assert_eq!(bundle.graph[0].turn_count, 1);
    assert_eq!(bundle.models[0].duration_millis, 10);
    assert_eq!(bundle.models[0].timed_tokens, 28);
    assert_eq!(bundle.models[0].sample_count, 1);

    let encoded = serde_json::to_value(bundle).expect("serializes");
    let object = encoded.as_object().expect("object bundle");
    let keys: BTreeSet<&str> = object.keys().map(String::as_str).collect();
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
    let graph_keys: BTreeSet<&str> = object["graph"][0]
        .as_object()
        .expect("graph object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        graph_keys,
        BTreeSet::from([
            "date",
            "client",
            "model",
            "provider",
            "inputTokens",
            "outputTokens",
            "cacheReadTokens",
            "cacheWriteTokens",
            "reasoningTokens",
            "totalTokens",
            "messageCount",
            "turnCount",
            "costNanoUsd",
        ])
    );
    assert!(!encoded.to_string().contains("private-session-sentinel"));
}

#[test]
fn remote_report_new_york_fallback_has_two_hour_segments() {
    let messages = [
        message("alpha", "model", "provider", 1_730_611_800_000),
        message("alpha", "model", "provider", 1_730_615_400_000),
    ];
    let bundle = aggregate_remote_usage_v1(&messages, &query("America/New_York"))
        .expect("valid fallback timestamps");
    assert_eq!(bundle.hourly.len(), 2);
    assert_eq!(bundle.hourly[0].utc_offset_seconds, -14_400);
    assert_eq!(bundle.hourly[1].utc_offset_seconds, -18_000);
    assert_ne!(
        bundle.hourly[0].bucket_start_unix_ms,
        bundle.hourly[1].bucket_start_unix_ms
    );
}

#[test]
fn remote_report_lord_howe_gap_and_fold_use_transition_segment_starts() {
    let gap = message("alpha", "model", "provider", 1_728_142_500_000);
    let gap_query = RemoteUsageQueryV1 {
        clients: Vec::new(),
        start_date: "2024-10-06".to_owned(),
        end_date_exclusive: "2024-10-07".to_owned(),
        timezone: "Australia/Lord_Howe".to_owned(),
        tzdb_revision: "2026c".to_owned(),
    };
    let gap_bundle = aggregate_remote_usage_v1(&[gap], &gap_query).unwrap();
    assert_eq!(gap_bundle.hourly.len(), 1);
    assert_eq!(gap_bundle.hourly[0].utc_offset_seconds, 39_600);
    assert_eq!(gap_bundle.hourly[0].bucket_start_unix_ms, 1_728_142_200_000);

    let fold = [
        message("alpha", "model", "provider", 1_712_413_500_000),
        message("alpha", "model", "provider", 1_712_416_500_000),
    ];
    let fold_query = RemoteUsageQueryV1 {
        clients: Vec::new(),
        start_date: "2024-04-07".to_owned(),
        end_date_exclusive: "2024-04-08".to_owned(),
        timezone: "Australia/Lord_Howe".to_owned(),
        tzdb_revision: "2026c".to_owned(),
    };
    let fold_bundle = aggregate_remote_usage_v1(&fold, &fold_query).unwrap();
    assert_eq!(fold_bundle.hourly.len(), 2);
    assert_eq!(fold_bundle.hourly[0].utc_offset_seconds, 39_600);
    assert_eq!(fold_bundle.hourly[1].utc_offset_seconds, 37_800);
    assert_eq!(
        fold_bundle.hourly[0].bucket_start_unix_ms,
        1_712_412_000_000
    );
    assert_eq!(
        fold_bundle.hourly[1].bucket_start_unix_ms,
        1_712_415_600_000
    );
}

#[test]
fn remote_report_non_authoritative_fields_do_not_change_bytes() {
    let mut first = message("alpha", "model", "provider", 1_730_613_600_000);
    let mut second = first.clone();
    second.session_id = "another-secret".to_owned();
    second.workspace_key = Some("/private/workspace".to_owned());
    second.workspace_label = Some("private label".to_owned());
    second.date = "1900-01-01".to_owned();
    second.dedup_key = Some("dedup-sentinel".to_owned());
    second.dedup_aliases = vec!["alias-sentinel".to_owned()];
    let q = RemoteUsageQueryV1 {
        clients: Vec::new(),
        start_date: "2024-11-03".to_owned(),
        end_date_exclusive: "2024-11-04".to_owned(),
        timezone: "UTC".to_owned(),
        tzdb_revision: "2026c".to_owned(),
    };
    let first_bytes =
        serde_json::to_vec(&aggregate_remote_usage_v1(&[first.clone()], &q).unwrap()).unwrap();
    let second_bytes =
        serde_json::to_vec(&aggregate_remote_usage_v1(&[second], &q).unwrap()).unwrap();
    assert_eq!(first_bytes, second_bytes);
    first.timestamp = 1_730_617_200_000;
    assert_ne!(
        serde_json::to_vec(&aggregate_remote_usage_v1(&[first], &q).unwrap()).unwrap(),
        first_bytes
    );
}

#[test]
fn remote_report_invalid_input_classes_are_payload_free() {
    let mut invalid = query("UTC");
    invalid.tzdb_revision = "sentinel".to_owned();
    assert_eq!(
        aggregate_remote_usage_v1(&[], &invalid),
        Err(RemoteUsageError::IncompatibleTzdb)
    );
    assert!(!format!("{:?}", RemoteUsageError::InvalidNumerator).contains("sentinel"));
    assert!(!RemoteUsageError::InvalidNumerator
        .to_string()
        .contains("sentinel"));
}

#[test]
fn remote_report_invalid_timestamp_is_rejected_without_date_fallback() {
    let mut invalid = message("alpha", "model", "provider", 0);
    invalid.date = "2024-11-03".to_owned();
    assert_eq!(
        aggregate_remote_usage_v1(&[invalid], &query("UTC")),
        Err(RemoteUsageError::InvalidTimestamp)
    );
}

#[test]
fn remote_report_timezone_case_link_and_nfc_rules_are_explicit() {
    let first = message("alpha", "model", "provider", 1_730_611_800_000);
    let mut link_query = query("US/Eastern");
    link_query.start_date = "2024-11-03".to_owned();
    let link_bundle = aggregate_remote_usage_v1(&[first], &link_query).unwrap();
    assert_eq!(link_bundle.hourly.len(), 1);

    let mut wrong_case = query("america/new_york");
    wrong_case.start_date = "2024-11-03".to_owned();
    assert_eq!(
        aggregate_remote_usage_v1(&[], &wrong_case),
        Err(RemoteUsageError::InvalidTimeZone)
    );

    let mut decomposed = message("e\u{301}", "model", "provider", 1_730_611_800_000);
    decomposed.client = "e\u{301}".to_owned();
    assert_eq!(
        aggregate_remote_usage_v1(&[decomposed], &query("UTC")),
        Err(RemoteUsageError::InvalidText)
    );
}

#[test]
fn remote_report_cost_rounding_and_u64_saturation_are_stable() {
    let mut tie = message("alpha", "model", "provider", 1_730_613_600_000);
    tie.cost = 0.0000000005;
    let rounded = aggregate_remote_usage_v1(&[tie], &query("UTC")).unwrap();
    assert_eq!(rounded.graph[0].cost_nano_usd, 0);

    let mut max = message("alpha", "model", "provider", 1_730_613_600_000);
    max.tokens = TokenBreakdown {
        input: i64::MAX,
        ..TokenBreakdown::default()
    };
    let saturated =
        aggregate_remote_usage_v1(&[max.clone(), max.clone(), max], &query("UTC")).unwrap();
    assert!(saturated.saturated);
    assert_eq!(saturated.graph[0].input_tokens, u64::MAX);
    assert_eq!(saturated.graph[0].total_tokens, u64::MAX);
}

fn clients_with_total_bytes(total: usize) -> Vec<String> {
    let mut sizes = Vec::new();
    let mut remaining = total;
    while remaining > 255 {
        sizes.push(255);
        remaining -= 255;
    }
    if remaining > 0 {
        if remaining < 6 {
            let last = sizes.last_mut().expect("a prior client exists");
            *last -= 6 - remaining;
            remaining = 6;
        }
        sizes.push(remaining);
    }
    let mut clients: Vec<String> = sizes
        .into_iter()
        .enumerate()
        .map(|(index, length)| format!("{index:05}{}", "x".repeat(length - 5)))
        .collect();
    clients.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    clients
}

#[test]
fn remote_report_query_cap_counts_all_query_strings() {
    let mut at_cap = query("UTC");
    at_cap.clients = clients_with_total_bytes(16_384 - 28);
    assert!(aggregate_remote_usage_v1(&[], &at_cap).is_ok());

    let mut over_cap = at_cap;
    over_cap.clients = clients_with_total_bytes(16_384 - 27);
    assert_eq!(
        aggregate_remote_usage_v1(&[], &over_cap),
        Err(RemoteUsageError::LimitExceeded)
    );
}

#[test]
fn remote_report_timestamp_max_millisecond_is_supported() {
    let max_millisecond = Timestamp::MAX.as_millisecond();
    let max_date = TimeZone::UTC.to_datetime(Timestamp::MAX).date();
    let end_date = max_date.tomorrow().expect("max timestamp has a date end");
    let max_query = RemoteUsageQueryV1 {
        clients: Vec::new(),
        start_date: max_date.to_string(),
        end_date_exclusive: end_date.to_string(),
        timezone: "UTC".to_owned(),
        tzdb_revision: "2026c".to_owned(),
    };
    assert!(aggregate_remote_usage_v1(
        &[message("alpha", "model", "provider", max_millisecond)],
        &max_query
    )
    .is_ok());
    assert_eq!(
        aggregate_remote_usage_v1(
            &[message("alpha", "model", "provider", max_millisecond + 1)],
            &max_query
        ),
        Err(RemoteUsageError::InvalidTimestamp)
    );
}

fn messages_with_label_bytes(last_agent_bytes: usize) -> Vec<UnifiedMessage> {
    let mut rows = vec![
        message(
            &"c".repeat(255),
            &"m".repeat(255),
            &"p".repeat(255),
            1_730_613_600_000,
        );
        8_224
    ];
    for row in &mut rows {
        row.agent = Some("a".repeat(255));
    }
    let mut last = message("z", "", "", 1_730_613_600_000);
    last.agent = Some("q".repeat(last_agent_bytes - 1));
    rows.push(last);
    rows
}

#[test]
fn remote_report_authoritative_label_occurrences_enforce_cap() {
    assert!(aggregate_remote_usage_v1(&messages_with_label_bytes(127), &query("UTC")).is_ok());
    assert!(aggregate_remote_usage_v1(&messages_with_label_bytes(128), &query("UTC")).is_ok());
    assert_eq!(
        aggregate_remote_usage_v1(&messages_with_label_bytes(129), &query("UTC")),
        Err(RemoteUsageError::LimitExceeded)
    );
}

#[test]
fn remote_report_accepts_nfc_unicode_model_without_panicking() {
    let bundle = aggregate_remote_usage_v1(
        &[message("alpha", "aébbbbbbb", "provider", 1_730_613_600_000)],
        &query("UTC"),
    )
    .expect("valid NFC model");

    assert_eq!(bundle.models[0].model.as_deref(), Some("aébbbbbbb"));
}

#[test]
fn remote_report_rejects_oversized_raw_agent_before_normalization() {
    let mut oversized = message("alpha", "model", "provider", 1_730_613_600_000);
    oversized.agent = Some("\u{200b}".repeat(86));

    assert_eq!(
        aggregate_remote_usage_v1(&[oversized], &query("UTC")),
        Err(RemoteUsageError::InvalidText)
    );
}

#[test]
fn remote_report_keeps_date_only_model_identifier() {
    let bundle = aggregate_remote_usage_v1(
        &[message("alpha", "-20260101", "provider", 1_730_613_600_000)],
        &query("UTC"),
    )
    .expect("date-only model remains nonempty");

    assert_eq!(bundle.models[0].model.as_deref(), Some("-20260101"));
}

#[test]
fn remote_report_rejects_oversized_raw_model_and_provider_before_trimming() {
    let oversized_model = message(
        "alpha",
        &format!("{}model", " ".repeat(251)),
        "provider",
        1_730_613_600_000,
    );
    assert_eq!(
        aggregate_remote_usage_v1(&[oversized_model], &query("UTC")),
        Err(RemoteUsageError::InvalidText)
    );

    let oversized_provider = message(
        "alpha",
        "model",
        &format!("{}provider", " ".repeat(248)),
        1_730_613_600_000,
    );
    assert_eq!(
        aggregate_remote_usage_v1(&[oversized_provider], &query("UTC")),
        Err(RemoteUsageError::InvalidText)
    );
}
