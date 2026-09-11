// Integration tests for the MCP client side: a fake stdio MCP server (a
// POSIX sh script in a tempdir) is spawned through `McpClient::spawn`, its
// advertised tools are adapted into ChatToolDefinitions, and calls run
// through `execute_tool_call` exactly as the harness would run them.
//
// Pattern follows tests/mcp_passthrough.rs: no real server binary, a
// watchdog so a client regression cannot hang the suite.
#![cfg(unix)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use drip::chat::types::{
    ChatMessage, ChatMessageBlock, ChatRole, ChatRuntimeContext, ToolCallStatus, WorkingFileContext,
    WorkingFileScope,
};
use drip::tools::async_jobs::{create_chat_tool_runtime_services, CreateChatToolRuntimeServicesOptions};
use drip::tools::execute::{execute_tool_call, ToolExecutionContext};
use drip::tools::mcp::client::McpClient;
use drip::tools::mcp::config::McpServerConfig;
use drip::tools::mcp::mcp_tool_definitions;
use drip::tools::types::ChatToolDefinition;
use tempfile::tempdir;

// ---------------------------------------------------------------------------
// Fake MCP server: replies to initialize, tools/list, and tools/call by
// extracting the request id, method, tool name, and `text` argument with sed.
// Notifications (no id) get no reply. `echo` echoes its `text` argument back;
// `boom` always reports isError.
// ---------------------------------------------------------------------------

const FAKE_SERVER: &str = r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  method=$(printf '%s' "$line" | sed -n 's/.*"method":"\([^"]*\)".*/\1/p')
  case "$method" in
    initialize)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"fake","version":"0"}}}\n' "$id" ;;
    tools/list)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","description":"Echo text back","inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}},{"name":"boom","description":"Always fails","inputSchema":{"type":"object"}}]}}\n' "$id" ;;
    tools/call)
      name=$(printf '%s' "$line" | sed -n 's/.*"name":"\([^"]*\)".*/\1/p')
      if [ "$name" = "hang" ]; then
        : # never replies: the client must hit its call timeout
      elif [ "$name" = "die" ]; then
        exit 0 # EOF before replying: the client must report the exit
      elif [ "$name" = "boom" ]; then
        printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"kaboom"}],"isError":true}}\n' "$id"
      else
        text=$(printf '%s' "$line" | sed -n 's/.*"text":"\([^"]*\)".*/\1/p')
        printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"echo: %s"}]}}\n' "$id" "$text"
      fi ;;
    *) ;;
  esac
done
"#;

struct FakeServer {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

fn fake_server() -> FakeServer {
    let dir = tempdir().expect("create tempdir");
    let path = dir.path().join("fake-mcp-server");
    std::fs::write(&path, format!("#!/bin/sh\n{FAKE_SERVER}\n")).expect("write fake server script");
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod fake server");
    FakeServer { _dir: dir, path }
}

fn server_config(command: &str) -> McpServerConfig {
    McpServerConfig {
        command: command.to_string(),
        timeout_secs: 10,
        ..McpServerConfig::default()
    }
}

/// Runs `body` on a helper thread and fails the test if it takes longer
/// than 30s — a hung handshake must never hang the whole suite.
fn with_watchdog<T: Send + 'static>(body: impl FnOnce() -> T + Send + 'static) -> T {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(body());
    });
    receiver
        .recv_timeout(Duration::from_secs(30))
        .expect("watchdog: MCP client test did not finish within 30s")
}

fn spawn_fake() -> (FakeServer, Vec<ChatToolDefinition>) {
    let fake = fake_server();
    let client = McpClient::spawn("fake", &server_config(fake.path.to_str().expect("fake path")), &PathBuf::from("."))
        .expect("spawn fake MCP server");
    let clients = vec![Arc::new(Mutex::new(client))];
    let tools = mcp_tool_definitions(&clients);
    (fake, tools)
}

fn message() -> ChatMessage {
    ChatMessage {
        blocks: vec![],
        context_files: None,
        context_state: None,
        created_at: None,
        failed: None,
        id: "m1".to_string(),
        pending: None,
        reply_to_message_id: None,
        role: ChatRole::User,
        tags: None,
        transport_state: None,
    }
}

