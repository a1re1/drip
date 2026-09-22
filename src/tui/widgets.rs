//! Pure render functions for the composer, picker and status bar. Each
//! function returns one
//! painted `String` per terminal row, with no trailing newline. Input
//! handling is owned by the app; picker state is passed in.

use crate::cli::images::GoalImageAttachment;
use crate::tui::theme::{paint, ACCENT_COLOR, DIM_COLOR};
use crate::watch::ansi::{char_width, fit, string_width, wrap_ansi};

use crate::cli::commands::SlashCommandSpec;

const INVERSE_ON: &str = "\u{1b}[7m";
const INVERSE_OFF: &str = "\u{1b}[27m";

/// Ink `borderStyle="round"` with `paddingX=1`: top `╭─...─╮`, content rows
/// padded or wrapped to `width - 4`, bottom `╰─...─╯`. The border glyphs are
/// painted with `theme::paint(color)`.
pub fn boxed(rows: Vec<String>, width: usize, color: &str) -> Vec<String> {
    let width = width.max(6);
    let paint = paint(color);
    let horizontal: String = std::iter::repeat('─').take(width - 2).collect();
    let mut out = Vec::with_capacity(rows.len() + 2);
    out.push(paint(&format!("╭{horizontal}╮")));
    let inner = width - 4;
    for row in rows {
        let visible_width = string_width(&row);
        if visible_width <= inner {
            let mut padded = row;
            for _ in visible_width..inner {
                padded.push(' ');
            }
            out.push(format!("{}{}{}", paint("│ "), padded, paint(" │")));
        } else {
            for piece in wrap_ansi(&row, inner) {
                let piece_width = string_width(&piece);
                let mut padded = piece;
                for _ in piece_width..inner {
                    padded.push(' ');
                }
                out.push(format!("{}{}{}", paint("│ "), padded, paint(" │")));
            }
        }
    }
    out.push(paint(&format!("╰{horizontal}╯")));
    out
}

/// Visual lines of the composer text: the char-index range of every row a
/// terminal draws for `text_width` columns of body space. Paragraphs are split
/// on `'\n'` and each is greedily word-wrapped: a word that would overflow
/// starts on the next line, the space it broke at stays at the end of the
/// upper line, and a single word wider than `text_width` is hard-broken. An
/// empty text is one empty line (`0..0`), a trailing `'\n'` adds a final empty
/// line, and the separator itself is never inside a range.
pub fn composer_lines(text: &str, text_width: usize) -> Vec<std::ops::Range<usize>> {
    let width = text_width.max(1);
    let chars: Vec<char> = text.chars().collect();
    let mut lines = Vec::new();
    let mut paragraph_start = 0usize;
    loop {
        match chars[paragraph_start..].iter().position(|&c| c == '\n') {
            Some(offset) => {
                let paragraph_end = paragraph_start + offset;
                wrap_paragraph(&chars, paragraph_start, paragraph_end, width, &mut lines);
                paragraph_start = paragraph_end + 1;
            }
            None => {
                wrap_paragraph(&chars, paragraph_start, chars.len(), width, &mut lines);
                break;
            }
        }
    }
    lines
}

fn wrap_paragraph(
    chars: &[char],
    start: usize,
    end: usize,
    width: usize,
    lines: &mut Vec<std::ops::Range<usize>>,
) {
    if start == end {
        lines.push(start..end);
        return;
    }
    let mut line_start = start;
    while line_start < end {
        let mut last_space: Option<usize> = None;
        let mut column = 0usize;
        let mut index = line_start;
        let mut line_end = line_start;
        while index < end {
            let cell = char_width(chars[index] as u32);
            if column + cell > width && index > line_start {
                break;
            }
            if chars[index] == ' ' {
                last_space = Some(index);
            }
            column += cell;
            index += 1;
            line_end = index;
            if column >= width {
                break;
            }
        }
        if line_end < end {
            if chars[line_end] == ' ' {
                // The line filled exactly at a word boundary: the space
                // belongs to the upper line (clipped when drawn) so the
                // continuation row never starts with a stray blank.
                line_end += 1;
            } else if let Some(space) = last_space {
                // Break at the last space so it stays on the upper line.
                line_end = space + 1;
            }
        }
        lines.push(line_start..line_end);
        line_start = line_end;
    }
}

