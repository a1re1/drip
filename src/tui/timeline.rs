//! Timeline cell rendering and repaint scheduling for the TUI.
//!
//! The Ink components become pure functions that return already-painted ANSI
//! rows: one String per terminal row, with no trailing newline.

use crate::cli::transcript::{format_model_route_lines, TranscriptEntry};
use crate::core::types::{HarnessEventType, HarnessRunReason};
use crate::tui::markdown_ansi::render_markdown_ansi;
use crate::tui::theme::{event_label, event_paint};
use crate::watch::ansi::{c, string_width, strip_ansi, wrap_ansi};

/// Collapse all whitespace runs to a single space, trim, and hard-cut to
/// `max_chars` characters (appending "...") when longer.
pub fn truncate_detail(detail: &str, max_chars: usize) -> String {
    let flattened = detail
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string();

    if flattened.chars().count() <= max_chars {
        return flattened;
    }

    let cut: String = flattened
        .chars()
        .take(max_chars.saturating_sub(3))
        .collect();
    format!("{}...", cut)
}

/// Wall-clock `HH:MM:SS` in the machine's local timezone for an RFC3339
/// `at` value. `None` for empty or unparseable timestamps (legacy rows, test
/// fixtures), so a row prefix can fall back to the plain `[  2]` block.
pub fn clock_time(at: &str) -> Option<String> {
    if at.is_empty() {
        return None;
    }

    let parsed = chrono::DateTime::parse_from_rfc3339(at).ok()?;

    Some(
        parsed
            .with_timezone(&chrono::Local)
            .format("%H:%M:%S")
            .to_string(),
    )
}

/// Timeline row prefix: `[  2 14:23:41] ` when the entry carries a timestamp,
/// `[  2] ` otherwise. The clock is what lets an operator eyeball when each
/// op / task / warn settled as a run advances.
pub fn entry_prefix(iteration: i64, at: &str) -> String {
    match clock_time(at) {
        Some(clock) => format!("[{:>3} {}] ", iteration, clock),
        None => format!("[{:>3}] ", iteration),
    }
}

fn reason_string(reason: &HarnessRunReason) -> String {
    serde_json::to_value(reason)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default()
}

/// Render one transcript entry as painted ANSI rows (no trailing newlines).
pub fn render_timeline_cell(entry: &TranscriptEntry, width: usize) -> Vec<String> {
    // ink wraps every row's text at word boundaries (`<Text wrap="wrap">`);
    // a row left longer than the terminal would be broken mid-word by the
    // terminal itself.
    render_timeline_cell_rows(entry, width)
        .into_iter()
        .flat_map(|row| {
            if width > 0 && string_width(&row) > width {
                wrap_ansi(&row, width)
            } else {
                vec![row]
            }
        })
        .collect()
}

