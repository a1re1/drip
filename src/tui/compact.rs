//! Compact TUI projection of the raw transcript timeline (presentation-only).
//!
//! The full transcript keeps every event; this layer folds back-to-back tool
//! activity within one goal cycle into a single summary row such as
//! `── 3 Tools called: READ, PATCH, BASH ──` so the TUI shows user goals and
//! model responses without per-tool chatter. Logs, `dripw` and headless
//! rendering are untouched — this module is only used by the TUI.
//!
//! Rules:
//! - Only `ToolCall` events are counted; a matching `ToolResult` (same
//!   `call_id`) is absorbed, never counted twice.
//! - Interleaved `Inference` telemetry is absorbed without breaking the group.
//! - A group never merges across `IterationStart` (cycle boundary), goals,
//!   model text, run summaries, warnings/errors, run end or other visible
//!   events — those finalize the open group and render themselves.

use crate::cli::transcript::{TranscriptEntry, TranscriptEventEntry};
use crate::core::types::HarnessEventType;
use crate::watch::ansi::{c, string_width};

/// One presentation row-group in the compact timeline.
#[derive(Debug, Clone, PartialEq)]
pub enum CompactCell {
	/// Folded `── N Tools called: … ──` row.
	ToolGroup(ToolGroupCell),
	/// Any other transcript entry rendered as before.
	Passthrough(TranscriptEntry),
}

impl CompactCell {
	/// The transcript entry this cell renders from (groups synthesize one).
	pub fn entry(&self) -> &TranscriptEntry {
		match self {
			CompactCell::ToolGroup(group) => &group.source,
			CompactCell::Passthrough(entry) => entry,
		}
	}
}

/// A folded group of tool activity within one cycle.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolGroupCell {
	/// Number of ToolCall events (not ToolResults).
	pub count: usize,
	/// Distinct tool names in first-seen order.
	pub tools: Vec<String>,
	/// Total calls that failed, when known.
	pub failed: usize,
	/// Cycle number of the first folded call.
	pub iteration: i64,
	/// The first folded ToolCall entry, used for its timestamp.
	source: TranscriptEntry,
}

/// Presentation-only state machine folding tool activity per cycle.
///
/// Generic over storage so the TUI app can project into its own cell vector
/// without a second allocation per group.
pub struct CompactProjection {
	/// Finalized cells, in chronological order.
	pub cells: Vec<CompactCell>,
	/// Open group (None when the last visible event was a boundary).
	open: Option<ToolGroupCell>,
}

impl Default for CompactProjection {
	fn default() -> Self {
		Self::new()
	}
}

impl CompactProjection {
	pub fn new() -> Self {
		Self {
			cells: Vec::new(),
			open: None,
		}
	}

	/// Project a full transcript (replay / session load).
	pub fn rebuild(entries: &[TranscriptEntry]) -> Self {
		let mut projection = Self::new();
		for entry in entries {
			projection.append(entry);
		}
		projection
	}

	/// Feed one transcript entry; appends, extends or finalizes cells.
	pub fn append(&mut self, entry: &TranscriptEntry) {
		match entry {
			TranscriptEntry::Event(event) => self.append_event(event),
			other => {
				self.flush_active();
				self.cells.push(CompactCell::Passthrough(other.clone()));
			}
		}
	}

	fn append_event(&mut self, event: &TranscriptEventEntry) {
		match event.kind {
			HarnessEventType::ToolCall => {
				self.open.get_or_insert_with(|| ToolGroupCell {
					count: 0,
					tools: Vec::from([event_tool_name(event)]),
					failed: 0,
					iteration: event.iteration,
					source: TranscriptEntry::Event(event.clone()),
				});
				let group = self.open.as_mut().unwrap();
				let name = event_tool_name(event);
				if !name.is_empty() && !group.tools.contains(&name) {
					group.tools.push(name);
				}
				group.count += 1;

			}
			HarnessEventType::ToolResult => {
				// Absorbed into the open group; counted once (as its ToolCall).
				if self.open.is_none() {
					// Result without a seen call (e.g. replay gap): render
					// as a normal event row rather than losing it.
					self.flush_active();
					self.cells.push(CompactCell::Passthrough(TranscriptEntry::Event(event.clone())));
				} else if event.data.as_ref().and_then(|data| data.failed) == Some(true) {
					// Failure signal from the transcript: keep the compact row
					// honest about calls that did not succeed.
					self.open.as_mut().unwrap().failed += 1;
				}
			}
			HarnessEventType::Inference => {
				// Routine telemetry: always suppressed in the compact view.
			}
			_ => {
				self.flush_active();
				self.cells.push(CompactCell::Passthrough(TranscriptEntry::Event(event.clone())));
			}
		}
	}

