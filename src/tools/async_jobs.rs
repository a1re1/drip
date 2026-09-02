// port of src/tools/async-jobs.ts
//
// The TS module provides two runtimes that BASH_ASYNC (tools/bash-tool.ts)
// rides on:
//
//   1. TmuxSessionManager — an in-memory registry of tmux sessions with a
//      liveness probe (`tmux has-session` + `display-message -p #{pane_dead}`)
//      and a 10s start grace period for sessions whose tmux server has not
//      finished registering them.
//   2. AsyncToolJobManager — create/track background jobs, each with a log
//      file under <cwd>/.drip/async-tools/<id>.log (TS used `.lci`; drip is
//      the only user-visible rename), a serialized append queue, and a
//      settle-once finish path.
//
// The BASH_ASYNC tool itself (schema + stages) lives in
// drip/src/tools/builtin/bash.rs; this module holds the shared runtime.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};

/// Port of TMUX_SESSION_START_GRACE_MS: how long a session may keep benefiting
/// from the doubt when `tmux has-session` cannot see it yet (the tmux server
/// registers sessions asynchronously).
pub const TMUX_SESSION_START_GRACE_MS: u64 = 10_000;

/// Port of formatError's non-Error fallback.
pub const UNKNOWN_ASYNC_TOOL_FAILURE: &str = "Unknown async tool failure.";

use crate::tools::types::{
    ChatAsyncToolCommandRequest, ChatAsyncToolJob, ChatAsyncToolJobStatus, ChatAsyncToolLogger,
    ChatAsyncToolRuntime, ChatAsyncToolTailResult, ChatAsyncToolTaskRequest,
    ChatAsyncToolWaitResult, ChatTmuxSession, ChatTmuxSessionRuntime, ChatToolRuntimeServices,
};

fn status_as_str(status: ChatAsyncToolJobStatus) -> &'static str {
    match status {
        ChatAsyncToolJobStatus::Running => "running",
        ChatAsyncToolJobStatus::Completed => "completed",
        ChatAsyncToolJobStatus::Failed => "failed",
    }
}

#[allow(dead_code)]
fn parse_status(text: &str) -> Option<ChatAsyncToolJobStatus> {
    match text {
        "running" => Some(ChatAsyncToolJobStatus::Running),
        "completed" => Some(ChatAsyncToolJobStatus::Completed),
        "failed" => Some(ChatAsyncToolJobStatus::Failed),
        _ => None,
    }
}

/// Port of ProcessResult — the shape runProcess hands back.
#[derive(Debug, Clone)]
pub struct ProcessResult {
    pub exit_code: Option<i32>,
    pub stderr: String,
    pub stdout: String,
}

/// Port of normalizeLine: logger.line always writes a terminated line.
pub fn normalize_line(text: &str) -> String {
    if text.ends_with('\n') {
        text.to_string()
    } else {
        format!("{text}\n")
    }
}

/// Port of formatCommand: `[command, ...args].join(" ").trim()`.
pub fn format_command(command: &str, args: &[String]) -> String {
    let mut parts = Vec::with_capacity(args.len() + 1);
    parts.push(command.to_string());
    parts.extend_from_slice(args);
    parts.join(" ").trim().to_string()
}

/// Port of tailText: CRLF is normalized, a single trailing newline does not
/// count as a line, and the last `lines` lines are kept.
pub fn tail_text(text: &str, lines: usize) -> String {
    let normalized_text = text.replace("\r\n", "\n");
    let mut normalized_lines: Vec<&str> = normalized_text.split('\n').collect();

    if normalized_lines.last() == Some(&"") {
        normalized_lines.pop();
    }

    let start = normalized_lines.len().saturating_sub(lines);
    normalized_lines[start..].join("\n")
}

/// Port of formatError for anyhow errors (the Error.message branch; the
/// non-Error fallback lives in UNKNOWN_ASYNC_TOOL_FAILURE).
pub fn format_error(error: &anyhow::Error) -> String {
    error.to_string()
}

/// Port of truncateText (the async-jobs logger / bash-tool preview share it):
/// cap at `max_length`, with `...` replacing the tail.
pub fn truncate_text(value: &str, max_length: usize) -> String {
    if value.len() <= max_length {
        return value.to_string();
    }

    let mut end = max_length.saturating_sub(3);
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }

    format!("{}...", &value[..end])
}

/// Port of compactWhitespace: runs of whitespace collapse to one space.
pub fn compact_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ---------------------------------------------------------------------------
// Job-dir / log-path helpers
//
// TS: jobsRoot = resolve(cwd, ".lci", "async-tools"); drip writes inside
// `.drip` instead (the only user-visible rename on this path).
// ---------------------------------------------------------------------------

pub fn default_jobs_root(cwd: &Path) -> PathBuf {
    cwd.join(".drip").join("async-tools")
}

