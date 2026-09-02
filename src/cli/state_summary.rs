// port of src/cli/state-summary.ts
//
// Imports from state-summary.ts:
//   countTaskStats, deriveVerificationSummary — ported in drip/src/cli/run_record.rs
//   loadHarnessState — ported inline below from src/harness/state.ts:516-536
//     (drip/src/core/state.rs is still a stub owned by another port lane)
//   readInboxMessages — ported inline here (from src/cli/follow.ts) since follow.rs
//     is not yet a drip module; the helper is small and has no other consumer this wave.
//   checkLease — ported in drip/src/core/lease.rs

use std::path::Path;

use serde::Serialize;
use serde_json::Value;

use crate::cli::run_record::{count_task_stats, derive_verification_summary};
use crate::core::lease::check_lease;
use crate::core::types::{HarnessState, HarnessTask, HarnessTaskStatus};
// ---------------------------------------------------------------------------
// loadHarnessState (inlined from src/harness/state.ts:516-536)
// Loads + shape-validates a harness state file: missing file -> None; a file
// present but malformed raises the TS error string verbatim. Backfills the
// loop clock for state files written before task loops existed. NOTE:
// drip/src/core/state.rs is still a stub owned by another port lane; once it
// lands its load_harness_state, this private copy should delegate to it.
// ---------------------------------------------------------------------------

fn is_harness_state(value: &serde_json::Value) -> bool {
    value.is_object()
        && value.get("goal").map_or(false, |v| v.is_string())
        && value.get("iteration").map_or(false, |v| v.is_i64())
        && value.get("tasks").map_or(false, |v| v.is_array())
        && value.get("memory").map_or(false, |v| v.is_array())
        && value.get("observations").map_or(false, |v| v.is_array())
        && value.get("promotedContext").map_or(false, |v| v.is_array())
        && value.get("telemetry").map_or(false, |v| v.is_object())
}

fn load_harness_state(state_path: &Path) -> Result<Option<HarnessState>, String> {
    if !state_path.exists() {
        return Ok(None);
    }

    let content = match std::fs::read_to_string(state_path) {
        Ok(c) => c,
        // Unreadable file: TS loadHarnessState would throw the read error.
        Err(error) => {
            return Err(format!(
                "Could not read harness state at {}: {error}",
                state_path.display()
            ))
        }
    };

    let mut state: HarnessState = match serde_json::from_str(&content) {
        Ok(s) => s,
        // Corrupt JSON: TS JSON.parse throws before the shape check.
        Err(error) => {
            return Err(format!(
                "Could not read harness state at {}: {error}",
                state_path.display()
            ))
        }
    };

    if !is_harness_state(&serde_json::to_value(&state).unwrap_or(Value::Null)) {
        return Err(format!(
            "The file at {} is not a valid harness state file.",
            state_path.display()
        ));
    }

    // State files written before goal history existed load with an empty history
    // (serde defaults the array); same for observations. State files written
    // before task loops existed used one loop per activation, so the iteration
    // counter is the correct continuation point for the loop clock.
    if state.r#loop == 0 {
        state.r#loop = state.iteration;
    }

    Ok(Some(state))
}


// ---------------------------------------------------------------------------
// readInboxMessages (inlined from src/cli/follow.ts:readInboxMessages)
// Reads JSONL records from inboxPath, skips the first consumedCount entries,
// returns the `text` field of each remaining parsed record.
// ---------------------------------------------------------------------------

fn read_inbox_messages(inbox_path: &Path, consumed_count: usize) -> Vec<String> {
    if !inbox_path.exists() {
        return vec![];
    }

    let content = match std::fs::read_to_string(inbox_path) {
        Ok(c) => c,
        Err(_) => return vec![],
    };

    let last_newline = match content.rfind('\n') {
        Some(pos) => pos,
        None => return vec![],
    };

    let complete = &content[..last_newline];
    let mut messages: Vec<String> = Vec::new();

    // readInboxEntries slices readJsonlRecords(...) output, and readJsonlRecords
    // (src/lib/fs.ts) yields one record per non-blank complete line (unparseable
    // lines still count as records, surfacing as an empty text), so the consumed
    // count indexes non-blank lines — not raw line numbers.
    let mut kept = 0usize;
    for line in complete.split('\n') {
        if line.trim().is_empty() {
            continue;
        }
        kept += 1;
        if kept <= consumed_count {
            continue;
        }
        let text = serde_json::from_str::<serde_json::Value>(line)
            .ok()
            .and_then(|v| v.get("text").and_then(|t| t.as_str()).map(|s| s.to_string()))
            .unwrap_or_default();
        messages.push(text);
    }

    messages
}

