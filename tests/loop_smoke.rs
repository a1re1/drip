// End-to-end smoke test for the loop driver: a scripted OpenAI-compatible
// endpoint plays plan_tasks → finish_task → (text-only) run summary, and the
// run must complete with the expected state shape and event sequence for
// that script.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use drip::core::types::{HarnessEvent, HarnessRunReason, HarnessTaskStatus};
use drip::harness::r#loop::{run_solid_state_harness, SolidStateHarnessOptions};
use drip::tools::async_jobs::{create_chat_tool_runtime_services, CreateChatToolRuntimeServicesOptions};

/// Serves one canned response per connection, in order.
fn spawn_scripted_server(responses: Vec<String>) -> (String, std::thread::JoinHandle<Vec<serde_json::Value>>) {
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
                        assert!(std::time::Instant::now() < deadline, "scripted endpoint did not receive the expected call");
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
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
                        .find_map(|l| l.strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok()))
                        .unwrap_or(0);
                    if data.len() >= pos + 4 + len {
                        break pos + 4;
                    }
                }
            };
            bodies.push(serde_json::from_slice(&data[body_start..]).unwrap_or(serde_json::Value::Null));
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
    (format!("http://127.0.0.1:{port}/v1/chat/completions"), handle)
}

fn tool_call_response(id: &str, name: &str, arguments: serde_json::Value) -> String {
    serde_json::json!({
        "choices": [{"message": {"content": null, "tool_calls": [{"id": id, "type": "function", "function": {"name": name, "arguments": arguments.to_string()}}]}}],
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
        tools.iter_mut().find(|tool| tool.name == "PATCH").unwrap().name = writer.into();
        let assertion = "test \"$(cat artifact.txt)\" = correct && printf 'DRIP_VERIFY {\"executed\":1,\"passed\":1,\"failed\":0}\\n'";
        let (url, server) = spawn_scripted_server(vec![
            tool_call_response("p", "plan_tasks", serde_json::json!({"tasks":["produce artifact"]})),
            text_response("planned"),
            tool_call_response("w", writer, serde_json::json!({"path":"artifact.txt","content":"correct\n"})),
            tool_call_response("v", "VERIFY", serde_json::json!({"command":"true"})),
            tool_call_response("f1", "finish_task", serde_json::json!({"status":"completed","summary":"done"})),
            tool_call_response("f2", "finish_task", serde_json::json!({"status":"completed","summary":"done"})),
            tool_call_response("a", route, serde_json::json!({"command":assertion})),
            tool_call_response("bad-write", "BASH", serde_json::json!({"command":"printf corrupt > artifact.txt; exit 9"})),
            tool_call_response("stale", "finish_task", serde_json::json!({"status":"completed","summary":"earlier check passed"})),
            tool_call_response("restore", writer, serde_json::json!({"path":"artifact.txt","content":"correct\n"})),
            tool_call_response("fresh", route, serde_json::json!({"command":assertion})),
            tool_call_response("f3", "finish_task", serde_json::json!({"status":"completed","summary":"one assertion passed","confidence":"high","anchor":"none","anchorNote":"the assertion script is self-authored; no external fixture exists"})),
            text_response("Artifact verified."),
        ]);
        let result = run_solid_state_harness(SolidStateHarnessOptions {
            cwd: Some(dir.path().to_string_lossy().into()), goal: "produce a verified artifact".into(),
            max_iterations: Some(6), model: Some("mock".into()), url: Some(url),
            tools,
            on_event: Some(Arc::new(move |event| sink.lock().unwrap().push(event))),
            state_path: Some(dir.path().join("state.json")),
            tool_services: Some(create_chat_tool_runtime_services(CreateChatToolRuntimeServicesOptions {
                cwd: Some(dir.path().into()), jobs_root: Some(dir.path().join("jobs")),
            })),
            ..Default::default()
        }).await.unwrap();
        server.join().unwrap();
        assert_eq!(result.reason, HarnessRunReason::Completed, "{route}");
        let records = result.state.verifications.as_ref().unwrap();
        assert_eq!(records.len(), 3, "{route}");
        assert!(!records[0].evidence.as_ref().unwrap().verifies_work());
        assert!(records[1].evidence.as_ref().unwrap().verifies_work());
        assert_eq!(records[1].evidence.as_ref().unwrap().executed, 1);
        assert!(records[2].evidence.as_ref().unwrap().verifies_work());
        assert_eq!(events.lock().unwrap().iter().filter(|event| event.detail.contains("not accepted yet — this task edited")).count(), 3);
        assert_eq!(std::fs::read_to_string(dir.path().join("artifact.txt")).unwrap(), "correct\n");
        let anchor = result.state.completion_anchor.as_ref().expect("completion anchor recorded");
        assert_eq!(anchor.kind, drip::core::types::CompletionAnchorKind::None, "{route}");
        assert_eq!(anchor.claimed_confidence, drip::core::types::ClaimedConfidence::High, "{route}");
    }
}

/// The expectation gate end to end: an "external" anchor on a check that
/// names the edited artifact is downgraded, a mismatched observation refuses
/// `completed`, and `unreconciled` finishes the run with exit 0, the anomaly
/// in the payload, and a calibration record beside the state file.
#[tokio::test]
async fn mismatched_expectation_ends_unreconciled_with_exit_zero() {
    let dir = tempfile::tempdir().unwrap();
    let events = Arc::new(Mutex::new(Vec::<HarnessEvent>::new()));
    let sink = events.clone();
    let assertion = "test \"$(cat artifact.txt)\" = correct && printf 'DRIP_VERIFY {\"executed\":1,\"passed\":1,\"failed\":0}\\n'";
    let (url, server) = spawn_scripted_server(vec![
        tool_call_response("p", "plan_tasks", serde_json::json!({
            "tasks":["produce artifact"],
            "expectations":[{"subject":"artifact sign","expected":"positive"}]
        })),
        text_response("planned"),
        tool_call_response("w", "PATCH", serde_json::json!({"path":"artifact.txt","content":"correct\n"})),
        tool_call_response("v", "VERIFY", serde_json::json!({"command":assertion,"anchor":{"kind":"external","source":"artifact.txt fixture"}})),
        tool_call_response("f1", "finish_task", serde_json::json!({
            "status":"completed","summary":"done","confidence":"high","anchor":"none","anchorNote":"no external fixture",
            "observations":[{"subject":"artifact sign","observed":"negative","matches":false}]
        })),
        tool_call_response("f2", "finish_task", serde_json::json!({
            "status":"unreconciled","summary":"done","confidence":"low","anchor":"none","anchorNote":"no external fixture",
            "observations":[{"subject":"artifact sign","observed":"negative","matches":false}],
            "anomalies":[{"subject":"artifact sign","expected":"positive","observed":"negative","note":"cannot reconcile"}]
        })),
        text_response("Artifact produced; sign unreconciled."),
    ]);
    let state_path = dir.path().join("session").join("state.json");
    let result = run_solid_state_harness(SolidStateHarnessOptions {
        cwd: Some(dir.path().to_string_lossy().into()), goal: "produce a measured artifact".into(),
        max_iterations: Some(6), model: Some("mock".into()), url: Some(url),
        tools: drip::tools::pack::builtin_tool_pack(Default::default()),
        on_event: Some(Arc::new(move |event| sink.lock().unwrap().push(event))),
        state_path: Some(state_path.clone()),
        tool_services: Some(create_chat_tool_runtime_services(CreateChatToolRuntimeServicesOptions {
            cwd: Some(dir.path().into()), jobs_root: Some(dir.path().join("jobs")),
        })),
        ..Default::default()
    }).await.unwrap();
    server.join().unwrap();

    assert_eq!(result.reason, HarnessRunReason::Unreconciled, "error: {:?}", result.error_message);
    assert_eq!(result.state.anomalies.len(), 1);
    assert_eq!(result.state.expectations[0].observations.len(), 1);
    let anchor = result.state.verifications.as_ref().unwrap()[0].evidence.as_ref().unwrap().anchor.clone().expect("anchor recorded");
    assert_eq!(anchor.kind, drip::core::types::VerificationAnchorKind::SelfAuthored, "names the edited artifact");
    assert!(anchor.downgraded_reason.as_deref().unwrap_or_default().contains("artifact.txt"));
    let events = events.lock().unwrap();
    assert!(events.iter().any(|event| event.detail.contains("anchor downgraded to self-authored")));
    assert!(events.iter().any(|event| event.detail.contains("P1 against the model")));

    let record = drip::cli::run_record::build_run_record(&drip::cli::run_record::BuildRunRecordArgs {
        ended_at: "2026-01-01T00:00:00.000Z", goal: "produce a measured artifact", goal_id: "g1",
        max_iterations: Some(6), pending_operator_messages: 0, result: &result,
    });
    assert_eq!(record.reason, "unreconciled");
    assert_eq!(record.anomalies.as_ref().map(|anomalies| anomalies.len()), Some(1));
    assert_eq!(record.calibration.as_ref().map(|calibration| calibration.claimed_confidence.as_str()), Some("low"));
    let payload = drip::cli::headless_output::headless_result_payload(drip::cli::headless_output::HeadlessResultArgs {
        record: &record, result_path: "/r", session_id: "s", session_id_prefix: "s", state_path: "/s", transcript_path: "/t",
    });
    assert_eq!(payload.exit_code, 0);
    assert!(payload.continue_command.is_none());

    let calibration = std::fs::read_to_string(dir.path().join("session").join("calibration.jsonl")).expect("calibration trace written");
    let lines: Vec<&str> = calibration.lines().collect();
    assert_eq!(lines.len(), 1, "{calibration}");
    assert!(lines[0].contains("\"status\":\"unreconciled\"") && lines[0].contains("\"claimedConfidence\":\"low\""), "{calibration}");
    assert!(lines[0].contains("\"selfAuthored\":1") && lines[0].contains("\"mismatched\":1") && lines[0].contains("\"anomalies\":1"), "{calibration}");
}

#[tokio::test]
async fn plan_finish_summary_completes_the_run() {
    let temp_dir = tempfile::tempdir().unwrap();
    let temp = temp_dir.path().to_path_buf();
    let state_path = temp.join("state.json");

    let (url, server) = spawn_scripted_server(vec![
        // Loop 1 (planning): plan_tasks, then a text turn ends the loop.
        tool_call_response("call-1", "plan_tasks", serde_json::json!({"tasks": ["work on the file"]})),
        text_response("planned"),
        // Loop 2 (task-1): finish_task ends the loop and completes the goal.
        tool_call_response("call-2", "finish_task", serde_json::json!({"status": "completed", "summary": "wrote it"})),
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
        on_event: Some(Arc::new(move |event| sink.lock().unwrap().push(event))),
        state_path: Some(state_path.clone()),
        tool_services: Some(create_chat_tool_runtime_services(CreateChatToolRuntimeServicesOptions {
            cwd: Some(temp.clone()),
            jobs_root: Some(temp.join("jobs")),
        })),
        url: Some(url),
        ..Default::default()
    };

    let result = run_solid_state_harness(options).await.expect("run starts");
    let bodies = server.join().unwrap();

    let kinds_debug: Vec<String> = events.lock().unwrap().iter().map(|e| format!("{:?}: {}", e.r#type, e.detail.chars().take(160).collect::<String>())).collect();
    assert_eq!(result.reason, HarnessRunReason::Completed, "error: {:?}\nevents: {:#?}", result.error_message, kinds_debug);
    assert_eq!(result.iterations, 2);
    assert_eq!(result.r#loops, 2);
    assert_eq!(result.state.tasks.len(), 1);
    assert_eq!(result.state.tasks[0].status, HarnessTaskStatus::Completed);
    assert_eq!(result.state.tasks[0].summary.as_deref(), Some("wrote it"));
    assert_eq!(result.state.run_summary.as_ref().map(|s| s.text.as_str()), Some("All done: the file was written."));
    assert_eq!(result.usage.calls, 4);
    assert_eq!(result.usage.prompt_tokens, 26);
    assert!(state_path.exists(), "state persisted");

    // Requests: planning call advertises tools; the summary call sends none.
    assert_eq!(bodies.len(), 4);
    assert!(bodies[0]["tools"].as_array().map_or(false, |t| t.iter().any(|t| t["function"]["name"] == "plan_tasks")));
    assert!(bodies[3]["tools"].is_null() || bodies[3]["tools"].as_array().map_or(true, |t| t.is_empty()));
    // Round 2 of loop 1 replays the assistant tool call and the tool result.
    assert_eq!(bodies[1]["messages"].as_array().unwrap().len(), bodies[0]["messages"].as_array().unwrap().len() + 2);
    assert_eq!(bodies[1]["messages"][2]["role"], "assistant");
    assert_eq!(bodies[1]["messages"][3]["role"], "tool");
    assert_eq!(bodies[1]["messages"][3]["tool_call_id"], "call-1");
    assert_eq!(bodies[0]["messages"][0]["role"], "system");
    assert!(bodies[0]["messages"][1]["content"].as_str().unwrap().contains("goal: write the file"));

    let kinds: Vec<String> = events
        .lock()
        .unwrap()
        .iter()
        .map(|event| serde_json::to_value(&event.r#type).unwrap().as_str().unwrap().to_string())
        .collect();
    let expected = [
        "loop-start", "iteration-start", "inference", "harness-op", "inference", "model-text",
        "loop-start", "iteration-start", "inference", "task-finished",
        // The summary call reports usage too, so it emits its own inference event.
        "inference", "run-summary", "run-complete",
    ];
    assert_eq!(kinds, expected, "event kinds");
}
