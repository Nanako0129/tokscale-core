//! Muse Code session parser
//!
//! Muse Code stores local sessions under
//! `~/.local/share/muse/sessions/YYYY/MM/DD/<session-uuid>/session.jsonl`
//! (the same XDG-style path on all platforms, including Windows).
//! Subagent runs log to their own `subagent/<child-uuid>/session.jsonl`
//! beside the parent transcript.
//!
//! Each line is an event-sourced record. Usage-bearing ones carry
//! `payload_type: "runtime.session"` with a
//! `payload.event.kind: "model_completed"` event holding the model id, a
//! Responses-shaped `usage` object, and the call's `duration_ms`:
//!
//! ```json
//! {"recorded_at": 1789790455896395, "payload_type": "runtime.session",
//!  "payload": {"kind": "run", "event": {"kind": "model_completed",
//!   "usage": {"input_tokens": 26964, "output_tokens": 379,
//!             "cached_tokens": 5105, "cache_write_tokens": 0,
//!             "cache_read_tokens": 5105, "reasoning_tokens": 278},
//!   "duration_ms": 5819, "model": "muse-spark-1.3-contributor"}}}
//! ```
//!
//! Only `model_completed` events are counted. The parent session also logs
//! `workflow_child_lifecycle` usage aggregates for workflow children, but
//! those duplicate the child's own `subagent/*/session.jsonl` transcript,
//! which the scanner picks up separately — counting both would double-bill.

use super::utils::{back_anchor_timestamp, file_modified_timestamp_ms, lossy_lines};
use super::{normalize_workspace_key, workspace_label_from_key, UnifiedMessage};
use crate::{pricing, provider_identity, TokenBreakdown};
use serde_json::Value;
use std::collections::HashSet;
use std::io::BufReader;
use std::path::Path;

const USAGE_EVENT_KIND: &str = "model_completed";
const METADATA_PAYLOAD_TYPE: &str = "runtime.session.metadata";

struct ProviderMetadata {
    record_index: usize,
    model_id: Option<String>,
    provider_id: Option<String>,
}