/// Executes `name` with `raw_input` through the harness's tool runner and
/// returns (tool content, failed).
fn run(tools: &[ChatToolDefinition], name: &str, raw_input: &str) -> (String, bool) {
    let cwd = std::env::current_dir().expect("cwd");
    let services = create_chat_tool_runtime_services(CreateChatToolRuntimeServicesOptions {
        cwd: Some(cwd.clone()),
        jobs_root: None,
    });
    let message = message();
    let executed = execute_tool_call(ToolExecutionContext {
        call_id: "c1",
        history: &[],
        message: &message,
        raw_input,
        runtime_context: ChatRuntimeContext {
            cwd: cwd.to_string_lossy().to_string(),
            working_file: WorkingFileContext {
                exists: false,
                path: String::new(),
                scope: WorkingFileScope::Cwd,
                text: None,
            },
        },
        services,
        tool: tools.iter().find(|tool| tool.name == name),
        tool_name: name,
    });
    let failed = executed
        .blocks
        .iter()
        .any(|block| matches!(block, ChatMessageBlock::ToolCall(block) if block.status == ToolCallStatus::Failed));
    (executed.tool_content, failed)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn spawn_lists_the_servers_tools_under_namespaced_names() {
    with_watchdog(|| {
        let (_fake, tools) = spawn_fake();
        let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
        assert_eq!(names, vec!["MCP__fake__echo", "MCP__fake__boom"]);

        let echo = &tools[0];
        assert!(echo.description.starts_with("[mcp:fake] "), "description: {}", echo.description);
        assert!(echo.description.contains("Echo text back"));
        assert!(!echo.mutates_workspace, "MCP tools never count as workspace progress");
        assert!(echo.parameters.properties.contains_key("text"));
        assert_eq!(echo.parameters.required.as_deref(), Some(&["text".to_string()][..]));

        // A bare `{"type":"object"}` schema degrades to an empty object schema.
        let boom = &tools[1];
        assert!(boom.parameters.properties.is_empty());
    });
}

#[test]
fn executing_an_mcp_tool_returns_the_servers_text() {
    with_watchdog(|| {
        let (_fake, tools) = spawn_fake();
        let (content, failed) = run(&tools, "MCP__fake__echo", r#"{"text":"hello from drip"}"#);
        assert!(!failed, "echo must not fail: {content}");
        assert!(content.contains("echo: hello from drip"), "content: {content}");
    });
}

#[test]
fn a_tool_level_is_error_reply_is_a_failed_tool_call() {
    with_watchdog(|| {
        let (_fake, tools) = spawn_fake();
        let (content, failed) = run(&tools, "MCP__fake__boom", "{}");
        assert!(failed, "isError must mark the call failed: {content}");
        assert!(content.contains("kaboom"), "content: {content}");
    });
}

#[test]
fn a_missing_server_binary_is_a_spawn_error_not_a_panic() {
    with_watchdog(|| {
        let error = McpClient::spawn(
            "missing",
            &server_config("/nonexistent/drip-mcp-client-fake-path"),
            &PathBuf::from("."),
        )
        .err()
        .expect("spawn must fail for a missing binary");
        assert!(error.contains("/nonexistent/drip-mcp-client-fake-path"), "error: {error}");
    });
}

#[test]
fn a_silent_server_hits_the_call_timeout_and_the_client_stays_usable() {
    with_watchdog(|| {
        let fake = fake_server();
        let mut config = server_config(fake.path.to_str().expect("fake path"));
        config.timeout_secs = 1;
        let mut client = McpClient::spawn("fake", &config, &PathBuf::from(".")).expect("spawn fake MCP server");
        let started = std::time::Instant::now();
        let error = client
            .call("hang", serde_json::json!({}))
            .err()
            .expect("a call the server never answers must time out");
        assert!(error.contains("timed out after 1s"), "error: {error}");
        assert!(started.elapsed() < Duration::from_secs(10), "timeout must fire near the deadline");
        // The server is still alive and later replies are still matched by id.
        let outcome = client.call("echo", serde_json::json!({"text": "after"})).expect("echo after timeout");
        assert!(!outcome.is_error);
        assert!(outcome.text.contains("echo: after"), "text: {}", outcome.text);
    });
}

#[test]
fn a_server_that_exits_before_replying_is_an_error_not_a_hang() {
    with_watchdog(|| {
        let fake = fake_server();
        let mut client =
            McpClient::spawn("fake", &server_config(fake.path.to_str().expect("fake path")), &PathBuf::from("."))
                .expect("spawn fake MCP server");
        let error = client
            .call("die", serde_json::json!({}))
            .err()
            .expect("a server that exits mid-call must surface an error");
        assert!(error.contains("exited before replying"), "error: {error}");
    });
}
