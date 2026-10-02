//! The "what changed" block a task loop leaves in scrollback: one headline,
//! then each changed file with a short line-numbered inline diff.
//!
//! Pure rendering over a "loop-changes" event (see harness::workspace_diff);
//! the full diff stays in the transcript, this is a preview.
//!
//! Preview lines are token-colored from the file's type: the tokens sit inside
//! the green/red line colour, and every token span hands that colour back
//! before it ends. A file whose type is unknown, and prose like Markdown,
//! renders exactly as it did before coloring existed.

use crate::core::types::{HarnessEventData, LoopChangedFile};
use crate::watch::ansi::{c, color_enabled, fit};

const ESC: &str = "\x1b";

/// Columns the line number, the sign and their padding take before the code.
const GUTTER: usize = 11;

/// Files previewed per block; the rest are counted.
pub const MAX_PREVIEW_FILES: usize = 6;
/// Diff lines previewed per file; the rest are counted.
pub const MAX_PREVIEW_LINES: usize = 12;

/// Painted rows for one "loop-changes" event (no trailing newlines).
///
/// The coloring here is preview-only: the diff text handed in is read, never
/// rewritten, so the transcript and the `loop-changes` payload keep exactly
/// the bytes git produced.
pub fn render_loop_changes(detail: &str, data: Option<&HarnessEventData>, width: usize) -> Vec<String> {
    let mut rows = vec![String::new(), format!("{} {}", c::green("●"), c::bold(&headline(detail)))];
    let files = data.and_then(|data| data.files.as_deref()).unwrap_or_default();
    for file in files.iter().take(MAX_PREVIEW_FILES) {
        rows.push(file_row(file, width));
        if file.status != "deleted" {
            rows.extend(diff_rows(file.diff.as_deref().unwrap_or_default(), &file.path, width));
        }
    }
    let hidden = files.len().saturating_sub(MAX_PREVIEW_FILES);
    if hidden > 0 {
        rows.push(c::dim(&format!(
            "  … +{hidden} more file{}",
            if hidden == 1 { "" } else { "s" }
        )));
    }
    rows
}

/// Rows `render_loop_changes` paints, for the scrollback tail budget.
pub fn estimate_loop_changes_rows(data: Option<&HarnessEventData>) -> usize {
    render_loop_changes("", data, 0).len()
}

/// `2 files changed (+14 −3)` → `Changed 2 files (+14 −3)`.
fn headline(detail: &str) -> String {
    match detail.split_once(" changed") {
        Some((count, rest)) => format!("Changed {count}{rest}"),
        None => detail.to_string(),
    }
}

fn clip(plain: &str, width: usize) -> String {
    if width == 0 {
        plain.to_string()
    } else {
        fit(plain, width, true).trim_end().to_string()
    }
}

fn file_row(file: &LoopChangedFile, width: usize) -> String {
    let note = if file.binary {
        "binary".to_string()
    } else {
        let counts = format!("+{} −{}", file.added, file.removed);
        match file.status.as_str() {
            "added" => format!("new, {counts}"),
            "deleted" => format!("deleted, {counts}"),
            _ => counts,
        }
    };
    let plain = clip(&format!("  ⎿ {} ({note})", file.path), width);
    match plain.strip_prefix("  ⎿ ") {
        Some(rest) => format!("  {} {}", c::dim("⎿"), rest),
        None => plain,
    }
}

/// `@@ -12,4 +12,6 @@` → (12, 12).
fn hunk_start(header: &str) -> Option<(usize, usize)> {
    let mut parts = header.split_whitespace().skip(1);
    let number = |part: &str| part[1..].split(',').next()?.parse::<usize>().ok();
    Some((number(parts.next()?)?, number(parts.next()?)?))
}

/// Paint the diff body for one changed file. `path` picks the tokenizer; a
/// file whose type it does not know renders exactly as it did before.
fn diff_rows(diff: &str, path: &str, width: usize) -> Vec<String> {
    let lang = syntax::detect(path);
    // One continuation per side: an unterminated block comment on a `-` line
    // says nothing about the `+` lines and one side's leftover never colors
    // the other's. A hunk header does not reset them, and the preview never
    // invents code a hunk does not show.
    let mut old_state = syntax::State::default();
    let mut new_state = syntax::State::default();
    let mut rows = Vec::new();
    let (mut old_line, mut new_line) = (0usize, 0usize);
    let mut shown = 0usize;
    let mut hidden = 0usize;
    let mut first_hunk = true;
    for line in diff.lines() {
        if line.starts_with("@@") {
            if let Some((old_start, new_start)) = hunk_start(line) {
                (old_line, new_line) = (old_start, new_start);
            }
            if !first_hunk && shown < MAX_PREVIEW_LINES {
                rows.push(c::dim("         ⋯"));
            }
            first_hunk = false;
            continue;
        }
        let (sign, number, base): (char, usize, &str) = match line.chars().next() {
            Some('+') => {
                new_line += 1;
                ('+', new_line - 1, "32")
            }
            Some('-') => {
                old_line += 1;
                ('-', old_line - 1, "31")
            }
            Some('\\') => continue,
            _ => {
                old_line += 1;
                new_line += 1;
                (' ', new_line - 1, "2")
            }
        };
        let text = line.get(1..).unwrap_or_default();
        // Past the cap nothing is painted, and `shown` only moves forward, so
        // a hidden line can never affect one that is on screen.
        if shown >= MAX_PREVIEW_LINES {
            hidden += 1;
            continue;
        }
        shown += 1;
        // The tokens are cut with the line: colorize what is left after
        // clipping, so an escape can never straddle the column limit.
        let row = clip(&format!("    {number:>4} {sign} {text}"), width);
        let row = match sign {
            // A context line belongs to both files, so it advances both
            // scanners — a comment the old side closes on one of these lines
            // must not stay open over the `-` lines that follow. The row the
            // reader sees is the new side's paint.
            ' ' => {
                let painted = paint_row(&row, lang, &mut new_state, base, width);
                let _ = paint_row(&row, lang, &mut old_state, base, width);
                painted
            }
            '-' => paint_row(&row, lang, &mut old_state, base, width),
            _ => paint_row(&row, lang, &mut new_state, base, width),
        };
        rows.push(row);
    }
    if hidden > 0 {
        rows.push(c::dim(&format!(
            "         … +{hidden} more line{}",
            if hidden == 1 { "" } else { "s" }
        )));
    }
    rows
}

