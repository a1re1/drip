// port of tools/index.ts (the built-in pack), tools/bash-tool.ts:473-624
// (asyncBashTool) and src/tools/framework-tools.ts (ASYNC_TAIL / ASYNC_WAIT).
//
// lci ships its built-in tools as TypeScript modules under <repo>/tools/,
// each a defineSyncTool/defineAsyncTool object with prepare/execute/complete
// stages. drip's builtin/ modules expose the same behavior as plain functions
// (definition() + execute(args, ctx)); this module wraps them back into
// `ChatToolDefinition`s so the harness loop (execute_tool_call in
// tools/execute.rs) drives them through the same three-stage pipeline.
//
// Sync wrapping runs the whole builtin pipeline inside the execute stage:
// the prepare stage only records the raw input. That is invisible to the
// harness — it consumes `toolContent` and the tool-call block status only —
// and both are identical: a builtin failure text is "ERROR: <message>"
// (buildFailureResult), so the adapter strips the prefix and returns `Err`,
// letting execute_tool_call rebuild exactly the same failure result.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{json, Value};

use crate::chat::types::{ChatMessageBlock, CompletionBlock, TextBlock, ToolCallStatus};
use crate::tools::builtin::{self, ToolCtx, ToolOutcome};
use crate::tools::async_jobs::{compact_whitespace, truncate_text};
use crate::tools::helpers::{
    format_tool_path, get_optional_number_argument, get_required_string_argument, parse_tool_arguments,
    resolve_tool_path,
};
use crate::tools::types::{
    define_async_tool, define_sync_tool, ChatAsyncToolJobStatus, ChatAsyncToolTaskRequest,
    ChatToolCompleteRequest, ChatToolCompletionResult, ChatToolDefinition, ChatToolExecuteRequest,
    ChatToolMode, ChatToolParameters, ChatToolPrepareRequest, ChatToolPreparedInput, ChatToolResult,
    ChatToolRuntimeServices, ChatTmuxSession,
};

/// The names of the built-in pack in tools/index.ts order.
pub const BUILTIN_TOOL_NAMES: [&str; 9] = [
    "READ",
    "PATCH",
    "DIR",
    "BASH",
    "BASH_ASYNC",
    "GREP",
    "VERIFY",
    "FETCH",
    "CHECK",
];

/// main.tsx:327 — `const PLAN_MODE_TOOLS = new Set(["READ", "GREP", "DIR"])`.
pub const PLAN_MODE_TOOLS: [&str; 3] = ["READ", "GREP", "DIR"];

/// Splits a builtin `definition()` envelope
/// ({type: "function", function: {name, description, parameters}}) into the
/// ChatToolDefinition fields the transport rebuilds it from.
fn split_definition(definition: &Value) -> (String, String, ChatToolParameters) {
    let function = &definition["function"];
    let name = function["name"].as_str().unwrap_or_default().to_string();
    let description = function["description"].as_str().unwrap_or_default().to_string();
    let parameters: ChatToolParameters =
        serde_json::from_value(function["parameters"].clone()).unwrap_or_default();

    (name, description, parameters)
}

fn tool_ctx(cwd: &str, allow_net: bool) -> ToolCtx {
    ToolCtx {
        cwd: PathBuf::from(cwd),
        allow_net,
    }
}

/// Converts a builtin ToolOutcome into the execute-stage result. A thrown
/// error (the "ERROR: " text buildFailureResult produces) becomes
/// `Err(message)` so execute_tool_call rebuilds the identical failure
/// result; a stage that *finished* with status "failed" (BASH non-zero exit,
/// VERIFY failing tests, CHECK errors) keeps its report text as the tool
/// message and only marks the block failed, exactly as the TS stages do.
fn outcome_to_result(outcome: ToolOutcome) -> Result<ChatToolResult, String> {
    if outcome.failed {
        if let Some(message) = outcome.text.strip_prefix("ERROR: ") {
            return Err(message.to_string());
        }

        return Ok(ChatToolResult {
            output_text: Some(outcome.text),
            status: Some(crate::chat::types::ToolCallStatus::Failed),
            ..ChatToolResult::default()
        });
    }

    Ok(ChatToolResult {
        output_text: Some(outcome.text),
        ..ChatToolResult::default()
    })
}

