// File outlines: a harness-generated map of the definitions in the files a
// task names, injected into the loop prompt so a worker on a 6000-line file
// jumps to the right line range instead of paging through it. Dogfood runs
// on src/harness/loop.rs spent 2-3 loops (60+ one-call rounds) re-reading
// the same file; an outline costs no inference and lands before the first
// round.

use std::path::Path;

/// Files shorter than this are cheap to READ whole; no outline.
pub const OUTLINE_MIN_LINES: usize = 200;
/// Definitions listed per file before the outline says "+N more".
pub const OUTLINE_MAX_ENTRIES: usize = 60;
/// Files a task may name and still get outlines for all of them.
pub const OUTLINE_MAX_FILES: usize = 4;
/// Definitions past OUTLINE_MAX_ENTRIES are listed compactly as `name@line`
/// up to this many more, so a 4,000-line file's methods are all reachable
/// from the first prompt (a recorded run spent 36 READ windows on one such file).
pub const OUTLINE_MAX_COMPACT: usize = 300;
/// Longest signature kept per entry.
const OUTLINE_ENTRY_CHARS: usize = 90;

/// Files the goal names that are short enough to READ whole are carried into
/// the first prompt as their numbered text: 350 of 712 recorded runs spent
/// their entire first round on READs of exactly those paths (one model turn
/// each, then the round trip) before doing anything else. A file over this
/// many lines gets an outline instead (READ's default window is 400 lines).
pub const NAMED_FILE_BODY_MAX_LINES: usize = 400;
/// Named files carried whole into the first prompt, first-mentioned first.
pub const NAMED_FILE_BODIES_MAX_FILES: usize = 3;
/// Total budget for the carried bodies; a file that would cross it is skipped.
pub const NAMED_FILE_BODIES_MAX_CHARS: usize = 40_000;
/// Longest line kept in a carried body (READ clamps at the same width).
const NAMED_FILE_LINE_CHARS: usize = 2000;

/// Identifiers named in a goal that get a pre-run `git grep` (first-mentioned first).
pub const SYMBOL_HITS_MAX_SYMBOLS: usize = 8;
/// Hits listed per identifier before the line says "+N more".
pub const SYMBOL_HITS_MAX_PER_SYMBOL: usize = 6;
/// Total budget for the symbol-hits block.
pub const SYMBOL_HITS_MAX_CHARS: usize = 6_000;
/// Wall-clock budget for the greps behind one block; later symbols are dropped.
const SYMBOL_HITS_TIME_BUDGET_MS: u128 = 400;
const SYMBOL_HIT_LINE_CHARS: usize = 110;

fn is_symbol_token(token: &str) -> bool {
    if token.len() < 4 || token.len() > 60 {
        return false;
    }
    let bytes = token.as_bytes();
    if !(bytes[0].is_ascii_alphabetic() || bytes[0] == b'_') {
        return false;
    }
    let has_underscore_inside = token[1..token.len() - 1].contains('_');
    let camel = token.chars().zip(token.chars().skip(1)).any(|(a, b)| a.is_ascii_lowercase() && b.is_ascii_uppercase());
    let all_caps = token.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    (has_underscore_inside || camel) && !all_caps
}

/// Identifiers worth a pre-run grep: snake_case and CamelCase names in `texts`
/// (qualified names split on `::` and `.`), skipping file paths, ALL_CAPS
/// words and plain prose; first-mentioned first, at most SYMBOL_HITS_MAX_SYMBOLS.
pub fn extract_goal_symbols(texts: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for text in texts {
        for raw in text.split(|c: char| c.is_whitespace() || "`'\"(),;:<>[]{}=+*!?&|#".contains(c)) {
            if raw.contains('/') || raw.is_empty() {
                continue;
            }
            for part in raw.split('.') {
                let part = part.trim_matches(|c: char| !(c.is_ascii_alphanumeric() || c == '_'));
                if part.contains('-') {
                    continue;
                }
                if is_symbol_token(part) && !out.iter().any(|seen| seen == part) {
                    out.push(part.to_string());
                    if out.len() >= SYMBOL_HITS_MAX_SYMBOLS {
                        return out;
                    }
                }
            }
        }
    }
    out
}

fn git_grep_hits(cwd: &str, symbol: &str) -> Option<Vec<String>> {
    let output = std::process::Command::new("git")
        .args(["grep", "-n", "-w", "-F", "--untracked", "-I", "-e", symbol, "--", ".", ":!*.lock", ":!*.min.*"])
        .current_dir(cwd)
        .output()
        .ok()?;
    // Exit 1 is "no match"; anything else (not a repo, no git) is unusable.
    if !output.status.success() && output.status.code() != Some(1) {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).lines().map(str::to_string).collect())
}

/// Plain words the goal quotes in backticks — `title` — that are not
/// identifiers. A whole-word grep for such a word is noise (hundreds of
/// hits), but the definitions whose *name* contains it are exactly where a
/// goal that says "the predicate whose name contains `title`" sends the
/// first rounds.
pub const DEFINITION_HITS_MAX_WORDS: usize = 4;
/// Files listed with their definitions per word; further files are named
/// with a count only. Source files come before test, fixture, eval, example
/// and vendor trees, then by hit count — a benchmark fixture with seventy
/// `def *_title_*` helpers must not crowd out the two in `src/`.
pub const DEFINITION_HITS_MAX_FILES: usize = 4;
pub const DEFINITION_HITS_MAX_PER_FILE: usize = 6;
pub const DEFINITION_HITS_MAX_NAMED_FILES: usize = 10;