// ---------------------------------------------------------------------------
// buildStateSummaryJson
// ---------------------------------------------------------------------------

/// task.status in state-summary.ts is already the wire string ("in_progress"
/// etc., src/harness/types.ts:1); drip models it as the HarnessTaskStatus enum,
/// so serialize it back to its TS spelling.
fn harness_task_status_json(status: crate::core::types::HarnessTaskStatus) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_default()
}

/// Serializable task shape for the JSON summary (id, status, title, optional summary).
#[derive(Debug, Serialize)]
pub struct TaskSummaryEntry {
    pub id: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    pub title: String,
}

fn task_to_summary_entry(task: &HarnessTask) -> TaskSummaryEntry {
    TaskSummaryEntry {
        id: task.id.clone(),
        status: harness_task_status_json(task.status),
        summary: task.summary.clone().filter(|s| !s.is_empty()),
        title: task.title.clone(),
    }
}

/// Machine-readable summary: goal, running?, tasks, taskStats, lastVerification, etc.
/// Returns None when no state file exists yet.
pub fn build_state_summary_json(
    inbox_path: &Path,
    lease_path: &Path,
    state_path: &Path,
) -> Option<serde_json::Map<String, Value>> {
    // Per the port contract, an Err here (unreadable/corrupt state) is treated
    // the same as a missing file: the JSON summary builder has no error path.
    let state = match load_harness_state(state_path) {
        Ok(s) => s,
        Err(_) => None,
    }?;

    // slice(Math.max(0, consumedCount)) in readInboxEntries clamps negatives to 0.
    let consumed = state.inbox_cursor.unwrap_or(0).max(0) as usize;
    let pending_operator_messages = read_inbox_messages(inbox_path, consumed).len();

    let lease_status = check_lease(lease_path, &|| chrono::Utc::now());
    let running = lease_status.alive();

    let tasks: Vec<Value> = state
        .tasks
        .iter()
        .map(|t| serde_json::to_value(task_to_summary_entry(t)).unwrap_or(Value::Null))
        .collect();

    let task_stats = count_task_stats(&state.tasks);
    let last_verification = derive_verification_summary(&state);

    let mut map = serde_json::Map::new();
    map.insert("goal".into(), Value::String(state.goal.clone()));
    map.insert(
        "historyCount".into(),
        Value::Number(serde_json::Number::from(state.history.len() as i64)),
    );
    map.insert(
        "iteration".into(),
        Value::Number(serde_json::Number::from(state.iteration)),
    );
    map.insert(
        "lastVerification".into(),
        serde_json::to_value(last_verification).unwrap_or(Value::Null),
    );
    map.insert(
        "memoryCount".into(),
        Value::Number(serde_json::Number::from(state.memory.len() as i64)),
    );
    map.insert(
        "pendingOperatorMessages".into(),
        Value::Number(serde_json::Number::from(pending_operator_messages as i64)),
    );
    map.insert("running".into(), Value::Bool(running));
    map.insert("tasks".into(), Value::Array(tasks));
    map.insert(
        "taskStats".into(),
        serde_json::to_value(task_stats).unwrap_or(Value::Null),
    );

    Some(map)
}

// ---------------------------------------------------------------------------
// formatStateSummary
// ---------------------------------------------------------------------------

/// Human-readable single-string summary of a session's state.
pub fn format_state_summary(state_path: &Path) -> String {
    let state = match load_harness_state(state_path) {
        Ok(Some(s)) => s,
        Ok(None) => return "no harness state yet — send a goal to start.".to_string(),
        Err(message) => return message,
    };

    let mut lines: Vec<String> = vec![
        format!("goal: {}", state.goal),
        format!("iteration: {}", state.iteration),
        String::new(),
    ];

    lines.push("tasks:".to_string());

    if state.tasks.is_empty() {
        lines.push("  none yet".to_string());
    }

    for task in &state.tasks {
        let mark = match task.status {
            HarnessTaskStatus::Completed => "x",
            HarnessTaskStatus::Blocked => "!",
            HarnessTaskStatus::Dropped => "-",
            _ => " ",
        };
        let summary_suffix = task
            .summary
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|s| format!(" — {s}"))
            .unwrap_or_default();
        lines.push(format!(
            "  [{mark}] {id}: {title}{summary_suffix}",
            mark = mark,
            id = task.id,
            title = task.title,
            summary_suffix = summary_suffix
        ));
    }

    lines.push(String::new());
    lines.push(format!("memory ({}):", state.memory.len()));

    for note in &state.memory {
        lines.push(format!("  ({}) {}", note.id, note.text));
    }

    if !state.promoted_context.is_empty() {
        lines.push(String::new());
        lines.push(format!(
            "warm context ({}):",
            state.promoted_context.len()
        ));
        for entry in &state.promoted_context {
            lines.push(format!(
                "  {} {} (ttl {}, reinforcements {})",
                entry.tool_name, entry.input_preview, entry.ttl, entry.reinforcements
            ));
        }
    }

    if !state.history.is_empty() {
        lines.push(String::new());
        lines.push(format!(
            "history: {} earlier goal(s) this session",
            state.history.len()
        ));
    }

    lines.join("\n")
}

