// port of src/cli/sessions.ts

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

pub fn open_session_index(db_path: &str) -> SessionIndex {
    // Create parent directories if needed.
    if let Some(parent) = Path::new(db_path).parent() {
        fs::create_dir_all(parent).expect("create index parent dirs");
    }

    let conn = Connection::open(db_path).expect("open sqlite db");

    conn.execute_batch("PRAGMA journal_mode = WAL;").ok();
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
