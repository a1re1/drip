//! Pure render functions for the composer, picker and status bar. Each
//! function returns one
//! painted `String` per terminal row, with no trailing newline. Input
//! handling is owned by the app; picker state is passed in.

use crate::cli::images::GoalImageAttachment;
use crate::tui::theme::{paint, ACCENT_COLOR, DIM_COLOR};
use crate::watch::ansi::{fit, string_width, strip_ansi, wrap_ansi};

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

/// Composer render inputs.
pub struct ComposerProps<'a> {
    pub attachments: &'a [GoalImageAttachment],
    pub cursor: usize,
    pub disabled: bool,
    pub mention_suggestions: &'a [String],
    pub selected_suggestion_index: usize,
    pub slash_suggestions: &'a [&'a SlashCommandSpec],
    pub text: &'a str,
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

    let border_color = if props.disabled { DIM_COLOR } else { ACCENT_COLOR };
    let prefix_paint = paint(border_color);
    let body = if props.disabled {
        let dim = paint(DIM_COLOR);
        if props.text.is_empty() {
            dim("running — press esc to stop the run")
        } else {
            dim(props.text)
        }
    } else {
        let before: String = props.text.chars().take(props.cursor).collect();
        let at: String = props.text.chars().skip(props.cursor).take(1).collect();
        let after: String = props.text.chars().skip(props.cursor + 1).collect();
        // A cursor on a newline is drawn as an inverse cell at the line's end,
        // then the break — never an inverse sequence spanning the split below.
        let (at, after) = match at.as_str() {
            "" => (" ".to_string(), after),
            "\n" => (" ".to_string(), format!("\n{after}")),
            _ => (at, after),
        };
        format!("{before}{INVERSE_ON}{at}{INVERSE_OFF}{after}")
    };
    // The prompt is a fixed-width sibling of the text in ink, so every line of
    // a multi-line goal (and every wrapped continuation) sits under the first
    // character of the text, inside the box.
    let inner = width.max(6) - 4;
    let text_width = inner.saturating_sub(2).max(1);
    let mut body_rows: Vec<String> = Vec::new();
    for line in body.split('\n') {
        // The inverse cursor cell drawn on a newline may sit one column past
        // the text width; the box's inner width still has room for it, and
        // wrap_ansi would otherwise drop it as a break space.
        let line_width = if line.ends_with(&format!("{INVERSE_ON} {INVERSE_OFF}")) { text_width + 1 } else { text_width };
        let pieces = if string_width(line) <= line_width { vec![line.to_string()] } else { wrap_ansi(line, line_width) };
        for piece in pieces {
            let lead = if body_rows.is_empty() { prefix_paint("❯ ") } else { "  ".to_string() };
            body_rows.push(format!("{lead}{piece}"));
        }
    }
    rows.extend(boxed(body_rows, width, border_color));

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
            selected_suggestion_index: 0,
            slash_suggestions: &[],
            text: "abc",
        };
        let rows = render_composer(&props, 40);
        assert!(rows.iter().any(|row| row.contains("\u{1b}[7mb\u{1b}[27m")));
    }

    #[test]
    fn composer_keeps_multi_line_text_inside_the_box() {
        let props = ComposerProps {
            attachments: &[],
            cursor: 8,
            disabled: false,
            mention_suggestions: &[],
            selected_suggestion_index: 0,
            slash_suggestions: &[],
            text: "line one\nline two",
        };
        let rows = plain(&render_composer(&props, 20));
        // ╭, "❯ line one", "  line two", ╰ — continuation rows sit under the
        // text; the cursor on the newline is an inverse cell after "one".
        assert_eq!(rows.len(), 4, "{rows:?}");
        assert_eq!(rows[1].trim_end_matches(" │").trim_end(), "│ ❯ line one");
        assert_eq!(rows[2].trim_end_matches(" │").trim_end(), "│   line two");
        assert!(rows.iter().all(|row| row.chars().count() == 20), "{rows:?}");
    }

    #[test]
    fn composer_menus_are_indented_like_ink_padding() {
        let mentions = vec!["hello.txt".to_string(), "help.md".to_string()];
        let props = ComposerProps {
            attachments: &[],
            cursor: 4,
            disabled: false,
            mention_suggestions: &mentions,
            selected_suggestion_index: 0,
            slash_suggestions: &[],
            text: "@hel",
        };
        let rows = plain(&render_composer(&props, 40));
        assert_eq!(rows[3], "  ▸ @hello.txt");
        assert_eq!(rows[4], "    @help.md");
    }

    #[test]
    fn composer_disabled_placeholder() {
        let props = ComposerProps {
            attachments: &[],
            cursor: 0,
            disabled: true,
            mention_suggestions: &[],
            selected_suggestion_index: 0,
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
}
