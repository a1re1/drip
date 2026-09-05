//! Bounded asynchronous runner for the user-configured status line command.
//!
//! The `statusLine` setting (see `src/core/config.rs`) names an explicit shell
//! command. This module owns everything about running it: the JSON payload on
//! stdin, the worker thread, the timeout/group-kill, output capture and
//! sanitization into a single display row.
//!
//! Trust boundary: the command comes from drip's own persisted config
//! (`~/.drip/config.json`) and is run as local code, exactly like a
//! `statusLine` in Claude Code. Nothing here reads or executes `~/.claude`
//! settings.
//!
//! Payload contract (single JSON object on stdin, stdin closed after write):
//! {
//!   "session_id":        string | null,
//!   "workspace":         { "current_dir": string | null },
//!   "model":             { "id": string | null, "display_name": string | null },
//!   "version":           string | null,
//!   "render_width_chars": number,
//!   "context_usage":     number | null   // 0..1, null until real telemetry exists
//! }
//!
//! Unavailable telemetry is sent as JSON null (or omitted for object fields)
//! rather than guessed, and that is documented in README.md by task-4.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::core::config::StatusLineSetting;
use crate::tools::child_env::build_child_process_env;
use crate::tools::child_process::{run_captured_process, CapturedProcessArgs};
use crate::watch::ansi::{char_width};

/// Upper bound on captured stdout, so a chatty command cannot balloon the TUI.
pub const STATUS_LINE_MAX_OUTPUT_CHARS: usize = 8_192;

/// Minimum spacing between two spawned jobs, independent of the configured
/// interval, so a fast interval plus a slow command can never overlap.
const MIN_REFRESH_SPACING_MS: u64 = 50;

/// Everything the worker needs to run one job.
#[derive(Debug, Clone)]
pub struct StatusLineRequest {
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub model_id: Option<String>,
    pub model_display_name: Option<String>,
    pub version: Option<String>,
    pub render_width_chars: usize,
    /// 0..=1, only when genuinely measured; `None` serializes as null.
    pub context_usage: Option<f64>,
}

/// One finished job as delivered back to the TUI.
#[derive(Debug, Clone, PartialEq)]
pub struct StatusLineOutput {
    /// Sanitized, width-normalized display row (SGR colors may remain).
    pub line: String,
    /// True when the row came from a successful run (cached lines stay true).
    pub ok: bool,
    /// True when this output was produced by the current request generation.
    pub fresh: bool,
    /// When the underlying command actually completed.
    pub finished_at: Instant,
}

impl StatusLineOutput {

}

/// Serialized stdin payload (kept public for the payload tests and README).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct StatusLinePayload<'a> {
    pub session_id: Option<&'a str>,
    pub workspace: StatusLineWorkspace<'a>,
    pub model: StatusLineModel<'a>,
    pub version: Option<&'a str>,
    pub render_width_chars: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_usage: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct StatusLineWorkspace<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_dir: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct StatusLineModel<'a> {
    #[serde(skip_serializing_if="Option::is_none")]
    pub id: Option<&'a str>,
    #[serde(skip_serializing_if="Option::is_none")]
    pub display_name: Option<&'a str>,
}

impl<'a> StatusLinePayload<'a> {
    pub fn from_request(request: &'a StatusLineRequest) -> Self {
        Self {
            session_id: request.session_id.as_deref(),
            workspace: StatusLineWorkspace {
                current_dir: request.cwd.as_deref(),
            },
            model: StatusLineModel {
                id: request.model_id.as_deref(),
                display_name: request.model_display_name.as_deref(),
            },
            version: request.version.as_deref(),
            render_width_chars: request.render_width_chars,
            context_usage: request.context_usage,
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
    }
}

/// One queued job sent to the worker thread.
struct StatusLineJob {
    request: StatusLineRequest,
    generation: u64,
}

/// Shared handle the TUI owns for the lifetime of the feature.
///
/// At most one job runs at any time (`in_flight` guards overlap); a refresh
/// requested while a job runs is coalesced into `pending` and picked up by the
/// worker when it finishes, so rapid state changes never queue a storm.
pub struct StatusLineRunner {
    setting: StatusLineSetting,
    job_tx: Option<Sender<StatusLineJob>>,
    output_rx: Receiver<StatusLineOutput>,
    last_started: Arc<Mutex<Option<Instant>>>,
    in_flight: Arc<AtomicBool>,
    pending: Arc<Mutex<Option<StatusLineRequest>>>,
    generation: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
}

impl StatusLineRunner {
    /// Builds a runner and starts its single worker thread. Nothing is
    /// executed until the first `request_refresh`.
    pub fn new(setting: StatusLineSetting) -> Self {
        let (job_tx, job_rx) = mpsc::channel::<StatusLineJob>();
        let (out_tx, out_rx) = mpsc::channel::<StatusLineOutput>();
        let in_flight = Arc::new(AtomicBool::new(false));
        let pending: Arc<Mutex<Option<StatusLineRequest>>> = Arc::new(Mutex::new(None));
        let generation = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let last_started: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));

