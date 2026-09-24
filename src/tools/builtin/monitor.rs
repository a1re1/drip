// The MONITOR tool: a structured way to wait for a signal (a file appearing,
// a port opening, a build finishing) without spending model rounds on
// `sleep N; check` loops.
//
// One call starts an async job. The job runs `check` with `bash -lc`; exit
// code 0 means the signal is here and the job completes carrying the check's
// output. Any other exit code means keep waiting: the loop sleeps intervalMs
// and tries again, until the wall-clock budget runs out and the job fails
// with the last check output. Every check is bounded by the remaining budget,
// so a hung check cannot outlive timeoutMs. In the harness the settled result
// reaches the model at the next round (harness/loop.rs
// `report_settled_background_jobs`) with no polling; in interactive chat the
// settled job is collected with ASYNC_WAIT or ASYNC_TAIL. A TUI session ends
// the moment its task does and hands the settled job back to itself as its
// next message (src/tui/app.rs `take_idle_background_reports` — the harness
// leaves the job for the session instead of draining it); a CLI run has no
// session to wake, so it instead waits for the settled result and resumes with
// it (harness/loop.rs `hold_for_pending_monitor`), and only a monitor still
// checking after MONITOR_HOLD_MAX_MS is reported as leaked background work.

use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::tools::async_jobs::{compact_whitespace, normalize_line, truncate_text};
use crate::tools::child_process::{run_captured_process, CapturedProcessArgs};
use crate::tools::types::ChatAsyncToolLogger;

/// The signal probe's exit status and captured output for one attempt.
#[derive(Clone, Debug, PartialEq)]
pub struct MonitorAttempt {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
}

impl MonitorAttempt {
    /// The signal is met when the check finished (not by timeout) with exit 0.
    pub fn signalled(&self) -> bool {
        !self.timed_out && self.exit_code == Some(0)
    }
}

/// One monitor call, resolved: what to check, where, how often, for how long.
#[derive(Clone, Debug, PartialEq)]
pub struct MonitorSpec {
    pub check: String,
    pub cwd: String,
    pub interval_ms: i64,
    pub timeout_ms: i64,
    pub description: Option<String>,
}

/// Sleep between checks.
pub const DEFAULT_INTERVAL_MS: i64 = 1_000;
/// Total wall-clock budget before the monitor fails.
pub const DEFAULT_TIMEOUT_MS: i64 = 60_000;
pub const MIN_INTERVAL_MS: i64 = 10;
pub const MAX_INTERVAL_MS: i64 = 60_000;
pub const MAX_TIMEOUT_MS: i64 = 3_600_000;
/// A single check never runs longer than this, however long the budget is.
pub const CHECK_TIMEOUT_CAP_MS: u64 = 60_000;

pub fn clamp_interval_ms(value: Option<f64>) -> i64 {
    match value {
        None => DEFAULT_INTERVAL_MS,
        Some(value) => (value.floor() as i64).clamp(MIN_INTERVAL_MS, MAX_INTERVAL_MS),
    }
}

pub fn clamp_timeout_ms(value: Option<f64>) -> i64 {
    match value {
        None => DEFAULT_TIMEOUT_MS,
        Some(value) => (value.floor() as i64).clamp(0, MAX_TIMEOUT_MS),
    }
}

/// How long one check may run: at least the sleep interval (so a check that
/// is itself a poll has room), never more than the remaining budget, and
/// never longer than the cap.
pub fn check_timeout_ms(interval_ms: i64, remaining_ms: i64) -> u64 {
    let interval = interval_ms.max(MIN_INTERVAL_MS) as u64;
    let remaining = remaining_ms.max(0) as u64;
    interval.max(remaining).min(CHECK_TIMEOUT_CAP_MS)
}

/// The job/display title: the caller's description, else a check preview.
pub fn monitor_title(description: Option<&str>, check: &str) -> String {
    match description.map(str::trim).filter(|text| !text.is_empty()) {
        Some(description) => format!("monitor: {description}"),
        None => format!("monitor: {}", truncate_text(&compact_whitespace(check), 72)),
    }
}

