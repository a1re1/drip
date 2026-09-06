//! Harness hooks: user-configured shell commands fired at harness lifecycle
//! events, in the spirit of Claude Code's hooks (docs/hooks) and Codex's
//! notification hooks (developers.openai.com/codex/config#hooks).
//!
//! Semantics (MVP):
//! - Hooks are commands from the user's own config, so they run with the
//!   user's trust level — the same trust boundary Claude Code documents.
//! - Commands run via `$SHELL -c` (fallback `/bin/sh`) with the session
//!   working directory as cwd. No DRIP_ALLOW_NET requirement: hooks are
//!   user-configured commands, not harness tool calls.
//! - The payload is a single JSON object on stdin:
//!   `{ event, cwd, tool_name, tool_input, timestamp }` — `tool_*` fields are
//!   only present for `PreToolUse` / `PostToolUse`, and `tool_input` is the
//!   (already-redacted) raw input, truncated.
//! - A hook that exits non-zero, times out, or fails to spawn is logged to
//!   stderr and never blocks the run. (`PreToolUse` exit-code-2 blocking is a
//!   follow-up: it needs an awaitable check at dispatch time.)
//! - Every hook is bounded by `timeout_seconds` (default 10): on expiry the
//!   child is killed and the run continues.

use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Default per-hook timeout. Claude Code documents a 60s ceiling; drip uses a
/// tighter default so hooks never stall a relay round, and it is configurable.
pub const HOOK_DEFAULT_TIMEOUT_SECONDS: u64 = 10;

/// Cap on the `tool_input` string embedded in a hook payload (characters), so
/// a hook that never reads stdin cannot be wedged by a pipe that fills.
const MAX_TOOL_INPUT_CHARS: usize = 24_000;

/// Lifecycle events that can fire hooks. Names mirror Claude Code where an
/// equivalent exists (`PreToolUse`, `PostToolUse`, `SessionStart`, `Stop`);
/// task/loop events are drip's relay lifecycle mapped onto that scheme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookEvent {
    SessionStart,
    LoopStart,
    LoopFinish,
    TaskStart,
    TaskFinish,
    PreToolUse,
    PostToolUse,
    Stop,
}

impl HookEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            HookEvent::SessionStart => "SessionStart",
            HookEvent::LoopStart => "LoopStart",
            HookEvent::LoopFinish => "LoopFinish",
            HookEvent::TaskStart => "TaskStart",
            HookEvent::TaskFinish => "TaskFinish",
            HookEvent::PreToolUse => "PreToolUse",
            HookEvent::PostToolUse => "PostToolUse",
            HookEvent::Stop => "Stop",
        }
    }
}

/// One `PreToolUse`/`PostToolUse` hook with an optional tool-name matcher.
/// Matcher syntax: `|`-separated alternatives with optional trailing `*`
/// wildcards (e.g. `Edit|Write`, `Bash*`); empty/None matches every tool.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct HookMatcher {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matcher: Option<String>,
    pub command: String,
}

impl HookMatcher {
    fn matches(&self, tool_name: Option<&str>) -> bool {
        let Some(name) = tool_name else {
            return false;
        };
        let Some(matcher) = self.matcher.as_deref() else {
            return true;
        };
        let matcher = matcher.trim();
        if matcher.is_empty() {
            return true;
        }
        matcher.split('|').map(str::trim).any(|alternative| {
            if alternative.is_empty() {
                return false;
            }
            match alternative.strip_suffix('*') {
                Some(prefix) => name.starts_with(prefix),
                None => name == alternative,
            }
        })
    }
}

/// The `hooks` section of the CLI config. JSON-compatible (camelCase keys,
/// like `statusLine` in `CliConfig`).
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct HooksConfig {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pre_tool_use: Vec<HookMatcher>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub post_tool_use: Vec<HookMatcher>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub task_start: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub task_finish: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub loop_start: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub loop_finish: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub session_start: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
}

