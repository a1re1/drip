// port of tools/check-tool.ts
//
// Deviation from the TS source: lci runs the TypeScript language service
// in-process (createOrGetService + collectDiagnostics over the service host).
// drip instead spawns `bunx tsc --noEmit --pretty false -p <tsconfig>` (with
// `npx tsc` as the fallback when bunx is missing) and parses the emitted
// `file(line,col): error TSxxxx: message` lines into the same
// DiagnosticEntry { file, line, message } shape. Everything the model sees —
// scope filtering, summary lines, error strings — matches the TS tool.

use anyhow::Result;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

use super::{ToolCompletion, ToolCompletionBlock, ToolCtx, ToolOutcome};
use crate::tools::helpers::resolve_tool_path;

/// TS type CheckToolInput.
#[derive(Debug)]
pub struct CheckToolInput {
    pub path: Option<PathBuf>,
    pub workspace_root: PathBuf,
}

/// TS type DiagnosticEntry.
#[derive(Debug, Clone, PartialEq)]
pub struct DiagnosticEntry {
    pub file: String,
    pub line: i64,
    pub message: String,
}

/// TS type CheckToolResult.
#[derive(Debug, Clone)]
pub struct CheckToolResult {
    pub diagnostics: Vec<DiagnosticEntry>,
    pub elsewhere_count: usize,
    pub scope: String,
    pub total_errors: usize,
}

/// What the prepare stage returns: { input, displayInput }.
#[derive(Debug)]
pub struct CheckToolPrepared {
    pub input: CheckToolInput,
    pub display_input: String,
}

/// What the execute stage returns: { data, outputText }.
#[derive(Debug)]
pub struct CheckToolExecution {
    pub data: CheckToolResult,
    pub output_text: String,
}

/// The OpenAI function definition lci sends for this tool (the
/// {type: "function", function: {...}} envelope built by
/// buildTransportTools in src/chat/runtime.ts).
pub fn definition() -> Value {
    json!({
        "type": "function",
        "function": {
            "description": "Run incremental TypeScript semantic + syntactic diagnostics on the workspace. Input: {path?: string}. With path, reports full diagnostics for that file plus an elsewhere count; without path, reports project-wide diagnostics. Reuses a cached LanguageService per tsconfig for performance.",
            "name": "CHECK",
            "parameters": {
                "additionalProperties": false,
                "properties": {
                    "path": {
                        "description": "Optional file path to focus diagnostics on. Relative paths are resolved from the workspace root.",
                        "type": "string"
                    }
                },
                "required": [],
                "type": "object"
            }
        }
    })
}

/// Port of the prepare stage. Parses the raw arguments, resolves the target
/// file (if any) and keeps the workspace root from the context cwd.
pub fn prepare(args: &Value, ctx: &ToolCtx) -> Result<CheckToolPrepared> {
    let workspace_root = ctx.cwd.to_string_lossy().to_string();

    let (file_path, has_path) = match args.get("path") {
        None | Some(Value::Null) => (None, false),
        Some(Value::String(raw_path)) => {
            let resolved = resolve_tool_path(&workspace_root, raw_path);
            (Some(resolved), true)
        }
        Some(_) => {
            return Err(anyhow::anyhow!("\"path\" must be a string."));
        }
    };

    let display_input = if has_path {
        format!("{{ path: \"{}\" }}", file_path.as_ref().unwrap().display())
    } else {
        "{}".to_string()
    };

    Ok(CheckToolPrepared {
        input: CheckToolInput {
            path: file_path,
            workspace_root: PathBuf::from(&workspace_root),
        },
        display_input,
    })
}

