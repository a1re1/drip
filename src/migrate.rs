// lci → drip migration (`--migrate-from-lci [--from <dir>] [--dry-run] [--project]`)
// — drip-only feature per drip/PLAN.md; there is no direct TS source file.
//
// Copies, never moves, an lci home (~/.lci or $LCI_HOME) into the drip home
// (~/.drip or $DRIP_HOME), plus optionally the current project's `.lci` →
// `.drip`. File formats (config.json, index.sqlite schema, session files) are
// identical between the two tools; only home paths and `lci --resume` command
// strings inside session files need rewriting (see rewrite helpers in task-4).
//
// Rules:
//   * Existing destination files are left alone unless byte-identical, in
//     which case they count as "skipped (exists)" — the migration is
//     idempotent: a second run skips everything.
//   * `--dry-run` prints the plan without writing anything.
//   * `--project` also copies `<project>/.lci` → `<project>/.drip`
//     (patches.jsonl, async-tools/, skills/, roles.json, plugins.json,
//     policy.json).
//   * env.vars is copied with mode 0600 preserved. index.sqlite is copied
//     as raw bytes together with any -wal/-shm sidecars; the WAL checkpoint
//     and the in-file rewrites of lci paths/commands are follow-ups.
//   * Prints a summary table (copied/skipped per category) and exits 0;
//     exits 1 with a clear message when the source home does not exist.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Home-root entries that migrate category by category. Anything not listed
/// (e.g. `bin/`, the top-level `index.sqlite` index drip does not use) stays
/// behind — copying it would change behavior rather than preserve it.
const HOME_CATEGORIES: &[&str] = &["config.json", "env.vars", "skills", "marketplaces", "projects"];

/// Entries copied from `<project>/.lci` → `<project>/.drip` with `--project`.
/// patch_journal.rs and friends read exactly these; anything else in `.lci`
/// (e.g. `sessions/`, the pre-move index) belongs to the home-tree copy.
const PROJECT_CATEGORIES: &[&str] = &[
    "patches.jsonl",
    "async-tools",
    "skills",
    "roles.json",
    "plugins.json",
    "policy.json",
];

pub struct MigrateOptions {
    /// `--from <dir>` override; default is $LCI_HOME or ~/.lci.
    pub from: Option<String>,
    /// `--dry-run`: print the plan, write nothing.
    pub dry_run: bool,
    /// `--project`: also copy `<cwd>/.lci` → `<cwd>/.drip`.
    pub project: bool,
    /// Override for the destination home; production passes None so the real
    /// DRIP_HOME/~ resolution runs. Tests point both ends at tempdirs.
    pub to: Option<String>,
    /// Override for the project root used by `--project`; production passes
    /// None (meaning the current directory). Tests pass a tempdir path.
    pub project_root: Option<String>,
}

impl Default for MigrateOptions {
    fn default() -> Self {
        Self { from: None, dry_run: false, project: false, to: None, project_root: None }
    }
}

/// The lci home: `$LCI_HOME` when set, else `~/.lci`.
///
/// (Mirrors src/cli/home.ts's resolveLciHome, with the rename held off until
/// the caller decides which side of the migration the path serves.)
pub fn lci_home_default() -> Result<PathBuf> {
    if let Some(home) = std::env::var_os("LCI_HOME") {
        if !home.is_empty() {
            return Ok(PathBuf::from(home));
        }
    }
    let home = dirs::home_dir().context("could not determine the home directory")?;
    Ok(home.join(".lci"))
}

/// The drip home: `$DRIP_HOME` when set, else `~/.drip`.
pub fn drip_home_default() -> Result<PathBuf> {
    if let Some(home) = std::env::var_os("DRIP_HOME") {
        if !home.is_empty() {
            return Ok(PathBuf::from(home));
        }
    }
    let home = dirs::home_dir().context("could not determine the home directory")?;
    Ok(home.join(".drip"))
}