/// How a tool's call is shown in the transcript and the TUI — the TS tool's
/// `displayInput`, derived from the parsed arguments. Falls back to the raw
/// input when the arguments do not parse; execute reports that error.
type DisplayInput = fn(&str, &ToolCtx) -> Option<String>;

/// Wraps one builtin module as a sync ChatToolDefinition.
fn sync_tool(
    definition: Value,
    mutates_workspace: bool,
    allow_net: bool,
    display: DisplayInput,
    run: Arc<dyn Fn(&str, &ToolCtx) -> ToolOutcome>,
) -> ChatToolDefinition {
    let (name, description, parameters) = split_definition(&definition);

    define_sync_tool(ChatToolDefinition {
        name,
        description,
        parameters,
        mutates_workspace,
        mode: ChatToolMode::Sync,
        prepare: Box::new(move |request: ChatToolPrepareRequest<'_>| {
            let ctx = tool_ctx(&request.runtime_context.cwd, allow_net);

            Ok(ChatToolPreparedInput {
                display_input: display(request.raw_input, &ctx).unwrap_or_else(|| request.raw_input.to_string()),
                input: Value::String(request.raw_input.to_string()),
                tags: None,
            })
        }),
        execute: Box::new(move |request: ChatToolExecuteRequest<'_>| {
            let raw_input = request.prepared.input.as_str().unwrap_or_default().to_string();
            let ctx = tool_ctx(&request.runtime_context.cwd, allow_net);

            outcome_to_result(run(&raw_input, &ctx))
        }),
        complete: Box::new(|request: ChatToolCompleteRequest<'_>| {
            Ok(ChatToolCompletionResult {
                blocks: None,
                tool_content: request.result.output_text.clone(),
                tags: None,
            })
        }),
    })
}

fn value_runner(run: fn(&Value, &ToolCtx) -> ToolOutcome) -> Arc<dyn Fn(&str, &ToolCtx) -> ToolOutcome> {
    Arc::new(move |raw_input: &str, ctx: &ToolCtx| run(&Value::String(raw_input.to_string()), ctx))
}

macro_rules! value_display {
    ($module:ident) => {
        |raw_input: &str, ctx: &ToolCtx| builtin::$module::display_input(&Value::String(raw_input.to_string()), ctx)
    };
}

// ---------------------------------------------------------------------------
// BASH_ASYNC — tools/bash-tool.ts:473-624
// ---------------------------------------------------------------------------

fn async_bash_prepare(request: ChatToolPrepareRequest<'_>) -> Result<ChatToolPreparedInput, String> {
    let args = parse_tool_arguments(request.raw_input).map_err(|error| error.to_string())?;
    let command = get_required_string_argument(&args, "command").map_err(|error| error.to_string())?;
    let raw_cwd = builtin::bash::get_optional_string_argument(&args, "cwd").map_err(|error| error.to_string())?;
    let context_cwd = request.runtime_context.cwd.clone();
    let absolute_cwd = resolve_tool_path(&context_cwd, raw_cwd.as_deref().unwrap_or(&context_cwd));
    let display_cwd = format_tool_path(&context_cwd, &absolute_cwd);
    let requested_session_name =
        builtin::bash::get_optional_string_argument(&args, "sessionName").map_err(|error| error.to_string())?;
    let session_name = builtin::bash::create_session_name(&command, requested_session_name.as_deref());

    // Async commands run detached and unwatched — the same gate applies.
    match crate::tools::command_policy::evaluate_command_policy(&command, &context_cwd) {
        crate::tools::command_policy::CommandPolicyVerdict::Block { rule, why } => {
            if crate::tools::command_policy::allow_destructive_enabled() {
                eprintln!("[policy] allowing destructive command (rule {rule}, --allow-destructive): {command}");
            } else {
                return Err(crate::tools::command_policy::format_policy_refusal(&command, &rule, &why));
            }
        }
        crate::tools::command_policy::CommandPolicyVerdict::Allow => {}
    }

    let title = builtin::bash::get_optional_string_argument(&args, "title")
        .map_err(|error| error.to_string())?
        .unwrap_or_else(|| format!("tmux {session_name}: {}", truncate_text(&compact_whitespace(&command), 72)));

    Ok(ChatToolPreparedInput {
        display_input: format!("{display_cwd}\nsession {session_name}\n{command}"),
        input: json!({
            "absoluteCwd": absolute_cwd.to_string_lossy(),
            "command": command,
            "displayCwd": display_cwd,
            "sessionName": session_name,
            "title": title
        }),
        tags: None,
    })
}

