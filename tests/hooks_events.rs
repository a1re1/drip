// End-to-end hook-event tests: a scripted OpenAI-compatible endpoint drives
// the run and marker-file hooks record which hook events fired. Covers the
// MemoryWrite success-only gate (failed/denied/no-op memory ops stay silent,
// a successful remember fires exactly once) and the PreToolUse exit-2 veto
// (blocked call never executes the tool, stderr reaches the tool result, no
// post-event fires, and a non-vetoing hook leaves execution untouched).

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use drip::core::types::{HarnessEvent, HarnessRunReason};
use drip::harness::hooks::HooksConfig;
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

fn marker_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("drip-hook-events-{}-{name}", std::process::id()))
}

fn marker_hook(path: &std::path::Path) -> String {
    format!("cat >> {}", path.to_string_lossy())
}

fn count_marker_lines(path: &std::path::Path, needle: &str) -> usize {
    std::fs::read_to_string(path)
        .map(|contents| contents.lines().filter(|l| l.contains(needle)).count())
        .unwrap_or(0)
}

// A registered stand-in for BASH: headless runs with a temp cwd load no
// default tools, so the veto/control tests inject this tool double to make
// side-effect and post_tool_use assertions meaningful (its command text must
// stay non-writing — see the non-veto test).
fn fake_bash_tool(side_effect: std::path::PathBuf) -> drip::tools::types::ChatToolDefinition {
    use drip::tools::types::{define_sync_tool, ChatToolDefinition, ChatToolMode, ChatToolParameters};
    define_sync_tool(ChatToolDefinition {
        name: "BASH".to_string(),
        description: "test double that records execution in a side-effect file".to_string(),
        parameters: ChatToolParameters::object(),
        // Only writes a temp marker file — not a real workspace mutation.
        // Leaving this true makes the harness schedule extra verification
        // rounds after the scripted responses run out (rate-limit storm).
        mutates_workspace: false,
        mode: ChatToolMode::Sync,
        prepare: Box::new(|request| {
            Ok(drip::tools::types::ChatToolPreparedInput {
                display_input: request.raw_input.to_string(),
                input: serde_json::from_str(request.raw_input).unwrap_or(serde_json::Value::Null),
                ..Default::default()
            })
        }),
        execute: Box::new(move |_| {
            let _ = std::fs::write(&side_effect, b"executed");
            Ok(drip::tools::types::ChatToolResult {
                output_text: Some("executed".to_string()),
                ..Default::default()
            })
        }),
        complete: Box::new(|_| {
            Ok(drip::tools::types::ChatToolCompletionResult {
                blocks: None,
                tool_content: Some("executed".to_string()),
                tags: None,
            })
        }),
    })
}

fn base_options(
    temp: &std::path::Path,
    url: String,
    hooks: HooksConfig,
    sink: Arc<Mutex<Vec<HarnessEvent>>>,
) -> SolidStateHarnessOptions {
    SolidStateHarnessOptions {
        cwd: Some(temp.to_string_lossy().to_string()),
        goal: "exercise the hooks".to_string(),
        max_iterations: Some(6),
        model: Some("mock".to_string()),
        hooks,
        on_event: Some(Arc::new(move |event| sink.lock().unwrap().push(event))),
        state_path: Some(temp.join("state.json")),
        tool_services: Some(create_chat_tool_runtime_services(CreateChatToolRuntimeServicesOptions {
            cwd: Some(temp.to_path_buf()),
            jobs_root: Some(temp.join("jobs")),
        })),
        url: Some(url),
        tools: Vec::new(),
        ..Default::default()
    }
}

