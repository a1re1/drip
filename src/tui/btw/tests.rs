//! Offline unit tests for the `/btw` sidebar: transcript digest, sidebar
//! framing, the multi-turn thread, and reply extraction. No network: the only
//! async half (`ask_btw`) needs a live route and is exercised through the
//! extracted pure pieces.

use super::*;
use crate::cli::transcript::{
    TranscriptEventEntry, TranscriptGoalEntry, TranscriptModelEntry, TranscriptNoteEntry,
    TranscriptRunEndEntry,
};
use crate::core::types::{HarnessEventData, HarnessRunReason};
use serde_json::json;

fn goal(text: &str) -> TranscriptEntry {
    TranscriptEntry::Goal(TranscriptGoalEntry {
        at: "2026-01-01T00:00:00.000Z".to_string(),
        goal_id: "g1".to_string(),
        images: Vec::new(),
        mentions: Vec::new(),
        text: text.to_string(),
    })
}

fn event(kind: HarnessEventType, detail: &str, tool: Option<&str>) -> TranscriptEntry {
    TranscriptEntry::Event(TranscriptEventEntry {
        at: "2026-01-01T00:00:01.000Z".to_string(),
        data: tool.map(|name| HarnessEventData {
            tool_name: Some(name.to_string()),
            ..Default::default()
        }),
        detail: detail.to_string(),
        goal_id: "g1".to_string(),
        iteration: 1,
        kind,
    })
}

fn model_entry() -> TranscriptEntry {
    TranscriptEntry::Model(TranscriptModelEntry {
        at: "2026-01-01T00:00:00.000Z".to_string(),
        goal_id: "g1".to_string(),
        model: "glm-5-2".to_string(),
        profile_id: "glm-5-2".to_string(),
        provider: "openrouter".to_string(),
        reasoning_effort: None,
        roles: None,
        tool_model: None,
        tool_profile_id: None,
        tool_reasoning_effort: None,
    })
}

fn run_end(reason: HarnessRunReason) -> TranscriptEntry {
    TranscriptEntry::RunEnd(TranscriptRunEndEntry {
        at: "2026-01-01T00:10:00.000Z".to_string(),
        goal_id: "g1".to_string(),
        iterations: 7,
        reason,
    })
}

fn response(content: &str) -> OpenAICompatibleResponse {
    // Built through json! rather than string interpolation: the reply text can
    // contain newlines and control characters, which a raw JSON string literal
    // would reject.
    serde_json::from_value(json!({
        "choices": [{ "message": { "content": content } }]
    }))
    .unwrap()
}

#[test]
fn digest_carries_goal_model_text_tools_and_run_end() {
    let entries = vec![
        goal("fix the flaky reconnect"),
        model_entry(),
        event(HarnessEventType::ToolCall, "PATCH", Some("PATCH")),
        event(
            HarnessEventType::ModelText,
            "I fixed the backoff and ran the tests.",
            None,
        ),
        TranscriptEntry::Info(TranscriptNoteEntry {
            at: "2026-01-01T00:05:00.000Z".to_string(),
            text: "pane title updated".to_string(),
        }),
        run_end(HarnessRunReason::Completed),
    ];
    let digest = build_btw_digest(
        &entries,
        "sess-1",
        "/tmp/session/transcript.jsonl",
        false,
        Some("fix the flaky reconnect"),
        BTW_DIGEST_CHARS,
    );

    assert!(digest.contains("session id: sess-1"));
    assert!(digest.contains("/tmp/session/transcript.jsonl"));
    assert!(digest.contains("no run in flight"));
    assert!(digest.contains("user goal: fix the flaky reconnect"));
    assert!(digest.contains("model route: glm-5-2"));
    assert!(digest.contains("tool call: PATCH"));
    assert!(digest.contains("assistant: I fixed the backoff"));
    assert!(digest.contains("notice: pane title updated"));
    assert!(digest.contains("run end: Completed after 7 iteration(s)"));
}

#[test]
fn digest_marks_a_live_run_and_skips_the_sidebars_own_lines() {
    let entries = vec![
        goal("wire the sidebar"),
        TranscriptEntry::Info(TranscriptNoteEntry {
            at: "2026-01-01T00:01:00.000Z".to_string(),
            text: "btw · what is it doing?".to_string(),
        }),
        TranscriptEntry::Info(TranscriptNoteEntry {
            at: "2026-01-01T00:01:01.000Z".to_string(),
            text: "btw | it is writing the digest".to_string(),
        }),
    ];
    let digest = build_btw_digest(&entries, "sess-2", "/t", true, None, BTW_DIGEST_CHARS);
    assert!(digest.contains("a run is in flight right now"));
    assert!(digest.contains("user goal: wire the sidebar"));
    assert!(!digest.contains("it is writing the digest"));
}

#[test]
fn digest_keeps_the_header_and_the_newest_tail_under_the_budget() {
    let mut entries = vec![goal("a long session")];
    for index in 0..200 {
        entries.push(event(
            HarnessEventType::ModelText,
            &format!("reply number {index} with some words in it"),
            None,
        ));
    }
    let digest = build_btw_digest(
        &entries,
        "sess-3",
        "/tmp/transcript.jsonl",
        false,
        None,
        2_000,
    );
    assert!(digest.chars().count() <= 2_000 + 200, "{}", digest.len());
    assert!(digest.starts_with("session id: sess-3"));
    assert!(digest.contains("/tmp/transcript.jsonl"));
    assert!(
        digest.contains("reply number 199"),
        "newest history must survive the budget"
    );
    assert!(
        !digest.contains("reply number 0 "),
        "oldest history is dropped"
    );
}

