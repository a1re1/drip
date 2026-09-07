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
	/// The finished task's `confidence` self-report (low|medium|high), as
	/// supplied on the accepted finish_task call.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub confidence: Option<ClaimedConfidence>,
	/// Records that completion was refused for weak verification. Kept for
	/// session compatibility; it never exempts later completion attempts.
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

/// Where a verification check came from. `external` means the check compares
/// against something the agent did not author — a pre-existing project test, a
/// task-provided fixture, a published constant, or an invariant independent of
/// the implementation. `selfAuthored` means the check was derived from the
/// agent's own implementation and only demonstrates internal consistency.
/// `undeclared` is the omitted/legacy case: no provenance was declared.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum VerificationAnchorKind {
	External,
	SelfAuthored,
	Undeclared,
}

/// Provenance of one verification: the anchor kind plus where the check came
/// from (`source`) and, when a check that claimed external provenance turned
/// out to touch files the agent edited this session, why it was downgraded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerificationAnchor {
	pub kind: VerificationAnchorKind,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub source: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub downgraded_reason: Option<String>,
}

/// One recorded answer to a pre-registered expectation: what was observed at
/// some iteration, whether it matched the expected value, and optional
/// evidence (from outside the agent's own derivation) backing a revised value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessExpectationObservation {
	pub at_iteration: u64,
	pub observed: String,
	pub matches: bool,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub evidence: Option<String>,
}

/// A value expected from the work, registered before the result exists.
/// Expectations are immutable once written: revisions happen by appending
/// observations (with evidence when the observed value changes), never by
/// rewriting the expectation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessExpectation {
	pub id: String,
	pub subject: String,
	pub expected: String,
	pub registered_at_iteration: u64,
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub observations: Vec<HarnessExpectationObservation>,
}

/// A terminal unresolved mismatch: the run finished with at least one
/// expectation whose observed value did not match what was pre-registered.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessAnomaly {
	pub subject: String,
	pub expected: String,
	pub observed: String,
	pub note: String,
}

/// The agent's own claimed confidence in a finished task's reported values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ClaimedConfidence {
	Low,
	Medium,
	High,
}

/// How a task's completion is anchored. `external` requires at least one
/// passed verification whose check the agent did not author; `none` is the
/// explicit declaration that no external anchor exists, which must carry a
/// note explaining why.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletionAnchor {
	pub kind: CompletionAnchorKind,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub note: Option<String>,
	pub claimed_confidence: ClaimedConfidence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CompletionAnchorKind {
	External,
	#[serde(rename = "none")]
	None,
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
	/// Structured evidence for what the command actually executed. Legacy
	/// records without it deserialize fine and remain weak/unverified.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub evidence: Option<VerificationEvidence>,
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
	/// Structured evidence (see VerificationEvidence). Legacy summaries without
	/// it deserialize fine and remain weak.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub evidence: Option<VerificationEvidence>,
	/// How the latest check is anchored (external / self-authored / undeclared).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub anchor: Option<VerificationAnchor>,
}

/// What kind of evidence a verification command produced. `build` and
/// `typecheck` are legitimate evidence within their stated scope (the thing
/// compiles); they never claim tests or custom assertions ran. `unverified`
/// covers unknown exit-zero scripts, all-skipped/zero-check suites, malformed
/// custom markers, and legacy records — none of these confer success.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum VerificationEvidenceKind {
	Tests,
	Custom,
	Build,
	Typecheck,
	Unverified,
}

/// Reported assertion counts from a verification command. These are evidence
/// of what the command claimed/executed — never a proof of scientific
/// correctness. Invariants: all counts nonnegative and `executed == passed +
/// failed` for assertion evidence; a passing assertion verdict requires
/// `executed > 0`. Build/typecheck evidence carries zero assertion counts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerificationEvidence {
	pub kind: VerificationEvidenceKind,
	/// Assertions/tests the command reported executing (0 for build/typecheck).
	pub executed: i64,
	pub passed: i64,
	pub failed: i64,
	/// Skipped/ignored assertions, when the runner distinguishes them.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub skipped: Option<i64>,
	/// Why the evidence is weak/unverified, when it is.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub detail: Option<String>,
	/// Where the check came from. Absent (legacy) means undeclared.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub anchor: Option<VerificationAnchor>,
}