pub fn extract_quoted_words(texts: &[&str]) -> Vec<String> {
    let symbols = extract_goal_symbols(texts);
    let mut out: Vec<String> = Vec::new();
    for text in texts {
        let mut parts = text.split('`');
        // Odd segments sit between backticks.
        parts.next();
        while let (Some(inner), next) = (parts.next(), parts.next()) {
            let word = inner.trim();
            let plain = word.len() >= 3
                && word.len() <= 30
                && word.chars().all(|c| c.is_ascii_alphabetic())
                && !symbols.iter().any(|symbol| symbol.eq_ignore_ascii_case(word))
                && !out.iter().any(|seen| seen.eq_ignore_ascii_case(word));
            if plain {
                out.push(word.to_string());
                if out.len() >= DEFINITION_HITS_MAX_WORDS {
                    return out;
                }
            }
            if next.is_none() {
                break;
            }
        }
    }
    out
}

fn git_grep_definitions(cwd: &str, word: &str) -> Option<Vec<String>> {
    let pattern = format!(
        "^[[:space:]]*(export[[:space:]]+(default[[:space:]]+)?)?(pub(\\(crate\\))?[[:space:]]+)?(async[[:space:]]+)?(fn|struct|enum|trait|type|impl|def|class|function|const|static|interface|func)[[:space:]]+[A-Za-z_]*{word}"
    );
    let output = std::process::Command::new("git")
        .args(["grep", "-n", "-i", "-I", "-E", "--untracked", "-e", &pattern, "--", ".", ":!*.lock", ":!*.min.*"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() && output.status.code() != Some(1) {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).lines().map(str::to_string).collect())
}

fn test_like_path(path: &str) -> bool {
    const TREES: &[&str] = &[
        "test", "tests", "testing", "fixture", "fixtures", "eval", "evals", "example", "examples", "bench", "benches",
        "benchmark", "benchmarks", "vendor", "third_party", "node_modules", "dist", "build", "target", "spec", "specs",
        "__tests__", "snapshots", "__snapshots__", "testdata",
    ];
    let mut segments = path.split('/').peekable();
    while let Some(segment) = segments.next() {
        let last = segments.peek().is_none();
        if last {
            let lower = segment.to_ascii_lowercase();
            return lower.starts_with("test") || lower.contains("_test") || lower.contains(".test") || lower.contains(".spec");
        }
        if TREES.contains(&segment) {
            return true;
        }
    }
    false
}

/// One orientation line for `word`: hits grouped by file, source files
/// first, each of the first files with its definitions as `line signature`.
fn definition_hits_line(word: &str, hits: &[String]) -> String {
    let mut groups: Vec<(String, Vec<(String, String)>)> = Vec::new();
    for hit in hits {
        let mut parts = hit.trim().splitn(3, ':');
        let (path, num, text) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or("").trim());
        if path.is_empty() || num.is_empty() {
            continue;
        }
        match groups.iter_mut().find(|(seen, _)| seen == path) {
            Some((_, entries)) => entries.push((num.to_string(), signature(text))),
            None => groups.push((path.to_string(), vec![(num.to_string(), signature(text))])),
        }
    }
    groups.sort_by(|a, b| {
        test_like_path(&a.0)
            .cmp(&test_like_path(&b.0))
            .then(b.1.len().cmp(&a.1.len()))
            .then_with(|| a.0.cmp(&b.0))
    });
    let mut rendered: Vec<String> = Vec::new();
    for (index, (path, entries)) in groups.iter().enumerate() {
        if index < DEFINITION_HITS_MAX_FILES {
            let shown: Vec<String> = entries.iter().take(DEFINITION_HITS_MAX_PER_FILE).map(|(num, sig)| format!("{num} {sig}")).collect();
            let mut part = format!("{path}: {}", shown.join("; "));
            if entries.len() > shown.len() {
                part.push_str(&format!("; +{} more", entries.len() - shown.len()));
            }
            rendered.push(part);
        } else if index < DEFINITION_HITS_MAX_NAMED_FILES {
            rendered.push(format!("{path} ({})", entries.len()));
        } else {
            rendered.push(format!("+{} more files", groups.len() - index));
            break;
        }
    }
    format!("{word}: {}", rendered.join(" | "))
}

/// Definition lines whose name contains a word the goal quotes in
/// backticks (`title` → `fn should_request_title`, `struct TitleRoute`),
/// grouped by file with source files first. None outside git or when the
/// goal quotes no plain word.
pub fn definition_hits_for_texts(cwd: &str, texts: &[&str]) -> Option<String> {
    let words = extract_quoted_words(texts);
    if words.is_empty() {
        return None;
    }
    let started = std::time::Instant::now();
    let mut lines: Vec<String> = Vec::new();
    for word in &words {
        if started.elapsed().as_millis() > SYMBOL_HITS_TIME_BUDGET_MS {
            break;
        }
        let hits = git_grep_definitions(cwd, word)?;
        if hits.is_empty() {
            lines.push(format!("{word}: no definition names contain it"));
        } else {
            lines.push(definition_hits_line(word, &hits));
        }
    }
    if lines.is_empty() {
        None
    } else {
        Some(format!(
            "definition hits (definitions whose name contains a word the goal quotes, grouped by file with source files before test and fixture trees — READ the one you need instead of GREPping for it):\n{}",
            lines.join("\n")
        ))
    }
}

