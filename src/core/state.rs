// port of src/harness/state.ts
//
// Harness state helpers: create/load/save, the task ledger, memory notes,
// observations, goal history, and the shared derivations (verification
// summary, task stats). Function names are the TS names in snake_case so the
// two files can be diffed side by side.
//
// Port notes:
// - `loadHarnessState` validates with the TS shape guards (ported literally
//   over serde_json::Value) so the error string matches byte for byte; only
//   after the guard does it deserialize into the typed structs.
// - Mutating helpers that return the affected task in TS return Option<&mut>
   // or a clone here; the mutation happens in place either way.
// - reopen_blocked_tasks' TS default maxReopens = Number.POSITIVE_INFINITY is
//   Option::None here.

use std::path::Path;

use anyhow::bail;
use chrono::{SecondsFormat, Utc};
use indexmap::IndexMap;
use serde_json::{json, Value};

use crate::core::types::{
	HarnessActivationDigest, HarnessDirectResponse, HarnessGoalRecord,
HarnessMemoryNote, HarnessObservation, HarnessOperatorMessage, HarnessRunSummaryNote, HarnessState,
HarnessTask, HarnessTaskStatus, HarnessTelemetryConfig, HarnessVerificationRecord, TaskStats,
VerificationSummary,
};
use crate::lib_fs::write_file_atomic;

/// port of `export type HarnessTaskPlacement = "end" | "next";`
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HarnessTaskPlacement {
	End,
	Next,
}

pub type HarnessTaskPlacementAlias = HarnessTaskPlacement;

pub fn create_harness_state(goal: &str) -> HarnessState {
	HarnessState {
		created_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
		direct_response: None,
		goal: goal.to_string(),
		history: Vec::new(),
		inbox_cursor: None,
		operator_messages: None,
		last_verification: None,
		mutations_since_verification: None,
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

pub fn start_follow_up_goal(state: &mut HarnessState, goal: &str) {
	if !state.tasks.is_empty() {
		state.history.push(HarnessGoalRecord {
			archived_at_iteration: state.iteration,
			goal: state.goal.clone(),
			summary: state.run_summary.as_ref().map(|note| note.text.clone()),
			tasks: std::mem::take(&mut state.tasks),
		});
	}

	state.goal = goal.to_string();
	state.tasks = Vec::new();
	state.last_activation = None;
	state.run_summary = None;
	state.direct_response = None;
	// A new goal must not inherit the previous goal's test outcome as "current"
	// — nor its verification timeline, failure streak, or staleness counter
	// (debt audit B4: the v0.30 fields missed this list and leaked).
	state.last_verification = None;
	state.verifications = None;
	state.verification_streak
 = None;
	state.mutations_since_verification
 = None;
	// Steering consumed during the previous goal was steering FOR that goal;
	// a new goal's text is the operator's latest word.
	state.operator_messages = None;
}

pub fn has_unfinished_tasks(state: &HarnessState) -> bool {
	state.tasks.iter().any(|task| task.status != HarnessTaskStatus::Completed && task.status != HarnessTaskStatus::Dropped)
}

fn next_sequence_id(prefix: &str, existing_ids: &[String]) -> String {
	let needle = format!("{}-", prefix);
	let mut highest_sequence: i64 = 0;

	for existing_id in existing_ids {
		if let Some(rest) = existing_id.strip_prefix(needle.as_str()) {
			// Mirrors the TS regex `^{prefix}-(\d+)$`.
			if !rest.is_empty() && rest.bytes().all(|byte| byte.is_ascii_digit()) {
				if let Ok(sequence) = rest.parse::<i64>() {
					highest_sequence = highest_sequence.max(sequence);
				}
			}
		}
	}

	format!("{}-{}", prefix, highest_sequence + 1)
}

pub struct HarnessTaskInput {
	/// Ids of tasks that must finish (complete/drop) before this one runs.
	pub depends_on: Option<Vec<String>>,
	/// Task this one reviews (verify-gate bookkeeping).
	pub review_of: Option<String>,
	/// Role whose loop should work this task.
	pub role: Option<String>,
	pub title: String,
}

impl From<&str> for HarnessTaskInput {
	fn from(title: &str) -> Self {
		HarnessTaskInput { depends_on: None, review_of: None, role: None, title: title.to_string() }
	}
}

impl From<String> for HarnessTaskInput {
	fn from(title: String) -> Self {
		HarnessTaskInput { depends_on: None, review_of: None, role: None, title }
	}
}

pub fn add_tasks(
	state: &mut HarnessState,
	entries: Vec<HarnessTaskInput>,
	placement: HarnessTaskPlacement,
) -> Vec<HarnessTask> {
	let mut added_tasks: Vec<HarnessTask> = Vec::new();

	for entry in entries {
		let trimmed_title = entry.title.trim();

		if trimmed_title.is_empty() {
			continue;
		}

		let depends_on = entry
			.depends_on
			.filter(|ids| !ids.is_empty())
			.map(|ids| ids.clone());
		let depends_on_ref = depends_on.as_deref();

		let existing_ids: Vec<String> = state
			.tasks
			.iter()
			.chain(added_tasks.iter())
			.map(|task| task.id.clone())
			.collect();

		added_tasks.push(HarnessTask {
			activations: Some(0),
			created_at_iteration: state.iteration,
			depends_on: depends_on_ref.map(|ids| ids.to_vec()),
			footprint: None,
			dropped_exhausted: None,
			finished_at_iteration: None,
			id: next_sequence_id("task", &existing_ids),
			notes: Vec::new(),
			reopen_count: None,
			review_of: entry.review_of.clone(),
			review_round: None,
			role: entry.role.clone(),
			stall_count: 0,
			status: HarnessTaskStatus::Pending,
			summary: None,
			title: trimmed_title.to_string(),
			verify_nudged: None,
			edit_nudged: None,
		});
	}

	if placement == HarnessTaskPlacement::Next {
		// Newly discovered prerequisite work slots in right after the current task,
		// ahead of the rest of the pending queue.
		let current_index = state.tasks.iter().position(|task| task.status == HarnessTaskStatus::InProgress);
		let first_pending_index = state.tasks.iter().position(|task| task.status == HarnessTaskStatus::Pending);
		let insert_index = match (current_index, first_pending_index) {
			(Some(index), _) => index + 1,
			(None, Some(index)) => index,
			(None, None) => state.tasks.len(),
		};

		let mut tail = state.tasks.split_off(insert_index);
		state.tasks.extend(added_tasks.iter().cloned());
		state.tasks.append(&mut tail);
	} else {
		state.tasks.extend(added_tasks.iter().cloned());
	}

	added_tasks
}

pub fn get_task_by_id<'a>(state: &'a HarnessState, task_id: &str) -> Option<&'a HarnessTask> {
	state.tasks.iter().find(|task| task.id == task_id)
}