impl HooksConfig {
    pub fn is_empty(&self) -> bool {
        self.pre_tool_use.is_empty()
            && self.post_tool_use.is_empty()
            && self.task_start.is_empty()
            && self.task_finish.is_empty()
            && self.loop_start.is_empty()
            && self.loop_finish.is_empty()
            && self.session_start.is_empty()
            && self.stop.is_empty()
            && self.timeout_seconds.is_none()
    }

    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_seconds.unwrap_or(HOOK_DEFAULT_TIMEOUT_SECONDS))
    }

    /// The commands to run for `event`; `tool_name` filters matcher-style
    /// events and must be `None` for every other event.
    pub fn commands_for(&self, event: HookEvent, tool_name: Option<&str>) -> Vec<String> {
        match event {
            HookEvent::PreToolUse => self
                .pre_tool_use
                .iter()
                .filter(|hook| hook.matches(tool_name))
                .map(|hook| hook.command.clone())
                .collect(),
            HookEvent::PostToolUse => self
                .post_tool_use
                .iter()
                .filter(|hook| hook.matches(tool_name))
                .map(|hook| hook.command.clone())
                .collect(),
            HookEvent::TaskStart => self.task_start.clone(),
            HookEvent::TaskFinish => self.task_finish.clone(),
            HookEvent::LoopStart => self.loop_start.clone(),
            HookEvent::LoopFinish => self.loop_finish.clone(),
            HookEvent::SessionStart => self.session_start.clone(),
            HookEvent::Stop => self.stop.clone(),
        }
    }
}

/// Result of one hook invocation — telemetry/logging only; nothing here can
/// fail the run by construction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookOutcome {
    pub command: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub spawn_failed: bool,
    pub stderr_excerpt: String,
}

impl HookOutcome {
    pub fn succeeded(&self) -> bool {
        !self.spawn_failed && !self.timed_out && self.exit_code == Some(0)
    }

    pub fn describe(&self) -> String {
        if self.spawn_failed {
            format!("spawn failed: {}", self.stderr_excerpt)
        } else if self.timed_out {
            "timed out (killed)".to_string()
        } else {
            format!("exit code {:?}", self.exit_code)
        }
    }
}

/// The JSON payload piped to a hook's stdin. `tool_name`/`tool_input` are only
/// present for tool events; `tool_input` is the raw (already-redacted) input
/// JSON, truncated to `MAX_TOOL_INPUT_CHARS`.
pub fn build_hook_payload(
    event: HookEvent,
    cwd: &str,
    tool: Option<(&str, &str)>,
    timestamp: &str,
) -> String {
    let (tool_name, tool_input) = match tool {
        Some((name, input)) => (Some(name), Some(truncate_tool_input(input))),
        None => (None, None),
    };
    serde_json::json!({
        "event": event.as_str(),
        "cwd": cwd,
        "tool_name": tool_name,
        "tool_input": tool_input,
        "timestamp": timestamp,
    })
    .to_string()
}

/// Run one hook command to completion (or timeout). Blocking by design so it
/// is callable from sync tool-dispatch code; bounded by `timeout` so the run
/// only ever waits that long per hook. stdin/stdout/stderr are piped: stdin
/// gets the payload, stdout is drained and discarded, the first bytes of
/// stderr are kept for failure logs.
pub fn run_hook_command(command: &str, cwd: &str, payload: &str, timeout: Duration) -> HookOutcome {
    let base = HookOutcome {
        command: command.to_string(),
        exit_code: None,
        timed_out: false,
        spawn_failed: false,
        stderr_excerpt: String::new(),
    };
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let mut child = match Command::new(&shell)
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            return HookOutcome {
                spawn_failed: true,
                stderr_excerpt: truncate_for_log(&format!("{error}")),
                ..base
            };
        }
    };

    // Feed the payload from a helper thread: writing blocks once the pipe
    // buffer fills, and a hook that never reads stdin must not wedge the run.
    let stdin = child.stdin.take();
    let payload = payload.to_string();
    let writer = std::thread::spawn(move || {
        if let Some(mut stdin) = stdin {
            let _ = stdin.write_all(payload.as_bytes());
            let _ = stdin.flush();
        }
    });
    // Drain stdout/stderr concurrently so a chatty hook cannot fill the pipe
    // and deadlock itself into a spurious timeout.
    let stdout_pipe = child.stdout.take();
    let stdout_reader = std::thread::spawn(move || drain_capped(stdout_pipe));
    let stderr_pipe = child.stderr.take();
    let stderr_reader = std::thread::spawn(move || drain_capped(stderr_pipe));

    let deadline = Instant::now() + timeout;
    let status: Option<std::process::ExitStatus> = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => break None,
        }
    };

    // The pipes are closed (child exited or was killed), so these join fast.
    let _ = writer.join();
    let _ = stdout_reader.join();
    let stderr_excerpt = stderr_reader.join().unwrap_or_default();

    match status {
        None => HookOutcome {
            timed_out: true,
            ..base
        },
        Some(status) => HookOutcome {
            exit_code: status.code(),
            stderr_excerpt: truncate_for_log(&stderr_excerpt),
            ..base
        },
    }
}

