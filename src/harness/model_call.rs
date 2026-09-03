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

/// The request outgrew the model's context window — recoverable by folding.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct ContextOverflowError(pub String);

/// The call's failure surface. `ContextOverflow` mirrors the TS
/// ContextOverflowError class (the caller checks the type to decide whether to
/// fold the transcript); `Message` is a plain Error with a message.
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
    #[serde(default, rename = "cache_creation_input_tokens", skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<i64>,
    /// Anthropic native: tokens served from the prompt cache (billed at ~10% of the input rate).
    #[serde(default, rename = "cache_read_input_tokens", skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<i64>,
    #[serde(default, rename = "completion_tokens", skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<i64>,
    #[serde(default, rename = "prompt_tokens", skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<i64>,
    /// OpenAI-compatible providers with automatic caching (OpenAI, Cerebras, xAI, Gemini) report cache hits here.
    #[serde(default, rename = "prompt_tokens_details", skip_serializing_if = "Option::is_none")]
    pub prompt_tokens_details: Option<OpenAICompatibleResponsePromptTokensDetails>,
    #[serde(default, rename = "total_tokens", skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OpenAICompatibleResponsePromptTokensDetails {
    #[serde(default, rename = "cached_tokens", skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OpenAICompatibleResponseMessage {
    /// Native Anthropic responses only: raw content blocks for verbatim replay (thinking blocks must survive tool round-trips).
    #[serde(default, rename = "anthropicContent", skip_serializing_if = "Option::is_none")]
    pub anthropic_content: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Value>,
    #[serde(default, rename = "tool_calls", skip_serializing_if = "Option::is_none")]
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
                        message: choice.message.map(|message| OpenAICompatibleResponseMessage {
                            anthropic_content: message.anthropic_content,
                            content: message.content,
                            tool_calls: message.tool_calls,
                        }),
                    })
                    .collect()
            }),
            error: translated
                .error
                .map(|error| OpenAICompatibleResponseError {
                    code: error.code,
                    message: error.message,
                    error_type: error.error_type,
                }),
            usage: translated.usage.map(|usage| OpenAICompatibleResponseUsage {
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
    pub refresh_headers: Option<Arc<dyn Fn() -> Result<Vec<(String, String)>, String> + Send + Sync>>,
    pub url: String,
}

impl std::fmt::Debug for ModelRoute {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ModelRoute")
            .field("fallback_route", &self.fallback_route.as_ref().map(|route| route.model.clone()))
            .field("headers", &self.headers.as_ref().map(|headers| headers.len()))
            .field("model", &self.model)
            .field("provider", &self.provider)
            .field("reasoning_effort", &self.reasoning_effort)
            .field("refresh_headers", &self.refresh_headers.as_ref().map(|_| "<fn>"))
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
        regex::Regex::new(r"(?i)failed to parse url|invalid url|invalid header|unsupported protocol").unwrap()
    })
}

fn quota_message_regex() -> &'static regex::Regex {
    static REGEX: OnceLock<regex::Regex> = OnceLock::new();
    REGEX.get_or_init(|| regex::Regex::new(r"(?i)insufficient[_ ]quota|credit balance|billing").unwrap())
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

fn context_overflow_regex() -> &'static regex::Regex {
    static REGEX: OnceLock<regex::Regex> = OnceLock::new();
    REGEX.get_or_init(|| regex::Regex::new(r"(?i)context|token|length|too long|too large").unwrap())
}