fn async_bash_execute(request: ChatToolExecuteRequest<'_>) -> Result<ChatToolResult, String> {
    let input = &request.prepared.input;
    let absolute_cwd = input["absoluteCwd"].as_str().unwrap_or_default().to_string();
    let display_cwd = input["displayCwd"].as_str().unwrap_or_default().to_string();
    let command = input["command"].as_str().unwrap_or_default().to_string();
    let session_name = input["sessionName"].as_str().unwrap_or_default().to_string();
    let title = input["title"].as_str().unwrap_or_default().to_string();

    crate::tools::helpers::assert_directory_path(std::path::Path::new(&absolute_cwd), &display_cwd)
        .map_err(|error| error.to_string())?;
    builtin::bash::assert_tmux_available().map_err(|error| error.to_string())?;

    if builtin::bash::tmux_session_exists(&session_name) {
        return Err(format!(
            "tmux session \"{session_name}\" already exists. Choose a different sessionName."
        ));
    }

    let attach_command = format!("tmux attach -t {session_name}");
    let kill_command = format!("tmux kill-session -t {session_name}");
    let run_session_name = session_name.clone();
    let run_attach = attach_command.clone();
    let run_kill = kill_command.clone();
    let run_cwd = absolute_cwd.clone();
    let run_command = command.clone();
    let job = request
        .services
        .async_jobs
        .start_task(ChatAsyncToolTaskRequest {
            cwd: Some(absolute_cwd.clone()),
            run: Box::new(move |logger| {
                logger.line(&format!("[session] {run_session_name}"))?;
                logger.line(&format!("[attach] {run_attach}"))?;
                logger.line(&format!("[kill] {run_kill}"))?;
                logger.line(&format!("[cwd] {run_cwd}"))?;
                logger.line(&format!(
                    "[command] {}",
                    truncate_text(&compact_whitespace(&run_command), 240)
                ))?;

                let outcome = (|| -> anyhow::Result<()> {
                    builtin::bash::start_tmux_session(builtin::bash::StartTmuxSessionArgs {
                        absolute_cwd: &run_cwd,
                        command: &run_command,
                        log_path: logger.log_path(),
                        session_name: &run_session_name,
                    })?;

                    let exit_code = builtin::bash::wait_for_tmux_session_exit(&run_session_name)?;

                    let Some(exit_code) = exit_code else {
                        anyhow::bail!(
                            "tmux session \"{run_session_name}\" ended before an exit status was available."
                        );
                    };

                    logger.line(&format!("[tmux-exit] session={run_session_name} exitCode={exit_code}"))?;

                    if exit_code != 0 {
                        anyhow::bail!("tmux session \"{run_session_name}\" exited with code {exit_code}.");
                    }

                    Ok(())
                })();

                if let Err(error) = &outcome {
                    logger.line(&format!("[tmux-error] {error}"))?;
                }

                outcome
            }),
            title: Some(title.clone()),
            tool_name: "BASH_ASYNC".to_string(),
        })
        .map_err(|error| error.to_string())?;

    request.services.tmux_sessions.register_session(ChatTmuxSession {
        attach_command: attach_command.clone(),
        cwd: absolute_cwd.clone(),
        job_id: job.id.clone(),
        kill_command: kill_command.clone(),
        session_name: session_name.clone(),
        started_at: job.started_at.clone(),
        title,
        tool_name: "BASH_ASYNC".to_string(),
    });

    Ok(ChatToolResult {
        async_job: Some(job),
        data: Some(json!({
            "attachCommand": attach_command,
            "command": command,
            "cwd": absolute_cwd,
            "killCommand": kill_command,
            "sessionName": session_name
        })),
        error: None,
        output_text: Some(format!("Started background bash command in tmux session {session_name}.")),
        status: Some(ToolCallStatus::Running),
        tags: None,
    })
}

