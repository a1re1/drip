// Integration tests for `drip-mcp`: newline-delimited JSON-RPC over stdio,
// exercised end-to-end with a fake `drip` executable so no real CLI runs.
//
// Pattern follows tests/statusline_example.rs: spawn the server binary,
// pipe requests into its stdin, close stdin, and read the replies back.
#![cfg(unix)]

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::tempdir;

// ---------------------------------------------------------------------------
// Fake `drip` executable (a small shell script in a tempdir)
// ---------------------------------------------------------------------------

/// A fake `drip` executable: a POSIX shell script whose body is `body`.
/// The tempdir is kept alive so the script path stays valid for the server.
struct FakeDrip {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

fn fake_drip(body: &str) -> FakeDrip {
    let dir = tempdir().expect("create tempdir");
    let path = dir.path().join("drip");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write fake drip script");
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod fake drip");
    FakeDrip { _dir: dir, path }
}

/// Fake that echoes its argv one argument per line, so verbatim-argv
/// assertions can compare the tool's stdout content against the exact args.
fn fake_drip_echo_argv() -> FakeDrip {
    fake_drip("for arg in \"$@\"; do echo \"$arg\"; done")
}

// ---------------------------------------------------------------------------
// Server driver: spawn drip-mcp, write one request per line, close stdin
// ---------------------------------------------------------------------------

struct ServerOutcome {
    status: std::process::ExitStatus,
    stdout: String,
}

/// Spawn `drip-mcp` with the given extra env (per-child, never process
/// global), feed it `requests`, close stdin (EOF), and wait — with a
/// watchdog so a server regression cannot hang the whole suite.
fn run_server(requests: &[String], env: &[(&str, &str)]) -> ServerOutcome {
    let mut command = Command::new(env!("CARGO_BIN_EXE_drip-mcp"));
    command.env_remove("DRIP_MCP_BIN");
    for (key, value) in env {
        command.env(key, value);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn drip-mcp");

    {
        let mut stdin = child.stdin.take().expect("server stdin");
        for request in requests {
            stdin.write_all(request.as_bytes()).expect("write request");
            stdin.write_all(b"\n").expect("write newline");
        }
        stdin.flush().expect("flush requests");
    } // stdin dropped -> EOF -> the server exits 0

    let mut stdout = child.stdout.take().expect("server stdout");
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        text
    });

    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        match child.try_wait().expect("poll drip-mcp") {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("watchdog: drip-mcp did not exit within 30s");
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };

    let stdout = reader.join().unwrap_or_default();
    ServerOutcome { status, stdout }
}

/// Parse every non-empty stdout line as a JSON-RPC value (fails the test on
/// any line that is not valid JSON).
fn parse_lines(stdout: &str) -> Vec<Value> {
    stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("stdout line not valid JSON-RPC: {error:?}\nline: {line}"))
        })
        .collect()
}

fn error_code(reply: &Value) -> i64 {
    reply["error"]["code"].as_i64().unwrap_or_else(|| panic!("no error code in {reply}"))
}

fn content_texts(result: &Value) -> Vec<&str> {
    result["content"]
        .as_array()
        .expect("content array")
        .iter()
        .map(|item| item["text"].as_str().expect("text content"))
        .collect()
}

fn request(id: &str, method: &str) -> String {
    format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{{}}}}"#)
}

fn tools_call(id: i64, arguments: &str) -> String {
    format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"drip","arguments":{arguments}}}}}"#)
}

// ---------------------------------------------------------------------------
// (a) initialize → tools/list: one tool named `drip` with `args` required
// ---------------------------------------------------------------------------

