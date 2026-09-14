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
            "description": "Apply a change to a file and save it to disk. Pass find + replace to swap exact text (every occurrence), or content to create a new file. For an existing file use find + replace on the regions that change — several entries for the same file in one files[] call apply in order — instead of rewriting it: a full rewrite re-sends every line and costs output time proportional to the file (recorded runs spent nine times the output on rewrites as on targeted edits). Rewriting a file of a few dozen lines is fine.",
            "parameters": {
                "additionalProperties": false,
                "properties": {
                    "after": {
                        "description": "With append: the name of a function, test, class or other definition in the file; the appended text is placed right after that definition's block (indented to match) instead of at the end of the file.",
                        "type": "string"
                    },
                    "append": {
                        "description": "Text to add at the end of the file (a newline is inserted first when the file does not end with one; the file is created if missing). For new tests or functions at the end of an existing file this sends only the new lines — content re-sends the whole file and the unchanged lines cost the round their tokens take. Pass after or before with a definition name to place the text next to it.",
                        "type": "string"
                    },
                    "before": {
                        "description": "With append: the name of a definition in the file; the appended text is placed right before it (above its attributes or decorators).",
                        "type": "string"
                    },
                    "content": {
                        "description": "Full file content to write. Creates the file (and parent directories) or overwrites it entirely. Use for new files; to add at the end of an existing file use append; otherwise prefer find + replace unless nearly every line changes or the file is only a few dozen lines.",
                        "type": "string"
                    },
                    "expectedOccurrences": {
                        "description": "How many occurrences of find you expect to replace. Required when find matches more than once; the edit is rejected if the actual count differs.",
                        "type": "number"
                    },
                    "files": {
                        "description": "Apply a batch of file edits in one call: every edit for the task — all files, several entries for the same file, applied in order — so the change lands in one round; a second PATCH call costs a model round (24 of 36 extra PATCH rounds in a recorded bench edited the file the previous round had just patched). All entries are validated before writing; on an ordinary write failure, rollback is attempted and any restoration failures are reported explicitly. This is not crash-atomic across files. Each entry uses content for a full write or find + replace for a targeted edit.",
                        "items": {
                            "additionalProperties": false,
                            "properties": {
                                "after": {
                                    "description": "With append: a definition name in the file; the text is placed right after that definition's block.",
                                    "type": "string"
                                },
                                "append": {
                                    "description": "Text to add at the end of the file (a newline is inserted first when needed; the file is created if missing). Sends only the new lines — use it for tests or functions added at the end of an existing file; pass after or before with a definition name to place it next to that definition.",
                                    "type": "string"
                                },
                                "before": {
                                    "description": "With append: a definition name in the file; the text is placed right before it.",
                                    "type": "string"
                                },
                                "content": {
                                    "description": "Full file content to write. Creates the file (and parent directories) or overwrites it entirely. Use for new files; to add at the end of an existing file use append.",
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
                    "finish": {
                        "description": "Finish the task on this same call when this edit is your last: the harness applies the patch, runs check (the goal's acceptance command or the project's test runner), and marks the task completed with summary — no separate finish_task round. Omit it while more edits follow; a finish on a PATCH that fails comes back with the error.",
                        "properties": {
                            "check": {
                                "description": "Command the harness runs before judging the finish: the goal's acceptance command or the project's test runner.",
                                "type": "string"
                            },
                            "summary": {
                                "description": "One to three short sentences on what was done.",
                                "type": "string"
                            }
                        },
                        "required": ["summary"],
                        "type": "object"
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
// Shared input/result types
// ---------------------------------------------------------------------------

/// One entry in a multi-file transaction, validated by the same rules as
/// single-file edits.
#[derive(Debug, Clone)]
pub struct FileEntry {
    /// Raw path from user input
    pub path: String,
    pub find: Option<String>,
    pub replace: Option<String>,
    pub expected_occurrences: Option<i64>,
    pub content: Option<String>,
    /// Text added at the end of the file (append mode).
    pub append: Option<String>,
    /// Append placed right after the definition block named here.
    pub after: Option<String>,
    /// Append placed right before the definition (and its attributes) named here.
    pub before: Option<String>,
}

/// What execute() hands complete():
#[derive(Debug, Clone)]
pub struct PatchToolResult {
    pub diff: String,
    pub summary: String,
}

/// Validated state for one entry after the validation pass.
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
    /// Lines added at the end of the file (append mode).
    pub appended: Option<usize>,
}

// ---------------------------------------------------------------------------
// Pure text helpers
// ---------------------------------------------------------------------------

/// Count of non-overlapping matches of `find` in `text`.
/// Note appended to a PATCH summary when the find matched only with its
/// indentation ignored.
pub const INDENT_MATCH_NOTE: &str = " (the find text matched only with its indentation ignored; the replacement was re-indented to the file's — check the diff)";

/// Note on a summary when the find text carried JSON escapes as literal
/// characters and matched only once they were decoded.
pub const ESCAPE_MATCH_NOTE: &str = " (the find text carried JSON escapes such as \\u2026 as literal characters; they were decoded to match the file)";

/// Decodes JSON string escapes that survived as literal text (`\u2026`, `\n`,
/// `\t`, `\"`, `\\`). Returns None when the text holds no escape or one is
/// malformed. A recorded big-file run sent the six characters `\u2026` for
/// the `…` its file held, lost the round to "not found", and then spent
/// twenty more repairing the guess.
pub fn decode_literal_escapes(text: &str) -> Option<String> {
    if !text.contains('\\') {
        return None;
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut decoded_any = false;
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('/') => out.push('/'),
            Some('u') => {
                let mut code = String::new();
                for _ in 0..4 {
                    code.push(chars.next()?);
                }
                let mut value = u32::from_str_radix(&code, 16).ok()?;
                if (0xD800..0xDC00).contains(&value) {
                    // A surrogate pair: the low half follows as another \uXXXX.
                    if chars.next()? != '\\' || chars.next()? != 'u' {
                        return None;
                    }
                    let mut low = String::new();
                    for _ in 0..4 {
                        low.push(chars.next()?);
                    }
                    let low = u32::from_str_radix(&low, 16).ok()?;
                    value = 0x10000 + ((value - 0xD800) << 10) + low.checked_sub(0xDC00)?;
                }
                out.push(char::from_u32(value)?);
            }
            _ => return None,
        }
        decoded_any = true;
    }
    decoded_any.then_some(out)
}

/// A find that misses only because its escapes were sent as text: when the
/// decoded find occurs in the file, returns it with the replacement decoded
/// the same way.
pub fn escape_tolerant_match(existing: &str, find: &str, replace: &str) -> Option<(String, String)> {
    let decoded_find = decode_literal_escapes(find)?;
    if count_occurrences(existing, &decoded_find) == 0 {
        return None;
    }
    let decoded_replace = decode_literal_escapes(replace).unwrap_or_else(|| replace.to_string());
    Some((decoded_find, decoded_replace))
}

/// Similarity of two lines as the share of the longer one covered by their
/// common prefix plus common suffix — the shape of a one-token miss.
fn line_similarity(a: &str, b: &str) -> f64 {
    let a: Vec<char> = a.trim().chars().collect();
    let b: Vec<char> = b.trim().chars().collect();
    let longest = a.len().max(b.len());
    if longest == 0 {
        return 0.0;
    }
    let prefix = a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count();
    let shortest = a.len().min(b.len());
    let suffix = a.iter().rev().zip(b.iter().rev()).take_while(|(x, y)| x == y).count().min(shortest - prefix);
    (prefix + suffix) as f64 / longest as f64
}

/// Minimum similarity for a file line to be named as the closest match.
const NEAREST_LINE_MIN_SIMILARITY: f64 = 0.6;


/// Splits a Python file into the text before its trailing `if __name__ ==
/// "__main__":` guard and the guard itself, when that guard is the last
/// top-level statement. An append lands before it: 16 of 31 recorded append
/// entries were indented test methods, and appended after the guard they sat
/// inside it — syntactically valid, never discovered, and the goal's own
/// check still passed.
pub fn split_python_main_guard(text: &str) -> Option<(&str, &str)> {
    let guard_start = text
        .match_indices("if __name__")
        .filter(|(index, _)| *index == 0 || text.as_bytes()[index - 1] == b'\n')
        .map(|(index, _)| index)
        .last()?;
    let guard_line = text[guard_start..].lines().next()?;
    if !(guard_line.contains("__main__") && guard_line.trim_end().ends_with(':')) {
        return None;
    }
    let after_guard_line = guard_start + guard_line.len();
    let tail_has_top_level = text[after_guard_line..]
        .lines()
        .any(|line| !line.trim().is_empty() && !line.starts_with(' ') && !line.starts_with('\t'));
    if tail_has_top_level {
        return None;
    }
    Some((&text[..guard_start], &text[guard_start..]))
}

/// Splits a brace-language file into the text before its trailing run of
/// bare closers (`}`, `});`, `]`, `);` … alone on a column-0 line) and the
/// closers. An indented append lands before them: a recorded claude-web
/// run appended a two-space-indented `test(...)` to a bun test file and it
/// landed after the `describe` block's `});`, outside the block the goal
/// named; the same shape puts a Rust `#[test]` outside `mod tests`.
pub fn split_trailing_closers(text: &str) -> Option<(&str, &str)> {
    let is_closer = |line: &str| {
        let trimmed = line.trim_end();
        !trimmed.is_empty()
            && !trimmed.starts_with(' ')
            && !trimmed.starts_with('\t')
            && trimmed.chars().all(|c| matches!(c, '}' | ')' | ']' | ';' | ','))
    };
    let body = text.trim_end_matches('\n');
    let mut split = body.len();
    let mut closers = 0usize;
    for line in body.rsplit('\n') {
        if !is_closer(line) {
            break;
        }
        closers += 1;
        split -= line.len();
        if split > 0 {
            split -= 1; // the '\n' before this line
        }
    }
    if closers == 0 {
        return None;
    }
    let head = &body[..split];
    if head.trim().is_empty() {
        return None;
    }
    Some((head, &body[split + 1..]))
}

/// Note on the append summary when the text went before trailing closers.
pub const APPEND_BEFORE_CLOSERS_NOTE: &str = " before the file's closing brace(s)";

/// Keywords that open a definition; the word after one is the name.
const DEFINITION_KEYWORDS: &[&str] = &[
    "fn", "def", "class", "struct", "enum", "impl", "trait", "mod", "function", "const", "static", "let", "var", "type",
    "interface", "func", "macro_rules!",
];
/// Modifiers that may precede a definition keyword.
const DEFINITION_MODIFIERS: &[&str] = &[
    "pub", "pub(crate)", "pub(super)", "export", "default", "async", "unsafe", "extern", "private", "public", "protected",
    "abstract", "final", "override", "declare",
];

/// The line (0-based) that defines `name` in `lines`: a definition keyword
/// followed by the name, or a `it("name"` / `test("name"` / `describe("name"`
/// call. Err names the reason when there is no such line or several.
pub fn find_definition_line(lines: &[&str], name: &str) -> Result<usize, String> {
    let is_ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    // A model naming the anchor as the call it reads in the file —
    // `describe("readRegistry")`, `test('x')` — passes the whole expression,
    // not a bare identifier. Match such a line by its literal opening; a
    // recorded dogfood sent `before: describe("readRegistry")` and the
    // strict lookup missed it, so the append fell to the end of the file and
    // cost three more edits to relocate.
    let anchor_is_call_expr = !name.is_empty() && !name.chars().all(|c| is_ident(c)) && name.contains('(');
    let literal = name.trim_end().trim_end_matches(')').trim_end();
    let search = if anchor_is_call_expr { literal } else { name };
    let mut hits: Vec<usize> = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if !trimmed.contains(search) {
            continue;
        }
        let literal_hit = anchor_is_call_expr && !literal.is_empty() && trimmed.starts_with(literal);
        let mut words = trimmed.split_whitespace().peekable();
        while words.peek().is_some_and(|word| DEFINITION_MODIFIERS.contains(word)) {
            words.next();
        }
        let keyword_hit = match (words.next(), words.next()) {
            (Some(keyword), Some(rest)) if DEFINITION_KEYWORDS.contains(&keyword) => {
                let ident: String = rest.chars().take_while(|c| is_ident(*c)).collect();
                ident == name
            }
            _ => false,
        };
        let call_hit = ["it(", "test(", "describe(", "it.only(", "test.only("].iter().any(|opener| {
            trimmed.starts_with(opener) && {
                let rest = trimmed[opener.len()..].trim_start();
                rest.get(1..1 + name.len()) == Some(name)
                    && rest.starts_with(|c| c == '"' || c == '\'' || c == '`')
                    && rest.get(1 + name.len()..).is_some_and(|tail| tail.starts_with(|c| c == '"' || c == '\'' || c == '`'))
            }
        });
        if keyword_hit || call_hit || literal_hit {
            hits.push(index);
        }
    }
    match hits.as_slice() {
        [index] => Ok(*index),
        [] => Err("is not defined in this file".to_string()),
        many => Err(format!("is defined {} times in this file", many.len())),
    }
}

/// `text` with `append` inserted right after (or before) the definition
/// block named by the anchor: the block's end comes from the same brace
/// and indentation walk the outline uses; `before` also steps over the
/// attributes, decorators and doc comments above the definition. An
/// unindented append joins an indented anchor at the anchor's indentation.
/// Ok carries the new text and the line the text now starts on; Err the
/// reason the anchor could not be used.
pub fn anchored_insert(path: &str, text: &str, append: &str, anchor: &str, after: bool) -> Result<(String, usize), String> {
    let lines: Vec<&str> = text.lines().collect();
    let start = find_definition_line(&lines, anchor)?;
    let ext = std::path::Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or("");
    let insert_at = if after {
        crate::harness::outline::definition_end(ext, &lines, start) + 1
    } else {
        let mut at = start;
        while at > 0 {
            let above = lines[at - 1].trim_start();
            if above.starts_with("#[") || above.starts_with('@') || above.starts_with("///") || above.starts_with("/**") || above.starts_with("* ") || above.starts_with("*/") {
                at -= 1;
            } else {
                break;
            }
        }
        at
    };
    let anchor_indent: String = lines[start].chars().take_while(|c| *c == ' ' || *c == '\t').collect();
    let body = append.trim_matches('\n');
    let body_indented = body.lines().find(|line| !line.trim().is_empty()).is_some_and(|line| line.starts_with(' ') || line.starts_with('\t'));
    let body: String = if !anchor_indent.is_empty() && !body_indented {
        body.lines().map(|line| if line.trim().is_empty() { String::new() } else { format!("{anchor_indent}{line}") }).collect::<Vec<_>>().join("\n")
    } else {
        body.to_string()
    };
    let mut out: Vec<String> = lines[..insert_at].iter().map(|line| line.to_string()).collect();
    if after && !out.is_empty() && !out.last().is_some_and(|line| line.trim().is_empty()) {
        out.push(String::new());
    }
    if !after && !out.is_empty() && !out.last().is_some_and(|line| line.trim().is_empty()) {
        out.push(String::new());
    }
    let first_line = out.len() + 1;
    out.extend(body.lines().map(str::to_string));
    if lines.get(insert_at).is_some_and(|line| !line.trim().is_empty()) {
        out.push(String::new());
    }
    out.extend(lines[insert_at..].iter().map(|line| line.to_string()));
    let mut joined = out.join("\n");
    if text.ends_with('\n') || !joined.ends_with('\n') {
        joined.push('\n');
    }
    Ok((joined, first_line))
}

/// Note on the append summary when the text went before the main guard.
pub const APPEND_BEFORE_GUARD_NOTE: &str = " before the `if __name__ == \"__main__\":` block";

/// Why a find text missed, from the file's side. The error alone ("not
/// found") sends the model back to a READ or, worse, to re-sending the same
/// guess. Names the first line of the find text that the file does not hold
/// and, when a file line resembles it, that line: the one token that
/// differs. When every line is present but not as one block, says so (a
/// blank line, an order change, or a hunk an earlier entry already edited).
pub fn nearest_line_hint(existing: &str, find: &str) -> Option<String> {
    let file_lines: Vec<&str> = existing.lines().collect();
    let find_lines: Vec<(usize, &str)> = find.lines().enumerate().filter(|(_, line)| line.trim().chars().count() >= 3).collect();
    let (missing_index, probe) = match find_lines.iter().find(|(_, line)| !file_lines.iter().any(|held| held.trim() == line.trim())) {
        Some(found) => *found,
        None => {
            let (_, first) = *find_lines.first()?;
            let at = file_lines.iter().position(|held| held.trim() == first.trim())? + 1;
            return Some(format!(
                " Every line of the find text is in the file (its first line is line {at}), but not as one contiguous block: check the lines between, or whether an earlier entry in this call already changed that region."
            ));
        }
    };
    let ordinal = if find_lines.len() > 1 { format!(" Line {} of the find text has no match in the file.", missing_index + 1) } else { String::new() };
    let closest = file_lines
        .iter()
        .enumerate()
        .map(|(index, line)| (index, line, line_similarity(line, probe)))
        .max_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal))
        .filter(|(_, _, score)| *score >= NEAREST_LINE_MIN_SIMILARITY)
        .map(|(index, line, _)| {
            let shown: String = line.chars().take(160).collect();
            format!(" The closest line in the file is line {}: `{}`", index + 1, shown.trim_end())
        })
        .unwrap_or_default();
    if ordinal.is_empty() && closest.is_empty() {
        return None;
    }
    Some(format!("{ordinal}{closest}"))
}

pub fn count_occurrences(text: &str, find: &str) -> usize {
    if find.is_empty() {
        return 0;
    }
    text.matches(find).count()
}

/// Leading whitespace of a line.
fn leading_ws(line: &str) -> &str {
    &line[..line.len() - line.trim_start().len()]
}

/// A find that misses only by indentation (a recorded run copied a block
/// at four spaces from a READ window whose file kept it at eight, and lost
/// the round to "not found"): when exactly one window of the file has the
/// same lines once leading and trailing whitespace is ignored, returns the
/// file's own text for that window and the replacement shifted by the same
/// indentation delta (added when the file is deeper, stripped when it is
/// shallower; a tab-vs-space mix leaves the replacement as sent).
pub fn indentation_tolerant_match(existing: &str, find: &str, replace: &str) -> Option<(String, String)> {
    let trailing_newline = find.ends_with('\n');
    let body = if trailing_newline { &find[..find.len() - 1] } else { find };
    let find_lines: Vec<&str> = body.split('\n').collect();
    if !find_lines.iter().any(|line| line.trim().chars().count() >= 3) {
        return None;
    }
    let file_lines: Vec<&str> = existing.split('\n').collect();
    if file_lines.len() < find_lines.len() {
        return None;
    }
    let same = |a: &str, b: &str| a.trim() == b.trim();
    let starts: Vec<usize> = (0..=file_lines.len() - find_lines.len())
        .filter(|&start| find_lines.iter().enumerate().all(|(j, line)| same(file_lines[start + j], line)))
        .collect();
    if starts.len() != 1 {
        return None;
    }
    let start = starts[0];
    let mut actual = file_lines[start..start + find_lines.len()].join("\n");
    if trailing_newline && existing[..].contains(&format!("{actual}\n")) {
        actual.push('\n');
    }
    if actual == find || count_occurrences(existing, &actual) != 1 {
        return None;
    }
    let anchor = find_lines.iter().position(|line| !line.trim().is_empty()).unwrap_or(0);
    let (file_indent, find_indent) = (leading_ws(file_lines[start + anchor]), leading_ws(find_lines[anchor]));
    let reindent = |line: &str| -> String {
        if line.trim().is_empty() {
            return line.to_string();
        }
        if let Some(extra) = file_indent.strip_prefix(find_indent) {
            format!("{extra}{line}")
        } else if let Some(surplus) = find_indent.strip_prefix(file_indent) {
            line.strip_prefix(surplus).map(str::to_string).unwrap_or_else(|| line.to_string())
        } else {
            line.to_string()
        }
    };
    let replaced: Vec<String> = replace.split('\n').map(reindent).collect();
    Some((actual, replaced.join("\n")))
}

/// 1-based line numbers where the find text starts, for honest summaries
/// and actionable multi-site errors.
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

/// Guard against incomplete overwrites: a full-content overwrite of a large
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

    // A fragment sent as "content": the new text opens with an indented line
    // and is much shorter than the file. A recorded run overwrote a 44-line
    // module with a 5-line indented snippet, then spent two rounds restoring
    // it; whole files essentially never start indented.
    if old_lines >= FRAGMENT_GUARD_MIN_LINES
        && new_line_count * FRAGMENT_GUARD_SHRINK_RATIO < old_line_count
        && new_text.lines().find(|line| !line.trim().is_empty()).map_or(false, |line| line.starts_with(' ') || line.starts_with('\t'))
    {
        return Some(format!(
            "the new content starts with an indented line and has {} line(s) against the file's {} — that reads as a fragment meant for find + replace, not a whole file. Send find + replace for the region that changes, or the complete file.",
            new_line_count, old_line_count
        ));
    }

    if old_lines >= SHRINK_GUARD_MIN_LINES
        && new_line_count < old_line_count * SHRINK_GUARD_KEEP_RATIO
    {
        return Some(format!(
            "the new content has {} line(s) but the existing file has {} — a full overwrite that removes more than half of a large file is almost always an accidental elision. Use find + replace to make the intended edit (or delete the removed sections explicitly with find + an empty replace).",
            new_line_count, old_line_count
        ));
    }

    find_duplicated_copy_error(old_text, new_text)
}

const DUPLICATE_GUARD_MIN_LINES: usize = 40;
const DUPLICATE_GUARD_MIN_APPENDED: usize = 20;
const DUPLICATE_GUARD_MIN_LINE_CHARS: usize = 12;
const DUPLICATE_GUARD_RATIO: f64 = 0.8;

/// Guard against duplicated-copy overwrites, the other small-model failure:
/// Overwrites of an existing file where at least this many lines are
/// re-sent unchanged get a note pointing at append / find + replace.
pub const REEMISSION_NOTE_MIN_LINES: usize = 20;

/// Non-blank lines of `new_text` that already stood in `old_text` (as a
/// multiset), i.e. the lines the model re-typed for nothing.
pub fn reemitted_line_count(old_text: &str, new_text: &str) -> usize {
    let mut pool: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for line in old_text.lines().map(str::trim_end).filter(|line| !line.trim().is_empty()) {
        *pool.entry(line).or_insert(0) += 1;
    }
    let mut count = 0usize;
    for line in new_text.lines().map(str::trim_end).filter(|line| !line.trim().is_empty()) {
        if let Some(left) = pool.get_mut(line) {
            if *left > 0 {
                *left -= 1;
                count += 1;
            }
        }
    }
    count
}

/// The note on a whole-file overwrite that re-sent most of an existing
/// file: 39 recorded overwrites re-emitted 59% of their lines unchanged,
/// and one round re-sent a 44-line module four times.
pub fn reemission_note(old_text: &str, new_text: &str) -> String {
    let total = new_text.lines().filter(|line| !line.trim().is_empty()).count();
    let unchanged = reemitted_line_count(old_text, new_text);
    if total >= REEMISSION_NOTE_MIN_LINES && unchanged * 2 >= total {
        format!(
            " — {unchanged} of {total} lines were already in the file; append (for additions at the end) or find + replace sends only the new ones and costs the round less"
        )
    } else {
        String::new()
    }
}

/// "content" = the existing file followed by a second (often lightly edited)
/// copy of it — the model meant to change a hunk and instead appended the
/// whole file again. The tail of such a write is made almost entirely of
/// lines the file already has.
pub fn find_duplicated_copy_error(old_text: &str, new_text: &str) -> Option<String> {
    let old_trimmed = old_text.trim_end();

    if count_lines(old_text) < DUPLICATE_GUARD_MIN_LINES
        || old_trimmed.is_empty()
        || !new_text.starts_with(old_trimmed)
    {
        return None;
    }

    let distinctive = |text: &str| -> Vec<String> {
        text.split('\n')
            .map(|line| line.trim())
            .filter(|line| line.chars().count() >= DUPLICATE_GUARD_MIN_LINE_CHARS)
            .map(|line| line.to_string())
            .collect()
    };
    let existing_lines: std::collections::HashSet<String> = distinctive(old_text).into_iter().collect();
    let appended_lines = distinctive(&new_text[old_trimmed.len()..]);

    if appended_lines.len() < DUPLICATE_GUARD_MIN_APPENDED {
        return None;
    }

    let duplicated = appended_lines.iter().filter(|line| existing_lines.contains(*line)).count();

    if (duplicated as f64) < appended_lines.len() as f64 * DUPLICATE_GUARD_RATIO {
        return None;
    }

    Some(format!(
        "the new content is the existing file followed by {} more line(s), {} of which the file already contains — that appends a second copy instead of editing it. Use find + replace to change the existing text, or write the complete intended file once.",
        appended_lines.len(),
        duplicated
    ))
}

const SHRINK_GUARD_MIN_LINES: usize = 200;
const FRAGMENT_GUARD_MIN_LINES: usize = 10;
const FRAGMENT_GUARD_SHRINK_RATIO: usize = 2; // fragment must be under half the file
const SHRINK_GUARD_KEEP_RATIO: usize = 2; // oldLines * 0.5 == oldLines / 2

/// The elision-marker pattern, compiled once as a lazy static and shared
/// across calls.
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

/// Returns the matched elision-marker substring for use in the error text.
fn find_elision_marker(text: &str) -> Option<String> {
    elision_marker_pattern().find(text).map(|m| m.as_str().to_string())
}

/// Unified old→new diff for `display_path`.
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
// Prepare stage
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
    /// The call carried both content and a find/replace pair; the pair was applied.
    pub stray_content: bool,
}

