//! `/btw` — a *sidebar* conversation spawned from a running or finished
//! drip session, Claude's `/btw` idea: a second drip you can ask side
//! questions about what has already happened without polluting the main chat.
//!
//! Three pure halves plus one bounded inference call:
//!
//! 1. [`build_btw_digest`] compiles the session's own transcript (goals,
//!    assistant text, tool calls, warnings, run ends) into a compact digest
//!    and names the transcript file, so the sidebar knows *where* to find the
//!    rest instead of guessing.
//! 2. [`build_btw_system_prompt`] frames the call as a sidebar conversation —
//!    explicitly NOT the agent working the goal — and asks for conversational
//!    prose rather than a run summary.
//! 3. [`BtwThread`] keeps the sidebar's own turns, so `/btw <follow-up>` is a
//!    continuation of one conversation. The thread never enters the session's
//!    context: it lives in memory here and is sent only to the sidebar call.
//!
//! Every failure (no profile or credential, offline restriction, timeout,
//! malformed reply) resolves to `None` and the caller prints a plain notice;
//! a sidebar question never fails the session or touches token accounting.

use crate::cli::transcript::{TranscriptEntry, TranscriptEventEntry};
use crate::core::types::HarnessEventType;
use crate::harness::chat_types::ChatRoleTag;
use crate::harness::model_call::{
    create_model_caller, ModelCallOptions, ModelCallerDeps, ModelRoute, OpenAICompatibleResponse,
};
use crate::harness::transport::{TransportContent, TransportRequestMessage};
use crate::tui::pane_title::strip_control;
use serde_json::Value;
use std::sync::Arc;

/// Wall-clock cap for one sidebar answer. Longer than the title/rename calls:
/// a conversational answer about a long transcript is not a five-word label.
pub const BTW_TIMEOUT_MS: u64 = 60_000;

/// Character budget for the transcript digest handed to the sidebar. Big
/// enough to carry the shape of a real session, small enough to stay cheap.
pub const BTW_DIGEST_CHARS: usize = 12_000;

/// How many of the sidebar's most recent turns are replayed to the model, so
/// a long btw conversation cannot grow the request without bound.
pub const BTW_HISTORY_TURNS: usize = 24;

/// Hard cap on one sidebar reply, applied before it is printed.
pub const BTW_MAX_REPLY_CHARS: usize = 8_000;

/// The sidebar framing. Its job is to keep the second drip honest about being
/// a *side* conversation: it reads and explains the session, it does not run
/// it, and it answers in prose instead of a run summary because the operator
/// is chatting, not reading a status report.
pub const BTW_SYSTEM_PROMPT: &str = "You are a *sidebar* (`/btw`) conversation spawned from a drip coding session. You are NOT the agent working on that session's goal, and nothing you say is sent to it: the operator opened this sidebar to ask side questions about the session without disturbing it.\n\nWhat you have:\n- Below: a digest of that session's transcript — its goals, its own assistant replies, its tool calls, tool results, warnings and run ends — plus the session id and the path to the full transcript.jsonl file for detail the digest does not carry.\n\nHow to answer:\n- Answer conversationally, in plain prose: what has happened so far, what the session is doing now, why it looks stuck or slow, what it read or changed, what it said, what seems worth doing next.\n- Do NOT produce a run summary, task table, or status report unless the operator explicitly asks for one. A couple of short paragraphs or a few bullets read better than headings.\n- Ground every claim in the digest. When the digest does not contain the answer, say so plainly and name what you would need (or the transcript line to check) instead of inventing it.\n- You cannot run tools, edit files, or change that session — you only read and explain.\n- The operator may ask follow-ups; keep the thread of the conversation and build on what you already said.\n- Be concise: your reply is printed straight into the operator's chat as it arrives.";

/// Which side of the sidebar a turn belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BtwRole {
    Assistant,
    User,
}

/// One sidebar turn.
#[derive(Clone, Debug, PartialEq)]
pub struct BtwTurn {
    pub role: BtwRole,
    pub text: String,
}

/// The sidebar's own conversation: in memory only, never the session's context.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BtwThread {
    turns: Vec<BtwTurn>,
}

