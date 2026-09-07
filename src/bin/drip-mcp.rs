// bin `drip-mcp` — stdio MCP (Model Context Protocol) server: a thin
// passthrough to the `drip` CLI, so an agent host with Bash disabled can
// still drive drip.

use std::io::Write as _;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio::io::AsyncReadExt as _;
use tokio::process::Command;

// ---------------------------------------------------------------------------
// Help text
// ---------------------------------------------------------------------------

const HELP_TEXT: &str = "\
drip-mcp — stdio MCP server: a thin passthrough to the `drip` CLI

Usage:
  drip-mcp [options]

Options:
  --help, -h     Print this help text and exit
  --version      Print the version and exit

Speaks newline-delimited JSON-RPC 2.0 on stdin/stdout (MCP protocol
version 2024-11-05) and exposes a single tool, `drip`, which runs the
`drip` CLI with the given argv. Long goals should start with --detach,
then poll with --wait --timeout-secs N.

stdout carries only JSON-RPC lines; diagnostics go to stderr.
";

const MCP_PROTOCOL_VERSION: &str = "2024-11-05";
const DEFAULT_TIMEOUT_SECS: u64 = 600;

const DRIP_TOOL_DESCRIPTION: &str = "Run the `drip` CLI with the given argv (no shell) and return its stdout, \
stderr, and exit code. For long goals, start the run with `--detach`, then poll it with \
`--wait --timeout-secs N` instead of blocking on a single call. To run a shell command and get \
back a context-guided distillation of its output instead of the raw stream, use \
[\"--bash\", \"<cmd>\", \"--context\", \"<what you expect / what success and failure look like / what to report>\"].";

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print!("{HELP_TEXT}");
        let _ = std::io::stdout().flush();
        std::process::exit(0);
    }

    if args.iter().any(|arg| arg == "--version") {
        println!("drip-mcp {}", env!("CARGO_PKG_VERSION"));
        std::process::exit(0);
    }

    if let Err(error) = serve() {
        eprintln!("drip-mcp: {error:#}");
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------------------
// Protocol loop
// ---------------------------------------------------------------------------

/// One decoded inbound line: either a reply to write, a parsed tool call to
/// execute, or nothing (notification / silence).
#[derive(Debug)]
enum Incoming {
    None,
    Reply(Value),
    Call {
        id: Value,
        argv: Vec<String>,
        cwd: Option<String>,
        timeout_secs: u64,
    },
}

#[tokio::main]
async fn serve() -> Result<()> {
    // Tokio is built without `io-std`, so pump stdin lines into the runtime
    // from a blocking thread; the channel closes on EOF and the loop exits 0.
    let (line_tx, mut line_rx) = tokio::sync::mpsc::channel::<String>(16);
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut locked = stdin.lock();
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match read_line(&mut locked, &mut buf) {
                Ok(0) => break, // EOF
                Ok(_) => {
                    let line = String::from_utf8_lossy(&buf).into_owned();
                    if line_tx.blocking_send(line).is_err() {
                        break; // server loop dropped
                    }
                }
                Err(error) => {
                    eprintln!("drip-mcp: reading stdin failed: {error}");
                    break;
                }
            }
        }
    });

    // Tool calls run concurrently: each one is spawned onto the runtime so
    // a slow or hung `drip` child never delays a later call past its own
    // `timeout_secs`. Replies may therefore interleave, which JSON-RPC
    // permits (the client matches on `id`); each reply line is written
    // atomically under the stdout lock. Protocol replies are still written
    // inline, in arrival order.
    let mut in_flight = tokio::task::JoinSet::new();
    while let Some(line) = line_rx.recv().await {
        if line.trim().is_empty() {
            continue;
        }
        match parse_line(&line) {
            Incoming::None => {}
            Incoming::Reply(reply) => write_reply(&reply)?,
            Incoming::Call {
                id,
                argv,
                cwd,
                timeout_secs,
            } => {
                in_flight.spawn(async move {
                    let result = run_drip_tool(&argv, cwd.as_deref(), timeout_secs).await;
                    if let Err(error) = write_reply(&json!({"jsonrpc": "2.0", "id": id, "result": result})) {
                        eprintln!("drip-mcp: {error:#}");
                    }
                });
            }
        }
        // Reap finished calls so the set does not grow without bound.
        while in_flight.try_join_next().is_some() {}
    }
    // Stdin closed: let every in-flight call finish and reply before exiting.
    while in_flight.join_next().await.is_some() {}
    Ok(())
}

