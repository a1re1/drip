// The one process-capture implementation every command-running tool shares
// (debt audit C1: bash-tool, verify-tool, and async-jobs each carried a copy,
// and the copies had already drifted — VERIFY's missed the #56 stop fix).
// Semantics: own process group (kills reach grandchildren), SIGTERM →
// SIGKILL escalation on timeout, settle-with-captured-output instead of
// hanging on survivors, and a process-wide terminator registry so a stop
// signal to drip interrupts every in-flight child.
//
// Rust implementation notes:
// - The result waits for the child to exit and for the reader threads to
//   drain the stdio pipes; a still-open pipe (a surviving grandchild) only
//   delays the result up to the bounded join, never hangs it.
// - `kill_tree` below signals the negative pid (the whole process group) and
//   falls back to the direct child kill when the group is gone.
// - A real POSIX signal handler may only touch atomics, so `on_stop_signal`
//   just sets STOP_SIGNAL_FIRED; the poll loop of each in-flight child
//   observes it and terminates, so a stop signal to drip interrupts every
//   in-flight child.

use std::collections::BTreeMap;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::thread;
use std::time::{Duration, Instant};

use crate::tools::child_env::build_child_process_env;

/// Tmux sessions the harness starts carry this prefix; drip gc reaps by it.
pub const TMUX_PREFIX: &str = "drip-";

#[derive(Debug)]
pub struct CapturedProcessResult {
    pub exit_code: Option<i32>,
    pub signal: Option<String>,
    pub stderr: String,
    pub stdout: String,
    pub timed_out: bool,
}

type ProcessTerminator = Box<dyn FnOnce() + Send>;

struct ActiveProcessTerminators {
    next_id: u64,
    active: BTreeMap<u64, ProcessTerminator>,
}

impl ActiveProcessTerminators {
    fn new() -> Self {
        ActiveProcessTerminators {
            next_id: 0,
            active: BTreeMap::new(),
        }
    }

    fn add(&mut self, terminate: ProcessTerminator) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.active.insert(id, terminate);
        id
    }

    fn remove(&mut self, id: u64) {
        self.active.remove(&id);
    }
}

static ACTIVE_PROCESS_TERMINATORS: Mutex<Option<ActiveProcessTerminators>> = Mutex::new(None);
static STOP_SIGNAL_HANDLERS_INSTALLED: Once = Once::new();
static STOP_SIGNAL_FIRED: AtomicBool = AtomicBool::new(false);

fn with_terminators<T>(apply: impl FnOnce(&mut ActiveProcessTerminators) -> T) -> T {
    let mut guard = ACTIVE_PROCESS_TERMINATORS.lock().unwrap();
    let slots = guard.get_or_insert_with(ActiveProcessTerminators::new);
    apply(slots)
}

pub fn terminate_active_processes() -> usize {
    let terminators: Vec<ProcessTerminator> = with_terminators(|slots| {
        let active = std::mem::take(&mut slots.active);
        active.into_iter().map(|(_, terminate)| terminate).collect()
    });

    let count = terminators.len();
    for terminate in terminators {
        terminate();
    }

    count
}

pub fn install_stop_signal_handlers() {
    STOP_SIGNAL_HANDLERS_INSTALLED.call_once(|| {
        extern "C" fn on_stop_signal(_signal: i32) {
            // Inside a signal handler we may only touch atomics; the actual
            // termination runs from the poll loop of the in-flight children.
            STOP_SIGNAL_FIRED.store(true, Ordering::SeqCst);
        }

        unsafe {
            libc::signal(libc::SIGTERM, on_stop_signal as libc::sighandler_t);
            libc::signal(libc::SIGINT, on_stop_signal as libc::sighandler_t);
        }
    });
}

/// Registers an external in-flight child; returns the unregister function.
pub fn register_process_terminator(terminate: ProcessTerminator) -> impl FnOnce() + Send {
    install_stop_signal_handlers();

    let id = with_terminators(|slots| slots.add(terminate));

    move || {
        with_terminators(|slots| slots.remove(id));
    }
}

pub struct CapturedProcessArgs<'a> {
    pub command: &'a str,
    pub cwd: Option<&'a str>,
    pub env: Option<&'a BTreeMap<String, String>>,
    pub process_args: &'a [String],
    pub timeout_ms: Option<u64>,
    /// Optional bytes written to the child's stdin; stdin is then closed so
    /// the child observes EOF. `None` keeps stdin disconnected (as before).
    pub stdin_payload: Option<&'a str>,
}

