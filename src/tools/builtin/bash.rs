// port of tools/bash-tool.ts (the synchronous BASH tool)
//
// The async/tmux half of the TS file (BASH_ASYNC, session names, terminate
// plumbing) is not ported here; drip covers the process-group kill semantics
// in child_process.rs.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use regex::Regex;
use serde_json::{json, Value};

use super::{ToolCompletion, ToolCompletionBlock, ToolCtx, ToolOutcome};
use crate::tools::helpers::{
    assert_directory_path, format_tool_path, get_optional_number_argument,
    get_required_string_argument, parse_tool_arguments, resolve_tool_path,
};
use crate::tools::child_process::{build_combined_output, run_captured_process, CapturedProcessArgs};
use crate::tools::command_policy::{
    allow_destructive_enabled, evaluate_command_policy, format_policy_refusal,
};

pub const DEFAULT_SYNC_TIMEOUT_MS: u64 = 120_000;

type BashToolInput = serde_json::Map<String, Value>;

#[derive(Debug, Clone)]
pub struct BashToolResult {
    pub command: String,
    pub cwd: String,
    pub exit_code: Option<i32>,
    pub output: String,
    pub signal: Option<String>,
    pub timed_out: bool,
}

/// Port of getOptionalStringArgument (this tool's own local version): undefined
/// → None; non-strings and whitespace-only strings reject with the shared
/// "Expected … to be a non-empty string." error; otherwise the value is
/// trimmed.
pub fn get_optional_string_argument(
    args: &BashToolInput,
    key: &str,
) -> Result<Option<String>> {
    let Some(value) = args.get(key) else {
        return Ok(None);
    };

    let Some(text) = value.as_str() else {
        anyhow::bail!("Expected \"{key}\" to be a non-empty string.");
    };

    let trimmed = text.trim();

    if trimmed.is_empty() {
        anyhow::bail!("Expected \"{key}\" to be a non-empty string.");
    }

    Ok(Some(trimmed.to_string()))
}

/// Port of clampSyncTimeoutMs: undefined → 120000, otherwise floored and
/// clamped to [1000, 3600000].
pub fn clamp_sync_timeout_ms(value: Option<f64>) -> u64 {
    match value {
        None => DEFAULT_SYNC_TIMEOUT_MS,
        Some(value) => value.floor().clamp(1_000.0, 3_600_000.0) as u64,
    }
}

// buildStatusLine's no-match special case: grep-family commands exit 1 when
// they find nothing, which is an answer for the model rather than a failure.
fn is_search_no_match(command: &str, result: &BashToolResult) -> bool {
    let pattern = Regex::new(r"^\s*(?:command\s+)?(?:rg|grep|egrep|fgrep)\b").unwrap();

    result.exit_code == Some(1)
        && !result.timed_out
        && result.signal.is_none()
        && pattern.is_match(command)
}

/// Port of buildStatusLine(command, result, timeoutMs).
pub fn build_status_line(
    command: &str,
    result: &BashToolResult,
    timeout_ms: u64,
) -> String {
    if result.timed_out {
        return format!(
            "TIMED OUT after {timeout_ms}ms — the command was killed and output below is incomplete. Narrow the command (e.g. scope find/grep to a subdirectory) or move genuinely long-running work to BASH_ASYNC"
        );
    }

    if let Some(signal) = &result.signal {
        return format!("KILLED by {signal} — output below is incomplete");
    }

    if is_search_no_match(command, result) {
        return "exit code 1 — no matches found (an answer, not a failure)".to_string();
    }

    if result.exit_code.unwrap_or(0) != 0 {
        if result.output.trim().is_empty() {
            return "FAILED with exit code ".to_string()
                + &result.exit_code.unwrap_or(0).to_string()
                + " (no output — probe commands like ls/find/test exit non-zero to mean \"not found\"; if stderr was suppressed with 2>/dev/null, re-run without it to see the error)";
        }

        return format!("FAILED with exit code {}", result.exit_code.unwrap_or(0));
    }

    "exit code 0".to_string()
}

/// Port of buildSyncSummary(result, displayCwd, timeoutMs).
pub fn build_sync_summary(
    result: &BashToolResult,
    display_cwd: &str,
    timeout_ms: u64,
) -> String {
    if result.timed_out {
        return format!("Command timed out after {timeout_ms}ms in {display_cwd}.");
    }

    if let Some(signal) = &result.signal {
        return format!("Command exited due to {signal} in {display_cwd}.");
    }

    if result.exit_code.unwrap_or(0) != 0 {
        return format!(
            "Command exited with code {} in {display_cwd}.",
            result.exit_code.unwrap_or(0)
        );
    }

    format!("Command completed successfully in {display_cwd}.")
}

/// Port of definition() → buildTransportTools' {type: "function", function: …}
/// envelope (the name stays "BASH" verbatim).
pub fn definition() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "BASH",
            "description": "Run a bash command in the workspace and return its combined output. Prefer scoped commands (narrow finds/greps, head/tail for long output) and a timeout when a command might hang. Every invocation runs synchronously in its own session; use BASH_ASYNC when a command must outlive the current turn or you want to continue while it runs.",
            "parameters": {
                "additionalProperties": false,
                "properties": {
                    "command": {
                        "description": "The bash command or script to run with `bash -lc`.",
                        "type": "string"
                    },
                    "cwd": {
                        "description": "Optional working directory for the command, relative to the current working directory or absolute. Defaults to the workspace cwd.",
                        "type": "string"
                    },
                    "timeoutMs": {
                        "description": "Optional timeout for the command in milliseconds. Defaults to 120000.",
                        "type": "number"
                    }
                },
                "required": ["command"],
                "type": "object"
            }
        }
    })
}

