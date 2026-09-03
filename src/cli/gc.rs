// `now` is an explicit `&dyn Fn() -> DateTime<Utc>` parameter so callers —
// tests especially — control the clock, and the sqlite handles are the
// core::sessions types.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::core::home::DripProject;
use crate::core::lease::check_lease;
use crate::core::sessions::{open_session_index, SessionIndex};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// GcSessionEntry — recursive size of one eligible session directory.
#[derive(Debug, Clone, PartialEq)]
pub struct GcSessionEntry {
    pub bytes: u64,
    pub dir: String,
    pub id: String,
}

/// GcPlan — every eligible session plus the combined byte count.
#[derive(Debug, Clone, PartialEq)]
pub struct GcPlan {
    pub sessions: Vec<GcSessionEntry>,
    pub total_bytes: u64,
}

/// GcResult — sessions compacted and bytes deleted by one pass.
#[derive(Debug, Clone, PartialEq)]
pub struct GcResult {
    pub compacted_sessions: u64,
    pub deleted_bytes: u64,
}

/// SweepResult — return shape of sweep_async_job_logs.
#[derive(Debug, Clone, PartialEq)]
pub struct SweepResult {
    pub deleted_bytes: u64,
    pub deleted_files: u64,
}

/// ReapResult — return shape of reap_orphan_tmux_sessions.
#[derive(Debug, Clone, PartialEq)]
pub struct ReapResult {
    pub killed: Vec<String>,
    pub kept: Vec<String>,
}

/// GcOptions — CLI-facing knobs.
#[derive(Debug, Clone)]
pub struct GcOptions {
    pub older_than_ms: i64,
    pub dry_run: bool,
    pub json: bool,
}

impl Default for GcOptions {
    fn default() -> Self {
        GcOptions {
            older_than_ms: 14 * 24 * 60 * 60 * 1000,
            dry_run: false,
            json: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Recursively sums the byte sizes of every file in a tree.
pub fn dir_bytes(dir: &str) -> u64 {
    let mut total = 0u64;

    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };

    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };

        if meta.is_dir() {
            let full = entry.path().to_string_lossy().into_owned();
            total += dir_bytes(&full);
        } else {
            total += meta.len();
        }
    }

    total
}

/// Splits on newline, keeps the last `n` non-empty lines, rejoins with a
/// trailing newline.
pub fn last_n_lines(text: &str, n: usize) -> String {
    let kept: Vec<&str> = text.split('\n').filter(|l| !l.is_empty()).collect();
    let start = kept.len().saturating_sub(n);
    let kept = &kept[start..];

    if kept.is_empty() {
        String::new()
    } else {
        format!("{}\n", kept.join("\n"))
    }
}

// ---------------------------------------------------------------------------
// collectGcPlan
// ---------------------------------------------------------------------------

/// collect_gc_plan — inspect the session index and return the sessions
/// eligible for GC. Cheap: no mutations, only size accounting.
///
/// Gates:
/// - status != "active" (the SQL WHERE clause)
/// - no live lease (check_lease alive → skip)
/// - updatedAt older than now - olderThanMs
/// - updatedAt older than now - olderThanMs
pub fn collect_gc_plan(
    index: &SessionIndex,
    project: &DripProject,
    older_than_ms: i64,
    now: &dyn Fn() -> DateTime<Utc>,
) -> GcPlan {
    let cutoff = now().timestamp_millis().saturating_sub(older_than_ms);

    // Every registry: the project's own index plus the legacy pre-move index
    // when it still exists on disk.
    let mut registries: Vec<(&rusqlite::Connection, &str)> = vec![(&index.conn, &project.sessions_dir)];

    // The legacy pre-move registry gets its own handle on its own db file.
    let legacy_index = match (
        project.legacy_index_db_path.as_deref(),
        project.legacy_sessions_dir.as_deref(),
    ) {
        (Some(legacy_db), Some(legacy_sessions_dir)) if Path::new(legacy_db).exists() => {
            Some((open_session_index(legacy_db), legacy_sessions_dir))
        }
        _ => None,
    };

    if let Some((opened, legacy_sessions_dir)) = &legacy_index {
        registries.push((&opened.conn, legacy_sessions_dir));
    }

    // Fetch every non-active session from each index. Active sessions are
    // always excluded: the running process may not yet hold a lease at the
    // instant we check (e.g. between index write and lease write at startup).
    let mut rows: Vec<(String, String, String)> = Vec::new();

    for (conn, sessions_dir) in &registries {
        let mut stmt = match conn.prepare("SELECT id, updated_at FROM sessions WHERE status != 'active'") {
            Ok(stmt) => stmt,
            Err(_) => continue,
        };

        let fetched = stmt
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
            .expect("collect_gc_plan query");

        for row in fetched.flatten() {
            rows.push((row.0, row.1, (*sessions_dir).to_string()));
        }
    }

    // Close the legacy index once its rows are fetched.
    drop(legacy_index);

    let mut sessions: Vec<GcSessionEntry> = Vec::new();

    for (id, updated_at, sessions_dir) in rows {

            // Age gate — sessions updated recently are skipped.
            let Ok(updated) = DateTime::parse_from_rfc3339(&updated_at) else {
                continue;
            };

            if updated.timestamp_millis() >= cutoff {
                continue;
            }

            let dir = PathBuf::from(sessions_dir).join(&id);

            // Liveness gate — a live lease means the session is running;
            // never touch it.
            let lease_path = dir.join("lease.json");
            if check_lease(Path::new(&lease_path), now).alive() {
                continue;
            }
            let bytes = dir_bytes(dir.to_str().unwrap_or(""));

            sessions.push(GcSessionEntry {
                bytes,
                dir: dir.to_string_lossy().into_owned(),
                id,
            });
        }

    let total_bytes = sessions.iter().map(|s| s.bytes).sum();

    GcPlan { sessions, total_bytes }
}

