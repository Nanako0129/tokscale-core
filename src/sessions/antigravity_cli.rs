//! Antigravity CLI session parser
//!
//! The Antigravity CLI (the terminal agent, distinct from the Antigravity IDE)
//! stores each conversation as a SQLite database under
//! `~/.gemini/antigravity-cli/conversations/<uuid>.db`. Unlike the IDE-backed
//! [`super::antigravity`] source — which depends on a *running* language server
//! reachable over RPC and caches JSONL under the config dir — the CLI usage is
//! already on disk and can be read directly. No RPC, no `antigravity sync`.
//!
//! Each `gen_metadata` row is one generation encoded as the same
//! `GeneratorMetadata` protobuf the IDE returns over
//! `GetCascadeTrajectoryGeneratorMetadata`. The repository has no `.proto` /
//! prost decoder (the IDE path receives JSON because the language server does
//! the proto→JSON conversion), so this module ships a tiny wire-format reader
//! and pulls only the few fields it needs. The field numbers below were
//! reverse-engineered from real databases and cross-checked across 6 sessions
//! / 140 turns (`#9 + #10 == #3`, i.e. output + thinking == total output;
//! `#5`/cacheRead only appears once a cached prefix exists and grows with the
//! conversation):
//!
//! - `gen_metadata.#1`            → chatModel message
//!   - `#19` (string)            → responseModel (e.g. `gemini-3-flash-a`)
//!   - `#9.#4` = `{#1: seconds, #2: nanos}` → per-generation wall-clock time
//!   - `#4`                      → usage message
//!     - `#1` (varint, const)    → fixed system-prompt tokens (≈1132)
//!     - `#2` (varint)           → newly-processed (non-cached) input tokens
//!     - `#5` (varint)           → cacheRead tokens
//!     - `#9` (varint)           → output (text) tokens
//!     - `#10` (varint)          → thinking / reasoning tokens
//!     - `#11` (string)          → responseId (dedup key)
//! - `trajectory_metadata_blob.#2` = `{#1: seconds, #2: nanos}` → created-at
//! - `trajectory_metadata_blob.#1.#1` (string)                  → workspace URI

use super::utils::open_readonly_sqlite;
use super::{normalize_workspace_key, workspace_label_from_key, UnifiedMessage};
use crate::{pricing, provider_identity, TokenBreakdown};
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Context built from conversation DBs and log files for Antigravity CLI session timestamping.
#[derive(Debug, Clone, Default)]
pub struct AntigravityCliTimestampContext {
    pub(crate) log_timestamps: HashMap<String, Option<i64>>,
    pub(crate) colliding_response_ids: HashSet<String>,
}

/// Derive existing `log/cli-*.log` dependencies for a set of conversation DB paths.
pub(crate) fn find_log_files(conversation_paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut log_dirs = HashSet::new();
    for path in conversation_paths {
        if let Some(cli_root) = path.parent().and_then(Path::parent) {
            let log_dir = cli_root.join("log");
            if log_dir.is_dir() {
                log_dirs.insert(log_dir);
            }
        }
    }

    let mut log_files = Vec::new();
    for dir in log_dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if file_name.starts_with("cli-") && file_name.ends_with(".log") {
                log_files.push(path);
            }
        }
    }
    log_files.sort();
    log_files
}

fn parse_log_year(path: &Path) -> Option<i32> {
    let file_name = path.file_name()?.to_str()?;
    let rest = file_name.strip_prefix("cli-")?;
    // `len()` / `&rest[..4]` are byte-based: a multi-byte UTF-8 filename such as
    // `cli-日本語.log` would slice mid-character and panic. `get` returns None
    // when 4 is not a char boundary, so the file is skipped instead.
    let year: i32 = rest.get(..4)?.parse().ok()?;
    if (1970..=9999).contains(&year) {
        Some(year)
    } else {
        None
    }
}

fn parse_header_timestamp(header: &str, year: i32) -> Option<i64> {
    let bytes = header.as_bytes();
    if bytes.first() != Some(&b'I') || bytes.len() < 16 {
        return None;
    }
    let month: u32 = header.get(1..3)?.parse().ok()?;
    let day: u32 = header.get(3..5)?.parse().ok()?;
    if bytes.get(5) != Some(&b' ') {
        return None;
    }
    let hour: u32 = header.get(6..8)?.parse().ok()?;
    if bytes.get(8) != Some(&b':') {
        return None;
    }
    let min: u32 = header.get(9..11)?.parse().ok()?;
    if bytes.get(11) != Some(&b':') {
        return None;
    }
    let sec: u32 = header.get(12..14)?.parse().ok()?;
    if bytes.get(14) != Some(&b'.') {
        return None;
    }

    let micro_str = &header[15..];
    let micro_digits_len = micro_str
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(micro_str.len());
    if micro_digits_len == 0 {
        return None;
    }
    let digits = &micro_str[..micro_digits_len];
    let take_len = digits.len().min(6);
    let micro_val: u32 = digits[..take_len].parse().ok()?;
    let scale = 10u32.pow((6 - take_len) as u32);
    let micro = micro_val.checked_mul(scale)?;

    let naive_date = chrono::NaiveDate::from_ymd_opt(year, month, day)?;
    let naive_dt = naive_date.and_hms_micro_opt(hour, min, sec, micro)?;
    // glog writes the wall-clock time of the machine that produced the line and
    // carries no UTC offset, so it must be read back in the system timezone.
    // Only meaningful when the log is parsed on the same machine/timezone that
    // wrote it; logs copied across timezones are not accounted for, and this is
    // not valid if the writer had FLAGS_log_utc_time enabled.
    use chrono::TimeZone;
    let dt = chrono::Local.from_local_datetime(&naive_dt).single()?;
    let utc_ms = dt.timestamp_millis();
    if utc_ms > 0 {
        Some(utc_ms)
    } else {
        None
    }
}

fn parse_log_line(line: &str, year: i32) -> Option<(String, Option<i64>)> {
    let resp_idx = line.find("ResponseID:")?;
    let after_resp = &line[resp_idx + "ResponseID:".len()..];
    let trimmed = after_resp.trim_start();
    let rid_len = trimmed
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .unwrap_or(trimmed.len());
    let rid = &trimmed[..rid_len];
    if rid.is_empty() {
        return None;
    }

    let header = line
        .strip_prefix("ERROR: logging before google.Init: ")
        .unwrap_or(line);
    let parsed_time = parse_header_timestamp(header, year);

    Some((rid.to_string(), parsed_time))
}

pub fn build_timestamp_context(conversation_paths: &[PathBuf]) -> AntigravityCliTimestampContext {
    let mut log_timestamps: HashMap<String, Option<i64>> = HashMap::new();

    let log_files = find_log_files(conversation_paths);
    for log_path in log_files {
        let Some(year) = parse_log_year(&log_path) else {
            continue;
        };
        let Ok(file) = std::fs::File::open(&log_path) else {
            continue;
        };
        use std::io::BufRead;
        let reader = std::io::BufReader::new(file);
        for line in reader.lines().map_while(Result::ok) {
            let Some((rid, parsed_time)) = parse_log_line(&line, year) else {
                continue;
            };
            match log_timestamps.entry(rid) {
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(parsed_time);
                }
                std::collections::hash_map::Entry::Occupied(mut e) => {
                    e.insert(None);
                }
            }
        }
    }

    let mut rid_first_db: HashMap<String, &Path> = HashMap::new();
    let mut colliding_response_ids: HashSet<String> = HashSet::new();

    for path in conversation_paths {
        let Some(conn) = open_readonly_sqlite(path) else {
            continue;
        };
        let mut stmt = match conn.prepare("SELECT data FROM gen_metadata") {
            Ok(stmt) => stmt,
            Err(_) => continue,
        };
        let rows = match stmt.query_map([], |row| row.get::<_, Vec<u8>>(0)) {
            Ok(rows) => rows,
            Err(_) => continue,
        };
        for blob in rows.flatten() {
            let Some(chat_model) = message_field(&blob, 1) else {
                continue;
            };
            let Some(usage) = message_field(chat_model, 4) else {
                continue;
            };
            let Some(rid) = string_field(usage, 11) else {
                continue;
            };
            let trimmed = rid.trim();
            if trimmed.is_empty() {
                continue;
            }
            match rid_first_db.entry(trimmed.to_string()) {
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(path.as_path());
                }
                std::collections::hash_map::Entry::Occupied(e) => {
                    if *e.get() != path.as_path() {
                        colliding_response_ids.insert(trimmed.to_string());
                    }
                }
            }
        }
    }

    AntigravityCliTimestampContext {
        log_timestamps,
        colliding_response_ids,
    }
}