/// Port of `join(this.jobsRoot, `${id}.log`)`.
pub fn log_path_for(jobs_root: &Path, id: &str) -> PathBuf {
    jobs_root.join(format!("{id}.log"))
}

/// Port of createJob's id: `randomUUID()` (TS: node:crypto randomUUID).
pub fn create_job_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Port of the manager's bookkeeping timestamps (ISO strings so the sort in
/// list_sessions stays lexicographic like the TS localeCompare on ISO dates).
pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Port of `Date.now() - Date.parse(startedAt)`: None means the TS NaN case
/// (unparseable date), which isFinite() rejects.
pub fn session_age_ms(started_at: &str) -> Option<i64> {
    let started = chrono::DateTime::parse_from_rfc3339(started_at).ok()?;
    let elapsed = chrono::Utc::now() - started.with_timezone(&chrono::Utc);
    Some(elapsed.num_milliseconds())
}

/// Port of getRecord's error: `Async job "${jobId}" was not found.`
pub fn job_not_found_message(job_id: &str) -> String {
    format!("Async job \"{job_id}\" was not found.")
}

/// Port of buildChildProcessEnv (tools/child-env.ts): the child inherits the
/// harness environment with the request's overrides applied on top.
pub fn build_child_process_env(overrides: Option<&BTreeMap<String, String>>) -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = std::env::vars().collect();

    if let Some(overrides) = overrides {
        for (key, value) in overrides {
            env.insert(key.clone(), value.clone());
        }
    }

    env
}

/// Port of runProcess (debt audit C1): tmux control commands go through the
/// shared capture core (runCapturedProcess) so they gain the kill-tree +
/// stop-terminator semantics the other tools already have. Default timeout is
/// 5s, matching the TS default argument.
pub fn run_process(command: &str, args: &[&str]) -> Result<ProcessResult> {
    let owned_args: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();

    let captured = crate::tools::child_process::run_captured_process(
        &crate::tools::child_process::CapturedProcessArgs {
            command,
            cwd: None,
            env: None,
            process_args: &owned_args,
            timeout_ms: Some(5_000),
        },
    )
    .map_err(|error| anyhow!(error))?;

    Ok(ProcessResult {
        exit_code: captured.exit_code,
        stderr: captured.stderr,
        stdout: captured.stdout,
    })
}

// ---------------------------------------------------------------------------
// Log appends
//
// TS serializes appends through a per-record writeQueue; here a single lock
// gives every append the same happens-before ordering, so `[finish]` always
// lands after the queued output exactly like the awaited writeQueue did.
// ---------------------------------------------------------------------------

static LOG_LOCK: Mutex<()> = Mutex::new(());

/// Port of the logger's append: `appendFile(logPath, text, "utf8")`.
fn append_to_log(log_path: &str, text: &str) {
    let _guard = LOG_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(log_path) {
        let _ = file.write_all(text.as_bytes());
    }
}

/// Port of ChatAsyncToolLogger — the handle background jobs write their log
/// lines through. The write-queue ordering is provided by LOG_LOCK.
#[derive(Debug, Clone)]
pub struct JobLogger {
    pub job_id: String,
    pub log_path: String,
}

impl JobLogger {
    pub fn new(job_id: &str, log_path: &str) -> Self {
        Self {
            job_id: job_id.to_string(),
            log_path: log_path.to_string(),
        }
    }

    /// Port of logger.append: raw text goes to the log verbatim.
    pub fn append(&self, text: &str) {
        append_to_log(&self.log_path, text);
    }

    /// Port of logger.line: append(normalizeLine(text)).
    pub fn line(&self, text: &str) {
        self.append(&normalize_line(text));
    }

    /// Port of logger.log: an alias for line.
    pub fn log(&self, text: &str) {
        self.line(text);
    }
}

/// Port of the TS AsyncJobRecord minus the promise machinery: the live job
/// snapshot plus the settle-once flag (Rust expresses waitPromise as a
/// Condvar the manager signals on finish).
#[derive(Debug, Clone)]
pub struct AsyncJobRecord {
    pub job: ChatAsyncToolJob,
    pub settled: bool,
}

/// In-memory job registry.
pub struct AsyncToolJobManager {
    jobs_root: PathBuf,
    records: Mutex<Vec<AsyncJobRecord>>,
    settled: Condvar,
}

impl AsyncToolJobManager {
    pub fn new(jobs_root: PathBuf) -> Self {
        Self {
            jobs_root,
            records: Mutex::new(Vec::new()),
            settled: Condvar::new(),
        }
    }

    /// Port of getJob: null when the id is unknown.
    pub fn get_job(&self, job_id: &str) -> Option<ChatAsyncToolJob> {
        self.records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .find(|record| record.job.id == job_id)
            .map(|record| record.job.clone())
    }

