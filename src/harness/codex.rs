// Codex provider bridge: drives the installed `codex app-server` (JSONL
// JSON-RPC 2.0 over stdio) instead of an HTTP chat/completions endpoint, so
// model traffic rides the ChatGPT login `codex login` already established —
// drip never reads auth.json and never proxies subscription credentials to
// an HTTP provider. drip stays the harness: its tool schemas are exposed to
// Codex as dynamicTools, every execution request comes back here as an
// OpenAI-compatible tool call for drip's executor, and tool results are
// returned to the still-open app-server request on the next model call.
//
// Codex's own execution surface is disabled as far as the installed CLI
// allows: shell_tool/unified_exec/web_search/apps/plugins/multi_agent
// features off, `environments: []` (no native execution environments for
// the turn), read-only sandbox, approvalPolicy "never", and every native
// approval or user-input request this bridge still receives is denied
// outright. apply_patch_freeform=false may
// not remove the function variant; any built-in that still tries to execute
// must be reported, never assumed away.
//
// Continuity: a Codex thread is reused only when the incoming message list
// strictly extends what that thread already saw and model / tool schema /
// system prompt are unchanged; otherwise the thread is discarded and the
// conversation is replayed into a fresh thread. A pending tool request is
// never answered with results from an unrelated call, and include_tools
// summary calls always run on an independent ephemeral thread.
//
// Rust notes: the subprocess is tokio::process (requires the "process" and
// "io-util" tokio features); a reader task funnels stdout frames through
// one mpsc queue so turn waits, aborts and timeouts all select() off a
// single channel, mirroring the AbortSignal/RequestOutcome conventions in
// model_call.rs. Kill-on-drop plus poison-on-error guarantee process
// cleanup even when a turn dies mid-flight.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{mpsc, oneshot, Mutex as AsyncMutex};

use crate::harness::chat_types::ChatRoleTag;
use crate::harness::model_call::{
    AbortSignal, ModelCallError, OpenAICompatibleResponse, OpenAICompatibleResponseChoice,
    OpenAICompatibleResponseMessage, OpenAICompatibleResponseUsage, DEFAULT_REQUEST_TIMEOUT_MS,
};
use crate::harness::transport::{
    OpenAICompatibleRequestTool, OpenAICompatibleToolCall, TransportContent, TransportContentPart,
    TransportRequestMessage,
};

// ---------------------------------------------------------------------------
// JSON-RPC wire types (subset of the app-server protocol; params are built
// with json!() and read defensively so schema drift degrades into explicit
// errors instead of compile-time coupling).
// ---------------------------------------------------------------------------

/// JSON-RPC request ids are int64 or string in the app-server protocol.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcId {
    Int(i64),
    Str(String),
}

/// The `error` object of a JSON-RPC error response (protocol shape).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcErrorError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl std::fmt::Display for JsonRpcErrorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(data) = &self.data {
            write!(
                f,
                "codex app-server error {}: {} ({})",
                self.code, self.message, data
            )
        } else {
            write!(f, "codex app-server error {}: {}", self.code, self.message)
        }
    }
}

/// One event pumped from the app-server's stdout into the bridge.
#[derive(Debug)]
pub(crate) enum ServerEvent {
    /// A reply to one of OUR client->server requests.
    Response {
        id: JsonRpcId,
        result: Option<Value>,
        error: Option<JsonRpcErrorError>,
    },
    /// A server->client REQUEST we must answer (item/tool/call, approvals...).
    ServerRequest {
        id: JsonRpcId,
        method: String,
        params: Value,
    },
    /// A server->client notification (turn/completed, token usage, noise).
    Notification { method: String, params: Value },
    /// A line that was not valid JSON or not a JSON-RPC frame.
    Malformed { reason: String, line: String },
}

/// A server->client item/tool/call request captured while its turn is open.
/// The JSON-RPC request stays unanswered until drip hands the tool result
/// back on the next model call.
#[derive(Debug, Clone)]
pub(crate) struct PendingToolCall {
    pub request_id: JsonRpcId,
    pub call_id: String,
    pub tool: String,
}

/// Token usage as reported by the app-server's tokenUsage breakdown,
/// read defensively (fields may be missing depending on CLI version).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CodexTokenUsage {
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
}

impl CodexTokenUsage {
    fn from_value(v: &Value) -> Option<CodexTokenUsage> {
        let obj = v.as_object()?;
        let num = |k: &str| obj.get(k).and_then(Value::as_i64).unwrap_or(0);
        let input = num("inputTokens");
        let total = obj
            .get("totalTokens")
            .and_then(Value::as_i64)
            .unwrap_or_else(|| input + num("outputTokens"));
        Some(CodexTokenUsage {
            input_tokens: input,
            cached_input_tokens: num("cachedInputTokens"),
            output_tokens: num("outputTokens"),
            total_tokens: total,
        })
    }
}

// ---------------------------------------------------------------------------
// Bridge session
// ---------------------------------------------------------------------------

/// Everything that defines one Codex conversation identity: a thread may only
/// be continued when all three are unchanged and the transcript strictly
/// extends what the thread already saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ThreadIdentity {
    pub model: Option<String>,
    pub cwd: Option<PathBuf>,
    /// Stable serialization of the dynamicTools drip exposed this turn.
    pub tools_digest: String,
    /// Stable serialization of the system prompt the thread was seeded with.
    pub system_digest: String,
}

#[derive(Debug, Default)]
struct ThreadState {
    thread_id: Option<String>,
    identity: Option<ThreadIdentity>,
    /// Number of user-visible drip messages replayed into the thread so far.
    messages_sent: usize,
    sent_messages: Vec<TransportRequestMessage>,
    /// tool calls drip has already answered (callId -> tool name), so a
    /// stale/mismatched result can never satisfy a live request.
    answered_calls: HashMap<String, String>,
}

