use std::sync::Arc;

use serde_json::{json, Value};

use crate::chat::types::ToolCallStatus;
use crate::cli::session_run::{run_session_goal, SessionGoalArgs};
use crate::cli::skills::LoadedCliSkill;
use crate::core::home::DripProject;
use crate::core::inference::ResolvedInferenceConfig;
use crate::core::sessions::{create_session, open_session_index, CreateSessionArgs, ProjectPaths, SessionEnvScope};
use crate::harness::model_call::AbortSignal;
use crate::tools::types::{
    define_sync_tool, ChatToolCompleteRequest, ChatToolCompletionResult, ChatToolDefinition,
    ChatToolExecuteRequest, ChatToolMode, ChatToolPrepareRequest, ChatToolPreparedInput, ChatToolResult,
    ChatToolRuntimeServices,
};

// Sub-delegation (backlog G3): drip could not safely spawn drip — a BASH-spawned
// child loses the harness credentials to the env scrub and escapes --stop
// (it does record its parent, via DRIP_SESSION_ID). DELEGATE runs the child
// goal IN-PROCESS through the same
// run_session_goal choreography instead: it inherits the resolved inference
// config directly, chains the parent's abort signal, and lands its own
// session (transcript, result.json, lease) beside the parent's.

const MAX_CHILD_ITERATIONS: i64 = 20;
const DEFAULT_CHILD_ITERATIONS: i64 = 10;
/// Wall-clock budget for a child run. Two recorded parents that looked like
/// 75-79 calls each hid a child that ran to max-iterations for 2.5-3.3
/// hours behind one DELEGATE call; an iteration cap bounds rounds, not
/// time, and a child stuck in slow builds or slow calls eats the parent's
/// whole afternoon. The child is aborted at the deadline and reports so.
pub const DEFAULT_CHILD_WALL_SECONDS: i64 = 1200;
pub const MAX_CHILD_WALL_SECONDS: i64 = 3600;