    /// Port of createJob: mkdir the jobs root, mint the id/log path/started_at,
    /// truncate the log file to empty, register the record, and hand back the
    /// job snapshot.
    pub fn create_job(
        &self,
        command: Option<String>,
        cwd: &str,
        title: &str,
        tool_name: &str,
    ) -> Result<ChatAsyncToolJob> {
        fs::create_dir_all(&self.jobs_root)?;

        let id = create_job_id();
        let log_path = log_path_for(&self.jobs_root, &id);
        let job = ChatAsyncToolJob {
            command,
            cwd: cwd.to_string(),
            id: id.clone(),
            log_path: log_path.to_string_lossy().to_string(),
            started_at: now_iso(),
            status: ChatAsyncToolJobStatus::Running,
            title: title.to_string(),
            tool_name: tool_name.to_string(),
            error: None,
            exit_code: None,
            finished_at: None,
        };

        fs::write(&log_path, "")?;

        self.records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(AsyncJobRecord {
                job: job.clone(),
                settled: false,
            });

        Ok(job)
    }

    /// Port of startCommand: log the start banner, spawn the child with piped
    /// stdio streaming into the log, and return the still-running job. Spawn
    /// failures settle the job as failed (Node emits them through the 'error'
    /// event) and the job is still returned.
    pub fn start_command(
        self: &Arc<Self>,
        command: &str,
        args: &[String],
        cwd: Option<&str>,
        title: Option<&str>,
        tool_name: &str,
        extra_env: Option<&BTreeMap<String, String>>,
    ) -> Result<ChatAsyncToolJob> {
        let formatted = format_command(command, args);
        let cwd = cwd
            .map(str::to_string)
            .unwrap_or_else(|| ".".to_string());
        let job = self.create_job(Some(formatted.clone()), &cwd, title.unwrap_or(&formatted), tool_name)?;
        let logger = JobLogger::new(&job.id, &job.log_path);

        logger.line(&format!("[start] {}", job.title));
        logger.line(&format!(
            "[command] {}",
            job.command.clone().unwrap_or_else(|| command.to_string())
        ));
        logger.line(&format!("[cwd] {}", job.cwd));

        let mut child_command = Command::new(command);
        child_command
            .args(args)
            .current_dir(&job.cwd)
            .envs(build_child_process_env(extra_env))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let manager = Arc::clone(self);
        let job_id = job.id.clone();
        let run_logger = logger.clone();

        match child_command.spawn() {
            Ok(mut child) => {
                thread::spawn(move || {
                    let stdout = child.stdout.take();
                    let stderr = child.stderr.take();
                    let out_logger = run_logger.clone();
                    let err_logger = run_logger.clone();

                    let out_thread = stdout.map(|mut pipe| {
                        thread::spawn(move || read_pipe_into_log(&mut pipe, &out_logger))
                    });
                    let err_thread = stderr.map(|mut pipe| {
                        thread::spawn(move || read_pipe_into_log(&mut pipe, &err_logger))
                    });

                    let status = child.wait();

                    // Join the readers first so their appends are queued
                    // before the [finish] line, like the awaited writeQueue.
                    if let Some(handle) = out_thread {
                        let _ = handle.join();
                    }
                    if let Some(handle) = err_thread {
                        let _ = handle.join();
                    }

                    match status {
                        Ok(exit) => {
                            let exit_code = exit.code();
                            let status = if exit_code == Some(0) {
                                ChatAsyncToolJobStatus::Completed
                            } else {
                                ChatAsyncToolJobStatus::Failed
                            };
                            manager.finish_job(&job_id, None, exit_code, status);
                        }
                        Err(error) => {
                            manager.finish_job(&job_id, Some(error.to_string()), None, ChatAsyncToolJobStatus::Failed);
                        }
                    }
                });
            }
            Err(error) => {
                self.finish_job(&job.id, Some(error.to_string()), None, ChatAsyncToolJobStatus::Failed);
            }
        }

        Ok(job)
    }

    /// Port of startTask: log the start header, then run `run` on a background
    /// thread — completion settles the job as completed with exitCode 0 and an
    /// error settles it as failed, mirroring the promise chain.
    pub fn start_task<F>(
        self: &Arc<Self>,
        cwd: &str,
        title: &str,
        tool_name: &str,
        run: F,
    ) -> Result<ChatAsyncToolJob>
    where
        F: FnOnce(&JobLogger) -> Result<()> + Send + 'static,
    {
        let job = self.create_job(None, cwd, title, tool_name)?;
        let logger = JobLogger::new(&job.id, &job.log_path);

        logger.line(&format!("[start] {}", job.title));
        logger.line(&format!("[cwd] {}", job.cwd));

        let manager = Arc::clone(self);
        let job_id = job.id.clone();

        thread::spawn(move || match run(&logger) {
            Ok(()) => {
                manager.finish_job(&job_id, None, Some(0), ChatAsyncToolJobStatus::Completed);
            }
            Err(error) => {
                manager.finish_job(&job_id, Some(format_error(&error)), None, ChatAsyncToolJobStatus::Failed);
            }
        });

        Ok(job)
    }

