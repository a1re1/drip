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
/// Longest signature kept per entry.
const OUTLINE_ENTRY_CHARS: usize = 90;

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
    let entries: Vec<String> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| is_definition(ext, line))
        .map(|(index, line)| format!("{} {}", index + 1, signature(line)))
        .collect();
    if entries.is_empty() {
        return None;
    }
    let shown = entries.len().min(OUTLINE_MAX_ENTRIES);
    let mut out = format!("{display} ({} lines): {}", lines.len(), entries[..shown].join("; "));
    if entries.len() > shown {
        out.push_str(&format!("; +{} more", entries.len() - shown));
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
    fn long_outlines_are_capped() {
        let dir = std::env::temp_dir().join(format!("outline-cap-{}", std::process::id()));
        let mut body = String::new();
        for i in 0..(OUTLINE_MAX_ENTRIES + 5) {
            body.push_str(&format!("fn f{i}() {{}}\n\n\n\n"));
        }
        let path = write(&dir, "many.rs", &body);
        let outline = file_outline(&path, "many.rs").expect("outline");
        assert!(outline.ends_with("; +5 more"), "{outline}");
    }
}
