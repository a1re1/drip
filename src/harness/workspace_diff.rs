// What a task loop changed in the workspace, from git rather than from the
// tools' own edit records: a script or a shell command that rewrites files
// shows up here exactly like a PATCH does. The working tree is snapshotted as
// a tree object through a throwaway index, so the user's index, HEAD and
// branch are never touched.
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::core::types::LoopChangedFile;

/// Files carried with their diff in one event; the rest are counted only.
pub const LOOP_CHANGES_MAX_FILES: usize = 20;
pub const LOOP_CHANGES_MAX_DIFF_LINES: usize = 200;
pub const LOOP_CHANGES_MAX_DIFF_BYTES: usize = 16_000;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LoopChanges {
    pub files: Vec<LoopChangedFile>,
    /// Totals over every changed file, including those past the file cap.
    pub file_count: usize,
    pub added: i64,
    pub removed: i64,
    pub truncated: bool,
}

impl LoopChanges {
    /// `2 files changed (+14 −3)`.
    pub fn headline(&self) -> String {
        format!(
            "{} file{} changed (+{} −{})",
            self.file_count,
            if self.file_count == 1 { "" } else { "s" },
            self.added,
            self.removed
        )
    }
}

static SNAPSHOT_SEQ: AtomicU64 = AtomicU64::new(0);

fn git(cwd: &str, index: Option<&PathBuf>, args: &[&str]) -> Option<String> {
    let mut command = Command::new("git");
    command
        .args(["-c", "core.quotePath=false"])
        .args(args)
        .current_dir(cwd);
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

/// The working tree (tracked edits plus untracked files, .gitignore
/// respected) written as a tree object; None outside git or when git fails.
/// The temporary index starts as a copy of the real one so unchanged files
/// keep their stat cache and are not rehashed.
pub fn snapshot_tree(cwd: &str) -> Option<String> {
    let real_index = git(cwd, None, &["rev-parse", "--path-format=absolute", "--git-path", "index"])?;
    let real_index = PathBuf::from(real_index.trim());
    let temp_index = std::env::temp_dir().join(format!(
        "drip-loop-index-{}-{}",
        std::process::id(),
        SNAPSHOT_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    if real_index.is_file() && std::fs::copy(&real_index, &temp_index).is_err() {
        return None;
    }
    let tree = git(cwd, Some(&temp_index), &["add", "-A"])
        .and_then(|_| git(cwd, Some(&temp_index), &["write-tree"]))
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty());
    let _ = std::fs::remove_file(&temp_index);
    tree
}

/// The changes between two snapshots, None when they are identical or git
/// fails.
pub fn diff_trees(cwd: &str, before: &str, after: &str) -> Option<LoopChanges> {
    if before == after {
        return None;
    }
    let numstat = git(cwd, None, &["diff", "--numstat", "--no-renames", before, after])?;
    let patch = git(
        cwd,
        None,
        &["diff", "--no-color", "--no-ext-diff", "--no-renames", "-U2", before, after],
    )
    .unwrap_or_default();
    let changes = parse_changes(&numstat, &patch);
    if changes.file_count == 0 {
        return None;
    }
    Some(changes)
}

/// `git diff --numstat` lines joined with the per-file sections of the patch.
/// Both list the same files in the same order; when they disagree the diffs
/// are dropped and the counts kept.
pub fn parse_changes(numstat: &str, patch: &str) -> LoopChanges {
    let stats: Vec<(&str, &str, &str)> = numstat
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, '\t');
            Some((parts.next()?, parts.next()?, parts.next()?))
        })
        .collect();
    let mut sections: Vec<&str> = Vec::new();
    let mut start: Option<usize> = None;
    let mut offset = 0;
    for line in patch.split_inclusive('\n') {
        if line.starts_with("diff --git ") {
            if let Some(start) = start {
                sections.push(&patch[start..offset]);
            }
            start = Some(offset);
        }
        offset += line.len();
    }
    if let Some(start) = start {
        sections.push(&patch[start..]);
    }
    let aligned = sections.len() == stats.len();

    let mut changes = LoopChanges {
        file_count: stats.len(),
        truncated: stats.len() > LOOP_CHANGES_MAX_FILES,
        ..Default::default()
    };
    for (index, (added, removed, path)) in stats.iter().enumerate() {
        let binary = *added == "-";
        let added = added.parse::<i64>().unwrap_or(0);
        let removed = removed.parse::<i64>().unwrap_or(0);
        changes.added += added;
        changes.removed += removed;
        if index >= LOOP_CHANGES_MAX_FILES {
            continue;
        }
        let section = if aligned { sections[index] } else { "" };
        let status = if section.lines().take(4).any(|line| line.starts_with("new file mode")) {
            "added"
        } else if section.lines().take(4).any(|line| line.starts_with("deleted file mode")) {
            "deleted"
        } else {
            "modified"
        };
        let diff = if binary {
            None
        } else {
            section.find("\n@@").map(|at| {
                let (text, cut) = cap_diff(&section[at + 1..]);
                changes.truncated |= cut;
                text
            })
        };
        changes.files.push(LoopChangedFile {
            path: path.to_string(),
            added,
            removed,
            status: status.to_string(),
            binary,
            diff,
        });
    }
    changes
}