    /// Port of tailJob: lines clamp to [1, 400], the pending write queue is
    /// drained first (here: appends are already serialized), and the log is
    /// tail-trimmed.
    pub fn tail_job(&self, job_id: &str, lines: i64) -> Result<ChatAsyncToolTailResult> {
        let record = self.get_record(job_id)?;
        let normalized_lines = lines.clamp(1, 400) as usize;
        let output = tail_text(
            &fs::read_to_string(&record.job.log_path).unwrap_or_default(),
            normalized_lines,
        );

        Ok(ChatAsyncToolTailResult {
            job: record.job.clone(),
            lines: normalized_lines as i64,
            output,
        })
    }

    /// Port of waitForJob: an already-settled job returns completed
    /// immediately; otherwise block until finishJob signals the Condvar or the
    /// (non-negative) timeout elapses.
    pub fn wait_for_job(&self, job_id: &str, timeout_ms: i64) -> Result<ChatAsyncToolWaitResult> {
        let mut guard = self
            .records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if let Some(record) = guard.iter().find(|record| record.job.id == job_id) {
            if record.job.status != ChatAsyncToolJobStatus::Running {
                return Ok(ChatAsyncToolWaitResult {
                    completed: true,
                    job: record.job.clone(),
                });
            }
        } else {
            bail!("{}", job_not_found_message(job_id));
        }

        // normalizedTimeoutMs = Math.max(0, Math.floor(timeoutMs)); a zero
        // timeout behaves like the TS setTimeout(0) race — it expires at once.
        let normalized_timeout_ms = timeout_ms.max(0) as u64;
        if normalized_timeout_ms == 0 {
            let record = guard
                .iter()
                .find(|record| record.job.id == job_id)
                .expect("checked above");
            return Ok(ChatAsyncToolWaitResult {
                completed: false,
                job: record.job.clone(),
            });
        }

        // wait_timeout_while re-checks the predicate on every wakeup and before
        // the first wait, mirroring the TS while-loop's re-read of
        // asyncJobs.get(jobId).status each iteration; predicate polarity is
        // "keep waiting while still Running" (missing ids would spin forever,
        // so a vanished job falls through to the timeout result).
        // wait_timeout_while blocks WHILE the predicate holds: keep waiting
        // as long as the job is still running.
        let still_running = |state: &mut Vec<AsyncJobRecord>| {
            state
                .iter()
                .find(|record| record.job.id == job_id)
                .map_or(false, |record| {
                    record.job.status == ChatAsyncToolJobStatus::Running
                })
        };
        let (guard, timeout_result) = self
            .settled
            .wait_timeout_while(guard, Duration::from_millis(normalized_timeout_ms), still_running)
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let record = guard
            .iter()
            .find(|record| record.job.id == job_id)
            .expect("checked above");

        if record.job.status != ChatAsyncToolJobStatus::Running {
            return Ok(ChatAsyncToolWaitResult {
                completed: true,
                job: record.job.clone(),
            });
        }

        // timed out waiting for the job to settle
        return Ok(ChatAsyncToolWaitResult {
            completed: false,
            job: record.job.clone(),
        });
    }

    /// Port of finishJob: settle-once — only the first finish wins, the record
    /// gains error/exitCode/finishedAt/status, and the `[error]`/`[finish]`
    /// lines are appended after the queued output. Returns the settled job
    /// snapshot (resolveWait's argument), or None for an unknown id.
    pub fn finish_job(
        &self,
        job_id: &str,
        error: Option<String>,
        exit_code: Option<i32>,
        status: ChatAsyncToolJobStatus,
    ) -> Option<ChatAsyncToolJob> {
        let mut guard = self
            .records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let record = guard
            .iter_mut()
            .find(|record| record.job.id == job_id)?;

        if record.settled {
            return None;
        }

        record.settled = true;
        record.job.error = error;
        // TS finishJob sets exitCode: number | null (null when the process
        // ended without a status) — never absent once settled.
        record.job.exit_code = Some(exit_code);
        record.job.finished_at = Some(now_iso());
        record.job.status = status;

        let logger = JobLogger::new(&record.job.id, &record.job.log_path);

        if let Some(error) = &record.job.error {
            if !error.is_empty() {
                logger.line(&format!("[error] {error}"));
            }
        }

        // TS: `exitCode=${record.job.exitCode}` prints "null" for null.
        let exit_code_text = match record.job.exit_code.flatten() {
            Some(code) => code.to_string(),
            None => "null".to_string(),
        };
        logger.line(&format!(
            "[finish] status={} exitCode={}",
            status_as_str(record.job.status),
            exit_code_text
        ));

        let settled_job = record.job.clone();
        drop(guard);
        self.settled.notify_all();

        Some(settled_job)
    }