fn async_bash_complete(request: ChatToolCompleteRequest<'_>) -> Result<ChatToolCompletionResult, String> {
    let data = request.result.data.clone().unwrap_or(Value::Null);
    let session_name = data["sessionName"].as_str().unwrap_or_default();
    let attach_command = data["attachCommand"].as_str().unwrap_or_default();
    let kill_command = data["killCommand"].as_str().unwrap_or_default();

    Ok(ChatToolCompletionResult {
        blocks: Some(vec![ChatMessageBlock::Text(TextBlock {
            context_state: None,
            tags: None,
            text: [
                format!("Started tmux session {session_name}."),
                format!("Attach: {attach_command}"),
                format!("Kill: {kill_command}"),
            ]
            .join("\n"),
        })]),
        tool_content: Some(
            [
                format!("Started background bash command in tmux session {session_name}."),
                format!("Attach: {attach_command}"),
                format!("Kill: {kill_command}"),
            ]
            .join("\n"),
        ),
        tags: None,
    })
}

/// tools/bash-tool.ts:473 — `export const asyncBashTool = defineAsyncTool(...)`.
pub fn async_bash_tool() -> ChatToolDefinition {
    let (name, description, parameters) = split_definition(&builtin::bash::async_definition());

    define_async_tool(ChatToolDefinition {
        name,
        description,
        parameters,
        mutates_workspace: false,
        mode: ChatToolMode::Async,
        prepare: Box::new(async_bash_prepare),
        execute: Box::new(async_bash_execute),
        complete: Box::new(async_bash_complete),
    })
}

// ---------------------------------------------------------------------------
// src/tools/framework-tools.ts — ASYNC_TAIL / ASYNC_WAIT
// ---------------------------------------------------------------------------

fn clamp_lines(value: Option<f64>) -> i64 {
    match value {
        None => 80,
        Some(value) => (value.floor() as i64).clamp(1, 400),
    }
}

fn clamp_timeout_ms(value: Option<f64>) -> i64 {
    match value {
        None => 60_000,
        Some(value) => (value.floor() as i64).clamp(0, 3_600_000),
    }
}

fn fallback_log_preview(output: &str) -> String {
    if output.trim().is_empty() {
        "[log file is empty]".to_string()
    } else {
        output.to_string()
    }
}

fn resolve_async_job_reference(job_id_or_session_name: &str, services: &ChatToolRuntimeServices) -> String {
    if services.async_jobs.get_job(job_id_or_session_name).is_some() {
        return job_id_or_session_name.to_string();
    }

    services
        .tmux_sessions
        .get_session(job_id_or_session_name)
        .map(|session| session.job_id)
        .unwrap_or_else(|| job_id_or_session_name.to_string())
}

fn job_status_text(status: ChatAsyncToolJobStatus) -> &'static str {
    match status {
        ChatAsyncToolJobStatus::Running => "running",
        ChatAsyncToolJobStatus::Failed => "failed",
        ChatAsyncToolJobStatus::Completed => "completed",
    }
}

fn job_status_to_tool_call_status(status: ChatAsyncToolJobStatus) -> ToolCallStatus {
    match status {
        ChatAsyncToolJobStatus::Running => ToolCallStatus::Running,
        ChatAsyncToolJobStatus::Failed => ToolCallStatus::Failed,
        ChatAsyncToolJobStatus::Completed => ToolCallStatus::Completed,
    }
}

fn join_present(parts: Vec<String>) -> String {
    parts.into_iter().filter(|part| !part.is_empty()).collect::<Vec<_>>().join("\n\n")
}

