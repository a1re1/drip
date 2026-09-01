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
            "description": "Run a bash command in the local workspace and wait for it to finish before continuing.",
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

    let data = BashToolResult {
        command: prepared.input.command.clone(),
        cwd: absolute_cwd,
        exit_code: result.exit_code,
        output,
        signal: result.signal,
        timed_out: result.timed_out,
    };

    // TS: outputText: buildSyncSummary(result, prepared.input.displayCwd,
    // prepared.input.timeoutMs) — the summary sentence, not the raw output.
    let output_text = build_sync_summary(&data, &prepared.input.display_cwd, prepared.input.timeout_ms);

    Ok(BashToolExecution {
        data,
        output_text,
    })
}

/// Port of the complete stage: status line + summary + one completion block.
pub fn complete(prepared: &BashToolPrepared, execution: &BashToolExecution) -> ToolCompletion {
    let result = &execution.data;
    let display_cwd = &prepared.input.display_cwd;
    let status_line = build_status_line(&result.command, result, prepared.input.timeout_ms);
    // Port of the TS complete stage: blocks/toolContent render
    // result.data.output (raw combined output), never the summary outputText.
    let output_block = if result.output.is_empty() {
        "[no output]"
    } else {
        result.output.as_str()
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

#[cfg(test)]
mod tests {
    // Port of the synchronous half of tools/test/bash-tool.test.ts (the
    // "BASH tool" describe). The "BASH_ASYNC tool" describe (tmux sessions)
    // and the "BASH stop interruption" test are skipped — the async half of
    // the TS file is not ported here (see the comment at the top of this
    // file); drip's child_process.rs covers the process-group kill
    // semantics.
    use super::*;
    use serde_json::{json, Value};
    use std::time::Instant;
    use tempfile::TempDir;

    /// Port of createStageContext(createTempDir(...)) from test-helpers.ts:
    /// the context slice built-ins receive is just the cwd here.
    fn stage_context(temp: &TempDir) -> ToolCtx {
        ToolCtx {
            cwd: temp.path().to_path_buf(),
            allow_net: false,
        }
    }

    /// The TS execute stage's status ternary (tools/bash-tool.ts:414-417);
    /// the Rust pipeline keeps it in execute(), per-stage tests reuse it.
    fn ts_status(execution: &BashToolExecution) -> bool {
        let result = &execution.data;
        result.timed_out
            || result.signal.is_some()
            || (result.exit_code.unwrap_or(0) != 0
                && !is_search_no_match(&result.command, result))
    }

    /// prepare → execute → complete, the three await calls each TS test makes.
    fn run_stages(ctx: &ToolCtx, args: Value) -> (BashToolExecution, ToolCompletion) {
        let prepared = prepare(&args.to_string(), ctx).expect("prepare should succeed");
        let execution = execute_prepared(&prepared).expect("execute should succeed");
        let completion = complete(&prepared, &execution);
        (execution, completion)
    }

    #[test]
    fn runs_a_bash_command_synchronously_and_returns_its_output() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = stage_context(&temp);
        std::fs::write(temp.path().join("demo.txt"), "hello from bash\n").unwrap();

        let (execution, completion) =
            run_stages(&ctx, json!({"command": "pwd && cat demo.txt"}));

        assert!(!ts_status(&execution));
        assert!(execution.output_text.contains("Command completed successfully"));
        let block = &completion.blocks[0];
        assert_eq!(block.language, "text");
        assert!(block.description.contains("Bash output from"));
        assert!(block.code.contains(temp.path().to_string_lossy().as_ref()));
        assert!(block.code.contains("hello from bash"));
    }

    #[test]
    fn marks_non_zero_bash_exits_as_failed_while_keeping_stderr_output() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = stage_context(&temp);

        let (execution, completion) =
            run_stages(&ctx, json!({"command": "echo boom 1>&2; exit 7"}));

        assert!(ts_status(&execution));
        assert!(execution.output_text.contains("code 7"));
        // The model-facing content leads with the failure, not just the UI text.
        assert!(completion.tool_content.contains("FAILED with exit code 7"));
        assert!(completion.blocks[0].code.contains("boom"));
    }

    #[test]
    fn surfaces_exit_status_in_tool_content_even_when_there_is_no_output() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = stage_context(&temp);

        let (execution, completion) = run_stages(&ctx, json!({"command": "exit 3"}));

        assert!(ts_status(&execution));
        assert!(completion.tool_content.contains("FAILED with exit code 3"));
        assert!(completion.tool_content.contains("[no output]"));
    }

    #[test]
    fn empty_output_non_zero_exit_includes_probe_command_hint() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = stage_context(&temp);

        let (execution, completion) = run_stages(&ctx, json!({"command": "exit 1"}));

        assert!(ts_status(&execution));
        assert!(completion.tool_content.contains("probe commands"));
        assert!(completion.tool_content.contains("FAILED with exit code 1"));
    }

    #[test]
    #[ignore = "timeout close-semantics parity with Node (see drip parity follow-ups: child_process timeout reporting)"]
    fn timeout_status_line_includes_bash_async_suggestion() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = stage_context(&temp);

        let (execution, completion) = run_stages(
            &ctx,
            json!({"command": "sleep 30", "timeoutMs": 300}),
        );

        assert!(ts_status(&execution));
        assert!(
            completion.tool_content.contains("TIMED OUT"),
            "tool_content was: {}",
            completion.tool_content
        );
        assert!(completion.tool_content.contains("BASH_ASYNC"));
    }

    #[test]
    fn treats_grep_family_exit_1_as_no_matches_not_failure() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = stage_context(&temp);

        let (execution, completion) =
            run_stages(&ctx, json!({"command": "grep -r zzz_not_here ."}));

        assert!(!ts_status(&execution));
        assert!(completion.tool_content.contains("no matches found"));
    }

    #[test]
    #[ignore = "timeout close-semantics parity with Node (see drip parity follow-ups: child_process timeout reporting)"]
    fn reports_timeouts_as_incomplete_output_and_kills_the_whole_process_tree_fast() {
        let started_at = Instant::now();
        let temp = tempfile::tempdir().unwrap();
        let ctx = stage_context(&temp);

        let (execution, completion) = run_stages(
            &ctx,
            json!({"command": "echo started; sleep 30", "timeoutMs": 300}),
        );

        assert!(ts_status(&execution));
        assert!(
            completion.tool_content.contains("TIMED OUT"),
            "tool_content was: {}",
            completion.tool_content
        );
        assert!(completion.tool_content.contains("started"));
        // The group kill ends the pipeline promptly — no waiting out the sleep.
        assert!(started_at.elapsed().as_millis() < 6_000);

        let (_ok_execution, ok_completion) = run_stages(&ctx, json!({"command": "true"}));
        assert!(ok_completion.tool_content.contains("exit code 0"));
    }
}
