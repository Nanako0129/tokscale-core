//! DeepSeek Harness session parser.
//!
//! DeepSeek Harness (DSH) appends every session's events to a zstd-compressed
//! JSONL log at `~/.dsh/sessions/<project>/<session>/session.v4.jsonl.zstd`
//! (`session.jsonl.zstd` for sessions recorded before format 4). The log is
//! the source of truth: the projcache snapshots under
//! `~/.dsh/storages/session_projcache` are projections of it, and which
//! projections a snapshot carries varies between installs of the same DSH
//! release (some carry `contextTimeline` / `contextActivity`, some neither).
//!
//! Usage comes from two event types, one message each:
//!
//! - `assistant/message`: one completed LLM step. `data.usage` holds
//!   `inputTokens` (the uncached slice), `cacheReadTokens`, `outputTokens`
//!   and, when reported, `cacheWriteTokens`. `data.message.source` names the
//!   step's own `provider` / `model`, and the event `time` is the step's own
//!   timestamp, so a mid-session model switch or a session spanning several
//!   days is attributed step by step.
//! - `compaction/summary`: the LLM call that summarizes a compacted history,
//!   with its own `provider` / `model` / `usage`. DSH's own session totals
//!   leave it out, but it is a billed call, so it is counted here.
//!
//! `assistant/attempt` (a failed attempt that was retried) and the streaming
//! `assistant/chunk` events are not counted: the completed step carries the
//! usage once.
//!
//! Token mapping: `input = inputTokens`, `cache_read = cacheReadTokens`,
//! `output = outputTokens` (whole; no reasoning split is reported, so
//! `reasoning = 0` and nothing is double-counted), `cache_write =
//! cacheWriteTokens` (absent to 0). Summed over `assistant/message`, these
//! equal the session's `tokenUsage.val.totals` in the projcache snapshot.
//!
//! Dedup keys carry no session id and no `seq`, on purpose:
//!
//! - A seeded session (`isSeeded`, `inheritedEventCount = n`) starts with a
//!   verbatim copy of its parent's first `n` events, the parent's usage
//!   included. Keyed by `time`, turn, step, model and tokens, a copied step
//!   has the same key as the original, so it is counted once whether the
//!   parent's log is present or not. The copied steps also carry the
//!   parent's session id: the header's `parentSession` is the parent's
//!   directory name (`session-<uuid>`, checked on real seeded and subagent
//!   logs), so both logs emit identical messages and the step belongs to the
//!   parent whichever log a lane reads first, or to the parent's id alone if
//!   its log was deleted.
//! - A session migrated to format 4 keeps its old `session.jsonl.zstd` next
//!   to the new log. Both record the same steps with the same `time` and
//!   tokens but different `seq` numbering, so the same key collapses them.
//!
//! Subagent sessions live in sibling directories named by a bare UUID
//! (`delegationDepth >= 1`, `parentSession` in the header). Their usage is not
//! part of the parent's log or totals, so each is parsed as its own session.
//!
//! Identity: the session id is the log's directory name (the parent's, for
//! inherited steps), and the workspace is the `cwd` of the log's `session`
//! header. Cost is left at 0.0 for the
//! pricing pipeline: free-tier `-free` models resolve through
//! `custom-pricing.json` shadow prices, paid rows through the built-in
//! catalogs.

use super::{normalize_workspace_key, workspace_label_from_key, UnifiedMessage};
use crate::{pricing, provider_identity, TokenBreakdown};
use serde::Deserialize;
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

/// Upper bound on the decoded log, so a corrupt stream cannot run unbounded.
/// Real logs are a few MB to a few tens of MB.
const MAX_LOG_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Default, Deserialize)]
struct DshEvent {
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    time: i64,
    // The `session` header only.
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default, rename = "parentSession")]
    parent_session: Option<String>,
    #[serde(default, rename = "inheritedEventCount")]
    inherited_event_count: Option<i64>,
    #[serde(default)]
    data: Option<DshEventData>,
}

#[derive(Debug, Default, Deserialize)]
struct DshEventData {
    #[serde(default)]
    turn: Option<i64>,
    #[serde(default)]
    step: Option<i64>,
    #[serde(default)]
    message: Option<DshMessage>,
    #[serde(default)]
    usage: Option<DshUsage>,
    // `compaction/summary` carries its provider/model at the top of `data`.
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default, rename = "compactionId")]
    compaction_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct DshMessage {
    #[serde(default)]
    source: Option<DshSource>,
}

