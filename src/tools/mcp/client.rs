// The MCP client side: spawn a stdio MCP server, run the initialize
// handshake, list its tools, and route `tools/call` requests to it. The
// framing mirrors the `drip-mcp` bin — newline-delimited JSON-RPC 2.0 over
// the child's stdin/stdout, replies matched by integer id, notifications
// (no id) skipped on the floor.
//
// Everything here is best-effort: a dead, malformed, or slow server comes
// back as an Err(String) that callers warn about and move past — it never
// fails a run.

use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::tools::mcp::config::McpServerConfig;

// The protocol version drip speaks as a client; the `drip-mcp` server
// advertises the same one.
pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

// How often the wait loop re-checks for a reply while burning down a timeout.
const REPLY_POLL_INTERVAL: Duration = Duration::from_millis(15);

// One tool advertised by a server in its `tools/list` reply.
#[derive(Debug, Clone)]
pub struct McpToolInfo {
    pub name: String,
    pub description: String,
    // The raw JSON schema object. The adapter normalizes a missing or
    // non-object schema into an empty object schema.
    pub input_schema: Value,
}

// The outcome of a `tools/call`: the concatenated text content plus the
// server's own isError flag (a tool-level failure is still a protocol
// success, not a transport error).
#[derive(Debug, Clone)]
pub struct McpCallOutcome {
    pub text: String,
    pub is_error: bool,
}

// A live connection to one MCP server. `spawn` completes the full
// initialize/initialized/tools/list handshake before returning, so a client
// always carries a settled tool list. The child is killed on drop.
pub struct McpClient {
    name: String,
    child: Child,
    stdin: Mutex<ChildStdin>,
    // Replies the reader thread has seen but no caller has claimed yet, keyed
    // by JSON-RPC id. Calls are synchronous, but replies may still arrive out
    // of order or after a timeout gave up, so matching happens through this
    // map rather than against the pipe directly.
    replies: Arc<Mutex<HashMap<u64, Value>>>,
    // False once the reader thread saw EOF on the child's stdout.
    output_open: Arc<AtomicBool>,
    next_id: AtomicU64,
    timeout: Duration,
    tools: Vec<McpToolInfo>,
}

impl McpClient {
    // Spawns `server` in `cwd` and completes the MCP handshake. Any failure
    // (binary missing, initialize timeout, malformed reply) is an Err; a
    // server that advertises no tools is a valid, if useless, client. The
    // child, if it ever started, is killed before returning so no orphan is
    // left behind.
    pub fn spawn(name: &str, server: &McpServerConfig, cwd: &Path) -> Result<McpClient, String> {
        let timeout = Duration::from_secs(server.timeout_secs);
        let mut command = Command::new(&server.command);
        command
            .args(&server.args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // The server's diagnostics belong to the run's stderr, not to a
            // tool result.
            .stderr(Stdio::inherit());
        for (key, value) in &server.env {
            command.env(key, value);
        }

        let mut child = command
            .spawn()
            .map_err(|error| format!("spawn {}: {error}", server.command))?;
        // Both pipes were requested above, so this cannot fail in practice;
        // if it ever does, reap the started child instead of leaking it.
        let (stdin, stdout) = match (child.stdin.take(), child.stdout.take()) {
            (Some(stdin), Some(stdout)) => (stdin, stdout),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("spawn {}: stdio pipes unavailable", server.command));
            }
        };

        let replies: Arc<Mutex<HashMap<u64, Value>>> = Arc::new(Mutex::new(HashMap::new()));
        let output_open = Arc::new(AtomicBool::new(true));
        spawn_reader(stdout, Arc::clone(&replies), Arc::clone(&output_open));

        let mut client = McpClient {
            name: name.to_string(),
            child,
            stdin: Mutex::new(stdin),
            replies,
            output_open,
            next_id: AtomicU64::new(1),
            timeout,
            tools: Vec::new(),
        };

        let result = client.initialize();
        if let Err(error) = result {
            client.kill_child();
            return Err(error);
        }