// ---------------------------------------------------------------------------
// executeGcPlan
// ---------------------------------------------------------------------------

const TRANSCRIPT_KEEP_LINES: usize = 200;

/// Delegates to last_n_lines.
fn last_n_lines_ts(text: &str, n: usize) -> String {
    last_n_lines(text, n)
}

/// Deletes images/ and *.log per session, and truncates transcript.jsonl to
/// its last 200 lines. Dry run touches nothing.
pub fn execute_gc_plan(plan: &GcPlan, dry_run: bool) -> GcResult {
    let mut deleted_bytes: u64 = 0;
    let mut compacted_sessions: u64 = 0;

    for session in &plan.sessions {
        let mut session_deleted: u64 = 0;

        // --- images/ directory ---------------------------------------------------
        let images_dir = PathBuf::from(&session.dir).join("images");

        if images_dir.exists() {
            session_deleted += dir_bytes(images_dir.to_str().unwrap_or(""));

            if !dry_run {
                let _ = fs::remove_dir_all(&images_dir);
            }
        }

        // --- *.log files ---------------------------------------------------------
        if let Ok(entries) = fs::read_dir(&session.dir) {
            for entry in entries.flatten() {
                let Ok(meta) = entry.metadata() else { continue };

                if !meta.is_file() {
                    continue;
                }

                let name = entry.file_name();
                let name = name.to_string_lossy();

                if !name.ends_with(".log") {
                    continue;
                }

                session_deleted += meta.len();

                if !dry_run {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }

        // --- transcript.jsonl truncation -----------------------------------------
        let transcript_path = PathBuf::from(&session.dir).join("transcript.jsonl");

        if transcript_path.exists() {
            if let Ok(original) = fs::read_to_string(&transcript_path) {
                let truncated = last_n_lines_ts(&original, TRANSCRIPT_KEEP_LINES);

                // Only count as freed bytes if we actually shorten the file.
                if truncated.len() < original.len() {
                    session_deleted += (original.len() - truncated.len()) as u64;

                    if !dry_run {
                        let _ = fs::write(&transcript_path, truncated);
                    }
                }
            }
        }

        deleted_bytes += session_deleted;
        compacted_sessions += 1;
    }

    GcResult {
        compacted_sessions,
        deleted_bytes,
    }
}

// ---------------------------------------------------------------------------
// sweepAsyncJobLogs
// ---------------------------------------------------------------------------

/// Deletes settled async-job logs under <root>/async-tools that are older
/// than the cutoff.
pub fn sweep_async_job_logs(
    project: &DripProject,
    older_than_ms: i64,
    dry_run: bool,
    now: &dyn Fn() -> DateTime<Utc>,
) -> SweepResult {
    // The async-tools dir hangs off the project root; there is no dedicated
    // DripProject field for it.
    let dir = PathBuf::from(&project.root).join("async-tools");
    let now_ms = now().timestamp_millis();
    let mut deleted_bytes: u64 = 0;
    let mut deleted_files: u64 = 0;

    if !dir.exists() {
        return SweepResult { deleted_bytes, deleted_files };
    }

    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };

            if !meta.is_file() {
                continue;
            }

            let mtime_ms = meta
                .modified()
                .ok()
                .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64);

            let Some(mtime_ms) = mtime_ms else { continue };

            if now_ms - mtime_ms <= older_than_ms {
                continue;
            }

            deleted_bytes += meta.len();
            deleted_files += 1;

            if !dry_run {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    SweepResult { deleted_bytes, deleted_files }
}

// ---------------------------------------------------------------------------
// reapOrphanTmuxSessions
// ---------------------------------------------------------------------------

/// Kills drip-prefixed tmux sessions older than the cutoff. The caller
/// supplies the listing and kill callbacks so the logic is testable without
/// a tmux server.
pub fn reap_orphan_tmux_sessions(
    listing: &str,
    older_than_ms: i64,
    dry_run: bool,
    now_ms: i64,
    kill_session: &dyn Fn(&str) -> Result<(), String>,
) -> ReapResult {
    let prefix = crate::tools::child_process::TMUX_PREFIX;
    let mut killed: Vec<String> = Vec::new();
    let mut kept: Vec<String> = Vec::new();

    for line in listing.split('\n') {
        let line = line.trim();

        if line.is_empty() || !line.starts_with(prefix) {
            continue;
        }

        let name = line.split_whitespace().next().unwrap_or("").to_string();

        if name.is_empty() {
            continue;
        }

        // Parse the created timestamp from "#{session_name} #{session_created}".
        let created_raw = line[name.len()..].trim();
        let Ok(created_ms) = created_raw.parse::<i64>() else {
            kept.push(name);
            continue;
        };

        let _ = created_ms;
        let age_ok = now_ms - created_ms * 1000 > older_than_ms;

        if !age_ok {
            kept.push(name);
            continue;
        }

        // Dry run never invokes tmux kill-session.
        if dry_run {
            killed.push(name);
            continue;
        }

        match kill_session(&name) {
            Ok(()) => killed.push(name),
            Err(_) => kept.push(name),
        }
    }

    ReapResult { killed, kept }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_n_lines_keeps_trailing_newline() {
        assert_eq!(last_n_lines("a\nb\nc\n", 2), "b\nc\n");
        assert_eq!(last_n_lines("a\nb\nc\n", 5), "a\nb\nc\n");
        assert_eq!(last_n_lines("", 3), "");
    }
}
