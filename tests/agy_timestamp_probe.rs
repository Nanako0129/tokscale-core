use rusqlite::Connection;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokscale_core::sessions::antigravity_cli::{
    build_timestamp_context, parse_antigravity_cli_file_with_context,
    AntigravityCliTimestampContext,
};
use tokscale_core::{TokenBreakdown, UnifiedMessage};

/// Saturating per-message token total. The parser clamps oversized token fields
/// to i64::MAX, so a plain `+` accumulator would overflow (panic in debug).
fn accumulate_tokens(acc: i64, tokens: &TokenBreakdown) -> i64 {
    acc.saturating_add(tokens.total())
}

#[test]
fn token_accumulator_saturates_on_clamped_buckets() {
    // The parser clamps oversized buckets to i64::MAX; summing them must not
    // overflow (debug builds panic on overflow).
    let clamped = TokenBreakdown {
        input: i64::MAX,
        output: i64::MAX,
        cache_read: i64::MAX,
        cache_write: i64::MAX,
        reasoning: i64::MAX,
        cache_write_1h: 0,
    };
    assert_eq!(clamped.total(), i64::MAX);
    let once = accumulate_tokens(0, &clamped);
    assert_eq!(once, i64::MAX);
    assert_eq!(accumulate_tokens(once, &clamped), i64::MAX);
}

#[test]
#[ignore]
fn real_antigravity_cli_timestamp_probe() {
    let Ok(home_str) = std::env::var("TOKENBAR_AGY_REAL_HOME") else {
        return;
    };
    let trimmed = home_str.trim();
    if trimmed.is_empty() {
        return;
    }

    let home_path = PathBuf::from(trimmed);
    let conv_dir = if home_path.ends_with("conversations") {
        home_path
    } else if home_path.ends_with("antigravity-cli") {
        home_path.join("conversations")
    } else if home_path.ends_with(".gemini") {
        home_path.join("antigravity-cli").join("conversations")
    } else {
        home_path
            .join(".gemini")
            .join("antigravity-cli")
            .join("conversations")
    };

    if !conv_dir.is_dir() {
        return;
    }

    let Ok(entries) = std::fs::read_dir(&conv_dir) else {
        return;
    };
    let mut conversation_paths = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some("db") {
            conversation_paths.push(path);
        }
    }
    conversation_paths.sort();
    if conversation_paths.is_empty() {
        return;
    }

    let context = build_timestamp_context(&conversation_paths);
    let default_context = AntigravityCliTimestampContext::default();

    let from_ms = chrono::NaiveDate::from_ymd_opt(2026, 9, 16)
        .unwrap()
        .and_hms_opt(17, 8, 21)
        .unwrap()
        .and_utc()
        .timestamp_millis();
    let until_ms = chrono::NaiveDate::from_ymd_opt(2026, 9, 16)
        .unwrap()
        .and_hms_opt(22, 8, 21)
        .unwrap()
        .and_utc()
        .timestamp_millis();

    let mut total: u64 = 0;
    let mut native_count: u64 = 0;
    let mut steps_count: u64 = 0;
    let mut log_count: u64 = 0;
    let mut session_count: u64 = 0;

    let mut legacy_session_tokens: i64 = 0;
    let mut corrected_tokens: i64 = 0;

    for path in &conversation_paths {
        let (session_ts, step_timestamps) = read_probe_db_meta(path);
        let messages: Vec<UnifiedMessage> = parse_antigravity_cli_file_with_context(path, &context);
        let fallback_messages: Vec<UnifiedMessage> =
            parse_antigravity_cli_file_with_context(path, &default_context);

        // Also inspect rows directly to attribute native layer sources
        let native_map = read_probe_native_timestamps(path);

        for (i, msg) in messages.iter().enumerate() {
            total += 1;

            let fallback_ts = fallback_messages.get(i).map(|m| m.timestamp);
            let rid = msg.dedup_key.as_deref().unwrap_or("");
            let has_native = !rid.is_empty() && native_map.get(rid).copied().unwrap_or(false);
            let has_steps = !rid.is_empty()
                && step_timestamps
                    .get(rid.as_bytes())
                    .copied()
                    .unwrap_or(false);

            if msg.timestamp != fallback_ts.unwrap_or(session_ts) {
                // Production parser timestamp differs from no-log fallback -> sourced from log
                log_count += 1;
            } else if has_native {
                native_count += 1;
            } else if has_steps {
                steps_count += 1;
            } else {
                session_count += 1;
            }

            if msg.timestamp >= from_ms && msg.timestamp < until_ms {
                corrected_tokens = accumulate_tokens(corrected_tokens, &msg.tokens);
            }

            if session_ts >= from_ms && session_ts < until_ms {
                legacy_session_tokens = accumulate_tokens(legacy_session_tokens, &msg.tokens);
            }
        }
    }

    let delta = corrected_tokens - legacy_session_tokens;

    println!(
        "AGY_TS_COVERAGE total={} native={} steps={} log={} session={}",
        total, native_count, steps_count, log_count, session_count
    );
    println!(
        "AGY_WINDOW from=2026-09-16T17:08:21Z until=2026-09-16T22:08:21Z legacy_session_tokens={} corrected_tokens={} delta={}",
        legacy_session_tokens, corrected_tokens, delta
    );
}

