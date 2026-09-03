use anyhow::Result;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use super::{tool_arguments, ToolCompletion, ToolCompletionBlock, ToolCtx, ToolOutcome};
use crate::tools::helpers::{
    assert_directory_path, default_ignored_dirs, format_tool_path, get_optional_number_argument,
    resolve_tool_path,
};

// TS type DirToolInput.
pub struct DirToolInput {
    pub absolute_path: PathBuf,
    pub display_path: String,
    pub max_depth: i64,
}

// TS type DirToolResult.
pub struct DirToolResult {
    pub entries: usize,
    pub tree: String,
}

/// What the prepare stage returns: { input, displayInput }.
pub struct DirToolPrepared {
    pub input: DirToolInput,
    pub display_input: String,
}

/// What the execute stage returns: { data, outputText }.
pub struct DirToolExecution {
    pub data: DirToolResult,
    pub output_text: String,
}

// .drip/.solid-state/.local-coding-app are the harness's own artifacts —
// showing them invites the model to read operator state instead of the code.
// (The ignore list itself is shared: see helpers.rs DEFAULT_IGNORED_DIRS,
// which lists the harness dir.)

/// Port of clampDepth: undefined → 4, otherwise floored and clamped to [1, 8].
pub fn clamp_depth(value: Option<f64>) -> i64 {
    match value {
        None => 4,
        Some(value) => value.floor().clamp(1.0, 8.0) as i64,
    }
}

// Big directories used to blow the tool-result budget and get mid-elided,
// leaving the model unable to tell which directories it never saw. Per-dir
// truncation with explicit "(+N more)" markers keeps omissions visible.
const MAX_ENTRIES_PER_DIRECTORY: usize = 60;
const MAX_TOTAL_ENTRIES: usize = 400;

#[derive(Default)]
struct WalkState {
    entries: usize,
    total_skipped: usize,
    budget_exhausted: bool,
}

/// Port of buildTree. The TS version is an async closure over mutable
/// counters; here the recursion passes the shared state explicitly.
pub fn build_tree(path: &Path, max_depth: i64, root_label: &str) -> Result<DirToolResult> {
    let mut lines: Vec<String> = vec![root_label.to_string()];
    let mut state = WalkState::default();

    walk(path, "", 0, max_depth, &mut state, &mut lines)?;

    if state.total_skipped > 0 {
        let skipped = state.total_skipped;
        lines.push(format!(
            "({} entr{} not shown in total)",
            skipped,
            if skipped == 1 { "y" } else { "ies" }
        ));
    }

    Ok(DirToolResult {
        entries: state.entries,
        tree: lines.join("\n"),
    })
}

/// Port of the inner walk() recursion. read_dir matches Node's
/// readdir(withFileTypes): symlinked directories report as symlinks, not
/// directories, on both sides.
fn walk(
    current_path: &Path,
    prefix: &str,
    depth: i64,
    max_depth: i64,
    state: &mut WalkState,
    lines: &mut Vec<String>,
) -> Result<()> {
    if depth >= max_depth || state.budget_exhausted {
        return Ok(());
    }

    let mut visible_entries: Vec<(String, bool)> = Vec::new();
    for entry in std::fs::read_dir(current_path)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if default_ignored_dirs().contains(name.as_str()) {
            continue;
        }
        let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
        visible_entries.push((name, is_dir));
    }

    // Directories sort first, then names by localeCompare (ICU root order:
    // `hello.txt` before `NOTES.md`).
    visible_entries.sort_by(|left, right| {
        right
            .1
            .cmp(&left.1)
            .then_with(|| crate::tools::helpers::locale_compare(&left.0, &right.0))
    });

    let shown_entries = &visible_entries[..visible_entries.len().min(MAX_ENTRIES_PER_DIRECTORY)];
    let hidden_here = visible_entries.len() - shown_entries.len();

    for (index, (name, is_dir)) in shown_entries.iter().enumerate() {
        if state.entries >= MAX_TOTAL_ENTRIES {
            state.budget_exhausted = true;
            let remaining = visible_entries.len() - index;
            state.total_skipped += remaining;
            lines.push(format!(
                "{prefix}└── (+{remaining} more — total entry budget reached; DIR a subdirectory for detail)"
            ));
            return Ok(());
        }

        let is_last = index == shown_entries.len() - 1 && hidden_here == 0;
        let connector = if is_last { "└──" } else { "├──" };
        let next_prefix = format!("{prefix}{}", if is_last { "    " } else { "│   " });

        lines.push(format!("{prefix}{connector} {name}"));
        state.entries += 1;

        if *is_dir {
            walk(
                &current_path.join(name),
                &next_prefix,
                depth + 1,
                max_depth,
                state,
                lines,
            )?;
        }
    }

    if hidden_here > 0 {
        state.total_skipped += hidden_here;
        lines.push(format!(
            "{prefix}└── (+{hidden_here} more in this directory — GREP or DIR it directly for the rest)"
        ));
    }

    Ok(())
}