fn render_timeline_cell_rows(entry: &TranscriptEntry, width: usize) -> Vec<String> {
    match entry {
        TranscriptEntry::Goal(goal) => {
            let mut rows = vec![String::new()];
            let mut line = c::accent_bold("❯ goal ");
            line.push_str(&c::bold(&goal.text));
            rows.push(line);

            if !goal.images.is_empty() {
                // The marker always lands; a supported terminal adds one
                // inline escape row per displayable image after it.
                rows.extend(crate::tui::images::goal_image_rows(&goal.images, width));
            }

            rows
        }
        TranscriptEntry::Event(event) => match event.kind {
            HarnessEventType::ModelText | HarnessEventType::RunSummary => {
                let mut rows = Vec::new();

                if event.kind == HarnessEventType::RunSummary {
                    rows.push(String::new());
                }

                if event.kind == HarnessEventType::RunSummary {
                    rows.push(event_paint(event.kind)("── run summary ──"));
                } else {
                    rows.push(event_paint(event.kind)(&format!(
                        "{}{}",
                        entry_prefix(event.iteration, &event.at),
                        event_label(event.kind)
                    )));
                }

                for line in render_markdown_ansi(&event.detail).split('\n') {
                    rows.push(line.to_string());
                }

                // Local markdown image links in model text render inline too
                // (no-op unless the TUI opted into a protocol).
                for path in crate::tui::images::markdown_image_paths(&event.detail) {
                    rows.extend(crate::tui::images::inline_image_rows(&[path], width));
                }

                rows
            }
            _ => {
                let label = format!("{:<7}", event_label(event.kind));
                let detail = truncate_detail(&event.detail, 160);
                let detail_paint = if event.kind == HarnessEventType::IterationStart {
                    c::white
                } else {
                    c::dim
                };

                let mut line = c::dim(&entry_prefix(event.iteration, &event.at));
                line.push_str(&event_paint(event.kind)(&label));
                line.push_str(&detail_paint(&detail));

                vec![line]
            }
        },
        TranscriptEntry::RunEnd(run_end) => {
            let cycles = if run_end.iterations == 1 { "" } else { "s" };
            let painted = if matches!(run_end.reason, HarnessRunReason::Completed) {
                c::green
            } else {
                c::yellow
            };

            vec![painted(&format!(
                "∎ run {} after {} cycle{}",
                reason_string(&run_end.reason),
                run_end.iterations,
                cycles
            ))]
        }
        TranscriptEntry::Model(model) => format_model_route_lines(model)
            .iter()
            .map(|line| c::dim(line))
            .collect(),
        TranscriptEntry::Skill(skill) => vec![c::dim(&format!(
            "skill {} {}",
            skill.name,
            if skill.enabled { "enabled" } else { "disabled" }
        ))],
        // An info row may carry its own color (the startup banner paints
        // itself); an uncolored note still renders dim. With color switched
        // off the escapes are removed instead of printed literally.
        TranscriptEntry::Info(note) => {
            let rows = wrap_ansi(&note.text, width);
            if !crate::watch::ansi::color_enabled() {
                return rows.iter().map(|line| strip_ansi(line)).collect();
            }
            rows.iter()
                .map(|line| {
                    if line.contains('\x1b') {
                        line.clone()
                    } else {
                        c::dim(line)
                    }
                })
                .collect()
        }
        TranscriptEntry::Error(note) => {
            let mut rows = vec![String::new()];

            for line in wrap_ansi(&note.text, width) {
                rows.push(c::red(&line));
            }

            rows
        }
    }
}

/// Mirrors TimelineCellView's layout closely enough to budget a repaint: one
/// row per compact line plus headers and marginTop rows. Wrapping is ignored —
/// an overestimate just means a slightly shorter tail.
pub fn estimate_cell_rows(entry: &TranscriptEntry) -> usize {
    match entry {
        TranscriptEntry::Goal(goal) => 2 + usize::from(!goal.images.is_empty()),
        TranscriptEntry::Event(event) => match event.kind {
            HarnessEventType::ModelText | HarnessEventType::RunSummary => {
                // Markdown rendering changes the line count (list spacing,
                // tables), so budget from the rendered output rather than the
                // raw detail.
                let rows = render_markdown_ansi(&event.detail).split('\n').count();
                1 + rows + usize::from(event.kind == HarnessEventType::RunSummary)
            }
            _ => 1,
        },
        TranscriptEntry::Model(model) => format_model_route_lines(model).len(),
        TranscriptEntry::RunEnd(_) | TranscriptEntry::Skill(_) => 1,
        TranscriptEntry::Info(note) => note.text.split('\n').count(),
        TranscriptEntry::Error(note) => note.text.split('\n').count() + 1,
    }
}

/// Rows the pinned live region (composer, menus, status bar) needs to stay
/// visible below a re-emitted timeline tail.
pub const LIVE_REGION_RESERVED_ROWS: usize = 8;
pub const MIN_TAIL_ROWS: usize = 4;

