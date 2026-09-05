use std::path::PathBuf;
use std::process::Command;

use drip::harness::codex::{BridgeConfig, CodexBridge};
use drip::harness::model_call::AbortSignal;
use drip::harness::transport::{
    create_request_tool, OpenAICompatibleRequestTool, TransportContent, TransportRequestMessage,
};

fn python3() -> String {
    Command::new("which")
        .arg("python3")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "python3".to_string())
}

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex_mock.py")
}

fn mock_config(script: &str, cwd: &PathBuf, timeout_ms: u64) -> BridgeConfig {
    BridgeConfig {
        executable: python3(),
        args: vec![
            fixture_path().to_string_lossy().into_owned(),
            "--script".to_string(),
            script.to_string(),
            "app-server".to_string(),
        ],
        cwd: Some(cwd.clone()),
        model: Some("gpt-5.6-luna".to_string()),
        reasoning_effort: Some("high".to_string()),
        request_timeout_ms: timeout_ms,
        ..Default::default()
    }
}

fn user_msg(text: &str) -> TransportRequestMessage {
    TransportRequestMessage {
        content: Some(TransportContent::Text(text.to_string())),
        role: drip::harness::chat_types::ChatRoleTag::User,
        ..Default::default()
    }
}

fn assistant_tool_call_msg(call_id: &str, tool: &str, arguments: &str) -> TransportRequestMessage {
    TransportRequestMessage {
        role: drip::harness::chat_types::ChatRoleTag::Assistant,
        tool_calls: Some(vec![drip::harness::transport::OpenAICompatibleToolCall {
            id: Some(call_id.to_string()),
            function: Some(drip::harness::transport::OpenAICompatibleToolCallFunction {
                name: Some(tool.to_string()),
                arguments: Some(arguments.to_string()),
            }),
            tool_type: Some("function".to_string()),
        }]),
        ..Default::default()
    }
}

fn tool_result_msg(call_id: &str, text: &str) -> TransportRequestMessage {
    TransportRequestMessage {
        role: drip::harness::chat_types::ChatRoleTag::Tool,
        tool_call_id: Some(call_id.to_string()),
        content: Some(TransportContent::Text(text.to_string())),
        ..Default::default()
    }
}

fn shell_tool() -> OpenAICompatibleRequestTool {
    create_request_tool(
        "shell",
        "run a shell command",
        serde_json::json!({"type":"object","properties":{"command":{"type":"string"}}}),
    )
}

async fn spawn_bridge(script: &str) -> (CodexBridge, PathBuf, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let cwd: PathBuf = dir.path().to_path_buf();
    let config = mock_config(script, &cwd, 2000);
    let bridge = CodexBridge::spawn(config).await.expect("spawn mock codex");
    // TempDir ownership moves to the caller so the cwd outlives the bridge.
    (bridge, cwd, dir)
}

fn read_log(cwd: &PathBuf) -> Vec<serde_json::Value> {
    let path = cwd.join("codex_mock_log.jsonl");
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read log {:?}: {}", path, e));
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap_or_else(|e| panic!("bad log line {:?}: {}", l, e)))
        .collect()
}

fn assert_request(log: &[serde_json::Value], method: &str) -> serde_json::Value {
    log.iter()
        .find_map(|e| {
            if e.get("received").and_then(|f| f.get("method")).and_then(|m| m.as_str()) == Some(method) {
                e.get("received").cloned()
            } else {
                None
            }
        })
        .unwrap_or_else(|| panic!("log missing {} request; log={:?}", method, log))
}