/// One copied (or skipped, or rewritten) file or directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanEntry {
    pub category: String,
    /// Display path with the destination root swapped for `<home>` / `<project>`.
    pub display: String,
    /// true when the destination file exists and is byte-identical.
    pub skipped: bool,
    /// true when file content was rewritten during the copy (task-4).
    pub rewritten: bool,
    /// true when the content rewriting planned in task-4 would fire here.
    pub would_rewrite: bool,
}

impl PlanEntry {
    pub fn status_line(&self) -> String {
        let action = if self.skipped {
            "skipped (exists)"
        } else if self.would_rewrite {
            "copy+rewrite"
        } else {
            "copy"
        };
        format!("  {action:16} {}\n", self.display)
    }
}

/// The completed migration plan + what actually happened.
#[derive(Debug, Default)]
pub struct MigrationReport {
    pub entries: Vec<PlanEntry>,
    pub dry_run: bool,
}

impl MigrationReport {
    /// Summary table: one row per category with copied/skipped/rewritten
    /// counts, plus a total line. Mirrors the shape of lci's per-category
    /// storage listing (projects listed individually).
    pub fn summary(&self) -> String {
        let mut categories: BTreeMap<&str, [usize; 3]> = BTreeMap::new();
        for entry in &self.entries {
            let counters = categories.entry(entry.category.as_str()).or_insert([0, 0, 0]);
            if entry.skipped {
                counters[1] += 1;
            } else if entry.rewritten {
                counters[2] += 1;
            } else {
                counters[0] += 1;
            }
        }
        let mut out = String::new();
        let _ = writeln!(out, "{}", if self.dry_run { "migration plan (dry run):" } else { "migration complete:" });
        if categories.is_empty() {
            let _ = writeln!(out, "  nothing to migrate");
            return out;
        }
        let _ = writeln!(out, "  {:18} {:>8} {:>8} {:>9}", "category", "copied", "skipped", "rewritten");
        let mut totals = [0usize; 3];
        for (category, counters) in &categories {
            let _ = writeln!(
                out,
                "  {:18} {:>8} {:>8} {:>9}",
                category,
                counters[0],
                counters[1],
                counters[2]
            );
            for i in 0..3 {
                totals[i] += counters[i];
            }
        }
        let _ = writeln!(out, "  {:18} {:>8} {:>8} {:>9}", "total", totals[0], totals[1], totals[2]);
        out
    }
}

/// Resolves the source and destination homes, validates that the source
/// exists, and runs the migration.
pub fn migrate_from_lci(options: &MigrateOptions) -> Result<MigrationReport> {
    let from: PathBuf = match &options.from {
        Some(dir) => PathBuf::from(dir),
        None => lci_home_default()?,
    };
    let to: PathBuf = match &options.to {
        Some(dir) => PathBuf::from(dir),
        None => drip_home_default()?,
    };
    if !from.exists() {
        return Err(anyhow::anyhow!(
            "drip: migration source {} does not exist — pass --from <dir> or set $LCI_HOME.",
            from.display()
        ));
    }
    let mut report = MigrationReport { entries: Vec::new(), dry_run: options.dry_run };

    migrate_dir(&from, &to, HOME_CATEGORIES, "home", "<home>", options, &mut report)?;
    if options.project {
        let project_root = match &options.project_root {
            Some(root) => PathBuf::from(root),
            None => std::env::current_dir().context("could not determine the current directory")?,
        };
        let from_project = project_root.join(".lci");
        let to_project = project_root.join(".drip");
        if !from_project.exists() {
            let _ = writeln!(std::io::stdout(), "no .lci directory in {} — skipping project migration", project_root.display());
        } else {
            migrate_dir(&from_project, &to_project, PROJECT_CATEGORIES, "project", "<project>", options, &mut report)?;
        }
    }
    Ok(report)
}

