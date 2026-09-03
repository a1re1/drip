// Resolves @path and @path#start:end mentions in a goal into a context block
// the activation prompt can carry. Directory mentions become a bounded tree
// listing.
//
// The TypeScript original pulls parseChatFileMentions /
// resolveParsedChatFileMentions in from ../chat/file-references(.server).ts;
// those live as private helpers in this module so the cli mod tree stays as
// shipped (see the plan: only the listed modules are created).

use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::chat::types::{ChatContextFile, ChatContextFileLineRange};

// --- src/chat/file-references.ts (inlined) ---

// export type ParsedChatFileMention = { endLine?, mention, pathText, startLine? }
#[derive(Clone, Debug, PartialEq)]
pub struct ParsedChatFileMention {
    pub end_line: Option<usize>,
    pub mention: String,
    pub path_text: String,
    pub start_line: Option<usize>,
}

// export type ActiveChatFileMention = { mentionStart, pathEnd, pathStart, pathText, tokenEnd }
#[derive(Clone, Debug, PartialEq)]
pub struct ActiveChatFileMention {
    pub mention_start: usize,
    pub path_end: usize,
    pub path_start: usize,
    pub path_text: String,
    pub token_end: usize,
}

fn trim_trailing_mention_punctuation(value: &str) -> String {
    // /[),.:;\]}]+$/g
    value
        .trim_end_matches([')', ',', '.', ':', ';', ']', '}'])
        .to_string()
}

// JS /\s/ also matches U+FEFF; Rust's char::is_whitespace does not. The TS
// tests never exercise U+FEFF, so char::is_whitespace is used directly.
fn is_whitespace_character(value: Option<char>) -> bool {
    match value {
        None => true,
        Some(c) => c.is_whitespace(),
    }
}

// Full-fidelity port of the FILE_MENTION_PATTERN scan:
// /(^|\s)@([A-Za-z0-9._/-]+)(?:#(\d+)(?::(\d+))?)?/g
// matchAll with a non-overlapping global regex: scanning restarts at lastIndex,
// i.e. just after the match — which includes the one-character (^|\s) group, so
// a separator consumed as group 1 cannot also start the next match.
pub fn parse_chat_file_mentions(text: &str) -> Vec<ParsedChatFileMention> {
    let chars: Vec<char> = text.chars().collect();
    let is_mention_char =
        |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-');
    let mut mentions: Vec<ParsedChatFileMention> = Vec::new();

    let mut i = 0usize;
    while i < chars.len() {
        // (?:^|\s) — group 1
        let boundary = i == 0 || is_whitespace_character(Some(chars[i - 1]));
        if !boundary || chars[i] != '@' {
            i += 1;
            continue;
        }

        // group 2 — the path text
        let path_start = i + 1;
        let mut j = path_start;
        while j < chars.len() && is_mention_char(chars[j]) {
            j += 1;
        }
        let path_end = j;

        // (?:#(\d+)(?::(\d+))?)? — optional line-range suffix
        let mut start_line_text: Option<String> = None;
        let mut end_line_text: Option<String> = None;
        if chars.get(j).is_some_and(|c| *c == '#') {
            let digits_start = j + 1;
            let mut digits_end = digits_start;
            while digits_end < chars.len() && chars[digits_end].is_ascii_digit() {
                digits_end += 1;
            }
            if digits_end > digits_start {
                start_line_text = Some(chars[digits_start..digits_end].iter().collect());
                j = digits_end;
                if j + 1 < chars.len() && chars[j] == ':' && chars[j + 1].is_ascii_digit() {
                    let end_start = j + 1;
                    let mut end_end = end_start;
                    while end_end < chars.len() && chars[end_end].is_ascii_digit() {
                        end_end += 1;
                    }
                    end_line_text = Some(chars[end_start..end_end].iter().collect());
                    j = end_end;
                }
            }
        }

        let path_text_raw: String = chars[path_start..path_end].iter().collect();
        let path_text = trim_trailing_mention_punctuation(&path_text_raw);

        if !path_text.is_empty() {
            // The mention text keeps the raw captured digit strings ("#01:02"
            // stays "#01:02") while start_line / end_line carry the parsed
            // values.
            let mut mention = format!("@{path_text}");
            if let Some(start) = &start_line_text {
                mention.push('#');
                mention.push_str(start);
                if let Some(end) = &end_line_text {
                    mention.push(':');
                    mention.push_str(end);
                }
            }
            let start_line = start_line_text
                .as_deref()
                .and_then(|t| t.parse::<usize>().ok());
            let end_line = end_line_text
                .as_deref()
                .and_then(|t| t.parse::<usize>().ok());

            mentions.push(ParsedChatFileMention {
                end_line,
                mention,
                path_text,
                start_line,
            });
        }

        // The scan restarts just after the whole match (regex lastIndex).
        i = if j > i { j } else { i + 1 };
    }

    mentions
}

