use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension};
use uuid::Uuid;

// Re-exported shared memory-note shape; backfill.rs tests import it via
// the sessions module.
pub use crate::core::types::HarnessMemoryNote;

// ---------------------------------------------------------------------------
// ProjectPaths — small struct holding just the project paths sessions.rs needs.
// A full home.rs port is another lane; swap to core::home::ProjectPaths when
// it lands.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ProjectPaths {
    /// Root of the drip home tree (e.g. ~/.drip).
    pub home_root: String,
    /// Per-project memory directory.
    pub memory_dir: String,
    /// Root of the git repo (main checkout; equals worktree_root for the main).
    pub repo_root: String,
    /// Per-project .drip directory inside the worktree.
    pub root: String,
    /// ~/.drip/projects/<slug>/sessions — where session dirs live.
    pub sessions_dir: String,
    /// Project slug (derived from worktree root path).
    pub slug: String,
    /// Root of the checked-out worktree (may differ from repo_root).
    pub worktree_root: String,
}

impl From<&crate::core::home::DripProject> for ProjectPaths {
    // The home.rs DripProject is the full project record; ProjectPaths is the
    // subset the session store reads. Optional roots fall back to the slug
    // holder's `root` so a --project-dir override still keys consistently.
    fn from(project: &crate::core::home::DripProject) -> Self {
        ProjectPaths {
            home_root: project.home_root.clone(),
            memory_dir: project.memory_dir.clone(),
            repo_root: project.repo_root.clone().unwrap_or_else(|| project.root.clone()),
            root: project.root.clone(),
            sessions_dir: project.sessions_dir.clone(),
            slug: project.slug.clone(),
            worktree_root: project.worktree_root.clone().unwrap_or_else(|| project.root.clone()),
        }
    }
}

// ---------------------------------------------------------------------------
// SessionStatus / SessionRecord
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum SessionStatus {
    Active,
    Completed,
    Idle,
}

impl SessionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionStatus::Active => "active",
            SessionStatus::Completed => "completed",
            SessionStatus::Idle => "idle",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionRecord {
    /// Set only for a pre-move record living under <repo>/.drip/sessions.
    pub sessions_dir: Option<String>,
    pub created_at: String,
    pub cwd: String,
    pub goal_count: i64,
    pub id: String,
    pub last_goal: Option<String>,
    pub project_slug: String,
    pub status: String,
    pub updated_at: String,
}

// ---------------------------------------------------------------------------
// SessionIndex — wraps an open rusqlite Connection
// ---------------------------------------------------------------------------

pub struct SessionIndex {
    pub conn: Connection,
}

impl SessionIndex {
    pub fn close(self) {
        // The connection closes on Drop; this is an explicit close for clarity.
        drop(self.conn);
    }
}

// ---------------------------------------------------------------------------
// open_session_index
// ---------------------------------------------------------------------------

fn persist_wal_files(conn: &Connection) {
    let mut flag: std::os::raw::c_int = 1;
    // SAFETY: sqlite3_file_control on the connection's main database with the
    // documented SQLITE_FCNTL_PERSIST_WAL int argument.
    unsafe {
        rusqlite::ffi::sqlite3_file_control(
            conn.handle(),
            c"main".as_ptr(),
            rusqlite::ffi::SQLITE_FCNTL_PERSIST_WAL,
            (&mut flag as *mut std::os::raw::c_int).cast(),
        );
    }
}