/// Drain a pipe to EOF, keeping at most the first 4 KiB.
fn drain_capped<R: Read>(pipe: Option<R>) -> String {
    let mut pipe = match pipe {
        Some(pipe) => pipe,
        None => return String::new(),
    };
    let mut kept: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if kept.len() < 4096 {
                    kept.extend_from_slice(&chunk[..n]);
                }
            }
        }
    }
    String::from_utf8_lossy(&kept).to_string()
}

fn truncate_tool_input(input: &str) -> String {
    if input.chars().count() <= MAX_TOOL_INPUT_CHARS {
        input.to_string()
    } else {
        let truncated: String = input.chars().take(MAX_TOOL_INPUT_CHARS).collect();
        format!("{truncated}…[truncated]")
    }
}

fn truncate_for_log(text: &str) -> String {
    const MAX_LOG_CHARS: usize = 240;
    if text.chars().count() <= MAX_LOG_CHARS {
        text.to_string()
    } else {
        let truncated: String = text.chars().take(MAX_LOG_CHARS).collect();
        format!("{truncated}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_has_claude_compatible_shape() {
        let payload = build_hook_payload(
            HookEvent::PreToolUse,
            "/tmp/proj",
            Some(("Bash", r#"{"command":"ls"}"#)),
            "2026-01-01T00:00:00.000Z",
        );
        let value: serde_json::Value = serde_json::from_str(&payload).expect("payload is JSON");
        assert_eq!(value["event"], "PreToolUse");
        assert_eq!(value["cwd"], "/tmp/proj");
        assert_eq!(value["tool_name"], "Bash");
        assert_eq!(value["tool_input"], r#"{"command":"ls"}"#);
        assert_eq!(value["timestamp"], "2026-01-01T00:00:00.000Z");
    }

    #[test]
    fn payload_omits_tool_fields_for_lifecycle_events() {
        let payload = build_hook_payload(HookEvent::Stop, "/tmp/proj", None, "t");
        let value: serde_json::Value = serde_json::from_str(&payload).expect("payload is JSON");
        assert_eq!(value["event"], "Stop");
        assert!(value["tool_name"].is_null());
        assert!(value["tool_input"].is_null());
    }

    #[test]
    fn matcher_wildcards_and_alternatives() {
        let hook = |matcher: Option<&str>| HookMatcher {
            matcher: matcher.map(String::from),
            command: "true".to_string(),
        };
        assert!(hook(None).matches(Some("Bash")));
        assert!(hook(None).matches(Some("Edit")));
        assert!(hook(Some("Edit|Write")).matches(Some("Write")));
        assert!(!hook(Some("Edit|Write")).matches(Some("Bash")));
        assert!(hook(Some("Bash*")).matches(Some("Bash")));
        assert!(!hook(Some("Bash*")).matches(Some("Write")));
        assert!(hook(Some(" ")).matches(Some("Bash")));
    }
}