#[derive(Debug, Clone)]
pub struct PatchToolPrepared {
    pub input: PatchToolInput,
    pub display_input: String,
}

/// A "files" value sent as a JSON-encoded string, decoded when it holds an
/// array. A second attempt repairs the escaping small models produce when
/// they nest JSON in a string — structural quotes written as `\"` around
/// keys and values (`\"path\": \"src/x.rs\"`) — without touching quotes
/// inside find/replace text, which follow other characters.
fn decode_files_string(encoded: &str) -> Option<Value> {
    if let Ok(decoded @ Value::Array(_)) = serde_json::from_str::<Value>(encoded) {
        return Some(decoded);
    }
    // Second attempt: a path value missing one or both quotes — `"path":
    // src/x.rs",` — as a recorded run sent in the second of two entries,
    // which cost the whole call and a round to resend. Well-formed paths
    // re-quote to themselves; this runs before the structural-quote repair
    // because that repair damages quoted text inside find/replace.
    let bare_path = regex::Regex::new(r#"("path"\s*:\s*)"?([^"\s,\}\]][^",\}\]]*)"?(\s*[,\}\]])"#).ok()?;
    let path_fixed = bare_path.replace_all(encoded, "${1}\"${2}\"${3}").into_owned();
    if let Ok(decoded @ Value::Array(_)) = serde_json::from_str::<Value>(&path_fixed) {
        return Some(decoded);
    }
    let opening = regex::Regex::new(r#"([\{\[,:]\s*)\\""#).ok()?;
    let closing = regex::Regex::new(r#"\\"(\s*[:,\}\]])"#).ok()?;
    let opened = opening.replace_all(&path_fixed, "${1}\"").into_owned();
    let repaired = closing.replace_all(&opened, "\"${1}").into_owned();
    match serde_json::from_str::<Value>(&repaired) {
        Ok(decoded @ Value::Array(_)) => Some(decoded),
        _ => None,
    }
}

/// Argument-shape repairs for one PATCH entry (a files[] object or the
/// call's own top-level fields): a missing path inherits `top_path`, else
/// `previous_path`; a non-empty "content" beside a "replace" and no "find"
/// becomes the "find" (the model named the old text "content").
pub fn repair_patch_entry(entry: &mut serde_json::Map<String, Value>, top_path: Option<&str>, previous_path: Option<&str>) {
    let path_missing = entry.get("path").and_then(Value::as_str).map_or(true, str::is_empty);
    if path_missing {
        if let Some(path) = top_path.or(previous_path) {
            entry.insert("path".to_string(), Value::String(path.to_string()));
        }
    }
    // Only when no "find" key was sent at all: an empty find beside content
    // is a placeholder for a whole-file write, not a misnamed pair.
    let content_nonempty = entry.get("content").and_then(Value::as_str).map_or(false, |text| !text.is_empty());
    let find_missing = entry.get("find").is_none();
    let replace_present = entry.get("replace").and_then(Value::as_str).map_or(false, |text| !text.is_empty());
    if content_nonempty && find_missing && replace_present {
        // content echoed as replace (a recorded run sent a whole file under
        // both names) is a whole-file write, not a no-op pair.
        if entry.get("content") == entry.get("replace") {
            entry.remove("replace");
        } else if let Some(content) = entry.remove("content") {
            entry.insert("find".to_string(), content);
        }
    }
}

pub fn prepare(args: &Value, ctx: &ToolCtx) -> Result<PatchToolPrepared> {
    use anyhow::anyhow;
    let mut args = tool_arguments(args)?;
    let workspace_root = ctx.cwd.to_string_lossy().into_owned();

    // Tolerance: a "files" value sent as a JSON-encoded string (a recorded
    // run lost a round to `"files": "[{...}]"`) is decoded when it holds an
    // array; anything else falls through to the usual shape errors.
    if let Some(Value::String(encoded)) = args.get("files") {
        match decode_files_string(encoded) {
            Some(decoded) => {
                args.insert("files".to_string(), decoded);
            }
            None if args.get("path").is_none() => {
                let why = serde_json::from_str::<Value>(encoded).err().map(|e| e.to_string()).unwrap_or_else(|| "not a JSON array".to_string());
                return Err(anyhow!(
                    "\"files\" was sent as a string that is not a valid JSON array ({why}). Send \"files\" as a JSON array of objects, each with \"path\" and either \"content\" or \"find\"/\"replace\" — not as a string."
                ));
            }
            None => {}
        }
    }

    // Tolerance: an entry with no path edits the file the call's top-level
    // "path" names, or failing that the file of the nearest earlier entry
    // (recorded runs sent three find/replace entries under one path, and
    // mixed lists where only the first entry carried the path, and lost a
    // round each to "Missing path"). An entry that sends the old text as
    // "content" beside a "replace" is a find + replace pair under the wrong
    // name; "content" beside a "find" is the replacement (handled below).
    if let Some(Value::Array(entries)) = args.get("files").cloned() {
        let top_path = args.get("path").and_then(Value::as_str).map(str::to_string);
        let mut previous_path: Option<String> = None;
        let filled: Vec<Value> = entries
            .into_iter()
            .map(|mut entry| {
                if let Some(object) = entry.as_object_mut() {
                    repair_patch_entry(object, top_path.as_deref(), previous_path.as_deref());
                    if let Some(path) = object.get("path").and_then(Value::as_str) {
                        previous_path = Some(path.to_string());
                    }
                }
                entry
            })
            .collect();
        args.insert("files".to_string(), Value::Array(filled));
    }

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

            if let Some(append) = entry.get("append").and_then(Value::as_str).filter(|text| !text.is_empty()) {
                // A real find + replace beside an append is two edits to the
                // same file: apply the pair first, then the append (the
                // transaction chains same-path entries). A replace or content
                // with no find is a stray echo of where the model meant to
                // insert; the append alone is what it asked for.
                let find = entry.get("find").and_then(Value::as_str).filter(|text| !text.is_empty());
                let replace = entry.get("replace").and_then(Value::as_str);
                if let (Some(find), Some(replace)) = (find, replace) {
                    files.push(FileEntry {
                        path: path.clone(),
                        find: Some(find.to_string()),
                        replace: Some(replace.to_string()),
                        expected_occurrences: entry.get("expectedOccurrences").and_then(Value::as_i64),
                        content: None,
                        append: None,
                        after: None,
                        before: None,
                    });
                }
                files.push(FileEntry {
                    path: path.clone(),
                    find: None,
                    replace: None,
                    expected_occurrences: None,
                    content: None,
                    append: Some(append.to_string()),
                    after: entry.get("after").and_then(Value::as_str).map(str::trim).filter(|name| !name.is_empty()).map(str::to_string),
                    before: entry.get("before").and_then(Value::as_str).map(str::trim).filter(|name| !name.is_empty()).map(str::to_string),
                });
                continue;
            }
            let content_present = entry.get("content").and_then(Value::as_str).map_or(false, |text| !text.is_empty());
            let find_empty = entry.get("find").and_then(Value::as_str) == Some("");
            let replace_empty = entry.get("replace").and_then(Value::as_str) == Some("");
            // Empty-string find/replace beside a real content are placeholders
            // (recorded runs sent `find: "", replace: ""` with a whole file, and
            // `find: <text>, replace: ""` with the replacement in content); an
            // empty replace beside a real find and no content is a deletion.
            let has_find = entry.get("find").map_or(false, Value::is_string) && !(content_present && find_empty);
            let has_replace = entry.get("replace").map_or(false, Value::is_string) && !(content_present && replace_empty);
            // Small models routinely send `content: ""` as a placeholder next to a
            // real find/replace pair; an empty content beside a find is noise, not
            // an overwrite request (an actual empty overwrite has no find).
            let placeholder_content = has_find && entry.get("content").and_then(Value::as_str) == Some("");
            let has_content = !placeholder_content && entry.get("content").map_or(false, Value::is_string);

            // A non-empty content beside a real find + replace pair is a stray
            // field (small models echo the snippet they are inserting); apply
            // the pair and say so rather than costing a round on the error.
            // Content beside an empty or missing find is still ambiguous.
            let find_nonempty = !entry.get("find").and_then(Value::as_str).unwrap_or("").is_empty();
            // {find, content} with no replace: content is the replacement text.
            let content_as_replace = has_content && has_find && !has_replace && find_nonempty;
            let has_replace = has_replace || content_as_replace;
            let stray_content = has_content && has_find && has_replace && find_nonempty && !content_as_replace;
            let has_content = has_content && !stray_content && !content_as_replace;

            if has_content && (has_find || has_replace) {
                return Err(anyhow!(
                    "[entry {} \"{}\"] Pass either content, or find + replace — not both. Re-send with only find + replace to make the targeted edit, or only content to write the whole file.",
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
                find: if has_find { entry.get("find").and_then(Value::as_str).map(str::to_string) } else { None },
                replace: if content_as_replace {
                    entry.get("content").and_then(Value::as_str).map(str::to_string)
                } else if has_replace {
                    entry.get("replace").and_then(Value::as_str).map(str::to_string)
                } else {
                    None
                },
                expected_occurrences: entry
                    .get("expectedOccurrences")
                    .and_then(Value::as_f64)
                    .map(|n| n as i64),
                content: if content_as_replace { None } else { entry
                    .get("content")
                    .and_then(Value::as_str)
                    .filter(|text| !(text.is_empty() && has_find) && !stray_content)
                    .map(str::to_string) },
                append: None,
                after: None,
                before: None,
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
                stray_content: false,
            },
        });
    }

    let raw_path = get_required_string_argument(&args, "path")?;
    // A single-file append rides the transaction path as its one entry.
    if let Some(append) = args.get("append").and_then(Value::as_str).filter(|text| !text.is_empty()) {
        // Same leniency as the files[] form: a real find + replace runs first
        // as its own chained entry; a stray replace or content is ignored.
        let mut files = Vec::with_capacity(2);
        let find = args.get("find").and_then(Value::as_str).filter(|text| !text.is_empty());
        let replace = args.get("replace").and_then(Value::as_str);
        if let (Some(find), Some(replace)) = (find, replace) {
            files.push(FileEntry {
                path: raw_path.clone(),
                find: Some(find.to_string()),
                replace: Some(replace.to_string()),
                expected_occurrences: args.get("expectedOccurrences").and_then(Value::as_i64),
                content: None,
                append: None,
                after: None,
                before: None,
            });
        }
        files.push(FileEntry {
            path: raw_path.clone(),
            find: None,
            replace: None,
            expected_occurrences: None,
            content: None,
            append: Some(append.to_string()),
            after: args.get("after").and_then(Value::as_str).map(str::trim).filter(|name| !name.is_empty()).map(str::to_string),
            before: args.get("before").and_then(Value::as_str).map(str::trim).filter(|name| !name.is_empty()).map(str::to_string),
        });
        let display_path = format_tool_path(&workspace_root, &resolve_tool_path(&workspace_root, &raw_path));
        return Ok(PatchToolPrepared {
            display_input: format!("{} (append {} line(s))", display_path, append.lines().count()),
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
                stray_content: false,
            },
        });
    }
    // find/replace/content are read raw (not trimmed): leading and trailing whitespace is significant in file edits.
    let find = args.get("find").and_then(Value::as_str).map(str::to_string);
    // `content: ""` beside a find is a placeholder, not an overwrite (see the
    // multi-file entry check).
    let content = args
        .get("content")
        .and_then(Value::as_str)
        .filter(|text| !(text.is_empty() && find.is_some()))
        .map(str::to_string);
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

    // content beside a replace and no find is the old text under the wrong
    // name ({content, replace} was 12 of 110 recorded PATCH failures).
    let (content, find, replace) = match (content, find, replace) {
        (Some(text), None, Some(echo)) if echo == text && !text.is_empty() => (Some(text), None, None),
        (Some(text), None, Some(new)) if !new.is_empty() && !text.is_empty() => (None, Some(text), Some(new)),
        triple => triple,
    };
    // content beside a non-empty find and no replace is the replacement text
    // under the wrong name (recorded runs sent {find, content} for a targeted
    // edit); read it as replace rather than costing a round on the error.
    let (content, replace) = match (content, replace) {
        (Some(text), None) if find.as_deref().map_or(false, |f| !f.is_empty()) => (None, Some(text)),
        pair => pair,
    };
    // A non-empty find beside content is a targeted edit with a stray content
    // field (small models echo the snippet they are inserting); apply the
    // find + replace and say so rather than costing a round on the error.
    let stray_content = content.is_some() && find.as_deref().map_or(false, |text| !text.is_empty()) && replace.is_some();
    let content = if stray_content { None } else { content };
    if content.is_some() && (find.is_some() || replace.is_some()) {
        return Err(anyhow!(
            "Pass either content, or find + replace — not both. Re-send with only find + replace to make the targeted edit, or only content to write the whole file."
        ));
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
    } else if stray_content {
        format!("{} (replace; stray content ignored)", display_path)
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
            stray_content,
        },
    })
}

