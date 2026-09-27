//! Claude `/context`-style rendering of a `HarnessContextBreakdown`.
//!
//! The harness emits one breakdown per loop cycle (the `context` field of a
//! `HarnessEvent`) estimating how the prompt's tokens are spent — system
//! prompt, tool schemas, skills, memory, warm context, task list, transcript.
//! This module turns that value into the pieces the dripw `[5]` pane paints:
//! a header (model · used/max · percent), a colored glyph grid (one cell is a
//! fixed share of the window), and the "Estimated usage by category" list,
//! which closes with the remaining free space.
//!
//! Everything here is pure: no clock, no I/O, no colors. `glyph_cells`
//! returns booleans and the caller maps them to its palette, so the layout is
//! unit-testable without a terminal.

use crate::core::types::HarnessContextBreakdown;

/// Cells in the glyph grid.
pub const GRID_CELLS: usize = 50;
/// Cells per painted row of the grid.
pub const GRID_COLS: usize = 10;

/// One row of the "Estimated usage by category" list.
#[derive(Debug, Clone, PartialEq)]
pub struct CategoryLine {
    /// Category name as the harness reported it (`system prompt`, `tools`, …),
    /// or `Free space` for the unspent remainder.
    pub name: String,
    pub tokens: i64,
    /// Share of the context window, in 0..=100. Zero when the max is unknown.
    pub percent: f64,
}

/// `tokens` as a share of `max`, in 0..=100; `None` when the window size is
/// unknown or zero, so callers can distinguish "no percentage" from "0%".
pub fn percent_of(tokens: i64, max: Option<i64>) -> Option<f64> {
    let max = max.filter(|m| *m > 0)?;
    Some((tokens as f64) * 100.0 / (max as f64))
}

/// Share of the window the breakdown's `total_tokens` occupies.
pub fn used_percent(breakdown: &HarnessContextBreakdown) -> Option<f64> {
    percent_of(breakdown.total_tokens, breakdown.max_tokens)
}

/// Compact token count: `940`, `12.3k`, `1.2m`. Negative counts clamp to 0.
pub fn fmt_tokens(tokens: i64) -> String {
    let n = tokens.max(0);
    if n < 1_000 {
        return n.to_string();
    }
    if n < 1_000_000 {
        return if n % 1_000 == 0 {
            format!("{}k", n / 1_000)
        } else {
            format!("{:.1}k", (n as f64) / 1_000.0)
        };
    }
    if n % 1_000_000 == 0 {
        format!("{}m", n / 1_000_000)
    } else {
        format!("{:.1}m", (n as f64) / 1_000_000.0)
    }
}

/// The pane header: `model · 12.3k / 200k (6%)`, or `model · 12.3k` when the
/// window size is unknown. An empty model falls back to `unknown`.
pub fn header_text(breakdown: &HarnessContextBreakdown, model: &str) -> String {
    let model = if model.trim().is_empty() {
        "unknown"
    } else {
        model.trim()
    };
    let used = fmt_tokens(breakdown.total_tokens);
    match breakdown.max_tokens.filter(|m| *m > 0) {
        Some(max) => {
            let pct = used_percent(breakdown).unwrap_or(0.0).round() as i64;
            format!("{model} · {used} / {} ({pct}%)", fmt_tokens(max))
        }
        None => format!("{model} · {used}"),
    }
}

/// The category list in the breakdown's own order, closed by the free space
/// when the window size is known and something is left over. Percentages
/// round to one decimal.
///
/// A reported `total_tokens` smaller than the sum of the categories is taken
/// at face value — the list never invents or drops rows to force a match.
pub fn category_lines(breakdown: &HarnessContextBreakdown) -> Vec<CategoryLine> {
    let max = breakdown.max_tokens;
    let mut out: Vec<CategoryLine> = breakdown
        .categories
        .iter()
        .map(|c| CategoryLine {
            name: c.name.clone(),
            tokens: c.tokens,
            percent: round1(percent_of(c.tokens, max).unwrap_or(0.0)),
        })
        .collect();
    if let Some(max) = max.filter(|m| *m > 0) {
        let free = max - breakdown.total_tokens;
        if free > 0 {
            out.push(CategoryLine {
                name: "Free space".to_string(),
                tokens: free,
                percent: round1(percent_of(free, Some(max)).unwrap_or(0.0)),
            });
        }
    }
    out
}

/// One boolean per grid cell: `true` when the cell sits inside the used share
/// of the window. With no known max every cell is `false` (nothing to show).
pub fn glyph_cells(breakdown: &HarnessContextBreakdown) -> Vec<bool> {
    let share = used_percent(breakdown)
        .map(|p| (p / 100.0).clamp(0.0, 1.0))
        .unwrap_or(0.0);
    let lit = (share * GRID_CELLS as f64).round() as usize;
    (0..GRID_CELLS).map(|i| i < lit).collect()
}

/// The grid as `GRID_COLS`-wide rows of `#` (used) and `.` (free) glyphs.
pub fn glyph_rows(breakdown: &HarnessContextBreakdown) -> Vec<String> {
    let cells = glyph_cells(breakdown);
    cells
        .chunks(GRID_COLS)
        .map(|chunk| {
            chunk
                .iter()
                .map(|lit| if *lit { '#' } else { '.' })
                .collect()
        })
        .collect()
}

