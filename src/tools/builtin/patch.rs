// port of tools/patch-tool.ts
//
// PATCH writes/edits files. Chunk 1 here holds the definition, the
// input/result structs and the pure text functions; the prepare/execute
// pipeline lives further down in this same file.

use anyhow::Result;
use serde_json::{json, Value};

use super::{tool_arguments, ToolCompletion, ToolCompletionBlock, ToolCtx, ToolOutcome};
use crate::tools::helpers::{count_lines, format_tool_path, get_optional_number_argument,
    get_required_string_argument, resolve_tool_path};

pub fn definition() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "PATCH",
            "description": "Apply a change to a file and save it to disk. Pass find + replace to swap exact text (every occurrence), or content to create the file or overwrite it entirely.",
            "parameters": {
                "additionalProperties": false,
                "properties": {
                    "content": {
                        "description": "Full file content to write. Creates the file (and parent directories) or overwrites it entirely. Use for new files or full rewrites.",
                        "type": "string"
                    },
                    "expectedOccurrences": {
                        "description": "How many occurrences of find you expect to replace. Required when find matches more than once; the edit is rejected if the actual count differs.",
                        "type": "number"
                    },
                    "files": {
                        "description": "Array of file edits to apply as a single atomic transaction. ALL entries are validated before any file is written; if any entry fails, NO files are changed. Each entry mirrors the single-file rules: pass content for a full write, or find + replace for a targeted edit.",
                        "items": {
                            "additionalProperties": false,
                            "properties": {
                                "content": {
                                    "description": "Full file content to write. Creates the file (and parent directories) or overwrites it entirely.",
                                    "type": "string"
                                },
                                "expectedOccurrences": {
                                    "description": "How many occurrences of find you expect to replace. Required when find matches more than once.",
                                    "type": "number"
                                },
                                "find": {
                                    "description": "Exact text to find in the file. Requires replace.",
                                    "type": "string"
                                },
                                "path": {
                                    "description": "Path to the file, relative to the current working directory or absolute.",
                                    "type": "string"
                                },
                                "replace": {
                                    "description": "Text that replaces every occurrence of find. May be empty to delete it.",
                                    "type": "string"
                                }
                            },
                            "required": ["path"],
                            "type": "object"
                        },
                        "type": "array"
                    },
                    "find": {
                        "description": "Exact text to find in the file, including whitespace. A unique match replaces one site; multiple matches require expectedOccurrences. Requires replace.",
                        "type": "string"
                    },
                    "path": {
                        "description": "Path to the file to change, relative to the current working directory or absolute.",
                        "type": "string"
                    },
                    "replace": {
                        "description": "Text that replaces every occurrence of find. May be empty to delete the found text.",
                        "type": "string"
                    }
                },
                "required": [],
                "type": "object"
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Shared input/result types (port of the TS PatchToolInput/FileEntry shapes)
// ---------------------------------------------------------------------------

/// Port of FileEntry — one entry in a multi-file transaction. Mirrors
/// single-file validation rules.
#[derive(Debug, Clone)]
pub struct FileEntry {
    /// Raw path from user input
    pub path: String,
    pub find: Option<String>,
    pub replace: Option<String>,
    pub expected_occurrences: Option<i64>,
    pub content: Option<String>,
}

/// Port of PatchToolResult — what execute() hands complete().
#[derive(Debug, Clone)]
pub struct PatchToolResult {
    pub diff: String,
    pub summary: String,
}

/// Port of ResolvedEntry — validated state for one entry after the
/// validation pass.
#[derive(Debug, Clone)]
pub struct ResolvedEntry {
    pub absolute_path: String,
    pub display_path: String,
    pub content: Option<String>,
    // find-replace mode
    pub effective_find: Option<String>,
    pub effective_replace: Option<String>,
    pub occurrences: Option<usize>,
    pub match_lines: Option<Vec<usize>>,
    pub crlf_note: Option<String>,
    /// The pre-image (None = new file)
    pub existing_text: Option<String>,
    /// The post-image that will be written
    pub new_text: String,
}

// ---------------------------------------------------------------------------
// Pure text helpers
// ---------------------------------------------------------------------------

/// Port of the countOccurrences helper (count of non-overlapping matches,
/// same as text.split(find).length - 1).
pub fn count_occurrences(text: &str, find: &str) -> usize {
    if find.is_empty() {
        return 0;
    }
    text.matches(find).count()
}

/// Port of matchLineNumbers — 1-based line numbers where the find text
/// starts, for honest summaries and actionable multi-site errors.
pub fn match_line_numbers(text: &str, find: &str) -> Vec<usize> {
    let mut lines: Vec<usize> = Vec::new();
    let mut search_from = 0usize;

    while let Some(rel) = text[search_from..].find(find) {
        if lines.len() >= 20 {
            return lines;
        }

        let index = search_from + rel;
        lines.push(text[..index].matches('\n').count() + 1);
        search_from = index + find.len().max(1);
    }

    lines
}

/// Port of findIncompleteOverwriteError. A full-content overwrite of a large
/// existing file must look like a complete file, not an abridged
/// reconstruction. Elision markers and drastic shrinks are rejected with the
/// file untouched; find/replace remains the escape hatch for genuinely large
/// deletions because it forces exact-text intent.
pub fn find_incomplete_overwrite_error(old_text: &str, new_text: &str) -> Option<String> {
    let old_lines = count_lines(old_text);
    // A file too small to hide an elision doesn't get marker-blocked: audit
    // probes saw an innocuous "rest of the file" sentence veto a 3-line doc.
    let marker_match = if old_lines >= 40 {
        find_elision_marker(new_text)
    } else {
        None
    };

    if let Some(marker) = marker_match {
        return Some(format!(
            "the new content contains \"{}\", which reads as an elided rewrite that would silently drop existing code. Write the complete file, or use find + replace for a targeted edit.",
            marker
        ));
    }

    let old_line_count = old_lines;
    let new_line_count = count_lines(new_text);

    if old_lines >= SHRINK_GUARD_MIN_LINES
        && new_line_count < old_line_count * SHRINK_GUARD_KEEP_RATIO
    {
        return Some(format!(
            "the new content has {} line(s) but the existing file has {} — a full overwrite that removes more than half of a large file is almost always an accidental elision. Use find + replace to make the intended edit (or delete the removed sections explicitly with find + an empty replace).",
            new_line_count, old_line_count
        ));
    }

    None
}

const SHRINK_GUARD_MIN_LINES: usize = 200;
const SHRINK_GUARD_KEEP_RATIO: usize = 2; // oldLines * 0.5 == oldLines / 2

/// Port of the ELISION_MARKER_PATTERN regex from patch-tool.ts. The TS keeps
/// a module-level compiled pattern; Rust needs it as a lazy static.
fn elision_marker_pattern() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static PATTERN: OnceLock<regex::Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        regex::Regex::new(
            r"omitted (?:here )?for brevity|rest of the (?:file|functions?|code|methods?)|unchanged from the original|remaining (?:code|functions?|lines?) (?:unchanged|omitted)|\.\.\. ?\(truncated\)|duplicate block removed",
        )
        .expect("elision marker regex")
    })
}