/// Read one `\n`-terminated line into `buf` (including the newline). Returns
/// bytes read, 0 at EOF. Stdin has no `Read` bound here beyond `std::io::Read`.
fn read_line(input: &mut impl std::io::Read, buf: &mut Vec<u8>) -> std::io::Result<usize> {
    let mut byte = [0u8; 1];
    loop {
        match input.read(&mut byte)? {
            0 => return Ok(if buf.is_empty() { 0 } else { buf.len() }),
            _ => {
                buf.push(byte[0]);
                if byte[0] == b'\n' {
                    return Ok(buf.len());
                }
            }
        }
    }
}

fn write_reply(reply: &Value) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(&mut stdout, reply).context("encoding JSON-RPC reply")?;
    stdout.write_all(b"\n").context("writing JSON-RPC reply")?;
    stdout.flush().context("flushing stdout")?;
    Ok(())
}

fn error_reply(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// Decode one inbound JSON-RPC line. Never touches I/O, so tests can call it
/// directly.
fn parse_line(line: &str) -> Incoming {
    let request: Value = match serde_json::from_str(line) {
        Ok(request) => request,
        Err(error) => return Incoming::Reply(error_reply(Value::Null, -32700, &format!("Parse error: {error}"))),
    };

    // Notifications (no id, or id null) are never answered.
    let id = match request.get("id") {
        Some(id) if !id.is_null() => id.clone(),
        _ => return Incoming::None,
    };

    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    match method {
        "initialize" => Incoming::Reply(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "drip-mcp", "version": env!("CARGO_PKG_VERSION")},
            }
        })),
        "ping" => Incoming::Reply(json!({"jsonrpc": "2.0", "id": id, "result": {}})),
        "tools/list" => Incoming::Reply(json!({"jsonrpc": "2.0", "id": id, "result": tools_list_result()})),
        "tools/call" => parse_tools_call(&request, id),
        _ => Incoming::Reply(error_reply(id, -32601, "Method not found")),
    }
}

fn tools_list_result() -> Value {
    json!({
        "tools": [{
            "name": "drip",
            "description": DRIP_TOOL_DESCRIPTION,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "args": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "argv for the drip CLI, passed verbatim (no shell)",
                    },
                    "cwd": {
                        "type": "string",
                        "description": "working directory (defaults to the server's cwd)",
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "kill the drip process after N seconds (default 600)",
                    },
                },
                "required": ["args"],
            },
        }]
    })
}

fn parse_tools_call(request: &Value, id: Value) -> Incoming {
    let params = request.get("params");
    let name = params.and_then(|p| p.get("name")).and_then(Value::as_str).unwrap_or("");
    if name != "drip" {
        return Incoming::Reply(error_reply(id, -32602, &format!("Unknown tool: {name}")));
    }
    let arguments = params.and_then(|p| p.get("arguments"));

    let argv = match arguments.and_then(|a| a.get("args")) {
        Some(Value::Array(items)) => match items.iter().map(|item| item.as_str().map(str::to_owned)).collect::<Option<Vec<_>>>() {
            Some(argv) => argv,
            None => return Incoming::Reply(error_reply(id, -32602, "Invalid params: `args` must be an array of strings")),
        },
        _ => return Incoming::Reply(error_reply(id, -32602, "Invalid params: `args` (array of strings) is required")),
    };

    let cwd = match arguments.and_then(|a| a.get("cwd")) {
        None | Some(Value::Null) => None,
        Some(Value::String(cwd)) => Some(cwd.clone()),
        Some(_) => return Incoming::Reply(error_reply(id, -32602, "Invalid params: `cwd` must be a string")),
    };

    let timeout_secs = match arguments.and_then(|a| a.get("timeout_secs")) {
        None | Some(Value::Null) => DEFAULT_TIMEOUT_SECS,
        Some(value) => match value.as_u64() {
            Some(secs) if secs >= 1 => secs,
            _ => return Incoming::Reply(error_reply(id, -32602, "Invalid params: `timeout_secs` must be a positive integer")),
        },
    };

    Incoming::Call {
        id,
        argv,
        cwd,
        timeout_secs,
    }
}

// ---------------------------------------------------------------------------
// Tool execution
// ---------------------------------------------------------------------------

fn drip_bin() -> String {
    std::env::var("DRIP_MCP_BIN").unwrap_or_else(|_| "drip".to_string())
}

fn text_content(text: impl Into<String>) -> Value {
    json!({"type": "text", "text": text.into()})
}

