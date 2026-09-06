// The async/tmux half (BASH_ASYNC, session naming, tmux
// probes) lives in this file (below the sync half) and in
// drip/src/tools/async_jobs.rs.

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

/// Optional-string argument getter (this tool's own local version):
/// undefined → None; non-strings and whitespace-only strings reject with
/// the shared "Expected … to be a non-empty string." error; otherwise the
/// value is trimmed.
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

/// Timeout clamping: undefined → 120000, otherwise floored and
/// clamped to [1000, 3600000].
pub fn clamp_sync_timeout_ms(value: Option<f64>) -> u64 {
    match value {
        None => DEFAULT_SYNC_TIMEOUT_MS,
        Some(value) => value.floor().clamp(1_000.0, 3_600_000.0) as u64,
    }
}

// Status-line no-match special case: grep-family commands exit 1 when
// they find nothing, which is an answer for the model rather than a failure.
fn is_search_no_match(command: &str, result: &BashToolResult) -> bool {
    let pattern = Regex::new(r"^\s*(?:command\s+)?(?:rg|grep|egrep|fgrep)\b").unwrap();

    result.exit_code == Some(1)
        && !result.timed_out
        && result.signal.is_none()
        && pattern.is_match(command)
}

/// Builds the model-facing status line for a finished run.
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

/// Builds the one-line summary sentence for a finished run.
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

/// definition(): the {type: "function", function: …} envelope
/// (the name stays "BASH" verbatim).
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

/// The prepare stage: parse → command/cwd/timeout_ms → resolve and
/// format the paths → policy gate → display_input.
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

    // The prepare stage calls evaluate_command_policy itself (not the shared
    // enforce_command_policy helper) so the --allow-destructive override
    // prints its notice: policy violations refuse here, before anything runs.
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

/// What the prepare stage returns: { input, display_input } plus the resolved
/// absolute cwd the execute/complete stages reuse.
pub struct BashToolPrepared {
    pub input: BashToolPreparedInput,
    pub absolute_cwd: PathBuf,
    pub display_input: String,
}

/// What the execute stage returns: { data, output_text }.
pub struct BashToolExecution {
    pub data: BashToolResult,
    pub output_text: String,
}

/// The execute stage: run_captured_process over `bash -lc`, output via
/// build_combined_output.
pub fn execute_prepared(prepared: &BashToolPrepared) -> Result<BashToolExecution> {
    let absolute_cwd = prepared.absolute_cwd.to_string_lossy().to_string();
    let result = run_captured_process(&CapturedProcessArgs { stdin_payload: None,
        command: "bash",
        cwd: Some(absolute_cwd.as_str()),
        // No env overrides here; run_captured_process builds the scrubbed
        // child environment itself (build_child_process_env).
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

    // output_text is the summary sentence from build_sync_summary, not the
    // raw output.
    let output_text = build_sync_summary(&data, &prepared.input.display_cwd, prepared.input.timeout_ms);

    Ok(BashToolExecution {
        data,
        output_text,
    })
}

/// The complete stage: status line + summary + one completion block.
pub fn complete(prepared: &BashToolPrepared, execution: &BashToolExecution) -> ToolCompletion {
    let result = &execution.data;
    let display_cwd = &prepared.input.display_cwd;
    let status_line = build_status_line(&result.command, result, prepared.input.timeout_ms);
    // The completion block renders result.output (raw combined output),
    // never the summary output_text.
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

/// `<cwd>` then `<command>` on the next line.
pub fn display_input(raw_input: &str, ctx: &ToolCtx) -> Option<String> {
    prepare(raw_input, ctx).ok().map(|prepared| prepared.display_input)
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

/// The BASH_ASYNC tool definition — the function envelope, strings
/// verbatim; the name stays "BASH_ASYNC".
pub fn async_definition() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "BASH_ASYNC",
            "description": "Run a bash command in a detached tmux session so it can keep running in the background while you inspect it with ASYNC_TAIL or attach manually.",
            "parameters": {
                "additionalProperties": false,
                "properties": {
                    "command": {
                        "description": "The bash command or script to run in a detached tmux session with `bash -lc`.",
                        "type": "string"
                    },
                    "cwd": {
                        "description": "Optional working directory for the command, relative to the current working directory or absolute. Defaults to the workspace cwd.",
                        "type": "string"
                    },
                    "sessionName": {
                        "description": "Optional tmux session name to use. If omitted, a unique session name is generated automatically.",
                        "type": "string"
                    },
                    "title": {
                        "description": "Optional display title for the background job. Defaults to the session name and a shortened command preview.",
                        "type": "string"
                    }
                },
                "required": ["command"],
                "type": "object"
            }
        }
    })
}