pub fn open_session_index(db_path: &str) -> SessionIndex {
    // Create parent directories if needed.
    if let Some(parent) = Path::new(db_path).parent() {
        fs::create_dir_all(parent).expect("create index parent dirs");
    }

    let conn = Connection::open(db_path).expect("open sqlite db");

    conn.execute_batch("PRAGMA journal_mode = WAL;").ok();
    // Bun's sqlite leaves the -wal/-shm side files in place when the last
    // connection closes; SQLite's default is to delete them, and a WAL
    // database without its -shm cannot be opened read-only (dripw, the parity
    // harness, and anything else that peeks at the index without write
    // intent). Persisting the WAL files keeps the on-disk layout stable
    // across versions.
    persist_wal_files(&conn);
    // The CLI and the web server share one index; WAL permits cross-process
    // access but a busy timeout is what prevents spurious SQLITE_BUSY throws.
    conn.execute_batch("PRAGMA busy_timeout = 5000;").ok();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS sessions (
            id TEXT PRIMARY KEY,
            project_slug TEXT NOT NULL,
            cwd TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            status TEXT NOT NULL DEFAULT 'active',
            goal_count INTEGER NOT NULL DEFAULT 0,
            last_goal TEXT
        );
        CREATE INDEX IF NOT EXISTS sessions_cwd_updated ON sessions (cwd, updated_at);
        CREATE TABLE IF NOT EXISTS session_memories (
            session_id TEXT NOT NULL,
            note_id TEXT NOT NULL,
            text TEXT NOT NULL,
            created_at_iteration INTEGER NOT NULL,
            updated_at TEXT NOT NULL,
            PRIMARY KEY (session_id, note_id)
        );",
    )
    .expect("create tables");

    SessionIndex { conn }
}

// ---------------------------------------------------------------------------
// row_to_record helper
// ---------------------------------------------------------------------------

fn row_to_record(
    id: String,
    project_slug: String,
    cwd: String,
    created_at: String,
    updated_at: String,
    status: String,
    goal_count: i64,
    last_goal: Option<String>,
) -> SessionRecord {
    let status = if status == "completed" || status == "idle" {
        status
    } else {
        "active".to_string()
    };
    SessionRecord {
        sessions_dir: None,
        created_at,
        cwd,
        goal_count,
        id,
        last_goal,
        project_slug,
        status,
        updated_at,
    }
}

// ---------------------------------------------------------------------------
// CreateSessionArgs takes `now` as a pre-formatted ISO string (backfill
// tests pass literal strings; callers that want wall-clock time pass
// chrono's to_rfc3339()).
// ---------------------------------------------------------------------------

pub struct CreateSessionArgs<'a> {
    pub cwd: String,
    pub project: &'a ProjectPaths,
    /// Pre-formatted ISO 8601 timestamp. Pass "" to use the current time.
    pub now: &'a str,
}

pub fn create_session(index: &SessionIndex, args: CreateSessionArgs) -> SessionRecord {
    use chrono::Utc;

    let now = if args.now.is_empty() {
        Utc::now()
            .format("%Y-%m-%dT%H:%M:%S%.3fZ")
            .to_string()
    } else {
        args.now.to_string()
    };

    let id = Uuid::new_v4().to_string();
    // An overridden/stubbed project without a slug falls back to
    // project_slug(args.cwd) so the row is still keyed consistently.
    let project_slug = if args.project.slug.is_empty() {
        crate::core::home::project_slug(&args.cwd)
    } else {
        args.project.slug.clone()
    };

    // Lay out session directory and images/ subdirectory.
    let session_dir = PathBuf::from(&args.project.sessions_dir).join(&id);
    let images_dir = session_dir.join("images");
    fs::create_dir_all(&images_dir).expect("create session dirs");

    // Write session.json (provenance: id, cwd, projectSlug, createdAt).
    let meta = serde_json::json!({
        "createdAt": now,
        "cwd": args.cwd,
        "id": id,
        "projectSlug": project_slug,
    });
    fs::write(
        session_dir.join("session.json"),
        format!("{}\n", serde_json::to_string_pretty(&meta).unwrap()),
    )
    .expect("write session.json");

    // Insert row.
    index
        .conn
        .execute(
            "INSERT INTO sessions (id, project_slug, cwd, created_at, updated_at, status, goal_count, last_goal)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![id, project_slug, args.cwd, now, now, "active", 0i64, Option::<String>::None],
        )
        .expect("insert session row");

    SessionRecord {
        sessions_dir: None,
        created_at: now.clone(),
        cwd: args.cwd,
        goal_count: 0,
        id,
        last_goal: None,
        project_slug,
        status: "active".to_string(),
        updated_at: now,
    }
}