/// Paint one already-clipped row: the gutter keeps the row's own colour, the
/// code inside is tokenized, and any trailing padding stays outside the token
/// spans. A row too narrow to hold its gutter is painted as one cue rather
/// than as a half-sliced token.
fn paint_row(
    row: &str,
    lang: Option<syntax::Lang>,
    state: &mut syntax::State,
    base: &str,
    width: usize,
) -> String {
    let (gutter, body) = match row.is_char_boundary(GUTTER) {
        true if width > GUTTER && row.len() > GUTTER => row.split_at(GUTTER),
        _ => return wrap_row(row, base),
    };
    let code = body.trim_end_matches(' ');
    let mut painted = String::with_capacity(row.len() + 32);
    painted.push_str(gutter);
    match lang {
        Some(lang) => painted.push_str(&syntax::colorize(code, lang, state, base)),
        None => painted.push_str(code),
    }
    painted.push_str(&body[code.len()..]);
    wrap_row(&painted, base)
}

/// Open the row's add/delete colour, and close it at the end. The syntax
/// spans re-open the same colour after each of their own, so a tinted token
/// never leaves the rest of the line unpainted.
fn wrap_row(plain: &str, base: &str) -> String {
    if color_enabled() {
        format!("{ESC}[{base}m{plain}{ESC}[0m")
    } else {
        plain.to_string()
    }
}

/// Diff-body tokenizing: a small, dependency-free highlighter for previews.
/// It scans one line at a time and colors keywords, strings, numbers, calls
/// and comments; anything else keeps the row's own colour.
mod syntax {
    use crate::watch::ansi::color_enabled;

    use super::ESC;

    /// SGR parameters per token class. A span always hands the row colour
    /// (`base`) back before it ends.
    const KEYWORD: &str = "1;35";
    const STRING: &str = "33";
    const NUMBER: &str = "36";
    const COMMENT: &str = "2";
    const CALL: &str = "1;34";
    const TYPE: &str = "1;36";