        // The `notifications/initialized` ack. No reply is expected; a
        // server that has already exited just makes the later tools/list
        // fail, which is the honest report anyway.
        let _ = client.send_raw(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
        }));

        let tools = client.list_tools()?;
        client.tools = tools;
        Ok(client)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    // The tool list settled during the handshake.
    pub fn tools(&self) -> &[McpToolInfo] {
        &self.tools
    }

    fn initialize(&mut self) -> Result<(), String> {
        let reply = self.request(
            "initialize",
            json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "drip", "version": env!("CARGO_PKG_VERSION")},
            }),
        )?;
        if reply.get("result").is_none() {
            return Err(format!(
                "initialize: malformed reply: {}",
                summary(&reply)
            ));
        }
        Ok(())
    }

    fn list_tools(&mut self) -> Result<Vec<McpToolInfo>, String> {
        let reply = self.request("tools/list", json!({}))?;
        let result = reply
            .get("result")
            .ok_or_else(|| format!("tools/list: no result in {}", summary(&reply)))?;
        let Some(tools) = result.get("tools").and_then(Value::as_array) else {
            return Err(format!("tools/list: no tools array in {}", summary(&reply)));
        };
        Ok(tools
            .iter()
            .filter_map(|tool| {
                let name = tool.get("name").and_then(Value::as_str)?;
                Some(McpToolInfo {
                    name: name.to_string(),
                    description: tool
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    input_schema: tool.get("inputSchema").cloned().unwrap_or(Value::Null),
                })
            })
            .collect())
    }

    // Sends `tools/call` and folds `result.content[]` text entries into one
    // string, honouring the server's isError flag. Transport failures
    // (timeout, dead server) are Err; tool-level failures come back with
    // `is_error: true`.
    pub fn call(&mut self, tool: &str, arguments: Value) -> Result<McpCallOutcome, String> {
        let reply = self.request(
            "tools/call",
            json!({"name": tool, "arguments": arguments}),
        )?;
        let result = reply
            .get("result")
            .ok_or_else(|| format!("tools/call {tool}: no result in {}", summary(&reply)))?;
        let mut text = String::new();
        if let Some(content) = result.get("content").and_then(Value::as_array) {
            for item in content {
                if item.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(chunk) = item.get("text").and_then(Value::as_str) {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(chunk);
                    }
                }
            }
        }
        let is_error = result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Ok(McpCallOutcome { text, is_error })
    }

    // One JSON-RPC request/response round trip: send, then wait for the
    // matching id within `self.timeout`. A timeout or a dead server is an
    // Err.
    fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.send_raw(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))?;
        self.wait_for_reply(id, method)
    }

    fn send_raw(&mut self, message: &Value) -> Result<(), String> {
        let mut stdin = self
            .stdin
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        serde_json::to_writer(&mut *stdin, message)
            .map_err(|error| format!("encode {message}: {error}"))?;
        stdin
            .write_all(b"\n")
            .and_then(|()| stdin.flush())
            .map_err(|error| format!("write to MCP server: {error}"))
    }

    fn wait_for_reply(&self, id: u64, method: &str) -> Result<Value, String> {
        let deadline = Instant::now() + self.timeout;
        loop {
            if let Some(reply) = self.replies.lock().unwrap_or_else(PoisonError::into_inner).remove(&id) {
                if let Some(error) = reply.get("error") {
                    return Err(format!(
                        "{method}: MCP error {}: {}",
                        error
                            .get("code")
                            .and_then(Value::as_i64)
                            .map(|code| code.to_string())
                            .unwrap_or_else(|| "?".to_string()),
                        error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown error"),
                    ));
                }
                return Ok(reply);
            }
            if !self.output_open.load(Ordering::SeqCst) {
                // The reader thread saw EOF: the server is gone and the id
                // will never arrive.
                return Err(format!("{method}: MCP server exited before replying"));
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "{method}: timed out after {}s waiting for the MCP server",
                    self.timeout.as_secs()
                ));
            }
            std::thread::sleep(REPLY_POLL_INTERVAL);
        }
    }

    // Kill the child and wait on it so no zombie lingers. Safe to call
    // twice; Drop delegates here.
    fn kill_child(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        self.kill_child();
    }
}

// Reads the child's stdout line by line on a background thread, filing every
// id-bearing reply into `replies` and clearing `output_open` at EOF. Lines
// that are not JSON or carry no id (server-initiated notifications, logs)
// are dropped.
fn spawn_reader(
    stdout: std::process::ChildStdout,
    replies: Arc<Mutex<HashMap<u64, Value>>>,
    output_open: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = Vec::new();
        loop {
            line.clear();
            match read_line(&mut reader, &mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let Ok(message) = serde_json::from_slice::<Value>(&line) else {
                        continue;
                    };
                    let Some(id) = message.get("id").and_then(Value::as_u64) else {
                        continue; // notification: no id, skipped
                    };
                    if replies
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .insert(id, message)
                        .is_some()
                    {
                        // A duplicate id is a protocol violation; the newer
                        // reply wins, which matches last-write semantics.
                        continue;
                    }
                }
            }
        }
        output_open.store(false, Ordering::SeqCst);
    });
}

// One `\n`-terminated line read into `buf` (newline included), byte by byte
// so it works on any `Read`. Returns bytes read, 0 at EOF. Mirrors the
// `read_line` helper in the `drip-mcp` bin.
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

// A short, single-line rendering of a reply for error messages: never dumps
// a full (possibly huge) JSON blob into the run log.
fn summary(value: &Value) -> String {
    // Truncate on a char boundary: the reply is untrusted UTF-8 and a byte
    // slice through a multibyte character would panic inside an error path.
    let rendered = value.to_string();
    match rendered.char_indices().nth(200) {
        Some((cut, _)) => format!("{}…", &rendered[..cut]),
        None => rendered,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // read_line returns one newline-terminated chunk per call and 0 at EOF,
    // even for a final line without a trailing newline.
    #[test]
    fn read_line_splits_on_newlines_and_handles_eof() {
        let mut input: &[u8] = b"{\"id\":1}\n{\"id\":2}";
        let mut buf = Vec::new();
        assert_eq!(read_line(&mut input, &mut buf).unwrap(), 9);
        assert_eq!(buf.as_slice(), b"{\"id\":1}\n");
        buf.clear();
        assert_eq!(read_line(&mut input, &mut buf).unwrap(), 8);
        assert_eq!(buf.as_slice(), b"{\"id\":2}");
        buf.clear();
        assert_eq!(read_line(&mut input, &mut buf).unwrap(), 0);
    }
}
