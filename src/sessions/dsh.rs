//! DeepSeek Harness session parser.
//!
//! DeepSeek Harness (DSH) persists per-project session snapshots under
//! `~/.dsh/storages/session_projcache/sessions/session-*.json`. Each file
//! holds one session's `record.rows` state. Usage arrives in two shapes:
//!
//! - **Per-step rows** (primary): `record.rows.contextTimeline.val.requests[]`,
//!   one entry per LLM step with cumulative prompt context (`prompt`), the
//!   cache-hit slice of it (`cacheRead`, absent on older snapshots), and fresh
//!   output (`output`). One entry maps to one message.
//! - **Session totals** (fallback): `record.rows.tokenUsage.val.totals`
//!   (`uncachedInputTokens`, `cacheReadTokens`, `outputTokens`,
//!   `cacheWriteTokens`). Some snapshots (notably `isSeeded` sessions whose
//!   timeline was never written) carry a null `contextTimeline.val` while the
//!   totals are large. Those map to a single aggregate message anchored on the
//!   snapshot file's mtime, deduped by `dsh:<session>:totals`.
//!
//! Token mapping (verified against `record.rows.tokenUsage.val.totals` on a
//! 7-file real corpus: per-file `sum(prompt - cacheRead)`, `sum(cacheRead)`
//! and `sum(output)` match `uncachedInputTokens`, `cacheReadTokens` and
//! `outputTokens` exactly):
//!
//! - `input = max(prompt - cacheRead, 0)` (the uncached slice)
//! - `cache_read = cacheRead`
//! - `output = output`
//! - `cache_write = 0` (Harness reports no write bucket; every observed
//!   `totals.cacheWriteTokens` is 0)
//! - `reasoning = 0` (no reasoning split is reported; `output` is taken whole
//!   so nothing is double-counted)
//!
//! Identity: `provider`/`model` come from
//! `record.rows.modelSelection.val.lastUsed`, falling back to the timeline's
//! `provider`/`model`/`lastModel` (unknown when the timeline is absent). The
//! workspace is the session's `record.identity.cwd`. Cost is left at 0.0 for
//! the pricing pipeline: free-tier `-free` models resolve through
//! `custom-pricing.json` shadow prices, paid rows through the built-in
//! catalogs.

use super::{
    normalize_workspace_key, workspace_label_from_key, UnifiedMessage,
};
use super::utils::file_modified_timestamp_ms;
use crate::{pricing, provider_identity, TokenBreakdown};
use serde::Deserialize;
use std::collections::HashSet;
use std::path::Path;

#[derive(Debug, Deserialize)]
struct DshFile {
    #[serde(default)]
    record: DshRecord,
}

#[derive(Debug, Default, Deserialize)]
struct DshRecord {
    #[serde(default)]
    identity: DshIdentity,
    #[serde(default)]
    rows: DshRows,
}

#[derive(Debug, Default, Deserialize)]
struct DshIdentity {
    #[serde(default)]
    cwd: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct DshRows {
    #[serde(default, rename = "contextTimeline")]
    context_timeline: RowEnvelope<DshTimeline>,
    #[serde(default, rename = "modelSelection")]
    model_selection: RowEnvelope<DshModelSelection>,
    #[serde(default, rename = "tokenUsage")]
    token_usage: RowEnvelope<DshTokenUsage>,
}

#[derive(Debug, Default, Deserialize)]
struct RowEnvelope<T> {
    #[serde(default)]
    val: Option<T>,
}

#[derive(Debug, Default, Deserialize)]
struct DshTimeline {
    #[serde(default)]
    requests: Vec<DshRequest>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default, rename = "lastModel")]
    last_model: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct DshRequest {
    #[serde(default)]
    time: i64,
    #[serde(default)]
    seq: i64,
    #[serde(default)]
    turn: i64,
    #[serde(default)]
    step: i64,
    #[serde(default)]
    prompt: i64,
    #[serde(default, rename = "cacheRead")]
    cache_read: Option<i64>,
    #[serde(default)]
    output: i64,
}

#[derive(Debug, Default, Deserialize)]
struct DshModelSelection {
    #[serde(default, rename = "lastUsed")]
    last_used: Option<DshLastUsed>,
}

#[derive(Debug, Default, Deserialize)]
struct DshTokenUsage {
    #[serde(default)]
    totals: Option<DshTotals>,
}

