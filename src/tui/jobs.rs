//! The background-jobs browser: the status-line counter for live background
//! work (`1 monitor`, `2 shells`) and the list/detail frames the counter opens
//! into. Pure render and formatting only: the app owns the runtime, the
//! running-job snapshot and the key handling, so nothing here reads a job or
//! paints a screen.

use crate::tools::types::{ChatAsyncToolJob, ChatAsyncToolJobStatus};
use crate::tui::theme::{paint, ACCENT_COLOR, DIM_COLOR};
use crate::tui::widgets::{boxed, boxed_titled};

/// How often the app re-reads the running-job snapshot. Quick enough that a
/// job starting or settling is visible before the eye moves, slow enough that
/// a long run is not repainting its frame constantly.
pub const JOBS_REFRESH_MS: u64 = 500;

/// The status-line counter: `1 monitor`, `2 monitors`, `1 shell`, `2 shells`,
/// joined by ` · ` when both kinds are live. `None` when nothing runs, so an
/// idle status line is exactly what it was before this existed.
pub fn background_counter(monitors: usize, shells: usize) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if monitors > 0 {
        parts.push(format!(
            "{monitors} {}",
            if monitors == 1 { "monitor" } else { "monitors" }
        ));
    }
    if shells > 0 {
        parts.push(format!(
            "{shells} {}",
            if shells == 1 { "shell" } else { "shells" }
        ));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}

/// Whether a job is a monitor or an async shell, from the tool that started
/// it (`BASH_ASYNC` is the shell tool; every other async tool rides the same
/// runtime and counts as a shell).
pub fn job_kind(job: &ChatAsyncToolJob) -> &'static str {
    if job.tool_name == "MONITOR" {
        "monitor"
    } else {
        "shell"
    }
}

/// Live jobs by kind. The caller passes the running snapshot, so a settled job
/// can never be counted.
pub fn job_counts(jobs: &[ChatAsyncToolJob]) -> (usize, usize) {
    let mut monitors = 0;
    let mut shells = 0;
    for job in jobs {
        if job.tool_name == "MONITOR" {
            monitors += 1;
        } else {
            shells += 1;
        }
    }
    (monitors, shells)
}

/// `20s`, `1m 05s` — a job runtime as a compact clock.
fn clock(ms: u64) -> String {
    let seconds = ms / 1000;
    if seconds >= 60 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

/// `running 20s` for a job whose age is known, `running` otherwise.
pub fn running_label(runtime_ms: Option<i64>) -> String {
    match runtime_ms {
        Some(ms) => format!("running {}", clock(ms.max(0) as u64)),
        None => "running".to_string(),
    }
}

/// The short job id the browser and the transcript notes share.
fn short_job_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// The runtime of a job as a clock string, `—` when the start time cannot be
/// parsed.
pub fn runtime_clock(runtime_ms: Option<i64>) -> String {
    runtime_ms
        .map(|ms| clock(ms.max(0) as u64))
        .unwrap_or_else(|| "—".to_string())
}

/// The status word a job shows in its detail frame.
pub fn job_status_label(job: &ChatAsyncToolJob) -> &'static str {
    match job.status {
        ChatAsyncToolJobStatus::Running => "running",
        ChatAsyncToolJobStatus::Completed => "completed",
        ChatAsyncToolJobStatus::Failed => "failed",
    }
}

/// The jobs browser list: a header with the live counts, one row per running
/// job (kind, title, counting runtime, short id) with the highlight kept
/// visible, and the keys that navigate it. Pure — snapshot, selection and
/// width come in.
pub fn render_jobs_list(jobs: &[ChatAsyncToolJob], selected: usize, width: usize) -> Vec<String> {
    let accent = paint(ACCENT_COLOR);
    let dim = paint(DIM_COLOR);
    let (monitors, shells) = job_counts(jobs);
    let header = match background_counter(monitors, shells) {
        Some(counts) => format!("Background jobs · {counts}"),
        None => "Background jobs".to_string(),
    };
    let mut rows = vec![accent(&header)];
    if jobs.is_empty() {
        rows.push(dim(
            "nothing running — monitors and async shells appear here while they run",
        ));
        rows.push(String::new());
        rows.push(dim("esc to close"));
        return boxed(rows, width, ACCENT_COLOR);
    }
    for (index, job) in jobs.iter().enumerate() {
        let is_selected = index == selected;
        let prefix = if is_selected {
            accent("▸ ")
        } else {
            dim("  ")
        };
        let label = format!("{}. {}", index + 1, job.title);
        let label = if is_selected { accent(&label) } else { label };
        let runtime = crate::tools::async_jobs::session_age_ms(&job.started_at);
        let detail = dim(&format!(
            " — {} · job {}",
            running_label(runtime),
            short_job_id(&job.id)
        ));
        rows.push(format!("{prefix}{label}{detail}"));
    }
    rows.push(String::new());
    rows.push(dim("↑/↓ move · enter to inspect · esc to close"));
    boxed(rows, width, ACCENT_COLOR)
}