/// Visual `(line_index, column)` of the char-index `cursor`: the display-column
/// offset of `cursor` inside its wrapped line. A cursor exactly at a wrap point
/// belongs to the column-0 start of the lower line.
pub fn composer_cursor_position(text: &str, text_width: usize, cursor: usize) -> (usize, usize) {
    let chars: Vec<char> = text.chars().collect();
    let lines = composer_lines(text, text_width);
    let target = cursor.min(chars.len());
    for (index, line) in lines.iter().enumerate() {
        if line.contains(&target) {
            return (index, display_columns(text, line.start, target));
        }
        // A cursor sitting on a paragraph break renders at the end of the
        // line that break closes, not at the start of the next one.
        if line.end == target && chars.get(target) == Some(&'\n') {
            return (index, display_columns(text, line.start, line.end));
        }
    }
    let last = lines.len() - 1;
    (last, display_columns(text, lines[last].start, lines[last].end))
}

/// Inverse of `composer_cursor_position`: the char index sitting `column`
/// display columns into visual line `line_index`. The column is clamped to the
/// line's end, and to the nearest char boundary when a wide char straddles it.
pub fn composer_cursor_at(text: &str, text_width: usize, line_index: usize, column: usize) -> usize {
    let lines = composer_lines(text, text_width);
    let line = match lines.get(line_index) {
        Some(line) => line.clone(),
        None => return text.chars().count(),
    };
    let chars: Vec<char> = text.chars().collect();
    let mut used = 0usize;
    let mut index = line.start;
    while index < line.end {
        let cell = char_width(chars[index] as u32);
        if used + cell > column {
            break;
        }
        used += cell;
        index += 1;
    }
    index
}

fn display_columns(text: &str, start: usize, end: usize) -> usize {
    text.chars()
        .skip(start)
        .take(end - start)
        .map(|c| char_width(c as u32))
        .sum()
}

/// Display columns the composer body gets at terminal `width`: the two-column
/// `❯ `/indent prefix and one spare column for the inverse cursor cell at the
/// end of a full line are reserved, so a body row never exceeds `width`.
pub fn composer_text_width(width: usize) -> usize {
    width.max(6) - 3
}

/// Composer render inputs.
pub struct ComposerProps<'a> {
    pub attachments: &'a [GoalImageAttachment],
    pub cursor: usize,
    pub disabled: bool,
    pub mention_suggestions: &'a [String],
    pub selected_skill_index: usize,
    pub selected_suggestion_index: usize,
    pub skill_suggestions: &'a [(String, String)],
    pub queued_count: usize,
    pub slash_suggestions: &'a [&'a SlashCommandSpec],
    pub text: &'a str,
}

/// Clamp a hint row to the terminal width and pad it, so every composer row
/// is exactly `width` wide like the painted box above it.
fn composer_hint_row(text: &str, width: usize) -> String {
    let mut row: String = text.chars().take(width).collect();
    while row.chars().count() < width {
        row.push(' ');
    }
    row
}