/// Port of the execute stage. Runs tsc over the workspace tsconfig and maps
/// its output into the same DiagnosticEntry shape the TS tool produces.
pub fn execute_prepared(prepared: &CheckToolPrepared) -> Result<CheckToolExecution> {
    // Locate tsconfig
    let start_dir = match &prepared.input.path {
        Some(path) => path
            .parent()
            .map(|parent| parent.to_path_buf())
            .unwrap_or_else(|| PathBuf::from(".")),
        None => PathBuf::from(&prepared.input.workspace_root),
    };
    let tsconfig_path = find_tsconfig(&start_dir, &prepared.input.workspace_root)?;

    // Run tsc (bunx preferred, npx fallback)
    let output = run_tsc(&tsconfig_path)?;

    let all_diags = parse_diagnostics_output(&output);

    let result = scope_result(
        all_diags,
        prepared.input.path.as_ref(),
        &prepared.input.workspace_root,
        &prepared.input.workspace_root,
    );

    let lead_line = format!("CHECK: {} error(s) in {}", result.total_errors, result.scope);

    Ok(CheckToolExecution {
        data: result,
        output_text: lead_line,
    })
}

/// Focused-vs-project scope filtering from the TS execute stage. With a
/// target path, diagnostics in that file stay in `diagnostics` (display
/// paths relative to the workspace root, matching the TS output) and the
/// rest are counted in `elsewhere_count`; without one, everything is kept
/// and scope is "project".
fn scope_result(
    all_diags: Vec<DiagnosticEntry>,
    target: Option<&PathBuf>,
    workspace_root: &Path,
    cwd: &Path,
) -> CheckToolResult {
    // TS maps every diagnostic file to its display path (relative to the
    // workspace root when it startsWith it) as diagnostics are collected, so
    // apply display_file_path before filtering.
    let root_display = workspace_root.to_string_lossy().to_string();
    let all_diags: Vec<DiagnosticEntry> = all_diags
        .into_iter()
        .map(|mut entry| {
            entry.file = display_file_path(&entry.file, &root_display);
            entry
        })
        .collect();

    let diagnostics;
    let elsewhere_count;
    let scope;

    if let Some(target_path) = target {
        // Focused mode: show diagnostics for targeted file, count elsewhere
        let target_display = display_file_path(
            target_path.to_string_lossy().as_ref(),
            workspace_root.to_string_lossy().as_ref(),
        );
        let file_diags: Vec<DiagnosticEntry> = all_diags
            .iter()
            .filter(|entry| entry.file == target_display)
            .cloned()
            .collect();
        elsewhere_count = all_diags.len().saturating_sub(file_diags.len());
        diagnostics = file_diags;
        scope = target_display;
    } else {
        // Project-wide mode
        elsewhere_count = 0;
        diagnostics = all_diags;
        scope = "project".to_string();
    }

    let total_errors = diagnostics.len() + elsewhere_count;

    CheckToolResult {
        diagnostics,
        elsewhere_count,
        scope,
        total_errors,
    }
}

/// Port of findTsconfig: walk up from `start_dir` looking for tsconfig.json.
/// Falls back to `workspace_root/tsconfig.json` if not found.
fn find_tsconfig(start_dir: &Path, workspace_root: &Path) -> Result<PathBuf> {
    // Don't walk above workspace root
    let root = resolve_workspace_path(workspace_root);

    let mut dir = start_dir.to_path_buf();

    loop {
        let candidate = dir.join("tsconfig.json");

        if candidate.exists() {
            return Ok(candidate);
        }

        let parent = match dir.parent() {
            Some(parent) => parent.to_path_buf(),
            None => break,
        };

        if parent == dir || dir == root {
            break;
        }

        dir = parent;
    }

    // Try workspace root itself
    let root_config = workspace_root.join("tsconfig.json");

    if root_config.exists() {
        return Ok(root_config);
    }

    Err(anyhow::anyhow!(
        "No tsconfig.json found walking up from {} or at workspace root {}",
        start_dir.display(),
        workspace_root.display()
    ))
}

/// Normalize a path for the startDir/root comparison the TS port does via
/// resolve(): make it absolute (cwd-relative inputs are already resolved by
/// the time this runs) so `dir == root` checks line up.
fn resolve_workspace_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