impl BtwThread {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.turns.is_empty()
    }

    pub fn len(&self) -> usize {
        self.turns.len()
    }

    /// How many questions the operator has asked this sidebar.
    pub fn ask_count(&self) -> usize {
        self.turns
            .iter()
            .filter(|turn| turn.role == BtwRole::User)
            .count()
    }

    pub fn reset(&mut self) {
        self.turns.clear();
    }

    pub fn turns(&self) -> &[BtwTurn] {
        &self.turns
    }

    pub fn push_user(&mut self, text: &str) {
        self.turns.push(BtwTurn {
            role: BtwRole::User,
            text: text.to_string(),
        });
    }

    pub fn push_assistant(&mut self, text: &str) {
        self.turns.push(BtwTurn {
            role: BtwRole::Assistant,
            text: text.to_string(),
        });
    }

    /// The tail of the thread, oldest first, for `/btw` with no question.
    pub fn tail_lines(&self, max_turns: usize) -> Vec<String> {
        let start = self.turns.len().saturating_sub(max_turns);
        self.turns[start..]
            .iter()
            .flat_map(|turn| {
                let who = match turn.role {
                    BtwRole::User => "you",
                    BtwRole::Assistant => "btw",
                };
                turn.text
                    .lines()
                    .take(6)
                    .map(|line| format!("btw | {who}: {line}"))
                    .collect::<Vec<_>>()
            })
            .collect()
    }
}

/// Collapse whitespace runs and hard-cut to `max_chars`, so one long tool
/// result cannot dominate the digest.
fn one_line(raw: &str, max_chars: usize) -> String {
    let flat = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max_chars {
        return flat;
    }
    let cut: String = flat.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{cut}…")
}

/// True for the sidebar's own chat lines (they are printed with a `btw`
/// prefix), so a sidebar never digests its own previous answers as session
/// history.
fn is_btw_line(text: &str) -> bool {
    text.trim_start().starts_with("btw")
}

/// One readable digest line for a transcript event, or None for the events
/// that carry no narrative (iteration starts, token accounting, repeats).
fn event_digest_line(event: &TranscriptEventEntry) -> Option<String> {
    let tool = event
        .data
        .as_ref()
        .and_then(|data| data.tool_name.as_deref());
    let (label, cap) = match event.kind {
        HarnessEventType::ModelText => ("assistant", 1200),
        HarnessEventType::ToolCall => ("tool call", 160),
        HarnessEventType::ToolResult => ("tool result", 300),
        HarnessEventType::RunSummary => ("run summary", 600),
        HarnessEventType::TaskFinished => ("task finished", 300),
        HarnessEventType::OperatorMessage => ("operator message", 300),
        HarnessEventType::Question => ("question", 300),
        HarnessEventType::RunWarning => ("warning", 200),
        HarnessEventType::HarnessOp => ("harness op", 200),
        _ => return None,
    };
    let detail = one_line(&event.detail, cap);
    let body = match (tool, detail.trim().is_empty()) {
        (Some(name), false) => format!("{name} — {detail}"),
        (Some(name), true) => name.to_string(),
        (None, false) => detail,
        (None, true) => return None,
    };
    Some(format!("{label}: {body}"))
}