/// The one log line per attempt: index, exit status, and an output preview.
pub fn attempt_line(index: usize, attempt: &MonitorAttempt, interval_ms: i64) -> String {
    let status = if attempt.timed_out {
        "timed out after the per-check budget".to_string()
    } else {
        match attempt.exit_code {
            Some(code) => format!("exit {code}"),
            None => "no exit status".to_string(),
        }
    };
    let mut preview = attempt.stdout.trim().to_string();
    if preview.is_empty() {
        preview = attempt.stderr.trim().to_string();
    }
    let detail = if preview.is_empty() {
        String::new()
    } else {
        format!(" — {}", truncate_text(&compact_whitespace(&preview), 160))
    };

    format!(
        "[monitor] attempt {index}: {status} (signal not met; retrying in {interval_ms}ms){detail}"
    )
}

/// The completion report written when the signal appears.
pub fn build_signal_report(spec: &MonitorSpec, outcome: &MonitorOutcome) -> String {
    let label = monitor_title(spec.description.as_deref(), &spec.check);
    let seconds = outcome.elapsed_ms as f64 / 1000.0;

    format!(
        "[monitor] signal met after {} attempt(s) in {seconds:.1}s: {label}\n{}",
        outcome.attempts_run,
        attempt_output_preview(outcome.last.as_ref())
    )
}

/// The failure report written when the budget runs out; it is also the job's
/// error text, so it reaches the model through the harness's settled-job
/// report even if nobody ever reads the tail again.
pub fn build_timeout_message(spec: &MonitorSpec, outcome: &MonitorOutcome) -> String {
    let label = monitor_title(spec.description.as_deref(), &spec.check);
    let seconds = outcome.elapsed_ms as f64 / 1000.0;

    format!(
        "monitor timed out after {seconds:.1}s ({} attempt(s), interval {}ms) without the signal: {label}. The check never exited 0. Last check output:\n{}",
        outcome.attempts_run,
        spec.interval_ms,
        attempt_output_preview(outcome.last.as_ref())
    )
}

fn attempt_output_preview(last: Option<&MonitorAttempt>) -> String {
    let Some(last) = last else {
        return "(the check never ran)".to_string();
    };
    let mut text = last.stdout.trim().to_string();
    if text.is_empty() {
        text = last.stderr.trim().to_string();
    }
    if text.is_empty() {
        "(no output)".to_string()
    } else {
        truncate_text(&text, 600)
    }
}

/// The result of a monitor's retry loop.
#[derive(Clone, Debug, PartialEq)]
pub struct MonitorOutcome {
    pub attempts_run: usize,
    pub elapsed_ms: i128,
    pub met: bool,
    pub last: Option<MonitorAttempt>,
}

/// The retry loop itself, with the check execution injected so the timing
/// rules are testable without spawning processes: run the check, stop on
/// exit 0, otherwise sleep the interval and try again until the wall-clock
/// budget is spent.
pub fn attempts_until_signal<F>(spec: &MonitorSpec, mut attempt: F) -> MonitorOutcome
where
    F: FnMut(usize, u64) -> MonitorAttempt,
{
    let started = Instant::now();
    let budget = Duration::from_millis(spec.timeout_ms.max(0) as u64);
    let mut attempts_run = 0usize;
    let mut last: Option<MonitorAttempt> = None;

    loop {
        let remaining_ms = budget.saturating_sub(started.elapsed()).as_millis() as i64;
        // The budget is checked after the first attempt, so a timeoutMs of 0
        // still runs the check once.
        if attempts_run > 0 && remaining_ms <= 0 {
            break;
        }

        attempts_run += 1;
        let one = attempt(
            attempts_run,
            check_timeout_ms(spec.interval_ms, remaining_ms),
        );
        let met = one.signalled();
        last = Some(one);
        if met {
            break;
        }

        let left_ms = budget.saturating_sub(started.elapsed()).as_millis() as i64;
        if left_ms <= 0 {
            break;
        }
        let nap = spec.interval_ms.clamp(0, left_ms);
        if nap > 0 {
            std::thread::sleep(Duration::from_millis(nap as u64));
        }
    }

    let met = last
        .as_ref()
        .map(MonitorAttempt::signalled)
        .unwrap_or(false);
    MonitorOutcome {
        attempts_run,
        elapsed_ms: started.elapsed().as_millis() as i128,
        met,
        last,
    }
}