pub fn get_task_by_id_mut<'a>(state: &'a mut HarnessState, task_id: &str) -> Option<&'a mut HarnessTask> {
	state.tasks.iter_mut().find(|task| task.id == task_id)
}

// A dependency is met once the task it names is terminal (completed or
// dropped) — or never existed, so a mistyped id cannot deadlock the plan.
fn has_unmet_dependencies_in(tasks: &[HarnessTask], task: &HarnessTask) -> bool {
	(task.depends_on.as_deref().unwrap_or(&[])).iter().any(|dependency_id| {
		tasks
			.iter()
			.find(|candidate| &candidate.id == dependency_id)
			.map_or(false, |dependency| {
				dependency.status != HarnessTaskStatus::Completed && dependency.status != HarnessTaskStatus::Dropped
			})
	})
}

pub fn has_unmet_dependencies(state: &HarnessState, task: &HarnessTask) -> bool {
	has_unmet_dependencies_in(&state.tasks, task)
}

pub fn get_current_task(state: &HarnessState) -> Option<&HarnessTask> {
	state
		.tasks
		.iter()
		.find(|task| task.status == HarnessTaskStatus::InProgress)
		.or_else(|| {
			state
				.tasks
				.iter()
				.find(|task| task.status == HarnessTaskStatus::Pending && !has_unmet_dependencies_in(&state.tasks, task))
		})
}

pub fn get_current_task_mut
(state: &mut HarnessState) -> Option<&mut HarnessTask> {
	if state.tasks.iter().any(|task| task.status == HarnessTaskStatus::InProgress) {
		return state
			.tasks
			.iter_mut()
			.find(|task| task.status == HarnessTaskStatus::InProgress);
	}

	let ready_ids: Vec<String> = state
		.tasks
		.iter()
		.filter(|task| task.status == HarnessTaskStatus::Pending && !has_unmet_dependencies_in(&state.tasks, task))
		.map(|task| task.id.clone())
		.collect();

	state
		.tasks
		.iter_mut()
		.find(|task| task.status == HarnessTaskStatus::Pending && ready_ids.contains(&task.id))
}

pub struct HarnessFinishArgs<'a> {
	pub status: HarnessTaskStatus,
	pub summary: &'a str,
	pub task_id: Option<&'a str>,
}

pub fn finish_task<'a>(state: &'a mut HarnessState, args: HarnessFinishArgs<'_>) -> Option<&'a mut HarnessTask> {
	let iteration = state.iteration;
	let task = match args.task_id {
		Some(task_id) => get_task_by_id_mut(state, task_id),
		None => get_current_task_mut(state),
	};

	let task = task?;

	task.finished_at_iteration = Some(iteration);
	task.status = args.status;
	let trimmed = args.summary.trim();
	task.summary = if trimmed.is_empty() { None } else { Some(trimmed.to_string()) };

	Some(task)
}

pub fn drop_task<'a>(state: &'a mut HarnessState, task_id: &str, reason: &str) -> Option<&'a mut HarnessTask> {
	let iteration = state.iteration;
	{
		let task = get_task_by_id(state, task_id)?;

		if task.status == HarnessTaskStatus::Completed || task.status == HarnessTaskStatus::Dropped {
			return None;
		}
	}

	let task = get_task_by_id_mut(state, task_id)?;
	task.finished_at_iteration = Some(iteration);
	task.status = HarnessTaskStatus::Dropped;
	let trimmed = reason.trim();
	task.summary = Some(if trimmed.is_empty() { "Dropped without a reason.".to_string() } else { trimmed.to_string() });

	Some(task)
}

pub fn revise_task<'a>(state: &'a mut HarnessState, task_id: &str, title: &str) -> Option<&'a mut HarnessTask> {
	let trimmed_title = title.trim();

	{
		let task = get_task_by_id(state, task_id)?;

		if task.status == HarnessTaskStatus::Completed
			|| task
				.status == HarnessTaskStatus::Dropped
			|| trimmed_title.is_empty()
		{
			return None;
		}
	}

	let task = get_task_by_id_mut(state, task_id)?;

	if trimmed_title != task.title {
		task.notes.push(format!("Retitled from \"{}\".", task.title));
		task.title = 
trimmed_title.to_string();
	}

	Some(task)
}

// Sends a reviewed task back to the queue after its reviewer rejected the
// work: pending again with a fresh stall budget, carrying the reviewer's
// findings as a note, and one review round closer to being blocked for good.
pub fn reopen_task_for_rework(task: &mut HarnessTask, note: &str) {
	task.review_round = Some(task.review_round.unwrap_or(0) + 1);
	task.status = HarnessTaskStatus::Pending;
	task.stall_count = 0;
	task.finished_at_iteration = None;
	task.summary = None;
	append_task_note(task, note);
}