impl VerificationEvidence {
	pub fn verifies_work(&self) -> bool {
		if self.failed != 0 || self.passed < 0 || self.executed < 0 || self.skipped.is_some_and(|count| count < 0) {
			return false;
		}
		match self.kind {
			VerificationEvidenceKind::Tests | VerificationEvidenceKind::Custom =>
				self.executed > 0 && self.passed.checked_add(self.failed) == Some(self.executed),
			VerificationEvidenceKind::Build | VerificationEvidenceKind::Typecheck => self.executed == 0 && self.passed == 0,
			VerificationEvidenceKind::Unverified => false,
		}
	}
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
	/// Workspace mutations over the whole run (never reset), conservatively
	/// including failed mutating calls whose side effects are unknown.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub workspace_edits: Option<i64>,
	/// Bounded timeline (newest last) of the goal's verification runs.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub verifications: Option<Vec<HarnessVerificationRecord>>,
	/// Live streak of identical verification failures; cleared by a pass or a changed failure.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub verification_streak: Option<HarnessVerificationStreak>,
	/// Operator disabled the review/verify lane for this run: set explicitly
	/// by --no-review/--lite or auto-detected from opt-out phrases in the
	/// goal/operator messages. Sticky: persisted and restored with the state.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub review_opt_out: Option<bool>,
	/// First matched opt-out phrase; keeps the run-warning one-shot per run.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub opt_out_warning_emitted: Option<String>,
	/// Values expected from the work, registered before results exist. Immutable
	/// once written; revisions append observations instead of rewriting these.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub expectations: Vec<HarnessExpectation>,
	/// Terminal unresolved mismatches recorded by finishes with status
	/// `unreconciled`.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub anomalies: Vec<HarnessAnomaly>,
	/// How the most recent task completion was anchored.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub completion_anchor: Option<CompletionAnchor>,
	/// Workspace paths the run has edited (PATCH targets), deduplicated. An
	/// "external" verification anchor that names one of these is downgraded
	/// to self-authored: a check the agent wrote is consistency, not
	/// correctness.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub edited_paths: Vec<String>,
	/// Survey awaiting operator answers (an ask_user timeout or run end while
	/// the question was open) — resumed runs re-emit it instead of losing it.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub pending_questions: Option<QuestionSurvey>,
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
			review_opt_out: None,
			opt_out_warning_emitted: None,
			expectations: Vec::new(),
			anomalies: Vec::new(),
			completion_anchor: None,
			edited_paths: Vec::new(),
			pending_questions: None,
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
	/// A blind role's loop started without the previous loop's tool exchanges.
	#[serde(rename = "context-withheld")]
	ContextWithheld,
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
	#[serde(rename = "question")]
	Question,
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

/// One staged multiple-choice clarification question the model asks the
/// operator (the ask_user harness tool's survey payload).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessSurveyOption {
	/// Short choice label — what answers.jsonl records back as `choice`.
	pub label: String,
	/// One-line explanation of what choosing this option means.
	pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessSurveyQuestion {
	/// Short label shown above the question.
	pub header: String,
	/// The question itself.
	pub question: String,
	/// 2-4 choices; the model's best guess goes first.
	pub options: Vec<HarnessSurveyOption>,
	/// When true (the default) the operator may answer with free text instead of a listed option.
	#[serde(default = "default_allow_other")]
	pub allow_other: bool,
}

fn default_allow_other() -> bool {
	true
}

/// The full survey carried by a question event and by
/// HarnessState.pendingQuestions while the run waits for answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionSurvey {
	pub questions: Vec<HarnessSurveyQuestion>,
	/// 0-based next-unconsumed answers.jsonl line (stale-line replay guard).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub answers_cursor: Option<usize>,
}

/// One recorded answer to the question at `index` (0-based within the survey).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessSurveyAnswer {
	pub index: i64,
	/// Chosen option label, or null when the operator answered with free text.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub choice: Option<String>,
	/// Free-text answer (the "Other..." path), or null when a listed option was chosen.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub other: Option<String>,
}

/// One answers.jsonl record: a batch of answers plus when they arrived.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessSurveyAnswers {
	pub at: String,
	pub answers: Vec<HarnessSurveyAnswer>,
}

