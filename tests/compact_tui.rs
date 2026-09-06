//! End-to-end regression coverage for the compact TUI timeline.
//!
//! These tests sit at the display/event boundary and prove that the same
//! transcript entries (a) fold into one compact tool row for the TUI while
//! (b) staying fully intact in the persisted transcript, the `dripw` watch
//! view and the headless event stream. Pure state/frame assertions only —
//! no timing.

use std::fs;
use std::path::PathBuf;

use drip::cli::headless_output::headless_event_line;
use drip::cli::transcript::{
	append_transcript_entry, read_transcript, TranscriptEntry, TranscriptEventEntry,
	TranscriptGoalEntry, TranscriptNoteEntry, TranscriptRunEndEntry,
};
use drip::core::types::{HarnessEvent, HarnessEventData, HarnessEventType, HarnessRunReason};
use drip::tui::compact::{
  render_compact_cell, render_cycle_transition, render_tool_group,
	select_compact_tail_start, CompactCell, CompactProjection, ToolGroupCell,
};
use drip::watch::ansi::{string_width, strip_ansi};
use drip::watch::transcript_view::flatten_transcript;

// ---------- fixtures ----------

fn event(kind: HarnessEventType, iteration: i64, detail: &str, tool_name: Option<&str>) -> TranscriptEntry {
	TranscriptEntry::Event(TranscriptEventEntry {
		at: String::new(),
		data: tool_name.map(|name| HarnessEventData {
			tool_name: Some(name.to_string()),
			..Default::default()
		}),
		detail: detail.to_string(),
		goal_id: "g".to_string(),
		iteration,
		kind,
	})
}

fn tool_call(iteration: i64, name: &str, call_id: &str, args: &str) -> TranscriptEntry {
	TranscriptEntry::Event(TranscriptEventEntry {
		at: String::new(),
		data: Some(HarnessEventData {
			call_id: Some(call_id.to_string()),
			tool_name: Some(name.to_string()),
			..Default::default()
		}),
		detail: format!("{name} {args}"),
		goal_id: "g".to_string(),
		iteration,
		kind: HarnessEventType::ToolCall,
	})
}

fn tool_result(call_id: &str, name: &str, failed: bool) -> TranscriptEntry {
	TranscriptEntry::Event(TranscriptEventEntry {
		at: String::new(),
		data: Some(HarnessEventData {
			call_id: Some(call_id.to_string()),
			tool_name: Some(name.to_string()),
			failed: Some(failed),
			..Default::default()
		}),
		detail: format!("{name}: result text"),
		goal_id: "g".to_string(),
		iteration: 1,
		kind: HarnessEventType::ToolResult,
	})
}

fn inference(iteration: i64, detail: &str) -> TranscriptEntry {
	TranscriptEntry::Event(TranscriptEventEntry {
		at: String::new(),
		data: Some(HarnessEventData {
			model: Some("z-ai/glm-5.3-flash".to_string()),
			prompt_tokens: Some(21018),
			completion_tokens: Some(864),
			..Default::default()
		}),
		detail: detail.to_string(),
		goal_id: "g".to_string(),
		iteration,
		kind: HarnessEventType::Inference,
	})
}

fn goal(text: &str) -> TranscriptEntry {
	TranscriptEntry::Goal(TranscriptGoalEntry {
		at: String::new(),
		goal_id: "g".to_string(),
		images: Vec::new(),
		mentions: Vec::new(),
		text: text.to_string(),
	})
}

fn model_text(iteration: i64, detail: &str) -> TranscriptEntry {
	event(HarnessEventType::ModelText, iteration, detail, None)
}

fn run_end(iterations: i64) -> TranscriptEntry {
	TranscriptEntry::RunEnd(TranscriptRunEndEntry {
		at: String::new(),
		goal_id: "g".to_string(),
		iterations,
		reason: HarnessRunReason::Completed,
	})
}

fn error_note(text: &str) -> TranscriptEntry {
	TranscriptEntry::Error(TranscriptNoteEntry {
		at: String::new(),
		text: text.to_string(),
	})
}

/// The exact exchange from the spec: READ → result → infer → PATCH → result.
fn one_exchange() -> Vec<TranscriptEntry> {
	vec![
		goal("fix the flaky test"),
		tool_call(1, "READ", "c1", "{\"path\":\"src/lib.rs\"}"),
		tool_result("c1", "READ", false),
		inference(1, "z-ai/glm-5.3-flash — 21018 prompt (0 cached, 0 written), 864 completion"),
		tool_call(1, "PATCH", "c2", "{\"path\":\"src/lib.rs\"}"),
		tool_result("c2", "PATCH", false),
	]
}

fn compact_rows(projection: &CompactProjection, width: usize) -> Vec<String> {
	projection
		.cells
		.iter()
		.flat_map(|cell| render_compact_cell(cell, width))
		.map(|row| strip_ansi(&row))
		.collect()
}

