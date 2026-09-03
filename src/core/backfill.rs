// One-shot relocation of sessions written before home-keying moved from the
// repo to the checkout (see resolve_drip_project): a session whose cwd sits in a
// linked worktree belongs under that worktree's slug, not the repo's. The
// plan is pure inspection; applying it lands the target row, renames the
// directory, and only then drops the source rows, so nothing is lost if any
// step throws. An interrupted run is re-runnable: the plan also lists a
// session whose directory already sits in its target home while its row is
// still in the source index, and applying that half-done move finishes it.
//
// NOTE (port): projectSlug delegates to the canonical
// crate::core::home::project_slug (imported above; it collapses each RUN of
// non-alphanumerics to one dash, like the TS regex /[^A-Za-z0-9]+/g).
// nearestGitAncestor stays a private copy: backfill takes a deleted
// <repo>/.worktrees/<name> root at its word (see backfill_target_root), a
// wrinkle home's resolver does not share.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::core::home::project_slug;
use crate::core::sessions::{open_session_index, SessionIndex};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackfillMove {
    pub cwd: String,
    pub from_dir: String,
    pub from_slug: String,
    pub id: String,
    pub to_dir: String,
    pub to_slug: String,
}

// projectSlug: delegated to the canonical crate::core::home::project_slug
// (see module NOTE) — the old per-char private copy diverged from home's
// run-collapsing slug (each RUN of non-alphanumerics must fold to ONE dash)
// and has been deleted.

// against the process cwd (tests only pass absolute paths, matching TS).
fn resolve_path(path: &str) -> String {
    let p = Path::new(path);
    if p.is_absolute() {
        normalize(p.to_string_lossy().as_ref())
    } else {
        normalize(&p.to_string_lossy())
    }
}

fn nearest_git_ancestor(start: &str) -> Option<String> {
    let mut dir = resolve_path(start);

    loop {
        if Path::new(&dir).join(".git").exists() {
            return Some(dir);
        }

        let parent = resolve_path(&format!("{}/..", dir));
        if parent == dir {
            return None;
        }
        dir = parent;
    }
}

// strip `.` and resolve `..`/`//` lexically — node:path resolve semantics.
fn normalize(path: &str) -> String {
    let is_abs = path.starts_with('/');
    let mut stack: Vec<&str> = Vec::new();

    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                // node resolve: `..` past root is dropped on absolute paths.
                if is_abs {
                    stack.pop();
                } else if stack.last() == Some(&"..") {
                    stack.push(part);
                } else {
                    match stack.pop() {
                        None => stack.push(part),
                        Some(p) if p == ".." => {
                            stack.push(p);
                            stack.push(part)
                        }
                        Some(_) => {}
                    }
                }
            }
            other => stack.push(other),
        }
    }

    let joined = stack.join("/");
    if is_abs {
        format!("/{}", joined)
    } else {
        joined
    }
}

// The checkout a session's cwd maps to: the nearest git ancestor, which is
// the worktree itself for a cwd inside a live linked worktree. A cwd under a
// `<repo>/.worktrees/<name>` checkout that has since been DELETED would walk
// up past the gap to the main repo and fold the session back in, so a missing
// worktree root is taken at its word instead — the session ran there, and a
// rebuilt worktree at that path should find it. A `.worktrees` directory that
// still exists is not second-guessed: git decides what it is. Null leaves the
// session where it is.
fn backfill_target_root(cwd: &str) -> Option<String> {
    let worktree = find_worktree_prefix(cwd);

    if let Some(ref wt) = worktree {
        if !Path::new(wt).exists() {
            return Some(wt.clone());
        }
    }

    nearest_git_ancestor(cwd)
}