/// The OpenAI function definition drip sends for this tool (the
/// {type: "function", function: {...}} envelope).
pub fn definition() -> Value {
    json!({
        "type": "function",
        "function": {
            "description": "Show the local project structure for a directory as a tree view.",
            "name": "DIR",
            "parameters": {
                "additionalProperties": false,
                "properties": {
                    "maxDepth": {
                        "description": "Optional maximum directory depth to traverse. Defaults to 4.",
                        "type": "number"
                    },
                    "path": {
                        "description": "Directory path to inspect, relative to the current working directory or absolute. Defaults to the cwd.",
                        "type": "string"
                    }
                },
                "type": "object"
            }
        }
    })
}

/// Port of the prepare stage.
pub fn prepare(args: &Value, ctx: &ToolCtx) -> Result<DirToolPrepared> {
    let args = tool_arguments(args)?;
    let cwd = ctx.cwd.to_string_lossy().to_string();

    let path_value = match args.get("path") {
        Some(Value::String(path)) if !path.trim().is_empty() => path.trim().to_string(),
        _ => cwd.clone(),
    };
    let absolute_path = resolve_tool_path(&cwd, &path_value);
    let display_path = format_tool_path(&cwd, &absolute_path);
    let max_depth = clamp_depth(get_optional_number_argument(&args, "maxDepth")?);

    Ok(DirToolPrepared {
        display_input: format!("{display_path}\ndepth {max_depth}"),
        input: DirToolInput {
            absolute_path,
            display_path,
            max_depth,
        },
    })
}

/// Port of the execute stage (renamed from `execute` because the
/// whole-pipeline entry point below owns that name per the builtin/mod.rs
/// contract).
pub fn execute_prepared(prepared: &DirToolPrepared) -> Result<DirToolExecution> {
    assert_directory_path(
        &prepared.input.absolute_path,
        &prepared.input.display_path,
    )?;
    let tree = build_tree(
        &prepared.input.absolute_path,
        prepared.input.max_depth,
        &prepared.input.display_path,
    )?;

    Ok(DirToolExecution {
        output_text: format!(
            "Listed {} item(s) under {} up to depth {}.",
            tree.entries, prepared.input.display_path, prepared.input.max_depth
        ),
        data: tree,
    })
}

/// Port of the complete stage.
pub fn complete(prepared: &DirToolPrepared, result: &DirToolResult) -> ToolCompletion {
    let input = &prepared.input;
    // The TS ternary is load-bearing: formatting "." would render
    // "Directory tree for .." with a double dot.
    let description = if input.display_path == "." {
        "Directory tree for .".to_string()
    } else {
        format!("Directory tree for {}.", input.display_path)
    };

    ToolCompletion {
        blocks: vec![ToolCompletionBlock {
            code: result.tree.clone(),
            description,
            language: "text".to_string(),
            path: input.absolute_path.clone(),
        }],
        tool_content: format!(
            "Directory tree for {}.\n\n{}",
            input.display_path, result.tree
        ),
    }
}

/// The transcript's display string for this call — what the TS tool's prepare
/// returns as `displayInput` — or None when the arguments do not parse (the
/// execute path reports that error).
pub fn display_input(args: &Value, ctx: &ToolCtx) -> Option<String> {
    prepare(args, ctx).ok().map(|prepared| prepared.display_input)
}

