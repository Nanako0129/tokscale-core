//! Mux (coder/mux) session parser
//!
//! Parses session-usage.json files from ~/.mux/sessions/<workspaceId>/session-usage.json

use super::utils::{file_modified_timestamp_ms, read_file_or_none};
use super::UnifiedMessage;
use crate::{provider_identity, TokenBreakdown};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Deserialize)]
pub struct MuxSessionUsage {
    #[allow(dead_code)]
    pub version: Option<u32>,
    #[serde(rename = "byModel")]
    pub by_model: Option<HashMap<String, MuxModelUsage>>,
    #[serde(rename = "lastRequest")]
    pub last_request: Option<MuxLastRequest>,
}

#[derive(Debug, Deserialize)]
pub struct MuxModelUsage {
    pub input: Option<MuxTokenBucket>,
    pub cached: Option<MuxTokenBucket>,
    #[serde(rename = "cacheCreate")]
    pub cache_create: Option<MuxTokenBucket>,
    pub output: Option<MuxTokenBucket>,
    pub reasoning: Option<MuxTokenBucket>,
}

#[derive(Debug, Deserialize)]
pub struct MuxTokenBucket {
    pub tokens: Option<i64>,
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Deserialize)]
pub struct MuxLastRequest {
    #[allow(dead_code)]
    pub model: Option<String>,
    pub timestamp: Option<i64>,
}

fn valid_mux_cost(bucket: Option<&MuxTokenBucket>) -> Option<f64> {
    bucket
        .and_then(|bucket| bucket.cost_usd)
        .filter(|cost| cost.is_finite() && *cost >= 0.0)
}