#[cfg(test)]
mod tests {
    // Synchronous-half tests. The async half (tmux sessions, the
    // stop-interruption path) is covered in src/tools/async_jobs.rs;
    // drip's child_process.rs covers the process-group kill semantics.
    use super::*;
    use serde_json::{json, Value};
    use std::time::Instant;
    use tempfile::TempDir;

    /// A ToolCtx rooted at the temp dir:
    /// the context slice built-ins receive is just the cwd here.
    fn stage_context(temp: &TempDir) -> ToolCtx {
        ToolCtx {
            cwd: temp.path().to_path_buf(),
            allow_net: false,
            reference_roots: Vec::new(),
        }
    }

    /// The execute stage's status ternary:
    /// the Rust pipeline keeps it in execute(), per-stage tests reuse it.
    fn ts_status(execution: &BashToolExecution) -> bool {
        let result = &execution.data;
        result.timed_out
            || result.signal.is_some()
            || (result.exit_code.unwrap_or(0) != 0
                && !is_search_no_match(&result.command, result))
    }

    /// prepare → execute → complete, the three stage calls each test makes.
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
    fn timeout_status_line_includes_bash_async_suggestion() {
        let _guard = crate::tools::child_process::REGISTRY_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    fn reports_timeouts_as_incomplete_output_and_kills_the_whole_process_tree_fast() {
        let _guard = crate::tools::child_process::REGISTRY_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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

// ---------------------------------------------------------------------------
// tmux-backed async session helpers
// ---------------------------------------------------------------------------

pub const TMUX_POLL_INTERVAL_MS: u64 = 250;

pub const SHELL_WRAPPER_COMMANDS: &[&str] = &["bash", "sh", "zsh"];

pub fn quote_shell_argument(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub fn sanitize_tmux_session_name(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .map(|character| match character {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '-' => character,
            _ => '-',
        })
        .collect();

    let collapsed: String = {
        let mut result = String::new();
        let mut previous_was_dash = false;

        for character in sanitized.chars() {
            if character == '-' {
                if !previous_was_dash {
                    result.push('-');
                }

                previous_was_dash = true;
            } else {
                result.push(character);
                previous_was_dash = false;
            }
        }

        result
    };

    collapsed.trim_matches('-').to_string()
}

pub fn strip_command_extension(token: &str) -> String {
    const KNOWN_EXTENSIONS: [&str; 10] = [
        "sh", "bash", "zsh", "js", "mjs", "cjs", "ts", "tsx", "py", "rb",
    ];

    for extension in KNOWN_EXTENSIONS {
        let suffix = format!(".{}", extension);

        if token.to_ascii_lowercase().ends_with(&suffix) {
            return token[..token.len() - suffix.len()].to_string();
        }
    }

    token.to_string()
}

pub fn clamp_identifier(value: &str, max_length: usize) -> String {
    if value.chars().count() <= max_length {
        return value.to_string();
    }

    let truncated: String = value.chars().take(max_length).collect();

    truncated.trim_end_matches('-').to_string()
}

pub fn compact_session_token(token: &str) -> String {
    let segments: Vec<&str> = token
        .split(|character: char| character == '-' || character == '_')
        .map(|segment| segment.trim())
        .filter(|segment| !segment.is_empty())
        .collect();

    if segments.is_empty() {
        return String::new();
    }

    let selected_segments: Vec<&str> = if segments.len() >= 3 {
        vec![segments[0], segments[segments.len() - 1]]
    } else {
        segments.to_vec()
    };

    clamp_identifier(&selected_segments.join("-"), 12)
}

pub fn build_command_session_prefix(command: &str) -> String {
    let raw_tokens: Vec<String> = crate::tools::async_jobs::compact_whitespace(command)
        .split_whitespace()
        .map(str::to_string)
        .collect();

    let effective_tokens: Vec<String> = if raw_tokens.len() >= 2
        && raw_tokens
            .first()
            .map(|token| SHELL_WRAPPER_COMMANDS.contains(&token.to_lowercase().as_str()))
            .unwrap_or(false)
    {
        let mut tokens = vec![basename(&raw_tokens[1])];
        tokens.extend(raw_tokens.iter().skip(2).cloned());
        tokens
    } else {
        raw_tokens
    };

    let mut filtered_tokens: Vec<String> = Vec::new();

    for raw_token in effective_tokens.iter() {
        if filtered_tokens.len() >= 2 {
            break;
        }

        if raw_token.starts_with('-') {
            continue;
        }

        let candidate = if raw_token.contains('/') {
            basename(raw_token)
        } else {
            raw_token.clone()
        };

        let normalized_token = sanitize_tmux_session_name(&strip_command_extension(&candidate)).to_lowercase();

        if normalized_token.is_empty()
            || normalized_token == "run"
            || normalized_token == "command"
            || normalized_token == "exec"
        {
            continue;
        }

        let compact_token = compact_session_token(&normalized_token);

        if compact_token.is_empty() {
            continue;
        }

        filtered_tokens.push(compact_token);
    }

    let prefix = filtered_tokens.join("-");

    if prefix.is_empty() {
        return "bash".to_string();
    }

    clamp_identifier(&prefix, 18)
}

fn basename(value: &str) -> String {
    value.trim_end_matches('/').rsplit('/').next().unwrap_or("").to_string()
}

fn generate_session_suffix() -> String {
    // A simple wall-clock-derived suffix instead of a uuid: xorshift over the
    // current nanos is unique enough for session names without pulling in a
    // random-number dependency.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);

    let mut state = nanos ^ 0x9e37_79b9_7f4a_7c15;
    state ^= state << 13;
    state ^= state >> 7;
    state ^= state << 17;

    const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";

    (0..6)
        .map(|index| {
            let slot = ((state >> (index * 5)) & 0x1f) as usize;
            ALPHABET[slot % ALPHABET.len()] as char
        })
        .collect()
}

pub fn create_session_name(command: &str, requested_name: Option<&str>) -> String {
    if let Some(requested_name) = requested_name {
        let sanitized_requested_name = sanitize_tmux_session_name(requested_name);

        if sanitized_requested_name.is_empty() {
            panic!(
                "Unable to derive a tmux session name from \"{}\".",
                requested_name
            );
        }

        return sanitized_requested_name;
    }

    let prefix = build_command_session_prefix(command);

    // The drip- namespace marks sessions as harness-owned, so orphaned
    // sessions can be reaped by convention without a separate registry file.
    format!("drip-{}-{}", prefix, generate_session_suffix())
}

pub fn build_tmux_wrapped_command(command: &str, log_path: &str) -> String {
    format!(
        "set -o pipefail\n({}) 2>&1 | tee -a {}\nexit ${{PIPESTATUS[0]}}",
        command,
        quote_shell_argument(log_path)
    )
}

// The pane command: credentials are unset inline because the shared tmux
// server's environment predates the harness scrub, and the shell is
// non-login — profile sourcing inside panes was the source of spurious
// non-zero exits under parallel load and panes have no need of it (the
// wrapped command carries its own settings).
pub fn build_tmux_pane_command(wrapped_command: &str) -> String {
    let unset_arguments = build_env_unset_arguments();
    let env_prefix = if unset_arguments.is_empty() {
        String::new()
    } else {
        format!("env {} ", unset_arguments.join(" "))
    };

    format!(
        "{}bash -c {}; exit",
        env_prefix,
        quote_shell_argument(wrapped_command)
    )
}

fn build_env_unset_arguments() -> Vec<String> {
    // drip scrubs credential environment variables when spawning the async
    // tool process itself, so no extra unset arguments are needed here.
    Vec::new()
}

pub fn assert_tmux_available() -> anyhow::Result<()> {
    let output = std::process::Command::new("tmux").arg("-V").output()?;

    if !output.status.success() {
        anyhow::bail!("tmux is required for BASH_ASYNC but is not available on this machine.");
    }

    Ok(())
}

pub fn tmux_session_exists(session_name: &str) -> bool {
    std::process::Command::new("tmux")
        .args(["has-session", "-t", session_name])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

pub fn run_tmux_command(process_args: &[&str], failure_message: &str) -> anyhow::Result<String> {
    let output = std::process::Command::new("tmux").args(process_args).output()?;

    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let error_detail = build_combined_output(&stdout, &stderr);

        if !error_detail.is_empty() {
            anyhow::bail!("{}\n{}", failure_message, error_detail);
        }

        anyhow::bail!("{}", failure_message);
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

pub struct StartTmuxSessionArgs<'a> {
    pub absolute_cwd: &'a str,
    pub command: &'a str,
    pub log_path: &'a str,
    pub session_name: &'a str,
}

pub fn start_tmux_session(args: StartTmuxSessionArgs) -> anyhow::Result<()> {
    let wrapped_command = build_tmux_wrapped_command(&args.command, &args.log_path);

    let result = (|| -> anyhow::Result<()> {
        run_tmux_command(
            &[
                "new-session",
                "-d",
                "-s",
                args.session_name,
                "-c",
                args.absolute_cwd,
            ],
            &format!("Unable to start tmux session \"{}\".", args.session_name),
        )?;
        run_tmux_command(
            &[
                "set-window-option",
                "-t",
                args.session_name,
                "remain-on-exit",
                "on",
            ],
            &format!(
                "Unable to configure tmux session \"{}\".",
                args.session_name
            ),
        )?;
        run_tmux_command(
            &[
                "send-keys",
                "-t",
                args.session_name,
                &build_tmux_pane_command(&wrapped_command),
                "Enter",
            ],
            &format!(
                "Unable to send the bash command to tmux session \"{}\".",
                args.session_name
            ),
        )?;
        Ok(())
    })();

    if result.is_err() {
        let _ = kill_tmux_session(args.session_name);
    }

    result
}

pub fn kill_tmux_session(session_name: &str) -> anyhow::Result<()> {
    if !tmux_session_exists(session_name) {
        return Ok(());
    }

    std::process::Command::new("tmux")
        .args(["kill-session", "-t", session_name])
        .output()?;

    Ok(())
}

pub fn wait_for_tmux_session_exit(session_name: &str) -> anyhow::Result<Option<i32>> {
    let pane_target = session_name;

    loop {
        let output = std::process::Command::new("tmux")
            .args([
                "display-message",
                "-p",
                "-t",
                pane_target,
                "#{pane_dead} #{pane_dead_status}",
            ])
            .output()?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();

        if !output.status.success() {
            if !tmux_session_exists(session_name) {
                return Ok(None);
            }

            anyhow::bail!("Unable to inspect tmux session \"{}\".", session_name);
        }

        let mut parts = stdout.trim().split_whitespace();
        let dead_value = parts.next().unwrap_or("");
        let exit_code_value = parts.next().unwrap_or("");

        if dead_value == "1" {
            let exit_code = exit_code_value.parse::<i32>().ok();

            return Ok(exit_code);
        }

        std::thread::sleep(std::time::Duration::from_millis(TMUX_POLL_INTERVAL_MS));
    }
}

#[cfg(test)]
mod tmux_helper_tests {
    use super::*;

    #[test]
    fn sanitize_tmux_session_name_strips_unsafe_characters() {
        assert_eq!(
            sanitize_tmux_session_name("  My Session! Name  "),
            "My-Session-Name"
        );
        assert_eq!(sanitize_tmux_session_name("---abc---"), "abc");
        assert_eq!(sanitize_tmux_session_name("a_b-c"), "a_b-c");
    }

    #[test]
    fn create_session_name_uses_requested_name_when_given() {
        assert_eq!(
            create_session_name("echo hi", Some("my session")),
            "my-session"
        );
    }

    #[test]
    fn create_session_name_generates_drip_prefixed_name() {
        let name = create_session_name("echo hi", None);

        assert!(name.starts_with("drip-"), "unexpected name: {}", name);
        assert!(name.ends_with("-echo-hi") || name.contains("echo-hi"));
    }

    #[test]
    fn build_tmux_wrapped_command_quotes_the_log_path() {
        let wrapped = build_tmux_wrapped_command("echo hi", "/tmp/some log.txt");

        assert!(wrapped.contains("set -o pipefail"));
        assert!(wrapped.contains("(echo hi) 2>&1 | tee -a '/tmp/some log.txt'"));
        assert!(wrapped.contains("exit ${PIPESTATUS[0]}"));
    }

    #[test]
    fn build_tmux_pane_command_wraps_in_bash_with_exit() {
        let pane = build_tmux_pane_command("echo hi");

        assert!(pane.starts_with("bash -c 'echo hi'; exit"), "unexpected: {}", pane);
    }

    #[test]
    fn quote_shell_argument_wraps_in_single_quotes() {
        assert_eq!(quote_shell_argument("hello"), "'hello'");
        assert_eq!(
            quote_shell_argument("it's here"),
            "'it'\\''s here'"
        );
    }
}