/// After a repaint clears the screen, only the most recent cells that fit
/// above the live region are re-emitted. Returns the index of the first cell
/// to show. The newest cell is always included, even oversized — a partially
/// scrolled cell above the composer beats a blank screen.
pub fn select_repaint_tail_start(entries: &[TranscriptEntry], terminal_rows: usize) -> usize {
    if entries.is_empty() {
        return 0;
    }

    let budget = MIN_TAIL_ROWS.max(terminal_rows.saturating_sub(LIVE_REGION_RESERVED_ROWS));
    let mut used_rows = 0usize;

    for (index, entry) in entries.iter().enumerate().rev() {
        used_rows += estimate_cell_rows(entry);

        if used_rows > budget {
            return (index + 1).min(entries.len() - 1);
        }
    }

    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::transcript::{
        TranscriptEventEntry, TranscriptNoteEntry, TranscriptRunEndEntry,
    };
    use crate::watch::ansi::strip_ansi;

    fn note(text: &str) -> TranscriptEntry {
        TranscriptEntry::Info(TranscriptNoteEntry {
            at: String::new(),
            text: text.to_string(),
        })
    }

    #[test]
    fn an_info_row_keeps_its_own_color_and_a_plain_one_stays_dim() {
        let _guard = crate::watch::ansi::color_test_lock();
        crate::watch::ansi::set_color_enabled(true);

        let colored = render_timeline_cell_rows(&note("\x1b[38;5;45mdrip\x1b[0m"), 80);
        assert!(
            colored[0].contains("\x1b[38;5;45m"),
            "the banner's own color was overwritten: {colored:?}"
        );
        assert_eq!(strip_ansi(&colored[0]), "drip");

        let dim = render_timeline_cell_rows(&note("plain note"), 80);
        assert_eq!(dim[0], "\x1b[2mplain note\x1b[0m");

        // No color: the escape is removed, never printed literally.
        crate::watch::ansi::set_color_enabled(false);
        let uncolored = render_timeline_cell_rows(&note("\x1b[38;5;45mdrip\x1b[0m"), 80);
        assert_eq!(uncolored[0], "drip");
        crate::watch::ansi::set_color_enabled(true);
    }

    #[test]
    fn truncate_detail_collapses_and_cuts() {
        assert_eq!(truncate_detail("  a \n\t b   c ", 40), "a b c");
        let long = truncate_detail("abcdefghij", 7);
        assert_eq!(long, "abcd...");
        assert_eq!(long.chars().count(), 7);

        let unicode = truncate_detail("héllo wörld", 8);
        assert_eq!(unicode, "héllo...");
        assert_eq!(unicode.chars().count(), 8);
    }

    #[test]
    fn long_event_rows_word_wrap_at_the_terminal_width() {
        let entry = TranscriptEntry::Event(TranscriptEventEntry {
            at: "2026-01-01T00:00:00.000Z".to_string(),
            data: None,
            detail: "plan_tasks: Added 2 task(s): task-1: read greeting; task-2: slow step."
                .to_string(),
            goal_id: "g".to_string(),
            iteration: 1,
            kind: HarnessEventType::HarnessOp,
        });
        let rows: Vec<String> = render_timeline_cell(&entry, 60)
            .iter()
            .map(|row| strip_ansi(row))
            .collect();
        // The clock rides in the iteration block and eats into the wrap budget.
        let clock = clock_time("2026-01-01T00:00:00.000Z").unwrap();
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert!(
            rows[0].starts_with(&format!("[  1 {clock}] op")),
            "{rows:?}"
        );
        assert!(rows[0].ends_with("task-1:"), "{rows:?}");
        assert_eq!(rows[1], "read greeting; task-2: slow step.");
        assert!(rows.iter().all(|row| row.chars().count() <= 60));
    }

    #[test]
    fn clock_time_shapes_and_rejects_non_timestamps() {
        let shaped = clock_time("2026-01-01T12:00:00.000Z").unwrap();
        assert_eq!(shaped.len(), 8, "{shaped:?}");
        assert_eq!(
            shaped.chars().filter(|ch| *ch == ':').count(),
            2,
            "{shaped:?}"
        );
        assert!(
            shaped.chars().all(|ch| ch.is_ascii_digit() || ch == ':'),
            "{shaped:?}"
        );

        assert_eq!(clock_time(""), None);
        assert_eq!(clock_time("not a date"), None);

        // Timezone-independent invariant: two instants an hour apart read one
        // hour apart on whatever local clock the machine runs.
        let minutes = |value: &str| -> i64 {
            let mut parts = value.split(':');
            let hh: i64 = parts.next().unwrap().parse().unwrap();
            let mm: i64 = parts.next().unwrap().parse().unwrap();
            hh * 60 + mm
        };
        let a = minutes(&clock_time("2026-01-01T12:00:00.000Z").unwrap());
        let b = minutes(&clock_time("2026-01-01T13:00:00.000Z").unwrap());
        assert_eq!((b - a).rem_euclid(24 * 60), 60);
    }

    #[test]
    fn entry_prefix_carries_the_clock_and_falls_back_when_absent() {
        let stamp = "2026-01-01T12:34:56.000Z";
        let clock = clock_time(stamp).unwrap();
        assert_eq!(entry_prefix(2, stamp), format!("[  2 {clock}] "));
        assert_eq!(entry_prefix(2, ""), "[  2] ");
        // Width of the block is stable regardless of the hour it names.
        assert_eq!(string_width(&entry_prefix(12, stamp)), 15);
    }

    #[test]
    fn event_rows_show_a_wall_clock_in_the_iteration_block() {
        let stamp = "2026-01-01T12:34:56.000Z";
        let clock = clock_time(stamp).unwrap();
        let event = |kind: HarnessEventType, detail: &str| {
            TranscriptEntry::Event(TranscriptEventEntry {
                at: stamp.to_string(),
                data: None,
                detail: detail.to_string(),
                goal_id: "g".to_string(),
                iteration: 2,
                kind,
            })
        };

        // op / task / warn / cycle rows all carry the same block.
        for (kind, label, detail) in [
            (
                HarnessEventType::HarnessOp,
                "op",
                "finish_task: task-1 marked completed",
            ),
            (
                HarnessEventType::TaskFinished,
                "task",
                "task-1 done: added the timestamp",
            ),
            (
                HarnessEventType::RunWarning,
                "warn",
                "verification anchor downgraded",
            ),
            (
                HarnessEventType::IterationStart,
                "cycle",
                "cycle 2/5 — task-1: keep going",
            ),
            (HarnessEventType::ModelText, "text", "All done."),
        ] {
            let rows: Vec<String> = render_timeline_cell(&event(kind, detail), 120)
                .iter()
                .map(|row| strip_ansi(row))
                .collect();
            let first = rows[0].clone();
            assert!(
                first.starts_with(&format!("[  2 {clock}] {label}")),
                "{kind:?}: {first:?}"
            );
            let word = detail.split_whitespace().next().unwrap();
            assert!(
                rows.iter().any(|row| row.contains(word)),
                "{kind:?}: {rows:?}"
            );
        }
    }

    #[test]
    fn goal_cell_has_margin_and_attachment_row() {
        let entry = TranscriptEntry::Goal(crate::cli::transcript::TranscriptGoalEntry {
            at: String::new(),
            goal_id: String::new(),
            images: vec!["a.png".to_string(), "b.png".to_string()],
            mentions: Vec::new(),
            text: "do the thing".to_string(),
        });

        let rows = render_timeline_cell(&entry, 80);
        let plain: Vec<String> = rows.iter().map(|row| strip_ansi(row)).collect();

        assert_eq!(plain.len(), 3);
        assert_eq!(plain[0], "");
        assert_eq!(plain[1], "❯ goal do the thing");
        assert_eq!(plain[2], "  [2 images attached]");

        let single = TranscriptEntry::Goal(crate::cli::transcript::TranscriptGoalEntry {
            at: String::new(),
            goal_id: String::new(),
            images: vec!["a.png".to_string()],
            mentions: Vec::new(),
            text: "one".to_string(),
        });
        let plain: Vec<String> = render_timeline_cell(&single, 80)
            .iter()
            .map(|row| strip_ansi(row))
            .collect();
        assert_eq!(plain[2], "  [1 image attached]");
    }

    #[test]
    fn run_end_singular_plural() {
        let make = |iterations: i64| {
            let entry = TranscriptEntry::RunEnd(TranscriptRunEndEntry {
                at: String::new(),
                goal_id: String::new(),
                iterations,
                reason: HarnessRunReason::Completed,
            });
            strip_ansi(&render_timeline_cell(&entry, 80)[0])
        };

        assert_eq!(make(1), "∎ run completed after 1 cycle");
        assert_eq!(make(3), "∎ run completed after 3 cycles");
    }

    #[test]
    fn repaint_tail_keeps_newest_cell() {
        let entries: Vec<TranscriptEntry> = (0..10).map(|_| note("l1\nl2\nl3")).collect();

        // terminal_rows 12 → budget = max(4, 12 - 8) = 4; each Info cell is 3
        // rows, so only the newest 1 fits (3 ≤ 4, adding the next exceeds).
        assert_eq!(select_repaint_tail_start(&entries, 12), 9);
    }

    #[test]
    fn repaint_tail_zero_when_everything_fits() {
        let entries: Vec<TranscriptEntry> = (0..3).map(|_| note("l1\nl2\nl3")).collect();

        assert_eq!(select_repaint_tail_start(&entries, 40), 0);
        assert_eq!(select_repaint_tail_start(&[], 40), 0);
    }
}