// ---------------------------------------------------------------------------
// Tests — state-summary.ts has no dedicated test file; behavior is covered
// by the integration tests for the --state flag (not yet ported). Two smoke
// tests verify the two functions work end-to-end without panicking.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::HarnessState;
    use std::io::Write;
    use tempfile::TempDir;

    fn make_temp_root() -> TempDir {
        tempfile::tempdir().unwrap()
    }

    fn write_state(dir: &Path, state: &HarnessState) {
        let path = dir.join("state.json");
        let json = serde_json::to_string_pretty(state).unwrap();
        std::fs::write(&path, json).unwrap();
    }

    // format_state_summary returns "no harness state yet" for missing file
    #[test]
    fn format_state_summary_missing_returns_placeholder() {
        let root = make_temp_root();
        let state_path = root.path().join("state.json");
        let result = format_state_summary(&state_path);
        assert_eq!(result, "no harness state yet — send a goal to start.");
    }

    // format_state_summary reports missing state with the placeholder text
    #[test]
    fn format_state_summary_reports_missing_state() {
        let root = make_temp_root();
        let state_path = root.path().join("state.json");
        assert!(!state_path.exists());
        let result = format_state_summary(&state_path);
        assert_eq!(result, "no harness state yet — send a goal to start.");
    }

    // format_state_summary surfaces the corrupt-file error verbatim
    #[test]
    fn format_state_summary_reports_corrupt_state() {
        let root = make_temp_root();
        let state_path = root.path().join("state.json");
        std::fs::write(&state_path, "{not json").unwrap();

        let result = format_state_summary(&state_path);
        assert!(
            result.contains("Could not read harness state at"),
            "expected read-error text, got: {result}"
        );
    }

    // format_state_summary renders goal, iteration, tasks, memory
    #[test]
    fn format_state_summary_renders_fields() {
        let root = make_temp_root();
        let mut state = HarnessState::default();
        state.goal = "do the thing".into();
        state.iteration = 3;
        write_state(root.path(), &state);

        let result = format_state_summary(&root.path().join("state.json"));
        assert!(result.contains("goal: do the thing"), "goal line missing");
        assert!(result.contains("iteration: 3"), "iteration line missing");
        assert!(result.contains("tasks:"), "tasks section missing");
        assert!(result.contains("  none yet"), "empty tasks missing");
        assert!(result.contains("memory (0):"), "memory line missing");
    }

    // build_state_summary_json returns None for missing state
    #[test]
    fn build_state_summary_json_missing_returns_none() {
        let root = make_temp_root();
        let result = build_state_summary_json(
            &root.path().join("inbox.jsonl"),
            &root.path().join("lease.json"),
            &root.path().join("state.json"),
        );
        assert!(result.is_none());
    }

    // build_state_summary_json returns the expected keys for a valid state
    #[test]
    fn build_state_summary_json_keys_present() {
        let root = make_temp_root();
        let mut state = HarnessState::default();
        state.goal = "build it".into();
        write_state(root.path(), &state);

        let result = build_state_summary_json(
            &root.path().join("inbox.jsonl"),
            &root.path().join("lease.json"),
            &root.path().join("state.json"),
        )
        .unwrap();

        assert!(result.contains_key("goal"));
        assert!(result.contains_key("running"));
        assert!(result.contains_key("tasks"));
        assert!(result.contains_key("taskStats"));
        assert!(result.contains_key("iteration"));
        assert!(result.contains_key("historyCount"));
        assert!(result.contains_key("memoryCount"));
        assert!(result.contains_key("pendingOperatorMessages"));
        assert_eq!(result["goal"], Value::String("build it".into()));
        assert_eq!(result["running"], Value::Bool(false));
    }
}