#[derive(Debug, Default, Deserialize)]
struct DshSource {
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct DshUsage {
    #[serde(default, rename = "inputTokens")]
    input: i64,
    #[serde(default, rename = "cacheReadTokens")]
    cache_read: i64,
    #[serde(default, rename = "outputTokens")]
    output: i64,
    #[serde(default, rename = "cacheWriteTokens")]
    cache_write: i64,
}

/// Only these lines are decoded as JSON; everything else in the log (tool
/// results, chunks, prompts) is skipped by a substring check first.
fn is_candidate_line(line: &str) -> bool {
    line.contains("\"type\":\"assistant/message\"")
        || line.contains("\"type\":\"compaction/summary\"")
        || line.contains("\"type\":\"session\"")
}

/// Parse one DSH `session*.jsonl.zstd` event log.
///
/// A missing or undecodable file yields no messages. A malformed line is
/// skipped and the lines after it are still read; a stream that fails to
/// decode partway keeps the steps read before the failure, since every
/// completed step is final once written.
pub fn parse_dsh_file(path: &Path) -> Vec<UnifiedMessage> {
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let Ok(decoder) = zstd::stream::read::Decoder::new(file) else {
        return Vec::new();
    };
    let mut reader = BufReader::new(decoder.take(MAX_LOG_BYTES));

    let session_id = path
        .parent()
        .and_then(|dir| dir.file_name())
        .and_then(|name| name.to_str())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or("unknown")
        .to_string();

    let mut workspace: Option<(Option<String>, Option<String>)> = None;
    // Events at index < `inherited` were copied from `parent_session`. The
    // index counts every non-empty line of the log, header included, as
    // `inheritedEventCount` does.
    let mut inherited: i64 = 0;
    let mut parent_session: Option<String> = None;
    let mut index: i64 = -1;
    let mut messages = Vec::new();
    let mut seen = HashSet::new();
    let mut raw = Vec::new();
    loop {
        raw.clear();
        match reader.read_until(b'\n', &mut raw) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if raw.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        index += 1;
        let Ok(line) = std::str::from_utf8(&raw) else {
            continue;
        };
        if !is_candidate_line(line) {
            continue;
        }
        let Ok(event) = serde_json::from_str::<DshEvent>(line) else {
            continue;
        };
        match event.kind.as_str() {
            "session" if workspace.is_none() => {
                parent_session = event
                    .parent_session
                    .clone()
                    .filter(|id| !id.trim().is_empty());
                inherited = event.inherited_event_count.unwrap_or(0).max(0);
                workspace = Some(
                    event
                        .cwd
                        .as_deref()
                        .and_then(normalize_workspace_key)
                        .map(|key| {
                            let label = workspace_label_from_key(&key);
                            (Some(key), label)
                        })
                        .unwrap_or((None, None)),
                );
            }
            "assistant/message" | "compaction/summary" => {
                let owner = match parent_session.as_deref() {
                    Some(parent) if index < inherited => parent,
                    _ => session_id.as_str(),
                };
                if let Some(message) = usage_message(&event, owner, &mut seen) {
                    messages.push(message);
                }
            }
            _ => {}
        }
    }

    if let Some((key, label)) = workspace {
        if key.is_some() || label.is_some() {
            for message in &mut messages {
                message.set_workspace(key.clone(), label.clone());
            }
        }
    }
    messages
}

fn usage_message(
    event: &DshEvent,
    session_id: &str,
    seen: &mut HashSet<String>,
) -> Option<UnifiedMessage> {
    let data = event.data.as_ref()?;
    let usage = data.usage.as_ref()?;
    if event.time <= 0 {
        return None;
    }
    let time = event.time;
    let tokens = TokenBreakdown {
        input: usage.input.max(0),
        output: usage.output.max(0),
        cache_read: usage.cache_read.max(0),
        cache_write: usage.cache_write.max(0),
        cache_write_1h: 0,
        reasoning: 0,
    };
    if tokens.input == 0 && tokens.output == 0 && tokens.cache_read == 0 && tokens.cache_write == 0
    {
        return None;
    }

    let is_compaction = event.kind == "compaction/summary";
    let source = data.message.as_ref().and_then(|m| m.source.as_ref());
    let (provider_raw, model_raw) = if is_compaction {
        (data.provider.as_deref(), data.model.as_deref())
    } else {
        (
            source.and_then(|s| s.provider.as_deref()),
            source.and_then(|s| s.model.as_deref()),
        )
    };
    let provider_raw = provider_raw.unwrap_or("unknown");
    let model_raw = model_raw.unwrap_or("unknown");

    let dedup_key = match (is_compaction, data.compaction_id.as_deref()) {
        (true, Some(id)) => format!("dsh:compaction:{id}"),
        _ => format!(
            "dsh:{}:{}:{}:{}/{}:{}:{}:{}:{}",
            time,
            data.turn.unwrap_or(0),
            data.step.unwrap_or(0),
            provider_raw,
            model_raw,
            tokens.input,
            tokens.cache_read,
            tokens.output,
            tokens.cache_write,
        ),
    };
    if !seen.insert(dedup_key.clone()) {
        return None;
    }

    let model_id = pricing::aliases::resolve_alias(model_raw)
        .unwrap_or(model_raw)
        .to_string();
    let provider_id = provider_identity::canonical_provider(provider_raw)
        .unwrap_or_else(|| provider_raw.to_string());
    let mut message = UnifiedMessage::new_with_dedup(
        "dsh",
        model_id,
        provider_id,
        session_id,
        time,
        tokens,
        0.0,
        Some(dedup_key),
    );
    message.is_turn_start = !is_compaction && data.step == Some(1);
    Some(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use tempfile::TempDir;

    /// Write `events` as a zstd JSONL log at `<dir>/<session>/<name>`.
    fn write_log(dir: &TempDir, session: &str, name: &str, events: &[Value]) -> std::path::PathBuf {
        let session_dir = dir.path().join(session);
        std::fs::create_dir_all(&session_dir).unwrap();
        let mut jsonl = String::new();
        for event in events {
            jsonl.push_str(&event.to_string());
            jsonl.push('\n');
        }
        let path = session_dir.join(name);
        std::fs::write(&path, zstd::encode_all(jsonl.as_bytes(), 3).unwrap()).unwrap();
        path
    }

    fn header(cwd: &str, extra: Value) -> Value {
        let mut event = json!({
            "type": "session",
            "version": 4,
            "id": "session-x",
            "createdAt": 1791661487168i64,
            "cwd": cwd,
            "isSeeded": false,
            "delegationDepth": 0
        });
        if let (Some(event), Some(extra)) = (event.as_object_mut(), extra.as_object()) {
            for (key, value) in extra {
                event.insert(key.clone(), value.clone());
            }
        }
        event
    }

    fn step(
        seq: i64,
        time: i64,
        turn: i64,
        step: i64,
        provider: &str,
        model: &str,
        usage: Value,
    ) -> Value {
        json!({
            "type": "assistant/message",
            "seq": seq,
            "time": time,
            "data": {
                "turn": turn,
                "step": step,
                "message": {
                    "role": "assistant",
                    "content": [{"type": "text", "text": "ok"}],
                    "source": {"kind": "model", "provider": provider, "model": model}
                },
                "usage": usage
            }
        })
    }

    fn usage(input: i64, cache_read: i64, output: i64) -> Value {
        json!({"inputTokens": input, "cacheReadTokens": cache_read, "outputTokens": output})
    }

    fn keys(messages: &[UnifiedMessage]) -> Vec<String> {
        messages
            .iter()
            .filter_map(|m| m.dedup_key.clone())
            .collect()
    }

    #[test]
    fn maps_steps_with_their_own_model_and_time() {
        let dir = TempDir::new().unwrap();
        let path = write_log(
            &dir,
            "session-abc",
            "session.v4.jsonl.zstd",
            &[
                header("C:\\Users\\alice\\repo", json!({})),
                json!({"type": "turn/start", "seq": 4, "time": 1791231855464i64, "data": {"turn": 1}}),
                step(
                    15,
                    1791231857486,
                    1,
                    1,
                    "opencode2dsh",
                    "muse-spark-1.3-contributor-free",
                    usage(11409, 143, 101),
                ),
                step(
                    23,
                    1791318257486,
                    1,
                    2,
                    "opencode2dsh",
                    "space-bunny-free",
                    usage(630, 8049, 372),
                ),
            ],
        );

        let messages = parse_dsh_file(&path);
        assert_eq!(messages.len(), 2);
        let first = &messages[0];
        assert_eq!(first.client, "dsh");
        assert_eq!(first.session_id, "session-abc");
        assert_eq!(first.model_id, "muse-spark-1.3-contributor-free");
        assert_eq!(first.provider_id, "opencode2dsh");
        assert_eq!(first.timestamp, 1791231857486);
        assert_eq!(first.tokens.input, 11409);
        assert_eq!(first.tokens.cache_read, 143);
        assert_eq!(first.tokens.output, 101);
        assert_eq!(first.tokens.cache_write, 0);
        assert_eq!(first.tokens.reasoning, 0);
        assert!(first.is_turn_start);
        assert_eq!(first.workspace_label.as_deref(), Some("repo"));
        // A model switch and a later day stay on the step that made them.
        assert_eq!(messages[1].model_id, "space-bunny-free");
        assert_eq!(messages[1].timestamp, 1791318257486);
        assert!(!messages[1].is_turn_start);
    }

    #[test]
    fn provider_is_canonicalized_and_cache_write_is_read() {
        let dir = TempDir::new().unwrap();
        let path = write_log(
            &dir,
            "session-flash",
            "session.v4.jsonl.zstd",
            &[
                header("C:\\x", json!({})),
                step(
                    9,
                    1790935688620,
                    1,
                    1,
                    "deepseek-account",
                    "deepseek-flash",
                    json!({"inputTokens": 8157, "outputTokens": 817, "cacheReadTokens": 0,
                           "cacheWriteTokens": 12, "totalTokens": 8986}),
                ),
            ],
        );
        let messages = parse_dsh_file(&path);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].model_id, "deepseek-flash");
        assert_eq!(messages[0].provider_id, "deepseek_account");
        assert_eq!(messages[0].tokens.cache_write, 12);
    }