/// Port of the Ink `Composer` component.
pub fn render_composer(props: &ComposerProps, width: usize) -> Vec<String> {
    let mut rows: Vec<String> = Vec::new();

    if !props.attachments.is_empty() {
        let yellow = paint("yellow");
        let line = props
            .attachments
            .iter()
            .enumerate()
            .map(|(index, attachment)| {
                yellow(&format!("[image #{} {}] ", index + 1, attachment.file_name))
            })
            .collect::<String>();
        rows.push(line);
    }

    // The skill menu is painted before the box, so these rows float ABOVE
    // the input line (Claude-style floating suggestions). Long names and
    // descriptions are clipped to the terminal width.
    let show_skill_menu = !props.disabled
        && props.slash_suggestions.is_empty()
        && !props.skill_suggestions.is_empty();
    if show_skill_menu {
        let accent = paint(ACCENT_COLOR);
        let dim = paint(DIM_COLOR);
        let line_width = width.max(6).saturating_sub(4).max(1);
        for (index, (name, description)) in props.skill_suggestions.iter().enumerate() {
            let line = fit(&format!("/{name} — {description}"), line_width, true);
            if index == props.selected_skill_index {
                rows.push(format!("  {}{}", accent("▸ "), accent(&line)));
            } else {
                rows.push(format!("  {}{}", dim("  "), dim(&line)));
            }
        }
    }

    let border_color = if props.disabled { DIM_COLOR } else { ACCENT_COLOR };
    let prefix_paint = paint(border_color);
    let dim = paint(DIM_COLOR);
    // A disabled composer with empty text shows the run placeholder instead.
    let placeholder = props.disabled && props.text.is_empty();
    let display: &str = if placeholder {
        "running — press esc to stop the run"
    } else {
        props.text
    };
    // Body rows: `❯ ` on the first row and a two-space indent on every
    // continuation row, so the text starts at column 2 on every row. Wrapping
    // comes from `composer_lines`, the same layout cursor movement uses, so
    // the two can never disagree.
    let text_width = composer_text_width(width);
    let lines = composer_lines(display, text_width);
    let cursor_line = if props.disabled {
        // A run owns the composer: no cursor cell is drawn.
        usize::MAX
    } else {
        composer_cursor_position(display, text_width, props.cursor).0
    };
    let text_chars: Vec<char> = display.chars().collect();

    let rule = dim(&"─".repeat(width));
    rows.push(rule.clone());
    for (index, line) in lines.iter().enumerate() {
        let lead = if index == 0 {
            prefix_paint("❯ ")
        } else {
            "  ".to_string()
        };
        let mut cells: Vec<String> = Vec::new();
        let mut used = 0usize;
        for (offset, ch) in text_chars[line.start..line.end].iter().enumerate() {
            let cell = char_width(*ch as u32);
            // A wide char straddling the wrap column is clipped so the inverse
            // cursor cell never escapes the body width.
            if used + cell > text_width {
                break;
            }
            used += cell;
            if index == cursor_line && line.start + offset == props.cursor {
                cells.push(format!("{INVERSE_ON}{ch}{INVERSE_OFF}"));
            } else {
                cells.push(ch.to_string());
            }
        }
        if index == cursor_line {
            if props.cursor >= line.end {
                // Cursor at the line end or on the newline after it: an
                // inverse cell on the blank column (`composer_text_width`
                // keeps one spare column for it on a full line).
                cells.push(format!("{INVERSE_ON} {INVERSE_OFF}"));
            } else if !cells.iter().any(|cell| cell.contains(INVERSE_ON)) {
                // The cursor column lands inside a wide char: draw the inverse
                // cell at the char boundary just before it.
                cells.push(format!("{INVERSE_ON} {INVERSE_OFF}"));
            }
        }
        let body = cells.concat();
        let body = if props.disabled { dim(&body) } else { body };
        rows.push(format!("{lead}{body}"));
    }
    rows.push(rule);

    let show_slash_menu = !props.disabled && !props.slash_suggestions.is_empty();
    let show_mention_menu =
        !props.disabled && !show_slash_menu && !props.mention_suggestions.is_empty();

    if show_slash_menu {
        let accent = paint(ACCENT_COLOR);
        let dim = paint(DIM_COLOR);
        for (index, command) in props.slash_suggestions.iter().enumerate() {
            let args = match command.args {
                Some(args) => format!(" {args}"),
                None => String::new(),
            };
            let line = format!(
                "/{}{} — {}",
                command.name, args, command.description
            );
            // ink: <Box paddingLeft={2}> around the menu.
            if index == props.selected_suggestion_index {
                rows.push(format!("  {}{}", accent("▸ "), accent(&line)));
            } else {
                rows.push(format!("  {}{}", dim("  "), dim(&line)));
            }
        }
    } else if show_mention_menu {
        let accent = paint(ACCENT_COLOR);
        let dim = paint(DIM_COLOR);
        for (index, path) in props.mention_suggestions.iter().enumerate() {
            let line = format!("@{path}");
            // ink: <Box paddingLeft={2}> around the menu.
            if index == props.selected_suggestion_index {
                rows.push(format!("  {}{}", accent("▸ "), accent(&line)));
            } else {
                rows.push(format!("  {}{}", dim("  "), dim(&line)));
            }
        }
    }

    // The queue/steer affordance rides under the box: what enter does, and
    // how to push a queued message into the run that is going right now.
    let dim = paint(DIM_COLOR);
    // Kept short enough to survive one line at a normal terminal width.
    let hint = if props.disabled {
        if props.queued_count > 0 {
            "enter queues · shift+enter steers with next queued message".to_string()
        } else {
            "enter queues · shift+enter steers the run".to_string()
        }
    } else if props.queued_count > 0 {
        format!(
            "{} queued — shift+enter steers the running goal with one",
            props.queued_count
        )
    } else {
        "enter sends · shift+enter steers with a queued message".to_string()
    };
    rows.push(dim(&composer_hint_row(&hint, width)));

    rows
}