	/// Finalize any open tool group into the cell list.
	pub fn finalize(&mut self) {
		self.flush_active();
	}

	/// Whether a tool group is currently open (live summary eligible).
	pub fn is_active(&self) -> bool {
		self.open.is_some()
	}

	/// Mutable access for the app to repaint the live row in place.
	pub fn active_group(&self) -> Option<&ToolGroupCell> {
		self.open.as_ref()
	}

	fn flush_active(&mut self) {
		if let Some(group) = self.open.take() {
			self.cells.push(CompactCell::ToolGroup(group));
		}
	}

	/// Consume the projection into renderable cells (replay / final paint).
	pub fn into_cells(self) -> Vec<CompactCell> {
		self.cells
	}
}

/// App-facing emitter wrapping a projection plus the "already painted"
/// cursor: the exact state `TuiApp` uses to guarantee each finalized group
/// reaches scrollback exactly once. Pure state — the app renders `absorb`/
/// `finalize`/`drain` results into scrollback and the live row comes from
/// `projection.active_group()`. Live batches, startup replay, session
/// switches and resize tail-budgeting all share this one helper.
pub struct CompactEmitter {
	/// Presentation projection (finalized cells + open group).
	pub projection: CompactProjection,
	emitted: usize,
}

impl Default for CompactEmitter {
	fn default() -> Self {
		Self::new()
	}
}

impl CompactEmitter {
	pub fn new() -> Self {
		Self { emitted: 0, projection: CompactProjection::new() }
	}

	/// Feed raw entries (a live batch or startup replay) and return the
	/// compact cells they finalized. Each cell is returned exactly once;
	/// telemetry-only batches return an empty vec.
	pub fn absorb(&mut self, entries: &[TranscriptEntry]) -> Vec<CompactCell> {
		for entry in entries {
			self.projection.append(entry);
		}
		self.drain()
	}

	/// Newly finalized cells since the last drain.
	pub fn drain(&mut self) -> Vec<CompactCell> {
		let cells = self.projection.cells[self.emitted..].to_vec();
		self.emitted = self.projection.cells.len();
		cells
	}

	/// Replace state from raw transcript entries (session switch / startup
	/// replay); everything already finalized counts as painted.
	pub fn rebuild(&mut self, entries: &[TranscriptEntry]) {
		self.projection = CompactProjection::rebuild(entries);
		self.emitted = self.projection.cells.len();
	}

	/// Settle any open group at a run boundary (end/error/cancel) and return
	/// the cells to paint (the group plus any pending passthroughs).
	pub fn finalize(&mut self) -> Vec<CompactCell> {
		self.projection.finalize();
		self.drain()
	}
}

/// Tool name for a ToolCall/ToolResult event: prefer structured `data`,
/// fall back to the detail's leading token.
fn event_tool_name(event: &TranscriptEventEntry) -> String {
	if let Some(name) = event.data.as_ref().and_then(|data| data.tool_name.as_ref()) {
		return name.clone();
	}

	event
		.detail
		.split(|ch: char| ch.is_whitespace())
		.next()
		.unwrap_or_default()
		.to_string()
}

/// Render a tool group as the single painted summary row: dim iteration
/// prefix, white `── N Tools called: … ──` summary, optional failed count.
/// Clipping happens on the plain text BEFORE painting (drop the failed
/// suffix first, then hard-clip with an ellipsis), so ANSI escapes stay
/// balanced and the row always occupies exactly one line.
pub fn render_tool_group(group: &ToolGroupCell, width: usize) -> Vec<String> {
	let label = if group.count == 1 { "Tool called" } else { "Tools called" };
	let summary = format!("\u{2500}\u{2500} {} {}: {} \u{2500}\u{2500}", group.count, label, group.tools.join(", "));
	let mut plain = format!("[{:>3}] {}", group.iteration, summary);

	if group.failed > 0 {
		plain.push_str(&format!(" ({} failed)", group.failed));
	}

	if width > 0 && string_width(&plain) > width {
		if group.failed > 0 {
			plain = format!("[{:>3}] {}", group.iteration, summary);
		}

		if string_width(&plain) > width {
			let mut clipped: String = plain.chars().take(width.saturating_sub(1)).collect();

			while string_width(&clipped) > width.saturating_sub(1) {
				clipped.pop();
			}

			clipped.push('\u{2026}');
			plain = clipped;
		}
	}

	let (prefix, rest) = plain.split_at(plain.find(']').map(|i| i + 1).unwrap_or(0));
	vec![format!("{}{}", c::dim(prefix), c::white(rest))]
}