/// Parse a mux session-usage.json file.
/// Returns one or two UnifiedMessage rows per model entry in byModel when
/// positive-token buckets have mixed cost provenance.
pub fn parse_mux_file(path: &Path) -> Vec<UnifiedMessage> {
    let Some(data) = read_file_or_none(path) else {
        return vec![];
    };

    let usage: MuxSessionUsage = match serde_json::from_slice(&data) {
        Ok(u) => u,
        Err(_) => return vec![],
    };

    let timestamp = usage
        .last_request
        .as_ref()
        .and_then(|lr| lr.timestamp)
        .unwrap_or_else(|| file_modified_timestamp_ms(path));

    let session_id = path
        .parent()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();

    let by_model = match usage.by_model {
        Some(m) => m,
        None => return vec![],
    };

    by_model
        .into_iter()
        .flat_map(|(model_key, model_usage)| {
            let tokens =
                |b: &Option<MuxTokenBucket>| b.as_ref().and_then(|b| b.tokens).unwrap_or(0).max(0);
            let buckets = [
                (
                    "input",
                    tokens(&model_usage.input),
                    valid_mux_cost(model_usage.input.as_ref()),
                ),
                (
                    "cached",
                    tokens(&model_usage.cached),
                    valid_mux_cost(model_usage.cached.as_ref()),
                ),
                (
                    "cache_create",
                    tokens(&model_usage.cache_create),
                    valid_mux_cost(model_usage.cache_create.as_ref()),
                ),
                (
                    "output",
                    tokens(&model_usage.output),
                    valid_mux_cost(model_usage.output.as_ref()),
                ),
                (
                    "reasoning",
                    tokens(&model_usage.reasoning),
                    valid_mux_cost(model_usage.reasoning.as_ref()),
                ),
            ];
            let mut known_tokens = TokenBreakdown::default();
            let mut unknown_tokens = TokenBreakdown::default();
            let mut known_cost = 0.0;

            for (bucket, token_count, cost) in buckets {
                if let Some(cost) = cost {
                    known_cost += cost;
                }
                if token_count == 0 {
                    continue;
                }
                let target = if cost.is_some() {
                    &mut known_tokens
                } else {
                    &mut unknown_tokens
                };
                match bucket {
                    "input" => target.input = token_count,
                    "cached" => target.cache_read = token_count,
                    "cache_create" => target.cache_write = token_count,
                    "output" => target.output = token_count,
                    "reasoning" => target.reasoning = token_count,
                    _ => unreachable!("all Mux token buckets are listed above"),
                }
            }

            let known_total = known_tokens.total();
            let unknown_total = unknown_tokens.total();
            if known_total == 0 && unknown_total == 0 {
                return Vec::new();
            }

            // Dedup key scoped to (workspace session, model): stable across
            // re-parses (no HashMap-iteration index) and unique per workspace,
            // so two workspaces reporting the same model are not collided into
            // one. The provenance suffix keeps a mixed known/unknown split from
            // dropping one row at the per-client dedup gate.
            let dedup_base = format!("mux:{session_id}:{model_key}");

            // Strip "provider:" prefix for model ID (e.g., "anthropic:claude-opus-4-6" -> "claude-opus-4-6")
            let (provider, model_id) = if model_key.contains(':') {
                let mut parts = model_key.splitn(2, ':');
                let p = parts.next().unwrap_or("").to_string();
                let m = parts.next().unwrap_or(&model_key).to_string();
                (p, m)
            } else {
                (String::new(), model_key)
            };
            let provider = provider_identity::canonical_provider(&provider).unwrap_or(provider);

            let make_message = |tokens: TokenBreakdown,
                                cost: f64,
                                suffix: &str,
                                message_count: i32,
                                provider_reported: bool| {
                let mut message = UnifiedMessage::new_with_dedup(
                    "mux",
                    model_id.clone(),
                    provider.clone(),
                    session_id.clone(),
                    timestamp,
                    tokens,
                    cost,
                    Some(format!("{dedup_base}:{suffix}")),
                );
                message.message_count = message_count;
                if provider_reported {
                    message.mark_provider_reported_cost();
                }
                message
            };

            match (known_total > 0 || known_cost > 0.0, unknown_total > 0) {
                (true, true) => vec![
                    make_message(known_tokens, known_cost, "known", 1, true),
                    make_message(unknown_tokens, 0.0, "unknown", 0, false),
                ],
                (true, false) => vec![make_message(known_tokens, known_cost, "known", 1, true)],
                (false, true) => vec![make_message(unknown_tokens, 0.0, "unknown", 1, false)],
                (false, false) => Vec::new(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_temp_json(content: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn test_parse_valid_session_usage() {
        let json = r#"{
            "version": 1,
            "byModel": {
                "anthropic:claude-opus-4-6": {
                    "input": { "tokens": 100, "cost_usd": 0.01 },
                    "cached": { "tokens": 5000, "cost_usd": 0.05 },
                    "cacheCreate": { "tokens": 200, "cost_usd": 0.02 },
                    "output": { "tokens": 300, "cost_usd": 0.03 },
                    "reasoning": { "tokens": 0, "cost_usd": 0 }
                },
                "openai:gpt-4o": {
                    "input": { "tokens": 50, "cost_usd": 0.005 },
                    "cached": { "tokens": 0, "cost_usd": 0 },
                    "cacheCreate": { "tokens": 0, "cost_usd": 0 },
                    "output": { "tokens": 150, "cost_usd": 0.015 },
                    "reasoning": { "tokens": 0, "cost_usd": 0 }
                }
            },
            "lastRequest": {
                "model": "anthropic:claude-opus-4-6",
                "timestamp": 1700000000000
            }
        }"#;
        let f = write_temp_json(json);
        let msgs = parse_mux_file(f.path());
        assert_eq!(msgs.len(), 2);

        // Find the claude message
        let claude = msgs
            .iter()
            .find(|m| m.model_id == "claude-opus-4-6")
            .unwrap();
        assert_eq!(claude.client, "mux");
        assert_eq!(claude.provider_id, "anthropic");
        assert_eq!(claude.tokens.input, 100);
        assert_eq!(claude.tokens.cache_read, 5000);
        assert_eq!(claude.tokens.cache_write, 200);
        assert_eq!(claude.tokens.output, 300);
        assert_eq!(claude.tokens.reasoning, 0);
        assert_eq!(claude.timestamp, 1700000000000);
        assert!(claude.has_authoritative_cost());

        let gpt = msgs.iter().find(|m| m.model_id == "gpt-4o").unwrap();
        assert_eq!(gpt.provider_id, "openai");
        assert_eq!(gpt.tokens.input, 50);
        assert_eq!(gpt.tokens.output, 150);
        assert!(gpt.has_authoritative_cost());
    }

    #[test]
    fn test_parse_empty_by_model() {
        let json = r#"{ "version": 1, "byModel": {} }"#;
        let f = write_temp_json(json);
        let msgs = parse_mux_file(f.path());
        assert!(msgs.is_empty());
    }

    #[test]
    fn test_parse_missing_by_model() {
        let json = r#"{ "version": 1 }"#;
        let f = write_temp_json(json);
        let msgs = parse_mux_file(f.path());
        assert!(msgs.is_empty());
    }

    #[test]
    fn test_zero_token_entries_filtered() {
        let json = r#"{
            "version": 1,
            "byModel": {
                "anthropic:claude-opus-4-6": {
                    "input": { "tokens": 0, "cost_usd": 0 },
                    "cached": { "tokens": 0, "cost_usd": 0 },
                    "cacheCreate": { "tokens": 0, "cost_usd": 0 },
                    "output": { "tokens": 0, "cost_usd": 0 },
                    "reasoning": { "tokens": 0, "cost_usd": 0 }
                }
            },
            "lastRequest": { "model": "anthropic:claude-opus-4-6", "timestamp": 1700000000000 }
        }"#;
        let f = write_temp_json(json);
        let msgs = parse_mux_file(f.path());
        assert!(msgs.is_empty());
    }

    #[test]
    fn test_zero_token_missing_cost_does_not_downgrade_coverage() {
        let json = r#"{
            "version": 1,
            "byModel": {
                "anthropic:claude-opus-4-6": {
                    "input": { "tokens": 100, "cost_usd": 0 },
                    "output": { "tokens": 0 }
                }
            },
            "lastRequest": { "timestamp": 1700000000000 }
        }"#;
        let f = write_temp_json(json);
        let msgs = parse_mux_file(f.path());
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].cost, 0.0);
        assert!(msgs[0].has_authoritative_cost());
    }

    #[test]
    fn test_model_without_provider_prefix() {
        let json = r#"{
            "version": 1,
            "byModel": {
                "claude-opus-4-6": {
                    "input": { "tokens": 100 },
                    "output": { "tokens": 200 }
                }
            },
            "lastRequest": { "timestamp": 1700000000000 }
        }"#;
        let f = write_temp_json(json);
        let msgs = parse_mux_file(f.path());
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].model_id, "claude-opus-4-6");
        assert_eq!(msgs[0].provider_id, "");
        assert_eq!(msgs[0].cost, 0.0);
        assert_eq!(msgs[0].message_count, 1);
        assert!(!msgs[0].has_authoritative_cost());
        assert!(msgs[0]
            .dedup_key
            .as_deref()
            .is_some_and(|key| key.ends_with(":unknown")));
    }

    #[test]
    fn test_mixed_bucket_costs_split_provenance_without_double_counting_messages() {
        let json = r#"{
            "version": 1,
            "byModel": {
                "anthropic:claude-opus-4-6": {
                    "input": { "tokens": 100, "cost_usd": 0.01 },
                    "cached": { "tokens": 200, "cost_usd": 0.02 },
                    "cacheCreate": { "tokens": 50 },
                    "output": { "tokens": 300, "cost_usd": 0.03 },
                    "reasoning": { "tokens": 25 }
                }
            },
            "lastRequest": { "timestamp": 1700000000000 }
        }"#;
        let f = write_temp_json(json);
        let msgs = parse_mux_file(f.path());
        assert_eq!(msgs.len(), 2);

        let known = msgs
            .iter()
            .find(|message| message.dedup_key.as_deref().unwrap().ends_with(":known"))
            .unwrap();
        let unknown = msgs
            .iter()
            .find(|message| message.dedup_key.as_deref().unwrap().ends_with(":unknown"))
            .unwrap();

        assert_eq!(known.tokens.input, 100);
        assert_eq!(known.tokens.cache_read, 200);
        assert_eq!(known.tokens.output, 300);
        assert_eq!(known.tokens.cache_write, 0);
        assert_eq!(known.tokens.reasoning, 0);
        assert!((known.cost - 0.06).abs() < 1e-12);
        assert!(known.has_authoritative_cost());
        assert_eq!(known.message_count, 1);

        assert_eq!(unknown.tokens.cache_write, 50);
        assert_eq!(unknown.tokens.reasoning, 25);
        assert_eq!(unknown.tokens.input, 0);
        assert_eq!(unknown.tokens.output, 0);
        assert_eq!(unknown.cost, 0.0);
        assert!(!unknown.has_authoritative_cost());
        assert_eq!(unknown.message_count, 0);
        assert_ne!(known.dedup_key, unknown.dedup_key);

        assert_eq!(known.tokens.total() + unknown.tokens.total(), 675);
        assert_eq!(known.message_count + unknown.message_count, 1);
    }

    #[test]
    fn test_zero_token_bucket_cost_is_preserved_with_unknown_tokens() {
        let json = r#"{
            "version": 1,
            "byModel": {
                "anthropic:claude-opus-4-6": {
                    "input": { "tokens": 0, "cost_usd": 0.25 },
                    "output": { "tokens": 10 }
                }
            },
            "lastRequest": { "timestamp": 1700000000000 }
        }"#;
        let f = write_temp_json(json);
        let msgs = parse_mux_file(f.path());
        assert_eq!(msgs.len(), 2);

        let known = msgs
            .iter()
            .find(|message| message.has_authoritative_cost())
            .unwrap();
        let unknown = msgs
            .iter()
            .find(|message| !message.has_authoritative_cost())
            .unwrap();
        assert_eq!(known.tokens.total(), 0);
        assert_eq!(known.cost, 0.25);
        assert_eq!(known.message_count, 1);
        assert_eq!(unknown.tokens.output, 10);
        assert_eq!(unknown.message_count, 0);
    }

    #[test]
    fn test_invalid_json() {
        let f = write_temp_json("not json at all");
        let msgs = parse_mux_file(f.path());
        assert!(msgs.is_empty());
    }

    #[test]
    fn test_nonexistent_file() {
        let msgs = parse_mux_file(Path::new("/nonexistent/path/session-usage.json"));
        assert!(msgs.is_empty());
    }

    #[test]
    fn test_negative_tokens_clamped() {
        let json = r#"{
            "version": 1,
            "byModel": {
                "anthropic:claude-opus-4-6": {
                    "input": { "tokens": -50, "cost_usd": 0.01 },
                    "output": { "tokens": 100, "cost_usd": 0.02 }
                }
            },
            "lastRequest": { "timestamp": 1700000000000 }
        }"#;
        let f = write_temp_json(json);
        let msgs = parse_mux_file(f.path());
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].tokens.input, 0);
        assert_eq!(msgs[0].tokens.output, 100);
    }

    #[test]
    fn test_source_cost_summed() {
        let json = r#"{
            "version": 1,
            "byModel": {
                "anthropic:claude-opus-4-6": {
                    "input": { "tokens": 100, "cost_usd": 0.01 },
                    "cached": { "tokens": 200, "cost_usd": 0.02 },
                    "cacheCreate": { "tokens": 50, "cost_usd": 0.005 },
                    "output": { "tokens": 300, "cost_usd": 0.03 },
                    "reasoning": { "tokens": 0, "cost_usd": 0 }
                }
            },
            "lastRequest": { "timestamp": 1700000000000 }
        }"#;
        let f = write_temp_json(json);
        let msgs = parse_mux_file(f.path());
        assert_eq!(msgs.len(), 1);
        let expected_cost = 0.01 + 0.02 + 0.005 + 0.03;
        assert!((msgs[0].cost - expected_cost).abs() < 1e-10);
    }

    #[test]
    fn test_multi_colon_model_key() {
        let json = r#"{
            "version": 1,
            "byModel": {
                "provider:sub:model-name": {
                    "input": { "tokens": 100 },
                    "output": { "tokens": 200 }
                }
            },
            "lastRequest": { "timestamp": 1700000000000 }
        }"#;
        let f = write_temp_json(json);
        let msgs = parse_mux_file(f.path());
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].provider_id, "provider");
        assert_eq!(msgs[0].model_id, "sub:model-name");
    }

    /// Local fix over upstream #760: the dedup key is scoped to the workspace
    /// session (parent dir name), not the HashMap iteration index. Two
    /// workspaces reporting the SAME model must produce DIFFERENT dedup keys so
    /// the streaming per-client seen set keeps both instead of dropping the
    /// second; re-parsing the same workspace reproduces the key so a genuine
    /// duplicate still collapses.
    #[test]
    fn test_mux_dedup_key_scoped_by_workspace_not_hashmap_index() {
        use std::fs;
        let json = r#"{ "version": 1, "byModel": {
            "anthropic:claude-opus-4-6": { "input": { "tokens": 100, "cost_usd": 0.01 }, "output": { "tokens": 50, "cost_usd": 0.005 } }
        }, "lastRequest": { "timestamp": 1700000000000 } }"#;
        let root = tempfile::TempDir::new().unwrap();
        let ws_a = root.path().join("ws_alpha");
        let ws_b = root.path().join("ws_beta");
        fs::create_dir_all(&ws_a).unwrap();
        fs::create_dir_all(&ws_b).unwrap();
        let file_a = ws_a.join("session-usage.json");
        let file_b = ws_b.join("session-usage.json");
        fs::write(&file_a, json).unwrap();
        fs::write(&file_b, json).unwrap();

        let a = parse_mux_file(&file_a);
        let b = parse_mux_file(&file_b);
        assert_eq!(a.len(), 1);
        assert_eq!(b.len(), 1);
        // Same model, different workspaces -> distinct keys (both survive the
        // cross-file dedup gate instead of colliding on "mux:<model>:0").
        assert_eq!(
            a[0].dedup_key.as_deref(),
            Some("mux:ws_alpha:anthropic:claude-opus-4-6:known")
        );
        assert_eq!(
            b[0].dedup_key.as_deref(),
            Some("mux:ws_beta:anthropic:claude-opus-4-6:known")
        );
        assert_ne!(a[0].dedup_key, b[0].dedup_key);
        // Re-parsing the same workspace reproduces the key (true duplicate).
        assert_eq!(parse_mux_file(&file_a)[0].dedup_key, a[0].dedup_key);
    }
}