pub fn reopen_blocked_tasks(
	state: &mut HarnessState,
	note: &str,
	max_reopens: Option<i64>,
) -> Vec<HarnessTask> {
	let mut reopened: Vec<HarnessTask> = Vec::new();

	for task in state.tasks.iter_mut() {
		if task.status != HarnessTaskStatus::Blocked {
			continue;
		}

		// A task that has already burned its reopen budget stays blocked: reopening
		// it again would replay the same stall loop instead of forcing an escalation.
		if let Some(max_reopens) = max_reopens {
			if task.reopen_count.unwrap_or(0) >= max_reopens {
				continue;
			}
		}

		task.reopen_count = Some(task.reopen_count.unwrap_or(0) + 1);
		task.status = HarnessTaskStatus::Pending;
		task.stall_count = 0;
		task.finished_at_iteration = None;
		append_task_note(task, note);
		reopened.push(task.clone());
	}

	reopened
}

// Escalation when every blocked task has exhausted its reopen budget: drop them
// with an honest summary so the run ends (or replans from scratch) instead of
// spinning through identical stall cycles forever.
pub fn drop_exhausted_blocked_tasks(state: &mut HarnessState) -> Vec<HarnessTask> {
	let mut dropped: Vec<HarnessTask> = Vec::new();

	for task in state.tasks.iter_mut() {
		if task.status != HarnessTaskStatus::Blocked {
			continue;
		}

		task.dropped_exhausted = Some(true);
		task.finished_at_iteration = Some(state.iteration);
		task.status = HarnessTaskStatus::Dropped;
		task.summary = Some(format!(
			"Dropped after {} reopen cycle(s) without recorded progress — this task needs a different approach or user input.",
			task.reopen_count.unwrap_or(0)
		));
		dropped
.push(task.clone());
	}

	dropped
}

pub fn append_task_note(task: &mut HarnessTask, note: &str) {
	let trimmed_note = note.trim();

	if !trimmed_note.is_empty() {
		task.notes.push(trimmed_note.to_string());
	}
}

pub fn add_memory_note(state: &mut HarnessState, text: &str) -> Option<HarnessMemoryNote> {
	let trimmed_text = text.trim();

	if trimmed_text.is_empty() {
		return None;
	}

	let existing_ids: Vec<String> = state.memory.iter().map(|note| note.id.clone()).collect();
	let note = HarnessMemoryNote {
		created_at_iteration: state.iteration,
		id: next_sequence_id("note", &existing_ids),
		text: trimmed_text.to_string(),
	};

	state.memory.push(note.clone());

	Some(note)
}

// Saves (or refreshes) a middle-term observation. Re-observing text that is
// already stored refreshes its ttl instead of duplicating the entry, so the
// model can keep a finding alive across activations while it still matters.
pub struct AddObservationArgs {
	pub text: String,
	pub ttl: Option<i64>,
}

pub fn add_observation(
	state: &mut HarnessState,
	args: AddObservationArgs,
	config: &HarnessTelemetryConfig,
) -> Option<(HarnessObservation, bool)> {
	let trimmed_text = args.text.trim();

	if trimmed_text.is_empty() {
		return None;
	}

	let requested_ttl = args.ttl.unwrap_or(config.observation_base_ttl);
	let ttl = config.max_observation_ttl.min(1.max(requested_ttl));
	if let Some(existing) = state.observations.iter_mut().find(|observation| observation.text == trimmed_text) {
		existing.ttl = existing.ttl.max(ttl);

		return Some((existing.clone(), true));
	}

	let observation = HarnessObservation {
		created_at_iteration: state.iteration,
		id: {
			let existing_ids: Vec<String> = state.observations.iter().map(|entry| entry.id.clone()).collect();
			next_sequence_id("obs", &existing_ids)
		},
		text: trimmed_text.to_string(),
		ttl,
	};

	state.observations.push(observation.clone());
	let new_index = state.observations.len() - 1;

	// The observation store is bounded: when full, the entry closest to expiry
	// makes room for the new one.
	if state.observations.len() as i64 > config.max_observations {
		let mut evictable: Option<usize> = None;
		for (index, entry) in state.observations.iter().enumerate() {
			if index == new_index {
				continue;
			}
			match evictable {
				None => evictable = Some(index),
				Some(best) => {
					if entry.ttl < state.observations[best].ttl {
						evictable = Some(index);
					}
				}
			}
		}

		if let Some(index) = evictable {
			state.observations.remove(index);
		}
	}

	Some((observation, false))
}

// Called once per task loop: observations decay toward expiry unless re-observed.
pub fn decay_observations(state: &mut HarnessState, fresh_after_iteration: Option<i64>) -> Vec<HarnessObservation> {
	let fresh_after = fresh_after_iteration.unwrap_or(state.iteration - 1);
	let mut expired: Vec<HarnessObservation> = Vec::new();
	let ids: Vec<String> = state.observations.iter().map(|observation| observation.id.clone()).collect();

	for id in ids {
		let Some(index) = state.observations.iter().position(|observation| observation.id == id) else {
			continue;
		};
		let observation = &mut state.observations[index];

		// An observation saved during the loop that is ending keeps its full ttl:
		// decay starts with the first loop that actually had a chance to read it.
		if observation.created_at_iteration > fresh_after {
			continue;
		}

		observation.ttl -= 1;

		if observation.ttl <= 0 {
			let expired_observation = state.observations.remove(index);
			expired.push(expired_observation);
		}
	}

	expired
}

pub fn 
remove_memory_note(state: &mut HarnessState, note_id: &str) -> bool {
	let Some(note_index) = state.memory.iter().position(|note| note.id == note_id) else {
		return false;
	};

	state.memory.remove(note_index);

	true
}

