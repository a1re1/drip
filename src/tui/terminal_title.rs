//! Best-effort chat-title generation, part 1: settings glue and the pure
//! prompt/extract halves. The initial goal becomes a one-shot, tool-free
//! inference request whose reply is a 3-5 word pane title. Every failure mode
//! (missing profile or credential, offline restriction, timeout, malformed or
//! empty output) must resolve to `None` upstream so the caller keeps the
//! deterministic fallback label. Nothing here touches conversation history,
//! token accounting, or harness tools.

use crate::core::config::{
    default_setting_values, ACTIVE_INFERENCE_PROFILE_SETTING_ID,
    ACTIVE_TOOL_PROFILE_SETTING_ID,
    TERMINAL_TITLE_ENABLED_SETTING_ID, TERMINAL_TITLE_PROFILE_SETTING_ID,
};
use crate::core::inference::{resolve_inference_config, EnvSource};
use crate::harness::model_call::{
    create_model_caller, ModelCallOptions, ModelCallerDeps, ModelRoute,
    OpenAICompatibleResponse,
};
use crate::harness::transport::{TransportContent, TransportRequestMessage};
use crate::tui::pane_title::{bound_words, sanitize};
use std::sync::Arc;

/// One request, bounded prompt: the goal is trimmed hard before it is ever
/// sent anywhere.
pub const TITLE_PROMPT_GOAL_LIMIT: usize = 800;

/// The fallback label used when no title can be generated (kept in sync with
/// `pane_title::FALLBACK_LABEL`).
pub const TITLE_FALLBACK: &str = "drip";

/// The prompt sent to the title model. Pure and deterministic: only the
/// sanitized, bounded initial goal shapes it.
pub fn build_title_prompt(goal: &str) -> String {
    let goal = bound_words(&sanitize(goal), 40, TITLE_PROMPT_GOAL_LIMIT);
    format!(
        "Write a terse terminal pane title (3 to 5 words) for a coding-agent chat with this initial goal. \
         Reply with the title only - no quotes, no markup, no trailing period.\n\nGoal: {goal}"
    )
}

/// Extracts and sanitizes the model's title reply. `None` keeps the fallback.
/// Content may arrive as a plain string or as an array of `{text}` parts.
pub fn extract_title(response: &OpenAICompatibleResponse) -> Option<String> {
    let message = response.choices.as_ref()?.first()?.message.as_ref()?;
    let content = message.content.as_ref()?;
    let text = match content {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(parts) => {
            let mut assembled = String::new();
            for part in parts {
                if let serde_json::Value::Object(part) = part {
                    if let Some(serde_json::Value::String(text)) = part.get("text") {
                        assembled.push_str(text);
                    }
                }
            }
            assembled
        }
        _ => return None,
    };
    let title = bound_words(&sanitize(&text), 5, 48);
    if title.is_empty() || title.eq_ignore_ascii_case(TITLE_FALLBACK) {
        None
    } else {
        Some(title)
    }
}

/// Whether terminal-title generation is enabled. Defaults to true; only an
/// explicit "false" opts out.
pub fn terminal_title_enabled(settings: &indexmap::IndexMap<String, String>) -> bool {
    settings
        .get(TERMINAL_TITLE_ENABLED_SETTING_ID)
        .map(|value| value.trim())
        .unwrap_or("true")
        != "false"
}

/// The configured title profile id, defaulting to the shipped fast preset
/// (glm-5-3-flash).
pub fn terminal_title_profile_id(settings: &indexmap::IndexMap<String, String>) -> String {
    settings
        .get(TERMINAL_TITLE_PROFILE_SETTING_ID)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| {
            default_setting_values()
                .get(TERMINAL_TITLE_PROFILE_SETTING_ID)
                .cloned()
                .unwrap_or_default()
        })
}


/// The configured wall-clock cap for the one-shot title request, in
/// milliseconds; non-numeric or sub-millisecond values fall back to the
/// default.
pub fn terminal_title_timeout_ms(settings: &indexmap::IndexMap<String, String>) -> u64 {
    settings
        .get(crate::core::config::TERMINAL_TITLE_TIMEOUT_MS_SETTING_ID)
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .unwrap_or(DEFAULT_TITLE_TIMEOUT_MS)
}

/// Wall-clock cap for the one-shot title request.
pub const DEFAULT_TITLE_TIMEOUT_MS: u64 = 8_000;

