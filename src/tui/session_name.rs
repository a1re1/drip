//! Best-effort session naming, part 1: pure prompt/extract/validation halves
//! plus session.json metadata persistence. `/rename` sends transcript context
//! and the user's goal as one tool-free inference request whose reply is a
//! simple 5-7 word session name (never a tmux/pane label). Every failure mode
//! (missing profile or credential, offline restriction, timeout, malformed or
//! empty output, empty transcript) resolves to `None` so the caller keeps the
//! current name. Nothing here touches conversation history or token accounting.

use crate::cli::transcript::TranscriptEntry;
use crate::harness::model_call::{
    create_model_caller, ModelCallOptions, ModelCallerDeps, ModelRoute, OpenAICompatibleResponse,
};
use crate::harness::transport::{TransportContent, TransportRequestMessage};
use crate::tui::pane_title::{bound_words, sanitize};
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::sync::Arc;

/// How much transcript context (per-message characters) shapes the prompt.
pub const SESSION_NAME_TRANSCRIPT_CHARS: usize = 600;

/// How many recent transcript lines feed the prompt.
pub const SESSION_NAME_TRANSCRIPT_LINES: usize = 12;

/// Extracts and sanitizes the model's session-name reply, enforcing the 5-7
/// word contract. `None` keeps the current name. Content may arrive as a
/// plain string or as an array of `{text}` parts.
pub fn extract_session_name(response: &OpenAICompatibleResponse) -> Option<String> {
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
    let cleaned = sanitize(&text);
    // Strict 5-7 word contract: a reply outside the range is rejected rather
    // than silently truncated mid-sentence.
    let words = cleaned.split_whitespace().count();
    if !(5..=7).contains(&words) {
        return None;
    }
    let name = bound_words(&cleaned, 7, 64);
    if !is_valid_session_name(&name) {
        return None;
    }
    Some(name)
}

/// A session name is 5-7 words after stripping quotes/punctuation edges, and
/// is not the terminal-title fallback.
pub fn is_valid_session_name(name: &str) -> bool {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("drip") {
        return false;
    }
    let words = trimmed.split_whitespace().count();
    (5..=7).contains(&words)
}

/// Shapes the one-shot prompt from a compact transcript digest plus the
/// user's goal. Pure and deterministic.
pub fn build_session_name_prompt(goal: &str, transcript_digest: &str) -> String {
    let goal = bound_words(&sanitize(goal), 40, 800);
    let digest = bound_words(
        &sanitize(transcript_digest),
        400,
        SESSION_NAME_TRANSCRIPT_LINES * SESSION_NAME_TRANSCRIPT_CHARS,
    );
    format!(
        "Write a simple 5 to 7 word session name for a coding-agent chat, using this transcript context and the user's goal. Reply with the name only - no quotes, no markup, no trailing period.\n\nGoal: {goal}\n\nTranscript:\n{digest}"
    )
}

/// One request, bounded prompt: mirrors `terminal_title::generate_chat_title`
/// but enforces the 5-7 word session-name contract on the reply.
pub async fn generate_session_name(
    route: ModelRoute,
    goal: &str,
    transcript_digest: &str,
    timeout_ms: u64,
) -> Option<String> {
    let timeout_ms = timeout_ms.max(1);
    let caller = create_model_caller(ModelCallerDeps {
        cwd: None,
        default_transport_tools: Vec::new(),
        emit: Arc::new(|_| {}),
        fallback_route: None,
        get_iteration: Arc::new(|| 0),
        headers: vec![("content-type".to_string(), "application/json".to_string())],
        model: route.model.clone(),
        on_retry_wait: Arc::new(|_| {}),
        on_usage: Arc::new(|_, _| {}),
        provider: route.provider.clone(),
        refresh_headers: None,
        prompt_cache_key: None,
        reasoning_effort: route.reasoning_effort.clone(),
        request_timeout_ms: Some(timeout_ms),
        signal: None,
        sleep_impl: None,
        tool_route: None,
        url: route.url.clone(),
        http_client: None,
    });
    let message = TransportRequestMessage {
        content: Some(TransportContent::Text(build_session_name_prompt(goal, transcript_digest))),
        ..Default::default()
    };
    let options = ModelCallOptions {
        include_tools: Some(false),
        route: Some(route),
        transport_tools: None,
        usage_task_id: None,
    };
    let attempt = caller.call_model(vec![message], Some(options));
    match tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), attempt).await {
        Ok(Ok(response)) => extract_session_name(&response),
        _ => None,
    }
}

/// Builds a compact transcript digest from parsed transcript entries: the
/// user's goal plus the most recent goals/model turns, oldest first. An empty
/// transcript still yields the goal line; callers keep the current name on
/// total failure.
pub fn read_session_name_context(goal: &str, transcript_entries: &[TranscriptEntry]) -> String {
    let mut lines: Vec<String> = Vec::new();
    if !goal.trim().is_empty() {
        lines.push(format!(
            "User: {}",
            bound_words(&sanitize(goal), 80, SESSION_NAME_TRANSCRIPT_CHARS)
        ));
    }
    // Oldest-first, capped to the most recent window of transcript entries.
    let start = transcript_entries.len().saturating_sub(SESSION_NAME_TRANSCRIPT_LINES);
    for entry in &transcript_entries[start..] {
        match entry {
            TranscriptEntry::Goal(g) if !g.text.trim().is_empty() => lines.push(format!(
                "User: {}",
                bound_words(&sanitize(&g.text), 80, SESSION_NAME_TRANSCRIPT_CHARS)
            )),
            TranscriptEntry::Model(_) => lines.push("Assistant: (reply)".to_string()),
            _ => {}
        }
    }
    lines.join("\n")
}

// ---------------------------------------------------------------------------
// session.json persistence: the session name rides existing session metadata
// conventions (pretty JSON next to the transcript) without touching the
// sqlite index, session id, or resume behavior.
// ---------------------------------------------------------------------------

/// Reads the persisted session name, if any. Missing or malformed metadata is
/// not an error: the caller keeps the current name.
pub fn read_session_name(meta_path: &Path) -> Option<String> {
    let raw = fs::read_to_string(meta_path).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    value
        .get("sessionName")?
        .as_str()
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())
}

/// Persists the session name into existing session.json metadata, preserving
/// every other field. Best-effort: false means the caller keeps the old name.
pub fn persist_session_name(meta_path: &Path, name: &str) -> bool {
    let raw = fs::read_to_string(meta_path).unwrap_or_else(|_| "{}\n".to_string());
    let mut value: Value = match serde_json::from_str(&raw) {
        Ok(value) => value,
        Err(_) => return false,
    };
    if !value.is_object() {
        return false;
    }
    value["sessionName"] = Value::String(name.to_string());
    let body = serde_json::to_string_pretty(&value)
        .map(|s| format!("{s}\n"))
        .unwrap_or_default();
    fs::write(meta_path, body).is_ok()
}

#[cfg(test)]
mod tests;
