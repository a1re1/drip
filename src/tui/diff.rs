//! The "what changed" block a task loop leaves in scrollback: one headline,
//! then each changed file with a short line-numbered inline diff.
//!
//! Pure rendering over a "loop-changes" event (see harness::workspace_diff);
//! the full diff stays in the transcript, this is a preview.

use crate::core::types::{HarnessEventData, LoopChangedFile};
use crate::watch::ansi::{c, fit};

/// Files previewed per block; the rest are counted.
pub const MAX_PREVIEW_FILES: usize = 6;
/// Diff lines previewed per file; the rest are counted.
pub const MAX_PREVIEW_LINES: usize = 12;

/// Painted rows for one "loop-changes" event (no trailing newlines).
pub fn render_loop_changes(detail: &str, data: Option<&HarnessEventData>, width: usize) -> Vec<String> {
    let mut rows = vec![String::new(), format!("{} {}", c::green("●"), c::bold(&headline(detail)))];
    let files = data.and_then(|data| data.files.as_deref()).unwrap_or_default();
    for file in files.iter().take(MAX_PREVIEW_FILES) {
        rows.push(file_row(file, width));
        if file.status != "deleted" {
            rows.extend(diff_rows(file.diff.as_deref().unwrap_or_default(), width));
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

fn diff_rows(diff: &str, width: usize) -> Vec<String> {
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
        let (sign, number, paint): (char, usize, fn(&str) -> String) = match line.chars().next() {
            Some('+') => {
                new_line += 1;
                ('+', new_line - 1, c::green)
            }
            Some('-') => {
                old_line += 1;
                ('-', old_line - 1, c::red)
            }
            Some('\\') => continue,
            _ => {
                old_line += 1;
                new_line += 1;
                (' ', new_line - 1, c::dim)
            }
        };
        if shown >= MAX_PREVIEW_LINES {
            hidden += 1;
            continue;
        }
        shown += 1;
        let text = line.get(1..).unwrap_or_default();
        rows.push(paint(&clip(&format!("    {number:>4} {sign} {text}"), width)));
    }
    if hidden > 0 {
        rows.push(c::dim(&format!(
            "         … +{hidden} more line{}",
            if hidden == 1 { "" } else { "s" }
        )));
    }
    rows
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
        assert!(rows[4].ends_with('…'), "{rows:?}");
        assert_eq!(rows[5], "         ⋯");
        assert_eq!(rows[6], "      40 - x");
    }

    #[test]
    fn an_event_without_data_still_renders_its_headline() {
        let rows = render_loop_changes("1 file changed (+1 −0)", None, 80);
        assert_eq!(strip_ansi(&rows[1]), "● Changed 1 file (+1 −0)");
        assert_eq!(rows.len(), 2);
    }
}