        let worker = StatusLineWorker {
            setting: setting.clone(),
            job_rx,
            job_tx: job_tx.clone(),
            out_tx,
            in_flight: Arc::clone(&in_flight),
            pending: Arc::clone(&pending),
            generation: Arc::clone(&generation),
            stop: Arc::clone(&stop),
        };
        std::thread::Builder::new()
            .name("status-line".to_string())
            .spawn(move || worker.run())
            .expect("spawn status-line worker");

        Self {
            setting,
            job_tx: Some(job_tx),
            output_rx: out_rx,
            last_started,
            in_flight,
            pending,
            generation,
            stop,
        }
    }

    pub fn setting(&self) -> &StatusLineSetting {
        &self.setting
    }

    /// True while a job is running (never blocks).
    pub fn job_running(&self) -> bool {
        self.in_flight.load(Ordering::SeqCst)
    }

    /// Asks for a refresh. Returns true when a job was started now; returns
    /// false (and coalesces the request) when one is already running or the
    /// minimum spacing between starts has not elapsed. Never blocks.
    pub fn request_refresh(&mut self, request: StatusLineRequest) -> bool {
        if self.stop.load(Ordering::SeqCst) {
            return false;
        }
        if self.job_running() {
            if let Ok(mut pending) = self.pending.lock() {
                *pending = Some(request);
            }
            return false;
        }
        if let Ok(last) = self.last_started.lock() {
            if let Some(started) = *last {
                if started.elapsed() < Duration::from_millis(MIN_REFRESH_SPACING_MS) {
                    if let Ok(mut pending) = self.pending.lock() {
                        *pending = Some(request);
                    }
                    return false;
                }
            }
        }
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        if let Ok(mut last) = self.last_started.lock() {
            *last = Some(Instant::now());
        }
        self.in_flight.store(true, Ordering::SeqCst);
        match &self.job_tx {
            Some(tx) => tx.send(StatusLineJob { request, generation }).is_ok(),
            None => {
                self.in_flight.store(false, Ordering::SeqCst);
                false
            }
        }
    }

    /// Drains the newest finished job, if any (non-blocking). Older results
    /// that queued up behind a newer one are dropped.
    pub fn poll_output(&self) -> Option<StatusLineOutput> {
        let mut newest = None;
        while let Ok(output) = self.output_rx.try_recv() {
            newest = Some(output);
        }
        newest
    }

    /// Stops accepting work; the worker exits after the in-flight job
    /// finishes (bounded by the command timeout plus slack).
    pub fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.job_tx = None;
    }
}

