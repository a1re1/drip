//! Pure render functions for the composer, picker and status bar. Each
//! function returns one
//! painted `String` per terminal row, with no trailing newline. Input
//! handling is owned by the app; picker state is passed in.

use crate::cli::images::GoalImageAttachment;
use crate::tui::theme::{paint, ACCENT_COLOR, DIM_COLOR};
use crate::watch::ansi::{fit, string_width, wrap_ansi};

use crate::cli::commands::SlashCommandSpec;

const INVERSE_ON: &str = "\u{1b}[7m";
const INVERSE_OFF: &str = "\u{1b}[27m";

/// Ink `borderStyle="round"` with `paddingX=1`: top `╭─...─╮`, content rows
/// padded or wrapped to `width - 4`, bottom `╰─...─╯`. The border glyphs are
/// painted with `theme::paint(color)`.
pub fn boxed(rows: Vec<String>, width: usize, color: &str) -> Vec<String> {
    boxed_titled(rows, width, color, None)
}

/// `boxed` with an optional caption set into the top rule, right-aligned
/// one dash in from the corner (Claude Code's session-name placement):
/// `╭──── name ─╮`. The caption is dimmed and clipped so the rule always
/// stays exactly `width` wide; `None` draws the plain rule.
pub fn boxed_titled(
    rows: Vec<String>,
    width: usize,
    color: &str,
    title: Option<&str>,
) -> Vec<String> {
    let width = width.max(6);
    let paint = paint(color);
    let horizontal: String = std::iter::repeat('─').take(width - 2).collect();
    let mut out = Vec::with_capacity(rows.len() + 2);
    out.push(top_rule(&horizontal, &paint, title));
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

/// The box's top rule: the plain `╭─...─╮` when there is no caption; with
/// one, `╭` + leading dashes + ` caption ` + `─╮`, the caption clipped to the
/// rule's interior so the row never grows past the box width. A caption that
/// cannot fit at all (very narrow terminal) yields the plain rule.
fn top_rule(horizontal: &str, paint: &dyn Fn(&str) -> String, title: Option<&str>) -> String {
    let rule_width = horizontal.chars().count();
    let Some(title) = title.map(str::trim).filter(|title| !title.is_empty()) else {
        return paint(&format!("╭{horizontal}╮"));
    };
    // Reserve one leading dash, the spaces around the caption, and the
    // trailing dash so the caption always sits inside the rule.
    let Some(caption_room) = rule_width.checked_sub(4).filter(|room| *room > 0) else {
        return paint(&format!("╭{horizontal}╮"));
    };
    // fit pads to the full room; the caption keeps only its own width so the
    // dashes, not trailing spaces, fill the rule to its left.
    let caption = fit(title, caption_room, true).trim_end().to_string();
    let lead = "─".repeat(rule_width - string_width(&caption) - 3);
    let dim = crate::tui::theme::paint(DIM_COLOR);
    format!(
        "{}{}{}",
        paint(&format!("╭{lead} ")),
        dim(&caption),
        paint(" ─╮")
    )
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
    /// The session's explicit `/rename` name, captioned into the box's top
    /// rule; `None` (never renamed) draws the plain rule.
    pub session_name: Option<&'a str>,
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
    rows.extend(boxed_titled(body_rows, width, border_color, props.session_name));

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

/// Stepped clarification survey (Claude-Code-style): the question's header as
/// an inverse accent chip with `question N of M`, the question in bold, one
/// numbered row per option with its description indented underneath, an
/// optional `Type something.` row, and always a final `Chat about this` row.
/// `options` are the listed choices only — the two escape-hatch rows are
/// generated here so their numbering always matches the app's `PickerItem`
/// list (`allow_other` controls whether the first of them exists).
pub fn render_survey(
    header: &str,
    question: &str,
    options: &[PickerItem],
    allow_other: bool,
    selected_index: usize,
    question_number: usize,
    question_total: usize,
    width: usize,
) -> Vec<String> {
    use crate::watch::ansi::c::{bold, reverse};

    let accent = paint(ACCENT_COLOR);
    let dim = paint(DIM_COLOR);
    let inner = width.saturating_sub(4).max(1);
    let mut rows: Vec<String> = Vec::new();
    rows.push(format!(
        "{}{}",
        accent(&reverse(&bold(header))),
        dim(&format!(" question {question_number} of {question_total}"))
    ));
    rows.push(String::new());
    rows.extend(wrap_ansi(&bold(question), inner));
    rows.push(String::new());
    for (index, item) in options.iter().enumerate() {
        let selected = index == selected_index;
        let prefix = if selected { accent("❯ ") } else { "  ".to_string() };
        let label = if selected { accent(&item.label) } else { item.label.clone() };
        rows.push(format!("{prefix}{}. {label}", index + 1));
        if let Some(description) = &item.detail {
            rows.push(format!("   {}", dim(description)));
        }
    }
    let mut number = options.len();
    if allow_other {
        number += 1;
        let selected = selected_index == number - 1;
        let prefix = if selected { accent("❯ ") } else { "  ".to_string() };
        let label = if selected {
            accent("Type something.")
        } else {
            "Type something.".to_string()
        };
        rows.push(format!("{prefix}{number}. {label}"));
    }
    number += 1;
    let selected = selected_index == number - 1;
    let prefix = if selected { accent("❯ ") } else { "  ".to_string() };
    let label = if selected {
        accent("Chat about this")
    } else {
        "Chat about this".to_string()
    };
    rows.push(format!("{prefix}{number}. {label}"));
    rows.push(String::new());
    rows.push(dim("Enter to select · ↑/↓ to navigate · 1-9 to jump · Esc to cancel"));
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
    fn boxed_titled_sets_the_caption_into_the_top_rule() {
        let rows = boxed_titled(vec!["ab".to_string()], 16, DIM_COLOR, Some("test"));
        let plain_rows = plain(&rows);
        assert_eq!(plain_rows[0], "╭─────── test ─╮");
        assert_eq!(plain_rows[0].chars().count(), 16, "the rule stays box-wide");
        assert_eq!(plain_rows[1], "│ ab           │");
        // Blank captions draw the plain rule; long ones clip to the interior.
        let blank = plain(&boxed_titled(vec![], 16, DIM_COLOR, Some("   ")));
        assert_eq!(blank[0], "╭──────────────╮");
        let long = plain(&boxed_titled(vec![], 16, DIM_COLOR, Some("a much longer session name")));
        assert_eq!(long[0].chars().count(), 16);
        assert!(long[0].starts_with("╭─ a much"), "{}", long[0]);
        assert!(long[0].ends_with("… ─╮"), "{}", long[0]);
    }

    #[test]
    fn composer_captions_the_session_name_on_the_box() {
        let props = ComposerProps {
            attachments: &[],
            cursor: 0,
            disabled: false,
            mention_suggestions: &[],
            selected_skill_index: 0,
            selected_suggestion_index: 0,
            skill_suggestions: &[],
            queued_count: 0,
            session_name: Some("test"),
            slash_suggestions: &[],
            text: "",
        };
        let rows = plain(&render_composer(&props, 40));
        assert_eq!(rows[0], format!("╭{} test ─╮", "─".repeat(31)));
        // While a run owns the composer the caption stays on the dimmed box.
        let running = ComposerProps { disabled: true, ..props };
        let rows = plain(&render_composer(&running, 40));
        assert!(rows[0].ends_with(" test ─╮"), "{}", rows[0]);
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
            session_name: None,
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
            selected_skill_index: 0,
            selected_suggestion_index: 0,
            skill_suggestions: &[],
            queued_count: 0,
            session_name: None,
            slash_suggestions: &[],
            text: "line one\nline two",
        };
        let rows = plain(&render_composer(&props, 20));
        // ╭, "❯ line one", "  line two", ╰, plus the enter/shift+enter hint
        // row — continuation rows sit under the text; the cursor on the
        // newline is an inverse cell after "one".
        assert_eq!(rows.len(), 5, "{rows:?}");
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
            selected_skill_index: 0,
            selected_suggestion_index: 0,
            skill_suggestions: &[],
            queued_count: 0,
            session_name: None,
            slash_suggestions: &[],
            text: "@hel",
        };
        let rows = plain(&render_composer(&props, 40));
        assert_eq!(rows[3], "  ▸ @hello.txt");
        assert_eq!(rows[4], "    @help.md");
    }

    #[test]
    fn skill_menu_rows_render_above_the_composer_box() {
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
            session_name: None,
            slash_suggestions: &[],
            text: "/na",
        };
        let rows = plain(&render_composer(&props, 40));
        assert!(
            rows[0].trim_end().ends_with("▸ /navis — test skill navis"),
            "{rows:?}"
        );
        assert_eq!(rows[1].trim_end(), "    /nada — test skill nada");
        assert!(rows[2].starts_with("╭"), "{rows:?}");
        let box_index = rows.iter().position(|row| row.starts_with("╭")).unwrap();
        assert!(
            rows.iter()
                .take(box_index)
                .any(|row| row.contains("/navis")),
            "menu rows must come before the box"
        );
        assert!(!rows.iter().skip(box_index).any(|row| row.contains("navis")));
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
            session_name: None,
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
            session_name: None,
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
            session_name: None,
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
            session_name: None,
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
    fn survey_renders_numbered_options_descriptions_and_the_footer() {
        let items = vec![
            PickerItem {
                detail: Some("watch the file".to_string()),
                id: "poll".to_string(),
                label: "Poll".to_string(),
            },
            PickerItem {
                detail: Some("read the pipe".to_string()),
                id: "chan".to_string(),
                label: "Channel".to_string(),
            },
        ];
        let rows = plain(&render_survey("Approach", "Poll or channel?", &items, true, 0, 1, 2, 72));
        assert!(rows
            .iter()
            .any(|row| row.contains("Approach") && row.contains("question 1 of 2")));
        assert!(rows.iter().any(|row| row.contains("Poll or channel?")));
        assert!(rows.iter().any(|row| row.contains("❯ 1. Poll")));
        assert!(rows.iter().any(|row| row.contains("2. Channel")));
        assert!(rows.iter().any(|row| row.contains("   watch the file")));
        assert!(rows.iter().any(|row| row.contains("   read the pipe")));
        assert!(rows.iter().any(|row| row.contains("3. Type something.")));
        assert!(rows.iter().any(|row| row.contains("4. Chat about this")));
        assert!(rows
            .iter()
            .any(|row| row.contains("Enter to select · ↑/↓ to navigate · 1-9 to jump · Esc to cancel")));
        assert!(rows.first().unwrap().starts_with('╭'));
        assert!(rows.last().unwrap().starts_with('╰'));
    }

    #[test]
    fn survey_hides_type_something_and_marks_the_chat_row_when_selected() {
        let items = vec![PickerItem {
            detail: None,
            id: "yes".to_string(),
            label: "Yes".to_string(),
        }];
        let rows = plain(&render_survey("Scope", "Include tests?", &items, false, 1, 2, 2, 72));
        assert!(rows.iter().any(|row| row.contains("❯ 2. Chat about this")));
        assert!(!rows.iter().any(|row| row.contains("Type something.")));
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
            session_name: None,
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
            session_name: None,
            slash_suggestions: &[],
            text: "/na",
        };
        let rows = plain(&render_composer(&props, 40));
        assert!(rows[0].contains("    /navis — one"), "{rows:?}");
        assert!(rows[1].contains("    /nada — two"), "{rows:?}");
        assert!(rows[2].contains("▸ /nab — three"), "{rows:?}");
        assert!(rows[3].starts_with("╭"), "{rows:?}");
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
            session_name: None,
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
            session_name: None,
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