#[test]
fn system_prompt_frames_a_sidebar_and_points_at_the_transcript() {
    let digest = build_btw_digest(
        &[goal("ship it")],
        "sess-4",
        "/x/t.jsonl",
        true,
        None,
        4_000,
    );
    let prompt = build_btw_system_prompt(&digest);
    assert!(prompt.contains("sidebar"));
    assert!(prompt.contains("NOT the agent"));
    assert!(prompt.contains("run summary"));
    assert!(prompt.contains("full transcript file: /x/t.jsonl"));
    assert!(prompt.contains("user goal: ship it"));
}

#[test]
fn messages_replay_the_thread_then_the_new_question() {
    let mut thread = BtwThread::new();
    thread.push_user("what changed?");
    thread.push_assistant("Two files.");
    let messages = build_btw_messages("SYSTEM", thread.turns(), "and why?");

    assert_eq!(messages.len(), 4);
    assert_eq!(messages[0].role, ChatRoleTag::System);
    assert_eq!(messages[1].role, ChatRoleTag::User);
    assert_eq!(messages[2].role, ChatRoleTag::Assistant);
    assert_eq!(messages[3].role, ChatRoleTag::User);
    let text = |message: &TransportRequestMessage| match message.content.as_ref() {
        Some(TransportContent::Text(text)) => text.clone(),
        other => panic!("unexpected content: {other:?}"),
    };
    assert_eq!(text(&messages[0]), "SYSTEM");
    assert_eq!(text(&messages[1]), "what changed?");
    assert_eq!(text(&messages[2]), "Two files.");
    assert_eq!(text(&messages[3]), "and why?");
    assert_eq!(
        messages[0].tool_calls, None,
        "a sidebar call advertises no tools"
    );
}

#[test]
fn messages_cap_a_very_long_thread() {
    let mut thread = BtwThread::new();
    for index in 0..60 {
        thread.push_user(&format!("q{index}"));
        thread.push_assistant(&format!("a{index}"));
    }
    let messages = build_btw_messages("SYSTEM", thread.turns(), "last");
    assert_eq!(messages.len(), BTW_HISTORY_TURNS + 2);
    let tail = match messages[BTW_HISTORY_TURNS].content.as_ref() {
        Some(TransportContent::Text(text)) => text.clone(),
        other => panic!("unexpected content: {other:?}"),
    };
    assert_eq!(tail, "a59", "the cap keeps the newest turns");
}

#[test]
fn thread_counts_questions_and_resets() {
    let mut thread = BtwThread::new();
    assert!(thread.is_empty());
    assert_eq!(thread.ask_count(), 0);
    thread.push_user("one");
    thread.push_assistant("answer");
    thread.push_user("two");
    assert_eq!(thread.ask_count(), 2);
    assert_eq!(thread.len(), 3);
    let tail = thread.tail_lines(2);
    assert!(tail.iter().any(|line| line.contains("btw | you: two")));
    thread.reset();
    assert!(thread.is_empty());
    assert_eq!(thread.ask_count(), 0);
}

#[test]
fn reply_extraction_accepts_text_parts_and_refuses_empty_output() {
    assert_eq!(
        extract_btw_reply(&response("  It is on the review task.  ")).as_deref(),
        Some("It is on the review task.")
    );
    // Multi-line replies survive; control characters are stripped.
    let reply = extract_btw_reply(&response("line one\n\u{1b}[31mline two")).unwrap();
    assert!(reply.starts_with("line one\n"), "{reply:?}");
    assert!(reply.contains("line two"), "{reply:?}");
    assert!(!reply.contains('\u{1b}'), "{reply:?}");

    let parts: OpenAICompatibleResponse = serde_json::from_value(json!({
        "choices": [{ "message": { "content": [{ "type": "text", "text": "from parts" }] } }]
    }))
    .unwrap();
    assert_eq!(extract_btw_reply(&parts).as_deref(), Some("from parts"));

    assert_eq!(extract_btw_reply(&response("   \n  ")), None);
    let no_content: OpenAICompatibleResponse =
        serde_json::from_str(r#"{"choices":[{"message":{}}]}"#).unwrap();
    assert_eq!(extract_btw_reply(&no_content), None);
}

#[test]
fn reply_is_bounded() {
    let long = "word ".repeat(BTW_MAX_REPLY_CHARS);
    let reply = extract_btw_reply(&response(&long)).unwrap();
    assert_eq!(reply.chars().count(), BTW_MAX_REPLY_CHARS);
    assert!(reply.ends_with('…'));
}

#[test]
fn chat_lines_echo_the_question_and_prefix_every_reply_line() {
    let lines = btw_chat_lines("why slow?", "Because it is on PATCH.\nStill running.");
    assert_eq!(
        lines,
        vec![
            "btw · why slow?".to_string(),
            "btw | Because it is on PATCH.".to_string(),
            "btw | Still running.".to_string(),
        ]
    );
    // The digest skips these lines: a sidebar never eats its own answers.
    let entries: Vec<TranscriptEntry> = lines
        .iter()
        .map(|line| {
            TranscriptEntry::Info(TranscriptNoteEntry {
                at: String::new(),
                text: line.clone(),
            })
        })
        .collect();
    let digest = build_btw_digest(&entries, "s", "/t", false, None, 4_000);
    assert!(!digest.contains("Because it is on PATCH"));
}
