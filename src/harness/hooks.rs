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
//!   present for `PreToolUse` / `PostToolUse` and for `MemoryWrite`, and
//!   `tool_input` is the (already-redacted) raw input as a JSON string,
//!   truncated. Known divergence: for `PostToolUse` the string carries the
//!   tool's output text, not its input (there is no separate `tool_output`).
//! - A hook that exits non-zero, times out, or fails to spawn is logged to
//!   stderr and never blocks the run — except `PreToolUse` exit code 2, which
//!   blocks the tool call (Claude Code's veto semantics) and feeds the hook's
//!   stderr back to the model.
//! - Every hook is bounded by `timeout_seconds` (default 10): on expiry the
//!   child is killed and the run continues.

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The heuristic that decides whether a tool call "published": git commit or
/// push, or `gh pr create`. Shared by the runner's published-run dirty-tree
/// tracking and the harness's PRReady hook firing so both agree on what a
/// publish attempt is. Best-effort by command text — not a verified PR.
pub(crate) fn git_publish_pattern() -> &'static Regex {
    static PATTERN: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| Regex::new(r"\bgit\s+(commit|push)\b|\bgh\s+pr\s+create\b").unwrap())
}

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
    /// drip-specific: a relay round (one model call + tool dispatch) started.
    RelayStart,
    /// drip-specific: a relay round finished (any outcome, including abort).
    RelayFinish,
    /// drip-specific: the harness memory bank was written (remember/forget).
    MemoryWrite,
    /// drip-specific: the run published a PR (git commit/push, `gh pr create`).
    PRReady,
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
            HookEvent::RelayStart => "RelayStart",
            HookEvent::RelayFinish => "RelayFinish",
            HookEvent::MemoryWrite => "MemoryWrite",
            HookEvent::PRReady => "PRReady",
        }
    }
}

/// One `PreToolUse`/`PostToolUse` hook with an optional tool-name matcher.
/// Matcher syntax: `|`-separated alternatives with optional trailing `*`
/// wildcards (e.g. `PATCH|READ`, `BASH*`); matching is case-sensitive
/// against canonical uppercase tool names, and empty/None matches every tool.
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
    /// drip-specific: fires at the start of each relay round.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub relay_start: Vec<String>,
    /// drip-specific: fires when a relay round ends (any outcome).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub relay_finish: Vec<String>,
    /// drip-specific: fires when the memory bank is written (remember/forget).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub memory_write: Vec<String>,
    /// drip-specific: fires when the run publishes a PR.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pr_ready: Vec<String>,
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
            && self.relay_start.is_empty()
            && self.relay_finish.is_empty()
            && self.memory_write.is_empty()
            && self.pr_ready.is_empty()
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
            HookEvent::RelayStart => self.relay_start.clone(),
            HookEvent::RelayFinish => self.relay_finish.clone(),
            HookEvent::MemoryWrite => self.memory_write.clone(),
            HookEvent::PRReady => self.pr_ready.clone(),
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

