//! Compact TUI projection of the raw transcript timeline (presentation-only).
//!
//! The full transcript keeps every event; this layer folds back-to-back tool
//! activity within one goal cycle into a single summary such as
//! `● Read 3 files, made 2 edits, ran 4 commands` (with the latest call's
//! path or command under it) so the TUI shows user goals and model responses
//! without per-tool chatter. Logs, `dripw` and headless
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
    /// Folded `● Read 3 files, ran 1 command` summary.
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
    /// Calls per tool, parallel to `tools`.
    pub tool_counts: Vec<usize>,
    /// What the most recent call was about — a path, a command, a search
    /// pattern — or empty when its input names nothing readable.
    pub last_detail: String,
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
                    tools: Vec::new(),
                    tool_counts: Vec::new(),
                    last_detail: String::new(),
                    failed: 0,
                    iteration: event.iteration,
                    source: TranscriptEntry::Event(event.clone()),
                });
                let group = self.open.as_mut().unwrap();
                let name = event_tool_name(event);
                match group.tools.iter().position(|tool| *tool == name) {
                    Some(index) => group.tool_counts[index] += 1,
                    None => {
                        group.tools.push(name);
                        group.tool_counts.push(1);
                    }
                }
                group.last_detail = tool_call_detail(event);
                group.count += 1;
            }
            HarnessEventType::ToolResult => {
                // Absorbed into the open group; counted once (as its ToolCall).
                if self.open.is_none() {
                    // Result without a seen call (e.g. replay gap): render
                    // as a normal event row rather than losing it.
                    self.flush_active();
                    self.cells
                        .push(CompactCell::Passthrough(TranscriptEntry::Event(
                            event.clone(),
                        )));
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
                self.cells
                    .push(CompactCell::Passthrough(TranscriptEntry::Event(
                        event.clone(),
                    )));
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
        Self {
            emitted: 0,
            projection: CompactProjection::new(),
        }
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

/// What a ToolCall was about, from its input (`detail` is `NAME {json}`): the
/// shell command, the file path, the search pattern or the URL. Empty when the
/// input names none of those — raw JSON arguments are never shown.
fn tool_call_detail(event: &TranscriptEventEntry) -> String {
    let Some(input) = event
        .detail
        .split_once(' ')
        .and_then(|(_, input)| serde_json::from_str::<serde_json::Value>(input).ok())
    else {
        return String::new();
    };
    let text = |keys: &[&str]| {
        keys.iter()
            .find_map(|key| input.get(*key).and_then(|value| value.as_str()))
            .map(|value| value.lines().next().unwrap_or_default().trim().to_string())
            .filter(|value| !value.is_empty())
    };
    if let Some(command) = text(&["command", "cmd"]) {
        return format!("$ {command}");
    }
    if let Some(path) = text(&["path", "file_path", "file"]) {
        return path;
    }
    // PATCH carries its targets as a list.
    if let Some(files) = input.get("files").and_then(|files| files.as_array()) {
        let mut paths: Vec<&str> = Vec::new();
        for path in files.iter().filter_map(|file| file.get("path").and_then(|path| path.as_str())) {
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
        if let Some(first) = paths.first() {
            return match paths.len() {
                1 => first.to_string(),
                count => format!("{first} +{} more", count - 1),
            };
        }
    }
    if let Some(pattern) = text(&["pattern", "query"]) {
        return format!("\"{pattern}\"");
    }
    text(&["url"]).unwrap_or_default()
}

/// The phrase a tool's calls fold into: tools doing the same kind of work
/// share one (BASH and CHECK both "ran N commands").
fn tool_phrase(tool: &str, count: usize) -> (&'static str, String) {
    let plural = |one: &str, many: &str| if count == 1 { one.to_string() } else { many.to_string() };
    match tool {
        "READ" | "REFERENCE" => ("read", format!("read {count} {}", plural("file", "files"))),
        "PATCH" => ("edit", format!("made {count} {}", plural("edit", "edits"))),
        "BASH" | "BASH_ASYNC" | "CHECK" | "VERIFY" | "START_DEV" => {
            ("run", format!("ran {count} {}", plural("command", "commands")))
        }
        "GREP" | "DIR" => ("search", format!("searched {count} {}", plural("time", "times"))),
        "FETCH" => ("fetch", format!("fetched {count} {}", plural("page", "pages"))),
        _ => ("", format!("used {tool}{}", if count == 1 { String::new() } else { format!(" ×{count}") })),
    }
}

/// `Read 3 files, made 2 edits, ran 4 commands`, kinds in first-seen order.
fn tool_group_summary(group: &ToolGroupCell) -> String {
    let mut kinds: Vec<(&'static str, &str, usize)> = Vec::new();
    for (tool, count) in group.tools.iter().zip(&group.tool_counts) {
        let (kind, _) = tool_phrase(tool, *count);
        match kinds.iter_mut().find(|(seen, _, _)| !kind.is_empty() && *seen == kind) {
            Some((_, _, total)) => *total += count,
            None => kinds.push((kind, tool, *count)),
        }
    }
    let summary = kinds
        .iter()
        .map(|(_, tool, count)| tool_phrase(tool, *count).1)
        .collect::<Vec<_>>()
        .join(", ");
    let mut chars = summary.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => summary,
    }
}

fn clip_row(plain: String, width: usize) -> String {
    if width == 0 || string_width(&plain) <= width {
        return plain;
    }
    let mut clipped: String = plain.chars().take(width.saturating_sub(1)).collect();
    while string_width(&clipped) > width.saturating_sub(1) {
        clipped.pop();
    }
    clipped.push('\u{2026}');
    clipped
}

/// Render a tool group as its painted rows: `● Read 3 files, ran 1 command`
/// with ` · N failed` when calls failed, then `  ⎿ <path or command>` for the
/// most recent call when its input names one. Clipping happens on the plain
/// text BEFORE painting (the failed suffix is dropped first), so ANSI escapes
/// stay balanced and each row occupies exactly one line.
pub fn render_tool_group(group: &ToolGroupCell, width: usize) -> Vec<String> {
    let summary = format!("\u{25cf} {}", tool_group_summary(group));
    let failed = if group.failed > 0 {
        format!(" \u{b7} {} failed", group.failed)
    } else {
        String::new()
    };
    let fits = width == 0 || string_width(&format!("{summary}{failed}")) <= width;
    let head = if fits {
        let (bullet, rest) = summary.split_at('\u{25cf}'.len_utf8());
        format!("{}{}{}", c::accent(bullet), c::white(rest), c::red(&failed))
    } else {
        let clipped = clip_row(summary, width);
        match clipped.strip_prefix('\u{25cf}') {
            Some(rest) => format!("{}{}", c::accent("\u{25cf}"), c::white(rest)),
            None => c::white(&clipped),
        }
    };
    let mut rows = vec![head];
    if !group.last_detail.is_empty() {
        rows.push(c::dim(&clip_row(format!("  \u{23bf} {}", group.last_detail), width)));
    }
    rows
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
        CompactCell::ToolGroup(group) => 1 + usize::from(!group.last_detail.is_empty()),
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

/// A transient notice as its painted `● text` row: a coloured bullet, then
/// the text clipped (before painting) so the row always occupies one line.
fn notice_row(kind: HarnessEventType, text: &str, width: usize, text_paint: fn(&str) -> String) -> Vec<String> {
    let mut chars = text.chars();
    let text: String = match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    };
    let plain = clip_row(format!("\u{25cf} {text}"), width);
    let bullet = crate::tui::theme::event_paint(kind);
    vec![match plain.strip_prefix('\u{25cf}') {
        Some(rest) => format!("{}{}", bullet("\u{25cf}"), text_paint(rest)),
        None => text_paint(&plain),
    }]
}

/// Cycle-transition preview row painted from an IterationStart detail
/// ("cycle 2/5 — task-3: Title: plan text [budget 1234/60000 tokens]") in the
/// same `●` style as the tool summary: `● Cycle 2/5 — task-3: Title: plan text`.
/// Whitespace-flattened, width-aware: the trailing budget indicator is kept
/// only while the whole row fits; the body is then hard-clipped so the row
/// always occupies exactly one line.
pub fn render_cycle_transition(event: &TranscriptEventEntry, width: usize) -> Vec<String> {
    let flat = event
        .detail
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let (body, budget) = match flat.rfind(" [") {
        Some(start) if flat.ends_with(']') => (&flat[..start], Some(&flat[start..])),
        _ => (flat.as_str(), None),
    };
    // Keep the budget indicator only while the whole row fits; otherwise the
    // planning text is the part worth reading.
    let text = match budget {
        Some(budget) if width == 0 || string_width(&format!("\u{25cf} {body}{budget}")) <= width => {
            format!("{body}{budget}")
        }
        _ => body.to_string(),
    };
    notice_row(event.kind, &text, width, c::white)
}

/// One transient event on the activity block above the working line, in the
/// `●` style every row there shares. A cycle transition keeps its preview, a
/// loop start is cut down to `● Loop 3 — task-1: Title` (its skill, plan and
/// tool lists stay in the transcript), anything else shows its detail on one
/// clipped row.
pub fn render_activity_notice(event: &TranscriptEventEntry, width: usize) -> Vec<String> {
    let flat = event
        .detail
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    match event.kind {
        HarnessEventType::IterationStart => render_cycle_transition(event, width),
        HarnessEventType::LoopStart => {
            let head = flat.split(" [").next().unwrap_or_default();
            let text = match flat.rsplit_once(" \u{2014} ") {
                Some((_, what)) if !head.contains(" \u{2014} ") => format!("{head} \u{2014} {what}"),
                _ => head.to_string(),
            };
            notice_row(event.kind, &text, width, c::dim)
        }
        _ => notice_row(event.kind, &flat, width, c::dim),
    }
}

/// What an IterationStart detail says the run is doing, for the working line:
/// `(activity, cycle)` — ("Working on Fix the parser", "2/5") for
/// "cycle 2/5 — task-3: Fix the parser [run 4/20]", ("Planning", "1/3") for
/// "cycle 1/3 — planning".
pub fn parse_cycle_detail(detail: &str) -> (Option<String>, Option<String>) {
    let flat = detail.split_whitespace().collect::<Vec<_>>().join(" ");
    // Only the harness's own trailing indicators are cut; a title may
    // itself end in brackets.
    let body = match flat.rfind(" [run ").or_else(|| flat.rfind(" [budget ")) {
        Some(start) if flat.ends_with(']') => &flat[..start],
        _ => flat.as_str(),
    };
    let (head, what) = match body.split_once(" \u{2014} ") {
        Some((head, what)) => (head, Some(what.trim())),
        None => (body, None),
    };
    let cycle = head
        .strip_prefix("cycle ")
        .map(|cycle| cycle.trim().to_string())
        .filter(|cycle| !cycle.is_empty());
    let activity = what.filter(|what| !what.is_empty()).map(|what| {
        match what.split_once(": ") {
            Some((id, title)) if !id.contains(' ') && !title.trim().is_empty() => {
                format!("Working on {}", title.trim())
            }
            _ => {
                let mut chars = what.chars();
                match chars.next() {
                    Some(first) => first.to_uppercase().chain(chars).collect(),
                    None => String::new(),
                }
            }
        }
    });
    (activity, cycle)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::transcript::{
        TranscriptEventEntry, TranscriptGoalEntry, TranscriptRunEndEntry,
    };
    use crate::watch::ansi::strip_ansi;

    fn event(
        kind: HarnessEventType,
        iteration: i64,
        detail: &str,
        tool_name: Option<&str>,
    ) -> TranscriptEntry {
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
        assert_eq!(
            group_rows(&p, 120),
            vec!["● Read 2 files, made 1 edit, ran 1 command", "  ⎿ x"]
        );
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
            group_rows(&p, 120),
            vec!["● Read 1 file, ran 1 command, fetched 1 page · 2 failed", "  ⎿ x"]
        );

        // Narrow terminal: the failed suffix is dropped before the summary
        // is hard-clipped, and the summary still takes exactly one row.
        let rows = group_rows(&p, 40);
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert!(string_width(rows[0].as_str()) <= 40, "{rows:?}");
        assert!(rows[0].starts_with("● Read 1 file, ran 1 command"), "{rows:?}");
        assert!(!rows[0].contains("failed"), "{rows:?}");
    }

    #[test]
    fn interleaved_inference_telemetry_does_not_break_the_group() {
        let mut p = CompactProjection::new();
        p.append(&tool_call(1, "READ", "c1"));
        p.append(&event(
            HarnessEventType::Inference,
            1,
            "z-ai/glm — 21018 prompt",
            None,
        ));
        p.append(&tool_result("c1", false));
        p.append(&event(
            HarnessEventType::Inference,
            1,
            "another infer row",
            None,
        ));
        p.append(&tool_call(1, "PATCH", "c2"));
        p.finalize();

        assert_eq!(p.cells.len(), 1, "{:?}", p.cells);
        assert_eq!(last_group(&p.cells).count, 2);
    }

    #[test]
    fn missing_tool_metadata_falls_back_to_detail() {
        let mut p = CompactProjection::new();
        p.append(&event(
            HarnessEventType::ToolCall,
            1,
            "GREP {\"pattern\":\"x\"}",
            None,
        ));
        p.finalize();

        let group = last_group(&p.cells);
        assert_eq!(group.count, 1);
        assert_eq!(group.tools, vec!["GREP"]);
        assert_eq!(
            group_rows(&p, 120),
            vec!["● Searched 1 time", "  ⎿ \"x\""]
        );
    }

    #[test]
    fn cycle_boundaries_goals_and_responses_flush_and_stay_visible() {
        let mut p = CompactProjection::new();
        p.append(&goal("fix the bug"));
        p.append(&tool_call(1, "READ", "c1"));
        p.append(&tool_result("c1", false));
        p.append(&event(
            HarnessEventType::ModelText,
            1,
            "Working on it…",
            None,
        ));
        p.append(&tool_call(1, "PATCH", "c2"));
        p.append(&event(
            HarnessEventType::IterationStart,
            2,
            "cycle 2/5 — task-1: fix the bug: plan text [budget 100/60000 tokens]",
            None,
        ));
        p.append(&tool_call(2, "BASH", "c3"));
        p.append(&event(HarnessEventType::RunSummary, 2, "done", None));
        p.finalize();

        let plain = group_rows(&p, 120);
        assert!(plain.iter().any(|row| row.contains("❯ goal fix the bug")));
        assert!(
            plain
                .iter()
                .any(|row| row == "● Read 1 file"),
            "{plain:?}"
        );
        assert!(
            plain.iter().any(|row| row.contains("Working on it")),
            "{plain:?}"
        );
        assert!(
            plain
                .iter()
                .any(|row| row == "● Made 1 edit"),
            "{plain:?}"
        );
        assert!(
            plain
                .iter()
                .any(|row| row.contains("cycle 2/5") && row.contains("task-1: fix the bug")),
            "{plain:?}"
        );
        assert!(
            plain
                .iter()
                .any(|row| row == "● Ran 1 command"),
            "{plain:?}"
        );
        assert!(
            plain.iter().any(|row| row == "done"),
            "{plain:?}"
        );
        // one row per burst, never merged across boundaries
        assert_eq!(
            p.cells
                .iter()
                .filter(|c| matches!(c, CompactCell::ToolGroup(_)))
                .count(),
            3
        );
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
            duration_ms: None,
            tasks_done: None,
            tasks_total: None,
        }));
        p.finalize();

        let plain = group_rows(&p, 120);
        assert_eq!(
            p.cells
                .iter()
                .filter(|c| matches!(c, CompactCell::ToolGroup(_)))
                .count(),
            2
        );
        assert!(plain.iter().any(|row| row.contains("second goal")));
        assert!(
            plain
                .iter()
                .any(|row| row == "✻ Worked · 3 cycles"),
            "{plain:?}"
        );
    }

    #[test]
    fn loop_start_and_other_events_act_as_boundaries() {
        let mut p = CompactProjection::new();
        p.append(&tool_call(1, "READ", "c1"));
        p.append(&event(
            HarnessEventType::LoopStart,
            1,
            "loop 2 starting",
            None,
        ));
        p.append(&tool_call(1, "BASH", "c2"));
        p.finalize();

        assert_eq!(
            p.cells
                .iter()
                .filter(|c| matches!(c, CompactCell::ToolGroup(_)))
                .count(),
            2
        );
    }

    #[test]
    fn visible_errors_and_warnings_stay_visible() {
        let mut p = CompactProjection::new();
        p.append(&tool_call(1, "READ", "c1"));
        p.append(&event(
            HarnessEventType::RunWarning,
            1,
            "rate limited, waiting 2s",
            None,
        ));
        p.append(&tool_call(1, "PATCH", "c2"));
        p.append(&TranscriptEntry::Error(
            crate::cli::transcript::TranscriptNoteEntry {
                at: String::new(),
                text: "boom".to_string(),
            },
        ));
        p.finalize();

        let plain = group_rows(&p, 120);
        assert!(plain.iter().any(|row| row.contains("rate limited")));
        assert!(plain.iter().any(|row| row.contains("boom")));
    }

    #[test]
    fn orphan_tool_result_without_open_group_is_kept() {
        let mut p = CompactProjection::new();
        p.append(&event(
            HarnessEventType::ToolResult,
            1,
            "READ: content",
            Some("READ"),
        ));
        p.finalize();

        assert!(matches!(
            p.cells.as_slice(),
            [CompactCell::Passthrough(TranscriptEntry::Event(_))]
        ));
    }

    #[test]
    fn zero_tool_cycle_renders_only_boundaries() {
        let mut p = CompactProjection::new();
        p.append(&event(
            HarnessEventType::IterationStart,
            1,
            "cycle 1/3 — task-1: think: no tools needed [budget 0/60000 tokens]",
            None,
        ));
        p.append(&event(HarnessEventType::ModelText, 1, "All done.", None));
        p.finalize();

        assert!(p
            .cells
            .iter()
            .all(|c| matches!(c, CompactCell::Passthrough(_))));
        let plain = group_rows(&p, 120);
        assert!(plain.iter().any(|row| row.contains("cycle 1/3")));
        assert!(plain.iter().any(|row| row.contains("All done.")));
        assert!(!plain.iter().any(|row| row.starts_with('●')));
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
        assert!(wide.starts_with("● Cycle 12/40"), "{wide:?}");
        assert!(wide.contains("écriture"));
        assert!(wide.contains("[budget 123456/60000 tokens]"));

        let mid = strip_ansi(&render_cycle_transition(&entry, 60)[0]);
        assert!(!mid.contains("[budget"), "{mid:?}");
        assert!(mid.starts_with("● Cycle 12/40"), "{mid:?}");
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
            event(
                HarnessEventType::IterationStart,
                2,
                "cycle 2/3 — task-1: g1: more [budget 5/60 tokens]",
                None,
            ),
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

        // A group is its summary row plus the latest call's detail row at
        // every width, even when the summary is long enough to exceed the
        // terminal: render_tool_group clips in place instead of wrapping.
        assert_eq!(estimate_compact_rows(&p.cells[0]), 2);
        assert_eq!(estimate_compact_rows(&p.cells[1]), 2);
        assert_eq!(select_compact_tail_start(&p.cells, 40), 0);

        // A very tight budget drops older cells but keeps the newest.
        let mut many = CompactProjection::new();

        for i in 0..50 {
            many.append(&goal(&format!(
                "goal number {i} that is long enough to take rows {i}"
            )));
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
        cells
            .iter()
            .filter(|c| matches!(c, CompactCell::ToolGroup(_)))
            .count()
    }

    #[test]
    fn emitter_separate_batches_update_a_single_live_two_tool_row_in_place() {
        let mut em = CompactEmitter::new();
        let b1 = vec![tool_call(1, "READ", "c1")];
        let b2 = vec![
            tool_result("c1", false),
            event(
                HarnessEventType::Inference,
                1,
                "21018 prompt, 864 completion",
                None,
            ),
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
        assert_eq!(live, "● Read 1 file, made 1 edit");
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
        assert!(matches!(
            finalized.last(),
            Some(CompactCell::Passthrough(_))
        ));

        // Exactly once: subsequent drains and a run-boundary finalize are no-ops.
        assert!(em.drain().is_empty());
        assert!(em.finalize().is_empty());

        let rows = render_finalized(&em, 120);
        assert!(rows.iter().any(|row| row.contains("cycle 2/5")), "{rows:?}");
        assert!(
            rows.iter()
                .any(|row| row.contains("task-2: wire the composer")),
            "{rows:?}"
        );
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
        assert!(
            rows.iter().any(|row| row.contains("fix the flaky test")),
            "{rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("stale cache")),
            "{rows:?}"
        );
        assert!(
            rows.iter()
                .any(|row| row.starts_with("● Read")),
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
            event(
                HarnessEventType::IterationStart,
                2,
                "cycle 2/3 — task-1: g1: more [budget 5/60 tokens]",
                None,
            ),
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
        assert!(matches!(
            live_finalized.first(),
            Some(CompactCell::Passthrough(_))
        ));
        assert_eq!(live.projection.is_active(), switched.projection.is_active());
        assert_eq!(live_row(&live, 100), live_row(&switched, 100));
        assert_eq!(live.finalize(), switched.finalize());
        assert_eq!(live.projection.cells, switched.projection.cells);
        assert!(!live.projection.is_active());
        assert!(!switched.projection.is_active());

        // Compact view never re-reveals raw tool payloads or results.
        let rows = render_finalized(&live, 120);
        assert!(
            !rows.iter().any(|row| row.contains("{\"path\":\"x\"}")),
            "{rows:?}"
        );
        assert!(
            !rows.iter().any(|row| row.contains("result text")),
            "{rows:?}"
        );
    }

    #[test]
    fn emitter_resize_tail_budget_uses_finalized_cells_only() {
        let mut em = CompactEmitter::new();
        for i in 0..6 {
            em.absorb(&[
                event(
                    HarnessEventType::IterationStart,
                    1,
                    &format!("cycle {i}/6 — task-1: many [budget 0/60 tokens]"),
                    None,
                ),
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
        let finalized = em.absorb(&[TranscriptEntry::Error(
            crate::cli::transcript::TranscriptNoteEntry {
                at: String::new(),
                text: "boom".to_string(),
            },
        )]);
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
        let names = [
            "READ", "PATCH", "BASH", "GREP", "FETCH", "VERIFY", "DIR", "EDIT", "WRITE", "LS",
            "TEST", "GIT", "NOTE", "PLAN", "SPAWN",
        ];
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
        assert_eq!(
            estimate_compact_rows(&CompactCell::ToolGroup(group.clone())),
            2
        );
        let wide = strip_ansi(&render_tool_group(group, 200)[0]);
        assert!(
            wide.starts_with("● Read 1 file, made 1 edit, ran 2 commands, searched 2 times, fetched 1 page, used EDIT"),
            "{wide:?}"
        );

        for width in [20usize, 40, 80, 200] {
            let rows = render_tool_group(group, width);
            assert_eq!(rows.len(), 2, "width {width}");
            for row in &rows {
                assert!(string_width(row) <= width, "width {width}: {:?}", strip_ansi(row));
            }
        }
    }

    #[test]
    fn tool_group_detail_row_names_the_latest_call_and_never_shows_raw_json() {
        let call = |detail: &str| {
            TranscriptEntry::Event(TranscriptEventEntry {
                at: String::new(),
                data: None,
                detail: detail.to_string(),
                goal_id: "g".to_string(),
                iteration: 1,
                kind: HarnessEventType::ToolCall,
            })
        };
        let detail_row = |detail: &str| {
            let mut p = CompactProjection::new();
            p.append(&call(detail));
            p.finalize();
            group_rows(&p, 60)
        };

        assert_eq!(
            detail_row("BASH {\"command\":\"cargo test --lib\\necho done\"}"),
            vec!["● Ran 1 command", "  ⎿ $ cargo test --lib"]
        );
        assert_eq!(
            detail_row("PATCH {\"files\":[{\"path\":\"src/a.rs\",\"find\":\"x\"},{\"path\":\"src/b.rs\"}]}"),
            vec!["● Made 1 edit", "  ⎿ src/a.rs +1 more"]
        );
        assert_eq!(
            detail_row("FETCH {\"url\":\"https://example.com/docs\"}"),
            vec!["● Fetched 1 page", "  ⎿ https://example.com/docs"]
        );
        // Input that names nothing readable (or is not JSON) leaves the
        // summary on its own rather than printing the arguments.
        assert_eq!(detail_row("MONITOR {\"waitMs\":5000}"), vec!["● Used MONITOR"]);
        assert_eq!(detail_row("READ not-json"), vec!["● Read 1 file"]);

        // Absurdly narrow terminals clip instead of panicking.
        for width in [1usize, 2, 3] {
            let mut p = CompactProjection::new();
            p.append(&call("BASH {\"command\":\"ls\"}"));
            p.finalize();
            assert_eq!(group_rows(&p, width).len(), 2, "width {width}");
        }

        // A long command is clipped to the terminal, never wrapped.
        let long = format!("BASH {{\"command\":\"{}\"}}", "x".repeat(200));
        let rows = detail_row(&long);
        assert_eq!(rows.len(), 2);
        assert!(string_width(rows[1].as_str()) <= 60 && rows[1].ends_with('…'), "{rows:?}");
    }

    #[test]
    fn cycle_detail_parses_into_the_working_line_parts() {
        assert_eq!(
            parse_cycle_detail("cycle 2/5 — task-3: Fix the parser [run 4/20]"),
            (Some("Working on Fix the parser".to_string()), Some("2/5".to_string()))
        );
        assert_eq!(
            parse_cycle_detail("cycle 1/3 — planning"),
            (Some("Planning".to_string()), Some("1/3".to_string()))
        );
        assert_eq!(
            parse_cycle_detail("cycle 3/3 — task-1: Title: with a colon [run 9/9 — budget exhausted after this cycle]"),
            (Some("Working on Title: with a colon".to_string()), Some("3/3".to_string()))
        );
        assert_eq!(
            parse_cycle_detail("cycle 1/2 — task-2: Fix the [bug] handler [legacy]"),
            (Some("Working on Fix the [bug] handler [legacy]".to_string()), Some("1/2".to_string()))
        );
        assert_eq!(parse_cycle_detail("something else"), (None, None));
    }

    #[test]
    fn cycle_transition_row_is_a_dot_row_without_the_iteration_block() {
        let event = TranscriptEventEntry {
            at: "2026-01-01T12:34:56.000Z".to_string(),
            data: None,
            detail: "cycle 2/5 — task-2: wire the composer [budget 100/60000 tokens]".to_string(),
            goal_id: "g".to_string(),
            iteration: 2,
            kind: HarnessEventType::IterationStart,
        };

        assert_eq!(
            strip_ansi(&render_cycle_transition(&event, 200)[0]),
            "● Cycle 2/5 — task-2: wire the composer [budget 100/60000 tokens]"
        );

        for width in [10usize, 24, 60, 200] {
            let rows = render_cycle_transition(&event, width);
            assert_eq!(rows.len(), 1, "width {width}");
            assert!(
                string_width(&rows[0]) <= width,
                "width {width}: {:?}",
                strip_ansi(&rows[0])
            );
        }
    }

    #[test]
    fn every_transient_notice_is_one_dot_row() {
        let notice = |kind: HarnessEventType, detail: &str, width: usize| {
            let rows = render_activity_notice(
                &TranscriptEventEntry {
                    at: "2026-01-01T12:34:56.000Z".to_string(),
                    data: None,
                    detail: detail.to_string(),
                    goal_id: "g".to_string(),
                    iteration: 4,
                    kind,
                },
                width,
            );
            assert_eq!(rows.len(), 1, "{rows:?}");
            strip_ansi(&rows[0])
        };

        assert_eq!(
            notice(HarnessEventType::IterationStart, "cycle 1/3 — replanning blocked tasks", 120),
            "● Cycle 1/3 — replanning blocked tasks"
        );
        assert_eq!(
            notice(
                HarnessEventType::ContextRefreshed,
                "context estimate for cycle 1: 38243 tokens across 6 categories",
                120
            ),
            "● Context estimate for cycle 1: 38243 tokens across 6 categories"
        );
        // A loop start names the loop and its task; the skill, plan and tool
        // lists never spill onto the block.
        assert_eq!(
            notice(
                HarnessEventType::LoopStart,
                "loop 3 [skills: verify-before-done] [tools: BASH_ASYNC, CHECK, DIR, FETCH, GREP, READ] [role: author] — task-1: Fix the parser",
                120
            ),
            "● Loop 3 — task-1: Fix the parser"
        );
        assert_eq!(notice(HarnessEventType::LoopStart, "loop 1 — planning", 120), "● Loop 1 — planning");
        // Long or multi-line details stay on one clipped row.
        let warning = notice(
            HarnessEventType::RunWarning,
            &format!("read-only nudge:\n{}", "nothing written ".repeat(20)),
            40,
        );
        assert!(warning.starts_with("● Read-only nudge: nothing written"), "{warning:?}");
        assert!(string_width(&warning) <= 40 && warning.ends_with('…'), "{warning:?}");
        assert!(!warning.contains('['), "{warning:?}");
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
            assert_eq!(rows.len(), 2, "width {width}");
            for row in &rows {
                assert!(string_width(row) <= width, "width {width}: {:?}", strip_ansi(row));
            }
        }

        // The open live-row path is equally width-safe.
        let mut open = CompactEmitter::new();
        open.absorb(&[tool_call(1, "日本語ツール", "c9")]);
        for width in [10usize, 24] {
            let rows = open
                .projection
                .active_group()
                .map(|g| render_tool_group(g, width))
                .unwrap();
            assert_eq!(rows.len(), 2, "width {width}");
            assert!(
                string_width(&rows[0]) <= width,
                "width {width}: {:?}",
                strip_ansi(&rows[0])
            );
        }
    }
}
