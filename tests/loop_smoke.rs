// End-to-end smoke test for the loop driver: a scripted OpenAI-compatible
// endpoint plays plan_tasks → finish_task → (text-only) run summary, and the
// run must complete with the expected state shape and event sequence for
// that script.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use drip::core::types::{HarnessEvent, HarnessRunReason, HarnessTaskStatus};
use drip::harness::r#loop::{run_solid_state_harness, SolidStateHarnessOptions};
use drip::tools::async_jobs::{
    create_chat_tool_runtime_services, CreateChatToolRuntimeServicesOptions,
};

/// Serves one canned response per connection, in order.
fn spawn_scripted_server(
    responses: Vec<String>,
) -> (String, std::thread::JoinHandle<Vec<serde_json::Value>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    let handle = std::thread::spawn(move || {
        let mut bodies = Vec::new();
        for response_body in responses {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "scripted endpoint did not receive the expected call"
                        );
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                .unwrap();
            let mut data: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 8192];
            let body_start = loop {
                let read = stream.read(&mut chunk).expect("read scripted request");
                assert!(read > 0, "client closed early");
                data.extend_from_slice(&chunk[..read]);
                if let Some(pos) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&data[..pos]).to_ascii_lowercase();
                    let len = head
                        .lines()
                        .find_map(|l| {
                            l.strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if data.len() >= pos + 4 + len {
                        break pos + 4;
                    }
                }
            };
            bodies.push(
                serde_json::from_slice(&data[body_start..]).unwrap_or(serde_json::Value::Null),
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.flush().unwrap();
        }
        bodies
    });
    (
        format!("http://127.0.0.1:{port}/v1/chat/completions"),
        handle,
    )
}

fn tool_call_response(id: &str, name: &str, arguments: serde_json::Value) -> String {
    serde_json::json!({
        "choices": [{"message": {"content": null, "tool_calls": [{"id": id, "type": "function", "function": {"name": name, "arguments": arguments.to_string()}}]}}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5}
    })
    .to_string()
}

fn tool_calls_response(calls: Vec<(&str, &str, serde_json::Value)>) -> String {
    let tool_calls: Vec<serde_json::Value> = calls
        .into_iter()
        .map(|(id, name, arguments)| serde_json::json!({"id": id, "type": "function", "function": {"name": name, "arguments": arguments.to_string()}}))
        .collect();
    serde_json::json!({
        "choices": [{"message": {"content": null, "tool_calls": tool_calls}}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5}
    })
    .to_string()
}

fn narrated_tool_call_response(
    text: &str,
    id: &str,
    name: &str,
    arguments: serde_json::Value,
) -> String {
    serde_json::json!({
        "choices": [{"message": {"content": text, "tool_calls": [{"id": id, "type": "function", "function": {"name": name, "arguments": arguments.to_string()}}]}}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5}
    })
    .to_string()
}

fn text_response(text: &str) -> String {
    serde_json::json!({"choices": [{"message": {"content": text}}], "usage": {"prompt_tokens": 3, "completion_tokens": 2}}).to_string()
}

#[tokio::test]
async fn no_op_verification_cannot_clear_completion_even_on_repeat() {
    for (route, writer) in [("VERIFY", "PATCH"), ("BASH", "PATCH"), ("VERIFY", "WRITE")] {
        let dir = tempfile::tempdir().unwrap();
        let events = Arc::new(Mutex::new(Vec::<HarnessEvent>::new()));
        let sink = events.clone();
        let mut tools = drip::tools::pack::builtin_tool_pack(Default::default());
        tools
            .iter_mut()
            .find(|tool| tool.name == "PATCH")
            .unwrap()
            .name = writer.into();
        let assertion = "test \"$(cat artifact.txt)\" = correct && printf 'DRIP_VERIFY {\"executed\":1,\"passed\":1,\"failed\":0}\\n'";
        let (url, server) = spawn_scripted_server(vec![
            tool_call_response(
                "p",
                "plan_tasks",
                serde_json::json!({"tasks":["produce artifact"]}),
            ),
            tool_call_response(
                "w",
                writer,
                serde_json::json!({"path":"artifact.txt","content":"correct\n"}),
            ),
            tool_call_response("v", "VERIFY", serde_json::json!({"command":"true"})),
            tool_call_response(
                "f1",
                "finish_task",
                serde_json::json!({"status":"completed","summary":"done"}),
            ),
            tool_call_response(
                "f2",
                "finish_task",
                serde_json::json!({"status":"completed","summary":"done"}),
            ),
            tool_call_response("a", route, serde_json::json!({"command":assertion})),
            tool_call_response(
                "bad-write",
                "BASH",
                serde_json::json!({"command":"printf corrupt > artifact.txt; exit 9"}),
            ),
            tool_call_response(
                "stale",
                "finish_task",
                serde_json::json!({"status":"completed","summary":"earlier check passed"}),
            ),
            tool_call_response(
                "restore",
                writer,
                serde_json::json!({"path":"artifact.txt","content":"correct\n"}),
            ),
            tool_call_response("fresh", route, serde_json::json!({"command":assertion})),
            tool_call_response(
                "f3",
                "finish_task",
                serde_json::json!({"status":"completed","summary":"one assertion passed","confidence":"high","anchor":"none","anchorNote":"the assertion script is self-authored; no external fixture exists"}),
            ),
            text_response("Artifact verified."),
        ]);
        let result = run_solid_state_harness(SolidStateHarnessOptions {
            cwd: Some(dir.path().to_string_lossy().into()),
            goal: "produce a verified artifact".into(),
            max_iterations: Some(6),
            model: Some("mock".into()),
            summarize_run: Some(true),
            url: Some(url),
            tools,
            on_event: Some(Arc::new(move |event| sink.lock().unwrap().push(event))),
            state_path: Some(dir.path().join("state.json")),
            tool_services: Some(create_chat_tool_runtime_services(
                CreateChatToolRuntimeServicesOptions {
                    cwd: Some(dir.path().into()),
                    jobs_root: Some(dir.path().join("jobs")),
                },
            )),
            ..Default::default()
        })
        .await
        .unwrap();
        server.join().unwrap();
        assert_eq!(result.reason, HarnessRunReason::Completed, "{route}");
        let records = result.state.verifications.as_ref().unwrap();
        // v1 the no-op check, v2 the assertion, v3 the harness's own re-run of
        // the assertion after the corrupting write (it fails, so the stale
        // finish is refused with the failure instead of a bare bounce), v4
        // the fresh passing assertion.
        assert_eq!(records.len(), 4, "{route}");
        assert!(!records[0].evidence.as_ref().unwrap().verifies_work());
        assert!(records[1].evidence.as_ref().unwrap().verifies_work());
        assert_eq!(records[1].evidence.as_ref().unwrap().executed, 1);
        assert!(
            records[2].failed,
            "the harness re-run sees the corrupt artifact: {route}"
        );
        assert!(records[3].evidence.as_ref().unwrap().verifies_work());
        assert_eq!(
            events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| event.detail.contains("not accepted yet — this task edited"))
                .count(),
            2,
            "{route}"
        );
        assert_eq!(
            events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| event
                    .detail
                    .starts_with("harness re-ran the last check after workspace edits"))
                .count(),
            1,
            "{route}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("artifact.txt")).unwrap(),
            "correct\n"
        );
        let anchor = result
            .state
            .completion_anchor
            .as_ref()
            .expect("completion anchor recorded");
        assert_eq!(
            anchor.kind,
            drip::core::types::CompletionAnchorKind::None,
            "{route}"
        );
        assert_eq!(
            anchor.claimed_confidence,
            Some(drip::core::types::ClaimedConfidence::High),
            "{route}"
        );
    }
}

