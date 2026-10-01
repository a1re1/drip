//! End-to-end tests for the conservative tmux session reaper
//! (`drip --reap-tmux`, the shared policy `drip --gc` reuses, and the
//! BASH_ASYNC completion cleanup) against a REAL tmux server.
//!
//! Every server these tests create lives on its own socket directory, passed
//! as `TMUX_TMPDIR`, so the suite can never see or touch the operator's
//! sessions. Each test skips itself when tmux is not installed.
//!
//! What is covered here (unit tests in `src/tools/tmux_reap.rs` cover the pure
//! eligibility matrix):
//!   * a pass on a live server kills only the dead, recorded, drip-owned
//!     session and leaves a running job, a fresh dead pane, a legacy unmarked
//!     session and a foreign session alone — through the CLI, twice;
//!   * `drip --reap-tmux` works from an empty directory with a fresh HOME and
//!     no project index (`--json` and plain output, plus the mode guard);
//!   * a session that vanishes mid-pass is kept, not killed, and the rest of
//!     the pass still runs;
//!   * a finished BASH_ASYNC job is collected as soon as its exit status is
//!     durable, and its job record + log stay readable afterwards;
//!   * a reap pass never touches a still-running BASH_ASYNC job, and killing
//!     its tmux session leaves the result readable;
//!   * a MONITOR job rides no tmux session and its settled result survives;
//!   * a missing tmux is not an error;
//!   * an exact target never resolves to a longer same-prefix session (only
//!     the longer name exists, yet the missing name stays missing and its
//!     sibling survives a kill of the missing name);
//!   * a session whose detached second window is still alive is kept.
use std::path::Path;
use std::process::{Command, Output};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use drip::tools::async_jobs::{
    create_chat_tool_runtime_services, CreateChatToolRuntimeServicesOptions,
};
use drip::tools::execute::{execute_tool_call, ToolExecutionContext};
use drip::tools::pack::{builtin_tool_pack, BuiltinToolOptions};
use drip::tools::tmux_reap::{
    list_tmux_sessions, now_epoch_ms, probe_tmux_session, reap_tmux_sessions_with_policy,
    reap_with, ReapPolicy, TMUX_OWNER_MARKER, TMUX_OWNER_OPTION, TMUX_RESULT_MARKER,
    TMUX_RESULT_OPTION,
};
use drip::tools::types::{ChatAsyncToolJob, ChatAsyncToolJobStatus, ChatToolRuntimeServices};

/// The drip binary under test (the `drip` bin of this crate).
const DRIP_BIN: &str = env!("CARGO_BIN_EXE_drip");

/// Serializes the tests that drive tmux through the *process environment*
/// (the library API and the tool pack read `TMUX_TMPDIR` from the environment,
/// so those tests must not run concurrently with a different socket).
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn tmux_available() -> bool {
    Command::new("tmux")
        .arg("-V")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// One private tmux server. Dropping it kills the server, so no session this
/// suite creates outlives a test.
struct TmuxServer {
    dir: tempfile::TempDir,
}

impl TmuxServer {
    fn new() -> Self {
        TmuxServer {
            dir: tempfile::tempdir().expect("a temp socket dir"),
        }
    }

    fn socket(&self) -> &Path {
        self.dir.path()
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new("tmux")
            .args(args)
            .env("TMUX_TMPDIR", self.socket())
            .env_remove("TMUX")
            .output()
            .expect("tmux must be runnable")
    }

    fn ok(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "tmux {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).to_string()
    }

    /// Sorted session names currently on this server.
    fn sessions(&self) -> Vec<String> {
        let output = self.run(&["ls", "-F", "#{session_name}"]);

        if !output.status.success() {
            // "no server running" — no sessions.
            return Vec::new();
        }

        let mut names: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|line| line.trim().to_string())
            .filter(|line| !line.is_empty())
            .collect();
        names.sort();
        names
    }

    fn create(&self, name: &str, command: &str) {
        self.ok(&["new-session", "-d", "-s", name, command]);
    }

    fn mark_owned(&self, name: &str) {
        self.ok(&[
            "set-option",
            "-t",
            name,
            TMUX_OWNER_OPTION,
            TMUX_OWNER_MARKER,
        ]);
    }

    fn mark_result_recorded(&self, name: &str) {
        self.ok(&[
            "set-option",
            "-t",
            name,
            TMUX_RESULT_OPTION,
            TMUX_RESULT_MARKER,
        ]);
    }

    /// A drip-owned session whose pane is dead and stays dead
    /// (`remain-on-exit on`, exactly like BASH_ASYNC leaves it).
    fn dead_owned_session(&self, name: &str, recorded: bool) {
        self.create(name, "sleep 300");
        self.ok(&["set-window-option", "-t", name, "remain-on-exit", "on"]);
        self.mark_owned(name);

        if recorded {
            self.mark_result_recorded(name);
        }

        self.ok(&["respawn-pane", "-k", "-t", name, "true"]);
        self.wait_for_dead_pane(name);
    }

    fn owned_live_session(&self, name: &str, command: &str) {
        self.create(name, command);
        self.mark_owned(name);
    }

    fn wait_for_dead_pane(&self, name: &str) {
        let started = Instant::now();

        while started.elapsed() < Duration::from_secs(10) {
            let output = self.run(&["display-message", "-p", "-t", name, "#{pane_dead}"]);

            if String::from_utf8_lossy(&output.stdout).trim() == "1" {
                return;
            }

            std::thread::sleep(Duration::from_millis(25));
        }

        panic!("pane of \"{name}\" never died");
    }
}