/// Compiles the session's transcript into the sidebar's digest.
///
/// The three header lines always survive (the sidebar needs to know *which*
/// session and *where* the full transcript is); the body is tail-filled to
/// `max_chars`, so a long session keeps its most recent history. Read-only and
/// deterministic.
pub fn build_btw_digest(
    entries: &[TranscriptEntry],
    session_id: &str,
    transcript_path: &str,
    running: bool,
    last_goal: Option<&str>,
    max_chars: usize,
) -> String {
    let mut header = vec![
        format!("session id: {session_id}"),
        format!("full transcript file: {transcript_path}"),
        format!(
            "state: {}",
            if running {
                "a run is in flight right now (the operator is chatting from inside it)"
            } else {
                "no run in flight (the session is idle or finished)"
            }
        ),
    ];
    if let Some(goal) = last_goal.filter(|goal| !goal.trim().is_empty()) {
        header.push(format!("last goal (recorded): {}", one_line(goal, 400)));
    }

    let mut body: Vec<String> = Vec::new();
    for entry in entries {
        match entry {
            TranscriptEntry::Goal(goal) if !goal.text.trim().is_empty() => {
                body.push(format!("user goal: {}", one_line(&goal.text, 400)))
            }
            TranscriptEntry::Model(model) => body.push(format!(
                "model route: {} ({})",
                model.model, model.profile_id
            )),
            TranscriptEntry::Event(event) => {
                if let Some(line) = event_digest_line(event) {
                    body.push(line);
                }
            }
            TranscriptEntry::RunEnd(run) => body.push(format!(
                "run end: {:?} after {} iteration(s)",
                run.reason, run.iterations
            )),
            TranscriptEntry::Info(note) if !is_btw_line(&note.text) => {
                body.push(format!("notice: {}", one_line(&note.text, 200)))
            }
            TranscriptEntry::Error(note) if !is_btw_line(&note.text) => {
                body.push(format!("error: {}", one_line(&note.text, 200)))
            }
            TranscriptEntry::Skill(skill) => body.push(format!(
                "skill {} {}",
                skill.name,
                if skill.enabled { "enabled" } else { "disabled" }
            )),
            _ => {}
        }
    }

    let fixed: usize = header.iter().map(|line| line.chars().count() + 1).sum();
    let budget = max_chars.saturating_sub(fixed);
    let mut kept: Vec<&str> = Vec::new();
    let mut used = 0usize;
    for line in body.iter().rev() {
        let cost = line.chars().count() + 1;
        if used + cost > budget && !kept.is_empty() {
            break;
        }
        used += cost;
        kept.push(line.as_str());
    }
    kept.reverse();

    let mut out = header.join("\n");
    if !kept.is_empty() {
        out.push('\n');
        out.push_str(&kept.join("\n"));
    }
    out
}

/// The sidebar's system message: the framing plus the transcript digest.
pub fn build_btw_system_prompt(digest: &str) -> String {
    format!("{BTW_SYSTEM_PROMPT}\n\n=== SESSION TRANSCRIPT DIGEST (oldest first) ===\n{digest}")
}

/// The request messages for one sidebar question: system framing + digest,
/// then the sidebar's recent turns (capped), then the question itself.
pub fn build_btw_messages(
    system: &str,
    turns: &[BtwTurn],
    question: &str,
) -> Vec<TransportRequestMessage> {
    let mut messages = vec![TransportRequestMessage {
        content: Some(TransportContent::Text(system.to_string())),
        name: None,
        role: ChatRoleTag::System,
        tool_call_id: None,
        tool_calls: None,
        anthropic_content: None,
    }];
    let start = turns.len().saturating_sub(BTW_HISTORY_TURNS);
    for turn in &turns[start..] {
        if turn.text.trim().is_empty() {
            continue;
        }
        messages.push(TransportRequestMessage {
            content: Some(TransportContent::Text(turn.text.clone())),
            name: None,
            role: match turn.role {
                BtwRole::User => ChatRoleTag::User,
                BtwRole::Assistant => ChatRoleTag::Assistant,
            },
            tool_call_id: None,
            tool_calls: None,
            anthropic_content: None,
        });
    }
    messages.push(TransportRequestMessage {
        content: Some(TransportContent::Text(question.to_string())),
        name: None,
        role: ChatRoleTag::User,
        tool_call_id: None,
        tool_calls: None,
        anthropic_content: None,
    });
    messages
}

/// Strips escape sequences and control characters from a sidebar reply while
/// keeping its newlines: unlike a pane title (single line by definition) a
/// sidebar answer is prose, and collapsing it to one line would lose the
/// paragraphs that make it readable in the chat.
pub fn sanitize_btw_reply(raw: &str) -> String {
    let mut out = String::new();
    for (index, line) in raw.split('\n').enumerate() {
        if index > 0 {
            out.push('\n');
        }
        out.push_str(strip_control(line).trim_end());
    }
    // Collapse runs of blank lines so a reply cannot pad the chat.
    let mut collapsed = String::with_capacity(out.len());
    let mut blanks = 0usize;
    for line in out.split('\n') {
        if line.trim().is_empty() {
            blanks += 1;
            if blanks > 1 {
                continue;
            }
        } else {
            blanks = 0;
        }
        if !collapsed.is_empty() {
            collapsed.push('\n');
        }
        collapsed.push_str(line.trim_end());
    }
    collapsed.trim().to_string()
}

