//! Offline unit tests for session naming: word contract, malformed/empty
//! model replies, prompt shape, transcript digest, and session.json
//! persistence round-trip. No network: the async error path uses an
//! unroutable localhost URL and a 1ms timeout.

use super::*;
use crate::cli::transcript::{TranscriptEntry, TranscriptGoalEntry, TranscriptModelEntry};

fn response_with_content(content: &str) -> OpenAICompatibleResponse {
    let escaped = content
        .replace('\u{1b}', "\\u001b")
        .replace('\u{7}', "\\u0007");
    serde_json::from_str(&format!(
        r#"{{"choices":[{{"message":{{"content":"{escaped}"}}}}]}}"#
    ))
    .unwrap()
}

fn response_with_parts(parts: &str) -> OpenAICompatibleResponse {
    serde_json::from_str(&format!(
        r#"{{"choices":[{{"message":{{"content":{parts}}}}}]}}"#
    ))
    .unwrap()
}

#[test]
fn accepts_five_to_seven_word_names() {
    let five = extract_session_name(&response_with_content(
        "Fix Flaky Websocket Reconnect Handshake",
    ));
    assert_eq!(
        five.as_deref(),
        Some("Fix Flaky Websocket Reconnect Handshake")
    );
    let seven = extract_session_name(&response_with_content(
        "Add Rename Command Wiring And Tests Today",
    ));
    assert_eq!(
        seven
            .as_deref()
            .unwrap_or_default()
            .split_whitespace()
            .count(),
        7
    );
}

#[test]
fn rejects_names_outside_the_word_contract() {
    assert_eq!(
        extract_session_name(&response_with_content("Fix Websocket Handshake")),
        None,
        "3 words is too short"
    );
    assert_eq!(
        extract_session_name(&response_with_content(
            "Ship Flaky Websocket Reconnect Handshake Now Please Quickly"
        ))
        .as_deref(),
        Some("Ship Flaky Websocket Reconnect Handshake Now Please"),
        "8-word reply trims to its first 7 whole words"
    );
    assert!(!is_valid_session_name(""));
    assert!(!is_valid_session_name("one two three"));
    assert!(!is_valid_session_name(
        crate::tui::pane_title::FALLBACK_LABEL
    ));
    assert!(is_valid_session_name("five whole words right here now"));
}