/// The JSON payload piped to a hook's stdin. `tool_name`/`tool_input` are
/// present for tool events and for `MemoryWrite`; `tool_input` is the raw
/// (already-redacted) input as a JSON string, truncated to
/// `MAX_TOOL_INPUT_CHARS`. For `PostToolUse` the string carries the tool's
/// output text rather than its input (no separate `tool_output` field).
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
    let mut child = match unsafe {
        Command::new(&shell)
            .arg("-c")
            .arg(command)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // The child leads its own process group (done in the child itself
            // to avoid the classic setpgid race) so a timeout can kill the
            // whole tree — the shell AND any backgrounded grandchildren that
            // inherited the pipes.
            .pre_exec(|| {
                // Best effort: failure just means the fallback child.kill()
                // path below still applies to the shell itself.
                let _ = libc::setpgid(0, 0);
                Ok(())
            })
            .spawn()
    } {
        Ok(child) => child,
        Err(error) => {
            return HookOutcome {
                spawn_failed: true,
                stderr_excerpt: truncate_for_log(&format!("{error}")),
                ..base
            };
        }
    };

    // The child's process group id equals its pid (set via unsafe_pre_exec
    // above); killing the group reaps the shell plus any grandchildren that
    // inherited the pipes. Without this, a grandchild holding stdout/stderr
    // keeps the drain threads from ever seeing EOF and the run hangs.
    let pgid = child.id();

    // Feed the payload from a helper thread: writing blocks once the pipe
    // buffer fills, and a hook that never reads stdin must not wedge the run.
    // Threads poll a done-flag and give up once the hook is finished/dead, so
    // a grandchild holding the write end cannot block the run's joins.
    let done = Arc::new(AtomicBool::new(false));
    let stdin = child.stdin.take();
    let payload = payload.to_string();
    let writer_done = Arc::clone(&done);
    let (writer_tx, writer_rx) = std::sync::mpsc::channel::<()>();
    let writer = std::thread::spawn(move || {
        if let Some(mut stdin) = stdin {
            // Poll instead of blocking in write_all forever: if the hook
            // finished without reading stdin (or died), stop writing and
            // close the pipe so the child's read side sees EOF.
            let chunks = payload.as_bytes();
            let mut offset = 0;
            while offset < chunks.len() {
                if writer_done.load(Ordering::Relaxed) {
                    break;
                }
                match stdin.write(&chunks[offset..]) {
                    Ok(0) => break,
                    Ok(n) => offset += n,
                    Err(_) => break,
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let _ = stdin.flush();
        }
        // `stdin` drops here, closing the pipe so a `cat`-style hook sees EOF.
        let _ = writer_tx.send(());
    });
    // Drain stdout/stderr concurrently so a chatty hook cannot fill the pipe
    // and deadlock itself into a spurious timeout. Each thread reports its
    // captured output through a channel so the run can collect it with a
    // deadline instead of an unconditional blocking join.
    let stdout_done = Arc::clone(&done);
    let stdout_pipe = child.stdout.take();
    let (stdout_tx, stdout_rx) = std::sync::mpsc::channel::<String>();
    let stdout_reader = std::thread::spawn(move || {
        let _ = stdout_tx.send(drain_capped(stdout_pipe, &stdout_done));
    });
    let stderr_done = Arc::clone(&done);
    let stderr_pipe = child.stderr.take();
    let (stderr_tx, stderr_rx) = std::sync::mpsc::channel::<String>();
    let stderr_reader = std::thread::spawn(move || {
        let _ = stderr_tx.send(drain_capped(stderr_pipe, &stderr_done));
    });

    let deadline = Instant::now() + timeout;
    let status: Option<std::process::ExitStatus> = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    kill_process_group(pgid);
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => break None,
        }
    };

    // The direct child is reaped. Signal the pipe threads to stop and kill
    // any grandchildren still holding the pipe ends so EOF arrives promptly.
    // Completion is then collected with a deadline, NEVER an unconditional
    // blocking join: a reader blocked inside read(2) cannot observe the
    // done-flag, so a descendant that escaped the process group (or survived
    // the kill, e.g. under sandboxed signal denial) must not wedge the run
    // past its bound. Stragglers finish in the background; their pipes close
    // when they eventually exit.
    done.store(true, Ordering::Relaxed);
    kill_process_group(pgid);

    let drain_grace = Duration::from_millis(250);
    let _ = writer_rx.recv_timeout(drain_grace);
    let _stdout_excerpt = stdout_rx.recv_timeout(drain_grace).unwrap_or_default();
    let stderr_excerpt = stderr_rx.recv_timeout(drain_grace).unwrap_or_default();
    // Detach any straggler pipe threads: dropping the JoinHandle is safe —
    // a detached thread only owns its pipe end, never the run's progress.
    drop(writer);
    drop(stdout_reader);
    drop(stderr_reader);

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