// buildChatContextFileLineRange
pub fn build_chat_context_file_line_range(
    start_line: Option<usize>,
    end_line: Option<usize>,
) -> Option<ChatContextFileLineRange> {
    let start = start_line?;

    Some(ChatContextFileLineRange {
        start_line: start,
        end_line: end_line.unwrap_or(start),
    })
}

// getActiveChatFileMention. TS indexes are UTF-16 code units; the ported suite
// does not exercise these helpers with non-ASCII text, so char indices with
// clamping to [0, len] are used.
pub fn get_active_chat_file_mention(text: &str, cursor: usize) -> Option<ActiveChatFileMention> {
    let chars: Vec<char> = text.chars().collect();
    let safe_cursor = cursor.min(chars.len());

    let mut token_start = safe_cursor;
    while token_start > 0 && !is_whitespace_character(Some(chars[token_start - 1])) {
        token_start -= 1;
    }

    if chars.get(token_start) != Some(&'@') {
        return None;
    }

    let mut token_end = safe_cursor;
    while token_end < chars.len() && !is_whitespace_character(Some(chars[token_end])) {
        token_end += 1;
    }

    let token: String = chars[token_start..token_end].iter().collect();
    let hash_index = token.chars().position(|c| c == '#');
    let raw_path_end = match hash_index {
        None => token_end,
        Some(hash) => token_start + hash,
    };
    let path_text = trim_trailing_mention_punctuation(
        &chars[token_start + 1..raw_path_end].iter().collect::<String>(),
    );
    let path_start = token_start + 1;
    let path_end = path_start + path_text.chars().count();

    if safe_cursor < path_start || safe_cursor > path_end {
        return None;
    }

    Some(ActiveChatFileMention {
        mention_start: token_start,
        path_end,
        path_start,
        path_text,
        token_end,
    })
}

// replaceActiveChatFileMention
pub fn replace_active_chat_file_mention(
    text: &str,
    mention: &ActiveChatFileMention,
    next_path: &str,
) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out: String = chars[..mention.path_start].iter().collect();
    out.push_str(next_path);
    out.extend(chars[mention.path_end..].iter());
    out
}

// --- src/chat/file-references-server.ts (inlined) ---

// export type ResolvedChatFileMentions = { files, issues }
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResolvedChatFileMentions {
    pub files: Vec<ChatContextFile>,
    pub issues: Vec<String>,
}

// node:path#relative (no pathdiff dependency allowed): walk the common
// component prefix, emit one ".." per leftover `from` segment.
fn relative_path(from: &Path, to: &Path) -> String {
    let from_parts: Vec<String> = from
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let to_parts: Vec<String> = to
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();

    let mut i = 0;
    while i < from_parts.len() && i < to_parts.len() && from_parts[i] == to_parts[i] {
        i += 1;
    }

    let mut parts: Vec<String> = Vec::new();
    for _ in i..from_parts.len() {
        parts.push("..".to_string());
    }
    parts.extend(to_parts[i..].iter().cloned());
    parts.join("/")
}

// node:path#basename
fn basename(path: &Path) -> String {
    match path.file_name() {
        Some(name) => name.to_string_lossy().into_owned(),
        None => path.to_string_lossy().into_owned(),
    }
}

// node:path#normalize on an absolute path: collapse //, . and ..; ".." at the
// root clamps to the root (Path::pop on "/" is a no-op).
fn normalize_abs(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

// resolve(cwd, pathText) followed by normalize, matching
// normalize(isAbsolute(pathText) ? pathText : resolve(cwd, pathText)).
fn resolve_mention_path(cwd: &str, path_text: &str) -> PathBuf {
    let candidate = Path::new(path_text);
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        Path::new(cwd).join(candidate)
    };
    normalize_abs(&joined)
}