#[test]
fn initialize_then_tools_list_has_one_drip_tool_with_required_args() {
    let outcome = run_server(
        &[request("1", "initialize"), request("2", "tools/list")],
        &[],
    );
    assert!(outcome.status.success(), "server must exit 0 on EOF");

    let replies = parse_lines(&outcome.stdout);
    assert_eq!(replies.len(), 2, "exactly two replies, got: {}", outcome.stdout);

    let initialize = &replies[0];
    assert_eq!(initialize["id"], 1);
    assert_eq!(initialize["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(initialize["result"]["capabilities"]["tools"], serde_json::json!({}));
    assert_eq!(initialize["result"]["serverInfo"]["name"], "drip-mcp");

    let tools = replies[1]["result"]["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 1, "exactly one tool");
    assert_eq!(tools[0]["name"], "drip");
    assert!(tools[0]["description"].as_str().expect("description").contains("--detach"));
    let schema = &tools[0]["inputSchema"];
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["required"], serde_json::json!(["args"]));
    assert_eq!(schema["properties"]["args"]["items"]["type"], "string");
    assert_eq!(schema["properties"]["timeout_secs"]["minimum"], 1);
}

// ---------------------------------------------------------------------------
// (b) tools/call passes argv verbatim (spaces, empty string, quotes,
// metacharacters) and returns exit code 0 with isError:false
// ---------------------------------------------------------------------------

#[test]
fn tools_call_passes_argv_verbatim() {
    let fake = fake_drip_echo_argv();
    let arguments = r#"{"args":["goal with spaces","","quotes 'and \"double\"'","$HOME `id` ; | &","--json"]}"#;
    let outcome = run_server(
        &[tools_call(7, arguments)],
        &[("DRIP_MCP_BIN", fake.path.to_str().expect("fake path"))],
    );
    assert!(outcome.status.success());

    let replies = parse_lines(&outcome.stdout);
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0]["id"], 7);
    let result = &replies[0]["result"];
    assert_eq!(result["isError"], false, "exit 0 must not be an error");

    // The fake echoes one argument per line; compare the full argv verbatim.
    let stdout_text = content_texts(result)[0];
    let echoed: Vec<&str> = stdout_text.lines().collect();
    assert_eq!(
        echoed,
        vec![
            "goal with spaces",
            "",
            "quotes 'and \"double\"'",
            "$HOME `id` ; | &",
            "--json",
        ]
    );
    assert!(content_texts(result).iter().any(|text| *text == "exit code: 0"));
}

// ---------------------------------------------------------------------------
// (c) non-zero exit → isError:true and "exit code: 3" present
// ---------------------------------------------------------------------------

#[test]
fn tools_call_nonzero_exit_is_tool_error() {
    let fake = fake_drip("echo boom >&2\nexit \"${FAKE_EXIT:-0}\"");
    let outcome = run_server(
        &[tools_call(8, r#"{"args":["goal"]}"#)],
        &[
            ("DRIP_MCP_BIN", fake.path.to_str().expect("fake path")),
            ("FAKE_EXIT", "3"),
        ],
    );
    assert!(outcome.status.success(), "tool-level failure is not a server failure");

    let replies = parse_lines(&outcome.stdout);
    assert_eq!(replies.len(), 1);
    let result = &replies[0]["result"];
    assert_eq!(result["isError"], true);
    let texts = content_texts(result);
    assert!(
        texts.iter().any(|text| *text == "exit code: 3"),
        "missing exit code text in {texts:?}"
    );
    assert!(
        texts.iter().any(|text| text.contains("stderr:\nboom")),
        "non-empty stderr must be reported in {texts:?}"
    );
}

// ---------------------------------------------------------------------------
// (d) unknown method with an id → error code -32601
// ---------------------------------------------------------------------------

#[test]
fn unknown_method_is_32601() {
    let outcome = run_server(&[request("9", "resources/list")], &[]);
    assert!(outcome.status.success());

    let replies = parse_lines(&outcome.stdout);
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0]["id"], 9);
    assert_eq!(error_code(&replies[0]), -32601);
    assert_eq!(replies[0]["error"]["message"], "Method not found");
}

// ---------------------------------------------------------------------------
// (e) timeout_secs:1 against a fake that sleeps 5 → tool error within ~3s;
// the fake keeps `sleep` as a forked shell child holding the inherited
// pipes, so a regression that awaits the drains would hang instead.
// ---------------------------------------------------------------------------