fn group_count(projection: &CompactProjection) -> usize {
	projection
		.cells
		.iter()
		.filter(|c| matches!(c, CompactCell::ToolGroup(_)))
		.count()
}

fn last_group(cells: &[CompactCell]) -> &ToolGroupCell {
	match cells.last() {
		Some(CompactCell::ToolGroup(group)) => group,
		other => panic!("expected trailing tool group, got {other:?}"),
	}
}

fn feed(entries: &[TranscriptEntry]) -> CompactProjection {
	let mut p = CompactProjection::new();
	for entry in entries {
		p.append(entry);
	}
	p.finalize();
	p
}

fn temp_jsonl(name: &str) -> PathBuf {
	let path = std::env::temp_dir().join(format!("drip-compact-tui-{}-{name}.jsonl", std::process::id()));
	let _ = fs::remove_file(&path);
	path
}

// ---------- TUI folding ----------

#[test]
fn read_result_infer_patch_result_folds_into_one_two_tool_row() {
	let p = feed(&one_exchange());

	assert_eq!(group_count(&p), 1, "{:?}", p.cells);
	let group = last_group(&p.cells);
	assert_eq!(group.count, 2);
	assert_eq!(group.tools, vec!["READ", "PATCH"]);
	assert_eq!(group.iteration, 1);

	let rows = compact_rows(&p, 120);
	// user goal stays visible
	assert!(rows.iter().any(|r| r.contains("fix the flaky test")), "{rows:?}");
	assert!(rows.iter().any(|r| r.contains("── 2 Tools called: READ, PATCH ──")), "{rows:?}");
	// absorbed: tool results and inference telemetry are not TUI rows
	assert!(!rows.iter().any(|r| r.contains("result text")), "{rows:?}");
	assert!(!rows.iter().any(|r| r.contains("21018 prompt")), "{rows:?}");
}

#[test]
fn next_cycle_begins_fresh_row_with_numbered_task_preview() {
	let mut entries = one_exchange();
	entries.push(event(
		HarnessEventType::IterationStart,
		2,
		"cycle 2/5 — task-1: fix the flaky test: plan the patch [budget 100/60000 tokens]",
		None,
	));
	entries.push(tool_call(2, "BASH", "c3", "{}"));
	let p = feed(&entries);

	assert_eq!(group_count(&p), 2, "cycles must never merge");
	let second = match p.cells.last() {
		Some(CompactCell::ToolGroup(group)) => group,
		other => panic!("{other:?}"),
	};
	assert_eq!(second.iteration, 2);
	assert_eq!(second.tools, vec!["BASH"]);

	let rows = compact_rows(&p, 160);
	// transition row: numbered cycle + task preview + budget indicator
	let transition = rows.iter().find(|r| r.contains("cycle 2/5")).expect("transition row");
	assert!(transition.contains("task-1: fix the flaky test"), "{transition}");
	assert!(transition.contains("budget 100/60000 tokens"), "{transition}");
	assert!(rows.iter().any(|r| r.contains("── 1 Tool called: BASH ──")), "{rows:?}");
}

#[test]
fn separate_goals_with_repeated_iteration_numbers_never_merge() {
	let entries = vec![
		goal("first goal"),
		tool_call(1, "READ", "c1", "{}"),
		model_text(1, "answer one"),
		goal("second goal"),
		tool_call(1, "BASH", "c2", "{}"),
		run_end(2),
	];
	let p = feed(&entries);

	assert_eq!(group_count(&p), 2);
	let rows = compact_rows(&p, 120);
	assert!(rows.iter().any(|r| r.contains("first goal")), "{rows:?}");
	assert!(rows.iter().any(|r| r.contains("answer one")), "{rows:?}");
	assert!(rows.iter().any(|r| r.contains("second goal")), "{rows:?}");
	assert!(rows.iter().any(|r| r.contains("── 1 Tool called: READ ──")), "{rows:?}");
	assert!(rows.iter().any(|r| r.contains("── 1 Tool called: BASH ──")), "{rows:?}");
}

#[test]
fn empty_cycle_produces_no_group_row() {
	let entries = vec![
		event(HarnessEventType::IterationStart, 1, "cycle 1/3 — task-1: think: no tools [budget 0/60000 tokens]", None),
		model_text(1, "All done."),
	];
	let p = feed(&entries);

	assert_eq!(group_count(&p), 0);
	let rows = compact_rows(&p, 120);
	assert!(rows.iter().any(|r| r.contains("cycle 1/3")), "{rows:?}");
	assert!(rows.iter().any(|r| r.contains("All done.")), "{rows:?}");
	assert!(!rows.iter().any(|r| r.contains("Tools called")), "{rows:?}");
}