/// Render one projected cell as painted ANSI rows (no trailing newlines).
pub fn render_compact_cell(cell: &CompactCell, width: usize) -> Vec<String> {
	match cell {
		CompactCell::ToolGroup(group) => render_tool_group(group, width),
		CompactCell::Passthrough(entry) => crate::tui::timeline::render_timeline_cell(entry, width),
	}
}

/// Budget helper mirroring `estimate_cell_rows` for projected cells.
pub fn estimate_compact_rows(cell: &CompactCell) -> usize {
	match cell {
		CompactCell::ToolGroup(_) => 1,
		CompactCell::Passthrough(entry) => crate::tui::timeline::estimate_cell_rows(entry),
	}
}

/// After a repaint clears the screen, only the most recent projected cells
/// that fit above the live region are re-emitted.
pub fn select_compact_tail_start(cells: &[CompactCell], terminal_rows: usize) -> usize {
	let budget = crate::tui::timeline::MIN_TAIL_ROWS
		.max(terminal_rows.saturating_sub(crate::tui::timeline::LIVE_REGION_RESERVED_ROWS));
	let mut used_rows = 0usize;

	if cells.is_empty() {
		return 0;
	}

	for (index, cell) in cells.iter().enumerate().rev() {
		used_rows += estimate_compact_rows(cell);

		if used_rows > budget {
			return (index + 1).min(cells.len() - 1);
		}
	}

	0
}

/// Numbered cycle-transition preview row painted from an IterationStart
/// detail ("cycle 2/5 — task-3: Title: plan text [budget 1234/60000 tokens]").
/// Whitespace-flattened, width-aware: the budget indicator is retained but is
/// the first thing dropped when the terminal is too narrow; the body is then
/// hard-clipped so the row always occupies exactly one line. Clipping happens
/// on the plain text BEFORE painting, so ANSI escapes stay balanced.
pub fn render_cycle_transition(event: &TranscriptEventEntry, width: usize) -> Vec<String> {
	let flat = event.detail.split_whitespace().collect::<Vec<_>>().join(" ");
	let budget_marker = " [budget ";
	let (body, budget) = match flat.rfind(budget_marker) {
		Some(start) => (flat[..start].trim_end().to_string(), Some(flat[start..].to_string())),
		None => (flat.clone(), None),
	};

	// Keep the budget indicator only while the whole row fits; otherwise the
	// planning text is the part worth reading.
	let mut plain = match &budget {
		Some(b) if width == 0 || string_width(&format!("{body} {b}")) <= width => format!("{body} {b}"),
		_ => body.clone(),
	};

	if width > 0 && string_width(&plain) > width {
		let mut clipped: String = plain.chars().take(width.saturating_sub(1)).collect();

		while string_width(&clipped) > width.saturating_sub(1) {
			clipped.pop();
		}

		clipped.push('\u{2026}');
		plain = clipped;
	}

	vec![c::white(&plain)]
}
#[cfg(test)]
mod tests {
	use super::*;
	use crate::watch::ansi::strip_ansi;
	use crate::cli::transcript::{TranscriptEventEntry, TranscriptGoalEntry, TranscriptRunEndEntry};

	fn event(kind: HarnessEventType, iteration: i64, detail: &str, tool_name: Option<&str>) -> TranscriptEntry {
		TranscriptEntry::Event(TranscriptEventEntry {
			at: String::new(),
			data: tool_name.map(|name| crate::core::types::HarnessEventData {
				tool_name: Some(name.to_string()),
				..Default::default()
			}),
			detail: detail.to_string(),
			goal_id: "g".to_string(),
			iteration,
			kind,
		})
	}

	fn tool_call(iteration: i64, name: &str, call_id: &str) -> TranscriptEntry {
		TranscriptEntry::Event(TranscriptEventEntry {
			at: String::new(),
			data: Some(crate::core::types::HarnessEventData {
				call_id: Some(call_id.to_string()),
				tool_name: Some(name.to_string()),
				..Default::default()
			}),
			detail: format!("{name} {{\"path\":\"x\"}}"),
			goal_id: "g".to_string(),
			iteration,
			kind: HarnessEventType::ToolCall,
		})
	}