// ---------------------------------------------------------------------------
// get_session
// ---------------------------------------------------------------------------

pub fn get_session(index: &SessionIndex, session_id: &str) -> Option<SessionRecord> {
    index
        .conn
        .query_row(
            "SELECT id, project_slug, cwd, created_at, updated_at, status, goal_count, last_goal FROM sessions WHERE id = ?1",
            rusqlite::params![session_id],
            |row| {
                Ok(row_to_record(
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ))
            },
        )
        .optional()
        .expect("get_session query")
}

// ---------------------------------------------------------------------------
// find_session_by_id_prefix
// ---------------------------------------------------------------------------

pub fn find_session_by_id_prefix(index: &SessionIndex, id_prefix: &str) -> Option<SessionRecord> {
    let pattern = format!("{}%", id_prefix);
    let mut stmt = index
        .conn
        .prepare(
            "SELECT id, project_slug, cwd, created_at, updated_at, status, goal_count, last_goal
             FROM sessions WHERE id LIKE ?1 ORDER BY updated_at DESC LIMIT 2",
        )
        .expect("prepare find_by_prefix");

    let rows: Vec<SessionRecord> = stmt
        .query_map(rusqlite::params![pattern], |row| {
            Ok(row_to_record(
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
            ))
        })
        .expect("query find_by_prefix")
        .map(|r| r.expect("row"))
        .collect();

    if rows.len() == 1 {
        Some(rows.into_iter().next().unwrap())
    } else if rows.len() > 1 && rows[0].id == id_prefix {
        Some(rows.into_iter().next().unwrap())
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// list_sessions
// ---------------------------------------------------------------------------

pub fn list_sessions(index: &SessionIndex, limit: Option<i64>) -> Vec<SessionRecord> {
    let mut stmt = match limit {
        // The LIMIT value is bound below via params_from_iter(limit); the arm
        // only selects the prepared statement shape.
        Some(_cap) => index
            .conn
            .prepare(
                "SELECT id, project_slug, cwd, created_at, updated_at, status, goal_count, last_goal
                 FROM sessions ORDER BY updated_at DESC LIMIT ?1",
            )
            .expect("prepare list_sessions"),
        None => index
            .conn
            .prepare(
                "SELECT id, project_slug, cwd, created_at, updated_at, status, goal_count, last_goal
                 FROM sessions ORDER BY updated_at DESC",
            )
            .expect("prepare list_sessions"),
    };

    let rows = stmt
        .query_map(rusqlite::params_from_iter(limit), |row| {
            Ok(row_to_record(
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
            ))
        })
        .expect("list_sessions query")
        .map(|r| r.expect("row"))
        .collect();
    rows
}

pub fn latest_session(index: &SessionIndex) -> Option<SessionRecord> {
    list_sessions(index, Some(1)).into_iter().next()
}

// ---------------------------------------------------------------------------
// touch_session — upsert updated_at/status/goal_count/last_goal
// ---------------------------------------------------------------------------

pub fn touch_session(index: &SessionIndex, session_id: &str, last_goal: Option<&str>, status: Option<&str>) {
    touch_session_at(index, session_id, last_goal, status, None);
}

/// Timestamps are injected so tests can pin them; `None` uses the current
/// time.
pub fn touch_session_at(
    index: &SessionIndex,
    session_id: &str,
    last_goal: Option<&str>,
    status: Option<&str>,
    now: Option<&str>,
) {
    use chrono::Utc;
    let now = match now {
        Some(value) if !value.is_empty() => value.to_string(),
        _ => Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
    };

    if let Some(goal) = last_goal {
        // with lastGoal → UPDATE updated_at, last_goal, goal_count+1, status=patch.status ?? "active"
        let new_status = status.unwrap_or("active");
        index
            .conn
            .execute(
                "UPDATE sessions SET updated_at = ?1, last_goal = ?2, goal_count = goal_count + 1, status = ?3 WHERE id = ?4",
                rusqlite::params![now, goal, new_status, session_id],
            )
            .expect("touch_session with goal");
    } else {
        // else UPDATE updated_at, status=COALESCE(?,status)
        let new_status = status.unwrap_or("");
        if new_status.is_empty() {
            index
                .conn
                .execute(
                    "UPDATE sessions SET updated_at = ?1 WHERE id = ?2",
                    rusqlite::params![now, session_id],
                )
                .expect("touch_session no status");
        } else {
            index
                .conn
                .execute(
                    "UPDATE sessions SET updated_at = ?1, status = COALESCE(?2, status) WHERE id = ?3",
                    rusqlite::params![now, new_status, session_id],
                )
                .expect("touch_session with status");
        }
    }
}

// ---------------------------------------------------------------------------
// sync_session_memories — DELETE all + INSERT per note in a transaction
// ---------------------------------------------------------------------------

pub fn sync_session_memories(index: &SessionIndex, session_id: &str, notes: &[HarnessMemoryNote]) {
    use chrono::Utc;
    let now = Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();

    // DELETE + INSERTs run in one transaction: a failed insert must not
    // leave the session's memory mirror truncated.
    let tx = index
        .conn
        .unchecked_transaction()
        .expect("begin session memories transaction");
    tx.execute("DELETE FROM session_memories WHERE session_id = ?1", rusqlite::params![session_id])
        .expect("delete session memories");

    for note in notes {
        tx.execute(
            "INSERT OR REPLACE INTO session_memories (session_id, note_id, text, created_at_iteration, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![session_id, note.id, note.text, note.created_at_iteration, now],
        )
        .expect("insert session memory");
    }
    tx.commit().expect("commit session memories");
}

// ---------------------------------------------------------------------------
// list_recent_project_memories — deduplicated memories from OTHER sessions
// ---------------------------------------------------------------------------

pub fn list_recent_project_memories(
    index: &SessionIndex,
    exclude_session_id: &str,
    limit: Option<usize>,
) -> Vec<String> {
    let limit = limit.unwrap_or(10);
    let mut stmt = index
        .conn
        .prepare(
            "SELECT text FROM session_memories WHERE session_id != ?1 ORDER BY updated_at DESC, created_at_iteration DESC LIMIT ?2",
        )
        .expect("prepare list_recent_project_memories");

    let rows: Vec<String> = stmt
        .query_map(rusqlite::params![exclude_session_id, (limit * 3) as i64], |row| row.get(0))
        .expect("query list_recent_project_memories")
        .map(|r| r.expect("row"))
        .collect();

    let mut seen = std::collections::HashSet::new();
    let mut notes = Vec::new();

    for text in rows {
        let normalized = text.trim().to_string();
        if normalized.is_empty() || seen.contains(&normalized) {
            continue;
        }
        seen.insert(normalized.clone());
        notes.push(normalized);
        if notes.len() >= limit {
            break;
        }
    }

    notes
}

// ---------------------------------------------------------------------------
// list_session_memories
// ---------------------------------------------------------------------------

pub fn list_session_memories(index: &SessionIndex, session_id: &str) -> Vec<HarnessMemoryNote> {
    let mut stmt = index
        .conn
        .prepare(
            "SELECT note_id, text, created_at_iteration FROM session_memories WHERE session_id = ?1 ORDER BY created_at_iteration",
        )
        .expect("prepare list_session_memories");

    stmt.query_map(rusqlite::params![session_id], |row| {
        Ok(HarnessMemoryNote {
            id: row.get(0)?,
            text: row.get(1)?,
            created_at_iteration: row.get(2)?,
        })
    })
    .expect("query list_session_memories")
    .map(|r| r.expect("row"))
    .collect()
}

// ---------------------------------------------------------------------------
// session_paths helper (partial — used by backfill)
// ---------------------------------------------------------------------------

pub struct SessionPaths {
    pub activation_path: String,
    pub dir: String,
    pub images_dir: String,
    pub inbox_path: String,
    pub lease_path: String,
    pub meta_path: String,
    pub queue_path: String,
    pub result_path: String,
    pub state_path: String,
    pub transcript_path: String,
}

/// A pre-move record's own sessionsDir wins over the project's.
pub fn session_paths_for(project: &crate::core::home::DripProject, record: &SessionRecord) -> SessionPaths {
    session_paths(&ProjectPaths::from(project), &record.id, record.sessions_dir.as_deref())
}

pub fn session_paths(project: &ProjectPaths, session_id: &str, sessions_dir_override: Option<&str>) -> SessionPaths {
    let base = sessions_dir_override.unwrap_or(&project.sessions_dir);
    let dir = PathBuf::from(base).join(session_id);
    let dir_str = dir.to_string_lossy().into_owned();

    SessionPaths {
        activation_path: dir.join("activation.json").to_string_lossy().into_owned(),
        images_dir: dir.join("images").to_string_lossy().into_owned(),
        inbox_path: dir.join("inbox.jsonl").to_string_lossy().into_owned(),
        lease_path: dir.join("lease.json").to_string_lossy().into_owned(),
        meta_path: dir.join("session.json").to_string_lossy().into_owned(),
        queue_path: dir.join("queue.jsonl").to_string_lossy().into_owned(),
        result_path: dir.join("result.json").to_string_lossy().into_owned(),
        state_path: dir.join("state.json").to_string_lossy().into_owned(),
        transcript_path: dir.join("transcript.jsonl").to_string_lossy().into_owned(),
        dir: dir_str,
    }
}

// ---------------------------------------------------------------------------
// Multi-registry helpers (enumerate_project_registries, open_project_indexes,
// open_home_registries, stamp, list_all_sessions, list_all_home_sessions,
// latest_any_session, resolve_any_session_ref, has_any_session_index)
// ---------------------------------------------------------------------------

use crate::core::home::DripProject;

/// One project registry under <drip home>/projects/: the index.sqlite path and
/// the sessions tree that project's records resolve against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRegistry {
    pub index_db_path: String,
    pub sessions_dir: String,
}

/// Every project registry under `<drip home>/projects/`, sorted by path.
///
/// Pure enumeration from an explicit home path: no git or environment lookups,
/// and nothing is created. Entries that are not directories or that carry no
/// index.sqlite are skipped; an absent projects directory yields an empty list.
pub fn enumerate_project_registries(drip_home: &Path) -> Vec<ProjectRegistry> {
    let Ok(entries) = fs::read_dir(drip_home.join("projects")) else {
        return Vec::new();
    };

    let mut registries: Vec<ProjectRegistry> = Vec::new();

    for entry in entries.flatten() {
        let project_dir = entry.path();

        if !project_dir.is_dir() {
            continue;
        }

        let index_db_path = project_dir.join("index.sqlite");

        if !index_db_path.is_file() {
            continue;
        }

        registries.push(ProjectRegistry {
            index_db_path: index_db_path.to_string_lossy().into_owned(),
            sessions_dir: project_dir.join("sessions").to_string_lossy().into_owned(),
        });
    }

    registries.sort_by(|a, b| a.index_db_path.cmp(&b.index_db_path));
    registries
}

pub struct OpenedProjectIndex {
    pub index: SessionIndex,
    pub sessions_dir: String,
}

/// Every registry to read for this project, newest tree first: the project's
/// own index followed by its legacy pre-move index, each entry stamped with its
/// own sessions_dir so session_paths resolves the record into the tree the
/// session actually lives in.
pub fn open_project_indexes(project: &DripProject) -> Vec<OpenedProjectIndex> {
    let mut entries: Vec<OpenedProjectIndex> = Vec::new();

    if Path::new(&project.index_db_path).exists() {
        entries.push(OpenedProjectIndex {
            index: open_session_index(&project.index_db_path),
            sessions_dir: project.sessions_dir.clone(),
        });
    }

    if let (Some(legacy_index), Some(legacy_sessions_dir)) =
        (project.legacy_index_db_path.as_deref(), project.legacy_sessions_dir.as_deref())
    {
        entries.push(OpenedProjectIndex {
            index: open_session_index(legacy_index),
            sessions_dir: legacy_sessions_dir.to_string(),
        });
    }

    entries
}

/// Open every registry under the drip home, skipping any whose index fails to
/// open, so one malformed registry never hides the valid ones.
pub fn open_home_registries(drip_home: &Path) -> Vec<OpenedProjectIndex> {
    enumerate_project_registries(drip_home)
        .into_iter()
        .filter_map(|registry| {
            let index = std::panic::catch_unwind(|| open_session_index(&registry.index_db_path)).ok()?;

            Some(OpenedProjectIndex {
                index,
                sessions_dir: registry.sessions_dir,
            })
        })
        .collect()
}

fn stamp(mut record: SessionRecord, sessions_dir: &str, project: &DripProject) -> SessionRecord {
    // Only a legacy record needs the marker; leaving it off for the project's own
    // tree keeps records comparable to what the single-index path produces.
    if sessions_dir != project.sessions_dir {
        record.sessions_dir = Some(sessions_dir.to_string());
    }

    record
}

/// Union of this project's registries, newest first — for --list and
/// --continue. Reads stay local to the project's own home.
pub fn list_all_sessions(project: &DripProject, limit: Option<i64>) -> Vec<SessionRecord> {
    let limit = limit.unwrap_or(50);
    let opened = open_project_indexes(project);
    let mut merged: Vec<SessionRecord> = opened
        .iter()
        .flat_map(|entry| {
            list_sessions(&entry.index, Some(limit))
                .into_iter()
                .map(|record| stamp(record, &entry.sessions_dir, project))
                .collect::<Vec<_>>()
        })
        .collect();

    // b.updatedAt.localeCompare(a.updatedAt): ISO stamps compare as plain
    // strings; a stable sort keeps registry order for equal stamps.
    merged.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    merged.truncate(limit.max(0) as usize);

    for entry in opened {
        entry.index.close();
    }

    merged
}

pub fn latest_any_session(project: &DripProject) -> Option<SessionRecord> {
    list_all_sessions(project, Some(1)).into_iter().next()
}

/// Resolves a session ref across every registry.
///
/// Ambiguity is decided over the UNION of candidates, not per registry: probing
/// the new tree first and falling back would report a unique match for a prefix
/// that is actually ambiguous once the old tree is considered.
pub fn resolve_any_session_ref(project: &DripProject, reference: Option<&str>) -> Option<SessionRecord> {
    let Some(reference) = reference else {
        return latest_any_session(project);
    };

    let opened = open_project_indexes(project);

    for entry in &opened {
        if let Some(exact) = get_session(&entry.index, reference) {
            return Some(stamp(exact, &entry.sessions_dir, project));
        }
    }

    let pattern = format!("{reference}%");
    let mut prefixed: Vec<SessionRecord> = Vec::new();

    for entry in &opened {
        let mut stmt = entry
            .index
            .conn
            .prepare(
                "SELECT id, project_slug, cwd, created_at, updated_at, status, goal_count, last_goal
                 FROM sessions WHERE id LIKE ?1 ORDER BY updated_at DESC LIMIT 2",
            )
            .expect("prepare resolve_any_session_ref");
        let rows: Vec<SessionRecord> = stmt
            .query_map(rusqlite::params![pattern], |row| {
                Ok(row_to_record(
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ))
            })
            .expect("query resolve_any_session_ref")
            .map(|row| row.expect("row"))
            .collect();

        prefixed.extend(rows.into_iter().map(|record| stamp(record, &entry.sessions_dir, project)));
    }

    let result = if prefixed.len() == 1 { prefixed.into_iter().next() } else { None };

    for entry in opened {
        entry.index.close();
    }

    result
}

/// True when neither registry exists — i.e. nothing was ever recorded here.
pub fn has_any_session_index(project: &DripProject) -> bool {
    Path::new(&project.index_db_path).exists()
        || project
            .legacy_index_db_path
            .as_deref()
            .map(|path| Path::new(path).exists())
            .unwrap_or(false)
}

/// Union of every project registry under the drip home, newest first, each
/// record stamped with its originating sessions_dir (these records come from
/// other projects' trees, so the override is always needed to resolve leases
/// and transcripts). Duplicates by id keep the newest updated_at. No limit is
/// applied here: callers filter by cwd before truncating.
pub fn list_all_home_sessions(drip_home: &Path) -> Vec<SessionRecord> {
    let opened = open_home_registries(drip_home);
    let mut merged: Vec<SessionRecord> = opened
        .iter()
        .flat_map(|entry| {
            // A registry can open fine but panic while extracting rows (e.g. a
            // schema drift); skip that registry like open_home_registries skips
            // one that fails to open, so it never blanks the valid ones.
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| list_sessions(&entry.index, None)))
                .unwrap_or_default()
                .into_iter()
                .map(|record| SessionRecord {
                    sessions_dir: Some(entry.sessions_dir.clone()),
                    ..record
                })
        })
        .collect();

    // b.updatedAt.localeCompare(a.updatedAt): ISO stamps compare as plain
    // strings; a stable sort keeps registry order for equal stamps.
    merged.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));

    // The same project can be reachable under two registry dirs (slug renames);
    // dedupe by id, keeping the newest occurrence.
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    merged.retain(|record| seen.insert(record.id.clone()));

    for entry in opened {
        entry.index.close();
    }

    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;

    fn temp_home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "drip-sessions-test-{}-{}",
            tag,
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&dir).expect("create temp home");
        dir
    }

    /// Writes a minimal index with one session recorded at `cwd`.
    fn seed_project(home: &Path, slug: &str, id: &str, cwd: &str, updated_at: &str) {
        let project_dir = home.join("projects").join(slug);
        fs::create_dir_all(&project_dir).expect("create project dir");
        let index = open_session_index(&project_dir.join("index.sqlite").to_string_lossy());
        index
            .conn
            .execute(
                "INSERT INTO sessions (id, project_slug, cwd, created_at, updated_at, status, goal_count, last_goal)
                 VALUES (?1, ?1, ?2, ?3, ?3, 'active', 0, NULL)",
                rusqlite::params![id, cwd, updated_at],
            )
            .expect("insert session");
        index.close();
    }

    fn cleanup(home: &Path) {
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn enumeration_lists_two_projects_with_origin_correct_records() {
        let home = temp_home("two-projects");
        seed_project(&home, "alpha", "aaaa", "/repo/a", "2026-01-01T00:00:00Z");
        seed_project(&home, "beta", "bbbb", "/repo/b", "2026-01-02T00:00:00Z");

        let records = list_all_home_sessions(&home);
        assert_eq!(records.len(), 2);
        // Newest first.
        assert_eq!(records[0].id, "bbbb");
        assert_eq!(records[1].id, "aaaa");
        // Each record resolves against the sessions tree it was written to.
        assert_eq!(
            records[0].sessions_dir.as_deref(),
            Some(home.join("projects").join("beta").join("sessions").to_string_lossy().as_ref())
        );
        assert_eq!(
            records[1].sessions_dir.as_deref(),
            Some(home.join("projects").join("alpha").join("sessions").to_string_lossy().as_ref())
        );

        cleanup(&home);
    }

    #[test]
    fn enumeration_skips_non_directories_and_indexless_projects() {
        let home = temp_home("skips");
        seed_project(&home, "real", "cccc", "/repo/c", "2026-01-01T00:00:00Z");
        fs::write(home.join("projects").join("not-a-dir"), "file").expect("write file");
        fs::create_dir_all(home.join("projects").join("no-index")).expect("create dir");

        let registries = enumerate_project_registries(&home);
        assert_eq!(registries.len(), 1);
        assert!(registries[0].index_db_path.ends_with("real/index.sqlite"));
        assert_eq!(list_all_home_sessions(&home).len(), 1);

        cleanup(&home);
    }

    #[test]
    fn absent_projects_dir_is_safe_and_creates_nothing() {
        let home = temp_home("absent");
        assert!(enumerate_project_registries(&home).is_empty());
        assert!(list_all_home_sessions(&home).is_empty());
        assert!(!home.join("projects").exists(), "no registry may be created");

        cleanup(&home);
    }

    #[test]
    fn malformed_registry_does_not_hide_valid_ones() {
        let home = temp_home("malformed");
        seed_project(&home, "good", "dddd", "/repo/d", "2026-01-01T00:00:00Z");
        let bad = home.join("projects").join("bad");
        fs::create_dir_all(&bad).expect("create bad dir");
        fs::write(bad.join("index.sqlite"), "this is not sqlite").expect("write bad index");

        let records = list_all_home_sessions(&home);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].id, "dddd");

        cleanup(&home);
    }

    #[test]
    fn query_panicking_registry_does_not_hide_valid_sessions() {
        let home = temp_home("query-panic");
        seed_project(&home, "good", "eeee", "/repo/e", "2026-01-02T00:00:00Z");

        // A registry that OPENS fine but whose sessions table has the wrong
        // column set: open_session_index's CREATE TABLE IF NOT EXISTS leaves
        // the existing table alone. The table keeps cwd/updated_at so
        // open_session_index's CREATE INDEX still succeeds (the defect must
        // not surface at open time), but drops every other column the
        // list_sessions SELECT needs, so the panic only happens when
        // list_sessions prepares its query.
        let bad_dir = home.join("projects").join("bad");
        fs::create_dir_all(&bad_dir).expect("create bad project dir");
        let bad_db = bad_dir.join("index.sqlite");
        {
            let conn = rusqlite::Connection::open(&bad_db).expect("open raw fixture connection");
            conn.execute_batch(
                "CREATE TABLE sessions (cwd TEXT NOT NULL, updated_at TEXT NOT NULL, unrelated TEXT NOT NULL);",
            )
            .expect("create wrong-schema sessions table");
            conn.close().expect("close raw fixture connection");
        }

        // Preconditions: the malformed registry is enumerated, opens, and only
        // fails at query time — not at open time.
        assert_eq!(enumerate_project_registries(&home).len(), 2);
        let bad_index = open_session_index(&bad_db.to_string_lossy());
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            list_sessions(&bad_index, None)
        }))
        .is_err();
        assert!(panicked, "fixture must open successfully but panic inside list_sessions");
        bad_index.close();

        let records = list_all_home_sessions(&home);
        assert_eq!(records.len(), 1, "the valid registry's session must survive");
        assert_eq!(records[0].id, "eeee");
        assert_eq!(records[0].cwd, "/repo/e");
        assert_eq!(
            records[0].sessions_dir.as_deref(),
            Some(home.join("projects").join("good").join("sessions").to_string_lossy().as_ref())
        );

        cleanup(&home);
    }
}