pub fn run_captured_process(args: &CapturedProcessArgs) -> Result<CapturedProcessResult, String> {
    install_stop_signal_handlers();

    let mut child = spawn_detached(
        args.command,
        args.process_args,
        args.cwd,
        args.env,
        args.stdin_payload,
    )
    .map_err(|error| error.to_string())?;

    let pid = child.id();

    // Write the stdin payload (if any) and close stdin so the child observes
    // EOF instead of blocking on read. Done before the wait loop so a child
    // that fills its stdout pipe cannot deadlock us mid-write.
    if let Some(payload) = args.stdin_payload {
        use std::io::Write;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(payload.as_bytes());
        }
    }

    // The terminator registered for this child: on an external stop the poll
    // loop SIGTERMs the group, SIGKILLs after 1s, and settles after 1.5s
    // with whatever was captured. The flag keeps the closure safe to call
    // from a real signal handler; the deadlines live in the poll loop.
    let terminate_now = Arc::new(AtomicBool::new(false));
    let terminator_flag = Arc::clone(&terminate_now);
    let terminator = Box::new(move || {
        terminator_flag.store(true, Ordering::SeqCst);
    });

    let mut guard = ACTIVE_PROCESS_TERMINATORS.lock().unwrap();
    let slots = guard.get_or_insert_with(ActiveProcessTerminators::new);
    let terminate_now_id = slots.add(terminator);
    drop(guard);

    let result = wait_with_pipes(
        &mut child,
        args.timeout_ms,
        terminate_now_id,
        &terminate_now,
        pid,
    );

    // Both the `close` and `error` paths delete the terminator before
    // settling; do the same here.
    with_terminators(|slots| slots.remove(terminate_now_id));

    Ok(result)
}

