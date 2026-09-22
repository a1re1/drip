// The model-transport layer of the harness (debt audit S2, stage 1): one
// place owns the retry ladder — network rejections, 429s (with retry-after
// and the non-retryable quota fast-fail), 5xx — plus context-overflow
// classification and per-call usage reporting. runSolidStateHarness binds
// this once per run; the loop body just calls the returned function.
//
// Rust notes: `fetch` becomes reqwest; the AbortSignal/AbortController pair
// becomes a shared AtomicBool flag (AbortSignal) polled by a tokio::select!
// arm so a --stop still cancels the in-flight request; the per-attempt
// timeout wraps the request AND its body read with tokio::time::timeout.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::core::types::{HarnessEvent, HarnessEventData, HarnessEventType};
use crate::harness::anthropic::{
    build_anthropic_headers, build_anthropic_messages_url, build_anthropic_request_payload,
    is_anthropic_native_provider, translate_anthropic_response, BuildAnthropicRequestPayloadArgs,
};
use crate::harness::codex::{BridgeConfig, CodexBridge};
use crate::harness::transport::{
    build_transport_request_payload, BuildTransportRequestPayloadArgs, OpenAICompatibleRequestTool,
    OpenAICompatibleToolCall, TransportRequestMessage,
};

pub const RATE_LIMIT_MAX_ATTEMPTS: u32 = 10;

// When a route has a fallback gateway, its own ladder is cut short: burning the
// full ~90s of backoff against a gateway that is down, when a working one is
// one retry away, is pure latency. The fallback then runs the full ladder, so
// the total patience of a call is unchanged for the route that can actually
// serve it. Routes without a fallback keep the full ladder, unchanged.
pub const FALLBACK_PRIMARY_MAX_ATTEMPTS: u32 = 2;

pub const RATE_LIMIT_BACKOFF_SECONDS: [u64; 9] = [1, 1, 2, 3, 5, 8, 13, 21, 34];

// A single request that never answers is the one failure the ladder could not
// see: a hung upstream (observed: a 33-minute completion, a 19-minute wait
// that ended in an empty 200) parks the whole run with no retry, no failover,
// and no event. Every attempt is now bounded; a timed-out attempt re-enters
// the same ladder as a network rejection. Four minutes comfortably covers a
// long reasoning completion while still catching a stall well inside the
// budget of a bounded child run such as a --review file reviewer.
pub const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 240_000;
/// Total wall-clock patience for one model call across unreachable /
/// timed-out attempts. Ten 240s timeouts plus backoff once cost a run 40
/// minutes on a single turn; past this bound the call fails and the run
/// ends resumable instead of burning its budget on a dead endpoint.
pub const MAX_UNREACHABLE_WALL_MS: u64 = 600_000;

// A stalled upstream rarely looks like a dead one: OpenRouter routes that
// usually answer in 5-10s occasionally park one request for minutes (a
// bench run spent 349s + 189s + 127s on three calls whose neighbours took
// 8s). The fixed 240s bound only catches the worst of those. Once a model
// has a few completed calls behind it, the FIRST attempt of each call is
// bounded by a multiple of its median latency instead; a call that blows
// that bound is retried at once, and the retry gets the full bound so a
// legitimately long completion still lands.
pub const STALL_TIMEOUT_MULTIPLIER: u64 = 8;
pub const STALL_TIMEOUT_MIN_SAMPLES: usize = 3;
pub const STALL_TIMEOUT_FLOOR_MS: u64 = 45_000;
pub const STALL_LATENCY_SAMPLES: usize = 8;

// The latency tail is where the wall goes: across two 12-run benches, GLM
// calls over 15s were 55% of all inference time (891s of 1621s) while the
// median call took 2.2s, and those slow calls produced 3-14 tokens/s against
// the usual 59 — queueing, not generation. A hedge (Dean & Barroso, "The Tail
// at Scale") races a second identical request once the first has run past a
// multiple of the model's median; whichever answers first wins and the other
// is dropped. Costs a duplicate request on the slow few percent of calls.
pub const HEDGE_MULTIPLIER: u64 = 2;
pub const HEDGE_FLOOR_MS: u64 = 8_000;
/// Streaming requests: the floor of the first-token hedge point. A first
/// attempt with no first token by HEDGE_MULTIPLIER × the model's median
/// first-token time (never under this floor, never past the wall-clock
/// hedge point) is raced against a second request. Recorded GLM calls show
/// a first token within about a second in a normal window; the old
/// wall-clock hedge waited a flat 8s and the second request then won 26 of
/// 43 races. In a queued window (first tokens at 5–10s) the point rises
/// with the median, so the race is not run against a provider that is
/// uniformly slow.
pub const FIRST_TOKEN_HEDGE_MS: u64 = 4_000;
/// Streaming requests: a first attempt whose stream has gone quiet for this
/// long after its first token is raced against a second request.
pub const STREAM_STALL_HEDGE_MS: u64 = 15_000;
/// How often the streaming hedge loop looks at the primary's progress.
const HEDGE_POLL_MS: u64 = 200;

/// What a streaming request has received so far, shared between the request
/// future and the hedge loop that watches it.
#[derive(Default)]
pub struct RequestProgress {
    started: std::sync::Mutex<Option<Instant>>,
    first_token: std::sync::Mutex<Option<Instant>>,
    last_byte: std::sync::Mutex<Option<Instant>>,
}

impl RequestProgress {
    pub fn new() -> Self {
        let progress = Self::default();
        *progress.started.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        progress
    }
    fn touch(&self) {
        *self.last_byte.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
    }
    fn mark_first_token(&self) {
        let mut first = self.first_token.lock().unwrap_or_else(|e| e.into_inner());
        if first.is_none() {
            *first = Some(Instant::now());
        }
    }
    pub fn first_token_seen(&self) -> bool {
        self.first_token
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }
    /// Milliseconds from the request start to the first token, once seen.
    pub fn first_token_ms(&self) -> Option<i64> {
        let started = (*self.started.lock().unwrap_or_else(|e| e.into_inner()))?;
        let first = (*self.first_token.lock().unwrap_or_else(|e| e.into_inner()))?;
        Some(first.duration_since(started).as_millis() as i64)
    }
    pub fn since_last_byte(&self) -> Option<Duration> {
        self.last_byte
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .map(|at| at.elapsed())
    }
}

/// The request body with streaming switched on (usage in the final chunk).
pub fn with_stream_fields(request_body: &Value) -> String {
    let mut body = request_body.clone();
    if let Some(object) = body.as_object_mut() {
        object.insert("stream".to_string(), Value::Bool(true));
        object.insert(
            "stream_options".to_string(),
            serde_json::json!({ "include_usage": true }),
        );
    }
    body.to_string()
}

fn is_event_stream(headers: &reqwest::header::HeaderMap) -> bool {
    headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("text/event-stream"))
}

/// True for an SSE data line that carries model output (content, a tool
/// call, or reasoning) or an error — not for a role-only first delta, a
/// keepalive comment, or `[DONE]`.
pub fn sse_line_carries_a_token(line: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(line) else {
        return false;
    };
    let Some(data) = text.trim().strip_prefix("data:") else {
        return false;
    };
    let data = data.trim();
    if data == "[DONE]" || data.is_empty() {
        return false;
    }
    let Ok(value) = serde_json::from_str::<Value>(data) else {
        return false;
    };
    if value.get("error").is_some_and(|error| !error.is_null()) {
        return true;
    }
    value
        .get("choices")
        .and_then(Value::as_array)
        .is_some_and(|choices| {
            choices.iter().any(|choice| {
                let delta = choice.get("delta").unwrap_or(&Value::Null);
                let non_empty = |key: &str| match delta.get(key) {
                    Some(Value::String(text)) => !text.is_empty(),
                    Some(Value::Array(items)) => !items.is_empty(),
                    Some(Value::Null) | None => false,
                    Some(_) => true,
                };
                non_empty("content")
                    || non_empty("tool_calls")
                    || non_empty("reasoning")
                    || non_empty("reasoning_content")
            })
        })
}

/// Reads a streamed (SSE) response to its end, marking progress as bytes
/// arrive and the first token as soon as a data line carries one.
async fn read_event_stream(
    mut response: reqwest::Response,
    progress: &RequestProgress,
) -> Result<Vec<u8>, reqwest::Error> {
    let mut buffer: Vec<u8> = Vec::new();
    let mut scanned = 0usize;
    while let Some(chunk) = response.chunk().await? {
        buffer.extend_from_slice(&chunk);
        progress.touch();
        if !progress.first_token_seen() {
            while let Some(offset) = buffer[scanned..].iter().position(|byte| *byte == b'\n') {
                let line = &buffer[scanned..scanned + offset];
                scanned += offset + 1;
                if sse_line_carries_a_token(line) {
                    progress.mark_first_token();
                    break;
                }
            }
        }
    }
    Ok(buffer)
}

/// Folds the chunks of a streamed chat completion back into the one JSON
/// body a non-streaming request returns, so everything after the transport
/// (error envelopes, usage, tool-call parsing) stays unchanged. Content
/// deltas concatenate; tool-call deltas merge by index with their argument
/// fragments appended; the last usage object wins; an error chunk becomes
/// the body's `error`.
pub fn assemble_streamed_response(sse: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(sse);
    let mut id: Option<Value> = None;
    let mut model: Option<Value> = None;
    let mut content = String::new();
    let mut content_seen = false;
    let mut finish_reason: Option<Value> = None;
    let mut usage: Option<Value> = None;
    let mut tool_calls: Vec<(u64, serde_json::Map<String, Value>)> = Vec::new();
    for line in text.lines() {
        let Some(data) = line.trim().strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data == "[DONE]" {
            break;
        }
        let Ok(value) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        if let Some(error) = value.get("error").filter(|error| !error.is_null()) {
            return serde_json::to_vec(&serde_json::json!({ "error": error })).unwrap_or_default();
        }
        if id.is_none() {
            id = value.get("id").cloned().filter(|v| !v.is_null());
        }
        if model.is_none() {
            model = value.get("model").cloned().filter(|v| !v.is_null());
        }
        if let Some(chunk_usage) = value.get("usage").filter(|v| v.is_object()) {
            usage = Some(chunk_usage.clone());
        }
        for choice in value
            .get("choices")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(reason) = choice.get("finish_reason").filter(|v| !v.is_null()) {
                finish_reason = Some(reason.clone());
            }
            let Some(delta) = choice.get("delta") else {
                continue;
            };
            if let Some(Value::String(piece)) = delta.get("content") {
                content_seen = true;
                content.push_str(piece);
            }
            for (position, call) in delta
                .get("tool_calls")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .enumerate()
            {
                let index = call
                    .get("index")
                    .and_then(Value::as_u64)
                    .unwrap_or(position as u64);
                let entry = match tool_calls.iter_mut().find(|(i, _)| *i == index) {
                    Some((_, entry)) => entry,
                    None => {
                        tool_calls.push((index, serde_json::Map::new()));
                        &mut tool_calls.last_mut().expect("just pushed").1
                    }
                };
                if let Some(Value::String(call_id)) = call.get("id") {
                    if !call_id.is_empty() {
                        entry.insert("id".to_string(), Value::String(call_id.clone()));
                    }
                }
                if let Some(Value::String(kind)) = call.get("type") {
                    entry.insert("type".to_string(), Value::String(kind.clone()));
                }
                if let Some(function) = call.get("function") {
                    let existing = entry
                        .entry("function")
                        .or_insert_with(|| serde_json::json!({ "name": "", "arguments": "" }));
                    if let (Some(Value::String(name)), Some(slot)) =
                        (function.get("name"), existing.get_mut("name"))
                    {
                        if !name.is_empty() && slot.as_str().unwrap_or("").is_empty() {
                            *slot = Value::String(name.clone());
                        }
                    }
                    if let (Some(Value::String(fragment)), Some(Value::String(arguments))) =
                        (function.get("arguments"), existing.get_mut("arguments"))
                    {
                        arguments.push_str(fragment);
                    }
                }
            }
        }
    }
    tool_calls.sort_by_key(|(index, _)| *index);
    let mut message = serde_json::Map::new();
    message.insert("role".to_string(), Value::String("assistant".to_string()));
    message.insert(
        "content".to_string(),
        if content_seen {
            Value::String(content)
        } else {
            Value::Null
        },
    );
    if !tool_calls.is_empty() {
        message.insert(
            "tool_calls".to_string(),
            Value::Array(
                tool_calls
                    .into_iter()
                    .map(|(_, mut entry)| {
                        entry
                            .entry("type")
                            .or_insert_with(|| Value::String("function".to_string()));
                        Value::Object(entry)
                    })
                    .collect(),
            ),
        );
    }
    let mut body = serde_json::Map::new();
    if let Some(id) = id {
        body.insert("id".to_string(), id);
    }
    if let Some(model) = model {
        body.insert("model".to_string(), model);
    }
    body.insert(
        "choices".to_string(),
        serde_json::json!([{ "index": 0, "finish_reason": finish_reason, "message": Value::Object(message) }]),
    );
    if let Some(usage) = usage {
        body.insert("usage".to_string(), usage);
    }
    serde_json::to_vec(&Value::Object(body)).unwrap_or_default()
}