/// Extracts the sidebar's reply: plain multi-line text, escape sequences and
/// non-newline control characters stripped (it is printed into a terminal),
/// trimmed, bounded. Empty or malformed output is refused so the caller
/// prints a notice instead of a blank answer. Content may arrive as a plain
/// string or `{text}` parts.
pub fn extract_btw_reply(response: &OpenAICompatibleResponse) -> Option<String> {
    let message = response.choices.as_ref()?.first()?.message.as_ref()?;
    let content = message.content.as_ref()?;
    let text = match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => {
            let mut assembled = String::new();
            for part in parts {
                if let Value::Object(part) = part {
                    if let Some(Value::String(text)) = part.get("text") {
                        assembled.push_str(text);
                    }
                }
            }
            assembled
        }
        _ => return None,
    };
    let cleaned = sanitize_btw_reply(&text);
    // Blank lines inside a reply are fine; a reply that is entirely empty or
    // entirely whitespace is not an answer.
    if cleaned.is_empty() {
        return None;
    }
    let mut bounded = cleaned;
    if bounded.chars().count() > BTW_MAX_REPLY_CHARS {
        bounded = bounded
            .chars()
            .take(BTW_MAX_REPLY_CHARS.saturating_sub(1))
            .collect::<String>();
        bounded.push('…');
    }
    Some(bounded)
}

/// One tool-free sidebar request, bounded by `timeout_ms`. Mirrors
/// `terminal_title::generate_chat_title` / `session_name::generate_session_name`;
/// no tools are advertised, so the sidebar can only read and explain.
pub async fn ask_btw(
    route: ModelRoute,
    system: &str,
    turns: &[BtwTurn],
    question: &str,
    timeout_ms: u64,
) -> Option<String> {
    let timeout_ms = timeout_ms.max(1);
    if question.trim().is_empty() {
        return None;
    }
    let caller = create_model_caller(ModelCallerDeps {
        codex_executable: None,
        cwd: None,
        default_transport_tools: Vec::new(),
        emit: Arc::new(|_| {}),
        fallback_route: None,
        get_iteration: Arc::new(|| 0),
        // A text-only call never reads the route object for its credential, so
        // the resolved route's Authorization must land on the deps or the
        // request goes out unauthenticated (401).
        headers: crate::tui::session_name::session_name_headers(
            vec![("content-type".to_string(), "application/json".to_string())],
            &route,
        ),
        model: route.model.clone(),
        on_retry_wait: Arc::new(|_| {}),
        on_usage: Arc::new(|_, _| {}),
        provider: route.provider.clone(),
        refresh_headers: None,
        prompt_cache_key: None,
        reasoning_effort: route.reasoning_effort.clone(),
        reasoning_effort_defaulted: false,
        request_timeout_ms: Some(timeout_ms),
        hedge_floor_ms: Some(0),
        latency_store: None,
        signal: None,
        sleep_impl: None,
        tool_route: None,
        url: route.url.clone(),
        http_client: None,
    });
    let messages = build_btw_messages(system, turns, question);
    let options = ModelCallOptions {
        include_tools: Some(false),
        route: Some(route),
        transport_tools: None,
        usage_task_id: None,
        max_tokens: None,
        tool_choice: None,
    };
    let attempt = caller.call_model(messages, Some(options));
    match tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), attempt).await {
        Ok(Ok(response)) => extract_btw_reply(&response),
        _ => None,
    }
}

/// The chat lines one sidebar exchange prints: the echoed question plus the
/// reply lines, all `btw`-prefixed so the sidebar is never mistaken for the
/// session's own output (and so the digest can skip them).
pub fn btw_chat_lines(question: &str, reply: &str) -> Vec<String> {
    let mut lines = vec![format!("btw · {question}")];
    for line in reply.lines() {
        lines.push(format!("btw | {line}"));
    }
    lines
}

#[cfg(test)]
mod tests;
