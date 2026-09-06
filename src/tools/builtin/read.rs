use anyhow::Result;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use super::dir::build_tree;
use super::{tool_arguments, ToolCompletion, ToolCompletionBlock, ToolCtx, ToolOutcome};
use crate::tools::helpers::{
    assert_readable_file_path, format_tool_path, get_optional_number_argument,
    get_required_string_argument, is_binary_buffer, resolve_tool_path,
};

const DEFAULT_LIMIT: i64 = 400;
const MAX_LIMIT: i64 = 1000;
const SIZE_LIMIT_BYTES: u64 = 5 * 1024 * 1024; // 5 MB
const LINE_CLAMP_CHARS: usize = 2000;

// Parsed arguments for one read call.
pub struct ReadToolInput {
    pub absolute_path: PathBuf,
    pub display_path: String,
    pub force: bool,
    pub limit: i64,
    pub offset: i64,
}

// Result of one read call (is_directory is Some only for directory reads).
#[derive(Debug)]
pub struct ReadToolResult {
    pub absolute_path: PathBuf,
    pub end_line: i64,
    pub is_directory: Option<bool>,
    pub start_line: i64,
    pub text: String,
    pub total_lines: i64,
}

/// What the prepare stage returns: { input, displayInput }.
pub struct ReadToolPrepared {
    pub input: ReadToolInput,
    pub display_input: String,
}

/// What the execute stage returns: { data, outputText }.
#[derive(Debug)]
pub struct ReadToolExecution {
    pub data: ReadToolResult,
    pub output_text: String,
}

/// Maps a file extension to its language key, or None for unknown types.
fn infer_language(path: &Path) -> Option<String> {
    let extension = path.extension()?.to_str()?.to_lowercase();

    match extension.as_str() {
        "jsx" => Some("jsx"),
        "js" | "mjs" | "cjs" => Some("js"),
        "json" => Some("json"),
        "md" => Some("md"),
        "tsx" => Some("tsx"),
        "ts" => Some("ts"),
        _ => None,
    }
    .map(str::to_string)
}

/// JS string length: UTF-16 code units, not chars or bytes. The line clamp
/// compares `line.length > LINE_CLAMP_CHARS` and interpolates that same
/// count into the truncation suffix.
fn utf16_len(line: &str) -> usize {
    line.chars().map(char::len_utf16).sum()
}

/// JS slice(0, n) over UTF-16 code units. A JS cut can land inside a
/// surrogate pair (leaving a lone surrogate, which renders as U+FFFD);
/// Rust strings cannot hold one, so the cut backs off to the whole
/// character — the visible content is identical.
fn take_utf16(line: &str, units: usize) -> &str {
    let mut count = 0usize;
    for (index, ch) in line.char_indices() {
        if count + ch.len_utf16() > units {
            return &line[..index];
        }
        count += ch.len_utf16();
    }
    line
}

/// The OpenAI function definition drip sends for this tool (the
/// {type: "function", function: {...}} envelope).
pub fn definition() -> Value {
    json!({
        "type": "function",
        "function": {
            "description": "Read the contents of a single file from the local workspace. Supports line-based paging: use offset (1-based first line, default 1) and limit (max lines to return, default 400, cap 1000). Every line is prefixed with its 1-based line number and a tab. The result starts with a header 'Read lines A-B of N from <path>' and, when more lines remain, ends with a footer telling you the next offset to use.",
            "name": "READ",
            "parameters": {
                "additionalProperties": false,
                "properties": {
                    "force": {
                        "description": "Pass true to bypass the 5 MB size limit and read oversized files. Does NOT override binary file refusal.",
                        "type": "boolean"
                    },
                    "limit": {
                        "description": "Maximum number of lines to return. Default is 400, capped at 1000.",
                        "type": "number"
                    },
                    "offset": {
                        "description": "1-based line number to start reading from. Default is 1 (beginning of file).",
                        "type": "number"
                    },
                    "path": {
                        "description": "Path to the file to read, relative to the current working directory or absolute.",
                        "type": "string"
                    }
                },
                "required": ["path"],
                "type": "object"
            }
        }
    })
}