pub fn is_goal_complete(state: &HarnessState) -> bool {
	// Dropped tasks do not block completion, but a goal where everything was
	// dropped and nothing completed is not "done" — it was abandoned.
	!state.tasks.is_empty()
		&& state
			.tasks
			.iter()
			.all(|task| task.status == HarnessTaskStatus::Completed || task.status == HarnessTaskStatus::Dropped)
		&& state.tasks.iter().any(|task| task.status == HarnessTaskStatus::Completed)
}

fn is_object(value: Option<&Value>) -> bool {
	value.map_or(false, |value| value.is_object())
}

fn is_string_field(value: Option<&Value>) -> bool {
	value.map_or(false, Value::is_string)
}

fn is_finite_number(value: Option<&Value>) -> bool {
	value.map_or(false
, |value| value.as_f64().map_or(false, |number| number.is_finite()))
}

fn is_array_field(value: Option<&
Value>) 
-> bool {
	value.map_or(false, Value::is_array)
}

fn every_is_string(value: Option<&Value>) -> bool {
	value.map_or(false, |value| {
		value.as_array().map_or(false, |items| items.iter().all(|item| item.is_string()))
	})
}

fn is_harness_task_shape(value: &Value) -> bool {
	is_object(Some(value))
		&& is_string_field(value.get("id"))
		&& is_string_field(value.get("title"))
		&& is_string_field(value.get("status"))
		&& is_array_field(value.get("notes"))
}

fn is_goal_record_shape(value: &Value) -> bool {
	is_object(Some(value))
		&& is_string_field(value.get("goal"))
		&& is_array_field(value.get("tasks"))
		&& value
			.get("tasks")
			.map_or(false, |tasks| tasks.as_array().map_or(false, |tasks| tasks.iter().all(is_harness_task_shape)))
}

fn is_memory_note_shape(value: &Value) -> bool {
	is_object(Some(value)) && is_string_field(value.get("id")) && is_string_field(value.get("text"))
}

fn is_promoted_entry_shape(value: &Value) -> bool {
	is_object(Some(value))
		&& is_string_field(value.get("key"))
		&& is_string_field(value.get("toolName"))
		&& is_string_field(value.get("output"))
		&& value.get("ttl").map_or(false, Value::is_number)
}

fn 
is_activation_digest_shape(value: 
&Value)
 -> bool {
	is_object(Some(value))
		&& value.get("iteration").map_or(false, Value::is_number)
		&& is_string_field(value.get("outcome"))
		&& is_array_field(value.get("actions"))
		&& every_is_string(value.get("actions"))
}

fn 
is_run_summary_shape(value: &Value) -> bool {
	is_object(Some(value)) && is_string_field(value.get("text")) && is_string_field(value.get("reason"))
}

fn
 is_operator_message_shape(value: &Value) -> bool {
	is_object(Some(value))
		&& is_string_field(value.get("id"))
		&& is_string_field(value.get("text"))
		&& value.get("receivedAtIteration").map_or(false, Value::is_number)
}

fn
 is_direct_response_shape(value: &Value) -> bool {
	is_object(Some(value)) && is_string_field(value.get("text")) && value.get("createdAtIteration").map_or(false, Value::is_number)
}

fn is_last_verification_shape(value: Option<&Value>) -> bool {
	match value {
		None => true,
		Some(value) if value.is_null() => true,
		Some(value) => {
			is_object(Some(value))
				&& is_string_field(value.get("command"))
				&& value.get("failed").map_or(false, Value
::is_boolean)
		}
	}
}

pub fn is_harness_state(value: &Value) -> bool {
	is_object(Some(value))
		&& value.get("version").and_then(Value::as_f64) == Some(1.0)
		&& is_string_field(value.get("goal"))
		&& is_finite_number(value.get("iteration"))
		&& match value.get("loop") {
			None | Some(Value::Null) => true,
			Some(_) => is_finite_number(value.get("loop")),
		}
		&& is_array_field(value
.get("tasks"))
		&& value
			.get("tasks")
			.map_or(false, |tasks| tasks.as_array().map_or(false, |tasks| tasks.iter().all(is_harness_task_shape)))
		&& match value.get("history") {
			None => true,
			Some(Value::Null) => true,
			Some(_) => {
				is_array_field(value.get("history"))
					&& value.get("history").map_or(false, |history| {
						history.as_array().map_or(false, |records| records.iter().all(is_goal_record_shape))
					})
			}
		}
		&& match value.get("lastActivation") {
			None | Some(Value::Null) => true,
			Some(digest) => is_activation_digest_shape(digest),
		}
		&& match value.get("runSummary") {
			None | Some(Value::Null) => true,
			Some(summary) => is_run_summary_shape(summary),
		}
		&& match value.get("directResponse") {
			None | Some(Value::Null) => true,
			Some(response) => is_direct_response_shape(response),
		}
		&& match value.get("inboxCursor") {
			None | Some(Value::Null) => true,
			Some(_) => is_finite_number(value.get("inboxCursor")),
		}
		&& match value.get
("operatorMessages") {
			None | Some(Value::Null) => true,
			Some(_) => {
				is_array_field(value.get("operatorMessages"))
					&& value.get("operatorMessages").map_or(false, |messages| {
						messages
							.as_array()
							.map_or(false, |messages| messages.iter().all(is_operator_message_shape))
					})
			}
		}
		&& is_last_verification_shape(value.get("lastVerification"))
		&& is_array_field(value.get("memory"))
		&& value.get("memory").map_or(false
, |memory| {
			memory.as_array().map_or(false, |notes| notes.iter().all(is_memory_note_shape))
		})
		&& 
is_array_field(value.get("promotedContext"))
		&& value.get("promotedContext").map_or(false, |promoted| {
			promoted
				.as_array()
				.map_or(false, |entries| entries.iter().all(is_promoted_entry_shape))
		})
		&& is_object(value.get("telemetry"))
}