/// String-level port of the TS classification (name + message + code + cause
/// against the network-error regex). reqwest errors go through
/// `is_network_transport_error`, which first maps the config-shaped
/// (builder) failures to "never heals".
pub fn is_network_fetch_error(error: &(dyn std::error::Error + 'static)) -> bool {
    let message = error.to_string();

    if config_error_regex().is_match(&message) {
        // A bad URL or header never heals with retries — fail fast with the cause.
        return false;
    }

    // Node/undici says "fetch failed" + ECONN codes; Bun says "Unable to
    // connect. Is the computer able to access the url?" with code
    // ConnectionRefused/ConnectionClosed. reqwest's Display carries the cause
    // chain (hyper/tcp error text), so walk it the way TS spreads
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
    // the Rust shape of the TS "fetch failed" TypeError.
    if error.is_connect() || error.is_timeout() || error.is_request() {
        return true;
    }

    is_network_fetch_error(error)
}

/// The run's cancellation flag: the TS code threads an AbortSignal through
/// fetch and every backoff sleep; here it is a shared AtomicBool the loop
/// sets on --stop. `sleep_unless_aborted` and the in-flight request both
/// observe it.
#[derive(Clone, Default)]
pub struct AbortSignal(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl AbortSignal {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn abort(&self) {
        self.0
            .store(true, std::sync::atomic::Ordering::SeqCst);
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
    // Poll in slices so an abort cuts the wait short, the way the TS
    // timer + abort listener resolves early.
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
/// `(ms, signal) => Promise<void>` — the TS `sleepImpl` injection point.
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
}

pub struct ModelCallerDeps {
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
    pub refresh_headers: Option<Arc<dyn Fn() -> Result<Vec<(String, String)>, String> + Send + Sync>>,
    /// Stable key sent to providers that support prompt-cache routing hints (OpenAI: prompt_cache_key body field; xAI: x-grok-conv-id header).
    pub prompt_cache_key: Option<String>,
    pub reasoning_effort: Option<String>,
    /// Per-attempt wall-clock cap on one HTTP request (default DEFAULT_REQUEST_TIMEOUT_MS); a timed-out attempt retries on the network ladder.
    pub request_timeout_ms: Option<u64>,
    pub signal: Option<AbortSignal>,
    pub sleep_impl: Option<SleepFn>,
    pub tool_route: Option<ModelRoute>,
    pub url: String,
    /// Test seam: a pre-built reqwest client (the TS `fetchImpl` injection
    /// point; the URL/server is the other half of that seam).
    pub http_client: Option<reqwest::Client>,
}

/// The TS `ModelCaller = (messages, callOptions) => Promise<...>` becomes a
/// struct with an async `call_model` method so the resolved dependencies
/// (client, timeout, sleep impl) live on it.
pub struct ModelCaller {
    deps: ModelCallerDeps,
    http_client: reqwest::Client,
    request_timeout_ms: u64,
    sleep: SleepFn,
}

pub fn create_model_caller(deps: ModelCallerDeps) -> ModelCaller {
    let request_timeout_ms = match deps.request_timeout_ms {
        Some(timeout_ms) if timeout_ms > 0 => timeout_ms,
        _ => DEFAULT_REQUEST_TIMEOUT_MS,
    };
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
        sleep,
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
            call_options.route.clone().or_else(|| self.deps.tool_route.clone())
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
        // A route with refreshHeaders re-mints its auth headers on every call so a
        // "cmd:" token that expires mid-run is transparently renewed; the command
        // result is TTL-cached, so calling this per request stays cheap.
        let route_headers: Option<Vec<(String, String)>> = match route {
            Some(route) => {
                let mut headers = vec![("content-type".to_string(), "application/json".to_string())];
                let route_headers = match &route.refresh_headers {
                    Some(refresh_headers) => refresh_headers().map_err(ModelCallError::Message)?,
                    None => route.headers.clone().unwrap_or_default(),
                };

                headers.extend(route_headers);
                Some(headers)
            }
            None => None,
        };
        // Text-only calls carry no route, so they resolve to the run's base
        // provider; a routed call uses that route's own provider.
        let provider = match route {
            Some(route) => route.provider.clone(),
            None => self.deps.provider.clone(),
        };
        let anthropic_native = is_anthropic_native_provider(provider.as_deref());
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
        let reasoning_effort = match route {
            Some(route) => route.reasoning_effort.clone(),
            None => self.deps.reasoning_effort.clone(),
        };
        let request_body = if anthropic_native {
            serde_json::to_value(build_anthropic_request_payload(BuildAnthropicRequestPayloadArgs {
                // One-shot calls (run summaries) never re-read their prefix, so a
                // cache write would be a pure premium.
                cache: Some(include_tools),
                max_tokens: None,
                messages: messages.to_vec(),
                model: model.clone(),
                tools: if include_tools {
                    Some(request_tools.to_vec())
                } else {
                    None
                },
            }))
            .map_err(|error| {
                ModelCallError::Message(format!("failed to serialize the Anthropic request payload: {error}"))
            })?
        } else {
            serde_json::to_value(build_transport_request_payload(BuildTransportRequestPayloadArgs {
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
            }))
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
        ) -> Result<(), ModelCallError> {
            if attempt >= max_attempts {
                return Err(ModelCallError::Message(format!(
                    "The inference endpoint could not be reached after {max_attempts} attempts: {message}"
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
            let request_fut = {
                let client = self.http_client.clone();
                let url = url.clone();
                let header_map = header_map.clone();
                let body = body.clone();

                async move {
                    let response = client.post(&url).headers(header_map).body(body).send().await?;
                    let status = response.status().as_u16();
                    let headers = response.headers().clone();
                    let body = match response.bytes().await {
                        Ok(bytes) => Ok(bytes.to_vec()),
                        Err(error) => Err(error),
                    };

                    Ok((status, headers, body))
                }
            };

            match run_bounded_request(request_fut, self.request_timeout_ms, self.deps.signal.as_ref()).await {
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

                    wait_out_unreachable(self, attempt, max_attempts, &message).await?;
                    attempt += 1;
                    continue;
                }
                RequestOutcome::TimedOut => {
                    // The headers arrived but the body never finished inside the bound.
                    wait_out_unreachable(
                        self,
                        attempt,
                        max_attempts,
                        &request_timeout_message(self.request_timeout_ms),
                    )
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
                        (self.sleep)((wait_seconds * 1000.0) as u64, self.deps.signal.clone()).await;

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
                        (self.sleep)((wait_seconds * 1000.0) as u64, self.deps.signal.clone()).await;

                        if self
                            .deps
                            .signal
                            .as_ref()
                            .is_some_and(|signal| signal.is_aborted())
                        {
                            return Err(ModelCallError::Message(
                                "The run was stopped while waiting out an endpoint outage.".to_string(),
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
                        if (status == 400 || status == 413) && context_overflow_regex().is_match(&message) {
                            return Err(ContextOverflowError(message).into());
                        }

                        return Err(ModelCallError::Message(message));
                    }

                    if let Some(message) = data.error.as_ref().and_then(|error| error.message.clone()) {
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
                        (self.sleep)((wait_seconds * 1000.0) as u64, self.deps.signal.clone()).await;

                        if self
                            .deps
                            .signal
                            .as_ref()
                            .is_some_and(|signal| signal.is_aborted())
                        {
                            return Err(ModelCallError::Message(
                                "The run was stopped while waiting out an endpoint outage.".to_string(),
                            ));
                        }

                        attempt += 1;
                        continue;
                    }

                    (self.deps.on_usage)(
                        &data,
                        ModelCallRecord {
                            latency_ms: call_started_at.elapsed().as_millis() as i64,
                            model,
                            provider,
                            task_id: call_options.usage_task_id.clone(),
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
        assert!(is_network_fetch_error(&MessageError("Connection refused (os error 61)".to_string())));
        assert!(is_network_fetch_error(&MessageError("fetch failed: connection reset".to_string())));
        assert!(!is_network_fetch_error(&MessageError(
            "failed to parse url: relative URL without a base".to_string()
        )));
        assert!(!is_network_fetch_error(&MessageError("invalid header value".to_string())));
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
        assert_eq!(request_timeout_message(1500), "request timed out after 2s with no response");
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
    /// handle whose join() yields the (request line, body) pairs it received
    /// (the Rust stand-in for the TS tests' Bun.serve + fetchImpl stubs).
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

        (format!("http://127.0.0.1:{port}/v1/chat/completions"), handle)
    }

    fn test_deps(url: String) -> ModelCallerDeps {
        ModelCallerDeps {
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
            request_timeout_ms: None,
            signal: None,
            sleep_impl: None,
            tool_route: None,
            url,
            http_client: None,
        }
    }

    fn user_message(text: &str) -> TransportRequestMessage {
        TransportRequestMessage {
            content: Some(crate::harness::transport::TransportContent::Text(text.to_string())),
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
        assert_eq!(body["stream"], false);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "hello");
        assert!(body.get("prompt_cache_key").is_none());
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
        let response = caller.call_model(vec![user_message("hello")], None).await.unwrap();

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
        let error = caller.call_model(vec![user_message("hello")], None).await.unwrap_err();

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