    /// Port of getRecord: throws `Async job "…" was not found.` for unknown ids.
    pub fn get_record(&self, job_id: &str) -> Result<AsyncJobRecord> {
        self.records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .find(|record| record.job.id == job_id)
            .cloned()
            .ok_or_else(|| anyhow!("{}", job_not_found_message(job_id)))
    }
}

/// Streaming half of startCommand: each stdout/stderr chunk is appended to the
/// log as it arrives (TS: `child.stdout.on("data", chunk => logger.append(...))`).
fn read_pipe_into_log(pipe: &mut impl Read, logger: &JobLogger) {
    let mut buffer = [0u8; 8192];

    loop {
        match pipe.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                logger.append(&String::from_utf8_lossy(&buffer[..read]));
            }
            Err(_) => break,
        }
    }
}

/// Port of isSessionRunning's probe decision, kept pure so the tmux-free path
/// is testable. The caller feeds back whether each tmux probe exited 0 (and
/// the pane_dead stdout when both did); a failed probe only counts as "still
/// starting" during the 10s grace window (TS: Number.isFinite(age) && age <
/// grace — a negative age from clock skew still passes isFinite).
pub fn is_session_probe_running(
    has_session_ok: bool,
    pane_probe_ok: bool,
    pane_dead_stdout: &str,
    session_age_ms: Option<i64>,
) -> bool {
    if has_session_ok && pane_probe_ok {
        return pane_dead_stdout.trim() == "0";
    }

    session_age_ms.is_some_and(|age| age < TMUX_SESSION_START_GRACE_MS as i64)
}

/// Port of TmuxSessionManager — an in-memory registry of tmux sessions.
pub struct TmuxSessionManager {
    sessions: Mutex<Vec<ChatTmuxSession>>,
}

impl TmuxSessionManager {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(Vec::new()),
        }
    }

    /// Port of registerSession.
    pub fn register_session(&self, session: ChatTmuxSession) {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(session);
    }

    /// Port of getSession: null when unknown or no longer running (a dead
    /// session is dropped from the registry).
    pub fn get_session(&self, session_name: &str) -> Option<ChatTmuxSession> {
        let running = {
            let mut sessions = self
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let index = sessions
                .iter()
                .position(|session| session.session_name == session_name)?;

            if self.is_session_running(&sessions[index]) {
                Some(sessions[index].clone())
            } else {
                sessions.remove(index);
                None
            }
        };

        running
    }

    /// Port of listSessions: live sessions only, newest first, dead ones
    /// dropped from the registry.
    pub fn list_sessions(&self) -> Vec<ChatTmuxSession> {
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut active_sessions = Vec::new();

        sessions.retain(|session| {
            if self.is_session_running(session) {
                active_sessions.push(session.clone());
                true
            } else {
                false
            }
        });

        active_sessions.sort_by(|left, right| right.started_at.cmp(&left.started_at));
        active_sessions
    }

    /// Port of isSessionRunning: `tmux has-session -t <name>`, then
    /// `tmux display-message -p -t <name>:0.0 #{pane_dead}`; a session is
    /// running when pane_dead is 0. Probe failures fall back to the 10s start
    /// grace window; a thrown spawn error returns false like the TS catch.
    fn is_session_running(&self, session: &ChatTmuxSession) -> bool {
        let age = session_age_ms(&session.started_at);

        let has_session = match run_process("tmux", &["has-session", "-t", &session.session_name]) {
            Ok(result) => result,
            Err(_) => return false,
        };

        if has_session.exit_code != Some(0) {
            return is_session_probe_running(false, true, "", age);
        }

        let pane_status = match run_process(
            "tmux",
            &[
                "display-message",
                "-p",
                "-t",
                &format!("{}:0.0", session.session_name),
                "#{pane_dead}",
            ],
        ) {
            Ok(result) => result,
            Err(_) => return false,
        };

        if pane_status.exit_code != Some(0) {
            return is_session_probe_running(true, false, "", age);
        }

        pane_status.stdout.trim() == "0"
    }
}

impl ChatAsyncToolLogger for JobLogger {
    fn append(&self, text: &str) -> anyhow::Result<()> {
        JobLogger::append(self, text);
        Ok(())
    }

    fn line(&self, text: &str) -> anyhow::Result<()> {
        JobLogger::line(self, text);
        Ok(())
    }