// /^(.*[\\/]\.worktrees[\\/][^\\/]+)/ — the innermost `/.worktrees/<name>`
// prefix of the path. Greedy `.*` picks the LAST one (innermost), matching
// the TS regex.
fn find_worktree_prefix(cwd: &str) -> Option<String> {
    let norm = cwd.replace('\\', "/");
    let last = norm.rfind("/.worktrees/")?;
    let rest = &norm[last + "/.worktrees/".len()..];
    let name = rest.split('/').next()?;
    if name.is_empty() {
        return None;
    }
    Some(norm[..last + "/.worktrees/".len() + name.len()].to_string())
}

// Restored port of the TS planFromMeta closure (backfill.ts:50): only
// sessions with a parseable, self-describing session.json are relocated — cwd
// decides the target home, so a legacy session whose directory lost its
// metadata cannot be placed. The directory is only moved when its metadata id
// matches the directory name; the target is the session's own worktree slug.
fn plan_from_meta(moves: &mut Vec<BackfillMove>, from_slug: &str, from_dir: &Path, projects_dir: &Path) {
    let meta_text = match fs::read_to_string(from_dir.join("session.json")) {
        Ok(text) => text,
        Err(_) => return,
    };
    let meta: serde_json::Value = match serde_json::from_str(&meta_text) {
        Ok(v) => v,
        Err(_) => return,
    };
    let cwd = match meta.get("cwd").and_then(|v| v.as_str()) {
        Some(cwd) => cwd.to_string(),
        None => return,
    };
    let id = match meta.get("id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return,
    };
    let to_slug = match backfill_target_root(&cwd).as_deref().map(project_slug) {
        Some(to_slug) => to_slug,
        None => return,
    };
    if to_slug == from_slug {
        return;
    }
    let to_dir = projects_dir
        .join(&to_slug)
        .join("sessions")
        .join(&id)
        .to_string_lossy()
        .into_owned();
    moves.push(BackfillMove {
        cwd,
        from_dir: from_dir.to_string_lossy().into_owned(),
        from_slug: from_slug.to_string(),
        id,
        to_dir,
        to_slug,
    });
}

pub fn plan_session_backfill(home_root: &str) -> Vec<BackfillMove> {
    let projects_dir = PathBuf::from(home_root).join("projects");
    let mut moves: Vec<BackfillMove> = Vec::new();

    if !projects_dir.exists() {
        return moves;
    }

    for entry in fs::read_dir(&projects_dir).unwrap_or_else(|_| panic!("readdir projects")) {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let from_slug = entry.file_name().to_string_lossy().into_owned();
        let from_project_dir = entry.path();
        let sessions_dir = from_project_dir.join("sessions");

        if sessions_dir.exists() {
            if let Ok(entries) = fs::read_dir(&sessions_dir) {
                for sub in entries.flatten() {
                    plan_from_meta(&mut moves, &from_slug, &sub.path(), &projects_dir);
                }
            }
        }

        // A row whose directory is gone from this home is the tail of a move that
        // was interrupted after the rename: the directory is where the plan would
        // send it, only the rows are still here. Re-listing it lets apply finish
        // the job; a row with no directory anywhere is left alone.
        let index_path = from_project_dir.join("index.sqlite");

        if !index_path.exists() {
            continue;
        }

        let index = open_session_index(index_path.to_string_lossy().as_ref());

        let session_rows: Vec<(String, String)> = {
            let mut v = Vec::new();
            let ok = match index.conn.prepare("SELECT id, cwd FROM sessions") {
                Err(_) => false,
                Ok(mut stmt) => {
                    let rows = stmt.query_map([], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    });
                    if let Ok(iter) = rows {
                        for r in iter.flatten() {
                            v.push(r);
                        }
                    }
                    true
                }
            };
            if !ok {
                drop(index);
                continue;
            }
            v
        };
        drop(index);

        for (id, cwd) in session_rows {
            if sessions_dir.join(&id).exists() {
                continue;
            }

            let target_root = backfill_target_root(&cwd);
            let to_slug = target_root.as_ref().map(|r| project_slug(r));

            let to_slug = match to_slug {
                Some(s) => s,
                None => continue,
            };
            if to_slug == from_slug {
                continue;
            }

            let to_dir = projects_dir.join(&to_slug).join("sessions").join(&id);

            if to_dir.join("session.json").exists() {
                moves.push(BackfillMove {
                    cwd,
                    from_dir: sessions_dir.join(&id).to_string_lossy().into_owned(),
                    from_slug: from_slug.clone(),
                    id,
                    to_dir: to_dir.to_string_lossy().into_owned(),
                    to_slug,
                });
            }
        }
    }

    // (a,b) => a.fromSlug===b.fromSlug ? a.id.localeCompare(b.id) : a.fromSlug.localeCompare(b.fromSlug)
    moves.sort_by(|a, b| {
        if a.from_slug == b.from_slug {
            a.id.cmp(&b.id)
        } else {
            a.from_slug.cmp(&b.from_slug)
        }
    });
    moves
}

