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

/// The composer's top rule: a plain dim `─` line, or with a `/rename`
/// caption, the name set into it right-aligned one dash in from the edge
/// (`──── name ─`, the same placement the boxed picker uses). The caption is
/// clipped so the rule always stays exactly `width` wide; a caption that
/// cannot fit at all yields the plain rule.
fn composer_rule(width: usize, title: Option<&str>) -> String {
    let dim = paint(DIM_COLOR);
    let plain = dim(&"─".repeat(width));
    let Some(title) = title.map(str::trim).filter(|title| !title.is_empty()) else {
        return plain;
    };
    // One leading dash, the spaces around the caption, and the trailing dash.
    let Some(caption_room) = width.checked_sub(4).filter(|room| *room > 0) else {
        return plain;
    };
    let caption = fit(title, caption_room, true).trim_end().to_string();
    let lead = "─".repeat(width - string_width(&caption) - 3);
    dim(&format!("{lead} {caption} ─"))
}

/// Display columns the composer body gets at terminal `width`: the two-column
/// `❯ `/indent prefix and one spare column for the inverse cursor cell at the
/// end of a full line are reserved, so a body row never exceeds `width`.
pub fn composer_text_width(width: usize) -> usize {
    width.max(6) - 3
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
    /// Prompts waiting for the next run, oldest first. They are listed above
    /// the composer (never in the timeline) so the whole queue stays visible.
    pub queued: &'a [String],
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

/// One dim row per queued prompt, listed in send order above the composer.
/// Multi-line prompts show their first line, and long ones clip with an
/// ellipsis, so the queue reads as a compact "what's next" list.
fn queued_rows(queued: &[String], width: usize) -> Vec<String> {
    if queued.is_empty() {
        return Vec::new();
    }
    let dim = paint(DIM_COLOR);
    let line_width = width.max(6).saturating_sub(4).max(1);
    let mut rows = Vec::with_capacity(queued.len() + 1);
    let noun = if queued.len() == 1 { "message" } else { "messages" };
    rows.push(dim(&fit(
        &format!("{} queued {noun} for the next run", queued.len()),
        width.max(1),
        true,
    )));
    for (index, text) in queued.iter().enumerate() {
        let first_line = text.lines().next().unwrap_or("").trim();
        let more = if text.lines().count() > 1 { " …" } else { "" };
        let line = fit(&format!("{}. {first_line}{more}", index + 1), line_width, true);
        rows.push(dim(&format!("  {line}")));
    }
    rows
}

/// Port of the Ink `Composer` component.
pub fn render_composer(props: &ComposerProps, width: usize) -> Vec<String> {
    let mut rows: Vec<String> = queued_rows(props.queued, width);

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
    let queued_placeholder = format!(
        "{} queued — ctrl+s steers the run with them all",
        props.queued.len()
    );
    let display: &str = if placeholder && !props.queued.is_empty() {
        &queued_placeholder
    } else if placeholder {
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
    let cursor_line = if placeholder {
        // Only the run placeholder hides the cursor: a draft typed while a
        // goal runs keeps its cursor cell so the writer can see where they are.
        usize::MAX
    } else {
        composer_cursor_position(display, text_width, props.cursor).0
    };
    let text_chars: Vec<char> = display.chars().collect();

    let rule = dim(&"─".repeat(width));
    rows.push(composer_rule(width, props.session_name));
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
        // Only the placeholder is dimmed: a draft typed while a goal runs
        // reads in the same colour as any other typing.
        let body = if placeholder { dim(&body) } else { body };
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
        if !props.queued.is_empty() {
            "enter queues · ctrl+s steers the run (whole queue when empty)".to_string()
        } else {
            "enter queues · ctrl+s steers the run".to_string()
        }
    } else if !props.queued.is_empty() {
        format!("{} queued — ctrl+s runs the next one", props.queued.len())
    } else {
        "enter sends · ctrl+s steers with a queued message".to_string()
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
/// `options` are the listed choices only — the escape-hatch rows (a confirm
/// row for a "select all that apply" question, `Type something.`, and the
/// final `Chat about this`) are generated here so their numbering always
/// matches the app's `PickerItem` list. `toggled` marks the option rows the
/// operator ticked with space; an empty slice renders a plain question.
pub fn render_survey(
    header: &str,
    question: &str,
    options: &[PickerItem],
    toggled: &[bool],
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
    let multiple = !toggled.is_empty();
    for (index, item) in options.iter().enumerate() {
        let selected = index == selected_index;
        let prefix = if selected {
            accent("❯ ")
        } else {
            "  ".to_string()
        };
        let mark = if multiple {
            if toggled.get(index).copied().unwrap_or(false) {
                "[x] "
            } else {
                "[ ] "
            }
        } else {
            ""
        };
        let label = if selected {
            accent(&item.label)
        } else {
            item.label.clone()
        };
        rows.push(format!("{prefix}{mark}{}. {label}", index + 1));
        if let Some(description) = &item.detail {
            rows.push(format!("   {}", dim(description)));
        }
    }
    let mut number = options.len();
    if multiple {
        // Exactly where open_survey_question appends it: after the option
        // rows, before `Type something.` and `Chat about this`.
        number += 1;
        let selected = selected_index == number - 1;
        let prefix = if selected {
            accent("❯ ")
        } else {
            "  ".to_string()
        };
        let label = if selected {
            accent("Confirm selection")
        } else {
            "Confirm selection".to_string()
        };
        rows.push(format!("{prefix}{number}. {label}"));
        rows.push(format!(
            "   {}",
            dim("enter records the options marked [x]")
        ));
    }
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
    rows.push(dim(if multiple {
        "Space toggles · Enter confirms the selection · ↑/↓ to navigate · Esc to cancel"
    } else {
        "Enter to select · ↑/↓ to navigate · 1-9 to jump · Esc to cancel"
    }));
    boxed(rows, width, ACCENT_COLOR)
}

/// One row of the interactive `/skills` picker (display data only).
#[derive(Clone, Debug)]
pub struct SkillPickerItem {
    pub description: String,
    pub enabled: bool,
    pub locked: bool,
    pub name: String,
    /// Where the skill came from: `project`, `user`, `builtin`, or the
    /// marketplace key for a plugin skill.
    pub source: String,
    /// Rough token size (byte length / 4), the Claude-style `~N tok` figure.
    pub tokens: usize,
}

/// The interactive `/skills` picker: a header with the counts, a search line,
/// up to eight rows (on/off mark, origin, rough size) with the highlight kept
/// visible by a sliding window, and a footer. Pure render — the app owns the
/// filter, the selection and the toggle side effects, so closing the picker
/// leaves nothing in scrollback.
pub fn render_skill_picker(
    query: &str,
    items: &[SkillPickerItem],
    selected_index: usize,
    total: usize,
    width: usize,
) -> Vec<String> {
    use crate::watch::ansi::c::{bold, green, yellow};

    let accent = paint(ACCENT_COLOR);
    let dim = paint(DIM_COLOR);
    /// Rows of the list shown at once; the window slides to follow the cursor.
    const WINDOW: usize = 8;

    let enabled = items.iter().filter(|item| item.enabled).count();
    let mut rows: Vec<String> = vec![
        accent(&bold("Skills")),
        dim(&format!(
            "{} of {total} skills · {enabled} on · enter/space toggles · ↑/↓ move · esc closes",
            items.len()
        )),
        String::new(),
    ];
    let search = if query.is_empty() {
        dim("type to search…")
    } else {
        format!("{}{}", query, accent("▏"))
    };
    rows.push(format!("{}{search}", accent("› ")));
    rows.push(String::new());

    let start = if selected_index >= WINDOW {
        selected_index + 1 - WINDOW
    } else {
        0
    };
    for (index, item) in items.iter().enumerate().skip(start).take(WINDOW) {
        let selected = index == selected_index;
        let cursor = if selected {
            accent("❯ ")
        } else {
            "  ".to_string()
        };
        // A locked marketplace row keeps its mark visible but dimmed: plugin
        // skills are managed through /marketplace, never toggled here.
        let mark = if item.locked {
            yellow("×")
        } else if item.enabled {
            green("✔")
        } else {
            dim("○")
        };
        let name = if selected {
            accent(&item.name)
        } else if item.locked {
            dim(&item.name)
        } else {
            item.name.clone()
        };
        rows.push(format!(
            "{cursor}{mark} {name} · {} · ~{} tok",
            dim(&item.source),
            item.tokens
        ));
        if selected && !item.description.is_empty() {
            rows.push(format!("      {}", dim(&item.description)));
        }
    }
    if items.is_empty() {
        rows.push(dim("no skills match — backspace to clear the search"));
    }
    rows.push(String::new());
    rows.push(dim("plugin skills are managed with /marketplace"));
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
    fn composer_captions_the_session_name_on_the_top_rule() {
        let props = ComposerProps {
            attachments: &[],
            cursor: 0,
            disabled: false,
            mention_suggestions: &[],
            selected_skill_index: 0,
            selected_suggestion_index: 0,
            skill_suggestions: &[],
            queued: &[],
            session_name: Some("test"),
            slash_suggestions: &[],
            text: "",
        };
        let rows = plain(&render_composer(&props, 40));
        assert_eq!(rows[0], format!("{} test ─", "─".repeat(33)));
        assert_eq!(rows[0].chars().count(), 40, "the rule stays terminal-wide");
        // While a run owns the composer the caption stays on the top rule.
        let running = ComposerProps { disabled: true, ..props };
        let rows = plain(&render_composer(&running, 40));
        assert!(rows[0].ends_with(" test ─"), "{}", rows[0]);
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
            queued: &[],
            session_name: None,
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
            queued: &[],
            session_name: None,
            slash_suggestions: &[],
            text: "line one\nline two",
        };
        let rows = plain(&render_composer(&props, 20));
        // Rule, "❯ line one" (the cursor on the newline is an inverse cell at
        // the line end), the two-space continuation row, the closing rule, then
        // the enter/ctrl+s hint.
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
            queued: &[],
            session_name: None,
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
            queued: &[],
            session_name: None,
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
            queued: &[],
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
            queued: &[],
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
            queued: &[],
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
    fn composer_keeps_the_cursor_on_a_draft_typed_while_a_run_owns_it() {
        let props = ComposerProps {
            attachments: &[],
            cursor: 3,
            disabled: true,
            mention_suggestions: &[],
            selected_skill_index: 0,
            selected_suggestion_index: 0,
            skill_suggestions: &[],
            queued: &[],
            session_name: None,
            slash_suggestions: &[],
            text: "queued draft",
        };
        let rows = render_composer(&props, 60);
        let body = rows.iter().find(|row| row.contains("ed draft")).unwrap();
        assert!(body.contains(&format!("que{INVERSE_ON}u{INVERSE_OFF}ed")), "{body:?}");
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
            queued: &[],
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
                .any(|row| row.contains("ctrl+s steers the run")),
            "{rows:?}"
        );
    }

    #[test]
    fn composer_lists_the_whole_queue_above_the_top_rule() {
        let queued = vec![
            "first queued".to_string(),
            "second line one\nsecond line two".to_string(),
        ];
        let props = ComposerProps {
            attachments: &[],
            cursor: 0,
            disabled: true,
            mention_suggestions: &[],
            selected_skill_index: 0,
            selected_suggestion_index: 0,
            skill_suggestions: &[],
            queued: &queued,
            session_name: None,
            slash_suggestions: &[],
            text: "",
        };
        let rows = plain(&render_composer(&props, 60));
        assert_eq!(rows[0].trim_end(), "2 queued messages for the next run", "{rows:?}");
        assert_eq!(rows[1].trim_end(), "  1. first queued", "{rows:?}");
        assert_eq!(rows[2].trim_end(), "  2. second line one …", "{rows:?}");
        assert_eq!(rows[3], "─".repeat(60), "the rule follows the queue");
        // With nothing typed the composer itself carries the steer-all hint,
        // and no per-message hint is repeated anywhere.
        assert!(
            rows[4].contains("2 queued — ctrl+s steers the run with them all"),
            "{rows:?}"
        );
        assert!(
            !rows.iter().any(|row| row.contains("steers the running goal with it now")),
            "{rows:?}"
        );
    }

    #[test]
    fn composer_draft_typed_mid_run_is_not_dimmed() {
        let queued = vec!["first".to_string()];
        let props = ComposerProps {
            attachments: &[],
            cursor: 5,
            disabled: true,
            mention_suggestions: &[],
            selected_skill_index: 0,
            selected_suggestion_index: 0,
            skill_suggestions: &[],
            queued: &queued,
            session_name: None,
            slash_suggestions: &[],
            text: "draft",
        };
        let rows = render_composer(&props, 60);
        let gray = paint(DIM_COLOR)("draft");
        let body = rows.iter().find(|row| row.contains("draft")).unwrap();
        assert!(!body.contains(&gray), "typed text must not be dimmed: {body:?}");
        assert!(body.contains(&format!("draft{INVERSE_ON} {INVERSE_OFF}")), "{body:?}");
    }

    #[test]
    fn composer_counts_queued_prompts_once_idle() {
        let queued = vec!["first".to_string(), "second".to_string()];
        let props = ComposerProps {
            attachments: &[],
            cursor: 0,
            disabled: false,
            mention_suggestions: &[],
            selected_skill_index: 0,
            selected_suggestion_index: 0,
            skill_suggestions: &[],
            queued: &queued,
            session_name: None,
            slash_suggestions: &[],
            text: "",
        };
        let rows = plain(&render_composer(&props, 60));
        assert!(rows.iter().any(|row| row.contains("2 queued")), "{rows:?}");
        assert!(
            rows.iter()
                .any(|row| row.contains("ctrl+s runs the next one")),
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
        let rows = plain(&render_survey("Approach", "Poll or channel?", &items, &[], true, 0, 1, 2, 72));
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
    fn survey_marks_toggled_options_and_shows_the_confirm_row() {
        let items = vec![
            PickerItem {
                detail: Some("with tests".to_string()),
                id: "yes".to_string(),
                label: "Yes".to_string(),
            },
            PickerItem {
                detail: None,
                id: "no".to_string(),
                label: "No".to_string(),
            },
        ];
        let rows = plain(&render_survey(
            "Scope",
            "Which parts?",
            &items,
            &[true, false],
            false,
            1,
            1,
            1,
            72,
        ));
        assert!(
            rows.iter().any(|row| row.contains("[x] 1. Yes")),
            "{rows:?}"
        );
        assert!(rows.iter().any(|row| row.contains("[ ] 2. No")), "{rows:?}");
        assert!(
            rows.iter().any(|row| row.contains("3. Confirm selection")),
            "{rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("4. Chat about this")),
            "{rows:?}"
        );
        assert!(
            !rows.iter().any(|row| row.contains("Type something.")),
            "{rows:?}"
        );
        assert!(
            rows.iter()
                .any(|row| row.contains("Space toggles · Enter confirms the selection")),
            "{rows:?}"
        );
    }

    #[test]
    fn survey_hides_type_something_and_marks_the_chat_row_when_selected() {
        let items = vec![PickerItem {
            detail: None,
            id: "yes".to_string(),
            label: "Yes".to_string(),
        }];
        let rows = plain(&render_survey("Scope", "Include tests?", &items, &[], false, 1, 2, 2, 72));
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
            queued: &[],
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
            queued: &[],
            session_name: None,
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
            queued: &[],
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
            queued: &[],
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

    #[test]
    fn skill_picker_shows_marks_origin_size_and_slides_the_window() {
        let items: Vec<SkillPickerItem> = (0..10)
            .map(|index| SkillPickerItem {
                description: format!("desc {index}"),
                enabled: index % 2 == 0,
                locked: false,
                name: format!("skill-{index}"),
                source: "project".to_string(),
                tokens: 100 * (index + 1),
            })
            .collect();
        let rows = plain(&render_skill_picker("", &items, 9, 10, 60));
        assert!(
            rows.iter().any(|row| row.contains("10 of 10 skills")),
            "{rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("skill-9")
                && row.contains("~1000 tok")
                && row.contains("project")),
            "{rows:?}"
        );
        assert!(rows.iter().any(|row| row.contains('✔')), "{rows:?}");
        assert!(rows.iter().any(|row| row.contains('○')), "{rows:?}");
        // The window follows the cursor, so the first rows are paged out.
        assert!(!rows.iter().any(|row| row.contains("skill-0")), "{rows:?}");
    }

    #[test]
    fn skill_picker_marks_locked_marketplace_rows_and_shows_the_search() {
        let items = vec![SkillPickerItem {
            description: "gated".to_string(),
            enabled: false,
            locked: true,
            name: "spellcraft:navis".to_string(),
            source: "spellcraft/navis".to_string(),
            tokens: 42,
        }];
        let rows = plain(&render_skill_picker("nav", &items, 0, 1, 60));
        assert!(
            rows.iter()
                .any(|row| row.contains("spellcraft:navis") && row.contains("spellcraft/navis")),
            "{rows:?}"
        );
        assert!(rows.iter().any(|row| row.contains("nav▏")), "{rows:?}");
    }
}