/// Resolves the model route for title generation from the configured profile.
/// Returns `None` when the feature is disabled or the profile cannot be
/// resolved (unknown profile, missing credential, malformed settings) — the
/// caller silently keeps the deterministic fallback label. Title resolution
/// pins the *title* profile onto a cloned settings map, so the active chat's
/// inference and tool profiles are never touched.
pub fn resolve_title_route(
    settings: &indexmap::IndexMap<String, String>,
    env: EnvSource<'_>,
) -> Option<ModelRoute> {
    if !terminal_title_enabled(settings) {
        return None;
    }
    let mut scoped = settings.clone();
    scoped.insert(
        ACTIVE_INFERENCE_PROFILE_SETTING_ID.to_string(),
        terminal_title_profile_id(settings),
    );
    // The title call is tool-free; an empty tool profile keeps its resolution
    // from interfering with the route we need.
    scoped.insert(ACTIVE_TOOL_PROFILE_SETTING_ID.to_string(), String::new());
    match resolve_inference_config(&scoped, env) {
        Ok(resolved) => Some(resolved.route.to_model_route()),
        Err(_) => None,
    }
}

/// The single inference request per chat: a tool-free call against the title
/// route, hard-bounded in time, whose sanitized reply becomes the pane label.
/// Every failure (offline restriction, timeout, malformed or empty reply) is
/// `None` so the caller keeps the fallback; this never fails the chat and
/// never invokes harness tools.
pub async fn generate_chat_title(
    route: ModelRoute,
    goal: &str,
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
        content: Some(TransportContent::Text(build_title_prompt(goal))),
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
        Ok(Ok(response)) => extract_title(&response),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::core::config::MODEL_PROFILES_SETTING_ID;

    fn response_with_content(content: &str) -> OpenAICompatibleResponse {
        // JSON-escape control characters so raw ESC/BEL fixtures stay valid JSON.
        let escaped = content
            .replace('\u{1b}', "\\u001b")
            .replace('\u{7}', "\\u0007");
        serde_json::from_str(&format!(
            r#"{{"choices":[{{"message":{{"content":"{escaped}"}}}}]}}"#
        ))
        .unwrap()
    }

    #[test]
    fn title_prompt_contains_the_goal_and_the_word_contract() {
        let prompt = build_title_prompt("fix the flaky websocket reconnect handshake");
        assert!(prompt.contains("fix the flaky websocket reconnect handshake"));
        assert!(prompt.contains("3 to 5 words"));
    }

    #[test]
    fn title_prompt_bounds_very_long_goals() {
        let long_goal = "word ".repeat(400);
        let bounded = build_title_prompt(&long_goal);
        assert!(bounded.len() < TITLE_PROMPT_GOAL_LIMIT + 300);
    }

    #[test]
    fn title_prompt_strips_control_characters_from_the_goal() {
        let prompt = build_title_prompt("fix \u{1b}]2;injected\u{7} the bug");
        assert!(!prompt.contains('\u{1b}'));
        assert!(!prompt.contains('\u{7}'));
    }

    #[test]
    fn extract_title_sanitizes_and_bounds_model_output() {
        // The OSC payload ("evil") is consumed whole with its terminator, so
        // injected sequence bodies cannot leak into the title either.
        let response = response_with_content(
            "  Fix \u{1b}]2;evil\u{7} websocket reconnect \\\"handshake\\\"  ",
        );
        let title = extract_title(&response).unwrap();
        assert_eq!(title, "Fix websocket reconnect handshake");
    }

    #[test]
    fn extract_title_rejects_empty_malformed_and_fallback_replies() {
        assert!(extract_title(&response_with_content("   ")).is_none());
        assert!(extract_title(&response_with_content("drip")).is_none());
        let missing: OpenAICompatibleResponse =
            serde_json::from_str(r#"{"choices":[]}"#).unwrap();
        assert!(extract_title(&missing).is_none());
        let broken: OpenAICompatibleResponse =
            serde_json::from_str(r#"{"choices":[{"message":{"content":123}}]}"#).unwrap();
        assert!(extract_title(&broken).is_none());
    }

    #[test]
    fn default_settings_ship_terminal_title_values() {
        let defaults = default_setting_values();
        assert_eq!(
            defaults.get(TERMINAL_TITLE_ENABLED_SETTING_ID).map(String::as_str),
            Some("true")
        );
        // The shipped default profile is the fast preset.
        assert_eq!(
            defaults.get(TERMINAL_TITLE_PROFILE_SETTING_ID).map(String::as_str),
            Some("glm-5-3-flash")
        );
    }

    #[test]
    fn terminal_title_settings_override_defaults() {
        let mut settings = default_setting_values();
        settings.insert(TERMINAL_TITLE_ENABLED_SETTING_ID.to_string(), "false".to_string());
        settings.insert(TERMINAL_TITLE_PROFILE_SETTING_ID.to_string(), "kimi-k3".to_string());
        assert!(!terminal_title_enabled(&settings));
        assert_eq!(terminal_title_profile_id(&settings), "kimi-k3");
    }

    #[test]
    fn terminal_title_enabled_defaults_true_when_key_missing() {
        let settings: indexmap::IndexMap<String, String> = indexmap::IndexMap::new();
        assert!(terminal_title_enabled(&settings));
        assert_eq!(terminal_title_profile_id(&settings), "glm-5-3-flash");
    }

    #[test]
    fn resolve_title_route_resolves_the_configured_profile() {
        let mut settings = default_setting_values();
        let profiles = r#"[{"id":"mock","label":"Mock","model":"m","provider":"openai-compatible","baseUrl":"http://127.0.0.1:9/v1/","apiKeyRef":"env:MOCK_KEY"}]"#;
        settings.insert(MODEL_PROFILES_SETTING_ID.to_string(), profiles.to_string());
        settings.insert(ACTIVE_INFERENCE_PROFILE_SETTING_ID.to_string(), "mock".to_string());
        // Override the title profile onto the mock list (exercises the
        // terminal_title_profile_id override path end to end).
        settings.insert(TERMINAL_TITLE_PROFILE_SETTING_ID.to_string(), "mock".to_string());
        let mut env = std::collections::HashMap::new();
        env.insert("MOCK_KEY".to_string(), "k".to_string());
        let route = resolve_title_route(&settings, Some(&env)).expect("route resolves");
        assert_eq!(route.model, "m");
        assert!(route.url.starts_with("http://127.0.0.1:9"));
    }

    #[test]
    fn resolve_title_route_is_none_when_disabled() {
        let mut settings = default_setting_values();
        settings.insert(TERMINAL_TITLE_ENABLED_SETTING_ID.to_string(), "false".to_string());
        let env = std::collections::HashMap::new();
        assert!(resolve_title_route(&settings, Some(&env)).is_none());
    }

    #[tokio::test]
    async fn generate_chat_title_success_round_trip() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 2048];
            let _ = std::io::Read::read(&mut stream, &mut buf);
            let body = r#"{"choices":[{"message":{"content":"Fix the login race"}}]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            std::io::Write::write_all(&mut stream, response.as_bytes()).unwrap();
        });
        let mut settings = default_setting_values();
        let profiles = format!(
            r#"[{{"id":"mock","label":"Mock","model":"m","provider":"openai-compatible","baseUrl":"http://{addr}/v1/","apiKeyRef":"env:MOCK_KEY"}}]"#
        );
        settings.insert(MODEL_PROFILES_SETTING_ID.to_string(), profiles);
        settings.insert(ACTIVE_INFERENCE_PROFILE_SETTING_ID.to_string(), "mock".to_string());
        // Override the title profile onto the mock list (exercises the
        // terminal_title_profile_id override path end to end).
        settings.insert(TERMINAL_TITLE_PROFILE_SETTING_ID.to_string(), "mock".to_string());
        let mut env = std::collections::HashMap::new();
        env.insert("MOCK_KEY".to_string(), "k".to_string());
        let route = resolve_title_route(&settings, Some(&env)).expect("route resolves");
        let title = generate_chat_title(route, "fix the login race", 10_000).await;
        assert_eq!(title.as_deref(), Some("Fix the login race"));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn generate_chat_title_timeout_returns_none() {
        // A listener that never answers: the request can connect but the
        // bounded call must give up and fall back to None.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut settings = default_setting_values();
        let profiles = format!(
            r#"[{{"id":"mock","label":"Mock","model":"m","provider":"openai-compatible","baseUrl":"http://{addr}/v1/","apiKeyRef":"env:MOCK_KEY"}}]"#
        );
        settings.insert(MODEL_PROFILES_SETTING_ID.to_string(), profiles);
        settings.insert(ACTIVE_INFERENCE_PROFILE_SETTING_ID.to_string(), "mock".to_string());
        // Override the title profile onto the mock list (exercises the
        // terminal_title_profile_id override path end to end).
        settings.insert(TERMINAL_TITLE_PROFILE_SETTING_ID.to_string(), "mock".to_string());
        let mut env = std::collections::HashMap::new();
        env.insert("MOCK_KEY".to_string(), "k".to_string());
        let route = resolve_title_route(&settings, Some(&env)).expect("route resolves");
        let title = generate_chat_title(route, "a slow goal", 200).await;
        assert!(title.is_none());
    }

    #[test]
    fn terminal_title_timeout_ms_parses_and_falls_back() {
        let mut settings = default_setting_values();
        assert_eq!(terminal_title_timeout_ms(&settings), DEFAULT_TITLE_TIMEOUT_MS);
        settings.insert(
            crate::core::config::TERMINAL_TITLE_TIMEOUT_MS_SETTING_ID.to_string(),
            "1234".to_string(),
        );
        assert_eq!(terminal_title_timeout_ms(&settings), 1234);
        for bad in ["nope", "0", "-5"] {
            settings.insert(
                crate::core::config::TERMINAL_TITLE_TIMEOUT_MS_SETTING_ID.to_string(),
                bad.to_string(),
            );
            assert_eq!(terminal_title_timeout_ms(&settings), DEFAULT_TITLE_TIMEOUT_MS);
        }
    }
}
