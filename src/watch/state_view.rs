// HarnessState → row cells for the dripw state pane.
//
// Mirrors the lci web app sidebar (web_ui/harness-sidebar.tsx): a goal/clock
// header plus four live sections — shared memory, warm context, last
// activation, and the telemetry watchlist — each keeping the empty-state hint
// the web UI shows. Markdown previews render as plain truncated text: this
// pane is a glance surface, not a markdown renderer.
//
// Pure: no I/O, no clock, no width. Rows are wrapped at a fixed width and
// render_pane re-fits plain rows to the actual pane width with an ellipsis.

use std::collections::HashSet;

use crate::core::types::HarnessState;
use crate::watch::ansi::{c, string_width};
use crate::watch::render::wrap_plain;
use crate::watch::transcript_view::RowCell;

/// Fixed wrapping width for multi-line plain rows (render_pane re-fits them).
const WRAP_WIDTH: usize = 48;
/// Most recent shared-memory notes shown, newest first.
const MEMORY_CAP: usize = 8;
/// Watchlist rows shown before the "+N more tracked" overflow hint.
const TELEMETRY_CAP: usize = 10;
/// Warm-context entries shown, oldest-first (the harness retires/re-ranks
/// the tail first, so the N most recent survive) capped to keep the later
/// sections visible in the fixed-height pane.
const WARM_CAP: usize = 8;

fn plain(text: impl Into<String>, color: fn(&str) -> String) -> RowCell {
    RowCell {
        text: text.into(),
        color: Some(color),
        selected: false,
        rich: false,
    }
}

fn dim(text: impl Into<String>) -> RowCell {
    plain(text, c::gray)
}

fn section_header(title: &str) -> RowCell {
    plain(title.to_uppercase(), c::accent)
}

/// Collapse a possibly-markdown value to one line of plain text and clip it.
/// An empty-state hint, wrapped so it never relies on render_pane's ellipsis.
fn hint(text: &str) -> Vec<RowCell> {
    word_wrap(text, WRAP_WIDTH).into_iter().map(dim).collect()
}

/// Sentence-wrap at word boundaries: wrap_plain is width-based and will
/// slice a word mid-run, which ruins exact-text assertions and readability.
fn word_wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let piece = if cur.is_empty() {
            word.to_string()
        } else {
            format!("{} {}", cur, word)
        };
        if piece.chars().count() > width && !cur.is_empty() {
            lines.push(std::mem::take(&mut cur));
            cur = word.to_string();
        } else {
            cur = piece;
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.is_empty() {
        vec![String::new()]
    } else {
        lines
    }
}