/// Spawn with its own process group, so a timeout kill reaches grandchildren
/// too — an orphaned `sleep` used to hold the stdio pipes open and stall the
/// result long past the kill.
fn spawn_detached(
    command: &str,
    process_args: &[String],
    cwd: Option<&str>,
    env: Option<&BTreeMap<String, String>>,
    stdin_payload: Option<&str>,
) -> std::io::Result<Child> {
    let child_env = build_child_process_env(env);

    let mut command_builder = Command::new(command);
    command_builder
        .args(process_args)
        // Its own process group, so a timeout kill reaches grandchildren too.
        .process_group(0)
        .env_clear()
        .envs(child_env.iter().map(|(key, value)| (key, value)))
        .stdin(if stdin_payload.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if let Some(cwd) = cwd {
        command_builder.current_dir(cwd);
    }

    command_builder.spawn()
}

/// Signal the negative pid (the whole process group) first, and fall through
/// to the direct kill when the group is gone or the child is not the leader.
fn kill_tree(child: &mut Child, signal: i32) {
    let pid = child.id() as i32;
    // process.kill(-pid, signal) — the group kill.
    unsafe {
        if libc::kill(-pid, signal) == 0 {
            return;
        }
    }
    // child.kill(signal) — the direct kill.
    unsafe {
        libc::kill(pid, signal);
    }
}

enum Outcome {
    Exited {
        exit_code: Option<i32>,
        signal: Option<String>,
    },
    KilledBySignal {
        signal: String,
    },
}

fn wait_with_pipes(
    child: &mut Child,
    timeout_ms: Option<u64>,
    terminate_now_id: u64,
    terminate_now: &AtomicBool,
    _pid: u32,
) -> CapturedProcessResult {
    let _ = terminate_now_id;

    // Take the pipe ends and drain them from reader threads into shared
    // buffers, so the parent can keep polling the child without blocking on
    // the pipes, and a settle deadline can still read whatever was captured
    // when a surviving grandchild holds a pipe open.
    let stdout_handle = spawn_pipe_reader(child.stdout.take());
    let stderr_handle = spawn_pipe_reader(child.stderr.take());

    let mut timed_out = false;
    let mut terminate_signal_sent: Option<Instant> = None;
    let mut settle_by: Option<Instant> = None;
    let mut killed_by_external_stop = false;

    let poll_interval = Duration::from_millis(25);
    let started = Instant::now();

    // Poll until the child has exited AND both pipes hit EOF (a grandchild
    // that inherited the pipes keeps them open after the shell itself
    // exits), or a settle deadline passes, or an
    // external stop fires. The timeout keeps running after the exit, so a
    // `sleep 30 &` left holding stdout is group-killed and reported as
    // timed_out with the shell's own exit code (measured 2026-09-02).
    let mut exited: Option<(Option<i32>, Option<String>)> = None;
    let outcome = loop {
        if STOP_SIGNAL_FIRED.load(Ordering::SeqCst) {
            terminate_active_processes();
        }

        if exited.is_none() {
            if let Ok(Some(status)) = child.try_wait() {
                exited = Some(status_from_exit(&status));
            }
        }

        if let Some((exit_code, signal)) = exited.clone() {
            if stdout_handle.eof.load(Ordering::Acquire) && stderr_handle.eof.load(Ordering::Acquire) {
                break Outcome::Exited { exit_code, signal };
            }
        }

        let now = Instant::now();

        if terminate_now.load(Ordering::SeqCst) && !killed_by_external_stop {
            // External stop: group SIGTERM, SIGKILL after 1s, settle after
            // 1.5s with whatever was captured.
            killed_by_external_stop = true;
            kill_tree(child, libc::SIGTERM);
            *settle_by.get_or_insert(now + Duration::from_millis(1_500)) =
                now + Duration::from_millis(1_500);
            terminate_signal_sent.get_or_insert(now);
        }

        if let Some(timeout_ms) = timeout_ms {
            let timeout_at = started + Duration::from_millis(timeout_ms);
            if now >= timeout_at && terminate_signal_sent.is_none() && !timed_out {
                timed_out = true;
                terminate_signal_sent = Some(now);
                kill_tree(child, libc::SIGTERM);
                settle_by.get_or_insert(now + Duration::from_millis(3_000));
            }
        }

        if let Some(sent_at) = terminate_signal_sent {
            // forceKillTimeout: SIGKILL 1s after the SIGTERM.
            if now >= sent_at + Duration::from_millis(1_000) {
                kill_tree(child, libc::SIGKILL);
            }
        }

        if let Some(settle_by) = settle_by {
            if now >= settle_by {
                // Belt and braces: if surviving grandchildren still hold the
                // stdio pipes open after the kills, settle with what was
                // captured instead of hanging until they exit.
                break match exited.clone() {
                    Some((exit_code, signal)) => Outcome::Exited { exit_code, signal },
                    None => Outcome::KilledBySignal {
                        signal: "SIGTERM".to_string(),
                    },
                };
            }
        }

        thread::sleep(poll_interval);
    };

    let (exit_code, signal, extra_wait) = match outcome {
        Outcome::Exited { exit_code, signal } => (exit_code, signal, Duration::from_millis(750)),
        Outcome::KilledBySignal { signal } => (None, Some(signal), Duration::from_millis(0)),
    };

    // Wait briefly for the reader threads so captured output is included;
    // on a settle deadline the captured text is whatever arrived before it.
    let stdout = join_pipe(stdout_handle, extra_wait);
    let stderr = join_pipe(stderr_handle, extra_wait);

    CapturedProcessResult {
        exit_code,
        signal,
        stderr,
        stdout,
        timed_out,
    }
}

fn status_from_exit(status: &std::process::ExitStatus) -> (Option<i32>, Option<String>) {
    use std::os::unix::process::ExitStatusExt;

    if let Some(code) = status.code() {
        (Some(code), None)
    } else if let Some(signal) = status.signal() {
        (None, Some(signal_name(signal).to_string()))
    } else {
        (None, None)
    }
}

fn signal_name(signal: i32) -> &'static str {
    match signal {
        libc::SIGHUP => "SIGHUP",
        libc::SIGINT => "SIGINT",
        libc::SIGQUIT => "SIGQUIT",
        libc::SIGILL => "SIGILL",
        libc::SIGTRAP => "SIGTRAP",
        libc::SIGABRT => "SIGABRT",
        libc::SIGBUS => "SIGBUS",
        libc::SIGFPE => "SIGFPE",
        libc::SIGKILL => "SIGKILL",
        libc::SIGUSR1 => "SIGUSR1",
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGUSR2 => "SIGUSR2",
        libc::SIGPIPE => "SIGPIPE",
        libc::SIGALRM => "SIGALRM",
        libc::SIGTERM => "SIGTERM",
        libc::SIGCHLD => "SIGCHLD",
        libc::SIGCONT => "SIGCONT",
        libc::SIGSTOP => "SIGSTOP",
        libc::SIGTSTP => "SIGTSTP",
        libc::SIGTTIN => "SIGTTIN",
        libc::SIGTTOU => "SIGTTOU",
        _ => "SIGUNKNOWN",
    }
}

/// A pipe drained by a reader thread into a shared buffer, so the captured
/// text survives even when the join gives up on survivors and the result
/// settles with whatever was captured while a pipe may still be open.
struct CapturedPipe {
    buffer: Arc<Mutex<Vec<u8>>>,
    eof: Arc<AtomicBool>,
}