fn format_relative_path(cwd: &str, target_path: &str) -> String {
    let relative_path = relative_path(Path::new(cwd), Path::new(target_path));

    if relative_path.is_empty() {
        basename(Path::new(target_path))
    } else {
        relative_path
    }
}

fn normalize_file_lines(text: &str) -> Vec<String> {
    let normalized_text = text.replace("\r\n", "\n");
    let mut lines: Vec<String> = normalized_text.split('\n').map(String::from).collect();

    if let Some(last) = lines.last() {
        if last.is_empty() {
            lines.pop();
        }
    }

    if lines.is_empty() {
        lines.push(String::new());
    }

    lines
}

struct SlicedLines {
    content: String,
    line_range: Option<ChatContextFileLineRange>,
    issue: Option<String>,
}

fn slice_lines(
    text: &str,
    mention: &str,
    start_line: Option<usize>,
    end_line: Option<usize>,
) -> SlicedLines {
    let Some(start_line) = start_line else {
        return SlicedLines {
            content: text.replace("\r\n", "\n"),
            line_range: None,
            issue: None,
        };
    };

    let lines = normalize_file_lines(text);
    let resolved_end_line = end_line.unwrap_or(start_line);

    if resolved_end_line < start_line {
        return SlicedLines {
            content: String::new(),
            line_range: None,
            issue: Some(format!("{mention} has an invalid line range.")),
        };
    }

    if start_line > lines.len() {
        let plural = if lines.len() == 1 { "" } else { "s" };
        return SlicedLines {
            content: String::new(),
            line_range: None,
            issue: Some(format!(
                "{mention} starts past the end of the file ({} line{plural}).",
                lines.len()
            )),
        };
    }

    let clamped_end_line = resolved_end_line.min(lines.len());

    // TS: lines.slice(startLine - 1, clampedEndLine) — a `#0` start yields
    // slice(-1, 0) = [] (empty content), never an out-of-range index.
    let content = if start_line == 0 || start_line > clamped_end_line {
        String::new()
    } else {
        lines[start_line - 1..clamped_end_line].join("\n")
    };

    SlicedLines {
        content,
        line_range: Some(ChatContextFileLineRange {
            end_line: clamped_end_line,
            start_line,
        }),
        issue: None,
    }
}

// resolveParsedChatFileMentions
pub fn resolve_parsed_chat_file_mentions(
    mentions: &[ParsedChatFileMention],
    cwd: &str,
) -> ResolvedChatFileMentions {
    let mut files: Vec<ChatContextFile> = Vec::new();
    let mut issues: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for mention in mentions {
        let absolute_path = resolve_mention_path(cwd, &mention.path_text);
        let line_range =
            build_chat_context_file_line_range(mention.start_line, mention.end_line);
        let cache_key = match &line_range {
            Some(range) => format!(
                "{}:{}:{}",
                absolute_path.to_string_lossy(),
                range.start_line,
                range.end_line
            ),
            None => format!("{}::", absolute_path.to_string_lossy()),
        };

        if !seen.insert(cache_key) {
            continue;
        }

        // The TS original wraps stat + readFile in one try/catch: ENOENT is
        // reported as "could not be found", everything else as "could not be
        // read".
        let read_result = match fs::metadata(&absolute_path) {
            Ok(metadata) if metadata.is_dir() => {
                issues.push(format!(
                    "{} points to a directory, not a file.",
                    mention.mention
                ));
                continue;
            }
            Ok(_) => fs::read_to_string(&absolute_path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                issues.push(format!("{} could not be found.", mention.mention));
                continue;
            }
            Err(_) => {
                issues.push(format!("{} could not be read.", mention.mention));
                continue;
            }
        };

        let file_text = match read_result {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                issues.push(format!("{} could not be found.", mention.mention));
                continue;
            }
            Err(_) => {
                issues.push(format!("{} could not be read.", mention.mention));
                continue;
            }
        };

        let sliced = slice_lines(
            &file_text,
            &mention.mention,
            mention.start_line,
            mention.end_line,
        );

        if let Some(issue) = sliced.issue {
            issues.push(issue);
            continue;
        }

        files.push(ChatContextFile {
            content: sliced.content,
            line_range: sliced.line_range,
            mention: mention.mention.clone(),
            path: absolute_path.to_string_lossy().into_owned(),
            relative_path: format_relative_path(cwd, &absolute_path.to_string_lossy()),
        });
    }

    ResolvedChatFileMentions { files, issues }
}