// ---------------------------------------------------------------------------
// Syntax gate
// ---------------------------------------------------------------------------

// Lowercased extension including the leading dot; "" when there is no
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

// .json targets are validated with a small built-in JSON syntax scan below.
// Returns the first syntax error in the text, or None when it parses (or when
// the file type has no checker available). Syntax checking runs the text
// through the host project's transpiler (spawned via `bun -e`) and reports
// its first diagnostic; if the dependency is unavailable in the host project,
// the gate degrades to a no-op.
pub fn find_syntax_error(file_path: &str, text: &str) -> Option<String> {
    let extension = path_extension(file_path);

    if extension == ".json" {
        // On failure, surface serde_json's parser message — the gate only
        // needs to know whether the text parses.
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
    resolve_file_entry(entry, index, workspace_root, None)
}

/// `base_text` replaces the on-disk pre-image for a find + replace entry: a
/// later entry for the same file in one transaction resolves against the
/// post-image of the earlier one, so several targeted edits to one file can
/// ride in a single PATCH call.
pub fn resolve_file_entry(
    entry: &FileEntry,
    index: usize,
    workspace_root: &str,
    base_text: Option<&str>,
) -> Result<ResolvedEntry, String> {
    let tag = format!("[entry {} \"{}\"]", index, entry.path);

    if let Some(append) = entry.append.as_deref() {
        // --- append mode: the pre-image (chained or on disk) plus the new
        // text at its end; a missing file is created.
        let absolute_path_buf = crate::tools::helpers::resolve_tool_path(workspace_root, &entry.path);
        let display_path = crate::tools::helpers::format_tool_path(workspace_root, &absolute_path_buf);
        let absolute_path = absolute_path_buf.to_string_lossy().to_string();
        let path_kind = match crate::tools::helpers::assert_patch_target_path(&absolute_path_buf, &display_path) {
            Ok(kind) => kind,
            Err(error) => return Err(error.to_string()),
        };
        let existing_text: Option<String> = match base_text {
            Some(text) => Some(text.to_string()),
            None if matches!(path_kind, crate::tools::helpers::ToolPathKind::File) => match std::fs::read_to_string(&absolute_path_buf) {
                Ok(text) => Some(text),
                Err(error) => return Err(format!("{} {}", tag, error)),
            },
            None => None,
        };
        let is_python = std::path::Path::new(&entry.path).extension().and_then(|ext| ext.to_str()) == Some("py");
        let indented = append.lines().find(|line| !line.trim().is_empty()).map_or(false, |line| line.starts_with(' ') || line.starts_with('\t'));
        let guard_split = if is_python { existing_text.as_deref().and_then(split_python_main_guard) } else { None };
        let closers_split = if !is_python && indented { existing_text.as_deref().and_then(split_trailing_closers) } else { None };
        let mut placement_note = "";
        let mut anchor_note = String::new();
        let anchored = match (entry.after.as_deref(), entry.before.as_deref(), existing_text.as_deref()) {
            (Some(name), _, Some(text)) | (None, Some(name), Some(text)) => {
                let after = entry.after.is_some();
                match anchored_insert(&entry.path, text, append, name, after) {
                    Ok((new_text, first_line)) => {
                        anchor_note = format!(" {} `{name}` (the new text starts at line {first_line})", if after { "after" } else { "before" });
                        Some(new_text)
                    }
                    Err(reason) => {
                        anchor_note = format!(" (`{name}` {reason}, so the text went at the end instead)");
                        None
                    }
                }
            }
            _ => None,
        };
        let mut new_text = match (anchored, guard_split, closers_split) {
            (Some(text), _, _) => text,
            (None, None, Some((head, closers))) => {
                // Inside the outermost block: one blank line, the text, the
                // closers as the file had them.
                let mut text = head.trim_end_matches('\n').to_string();
                text.push_str("\n\n");
                text.push_str(append.trim_matches('\n'));
                text.push('\n');
                text.push_str(closers);
                placement_note = APPEND_BEFORE_CLOSERS_NOTE;
                text
            }
            (None, Some((head, guard)), _) => {
                // Before the guard: one blank line for an indented body
                // continuation (a method joining the last class), two for a
                // new top-level definition, then the guard restored after
                // two blank lines as the file had it.
                let mut text = head.trim_end_matches('\n').to_string();
                text.push_str(if indented { "\n\n" } else { "\n\n\n" });
                text.push_str(append.trim_matches('\n'));
                text.push_str("\n\n\n");
                text.push_str(guard);
                placement_note = APPEND_BEFORE_GUARD_NOTE;
                text
            }
            (None, None, None) => {
                let mut text = existing_text.clone().unwrap_or_default();
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str(append);
                text
            }
        };
        if !new_text.ends_with('\n') {
            new_text.push('\n');
        }
        if let Some(existing) = existing_text.as_deref() {
            if let Err(error) = assert_patch_keeps_file_parseable(&display_path, &absolute_path, existing, &new_text) {
                return Err(error);
            }
        }
        let appended = if placement_note.is_empty() { append.lines().count() } else { append.trim_matches('\n').lines().count() };
        return Ok(ResolvedEntry {
            absolute_path,
            display_path,
            content: None,
            effective_find: None,
            effective_replace: None,
            occurrences: None,
            match_lines: None,
            crlf_note: (!placement_note.is_empty() || !anchor_note.is_empty()).then(|| format!("{placement_note}{anchor_note}")),
            existing_text,
            new_text,
            appended: Some(appended),
        });
    }

    // Mutual-exclusion: content XOR find/replace
    if entry.content.is_some() && (entry.find.is_some() || entry.replace.is_some()) {
        return Err(format!(
            "{} Pass either content, or find + replace — not both. Re-send with only find + replace to make the targeted edit, or only content to write the whole file.",
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
            appended: None,
        });
    }

    // --- find/replace mode ---
    if matches!(path_kind, crate::tools::helpers::ToolPathKind::Missing) {
        return Err(format!(
            "{} \"{}\" does not exist. Pass content to create it, or fix the path.",
            tag, display_path
        ));
    }

    let existing_text = match base_text {
        Some(text) => text.to_string(),
        None => match std::fs::read_to_string(&absolute_path_buf) {
            Ok(text) => text,
            Err(error) => return Err(format!("{} {}", tag, error)),
        },
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
        if let Some((actual_find, reindented)) = indentation_tolerant_match(&existing_text, &effective_find, &effective_replace) {
            effective_find = actual_find;
            effective_replace = reindented;
            occurrences = count_occurrences(&existing_text, &effective_find);
            crlf_note = INDENT_MATCH_NOTE.to_string();
        }
    }

    if occurrences == 0 {
        if let Some((decoded_find, decoded_replace)) = escape_tolerant_match(&existing_text, &effective_find, &effective_replace) {
            effective_find = decoded_find;
            effective_replace = decoded_replace;
            occurrences = count_occurrences(&existing_text, &effective_find);
            crlf_note = ESCAPE_MATCH_NOTE.to_string();
        }
    }

    if occurrences == 0 {
        // A find written against the file on disk, when an earlier entry in
        // this call already changed that region (a recorded http-serve run
        // lost two rounds to this: its second entry's find spanned the hunk
        // its first entry had replaced).
        if base_text.is_some() {
            if let Ok(on_disk) = std::fs::read_to_string(&absolute_path_buf) {
                if count_occurrences(&on_disk, &effective_find) > 0 {
                    return Err(format!(
                        "{} The find text matches the file on disk but not the text after the earlier entries in this call for \"{}\": entries to one file apply in order, so write this find against the text those entries leave, or fold both changes into one entry.",
                        tag, display_path
                    ));
                }
            }
        }
        return Err(format!(
            "{} The find text was not found in \"{}\".{} READ the file and pass the exact text, including whitespace.",
            tag,
            display_path,
            nearest_line_hint(&existing_text, &effective_find).unwrap_or_default()
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
        appended: None,
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

/// Resolves a destination to a stable identity for duplicate detection.
/// Existing paths canonicalize through the filesystem (so `./`, `..`, and
/// symlink aliases collapse to the same identity). New paths keep their
/// missing suffix under the nearest existing canonical parent. Existing
/// symlinks resolve before subsequent `..` components, while missing suffixes
/// normalize lexically as the writer would when creating their directories.
fn canonical_destination_identity(absolute_path: &str) -> std::path::PathBuf {
    let path = std::path::Path::new(absolute_path);
    if let Ok(canonical) = path.canonicalize() {
        return canonical;
    }
    let mut identity = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {},
            std::path::Component::ParentDir => { identity.pop(); },
            _ => {
                identity.push(component.as_os_str());
                // Resolve existing symlinks before processing a later `..`.
                // Missing components can be normalized lexically because the
                // writer would create those directories itself.
                if let Ok(canonical) = identity.canonicalize() {
                    identity = canonical;
                }
            }
        }
    }
    identity
}

/// Rejects two entries that resolve to the same destination identity before
/// any write happens. Sequential writes inside one transaction overwrite each
/// other, so the first edit would be silently lost; separate PATCH calls are
/// the simplest correct contract for multiple edits to one file.
fn reject_duplicate_destinations(resolved: &[ResolvedEntry]) -> Result<(), String> {
    let mut identities: std::collections::HashSet<std::path::PathBuf> = std::collections::HashSet::new();
    for entry in resolved {
        let identity = canonical_destination_identity(&entry.absolute_path);
        if !identities.insert(identity) {
            return Err(format!(
                "Transaction rejected: two entries write content to the same file (\"{}\"). Writes in one transaction overwrite each other and the earlier edit would be lost. Use one content write per file; several find + replace entries for one file are fine and apply in order.",
                entry.display_path
            ));
        }
    }
    Ok(())
}

fn execute_transaction(resolved: &[ResolvedEntry], workspace_root: &str) -> Result<(), String> {
    reject_duplicate_destinations(resolved)?;

    // Pre-image per resolved index, captured before that destination's write:
    // Some(bytes) = existing file whose bytes are restored on rollback,
    // None = destination that did not exist yet and is removed on rollback.
    let mut pre_images: std::collections::HashMap<usize, Option<Vec<u8>>> =
        std::collections::HashMap::new();
    // Indices whose writes were attempted, including the failing entry, which
    // may have been partially written.
    let mut attempted: Vec<usize> = Vec::new();
    for (index, entry) in resolved.iter().enumerate() {
        let pre_image = match std::fs::read(&entry.absolute_path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return report_rolled_back_transaction(
                    resolved,
                    &attempted,
                    &pre_images,
                    &entry.display_path,
                    &error.to_string(),
                );
            }
        };
        attempted.push(index);
        pre_images.insert(index, pre_image);
        if let Err(error) = crate::lib_fs::write_file_atomic(
            std::path::Path::new(&entry.absolute_path),
            &entry.new_text,
            true,
        ) {
            return report_rolled_back_transaction(
                resolved,
                &attempted,
                &pre_images,
                &entry.display_path,
                &error.to_string(),
            );
        }
    }

    // Every write succeeded — only now record the journal entries, so a failed
    // transaction never leaves success entries behind.
    for entry in resolved {
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

/// Restores every attempted destination after a failed write and reports the
/// combined failure. Existing files are rewritten with their pre-image bytes
/// (without creating parent directories); destinations that did not exist
/// before the transaction are removed again. Rollback failures are reported
/// next to the original error instead of being swallowed. This is best-effort
/// rollback for ordinary write failures — it makes no claim of
/// filesystem-wide or crash atomicity.
fn report_rolled_back_transaction(
    resolved: &[ResolvedEntry],
    attempted: &[usize],
    pre_images: &std::collections::HashMap<usize, Option<Vec<u8>>>,
    failing_display_path: &str,
    original_error: &str,
) -> Result<(), String> {
    let mut restored: Vec<String> = Vec::new();
    let mut rollback_failures: Vec<String> = Vec::new();
    for &index in attempted.iter().rev() {
        let entry = &resolved[index];
        let outcome = match pre_images.get(&index) {
            Some(Some(bytes)) => match std::str::from_utf8(bytes) {
                Ok(text) => crate::lib_fs::write_file_atomic(
                    std::path::Path::new(&entry.absolute_path),
                    text,
                    false,
                ),
                Err(error) => Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("pre-image is not valid UTF-8: {error}"),
                )),
            },
            _ => match std::fs::remove_file(std::path::Path::new(&entry.absolute_path)) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
            },
        };
        match outcome {
            Ok(()) => restored.push(entry.display_path.clone()),
            Err(error) => rollback_failures.push(format!(
                "\"{}\" could not be restored: {}",
                entry.display_path, error
            )),
        }
    }
    let original = format!(
        "Transaction failed writing \"{}\": {}",
        failing_display_path, original_error
    );
    if rollback_failures.is_empty() {
        Err(format!(
            "{original} Transaction rolled back: {} file(s) restored to their previous contents; the patch journal was not touched.",
            restored.len()
        ))
    } else {
        Err(format!(
            "{original} ROLLBACK INCOMPLETE — {} file(s) may still hold this transaction's content: {}. Inspect these paths and restore them from your own records before continuing; this transaction has no success journal entries.",
            rollback_failures.len(),
            rollback_failures.join("; ")
        ))
    }
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
            // A second find + replace for a file already in this transaction
            // chains onto that entry's post-image and folds into it (one write
            // per file); content writes keep the duplicate-destination rejection.
            let chained = if file_entry.content.is_none() {
                let absolute = resolve_tool_path(&workspace_root, &file_entry.path);
                let identity = canonical_destination_identity(&absolute.to_string_lossy());
                resolved.iter().position(|earlier| earlier.content.is_none() && canonical_destination_identity(&earlier.absolute_path) == identity)
            } else {
                None
            };
            match chained {
                Some(earlier_index) => {
                    let base = resolved[earlier_index].new_text.clone();
                    let next = resolve_file_entry(file_entry, index, &workspace_root, Some(&base))?;
                    let earlier = &mut resolved[earlier_index];
                    earlier.new_text = next.new_text;
                    earlier.occurrences = Some(earlier.occurrences.unwrap_or(0) + next.occurrences.unwrap_or(0));
                    if let Some(appended) = next.appended {
                        earlier.appended = Some(earlier.appended.unwrap_or(0) + appended);
                        if next.crlf_note.is_some() {
                            earlier.crlf_note = next.crlf_note;
                        }
                    }
                    if let Some(lines) = next.match_lines {
                        earlier.match_lines.get_or_insert_with(Vec::new).extend(lines);
                    }
                }
                None => resolved.push(validate_file_entry(file_entry, index, &workspace_root)?),
            }
        }
        execute_transaction(&resolved, &workspace_root)?;
        let mut summary_lines: Vec<String> = Vec::new();
        let mut diff_parts: Vec<String> = Vec::new();
        for entry in &resolved {
            let line_summary = if entry.content.is_some() {
                let verb = if entry.existing_text.is_some() { "Overwrote" } else { "Created" };
                format!(
                    "{}: {} {} line(s){}",
                    entry.display_path,
                    verb.to_lowercase(),
                    crate::tools::helpers::count_lines(&entry.new_text),
                    entry.existing_text.as_deref().map(|existing| reemission_note(existing, &entry.new_text)).unwrap_or_default()
                )
            } else if let Some(appended) = entry.appended {
                let replaced = entry.occurrences.unwrap_or(0);
                format!(
                    "{}: appended {} line(s){}{}{}",
                    entry.display_path,
                    appended,
                    entry.crlf_note.clone().unwrap_or_default(),
                    if replaced > 0 { format!(" after replacing {} occurrence(s)", replaced) } else { String::new() },
                    if entry.existing_text.is_none() { " (file created)" } else { "" }
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

        // Overwrites of an existing parseable file are gated; brand-new files are
        // not (deliberately broken fixtures are a legitimate thing to create).
        if let Some(existing) = existing_text.as_deref() {
            if let Some(incomplete_error) = find_incomplete_overwrite_error(existing, &content) {
                return Err(format!("PATCH rejected: {incomplete_error} The file was left unchanged."));
            }

            assert_patch_keeps_file_parseable(&display_path, &absolute_path, existing, &content)?;
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
            format!(
                "Overwrote {} with {} line(s).{}",
                display_path,
                crate::tools::helpers::count_lines(&content),
                reemission_note(existing_text.as_deref().unwrap_or_default(), &content)
            )
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
        if let Some((actual_find, reindented)) = indentation_tolerant_match(&existing_text, &effective_find, &effective_replace) {
            effective_find = actual_find;
            effective_replace = reindented;
            occurrences = existing_text.matches(&effective_find).count();
            crlf_note = INDENT_MATCH_NOTE.to_string();
        }
    }

    if occurrences == 0 {
        if let Some((decoded_find, decoded_replace)) = escape_tolerant_match(&existing_text, &effective_find, &effective_replace) {
            effective_find = decoded_find;
            effective_replace = decoded_replace;
            occurrences = existing_text.matches(&effective_find).count();
            crlf_note = ESCAPE_MATCH_NOTE.to_string();
        }
    }

    if occurrences == 0 {
        return Err(format!(
            "The find text was not found in \"{}\".{} READ the file and pass the exact text, including whitespace (do not include READ's line-number prefixes).",
            display_path,
            nearest_line_hint(&existing_text, &effective_find).unwrap_or_default()
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
    let summary = if prepared.input.stray_content {
        format!("{summary} The stray \"content\" field was ignored — send only find + replace for a targeted edit.")
    } else {
        summary
    };

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

/// The transcript's display string for this call — the `display_input` value
/// prepare() produces — or None when the arguments do not parse (the
/// execute path reports that error).
pub fn display_input(args: &serde_json::Value, ctx: &ToolCtx) -> Option<String> {
    prepare(args, ctx).ok().map(|prepared| prepared.display_input)
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
    pub(super) fn temp_workspace(tag: &str) -> std::path::PathBuf {
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

    pub(super) fn ctx_for(dir: &std::path::Path) -> ToolCtx {
        ToolCtx {
            cwd: dir.to_path_buf(),
            allow_net: false,
            reference_roots: Vec::new(),
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
    fn append_adds_at_the_end_creates_missing_files_and_chains() {
        let dir = temp_workspace("append");
        let ctx = ctx_for(&dir);
        std::fs::write(dir.join("tests.py"), "import unittest\n\nclass A(unittest.TestCase):\n    pass").unwrap();
        let outcome = execute(&serde_json::json!({"path": "tests.py", "append": "\n\nclass B(unittest.TestCase):\n    pass\n"}), &ctx);
        assert!(!outcome.failed, "{}", outcome.text);
        assert!(outcome.text.starts_with("tests.py: appended 4 line(s)"), "{}", outcome.text);
        let text = std::fs::read_to_string(dir.join("tests.py")).unwrap();
        assert!(text.starts_with("import unittest\n\nclass A(unittest.TestCase):\n    pass\n\n\nclass B"), "{text}");
        assert!(text.ends_with("    pass\n"), "{text}");
        let outcome = execute(&serde_json::json!({"files": [{"path": "new/notes.txt", "append": "first line"}]}), &ctx);
        assert!(!outcome.failed, "{}", outcome.text);
        assert!(outcome.text.contains("appended 1 line(s) (file created)"), "{}", outcome.text);
        assert_eq!(std::fs::read_to_string(dir.join("new/notes.txt")).unwrap(), "first line\n");
        let outcome = execute(&serde_json::json!({"files": [
            {"path": "tests.py", "find": "class A(", "replace": "class A0("},
            {"path": "tests.py", "append": "class C:\n    pass\n"}
        ]}), &ctx);
        assert!(!outcome.failed, "{}", outcome.text);
        assert!(outcome.text.contains("tests.py: appended 2 line(s) after replacing 1 occurrence(s)"), "{}", outcome.text);
        let text = std::fs::read_to_string(dir.join("tests.py")).unwrap();
        assert!(text.contains("class A0(") && text.ends_with("class C:\n    pass\n"), "{text}");
        // A stray replace (the model echoing where it meant to insert) beside
        // an append is ignored; a real find + replace runs first, then the append.
        let outcome = execute(&serde_json::json!({"files": [
            {"path": "tests.py", "append": "class D:\n    pass\n", "replace": "class C:\n    pass\n"}
        ]}), &ctx);
        assert!(!outcome.failed, "{}", outcome.text);
        assert!(outcome.text.starts_with("tests.py: appended 2 line(s)"), "{}", outcome.text);
        let outcome = execute(&serde_json::json!({"path": "tests.py", "append": "class E:\n    pass\n", "find": "class D:", "replace": "class D0:"}), &ctx);
        assert!(!outcome.failed, "{}", outcome.text);
        assert!(outcome.text.contains("tests.py: appended 2 line(s) after replacing 1 occurrence(s)"), "{}", outcome.text);
        let text = std::fs::read_to_string(dir.join("tests.py")).unwrap();
        assert!(text.contains("class D0:") && text.ends_with("class E:\n    pass\n"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn escaped_find_text_is_decoded_and_a_near_miss_names_the_closest_line() {
        assert_eq!(decode_literal_escapes("a \\u2026 b\\n"), Some("a … b\n".to_string()));
        assert_eq!(decode_literal_escapes("plain"), None);
        assert_eq!(decode_literal_escapes("bad \\u12"), None);
        assert_eq!(decode_literal_escapes("\\ud83d\\ude00"), Some("😀".to_string()));
        let dir = temp_workspace("escapes");
        let ctx = ctx_for(&dir);
        std::fs::write(dir.join("t.py"), "def f(text):\n    return text[:left] + \"…\" + text[right:]\n").unwrap();
        let outcome = execute(
            &serde_json::json!({"files": [{"path": "t.py", "find": "    return text[:left] + \"\\u2026\" + text[right:]", "replace": "    return text[:left] + \"\\u2026\" + tail"}]}),
            &ctx,
        );
        assert!(!outcome.failed, "{}", outcome.text);
        assert!(outcome.text.contains("JSON escapes"), "{}", outcome.text);
        assert_eq!(std::fs::read_to_string(dir.join("t.py")).unwrap(), "def f(text):\n    return text[:left] + \"…\" + tail\n");
        let outcome = execute(&serde_json::json!({"path": "t.py", "find": "    return text[:left] + \"...\" + tail", "replace": "x"}), &ctx);
        assert!(outcome.failed, "{}", outcome.text);
        assert!(
            outcome.text.contains("The closest line in the file is line 2: `    return text[:left] + \"…\" + tail`"),
            "{}",
            outcome.text
        );
        let outcome = execute(&serde_json::json!({"path": "t.py", "find": "nothing like this at all", "replace": "x"}), &ctx);
        assert!(outcome.failed && !outcome.text.contains("closest line"), "{}", outcome.text);
        // A later line of the find text is the one that misses.
        let outcome = execute(&serde_json::json!({"path": "t.py", "find": "def f(text):\n    return text[:left] + \"...\" + tail", "replace": "x"}), &ctx);
        assert!(outcome.text.contains("Line 2 of the find text has no match in the file. The closest line in the file is line 2:"), "{}", outcome.text);
        // Every line present, but not contiguous.
        std::fs::write(dir.join("u.py"), "a = 1\n\nb = 2\nc = 3\n").unwrap();
        let outcome = execute(&serde_json::json!({"path": "u.py", "find": "a = 1\nb = 2", "replace": "x"}), &ctx);
        assert!(outcome.text.contains("Every line of the find text is in the file (its first line is line 1), but not as one contiguous block"), "{}", outcome.text);
        // A second entry whose find spans the hunk the first entry replaced.
        let outcome = execute(&serde_json::json!({"files": [
            {"path": "u.py", "find": "b = 2\nc = 3\n", "replace": "bc = 5\n"},
            {"path": "u.py", "find": "c = 3\n", "replace": "c = 4\n"}
        ]}), &ctx);
        assert!(outcome.failed && outcome.text.contains("matches the file on disk but not the text after the earlier entries in this call"), "{}", outcome.text);
        assert_eq!(std::fs::read_to_string(dir.join("u.py")).unwrap(), "a = 1\n\nb = 2\nc = 3\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn append_to_a_python_file_goes_before_the_main_guard() {
        let dir = temp_workspace("append-guard");
        let ctx = ctx_for(&dir);
        let original = "import unittest\n\n\nclass StoreTests(unittest.TestCase):\n    def test_a(self):\n        pass\n\n\nif __name__ == \"__main__\":\n    unittest.main()\n";
        std::fs::write(dir.join("tests/test_store.py"), original).ok();
        std::fs::create_dir_all(dir.join("tests")).unwrap();
        std::fs::write(dir.join("tests/test_store.py"), original).unwrap();
        let outcome = execute(&serde_json::json!({"path": "tests/test_store.py", "append": "\n    def test_b(self):\n        pass\n"}), &ctx);
        assert!(!outcome.failed, "{}", outcome.text);
        assert!(outcome.text.starts_with("tests/test_store.py: appended 2 line(s) before the `if __name__ == \"__main__\":` block"), "{}", outcome.text);
        let text = std::fs::read_to_string(dir.join("tests/test_store.py")).unwrap();
        assert_eq!(
            text,
            "import unittest\n\n\nclass StoreTests(unittest.TestCase):\n    def test_a(self):\n        pass\n\n    def test_b(self):\n        pass\n\n\nif __name__ == \"__main__\":\n    unittest.main()\n"
        );
        // A top-level definition gets two blank lines; a guard that is not
        // the last top-level statement is not a tail.
        let outcome = execute(&serde_json::json!({"path": "tests/test_store.py", "append": "class More(unittest.TestCase):\n    pass\n"}), &ctx);
        assert!(!outcome.failed, "{}", outcome.text);
        let text = std::fs::read_to_string(dir.join("tests/test_store.py")).unwrap();
        assert!(text.contains("        pass\n\n\nclass More(unittest.TestCase):\n    pass\n\n\nif __name__"), "{text}");
        assert!(split_python_main_guard("if __name__ == \"__main__\":\n    main()\n\nx = 1\n").is_none());
        assert!(split_python_main_guard("def f():\n    if __name__ == \"__main__\":\n        pass\n").is_none());
        let outcome = execute(&serde_json::json!({"path": "notes.txt", "append": "if __name__ == x:\n"}), &ctx);
        assert!(!outcome.failed && !outcome.text.contains("block"), "{}", outcome.text);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn append_after_a_named_definition_lands_after_its_block() {
        let dir = temp_workspace("append-anchor");
        let ctx = ctx_for(&dir);
        let src = "mod tests {\n    use super::*;\n\n    #[test]\n    fn alpha() {\n        if true {\n            assert!(true);\n        }\n    }\n\n    #[test]\n    fn beta() {\n        assert!(true);\n    }\n}\n";
        std::fs::write(dir.join("lib.rs"), src).unwrap();
        let outcome = execute(&serde_json::json!({"path": "lib.rs", "after": "alpha", "append": "#[test]\nfn gamma() {\n    assert!(true);\n}\n"}), &ctx);
        assert!(!outcome.failed, "{}", outcome.text);
        assert!(outcome.text.contains("after `alpha` (the new text starts at line 11)"), "{}", outcome.text);
        let text = std::fs::read_to_string(dir.join("lib.rs")).unwrap();
        let expected = "mod tests {\n    use super::*;\n\n    #[test]\n    fn alpha() {\n        if true {\n            assert!(true);\n        }\n    }\n\n    #[test]\n    fn gamma() {\n        assert!(true);\n    }\n\n    #[test]\n    fn beta() {\n        assert!(true);\n    }\n}\n";
        assert_eq!(text, expected);
        // before: above the attribute of the anchor.
        let outcome = execute(&serde_json::json!({"files": [{"path": "lib.rs", "before": "beta", "append": "    fn delta() {}\n"}]}), &ctx);
        assert!(!outcome.failed, "{}", outcome.text);
        let text = std::fs::read_to_string(dir.join("lib.rs")).unwrap();
        assert!(text.contains("    }\n\n    fn delta() {}\n\n    #[test]\n    fn beta() {"), "{text}");
        // Unknown or ambiguous anchor: plain append with the reason in the summary.
        let outcome = execute(&serde_json::json!({"path": "lib.rs", "after": "omega", "append": "// tail\n"}), &ctx);
        assert!(!outcome.failed && outcome.text.contains("`omega` is not defined in this file, so the text went at the end instead"), "{}", outcome.text);
        assert!(std::fs::read_to_string(dir.join("lib.rs")).unwrap().ends_with("}\n// tail\n"));
        // Python: indentation-delimited block, decorator stepped over for before.
        std::fs::write(dir.join("t.py"), "import unittest\n\n\nclass T(unittest.TestCase):\n    def test_a(self):\n        self.assertTrue(True)\n\n    @skip\n    def test_b(self):\n        pass\n").unwrap();
        let outcome = execute(&serde_json::json!({"path": "t.py", "after": "test_a", "append": "def test_mid(self):\n    pass\n"}), &ctx);
        assert!(!outcome.failed, "{}", outcome.text);
        let text = std::fs::read_to_string(dir.join("t.py")).unwrap();
        assert!(text.contains("        self.assertTrue(True)\n\n    def test_mid(self):\n        pass\n\n    @skip\n    def test_b(self):"), "{text}");
        // A call-expression anchor (a describe/test block named as the model
        // reads it) matches by its literal opening, not only a bare name.
        std::fs::write(dir.join("d.test.ts"), "describe(\"a\", () => {\n  test(\"x\", () => {});\n});\n\ndescribe(\"b\", () => {\n  test(\"y\", () => {});\n});\n").unwrap();
        let outcome = execute(&serde_json::json!({"path": "d.test.ts", "before": "describe(\"b\")", "append": "  test(\"z\", () => {});\n"}), &ctx);
        assert!(!outcome.failed, "{}", outcome.text);
        assert!(outcome.text.contains("before `describe(\"b\")`"), "{}", outcome.text);
        let text = std::fs::read_to_string(dir.join("d.test.ts")).unwrap();
        assert!(text.contains("});\n\n  test(\"z\", () => {});\n\ndescribe(\"b\", () => {"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn indented_append_to_a_brace_file_goes_inside_the_outermost_block() {
        let dir = temp_workspace("append-closers");
        let ctx = ctx_for(&dir);
        std::fs::create_dir_all(dir.join("test")).unwrap();
        let ts = "import { test } from \"bun:test\";\n\ndescribe(\"bounds\", () => {\n  test(\"a\", () => {\n    expect(1).toBe(1);\n  });\n});\n";
        std::fs::write(dir.join("test/p.test.ts"), ts).unwrap();
        let outcome = execute(&serde_json::json!({"path": "test/p.test.ts", "append": "\n  test(\"b\", () => {\n    expect(2).toBe(2);\n  });\n"}), &ctx);
        assert!(!outcome.failed, "{}", outcome.text);
        assert!(outcome.text.starts_with("test/p.test.ts: appended 3 line(s) before the file's closing brace(s)"), "{}", outcome.text);
        assert_eq!(
            std::fs::read_to_string(dir.join("test/p.test.ts")).unwrap(),
            "import { test } from \"bun:test\";\n\ndescribe(\"bounds\", () => {\n  test(\"a\", () => {\n    expect(1).toBe(1);\n  });\n\n  test(\"b\", () => {\n    expect(2).toBe(2);\n  });\n});\n"
        );
        // Rust: a #[test] joins mod tests; a top-level (unindented) append stays at the end.
        let rs = "fn f() -> u8 {\n    1\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn a() {\n        assert_eq!(f(), 1);\n    }\n}\n";
        std::fs::write(dir.join("m.rs"), rs).unwrap();
        let outcome = execute(&serde_json::json!({"path": "m.rs", "append": "    #[test]\n    fn b() {\n        assert_eq!(f(), 1);\n    }\n"}), &ctx);
        assert!(!outcome.failed, "{}", outcome.text);
        let text = std::fs::read_to_string(dir.join("m.rs")).unwrap();
        assert!(text.ends_with("    }\n\n    #[test]\n    fn b() {\n        assert_eq!(f(), 1);\n    }\n}\n"), "{text}");
        let outcome = execute(&serde_json::json!({"path": "m.rs", "append": "fn g() -> u8 {\n    2\n}\n"}), &ctx);
        assert!(!outcome.failed && !outcome.text.contains("closing brace"), "{}", outcome.text);
        assert!(std::fs::read_to_string(dir.join("m.rs")).unwrap().ends_with("}\nfn g() -> u8 {\n    2\n}\n"));
        assert!(split_trailing_closers("}\n").is_none());
        assert_eq!(split_trailing_closers("a {\n  b\n});\n]\n"), Some(("a {\n  b", "});\n]")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn overwrite_that_resends_most_of_a_file_says_so() {
        use super::{reemission_note, reemitted_line_count};
        let old: String = (1..=30).map(|i| format!("line {i}\n")).collect();
        let mut new = old.clone();
        new.push_str("line 31\nline 32\n");
        assert_eq!(reemitted_line_count(&old, &new), 30);
        assert!(reemission_note(&old, &new).contains("30 of 32 lines were already in the file"), "{}", reemission_note(&old, &new));
        let rewritten: String = (1..=30).map(|i| format!("new {i}\n")).collect();
        assert_eq!(reemission_note(&old, &rewritten), "", "a real rewrite gets no note");
        assert_eq!(reemission_note("a\nb\n", "a\nb\nc\n"), "", "short files get no note");
        let dir = temp_workspace("reemit");
        let ctx = ctx_for(&dir);
        std::fs::write(dir.join("m.py"), &old).unwrap();
        let outcome = execute(&serde_json::json!({"path": "m.py", "content": new}), &ctx);
        assert!(!outcome.failed && outcome.text.contains("were already in the file; append"), "{}", outcome.text);
        let _ = std::fs::remove_dir_all(&dir);
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

    #[test]
    fn same_path_edits_in_one_transaction_apply_in_order() {
        let workspace = temp_workspace("dup-same-path");
        let ctx = ctx_for(&workspace);
        let file = workspace.join("notes.txt");
        let original = "alpha\nbeta\ngamma\n";
        std::fs::write(&file, original).unwrap();

        let outcome = execute(
            &serde_json::json!({
                "files": [
                    { "path": "notes.txt", "find": "alpha", "replace": "ONE" },
                    { "path": "notes.txt", "find": "gamma", "replace": "THREE" }
                ]
            }),
            &ctx,
        );

        assert!(!outcome.failed, "{}", outcome.text);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "ONE\nbeta\nTHREE\n");
    }

    #[test]
    fn missing_directory_parent_components_cannot_hide_duplicate_destinations() {
        let workspace = temp_workspace("dup-missing-parent");
        let ctx = ctx_for(&workspace);
        let first = workspace.join("a.txt");
        let alias = workspace.join("new/../a.txt");
        let outcome = execute(&serde_json::json!({"files":[
            {"path":first,"content":"first\n"},
            {"path":alias,"content":"second\n"}
        ]}), &ctx);
        assert!(outcome.failed, "{}", outcome.text);
        assert!(outcome.text.contains("same file"));
        assert!(!first.exists());
        assert!(!workspace.join("new").exists());
    }

    #[test]
    fn an_indented_fragment_does_not_overwrite_a_module() {
        let module: String = (1..=44).map(|i| format!("def f{i}():\n    return {i}\n")).collect();
        let fragment = "    p_set = sub.add_parser(\"set\")\n    p_set.add_argument(\"key\")\n    p_set.add_argument(\"value\")\n";
        let error = find_incomplete_overwrite_error(&module, fragment).expect("fragment rejected");
        assert!(error.contains("starts with an indented line"), "{error}");
        // A complete short rewrite (unindented first line) is still allowed.
        assert!(find_incomplete_overwrite_error(&module, "import os\n\ndef f1():\n    return 1\n").is_none());
        // A short file rewritten with an indented first line is not guarded.
        assert!(find_incomplete_overwrite_error("a\nb\nc\n", "    x\n").is_none());
    }

    #[test]
    fn empty_find_and_replace_placeholders_beside_content_are_ignored() {
        let workspace = temp_workspace("placeholders-beside-content");
        let ctx = ctx_for(&workspace);
        let whole = workspace.join("whole.txt");
        let edited = workspace.join("edited.txt");
        std::fs::write(&edited, "alpha\nbeta\n").unwrap();
        let outcome = execute(&serde_json::json!({"files": [
            {"path": whole, "content": "new file\n", "find": "", "replace": ""},
            {"path": edited, "find": "beta", "replace": "", "content": "gamma"}
        ]}), &ctx);
        assert!(!outcome.failed, "{}", outcome.text);
        assert_eq!(std::fs::read_to_string(&whole).unwrap(), "new file\n");
        assert_eq!(std::fs::read_to_string(&edited).unwrap(), "alpha\ngamma\n");
        // A deletion (find + empty replace, no content) still deletes.
        let outcome = execute(&serde_json::json!({"files": [{"path": edited, "find": "gamma\n", "replace": ""}]}), &ctx);
        assert!(!outcome.failed, "{}", outcome.text);
        assert_eq!(std::fs::read_to_string(&edited).unwrap(), "alpha\n");
    }

    #[test]
    fn a_top_level_path_fills_entries_that_carry_none() {
        let workspace = temp_workspace("top-level-path");
        let ctx = ctx_for(&workspace);
        let file = workspace.join("notes.txt");
        std::fs::write(&file, "alpha\nbeta\ngamma\n").unwrap();
        let outcome = execute(&serde_json::json!({"path": file, "files": [
            {"find": "alpha", "replace": "ALPHA"},
            {"find": "gamma", "replace": "GAMMA"}
        ]}), &ctx);
        assert!(!outcome.failed, "{}", outcome.text);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "ALPHA\nbeta\nGAMMA\n");
    }

    #[test]
    fn a_files_string_with_escaped_structural_quotes_is_repaired() {
        // Structural quotes doubled by the model; the find text keeps its own quoted `"x"`.
        let encoded = r#"[{"find": "let a = \"x\";", \"path\": \"notes.rs\", \"replace\": \"let a = 1;\"}]"#;
        let decoded = decode_files_string(encoded).expect("repaired");
        let entry = &decoded.as_array().unwrap()[0];
        assert_eq!(entry["path"], "notes.rs");
        assert_eq!(entry["find"], "let a = \"x\";");
        assert_eq!(entry["replace"], "let a = 1;");
        assert!(decode_files_string("not json at all").is_none());
        // A bare path value in one entry (recorded run: `"path": src/harness/loop.rs}`).
        let bare = r#"[{"find": "a", "path": "src/a.rs", "replace": "b"}, {"find": "c", "path": src/harness/loop.rs, "replace": "d"}]"#;
        let decoded = decode_files_string(bare).expect("bare path quoted");
        assert_eq!(decoded.as_array().unwrap()[1]["path"], "src/harness/loop.rs");
        assert_eq!(decoded.as_array().unwrap()[1]["replace"], "d");
        // The recorded shape: only the opening quote missing, and quoted text
        // (`, \"jest\"`) inside the find that the structural repair must not touch.
        let half = r#"[{"find": "(\"jest\", \"jest\"),", "path": "src/a.rs", "replace": "x"}, {"find": "c", "path": src/harness/loop.rs", "replace": "d"}]"#;
        let decoded = decode_files_string(half).expect("half-quoted path repaired");
        assert_eq!(decoded.as_array().unwrap()[0]["find"], "(\"jest\", \"jest\"),");
        assert_eq!(decoded.as_array().unwrap()[1]["path"], "src/harness/loop.rs");
    }

    #[test]
    fn an_undecodable_files_string_reports_the_shape_not_a_missing_path() {
        let workspace = temp_workspace("files-string-bad");
        let ctx = ctx_for(&workspace);
        let outcome = execute(&serde_json::json!({"files": "[{oops"}), &ctx);
        assert!(outcome.failed);
        assert!(outcome.text.contains("not a valid JSON array"), "{}", outcome.text);
        assert!(outcome.text.contains("Send \"files\" as a JSON array"), "{}", outcome.text);
    }

    #[test]
    fn a_files_array_sent_as_a_json_string_is_decoded() {
        let workspace = temp_workspace("files-as-string");
        let ctx = ctx_for(&workspace);
        let file = workspace.join("notes.txt");
        std::fs::write(&file, "alpha\nbeta\n").unwrap();
        let encoded = serde_json::json!([{"path": file, "find": "beta", "replace": "gamma"}]).to_string();
        let outcome = execute(&serde_json::json!({"files": encoded}), &ctx);
        assert!(!outcome.failed, "{}", outcome.text);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "alpha\ngamma\n");
    }

    #[test]
    fn same_file_find_replace_entries_chain_in_one_transaction() {
        let workspace = temp_workspace("dup-dot-slash");
        let ctx = ctx_for(&workspace);
        let file = workspace.join("notes.txt");
        std::fs::write(&file, "alpha\nbeta\ngamma\n").unwrap();

        // Two find + replace entries for one file (here through a ./ alias)
        // chain in order and land as a single write.
        let outcome = execute(
            &serde_json::json!({
                "files": [
                    { "path": "notes.txt", "find": "alpha", "replace": "ONE" },
                    { "path": "./notes.txt", "find": "gamma", "replace": "THREE" }
                ]
            }),
            &ctx,
        );
        assert!(!outcome.failed, "{}", outcome.text);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "ONE\nbeta\nTHREE\n");
        assert!(outcome.text.contains("replaced 2 occurrence(s)"), "{}", outcome.text);
    }

    #[test]
    fn content_write_plus_edit_on_one_file_is_still_rejected() {
        let workspace = temp_workspace("dup-content");
        let ctx = ctx_for(&workspace);
        let file = workspace.join("notes.txt");
        std::fs::write(&file, "alpha\n").unwrap();
        let outcome = execute(
            &serde_json::json!({
                "files": [
                    { "path": "notes.txt", "content": "fresh\n" },
                    { "path": "notes.txt", "find": "alpha", "replace": "ONE" }
                ]
            }),
            &ctx,
        );
        assert!(outcome.failed, "{}", outcome.text);
        assert!(outcome.text.contains("write content to the same file"), "{}", outcome.text);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "alpha\n");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_alias_entries_chain_onto_the_real_file() {
        let workspace = temp_workspace("dup-symlink");
        let ctx = ctx_for(&workspace);
        let real = workspace.join("real.txt");
        let original = "alpha\nbeta\ngamma\n";
        std::fs::write(&real, original).unwrap();
        std::os::unix::fs::symlink("real.txt", workspace.join("link.txt")).expect("create symlink");

        let outcome = execute(
            &serde_json::json!({
                "files": [
                    { "path": "real.txt", "find": "alpha", "replace": "ONE" },
                    { "path": "link.txt", "find": "gamma", "replace": "THREE" }
                ]
            }),
            &ctx,
        );

        assert!(!outcome.failed, "{}", outcome.text);
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "ONE\nbeta\nTHREE\n");
    }

    #[cfg(unix)]
    #[test]
    fn new_files_under_aliased_parent_dirs_are_rejected() {
        let workspace = temp_workspace("dup-aliased-parent");
        let ctx = ctx_for(&workspace);
        std::fs::create_dir(workspace.join("real_dir")).expect("create real dir");
        std::os::unix::fs::symlink("real_dir", workspace.join("link_dir"))
            .expect("create dir symlink");
        let content_one = "first\n";
        let content_two = "second\n";

        let outcome = execute(
            &serde_json::json!({
                "files": [
                    { "path": "real_dir/new.txt", "content": content_one },
                    { "path": "link_dir/new.txt", "content": content_two }
                ]
            }),
            &ctx,
        );

        assert!(
            !workspace.join("real_dir/new.txt").exists(),
            "rejection must happen before any write: no file may be created"
        );
        assert!(
            outcome.failed,
            "two new files under aliased parents must be rejected, got success: {}",
            outcome.text
        );
        assert!(
            outcome.text.contains("write content to the same file"),
            "error must name the duplicate content write, got: {}",
            outcome.text
        );
        let _ = (content_one, content_two);
    }

    #[test]
    fn distinct_files_in_one_transaction_both_edits_land() {
        let workspace = temp_workspace("distinct-files");
        let ctx = ctx_for(&workspace);
        let a = workspace.join("a.txt");
        let b = workspace.join("b.txt");
        std::fs::write(&a, "alpha\n").unwrap();
        std::fs::write(&b, "bravo\n").unwrap();

        let outcome = execute(
            &serde_json::json!({
                "files": [
                    { "path": "a.txt", "find": "alpha", "replace": "ONE" },
                    { "path": "b.txt", "find": "bravo", "replace": "TWO" }
                ]
            }),
            &ctx,
        );

        assert!(
            !outcome.failed,
            "distinct files must patch in one transaction, got: {}",
            outcome.text
        );
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "ONE\n");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "TWO\n");
    }

    #[test]
    fn two_new_files_under_one_missing_parent_are_distinct_and_both_land() {
        let workspace = temp_workspace("distinct-new-under-missing-parent");
        let ctx = ctx_for(&workspace);
        let first = workspace.join("new/a.txt");
        let second = workspace.join("new/b.txt");

        let outcome = execute(
            &serde_json::json!({
                "files": [
                    { "path": "new/a.txt", "content": "alpha\n" },
                    { "path": "new/b.txt", "content": "bravo\n" }
                ]
            }),
            &ctx,
        );

        assert!(
            !outcome.failed,
            "two distinct new files under one missing parent must both be created, got: {}",
            outcome.text
        );
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "alpha\n");
        assert_eq!(std::fs::read_to_string(&second).unwrap(), "bravo\n");
    }

    #[test]
    fn two_new_files_under_a_two_level_missing_parent_are_distinct() {
        let workspace = temp_workspace("distinct-new-two-level-missing-parent");
        let ctx = ctx_for(&workspace);
        let first = workspace.join("deep/er/a.txt");
        let second = workspace.join("deep/er/b.txt");

        let outcome = execute(
            &serde_json::json!({
                "files": [
                    { "path": "deep/er/a.txt", "content": "alpha\n" },
                    { "path": "deep/er/b.txt", "content": "bravo\n" }
                ]
            }),
            &ctx,
        );

        assert!(
            !outcome.failed,
            "two distinct new files under a two-level missing parent must both be created, got: {}",
            outcome.text
        );
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "alpha\n");
        assert_eq!(std::fs::read_to_string(&second).unwrap(), "bravo\n");
    }

    #[test]
    fn later_write_failure_rolls_back_earlier_existing_file_and_journals_nothing() {
        let workspace = temp_workspace("rollback-existing");
        let first = workspace.join("first.txt");
        let original = "original first\n";
        std::fs::write(&first, original).unwrap();
        // A file where the second entry's parent directory would be: the
        // second write cannot create its parent, so the transaction fails
        // deterministically without touching permissions root bypasses.
        std::fs::write(workspace.join("doomed"), "blocker\n").unwrap();

        let resolved = vec![
            ResolvedEntry {
                absolute_path: first.to_string_lossy().to_string(),
                display_path: "first.txt".to_string(),
                content: Some(String::new()),
                effective_find: None,
                effective_replace: None,
                occurrences: None,
                match_lines: None,
                crlf_note: None,
                existing_text: Some(original.to_string()),
                new_text: "rewritten first\n".to_string(),
                appended: None,
            },
            ResolvedEntry {
                absolute_path: workspace.join("doomed/child.txt").to_string_lossy().to_string(),
                display_path: "doomed/child.txt".to_string(),
                content: Some(String::new()),
                effective_find: None,
                effective_replace: None,
                occurrences: None,
                match_lines: None,
                crlf_note: None,
                existing_text: None,
                new_text: "never lands\n".to_string(),
                appended: None,
            },
        ];

        let error = execute_transaction(&resolved, &workspace.to_string_lossy())
            .expect_err("second write must fail; the transaction must roll back");

        assert!(
            error.contains("doomed/child.txt"),
            "error must name the failing entry, got: {error}"
        );
        assert!(
            error.contains("rolled back"),
            "error must report the rollback, got: {error}"
        );
        assert_eq!(
            std::fs::read_to_string(&first).unwrap(),
            original,
            "earlier existing file must be restored byte-for-byte"
        );
        let journal_path = crate::tools::patch_journal::patch_journal_path(&workspace);
        assert!(
            !journal_path.exists(),
            "failed transaction must not leave success journal entries"
        );
    }

    #[test]
    fn later_write_failure_removes_newly_created_files_and_journals_nothing() {
        let workspace = temp_workspace("rollback-new-file");
        let created = workspace.join("created.txt");
        // A file where the second entry's parent directory would be, so the
        // failing write never lands and the earlier new file must be removed.
        std::fs::write(workspace.join("blocker"), "blocker\n").unwrap();

        let resolved = vec![
            ResolvedEntry {
                absolute_path: created.to_string_lossy().to_string(),
                display_path: "created.txt".to_string(),
                content: Some(String::new()),
                effective_find: None,
                effective_replace: None,
                occurrences: None,
                match_lines: None,
                crlf_note: None,
                existing_text: None,
                new_text: "created then rolled back\n".to_string(),
                appended: None,
            },
            ResolvedEntry {
                absolute_path: workspace.join("blocker/doomed.txt").to_string_lossy().to_string(),
                display_path: "blocker/doomed.txt".to_string(),
                content: Some(String::new()),
                effective_find: None,
                effective_replace: None,
                occurrences: None,
                match_lines: None,
                crlf_note: None,
                existing_text: None,
                new_text: "never lands\n".to_string(),
                appended: None,
            },
        ];

        let error = execute_transaction(&resolved, &workspace.to_string_lossy())
            .expect_err("second write must fail; the transaction must roll back");

        assert!(
            error.contains("blocker/doomed.txt"),
            "error must name the failing entry, got: {error}"
        );
        assert!(
            !created.exists(),
            "newly created file must be removed by the rollback"
        );
        let journal_path = crate::tools::patch_journal::patch_journal_path(&workspace);
        assert!(
            !journal_path.exists(),
            "failed transaction must not leave success journal entries"
        );
    }

    #[test]
    fn rollback_failure_is_reported_explicitly() {
        let workspace = temp_workspace("rollback-failure");
        let resolved = vec![ResolvedEntry {
            absolute_path: workspace.join("gone/victim.txt").to_string_lossy().to_string(),
            display_path: "gone/victim.txt".to_string(),
            content: Some(String::new()),
            effective_find: None,
            effective_replace: None,
            occurrences: None,
            match_lines: None,
            crlf_note: None,
            existing_text: Some("previous\n".to_string()),
            new_text: "rewritten\n".to_string(),
            appended: None,
        }];
        let mut pre_images: std::collections::HashMap<usize, Option<Vec<u8>>> =
            std::collections::HashMap::new();
        pre_images.insert(0usize, Some(b"previous\n".to_vec()));

        let error = report_rolled_back_transaction(
            &resolved,
            &[0usize],
            &pre_images,
            "gone/victim.txt",
            "parent directory vanished",
        )
        .expect_err("restore must fail when the destination's parent directory is gone");

        assert!(
            error.contains("ROLLBACK INCOMPLETE"),
            "rollback failure must be explicit, got: {error}"
        );
        assert!(
            error.contains("gone/victim.txt"),
            "rollback failure must name the destination, got: {error}"
        );
    }
}

#[cfg(test)]
mod duplicate_guard_tests {
    use super::*;

    fn sixty_lines() -> String {
        (0..60).map(|i| format!("export const value{i} = {i}; // keep this line\n")).collect()
    }

    #[test]
    fn rejects_an_overwrite_that_appends_a_second_copy() {
        let original = sixty_lines();
        let second_copy = original.replace("value3 = 3", "value3 = 30");
        let error = find_incomplete_overwrite_error(&original, &format!("{original}{second_copy}"))
            .expect("duplicate copy rejected");
        assert!(error.contains("appends a second copy"), "{error}");
    }

    #[test]
    fn allows_an_overwrite_that_appends_new_lines() {
        let original = sixty_lines();
        let additions: String = (0..30).map(|i| format!("export const extra{i} = value{i} * 2; // new\n")).collect();
        assert_eq!(find_incomplete_overwrite_error(&original, &format!("{original}{additions}")), None);
    }
}

#[cfg(test)]
mod prepare_tests {
    use super::*;

    fn test_ctx() -> ToolCtx {
        ToolCtx {
            cwd: std::env::temp_dir(),
            allow_net: false,
            reference_roots: Vec::new(),
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

    #[test]
    fn multi_file_stray_content_beside_find_replace_applies_the_pair() {
        let prepared = prepare(
            &json!({
                "files": [
                    {
                        "path": "a.txt",
                        "content": "hello\n",
                        "find": "x",
                        "replace": "y"
                    }
                ]
            }),
            &test_ctx(),
        )
        .unwrap();
        assert_eq!(prepared.input.files.len(), 1);
        assert_eq!(prepared.input.files[0].content, None, "the stray content is dropped");
        assert_eq!(prepared.input.files[0].find.as_deref(), Some("x"));
        assert_eq!(prepared.input.files[0].replace.as_deref(), Some("y"));
    }

    #[test]
    fn content_plus_empty_find_is_rejected_with_actionable_guidance() {
        let err = prepare(
            &json!({
                "files": [
                    {
                        "path": "a.txt",
                        "content": "hello\n",
                        "find": "",
                        "replace": "y"
                    }
                ]
            }),
            &test_ctx(),
        )
        .unwrap_err();

        assert_eq!(
            err.to_string(),
            "[entry 0 \"a.txt\"] Pass either content, or find + replace — not both. Re-send with only find + replace to make the targeted edit, or only content to write the whole file."
        );
    }

    #[test]
    fn single_file_content_plus_find_replace_applies_the_pair() {
        let prepared = prepare(
            &json!({
                "path": "a.txt",
                "content": "x",
                "find": "x",
                "replace": "y"
            }),
            &test_ctx(),
        )
        .unwrap();
        assert!(prepared.input.stray_content, "stray content is flagged");
        assert_eq!(prepared.input.content, None, "the stray content is dropped");
        assert_eq!(prepared.input.find.as_deref(), Some("x"));
        assert!(prepared.display_input.ends_with("(replace; stray content ignored)"), "{}", prepared.display_input);
    }

    #[test]
    fn content_beside_find_without_replace_is_the_replacement() {
        let prepared = prepare(&json!({ "path": "a.txt", "find": "old", "content": "new" }), &test_ctx()).unwrap();
        assert_eq!(prepared.input.content, None);
        assert_eq!(prepared.input.find.as_deref(), Some("old"));
        assert_eq!(prepared.input.replace.as_deref(), Some("new"));
        assert!(!prepared.input.stray_content);
        let multi = prepare(&json!({ "files": [{ "path": "a.txt", "find": "old", "content": "new" }] }), &test_ctx()).unwrap();
        assert_eq!(multi.input.files[0].content, None);
        assert_eq!(multi.input.files[0].replace.as_deref(), Some("new"));
    }

    #[test]
    fn single_file_content_plus_empty_find_is_still_rejected() {
        let err = prepare(
            &json!({ "path": "a.txt", "content": "x", "find": "", "replace": "y" }),
            &test_ctx(),
        )
        .unwrap_err();
        assert!(err.to_string().starts_with("Pass either content, or find + replace"), "{err}");
    }

    #[test]
    fn a_find_that_misses_only_by_indentation_matches_and_reindents_the_replacement() {
        let workspace = super::execute_tests::temp_workspace("indent-tolerant");
        let ctx = super::execute_tests::ctx_for(&workspace);
        let file = workspace.join("mod.py");
        std::fs::write(&file, "class A:\n    def f(self):\n        x = 1\n        return x\n").unwrap();
        let outcome = execute(
            &json!({"path": "mod.py", "find": "    x = 1\n    return x\n", "replace": "    x = 2\n    return x\n"}),
            &ctx,
        );
        assert!(!outcome.failed, "{}", outcome.text);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "class A:\n    def f(self):\n        x = 2\n        return x\n");
        assert!(outcome.text.contains("indentation ignored"), "{}", outcome.text);
        // Deeper find than file: the surplus is stripped from the replacement.
        let outcome = execute(
            &json!({"files": [{"path": "mod.py", "find": "            x = 2", "replace": "            x = 3"}]}),
            &ctx,
        );
        assert!(!outcome.failed, "{}", outcome.text);
        assert!(std::fs::read_to_string(&file).unwrap().contains("        x = 3\n"));
        // Ambiguous windows stay "not found".
        std::fs::write(&file, "a\n    b\nc\n        b\n").unwrap();
        let outcome = execute(&json!({"path": "mod.py", "find": "  bbb", "replace": "z"}), &ctx);
        assert!(outcome.failed);
        assert!(indentation_tolerant_match("a\n    b\nc\n        b\n", "  b", "z").is_none(), "two windows and a too-short line");
        assert!(indentation_tolerant_match("    let value = 1;\n", "let value = 1;", "let value = 2;").is_some());
    }

    #[test]
    fn content_beside_replace_is_the_find_and_pathless_entries_inherit_a_path() {
        let workspace = super::execute_tests::temp_workspace("repair-shapes");
        let ctx = super::execute_tests::ctx_for(&workspace);
        std::fs::write(workspace.join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(workspace.join("b.txt"), "three\n").unwrap();
        let outcome = execute(&json!({"path": "a.txt", "content": "one", "replace": "uno"}), &ctx);
        assert!(!outcome.failed, "{}", outcome.text);
        assert_eq!(std::fs::read_to_string(workspace.join("a.txt")).unwrap(), "uno\ntwo\n");
        // The same text under both names is a whole-file write.
        let outcome = execute(&json!({"path": "c.txt", "content": "whole\n", "replace": "whole\n"}), &ctx);
        assert!(!outcome.failed, "{}", outcome.text);
        assert_eq!(std::fs::read_to_string(workspace.join("c.txt")).unwrap(), "whole\n");
        let outcome = execute(&json!({"files": [{"path": "d.txt", "content": "whole\n", "replace": "whole\n"}]}), &ctx);
        assert!(!outcome.failed, "{}", outcome.text);
        assert_eq!(std::fs::read_to_string(workspace.join("d.txt")).unwrap(), "whole\n");
        let outcome = execute(
            &json!({"files": [
                {"path": "b.txt", "find": "three", "replace": "tres"},
                {"content": "tres", "replace": "3"},
                {"path": "a.txt", "find": "two", "replace": "dos"},
                {"find": "dos", "replace": "2"}
            ]}),
            &ctx,
        );
        assert!(!outcome.failed, "{}", outcome.text);
        assert_eq!(std::fs::read_to_string(workspace.join("a.txt")).unwrap(), "uno\n2\n");
        assert_eq!(std::fs::read_to_string(workspace.join("b.txt")).unwrap(), "3\n");
        let outcome = execute(&json!({"path": "a.txt", "files": [{"find": "uno", "replace": "1"}, {"path": "b.txt", "find": "3", "replace": "iii"}]}), &ctx);
        assert!(!outcome.failed, "{}", outcome.text);
        assert_eq!(std::fs::read_to_string(workspace.join("a.txt")).unwrap(), "1\n2\n");
        let err = prepare(&json!({"files": [{"find": "x", "replace": "y"}]}), &test_ctx()).unwrap_err();
        assert!(err.to_string().contains("Missing or empty \"path\""), "{err}");
    }
}