/// `git grep -nw` hits for the identifiers `texts` name, one line per
/// identifier, so the first rounds of a task start from the call sites and
/// definitions instead of GREPping for them. None outside git or when
/// nothing qualifies.
pub fn symbol_hits_for_texts(cwd: &str, texts: &[&str]) -> Option<String> {
    let symbols = extract_goal_symbols(texts);
    if symbols.is_empty() {
        return None;
    }
    let started = std::time::Instant::now();
    let mut lines: Vec<String> = Vec::new();
    let mut total = 0usize;
    for symbol in &symbols {
        if started.elapsed().as_millis() > SYMBOL_HITS_TIME_BUDGET_MS {
            break;
        }
        let hits = git_grep_hits(cwd, symbol)?;
        let line = if hits.is_empty() {
            format!("{symbol}: no hits")
        } else {
            let shown: Vec<String> = hits
                .iter()
                .take(SYMBOL_HITS_MAX_PER_SYMBOL)
                .map(|hit| {
                    let hit = hit.trim();
                    // "path:line:text" — keep the locator, tighten the text.
                    let mut parts = hit.splitn(3, ':');
                    let (path, num, text) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or("").trim());
                    let text: String = text.chars().take(SYMBOL_HIT_LINE_CHARS).collect();
                    format!("{path}:{num} {text}")
                })
                .collect();
            let mut line = format!("{symbol}: {}", shown.join(" | "));
            if hits.len() > shown.len() {
                line.push_str(&format!(" | +{} more", hits.len() - shown.len()));
            }
            line
        };
        total += line.len() + 1;
        if total > SYMBOL_HITS_MAX_CHARS {
            break;
        }
        lines.push(line);
    }
    if lines.is_empty() {
        None
    } else {
        Some(format!("symbol hits (git grep -nw, first {SYMBOL_HITS_MAX_PER_SYMBOL} per name):\n{}", lines.join("\n")))
    }
}

pub fn is_definition(ext: &str, line: &str) -> bool {
    let indent = line.len() - line.trim_start().len();
    let body = line.trim_start();
    let starts_with_any = |words: &[&str]| words.iter().any(|w| body.starts_with(w));
    match ext {
        "rs" => {
            indent <= 4
                && starts_with_any(&[
                    "pub fn ", "fn ", "pub async fn ", "async fn ", "pub(crate) fn ", "pub(crate) async fn ",
                    "pub struct ", "struct ", "pub enum ", "enum ", "pub trait ", "trait ", "impl ", "impl<",
                    "pub mod ", "mod ", "pub const ", "const ", "pub static ", "static ", "pub type ", "type ",
                    "macro_rules! ", "pub(crate) struct ", "pub(crate) enum ", "pub(crate) const ",
                ])
        }
        "py" => indent <= 4 && starts_with_any(&["def ", "async def ", "class "]),
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" => {
            indent == 0
                && starts_with_any(&[
                    "export function ", "export async function ", "export default function ", "export class ",
                    "export default class ", "export const ", "export interface ", "export type ", "export enum ",
                    "function ", "async function ", "class ", "interface ", "type ", "enum ",
                ])
        }
        "go" => indent == 0 && starts_with_any(&["func ", "type "]),
        "rb" => indent <= 2 && starts_with_any(&["def ", "class ", "module "]),
        "java" | "kt" | "swift" | "cs" | "scala" => {
            indent <= 4
                && (starts_with_any(&["class ", "interface ", "enum ", "struct ", "protocol ", "extension ", "object ", "fun ", "func "])
                    || (indent == 4
                        && starts_with_any(&["public ", "private ", "protected ", "static ", "override ", "internal ", "open "])
                        && body.contains('(')
                        && !body.trim_end().ends_with(';')))
        }
        "c" | "h" | "cc" | "cpp" | "hpp" => {
            indent == 0
                && body.contains('(')
                && !body.starts_with('#')
                && !body.starts_with("//")
                && !body.starts_with("/*")
                && !body.starts_with('}')
                && (body.trim_end().ends_with('{') || body.trim_end().ends_with(')'))
        }
        _ => false,
    }
}

fn signature(line: &str) -> String {
    let body = line.trim();
    let cut = body
        .find(" {")
        .or_else(|| body.find('{'))
        .map(|at| body[..at].trim_end())
        .unwrap_or(body);
    let cut = cut.trim_end_matches(':').trim_end();
    if cut.chars().count() > OUTLINE_ENTRY_CHARS {
        let mut short: String = cut.chars().take(OUTLINE_ENTRY_CHARS - 1).collect();
        short.push('…');
        short
    } else {
        cut.to_string()
    }
}