/// One real check: `bash -lc <check>` in the monitor's cwd, bounded by
/// `timeout_ms`. A process that could not even start counts as a failed
/// attempt (its message becomes the captured stderr).
pub fn run_check_once(check: &str, cwd: &str, timeout_ms: u64) -> MonitorAttempt {
    let process_args = vec!["-lc".to_string(), check.to_string()];
    let args = CapturedProcessArgs {
        command: "bash",
        cwd: Some(cwd),
        env: None,
        process_args: &process_args,
        timeout_ms: Some(timeout_ms),
        stdin_payload: None,
    };

    match run_captured_process(&args) {
        Ok(result) => MonitorAttempt {
            exit_code: result.exit_code,
            timed_out: result.timed_out,
            stdout: result.stdout,
            stderr: result.stderr,
        },
        Err(error) => MonitorAttempt {
            exit_code: None,
            timed_out: false,
            stdout: String::new(),
            stderr: error,
        },
    }
}

/// The background job body: log the spec, run the retry loop with real checks
/// (logging every attempt), then settle — Ok when the signal appeared, an
/// error carrying the timeout report otherwise.
pub fn run_monitor(spec: &MonitorSpec, logger: &dyn ChatAsyncToolLogger) -> anyhow::Result<()> {
    let label = monitor_title(spec.description.as_deref(), &spec.check);
    let _ = logger.line(&format!("[monitor] {label}"));
    let _ = logger.line(&format!(
        "[monitor] check every {}ms, timeout {}ms, cwd {}",
        spec.interval_ms, spec.timeout_ms, spec.cwd
    ));
    let _ = logger.line(&format!(
        "[monitor] check: {}",
        truncate_text(&spec.check, 240)
    ));

    let outcome = attempts_until_signal(spec, |index, timeout_ms| {
        let attempt = run_check_once(&spec.check, &spec.cwd, timeout_ms);
        let _ = logger.line(&attempt_line(index, &attempt, spec.interval_ms));
        attempt
    });

    if outcome.met {
        let _ = logger.line(&build_signal_report(spec, &outcome));
        return Ok(());
    }

    let message = build_timeout_message(spec, &outcome);
    let _ = logger.line(&normalize_line(&message));
    anyhow::bail!("{message}")
}