    fn log(&self, text: &str) -> anyhow::Result<()> {
        JobLogger::log(self, text);
        Ok(())
    }

    fn job_id(&self) -> &str {
        &self.job_id
    }

    fn log_path(&self) -> &str {
        &self.log_path
    }
}

/// The runtime trait is implemented on `Arc<AsyncToolJobManager>` because the
/// start methods spawn threads that hold an Arc to the manager (TS: the
/// closures capture `this`).
impl ChatAsyncToolRuntime for Arc<AsyncToolJobManager> {
    fn get_job(&self, job_id: &str) -> Option<ChatAsyncToolJob> {
        AsyncToolJobManager::get_job(self, job_id)
    }

    fn start_command(&self, request: ChatAsyncToolCommandRequest) -> Result<ChatAsyncToolJob> {
        AsyncToolJobManager::start_command(
            self,
            &request.command,
            request.args.as_deref().unwrap_or(&[]),
            request.cwd.as_deref(),
            request.title.as_deref(),
            &request.tool_name,
            request.env.as_ref(),
        )
    }

    fn start_task(&self, request: ChatAsyncToolTaskRequest) -> Result<ChatAsyncToolJob> {
        let run = request.run;
        AsyncToolJobManager::start_task(
            self,
            request.cwd.as_deref().unwrap_or("."),
            request.title.as_deref().unwrap_or(&request.tool_name),
            &request.tool_name,
            move |logger: &JobLogger| run(logger),
        )
    }

    fn tail_job(&self, job_id: &str, lines: Option<i64>) -> Result<ChatAsyncToolTailResult> {
        AsyncToolJobManager::tail_job(self, job_id, lines.unwrap_or(60))
    }

    fn wait_for_job(&self, job_id: &str, timeout_ms: Option<i64>) -> Result<ChatAsyncToolWaitResult> {
        AsyncToolJobManager::wait_for_job(self, job_id, timeout_ms.unwrap_or(60_000))
    }
}

impl ChatTmuxSessionRuntime for TmuxSessionManager {
    fn get_session(&self, session_name: &str) -> Option<ChatTmuxSession> {
        TmuxSessionManager::get_session(self, session_name)
    }

    fn list_sessions(&self) -> Vec<ChatTmuxSession> {
        TmuxSessionManager::list_sessions(self)
    }

    fn register_session(&self, session: ChatTmuxSession) {
        TmuxSessionManager::register_session(self, session)
    }
}

/// Port of CreateChatToolRuntimeServicesOptions.
#[derive(Debug, Clone, Default)]
pub struct CreateChatToolRuntimeServicesOptions {
    pub cwd: Option<PathBuf>,
    pub jobs_root: Option<PathBuf>,
}