/// Spawn tsc with the tsconfig's directory as cwd and capture its output.
/// Prefers bunx, falls back to npx; fails when neither is available.
fn run_tsc(tsconfig_path: &Path) -> Result<String> {
    let tsconfig = tsconfig_path.to_string_lossy().to_string();

    for command_name in ["bunx", "npx"] {
        let mut command = Command::new(command_name);
        command
            .arg("tsc")
            .arg("--noEmit")
            .arg("--pretty")
            .arg("false")
            .arg("-p")
            .arg(&tsconfig);

        // Inherit the workspace root as the starting cwd; the spawn itself
        // fails fast when the binary does not exist.
        let output = match command.output() {
            Ok(output) => output,
            Err(_) => continue,
        };

        let mut combined = String::from_utf8_lossy(&output.stdout).to_string();
        if !output.stderr.is_empty() {
            if !combined.is_empty() && !combined.ends_with('\n') {
                combined.push('\n');
            }
            combined.push_str(&String::from_utf8_lossy(&output.stderr));
        }
        return Ok(combined);
    }

    Err(anyhow::anyhow!("CHECK requires tsc (bun or node) on PATH"))
}

/// Parse tsc's `file(line,col): error TSxxxx: message` lines into
/// DiagnosticEntry values. Lines that do not match the diagnostic shape
/// (blank lines, tsc banners) are skipped.
fn parse_diagnostics_output(output: &str) -> Vec<DiagnosticEntry> {
    let mut entries = Vec::new();

    for line in output.lines() {
        // Location prefix: everything up to the first ": " after "(", and the
        // line/column sit between them.
        let (location, rest) = match line.split_once(": ") {
            Some(parts) => parts,
            None => continue,
        };

        let (file, line_number) = match split_location(location) {
            Some(parts) => parts,
            None => continue,
        };

        // The message is whatever follows the severity/code prefix
        // ("error TS2322: message"); keep the raw text when it doesn't match.
        let message = match rest.split_once(": ") {
            Some((_code, message)) if !_code.is_empty() && !message.is_empty() => {
                message.to_string()
            }
            _ => rest.to_string(),
        };

        entries.push(DiagnosticEntry {
            file: file.to_string(),
            line: line_number,
            message,
        });
    }

    entries
}

/// Split `path(line,col)` into (path, line). Returns None when the text does
/// not have the expected shape.
fn split_location(location: &str) -> Option<(String, i64)> {
    let (path, position) = location.rsplit_once('(')?;
    let position = position.strip_suffix(')')?;
    let line_text = position.split(',').next()?;
    let line: i64 = line_text.parse().ok()?;
    Some((path.to_string(), line))
}

/// Make a diagnostic path relative to the workspace root when possible — the
/// display path formatDiagnostic produces.
fn display_file_path(file: &str, workspace_root: &str) -> String {
    if file.starts_with(workspace_root) {
        let relative = file
            .strip_prefix(workspace_root)
            .unwrap_or(file)
            .trim_start_matches('/');
        return relative.to_string();
    }
    file.to_string()
}

/// Port of the complete stage.
pub fn complete(prepared: &CheckToolPrepared, result: &CheckToolResult) -> ToolCompletion {
    let lead_line = format!("CHECK: {} error(s) in {}", result.total_errors, result.scope);
    let mut lines: Vec<String> = vec![lead_line.clone()];

    if !result.diagnostics.is_empty() {
        lines.push(String::new());

        for diagnostic in &result.diagnostics {
            lines.push(format!(
                "  {}:{}: {}",
                diagnostic.file, diagnostic.line, diagnostic.message
            ));
        }
    }

    if result.elsewhere_count > 0 {
        lines.push(String::new());
        lines.push(format!(
            "  {} error(s) elsewhere in the project",
            result.elsewhere_count
        ));
    }

    ToolCompletion {
        blocks: vec![ToolCompletionBlock {
            code: String::new(),
            description: lead_line,
            language: "text".to_string(),
            path: prepared.input.workspace_root.clone(),
        }],
        tool_content: lines.join("\n"),
    }
}