/// A finish that bounces only because an edit landed after the last check
/// is re-verified by the harness itself: the same assertion runs again,
/// passes, and the finish is accepted in the same round — no extra model
/// call, no bounce message for the model to act on.
#[tokio::test]
async fn stale_finish_is_reverified_by_the_harness_without_another_round() {
    let dir = tempfile::tempdir().unwrap();
    let events = Arc::new(Mutex::new(Vec::<HarnessEvent>::new()));
    let sink = events.clone();
    let tools = drip::tools::pack::builtin_tool_pack(Default::default());
    let assertion = "test \"$(cat artifact.txt)\" = correct && printf 'DRIP_VERIFY {\"executed\":1,\"passed\":1,\"failed\":0}\\n'";
    let (url, server) = spawn_scripted_server(vec![
        tool_call_response(
            "p",
            "plan_tasks",
            serde_json::json!({"tasks":["produce artifact"]}),
        ),
        tool_call_response(
            "w",
            "PATCH",
            serde_json::json!({"path":"artifact.txt","content":"draft\n"}),
        ),
        tool_call_response("a", "VERIFY", serde_json::json!({"command":"true"})),
        // Edit after the check, then finish: stale, so the harness re-runs
        // the assertion itself and accepts the finish in this round.
        tool_call_response(
            "w2",
            "PATCH",
            serde_json::json!({"path":"artifact.txt","content":"correct\n"}),
        ),
        tool_call_response("a2", "VERIFY", serde_json::json!({"command":assertion})),
        tool_call_response(
            "w3",
            "PATCH",
            serde_json::json!({"path":"artifact.txt","content":"correct\n"}),
        ),
        tool_call_response(
            "f",
            "finish_task",
            serde_json::json!({"status":"completed","summary":"done","anchor":"none","anchorNote":"self-authored assertion only"}),
        ),
        text_response("Artifact verified."),
    ]);
    let result = run_solid_state_harness(SolidStateHarnessOptions {
        cwd: Some(dir.path().to_string_lossy().into()),
        goal: "produce a verified artifact".into(),
        max_iterations: Some(6),
        model: Some("mock".into()),
        summarize_run: Some(true),
        url: Some(url),
        tools,
        on_event: Some(Arc::new(move |event| sink.lock().unwrap().push(event))),
        state_path: Some(dir.path().join("state.json")),
        tool_services: Some(create_chat_tool_runtime_services(
            CreateChatToolRuntimeServicesOptions {
                cwd: Some(dir.path().into()),
                jobs_root: Some(dir.path().join("jobs")),
            },
        )),
        ..Default::default()
    })
    .await
    .unwrap();
    server.join().unwrap();
    assert_eq!(
        result.reason,
        HarnessRunReason::Completed,
        "{:?}",
        result.error_message
    );
    let records = result.state.verifications.as_ref().unwrap();
    assert_eq!(
        records.len(),
        3,
        "true, the assertion, and the harness re-run of the assertion"
    );
    assert!(!records[2].failed);
    assert_eq!(records[2].command, records[1].command);
    let events = events.lock().unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.detail.contains("not accepted yet"))
            .count(),
        0,
        "no bounce reached the model"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event
                .detail
                .starts_with("harness re-ran the last check after workspace edits"))
            .count(),
        1
    );
    assert!(
        events.iter().any(|event| event
            .detail
            .starts_with("finish_task: harness re-ran the last check after your edits")),
        "{:?}",
        events
            .iter()
            .map(|e| e.detail.clone())
            .filter(|d| d.starts_with("finish_task"))
            .collect::<Vec<_>>()
    );
}

/// A finish with no check run at all: the harness runs the check the goal
/// declares in backticks on the model's behalf, records it as an external
/// (task-provided) anchor, and accepts the finish in the same round.
#[tokio::test]
async fn unchecked_finish_runs_the_goal_declared_check() {
    let dir = tempfile::tempdir().unwrap();
    let events = Arc::new(Mutex::new(Vec::<HarnessEvent>::new()));
    let sink = events.clone();
    let tools = drip::tools::pack::builtin_tool_pack(Default::default());
    let (url, server) = spawn_scripted_server(vec![
        tool_call_response(
            "p",
            "plan_tasks",
            serde_json::json!({"tasks":["add the test"]}),
        ),
        tool_call_response(
            "w",
            "PATCH",
            serde_json::json!({"path":"tests/test_ok.py","content":"import unittest\n\nclass T(unittest.TestCase):\n    def test_ok(self):\n        self.assertEqual(1 + 1, 2)\n"}),
        ),
        tool_call_response(
            "f",
            "finish_task",
            serde_json::json!({"status":"completed","summary":"added the test","anchor":"none","anchorNote":"nothing ran"}),
        ),
        text_response("Test added."),
    ]);
    let result = run_solid_state_harness(SolidStateHarnessOptions {
        cwd: Some(dir.path().to_string_lossy().into()),
        goal: "Add tests/test_ok.py. Acceptance: `python3 -m unittest discover -s tests -q` must pass.".into(),
        plan_mode: Some("always".into()),
        max_iterations: Some(6), model: Some("mock".into()), summarize_run: Some(true), url: Some(url),
        tools,
        on_event: Some(Arc::new(move |event| sink.lock().unwrap().push(event))),
        state_path: Some(dir.path().join("state.json")),
        tool_services: Some(create_chat_tool_runtime_services(CreateChatToolRuntimeServicesOptions {
            cwd: Some(dir.path().into()), jobs_root: Some(dir.path().join("jobs")),
        })),
        ..Default::default()
    }).await.unwrap();
    server.join().unwrap();
    assert_eq!(
        result.reason,
        HarnessRunReason::Completed,
        "{:?}",
        result.error_message
    );
    let records = result.state.verifications.as_ref().unwrap();
    assert_eq!(records.len(), 1, "only the harness-run goal check");
    assert!(!records[0].failed);
    assert_eq!(
        records[0].command,
        "python3 -m unittest discover -s tests -q"
    );
    let anchor = records[0]
        .evidence
        .as_ref()
        .unwrap()
        .anchor
        .clone()
        .expect("anchor recorded");
    assert_eq!(
        anchor.kind,
        drip::core::types::VerificationAnchorKind::External
    );
    let events = events.lock().unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.detail.contains("not accepted yet"))
            .count(),
        0,
        "no bounce reached the model"
    );
    // The check ran when the PATCH landed (run_edit_check); the finish that
    // followed without further edits was accepted on that record.
    assert_eq!(
        events
            .iter()
            .filter(|event| event
                .detail
                .starts_with("harness ran the goal-declared check after this round's edits"))
            .count(),
        1,
        "{:?}",
        events
            .iter()
            .map(|e| e.detail.clone())
            .filter(|d| d.starts_with("harness ran"))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event
                .detail
                .starts_with("harness ran the goal-declared check for the finish"))
            .count(),
        0
    );
}