fn streaming_rejection_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"(?i)stream").expect("streaming rejection regex"))
}
/// File under the drip home that remembers each model's recent latencies
/// across runs, so the stall bound and hedge point apply from the first call.
pub const LATENCY_STORE_FILE: &str = "latency.json";

type LatencySamples = std::collections::HashMap<String, std::collections::VecDeque<u64>>;

/// Reads the persisted latency samples; empty when the file is missing or unreadable.
pub fn load_latency_store(path: &std::path::Path) -> LatencySamples {
    let Ok(text) = std::fs::read_to_string(path) else {
        return LatencySamples::new();
    };
    let Ok(raw) = serde_json::from_str::<std::collections::HashMap<String, Vec<u64>>>(&text) else {
        return LatencySamples::new();
    };
    raw.into_iter()
        .map(|(model, samples)| {
            let keep = samples.len().saturating_sub(STALL_LATENCY_SAMPLES);
            (model, samples.into_iter().skip(keep).collect())
        })
        .collect()
}

/// Writes the samples atomically (temp file + rename); errors are ignored
/// because the store is a cache, never a source of truth.
pub fn save_latency_store(path: &std::path::Path, samples: &LatencySamples) {
    let raw: std::collections::BTreeMap<&str, Vec<u64>> = samples
        .iter()
        .map(|(model, recent)| (model.as_str(), recent.iter().copied().collect()))
        .collect();
    let Ok(text) = serde_json::to_string(&raw) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// When to hedge a first attempt for a model with `samples` recent latencies:
/// HEDGE_MULTIPLIER × median clamped to [floor, timeout/2]; None until enough
/// samples exist or when hedging is disabled (floor 0).
/// The streaming hedge point for a model with `samples` recent first-token
/// times (ms): HEDGE_MULTIPLIER × median, clamped to
/// [FIRST_TOKEN_HEDGE_MS, wall_delay_ms]; the floor until enough samples
/// exist (and never past the wall-clock point).
pub fn first_token_hedge_ms(samples: &[u64], wall_delay_ms: u64) -> u64 {
    if samples.len() < STALL_TIMEOUT_MIN_SAMPLES {
        return FIRST_TOKEN_HEDGE_MS.min(wall_delay_ms);
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let median = sorted[sorted.len() / 2];
    median
        .saturating_mul(HEDGE_MULTIPLIER)
        .max(FIRST_TOKEN_HEDGE_MS)
        .min(wall_delay_ms)
}

pub fn hedge_delay_ms(samples: &[u64], floor_ms: u64, timeout_ms: u64) -> Option<u64> {
    if floor_ms == 0 || samples.len() < STALL_TIMEOUT_MIN_SAMPLES {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let median = sorted[sorted.len() / 2];
    Some(
        median
            .saturating_mul(HEDGE_MULTIPLIER)
            .max(floor_ms)
            .min(timeout_ms / 2),
    )
}

/// The first-attempt bound for a model with `samples` recent successful
/// latencies (ms): STALL_TIMEOUT_MULTIPLIER × median, clamped to
/// [STALL_TIMEOUT_FLOOR_MS, base]; `base` until enough samples exist.
pub fn stall_timeout_ms(samples: &[u64], base: u64) -> u64 {
    if samples.len() < STALL_TIMEOUT_MIN_SAMPLES {
        return base;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let median = sorted[sorted.len() / 2];
    median
        .saturating_mul(STALL_TIMEOUT_MULTIPLIER)
        .max(STALL_TIMEOUT_FLOOR_MS)
        .min(base)
}

/// The request outgrew the model's context window — recoverable by folding.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct ContextOverflowError(pub String);

/// The call's failure surface. `ContextOverflow` is a typed variant the
/// caller checks to decide whether to fold the transcript; `Message` is a
/// plain error with a message.
#[derive(Debug, thiserror::Error)]
pub enum ModelCallError {
    #[error("{0}")]
    ContextOverflow(#[from] ContextOverflowError),
    #[error("{0}")]
    Message(String),
}

impl ModelCallError {
    pub fn message(&self) -> &str {
        match self {
            ModelCallError::ContextOverflow(error) => &error.0,
            ModelCallError::Message(message) => message,
        }
    }

    pub fn is_context_overflow(&self) -> bool {
        matches!(self, ModelCallError::ContextOverflow(_))
    }
}

/// The ladder's abort throws. A stopped run must never fail over — the
/// operator asked for silence, not another gateway.
fn is_run_stopped_error(error: &ModelCallError) -> bool {
    error.message().starts_with("The run was stopped")
}

fn request_timeout_message(timeout_ms: u64) -> String {
    format!(
        "request timed out after {}s with no response",
        (timeout_ms as f64 / 1000.0).round() as i64
    )
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OpenAICompatibleResponseError {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub error_type: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OpenAICompatibleResponseUsage {
    /// Anthropic native: tokens written to the prompt cache this call (billed at the cache-write premium).
    #[serde(
        default,
        rename = "cache_creation_input_tokens",
        skip_serializing_if = "Option::is_none"
    )]
    pub cache_creation_input_tokens: Option<i64>,
    /// Anthropic native: tokens served from the prompt cache (billed at ~10% of the input rate).
    #[serde(
        default,
        rename = "cache_read_input_tokens",
        skip_serializing_if = "Option::is_none"
    )]
    pub cache_read_input_tokens: Option<i64>,
    #[serde(
        default,
        rename = "completion_tokens",
        skip_serializing_if = "Option::is_none"
    )]
    pub completion_tokens: Option<i64>,
    #[serde(
        default,
        rename = "completion_tokens_details",
        skip_serializing_if = "Option::is_none"
    )]
    pub completion_tokens_details: Option<OpenAICompatibleResponseCompletionTokensDetails>,
    #[serde(
        default,
        rename = "prompt_tokens",
        skip_serializing_if = "Option::is_none"
    )]
    pub prompt_tokens: Option<i64>,
    /// OpenAI-compatible providers with automatic caching (OpenAI, Cerebras, xAI, Gemini) report cache hits here.
    #[serde(
        default,
        rename = "prompt_tokens_details",
        skip_serializing_if = "Option::is_none"
    )]
    pub prompt_tokens_details: Option<OpenAICompatibleResponsePromptTokensDetails>,
    #[serde(
        default,
        rename = "total_tokens",
        skip_serializing_if = "Option::is_none"
    )]
    pub total_tokens: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OpenAICompatibleResponsePromptTokensDetails {
    #[serde(
        default,
        rename = "cached_tokens",
        skip_serializing_if = "Option::is_none"
    )]
    pub cached_tokens: Option<i64>,
}

/// OpenAI-compatible providers that expose hidden reasoning report its size
/// here; the harness logs it on the inference event, because a reply of
/// 8,000 tokens that ends in one small GREP call is thinking, not code.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OpenAICompatibleResponseCompletionTokensDetails {
    #[serde(
        default,
        rename = "reasoning_tokens",
        skip_serializing_if = "Option::is_none"
    )]
    pub reasoning_tokens: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OpenAICompatibleResponseMessage {
    /// Native Anthropic responses only: raw content blocks for verbatim replay (thinking blocks must survive tool round-trips).
    #[serde(
        default,
        rename = "anthropicContent",
        skip_serializing_if = "Option::is_none"
    )]
    pub anthropic_content: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Value>,
    #[serde(
        default,
        rename = "tool_calls",
        skip_serializing_if = "Option::is_none"
    )]
    pub tool_calls: Option<Vec<OpenAICompatibleToolCall>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OpenAICompatibleResponseChoice {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<OpenAICompatibleResponseMessage>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OpenAICompatibleResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub choices: Option<Vec<OpenAICompatibleResponseChoice>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<OpenAICompatibleResponseError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<OpenAICompatibleResponseUsage>,
}

impl From<crate::harness::anthropic::AnthropicTranslatedResponse> for OpenAICompatibleResponse {
    fn from(translated: crate::harness::anthropic::AnthropicTranslatedResponse) -> Self {
        OpenAICompatibleResponse {
            choices: translated.choices.map(|choices| {
                choices
                    .into_iter()
                    .map(|choice| OpenAICompatibleResponseChoice {
                        finish_reason: choice.finish_reason,
                        message: choice
                            .message
                            .map(|message| OpenAICompatibleResponseMessage {
                                anthropic_content: message.anthropic_content,
                                content: message.content,
                                tool_calls: message.tool_calls,
                            }),
                    })
                    .collect()
            }),
            error: translated.error.map(|error| OpenAICompatibleResponseError {
                code: error.code,
                message: error.message,
                error_type: error.error_type,
            }),
            usage: translated.usage.map(|usage| OpenAICompatibleResponseUsage {
                completion_tokens_details: None,
                cache_creation_input_tokens: Some(usage.cache_creation_input_tokens as i64),
                cache_read_input_tokens: Some(usage.cache_read_input_tokens as i64),
                completion_tokens: Some(usage.completion_tokens as i64),
                prompt_tokens: Some(usage.prompt_tokens as i64),
                prompt_tokens_details: None,
                total_tokens: Some(usage.total_tokens as i64),
            }),
        }
    }
}

#[derive(Clone)]
pub struct ModelRoute {
    /// Next gateway attempted when this route's retry ladder is exhausted; its own provider/url/model decide the request surface, and it may carry a fallback of its own (a chain).
    pub fallback_route: Option<Box<ModelRoute>>,
    pub headers: Option<Vec<(String, String)>>,
    pub model: String,
    /// Inference provider id (e.g. "claude", "openai"); "claude" routes through the native Anthropic API for prompt caching.
    pub provider: Option<String>,
    pub reasoning_effort: Option<String>,
    /// Re-mints this route's headers before each request when the credential is dynamic (a "cmd:" token).
    pub refresh_headers:
        Option<Arc<dyn Fn() -> Result<Vec<(String, String)>, String> + Send + Sync>>,
    pub url: String,
}

impl std::fmt::Debug for ModelRoute {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ModelRoute")
            .field(
                "fallback_route",
                &self
                    .fallback_route
                    .as_ref()
                    .map(|route| route.model.clone()),
            )
            .field(
                "headers",
                &self.headers.as_ref().map(|headers| headers.len()),
            )
            .field("model", &self.model)
            .field("provider", &self.provider)
            .field("reasoning_effort", &self.reasoning_effort)
            .field(
                "refresh_headers",
                &self.refresh_headers.as_ref().map(|_| "<fn>"),
            )
            .field("url", &self.url)
            .finish()
    }
}

// What actually served one inference call. The run-level route line records
// which routes were *armed*; this records which one answered — which matters
// more now that a call can fail over to a second gateway: `model`/`provider`
// are the winning route's, resolved inside attemptRoute, while `latencyMs` is
// the caller's real wait (it spans the primary's ladder plus any failover).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelCallRecord {
    pub latency_ms: i64,
    pub model: String,
    pub provider: Option<String>,
    pub task_id: Option<String>,
    /// The call raced a second request (a hedge was fired).
    pub hedged: bool,
    /// The second (hedged) request answered first.
    pub hedge_won: bool,
    /// Milliseconds to the first streamed token of the request that won.
    pub first_token_ms: Option<i64>,
}

/// A 429 that means "the account is out of credits" never resolves by waiting.
/// Retrying it burns the whole backoff ladder (~90s) per call and then fails
/// with the same message anyway, so it is surfaced immediately instead. The
/// message match is deliberately narrow: transient per-minute limits often say
/// "quota" too (e.g. Gemini RPM quotas) and those must keep the backoff ladder.
pub fn is_non_retryable_quota_error(error: Option<&OpenAICompatibleResponseError>) -> bool {
    let Some(error) = error else {
        return false;
    };

    let code = format!(
        "{} {}",
        error.code.clone().unwrap_or_default(),
        error.error_type.clone().unwrap_or_default()
    )
    .to_lowercase();

    code.contains("insufficient_quota")
        || code.contains("billing")
        || quota_message_regex().is_match(error.message.as_deref().unwrap_or(""))
}

fn config_error_regex() -> &'static regex::Regex {
    static REGEX: OnceLock<regex::Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        regex::Regex::new(
            r"(?i)failed to parse url|invalid url|invalid header|unsupported protocol",
        )
        .unwrap()
    })
}

fn quota_message_regex() -> &'static regex::Regex {
    static REGEX: OnceLock<regex::Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        regex::Regex::new(r"(?i)insufficient[_ ]quota|credit balance|billing").unwrap()
    })
}

fn network_error_regex() -> &'static regex::Regex {
    static REGEX: OnceLock<regex::Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        regex::Regex::new(
            r"(?i)fetch failed|unable to connect|connection\s?(refused|closed|reset)|ECONNREFUSED|ECONNRESET|ENOTFOUND|EAI_AGAIN|ETIMEDOUT|EPIPE|socket hang up|network",
        )
        .unwrap()
    })
}

/// A 400 that names the reasoning-effort field: the provider does not take
/// it, so a harness-defaulted effort is dropped for the rest of the run.
fn reasoning_rejection_regex() -> &'static regex::Regex {
    static REGEX: OnceLock<regex::Regex> = OnceLock::new();
    REGEX.get_or_init(|| regex::Regex::new(r"(?i)reasoning").unwrap())
}