#[derive(Debug, Deserialize)]
struct DshTotals {
    #[serde(default, rename = "uncachedInputTokens")]
    uncached_input: i64,
    #[serde(default, rename = "cacheReadTokens")]
    cache_read: i64,
    #[serde(default, rename = "outputTokens")]
    output: i64,
    #[serde(default, rename = "cacheWriteTokens")]
    cache_write: i64,
}

impl Default for DshTotals {
    fn default() -> Self {
        Self {
            uncached_input: 0,
            cache_read: 0,
            output: 0,
            cache_write: 0,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct DshLastUsed {
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

/// Parse one DSH `session-*.json` projcache snapshot.
///
/// Missing/unreadable files and malformed JSON yield no messages. A snapshot
/// with per-step `requests[]` maps one message per row. A snapshot whose
/// timeline is absent but whose `tokenUsage.val.totals` is non-zero maps a
/// single aggregate message anchored on the file's mtime. Zero-usage rows
/// (and zero totals) are skipped.
pub fn parse_dsh_file(path: &Path) -> Vec<UnifiedMessage> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(_) => return Vec::new(),
    };
    let file: DshFile = match serde_json::from_str(&content) {
        Ok(file) => file,
        Err(_) => return Vec::new(),
    };

    let last_used = file.record.rows.model_selection.val.and_then(|s| s.last_used);
    let timeline = file.record.rows.context_timeline.val;

    let model_raw = last_used
        .as_ref()
        .and_then(|u| u.model.as_deref())
        .or(timeline.as_ref().and_then(|t| t.model.as_deref()))
        .or(timeline.as_ref().and_then(|t| t.last_model.as_deref()))
        .unwrap_or("unknown");
    let model_id = pricing::aliases::resolve_alias(model_raw)
        .unwrap_or(model_raw)
        .to_string();
    let provider_raw = last_used
        .as_ref()
        .and_then(|u| u.provider.as_deref())
        .or(timeline.as_ref().and_then(|t| t.provider.as_deref()))
        .unwrap_or("unknown");
    let provider_id = provider_identity::canonical_provider(provider_raw)
        .unwrap_or_else(|| provider_raw.to_string());

    let session_id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("unknown")
        .to_string();

    let (workspace_key, workspace_label) = file
        .record
        .identity
        .cwd
        .as_deref()
        .and_then(normalize_workspace_key)
        .map(|key| {
            let label = workspace_label_from_key(&key);
            (Some(key), label)
        })
        .unwrap_or((None, None));

    let mut messages = Vec::new();
    let mut seen = HashSet::new();
    let mut prev_turn: Option<i64> = None;
    if let Some(timeline) = timeline.as_ref().filter(|t| !t.requests.is_empty()) {
        for req in &timeline.requests {
            if req.time <= 0 {
                continue;
            }
            let cache_read = req.cache_read.unwrap_or(0).max(0);
            let prompt = req.prompt.max(0);
            let output = req.output.max(0);
            let input = prompt.saturating_sub(cache_read);
            if input == 0 && cache_read == 0 && output == 0 {
                continue;
            }
            let tokens = TokenBreakdown {
                input,
                output,
                cache_read,
                cache_write: 0,
                cache_write_1h: 0,
                reasoning: 0,
            };
            let dedup_key = format!(
                "dsh:{}:{}:{}:{}:{}:{}:{}:{}",
                session_id, req.seq, req.turn, req.step, model_id, input, output, cache_read,
            );
            if !seen.insert(dedup_key.clone()) {
                continue;
            }
            let mut message = UnifiedMessage::new_with_dedup(
                "dsh",
                model_id.clone(),
                provider_id.clone(),
                &session_id,
                req.time,
                tokens,
                0.0,
                Some(dedup_key),
            );
            if prev_turn.is_none_or(|prev| prev != req.turn) {
                message.is_turn_start = true;
            }
            prev_turn = Some(req.turn);
            if workspace_key.is_some() || workspace_label.is_some() {
                message.set_workspace(workspace_key.clone(), workspace_label.clone());
            }
            messages.push(message);
        }
    }

    if !messages.is_empty() {
        return messages;
    }

    // Fallback: timeline absent (or no rows), but the session totals are
    // non-zero. Emit one aggregate message anchored on the file's mtime so
    // the usage still reaches the reports instead of vanishing. The totals
    // buckets are already split the way TokenBreakdown wants them.
    let Some(totals) = file.record.rows.token_usage.val.and_then(|usage| usage.totals) else {
        return Vec::new();
    };
    let input = totals.uncached_input.max(0);
    let cache_read = totals.cache_read.max(0);
    let output = totals.output.max(0);
    // cache_write is authoritative-zero on every observed file; a future
    // nonzero write still must not leak into the input bucket.
    let cache_write = totals.cache_write.max(0);
    if input == 0 && cache_read == 0 && output == 0 && cache_write == 0 {
        return Vec::new();
    }
    let timestamp = file_modified_timestamp_ms(path);
    if timestamp <= 0 {
        return Vec::new();
    }
    let tokens = TokenBreakdown {
        input,
        output,
        cache_read,
        cache_write,
        cache_write_1h: 0,
        reasoning: 0,
    };
    let dedup_key = format!("dsh:{session_id}:totals");
    let mut message = UnifiedMessage::new_with_dedup(
        "dsh",
        model_id,
        provider_id,
        &session_id,
        timestamp,
        tokens,
        0.0,
        Some(dedup_key),
    );
    message.is_turn_start = true;
    if workspace_key.is_some() || workspace_label.is_some() {
        message.set_workspace(workspace_key, workspace_label);
    }
    messages.push(message);

    messages
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn write_session(dir: &TempDir, name: &str, content: &str) -> std::path::PathBuf {
        let path = dir.path().join(name);
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(content.as_bytes()).unwrap();
        file.flush().unwrap();
        path
    }

    fn minimal_file(
        cwd: &str,
        provider: &str,
        model: &str,
        requests: &str,
    ) -> String {
        let requests_val: Vec<serde_json::Value> =
            serde_json::from_str(&format!("[{requests}]")).unwrap();
        serde_json::json!({
            "version": 7,
            "record": {
                "identity": {
                    "formatVersion": 4,
                    "createdAt": 1791194458614i64,
                    "cwd": cwd,
                    "isSeeded": false
                },
                "rows": {
                    "contextTimeline": {
                        "ver": 24,
                        "seq": 280,
                        "val": {
                            "model": model,
                            "provider": provider,
                            "requests": requests_val
                        }
                    },
                    "modelSelection": {
                        "ver": 2,
                        "seq": 280,
                        "val": {
                            "lastUsed": {
                                "provider": provider,
                                "model": model
                            }
                        }
                    }
                }
            }
        })
        .to_string()
    }

    #[test]
    fn parses_requests_with_prompt_minus_cache_mapping() {
        let dir = TempDir::new().unwrap();
        let requests = concat!(
            r#"{"time":1791194523506,"seq":17,"system":1505,"tools":8372,"user":11,"inject":132,"skill":622,"assistant":0,"tool":0,"total":10642,"turn":1,"step":1,"prompt":10741,"cacheRead":113,"output":387},"#,
            r#"{"time":1791194578634,"seq":25,"system":1505,"tools":8372,"user":26,"inject":132,"skill":622,"assistant":36,"tool":0,"total":10693,"turn":2,"step":1,"prompt":10811,"cacheRead":113,"output":412}"#,
        );
        let content = minimal_file(
            "C:\\Users\\alice\\repo",
            "opencode2dsh",
            "muse-spark-1.3-contributor-free",
            requests,
        );
        let path = write_session(&dir, "session-abc.json", &content);

        let messages = parse_dsh_file(&path);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].client, "dsh");
        assert_eq!(messages[0].model_id, "muse-spark-1.3-contributor-free");
        assert_eq!(messages[0].provider_id, "opencode2dsh");
        assert_eq!(messages[0].session_id, "session-abc");
        // prompt - cacheRead = uncached input.
        assert_eq!(messages[0].tokens.input, 10741 - 113);
        assert_eq!(messages[0].tokens.cache_read, 113);
        assert_eq!(messages[0].tokens.output, 387);
        assert_eq!(messages[0].tokens.cache_write, 0);
        assert_eq!(messages[0].tokens.reasoning, 0);
        assert_eq!(messages[0].timestamp, 1791194523506);
        assert_eq!(messages[0].cost, 0.0);
        assert!(messages[0].dedup_key.as_deref().unwrap().starts_with("dsh:session-abc:17:"));
        // First step of each turn starts a turn.
        assert!(messages[0].is_turn_start);
        assert!(messages[1].is_turn_start);
        // Workspace comes from identity.cwd.
        assert_eq!(
            messages[0].workspace_key.as_deref(),
            Some("C:/Users/alice/repo")
        );
        assert_eq!(messages[0].workspace_label.as_deref(), Some("repo"));
    }

