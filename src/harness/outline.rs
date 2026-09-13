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

fn is_definition(ext: &str, line: &str) -> bool {
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
fn short_name(line: &str) -> String {
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

/// Outlines for the workspace files named in `texts` (task title, notes,
/// goal), first-mentioned first, at most OUTLINE_MAX_FILES; None when no
/// named file earns one.
pub fn outlines_for_texts(cwd: &str, texts: &[&str]) -> Option<String> {
    let mut seen: Vec<String> = Vec::new();
    let mut outlines: Vec<String> = Vec::new();
    for text in texts {
        for rel in crate::harness::r#loop::extract_goal_paths(text) {
            if seen.contains(&rel) {
                continue;
            }
            seen.push(rel.clone());
            let full = Path::new(cwd).join(&rel);
            if !full.is_file() {
                continue;
            }
            if let Some(outline) = file_outline(&full, &rel) {
                outlines.push(outline);
                if outlines.len() >= OUTLINE_MAX_FILES {
                    return Some(outlines.join("\n"));
                }
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
}
