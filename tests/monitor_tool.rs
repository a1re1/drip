// End-to-end MONITOR: one call starts an async job that retries its check
// until the signal appears (or the timeout expires), and the settled result
// is readable through the async-job manager the harness drains every round.
use std::time::Duration;

use drip::chat::types::{
    ChatMessage, ChatRole, ChatRuntimeContext, WorkingFileContext, WorkingFileScope,
};
use drip::tools::async_jobs::{
    create_chat_tool_runtime_services, CreateChatToolRuntimeServicesOptions,
};
use drip::tools::execute::{execute_tool_call, ToolExecutionContext};
use drip::tools::pack::{builtin_tool_pack, BuiltinToolOptions};
use drip::tools::types::{ChatAsyncToolJobStatus, ChatToolRuntimeServices};

fn message() -> ChatMessage {
    ChatMessage {
        blocks: vec![],
        context_files: None,
        context_state: None,
        created_at: None,
        failed: None,
        id: "m1".to_string(),
        pending: None,
        reply_to_message_id: None,
        role: ChatRole::User,
        tags: None,
        transport_state: None,
    }
}

/// Runs MONITOR in `cwd` and returns (tool content, failed, services).
fn run_monitor(cwd: &std::path::Path, raw_input: &str) -> (String, bool, ChatToolRuntimeServices) {
    let services = create_chat_tool_runtime_services(CreateChatToolRuntimeServicesOptions {
        cwd: Some(cwd.to_path_buf()),
        jobs_root: Some(cwd.join("jobs")),
    });
    let tools = builtin_tool_pack(BuiltinToolOptions::default());
    let message = message();
    let executed = execute_tool_call(ToolExecutionContext {
        call_id: "c1",
        history: &[],
        message: &message,
        raw_input,
        runtime_context: ChatRuntimeContext {
            cwd: cwd.to_string_lossy().to_string(),
            working_file: WorkingFileContext {
                exists: false,
                path: String::new(),
                scope: WorkingFileScope::Cwd,
                text: None,
            },
        },
        services: services.clone(),
        tool: tools.iter().find(|tool| tool.name == "MONITOR"),
        tool_name: "MONITOR",
    });

    let failed = executed.tool_content.contains("ERROR");
    (executed.tool_content, failed, services)
}

#[test]
fn monitor_reports_a_signal_that_appears_while_it_waits() {
    let dir = tempfile::tempdir().unwrap();
    let signal = dir.path().join("signal.txt");
    let writer_signal = signal.clone();
    // The signal appears a few intervals in, so the loop really retries.
    let writer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        std::fs::write(&writer_signal, "ready\n").unwrap();
    });

    let (content, failed, _services) = run_monitor(
        dir.path(),
        r#"{"check":"test -f signal.txt && echo FILE-IS-HERE","intervalMs":20,"timeoutMs":20000,"description":"the signal file"}"#,
    );
    writer.join().unwrap();

    assert!(!failed, "{content}");
    assert!(content.contains("Signal met"), "{content}");
    assert!(content.contains("FILE-IS-HERE"), "{content}");
    // More than one attempt ran: the first checks all failed.
    assert!(content.contains("attempt 2"), "{content}");
}

#[test]
fn monitor_reports_its_timeout_when_the_signal_never_appears() {
    let dir = tempfile::tempdir().unwrap();

    let (content, failed, _services) = run_monitor(
        dir.path(),
        r#"{"check":"test -f never.txt","intervalMs":20,"timeoutMs":400,"description":"a file that never comes"}"#,
    );

    assert!(!failed, "{content}");
    assert!(!content.contains("Signal met"), "{content}");
    assert!(content.contains("did not appear within 400ms"), "{content}");
    // The report names the signal, the attempt count and the last output.
    assert!(content.contains("a file that never comes"), "{content}");
    assert!(content.contains("timed out after"), "{content}");
    assert!(content.contains("attempt"), "{content}");
}