#[test]
fn warnings_and_errors_stay_visible_and_flush_groups() {
	let mut p = CompactProjection::new();
	p.append(&tool_call(1, "READ", "c1", "{}"));
	p.append(&event(HarnessEventType::RunWarning, 1, "rate limited, waiting 2s", None));
	p.append(&tool_call(1, "PATCH", "c2", "{}"));
	p.append(&error_note("boom"));
	p.finalize();

	assert_eq!(group_count(&p), 2, "warning/error are boundaries, not group members");
	let rows = compact_rows(&p, 120);
	assert!(rows.iter().any(|r| r.contains("rate limited")), "{rows:?}");
	assert!(rows.iter().any(|r| r.contains("boom")), "{rows:?}");
}

#[test]
fn replay_with_unfinished_final_group_matches_incremental_feed() {
	let mut entries = one_exchange();
	// group still open when the session is reloaded (no boundary yet)
	entries.push(tool_call(1, "GREP", "c4", "{\"pattern\":\"todo\"}"));

	let mut incremental = CompactProjection::new();
	for entry in &entries {
		incremental.append(entry);
	}
	let mut replay = CompactProjection::rebuild(&entries);

	assert_eq!(incremental.cells, replay.cells, "live and replay must agree");
	assert_eq!(incremental.is_active(), replay.is_active());
	assert!(replay.is_active(), "unfinished final group stays open for the live row");
	assert_eq!(replay.active_group().unwrap().count, 3);

	incremental.finalize();
	replay.finalize();
	assert_eq!(incremental.cells, replay.cells);
	assert_eq!(last_group(&incremental.cells).tools, vec!["READ", "PATCH", "GREP"]);
}

// ---------- narrow terminals / resize ----------

#[test]
fn narrow_terminals_clip_rows_ansi_safely() {
	let mut p = CompactProjection::new();
	p.append(&goal("fix the ☕ café test — a very long unicode goal text"));
	p.append(&tool_call(1, "READ", "c1", "{\"path\":\"src/ünïcödé/päth.rs\"}"));
	p.append(&tool_call(1, "PATCH", "c2", "{}"));
	p.append(&event(
		HarnessEventType::IterationStart,
		2,
		"cycle 2/5 — task-1: fix the ☕ café test: a very long planning preview [budget 100/60000 tokens]",
		None,
	));
	p.finalize();

	let iteration_event = p
		.cells
		.iter()
		.find_map(|cell| match cell {
			CompactCell::Passthrough(TranscriptEntry::Event(ev))
				if ev.kind == HarnessEventType::IterationStart =>
			{
				Some(ev.clone())
			}
			_ => None,
		})
		.expect("iteration transition cell");

	for width in [10usize, 20, 40] {
		for cell in &p.cells {
			let rows = match cell {
				CompactCell::ToolGroup(group) => render_tool_group(group, width),
				CompactCell::Passthrough(TranscriptEntry::Event(ev))
					if ev.kind == HarnessEventType::IterationStart =>
				{
					render_cycle_transition(ev, width)
				}
				other => render_compact_cell(other, width),
			};
			assert!(!rows.is_empty(), "width {width}");
			for row in rows {
				let plain = strip_ansi(&row);
				assert!(string_width(&plain) <= width, "width {width}: {plain:?}");
			}
		}
	}

	// the narrow transition keeps the cycle number, drops the budget, marks the clip
	let transition = render_cycle_transition(&iteration_event, 24);
	assert_eq!(transition.len(), 1);
	let plain = strip_ansi(&transition[0]);
	assert!(plain.contains("cycle 2/5"), "{plain}");
	assert!(plain.contains('…'), "{plain}");
	assert!(!plain.contains("budget"), "{plain}");
}

#[test]
fn resize_tail_budget_keeps_the_newest_cells_only() {
	let mut p = CompactProjection::new();
	for i in 1..=12 {
		p.append(&event(
			HarnessEventType::IterationStart,
			i,
			&format!("cycle {i}/12 — task-{i}: preview {i} [budget {i}/60000 tokens]"),
			None,
		));
		p.append(&tool_call(i, "READ", &format!("c{i}"), "{}"));
	}
	p.finalize();
	let cells = p.into_cells();
	assert_eq!(cells.len(), 24);

	// roomy terminal keeps the whole timeline
	assert_eq!(select_compact_tail_start(&cells, 500), 0);

	// tiny terminal trims but the newest cycle's transition stays visible
	let start = select_compact_tail_start(&cells, 8);
	assert!(start > 0, "tiny terminal must trim, start={start}");
	assert!(start < cells.len());
	let tail_text: String = cells[start..]
		.iter()
		.flat_map(|cell| render_compact_cell(cell, 80))
		.map(|row| strip_ansi(&row))
		.collect::<Vec<_>>()
		.join("\n");
	assert!(tail_text.contains("cycle 12/12"), "{tail_text}");
}