fn event_kinds(events: &Arc<Mutex<Vec<HarnessEvent>>>) -> Vec<String> {
    events
        .lock()
        .unwrap()
        .iter()
        .map(|e| format!("{:?}", e.r#type))
        .collect()
}

#[tokio::test]
async fn successful_remember_fires_memory_write_exactly_once() {
    let marker = marker_path("mw-success");
    let _ = std::fs::remove_file(&marker);
    let temp_dir = tempfile::tempdir().unwrap();
    let temp = temp_dir.path().to_path_buf();
    let hooks: HooksConfig = serde_json::from_value(serde_json::json!({
        "memory_write": [marker_hook(&marker)]
    }))
    .unwrap();

    let events = Arc::new(Mutex::new(Vec::new()));
    let (url, server) = spawn_scripted_server(vec![
        tool_call_response("call-1", "plan_tasks", serde_json::json!({"tasks": ["remember a fact"]})),
        text_response("planned"),
        tool_call_response("call-2", "remember", serde_json::json!({"note": "the fixture note alpha-xyz"})),
        tool_call_response("call-3", "finish_task", serde_json::json!({"status": "completed", "summary": "remembered"})),
        text_response("done"),
    ]);

    let result = run_solid_state_harness(base_options(&temp, url, hooks, events.clone()))
        .await
        .expect("run starts");
    let _bodies = server.join().unwrap();

    assert_eq!(
        result.reason,
        HarnessRunReason::Completed,
        "events: {:?}",
        event_kinds(&events)
    );
    let contents = std::fs::read_to_string(&marker).unwrap_or_default();
    let mw_lines: Vec<&str> = contents.lines().filter(|l| l.contains("MemoryWrite")).collect();
    assert_eq!(mw_lines.len(), 1, "exactly one MemoryWrite payload, got: {contents}");
    assert!(
        mw_lines[0].contains("\"tool_name\"") && mw_lines[0].contains("remember"),
        "payload names the tool: {}",
        mw_lines[0]
    );
    assert!(
        contents.contains("the fixture note alpha-xyz"),
        "payload carries the note: {contents}"
    );
    let _ = std::fs::remove_file(&marker);
}

#[tokio::test]
async fn failed_remember_fires_no_memory_write() {
    let marker = marker_path("mw-failed");
    let _ = std::fs::remove_file(&marker);
    let temp_dir = tempfile::tempdir().unwrap();
    let temp = temp_dir.path().to_path_buf();
    let hooks: HooksConfig = serde_json::from_value(serde_json::json!({
        "memory_write": [marker_hook(&marker)]
    }))
    .unwrap();

    let events = Arc::new(Mutex::new(Vec::new()));
    let (url, server) = spawn_scripted_server(vec![
        tool_call_response("call-1", "plan_tasks", serde_json::json!({"tasks": ["remember a fact"]})),
        text_response("planned"),
        // Malformed: `note` is required, so apply_harness_op rejects the op
        // (parse failure keeps the run alive) and state_changed stays false.
        tool_call_response("call-2", "remember", serde_json::json!({"scope": "session"})),
        tool_call_response("call-3", "finish_task", serde_json::json!({"status": "completed", "summary": "gave up"})),
        text_response("done"),
    ]);

    let result = run_solid_state_harness(base_options(&temp, url, hooks, events.clone()))
        .await
        .expect("run starts");
    let _bodies = server.join().unwrap();

    assert_eq!(
        result.reason,
        HarnessRunReason::Completed,
        "events: {:?}",
        event_kinds(&events)
    );
    assert_eq!(
        count_marker_lines(&marker, "MemoryWrite"),
        0,
        "failed memory op must not fire memory_write"
    );
    let _ = std::fs::remove_file(&marker);
}

#[tokio::test]
async fn pre_tool_use_exit_two_blocks_the_tool_call() {
    let marker = marker_path("veto-post");
    let side_effect = marker_path("veto-side-effect");
    let _ = std::fs::remove_file(&marker);
    let _ = std::fs::remove_file(&side_effect);
    let temp_dir = tempfile::tempdir().unwrap();
    let temp = temp_dir.path().to_path_buf();
    // No matcher = applies to every workspace tool. The hook writes to stderr
    // and exits 2 — the veto code.
    let hooks: HooksConfig = serde_json::from_value(serde_json::json!({
        "pre_tool_use": [{"command": "echo hook-says-no >&2; exit 2"}],
        "post_tool_use": [{"command": marker_hook(&marker)}]
    }))
    .unwrap();

    let events = Arc::new(Mutex::new(Vec::new()));
    let (url, server) = spawn_scripted_server(vec![
        tool_call_response("call-1", "plan_tasks", serde_json::json!({"tasks": ["touch the file"]})),
        text_response("planned"),
        tool_call_response(
            "call-2",
            "BASH",
            serde_json::json!({"command": format!("touch {}", side_effect.to_string_lossy())}),
        ),
        tool_call_response("call-3", "finish_task", serde_json::json!({"status": "completed", "summary": "done"})),
        text_response("done"),
    ]);

    let mut options = base_options(&temp, url, hooks, events.clone());
    options.tools = vec![fake_bash_tool(side_effect.clone())];
    let result = run_solid_state_harness(options).await.expect("run starts");
    let bodies = server.join().unwrap();

    assert_eq!(
        result.reason,
        HarnessRunReason::Completed,
        "events: {:?}",
        event_kinds(&events)
    );

    // bodies[3] replays loop 2 round 1: the assistant tool call and the tool
    // result — which must be the veto message carrying the hook's stderr.
    let replay = bodies[3]["messages"].as_array().cloned().unwrap_or_default();
    let tool_texts: Vec<String> = replay
        .iter()
        .filter(|m| m["role"] == "tool")
        .filter_map(|m| m["content"].as_str().map(|s| s.to_string()))
        .collect();
    assert!(
        tool_texts
            .iter()
            .any(|t| t.contains("tool call blocked by pre_tool_use hook") && t.contains("hook-says-no")),
        "tool result must carry the veto + hook stderr, got: {tool_texts:?}"
    );

    // The vetoed tool never ran and no post_tool_use hook fired for it.
    assert!(!side_effect.exists(), "vetoed tool must not execute its side effect");
    assert_eq!(
        count_marker_lines(&marker, "PostToolUse"),
        0,
        "no post_tool_use event may fire for a vetoed call"
    );
    let _ = std::fs::remove_file(&marker);
    let _ = std::fs::remove_file(&side_effect);
}

#[tokio::test]
async fn non_vetoing_pre_tool_use_leaves_execution_untouched() {
    let marker = marker_path("pass-post");
    let side_effect = marker_path("pass-side-effect");
    let _ = std::fs::remove_file(&marker);
    let _ = std::fs::remove_file(&side_effect);
    let temp_dir = tempfile::tempdir().unwrap();
    let temp = temp_dir.path().to_path_buf();
    let hooks: HooksConfig = serde_json::from_value(serde_json::json!({
        "pre_tool_use": [{"command": "exit 0"}],
        "post_tool_use": [{"command": marker_hook(&marker)}]
    }))
    .unwrap();

    let events = Arc::new(Mutex::new(Vec::new()));
    let (url, server) = spawn_scripted_server(vec![
        tool_call_response("call-1", "plan_tasks", serde_json::json!({"tasks": ["touch the file"]})),
        text_response("planned"),
        tool_call_response(
            "call-2",
            "BASH",
            // Non-writing command text on purpose: is_writing_shell_command
            // counts "touch" as a workspace mutation, which schedules extra
            // verification rounds after the scripted responses end. The fake
            // tool's closure creates the side-effect file itself.
            serde_json::json!({"command": "echo touched-ok"}),
        ),
        tool_call_response("call-3", "finish_task", serde_json::json!({"status": "completed", "summary": "done"})),
        text_response("done"),
    ]);

    let mut options = base_options(&temp, url, hooks, events.clone());
    options.tools = vec![fake_bash_tool(side_effect.clone())];
    let result = run_solid_state_harness(options).await.expect("run starts");
    let bodies = server.join().unwrap();

    assert_eq!(
        result.reason,
        HarnessRunReason::Completed,
        "events: {:?}",
        event_kinds(&events)
    );
    let replay = bodies[3]["messages"].as_array().cloned().unwrap_or_default();
    let tool_texts: Vec<String> = replay
        .iter()
        .filter(|m| m["role"] == "tool")
        .filter_map(|m| m["content"].as_str().map(|s| s.to_string()))
        .collect();
    assert!(
        !tool_texts
            .iter()
            .any(|t| t.contains("blocked by pre_tool_use hook")),
        "non-vetoing pre hook must not block, got: {tool_texts:?}"
    );
    assert!(
        tool_texts.iter().any(|t| t.contains("executed")),
        "tool result should carry the tool's output, got: {tool_texts:?}"
    );
    assert!(side_effect.exists(), "tool must execute when the pre hook does not veto");
    assert_eq!(
        count_marker_lines(&marker, "PostToolUse"),
        1,
        "exactly one post_tool_use event for the executed tool"
    );
    let _ = std::fs::remove_file(&marker);
    let _ = std::fs::remove_file(&side_effect);
}

// ---------------------------------------------------------------------------
// PRReady regression tests (task-7 follow-up): the harness fires pr_ready only
// after a successful, non-vetoed tool execution whose command text matches the
// shared publish pattern (git commit/push, gh pr create).
// ---------------------------------------------------------------------------

fn fake_failing_bash_tool() -> drip::tools::types::ChatToolDefinition {
    use drip::tools::types::{define_sync_tool, ChatToolDefinition, ChatToolMode, ChatToolParameters};
    define_sync_tool(ChatToolDefinition {
        name: "BASH".to_string(),
        description: "test double whose execution always fails".to_string(),
        parameters: ChatToolParameters::object(),
        mutates_workspace: false,
        mode: ChatToolMode::Sync,
        prepare: Box::new(|request| {
            Ok(drip::tools::types::ChatToolPreparedInput {
                display_input: request.raw_input.to_string(),
                input: serde_json::from_str(request.raw_input).unwrap_or(serde_json::Value::Null),
                ..Default::default()
            })
        }),
        // An Err from the execute stage becomes a failed tool-call block
        // (execute.rs build_failure_result), so execution.failed = true.
        execute: Box::new(|_| Err("git push failed: rejected".to_string())),
        complete: Box::new(|_| {
            Ok(drip::tools::types::ChatToolCompletionResult {
                blocks: None,
                tool_content: Some("git push failed: rejected".to_string()),
                tags: None,
            })
        }),
    })
}

// "git push" (not "git commit") on purpose: is_writing_shell_command counts
// "commit" via WRITING_GIT_SUBCOMMANDS as a workspace mutation, which
// schedules extra verification rounds after the scripted responses end
// (rate-limit storm). "push" is still a publish-pattern match.
const PUBLISH_COMMAND: &str = "git push origin feat/hooks";

fn publish_script() -> Vec<String> {
    vec![
        tool_call_response("call-1", "plan_tasks", serde_json::json!({"tasks": ["publish the branch"]})),
        text_response("planned"),
        tool_call_response(
            "call-2",
            "BASH",
            serde_json::json!({"command": PUBLISH_COMMAND}),
        ),
        tool_call_response("call-3", "finish_task", serde_json::json!({"status": "completed", "summary": "done"})),
        text_response("done"),
    ]
}

#[tokio::test]
async fn successful_publish_fires_pr_ready_exactly_once() {
    let marker = marker_path("pr-ready-success");
    let side_effect = marker_path("pr-ready-success-side-effect");
    let _ = std::fs::remove_file(&marker);
    let _ = std::fs::remove_file(&side_effect);
    let temp_dir = tempfile::tempdir().unwrap();
    let temp = temp_dir.path().to_path_buf();
    // Ordering guard: the hook only appends a marker line when the publish
    // tool's side-effect file already exists. PRReady firing BEFORE tool
    // execution would make this command exit 1 and leave the marker empty,
    // failing the count assertion below.
    let hooks: HooksConfig = serde_json::from_value(serde_json::json!({
        "pr_ready": [format!("[ -f {} ] && cat >> {}", side_effect.display(), marker.display())]
    }))
    .unwrap();

    let events = Arc::new(Mutex::new(Vec::new()));
    let (url, server) = spawn_scripted_server(publish_script());

    let mut options = base_options(&temp, url, hooks, events.clone());
    options.tools = vec![fake_bash_tool(side_effect.clone())];
    let result = run_solid_state_harness(options).await.expect("run starts");
    let _bodies = server.join().unwrap();

    assert_eq!(
        result.reason,
        HarnessRunReason::Completed,
        "events: {:?}",
        event_kinds(&events)
    );
    assert!(side_effect.exists(), "publish tool must have executed");
    assert_eq!(
        count_marker_lines(&marker, "PRReady"),
        1,
        "exactly one pr_ready event after a successful publish"
    );
    let _ = std::fs::remove_file(&marker);
    let _ = std::fs::remove_file(&side_effect);
}

#[tokio::test]
async fn failed_publish_fires_no_pr_ready() {
    let marker = marker_path("pr-ready-failed");
    let _ = std::fs::remove_file(&marker);
    let temp_dir = tempfile::tempdir().unwrap();
    let temp = temp_dir.path().to_path_buf();
    let hooks: HooksConfig = serde_json::from_value(serde_json::json!({
        "pr_ready": [marker_hook(&marker)]
    }))
    .unwrap();

    let events = Arc::new(Mutex::new(Vec::new()));
    let (url, server) = spawn_scripted_server(publish_script());

    let mut options = base_options(&temp, url, hooks, events.clone());
    options.tools = vec![fake_failing_bash_tool()];
    let result = run_solid_state_harness(options).await.expect("run starts");
    let _bodies = server.join().unwrap();

    assert_eq!(
        result.reason,
        HarnessRunReason::Completed,
        "events: {:?}",
        event_kinds(&events)
    );
    assert_eq!(
        count_marker_lines(&marker, "PRReady"),
        0,
        "a failed publish must stay silent"
    );
    let _ = std::fs::remove_file(&marker);
}

#[tokio::test]
async fn vetoed_publish_fires_no_pr_ready() {
    let marker = marker_path("pr-ready-vetoed");
    let side_effect = marker_path("pr-ready-vetoed-side-effect");
    let _ = std::fs::remove_file(&marker);
    let _ = std::fs::remove_file(&side_effect);
    let temp_dir = tempfile::tempdir().unwrap();
    let temp = temp_dir.path().to_path_buf();
    let hooks: HooksConfig = serde_json::from_value(serde_json::json!({
        "pre_tool_use": [{"command": "echo hook-says-no >&2; exit 2"}],
        "pr_ready": [marker_hook(&marker)]
    }))
    .unwrap();

    let events = Arc::new(Mutex::new(Vec::new()));
    let (url, server) = spawn_scripted_server(publish_script());

    let mut options = base_options(&temp, url, hooks, events.clone());
    options.tools = vec![fake_bash_tool(side_effect.clone())];
    let result = run_solid_state_harness(options).await.expect("run starts");
    let bodies = server.join().unwrap();

    assert_eq!(
        result.reason,
        HarnessRunReason::Completed,
        "events: {:?}",
        event_kinds(&events)
    );
    // The veto still blocks the publish tool...
    let replay = bodies[3]["messages"].as_array().cloned().unwrap_or_default();
    let tool_texts: Vec<String> = replay
        .iter()
        .filter(|m| m["role"] == "tool")
        .filter_map(|m| m["content"].as_str().map(|s| s.to_string()))
        .collect();
    assert!(
        tool_texts
            .iter()
            .any(|t| t.contains("tool call blocked by pre_tool_use hook") && t.contains("hook-says-no")),
        "vetoed publish must be blocked with the hook stderr, got: {tool_texts:?}"
    );
    assert!(!side_effect.exists(), "vetoed publish tool must not execute");
    // ...and pr_ready never fires for it.
    assert_eq!(
        count_marker_lines(&marker, "PRReady"),
        0,
        "a vetoed publish must stay silent"
    );
    let _ = std::fs::remove_file(&marker);
    let _ = std::fs::remove_file(&side_effect);
}

#[tokio::test]
async fn pr_ready_hook_failure_surfaces_run_warning() {
    let side_effect = marker_path("pr-ready-runwarning-side-effect");
    let _ = std::fs::remove_file(&side_effect);
    let temp_dir = tempfile::tempdir().unwrap();
    let temp = temp_dir.path().to_path_buf();
    let hooks: HooksConfig = serde_json::from_value(serde_json::json!({
        "pr_ready": ["exit 3"]
    }))
    .unwrap();

    let events = Arc::new(Mutex::new(Vec::new()));
    let (url, server) = spawn_scripted_server(publish_script());

    let mut options = base_options(&temp, url, hooks, events.clone());
    options.tools = vec![fake_bash_tool(side_effect.clone())];
    let result = run_solid_state_harness(options).await.expect("run starts");
    let _bodies = server.join().unwrap();

    assert_eq!(
        result.reason,
        HarnessRunReason::Completed,
        "events: {:?}",
        event_kinds(&events)
    );
    assert!(side_effect.exists(), "publish tool must have executed");
    assert!(
        event_kinds(&events).iter().any(|k| k.contains("RunWarning")),
        "pr_ready hook failure must surface as RunWarning, events: {:?}",
        event_kinds(&events)
    );
    let _ = std::fs::remove_file(&side_effect);
}

#[tokio::test]
async fn empty_pr_ready_configuration_stays_inert() {
    let marker = marker_path("pr-ready-inert");
    let side_effect = marker_path("pr-ready-inert-side-effect");
    let _ = std::fs::remove_file(&marker);
    let _ = std::fs::remove_file(&side_effect);
    let temp_dir = tempfile::tempdir().unwrap();
    let temp = temp_dir.path().to_path_buf();
    // Empty (default) hooks config: pr_ready must not spawn any process.
    let hooks = HooksConfig::default();

    let events = Arc::new(Mutex::new(Vec::new()));
    let (url, server) = spawn_scripted_server(publish_script());

    let mut options = base_options(&temp, url, hooks, events.clone());
    options.tools = vec![fake_bash_tool(side_effect.clone())];
    let result = run_solid_state_harness(options).await.expect("run starts");
    let _bodies = server.join().unwrap();

    assert_eq!(
        result.reason,
        HarnessRunReason::Completed,
        "events: {:?}",
        event_kinds(&events)
    );
    assert!(side_effect.exists(), "publish tool still executes with empty hook config");
    assert!(!marker.exists(), "no hook command exists, so no marker may be created");
    assert!(
        !event_kinds(&events).iter().any(|k| k.contains("RunWarning")),
        "empty config is inert — no hook failure warnings, events: {:?}",
        event_kinds(&events)
    );
    let _ = std::fs::remove_file(&side_effect);
}

// task-17: task-6 gap cases. (1) A successful session-scope forget (of a note
// created by a remember earlier in the SAME run) must fire MemoryWrite exactly
// once for the forget, distinguishable from the remember's own MemoryWrite by
// tool_name. (2) A PreToolUse exit-2 hook that writes nothing to stderr must
// veto the call with the "(no stderr output)" fallback and execute nothing.
#[tokio::test]
async fn successful_forget_fires_memory_write_exactly_once() {
    let marker = marker_path("forget-mw");
    let _ = std::fs::remove_file(&marker);
    let temp_dir = tempfile::tempdir().unwrap();
    let temp = temp_dir.path().to_path_buf();
    // memory_write is a lifecycle event: plain command array, no matchers.
    // The payload has no trailing newline, so this hook appends one itself to
    // keep successive events on separate lines.
    let hooks: HooksConfig = serde_json::from_value(serde_json::json!({
        "memory_write": [format!("cat >> {}; echo >> {}", marker.display(), marker.display())]
    }))
    .unwrap();

    // Remember then forget, same run. Session note ids are sequential
    // ("note-1" is the first note in a fresh state), so the scripted forget
    // can target the note this very run created.
    let events = Arc::new(Mutex::new(Vec::new()));
    let (url, server) = spawn_scripted_server(vec![
        tool_call_response("call-1", "plan_tasks", serde_json::json!({"tasks": ["note then forget"]})),
        text_response("planned"),
        tool_call_response(
            "call-2",
            "remember",
            serde_json::json!({"note": "forget-me alpha-1"}),
        ),
        tool_call_response("call-3", "forget", serde_json::json!({"noteId": "note-1"})),
        tool_call_response("call-4", "finish_task", serde_json::json!({"status": "completed", "summary": "done"})),
        text_response("done"),
    ]);

    let options = base_options(&temp, url, hooks, events.clone());
    let result = run_solid_state_harness(options).await.expect("run starts");
    let bodies = server.join().unwrap();

    assert_eq!(
        result.reason,
        HarnessRunReason::Completed,
        "events: {:?}",
        event_kinds(&events)
    );

    // The forget actually removed the note this run created. Its tool result
    // appears in the 5th request (after the forget response is consumed).
    let replay = bodies[4]["messages"].as_array().cloned().unwrap_or_default();
    let tool_texts: Vec<String> = replay
        .iter()
        .filter(|m| m["role"] == "tool")
        .filter_map(|m| m["content"].as_str().map(|s| s.to_string()))
        .collect();
    assert!(
        tool_texts
            .iter()
            .any(|t| t.contains("Removed memory note note-1.")),
        "forget must have removed note-1, got: {tool_texts:?}"
    );

    // Both successful memory ops fire exactly once each, distinguishable by
    // tool_name: one for the remember, one for the forget.
    let marker_text = std::fs::read_to_string(&marker).unwrap_or_default();
    assert_eq!(
        count_marker_lines(&marker, "MemoryWrite"),
        2,
        "one MemoryWrite per successful memory op (remember + forget), got: {marker_text}"
    );
    let forget_line = marker_text
        .lines()
        .find(|l| l.contains("\"tool_name\":\"forget\""))
        .expect("forget must fire memory_write with tool_name \"forget\"");
    let remember_count = marker_text
        .lines()
        .filter(|l| l.contains("\"tool_name\":\"remember\""))
        .count();
    assert_eq!(
        remember_count, 1,
        "remember must fire memory_write exactly once, got: {marker_text}"
    );
    assert!(
        forget_line.contains("note-1"),
        "forget's MemoryWrite payload must carry the redacted forget input, got: {forget_line}"
    );
    assert!(
        !forget_line.contains("forget-me alpha-1"),
        "forget's MemoryWrite payload must not carry the remember's note text, got: {forget_line}"
    );
    let _ = std::fs::remove_file(&marker);
}

#[tokio::test]
async fn pre_tool_use_exit_two_with_empty_stderr_reports_no_stderr_output() {
    let marker = marker_path("veto-empty-stderr-post");
    let side_effect = marker_path("veto-empty-stderr-side-effect");
    let _ = std::fs::remove_file(&marker);
    let _ = std::fs::remove_file(&side_effect);
    let temp_dir = tempfile::tempdir().unwrap();
    let temp = temp_dir.path().to_path_buf();
    // Veto hook that produces no stderr at all.
    let hooks: HooksConfig = serde_json::from_value(serde_json::json!({
        "pre_tool_use": [{"command": "exit 2"}],
        "post_tool_use": [{"command": marker_hook(&marker)}]
    }))
    .unwrap();

    let events = Arc::new(Mutex::new(Vec::new()));
    let (url, server) = spawn_scripted_server(vec![
        tool_call_response("call-1", "plan_tasks", serde_json::json!({"tasks": ["touch the file"]})),
        text_response("planned"),
        tool_call_response("call-2", "BASH", serde_json::json!({"command": "echo touched-ok"})),
        tool_call_response("call-3", "finish_task", serde_json::json!({"status": "completed", "summary": "done"})),
        text_response("done"),
    ]);

    let mut options = base_options(&temp, url, hooks, events.clone());
    options.tools = vec![fake_bash_tool(side_effect.clone())];
    let result = run_solid_state_harness(options).await.expect("run starts");
    let bodies = server.join().unwrap();

    assert_eq!(
        result.reason,
        HarnessRunReason::Completed,
        "events: {:?}",
        event_kinds(&events)
    );

    let replay = bodies[3]["messages"].as_array().cloned().unwrap_or_default();
    let tool_texts: Vec<String> = replay
        .iter()
        .filter(|m| m["role"] == "tool")
        .filter_map(|m| m["content"].as_str().map(|s| s.to_string()))
        .collect();
    assert!(
        tool_texts
            .iter()
            .any(|t| t.contains("tool call blocked by pre_tool_use hook") && t.contains("(no stderr output)")),
        "empty-stderr veto must surface the fallback text, got: {tool_texts:?}"
    );

    // The vetoed tool never ran and no post_tool_use hook fired for it.
    assert!(!side_effect.exists(), "vetoed tool must not execute its side effect");
    assert_eq!(
        count_marker_lines(&marker, "PostToolUse"),
        0,
        "no post_tool_use event may fire for a vetoed call"
    );
    let _ = std::fs::remove_file(&marker);
    let _ = std::fs::remove_file(&side_effect);
}