    /// One language's scanning rules. Only types `detect` knows reach here.
    #[derive(Clone, Copy)]
    pub struct Lang {
        /// Prefixes that start a comment wherever they appear.
        line_comments: &'static [&'static str],
        /// Block-comment delimiters (open, close).
        block_comment: Option<(&'static str, &'static str)>,
        /// Quote pairs that are strings, and whether backslash escapes them.
        strings: &'static [(&'static str, &'static str, bool)],
        /// Delimiters of strings that may run over a line (Python's triple
        /// quotes).
        doc_strings: &'static [(&'static str, &'static str)],
        /// The quote also opens Rust lifetimes (`'a`), which are not strings.
        lifetimes: bool,
    }

    /// What a line left open for the next one. Carried per diff side.
    #[derive(Default)]
    pub struct State {
        comment_end: Option<&'static str>,
        doc_end: Option<&'static str>,
    }

    const C_STRINGS: &[(&str, &str, bool)] = &[("\"", "\"", true), ("'", "'", true)];
    const SCRIPT_STRINGS: &[(&str, &str, bool)] =
        &[("\"", "\"", true), ("'", "'", true), ("`", "`", true)];
    const TRIPLE: &[(&str, &str)] = &[("\"\"\"", "\"\"\""), ("'''", "'''")];

    const C_LIKE: Lang = Lang {
        line_comments: &["//"],
        block_comment: Some(("/*", "*/")),
        strings: C_STRINGS,
        doc_strings: &[],
        lifetimes: false,
    };
    const RUST: Lang = Lang {
        line_comments: &["//"],
        block_comment: Some(("/*", "*/")),
        strings: C_STRINGS,
        doc_strings: &[],
        lifetimes: true,
    };
    /// JS/TS, and Go's raw strings, quote with backticks as well as the C pair.
    const JS_LIKE: Lang = Lang {
        line_comments: &["//"],
        block_comment: Some(("/*", "*/")),
        strings: SCRIPT_STRINGS,
        doc_strings: &[],
        lifetimes: false,
    };
    const PYTHON: Lang = Lang {
        line_comments: &["#"],
        block_comment: None,
        strings: SCRIPT_STRINGS,
        doc_strings: TRIPLE,
        lifetimes: false,
    };
    const SCRIPT: Lang = Lang {
        line_comments: &["#"],
        block_comment: None,
        strings: SCRIPT_STRINGS,
        doc_strings: &[],
        lifetimes: false,
    };
    const MARKUP: Lang = Lang {
        line_comments: &[],
        block_comment: Some(("<!--", "-->")),
        strings: C_STRINGS,
        doc_strings: &[],
        lifetimes: false,
    };
    const SQL: Lang = Lang {
        line_comments: &["--"],
        block_comment: Some(("/*", "*/")),
        strings: C_STRINGS,
        doc_strings: &[],
        lifetimes: false,
    };
    const LUA: Lang = Lang {
        line_comments: &["--"],
        block_comment: Some(("--[[", "]]")),
        strings: C_STRINGS,
        doc_strings: &[],
        lifetimes: false,
    };

    /// `Some` only when the file's name says "code". Prose, data and unknown
    /// extensions get `None`, which keeps their preview byte-identical to the
    /// uncolored one.
    pub fn detect(path: &str) -> Option<Lang> {
        let name = path.rsplit(['/', '\\']).next().unwrap_or(path).to_ascii_lowercase();
        match name.as_str() {
            "dockerfile" | "containerfile" | "makefile" | "gnumakefile" | "justfile"
            | "rakefile" | "gemfile" | ".gitignore" | ".dockerignore" | ".env" | ".envrc" => {
                return Some(SCRIPT)
            }
            _ => {}
        }
        let ext = match name.rsplit_once('.') {
            Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => ext,
            _ => return None,
        };
        Some(match ext {
            "rs" => RUST,
            "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "go" => JS_LIKE,
            "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "java" | "kt" | "kts"
            | "swift" | "php" | "cs" | "scala" | "dart" | "m" | "mm" | "proto" | "css"
            | "scss" | "less" | "groovy" | "zig" => C_LIKE,
            "py" | "pyi" => PYTHON,
            "rb" | "sh" | "bash" | "zsh" | "fish" | "pl" | "pm" | "r" | "jl" | "nim" | "ex"
            | "exs" | "cr" | "awk" | "tcl" | "yaml" | "yml" | "toml" | "ini" | "cfg"
            | "conf" | "lock" | "properties" | "env" | "mk" | "cmake" | "gradle" => SCRIPT,
            "html" | "htm" | "xml" | "svg" | "vue" | "svelte" => MARKUP,
            "sql" => SQL,
            "lua" => LUA,
            _ => return None,
        })
    }

    const KEYWORDS: &[&str] = &[
        "as", "async", "await", "break", "case", "catch", "chan", "class", "const",
        "continue", "crate", "def", "defer", "do", "dyn", "elif", "else", "elsif", "end",
        "enum", "except", "export", "extends", "extern", "false", "final", "finally", "fn",
        "for", "from", "func", "function", "global", "go", "goto", "if", "impl",
        "implements", "import", "in", "instanceof", "interface", "is", "lambda", "let",
        "local", "loop", "match", "mod", "move", "mut", "namespace", "new", "nil", "none",
        "not", "null", "operator", "or", "override", "package", "pass", "private",
        "protected", "pub", "public", "raise", "ref", "require", "return", "select", "self",
        "sizeof", "static", "struct", "super", "switch", "synchronized", "template", "this",
        "throw", "throws", "trait", "true", "try", "type", "typedef", "typeof", "union",
        "unsafe", "unsigned", "use", "using", "var", "virtual", "void", "volatile", "where",
        "while", "with", "yield",
    ];

    /// Colorize one line body. `base` is the row's own SGR parameters, which
    /// every token span re-opens before it ends.
    pub fn colorize(text: &str, lang: Lang, state: &mut State, base: &str) -> String {
        let enabled = color_enabled();
        let mut out = String::with_capacity(text.len() + 16);
        let mut i = 0usize;
        while i < text.len() {
            let rest = &text[i..];
            if let Some(end) = state.comment_end {
                let (piece, next, closed) = take_until(rest, end);
                span(&mut out, COMMENT, piece, base, enabled);
                i += next;
                if closed {
                    state.comment_end = None;
                }
                continue;
            }
            if let Some(end) = state.doc_end {
                let (piece, next, closed) = take_until(rest, end);
                span(&mut out, STRING, piece, base, enabled);
                i += next;
                if closed {
                    state.doc_end = None;
                }
                continue;
            }
            if let Some((open, close)) = lang
                .doc_strings
                .iter()
                .find(|(open, _)| rest.starts_with(*open))
                .copied()
            {
                match rest[open.len()..].find(close) {
                    Some(at) => {
                        let end = open.len() + at + close.len();
                        span(&mut out, STRING, &rest[..end], base, enabled);
                        i += end;
                    }
                    None => {
                        span(&mut out, STRING, rest, base, enabled);
                        state.doc_end = Some(close);
                        i = text.len();
                    }
                }
                continue;
            }
            // A Rust lifetime (`'a`, `'static`) opens like a char literal but
            // is not one; read as a string it would run to the next quote or
            // swallow the rest of the line.
            if lang.lifetimes {
                if let Some(end) = lifetime_end(rest) {
                    out.push_str(&rest[..end]);
                    i += end;
                    continue;
                }
            }
            if let Some((open, close, escapes)) = lang
                .strings
                .iter()
                .find(|(open, _, _)| rest.starts_with(*open))
                .copied()
            {
                let end = scan_string(rest, open, close, escapes);
                span(&mut out, STRING, &rest[..end], base, enabled);
                i += end;
                continue;
            }
            // The block delimiters are tried first because a line-comment
            // prefix can be a prefix of one: Lua's `--` would otherwise
            // swallow `--[[` and its multi-line comment would never open.
            if let Some((open, close)) = lang
                .block_comment
                .filter(|(open, _)| rest.starts_with(open))
            {
                match rest[open.len()..].find(close) {
                    Some(at) => {
                        let end = open.len() + at + close.len();
                        span(&mut out, COMMENT, &rest[..end], base, enabled);
                        i += end;
                    }
                    None => {
                        span(&mut out, COMMENT, rest, base, enabled);
                        state.comment_end = Some(close);
                        i = text.len();
                    }
                }
                continue;
            }
            if lang.line_comments.iter().any(|prefix| rest.starts_with(*prefix)) {
                span(&mut out, COMMENT, rest, base, enabled);
                i = text.len();
                continue;
            }
            let ch = rest.chars().next().unwrap();
            if ch.is_ascii_digit() {
                let end = ident_end(rest, |c| c.is_ascii_alphanumeric() || c == '_' || c == '.');
                span(&mut out, NUMBER, &rest[..end], base, enabled);
                i += end;
                continue;
            }
            if ch.is_alphanumeric() || ch == '_' || ch == '$' {
                let end = ident_end(rest, |c| c.is_alphanumeric() || c == '_' || c == '$');
                let word = &rest[..end];
                let params = if KEYWORDS.contains(&word) {
                    KEYWORD
                } else if rest[end..].starts_with("!(") || rest[end..].trim_start().starts_with('(') {
                    CALL
                } else if word.starts_with(char::is_uppercase) {
                    TYPE
                } else {
                    ""
                };
                if params.is_empty() {
                    out.push_str(word);
                } else {
                    span(&mut out, params, word, base, enabled);
                }
                i += end;
                continue;
            }
            out.push(ch);
            i += ch.len_utf8();
        }
        out
    }

    /// Byte length of the leading run of `keep`-matching characters.
    fn ident_end(s: &str, keep: impl Fn(char) -> bool) -> usize {
        s.find(|c: char| !keep(c)).unwrap_or(s.len())
    }

    /// Byte length of a Rust lifetime (`'a`, `'static`) at `s`, which always
    /// starts with a quote. A quote whose identifier run is followed by
    /// another quote is a char literal (`'x'`), and one with no identifier at
    /// all (`'\n'`) is left to the string scanner: both return `None`.
    fn lifetime_end(s: &str) -> Option<usize> {
        let after_quote = s.strip_prefix('\'')?;
        let name = ident_end(after_quote, |c| c.is_alphanumeric() || c == '_');
        if name == 0 {
            return None;
        }
        (!after_quote[name..].starts_with('\'')).then_some(1 + name)
    }

    /// Byte length of a single-line string starting at `s`, quotes included.
    fn scan_string(s: &str, open: &str, close: &str, escapes: bool) -> usize {
        let body = &s[open.len()..];
        let mut j = 0usize;
        while j < body.len() {
            let rest = &body[j..];
            if escapes && rest.starts_with('\\') {
                j += 1 + rest[1..].chars().next().map(char::len_utf8).unwrap_or(0);
                continue;
            }
            if rest.starts_with(close) {
                return open.len() + j + close.len();
            }
            j += rest.chars().next().unwrap().len_utf8();
        }
        s.len()
    }

    /// The rest of the line up to and including `end`, whether it was found.
    fn take_until<'a>(rest: &'a str, end: &str) -> (&'a str, usize, bool) {
        match rest.find(end) {
            Some(at) => (&rest[..at + end.len()], at + end.len(), true),
            None => (rest, rest.len(), false),
        }
    }

    /// Wrap `text` in `params` and hand the row colour back afterwards. The
    /// hand-back resets first (`ESC[0;{base}m`): `0` clears the attributes the
    /// token set — the bold of a keyword, call or type — while the parameters
    /// that follow re-apply the row's own cue, which `0` alone would drop.
    fn span(out: &mut String, params: &str, text: &str, base: &str, enabled: bool) {
        if text.is_empty() {
            return;
        }
        if enabled {
            out.push_str(ESC);
            out.push('[');
            out.push_str(params);
            out.push('m');
            out.push_str(text);
            out.push_str(ESC);
            out.push_str("[0;");
            out.push_str(base);
            out.push('m');
        } else {
            out.push_str(text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watch::ansi::strip_ansi;

    fn file(path: &str, added: i64, removed: i64, status: &str, diff: Option<&str>) -> LoopChangedFile {
        LoopChangedFile {
            path: path.to_string(),
            added,
            removed,
            status: status.to_string(),
            binary: false,
            diff: diff.map(str::to_string),
        }
    }

    fn plain(detail: &str, files: Vec<LoopChangedFile>, width: usize) -> Vec<String> {
        let data = HarnessEventData {
            files: Some(files),
            ..Default::default()
        };
        render_loop_changes(detail, Some(&data), width)
            .iter()
            .map(|row| strip_ansi(row))
            .collect()
    }

    #[test]
    fn block_shows_headline_files_and_line_numbered_hunks() {
        let diff = "@@ -12,3 +12,3 @@ fn main() {\n let a = 1;\n-let b = 2;\n+let b = 3;\n let c = 4;\n\\ No newline at end of file\n";
        let rows = plain(
            "2 files changed (+2 −1)",
            vec![
                file("src/foo.rs", 1, 1, "modified", Some(diff)),
                file("notes.md", 1, 0, "added", Some("@@ -0,0 +1 @@\n+hello\n")),
            ],
            80,
        );
        assert_eq!(
            rows,
            vec![
                "",
                "● Changed 2 files (+2 −1)",
                "  ⎿ src/foo.rs (+1 −1)",
                "      12   let a = 1;",
                "      13 - let b = 2;",
                "      13 + let b = 3;",
                "      14   let c = 4;",
                "  ⎿ notes.md (new, +1 −0)",
                "       1 + hello",
            ]
        );
    }

    #[test]
    fn long_diffs_and_long_file_lists_are_capped_with_counts() {
        let mut diff = String::from("@@ -0,0 +1,30 @@\n");
        for n in 0..30 {
            diff.push_str(&format!("+line {n}\n"));
        }
        let files: Vec<LoopChangedFile> = (0..8)
            .map(|n| file(&format!("f{n}.txt"), 30, 0, "added", Some(&diff)))
            .collect();
        let rows = plain("8 files changed (+240 −0)", files, 80);
        let per_file = 1 + MAX_PREVIEW_LINES + 1;
        assert_eq!(rows.len(), 2 + MAX_PREVIEW_FILES * per_file + 1);
        assert_eq!(rows[2 + MAX_PREVIEW_LINES + 1], "         … +18 more lines");
        assert_eq!(rows.last().unwrap(), "  … +2 more files");
        let data = HarnessEventData {
            files: Some((0..8).map(|n| file(&format!("f{n}.txt"), 30, 0, "added", Some(&diff))).collect()),
            ..Default::default()
        };
        assert_eq!(estimate_loop_changes_rows(Some(&data)), rows.len());
    }

    #[test]
    fn binary_and_deleted_files_show_no_diff_body() {
        let mut logo = file("logo.png", 0, 0, "modified", None);
        logo.binary = true;
        let rows = plain(
            "2 files changed (+0 −3)",
            vec![logo, file("old.rs", 0, 3, "deleted", Some("@@ -1,3 +0,0 @@\n-a\n-b\n-c\n"))],
            80,
        );
        assert_eq!(rows[2..], ["  ⎿ logo.png (binary)", "  ⎿ old.rs (deleted, +0 −3)"]);
    }

    #[test]
    fn rows_never_exceed_the_terminal_width_and_hunks_are_separated() {
        let diff = "@@ -1,1 +1,1 @@\n-short\n+a very long replacement line that cannot possibly fit\n@@ -40,1 +40,1 @@\n-x\n+y\n";
        let rows = plain("1 file changed (+2 −2)", vec![file("a.rs", 2, 2, "modified", Some(diff))], 30);
        assert!(rows.iter().all(|row| crate::watch::ansi::string_width(row) <= 30), "{rows:?}");
        assert!(strip_ansi(&rows[4]).ends_with('…'), "{rows:?}");
        assert_eq!(rows[5], "         ⋯");
        assert_eq!(strip_ansi(&rows[6]), "      40 - x");
    }

    #[test]
    fn an_event_without_data_still_renders_its_headline() {
        let rows = render_loop_changes("1 file changed (+1 −0)", None, 80);
        assert_eq!(strip_ansi(&rows[1]), "● Changed 1 file (+1 −0)");
        assert_eq!(rows.len(), 2);
    }

    /// Raw (escape-carrying) rows for one file, holding the global colour
    /// switch the way the other colour tests do.
    fn rows_with_color(
        on: bool,
        path: &str,
        diff: &str,
        width: usize,
    ) -> (Vec<String>, std::sync::MutexGuard<'static, ()>) {
        let guard = crate::watch::ansi::color_test_lock();
        let previous = crate::watch::ansi::color_enabled();
        crate::watch::ansi::set_color_enabled(on);
        let data = HarnessEventData {
            files: Some(vec![file(path, 1, 1, "modified", Some(diff))]),
            ..Default::default()
        };
        let rows = render_loop_changes("1 file changed (+1 −1)", Some(&data), width);
        crate::watch::ansi::set_color_enabled(previous);
        (rows, guard)
    }

    #[test]
    fn rust_code_is_token_colored_inside_the_change_cue() {
        let diff = "@@ -1,1 +1,1 @@\n-let n = 41; // before\n+let n = 42;\n";
        let (rows, _guard) = rows_with_color(true, "src/foo.rs", diff, 80);
        let removed = &rows[3];
        assert!(removed.starts_with(&format!("{ESC}[31m")), "{removed:?}");
        assert!(removed.ends_with(&format!("{ESC}[0m")), "{removed:?}");
        assert!(
            removed.contains(&format!("{ESC}[1;35mlet{ESC}[0;31m")),
            "the keyword is tinted and hands the row colour back: {removed:?}"
        );
        assert!(removed.contains(&format!("{ESC}[36m41{ESC}[0;31m")), "{removed:?}");
        assert!(
            removed.contains(&format!("{ESC}[2m// before{ESC}[0;31m")),
            "the comment dims inside the red row: {removed:?}"
        );
        let added = &rows[4];
        assert!(added.starts_with(&format!("{ESC}[32m")), "{added:?}");
        assert_eq!(strip_ansi(added), "       1 + let n = 42;");
        assert_eq!(strip_ansi(removed), "       1 - let n = 41; // before");
    }

    #[test]
    fn other_languages_color_their_own_tokens() {
        let (rows, _guard) = rows_with_color(
            true,
            "tools/run.py",
            "@@ -1,1 +1,1 @@\n+def f(x):  # note\n",
            80,
        );
        let row = &rows[3];
        assert!(row.contains(&format!("{ESC}[1;35mdef{ESC}[0;32m")), "{row:?}");
        assert!(
            row.contains(&format!("{ESC}[1;34mf{ESC}[0;32m")),
            "a call is tinted: {row:?}"
        );
        assert!(row.contains(&format!("{ESC}[2m# note{ESC}[0;32m")), "{row:?}");
        assert_eq!(strip_ansi(row), "       1 + def f(x):  # note");
    }

    #[test]
    fn unknown_extensions_and_prose_keep_the_plain_preview() {
        for path in ["notes.xyz", "README.md", "data.bin", "Makefile.orig"] {
            let (rows, _guard) = rows_with_color(
                true,
                path,
                "@@ -1,1 +1,1 @@\n+let n = 1; // c\n",
                80,
            );
            let row = &rows[3];
            assert_eq!(strip_ansi(row), "       1 + let n = 1; // c", "{path}");
            assert!(row.starts_with(&format!("{ESC}[32m")), "{path}: {row:?}");
            assert!(
                !row.contains(&format!("{ESC}[1;35m")) && !row.contains(&format!("{ESC}[2m//")),
                "{path} must not be tokenized: {row:?}"
            );
        }
        // A bare file name the tokenizer knows is still code.
        let (rows, _guard) = rows_with_color(
            true,
            "Dockerfile",
            "@@ -1,1 +1,1 @@\n+RUN true # c\n",
            80,
        );
        assert!(
            rows[3].contains(&format!("{ESC}[2m# c{ESC}[0;32m")),
            "{:?}",
            rows[3]
        );
    }

    #[test]
    fn narrow_widths_clip_plain_text_and_wide_characters_stay_inside_the_row() {
        let (rows, guard) = rows_with_color(
            true,
            "src/foo.rs",
            "@@ -1,1 +1,1 @@\n+let s = \"héllo→world\";\n",
            24,
        );
        let body = strip_ansi(&rows[3]);
        assert!(
            crate::watch::ansi::string_width(&rows[3]) <= 24,
            "a wide character must not push the row past the terminal: {body:?}"
        );
        assert!(body.ends_with('…'), "the clipped line is marked: {body:?}");
        assert!(rows[3].contains(&format!("{ESC}[1;35m")), "{:?}", rows[3]);
        assert_eq!(rows[3].matches(ESC).count() % 2, 0, "{rows:?}");
        // Narrower than the gutter: one plain cue, never a half-sliced token.
        // (The colour lock is not reentrant, so hand the first row's back.)
        drop(guard);
        let (narrow, _guard) = rows_with_color(
            true,
            "src/foo.rs",
            "@@ -1,1 +1,1 @@\n+let n = 1;\n",
            8,
        );
        assert_eq!(strip_ansi(&narrow[3]), "       …");
        assert_eq!(crate::watch::ansi::string_width(&narrow[3]), 8);
        assert_eq!(narrow[3].matches(ESC).count() % 2, 0, "{narrow:?}");
    }

    #[test]
    fn every_token_span_resets_before_reopening_the_row_colour() {
        let (rows, _guard) = rows_with_color(
            true,
            "src/foo.rs",
            "@@ -1,1 +1,1 @@\n+let s = \"hi\"; // t\n",
            80,
        );
        let row = &rows[3];
        assert_eq!(row.matches(&format!("{ESC}[0m")).count(), 1, "{row:?}");
        assert!(row.ends_with(&format!("{ESC}[0m")), "{row:?}");
        for token in ["1;35", "33", "2"] {
            let open = format!("{ESC}[{token}m");
            if let Some(at) = row.find(&open) {
                assert!(
                    row[at + open.len()..].contains(&format!("{ESC}[0;32m")),
                    "token {token} must reset and hand the row colour back: {row:?}"
                );
            }
        }
    }

    #[test]
    fn block_comment_state_follows_each_side_and_survives_a_hunk_boundary() {
        let diff = "@@ -1,2 +1,2 @@\n-/* note\n-let a = 1;\n+let a = 2;\n@@ -9,2 +9,2 @@\n-still inside */\n-let b = 1;\n+let b = 2;\n";
        let (rows, _guard) = rows_with_color(true, "src/foo.rs", diff, 60);
        assert!(
            rows[3].contains(&format!("{ESC}[2m/* note{ESC}[0;31m")),
            "{:?}",
            rows[3]
        );
        assert!(
            rows[4].contains(&format!("{ESC}[2mlet a = 1;{ESC}[0;31m")),
            "the old side stays a comment: {:?}",
            rows[4]
        );
        assert!(
            rows[5].contains(&format!("{ESC}[1;35mlet{ESC}[0;32m")),
            "an open comment on the old side must not touch the new one: {:?}",
            rows[5]
        );
        assert_eq!(strip_ansi(&rows[6]), "         ⋯");
        assert!(
            rows[7].contains(&format!("{ESC}[2mstill inside */{ESC}[0;31m")),
            "the state survives the hunk header: {:?}",
            rows[7]
        );
        assert!(
            rows[8].contains(&format!("{ESC}[1;35mlet{ESC}[0;31m")),
            "closing the comment restores code coloring: {:?}",
            rows[8]
        );
    }

    #[test]
    fn a_hunk_that_opens_a_comment_never_colors_the_lines_it_does_not_show() {
        // The `-` side opens a string and a comment it never closes; the `+`
        // side of the same hunk is ordinary code.
        let diff = "@@ -1,2 +1,2 @@\n-let s = \"open\n-/* c\n+println!(\"ok\");\n";
        let (rows, _guard) = rows_with_color(true, "src/foo.rs", diff, 60);
        assert!(
            rows[4].contains(&format!("{ESC}[2m/* c{ESC}[0;31m")),
            "the unterminated block comment colors its own line: {:?}",
            rows[4]
        );
        assert!(
            rows[5].contains(&format!("{ESC}[1;34mprintln{ESC}[0;32m")),
            "the call keeps its own colour: {:?}",
            rows[5]
        );
        assert!(
            !rows[5].contains(&format!("{ESC}[2m")),
            "the old side's open comment never reaches the new side: {:?}",
            rows[5]
        );
    }

    #[test]
    fn token_spans_clear_their_attributes_before_handing_the_cue_back() {
        let (rows, _guard) = rows_with_color(
            true,
            "src/foo.rs",
            "@@ -1,1 +1,1 @@\n+let n = 42; // t\n",
            80,
        );
        let row = &rows[3];
        assert!(
            row.contains(&format!("{ESC}[1;35mlet{ESC}[0;32m")),
            "a bold keyword clears its own attributes: {row:?}"
        );
        assert!(
            !row.contains(&format!("{ESC}[1;35mlet{ESC}[32m")),
            "the cue must be reset, not merely re-opened: {row:?}"
        );
        // Nothing after the keyword is still bold: the rest of the row is the
        // plain green cue, so the change cue never thickens mid-line.
        let tail = row
            .split(&format!("{ESC}[0;32m"))
            .nth(1)
            .expect("the keyword hands the cue back");
        assert!(
            !tail.contains(&format!("{ESC}[1;")),
            "no attribute survives the span: {tail:?}"
        );
        assert!(row.ends_with(&format!("{ESC}[0m")), "{row:?}");
        assert_eq!(strip_ansi(row), "       1 + let n = 42; // t");
    }

    #[test]
    fn context_rows_keep_their_faint_cue_after_a_tinted_token() {
        let (rows, _guard) = rows_with_color(
            true,
            "src/foo.rs",
            "@@ -1,1 +1,1 @@\n let n = 42; // t\n",
            80,
        );
        let row = &rows[3];
        assert!(row.starts_with(&format!("{ESC}[2m")), "{row:?}");
        assert!(
            row.contains(&format!("{ESC}[1;35mlet{ESC}[0;2m")),
            "the faint cue survives the reset: {row:?}"
        );
        assert!(row.contains(&format!("{ESC}[36m42{ESC}[0;2m")), "{row:?}");
        assert_eq!(strip_ansi(row), "       1   let n = 42; // t");
    }

    #[test]
    fn template_literals_are_strings_and_rust_lifetimes_are_not() {
        let (rows, guard) = rows_with_color(
            true,
            "web/app.ts",
            "@@ -1,1 +1,1 @@\n+const s = `t${n}`; // c\n",
            80,
        );
        let row = &rows[3];
        assert!(
            row.contains(&format!("{ESC}[33m`t${{n}}`{ESC}[0;32m")),
            "a JS template literal is a string: {row:?}"
        );
        assert!(row.contains(&format!("{ESC}[2m// c{ESC}[0;32m")), "{row:?}");
        drop(guard);
        // Go's raw strings use the same delimiter.
        let (rows, guard) = rows_with_color(true, "tools/run.go", "@@ -1,1 +1,1 @@\n+s := `raw`\n", 80);
        assert!(
            rows[3].contains(&format!("{ESC}[33m`raw`{ESC}[0;32m")),
            "{:?}",
            rows[3]
        );
        drop(guard);
        // A lifetime is not a string, but a real char literal still is.
        // (The colour lock is not reentrant: hand it back before re-taking it.)
        let (rows, guard) = rows_with_color(
            true,
            "src/foo.rs",
            "@@ -1,1 +1,1 @@\n+fn f<'a>(x: &'a str) -> &'a str { x }\n",
            90,
        );
        let row = &rows[3];
        assert!(
            !row.contains(&format!("{ESC}[33m")),
            "a lifetime must not open a string: {row:?}"
        );
        assert!(row.contains(&format!("{ESC}[1;35mfn{ESC}[0;32m")), "{row:?}");
        assert_eq!(
            strip_ansi(row),
            "       1 + fn f<'a>(x: &'a str) -> &'a str { x }"
        );
        drop(guard);
        let (rows, _guard) = rows_with_color(
            true,
            "src/foo.rs",
            "@@ -1,1 +1,1 @@\n+let c = 'x'; // t\n",
            80,
        );
        assert!(
            rows[3].contains(&format!("{ESC}[33m'x'{ESC}[0;32m")),
            "a char literal is still a string: {:?}",
            rows[3]
        );
    }

    #[test]
    fn a_block_comment_opens_even_where_a_line_comment_shares_its_prefix() {
        // Lua's `--` is a prefix of its `--[[` block opener; the block must win.
        let diff = "@@ -1,2 +1,2 @@\n---[[ note\n-let a = 1;\n";
        // (The colour lock is not reentrant — hand it back before re-taking it.)
        let (rows, guard) = rows_with_color(true, "conf/app.lua", diff, 60);
        assert!(
            rows[3].contains(&format!("{ESC}[2m--[[ note{ESC}[0;31m")),
            "the block opener colors its own line: {:?}",
            rows[3]
        );
        assert!(
            rows[4].contains(&format!("{ESC}[2mlet a = 1;{ESC}[0;31m")),
            "and stays open on the next line: {:?}",
            rows[4]
        );
        drop(guard);
        // A plain line comment still ends with its own line.
        let (rows, _guard) = rows_with_color(
            true,
            "conf/app.lua",
            "@@ -1,1 +1,1 @@\n+print(1) -- note\n",
            60,
        );
        assert!(
            rows[3].contains(&format!("{ESC}[2m-- note{ESC}[0;32m")),
            "{:?}",
            rows[3]
        );
        assert_eq!(strip_ansi(&rows[3]), "       1 + print(1) -- note");
    }

    #[test]
    fn a_context_line_advances_both_sides_comment_state() {
        // The old side opens a comment; the context line closes it, so the `-`
        // line after it is code again rather than a stale continuation.
        let diff = "@@ -1,3 +1,3 @@\n-/* open\n */\n-let a = 1;\n";
        let (rows, _guard) = rows_with_color(true, "src/foo.rs", diff, 60);
        assert!(
            rows[3].contains(&format!("{ESC}[2m/* open{ESC}[0;31m")),
            "{:?}",
            rows[3]
        );
        assert_eq!(strip_ansi(&rows[4]), "       1   */");
        assert!(
            rows[5].contains(&format!("{ESC}[1;35mlet{ESC}[0;31m")),
            "closing on a context line ends the old side's comment: {:?}",
            rows[5]
        );
    }

    #[test]
    fn hidden_lines_past_the_cap_do_not_touch_the_visible_rows() {
        let mut diff = String::from("@@ -1,20 +1,1 @@\n");
        diff.push_str("+keep\n");
        for n in 0..20 {
            diff.push_str(&format!("-let x{n} = {n}; /* open\n"));
        }
        let (rows, _guard) = rows_with_color(true, "src/foo.rs", &diff, 60);
        // Only the capped preview is painted, and the hidden tail's open
        // comment never reaches back into it.
        assert_eq!(strip_ansi(&rows[3]), "       1 + keep");
        assert_eq!(rows[3], format!("{ESC}[32m       1 + keep{ESC}[0m"));
        assert_eq!(rows.len(), 3 + MAX_PREVIEW_LINES + 1);
        assert_eq!(strip_ansi(rows.last().unwrap()), "         … +9 more lines");
    }

    #[test]
    fn with_color_disabled_the_colored_preview_is_plain_text() {
        let (rows, _guard) = rows_with_color(
            false,
            "src/foo.rs",
            "@@ -1,1 +1,1 @@\n+let s = \"hi\"; // t\n",
            80,
        );
        assert_eq!(rows[3], "       1 + let s = \"hi\"; // t");
        assert!(!rows[3].contains(ESC));
    }
}