#[test]
fn tools_call_timeout_kills_child_and_reports_quickly() {
    let fake = fake_drip("sleep 5 & wait $!");
    let started = Instant::now();
    let outcome = run_server(
        &[tools_call(11, r#"{"args":["slow goal"],"timeout_secs":1}"#)],
        &[("DRIP_MCP_BIN", fake.path.to_str().expect("fake path"))],
    );
    let elapsed = started.elapsed();
    assert!(outcome.status.success());

    assert!(
        elapsed < Duration::from_secs(3),
        "server must not wait out the 5s child (or hang on its pipes); took {elapsed:?}"
    );
    let replies = parse_lines(&outcome.stdout);
    assert_eq!(replies.len(), 1);
    let result = &replies[0]["result"];
    assert_eq!(result["isError"], true);
    let texts = content_texts(result);
    assert!(
        texts.iter().any(|text| text.contains("timed out") && text.contains("1s")),
        "timeout explanation missing in {texts:?}"
    );
}

// ---------------------------------------------------------------------------
// Focused coverage: malformed JSON + recovery, notification silence,
// validation errors, spawn failure, cwd, stderr omission, ordering
// ---------------------------------------------------------------------------

#[test]
fn malformed_json_gets_32700_then_server_recovers() {
    let outcome = run_server(&["{not json".to_string(), request("2", "initialize")], &[]);
    assert!(outcome.status.success());
    let replies = parse_lines(&outcome.stdout);
    assert_eq!(replies.len(), 2);
    assert_eq!(error_code(&replies[0]), -32700);
    assert!(replies[0]["id"].is_null(), "parse error id must be null");
    assert_eq!(replies[1]["result"]["protocolVersion"], "2024-11-05");
}

#[test]
fn notifications_are_silent() {
    let outcome = run_server(
        &[
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#.to_string(),
            r#"{"jsonrpc":"2.0","id":null,"method":"tools/list"}"#.to_string(),
            request("3", "ping"),
        ],
        &[],
    );
    assert!(outcome.status.success());
    let replies = parse_lines(&outcome.stdout);
    assert_eq!(replies.len(), 1, "notifications must produce no reply");
    assert_eq!(replies[0]["id"], 3);
}

#[test]
fn tools_call_unknown_tool_is_32602() {
    let outcome = run_server(
        &[r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"other","arguments":{"args":[]}}}"#.to_string()],
        &[],
    );
    let replies = parse_lines(&outcome.stdout);
    assert_eq!(replies.len(), 1);
    assert_eq!(error_code(&replies[0]), -32602);
}

#[test]
fn tools_call_missing_args_is_32602() {
    let outcome = run_server(
        &[r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"drip","arguments":{}}}"#.to_string()],
        &[],
    );
    let replies = parse_lines(&outcome.stdout);
    assert_eq!(replies.len(), 1);
    assert_eq!(error_code(&replies[0]), -32602);
}