/// A completion report instead of finish_task, on a workspace the edit check
/// already verified: accepted as the finish in that round, no re-seeded loop.
#[tokio::test]
async fn verified_narration_is_accepted_as_the_finish() {
    let dir = tempfile::tempdir().unwrap();
    let events = Arc::new(Mutex::new(Vec::<HarnessEvent>::new()));
    let sink = events.clone();
    let tools = drip::tools::pack::builtin_tool_pack(Default::default());
    let (url, server) = spawn_scripted_server(vec![
        tool_call_response("w", "PATCH", serde_json::json!({"path":"tests/test_ok.py","content":"import unittest\n\nclass T(unittest.TestCase):\n    def test_ok(self):\n        self.assertEqual(1 + 1, 2)\n"})),
        text_response("The test file tests/test_ok.py is in place and the unittest suite passes with one test executed."),
        text_response("(should not be reached)"),
    ]);
    let result = run_solid_state_harness(SolidStateHarnessOptions {
        cwd: Some(dir.path().to_string_lossy().into()),
        goal: "Add tests/test_ok.py. Acceptance: `python3 -m unittest discover -s tests -q` must pass.".into(),
        plan_mode: Some("never".into()),
        max_iterations: Some(6), model: Some("mock".into()), summarize_run: Some(true), url: Some(url),
        tools,
        on_event: Some(Arc::new(move |event| sink.lock().unwrap().push(event))),
        state_path: Some(dir.path().join("state.json")),
        tool_services: Some(create_chat_tool_runtime_services(CreateChatToolRuntimeServicesOptions {
            cwd: Some(dir.path().into()), jobs_root: Some(dir.path().join("jobs")),
        })),
        ..Default::default()
    }).await.unwrap();
    server.join().unwrap();
    assert_eq!(
        result.reason,
        HarnessRunReason::Completed,
        "{:?}",
        result.error_message
    );
    let events = events.lock().unwrap();
    let details: Vec<String> = events.iter().map(|e| e.detail.clone()).collect();
    assert_eq!(
        details
            .iter()
            .filter(|d| d.starts_with("narration accepted as finish_task"))
            .count(),
        1,
        "{details:?}"
    );
    assert!(
        details
            .iter()
            .any(|d| d.starts_with("finish_task:") && d.contains("marked completed")),
        "{details:?}"
    );
    assert_eq!(
        details.iter().filter(|d| d.starts_with("loop ")).count(),
        1,
        "the task must not be re-seeded in a second loop: {details:?}"
    );
    assert_eq!(
        result
            .state
            .tasks
            .iter()
            .filter(|t| t.status == drip::core::types::HarnessTaskStatus::Completed)
            .count(),
        1
    );
}

/// A completion report sent in the same response as the edits: the goal's
/// check passes right after the PATCH, the goal-named file was touched, and
/// the text reads as done, so the harness dispatches the finish in that
/// round instead of waiting for a lone finish_task round.
#[tokio::test]
async fn completion_report_sent_with_the_edits_is_accepted_as_the_finish() {
    let dir = tempfile::tempdir().unwrap();
    let events = Arc::new(Mutex::new(Vec::<HarnessEvent>::new()));
    let sink = events.clone();
    let tools = drip::tools::pack::builtin_tool_pack(Default::default());
    let (url, server) = spawn_scripted_server(vec![
        narrated_tool_call_response(
            "The test file tests/test_ok.py is in place with one passing test; the task is complete.",
            "w",
            "PATCH",
            serde_json::json!({"path":"tests/test_ok.py","content":"import unittest\n\nclass T(unittest.TestCase):\n    def test_ok(self):\n        self.assertEqual(1 + 1, 2)\n"}),
        ),
        text_response("(should not be reached)"),
    ]);
    let result = run_solid_state_harness(SolidStateHarnessOptions {
        cwd: Some(dir.path().to_string_lossy().into()),
        goal: "Add tests/test_ok.py. Acceptance: `python3 -m unittest discover -s tests -q` must pass.".into(),
        plan_mode: Some("never".into()),
        max_iterations: Some(6), model: Some("mock".into()), summarize_run: Some(true), url: Some(url),
        tools,
        on_event: Some(Arc::new(move |event| sink.lock().unwrap().push(event))),
        state_path: Some(dir.path().join("state.json")),
        tool_services: Some(create_chat_tool_runtime_services(CreateChatToolRuntimeServicesOptions {
            cwd: Some(dir.path().into()), jobs_root: Some(dir.path().join("jobs")),
        })),
        ..Default::default()
    }).await.unwrap();
    server.join().unwrap();
    assert_eq!(
        result.reason,
        HarnessRunReason::Completed,
        "{:?}",
        result.error_message
    );
    let events = events.lock().unwrap();
    let details: Vec<String> = events.iter().map(|e| e.detail.clone()).collect();
    assert_eq!(
        details
            .iter()
            .filter(|d| d.starts_with("narration accepted as finish_task")
                && d.contains("sent with this round's edits"))
            .count(),
        1,
        "{details:?}"
    );
    assert!(
        details
            .iter()
            .any(|d| d.starts_with("finish_task:") && d.contains("marked completed")),
        "{details:?}"
    );
    assert_eq!(
        details.iter().filter(|d| d.starts_with("loop ")).count(),
        1,
        "{details:?}"
    );
    assert_eq!(
        result
            .state
            .tasks
            .iter()
            .filter(|t| t.status == drip::core::types::HarnessTaskStatus::Completed)
            .count(),
        1
    );
}

/// Two PATCH calls in one response: the check runs once, after the last of
/// them, and its verdict rides on that PATCH's result. A recorded bench run
/// whose first PATCH created a source file and whose second created its test
/// must not be measured between the two.
#[tokio::test]
async fn edit_check_runs_once_after_the_last_patch_of_a_response() {
    let dir = tempfile::tempdir().unwrap();
    let events = Arc::new(Mutex::new(Vec::<HarnessEvent>::new()));
    let sink = events.clone();
    let tools = drip::tools::pack::builtin_tool_pack(Default::default());
    let (url, server) = spawn_scripted_server(vec![
        tool_calls_response(vec![
            (
                "w1",
                "PATCH",
                serde_json::json!({"path":"pkg/__init__.py","content":"def add(a, b):\n    return a + b\n"}),
            ),
            (
                "w2",
                "PATCH",
                serde_json::json!({"path":"tests/test_ok.py","content":"import unittest\nfrom pkg import add\n\nclass T(unittest.TestCase):\n    def test_ok(self):\n        self.assertEqual(add(1, 1), 2)\n"}),
            ),
        ]),
        tool_call_response(
            "f",
            "finish_task",
            serde_json::json!({"status":"completed","summary":"added add and its test","anchor":"none","anchorNote":"nothing ran"}),
        ),
        text_response("Done."),
    ]);
    let result = run_solid_state_harness(SolidStateHarnessOptions {
        cwd: Some(dir.path().to_string_lossy().into()),
        goal: "Add pkg/__init__.py with add() and tests/test_ok.py. Acceptance: `python3 -m unittest discover -s tests -q` must pass.".into(),
        plan_mode: Some("never".into()),
        max_iterations: Some(6), model: Some("mock".into()), summarize_run: Some(true), url: Some(url),
        tools,
        on_event: Some(Arc::new(move |event| sink.lock().unwrap().push(event))),
        state_path: Some(dir.path().join("state.json")),
        tool_services: Some(create_chat_tool_runtime_services(CreateChatToolRuntimeServicesOptions {
            cwd: Some(dir.path().into()), jobs_root: Some(dir.path().join("jobs")),
        })),
        ..Default::default()
    }).await.unwrap();
    server.join().unwrap();
    assert_eq!(
        result.reason,
        HarnessRunReason::Completed,
        "{:?}",
        result.error_message
    );
    let events = events.lock().unwrap();
    let details: Vec<String> = events.iter().map(|e| e.detail.clone()).collect();
    // Tool-call events are emitted after the call runs, so order them by
    // which PATCH result carries the check's trailer instead.
    let checks: Vec<&String> = details
        .iter()
        .filter(|d| d.starts_with("harness ran the goal-declared check after this round's edits"))
        .collect();
    assert_eq!(checks.len(), 1, "{details:?}");
    assert!(checks[0].contains("-> passed"), "{}", checks[0]);
    // The test file exists only after the second PATCH: a check that ran
    // between the two would have found no tests at all.
    let records = result.state.verifications.as_ref().unwrap();
    assert_eq!(records.len(), 1, "one harness-run check");
    assert_eq!(
        records[0].evidence.as_ref().unwrap().executed,
        1,
        "the check ran before tests/test_ok.py existed: {:?}",
        records[0]
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event
                .detail
                .starts_with("harness ran the goal-declared check for the finish"))
            .count(),
        0
    );
}