/// Picker list item.
#[derive(Clone, Debug)]
pub struct PickerItem {
    pub detail: Option<String>,
    pub id: String,
    pub label: String,
}

/// Port of the Ink `Picker` component (render only; state is passed in).
pub fn render_picker(title: &str, items: &[PickerItem], selected_index: usize, width: usize) -> Vec<String> {
    let accent = paint(ACCENT_COLOR);
    let dim = paint(DIM_COLOR);
    let mut rows: Vec<String> = Vec::new();
    rows.push(accent(&paint_bold_title(title)));
    if items.is_empty() {
        rows.push(dim("nothing to select — esc to close"));
    }
    for (index, item) in items.iter().enumerate() {
        let selected = index == selected_index;
        let prefix = if selected {
            accent("▸ ")
        } else {
            dim("  ")
        };
        let label = if selected {
            accent(&item.label)
        } else {
            item.label.clone()
        };
        let detail = match &item.detail {
            Some(detail) => dim(&format!(" — {detail}")),
            None => String::new(),
        };
        rows.push(format!("{prefix}{label}{detail}"));
    }
    rows.push(dim("↑/↓ move · enter select · esc cancel"));
    boxed(rows, width, ACCENT_COLOR)
}

fn paint_bold_title(title: &str) -> String {
    crate::watch::ansi::c::bold(title)
}

/// Status bar render inputs.
pub struct StatusBarProps<'a> {
    pub active_skill_names: &'a [String],
    pub cwd: &'a str,
    pub model_label: &'a str,
    pub running: bool,
    pub running_detail: Option<&'a str>,
    pub session_id: &'a str,
}