pub fn parse_muse_file(path: &Path) -> Vec<UnifiedMessage> {
    let path_session_id = session_id_from_path(path);
    let default_timestamp = file_modified_timestamp_ms(path);
    let mut messages = Vec::new();
    let mut seen = HashSet::new();
    // The metadata record lands near the head of the file; it may also be
    // absent (e.g. `--no-session-log` runs that kept no transcript), so
    // workspace stays optional and is applied to every message afterwards.
    let mut workspace: Option<(String, String)> = None;
    let mut provider_metadata = Vec::new();

    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    // `lossy_lines` keeps empty lines, so `index` is the physical line number,
    // and strips a leading BOM, matching upstream's `for_each_json_line`.
    for (index, line) in lossy_lines(BufReader::new(file)).enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Cheap pre-filter only: session transcripts are multi-MB and most
        // lines are tool/text payloads Tokscale does not need. The
        // authoritative decision is made on the parsed fields below.
        if !line.contains(USAGE_EVENT_KIND) && !line.contains(METADATA_PAYLOAD_TYPE) {
            continue;
        }

        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };

        if string_field(&value, "payload_type") == Some(METADATA_PAYLOAD_TYPE) {
            if workspace.is_none() {
                workspace = workspace_from_metadata(&value);
            }
            if let Some(record) = value.pointer("/payload/record") {
                // Native session/start uses the client name as a sentinel
                // when providerId is omitted. It is not an authoritative
                // provider and must not replace model-family inference.
                let provider_id = string_field(record, "provider_id")
                    .filter(|provider| {
                        !provider.eq_ignore_ascii_case("unknown")
                            && !provider.eq_ignore_ascii_case("muse")
                    })
                    .map(str::to_string);
                // Keep every snapshot in the timeline: a missing or
                // nonauthoritative provider clears an earlier override.
                // Dropping it would also let a later explicit snapshot
                // backfill across the initial metadata boundary.
                provider_metadata.push(ProviderMetadata {
                    record_index: index,
                    model_id: string_field(record, "model_id").map(|model| {
                        pricing::aliases::resolve_alias(model)
                            .unwrap_or(model)
                            .to_string()
                    }),
                    provider_id,
                });
            }
            continue;
        }

        let Some(event) = value
            .pointer("/payload/event")
            .filter(|event| string_field(event, "kind") == Some(USAGE_EVENT_KIND))
        else {
            continue;
        };
        let Some(usage) = event.get("usage") else {
            continue;
        };
        let Some(model_raw) = string_field(event, "model") else {
            continue;
        };

        // `recorded_at` is written when the completion record lands, i.e. the
        // call's *end*, not its start. Back-calculate the start anchor the
        // same way junie does — but only from an explicit per-record end
        // timestamp, never from the file-mtime fallback, which would shift
        // the message into the wrong day.
        let explicit_timestamp = number_field(&value, "recorded_at")
            .and_then(recorded_at_to_ms)
            .filter(|timestamp| *timestamp > 0);
        let timestamp = explicit_timestamp.unwrap_or(default_timestamp);
        let duration_ms = number_field(event, "duration_ms").filter(|duration| *duration > 0);
        let start_timestamp = match (explicit_timestamp, duration_ms) {
            (Some(end), Some(duration)) => back_anchor_timestamp(end, duration),
            _ => timestamp,
        };

        let model_id = pricing::aliases::resolve_alias(model_raw)
            .unwrap_or(model_raw)
            .to_string();
        let provider_id = provider_identity::inferred_provider_from_model(&model_id)
            .or_else(|| muse_spark_provider(&model_id))
            .unwrap_or("unknown");
        let tokens = tokens_from_usage(usage);
        if tokens.total() == 0 {
            continue;
        }

        // The record `sequence` is this stream's stable ordinal, so a
        // replayed identical event reproduces the same key and collapses in
        // the in-file `seen` set and the cross-file dedup filter, while two
        // genuinely distinct calls can never share one.
        let ordinal = number_field(&value, "sequence")
            .map(|sequence| sequence.to_string())
            .unwrap_or_else(|| index.to_string());
        let session_id = session_id_from_record(&value).unwrap_or_else(|| path_session_id.clone());
        let dedup_key = format!(
            "muse:{session_id}:{ordinal}:{model_id}:{}:{}:{}:{}:{}",
            tokens.input, tokens.output, tokens.cache_read, tokens.cache_write, tokens.reasoning,
        );
        if !seen.insert(dedup_key.clone()) {
            continue;
        }

        let mut message = UnifiedMessage::new_with_dedup(
            "muse",
            model_id,
            provider_id,
            &session_id,
            start_timestamp,
            tokens,
            0.0,
            Some(dedup_key),
        );
        message.duration_ms = duration_ms;
        messages.push((index, message));
    }

    messages
        .into_iter()
        .map(|(index, mut message)| {
            // Metadata can follow the first completion. Only that initial
            // snapshot applies backwards; later snapshots affect subsequent
            // calls, so a provider switch cannot relabel earlier usage.
            let position = provider_metadata.partition_point(|meta| meta.record_index <= index);
            if let Some(metadata) = provider_metadata.get(position.saturating_sub(1)) {
                // /models can switch providers within a session. A metadata
                // provider belongs to its paired model, not to every model
                // the Muse client happens to run afterwards.
                if metadata
                    .model_id
                    .as_deref()
                    .is_none_or(|model| model == message.model_id)
                {
                    if let Some(provider_id) = &metadata.provider_id {
                        message.provider_id.clone_from(provider_id);
                    }
                }
            }
            if let Some((key, label)) = &workspace {
                message.set_workspace(Some(key.clone()), Some(label.clone()));
            }
            message
        })
        .collect()
}