    #[test]
    fn second_step_of_same_turn_is_not_turn_start() {
        let dir = TempDir::new().unwrap();
        let content = minimal_file(
            "C:\\Users\\alice\\repo",
            "openrouter",
            "stealth/ox-alpha",
            r#"{"time":1791194523506,"seq":17,"turn":1,"step":1,"prompt":100,"cacheRead":10,"output":5},{"time":1791194524506,"seq":18,"turn":1,"step":2,"prompt":200,"cacheRead":20,"output":6}"#,
        );
        let path = write_session(&dir, "session-turn.json", &content);

        let messages = parse_dsh_file(&path);
        assert_eq!(messages.len(), 2);
        assert!(messages[0].is_turn_start);
        assert!(!messages[1].is_turn_start);
    }

    #[test]
    fn missing_cache_read_defaults_to_zero() {
        // Older snapshots (e.g. 2026-08) carry no cacheRead key at all.
        let dir = TempDir::new().unwrap();
        let content = minimal_file(
            "C:\\Users\\alice\\repo",
            "openrouter",
            "stealth/ox-alpha",
            r#"{"time":1787497454467,"seq":15,"system":1632,"tools":6769,"user":9,"inject":131,"skill":0,"assistant":0,"tool":0,"total":8541,"turn":1,"step":1,"prompt":7841,"output":178}"#,
        );
        let path = write_session(&dir, "session-old.json", &content);

        let messages = parse_dsh_file(&path);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].tokens.input, 7841);
        assert_eq!(messages[0].tokens.cache_read, 0);
        assert_eq!(messages[0].tokens.output, 178);
    }

    #[test]
    fn skips_zero_usage_heartbeat_rows() {
        let dir = TempDir::new().unwrap();
        let content = minimal_file(
            "C:\\Users\\alice\\repo",
            "openrouter",
            "stealth/ox-alpha",
            r#"{"time":1791194523506,"seq":17,"turn":1,"step":1,"prompt":0,"cacheRead":0,"output":0},{"time":1791194524506,"seq":18,"turn":1,"step":2,"prompt":100,"cacheRead":10,"output":5}"#,
        );
        let path = write_session(&dir, "session-heartbeat.json", &content);

        let messages = parse_dsh_file(&path);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].timestamp, 1791194524506);
    }

    #[test]
    fn falls_back_to_timeline_model_when_last_used_missing() {
        let dir = TempDir::new().unwrap();
        let content = r#"{"version":7,"record":{"identity":{"formatVersion":4,"cwd":"C:\\Users\\alice\\repo"},"rows":{"contextTimeline":{"ver":24,"seq":1,"val":{"model":"deepseek-flash","provider":"deepseek-account","requests":[{"time":1790935688620,"seq":18,"turn":1,"step":1,"prompt":8157,"cacheRead":0,"output":817}]}},"modelSelection":{"ver":2,"seq":1,"val":{"lastUsed":null}}}}}"#;
        let path = write_session(&dir, "session-fallback.json", content);

        let messages = parse_dsh_file(&path);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].model_id, "deepseek-flash");
        // Provider ids are canonicalized (`-` -> `_`), same as every other lane.
        assert_eq!(messages[0].provider_id, "deepseek_account");
    }

    #[test]
    fn totals_fallback_when_timeline_absent() {
        // Seeded sessions whose timeline was never written: totals are large,
        // `contextTimeline` is absent entirely. One aggregate message anchored
        // on the file's mtime, deduped by `dsh:<session>:totals`.
        let dir = TempDir::new().unwrap();
        let content = r#"{"version":7,"record":{"identity":{"formatVersion":4,"createdAt":1791349276317,"cwd":"C:\\chatbot","isSeeded":true},"rows":{"tokenUsage":{"ver":1,"seq":100,"val":{"totals":{"uncachedInputTokens":1243037,"outputTokens":596044,"cacheReadTokens":376403209,"cacheWriteTokens":0}}},"modelSelection":{"ver":2,"seq":100,"val":{"lastUsed":{"provider":"opencode2dsh","model":"muse-spark-1.3-contributor-free"}}}}}}"#;
        let path = write_session(&dir, "session-e7ee0d32.json", content);

        let messages = parse_dsh_file(&path);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].client, "dsh");
        assert_eq!(messages[0].model_id, "muse-spark-1.3-contributor-free");
        assert_eq!(messages[0].provider_id, "opencode2dsh");
        assert_eq!(messages[0].session_id, "session-e7ee0d32");
        assert_eq!(messages[0].tokens.input, 1243037);
        assert_eq!(messages[0].tokens.cache_read, 376403209);
        assert_eq!(messages[0].tokens.output, 596044);
        assert_eq!(messages[0].tokens.cache_write, 0);
        assert_eq!(messages[0].tokens.reasoning, 0);
        assert_eq!(messages[0].cost, 0.0);
        assert!(messages[0].is_turn_start);
        assert_eq!(
            messages[0].dedup_key.as_deref(),
            Some("dsh:session-e7ee0d32:totals")
        );
        // Anchored on the snapshot file's mtime, not on any request.
        assert!(messages[0].timestamp > 0);
        assert!(!messages[0].date.is_empty());
        assert_eq!(
            messages[0].workspace_key.as_deref(),
            Some("C:/chatbot")
        );
        assert_eq!(messages[0].workspace_label.as_deref(), Some("chatbot"));
    }

    #[test]
    fn timeline_rows_win_over_totals_no_double_count() {
        // Both present: the per-step rows are authoritative, totals must not
        // add a second aggregate message.
        let dir = TempDir::new().unwrap();
        let content = serde_json::json!({
            "version": 7,
            "record": {
                "identity": {"formatVersion": 4, "cwd": "C:\\chatbot"},
                "rows": {
                    "contextTimeline": {"ver": 24, "seq": 2, "val": {
                        "model": "muse-spark-1.3-contributor-free",
                        "provider": "opencode2dsh",
                        "requests": [
                            {"time": 1791349276317i64, "seq": 1, "turn": 1, "step": 1,
                             "prompt": 1000, "cacheRead": 100, "output": 50}
                        ]
                    }},
                    "modelSelection": {"ver": 2, "seq": 2, "val": {
                        "lastUsed": {"provider": "opencode2dsh",
                                     "model": "muse-spark-1.3-contributor-free"}
                    }},
                    "tokenUsage": {"ver": 1, "seq": 2, "val": {
                        "totals": {"uncachedInputTokens": 999999,
                                   "outputTokens": 888888,
                                   "cacheReadTokens": 777777,
                                   "cacheWriteTokens": 0}
                    }}
                }
            }
        })
        .to_string();
        let path = write_session(&dir, "session-both.json", &content);

        let messages = parse_dsh_file(&path);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].tokens.input, 900);
        assert_eq!(messages[0].tokens.cache_read, 100);
        assert_eq!(messages[0].tokens.output, 50);
        assert!(messages[0]
            .dedup_key
            .as_deref()
            .unwrap()
            .starts_with("dsh:session-both:1:"));
    }

    #[test]
    fn zero_totals_yield_nothing() {
        let dir = TempDir::new().unwrap();
        let content = r#"{"version":7,"record":{"identity":{"formatVersion":4,"cwd":"C:\\chatbot"},"rows":{"tokenUsage":{"ver":1,"seq":1,"val":{"totals":{"uncachedInputTokens":0,"outputTokens":0,"cacheReadTokens":0,"cacheWriteTokens":0}}},"modelSelection":{"ver":2,"seq":1,"val":{"lastUsed":{"provider":"opencode2dsh","model":"muse-spark-1.3-contributor-free"}}}}}}"#;
        let path = write_session(&dir, "session-zero.json", content);

        assert!(parse_dsh_file(&path).is_empty());
    }

    #[test]
    fn malformed_and_empty_inputs_yield_nothing() {
        let dir = TempDir::new().unwrap();
        let bad = write_session(&dir, "session-bad.json", "not json at all");
        assert!(parse_dsh_file(&bad).is_empty());

        let no_requests = write_session(
            &dir,
            "session-empty.json",
            r#"{"version":7,"record":{"identity":{"cwd":"C:\\x"},"rows":{"contextTimeline":{"ver":24,"seq":1,"val":{"model":"m","provider":"p","requests":[]}},"modelSelection":{"ver":2,"seq":1,"val":{}}}}}"#,
        );
        assert!(parse_dsh_file(&no_requests).is_empty());

        let no_timeline = write_session(
            &dir,
            "session-notimeline.json",
            r#"{"version":7,"record":{"identity":{"cwd":"C:\\x"},"rows":{}}}"#,
        );
        assert!(parse_dsh_file(&no_timeline).is_empty());

        // Absent timeline AND absent totals: nothing to aggregate.
        let no_timeline_no_totals = write_session(
            &dir,
            "session-notimeline-nototals.json",
            r#"{"version":7,"record":{"identity":{"cwd":"C:\\x"},"rows":{"modelSelection":{"ver":2,"seq":1,"val":{"lastUsed":{"provider":"p","model":"m"}}}}}}"#,
        );
        assert!(parse_dsh_file(&no_timeline_no_totals).is_empty());

        assert!(parse_dsh_file(dir.path().join("does-not-exist.json").as_path()).is_empty());
    }
}