// ---------- persistence / dripw / headless stay raw ----------

#[test]
fn serialized_transcript_roundtrip_keeps_raw_tool_and_inference_data() {
	let path = temp_jsonl("roundtrip");
	let entries = vec![
		goal("fix the flaky test"),
		tool_call(1, "READ", "c1", "{\"path\":\"src/lib.rs\",\"limit\":40}"),
		tool_result("c1", "READ", false),
		inference(1, "z-ai/glm-5.3-flash — 21018 prompt"),
		error_note("boom"),
		run_end(3),
	];
	for entry in &entries {
		append_transcript_entry(&path, entry).expect("append");
	}

	let read_back = read_transcript(&path);
	assert_eq!(read_back.len(), entries.len(), "{read_back:?}");
	assert_eq!(read_back, entries, "typed roundtrip changed the record");

	// spot-check raw fields survive: arguments, call ids, failure flags, token counts
	match &read_back[1] {
		TranscriptEntry::Event(ev) => {
			assert_eq!(ev.kind, HarnessEventType::ToolCall);
			assert_eq!(ev.detail, "READ {\"path\":\"src/lib.rs\",\"limit\":40}");
			let data = ev.data.as_ref().unwrap();
			assert_eq!(data.tool_name.as_deref(), Some("READ"));
			assert_eq!(data.call_id.as_deref(), Some("c1"));
		}
		other => panic!("{other:?}"),
	}
	match &read_back[2] {
		TranscriptEntry::Event(ev) => {
			assert_eq!(ev.kind, HarnessEventType::ToolResult);
			assert_eq!(ev.data.as_ref().unwrap().failed, Some(false));
		}
		other => panic!("{other:?}"),
	}
	match &read_back[3] {
		TranscriptEntry::Event(ev) => {
			assert_eq!(ev.kind, HarnessEventType::Inference);
			let data = ev.data.as_ref().unwrap();
			assert_eq!(data.prompt_tokens, Some(21018));
			assert_eq!(data.completion_tokens, Some(864));
			assert_eq!(data.model.as_deref(), Some("z-ai/glm-5.3-flash"));
		}
		other => panic!("{other:?}"),
	}
	let _ = fs::remove_file(&path);
}

#[test]
fn dripw_view_still_shows_raw_tool_rows_and_telemetry() {
	let entries = one_exchange();
	let rows: Vec<String> = flatten_transcript(&entries, 200)
		.into_iter()
		.map(|cell| cell.text)
		.collect();

	// compaction is TUI-only: dripw keeps one raw row per tool, per result and
	// per inference event (its own formatting, nothing folded)
	assert_eq!(rows.iter().filter(|r| r.contains("] tool ")).count(), 2, "{rows:?}");
	assert_eq!(rows.iter().filter(|r| r.contains("] result ")).count(), 2, "{rows:?}");
	assert_eq!(rows.iter().filter(|r| r.contains("] infer ")).count(), 1, "{rows:?}");
	assert!(rows.iter().any(|r| r.contains("READ src/lib.rs")), "{rows:?}");
	assert!(rows.iter().any(|r| r.contains("PATCH src/lib.rs")), "{rows:?}");
	assert!(rows.iter().any(|r| r.contains("result text")), "{rows:?}");
	assert!(rows.iter().any(|r| r.contains("21.0k")), "{rows:?}");
	assert!(rows.iter().any(|r| r.contains("fix the flaky test")), "{rows:?}");
}

#[test]
fn headless_stream_still_shows_every_tool_and_inference_event() {
	let harness_event = HarnessEvent {
		data: Some(HarnessEventData {
			call_id: Some("c1".to_string()),
			tool_name: Some("READ".to_string()),
			..Default::default()
		}),
		detail: "READ {\"path\":\"src/lib.rs\"}".to_string(),
		iteration: 1,
		r#type: HarnessEventType::ToolCall,
	};
	let plain = headless_event_line(&harness_event, false).expect("plain line");
	assert!(plain.contains("READ"), "{plain}");
	assert!(plain.contains("src/lib.rs"), "{plain}");
	let json = headless_event_line(&harness_event, true).expect("json line");
	assert!(json.contains("READ") && json.contains("c1"), "{json}");

	let infer_event = HarnessEvent {
		data: Some(HarnessEventData {
			prompt_tokens: Some(21018),
			..Default::default()
		}),
		detail: "z-ai/glm-5.3-flash — 21018 prompt".to_string(),
		iteration: 1,
		r#type: HarnessEventType::Inference,
	};
	let infer_line = headless_event_line(&infer_event, false).expect("inference line");
	assert!(infer_line.contains("21018 prompt"), "{infer_line}");
}