/// Port of createChatToolRuntimeServices: the jobs root defaults to
/// <cwd>/.drip/async-tools (TS: .lci/async-tools).
pub fn create_chat_tool_runtime_services(
    options: CreateChatToolRuntimeServicesOptions,
) -> ChatToolRuntimeServices {
    let cwd = options
        .cwd
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let jobs_root = options
        .jobs_root
        .unwrap_or_else(|| default_jobs_root(&cwd));

    ChatToolRuntimeServices {
        async_jobs: Arc::new(Arc::new(AsyncToolJobManager::new(jobs_root))),
        tmux_sessions: Arc::new(TmuxSessionManager::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_text_keeps_last_lines_and_drops_trailing_newline() {
        assert_eq!(tail_text("a\nb\nc\n", 2), "b\nc");
        assert_eq!(tail_text("a\nb\nc", 2), "b\nc");
        assert_eq!(tail_text("a\r\nb\r\nc\r\n", 2), "b\nc");
    }

    #[test]
    fn tail_text_with_more_lines_than_present_returns_everything() {
        assert_eq!(tail_text("a\nb\n", 10), "a\nb");
        assert_eq!(tail_text("", 5), "");
    }

    #[test]
    fn normalize_line_appends_newline_only_when_missing() {
        assert_eq!(normalize_line("x"), "x\n");
        assert_eq!(normalize_line("x\n"), "x\n");
    }

    #[test]
    fn format_command_joins_and_trims() {
        assert_eq!(
            format_command("bun", &["run".into(), "test".into()]),
            "bun run test"
        );
        assert_eq!(format_command(" bun ", &[]), "bun");
    }

    #[test]
    fn job_paths_live_under_the_drip_jobs_root() {
        let root = default_jobs_root(Path::new("/tmp/proj"));
        assert_eq!(root, PathBuf::from("/tmp/proj/.drip/async-tools"));
        assert_eq!(
            log_path_for(&root, "abc"),
            PathBuf::from("/tmp/proj/.drip/async-tools/abc.log")
        );
    }

    #[test]
    fn truncate_text_caps_long_values_and_keeps_short_ones() {
        assert_eq!(truncate_text("abcdef", 10), "abcdef");
        assert_eq!(truncate_text("abcdef", 6), "abcdef");
        assert_eq!(truncate_text("abcdefg", 6).len(), 6);
        assert!(truncate_text("abcdefg", 6).ends_with("..."));
    }

    #[test]
    fn compact_whitespace_collapses_runs() {
        assert_eq!(compact_whitespace("  a\n\tb   c "), "a b c");
        assert_eq!(compact_whitespace(""), "");
    }

    #[test]
    fn status_parse_round_trips_the_three_statuses() {
        for text in ["running", "completed", "failed"] {
            assert_eq!(status_as_str(parse_status(text).unwrap()), text);
        }
        assert!(parse_status("unknown").is_none());
    }

    #[test]
    fn session_probe_grace_window_matches_the_ts_semantics() {
        // Both probes succeeded → running iff pane_dead is 0.
        assert!(is_session_probe_running(true, true, "0", None));
        assert!(!is_session_probe_running(true, true, "1", None));
        // A failed probe inside the grace window → still starting; outside →
        // gone. Negative ages pass the TS isFinite check.
        assert!(is_session_probe_running(false, true, "", Some(1_000)));
        assert!(is_session_probe_running(true, false, "", Some(-500)));
        assert!(!is_session_probe_running(false, true, "", Some(60_000)));
        // Unparseable startedAt is the TS NaN case → not running.
        assert!(!is_session_probe_running(false, true, "", None));
    }

    #[test]
    fn build_child_process_env_applies_overrides_on_top_of_the_inherited_env() {
        std::env::set_var("DRIP_ASYNC_JOBS_TEST_VAR", "base");
        let env = build_child_process_env(Some(&BTreeMap::from([(
            "DRIP_ASYNC_JOBS_TEST_VAR".to_string(),
            "override".to_string(),
        )])));
        assert_eq!(env.get("DRIP_ASYNC_JOBS_TEST_VAR").unwrap(), "override");
    }

    #[test]
    fn job_manager_registers_and_finishes_records() {
        let manager = AsyncToolJobManager::new(PathBuf::from("/tmp/drip-jobs-test"));

        let job = manager
            .create_job(None, "/tmp", "tmux session", "BASH_ASYNC")
            .unwrap();
        assert_eq!(job.status, ChatAsyncToolJobStatus::Running);
        assert!(job.log_path.ends_with(".log"));
        assert_eq!(job.tool_name, "BASH_ASYNC");
        assert_eq!(job.title, "tmux session");
        assert_eq!(job.command, None);

        manager.finish_job(&job.id, None, Some(0), ChatAsyncToolJobStatus::Completed);
        let finished = manager.get_job(&job.id).unwrap();
        assert_eq!(finished.status, ChatAsyncToolJobStatus::Completed);
        assert_eq!(finished.exit_code, Some(Some(0)));
        assert!(finished.finished_at.is_some());

        // Unknown ids report not-found, never panic.
        assert!(manager.get_job("missing").is_none());
        assert_eq!(
            job_not_found_message("missing"),
            "Async job \"missing\" was not found."
        );
    }

    #[test]
    fn finish_job_settles_once_and_writes_the_finish_lines() {
        let manager = AsyncToolJobManager::new(PathBuf::from("/tmp/drip-jobs-test"));
        let job = manager
            .create_job(Some("bun run build".to_string()), "/tmp", "bun run build", "BASH_ASYNC")
            .unwrap();

        let first = manager.finish_job(
            &job.id,
            Some("boom".to_string()),
            Some(3),
            ChatAsyncToolJobStatus::Failed,
        );
        assert_eq!(first.unwrap().exit_code, Some(Some(3)));

        // The second finish is a no-op (settle-once).
        let second = manager.finish_job(&job.id, None, Some(0), ChatAsyncToolJobStatus::Completed);
        assert!(second.is_none());
        assert_eq!(manager.get_job(&job.id).unwrap().exit_code, Some(Some(3)));

        let log = fs::read_to_string(&job.log_path).unwrap();
        assert!(log.contains("[error] boom\n"));
        assert!(log.contains("[finish] status=failed exitCode=3\n"));
    }

    #[test]
    fn tail_job_clamps_lines_and_reads_the_log_back() {
        let manager = AsyncToolJobManager::new(PathBuf::from("/tmp/drip-jobs-test"));
        let job = manager
            .create_job(None, "/tmp", "tail me", "BASH_ASYNC")
            .unwrap();
        let logger = JobLogger::new(&job.id, &job.log_path);
        for index in 0..500 {
            logger.line(&format!("line {index}"));
        }

        let result = AsyncToolJobManager::tail_job(&manager, &job.id, 100_000).unwrap();
        assert_eq!(result.lines, 400);
        assert!(result.output.starts_with("line 100\n"));
        assert!(result.output.ends_with("line 499"));

        let small = AsyncToolJobManager::tail_job(&manager, &job.id, 0).unwrap();
        assert_eq!(small.lines, 1);
        assert_eq!(small.output, "line 499");

        assert!(AsyncToolJobManager::tail_job(&manager, "missing", 10).is_err());
    }

    #[test]
    fn wait_for_job_reports_completion_and_timeouts() {
        let manager = Arc::new(AsyncToolJobManager::new(PathBuf::from("/tmp/drip-jobs-test")));
        let job = manager
            .create_job(None, "/tmp", "wait on me", "BASH_ASYNC")
            .unwrap();

        // Zero timeout on a running job → the TS setTimeout(0) race expires.
        let pending = AsyncToolJobManager::wait_for_job(&manager, &job.id, 0).unwrap();
        assert!(!pending.completed);

        // A short timeout on a still-running job reports completed: false.
        let timed_out = AsyncToolJobManager::wait_for_job(&manager, &job.id, 20).unwrap();
        assert!(!timed_out.completed);

        manager.finish_job(&job.id, None, Some(0), ChatAsyncToolJobStatus::Completed);

        // Settled jobs return immediately, even with a zero timeout.
        let finished = AsyncToolJobManager::wait_for_job(&manager, &job.id, 0).unwrap();
        assert!(finished.completed);
        assert_eq!(finished.job.status, ChatAsyncToolJobStatus::Completed);

        assert!(AsyncToolJobManager::wait_for_job(&manager, "missing", 10).is_err());
    }

    #[test]
    fn start_task_runs_the_closure_and_settles_the_job() {
        let manager = Arc::new(AsyncToolJobManager::new(PathBuf::from("/tmp/drip-jobs-test")));
        let job = manager
            .start_task("/tmp", "background work", "BASH_ASYNC", |logger| {
                logger.line("[session] drip-test");
                Ok(())
            })
            .unwrap();

        assert_eq!(job.status, ChatAsyncToolJobStatus::Running);

        let finished = AsyncToolJobManager::wait_for_job(&manager, &job.id, 10_000)
            .unwrap();
        assert!(finished.completed);
        assert_eq!(finished.job.status, ChatAsyncToolJobStatus::Completed);
        assert_eq!(finished.job.exit_code, Some(Some(0)));

        let log = fs::read_to_string(&job.log_path).unwrap();
        assert!(log.contains("[start] background work\n"));
        assert!(log.contains("[cwd] /tmp\n"));
        assert!(log.contains("[session] drip-test\n"));
        assert!(log.contains("[finish] status=completed exitCode=0\n"));
    }

    #[test]
    fn start_task_failures_settle_the_job_as_failed() {
        let manager = Arc::new(AsyncToolJobManager::new(PathBuf::from("/tmp/drip-jobs-test")));
        let job = manager
            .start_task("/tmp", "failing work", "BASH_ASYNC", |_logger| {
                bail!("task exploded")
            })
            .unwrap();

        let finished = AsyncToolJobManager::wait_for_job(&manager, &job.id, 10_000).unwrap();
        assert!(finished.completed);
        assert_eq!(finished.job.status, ChatAsyncToolJobStatus::Failed);
        assert_eq!(finished.job.exit_code, Some(None));
        assert_eq!(finished.job.error.as_deref(), Some("task exploded"));

        let log = fs::read_to_string(&job.log_path).unwrap();
        assert!(log.contains("[error] task exploded\n"));
        assert!(log.contains("[finish] status=failed exitCode=null\n"));
    }

    /// Requires a real tmux server; run with `cargo test -- --ignored`.
    #[test]
    #[ignore = "requires tmux"]
    fn tmux_session_manager_tracks_a_live_session() {
        let session_name = format!("drip-test-{}", create_job_id());
        std::process::Command::new("tmux")
            .args(["new-session", "-d", "-s", &session_name])
            .status()
            .expect("tmux available");

        let manager = TmuxSessionManager::new();
        manager.register_session(ChatTmuxSession {
            attach_command: format!("tmux attach -t {session_name}"),
            cwd: "/tmp".to_string(),
            job_id: create_job_id(),
            kill_command: format!("tmux kill-session -t {session_name}"),
            session_name: session_name.clone(),
            started_at: now_iso(),
            title: "test".to_string(),
            tool_name: "BASH_ASYNC".to_string(),
        });

        assert!(manager.get_session(&session_name).is_some());
        assert_eq!(manager.list_sessions().len(), 1);

        std::process::Command::new("tmux")
            .args(["kill-session", "-t", &session_name])
            .status()
            .expect("kill session");
    }
}