pub fn parse_antigravity_cli_file(path: &Path) -> Vec<UnifiedMessage> {
    parse_antigravity_cli_file_with_context(path, &AntigravityCliTimestampContext::default())
}

pub fn parse_antigravity_cli_file_with_context(
    path: &Path,
    context: &AntigravityCliTimestampContext,
) -> Vec<UnifiedMessage> {
    let Some(conn) = open_readonly_sqlite(path) else {
        return Vec::new();
    };

    let session_id = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("unknown")
        .to_string();

    let (timestamp, workspace_key, workspace_label) = read_trajectory_meta(&conn, path);

    let mut stmt = match conn.prepare("SELECT data FROM gen_metadata ORDER BY idx") {
        Ok(stmt) => stmt,
        // Not an Antigravity CLI database (table missing) — nothing to count.
        Err(_) => return Vec::new(),
    };
    let rows = match stmt.query_map([], |row| row.get::<_, Vec<u8>>(0)) {
        Ok(rows) => rows,
        Err(_) => return Vec::new(),
    };

    let step_timestamps = read_step_timestamps(&conn);

    let mut messages = Vec::new();
    let mut seen_response_ids: HashSet<String> = HashSet::new();
    for blob in rows.flatten() {
        // `timestamp` is the session-created fallback; each row prefers its own
        // per-generation wall-clock stamp (see `parse_gen_metadata`).
        if let Some(mut message) = parse_gen_metadata(
            &blob,
            &session_id,
            timestamp,
            &step_timestamps,
            context,
            &mut seen_response_ids,
        ) {
            if workspace_key.is_some() {
                message.set_workspace(workspace_key.clone(), workspace_label.clone());
            }
            messages.push(message);
        }
    }

    messages
}

fn parse_gen_metadata(
    blob: &[u8],
    session_id: &str,
    session_timestamp: i64,
    step_timestamps: &HashMap<Vec<u8>, StepTimestamp>,
    context: &AntigravityCliTimestampContext,
    seen_response_ids: &mut HashSet<String>,
) -> Option<UnifiedMessage> {
    let chat_model = message_field(blob, 1)?;
    let usage = message_field(chat_model, 4)?;

    // Per-generation wall-clock time fallback chain:
    // 1. `chatModel.#9.#4` (native Timestamp {#1 seconds, #2 nanos})
    // 2. `steps` table timestamp joined on responseId (`usage.#11`)
    // 3. CLI log timestamp joined on responseId (`usage.#11`)
    // 4. session-created fallback `session_timestamp`
    let native = message_field(chat_model, 9)
        .and_then(|gen| message_field(gen, 4))
        .and_then(proto_timestamp_ms)
        .filter(|&ms| ms > 0);

    let steps = message_field(usage, 11)
        .and_then(|resp_id| step_timestamps.get(resp_id))
        .and_then(|ts| match ts {
            StepTimestamp::Unique(ms) if *ms > 0 => Some(*ms),
            _ => None,
        });

    let log = string_field(usage, 11)
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .and_then(|key| {
            if context.colliding_response_ids.contains(key) {
                None
            } else {
                context
                    .log_timestamps
                    .get(key)
                    .copied()
                    .flatten()
                    .filter(|&ms| ms > 0)
            }
        });

    let timestamp = native.or(steps).or(log).unwrap_or(session_timestamp);

    // input = fixed system prompt (#1) + newly-processed input (#2). The
    // constant #1 is, to the best of our reverse-engineering, the agent's fixed
    // system prompt and counts as billable input; if an official schema later
    // contradicts this, only the input total needs revisiting.
    // Clamp untrusted u64 varints into i64 (a corrupt/malicious blob could
    // encode a value > i64::MAX, which `as i64` would wrap to a negative count)
    // and combine with saturating_add so totals never overflow.
    let to_i64 = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
    let input = to_i64(varint_field(usage, 1).unwrap_or(0))
        .saturating_add(to_i64(varint_field(usage, 2).unwrap_or(0)));
    let cache_read = to_i64(varint_field(usage, 5).unwrap_or(0));
    let output = to_i64(varint_field(usage, 9).unwrap_or(0));
    let reasoning = to_i64(varint_field(usage, 10).unwrap_or(0));
    if input == 0 && output == 0 && cache_read == 0 && reasoning == 0 {
        return None;
    }

    let dedup_key = string_field(usage, 11)
        .filter(|text| !text.trim().is_empty())
        .map(|text| text.to_string());
    if let Some(key) = &dedup_key {
        if !seen_response_ids.insert(key.clone()) {
            return None;
        }
    }

    let model_raw = string_field(chat_model, 19)
        .filter(|text| !text.trim().is_empty())
        .unwrap_or("unknown");
    let model_id = pricing::aliases::resolve_alias(model_raw)
        .unwrap_or(model_raw)
        .to_string();
    let provider_id = provider_identity::inferred_provider_from_model(&model_id)
        .unwrap_or("antigravity")
        .to_string();

    Some(UnifiedMessage::new_with_dedup(
        "antigravity-cli",
        model_id,
        provider_id,
        session_id,
        timestamp,
        TokenBreakdown {
            input,
            output,
            cache_read,
            cache_write: 0,
            reasoning,
            cache_write_1h: 0,
        },
        0.0,
        dedup_key,
    ))
}

/// Read the session-level created-at timestamp and workspace from the single
/// `trajectory_metadata_blob` row. This timestamp dates the conversation as a
/// whole and is the per-row fallback for any `gen_metadata` row missing its own
/// `#9.#4` wall-clock stamp. Falls back to the file mtime when the blob is
/// absent or undecodable.
fn read_trajectory_meta(conn: &Connection, path: &Path) -> (i64, Option<String>, Option<String>) {
    let blob: Option<Vec<u8>> = conn
        .query_row(
            "SELECT data FROM trajectory_metadata_blob LIMIT 1",
            [],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .ok();

    let mut timestamp = None;
    let mut workspace_key = None;
    let mut workspace_label = None;

    if let Some(blob) = &blob {
        timestamp = session_created_ms(blob).filter(|&ms| ms > 0);

        if let Some(uri) = message_field(blob, 1).and_then(|folder| string_field(folder, 1)) {
            if let Some(path_str) = file_uri_to_path(uri) {
                workspace_key = normalize_workspace_key(&path_str);
                workspace_label = workspace_key.as_deref().and_then(workspace_label_from_key);
            }
        }
    }

    let timestamp = timestamp.unwrap_or_else(|| file_modified_ms(path));
    (timestamp, workspace_key, workspace_label)
}

fn session_created_ms(blob: &[u8]) -> Option<i64> {
    proto_timestamp_ms(message_field(blob, 2)?)
}

/// One resolved step timestamp candidate for a responseId.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepTimestamp {
    Unique(i64),
    Ambiguous,
}