pub struct CodexBridge {
    pub(crate) config: BridgeConfig,
    child: Mutex<Option<Child>>,
    stdin: AsyncMutex<Option<ChildStdin>>,
    /// Server stdout frames, demultiplexed for whichever call is waiting.
    events: mpsc::Receiver<ServerEvent>,
    next_id: i64,
    state: Mutex<ThreadState>,
    pending_handshake: Arc<Mutex<Option<oneshot::Sender<Result<Value, ModelCallError>>>>>,
    /// Bounded stderr tail for subprocess diagnostics.
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
    /// Set when the subprocess died or the channel closed; every later call
    /// fails fast with the captured reason instead of hanging.
    poisoned: Mutex<Option<String>>,
    /// Server->client item/tool/call that drip returned to its executor and
    /// has not answered on the wire yet (answered on the NEXT model call).
    pending_tool: Option<PendingToolCall>,
    /// turn/start request id of the turn currently executing on the server.
    active_turn: Option<JsonRpcId>,
    /// turnId from the turn/start response, needed for turn/interrupt.
    active_turn_id: Option<String>,
    deferred_events: VecDeque<ServerEvent>,
    total_usage: CodexTokenUsage,
    reported_usage: CodexTokenUsage,
    allowed_tools: Vec<String>,
}

/// Static description of how to spawn `codex app-server`. Overridable so the
/// deterministic test suite can substitute a fake codex subprocess.
#[derive(Clone)]
pub struct BridgeConfig {
    pub executable: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub model: Option<String>,
    /// Reasoning effort forwarded as turn/start `effort` (e.g. "high" for the
    /// gpt-5.6-luna-high profile; thread/start has no reasoningEffort field).
    pub reasoning_effort: Option<String>,
    /// ChatGPT-subscription billing gate: when set, force modelProvider
    /// "openai" + forced_login_method "chatgpt" and verify via account/read
    /// that the account is not on API-key billing before any turn starts.
    pub force_chatgpt_auth: bool,
    pub request_timeout_ms: u64,
    /// JSON config overrides threaded into thread/start (feature flags that
    /// disable Codex built-in execution surfaces).
    pub config_overrides: Option<Value>,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        BridgeConfig {
            executable: "codex".to_string(),
            args: vec!["app-server".to_string()],
            cwd: None,
            model: None,
            reasoning_effort: None,
            force_chatgpt_auth: true,
            request_timeout_ms: DEFAULT_REQUEST_TIMEOUT_MS,
            // Native execution feature flags for codex-cli
            // 0.153.x: no native shell, no unified exec, no web search, no
            // connected-app/plugin surfaces, no multi-agent orchestration.
            // apply_patch_freeform is also sent, with the documented caveat
            // that it may not remove the function variant — the defensive
            // approval denial below covers whatever survives.
            config_overrides: Some(json!({
                "features": {
                    "shell_tool": false,
                    "unified_exec": false,
                    "web_search": false,
                    "apps": false,
                    "plugins": false,
                    "multi_agent": false,
                    "apply_patch_freeform": false
                }
            })),
        }
    }
}

