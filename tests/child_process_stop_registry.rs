//! Independent review of the `REGISTER_SPAWNED_CHILDREN` fix in
//! `src/tools/child_process.rs`.
//!
//! The fix stops `run_captured_process` from registering its child in the
//! process-wide stop registry **in the test build**, because `cargo test` runs
//! every unit test of `drip` inside one process and a test that fires
//! `terminate_active_processes` for its own sake therefore group-killed the
//! children of unrelated tests (the status-line runner jobs came back
//! SIGTERM-killed and `assert!(output.ok)` flaked).
//!
//! That exemption is only safe if production is untouched. This file is an
//! integration test: it links the real `drip` library, where `cfg(test)` is
//! FALSE, so it exercises exactly the code path a shipped drip runs. It fails
//! if the exemption ever leaks out of the test build — the module's documented
//! contract is that "a stop signal to drip interrupts every in-flight child".
use std::time::{Duration, Instant};

use drip::tools::child_process::{
    run_captured_process, terminate_active_processes, CapturedProcessArgs,
};

#[test]
fn a_stop_sweep_still_reaches_a_child_spawned_by_run_captured_process() {
    let process_args = vec!["-c".to_string(), "sleep 30".to_string()];

    let runner = std::thread::spawn(move || {
        let args = CapturedProcessArgs {
            command: "/bin/sh",
            cwd: None,
            env: None,
            process_args: &process_args,
            timeout_ms: Some(20_000),
            stdin_payload: None,
        };
        let started = Instant::now();
        let result = run_captured_process(&args).expect("spawn failed");
        (result, started.elapsed())
    });

    // Let the child spawn and register its terminator before sweeping.
    std::thread::sleep(Duration::from_millis(300));

    let swept = terminate_active_processes();
    assert!(
        swept >= 1,
        "a production build must register the spawned child in the stop registry"
    );

    let (result, elapsed) = runner.join().expect("runner thread must not panic");
    assert!(
        !result.timed_out,
        "the stop sweep, not the 20s timeout, must have ended this run"
    );
    assert_eq!(result.exit_code, None, "the child must die of the sweep");
    assert_eq!(result.signal.as_deref(), Some("SIGTERM"));
    assert!(
        elapsed < Duration::from_secs(5),
        "a swept child must settle promptly, took {elapsed:?}"
    );
}