    #[test]
    fn seeded_child_shares_keys_with_its_parent_for_inherited_steps() {
        // Shape measured on DSH 0.2.0-rc.2: the child's first
        // `inheritedEventCount` events are a verbatim copy of the parent's,
        // usage included, followed by the child's own steps.
        let dir = TempDir::new().unwrap();
        let parent_events = vec![
            header("C:\\proj", json!({})),
            step(
                16,
                1791662639676,
                1,
                1,
                "opencode2dsh",
                "muse-spark-1.3-contributor-free",
                usage(7792, 0, 146),
            ),
            step(
                24,
                1791662658328,
                2,
                1,
                "opencode2dsh",
                "muse-spark-1.3-contributor-free",
                usage(7875, 0, 262),
            ),
        ];
        let parent = write_log(
            &dir,
            "session-parent",
            "session.v4.jsonl.zstd",
            &parent_events,
        );
        let mut child_events = vec![header(
            "C:\\proj",
            json!({"isSeeded": true, "inheritedEventCount": 3, "parentSession": "session-parent"}),
        )];
        child_events.extend(parent_events[1..].iter().cloned());
        child_events.push(
            json!({"type": "session/end-seed", "seq": 40, "time": 1791662744274i64, "data": {}}),
        );
        child_events.push(step(
            50,
            1791662882361,
            4,
            1,
            "opencode2dsh",
            "muse-spark-1.3-contributor-free",
            usage(123, 8049, 976),
        ));
        let child = write_log(
            &dir,
            "session-child",
            "session.v4.jsonl.zstd",
            &child_events,
        );

        let parent_messages = parse_dsh_file(&parent);
        let child_messages = parse_dsh_file(&child);
        assert_eq!(parent_messages.len(), 2);
        // The child file alone still reports what it holds (parent absent),
        // with the inherited steps under the parent's id.
        assert_eq!(child_messages.len(), 3);
        let owners: Vec<_> = child_messages
            .iter()
            .map(|m| m.session_id.as_str())
            .collect();
        assert_eq!(
            owners,
            ["session-parent", "session-parent", "session-child"]
        );

        // Both present: each inherited step comes out of the child's log
        // exactly as it comes out of the parent's, so whichever log a lane
        // reads first, the step keeps its key and its owner.
        let identity = |m: &UnifiedMessage| (m.dedup_key.clone(), m.session_id.clone());
        let parent_view: Vec<_> = parent_messages.iter().map(identity).collect();
        let child_view: Vec<_> = child_messages[..2].iter().map(identity).collect();
        assert_eq!(child_view, parent_view);

        let parent_keys = keys(&parent_messages);
        let mut all: HashSet<String> = parent_keys.iter().cloned().collect();
        all.extend(keys(&child_messages));
        assert_eq!(all.len(), 3);
        let own = &child_messages[2];
        assert!(!parent_keys.contains(own.dedup_key.as_ref().unwrap()));
        assert_eq!(own.tokens.input, 123);
        assert_eq!(own.tokens.cache_read, 8049);
        assert_eq!(own.tokens.output, 976);
    }