pub fn load_harness_state(state_path: &Path) -> anyhow::Result<Option<HarnessState>> {
	if !state_path.exists() {
		return Ok(None);
	}

	let raw = std::fs::read_to_string(state_path)?;
	let mut parsed_value: Value = serde_json::from_str(&raw)?;

	if 
!is_harness_state(&parsed_value) {
		bail!("The file at {} is not a valid harness state file.", state_path.display());
	}

	// State files written before goal history existed load with an empty history.
	if parsed_value.get("history").map_or(true, Value::is_null) {
		parsed_value["history"] = serde_json::json!([]);
	}
	// State files written before middle-term observations existed load with none.
	if parsed_value.get("observations").map_or(true, Value::is_null) {
		parsed_value["observations"] = serde_json::json!([]);
	}
	// State files written before task loops existed used one loop per activation,
	// so the iteration counter is the correct continuation point for the loop clock.
	if parsed_value.get("loop").map_or(true, Value::is_null) {
		let iteration = parsed_value.get("iteration").cloned().unwrap_or(Value::Null);
		parsed_value["loop"] = iteration;
	}

	let state = serde_json::from_value(parsed_value)?;

	Ok(Some(state))
}

pub fn save_harness_state(state_path: &Path, state: &HarnessState) -> std::io::Result<()> {
	let serialized = serde_json::to_string_pretty(state).expect("harness state serializes");
	// JSON.stringify(state, null, 2) plus the trailing newline the TS appends.
	write_file_atomic(state_path, &format!("{}\n", serialized), true)
}

// The single derivation for the result contract's verification and task-stat
// views (debt audit C2: these were hand-built in four files in lockstep).
/// One verdict vocabulary for every surface that prints a verification:
/// "passed" / "FAILED" / the green-but-empty case that must not read as evidence.
pub fn describe_verification_outcome(failed: bool, ran_no_tests: Option<bool>) -> &'static str {
	if failed {
		"FAILED"
	} else if ran_no_tests == Some(true) {
		"passed but executed 0 tests (not evidence)"
	} else {
		"passed"
	}
}

pub fn derive_verification_summary(state: &HarnessState) -> Option<VerificationSummary> {
	let verification = state.last_verification.as_ref()?;

	Some(VerificationSummary {
		at_iteration: verification.at_iteration,
		command: verification.command.clone(),
		failed: verification.failed,
		mutations_after: state.mutations_since_verification.unwrap_or(0),
		ran_no_tests: verification.ran_no_tests.filter(|flag| *flag),
	})
}

