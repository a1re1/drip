use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use regex::Regex;
use serde_json::{json, Value};

use crate::tools::helpers::{
    default_ignored_dirs, format_tool_path, get_optional_number_argument,
    is_binary_buffer, resolve_tool_path,
};

use super::{ToolCtx, ToolOutcome};

const MAX_FILE_SIZE: u64 = 1024 * 1024; // 1MB cap for scanned files
const BINARY_PROBE_SIZE: usize = 1024; // 1KB

// ── internal types ────────────────────────────────────────────────────────────

#[derive(Debug)]
struct GrepToolInput {
    absolute_path: PathBuf,
    context: usize,
    cwd: String,
    display_path: String,
    flags: String,
    glob: Option<String>,
    literal: bool,
    max_results: usize,
    pattern: String,
}

struct GrepMatch {
    line: usize, // 1-based
    path: PathBuf,
    text: String,
}

struct GrepToolResult {
    context: usize,
    file_count: usize,
    matches: Vec<GrepMatch>,
    pattern: String, // regex source
    total_matches: usize,
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Only the last segment of the glob pattern is used against the basename
/// (the walk already recurses into dirs).
fn matches_glob(filename: &str, glob: &str) -> bool {
    let last_segment = glob.split('/').last().unwrap_or(glob);
    // Escape regex metacharacters except * and ?, then translate those two
    let mut pattern = String::from("^");
    for ch in last_segment.chars() {
        match ch {
            '.' | '+' | '^' | '$' | '{' | '}' | '(' | ')' | '|' | '[' | ']' | '\\' => {
                pattern.push('\\');
                pattern.push(ch);
            }
            '*' => pattern.push_str(".*"),
            '?' => pattern.push('.'),
            c => pattern.push(c),
        }
    }
    pattern.push('$');
    Regex::new(&pattern)
        .map(|re| re.is_match(filename))
        .unwrap_or_else(|_| filename == glob)
}

/// Escape regex metacharacters so the pattern matches literally.
fn escape_regex(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for ch in s.chars() {
        if matches!(
            ch,
            '.' | '*' | '+' | '?' | '^' | '$' | '{' | '}' | '(' | ')' | '|' | '[' | ']' | '\\'
        ) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Render matches with context windows, `>` markers and `--` separators.
fn render_with_context(lines: &[&str], match_indices: &[usize], context_lines: usize) -> Vec<String> {
    if match_indices.is_empty() {
        return vec![];
    }

    // Build groups of consecutive+overlapping match windows
    struct Group {
        start: usize,
        end: usize,
        match_set: HashSet<usize>,
    }

    let mut groups: Vec<Group> = vec![];

    for &idx in match_indices {
        let win_start = idx.saturating_sub(context_lines);
        let win_end = (idx + context_lines).min(lines.len().saturating_sub(1));

        if let Some(last) = groups.last_mut() {
            if win_start <= last.end + 1 {
                last.end = last.end.max(win_end);
                last.match_set.insert(idx);
                continue;
            }
        }
        let mut ms = HashSet::new();
        ms.insert(idx);
        groups.push(Group { start: win_start, end: win_end, match_set: ms });
    }

    let mut rendered = vec![];

    for (g, group) in groups.iter().enumerate() {
        if g > 0 {
            rendered.push("--".to_string());
        }
        for i in group.start..=group.end {
            let line_num = i + 1;
            let marker = if group.match_set.contains(&i) { ">" } else { " " };
            rendered.push(format!("{} {}: {}", marker, line_num, lines[i]));
        }
    }

    rendered
}

// ── prepare ───────────────────────────────────────────────────────────────────

/// Validates args, resolves paths, and returns the prepared GrepToolInput.
fn prepare(args: &serde_json::Map<String, Value>, ctx: &ToolCtx) -> Result<GrepToolInput> {
    // Validate required pattern
    let pattern = match args.get("pattern") {
        Some(Value::String(s)) if !s.trim().is_empty() => s.trim().to_string(),
        _ => return Err(anyhow!("Missing required string argument \"pattern\".")),
    };

    // Validate flags
    let flags = match args.get("flags") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => {
            for ch in s.chars() {
                if ch != 'i' && ch != 'm' && ch != 's' {
                    return Err(anyhow!(
                        "Invalid flag \"{ch}\". \"flags\" may only contain the characters \"i\", \"m\", and \"s\"."
                    ));
                }
            }
            s.clone()
        }
        _ => {
            return Err(anyhow!(
                "\"flags\" must be a string containing only the characters \"i\", \"m\", and \"s\"."
            ))
        }
    };

    // Validate literal
    let literal = match args.get("literal") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        _ => return Err(anyhow!("\"literal\" must be a boolean.")),
    };

    // Validate regex early (only when not literal)
    if !literal {
        build_regex(&pattern, &flags)
            .map_err(|e| anyhow!("Invalid regex pattern: {e}"))?;
    }

    // Path
    let raw_path = match args.get("path") {
        Some(Value::String(s)) if !s.trim().is_empty() => s.trim().to_string(),
        _ => ".".to_string(),
    };
    let absolute_path = resolve_tool_path(&ctx.cwd.to_string_lossy(), &raw_path);
    let display_path = format_tool_path(&ctx.cwd.to_string_lossy(), &absolute_path);

    // Glob
    let glob = match args.get("glob") {
        Some(Value::String(s)) if !s.trim().is_empty() => Some(s.trim().to_string()),
        _ => None,
    };

    let max_results = clamp_max_results(get_optional_number_argument(args, "maxResults")?);
    let context_lines = clamp_context(get_optional_number_argument(args, "context")?);

    Ok(GrepToolInput {
        absolute_path,
        context: context_lines,
        cwd: ctx.cwd.to_string_lossy().to_string(),
        display_path,
        flags,
        glob,
        literal,
        max_results,
        pattern,
    })
}

// ── regex builder ─────────────────────────────────────────────────────────────

/// Build a Regex from pattern+flags. Flags are the characters i/m/s, applied
/// as a regex crate inline (?flags) prefix.
fn build_regex(pattern: &str, flags: &str) -> Result<Regex> {
    let prefix: String = if flags.is_empty() {
        String::new()
    } else {
        format!("(?{})", flags)
    };
    let full = format!("{prefix}{pattern}");
    Regex::new(&full).map_err(|e| anyhow!("{e}"))
}

// ── execute (grep files) ──────────────────────────────────────────────────────

fn clamp_max_results(v: Option<f64>) -> usize {
    match v {
        None => 50,
        Some(f) => (f.floor() as i64).max(1).min(200) as usize,
    }
}

fn clamp_context(v: Option<f64>) -> usize {
    match v {
        None => 0,
        Some(f) => (f.floor() as i64).max(0).min(5) as usize,
    }
}

/// Grep a single file, or walk a directory tree.
fn grep_files(
    root_path: &Path,
    regex: &Regex,
    glob: Option<&str>,
    max_results: usize,
    context_lines: usize,
) -> GrepToolResult {
    let mut matches: Vec<GrepMatch> = vec![];
    let mut matched_files: HashSet<PathBuf> = HashSet::new();
    let mut total_matches: usize = 0;

    let root_stat = match fs::metadata(root_path) {
        Ok(m) => m,
        Err(_) => {
            return GrepToolResult {
                context: context_lines,
                file_count: 0,
                matches: vec![],
                pattern: regex.to_string(),
                total_matches: 0,
            };
        }
    };

    if root_stat.is_file() {
        if root_stat.len() <= MAX_FILE_SIZE {
            if let Ok(buf) = fs::read(root_path) {
                if !is_binary_buffer(&buf) {
                    let text = String::from_utf8_lossy(&buf).into_owned();
                    let lines: Vec<&str> = text.split('\n').collect();
                    for (i, line) in lines.iter().enumerate() {
                        if total_matches >= max_results {
                            break;
                        }
                        if regex.is_match(line) {
                            let match_text = if context_lines == 0 {
                                line.trim().chars().take(200).collect()
                            } else {
                                line.to_string()
                            };
                            matches.push(GrepMatch {
                                line: i + 1,
                                path: root_path.to_path_buf(),
                                text: match_text,
                            });
                            matched_files.insert(root_path.to_path_buf());
                            total_matches += 1;
                        }
                    }
                }
            }
        }
    } else {
        walk(
            root_path,
            regex,
            glob,
            max_results,
            context_lines,
            default_ignored_dirs(),
            &mut matches,
            &mut matched_files,
            &mut total_matches,
        );
    }

    GrepToolResult {
        context: context_lines,
        file_count: matched_files.len(),
        matches,
        pattern: regex.to_string(),
        total_matches,
    }
}

/// Recursive directory walker. Entries sort by locale order.
#[allow(clippy::too_many_arguments)]
fn walk(
    current_path: &Path,
    regex: &Regex,
    glob: Option<&str>,
    max_results: usize,
    context_lines: usize,
    ignored: &HashSet<&'static str>,
    matches: &mut Vec<GrepMatch>,
    matched_files: &mut HashSet<PathBuf>,
    total_matches: &mut usize,
) {
    if *total_matches >= max_results {
        return;
    }

    let read_dir = match fs::read_dir(current_path) {
        Ok(rd) => rd,
        Err(_) => return,
    };

    let mut entries: Vec<_> = read_dir.flatten().collect();
    entries.sort_by(|a, b| {
        crate::tools::helpers::locale_compare(&a.file_name().to_string_lossy(), &b.file_name().to_string_lossy())
    });

    for entry in entries {
        if *total_matches >= max_results {
            break;
        }

        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if ignored.contains(name_str.as_ref()) {
            continue;
        }

        let entry_path = entry.path();
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };

        if file_type.is_dir() {
            walk(
                &entry_path,
                regex,
                glob,
                max_results,
                context_lines,
                ignored,
                matches,
                matched_files,
                total_matches,
            );
            continue;
        }

        if !file_type.is_file() {
            continue;
        }

        // Glob filter
        if let Some(g) = glob {
            if !matches_glob(&name_str, g) {
                continue;
            }
        }

        // Size check
        let file_size = match fs::metadata(&entry_path) {
            Ok(m) => m.len(),
            Err(_) => continue,
        };
        if file_size > MAX_FILE_SIZE {
            continue;
        }

        // Read and binary check
        let buf = match fs::read(&entry_path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        if is_binary_buffer(&buf) {
            continue;
        }

        let text = String::from_utf8_lossy(&buf).into_owned();
        let lines: Vec<&str> = text.split('\n').collect();

        for (i, line) in lines.iter().enumerate() {
            if *total_matches >= max_results {
                break;
            }
            if regex.is_match(line) {
                let match_text = if context_lines == 0 {
                    line.trim().chars().take(200).collect()
                } else {
                    line.to_string()
                };
                matches.push(GrepMatch {
                    line: i + 1,
                    path: entry_path.clone(),
                    text: match_text,
                });
                matched_files.insert(entry_path.clone());
                *total_matches += 1;
            }
        }
    }
}

// ── complete ──────────────────────────────────────────────────────────────────

/// Format the final tool content string.
fn complete_output(input: &GrepToolInput, result: &GrepToolResult) -> String {
    let GrepToolResult { matches, pattern, total_matches, file_count, context: context_lines } = result;
    let showing = matches.len();

    if *total_matches == 0 {
        return format!("No matches for {pattern}");
    }

    if *context_lines > 0 {
        // Group matches by file
        let mut by_file: BTreeMap<String, Vec<&GrepMatch>> = BTreeMap::new();
        for m in matches {
            by_file
                .entry(m.path.to_string_lossy().into_owned())
                .or_default()
                .push(m);
        }

        let header = format!("{total_matches} match(es) in {file_count} file(s), showing {showing}");
        let mut parts = vec![header];

        for (file_path, file_matches) in &by_file {
            let display_path = format_tool_path(&input.cwd, Path::new(file_path));
            parts.push(format!("\n{display_path}:"));
            for m in file_matches {
                parts.push(format!("  {}: {}", m.line, m.text));
            }
        }

        parts.join("\n")
    } else {
        let header = format!("{total_matches} match(es) in {file_count} file(s), showing {showing}");
        let lines: Vec<String> = matches
            .iter()
            .map(|m| {
                let display_path = format_tool_path(&input.cwd, &m.path);
                format!("{display_path}:{}: {}", m.line, m.text)
            })
            .collect();
        std::iter::once(header)
            .chain(lines)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

// ── execute_prepared ──────────────────────────────────────────────────────────

/// Runs the grep and produces the result plus output text.
fn execute_prepared(input: &GrepToolInput) -> (GrepToolResult, String) {
    let final_pattern = if input.literal {
        escape_regex(&input.pattern)
    } else {
        input.pattern.clone()
    };
    // build_regex is infallible here because we validated in prepare
    let regex = build_regex(&final_pattern, &input.flags).expect("regex validated in prepare");
    let grep_result = grep_files(&input.absolute_path, &regex, input.glob.as_deref(), input.max_results, input.context);

    let total_matches = grep_result.total_matches;
    let file_count = grep_result.file_count;
    let showing = grep_result.matches.len();

    let output_text = if total_matches == 0 {
        format!("No matches for {}", input.pattern)
    } else if input.context > 0 {
        // Re-read files to render context — group matches by file
        let mut file_match_map: BTreeMap<PathBuf, Vec<usize>> = BTreeMap::new();
        for m in &grep_result.matches {
            file_match_map
                .entry(m.path.clone())
                .or_default()
                .push(m.line - 1); // 0-based
        }

        let mut parts: Vec<String> = vec![];

        for (file_path, match_indices) in &file_match_map {
            if let Ok(buf) = fs::read(file_path) {
                let text = String::from_utf8_lossy(&buf).into_owned();
                let file_lines: Vec<&str> = text.split('\n').collect();
                let rendered = render_with_context(&file_lines, match_indices, input.context);
                let display_path = format_tool_path(&input.cwd, file_path);
                parts.push(format!("{display_path}:"));
                parts.extend(rendered);
            }
        }

        parts.join("\n")
    } else {
        format!("Found {total_matches} match(es) in {file_count} file(s), showing {showing}.")
    };

    (grep_result, output_text)
}

// ── public API ────────────────────────────────────────────────────────────────

/// OpenAI function schema for the grep tool.
pub fn definition() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "GREP",
            "description": "Search files in the workspace for lines matching a JavaScript-flavored regex pattern.",
            "parameters": {
                "additionalProperties": false,
                "properties": {
                    "context": {
                        "description": "Number of lines of context (before and after) to include around each match. Accepts 0–5, defaults to 0.",
                        "maximum": 5,
                        "minimum": 0,
                        "type": "number"
                    },
                    "flags": {
                        "description": "Optional regex flags to apply. Allowed characters: 'i' (case-insensitive), 'm' (multiline), 's' (dot-all). Example: \"im\".",
                        "type": "string"
                    },
                    "glob": {
                        "description": "Optional simple suffix filter like '*.ts' to restrict which files are searched.",
                        "type": "string"
                    },
                    "literal": {
                        "description": "When true, treat the pattern as a literal string (all regex metacharacters are escaped).",
                        "type": "boolean"
                    },
                    "maxResults": {
                        "description": "Maximum number of matching lines to return. Defaults to 50, capped at 200.",
                        "type": "number"
                    },
                    "path": {
                        "description": "Subdirectory or file to search within, relative to the current working directory or absolute. Defaults to '.'.",
                        "type": "string"
                    },
                    "pattern": {
                        "description": "A JavaScript-flavored regex string to search for.",
                        "type": "string"
                    }
                },
                "required": ["pattern"],
                "type": "object"
            }
        }
    })
}

