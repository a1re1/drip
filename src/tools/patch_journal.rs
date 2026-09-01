// port of src/tools/patch-journal.ts
//
// PATCH used to write files in place: a crash or stop landing mid-write left
// a truncated source file with no record of what was there. Writes now go
// temp+rename (same-directory, so the rename is atomic), and every edit
// appends a journal entry with the pre-image so `drip --undo-last` can walk
// changes back.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Re-exported for callers that import write_file_atomic from this module.
pub use crate::lib_fs::write_file_atomic;

/// One JSON line in `.drip/patches.jsonl`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PatchJournalEntry {
    pub at: String,
    pub path: String,
    /// Sha of the content the patch wrote — undo refuses when the file moved on.
    pub post_sha256: String,
    /// Full previous content; None when the patch created the file (undo deletes it),
    /// or omitted for oversized pre-images.
    pub pre_image: Option<String>,
    /// Set when the pre-image was too large to journal — the entry is not undoable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pre_image_elided: Option<bool>,
}

/// Pre-images above this size are not journaled (the journal is not a VCS).
pub const MAX_PRE_IMAGE_CHARS: usize = 1_000_000;

pub fn sha256(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

// Mirrors resolve_lci_project's git-style discovery (nearest .drip, then
// nearest .git, then the start dir) so a run started in a subdirectory
// journals to the same .drip its session lives in, not a stray nested one.
pub fn find_journal_root(start_dir: &Path) -> PathBuf {
    let mut dir = start_dir.to_path_buf();
    let mut git_root: Option<PathBuf> = None;

    loop {
        if dir.join(".drip").exists() {
            return dir;
        }

        if git_root.is_none() && dir.join(".git").exists() {
            git_root = Some(dir.clone());
        }

        // Path::parent() returns None at the filesystem root where the TS
        // loop's dirname(dir) === dir check fires — fall back the same way.
        match dir.parent() {
            Some(parent) => dir = parent.to_path_buf(),
            None => return git_root.unwrap_or_else(|| start_dir.to_path_buf()),
        }
    }
}

pub fn patch_journal_path(workspace_root: &Path) -> PathBuf {
    find_journal_root(workspace_root).join(".drip").join("patches.jsonl")
}

pub fn append_patch_journal(
    workspace_root: &Path,
    entry: &AppendPatchJournalEntry,
) {
    let journal_path = patch_journal_path(workspace_root);
    let elided =
        entry.pre_image.as_deref().map_or(false, |pre| pre.chars().count() > MAX_PRE_IMAGE_CHARS);
    let record = PatchJournalEntry {
        at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        path: entry.path.clone(),
        post_sha256: sha256(&entry.post_content),
        pre_image: if elided { None } else { entry.pre_image.clone() },
        pre_image_elided: if elided { Some(true) } else { None },
    };

    let line = match serde_json::to_string(&record) {
        Ok(line) => line,
        Err(_) => return, // Journaling is best-effort: an unwritable .drip must not fail the edit.
    };

    // TS: mkdirSync(dirname, {recursive}) + appendFileSync — a true O_APPEND
    // append, so concurrent writers never clobber each other and a read error
    // can never discard the prior undo history. Best-effort: an unwritable
    // .drip must not fail the edit.
    if let Some(parent) = journal_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&journal_path) {
        use std::io::Write;
        let _ = file.write_all(line.as_bytes()).and_then(|_| file.write_all(b"\n"));
    }
}

