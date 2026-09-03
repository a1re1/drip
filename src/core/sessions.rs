use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension};
use uuid::Uuid;

// Re-exported: sessions.ts re-uses the shared memory-note shape, and
// backfill.rs tests import it via the sessions module.
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
        // Connection is closed on Drop; explicit method matches TS API.
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
// CreateSessionArgs — matches TS createSession(index, { cwd, project, now? })
// but takes `now` as a pre-formatted ISO string (backfill tests pass literal
// strings; callers that want wall-clock time pass chrono's to_rfc3339()).
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
    // TS: args.project.slug ?? projectSlug(args.cwd) — an overridden/stubbed
    // project without a slug must still key the row consistently.
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
    let limit = limit.unwrap_or(50);
    let mut stmt = index
        .conn
        .prepare(
            "SELECT id, project_slug, cwd, created_at, updated_at, status, goal_count, last_goal
             FROM sessions ORDER BY updated_at DESC LIMIT ?1",
        )
        .expect("prepare list_sessions");

    stmt.query_map(rusqlite::params![limit], |row| {
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
    .collect()
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

/// touchSession with the TS `patch.now` injection: tests pin timestamps
/// through it; `None` uses the current time like the TS default.
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

    // TS wraps DELETE + INSERTs in db.transaction(): a failed insert must not
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

/// sessions.ts:123 — `sessionPaths(project, record)`: a pre-move record's own
/// sessionsDir wins over the project's.
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
// Multi-registry helpers — sessions.ts:228-361 (siblingWorktreeHomes,
// openProjectIndexes, stamp, listAllSessions, latestAnySession,
// resolveAnySessionRef, hasAnySessionIndex, hasAnyWorktreeSessionIndex)
// ---------------------------------------------------------------------------

use crate::core::home::DripProject;

fn sibling_worktree_homes(project: &DripProject) -> Vec<String> {
    let Some(repo_root) = project.repo_root.as_deref() else {
        return Vec::new();
    };

    let mut homes: Vec<String> = Vec::new();
    let mut roots = vec![repo_root.to_string()];
    roots.extend(crate::core::home::list_linked_worktree_roots(repo_root));

    for root in roots {
        let slug = crate::core::home::project_slug(&root);

        if slug == project.slug {
            continue;
        }

        let home = PathBuf::from(&project.home_root).join("projects").join(&slug);

        if home.join("index.sqlite").exists() {
            homes.push(home.to_string_lossy().into_owned());
        }
    }

    homes
}

pub struct OpenedProjectIndex {
    pub index: SessionIndex,
    pub sessions_dir: String,
}

/// Every registry to read for this project, newest tree first.
///
/// With all_worktrees, the project's own index is followed by every sibling
/// worktree home of the same repo (local wins ties), each entry stamped with
/// its own sessions_dir so session_paths resolves the record into the home the
/// session actually lives in. Without it, the read stays worktree-local.
pub fn open_project_indexes(project: &DripProject, all_worktrees: bool) -> Vec<OpenedProjectIndex> {
    let mut entries: Vec<OpenedProjectIndex> = Vec::new();

    if Path::new(&project.index_db_path).exists() {
        entries.push(OpenedProjectIndex {
            index: open_session_index(&project.index_db_path),
            sessions_dir: project.sessions_dir.clone(),
        });
    }

    if all_worktrees {
        for home in sibling_worktree_homes(project) {
            let home = PathBuf::from(home);
            entries.push(OpenedProjectIndex {
                index: open_session_index(&home.join("index.sqlite").to_string_lossy()),
                sessions_dir: home.join("sessions").to_string_lossy().into_owned(),
            });
        }
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

fn stamp(mut record: SessionRecord, sessions_dir: &str, project: &DripProject) -> SessionRecord {
    // Only a legacy record needs the marker; leaving it off for the project's own
    // tree keeps records comparable to what the single-index path produces.
    if sessions_dir != project.sessions_dir {
        record.sessions_dir = Some(sessions_dir.to_string());
    }

    record
}

/// Union of every registry, newest first — for --list and --continue. With
/// all_worktrees the union spans every sibling worktree home of the same repo.
pub fn list_all_sessions(project: &DripProject, limit: Option<i64>, all_worktrees: bool) -> Vec<SessionRecord> {
    let limit = limit.unwrap_or(50);
    let opened = open_project_indexes(project, all_worktrees);
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
    list_all_sessions(project, Some(1), false).into_iter().next()
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

    let opened = open_project_indexes(project, false);

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

/// The repo-wide form of that gate: true when this worktree OR any sibling
/// worktree of the same repo has an index, so dripw does not read a repo as
/// empty just because every run so far happened in a worktree.
pub fn has_any_worktree_session_index(project: &DripProject) -> bool {
    has_any_session_index(project) || !sibling_worktree_homes(project).is_empty()
}
