// Run-record IO: building, saving, and loading the per-goal run record, plus
// wrappers over the shared task-tally and verification-summary derivations.
//     -> `count_task_stats` / `derive_verification_summary`

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::core::types::{
	HarnessLeakedJob, HarnessRunReason, HarnessRunResult, HarnessRunUsage, HarnessState,
	HarnessTask, HarnessTaskStatus, TaskStats, VerificationSummary,
};

// The persisted outcome of a session's most recent run. The headless result
// line used to exist only on the run process's stdout — if the driving script
// lost it (tool timeout, pipe break, run started from the TUI/web), the
// outcome was unrecoverable. Every surface now persists this record at
// run-end, and `drip --result` / `drip --wait` replay it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunRecord {
	/// How to continue an unfinished run (e.g. awaiting-input) — the same
	/// command the headless result line carries, so `--result`/`--wait`
	/// drivers can resume without having seen that stdout line.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub continue_command: Option<String>,
	pub ended_at: String,
	/// Present when reason is "error": what killed the run.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub error_message: Option<String>,
	pub goal: String,
	pub goal_id: String,
	pub iterations: i64,
	/// Background jobs still running when the run ended.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub leaked_jobs: Option<Vec<HarnessLeakedJob>>,
	/// mutationsAfter > 0 means the result is STALE: workspace edits landed after this run.
	pub last_verification: Option<VerificationSummary>,
	pub loops: i64,
	/// The --max-iterations cap the run was given, when one was set.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub max_iterations: Option<i64>,
	/// Operator messages that arrived too late for this run.
	pub pending_operator_messages: i64,
	pub reason: String,
	/// Present when a stop was requested: ms from abort signal to run end.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub stop_latency_ms: Option<i64>,
	pub summary: Option<String>,
	/// Token/latency economics of the run; null on records from older versions.
	/// (Declared before taskStats so serde emits the canonical key order.)
	pub usage: Option<HarnessRunUsage>,
	pub task_stats: TaskStats,
	/// How the last completion was anchored (external check vs declared none) and the claimed confidence.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub completion_anchor: Option<crate::core::types::CompletionAnchor>,
	/// Expectations the run could not reconcile (reason "unreconciled").
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub anomalies: Option<Vec<crate::core::types::HarnessAnomaly>>,
}

pub struct BuildRunRecordArgs<'a> {
	pub ended_at: &'a str,
	pub goal: &'a str,
	pub goal_id: &'a str,
	pub max_iterations: Option<i64>,
	pub pending_operator_messages: i64,
	pub result: &'a HarnessRunResult,
}

pub fn build_run_record(args: &BuildRunRecordArgs) -> RunRecord {
	let tasks = &args.result.state.tasks;
	let reason = run_reason_string(&args.result.reason);

	RunRecord {
		continue_command: args.result.continue_command.clone(),
		ended_at: args.ended_at.to_string(),
		// An empty error message is dropped, same as a missing one.
		error_message: args
			.result
			.error_message
			.clone()
			.filter(|message| !message.is_empty()),
		goal: args.goal.to_string(),
		goal_id: args.goal_id.to_string(),
		iterations: args.result.iterations,
		// An empty leaked-jobs list is dropped, same as a missing one.
		leaked_jobs: args
			.result
			.leaked_jobs
			.clone()
			.filter(|jobs| !jobs.is_empty()),
		last_verification: derive_verification_summary(&args.result.state),
		loops: args.result.loops,
		max_iterations: args.max_iterations,
		pending_operator_messages: args.pending_operator_messages,
		stop_latency_ms: args.result.stop_latency_ms,
		summary: args
			.result
			.state
			.run_summary
			.as_ref()
			.map(|note| note.text.clone()),
		task_stats: count_task_stats(tasks),
		usage: Some(args.result.usage.clone()),
		completion_anchor: args.result.state.completion_anchor.clone(),
		// An empty anomaly list is dropped, same as a missing one.
		anomalies: Some(args.result.state.anomalies.clone()).filter(|anomalies| !anomalies.is_empty()),
		// Run-level record: `status` uses the run vocabulary marker, not a
		// task finish status, so consumers never confuse the two.
		reason,
	}
}

// The record stores the reason union's literal wire string; round-trip through
// serde's rename table, the single source of those strings.
// serde's rename table, the single source of the wire strings.
fn run_reason_string(reason: &HarnessRunReason) -> String {
	serde_json::to_value(reason)
		.expect("HarnessRunReason serializes to a string")
		.as_str()
		.expect("HarnessRunReason serializes to a string")
		.to_string()
}

// Temp+rename so a crash mid-write never leaves a torn record — the same
// discipline saveHarnessState uses for state.json.
pub fn save_run_record(path: &Path, record: &RunRecord) -> std::io::Result<()> {
	// JSON.stringify(record, null, 2) — pretty-printed, no trailing newline.
	crate::lib_fs::write_file_atomic(
		path,
		&serde_json::to_string_pretty(record).expect("run record serializes"),
		false,
	)
}

pub fn load_run_record(path: &Path) -> Option<RunRecord> {
	if !path.exists() {
		return None;
	}

	let content = fs::read_to_string(path).ok()?;
	let parsed: serde_json::Value = serde_json::from_str(&content).ok()?;

	// parsed && typeof parsed === "object" && typeof parsed.reason === "string"
	// && typeof parsed.endedAt === "string"
	let object = parsed.as_object()?;
	object.get("reason")?.as_str()?;
	object.get("endedAt")?.as_str()?;

	// Only reason/endedAt are gated before the struct parse, so a hand-edited
	// record that trims other fields reads as null — the same "no run recorded"
	// outcome as a torn file (no test observes the difference).
	// outcome as a torn file (no test observes the difference).
	serde_json::from_value(parsed).ok()
}