fn read_probe_db_meta(path: &Path) -> (i64, HashMap<Vec<u8>, bool>) {
    let Ok(conn) = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
    else {
        return (0, HashMap::new());
    };

    let session_ts = conn
        .query_row(
            "SELECT data FROM trajectory_metadata_blob LIMIT 1",
            [],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .ok()
        .and_then(|blob| proto_probe_timestamp_ms(&blob, 2))
        .filter(|&ms| ms > 0)
        .unwrap_or_else(|| {
            std::fs::metadata(path)
                .and_then(|m| m.modified())
                .ok()
                .map(|t| chrono::DateTime::<chrono::Utc>::from(t).timestamp_millis())
                .unwrap_or(0)
        });

    let mut step_timestamps = HashMap::new();
    if let Ok(mut stmt) = conn.prepare("SELECT step_type, metadata FROM steps") {
        if let Ok(rows) = stmt.query_map([], |row| {
            let step_type: Option<i64> = row.get(0).ok();
            let metadata: Option<Vec<u8>> = row.get(1).ok();
            Ok((step_type, metadata))
        }) {
            let mut counts: HashMap<Vec<u8>, (usize, bool)> = HashMap::new();
            for row in rows.flatten() {
                let (Some(15), Some(metadata)) = row else {
                    continue;
                };
                let Some(key) = probe_field(&metadata, 9)
                    .and_then(|m9| probe_field(m9, 11))
                    .filter(|k| !k.is_empty())
                else {
                    continue;
                };
                let has_valid_ts = probe_field(&metadata, 1)
                    .and_then(probe_proto_timestamp)
                    .is_some_and(|ms| ms > 0);
                let entry = counts.entry(key.to_vec()).or_insert((0, false));
                entry.0 += 1;
                if has_valid_ts {
                    entry.1 = true;
                }
            }
            for (key, (count, valid)) in counts {
                if count == 1 && valid {
                    step_timestamps.insert(key, true);
                }
            }
        }
    }

    (session_ts, step_timestamps)
}

fn read_probe_native_timestamps(path: &Path) -> HashMap<String, bool> {
    let mut map = HashMap::new();
    let Ok(conn) = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
    else {
        return map;
    };
    let Ok(mut stmt) = conn.prepare("SELECT data FROM gen_metadata ORDER BY idx") else {
        return map;
    };
    let Ok(rows) = stmt.query_map([], |row| row.get::<_, Vec<u8>>(0)) else {
        return map;
    };
    for blob in rows.flatten() {
        let Some(chat_model) = probe_field(&blob, 1) else {
            continue;
        };
        let Some(usage) = probe_field(chat_model, 4) else {
            continue;
        };
        let Some(rid_bytes) = probe_field(usage, 11) else {
            continue;
        };
        let Ok(rid) = std::str::from_utf8(rid_bytes) else {
            continue;
        };
        let trimmed = rid.trim();
        if trimmed.is_empty() {
            continue;
        }

        let has_native = probe_field(chat_model, 9)
            .and_then(|gen| probe_field(gen, 4))
            .and_then(probe_proto_timestamp)
            .is_some_and(|ms| ms > 0);

        map.insert(trimmed.to_string(), has_native);
    }
    map
}

fn probe_field(data: &[u8], target_field: u32) -> Option<&[u8]> {
    let mut offset = 0;
    while offset < data.len() {
        let (key, consumed) = read_probe_varint(&data[offset..])?;
        offset += consumed;
        let field_num = (key >> 3) as u32;
        let wire_type = key & 0x07;
        match wire_type {
            0 => {
                let (_, c) = read_probe_varint(&data[offset..])?;
                offset += c;
            }
            2 => {
                let (len, c) = read_probe_varint(&data[offset..])?;
                offset += c;
                let len = usize::try_from(len).ok()?;
                let end = offset.checked_add(len)?;
                if end > data.len() {
                    return None;
                }
                if field_num == target_field {
                    return Some(&data[offset..end]);
                }
                offset = end;
            }
            _ => return None,
        }
    }
    None
}

fn read_probe_varint(data: &[u8]) -> Option<(u64, usize)> {
    let mut value = 0u64;
    let mut shift = 0;
    for (i, &byte) in data.iter().enumerate().take(10) {
        value |= ((byte & 0x7F) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some((value, i + 1));
        }
        shift += 7;
    }
    None
}

fn probe_proto_timestamp(ts: &[u8]) -> Option<i64> {
    let mut offset = 0;
    let mut seconds = None;
    let mut nanos = 0i64;
    while offset < ts.len() {
        let (key, c) = read_probe_varint(&ts[offset..])?;
        offset += c;
        let field_num = (key >> 3) as u32;
        let wire_type = key & 0x07;
        if wire_type == 0 {
            let (val, vc) = read_probe_varint(&ts[offset..])?;
            offset += vc;
            if field_num == 1 {
                seconds = Some(val as i64);
            } else if field_num == 2 {
                nanos = i64::try_from(val).ok()?;
            }
        } else {
            return None;
        }
    }
    if !(0..=999_999_999).contains(&nanos) {
        return None;
    }
    let sec = seconds?;
    sec.checked_mul(1000)?.checked_add(nanos / 1_000_000)
}

fn proto_probe_timestamp_ms(blob: &[u8], target_field: u32) -> Option<i64> {
    let ts_bytes = probe_field(blob, target_field)?;
    probe_proto_timestamp(ts_bytes)
}