/// framework-tools.ts:50-105 — asyncTailTool.
pub fn async_tail_tool() -> ChatToolDefinition {
    define_sync_tool(ChatToolDefinition {
        name: "ASYNC_TAIL".to_string(),
        description: "Read the latest lines from an async background tool job log.".to_string(),
        parameters: serde_json::from_value(json!({
            "additionalProperties": false,
            "properties": {
                "jobId": {
                    "description": "The async job id returned when the background tool started, or a tmux session name from BASH_ASYNC.",
                    "type": "string"
                },
                "lines": {
                    "description": "Optional number of log lines to read from the end of the file. Defaults to 80.",
                    "type": "number"
                }
            },
            "required": ["jobId"],
            "type": "object"
        }))
        .unwrap_or_default(),
        mutates_workspace: false,
        mode: ChatToolMode::Sync,
        prepare: Box::new(|request: ChatToolPrepareRequest<'_>| {
            let args = parse_tool_arguments(request.raw_input).map_err(|error| error.to_string())?;
            let job_id = get_required_string_argument(&args, "jobId").map_err(|error| error.to_string())?;
            let lines = clamp_lines(get_optional_number_argument(&args, "lines").map_err(|error| error.to_string())?);

            Ok(ChatToolPreparedInput {
                display_input: format!("{job_id}\nlines {lines}"),
                input: json!({ "jobId": job_id, "lines": lines }),
                tags: None,
            })
        }),
        execute: Box::new(|request: ChatToolExecuteRequest<'_>| {
            let job_id = request.prepared.input["jobId"].as_str().unwrap_or_default();
            let lines = request.prepared.input["lines"].as_i64();
            let resolved_job_id = resolve_async_job_reference(job_id, &request.services);
            let result = request
                .services
                .async_jobs
                .tail_job(&resolved_job_id, lines)
                .map_err(|error| error.to_string())?;
            let output_text = format!(
                "Read {} line(s) from async job {}. Current status: {}.",
                result.lines,
                result.job.id,
                job_status_text(result.job.status)
            );

            Ok(ChatToolResult {
                data: Some(serde_json::to_value(&result).map_err(|error| error.to_string())?),
                output_text: Some(output_text),
                ..ChatToolResult::default()
            })
        }),
        complete: Box::new(|request: ChatToolCompleteRequest<'_>| {
            let data = request.result.data.clone().unwrap_or(Value::Null);
            let output = data["output"].as_str().unwrap_or_default().to_string();
            let job_id = data["job"]["id"].as_str().unwrap_or_default();
            let status = data["job"]["status"].as_str().unwrap_or_default();
            let log_path = data["job"]["logPath"].as_str().unwrap_or_default().to_string();
            let lines = request.prepared.input["lines"].as_i64().unwrap_or(80);
            let input_job_id = request.prepared.input["jobId"].as_str().unwrap_or_default();

            Ok(ChatToolCompletionResult {
                blocks: Some(vec![ChatMessageBlock::Completion(CompletionBlock {
                    code: fallback_log_preview(&output),
                    context_state: None,
                    description: Some(format!("Latest {lines} line(s) from async job {input_job_id}.")),
                    language: Some("text".to_string()),
                    path: Some(log_path.clone()),
                    tags: None,
                })]),
                tool_content: Some(join_present(vec![
                    format!("Async job {job_id} is {status}."),
                    format!("Log file: {log_path}"),
                    output,
                ])),
                tags: None,
            })
        }),
    })
}