/// The TS guard tests `newText.match(ELISION_MARKER_PATTERN)` and uses the
/// match text in the error — here we return the matched substring directly.
fn find_elision_marker(text: &str) -> Option<String> {
    elision_marker_pattern().find(text).map(|m| m.as_str().to_string())
}

/// Port of buildUnifiedDiff.
pub fn build_unified_diff(display_path: &str, old_text: Option<&str>, new_text: &str) -> String {
    if let Some(old_text) = old_text {
        if old_text == new_text {
            return format!(
                "--- a/{}\n+++ b/{}\n(no changes)",
                display_path, display_path
            );
        }
    }

    let new_lines: Vec<&str> = if new_text.is_empty() {
        Vec::new()
    } else {
        new_text.split('\n').collect()
    };

    if old_text.is_none() {
        return format!(
            "--- /dev/null\n+++ b/{}\n@@ -0,0 +1,{} @@\n{}",
            display_path,
            new_lines.len(),
            new_lines
                .iter()
                .map(|line| format!("+{}", line))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    let old_lines: Vec<&str> = old_text.unwrap().split('\n').collect();
    let mut prefix_length = 0usize;

    while prefix_length < old_lines.len()
        && prefix_length < new_lines.len()
        && old_lines[prefix_length] == new_lines[prefix_length]
    {
        prefix_length += 1;
    }

    let mut suffix_length = 0usize;

    while suffix_length < old_lines.len() - prefix_length.min(old_lines.len())
        && suffix_length < new_lines.len().saturating_sub(prefix_length)
        && old_lines[old_lines.len() - 1 - suffix_length] == new_lines[new_lines.len() - 1 - suffix_length]
    {
        suffix_length += 1;
    }

    let removed_lines = &old_lines[prefix_length..old_lines.len() - suffix_length];
    let added_lines = &new_lines[prefix_length..new_lines.len() - suffix_length];
    let old_start = if removed_lines.is_empty() { prefix_length } else { prefix_length + 1 };
    let new_start = if added_lines.is_empty() { prefix_length } else { prefix_length + 1 };

    let mut out = String::new();
    out.push_str(&format!("--- a/{}\n+++ b/{}\n@@ -{},{} +{},{} @@", display_path, display_path, old_start, removed_lines.len(), new_start, added_lines.len()));
    for line in removed_lines {
        out.push_str(&format!("\n-{}", line));
    }
    for line in added_lines {
        out.push_str(&format!("\n+{}", line));
    }
    out
}

// ---------------------------------------------------------------------------
// Prepare stage (port of patch-tool.ts prepare)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct PatchToolInput {
    pub files: Vec<FileEntry>,
    pub verification: Option<String>,
    pub absolute_path: String,
    pub display_path: String,
    pub workspace_root: String,
    pub content: Option<String>,
    pub find: Option<String>,
    pub replace: Option<String>,
    pub expected_occurrences: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct PatchToolPrepared {
    pub input: PatchToolInput,
    pub display_input: String,
}

pub fn prepare(args: &Value, ctx: &ToolCtx) -> Result<PatchToolPrepared> {
    use anyhow::anyhow;
    let args = tool_arguments(args)?;
    let workspace_root = ctx.cwd.to_string_lossy().into_owned();

    // Multi-file transaction mode: files array takes precedence
    if args.get("files").map_or(false, |v| v.is_array()) {
        let entries = args.get("files").and_then(Value::as_array).unwrap();

        if entries.is_empty() {
            return Err(anyhow!("The \"files\" array must not be empty."));
        }

        let mut files: Vec<FileEntry> = Vec::with_capacity(entries.len());

        // Validate each entry's shape synchronously (path presence, XOR rules)
        for (i, entry) in entries.iter().enumerate() {
            let path = match entry.get("path").and_then(Value::as_str) {
                Some(path) if !path.is_empty() => path.to_string(),
                _ => return Err(anyhow!("[entry {}] Missing or empty \"path\".", i)),
            };

            let has_content = entry.get("content").map_or(false, Value::is_string);
            let has_find = entry.get("find").map_or(false, Value::is_string);
            let has_replace = entry.get("replace").map_or(false, Value::is_string);

            if has_content && (has_find || has_replace) {
                return Err(anyhow!(
                    "[entry {} \"{}\"] Pass either content, or find + replace — not both.",
                    i,
                    path
                ));
            }

            if !has_content {
                if !has_find || entry.get("find").and_then(Value::as_str).unwrap_or("").is_empty() {
                    return Err(anyhow!(
                        "[entry {} \"{}\"] Needs either content (full file write) or a non-empty \"find\" with \"replace\".",
                        i,
                        path
                    ));
                }

                if !has_replace {
                    return Err(anyhow!("[entry {} \"{}\"] Missing \"replace\".", i, path));
                }

                let find = entry.get("find").and_then(Value::as_str).unwrap_or("");
                let replace = entry.get("replace").and_then(Value::as_str).unwrap_or("");

                if find == replace {
                    return Err(anyhow!(
                        "[entry {} \"{}\"] \"find\" and \"replace\" are identical — nothing would change.",
                        i,
                        path
                    ));
                }
            }

            if let Some(expected_value) = entry.get("expectedOccurrences") {
                let valid = expected_value
                    .as_f64()
                    .map_or(false, |n| n.fract() == 0.0 && n >= 1.0);

                if !valid {
                    return Err(anyhow!(
                        "[entry {} \"{}\"] expectedOccurrences must be a positive integer.",
                        i,
                        path
                    ));
                }
            }

            files.push(FileEntry {
                path: path.clone(),
                find: entry.get("find").and_then(Value::as_str).map(str::to_string),
                replace: entry
                    .get("replace")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                expected_occurrences: entry
                    .get("expectedOccurrences")
                    .and_then(Value::as_f64)
                    .map(|n| n as i64),
                content: entry
                    .get("content")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            });
        }

        let display_input = format!("transaction: {} file(s)", files.len());

        return Ok(PatchToolPrepared {
            display_input,
            input: PatchToolInput {
                files,
                verification: None,
                absolute_path: String::new(),
                display_path: String::new(),
                workspace_root,
                content: None,
                find: None,
                replace: None,
                expected_occurrences: None,
            },
        });
    }

    let raw_path = get_required_string_argument(&args, "path")?;
    // find/replace/content are read raw (not trimmed): leading and trailing whitespace is significant in file edits.
    let content = args.get("content").and_then(Value::as_str).map(str::to_string);
    let find = args.get("find").and_then(Value::as_str).map(str::to_string);
    let replace = args
        .get("replace")
        .and_then(Value::as_str)
        .map(str::to_string);
    let expected_occurrences = get_optional_number_argument(&args, "expectedOccurrences")?;

    if let Some(expected) = expected_occurrences {
        if expected.fract() != 0.0 || expected < 1.0 {
            return Err(anyhow!("expectedOccurrences must be a positive integer."));
        }
    }

    if content.is_some() && (find.is_some() || replace.is_some()) {
        return Err(anyhow!("Pass either content, or find + replace — not both."));
    }

    if content.is_none() {
        if find.as_deref().map_or(true, str::is_empty) {
            return Err(anyhow!(
                "PATCH needs either content (full file write) or a non-empty \"find\" with \"replace\"."
            ));
        }

        if replace.is_none() {
            return Err(anyhow!(
                "Missing \"replace\". Pass the text that should replace \"find\" (an empty string deletes it)."
            ));
        }

        if find == replace {
            return Err(anyhow!(
                "\"find\" and \"replace\" are identical — nothing would change."
            ));
        }
    }

    let absolute_path = resolve_tool_path(&workspace_root, &raw_path);
    let display_path = format_tool_path(&workspace_root, &absolute_path);
    let display_input = if let Some(content) = &content {
        format!(
            "{} (write {} line(s))",
            display_path,
            count_lines(content)
        )
    } else {
        format!("{} (replace)", display_path)
    };

    Ok(PatchToolPrepared {
        display_input,
        input: PatchToolInput {
            files: Vec::new(),
            verification: None,
            absolute_path: absolute_path.to_string_lossy().into_owned(),
            display_path,
            workspace_root,
            content,
            find,
            replace,
            expected_occurrences: expected_occurrences.map(|n| n as i64),
        },
    })
}

// ---------------------------------------------------------------------------
// Syntax gate
// ---------------------------------------------------------------------------

// Port of path.extname(filePath).toLowerCase() — returns "" when there is no
// extension (including dotfiles like ".json").
fn path_extension(file_path: &str) -> String {
    std::path::Path::new(file_path)
        .extension()
        .map(|extension| format!(".{}", extension.to_string_lossy().to_lowercase()))
        .unwrap_or_default()
}

pub const SYNTAX_CHECKED_EXTENSIONS: &[&str] = &[
    ".cjs", ".cts", ".js", ".jsx", ".mjs", ".mts", ".ts", ".tsx",
];

// DEVIATION vs the TS gate: lci validates .json targets with JSON.parse; drip
// mirrors that with a small built-in JSON syntax scan below.
// Returns the first syntax error in the text, or None when it parses (or when
// the file type has no checker available). Uses the TypeScript transpiler's
// syntactic diagnostics because it runs under both Bun and Node; if the
// dependency is unavailable in the host project, the gate degrades to a no-op.
pub fn find_syntax_error(file_path: &str, text: &str) -> Option<String> {
    let extension = path_extension(file_path);

    if extension == ".json" {
        // TS: JSON.parse(text) → the parser's message on failure. serde_json's
        // message wording differs from V8's; the gate semantics are identical.
        return match serde_json::from_str::<serde_json::Value>(text) {
            Ok(_) => None,
            Err(error) => Some(error.to_string()),
        };
    }

    if !SYNTAX_CHECKED_EXTENSIONS.contains(&extension.as_str()) {
        return None;
    }

    let script = r#"const ts=require("typescript");let s="";process.stdin.on("data",c=>s+=c).on("end",()=>{const r=ts.transpileModule(s,{fileName:process.argv[1],reportDiagnostics:true,compilerOptions:{jsx:ts.JsxEmit.Preserve,target:ts.ScriptTarget.ESNext}});const d=(r.diagnostics||[])[0];if(!d){process.stdout.write("");return;}const p=d.file?d.file.getLineAndCharacterOfPosition(d.start||0):{line:0,character:0};process.stdout.write(`${p.line+1}:${p.character+1} ${ts.flattenDiagnosticMessageText(d.messageText," ")}`);})"#;

    let mut child = match std::process::Command::new("bun")
        .arg("-e")
        .arg(script)
        .arg(file_path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return None,
    };

    {
        use std::io::Write;
        let mut stdin = match child.stdin.take() {
            Some(stdin) => stdin,
            None => return None,
        };
        let _ = stdin.write_all(text.as_bytes());
    }

    let output = match child.wait_with_output() {
        Ok(output) => output,
        Err(_) => return None,
    };

    if !output.status.success() {
        return None;
    }

    let rendered = String::from_utf8_lossy(&output.stdout).trim().to_string();

    if rendered.is_empty() {
        None
    } else {
        Some(rendered)
    }
}

// A patch may not turn a parseable file unparseable. This is the guard against
// partial-match find/replace mangling a declaration: the mistake surfaces as an
// immediate tool error (with the file untouched) instead of a broken build that
// some later activation has to rediscover.
pub fn assert_patch_keeps_file_parseable(
    display_path: &str,
    absolute_path: &str,
    old_text: &str,
    new_text: &str,
) -> Result<(), String> {
    let new_syntax_error = find_syntax_error(absolute_path, new_text);

    if new_syntax_error.is_none() {
        return Ok(());
    }

    let old_syntax_error = find_syntax_error(absolute_path, old_text);

    if old_syntax_error.is_some() {
        // The file was already broken; refusing every edit would leave no way to fix it.
        return Ok(());
    }

    Err(format!(
        "PATCH rejected: the change would introduce a syntax error in \"{}\" — {}. The file was left unchanged. READ the affected region and send a corrected patch (make sure find matches the complete construct being replaced, not a prefix of it).",
        display_path,
        new_syntax_error.unwrap_or_default()
    ))
}

// Validate one FileEntry and compute its post-image. Returns an error message
// that includes the entry index + path on any problem so the caller can
// surface it without writing anything.
pub fn validate_file_entry(
    entry: &FileEntry,
    index: usize,
    workspace_root: &str,
) -> Result<ResolvedEntry, String> {
    let tag = format!("[entry {} \"{}\"]", index, entry.path);

    // Mutual-exclusion: content XOR find/replace
    if entry.content.is_some() && (entry.find.is_some() || entry.replace.is_some()) {
        return Err(format!(
            "{} Pass either content, or find + replace — not both.",
            tag
        ));
    }

    if entry.content.is_none() {
        if entry
            .find
            .as_deref()
            .map(|find| find.is_empty())
            .unwrap_or(true)
        {
            return Err(format!(
                "{} Needs either content (full file write) or a non-empty \"find\" with \"replace\".",
                tag
            ));
        }

        if entry.replace.is_none() {
            return Err(format!(
                "{} Missing \"replace\". Pass the text that should replace \"find\" (an empty string deletes it).",
                tag
            ));
        }
    }

    if let Some(expected_occurrences) = entry.expected_occurrences {
        if expected_occurrences < 1 {
            return Err(format!(
                "{} expectedOccurrences must be a positive integer.",
                tag
            ));
        }
    }

    let absolute_path_buf = crate::tools::helpers::resolve_tool_path(workspace_root, &entry.path);
    let display_path = crate::tools::helpers::format_tool_path(workspace_root, &absolute_path_buf);
    let absolute_path = absolute_path_buf.to_string_lossy().to_string();
    let path_kind = match crate::tools::helpers::assert_patch_target_path(&absolute_path_buf, &display_path) {
        Ok(kind) => kind,
        Err(error) => return Err(error.to_string()),
    };

    if let Some(content) = entry.content.clone() {
        // --- content mode ---
        let existing_text: Option<String> = if matches!(path_kind, crate::tools::helpers::ToolPathKind::File) {
            match std::fs::read_to_string(&absolute_path_buf) {
                Ok(text) => Some(text),
                Err(error) => return Err(format!("{} {}", tag, error)),
            }
        } else {
            None
        };

        if existing_text.is_some() {
            let existing = existing_text.clone().unwrap_or_default();
            let incomplete_error = find_incomplete_overwrite_error(&existing, &content);

            if let Some(message) = incomplete_error {
                return Err(format!("{} PATCH rejected: {}", tag, message));
            }

            if let Err(error) =
                assert_patch_keeps_file_parseable(&display_path, &absolute_path, &existing, &content)
            {
                return Err(error);
            }
        }

        return Ok(ResolvedEntry {
            absolute_path,
            display_path,
            content: Some(content.clone()),
            effective_find: None,
            effective_replace: None,
            occurrences: None,
            match_lines: None,
            crlf_note: None,
            existing_text,
            new_text: content,
        });
    }

    // --- find/replace mode ---
    if matches!(path_kind, crate::tools::helpers::ToolPathKind::Missing) {
        return Err(format!(
            "{} \"{}\" does not exist. Pass content to create it, or fix the path.",
            tag, display_path
        ));
    }

    let existing_text = match std::fs::read_to_string(&absolute_path_buf) {
        Ok(text) => text,
        Err(error) => return Err(format!("{} {}", tag, error)),
    };
    let mut effective_find = entry.find.clone().unwrap_or_default();
    let mut effective_replace = entry.replace.clone().unwrap_or_default();
    let mut crlf_note = String::new();
    let mut occurrences = count_occurrences(&existing_text, &effective_find);

    if occurrences == 0
        && existing_text.contains("\r\n")
        && effective_find.contains('\n')
        && !effective_find.contains('\r')
    {
        let crlf_find = effective_find.replace('\n', "\r\n");
        let crlf_occurrences = count_occurrences(&existing_text, &crlf_find);

        if crlf_occurrences > 0 {
            effective_find = crlf_find;
            effective_replace = effective_replace.replace('\n', "\r\n");
            occurrences = count_occurrences(&existing_text, &effective_find);
            crlf_note = " (the file uses CRLF line endings; the edit was applied with CRLF)".to_string();
        }
    }


    if occurrences == 0 && find_has_line_number_prefix(&effective_find) {
        let stripped_find = strip_line_number_prefixes(&effective_find);
        let stripped_occurrences = count_occurrences(&existing_text, &stripped_find);

        if stripped_occurrences > 0 {
            effective_find = stripped_find;
            effective_replace = strip_line_number_prefixes(&effective_replace);
            occurrences = count_occurrences(&existing_text, &effective_find);
            crlf_note = " (line-number prefixes copied from READ output were stripped)".to_string();
        }
    }

    if occurrences == 0 {
        return Err(format!(
            "{} The find text was not found in \"{}\". READ the file and pass the exact text, including whitespace.",
            tag, display_path
        ));
    }

    if entry.expected_occurrences.is_none() && occurrences > 1 {
        return Err(format!(
            "{} The find text appears {} times in \"{}\" (lines {}). Pass expectedOccurrences: {} to replace all of them, or extend the find text.",
            tag,
            occurrences,
            display_path,
            match_line_numbers(&existing_text, &effective_find)
                .iter()
                .map(|line| line.to_string())
                .collect::<Vec<String>>()
                .join(", "),
            occurrences
        ));
    }

    if entry.expected_occurrences.is_some() {
        let expected = entry.expected_occurrences.unwrap_or_default();
        if occurrences != expected as usize {
            return Err(format!(
                "{} Expected {} occurrence(s) of the find text in \"{}\" but found {}. Nothing was changed — pass expectedOccurrences: {} or extend the find text.",
                tag, expected, display_path, occurrences, expected
            ));
        }
    }

    let match_lines = match_line_numbers(&existing_text, &effective_find);
    let new_text = existing_text
        .split(&effective_find)
        .collect::<Vec<&str>>()
        .join(&effective_replace);

    if let Err(error) = assert_patch_keeps_file_parseable(&display_path, &absolute_path, &existing_text, &new_text) {
        return Err(error);
    }

    Ok(ResolvedEntry {
        absolute_path,
        display_path,
        content: None,
        effective_find: Some(effective_find),
        effective_replace: Some(effective_replace),
        occurrences: Some(occurrences),
        match_lines: Some(match_lines),
        crlf_note: Some(crlf_note),
        existing_text: Some(existing_text),
        new_text,
    })
}

fn find_has_line_number_prefix(text: &str) -> bool {
    text.lines().any(|line| line_number_prefix_len(line).is_some())
}

fn strip_line_number_prefixes(text: &str) -> String {
    text.split('\n')
        .map(|line| line_number_prefix_len(line).map_or(line, |length| &line[length..]))
        .collect::<Vec<&str>>()
        .join("\n")
}

fn line_number_prefix_len(line: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut index = 0usize;

    while index < bytes.len() && (bytes[index] == b' ' || bytes[index] == b'\t') {
        index += 1;
    }

    let digits_start = index;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        index += 1;
    }

    if index == digits_start || index >= bytes.len() || bytes[index] != b'\t' {
        return None;
    }

    Some(index + 1)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

// MARKER_TEST
// ---------------------------------------------------------------------------
// Execute + complete stages
// ---------------------------------------------------------------------------

fn execute_transaction(resolved: &[ResolvedEntry], workspace_root: &str) -> Result<(), String> {
    for entry in resolved {
        crate::lib_fs::write_file_atomic(std::path::Path::new(&entry.absolute_path), &entry.new_text, true)
            .map_err(|error| error.to_string())?;
        let _ = crate::tools::patch_journal::append_patch_journal(
            std::path::Path::new(workspace_root),
            &crate::tools::patch_journal::AppendPatchJournalEntry {
                path: entry.absolute_path.clone(),
                post_content: entry.new_text.clone(),
                pre_image: entry.existing_text.clone(),
            },
        );
    }
    Ok(())
}

/// What the execute stage returns: { data, outputText }.
pub struct PatchToolExecution {
    pub data: PatchToolResult,
    pub output_text: String,
}

pub fn execute_prepared(prepared: &PatchToolPrepared, ctx: &ToolCtx) -> Result<PatchToolExecution, String> {
    let workspace_root = ctx.cwd.display().to_string();

    if !prepared.input.files.is_empty() {
        let mut resolved: Vec<ResolvedEntry> = Vec::new();
        for (index, file_entry) in prepared.input.files.iter().enumerate() {
            resolved.push(validate_file_entry(file_entry, index, &workspace_root)?);
        }
        execute_transaction(&resolved, &workspace_root)?;
        let mut summary_lines: Vec<String> = Vec::new();
        let mut diff_parts: Vec<String> = Vec::new();
        for entry in &resolved {
            let line_summary = if entry.content.is_some() {
                let verb = if entry.existing_text.is_some() { "Overwrote" } else { "Created" };
                format!(
                    "{}: {} {} line(s)",
                    entry.display_path,
                    verb.to_lowercase(),
                    crate::tools::helpers::count_lines(&entry.new_text)
                )
            } else {
                format!(
                    "{}: replaced {} occurrence(s){}",
                    entry.display_path,
                    entry.occurrences.unwrap_or(0),
                    entry.crlf_note.clone().unwrap_or_default()
                )
            };
            summary_lines.push(line_summary);
            diff_parts.push(build_unified_diff(&entry.display_path, entry.existing_text.as_deref(), &entry.new_text));
        }
        let summary = summary_lines.join("\n");
        return Ok(PatchToolExecution {
            data: PatchToolResult {
                diff: diff_parts.join("\n"),
                summary: summary.clone(),
            },
            output_text: summary,
        });
    }

    let absolute_path = prepared.input.absolute_path.clone();
    let display_path = prepared.input.display_path.clone();

    if prepared.input.content.is_some() {
        let content = prepared.input.content.clone().unwrap_or_default();
        let existing_text: Option<String> = if std::path::Path::new(&absolute_path).exists() {
            match std::fs::read_to_string(&absolute_path) {
                Ok(text) => Some(text),
                Err(error) => return Err(format!("Could not read \"{}\": {}", display_path, error)),
            }
        } else {
            None
        };

        if existing_text.is_some() {
            assert_patch_keeps_file_parseable(&display_path, &absolute_path, existing_text.as_deref().unwrap_or(""), &content)?;
        }

        crate::lib_fs::write_file_atomic(std::path::Path::new(&absolute_path), &content, true).map_err(|error| error.to_string())?;
        let _ = crate::tools::patch_journal::append_patch_journal(
            std::path::Path::new(&workspace_root),
            &crate::tools::patch_journal::AppendPatchJournalEntry {
                path: absolute_path.clone(),
                post_content: content.clone(),
                pre_image: existing_text.clone(),
            },
        );

        let summary = if existing_text.is_none() {
            format!("Created {} with {} line(s).", display_path, crate::tools::helpers::count_lines(&content))
        } else {
            format!("Overwrote {} with {} line(s).", display_path, crate::tools::helpers::count_lines(&content))
        };

        return Ok(PatchToolExecution {
            data: PatchToolResult {
                diff: build_unified_diff(&display_path, existing_text.as_deref(), &content),
                summary: summary.clone(),
            },
            output_text: summary,
        });
    }

    if !std::path::Path::new(&absolute_path).exists() {
        return Err(format!(
            "\"{}\" does not exist. Pass content to create it, or fix the path.",
            display_path
        ));
    }
    let existing_text = std::fs::read_to_string(&absolute_path).map_err(|error| error.to_string())?;

    let mut effective_find = prepared.input.find.clone().unwrap_or_default();
    let mut effective_replace = prepared.input.replace.clone().unwrap_or_default();
    let mut crlf_note = String::new();
    let mut occurrences = count_occurrences(&existing_text, &effective_find);

    // LF find text against a CRLF file fails wholesale with a hint that
    // misses the cause; retry with converted line endings and say so.
    if occurrences == 0 && existing_text.contains("\r\n") && effective_find.contains('\n') && !effective_find.contains('\r') {
        let crlf_find = effective_find.replace('\n', "\r\n");
        let crlf_occurrences = existing_text.matches(&crlf_find).count();
        if crlf_occurrences > 0 {
            effective_find = crlf_find;
            effective_replace = effective_replace.replace('\n', "\r\n");
            occurrences = crlf_occurrences;
            crlf_note = " (the file uses CRLF line endings; the edit was applied with CRLF)".to_string();
        }
    }

    // READ output prefixes every line with "N\t"; a find copied verbatim from
    // it will never match the raw file. Strip the prefixes and retry.
    if occurrences == 0 && has_line_number_prefix_signal(&effective_find) {
        let stripped_find = strip_line_number_prefixes(&effective_find);
        let stripped_occurrences = existing_text.matches(&stripped_find).count();
        if stripped_occurrences > 0 {
            effective_find = stripped_find;
            effective_replace = strip_line_number_prefixes(&effective_replace);
            occurrences = stripped_occurrences;
            crlf_note = " (line-number prefixes copied from READ output were stripped)".to_string();
        }
    }

    if occurrences == 0 {
        return Err(format!(
            "The find text was not found in \"{}\". READ the file and pass the exact text, including whitespace (do not include READ's line-number prefixes).",
            display_path
        ));
    }

    if let Some(expected) = prepared.input.expected_occurrences {
        if occurrences as i64 != expected {
            return Err(format!(
                "Expected {} occurrence(s) of the find text in \"{}\" but found {}. Nothing was changed — re-check with READ/GREP, then pass expectedOccurrences: {} or extend the find text.",
                expected, display_path, occurrences, occurrences
            ));
        }
    }

    // Replacing several sites the model may not know about is how spread
    // damage happens; multi-site replaces must be explicit.
    if prepared.input.expected_occurrences.is_none() && occurrences > 1 {
        return Err(format!(
            "The find text appears {} times in \"{}\" (lines {}). Nothing was changed — pass expectedOccurrences: {} to replace all of them, or extend the find text to target one site.",
            occurrences,
            display_path,
            match_line_numbers(&existing_text, &effective_find)
                .iter()
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(", "),
            occurrences
        ));
    }

    let new_text = existing_text.replace(&effective_find, &effective_replace);

    assert_patch_keeps_file_parseable(&display_path, &absolute_path, &existing_text, &new_text)?;
    crate::lib_fs::write_file_atomic(std::path::Path::new(&absolute_path), &new_text, true).map_err(|error| error.to_string())?;
    let _ = crate::tools::patch_journal::append_patch_journal(
        std::path::Path::new(&workspace_root),
        &crate::tools::patch_journal::AppendPatchJournalEntry {
            path: absolute_path.clone(),
            post_content: new_text.clone(),
            pre_image: Some(existing_text.clone()),
        },
    );

    let summary = format!(
        "Replaced {} occurrence(s) at line(s) {} in {}{}.",
        occurrences,
        match_line_numbers(&existing_text, &effective_find)
            .iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(", "),
        display_path,
        crlf_note
    );

    Ok(PatchToolExecution {
        data: PatchToolResult {
            diff: build_unified_diff(&display_path, Some(&existing_text), &new_text),
            summary: summary.clone(),
        },
        output_text: summary,
    })
}

pub fn complete(prepared: &PatchToolPrepared, result: &PatchToolResult) -> ToolCompletion {
    let description = if !prepared.input.files.is_empty() {
        format!("Applied changes to {} file(s).", prepared.input.files.len())
    } else {
        format!("Applied change to {}.", prepared.input.display_path)
    };
    let primary_path = if !prepared.input.files.is_empty() {
        prepared.input.files.first().map(|entry| entry.path.clone()).unwrap_or_default()
    } else {
        prepared.input.absolute_path.clone()
    };

    ToolCompletion {
        blocks: vec![ToolCompletionBlock {
            code: result.diff.clone(),
            description,
            language: "diff".to_string(),
            path: std::path::PathBuf::from(&primary_path),
        }],
        tool_content: format!("{}\n\n{}", result.summary, result.diff),
    }
}

pub fn execute(args: &serde_json::Value, ctx: &ToolCtx) -> ToolOutcome {
    let prepared = match prepare(args, ctx) {
        Ok(prepared) => prepared,
        Err(error) => return ToolOutcome::error(error),
    };
    let result = match execute_prepared(&prepared, ctx) {
        Ok(result) => result,
        Err(message) => return ToolOutcome::error(anyhow::anyhow!(message)),
    };
    let completion = complete(&prepared, &result.data);
    ToolOutcome::success(completion.tool_content)
}

fn has_line_number_prefix_signal(text: &str) -> bool {
    text.split('\n').any(|line| {
        let trimmed = line.trim_start();
        let digit_count = trimmed.chars().take_while(|c| c.is_ascii_digit()).count();
        digit_count > 0 && trimmed[digit_count..].starts_with('\t')
    })
}

#[cfg(test)]
mod execute_tests {
    use super::*;

    // Creates a unique workspace dir under the OS temp dir and returns its path.
    fn temp_workspace(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "drip-patch-execute-tests-{}-{}",
            tag,
            std::process::id() as u64 ^ std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("create temp workspace");
        dir
    }

    fn ctx_for(dir: &std::path::Path) -> ToolCtx {
        ToolCtx {
            cwd: dir.to_path_buf(),
            allow_net: false,
        }
    }

    #[test]
    fn find_replace_rewrites_file_and_tool_content_carries_diff() {
        let workspace = temp_workspace("find-replace");
        let ctx = ctx_for(&workspace);
        let file = workspace.join("notes.txt");
        std::fs::write(&file, "alpha\nbeta\ngamma\n").unwrap();

        let outcome = execute(
            &serde_json::json!({
                "path": "notes.txt",
                "find": "beta",
                "replace": "delta"
            }),
            &ctx,
        );

        assert!(!outcome.failed, "patch failed: {}", outcome.text);
        let rewritten = std::fs::read_to_string(&file).unwrap();
        assert_eq!(rewritten, "alpha\ndelta\ngamma\n");
        assert!(
            outcome.text.contains("--- a/notes.txt\n+++ b/notes.txt\n@@ -2,1 +2,1 @@\n-beta\n+delta"),
            "expected unified diff header in tool_content, got: {}",
            outcome.text
        );
        assert!(outcome.text.contains("notes.txt"), "diff header must name the display path");
    }

    #[test]
    fn overwrite_creates_new_file_parents_and_one_journal_line() {
        let workspace = temp_workspace("overwrite-journal");
        let ctx = ctx_for(&workspace);
        let journal_path = crate::tools::patch_journal::patch_journal_path(&workspace);
        assert!(
            !journal_path.exists(),
            "journal should not exist before the patch"
        );

        let outcome = execute(
            &serde_json::json!({
                "path": "nested/dir/hello.txt",
                "content": "hello\nworld\n"
            }),
            &ctx,
        );

        assert!(!outcome.failed, "patch failed: {}", outcome.text);
        let written = std::fs::read_to_string(workspace.join("nested/dir/hello.txt")).unwrap();
        assert_eq!(written, "hello\nworld\n");

        let journal = std::fs::read_to_string(&journal_path).unwrap();
        let lines: Vec<&str> = journal.lines().collect();
        assert_eq!(
            lines.len(),
            1,
            "expected exactly one journal line, got: {lines:?}"
        );
        assert!(lines[0].contains("nested/dir/hello.txt"));
    }

    #[test]
    fn zero_matches_returns_exact_error_and_leaves_file_untouched() {
        let workspace = temp_workspace("zero-match");
        let ctx = ctx_for(&workspace);
        let file = workspace.join("plain.txt");
        let original = "unchanged body\n";
        std::fs::write(&file, original).unwrap();

        let outcome = execute(
            &serde_json::json!({
                "path": "plain.txt",
                "find": "not-present-text",
                "replace": "anything"
            }),
            &ctx,
        );

        assert!(outcome.failed);
        assert_eq!(
            outcome.text,
            "ERROR: The find text was not found in \"plain.txt\". READ the file and pass the exact text, including whitespace (do not include READ's line-number prefixes)."
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), original);
        let journal_path = crate::tools::patch_journal::patch_journal_path(&workspace);
        assert!(
            !journal_path.exists(),
            "failed patch must not touch the journal"
        );
    }
}

#[cfg(test)]
mod prepare_tests {
    use super::*;

    fn test_ctx() -> ToolCtx {
        ToolCtx {
            cwd: std::env::temp_dir(),
            allow_net: false,
        }
    }

    #[test]
    fn missing_files_array_falls_through_to_required_path() {
        let err = prepare(&json!({}), &test_ctx()).unwrap_err();

        assert!(
            err.to_string().contains("path"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn entry_without_find_replace_or_content_is_rejected() {
        let err = prepare(
            &json!({ "files": [{ "path": "a.txt" }] }),
            &test_ctx(),
        )
        .unwrap_err();

        assert_eq!(
            err.to_string(),
            "[entry 0 \"a.txt\"] Needs either content (full file write) or a non-empty \"find\" with \"replace\"."
        );
    }

    #[test]
    fn two_file_transaction_builds_display_input() {
        let prepared = prepare(
            &json!({
                "files": [
                    { "path": "a.txt", "content": "hello\n" },
                    { "path": "b.txt", "find": "x", "replace": "y" }
                ]
            }),
            &test_ctx(),
        )
        .unwrap();

        assert_eq!(prepared.display_input, "transaction: 2 file(s)");
        assert_eq!(prepared.input.files.len(), 2);
        assert_eq!(prepared.input.files[0].content.as_deref(), Some("hello\n"));
        assert_eq!(prepared.input.files[1].find.as_deref(), Some("x"));
        assert_eq!(prepared.input.files[1].replace.as_deref(), Some("y"));
        assert_eq!(
            prepared.input.workspace_root,
            test_ctx().cwd.to_string_lossy()
        );
    }
}