fn tool_result(content: Vec<Value>, is_error: bool) -> Value {
    json!({"content": content, "isError": is_error})
}

/// Spawn the `drip` executable with `argv` verbatim (no shell, stdin null),
/// capture stdout/stderr concurrently so a chatty child cannot deadlock on a
/// full pipe, and bound the wait with `timeout_secs`. Tool-level failures
/// (spawn error, non-zero exit, signal, timeout) come back as
/// `result.isError`, never as JSON-RPC protocol errors.
async fn run_drip_tool(argv: &[String], cwd: Option<&str>, timeout_secs: u64) -> Value {
    let started = Instant::now();
    let bin = drip_bin();

    let mut command = Command::new(&bin);
    command
        .args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return tool_result(
                vec![text_content(format!("failed to spawn `{bin}`: {error}"))],
                true,
            );
        }
    };

    // Take the pipes so `child.wait()` below waits only on the process, and
    // drain them concurrently (keeps the `Child` handle alive for kill()).
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let (stdout_tx, stdout_rx) = tokio::sync::oneshot::channel();
    let (stderr_tx, stderr_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let mut buffer = Vec::new();
        if let Some(mut pipe) = stdout_pipe {
            let _ = pipe.read_to_end(&mut buffer).await;
        }
        let _ = stdout_tx.send(buffer);
    });
    tokio::spawn(async move {
        let mut buffer = Vec::new();
        if let Some(mut pipe) = stderr_pipe {
            let _ = pipe.read_to_end(&mut buffer).await;
        }
        let _ = stderr_tx.send(buffer);
    });

    match tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait()).await {
        Err(_elapsed) => {
            // Deadline hit: kill and reap the direct child, but do NOT await
            // the drain tasks — a surviving descendant may hold the pipes
            // open, and waiting on them would hang the server.
            let _ = child.kill().await;
            let _ = child.wait().await;
            let elapsed = started.elapsed().as_secs();
            return tool_result(
                vec![text_content(format!(
                    "timed out: killed `drip` after the configured {timeout_secs}s (elapsed {elapsed}s)"
                ))],
                true,
            );
        }
        Ok(Err(error)) => {
            return tool_result(
                vec![text_content(format!("failed to wait for `{bin}`: {error}"))],
                true,
            );
        }
        Ok(Ok(status)) => {
            // Normal exit: the drains finish once the pipes close.
            let stdout = stdout_rx.await.unwrap_or_default();
            let stderr = stderr_rx.await.unwrap_or_default();

            let mut content = vec![text_content(String::from_utf8_lossy(&stdout).into_owned())];
            if !stderr.is_empty() {
                content.push(text_content(format!("stderr:\n{}", String::from_utf8_lossy(&stderr))));
            }
            match status.code() {
                Some(code) => {
                    content.push(text_content(format!("exit code: {code}")));
                    tool_result(content, code != 0)
                }
                // Killed by a signal: no exit code exists, report it explicitly.
                None => {
                    content.push(text_content(format!(
                        "drip was terminated by a signal ({}); no exit code",
                        signal_name(&status)
                    )));
                    tool_result(content, true)
                }
            }
        }
    }
}

#[cfg(unix)]
fn signal_name(status: &std::process::ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt as _;
    match status.signal() {
        Some(signal) => signal.to_string(),
        None => "unknown".to_string(),
    }
}

#[cfg(not(unix))]
fn signal_name(_status: &std::process::ExitStatus) -> String {
    "unknown".to_string()
}

