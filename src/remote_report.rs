use std::{
    cmp::Ordering,
    collections::{BTreeMap, HashSet},
    fmt,
};

use jiff::{civil::Date, tz::TimeZone, Timestamp};
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::{
    canonical_model_id, report_message_client_passes,
    sessions::{normalize_agent_name, normalize_copilot_agent_name},
    UnifiedMessage,
};

pub const REMOTE_TZDB_REVISION: &str = "2026c";

const MAX_MESSAGES: usize = 262_144;
const MAX_CLIENTS: usize = 128;
const MAX_TEXT_BYTES: usize = 255;
const MAX_QUERY_CLIENT_BYTES: usize = 16_384;
const MAX_RANGE_DAYS: i32 = 370;
const MAX_LABEL_BYTES: usize = 8 * 1024 * 1024;
const MAX_RECORDS: usize = 262_144;
const MAX_OUTPUT_STRING_BYTES: usize = 8 * 1024 * 1024;
const MAX_COST_NANO_USD: f64 = 18_446_744_073_709_551_616.0;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteUsageQueryV1 {
    pub clients: Vec<String>,
    pub start_date: String,
    pub end_date_exclusive: String,
    pub timezone: String,
    pub tzdb_revision: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteUsageBundleV1 {
    pub tzdb_revision: String,
    pub graph: Vec<RemoteGraphRecordV1>,
    pub models: Vec<RemoteModelsRecordV1>,
    pub hourly: Vec<RemoteHourlyRecordV1>,
    pub agents: Vec<RemoteAgentsRecordV1>,
    pub saturated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteGraphRecordV1 {
    pub date: String,
    pub client: String,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
    pub message_count: u64,
    pub turn_count: u64,
    pub cost_nano_usd: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteModelsRecordV1 {
    pub client: String,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
    pub message_count: u64,
    pub turn_count: u64,
    pub cost_nano_usd: u64,
    pub duration_millis: u64,
    pub timed_tokens: u64,
    pub sample_count: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteHourlyRecordV1 {
    pub bucket_start_unix_ms: i64,
    pub utc_offset_seconds: i32,
    pub client: String,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
    pub message_count: u64,
    pub turn_count: u64,
    pub cost_nano_usd: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteAgentsRecordV1 {
    pub agent: String,
    pub client: String,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
    pub message_count: u64,
    pub turn_count: u64,
    pub cost_nano_usd: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoteUsageError {
    InvalidQuery,
    IncompatibleTzdb,
    InvalidTimeZone,
    InvalidTimestamp,
    InvalidText,
    InvalidNumerator,
    LimitExceeded,
}

impl fmt::Display for RemoteUsageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidQuery => "invalid query",
            Self::IncompatibleTzdb => "incompatible tzdb",
            Self::InvalidTimeZone => "invalid timezone",
            Self::InvalidTimestamp => "invalid timestamp",
            Self::InvalidText => "invalid text",
            Self::InvalidNumerator => "invalid numerator",
            Self::LimitExceeded => "limit exceeded",
        })
    }
}

impl std::error::Error for RemoteUsageError {}

#[derive(Clone, Copy, Default)]
struct Numerators {
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    reasoning_tokens: u64,
    message_count: u64,
    turn_count: u64,
    cost_nano_usd: u64,
    duration_millis: Option<u64>,
}

#[derive(Clone)]
struct PreparedMessage {
    date_text: String,
    client: String,
    model: Option<String>,
    provider: Option<String>,
    agent: String,
    bucket_start_unix_ms: i64,
    utc_offset_seconds: i32,
    numerators: Numerators,
}

#[derive(Clone, Copy, Default)]
struct CommonAccumulator {
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    reasoning_tokens: u64,
    message_count: u64,
    turn_count: u64,
    cost_nano_usd: u64,
}

#[derive(Clone, Copy, Default)]
struct ModelAccumulator {
    common: CommonAccumulator,
    duration_millis: u64,
    timed_tokens: u64,
    sample_count: u64,
}

type GraphKey = (String, String, Option<String>, Option<String>);
type ModelKey = (String, Option<String>, Option<String>);
type HourlyKey = (i64, i32, String, Option<String>, Option<String>);
type AgentKey = (String, String, Option<String>, Option<String>);

pub fn aggregate_remote_usage_v1(
    messages: &[UnifiedMessage],
    query: &RemoteUsageQueryV1,
) -> Result<RemoteUsageBundleV1, RemoteUsageError> {
    let mut fold = RemoteUsageFold::new(query)?;
    if messages.len() > MAX_MESSAGES {
        return Err(RemoteUsageError::LimitExceeded);
    }
    for message in messages {
        fold.push(message)?;
    }
    fold.finish()
}

pub(crate) struct RemoteUsageFold {
    exact_clients: Option<HashSet<String>>,
    timezone: TimeZone,
    start_date: Date,
    end_date_exclusive: Date,
    accumulator: RemoteUsageAccumulator,
}

impl RemoteUsageFold {
    pub(crate) fn new(query: &RemoteUsageQueryV1) -> Result<Self, RemoteUsageError> {
        let (start_date, end_date_exclusive) = validate_query(query)?;
        let timezone = bundled_timezone(query)?;
        let exact_clients = if query.clients.is_empty() {
            None
        } else {
            Some(query.clients.iter().cloned().collect())
        };
        Ok(Self {
            exact_clients,
            timezone,
            start_date,
            end_date_exclusive,
            accumulator: RemoteUsageAccumulator::default(),
        })
    }

    pub(crate) fn push(&mut self, message: &UnifiedMessage) -> Result<(), RemoteUsageError> {
        self.accumulator.add(
            message,
            &self.exact_clients,
            &self.timezone,
            self.start_date,
            self.end_date_exclusive,
        )
    }

    pub(crate) fn finish(self) -> Result<RemoteUsageBundleV1, RemoteUsageError> {
        self.accumulator.finish()
    }
}

#[derive(Default)]
struct RemoteUsageAccumulator {
    messages: usize,
    label_bytes: usize,
    graph: BTreeMap<GraphKey, CommonAccumulator>,
    models: BTreeMap<ModelKey, ModelAccumulator>,
    hourly: BTreeMap<HourlyKey, CommonAccumulator>,
    agents: BTreeMap<AgentKey, CommonAccumulator>,
    saturated: bool,
}

impl RemoteUsageAccumulator {
    fn add(
        &mut self,
        message: &UnifiedMessage,
        exact_clients: &Option<HashSet<String>>,
        timezone: &TimeZone,
        start_date: Date,
        end_date_exclusive: Date,
    ) -> Result<(), RemoteUsageError> {
        self.messages = self
            .messages
            .checked_add(1)
            .ok_or(RemoteUsageError::LimitExceeded)?;
        if self.messages > MAX_MESSAGES {
            return Err(RemoteUsageError::LimitExceeded);
        }
        validate_message_timestamp(message.timestamp)?;
        let timestamp = timestamp_from_millisecond(message.timestamp)?;
        let local_datetime = timezone.to_datetime(timestamp);
        let local_date = local_datetime.date();
        let date_text = local_date.to_string();
        let offset = timezone.to_offset(timestamp).seconds();
        let bucket = bucket_start(
            timezone,
            timestamp,
            local_date,
            local_datetime.hour(),
            offset,
        )?;
        let client = validate_client(&message.client)?;
        let numerators = validate_numerators(message)?;
        let included = report_message_client_passes(exact_clients, message);
        if !included || !(start_date..end_date_exclusive).contains(&local_date) {
            return Ok(());
        }
        let model = normalize_model(&message.model_id)?;
        let provider = normalize_provider(&message.provider_id)?;
        let agent = normalize_agent(&message.client, message.agent.as_deref())?;
        add_label_bytes(&mut self.label_bytes, &client)?;
        if let Some(model) = &model {
            add_label_bytes(&mut self.label_bytes, model)?;
        }
        if let Some(provider) = &provider {
            add_label_bytes(&mut self.label_bytes, provider)?;
        }
        add_label_bytes(&mut self.label_bytes, &agent)?;

        let prepared = PreparedMessage {
            date_text,
            client,
            model,
            provider,
            agent,
            bucket_start_unix_ms: bucket,
            utc_offset_seconds: offset,
            numerators,
        };
        let token_total = token_total(&prepared.numerators, &mut self.saturated);
        add_common(
            self.graph
                .entry((
                    prepared.date_text.clone(),
                    prepared.client.clone(),
                    prepared.model.clone(),
                    prepared.provider.clone(),
                ))
                .or_default(),
            &prepared.numerators,
            &mut self.saturated,
        );
        add_common(
            self.hourly
                .entry((
                    prepared.bucket_start_unix_ms,
                    prepared.utc_offset_seconds,
                    prepared.client.clone(),
                    prepared.model.clone(),
                    prepared.provider.clone(),
                ))
                .or_default(),
            &prepared.numerators,
            &mut self.saturated,
        );
        add_common(
            self.agents
                .entry((
                    prepared.agent.clone(),
                    prepared.client.clone(),
                    prepared.model.clone(),
                    prepared.provider.clone(),
                ))
                .or_default(),
            &prepared.numerators,
            &mut self.saturated,
        );
        let model_accumulator = self
            .models
            .entry((prepared.client, prepared.model, prepared.provider))
            .or_default();
        add_common(
            &mut model_accumulator.common,
            &prepared.numerators,
            &mut self.saturated,
        );
        if let Some(duration_millis) = prepared.numerators.duration_millis {
            if duration_millis > 0 && token_total > 0 {
                saturating_add(
                    &mut model_accumulator.duration_millis,
                    duration_millis,
                    &mut self.saturated,
                );
                saturating_add(
                    &mut model_accumulator.timed_tokens,
                    token_total,
                    &mut self.saturated,
                );
                saturating_add(&mut model_accumulator.sample_count, 1, &mut self.saturated);
            }
        }
        if self.graph.len() > MAX_RECORDS
            || self.models.len() > MAX_RECORDS
            || self.hourly.len() > MAX_RECORDS
            || self.agents.len() > MAX_RECORDS
        {
            return Err(RemoteUsageError::LimitExceeded);
        }
        Ok(())
    }

    fn finish(self) -> Result<RemoteUsageBundleV1, RemoteUsageError> {
        let mut saturated = self.saturated;
        let graph = build_graph_records(self.graph, &mut saturated);
        let models = build_model_records(self.models, &mut saturated);
        let hourly = build_hourly_records(self.hourly, &mut saturated);
        let agents = build_agent_records(self.agents, &mut saturated);
        if output_string_bytes_graph(&graph) > MAX_OUTPUT_STRING_BYTES
            || output_string_bytes_models(&models) > MAX_OUTPUT_STRING_BYTES
            || output_string_bytes_hourly(&hourly) > MAX_OUTPUT_STRING_BYTES
            || output_string_bytes_agents(&agents) > MAX_OUTPUT_STRING_BYTES
        {
            return Err(RemoteUsageError::LimitExceeded);
        }
        Ok(RemoteUsageBundleV1 {
            tzdb_revision: REMOTE_TZDB_REVISION.to_owned(),
            graph,
            models,
            hourly,
            agents,
            saturated,
        })
    }
}

fn validate_query(query: &RemoteUsageQueryV1) -> Result<(Date, Date), RemoteUsageError> {
    if query.tzdb_revision != REMOTE_TZDB_REVISION {
        return Err(RemoteUsageError::IncompatibleTzdb);
    }
    if query.clients.len() > MAX_CLIENTS {
        return Err(RemoteUsageError::LimitExceeded);
    }
    let mut total_query_bytes = 0usize;
    for client in &query.clients {
        validate_text(client, false)?;
        total_query_bytes = total_query_bytes
            .checked_add(client.len())
            .ok_or(RemoteUsageError::LimitExceeded)?;
        if total_query_bytes > MAX_QUERY_CLIENT_BYTES {
            return Err(RemoteUsageError::LimitExceeded);
        }
    }
    for value in [
        &query.start_date,
        &query.end_date_exclusive,
        &query.timezone,
        &query.tzdb_revision,
    ] {
        total_query_bytes = total_query_bytes
            .checked_add(value.len())
            .ok_or(RemoteUsageError::LimitExceeded)?;
        if total_query_bytes > MAX_QUERY_CLIENT_BYTES {
            return Err(RemoteUsageError::LimitExceeded);
        }
    }
    if query
        .clients
        .windows(2)
        .any(|window| window[0].as_bytes().cmp(window[1].as_bytes()) != Ordering::Less)
    {
        return Err(RemoteUsageError::InvalidQuery);
    }
    let start_date = parse_date(&query.start_date)?;
    let end_date_exclusive = parse_date(&query.end_date_exclusive)?;
    let days = start_date
        .until(end_date_exclusive)
        .map_err(|_| RemoteUsageError::InvalidQuery)?
        .get_days();
    if !(1..=MAX_RANGE_DAYS).contains(&days) {
        return Err(RemoteUsageError::InvalidQuery);
    }
    Ok((start_date, end_date_exclusive))
}

fn bundled_timezone(query: &RemoteUsageQueryV1) -> Result<TimeZone, RemoteUsageError> {
    if query.timezone.is_empty()
        || query.timezone.len() > MAX_TEXT_BYTES
        || !is_nfc(&query.timezone)
    {
        return Err(RemoteUsageError::InvalidTimeZone);
    }
    let Some((canonical_name, tzif_bytes)) = jiff_tzdb::get(&query.timezone) else {
        return Err(RemoteUsageError::InvalidTimeZone);
    };
    if canonical_name != query.timezone || jiff_tzdb::VERSION != Some(REMOTE_TZDB_REVISION) {
        return Err(if jiff_tzdb::VERSION == Some(REMOTE_TZDB_REVISION) {
            RemoteUsageError::InvalidTimeZone
        } else {
            RemoteUsageError::IncompatibleTzdb
        });
    }
    TimeZone::tzif(canonical_name, tzif_bytes).map_err(|_| RemoteUsageError::InvalidTimeZone)
}

fn parse_date(value: &str) -> Result<Date, RemoteUsageError> {
    if value.len() != 10
        || value.as_bytes().get(4) != Some(&b'-')
        || value.as_bytes().get(7) != Some(&b'-')
    {
        return Err(RemoteUsageError::InvalidQuery);
    }
    let date = Date::strptime("%Y-%m-%d", value).map_err(|_| RemoteUsageError::InvalidQuery)?;
    if date.to_string() != value {
        return Err(RemoteUsageError::InvalidQuery);
    }
    Ok(date)
}

fn validate_message_timestamp(timestamp: i64) -> Result<(), RemoteUsageError> {
    if !(1..=Timestamp::MAX.as_millisecond()).contains(&timestamp) {
        return Err(RemoteUsageError::InvalidTimestamp);
    }
    Ok(())
}

fn timestamp_from_millisecond(timestamp: i64) -> Result<Timestamp, RemoteUsageError> {
    let second = timestamp.div_euclid(1_000);
    let millisecond = timestamp.rem_euclid(1_000) as i32;
    Timestamp::new(second, millisecond * 1_000_000).map_err(|_| RemoteUsageError::InvalidTimestamp)
}

fn validate_client(client: &str) -> Result<String, RemoteUsageError> {
    validate_text(client, false)?;
    Ok(client.to_owned())
}

fn validate_text(value: &str, allow_empty: bool) -> Result<(), RemoteUsageError> {
    if (!allow_empty && value.is_empty()) || value.len() > MAX_TEXT_BYTES || !is_nfc(value) {
        return Err(RemoteUsageError::InvalidText);
    }
    Ok(())
}

fn is_nfc(value: &str) -> bool {
    value.nfc().eq(value.chars())
}

fn normalize_model(value: &str) -> Result<Option<String>, RemoteUsageError> {
    validate_text(value, true)?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    validate_text(trimmed, false)?;
    let model = canonical_model_id(trimmed);
    validate_text(&model, false)?;
    Ok(Some(model))
}

fn normalize_provider(value: &str) -> Result<Option<String>, RemoteUsageError> {
    validate_text(value, true)?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    validate_text(trimmed, false)?;
    Ok(Some(trimmed.to_owned()))
}

fn normalize_agent(client: &str, value: Option<&str>) -> Result<String, RemoteUsageError> {
    let Some(value) = value else {
        return Ok("Main".to_owned());
    };
    validate_text(value, true)?;
    let normalized = if client == "copilot" {
        normalize_copilot_agent_name(value)
    } else {
        normalize_agent_name(value)
    };
    let normalized = if normalized.is_empty() {
        "Main".to_owned()
    } else {
        normalized
    };
    validate_text(&normalized, false)?;
    Ok(normalized)
}

fn validate_numerators(message: &UnifiedMessage) -> Result<Numerators, RemoteUsageError> {
    let tokens = &message.tokens;
    if tokens.input < 0
        || tokens.output < 0
        || tokens.cache_read < 0
        || tokens.cache_write < 0
        || tokens.reasoning < 0
        || message.message_count < 0
        || message.duration_ms.is_some_and(|duration| duration < 0)
        || !message.cost.is_finite()
        || message.cost < 0.0
    {
        return Err(RemoteUsageError::InvalidNumerator);
    }
    let scaled = message.cost * 1_000_000_000.0;
    if !scaled.is_finite() {
        return Err(RemoteUsageError::InvalidNumerator);
    }
    let rounded = scaled.round_ties_even();
    if !(0.0..MAX_COST_NANO_USD).contains(&rounded) {
        return Err(RemoteUsageError::InvalidNumerator);
    }
    Ok(Numerators {
        input_tokens: tokens.input as u64,
        output_tokens: tokens.output as u64,
        cache_read_tokens: tokens.cache_read as u64,
        cache_write_tokens: tokens.cache_write as u64,
        reasoning_tokens: tokens.reasoning as u64,
        message_count: message.message_count as u64,
        turn_count: u64::from(message.is_turn_start),
        cost_nano_usd: rounded as u64,
        duration_millis: message.duration_ms.map(|duration| duration as u64),
    })
}

fn add_label_bytes(total: &mut usize, label: &str) -> Result<(), RemoteUsageError> {
    *total = total
        .checked_add(label.len())
        .ok_or(RemoteUsageError::LimitExceeded)?;
    if *total > MAX_LABEL_BYTES {
        return Err(RemoteUsageError::LimitExceeded);
    }
    Ok(())
}

fn bucket_start(
    timezone: &TimeZone,
    timestamp: Timestamp,
    date: Date,
    hour: i8,
    offset_seconds: i32,
) -> Result<i64, RemoteUsageError> {
    let hour_start = date.at(hour, 0, 0, 0);
    let ambiguous = timezone.to_ambiguous_timestamp(hour_start);
    let mut candidates = Vec::with_capacity(4);
    if let Ok(candidate) = ambiguous.earlier() {
        candidates.push(candidate);
    }
    if let Ok(candidate) = ambiguous.later() {
        candidates.push(candidate);
    }

    let before = timestamp_from_millisecond(timestamp.as_millisecond().saturating_sub(1))?;
    if let Some(transition) = timezone.following(before).next() {
        candidates.push(transition.timestamp());
    }
    if let Some(transition) = timezone.preceding(timestamp).next() {
        candidates.push(transition.timestamp());
    }

    let mut best = None;
    for candidate in candidates {
        if candidate > timestamp || timezone.to_offset(candidate).seconds() != offset_seconds {
            continue;
        }
        let local = timezone.to_datetime(candidate);
        if local.date() != date || local.hour() != hour {
            continue;
        }
        best = Some(best.map_or(candidate, |current: Timestamp| current.min(candidate)));
    }
    best.map(|candidate| candidate.as_millisecond())
        .ok_or(RemoteUsageError::InvalidTimestamp)
}

fn add_common(accumulator: &mut CommonAccumulator, numerators: &Numerators, saturated: &mut bool) {
    saturating_add(
        &mut accumulator.input_tokens,
        numerators.input_tokens,
        saturated,
    );
    saturating_add(
        &mut accumulator.output_tokens,
        numerators.output_tokens,
        saturated,
    );
    saturating_add(
        &mut accumulator.cache_read_tokens,
        numerators.cache_read_tokens,
        saturated,
    );
    saturating_add(
        &mut accumulator.cache_write_tokens,
        numerators.cache_write_tokens,
        saturated,
    );
    saturating_add(
        &mut accumulator.reasoning_tokens,
        numerators.reasoning_tokens,
        saturated,
    );
    saturating_add(
        &mut accumulator.message_count,
        numerators.message_count,
        saturated,
    );
    saturating_add(
        &mut accumulator.turn_count,
        numerators.turn_count,
        saturated,
    );
    saturating_add(
        &mut accumulator.cost_nano_usd,
        numerators.cost_nano_usd,
        saturated,
    );
}

fn saturating_add(target: &mut u64, value: u64, saturated: &mut bool) {
    let (sum, overflowed) = target.overflowing_add(value);
    if overflowed {
        *target = u64::MAX;
        *saturated = true;
    } else {
        *target = sum;
    }
}

fn token_total(numerators: &Numerators, saturated: &mut bool) -> u64 {
    let mut total = 0;
    saturating_add(&mut total, numerators.input_tokens, saturated);
    saturating_add(&mut total, numerators.output_tokens, saturated);
    saturating_add(&mut total, numerators.cache_read_tokens, saturated);
    saturating_add(&mut total, numerators.cache_write_tokens, saturated);
    saturating_add(&mut total, numerators.reasoning_tokens, saturated);
    total
}

fn common_values(
    common: CommonAccumulator,
    saturated: &mut bool,
) -> (u64, u64, u64, u64, u64, u64, u64, u64, u64) {
    let total_tokens = token_total(
        &Numerators {
            input_tokens: common.input_tokens,
            output_tokens: common.output_tokens,
            cache_read_tokens: common.cache_read_tokens,
            cache_write_tokens: common.cache_write_tokens,
            reasoning_tokens: common.reasoning_tokens,
            ..Numerators::default()
        },
        saturated,
    );
    (
        common.input_tokens,
        common.output_tokens,
        common.cache_read_tokens,
        common.cache_write_tokens,
        common.reasoning_tokens,
        total_tokens,
        common.message_count,
        common.turn_count,
        common.cost_nano_usd,
    )
}

fn build_graph_records(
    records: BTreeMap<GraphKey, CommonAccumulator>,
    saturated: &mut bool,
) -> Vec<RemoteGraphRecordV1> {
    records
        .into_iter()
        .map(|((date, client, model, provider), common)| {
            let (
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens,
                reasoning_tokens,
                total_tokens,
                message_count,
                turn_count,
                cost_nano_usd,
            ) = common_values(common, saturated);
            RemoteGraphRecordV1 {
                date,
                client,
                model,
                provider,
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens,
                reasoning_tokens,
                total_tokens,
                message_count,
                turn_count,
                cost_nano_usd,
            }
        })
        .collect()
}

fn build_model_records(
    records: BTreeMap<ModelKey, ModelAccumulator>,
    saturated: &mut bool,
) -> Vec<RemoteModelsRecordV1> {
    records
        .into_iter()
        .map(|((client, model, provider), accumulator)| {
            let (
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens,
                reasoning_tokens,
                total_tokens,
                message_count,
                turn_count,
                cost_nano_usd,
            ) = common_values(accumulator.common, saturated);
            RemoteModelsRecordV1 {
                client,
                model,
                provider,
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens,
                reasoning_tokens,
                total_tokens,
                message_count,
                turn_count,
                cost_nano_usd,
                duration_millis: accumulator.duration_millis,
                timed_tokens: accumulator.timed_tokens,
                sample_count: accumulator.sample_count,
            }
        })
        .collect()
}

fn build_hourly_records(
    records: BTreeMap<HourlyKey, CommonAccumulator>,
    saturated: &mut bool,
) -> Vec<RemoteHourlyRecordV1> {
    records
        .into_iter()
        .map(
            |((bucket_start_unix_ms, utc_offset_seconds, client, model, provider), common)| {
                let (
                    input_tokens,
                    output_tokens,
                    cache_read_tokens,
                    cache_write_tokens,
                    reasoning_tokens,
                    total_tokens,
                    message_count,
                    turn_count,
                    cost_nano_usd,
                ) = common_values(common, saturated);
                RemoteHourlyRecordV1 {
                    bucket_start_unix_ms,
                    utc_offset_seconds,
                    client,
                    model,
                    provider,
                    input_tokens,
                    output_tokens,
                    cache_read_tokens,
                    cache_write_tokens,
                    reasoning_tokens,
                    total_tokens,
                    message_count,
                    turn_count,
                    cost_nano_usd,
                }
            },
        )
        .collect()
}

fn build_agent_records(
    records: BTreeMap<AgentKey, CommonAccumulator>,
    saturated: &mut bool,
) -> Vec<RemoteAgentsRecordV1> {
    records
        .into_iter()
        .map(|((agent, client, model, provider), common)| {
            let (
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens,
                reasoning_tokens,
                total_tokens,
                message_count,
                turn_count,
                cost_nano_usd,
            ) = common_values(common, saturated);
            RemoteAgentsRecordV1 {
                agent,
                client,
                model,
                provider,
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens,
                reasoning_tokens,
                total_tokens,
                message_count,
                turn_count,
                cost_nano_usd,
            }
        })
        .collect()
}

fn output_string_bytes_graph(records: &[RemoteGraphRecordV1]) -> usize {
    records
        .iter()
        .map(|record| {
            record.date.len()
                + record.client.len()
                + record.model.as_ref().map_or(0, String::len)
                + record.provider.as_ref().map_or(0, String::len)
        })
        .sum()
}

fn output_string_bytes_models(records: &[RemoteModelsRecordV1]) -> usize {
    records
        .iter()
        .map(|record| {
            record.client.len()
                + record.model.as_ref().map_or(0, String::len)
                + record.provider.as_ref().map_or(0, String::len)
        })
        .sum()
}

fn output_string_bytes_hourly(records: &[RemoteHourlyRecordV1]) -> usize {
    records
        .iter()
        .map(|record| {
            record.client.len()
                + record.model.as_ref().map_or(0, String::len)
                + record.provider.as_ref().map_or(0, String::len)
        })
        .sum()
}

fn output_string_bytes_agents(records: &[RemoteAgentsRecordV1]) -> usize {
    records
        .iter()
        .map(|record| {
            record.agent.len()
                + record.client.len()
                + record.model.as_ref().map_or(0, String::len)
                + record.provider.as_ref().map_or(0, String::len)
        })
        .sum()
}
