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
//   * env.vars is copied with mode 0600 preserved. index.sqlite is never
//     opened at the source (not even read-only, not even for --dry-run):
//     its bytes plus any -wal/-shm sidecars are copied to a temp directory,
//     the copy is checkpointed with PRAGMA wal_checkpoint(TRUNCATE), and
//     the checkpointed file is what is compared against and written to the
//     destination.
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
    /// true when file content was rewritten during the copy.
    pub rewritten: bool,
    /// true when the content rewriting would fire here.
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
            // Counts are per FILE: a rewritten file was also copied, so it
            // tallies once in each column; skipped files tally only there.
            if entry.skipped {
                counters[1] += 1;
            } else {
                counters[0] += 1;
                if entry.rewritten {
                    counters[2] += 1;
                }
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

/// Rewrites lci-origin strings inside copied session files (task-4): paths
/// under the old home move to the new home, `.lci/` segments become `.drip/`,
/// and `lci --resume`/`lci --result` command strings become `drip ...` with
/// the same arguments.
pub mod rewrite {
    use super::*;

    /// The session files whose content carries lci paths and command strings.
    const REWRITTEN_SESSION_FILES: [&str; 4] =
        ["session.json", "state.json", "result.json", "transcript.jsonl"];

    /// true when this file is one of the session files that get rewritten.
    pub fn is_session_file(name: &str) -> bool {
        REWRITTEN_SESSION_FILES.contains(&name)
    }

    /// true when the bytes contain anything the rewriting would change (any
    /// mention of `lci` — a path under the old home, a `.lci/` segment, or an
    /// `lci --resume`-style command).
    pub fn needs_rewrite_for_bytes(bytes: &[u8]) -> bool {
        bytes.windows(3).any(|window| window == b"lci")
    }

    /// The bytes to write for a copy: `<from-home>/...` paths become
    /// `<to-home>/...`, `.lci/` segments become `.drip/`, and `lci --resume` /
    /// `lci --result` command strings become `drip ...`.
    pub fn rewrite_for_copy(bytes: &[u8], from_home: &Path, to_home: &Path) -> Vec<u8> {
        let text = String::from_utf8_lossy(bytes);
        // Home-prefixed paths first — they swallow the `.lci/` inside the old
        // home path itself, so the segment rule below only hits other `.lci/`.
        let from_prefix = format!("{}/", from_home.display());
        let mut out = if text.contains(from_prefix.as_str()) {
            text.replace(from_prefix.as_str(), &format!("{}/", to_home.display()))
        } else {
            text.into_owned()
        };
        out = out.replace(".lci/", ".drip/");
        out = out.replace("lci --resume", "drip --resume");
        out = out.replace("lci --result", "drip --result");
        out.into_bytes()
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
                from,
                to,
                category_scope,
                display_root,
                options,
                report,
            )?;
        } else {
            migrate_file(&src, &dst, category_scope, display_root, from, to, options, report)?;
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
    from_home: &Path,
    to_home: &Path,
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
        // index.sqlite -wal/-shm sidecars are folded into the checkpointed
        // database copy; they are not separate files and never reach the
        // destination, so they get no plan entry and no count.
        let name = entry.file_name().to_str().unwrap_or("");
        if name == "index.sqlite-wal" || name == "index.sqlite-shm" {
            continue;
        }
        let path = entry.path();
        let relative = path
            .strip_prefix(src_root)
            .with_context(|| format!("could not relativize {}", path.display()))?;
        let destination = dst_root.join(relative);
        migrate_file(
            path,
            &destination,
            category_scope,
            display_root,
            from_home,
            to_home,
            options,
            report,
        )?;
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
    from_home: &Path,
    to_home: &Path,
    options: &MigrateOptions,
    report: &mut MigrationReport,
) -> Result<()> {
    // index.sqlite is not copied byte-for-byte: the -wal sidecar may hold
    // committed-but-not-checkpointed rows, so a raw copy could lose them.
    // Checkpointing a copy of the database into the destination folds the
    // sidecar into the file and leaves no -wal/-shm behind (task-4).
    if src.file_name().and_then(|n| n.to_str()) == Some("index.sqlite") {
        return migrate_sqlite_index(src, dst, category_scope, display_root, options, report);
    }

    let bytes = fs::read(src).with_context(|| format!("could not read {}", src.display()))?;

    // Only the session files carry lci paths/commands to rewrite; config.json,
    // env.vars and every other category copy byte-for-byte. The rewritten
    // bytes are computed up front because the skip comparison must run
    // against what WOULD be written — otherwise a rewritten file that already
    // sits in the destination gets re-copied on every run.
    let is_session_file = src
        .file_name()
        .and_then(|n| n.to_str())
        .map(rewrite::is_session_file)
        .unwrap_or(false);
    let rewritten_bytes = if is_session_file {
        rewrite::rewrite_for_copy(&bytes, from_home, to_home)
    } else {
        bytes.clone()
    };
    let needs_rewrite = is_session_file && rewrite::needs_rewrite_for_bytes(&bytes);

    // Byte-identical destination files are left alone and reported as
    // "skipped (exists)" — the migration is idempotent. Session files are
    // compared against their rewritten form, never the raw source bytes.
    if let Ok(existing) = fs::read(dst) {
        if existing == rewritten_bytes {
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

    report.entries.push(PlanEntry {
        category: category_scope.to_string(),
        display: display_path(dst, display_root),
        skipped: false,
        rewritten: needs_rewrite,
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
        fs::write(dst, &rewritten_bytes)
            .with_context(|| format!("could not write {}", dst.display()))?;
        set_mode_0600(dst)?;
    } else {
        fs::write(dst, &rewritten_bytes)
            .with_context(|| format!("could not write {}", dst.display()))?;
    }
    Ok(())
}

/// Copies `projects/<slug>/index.sqlite`. The source database is never
/// opened — not read-only, not for `--dry-run`: opening it would create
/// -wal/-shm sidecars next to the live database. Instead its raw bytes plus
/// any -wal/-shm sidecars are copied into a temp directory, THAT copy is
/// opened with rusqlite and checkpointed with `PRAGMA wal_checkpoint(TRUNCATE)`
/// so every committed row — including ones still living only in the -wal
/// sidecar — lands in the main database file, and the checkpointed temp file
/// is what gets compared against and copied to the destination. No -wal/-shm
/// sidecars are carried to the destination.
fn migrate_sqlite_index(
    src: &Path,
    dst: &Path,
    category_scope: &str,
    display_root: &str,
    options: &MigrateOptions,
    report: &mut MigrationReport,
) -> Result<()> {
    // Work happens entirely on the temp copy: the temp dir is removed when
    // `temp` drops, so no -wal/-shm sidecars are ever left behind — neither
    // next to the source nor in the destination.
    let temp = tempfile::tempdir()
        .context("could not create a temp directory for the index.sqlite copy")?;
    let staged = temp.path().join("index.sqlite");
    if !src.exists() {
        return Err(anyhow::anyhow!("could not copy {}", src.display()));
    }
    checkpointed_index_copy(src, &staged)
        .with_context(|| format!("could not copy {}", src.display()))?;
    let staged_bytes = fs::read(&staged)
        .with_context(|| format!("could not read {}", src.display()))?;

    // Like every other migrated file, the skip decision is byte identity —
    // here between the checkpointed temp copy and the existing destination —
    // never row counts. Decided before the dry-run bail so plans and real
    // runs report the same action.
    let destination_bytes = fs::read(dst).unwrap_or_default();
    let already_current = !destination_bytes.is_empty() && destination_bytes == *staged_bytes;
    let would_rewrite = !already_current;
    report.entries.push(PlanEntry {
        category: category_scope.to_string(),
        display: display_path(dst, display_root),
        skipped: already_current,
        rewritten: false,
        would_rewrite,
    });
    if options.dry_run || already_current {
        return Ok(());
    }

    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).with_context(|| format!("could not create {}", parent.display()))?;
    }
    fs::write(dst, &staged_bytes)
        .with_context(|| format!("could not write {}", dst.display()))?;
    Ok(())
}

/// Copies `src` plus any -wal/-shm sidecars into `staged` (a path inside a
/// temp directory), opens the copy, and runs `PRAGMA wal_checkpoint(TRUNCATE)`
/// so rows still living only in the -wal land in the main database file.
/// Returns false when `src` does not exist; `src` itself is never opened.
fn checkpointed_index_copy(src: &Path, staged: &Path) -> Result<bool> {
    if !src.exists() {
        return Ok(false);
    }
    fs::copy(src, staged).with_context(|| format!("could not copy {}", src.display()))?;
    // Rows committed but still living only in the source -wal ride along:
    // copying the sidecar next to the staged database lets SQLite recover
    // those rows when the copy is opened, so the checkpoint folds them in.
    let src_wal = PathBuf::from(format!("{}-wal", src.display()));
    if src_wal.exists() {
        fs::copy(&src_wal, format!("{}-wal", staged.display()))
            .with_context(|| format!("could not copy {}", src_wal.display()))?;
    }
    // Opening the staged copy may create fresh -wal/-shm sidecars next to it;
    // they live in the temp dir and vanish with it.
    let checkpoint = rusqlite::Connection::open(staged)
        .and_then(|conn| conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);"))
        .context("could not checkpoint the copied index.sqlite");
    let checkpoint = match checkpoint {
        Ok(()) => Ok(()),
        // A staged copy from an earlier failed attempt can be unreadable as
        // a database; drop it and retry once from a clean copy.
        Err(_) => {
            fs::copy(src, staged).with_context(|| format!("could not copy {}", src.display()))?;
            if src_wal.exists() {
                fs::copy(&src_wal, format!("{}-wal", staged.display()))
                    .with_context(|| format!("could not copy {}", src_wal.display()))?;
            }
            rusqlite::Connection::open(staged)
                .and_then(|conn| conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);"))
                .context("could not checkpoint the copied index.sqlite")
        }
    };
    checkpoint?;
    Ok(true)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use tempfile::tempdir;

    /// The canonical lci home shape used across these tests, with a real
    /// sqlite index and session files:
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
        // A real sqlite index with the sessions table (rusqlite, same schema
        // sessions.ts creates), so the checkpoint-on-copy path is exercised.
        let index = home.join("projects/-com-example-app/index.sqlite");
        let conn = rusqlite::Connection::open(&index).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; \
             CREATE TABLE sessions (id TEXT PRIMARY KEY, cwd TEXT, status TEXT); \
             INSERT INTO sessions (id, cwd, status) VALUES ('abc', 'x', 'active');",
        )
        .unwrap();
        drop(conn);
        fs::write(home.join("projects/-com-example-app/memory/MEMORY.md"), b"# mem\n").unwrap();
        let lci_home_str = home.display().to_string();
        let session = home.join("projects/-com-example-app/sessions/abc");
        fs::write(
            session.join("session.json"),
            format!("{{\"home\":\"{lci_home_str}/sessions/abc\"}}\n"),
        )
        .unwrap();
        fs::write(
            session.join("state.json"),
            format!("{{\"resumeCommand\":\"lci --resume abc\",\"dir\":\"{lci_home_str}/projects/-com-example-app\"}}\n"),
        )
        .unwrap();
        fs::write(
            session.join("result.json"),
            format!("{{\"skills\":\"{lci_home_str}/.lci/skills\"}}\n"),
        )
        .unwrap();
        fs::write(
            session.join("transcript.jsonl"),
            format!("{{\"path\":\"{lci_home_str}/projects/-com-example-app/index.sqlite\"}}\n"),
        )
        .unwrap();
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
        // One entry per FILE: config.json, env.vars, skills/tdd.md,
        // marketplaces/registry.json, index.sqlite (its -wal/-shm stay folded
        // into the checkpointed copy), memory/MEMORY.md and the 4 session
        // files — the two rewritten ones still count once, as copies.
        assert_eq!(report.entries.len(), 10);
        assert!(report.entries.iter().all(|e| !e.skipped));
        assert_eq!(report.summary().lines().count(), 4);

        // Destination tree mirrors the source layout.
        assert_eq!(fs::read(drip_home.join("config.json")).unwrap(), b"{\"model\":\"sonnet\"}");
        assert_eq!(fs::read(drip_home.join("skills/tdd.md")).unwrap(), b"# tdd\n");
        assert_eq!(
            fs::read(drip_home.join("projects/-com-example-app/sessions/abc/session.json")).unwrap(),
            format!(r#"{{"home":"{}/sessions/abc"}}"#, drip_home.display())
                .as_bytes()
                .iter()
                .chain(b"\n")
                .copied()
                .collect::<Vec<u8>>()
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
        assert_eq!(second.entries.len(), 10);
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
        assert_eq!(report.entries.len(), 10);
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

        // 10 home entries + 2 project entries (patches.jsonl, async-tools/job.json).
        assert_eq!(report.entries.len(), 12);
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
        assert_eq!(report.entries.len(), 10);
        assert!(!project.join(".drip").exists());
    }

    #[test]
    fn rewrites_lci_paths_and_commands_in_session_files() {
        let root = tempdir().unwrap();
        let home = fake_lci_home(root.path());
        let drip_home = root.path().join("drip-home");

        let report = migrate_from_lci(&options_from(&home, &drip_home)).unwrap();

        let session = drip_home.join("projects/-com-example-app/sessions/abc");
        for name in ["session.json", "state.json", "result.json", "transcript.jsonl"] {
            let text = String::from_utf8(fs::read(session.join(name)).unwrap())
                .unwrap_or_else(|_| panic!("{} should still be valid utf-8", name));
            assert!(
                text.contains("drip-home"),
                "{} should point at the drip home: {}",
                name,
                text
            );
            assert!(
                !text.contains(&home.display().to_string()),
                "{} still references the lci home: {}",
                name,
                text
            );
            assert!(!text.contains(".lci/"), "{} still has a .lci/ segment: {}", name, text);
            assert!(
                !text.contains("lci --resume"),
                "{} still has an lci --resume command: {}",
                name,
                text
            );
        }
        let state = String::from_utf8(fs::read(session.join("state.json")).unwrap()).unwrap();
        assert!(state.contains("drip --resume abc"), "state.json: {}", state);
        let result = String::from_utf8(fs::read(session.join("result.json")).unwrap()).unwrap();
        assert!(result.contains(".drip/skills"), "result.json: {}", result);

        // The source files keep their lci strings — copy, never rewrite in place.
        let source_state = String::from_utf8(
            fs::read(home.join("projects/-com-example-app/sessions/abc/state.json")).unwrap(),
        )
        .unwrap();
        assert!(source_state.contains("lci --resume"));

        // Exactly the four session files were reported as copy+rewrite.
        assert_eq!(report.entries.iter().filter(|e| e.rewritten).count(), 4);
    }

    #[test]
    fn sqlite_index_is_checkpointed_into_the_destination() {
        let root = tempdir().unwrap();
        let home = fake_lci_home(root.path());
        let drip_home = root.path().join("drip-home");
        let db = home.join("projects/-com-example-app/index.sqlite");

        // Keep a connection open across the migration with a row that lives
        // only in the -wal sidecar, so the checkpoint-on-copy is what makes
        // the row survive into the destination.
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
        conn.execute(
            "INSERT INTO sessions (id, cwd, status) VALUES ('def', 'x', 'active')",
            [],
        )
        .unwrap();

        migrate_from_lci(&options_from(&home, &drip_home)).unwrap();
        drop(conn);

        let dest = drip_home.join("projects/-com-example-app/index.sqlite");
        assert!(
            !PathBuf::from(format!("{}-wal", dest.display())).exists(),
            "the -wal sidecar must not be copied to the destination"
        );
        assert!(
            !PathBuf::from(format!("{}-shm", dest.display())).exists(),
            "the -shm sidecar must not be copied to the destination"
        );
        let conn = rusqlite::Connection::open(&dest).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            count, 2,
            "the row living only in -wal must survive the checkpointed copy"
        );
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

    /// The source home (in particular `projects/<slug>/index.sqlite`) must
    /// never be opened — not even read-only, not even for --dry-run — so its
    /// contents and mtimes are byte-for-byte identical after a migration and
    /// after a --dry-run, and no -wal/-shm sidecars appear next to the source
    /// index (opening a sqlite db with rusqlite would create them).
    #[test]
    fn source_directory_is_never_modified_by_migrate_or_dry_run() {
        type Snapshot = Vec<(String, std::time::SystemTime, Vec<u8>)>;

        fn snapshot(dir: &Path) -> Snapshot {
            fn walk(dir: &Path, prefix: &str, out: &mut Snapshot) {
                let mut entries: Vec<_> = fs::read_dir(dir)
                    .unwrap()
                    .map(|e| e.unwrap())
                    .collect();
                entries.sort_by_key(|e| e.file_name());
                for entry in entries {
                    let rel = format!("{prefix}{}", entry.file_name().to_string_lossy());
                    let meta = entry.metadata().unwrap();
                    if meta.is_dir() {
                        out.push((format!("{rel}/"), meta.modified().unwrap(), Vec::new()));
                        walk(&entry.path(), &format!("{rel}/"), out);
                    } else {
                        out.push((rel, meta.modified().unwrap(), fs::read(entry.path()).unwrap()));
                    }
                }
            }
            let mut out = Vec::new();
            walk(dir, "", &mut out);
            out
        }

        let root = tempdir().unwrap();
        let home = fake_lci_home(root.path());
        let before = snapshot(&home);

        let drip_home = root.path().join("drip-home");
        migrate_from_lci(&options_from(&home, &drip_home)).unwrap();
        assert_eq!(
            snapshot(&home),
            before,
            "the source home must be untouched by a real migration"
        );

        let dry_home = root.path().join("drip-dry-home");
        let mut options = options_from(&home, &dry_home);
        options.dry_run = true;
        migrate_from_lci(&options).unwrap();
        assert_eq!(
            snapshot(&home),
            before,
            "the source home must be untouched by --dry-run"
        );

        let index = home.join("projects/-com-example-app/index.sqlite");
        assert!(
            !PathBuf::from(format!("{}-wal", index.display())).exists(),
            "no -wal sidecar may be created next to the source index"
        );
        assert!(
            !PathBuf::from(format!("{}-shm", index.display())).exists(),
            "no -shm sidecar may be created next to the source index"
        );
    }
}