// Mirrors the sessions-table row shape; session_memories rows are moved
// verbatim, so only the columns actually copied need names here.
struct IndexSessionRow {
    created_at: String,
    cwd: String,
    goal_count: i64,
    id: String,
    last_goal: Option<String>,
    project_slug: String,
    status: String,
    updated_at: String,
}

// load_session_row: SELECT by column name (SELECT * ordering is an
// implementation detail; the TS reads fields off the row object by name).
fn load_session_row(db: &Connection, id: &str) -> Option<IndexSessionRow> {
    let mut stmt = db
        .prepare(
            "SELECT id, project_slug, cwd, created_at, updated_at, status, goal_count, last_goal FROM sessions WHERE id = ?1",
        )
        .expect("prepare source session row query");
    let mut rows = stmt.query([id]).expect("query source session row");
    if let Some(row) = rows.next().expect("step source session row") {
        Some(IndexSessionRow {
            id: row.get(0).expect("id"),
            project_slug: row.get(1).expect("project_slug"),
            cwd: row.get(2).expect("cwd"),
            created_at: row.get(3).expect("created_at"),
            updated_at: row.get(4).expect("updated_at"),
            status: row.get(5).expect("status"),
            goal_count: row.get(6).expect("goal_count"),
            last_goal: row.get(7).expect("last_goal"),
        })
    } else {
        None
    }
}