impl CodexBridge {
    /// Spawn `codex app-server` and run the initialize handshake. The child
    /// is killed on drop; an early failure kills it before returning.
    pub async fn spawn(config: BridgeConfig) -> Result<CodexBridge, ModelCallError> {
        let mut command = tokio::process::Command::new(&config.executable);
        command
            .args(&config.args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // Dropping a tokio Child does NOT kill it unless this is set.
            .kill_on_drop(true);
        if let Some(cwd) = &config.cwd {
            command.current_dir(cwd);
        }
        // OPENAI_API_KEY must never leak into the ChatGPT-login session and
        // flip billing to API credits; a bad inherited key would also fail
        // every turn with diagnostics that look like outages.
        command.env_remove("OPENAI_API_KEY");
        command.env_remove("CODEX_API_KEY");

        let mut child = command.spawn().map_err(|error| {
            ModelCallError::Message(format!(
                "codex executable \"{}\" not found or failed to spawn: {} (install codex-cli or set the executable path)",
                config.executable, error
            ))
        })?;

        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        if stdin.is_none() || stdout.is_none() {
            let _ = child.kill().await;
            return Err(ModelCallError::Message(
                "codex app-server did not provide stdio pipes; unable to drive the JSONL protocol"
                    .to_string(),
            ));
        }

        let stderr_tail: Arc<Mutex<VecDeque<String>>> = Arc::default();
        let stderr_task_tail = Arc::clone(&stderr_tail);
        if let Some(stderr) = stderr {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if let Ok(mut tail) = stderr_task_tail.lock() {
                        if tail.len() >= STDERR_TAIL_LINES {
                            tail.pop_front();
                        }
                        tail.push_back(line);
                    }
                }
            });
        }

        let pending_handshake: Arc<Mutex<Option<oneshot::Sender<Result<Value, ModelCallError>>>>> =
            Arc::new(Mutex::new(None));
        let (event_tx, event_rx) = mpsc::channel::<ServerEvent>(64);
        tokio::spawn(read_server_events(
            stdout.unwrap(),
            event_tx,
            Arc::clone(&pending_handshake),
        ));

        let bridge = CodexBridge {
            config,
            child: Mutex::new(Some(child)),
            stdin: AsyncMutex::new(stdin),
            events: event_rx,
            next_id: 1,
            state: Mutex::new(ThreadState::default()),
            pending_handshake,
            stderr_tail,
            poisoned: Mutex::new(None),
            pending_tool: None,
            active_turn: None,
            active_turn_id: None,
            deferred_events: VecDeque::new(),
            total_usage: CodexTokenUsage::default(),
            reported_usage: CodexTokenUsage::default(),
            allowed_tools: Vec::new(),
        };
        tokio::time::timeout(
            Duration::from_millis(bridge.config.request_timeout_ms),
            bridge.handshake(),
        )
        .await
        .map_err(|_| ModelCallError::Message("codex initialize timed out".into()))??;
        bridge.send_notification("initialized", json!({})).await?;
        Ok(bridge)
    }

    fn next_request_id(&mut self) -> JsonRpcId {
        let id = JsonRpcId::Int(self.next_id);
        self.next_id += 1;
        id
    }

    async fn send_raw(&self, payload: Value) -> Result<(), ModelCallError> {
        let mut line = payload.to_string();
        line.push('\n');
        // tokio::sync::Mutex: its guard is Send and safe to hold across the
        // write/flush awaits below; a std::sync::MutexGuard must never cross
        // an await point (non-Send future, lock held while blocked).
        let mut stdin_slot = self.stdin.lock().await;
        match stdin_slot.as_mut() {
            Some(stdin) => {
                if let Err(error) = stdin.write_all(line.as_bytes()).await {
                    let reason = format!("codex app-server stdin closed: {}", error);
                    self.set_poison(reason.clone());
                    return Err(ModelCallError::Message(reason));
                }
                if let Err(error) = stdin.flush().await {
                    let reason = format!("codex app-server stdin closed: {}", error);
                    self.set_poison(reason.clone());
                    return Err(ModelCallError::Message(reason));
                }
                Ok(())
            }
            None => Err(ModelCallError::Message(self.poison_reason())),
        }
    }

    /// Send a client->server request and register its reply channel.
    async fn send_request(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<JsonRpcId, ModelCallError> {
        if let Some(reason) = self.poison_reason_opt() {
            return Err(ModelCallError::Message(reason));
        }
        let id = self.next_request_id();
        let payload = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.send_raw(payload).await?;
        Ok(id)
    }

    async fn send_notification(&self, method: &str, params: Value) -> Result<(), ModelCallError> {
        self.send_raw(json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await
    }

    /// Answer a server->client request by echoing its JSON-RPC id verbatim.
    async fn answer_server_request(
        &self,
        id: &JsonRpcId,
        result: Result<Value, JsonRpcErrorError>,
    ) -> Result<(), ModelCallError> {
        let payload = match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(error) => json!({"jsonrpc": "2.0", "id": id, "error": error}),
        };
        self.send_raw(payload).await
    }

    fn set_poison(&self, reason: String) {
        if let Ok(mut poison) = self.poisoned.lock() {
            if poison.is_none() {
                *poison = Some(reason);
            }
        }
    }

    fn poison_reason_opt(&self) -> Option<String> {
        self.poisoned.lock().ok().and_then(|poison| poison.clone())
    }

    fn poison_reason(&self) -> String {
        self.poison_reason_opt()
            .unwrap_or_else(|| "codex app-server subprocess is no longer available".to_string())
    }

    fn note_stderr(&self, context: &str) -> String {
        match self.stderr_tail.lock() {
            Ok(tail) if !tail.is_empty() => {
                format!(
                    "{}; codex stderr tail: {}",
                    context,
                    join_stderr_lines(tail.iter())
                )
            }
            _ => context.to_string(),
        }
    }

    /// ChatGPT-subscription billing gate: drip rides
    /// the `codex login` ChatGPT session and refuses to consume API-key
    /// credits. auth.json is never read and nothing is sent to
    /// chat/completions — this asks the running app-server how it is
    /// authenticated and fails closed on API-key accounts.
    async fn verify_chatgpt_billing(&mut self) -> Result<(), ModelCallError> {
        let request_id = self.send_request("account/read", json!({})).await?;
        let result = self
            .await_response(request_id, self.config.request_timeout_ms)
            .await?;
        match result.pointer("/account/type").and_then(Value::as_str) {
            Some("chatgpt") => Ok(()),
            Some(kind) => Err(ModelCallError::Message(format!(
                "codex account is on {} billing; run `codex login` (ChatGPT) — drip refuses to consume OPENAI_API_KEY credits",
                kind
            ))),
            None => Err(ModelCallError::Message(
                "could not determine codex account auth mode from account/read; run `codex login` (ChatGPT) — drip refuses to consume OPENAI_API_KEY credits".to_string(),
            )),
        }
    }

    async fn handshake(&self) -> Result<Value, ModelCallError> {
        // experimentalApi: true is REQUIRED for thread/start dynamicTools on
        // 0.153.x; without it thread/start fails with -32600.
        let params = json!({
            "clientInfo": {
                "name": "drip",
                "title": "drip solid-state harness",
                "version": env!("CARGO_PKG_VERSION")
            },
            "capabilities": { "experimentalApi": true }
        });
        let (tx, rx) = oneshot::channel();
        self.pending_handshake
            .lock()
            .map_err(|_| poisoned_lock_error("handshake"))?
            .replace(tx);
        self.send_raw(json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": params}))
            .await?;
        match rx.await {
            Ok(reply) => reply,
            Err(_) => Err(ModelCallError::Message(self.note_stderr(
                "codex app-server closed stdout before the initialize handshake completed",
            ))),
        }
    }
}

fn poisoned_lock_error(what: &str) -> ModelCallError {
    ModelCallError::Message(format!("codex bridge {} lock poisoned", what))
}

fn join_stderr_lines<'a, I: Iterator<Item = &'a String>>(lines: I) -> String {
    // `Vec<&String>` has no `Join` impl; map to `&str` slices first.
    lines
        .map(|line| line.as_str())
        .collect::<Vec<_>>()
        .join(" | ")
}

const STDERR_TAIL_LINES: usize = 8;

/// Demultiplex one JSONL line from the app-server into a ServerEvent.
/// Unknown notifications (status/changed, mcpServer/startupStatus/updated,
/// remoteControl) arrive unsolicited and are forwarded as noise; the call
/// sites decide what matters. The initialize reply (id 0) resolves the
/// handshake slot registered by CodexBridge::handshake.
fn classify_frame(line: &str) -> ServerEvent {
    let parsed: Value = match serde_json::from_str(line) {
        Ok(parsed) => parsed,
        Err(error) => {
            let reason = format!("malformed codex stdout JSON: {}", error);
            return ServerEvent::Malformed {
                reason,
                line: line.to_string(),
            };
        }
    };
    let id: Option<JsonRpcId> = parsed
        .get("id")
        .filter(|frame_id| !frame_id.is_null())
        .and_then(|frame_id| serde_json::from_value(frame_id.clone()).ok());
    let method = parsed
        .get("method")
        .and_then(Value::as_str)
        .map(|method| method.to_string());
    match (id, method) {
        (Some(id), Some(method)) => ServerEvent::ServerRequest {
            id,
            method,
            params: parsed.get("params").cloned().unwrap_or(Value::Null),
        },
        (Some(id), None) => ServerEvent::Response {
            result: parsed
                .get("result")
                .cloned()
                .filter(|value| !value.is_null()),
            error: parsed
                .get("error")
                .cloned()
                .and_then(|error| serde_json::from_value(error).ok()),
            id,
        },
        (None, Some(method)) => ServerEvent::Notification {
            method,
            params: parsed.get("params").cloned().unwrap_or(Value::Null),
        },
        (None, None) => ServerEvent::Malformed {
            reason: "JSON-RPC frame carried neither id nor method".to_string(),
            line: line.to_string(),
        },
    }
}

/// The initialize handshake always uses this client request id.
pub(crate) const INITIALIZE_REQUEST_ID: i64 = 0;

/// Reader task: parse stdout lines into ServerEvents and pump them into the
/// bridge's queue. Exits when stdout closes (child death), sending a final
/// Malformed marker so waiters fail fast instead of hanging forever.
async fn read_server_events(
    stdout: tokio::process::ChildStdout,
    event_tx: mpsc::Sender<ServerEvent>,
    handshake_slot: Arc<Mutex<Option<oneshot::Sender<Result<Value, ModelCallError>>>>>,
) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let event = classify_frame(&line);
        if let ServerEvent::Malformed { ref reason, .. } = event {
            if let Ok(mut slot) = handshake_slot.lock() {
                if let Some(sender) = slot.take() {
                    let _ = sender.send(Err(ModelCallError::Message(reason.clone())));
                }
            }
        }
        // Resolve the initialize handshake directly so CodexBridge::spawn
        // can await it before the demux loop is being consumed.
        if let ServerEvent::Response {
            ref id,
            ref result,
            ref error,
        } = event
        {
            if *id == JsonRpcId::Int(INITIALIZE_REQUEST_ID) {
                if let Ok(mut slot) = handshake_slot.lock() {
                    if let Some(sender) = slot.take() {
                        let outcome = match error {
                            Some(error) => Err(ModelCallError::Message(error.to_string())),
                            None => Ok(result.clone().unwrap_or(Value::Null)),
                        };
                        let _ = sender.send(outcome);
                    }
                }
                continue;
            }
        }
        if event_tx.send(event).await.is_err() {
            return; // Bridge dropped; nothing left to receive.
        }
    }
    if let Ok(mut slot) = handshake_slot.lock() {
        if let Some(sender) = slot.take() {
            let _ = sender.send(Err(ModelCallError::Message(
                "codex app-server closed stdout before initialize completed".into(),
            )));
        }
    }
    let _ = event_tx
        .send(ServerEvent::Malformed {
            reason: "codex app-server closed stdout (process exited)".to_string(),
            line: String::new(),
        })
        .await;
}