/// Aborts `child` when `parent` aborts or `wall_seconds` elapse, until `done`
/// is set, and terminates the child's in-flight processes at that moment. Returns the watcher thread and the flag that says the deadline
/// (not the parent) fired.
fn watch_child_budget(
    child: AbortSignal,
    parent: Option<AbortSignal>,
    wall_seconds: i64,
    done: Arc<std::sync::atomic::AtomicBool>,
    terminate_in_flight: Arc<dyn Fn() + Send + Sync>,
) -> (std::thread::JoinHandle<()>, Arc<std::sync::atomic::AtomicBool>) {
    use std::sync::atomic::Ordering;
    let deadline_hit = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let hit = deadline_hit.clone();
    let handle = std::thread::spawn(move || {
        let started = std::time::Instant::now();
        let budget = std::time::Duration::from_secs(wall_seconds.max(1) as u64);
        while !done.load(Ordering::SeqCst) {
            if parent.as_ref().is_some_and(|signal| signal.is_aborted()) {
                child.abort();
                terminate_in_flight();
                break;
            }
            if started.elapsed() >= budget {
                hit.store(true, Ordering::SeqCst);
                child.abort();
                // The abort is only observed between steps; a tool call in
                // flight (a full cargo run, in a recorded dogfood) would
                // otherwise finish first. The parent is blocked on this
                // DELEGATE, so every active process belongs to the child.
                // (Injected: the unit tests must not kill the test binary's
                // other children — that took out three unrelated tests.)
                terminate_in_flight();
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    });
    (handle, deadline_hit)
}

/// Wiring for the DELEGATE tool. Tool definitions are not
/// clonable (boxed stage closures), so the child pack arrives as a factory
/// that builds the parent's tools afresh; the session index is reopened per
/// child (rusqlite connections are not shareable across the boxed closure).
pub struct DelegateToolWiring {
    pub cwd: String,
    pub index_db_path: String,
    pub inference: ResolvedInferenceConfig,
    pub parent_session_id: String,
    pub project: DripProject,
    pub redact_secrets: Vec<(String, String)>,
    pub signal: Option<AbortSignal>,
    pub skills: Vec<LoadedCliSkill>,
    pub tool_services: Option<ChatToolRuntimeServices>,
    /// Parent tool pack; the child gets it minus DELEGATE (depth is capped at one).
    pub tools: Arc<dyn Fn() -> Vec<ChatToolDefinition>>,
}

/// Builds the DELEGATE tool from the wiring.
pub fn build_delegate_tool(wiring: DelegateToolWiring) -> ChatToolDefinition {
    let wiring = Arc::new(wiring);

    define_sync_tool(ChatToolDefinition {
        name: "DELEGATE".to_string(),
        description: "Delegate a self-contained subtask to a fresh drip child session and wait for its result. The child works in the same workspace with the same tools (minus DELEGATE — depth is one). Returns the child's result contract: reason, task stats, verification, summary, and the child session id for --inspect. Use for subtasks big enough to deserve their own task ledger; do the work directly otherwise.".to_string(),
        parameters: serde_json::from_value(json!({
            "additionalProperties": false,
            "properties": {
                "goal": {
                    "description": "The complete, self-contained subtask goal — include the definition of done and the exact verification command.",
                    "type": "string"
                },
                "maxIterations": {
                    "description": format!("Cycle budget for the child (default {DEFAULT_CHILD_ITERATIONS}, max {MAX_CHILD_ITERATIONS})."),
                    "type": "number"
                },
                "wallSeconds": {
                    "description": format!("Wall-clock budget for the child in seconds (default {DEFAULT_CHILD_WALL_SECONDS}, max {MAX_CHILD_WALL_SECONDS}); the child is stopped at the deadline and reports what it finished."),
                    "type": "number"
                }
            },
            "required": ["goal"],
            "type": "object"
        }))
        .unwrap_or_default(),
        mutates_workspace: false,
        mode: ChatToolMode::Sync,
        prepare: Box::new(|request: ChatToolPrepareRequest<'_>| {
            let parsed: Value = serde_json::from_str(request.raw_input).map_err(|error| error.to_string())?;
            let goal = parsed
                .get("goal")
                .and_then(Value::as_str)
                .map(|goal| goal.trim().to_string())
                .unwrap_or_default();

            if goal.is_empty() {
                return Err("DELEGATE needs a \"goal\" string — the complete subtask description.".to_string());
            }

            let requested = parsed
                .get("maxIterations")
                .and_then(Value::as_f64)
                .filter(|value| value.is_finite())
                .map(|value| value.floor() as i64)
                .unwrap_or(DEFAULT_CHILD_ITERATIONS);

            let wall_seconds = parsed
                .get("wallSeconds")
                .and_then(Value::as_f64)
                .filter(|value| value.is_finite())
                .map(|value| value.floor() as i64)
                .unwrap_or(DEFAULT_CHILD_WALL_SECONDS);

            Ok(ChatToolPreparedInput {
                display_input: format!("delegate: {}", goal.chars().take(80).collect::<String>()),
                input: json!({
                    "goal": goal,
                    "maxIterations": requested.clamp(1, MAX_CHILD_ITERATIONS),
                    "wallSeconds": wall_seconds.clamp(30, MAX_CHILD_WALL_SECONDS)
                }),
                tags: None,
            })
        }),
        execute: Box::new(move |request: ChatToolExecuteRequest<'_>| {
            let goal = request.prepared.input["goal"].as_str().unwrap_or_default().to_string();
            let max_iterations = request.prepared.input["maxIterations"].as_i64().unwrap_or(DEFAULT_CHILD_ITERATIONS);
            let wall_seconds = request.prepared.input["wallSeconds"].as_i64().unwrap_or(DEFAULT_CHILD_WALL_SECONDS);
            let child_signal = AbortSignal::new();
            let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (watcher, deadline_hit) = watch_child_budget(
                child_signal.clone(),
                wiring.signal.clone(),
                wall_seconds,
                done.clone(),
                Arc::new(|| {
                    crate::tools::child_process::terminate_active_processes();
                }),
            );
            let wiring = wiring.clone();
            let index = open_session_index(&wiring.index_db_path);
            let project_paths = ProjectPaths::from(&wiring.project);
            let child_session = create_session(
                &index,
                CreateSessionArgs {
                    cwd: wiring.cwd.clone(),
                    // The child's parent is the session whose DELEGATE call this
                    // is, known in-process — no environment lookup involved.
                    parent_id: Some(wiring.parent_session_id.clone()),
                    project: &project_paths,
                    now: "",
                },
            );
            let child_tools: Vec<ChatToolDefinition> = (wiring.tools)()
                .into_iter()
                .filter(|tool| tool.name != "DELEGATE")
                .collect();

            // Repoint DRIP_SESSION_ID at the child for the child's lifetime so a
            // skill the child shells out to nests under the child. The parent
            // is blocked inside this tool call the whole time (a response's
            // tool calls run one after another, and a background job captured
            // its environment when it was spawned), so nothing else reads the
            // variable meanwhile; the guard restores the parent's value on every
            // exit path, including an unwinding child, so the parent's later
            // tool subprocesses attach to the parent again.
            let _session_env = SessionEnvScope::enter(&child_session.id);

            let outcome = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(run_session_goal(SessionGoalArgs {
                    // A delegated child blocking on operator answers would stall
                    // the parent's tool call — children never get ask_user.
                    ask_user_enabled: false,
                    ask_user_timeout_seconds: None,
                    cwd: wiring.cwd.clone(),
                    goal: goal.clone(),
                    goal_context: Some(format!(
                        "delegated_from: session {} — this goal is a subtask of that session's work; finish it and report, do not expand scope.",
                        wiring.parent_session_id
                    )),
                    goal_images: None,
                    hooks: Default::default(),
                    index: &index,
                    inference: wiring.inference.clone(),
                    max_iterations: Some(max_iterations),
                    max_loops: None,
                    task_loop_limit: None,
                    review_waiver_lines: None,
                    plan_mode: None,
                    mentions: None,
                    new_goal: false,
                    no_repo_memory: false,
                    on_event: Arc::new(|_| {}),
                    plan_only: false,
                    redact_secrets: wiring.redact_secrets.clone(),
                    request_timeout_ms: None,
                    seed_tasks: None,
                    project: &wiring.project,
                    role_bindings: None,
                    roles: None,
                    session: &child_session,
                    signal: Some(child_signal.clone()),
                    skills: wiring.skills.clone(),
                    summarize_run: None,
                    lite: false,
                    no_review: false,
                    tools: child_tools,
                    tool_services: wiring.tool_services.clone(),
                    // A delegate child gets no MCP surface of its own.
                    mcp_servers: None,
                }))
            });
            drop(_session_env);
            done.store(true, std::sync::atomic::Ordering::SeqCst);
            let _ = watcher.join();
            let deadline_hit = deadline_hit.load(std::sync::atomic::Ordering::SeqCst);
            let outcome = outcome.map_err(|error| error.to_string())?;
            let record = &outcome.record;
            let verification = record.last_verification.as_ref();
            let short_id: String = child_session.id.chars().take(8).collect();
            let lines = vec![
                if deadline_hit {
                    format!(
                        "DELEGATE wall budget of {wall_seconds}s exhausted ({}): {} completed, {} pending, {} blocked (child session {short_id}) — resume it with a narrower goal or a larger wallSeconds, or do the rest directly",
                        record.reason, record.task_stats.completed, record.task_stats.pending, record.task_stats.blocked
                    )
                } else {
                    format!(
                        "DELEGATE {}: {} completed, {} pending, {} blocked (child session {short_id})",
                        record.reason, record.task_stats.completed, record.task_stats.pending, record.task_stats.blocked
                    )
                },
                match verification {
                    Some(verification) => format!(
                        "verification: {} → {}{}",
                        verification.command,
                        crate::core::state::describe_verification_outcome(verification.failed, verification.ran_no_tests, verification.evidence.as_ref()),
                        if verification.mutations_after > 0 { " (STALE)" } else { "" }
                    ),
                    None => "verification: none recorded".to_string(),
                },
                record.summary.clone().unwrap_or_else(|| "(no summary)".to_string()),
                format!(
                    "inspect: drip --inspect {short_id} · resume: drip --resume {short_id} --prompt \"<follow-up>\""
                ),
            ];

            Ok(ChatToolResult {
                data: Some(Value::String(lines.join("\n"))),
                output_text: Some(lines[0].clone()),
                // A child that did not complete is a failed delegation — the parent
                // must see that honestly and decide (resume the child, or replan).
                status: Some(if crate::cli::headless_output::reason_is_complete(&record.reason) {
                    ToolCallStatus::Completed
                } else {
                    ToolCallStatus::Failed
                }),
                ..ChatToolResult::default()
            })
        }),
        complete: Box::new(|request: ChatToolCompleteRequest<'_>| {
            Ok(ChatToolCompletionResult {
                blocks: Some(vec![]),
                tool_content: request.result.data.as_ref().and_then(Value::as_str).map(str::to_string),
                tags: None,
            })
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prepare(raw: &str) -> Result<ChatToolPreparedInput, String> {
        let home = tempfile::tempdir().unwrap();
        let wiring = DelegateToolWiring {
            cwd: ".".to_string(),
            index_db_path: String::new(),
            inference: crate::core::inference::ResolvedInferenceConfig {
                route: crate::core::inference::ResolvedModelRoute {
                    fallback_route: None,
                    headers: vec![],
                    model: "m".to_string(),
                    profile_id: "p".to_string(),
                    provider: "openai".to_string(),
                    reasoning_effort: None,
                    refresh_headers: None,
                    url: "http://127.0.0.1:1/v1/chat/completions".to_string(),
                },
                system_prompt: String::new(),
                tool_route: None,
                tool_route_warning: None,
            },
            parent_session_id: "parent".to_string(),
            project: crate::core::home::resolve_drip_project(".", &home.path().to_string_lossy(), None).unwrap(),
            redact_secrets: vec![],
            signal: None,
            skills: vec![],
            tool_services: None,
            tools: Arc::new(Vec::new),
        };
        let tool = build_delegate_tool(wiring);
        let message = crate::chat::types::ChatMessage {
            blocks: vec![],
            context_files: None,
            context_state: None,
            created_at: None,
            failed: None,
            id: "m".to_string(),
            pending: None,
            reply_to_message_id: None,
            role: crate::chat::types::ChatRole::User,
            tags: None,
            transport_state: None,
        };
        let services = crate::tools::async_jobs::create_chat_tool_runtime_services(
            crate::tools::async_jobs::CreateChatToolRuntimeServicesOptions { cwd: None, jobs_root: None },
        );
        (tool.prepare)(ChatToolPrepareRequest {
            call_id: "c",
            history: &[],
            message: &message,
            raw_input: raw,
            runtime_context: crate::chat::types::ChatRuntimeContext {
                cwd: ".".to_string(),
                working_file: crate::chat::types::WorkingFileContext {
                    exists: false,
                    path: String::new(),
                    scope: crate::chat::types::WorkingFileScope::Cwd,
                    text: None,
                },
            },
            services,
        })
    }

    #[test]
    fn the_budget_watcher_aborts_the_child_at_the_deadline_or_on_parent_abort() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        let kills = Arc::new(AtomicUsize::new(0));
        let terminate = {
            let kills = kills.clone();
            Arc::new(move || {
                kills.fetch_add(1, Ordering::SeqCst);
            }) as Arc<dyn Fn() + Send + Sync>
        };
        // Deadline: a 1s budget fires, marks the deadline, and kills in-flight work.
        let child = AbortSignal::new();
        let done = Arc::new(AtomicBool::new(false));
        let (watcher, hit) = watch_child_budget(child.clone(), None, 1, done.clone(), terminate.clone());
        watcher.join().unwrap();
        assert!(child.is_aborted() && hit.load(Ordering::SeqCst));
        assert_eq!(kills.load(Ordering::SeqCst), 1);
        // Parent abort: forwarded with a kill, deadline not marked.
        let child = AbortSignal::new();
        let parent = AbortSignal::new();
        let done = Arc::new(AtomicBool::new(false));
        let (watcher, hit) = watch_child_budget(child.clone(), Some(parent.clone()), 600, done.clone(), terminate.clone());
        parent.abort();
        watcher.join().unwrap();
        assert!(child.is_aborted() && !hit.load(Ordering::SeqCst));
        assert_eq!(kills.load(Ordering::SeqCst), 2);
        // Done: the watcher exits without aborting or killing.
        let child = AbortSignal::new();
        let done = Arc::new(AtomicBool::new(true));
        let (watcher, hit) = watch_child_budget(child.clone(), None, 600, done, terminate);
        watcher.join().unwrap();
        assert!(!child.is_aborted() && !hit.load(Ordering::SeqCst));
        assert_eq!(kills.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn prepare_clamps_iterations_and_requires_goal() {
        let prepared = prepare(r#"{"goal":"  do it  ","maxIterations":99.7}"#).unwrap();
        assert_eq!(prepared.input["goal"], "do it");
        assert_eq!(prepared.input["maxIterations"], 20);
        assert_eq!(prepared.display_input, "delegate: do it");

        let prepared = prepare(r#"{"goal":"x"}"#).unwrap();
        assert_eq!(prepared.input["maxIterations"], 10);
        let prepared = prepare(r#"{"goal":"x","maxIterations":0}"#).unwrap();
        assert_eq!(prepared.input["maxIterations"], 1);
        assert_eq!(prepared.input["wallSeconds"], DEFAULT_CHILD_WALL_SECONDS);
        let prepared = prepare(r#"{"goal":"x","wallSeconds":99999}"#).unwrap();
        assert_eq!(prepared.input["wallSeconds"], MAX_CHILD_WALL_SECONDS);
        let prepared = prepare(r#"{"goal":"x","wallSeconds":1}"#).unwrap();
        assert_eq!(prepared.input["wallSeconds"], 30);

        let error = prepare(r#"{"goal":"   "}"#).unwrap_err();
        assert_eq!(error, "DELEGATE needs a \"goal\" string — the complete subtask description.");
    }
}