	fn tool_result(call_id: &str, failed: bool) -> TranscriptEntry {
		TranscriptEntry::Event(TranscriptEventEntry {
			at: String::new(),
			data: Some(crate::core::types::HarnessEventData {
				call_id: Some(call_id.to_string()),
				tool_name: Some("READ".to_string()),
				failed: Some(failed),
				..Default::default()
			}),
			detail: "READ: result text".to_string(),
			goal_id: "g".to_string(),
			iteration: 1,
			kind: HarnessEventType::ToolResult,
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

	fn group_rows(projection: &CompactProjection, width: usize) -> Vec<String> {
		projection
			.cells
			.iter()
			.flat_map(|cell| render_compact_cell(cell, width))
			.map(|row| strip_ansi(&row))
			.collect()
	}

	fn last_group(cells: &[CompactCell]) -> &ToolGroupCell {
		match cells.last() {
			Some(CompactCell::ToolGroup(group)) => group,
			other => panic!("expected trailing tool group, got {other:?}"),
		}
	}

	#[test]
	fn repeated_tools_fold_with_distinct_names_in_first_seen_order() {
		let mut p = CompactProjection::new();
		p.append(&tool_call(1, "READ", "c1"));
		p.append(&tool_call(1, "PATCH", "c2"));
		p.append(&tool_call(1, "READ", "c3"));
		p.append(&tool_call(1, "BASH", "c4"));
		p.finalize();

		let group = last_group(&p.cells);
		assert_eq!(group.count, 4);
		assert_eq!(group.tools, vec!["READ", "PATCH", "BASH"]);
		assert_eq!(group.iteration, 1);
		assert_eq!(group_rows(&p, 120).last().unwrap(), "[  1] ── 4 Tools called: READ, PATCH, BASH ──");
	}

	#[test]
	fn tool_results_are_absorbed_and_never_counted() {
		let mut p = CompactProjection::new();
		p.append(&tool_call(1, "READ", "c1"));
		p.append(&tool_result("c1", false));
		p.append(&tool_call(1, "BASH", "c2"));
		p.append(&tool_result("c2", false));
		p.finalize();

		assert_eq!(last_group(&p.cells).count, 2);
		assert_eq!(last_group(&p.cells).tools, vec!["READ", "BASH"]);
	}

	#[test]
	fn failed_results_increment_the_group_failed_count() {
		let mut p = CompactProjection::new();
		p.append(&tool_call(1, "READ", "c1"));
		p.append(&tool_result("c1", true));
		p.append(&tool_call(1, "BASH", "c2"));
		p.append(&tool_result("c2", false));
		p.append(&tool_call(1, "FETCH", "c3"));
		p.append(&tool_result("c3", true));
		p.finalize();

		let group = last_group(&p.cells);
		assert_eq!(group.count, 3, "{group:?}");
		assert_eq!(group.failed, 2, "{group:?}");
		assert_eq!(
			group_rows(&p, 120).last().unwrap(),
			"[  1] ── 3 Tools called: READ, BASH, FETCH ── (2 failed)"
		);

		// Narrow terminal: the failed suffix is dropped before the summary
		// is hard-clipped, and the group still renders as exactly one row.
		let rows = group_rows(&p, 40);
		assert_eq!(rows.len(), 1, "{rows:?}");
		assert!(string_width(rows[0].as_str()) <= 40, "{rows:?}");
		assert!(rows[0].contains("3 Tools called"), "{rows:?}");
		assert!(!rows[0].contains("failed"), "{rows:?}");
	}

	#[test]
	fn interleaved_inference_telemetry_does_not_break_the_group() {
		let mut p = CompactProjection::new();
		p.append(&tool_call(1, "READ", "c1"));
		p.append(&event(HarnessEventType::Inference, 1, "z-ai/glm — 21018 prompt", None));
		p.append(&tool_result("c1", false));
		p.append(&event(HarnessEventType::Inference, 1, "another infer row", None));
		p.append(&tool_call(1, "PATCH", "c2"));
		p.finalize();

		assert_eq!(p.cells.len(), 1, "{:?}", p.cells);
		assert_eq!(last_group(&p.cells).count, 2);
	}

	#[test]
	fn missing_tool_metadata_falls_back_to_detail() {
		let mut p = CompactProjection::new();
		p.append(&event(HarnessEventType::ToolCall, 1, "GREP {\"pattern\":\"x\"}", None));
		p.finalize();

		let group = last_group(&p.cells);
		assert_eq!(group.count, 1);
		assert_eq!(group.tools, vec!["GREP"]);
		assert_eq!(group_rows(&p, 120).last().unwrap(), "[  1] ── 1 Tool called: GREP ──");
	}

	#[test]
	fn cycle_boundaries_goals_and_responses_flush_and_stay_visible() {
		let mut p = CompactProjection::new();
		p.append(&goal("fix the bug"));
		p.append(&tool_call(1, "READ", "c1"));
		p.append(&tool_result("c1", false));
		p.append(&event(HarnessEventType::ModelText, 1, "Working on it…", None));
		p.append(&tool_call(1, "PATCH", "c2"));
		p.append(&event(HarnessEventType::IterationStart, 2, "cycle 2/5 — task-1: fix the bug: plan text [budget 100/60000 tokens]", None));
		p.append(&tool_call(2, "BASH", "c3"));
		p.append(&event(HarnessEventType::RunSummary, 2, "done", None));
		p.finalize();

		let plain = group_rows(&p, 120);
		assert!(plain.iter().any(|row| row.contains("❯ goal fix the bug")));
		assert!(plain.iter().any(|row| row.contains("── 1 Tools called: READ ──") || row.contains("── 1 Tool called: READ ──")), "{plain:?}");
		assert!(plain.iter().any(|row| row.contains("Working on it")), "{plain:?}");
		assert!(plain.iter().any(|row| row.contains("── 1 Tool called: PATCH ──")), "{plain:?}");
		assert!(plain.iter().any(|row| row.contains("cycle 2/5") && row.contains("task-1: fix the bug")), "{plain:?}");
		assert!(plain.iter().any(|row| row.contains("── 1 Tool called: BASH ──")), "{plain:?}");
		assert!(plain.iter().any(|row| row.contains("run summary")), "{plain:?}");
		// one row per burst, never merged across boundaries
		assert_eq!(p.cells.iter().filter(|c| matches!(c, CompactCell::ToolGroup(_))).count(), 3);
	}

	#[test]
	fn never_merges_across_goals_or_run_end() {
		let mut p = CompactProjection::new();
		p.append(&tool_call(1, "READ", "c1"));
		p.append(&goal("second goal"));
		p.append(&tool_call(1, "BASH", "c2"));
		p.append(&TranscriptEntry::RunEnd(TranscriptRunEndEntry {
			at: String::new(),
			goal_id: "g".to_string(),
			iterations: 3,
			reason: crate::core::types::HarnessRunReason::Completed,
		}));
		p.finalize();

		let plain = group_rows(&p, 120);
		assert_eq!(p.cells.iter().filter(|c| matches!(c, CompactCell::ToolGroup(_))).count(), 2);
		assert!(plain.iter().any(|row| row.contains("second goal")));
		assert!(plain.iter().any(|row| row.contains("run completed after 3 cycles")), "{plain:?}");
	}

	#[test]
	fn loop_start_and_other_events_act_as_boundaries() {
		let mut p = CompactProjection::new();
		p.append(&tool_call(1, "READ", "c1"));
		p.append(&event(HarnessEventType::LoopStart, 1, "loop 2 starting", None));
		p.append(&tool_call(1, "BASH", "c2"));
		p.finalize();

		assert_eq!(p.cells.iter().filter(|c| matches!(c, CompactCell::ToolGroup(_))).count(), 2);
	}

	#[test]
	fn visible_errors_and_warnings_stay_visible() {
		let mut p = CompactProjection::new();
		p.append(&tool_call(1, "READ", "c1"));
		p.append(&event(HarnessEventType::RunWarning, 1, "rate limited, waiting 2s", None));
		p.append(&tool_call(1, "PATCH", "c2"));
		p.append(&TranscriptEntry::Error(crate::cli::transcript::TranscriptNoteEntry {
			at: String::new(),
			text: "boom".to_string(),
		}));
		p.finalize();

		let plain = group_rows(&p, 120);
		assert!(plain.iter().any(|row| row.contains("rate limited")));
		assert!(plain.iter().any(|row| row.contains("boom")));
	}

	#[test]
	fn orphan_tool_result_without_open_group_is_kept() {
		let mut p = CompactProjection::new();
		p.append(&event(HarnessEventType::ToolResult, 1, "READ: content", Some("READ")));
		p.finalize();

		assert!(matches!(p.cells.as_slice(), [CompactCell::Passthrough(TranscriptEntry::Event(_))]));
	}

	#[test]
	fn zero_tool_cycle_renders_only_boundaries() {
		let mut p = CompactProjection::new();
		p.append(&event(HarnessEventType::IterationStart, 1, "cycle 1/3 — task-1: think: no tools needed [budget 0/60000 tokens]", None));
		p.append(&event(HarnessEventType::ModelText, 1, "All done.", None));
		p.finalize();

		assert!(p.cells.iter().all(|c| matches!(c, CompactCell::Passthrough(_))));
		let plain = group_rows(&p, 120);
		assert!(plain.iter().any(|row| row.contains("cycle 1/3")));
		assert!(plain.iter().any(|row| row.contains("All done.")));
		assert!(!plain.iter().any(|row| row.contains("Tools called")));
	}

	#[test]
	fn unicode_rows_clip_to_narrow_widths() {
		let mut p = CompactProjection::new();
		p.append(&tool_call(1, "READ", "c1"));
		p.append(&tool_call(1, "PATCH", "c2"));
		p.finalize();

		for width in [10usize, 20, 40] {
			for row in p.cells.iter().flat_map(|c| render_compact_cell(c, width)) {
				assert!(string_width(&row) <= width, "width {width}: {row:?}");
			}
		}
	}

	#[test]
	fn transition_row_keeps_budget_when_it_fits_and_clips_unicode() {
		let entry = TranscriptEventEntry {
			at: String::new(),
			data: None,
			detail: "cycle 12/40 — task-3: écriture française: héllö wörld wïth ünïcödé änd löng pläning text [budget 123456/60000 tokens]".to_string(),
			goal_id: "g".to_string(),
			iteration: 12,
			kind: HarnessEventType::IterationStart,
		};

		let wide = strip_ansi(&render_cycle_transition(&entry, 200)[0]);
		assert!(wide.contains("cycle 12/40"));
		assert!(wide.contains("écriture"));
		assert!(wide.contains("[budget 123456/60000 tokens]"));

		let mid = strip_ansi(&render_cycle_transition(&entry, 60)[0]);
		assert!(!mid.contains("[budget"), "{mid:?}");
		assert!(mid.contains("cycle 12/40"), "{mid:?}");
		assert!(mid.ends_with('…'));

		let narrow = render_cycle_transition(&entry, 10)[0].clone();
		assert_eq!(string_width(&narrow), 10, "{narrow:?}");

		for width in [10usize, 24, 60, 200] {
			assert_eq!(render_cycle_transition(&entry, width).len(), 1);
		}
	}

	#[test]
	fn replay_matches_incremental_appends() {
		let entries = vec![
			goal("g1"),
			tool_call(1, "READ", "c1"),
			tool_result("c1", false),
			event(HarnessEventType::Inference, 1, "infer", None),
			tool_call(1, "BASH", "c2"),
			event(HarnessEventType::IterationStart, 2, "cycle 2/3 — task-1: g1: more [budget 5/60 tokens]", None),
			tool_call(2, "PATCH", "c3"),
			event(HarnessEventType::ModelText, 2, "finished", None),
		];

		let replayed = CompactProjection::rebuild(&entries).into_cells();
		let mut incremental = CompactProjection::new();

		for entry in &entries {
			incremental.append(entry);
		}
		incremental.finalize();

		assert_eq!(replayed, incremental.cells);
	}

	#[test]
	fn estimate_and_tail_budget_helpers_mirror_timeline() {
		let mut p = CompactProjection::new();
		p.append(&tool_call(1, "READ", "c1"));
		p.append(&goal("g"));
		p.finalize();

		// Groups are strictly ONE physical row at every width, even when the
		// name list is long enough to exceed the terminal: render_tool_group
		// clips in place instead of wrapping.
		assert_eq!(estimate_compact_rows(&p.cells[0]), 1);
		assert_eq!(estimate_compact_rows(&p.cells[1]), 2);
		assert_eq!(select_compact_tail_start(&p.cells, 40), 0);

		// A very tight budget drops older cells but keeps the newest.
		let mut many = CompactProjection::new();

		for i in 0..50 {
			many.append(&goal(&format!("goal number {i} that is long enough to take rows {i}")));
			many.append(&tool_call(1, "READ", &format!("c{i}")));
			many.finalize();
		}

		let start = select_compact_tail_start(&many.cells, 12);
		assert!(start > 0);
		assert!(start < many.cells.len());
	}

	// --- CompactEmitter: the app's actual display-state path (src/tui/app.rs
	// feeds absorb/rebuild/finalize and renders projection.active_group) ---

	fn render_finalized(emitter: &CompactEmitter, width: usize) -> Vec<String> {
		emitter
			.projection
			.cells
			.iter()
			.flat_map(|cell| render_compact_cell(cell, width))
			.map(|row| strip_ansi(&row))
			.collect()
	}

	fn live_row(emitter: &CompactEmitter, width: usize) -> String {
		match emitter.projection.active_group() {
			Some(group) => strip_ansi(&render_tool_group(group, width)[0]),
			None => String::new(),
		}
	}

	fn group_count(cells: &[CompactCell]) -> usize {
		cells.iter().filter(|c| matches!(c, CompactCell::ToolGroup(_))).count()
	}

	#[test]
	fn emitter_separate_batches_update_a_single_live_two_tool_row_in_place() {
		let mut em = CompactEmitter::new();
		let b1 = vec![tool_call(1, "READ", "c1")];
		let b2 = vec![
			tool_result("c1", false),
			event(HarnessEventType::Inference, 1, "21018 prompt, 864 completion", None),
		];
		let b3 = vec![tool_call(1, "PATCH", "c2")];
		let b4 = vec![tool_result("c2", false)];

		// Four separate event batches, like four Msg::HarnessEvent deliveries:
		// nothing finalizes, the one live row just grows 1 -> 2 tools.
		assert_eq!(em.absorb(&b1), Vec::<CompactCell>::new());
		assert_eq!(em.absorb(&b2), Vec::<CompactCell>::new());
		assert_eq!(em.absorb(&b3), Vec::<CompactCell>::new());
		assert_eq!(em.absorb(&b4), Vec::<CompactCell>::new());
		assert!(em.projection.is_active());
		assert!(em.projection.cells.is_empty());

		let live = live_row(&em, 120);
		assert!(live.contains("2 Tools called"), "{live:?}");
		assert!(live.contains("READ"), "{live:?}");
		assert!(live.contains("PATCH"), "{live:?}");
		// Raw payloads and telemetry never surface in the live row.
		assert!(!live.contains("{\"path\":\"x\"}"), "{live:?}");
		assert!(!live.contains("result text"), "{live:?}");
		assert!(!live.contains("21018"), "{live:?}");
	}

	#[test]
	fn emitter_cycle_boundary_finalizes_exactly_once_and_previews_next_task() {
		let mut em = CompactEmitter::new();
		em.absorb(&[tool_call(1, "READ", "c1")]);

		let finalized = em.absorb(&[event(
			HarnessEventType::IterationStart,
			2,
			"cycle 2/5 — task-2: wire the composer [budget 100/60000 tokens]",
			None,
		)]);
		assert_eq!(group_count(&finalized), 1);
		assert!(matches!(finalized.last(), Some(CompactCell::Passthrough(_))));

		// Exactly once: subsequent drains and a run-boundary finalize are no-ops.
		assert!(em.drain().is_empty());
		assert!(em.finalize().is_empty());

		let rows = render_finalized(&em, 120);
		assert!(rows.iter().any(|row| row.contains("cycle 2/5")), "{rows:?}");
		assert!(rows.iter().any(|row| row.contains("task-2: wire the composer")), "{rows:?}");
	}

	#[test]
	fn emitter_goal_and_response_text_stay_visible_across_batches() {
		let mut em = CompactEmitter::new();
		em.absorb(&[goal("fix the flaky test")]);
		em.absorb(&[tool_call(1, "READ", "c1"), tool_result("c1", false)]);
		let finalized = em.absorb(&[event(
			HarnessEventType::ModelText,
			1,
			"The flake was a stale cache; fixed.",
			None,
		)]);

		assert_eq!(group_count(&finalized), 1);
		assert!(!em.projection.is_active());

		let rows = render_finalized(&em, 120);
		assert!(rows.iter().any(|row| row.contains("fix the flaky test")), "{rows:?}");
		assert!(rows.iter().any(|row| row.contains("stale cache")), "{rows:?}");
		assert!(
			rows.iter().any(|row| row.contains("called") && row.contains("READ")),
			"{rows:?}"
		);
	}

	#[test]
	fn emitter_live_replay_and_session_switch_produce_equivalent_output() {
		let entries = vec![
			goal("g1"),
			tool_call(1, "READ", "c1"),
			tool_result("c1", false),
			event(HarnessEventType::Inference, 1, "tok", None),
			tool_call(1, "BASH", "c2"),
			event(HarnessEventType::IterationStart, 2, "cycle 2/3 — task-1: g1: more [budget 5/60 tokens]", None),
			tool_call(2, "PATCH", "c3"),
			tool_result("c3", false), // trailing group stays open
		];

		// Live path: one entry per batch, exactly as the event loop delivers.
		let mut live = CompactEmitter::new();
		let mut live_finalized = Vec::new();
		for entry in &entries {
			live_finalized.extend(live.absorb(std::slice::from_ref(entry)));
		}

		// Replay/switch path: the same history rebuilt in one shot.
		let mut switched = CompactEmitter::new();
		switched.rebuild(&entries);

		assert_eq!(live.projection.cells, switched.projection.cells);
		// Each visible boundary finalized exactly one cell during the live feed:
		// goal passthrough, the READ+BASH group, then the IterationStart row.
		assert_eq!(group_count(&live_finalized), 1);
		assert!(matches!(live_finalized.first(), Some(CompactCell::Passthrough(_))));
		assert_eq!(live.projection.is_active(), switched.projection.is_active());
		assert_eq!(live_row(&live, 100), live_row(&switched, 100));
		assert_eq!(live.finalize(), switched.finalize());
		assert_eq!(live.projection.cells, switched.projection.cells);
		assert!(!live.projection.is_active());
		assert!(!switched.projection.is_active());

		// Compact view never re-reveals raw tool payloads or results.
		let rows = render_finalized(&live, 120);
		assert!(!rows.iter().any(|row| row.contains("{\"path\":\"x\"}")), "{rows:?}");
		assert!(!rows.iter().any(|row| row.contains("result text")), "{rows:?}");
	}

	#[test]
	fn emitter_resize_tail_budget_uses_finalized_cells_only() {
		let mut em = CompactEmitter::new();
		for i in 0..6 {
			em.absorb(&[
				event(HarnessEventType::IterationStart, 1, &format!("cycle {i}/6 — task-1: many [budget 0/60 tokens]"), None),
				tool_call(1, "READ", &format!("c{i}")),
			]);
			em.finalize();
		}
		// One group still open: drawn by the live region, never scrollback.
		em.absorb(&[tool_call(1, "BASH", "open")]);

		let finalized = em.projection.cells.clone();
		assert_eq!(group_count(&finalized), 6);
		assert!(em.drain().is_empty());
		assert!(em.projection.is_active());

		let start = select_compact_tail_start(&finalized, 10);
		assert!(start > 0, "tight budget must drop older cells");
		assert!(start < finalized.len());
	}

	#[test]
	fn emitter_error_and_end_boundaries_flush_the_open_group_once() {
		// Error note boundary.
		let mut em = CompactEmitter::new();
		em.absorb(&[tool_call(1, "READ", "c1")]);
		let finalized = em.absorb(&[TranscriptEntry::Error(crate::cli::transcript::TranscriptNoteEntry {
			at: String::new(),
			text: "boom".to_string(),
		})]);
		assert_eq!(group_count(&finalized), 1);
		assert!(em.finalize().is_empty());

		// Warning boundary (cancellation path paints the same way).
		let mut em2 = CompactEmitter::new();
		em2.absorb(&[tool_call(1, "PATCH", "c1")]);
		let finalized2 = em2.absorb(&[event(HarnessEventType::RunWarning, 1, "cancelled", None)]);
		assert_eq!(group_count(&finalized2), 1);

		// Silent cancellation path: finish_run's finalize settles the row.
		let mut em3 = CompactEmitter::new();
		em3.absorb(&[tool_call(1, "BASH", "c1")]);
		let settled = em3.finalize();
		assert_eq!(group_count(&settled), 1);
		assert!(em3.finalize().is_empty());
		assert!(!em3.projection.is_active());
	}

	#[test]
	fn emitter_many_tools_stay_on_one_row_at_every_width() {
		let mut em = CompactEmitter::new();
		let names = ["READ", "PATCH", "BASH", "GREP", "FETCH", "VERIFY", "DIR", "EDIT", "WRITE", "LS", "TEST", "GIT", "NOTE", "PLAN", "SPAWN"];
		let batch: Vec<TranscriptEntry> = names
			.iter()
			.enumerate()
			.map(|(i, name)| tool_call(1, name, &format!("c{i}")))
			.collect();
		em.absorb(&batch);
		em.finalize();

		let group = match em
			.projection
			.cells
			.iter()
			.find(|c| matches!(c, CompactCell::ToolGroup(_)))
		{
			Some(CompactCell::ToolGroup(group)) => group,
			other => panic!("expected a finalized tool group, got {other:?}"),
		};
		assert_eq!(estimate_compact_rows(&CompactCell::ToolGroup(group.clone())), 1);
		let wide = strip_ansi(&render_tool_group(group, 200)[0]);
		assert!(wide.contains("15 Tools called"), "{wide:?}");

		for width in [20usize, 40, 80, 200] {
			let rows = render_tool_group(group, width);
			assert_eq!(rows.len(), 1, "width {width}");
			assert!(string_width(&rows[0]) <= width, "width {width}: {:?}", strip_ansi(&rows[0]));
		}
	}

	#[test]
	fn emitter_unicode_tool_names_clip_on_narrow_rows() {
		let mut em = CompactEmitter::new();
		em.absorb(&[
			tool_call(1, "ÉCRITURE", "c1"),
			tool_call(1, "日本語ツール", "c2"),
			tool_call(1, "naïve-🛠️", "c3"),
		]);
		em.finalize();

		let group = match em
			.projection
			.cells
			.iter()
			.find(|c| matches!(c, CompactCell::ToolGroup(_)))
		{
			Some(CompactCell::ToolGroup(group)) => group,
			other => panic!("expected a finalized tool group, got {other:?}"),
		};
		for width in [10usize, 24, 60] {
			let rows = render_tool_group(group, width);
			assert_eq!(rows.len(), 1, "width {width}");
			assert!(string_width(&rows[0]) <= width, "width {width}: {:?}", strip_ansi(&rows[0]));
		}

		// The open live-row path is equally width-safe.
		let mut open = CompactEmitter::new();
		open.absorb(&[tool_call(1, "日本語ツール", "c9")]);
		for width in [10usize, 24] {
			let rows = open.projection.active_group().map(|g| render_tool_group(g, width)).unwrap();
			assert_eq!(rows.len(), 1, "width {width}");
			assert!(string_width(&rows[0]) <= width, "width {width}: {:?}", strip_ansi(&rows[0]));
		}
	}
}