fn read_step_timestamps(conn: &Connection) -> HashMap<Vec<u8>, StepTimestamp> {
    let mut map = HashMap::new();
    let mut stmt = match conn.prepare("SELECT idx, step_type, metadata FROM steps") {
        Ok(stmt) => stmt,
        Err(_) => return map,
    };

    let rows = match stmt.query_map([], |row| {
        let step_type: Option<i64> = row.get(1).ok();
        let metadata: Option<Vec<u8>> = row.get(2).ok();
        Ok((step_type, metadata))
    }) {
        Ok(rows) => rows,
        Err(_) => return map,
    };

    for row in rows.flatten() {
        let (Some(15), Some(metadata)) = row else {
            continue;
        };

        let Some(key) = message_field(&metadata, 9)
            .and_then(|m9| message_field(m9, 11))
            .filter(|k| !k.is_empty())
        else {
            continue;
        };

        let Some(ms) = message_field(&metadata, 1)
            .and_then(proto_timestamp_ms)
            .filter(|&ms| ms > 0)
        else {
            continue;
        };

        match map.entry(key.to_vec()) {
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(StepTimestamp::Unique(ms));
            }
            std::collections::hash_map::Entry::Occupied(mut e) => {
                e.insert(StepTimestamp::Ambiguous);
            }
        }
    }

    map
}

/// Decode a protobuf `{#1: seconds, #2: nanos}` Timestamp message to epoch ms.
/// Shared by the session-created stamp and the per-generation `#9.#4` stamp.
///
/// `seconds` is an unbounded wire varint, so a malformed blob can carry a value
/// whose `* 1000` overflows `i64` and panics in debug builds. Use checked
/// arithmetic and return `None` on overflow to keep the module's
/// "malformed data degrades to `None`, never panics" contract.
///
/// `nanos` is range-validated against the protobuf Timestamp spec (must be
/// `0..=999_999_999`); an out-of-range or negative `nanos` marks the whole
/// stamp as malformed (`None`) so the caller's `ms > 0` filter and
/// session-timestamp fallback take over instead of producing a skewed time.
fn proto_timestamp_ms(ts: &[u8]) -> Option<i64> {
    let seconds = varint_field(ts, 1)? as i64;
    let nanos = i64::try_from(varint_field(ts, 2).unwrap_or(0)).ok()?;
    if !(0..=999_999_999).contains(&nanos) {
        return None;
    }
    seconds.checked_mul(1000)?.checked_add(nanos / 1_000_000)
}

fn file_modified_ms(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .map(|time| chrono::DateTime::<chrono::Utc>::from(time).timestamp_millis())
        .unwrap_or(0)
}

/// Convert a `file://` URI to a filesystem path, percent-decoding UTF-8 escapes
/// (workspace paths on cloud drives can be percent-encoded CJK). After the
/// scheme the remainder is `authority + path`; the three shapes RFC 8089 (and
/// Antigravity) produce are handled:
/// - `file:///C:/x`        → `C:/x`            (empty authority, Windows drive: drop the leading slash)
/// - `file:///home/x`      → `/home/x`         (empty authority, POSIX absolute: keep as-is)
/// - `file://host/share/x` → `//host/share/x`  (non-empty authority → UNC: restore the leading `//`)
fn file_uri_to_path(uri: &str) -> Option<String> {
    let decoded = percent_decode(uri.strip_prefix("file://")?);
    let bytes = decoded.as_bytes();
    let path = if bytes.first() == Some(&b'/') {
        // Empty authority. Drop the slash before a Windows drive letter
        // (`/C:/...`); keep POSIX absolute paths untouched.
        if bytes.len() >= 3 && bytes[2] == b':' {
            decoded[1..].to_string()
        } else {
            decoded
        }
    } else {
        // Non-empty authority (`host/share/...`) is a UNC path; restore the
        // leading `//` so `normalize_workspace_key` preserves the UNC prefix
        // instead of collapsing it into the path body.
        format!("//{decoded}")
    };
    Some(path)
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_value(bytes[i + 1]), hex_value(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Minimal protobuf wire-format reader (no prost / schema dependency).
// ---------------------------------------------------------------------------

enum Wire<'a> {
    Varint(u64),
    Len(&'a [u8]),
    Fixed64,
    Fixed32,
}

struct ProtoReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> ProtoReader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn read_varint(&mut self) -> Option<u64> {
        let mut result: u64 = 0;
        let mut shift = 0u32;
        loop {
            let byte = *self.buf.get(self.pos)?;
            self.pos += 1;
            result |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Some(result);
            }
            shift += 7;
            if shift >= 64 {
                return None;
            }
        }
    }

    /// Yield the next `(field_number, value)` pair, or `None` at end-of-buffer
    /// or on a malformed/unsupported wire type. Group wire types (3/4) are
    /// deprecated and never appear here; we stop rather than risk desync.
    fn next_field(&mut self) -> Option<(u64, Wire<'a>)> {
        if self.pos >= self.buf.len() {
            return None;
        }
        let tag = self.read_varint()?;
        let field = tag >> 3;
        let wire = match tag & 0x7 {
            0 => Wire::Varint(self.read_varint()?),
            1 => {
                self.pos = self.pos.checked_add(8).filter(|&p| p <= self.buf.len())?;
                Wire::Fixed64
            }
            2 => {
                let len = self.read_varint()? as usize;
                let end = self.pos.checked_add(len).filter(|&p| p <= self.buf.len())?;
                let bytes = &self.buf[self.pos..end];
                self.pos = end;
                Wire::Len(bytes)
            }
            5 => {
                self.pos = self.pos.checked_add(4).filter(|&p| p <= self.buf.len())?;
                Wire::Fixed32
            }
            _ => return None,
        };
        Some((field, wire))
    }
}

/// First length-delimited (sub-message / string / bytes) value for `field`.
fn message_field(buf: &[u8], field: u64) -> Option<&[u8]> {
    let mut reader = ProtoReader::new(buf);
    while let Some((found, wire)) = reader.next_field() {
        if found == field {
            if let Wire::Len(bytes) = wire {
                return Some(bytes);
            }
        }
    }
    None
}

/// First varint value for `field`.
fn varint_field(buf: &[u8], field: u64) -> Option<u64> {
    let mut reader = ProtoReader::new(buf);
    while let Some((found, wire)) = reader.next_field() {
        if found == field {
            if let Wire::Varint(value) = wire {
                return Some(value);
            }
        }
    }
    None
}