impl ModelCaller {
    /// The effort a call goes out with and whether it is the harness default
    /// rather than a configured value: a route that sets none gets
    /// `BASE_MODEL_DEFAULT_REASONING_EFFORT`, the base model gets the
    /// (possibly defaulted) deps value, and once a provider has rejected the
    /// default this run every defaulted call omits the field.
    fn effective_reasoning_effort(&self, route: Option<&ModelRoute>) -> (Option<String>, bool) {
        let disabled = self
            .reasoning_default_disabled
            .load(std::sync::atomic::Ordering::Relaxed);
        match route {
            Some(route) => match route
                .reasoning_effort
                .as_deref()
                .map(str::trim)
                .filter(|effort| !effort.is_empty())
            {
                Some(effort) => (Some(effort.to_string()), false),
                None if disabled => (None, true),
                None => (Some(BASE_MODEL_DEFAULT_REASONING_EFFORT.to_string()), true),
            },
            None => {
                if self.deps.reasoning_effort_defaulted && disabled {
                    (None, true)
                } else {
                    (
                        self.deps.reasoning_effort.clone(),
                        self.deps.reasoning_effort_defaulted,
                    )
                }
            }
        }
    }
}

/// Reasoning effort for a call whose route or profile sets none; see the
/// README ("A base-model call whose profile sets no reasoning effort").
pub const BASE_MODEL_DEFAULT_REASONING_EFFORT: &str = "low";

fn context_overflow_regex() -> &'static regex::Regex {
    static REGEX: OnceLock<regex::Regex> = OnceLock::new();
    REGEX.get_or_init(|| regex::Regex::new(r"(?i)context|token|length|too long|too large").unwrap())
}