/// Whole-pipeline entry point: prepare → execute → complete, mapping errors
/// to the model-facing failure text (buildFailureResult shape).
pub fn execute(args: &Value, ctx: &ToolCtx) -> ToolOutcome {
    let outcome = prepare(args, ctx).and_then(|prepared| {
        let execution = execute_prepared(&prepared)?;
        let _completion = complete(&prepared, &execution.data);
        Ok(execution.output_text)
    });

    match outcome {
        Ok(text) => ToolOutcome::success(text),
        Err(error) => ToolOutcome::error(error),
    }
}

// port of tools/test/check-tool.test.ts (scope filtering and the
// diagnostic-line parser; the language-service invocation itself is not
// exercised here).
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tsc_diagnostic_lines_into_entries() {
        let output = concat!(
            "drip/src/tools/builtin/check.rs(10,5): error TS2322: Type 'string' is not assignable to type 'number'.\n",
            "drip/src/core/state.rs(42,1): error TS2304: Cannot find name 'missing'.\n",
            "\n",
            "Found 2 errors.\n"
        );

        let entries = parse_diagnostics_output(output);

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].file, "drip/src/tools/builtin/check.rs");
        assert_eq!(entries[0].line, 10);
        assert_eq!(
            entries[0].message,
            "Type 'string' is not assignable to type 'number'."
        );
        assert_eq!(entries[1].file, "drip/src/core/state.rs");
        assert_eq!(entries[1].line, 42);
        assert_eq!(entries[1].message, "Cannot find name 'missing'.");
    }

    #[test]
    fn skips_non_diagnostic_lines() {
        let output = "error TS5102: option 'noEmit' required\nnot a diagnostic line\n";

        let entries = parse_diagnostics_output(output);

        // Blank/unmatched lines produce no entries.
        assert_eq!(entries.len(), 0);
    }

    #[test]
    fn formats_relative_display_paths() {
        let file = display_file_path("/repo/drip/src/lib.rs", "/repo");

        assert_eq!(file, "drip/src/lib.rs");
    }

    #[test]
    fn keeps_absolute_paths_outside_the_workspace() {
        let file = display_file_path("/elsewhere/lib.rs", "/repo");

        assert_eq!(file, "/elsewhere/lib.rs");
    }

    /// Port of the focused-mode scope filtering from the TS execute stage:
    /// a diagnostic in the scoped file stays in `diagnostics`, everything
    /// else is counted in elsewhere_count.
    #[test]
    fn focused_mode_filters_by_scoped_file_and_counts_elsewhere() {
        let target = PathBuf::from("/repo/src/main.rs");
        let all = vec![
            DiagnosticEntry {
                file: "/repo/src/main.rs".to_string(),
                line: 10,
                message: "in scope".to_string(),
            },
            DiagnosticEntry {
                file: "/repo/src/other.rs".to_string(),
                line: 42,
                message: "elsewhere".to_string(),
            },
            DiagnosticEntry {
                file: "/repo/src/other.rs".to_string(),
                line: 7,
                message: "also elsewhere".to_string(),
            },
        ];

        let result = scope_result(all, Some(&target), Path::new("/repo"), Path::new("/repo"));

        assert_eq!(result.total_errors, 3);
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].file, "src/main.rs");
        assert_eq!(result.diagnostics[0].line, 10);
        assert_eq!(result.elsewhere_count, 2);
        assert_eq!(result.scope, "src/main.rs");
    }

    /// Project-wide mode keeps everything (up to the cap) and reports no
    /// elsewhere count.
    #[test]
    fn project_mode_keeps_all_and_reports_no_elsewhere() {
        let all = vec![DiagnosticEntry {
            file: "/repo/src/main.rs".to_string(),
            line: 10,
            message: "boom".to_string(),
        }];

        let result = scope_result(all, None, Path::new("/repo"), Path::new("/repo"));

        assert_eq!(result.total_errors, 1);
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].file, "src/main.rs");
        assert_eq!(result.elsewhere_count, 0);
        assert_eq!(result.scope, "project");
    }
}