/// Runs the category-by-category copy from `from`/`to` roots. `display_root`
/// replaces the destination root in printed paths so the summary stays
/// stable regardless of where the migration lands.
fn migrate_dir(
    from: &Path,
    to: &Path,
    categories: &[&str],
    category_scope: &str,
    display_root: &str,
    options: &MigrateOptions,
    report: &mut MigrationReport,
) -> Result<()> {
    for name in categories {
        let src = from.join(name);
        if !src.exists() {
            continue; // category absent on this side; nothing to do
        }
        let dst = to.join(name);
        if src.is_dir() {
            migrate_tree(
                &src,
                &dst,
                &src,
                &dst,
                category_scope,
                display_root,
                options,
                report,
            )?;
        } else {
            migrate_file(&src, &dst, category_scope, display_root, options, report)?;
        }
    }
    Ok(())
}

/// Recursively walks a directory tree, copying every file it contains. The
/// relative layout under `src_root` is preserved under `dst_root`.
fn migrate_tree(
    src: &Path,
    _dst: &Path,
    src_root: &Path,
    dst_root: &Path,
    category_scope: &str,
    display_root: &str,
    options: &MigrateOptions,
    report: &mut MigrationReport,
) -> Result<()> {
    for entry in walkdir::WalkDir::new(src).sort_by_file_name().contents_first(false) {
        let entry = entry.with_context(|| format!("could not walk {}", src.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let relative = path
            .strip_prefix(src_root)
            .with_context(|| format!("could not relativize {}", path.display()))?;
        let destination = dst_root.join(relative);
        migrate_file(path, &destination, category_scope, display_root, options, report)?;
    }
    Ok(())
}

/// Copies one file, preserving mode 0600 for env.vars and applying the
/// byte-identical skip rule for everything else. Returns whether the file
/// was copied, skipped, or rewritten. When `dry_run` is set nothing is
/// written — only the plan entry is recorded.
fn migrate_file(
    src: &Path,
    dst: &Path,
    category_scope: &str,
    display_root: &str,
    options: &MigrateOptions,
    report: &mut MigrationReport,
) -> Result<()> {
    let bytes = fs::read(src).with_context(|| format!("could not read {}", src.display()))?;

    // Byte-identical destination files are left alone and reported as
    // "skipped (exists)" — the migration is idempotent.
    if let Ok(existing) = fs::read(dst) {
        if existing == bytes {
            report.entries.push(PlanEntry {
                category: category_scope.to_string(),
                display: display_path(dst, display_root),
                skipped: true,
                rewritten: false,
                would_rewrite: false,
            });
            return Ok(());
        }
    }

    let needs_rewrite = crate::migrate::rewrite::needs_rewrite_for_bytes(&bytes);
    report.entries.push(PlanEntry {
        category: category_scope.to_string(),
        display: display_path(dst, display_root),
        skipped: false,
        // Content rewriting (paths / `.lci/` segments / `lci --resume` commands)
        // is a follow-up: until it lands, never claim a rewrite happened.
        rewritten: false,
        would_rewrite: needs_rewrite,
    });

    if options.dry_run {
        return Ok(());
    }

    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).with_context(|| format!("could not create {}", parent.display()))?;
    }
    // env.vars holds credentials and lives with mode 0600; preserve that.
    if src.file_name().and_then(|n| n.to_str()) == Some("env.vars") {
        fs::write(dst, &bytes).with_context(|| format!("could not write {}", dst.display()))?;
        set_mode_0600(dst)?;
    } else {
        fs::write(dst, &bytes).with_context(|| format!("could not write {}", dst.display()))?;
    }
    Ok(())
}

/// Strips the destination prefix so output stays stable across machines;
/// `<home>/skills/foo.md` reads better than an absolute tempdir path.
fn display_path(dst: &Path, display_root: &str) -> String {
    format!("{}/{}", display_root, dst.file_name().and_then(|n| n.to_str()).unwrap_or("<file>"))
}