impl Drop for StatusLineRunner {
    fn drop(&mut self) {
        // Stop the worker on exit: the worker holds its own job-channel
        // sender, so dropping the runner alone would not close the channel.
        // An in-flight job still finishes, bounded by its own timeout.
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// The single worker thread: serialized jobs, one at a time, forever.
struct StatusLineWorker {
    setting: StatusLineSetting,
    job_rx: Receiver<StatusLineJob>,
    job_tx: Sender<StatusLineJob>,
    out_tx: Sender<StatusLineOutput>,
    in_flight: Arc<AtomicBool>,
    pending: Arc<Mutex<Option<StatusLineRequest>>>,
    generation: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
}

impl StatusLineWorker {
    fn run(mut self) {
        loop {
            if self.stop.load(Ordering::SeqCst) {
                return;
            }
            let job = match self.job_rx.recv_timeout(Duration::from_millis(250)) {
                Ok(job) => job,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => return,
            };

            let result = run_status_line_job(&self.setting, &job.request);

            // Only the newest generation may publish; a superseded job's
            // output is stale by definition.
            let fresh =
                job.generation == self.generation.load(Ordering::SeqCst);
            let output = match result {
                Some(mut output) => {
                    output.fresh = fresh;
                    output
                }
                None => StatusLineOutput {
                    line: String::new(),
                    ok: false,
                    fresh,
                    finished_at: Instant::now(),
                },
            };

            // Deliver every finished job, successes and failures alike: the
            // TUI replaces the custom row with the built-in bar whenever the
            // newest result failed, timed out, or was blank (the documented
            // fallback), so a stale success is never kept visible.
            self.finish_job(output);
        }
    }

    fn finish_job(&self, output: StatusLineOutput) {
        let _ = self.out_tx.send(output);
        // Pick up a request that arrived while this job ran; it becomes the
        // next job without another wakeup from the TUI.
        let next = self.pending.lock().ok().and_then(|mut guard| guard.take());
        self.in_flight.store(false, Ordering::SeqCst);
        if let Some(request) = next {
            if !self.stop.load(Ordering::SeqCst) {
                let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
                let _ = self.job_tx.send(StatusLineJob { request, generation });
                self.in_flight.store(true, Ordering::SeqCst);
            }
        }
    }
}

/// Expands a leading `~/` (or bare `~`) to the user's home directory.
///
/// The home value comes from the environment — `USERPROFILE` on Windows with
/// `HOME` as fallback, `HOME` elsewhere — and tests never mutate it; the pure
/// logic lives in [`expand_home_with`].
pub fn expand_home(command: &str) -> String {
    let primary = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    let home = std::env::var_os(primary)
        .or_else(|| std::env::var_os("HOME"))
        .map(|home| home.to_string_lossy().into_owned());
    expand_home_with(command, home.as_deref())
}

/// Pure core of [`expand_home`]: the looked-up `home` value (or `None` when
/// unset) is injected, so tests never touch the process environment.
fn expand_home_with(command: &str, home: Option<&str>) -> String {
    let trimmed = command.trim_start();
    if trimmed == "~" || trimmed.starts_with("~/") {
        if let Some(home) = home {
            if trimmed == "~" {
                return home.to_string();
            }
            return format!("{}/{}", home.trim_end_matches('/'), &trimmed[2..]);
        }
    }
    command.to_string()
}

fn shell_for_platform() -> (&'static str, Vec<String>) {
    if cfg!(windows) {
        ("cmd", vec!["/C".to_string()])
    } else {
        ("/bin/sh", vec!["-c".to_string()])
    }
}

/// Runs one job synchronously (worker thread only). Returns `None` when the
/// command could not be spawned at all; a spawned command that fails, times
/// out or prints nothing yields a defined fallback instead.
pub fn run_status_line_job(
    setting: &StatusLineSetting,
    request: &StatusLineRequest,
) -> Option<StatusLineOutput> {
    let command = expand_home(&setting.command);
    if command.trim().is_empty() {
        return None;
    }
    let (shell, shell_args) = shell_for_platform();
    let mut process_args = shell_args;
    process_args.push(command);

    let env = build_child_process_env(None);
    // Session JSON payload goes to the command's stdin; stdin is closed right
    // after, so scripts reading stdin observe EOF (see CapturedProcessArgs).
    let payload = StatusLinePayload::from_request(request).to_json();
    let args = CapturedProcessArgs {
        command: shell,
        cwd: request.cwd.as_deref(),
        env: Some(&env),
        process_args: &process_args,
        timeout_ms: Some(setting.timeout_ms),
        stdin_payload: Some(payload.as_str()),
    };

    let result = run_captured_process(&args).ok()?;
    // Completion instant: when the command actually finished (success,
    // failure or timeout), not when the job started.
    let finished_at = Instant::now();
    let timed_out = result.timed_out;
    // Exit 127 means the shell could not find the configured command at all;
    // there is nothing to display, so report "no output" instead of a
    // permanently blank failed row.
    if !timed_out && result.exit_code == Some(127) {
        return None;
    }
    let ok = !timed_out && result.exit_code == Some(0);

    // Cap captured output before any processing.
    let raw: String = result.stdout.chars().take(STATUS_LINE_MAX_OUTPUT_CHARS).collect();
    // Content-only sanitization: width fitting and configured padding happen
    // at render time (the terminal width changes without re-running the job).
    let line = sanitize_status_line(raw.trim(), 0, 0);

    Some(StatusLineOutput {
        line,
        ok,
        fresh: true,
        finished_at,
    })
}

// ---------------------------------------------------------------------------
// Output sanitization
// ---------------------------------------------------------------------------

const SGR_RESET: &str = "\x1b[0m";

/// Collapses captured stdout into one display row.
///
/// - Only the first line of the output is used.
/// - OSC sequences (window title etc.), cursor movement and other escapes,
///   and all other control characters are dropped.
/// - SGR color sequences (`ESC [ ... m`) are retained so scripts can color
///   their output; an SGR reset is appended when any color was emitted so the
///   row cannot bleed into the rest of the TUI.
/// - Visible width is measured Unicode-aware (combining marks are zero-width,
///   East-Asian wide characters are two cells) and truncated to `width`.
/// - `padding` (the configured 0-4) is applied on both sides inside `width`;
///   when `width` < 2*padding+1 the padding is clamped so the row never
///   exceeds `width`.
pub fn sanitize_status_line(raw: &str, width: usize, padding: u16) -> String {
    let first_line = raw.lines().next().unwrap_or("");
    let pad_requested = padding.min(4) as usize;
    // Clamp the padding so a nonzero `width` smaller than 2*padding+1 still
    // fits: the padded row is exactly `width` visible cells, never more.
    // `width == 0` keeps the content-only mode (no padding, no truncation).
    let pad = pad_requested.min(width.saturating_sub(1) / 2);
    let inner = if width == 0 {
        usize::MAX
    } else {
        width.saturating_sub(2 * pad).max(1)
    };

    let mut out = String::new();
    let mut visible = 0usize;
    let mut saw_sgr = false;
    let mut chars = first_line.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    let mut seq = String::from("\x1b[");
                    while let Some(&next) = chars.peek() {
                        seq.push(next);
                        chars.next();
                        if next.is_ascii_alphabetic() || seq.len() > 64 {
                            break;
                        }
                    }
                    // Keep only SGR (color) sequences; drop cursor movement,
                    // erase-in-display and every other CSI form.
                    if seq.ends_with('m') {
                        saw_sgr = true;
                        out.push_str(&seq);
                    }
                }
                Some(']') => {
                    // OSC: consume through the terminator (BEL or ESC \).
                    chars.next();
                    let mut last_was_esc = false;
                    for next in chars.by_ref() {
                        if next == '\x07' {
                            break;
                        }
                        if last_was_esc && next == '\\' {
                            break;
                        }
                        last_was_esc = next == '\x1b';
                    }
                }
                _ => {
                    // Lone ESC or two-character escape: drop.
                }
            }
            continue;
        }
        if c.is_control() {
            continue;
        }
        let cw = char_width(c as u32);
        if visible + cw > inner {
            break;
        }
        visible += cw;
        out.push(c);
    }

    if width == 0 {
        if saw_sgr {
            out.push_str(SGR_RESET);
        }
        return out;
    }

    // Normalize to exactly `inner` visible cells, then apply the padding.
    let mut padded = String::with_capacity(out.len() + 2 * pad);
    padded.push_str(&" ".repeat(pad));
    padded.push_str(&out);
    let trailing = inner.saturating_sub(visible);
    padded.push_str(&" ".repeat(trailing));
    padded.push_str(&" ".repeat(pad));
    // End the row with a reset when any color was emitted so it cannot bleed
    // into the rest of the TUI.
    if saw_sgr {
        padded.push_str(SGR_RESET);
    }
    padded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_serializes_available_fields_and_omits_nulls() {
        let request = StatusLineRequest {
            session_id: Some("sess-1234".to_string()),
            cwd: Some("/tmp/work".to_string()),
            model_id: Some("claude-opus-4-6".to_string()),
            model_display_name: Some("Opus 5 (1M context)".to_string()),
            version: Some("2.1.261".to_string()),
            render_width_chars: 120,
            context_usage: Some(0.35),
        };
        let payload = StatusLinePayload::from_request(&request);
        let json = payload.to_json();
        assert!(json.contains(r#""session_id":"sess-1234""#));
        assert!(json.contains(r#""workspace":{"current_dir":"/tmp/work"}"#));
        assert!(json.contains(r#""model":{"id":"claude-opus-4-6""#));
        assert!(json.contains(r#""display_name":"Opus 5 (1M context)""#));
        assert!(json.contains(r#""version":"2.1.261""#));
        assert!(json.contains(r#""render_width_chars":120"#));
        assert!(json.contains(r#""context_usage":0.35"#));
    }

    #[test]
    fn payload_uses_null_for_unavailable_telemetry() {
        let request = StatusLineRequest {
            session_id: None,
            cwd: None,
            model_id: None,
            model_display_name: None,
            version: None,
            render_width_chars: 80,
            context_usage: None,
        };
        let json = StatusLinePayload::from_request(&request).to_json();
        assert!(json.contains(r#""session_id":null"#));
        assert!(json.contains(r#""version":null"#));
        assert!(!json.contains("current_dir"));
        assert!(!json.contains("display_name"));
        assert!(!json.contains("context_usage"));
    }

    fn setting(command: &str) -> StatusLineSetting {
        StatusLineSetting {
            kind: "command".to_string(),
            command: command.to_string(),
            padding: 0,
            update_interval_ms: 300,
            timeout_ms: 5_000,
        }
    }

    fn request(width: usize) -> StatusLineRequest {
        StatusLineRequest {
            session_id: Some("s".to_string()),
            cwd: None,
            model_id: None,
            model_display_name: None,
            version: None,
            render_width_chars: width,
            context_usage: None,
        }
    }

    #[test]
    fn expand_home_uses_injected_home_value() {
        let home = Some("/Users/tester");
        assert_eq!(
            expand_home_with("~/bin/status.sh", home),
            "/Users/tester/bin/status.sh"
        );
        assert_eq!(expand_home_with("~", home), "/Users/tester");
        assert_eq!(expand_home_with(" /bin/echo ok", home), " /bin/echo ok");
        assert_eq!(expand_home_with("a~b", home), "a~b");
        assert_eq!(expand_home_with("echo ~", home), "echo ~");
        assert_eq!(expand_home_with("~x/rel", home), "~x/rel");
    }

    #[test]
    fn expand_home_with_unset_home_leaves_command_alone() {
        assert_eq!(expand_home_with("~/bin/status.sh", None), "~/bin/status.sh");
        assert_eq!(expand_home_with("~", None), "~");
        assert_eq!(expand_home_with("/bin/echo ok", None), "/bin/echo ok");
    }

    #[test]
    fn expand_home_public_fn_reads_environment_without_mutating_it() {
        // Pure pass-through: whatever the process env holds (or not), the
        // public wrapper must not mutate it and must fall back gracefully.
        // `unset` / `USERPROFILE`-only shapes are covered by the pure core
        // tests above; here we only pin that the wrapper is env-read-only.
        let before = std::env::var_os("HOME");
        let _ = expand_home("~/bin/status.sh");
        assert_eq!(std::env::var_os("HOME"), before);
    }

    #[test]
    fn sanitize_clamps_padding_on_narrow_widths() {
        // Nonzero widths smaller than 2*padding+1 must not overflow: the row
        // is exactly `width` visible cells (padding clamped, content >= 1).
        for width in 1..=5usize {
            for pad in 0..=4u16 {
                let out = sanitize_status_line("abcdef", width, pad);
                let visible: usize = out.chars().map(|c| char_width(c as u32)).sum();
                assert_eq!(visible, width, "width={width} pad={pad} out={out:?}");
                assert!(!out.contains('\u{1b}'), "no SGR expected: {out:?}");
            }
        }
        // width 1: all padding clamped to 0, first char kept.
        assert_eq!(sanitize_status_line("abcdef", 1, 2), "a");
        // width == 2*pad + 1: full padding fits with one content cell.
        assert_eq!(sanitize_status_line("abcdef", 5, 2), "  a  ");
    }

    #[test]
    fn sanitize_unicode_width_and_padding() {
        // East-Asian wide chars are two cells; the row fits exactly.
        let out = sanitize_status_line("日本", 6, 0);
        assert_eq!(out, "日本  "); // 4 cells content + 2 trailing spaces
        // Combining mark adds zero width.
        let out = sanitize_status_line("e\u{301}x", 4, 0);
        let visible: usize = out.chars().map(|c| char_width(c as u32)).sum();
        assert_eq!(visible, 4);
        // Padding counts inside width for wide content.
        let out = sanitize_status_line("日本", 10, 2);
        let visible: usize = out.chars().map(|c| char_width(c as u32)).sum();
        assert_eq!(visible, 10);
    }

    #[test]
    fn finished_at_is_the_completion_instant() {
        let s = setting("sleep 0.3; printf ok");
        let before = Instant::now();
        let out = run_status_line_job(&s, &request(80)).expect("job ran");
        let after = Instant::now();
        // The stamp must land in [after-spawn, completion], not at the start:
        // the command slept 300ms, so started+300ms <= finished_at must hold.
        assert!(out.finished_at >= before, "stamp before spawn");
        assert!(out.finished_at <= after, "stamp after completion");
        assert!(
            out.finished_at - before >= Duration::from_millis(250),
            "finished_at reflects command duration, not the start instant"
        );
        assert!(out.ok);
        assert_eq!(out.line, "ok");
    }

    #[test]
    fn finished_at_set_on_timeout_and_failure() {
        let s = setting("sleep 5");
        let s = StatusLineSetting { timeout_ms: 300, ..s };
        let started = Instant::now();
        let out = run_status_line_job(&s, &request(80)).expect("job ran");
        assert!(!out.ok);
        // Timeout fires at ~300ms; a start-instant stamp would be ~0ms after.
        assert!(
            out.finished_at.duration_since(started) >= Duration::from_millis(250),
            "timeout stamp should be near the timeout, not the start"
        );
        let s = setting("exit 3");
        let out = run_status_line_job(&s, &request(80)).expect("job ran");
        assert!(!out.ok);
        assert!(out.finished_at >= started);
    }

    #[test]
    fn quoting_via_shell_cannot_inject_extra_commands() {
        let s = setting("echo 'hello; echo evil'");
        let out = run_status_line_job(&s, &request(200)).expect("job ran");
        assert!(out.ok);
        assert!(out.line.contains("hello; echo evil"), "{}", out.line);
        assert!(!out.line.contains("evil\n"));
    }

    #[test]
    fn success_runs_and_captures_stdout() {
        let s = setting("printf '  spaced  '");
        let out = run_status_line_job(&s, &request(80)).expect("job ran");
        assert!(out.ok);
        assert_eq!(out.line, "spaced");
    }

    #[test]
    fn empty_output_is_ok_but_blank() {
        let s = setting("true");
        let out = run_status_line_job(&s, &request(80)).expect("job ran");
        assert!(out.ok);
        assert_eq!(out.line, "");
    }

    #[test]
    fn nonzero_exit_is_failure_with_empty_row() {
        let s = setting("echo boom >&2; exit 3");
        let out = run_status_line_job(&s, &request(80)).expect("job ran");
        assert!(!out.ok);
        assert_eq!(out.line, "");
    }

    #[test]
    fn unspawnable_command_returns_none() {
        let s = setting("~/definitely/not/a/real/binary-xyz");
        assert!(run_status_line_job(&s, &request(80)).is_none());
    }

    #[test]
    fn timeout_is_enforced_and_reported() {
        let s = setting("sleep 5");
        let s = StatusLineSetting {
            timeout_ms: 300,
            ..s
        };
        let started = Instant::now();
        let out = run_status_line_job(&s, &request(80)).expect("job ran");
        assert!(!out.ok);
        assert!(started.elapsed() < Duration::from_millis(4_000));
    }

    #[test]
    fn output_is_capped() {
        let s = setting("yes drip | head -c 100000");
        let out = run_status_line_job(&s, &request(STATUS_LINE_MAX_OUTPUT_CHARS * 2))
            .expect("job ran");
        assert!(out.ok);
        assert!(out.line.chars().count() <= STATUS_LINE_MAX_OUTPUT_CHARS + 32);
    }

    #[test]
    fn only_first_line_is_kept() {
        let out = sanitize_status_line("row one\nrow two\nrow three", 200, 0);
        assert_eq!(out, "row one".to_string() + &" ".repeat(200 - 7));
    }

    #[test]
    fn osc_and_control_chars_are_filtered() {
        let raw = "\x1b]0;window title\x07ok \x1b[2J\x1b[1;1Hgo\x07";
        assert_eq!(
            sanitize_status_line(raw, 80, 0),
            "ok go".to_string() + &" ".repeat(75)
        );
    }

    #[test]
    fn sgr_colors_are_retained_and_reset() {
        let raw = "\x1b[32mgreen\x1b[0m plain";
        let out = sanitize_status_line(raw, 80, 0);
        assert!(out.starts_with("\x1b[32mgreen\x1b[0m plain"));
        assert!(out.ends_with("\x1b[0m"));
        assert_eq!(crate::watch::ansi::string_width(&crate::watch::ansi::strip_ansi(&out)), 80);
    }

    #[test]
    fn unicode_width_and_narrow_truncation() {
        // '世' and '界' are wide (2 cells each): 4 visible cells.
        let out = sanitize_status_line("世界", 6, 1);
        assert_eq!(crate::watch::ansi::string_width(&crate::watch::ansi::strip_ansi(&out)), 6);
        // Truncation never splits a wide char past the budget.
        let out = sanitize_status_line("世界世界", 5, 0);
        assert_eq!(crate::watch::ansi::string_width(&crate::watch::ansi::strip_ansi(&out)), 5);
        assert_eq!(out, "世界".to_string() + &" ".repeat(1));
        // Combining marks add zero width.
        let out = sanitize_status_line("e\u{0301}x", 80, 0);
        assert_eq!(crate::watch::ansi::string_width(&crate::watch::ansi::strip_ansi(&out)), 80);
    }

    #[test]
    fn padding_is_applied_within_width() {
        let out = sanitize_status_line("hi", 10, 2);
        assert_eq!(crate::watch::ansi::string_width(&crate::watch::ansi::strip_ansi(&out)), 10);
        assert!(out.starts_with("  hi  "));
    }

    // --- StatusLineRunner delivery regressions (F-2): every finished job,
    // successes and failures alike, is published on the output channel, so
    // the TUI can fall back to the built-in bar on the newest result.

    fn worker_runner(command: &str, timeout_ms: u64) -> StatusLineRunner {
        StatusLineRunner::new(StatusLineSetting {
            timeout_ms,
            ..setting(command)
        })
    }

    fn drive_until(
        runner: &mut StatusLineRunner,
        width: usize,
        want: impl Fn(&StatusLineOutput) -> bool,
    ) -> StatusLineOutput {
        let started = Instant::now();
        loop {
            // Re-requesting is idempotent: it starts the job, coalesces into
            // `pending` while throttled or in flight, and the worker picks it
            // up regardless of which path the call took.
            runner.request_refresh(request(width));
            if let Some(out) = runner.poll_output() {
                if want(&out) {
                    return out;
                }
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "status-line runner never produced the expected output"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn runner_delivers_failure_after_an_earlier_success() {
        // The command succeeds on the first run and exits nonzero on the
        // second (marker file), so the same runner sees success then failure.
        let marker = std::env::temp_dir().join(format!(
            "drip-statusline-fail-{}.flag",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&marker);
        let path = marker.to_string_lossy().into_owned();
        let mut runner =
            worker_runner(&format!("if [ -f {path} ]; then exit 3; fi; touch {path}; printf drip-row"), 5_000);
        let first = drive_until(&mut runner, 80, |out| out.ok);
        assert_eq!(first.line, "drip-row");
        let second = drive_until(&mut runner, 80, |out| !out.ok);
        assert_eq!(second.line, "");
        let _ = std::fs::remove_file(&marker);
    }

    #[test]
    fn runner_delivers_timeout_after_an_earlier_success() {
        // Second run sleeps past the timeout: the timed-out failure must be
        // delivered promptly so the TUI replaces the row with the built-in
        // bar instead of leaving a stale success visible.
        let marker = std::env::temp_dir().join(format!(
            "drip-statusline-timeout-{}.flag",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&marker);
        let path = marker.to_string_lossy().into_owned();
        let mut runner = worker_runner(
            &format!("if [ -f {path} ]; then sleep 5; fi; touch {path}; printf drip-row"),
            300,
        );
        let first = drive_until(&mut runner, 80, |out| out.ok);
        assert_eq!(first.line, "drip-row");
        let started = Instant::now();
        let second = drive_until(&mut runner, 80, |out| !out.ok);
        assert_eq!(second.line, "");
        assert!(started.elapsed() < Duration::from_secs(4));
        let _ = std::fs::remove_file(&marker);
    }

    #[test]
    fn runner_delivers_failure_output_for_unspawnable_command() {
        // Spawn failure (None from run_status_line_job) also becomes a
        // delivered failure output, never a silently dropped job.
        let mut runner = worker_runner("~/definitely/not/a/real/binary-xyz", 5_000);
        let out = drive_until(&mut runner, 80, |out| !out.ok);
        assert_eq!(out.line, "");
    }
}