#[test]
fn rejects_malformed_and_empty_model_replies() {
    // Empty content, whitespace, and control characters.
    assert_eq!(extract_session_name(&response_with_content("")), None);
    assert_eq!(extract_session_name(&response_with_content("   ")), None);
    assert_eq!(
        extract_session_name(&response_with_content("\u{7}\u{1b}")),
        None
    );
    // Malformed JSON shapes: missing choices, missing message, non-string content.
    let empty: OpenAICompatibleResponse = serde_json::from_str(r#"{"choices":[]}"#).unwrap();
    assert_eq!(extract_session_name(&empty), None);
    let no_message: OpenAICompatibleResponse = serde_json::from_str(r#"{"choices":[{}]}"#).unwrap();
    assert_eq!(extract_session_name(&no_message), None);
    let numeric: OpenAICompatibleResponse =
        serde_json::from_str(r#"{"choices":[{"message":{"content":42}}]}"#).unwrap();
    assert_eq!(extract_session_name(&numeric), None);
}

#[test]
fn assembles_content_parts_and_strips_quotes() {
    let parts = response_with_parts(
        r#"[{"type":"text","text":"\"Fix "},{"type":"text","text":"Flaky Websocket Reconnect Handshake\""}]"#,
    );
    assert_eq!(
        extract_session_name(&parts).as_deref(),
        Some("Fix Flaky Websocket Reconnect Handshake")
    );
}

#[test]
fn prompt_carries_goal_digest_and_word_contract() {
    let prompt = build_session_name_prompt(
        "fix the flaky websocket handshake",
        "User: please start\nAssistant: (reply)",
    );
    assert!(prompt.contains("5 to 7 word"));
    assert!(prompt.contains("fix the flaky websocket handshake"));
    assert!(prompt.contains("Assistant: (reply)"));
}

#[test]
fn digest_prefers_goals_and_handles_empty_transcripts() {
    let goal = TranscriptGoalEntry {
        at: "t1".into(),
        goal_id: "g1".into(),
        images: Vec::new(),
        mentions: Vec::new(),
        text: "first goal".into(),
    };
    let entries = vec![
        TranscriptEntry::Goal(goal),
        TranscriptEntry::Model(TranscriptModelEntry {
            at: "t2".into(),
            goal_id: "g1".into(),
            model: "m".into(),
            profile_id: "p".into(),
            provider: "openai".into(),
            reasoning_effort: None,
            roles: None,
            tool_model: None,
            tool_profile_id: None,
            tool_reasoning_effort: None,
        }),
    ];
    let digest = read_session_name_context("current goal", &entries);
    assert!(digest.starts_with("User: current goal"));
    assert!(digest.contains("first goal"));
    // Empty transcript + empty goal degrades to an empty digest, not an error.
    assert_eq!(read_session_name_context("", &[]), "");
    assert_eq!(
        read_session_name_context("solo goal", &[]),
        "User: solo goal"
    );
}

fn temp_meta_path(tag: &str) -> std::path::PathBuf {
    let dir =
        std::env::temp_dir().join(format!("drip-session-name-{}-{}", std::process::id(), tag));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir.join("session.json")
}

#[test]
fn persistence_round_trip_preserves_other_fields() {
    let path = temp_meta_path("roundtrip");
    std::fs::write(
        &path,
        "{\n  \"createdAt\": \"2026-01-01T00:00:00Z\",\n  \"id\": \"abc\"\n}\n",
    )
    .unwrap();
    assert_eq!(read_session_name(&path), None, "no name yet");
    assert!(persist_session_name(
        &path,
        "Fix Flaky Websocket Handshake Today"
    ));
    assert_eq!(
        read_session_name(&path).as_deref(),
        Some("Fix Flaky Websocket Handshake Today")
    );
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(
        raw.contains("\"createdAt\""),
        "existing fields survive: {raw}"
    );
    assert!(raw.contains("\"id\": \"abc\""));
    // Re-persisting overwrites the name in place.
    assert!(persist_session_name(&path, "A Whole Fresh Name Here"));
    assert_eq!(
        read_session_name(&path).as_deref(),
        Some("A Whole Fresh Name Here")
    );
}

#[test]
fn persistence_rejects_malformed_metadata_and_creates_missing_files() {
    let path = temp_meta_path("malformed");
    std::fs::write(&path, "not json at all").unwrap();
    assert!(!persist_session_name(
        &path,
        "Fix Flaky Websocket Handshake Today"
    ));
    assert_eq!(
        read_session_name(&path),
        None,
        "malformed file yields no name"
    );
    // A non-object JSON document is rejected too.
    std::fs::write(&path, "[1, 2, 3]").unwrap();
    assert!(!persist_session_name(
        &path,
        "Fix Flaky Websocket Handshake Today"
    ));
    // Missing session.json: the write creates bare metadata and round-trips.
    let missing = temp_meta_path("missing");
    let _ = std::fs::remove_file(&missing);
    assert!(persist_session_name(
        &missing,
        "Fix Flaky Websocket Handshake Today"
    ));
    assert_eq!(
        read_session_name(&missing).as_deref(),
        Some("Fix Flaky Websocket Handshake Today")
    );
}

#[test]
fn async_error_paths_resolve_to_none() {
    let route = ModelRoute {
        fallback_route: None,
        headers: None,
        model: "test-model".into(),
        provider: Some("openai".into()),
        reasoning_effort: None,
        refresh_headers: None,
        url: "http://127.0.0.1:9/unreachable".into(),
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    // Unroutable endpoint (connection refused) with a 1ms cap.
    let refused = runtime.block_on(generate_session_name(
        route.clone(),
        "fix the flaky websocket handshake",
        "User: please start",
        1,
    ));
    assert_eq!(refused, None, "connection errors must not yield a name");
    // Empty transcript digest still forms a prompt but the endpoint is dead.
    let empty_digest = runtime.block_on(generate_session_name(route, "goal only", "", 1));
    assert_eq!(empty_digest, None);
}

#[test]
fn empty_goal_and_transcript_yields_none_without_a_model_call() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let route = ModelRoute {
        fallback_route: None,
        headers: None,
        model: "test-model".into(),
        provider: Some("openai".into()),
        reasoning_effort: None,
        refresh_headers: None,
        // Would refuse if a call were attempted; the guard must
        // short-circuit before any network activity.
        url: "http://127.0.0.1:9/unreachable".into(),
    };
    let both_empty = runtime.block_on(generate_session_name(route.clone(), "", "", 1));
    assert_eq!(
        both_empty, None,
        "empty goal + digest must not fabricate a name"
    );
    let whitespace_only = runtime.block_on(generate_session_name(route, "   \n\t ", "  ", 1));
    assert_eq!(
        whitespace_only, None,
        "whitespace-only context must not fabricate a name"
    );
}
/// Contract: credentials live on the route, and a text-only call never reads
/// the route object, so these headers must be copied onto the caller's deps.
#[test]
fn session_name_headers_copy_the_route_credential_and_keep_base_order() {
    let route = ModelRoute {
        fallback_route: None,
        headers: Some(vec![
            ("X-Title".into(), "lci".into()),
            ("Authorization".into(), "Bearer k".into()),
        ]),
        model: "m".into(),
        provider: Some("openai".into()),
        reasoning_effort: None,
        refresh_headers: None,
        url: "http://127.0.0.1:9/v1/chat/completions".into(),
    };
    let headers = session_name_headers(
        vec![("content-type".into(), "application/json".into())],
        &route,
    );
    assert_eq!(
        headers,
        vec![
            ("content-type".to_string(), "application/json".to_string()),
            ("X-Title".to_string(), "lci".to_string()),
            ("Authorization".to_string(), "Bearer k".to_string()),
        ]
    );
    // A base entry already present wins: no duplicate header is emitted.
    let overriding =
        session_name_headers(vec![("Authorization".into(), "Bearer base".into())], &route);
    assert_eq!(
        overriding
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("authorization"))
            .count(),
        1
    );
    assert_eq!(overriding[0].1, "Bearer base");
}

/// End to end: a mock endpoint that answers 401 to any request without the
/// route's Authorization header and 200 with a valid name once it is present.
/// This is the production /rename wire path (`include_tools: false`).
#[test]
fn generate_session_name_sends_the_route_authorization_header() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 4096];
        let read = std::io::Read::read(&mut stream, &mut buf).unwrap();
        let request = String::from_utf8_lossy(&buf[..read]).to_ascii_lowercase();
        let (status, body) = if request.contains("authorization: bearer test-key") {
            (
                "200 OK",
                r#"{"choices":[{"message":{"content":"Fix Flaky Websocket Handshake Today"}}]}"#,
            )
        } else {
            (
                "401 Unauthorized",
                r#"{"error":{"message":"No auth credentials found"}}"#,
            )
        };
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        std::io::Write::write_all(&mut stream, response.as_bytes()).unwrap();
    });
    let route = ModelRoute {
        fallback_route: None,
        headers: Some(vec![("Authorization".into(), "Bearer test-key".into())]),
        model: "test-model".into(),
        provider: Some("openai".into()),
        reasoning_effort: None,
        refresh_headers: None,
        url: format!("http://{addr}/v1/chat/completions"),
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let name = runtime.block_on(generate_session_name(
        route,
        "fix the flaky websocket handshake",
        "User: please start",
        5_000,
    ));
    assert_eq!(
        name.as_deref(),
        Some("Fix Flaky Websocket Handshake Today"),
        "the route credential must ride the request headers, not be dropped"
    );
    server.join().unwrap();
}