/// Port of the prepare stage: parse → command/cwd/timeoutMs → resolve and
/// format the paths → policy gate → displayInput.
pub fn prepare(raw_input: &str, ctx: &ToolCtx) -> Result<BashToolPrepared> {
    let args = parse_tool_arguments(raw_input)?;

    let command = get_required_string_argument(&args, "command")?;
    let cwd_argument = get_optional_string_argument(&args, "cwd")?;
    let timeout_argument = get_optional_number_argument(&args, "timeoutMs")?;

    let workspace_root = ctx.cwd.to_string_lossy().to_string();
    let absolute_cwd = resolve_tool_path(&workspace_root, cwd_argument.as_deref().unwrap_or("."));
    assert_directory_path(&absolute_cwd, "cwd")?;
    let display_cwd = format_tool_path(&workspace_root, &absolute_cwd);
    let timeout_ms = clamp_sync_timeout_ms(timeout_argument);

    // The TS prepare calls evaluateCommandPolicy itself (not the shared
    // enforceCommandPolicy helper) so the --allow-destructive override prints
    // its notice: policy violations refuse here, before anything runs.
    match evaluate_command_policy(&command, &workspace_root) {
        crate::tools::command_policy::CommandPolicyVerdict::Block { rule, why } => {
            if allow_destructive_enabled() {
                eprintln!(
                    "[policy] allowing destructive command (rule {rule}, --allow-destructive): {command}"
                );
            } else {
                anyhow::bail!("{}", format_policy_refusal(&command, &rule, &why));
            }
        }
        crate::tools::command_policy::CommandPolicyVerdict::Allow => {}
    }

    let display_input = format!("{display_cwd}\n{command}");

    Ok(BashToolPrepared {
        input: BashToolPreparedInput {
            command,
            display_cwd,
            timeout_ms,
        },
        absolute_cwd,
        display_input,
    })
}

pub struct BashToolPreparedInput {
    pub command: String,
    pub display_cwd: String,
    pub timeout_ms: u64,
}

/// What the prepare stage returns: { input, displayInput } plus the resolved
/// absolute cwd the execute/complete stages reuse.
pub struct BashToolPrepared {
    pub input: BashToolPreparedInput,
    pub absolute_cwd: PathBuf,
    pub display_input: String,
}

/// What the execute stage returns: { data, outputText }.
pub struct BashToolExecution {
    pub data: BashToolResult,
    pub output_text: String,
}

/// Port of the execute stage: runCapturedProcess over `bash -lc`, output via
/// buildCombinedOutput.
pub fn execute_prepared(prepared: &BashToolPrepared) -> Result<BashToolExecution> {
    let absolute_cwd = prepared.absolute_cwd.to_string_lossy().to_string();
    let result = run_captured_process(&CapturedProcessArgs {
        command: "bash",
        cwd: Some(absolute_cwd.as_str()),
        // The TS call site passes no env overrides; runCapturedProcess builds
        // the scrubbed child environment itself (buildChildProcessEnv).
        env: None,
        process_args: &["-lc".to_string(), prepared.input.command.clone()],
        timeout_ms: Some(prepared.input.timeout_ms),
    })
    .map_err(|error| anyhow::anyhow!("{error}"))?;

    let output = build_combined_output(&result.stdout, &result.stderr);

    Ok(BashToolExecution {
        data: BashToolResult {
            command: prepared.input.command.clone(),
            cwd: absolute_cwd,
            exit_code: result.exit_code,
            output: output.clone(),
            signal: result.signal,
            timed_out: result.timed_out,
        },
        output_text: output,
    })
}

/// Port of the complete stage: status line + summary + one completion block.
pub fn complete(prepared: &BashToolPrepared, execution: &BashToolExecution) -> ToolCompletion {
    let result = &execution.data;
    let display_cwd = &prepared.input.display_cwd;
    let status_line = build_status_line(&result.command, result, prepared.input.timeout_ms);
    let summary = build_sync_summary(result, display_cwd, prepared.input.timeout_ms);
    let output_text = &execution.output_text;
    let output_block = if output_text.is_empty() {
        "[no output]"
    } else {
        output_text
    };

    ToolCompletion {
        blocks: vec![ToolCompletionBlock {
            code: output_block.to_string(),
            description: format!("Bash output from {display_cwd} ({status_line})."),
            language: "text".to_string(),
            path: prepared.absolute_cwd.clone(),
        }],
        tool_content: format!(
            "Bash command output from {display_cwd} — {status_line}.\n\n{output_block}"
        ),
    }
}

/// Whole-pipeline entry point: prepare → execute → complete. A failed stage
/// maps to the model-facing ERROR text; a finished run is marked failed unless
/// the command exited cleanly (grep-family "no matches" exits stay completed).
pub fn execute(raw_input: &str, ctx: &ToolCtx) -> ToolOutcome {
    let prepared = match prepare(raw_input, ctx) {
        Ok(prepared) => prepared,
        Err(error) => return ToolOutcome::error(error),
    };

    match execute_prepared(&prepared) {
        Ok(execution) => {
            let completion = complete(&prepared, &execution);
            let result = &execution.data;
            let failed = result.timed_out
                || result.signal.is_some()
                || (result.exit_code.unwrap_or(0) != 0 && !is_search_no_match(&result.command, result));
            ToolOutcome {
                text: completion.tool_content,
                failed,
            }
        }
        Err(error) => ToolOutcome::error(error),
    }
}