pub fn count_task_stats(tasks: &[HarnessTask]) 
-> TaskStats {
	TaskStats {
		blocked: tasks.iter().filter(|task| task.status == HarnessTaskStatus::Blocked).count() as i64,
		completed: tasks.iter().filter(|task| task
.status == HarnessTaskStatus::Completed).count() as i64,
		dropped: tasks.iter().filter(|task| task.status == HarnessTaskStatus::Dropped).count() as i64,
		pending: tasks
			.iter()
			.filter(|task| task.status == HarnessTaskStatus::Pending || task.status == HarnessTaskStatus::InProgress)
			.count() as i64,
	}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::{HarnessRunReason, HarnessVerificationStreak};
    use serde_json::{json, Value};
    use std::fs;
    use tempfile::TempDir;

    fn input(title: &str) -> HarnessTaskInput {
        HarnessTaskInput::from(title)
    }

    fn input_with_deps(title: &str, depends_on: &[&str]) -> HarnessTaskInput {
        HarnessTaskInput {
            depends_on: Some(depends_on.iter().map(|id| id.to_string()).collect()),
            review_of: None,
            role: None,
            title: title.to_string(),
        }
    }

    // port of it("creates empty state seeded with the goal")
    #[test]
    fn creates_empty_state_seeded_with_the_goal() {
        let state = create_harness_state("ship the feature");

        assert_eq!(state.goal, "ship the feature");
        assert_eq!(state.iteration, 0);
        assert!(state.tasks.is_empty());
        assert!(state.memory.is_empty());
        assert!(state.promoted_context.is_empty());
        assert!(!is_goal_complete(&state));
    }

    // port of it("adds tasks with sequential ids and skips blank titles")
    #[test]
    fn adds_tasks_with_sequential_ids_and_skips_blank_titles() {
        let mut state = create_harness_state("goal");

        let added_tasks = add_tasks(
            &mut state,
            vec![input("first task"), input("  "), input("second task")],
            HarnessTaskPlacement::End,
        );

        let ids: Vec<&str> = added_tasks.iter().map(|task| task.id.as_str()).collect();
        assert_eq!(ids, ["task-1", "task-2"]);
        assert_eq!(state.tasks.len(), 2);
        assert_eq!(state.tasks[0].status, HarnessTaskStatus::Pending);
    }

    // port of it("selects the current task preferring in-progress over pending")
    #[test]
    fn selects_the_current_task_preferring_in_progress_over_pending() {
        let mut state = create_harness_state("goal");

        add_tasks(&mut state, vec![input("first"), input("second")], HarnessTaskPlacement::End);
        assert_eq!(get_current_task(&state).map(|task| task.id.as_str()), Some("task-1"));

        state.tasks[1].status = HarnessTaskStatus::InProgress;
        assert_eq!(get_current_task(&state).map(|task| task.id.as_str()), Some("task-2"));
    }

    // port of it("resets ALL per-goal verification state at the goal boundary (pin the full reset list)")
    #[test]
    fn resets_all_per_goal_verification_state_at_the_goal_boundary() {
        let mut state = create_harness_state("first goal");

        add_tasks(&mut state, vec![input("work")], HarnessTaskPlacement::End);
        state.last_verification = Some(HarnessVerificationRecord {
            at_iteration: 2,
            command: "bun test".to_string(),
            failed: true,
            output_tail: "1 fail".to_string(),
            ran_no_tests: None,
        });
        state.verifications = Some(vec![state.last_verification.clone().unwrap()]);
        state.verification_streak = Some(HarnessVerificationStreak {
            command: "bun test".to_string(),
            consecutive_failures: 2,
            output_tail_hash: "abc".to_string(),
        });
        state.mutations_since_verification = Some(3);

        start_follow_up_goal(&mut state, "second goal");

        // Debt audit B4: these leaked into the next goal, which could start
        // "stuck" on the previous goal's failure streak.
        assert!(state.last_verification.is_none());
        assert!(state.verifications.is_none());
        assert!(state.verification_streak.is_none());
        assert!(state.mutations_since_verification.is_none());
    }

    // port of it("skips dependency-gated tasks until their dependencies are terminal")
    #[test]
    fn skips_dependency_gated_tasks_until_their_dependencies_are_terminal() {
        let mut state = create_harness_state("deps goal");

        add_tasks(
            &mut state,
            vec![
                input("build"),
                input_with_deps("test after build", &["task-1"]),
                input_with_deps("mistyped dep", &["task-404"]),
            ],
            HarnessTaskPlacement::End,
        );

        // task-2 waits on task-1; task-3's dependency never existed, so it must
        // not deadlock — but ordering still prefers the first workable task.
        assert_eq!(get_current_task(&state).map(|task| task.id.as_str()), Some("task-1"));
        assert!(has_unmet_dependencies(&state, &state.tasks[1]));
        assert!(!has_unmet_dependencies(&state, &state.tasks[2]));

        finish_task(
            &mut state,
            HarnessFinishArgs { status: HarnessTaskStatus::Completed, summary: "built", task_id: Some("task-1") },
        );
        assert_eq!(get_current_task(&state).map(|task| task.id.as_str()), Some("task-2"));

        // A dropped dependency also unblocks (the work was resolved either way).
        let mut state2 = create_harness_state("drop goal");

        add_tasks(
            &mut state2,
            vec![input("a"), input_with_deps("b", &["task-1"])],
            HarnessTaskPlacement::End,
        );
        drop_task(&mut state2, "task-1", "obsolete");
        assert_eq!(get_current_task(&state2).map(|task| task.id.as_str()), Some("task-2"));
    }

    // port of it("finishes the current task by default and named tasks by id")
    #[test]
    fn finishes_the_current_task_by_default_and_named_tasks_by_id() {
        let mut state = create_harness_state("goal");

        add_tasks(&mut state, vec![input("first"), input("second")], HarnessTaskPlacement::End);
        state.iteration = 3;

        let finished_task = finish_task(
            &mut state,
            HarnessFinishArgs { status: HarnessTaskStatus::Completed, summary: "done", task_id: None },
        )
        .unwrap();

        assert_eq!(finished_task.id, "task-1");
        assert_eq!(finished_task.status, HarnessTaskStatus::Completed);
        assert_eq!(finished_task.finished_at_iteration, Some(3));

        let blocked_task = finish_task(
            &mut state,
            HarnessFinishArgs { status: HarnessTaskStatus::Blocked, summary: "missing credentials", task_id: Some("task-2") },
        )
        .unwrap();

        assert_eq!(blocked_task.id, "task-2");
        assert_eq!(blocked_task.status, HarnessTaskStatus::Blocked);
        assert!(!is_goal_complete(&state));

        finish_task(
            &mut state,
            HarnessFinishArgs { status: HarnessTaskStatus::Completed, summary: "unblocked and done", task_id: Some("task-2") },
        );
        assert!(is_goal_complete(&state));
    }

    // port of it("inserts placement-next tasks after the current task, or at the front of the pending queue")
    #[test]
    fn inserts_placement_next_tasks_after_the_current_task_or_at_the_front_of_the_pending_queue() {
        let mut state = create_harness_state("goal");

        add_tasks(&mut state, vec![input("first"), input("second")], HarnessTaskPlacement::End);
        state.tasks[0].status = HarnessTaskStatus::InProgress;
        add_tasks(&mut state, vec![input("prerequisite")], HarnessTaskPlacement::Next);
        let titles: Vec<&str> = state.tasks.iter().map(|task| task.title.as_str()).collect();
        assert_eq!(titles, ["first", "prerequisite", "second"]);

        // Without an in-progress task, "next" lands before the first pending task.
        let mut pending_only = create_harness_state("goal");

        add_tasks(&mut pending_only, vec![input("a"), input("b")], HarnessTaskPlacement::End);
        add_tasks(&mut pending_only, vec![input("urgent")], HarnessTaskPlacement::Next);
        let pending_titles: Vec<&str> = pending_only.tasks.iter().map(|task| task.title.as_str()).collect();
        assert_eq!(pending_titles, ["urgent", "a", "b"]);

        // Batch ids stay sequential even when inserted mid-list.
        let mut ids: Vec<&str> = state.tasks.iter().map(|task| task.id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, ["task-1", "task-2", "task-3"]);
    }

    // port of it("drops unfinished tasks with a reason and refuses finished ones")
    #[test]
    fn drops_unfinished_tasks_with_a_reason_and_refuses_finished_ones() {
        let mut state = create_harness_state("goal");

        add_tasks(&mut state, vec![input("keep"), input("drop me")], HarnessTaskPlacement::End);
        state.iteration = 2;

        let dropped_task = drop_task(&mut state, "task-2", "  superseded  ").unwrap();

        assert_eq!(dropped_task.status, HarnessTaskStatus::Dropped);
        assert_eq!(dropped_task.summary.as_deref(), Some("superseded"));
        assert_eq!(dropped_task.finished_at_iteration, Some(2));

        assert!(drop_task(&mut state, "task-2", "again").is_none());
        finish_task(
            &mut state,
            HarnessFinishArgs { status: HarnessTaskStatus::Completed, summary: "done", task_id: Some("task-1") },
        );
        assert!(drop_task(&mut state, "task-1", "too late").is_none());
    }

    // port of it("revises unfinished task titles and records the old title as a note")
    #[test]
    fn revises_unfinished_task_titles_and_records_the_old_title_as_a_note() {
        let mut state = create_harness_state("goal");

        add_tasks(&mut state, vec![input("vague work")], HarnessTaskPlacement::End);

        let revised_task = revise_task(&mut state, "task-1", "  concrete work  ").unwrap();

        assert_eq!(revised_task.title, "concrete work");
        assert_eq!(revised_task.notes, ["Retitled from \"vague work\"."]);

        assert!(revise_task(&mut state, "task-1", "   ").is_none());
        assert!(revise_task(&mut state, "task-99", "anything").is_none());
    }

    // port of it("treats dropped tasks as finished for goal completion, but never all-dropped as complete")
    #[test]
    fn treats_dropped_tasks_as_finished_for_goal_completion_but_never_all_dropped_as_complete() {
        let mut state = create_harness_state("goal");

        add_tasks(&mut state, vec![input("real work"), input("stale work")], HarnessTaskPlacement::End);
        finish_task(
            &mut state,
            HarnessFinishArgs { status: HarnessTaskStatus::Completed, summary: "done", task_id: Some("task-1") },
        );
        assert!(!is_goal_complete(&state));

        drop_task(&mut state, "task-2", "not needed");
        assert!(is_goal_complete(&state));
        assert!(!has_unfinished_tasks(&state));

        let mut all_dropped = create_harness_state("goal");

        add_tasks(&mut all_dropped, vec![input("only work")], HarnessTaskPlacement::End);
        drop_task(&mut all_dropped, "task-1", "abandoned");
        assert!(!is_goal_complete(&all_dropped));
    }

    // port of it("appends non-empty task notes")
    #[test]
    fn appends_non_empty_task_notes() {
        let mut state = create_harness_state("goal");
        add_tasks(&mut state, vec![input("first")], HarnessTaskPlacement::End);

        append_task_note(&mut state.tasks[0], "  found the config file  ");
        append_task_note(&mut state.tasks[0], "   ");

        assert_eq!(state.tasks[0].notes, ["found the config file"]);
    }

    // port of it("manages shared memory notes")
    #[test]
    fn manages_shared_memory_notes() {
        let mut state = create_harness_state("goal");

        let note = add_memory_note(&mut state, "tests live in ./test").unwrap();

        assert_eq!(note.id, "note-1");
        assert!(add_memory_note(&mut state, "  ").is_none());
        assert!(remove_memory_note(&mut state, "note-1"));
        assert!(!remove_memory_note(&mut state, "note-1"));
        assert!(state.memory.is_empty());
    }

    // port of it("round-trips state through the state file")
    #[test]
    fn round_trips_state_through_the_state_file() {
        let temp = TempDir::new().unwrap();
        let state_dir = temp.path();
        let state_path = state_dir.join("nested").join("state.json");

        let mut state = create_harness_state("persist me");

        add_tasks(&mut state, vec![input("first")], HarnessTaskPlacement::End);
        add_memory_note(&mut state, "a durable fact");
        save_harness_state(&state_path, &state).unwrap();

        let loaded_state = load_harness_state(&state_path).unwrap().unwrap();

        assert_eq!(
            serde_json::to_value(&loaded_state).unwrap(),
            serde_json::to_value(&state).unwrap()
        );
        assert!(load_harness_state(&state_dir.join("missing.json")).unwrap().is_none());
    }

    // port of it("rejects state files that are not valid harness state")
    #[test]
    fn rejects_state_files_that_are_not_valid_harness_state() {
        let temp = TempDir::new().unwrap();
        let state_dir = temp.path();

        let wrong_shape_path = state_dir.join("wrong-shape.json");
        fs::write(&wrong_shape_path, json!({ "goal": "g", "version": 2 }).to_string()).unwrap();
        let error = load_harness_state(&wrong_shape_path).unwrap_err();
        assert!(error.to_string().contains("not a valid harness state file"));

        let corrupt_path = state_dir.join("corrupt.json");
        fs::write(&corrupt_path, "{corrupt").unwrap();
        assert!(load_harness_state(&corrupt_path).is_err());

        let bad_digest_path = state_dir.join("bad-digest.json");
        let mut digest_state = serde_json::to_value(create_harness_state("g")).unwrap();
        digest_state["lastActivation"] = json!({ "bogus": true });
        fs::write(&bad_digest_path, digest_state.to_string()).unwrap();
        let error = load_harness_state(&bad_digest_path).unwrap_err();
        assert!(error.to_string().contains("not a valid harness state file"));

        let bad_summary_path = state_dir.join("bad-summary.json");
        let mut summary_state = serde_json::to_value(create_harness_state("g")).unwrap();
        summary_state["runSummary"] = json!({ "text": 42 });
        fs::write(&bad_summary_path, summary_state.to_string()).unwrap();
        let error = load_harness_state(&bad_summary_path).unwrap_err();
        assert!(error.to_string().contains("not a valid harness state file"));
    }

    // port of it("archives the current goal's tasks into history on a follow-up and keeps memory")
    #[test]
    fn archives_the_current_goals_tasks_into_history_on_a_follow_up_and_keeps_memory() {
        let mut state = create_harness_state("first goal");

        add_tasks(&mut state, vec![input("only task")], HarnessTaskPlacement::End);
        finish_task(
            &mut state,
            HarnessFinishArgs { status: HarnessTaskStatus::Completed, summary: "done", task_id: Some("task-1") },
        );
        add_memory_note(&mut state, "a durable fact");
        state.iteration = 4;

        state.last_activation = Some(HarnessActivationDigest {
            actions: vec!["finish_task: Task task-1 marked completed.".to_string()],
            cycles: None,
            iteration: 4,
            r#loop: None,
            outcome: "a task was finished".to_string(),
            task_id: None,
        });
        state.run_summary = Some(HarnessRunSummaryNote {
            created_at_iteration: 4,
            reason: HarnessRunReason::Completed,
            text: "Finished the only task.".to_string(),
        });

        start_follow_up_goal(&mut state, "second goal");

        assert_eq!(state.goal, "second goal");
        assert!(state.tasks.is_empty());
        assert_eq!(state.history.len(), 1);
        assert_eq!(state.history[0].goal, "first goal");
        assert_eq!(state.history[0].archived_at_iteration, 4);
        assert_eq!(state.history[0].tasks[0].summary.as_deref(), Some("done"));
        // The run summary is archived onto the goal record; per-goal scratch state resets.
        assert_eq!(state.history[0].summary.as_deref(), Some("Finished the only task."));
        assert!(state.run_summary.is_none());
        assert!(state.last_activation.is_none());
        assert_eq!(state.memory.len(), 1);
        assert!(!is_goal_complete(&state));
    }

    // port of it("skips an empty history record when following up before any tasks were planned")
    #[test]
    fn skips_an_empty_history_record_when_following_up_before_any_tasks_were_planned() {
        let mut state = create_harness_state("first goal");

        start_follow_up_goal(&mut state, "second goal");

        assert!(state.history.is_empty());
        assert_eq!(state.goal, "second goal");
    }

    // port of it("reports unfinished tasks for anything not completed")
    #[test]
    fn reports_unfinished_tasks_for_anything_not_completed() {
        let mut state = create_harness_state("goal");

        assert!(!has_unfinished_tasks(&state));
        add_tasks(&mut state, vec![input("a"), input("b")], HarnessTaskPlacement::End);
        assert!(has_unfinished_tasks(&state));
        finish_task(
            &mut state,
            HarnessFinishArgs { status: HarnessTaskStatus::Completed, summary: "", task_id: Some("task-1") },
        );
        finish_task(
            &mut state,
            HarnessFinishArgs { status: HarnessTaskStatus::Blocked, summary: "stuck", task_id: Some("task-2") },
        );
        // Blocked still counts as unfinished so a same-goal re-run resumes instead of archiving.
        assert!(has_unfinished_tasks(&state));
    }

    // port of it("backfills the loop counter from the iteration counter for pre-loop state files")
    #[test]
    fn backfills_the_loop_counter_from_the_iteration_counter_for_pre_loop_state_files() {
        let temp = TempDir::new().unwrap();
        let state_dir = temp.path();
        let state_path = state_dir.join("state.json");

        // State files written before task loops existed ran one loop per activation.
        let mut legacy_state = serde_json::to_value(create_harness_state("legacy goal")).unwrap();
        legacy_state["iteration"] = json!(7);
        legacy_state.as_object_mut().unwrap().remove("loop");
        fs::write(&state_path, serde_json::to_string(&legacy_state).unwrap()).unwrap();

        let loaded_state = load_harness_state(&state_path).unwrap().unwrap();

        assert_eq!(loaded_state.r#loop, 7);
    }

    // port of it("loads a pre-history state file with an empty history")
    #[test]
    fn loads_a_pre_history_state_file_with_an_empty_history() {
        let temp = TempDir::new().unwrap();
        let state_dir = temp.path();
        let state_path = state_dir.join("state.json");

        let mut legacy_state = serde_json::to_value(create_harness_state("legacy goal")).unwrap();
        legacy_state.as_object_mut().unwrap().remove("history");
        fs::write(&state_path, serde_json::to_string(&legacy_state).unwrap()).unwrap();

        let loaded_state = load_harness_state(&state_path).unwrap().unwrap();

        assert!(loaded_state.history.is_empty());
    }

    // Fixture round-trip: a small real state.json from a real session must
    // survive load -> save with every field preserved (serde_json::Value
    // equality). The fixture is a legacy pre-observations/pre-loop file, so
    // load backfills history/observations/loop exactly like the TS `??=`
    // defaults in loadHarnessState; the round-trip expectation is the
    // original with those defaults applied.
    #[test]
    fn fixture_state_sample_round_trips_every_field() {
        let mut expected: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/state-sample.json")).unwrap();
        if expected.get("history").map_or(true, Value::is_null) {
            expected["history"] = json!([]);
        }
        if expected.get("observations").map_or(true, Value::is_null) {
            expected["observations"] = json!([]);
        }
        if expected.get("loop").map_or(true, Value::is_null) {
            let iteration = expected.get("iteration").cloned().unwrap_or(Value::Null);
            expected["loop"] = iteration;
        }

        let temp = TempDir::new().unwrap();
        let state_path = temp.path().join("state-sample.json");
        fs::write(&state_path, include_str!("../../tests/fixtures/state-sample.json")).unwrap();

        let loaded = load_harness_state(&state_path).unwrap().unwrap();

        let saved_path = temp.path().join("saved.json");
        save_harness_state(&saved_path, &loaded).unwrap();
        let saved: Value = serde_json::from_str(&fs::read_to_string(&saved_path).unwrap()).unwrap();

        assert_eq!(expected, saved);
    }
}