/// First UTF-8 string value for `field`.
fn string_field(buf: &[u8], field: u64) -> Option<&str> {
    message_field(buf, field).and_then(|bytes| std::str::from_utf8(bytes).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{params, Connection};

    fn enc_varint(field: u64, value: u64) -> Vec<u8> {
        let mut out = encode_varint(field << 3);
        out.extend(encode_varint(value));
        out
    }

    fn enc_len(field: u64, payload: &[u8]) -> Vec<u8> {
        let mut out = encode_varint((field << 3) | 2);
        out.extend(encode_varint(payload.len() as u64));
        out.extend_from_slice(payload);
        out
    }

    fn encode_varint(mut value: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let mut byte = (value & 0x7f) as u8;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if value == 0 {
                break;
            }
        }
        out
    }

    fn build_gen_metadata() -> Vec<u8> {
        build_gen_metadata_with_model("gemini-3-flash-a")
    }

    fn build_gen_metadata_with_model(model: &str) -> Vec<u8> {
        build_gen_metadata_full(model, Some(b"resp-1"), None)
    }

    fn build_gen_metadata_full(
        model: &str,
        response_id: Option<&[u8]>,
        native_ts: Option<(u64, u64)>,
    ) -> Vec<u8> {
        let mut usage = Vec::new();
        usage.extend(enc_varint(1, 1132)); // fixed system prompt
        usage.extend(enc_varint(2, 500)); // new input
        usage.extend(enc_varint(5, 16000)); // cacheRead
        usage.extend(enc_varint(9, 300)); // output
        usage.extend(enc_varint(10, 40)); // thinking
        if let Some(resp_id) = response_id {
            usage.extend(enc_len(11, resp_id)); // responseId
        }

        let mut chat_model = Vec::new();
        chat_model.extend(enc_len(4, &usage));
        if let Some((sec, nano)) = native_ts {
            let mut gen_time = Vec::new();
            gen_time.extend(enc_varint(1, sec));
            gen_time.extend(enc_varint(2, nano));
            let gen9 = enc_len(4, &gen_time);
            chat_model.extend(enc_len(9, &gen9));
        }
        chat_model.extend(enc_len(19, model.as_bytes()));

        enc_len(1, &chat_model)
    }

    fn build_step_metadata_raw(response_id: Option<&[u8]>, ts_bytes: Option<&[u8]>) -> Vec<u8> {
        let mut metadata = Vec::new();
        if let Some(ts) = ts_bytes {
            metadata.extend(enc_len(1, ts));
        }
        if let Some(resp_id) = response_id {
            let mut m9 = Vec::new();
            m9.extend(enc_len(11, resp_id));
            metadata.extend(enc_len(9, &m9));
        }
        metadata
    }

    fn build_step_metadata(response_id: &[u8], seconds: u64, nanos: u64) -> Vec<u8> {
        let mut ts = Vec::new();
        ts.extend(enc_varint(1, seconds));
        ts.extend(enc_varint(2, nanos));
        build_step_metadata_raw(Some(response_id), Some(&ts))
    }

    fn assert_standard_message_fields(msg: &UnifiedMessage, expected_dedup: Option<&str>) {
        assert_eq!(msg.client, "antigravity-cli");
        assert_eq!(msg.model_id, "gemini-3.5-flash-high");
        assert_eq!(msg.provider_id, "google");
        assert_eq!(msg.tokens.input, 1632);
        assert_eq!(msg.tokens.output, 300);
        assert_eq!(msg.tokens.cache_read, 16000);
        assert_eq!(msg.tokens.reasoning, 40);
        assert_eq!(msg.tokens.cache_write, 0);
        assert_eq!(msg.dedup_key.as_deref(), expected_dedup);
    }

    fn build_trajectory_meta() -> Vec<u8> {
        let workspace = enc_len(1, b"file:///C:/Users/Frank/obsidian-vault");
        let created = {
            let mut created = Vec::new();
            created.extend(enc_varint(1, 1_781_502_653)); // seconds
            created.extend(enc_varint(2, 0)); // nanos
            created
        };
        let mut blob = Vec::new();
        blob.extend(enc_len(1, &workspace));
        blob.extend(enc_len(2, &created));
        blob
    }

    fn parse_gen_metadata_default(
        blob: &[u8],
        session_id: &str,
        session_timestamp: i64,
        step_timestamps: &HashMap<Vec<u8>, StepTimestamp>,
        seen_response_ids: &mut HashSet<String>,
    ) -> Option<UnifiedMessage> {
        parse_gen_metadata(
            blob,
            session_id,
            session_timestamp,
            step_timestamps,
            &AntigravityCliTimestampContext::default(),
            seen_response_ids,
        )
    }

    #[test]
    fn overlarge_varint_token_counts_are_clamped_not_wrapped() {
        // A corrupt/malicious blob encoding a varint > i64::MAX must clamp to a
        // non-negative i64 (saturating), never wrap `as i64` to a negative count.
        let mut usage = Vec::new();
        usage.extend(enc_varint(1, u64::MAX)); // huge fixed system prompt
        usage.extend(enc_varint(2, 10)); // + small input -> saturating_add
        usage.extend(enc_varint(9, u64::MAX)); // huge output
        usage.extend(enc_len(11, b"resp-overflow"));
        let mut chat_model = Vec::new();
        chat_model.extend(enc_len(4, &usage));
        chat_model.extend(enc_len(19, b"gemini-3-flash-a"));
        let blob = enc_len(1, &chat_model);

        let mut seen = HashSet::new();
        let msg = parse_gen_metadata_default(&blob, "s", 1_000, &HashMap::new(), &mut seen)
            .expect("parses");
        assert_eq!(msg.tokens.output, i64::MAX);
        assert_eq!(msg.tokens.input, i64::MAX); // saturating_add, not negative
        assert!(msg.tokens.input >= 0 && msg.tokens.output >= 0);
    }

    #[test]
    fn parses_tokens_model_and_workspace_from_db() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-test.db");

        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE gen_metadata (idx integer, data blob, size integer);
                 CREATE TABLE trajectory_metadata_blob (id text, data blob);",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO gen_metadata (idx, data, size) VALUES (0, ?1, 0)",
                params![build_gen_metadata()],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO trajectory_metadata_blob (id, data) VALUES ('main', ?1)",
                params![build_trajectory_meta()],
            )
            .unwrap();
        }

        let messages = parse_antigravity_cli_file(&path);
        assert_eq!(messages.len(), 1);
        let message = &messages[0];
        assert_eq!(message.client, "antigravity-cli");
        // `gemini-3-flash-a` (raw #19 responseModel) is alias-resolved to the
        // priced canonical model so cost lookups don't fall through to 0.
        // Per upstream (models.ts@603e3ea), this legacy M132 response model
        // belongs to the Gemini 3.5 Flash High tier, not the retired preview
        // family.
        assert_eq!(message.model_id, "gemini-3.5-flash-high");
        assert_eq!(message.provider_id, "google");
        assert_eq!(message.session_id, "session-test");
        assert_eq!(message.tokens.input, 1632); // 1132 + 500
        assert_eq!(message.tokens.cache_read, 16000);
        assert_eq!(message.tokens.output, 300);
        assert_eq!(message.tokens.reasoning, 40);
        assert_eq!(message.dedup_key.as_deref(), Some("resp-1"));
        assert_eq!(message.timestamp, 1_781_502_653_000);
        assert_eq!(
            message.workspace_key.as_deref(),
            Some("C:/Users/Frank/obsidian-vault")
        );
        assert_eq!(message.workspace_label.as_deref(), Some("obsidian-vault"));
    }

    #[test]
    fn resolves_current_antigravity_cli_response_model() {
        let blob = build_gen_metadata_with_model("gemini-3-flash-agent");
        let mut seen = HashSet::new();

        let message =
            parse_gen_metadata_default(&blob, "session", 1_000, &HashMap::new(), &mut seen)
                .unwrap();

        assert_eq!(message.model_id, "gemini-3.5-flash-high");
        assert_eq!(message.provider_id, "google");
    }

    #[test]
    fn per_generation_timestamp_overrides_session_fallback() {
        // chatModel.#9.#4 = {#1: seconds, #2: nanos} is the per-turn wall-clock
        // stamp. When present it dates the row; when absent the row falls back
        // to the session-created timestamp passed in. (Verified against real
        // databases: every gen_metadata row carries a distinct, monotonic
        // #9.#4 stamp >= the session-created time.)
        let session_fallback = 111_000_i64;

        let mut usage = Vec::new();
        usage.extend(enc_varint(2, 500)); // input
        usage.extend(enc_varint(9, 300)); // output
        usage.extend(enc_len(11, b"with-time")); // responseId

        // #9 wraps a sub-message whose #4 is the {seconds, nanos} Timestamp.
        let mut gen_time = Vec::new();
        gen_time.extend(enc_varint(1, 1_781_000_000)); // seconds
        gen_time.extend(enc_varint(2, 250_000_000)); // nanos -> +250ms
        let gen9 = enc_len(4, &gen_time);

        let mut chat_model = Vec::new();
        chat_model.extend(enc_len(4, &usage));
        chat_model.extend(enc_len(9, &gen9));
        chat_model.extend(enc_len(19, b"gemini-3-flash-a"));
        let blob = enc_len(1, &chat_model);

        let mut seen = HashSet::new();
        let message =
            parse_gen_metadata_default(&blob, "s", session_fallback, &HashMap::new(), &mut seen)
                .unwrap();
        assert_eq!(
            message.timestamp,
            1_781_000_000 * 1000 + 250,
            "per-generation #9.#4 timestamp must override the session fallback"
        );

        // The same row shape without #9 falls back to the session timestamp
        // (build_gen_metadata carries no #9.#4).
        let mut seen2 = HashSet::new();
        let fallback_msg = parse_gen_metadata_default(
            &build_gen_metadata(),
            "s",
            session_fallback,
            &HashMap::new(),
            &mut seen2,
        )
        .unwrap();
        assert_eq!(
            fallback_msg.timestamp, session_fallback,
            "a row without #9.#4 must use the session-created fallback"
        );
    }

    #[test]
    fn dedupes_repeated_response_ids_and_skips_zero_usage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dupes.db");

        // Two rows share responseId "dup"; a third row has all-zero usage.
        let mut zero_usage = Vec::new();
        zero_usage.extend(enc_len(11, b"zero"));
        let mut zero_chat = Vec::new();
        zero_chat.extend(enc_len(4, &zero_usage));
        zero_chat.extend(enc_len(19, b"gemini-3-flash-a"));
        let zero_blob = enc_len(1, &zero_chat);

        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("CREATE TABLE gen_metadata (idx integer, data blob, size integer);")
                .unwrap();
            for (idx, blob) in [
                (0, build_gen_metadata()),
                (1, build_gen_metadata()),
                (2, zero_blob),
            ] {
                conn.execute(
                    "INSERT INTO gen_metadata (idx, data, size) VALUES (?1, ?2, 0)",
                    params![idx, blob],
                )
                .unwrap();
            }
        }

        let messages = parse_antigravity_cli_file(&path);
        // Only the first "resp-1" row survives; the duplicate and the
        // zero-usage row are dropped. Missing trajectory_metadata_blob table is
        // tolerated (timestamp falls back to file mtime).
        assert_eq!(messages.len(), 1);
        assert!(messages[0].timestamp > 0);
    }

    #[test]
    fn emitted_model_string_resolves_to_priced_alias() {
        // The parser emits the raw `#19` responseModel (`gemini-3-flash-a`) and
        // relies on the alias table to map it onto a priced model. Without the
        // alias the cost would resolve to 0, so lock the resolution here at the
        // unit level (an end-to-end calculate_cost path needs the live pricing
        // dataset, which is unavailable in unit tests).
        assert_eq!(
            pricing::aliases::resolve_alias("gemini-3-flash-a"),
            Some("gemini-3.5-flash-high")
        );
    }

    #[test]
    fn output_and_thinking_map_to_fields_9_and_10() {
        // Lock the field-mapping contract asserted by the module doc-comment:
        // `#9 + #10 == #3` (output + thinking == stored total output). Build a
        // synthetic blob where #9=output, #10=thinking, #3=output+thinking and
        // verify the parsed message keeps #9 as output and #10 as reasoning.
        let output = 300u64;
        let thinking = 40u64;
        let total_output = output + thinking; // #3

        let mut usage = Vec::new();
        usage.extend(enc_varint(1, 1132)); // fixed system prompt
        usage.extend(enc_varint(2, 500)); // new input
        usage.extend(enc_varint(3, total_output)); // stored total output (#3)
        usage.extend(enc_varint(9, output)); // output (#9)
        usage.extend(enc_varint(10, thinking)); // thinking (#10)
        usage.extend(enc_len(11, b"invariant-1"));

        let mut chat_model = Vec::new();
        chat_model.extend(enc_len(4, &usage));
        chat_model.extend(enc_len(19, b"gemini-3-flash-a"));
        let blob = enc_len(1, &chat_model);

        let mut seen = HashSet::new();
        let message =
            parse_gen_metadata_default(&blob, "session", 0, &HashMap::new(), &mut seen).unwrap();
        assert_eq!(message.tokens.output, output as i64);
        assert_eq!(message.tokens.reasoning, thinking as i64);
        // The contract: the two component fields sum to the stored total.
        assert_eq!(
            (message.tokens.output + message.tokens.reasoning) as u64,
            total_output
        );
    }

    #[test]
    fn malformed_blob_returns_none_without_panic() {
        let mut seen = HashSet::new();
        // Empty buffer: no chatModel sub-message.
        assert!(parse_gen_metadata_default(&[], "s", 0, &HashMap::new(), &mut seen).is_none());
        // Garbage bytes that do not form a valid wire-format message.
        assert!(parse_gen_metadata_default(
            &[0xff, 0xff, 0xff, 0xff],
            "s",
            0,
            &HashMap::new(),
            &mut seen
        )
        .is_none());
        // A length-delimited #1 whose declared length overruns the buffer:
        // exercises the ProtoReader bounds check (must stop, not index OOB).
        let truncated = [(1u8 << 3) | 2, 0x7f, 0x01, 0x02];
        assert!(
            parse_gen_metadata_default(&truncated, "s", 0, &HashMap::new(), &mut seen).is_none()
        );
        // Valid outer #1 wrapping a #4 usage whose declared length overruns:
        // the inner reader must bail without panicking.
        let inner = [(4u8 << 3) | 2, 0x40, 0x00];
        let mut outer = vec![(1u8 << 3) | 2, inner.len() as u8];
        outer.extend_from_slice(&inner);
        assert!(parse_gen_metadata_default(&outer, "s", 0, &HashMap::new(), &mut seen).is_none());
    }

    #[test]
    fn proto_timestamp_ms_overflow_returns_none_without_panic() {
        // A malformed Timestamp can carry a `seconds` varint whose `* 1000`
        // overflows i64. Debug builds (overflow-checks = on) would panic on the
        // unchecked multiply; the decode must degrade to None instead, matching
        // the module's malformed-data contract.
        let mut overflow = Vec::new();
        overflow.extend(enc_varint(1, i64::MAX as u64)); // seconds -> *1000 overflows
        overflow.extend(enc_varint(2, 0)); // nanos
        assert_eq!(proto_timestamp_ms(&overflow), None);

        // The boundary case: largest `seconds` whose *1000 still fits i64 must
        // decode, proving the guard rejects only genuine overflow.
        let ok_seconds = i64::MAX / 1000;
        let mut ok = Vec::new();
        ok.extend(enc_varint(1, ok_seconds as u64));
        ok.extend(enc_varint(2, 0));
        assert_eq!(proto_timestamp_ms(&ok), Some(ok_seconds * 1000));

        // A normal, in-range stamp still decodes (seconds + nanos -> ms).
        let mut normal = Vec::new();
        normal.extend(enc_varint(1, 1_781_000_000));
        normal.extend(enc_varint(2, 250_000_000)); // +250ms
        assert_eq!(
            proto_timestamp_ms(&normal),
            Some(1_781_000_000 * 1000 + 250)
        );
    }

    #[test]
    fn proto_timestamp_ms_rejects_out_of_range_nanos() {
        // The protobuf Timestamp spec requires `nanos` in 0..=999_999_999.
        // An out-of-range `nanos` marks the stamp malformed (None) rather than
        // producing a skewed time. 1_000_000_000 (== one extra second) is the
        // first invalid value above the inclusive upper bound.
        let mut bad_nanos = Vec::new();
        bad_nanos.extend(enc_varint(1, 1_781_000_000)); // valid seconds
        bad_nanos.extend(enc_varint(2, 1_000_000_000)); // nanos out of range
        assert_eq!(proto_timestamp_ms(&bad_nanos), None);

        // A nanos varint large enough to be negative once cast to i64 is also
        // rejected (never wraps to a bogus negative offset).
        let mut huge_nanos = Vec::new();
        huge_nanos.extend(enc_varint(1, 1_781_000_000));
        huge_nanos.extend(enc_varint(2, u64::MAX));
        assert_eq!(proto_timestamp_ms(&huge_nanos), None);

        // The inclusive upper bound is accepted (999_999_999 ns -> +999 ms).
        let mut max_nanos = Vec::new();
        max_nanos.extend(enc_varint(1, 1_781_000_000));
        max_nanos.extend(enc_varint(2, 999_999_999));
        assert_eq!(
            proto_timestamp_ms(&max_nanos),
            Some(1_781_000_000 * 1000 + 999)
        );

        // End-to-end: a gen_metadata row whose #9.#4 carries out-of-range nanos
        // must fall back to the session-created timestamp (the caller's invalid
        // -> None -> session fallback path), not adopt a skewed per-turn stamp.
        let session_fallback = 222_000_i64;
        let mut usage = Vec::new();
        usage.extend(enc_varint(2, 500)); // input
        usage.extend(enc_varint(9, 300)); // output
        usage.extend(enc_len(11, b"bad-nanos")); // responseId

        let mut gen_time = Vec::new();
        gen_time.extend(enc_varint(1, 1_781_000_000)); // seconds
        gen_time.extend(enc_varint(2, 1_000_000_000)); // nanos out of range
        let gen9 = enc_len(4, &gen_time);

        let mut chat_model = Vec::new();
        chat_model.extend(enc_len(4, &usage));
        chat_model.extend(enc_len(9, &gen9));
        chat_model.extend(enc_len(19, b"gemini-3-flash-a"));
        let blob = enc_len(1, &chat_model);

        let mut seen = HashSet::new();
        let message =
            parse_gen_metadata_default(&blob, "s", session_fallback, &HashMap::new(), &mut seen)
                .unwrap();
        assert_eq!(
            message.timestamp, session_fallback,
            "out-of-range per-generation nanos must fall back to the session timestamp"
        );
    }

    #[test]
    fn file_uri_to_path_handles_windows_posix_and_unc() {
        // Empty authority + Windows drive: drop the slash before the drive.
        assert_eq!(
            file_uri_to_path("file:///C:/Users/Frank/obsidian-vault").as_deref(),
            Some("C:/Users/Frank/obsidian-vault")
        );
        // Empty authority + POSIX absolute: keep as-is.
        assert_eq!(
            file_uri_to_path("file:///home/frank/project").as_deref(),
            Some("/home/frank/project")
        );
        // Non-empty authority is a UNC path; the host must survive as `//host`.
        assert_eq!(
            file_uri_to_path("file://server/share/code").as_deref(),
            Some("//server/share/code")
        );
        // Percent-encoded UTF-8 (CJK) decodes to valid characters.
        assert_eq!(
            file_uri_to_path("file:///D:/%E6%88%91%E7%9A%84").as_deref(),
            Some("D:/我的")
        );
        // Anything without the scheme prefix is rejected.
        assert_eq!(file_uri_to_path("not-a-file-uri"), None);
    }

    #[test]
    fn native_timestamp_takes_precedence_over_step_and_session() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("native-precedence.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE gen_metadata (idx integer, data blob, size integer);
             CREATE TABLE trajectory_metadata_blob (id text, data blob);
             CREATE TABLE steps (idx integer, step_type integer, metadata blob);",
        )
        .unwrap();

        let native_sec = 1_783_000_000u64;
        let native_nano = 250_000_000u64;
        let expected_native_ms = 1_783_000_000 * 1000 + 250;

        let step_sec = 1_782_000_000u64;
        let step_nano = 100_000_000u64;

        conn.execute(
            "INSERT INTO gen_metadata (idx, data, size) VALUES (0, ?1, 0)",
            params![build_gen_metadata_full(
                "gemini-3-flash-a",
                Some(b"resp-1"),
                Some((native_sec, native_nano))
            )],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO trajectory_metadata_blob (id, data) VALUES ('main', ?1)",
            params![build_trajectory_meta()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO steps (idx, step_type, metadata) VALUES (0, 15, ?1)",
            params![build_step_metadata(b"resp-1", step_sec, step_nano)],
        )
        .unwrap();
        drop(conn);

        let messages = parse_antigravity_cli_file(&path);
        assert_eq!(messages.len(), 1);
        let msg = &messages[0];
        assert_eq!(msg.timestamp, expected_native_ms);
        assert_standard_message_fields(msg, Some("resp-1"));
    }

    #[test]
    fn steps_join_succeeds_when_native_timestamp_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("steps-join.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE gen_metadata (idx integer, data blob, size integer);
             CREATE TABLE trajectory_metadata_blob (id text, data blob);
             CREATE TABLE steps (idx integer, step_type integer, metadata blob);",
        )
        .unwrap();

        let step_sec = 1_782_000_000u64;
        let step_nano = 100_000_000u64;
        let expected_step_ms = 1_782_000_000 * 1000 + 100;

        conn.execute(
            "INSERT INTO gen_metadata (idx, data, size) VALUES (0, ?1, 0)",
            params![build_gen_metadata_full(
                "gemini-3-flash-a",
                Some(b"resp-1"),
                None
            )],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO trajectory_metadata_blob (id, data) VALUES ('main', ?1)",
            params![build_trajectory_meta()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO steps (idx, step_type, metadata) VALUES (0, 15, ?1)",
            params![build_step_metadata(b"resp-1", step_sec, step_nano)],
        )
        .unwrap();
        drop(conn);

        let messages = parse_antigravity_cli_file(&path);
        assert_eq!(messages.len(), 1);
        let msg = &messages[0];
        assert_eq!(msg.timestamp, expected_step_ms);
        assert_standard_message_fields(msg, Some("resp-1"));
    }

    #[test]
    fn join_fallback_when_generation_has_no_response_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("no-response-id.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE gen_metadata (idx integer, data blob, size integer);
             CREATE TABLE trajectory_metadata_blob (id text, data blob);
             CREATE TABLE steps (idx integer, step_type integer, metadata blob);",
        )
        .unwrap();

        conn.execute(
            "INSERT INTO gen_metadata (idx, data, size) VALUES (0, ?1, 0)",
            params![build_gen_metadata_full("gemini-3-flash-a", None, None)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO trajectory_metadata_blob (id, data) VALUES ('main', ?1)",
            params![build_trajectory_meta()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO steps (idx, step_type, metadata) VALUES (0, 15, ?1)",
            params![build_step_metadata(b"resp-1", 1_782_000_000, 0)],
        )
        .unwrap();
        drop(conn);

        let messages = parse_antigravity_cli_file(&path);
        assert_eq!(messages.len(), 1);
        let msg = &messages[0];
        assert_eq!(msg.timestamp, 1_781_502_653_000);
        assert_standard_message_fields(msg, None);
    }

    #[test]
    fn join_fallback_when_multiple_steps_match_response_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ambiguous-steps.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE gen_metadata (idx integer, data blob, size integer);
             CREATE TABLE trajectory_metadata_blob (id text, data blob);
             CREATE TABLE steps (idx integer, step_type integer, metadata blob);",
        )
        .unwrap();

        conn.execute(
            "INSERT INTO gen_metadata (idx, data, size) VALUES (0, ?1, 0)",
            params![build_gen_metadata_full(
                "gemini-3-flash-a",
                Some(b"resp-1"),
                None
            )],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO trajectory_metadata_blob (id, data) VALUES ('main', ?1)",
            params![build_trajectory_meta()],
        )
        .unwrap();
        // Two steps with same responseId (different timestamps) -> ambiguous
        conn.execute(
            "INSERT INTO steps (idx, step_type, metadata) VALUES (0, 15, ?1)",
            params![build_step_metadata(b"resp-1", 1_782_000_000, 0)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO steps (idx, step_type, metadata) VALUES (1, 15, ?1)",
            params![build_step_metadata(b"resp-1", 1_782_500_000, 0)],
        )
        .unwrap();
        drop(conn);

        let messages = parse_antigravity_cli_file(&path);
        assert_eq!(messages.len(), 1);
        let msg = &messages[0];
        assert_eq!(msg.timestamp, 1_781_502_653_000);
        assert_standard_message_fields(msg, Some("resp-1"));
    }

    #[test]
    fn join_fallback_when_multiple_steps_have_identical_timestamps() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identical-ambiguous-steps.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE gen_metadata (idx integer, data blob, size integer);
             CREATE TABLE trajectory_metadata_blob (id text, data blob);
             CREATE TABLE steps (idx integer, step_type integer, metadata blob);",
        )
        .unwrap();

        conn.execute(
            "INSERT INTO gen_metadata (idx, data, size) VALUES (0, ?1, 0)",
            params![build_gen_metadata_full(
                "gemini-3-flash-a",
                Some(b"resp-1"),
                None
            )],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO trajectory_metadata_blob (id, data) VALUES ('main', ?1)",
            params![build_trajectory_meta()],
        )
        .unwrap();
        // Two steps with same responseId and identical timestamps -> ambiguous
        conn.execute(
            "INSERT INTO steps (idx, step_type, metadata) VALUES (0, 15, ?1)",
            params![build_step_metadata(b"resp-1", 1_782_000_000, 0)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO steps (idx, step_type, metadata) VALUES (1, 15, ?1)",
            params![build_step_metadata(b"resp-1", 1_782_000_000, 0)],
        )
        .unwrap();
        drop(conn);

        let messages = parse_antigravity_cli_file(&path);
        assert_eq!(messages.len(), 1);
        let msg = &messages[0];
        assert_eq!(msg.timestamp, 1_781_502_653_000);
        assert_standard_message_fields(msg, Some("resp-1"));
    }

    #[test]
    fn join_fallback_when_step_timestamp_is_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("invalid-step-time.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE gen_metadata (idx integer, data blob, size integer);
             CREATE TABLE trajectory_metadata_blob (id text, data blob);
             CREATE TABLE steps (idx integer, step_type integer, metadata blob);",
        )
        .unwrap();

        conn.execute(
            "INSERT INTO gen_metadata (idx, data, size) VALUES (0, ?1, 0)",
            params![build_gen_metadata_full(
                "gemini-3-flash-a",
                Some(b"resp-1"),
                None
            )],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO trajectory_metadata_blob (id, data) VALUES ('main', ?1)",
            params![build_trajectory_meta()],
        )
        .unwrap();

        // Step timestamp carries out-of-range nanos (1_000_000_000)
        let mut bad_ts = Vec::new();
        bad_ts.extend(enc_varint(1, 1_782_000_000));
        bad_ts.extend(enc_varint(2, 1_000_000_000));
        conn.execute(
            "INSERT INTO steps (idx, step_type, metadata) VALUES (0, 15, ?1)",
            params![build_step_metadata_raw(Some(b"resp-1"), Some(&bad_ts))],
        )
        .unwrap();
        drop(conn);

        let messages = parse_antigravity_cli_file(&path);
        assert_eq!(messages.len(), 1);
        let msg = &messages[0];
        assert_eq!(msg.timestamp, 1_781_502_653_000);
        assert_standard_message_fields(msg, Some("resp-1"));
    }

    #[test]
    fn join_fallback_when_steps_table_missing_or_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing-steps.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE gen_metadata (idx integer, data blob, size integer);
             CREATE TABLE trajectory_metadata_blob (id text, data blob);",
        )
        .unwrap();

        conn.execute(
            "INSERT INTO gen_metadata (idx, data, size) VALUES (0, ?1, 0)",
            params![build_gen_metadata_full(
                "gemini-3-flash-a",
                Some(b"resp-1"),
                None
            )],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO trajectory_metadata_blob (id, data) VALUES ('main', ?1)",
            params![build_trajectory_meta()],
        )
        .unwrap();
        drop(conn);

        let messages = parse_antigravity_cli_file(&path);
        assert_eq!(messages.len(), 1);
        let msg = &messages[0];
        assert_eq!(msg.timestamp, 1_781_502_653_000);
        assert_standard_message_fields(msg, Some("resp-1"));
    }

    #[test]
    fn read_step_timestamps_filters_and_marks_ambiguous() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE steps (idx integer, step_type integer, metadata blob);")
            .unwrap();

        // 1. Valid step_type=15
        conn.execute(
            "INSERT INTO steps (idx, step_type, metadata) VALUES (0, 15, ?1)",
            params![build_step_metadata(b"key-unique", 1_782_000_000, 0)],
        )
        .unwrap();

        // 2. step_type != 15 should be ignored
        conn.execute(
            "INSERT INTO steps (idx, step_type, metadata) VALUES (1, 14, ?1)",
            params![build_step_metadata(b"key-ignored-type", 1_782_000_000, 0)],
        )
        .unwrap();

        // 3. metadata NULL should be ignored
        conn.execute(
            "INSERT INTO steps (idx, step_type, metadata) VALUES (2, 15, NULL)",
            [],
        )
        .unwrap();

        // 4. Duplicate keys become Ambiguous
        conn.execute(
            "INSERT INTO steps (idx, step_type, metadata) VALUES (3, 15, ?1)",
            params![build_step_metadata(b"key-ambig", 1_782_000_000, 0)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO steps (idx, step_type, metadata) VALUES (4, 15, ?1)",
            params![build_step_metadata(b"key-ambig", 1_782_100_000, 0)],
        )
        .unwrap();

        let map = read_step_timestamps(&conn);
        assert_eq!(
            map.get(b"key-unique".as_slice()),
            Some(&StepTimestamp::Unique(1_782_000_000_000))
        );
        assert_eq!(map.get(b"key-ignored-type".as_slice()), None);
        assert_eq!(
            map.get(b"key-ambig".as_slice()),
            Some(&StepTimestamp::Ambiguous)
        );
    }

    #[test]
    fn log_join_succeeds_when_native_and_steps_absent() {
        let dir = tempfile::tempdir().unwrap();
        let conv_dir = dir.path().join("conversations");
        let log_dir = dir.path().join("log");
        std::fs::create_dir_all(&conv_dir).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();

        let db_path = conv_dir.join("session-log-join.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE gen_metadata (idx integer, data blob, size integer);
             CREATE TABLE trajectory_metadata_blob (id text, data blob);",
        )
        .unwrap();

        // Row without native timestamp (#9.#4)
        conn.execute(
            "INSERT INTO gen_metadata (idx, data, size) VALUES (0, ?1, 0)",
            params![build_gen_metadata_full(
                "gemini-3-flash-a",
                Some(b"resp-1"),
                None
            )],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO trajectory_metadata_blob (id, data) VALUES ('main', ?1)",
            params![build_trajectory_meta()],
        )
        .unwrap();
        drop(conn);

        // Write log file: cli-20260907_055144.log
        let log_path = log_dir.join("cli-20260907_055144.log");
        std::fs::write(
            &log_path,
            "ERROR: logging before google.Init: I0907 05:51:58.547986     135 http_helpers.go:296] URL: https://example.com Trace: 0x123 ResponseID: resp-1\n",
        )
        .unwrap();

        let ctx = build_timestamp_context(std::slice::from_ref(&db_path));
        let messages = parse_antigravity_cli_file_with_context(&db_path, &ctx);
        assert_eq!(messages.len(), 1);
        let msg = &messages[0];

        let expected_utc_ms = {
            let naive = chrono::NaiveDate::from_ymd_opt(2026, 9, 7)
                .unwrap()
                .and_hms_micro_opt(5, 51, 58, 547986)
                .unwrap();
            chrono::TimeZone::from_local_datetime(&chrono::Local, &naive)
                .single()
                .unwrap()
                .timestamp_millis()
        };
        assert_eq!(msg.timestamp, expected_utc_ms);
        assert_standard_message_fields(msg, Some("resp-1"));
    }

    #[test]
    fn log_fallback_when_log_ambiguous_two_lines() {
        let dir = tempfile::tempdir().unwrap();
        let conv_dir = dir.path().join("conversations");
        let log_dir = dir.path().join("log");
        std::fs::create_dir_all(&conv_dir).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();

        let db_path = conv_dir.join("session-ambig.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE gen_metadata (idx integer, data blob, size integer);
             CREATE TABLE trajectory_metadata_blob (id text, data blob);",
        )
        .unwrap();

        conn.execute(
            "INSERT INTO gen_metadata (idx, data, size) VALUES (0, ?1, 0)",
            params![build_gen_metadata_full(
                "gemini-3-flash-a",
                Some(b"resp-1"),
                None
            )],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO trajectory_metadata_blob (id, data) VALUES ('main', ?1)",
            params![build_trajectory_meta()],
        )
        .unwrap();
        drop(conn);

        // Two lines for resp-1 -> ambiguous
        let log_path = log_dir.join("cli-20260907_055144.log");
        std::fs::write(
            &log_path,
            "I0907 05:51:58.547986 135 http_helpers.go:296] ResponseID: resp-1\nI0907 05:52:00.123456 135 http_helpers.go:296] ResponseID: resp-1\n",
        )
        .unwrap();

        let ctx = build_timestamp_context(std::slice::from_ref(&db_path));
        let messages = parse_antigravity_cli_file_with_context(&db_path, &ctx);
        assert_eq!(messages.len(), 1);
        let msg = &messages[0];
        // Falls back to session created timestamp
        assert_eq!(msg.timestamp, 1_781_502_653_000);
        assert_standard_message_fields(msg, Some("resp-1"));
    }

    #[test]
    fn log_fallback_when_log_missing_or_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let conv_dir = dir.path().join("conversations");
        let log_dir = dir.path().join("log");
        std::fs::create_dir_all(&conv_dir).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();

        let db_path = conv_dir.join("session-corrupt.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE gen_metadata (idx integer, data blob, size integer);
             CREATE TABLE trajectory_metadata_blob (id text, data blob);",
        )
        .unwrap();

        conn.execute(
            "INSERT INTO gen_metadata (idx, data, size) VALUES (0, ?1, 0)",
            params![build_gen_metadata_full(
                "gemini-3-flash-a",
                Some(b"resp-1"),
                None
            )],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO trajectory_metadata_blob (id, data) VALUES ('main', ?1)",
            params![build_trajectory_meta()],
        )
        .unwrap();
        drop(conn);

        // Corrupt log content (cannot parse timestamp)
        let log_path = log_dir.join("cli-20260907_055144.log");
        std::fs::write(
            &log_path,
            "Garbage log line without header ResponseID: resp-1\n",
        )
        .unwrap();

        let ctx = build_timestamp_context(std::slice::from_ref(&db_path));
        let messages = parse_antigravity_cli_file_with_context(&db_path, &ctx);
        assert_eq!(messages.len(), 1);
        let msg = &messages[0];
        // Falls back to session created timestamp
        assert_eq!(msg.timestamp, 1_781_502_653_000);
        assert_standard_message_fields(msg, Some("resp-1"));
    }

    #[test]
    fn log_fallback_when_cross_db_duplicate_rid() {
        let dir = tempfile::tempdir().unwrap();
        let conv_dir = dir.path().join("conversations");
        let log_dir = dir.path().join("log");
        std::fs::create_dir_all(&conv_dir).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();

        let db1_path = conv_dir.join("session-1.db");
        let conn1 = Connection::open(&db1_path).unwrap();
        conn1
            .execute_batch(
                "CREATE TABLE gen_metadata (idx integer, data blob, size integer);
                 CREATE TABLE trajectory_metadata_blob (id text, data blob);",
            )
            .unwrap();
        conn1
            .execute(
                "INSERT INTO gen_metadata (idx, data, size) VALUES (0, ?1, 0)",
                params![build_gen_metadata_full(
                    "gemini-3-flash-a",
                    Some(b"resp-1"),
                    None
                )],
            )
            .unwrap();
        conn1
            .execute(
                "INSERT INTO trajectory_metadata_blob (id, data) VALUES ('main', ?1)",
                params![build_trajectory_meta()],
            )
            .unwrap();
        drop(conn1);

        let db2_path = conv_dir.join("session-2.db");
        let conn2 = Connection::open(&db2_path).unwrap();
        conn2
            .execute_batch(
                "CREATE TABLE gen_metadata (idx integer, data blob, size integer);
                 CREATE TABLE trajectory_metadata_blob (id text, data blob);",
            )
            .unwrap();
        conn2
            .execute(
                "INSERT INTO gen_metadata (idx, data, size) VALUES (0, ?1, 0)",
                params![build_gen_metadata_full(
                    "gemini-3-flash-a",
                    Some(b"resp-1"),
                    None
                )],
            )
            .unwrap();
        conn2
            .execute(
                "INSERT INTO trajectory_metadata_blob (id, data) VALUES ('main', ?1)",
                params![build_trajectory_meta()],
            )
            .unwrap();
        drop(conn2);

        // One log entry for resp-1
        let log_path = log_dir.join("cli-20260907_055144.log");
        std::fs::write(
            &log_path,
            "I0907 05:51:58.547986 135 http_helpers.go:296] ResponseID: resp-1\n",
        )
        .unwrap();

        let ctx = build_timestamp_context(&[db1_path.clone(), db2_path.clone()]);
        assert!(ctx.colliding_response_ids.contains("resp-1"));

        // Both DBs must NOT adopt log time (both fall back to session timestamp)
        let messages1 = parse_antigravity_cli_file_with_context(&db1_path, &ctx);
        assert_eq!(messages1.len(), 1);
        let msg1 = &messages1[0];
        assert_eq!(msg1.timestamp, 1_781_502_653_000);
        assert_standard_message_fields(msg1, Some("resp-1"));

        let messages2 = parse_antigravity_cli_file_with_context(&db2_path, &ctx);
        assert_eq!(messages2.len(), 1);
        let msg2 = &messages2[0];
        assert_eq!(msg2.timestamp, 1_781_502_653_000);
        assert_standard_message_fields(msg2, Some("resp-1"));
    }

    #[test]
    fn log_scan_skips_multibyte_utf8_log_filename_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let conv_dir = dir.path().join("conversations");
        let log_dir = dir.path().join("log");
        std::fs::create_dir_all(&conv_dir).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();

        let db_path = conv_dir.join("session-multibyte.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE gen_metadata (idx integer, data blob, size integer);
             CREATE TABLE trajectory_metadata_blob (id text, data blob);",
        )
        .unwrap();

        conn.execute(
            "INSERT INTO gen_metadata (idx, data, size) VALUES (0, ?1, 0)",
            params![build_gen_metadata_full(
                "gemini-3-flash-a",
                Some(b"resp-1"),
                None
            )],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO trajectory_metadata_blob (id, data) VALUES ('main', ?1)",
            params![build_trajectory_meta()],
        )
        .unwrap();
        drop(conn);

        let log_path = log_dir.join("cli-日本語.log");
        std::fs::write(
            &log_path,
            "I0907 05:51:58.547986 135 http_helpers.go:296] ResponseID: resp-1\n",
        )
        .unwrap();

        let ctx = build_timestamp_context(std::slice::from_ref(&db_path));
        let messages = parse_antigravity_cli_file_with_context(&db_path, &ctx);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].timestamp, 1_781_502_653_000);
        assert_standard_message_fields(&messages[0], Some("resp-1"));
    }
}
