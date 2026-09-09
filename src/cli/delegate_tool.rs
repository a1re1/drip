use std::sync::Arc;

use serde_json::{json, Value};

use crate::chat::types::ToolCallStatus;
use crate::cli::session_run::{run_session_goal, SessionGoalArgs};
use crate::cli::skills::LoadedCliSkill;
use crate::core::home::DripProject;
use crate::core::inference::ResolvedInferenceConfig;
use crate::core::sessions::{create_session, open_session_index, CreateSessionArgs, ProjectPaths};
use crate::harness::model_call::AbortSignal;
use crate::tools::types::{
    define_sync_tool, ChatToolCompleteRequest, ChatToolCompletionResult, ChatToolDefinition,
    ChatToolExecuteRequest, ChatToolMode, ChatToolPrepareRequest, ChatToolPreparedInput, ChatToolResult,
    ChatToolRuntimeServices,
};

// Sub-delegation (backlog G3): drip could not safely spawn drip — a BASH-spawned
// child loses the harness credentials to the env scrub, holds no parent link,
// and escapes --stop. DELEGATE runs the child goal IN-PROCESS through the same
// run_session_goal choreography instead: it inherits the resolved inference
// config directly, chains the parent's abort signal, and lands its own
// session (transcript, result.json, lease) beside the parent's.

const MAX_CHILD_ITERATIONS: i64 = 20;
const DEFAULT_CHILD_ITERATIONS: i64 = 10;

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

            Ok(ChatToolPreparedInput {
                display_input: format!("delegate: {}", goal.chars().take(80).collect::<String>()),
                input: json!({
                    "goal": goal,
                    "maxIterations": requested.clamp(1, MAX_CHILD_ITERATIONS)
                }),
                tags: None,
            })
        }),
        execute: Box::new(move |request: ChatToolExecuteRequest<'_>| {
            let goal = request.prepared.input["goal"].as_str().unwrap_or_default().to_string();
            let max_iterations = request.prepared.input["maxIterations"].as_i64().unwrap_or(DEFAULT_CHILD_ITERATIONS);
            let wiring = wiring.clone();
            let index = open_session_index(&wiring.index_db_path);
            let project_paths = ProjectPaths::from(&wiring.project);
            let child_session = create_session(
                &index,
                CreateSessionArgs {
                    cwd: wiring.cwd.clone(),
                    project: &project_paths,
                    now: "",
                },
            );
            let child_tools: Vec<ChatToolDefinition> = (wiring.tools)()
                .into_iter()
                .filter(|tool| tool.name != "DELEGATE")
                .collect();

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
                    signal: wiring.signal.clone(),
                    skills: wiring.skills.clone(),
                    summarize_run: None,
                    lite: false,
                    no_review: false,
                    tools: child_tools,
                    tool_services: wiring.tool_services.clone(),
                }))
            })
            .map_err(|error| error.to_string())?;
            let record = &outcome.record;
            let verification = record.last_verification.as_ref();
            let short_id: String = child_session.id.chars().take(8).collect();
            let lines = vec![
                format!(
                    "DELEGATE {}: {} completed, {} pending, {} blocked (child session {short_id})",
                    record.reason, record.task_stats.completed, record.task_stats.pending, record.task_stats.blocked
                ),
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
    fn prepare_clamps_iterations_and_requires_goal() {
        let prepared = prepare(r#"{"goal":"  do it  ","maxIterations":99.7}"#).unwrap();
        assert_eq!(prepared.input["goal"], "do it");
        assert_eq!(prepared.input["maxIterations"], 20);
        assert_eq!(prepared.display_input, "delegate: do it");

        let prepared = prepare(r#"{"goal":"x"}"#).unwrap();
        assert_eq!(prepared.input["maxIterations"], 10);
        let prepared = prepare(r#"{"goal":"x","maxIterations":0}"#).unwrap();
        assert_eq!(prepared.input["maxIterations"], 1);

        let error = prepare(r#"{"goal":"   "}"#).unwrap_err();
        assert_eq!(error, "DELEGATE needs a \"goal\" string — the complete subtask description.");
    }
}