/// Display string: `pattern=<p> path=<display> [glob=<g>]`.
pub fn display_input(args: &Value, ctx: &ToolCtx) -> Option<String> {
    let map = super::tool_arguments(args).ok()?;
    let input = prepare(&map, ctx).ok()?;
    let glob = input.glob.as_deref().map(|glob| format!(" glob={glob}")).unwrap_or_default();

    Some(format!("pattern={} path={}{glob}", input.pattern, input.display_path))
}

/// Full pipeline: parse args → prepare → execute → complete → ToolOutcome.
pub fn execute(args: &Value, ctx: &ToolCtx) -> ToolOutcome {
    let map = match super::tool_arguments(args) {
        Ok(m) => m,
        Err(e) => {
            return ToolOutcome {
                text: format!("ERROR: {e}"),
                failed: true,
            }
        }
    };

    let input = match prepare(&map, ctx) {
        Ok(i) => i,
        Err(e) => {
            return ToolOutcome {
                text: format!("ERROR: {e}"),
                failed: true,
            }
        }
    };

    let (grep_result, output_text) = execute_prepared(&input);
    let tool_content = complete_output(&input, &grep_result);

    // No-match is not a failure
    ToolOutcome {
        text: tool_content,
        failed: false,
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_ctx(cwd: &str) -> ToolCtx {
        ToolCtx { cwd: cwd.to_string().into(), allow_net: false, reference_roots: Vec::new() }
    }

    fn run_grep(cwd: &str, json: serde_json::Value) -> (String, String) {
        // Returns (output_text, tool_content)
        let map = json.as_object().unwrap().clone();
        let ctx = make_ctx(cwd);
        let input = prepare(&map, &ctx).unwrap();
        let (result, output_text) = execute_prepared(&input);
        let tool_content = complete_output(&input, &result);
        (output_text, tool_content)
    }

    fn run_grep_expect_err(cwd: &str, json: serde_json::Value) -> String {
        let map = json.as_object().unwrap().clone();
        let ctx = make_ctx(cwd);
        prepare(&map, &ctx).unwrap_err().to_string()
    }

    #[test]
    fn finds_matches_with_correct_file_line_text_format() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        fs::write(
            tmp.path().join("foo.ts"),
            "const hello = 'world';\nconst bye = 'earth';\n",
        )
        .unwrap();
        let (output_text, tool_content) = run_grep(
            cwd,
            serde_json::json!({ "pattern": "hello" }),
        );
        assert!(output_text.contains("1 match(es) in 1 file(s)") || tool_content.contains("1 match(es) in 1 file(s)"),
            "output_text={output_text} tool_content={tool_content}");
        assert!(tool_content.contains("foo.ts:1: const hello = 'world';"),
            "tool_content={tool_content}");
    }

    #[test]
    fn returns_line_numbers_correctly_for_multiple_matches() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        fs::write(
            tmp.path().join("data.ts"),
            "// line one\nconst foo = 1;\nconst bar = 2;\nconst foo2 = 3;\n",
        )
        .unwrap();
        let (_, tool_content) = run_grep(cwd, serde_json::json!({ "pattern": "const" }));
        assert!(tool_content.contains("data.ts:2: const foo = 1;"), "tc={tool_content}");
        assert!(tool_content.contains("data.ts:3: const bar = 2;"), "tc={tool_content}");
        assert!(tool_content.contains("data.ts:4: const foo2 = 3;"), "tc={tool_content}");
        assert!(tool_content.contains("3 match(es) in 1 file(s)"), "tc={tool_content}");
    }

    #[test]
    fn filters_files_by_glob_suffix() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        fs::write(tmp.path().join("a.ts"), "needle\n").unwrap();
        fs::write(tmp.path().join("b.md"), "needle\n").unwrap();
        let (_, tc) = run_grep(cwd, serde_json::json!({ "pattern": "needle", "glob": "*.ts" }));
        assert!(tc.contains("a.ts"), "tc={tc}");
        assert!(!tc.contains("b.md"), "tc={tc}");
    }

    #[test]
    fn no_match_is_not_a_failure() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        fs::write(tmp.path().join("a.ts"), "hello world\n").unwrap();
        let (output_text, tool_content) = run_grep(cwd, serde_json::json!({ "pattern": "xyzzy_not_found" }));
        assert!(output_text.contains("No matches for") || tool_content.contains("No matches for"),
            "output={output_text}");
        // execute() should not set failed=true
        let ctx = make_ctx(cwd);
        let outcome = execute(&serde_json::json!({ "pattern": "xyzzy_not_found" }), &ctx);
        assert!(!outcome.failed, "no-match should not be failure");
    }

    #[test]
    fn ignores_default_ignored_dirs() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        fs::create_dir_all(tmp.path().join("node_modules")).unwrap();
        fs::write(tmp.path().join("node_modules").join("pkg.ts"), "needle\n").unwrap();
        fs::write(tmp.path().join("real.ts"), "other\n").unwrap();
        let (_, tc) = run_grep(cwd, serde_json::json!({ "pattern": "needle" }));
        assert!(!tc.contains("node_modules"), "tc={tc}");
    }

    #[test]
    fn caps_results_at_max_results() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let content: String = (1..=10).map(|i| format!("match line {i}\n")).collect();
        fs::write(tmp.path().join("big.ts"), content).unwrap();
        let (_, tc) = run_grep(cwd, serde_json::json!({ "pattern": "match", "maxResults": 3 }));
        assert!(tc.contains("3 match(es)"), "tc={tc}");
    }

    #[test]
    fn skips_binary_files() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let mut bin = vec![0u8, 1u8, 2u8];
        bin.extend_from_slice(b"needle");
        fs::write(tmp.path().join("binary.bin"), &bin).unwrap();
        fs::write(tmp.path().join("text.ts"), "needle\n").unwrap();
        let (_, tc) = run_grep(cwd, serde_json::json!({ "pattern": "needle" }));
        assert!(!tc.contains("binary.bin"), "tc={tc}");
        assert!(tc.contains("text.ts"), "tc={tc}");
    }

    #[test]
    fn case_insensitive_search_with_flags_i() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        fs::write(tmp.path().join("hello.ts"), "Hello World\nhello world\nHELLO WORLD\n").unwrap();
        let (_, tc) = run_grep(cwd, serde_json::json!({ "pattern": "hello", "flags": "i" }));
        assert!(tc.contains("3 match(es)"), "tc={tc}");
        assert!(tc.contains("hello.ts:1:"), "tc={tc}");
        assert!(tc.contains("hello.ts:2:"), "tc={tc}");
        assert!(tc.contains("hello.ts:3:"), "tc={tc}");
    }

    #[test]
    fn literal_true_finds_verbatim_not_regex() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        fs::write(tmp.path().join("data.ts"), "axb(c)\na.b(c)\nfoo\n").unwrap();
        let (_, tc) = run_grep(cwd, serde_json::json!({ "pattern": "a.b(c)", "literal": true }));
        assert!(tc.contains("1 match(es)"), "tc={tc}");
        assert!(tc.contains("data.ts:2:"), "tc={tc}");
        assert!(!tc.contains("data.ts:1:"), "tc={tc}");
    }

    #[test]
    fn context_renders_before_after_lines_with_marker() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        fs::write(
            tmp.path().join("fruit.ts"),
            "apple\nbanana\nMATCH\ncherry\ndate\nelderberry\nMATCH\nfig\n",
        )
        .unwrap();
        let map = serde_json::json!({ "pattern": "MATCH", "context": 2 });
        let map = map.as_object().unwrap().clone();
        let ctx = make_ctx(cwd);
        let input = prepare(&map, &ctx).unwrap();
        let (_, output_text) = execute_prepared(&input);
        assert!(output_text.contains("> 3: MATCH"), "out={output_text}");
        assert!(output_text.contains("  2: banana"), "out={output_text}");
        assert!(output_text.contains("  4: cherry"), "out={output_text}");
        assert!(output_text.contains("> 7: MATCH"), "out={output_text}");
        assert!(output_text.contains("  6: elderberry"), "out={output_text}");
        assert!(output_text.contains("  8: fig"), "out={output_text}");
    }

    #[test]
    fn context_renders_separator_between_non_overlapping_groups() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let lines = ["MATCH", "b", "c", "d", "e", "f", "g", "h", "i", "MATCH"];
        let content = lines.join("\n") + "\n";
        fs::write(tmp.path().join("sep.ts"), content).unwrap();
        let map = serde_json::json!({ "pattern": "MATCH", "context": 2 });
        let map = map.as_object().unwrap().clone();
        let ctx = make_ctx(cwd);
        let input = prepare(&map, &ctx).unwrap();
        let (_, output_text) = execute_prepared(&input);
        assert!(output_text.contains("> 1: MATCH"), "out={output_text}");
        assert!(output_text.contains("> 10: MATCH"), "out={output_text}");
        assert!(output_text.contains("--"), "out={output_text}");
    }

    #[test]
    fn invalid_flags_string_throws_clear_error() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let err = run_grep_expect_err(cwd, serde_json::json!({ "pattern": "foo", "flags": "ig" }));
        // 'g' is not allowed — check error mentions "flag"
        assert!(err.to_lowercase().contains("flag"), "err={err}");
    }

    #[test]
    fn flags_and_literal_compose_case_insensitive_literal() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        fs::write(
            tmp.path().join("combo.ts"),
            "Hello.World\nhello.world\nHELLO.WORLD\nhXllXworld\n",
        )
        .unwrap();
        let (_, tc) = run_grep(
            cwd,
            serde_json::json!({ "pattern": "hello.world", "literal": true, "flags": "i" }),
        );
        // Should match lines 1-3 but NOT line 4
        assert!(tc.contains("3 match(es)"), "tc={tc}");
        assert!(!tc.contains("combo.ts:4:"), "tc={tc}");
    }

    #[test]
    fn matches_path_style_and_wildcard_globs_on_basename() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src").join("alpha.ts"), "needle here\n").unwrap();
        fs::write(tmp.path().join("src").join("alpha.md"), "needle here\n").unwrap();
        fs::write(tmp.path().join("turn1.ndjson"), "needle here\n").unwrap();

        let (_, tc_ts) = run_grep(cwd, serde_json::json!({ "pattern": "needle", "glob": "**/*.ts" }));
        assert!(tc_ts.contains("alpha.ts"), "ts tc={tc_ts}");
        assert!(!tc_ts.contains("alpha.md"), "ts tc={tc_ts}");

        let (_, tc_ndjson) = run_grep(cwd, serde_json::json!({ "pattern": "needle", "glob": "turn*.ndjson" }));
        assert!(tc_ndjson.contains("turn1.ndjson"), "ndjson tc={tc_ndjson}");

        let (_, tc_alpha) = run_grep(cwd, serde_json::json!({ "pattern": "needle", "glob": "alpha.*" }));
        assert!(tc_alpha.contains("alpha.md"), "alpha tc={tc_alpha}");
    }

    #[test]
    fn throws_error_for_invalid_regex_pattern() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let err = run_grep_expect_err(cwd, serde_json::json!({ "pattern": "[invalid((" }));
        assert!(err.contains("Invalid regex pattern"), "err={err}");
    }

    #[test]
    fn throws_error_when_pattern_is_missing() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        let err = run_grep_expect_err(cwd, serde_json::json!({}));
        assert!(err.to_lowercase().contains("pattern"), "err={err}");
    }

    // Explicit node_modules/pkg structure proves default-ignored dirs are skipped.
    #[test]
    fn skips_node_modules_directories() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        fs::create_dir_all(tmp.path().join("node_modules").join("pkg")).unwrap();
        fs::write(
            tmp.path().join("node_modules").join("pkg").join("index.ts"),
            "const secret = 'hidden';\n",
        )
        .unwrap();
        fs::write(tmp.path().join("src.ts"), "const visible = 'found';\n").unwrap();
        let (_, tc) = run_grep(cwd, serde_json::json!({ "pattern": "const" }));
        assert!(tc.contains("src.ts"), "tc={tc}");
        assert!(!tc.contains("node_modules"), "tc={tc}");
    }

    #[test]
    fn searches_specific_file_when_path_points_to_file() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().to_str().unwrap();
        fs::write(tmp.path().join("target.ts"), "const needle = 1;\nconst haystack = 2;\n").unwrap();
        fs::write(tmp.path().join("other.ts"), "const needle = 99;\n").unwrap();
        let (_, tc) = run_grep(cwd, serde_json::json!({ "pattern": "needle", "path": "target.ts" }));
        assert!(tc.contains("target.ts"), "tc={tc}");
        assert!(!tc.contains("other.ts"), "tc={tc}");
        assert!(tc.contains("1 match(es) in 1 file(s)"), "tc={tc}");
    }
}