impl Drop for TmuxServer {
    fn drop(&mut self) {
        let _ = self.run(&["kill-server"]);
    }
}

/// Runs `drip <args>` against `server` from an empty directory with a fresh
/// HOME, so nothing the binary does can depend on a project index or config.
fn run_cli(server: &TmuxServer, args: &[&str]) -> (Output, tempfile::TempDir) {
    let sandbox = tempfile::tempdir().expect("a sandbox dir");
    let cwd = sandbox.path().join("empty-cwd");
    let home = sandbox.path().join("home");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&home).unwrap();

    let output = Command::new(DRIP_BIN)
        .args(args)
        .current_dir(&cwd)
        .env("TMUX_TMPDIR", server.socket())
        .env("HOME", &home)
        .env_remove("TMUX")
        .output()
        .expect("the drip binary must run");

    (output, sandbox)
}

fn json_of(output: &Output) -> serde_json::Value {
    assert!(
        output.status.success(),
        "drip failed ({}): {} {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    serde_json::from_str(String::from_utf8_lossy(&output.stdout).trim())
        .expect("the reap CLI must print one JSON object")
}

fn killed_of(payload: &serde_json::Value) -> Vec<String> {
    let mut names: Vec<String> = payload["killed"]
        .as_array()
        .expect("killed must be an array")
        .iter()
        .map(|value| value.as_str().unwrap_or_default().to_string())
        .collect();
    names.sort();
    names
}

fn keep_reason(payload: &serde_json::Value, name: &str) -> Option<String> {
    payload["keptReasons"]
        .as_array()?
        .iter()
        .find(|entry| entry["name"].as_str() == Some(name))
        .and_then(|entry| entry["reason"].as_str())
        .map(str::to_string)
}

#[test]
fn a_real_pass_kills_only_the_dead_recorded_owned_session() {
    if !tmux_available() {
        return;
    }

    let server = TmuxServer::new();
    server.dead_owned_session("drip-dead-recorded", true);
    server.dead_owned_session("drip-dead-unrecorded", false);
    server.owned_live_session("drip-still-running", "sleep 300");
    server.create("drip-legacy-unmarked", "sleep 300");
    server.create("unrelated-plain", "sleep 300");
    assert_eq!(server.sessions().len(), 5);

    // A dry run reports the one eligible session and kills nothing.
    let (output, _sandbox) = run_cli(&server, &["--reap-tmux", "--dry-run", "--json"]);
    let payload = json_of(&output);
    assert_eq!(
        payload["dryRun"],
        serde_json::Value::Bool(true),
        "{payload}"
    );
    assert_eq!(killed_of(&payload), vec!["drip-dead-recorded".to_string()]);
    assert_eq!(server.sessions().len(), 5, "a dry run must kill nothing");

    // The real pass kills exactly that session and reports why the rest stay.
    let (output, _sandbox) = run_cli(&server, &["--reap-tmux", "--json"]);
    let payload = json_of(&output);
    assert_eq!(payload["ok"], serde_json::Value::Bool(true), "{payload}");
    assert_eq!(
        payload["dryRun"],
        serde_json::Value::Bool(false),
        "{payload}"
    );
    assert_eq!(killed_of(&payload), vec!["drip-dead-recorded".to_string()]);
    assert_eq!(
        keep_reason(&payload, "drip-still-running").as_deref(),
        Some("panes-alive")
    );
    assert_eq!(
        keep_reason(&payload, "drip-dead-unrecorded").as_deref(),
        Some("pane-death-too-recent")
    );
    assert_eq!(
        keep_reason(&payload, "drip-legacy-unmarked").as_deref(),
        Some("legacy-unmarked")
    );
    assert_eq!(
        keep_reason(&payload, "unrelated-plain").as_deref(),
        Some("legacy-unmarked")
    );
    assert_eq!(
        server.sessions(),
        vec![
            "drip-dead-unrecorded".to_string(),
            "drip-legacy-unmarked".to_string(),
            "drip-still-running".to_string(),
            "unrelated-plain".to_string(),
        ]
    );

    // Reaping is idempotent: a second pass kills nothing at all.
    let (output, _sandbox) = run_cli(&server, &["--reap-tmux", "--json"]);
    let payload = json_of(&output);
    assert!(killed_of(&payload).is_empty(), "{payload}");
    assert_eq!(server.sessions().len(), 4);
}

#[test]
fn the_reap_cli_needs_no_project_config_or_inference_setup() {
    if !tmux_available() {
        return;
    }

    let server = TmuxServer::new();
    server.dead_owned_session("drip-cron-target", true);

    // Plain output names the session it reaped (no goal, no project index).
    let (output, sandbox) = run_cli(&server, &["--reap-tmux"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("reaped 1 session"), "{stdout}");
    assert!(stdout.contains("drip-cron-target"), "{stdout}");
    assert!(server.sessions().is_empty());
    // Nothing was created in the working directory (no project state).
    assert_eq!(
        std::fs::read_dir(sandbox.path().join("empty-cwd"))
            .unwrap()
            .count(),
        0,
        "--reap-tmux must not write project state"
    );

    // An empty server is a clean no-op, not an error.
    let (output, _sandbox) = run_cli(&server, &["--reap-tmux", "--json"]);
    let payload = json_of(&output);
    assert!(killed_of(&payload).is_empty(), "{payload}");

    // The exclusive-mode guard still applies before any project resolution.
    let (output, _sandbox) = run_cli(&server, &["--reap-tmux", "--list"]);
    assert!(
        !output.status.success(),
        "--reap-tmux --list must be refused"
    );
}

/// The cron case: a crontab runs with the operator's `$HOME` as the working
/// directory, and drip refuses to treat `~/.drip` as a project. The reap path
/// must never reach that gate, and must not create anything in `$HOME`.
#[test]
fn the_reap_cli_runs_with_home_as_the_working_directory_like_a_cron_entry() {
    if !tmux_available() {
        return;
    }

    let server = TmuxServer::new();
    server.dead_owned_session("drip-cron-from-home", true);

    // HOME and the working directory are the same directory — the shape a
    // crontab gives every command.
    let home = tempfile::tempdir().expect("a home dir");
    let output = Command::new(DRIP_BIN)
        .args(["--reap-tmux", "--json"])
        .current_dir(home.path())
        .env("HOME", home.path())
        .env("TMUX_TMPDIR", server.socket())
        .env_remove("TMUX")
        .env_remove("DRIP_PROJECT_DIR")
        .output()
        .expect("the drip binary must run");

    assert!(
        output.status.success(),
        "reaping from $HOME must exit 0 ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let payload = json_of(&output);
    assert_eq!(payload["ok"], serde_json::Value::Bool(true), "{payload}");
    assert_eq!(
        killed_of(&payload),
        vec!["drip-cron-from-home".to_string()],
        "{payload}"
    );
    assert!(server.sessions().is_empty());
    assert!(
        std::fs::read_dir(home.path()).unwrap().count() == 0,
        "--reap-tmux must not create home or project state"
    );

    // A dry run from the same shape behaves the same way.
    let dry_home = tempfile::tempdir().expect("a home dir");
    let output = Command::new(DRIP_BIN)
        .args(["--reap-tmux", "--dry-run", "--json"])
        .current_dir(dry_home.path())
        .env("HOME", dry_home.path())
        .env("TMUX_TMPDIR", server.socket())
        .env_remove("TMUX")
        .env_remove("DRIP_PROJECT_DIR")
        .output()
        .expect("the drip binary must run");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let payload = json_of(&output);
    assert_eq!(
        payload["dryRun"],
        serde_json::Value::Bool(true),
        "{payload}"
    );

    // The exclusive-mode guard still runs before the reap dispatch.
    let output = Command::new(DRIP_BIN)
        .args(["--reap-tmux", "--list"])
        .current_dir(dry_home.path())
        .env("HOME", dry_home.path())
        .env("TMUX_TMPDIR", server.socket())
        .env_remove("TMUX")
        .output()
        .expect("the drip binary must run");
    assert!(
        !output.status.success(),
        "--reap-tmux --list must still be refused"
    );
}

/// A machine where the tmux server was never started: nothing to reap, no
/// error. This is the ordinary state of a cron environment, and the pass must
/// stay quiet and exit 0 rather than fail every tick.
#[test]
fn a_tmux_server_that_was_never_started_is_a_clean_no_op() {
    if !tmux_available() {
        return;
    }

    // A socket directory with no server in it: tmux answers "error connecting".
    let socket = tempfile::tempdir().expect("a socket dir");
    let home = tempfile::tempdir().expect("a home dir");
    let output = Command::new(DRIP_BIN)
        .args(["--reap-tmux", "--json"])
        .current_dir(home.path())
        .env("HOME", home.path())
        .env("TMUX_TMPDIR", socket.path())
        .env_remove("TMUX")
        .env_remove("DRIP_PROJECT_DIR")
        .output()
        .expect("the drip binary must run");

    assert!(
        output.status.success(),
        "an absent tmux server must exit 0 ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let payload = json_of(&output);
    assert_eq!(payload["ok"], serde_json::Value::Bool(true), "{payload}");
    assert!(killed_of(&payload).is_empty(), "{payload}");
}

#[test]
fn a_session_that_vanishes_mid_pass_is_kept_and_the_rest_still_reaps() {
    if !tmux_available() {
        return;
    }

    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let server = TmuxServer::new();
    server.dead_owned_session("drip-vanishing", true);
    server.dead_owned_session("drip-survivor", true);

    let previous = std::env::var_os("TMUX_TMPDIR");
    std::env::set_var("TMUX_TMPDIR", server.socket());
    std::env::remove_var("TMUX");

    let listed = list_tmux_sessions().expect("listing a real server must succeed");
    assert_eq!(listed.len(), 2, "{listed:?}");

    // The first session disappears between the listing and the probe: a partial
    // failure the pass must tolerate.
    server.ok(&["kill-session", "-t", "drip-vanishing"]);

    let result = reap_with(
        &listed,
        now_epoch_ms(),
        false,
        ReapPolicy::default(),
        &probe_tmux_session,
        &|name: &str| {
            drip::tools::builtin::bash::kill_tmux_session(name).map_err(|error| error.to_string())
        },
    );

    assert_eq!(result.killed, vec!["drip-survivor".to_string()]);
    assert!(
        result
            .kept_reasons
            .contains(&("drip-vanishing".to_string(), "vanished".to_string())),
        "{:?}",
        result.kept_reasons
    );
    assert!(server.sessions().is_empty());

    // And a pass over the now-empty server is a clean no-op.
    let empty = reap_tmux_sessions_with_policy(false, ReapPolicy::default())
        .expect("reaping an empty server must succeed");
    assert!(
        empty.killed.is_empty() && empty.kept.is_empty(),
        "{empty:?}"
    );
}

#[test]
fn a_missing_tmux_is_not_an_error() {
    let (output, _sandbox) = {
        let sandbox = tempfile::tempdir().unwrap();
        let output = Command::new(DRIP_BIN)
            .args(["--reap-tmux", "--json"])
            .current_dir(sandbox.path())
            .env("PATH", "")
            .env_remove("TMUX")
            .output()
            .expect("the drip binary must run");
        (output, sandbox)
    };

    let payload = json_of(&output);
    assert_eq!(payload["ok"], serde_json::Value::Bool(true), "{payload}");
    assert!(killed_of(&payload).is_empty(), "{payload}");
}

fn message() -> drip::chat::types::ChatMessage {
    drip::chat::types::ChatMessage {
        blocks: vec![],
        context_files: None,
        context_state: None,
        created_at: None,
        failed: None,
        id: "m1".to_string(),
        pending: None,
        reply_to_message_id: None,
        role: drip::chat::types::ChatRole::User,
        tags: None,
        transport_state: None,
    }
}

/// Runs one built-in tool in `cwd` against runtime services rooted there.
fn run_tool(
    tool_name: &str,
    cwd: &Path,
    raw_input: String,
    services: &ChatToolRuntimeServices,
) -> String {
    let tools = builtin_tool_pack(BuiltinToolOptions::default());
    let message = message();
    let executed = execute_tool_call(ToolExecutionContext {
        call_id: "c1",
        history: &[],
        message: &message,
        raw_input: &raw_input,
        runtime_context: drip::chat::types::ChatRuntimeContext {
            cwd: cwd.to_string_lossy().to_string(),
            working_file: drip::chat::types::WorkingFileContext {
                exists: false,
                path: String::new(),
                scope: drip::chat::types::WorkingFileScope::Cwd,
                text: None,
            },
        },
        services: services.clone(),
        tool: tools.iter().find(|tool| tool.name == tool_name),
        tool_name,
    });

    executed.tool_content
}

/// Polls a job until it leaves the running state (the tool's own finalization
/// happens on a worker thread, so the call can return just before it settles).
fn wait_for_settled_job(services: &ChatToolRuntimeServices, job_id: &str) -> ChatAsyncToolJob {
    let started = Instant::now();

    while started.elapsed() < Duration::from_secs(10) {
        if let Some(job) = services.async_jobs.get_job(job_id) {
            if job.status != ChatAsyncToolJobStatus::Running {
                return job;
            }
        }

        std::thread::sleep(Duration::from_millis(20));
    }

    panic!("job {job_id} never settled");
}

fn services_for(cwd: &Path) -> ChatToolRuntimeServices {
    create_chat_tool_runtime_services(CreateChatToolRuntimeServicesOptions {
        cwd: Some(cwd.to_path_buf()),
        jobs_root: Some(cwd.join("jobs")),
    })
}

fn job_id_for_session(services: &ChatToolRuntimeServices, session_name: &str) -> String {
    services
        .tmux_sessions
        .list_sessions()
        .into_iter()
        .find(|session| session.session_name == session_name)
        .unwrap_or_else(|| panic!("no job registered for tmux session {session_name}"))
        .job_id
}

#[test]
fn a_finished_bash_async_job_is_collected_and_its_result_stays_readable() {
    if !tmux_available() {
        return;
    }

    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let server = TmuxServer::new();
    let _restore = SocketRestore(set_process_socket(server.socket()));

    let cwd = tempfile::tempdir().unwrap();
    let services = services_for(cwd.path());
    let session_name = "drip-e2e-finished";
    let input = format!(
        "{{\"command\":\"echo done-from-async\",\"sessionName\":\"{session_name}\",\"cwd\":\"{}\",\"waitMs\":15000}}",
        cwd.path().display()
    );

    let content = run_tool("BASH_ASYNC", cwd.path(), input, &services);
    assert!(
        content.contains("finished") && content.contains("done-from-async"),
        "{content}"
    );

    // The completion path stamps the durable result and collects the session.
    let job_id = job_id_for_session(&services, session_name);
    let settled = wait_for_settled_job(&services, &job_id);
    assert_ne!(settled.status, ChatAsyncToolJobStatus::Running);
    assert_eq!(settled.exit_code, Some(Some(0)));
    let log = std::fs::read_to_string(&settled.log_path).expect("the job log must be readable");
    assert!(log.contains("done-from-async"), "{log}");
    assert!(log.contains("[tmux-exit]"), "{log}");
    assert!(
        !tmux_session_exists(&server, session_name),
        "a finished job's session must not dangle"
    );

    // The result is still readable after the session is gone.
    let tail = services
        .async_jobs
        .tail_job(&job_id, Some(60))
        .expect("the job record must outlive its tmux session");
    assert!(tail.output.contains("done-from-async"), "{}", tail.output);
}

#[test]
fn a_running_bash_async_job_survives_a_pass_and_its_result_outlives_cleanup() {
    if !tmux_available() {
        return;
    }

    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let server = TmuxServer::new();
    let _restore = SocketRestore(set_process_socket(server.socket()));

    let cwd = tempfile::tempdir().unwrap();
    let services = services_for(cwd.path());
    let session_name = "drip-e2e-running";
    let input = format!(
        "{{\"command\":\"sleep 30\",\"sessionName\":\"{session_name}\",\"cwd\":\"{}\",\"waitMs\":200}}",
        cwd.path().display()
    );

    let content = run_tool("BASH_ASYNC", cwd.path(), input, &services);
    assert!(content.contains("still running"), "{content}");
    assert!(tmux_session_exists(&server, session_name), "{content}");

    // Work that is still in flight is never reaped, however often a pass runs.
    for _ in 0..2 {
        let result = reap_tmux_sessions_with_policy(false, ReapPolicy::default())
            .expect("reaping a real server must succeed");
        assert!(result.killed.is_empty(), "{result:?}");
        assert!(
            result
                .kept_reasons
                .contains(&(session_name.to_string(), "panes-alive".to_string())),
            "{:?}",
            result.kept_reasons
        );
        assert!(tmux_session_exists(&server, session_name));
    }

    // Its result is readable while it runs, and still after the session goes.
    let job_id = job_id_for_session(&services, session_name);
    let running = services
        .async_jobs
        .wait_for_job(&job_id, Some(0))
        .expect("a running job must be waitable");
    assert!(!running.completed);

    drip::tools::builtin::bash::kill_tmux_session(session_name).expect("cleanup must work");
    assert!(!tmux_session_exists(&server, session_name));
    assert!(
        services.async_jobs.get_job(&job_id).is_some(),
        "the job record must outlive the tmux session"
    );
    let log = std::fs::read_to_string(cwd.path().join("jobs").join(format!("{job_id}.log")))
        .expect("the job log must survive the tmux session");
    assert!(log.contains("[session]"), "{log}");
}

#[test]
fn monitor_work_rides_no_tmux_session_and_survives_a_pass() {
    if !tmux_available() {
        return;
    }

    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let server = TmuxServer::new();
    let _restore = SocketRestore(set_process_socket(server.socket()));
    server.dead_owned_session("drip-bystander", true);

    let cwd = tempfile::tempdir().unwrap();
    let services = services_for(cwd.path());
    let input = format!(
        "{{\"check\":\"test -f never.txt\",\"intervalMs\":50,\"timeoutMs\":800,\"description\":\"a signal that never comes\",\"cwd\":\"{}\"}}",
        cwd.path().display()
    );

    let content = run_tool("MONITOR", cwd.path(), input, &services);
    assert!(
        content.contains("Monitor started") || content.contains("did not appear"),
        "{content}"
    );

    // A pass reaps the dead bystander and leaves the monitor's own work alone.
    let result = reap_tmux_sessions_with_policy(false, ReapPolicy::default())
        .expect("reaping a real server must succeed");
    assert_eq!(result.killed, vec!["drip-bystander".to_string()]);

    // The monitor's settled result is durable on disk: wait for its log to
    // carry the attempt trail, then check that the reap pass left it intact.
    let jobs_root = cwd.path().join("jobs");
    let started = Instant::now();
    let mut log_text = String::new();

    while started.elapsed() < Duration::from_secs(15) {
        if let Ok(entries) = std::fs::read_dir(&jobs_root) {
            for entry in entries.flatten() {
                let text = std::fs::read_to_string(entry.path()).unwrap_or_default();

                if text.contains("attempt") {
                    log_text = text;
                    break;
                }
            }
        }

        if !log_text.is_empty() {
            break;
        }

        std::thread::sleep(Duration::from_millis(50));
    }

    assert!(
        log_text.contains("attempt") && log_text.contains("[monitor]"),
        "the monitor's settled log must survive the pass: {log_text}"
    );
    // A monitor rides no tmux session, so its log carries no session line.
    assert!(!log_text.contains("[session]"), "{log_text}");
}

/// Points the process (and so the library/tool-pack tmux calls) at `socket`,
/// returning the previous value so the caller can restore it.
fn set_process_socket(socket: &Path) -> Option<std::ffi::OsString> {
    let previous = std::env::var_os("TMUX_TMPDIR");
    std::env::set_var("TMUX_TMPDIR", socket);
    std::env::remove_var("TMUX");
    previous
}

/// Restores what `set_process_socket` replaced. The library assertions in
/// between can panic, and the panicking thread would otherwise leave this
/// process pointed at a socket directory that is being deleted, breaking every
/// later test that reads `TMUX_TMPDIR`. `Drop` restores on both paths.
struct SocketRestore(Option<std::ffi::OsString>);

impl Drop for SocketRestore {
    fn drop(&mut self) {
        // The suite serializes these tests through ENV_LOCK, so no other
        // thread is touching the environment here (same reasoning as
        // `set_process_socket`).
        match self.0.take() {
            Some(value) => std::env::set_var("TMUX_TMPDIR", value),
            None => std::env::remove_var("TMUX_TMPDIR"),
        }
    }
}

fn tmux_session_exists(server: &TmuxServer, name: &str) -> bool {
    server.sessions().iter().any(|session| session == name)
}
#[test]
fn an_exact_target_never_resolves_to_a_longer_same_prefix_session() {
    if !tmux_available() {
        return;
    }

    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let server = TmuxServer::new();
    let _restore = SocketRestore(set_process_socket(server.socket()));

    // Only the LONGER name exists. tmux resolves a bare `-t` target by prefix
    // match, so `drip-e2e-collide` would otherwise address this live session.
    server.owned_live_session("drip-e2e-collide-long", "sleep 300");

    assert!(
        !drip::tools::builtin::bash::tmux_session_exists("drip-e2e-collide"),
        "an existence check must not see a same-prefix session"
    );

    let probe = probe_tmux_session("drip-e2e-collide");
    assert!(
        !probe.exists,
        "a missing session must probe as missing: {probe:?}"
    );

    assert!(
        !drip::tools::tmux_reap::reap_session_if_eligible("drip-e2e-collide")
            .expect("reaping a missing session must not error"),
        "a missing session has nothing to reap"
    );

    drip::tools::builtin::bash::kill_tmux_session("drip-e2e-collide")
        .expect("killing a missing session must be a no-op, not an error");

    assert!(
        tmux_session_exists(&server, "drip-e2e-collide-long"),
        "the prefix sibling must survive: {:?}",
        server.sessions()
    );
}

#[test]
fn a_live_second_window_keeps_the_session() {
    if !tmux_available() {
        return;
    }

    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let server = TmuxServer::new();
    let _restore = SocketRestore(set_process_socket(server.socket()));
    let name = "drip-e2e-two-windows";

    // Window 1 is dead (and its result recorded, so age never matters). A
    // *detached* second window is still running, so the session is in use:
    // only `list-panes -s` can see it.
    server.dead_owned_session(name, true);
    server.ok(&[
        "new-window",
        "-d",
        "-t",
        "=drip-e2e-two-windows:",
        "sleep 300",
    ]);

    let result = reap_tmux_sessions_with_policy(false, ReapPolicy::default())
        .expect("reaping a real server must succeed");

    assert!(
        !result.killed.contains(&name.to_string()),
        "a live window must keep its session: {:?}",
        result.killed
    );
    assert!(
        result
            .kept_reasons
            .contains(&(name.to_string(), "panes-alive".to_string())),
        "the keep reason must name the live pane: {:?}",
        result.kept_reasons
    );
    assert!(tmux_session_exists(&server, name));
}