/// A stale finish whose last check cannot be re-run (the record keeps only a
/// 200-char truncation) falls back to the goal-declared check.
#[tokio::test]
async fn stale_finish_with_unrerunnable_check_falls_back_to_the_goal_check() {
    let dir = tempfile::tempdir().unwrap();
    let events = Arc::new(Mutex::new(Vec::<HarnessEvent>::new()));
    let sink = events.clone();
    let tools = drip::tools::pack::builtin_tool_pack(Default::default());
    let long_check = format!("python3 - <<'EOF'\n# {}\nprint('ok')\nEOF", "x".repeat(220));
    let (url, server) = spawn_scripted_server(vec![
        tool_call_response(
            "p",
            "plan_tasks",
            serde_json::json!({"tasks":["add the test"]}),
        ),
        tool_call_response(
            "w",
            "PATCH",
            serde_json::json!({"path":"tests/test_ok.py","content":"import unittest\n\nclass T(unittest.TestCase):\n    def test_ok(self):\n        self.assertEqual(1 + 1, 2)\n"}),
        ),
        tool_call_response("v", "VERIFY", serde_json::json!({"command":long_check})),
        tool_call_response(
            "w2",
            "PATCH",
            serde_json::json!({"path":"tests/test_ok.py","content":"import unittest\n\nclass T(unittest.TestCase):\n    def test_ok(self):\n        self.assertEqual(2 + 2, 4)\n"}),
        ),
        tool_call_response(
            "f",
            "finish_task",
            serde_json::json!({"status":"completed","summary":"added the test","anchor":"none","anchorNote":"self-authored probe only"}),
        ),
        text_response("Test added."),
    ]);
    let result = run_solid_state_harness(SolidStateHarnessOptions {
        cwd: Some(dir.path().to_string_lossy().into()),
        goal: "Add tests/test_ok.py. Acceptance: `python3 -m unittest discover -s tests -q` must pass.".into(),
        plan_mode: Some("always".into()),
        max_iterations: Some(6), model: Some("mock".into()), summarize_run: Some(true), url: Some(url),
        tools,
        on_event: Some(Arc::new(move |event| sink.lock().unwrap().push(event))),
        state_path: Some(dir.path().join("state.json")),
        tool_services: Some(create_chat_tool_runtime_services(CreateChatToolRuntimeServicesOptions {
            cwd: Some(dir.path().into()), jobs_root: Some(dir.path().join("jobs")),
        })),
        ..Default::default()
    }).await.unwrap();
    server.join().unwrap();
    assert_eq!(
        result.reason,
        HarnessRunReason::Completed,
        "{:?}",
        result.error_message
    );
    let records = result.state.verifications.as_ref().unwrap();
    // Each PATCH lands the goal check (run_edit_check) around the long probe;
    // the finish is accepted on the last of them without a re-run.
    assert_eq!(
        records.len(),
        3,
        "goal check, the long probe, goal check: {:?}",
        records
            .iter()
            .map(|r| r.command.clone())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        records[2].command,
        "python3 -m unittest discover -s tests -q"
    );
    let events = events.lock().unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.detail.contains("not accepted yet"))
            .count(),
        0,
        "no bounce reached the model"
    );
}

/// A task that keeps editing but never calls finish_task is bounded by the
/// task loop budget: the prompt warns on the last loop and the harness
/// blocks the task when that loop ends without finish_task.
#[tokio::test]
async fn task_loop_budget_blocks_a_task_that_never_finishes() {
    let dir = tempfile::tempdir().unwrap();
    let events = Arc::new(Mutex::new(Vec::<HarnessEvent>::new()));
    let sink = events.clone();
    let (url, server) = spawn_scripted_server(vec![
        tool_call_response(
            "p",
            "plan_tasks",
            serde_json::json!({"tasks":["never done"]}),
        ),
        tool_call_response(
            "w1",
            "PATCH",
            serde_json::json!({"path":"a.txt","content":"one\n"}),
        ),
        text_response("still going"),
        tool_call_response(
            "w2",
            "PATCH",
            serde_json::json!({"path":"a.txt","content":"two\n"}),
        ),
        text_response("still going"),
        text_response("Budget spent."),
    ]);
    let result = run_solid_state_harness(SolidStateHarnessOptions {
        cwd: Some(dir.path().to_string_lossy().into()),
        goal: "never finish".into(),
        max_iterations: Some(10),
        max_loops: Some(3),
        task_loop_limit: Some(2),
        model: Some("mock".into()),
        summarize_run: Some(true),
        url: Some(url),
        r#loop: Some(drip::harness::roles::PartialHarnessLoopConfig {
            max_cycles: Some(1),
            ..Default::default()
        }),
        tools: drip::tools::pack::builtin_tool_pack(Default::default()),
        on_event: Some(Arc::new(move |event| sink.lock().unwrap().push(event))),
        state_path: Some(dir.path().join("state.json")),
        tool_services: Some(create_chat_tool_runtime_services(
            CreateChatToolRuntimeServicesOptions {
                cwd: Some(dir.path().into()),
                jobs_root: Some(dir.path().join("jobs")),
            },
        )),
        ..Default::default()
    })
    .await
    .unwrap();
    let bodies = server.join().unwrap();
    let task = &result.state.tasks[0];
    assert_eq!(task.loops_run, Some(2));
    assert_eq!(
        task.status,
        drip::core::types::HarnessTaskStatus::Blocked,
        "{:?}",
        task.summary
    );
    assert!(task
        .summary
        .as_deref()
        .unwrap_or_default()
        .contains("budget 2"));
    let events = events.lock().unwrap();
    assert!(events.iter().any(|event| event
        .detail
        .contains("auto-blocked after 2 task loops (task loop budget 2)")));
    let texts: Vec<String> = bodies.iter().map(|b| b.to_string()).collect();
    assert!(
        texts
            .iter()
            .any(|t| t.contains("task loop budget: this is task loop 2 of 2")),
        "last-loop warning reaches the model"
    );
}