/// Port of the Ink `StatusBar` component.
pub fn render_status_bar(props: &StatusBarProps, width: usize) -> Vec<String> {
    let mut rows: Vec<String> = Vec::new();
    if props.running {
        let yellow = paint("yellow");
        let dim = paint(DIM_COLOR);
        rows.push(format!(
            "{}{}",
            yellow("● running "),
            dim(&format!(
                "{} (esc to stop)",
                props.running_detail.unwrap_or("")
            )),
        ));
    }
    let dim = paint(DIM_COLOR);
    let mut line = format!(
        "{} · session {}",
        props.model_label,
        props.session_id.chars().take(8).collect::<String>()
    );
    if !props.active_skill_names.is_empty() {
        line.push_str(&format!(" · skills: {}", props.active_skill_names.join(", ")));
    }
    line.push_str(&format!(" · {}", props.cwd));
    rows.push(dim(&fit(&line, width, true)));
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watch::ansi::strip_ansi;

    fn plain(rows: &[String]) -> Vec<String> {
        rows.iter().map(|row| strip_ansi(row)).collect()
    }

    #[test]
    fn boxed_pads_and_frames() {
        let rows = boxed(vec!["ab".to_string()], 10, DIM_COLOR);
        assert_eq!(rows.len(), 3);
        let plain_rows = plain(&rows);
        assert_eq!(plain_rows[0], "╭────────╮");
        assert_eq!(plain_rows[1], "│ ab     │");
        assert_eq!(plain_rows[2], "╰────────╯");
    }

    #[test]
    fn composer_shows_cursor_block() {
        let props = ComposerProps {
            attachments: &[],
            cursor: 1,
            disabled: false,
            mention_suggestions: &[],
            selected_skill_index: 0,
            selected_suggestion_index: 0,
            skill_suggestions: &[],
            queued_count: 0,
            slash_suggestions: &[],
            text: "abc",
        };
        let rows = render_composer(&props, 40);
        assert!(rows.iter().any(|row| row.contains("\u{1b}[7mb\u{1b}[27m")));
    }

    #[test]
    fn composer_renders_multi_line_text_between_rules() {
        let props = ComposerProps {
            attachments: &[],
            cursor: 8,
            disabled: false,
            mention_suggestions: &[],
            selected_skill_index: 0,
            selected_suggestion_index: 0,
            skill_suggestions: &[],
            queued_count: 0,
            slash_suggestions: &[],
            text: "line one\nline two",
        };
        let rows = plain(&render_composer(&props, 20));
        // Rule, "❯ line one" (the cursor on the newline is an inverse cell at
        // the line end), the two-space continuation row, the closing rule, then
        // the enter/shift+enter hint.
        assert_eq!(rows.len(), 5, "{rows:?}");
        assert_eq!(rows[0], "─".repeat(20), "{rows:?}");
        assert_eq!(rows[1].trim_end(), "❯ line one", "{rows:?}");
        assert_eq!(rows[2], "  line two", "{rows:?}");
        assert_eq!(rows[3], "─".repeat(20), "{rows:?}");
        assert!(rows[4].contains("enter sends"), "{rows:?}");
    }

    #[test]
    fn composer_renders_wrapped_rows_with_a_two_space_indent() {
        let props = ComposerProps {
            attachments: &[],
            cursor: 0,
            disabled: false,
            mention_suggestions: &[],
            selected_skill_index: 0,
            selected_suggestion_index: 0,
            skill_suggestions: &[],
            queued_count: 0,
            slash_suggestions: &[],
            text: "aaaa bbbb cccc dddd eeee",
        };
        let rows = plain(&render_composer(&props, 20));
        // Body width is 18, so the break space ends the first row and the tail
        // continues under it, still starting at column 2.
        assert_eq!(rows[0], "─".repeat(20), "{rows:?}");
        assert_eq!(rows[1], "❯ aaaa bbbb cccc ", "{rows:?}");
        assert_eq!(rows[2], "  dddd eeee", "{rows:?}");
        assert_eq!(rows[3], "─".repeat(20), "{rows:?}");
    }

    #[test]
    fn composer_lines_wraps_at_spaces_and_owns_the_break_space() {
        // "one two" fills the 7 columns exactly; the space after it is
        // absorbed by the upper line so "three" starts flush at column 0.
        assert_eq!(composer_lines("one two three", 7), vec![0..8, 8..13]);
        assert_eq!(composer_lines("one twos three", 7), vec![0..4, 4..9, 9..14]);
        assert_eq!(
            composer_lines("aaaa bbbb cccc dddd eeee", 18),
            vec![0..15, 15..24]
        );
    }

    #[test]
    fn composer_lines_hard_breaks_a_word_longer_than_the_width() {
        assert_eq!(composer_lines("abcdefghij", 4), vec![0..4, 4..8, 8..10]);
        assert_eq!(composer_lines("abcdef", 6), vec![0..6]);
    }

    #[test]
    fn composer_lines_splits_paragraphs_and_keeps_empty_lines() {
        assert_eq!(composer_lines("", 10), vec![0..0]);
        assert_eq!(composer_lines("aa\nbb", 10), vec![0..2, 3..5]);
        assert_eq!(composer_lines("aa\n", 10), vec![0..2, 3..3]);
        assert_eq!(composer_lines("aa\n\nbb", 10), vec![0..2, 3..3, 4..6]);
    }

    #[test]
    fn composer_cursor_position_puts_a_wrap_point_on_the_lower_line() {
        let text = "one twos three";
        assert_eq!(composer_cursor_position(text, 7, 4), (1, 0));
        assert_eq!(composer_cursor_at(text, 7, 1, 0), 4);
        assert_eq!(composer_cursor_position(text, 7, 3), (0, 3));
        assert_eq!(composer_cursor_position(text, 7, 14), (2, 5));
        assert_eq!(
            composer_cursor_at(text, 7, 2, 99),
            14,
            "column clamped to the line end"
        );
        // A cursor on the absorbed space after a full line stays on that line,
        // one column past the text; the next index starts the lower line.
        assert_eq!(composer_cursor_position("one two three", 7, 7), (0, 7));
        assert_eq!(composer_cursor_position("one two three", 7, 8), (1, 0));
        assert_eq!(composer_cursor_at(text, 7, 9, 0), 14, "line out of range");
        // A cursor on a paragraph break belongs to the end of the upper line.
        assert_eq!(composer_cursor_position("aa\nbb", 7, 2), (0, 2));
        assert_eq!(composer_cursor_at("aa\nbb", 7, 0, 2), 2);
    }

    #[test]
    fn composer_cursor_helpers_round_trip_every_boundary() {
        for text in ["one two three", "aa\nbb", ""] {
            let width = 7;
            let total = text.chars().count();
            for cursor in 0..=total {
                let (line, column) = composer_cursor_position(text, width, cursor);
                assert_eq!(
                    composer_cursor_at(text, width, line, column),
                    cursor,
                    "text {text:?} cursor {cursor}"
                );
            }
        }
    }

    #[test]
    fn composer_cursor_helpers_clamp_wide_char_boundaries() {
        let text = "你好世界";
        assert_eq!(composer_lines(text, 4), vec![0..2, 2..4]);
        assert_eq!(composer_cursor_position(text, 4, 2), (1, 0));
        assert_eq!(composer_cursor_position(text, 4, 1), (0, 2));
        assert_eq!(
            composer_cursor_at(text, 4, 0, 1),
            0,
            "nearest boundary before the wide char"
        );
        assert_eq!(composer_cursor_at(text, 4, 0, 2), 1);
        for cursor in 0..=text.chars().count() {
            let (line, column) = composer_cursor_position(text, 4, cursor);
            assert_eq!(composer_cursor_at(text, 4, line, column), cursor);
        }
    }

    #[test]
    fn composer_menus_are_indented_like_ink_padding() {
        let mentions = vec!["hello.txt".to_string(), "help.md".to_string()];
        let props = ComposerProps {
            attachments: &[],
            cursor: 4,
            disabled: false,
            mention_suggestions: &mentions,
            selected_skill_index: 0,
            selected_suggestion_index: 0,
            skill_suggestions: &[],
            queued_count: 0,
            slash_suggestions: &[],
            text: "@hel",
        };
        let rows = plain(&render_composer(&props, 40));
        assert_eq!(rows[3], "  ▸ @hello.txt");
        assert_eq!(rows[4], "    @help.md");
    }

    #[test]
    fn skill_menu_rows_render_above_the_top_rule() {
        let skills = vec![
            ("navis".to_string(), "test skill navis".to_string()),
            ("nada".to_string(), "test skill nada".to_string()),
        ];
        let props = ComposerProps {
            attachments: &[],
            cursor: 3,
            disabled: false,
            mention_suggestions: &[],
            selected_skill_index: 0,
            selected_suggestion_index: 0,
            skill_suggestions: &skills,
            queued_count: 0,
            slash_suggestions: &[],
            text: "/na",
        };
        let rows = plain(&render_composer(&props, 40));
        assert!(
            rows[0].trim_end().ends_with("▸ /navis — test skill navis"),
            "{rows:?}"
        );
        assert_eq!(rows[1].trim_end(), "    /nada — test skill nada");
        assert_eq!(rows[2], "─".repeat(40), "{rows:?}");
        let rule_index = rows
            .iter()
            .position(|row| row.chars().all(|c| c == '─'))
            .unwrap();
        assert_eq!(rule_index, 2, "menu rows must come before the top rule");
        assert!(!rows
            .iter()
            .skip(rule_index)
            .any(|row| row.contains("navis")));
    }

    #[test]
    fn skill_menu_clips_long_descriptions_on_narrow_widths() {
        let skills = vec![(
            "navis".to_string(),
            "an extremely long description that cannot possibly fit inside twenty columns"
                .to_string(),
        )];
        let props = ComposerProps {
            attachments: &[],
            cursor: 5,
            disabled: false,
            mention_suggestions: &[],
            selected_skill_index: 0,
            selected_suggestion_index: 0,
            skill_suggestions: &skills,
            queued_count: 0,
            slash_suggestions: &[],
            text: "/navi",
        };
        let rows = plain(&render_composer(&props, 20));
        assert!(rows[0].contains("…"), "{rows:?}");
        assert!(rows[0].chars().count() <= 20, "{rows:?}");
    }

    #[test]
    fn composer_disabled_placeholder() {
        let props = ComposerProps {
            attachments: &[],
            cursor: 0,
            disabled: true,
            mention_suggestions: &[],
            selected_skill_index: 0,
            selected_suggestion_index: 0,
            skill_suggestions: &[],
            queued_count: 0,
            slash_suggestions: &[],
            text: "",
        };
        let rows = plain(&render_composer(&props, 60));
        assert!(rows
            .iter()
            .any(|row| row.contains("running — press esc to stop the run")));
        assert!(!rows.iter().any(|row| row.contains("\u{1b}[7m")));
    }

    #[test]
    fn composer_hints_queue_and_steer_while_a_run_owns_the_composer() {
        let props = ComposerProps {
            attachments: &[],
            cursor: 0,
            disabled: true,
            mention_suggestions: &[],
            selected_skill_index: 0,
            selected_suggestion_index: 0,
            skill_suggestions: &[],
            queued_count: 0,
            slash_suggestions: &[],
            text: "",
        };
        let rows = plain(&render_composer(&props, 60));
        assert!(
            rows.iter().any(|row| row.contains("enter queues")),
            "{rows:?}"
        );
        assert!(
            rows.iter()
                .any(|row| row.contains("shift+enter steers the run")),
            "{rows:?}"
        );
    }

    #[test]
    fn composer_counts_queued_prompts_once_idle() {
        let props = ComposerProps {
            attachments: &[],
            cursor: 0,
            disabled: false,
            mention_suggestions: &[],
            selected_skill_index: 0,
            selected_suggestion_index: 0,
            skill_suggestions: &[],
            queued_count: 2,
            slash_suggestions: &[],
            text: "",
        };
        let rows = plain(&render_composer(&props, 60));
        assert!(rows.iter().any(|row| row.contains("2 queued")), "{rows:?}");
        assert!(
            rows.iter()
                .any(|row| row.contains("shift+enter steers the running goal")),
            "{rows:?}"
        );
    }

    #[test]
    fn picker_marks_selected_row() {
        let items = vec![
            PickerItem {
                detail: Some("first".to_string()),
                id: "one".to_string(),
                label: "alpha".to_string(),
            },
            PickerItem {
                detail: None,
                id: "two".to_string(),
                label: "beta".to_string(),
            },
        ];
        let rows = plain(&render_picker("pick", &items, 1, 40));
        assert_eq!(rows.len(), 6);
        assert!(rows.iter().any(|row| row.contains("▸ beta")));
        assert!(rows.iter().any(|row| row.contains("alpha — first")));
        assert!(!rows.iter().any(|row| row.contains("▸ alpha")));
        assert!(rows.iter().any(|row| row.contains("esc cancel")));
    }

    #[test]
    fn status_bar_truncates_to_width() {
        let props = StatusBarProps {
            active_skill_names: &["a".to_string(), "b".to_string()],
            cwd: "/tmp",
            model_label: "model-x",
            running: false,
            running_detail: None,
            session_id: "1234567890abcdef",
        };
        let rows = plain(&render_status_bar(&props, 30));
        assert_eq!(rows.len(), 1);
        assert!(rows[0].chars().count() <= 30);
    }

    #[test]
    fn skill_menu_clips_long_names_on_narrow_widths() {
        let skills = vec![(
            "an-absurdly-long-skill-name-that-cannot-fit".to_string(),
            "d".to_string(),
        )];
        let props = ComposerProps {
            attachments: &[],
            cursor: 3,
            disabled: false,
            mention_suggestions: &[],
            selected_skill_index: 0,
            selected_suggestion_index: 0,
            skill_suggestions: &skills,
            queued_count: 0,
            slash_suggestions: &[],
            text: "/an",
        };
        let rows = plain(&render_composer(&props, 20));
        assert!(rows[0].contains("…"), "{rows:?}");
        assert!(rows[0].chars().count() <= 20, "{rows:?}");
        assert!(rows[0].contains("/an-absurd"), "{rows:?}");
    }

    #[test]
    fn skill_menu_marks_the_selected_row_among_several() {
        let skills = vec![
            ("navis".to_string(), "one".to_string()),
            ("nada".to_string(), "two".to_string()),
            ("nab".to_string(), "three".to_string()),
        ];
        let props = ComposerProps {
            attachments: &[],
            cursor: 3,
            disabled: false,
            mention_suggestions: &[],
            selected_skill_index: 2,
            selected_suggestion_index: 0,
            skill_suggestions: &skills,
            queued_count: 0,
            slash_suggestions: &[],
            text: "/na",
        };
        let rows = plain(&render_composer(&props, 40));
        assert!(rows[0].contains("    /navis — one"), "{rows:?}");
        assert!(rows[1].contains("    /nada — two"), "{rows:?}");
        assert!(rows[2].contains("▸ /nab — three"), "{rows:?}");
        assert!(rows[3].chars().all(|c| c == '─'), "{rows:?}");
    }

    #[test]
    fn skill_menu_yields_to_the_builtin_slash_menu() {
        let skills = vec![("navis".to_string(), "one".to_string())];
        let first_builtin = crate::cli::commands::SLASH_COMMANDS[0].name;
        let builtins: Vec<&crate::cli::commands::SlashCommandSpec> =
            crate::cli::commands::SLASH_COMMANDS.iter().collect();
        let props = ComposerProps {
            attachments: &[],
            cursor: 1,
            disabled: false,
            mention_suggestions: &[],
            selected_skill_index: 0,
            selected_suggestion_index: 0,
            skill_suggestions: &skills,
            queued_count: 0,
            slash_suggestions: &builtins,
            text: "/",
        };
        let rows = plain(&render_composer(&props, 40));
        // Both suggestion sources are non-empty: the builtin slash menu wins
        // and no skill rows may render.
        assert!(
            rows.iter().all(|row| !row.contains("/navis — one")),
            "{rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains(first_builtin)),
            "{rows:?}"
        );
    }

    #[test]
    fn skill_menu_hidden_while_composer_disabled() {
        let skills = vec![("navis".to_string(), "one".to_string())];
        let props = ComposerProps {
            attachments: &[],
            cursor: 6,
            disabled: true,
            mention_suggestions: &[],
            selected_skill_index: 0,
            selected_suggestion_index: 0,
            skill_suggestions: &skills,
            queued_count: 0,
            slash_suggestions: &[],
            text: "/navis",
        };
        let rows = plain(&render_composer(&props, 40));
        // A disabled composer must not paint skill rows even when the filter
        // has matches.
        assert!(
            rows.iter().all(|row| !row.contains("/navis \u{2014} one")),
            "skill menu rendered while composer disabled: {rows:?}"
        );
    }
}
