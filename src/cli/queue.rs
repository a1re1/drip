use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::lib_fs::{read_jsonl_records, write_file_atomic};

// ---------------------------------------------------------------------------
// QueuedGoal
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedGoal {
    pub at: String,
    pub goal: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "maxIterations")]
    pub max_iterations: Option<i64>,
}

// ---------------------------------------------------------------------------
// read_queue_lines
// ---------------------------------------------------------------------------

fn read_queue_lines(queue_path: &Path) -> Vec<String> {
    // readJsonlRecords gives { raw, parsed } per complete line — the queue only
    // needs the raw strings; a missing file reads as empty (lib_fs behavior).
    read_jsonl_records::<serde_json::Value>(queue_path)
        .into_iter()
        .map(|record| record.raw)
        .collect()
}

// ---------------------------------------------------------------------------
// cursor helpers
// ---------------------------------------------------------------------------

fn cursor_path_for(queue_path: &Path) -> PathBuf {
    let mut s = queue_path.as_os_str().to_owned();
    s.push(".cursor");
    PathBuf::from(s)
}

fn read_cursor(queue_path: &Path) -> usize {
    let cursor_path = cursor_path_for(queue_path);
    if !cursor_path.exists() {
        return 0;
    }
    let raw = match fs::read_to_string(&cursor_path) {
        Ok(s) => s,
        Err(_) => return 0,
    };
    let parsed: i64 = match raw.trim().parse() {
        Ok(n) => n,
        Err(_) => return 0,
    };
    if parsed >= 0 {
        parsed as usize
    } else {
        0
    }
}

fn write_cursor(queue_path: &Path, value: usize) {
    let cursor_path = cursor_path_for(queue_path);
    let _ = write_file_atomic(&cursor_path, &value.to_string(), false);
}

// ---------------------------------------------------------------------------
// append_queued_goal
// Returns the number of pending (not-yet-drained) goals after appending.
// ---------------------------------------------------------------------------

pub fn append_queued_goal(queue_path: &Path, goal: &str, max_iterations: Option<i64>) -> usize {
    if let Some(dir) = queue_path.parent() {
        let _ = fs::create_dir_all(dir);
    }

    let at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let entry = QueuedGoal {
        at,
        goal: goal.to_string(),
        max_iterations,
    };
    let line = format!("{}\n", serde_json::to_string(&entry).unwrap_or_default());
    use std::fs::OpenOptions;
    use std::io::Write;
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(queue_path) {
        let _ = f.write_all(line.as_bytes());
    }

    pending_queued_goals(queue_path).len()
}

// ---------------------------------------------------------------------------
// pending_queued_goals
// ---------------------------------------------------------------------------

pub fn pending_queued_goals(queue_path: &Path) -> Vec<QueuedGoal> {
    let lines = read_queue_lines(queue_path);
    let cursor = read_cursor(queue_path);
    let remaining = lines.into_iter().skip(cursor);
    let mut entries = Vec::new();

    for line in remaining {
        if let Ok(parsed) = serde_json::from_str::<QueuedGoal>(&line) {
            if !parsed.goal.trim().is_empty() {
                entries.push(parsed);
            }
        }
        // Malformed lines are skipped; they advance cursor at take time.
    }

    entries
}

// ---------------------------------------------------------------------------
// take_next_queued_goal
// Pops the next runnable queued goal, advancing past malformed lines.
// ---------------------------------------------------------------------------

pub fn take_next_queued_goal(queue_path: &Path) -> Option<QueuedGoal> {
    let lines = read_queue_lines(queue_path);
    let mut cursor = read_cursor(queue_path);

    while cursor < lines.len() {
        let line = &lines[cursor];
        cursor += 1;

        if let Ok(parsed) = serde_json::from_str::<QueuedGoal>(line) {
            if !parsed.goal.trim().is_empty() {
                write_cursor(queue_path, cursor);
                return Some(parsed);
            }
        }
        // Skip malformed / empty-goal lines.
    }

    write_cursor(queue_path, cursor);
    None
}

// ---------------------------------------------------------------------------
// DrainOutcome
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrainOutcome {
    /// Exit code of the LAST goal run (queued runs override the first run's).
    pub exit_code: i32,
    pub ran_goals: usize,
}

// ---------------------------------------------------------------------------
// drain_queued_goals
// The drain loop's control flow, extracted for testability.
// Each lap re-reads the queue so enqueues landing mid-drain run too;
// a LiveRunError-shaped refusal stops draining; an abort stops between goals.
// ---------------------------------------------------------------------------

// In Rust we use a synchronous callback (no async runtime needed for the
// drain logic; async callers wrap this in tokio::task::spawn_blocking or
// call async equivalents themselves).  The TS version is async because
// runGoal was async; the Rust port accepts a closure returning Result<i32>.
// The LiveRunError name-check becomes an explicit flag in the error type.
// The abort signal is a closure re-checked at the top of every lap (mirrors
// the TS `while (!args.signal?.aborted)` loop condition).

pub struct LiveRunError(pub String);

pub fn drain_queued_goals(
    queue_path: &Path,
    initial_exit_code: i32,
    mut on_live_run_refusal: Option<&mut dyn FnMut(&str)>,
    run_goal: &mut dyn FnMut(&QueuedGoal) -> std::result::Result<i32, DrainError>,
    aborted: &dyn Fn() -> bool,
) -> Result<DrainOutcome, DrainError> {
    let mut exit_code = initial_exit_code;
    let mut ran_goals: usize = 0;

    while !(aborted)() {
        let queued = match take_next_queued_goal(queue_path) {
            Some(q) => q,
            None => break,
        };

        match run_goal(&queued) {
            Ok(code) => {
                exit_code = code;
                ran_goals += 1;
            }
            Err(DrainError::LiveRunError(msg)) => {
                if let Some(ref mut cb) = on_live_run_refusal {
                    cb(&msg);
                }
                break;
            }
            Err(e @ DrainError::Other(_)) => return Err(e),
        }
    }

    Ok(DrainOutcome { exit_code, ran_goals })
}