// ---------------------------------------------------------------------------
// Section A: continuity gate — decide whether an incoming call continues the
// existing thread. Strict prefix extension + unchanged identity => reuse.
// ---------------------------------------------------------------------------

impl ThreadIdentity {
    fn build(
        model: Option<&str>,
        cwd: &Path,
        tools: &[OpenAICompatibleRequestTool],
        messages: &[TransportRequestMessage],
    ) -> Self {
        let tools_digest = serde_json::to_string(tools).expect("serializable tool schemas");
        let system_digest = serde_json::to_string(&Value::Array(
            messages
                .iter()
                .filter(|m| matches!(m.role, ChatRoleTag::System))
                .map(|m| serde_json::to_value(m).unwrap_or(Value::Null))
                .collect::<Vec<_>>(),
        ))
        .expect("serializable messages");
        ThreadIdentity {
            model: model.map(|m| m.to_string()),
            cwd: Some(cwd.to_path_buf()),
            tools_digest,
            system_digest,
        }
    }
}

/// What the bridge should do with the thread for this call.
#[derive(Debug, PartialEq, Eq)]
enum Continuation {
    /// Thread exists and the messages strictly extend what it saw.
    Reuse,
    /// Thread exists but context diverged; start fresh and replay history.
    Replay,
    /// No thread yet; start one.
    Fresh,
}

fn decide_continue(
    state: &ThreadState,
    model: Option<&str>,
    cwd: &Path,
    messages: &[TransportRequestMessage],
    tools: &[OpenAICompatibleRequestTool],
) -> Continuation {
    let Some(existing) = &state.identity else {
        return Continuation::Fresh;
    };
    let identity = ThreadIdentity::build(model, cwd, tools, messages);
    if *existing != identity {
        // Model switch, changed tool schemas, folded context or new system
        // prompt: never mix contexts on one thread.
        return Continuation::Replay;
    }
    // Messages must strictly extend what the thread already saw. A shorter or
    // rewritten list means the caller folded context; replay on a new thread.
    if !messages.starts_with(&state.sent_messages) {
        return Continuation::Replay;
    }
    Continuation::Reuse
}

// ---------------------------------------------------------------------------
// Section B: thread/start — one thread per run; drip's tool schemas are the
// ONLY tools (dynamicTools). Built-in execution features are disabled and
// environment access is removed so Codex never executes natively; drip owns
// execution, policy, ledger and verification.
// ---------------------------------------------------------------------------

impl CodexBridge {
    fn dynamic_tools(tools: &[OpenAICompatibleRequestTool]) -> Vec<Value> {
        tools
            .iter()
            .map(|tool| {
                let function = &tool.function;
                json!({
                    "type": "function",
                    "name": function.name.clone(),
                    "description": function.description.clone(),
                    "inputSchema": function.parameters.clone(),
                })
            })
            .collect()
    }