fn spawn_pipe_reader<R: Read + Send + 'static>(pipe: Option<R>) -> CapturedPipe {
    let captured = CapturedPipe {
        buffer: Arc::new(Mutex::new(Vec::new())),
        eof: Arc::new(AtomicBool::new(false)),
    };

    if let Some(mut pipe) = pipe {
        let buffer = Arc::clone(&captured.buffer);
        let eof = Arc::clone(&captured.eof);
        thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        if let Ok(mut buffer) = buffer.lock() {
                            buffer.extend_from_slice(&chunk[..read]);
                        }
                    }
                }
            }

            // The child and all its grandchildren must close their copies of
            // the pipe write end before the EOF flag is set.
            eof.store(true, Ordering::Release);
        });
    } else {
        captured.eof.store(true, Ordering::Release);
    }

    captured
}

/// Waits up to `patience` for the reader to reach EOF, then returns the
/// captured text — on a settle deadline that is whatever arrived before it.
fn join_pipe(pipe: CapturedPipe, patience: Duration) -> String {
    let started = Instant::now();
    while !pipe.eof.load(Ordering::Acquire) {
        if started.elapsed() >= patience {
            break;
        }

        thread::sleep(Duration::from_millis(5));
    }

    let buffer = pipe
        .buffer
        .lock()
        .map(|buffer| buffer.clone())
        .unwrap_or_default();
    String::from_utf8_lossy(&buffer).into_owned()
}

pub fn build_combined_output(stdout: &str, stderr: &str) -> String {
    let normalized_stdout = stdout.trim_end();
    let normalized_stderr = stderr.trim_end();

    let mut sections: Vec<String> = Vec::new();

    if !normalized_stdout.is_empty() {
        sections.push(normalized_stdout.to_string());
    }

    if !normalized_stderr.is_empty() {
        if !normalized_stdout.is_empty() {
            sections.push(format!("[stderr]\n{}", normalized_stderr));
        } else {
            sections.push(normalized_stderr.to_string());
        }
    }

    sections.join("\n\n")
}

