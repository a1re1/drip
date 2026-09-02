// End-to-end smoke test for the loop driver: a scripted OpenAI-compatible
// endpoint plays plan_tasks → finish_task → (text-only) run summary, and the
// run must complete with the same state shape and event sequence lci
// produces for that script.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use drip::core::types::{HarnessEvent, HarnessRunReason, HarnessTaskStatus};
use drip::harness::r#loop::{run_solid_state_harness, SolidStateHarnessOptions};
use drip::tools::async_jobs::{create_chat_tool_runtime_services, CreateChatToolRuntimeServicesOptions};

/// Serves one canned response per connection, in order.
fn spawn_scripted_server(responses: Vec<String>) -> (String, std::thread::JoinHandle<Vec<serde_json::Value>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let mut bodies = Vec::new();
        for response_body in responses {
            let (mut stream, _) = listener.accept().unwrap();
            let mut data: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 8192];
            let body_start = loop {
                let read = stream.read(&mut chunk).unwrap_or(0);
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
        // The summary call reports usage too (TS onUsage runs inside callModel).
        "inference", "run-summary", "run-complete",
    ];
    assert_eq!(kinds, expected, "event kinds");
}