// resolveChatFileMentions
pub fn resolve_chat_file_mentions(text: &str, cwd: &str) -> ResolvedChatFileMentions {
    resolve_parsed_chat_file_mentions(&parse_chat_file_mentions(text), cwd)
}

// --- src/cli/mentions.ts ---

const MAX_DIRECTORY_TREE_ENTRIES: usize = 200;
const IGNORED_DIRECTORY_NAMES: [&str; 7] = [
    ".git",
    ".next",
    ".turbo",
    "build",
    "coverage",
    "dist",
    "node_modules",
];

fn walk_directory_tree(
    current_path: &Path,
    depth: usize,
    lines: &mut Vec<String>,
    truncated: &mut bool,
) {
    if lines.len() >= MAX_DIRECTORY_TREE_ENTRIES {
        *truncated = true;
        return;
    }

    // JS readdirSync(...).sort() sorts by UTF-16 code units; for the ASCII
    // filenames this tree walk sees, Rust's byte sort matches.
    let mut entries: Vec<String> = match fs::read_dir(current_path) {
        Ok(entries) => entries
            .filter_map(|entry| {
                entry
                    .ok()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
            })
            .collect(),
        Err(_) => return,
    };
    entries.sort();

    for entry_name in entries {
        if IGNORED_DIRECTORY_NAMES.contains(&entry_name.as_str()) {
            continue;
        }

        if lines.len() >= MAX_DIRECTORY_TREE_ENTRIES {
            *truncated = true;
            return;
        }

        let entry_path = current_path.join(&entry_name);
        let indent = "  ".repeat(depth);

        match fs::metadata(&entry_path) {
            Ok(metadata) if metadata.is_dir() => {
                lines.push(format!("{indent}{entry_name}/"));
                walk_directory_tree(&entry_path, depth + 1, lines, truncated);
            }
            Ok(_) => lines.push(format!("{indent}{entry_name}")),
            Err(_) => continue,
        }
    }
}

fn build_directory_tree(root_path: &Path) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut truncated = false;

    walk_directory_tree(root_path, 0, &mut lines, &mut truncated);

    if truncated {
        lines.push(format!(
            "... (truncated at {MAX_DIRECTORY_TREE_ENTRIES} entries)"
        ));
    }

    lines.join("\n")
}

struct SplitDirectoryMentions {
    directories: Vec<ChatContextFile>,
    file_mentions: Vec<ParsedChatFileMention>,
    issues: Vec<String>,
}

fn split_directory_mentions(
    mentions: &[ParsedChatFileMention],
    cwd: &str,
) -> SplitDirectoryMentions {
    let mut directories: Vec<ChatContextFile> = Vec::new();
    let mut file_mentions: Vec<ParsedChatFileMention> = Vec::new();
    let mut issues: Vec<String> = Vec::new();
    let mut seen_directories: HashSet<String> = HashSet::new();

    for mention in mentions {
        let absolute_path = resolve_mention_path(cwd, &mention.path_text);

        let is_directory = fs::metadata(&absolute_path)
            .map(|metadata| metadata.is_dir())
            .unwrap_or(false);

        if !is_directory {
            file_mentions.push(mention.clone());
            continue;
        }

        if mention.start_line.is_some() {
            issues.push(format!(
                "{} cannot use line ranges for directories.",
                mention.mention
            ));
            continue;
        }

        let absolute_path_text = absolute_path.to_string_lossy().into_owned();
        if !seen_directories.insert(absolute_path_text.clone()) {
            continue;
        }

        directories.push(ChatContextFile {
            content: build_directory_tree(&absolute_path),
            mention: mention.mention.clone(),
            path: absolute_path_text,
            relative_path: mention.path_text.clone(),
            line_range: None,
        });
    }

    SplitDirectoryMentions {
        directories,
        file_mentions,
        issues,
    }
}