/// The most recent context breakdown among `transcript`'s events, if any.
///
/// Mirrors the `skill_loads` scan: it walks the transcript backwards and
/// returns the `context` field of the newest event that carries one, so the
/// `[5]` pane always describes the focused session's latest cycle.
pub fn latest_context(
    transcript: &[crate::cli::transcript::TranscriptEntry],
) -> Option<HarnessContextBreakdown> {
    use crate::cli::transcript::TranscriptEntry;
    transcript.iter().rev().find_map(|entry| match entry {
        TranscriptEntry::Event(event) => event.data.as_ref().and_then(|data| data.context.clone()),
        _ => None,
    })
}

fn round1(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::{HarnessContextBreakdown, HarnessContextCategory};

    fn breakdown(
        categories: &[(&str, i64)],
        total: i64,
        max: Option<i64>,
    ) -> HarnessContextBreakdown {
        HarnessContextBreakdown {
            categories: categories
                .iter()
                .map(|(name, tokens)| HarnessContextCategory {
                    name: (*name).to_string(),
                    tokens: *tokens,
                })
                .collect(),
            total_tokens: total,
            max_tokens: max,
            prompt_tokens: None,
        }
    }

    #[test]
    fn header_rounds_the_window_percentage() {
        let b = breakdown(&[("tools", 24_600)], 24_600, Some(200_000));
        assert_eq!(header_text(&b, "gpt-5.4"), "gpt-5.4 · 24.6k / 200k (12%)");
    }

    #[test]
    fn header_omits_the_percentage_without_a_window_size() {
        let b = breakdown(&[("tools", 900)], 900, None);
        assert_eq!(header_text(&b, ""), "unknown · 900");
    }

    #[test]
    fn category_list_closes_with_the_free_space() {
        let b = breakdown(
            &[("tools", 60_000), ("system prompt", 40_000)],
            100_000,
            Some(200_000),
        );
        let lines = category_lines(&b);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].percent, 30.0);
        assert_eq!(lines[1].tokens, 40_000);
        assert_eq!(lines[2].name, "Free space");
        assert_eq!(lines[2].tokens, 100_000);
        assert_eq!(lines[2].percent, 50.0);
    }

    #[test]
    fn category_list_has_no_free_row_without_a_window_size_or_when_full() {
        let no_max = breakdown(&[("tools", 10)], 10, None);
        assert_eq!(category_lines(&no_max).len(), 1);
        let full = breakdown(&[("tools", 200_000)], 200_000, Some(200_000));
        assert_eq!(category_lines(&full).len(), 1);
    }

    #[test]
    fn category_percentages_are_zero_when_the_window_is_unknown() {
        let b = breakdown(&[("tools", 10)], 10, None);
        assert_eq!(category_lines(&b)[0].percent, 0.0);
        assert_eq!(used_percent(&b), None);
    }

    #[test]
    fn glyph_grid_lights_the_used_share_and_never_overflow_s() {
        let b = breakdown(&[], 50_000, Some(200_000));
        let cells = glyph_cells(&b);
        assert_eq!(cells.len(), GRID_CELLS);
        assert_eq!(cells.iter().filter(|lit| **lit).count(), 13); // 25% of 50 cells, rounded
        let rows = glyph_rows(&b);
        assert_eq!(rows.len(), GRID_CELLS / GRID_COLS);
        assert!(rows.iter().all(|r| r.chars().count() == GRID_COLS));
        assert_eq!(rows[0], "##########");
        assert_eq!(rows[1], "###.......");
    }

    #[test]
    fn glyph_grid_is_dark_when_the_max_is_unknown_or_over_budget() {
        let unknown = breakdown(&[], 10, None);
        assert!(glyph_cells(&unknown).iter().all(|lit| !*lit));
        let over = breakdown(&[], 400_000, Some(200_000));
        assert!(glyph_cells(&over).iter().all(|lit| *lit));
    }

    #[test]
    fn token_counts_use_compact_units() {
        assert_eq!(fmt_tokens(0), "0");
        assert_eq!(fmt_tokens(940), "940");
        assert_eq!(fmt_tokens(12_300), "12.3k");
        assert_eq!(fmt_tokens(1_250_000), "1.2m");
        assert_eq!(fmt_tokens(-5), "0");
    }

    #[test]
    fn fmt_tokens_drops_a_zero_mantissa() {
        assert_eq!(fmt_tokens(200_000), "200k");
        assert_eq!(fmt_tokens(1_000_000), "1m");
        assert_eq!(fmt_tokens(200_500), "200.5k");
    }

    #[test]
    fn latest_context_reads_the_newest_event_that_carries_one() {
        use crate::cli::transcript::{TranscriptEntry, TranscriptEventEntry};
        use crate::core::types::{HarnessEventData, HarnessEventType};

        fn ev(context: Option<HarnessContextBreakdown>) -> TranscriptEntry {
            TranscriptEntry::Event(TranscriptEventEntry {
                at: String::new(),
                data: Some(HarnessEventData {
                    context,
                    ..HarnessEventData::default()
                }),
                detail: String::new(),
                goal_id: String::new(),
                iteration: 0,
                kind: HarnessEventType::ContextRefreshed,
            })
        }

        let older = breakdown(&[("tools", 1)], 1, None);
        let newer = breakdown(&[("tools", 2)], 2, None);
        let entries = vec![ev(Some(older)), ev(None), ev(Some(newer.clone()))];
        assert_eq!(latest_context(&entries), Some(newer));
        assert_eq!(latest_context(&[]), None);
        assert_eq!(latest_context(&[ev(None)]), None);
    }
}