/// A cycle that edited the workspace earns the loop one more cycle instead
/// of a transcript reset: with a one-cycle, one-round budget the PATCH in
/// cycle 1 extends the loop, and finish_task lands in cycle 2 of the SAME
/// loop.
#[tokio::test]
async fn a_cycle_that_edits_extends_the_loop_by_one_cycle() {
    let dir = tempfile::tempdir().unwrap();
    let events = Arc::new(Mutex::new(Vec::<HarnessEvent>::new()));
    let sink = events.clone();
    let (url, server) = spawn_scripted_server(vec![
        tool_call_response(
            "p",
            "plan_tasks",
            serde_json::json!({"tasks":["add the test"]}),
        ),
        tool_call_response(
            "w",
            "PATCH",
            serde_json::json!({"path":"tests/test_ok.py","content":"import unittest\n\nclass T(unittest.TestCase):\n    def test_ok(self):\n        self.assertEqual(1 + 1, 2)\n"}),
        ),
        tool_call_response(
            "f",
            "finish_task",
            serde_json::json!({"status":"completed","summary":"added the test","anchor":"none","anchorNote":"nothing ran"}),
        ),
        text_response("Test added."),
    ]);
    let result = run_solid_state_harness(SolidStateHarnessOptions {
        cwd: Some(dir.path().to_string_lossy().into()),
        goal: "Add tests/test_ok.py. Acceptance: `python3 -m unittest discover -s tests -q` must pass.".into(),
        plan_mode: Some("always".into()),
        max_iterations: Some(8), model: Some("mock".into()), summarize_run: Some(true), url: Some(url),
        r#loop: Some(drip::harness::roles::PartialHarnessLoopConfig { max_cycles: Some(1), max_tool_rounds_per_cycle: Some(1), ..Default::default() }),
        tools: drip::tools::pack::builtin_tool_pack(Default::default()),
        on_event: Some(Arc::new(move |event| sink.lock().unwrap().push(event))),
        state_path: Some(dir.path().join("state.json")),
        tool_services: Some(create_chat_tool_runtime_services(CreateChatToolRuntimeServicesOptions {
            cwd: Some(dir.path().into()), jobs_root: Some(dir.path().join("jobs")),
        })),
        ..Default::default()
    }).await.unwrap();
    server.join().unwrap();
    assert_eq!(
        result.reason,
        HarnessRunReason::Completed,
        "{:?}",
        result.error_message
    );
    assert_eq!(result.r#loops, 2, "planning loop + one extended task loop");
    let events = events.lock().unwrap();
    assert!(
        events
            .iter()
            .any(|event| event.detail.starts_with("cycle budget extended to 2")),
        "{:?}",
        events
            .iter()
            .map(|e| e.detail.clone())
            .filter(|d| d.contains("cycle"))
            .collect::<Vec<_>>()
    );
}

/// Plan mode auto: a small goal that declares its own check gets one direct
/// task and no planner loop — the first model call is already the author's.
#[tokio::test]
async fn plan_mode_auto_seeds_a_direct_task_for_a_small_goal() {
    let dir = tempfile::tempdir().unwrap();
    let events = Arc::new(Mutex::new(Vec::<HarnessEvent>::new()));
    let sink = events.clone();
    let (url, server) = spawn_scripted_server(vec![
        tool_call_response(
            "w",
            "PATCH",
            serde_json::json!({"path":"tests/test_ok.py","content":"import unittest\n\nclass T(unittest.TestCase):\n    def test_ok(self):\n        self.assertEqual(1 + 1, 2)\n"}),
        ),
        tool_call_response(
            "f",
            "finish_task",
            serde_json::json!({"status":"completed","summary":"added the test","anchor":"none","anchorNote":"nothing ran"}),
        ),
        text_response("Test added."),
    ]);
    let result = run_solid_state_harness(SolidStateHarnessOptions {
        cwd: Some(dir.path().to_string_lossy().into()),
        goal: "Add tests/test_ok.py with one passing test. Acceptance: `python3 -m unittest discover -s tests -q` must pass.".into(),
        plan_mode: Some("auto".into()),
        max_iterations: Some(6), model: Some("mock".into()), summarize_run: Some(true), url: Some(url),
        tools: drip::tools::pack::builtin_tool_pack(Default::default()),
        on_event: Some(Arc::new(move |event| sink.lock().unwrap().push(event))),
        state_path: Some(dir.path().join("state.json")),
        tool_services: Some(create_chat_tool_runtime_services(CreateChatToolRuntimeServicesOptions {
            cwd: Some(dir.path().into()), jobs_root: Some(dir.path().join("jobs")),
        })),
        ..Default::default()
    }).await.unwrap();
    let bodies = server.join().unwrap();
    assert_eq!(
        result.reason,
        HarnessRunReason::Completed,
        "{:?}",
        result.error_message
    );
    assert_eq!(result.r#loops, 1, "no planning loop");
    assert_eq!(result.state.tasks.len(), 1);
    assert!(result.state.tasks[0]
        .notes
        .iter()
        .any(|note| note.starts_with("direct task: the planner was skipped")));
    let events = events.lock().unwrap();
    assert!(events.iter().any(|event| event
        .detail
        .starts_with("direct task seeded, planner skipped (plan mode Auto)")));
    assert!(
        bodies[0].to_string().contains("tests/test_ok.py"),
        "the first call already carries the task"
    );
}

/// A declared anomaly whose own observed text reports success (exit 0, 0
/// failures) does not end the run unreconciled: it becomes a task note and
/// the run completes. Two real sessions had ended unreconciled on green work.
#[tokio::test]
async fn self_passing_anomalies_do_not_end_the_run_unreconciled() {
    let dir = tempfile::tempdir().unwrap();
    let events = Arc::new(Mutex::new(Vec::<HarnessEvent>::new()));
    let sink = events.clone();
    let assertion = "test \"$(cat artifact.txt)\" = correct && printf 'DRIP_VERIFY {\"executed\":1,\"passed\":1,\"failed\":0}\\n'";
    let (url, server) = spawn_scripted_server(vec![
        tool_call_response(
            "p",
            "plan_tasks",
            serde_json::json!({"tasks":["produce artifact"], "expectations":[{"subject":"suite result","expected":"all pass"}]}),
        ),
        tool_call_response(
            "w",
            "PATCH",
            serde_json::json!({"path":"artifact.txt","content":"correct\n"}),
        ),
        tool_call_response(
            "v",
            "VERIFY",
            serde_json::json!({"command":assertion,"anchor":{"kind":"external","source":"artifact fixture"}}),
        ),
        tool_call_response(
            "f",
            "finish_task",
            serde_json::json!({
                "status":"unreconciled","summary":"done","confidence":"high","anchor":"none","anchorNote":"fixture",
                "observations":[{"subject":"suite result","observed":"exit 0; 83 pass / 0 fail","matches":false}],
                "anomalies":[{"subject":"suite result","expected":"all pass","observed":"exit 0; 83 pass / 0 fail","note":"test count drifted from the baseline"}]
            }),
        ),
        text_response("Artifact produced."),
    ]);
    let result = run_solid_state_harness(SolidStateHarnessOptions {
        cwd: Some(dir.path().to_string_lossy().into()),
        goal: "produce a measured artifact".into(),
        plan_mode: Some("always".into()),
        max_iterations: Some(6),
        model: Some("mock".into()),
        summarize_run: Some(true),
        url: Some(url),
        tools: drip::tools::pack::builtin_tool_pack(Default::default()),
        on_event: Some(Arc::new(move |event| sink.lock().unwrap().push(event))),
        state_path: Some(dir.path().join("state.json")),
        tool_services: Some(create_chat_tool_runtime_services(
            CreateChatToolRuntimeServicesOptions {
                cwd: Some(dir.path().into()),
                jobs_root: Some(dir.path().join("jobs")),
            },
        )),
        ..Default::default()
    })
    .await
    .unwrap();
    server.join().unwrap();
    assert_eq!(
        result.reason,
        HarnessRunReason::Completed,
        "{:?}",
        result.error_message
    );
    assert!(result.state.anomalies.is_empty());
    assert!(
        result.state.tasks[0]
            .notes
            .iter()
            .any(|note| note.starts_with("non-blocking anomaly")),
        "{:?}",
        result.state.tasks[0].notes
    );
    let events = events.lock().unwrap();
    assert!(events
        .iter()
        .any(|event| event.detail.contains("did not block completion")));
}

/// The expectation gate end to end: an "external" anchor on a check that
/// names the edited artifact is downgraded, a mismatched observation refuses
/// `completed`, and `unreconciled` finishes the run with exit 0, the anomaly
/// in the payload, and the claimed confidence persisted on the task.
#[tokio::test]
async fn mismatched_expectation_ends_unreconciled_with_exit_zero() {
    let dir = tempfile::tempdir().unwrap();
    let events = Arc::new(Mutex::new(Vec::<HarnessEvent>::new()));
    let sink = events.clone();
    let assertion = "test \"$(cat artifact.txt)\" = correct && printf 'DRIP_VERIFY {\"executed\":1,\"passed\":1,\"failed\":0}\\n'";
    let (url, server) = spawn_scripted_server(vec![
        tool_call_response(
            "p",
            "plan_tasks",
            serde_json::json!({
                "tasks":["produce artifact"],
                "expectations":[{"subject":"artifact sign","expected":"positive"}]
            }),
        ),
        tool_call_response(
            "w",
            "PATCH",
            serde_json::json!({"path":"artifact.txt","content":"correct\n"}),
        ),
        tool_call_response(
            "v",
            "VERIFY",
            serde_json::json!({"command":assertion,"anchor":{"kind":"external","source":"artifact.txt fixture"}}),
        ),
        tool_call_response(
            "f1",
            "finish_task",
            serde_json::json!({
                "status":"completed","summary":"done","confidence":"high","anchor":"none","anchorNote":"no external fixture",
                "observations":[{"subject":"artifact sign","observed":"negative","matches":false}]
            }),
        ),
        tool_call_response(
            "f2",
            "finish_task",
            serde_json::json!({
                "status":"unreconciled","summary":"done","confidence":"low","anchor":"none","anchorNote":"no external fixture",
                "observations":[{"subject":"artifact sign","observed":"negative","matches":false}],
                "anomalies":[{"subject":"artifact sign","expected":"positive","observed":"negative","note":"cannot reconcile"}]
            }),
        ),
        text_response("Artifact produced; sign unreconciled."),
    ]);
    let state_path = dir.path().join("session").join("state.json");
    let result = run_solid_state_harness(SolidStateHarnessOptions {
        cwd: Some(dir.path().to_string_lossy().into()),
        goal: "produce a measured artifact".into(),
        max_iterations: Some(6),
        model: Some("mock".into()),
        summarize_run: Some(true),
        url: Some(url),
        tools: drip::tools::pack::builtin_tool_pack(Default::default()),
        on_event: Some(Arc::new(move |event| sink.lock().unwrap().push(event))),
        state_path: Some(state_path.clone()),
        tool_services: Some(create_chat_tool_runtime_services(
            CreateChatToolRuntimeServicesOptions {
                cwd: Some(dir.path().into()),
                jobs_root: Some(dir.path().join("jobs")),
            },
        )),
        ..Default::default()
    })
    .await
    .unwrap();
    server.join().unwrap();

    assert_eq!(
        result.reason,
        HarnessRunReason::Unreconciled,
        "error: {:?}",
        result.error_message
    );
    assert_eq!(result.state.anomalies.len(), 1);
    // Both valid measurements persist, including the refused clean finish.
    assert_eq!(result.state.expectations[0].observations.len(), 2);
    let anchor = result.state.verifications.as_ref().unwrap()[0]
        .evidence
        .as_ref()
        .unwrap()
        .anchor
        .clone()
        .expect("anchor recorded");
    assert_eq!(
        anchor.kind,
        drip::core::types::VerificationAnchorKind::SelfAuthored,
        "names the edited artifact"
    );
    assert!(anchor
        .downgraded_reason
        .as_deref()
        .unwrap_or_default()
        .contains("artifact.txt"));
    let events = events.lock().unwrap();
    assert!(events
        .iter()
        .any(|event| event.detail.contains("anchor downgraded to self-authored")));
    assert!(events
        .iter()
        .any(|event| event.detail.contains("P1 against the model")));

    let record =
        drip::cli::run_record::build_run_record(&drip::cli::run_record::BuildRunRecordArgs {
            ended_at: "2026-01-01T00:00:00.000Z",
            goal: "produce a measured artifact",
            goal_id: "g1",
            max_iterations: Some(6),
            max_loops: None,
            pending_operator_messages: 0,
            plan_mode: None,
            result: &result,
        });
    assert_eq!(record.reason, "unreconciled");
    assert_eq!(
        record.anomalies.as_ref().map(|anomalies| anomalies.len()),
        Some(1)
    );
    let finished = result
        .state
        .tasks
        .iter()
        .find(|task| task.id == "task-1")
        .expect("finished task in state");
    assert_eq!(
        finished.status,
        drip::core::types::HarnessTaskStatus::Completed
    );
    assert_eq!(
        finished.confidence,
        Some(drip::core::types::ClaimedConfidence::Low)
    );
    let payload = drip::cli::headless_output::headless_result_payload(
        drip::cli::headless_output::HeadlessResultArgs {
            record: &record,
            result_path: "/r",
            session_id: "s",
            session_id_prefix: "s",
            state_path: "/s",
            transcript_path: "/t",
        },
    );
    assert_eq!(payload.exit_code, 0);
    assert!(payload.continue_command.is_none());

    let state_value = serde_json::to_value(&result.state).expect("state serializes");
    let tasks_json = state_value["tasks"]
        .as_array()
        .expect("tasks array in serialized state");
    let f2 = tasks_json
        .iter()
        .find(|task| task["id"] == "task-1")
        .expect("finished task serialized");
    assert_eq!(f2["confidence"], serde_json::json!("low"), "{state_value}");
}

#[tokio::test]
async fn plan_finish_summary_completes_the_run() {
    let temp_dir = tempfile::tempdir().unwrap();
    let temp = temp_dir.path().to_path_buf();
    let state_path = temp.join("state.json");

    let (url, server) = spawn_scripted_server(vec![
        // Loop 1 (planning): plan_tasks, then a text turn ends the loop.
        tool_call_response(
            "call-1",
            "plan_tasks",
            serde_json::json!({"tasks": ["work on the file"]}),
        ),
        // Loop 2 (task-1): finish_task ends the loop and completes the goal.
        tool_call_response(
            "call-2",
            "finish_task",
            serde_json::json!({"status": "completed", "summary": "wrote it"}),
        ),
        // Run summary (text-only call).
        text_response("All done: the file was written."),
    ]);

    let events: Arc<Mutex<Vec<HarnessEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let options = SolidStateHarnessOptions {
        cwd: Some(temp.to_string_lossy().to_string()),
        goal: "write the file".to_string(),
        max_iterations: Some(4),
        model: Some("mock".to_string()),
        summarize_run: Some(true),
        on_event: Some(Arc::new(move |event| sink.lock().unwrap().push(event))),
        state_path: Some(state_path.clone()),
        tool_services: Some(create_chat_tool_runtime_services(
            CreateChatToolRuntimeServicesOptions {
                cwd: Some(temp.clone()),
                jobs_root: Some(temp.join("jobs")),
            },
        )),
        url: Some(url),
        // Keep latency memory off the operator's real ~/.drip/latency.json:
        // a non-empty store emits a harness-op seed event before loop-start,
        // which would break the event-kind assertion below.
        latency_store: Some(temp.join("latency.json")),
        ..Default::default()
    };

    let result = run_solid_state_harness(options).await.expect("run starts");
    let bodies = server.join().unwrap();

    let kinds_debug: Vec<String> = events
        .lock()
        .unwrap()
        .iter()
        .map(|e| {
            format!(
                "{:?}: {}",
                e.r#type,
                e.detail.chars().take(160).collect::<String>()
            )
        })
        .collect();
    assert_eq!(
        result.reason,
        HarnessRunReason::Completed,
        "error: {:?}\nevents: {:#?}",
        result.error_message,
        kinds_debug
    );
    assert_eq!(result.iterations, 2);
    assert_eq!(result.r#loops, 2);
    assert_eq!(result.state.tasks.len(), 1);
    assert_eq!(result.state.tasks[0].status, HarnessTaskStatus::Completed);
    assert_eq!(result.state.tasks[0].summary.as_deref(), Some("wrote it"));
    assert_eq!(
        result.state.run_summary.as_ref().map(|s| s.text.as_str()),
        Some("All done: the file was written.")
    );
    assert_eq!(result.usage.calls, 3);
    assert!(state_path.exists(), "state persisted");

    // Requests: planning call advertises tools; the planning loop yields as
    // soon as plan_tasks lands (no narration round); the summary call sends
    // no tools.
    assert_eq!(bodies.len(), 3);
    assert!(bodies[0]["tools"].as_array().map_or(false, |t| t
        .iter()
        .any(|t| t["function"]["name"] == "plan_tasks")));
    assert!(
        bodies[2]["tools"].is_null()
            || bodies[2]["tools"].as_array().map_or(true, |t| t.is_empty())
    );
    // Loop 2 opens on its own fresh snapshot (a harness-only planning
    // exchange carries nothing over).
    assert_eq!(bodies[1]["messages"][0]["role"], "system");
    assert_eq!(bodies[0]["messages"][0]["role"], "system");
    assert!(bodies[0]["messages"][1]["content"]
        .as_str()
        .unwrap()
        .contains("goal: write the file"));

    let kinds: Vec<String> = events
        .lock()
        .unwrap()
        .iter()
        .map(|event| {
            serde_json::to_value(&event.r#type)
                .unwrap()
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    let expected = [
        "loop-start",
        "iteration-start",
        "inference",
        "harness-op",
        "loop-start",
        "iteration-start",
        "inference",
        "task-finished",
        // The summary call reports usage too, so it emits its own inference event.
        "inference",
        "run-summary",
        "run-complete",
    ];
    assert_eq!(kinds, expected, "event kinds");
}

fn role(name: &str) -> drip::harness::roles::HarnessRoleRuntime {
    drip::harness::roles::HarnessRoleRuntime {
        description: None,
        r#loop: None,
        name: name.to_string(),
        route: None,
        system_prompt_suffix: None,
        tool_names: None,
        mcp_servers: None,
        verified_by: None,
        blind: false,
    }
}

/// --max-loops bounds task loops directly: two loops of a two-task plan end
/// the run as max-loops (not max-iterations, which is unset) with the
/// remaining task still pending.
#[tokio::test]
async fn max_loops_ends_the_run_after_that_many_task_loops() {
    let temp_dir = tempfile::tempdir().unwrap();
    let temp = temp_dir.path().to_path_buf();
    let (url, server) = spawn_scripted_server(vec![
        tool_call_response(
            "p",
            "plan_tasks",
            serde_json::json!({"tasks": ["first", "second"]}),
        ),
        tool_call_response(
            "b",
            "finish_task",
            serde_json::json!({"status": "blocked", "summary": "stuck", "confidence": "low"}),
        ),
        text_response("Loop budget spent."),
    ]);
    let result = run_solid_state_harness(SolidStateHarnessOptions {
        cwd: Some(temp.to_string_lossy().to_string()),
        goal: "do two things".to_string(),
        max_loops: Some(2),
        model: Some("mock".to_string()),
        summarize_run: Some(true),
        state_path: Some(temp.join("state.json")),
        tool_services: Some(create_chat_tool_runtime_services(
            CreateChatToolRuntimeServicesOptions {
                cwd: Some(temp.clone()),
                jobs_root: Some(temp.join("jobs")),
            },
        )),
        url: Some(url),
        ..Default::default()
    })
    .await
    .expect("run starts");
    server.join().unwrap();

    assert_eq!(
        result.reason,
        HarnessRunReason::MaxLoops,
        "{:?}",
        result.error_message
    );
    assert_eq!(result.r#loops, 2);
    assert_eq!(result.state.tasks[1].status, HarnessTaskStatus::Pending);
    assert_eq!(
        result.usage.calls, 3,
        "no third loop was started (plan, blocked finish, summary)"
    );
}

/// A task blocked on operator input is a terminal state: with nothing else
/// workable the run ends blocked-on-input instead of replanning around the
/// gap, resuming without a reply ends the same way at zero cost, and resuming
/// with a reply reopens the task with the reply on its notes.
#[tokio::test]
async fn blocked_on_operator_input_ends_the_run_until_a_reply_arrives() {
    let temp_dir = tempfile::tempdir().unwrap();
    let temp = temp_dir.path().to_path_buf();
    let state_path = temp.join("state.json");
    let services = || {
        create_chat_tool_runtime_services(CreateChatToolRuntimeServicesOptions {
            cwd: Some(temp.clone()),
            jobs_root: Some(temp.join("jobs")),
        })
    };
    let events: Arc<Mutex<Vec<HarnessEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();

    let (url, server) = spawn_scripted_server(vec![
        tool_call_response(
            "p",
            "plan_tasks",
            serde_json::json!({"tasks": ["restore the chunks"]}),
        ),
        tool_call_response(
            "b",
            "finish_task",
            serde_json::json!({
                "status": "blocked", "blockedOn": "operator", "confidence": "low",
                "summary": "need the original copies of chunks 3 and 7"
            }),
        ),
        text_response("Blocked on the operator."),
    ]);
    let result = run_solid_state_harness(SolidStateHarnessOptions {
        cwd: Some(temp.to_string_lossy().to_string()),
        goal: "restore the data".to_string(),
        model: Some("mock".to_string()),
        summarize_run: Some(true),
        on_event: Some(Arc::new(move |event| sink.lock().unwrap().push(event))),
        state_path: Some(state_path.clone()),
        tool_services: Some(services()),
        url: Some(url.clone()),
        ..Default::default()
    })
    .await
    .expect("run starts");
    server.join().unwrap();

    assert_eq!(
        result.reason,
        HarnessRunReason::BlockedOnInput,
        "{:?}",
        result.error_message
    );
    assert_eq!(result.r#loops, 2, "no replanning loop ran after the block");
    let task = &result.state.tasks[0];
    assert_eq!(task.status, HarnessTaskStatus::Blocked);
    assert_eq!(
        task.blocked_on,
        Some(drip::core::types::HarnessTaskBlocker::Operator)
    );
    assert!(events
        .lock()
        .unwrap()
        .iter()
        .any(|event| event.detail.contains("Blocked on operator input")));

    // Resume with the same goal and no reply: nothing to do, no model call.
    let (url_idle, server_idle) = spawn_scripted_server(vec![]);
    let idle = run_solid_state_harness(SolidStateHarnessOptions {
        cwd: Some(temp.to_string_lossy().to_string()),
        goal: "restore the data".to_string(),
        initial_state: Some(result.state.clone()),
        model: Some("mock".to_string()),
        summarize_run: Some(true),
        state_path: Some(state_path.clone()),
        tool_services: Some(services()),
        url: Some(url_idle),
        ..Default::default()
    })
    .await
    .expect("idle resume starts");
    server_idle.join().unwrap();
    assert_eq!(idle.reason, HarnessRunReason::BlockedOnInput);
    assert_eq!(idle.r#loops, 0);
    assert_eq!(idle.usage.calls, 0);

    // Resume with a reply (what prepare_state_for_goal records for a new
    // prompt against an unfinished ledger): the task reopens and completes.
    let mut answered = result.state.clone();
    answered.operator_messages = Some(vec![drip::core::types::HarnessOperatorMessage {
        id: format!("resume-{}", answered.iteration),
        received_at_iteration: answered.iteration,
        text: "the originals are in /backup/chunks".to_string(),
    }]);
    let (url_reply, server_reply) = spawn_scripted_server(vec![
        tool_call_response(
            "f",
            "finish_task",
            serde_json::json!({
                "status": "completed", "confidence": "high", "anchor": "none",
                "anchorNote": "restored from the operator's backup; no external fixture exists",
                "summary": "restored from /backup/chunks"
            }),
        ),
        text_response("Restored."),
    ]);
    let resumed = run_solid_state_harness(SolidStateHarnessOptions {
        cwd: Some(temp.to_string_lossy().to_string()),
        goal: "restore the data".to_string(),
        initial_state: Some(answered),
        model: Some("mock".to_string()),
        summarize_run: Some(true),
        state_path: Some(state_path),
        tool_services: Some(services()),
        url: Some(url_reply),
        ..Default::default()
    })
    .await
    .expect("reply resume starts");
    server_reply.join().unwrap();
    assert_eq!(
        resumed.reason,
        HarnessRunReason::Completed,
        "{:?}",
        resumed.error_message
    );
    let task = &resumed.state.tasks[0];
    assert_eq!(task.status, HarnessTaskStatus::Completed);
    assert_eq!(task.blocked_on, None);
    assert!(
        task.notes
            .iter()
            .any(|note| note
                .contains("Operator input received: the originals are in /backup/chunks")),
        "{:?}",
        task.notes
    );
}

/// Replanning runs under the replanning binding; a replanning loop that
/// leaves the ledger unworkable (notes only) escalates the next one to the
/// planning role, and one that resolves the ledger completes the run.
#[tokio::test]
async fn replanning_uses_the_cheap_role_and_escalates_when_it_gets_nowhere() {
    let temp_dir = tempfile::tempdir().unwrap();
    let temp = temp_dir.path().to_path_buf();
    let events: Arc<Mutex<Vec<HarnessEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let (url, server) = spawn_scripted_server(vec![
        // Loop 1 (planning → planner).
        tool_call_response(
            "p",
            "plan_tasks",
            serde_json::json!({"tasks": ["fix the thing"]}),
        ),
        // Loop 2 (task-1 → author) blocks.
        tool_call_response(
            "b",
            "finish_task",
            serde_json::json!({"status": "blocked", "summary": "stuck", "confidence": "low"}),
        ),
        // Loop 3 (replanning → replanner) only takes notes: no workable ledger.
        tool_call_response(
            "o",
            "observe",
            serde_json::json!({"note": "still thinking"}),
        ),
        text_response("nothing to add"),
        // Loop 4 (replanning → planner, escalated) resolves the block by id.
        tool_call_response(
            "f",
            "finish_task",
            serde_json::json!({
                "taskId": "task-1", "status": "completed", "confidence": "high", "anchor": "none",
                "anchorNote": "the earlier block was spurious; no external fixture exists", "summary": "already done"
            }),
        ),
        text_response("Done."),
    ]);
    let result = run_solid_state_harness(SolidStateHarnessOptions {
        cwd: Some(temp.to_string_lossy().to_string()),
        goal: "fix the thing".to_string(),
        model: Some("mock".to_string()),
        summarize_run: Some(true),
        on_event: Some(Arc::new(move |event| sink.lock().unwrap().push(event))),
        role_bindings: Some(drip::harness::roles::HarnessRoleBindings {
            planning: Some("planner".to_string()),
            replanning: Some("replanner".to_string()),
            task: Some("author".to_string()),
        }),
        roles: Some(vec![role("planner"), role("replanner"), role("author")]),
        state_path: Some(temp.join("state.json")),
        tool_services: Some(create_chat_tool_runtime_services(
            CreateChatToolRuntimeServicesOptions {
                cwd: Some(temp.clone()),
                jobs_root: Some(temp.join("jobs")),
            },
        )),
        url: Some(url),
        ..Default::default()
    })
    .await
    .expect("run starts");
    server.join().unwrap();

    assert_eq!(
        result.reason,
        HarnessRunReason::Completed,
        "{:?}",
        result.error_message
    );
    let loop_starts: Vec<String> = events
        .lock()
        .unwrap()
        .iter()
        .filter(|event| event.r#type == drip::core::types::HarnessEventType::LoopStart)
        .map(|event| event.detail.clone())
        .collect();
    assert_eq!(loop_starts.len(), 4, "{loop_starts:?}");
    // The detail reads `loop N[ [skills: …]][ [tools: …]] [role: R] — phase`, so
    // the surface segments sit between the iteration and the role: match the
    // role/phase suffix with `contains` rather than a prefix.
    assert!(
        loop_starts[0].contains("[role: planner] — planning"),
        "{}",
        loop_starts[0]
    );
    assert!(
        loop_starts[1].contains("[role: author] — task-1"),
        "{}",
        loop_starts[1]
    );
    assert!(
        loop_starts[2].contains("[role: replanner] — replanning"),
        "{}",
        loop_starts[2]
    );
    assert!(
        loop_starts[3].contains("[role: planner] — replanning"),
        "{}",
        loop_starts[3]
    );
}

/// Operator directive: a task loop whose model walks away while its MONITOR is
/// still checking does not end with the signal in flight — the loop holds for
/// the job, hands the settled result to the model in a fresh round, and the
/// model can then finish.
#[tokio::test]
async fn a_loop_waits_for_a_pending_monitor_and_wakes_up_with_its_result() {
    let dir = tempfile::tempdir().unwrap();
    let signal_path = dir.path().join("signal.txt");
    let signal_for_thread = signal_path.clone();
    // Land the signal well past MONITOR's inline grace window (2500ms), so the
    // call returns "still waiting" and the job is genuinely in flight when the
    // model concludes the loop.
    let writer = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(3_000));
        std::fs::write(&signal_for_thread, "ready\n").unwrap();
    });
    let tools = drip::tools::pack::builtin_tool_pack(Default::default());
    let (url, server) = spawn_scripted_server(vec![
        tool_call_response(
            "p",
            "plan_tasks",
            serde_json::json!({"tasks":["wait for signal.txt"]}),
        ),
        tool_call_response(
            "m",
            "MONITOR",
            serde_json::json!({
                "check": format!("test -f {}", signal_path.display()),
                "description": "signal.txt exists",
                "intervalMs": 50,
                "timeoutMs": 30_000
            }),
        ),
        // The model walks away: a text-only reply ends the cycle, and the loop
        // must hold instead of ending with the monitor still checking.
        text_response("The monitor is watching for signal.txt; nothing to do until it lands."),
        tool_call_response(
            "f",
            "finish_task",
            serde_json::json!({
                "status": "completed",
                "summary": "the signal appeared",
                "confidence": "high",
                "anchor": "none",
                "anchorNote": "the goal's own check is the signal file"
            }),
        ),
        text_response("Signal observed."),
    ]);
    let events = Arc::new(Mutex::new(Vec::<HarnessEvent>::new()));
    let sink = events.clone();
    let result = run_solid_state_harness(SolidStateHarnessOptions {
        cwd: Some(dir.path().to_string_lossy().into()),
        goal: "Wait for signal.txt to appear. Acceptance: `test -f signal.txt` must pass.".into(),
        max_iterations: Some(8),
        model: Some("mock".into()),
        summarize_run: Some(true),
        url: Some(url),
        tools,
        on_event: Some(Arc::new(move |event| sink.lock().unwrap().push(event))),
        state_path: Some(dir.path().join("state.json")),
        tool_services: Some(create_chat_tool_runtime_services(
            CreateChatToolRuntimeServicesOptions {
                cwd: Some(dir.path().into()),
                jobs_root: Some(dir.path().join("jobs")),
            },
        )),
        ..Default::default()
    })
    .await
    .unwrap();
    let bodies = server.join().unwrap();
    writer.join().unwrap();

    assert_eq!(
        result.reason,
        HarnessRunReason::Completed,
        "{:?}",
        result.error_message
    );
    assert_eq!(
        bodies.len(),
        5,
        "the resumed round must have run: {bodies:#?}"
    );
    // The hold fired, and the resumed round carried the settled report.
    let events = events.lock().unwrap();
    assert!(
        events
            .iter()
            .any(|event| event.detail.contains("still-checking MONITOR job")),
        "{:?}",
        events
            .iter()
            .map(|event| event.detail.clone())
            .collect::<Vec<_>>()
    );
    assert!(
        events
            .iter()
            .any(|event| event.detail.contains("finished: completed")),
        "the settled MONITOR must be reported: {:?}",
        events
            .iter()
            .map(|event| event.detail.clone())
            .collect::<Vec<_>>()
    );
    let saw_report = bodies
        .iter()
        .any(|body| body.to_string().contains("background job"));
    assert!(
        saw_report,
        "the round after the hold must carry the settled result: {bodies:#?}"
    );
}