/// The prepare stage.
pub fn prepare(args: &Value, ctx: &ToolCtx) -> Result<ReadToolPrepared> {
    let args = tool_arguments(args)?;
    let cwd = ctx.cwd.to_string_lossy().to_string();

    let raw_path = get_required_string_argument(&args, "path")?;
    let absolute_path = resolve_tool_path(&cwd, &raw_path);

    let raw_offset = get_optional_number_argument(&args, "offset")?.unwrap_or(1.0);
    let raw_limit = get_optional_number_argument(&args, "limit")?.unwrap_or(DEFAULT_LIMIT as f64);

    let offset = raw_offset.floor().max(1.0) as i64;
    let limit = raw_limit.floor().max(1.0).min(MAX_LIMIT as f64) as i64;

    let force = matches!(args.get("force"), Some(Value::Bool(true)));

    let display_input = format_tool_path(&cwd, &absolute_path);
    Ok(ReadToolPrepared {
        display_input: display_input.clone(),
        input: ReadToolInput {
            absolute_path,
            display_path: display_input,
            force,
            limit,
            offset,
        },
    })
}

/// The execute stage (named `execute_prepared`: the whole-pipeline entry
/// point below owns `execute` per the builtin/mod.rs contract).
pub fn execute_prepared(prepared: &ReadToolPrepared) -> Result<ReadToolExecution> {
    let input = &prepared.input;

    // Directory intercept: before assertReadableFilePath (which throws for dirs),
    // check if the path is a directory and return a listing instead.
    let path_stat = std::fs::metadata(&input.absolute_path).ok();

    if path_stat.as_ref().is_some_and(|stat| stat.is_dir()) {
        let tree_result = build_tree(&input.absolute_path, 2, &input.display_path)?;
        let text = format!(
            "\"{}\" is a directory — listing it instead (READ is for files; use DIR for deeper trees):\n{}",
            input.display_path, tree_result.tree
        );
        return Ok(ReadToolExecution {
            data: ReadToolResult {
                absolute_path: input.absolute_path.clone(),
                end_line: 1,
                is_directory: Some(true),
                start_line: 1,
                text: text.clone(),
                total_lines: 1,
            },
            output_text: text,
        });
    }

    assert_readable_file_path(&input.absolute_path, &input.display_path)?;

    // Size guard: stat before reading to avoid loading huge files
    let file_size_bytes = match path_stat.as_ref() {
        Some(stat) => stat.len(),
        None => std::fs::metadata(&input.absolute_path)?.len(),
    };
    if file_size_bytes > SIZE_LIMIT_BYTES && !input.force {
        let size_mb = file_size_bytes as f64 / (1024.0 * 1024.0);
        return Err(anyhow::anyhow!(
            "\"{}\" is {:.1} MB — too large to read directly (limit: 5 MB). Use GREP to locate specific line ranges, or BASH with head/tail to read portions. Pass force: true to override this limit.",
            input.display_path,
            size_mb
        ));
    }

    // Read the raw bytes first for binary detection
    let raw_buffer = std::fs::read(&input.absolute_path)?;

    // Binary detection: refuse binary files even with force: true
    if is_binary_buffer(&raw_buffer) {
        return Err(anyhow::anyhow!(
            "\"{}\" appears to be a binary file (NUL bytes detected). READ only supports text files.",
            input.display_path
        ));
    }

    let raw = String::from_utf8_lossy(&raw_buffer);

    // Split into lines, preserving a trailing newline as not adding an extra blank line
    let all_lines: Vec<&str> = match raw.strip_suffix('\n') {
        Some(stripped) => stripped.split('\n').collect(),
        None => raw.split('\n').collect(),
    };
    let total_lines = all_lines.len() as i64;

    // Validate offset
    if input.offset > total_lines {
        return Err(anyhow::anyhow!(
            "Offset {} is past the end of \"{}\" ({} line{}).",
            input.offset,
            input.display_path,
            total_lines,
            if total_lines == 1 { "" } else { "s" }
        ));
    }

    // Compute slice (offset is 1-based)
    let start_index = (input.offset - 1) as usize; // inclusive, 0-based
    let end_index = (start_index as i64 + input.limit).min(total_lines) as usize; // exclusive, 0-based

    let start_line = input.offset; // 1-based
    let end_line = end_index as i64; // 1-based (last line read)

    let selected_lines = &all_lines[start_index..end_index];

    // Line clamp: truncate lines longer than LINE_CLAMP_CHARS
    let clamped_lines: Vec<String> = selected_lines
        .iter()
        .map(|line| {
            if utf16_len(line) > LINE_CLAMP_CHARS {
                format!(
                    "{}[line truncated: {} chars total]",
                    take_utf16(line, LINE_CLAMP_CHARS),
                    utf16_len(line)
                )
            } else {
                (*line).to_string()
            }
        })
        .collect();

    let numbered_lines: Vec<String> = clamped_lines
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{}\t{}", start_line + i as i64, line))
        .collect();
    let text = numbered_lines.join("\n");

    Ok(ReadToolExecution {
        data: ReadToolResult {
            absolute_path: input.absolute_path.clone(),
            end_line,
            // File reads leave is_directory unset.
            is_directory: None,
            start_line,
            text,
            total_lines,
        },
        output_text: format!(
            "Loaded lines {}-{} of {} from {}.",
            start_line, end_line, total_lines, input.display_path
        ),
    })
}