/// Input to [`append_patch_journal`] — the TS call-site object shape.
#[derive(Debug, Clone)]
pub struct AppendPatchJournalEntry {
    pub path: String,
    pub post_content: String,
    pub pre_image: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum UndoOutcome {
    Undone { path: String },
    Deleted { path: String },
    Refused { path: String, why: String },
    Empty,
}

// Walks the journal backwards, restoring pre-images. Each undone entry is
// removed from the journal so repeated --undo-last calls keep walking back.
pub fn undo_last_patches(workspace_root: &Path, count: usize) -> Vec<UndoOutcome> {
    let journal_path = patch_journal_path(workspace_root);

    if !journal_path.exists() {
        return vec![UndoOutcome::Empty];
    }

    let mut lines: Vec<String> = crate::lib_fs::read_jsonl_records::<PatchJournalEntry>(&journal_path)
        .into_iter()
        .map(|record| record.raw)
        .collect();
    let mut outcomes: Vec<UndoOutcome> = Vec::new();
    let mut remaining = count;

    while remaining > 0 && !lines.is_empty() {
        let raw = lines[lines.len() - 1].clone();
        let entry: PatchJournalEntry = match serde_json::from_str(&raw) {
            Ok(entry) => entry,
            Err(_) => {
                lines.pop();
                continue;
            }
        };

        if entry.pre_image_elided == Some(true) {
            outcomes.push(UndoOutcome::Refused {
                path: entry.path.clone(),
                why: "the pre-image was too large to journal — restore it from git instead"
                    .to_string(),
            });
            lines.pop();
            remaining -= 1;
            continue;
        }

        let current_content = std::fs::read_to_string(&entry.path).ok();

        if current_content.as_deref().map_or(true, |current| sha256(current) != entry.post_sha256) {
            // The file moved on since this patch — undoing would destroy newer work.
            outcomes.push(UndoOutcome::Refused {
                path: entry.path.clone(),
                why: "the file changed after this patch (or was deleted) — undo would clobber newer work"
                    .to_string(),
            });
            lines.pop();
            remaining -= 1;
            continue;
        }

        if entry.pre_image.is_none() {
            let _ = std::fs::remove_file(&entry.path);
            outcomes.push(UndoOutcome::Deleted { path: entry.path.clone() });
        } else {
            let _ = crate::lib_fs::write_file_atomic(
                Path::new(&entry.path),
                entry.pre_image.as_deref().unwrap_or_default(),
                false,
            );
            outcomes.push(UndoOutcome::Undone { path: entry.path.clone() });
        }

        lines.pop();
        remaining -= 1;
    }

    if outcomes.is_empty() {
        outcomes.push(UndoOutcome::Empty);
    }

    let rewritten = if lines.is_empty() {
        String::new()
    } else {
        let mut out = lines.join("\n");
        out.push('\n');
        out
    };
    let _ = std::fs::write(&journal_path, rewritten);

    outcomes
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // Port of makeTempRoot + the anchor dir from test/patch-journal.test.ts.
    // Anchor discovery in the temp root so tests never climb into a real
    // repo above tmp.
    fn make_root() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join(".drip")).unwrap();
        root
    }

    // it("writes atomically without leaving temp files")
    #[test]
    fn writes_atomically_without_leaving_temp_files() {
        let root = make_root();
        let target = root.path().join("file.ts");

        write_file_atomic(&target, "content", false).unwrap();

        assert_eq!(fs::read_to_string(&target).unwrap(), "content");
        let temp_files: Vec<String> = fs::read_dir(root.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("drip-tmp"))
            .collect();
        assert!(temp_files.is_empty());
    }

    // it("journals to the discovered .lci root, not a stray nested one")
    #[test]
    fn journals_to_the_discovered_root_not_a_stray_nested_one() {
        let root = make_root();
        let subdir = root.path().join("packages").join("app");

        fs::create_dir_all(&subdir).unwrap();

        assert_eq!(find_journal_root(&subdir), root.path());

        append_patch_journal(
            &subdir,
            &AppendPatchJournalEntry {
                path: subdir.join("x.ts").to_string_lossy().into_owned(),
                post_content: "post".to_string(),
                pre_image: Some("pre".to_string()),
            },
        );

        assert!(patch_journal_path(root.path()).exists());
        assert!(!subdir.join(".drip").exists());
    }

    // it("undoes edits in reverse order and deletes files a patch created")
    #[test]
    fn undoes_edits_in_reverse_order_and_deletes_files_a_patch_created() {
        let root = make_root();
        let edited = root.path().join("edited.ts");
        let created = root.path().join("created.ts");

        fs::write(&edited, "v1").unwrap();
        write_file_atomic(&edited, "v2", false).unwrap();
        append_patch_journal(
            root.path(),
            &AppendPatchJournalEntry {
                path: edited.to_string_lossy().into_owned(),
                post_content: "v2".to_string(),
                pre_image: Some("v1".to_string()),
            },
        );
        write_file_atomic(&created, "new file", false).unwrap();
        append_patch_journal(
            root.path(),
            &AppendPatchJournalEntry {
                path: created.to_string_lossy().into_owned(),
                post_content: "new file".to_string(),
                pre_image: None,
            },
        );

        let outcomes = undo_last_patches(root.path(), 2);

        assert_eq!(
            outcomes,
            vec![
                UndoOutcome::Deleted { path: created.to_string_lossy().into_owned() },
                UndoOutcome::Undone { path: edited.to_string_lossy().into_owned() },
            ]
        );
        assert!(!created.exists());
        assert_eq!(fs::read_to_string(&edited).unwrap(), "v1");
        // The journal drained.
        assert_eq!(undo_last_patches(root.path(), 1), vec![UndoOutcome::Empty]);
    }

    // it("refuses to undo a file that changed after the patch")
    #[test]
    fn refuses_to_undo_a_file_that_changed_after_the_patch() {
        let root = make_root();
        let target = root.path().join("file.ts");

        fs::write(&target, "v1").unwrap();
        write_file_atomic(&target, "v2", false).unwrap();
        append_patch_journal(
            root.path(),
            &AppendPatchJournalEntry {
                path: target.to_string_lossy().into_owned(),
                post_content: "v2".to_string(),
                pre_image: Some("v1".to_string()),
            },
        );
        fs::write(&target, "v3-by-someone-else").unwrap();

        let outcomes = undo_last_patches(root.path(), 1);

        assert!(
            matches!(&outcomes[0], UndoOutcome::Refused { .. }),
            "expected refused, got {:?}",
            outcomes[0]
        );
        assert_eq!(fs::read_to_string(&target).unwrap(), "v3-by-someone-else");
    }
}