pub fn apply_session_backfill(moves: &[BackfillMove]) {
    if moves.is_empty() {
        return;
    }

    // Plan and apply are always a matched pair over one home root, so it is
    // recovered from the first move rather than threaded through the signature:
    // <id> -> sessions -> <fromSlug> -> projects -> home.
    let home_root = Path::new(&moves[0].from_dir)
        .parent() // sessions
        .and_then(|p| p.parent()) // <fromSlug>
        .and_then(|p| p.parent()) // projects
        .and_then(|p| p.parent()) // home
        .expect("move from_dir must live under <home>/projects/<slug>/sessions")
        .to_path_buf();
    // Each slug's index is opened once across the whole batch: sqlite files are
    // created for the target side and every move out of a source costs one open
    // otherwise.
    let mut indexes: HashMap<String, SessionIndex> = HashMap::new();

    for move_ in moves {
        let to_sessions_dir = home_root.join("projects").join(&move_.to_slug).join("sessions");

        if !to_sessions_dir.exists() {
            fs::create_dir_all(&to_sessions_dir).expect("create target sessions dir");
        }

        // The directory is either still at the source or — resuming a move that
        // was interrupted after its rename — already at the target. Both present
        // means the plan is stale (a different session took the id): fail loudly
        // instead of clobbering either.
        let renamed = !Path::new(&move_.from_dir).exists() && Path::new(&move_.to_dir).exists();

        if !renamed && Path::new(&move_.to_dir).exists() {
            panic!(
                "session dir already exists at target: {} (moving from {})",
                move_.to_dir, move_.from_dir
            );
        }

        let meta_dir = if renamed { &move_.to_dir } else { &move_.from_dir };
        let meta_path = Path::new(meta_dir).join("session.json");
        let meta_text = fs::read_to_string(&meta_path).expect("read session.json");
        let mut meta: serde_json::Value = serde_json::from_str(&meta_text).expect("parse session.json");
        let source_index_path = home_root.join("projects").join(&move_.from_slug).join("index.sqlite");
        let source_existed = source_index_path.exists();
        // Open (or reuse) the source index, then copy the row out as owned data
        // so the borrow of `indexes` ends before the target insert below.
        let (row, memories): (Option<IndexSessionRow>, Vec<(String, String, i64, String)>) =
            if source_existed {
                let key = move_.from_slug.clone();
                let index = indexes
                    .entry(key)
                    .or_insert_with(|| {
                        open_session_index(source_index_path.to_string_lossy().as_ref())
                    });
                (
                    load_session_row(&index.conn, &move_.id),
                    list_source_memories(&index.conn, &move_.id),
                )
            } else {
                (None, Vec::new())
            };
        let target_key = move_.to_slug.clone();
        if !indexes.contains_key(&target_key) {
            let target_path = home_root
                .join("projects")
                .join(&target_key)
                .join("index.sqlite");
            let index = open_session_index(target_path.to_string_lossy().as_ref());
            indexes.insert(target_key.clone(), index);
        }
        let target = indexes.get(&target_key).expect("just inserted");

        // A source row is authoritative and overwrites. Without one (a home that
        // never had an index, or a row lost earlier) the target keeps whatever
        // it already holds, else gets a row rebuilt from session.json: the
        // recorded createdAt, the directory's mtime as the last activity, and a
        // finished status — the run cannot still be going.
        match row {
            Some(row) => {
                target
                    .conn
                    .execute(
                        "INSERT OR REPLACE INTO sessions (id, project_slug, cwd, created_at, updated_at, status, goal_count, last_goal)
                         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                        rusqlite::params![
                            move_.id,
                            move_.to_slug,
                            row.cwd,
                            row.created_at,
                            row.updated_at,
                            row.status,
                            row.goal_count,
                            row.last_goal
                        ],
                    )
                    .expect("upsert target session row");
            }
            None => {
                // statSync(dirname(metaPath)).mtime — the session directory, not the meta file.
                let updated_at = mtime_iso_string(Path::new(meta_dir));
                let created_at = match meta.get("createdAt").and_then(|v| v.as_str()) {
                    Some(s) => s.to_string(),
                    None => updated_at.clone(),
                };

                target
                    .conn
                    .execute(
                        "INSERT OR IGNORE INTO sessions (id, project_slug, cwd, created_at, updated_at, status, goal_count, last_goal)
                         VALUES (?, ?, ?, ?, ?, 'idle', 0, NULL)",
                        rusqlite::params![move_.id, move_.to_slug, move_.cwd, created_at, updated_at],
                    )
                    .expect("insert rebuilt target session row");
            }
        }

        for (note_id, text, created_at_iteration, updated_at) in memories {
            target
                .conn
                .execute(
                    "INSERT OR REPLACE INTO session_memories (session_id, note_id, text, created_at_iteration, updated_at) VALUES (?, ?, ?, ?, ?)",
                    rusqlite::params![move_.id, note_id, text, created_at_iteration, updated_at],
                )
                .expect("copy memory row");
        }

        if !renamed {
            fs::rename(&move_.from_dir, &move_.to_dir).expect("rename session dir");
        }

        // The meta file travels with the dir but still names the home it landed
        // under — projectSlug is the slug of the session's own home, which is
        // now the checkout's; keep every other field byte-for-byte.
        if let Some(map) = meta.as_object_mut() {
            map.insert("projectSlug".to_string(), serde_json::json!(move_.to_slug));
        }
        let written = serde_json::to_string_pretty(&meta).expect("serialize session.json")
            + "\n";
        fs::write(Path::new(&move_.to_dir).join("session.json"), written).expect("write session.json");

        // Source rows go last: everything above is idempotent, so a throw before
        // this point leaves a state the next run simply re-applies.
        if source_existed {
            let source = indexes.get(&move_.from_slug).expect("source index");
            source
                .conn
                .execute("DELETE FROM session_memories WHERE session_id = ?", [&move_.id])
                .expect("delete source memories");
            source
                .conn
                .execute("DELETE FROM sessions WHERE id = ?", [&move_.id])
                .expect("delete source session");
        }
    }

    // Consuming the map closes every index (Connection closes on Drop).
    for index in indexes.into_values() {
        index.close();
    }
}