fn cap_diff(hunks: &str) -> (String, bool) {
    let mut out = String::new();
    for (count, line) in hunks.lines().enumerate() {
        if count >= LOOP_CHANGES_MAX_DIFF_LINES || out.len() + line.len() > LOOP_CHANGES_MAX_DIFF_BYTES {
            return (out, true);
        }
        out.push_str(line);
        out.push('\n');
    }
    (out, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(cwd: &std::path::Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .expect("git runs");
        assert!(status.status.success(), "git {args:?} failed");
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        run(dir.path(), &["init", "-q"]);
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored/\n").unwrap();
        run(dir.path(), &["add", "-A"]);
        run(dir.path(), &["commit", "-q", "-m", "init"]);
        dir
    }

    #[test]
    fn consecutive_snapshots_diff_tracked_untracked_and_script_edits() {
        let dir = repo();
        let cwd = dir.path().to_string_lossy().to_string();
        let before = snapshot_tree(&cwd).expect("snapshot in a git repo");

        std::fs::write(dir.path().join("a.txt"), "one\nTWO\nthree\n").unwrap();
        std::fs::write(dir.path().join("new.txt"), "fresh\n").unwrap();
        std::fs::create_dir(dir.path().join("ignored")).unwrap();
        std::fs::write(dir.path().join("ignored/skip.txt"), "no\n").unwrap();
        // A change no edit tool made: a shell command rewriting a file.
        let status = Command::new("sh")
            .args(["-c", "printf 'four\\n' >> a.txt"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success());

        let after = snapshot_tree(&cwd).unwrap();
        let changes = diff_trees(&cwd, &before, &after).expect("trees differ");
        assert_eq!(changes.headline(), "2 files changed (+3 −1)");
        let paths: Vec<&str> = changes.files.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(paths, vec!["a.txt", "new.txt"]);
        assert_eq!(changes.files[0].status, "modified");
        assert_eq!(changes.files[1].status, "added");
        let diff = changes.files[0].diff.as_deref().unwrap();
        assert!(diff.starts_with("@@ "), "{diff}");
        assert!(diff.contains("-two\n+TWO\n") && diff.contains("+four\n"), "{diff}");

        // The next loop diffs from this snapshot, not from run start.
        std::fs::remove_file(dir.path().join("new.txt")).unwrap();
        let last = snapshot_tree(&cwd).unwrap();
        let changes = diff_trees(&cwd, &after, &last).unwrap();
        assert_eq!(changes.headline(), "1 file changed (+0 −1)");
        assert_eq!(changes.files[0].status, "deleted");
    }

    #[test]
    fn snapshot_leaves_the_index_and_head_alone() {
        let dir = repo();
        let cwd = dir.path().to_string_lossy().to_string();
        std::fs::write(dir.path().join("new.txt"), "fresh\n").unwrap();
        let status_before = git(&cwd, None, &["status", "--porcelain"]).unwrap();
        let head_before = git(&cwd, None, &["rev-parse", "HEAD"]).unwrap();
        snapshot_tree(&cwd).unwrap();
        assert_eq!(git(&cwd, None, &["status", "--porcelain"]).unwrap(), status_before);
        assert_eq!(status_before, "?? new.txt\n");
        assert_eq!(git(&cwd, None, &["rev-parse", "HEAD"]).unwrap(), head_before);
    }

    #[test]
    fn identical_trees_and_non_git_dirs_yield_nothing() {
        let dir = repo();
        let cwd = dir.path().to_string_lossy().to_string();
        let tree = snapshot_tree(&cwd).unwrap();
        assert_eq!(snapshot_tree(&cwd).unwrap(), tree);
        assert!(diff_trees(&cwd, &tree, &tree).is_none());

        let plain = tempfile::tempdir().unwrap();
        assert!(snapshot_tree(&plain.path().to_string_lossy()).is_none());
    }

    #[test]
    fn binary_files_carry_no_diff_and_caps_mark_truncation() {
        let numstat = "-\t-\tlogo.png\n300\t0\tbig.txt\n";
        let mut patch = String::from(
            "diff --git a/big.txt b/big.txt\nnew file mode 100644\n--- /dev/null\n+++ b/big.txt\n@@ -0,0 +1,300 @@\n",
        );
        for n in 0..300 {
            patch.push_str(&format!("+line {n}\n"));
        }
        let patch = format!("diff --git a/logo.png b/logo.png\nBinary files a/logo.png and b/logo.png differ\n{patch}");
        let changes = parse_changes(numstat, &patch);
        assert!(changes.files[0].binary && changes.files[0].diff.is_none());
        assert_eq!(changes.files[1].status, "added");
        assert_eq!(
            changes.files[1].diff.as_deref().unwrap().lines().count(),
            LOOP_CHANGES_MAX_DIFF_LINES
        );
        assert!(changes.truncated);
        assert_eq!((changes.added, changes.removed), (300, 0));
    }
}