/// Errors that drain_queued_goals recognises.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DrainError {
    /// Equivalent to TS `error.name === "LiveRunError"` — stops the drain.
    LiveRunError(String),
    /// Any other error — re-thrown in TS; returned as `Err` here.
    Other(String),
}

// ---------------------------------------------------------------------------
// Tests — ports of test/cli-queue.test.ts
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_queue_path() -> (TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.jsonl");
        (dir, path)
    }

    // it("appends, reports positions, and drains in order via the cursor")
    #[test]
    fn appends_reports_positions_and_drains_in_order_via_the_cursor() {
        let (_dir, queue_path) = make_queue_path();

        assert!(take_next_queued_goal(&queue_path).is_none());

        assert_eq!(append_queued_goal(&queue_path, "first goal", Some(8)), 1);
        assert_eq!(append_queued_goal(&queue_path, "second goal", None), 2);

        let first = take_next_queued_goal(&queue_path).expect("first goal");
        assert_eq!(first.goal, "first goal");
        assert_eq!(first.max_iterations, Some(8));

        assert_eq!(pending_queued_goals(&queue_path).len(), 1);

        let second = take_next_queued_goal(&queue_path).expect("second goal");
        assert_eq!(second.goal, "second goal");
        assert!(second.max_iterations.is_none());

        assert!(take_next_queued_goal(&queue_path).is_none());

        // Enqueues landing after a full drain still run.
        append_queued_goal(&queue_path, "third goal", None);
        let third = take_next_queued_goal(&queue_path).expect("third goal");
        assert_eq!(third.goal, "third goal");
    }

    // it("skips malformed lines without stalling the queue")
    #[test]
    fn skips_malformed_lines_without_stalling_the_queue() {
        let (_dir, queue_path) = make_queue_path();

        // Write a torn JSON line directly
        use std::fs::OpenOptions;
        use std::io::Write;
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&queue_path)
            .unwrap();
        f.write_all(b"{torn json\n").unwrap();
        drop(f);

        append_queued_goal(&queue_path, "real goal", None);

        let got = take_next_queued_goal(&queue_path).expect("real goal");
        assert_eq!(got.goal, "real goal");
        assert!(take_next_queued_goal(&queue_path).is_none());
    }

    // it("runs queued goals in order, last exit code wins, and picks up mid-drain enqueues")
    #[test]
    fn runs_queued_goals_in_order_last_exit_code_wins_and_picks_up_mid_drain_enqueues() {
        let (_dir, queue_path) = make_queue_path();
        let mut ran: Vec<String> = Vec::new();

        append_queued_goal(&queue_path, "first", None);
        append_queued_goal(&queue_path, "second", None);

        let queue_path_clone = queue_path.clone();
        let outcome = drain_queued_goals(
            &queue_path,
            0,
            None,
            &mut |queued: &QueuedGoal| {
                ran.push(queued.goal.clone());
                if queued.goal == "first" {
                    // Enqueue landing mid-drain
                    append_queued_goal(&queue_path_clone, "third", None);
                    return Ok(2);
                }
                Ok(if queued.goal == "third" { 0 } else { 2 })
            },
            &|| false, // abort signal never fires
        )
        .unwrap();

        assert_eq!(ran, vec!["first", "second", "third"]);
        assert_eq!(outcome, DrainOutcome { exit_code: 0, ran_goals: 3 });
    }

    // it("stops draining on a LiveRunError-shaped refusal and on abort")
    #[test]
    fn stops_draining_on_live_run_error_refusal_and_on_abort() {
        let (_dir, queue_path) = make_queue_path();
        let mut refusals: Vec<String> = Vec::new();

        append_queued_goal(&queue_path, "a", None);
        append_queued_goal(&queue_path, "b", None);

        let outcome = drain_queued_goals(
            &queue_path,
            0,
            Some(&mut |msg: &str| refusals.push(msg.to_string())),
            &mut |_| Err(DrainError::LiveRunError("another process owns the lease".to_string())),
            &|| false,
        )
        .unwrap();

        assert_eq!(outcome, DrainOutcome { exit_code: 0, ran_goals: 0 });
        assert_eq!(refusals.len(), 1);

        // Abort scenario — signal is aborted before the drain starts
        let aborted_outcome = drain_queued_goals(
            &queue_path,
            2,
            None,
            &mut |_| Ok(0),
            &|| true, // abort signal already fired before the first lap
        )
        .unwrap();
        assert_eq!(aborted_outcome, DrainOutcome { exit_code: 2, ran_goals: 0 });
    }

    // TS re-throws non-LiveRunError errors instead of swallowing them; the
    // Rust port returns them as Err(DrainError::Other) rather than panicking.
    #[test]
    fn other_errors_are_returned_not_panicked() {
        let (_dir, queue_path) = make_queue_path();
        append_queued_goal(&queue_path, "a", None);

        let result = drain_queued_goals(
            &queue_path,
            0,
            None,
            &mut |_| Err(DrainError::Other("boom".to_string())),
            &|| false,
        );

        assert_eq!(result, Err(DrainError::Other("boom".to_string())));
    }
}