    fn base_isolation_config(&self) -> Value {
        // Keep native execution separate from drip's tool executor:
        // - feature flags disable every native execution surface;
        // - approval requests are rejected (drip answers approvals itself);
        // - `environments: []` denies all environment access for turns
        //   without an override;
        // - sandbox read-only + approvalPolicy "never" as defense in depth.
        // NOTE: AppConfig.default_tools_enabled governs connected apps only —
        // it is NOT a global built-in-tool switch and is not relied on here.
        let mut overrides = self
            .config
            .config_overrides
            .clone()
            .unwrap_or_else(|| json!({}));
        let features = json!({
            "shell_tool": false,
            "unified_exec": false,
            "web_search": false,
            "apps": false,
            "plugins": false,
            "multi_agent": false,
            "apply_patch_freeform": false,
        });
        merge_json_object(&mut overrides, &json!({ "features": features }));
        merge_json_object(
            &mut overrides,
            &json!({
                "web_search": "disabled",
                "features": {"code_mode": false, "code_mode_only": false, "js_repl": false,
                    "hooks": false, "codex_hooks": false, "image_generation": false,
                    "browser_use": false, "computer_use": false}
            }),
        );
        if self.config.force_chatgpt_auth {
            overrides["forced_login_method"] = json!("chatgpt");
        }
        overrides
    }

    async fn ensure_thread(
        &mut self,
        model: Option<&str>,
        cwd: &Path,
        messages: &[TransportRequestMessage],
        tools: &[OpenAICompatibleRequestTool],
        identity: Option<ThreadIdentity>,
    ) -> Result<String, ModelCallError> {
        {
            let state = self.state.lock().expect("thread state poisoned");
            if let Some(thread_id) = &state.thread_id {
                return Ok(thread_id.clone());
            }
        }
        if self.config.force_chatgpt_auth {
            self.verify_chatgpt_billing().await?;
        }
        let params = json!({
            "cwd": cwd.to_string_lossy(),
            "dynamicTools": Self::dynamic_tools(tools),
            "model": model,
            "modelProvider": "openai",
            "approvalPolicy": "never",
            "sandbox": "read-only",
            "environments": [],
            "ephemeral": true,
            "baseInstructions": system_prompt_of(messages),
            "config": self.base_isolation_config(),
        });
        let request_id = self.send_request("thread/start", params).await?;
        let result = self
            .await_response(request_id, self.config.request_timeout_ms)
            .await?;
        let thread_id = result
            .pointer("/thread/id")
            .and_then(Value::as_str)
            .map(|s| s.to_string())
            .ok_or_else(|| {
                ModelCallError::Message(format!(
                    "codex thread/start returned no thread.id: {}",
                    serde_json::to_string(&result).unwrap_or_default()
                ))
            })?;
        {
            let mut state = self.state.lock().expect("thread state poisoned");
            state.thread_id = Some(thread_id.clone());
            state.identity = identity;
            state.messages_sent = 0;
        }
        Ok(thread_id)
    }

    /// Await the JSON-RPC response for a client request id, draining
    /// interleaved notifications and answering stray server requests so the
    /// protocol cannot stall while no turn is running.
    async fn await_response(
        &mut self,
        request_id: JsonRpcId,
        timeout_ms: u64,
    ) -> Result<Value, ModelCallError> {
        let waiter = async {
            while let Some(event) = self.events.recv().await {
                match event {
                    ServerEvent::Response { id, result, error } => {
                        if id == request_id {
                            return match error {
                                Some(error) => Err(ModelCallError::Message(error.to_string())),
                                None => Ok(result.unwrap_or(Value::Null)),
                            };
                        }
                        // A different response while awaiting: keep draining.
                    }
                    event @ ServerEvent::ServerRequest { .. } => {
                        self.deferred_events.push_back(event)
                    }
                    ServerEvent::Malformed { reason, .. } => {
                        return Err(ModelCallError::Message(self.note_stderr(&reason)));
                    }
                    event @ ServerEvent::Notification { .. } => {
                        self.deferred_events.push_back(event)
                    }
                }
            }
            Err(ModelCallError::Message(self.note_stderr(
                "codex app-server closed stdout while awaiting a response",
            )))
        };
        match tokio::time::timeout(Duration::from_millis(timeout_ms), waiter).await {
            Ok(outcome) => outcome,
            Err(_) => Err(ModelCallError::Message(format!(
                "timed out after {}ms waiting for codex response",
                timeout_ms
            ))),
        }
    }