/// Clip a possibly-markdown value to `max` VISIBLE columns of plain text
/// (ansi::string_width — the same budget render_pane fits by), appending an
/// ellipsis when clipped.
fn plain_preview(text: &str, max: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if string_width(&flat) <= max {
        return flat;
    }
    let mut out = String::new();
    let mut w = 0usize;
    for ch in flat.chars() {
        // P2-2: keep the ellipsis cell in-budget too, so the result is never
        // wider than the caller's max even under re-fitting.
        if w + 1 > max.saturating_sub(1) {
            break;
        }
        let cw = string_width(&ch.to_string());
        if w + cw > max.saturating_sub(1) {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

/// Row cells for the dripw state pane, in top-to-bottom order.
pub fn build_state_rows(state: &HarnessState) -> Vec<RowCell> {
    let mut rows: Vec<RowCell> = Vec::new();

    // Header: the goal the harness is working on, then the harness clocks.
    for line in wrap_plain(&state.goal, WRAP_WIDTH) {
        rows.push(plain(line, c::accent_bold));
    }
    rows.push(dim(format!(
        "iteration {} · loop {}",
        state.iteration, state.r#loop
    )));
    rows.push(dim(String::new()));

    rows.extend(shared_memory_rows(state));
    rows.push(dim(String::new()));
    rows.extend(warm_context_rows(state));
    rows.push(dim(String::new()));
    rows.extend(last_activation_rows(state));
    rows.push(dim(String::new()));
    rows.extend(telemetry_watchlist_rows(state));
    rows
}

fn shared_memory_rows(state: &HarnessState) -> Vec<RowCell> {
    let mut rows = vec![section_header("Shared memory")];
    let notes = &state.memory;
    if notes.is_empty() {
        rows.extend(hint(
            "Nothing remembered yet. Durable facts the model saves with remember appear here.",
        ));
        return rows;
    }
    for note in notes.iter().rev().take(MEMORY_CAP) {
        rows.push(plain(
            format!("{} · saved @ cycle {}", note.id, note.created_at_iteration),
            c::white,
        ));
        for line in wrap_plain(&plain_preview(&note.text, 160), WRAP_WIDTH) {
            rows.push(if line.is_empty() {
                dim(String::new())
            } else {
                plain(line, c::white)
            });
        }
    }
    if notes.len() > MEMORY_CAP {
        rows.push(dim(format!("+{} more notes", notes.len() - MEMORY_CAP)));
    }
    rows
}

fn warm_context_rows(state: &HarnessState) -> Vec<RowCell> {
    let mut rows = vec![section_header("Warm context")];
    let entries = &state.promoted_context;
    if entries.is_empty() {
        rows.extend(hint("Nothing promoted yet. Tool results reached for in 2+ task loops get cached here so future loops skip the call."));
        return rows;
    }
    for entry in entries.iter().skip(entries.len().saturating_sub(WARM_CAP)) {
        let head = format!(
            "{} {}",
            entry.tool_name,
            plain_preview(&entry.input_preview, 80)
        );
        for line in wrap_plain(&head, WRAP_WIDTH) {
            rows.push(plain(line, c::yellow));
        }
        let mut meta = format!("ttl {}", entry.ttl);
        if entry.reinforcements > 0 {
            meta.push_str(&format!(" · ×{}", entry.reinforcements));
        }
        if entry.dynamic {
            meta.push_str(" · live");
        }
        if entry.last_failed == Some(true) {
            meta.push_str(" · failed");
        }
        rows.push(dim(meta));
    }
    let hidden = entries.len().saturating_sub(WARM_CAP);
    if hidden > 0 {
        rows.push(dim(format!("+{} more", hidden)));
    }
    rows
}

fn last_activation_rows(state: &HarnessState) -> Vec<RowCell> {
    let mut rows = vec![section_header("Last activation")];
    let Some(digest) = state.last_activation.as_ref() else {
        rows.extend(hint("Nothing yet. After each task loop, a digest of what it did and how it ended is fed into the next prompt."));
        return rows;
    };
    let mut head = format!("loop digest @ cycle {}", digest.iteration);
    if let Some(task_id) = &digest.task_id {
        head.push_str(&format!(" · {}", task_id));
    }
    if let Some(cycles) = digest.cycles {
        head.push_str(&format!(" · {} cycles", cycles));
    }
    rows.push(plain(head, c::cyan_bold));
    for action in &digest.actions {
        for line in wrap_plain(&plain_preview(action, 160), WRAP_WIDTH) {
            rows.push(if line.is_empty() {
                dim(String::new())
            } else {
                plain(line, c::white)
            });
        }
    }
    if !digest.outcome.is_empty() {
        for line in wrap_plain(&format!("→ {}", digest.outcome), WRAP_WIDTH) {
            rows.push(plain(line, c::green));
        }
    }
    rows
}

fn telemetry_watchlist_rows(state: &HarnessState) -> Vec<RowCell> {
    let mut rows = vec![section_header("Telemetry watchlist")];
    if state.telemetry.is_empty() {
        rows.extend(hint("No workspace tool calls tracked yet. Calls used in 2+ task loops get promoted into warm context."));
        return rows;
    }
    let promoted: HashSet<&str> = state
        .promoted_context
        .iter()
        .map(|entry| entry.key.as_str())
        .collect();
    let mut watched: Vec<_> = state
        .telemetry
        .values()
        .filter(|record| !promoted.contains(record.key.as_str()))
        .collect();
    if watched.is_empty() {
        rows.extend(hint(
            "Every tracked call has been promoted into warm context.",
        ));
        return rows;
    }
    watched.sort_by(|a, b| {
        b.iterations_used
            .len()
            .cmp(&a.iterations_used.len())
            .then(b.last_used_iteration.cmp(&a.last_used_iteration))
    });
    for record in watched.iter().take(TELEMETRY_CAP) {
        let head = format!(
            "{} {}",
            record.tool_name,
            plain_preview(&record.input_preview, 60)
        );
        for line in wrap_plain(&head, WRAP_WIDTH) {
            rows.push(plain(line, c::yellow));
        }
        rows.push(dim(format!(
            "{} act · {} calls",
            record.iterations_used.len(),
            record.call_count
        )));
    }
    let extra = watched.len().saturating_sub(TELEMETRY_CAP);
    if extra > 0 {
        rows.push(dim(format!("+{} more tracked", extra)));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::{
        HarnessActivationDigest, HarnessMemoryNote, PromotedContextEntry, ToolTelemetryRecord,
    };
    use indexmap::IndexMap;

    fn record(
        key: &str,
        preview: &str,
        loops: Vec<i64>,
        last_used: i64,
        calls: i64,
    ) -> ToolTelemetryRecord {
        ToolTelemetryRecord {
            call_count: calls,
            input_preview: preview.into(),
            iterations_used: loops,
            key: key.into(),
            last_failed: None,
            last_output: String::new(),
            last_used_iteration: last_used,
            raw_input: String::new(),
            reinforcements: 0,
            tool_name: "bash".into(),
        }
    }

    fn promoted(key: &str) -> PromotedContextEntry {
        PromotedContextEntry {
            dynamic: false,
            input_preview: String::new(),
            key: key.into(),
            last_failed: None,
            output: String::new(),
            promoted_at_iteration: 1,
            raw_input: String::new(),
            reinforcements: 0,
            tool_name: "bash".into(),
            ttl: 1,
        }
    }

    fn sample_state() -> HarnessState {
        let mut state = HarnessState {
            goal: "Build the dripw state pane".into(),
            iteration: 7,
            r#loop: 3,
            ..Default::default()
        };
        state.memory.push(HarnessMemoryNote {
            id: "note-1".into(),
            created_at_iteration: 2,
            text: "# fact\nthe answer is 42".into(),
        });
        state.promoted_context.push(PromotedContextEntry {
            dynamic: true,
            input_preview: "cargo test --watch".into(),
            key: "bash::cargo test --watch".into(),
            last_failed: None,
            output: String::new(),
            promoted_at_iteration: 6,
            raw_input: String::new(),
            reinforcements: 2,
            tool_name: "bash".into(),
            ttl: 5,
        });
        state.last_activation = Some(HarnessActivationDigest {
            actions: vec!["wrote src/watch/state_view.rs".into()],
            cycles: Some(3),
            iteration: 7,
            r#loop: Some(3),
            outcome: "completed: cargo test green".into(),
            task_id: Some("task-1".into()),
        });
        let mut telemetry: IndexMap<String, ToolTelemetryRecord> = IndexMap::new();
        telemetry.insert(
            "bash::cargo test".into(),
            record("bash::cargo test", "cargo test", vec![1, 2, 3], 7, 9),
        );
        telemetry.insert(
            "bash::git status".into(),
            record("bash::git status", "git status", vec![1], 5, 2),
        );
        telemetry.insert(
            "bash::cargo test --watch".into(),
            record(
                "bash::cargo test --watch",
                "already-promoted",
                vec![1, 2],
                8,
                4,
            ),
        );
        state.telemetry = telemetry;
        state
    }

    /// Collapse newlines/whitespace so wrapped hint lines still match.
    fn norm(text: &str) -> String {
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    fn joined(rows: &[RowCell]) -> String {
        rows.iter()
            .map(|row| row.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn empty_state_shows_lci_hints() {
        let out = joined(&build_state_rows(&HarnessState::default()));
        for hint in [
			"SHARED MEMORY",
			"WARM CONTEXT",
			"LAST ACTIVATION",
			"TELEMETRY WATCHLIST",
			"Nothing remembered yet. Durable facts the model saves with remember appear here.",
			"Nothing promoted yet. Tool results reached for in 2+ task loops get cached here so future loops skip the call.",
			"Nothing yet. After each task loop, a digest of what it did and how it ended is fed into the next prompt.",
			"No workspace tool calls tracked yet. Calls used in 2+ task loops get promoted into warm context.",
		] {
			assert!(norm(&out).contains(norm(hint).as_str()), "missing hint {hint:?}\n{out}");
		}
    }

    #[test]
    fn populated_state_mirrors_lci_sections() {
        let state = sample_state();
        let out = joined(&build_state_rows(&state));
        assert!(
            out.contains("Build the dripw state pane"),
            "goal header missing:\n{out}"
        );
        assert!(
            out.contains("iteration 7 · loop 3"),
            "clocks missing:\n{out}"
        );
        assert!(
            out.contains("note-1 · saved @ cycle 2"),
            "memory row missing:\n{out}"
        );
        assert!(
            out.contains("the answer is 42"),
            "memory text missing:\n{out}"
        );
        assert!(
            out.contains("cargo test --watch"),
            "warm preview missing:\n{out}"
        );
        assert!(out.contains("ttl 5"), "warm ttl missing:\n{out}");
        assert!(out.contains("×2"), "warm reinforcements missing:\n{out}");
        assert!(out.contains("live"), "warm live badge missing:\n{out}");
        assert!(out.contains("task-1"), "activation task id missing:\n{out}");
        assert!(
            out.contains("wrote src/watch/state_view.rs"),
            "activation action missing:\n{out}"
        );
        assert!(
            out.contains("cargo test green"),
            "activation outcome missing:\n{out}"
        );
        assert!(
            out.contains("3 act · 9 calls"),
            "telemetry top row missing:\n{out}"
        );
        assert!(
            out.find("3 act · 9 calls").unwrap() < out.find("1 act · 2 calls").unwrap(),
            "watchlist not sorted by loop count desc:\n{out}"
        );
        assert!(
            !out.contains("already-promoted"),
            "promoted key leaked into watchlist:\n{out}"
        );
        assert!(
            !out.contains("more tracked"),
            "no overflow for 2 candidates:\n{out}"
        );
    }

    #[test]
    fn memory_cap_keeps_last_eight_newest_first() {
        let mut state = HarnessState::default();
        for i in 1..=12 {
            state.memory.push(HarnessMemoryNote {
                id: format!("note-{i:02}"),
                created_at_iteration: i,
                text: format!("text {i}"),
            });
        }
        let out = joined(&build_state_rows(&state));
        assert!(
            out.contains("note-12 · saved @ cycle 12"),
            "newest note missing:\n{out}"
        );
        assert!(
            out.contains("note-05 · saved @ cycle 5"),
            "oldest kept note missing:\n{out}"
        );
        assert!(
            !out.contains("note-04 ·"),
            "note below the cap shown:\n{out}"
        );
        assert!(
            out.contains("+4 more notes"),
            "overflow hint missing:\n{out}"
        );
        assert!(
            out.find("note-12 ·").unwrap() < out.find("note-05 ·").unwrap(),
            "notes not newest-first:\n{out}"
        );
    }

    #[test]
    fn telemetry_caps_at_ten_and_hides_promoted() {
        let mut state = HarnessState::default();
        for i in 0..12 {
            state.telemetry.insert(
                format!("key-{i}"),
                record(
                    &format!("key-{i}"),
                    &format!("p{i:02}"),
                    vec![1, 2, 3],
                    i,
                    1,
                ),
            );
        }
        // Promote the highest-last-use record: if the promoted-key filter
        // breaks, p11 (its record preview, rendered nowhere else) leaks in.
        state.promoted_context.push(promoted("key-11"));
        let out = joined(&build_state_rows(&state));
        // 11 un-promoted candidates: 10 shown (p10..p01), +1 tracked (p00).
        assert!(
            out.contains("+1 more tracked"),
            "overflow hint wrong:\n{out}"
        );
        assert!(
            out.contains("p10"),
            "highest un-promoted row missing:\n{out}"
        );
        assert!(
            out.find("p10").unwrap() < out.find("p09").unwrap()
                && out.find("p09").unwrap() < out.find("p01").unwrap(),
            "watchlist not sorted by last use desc:\n{out}"
        );
        assert!(
            out.contains("p01") && !out.contains("p00"),
            "cap boundary wrong (p01 shown, p00 overflowed):\n{out}"
        );
        assert!(
            !out.contains("p11"),
            "promoted record leaked into watchlist:\n{out}"
        );
    }

    #[test]
    fn warm_context_capped_and_overflow_hinted() {
        let mut state = HarnessState::default();
        for i in 1..=12 {
            let preview = if i <= 4 {
                format!("dropped-{i}")
            } else {
                format!("kept-{}", i - 4)
            };
            state
                .promoted_context
                .push(crate::core::types::PromotedContextEntry {
                    dynamic: false,
                    input_preview: preview,
                    key: format!("k{}", i),
                    last_failed: None,
                    output: String::new(),
                    promoted_at_iteration: i,
                    raw_input: String::new(),
                    reinforcements: 0,
                    tool_name: "bash".into(),
                    ttl: i,
                });
        }
        let out = joined(&build_state_rows(&state));
        // The four oldest (dropped-1..4) fall below the cap; kept-1..kept-8
        // (i = 5..12) render in promotion order, oldest first, with a hint.
        for i in 1..=4 {
            let dropped = format!("dropped-{i}");
            assert!(
                !out.contains(dropped.as_str()),
                "entry below the cap shown ({}):\n{out}",
                dropped
            );
        }
        for k in 1..=8 {
            let kept = format!("kept-{k}");
            assert!(
                out.contains(kept.as_str()),
                "kept entry missing ({}):\n{out}",
                kept
            );
        }
        assert!(
            out.find("kept-1").unwrap() < out.find("kept-8").unwrap(),
            "kept entries not oldest-first:\n{out}"
        );
        assert!(out.contains("+4 more"), "overflow hint wrong:\n{out}");
    }

    #[test]
    fn all_promoted_telemetry_shows_lci_hint() {
        let mut state = HarnessState::default();
        state
            .telemetry
            .insert("bash::x".into(), record("bash::x", "x", vec![1], 1, 2));
        state.promoted_context.push(promoted("bash::x"));
        let out = joined(&build_state_rows(&state));
        assert!(
            norm(&out)
                .contains(norm("Every tracked call has been promoted into warm context.").as_str()),
            "all-promoted hint missing:\n{out}"
        );
        assert!(!out.contains("1 act"), "no watchlist rows expected:\n{out}");
    }

    #[test]
    fn missing_optional_fields_render_bare_digest() {
        let state = HarnessState {
            goal: "g".into(),
            last_activation: Some(HarnessActivationDigest {
                actions: vec![],
                cycles: None,
                iteration: 4,
                r#loop: None,
                outcome: String::new(),
                task_id: None,
            }),
            ..Default::default()
        };
        let out = joined(&build_state_rows(&state));
        let lines: Vec<&str> = out.split('\n').collect();
        assert!(
            lines.contains(&"loop digest @ cycle 4"),
            "bare digest head missing:\n{out}"
        );
        assert!(
            !lines.iter().any(|line| line.starts_with("→ ")),
            "no outcome row expected for empty outcome:\n{out}"
        );
    }

    #[test]
    fn long_markdown_preview_is_plain_and_clipped() {
        let mut state = HarnessState::default();
        let md = format!("**intro** with `code` and trailing {}", "x".repeat(400));
        state.memory.push(HarnessMemoryNote {
            id: "long".into(),
            created_at_iteration: 1,
            text: md,
        });
        let rows = build_state_rows(&state);
        for row in &rows {
            assert!(
                row.text.chars().count() <= WRAP_WIDTH + 1,
                "row exceeds wrap width: {:?}",
                row.text
            );
            assert!(
                !row.text.contains('\n'),
                "row must be single-line: {:?}",
                row.text
            );
        }
        let out = joined(&rows);
        assert!(
            rows.iter().any(|row| row.text.ends_with('…')),
            "long preview should clip with an ellipsis:\n{out}"
        );
        assert!(out.contains("intro"), "preview head lost:\n{out}");
    }
}