fn set_mode_0600(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("could not set permissions on {}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

pub mod rewrite {
    //! Path/command string rewriting inside copied session files (task-4).

    use anyhow::Result;
    use std::path::Path;

    /// Reads a file's bytes and returns whether any lci-origin path or
    /// `lci --resume` command strings inside it need rewriting. Binary-safe:
    /// non-UTF-8 files (index.sqlite) report false — only text session files
    /// are rewritten.
    pub fn needs_rewrite_for_bytes(bytes: &[u8]) -> bool {
        let Ok(text) = std::str::from_utf8(bytes) else {
            return false;
        };
        super::needs_rewrite(text)
    }

    pub fn needs_rewrite_file(path: &Path) -> Result<bool> {
        let bytes = std::fs::read(path)?;
        Ok(needs_rewrite_for_bytes(&bytes))
    }
}

/// Placeholder for the real task-4 rewriting logic; byte-level detection of
/// `lci`-origin paths and command strings lives there.
pub fn needs_rewrite(text: &str) -> bool {
    text.contains(".lci/") || text.contains("lci --resume")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use tempfile::tempdir;

    /// The canonical lci home shape used across these tests (task-4 expands
    /// it with a real sqlite index and session files):
    ///   config.json, env.vars (0600), skills/<file>, marketplaces/registry
    ///   projects/<slug>/{index.sqlite, memory/, sessions/<id>/{...}}
    fn fake_lci_home(root: &Path) -> PathBuf {
        let home = root.join("lci-home");
        fs::create_dir_all(home.join("skills")).unwrap();
        fs::create_dir_all(home.join("marketplaces")).unwrap();
        fs::create_dir_all(home.join("projects/-com-example-app")).unwrap();
        fs::create_dir_all(home.join("projects/-com-example-app/memory")).unwrap();
        fs::create_dir_all(home.join("projects/-com-example-app/sessions/abc")).unwrap();
        fs::write(home.join("config.json"), b"{\"model\":\"sonnet\"}").unwrap();
        fs::write(home.join("env.vars"), b"ANTHROPIC_API_KEY=sk-test\n").unwrap();
        fs::write(home.join("skills/tdd.md"), b"# tdd\n").unwrap();
        fs::write(home.join("marketplaces/registry.json"), b"{}").unwrap();
        fs::write(
            home.join("projects/-com-example-app/index.sqlite"),
            b"placeholder-bytes",
        )
        .unwrap();
        fs::write(home.join("projects/-com-example-app/memory/MEMORY.md"), b"# mem\n").unwrap();
        fs::write(home.join("projects/-com-example-app/sessions/abc/session.json"), b"{}").unwrap();
        home
    }

    fn options_from(from: &Path, to: &Path) -> MigrateOptions {
        MigrateOptions {
            from: Some(from.display().to_string()),
            to: Some(to.display().to_string()),
            ..MigrateOptions::default()
        }
    }

    #[test]
    fn migrates_the_whole_home_tree_with_categories() {
        let root = tempdir().unwrap();
        let home = fake_lci_home(root.path());
        let drip_home = root.path().join("drip-home");

        let report = migrate_from_lci(&options_from(&home, &drip_home)).unwrap();
        assert_eq!(report.entries.len(), 7);
        assert!(report.entries.iter().all(|e| !e.skipped));
        assert_eq!(report.summary().lines().count(), 4);

        // Destination tree mirrors the source layout.
        assert_eq!(fs::read(drip_home.join("config.json")).unwrap(), b"{\"model\":\"sonnet\"}");
        assert_eq!(fs::read(drip_home.join("skills/tdd.md")).unwrap(), b"# tdd\n");
        assert_eq!(
            fs::read(drip_home.join("projects/-com-example-app/sessions/abc/session.json")).unwrap(),
            b"{}"
        );
        // Source is untouched — copy, never move.
        assert!(home.join("config.json").exists());
    }

    #[test]
    fn env_vars_keeps_mode_0600() {
        let root = tempdir().unwrap();
        let home = fake_lci_home(root.path());
        fs::set_permissions(home.join("env.vars"), fs::Permissions::from_mode(0o600)).unwrap();
        let mode = fs::metadata(home
            .join("env.vars"))
            .unwrap()
            .permissions()
            .mode();
        let drip_home = root.path().join("drip-home");

        migrate_from_lci(&options_from(&home, &drip_home)).unwrap();
        let copied = fs::metadata(drip_home.join("env.vars")).unwrap().permissions().mode();
        assert_eq!(copied & 0o777, 0o600);
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn second_run_skips_everything() {
        let root = tempdir().unwrap();
        let home = fake_lci_home(root.path());
        let drip_home = root.path().join("drip-home");
        let options = options_from(&home, &drip_home);

        migrate_from_lci(&options).unwrap();
        let second = migrate_from_lci(&options).unwrap();
        assert!(second.entries.iter().all(|e| e.skipped), "expected every entry skipped on the second run");
        assert_eq!(second.entries.len(), 7);
    }

    #[test]
    fn dry_run_writes_nothing() {
        let root = tempdir().unwrap();
        let home = fake_lci_home(root.path());
        let drip_home = root.path().join("drip-home");

        let report = migrate_from_lci(&MigrateOptions {
            dry_run: true,
            ..options_from(&home, &drip_home)
        })
        .unwrap();
        assert!(report.dry_run);
        assert_eq!(report.entries.len(), 7);
        assert!(!drip_home.exists(), "dry run must not create the destination tree");
    }

    #[test]
    fn missing_source_is_a_clear_error() {
        let root = tempdir().unwrap();
        let missing = root.path().join("nope");
        let err = migrate_from_lci(&options_from(&missing, &root.path().join("to"))).unwrap_err();
        assert!(err.to_string().contains("does not exist"), "unexpected error: {err}");
    }

    #[test]
    fn project_flag_copies_the_local_lci_dir() {
        let root = tempdir().unwrap();
        let project = root.path().join("repo");
        fs::create_dir_all(project.join(".lci/async-tools")).unwrap();
        fs::write(project.join(".lci/patches.jsonl"), b"[]\n").unwrap();
        fs::write(project.join(".lci/async-tools/job.json"), b"{}\n").unwrap();
        fs::write(project.join(".lci/untracked.txt"), b"stay behind\n").unwrap();

        let home = fake_lci_home(root.path());
        let drip_home = root.path().join("drip-home");
        let report = migrate_from_lci(&MigrateOptions {
            project: true,
            project_root: Some(project.display().to_string()),
            ..options_from(&home, &drip_home)
        })
        .unwrap();

        // 7 home entries + 2 project entries (patches.jsonl, async-tools/job.json).
        assert_eq!(report.entries.len(), 9);
        assert_eq!(fs::read(project.join(".drip/patches.jsonl")).unwrap(), b"[]\n");
        assert_eq!(fs::read(project.join(".drip/async-tools/job.json")).unwrap(), b"{}\n");
        assert!(!project.join(".drip/untracked.txt").exists(), "files outside PROJECT_CATEGORIES stay behind");
    }

    #[test]
    fn project_flag_without_a_local_lci_dir_is_a_noop() {
        let root = tempdir().unwrap();
        let project = root.path().join("repo");
        fs::create_dir_all(&project).unwrap();
        let home = fake_lci_home(root.path());

        let report = migrate_from_lci(&MigrateOptions {
            project: true,
            project_root: Some(project.display().to_string()),
            ..options_from(&home, &root.path().join("drip-home"))
        })
        .unwrap();
        assert_eq!(report.entries.len(), 7);
        assert!(!project.join(".drip").exists());
    }

    #[test]
    fn plan_entry_status_lines_match_the_reported_actions() {
        let copied = PlanEntry {
            category: "home".into(),
            display: "<home>/config.json".into(),
            skipped: false,
            rewritten: false,
            would_rewrite: false,
        };
        assert!(copied.status_line().starts_with("  copy            "));
        let skipped = PlanEntry { skipped: true, ..copied.clone() };
        assert!(skipped.status_line().contains("skipped (exists)"));
        let rewritten = PlanEntry {
            category: "home".into(),
            display: "<home>/x".into(),
            skipped: false,
            rewritten: true,
            would_rewrite: true,
        };
        assert!(rewritten.status_line().starts_with("  copy+rewrite"));
    }
}