/// Whole-pipeline entry point: prepare → execute → complete, mapping errors
/// to the model-facing failure text (buildFailureResult shape).
pub fn execute(args: &Value, ctx: &ToolCtx) -> ToolOutcome {
    let outcome = prepare(args, ctx).and_then(|prepared| {
        let execution = execute_prepared(&prepared)?;
        let completion = complete(&prepared, &execution.data);
        Ok(completion.tool_content)
    });

    match outcome {
        Ok(text) => ToolOutcome::success(text),
        Err(error) => ToolOutcome::error(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// createTempDir from tools/test/test-helpers.ts (mkdtemp in the system
    /// tmpdir). The TempDir guard is returned so the dir stays alive for the
    /// test body like cleanupTempDirs' afterEach would.
    fn create_temp_dir(prefix: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::Builder::new()
            .prefix(prefix)
            .tempdir()
            .expect("tempdir");
        let path = dir.path().to_path_buf();
        (dir, path)
    }

    fn stage_context(cwd: &Path) -> ToolCtx {
        ToolCtx {
            cwd: cwd.to_path_buf(),
            allow_net: false,
        }
    }

    // it("builds a directory tree completion")
    #[test]
    fn builds_a_directory_tree_completion() {
        let (_temp, cwd) = create_temp_dir("dir-tool-");

        std::fs::create_dir(cwd.join("src")).unwrap();
        std::fs::write(cwd.join("src").join("index.ts"), "export {};\n").unwrap();
        std::fs::write(cwd.join("README.md"), "# Demo\n").unwrap();

        let ctx = stage_context(&cwd);
        let prepared = prepare(&json!({"maxDepth": 2, "path": "."}), &ctx).unwrap();
        let result = execute_prepared(&prepared).unwrap();
        let completion = complete(&prepared, &result.data);

        assert!(result.output_text.contains("Listed"));
        // toMatchObject({description, type: "completion"}) — the
        // ToolCompletionBlock type is the "completion" block by construction.
        assert_eq!(completion.blocks[0].description, "Directory tree for .");
        assert!(completion.blocks[0].code.contains("src"));
        assert!(completion.blocks[0].code.contains("index.ts"));
    }

    // it("accepts numeric-string depth and cwd aliases")
    #[test]
    fn accepts_numeric_string_depth_and_cwd_aliases() {
        let (_temp, cwd) = create_temp_dir("dir-tool-alias-");

        std::fs::create_dir(cwd.join("src")).unwrap();
        std::fs::write(cwd.join("src").join("index.ts"), "export {};\n").unwrap();

        let ctx = stage_context(&cwd);
        let prepared = prepare(
            &json!({"maxDepth": "2", "path": "current working directory"}),
            &ctx,
        )
        .unwrap();
        let result = execute_prepared(&prepared).unwrap();

        assert_eq!(prepared.input.display_path, ".");
        assert_eq!(prepared.input.max_depth, 2);
        assert!(result.output_text.contains("up to depth 2"));
    }

    // it("truncates oversized directories with visible (+N more) markers")
    #[test]
    fn truncates_oversized_directories_with_visible_more_markers() {
        let (_temp, cwd) = create_temp_dir("dir-cap-");

        for index in 0..75 {
            std::fs::write(cwd.join(format!("file-{:03}.txt", index)), "x").unwrap();
        }

        std::fs::create_dir_all(cwd.join("aaa-sub")).unwrap();
        std::fs::write(cwd.join("aaa-sub").join("inner.txt"), "x").unwrap();

        let ctx = stage_context(&cwd);
        let prepared = prepare(&json!({"path": "."}), &ctx).unwrap();
        let result = execute_prepared(&prepared).unwrap();
        let completion = complete(&prepared, &result.data);

        assert!(completion
            .tool_content
            .contains("(+16 more in this directory"));
        assert!(completion
            .tool_content
            .contains("entries not shown in total"));
        // The subdirectory still renders (directories sort first).
        assert!(completion.tool_content.contains("aaa-sub"));
        assert!(completion.tool_content.contains("inner.txt"));
    }
}