/// Test-only: `terminate_active_processes` sweeps EVERY in-flight child in
/// the process, so a test that fires it must not overlap a test whose child
/// is still running (it would report a spurious "KILLED by SIGTERM").
/// Registry tests and timeout tests across modules hold this lock.
#[cfg(test)]
pub(crate) static REGISTRY_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn captured_process_result_captures_stdout_stderr_and_exit_code() {
        let process_args = owned(&["-c", "echo out; echo err 1>&2; exit 3"]);
        let args = CapturedProcessArgs { stdin_payload: None,
            command: "/bin/sh",
            cwd: None,
            env: None,
            process_args: &process_args,
            timeout_ms: None,
        };
        let result = run_captured_process(&args).expect("spawn failed");

        assert_eq!(result.exit_code, Some(3));
        assert_eq!(result.signal, None);
        assert_eq!(result.stdout.trim_end(), "out");
        assert_eq!(result.stderr.trim_end(), "err");
        assert!(!result.timed_out);
    }

    #[test]
    fn captured_process_honors_cwd_and_empty_process_args() {
        let args = CapturedProcessArgs { stdin_payload: None,
            command: "/bin/pwd",
            cwd: Some("/"),
            env: None,
            process_args: &[],
            timeout_ms: None,
        };
        let result = run_captured_process(&args).expect("spawn failed");

        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.stdout.trim_end(), "/");
    }

    #[test]
    fn timeout_sigterms_the_group_and_reports_the_reaping_signal_with_timed_out() {
        let _guard = REGISTRY_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // `sh` ignores TERM (and `sleep` inherits the ignore across exec), so
        // only the SIGKILL escalation 1s after the SIGTERM reaps it. Measured
        // (2026-09-02): { exit_code: None, signal: Some("SIGKILL"),
        // timed_out: true } in ~1.3s — the signal reported is the one that
        // actually reaped the child, not the one first sent.
        let process_args = owned(&["-c", "trap '' TERM; echo started; sleep 30"]);
        let args = CapturedProcessArgs { stdin_payload: None,
            command: "/bin/sh",
            cwd: None,
            env: None,
            process_args: &process_args,
            timeout_ms: Some(300),
        };
        let result = run_captured_process(&args).expect("spawn failed");

        assert_eq!(result.exit_code, None);
        assert_eq!(result.signal, Some("SIGKILL".to_string()));
        assert!(result.timed_out);
    }

    #[test]
    fn timeout_kill_reaches_grandchildren_in_the_process_group() {
        let _guard = REGISTRY_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // `sh` exits at once but the backgrounded sleeps hold the stdio pipes,
        // so the result cannot settle until the timeout group-kills them.
        // Measured (2026-09-02): { exit_code: Some(0), signal: None,
        // timed_out: true } in ~0.3s — the shell's own exit code survives, and
        // timed_out records that the kill is what freed the pipes.
        let process_args = owned(&["-c", "sleep 30 & sleep 30 & echo started"]);
        let args = CapturedProcessArgs { stdin_payload: None,
            command: "/bin/sh",
            cwd: None,
            env: None,
            process_args: &process_args,
            timeout_ms: Some(300),
        };
        let started = std::time::Instant::now();
        let result = run_captured_process(&args).expect("spawn failed");

        assert!(result.timed_out);
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.signal, None);
        assert_eq!(result.stdout.trim(), "started");
        assert!(started.elapsed() < std::time::Duration::from_secs(5), "settled in {:?}", started.elapsed());
    }

    #[test]
    fn env_override_reaches_the_child() {
        let mut env = BTreeMap::new();
        env.insert("DRIP_TEST_VALUE".to_string(), "42".to_string());
        let process_args = owned(&["-c", "echo $DRIP_TEST_VALUE"]);
        let args = CapturedProcessArgs { stdin_payload: None,
            command: "/bin/sh",
            cwd: None,
            env: Some(&env),
            process_args: &process_args,
            timeout_ms: None,
        };
        let result = run_captured_process(&args).expect("spawn failed");

        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.stdout.trim_end(), "42");
    }

    #[test]
    fn missing_command_rejects_with_an_error() {
        let args = CapturedProcessArgs { stdin_payload: None,
            command: "/definitely/not/a/real/binary",
            cwd: None,
            env: None,
            process_args: &[],
            timeout_ms: None,
        };
        let error = run_captured_process(&args).expect_err("spawn should fail");

        // The spawn error surfaces as the call's Err.
        assert!(
            error.contains("No such file"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn register_process_terminator_registers_then_unregisters() {
        let _guard = REGISTRY_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Other tests spawn processes concurrently (each registers a
        // terminator), so assert on the delta this test causes, not on zero.
        let before = with_terminators(|slots| slots.active.len());
        let unregister = register_process_terminator(Box::new(|| {
            panic!("terminator should not run on unregister");
        }));
        assert_eq!(with_terminators(|slots| slots.active.len()), before + 1);

        unregister();

        let count = with_terminators(|slots| slots.active.len());
        assert_eq!(count, before);
    }

    #[test]
    fn terminate_active_processes_runs_every_registered_terminator() {
        let _guard = REGISTRY_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let fired = Arc::new(AtomicBool::new(false));
        let fired_flag = Arc::clone(&fired);
        let _unregister = register_process_terminator(Box::new(move || {
            fired_flag.store(true, Ordering::SeqCst);
        }));

        let count = terminate_active_processes();

        // At least ours ran; concurrent process tests may add their own.
        assert!(count >= 1, "count = {count}");
        assert!(fired.load(Ordering::SeqCst));
    }

    #[test]
    fn tmux_prefix_is_drip_branded() {
        assert_eq!(TMUX_PREFIX, "drip-");
    }

    #[test]
    fn build_combined_output_joins_sections_with_a_blank_line() {
        assert_eq!(build_combined_output("out\n", "err\n"), "out\n\n[stderr]\nerr");
    }

    #[test]
    fn build_combined_output_stdout_only() {
        assert_eq!(build_combined_output("out", ""), "out");
    }

    #[test]
    fn build_combined_output_stderr_only_has_no_label() {
        assert_eq!(build_combined_output("", "err"), "err");
    }

    #[test]
    fn build_combined_output_empty_stays_empty() {
        assert_eq!(build_combined_output("", ""), "");
    }

    #[test]
    fn build_combined_output_trims_trailing_whitespace_only() {
        assert_eq!(
            build_combined_output("  out  \n\n", "  err  \n\n"),
            "  out\n\n[stderr]\n  err"
        );
    }

    #[test]
    fn build_combined_output_leading_whitespace_is_kept() {
        assert_eq!(
            build_combined_output("\nout", "\nerr"),
            "\nout\n\n[stderr]\n\nerr"
        );
    }

    #[test]
    fn captured_process_writes_stdin_then_closes_it() {
        let process_args = vec!["-c".to_string(), "cat".to_string()];
        let args = CapturedProcessArgs {
            command: "/bin/sh",
            cwd: None,
            env: None,
            process_args: &process_args,
            timeout_ms: Some(2_000),
            stdin_payload: Some("payload-through-stdin"),
        };
        let result = run_captured_process(&args).expect("cat should run");
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.stdout, "payload-through-stdin");
    }
}