/// The complete stage.
pub fn complete(prepared: &ReadToolPrepared, result: &ReadToolResult) -> ToolCompletion {
    let display_path = &prepared.input.display_path;

    if result.is_directory == Some(true) {
        return ToolCompletion {
            blocks: vec![],
            tool_content: result.text.clone(),
        };
    }

    let header = format!(
        "Read lines {}-{} of {} from {}.",
        result.start_line, result.end_line, result.total_lines, display_path
    );
    let footer = if result.end_line < result.total_lines {
        Some(format!(
            "File continues — call READ again with offset {} to keep reading",
            result.end_line + 1
        ))
    } else {
        None
    };

    let mut tool_content_parts = vec![header.clone(), String::new(), result.text.clone()];
    if let Some(footer) = footer {
        tool_content_parts.push(footer);
    }

    ToolCompletion {
        blocks: vec![ToolCompletionBlock {
            code: result.text.clone(),
            description: header,
            // Unknown extensions map to "" — no language key is emitted for them.
            language: infer_language(&result.absolute_path).unwrap_or_default(),
            path: result.absolute_path.clone(),
        }],
        tool_content: tool_content_parts.join("\n"),
    }
}

/// The transcript's display string for this call — or None when the
/// arguments do not parse (the execute path reports that error).
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

    /// A temp directory under the system tmpdir. The TempDir guard is returned
    /// so the dir stays alive for the test body and is removed on drop.
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
            reference_roots: Vec::new(),
        }
    }

    #[test]
    fn reads_file_contents_and_returns_a_completion_block_with_header_and_line_numbers() {
        let (_temp, cwd) = create_temp_dir("read-tool-");
        let file_path = cwd.join("scratch.ts");

        std::fs::write(&file_path, "export const ready = true;\n").unwrap();

        let ctx = stage_context(&cwd);
        let prepared = prepare(&json!({"path": "scratch.ts"}), &ctx).unwrap();
        let result = execute_prepared(&prepared).unwrap();
        let completion = complete(&prepared, &result.data);

        // outputText reflects new format
        assert!(result.output_text.contains("Loaded lines 1-1 of 1"));

        // The block code uses numbered lines
        assert_eq!(completion.blocks[0].code, "1\texport const ready = true;");
        assert_eq!(completion.blocks[0].path, file_path);

        // toolContent starts with header
        assert!(completion
            .tool_content
            .starts_with("Read lines 1-1 of 1 from scratch.ts."));

        // No footer when file is fully read
        assert!(!completion.tool_content.contains("File continues"));
    }

    #[test]
    fn returns_a_directory_listing_not_an_error_when_pointed_at_a_directory() {
        let (_temp, cwd) = create_temp_dir("read-tool-dir-");
        // Create a file inside so the tree has something to show
        std::fs::write(cwd.join("hello.ts"), "export {};\n").unwrap();

        let ctx = stage_context(&cwd);
        let prepared = prepare(&json!({"path": "."}), &ctx).unwrap();

        // execute() must NOT throw
        let result = execute_prepared(&prepared).unwrap();

        // Result text starts with the directory preamble
        assert!(result
            .output_text
            .contains("is a directory — listing it instead"));
        assert!(result
            .output_text
            .contains("READ is for files; use DIR for deeper trees"));
        // The tree output should mention the file we created
        assert!(result.output_text.contains("hello.ts"));

        // complete() returns a successful toolContent (not a throw)
        let completion = complete(&prepared, &result.data);
        assert!(completion
            .tool_content
            .contains("is a directory — listing it instead"));
        // No "Read lines" header for directory case
        assert!(!completion.tool_content.contains("Read lines"));
    }

    #[test]
    fn still_errors_for_a_missing_non_existent_path() {
        let (_temp, cwd) = create_temp_dir("read-tool-missing-");

        let ctx = stage_context(&cwd);
        let prepared = prepare(&json!({"path": "does-not-exist.ts"}), &ctx).unwrap();

        let error = execute_prepared(&prepared).unwrap_err();
        assert!(error.to_string().contains("does not exist"));
    }

    #[test]
    fn uses_default_offset_1_and_default_limit_when_not_provided() {
        let (_temp, cwd) = create_temp_dir("read-tool-defaults-");
        let file_path = cwd.join("file.ts");
        let lines = (1..=5)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&file_path, lines).unwrap();

        let ctx = stage_context(&cwd);
        let prepared = prepare(&json!({"path": "file.ts"}), &ctx).unwrap();
        let result = execute_prepared(&prepared).unwrap();

        assert_eq!(result.data.start_line, 1);
        assert_eq!(result.data.end_line, 5);
        assert_eq!(result.data.total_lines, 5);
        // All lines numbered
        assert!(result.data.text.contains("1\tline1"));
        assert!(result.data.text.contains("5\tline5"));
    }

    #[test]
    fn pages_with_offset_and_limit_returns_correct_numbered_lines() {
        let (_temp, cwd) = create_temp_dir("read-tool-paging-");
        let file_path = cwd.join("big.ts");
        let lines = (1..=10)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&file_path, lines).unwrap();

        let ctx = stage_context(&cwd);
        let prepared = prepare(&json!({"path": "big.ts", "offset": 4, "limit": 3}), &ctx).unwrap();
        let result = execute_prepared(&prepared).unwrap();

        assert_eq!(result.data.start_line, 4);
        assert_eq!(result.data.end_line, 6);
        assert_eq!(result.data.total_lines, 10);
        assert_eq!(result.data.text, "4\tline4\n5\tline5\n6\tline6");
    }

    #[test]
    fn includes_a_continue_footer_when_more_lines_remain() {
        let (_temp, cwd) = create_temp_dir("read-tool-footer-");
        let file_path = cwd.join("big.ts");
        let lines = (1..=10)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&file_path, lines).unwrap();

        let ctx = stage_context(&cwd);
        let prepared = prepare(&json!({"path": "big.ts", "offset": 1, "limit": 5}), &ctx).unwrap();
        let result = execute_prepared(&prepared).unwrap();
        let completion = complete(&prepared, &result.data);

        // Header present
        assert!(completion
            .tool_content
            .starts_with("Read lines 1-5 of 10 from big.ts."));
        // Footer present with exact next offset
        assert!(completion
            .tool_content
            .contains("File continues — call READ again with offset 6 to keep reading"));
    }

    #[test]
    fn does_not_include_a_footer_when_all_lines_are_read() {
        let (_temp, cwd) = create_temp_dir("read-tool-no-footer-");
        let file_path = cwd.join("small.ts");
        std::fs::write(&file_path, "a\nb\nc\n").unwrap();

        let ctx = stage_context(&cwd);
        let prepared = prepare(&json!({"path": "small.ts"}), &ctx).unwrap();
        let result = execute_prepared(&prepared).unwrap();
        let completion = complete(&prepared, &result.data);

        assert!(completion
            .tool_content
            .starts_with("Read lines 1-3 of 3 from small.ts."));
        assert!(!completion.tool_content.contains("File continues"));
    }

    #[test]
    fn errors_when_offset_is_past_end_of_file_naming_the_actual_line_count() {
        let (_temp, cwd) = create_temp_dir("read-tool-offset-error-");
        let file_path = cwd.join("short.ts");
        std::fs::write(&file_path, "only one line\n").unwrap();

        let ctx = stage_context(&cwd);
        let prepared = prepare(&json!({"path": "short.ts", "offset": 99}), &ctx).unwrap();

        // /Offset 99 is past the end.*1 line/
        let error = execute_prepared(&prepared).unwrap_err().to_string();
        assert!(error.contains("Offset 99 is past the end"));
        assert!(error.contains("1 line"));
    }

    #[test]
    fn fails_with_a_helpful_message_when_the_file_does_not_exist() {
        let (_temp, cwd) = create_temp_dir("read-tool-missing-");

        let ctx = stage_context(&cwd);
        let prepared = prepare(&json!({"path": "nonexistent.ts"}), &ctx).unwrap();

        let error = execute_prepared(&prepared).unwrap_err();
        assert!(error.to_string().contains("does not exist"));
    }

    //
    #[test]
    fn refuses_files_larger_than_5_mb_with_a_helpful_error() {
        // Write a file that is just over 5 MB
        let (_temp, cwd) = create_temp_dir("read-tool-large-");
        let file_path = cwd.join("large.txt");
        // 5 MB + 1 byte — fill with 'A' (non-binary) so binary detection doesn't fire
        let size_bytes = 5 * 1024 * 1024 + 1;
        std::fs::write(&file_path, vec![b'A'; size_bytes]).unwrap();

        let ctx = stage_context(&cwd);
        let prepared = prepare(&json!({"path": "large.txt"}), &ctx).unwrap();

        // Four facets asserted against one rejection (the tool is pure, so the
        // message is identical every call).
        let error = execute_prepared(&prepared).unwrap_err().to_string();
        assert!(error.contains("too large to read directly"));
        // Error should mention the size in MB
        assert!(error.contains("MB"));
        // Error should suggest GREP
        assert!(error.contains("GREP"));
        // Error should suggest head/tail or BASH
        assert!(error.contains("head/tail") || error.contains("BASH"));
    }

    #[test]
    fn allows_files_larger_than_5_mb_when_force_true_is_passed() {
        let (_temp, cwd) = create_temp_dir("read-tool-large-force-");
        let file_path = cwd.join("large.txt");
        let size_bytes = 5 * 1024 * 1024 + 1;
        // Fill with 'A' chars (non-binary) so binary detection doesn't fire
        std::fs::write(&file_path, vec![b'A'; size_bytes]).unwrap();

        let ctx = stage_context(&cwd);
        let prepared = prepare(&json!({"path": "large.txt", "force": true}), &ctx).unwrap();

        // Should NOT throw — result should succeed
        let result = execute_prepared(&prepared).unwrap();
        assert!(result.data.total_lines >= 1);
    }

    //
    #[test]
    fn refuses_binary_files_even_when_force_true_is_passed() {
        let (_temp, cwd) = create_temp_dir("read-tool-binary-");
        let file_path = cwd.join("image.bin");
        // Write a buffer containing NUL bytes — unmistakably binary
        std::fs::write(&file_path, [0x89, 0x50, 0x4e, 0x47, 0x00, 0x00, 0x00, 0x00, 0x41, 0x42])
            .unwrap();

        // Without force
        let ctx = stage_context(&cwd);
        let prepared = prepare(&json!({"path": "image.bin"}), &ctx).unwrap();
        let error = execute_prepared(&prepared).unwrap_err().to_string();
        assert!(error.to_lowercase().contains("binary"));

        // With force: true — binary refusal is NOT overridden
        let prepared_force = prepare(&json!({"path": "image.bin", "force": true}), &ctx).unwrap();
        let error = execute_prepared(&prepared_force).unwrap_err().to_string();
        assert!(error.to_lowercase().contains("binary"));
    }

    //
    #[test]
    fn truncates_lines_longer_than_2000_chars_with_a_suffix() {
        let (_temp, cwd) = create_temp_dir("read-tool-clamp-");
        let file_path = cwd.join("minified.js");
        // Write a single line of 10 000 characters
        let long_line = "x".repeat(10_000);
        std::fs::write(&file_path, &long_line).unwrap();

        let ctx = stage_context(&cwd);
        let prepared = prepare(&json!({"path": "minified.js"}), &ctx).unwrap();
        let result = execute_prepared(&prepared).unwrap();

        // The text should contain the truncation suffix mentioning 10000 chars
        assert!(result.data.text.contains("[line truncated: 10000 chars total]"));
        // The actual content before suffix should be clamped to 2000 chars
        // Format: "1\t<2000 x-chars>[line truncated: 10000 chars total]"
        let line_content = result.data.text.strip_prefix("1\t").unwrap();
        assert!(line_content.starts_with(&"x".repeat(2000)));
        // And definitely not the full 10000 x's
        assert_ne!(line_content, "x".repeat(10_000));
    }

    //
    #[test]
    fn reads_a_normal_text_file_without_modification() {
        let (_temp, cwd) = create_temp_dir("read-tool-normal-");
        let file_path = cwd.join("normal.ts");
        let content = "const x = 1;\nconst y = 2;\n";
        std::fs::write(&file_path, content).unwrap();

        let ctx = stage_context(&cwd);
        let prepared = prepare(&json!({"path": "normal.ts"}), &ctx).unwrap();
        let result = execute_prepared(&prepared).unwrap();

        assert_eq!(result.data.total_lines, 2);
        assert_eq!(result.data.text, "1\tconst x = 1;\n2\tconst y = 2;");
        assert!(!result.data.text.contains("[line truncated"));
    }
}