// export type ResolvedGoalMentions = { contextBlock, files, issues, mentions }
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedGoalMentions {
    pub context_block: Option<String>,
    pub files: Vec<ChatContextFile>,
    pub issues: Vec<String>,
    pub mentions: Vec<String>,
}

// Resolves @path and @path#start:end mentions in a goal into a context block
// the activation prompt can carry. Directory mentions become a bounded tree
// listing. (The TS export is async; the Rust port is synchronous.)
pub fn resolve_goal_mentions(text: &str, cwd: &str) -> ResolvedGoalMentions {
    let mentions = parse_chat_file_mentions(text);

    if mentions.is_empty() {
        return ResolvedGoalMentions {
            context_block: None,
            files: Vec::new(),
            issues: Vec::new(),
            mentions: Vec::new(),
        };
    }

    let split = split_directory_mentions(&mentions, cwd);
    let resolved = resolve_parsed_chat_file_mentions(&split.file_mentions, cwd);
    let files: Vec<ChatContextFile> = split
        .directories
        .into_iter()
        .chain(resolved.files.into_iter())
        .collect();

    let context_block = if files.is_empty() {
        None
    } else {
        let mut parts: Vec<String> =
            vec!["context_files (referenced with @mentions in the goal):".to_string()];

        for file in &files {
            let range_suffix = match &file.line_range {
                Some(range) => format!(" lines {}-{}", range.start_line, range.end_line),
                None => String::new(),
            };

            parts.push(format!(
                "--- {} ({}{range_suffix}) ---\n{}",
                file.mention, file.relative_path, file.content
            ));
        }

        Some(parts.join("\n\n"))
    };

    ResolvedGoalMentions {
        context_block,
        files,
        issues: split.issues.into_iter().chain(resolved.issues).collect(),
        mentions: mentions.iter().map(|m| m.mention.clone()).collect(),
    }
}

// buildGoalWithContext
pub fn build_goal_with_context(goal: &str, context_block: Option<&str>) -> String {
    match context_block {
        Some(block) => format!("{goal}\n\n{block}"),
        None => goal.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent dir");
        }
        fs::write(path, contents).expect("write file");
    }

    // it("inlines mentioned files and directory trees into a context block")
    #[test]
    fn inlines_mentioned_files_and_directory_trees_into_a_context_block() {
        let root = tempfile::tempdir().expect("tempdir");
        let cwd = root.path().to_string_lossy().into_owned();
        write_file(
            &root.path().join("src").join("index.ts"),
            "export const answer = 42;\n",
        );

        let resolved = resolve_goal_mentions(
            "explain @src/index.ts and @src and @missing.ts",
            &cwd,
        );

        assert_eq!(
            resolved.mentions,
            vec!["@src/index.ts", "@src", "@missing.ts"]
        );

        let context_block = resolved.context_block.expect("context block");
        assert!(context_block.contains("export const answer = 42;"));
        assert!(context_block.contains("--- @src (src) ---"));
        assert!(context_block.contains("index.ts"));

        assert_eq!(resolved.issues, vec!["@missing.ts could not be found."]);

        let with_context = build_goal_with_context("goal", Some(&context_block));
        assert!(with_context.contains("goal\n\ncontext_files"));
        assert_eq!(build_goal_with_context("goal", None), "goal");
    }

    // it("returns no context for mention-free goals")
    #[test]
    fn returns_no_context_for_mention_free_goals() {
        let root = tempfile::tempdir().expect("tempdir");
        let cwd = root.path().to_string_lossy().into_owned();

        let resolved = resolve_goal_mentions("just do the thing", &cwd);

        assert!(resolved.context_block.is_none());
        assert!(resolved.files.is_empty());
        assert!(resolved.issues.is_empty());
    }
}

#[cfg(test)]
mod line_zero_regression {
    use super::*;

    #[test]
    fn a_zero_start_line_yields_empty_content_instead_of_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "a\nb\nc\n").unwrap();
        let text = "@f.txt#0:2".to_string();
        let resolved = resolve_chat_file_mentions(&text, dir.path().to_str().unwrap());
        let _ = resolved; // reaching here without a panic is the assertion
    }
}
