// Serde definitions for every type in the harness state contract. Field
// names serialize in camelCase: Rust fields are snake_case +
// #[serde(rename_all = "camelCase")], with explicit renames for reserved
// words (`type`, `loop`). Optional fields are Option<T> +
// skip_serializing_if = "Option::is_none" so missing keys are omitted from
// the JSON. String-literal unions become fieldless enums with explicit
// serde renames.
//
// Notes:
// - `version` is a plain u8, always serialized.
// - Seconds fields (waitSeconds / rateLimitWaitSeconds) are f64 so
//   fractional waits ("waiting 2.5s") parse. All other numbers are integers.
// - Record<string, T> maps are IndexMap (insertion-ordered, keeping JSON
//   key order stable for byte-identical state files), not HashMap.
#![allow(clippy::upper_case_acronyms)]

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

/// JSON.stringify prints an integral number without a fraction (`0`, not
/// `0.0`); serde_json prints every f64 with one. Seconds fields go through
/// this so the wire text stays in the JavaScript form.
pub fn serialize_js_number<S: serde::Serializer>(value: &f64, serializer: S) -> Result<S::Ok, S::Error> {
	if value.is_finite() && value.fract() == 0.0 && value.abs() < 9_007_199_254_740_992.0 {
		serializer.serialize_i64(*value as i64)
	} else {
		serializer.serialize_f64(*value)
	}
}