/// Network-error classification by string matching (name + message + code +
/// cause against the network-error regex). reqwest errors go through
/// `is_network_transport_error`, which first maps the config-shaped
/// (builder) failures to "never heals".
pub fn is_network_fetch_error(error: &(dyn std::error::Error + 'static)) -> bool {
    let message = error.to_string();

    if config_error_regex().is_match(&message) {
        // A bad URL or header never heals with retries — fail fast with the cause.
        return false;
    }

    // Runtime error text varies ("fetch failed" + ECONN codes, "Unable to
    // connect. Is the computer able to access the url?" with
    // ConnectionRefused/ConnectionClosed). reqwest's Display carries the
    // cause chain (hyper/tcp error text), so walk it and collect
    // name/message/code/cause into `details`.
    let mut details = message;

    let mut source = error.source();

    while let Some(error) = source {
        details.push(' ');
        details.push_str(&error.to_string());
        source = error.source();
    }

    network_error_regex().is_match(&details)
}

fn is_network_transport_error(error: &reqwest::Error) -> bool {
    if error.is_builder() {
        // A bad URL or header never heals with retries — fail fast with the cause.
        return false;
    }

    // reqwest's request-phase failures (connect refused/reset, DNS, I/O) are
    // the "fetch failed" equivalent.
    if error.is_connect() || error.is_timeout() || error.is_request() {
        return true;
    }

    is_network_fetch_error(error)
}

/// The run's cancellation flag: a shared AtomicBool the loop sets on --stop.
/// `sleep_unless_aborted` and the in-flight request both observe it.
#[derive(Clone, Default)]
pub struct AbortSignal(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl AbortSignal {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn abort(&self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn is_aborted(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

pub async fn sleep_unless_aborted(ms: u64, signal: Option<&AbortSignal>) {
    if signal.is_some_and(|signal| signal.is_aborted()) {
        return;
    }

    // Deliberately NOT detached: this is the retry backoff of a live run.
    // Poll in slices so an abort cuts the wait short instead of sleeping
    // through the full backoff.
    let deadline = Instant::now() + Duration::from_millis(ms);

    while Instant::now() < deadline {
        if signal.is_some_and(|signal| signal.is_aborted()) {
            return;
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        tokio::time::sleep(remaining.min(Duration::from_millis(25))).await;
    }
}

pub type SleepFuture = Pin<Box<dyn std::future::Future<Output = ()> + Send>>;
/// Injectable, abort-aware sleep used by the retry backoff.
pub type SleepFn = Arc<dyn Fn(u64, Option<AbortSignal>) -> SleepFuture + Send + Sync>;

fn default_sleep(ms: u64, signal: Option<AbortSignal>) -> SleepFuture {
    Box::pin(async move { sleep_unless_aborted(ms, signal.as_ref()).await })
}

#[derive(Debug, Clone, Default)]
pub struct ModelCallOptions {
    pub include_tools: Option<bool>,
    pub route: Option<ModelRoute>,
    pub transport_tools: Option<Vec<OpenAICompatibleRequestTool>>,
    pub usage_task_id: Option<String>,
    /// Output cap for this call (None: provider default).
    pub max_tokens: Option<u64>,
    /// tool_choice override for OpenAI-compatible providers ("required"
    /// forces a tool call); None sends "auto".
    pub tool_choice: Option<String>,
}

pub struct ModelCallerDeps {
    /// Harness working directory handed to the codex bridge so spawned
    /// `codex app-server` processes run where the harness runs.
    pub cwd: Option<String>,
    /// Executable the codex bridge spawns (None: "codex" on PATH). Tests point
    /// it at a missing binary to exercise the base-model fallback.
    pub codex_executable: Option<String>,
    pub default_transport_tools: Vec<OpenAICompatibleRequestTool>,
    pub emit: Arc<dyn Fn(HarnessEvent) + Send + Sync>,
    /// Fallback gateway for calls that use the base model/url (text-only calls such as run summaries), where there is no route object to hang one off.
    pub fallback_route: Option<ModelRoute>,
    /// Rate-limit events carry the cycle they interrupted.
    pub get_iteration: Arc<dyn Fn() -> i64 + Send + Sync>,
    pub headers: Vec<(String, String)>,
    pub model: String,
    pub on_retry_wait: Arc<dyn Fn(f64) + Send + Sync>,
    pub on_usage: Arc<dyn Fn(&OpenAICompatibleResponse, ModelCallRecord) + Send + Sync>,
    /// Provider of the base model route; "claude" switches to the native Anthropic API (prompt caching).
    pub provider: Option<String>,
    /// Re-mints the base route's headers before each request when the credential is dynamic (a "cmd:" token).
    pub refresh_headers:
        Option<Arc<dyn Fn() -> Result<Vec<(String, String)>, String> + Send + Sync>>,
    /// Stable key sent to providers that support prompt-cache routing hints (OpenAI: prompt_cache_key body field; xAI: x-grok-conv-id header).
    pub prompt_cache_key: Option<String>,
    pub reasoning_effort: Option<String>,
    /// `reasoning_effort` is the harness's own default for a profile that
    /// set none (see `BASE_MODEL_DEFAULT_REASONING_EFFORT`), so a provider
    /// that rejects the field gets one retry without it and the run goes on.
    pub reasoning_effort_defaulted: bool,
    /// Per-attempt wall-clock cap on one HTTP request (default DEFAULT_REQUEST_TIMEOUT_MS); a timed-out attempt retries on the network ladder.
    pub request_timeout_ms: Option<u64>,
    /// Earliest point (ms) at which a slow first attempt is hedged with a second identical request
    /// (default HEDGE_FLOOR_MS; Some(0) disables hedging — used by one-shot helper callers).
    pub hedge_floor_ms: Option<u64>,
    /// Persisted per-model latency samples (LATENCY_STORE_FILE under the drip home); None keeps them in memory only.
    pub latency_store: Option<std::path::PathBuf>,
    pub signal: Option<AbortSignal>,
    pub sleep_impl: Option<SleepFn>,
    pub tool_route: Option<ModelRoute>,
    pub url: String,
    /// Test seam: a pre-built reqwest client (the URL/server is the other
    /// half of that seam).
    pub http_client: Option<reqwest::Client>,
}

/// Calls the model: `call_model` takes the messages and call options and
/// returns the response; the resolved dependencies (client, timeout, sleep
/// impl) live on the struct.
pub struct ModelCaller {
    deps: ModelCallerDeps,
    http_client: reqwest::Client,
    request_timeout_ms: u64,
    hedge_floor_ms: u64,
    latency_store: Option<std::path::PathBuf>,
    sleep: SleepFn,
    /// Recent successful latencies per model, for the stall-aware first-attempt bound.
    latency_samples:
        std::sync::Mutex<std::collections::HashMap<String, std::collections::VecDeque<u64>>>,
    /// Recent first-token times per model from streamed replies (this run
    /// only), the basis of the streaming hedge point.
    first_token_samples:
        std::sync::Mutex<std::collections::HashMap<String, std::collections::VecDeque<u64>>>,
    /// Codex lane for tool-bearing calls (the run's main conversation).
    codex_tool_lane: tokio::sync::Mutex<Option<CodexBridge>>,
    /// Codex lane for include_tools=false calls (run summaries), so they never
    /// interleave with the tool lane's pending tool request.
    codex_summary_lane: tokio::sync::Mutex<Option<CodexBridge>>,
    /// Set once a provider rejected the harness-default reasoning effort;
    /// later base-model calls omit the field.
    reasoning_default_disabled: std::sync::atomic::AtomicBool,
    /// Set once a provider rejected a streaming request; later calls send
    /// non-streaming requests (and hedge on wall time as before).
    streaming_disabled: std::sync::atomic::AtomicBool,
}

/// A codex bridge that could not start at all (missing binary, spawn error):
/// the one gateway failure no retry or wait can fix.
pub fn is_codex_spawn_failure(message: &str) -> bool {
    message.contains("not found or failed to spawn")
}

pub fn create_model_caller(deps: ModelCallerDeps) -> ModelCaller {
    let request_timeout_ms = match deps.request_timeout_ms {
        Some(timeout_ms) if timeout_ms > 0 => timeout_ms,
        _ => DEFAULT_REQUEST_TIMEOUT_MS,
    };
    let hedge_floor_ms = deps.hedge_floor_ms.unwrap_or(HEDGE_FLOOR_MS);
    let latency_store = deps.latency_store.clone();
    let seeded = latency_store
        .as_deref()
        .map(load_latency_store)
        .unwrap_or_default();
    if let Some(path) = latency_store.as_deref().filter(|_| !seeded.is_empty()) {
        (deps.emit)(HarnessEvent {
            data: None,
            detail: format!(
                "latency memory: seeded {} model(s) from {} — stall bound and hedge point apply from the first call",
                seeded.len(),
                path.display()
            ),
            iteration: (deps.get_iteration)(),
            r#type: HarnessEventType::HarnessOp,
        });
    }
    let http_client = deps
        .http_client
        .clone()
        .unwrap_or_else(|| reqwest::Client::builder().build().expect("reqwest client"));
    let sleep = deps.sleep_impl.clone().unwrap_or_else(|| {
        let default: SleepFn = Arc::new(default_sleep);
        default
    });

    ModelCaller {
        deps,
        http_client,
        request_timeout_ms,
        hedge_floor_ms,
        latency_store,
        sleep,
        latency_samples: std::sync::Mutex::new(seeded),
        first_token_samples: std::sync::Mutex::new(std::collections::HashMap::new()),
        codex_tool_lane: tokio::sync::Mutex::new(None),
        codex_summary_lane: tokio::sync::Mutex::new(None),
        reasoning_default_disabled: std::sync::atomic::AtomicBool::new(false),
        streaming_disabled: std::sync::atomic::AtomicBool::new(false),
    }
}

type RawResponseParts = (
    u16,
    reqwest::header::HeaderMap,
    Result<Vec<u8>, reqwest::Error>,
);
type RawResponse = Result<RawResponseParts, reqwest::Error>;

enum RequestOutcome {
    Completed(RawResponseParts),
    Failed(reqwest::Error),
    TimedOut,
    Stopped,
}

impl RequestOutcome {
    fn describe(&self) -> &'static str {
        match self {
            RequestOutcome::Completed(_) => "completed",
            RequestOutcome::Failed(_) => "failed",
            RequestOutcome::TimedOut => "timed out",
            RequestOutcome::Stopped => "stopped",
        }
    }
}

async fn wait_for_abort(signal: &AbortSignal) {
    loop {
        if signal.is_aborted() {
            return;
        }

        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// One bound spans the request AND its body read: a non-streaming completion
/// arrives as one body, so a stalled upstream can hang either phase. The
/// caller distinguishes a timeout from a run-level abort through the outcome.
async fn run_bounded_request(
    request_fut: impl Future<Output = RawResponse>,
    timeout_ms: u64,
    signal: Option<&AbortSignal>,
) -> RequestOutcome {
    let bounded = tokio::time::timeout(Duration::from_millis(timeout_ms), request_fut);

    let result = match signal {
        Some(signal) => tokio::select! {
            () = wait_for_abort(signal) => return RequestOutcome::Stopped,
            result = bounded => result,
        },
        None => bounded.await,
    };

    match result {
        Ok(Ok(parts)) => RequestOutcome::Completed(parts),
        Ok(Err(error)) => RequestOutcome::Failed(error),
        Err(_) => RequestOutcome::TimedOut,
    }
}

/// Bounds a codex-side wait (the lane mutex, the bridge spawn) with the
/// per-attempt request deadline and the run's abort signal, mirroring
/// run_bounded_request's outcome handling: an aborted run surfaces as the
/// canonical stop error and a deadline overrun as an error, instead of the
/// wait blocking the caller uninterruptibly.
async fn bounded_codex_wait<T>(
    wait_fut: impl Future<Output = T>,
    timeout_ms: u64,
    signal: Option<&AbortSignal>,
) -> Result<T, ModelCallError> {
    let bounded = tokio::time::timeout(Duration::from_millis(timeout_ms), wait_fut);

    let result = match signal {
        Some(signal) => tokio::select! {
            () = wait_for_abort(signal) => {
                return Err(ModelCallError::Message("The run was stopped.".to_string()));
            }
            result = bounded => result,
        },
        None => bounded.await,
    };

    result.map_err(|_| {
        ModelCallError::Message(format!("codex bridge wait timed out after {timeout_ms}ms"))
    })
}

fn build_header_map(headers: &[(String, String)]) -> Result<reqwest::header::HeaderMap, String> {
    let mut map = reqwest::header::HeaderMap::new();

    for (name, value) in headers {
        let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
            .map_err(|error| format!("invalid header {name:?}: {error}"))?;
        let value = reqwest::header::HeaderValue::from_str(value)
            .map_err(|error| format!("invalid header {name:?}: {error}"))?;
        map.append(name, value);
    }

    Ok(map)
}

fn backoff_seconds(attempt: u32) -> f64 {
    let index = attempt.saturating_sub(1) as usize;

    match RATE_LIMIT_BACKOFF_SECONDS.get(index) {
        Some(seconds) => *seconds as f64,
        None => *RATE_LIMIT_BACKOFF_SECONDS.last().unwrap_or(&34) as f64,
    }
}

impl ModelCaller {
    /// The bound for one attempt: the stall-aware bound on the first attempt
    /// once the model has STALL_TIMEOUT_MIN_SAMPLES completed calls, the full
    /// per-attempt timeout otherwise (and always on retries).
    fn attempt_timeout_ms(&self, model: &str, attempt: u32) -> u64 {
        if attempt > 1 {
            return self.request_timeout_ms;
        }
        let samples = self
            .latency_samples
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match samples.get(model) {
            Some(recent) => stall_timeout_ms(
                &recent.iter().copied().collect::<Vec<_>>(),
                self.request_timeout_ms,
            ),
            None => self.request_timeout_ms,
        }
    }

    /// The hedge point for this call's first attempt, if the model has enough history.
    fn first_token_delay_for(&self, model: &str, wall_delay_ms: u64) -> u64 {
        let samples = self
            .first_token_samples
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let recent: Vec<u64> = samples
            .get(model)
            .map(|r| r.iter().copied().collect())
            .unwrap_or_default();
        first_token_hedge_ms(&recent, wall_delay_ms)
    }

    fn record_first_token(&self, model: &str, first_token_ms: i64) {
        let mut samples = self
            .first_token_samples
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let recent = samples.entry(model.to_string()).or_default();
        recent.push_back(first_token_ms.max(0) as u64);
        while recent.len() > STALL_LATENCY_SAMPLES {
            recent.pop_front();
        }
    }

    fn hedge_delay_for(&self, model: &str, attempt: u32, timeout_ms: u64) -> Option<u64> {
        if attempt > 1 {
            return None;
        }
        let samples = self
            .latency_samples
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let recent: Vec<u64> = samples
            .get(model)
            .map(|r| r.iter().copied().collect())
            .unwrap_or_default();
        hedge_delay_ms(&recent, self.hedge_floor_ms, timeout_ms)
    }

    /// Races a second identical request once the first has run `delay_ms`
    /// without answering; the first outcome of either wins and the loser is
    /// dropped (its connection closes with the future).
    async fn run_hedged_request<F, Fut>(
        &self,
        build: F,
        delay_ms: u64,
        timeout_ms: u64,
        model: &str,
        first_token_delay: Option<u64>,
    ) -> (RequestOutcome, bool, Option<i64>)
    where
        F: Fn() -> (Fut, Arc<RequestProgress>),
        Fut: Future<Output = RawResponse>,
    {
        let signal = self.deps.signal.as_ref();
        let started = Instant::now();
        let (primary_future, primary_progress) = build();
        let primary = run_bounded_request(primary_future, timeout_ms, signal);
        tokio::pin!(primary);
        let reason = if let Some(first_token_delay) = first_token_delay {
            // A streaming primary shows its health: hedge when no first
            // token has arrived by the first-token point, or when the
            // stream goes quiet mid-reply; a reply that is streaming
            // normally is never raced however long it takes.
            loop {
                let poll = tokio::time::sleep(Duration::from_millis(HEDGE_POLL_MS));
                tokio::pin!(poll);
                tokio::select! {
                    outcome = &mut primary => return (outcome, false, primary_progress.first_token_ms()),
                    () = &mut poll => {}
                }
                let elapsed_ms = started.elapsed().as_millis() as u64;
                if primary_progress.first_token_seen() {
                    if let Some(quiet) = primary_progress
                        .since_last_byte()
                        .filter(|quiet| quiet.as_millis() as u64 >= STREAM_STALL_HEDGE_MS)
                    {
                        break format!(
                            "has streamed nothing for {:.1}s after its first token",
                            quiet.as_secs_f64()
                        );
                    }
                } else if elapsed_ms >= first_token_delay {
                    break format!(
                        "has sent no first token after {:.1}s",
                        elapsed_ms as f64 / 1000.0
                    );
                }
            }
        } else {
            let delay = tokio::time::sleep(Duration::from_millis(delay_ms));
            tokio::pin!(delay);
            tokio::select! {
                outcome = &mut primary => return (outcome, false, None),
                () = &mut delay => {}
            }
            format!(
                "has not answered after {:.1}s ({}× its typical latency)",
                delay_ms as f64 / 1000.0,
                HEDGE_MULTIPLIER
            )
        };
        self.emit(
            HarnessEventType::HarnessOp,
            format!("hedged model request: {model} {reason} — racing a second request"),
            None,
        );
        let (hedge_future, hedge_progress) = build();
        let hedge = run_bounded_request(hedge_future, timeout_ms, signal);
        tokio::pin!(hedge);
        let (winner, outcome, hedge_won) = tokio::select! {
            outcome = &mut primary => ("first request", outcome, false),
            outcome = &mut hedge => ("second request", outcome, true),
        };
        let elapsed_ms = started.elapsed().as_millis() as u64;
        self.emit(
            HarnessEventType::HarnessOp,
            format!(
                "hedge resolved: the {winner} {} after {elapsed_ms}ms",
                outcome.describe()
            ),
            None,
        );
        let first_token_ms = if hedge_won {
            hedge_progress.first_token_ms()
        } else {
            primary_progress.first_token_ms()
        };
        (outcome, hedge_won, first_token_ms)
    }

    fn record_latency(&self, model: &str, latency_ms: i64) {
        let mut samples = self
            .latency_samples
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let recent = samples.entry(model.to_string()).or_default();
        recent.push_back(latency_ms.max(0) as u64);
        while recent.len() > STALL_LATENCY_SAMPLES {
            recent.pop_front();
        }
        if let Some(path) = self.latency_store.as_deref() {
            save_latency_store(path, &samples);
        }
    }

    fn emit(&self, event_type: HarnessEventType, detail: String, data: Option<HarnessEventData>) {
        (self.deps.emit)(HarnessEvent {
            data,
            detail,
            iteration: (self.deps.get_iteration)(),
            r#type: event_type,
        });
    }

    pub async fn call_model(
        &self,
        messages: Vec<TransportRequestMessage>,
        call_options: Option<ModelCallOptions>,
    ) -> Result<OpenAICompatibleResponse, ModelCallError> {
        let call_options = call_options.unwrap_or_default();
        let include_tools = call_options.include_tools != Some(false);
        // Tool-bearing calls route to the loop role's model when it has one, then
        // the run's tool-calling model; text-only calls (run summaries) use the
        // general model.
        let primary_route = if include_tools {
            call_options
                .route
                .clone()
                .or_else(|| self.deps.tool_route.clone())
        } else {
            None
        };
        let request_tools = call_options
            .transport_tools
            .clone()
            .unwrap_or_else(|| self.deps.default_transport_tools.clone());
        // A text-only call (a run summary) has no route object, so its fallback comes
        // from the run's base wiring — otherwise those calls would sit out the full
        // ladder against a dead gateway with nowhere to go.
        let effective_fallback_route = match &primary_route {
            Some(route) => route.fallback_route.clone().map(|boxed| *boxed),
            None => self.deps.fallback_route.clone(),
        };
        // Measured across the whole call, not one attempt: after a failover the
        // caller genuinely waited for the primary's ladder too, and reporting only
        // the winning attempt would hide that cost.
        let call_started_at = Instant::now();

        // Walk the fallback chain: primary, then each route's own fallback in turn.
        // Every route that still has a fallback behind it gets the short ladder;
        // the last one gets the full ladder, so a call's total patience lands on
        // the route that can actually serve it. Each hop's failure is recorded so
        // the final error names every route that was tried.
        let mut failures: Vec<String> = Vec::new();
        let mut route = primary_route;
        let mut next_route = effective_fallback_route;
        // A role route whose codex executable cannot even be spawned is a
        // local, permanent failure: instead of ending the run ("codex
        // executable not found" killed a recorded run at its replanning
        // loop), the call falls through to the run's base model once.
        let mut base_fallback_used = false;

        loop {
            let max_attempts = if next_route.is_some() {
                FALLBACK_PRIMARY_MAX_ATTEMPTS
            } else {
                RATE_LIMIT_MAX_ATTEMPTS
            };

            match self
                .attempt_route(
                    route.as_ref(),
                    max_attempts,
                    &messages,
                    include_tools,
                    &request_tools,
                    &call_options,
                    call_started_at,
                )
                .await
            {
                Ok(response) => return Ok(response),
                Err(error) => {
                    // Failover covers gateway failures only: a context overflow is the
                    // harness's cue to fold the transcript (another gateway would overflow
                    // identically), and a stopped run must stay stopped. Either surfaces
                    // as-is even mid-chain.
                    if error.is_context_overflow()
                        || self
                            .deps
                            .signal
                            .as_ref()
                            .is_some_and(|signal| signal.is_aborted())
                        || is_run_stopped_error(&error)
                    {
                        return Err(error);
                    }

                    let message = error.message().to_string();

                    failures.push(message.clone());

                    let Some(next) = next_route else {
                        if route.is_some()
                            && !base_fallback_used
                            && is_codex_spawn_failure(&message)
                        {
                            self.emit(
                                HarnessEventType::RunWarning,
                                format!(
                                    "role route {} (codex) is unavailable ({message}) — falling back to the run's base model {} for this call",
                                    route.as_ref().map(|route| route.model.clone()).unwrap_or_default(),
                                    self.deps.model
                                ),
                                None,
                            );
                            base_fallback_used = true;
                            route = None;
                            next_route = None;
                            continue;
                        }
                        if failures.len() == 1 {
                            return Err(error);
                        }

                        if failures.len() == 2 {
                            return Err(ModelCallError::Message(format!(
                                "Both model routes failed. Primary route: {} Fallback route: {}",
                                failures[0], failures[1]
                            )));
                        }

                        let primary_message = failures[0].clone();
                        let fallback_messages = failures[1..]
                            .iter()
                            .enumerate()
                            .map(|(index, text)| format!("Fallback route {}: {}", index + 1, text))
                            .collect::<Vec<_>>()
                            .join(" ");

                        return Err(ModelCallError::Message(format!(
                            "All {} model routes failed. Primary route: {} {}",
                            failures.len(),
                            primary_message,
                            fallback_messages
                        )));
                    };

                    let hop = failures.len() as i64 - 1;
                    let label = if hop == 0 {
                        "primary model route".to_string()
                    } else {
                        format!(
                            "fallback route {} ({})",
                            hop,
                            route
                                .as_ref()
                                .map(|route| route.model.clone())
                                .unwrap_or_else(|| self.deps.model.clone())
                        )
                    };

                    self.emit(
                        HarnessEventType::RunWarning,
                        format!(
                            "{label} failed ({message}) — failing over to the next fallback route ({})",
                            next.model
                        ),
                        None,
                    );

                    route = Some(next.clone());
                    next_route = next.fallback_route.map(|boxed| *boxed);
                }
            }
        }
    }

    /// One attempt = the whole retry ladder against one route. Everything the
    /// request shape depends on (Anthropic-native vs OpenAI-compatible, url,
    /// headers, model) is re-derived from the route handed in, so a failover to
    /// a different gateway never inherits the primary's surface decisions.
    /// Bridge configuration for a codex call: spawn `codex app-server` in the
    /// harness cwd with the call's model and the run's per-attempt timeout.
    /// Per-call knobs (tools, reasoning effort) ride the call, not the config.
    fn codex_bridge_config(&self, model: &str) -> BridgeConfig {
        BridgeConfig {
            cwd: self.deps.cwd.as_deref().map(std::path::PathBuf::from),
            executable: self
                .deps
                .codex_executable
                .clone()
                .unwrap_or_else(|| "codex".to_string()),
            model: Some(model.to_string()),
            request_timeout_ms: self.request_timeout_ms,
            ..BridgeConfig::default()
        }
    }

    /// One codex attempt = a single bridge call. The bridge owns its own
    /// network retries and the caller's configured fallback ladder owns
    /// failover, so auth/config/protocol failures surface immediately instead
    /// of burning a rate-limit backoff ladder. Tool calls and text-only
    /// summaries ride separate lanes so a summary can never interleave with
    /// the run's pending tool request, and a model change replaces the lane's
    /// bridge.
    #[allow(clippy::too_many_arguments)]
    async fn attempt_codex_route(
        &self,
        model: String,
        messages: &[TransportRequestMessage],
        include_tools: bool,
        request_tools: &[OpenAICompatibleRequestTool],
        call_options: &ModelCallOptions,
        reasoning_effort: Option<String>,
        call_started_at: Instant,
    ) -> Result<OpenAICompatibleResponse, ModelCallError> {
        let lane = if include_tools {
            &self.codex_tool_lane
        } else {
            &self.codex_summary_lane
        };
        let deadline = tokio::time::Instant::now() + Duration::from_millis(self.request_timeout_ms);
        let remaining_ms = || {
            deadline
                .saturating_duration_since(tokio::time::Instant::now())
                .as_millis() as u64
        };
        // Summary calls carry no tools; tool calls get the call's filtered
        // dynamic tool set.
        let tools_for_lane: &[OpenAICompatibleRequestTool] =
            if include_tools { request_tools } else { &[] };

        if self
            .deps
            .signal
            .as_ref()
            .is_some_and(|signal| signal.is_aborted())
        {
            return Err(ModelCallError::Message("The run was stopped.".to_string()));
        }

        // Serializing on the lane keeps one pending request per bridge; the
        // guard is held across the call because a codex bridge serves a single
        // turn at a time. The lock wait and the spawn are both bounded by the
        // request deadline and the abort signal — an uninterruptible wait here
        // would wedge the caller past a stop or a stalled handshake.
        let mut lane_guard =
            bounded_codex_wait(lane.lock(), remaining_ms(), self.deps.signal.as_ref()).await?;

        let reuse = lane_guard
            .as_ref()
            .is_some_and(|bridge| bridge.config.model.as_deref() == Some(model.as_str()));

        if !reuse {
            // Replacing the bridge (model/profile change) drops the old one,
            // killing its child process, before the new one spawns. A spawn
            // that loses the race against the deadline/abort leaves no owner
            // holding the child, so kill_on_drop reaps it.
            *lane_guard = None;
            let bridge = match bounded_codex_wait(
                CodexBridge::spawn(self.codex_bridge_config(&model)),
                remaining_ms(),
                self.deps.signal.as_ref(),
            )
            .await
            {
                Ok(Ok(bridge)) => bridge,
                Ok(Err(error)) | Err(error) => return Err(error),
            };
            *lane_guard = Some(bridge);
        }

        let bridge = lane_guard.as_mut().expect("codex bridge is spawned above");
        let outcome = bounded_codex_wait(
            bridge.call(
                messages,
                tools_for_lane,
                self.deps.signal.as_ref(),
                reasoning_effort.as_deref(),
            ),
            remaining_ms(),
            self.deps.signal.as_ref(),
        )
        .await
        .and_then(|outcome| outcome);

        match outcome {
            Ok(response) => {
                (self.deps.on_usage)(
                    &response,
                    ModelCallRecord {
                        latency_ms: call_started_at.elapsed().as_millis() as i64,
                        model,
                        provider: Some("codex".to_string()),
                        task_id: call_options.usage_task_id.clone(),
                        ..Default::default()
                    },
                );

                Ok(response)
            }
            Err(error) => {
                // A failed call leaves the bridge in an unknown state (the
                // child may be mid-turn or already killed) — discard it so the
                // next call spawns a fresh process. The error itself goes
                // straight out: the bridge owns its network retries and the
                // caller's fallback ladder owns failover.
                *lane_guard = None;

                if self
                    .deps
                    .signal
                    .as_ref()
                    .is_some_and(|signal| signal.is_aborted())
                    || is_run_stopped_error(&error)
                {
                    // Stopped runs are never retried.
                    return Err(ModelCallError::Message("The run was stopped.".to_string()));
                }

                Err(error)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn attempt_route(
        &self,
        route: Option<&ModelRoute>,
        max_attempts: u32,
        messages: &[TransportRequestMessage],
        include_tools: bool,
        request_tools: &[OpenAICompatibleRequestTool],
        call_options: &ModelCallOptions,
        call_started_at: Instant,
    ) -> Result<OpenAICompatibleResponse, ModelCallError> {
        // Text-only calls carry no route, so they resolve to the run's base
        // provider; a routed call uses that route's own provider.
        let provider = match route {
            Some(route) => route.provider.clone(),
            None => self.deps.provider.clone(),
        };
        let anthropic_native = is_anthropic_native_provider(provider.as_deref());
        // The codex provider speaks the `codex app-server` JSON-RPC protocol
        // over stdio, not HTTPS — dispatch before any HTTP header or reqwest
        // work happens for the attempt.
        if provider.as_deref() == Some("codex") {
            let model = route
                .map(|route| route.model.clone())
                .unwrap_or_else(|| self.deps.model.clone());
            let (reasoning_effort, _effort_defaulted) = self.effective_reasoning_effort(route);

            return self
                .attempt_codex_route(
                    model,
                    messages,
                    include_tools,
                    request_tools,
                    call_options,
                    reasoning_effort,
                    call_started_at,
                )
                .await;
        }
        // HTTP credentials are resolved only after local-provider dispatch.
        let route_headers: Option<Vec<(String, String)>> = match route {
            Some(route) => {
                let mut headers =
                    vec![("content-type".to_string(), "application/json".to_string())];
                let route_headers = match &route.refresh_headers {
                    Some(refresh_headers) => refresh_headers().map_err(ModelCallError::Message)?,
                    None => route.headers.clone().unwrap_or_default(),
                };
                headers.extend(route_headers);
                Some(headers)
            }
            None => None,
        };
        let request_url = route
            .map(|route| route.url.clone())
            .unwrap_or_else(|| self.deps.url.clone());
        let request_headers = match route_headers {
            Some(headers) => headers,
            None => match &self.deps.refresh_headers {
                Some(refresh_headers) => refresh_headers().map_err(ModelCallError::Message)?,
                None => self.deps.headers.clone(),
            },
        };
        let model = route
            .map(|route| route.model.clone())
            .unwrap_or_else(|| self.deps.model.clone());
        let (reasoning_effort, effort_defaulted) = self.effective_reasoning_effort(route);
        let request_body = if anthropic_native {
            serde_json::to_value(build_anthropic_request_payload(
                BuildAnthropicRequestPayloadArgs {
                    // One-shot calls (run summaries) never re-read their prefix, so a
                    // cache write would be a pure premium.
                    cache: Some(include_tools),
                    max_tokens: call_options.max_tokens,
                    messages: messages.to_vec(),
                    model: model.clone(),
                    tools: if include_tools {
                        Some(request_tools.to_vec())
                    } else {
                        None
                    },
                },
            ))
            .map_err(|error| {
                ModelCallError::Message(format!(
                    "failed to serialize the Anthropic request payload: {error}"
                ))
            })?
        } else {
            serde_json::to_value(build_transport_request_payload(
                BuildTransportRequestPayloadArgs {
                    messages: messages.to_vec(),
                    model: model.clone(),
                    prompt_cache_key: if provider.as_deref() == Some("openai") {
                        self.deps.prompt_cache_key.clone()
                    } else {
                        None
                    },
                    reasoning_effort,
                    tools: if include_tools {
                        Some(request_tools.to_vec())
                    } else {
                        None
                    },
                    max_tokens: call_options.max_tokens,
                    tool_choice: call_options.tool_choice.clone(),
                },
            ))
            .map_err(|error| {
                ModelCallError::Message(format!("failed to serialize the request payload: {error}"))
            })?
        };
        // For xAI, send the routing hint as a header instead of a body field.
        let effective_request_headers = if provider.as_deref() == Some("xai") {
            match &self.deps.prompt_cache_key {
                Some(prompt_cache_key) => {
                    let has_header = request_headers
                        .iter()
                        .any(|(name, _)| name.eq_ignore_ascii_case("x-grok-conv-id"));

                    if has_header {
                        request_headers
                    } else {
                        let mut headers = request_headers;

                        headers.push(("x-grok-conv-id".to_string(), prompt_cache_key.clone()));
                        headers
                    }
                }
                None => request_headers,
            }
        } else {
            request_headers
        };
        let wire_headers = if anthropic_native {
            build_anthropic_headers(&effective_request_headers)
        } else {
            effective_request_headers
        };
        let header_map = build_header_map(&wire_headers).map_err(ModelCallError::Message)?;
        let url = if anthropic_native {
            build_anthropic_messages_url(&request_url)
        } else {
            request_url
        };
        let body = request_body.to_string();

        // Network-level rejections (connection refused/reset, DNS) are the
        // normal failure mode of a local inference server restarting under
        // load — retry on the same ladder as rate limits instead of killing an
        // hours-long run. A request that hit the per-attempt timeout is the
        // same class of failure: the upstream is not answering. Returns once
        // the backoff has elapsed; throws when the ladder is exhausted.
        async fn wait_out_unreachable(
            model_caller: &ModelCaller,
            attempt: u32,
            max_attempts: u32,
            message: &str,
            call_started_at: Instant,
        ) -> Result<(), ModelCallError> {
            if attempt >= max_attempts {
                return Err(ModelCallError::Message(format!(
                    "The inference endpoint could not be reached after {max_attempts} attempts: {message}"
                )));
            }
            let elapsed_ms = call_started_at.elapsed().as_millis() as u64;
            if elapsed_ms >= MAX_UNREACHABLE_WALL_MS {
                return Err(ModelCallError::Message(format!(
                    "The inference endpoint could not be reached within {}s ({attempt} attempts): {message}",
                    MAX_UNREACHABLE_WALL_MS / 1000
                )));
            }

            let wait_seconds = backoff_seconds(attempt);

            model_caller.emit(
                HarnessEventType::RateLimited,
                format!(
                    "endpoint unreachable ({message}) — waiting {wait_seconds}s before attempt {}/{}",
                    attempt + 1,
                    max_attempts
                ),
                Some(HarnessEventData {
                    wait_seconds: Some(wait_seconds),
                    ..Default::default()
                }),
            );
            (model_caller.deps.on_retry_wait)(wait_seconds);
            (model_caller.sleep)(
                (wait_seconds * 1000.0) as u64,
                model_caller.deps.signal.clone(),
            )
            .await;

            if model_caller
                .deps
                .signal
                .as_ref()
                .is_some_and(|signal| signal.is_aborted())
            {
                return Err(ModelCallError::Message(
                    "The run was stopped while waiting out an endpoint outage.".to_string(),
                ));
            }

            Ok(())
        }

        let mut attempt: u32 = 1;

        loop {
            let attempt_timeout_ms = self.attempt_timeout_ms(&model, attempt);
            // OpenAI-compatible providers get a streaming request: the
            // hedge then watches the first token instead of the clock, and
            // the chunks are folded back into one response body below.
            let streaming = !anthropic_native
                && !self
                    .streaming_disabled
                    .load(std::sync::atomic::Ordering::Relaxed);
            let wire_body = if streaming {
                with_stream_fields(&request_body)
            } else {
                body.clone()
            };
            let build_request = || {
                let client = self.http_client.clone();
                let url = url.clone();
                let header_map = header_map.clone();
                let body = wire_body.clone();
                let progress = Arc::new(RequestProgress::new());
                let tracker = progress.clone();

                (
                    async move {
                        let response = client
                            .post(&url)
                            .headers(header_map)
                            .body(body)
                            .send()
                            .await?;
                        let status = response.status().as_u16();
                        let headers = response.headers().clone();
                        let body = if streaming && is_event_stream(&headers) {
                            read_event_stream(response, &tracker)
                                .await
                                .map(|sse| assemble_streamed_response(&sse))
                        } else {
                            response.bytes().await.map(|bytes| bytes.to_vec())
                        };

                        Ok((status, headers, body))
                    },
                    progress,
                )
            };
            let (outcome, call_hedged, call_hedge_won, call_first_token_ms) = match self
                .hedge_delay_for(&model, attempt, attempt_timeout_ms)
            {
                Some(delay_ms) => {
                    let first_token_delay =
                        streaming.then(|| self.first_token_delay_for(&model, delay_ms));
                    let (outcome, hedge_won, first_token_ms) = self
                        .run_hedged_request(
                            &build_request,
                            delay_ms,
                            attempt_timeout_ms,
                            &model,
                            first_token_delay,
                        )
                        .await;
                    (outcome, true, hedge_won, first_token_ms)
                }
                None => {
                    let (future, progress) = build_request();
                    let outcome =
                        run_bounded_request(future, attempt_timeout_ms, self.deps.signal.as_ref())
                            .await;
                    (outcome, false, false, progress.first_token_ms())
                }
            };

            match outcome {
                RequestOutcome::Stopped => {
                    return Err(ModelCallError::Message("The run was stopped.".to_string()));
                }

                RequestOutcome::Failed(error) => {
                    if self
                        .deps
                        .signal
                        .as_ref()
                        .is_some_and(|signal| signal.is_aborted())
                    {
                        return Err(ModelCallError::Message("The run was stopped.".to_string()));
                    }

                    let message = error.to_string();

                    if !is_network_transport_error(&error) {
                        return Err(ModelCallError::Message(message));
                    }

                    wait_out_unreachable(self, attempt, max_attempts, &message, call_started_at)
                        .await?;
                    attempt += 1;
                    continue;
                }
                RequestOutcome::TimedOut => {
                    // The headers arrived but the body never finished inside the bound.
                    let message = if attempt_timeout_ms < self.request_timeout_ms {
                        format!(
                            "{} — {}× this model's typical latency; the retry gets the full {}s",
                            request_timeout_message(attempt_timeout_ms),
                            STALL_TIMEOUT_MULTIPLIER,
                            (self.request_timeout_ms as f64 / 1000.0).round() as i64
                        )
                    } else {
                        request_timeout_message(attempt_timeout_ms)
                    };
                    wait_out_unreachable(self, attempt, max_attempts, &message, call_started_at)
                        .await?;
                    attempt += 1;
                    continue;
                }
                RequestOutcome::Completed((status, response_headers, body)) => {
                    let (data, parsed) = match body {
                        // Non-JSON bodies (HTML error pages, empty responses) fall through to the status check below.
                        Err(_) => (OpenAICompatibleResponse::default(), false),
                        Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
                            Err(_) => (OpenAICompatibleResponse::default(), false),
                            Ok(raw) => {
                                // Translating before the status checks keeps the retry ladder
                                // provider-agnostic: native error envelopes ({type:"error", error})
                                // land on data.error exactly like OpenAI-compatible ones.
                                if anthropic_native {
                                    (translate_anthropic_response(&raw).into(), true)
                                } else {
                                    match serde_json::from_value::<OpenAICompatibleResponse>(raw) {
                                        Ok(data) => (data, true),
                                        Err(_) => (OpenAICompatibleResponse::default(), false),
                                    }
                                }
                            }
                        },
                    };

                    if self
                        .deps
                        .signal
                        .as_ref()
                        .is_some_and(|signal| signal.is_aborted())
                    {
                        return Err(ModelCallError::Message("The run was stopped.".to_string()));
                    }

                    if status == 429 {
                        if is_non_retryable_quota_error(data.error.as_ref()) {
                            return Err(ModelCallError::Message(format!(
                                "{} [harness: quota/billing errors are not retried — fix the account or switch model profiles]",
                                data
                                    .error
                                    .as_ref()
                                    .and_then(|error| error.message.clone())
                                    .unwrap_or_else(|| "The inference endpoint reported exhausted quota (429).".to_string())
                            )));
                        }

                        if attempt >= max_attempts {
                            return Err(ModelCallError::Message(
                                data.error
                                    .as_ref()
                                    .and_then(|error| error.message.clone())
                                    .unwrap_or_else(|| {
                                        format!("The inference endpoint rate limited the run (429) {max_attempts} times in a row.")
                                    }),
                            ));
                        }

                        let backoff_seconds = backoff_seconds(attempt);
                        let retry_after_seconds = response_headers
                            .get("retry-after")
                            .and_then(|value| value.to_str().ok())
                            .and_then(|value| value.trim().parse::<f64>().ok())
                            .filter(|value| value.is_finite());
                        let wait_seconds = match retry_after_seconds {
                            Some(retry_after_seconds) => backoff_seconds.max(retry_after_seconds),
                            None => backoff_seconds,
                        };

                        self.emit(
                            HarnessEventType::RateLimited,
                            format!(
                                "429 rate limited — waiting {wait_seconds}s before attempt {}/{}",
                                attempt + 1,
                                max_attempts
                            ),
                            Some(HarnessEventData {
                                wait_seconds: Some(wait_seconds),
                                ..Default::default()
                            }),
                        );
                        (self.deps.on_retry_wait)(wait_seconds);
                        (self.sleep)((wait_seconds * 1000.0) as u64, self.deps.signal.clone())
                            .await;

                        if self
                            .deps
                            .signal
                            .as_ref()
                            .is_some_and(|signal| signal.is_aborted())
                        {
                            return Err(ModelCallError::Message(
                                "The run was stopped while waiting out a rate limit.".to_string(),
                            ));
                        }

                        attempt += 1;
                        continue;
                    }

                    if status >= 500 {
                        if attempt >= max_attempts {
                            return Err(ModelCallError::Message(
                                data.error
                                    .as_ref()
                                    .and_then(|error| error.message.clone())
                                    .unwrap_or_else(|| {
                                        format!("The inference endpoint returned {status} {max_attempts} times in a row.")
                                    }),
                            ));
                        }

                        let wait_seconds = backoff_seconds(attempt);

                        self.emit(
                            HarnessEventType::RateLimited,
                            format!(
                                "{status} from the endpoint — waiting {wait_seconds}s before attempt {}/{}",
                                attempt + 1,
                                max_attempts
                            ),
                            Some(HarnessEventData {
                                wait_seconds: Some(wait_seconds),
                                ..Default::default()
                            }),
                        );
                        (self.deps.on_retry_wait)(wait_seconds);
                        (self.sleep)((wait_seconds * 1000.0) as u64, self.deps.signal.clone())
                            .await;

                        if self
                            .deps
                            .signal
                            .as_ref()
                            .is_some_and(|signal| signal.is_aborted())
                        {
                            return Err(ModelCallError::Message(
                                "The run was stopped while waiting out an endpoint outage."
                                    .to_string(),
                            ));
                        }

                        attempt += 1;
                        continue;
                    }

                    if !(200..300).contains(&status) {
                        let message = data
                            .error
                            .as_ref()
                            .and_then(|error| error.message.clone())
                            .unwrap_or_else(|| format!("Request failed with {status}."));

                        // Context-window overflows are recoverable by folding the loop's
                        // transcript; everything else stays fatal to the call.
                        if (status == 400 || status == 413)
                            && context_overflow_regex().is_match(&message)
                        {
                            return Err(ContextOverflowError(message).into());
                        }

                        // A provider that rejects streaming requests: send
                        // non-streaming ones for the rest of the run.
                        if status == 400
                            && streaming
                            && streaming_rejection_regex().is_match(&message)
                            && !self
                                .streaming_disabled
                                .swap(true, std::sync::atomic::Ordering::Relaxed)
                        {
                            self.emit(
                                HarnessEventType::RunWarning,
                                format!(
                                    "the provider rejected the streaming request ({message}) — retrying without streaming, and sending non-streaming requests for the rest of the run"
                                ),
                                None,
                            );
                            continue;
                        }
                        // The harness-default effort on a provider that does not
                        // take the field: drop it for the run and retry this call.
                        if status == 400
                            && effort_defaulted
                            && reasoning_rejection_regex().is_match(&message)
                            && !self
                                .reasoning_default_disabled
                                .swap(true, std::sync::atomic::Ordering::Relaxed)
                        {
                            self.emit(
                                HarnessEventType::RunWarning,
                                format!(
                                    "the provider rejected the harness-default reasoning effort ({message}) — retrying without it, and omitting it for the rest of the run"
                                ),
                                None,
                            );
                            continue;
                        }

                        return Err(ModelCallError::Message(message));
                    }

                    if let Some(message) =
                        data.error.as_ref().and_then(|error| error.message.clone())
                    {
                        return Err(ModelCallError::Message(message));
                    }

                    // A 200 with an unparseable body or no choices is a protocol failure,
                    // not a "model chose to do nothing" — and in practice a transient one
                    // (a gateway answering an upstream stall with an empty envelope), so it
                    // takes the same ladder as a 5xx. Only a sustained run of them fails
                    // the call, which still keeps a flaky endpoint from silently burning
                    // the stall budget toward an auto-block.
                    if !parsed || data.choices.is_none() {
                        if attempt >= max_attempts {
                            return Err(ModelCallError::Message(format!(
                                "The inference endpoint returned a 200 response with no choices array {max_attempts} times in a row."
                            )));
                        }

                        let wait_seconds = backoff_seconds(attempt);

                        self.emit(
                            HarnessEventType::RateLimited,
                            format!(
                                "200 with no choices array from the endpoint — waiting {wait_seconds}s before attempt {}/{}",
                                attempt + 1,
                                max_attempts
                            ),
                            Some(HarnessEventData {
                                wait_seconds: Some(wait_seconds),
                                ..Default::default()
                            }),
                        );
                        (self.deps.on_retry_wait)(wait_seconds);
                        (self.sleep)((wait_seconds * 1000.0) as u64, self.deps.signal.clone())
                            .await;

                        if self
                            .deps
                            .signal
                            .as_ref()
                            .is_some_and(|signal| signal.is_aborted())
                        {
                            return Err(ModelCallError::Message(
                                "The run was stopped while waiting out an endpoint outage."
                                    .to_string(),
                            ));
                        }

                        attempt += 1;
                        continue;
                    }

                    let latency_ms = call_started_at.elapsed().as_millis() as i64;
                    self.record_latency(&model, latency_ms);
                    if let Some(first_token_ms) = call_first_token_ms {
                        self.record_first_token(&model, first_token_ms);
                    }
                    (self.deps.on_usage)(
                        &data,
                        ModelCallRecord {
                            latency_ms,
                            model,
                            provider,
                            task_id: call_options.usage_task_id.clone(),
                            hedged: call_hedged,
                            hedge_won: call_hedge_won,
                            first_token_ms: call_first_token_ms,
                        },
                    );

                    return Ok(data);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn routes_and_profiles_without_an_effort_get_the_harness_default() {
        let mut deps = test_deps("http://127.0.0.1:1/v1/chat/completions".to_string());
        deps.reasoning_effort = Some("low".to_string());
        deps.reasoning_effort_defaulted = true;
        let caller = create_model_caller(deps);
        let route = |effort: Option<&str>| ModelRoute {
            fallback_route: None,
            headers: None,
            model: "m".to_string(),
            provider: Some("openrouter".to_string()),
            reasoning_effort: effort.map(str::to_string),
            refresh_headers: None,
            url: "http://127.0.0.1:1/v1/chat/completions".to_string(),
        };
        assert_eq!(
            caller.effective_reasoning_effort(None),
            (Some("low".to_string()), true)
        );
        assert_eq!(
            caller.effective_reasoning_effort(Some(&route(None))),
            (Some("low".to_string()), true)
        );
        assert_eq!(
            caller.effective_reasoning_effort(Some(&route(Some("high")))),
            (Some("high".to_string()), false)
        );
        caller
            .reasoning_default_disabled
            .store(true, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(caller.effective_reasoning_effort(None), (None, true));
        assert_eq!(
            caller.effective_reasoning_effort(Some(&route(None))),
            (None, true)
        );
        assert_eq!(
            caller.effective_reasoning_effort(Some(&route(Some("high")))),
            (Some("high".to_string()), false)
        );
    }

    #[test]
    fn usage_parses_reasoning_tokens_and_the_rejection_regex_is_narrow() {
        let usage: OpenAICompatibleResponseUsage = serde_json::from_str(
            r#"{"prompt_tokens":10,"completion_tokens":9000,"completion_tokens_details":{"reasoning_tokens":8700},"total_tokens":9010}"#,
        )
        .unwrap();
        assert_eq!(
            usage
                .completion_tokens_details
                .and_then(|d| d.reasoning_tokens),
            Some(8700)
        );
        let plain: OpenAICompatibleResponseUsage =
            serde_json::from_str(r#"{"prompt_tokens":1,"completion_tokens":1}"#).unwrap();
        assert!(plain.completion_tokens_details.is_none());
        assert!(reasoning_rejection_regex().is_match("Unsupported parameter: reasoning_effort"));
        assert!(reasoning_rejection_regex().is_match("unknown field `reasoning_effort`"));
        assert!(!reasoning_rejection_regex().is_match("maximum context length exceeded"));
    }
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn flags_the_non_retryable_quota_shapes_and_lets_transient_quota_wording_through() {
        let code = OpenAICompatibleResponseError {
            code: Some("insufficient_quota".to_string()),
            message: Some("You exceeded your current quota".to_string()),
            error_type: None,
        };
        let billing_type = OpenAICompatibleResponseError {
            code: None,
            message: None,
            error_type: Some("billing_hard_limit".to_string()),
        };
        let credit_balance = OpenAICompatibleResponseError {
            code: None,
            message: Some("Your credit balance is too low".to_string()),
            error_type: None,
        };
        let transient = OpenAICompatibleResponseError {
            code: None,
            message: Some("Rate limit exceeded for requests per minute quota".to_string()),
            error_type: None,
        };

        assert!(is_non_retryable_quota_error(Some(&code)));
        assert!(is_non_retryable_quota_error(Some(&billing_type)));
        assert!(is_non_retryable_quota_error(Some(&credit_balance)));
        assert!(!is_non_retryable_quota_error(Some(&transient)));
        assert!(!is_non_retryable_quota_error(None));
    }

    #[test]
    fn classifies_network_failures_and_fails_fast_on_config_errors() {
        #[derive(Debug)]
        struct MessageError(String);

        impl std::fmt::Display for MessageError {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl std::error::Error for MessageError {}

        assert!(is_network_fetch_error(&MessageError(
            "Unable to connect. Is the computer able to access the url?".to_string()
        )));
        assert!(is_network_fetch_error(&MessageError(
            "Connection refused (os error 61)".to_string()
        )));
        assert!(is_network_fetch_error(&MessageError(
            "fetch failed: connection reset".to_string()
        )));
        assert!(!is_network_fetch_error(&MessageError(
            "failed to parse url: relative URL without a base".to_string()
        )));
        assert!(!is_network_fetch_error(&MessageError(
            "invalid header value".to_string()
        )));
    }

    #[test]
    fn walks_the_backoff_ladder_and_clamps_to_the_last_rung() {
        assert_eq!(backoff_seconds(1), 1.0);
        assert_eq!(backoff_seconds(2), 1.0);
        assert_eq!(backoff_seconds(3), 2.0);
        assert_eq!(backoff_seconds(9), 34.0);
        assert_eq!(backoff_seconds(10), 34.0);
        assert_eq!(backoff_seconds(50), 34.0);
    }

    #[test]
    fn request_timeout_message_rounds_to_seconds() {
        assert_eq!(
            request_timeout_message(DEFAULT_REQUEST_TIMEOUT_MS),
            "request timed out after 240s with no response"
        );
        assert_eq!(
            request_timeout_message(1500),
            "request timed out after 2s with no response"
        );
    }

    #[test]
    fn stall_timeout_needs_samples_and_clamps_to_the_floor_and_base() {
        assert_eq!(
            stall_timeout_ms(&[8_000, 9_000], 240_000),
            240_000,
            "too few samples: full bound"
        );
        assert_eq!(
            stall_timeout_ms(&[8_000, 9_000, 7_000], 240_000),
            8_000 * STALL_TIMEOUT_MULTIPLIER,
            "median × multiplier"
        );
        assert_eq!(
            stall_timeout_ms(&[1_000, 1_000, 1_000], 240_000),
            STALL_TIMEOUT_FLOOR_MS,
            "floor"
        );
        assert_eq!(
            stall_timeout_ms(&[60_000, 70_000, 80_000], 240_000),
            240_000,
            "never above the base"
        );
        assert_eq!(
            stall_timeout_ms(&[1_000, 1_000, 1_000], 30_000),
            30_000,
            "base below the floor wins"
        );
    }

    #[test]
    fn first_attempt_uses_the_stall_bound_only_after_recorded_latencies() {
        let caller = create_model_caller(test_deps(
            "http://127.0.0.1:9/v1/chat/completions".to_string(),
        ));
        assert_eq!(
            caller.attempt_timeout_ms("m", 1),
            DEFAULT_REQUEST_TIMEOUT_MS
        );
        caller.record_latency("m", 6_000);
        caller.record_latency("m", 7_000);
        assert_eq!(
            caller.attempt_timeout_ms("m", 1),
            DEFAULT_REQUEST_TIMEOUT_MS,
            "two samples are not enough"
        );
        caller.record_latency("m", 8_000);
        assert_eq!(
            caller.attempt_timeout_ms("m", 1),
            7_000 * STALL_TIMEOUT_MULTIPLIER
        );
        assert_eq!(
            caller.attempt_timeout_ms("m", 2),
            DEFAULT_REQUEST_TIMEOUT_MS,
            "retries get the full bound"
        );
        assert_eq!(
            caller.attempt_timeout_ms("other", 1),
            DEFAULT_REQUEST_TIMEOUT_MS,
            "per model"
        );
        for _ in 0..STALL_LATENCY_SAMPLES {
            caller.record_latency("m", 20_000);
        }
        assert_eq!(
            caller.attempt_timeout_ms("m", 1),
            20_000 * STALL_TIMEOUT_MULTIPLIER,
            "window forgets old samples"
        );
    }

    #[test]
    fn is_run_stopped_error_matches_only_the_stop_prefix() {
        assert!(is_run_stopped_error(&ModelCallError::Message(
            "The run was stopped while waiting out a rate limit.".to_string()
        )));
        assert!(!is_run_stopped_error(&ModelCallError::Message(
            "The inference endpoint rate limited the run (429) 10 times in a row.".to_string()
        )));
    }

    /// HTTP/1.1 mock over a std TcpListener: accepts exactly `request_count`
    /// connections, serving the same canned response to each one, and returns a
    /// handle whose join() yields the (request line, body) pairs it received.
    /// `connection: close` on every response keeps reqwest from pooling, so
    /// each attempt opens a fresh connection for the accept loop to pick up.
    fn spawn_mock_server(
        request_count: usize,
        status: u16,
        response_body: &'static str,
    ) -> (String, std::thread::JoinHandle<Vec<(String, Vec<u8>)>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let handle = std::thread::spawn(move || {
            let mut requests: Vec<(String, Vec<u8>)> = Vec::new();

            for _ in 0..request_count {
                let (mut stream, _) = listener.accept().unwrap();
                let mut data: Vec<u8> = Vec::new();
                let mut chunk = [0u8; 4096];
                let body_start = loop {
                    let read = stream.read(&mut chunk).unwrap_or(0);

                    assert!(read > 0, "client closed before sending a full request");
                    data.extend_from_slice(&chunk[..read]);

                    if let Some(pos) = data.windows(4).position(|window| window == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&data[..pos]).to_ascii_lowercase();
                        let content_length = head
                            .lines()
                            .find_map(|line| {
                                line.strip_prefix("content-length:")
                                    .and_then(|value| value.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);

                        if data.len() >= pos + 4 + content_length {
                            break pos + 4;
                        }
                    }
                };
                let head = String::from_utf8_lossy(&data[..body_start]).to_string();
                let request_line = head.lines().next().unwrap_or_default().to_string();
                let request_body = data[body_start..].to_vec();
                let response = format!(
                    "HTTP/1.1 {status} Test\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body
                );

                stream.write_all(response.as_bytes()).unwrap();
                stream.flush().unwrap();

                requests.push((request_line, request_body));
            }

            requests
        });

        (
            format!("http://127.0.0.1:{port}/v1/chat/completions"),
            handle,
        )
    }

    /// A mock that answers one connection with a server-sent event stream
    /// (`content-type: text/event-stream`), each line written as its own
    /// chunk, so the streaming reader sees the reply arrive piecewise.
    fn spawn_sse_mock_server(
        lines: Vec<&'static str>,
    ) -> (String, std::thread::JoinHandle<Vec<u8>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut data: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 4096];
            let body_start = loop {
                let read = stream.read(&mut chunk).unwrap_or(0);
                assert!(read > 0, "client closed before sending a full request");
                data.extend_from_slice(&chunk[..read]);
                if let Some(pos) = data.windows(4).position(|window| window == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&data[..pos]).to_ascii_lowercase();
                    let content_length = head
                        .lines()
                        .find_map(|line| {
                            line.strip_prefix("content-length:")
                                .and_then(|value| value.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if data.len() >= pos + 4 + content_length {
                        break pos + 4;
                    }
                }
            };
            let body: String = lines.iter().map(|line| format!("{line}\n\n")).collect();
            let response = format!(
                "HTTP/1.1 200 Test\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            for line in &lines {
                stream.write_all(format!("{line}\n\n").as_bytes()).unwrap();
                stream.flush().unwrap();
                std::thread::sleep(Duration::from_millis(5));
            }
            data[body_start..].to_vec()
        });
        (
            format!("http://127.0.0.1:{port}/v1/chat/completions"),
            handle,
        )
    }

    #[test]
    fn a_streamed_reply_is_folded_back_into_one_response_body() {
        let sse = concat!(
            "data: {\"id\":\"r1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"}}]}\n\n",
            ": keepalive\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hel\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"lo\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"READ\",\"arguments\":\"{\\\"pa\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"th\\\":1}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":7,\"total_tokens\":12}}\n\n",
            "data: [DONE]\n\n",
        );
        let body: Value =
            serde_json::from_slice(&assemble_streamed_response(sse.as_bytes())).unwrap();
        assert_eq!(body["id"], "r1");
        assert_eq!(body["model"], "m");
        assert_eq!(body["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(body["choices"][0]["message"]["content"], "Hello");
        assert_eq!(
            body["choices"][0]["message"]["tool_calls"][0]["id"],
            "call_1"
        );
        assert_eq!(
            body["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
            "READ"
        );
        assert_eq!(
            body["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
            "{\"path\":1}"
        );
        assert_eq!(body["usage"]["completion_tokens"], 7);
        let parsed: OpenAICompatibleResponse = serde_json::from_value(body).unwrap();
        assert_eq!(parsed.usage.unwrap().total_tokens, Some(12));

        let error = "data: {\"error\":{\"message\":\"overloaded\",\"code\":503}}\n\n";
        let body: Value =
            serde_json::from_slice(&assemble_streamed_response(error.as_bytes())).unwrap();
        assert_eq!(body["error"]["message"], "overloaded");

        let empty: Value =
            serde_json::from_slice(&assemble_streamed_response(b"data: [DONE]\n\n")).unwrap();
        assert!(empty["choices"][0]["message"]["content"].is_null());
    }

    #[test]
    fn the_first_token_hedge_point_rises_with_the_median_first_token() {
        assert_eq!(
            first_token_hedge_ms(&[], 8_000),
            4_000,
            "the floor before any history"
        );
        assert_eq!(
            first_token_hedge_ms(&[900, 1_100], 8_000),
            4_000,
            "the floor with too little history"
        );
        assert_eq!(
            first_token_hedge_ms(&[900, 1_100, 1_000], 8_000),
            4_000,
            "a quick provider stays at the floor"
        );
        assert_eq!(
            first_token_hedge_ms(&[5_000, 6_000, 7_000], 20_000),
            12_000,
            "a queued window doubles its median"
        );
        assert_eq!(
            first_token_hedge_ms(&[5_000, 6_000, 7_000], 8_000),
            8_000,
            "never past the wall-clock point"
        );
        assert_eq!(
            first_token_hedge_ms(&[], 3_000),
            3_000,
            "a wall-clock point under the floor wins"
        );
    }

    #[test]
    fn a_first_token_is_a_delta_with_output_not_a_role_or_keepalive() {
        assert!(!sse_line_carries_a_token(b": keepalive"));
        assert!(!sse_line_carries_a_token(b"data: [DONE]"));
        assert!(!sse_line_carries_a_token(
            br#"data: {"choices":[{"delta":{"role":"assistant","content":""}}]}"#
        ));
        assert!(!sse_line_carries_a_token(
            br#"data: {"choices":[{"delta":{"content":null}}]}"#
        ));
        assert!(sse_line_carries_a_token(
            br#"data: {"choices":[{"delta":{"content":"H"}}]}"#
        ));
        assert!(sse_line_carries_a_token(
            br#"data: {"choices":[{"delta":{"reasoning":"thinking"}}]}"#
        ));
        assert!(sse_line_carries_a_token(
            br#"data: {"choices":[{"delta":{"tool_calls":[{"index":0}]}}]}"#
        ));
        assert!(sse_line_carries_a_token(
            br#"data: {"error":{"message":"nope"}}"#
        ));
    }

    #[test]
    fn the_wire_body_turns_streaming_on_without_touching_the_recorded_body() {
        let request_body = serde_json::json!({ "model": "m", "messages": [], "stream": false });
        let wire: Value = serde_json::from_str(&with_stream_fields(&request_body)).unwrap();
        assert_eq!(wire["stream"], true);
        assert_eq!(wire["stream_options"]["include_usage"], true);
        assert_eq!(request_body["stream"], false);
    }

    #[tokio::test]
    async fn a_streaming_provider_reply_is_parsed_and_its_first_token_recorded() {
        let (url, server) = spawn_sse_mock_server(vec![
            r#"data: {"id":"r1","choices":[{"index":0,"delta":{"role":"assistant"}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{"content":"streamed"}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            r#"data: {"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
            "data: [DONE]",
        ]);
        let records: Arc<std::sync::Mutex<Vec<ModelCallRecord>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = records.clone();
        let mut deps = test_deps(url);
        deps.on_usage = Arc::new(move |_, record| sink.lock().unwrap().push(record));
        let caller = create_model_caller(deps);
        let response = caller
            .call_model(vec![user_message("hello")], None)
            .await
            .unwrap();
        assert_eq!(
            response.choices.unwrap()[0]
                .message
                .as_ref()
                .unwrap()
                .content,
            Some(serde_json::json!("streamed"))
        );
        assert_eq!(response.usage.unwrap().total_tokens, Some(2));
        let request: Value = serde_json::from_slice(&server.join().unwrap()).unwrap();
        assert_eq!(
            request["stream"], true,
            "the wire request should ask for a stream: {request}"
        );
        assert_eq!(request["stream_options"]["include_usage"], true);
        let records = records.lock().unwrap();
        assert_eq!(records.len(), 1);
        assert!(
            records[0].first_token_ms.is_some(),
            "the first-token time should be recorded: {:?}",
            records[0]
        );
    }

    fn test_deps(url: String) -> ModelCallerDeps {
        ModelCallerDeps {
            cwd: None,
            codex_executable: None,
            default_transport_tools: Vec::new(),
            emit: Arc::new(|_| {}),
            fallback_route: None,
            get_iteration: Arc::new(|| 0),
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            model: "test-model".to_string(),
            on_retry_wait: Arc::new(|_| {}),
            on_usage: Arc::new(|_, _| {}),
            provider: None,
            refresh_headers: None,
            prompt_cache_key: None,
            reasoning_effort: None,
            reasoning_effort_defaulted: false,
            request_timeout_ms: None,
            hedge_floor_ms: None,
            latency_store: None,
            signal: None,
            sleep_impl: None,
            tool_route: None,
            url,
            http_client: None,
        }
    }

    /// A mock that answers each connection on its own thread after the
    /// matching delay, so a slow first request does not block the second.
    fn spawn_delayed_mock_server(
        delays_ms: Vec<u64>,
        response_body: &'static str,
    ) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let served = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let served_in_thread = served.clone();
        std::thread::spawn(move || {
            for delay_ms in delays_ms {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let served = served_in_thread.clone();
                std::thread::spawn(move || {
                    let mut data: Vec<u8> = Vec::new();
                    let mut chunk = [0u8; 4096];
                    loop {
                        let read = stream.read(&mut chunk).unwrap_or(0);
                        if read == 0 {
                            return;
                        }
                        data.extend_from_slice(&chunk[..read]);
                        if let Some(pos) = data.windows(4).position(|window| window == b"\r\n\r\n")
                        {
                            let head = String::from_utf8_lossy(&data[..pos]).to_ascii_lowercase();
                            let content_length = head
                                .lines()
                                .find_map(|line| {
                                    line.strip_prefix("content-length:")
                                        .and_then(|value| value.trim().parse::<usize>().ok())
                                })
                                .unwrap_or(0);
                            if data.len() >= pos + 4 + content_length {
                                break;
                            }
                        }
                    }
                    std::thread::sleep(Duration::from_millis(delay_ms));
                    let response = format!(
                        "HTTP/1.1 200 Test\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        response_body.len(),
                        response_body
                    );
                    if stream.write_all(response.as_bytes()).is_ok() {
                        served.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                });
            }
        });
        (
            format!("http://127.0.0.1:{port}/v1/chat/completions"),
            served,
        )
    }

    #[tokio::test]
    async fn a_slow_first_attempt_is_hedged_and_the_faster_answer_wins() {
        let (url, _served) = spawn_delayed_mock_server(
            vec![3_000, 0],
            r#"{"choices":[{"message":{"content":"hedged"}}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
        );
        let events: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = events.clone();
        let mut deps = test_deps(url);
        deps.hedge_floor_ms = Some(200);
        deps.emit = Arc::new(move |event| sink.lock().unwrap().push(event.detail));
        let caller = create_model_caller(deps);
        for _ in 0..STALL_TIMEOUT_MIN_SAMPLES {
            caller.record_latency("test-model", 20);
        }
        let started = Instant::now();
        let response = caller
            .call_model(vec![user_message("hello")], None)
            .await
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_millis(2_500),
            "the hedge should answer long before the 3s primary: {:?}",
            started.elapsed()
        );
        assert_eq!(
            response.choices.unwrap()[0]
                .message
                .as_ref()
                .unwrap()
                .content,
            Some(serde_json::json!("hedged"))
        );
        let events = events.lock().unwrap();
        assert!(
            events.iter().any(|detail| detail
                .starts_with("hedged model request: test-model has sent no first token after 0.")),
            "{events:?}"
        );
        let resolved = events
            .iter()
            .find(|detail| detail.starts_with("hedge resolved: "))
            .expect("a hedge resolution event should be emitted");
        assert!(
            resolved.starts_with("hedge resolved: the second request completed after "),
            "{resolved}"
        );
        let elapsed_ms: u64 = resolved
            .rsplit_once("after ")
            .unwrap()
            .1
            .trim_end_matches("ms")
            .trim()
            .parse()
            .unwrap();
        assert!(
            (200..2_500).contains(&elapsed_ms),
            "elapsed should cover the hedge delay but not the 3s primary: {resolved}"
        );
    }

    #[tokio::test]
    async fn hedging_waits_for_history_and_a_prompt_answer_never_hedges() {
        let (url, served) = spawn_delayed_mock_server(
            vec![0, 0],
            r#"{"choices":[{"message":{"content":"fast"}}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
        );
        let events: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = events.clone();
        let mut deps = test_deps(url);
        deps.hedge_floor_ms = Some(200);
        deps.emit = Arc::new(move |event| sink.lock().unwrap().push(event.detail));
        let caller = create_model_caller(deps);
        assert_eq!(
            caller.hedge_delay_for("test-model", 1, 240_000),
            None,
            "no history, no hedge"
        );
        for _ in 0..STALL_TIMEOUT_MIN_SAMPLES {
            caller.record_latency("test-model", 20);
        }
        assert_eq!(caller.hedge_delay_for("test-model", 1, 240_000), Some(200));
        assert_eq!(
            caller.hedge_delay_for("test-model", 2, 240_000),
            None,
            "retries are never hedged"
        );
        caller
            .call_model(vec![user_message("hello")], None)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(
            served.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a prompt answer sends one request"
        );
        assert!(
            events.lock().unwrap().is_empty(),
            "{:?}",
            events.lock().unwrap()
        );
    }

    #[test]
    fn hedge_delay_clamps_and_respects_the_off_switch() {
        assert_eq!(
            hedge_delay_ms(&[2_000, 2_000], 8_000, 240_000),
            None,
            "too few samples"
        );
        assert_eq!(
            hedge_delay_ms(&[2_000, 2_000, 2_000], 8_000, 240_000),
            Some(8_000),
            "floor"
        );
        assert_eq!(
            hedge_delay_ms(&[5_000, 4_000, 6_000], 8_000, 240_000),
            Some(10_000),
            "2× median"
        );
        assert_eq!(
            hedge_delay_ms(&[50_000, 50_000, 50_000], 8_000, 100_000),
            Some(50_000),
            "half the timeout"
        );
        assert_eq!(
            hedge_delay_ms(&[2_000, 2_000, 2_000], 0, 240_000),
            None,
            "floor 0 disables"
        );
    }

    #[test]
    fn latency_store_round_trips_and_caps_samples() {
        let dir = std::env::temp_dir().join(format!("drip-latency-{}", std::process::id()));
        let path = dir.join("nested").join(LATENCY_STORE_FILE);
        assert!(
            load_latency_store(&path).is_empty(),
            "missing file reads as empty"
        );
        let mut samples = LatencySamples::new();
        samples.insert(
            "m".to_string(),
            (1..=(STALL_LATENCY_SAMPLES as u64 + 3)).collect(),
        );
        save_latency_store(&path, &samples);
        let loaded = load_latency_store(&path);
        let recent: Vec<u64> = loaded["m"].iter().copied().collect();
        assert_eq!(
            recent.len(),
            STALL_LATENCY_SAMPLES,
            "capped to the recent window"
        );
        assert_eq!(
            recent.last().copied(),
            Some(STALL_LATENCY_SAMPLES as u64 + 3),
            "keeps the newest samples"
        );
        std::fs::write(&path, "not json").unwrap();
        assert!(
            load_latency_store(&path).is_empty(),
            "corrupt file reads as empty"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn seeded_latency_store_hedges_from_the_first_call() {
        let dir = std::env::temp_dir().join(format!("drip-latency-seed-{}", std::process::id()));
        let path = dir.join(LATENCY_STORE_FILE);
        let mut samples = LatencySamples::new();
        samples.insert("m".to_string(), [2_000, 2_000, 2_000].into_iter().collect());
        save_latency_store(&path, &samples);
        let mut deps = test_deps("http://127.0.0.1:9".to_string());
        deps.latency_store = Some(path.clone());
        let caller = create_model_caller(deps);
        assert_eq!(
            caller.hedge_delay_for("m", 1, 240_000),
            Some(8_000),
            "hedges before any call in this run"
        );
        assert_eq!(
            caller.attempt_timeout_ms("m", 1),
            STALL_TIMEOUT_FLOOR_MS,
            "stall bound from the seeded samples"
        );
        caller.record_latency("m", 3_000);
        let persisted: Vec<u64> = load_latency_store(&path)["m"].iter().copied().collect();
        assert_eq!(
            persisted,
            vec![2_000, 2_000, 2_000, 3_000],
            "new samples are written back"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn user_message(text: &str) -> TransportRequestMessage {
        TransportRequestMessage {
            content: Some(crate::harness::transport::TransportContent::Text(
                text.to_string(),
            )),
            name: None,
            role: crate::harness::chat_types::ChatRoleTag::User,
            tool_call_id: None,
            tool_calls: None,
            anthropic_content: None,
        }
    }

    #[tokio::test]
    async fn call_model_posts_the_openai_compatible_payload_and_returns_the_response() {
        let (url, server) = spawn_mock_server(
            1,
            200,
            r#"{"choices":[{"message":{"content":"hi there"}}],"usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}}"#,
        );
        let caller = create_model_caller(test_deps(url));
        let response = caller
            .call_model(vec![user_message("hello")], None)
            .await
            .unwrap();

        assert_eq!(
            response.choices.as_ref().unwrap()[0]
                .message
                .as_ref()
                .unwrap()
                .content
                .as_ref()
                .unwrap(),
            &serde_json::json!("hi there")
        );

        let requests = server.join().unwrap();

        assert_eq!(requests.len(), 1);

        let (request_line, body) = requests.into_iter().next().unwrap();

        assert_eq!(request_line, "POST /v1/chat/completions HTTP/1.1");

        let body: Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(body["model"], "test-model");
        assert_eq!(
            body["stream"], true,
            "OpenAI-compatible requests ask for a stream"
        );
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "hello");
        assert!(body.get("prompt_cache_key").is_none());
    }

    #[tokio::test]
    async fn a_codex_role_route_whose_executable_is_missing_falls_back_to_the_base_model() {
        let (url, server) = spawn_mock_server(
            1,
            200,
            r#"{"choices":[{"message":{"content":"base hi"}}],"usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}}"#,
        );
        let events = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let mut deps = test_deps(url);
        let sink = events.clone();
        deps.emit = Arc::new(move |event| sink.lock().unwrap().push(event.detail));
        deps.codex_executable = Some("drip-missing-codex-binary-for-tests".to_string());
        let caller = create_model_caller(deps);
        let route = ModelRoute {
            fallback_route: None,
            headers: None,
            model: "gpt-6-astra".to_string(),
            provider: Some("codex".to_string()),
            reasoning_effort: None,
            refresh_headers: None,
            url: String::new(),
        };
        let response = caller
            .call_model(
                vec![user_message("plan")],
                Some(ModelCallOptions {
                    route: Some(route),
                    ..Default::default()
                }),
            )
            .await
            .unwrap();
        assert_eq!(
            response.choices.as_ref().unwrap()[0]
                .message
                .as_ref()
                .unwrap()
                .content
                .as_ref()
                .unwrap(),
            &serde_json::json!("base hi")
        );
        let _ = server.join();
        let events = events.lock().unwrap();
        assert!(
            events
                .iter()
                .any(|detail| detail.contains("falling back to the run's base model test-model")),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn claude_routes_go_to_the_native_messages_endpoint_with_the_translated_headers() {
        let (url, server) = spawn_mock_server(
            1,
            200,
            r#"{"content":[{"type":"text","text":"native hi"}],"usage":{"input_tokens":3,"output_tokens":2},"stop_reason":"end_turn"}"#,
        );
        let mut deps = test_deps(url);

        deps.provider = Some("claude".to_string());
        deps.headers = vec![
            ("authorization".to_string(), "Bearer sk-test".to_string()),
            ("content-type".to_string(), "application/json".to_string()),
        ];

        let caller = create_model_caller(deps);
        let response = caller
            .call_model(vec![user_message("hello")], None)
            .await
            .unwrap();

        assert_eq!(
            response.choices.as_ref().unwrap()[0]
                .message
                .as_ref()
                .unwrap()
                .content
                .as_ref()
                .unwrap(),
            &serde_json::json!("native hi")
        );

        let requests = server.join().unwrap();

        assert_eq!(requests.len(), 1);

        let (request_line, body) = requests.into_iter().next().unwrap();

        assert_eq!(request_line, "POST /v1/messages HTTP/1.1");

        let body: Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(body["model"], "test-model");
        assert!(body["max_tokens"].is_u64());
        assert_eq!(body["messages"][0]["role"], "user");
    }

    #[tokio::test]
    async fn a_500_response_retries_then_fails_with_the_ladder_exhausted_message() {
        let (url, server) = spawn_mock_server(
            RATE_LIMIT_MAX_ATTEMPTS as usize,
            500,
            r#"{"error":{"message":"upstream exploded"}}"#,
        );
        let waits = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut deps = test_deps(url);
        let waits_for_deps = std::sync::Arc::clone(&waits);

        deps.on_retry_wait = Arc::new(move |seconds| {
            waits_for_deps.lock().unwrap().push(seconds);
        });
        // A zero-latency sleep keeps the test instant while still exercising the ladder.
        deps.sleep_impl = Some(Arc::new(|_, _| Box::pin(async {})));

        let caller = create_model_caller(deps);
        let error = caller
            .call_model(vec![user_message("hello")], None)
            .await
            .unwrap_err();

        assert_eq!(
            error.message(),
            "upstream exploded",
            "the endpoint's own error message wins once the ladder is exhausted"
        );
        assert_eq!(
            waits.lock().unwrap().as_slice(),
            &[1.0, 1.0, 2.0, 3.0, 5.0, 8.0, 13.0, 21.0, 34.0],
            "one wait per retried attempt, from RATE_LIMIT_BACKOFF_SECONDS"
        );

        // The accept-loop mock must have served every attempt on the ladder.
        assert_eq!(
            server.join().unwrap().len(),
            RATE_LIMIT_MAX_ATTEMPTS as usize,
            "every attempt on the ladder must have reached the endpoint"
        );
    }
}