// Delegates to the shared verification-summary derivation in core::state.
pub fn derive_verification_summary(state: &HarnessState) -> Option<VerificationSummary> {
	crate::core::state::derive_verification_summary(state)
}

// Tally the state's tasks into blocked / completed / dropped counts.
pub fn count_task_stats(tasks: &[HarnessTask]) -> TaskStats {
	TaskStats {
		blocked: tasks
			.iter()
			.filter(|task| task.status == HarnessTaskStatus::Blocked)
			.count() as i64,
		completed: tasks
			.iter()
			.filter(|task| task.status == HarnessTaskStatus::Completed)
			.count() as i64,
		dropped: tasks
			.iter()
			.filter(|task| task.status == HarnessTaskStatus::Dropped)
			.count() as i64,
		pending: tasks
			.iter()
			.filter(|task| {
				task.status == HarnessTaskStatus::Pending
					|| task.status == HarnessTaskStatus::InProgress
			})
			.count() as i64,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	use crate::core::types::HarnessVerificationRecord;

	// The test task literals only set seven fields; the rest are None here,
	// matching the fixtures' omitted keys.
	// the rest read as undefined there and None here.
	fn make_task(id: &str, status: HarnessTaskStatus, title: &str) -> HarnessTask {
		HarnessTask {
			activations: None,
			created_at_iteration: 1,
			depends_on: None,
			footprint: None,
			dropped_exhausted: None,
			finished_at_iteration: None,
			id: id.into(),
			notes: vec![],
			reopen_count: None,
			review_of: None,
			review_round: None,
			role: None,
			stall_count: 0,
			status,
			summary: None,
			title: title.into(),
			verify_nudged: None,
			edit_nudged: None,
			confidence: None,
		}
	}

	// Shared test fixture. HarnessState::default() matches the fixture's
	// blank-slate state (version 1, empty ledger, iteration/loop 0) with an
	// empty createdAt, so set a fixed ISO timestamp like the fixtures do.
	// createdAt, so set a timestamp like now().toISOString() would.
	fn make_record() -> RunRecord {
		let mut state = HarnessState::default();
		state.created_at = "2026-07-09T10:00:00.000Z".into();
		state.goal = "build the thing".into();

		state
			.tasks
			.push(make_task("t1", HarnessTaskStatus::Completed, "one"));
		state
			.tasks
			.push(make_task("t2", HarnessTaskStatus::Pending, "two"));
		state.last_verification = Some(HarnessVerificationRecord {
			at_iteration: 2,
			command: "bun test".into(),
			failed: false,
			output_tail: "2 pass".into(),
			ran_no_tests: None,
            evidence: None,
            id: None,
		});

		build_run_record(&BuildRunRecordArgs {
			ended_at: "2026-07-09T12:00:00.000Z",
			goal: "build the thing",
			goal_id: "g-1",
			max_iterations: Some(10),
			pending_operator_messages: 1,
			result: &HarnessRunResult {
				continue_command: None,
				error_message: None,
				iterations: 4,
				r#loops: 2,
				leaked_jobs: None,
				reason: HarnessRunReason::MaxIterations,
				state,
				stop_latency_ms: None,
				usage: HarnessRunUsage {
					by_task: indexmap::IndexMap::new(),
					cache_creation_tokens: None,
					cache_read_tokens: None,
					calls: 2,
					completion_tokens: 40,
					prompt_tokens: 200,
					rate_limit_wait_seconds: 0.0,
					retries: 0,
					wall_ms: 1500,
				},
			},
		})
	}

	#[test]
	fn captures_the_outcome_fields_a_driver_needs() {
		let record = make_record();

		assert_eq!(
			record.task_stats,
			TaskStats {
				blocked: 0,
				completed: 1,
				dropped: 0,
				pending: 1,
			}
		);
		assert_eq!(
			record.last_verification,
			Some(VerificationSummary {
				at_iteration: 2,
				command: "bun test".into(),
				failed: false,
				mutations_after: 0,
				ran_no_tests: None,
				evidence: None,
				anchor: None,
			})
		);
		assert_eq!(record.goal, "build the thing");
		assert_eq!(record.max_iterations, Some(10));
		assert_eq!(record.pending_operator_messages, 1);
		assert_eq!(record.reason, "max-iterations");
		// No completion was anchored and nothing was unreconciled, so the
		// anchoring fields stay absent from the record (and its JSON).
		assert_eq!(record.completion_anchor, None);
		assert_eq!(record.anomalies, None);
		let json = serde_json::to_string(&record).unwrap();
		assert!(!json.contains("completionAnchor") && !json.contains("anomalies"), "{json}");
	}

	#[test]
	fn round_trips_through_save_load_without_leaving_a_temp_file() {
		let root = tempfile::TempDir::new().unwrap();
		let path = root.path().join("result.json");
		let record = make_record();

		save_run_record(&path, &record).unwrap();

		assert_eq!(load_run_record(&path), Some(record));
		let mut entries: Vec<String> = fs::read_dir(root.path())
			.unwrap()
			.map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
			.collect();
		entries.sort();
		assert_eq!(entries, vec!["result.json".to_string()]);
	}

	#[test]
	fn reads_a_missing_or_torn_record_as_null_instead_of_throwing() {
		let root = tempfile::TempDir::new().unwrap();
		let path = root.path().join("result.json");

		assert_eq!(load_run_record(&path), None);

		fs::write(&path, "{\"reason\": \"comp").unwrap();

		assert_eq!(load_run_record(&path), None);
		assert!(path.exists());
	}
}