fn text_of(resp: &drip::harness::model_call::OpenAICompatibleResponse) -> String {
    let choice = resp
        .choices
        .as_ref()
        .and_then(|c| c.first())
        .unwrap_or_else(|| panic!("no choices in response"));
    let msg = choice.message.as_ref().unwrap_or_else(|| panic!("no message"));
    match msg.content.as_ref() {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn assert_usage_not_double_counted(resp: &drip::harness::model_call::OpenAICompatibleResponse) {
    let usage = resp
        .usage
        .as_ref()
        .unwrap_or_else(|| panic!("response missing usage"));
    assert_eq!(usage.prompt_tokens, Some(10), "inputTokens must not be double-counted");
    assert_eq!(usage.completion_tokens, Some(5), "outputTokens must not be double-counted");
    assert_eq!(usage.total_tokens, Some(15), "totalTokens must not be double-counted");
}

#[tokio::test]
async fn happy_path_initializes_reads_account_starts_thread_and_turn() {
    let (mut bridge, cwd, _dir) = spawn_bridge("happy").await;
    let resp = bridge
        .call(&[user_msg("hello mock")], &[], None, Some("high"))
        .await
        .expect("call should succeed");
    let text = text_of(&resp);
    assert_eq!(text, "final answer from mock");
    assert_usage_not_double_counted(&resp);

    let log = read_log(&cwd);
    // initialize id echoed
    let init = assert_request(&log, "initialize");
    assert_eq!(init.get("id"), Some(&serde_json::json!(0)));
    // account read with chatgpt type served before thread/start
    let account = assert_request(&log, "account/read");
    let as_id = |v: &serde_json::Value| {
        v.as_i64().unwrap_or_else(|| panic!("non-integer JSON-RPC id: {}", v))
    };
    let account_id = as_id(account.get("id").expect("account/read id"));
    let thread_start = assert_request(&log, "thread/start");
    let thread_id = as_id(thread_start.get("id").expect("thread/start id"));
    let turn_start = assert_request(&log, "turn/start");
    let turn_id = as_id(turn_start.get("id").expect("turn/start id"));
    assert!(
        account_id < thread_id && thread_id < turn_id,
        "account/read must precede thread/start which precedes turn/start"
    );
    // thread/start result.thread.id consumed
    assert_eq!(thread_start.pointer("/params/threadId"), None);
    // turn/start carries the requested model and effort; sandbox/cwd/
    // modelProvider are thread/start settings inherited by turns.
    let params = turn_start.get("params").cloned().unwrap_or_default();
    assert_eq!(params.get("model"), Some(&serde_json::json!("gpt-5.6-luna")));
    assert_eq!(params.get("effort"), Some(&serde_json::json!("high")));
    let tparams = thread_start.get("params").cloned().unwrap_or_default();
    let sandbox = tparams
        .get("sandbox")
        .or_else(|| tparams.get("sandboxPolicy"))
        .cloned()
        .unwrap_or_else(|| panic!("thread/start must carry sandbox; params={}", tparams));
    let sandbox_str = serde_json::to_string(&sandbox).unwrap();
    assert!(
        sandbox_str.contains("read")
            && !sandbox_str.contains("danger-full-access")
            && !sandbox_str.contains("workspace-write"),
        "sandbox must be read-only, got {}",
        sandbox_str
    );
    let sent_cwd = tparams
        .get("cwd")
        .and_then(|c| c.as_str())
        .unwrap_or_else(|| panic!("thread/start params missing cwd: {}", tparams));
    let expected_cwd = std::fs::canonicalize(&cwd).unwrap();
    assert_eq!(std::fs::canonicalize(sent_cwd).unwrap(), expected_cwd);
    assert_eq!(
        tparams.get("modelProvider"),
        Some(&serde_json::json!("openai")),
        "thread/start must pin modelProvider openai"
    );
}

#[tokio::test]
async fn echo_turn_reports_requested_model_effort_and_cwd() {
    let (mut bridge, _cwd, _dir) = spawn_bridge("echo_turn").await;
    let resp = bridge
        .call(&[user_msg("echo please")], &[], None, Some("high"))
        .await
        .expect("echo call should succeed");
    assert_usage_not_double_counted(&resp);
    let echo: serde_json::Value =
        serde_json::from_str(&text_of(&resp)).expect("final text must be JSON echo");
    assert_eq!(echo.get("model"), Some(&serde_json::json!("gpt-5.6-luna")));
    assert_eq!(echo.get("effort"), Some(&serde_json::json!("high")));
}

#[tokio::test]
async fn tool_roundtrip_answers_open_jsonrpc_request_and_completes() {
    let (mut bridge, cwd, _dir) = spawn_bridge("tool_roundtrip").await;
    let tools = [shell_tool()];

    // Turn 1: the pending item/tool/call surfaces as an assistant tool call.
    let first = bridge
        .call(&[user_msg("list the files")], &tools, None, Some("high"))
        .await
        .expect("first call should surface the tool call");
    let choice = first
        .choices
        .as_ref()
        .and_then(|c| c.first())
        .unwrap_or_else(|| panic!("no choice in first response"));
    assert_eq!(choice.finish_reason.as_deref(), Some("tool_calls"));
    let calls = choice
        .message
        .as_ref()
        .and_then(|m| m.tool_calls.as_ref())
        .unwrap_or_else(|| panic!("no tool_calls in first response"));
    assert_eq!(calls.len(), 1, "exactly one tool call expected");
    assert_eq!(calls[0].id.as_deref(), Some("c1"), "callId must be the tool call id");
    assert_eq!(calls[0].function.as_ref().unwrap().name.as_deref(), Some("shell"));
    assert!(
        calls[0].function.as_ref().unwrap().arguments.as_deref().unwrap_or("").contains("ls"),
        "arguments must carry the mock command"
    );

    // Turn 2: hand the Tool-role result back; the bridge must answer the OPEN
    // JSON-RPC request id 71, then report the completed turn.
    let second = bridge
        .call(
            &[
                user_msg("list the files"),
                assistant_tool_call_msg("c1", "shell", r#"{"command":"ls"}"#),
                tool_result_msg("c1", "ls output"),
            ],
            &tools,
            None,
            Some("high"),
        )
        .await
        .expect("second call should complete the turn");
    assert_eq!(
        second.choices.as_ref().unwrap()[0].finish_reason.as_deref(),
        Some("stop")
    );
    let final_text = text_of(&second);
    assert!(
        final_text.contains("tool result was 'ls output'"),
        "final text must reflect the tool output, got: {}",
        final_text
    );
    assert_usage_not_double_counted(&second);

    // Wire: exactly one answer frame, echoing the original request id 71.
    let log = read_log(&cwd);
    let answers: Vec<&serde_json::Value> = log
        .iter()
        .filter(|e| e.get("sent").and_then(|s| s.as_str()) == Some("item/tool/call answer"))
        .collect();
    assert_eq!(answers.len(), 1, "exactly one tool answer expected: {:?}", answers);
    assert_eq!(answers[0].get("id"), Some(&serde_json::json!(71)));
    assert_eq!(answers[0].pointer("/params/success"), Some(&serde_json::json!(true)));
    assert_eq!(
        answers[0].pointer("/params/contentItems/0/type"),
        Some(&serde_json::json!("inputText"))
    );
    assert_eq!(
        answers[0].pointer("/params/contentItems/0/text"),
        Some(&serde_json::json!("ls output"))
    );

    // Same conversation: thread reused, not restarted.
    let thread_starts = log
        .iter()
        .filter(|e| {
            e.get("received").and_then(|f| f.get("method")).and_then(|m| m.as_str())
                == Some("thread/start")
        })
        .count();
    assert_eq!(thread_starts, 1, "same conversation must reuse the thread");
}

#[tokio::test]
async fn second_tool_call_roundtrips_without_deadlock() {
    let (mut bridge, cwd, _dir) = spawn_bridge("tool_two").await;
    let tools = [shell_tool()];

    let first = bridge
        .call(&[user_msg("inspect")], &tools, None, Some("high"))
        .await
        .expect("first call should surface the first tool call");
    let calls1 = first.choices.as_ref().unwrap()[0]
        .message.as_ref().unwrap().tool_calls.as_ref().unwrap();
    assert_eq!(calls1[0].id.as_deref(), Some("c1"));

    // Answer c1: the mock confirms id 71 and immediately asks a SECOND tool
    // call (c2, id 72) inside the same turn. This must not deadlock.
    let second = bridge
        .call(
            &[
                user_msg("inspect"),
                assistant_tool_call_msg("c1", "shell", r#"{"command":"ls"}"#),
                tool_result_msg("c1", "ls output"),
            ],
            &tools,
            None,
            Some("high"),
        )
        .await
        .expect("second call must surface the second tool call");
    assert_eq!(second.choices.as_ref().unwrap()[0].finish_reason.as_deref(), Some("tool_calls"));
    let calls2 = second.choices.as_ref().unwrap()[0]
        .message.as_ref().unwrap().tool_calls.as_ref().unwrap();
    assert_eq!(calls2[0].id.as_deref(), Some("c2"), "second pending call must be c2");
    assert_eq!(calls2[0].function.as_ref().unwrap().name.as_deref(), Some("shell"));

    // Answer c2: the turn completes and the reply text reflects the output.
    let third = bridge
        .call(
            &[
                user_msg("inspect"),
                assistant_tool_call_msg("c1", "shell", r#"{"command":"ls"}"#),
                tool_result_msg("c1", "ls output"),
                assistant_tool_call_msg("c2", "shell", r#"{"command":"cat"}"#),
                tool_result_msg("c2", "cat output"),
            ],
            &tools,
            None,
            Some("high"),
        )
        .await
        .expect("third call should complete the turn");
    assert_eq!(third.choices.as_ref().unwrap()[0].finish_reason.as_deref(), Some("stop"));
    assert!(text_of(&third).contains("cat output"), "final text: {}", text_of(&third));

    // Wire: both answers echoed the ORIGINAL request ids 71 then 72.
    let log = read_log(&cwd);
    let ids: Vec<i64> = log
        .iter()
        .filter(|e| e.get("sent").and_then(|s| s.as_str()) == Some("item/tool/call answer"))
        .filter_map(|e| e.get("id").and_then(|v| v.as_i64()))
        .collect();
    assert_eq!(ids, vec![71, 72], "wire replies must echo ids 71 and 72");
}

#[tokio::test]
async fn apikey_auth_fails_before_thread_start() {
    let (mut bridge, cwd, _dir) = spawn_bridge("auth_apikey").await;
    let err = bridge
        .call(&[user_msg("hello")], &[], None, Some("high"))
        .await
        .expect_err("apiKey billing must be rejected");
    let msg = match err {
        drip::harness::model_call::ModelCallError::Message(m) => m,
        other => panic!("expected Message error, got {:?}", other),
    };
    assert!(msg.contains("chatgpt") || msg.contains("codex login") || msg.contains("apiKey"),
        "auth error must explain the billing problem: {}", msg);

    // Failed BEFORE thread/start: no thread was ever created.
    let log = read_log(&cwd);
    let thread_starts = log
        .iter()
        .filter(|e| {
            e.get("received").and_then(|f| f.get("method")).and_then(|m| m.as_str())
                == Some("thread/start")
        })
        .count();
    assert_eq!(thread_starts, 0, "no thread/start may happen on auth failure");
    assert_request(&log, "account/read");
}

#[tokio::test]
async fn missing_executable_fails_spawn() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = BridgeConfig {
        executable: "definitely-not-a-real-codex-binary-xyz".to_string(),
        args: vec!["app-server".to_string()],
        cwd: Some(dir.path().to_path_buf()),
        model: Some("gpt-5.6-luna".to_string()),
        ..Default::default()
    };
    let err = match CodexBridge::spawn(config).await {
        Err(e) => e,
        Ok(_) => panic!("missing executable must fail spawn"),
    };
    let msg = match err {
        drip::harness::model_call::ModelCallError::Message(m) => m,
        other => panic!("expected Message error, got {:?}", other),
    };
    assert!(
        msg.contains("definitely-not-a-real-codex-binary-xyz"),
        "spawn error must name the executable: {}", msg
    );
}

#[tokio::test]
async fn malformed_response_fails_the_call() {
    // The mock writes one garbage line and exits; the bridge must fail loudly
    // (at handshake during spawn, or on the call itself) with a diagnostic.
    let dir = tempfile::tempdir().expect("tempdir");
    let config = mock_config("malformed", &dir.path().to_path_buf(), 2000);
    let err = match CodexBridge::spawn(config).await {
        Err(e) => e,
        Ok(mut bridge) => bridge
            .call(&[user_msg("hello")], &[], None, Some("high"))
            .await
            .expect_err("malformed stdout must fail the call"),
    };
    match err {
        drip::harness::model_call::ModelCallError::Message(m) => assert!(
            m.contains("malformed") || m.contains("closed stdout") || m.contains("timed out"),
            "diagnostic must explain the malformed stream, got: {}",
            m
        ),
        other => panic!("expected Message error, got {:?}", other),
    }
}

#[tokio::test]
async fn failed_turn_status_surfaces_error() {
    let (mut bridge, _cwd, _dir) = spawn_bridge("turn_failed").await;
    let err = bridge
        .call(&[user_msg("hello")], &[], None, Some("high"))
        .await
        .expect_err("turn/completed status failed must be an error");
    let msg = match err {
        drip::harness::model_call::ModelCallError::Message(m) => m,
        other => panic!("expected Message error, got {:?}", other),
    };
    assert!(
        msg.contains("mock turn exploded") || msg.contains("failed"),
        "error must carry the turn failure, got: {}", msg
    );
}

#[tokio::test]
async fn initialize_timeout_fails_when_handshake_is_unanswered() {
    // no_handshake stays alive but never replies to initialize; the bridge's
    // handshake deadline must fail spawn (killing the child), not hang.
    let dir = tempfile::tempdir().expect("tempdir");
    let config = mock_config("no_handshake", &dir.path().to_path_buf(), 300);
    let started = std::time::Instant::now();
    let err = match CodexBridge::spawn(config).await {
        Err(e) => e,
        Ok(_) => panic!("unanswered initialize must fail spawn"),
    };
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "handshake timeout must bound spawn, took {:?}",
        started.elapsed()
    );
    let msg = match &err {
        drip::harness::model_call::ModelCallError::Message(m) => m.clone(),
        other => panic!("expected bounded handshake failure, got {:?}", other),
    };
    assert!(
        msg.contains("initialize")
            || msg.contains("handshake")
            || msg.contains("timed out")
            || msg.contains("closed stdout"),
        "diagnostic must explain the unanswered handshake: {}",
        msg
    );
}

#[tokio::test]
async fn abort_during_hung_turn_interrupts_and_frees_the_child() {
    let (mut bridge, cwd, _dir) = spawn_bridge("hang_turn").await;
    let signal = AbortSignal::new();

    // The mock accepts turn/start but never completes the turn: the abort
    // signal must break the wait and produce an error.
    let handle = {
        let signal = signal.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            signal.abort();
        })
    };
    let err = bridge
        .call(&[user_msg("hang")], &[], Some(&signal), Some("high"))
        .await
        .expect_err("aborted turn must surface an error");
    let msg = format!("{:?}", err);
    handle.await.expect("aborter task");
    assert!(
        msg.to_lowercase().contains("abort")
            || msg.to_lowercase().contains("stopped")
            || msg.to_lowercase().contains("cancel"),
        "error must reflect the abort, got: {}", msg
    );

    // Wire: drip must have sent turn/interrupt (request or notification).
    let log = read_log(&cwd);
    let interrupts: Vec<&serde_json::Value> = log
        .iter()
        .filter(|e| {
            let received = e.get("received").and_then(|f| f.get("method"));
            let sent = e.get("sent");
            received == Some(&serde_json::json!("turn/interrupt"))
                || sent == Some(&serde_json::json!("turn/interrupt"))
        })
        .collect();
    assert!(
        !interrupts.is_empty(),
        "abort must send turn/interrupt before killing the child"
    );
    // Every interrupt must target the active thread and turn (retries with
    // fresh JSON-RPC ids are allowed while the acknowledgement is bounded).
    for entry in &interrupts {
        let params = entry
            .pointer("/params")
            .cloned()
            .or_else(|| entry.pointer("/received/params").cloned())
            .expect("interrupt must carry params");
        assert_eq!(
            params.get("threadId").and_then(|v| v.as_str()),
            Some("th mock thread id 1")
        );
        assert_eq!(
            params.get("turnId").and_then(|v| v.as_str()),
            Some("turn-1")
        );
    }

    // The child must be gone: dropping the bridge closes stdin, and the mock
    // exits when stdin hits EOF.
    drop(bridge);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let pids = std::process::Command::new("pgrep")
        .args(["-f", "codex_mock.py --script hang_turn"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    assert!(pids.is_empty(), "mock child must be killed after abort, still running: {}", pids);
}

#[tokio::test]
async fn auth_null_rejected_before_thread_start() {
    let (mut bridge, cwd, _dir) = spawn_bridge("auth_null").await;
    let err = bridge
        .call(&[user_msg("hello")], &[], None, Some("high"))
        .await
        .expect_err("null account type must be rejected");
    let msg = match err {
        drip::harness::model_call::ModelCallError::Message(m) => m,
        other => panic!("expected Message error, got {:?}", other),
    };
    let lower = msg.to_lowercase();
    assert!(
        lower.contains("auth")
            || lower.contains("account")
            || lower.contains("chatgpt")
            || lower.contains("login")
            || lower.contains("billing"),
        "auth error must explain the account problem: {}",
        msg
    );

    // Rejected BEFORE thread/start: no thread was ever created.
    let log = read_log(&cwd);
    let thread_starts = log
        .iter()
        .filter(|e| {
            e.get("received").and_then(|f| f.get("method")).and_then(|m| m.as_str())
                == Some("thread/start")
        })
        .count();
    assert_eq!(thread_starts, 0, "no thread/start may happen on null auth");
    assert_request(&log, "account/read");
}

#[tokio::test]
async fn rewritten_same_length_history_starts_fresh_thread_with_replay() {
    let (mut bridge, cwd, _dir) = spawn_bridge("tool_roundtrip").await;
    let tools = [shell_tool()];

    // Turn 1: surface the tool call (turn stays open on thread 1).
    let first = bridge
        .call(&[user_msg("check files")], &tools, None, Some("high"))
        .await
        .expect("first call should surface the tool call");
    assert_eq!(
        first.choices.as_ref().unwrap()[0]
            .message.as_ref().unwrap().tool_calls.as_ref().unwrap()[0]
            .id.as_deref(),
        Some("c1")
    );

    // Turn 2: answer c1; the thread has now seen 3 messages.
    let second = bridge
        .call(
            &[
                user_msg("check files"),
                assistant_tool_call_msg("c1", "shell", r#"{"command":"ls"}"#),
                tool_result_msg("c1", "ls output"),
            ],
            &tools,
            None,
            Some("high"),
        )
        .await
        .expect("tool answer call should complete");
    assert!(
        text_of(&second).contains("tool result was"),
        "text: {}",
        text_of(&second)
    );

    // Same message COUNT but a rewritten first message: the thread must NOT
    // be reused. A fresh thread is started and the full history — including
    // the prior tool output — is replayed into its first turn.
    bridge
        .call(
            &[
                user_msg("check files REWRITTEN"),
                assistant_tool_call_msg("c1", "shell", r#"{"command":"ls"}"#),
                tool_result_msg("c1", "ls output"),
            ],
            &tools,
            None,
            Some("high"),
        )
        .await
        .expect("rewritten-history call should still be served");

    let log = read_log(&cwd);
    let thread_starts = log
        .iter()
        .filter(|e| {
            e.get("received").and_then(|f| f.get("method")).and_then(|m| m.as_str())
                == Some("thread/start")
        })
        .count();
    assert_eq!(
        thread_starts, 2,
        "rewritten same-length history must start a fresh thread"
    );

    let turns: Vec<&serde_json::Value> = log
        .iter()
        .filter(|e| e.get("sent").and_then(|s| s.as_str()) == Some("turn/start"))
        .collect();
    assert_eq!(turns.len(), 2);
    let threads: Vec<&str> = turns
        .iter()
        .filter_map(|e| e.pointer("/params/threadId").and_then(|v| v.as_str()))
        .collect();
    assert_eq!(
        threads,
        vec!["th mock thread id 1", "th mock thread id 2"],
        "second turn must target a NEW thread id"
    );
    let replay = serde_json::to_string(&turns[1]).unwrap();
    assert!(
        replay.contains("check files REWRITTEN"),
        "replay must contain the rewritten history: {}",
        replay
    );
    assert!(
        replay.contains("ls output"),
        "replay must retain the prior tool output: {}",
        replay
    );
}