#[test]
fn a_long_monitor_hands_its_settled_result_to_the_harness_queue() {
    let dir = tempfile::tempdir().unwrap();

    let (content, failed, services) = run_monitor(
        dir.path(),
        r#"{"check":"test -f never.txt","intervalMs":50,"timeoutMs":6000,"description":"a slow signal"}"#,
    );

    // The call returns while the monitor is still checking.
    assert!(!failed, "{content}");
    assert!(content.contains("Monitor started"), "{content}");
    assert!(content.contains("ASYNC_WAIT"), "{content}");

    // ...and the harness-facing queue hands back the settled result later.
    let mut settled = None;
    for _ in 0..600 {
        if let Some(job) = services
            .async_jobs
            .take_settled_unreported()
            .into_iter()
            .next()
        {
            settled = Some(job);
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let job = settled.expect("the monitor settles at its own timeout");
    assert_eq!(job.status, ChatAsyncToolJobStatus::Failed, "{content}");
    assert_eq!(job.tool_name, "MONITOR");
    assert!(job.title.contains("a slow signal"), "{}", job.title);
    let error = job.error.clone().unwrap_or_default();
    assert!(error.contains("timed out after"), "{error}");

    let log = std::fs::read_to_string(&job.log_path).unwrap();
    assert!(log.contains("timed out after"), "{log}");
    assert!(log.contains("attempt"), "{log}");
}

#[test]
fn a_hung_check_cannot_outlive_the_monitor_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let started = std::time::Instant::now();

    // The check itself never returns: bash is killed at the per-check budget.
    let (content, failed, services) = run_monitor(
        dir.path(),
        r#"{"check":"sleep 30","intervalMs":20,"timeoutMs":300,"description":"a hung check"}"#,
    );
    assert!(!failed, "{content}");

    let reported_inline = content.contains("did not appear within 300ms");
    let mut settled = None;
    if !reported_inline {
        // The call returned while the job was still settling; the settled
        // result lands in the harness-facing queue.
        for _ in 0..250 {
            if let Some(job) = services
                .async_jobs
                .take_settled_unreported()
                .into_iter()
                .next()
            {
                settled = Some(job);
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    let elapsed = started.elapsed();
    let late_error = settled
        .map(|job| job.error.clone().unwrap_or_default())
        .unwrap_or_default();

    assert!(
        reported_inline || late_error.contains("did not appear"),
        "a hung check must settle as a timeout: inline={reported_inline} late={late_error} content={content}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "a hung check must not outlive the monitor budget (timeoutMs 300): {elapsed:?}"
    );
}

#[test]
fn a_running_monitor_is_visible_as_a_running_job() {
    let dir = tempfile::tempdir().unwrap();

    let (content, failed, services) = run_monitor(
        dir.path(),
        r#"{"check":"test -f never.txt","intervalMs":50,"timeoutMs":8000,"description":"a visible signal"}"#,
    );

    assert!(!failed, "{content}");
    assert!(content.contains("Monitor started"), "{content}");

    // The loop-end leaked-jobs report reads this, so a monitor still
    // checking at loop end is surfaced instead of vanishing.
    let running = services.async_jobs.running_jobs();
    let monitor = running
        .iter()
        .find(|job| job.tool_name == "MONITOR")
        .unwrap_or_else(|| panic!("a running monitor must be visible: {running:?}"));
    assert_eq!(monitor.status, ChatAsyncToolJobStatus::Running);
    assert!(
        monitor.title.contains("a visible signal"),
        "{}",
        monitor.title
    );

    // Let it settle so the test leaves no live job behind.
    let finished = services
        .async_jobs
        .wait_for_job(&monitor.id, Some(15_000))
        .unwrap();
    assert!(finished.completed, "{content}");
    assert!(
        services.async_jobs.running_jobs().is_empty(),
        "a settled job is not running"
    );
}