impl HarnessSurveyAnswers {
	/// RFC3339 UTC timestamp with millisecond precision — drip's transcript convention.
	pub fn now_iso() -> String {
		chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
	}
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
	// question events carry the survey; survey-answer harness-ops carry the
	// recorded batch. NDJSON field order is the contract: surveyAnswers
	// before questionSurvey, like toolName before taskId.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub survey_answers: Option<HarnessSurveyAnswers>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub question_survey: Option<QuestionSurvey>,
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
	#[serde(rename = "awaiting-input")]
	AwaitingInput,
	#[serde(rename = "aborted")]
	Aborted,
	#[serde(rename = "completed")]
	Completed,
	/// Draft mode (--lite): every task finished, but the run is a draft for
	/// the operator to review and harden rather than a shipped result.
	#[serde(rename = "draft")]
	Draft,
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
	#[serde(rename = "unreconciled")]
	Unreconciled,
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
	/// Set when the run ended awaiting operator input (drip --resume picks it up).
	#[serde(skip_serializing_if = "Option::is_none")]
	pub continue_command: Option<String>,
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
			confidence: None,
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
			serde_json::to_value(HarnessEventType::Question).unwrap(),
			"question"
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
			continue_command: None,
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

	// Anchor/expectation/confidence vocabulary: serialized spellings are part
	// of the persisted state contract.
	#[test]
	fn anchor_and_confidence_enums_serialize_with_requested_spellings() {
		use crate::core::types::{ClaimedConfidence, CompletionAnchorKind, HarnessRunReason, VerificationAnchorKind};
		assert_eq!(serde_json::to_value(VerificationAnchorKind::External).unwrap(), "external");
		assert_eq!(serde_json::to_value(VerificationAnchorKind::SelfAuthored).unwrap(), "selfAuthored");
		assert_eq!(serde_json::to_value(VerificationAnchorKind::Undeclared).unwrap(), "undeclared");
		assert_eq!(serde_json::to_value(ClaimedConfidence::Low).unwrap(), "low");
		assert_eq!(serde_json::to_value(ClaimedConfidence::Medium).unwrap(), "medium");
		assert_eq!(serde_json::to_value(ClaimedConfidence::High).unwrap(), "high");
		assert_eq!(serde_json::to_value(CompletionAnchorKind::External).unwrap(), "external");
		assert_eq!(serde_json::to_value(CompletionAnchorKind::None).unwrap(), "none");
		assert_eq!(serde_json::to_value(HarnessRunReason::Unreconciled).unwrap(), "unreconciled");
	}

	// Optional anchor fields drop out of the JSON when absent and roundtrip
	// when present.
	#[test]
	fn verification_anchor_roundtrips_and_omits_absent_optionals() {
		let full = VerificationAnchor {
			kind: VerificationAnchorKind::SelfAuthored,
			source: Some("repo test suite".into()),
			downgraded_reason: Some("command names edited file src/lib.rs".into()),
		};
		let json = serde_json::to_value(&full).unwrap();
		assert_eq!(json["kind"], "selfAuthored");
		assert_eq!(json["source"], "repo test suite");
		assert_eq!(json["downgradedReason"], "command names edited file src/lib.rs");
		assert_eq!(serde_json::from_value::<VerificationAnchor>(json).unwrap(), full);

		let bare = VerificationAnchor {
			kind: VerificationAnchorKind::External,
			source: None,
			downgraded_reason: None,
		};
		let bare_json = serde_json::to_value(&bare).unwrap();
		let bare_obj = bare_json.as_object().unwrap();
		assert!(!bare_obj.contains_key("source"));
		assert!(!bare_obj.contains_key("downgradedReason"));
	}

	// Evidence written before anchors existed (no anchor key) still loads as
	// the undeclared case.
	#[test]
	fn legacy_verification_evidence_without_anchor_deserializes_undeclared() {
		let legacy = serde_json::json!({
			"kind": "tests",
			"executed": 3,
			"passed": 3,
			"failed": 0,
		});
		let evidence: VerificationEvidence = serde_json::from_value(legacy).unwrap();
		assert_eq!(evidence.anchor, None);
		assert!(evidence.verifies_work());
	}
}