/// framework-tools.ts:107-180 — asyncWaitTool.
pub fn async_wait_tool() -> ChatToolDefinition {
    define_sync_tool(ChatToolDefinition {
        name: "ASYNC_WAIT".to_string(),
        description: "Wait for an async background tool job to finish, optionally timing out while leaving the job running.".to_string(),
        parameters: serde_json::from_value(json!({
            "additionalProperties": false,
            "properties": {
                "jobId": {
                    "description": "The async job id returned when the background tool started, or a tmux session name from BASH_ASYNC.",
                    "type": "string"
                },
                "tailLines": {
                    "description": "Optional number of trailing log lines to include with the wait result. Defaults to 80.",
                    "type": "number"
                },
                "timeoutMs": {
                    "description": "Optional maximum time to wait before returning while the job keeps running. Defaults to 60000.",
                    "type": "number"
                }
            },
            "required": ["jobId"],
            "type": "object"
        }))
        .unwrap_or_default(),
        mutates_workspace: false,
        mode: ChatToolMode::Sync,
        prepare: Box::new(|request: ChatToolPrepareRequest<'_>| {
            let args = parse_tool_arguments(request.raw_input).map_err(|error| error.to_string())?;
            let job_id = get_required_string_argument(&args, "jobId").map_err(|error| error.to_string())?;
            let tail_lines =
                clamp_lines(get_optional_number_argument(&args, "tailLines").map_err(|error| error.to_string())?);
            let timeout_ms =
                clamp_timeout_ms(get_optional_number_argument(&args, "timeoutMs").map_err(|error| error.to_string())?);

            Ok(ChatToolPreparedInput {
                display_input: format!("{job_id}\nwait {timeout_ms}ms\ntail {tail_lines}"),
                input: json!({ "jobId": job_id, "tailLines": tail_lines, "timeoutMs": timeout_ms }),
                tags: None,
            })
        }),
        execute: Box::new(|request: ChatToolExecuteRequest<'_>| {
            let job_id = request.prepared.input["jobId"].as_str().unwrap_or_default();
            let tail_lines = request.prepared.input["tailLines"].as_i64();
            let timeout_ms = request.prepared.input["timeoutMs"].as_i64().unwrap_or(60_000);
            let resolved_job_id = resolve_async_job_reference(job_id, &request.services);
            let wait_result = request
                .services
                .async_jobs
                .wait_for_job(&resolved_job_id, Some(timeout_ms))
                .map_err(|error| error.to_string())?;
            let tail = request
                .services
                .async_jobs
                .tail_job(&resolved_job_id, tail_lines)
                .map_err(|error| error.to_string())?;
            let output_text = if wait_result.completed {
                format!(
                    "Async job {} finished with status {}.",
                    wait_result.job.id,
                    job_status_text(wait_result.job.status)
                )
            } else {
                format!(
                    "Async job {} is still running after waiting {timeout_ms}ms.",
                    wait_result.job.id
                )
            };
            let status = if wait_result.completed {
                job_status_to_tool_call_status(wait_result.job.status)
            } else {
                ToolCallStatus::Running
            };
            let mut data = serde_json::to_value(&wait_result).map_err(|error| error.to_string())?;
            data["tail"] = serde_json::to_value(&tail).map_err(|error| error.to_string())?;

            Ok(ChatToolResult {
                data: Some(data),
                output_text: Some(output_text),
                status: Some(status),
                ..ChatToolResult::default()
            })
        }),
        complete: Box::new(|request: ChatToolCompleteRequest<'_>| {
            let data = request.result.data.clone().unwrap_or(Value::Null);
            let completed = data["completed"].as_bool().unwrap_or(false);
            let status = data["job"]["status"].as_str().unwrap_or_default();
            let log_path = data["job"]["logPath"].as_str().unwrap_or_default().to_string();
            let output = data["tail"]["output"].as_str().unwrap_or_default().to_string();
            let input_job_id = request.prepared.input["jobId"].as_str().unwrap_or_default();
            let tail_lines = request.prepared.input["tailLines"].as_i64().unwrap_or(80);
            let timeout_ms = request.prepared.input["timeoutMs"].as_i64().unwrap_or(60_000);
            let summary = if completed {
                format!("Async job {input_job_id} finished with status {status}.")
            } else {
                format!("Async job {input_job_id} is still running after waiting {timeout_ms}ms.")
            };

            Ok(ChatToolCompletionResult {
                blocks: Some(vec![
                    ChatMessageBlock::Text(TextBlock {
                        context_state: None,
                        tags: None,
                        text: summary.clone(),
                    }),
                    ChatMessageBlock::Completion(CompletionBlock {
                        code: fallback_log_preview(&output),
                        context_state: None,
                        description: Some(format!("Latest {tail_lines} line(s) from async job {input_job_id}.")),
                        language: Some("text".to_string()),
                        path: Some(log_path.clone()),
                        tags: None,
                    }),
                ]),
                tool_content: Some(join_present(vec![summary, format!("Log file: {log_path}"), output])),
                tags: None,
            })
        }),
    })
}

/// framework-tools.ts:182 — `getFrameworkToolDefinitions()`.
pub fn get_framework_tool_definitions() -> Vec<ChatToolDefinition> {
    vec![async_tail_tool(), async_wait_tool()]
}

// ---------------------------------------------------------------------------
// tools/index.ts — the built-in pack
// ---------------------------------------------------------------------------