/// The bare name of a definition line: the first identifier after its
/// keywords (`pub async fn merged_env(` -> `merged_env`, `impl TuiApp {` -> `TuiApp`).
pub fn short_name(line: &str) -> String {
    const KEYWORDS: &[&str] = &[
        "pub", "pub(crate)", "async", "fn", "struct", "enum", "trait", "impl", "mod", "const", "static", "type", "def", "class",
        "function", "export", "default", "func", "interface", "public", "private", "protected", "override", "internal", "open",
        "fun", "object", "protocol", "extension", "module", "macro_rules!",
    ];
    let body = line.trim();
    for word in body.split(|c: char| c.is_whitespace()) {
        let word = word.trim_matches(|c: char| c == '{' || c == ':' || c == '(' || c == ',');
        if word.is_empty() || KEYWORDS.contains(&word) {
            continue;
        }
        let name: String = word.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
        if !name.is_empty() {
            return name;
        }
    }
    body.chars().take(20).collect()
}

/// The name of the definition enclosing `index` (0-based line), from the
/// nearest preceding definition line: `Some("merged_env")` for a line inside
/// `fn merged_env`. Used by GREP to say which function a hit sits in, so
/// orientation needs fewer READs. Rust: a method's enclosing `impl` block is
/// skipped in favour of the method; lines before any definition get None.
pub fn enclosing_definition(ext: &str, lines: &[&str], index: usize) -> Option<String> {
    let indent = |line: &str| line.len() - line.trim_start().len();
    let mut cursor = index.min(lines.len().saturating_sub(1));
    let target_indent = indent(lines[cursor]);
    loop {
        let line = lines[cursor];
        if is_definition(ext, line) && (cursor == index || indent(line) < target_indent || target_indent == 0) {
            return Some(short_name(line));
        }
        if cursor == 0 {
            return None;
        }
        cursor -= 1;
    }
}

/// The 0-based index of the last line of the definition that starts at
/// `start`: for brace languages the line that closes the block opened on
/// (or within two lines of) the definition line, for Python and Ruby the
/// last line indented deeper than the definition (Ruby's closing `end`
/// included). A signature-only line (`struct Unit;`, a type alias, an
/// abstract method) ends on itself. Brace counting ignores strings and
/// comments, which is good enough to bound a READ window.
pub fn definition_end(ext: &str, lines: &[&str], start: usize) -> usize {
    let last = lines.len().saturating_sub(1);
    let start = start.min(last);
    let indent = |line: &str| line.len() - line.trim_start().len();
    match ext {
        "py" | "rb" => {
            let base = indent(lines[start]);
            let mut end = start;
            for (index, line) in lines.iter().enumerate().skip(start + 1) {
                if line.trim().is_empty() {
                    continue;
                }
                if indent(line) <= base {
                    if ext == "rb" && line.trim() == "end" && indent(line) == base {
                        return index;
                    }
                    return end;
                }
                end = index;
            }
            end
        }
        _ => {
            let mut depth: i64 = 0;
            let mut opened = false;
            for (index, line) in lines.iter().enumerate().skip(start) {
                for ch in line.chars() {
                    match ch {
                        '{' => {
                            depth += 1;
                            opened = true;
                        }
                        '}' => depth -= 1,
                        _ => {}
                    }
                }
                if opened && depth <= 0 {
                    return index;
                }
                if !opened && line.trim_end().ends_with(';') {
                    return index;
                }
                if !opened && index >= start + 8 {
                    return start;
                }
            }
            if opened {
                last
            } else {
                start
            }
        }
    }
}

/// The outline of one file: "<display> (N lines): 12 pub fn a; 40 struct B; …",
/// or None when the file is small, unreadable, or has no recognisable
/// definitions.
pub fn file_outline(path: &Path, display: &str) -> Option<String> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let text = std::fs::read_to_string(path).ok()?;
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() < OUTLINE_MIN_LINES {
        return None;
    }
    let definitions: Vec<(usize, &str)> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| is_definition(ext, line))
        .map(|(index, line)| (index + 1, *line))
        .collect();
    if definitions.is_empty() {
        return None;
    }
    let shown = definitions.len().min(OUTLINE_MAX_ENTRIES);
    let entries: Vec<String> = definitions[..shown].iter().map(|(number, line)| format!("{number} {}", signature(line))).collect();
    let mut out = format!("{display} ({} lines): {}", lines.len(), entries.join("; "));
    let rest = &definitions[shown..];
    if !rest.is_empty() {
        let compact_shown = rest.len().min(OUTLINE_MAX_COMPACT);
        let compact: Vec<String> = rest[..compact_shown].iter().map(|(number, line)| format!("{}@{number}", short_name(line))).collect();
        out.push_str(&format!("; then (name@line) {}", compact.join(" ")));
        if rest.len() > compact_shown {
            out.push_str(&format!("; +{} more", rest.len() - compact_shown));
        }
    }
    Some(out)
}

/// Workspace-relative paths named in `texts`, first-mentioned first, deduplicated.
pub fn named_paths_for_texts(texts: &[&str]) -> Vec<String> {
    let mut paths: Vec<String> = Vec::new();
    for text in texts {
        for rel in crate::harness::r#loop::extract_goal_paths(text) {
            if !paths.contains(&rel) {
                paths.push(rel);
            }
        }
    }
    paths
}

