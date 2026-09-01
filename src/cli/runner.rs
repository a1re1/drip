// port of src/cli/runner.ts
//
// This file currently carries only capture_workspace_baseline; the rest of
// runner.ts (seedInitialState, runCliGoal, …) is ported separately.

use std::path::Path;
use std::process::Command;

fn run_git(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).current_dir(cwd).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Result of [`capture_workspace_baseline`] — the TS anonymous return shape
/// `{ contextNote, note, sha }`.
pub struct WorkspaceBaseline {
    pub context_note: String,
    pub note: String,
    pub sha: String,
}

// A dirty tree at run start is the operator uncommitted work sitting in
// the model blast zone. `git stash create` builds a stash commit WITHOUT
// touching the worktree; pinning it under refs/drip/baseline/ gives the
// operator a guaranteed restore point (git stash apply <sha>) at zero cost.
//
// TS returns a Promise; the Rust port runs the same blocking git calls
// synchronously with identical observable behavior. Any failure (not a git
// repo, git unavailable) is best-effort: returns None instead of failing.
pub fn capture_workspace_baseline(cwd: &Path, session_id: &str) -> Option<WorkspaceBaseline> {
    let status = run_git(cwd, &["status", "--porcelain"])?;

    if status.trim().is_empty() {
        return None;
    }

    let stash_out = run_git(
        cwd,
        &[
            "stash",
            "create",
            &format!("drip baseline before session {session_id}"),
        ],
    )?;
    let sha = stash_out.trim().to_string();

    if sha.is_empty() {
        return None;
    }

    run_git(
        cwd,
        &[
            "update-ref",
            &format!("refs/drip/baseline/{session_id}"),
            &sha,
        ],
    )?;

    Some(WorkspaceBaseline {
        // The model-visible variant carries no restore command: a dogfooded
        // worker executed the "git stash apply <sha>" from the old note
        // mid-run (N9) — re-applying a baseline over in-progress edits would
        // silently revert work. The command stays on the operator-facing
        // event note below.
        context_note: format!(
            "The working tree had uncommitted changes at run start; a baseline snapshot is pinned at refs/drip/baseline/{session_id} for the operator to recover pre-run state after the run. Never apply or restore it during the run — treat it as read-only bookkeeping."
        ),
        note: format!(
            "The working tree had uncommitted changes at run start; a baseline snapshot is pinned at refs/drip/baseline/{session_id} ({}) — restorable with: git stash apply {}",
            &sha[..12],
            &sha[..12]
        ),
        sha,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    use super::capture_workspace_baseline;

    fn run_git(cwd: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git should be available");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    // it("pins a stash ref for dirty trees and skips clean ones")
    #[test]
    fn pins_a_stash_ref_for_dirty_trees_and_skips_clean_ones() {
        let root = tempfile::TempDir::with_prefix("drip-baseline-").unwrap();

        run_git(root.path(), &["init", "-q"]);
        run_git(
            root.path(),
            &[
                "-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "--allow-empty", "-m", "init",
            ],
        );

        assert!(capture_workspace_baseline(root.path(), "session-1").is_none());

        fs::write(root.path().join("work.txt"), "uncommitted").unwrap();
        run_git(root.path(), &["add", "work.txt"]);

        let baseline =
            capture_workspace_baseline(root.path(), "session-1").expect("dirty tree should pin a baseline");

        assert!(baseline.note.contains("refs/drip/baseline/session-1"));

        let ref_sha = run_git(root.path(), &["rev-parse", "refs/drip/baseline/session-1"]);
        assert_eq!(ref_sha.trim(), baseline.sha);
        // The worktree itself was not touched.
        assert_eq!(
            fs::read_to_string(root.path().join("work.txt")).unwrap(),
            "uncommitted"
        );
    }

    // it("returns null outside a git repo instead of failing the run")
    #[test]
    fn returns_null_outside_a_git_repo_instead_of_failing_the_run() {
        let root = tempfile::TempDir::with_prefix("drip-baseline-").unwrap();

        assert!(capture_workspace_baseline(root.path(), "session-2").is_none());
    }
}
