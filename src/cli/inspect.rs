// drip --inspect: answers "why did this run take 40 minutes and which tool
// kept failing" from the artifacts a session already persists — transcript
// events (with structured data since v0.33), state, and the run record —
// without pulling thousands of raw JSONL lines into a driver's context.
//
// Reused modules: run-record loading (crate::cli::run_record), transcript
// reading (crate::cli::transcript); the harness-state loader is inlined
// below — state_summary.rs carries the same parse with the loop-clock
// backfill, but only the verifications trail is consumed here.

use std::path::Path;

use indexmap::IndexMap;
use serde::Serialize;

use crate::cli::run_record::{load_run_record, RunRecord};
use crate::cli::transcript::{read_transcript, TranscriptEntry};
use crate::core::types::{HarnessEventType, HarnessRunUsage, HarnessState};

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InspectToolStat {
	pub calls: i64,
	pub failures: i64,
	pub total_duration_ms: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InspectOperatorMessages {
	pub adoption_latencies_ms: Vec<i64>,
	pub count: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InspectRateLimited {
	pub count: i64,
	#[serde(serialize_with = "crate::core::types::serialize_js_number")]
	pub total_wait_seconds: f64,
}

// Field order in the serialized --json object: endedAt, eventCounts, goal,
// goalId, operatorMessages, rateLimited, reason, startedAt, toolStats,
// wallSeconds — stable keys keep the output deterministic across runs.
// `reason` and `wallSeconds` are nullable: None serializes as null, never
// skipped.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InspectGoalReport {
	pub ended_at: Option<String>,
	pub event_counts: IndexMap<String, i64>,
	pub goal: String,
	pub goal_id: String,
	pub operator_messages: InspectOperatorMessages,
	pub rate_limited: InspectRateLimited,
	pub reason: Option<String>,
	pub started_at: String,
	pub tool_stats: IndexMap<String, InspectToolStat>,
	pub wall_seconds: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InspectLastRun {
	pub reason: String,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub stop_latency_ms: Option<i64>,
	// usage: None is skipped rather than serialized as null.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub usage: Option<HarnessRunUsage>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InspectVerification {
	pub at_iteration: i64,
	pub command: String,
	pub failed: bool,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub ran_no_tests: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InspectReport {
	pub goals: Vec<InspectGoalReport>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub last_run: Option<InspectLastRun>,
	pub verifications: Vec<InspectVerification>,
}

pub struct InspectPaths<'a> {
	pub result_path: &'a Path,
	pub state_path: &'a Path,
	pub transcript_path: &'a Path,
}

fn new_goal_report(goal_id: &str, goal: &str, started_at: &str) -> InspectGoalReport {
	InspectGoalReport {
		ended_at: None,
		event_counts: IndexMap::new(),
		goal: goal.to_string(),
		goal_id: goal_id.to_string(),
		operator_messages: InspectOperatorMessages::default(),
		rate_limited: InspectRateLimited::default(),
		reason: None,
		started_at: started_at.to_string(),
		tool_stats: IndexMap::new(),
		wall_seconds: None,
	}
}

fn event_kind_string(kind: HarnessEventType) -> String {
	serde_json::to_value(kind)
		.ok()
		.and_then(|v| v.as_str().map(str::to_string))
		.unwrap_or_default()
}

fn run_reason_string(reason: &serde_json::Value) -> String {
	reason.as_str().unwrap_or_default().to_string()
}

fn record_event(report: &mut InspectGoalReport, entry: &crate::cli::transcript::TranscriptEventEntry) {
	*report
		.event_counts
		.entry(event_kind_string(entry.kind))
		.or_insert(0) += 1;

	// Structured data when present (v0.33+); prose-prefix fallback for older
	// transcripts so --inspect still works on pre-upgrade sessions.
	if entry.kind == HarnessEventType::ToolCall || entry.kind == HarnessEventType::ToolResult {
		let tool_name = entry
			.data
			.as_ref()
			.and_then(|d| d.tool_name.clone())
			.unwrap_or_else(|| {
				entry
					.detail
					.split(|c: char| c.is_whitespace() || c == ':' || c == '(')
					.next()
					.unwrap_or("")
					.to_string()
			});

		if !tool_name.is_empty() && entry.kind == HarnessEventType::ToolResult {
			let stat = report
				.tool_stats
				.entry(tool_name.clone())
				.or_insert_with(Default::default);

			stat.calls += 1;
			let failed = entry
				.data
				.as_ref()
				.and_then(|d| d.failed)
				.unwrap_or_else(|| entry.detail.starts_with(&format!("{tool_name} (failed)")));
			stat.failures += if failed { 1 } else { 0 };
			stat.total_duration_ms += entry.data.as_ref().and_then(|d| d.duration_ms).unwrap_or(0);
		}
	}

	if entry.kind == HarnessEventType::RateLimited {
		report.rate_limited.count += 1;
		let wait = entry
			.data
			.as_ref()
			.and_then(|d| d.wait_seconds)
			.unwrap_or_else(|| {
				rx_waiting_seconds(&entry.detail).unwrap_or(0.0)
			});
		report.rate_limited.total_wait_seconds += wait;
	}

	if entry.kind == HarnessEventType::OperatorMessage {
		report.operator_messages.count += 1;

		if let Some(latency_ms) = entry.data.as_ref().and_then(|d| d.latency_ms) {
			report.operator_messages.adoption_latencies_ms.push(latency_ms);
		}
	}
}

// Extracts the seconds value from a "waiting Ns" detail suffix, as a float.
fn rx_waiting_seconds(detail: &str) -> Option<f64> {
	let idx = detail.find("waiting ")? + "waiting ".len();
	let rest = &detail[idx..];
	let end = rest
		.find(|c: char| !(c.is_ascii_digit() || c == '.'))
		.unwrap_or(rest.len());
	let token = &rest[..end];
	if token.is_empty() {
		return None;
	}
	token.parse::<f64>().ok()
}

// Date.parse + Number.isFinite: an unparseable timestamp yields no wall time.
fn parse_epoch_ms(ts: &str) -> Option<i64> {
	chrono::DateTime::parse_from_rfc3339(ts)
		.ok()
		.map(|dt| dt.timestamp_millis())
}

pub fn build_inspect_report(paths: &InspectPaths) -> InspectReport {
	let entries = read_transcript(paths.transcript_path);
	let mut goals: IndexMap<String, InspectGoalReport> = IndexMap::new();

	for entry in &entries {
		match entry {
			TranscriptEntry::Goal(goal) => {
				goals.insert(goal.goal_id.clone(), new_goal_report(&goal.goal_id, &goal.text, &goal.at));
			}
			TranscriptEntry::Event(event) => {
				if let Some(report) = goals.get_mut(&event.goal_id) {
					record_event(report, event);
				}
			}
			TranscriptEntry::RunEnd(run_end) => {
				if let Some(report) = goals.get_mut(&run_end.goal_id) {
					report.ended_at = Some(run_end.at.clone());
					report.reason = Some(run_reason_string(
						&serde_json::to_value(run_end.reason).unwrap_or_default(),
					));

					let wall_seconds = match (parse_epoch_ms(&report.started_at), parse_epoch_ms(&run_end.at)) {
						(Some(start_ms), Some(end_ms)) => Some(((end_ms - start_ms) as f64 / 1000.0).round().max(0.0) as i64),
						_ => None,
					};
					report.wall_seconds = wall_seconds;
				}
			}
			_ => {}
		}
	}

	let state = load_harness_state(paths.state_path);
	let record: Option<RunRecord> = load_run_record(paths.result_path);

	InspectReport {
		goals: goals.into_values().collect(),
		last_run: record.map(|record| InspectLastRun {
			reason: record.reason,
			stop_latency_ms: record.stop_latency_ms,
			usage: record.usage,
		}),
		verifications: state
			.and_then(|state| state.verifications)
			.unwrap_or_default()
			.iter()
			.map(|verification| InspectVerification {
				at_iteration: verification.at_iteration,
				command: verification.command.clone(),
				failed: verification.failed,
				ran_no_tests: verification.ran_no_tests.filter(|flag| *flag),
			})
			.collect(),
	}
}

pub fn format_inspect_report(report: &InspectReport) -> String {
	let mut lines: Vec<String> = Vec::new();

	for goal in &report.goals {
		lines.push(format!("goal {}: {}", &goal.goal_id.chars().take(8).collect::<String>(), goal.goal));
		lines.push(format!(
			"  {}{} — started {}",
			goal.reason.as_deref().unwrap_or("(no run-end recorded)"),
			goal.wall_seconds.map(|s| format!(" in {s}s")).unwrap_or_default(),
			goal.started_at
		));

		let tool_lines: Vec<String> = goal
			.tool_stats
			.iter()
			.map(|(name, stat)| {
				let duration = if stat.total_duration_ms > 0 {
					format!(", {}s in-tool", (stat.total_duration_ms as f64 / 1000.0).round() as i64)
				} else {
					String::new()
				};

				format!(
					"{name} ×{}{}{}",
					stat.calls,
					if stat.failures > 0 { format!(" ({} failed)", stat.failures) } else { String::new() },
					duration
				)
			})
			.collect();

		if !tool_lines.is_empty() {
			lines.push(format!("  tools: {}", tool_lines.join("  ")));
		}

		if goal.rate_limited.count > 0 {
			lines.push(format!(
				"  rate-limited {}× ({}s waiting)",
				goal.rate_limited.count,
				goal.rate_limited.total_wait_seconds.round() as i64
			));
		}

		if goal.operator_messages.count > 0 {
			let latencies = &goal.operator_messages.adoption_latencies_ms;
			let avg = if !latencies.is_empty() {
				let sum: i64 = latencies.iter().sum();
				format!(" (adoption avg {}s)", (sum as f64 / latencies.len() as f64 / 1000.0).round() as i64)
			} else {
				String::new()
			};

			lines.push(format!("  steering: {} message(s){avg}", goal.operator_messages.count));
		}

		lines.push(String::new());
	}

	if !report.verifications.is_empty() {
		lines.push("verifications (oldest first):".to_string());

		for verification in &report.verifications {
			lines.push(format!(
				"  [{}] cycle {}: {}",
				if verification.failed {
					"FAIL"
				} else if verification.ran_no_tests == Some(true) {
					"pass, 0 tests"
				} else {
					"pass"
				},
				verification.at_iteration,
				verification.command
			));
		}

		lines.push(String::new());
	}

	if let Some(last_run) = &report.last_run {
		let usage_line = last_run
			.usage
			.as_ref()
			.map(|usage| {
				format!(
					" — {} call(s), {} prompt + {} completion tokens",
					usage.calls, usage.prompt_tokens, usage.completion_tokens
				)
			})
			.unwrap_or_default();

		lines.push(format!(
			"last run: {}{}{usage_line}",
			last_run.reason,
			last_run
				.stop_latency_ms
				.map(|ms| format!(" (stop latency {ms}ms)"))
				.unwrap_or_default()
		));
	}

	let joined = lines.join("\n");
	let trimmed = joined.trim_end();
	if trimmed.is_empty() {
		"no transcript entries yet — run a goal first.".to_string()
	} else {
		trimmed.to_string()
	}
}

// ---------------------------------------------------------------------------
// load_harness_state
// Missing file -> None; a malformed file raises the harness-state error
// string verbatim. state_summary.rs carries the same parse with the loop
// clock backfill; inspect only consumes the verifications trail, so the
// shape guard + parse are all that is needed here.
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

fn load_harness_state(state_path: &Path) -> Option<HarnessState> {
	if !state_path.exists() {
		return None;
	}

	let content = match std::fs::read_to_string(state_path) {
		Ok(c) => c,
		// An unreadable file surfaces through the same error path as a malformed
		// one.
		Err(_) => panic!("The file at {} is not a valid harness state file.", state_path.display()),
	};

	let value: serde_json::Value = match serde_json::from_str(&content) {
		Ok(v) => v,
		Err(_) => panic!("The file at {} is not a valid harness state file.", state_path.display()),
	};

	if !is_harness_state(&value) {
		panic!("The file at {} is not a valid harness state file.", state_path.display());
	}

	serde_json::from_value(value).ok()
}

// --- fixture helpers and the four report scenarios ---
#[cfg(test)]
mod tests {
	use super::*;

	use crate::cli::run_record::{build_run_record, save_run_record, BuildRunRecordArgs};
	use crate::core::types::{
		HarnessRunReason, HarnessRunResult, HarnessRunUsage, HarnessVerificationRecord,
	};
	use std::path::PathBuf;
	use std::sync::atomic::{AtomicUsize, Ordering};

	static NEXT_TEMP_ID: AtomicUsize = AtomicUsize::new(0);

	fn make_temp_root(prefix: &str) -> PathBuf {
		let id = NEXT_TEMP_ID.fetch_add(1, Ordering::SeqCst);
		let root = std::env::temp_dir().join(format!(
			"{}{}-{}",
			prefix,
			std::process::id(),
			id
		));
		std::fs::create_dir_all(&root).unwrap();
		root
	}

	// Appends one event to the transcript file.
	fn write_entry(transcript_path: &Path, entry: serde_json::Value) {
		use std::io::Write;
		let mut file = std::fs::OpenOptions::new()
			.create(true)
			.append(true)
			.open(transcript_path)
			.unwrap();
		writeln!(file, "{}", entry).unwrap();
	}

	// Shared fixture: seven transcript entries, a state.json with a two-entry
	// verification trail, and a result.json built by build_run_record.
	fn make_fixture() -> (PathBuf, PathBuf, PathBuf) {
		let root = make_temp_root("drip-inspect-");
		let transcript_path = root.join("transcript.jsonl");
		let state_path = root.join("state.json");
		let result_path = root.join("result.json");

		write_entry(
			&transcript_path,
			serde_json::json!({
				"at": "2026-07-09T12:00:00.000Z", "goalId": "g1", "images": [],
				"mentions": [], "text": "build the thing", "type": "goal"
			}),
		);
		write_entry(
			&transcript_path,
			serde_json::json!({
				"at": "2026-07-09T12:00:01.000Z", "data": {"cycle": 1, "loop": 1},
				"detail": "cycle 1/3", "goalId": "g1", "iteration": 1,
				"kind": "iteration-start", "type": "event"
			}),
		);
		write_entry(
			&transcript_path,
			serde_json::json!({
				"at": "2026-07-09T12:00:02.000Z",
				"data": {"callId": "c1", "loop": 1, "taskId": "task-1", "toolName": "BASH"},
				"detail": "BASH {\"command\":\"bun test\"}", "goalId": "g1",
				"iteration": 1, "kind": "tool-call", "type": "event"
			}),
		);
		write_entry(
			&transcript_path,
			serde_json::json!({
				"at": "2026-07-09T12:00:04.000Z",
				"data": {"callId": "c1", "durationMs": 2100, "failed": true, "loop": 1,
					"taskId": "task-1", "toolName": "BASH"},
				"detail": "BASH (failed): 1 fail", "goalId": "g1", "iteration": 1,
				"kind": "tool-result", "type": "event"
			}),
		);
		write_entry(
			&transcript_path,
			serde_json::json!({
				"at": "2026-07-09T12:00:05.000Z", "data": {"waitSeconds": 8},
				"detail": "429 rate limited — waiting 8s before attempt 2/10",
				"goalId": "g1", "iteration": 1, "kind": "rate-limited", "type": "event"
			}),
		);
		write_entry(
			&transcript_path,
			serde_json::json!({
				"at": "2026-07-09T12:00:06.000Z",
				"data": {"latencyMs": 4000, "sentAt": "2026-07-09T12:00:02.000Z"},
				"detail": "change of plans", "goalId": "g1", "iteration": 2,
				"kind": "operator-message", "type": "event"
			}),
		);
		write_entry(
			&transcript_path,
			serde_json::json!({
				"at": "2026-07-09T12:05:00.000Z", "goalId": "g1", "iterations": 5,
				"reason": "completed", "type": "run-end"
			}),
		);

		// createHarnessState("build the thing") then the verification trail.
		// HarnessState::default() is the same shape (run_record.rs tests).
		let mut state = HarnessState::default();
		state.created_at = "2026-07-09T12:00:00.000Z".into();
		state.goal = "build the thing".into();
		state.verifications = Some(vec![
			HarnessVerificationRecord {
				at_iteration: 1,
				command: "bun test".into(),
				failed: true,
				output_tail: "1 fail".into(),
				ran_no_tests: None,
			},
			HarnessVerificationRecord {
				at_iteration: 4,
				command: "bun test".into(),
				failed: false,
				output_tail: "5 pass".into(),
				ran_no_tests: None,
			},
		]);
		std::fs::write(
			&state_path,
			serde_json::to_string_pretty(&state).unwrap(),
		)
		.unwrap();

		let record = build_run_record(&BuildRunRecordArgs {
			ended_at: "2026-07-09T12:05:00.000Z",
			goal: "build the thing",
			goal_id: "g1",
			max_iterations: None,
			pending_operator_messages: 0,
			result: &HarnessRunResult {
				error_message: None,
				iterations: 5,
				r#loops: 2,
				leaked_jobs: None,
				reason: HarnessRunReason::Completed,
				state,
				stop_latency_ms: Some(900),
				usage: HarnessRunUsage {
					by_task: indexmap::IndexMap::new(),
					cache_creation_tokens: None,
					cache_read_tokens: None,
					calls: 7,
					completion_tokens: 140,
					prompt_tokens: 700,
					rate_limit_wait_seconds: 8.0,
					retries: 1,
					wall_ms: 300000,
				},
			},
		});
		save_run_record(&result_path, &record).unwrap();

		(transcript_path, state_path, result_path)
	}

	fn fixture_paths(
		(transcript_path, state_path, result_path): &(PathBuf, PathBuf, PathBuf),
	) -> InspectPaths<'_> {
		InspectPaths {
			result_path,
			state_path,
			transcript_path,
		}
	}

	#[test]
	fn aggregates_per_goal_wall_time_tool_stats_waits_and_steering_latency() {
		let fixture = make_fixture();
		let report = build_inspect_report(&fixture_paths(&fixture));

		assert_eq!(report.goals.len(), 1);

		let goal = &report.goals[0];
		assert_eq!(goal.goal, "build the thing");
		assert_eq!(goal.reason.as_deref(), Some("completed"));
		assert_eq!(goal.wall_seconds, Some(300));
		let bash = goal.tool_stats.get("BASH").unwrap();
		assert_eq!(
			(bash.calls, bash.failures, bash.total_duration_ms),
			(1, 1, 2100)
		);
		assert_eq!(goal.rate_limited.count, 1);
		assert_eq!(goal.rate_limited.total_wait_seconds, 8.0);
		assert_eq!(goal.operator_messages.count, 1);
		assert_eq!(goal.operator_messages.adoption_latencies_ms, vec![4000]);
		assert_eq!(goal.event_counts.get("tool-call"), Some(&1));
	}

	#[test]
	fn carries_the_verification_timeline_and_last_run_economics() {
		let fixture = make_fixture();
		let report = build_inspect_report(&fixture_paths(&fixture));

		assert_eq!(report.verifications.len(), 2);
		let v0 = &report.verifications[0];
		assert_eq!(
			(v0.at_iteration, v0.command.as_str(), v0.failed),
			(1, "bun test", true)
		);
		let v1 = &report.verifications[1];
		assert_eq!(
			(v1.at_iteration, v1.command.as_str(), v1.failed),
			(4, "bun test", false)
		);
		let last_run = report.last_run.as_ref().unwrap();
		assert_eq!(last_run.reason, "completed");
		assert_eq!(last_run.stop_latency_ms, Some(900));
	}

	#[test]
	fn formats_a_readable_report() {
		let fixture = make_fixture();
		let text = format_inspect_report(&build_inspect_report(&fixture_paths(&fixture)));

		assert!(text.contains("build the thing"));
		assert!(text.contains("completed in 300s"));
		assert!(text.contains("BASH ×1 (1 failed)"));
		assert!(text.contains("rate-limited 1× (8s waiting)"));
		assert!(text.contains("steering: 1 message(s) (adoption avg 4s)"));
		assert!(text.contains("[FAIL] cycle 1: bun test"));
		assert!(text.contains("stop latency 900ms"));
	}

	#[test]
	fn reports_an_empty_session_gracefully() {
		let root = make_temp_root("drip-inspect-");
		let text = format_inspect_report(&build_inspect_report(&InspectPaths {
			result_path: &root.join("r.json"),
			state_path: &root.join("s.json"),
			transcript_path: &root.join("t.jsonl"),
		}));

		assert!(text.contains("no transcript entries yet"));
	}
}