/// Outlines for the workspace files named in `texts` (task title, notes,
/// goal), first-mentioned first, at most OUTLINE_MAX_FILES; None when no
/// named file earns one.
pub fn outlines_for_texts(cwd: &str, texts: &[&str]) -> Option<String> {
    outlines_for_paths(cwd, &named_paths_for_texts(texts))
}

/// Names the goal uses as a qualifier — `Store` in `Store.set(...)`, `Run`
/// in `Run::new` — even when the name alone is not an identifier token
/// (no underscore, no inner capital). Such a name is almost always the
/// class or type the task edits, and its file is the first READ of the run.
pub fn qualified_type_names(texts: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for text in texts {
        for raw in text.split(|c: char| c.is_whitespace() || "`'\"(),;<>[]{}=+*!?&|#".contains(c)) {
            if raw.contains('/') {
                continue;
            }
            let Some(head) = raw.split("::").next().and_then(|part| part.split('.').next()) else { continue };
            let qualifies = raw.len() > head.len() && (raw[head.len()..].starts_with('.') || raw[head.len()..].starts_with("::"));
            let member = &raw[head.len()..];
            let member_is_name = member.trim_start_matches(|c| c == '.' || c == ':').chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
            if !qualifies || !member_is_name || head.len() < 3 || head.len() > 40 {
                continue;
            }
            if !head.chars().next().is_some_and(|c| c.is_ascii_uppercase()) || !head.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                continue;
            }
            if !out.iter().any(|seen| seen == head) {
                out.push(head.to_string());
            }
        }
    }
    out
}

/// Words the goal names that should locate a definition file: identifier
/// tokens, qualifier type names, and backticked words, first-mentioned first.
pub const DEFINITION_FILES_MAX_WORDS: usize = 8;

/// Workspace files that define a name the goal uses (`class Store` for
/// `Store.set`, `def truncate_middle` for `truncate_middle(text, width)`),
/// source trees before test-like trees, deduplicated, skipping `skip`. An
/// exact-name definition is preferred over one that merely contains the word.
pub fn definition_files_for_texts(cwd: &str, texts: &[&str], skip: &[String]) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    for word in extract_goal_symbols(texts).into_iter().chain(qualified_type_names(texts)).chain(extract_quoted_words(texts)) {
        if !words.iter().any(|seen| seen.eq_ignore_ascii_case(&word)) {
            words.push(word);
        }
    }
    let mut files: Vec<String> = Vec::new();
    let started = std::time::Instant::now();
    for word in words.iter().take(DEFINITION_FILES_MAX_WORDS) {
        if started.elapsed().as_millis() > SYMBOL_HITS_TIME_BUDGET_MS {
            break;
        }
        let Some(hits) = git_grep_definitions(cwd, word) else { break };
        let mut exact: Vec<String> = Vec::new();
        let mut partial: Vec<String> = Vec::new();
        for hit in &hits {
            let mut parts = hit.splitn(3, ':');
            let (path, _, text) = (parts.next().unwrap_or(""), parts.next(), parts.next().unwrap_or(""));
            if path.is_empty() || skip.iter().any(|s| s == path) {
                continue;
            }
            let bucket = if short_name(text).eq_ignore_ascii_case(word) { &mut exact } else { &mut partial };
            if !bucket.iter().any(|p| p == path) {
                bucket.push(path.to_string());
            }
        }
        let mut chosen = if exact.is_empty() { partial } else { exact };
        chosen.sort_by_key(|path| test_like_path(path));
        for path in chosen {
            if !files.iter().any(|f| f == &path) {
                files.push(path);
            }
        }
    }
    files
}

/// Header of the section that carries named files whole.
pub const NAMED_FILE_BODIES_HEADER: &str = "files the goal names or that define a name it uses, already read (a READ of these paths returns exactly this text — start from PATCH, and READ one only after a PATCH changed it):";

/// The numbered text of the named files short enough to carry whole (at most
/// NAMED_FILE_BODY_MAX_LINES lines, NAMED_FILE_BODIES_MAX_FILES files,
/// NAMED_FILE_BODIES_MAX_CHARS in total), in the form a READ returns, plus
/// the paths carried so the caller can skip their outlines. Missing,
/// directory, empty, and non-UTF-8 paths are skipped.
pub fn named_file_bodies_for_paths(cwd: &str, paths: &[String]) -> (Option<String>, Vec<String>) {
    let mut blocks: Vec<String> = Vec::new();
    let mut carried: Vec<String> = Vec::new();
    let mut chars = 0usize;
    for rel in paths {
        let full = Path::new(cwd).join(rel);
        if !full.is_file() {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&full) else { continue };
        let lines: Vec<&str> = text.lines().collect();
        if lines.is_empty() || lines.len() > NAMED_FILE_BODY_MAX_LINES {
            continue;
        }
        let numbered: Vec<String> = lines
            .iter()
            .enumerate()
            .map(|(index, line)| {
                let shown: String = if line.chars().count() > NAMED_FILE_LINE_CHARS {
                    format!("{}[line truncated: {} chars total]", line.chars().take(NAMED_FILE_LINE_CHARS).collect::<String>(), line.chars().count())
                } else {
                    (*line).to_string()
                };
                format!("{}\t{shown}", index + 1)
            })
            .collect();
        let block = format!("== {rel} ({} lines)\n{}", lines.len(), numbered.join("\n"));
        let block_chars = block.chars().count();
        if chars + block_chars > NAMED_FILE_BODIES_MAX_CHARS {
            continue;
        }
        chars += block_chars;
        blocks.push(block);
        carried.push(rel.clone());
        if blocks.len() >= NAMED_FILE_BODIES_MAX_FILES {
            break;
        }
    }
    if blocks.is_empty() {
        (None, carried)
    } else {
        (Some(format!("{NAMED_FILE_BODIES_HEADER}\n{}", blocks.join("\n"))), carried)
    }
}