/// definition(): the {type: "function", function: …} envelope (the name
/// stays "MONITOR" verbatim).
pub fn definition() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "MONITOR",
            "description": "Wait for a signal without spending model rounds. Runs `check` (a bash command, `bash -lc`) every intervalMs in a background job and finishes the moment it exits 0; on timeout it finishes as failed with the last check output. Returns inline when the signal is already met. Otherwise it keeps checking on its own: as a TUI session the run ends when its task does and the settled result arrives as that session's next message; a CLI run has no session to wake, so it instead waits for the settled result, reports it to you, and resumes — a task loop that would end while the monitor is still checking, even one whose task already finished, waits for the settled result and resumes with it, so a monitor that timed out or failed still reaches you. In interactive chat collect it with ASYNC_WAIT or ASYNC_TAIL — never spend rounds on sleep-and-check loops in BASH.",
            "parameters": {
                "additionalProperties": false,
                "properties": {
                    "check": {
                        "description": "The signal probe: a bash command run with `bash -lc` on every attempt. Exit code 0 means the signal is here; any other exit code (or a check timeout) means keep waiting.",
                        "type": "string"
                    },
                    "cwd": {
                        "description": "Optional working directory for the check, relative to the current working directory or absolute. Defaults to the workspace cwd.",
                        "type": "string"
                    },
                    "description": {
                        "description": "Optional human-readable description of the signal (shown in the job title and the timeout report).",
                        "type": "string"
                    },
                    "intervalMs": {
                        "description": "Milliseconds to sleep between checks (10-60000, default 1000). No check runs while sleeping.",
                        "type": "number"
                    },
                    "timeoutMs": {
                        "description": "Total milliseconds to keep waiting before the monitor fails (0-3600000, default 60000).",
                        "type": "number"
                    }
                },
                "required": ["check"],
                "type": "object"
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(interval_ms: i64, timeout_ms: i64) -> MonitorSpec {
        MonitorSpec {
            check: "test -f signal.txt".to_string(),
            cwd: "/tmp".to_string(),
            interval_ms,
            timeout_ms,
            description: None,
        }
    }

    fn attempt(exit_code: Option<i32>, timed_out: bool) -> MonitorAttempt {
        MonitorAttempt {
            exit_code,
            timed_out,
            stdout: String::new(),
            stderr: String::new(),
        }
    }

    #[test]
    fn clamps_default_and_bounds() {
        assert_eq!(clamp_interval_ms(None), DEFAULT_INTERVAL_MS);
        assert_eq!(clamp_interval_ms(Some(0.0)), MIN_INTERVAL_MS);
        assert_eq!(clamp_interval_ms(Some(9e9)), MAX_INTERVAL_MS);
        assert_eq!(clamp_interval_ms(Some(250.0)), 250);
        assert_eq!(clamp_timeout_ms(None), DEFAULT_TIMEOUT_MS);
        assert_eq!(clamp_timeout_ms(Some(-5.0)), 0);
        assert_eq!(clamp_timeout_ms(Some(9e9)), MAX_TIMEOUT_MS);
    }

    #[test]
    fn per_check_timeout_never_runs_past_the_cap() {
        assert_eq!(check_timeout_ms(1_000, 500), 1_000);
        assert_eq!(check_timeout_ms(1_000, 200_000), CHECK_TIMEOUT_CAP_MS);
        assert_eq!(check_timeout_ms(10, 0), 10);
    }

    #[test]
    fn stops_on_the_attempt_that_exits_zero() {
        let outcome = attempts_until_signal(&spec(10, 5_000), |index, _| {
            if index < 3 {
                attempt(Some(1), false)
            } else {
                attempt(Some(0), false)
            }
        });

        assert!(outcome.met);
        assert_eq!(outcome.attempts_run, 3);
        assert!(build_signal_report(&spec(10, 5_000), &outcome)
            .contains("signal met after 3 attempt(s)"));
    }

    #[test]
    fn a_timed_out_check_is_not_a_signal() {
        let outcome = attempts_until_signal(&spec(10, 40), |_, _| attempt(None, true));

        assert!(!outcome.met);
        assert!(outcome.attempts_run >= 1);
        assert!(build_timeout_message(&spec(10, 40), &outcome).contains("timed out after"));
    }

    #[test]
    fn the_budget_bounds_the_loop_and_the_interval_bounds_its_rate() {
        let slow = attempts_until_signal(&spec(60, 240), |_, _| attempt(Some(1), false));
        let fast = attempts_until_signal(&spec(10, 240), |_, _| attempt(Some(1), false));

        assert!(!slow.met && !fast.met);
        assert!(
            fast.attempts_run > slow.attempts_run,
            "fast={} slow={}",
            fast.attempts_run,
            slow.attempts_run
        );
        assert!(slow.attempts_run >= 2, "slow={}", slow.attempts_run);
        assert!(slow.elapsed_ms < 2_000, "elapsed={}", slow.elapsed_ms);
    }

    #[test]
    fn an_empty_timeout_still_runs_the_check_once() {
        let outcome = attempts_until_signal(&spec(10, 0), |_, _| attempt(Some(1), false));

        assert_eq!(outcome.attempts_run, 1);
        assert!(!outcome.met);
    }

    #[test]
    fn a_real_check_runs_bash_with_the_signal_semantics() {
        let met = run_check_once("exit 0", "/tmp", 5_000);
        assert!(met.signalled());

        let not_met = run_check_once("exit 3", "/tmp", 5_000);
        assert_eq!(not_met.exit_code, Some(3));
        assert!(!not_met.signalled());

        let output = run_check_once("echo waiting-for-file", "/tmp", 5_000);
        assert_eq!(output.exit_code, Some(0));
        assert!(output.stdout.contains("waiting-for-file"));
    }

    #[test]
    fn titles_and_attempt_lines_name_the_signal() {
        assert_eq!(
            monitor_title(Some("the build to finish"), "test -f target/debug/x"),
            "monitor: the build to finish"
        );
        assert!(monitor_title(None, "test -f  a.txt").starts_with("monitor: test -f a.txt"));

        let line = attempt_line(1, &attempt(Some(7), false), 250);
        assert!(line.contains("attempt 1"));
        assert!(line.contains("exit 7"));
        assert!(line.contains("250ms"));
    }
}