/// Kill a whole process group (SIGTERM, then SIGKILL for stragglers). Used
/// both on timeout and after the direct child exits: a backgrounded
/// grandchild that inherited the pipes would otherwise keep the drain
/// threads from ever seeing EOF. Best effort — a race with process exit is
/// harmless (`ESRCH` is ignored); a killed group can be momentarily a zombie.
fn kill_process_group(pgid: u32) {
    unsafe {
        if libc::kill(pgid as libc::pid_t, libc::SIGTERM) != 0 {
            // Group already gone (or we raced) — nothing more to do.
            let _ = libc::kill(pgid as libc::pid_t, libc::SIGKILL);
            return;
        }
        // Give the group a short grace period to exit on SIGTERM before
        // escalating to SIGKILL; keeps flushing hooks graceful in practice.
        for _ in 0..20 {
            if libc::kill(pgid as libc::pid_t, 0) != 0 {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        libc::kill(pgid as libc::pid_t, libc::SIGKILL);
    }
}

/// Drain a pipe to EOF, keeping at most the first 4 KiB. Bails out early when
/// `done` flips — so a grandchild that inherited the pipe and never closes it
/// cannot wedge the reader thread past the hook's deadline.
fn drain_capped<R: Read>(pipe: Option<R>, done: &AtomicBool) -> String {
    let mut pipe = match pipe {
        Some(pipe) => pipe,
        None => return String::new(),
    };
    let mut kept: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        if done.load(Ordering::Relaxed) {
            break;
        }
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

    #[test]
    fn run_hook_command_success_and_failure_exit_codes() {
        let payload = "{}";
        let ok = run_hook_command("cat > /dev/null; exit 0", ".", payload, Duration::from_secs(5));
        assert_eq!(ok.exit_code, Some(0));
        assert!(ok.succeeded());

        let failed =
            run_hook_command("cat > /dev/null; exit 3", ".", payload, Duration::from_secs(5));
        assert_eq!(failed.exit_code, Some(3));
        assert!(!failed.succeeded());

        // 2 is the PreToolUse veto code: it must be observable on the
        // outcome so execute_workspace_tool can block the call.
        let veto = run_hook_command("exit 2", ".", payload, Duration::from_secs(5));
        assert_eq!(veto.exit_code, Some(2));
        assert!(!veto.succeeded());
    }

    #[test]
    fn run_hook_command_reports_spawn_failure() {
        let outcome = run_hook_command(
            "definitely-not-a-real-command-xyz",
            ".",
            "{}",
            Duration::from_secs(5),
        );
        // The user shell reports command-not-found as a nonzero exit (or the
        // spawn itself fails); either way the hook must not look successful.
        assert!(!outcome.succeeded());
    }

    #[test]
    fn run_hook_command_times_out_and_kills() {
        let started = std::time::Instant::now();
        let outcome = run_hook_command("sleep 5", ".", "{}", Duration::from_millis(150));
        assert!(outcome.timed_out);
        assert!(!outcome.succeeded());
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn run_hook_command_times_out_on_compound_command() {
        // A compound command defeats the shell's exec-optimization for a bare
        // command, so the shell itself outlives the deadline. The group kill
        // plus bounded drain must still return promptly (regression: without
        // process-group cleanup this could hang well past timeout_seconds).
        let started = std::time::Instant::now();
        let outcome = run_hook_command("echo hi; sleep 5", ".", "{}", Duration::from_millis(150));
        assert!(outcome.timed_out);
        assert!(!outcome.succeeded());
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "compound timeout took {:?}; bounded-drain regression",
            started.elapsed()
        );
    }

    #[test]
    fn run_hook_command_survives_background_grandchild_holding_pipes() {
        // The shell exits quickly but leaves a backgrounded grandchild holding
        // stdout/stderr. The hook must return on the shell's own exit via the
        // bounded drain grace, never waiting out the grandchild.
        let started = std::time::Instant::now();
        let outcome = run_hook_command("sleep 5 & echo done", ".", "{}", Duration::from_secs(5));
        assert!(outcome.succeeded());
        assert!(!outcome.timed_out);
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "drain collection blocked on a pipe-holding grandchild ({:?})",
            started.elapsed()
        );
    }

    #[test]
    fn run_hook_command_large_payload_unread_stdin_does_not_wedge() {
        // A 1 MiB payload to a hook that never reads stdin and outlives the
        // deadline: the writer thread must stop on the done-flag and the call
        // must return bounded instead of blocking in write/read.
        let payload = "x".repeat(1024 * 1024);
        let started = std::time::Instant::now();
        let outcome = run_hook_command("sleep 2", ".", &payload, Duration::from_millis(150));
        assert!(outcome.timed_out);
        assert!(!outcome.succeeded());
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "unread-stdin payload wedge ({:?})",
            started.elapsed()
        );
    }

    #[test]
    fn payload_shape_for_drip_specific_events() {
        let memory = build_hook_payload(
            HookEvent::MemoryWrite,
            "/tmp/proj",
            Some(("remember", r#"{"note":"hi"}"#)),
            "t",
        );
        let value: serde_json::Value = serde_json::from_str(&memory).expect("payload is JSON");
        assert_eq!(value["event"], "MemoryWrite");
        assert_eq!(value["tool_name"], "remember");
        assert_eq!(value["tool_input"], r#"{"note":"hi"}"#);

        for event in [HookEvent::RelayStart, HookEvent::RelayFinish, HookEvent::PRReady] {
            let payload = build_hook_payload(event, "/tmp/proj", None, "t");
            let value: serde_json::Value = serde_json::from_str(&payload).expect("payload is JSON");
            assert!(value["tool_name"].is_null());
            assert!(value["tool_input"].is_null());
        }
    }

    #[test]
    fn commands_for_drip_specific_events() {
        let config = HooksConfig {
            relay_start: vec!["echo relay-start".into()],
            relay_finish: vec!["echo relay-finish".into()],
            memory_write: vec!["echo memory".into()],
            pr_ready: vec!["echo pr".into()],
            ..Default::default()
        };
        assert_eq!(
            config.commands_for(HookEvent::RelayStart, None),
            vec!["echo relay-start".to_string()]
        );
        assert_eq!(
            config.commands_for(HookEvent::RelayFinish, None),
            vec!["echo relay-finish".to_string()]
        );
        assert_eq!(
            config.commands_for(HookEvent::MemoryWrite, None),
            vec!["echo memory".to_string()]
        );
        assert_eq!(
            config.commands_for(HookEvent::PRReady, None),
            vec!["echo pr".to_string()]
        );
        assert!(!config.is_empty());
    }

    /// Regression for the P1 timeout finding: a compound foreground command
    /// ("echo; sleep 5") cannot be exec-optimized away by the shell, so the
    /// old kill-only-the-shell path plus unconditional joins would stall the
    /// run past the deadline. The whole run must stay bounded.
    #[test]
    fn run_hook_command_bounds_compound_foreground_sleep() {
        let started = std::time::Instant::now();
        let outcome = run_hook_command("echo hi; sleep 5", ".", "{}", Duration::from_millis(150));
        assert!(outcome.timed_out);
        assert!(!outcome.succeeded());
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    /// Regression for the P1 finding: the shell exits 0 but a backgrounded
    /// grandchild keeps stdout open, so the old code joined the drain threads
    /// for the grandchild's full lifetime. The group kill must reap the
    /// grandchild and return promptly. (Uses a bounded 10s sleep so a
    /// regression fails this test rather than wedging CI forever.)
    #[test]
    fn run_hook_command_returns_when_backgrounded_grandchild_holds_pipes() {
        let started = std::time::Instant::now();
        let outcome = run_hook_command("sleep 10 & echo done", ".", "{}", Duration::from_millis(500));
        assert_eq!(outcome.exit_code, Some(0));
        assert!(outcome.succeeded());
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    /// Regression: a payload larger than the pipe buffer fed to a hook that
    /// never reads stdin must not wedge the writer thread — the run stays
    /// bounded by the deadline and the timeout kill.
    #[test]
    fn run_hook_command_survives_oversized_payload_with_unread_stdin() {
        let started = std::time::Instant::now();
        let payload = "x".repeat(2 * 1024 * 1024);
        let outcome = run_hook_command("sleep 2", ".", &payload, Duration::from_millis(500));
        assert!(outcome.timed_out);
        assert!(!outcome.succeeded());
        assert!(started.elapsed() < Duration::from_secs(8));
    }
}
