// port of src/harness/telemetry.ts
//
// Tool-output telemetry: canonicalized keys so key order does not split
// records, end-keeping truncation (verdicts live at the END of verification
// output), per-loop reinforcement, warm-context promotion, decay/eviction.
// The harness state types live in drip/src/core/types.rs (port of
// src/harness/types.ts); this module works on those shared types.
use crate::core::types::{HarnessState, HarnessTelemetryConfig, PromotedContextEntry, ToolTelemetryRecord};
use serde_json::Value;

const MAX_INPUT_PREVIEW_CHARS: usize = 160;

fn sort_json_value(value: Value) -> Value {
	match value {
		Value::Array(items) => Value::Array(items.into_iter().map(sort_json_value).collect()),
		Value::Object(map) => {
			// JS `localeCompare` on ASCII keys ~ codepoint order; sort by key.
			let mut sorted_entries: Vec<(String, Value)> = map.into_iter().collect();
			sorted_entries.sort_by(|(left_key, _), (right_key, _)| left_key.cmp(right_key));
			Value::Object(sorted_entries.into_iter().collect())
		}
		other => other,
	}
}

pub fn canonicalize_tool_input(raw_input: &str) -> String {
	let trimmed_input = raw_input.trim();

	match serde_json::from_str::<Value>(trimmed_input) {
		Ok(parsed) => sort_json_value(parsed).to_string(),
		Err(_) => trimmed_input.to_string(),
	}
}

pub fn tool_telemetry_key(tool_name: &str, raw_input: &str) -> String {
	format!("{}:{}", tool_name, canonicalize_tool_input(raw_input))
}

pub fn truncate_text(value: &str, max_length: usize) -> String {
	if value.chars().count() <= max_length {
		return value.to_string();
	}
	let head = value.chars().take(max_length.saturating_sub(3)).collect::<String>();
	format!("{}...", head)
}

// Truncation for cached tool outputs keeps both ends: verification commands
// (test runners, builds, typecheckers) print their verdict at the END of the
// output, so head-only truncation would cache a result whose conclusion is
// missing — exactly the shape that lets an activation misread a failing run
// as passing. The tail gets the larger share for that reason.
pub fn truncate_text_keeping_ends(value: &str, max_length: usize) -> String {
	let value_len = value.chars().count();
	if value_len <= max_length {
		return value.to_string();
	}

	let marker = format!("\n[... {} chars elided ...]\n", value_len - max_length);
	let marker_len = marker.chars().count();

	// Budgets too small to fit the marker plus useful text on both sides fall
	// back to plain head truncation.
	if max_length < marker_len + 8 {
		return truncate_text(value, max_length);
	}

	let budget = max_length - marker_len;
	let head_length = budget / 3;
	let tail_length = budget - head_length;
	let head: String = value.chars().take(head_length).collect();
	let tail: String = value.chars().skip(value_len - tail_length).collect();

	format!("{}{}{}", head, marker, tail)
}

fn compute_ttl(reinforcements: i64, config: &HarnessTelemetryConfig) -> i64 {
	// baseTtl * 2 ** Math.max(0, reinforcements - 1), capped at maxTtl.
	let exponent = (reinforcements - 1).max(0) as u32;
	config.max_ttl.min(config.base_ttl * (2i64).pow(exponent))
}

fn find_promoted_entry<'a>(state: &'a HarnessState, key: &str) -> Option<&'a PromotedContextEntry> {
	state.promoted_context.iter().find(|entry| entry.key == key)
}