/// Muse Spark models are Meta's. Upstream #1355 adds `muse-spark` to the
/// shared `inferred_provider_from_model` through `contains_versioned_family`,
/// which this vendor has not adopted (see UPSTREAM.md, "shared provider
/// inference"), so the same rule lives here for Muse alone: `muse-spark` at a
/// non-alphanumeric boundary, followed by a digit or a non-letter.
fn muse_spark_provider(model_id: &str) -> Option<&'static str> {
    let lower = model_id.to_ascii_lowercase();
    lower
        .match_indices("muse-spark")
        .any(|(pos, family)| {
            let before_ok = lower[..pos]
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_alphanumeric());
            let after_ok = lower[pos + family.len()..]
                .chars()
                .next()
                .is_none_or(|c| c.is_ascii_digit() || !c.is_alphanumeric());
            before_ok && after_ok
        })
        .then_some("meta")
}

fn session_id_from_path(path: &Path) -> String {
    path.parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or("unknown")
        .to_string()
}

fn session_id_from_record(value: &Value) -> Option<String> {
    value
        .pointer("/stream/id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

/// Convert a Muse `recorded_at` stamp to Unix milliseconds.
///
/// Current sessions write microseconds (~1.79e15 for 2026); accept
/// milliseconds and seconds too so an older or newer schema unit cannot
/// silently land usage in 1970 or year 58000.
fn recorded_at_to_ms(raw: i64) -> Option<i64> {
    if raw >= 1_000_000_000_000_000 {
        Some(raw / 1000)
    } else if raw >= 1_000_000_000_000 {
        Some(raw)
    } else if raw >= 1_000_000_000 {
        Some(raw.checked_mul(1000)?)
    } else {
        None
    }
}

fn workspace_from_metadata(value: &Value) -> Option<(String, String)> {
    let root = value.pointer("/payload/record/workspace_root")?.as_str()?;
    let key = normalize_workspace_key(root)?;
    let label = workspace_label_from_key(&key)?;
    Some((key, label))
}

fn tokens_from_usage(usage: &Value) -> TokenBreakdown {
    let reported_input = first_number_field(usage, &["input_tokens"]);
    let reported_output = first_number_field(usage, &["output_tokens"]);
    // Meta documents `cached_tokens` as a *subset* of input, not an extra
    // charge ("cached_tokens is a subset of your input tokens"; prompt-caching
    // docs). The `cache_read_tokens` key carries the same count; accept
    // either. Tokscale prices input and cache buckets independently, so
    // remove the overlap here rather than charging cached reads twice.
    let cache_read =
        first_number_field(usage, &["cache_read_tokens", "cached_tokens"]).min(reported_input);
    let cache_write = first_number_field(usage, &["cache_write_tokens"]);
    // `reasoning_tokens` rides inside `output_tokens` (Responses-shaped
    // usage, where `total == input + output`): every observed record
    // satisfies `reasoning <= output`. `TokenBreakdown` buckets are
    // additive — `total()` sums output and reasoning, and `compute_cost`
    // prices their sum at the output rate — so carrying the raw output
    // through while also filling `reasoning` would count every reasoning
    // token twice. Split it out instead (the Codex correction), clamped so
    // a malformed row cannot drive the bucket negative.
    let reasoning = first_number_field(usage, &["reasoning_tokens"]).min(reported_output);
    TokenBreakdown {
        input: reported_input.saturating_sub(cache_read),
        output: reported_output.saturating_sub(reasoning),
        cache_read,
        cache_write,
        cache_write_1h: 0,
        reasoning,
    }
}

fn string_field<'a>(value: &'a Value, field: &str) -> Option<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn first_number_field(value: &Value, fields: &[&str]) -> i64 {
    fields
        .iter()
        .find_map(|field| number_field(value, field))
        .unwrap_or(0)
}

fn number_field(value: &Value, field: &str) -> Option<i64> {
    number_value(value.get(field)?)
}

fn number_value(value: &Value) -> Option<i64> {
    if let Some(value) = value.as_i64() {
        return Some(value.max(0));
    }
    if let Some(value) = value.as_u64() {
        return Some(value.min(i64::MAX as u64) as i64);
    }
    if let Some(value) = value.as_f64() {
        return value.is_finite().then_some(value.max(0.0) as i64);
    }
    value
        .as_str()
        .and_then(|value| value.trim().parse::<f64>().ok())
        .and_then(|value| value.is_finite().then_some(value.max(0.0) as i64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    const SESSION_ID: &str = "01a0b7d1-b7af-7de2-8369-4f4815ae1d72";

    /// Write the given JSONL `content` to `session.jsonl` inside a session
    /// directory whose name drives the path-derived session id, then parse it.
    fn parse_session(content: &str) -> Vec<UnifiedMessage> {
        let dir = TempDir::new().unwrap();
        let session_dir = dir.path().join("sessions/2026/09/18").join(SESSION_ID);
        std::fs::create_dir_all(&session_dir).unwrap();
        let path = session_dir.join("session.jsonl");
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(content.as_bytes()).unwrap();
        drop(file);
        parse_muse_file(&path)
    }

    fn usage_line(
        sequence: i64,
        recorded_at: i64,
        model: &str,
        usage: &str,
        duration_ms: i64,
    ) -> String {
        format!(
            r#"{{"schema_version":1,"stream":{{"kind":"session","id":"{SESSION_ID}"}},"sequence":{sequence},"recorded_at":{recorded_at},"record_type":"event","payload_type":"runtime.session","payload_schema_version":1,"payload":{{"kind":"run","run_id":"898f5ab5-88d7-4baa-a281-86040f9d6a57","event":{{"kind":"model_completed","usage":{usage},"duration_ms":{duration_ms},"finish_reason":"tool_calls","model":"{model}"}}}}}}"#
        )
    }

    fn metadata_line(workspace_root: &str) -> String {
        format!(
            r#"{{"schema_version":1,"stream":{{"kind":"session","id":"{SESSION_ID}"}},"sequence":3,"recorded_at":1789790369778491,"record_type":"event","payload_type":"runtime.session.metadata","payload_schema_version":1,"payload":{{"kind":"metadata","record":{{"workspace_root":"{workspace_root}","provider_id":"meta","model_id":"muse-spark-1.3-contributor"}}}}}}"#
        )
    }

    #[test]
    fn test_model_completed_maps_to_unified_message() {
        let content = usage_line(
            39,
            1789790455896395,
            "muse-spark-1.3-contributor",
            r#"{"input_tokens":26964,"output_tokens":379,"cached_tokens":5105,"cache_write_tokens":0,"cache_read_tokens":5105,"reasoning_tokens":278}"#,
            5819,
        );
        let messages = parse_session(&content);
        assert_eq!(messages.len(), 1);
        let message = &messages[0];
        assert_eq!(message.client, "muse");
        assert_eq!(message.session_id, SESSION_ID);
        assert_eq!(message.model_id, "muse-spark-1.3-contributor");
        assert_eq!(message.provider_id, "meta");
        // Cache reads are a subset of input; reasoning rides inside output.
        assert_eq!(message.tokens.input, 26964 - 5105);
        assert_eq!(message.tokens.cache_read, 5105);
        assert_eq!(message.tokens.output, 379 - 278);
        assert_eq!(message.tokens.reasoning, 278);
        assert_eq!(message.tokens.cache_write, 0);
        assert_eq!(message.duration_ms, Some(5819));
        // recorded_at is microseconds: 1789790455896395 -> ...558963 ms.
        assert_eq!(message.timestamp, 1789790455896 - 5819);
        assert!(message.dedup_key.as_deref().unwrap().starts_with("muse:"));
    }

    #[test]
    fn test_cached_tokens_key_is_accepted_without_cache_read_tokens() {
        let content = usage_line(
            40,
            1789790455896395,
            "muse-spark-1.3",
            r#"{"input_tokens":1847,"output_tokens":98,"cached_tokens":1792,"reasoning_tokens":0}"#,
            100,
        );
        let messages = parse_session(&content);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].tokens.input, 1847 - 1792);
        assert_eq!(messages[0].tokens.cache_read, 1792);
        assert_eq!(messages[0].tokens.output, 98);
    }

    #[test]
    fn test_malformed_counts_are_clamped_not_negative() {
        let content = usage_line(
            41,
            1789790455896395,
            "muse-spark-1.3",
            r#"{"input_tokens":100,"output_tokens":10,"cache_read_tokens":999,"reasoning_tokens":999}"#,
            100,
        );
        let messages = parse_session(&content);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].tokens.input, 0);
        assert_eq!(messages[0].tokens.cache_read, 100);
        assert_eq!(messages[0].tokens.output, 0);
        assert_eq!(messages[0].tokens.reasoning, 10);
    }

    #[test]
    fn test_metadata_workspace_is_attached_to_messages() {
        let dir = TempDir::new().unwrap();
        let workspace = dir.path().join("repo");
        std::fs::create_dir_all(&workspace).unwrap();
        let root = workspace.to_string_lossy().replace('\\', "\\\\");
        let content = format!(
            "{}\n{}",
            metadata_line(&root),
            usage_line(
                39,
                1789790455896395,
                "muse-spark-1.3-contributor",
                r#"{"input_tokens":100,"output_tokens":10,"reasoning_tokens":0}"#,
                100,
            )
        );
        let session_dir = dir.path().join("sessions").join(SESSION_ID);
        std::fs::create_dir_all(&session_dir).unwrap();
        let path = session_dir.join("session.jsonl");
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(content.as_bytes()).unwrap();
        drop(file);
        let messages = parse_muse_file(&path);
        assert_eq!(messages.len(), 1);
        assert!(messages[0].workspace_key.is_some());
        assert_eq!(messages[0].workspace_label.as_deref(), Some("repo"));
    }

    #[test]
    fn test_child_lifecycle_aggregates_and_noise_are_skipped() {
        let content = [
            // Parent-side aggregate of a workflow child's usage: the child
            // transcript is scanned separately, so this must not be counted.
            r#"{"sequence":282,"recorded_at":1789790455896395,"payload_type":"runtime.session","payload":{"kind":"run","event":{"kind":"workflow_child_lifecycle","status":"usage","usage":{"input_tokens":917394,"output_tokens":5917,"cached_tokens":845908,"cache_read_tokens":845908,"reasoning_tokens":926}}}}"#,
            // A line that merely *mentions* the usage kind in free text.
            r#"{"sequence":283,"recorded_at":1789790455896395,"payload_type":"runtime.session","payload":{"kind":"run","event":{"kind":"text","text":"model_completed is the event we parse"}}}"#,
            // Malformed JSON must not abort the file.
            r#"{"sequence":284,"recorded_at":"#,
        ]
        .join("\n");
        assert!(parse_session(&content).is_empty());
    }

    #[test]
    fn test_replayed_identical_event_collapses_to_one_message() {
        let line = usage_line(
            39,
            1789790455896395,
            "muse-spark-1.3-contributor",
            r#"{"input_tokens":100,"output_tokens":10,"reasoning_tokens":0}"#,
            100,
        );
        let content = format!("{line}\n{line}");
        let messages = parse_session(&content);
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn test_recorded_at_unit_conversions() {
        assert_eq!(recorded_at_to_ms(1789790455896395), Some(1789790455896));
        assert_eq!(recorded_at_to_ms(1789790455896), Some(1789790455896));
        assert_eq!(recorded_at_to_ms(1789790455), Some(1789790455000));
        assert_eq!(recorded_at_to_ms(0), None);
        assert_eq!(recorded_at_to_ms(-5), None);
    }

    // The cases of upstream's `muse_spark_models_map_to_meta_without_claiming_the_muse_client`
    // (#1355), against the Muse-local rule.
    #[test]
    fn muse_spark_models_map_to_meta_without_claiming_the_muse_client() {
        for model in [
            "muse-spark-1.2",
            "muse-spark-1.3-contributor",
            "meta/muse-spark-1.3",
            "MUSE-SPARK-1.3",
        ] {
            assert_eq!(muse_spark_provider(model), Some("meta"), "{model}");
        }
        for model in ["muse", "muse-custom", "amuse-spark-1.3", "muse-sparkling"] {
            assert_eq!(muse_spark_provider(model), None, "{model}");
            assert_eq!(
                provider_identity::inferred_provider_from_model(model),
                None,
                "{model}"
            );
        }
    }

    // Ported from upstream `tests/muse.rs`
    // (`test_muse_parser_reads_usage_skips_aggregates_and_attaches_workspace`).
    #[test]
    fn test_parser_reads_usage_skips_aggregates_and_attaches_workspace() {
        let dir = TempDir::new().unwrap();
        let workspace = dir.path().join("repo");
        std::fs::create_dir_all(&workspace).unwrap();
        let root = workspace.to_string_lossy().replace('\\', "\\\\");
        let lines = [
            format!(
                r#"{{"sequence":3,"recorded_at":1789790369778491,"payload_type":"runtime.session.metadata","payload":{{"kind":"metadata","record":{{"workspace_root":"{root}","provider_id":"meta","model_id":"muse-spark-1.3-contributor"}}}}}}"#
            ),
            r#"{"sequence":30,"recorded_at":1789790455896390,"payload_type":"tool_batch.effect.started","payload":{"kind":"tool"}}"#.to_string(),
            format!(
                r#"{{"schema_version":1,"stream":{{"kind":"session","id":"{SESSION_ID}"}},"sequence":39,"recorded_at":1789790455896395,"record_type":"event","payload_type":"runtime.session","payload_schema_version":1,"payload":{{"kind":"run","event":{{"kind":"model_completed","usage":{{"input_tokens":26964,"output_tokens":379,"cached_tokens":5105,"cache_write_tokens":0,"cache_read_tokens":5105,"reasoning_tokens":278}},"duration_ms":5819,"finish_reason":"tool_calls","model":"muse-spark-1.3-contributor"}}}}}}"#
            ),
            r#"{"sequence":282,"recorded_at":1789790455896400,"payload_type":"runtime.session","payload":{"kind":"run","event":{"kind":"workflow_child_lifecycle","status":"usage","usage":{"input_tokens":917394,"output_tokens":5917,"cached_tokens":845908,"cache_read_tokens":845908,"reasoning_tokens":926}}}}"#.to_string(),
        ]
        .join("\n");

        let messages = parse_session(&lines);

        assert_eq!(messages.len(), 1);
        let message = &messages[0];
        assert_eq!(message.client, "muse");
        assert_eq!(message.session_id, SESSION_ID);
        assert_eq!(message.model_id, "muse-spark-1.3-contributor");
        assert_eq!(message.provider_id, "meta");
        assert_eq!(message.tokens.input, 26964 - 5105);
        assert_eq!(message.tokens.cache_read, 5105);
        assert_eq!(message.tokens.output, 379 - 278);
        assert_eq!(message.tokens.reasoning, 278);
        assert_eq!(message.duration_ms, Some(5819));
        assert_eq!(message.workspace_label.as_deref(), Some("repo"));
    }

    // The `line.contains("model_completed")` pre-filter is only a shortcut: a
    // child-lifecycle aggregate that merely mentions the word (here in its
    // `last_event`) passes it, and the event-kind check must still drop it,
    // or the parent would double-bill its child's scanned transcript.
    #[test]
    fn test_lifecycle_aggregate_mentioning_model_completed_is_still_skipped() {
        let line = r#"{"sequence":282,"recorded_at":1789790455896400,"payload_type":"runtime.session","payload":{"kind":"run","event":{"kind":"workflow_child_lifecycle","status":"usage","last_event":"model_completed","model":"muse-spark-1.3-contributor","usage":{"input_tokens":917394,"output_tokens":5917,"cached_tokens":845908,"cache_read_tokens":845908,"reasoning_tokens":926}}}}"#;

        assert!(parse_session(line).is_empty());
    }
}