// ---------------------------------------------------------------------------
// In-file tests: protocol handling, validation, and tool schema (no I/O)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn reply_of(incoming: Incoming) -> Value {
        match incoming {
            Incoming::Reply(reply) => reply,
            other => panic!("expected a reply, got {other:?}"),
        }
    }

    fn error_code(reply: &Value) -> i64 {
        reply["error"]["code"].as_i64().expect("error code")
    }

    #[test]
    fn initialize_returns_protocol_version_and_server_info() {
        let reply = reply_of(parse_line(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#));
        assert_eq!(reply["id"], 1);
        assert_eq!(reply["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(reply["result"]["capabilities"]["tools"], json!({}));
        assert_eq!(reply["result"]["serverInfo"]["name"], "drip-mcp");
        assert_eq!(reply["result"]["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn ping_returns_empty_result() {
        let reply = reply_of(parse_line(r#"{"jsonrpc":"2.0","id":"p","method":"ping"}"#));
        assert_eq!(reply["id"], "p");
        assert_eq!(reply["result"], json!({}));
    }

    #[test]
    fn tools_list_has_one_drip_tool_with_required_args() {
        let reply = reply_of(parse_line(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#));
        let tools = reply["result"]["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "drip");
        assert!(tools[0]["description"].as_str().unwrap().contains("--detach"));
        let schema = &tools[0]["inputSchema"];
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["required"], json!(["args"]));
        assert_eq!(schema["properties"]["args"]["type"], "array");
        assert_eq!(schema["properties"]["args"]["items"]["type"], "string");
        assert_eq!(schema["properties"]["cwd"]["type"], "string");
        assert_eq!(schema["properties"]["timeout_secs"]["type"], "integer");
        assert_eq!(schema["properties"]["timeout_secs"]["minimum"], 1);
    }

    #[test]
    fn unknown_method_with_id_is_32601() {
        let reply = reply_of(parse_line(r#"{"jsonrpc":"2.0","id":9,"method":"resources/list"}"#));
        assert_eq!(error_code(&reply), -32601);
        assert_eq!(reply["error"]["message"], "Method not found");
    }

    #[test]
    fn parse_error_is_32700_with_null_id() {
        let reply = reply_of(parse_line("{not json"));
        assert_eq!(error_code(&reply), -32700);
        assert!(reply["id"].is_null());
    }

    #[test]
    fn notifications_are_ignored() {
        assert!(matches!(
            parse_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#),
            Incoming::None
        ));
        assert!(matches!(
            parse_line(r#"{"jsonrpc":"2.0","method":"unknown/notification","params":{}}"#),
            Incoming::None
        ));
    }

    #[test]
    fn tools_call_unknown_tool_is_32602() {
        let reply = reply_of(parse_line(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"other","arguments":{"args":[]}}}"#,
        ));
        assert_eq!(error_code(&reply), -32602);
    }

    #[test]
    fn tools_call_missing_args_is_32602() {
        let reply = reply_of(parse_line(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"drip","arguments":{}}}"#,
        ));
        assert_eq!(error_code(&reply), -32602);
    }

    #[test]
    fn tools_call_non_string_args_are_32602() {
        let reply = reply_of(parse_line(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"drip","arguments":{"args":["fix",7]}}}"#,
        ));
        assert_eq!(error_code(&reply), -32602);
    }

    #[test]
    fn tools_call_non_string_cwd_is_32602() {
        let reply = reply_of(parse_line(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"drip","arguments":{"args":[],"cwd":5}}}"#,
        ));
        assert_eq!(error_code(&reply), -32602);
    }

    #[test]
    fn tools_call_invalid_timeout_secs_is_32602() {
        for bad in ["0", "\"ten\"", "-1"] {
            let line = format!(
                r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"drip","arguments":{{"args":[],"timeout_secs":{bad}}}}}}}"#
            );
            assert_eq!(error_code(&reply_of(parse_line(&line))), -32602, "timeout_secs={bad}");
        }
    }

    #[test]
    fn tools_call_parses_argv_cwd_and_default_timeout() {
        match parse_line(
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"drip","arguments":{"args":["fix the bug","--json"],"cwd":"/tmp/x"}}}"#,
        ) {
            Incoming::Call {
                id,
                argv,
                cwd,
                timeout_secs,
            } => {
                assert_eq!(id, 4);
                assert_eq!(argv, vec!["fix the bug".to_string(), "--json".to_string()]);
                assert_eq!(cwd.as_deref(), Some("/tmp/x"));
                assert_eq!(timeout_secs, DEFAULT_TIMEOUT_SECS);
            }
            other => panic!("expected a parsed call, got {other:?}"),
        }
    }

    #[test]
    fn tools_call_accepts_explicit_timeout() {
        match parse_line(
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"drip","arguments":{"args":["--detach","long goal"],"timeout_secs":30}}}"#,
        ) {
            Incoming::Call { argv, timeout_secs, .. } => {
                assert_eq!(argv, vec!["--detach".to_string(), "long goal".to_string()]);
                assert_eq!(timeout_secs, 30);
            }
            other => panic!("expected a parsed call, got {other:?}"),
        }
    }

    #[test]
    fn error_reply_keeps_request_id() {
        let reply = error_reply(json!("req-1"), -32601, "Method not found");
        assert_eq!(reply["id"], "req-1");
        assert_eq!(reply["jsonrpc"], "2.0");
    }
}