/// tools/index.ts — `[readTool, patchTool, dirTool, bashTool, asyncBashTool,
/// grepTool, verifyTool, fetchTool, checkTool]`. `allow_net` is the
/// LCI_ALLOW_NET=1 gate FETCH honors (main.tsx:707 sets it from --allow-net).
pub fn builtin_tool_pack(allow_net: bool) -> Vec<ChatToolDefinition> {
    vec![
        sync_tool(builtin::read::definition(), false, allow_net, value_display!(read), value_runner(builtin::read::execute)),
        sync_tool(builtin::patch::definition(), true, allow_net, value_display!(patch), value_runner(builtin::patch::execute)),
        sync_tool(builtin::dir::definition(), false, allow_net, value_display!(dir), value_runner(builtin::dir::execute)),
        // Only PATCH declares mutatesWorkspace (patch-tool.ts:401); a BASH
        // call is not progress for the stall accounting.
        sync_tool(
            builtin::bash::definition(),
            false,
            allow_net,
            builtin::bash::display_input,
            Arc::new(|raw_input: &str, ctx: &ToolCtx| builtin::bash::execute(raw_input, ctx)),
        ),
        async_bash_tool(),
        sync_tool(builtin::grep::definition(), false, allow_net, value_display!(grep), value_runner(builtin::grep::execute)),
        sync_tool(builtin::verify::definition(), false, allow_net, value_display!(verify), value_runner(builtin::verify::execute)),
        sync_tool(builtin::fetch::definition(), false, allow_net, value_display!(fetch), value_runner(builtin::fetch::execute)),
        sync_tool(builtin::check::definition(), false, allow_net, value_display!(check), value_runner(builtin::check::execute)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::types::{ChatMessage, ChatRole, ChatRuntimeContext, WorkingFileContext, WorkingFileScope};
    use crate::tools::async_jobs::{create_chat_tool_runtime_services, CreateChatToolRuntimeServicesOptions};
    use crate::tools::execute::{execute_tool_call, ToolExecutionContext};

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

    fn run(tools: &[ChatToolDefinition], name: &str, raw_input: &str, cwd: &std::path::Path) -> (String, bool) {
        let services = create_chat_tool_runtime_services(CreateChatToolRuntimeServicesOptions {
            cwd: Some(cwd.to_path_buf()),
            jobs_root: None,
        });
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
            services,
            tool: tools.iter().find(|tool| tool.name == name),
            tool_name: name,
        });
        let failed = executed
            .blocks
            .iter()
            .any(|block| matches!(block, ChatMessageBlock::ToolCall(block) if block.status == ToolCallStatus::Failed));

        (executed.tool_content, failed)
    }

    /// The tool-call block's `input` for one call — the TS tool's displayInput.
    fn display_of(name: &str, raw_input: &str, cwd: &std::path::Path) -> String {
        let tools = builtin_tool_pack(false);
        let services = create_chat_tool_runtime_services(CreateChatToolRuntimeServicesOptions { cwd: Some(cwd.to_path_buf()), jobs_root: None });
        let message = message();
        let executed = execute_tool_call(ToolExecutionContext {
            call_id: "c1",
            history: &[],
            message: &message,
            raw_input,
            runtime_context: ChatRuntimeContext {
                cwd: cwd.to_string_lossy().to_string(),
                working_file: WorkingFileContext { exists: false, path: String::new(), scope: WorkingFileScope::Cwd, text: None },
            },
            services,
            tool: tools.iter().find(|tool| tool.name == name),
            tool_name: name,
        });

        executed
            .blocks
            .iter()
            .find_map(|block| match block {
                ChatMessageBlock::ToolCall(block) => block.input.clone(),
                _ => None,
            })
            .unwrap_or_default()
    }

    #[test]
    fn tool_call_blocks_carry_the_ts_display_input() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hello.txt"), "hi\n").unwrap();
        let cwd = dir.path();

        assert_eq!(display_of("READ", r#"{"path":"hello.txt"}"#, cwd), "hello.txt");
        assert_eq!(display_of("PATCH", r#"{"path":"hello.txt","content":"a\nb\n"}"#, cwd), "hello.txt (write 3 line(s))");
        assert_eq!(display_of("DIR", r#"{"path":".","maxDepth":1}"#, cwd), ".\ndepth 1");
        assert_eq!(display_of("GREP", r#"{"pattern":"hi","path":".","glob":"*.txt"}"#, cwd), "pattern=hi path=. glob=*.txt");
        assert_eq!(display_of("VERIFY", r#"{"command":"true"}"#, cwd), "command=true");
        assert_eq!(display_of("BASH", r#"{"command":"true"}"#, cwd), ".\ntrue");
        // FETCH's prepare refuses without --allow-net (as the TS prepare throws), so
        // the block falls back to the raw input there; with net the TS formula shows.
        assert_eq!(display_of("FETCH", r#"{"url":"https://example.com/x"}"#, cwd), r#"{"url":"https://example.com/x"}"#);
        assert_eq!(
            builtin::fetch::display_input(&Value::String(r#"{"url":"https://example.com/x","maxBytes":100}"#.into()), &tool_ctx(&cwd.to_string_lossy(), true)),
            Some("GET https://example.com/x (max 100 bytes)".to_string())
        );
        // A failed execution (no tsconfig here) shows the raw input, as buildFailureResult does in TS.
        assert_eq!(display_of("CHECK", r#"{"path":"hello.txt"}"#, cwd), r#"{"path":"hello.txt"}"#);
        assert_eq!(
            builtin::check::display_input(&Value::String(r#"{"path":" hello.txt "}"#.into()), &tool_ctx(&cwd.to_string_lossy(), false)),
            Some(r#"{ path: "hello.txt" }"#.to_string())
        );
        // Unparseable arguments fall back to the raw input; execute reports the error.
        assert_eq!(display_of("READ", "not json", cwd), "not json");
    }

    #[test]
    fn pack_names_match_tools_index_order() {
        let names: Vec<String> = builtin_tool_pack(false).into_iter().map(|tool| tool.name).collect();
        assert_eq!(names, BUILTIN_TOOL_NAMES.iter().map(|name| name.to_string()).collect::<Vec<_>>());
        let modes: Vec<ChatToolMode> = builtin_tool_pack(false).into_iter().map(|tool| tool.mode).collect();
        assert_eq!(modes[4], ChatToolMode::Async);
        assert!(modes.iter().enumerate().all(|(index, mode)| index == 4 || *mode == ChatToolMode::Sync));
    }

    #[test]
    fn pack_schemas_match_builtin_definitions() {
        let definitions = [
            builtin::read::definition(),
            builtin::patch::definition(),
            builtin::dir::definition(),
            builtin::bash::definition(),
            builtin::bash::async_definition(),
            builtin::grep::definition(),
            builtin::verify::definition(),
            builtin::fetch::definition(),
            builtin::check::definition(),
        ];

        for (tool, definition) in builtin_tool_pack(false).iter().zip(definitions.iter()) {
            let rebuilt = crate::harness::transport::create_request_tool(
                &tool.name,
                &tool.description,
                serde_json::to_value(&tool.parameters).unwrap(),
            );
            assert_eq!(serde_json::to_value(&rebuilt).unwrap(), *definition, "{}", tool.name);
        }
    }

    #[test]
    fn sync_adapter_preserves_success_and_failure_text() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hello.txt"), "hi\n").unwrap();
        let tools = builtin_tool_pack(false);

        let (content, failed) = run(&tools, "READ", r#"{"path":"hello.txt"}"#, dir.path());
        assert!(!failed);
        let direct = builtin::read::execute(
            &json!({ "path": "hello.txt" }),
            &tool_ctx(&dir.path().to_string_lossy(), false),
        );
        assert_eq!(content, direct.text);

        let (content, failed) = run(&tools, "READ", r#"{"path":"missing.txt"}"#, dir.path());
        assert!(failed);
        let direct = builtin::read::execute(
            &json!({ "path": "missing.txt" }),
            &tool_ctx(&dir.path().to_string_lossy(), false),
        );
        assert_eq!(content, direct.text);
        assert!(content.starts_with("ERROR: "));
    }

    #[test]
    fn unknown_tool_reports_the_loader_message() {
        let dir = tempfile::tempdir().unwrap();
        let tools = builtin_tool_pack(false);
        let (content, failed) = run(&tools, "NOPE", "{}", dir.path());
        assert!(failed);
        assert_eq!(content, "ERROR: Tool \"NOPE\" is not available in the loaded tools folder.");
    }

    #[test]
    fn framework_tools_have_the_ts_names_and_clamps() {
        let names: Vec<String> = get_framework_tool_definitions().into_iter().map(|tool| tool.name).collect();
        assert_eq!(names, vec!["ASYNC_TAIL", "ASYNC_WAIT"]);
        assert_eq!(clamp_lines(None), 80);
        assert_eq!(clamp_lines(Some(0.0)), 1);
        assert_eq!(clamp_lines(Some(999.0)), 400);
        assert_eq!(clamp_lines(Some(12.9)), 12);
        assert_eq!(clamp_timeout_ms(None), 60_000);
        assert_eq!(clamp_timeout_ms(Some(-5.0)), 0);
        assert_eq!(clamp_timeout_ms(Some(9e9)), 3_600_000);
    }

    #[test]
    fn async_tail_reports_unknown_job() {
        let dir = tempfile::tempdir().unwrap();
        let tools = get_framework_tool_definitions();
        let (content, failed) = run(&tools, "ASYNC_TAIL", r#"{"jobId":"nope"}"#, dir.path());
        assert!(failed);
        assert!(content.starts_with("ERROR: "), "{content}");
    }
}