    #[test]
    fn subagent_parent_link_does_not_move_its_steps() {
        // Subagents name their parent too, but inherit nothing: without
        // `inheritedEventCount` every step stays with the subagent.
        let dir = TempDir::new().unwrap();
        let path = write_log(
            &dir,
            "043682af-3599-4bfa-bc4e-08c73e8551b3",
            "session.v4.jsonl.zstd",
            &[
                header(
                    "C:\\proj",
                    json!({"delegationDepth": 1, "parentSession": "session-parent"}),
                ),
                step(14, 1791613241996, 1, 1, "p", "m", usage(8415, 0, 184)),
            ],
        );
        let messages = parse_dsh_file(&path);
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0].session_id,
            "043682af-3599-4bfa-bc4e-08c73e8551b3"
        );
    }

    #[test]
    fn migrated_session_logs_share_keys_despite_different_seq() {
        // A format-4 migration keeps `session.jsonl.zstd` beside the new log:
        // same steps, same time and tokens, renumbered `seq`.
        let dir = TempDir::new().unwrap();
        let old = write_log(
            &dir,
            "session-mig",
            "session.jsonl.zstd",
            &[
                json!({"type": "session", "version": 0, "cwd": "C:\\proj"}),
                step(
                    81,
                    1787497454467,
                    1,
                    1,
                    "openrouter",
                    "stealth/ox-alpha",
                    usage(7841, 0, 178),
                ),
                json!({"type": "assistant/chunk", "seq": 82, "time": 1787497454470i64,
                       "data": {"chunk": {"usage": {"inputTokens": 7841, "outputTokens": 178}}}}),
            ],
        );
        let new = write_log(
            &dir,
            "session-mig",
            "session.v4.jsonl.zstd",
            &[
                header("C:\\proj", json!({})),
                step(
                    15,
                    1787497454467,
                    1,
                    1,
                    "openrouter",
                    "stealth/ox-alpha",
                    usage(7841, 0, 178),
                ),
            ],
        );
        let old_keys = keys(&parse_dsh_file(&old));
        let new_keys = keys(&parse_dsh_file(&new));
        assert_eq!(old_keys.len(), 1, "the streaming chunk is not counted");
        assert_eq!(old_keys, new_keys);
    }

    #[test]
    fn appending_to_the_log_leaves_earlier_steps_unchanged() {
        // DSH appends to the live log. A later write, here a new turn a day
        // later, must not move, re-key or re-size the steps already counted:
        // reports keep the same totals for past days, and the steps a cache
        // entry already holds match the ones a re-parse produces.
        let dir = TempDir::new().unwrap();
        let mut events = vec![
            header("C:\\proj", json!({})),
            step(
                15,
                1791231857486,
                1,
                1,
                "opencode2dsh",
                "muse-spark-1.3-contributor-free",
                usage(11409, 143, 101),
            ),
            step(
                23,
                1791231860453,
                1,
                2,
                "opencode2dsh",
                "muse-spark-1.3-contributor-free",
                usage(630, 8049, 372),
            ),
        ];
        let path = write_log(&dir, "session-live", "session.v4.jsonl.zstd", &events);
        let before = parse_dsh_file(&path);
        assert_eq!(before.len(), 2);

        events.push(
            json!({"type": "turn/start", "seq": 30, "time": 1791318257000i64, "data": {"turn": 2}}),
        );
        events.push(step(
            31,
            1791318257486,
            2,
            1,
            "opencode2dsh",
            "space-bunny-free",
            usage(500, 9000, 80),
        ));
        let path = write_log(&dir, "session-live", "session.v4.jsonl.zstd", &events);
        let after = parse_dsh_file(&path);
        assert_eq!(after.len(), 3);

        for (old, new) in before.iter().zip(&after) {
            assert_eq!(old.dedup_key, new.dedup_key);
            assert_eq!(old.timestamp, new.timestamp);
            assert_eq!(old.tokens, new.tokens);
            assert_eq!(old.model_id, new.model_id);
            assert_eq!(old.is_turn_start, new.is_turn_start);
        }
        assert_eq!(after[2].timestamp, 1791318257486);
        assert_eq!(after[2].model_id, "space-bunny-free");
        assert!(after[2].is_turn_start);
    }

    #[test]
    fn compaction_summary_is_counted_once_by_its_id() {
        let dir = TempDir::new().unwrap();
        let summary = json!({
            "type": "compaction/summary",
            "seq": 54157,
            "time": 1787512178236i64,
            "data": {
                "compactionId": "500f304a-ed00-473a-968c-0699b5233483",
                "summary": [{"type": "text", "text": "summary"}],
                "provider": "openrouter",
                "model": "stealth/ox-alpha",
                "usage": {"inputTokens": 171220, "outputTokens": 5270}
            }
        });
        let path = write_log(
            &dir,
            "session-cmp",
            "session.v4.jsonl.zstd",
            &[header("C:\\proj", json!({})), summary.clone(), summary],
        );
        let messages = parse_dsh_file(&path);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].model_id, "stealth/ox-alpha");
        assert_eq!(messages[0].tokens.input, 171220);
        assert_eq!(messages[0].tokens.output, 5270);
        assert!(!messages[0].is_turn_start);
        assert_eq!(
            messages[0].dedup_key.as_deref(),
            Some("dsh:compaction:500f304a-ed00-473a-968c-0699b5233483")
        );
    }

    #[test]
    fn subagent_log_is_its_own_session() {
        let dir = TempDir::new().unwrap();
        let path = write_log(
            &dir,
            "92aab9a4-1111-2222-3333-444455556666",
            "session.v4.jsonl.zstd",
            &[
                header(
                    "C:\\proj",
                    json!({"delegationDepth": 1, "parentSession": "session-parent", "origin": "subagent"}),
                ),
                step(
                    14,
                    1791613241996,
                    1,
                    1,
                    "opencode2dsh",
                    "muse-spark-1.3-contributor-free",
                    usage(8415, 0, 184),
                ),
            ],
        );
        let messages = parse_dsh_file(&path);
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0].session_id,
            "92aab9a4-1111-2222-3333-444455556666"
        );
        assert_eq!(messages[0].workspace_label.as_deref(), Some("proj"));
    }

    #[test]
    fn skips_attempts_zero_usage_and_malformed_lines() {
        let dir = TempDir::new().unwrap();
        let session_dir = dir.path().join("session-mixed");
        std::fs::create_dir_all(&session_dir).unwrap();
        let mut jsonl = String::new();
        for event in [
            header("C:\\proj", json!({})),
            json!({"type": "assistant/attempt", "seq": 5, "time": 1791194523000i64,
                   "data": {"stream": [{"chunk": {"usage": {"inputTokens": 900, "outputTokens": 9}}}]}}),
            step(6, 1791194523506, 1, 1, "p", "m", usage(0, 0, 0)),
            step(7, 1791194524506, 1, 2, "p", "m", usage(100, 10, 5)),
        ] {
            jsonl.push_str(&event.to_string());
            jsonl.push('\n');
        }
        jsonl.push_str("{\"type\":\"assistant/message\", not json\n");
        jsonl.push_str(&step(8, 1791194525506, 1, 3, "p", "m", usage(200, 20, 6)).to_string());
        let path = session_dir.join("session.v4.jsonl.zstd");
        std::fs::write(&path, zstd::encode_all(jsonl.as_bytes(), 3).unwrap()).unwrap();

        let messages = parse_dsh_file(&path);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].tokens.input, 100);
        assert_eq!(messages[1].tokens.input, 200);
    }

    #[test]
    fn unreadable_inputs_yield_nothing() {
        let dir = TempDir::new().unwrap();
        assert!(parse_dsh_file(&dir.path().join("missing/session.v4.jsonl.zstd")).is_empty());

        let session_dir = dir.path().join("session-bad");
        std::fs::create_dir_all(&session_dir).unwrap();
        let not_zstd = session_dir.join("session.v4.jsonl.zstd");
        std::fs::write(&not_zstd, b"not a zstd stream").unwrap();
        assert!(parse_dsh_file(&not_zstd).is_empty());

        let header_only = write_log(
            &dir,
            "session-empty",
            "session.v4.jsonl.zstd",
            &[header("C:\\x", json!({}))],
        );
        assert!(parse_dsh_file(&header_only).is_empty());
    }
}