#[test]
fn tools_call_non_string_args_is_32602() {
    let outcome = run_server(&[tools_call(4, r#"{"args":["goal",7]}"#)], &[]);
    let replies = parse_lines(&outcome.stdout);
    assert_eq!(replies.len(), 1);
    assert_eq!(error_code(&replies[0]), -32602);
}

#[test]
fn tools_call_nonexistent_executable_is_tool_error() {
    let outcome = run_server(
        &[tools_call(12, r#"{"args":["goal"]}"#)],
        &[("DRIP_MCP_BIN", "/nonexistent/drip-mcp-fake-path")],
    );
    assert!(outcome.status.success(), "spawn failure must not kill the server");
    let replies = parse_lines(&outcome.stdout);
    assert_eq!(replies.len(), 1);
    let result = &replies[0]["result"];
    assert_eq!(result["isError"], true);
    assert!(content_texts(result).iter().any(|text| {
        text.contains("failed to spawn") && text.contains("/nonexistent/drip-mcp-fake-path")
    }));
}

#[test]
fn tools_call_runs_in_explicit_cwd() {
    let fake = fake_drip("pwd -P");
    let dir = tempdir().expect("cwd tempdir");
    let cwd = dir.path().canonicalize().expect("canonical cwd");
    let arguments = format!(
        r#"{{"args":[],"cwd":{}}}"#,
        serde_json::to_string(cwd.to_str().expect("cwd str")).expect("json cwd")
    );
    let outcome = run_server(
        &[tools_call(14, &arguments)],
        &[("DRIP_MCP_BIN", fake.path.to_str().expect("fake path"))],
    );
    assert!(outcome.status.success());
    let replies = parse_lines(&outcome.stdout);
    assert_eq!(replies.len(), 1);
    assert_eq!(
        content_texts(&replies[0]["result"])[0].trim(),
        cwd.to_str().expect("cwd str"),
        "fake must run inside the requested cwd"
    );
}

#[test]
fn tools_call_empty_stderr_is_omitted() {
    let fake = fake_drip("echo out only");
    let outcome = run_server(
        &[tools_call(13, r#"{"args":["goal"]}"#)],
        &[("DRIP_MCP_BIN", fake.path.to_str().expect("fake path"))],
    );
    let replies = parse_lines(&outcome.stdout);
    let result = &replies[0]["result"];
    assert_eq!(result["isError"], false);
    let texts = content_texts(result);
    assert_eq!(texts.len(), 2, "stdout + exit code only, got {texts:?}");
    assert_eq!(texts[0], "out only\n");
    assert_eq!(texts[1], "exit code: 0");
}

#[test]
fn every_request_gets_exactly_one_reply_matched_by_id() {
    let fake = fake_drip("echo seq");
    let outcome = run_server(
        &[
            request(r#""a""#, "ping"),
            tools_call(1, r#"{"args":["one"]}"#),
            request(r#""b""#, "ping"),
            tools_call(2, r#"{"args":["two"]}"#),
        ],
        &[("DRIP_MCP_BIN", fake.path.to_str().expect("fake path"))],
    );
    assert!(outcome.status.success());
    let replies = parse_lines(&outcome.stdout);
    assert_eq!(replies.len(), 4, "one reply per request");
    // Protocol replies are written inline in arrival order; tool replies may
    // interleave with them but must still be present and matched by id.
    let ids: Vec<String> = replies.iter().map(|reply| reply["id"].to_string()).collect();
    assert!(ids.iter().position(|id| id == "\"a\"") < ids.iter().position(|id| id == "\"b\""));
    for id in [1, 2] {
        let reply = replies.iter().find(|reply| reply["id"] == id).unwrap_or_else(|| panic!("no reply for id {id}"));
        assert_eq!(reply["result"]["content"][0]["text"], "seq\n");
    }
}

// A slow call must not hold up a later fast call: the fast call's reply
// lands first, and the server still waits for the slow call before exiting.
#[test]
fn slow_tool_call_does_not_block_a_later_call() {
    let fake = fake_drip(r#"if [ "$1" = slow ]; then sleep 2; fi; echo "$1""#);
    let started = Instant::now();
    let outcome = run_server(
        &[
            tools_call(1, r#"{"args":["slow"],"timeout_secs":30}"#),
            tools_call(2, r#"{"args":["fast"]}"#),
        ],
        &[("DRIP_MCP_BIN", fake.path.to_str().expect("fake path"))],
    );
    let elapsed = started.elapsed();
    assert!(outcome.status.success());
    assert!(elapsed >= Duration::from_secs(2), "server must wait for the in-flight slow call; took {elapsed:?}");
    let replies = parse_lines(&outcome.stdout);
    assert_eq!(replies.len(), 2);
    assert_eq!(replies[0]["id"], 2, "fast call replies first: {replies:?}");
    assert_eq!(replies[0]["result"]["content"][0]["text"], "fast\n");
    assert_eq!(replies[1]["id"], 1);
    assert_eq!(replies[1]["result"]["content"][0]["text"], "slow\n");
}

// ---------------------------------------------------------------------------
// --help / -h / --version (no stdio protocol involved)
// ---------------------------------------------------------------------------

fn run_args(args: &[&str]) -> (std::process::ExitStatus, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_drip-mcp"))
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .expect("run drip-mcp with args");
    (output.status, String::from_utf8_lossy(&output.stdout).into_owned())
}

#[test]
fn help_prints_usage_and_exits_zero() {
    for flag in ["--help", "-h"] {
        let (status, stdout) = run_args(&[flag]);
        assert!(status.success(), "{flag} must exit 0");
        assert!(stdout.contains("drip-mcp"), "{flag} output should name the binary");
    }
}

#[test]
fn version_prints_name_and_pkg_version() {
    let (status, stdout) = run_args(&["--version"]);
    assert!(status.success());
    assert_eq!(stdout.trim(), format!("drip-mcp {}", env!("CARGO_PKG_VERSION")));
}