pub fn serialize_js_number_option<S: serde::Serializer>(value: &Option<f64>, serializer: S) -> Result<S::Ok, S::Error> {
	match value {
		Some(number) => serialize_js_number(number, serializer),
		None => serializer.serialize_none(),
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HarnessTaskStatus {
	#[serde(rename = "blocked")]
	Blocked,
	#[serde(rename = "completed")]
	Completed,
	#[serde(rename = "dropped")]
	Dropped,
	#[serde(rename = "in_progress")]
	InProgress,
	#[serde(rename = "pending")]
	Pending,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessTask {
	pub activations: Option<i64>,
	pub created_at_iteration: i64,
	/// Task ids that must reach a terminal state before this task is workable.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub depends_on: Option<Vec<String>>,
	/// Harness-recorded evidence trail (files patched, verifications run) handed to review tasks.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub footprint: Option<Vec<String>>,
	/// Set when the harness dropped this task after it exhausted its reopen budget (as opposed to the model pruning it as unnecessary).
	#[serde(skip_serializing_if = "Option::is_none")]
	pub dropped_exhausted: Option<bool>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub finished_at_iteration: Option<i64>,
	pub id: String,
	pub notes: Vec<String>,
	/// How many times stall recovery has force-reopened this task after it auto-blocked.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub reopen_count: Option<i64>,
	/// Id of the task this one reviews: set by the verify gate when a role's completed work needs confirmation by its verifiedBy role.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub review_of: Option<String>,
	/// How many review rejections this task has absorbed; at the cap a further rejection blocks it instead of reopening it.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub review_round: Option<i64>,
	/// Role (capability profile) whose loop works this task; unset tasks use the run's task binding.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub role: Option<String>,
	pub stall_count: i64,
	pub status: HarnessTaskStatus,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub summary: Option<String>,
	pub title: String,
	/// Set once the finish gate has bounced a completion for missing/stale/failed verification — the next attempt is accepted.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub verify_nudged: Option<bool>,
	/// The finish gate already bounced this task once for completing a build-shaped task without any workspace edit.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub edit_nudged: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessMemoryNote {
	pub created_at_iteration: i64,
	pub id: String,
	pub text: String,
}

/// Middle-term memory: findings that matter for the next few activations (a
/// failing test's cause, an in-flight hypothesis) without becoming permanent
/// memory. Each observation decays one ttl per activation and is dropped at
/// zero unless re-observed, which refreshes it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessObservation {
	pub created_at_iteration: i64,
	pub id: String,
	pub text: String,
	pub ttl: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolTelemetryRecord {
	pub call_count: i64,
	pub input_preview: String,
	/// Distinct task-loop indexes this call was reached for in (field name kept for state-file compatibility; unit is loops since the loop clock landed).
	pub iterations_used: Vec<i64>,
	pub key: String,
	/// True when the most recent execution of this call failed.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub last_failed: Option<bool>,
	pub last_output: String,
	/// Task-loop index of the most recent use (field name kept for state-file compatibility).
	pub last_used_iteration: i64,
	pub raw_input: String,
	pub reinforcements: i64,
	pub tool_name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromotedContextEntry {
	pub dynamic: bool,
	pub input_preview: String,
	pub key: String,
	/// True when the most recent execution of this call failed.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub last_failed: Option<bool>,
	pub output: String,
	/// Task-loop index of the promotion (field name kept for state-file compatibility).
	pub promoted_at_iteration: i64,
	pub raw_input: String,
	pub reinforcements: i64,
	pub tool_name: String,
	pub ttl: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessGoalRecord {
	pub archived_at_iteration: i64,
	pub goal: String,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub summary: Option<String>,
	pub tasks: Vec<HarnessTask>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessActivationDigest {
	pub actions: Vec<String>,
	/// How many cycles the loop that produced this digest ran.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub cycles: Option<i64>,
	pub iteration: i64,
	/// The task-loop index that produced this digest.
	#[serde(rename = "loop", skip_serializing_if = "Option::is_none")]
	pub r#loop: Option<i64>,
	pub outcome: String,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub task_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessRunSummaryNote {
	pub created_at_iteration: i64,
	pub reason: HarnessRunReason,
	pub text: String,
}

/// The most recent verification-shaped command this goal ran (tests, typecheck, build).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessVerificationRecord {
	pub at_iteration: i64,
	pub command: String,
	pub failed: bool,
	/// Ends-kept tail of the command output — enough to cite the actual counts.
	pub output_tail: String,
	/// Set when a test-shaped command exited green without executing a single
	/// test (e.g. `cargo test` printing only "running 0 tests"): a pass that
	/// proves nothing. Absent otherwise.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub ran_no_tests: Option<bool>,
}

/// The driver-facing view of the latest verification: the record fields plus
/// staleness (edits after the run). One shape shared by result.json, the
/// curated state summary, the headless payload, and DELEGATE's report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerificationSummary {
	pub at_iteration: i64,
	pub command: String,
	pub failed: bool,
	pub mutations_after: i64,
	/// Present (true) only when the run passed without executing any test.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub ran_no_tests: Option<bool>,
}

/// Task-ledger tally in the vocabulary every payload shares.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskStats {
	pub blocked: i64,
	pub completed: i64,
	pub dropped: i64,
	pub pending: i64,
}

/// Consecutive identical failures of one verification command — the signal
/// that edits are churning without affecting the failure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessVerificationStreak {
	pub command: String,
	pub consecutive_failures: i64,
	/// Hash of the failing output tail; a changed failure resets the streak.
	pub output_tail_hash: String,
}

/// A steering message injected into a running session (drip --send).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessOperatorMessage {
	pub id: String,
	pub received_at_iteration: i64,
	pub text: String,
}

/// A direct answer to a question-shaped goal, recorded by the respond op.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessDirectResponse {
	pub created_at_iteration: i64,
	pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessState {
	pub created_at: String,
	/// Set when the model answered the goal directly instead of planning tasks.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub direct_response: Option<HarnessDirectResponse>,
	pub goal: String,
	pub history: Vec<HarnessGoalRecord>,
	/// Count of inbox lines already consumed from the session's inbox.jsonl.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub inbox_cursor: Option<i64>,
	/// Recent operator steering messages, rendered to every activation prompt.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub operator_messages: Option<Vec<HarnessOperatorMessage>>,
	/// Harness-recorded outcome of the goal's most recent verification command.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub last_verification: Option<HarnessVerificationRecord>,
	/// Successful workspace mutations since the last verification ran — when > 0 the lastVerification result is stale.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub mutations_since_verification: Option<i64>,
	/// Successful workspace mutations over the whole run (never reset) — the edit gate's "did anything change at all" signal.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub workspace_edits: Option<i64>,
	/// Bounded timeline (newest last) of the goal's verification runs.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub verifications: Option<Vec<HarnessVerificationRecord>>,
	/// Live streak of identical verification failures; cleared by a pass or a changed failure.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub verification_streak: Option<HarnessVerificationStreak>,
	pub iteration: i64,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub last_activation: Option<HarnessActivationDigest>,
	/// Task-loop counter: one loop = the consecutive cycles a subagent spends on one task (or on planning) with a shared transcript.
	#[serde(rename = "loop")]
	pub r#loop: i64,
	pub memory: Vec<HarnessMemoryNote>,
	pub observations: Vec<HarnessObservation>,
	pub promoted_context: Vec<PromotedContextEntry>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub run_summary: Option<HarnessRunSummaryNote>,
	pub tasks: Vec<HarnessTask>,
	pub telemetry: IndexMap<String, ToolTelemetryRecord>,
	pub version: u8,
}

impl Default for HarnessState {
	fn default() -> Self {
		HarnessState {
			created_at: String::new(),
			direct_response: None,
			goal: String::new(),
			history: Vec::new(),
			inbox_cursor: None,
			operator_messages: None,
			last_verification: None,
			mutations_since_verification: None,
			workspace_edits: None,
			verifications: None,
			verification_streak: None,
			iteration: 0,
			last_activation: None,
			r#loop: 0,
			memory: Vec::new(),
			observations: Vec::new(),
			promoted_context: Vec::new(),
			run_summary: None,
			tasks: Vec::new(),
			telemetry: IndexMap::new(),
			version: 1,
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessTelemetryConfig {
	pub base_ttl: i64,
	pub max_observations: i64,
	pub max_observation_ttl: i64,
	pub max_promoted_entries: i64,
	pub max_promoted_output_chars: i64,
	pub max_ttl: i64,
	pub observation_base_ttl: i64,
	pub promote_threshold: i64,
	pub recency_window: i64,
	pub telemetry_retention: i64,
}

/// How a task loop spends its context window: a loop is a few short cycles
/// sharing one transcript. Recent tool results ride along verbatim ("hot");
/// older ones are folded to digests; every result is size-capped. When the
/// loop ends the transcript is discarded — only shared state survives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessLoopConfig {
	/// Most recent tool results kept verbatim in the loop transcript; older ones are folded to one-line digests.
	pub hot_tool_results: i64,
	/// Cycles a task loop may run before it yields the task back to the queue.
	pub max_cycles: i64,
	/// Per tool result character cap in the live loop transcript (ends kept).
	pub max_tool_result_chars: i64,
	/// Tool rounds within a single cycle.
	pub max_tool_rounds_per_cycle: i64,
}

pub const DEFAULT_LOOP_CONFIG: HarnessLoopConfig = HarnessLoopConfig {
	hot_tool_results: 6,
	max_cycles: 3,
	max_tool_result_chars: 8000,
	max_tool_rounds_per_cycle: 4,
};

pub const DEFAULT_TELEMETRY_CONFIG: HarnessTelemetryConfig = HarnessTelemetryConfig {
	base_ttl: 3,
	max_observations: 8,
	max_observation_ttl: 12,
	max_promoted_entries: 8,
	max_promoted_output_chars: 2000,
	max_ttl: 48,
	observation_base_ttl: 4,
	promote_threshold: 2,
	recency_window: 6,
	telemetry_retention: 50,
};

impl Default for HarnessTelemetryConfig {
	fn default() -> Self {
		DEFAULT_TELEMETRY_CONFIG
	}
}

impl Default for HarnessLoopConfig {
	fn default() -> Self {
		DEFAULT_LOOP_CONFIG
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HarnessEventType {
	#[serde(rename = "context-expired")]
	ContextExpired,
	#[serde(rename = "context-promoted")]
	ContextPromoted,
	#[serde(rename = "context-refreshed")]
	ContextRefreshed,
	#[serde(rename = "harness-op")]
	HarnessOp,
	#[serde(rename = "inference")]
	Inference,
	#[serde(rename = "iteration-start")]
	IterationStart,
	#[serde(rename = "loop-start")]
	LoopStart,
	#[serde(rename = "model-text")]
	ModelText,
	#[serde(rename = "operator-message")]
	OperatorMessage,
	#[serde(rename = "rate-limited")]
	RateLimited,
	#[serde(rename = "run-complete")]
	RunComplete,
	#[serde(rename = "run-summary")]
	RunSummary,
	#[serde(rename = "run-warning")]
	RunWarning,
	#[serde(rename = "stall-recovery")]
	StallRecovery,
	#[serde(rename = "task-finished")]
	TaskFinished,
	#[serde(rename = "tool-call")]
	ToolCall,
	#[serde(rename = "tool-result")]
	ToolResult,
}

/// Structured payload carried by events so consumers (transcript analytics,
/// eval assertions, UIs) never re-parse the prose in `detail`. Fields are
/// per-kind: tool events carry toolName/callId/failed/durationMs, rate-limit
/// events carry waitSeconds, operator-message events carry sentAt/latencyMs,
/// loop/task events carry loop/taskId.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessEventData {
	#[serde(skip_serializing_if = "Option::is_none")]
	pub call_id: Option<String>,
	/// "inference" events: prompt tokens written to the provider's cache this call.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub cache_creation_tokens: Option<i64>,
	/// "inference" events: prompt tokens served from the provider's cache this call (counted inside promptTokens).
	#[serde(skip_serializing_if = "Option::is_none")]
	pub cache_read_tokens: Option<i64>,
	/// "inference" events: completion tokens returned by this call.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub completion_tokens: Option<i64>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub cycle: Option<i64>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub duration_ms: Option<i64>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub failed: Option<bool>,
	/// Steering adoption latency: sentAt → consumed at a cycle boundary.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub latency_ms: Option<i64>,
	#[serde(rename = "loop", skip_serializing_if = "Option::is_none")]
	pub r#loop: Option<i64>,
	/// "inference" events: the model that served this individual call.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub model: Option<String>,
	/// "inference" events: prompt tokens sent on this call (cache reads included).
	#[serde(skip_serializing_if = "Option::is_none")]
	pub prompt_tokens: Option<i64>,
	/// "inference" events: the inference provider that served this individual call.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub provider: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub reason: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub sent_at: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub status: Option<String>,
	// toolName before taskId: NDJSON field order is the contract (taskId
	// last when both are present).
	#[serde(skip_serializing_if = "Option::is_none")]
	pub tool_name: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub task_id: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none", serialize_with = "serialize_js_number_option")]
	pub wait_seconds: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessEvent {
	#[serde(skip_serializing_if = "Option::is_none")]
	pub data: Option<HarnessEventData>,
	pub detail: String,
	pub iteration: i64,
	#[serde(rename = "type")]
	pub r#type: HarnessEventType,
}

/// "partial": every task reached a terminal state, but some were dropped along
/// the way — the goal was not fully accomplished and should not read as done.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HarnessRunReason {
	#[serde(rename = "aborted")]
	Aborted,
	#[serde(rename = "completed")]
	Completed,
	#[serde(rename = "error")]
	Error,
	#[serde(rename = "futile")]
	Futile,
	#[serde(rename = "max-iterations")]
	MaxIterations,
	#[serde(rename = "partial")]
	Partial,
	#[serde(rename = "planned")]
	Planned,
}

/// Per-call token economics attributed to one task (the byTask record value).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessUsageByTask {
	pub calls: i64,
	pub completion_tokens: i64,
	pub prompt_tokens: i64,
}

/// Per-run token/latency economics — what a delegating agent needs to decide
/// whether resuming is worth it and which task burned the budget.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessRunUsage {
	/// Prompt/completion tokens attributed to the task the model call worked.
	pub by_task: IndexMap<String, HarnessUsageByTask>,
	/// Prompt tokens written to the provider's cache (billed at that provider's write premium). Optional: absent on records from pre-caching versions.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub cache_creation_tokens: Option<i64>,
	/// Prompt tokens served from the provider's prompt cache (billed at the discounted cache-read rate). Counted inside promptTokens.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub cache_read_tokens: Option<i64>,
	pub calls: i64,
	pub completion_tokens: i64,
	pub prompt_tokens: i64,
	/// Seconds spent sleeping out 429s/5xx/network outages.
	#[serde(serialize_with = "serialize_js_number")]
	pub rate_limit_wait_seconds: f64,
	pub retries: i64,
	pub wall_ms: i64,
}

/// Background jobs (BASH_ASYNC tmux sessions) still alive at run end —
/// inline element type of HarnessRunResult.leakedJobs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessLeakedJob {
	pub command: String,
	pub kill_command: String,
	pub session_name: String,
	pub started_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessRunResult {
	/// Set when reason is "error": what killed the run (endpoint/network/harness).
	#[serde(skip_serializing_if = "Option::is_none")]
	pub error_message: Option<String>,
	/// Cycles run by this invocation (per-run, like loops).
	pub iterations: i64,
	/// Task loops run by this invocation.
	pub r#loops: i64,
	/// Background jobs (BASH_ASYNC tmux sessions) still alive at run end.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub leaked_jobs: Option<Vec<HarnessLeakedJob>>,
	pub reason: HarnessRunReason,
	pub state: HarnessState,
	/// Milliseconds from stop request (abort signal) to run end; only present when a stop was requested.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub stop_latency_ms: Option<i64>,
	pub usage: HarnessRunUsage,
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Optional fields must drop out of the JSON entirely (JSON.stringify
	/// drops `undefined`), and required ones must serialize camelCase.
	#[test]
	fn task_serializes_camel_case_and_drops_none_options() {
		let task = HarnessTask {
			activations: Some(2),
			created_at_iteration: 3,
			depends_on: Some(vec!["task-1".into()]),
			footprint: None,
			dropped_exhausted: None,
			finished_at_iteration: None,
			id: "task-2".into(),
			notes: vec![],
			reopen_count: None,
			review_of: None,
			review_round: None,
			role: None,
			stall_count: 0,
			status: HarnessTaskStatus::InProgress,
			summary: None,
			title: "Port types".into(),
			verify_nudged: None,
			edit_nudged: None,
		};
		let json = serde_json::to_value(&task).unwrap();
		let obj = json.as_object().unwrap();
		assert!(obj.contains_key("createdAtIteration"));
		assert!(obj.contains_key("in_progress") == false);
		assert_eq!(obj["status"], "in_progress");
		assert_eq!(obj["activations"], 2);
		// None options must be dropped, not null.
		assert!(!obj.contains_key("footprint"));
		assert!(!obj.contains_key("summary"));
		// Round-trip.
		let back: HarnessTask = serde_json::from_value(json).unwrap();
		assert_eq!(back, task);
	}

	/// String-literal unions must round-trip their exact spellings.
	#[test]
	fn union_enums_round_trip_exact_strings() {
		assert_eq!(
			serde_json::to_value(HarnessRunReason::MaxIterations).unwrap(),
			"max-iterations"
		);
		assert_eq!(
			serde_json::to_value(HarnessEventType::IterationStart).unwrap(),
			"iteration-start"
		);
		assert_eq!(
			serde_json::to_value(HarnessTaskStatus::Dropped).unwrap(),
			"dropped"
		);
		assert_eq!(
			serde_json::from_value::<HarnessRunReason>("futile".into()).unwrap(),
			HarnessRunReason::Futile
		);
	}

	/// The state contract: `type`/`loop` reserved-word renames, the version
	/// literal, and the default values.
	#[test]
	fn state_serializes_type_loop_and_version_one() {
		let state = HarnessState {
			goal: "ship the feature".into(),
			..Default::default()
		};
		let json = serde_json::to_value(&state).unwrap();
		let obj = json.as_object().unwrap();
		assert_eq!(obj["version"], 1);
		assert_eq!(obj["loop"], 0);
		assert!(obj.contains_key("type") == false);
		assert!(obj.contains_key("tasks"));
		assert!(obj.contains_key("telemetry"));

		let loop_config = DEFAULT_LOOP_CONFIG;
		let telemetry_config = DEFAULT_TELEMETRY_CONFIG;
		assert_eq!(loop_config.max_cycles, 3);
		assert_eq!(loop_config.hot_tool_results, 6);
		assert_eq!(loop_config.max_tool_result_chars, 8000);
		assert_eq!(loop_config.max_tool_rounds_per_cycle, 4);
		assert_eq!(telemetry_config.base_ttl, 3);
		assert_eq!(telemetry_config.max_observations, 8);
		assert_eq!(telemetry_config.max_observation_ttl, 12);
		assert_eq!(telemetry_config.max_promoted_entries, 8);
		assert_eq!(telemetry_config.max_promoted_output_chars, 2000);
		assert_eq!(telemetry_config.max_ttl, 48);
		assert_eq!(telemetry_config.observation_base_ttl, 4);
		assert_eq!(telemetry_config.promote_threshold, 2);
		assert_eq!(telemetry_config.recency_window, 6);
		assert_eq!(telemetry_config.telemetry_retention, 50);
	}

	/// Run-result shape: reserved-word `loops` field, skipped options, and
	/// nested state round-trip.
	#[test]
	fn run_result_round_trips() {
		let result = HarnessRunResult {
			error_message: None,
			iterations: 9,
			r#loops: 4,
			leaked_jobs: Some(vec![HarnessLeakedJob {
				command: "sleep 100".into(),
				kill_command: "tmux kill-session -t x".into(),
				session_name: "job-1".into(),
				started_at: "2026-01-01T00:00:00.000Z".into(),
			}]),
			reason: HarnessRunReason::Completed,
			state: HarnessState::default(),
			stop_latency_ms: None,
			usage: HarnessRunUsage {
				by_task: IndexMap::new(),
				cache_creation_tokens: Some(10),
				cache_read_tokens: None,
				calls: 3,
				completion_tokens: 100,
				prompt_tokens: 200,
				rate_limit_wait_seconds: 2.5,
				retries: 1,
				wall_ms: 4567,
			},
		};
		let json = serde_json::to_value(&result).unwrap();
		let obj = json.as_object().unwrap();
		assert!(obj.contains_key("loops"));
		assert!(!obj.contains_key("errorMessage"));
		assert!(!obj.contains_key("stopLatencyMs"));
		assert_eq!(obj["usage"]["rateLimitWaitSeconds"], 2.5);
		assert!(!obj["usage"].as_object().unwrap().contains_key("cacheReadTokens"));
		assert!(obj["usage"].as_object().unwrap().contains_key("cacheCreationTokens"));
		let back: HarnessRunResult = serde_json::from_value(json).unwrap();
		assert_eq!(back, result);
	}
}