fn list_source_memories(db: &Connection, session_id: &str) -> Vec<(String, String, i64, String)> {
    let mut stmt = db
        .prepare("SELECT note_id, text, created_at_iteration, updated_at FROM session_memories WHERE session_id = ?")
        .expect("prepare memories query");
    let rows = stmt
        .query_map([session_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .expect("query memories");
    rows.flatten().collect()
}

// statSync(...).mtime.toISOString() — millisecond precision, UTC, Z suffix.
fn mtime_iso_string(path: &Path) -> String {
    let mtime = fs::metadata(path)
        .and_then(|m| m.modified())
        .expect("stat session dir");
    let dt: chrono::DateTime<chrono::Utc> = mtime.into();
    dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::sessions::{
        create_session, get_session, list_session_memories, sync_session_memories,
        touch_session, CreateSessionArgs, HarnessMemoryNote, ProjectPaths,
    };
    use std::fs;
    use std::path::{Path, PathBuf};

    // Slug parity: backfill computes slugs through the canonical
    // crate::core::home::project_slug (each RUN of non-alphanumerics collapses
    // to one dash, like the TS /[^A-Za-z0-9]+/g), so a cwd like
    // 'my--repo/a  b' yields the same slug from backfill as from home.
    #[test]
    fn backfill_slug_matches_the_canonical_home_project_slug() {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = PathBuf::from(root.path())
            .join("my--repo")
            .to_string_lossy()
            .into_owned();
        let cwd = format!("{}/a  b", repo);

        fs::create_dir_all(Path::new(&repo).join(".git")).unwrap();
        fs::create_dir_all(&cwd).unwrap();

        // backfill resolves the session's target home from the nearest git
        // ancestor of the session cwd…
        let target_root = backfill_target_root(&cwd).expect("git ancestor");
        assert_eq!(target_root, repo);

        // …and the slug it derives is exactly the canonical one.
        assert_eq!(project_slug(&target_root), crate::core::home::project_slug(&target_root));

        // The cwd 'my--repo/a  b' slugs identically from either path, with each
        // non-alphanumeric RUN ('--' in my--repo, the two spaces in 'a  b')
        // collapsed to a single dash.
        assert_eq!(project_slug(&cwd), crate::core::home::project_slug(&cwd));
        assert_eq!(project_slug(&cwd), format!("{}-a-b", project_slug(&repo)));
        assert!(!project_slug(&cwd).contains("--"));
        assert!(!project_slug(&cwd).contains("  "));
    }

    // Path join over string parts; mirrors node:path join in the TS tests.
    // (macros must be defined before use inside the module)
    macro_rules! join {
        ($base:expr $(, $part:expr)*) => {{
            #[allow(unused_mut)]
            let mut p = ::std::path::PathBuf::from($base);
            $( p.push($part); )*
            p.to_string_lossy().into_owned()
        }};
    }

    // it("relocates sessions keyed by the repo into their checkout's home")
    #[test]
    fn relocates_sessions_keyed_by_the_repo_into_their_checkouts_home() {
        let root = tempfile::tempdir().expect("tempdir");
        let root = root.path().to_str().unwrap();
        let home_root = join!(root, "fake-home");
        let main = join!(root, "repo");
        let linked = join!(&main, ".worktrees", "abc123");

        // A linked worktree's .git is a FILE pointing into the main checkout's
        // .git/worktrees/<name>, whose gitdir file points back at the checkout.
        fs::create_dir_all(join!(&main, ".git", "worktrees", "abc123")).unwrap();
        fs::write(
            join!(&main, ".git", "worktrees", "abc123", "gitdir"),
            format!("{}\n", join!(&linked, ".git")),
        )
        .unwrap();
        fs::create_dir_all(join!(&linked, "src")).unwrap();
        fs::write(join!(&linked, ".git"), format!("gitdir: {}\n", join!(&main, ".git", "worktrees", "abc123"))).unwrap();

        let repo_slug = project_slug(&main);
        let stale_project = ProjectPaths {
            home_root: home_root.clone(),
            memory_dir: join!(&home_root, "projects", &repo_slug, "memory"),
            repo_root: main.clone(),
            root: join!(&main, ".drip"),
            sessions_dir: join!(&home_root, "projects", &repo_slug, "sessions"),
            slug: repo_slug.clone(),
            worktree_root: main.clone(),
        };

        // The stale layout predates the home: nothing created its directories yet.
        fs::create_dir_all(&stale_project.sessions_dir).unwrap();

        // Two sessions mislocated by the old repo-keyed layout: one run in the
        // linked worktree, one in a worktree that has since been deleted. The
        // third already lives in the repo's own home and must not move.
        let index = open_session_index(&join!(&home_root, "projects", &repo_slug, "index.sqlite"));
        let linked_session = create_session(
            &index,
            CreateSessionArgs {
                cwd: join!(&linked, "src"),
                project: &stale_project,
                now: "2026-07-01T00:00:00.000Z",
            },
        );
        let gone_session = create_session(
            &index,
            CreateSessionArgs {
                cwd: join!(&main, ".worktrees", "gone1234", "src"),
                project: &stale_project,
                now: "2026-07-01T01:00:00.000Z",
            },
        );
        let home_session = create_session(
            &index,
            CreateSessionArgs {
                cwd: main.clone(),
                project: &stale_project,
                now: "2026-07-01T02:00:00.000Z",
            },
        );
        // A checkout nested inside a (deleted) worktree keys to the innermost one.
        let nested_session = create_session(
            &index,
            CreateSessionArgs {
                cwd: join!(&main, ".worktrees", "outer", ".worktrees", "inner", "src"),
                project: &stale_project,
                now: "2026-07-01T03:00:00.000Z",
            },
        );
        // A directory with no session.json is not recognizably a session: skipped.
        fs::create_dir_all(join!(&stale_project.sessions_dir, "not-a-session")).unwrap();

        let notes = vec![HarnessMemoryNote {
            created_at_iteration: 3,
            id: "note-1".to_string(),
            text: "the flaky test is timezone-bound".to_string(),
        }];
        sync_session_memories(&index, &linked_session.id, &notes);

        // createSession stamps session.json with the (stale) project it was given;
        // rewrite the three to exactly the pre-move shape the backfill reads.
        for (session, cwd, at) in [
            (&linked_session, join!(&linked, "src"), "2026-07-01T00:00:00.000Z"),
            (&gone_session, join!(&main, ".worktrees", "gone1234", "src"), "2026-07-01T01:00:00.000Z"),
            (&home_session, main.clone(), "2026-07-01T02:00:00.000Z"),
        ] {
            fs::write(
                join!(&stale_project.sessions_dir, &session.id, "session.json"),
                format!(
                    "{}\n",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "createdAt": at,
                        "cwd": cwd,
                        "id": session.id,
                        "projectSlug": repo_slug,
                    }))
                    .unwrap()
                ),
            )
            .unwrap();
        }

        index.close();

        let linked_slug = project_slug(&linked);
        let gone_slug = project_slug(&join!(&main, ".worktrees", "gone1234"));
        let nested_slug = project_slug(&join!(&main, ".worktrees", "outer", ".worktrees", "inner"));
        let moves = plan_session_backfill(&home_root);

        // Ids are random, so the plan's fromSlug-then-id order is asserted by
        // sorting the expectation the same way.
        let mut expected = vec![
            (join!(&linked, "src"), linked_session.id.clone(), linked_slug.clone()),
            (join!(&main, ".worktrees", "gone1234", "src"), gone_session.id.clone(), gone_slug.clone()),
            (join!(&main, ".worktrees", "outer", ".worktrees", "inner", "src"), nested_session.id.clone(), nested_slug.clone()),
        ];
        expected.sort_by(|a, b| a.1.cmp(&b.1));

        let got: Vec<(String, String, String)> = moves
            .iter()
            .map(|m| (m.cwd.clone(), m.id.clone(), m.to_slug.clone()))
            .collect();
        assert_eq!(got, expected);
        assert!(moves.iter().all(|m| m.from_slug == repo_slug));

        apply_session_backfill(&moves);

        // The directories moved, and the moved session.json now names its new home.
        let moved_meta: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(join!(&home_root, "projects", &linked_slug, "sessions", &linked_session.id, "session.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(moved_meta["projectSlug"], linked_slug);
        assert!(Path::new(&join!(&home_root, "projects", &linked_slug, "sessions", &linked_session.id)).exists());
        assert!(Path::new(&join!(&home_root, "projects", &gone_slug, "sessions", &gone_session.id)).exists());
        assert!(!Path::new(&join!(&home_root, "projects", &repo_slug, "sessions", &linked_session.id)).exists());
        assert!(Path::new(&join!(&home_root, "projects", &repo_slug, "sessions", &home_session.id)).exists());
        assert!(Path::new(&join!(&stale_project.sessions_dir, "not-a-session")).exists());
        assert!(Path::new(&join!(&home_root, "projects", &nested_slug, "sessions", &nested_session.id)).exists());

        // The index rows followed the files: gone from the repo home, present
        // under the checkout's slug in the checkout's own index.
        let repo_index = open_session_index(&join!(&home_root, "projects", &repo_slug, "index.sqlite"));
        let linked_index = open_session_index(&join!(&home_root, "projects", &linked_slug, "index.sqlite"));
        let gone_index = open_session_index(&join!(&home_root, "projects", &gone_slug, "index.sqlite"));

        assert!(get_session(&repo_index, &linked_session.id).is_none());
        assert!(get_session(&repo_index, &gone_session.id).is_none());
        assert_eq!(get_session(&repo_index, &home_session.id).unwrap().cwd, main);
        assert_eq!(get_session(&linked_index, &linked_session.id).unwrap().project_slug, linked_slug);
        assert_eq!(get_session(&gone_index, &gone_session.id).unwrap().project_slug, gone_slug);
        // Memory notes cross with their session and leave nothing behind.
        assert_eq!(list_session_memories(&linked_index, &linked_session.id), notes);
        assert_eq!(list_session_memories(&repo_index, &linked_session.id), vec![]);

        linked_index.close();
        gone_index.close();
        repo_index.close();

        // Idempotent: everything already sits where it belongs.
        assert!(plan_session_backfill(&home_root).is_empty());
    }

    // it("re-applies a move interrupted between the index and the rename without losing the row")
    #[test]
    fn re_applies_a_move_interrupted_between_the_index_and_the_rename() {
        let root = tempfile::tempdir().expect("tempdir");
        let root = root.path().to_str().unwrap();
        let home_root = join!(root, "fake-home");
        let main = join!(root, "repo");
        let gone = join!(&main, ".worktrees", "gone1234");
        let repo_slug = project_slug(&main);
        let stale_project = ProjectPaths {
            home_root: home_root.clone(),
            memory_dir: join!(&home_root, "projects", &repo_slug, "memory"),
            repo_root: main.clone(),
            root: join!(&main, ".drip"),
            sessions_dir: join!(&home_root, "projects", &repo_slug, "sessions"),
            slug: repo_slug.clone(),
            worktree_root: main.clone(),
        };

        fs::create_dir_all(join!(&main, ".git")).unwrap();
        fs::create_dir_all(&stale_project.sessions_dir).unwrap();

        let index = open_session_index(&join!(&home_root, "projects", &repo_slug, "index.sqlite"));
        let session = create_session(
            &index,
            CreateSessionArgs {
                cwd: join!(&gone, "src"),
                project: &stale_project,
                now: "",
            },
        );
        touch_session(&index, &session.id, Some("ship it"), Some("completed"));
        index.close();

        let moves = plan_session_backfill(&home_root);
        assert_eq!(moves.len(), 1);
        let move_ = moves[0].clone();

        // Simulate a crash after the rename but before the source rows were
        // dropped: the directory already sits at the target, the rows are still
        // in the source index. The plan re-lists it from the index alone.
        fs::create_dir_all(parent_of(&move_.to_dir)).unwrap();
        fs::rename(&move_.from_dir, &move_.to_dir).unwrap();
        assert_eq!(plan_session_backfill(&home_root), vec![move_.clone()]);
        apply_session_backfill(&[move_.clone()]);

        let source = open_session_index(&join!(&home_root, "projects", &repo_slug, "index.sqlite"));
        assert!(get_session(&source, &session.id).is_none());
        source.close();

        let target = open_session_index(&join!(&home_root, "projects", &move_.to_slug, "index.sqlite"));
        let relocated = get_session(&target, &session.id);
        target.close();

        assert!(Path::new(&move_.to_dir).exists());
        assert!(!Path::new(&move_.from_dir).exists());
        let relocated = relocated.expect("relocated row");
        assert_eq!(relocated.goal_count, 1);
        assert_eq!(relocated.last_goal.as_deref(), Some("ship it"));
        assert_eq!(relocated.status, "completed");
        assert!(plan_session_backfill(&home_root).is_empty());
    }

    // it("refuses to overwrite an existing session directory")
    #[test]
    fn refuses_to_overwrite_an_existing_session_directory() {
        let root = tempfile::tempdir().expect("tempdir");
        let root = root.path().to_str().unwrap();
        let home_root = join!(root, "fake-home");
        let repo_slug = project_slug(root);
        let from_dir = join!(&home_root, "projects", &repo_slug, "sessions", "sess-1");
        let to_dir = join!(&home_root, "projects", "-other", "sessions", "sess-1");

        fs::create_dir_all(&from_dir).unwrap();
        fs::create_dir_all(&to_dir).unwrap();
        fs::write(
            join!(&from_dir, "session.json"),
            format!(
                "{}\n",
                serde_json::to_string_pretty(&serde_json::json!({
                    "cwd": "/other",
                    "id": "sess-1",
                    "projectSlug": repo_slug,
                }))
                .unwrap()
            ),
        )
        .unwrap();

        let move_ = BackfillMove {
            cwd: "/other".to_string(),
            from_dir: from_dir.clone(),
            from_slug: repo_slug.clone(),
            id: "sess-1".to_string(),
            to_dir: to_dir.clone(),
            to_slug: "-other".to_string(),
        };
        let result = std::panic::catch_unwind(|| apply_session_backfill(&[move_]));
        assert!(result.is_err(), "expected a panic containing sess-1");
    }

    // Path join over string parts; mirrors node:path join in the TS tests.
    macro_rules! join {
        ($base:expr $(, $part:expr)*) => {{
            #[allow(unused_mut)]
            let mut p = ::std::path::PathBuf::from($base);
            $( p.push($part); )*
            p.to_string_lossy().into_owned()
        }};
    }

    fn parent_of(path: &str) -> String {
        Path::new(path)
            .parent()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

}