pub fn record_tool_telemetry(
	state: &mut HarnessState,
	args: RecordToolTelemetryArgs<'_>,
	config: &HarnessTelemetryConfig,
) -> ToolTelemetryRecord {
	// The telemetry clock ticks per task loop, not per cycle: reaching for a
	// result across two cycles of the same loop is just working (the transcript
	// already holds it); reaching for it in two distinct loops is re-derivation,
	// which is what warm context exists to absorb.
	let key = tool_telemetry_key(args.tool_name, args.raw_input);
	let truncated_output = truncate_text_keeping_ends(args.output, config.max_promoted_output_chars as usize);
	let mut record = match state.telemetry.get(&key) {
		Some(existing_record) => existing_record.clone(),
		None => ToolTelemetryRecord {
			call_count: 0,
			input_preview: truncate_text(&canonicalize_tool_input(args.raw_input), MAX_INPUT_PREVIEW_CHARS),
			iterations_used: Vec::new(),
			key: key.clone(),
			last_failed: None,
			last_output: truncated_output.clone(),
			last_used_iteration: state.r#loop,
			raw_input: args.raw_input.to_string(),
			reinforcements: 0,
			tool_name: args.tool_name.to_string(),
		},
	};

	let first_use_this_loop = !record.iterations_used.contains(&state.r#loop);

	record.call_count += 1;
	record.last_failed = Some(args.failed.unwrap_or(false));
	record.last_output = truncated_output.clone();
	record.last_used_iteration = state.r#loop;

	if first_use_this_loop {
		record.iterations_used.push(state.r#loop);
	}

	record.iterations_used.retain(|&loop_index| loop_index > state.r#loop - config.recency_window);
	state.telemetry.insert(key.clone(), record.clone());

	if let Some(promoted_entry) = state.promoted_context.iter_mut().find(|entry| entry.key == key) {
		if first_use_this_loop {
			record.reinforcements += 1;
		}

		promoted_entry.last_failed = record.last_failed;
		promoted_entry.output = truncated_output;
		promoted_entry.reinforcements = record.reinforcements;
		promoted_entry.ttl = compute_ttl(record.reinforcements, config);
	}

	record
}

pub struct RecordToolTelemetryArgs<'a> {
	pub failed: Option<bool>,
	pub output: &'a str,
	pub raw_input: &'a str,
	pub tool_name: &'a str,
}

#[derive(Debug, Default)]
pub struct TelemetryMaintenanceResult {
	pub expired: Vec<PromotedContextEntry>,
	pub promoted: Vec<PromotedContextEntry>,
}

pub fn run_telemetry_maintenance(
	state: &mut HarnessState,
	args: RunTelemetryMaintenanceArgs,
	config: &HarnessTelemetryConfig,
) -> TelemetryMaintenanceResult {
	let mut expired: Vec<PromotedContextEntry> = Vec::new();
	let mut promoted: Vec<PromotedContextEntry> = Vec::new();

	// TS iterates a copy while splicing the live list, so every entry decays
	// once per maintenance run even when one expires mid-pass.
	let promoted_snapshot: Vec<PromotedContextEntry> = state.promoted_context.clone();
	for entry in promoted_snapshot {
		let index = state
			.promoted_context
			.iter()
			.position(|live| live.key == entry.key)
			.expect("promoted entry disappeared mid-decay");
		state.promoted_context[index].ttl -= 1;

		if state.promoted_context[index].ttl <= 0 {
			expired.push(state.promoted_context.remove(index));
		}
	}

	// Promotion pass — iterate telemetry records in insertion order.
	let telemetry_snapshot: Vec<(String, ToolTelemetryRecord)> = state.telemetry.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
	for (key, mut record) in telemetry_snapshot {
		if find_promoted_entry(state, &key).is_some() {
			continue;
		}

		if record.iterations_used.len() < config.promote_threshold as usize {
			continue;
		}

		record.reinforcements += 1;

		let entry = PromotedContextEntry {
			dynamic: args.dynamic_tool_names.contains(&record.tool_name),
			input_preview: record.input_preview.clone(),
			key: record.key.clone(),
			last_failed: if record.last_failed == Some(true) { Some(true) } else { None },
			output: record.last_output.clone(),
			promoted_at_iteration: state.r#loop,
			raw_input: record.raw_input.clone(),
			reinforcements: record.reinforcements,
			tool_name: record.tool_name.clone(),
			ttl: compute_ttl(record.reinforcements, config),
		};

		state.promoted_context.push(entry.clone());
		promoted.push(entry);
		state.telemetry.insert(key.clone(), record);
		// TS clears iterationsUsed after promotion — the recency window
		// restarts so re-promotion counts distinct loops afresh.
		if let Some(live) = state.telemetry.get_mut(&key) {
			live.iterations_used = Vec::new();
		}
	}

	// Evict stale, unpromoted telemetry records so the persisted state file stays bounded over long runs.
	let promoted_keys: Vec<String> = state.promoted_context.iter().map(|entry| entry.key.clone()).collect();
	let stale_keys: Vec<String> = state
		.telemetry
		.iter()
		.filter(|(key, record)| {
			!promoted_keys.iter().any(|pk| pk == *key)
				&& record.last_used_iteration <= state.r#loop - config.telemetry_retention
		})
		.map(|(key, _)| key.clone())
		.collect();
	for key in stale_keys {
		state.telemetry.shift_remove(&key);
	}

	if state.promoted_context.len() > config.max_promoted_entries as usize {
		let mut ranked_entries = state.promoted_context.clone();
		ranked_entries.sort_by(|left, right| {
			if left.reinforcements != right.reinforcements {
				return right.reinforcements.cmp(&left.reinforcements);
			}

			right.promoted_at_iteration.cmp(&left.promoted_at_iteration)
		});

		state.promoted_context = ranked_entries
			.into_iter()
			.take(config.max_promoted_entries as usize)
			.collect();
	}

	TelemetryMaintenanceResult { expired, promoted }
}

pub struct RunTelemetryMaintenanceArgs {
	pub dynamic_tool_names: std::collections::HashSet<String>,
}

// port of the createHarnessState("goal") fixture the TS tests use — only the
// fields telemetry touches (loop, promotedContext, telemetry).
#[cfg(test)]
fn create_harness_state(goal: &str) -> HarnessState {
	let _ = goal;
	HarnessState::default()
}

#[cfg(test)]
mod tests {
	// port of test/harness-telemetry.test.ts
	use super::{
		canonicalize_tool_input, create_harness_state, record_tool_telemetry,
		run_telemetry_maintenance, tool_telemetry_key, RecordToolTelemetryArgs, RunTelemetryMaintenanceArgs,
		TelemetryMaintenanceResult,
	};
	use crate::core::types::{DEFAULT_TELEMETRY_CONFIG, HarnessState, HarnessTelemetryConfig, PromotedContextEntry};
	use std::collections::HashSet;

	// const config = { ...DEFAULT_TELEMETRY_CONFIG, baseTtl: 3, maxTtl: 48, promoteThreshold: 2 };
	fn config() -> HarnessTelemetryConfig {
		HarnessTelemetryConfig {
			base_ttl: 3,
			max_observations: DEFAULT_TELEMETRY_CONFIG.max_observations,
			max_observation_ttl: DEFAULT_TELEMETRY_CONFIG.max_observation_ttl,
			max_promoted_entries: DEFAULT_TELEMETRY_CONFIG.max_promoted_entries,
			max_promoted_output_chars: DEFAULT_TELEMETRY_CONFIG.max_promoted_output_chars,
			max_ttl: 48,
			observation_base_ttl: DEFAULT_TELEMETRY_CONFIG.observation_base_ttl,
			promote_threshold: 2,
			recency_window: DEFAULT_TELEMETRY_CONFIG.recency_window,
			telemetry_retention: DEFAULT_TELEMETRY_CONFIG.telemetry_retention,
		}
	}

	fn record_call(state: &mut HarnessState, tool_name: &str, raw_input: &str, config: &HarnessTelemetryConfig) {
		record_tool_telemetry(
			state,
			RecordToolTelemetryArgs {
				failed: None,
				output: &format!("output for {}", tool_name),
				raw_input,
				tool_name,
			},
			config,
		);
	}

	fn empty_dynamic() -> RunTelemetryMaintenanceArgs {
		RunTelemetryMaintenanceArgs {
			dynamic_tool_names: HashSet::new(),
		}
	}

	// it("canonicalizes tool inputs so key order does not split telemetry")
	#[test]
	fn canonicalizes_tool_inputs_so_key_order_does_not_split_telemetry() {
		assert_eq!(canonicalize_tool_input(r#"{"b":1,"a":{"d":2,"c":3}}"#), r#"{"a":{"c":3,"d":2},"b":1}"#);
		assert_eq!(canonicalize_tool_input("not json"), "not json");
		assert_eq!(tool_telemetry_key("READ", r#"{"b":1,"a":2}"#), tool_telemetry_key("READ", r#"{"a":2,"b":1}"#));
	}

	// it("does not promote a result reached for in only one task loop")
	#[test]
	fn does_not_promote_a_result_reached_for_in_only_one_task_loop() {
		let config = config();
		let mut state = create_harness_state("goal");

		state.r#loop = 1;
		record_call(&mut state, "READ", r#"{"path":"src/a.ts"}"#, &config);
		record_call(&mut state, "READ", r#"{"path":"src/a.ts"}"#, &config);
		let _ = run_telemetry_maintenance(&mut state, empty_dynamic(), &config);

		assert!(state.promoted_context.is_empty());
	}

	// it("promotes a result reached for across the threshold of distinct task loops")
	#[test]
	fn promotes_a_result_reached_for_across_the_threshold_of_distinct_task_loops() {
		let config = config();
		let mut state = create_harness_state("goal");

		state.r#loop = 1;
		record_call(&mut state, "READ", r#"{"path":"src/a.ts"}"#, &config);
		let _ = run_telemetry_maintenance(&mut state, empty_dynamic(), &config);
		state.r#loop = 2;
		record_call(&mut state, "READ", r#"{"path":"src/a.ts"}"#, &config);
		let maintenance = run_telemetry_maintenance(&mut state, empty_dynamic(), &config);

		assert_eq!(maintenance.promoted.len(), 1);
		assert_eq!(state.promoted_context.len(), 1);
		assert_eq!(state.promoted_context[0].reinforcements, 1);
		assert_eq!(state.promoted_context[0].ttl, config.base_ttl);
	}

	// it("decays unused promoted entries and expires them at zero ttl")
	#[test]
	fn decays_unused_promoted_entries_and_expires_them_at_zero_ttl() {
		let config = config();
		let mut state = create_harness_state("goal");

		state.r#loop = 1;
		record_call(&mut state, "READ", r#"{"path":"src/a.ts"}"#, &config);
		state.r#loop = 2;
		record_call(&mut state, "READ", r#"{"core/":"src/a.ts"}"#.replace("core/", "path").as_str(), &config);
		let _ = run_telemetry_maintenance(&mut state, empty_dynamic(), &config);
		assert_eq!(state.promoted_context[0].ttl, config.base_ttl);

		let mut expired_len = 0;

		for iteration in 3..=(2 + config.base_ttl) {
			state.r#loop = iteration;
			let result = run_telemetry_maintenance(&mut state, empty_dynamic(), &config);
			expired_len = result.expired.len();
		}

		assert_eq!(expired_len, 1);
		assert!(state.promoted_context.is_empty());
	}

	// it("doubles the ttl on each re-promotion like a strengthening pathway")
	#[test]
	fn doubles_the_ttl_on_each_re_promotion_like_a_strengthening_pathway() {
		let config = config();
		let mut state = create_harness_state("goal");

		state.r#loop = 1;
		record_call(&mut state, "READ", r#"{"path":"src/a.ts"}"#, &config);
		state.r#loop = 2;
		record_call(&mut state, "READ", r#"{"path":"src/a.ts"}"#, &config);
		let _ = run_telemetry_maintenance(&mut state, empty_dynamic(), &config);
		assert_eq!(state.promoted_context[0].ttl, 3);

		for iteration in 3..=5 {
			state.r#loop = iteration;
			let _ = run_telemetry_maintenance(&mut state, empty_dynamic(), &config);
		}

		assert!(state.promoted_context.is_empty());

		state.r#loop = 6;
		record_call(&mut state, "READ", r#"{"path":"src/a.ts"}"#, &config);
		state.r#loop = 7;
		record_call(&mut state, "READ", r#"{"path":"src/a.ts"}"#, &config);
		let _ = run_telemetry_maintenance(&mut state, empty_dynamic(), &config);

		assert_eq!(state.promoted_context.len(), 1);
		assert_eq!(state.promoted_context[0].reinforcements, 2);
		assert_eq!(state.promoted_context[0].ttl, 6);
	}

	// it("refreshes ttl and output when a promoted result is reached for again")
	#[test]
	fn refreshes_ttl_and_output_when_a_promoted_result_is_reached_for_again() {
		let config = config();
		let mut state = create_harness_state("goal");

		state.r#loop = 1;
		record_call(&mut state, "READ", r#"{"path":"src/a.ts"}"#, &config);
		state.r#loop = 2;
		record_call(&mut state, "READ", r#"{"path":"src/a.ts"}"#, &config);
		let _ = run_telemetry_maintenance(&mut state, empty_dynamic(), &config);

		state.r#loop = 3;
		record_tool_telemetry(
			&mut state,
			RecordToolTelemetryArgs {
				failed: None,
				output: "fresher output",
				raw_input: r#"{"path":"src/a.ts"}"#,
				tool_name: "READ",
			},
			&config,
		);

		assert_eq!(state.promoted_context[0].output, "fresher output");
		assert_eq!(state.promoted_context[0].reinforcements, 2);
		assert_eq!(state.promoted_context[0].ttl, 6);
	}

	// it("caps ttl at maxTtl and promoted entries at maxPromotedEntries")
	#[test]
	fn caps_ttl_at_max_ttl_and_promoted_entries_at_max_promoted_entries() {
		let tight_config = HarnessTelemetryConfig {
			max_ttl: 5,
			max_promoted_entries: 2,
			..config()
		};
		let mut state = create_harness_state("goal");

		for tool_name in ["WEAK", "STRONG"] {
			state.r#loop = 1;
			record_tool_telemetry(
				&mut state,
				RecordToolTelemetryArgs {
					failed: None,
					output: "o",
					raw_input: "{}",
					tool_name,
				},
				&tight_config,
			);
			state.r#loop = 2;
			record_tool_telemetry(
				&mut state,
				RecordToolTelemetryArgs {
					failed: None,
					output: "o",
					raw_input: "{}",
					tool_name,
				},
				&tight_config,
			);
		}

		let _ = run_telemetry_maintenance(&mut state, empty_dynamic(), &tight_config);
		assert_eq!(state.promoted_context.len(), 2);

		for entry in &state.promoted_context {
			assert!(entry.ttl <= tight_config.max_ttl);
		}
	}

	// it("does not promote when earlier uses fall outside the recency window")
	#[test]
	fn does_not_promote_when_earlier_uses_fall_outside_the_recency_window() {
		let window_config = HarnessTelemetryConfig {
			recency_window: 3,
			..config()
		};
		let mut state = create_harness_state("goal");

		state.r#loop = 1;
		record_call(&mut state, "READ", r#"{"path":"src/a.ts"}"#, &config());
		state.r#loop = 10;
		record_tool_telemetry(
			&mut state,
			RecordToolTelemetryArgs {
				failed: None,
				output: "o",
				raw_input: r#"{"path":"src/a.ts"}"#,
				tool_name: "READ",
			},
			&window_config,
		);
		let _ = run_telemetry_maintenance(&mut state, empty_dynamic(), &window_config);

		assert!(state.promoted_context.is_empty());
	}

	// it("truncates oversized outputs keeping both ends and canonicalizes nested arrays")
	#[test]
	fn truncates_oversized_outputs_keeping_both_ends_and_canonicalizes_nested_arrays() {
		let truncating_config = HarnessTelemetryConfig {
			max_promoted_output_chars: 60,
			..config()
		};
		let mut state = create_harness_state("goal");
		// Verification-command shape: the verdict lives at the END of the output.
		let output = format!("{}\n2 tests FAILED", "x".repeat(200));

		state.r#loop = 1;
		record_tool_telemetry(
			&mut state,
			RecordToolTelemetryArgs {
				failed: None,
				output: &output,
				raw_input: r#"{"a":[{"d":2,"c":3}]}"#,
				tool_name: "READ",
			},
			&truncating_config,
		);

		let record = state.telemetry.values().next().expect("telemetry record exists");
		assert!(record.last_output.chars().count() <= 60);
		assert!(record.last_output.contains("chars elided"));
		// Head-only truncation would lose the verdict; end-keeping truncation keeps it.
		assert!(record.last_output.ends_with("2 tests FAILED"));
		assert!(record.last_output.starts_with("xxx"));
		assert_eq!(record.key, r#"READ:{"a":[{"c":3,"d":2}]}"#);
	}
}