/// The detail frame one job opens into: status, counting runtime, the script
/// (or the monitor's check, which rides its title), the log tail, and the keys
/// that lead back out. `output` is the tail the app read from the job log.
pub fn render_job_detail(
    job: &ChatAsyncToolJob,
    runtime_ms: Option<i64>,
    output: &str,
    width: usize,
) -> Vec<String> {
    let dim = paint(DIM_COLOR);
    let accent = paint(ACCENT_COLOR);
    let title = if job_kind(job) == "monitor" {
        "Monitor details"
    } else {
        "Shell details"
    };
    let script = job
        .command
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .unwrap_or(job.title.as_str());
    let mut rows = vec![
        format!("{} {}", dim("Status:"), accent(job_status_label(job))),
        format!("{} {}", dim("Runtime:"), accent(&runtime_clock(runtime_ms))),
        format!("{} {script}", dim("Script:")),
        String::new(),
        dim("Output:"),
    ];
    let tail = output.trim_end();
    if tail.is_empty() {
        rows.push(dim("No output available"));
    } else {
        for line in tail.lines() {
            rows.push(line.to_string());
        }
    }
    rows.push(String::new());
    rows.push(dim("← to go back · Esc/Enter/Space to close"));
    boxed_titled(rows, width, ACCENT_COLOR, Some(title))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(id: &str, tool_name: &str, title: &str) -> ChatAsyncToolJob {
        ChatAsyncToolJob {
            command: None,
            cwd: "/tmp".to_string(),
            error: None,
            exit_code: None,
            finished_at: None,
            id: id.to_string(),
            log_path: "/tmp/job.log".to_string(),
            started_at: "2026-01-01T00:00:00Z".to_string(),
            status: ChatAsyncToolJobStatus::Running,
            title: title.to_string(),
            tool_name: tool_name.to_string(),
        }
    }

    fn plain(rows: &[String]) -> Vec<String> {
        let strip = regex::Regex::new("\u{1b}\\[[0-9;]*m").expect("ansi regex");
        rows.iter()
            .map(|row| strip.replace_all(row, "").into_owned())
            .collect()
    }

    #[test]
    fn background_counter_names_each_kind_and_only_when_live() {
        assert_eq!(background_counter(0, 0), None);
        assert_eq!(background_counter(1, 0).as_deref(), Some("1 monitor"));
        assert_eq!(background_counter(2, 0).as_deref(), Some("2 monitors"));
        assert_eq!(background_counter(0, 1).as_deref(), Some("1 shell"));
        assert_eq!(background_counter(0, 3).as_deref(), Some("3 shells"));
        assert_eq!(
            background_counter(1, 2).as_deref(),
            Some("1 monitor · 2 shells")
        );
    }

    #[test]
    fn counts_split_monitors_from_shells() {
        let jobs = vec![
            job("monitor-1", "MONITOR", "monitor: watch the build"),
            job("shell-1", "BASH_ASYNC", "cargo test --lib"),
            job("shell-2", "BASH_ASYNC", "bun test"),
        ];
        assert_eq!(job_counts(&jobs), (1, 2));
        assert_eq!(job_counts(&[]), (0, 0));
        assert_eq!(job_kind(&jobs[0]), "monitor");
        assert_eq!(job_kind(&jobs[1]), "shell");
    }

    #[test]
    fn the_list_rows_carry_kind_runtime_and_the_navigation_hint() {
        let jobs = vec![
            job("monitor-1", "MONITOR", "monitor: watch the build"),
            job("shell-1", "BASH_ASYNC", "cargo test --lib"),
        ];
        let rows = plain(&render_jobs_list(&jobs, 1, 72));
        let text = rows.join("\n");
        assert!(
            text.contains("Background jobs · 1 monitor · 1 shell"),
            "{text}"
        );
        assert!(text.contains("1. monitor: watch the build"), "{text}");
        assert!(text.contains("2. cargo test --lib"), "{text}");
        assert!(text.contains("job monitor-"), "{text}");
        assert!(text.contains("enter to inspect"), "{text}");
    }

    #[test]
    fn an_empty_browser_says_so_instead_of_showing_a_blank_box() {
        let rows = plain(&render_jobs_list(&[], 0, 72));
        let text = rows.join("\n");
        assert!(text.contains("nothing running"), "{text}");
        assert!(text.contains("esc to close"), "{text}");
    }

    #[test]
    fn the_detail_frame_shows_status_runtime_script_and_output() {
        let mut monitor = job("monitor-1", "MONITOR", "monitor: watch the build");
        let rows = plain(&render_job_detail(&monitor, Some(20_000), "", 72));
        let text = rows.join("\n");
        assert!(text.contains("Monitor details"), "{text}");
        assert!(text.contains("Status: running"), "{text}");
        assert!(text.contains("Runtime: 20s"), "{text}");
        // A monitor's script line falls back to its title when it has no
        // command of its own.
        assert!(text.contains("Script: monitor: watch the build"), "{text}");
        assert!(text.contains("No output available"), "{text}");
        assert!(text.contains("← to go back"), "{text}");

        monitor.command = Some("sleep 120; echo \"foo bar baz\"".to_string());
        let rows = plain(&render_job_detail(
            &monitor,
            Some(1_500),
            "line one\nline two\n",
            72,
        ));
        let text = rows.join("\n");
        assert!(text.contains("Script: sleep 120"), "{text}");
        assert!(text.contains("line two"), "{text}");
        assert!(!text.contains("No output available"), "{text}");
    }

    #[test]
    fn a_shell_job_gets_the_shell_detail_title_and_its_own_status_word() {
        let mut shell = job("shell-1", "BASH_ASYNC", "cargo test --lib");
        shell.command = Some("cargo test --lib".to_string());
        shell.status = ChatAsyncToolJobStatus::Failed;
        let text = plain(&render_job_detail(&shell, Some(2_000), "boom", 72)).join("\n");
        assert!(text.contains("Shell details"), "{text}");
        assert!(text.contains("Status: failed"), "{text}");
        assert!(text.contains("Runtime: 2s"), "{text}");
    }
}