/// Outlines for explicit workspace-relative paths (files that do not exist or
/// are too small to outline are skipped), capped at OUTLINE_MAX_FILES.
pub fn outlines_for_paths(cwd: &str, paths: &[String]) -> Option<String> {
    let mut outlines: Vec<String> = Vec::new();
    for rel in paths {
        let full = Path::new(cwd).join(rel);
        if !full.is_file() {
            continue;
        }
        if let Some(outline) = file_outline(&full, rel) {
            outlines.push(outline);
            if outlines.len() >= OUTLINE_MAX_FILES {
                break;
            }
        }
    }
    if outlines.is_empty() {
        None
    } else {
        Some(outlines.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, body).unwrap();
        path
    }

    fn big_rust() -> String {
        let mut s = String::from("use std::io;\n\npub struct Run {\n    x: i64,\n}\n\nimpl Run {\n    pub fn new() -> Run {\n        Run { x: 0 }\n    }\n\n    fn helper(&self) {}\n}\n\nfn main() {\n");
        for _ in 0..OUTLINE_MIN_LINES {
            s.push_str("    let _ = 1;\n");
        }
        s.push_str("}\n");
        s
    }

    #[test]
    fn small_files_and_unknown_languages_get_no_outline() {
        let dir = std::env::temp_dir().join(format!("outline-{}", std::process::id()));
        let small = write(&dir, "small.rs", "fn a() {}\nfn b() {}\n");
        assert_eq!(file_outline(&small, "small.rs"), None);
        let unknown = write(&dir, "notes.txt", &"line\n".repeat(OUTLINE_MIN_LINES + 1));
        assert_eq!(file_outline(&unknown, "notes.txt"), None);
    }

    #[test]
    fn rust_outline_lists_definitions_with_line_numbers() {
        let dir = std::env::temp_dir().join(format!("outline-rs-{}", std::process::id()));
        let path = write(&dir, "src/run.rs", &big_rust());
        let outline = file_outline(&path, "src/run.rs").expect("outline");
        assert!(outline.starts_with(&format!("src/run.rs ({} lines): 3 pub struct Run; 7 impl Run; 8 pub fn new() -> Run; 12 fn helper(&self); 15 fn main()", OUTLINE_MIN_LINES + 16)), "{outline}");
    }

    #[test]
    fn outlines_follow_the_paths_the_texts_name() {
        let dir = std::env::temp_dir().join(format!("outline-texts-{}", std::process::id()));
        write(&dir, "src/run.rs", &big_rust());
        write(&dir, "src/tiny.rs", "fn a() {}\n");
        let cwd = dir.to_string_lossy().to_string();
        assert!(outlines_for_texts(&cwd, &["fix `src/tiny.rs`"]).is_none());
        let got = outlines_for_texts(&cwd, &["tidy src/tiny.rs and src/run.rs", "src/run.rs again"]).expect("outline");
        assert_eq!(got.matches("src/run.rs (").count(), 1, "one outline per file: {got}");
        assert!(outlines_for_texts(&cwd, &["src/missing.rs"]).is_none());
    }

    #[test]
    fn goal_symbols_pick_identifiers_not_prose_or_paths() {
        let goal = "Add counters `hedges_fired` and hedges_won to RoleInferenceTotals and HarnessRun.role_inference in src/harness/loop.rs; serialise them in the `roleInference` map. Run `cargo test --lib harness`. PATCH the README.";
        let symbols = extract_goal_symbols(&[goal]);
        assert_eq!(
            symbols,
            vec!["hedges_fired", "hedges_won", "RoleInferenceTotals", "HarnessRun", "role_inference", "roleInference"],
            "{symbols:?}"
        );
        assert!(extract_goal_symbols(&["fix the bug in the parser"]).is_empty());
        let many: Vec<String> = (0..20).map(|i| format!("sym_{i}")).collect();
        assert_eq!(extract_goal_symbols(&[&many.join(" ")]).len(), SYMBOL_HITS_MAX_SYMBOLS);
    }

    #[test]
    fn symbol_hits_come_from_git_grep() {
        let dir = std::env::temp_dir().join(format!("drip-symbol-hits-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cwd = dir.to_string_lossy().into_owned();
        assert!(symbol_hits_for_texts(&cwd, &["touch role_totals"]).is_none(), "not a repo");
        assert!(std::process::Command::new("git").args(["init", "-q"]).current_dir(&dir).status().unwrap().success());
        write(&dir, "src/a.rs", "pub struct RoleTotals {}\nfn use_role_totals(x: RoleTotals) {}\n");
        write(&dir, "src/b.rs", "// RoleTotalsX is not a whole-word hit\n");
        let hits = symbol_hits_for_texts(&cwd, &["extend RoleTotals and missing_name"]).unwrap();
        assert!(hits.starts_with("symbol hits (git grep -nw"), "{hits}");
        assert!(hits.contains("RoleTotals: src/a.rs:1 pub struct RoleTotals {} | src/a.rs:2 fn use_role_totals(x: RoleTotals) {}"), "{hits}");
        assert!(!hits.contains("src/b.rs"), "{hits}");
        assert!(hits.contains("missing_name: no hits"), "{hits}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn enclosing_definition_names_the_nearest_definition_above() {
        let text = "use std::io;\n\nimpl Run {\n    pub fn new() -> Run {\n        Run { x: 0 }\n    }\n\n    fn helper(&self) {\n        let y = 1;\n    }\n}\n\nfn main() {\n    println!();\n}\n";
        let lines: Vec<&str> = text.split('\n').collect();
        assert_eq!(enclosing_definition("rs", &lines, 0), None, "before any definition");
        assert_eq!(enclosing_definition("rs", &lines, 4).as_deref(), Some("new"));
        assert_eq!(enclosing_definition("rs", &lines, 8).as_deref(), Some("helper"));
        assert_eq!(enclosing_definition("rs", &lines, 13).as_deref(), Some("main"));
        assert_eq!(enclosing_definition("rs", &lines, 3).as_deref(), Some("new"), "the definition line itself");
        let py: Vec<&str> = "class A:\n    def f(self):\n        return 1\n\n\ndef g():\n    pass\n".split('\n').collect();
        assert_eq!(enclosing_definition("py", &py, 2).as_deref(), Some("f"));
        assert_eq!(enclosing_definition("py", &py, 6).as_deref(), Some("g"));
    }

    #[test]
    fn long_outlines_are_capped() {
        let dir = std::env::temp_dir().join(format!("outline-cap-{}", std::process::id()));
        let mut body = String::new();
        for i in 0..(OUTLINE_MAX_ENTRIES + OUTLINE_MAX_COMPACT + 5) {
            body.push_str(&format!("    pub async fn f{i}(&self) {{}}\n\n\n\n"));
        }
        let path = write(&dir, "many.rs", &body);
        let outline = file_outline(&path, "many.rs").expect("outline");
        let last_full = OUTLINE_MAX_ENTRIES - 1;
        assert!(outline.contains(&format!("pub async fn f{last_full}(&self)")), "{outline}");
        assert!(outline.contains(&format!("; then (name@line) f{}@", OUTLINE_MAX_ENTRIES)), "{outline}");
        let last_compact = OUTLINE_MAX_ENTRIES + OUTLINE_MAX_COMPACT - 1;
        assert!(outline.contains(&format!(" f{last_compact}@")), "{outline}");
        assert!(outline.ends_with("; +5 more"), "{outline}");
        assert_eq!(short_name("impl TuiApp {"), "TuiApp");
        assert_eq!(short_name("    pub(crate) fn merged_env(&self) -> BTreeMap<String, String> {"), "merged_env");
        assert_eq!(short_name("export default class Foo extends Bar {"), "Foo");
    }

    #[test]
    fn definition_end_bounds_brace_and_indent_blocks() {
        let rs = "fn alpha(\n    a: u32,\n) -> u32 {\n    if a > 1 {\n        return 2;\n    }\n    a\n}\n\nstruct Unit;\nfn beta() {}\n";
        let lines: Vec<&str> = rs.lines().collect();
        assert_eq!(definition_end("rs", &lines, 0), 7);
        assert_eq!(definition_end("rs", &lines, 9), 9);
        assert_eq!(definition_end("rs", &lines, 10), 10);
        let py = "def alpha(x):\n    if x:\n        return 1\n\n    return 2\n\ndef beta():\n    pass\n";
        let lines: Vec<&str> = py.lines().collect();
        assert_eq!(definition_end("py", &lines, 0), 4);
        assert_eq!(definition_end("py", &lines, 6), 7);
        let rb = "def alpha\n  1\nend\ndef beta\n  2\nend\n";
        let lines: Vec<&str> = rb.lines().collect();
        assert_eq!(definition_end("rb", &lines, 0), 2);
        let unterminated = "fn open() {\n    let x = 1;\n";
        let lines: Vec<&str> = unterminated.lines().collect();
        assert_eq!(definition_end("rs", &lines, 0), 1);
    }

    #[test]
    fn quoted_words_find_the_definitions_named_after_them() {
        assert_eq!(extract_quoted_words(&["the predicate whose name contains `title` and calls `RoleTotals`"]), vec!["title".to_string()]);
        assert!(extract_quoted_words(&["run `cargo test --lib` and `x`"]).is_empty());
        let dir = std::env::temp_dir().join(format!("drip-definition-hits-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cwd = dir.to_string_lossy().into_owned();
        assert!(definition_hits_for_texts(&cwd, &["`title`"]).is_none(), "not a repo");
        assert!(std::process::Command::new("git").args(["init", "-q"]).current_dir(&dir).status().unwrap().success());
        write(&dir, "src/a.rs", "pub struct TitleRoute {}\n    fn should_request_title(x: u32) -> bool {\n        let title = x;\n        title > 1\n    }\n");
        write(&dir, "src/b.py", "def make_title():\n    return 1\n");
        let fixture: String = (0..30).map(|i| format!("def pad_title_{i}(x):\n    return x\n")).collect();
        write(&dir, "evals/fixture/textutil.py", &fixture);
        let hits = definition_hits_for_texts(&cwd, &["find the predicate whose name contains `title`"]).unwrap();
        assert!(hits.starts_with("definition hits ("), "{hits}");
        assert!(hits.contains("title: src/a.rs: 1 pub struct TitleRoute; 2 fn should_request_title(x: u32) -> bool | src/b.py: 1 def make_title() | evals/fixture/textutil.py: 1 def pad_title_0(x); 3 def pad_title_1(x); "), "{hits}");
        assert!(hits.contains("; +24 more"), "{hits}");
        assert!(!hits.contains("let title"), "{hits}");
        assert!(test_like_path("evals/fixture/textutil.py") && test_like_path("src/foo_test.go") && !test_like_path("src/tui/app.rs"));
        assert!(definition_hits_for_texts(&cwd, &["`nothing`"]).unwrap().contains("nothing: no definition names contain it"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn short_named_files_are_carried_whole_and_long_ones_are_not() {
        let dir = std::env::temp_dir().join(format!("drip-named-bodies-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write(&dir, "kv/store.py", "class Store:\n    def get(self, k):\n        return self.d[k]\n");
        let long: String = (0..(NAMED_FILE_BODY_MAX_LINES + 1)).map(|i| format!("x{i} = {i}\n")).collect();
        write(&dir, "kv/big.py", &long);
        write(&dir, "kv/empty.py", "");
        let cwd = dir.to_string_lossy().to_string();
        let paths = named_paths_for_texts(&["fix kv/store.py, kv/big.py, kv/empty.py and kv/missing.py"]);
        assert_eq!(paths, vec!["kv/store.py", "kv/big.py", "kv/empty.py", "kv/missing.py"]);
        let (section, carried) = named_file_bodies_for_paths(&cwd, &paths);
        let section = section.expect("store.py is carried");
        assert!(section.starts_with(NAMED_FILE_BODIES_HEADER), "{section}");
        assert!(section.contains("== kv/store.py (3 lines)\n1\tclass Store:\n2\t    def get(self, k):\n3\t        return self.d[k]"), "{section}");
        assert!(!section.contains("big.py") && !section.contains("empty.py") && !section.contains("missing.py"), "{section}");
        assert_eq!(carried, vec!["kv/store.py"]);
        let (none, carried) = named_file_bodies_for_paths(&cwd, &["kv/big.py".to_string()]);
        assert!(none.is_none() && carried.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn qualifier_type_names_and_definition_files_locate_the_class_the_goal_edits() {
        assert_eq!(qualified_type_names(&["Add TTL: Store.set(key, ttl=None); Run::new() and kv.store.Store.get, not a/b.py or 3.14"]), vec!["Store", "Run"]);
        assert!(qualified_type_names(&["plain words. Sentence ends. store.set"]).is_empty());
        let dir = std::env::temp_dir().join(format!("drip-def-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(std::process::Command::new("git").args(["init", "-q"]).current_dir(&dir).status().unwrap().success());
        write(&dir, "kv/store.py", "class Store:\n    def set(self, k, v):\n        pass\n");
        write(&dir, "kv/util.py", "def store_setup():\n    pass\n");
        write(&dir, "tests/test_store.py", "class StoreTest:\n    def test_set(self):\n        pass\n");
        let cwd = dir.to_string_lossy().to_string();
        let files = definition_files_for_texts(&cwd, &["Add TTL to Store.set(key) and the `set` subcommand"], &[]);
        assert_eq!(files, vec!["kv/store.py"], "exact `class Store` and `def set` beat `StoreTest`, `store_setup` and `test_set`");
        let files = definition_files_for_texts(&cwd, &["Store.set"], &["kv/store.py".to_string()]);
        assert_eq!(files, vec!["kv/util.py", "tests/test_store.py"], "without an exact hit the containing names count, source trees first");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn carried_bodies_stop_at_the_file_and_char_budgets() {
        let dir = std::env::temp_dir().join(format!("drip-named-budget-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut paths = Vec::new();
        for i in 0..(NAMED_FILE_BODIES_MAX_FILES + 1) {
            write(&dir, &format!("m/f{i}.py"), "a = 1\n");
            paths.push(format!("m/f{i}.py"));
        }
        let cwd = dir.to_string_lossy().to_string();
        let (_, carried) = named_file_bodies_for_paths(&cwd, &paths);
        assert_eq!(carried.len(), NAMED_FILE_BODIES_MAX_FILES);
        let wide: String = (0..300).map(|_| format!("{}\n", "w".repeat(200))).collect();
        write(&dir, "m/wide.py", &wide);
        let (section, carried) = named_file_bodies_for_paths(&cwd, &["m/wide.py".to_string(), "m/f0.py".to_string()]);
        assert_eq!(carried, vec!["m/f0.py"], "the 60K-char file is skipped, the small one after it still carried");
        assert!(section.unwrap().contains("== m/f0.py"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