    /// AbortSignal poll (model_call.rs::wait_for_abort is private there):
    /// resolves when the operator aborts, checked every 20ms like the HTTP
    /// transport's Stopped outcome.
    async fn poll_abort(signal: Option<&AbortSignal>) {
        let mut interval = tokio::time::interval(Duration::from_millis(20));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if signal.map(|signal| signal.is_aborted()).unwrap_or(false) {
                return;
            }
        }
    }

    /// Final assistant text of a completed turn: walk items[].content[].text
    /// AND items[].fragments[].text defensively (schema drifted here across
    /// CLI versions); falls back to item/agentMessage/delta accumulation.
    fn extract_turn_text(turn: &Value) -> String {
        let mut parts: Vec<String> = Vec::new();
        let mut push_text = |text: &str| {
            if !text.is_empty() && !parts.last().map(|last| last == text).unwrap_or(false) {
                parts.push(text.to_string());
            }
        };
        if let Some(items) = turn.get("items").and_then(Value::as_array) {
            for item in items {
                if item.get("type").and_then(Value::as_str) != Some("agentMessage") {
                    continue;
                }
                for key in ["content", "fragments", "text"] {
                    match item.get(key) {
                        Some(Value::String(text)) => push_text(text),
                        Some(Value::Array(blocks)) => {
                            for block in blocks {
                                if let Some(text) = block.get("text").and_then(Value::as_str) {
                                    push_text(text);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        parts.join("\n")
    }

    fn map_usage(usage: CodexTokenUsage) -> OpenAICompatibleResponseUsage {
        OpenAICompatibleResponseUsage {
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            completion_tokens: Some(usage.output_tokens),
            prompt_tokens: Some(usage.input_tokens),
            prompt_tokens_details: Some(
                crate::harness::model_call::OpenAICompatibleResponsePromptTokensDetails {
                    cached_tokens: Some(usage.cached_input_tokens),
                },
            ),
            total_tokens: Some(usage.total_tokens),
        }
    }

    fn auth_diagnostic(message: &str) -> String {
        let lowered = message.to_ascii_lowercase();
        if lowered.contains("unauthorized")
            || lowered.contains("not logged in")
            || lowered.contains("unauthorizedRequest")
            || lowered.contains("401")
        {
            format!(
                "{} (codex is not logged in with ChatGPT — run `codex login`)",
                message
            )
        } else {
            message.to_string()
        }
    }

    /// Interrupt + kill the active turn (turn/interrupt BEFORE kill, then
    /// poison so every later call fails fast with the captured reason).
    async fn interrupt_and_kill(&mut self, context: &str) -> String {
        if let (Some(thread_id), Some(turn_id)) = (
            self.state.lock().ok().and_then(|s| s.thread_id.clone()),
            self.active_turn_id.clone(),
        ) {
            let _ = tokio::time::timeout(Duration::from_millis(100), async {
                let id = self
                    .send_request(
                        "turn/interrupt",
                        json!({ "threadId": thread_id, "turnId": turn_id }),
                    )
                    .await?;
                self.await_response(id, 100).await
            })
            .await;
        }
        self.active_turn = None;
        self.active_turn_id = None;
        let child = self.child.lock().ok().and_then(|mut slot| slot.take());
        if let Some(mut child) = child {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
        }
        let reason = self.note_stderr(context);
        self.set_poison(reason.clone());
        reason
    }

    /// The public model-call entry used by ModelCaller: one
    /// call either answers the pending tool request, or advances the thread
    /// with a new turn, and always returns an OpenAI-compatible response.
    pub async fn call(
        &mut self,
        messages: &[TransportRequestMessage],
        tools: &[OpenAICompatibleRequestTool],
        signal: Option<&AbortSignal>,
        reasoning_effort: Option<&str>,
    ) -> Result<OpenAICompatibleResponse, ModelCallError> {
        if let Some(reason) = self.poison_reason_opt() {
            return Err(ModelCallError::Message(reason));
        }
        self.allowed_tools = tools
            .iter()
            .map(|tool| tool.function.name.clone())
            .collect();
        let deadline =
            tokio::time::Instant::now() + Duration::from_millis(self.config.request_timeout_ms);
        let outcome = tokio::select! {
            outcome = self.call_inner(messages, tools, signal, reasoning_effort) => outcome,
            _ = Self::poll_abort(signal) => {
                let reason = self.interrupt_and_kill("The run was stopped while a codex turn was in flight").await;
                Err(ModelCallError::Message(reason))
            }
            _ = tokio::time::sleep_until(deadline) => {
                let reason = self.interrupt_and_kill(&format!(
                    "codex turn exceeded the {}ms deadline", self.config.request_timeout_ms
                )).await;
                Err(ModelCallError::Message(reason))
            }
        };
        if outcome.is_err() && self.poison_reason_opt().is_none() {
            self.interrupt_and_kill("codex call failed").await;
        }
        outcome
    }

    async fn call_inner(
        &mut self,
        messages: &[TransportRequestMessage],
        tools: &[OpenAICompatibleRequestTool],
        signal: Option<&AbortSignal>,
        reasoning_effort: Option<&str>,
    ) -> Result<OpenAICompatibleResponse, ModelCallError> {
        let cwd = self.config.cwd.clone().unwrap_or(
            std::env::current_dir().map_err(|e| ModelCallError::Message(e.to_string()))?,
        );
        let continuation = {
            let state = self
                .state
                .lock()
                .map_err(|_| poisoned_lock_error("thread"))?;
            decide_continue(&state, self.config.model.as_deref(), &cwd, messages, tools)
        };
        if continuation == Continuation::Replay {
            let old_thread = self
                .state
                .lock()
                .map_err(|_| poisoned_lock_error("thread"))?
                .thread_id
                .clone();
            if let (Some(thread_id), Some(turn_id)) = (old_thread, self.active_turn_id.clone()) {
                let id = self
                    .send_request(
                        "turn/interrupt",
                        json!({"threadId":thread_id,"turnId":turn_id}),
                    )
                    .await?;
                self.await_response(id, self.config.request_timeout_ms)
                    .await?;
            }
            self.pending_tool = None;
            self.active_turn = None;
            self.active_turn_id = None;
            *self
                .state
                .lock()
                .map_err(|_| poisoned_lock_error("thread"))? = ThreadState::default();
            self.total_usage = CodexTokenUsage::default();
            self.reported_usage = CodexTokenUsage::default();
        }

        // The caller owns execution; only its matching tool result can
        // release a pending request. Check context identity before replying.
        if let Some(pending) = self.pending_tool.take() {
            let result_message = messages.iter().rev().find_map(|message| {
                if matches!(message.role, ChatRoleTag::Tool)
                    && message.tool_call_id.as_deref() == Some(pending.call_id.as_str())
                {
                    message_text(message)
                } else {
                    None
                }
            });
            let text = result_message.ok_or_else(|| ModelCallError::Message(format!(
                "drip returned no tool result for codex tool call {} ({}); cannot answer the pending item/tool/call",
                pending.call_id, pending.tool
            )))?;
            self.answer_server_request(
                &pending.request_id,
                Ok(json!({
                    "success": true,
                    "contentItems": [{ "type": "inputText", "text": text }]
                })),
            )
            .await?;
            if let Ok(mut state) = self.state.lock() {
                state
                    .answered_calls
                    .insert(pending.call_id.clone(), pending.tool.clone());
                state.sent_messages = messages.to_vec();
                state.messages_sent = messages.len();
            }
            return self.await_turn_completion(signal).await;
        }

        // Lane 2: advance the thread. Continuation gate first (fresh / reuse
        // / replay / ephemeral summary thread).
        let model = self.config.model.clone();
        if matches!(continuation, Continuation::Fresh | Continuation::Replay) {
            let identity = Some(ThreadIdentity::build(
                model.as_deref(),
                &cwd,
                tools,
                messages,
            ));
            self.ensure_thread(model.as_deref(), &cwd, messages, tools, identity)
                .await?;
        }
        let thread_id = {
            let state = self.state.lock().expect("thread state poisoned");
            state.thread_id.clone().ok_or_else(|| {
                ModelCallError::Message(
                    "codex bridge has no thread; thread/start did not run".to_string(),
                )
            })?
        };

        // A fresh process (including drip --resume) must receive all history,
        // with roles and tool results intact. Existing threads receive only
        // the suffix they have not seen.
        let mut input: Vec<Value> = Vec::new();
        let sent = self
            .state
            .lock()
            .map_err(|_| poisoned_lock_error("thread"))?
            .messages_sent;
        for message in &messages[sent.min(messages.len())..] {
            if !matches!(message.role, ChatRoleTag::System) {
                let mut textual = message.clone();
                if let Some(TransportContent::Parts(parts)) = &message.content {
                    for part in parts {
                        if let TransportContentPart::ImageUrl { image_url } = part {
                            input.push(json!({"type":"image","url":image_url.url}));
                        }
                    }
                    textual.content = message_text(message).map(TransportContent::Text);
                }
                input.push(json!({"type":"text","text":serde_json::to_string(&textual)
                    .map_err(|e| ModelCallError::Message(e.to_string()))?}));
            }
        }
        if input.is_empty() {
            input.push(json!({ "type": "text", "text": "Continue." }));
        }

        let effort = reasoning_effort
            .map(|e| json!(e))
            .or_else(|| self.config.reasoning_effort.clone().map(Value::String));
        let mut params = json!({
            "threadId": thread_id,
            "input": input,
            "approvalPolicy": "never",
        });
        if let Some(model) = &self.config.model {
            params["model"] = json!(model);
        }
        if let Some(effort) = effort {
            params["effort"] = effort;
        }
        let turn_request_id = self.send_request("turn/start", params).await?;
        let turn_result = self
            .await_response(turn_request_id.clone(), self.config.request_timeout_ms)
            .await?;
        let turn_id = turn_result
            .pointer("/turn/id")
            .and_then(Value::as_str)
            .map(|s| s.to_string());
        self.active_turn = Some(turn_request_id);
        self.active_turn_id = turn_id;
        {
            let mut state = self
                .state
                .lock()
                .map_err(|_| poisoned_lock_error("thread"))?;
            state.sent_messages = messages.to_vec();
            state.messages_sent = messages.len();
        }

        self.await_turn_completion(signal).await
    }

    /// Shared event loop for a running turn: yields the FIRST dynamic tool
    /// request as an OpenAI tool-calls response (turn stays open; drip
    /// answers on its next call — returning on EVERY item/tool/call, even a
    /// second one after an earlier was answered, or the second would
    /// deadlock), or the completed turn's text.
    async fn await_turn_completion(
        &mut self,
        signal: Option<&AbortSignal>,
    ) -> Result<OpenAICompatibleResponse, ModelCallError> {
        let mut deltas = String::new();
        let timeout = Duration::from_millis(self.config.request_timeout_ms);
        loop {
            let event = tokio::select! {
                event = async {
                    match self.deferred_events.pop_front() {
                        Some(event) => Some(event),
                        None => self.events.recv().await,
                    }
                } => event,
                _ = Self::poll_abort(signal) => {
                    let reason = self.interrupt_and_kill("The run was stopped while a codex turn was in flight").await;
                    return Err(ModelCallError::Message(reason));
                }
                _ = tokio::time::sleep(timeout) => {
                    let reason = self.interrupt_and_kill(&format!(
                        "codex turn exceeded the {}ms deadline", self.config.request_timeout_ms
                    )).await;
                    return Err(ModelCallError::Message(reason));
                }
            };
            match event {
                Some(ServerEvent::ServerRequest { id, method, params }) => {
                    if !self.is_current_event(&params) {
                        continue;
                    }
                    match method.as_str() {
                        "item/tool/call" => {
                            let call_id = params
                                .get("callId")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string();
                            let tool = params
                                .get("tool")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string();
                            if call_id.is_empty() || tool.is_empty() {
                                return Err(ModelCallError::Message(
                                    "malformed codex dynamic tool request".into(),
                                ));
                            }
                            if !self.allowed_tools.contains(&tool) {
                                return Err(ModelCallError::Message(format!(
                                    "codex requested unregistered tool {tool}"
                                )));
                            }
                            if self
                                .state
                                .lock()
                                .map_err(|_| poisoned_lock_error("thread"))?
                                .answered_calls
                                .contains_key(&call_id)
                            {
                                return Err(ModelCallError::Message(format!(
                                    "codex repeated tool call id {call_id}"
                                )));
                            }
                            let arguments = match params.get("arguments").unwrap_or(&Value::Null) {
                                Value::String(raw) => raw.clone(),
                                Value::Null => "{}".to_string(),
                                other => serde_json::to_string(other)
                                    .unwrap_or_else(|_| "{}".to_string()),
                            };
                            self.pending_tool = Some(PendingToolCall {
                                request_id: id,
                                call_id: call_id.clone(),
                                tool: tool.clone(),
                            });
                            return Ok(OpenAICompatibleResponse {
                                choices: Some(vec![OpenAICompatibleResponseChoice {
                                    finish_reason: Some("tool_calls".to_string()),
                                    message: Some(OpenAICompatibleResponseMessage {
                                        anthropic_content: None,
                                        content: if deltas.is_empty() { None } else { Some(Value::String(deltas)) },
                                        tool_calls: Some(vec![OpenAICompatibleToolCall {
                                            function: Some(crate::harness::transport::OpenAICompatibleToolCallFunction {
                                                arguments: Some(arguments),
                                                name: Some(tool),
                                            }),
                                            id: Some(call_id),
                                            tool_type: Some("function".to_string()),
                                        }]),
                                    }),
                                }]),
                                error: None,
                                usage: Some(self.take_usage()),
                            });
                        }
                        // Every other server->client request is a native
                        // execution/approval surface drip denies outright —
                        // drip owns execution, policy, ledger, verification.
                        _ => {
                            let denial = format!(
                                "drip denies codex {} requests: drip owns execution and policy",
                                method
                            );
                            let _ = self
                                .answer_server_request(
                                    &id,
                                    Err(JsonRpcErrorError {
                                        code: -32601,
                                        message: denial,
                                        data: None,
                                    }),
                                )
                                .await;
                        }
                    }
                }
                Some(ServerEvent::Notification { method, params }) => {
                    if !self.is_current_event(&params) {
                        continue;
                    }
                    match method.as_str() {
                        "turn/completed" => {
                            let turn = params.get("turn").cloned().unwrap_or(Value::Null);
                            if turn.get("status").and_then(Value::as_str) != Some("completed") {
                                let message = turn
                                    .pointer("/error/message")
                                    .and_then(Value::as_str)
                                    .unwrap_or("codex turn did not complete successfully");
                                return Err(ModelCallError::Message(message.into()));
                            }
                            let text = {
                                let extracted = Self::extract_turn_text(&turn);
                                if extracted.is_empty() {
                                    std::mem::take(&mut deltas)
                                } else {
                                    extracted
                                }
                            };
                            self.active_turn = None;
                            self.active_turn_id = None;
                            return Ok(OpenAICompatibleResponse {
                                choices: Some(vec![OpenAICompatibleResponseChoice {
                                    finish_reason: Some("stop".to_string()),
                                    message: Some(OpenAICompatibleResponseMessage {
                                        anthropic_content: None,
                                        content: Some(Value::String(text)),
                                        tool_calls: None,
                                    }),
                                }]),
                                error: None,
                                usage: Some(self.take_usage()),
                            });
                        }
                        "thread/tokenUsage/updated" => {
                            let breakdown = params
                                .pointer("/tokenUsage/total")
                                .or_else(|| params.pointer("/tokenUsage/last"));
                            if let Some(found) = breakdown.and_then(CodexTokenUsage::from_value) {
                                self.total_usage = found;
                            }
                        }
                        "item/agentMessage/delta" => {
                            if let Some(delta) = params.get("delta").and_then(Value::as_str) {
                                deltas.push_str(delta);
                            }
                        }
                        "item/completed" => {
                            let item = &params["item"];
                            match item["type"].as_str() {
                                Some("agentMessage") => {
                                    if let Some(text) = item["text"].as_str() {
                                        deltas = text.to_string();
                                    }
                                }
                                Some("commandExecution" | "fileChange" | "mcpToolCall") => {
                                    return Err(ModelCallError::Message(
                                    "Codex attempted a native tool; this provider requires execution through drip tools".into()));
                                }
                                _ => {}
                            }
                        }
                        "error" | "turn/failed" | "thread/error" => {
                            if params["willRetry"].as_bool() == Some(true) {
                                continue;
                            }
                            let error = &params["error"];
                            let message = error
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("codex turn failed")
                                .to_string();
                            return Err(ModelCallError::Message(
                                self.note_stderr(&Self::auth_diagnostic(&message)),
                            ));
                        }
                        _ => {}
                    }
                }
                Some(ServerEvent::Response { id, error, .. }) => {
                    if Some(&id) == self.active_turn.as_ref() {
                        if let Some(error) = error {
                            return Err(ModelCallError::Message(
                                self.note_stderr(&Self::auth_diagnostic(&error.to_string())),
                            ));
                        }
                    }
                }
                Some(ServerEvent::Malformed { reason, .. }) => {
                    return Err(ModelCallError::Message(self.note_stderr(&reason)));
                }
                None => {
                    return Err(ModelCallError::Message(self.note_stderr(
                        "codex app-server closed stdout while a turn was in flight",
                    )));
                }
            }
        }
    }

    fn is_current_event(&self, params: &Value) -> bool {
        let thread = self.state.lock().ok().and_then(|s| s.thread_id.clone());
        if params
            .get("threadId")
            .and_then(Value::as_str)
            .is_some_and(|id| Some(id) != thread.as_deref())
        {
            return false;
        }
        !params
            .get("turnId")
            .or_else(|| params.pointer("/turn/id"))
            .and_then(Value::as_str)
            .is_some_and(|id| Some(id) != self.active_turn_id.as_deref())
    }

    fn take_usage(&mut self) -> OpenAICompatibleResponseUsage {
        let delta = CodexTokenUsage {
            input_tokens: (self.total_usage.input_tokens - self.reported_usage.input_tokens).max(0),
            cached_input_tokens: (self.total_usage.cached_input_tokens
                - self.reported_usage.cached_input_tokens)
                .max(0),
            output_tokens: (self.total_usage.output_tokens - self.reported_usage.output_tokens)
                .max(0),
            total_tokens: (self.total_usage.total_tokens - self.reported_usage.total_tokens).max(0),
        };
        self.reported_usage = self.total_usage;
        Self::map_usage(delta)
    }
}

impl Drop for CodexBridge {
    fn drop(&mut self) {
        // Best-effort synchronous cleanup: close stdin so the app-server sees
        // EOF, then kill the child. kill_on_drop covers the take() races.
        if let Ok(mut stdin_slot) = self.stdin.try_lock() {
            *stdin_slot = None;
        }
        if let Ok(mut slot) = self.child.lock() {
            if let Some(child) = slot.as_mut() {
                let _ = child.start_kill();
            }
        }
    }
}

fn merge_json_object(base: &mut Value, overlay: &Value) {
    match (base, overlay) {
        (Value::Object(base_map), Value::Object(overlay_map)) => {
            for (key, value) in overlay_map {
                match base_map.get_mut(key) {
                    Some(existing) => merge_json_object(existing, value),
                    None => {
                        base_map.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        (base, overlay) => *base = overlay.clone(),
    }
}

fn system_prompt_of(messages: &[TransportRequestMessage]) -> Option<String> {
    let mut parts = Vec::new();
    for message in messages {
        if matches!(message.role, ChatRoleTag::System) {
            if let Some(text) = message_text(message) {
                parts.push(text);
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

fn message_text(message: &TransportRequestMessage) -> Option<String> {
    match &message.content {
        Some(TransportContent::Text(text)) => Some(text.clone()),
        Some(TransportContent::Parts(parts)) => {
            let mut out = String::new();
            for part in parts {
                if let TransportContentPart::Text { text } = part {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(text);
                }
            }
            if out.is_empty() {
                None
            } else {
                Some(out)
            }
        }
        None => None,
    }
}
