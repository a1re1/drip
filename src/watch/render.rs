// Pure frame renderer for the dripw watch TUI: render_frame(vm, cols, rows)
// returns exactly `rows` lines, each exactly `cols` visible columns. No I/O,
// no clock — `vm.now` is the only time it may read.

use std::collections::{BTreeMap, HashMap};

use crate::cli::transcript::{format_model_route_lines, TranscriptEntry};
use crate::core::sessions::SessionRecord;
use crate::core::types::{HarnessEventType, HarnessTask, HarnessTaskStatus};
use crate::watch::ansi::{c, char_width, fit, string_width, strip_ansi};
use crate::watch::ps::PsProc;
pub use crate::watch::transcript_view::{flatten_transcript, RowCell};

// Below this many columns the panes stack vertically (portrait).
pub const PORTRAIT_MAX_COLS: usize = 90;

const DOT: &str = "●";
const TRANSCRIPT_FLOOR: usize = 6; // content rows floor for [0] in portrait
const MIN_PANE: usize = 3; // border+border + 1 content row

// ── View model ───────────────────────────────────────────────────────────────

/// 1 | 2 | 3 | 4
pub type FocusPane = u8;

/// Which slice of the session index the [1] Sessions pane shows. `r` cycles it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionsMode {
    Running,
    Recent,
    All,
}

impl SessionsMode {
    /// Running → Recent → All → Running.
    pub fn next(self) -> Self {
        match self {
            SessionsMode::Running => SessionsMode::Recent,
            SessionsMode::Recent => SessionsMode::All,
            SessionsMode::All => SessionsMode::Running,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SessionsMode::Running => "running",
            SessionsMode::Recent => "recent",
            SessionsMode::All => "all",
        }
    }
}

#[derive(Debug, Clone)]
pub struct WatchViewModel {
    /// Epoch ms — the only clock render_frame may read.
    pub now: i64,
    /// The [1] Sessions pane's filter.
    pub mode: SessionsMode,
    /// Rows visible under `mode`, already filtered by the app: Running → the
    /// live-lease sessions, Recent → all others, All → running then recent.
    pub sessions: Vec<SessionRecord>,
    /// Box-drawing connector for each row of `sessions`, same length and order:
    /// the dripw rendering of the parent/child tree produced by `tree_rows`.
    pub session_prefixes: Vec<String>,
    /// Lease started_at as epoch ms, keyed by session id (running only).
    pub started_at_ms: HashMap<String, i64>,
    pub focus: FocusPane,
    /// Selection index into `sessions` (clamped by the app). The transcript
    /// always follows this row.
    pub sel_session: usize,
    pub transcript: Vec<TranscriptEntry>,
    /// When true, footer_note on the transcript pane reads "following".
    pub following: bool,
    /// Scroll lines up from the newest end (0 = pinned to bottom).
    pub transcript_scroll: usize,
    /// Child processes of the focused running session (empty when none).
    pub shells: Vec<PsProc>,
    /// Selection index into shells (used when focus == 3).
    pub sel_shell: usize,
    /// Raw stdout/stderr lines tailed for the selected shell (focus == 3).
    pub shell_log_lines: Vec<String>,
    /// Files discovered behind the selected shell's fd 1/2 — tells the empty
    /// states "no tailable fds" and "tailing, nothing yet" apart.
    pub shell_log_files: Vec<String>,
    /// The focused session's captured MCP tool advertisements — what the
    /// servers that session spawned said about their own tools during the
    /// `tools/list` handshake, recorded into its directory by the run itself.
    /// Empty when it spawned no server or predates the snapshot file; the
    /// renderer never launches a server to fill it.
    pub mcp_tools: Vec<crate::tools::mcp::advertise::McpToolAdvertisement>,
    /// The focused session's task ledger (empty when it has none / unreadable).
    pub tasks: Vec<HarnessTask>,
    /// Selection index into the ordered task list (used when focus == 2).
    pub sel_task: usize,
    /// The focused session's loaded skills, one entry per loop, oldest first —
    /// read from the loop-start telemetry of `transcript`. Empty when the
    /// session recorded none (or predates the field).
    pub skill_loads: Vec<SkillLoad>,
    /// Page shown by the [4] Skills, Tools & Plans pane when its wrapped content does
    /// not fit the pane (the renderer clamps it to the real pages).
    pub skill_page: usize,
    /// Global index into `skill_items` of the skill/tool picked in the [4]
    /// pane — `None` means nothing picked, and the [0] column keeps showing
    /// the transcript.
    pub sel_skill: Option<usize>,
    /// The picked item's read-up (SKILL.md / tool listing), loaded by the app
    /// because the renderer does no I/O. `None` while nothing is picked.
    pub skill_detail: Option<SkillDetail>,
    /// Rows scrolled into the read-up body while it holds more than the [0]
    /// column seats. The box clamps it; the app zeroes it on a new pick.
    pub skill_detail_scroll: usize,
}

/// A skill or tool the [4] pane lists, named as the pane shows it — the
/// telemetry name, no `(2 loops)` count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillItem {
    Skill(String),
    Tool(String),
}

impl SkillItem {
    /// The bare name (what the pane's labels carry, minus any count suffix).
    pub fn name(&self) -> &str {
        match self {
            SkillItem::Skill(name) | SkillItem::Tool(name) => name,
        }
    }

    /// `skill` / `tool` — the word the read-up title uses.
    pub fn kind_label(&self) -> &'static str {
        match self {
            SkillItem::Skill(_) => "skill",
            SkillItem::Tool(_) => "tool",
        }
    }
}

/// Where one item's label sits in the [4] pane's wrapped content: `first` is
/// the content-relative row its name starts on (column `first_col` of it) and
/// `last` the last row it spans (ending at column `last_col`). Two names that
/// share a row therefore still resolve apart, by the column clicked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillItemRow {
    pub item: SkillItem,
    pub first: usize,
    pub first_col: usize,
    pub last: usize,
    pub last_col: usize,
}

/// The read-up shown in the [0] column while a skill/tool is picked in [4]:
/// the title (`skill · navis`) and the already-rendered body rows.
#[derive(Debug, Clone)]
pub struct SkillDetail {
    pub title: String,
    pub lines: Vec<RowCell>,
}

// ── Time helpers (pure; exported for tests) ──────────────────────────────────

/// Live elapsed timer: "m:ss" under an hour, "h:mm" at/above.
pub fn fmt_duration(total_sec: Option<i64>) -> String {
    let Some(total_sec) = total_sec else { return "--:--".to_string() };
    // A lease started_at marginally in the future (clock skew) reads as 0:00.
    let s = total_sec.max(0);
    if s < 3600 {
        return format!("{}:{:02}", s / 60, s % 60);
    }
    format!("{}:{:02}", s / 3600, (s % 3600) / 60)
}

/// Compact relative age from an ISO timestamp against `now` ms.
pub fn rel_time(iso: Option<&str>, now: i64) -> String {
    let Some(iso) = iso.filter(|s| !s.is_empty()) else { return "?".to_string() };
    let Ok(t) = chrono::DateTime::parse_from_rfc3339(iso) else { return "?".to_string() };
    let sec = ((now - t.timestamp_millis()) / 1000).max(0);
    if sec < 60 {
        format!("{sec}s")
    } else if sec < 3600 {
        format!("{}m", sec / 60)
    } else if sec < 86400 {
        format!("{}h", sec / 3600)
    } else {
        format!("{}d", sec / 86400)
    }
}

// ── Line-diff helper (exported for tests + dirty paint) ──────────────────────

/// Indices where `next[i] != prev[i]` (also any trailing length mismatch).
pub fn diff_lines(prev: &[String], next: &[String]) -> Vec<usize> {
    let n = prev.len().max(next.len());
    (0..n).filter(|&i| prev.get(i) != next.get(i)).collect()
}

fn plain(text: impl Into<String>, color: fn(&str) -> String) -> RowCell {
    RowCell {
        text: text.into(),
        color: Some(color),
        selected: false,
        sel_span: None,
        rich: false,
    }
}

fn selectable(text: impl Into<String>, color: fn(&str) -> String, selected: bool) -> RowCell {
    RowCell {
        text: text.into(),
        color: Some(color),
        selected,
        sel_span: None,
        rich: false,
    }
}

// ── Pane chrome ──────────────────────────────────────────────────────────────

/// Inline-titled border box. Emits exactly `height` lines of exactly `width`
/// visible columns. `rows` are content cells (not yet fitted).
pub fn render_pane(width: usize, height: usize, title: &str, focused: bool, rows: &[RowCell], footer_note: Option<&str>) -> Vec<String> {
    let width = width.max(1);
    let height = height.max(1);
    let border: fn(&str) -> String = if focused { c::accent } else { c::dim };
    let mut out: Vec<String> = Vec::new();

    // Top: ╭─ title ────╮  (left-aligned like sub-zero; clipped to fit `width`)
    if height >= 1 {
        let max_title = width.saturating_sub(4); // ╭─ + ─╮ around the padded title
        let padded_title = format!(" {title} ");
        let t = if width >= 6 { fit(&padded_title, string_width(&padded_title).min(max_title), true) } else { String::new() };
        let right_pad = width.saturating_sub(3 + string_width(&t));
        let top = format!(
            "{}{}{}{}",
            border("╭─"),
            if focused { c::accent_bold(&t) } else { c::bold(&t) },
            border(&"─".repeat(right_pad)),
            border("╮")
        );
        out.push(fit_ansi_line(&top, width));
    }

    let inner_h = height.saturating_sub(2);
    let inner_w = width.saturating_sub(2);
    for i in 0..inner_h {
        let cell = match rows.get(i) {
            None => fit("", inner_w, false),
            Some(row) if row.rich => {
                // Rich rows carry their own SGR styling and must survive verbatim: pad
                // to exactly inner_w visible columns; if one is somehow still wider,
                // fall back to clipping the stripped plain text so the frame invariant
                // (rows x exact cols) can never break.
                let w = string_width(&row.text);
                let padded = if w <= inner_w { format!("{}{}", row.text, " ".repeat(inner_w - w)) } else { fit(&strip_ansi(&row.text), inner_w, true) };
                if row.selected { c::on_cyan(&padded) } else { padded }
            }
            Some(row) => {
                let fitted = fit(&row.text, inner_w, true);
                let paint = row.color.unwrap_or(c::white);
                match (row.selected, row.sel_span) {
                    // A span pick (the [4] pane's skill/tool) lights its own
                    // columns only: the rest of the row keeps its colour, so a
                    // row shared with other names reads as one picked name
                    // instead of a lit-up line.
                    (true, Some((a, b))) => match split_cols(&fitted, a, b) {
                        Some((lead, mid, tail)) => {
                            format!("{}{}{}", paint(&lead), c::on_cyan(&mid), paint(&tail))
                        }
                        None => c::on_cyan(&fitted),
                    },
                    // on_cyan already pairs cyan bg with dark fg (46;30)
                    (true, None) => c::on_cyan(&fitted),
                    _ => paint(&fitted),
                }
            }
        };
        out.push(format!("{}{}{}", border("│"), cell, border("│")));
    }

    // Bottom: ╰─ note ─╯  (note right-aligned in the bottom border when present)
    if height >= 2 {
        let note = footer_note.unwrap_or("");
        let note_w = string_width(note);
        let inner = width.saturating_sub(2);
        if note_w > 0 && note_w + 2 <= inner {
            let left = inner.saturating_sub(note_w + 2);
            let bot = format!("{}{}{}{}", border("╰"), border(&"─".repeat(left)), c::dim(&format!(" {note} ")), border("╯"));
            // dim note adds spaces; ensure exact width
            out.push(fit_ansi_line(&bot, width));
        } else {
            out.push(border(&format!("╰{}╯", "─".repeat(width.saturating_sub(2)))));
        }
    }

    // Pad/truncate to exact height (defensive — callers pass height >= 3 normally)
    while out.len() < height {
        out.push(fit("", width, false));
    }
    out.truncate(height);
    out
}

/// Ensure a possibly-colored line is exactly `width` visible columns.
fn fit_ansi_line(line: &str, width: usize) -> String {
    let w = string_width(line);
    if w == width {
        return line.to_string();
    }
    if w < width {
        return format!("{line}{}", " ".repeat(width - w));
    }
    // Shouldn't happen for our borders; fall back to plain fit of stripped text.
    fit(&strip_ansi(line), width, true)
}

/// How wide the [0] read-up may wrap in a `cols` x `rows` terminal: the inner
/// width of the column it paints in, so its text reaches both borders instead
/// of stopping short of the right one.
pub fn read_up_inner_w(vm: &WatchViewModel, cols: usize, rows: usize) -> usize {
    transcript_region(vm, cols, rows)
        .map(|(_, _, c0, c1)| c1.saturating_sub(c0 + 1).max(1))
        .unwrap_or(40)
}

/// Split a fitted, ANSI-free row into the columns before, inside and after the
/// inclusive visible-column span `a..=b`. The three pieces re-join to the row,
/// so a partial highlight colours it without moving a character; `None` when
/// the span holds no column of this row (a truncated row, or an empty pick).
fn split_cols(text: &str, a: usize, b: usize) -> Option<(String, String, String)> {
    if b < a {
        return None;
    }
    let (mut lead, mut mid, mut tail) = (String::new(), String::new(), String::new());
    let mut col = 0usize;
    for ch in text.chars() {
        let target = if col < a {
            &mut lead
        } else if col <= b {
            &mut mid
        } else {
            &mut tail
        };
        target.push(ch);
        col += char_width(ch as u32);
    }
    (!mid.is_empty()).then_some((lead, mid, tail))
}

// ── Layout helpers ───────────────────────────────────────────────────────────

/// Content-hugging heights for portrait: each of the first N-1 panes gets
/// min(desired, remaining) with desired = max(1, count)+2; the last pane
/// (transcript) absorbs the rest and keeps a floor of ~TRANSCRIPT_FLOOR content
/// rows (+2 chrome) when budget allows. Mirrors sub-zero portraitHeights.
pub fn portrait_heights(body_h: usize, counts: &[usize]) -> Vec<usize> {
    let n = counts.len();
    if n == 0 {
        return Vec::new();
    }
    // desired: chrome(2) + max(1, count) content for list panes; transcript desired is generous
    let desired: Vec<usize> =
        counts.iter().enumerate().map(|(i, &ct)| if i == n - 1 { MIN_PANE.max(TRANSCRIPT_FLOOR + 2) } else { ct.max(1) + 2 }).collect();

    // First pass: give list panes their hug size, transcript the remainder.
    let mut heights = vec![MIN_PANE; n];
    let mut used = 0usize;
    for i in 0..n - 1 {
        heights[i] = desired[i];
        used += heights[i];
    }
    let last = body_h as i64 - used as i64;

    // If transcript fell below floor, steal from list panes (top-down) via cap.
    let floor_last = body_h.min(MIN_PANE.max(TRANSCRIPT_FLOOR + 2));
    if last < floor_last as i64 {
        let wanted: Vec<usize> = counts.iter().enumerate().map(|(i, &ct)| if i == n - 1 { floor_last } else { ct.max(1) + 2 }).collect();
        return cap_to_budget(&wanted, body_h);
    }
    // If we overshot (body_h tiny), cap everything.
    if last < MIN_PANE as i64 || used > body_h {
        let wanted: Vec<usize> = desired.iter().enumerate().map(|(i, &d)| if i == n - 1 { floor_last } else { d }).collect();
        return cap_to_budget(&wanted, body_h);
    }
    heights[n - 1] = last as usize;
    heights
}

/// Shrink desired heights from the top until they sum to budget; each ≥ 1.
pub fn cap_to_budget(desired: &[usize], budget: usize) -> Vec<usize> {
    let mut h: Vec<usize> = desired.iter().map(|&d| d.max(1)).collect();
    let mut used: usize = h.iter().sum();
    let mut i = 0;
    while used > budget && i < h.len() {
        let room = h[i] - 1;
        if room > 0 {
            let cut = room.min(used - budget);
            h[i] -= cut;
            used -= cut;
        }
        if h[i] <= 1 {
            i += 1;
        }
    }
    // Fix residual rounding on the last pane.
    if let Some(last) = h.last_mut() {
        if used < budget {
            *last += budget - used;
        } else if used > budget {
            *last = last.saturating_sub(used - budget).max(1);
        }
    }
    h
}

fn split_heights(total_h: usize, weights: &[usize]) -> Vec<usize> {
    let sum: usize = weights.iter().sum::<usize>().max(1);
    let mut heights: Vec<usize> = weights.iter().map(|&w| MIN_PANE.max(total_h * w / sum)).collect();
    let used: i64 = heights.iter().sum::<usize>() as i64;
    let max_weight = weights.iter().copied().max().unwrap_or(0);
    let idx = weights.iter().position(|&w| w == max_weight).unwrap_or(heights.len().saturating_sub(1));
    let adjusted = heights[idx] as i64 + (total_h as i64 - used);
    if adjusted < 1 {
        let n = heights.len();
        for slot in heights.iter_mut() {
            *slot = total_h / n;
        }
        let used: usize = heights.iter().sum();
        heights[n - 1] += total_h.saturating_sub(used);
    } else {
        heights[idx] = adjusted as usize;
    }
    heights
}

fn hconcat(left: &[String], right: &[String]) -> Vec<String> {
    let n = left.len().max(right.len());
    (0..n).map(|i| format!("{}{}", left.get(i).map(String::as_str).unwrap_or(""), right.get(i).map(String::as_str).unwrap_or(""))).collect()
}

fn scroll_start(sel: usize, len: usize, window_h: usize) -> usize {
    if len <= window_h {
        return 0;
    }
    let mut start = sel as i64 - (window_h / 2) as i64;
    if start < 0 {
        start = 0;
    }
    let start = start as usize;
    if start + window_h > len {
        return len - window_h;
    }
    start
}

// ── Status / ink color mapping ───────────────────────────────────────────────

fn status_dot_color(status: &str) -> fn(&str) -> String {
    match status {
        "completed" => c::blue,
        "idle" => c::gray,
        _ => c::yellow,
    }
}

// ── Session rows ─────────────────────────────────────────────────────────────

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

fn goal_text(r: &SessionRecord) -> String {
    let goal = r.last_goal.as_deref().unwrap_or("").split_whitespace().collect::<Vec<_>>().join(" ");
    if goal.is_empty() { "(no goal)".to_string() } else { goal }
}

fn running_row(r: &SessionRecord, prefix: &str, now: i64, started_at_ms: Option<i64>, inner_w: usize, selected: bool) -> RowCell {
    let elapsed_sec = started_at_ms.map(|started| ((now - started) / 1000).max(0));
    let right = format!(" {}", fmt_duration(elapsed_sec));
    // The tree connector sits left of the status dot; the right-aligned time
    // column is unchanged, so a child's elapsed time still lines up.
    let main = format!("{prefix}{DOT} {} {}", short_id(&r.id), goal_text(r));
    let left_w = inner_w.saturating_sub(string_width(&right));
    selectable(format!("{}{}", fit(&main, left_w, true), right), c::green, selected)
}

fn recent_row(r: &SessionRecord, prefix: &str, now: i64, inner_w: usize, selected: bool) -> RowCell {
    let right = format!(" {}", rel_time(Some(&r.updated_at), now));
    let main = format!("{prefix}{DOT} {} {}", short_id(&r.id), goal_text(r));
    let left_w = inner_w.saturating_sub(string_width(&right));
    selectable(format!("{}{}", fit(&main, left_w, true), right), status_dot_color(&r.status), selected)
}

fn empty_sessions_msg(mode: SessionsMode) -> &'static str {
    match mode {
        SessionsMode::Running => "  no running sessions",
        SessionsMode::Recent => "  no recent sessions",
        SessionsMode::All => "  no sessions yet",
    }
}

/// Rows for the [1] Sessions pane under `vm.mode`. A record draws a live
/// elapsed timer while `started_at_ms` knows it and its relative age
/// otherwise, so All mode mixes both.
fn session_rows(vm: &WatchViewModel, inner_w: usize, inner_h: usize) -> Vec<RowCell> {
    if vm.sessions.is_empty() {
        return vec![plain(empty_sessions_msg(vm.mode), c::dim)];
    }
    let focused = vm.focus == 1;
    let start = scroll_start(vm.sel_session, vm.sessions.len(), inner_h);
    (start..vm.sessions.len().min(start + inner_h))
        .map(|i| {
            let r = &vm.sessions[i];
            let prefix = vm.session_prefixes.get(i).map(String::as_str).unwrap_or("");
            let selected = focused && i == vm.sel_session;
            match vm.started_at_ms.get(&r.id).copied() {
                Some(started) => running_row(r, prefix, vm.now, Some(started), inner_w, selected),
                None => recent_row(r, prefix, vm.now, inner_w, selected),
            }
        })
        .collect()
}

// ── Task rows (the [2] Tasks pane) ───────────────────────────────────────────

/// in_progress tasks float to the top; everything else keeps ledger order.
/// Mirrors the active/rest split in web/src/components/detail-panel.tsx.
pub fn ordered_tasks(tasks: &[HarnessTask]) -> Vec<&HarnessTask> {
    let mut out: Vec<&HarnessTask> = tasks.iter().filter(|t| t.status == HarnessTaskStatus::InProgress).collect();
    out.extend(tasks.iter().filter(|t| t.status != HarnessTaskStatus::InProgress));
    out
}

/// The status glyph and its ink. The browser UI's TaskRow draws a bordered
/// circle filled for terminal states; a one-cell glyph per status is the
/// terminal equivalent, not a copy of that scheme.
fn task_glyph(status: HarnessTaskStatus) -> (&'static str, fn(&str) -> String) {
    match status {
        HarnessTaskStatus::InProgress => ("◐", c::accent as fn(&str) -> String),
        HarnessTaskStatus::Pending => ("○", c::dim as fn(&str) -> String),
        HarnessTaskStatus::Completed => ("●", c::green as fn(&str) -> String),
        HarnessTaskStatus::Blocked => ("✗", c::red as fn(&str) -> String),
        // Dropped keeps pending's hollow glyph, dimmed like its struck title.
        HarnessTaskStatus::Dropped => ("○", c::dim as fn(&str) -> String),
    }
}

fn task_rows(vm: &WatchViewModel, inner_w: usize, inner_h: usize) -> Vec<RowCell> {
    let tasks = ordered_tasks(&vm.tasks);
    if tasks.is_empty() {
        return vec![plain("  no task ledger yet", c::dim)];
    }
    let focused = vm.focus == 2;
    let start = scroll_start(vm.sel_task, tasks.len(), inner_h);
    (start..tasks.len().min(start + inner_h))
        .map(|i| {
            let (glyph, color) = task_glyph(tasks[i].status);
            // An untitled task still needs a visible handle: fall back to its id.
            let label = if tasks[i].title.trim().is_empty() {
                tasks[i].id.as_str()
            } else {
                tasks[i].title.as_str()
            };
            let text = fit(&format!("{glyph} {label}"), inner_w, true);
            selectable(text, color, focused && i == vm.sel_task)
        })
        .collect()
}

/// `3 pending · 1 completed` — the statuses present, in a fixed order, zero
/// counts omitted. None when the ledger is empty.
fn task_count_note(tasks: &[HarnessTask]) -> Option<String> {
    if tasks.is_empty() {
        return None;
    }
    let counts: [(HarnessTaskStatus, &str); 5] = [
        (HarnessTaskStatus::InProgress, "in progress"),
        (HarnessTaskStatus::Pending, "pending"),
        (HarnessTaskStatus::Completed, "completed"),
        (HarnessTaskStatus::Blocked, "blocked"),
        (HarnessTaskStatus::Dropped, "dropped"),
    ];
    let parts: Vec<String> = counts
        .iter()
        .filter_map(|&(status, label)| {
            let n = tasks.iter().filter(|t| t.status == status).count();
            (n > 0).then(|| format!("{n} {label}"))
        })
        .collect();
    Some(parts.join(" · "))
}

// ── Shell rows ───────────────────────────────────────────────────────────────

/// True when the currently selected session is a running (live-lease) one.
fn focused_is_running(vm: &WatchViewModel) -> bool {
    vm.sessions
        .get(vm.sel_session)
        .is_some_and(|r| vm.started_at_ms.contains_key(&r.id))
}

fn shell_rows(vm: &WatchViewModel, inner_w: usize, inner_h: usize) -> Vec<RowCell> {
    if vm.shells.is_empty() {
        let msg = if focused_is_running(vm) {
            "  (no shell processes)"
        } else {
            "  (no running session focused)"
        };
        return vec![plain(msg, c::dim)];
    }
    let focused = vm.focus == 3;
    let start = scroll_start(vm.sel_shell, vm.shells.len(), inner_h);
    (start..vm.shells.len().min(start + inner_h))
        .map(|i| {
            let p = &vm.shells[i];
            let right = format!(" {}", fmt_duration(p.etime_sec));
            let main = format!("{DOT} {:>6} {}", p.pid, p.command);
            let left_w = inner_w.saturating_sub(string_width(&right));
            // Every process in the table is live, so the dot is always green.
            selectable(
                format!("{}{}", fit(&main, left_w, true), right),
                c::green,
                focused && i == vm.sel_shell,
            )
        })
        .collect()
}

// The [0] column when a shell is focused: details of the selected process — a
// shell has no transcript, so the full (untruncated) command line plus its
// child processes are the useful "log-equivalent" header.
fn shell_detail_rows(vm: &WatchViewModel, inner_w: usize) -> Vec<RowCell> {
    let Some(p) = vm.shells.get(vm.sel_shell) else {
        return vec![plain("  (no process selected)", c::dim)];
    };
    let mut rows = vec![
        plain(format!("pid      {}", p.pid), c::white),
        plain(format!("ppid     {}", p.ppid), c::gray),
        plain(format!("running  {}", fmt_duration(p.etime_sec)), c::green),
        plain("", c::dim),
        plain("command", c::accent),
    ];
    for line in wrap_plain(&p.command, inner_w.saturating_sub(2).max(1)) {
        rows.push(plain(format!("  {line}"), c::white));
    }
    let children: Vec<&PsProc> = vm.shells.iter().filter(|x| x.ppid == p.pid && x.pid != p.pid).collect();
    if !children.is_empty() {
        rows.push(plain("", c::dim));
        rows.push(plain(format!("children ({})", children.len()), c::accent));
        for ch in children {
            rows.push(plain(format!("  {:>6} {}", ch.pid, ch.command), c::gray));
        }
    }
    rows
}

// Raw stdout/stderr tail for the selected shell, scrolled exactly like the
// transcript. Distinct empty states tell "no tailable fds" (only ttys/pipes)
// apart from "tailing, nothing yet".
fn shell_log_rows(vm: &WatchViewModel, inner_h: usize) -> Vec<RowCell> {
    if vm.shell_log_lines.is_empty() {
        let msg = if vm.shell_log_files.is_empty() { "  (no tailable stdout/stderr)" } else { "  (waiting for output…)" };
        return vec![plain(msg, c::dim)];
    }
    let total = vm.shell_log_lines.len();
    let scroll = vm.transcript_scroll.min(total - 1);
    let end = total - scroll;
    let start = end.saturating_sub(inner_h);
    vm.shell_log_lines[start..end].iter().map(|line| plain(line.clone(), c::white)).collect()
}

// ── Transcript flattening ────────────────────────────────────────────────────

/// Wrap plain text to a visible-width limit (code-point aware). Embedded
/// newlines hard-break — they must never reach fit(), which would swap them
/// for spaces and push the line past the pane width.
pub fn wrap_plain(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let mut out: Vec<String> = Vec::new();
    for para in text.split('\n') {
        let mut cur = String::new();
        let mut w = 0usize;
        for ch in para.chars() {
            let cw = char_width(ch as u32);
            if w + cw > width && !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
                cur.push(ch);
                w = cw;
            } else {
                cur.push(ch);
                w += cw;
            }
        }
        out.push(cur);
    }
    if out.is_empty() {
        vec![String::new()]
    } else {
        out
    }
}

// The inference route the focused session's newest run was launched on,
// pinned as the first line of the [0] pane so the model and effort in play are
// visible without scrolling to the run start (which the REPLAY_LIMIT window
// may have already dropped). Returns no rows for transcripts written before
// the route was recorded.
pub fn model_header_rows(transcript: &[TranscriptEntry], inner_w: usize) -> Vec<RowCell> {
    for entry in transcript.iter().rev() {
        let TranscriptEntry::Model(model) = entry else { continue };
        let mut rows = Vec::new();
        for route in format_model_route_lines(model) {
            for line in wrap_plain(&route, inner_w.max(1)) {
                rows.push(plain(line, c::cyan));
            }
        }
        return rows;
    }
    Vec::new()
}

fn transcript_rows(vm: &WatchViewModel, inner_w: usize, inner_h: usize) -> Vec<RowCell> {
    let flat = flatten_transcript(&vm.transcript, inner_w);
    if flat.is_empty() {
        return vec![plain("  no transcript entries", c::dim)];
    }
    let total = flat.len();
    let scroll = vm.transcript_scroll.min(total.saturating_sub(1));
    let end = total - scroll;
    let start = end.saturating_sub(inner_h);
    flat[start..end].to_vec()
}

// ── Loop surface rows (the [4] Skills, Tools & Plans pane) ─────────────────────────

/// One loop's capability surface, exactly as the harness recorded it on its
/// loop-start telemetry: the skills composed into its prompt (the
/// classifier-selected ones plus the run's base-prompt `--skill` activations,
/// deduped in composition order) and every tool it could actually call — the
/// packed tools its role allowlist and the MCP gate leave reachable, plus the
/// harness tools (`plan_tasks`, `finish_task`, `ask_user`, …).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillLoad {
    /// The loop iteration the skills were composed for.
    pub iteration: i64,
    pub skills: Vec<String>,
    pub tools: Vec<String>,
    pub plans: Vec<String>,
}

/// Every loop-start event in `transcript` that recorded a capability surface
/// (skills, tools, plans, or any combination), in transcript Events from
/// before the fields existed — or loops that ran with none — carry none
/// and are skipped, so an old session simply shows the pane's empty state.
pub fn skill_loads(transcript: &[TranscriptEntry]) -> Vec<SkillLoad> {
    transcript
        .iter()
        .filter_map(|entry| {
            let TranscriptEntry::Event(event) = entry else {
                return None;
            };
            if event.kind != HarnessEventType::LoopStart {
                return None;
            }
            let data = event.data.as_ref()?;
            let skills = data.skills.clone().unwrap_or_default();
            let tools = data.tools.clone().unwrap_or_default();
            let plans = data.plans.clone().unwrap_or_default();
            if skills.is_empty() && tools.is_empty() && plans.is_empty() {
                return None;
            }
            Some(SkillLoad {
                iteration: event.iteration,
                skills,
                tools,
                plans,
            })
        })
        .collect()
}

/// The union of every entry `pick` selects from each load, with the number of
/// loops that carried it, ordered by count (descending) then name so the
/// roll-up is stable frame to frame.
fn roll_up(loads: &[SkillLoad], pick: impl Fn(&SkillLoad) -> &[String]) -> Vec<(String, usize)> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for load in loads {
        for name in pick(load) {
            *counts.entry(name.clone()).or_insert(0) += 1;
        }
    }
    let mut out: Vec<(String, usize)> = counts.into_iter().collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out
}

/// The union of every loaded skill across `loads`, with the number of loops
/// that loaded it. This is the pane's "all loaded skills" section.
pub fn all_loaded_skills(loads: &[SkillLoad]) -> Vec<(String, usize)> {
    roll_up(loads, |load| &load.skills)
}

/// The union of every tool the loops could call across `loads`, with the number
/// of loops that had it. This is the pane's "all available tools" section.
pub fn all_loaded_tools(loads: &[SkillLoad]) -> Vec<(String, usize)> {
    roll_up(loads, |load| &load.tools)
}

/// The union of every plan the loops were shaped by across `loads`, with the
/// number of loops that chose it. This is the pane's "all chosen plans" section.
pub fn all_loaded_plans(loads: &[SkillLoad]) -> Vec<(String, usize)> {
    roll_up(loads, |load| &load.plans)
}

/// The [4] pane's content rows plus, for every skill/tool it lists, the rows
/// (content-relative) its label occupies — the surface a click and the arrow
/// keys resolve against. Built in one pass, so the pane and the hit test can
/// never disagree about which line an item sits on.
struct SkillSurface {
    rows: Vec<RowCell>,
    items: Vec<SkillItemRow>,
}

fn skill_surface(vm: &WatchViewModel, inner_w: usize) -> SkillSurface {
    if vm.skill_loads.is_empty() {
        return SkillSurface {
            rows: vec![plain("  no loop telemetry yet", c::dim)],
            items: Vec::new(),
        };
    }
    let skills = all_loaded_skills(&vm.skill_loads);
    let tools = all_loaded_tools(&vm.skill_loads);
    let mut rows: Vec<RowCell> = vec![plain(
        format!("all loaded skills ({})", skills.len()),
        c::accent,
    )];
    let mut items: Vec<SkillItemRow> = Vec::new();
    let labels = comma_list(&skills);
    let kinds: Vec<SkillItem> = skills
        .iter()
        .map(|(name, _)| SkillItem::Skill(name.clone()))
        .collect();
    push_surface(
        &mut rows,
        &mut items,
        "  ",
        "  ",
        &labels,
        &kinds,
        c::white,
        inner_w,
    );
    let plans = all_loaded_plans(&vm.skill_loads);
    if !plans.is_empty() {
        rows.push(plain(
            format!("all chosen plans ({})", plans.len()),
            c::accent,
        ));
        let labels = comma_list(&plans);
        push_plain_surface(&mut rows, "  ", "  ", &labels.join(", "), c::white, inner_w);
    }
    rows.push(plain(
        format!("all available tools ({})", tools.len()),
        c::accent,
    ));
    let labels = comma_list(&tools);
    let kinds: Vec<SkillItem> = tools
        .iter()
        .map(|(name, _)| SkillItem::Tool(name.clone()))
        .collect();
    push_surface(
        &mut rows,
        &mut items,
        "  ",
        "  ",
        &labels,
        &kinds,
        c::white,
        inner_w,
    );
    // Newest loop first: its block is the surface a new loop would run with.
    let mut loads: Vec<&SkillLoad> = vm.skill_loads.iter().collect();
    loads.sort_by_key(|l| std::cmp::Reverse(l.iteration));
    for (i, load) in loads.iter().enumerate() {
        if i > 0 {
            rows.push(plain("", c::dim));
        }
        rows.push(plain(
            format!(
                "loop {} · {} skill{} · {} tool{}",
                load.iteration,
                load.skills.len(),
                plural(load.skills.len()),
                load.tools.len(),
                plural(load.tools.len())
            ),
            c::accent,
        ));
        let kinds: Vec<SkillItem> = load
            .skills
            .iter()
            .map(|n| SkillItem::Skill(n.clone()))
            .collect();
        push_surface(
            &mut rows,
            &mut items,
            "  skills  ",
            "          ",
            &load.skills,
            &kinds,
            c::gray,
            inner_w,
        );
        let kinds: Vec<SkillItem> = load
            .tools
            .iter()
            .map(|n| SkillItem::Tool(n.clone()))
            .collect();
        push_surface(
            &mut rows,
            &mut items,
            "  tools   ",
            "          ",
            &load.tools,
            &kinds,
            c::gray,
            inner_w,
        );
        if !load.plans.is_empty() {
            push_plain_surface(
                &mut rows,
                "  plans   ",
                "          ",
                &load.plans.join(", "),
                c::gray,
                inner_w,
            );
        }
    }
    // The picked item paints in reverse video (render_pane's on_cyan) on its
    // own columns: the row it shares with other names keeps its colour, so the
    // highlight marks the picked skill/tool rather than the whole line.
    if let Some(entry) = vm.sel_skill.and_then(|i| items.get(i)) {
        let last_row = entry.last.min(rows.len().saturating_sub(1));
        for r in entry.first..=last_row {
            if let Some(row) = rows.get_mut(r) {
                // The label begins on `first_col` of its first row and ends on
                // `last_col` of its last; the rows between exist only because
                // the name wrapped, so they belong to it entirely.
                let start = if r == entry.first { entry.first_col } else { 0 };
                let end = if r == entry.last {
                    entry.last_col
                } else {
                    inner_w.saturating_sub(1)
                };
                row.selected = true;
                row.sel_span = Some((start, end));
            }
        }
    }
    SkillSurface { rows, items }
}

/// The [4] pane's content rows, unpaged (the pane's window is `skill_rows`).
fn skill_lines(vm: &WatchViewModel, inner_w: usize) -> Vec<RowCell> {
    skill_surface(vm, inner_w).rows
}

/// Every skill/tool the [4] pane lists at `inner_w`, in pane order, each with
/// the rows its label spans (content-relative).
pub fn skill_items(vm: &WatchViewModel, inner_w: usize) -> Vec<SkillItemRow> {
    skill_surface(vm, inner_w).items
}

/// The roll-up's names as display strings — `navis (2 loops)`, `tdd` — keeping
/// the loop count only where it says something (a name more than one loop
/// reached for).
fn comma_list(entries: &[(String, usize)]) -> Vec<String> {
    entries
        .iter()
        .map(|(name, count)| {
            if *count > 1 {
                format!("{name} ({count} loops)")
            } else {
                name.clone()
            }
        })
        .collect()
}

/// The selectable surface: a wrapped comma-separated list of rows, plus one
/// `SkillItemRow` per entry recording the content rows its label spans —
/// the map a click and the arrow keys navigate. `labels` and `kinds` are
/// parallel: one display label per item, in pane order.
#[allow(clippy::too_many_arguments)]
fn push_surface(
    rows: &mut Vec<RowCell>,
    items: &mut Vec<SkillItemRow>,
    first: &str,
    cont: &str,
    labels: &[String],
    kinds: &[SkillItem],
    color: fn(&str) -> String,
    inner_w: usize,
) {
    if kinds.is_empty() {
        return;
    }
    let lead_w = string_width(first).max(string_width(cont));
    let wrap_w = inner_w.saturating_sub(lead_w).max(1);
    let section_start = rows.len();
    let mut cur = String::new();
    for (i, kind) in kinds.iter().enumerate() {
        let label = labels.get(i).map(String::as_str).unwrap_or(kind.name());
        if !cur.is_empty() {
            if string_width(&cur) + 2 + string_width(label) <= wrap_w {
                cur.push_str(", ");
            } else {
                push_surface_row(rows, first, cont, section_start, &cur, color);
                cur.clear();
            }
        }
        // `cur` now begins this item's label, so the rows and columns the
        // label lands on are the item's span: a click anywhere on a broken name
        // picks it, and two names sharing a row stay apart by column.
        let start_col = lead_w + string_width(&cur);
        let mut pushed_end = start_col;
        let first_row = rows.len();
        let mut rest = label;
        while string_width(rest) > wrap_w.saturating_sub(string_width(&cur)) {
            let (chunk, rem) = split_at_width(rest, wrap_w.saturating_sub(string_width(&cur)));
            cur.push_str(chunk);
            pushed_end = lead_w + string_width(&cur).saturating_sub(1);
            push_surface_row(rows, first, cont, section_start, &cur, color);
            cur.clear();
            rest = rem;
        }
        cur.push_str(rest);
        // An empty `cur` here means the last chunk consumed the label exactly
        // to the row's end, so that pushed row is the one that ends the span;
        // otherwise the label ends on the row `rows.len()` names.
        let (last_row, last_col) = if cur.is_empty() {
            (rows.len().saturating_sub(1).max(first_row), pushed_end)
        } else {
            (rows.len(), lead_w + string_width(&cur).saturating_sub(1))
        };
        items.push(SkillItemRow {
            item: kind.clone(),
            first: first_row,
            first_col: start_col,
            last: last_row,
            last_col,
        });
    }
    if !cur.is_empty() {
        push_surface_row(rows, first, cont, section_start, &cur, color);
    }
}

/// One rendered row of a wrapped surface line: `first` leads the section's
/// first row, `cont` every row after it.
fn push_surface_row(
    rows: &mut Vec<RowCell>,
    first: &str,
    cont: &str,
    section_start: usize,
    text: &str,
    color: fn(&str) -> String,
) {
    let prefix = if rows.len() == section_start {
        first
    } else {
        cont
    };
    rows.push(plain(format!("{prefix}{text}"), color));
}

/// A wrapped comma-separated line of plain, non-selectable names: the plans a
/// loop was shaped by. Like `push_surface`, but the names carry no hit-test
/// rows — only the skills and tools the pane lists are clickable.
fn push_plain_surface(
    rows: &mut Vec<RowCell>,
    first: &str,
    cont: &str,
    text: &str,
    color: fn(&str) -> String,
    inner_w: usize,
) {
    if text.is_empty() {
        return;
    }
    let lead_w = string_width(first).max(string_width(cont));
    let wrap_w = inner_w.saturating_sub(lead_w).max(1);
    let section_start = rows.len();
    let mut rest = text;
    loop {
        let (chunk, rem) = split_plain_at_width(rest, wrap_w);
        push_surface_row(rows, first, cont, section_start, chunk.trim_end(), color);
        rest = rem.trim_start();
        if rest.is_empty() {
            break;
        }
    }
}

/// `split_at_width`, backed off to the last `, ` boundary inside the chunk so a
/// wrapped plan list breaks between names rather than mid-name.
fn split_plain_at_width(text: &str, room: usize) -> (&str, &str) {
    if string_width(text) <= room {
        return (text, "");
    }
    let (chunk, rem) = split_at_width(text, room);
    if rem.is_empty() {
        return (chunk, rem);
    }
    match chunk.rfind(", ") {
        Some(pos) => (&text[..pos + 2], &text[pos + 2..]),
        None => (chunk, rem),
    }
}

/// Split `text` at the last char boundary that fits `room` visible columns —
/// always at least one char, so a degenerate row cannot loop forever.
fn split_at_width(text: &str, room: usize) -> (&str, &str) {
    let mut cut = 0usize;
    let mut w = 0usize;
    for (idx, ch) in text.char_indices() {
        let cw = char_width(ch as u32);
        if w + cw > room {
            break;
        }
        w += cw;
        cut = idx + ch.len_utf8();
    }
    text.split_at(cut.max(1).min(text.len()))
}

/// The [4] pane's visible rows in an `inner_w` × `inner_h` box: the wrapped
/// content while it fits, otherwise the current page of it plus a pager row.
/// `vm.skill_page` is clamped here, so an index left over from a taller
/// terminal or a longer session still paints.
fn skill_rows(vm: &WatchViewModel, inner_w: usize, inner_h: usize) -> Vec<RowCell> {
    let all = skill_lines(vm, inner_w);
    let (size, pages) = page_window(all.len(), inner_h);
    if pages <= 1 {
        return all;
    }
    let page = vm.skill_page.min(pages - 1);
    let start = page * size;
    let end = (start + size).min(all.len());
    let mut out: Vec<RowCell> = all[start..end].to_vec();
    let pager = plain(
        format!("  page {}/{} · [/] pages · j/k/↑↓ picks", page + 1, pages),
        c::dim,
    );
    if out.len() < inner_h {
        out.push(pager);
    } else {
        // A one-row box seats no pager row: the page still turns, the hint just
        // has nowhere to go, and clipping it would hide a content row instead.
        out.truncate(inner_h.max(1));
    }
    out
}

/// The rows in one page and how many pages `len` rows take in an `inner_h`-row
/// box: every row in one page while it fits, otherwise the content rows plus a
/// one-row pager (so a page is `inner_h - 1` rows).
fn page_window(len: usize, inner_h: usize) -> (usize, usize) {
    if inner_h == 0 || len <= inner_h {
        return (len.max(1), 1);
    }
    // A pager row is only worth budgeting when the box seats it next to at
    // least one content row, so a degenerate one-row pane still pages.
    let size = if inner_h >= 2 { inner_h - 1 } else { inner_h };
    (size, len.div_ceil(size.max(1)))
}

/// "s" for anything but one, so "1 skill" and "2 skills" both read right.
fn plural(count: usize) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

/// How many rows the [4] pane's wrapped content wants at `inner_w` — the layout
/// hugs the pane to it. Zero when the pane shows its empty-state message, which
/// is not a clickable row.
fn skill_row_count(vm: &WatchViewModel, inner_w: usize) -> usize {
    if vm.skill_loads.is_empty() {
        return 0;
    }
    skill_lines(vm, inner_w).len()
}

/// How many pages the [4] pane's wrapped content needs in a `cols` x `rows`
/// terminal - `1` while it fits, so the app can clamp its page index with this
/// after a resize or a shorter session.
pub fn skill_page_count(vm: &WatchViewModel, cols: usize, rows: usize) -> usize {
    let Some(layout) = pane_layout(vm, cols, rows) else {
        return 1;
    };
    let inner_w = layout.list_w.saturating_sub(2);
    let inner_h = layout.heights[3].saturating_sub(2);
    page_window(skill_lines(vm, inner_w).len(), inner_h).1
}

// ── Titles / footer notes ────────────────────────────────────────────────────

/// The [4] pane's content row under a 1-based cell, as the item it lists —
/// `None` for a border, the empty state, or the pager row. `vm.skill_page` is
/// honoured, so a click lands on the item the user actually sees.
pub fn skill_item_at(
    vm: &WatchViewModel,
    cols: usize,
    rows: usize,
    col: usize,
    row: usize,
) -> Option<SkillItem> {
    let (r0, r1, c0, c1) = panel_regions(vm, cols, rows).into_iter().nth(3)?;
    if row <= r0 || row >= r1 || col < c0 || col > c1 {
        return None;
    }
    let inner_w = c1.saturating_sub(c0 + 1).max(1);
    let inner_h = r1.saturating_sub(r0 + 1).max(1);
    let items = skill_items(vm, inner_w);
    if items.is_empty() {
        return None;
    }
    let content = skill_lines(vm, inner_w).len();
    let (size, pages) = page_window(content, inner_h);
    let page = vm.skill_page.min(pages.saturating_sub(1));
    let within = row - (r0 + 1);
    // A pager row is a control, not an item.
    if pages > 1 && within >= size {
        return None;
    }
    let line = page * size + within;
    let col_rel = col.saturating_sub(c0 + 1);
    // The innermost (latest-starting) item whose box holds the cell, so a click
    // lands on the name the pointer was actually over.
    items
        .into_iter()
        .filter(|entry| {
            let at_or_after =
                line > entry.first || (line == entry.first && col_rel >= entry.first_col);
            let at_or_before =
                line < entry.last || (line == entry.last && col_rel <= entry.last_col);
            at_or_after && at_or_before
        })
        .max_by_key(|entry| entry.first)
        .map(|entry| entry.item)
}

/// The [4] pane's read-up for the [0] column while a skill/tool is picked:
/// `height` lines of a titled box around `vm.skill_detail`'s rows. `None`
/// while nothing is picked (the column keeps the transcript).
pub fn skill_detail_box(vm: &WatchViewModel, width: usize, height: usize) -> Option<Vec<String>> {
    // Only while the pick is being browsed: leaving [4] puts the transcript
    // back in the column, with the pick still standing for the way back.
    if vm.focus != 4 {
        return None;
    }
    let detail = vm.skill_detail.as_ref()?;
    let mut all = detail.lines.clone();
    if all.is_empty() {
        all.push(plain("  (no content)", c::dim));
    }
    // The body scrolls: a SKILL.md or a tool schema is longer than the column,
    // and the whole of it has to stay reachable. `vm.skill_detail_scroll` is
    // clamped here, so a value left over from a taller terminal still paints.
    let inner_h = height.saturating_sub(2).max(1);
    let total = all.len();
    let max_scroll = total.saturating_sub(inner_h);
    let scroll = vm.skill_detail_scroll.min(max_scroll);
    let rows: Vec<RowCell> = all.into_iter().skip(scroll).collect();
    let note = if max_scroll == 0 {
        "↑/↓ or click picks a skill/tool · [4]".to_string()
    } else {
        format!(
            "{}-{} of {} · PgUp/PgDn or wheel scrolls",
            scroll + 1,
            (scroll + inner_h).min(total),
            total
        )
    };
    Some(render_pane(
        width,
        height,
        &format!("[0] {}", detail.title),
        true,
        &rows,
        Some(note.as_str()),
    ))
}

/// How far the [0] read-up can scroll in a `cols` x `rows` terminal: the rows
/// its body overflows the box by (`0` while it all fits), so the app can clamp
/// its wheel and PageDown steps to a listing that ends.
pub fn skill_detail_max_scroll(vm: &WatchViewModel, cols: usize, rows: usize) -> usize {
    let Some(layout) = pane_layout(vm, cols, rows) else {
        return 0;
    };
    let inner_h = layout.zero_h.saturating_sub(2).max(1);
    vm.skill_detail
        .as_ref()
        .map(|detail| detail.lines.len())
        .unwrap_or(0)
        .saturating_sub(inner_h)
}

// ── Titles / footer notes ────────────────────────────────────────────────────

fn sessions_title(vm: &WatchViewModel) -> String {
    format!("[1] Sessions · {} ({})", vm.mode.label(), vm.sessions.len())
}

fn tasks_title(vm: &WatchViewModel) -> String {
    format!("[2] Tasks ({})", vm.tasks.len())
}

fn shells_title(vm: &WatchViewModel) -> String {
    format!("[3] Shells ({})", vm.shells.len())
}

/// `[4] Skills, Tools & Plans` — the pane's roll-up sections list the counts (top of
/// the box: distinct skills, then the tools the loops had at their disposal).
fn skills_title() -> String {
    "[4] Skills, Tools & Plans".to_string()
}

fn shell_detail_title(vm: &WatchViewModel) -> String {
    format!("[0] Shell · {}", vm.shells.get(vm.sel_shell).map(|p| p.pid.to_string()).unwrap_or_else(|| "—".to_string()))
}

fn shell_log_title(vm: &WatchViewModel) -> String {
    format!("Log · {}", vm.shells.get(vm.sel_shell).map(|p| p.pid.to_string()).unwrap_or_else(|| "—".to_string()))
}

fn transcript_title(vm: &WatchViewModel) -> String {
    let Some(r) = vm.sessions.get(vm.sel_session) else { return "[0] Transcript".to_string() };
    let status = if vm.started_at_ms.contains_key(&r.id) { "running" } else { r.status.as_str() };
    format!("[0] {} · {status} · {}", short_id(&r.id), goal_text(r))
}

fn pos_note(sel: usize, len: usize) -> Option<String> {
    if len == 0 {
        return None;
    }
    Some(format!("{}/{len}", (sel + 1).min(len)))
}

// ── Frame ────────────────────────────────────────────────────────────────────

const FOOTER_HINT: &str =
    "1/2/3/4 focus · tab cycle · r mode · j/k move · [/] h/l page · [4] PgUp/Dn read-up · q quit";

/// Pure full-frame render. Returns a single string of exactly `rows` lines
/// joined by \n, each line exactly `cols` visible columns.
/// The pane geometry `render_frame` paints with, read by every hit test so the
/// rectangles a click is measured against cannot drift from the frame.
struct PaneLayout {
    /// Sessions / Tasks / Shells / Skills & Tools heights, top to bottom.
    heights: [usize; 4],
    /// Width of the list column (portrait: the full frame).
    list_w: usize,
    /// Height of the `[0]` pane: the stacked transcript in portrait, the full
    /// body height in landscape.
    zero_h: usize,
    /// Width of the `[0]` column; `None` in portrait, where `[0]` sits below
    /// the lists at full width.
    zero_w: Option<usize>,
}

/// The layout of a terminal, or `None` when it is too small to paint the frame
/// at all. `render_frame`, `transcript_region` and `panel_regions` all compute
/// their geometry here, so the pane budgeting — and with it the mapping from a
/// painted cell back to a list row — exists once: a change to the layout moves
/// the hit-test rectangles with it.
fn pane_layout(vm: &WatchViewModel, cols: usize, rows: usize) -> Option<PaneLayout> {
    let cols = cols.max(1);
    let rows = rows.max(1);
    if cols < 20 || rows < 8 {
        return None;
    }
    let body_h = rows - 1;

    if cols < PORTRAIT_MAX_COLS {
        // Portrait: [1] / [2] / Shells / [0] stacked full-width; lists hug,
        // transcript absorbs the reclaimed rows.
        let heights = portrait_heights(
            body_h,
            &[
                vm.sessions.len(),
                vm.tasks.len(),
                vm.shells.len(),
                skill_row_count(vm, cols.saturating_sub(2)),
                0,
            ],
        );
        return Some(PaneLayout {
            heights: [heights[0], heights[1], heights[2], heights[3]],
            list_w: cols,
            zero_h: heights[4],
            zero_w: None,
        });
    }

    // Landscape: left [1]/[2]/[3]/[4] (~40% width), right full-height [0].
    // Content-hug sessions, shells and skills; tasks takes the rest of the left
    // column (its ledger is the one list that grows without a natural bound).
    let list_w = (cols * 2 / 5).max(30).min(cols - 20);
    let sessions_desired = vm.sessions.len().max(1) + 2;
    let shells_desired = vm.shells.len().max(1) + 2;
    let skills_desired = skill_row_count(vm, list_w.saturating_sub(2)).max(1) + 2;
    let mut h1 = sessions_desired.min(MIN_PANE.max(body_h.saturating_sub(3 * MIN_PANE)));
    let mut h3 = shells_desired.min(MIN_PANE.max(body_h.saturating_sub(h1 + 2 * MIN_PANE)));
    let mut h4 = skills_desired.min(MIN_PANE.max(body_h.saturating_sub(h1 + h3 + MIN_PANE)));
    let mut h2 = body_h as i64 - h1 as i64 - h3 as i64 - h4 as i64;
    if h2 < MIN_PANE as i64 {
        let capped = split_heights(body_h, &[4, 4, 3, 3]);
        h1 = capped[0];
        h2 = capped[1] as i64;
        h3 = capped[2];
        h4 = capped[3];
    }
    Some(PaneLayout {
        heights: [h1, h2.max(0) as usize, h3, h4],
        list_w,
        zero_h: body_h,
        zero_w: Some(cols - list_w),
    })
}

pub fn render_frame(vm: &WatchViewModel, cols: usize, rows: usize) -> String {
    let cols = cols.max(1);
    let rows = rows.max(1);

    if cols < 20 || rows < 8 {
        let msg = fit(" dripw: terminal too small", cols, true);
        let lines: Vec<String> = (0..rows).map(|i| if i == 0 { msg.clone() } else { fit("", cols, false) }).collect();
        return lines.join("\n");
    }

    let footer = c::dim(&fit(FOOTER_HINT, cols, true));
    let sessions_focused = vm.focus == 1;
    let tasks_focused = vm.focus == 2;

    let mk_sessions = |w: usize, h: usize, height: usize| -> Vec<String> {
        render_pane(
            w,
            height,
            &sessions_title(vm),
            sessions_focused,
            &session_rows(vm, w.saturating_sub(2), h),
            pos_note(vm.sel_session, vm.sessions.len()).as_deref(),
        )
    };

    let mk_tasks = |w: usize, h: usize, height: usize| -> Vec<String> {
        render_pane(w, height, &tasks_title(vm), tasks_focused, &task_rows(vm, w.saturating_sub(2), h), task_count_note(&vm.tasks).as_deref())
    };

    let mk_shells = |w: usize, height: usize| -> Vec<String> {
        let note = if vm.focus == 3 { pos_note(vm.sel_shell, vm.shells.len()) } else { None };
        render_pane(w, height, &shells_title(vm), vm.focus == 3, &shell_rows(vm, w.saturating_sub(2), height.saturating_sub(2)), note.as_deref())
    };

    let mk_skills = |w: usize, height: usize| -> Vec<String> {
        let note = if vm.skill_loads.is_empty() { None } else { Some(format!("{} loops", vm.skill_loads.len())) };
        render_pane(
            w,
            height,
            &skills_title(),
            vm.focus == 4,
            &skill_rows(vm, w.saturating_sub(2), height.saturating_sub(2)),
            note.as_deref(),
        )
    };

    // The [0] column: the transcript normally; a skill/tool read-up while one
    // is picked in [4]; a shell-detail box over a raw stdout/stderr tail box
    // when the Shells pane is focused (sub-zero's renderRight). Always emits
    // exactly `height` lines.
    let mk_zero = |w: usize, height: usize| -> Vec<String> {
        if let Some(box_lines) = skill_detail_box(vm, w, height) {
            return box_lines;
        }
        if vm.focus != 3 {
            let mut rows = model_header_rows(&vm.transcript, w.saturating_sub(2));
            // The header eats into the scrolling viewport, never the pane height.
            let body_h = height.saturating_sub(2 + rows.len()).max(1);
            rows.extend(transcript_rows(vm, w.saturating_sub(2), body_h));
            return render_pane(w, height, &transcript_title(vm), false, &rows, if vm.following { Some("following") } else { None });
        }
        let detail_rows = shell_detail_rows(vm, w.saturating_sub(2));
        // Too short to seat two boxes: show the detail box alone.
        if height < MIN_PANE * 2 {
            return render_pane(w, height, &shell_detail_title(vm), true, &detail_rows, None);
        }
        // Detail box hugs its content but always leaves the log box at least its min.
        let detail_h = MIN_PANE.max((detail_rows.len() + 2).min(height - MIN_PANE));
        let log_h = height - detail_h;
        let scroll_note = format!("-{}", vm.transcript_scroll);
        let mut out = render_pane(w, detail_h, &shell_detail_title(vm), true, &detail_rows, None);
        out.extend(render_pane(
            w,
            log_h,
            &shell_log_title(vm),
            false,
            &shell_log_rows(vm, log_h.saturating_sub(2)),
            Some(if vm.following { "following" } else { scroll_note.as_str() }),
        ));
        out
    };

    // The one place the pane budgeting lives; the hit tests below read it too.
    let layout = pane_layout(vm, cols, rows).expect("terminal size checked above");
    let (h1, h2, h3, h4) = (layout.heights[0], layout.heights[1], layout.heights[2], layout.heights[3]);

    let body: Vec<String> = match layout.zero_w {
        None => {
            let mut body = mk_sessions(layout.list_w, h1.saturating_sub(2), h1);
            body.extend(mk_tasks(layout.list_w, h2.saturating_sub(2), h2));
            body.extend(mk_shells(layout.list_w, h3));
            body.extend(mk_skills(layout.list_w, h4));
            body.extend(mk_zero(layout.list_w, layout.zero_h));
            body
        }
        Some(right_w) => {
            let left_w = layout.list_w;
            let mut left = mk_sessions(left_w, h1.saturating_sub(2), h1);
            left.extend(mk_tasks(left_w, h2.saturating_sub(2), h2));
            left.extend(mk_shells(left_w, h3));
            left.extend(mk_skills(left_w, h4));
            let right = mk_zero(right_w, layout.zero_h);
            hconcat(&left, &right)
        }
    };

    let mut all = body;
    all.push(footer);
    while all.len() < rows {
        all.push(fit("", cols, false));
    }
    all.truncate(rows);
    all.join("\n")
}

// ── Mouse hit-testing ────────────────────────────────────────────────────────

/// The 1-based inclusive rectangle of the `[0]` pane — the transcript, or the
/// shell-log box when the Shells pane is focused — as
/// `(row_start, row_end, col_start, col_end)`.
///
/// Taken from `pane_layout`, the same geometry `render_frame` paints with
/// (portrait stacks the pane full-width at the bottom; landscape puts it in
/// the right column), so a wheel event's hovered cell is tested against the
/// pane the user actually sees. `None` when the terminal is too small to paint
/// the frame at all.
pub fn transcript_region(vm: &WatchViewModel, cols: usize, rows: usize) -> Option<(usize, usize, usize, usize)> {
    let layout = pane_layout(vm, cols, rows)?;
    match layout.zero_w {
        None => {
            // Portrait: below the three list panes, absorbing what is left.
            let top: usize = layout.heights.iter().sum();
            if layout.zero_h == 0 {
                return None;
            }
            Some((top + 1, top + layout.zero_h, 1, cols))
        }
        Some(_) => Some((1, layout.zero_h, layout.list_w + 1, cols)),
    }
}

/// The 1-based inclusive rectangle of each list pane, in panel order — `[0]`
/// Sessions, `[1]` Tasks, `[2]` Shells, `[3]` Skills & Tools — as `(row_start,
/// row_end, col_start, col_end)`.
///
/// Read from `pane_layout`, the same geometry `render_frame` paints with
/// (portrait stacks them full-width; landscape seats them in the left column),
/// so a click's cell maps back to the list row it landed on by construction.
/// Empty when the terminal is too small to paint the frame at all.
pub fn panel_regions(vm: &WatchViewModel, cols: usize, rows: usize) -> Vec<(usize, usize, usize, usize)> {
    let Some(layout) = pane_layout(vm, cols, rows) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut top = 0usize;
    for h in layout.heights {
        out.push((top + 1, top + h, 1, layout.list_w));
        top += h;
    }
    out
}

/// The selectable list row under a 1-based cell: `Some((panel, index))` where
/// panel 0/1/2 is Sessions/Tasks/Shells and `index` is the row's position in
/// that pane's rendered window (Sessions and Shells index straight into
/// `vm.sessions` / `vm.shells`; Tasks index into `ordered_tasks`).
///
/// `None` for a cell on a border, on a pane's empty-state message, past the
/// last row, or when the terminal is too small to paint the frame. Panel 3 (the
/// Skills & Tools pane) has no selection of its own, so it reports the row's position
/// only to tell a click it landed on the pane rather than on a border.
pub fn list_row_at(vm: &WatchViewModel, cols: usize, rows: usize, col: usize, row: usize) -> Option<(usize, usize)> {
    for (panel, &(r0, r1, c0, c1)) in panel_regions(vm, cols, rows).iter().enumerate() {
        if row < r0 || row > r1 || col < c0 || col > c1 {
            continue;
        }
        // Every list pane seats its rows between a top and a bottom border row;
        // the borders themselves and any clipped row are not rows.
        // `r0`/`r1` are the pane's own top and bottom border rows, so the rows
        // it can seat sit strictly between them.
        if row <= r0 || row >= r1 {
            continue;
        }
        let inner_h = r1 - r0 - 1;
        let within = row - (r0 + 1);
        if within >= inner_h {
            continue;
        }
        let (len, sel) = match panel {
            0 => (vm.sessions.len(), vm.sel_session),
            1 => (ordered_tasks(&vm.tasks).len(), vm.sel_task),
            2 => (vm.shells.len(), vm.sel_shell),
            _ => (skill_row_count(vm, c1.saturating_sub(c0 + 1).max(1)), 0),
        };
        if len == 0 {
            continue; // the pane shows its empty-state message, not rows
        }
        let index = scroll_start(sel, len, inner_h) + within;
        if index < len {
            return Some((panel, index));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watch::ansi::set_color_enabled;

    fn empty_vm() -> WatchViewModel {
        WatchViewModel {
            now: 0,
            mode: SessionsMode::Running,
            sessions: vec![],
            session_prefixes: vec![],
            started_at_ms: HashMap::new(),
            focus: 1,
            sel_session: 0,
            transcript: vec![],
            following: true,
            transcript_scroll: 0,
            shells: vec![],
            sel_shell: 0,
            shell_log_lines: vec![],
            shell_log_files: vec![],
            mcp_tools: vec![],
            tasks: vec![],
            sel_task: 0,
            skill_loads: vec![],
            skill_page: 0,
            sel_skill: None,
            skill_detail: None,
            skill_detail_scroll: 0,
        }
    }

    fn task(id: &str, title: &str, status: HarnessTaskStatus) -> HarnessTask {
        HarnessTask {
            activations: None,
            created_at_iteration: 0,
            depends_on: None,
            footprint: None,
            dropped_exhausted: None,
            finished_at_iteration: None,
            id: id.into(),
            notes: vec![],
            reopen_count: None,
            review_of: None,
            reviews: None,
            review_round: None,
            awaiting_review_by: None,
            role: None,
            loops_run: None,
            stall_count: 0,
            status,
            summary: None,
            title: title.into(),
            verify_nudged: None,
            edit_nudged: None,
            confidence: None,
            blocked_on: None,
            recovery_history: None,
        }
    }

    fn task_texts(rows: &[RowCell]) -> Vec<String> {
        rows.iter().map(|r| r.text.trim_end().to_string()).collect()
    }

    fn record(id: &str, goal: &str) -> SessionRecord {
        SessionRecord {
            sessions_dir: None,
            created_at: "2026-01-01T00:00:00.000Z".into(),
            cwd: "/r".into(),
            goal_count: 1,
            id: id.into(),
            last_goal: Some(goal.into()),
            parent_id: None,
            project_slug: "p".into(),
            status: "idle".into(),
            updated_at: "2026-01-01T00:00:00.000Z".into(),
        }
    }

    fn widths(frame: &str) -> Vec<usize> {
        frame.split('\n').map(|line| string_width(&strip_ansi(line))).collect()
    }

    #[test]
    fn durations_and_relative_times_match_the_ts() {
        assert_eq!(fmt_duration(Some(65)), "1:05");
        assert_eq!(fmt_duration(Some(3600)), "1:00");
        assert_eq!(fmt_duration(Some(3725)), "1:02");
        assert_eq!(fmt_duration(Some(-3)), "0:00");
        assert_eq!(fmt_duration(None), "--:--");
        let now = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:01:30Z").unwrap().timestamp_millis();
        assert_eq!(rel_time(Some("2026-01-01T00:00:00Z"), now), "1m");
        assert_eq!(rel_time(Some("2025-12-30T00:00:00Z"), now), "2d");
        assert_eq!(rel_time(None, now), "?");
        assert_eq!(rel_time(Some("garbage"), now), "?");
    }

    #[test]
    fn diff_lines_reports_changed_and_extra_indices() {
        let prev = vec!["a".to_string(), "b".to_string()];
        let next = vec!["a".to_string(), "c".to_string(), "d".to_string()];
        assert_eq!(diff_lines(&prev, &next), vec![1, 2]);
        assert!(diff_lines(&prev, &prev).is_empty());
    }

    #[test]
    fn layout_helpers_respect_budgets() {
        assert_eq!(cap_to_budget(&[10, 10, 10], 12).iter().sum::<usize>(), 12);
        assert_eq!(portrait_heights(40, &[2, 5, 0, 0]).iter().sum::<usize>(), 40);
        assert_eq!(portrait_heights(10, &[20, 20, 20, 0]).iter().sum::<usize>(), 10);
        assert!(portrait_heights(40, &[2, 5, 0, 0]).iter().all(|&h| h >= 1));
    }

    #[test]
    fn transcript_region_matches_the_rendered_pane() {
        let vm = empty_vm();

        // Portrait: full width, last body pane above the footer.
        let (cols, rows) = (80usize, 40usize);
        let frame = render_frame(&vm, cols, rows);
        let plain: Vec<String> = frame.split('\n').map(strip_ansi).collect();
        let (r0, r1, c0, c1) = transcript_region(&vm, cols, rows).expect("portrait region");
        assert_eq!(c0, 1);
        assert_eq!(c1, cols);
        assert_eq!(r1, rows - 1, "transcript ends at the last body row");
        assert_eq!(plain[r0 - 1].chars().nth(c0 - 1), Some('╭'));
        assert_eq!(plain[r1 - 1].chars().nth(c1 - 1), Some('╯'));

        // Landscape: right column, top body row to the last body row.
        let (cols, rows) = (120usize, 40usize);
        let frame = render_frame(&vm, cols, rows);
        let plain: Vec<String> = frame.split('\n').map(strip_ansi).collect();
        let (r0, r1, c0, c1) = transcript_region(&vm, cols, rows).expect("landscape region");
        assert_eq!(r0, 1);
        assert_eq!(r1, rows - 1);
        assert_eq!(plain[r0 - 1].chars().nth(c0 - 1), Some('╭'));
        assert_eq!(plain[r1 - 1].chars().nth(c1 - 1), Some('╯'));
        // The [0] title sits inside the region, at the pane's first content column.
        assert_eq!(plain[r0 - 1].chars().nth(c0), Some('─'));
        let title_at = plain[r0 - 1].find("[0]").expect("transcript title");
        assert!(title_at >= c0 - 1 && title_at < c1);
    }

    #[test]
    fn list_row_at_maps_a_cell_back_to_the_clicked_row() {
        let _guard = crate::watch::ansi::color_test_lock();
        set_color_enabled(false);
        let mut vm = empty_vm();
        for i in 0..4 {
            vm.sessions.push(record(&format!("aaaaaaaa-{i}"), "goal"));
        }
        vm.tasks = vec![
            task("task-1", "first", HarnessTaskStatus::Pending),
            task("task-2", "second", HarnessTaskStatus::Pending),
        ];
        vm.shells.push(PsProc { pid: 12, ppid: 1, etime_sec: Some(61), command: "bash -c sleep".into() });

        for (cols, rows) in [(100usize, 30usize), (60, 24)] {
            let regions = panel_regions(&vm, cols, rows);
            assert_eq!(regions.len(), 4, "{cols}x{rows}");
            let (sr0, sr1, sc0, sc1) = regions[0];
            // Every pane's rows are bracketed by its own top and bottom border.
            assert_eq!(list_row_at(&vm, cols, rows, sc0 + 3, sr0), None, "top border {cols}x{rows}");
            assert_eq!(list_row_at(&vm, cols, rows, sc0 + 3, sr1), None, "bottom border {cols}x{rows}");
            assert_eq!(list_row_at(&vm, cols, rows, sc0 - 1, sr0 + 1), None, "left of pane");
            assert_eq!(list_row_at(&vm, cols, rows, sc1 + 1, sr0 + 1), None, "right of pane");
            // Four panes leave a short terminal few rows each, so every pane
            // asserts only what its own borders actually enclose.
            let seated = |r: (usize, usize, usize, usize)| r.1 - r.0 - 1;
            for i in 0..seated(regions[0]).min(4) {
                assert_eq!(list_row_at(&vm, cols, rows, sc0 + 3, sr0 + 1 + i), Some((0, i)), "session {i} {cols}x{rows}");
            }
            // With a row to spare past the fourth session, that row is blank — it
            // is this pane's own, not a hit borrowed from the pane below.
            if seated(regions[0]) > 4 {
                assert_eq!(list_row_at(&vm, cols, rows, sc0 + 3, sr0 + 5), None, "past the last session");
            }

            let (tr0, tr1, _, _) = regions[1];
            if seated(regions[1]) >= 1 {
                assert_eq!(list_row_at(&vm, cols, rows, sc0 + 3, tr0 + 1), Some((1, 0)), "first task");
            }
            if seated(regions[1]) >= 2 {
                assert_eq!(list_row_at(&vm, cols, rows, sc0 + 3, tr0 + 2), Some((1, 1)), "second task");
            }
            assert_eq!(list_row_at(&vm, cols, rows, sc0 + 3, tr1), None, "task bottom border");

            let (kr0, _, kc0, _) = regions[2];
            if seated(regions[2]) >= 1 {
                assert_eq!(list_row_at(&vm, cols, rows, kc0 + 3, kr0 + 1), Some((2, 0)), "first shell");
            }
            if seated(regions[2]) >= 2 {
                assert_eq!(list_row_at(&vm, cols, rows, kc0 + 3, kr0 + 2), None, "no second shell");
            }

            // The Skills pane's rows report their position; its empty state does not.
            let (sk0, _, skc0, _) = regions[3];
            assert_eq!(list_row_at(&vm, cols, rows, skc0 + 3, sk0), None, "skills top border");
            assert_eq!(list_row_at(&vm, cols, rows, skc0 + 3, sk0 + 1), None, "the empty state is not a row");
            if seated(regions[3]) >= 1 {
                vm.skill_loads = vec![SkillLoad { iteration: 2, skills: vec!["navis".into()], tools: vec![], plans: vec![] }];
                let (sk0, _, skc0, _) = panel_regions(&vm, cols, rows)[3];
                assert_eq!(list_row_at(&vm, cols, rows, skc0 + 3, sk0 + 1), Some((3, 0)), "first skills row");
                vm.skill_loads.clear();
            }
        }

        // Too small to paint: no panes, no hits.
        assert!(panel_regions(&vm, 19, 5).is_empty());
        assert_eq!(list_row_at(&vm, 19, 5, 1, 1), None);
    }

    #[test]
    fn list_row_at_is_scoped_to_the_pane_the_cell_sits_in() {
        let _guard = crate::watch::ansi::color_test_lock();
        set_color_enabled(false);
        let mut vm = empty_vm();
        vm.tasks = vec![task("task-1", "only task", HarnessTaskStatus::Pending)];
        // A task row is only clickable inside the [2] pane, never in [1].
        let regions = panel_regions(&vm, 100, 30);
        let (sr0, _, sc0, _) = regions[0];
        let (tr0, _, _, _) = regions[1];
        assert_eq!(list_row_at(&vm, 100, 30, sc0 + 3, sr0 + 1), None, "empty sessions pane");
        assert_eq!(list_row_at(&vm, 100, 30, sc0 + 3, tr0 + 1), Some((1, 0)));
    }

    #[test]
    fn render_pane_emits_exact_box() {
        let _guard = crate::watch::ansi::color_test_lock();
        set_color_enabled(false);
        let rows = vec![
            plain("hello", c::white),
            RowCell {
                text: "rich".into(),
                color: None,
                selected: false,
                sel_span: None,
                rich: true,
            },
        ];
        let lines = render_pane(20, 5, "T", true, &rows, Some("1/2"));
        assert_eq!(lines.len(), 5);
        assert!(lines.iter().all(|line| string_width(line) == 20), "{lines:?}");
        assert!(lines[0].starts_with("╭─ T "));
        assert_eq!(lines[2], "│rich              │");
        assert!(lines[4].ends_with(" 1/2 ╯"));
    }

    #[test]
    fn wrap_plain_breaks_on_width_and_newlines() {
        assert_eq!(wrap_plain("abcdef\ngh", 4), vec!["abcd", "ef", "gh"]);
        assert_eq!(wrap_plain("", 4), vec![""]);
    }

    #[test]
    fn render_frame_is_exactly_rows_by_cols_in_both_layouts() {
        let _guard = crate::watch::ansi::color_test_lock();
        set_color_enabled(false);
        let mut vm = empty_vm();
        vm.mode = SessionsMode::All;
        vm.sessions.push(record("aaaaaaaa-1", "run a goal that is rather long so it gets clipped by the pane"));
        vm.sessions.push(record("bbbbbbbb-2", "old goal"));
        vm.started_at_ms.insert("aaaaaaaa-1".into(), 0);
        vm.tasks = vec![task("task-1", "port types", HarnessTaskStatus::Pending), task("task-2", "wire it up", HarnessTaskStatus::InProgress)];
        vm.shells.push(PsProc { pid: 12, ppid: 1, etime_sec: Some(61), command: "bash -c sleep".into() });
        for (cols, rows) in [(100, 30), (60, 24), (19, 5), (120, 8)] {
            let frame = render_frame(&vm, cols, rows);
            let w = widths(&frame);
            assert_eq!(w.len(), rows, "{cols}x{rows}");
            assert!(w.iter().all(|&x| x == cols), "{cols}x{rows}: {w:?}");
        }
        vm.focus = 3;
        vm.shell_log_lines = vec!["out 1".into(), "out 2".into()];
        let frame = render_frame(&vm, 100, 30);
        assert!(frame.contains("[0] Shell · 12"));
        assert!(frame.contains("Log · 12"));
        assert!(widths(&frame).iter().all(|&x| x == 100));
    }

    #[test]
    fn sessions_mode_cycles_through_all_three_and_back() {
        assert_eq!(SessionsMode::Running.next(), SessionsMode::Recent);
        assert_eq!(SessionsMode::Recent.next(), SessionsMode::All);
        assert_eq!(SessionsMode::All.next(), SessionsMode::Running);
        assert_eq!(SessionsMode::Running.label(), "running");
        assert_eq!(SessionsMode::Recent.label(), "recent");
        assert_eq!(SessionsMode::All.label(), "all");
    }

    #[test]
    fn sessions_title_names_the_mode_and_count() {
        let mut vm = empty_vm();
        vm.mode = SessionsMode::Recent;
        vm.sessions.push(record("aaaaaaaa-1", "old goal"));
        assert_eq!(sessions_title(&vm), "[1] Sessions · recent (1)");
        vm.mode = SessionsMode::Running;
        assert_eq!(sessions_title(&vm), "[1] Sessions · running (1)");
        vm.mode = SessionsMode::All;
        assert_eq!(sessions_title(&vm), "[1] Sessions · all (1)");
    }

    #[test]
    fn sessions_empty_state_follows_the_mode() {
        let _guard = crate::watch::ansi::color_test_lock();
        set_color_enabled(false);
        let mut vm = empty_vm();
        assert_eq!(session_rows(&vm, 20, 5)[0].text, "  no running sessions");
        vm.mode = SessionsMode::Recent;
        assert_eq!(session_rows(&vm, 20, 5)[0].text, "  no recent sessions");
        vm.mode = SessionsMode::All;
        assert_eq!(session_rows(&vm, 20, 5)[0].text, "  no sessions yet");
    }

    #[test]
    fn all_mode_mixes_live_timers_and_relative_times() {
        let _guard = crate::watch::ansi::color_test_lock();
        set_color_enabled(false);
        let mut vm = empty_vm();
        vm.mode = SessionsMode::All;
        let now = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:01:30Z").unwrap().timestamp_millis();
        vm.now = now;
        vm.sessions.push(record("aaaaaaaa-1", "live"));
        vm.sessions.push(record("bbbbbbbb-2", "old"));
        vm.started_at_ms.insert("aaaaaaaa-1".into(), now - 65_000);
        let rows = session_rows(&vm, 40, 5);
        assert!(rows[0].text.contains("1:05"), "{}", rows[0].text);
        assert!(rows[1].text.contains("1m"), "{}", rows[1].text);
    }

    #[test]
    fn session_rows_draw_the_tree_prefix_left_of_the_status_dot() {
        let _guard = crate::watch::ansi::color_test_lock();
        set_color_enabled(false);
        let mut vm = empty_vm();
        vm.mode = SessionsMode::All;
        vm.sessions.push(record("aaaaaaaa-1", "parent goal"));
        vm.sessions.push(record("bbbbbbbb-2", "child goal"));
        vm.session_prefixes = vec![String::new(), "└─ ".to_string()];

        let rows = session_rows(&vm, 40, 5);
        assert!(
            rows[0].text.starts_with(&format!("{DOT} aaaaaaaa")),
            "a root row has no connector: {}",
            rows[0].text
        );
        assert!(
            rows[1].text.starts_with(&format!("└─ {DOT} bbbbbbbb")),
            "the connector precedes the status dot: {}",
            rows[1].text
        );
    }

    #[test]
    fn task_rows_put_in_progress_first_with_expected_glyphs() {
        let _guard = crate::watch::ansi::color_test_lock();
        set_color_enabled(false);
        let mut vm = empty_vm();
        vm.focus = 2;
        vm.tasks = vec![
            task("task-1", "pending one", HarnessTaskStatus::Pending),
            task("task-2", "active one", HarnessTaskStatus::InProgress),
            task("task-3", "done one", HarnessTaskStatus::Completed),
            task("task-4", "stuck one", HarnessTaskStatus::Blocked),
            task("task-5", "cut one", HarnessTaskStatus::Dropped),
        ];
        let rows = task_rows(&vm, 24, 10);
        assert_eq!(
            task_texts(&rows),
            vec![
                "◐ active one",
                "○ pending one",
                "● done one",
                "✗ stuck one",
                "○ cut one"
            ]
        );
        assert!(
            rows[0].selected,
            "the focused pane highlights the selected task"
        );
    }

    #[test]
    fn task_count_note_summarizes_present_statuses() {
        let _guard = crate::watch::ansi::color_test_lock();
        set_color_enabled(false);
        assert_eq!(task_count_note(&[]), None);
        let tasks = vec![
            task("task-1", "a", HarnessTaskStatus::Pending),
            task("task-2", "b", HarnessTaskStatus::Pending),
            task("task-3", "c", HarnessTaskStatus::Completed),
        ];
        assert_eq!(
            task_count_note(&tasks).as_deref(),
            Some("2 pending · 1 completed")
        );
        let mut vm = empty_vm();
        vm.tasks = tasks;
        assert_eq!(tasks_title(&vm), "[2] Tasks (3)");
    }

    #[test]
    fn task_rows_follow_the_scroll_window() {
        let _guard = crate::watch::ansi::color_test_lock();
        set_color_enabled(false);
        let mut vm = empty_vm();
        let mut tasks: Vec<HarnessTask> = (0..10)
            .map(|i| {
                task(
                    &format!("task-{i}"),
                    &format!("t{i}"),
                    HarnessTaskStatus::Pending,
                )
            })
            .collect();
        tasks.push(task("task-x", "active", HarnessTaskStatus::InProgress));
        vm.tasks = tasks;
        // The window starts at the selected row: first row selected shows the
        // floated in_progress task, last row selected shows the ledger's end.
        vm.sel_task = 0;
        assert_eq!(task_texts(&task_rows(&vm, 24, 2)), vec!["◐ active", "○ t0"]);
        vm.sel_task = 10;
        assert_eq!(
            task_texts(&task_rows(&vm, 24, 3)),
            vec!["○ t7", "○ t8", "○ t9"]
        );
    }

    #[test]
    fn loop_start_telemetry_reads_skills_and_tools_in_order() {
        use crate::cli::transcript::{TranscriptEntry, TranscriptEventEntry};
        use crate::core::types::HarnessEventData;

        fn loop_start(
            iteration: i64,
            skills: Option<Vec<&str>>,
            tools: Option<Vec<&str>>,
        ) -> TranscriptEntry {
            TranscriptEntry::Event(TranscriptEventEntry {
                at: "2026-01-01T00:00:00Z".into(),
                data: Some(HarnessEventData {
                    skills: skills.map(|s| s.into_iter().map(str::to_string).collect()),
                    tools: tools.map(|t| t.into_iter().map(str::to_string).collect()),
                    ..Default::default()
                }),
                detail: "loop 1".into(),
                goal_id: "g1".into(),
                iteration,
                kind: HarnessEventType::LoopStart,
            })
        }

        // A non-loop-start event carrying a surface (never emitted today) is
        // ignored: the pane reads loop-start telemetry only.
        let other = TranscriptEntry::Event(TranscriptEventEntry {
            at: "2026-01-01T00:00:00Z".into(),
            data: Some(HarnessEventData {
                skills: Some(vec!["bogus".into()]),
                tools: Some(vec!["BOGUS".into()]),
                ..Default::default()
            }),
            detail: "tool".into(),
            goal_id: "g1".into(),
            iteration: 1,
            kind: HarnessEventType::ToolCall,
        });

        let transcript = vec![
            other,
            loop_start(1, Some(vec!["navis"]), Some(vec!["BASH", "READ"])),
            loop_start(2, None, None),
            loop_start(3, Some(vec![]), Some(vec![])),
            loop_start(
                4,
                Some(vec!["navis", "verify-before-done"]),
                Some(vec!["PATCH", "READ"]),
            ),
        ];
        let loads = skill_loads(&transcript);
        assert_eq!(
            loads,
            vec![
                SkillLoad {
                    iteration: 1,
                    skills: vec!["navis".into()],
                    tools: vec!["BASH".into(), "READ".into()],
                    plans: vec![],
                },
                SkillLoad {
                    iteration: 4,
                    skills: vec!["navis".into(), "verify-before-done".into()],
                    tools: vec!["PATCH".into(), "READ".into()],
                    plans: vec![],
                },
            ]
        );
        // A loop that recorded tools but no skills is still a surface.
        assert_eq!(
            skill_loads(&[loop_start(5, None, Some(vec!["READ"]))]),
            vec![SkillLoad {
                iteration: 5,
                skills: vec![],
                tools: vec!["READ".into()],
                plans: vec![],
            }]
        );
        // The roll-up counts loops per skill/tool, most-loaded first (ties by name).
        assert_eq!(
            all_loaded_skills(&loads),
            vec![
                ("navis".to_string(), 2),
                ("verify-before-done".to_string(), 1)
            ]
        );
        assert_eq!(
            all_loaded_tools(&loads),
            vec![
                ("READ".to_string(), 2),
                ("BASH".to_string(), 1),
                ("PATCH".to_string(), 1)
            ]
        );
        assert!(skill_loads(&[]).is_empty());
        assert!(all_loaded_skills(&[]).is_empty());
        assert!(all_loaded_tools(&[]).is_empty());
    }

    #[test]
    fn the_skills_and_tools_pane_rolls_up_then_lists_each_loop() {
        let _guard = crate::watch::ansi::color_test_lock();
        set_color_enabled(false);
        let mut vm = empty_vm();
        assert_eq!(skills_title(), "[4] Skills, Tools & Plans");
        assert_eq!(skill_rows(&vm, 40, 10)[0].text, "  no loop telemetry yet");

        vm.skill_loads = vec![
            SkillLoad {
                iteration: 1,
                skills: vec!["navis".into()],
                tools: vec!["READ".into()],
                plans: vec![],
            },
            SkillLoad {
                iteration: 2,
                skills: vec!["navis".into(), "tdd".into()],
                tools: vec!["READ".into(), "PATCH".into()],
                plans: vec![],
            },
        ];
        assert_eq!(skills_title(), "[4] Skills, Tools & Plans");
        let texts: Vec<String> = skill_rows(&vm, 40, 20)
            .iter()
            .map(|r| r.text.clone())
            .collect();
        assert_eq!(
            texts,
            vec![
                "all loaded skills (2)",
                "  navis (2 loops), tdd",
                "all available tools (2)",
                "  READ (2 loops), PATCH",
                "loop 2 · 2 skills · 2 tools",
                "  skills  navis, tdd",
                "  tools   READ, PATCH",
                "",
                "loop 1 · 1 skill · 1 tool",
                "  skills  navis",
                "  tools   READ",
            ]
        );
    }

    #[test]
    fn the_skills_and_tools_lists_wrap_into_a_few_comma_separated_rows() {
        let _guard = crate::watch::ansi::color_test_lock();
        set_color_enabled(false);
        let mut vm = empty_vm();
        vm.skill_loads = vec![SkillLoad {
            iteration: 1,
            skills: vec!["navis".into(), "cs-reference".into(), "tdd".into()],
            tools: (0..20).map(|i| format!("TOOL-{i}")).collect(),
            plans: vec![],
        }];
        // Wide pane: every row fits, nothing is drawn one name per line.
        let rows = skill_rows(&vm, 76, 100);
        assert!(rows.iter().all(|r| string_width(&r.text) <= 76), "{rows:?}");
        let joined = rows
            .iter()
            .map(|r| r.text.clone())
            .collect::<Vec<_>>()
            .join(" ");
        for name in ["navis", "cs-reference", "tdd", "TOOL-0", "TOOL-19"] {
            assert!(joined.contains(name), "{name} missing from {joined:?}");
        }
        // 23 names cost a handful of wrapped rows, not 23.
        let lines = skill_lines(&vm, 76);
        assert!(
            lines.len() < 14,
            "{:?}",
            lines.iter().map(|r| &r.text).collect::<Vec<_>>()
        );
        // Narrow panes wrap hard, but nothing overflows the width and no row
        // breaks a name in half.
        let narrow = skill_lines(&vm, 30);
        assert!(
            narrow.iter().all(|r| string_width(&r.text) <= 30),
            "{:?}",
            narrow.iter().map(|r| &r.text).collect::<Vec<_>>()
        );
        assert!(
            !narrow.iter().any(|r| r.text.trim_end().ends_with("TO")),
            "{:?}",
            narrow.iter().map(|r| &r.text).collect::<Vec<_>>()
        );

        // A loop with no skills prints the roll-up header and no skills row.
        vm.skill_loads = vec![SkillLoad {
            iteration: 3,
            skills: vec![],
            tools: vec!["READ".into()],
            plans: vec![],
        }];
        let texts: Vec<String> = skill_lines(&vm, 76)
            .iter()
            .map(|r| r.text.clone())
            .collect();
        assert!(texts.contains(&"all loaded skills (0)".to_string()));
        assert!(
            !texts.iter().any(|t| t.starts_with("  skills")),
            "{texts:?}"
        );
        assert!(texts.contains(&"  tools   READ".to_string()));
    }

    #[test]
    fn a_surface_too_long_for_the_pane_pages_instead_of_overflowing() {
        let _guard = crate::watch::ansi::color_test_lock();
        set_color_enabled(false);
        let mut vm = empty_vm();
        vm.skill_loads = (1..=30)
            .map(|i| SkillLoad {
                iteration: i,
                skills: vec![format!("skill-{i}")],
                tools: vec![format!("TOOL-{i}")],
                plans: vec![],
            })
            .collect();
        let full = skill_lines(&vm, 40).len();
        assert!(full > 8, "the fixture must not fit the pane: {full}");
        let pages = full.div_ceil(7);

        let rows = skill_rows(&vm, 40, 8);
        assert_eq!(rows.len(), 8);
        assert_eq!(
            rows.last().unwrap().text,
            format!("  page 1/{pages} · [/] pages · j/k/↑↓ picks")
        );

        // Every page stays inside the box: never more rows than it seats, and
        // never wider than it is, whatever page is shown.
        for want in 0..pages {
            vm.skill_page = want;
            let page_rows = skill_rows(&vm, 40, 8);
            // A short last page paints fewer rows; the pane pads the rest.
            assert!(
                !page_rows.is_empty() && page_rows.len() <= 8,
                "page {}",
                want + 1
            );
            assert!(page_rows.iter().all(|r| string_width(&r.text) <= 40));
            assert!(
                page_rows
                    .last()
                    .unwrap()
                    .text
                    .contains(&format!("page {}/{pages}", want + 1)),
                "{page_rows:?}"
            );
        }

        vm.skill_page = 1;
        let second = skill_rows(&vm, 40, 8);
        assert_eq!(
            second.last().unwrap().text,
            format!("  page 2/{pages} · [/] pages · j/k/↑↓ picks")
        );
        assert_ne!(second[0].text, rows[0].text, "the window moved");

        // A stale index (a taller terminal, a shorter list) clamps.
        vm.skill_page = 999;
        let last = skill_rows(&vm, 40, 8);
        assert_eq!(
            last.last().unwrap().text,
            format!("  page {pages}/{pages} · [/] pages · j/k/↑↓ picks")
        );

        // Content that fits is never paged.
        vm.skill_loads.truncate(1);
        let whole = skill_rows(&vm, 40, 8);
        assert!(
            !whole.iter().any(|r| r.text.contains("page ")),
            "{:?}",
            whole.iter().map(|r| &r.text).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_one_row_box_still_turns_its_pages() {
        let _guard = crate::watch::ansi::color_test_lock();
        set_color_enabled(false);
        let mut vm = empty_vm();
        vm.skill_loads = (1..=8)
            .map(|i| SkillLoad {
                iteration: i,
                skills: vec![format!("skill-{i}")],
                tools: vec![format!("TOOL-{i}")],
                plans: vec![],
            })
            .collect();
        let full = skill_lines(&vm, 30).len();
        assert!(full > 1, "the fixture must not fit one row: {full}");
        let expect_pages = full.div_ceil(1);

        // A box too short for a pager row must never paint more rows than it
        // seats (the frame invariant would break) and must still page.
        for want in [0usize, 1, expect_pages - 1] {
            vm.skill_page = want;
            let rows = skill_rows(&vm, 30, 1);
            assert_eq!(rows.len(), 1, "page {}", want + 1);
            assert_eq!(rows[0].text, skill_lines(&vm, 30)[want].text);
        }
    }

    #[test]
    fn the_skills_and_tools_pane_paints_a_titled_box() {
        let _guard = crate::watch::ansi::color_test_lock();
        set_color_enabled(false);
        let mut vm = empty_vm();
        vm.skill_loads = vec![SkillLoad {
            iteration: 7,
            skills: vec!["navis".into()],
            tools: vec!["READ".into()],
            plans: vec![],
        }];
        let frame = render_frame(&vm, 120, 40);
        let plain: Vec<String> = frame.split('\n').map(strip_ansi).collect();
        assert!(
            plain
                .iter()
                .any(|l| l.contains("[4] Skills, Tools & Plans")),
            "the frame titles the pane"
        );
        assert!(plain.iter().any(|l| l.contains("all loaded skills (1)")));
        assert!(plain.iter().any(|l| l.contains("all available tools (1)")));
        assert!(
            plain.iter().any(|l| l.contains("loop 7 · 1 skill · 1 tool")),
            "the loop's own surface is listed"
        );
        assert!(
            widths(&frame).iter().all(|&w| w == 120),
            "every row keeps the frame width"
        );
    }

    #[test]
    fn empty_task_ledger_renders_the_empty_row_and_no_note() {
        let _guard = crate::watch::ansi::color_test_lock();
        set_color_enabled(false);
        let vm = empty_vm();
        assert_eq!(task_count_note(&vm.tasks), None);
        assert_eq!(task_rows(&vm, 24, 5)[0].text, "  no task ledger yet");
    }

    #[test]
    fn skill_items_map_every_listed_name_to_the_rows_it_spans() {
        let _guard = crate::watch::ansi::color_test_lock();
        set_color_enabled(false);
        let mut vm = empty_vm();
        vm.skill_loads = vec![
            SkillLoad {
                iteration: 1,
                skills: vec!["navis".into()],
                tools: vec!["READ".into()],
                plans: vec![],
            },
            SkillLoad {
                iteration: 2,
                skills: vec!["navis".into(), "tdd".into()],
                tools: vec!["READ".into(), "GREP".into()],
                plans: vec![],
            },
        ];

        let inner_w = 30;
        let rows = skill_lines(&vm, inner_w);
        let items = skill_items(&vm, inner_w);

        // Every name is listed once in the roll-up, then again in each loop
        // block that had it: 4 roll-up entries + 4 in the newest loop + 2 in
        // the older one.
        assert_eq!(items.len(), 10, "{items:?}");
        for entry in &items {
            assert!(entry.first <= entry.last, "{entry:?}");
            assert!(entry.last < rows.len(), "{entry:?}");
            assert!(
                rows[entry.first].text.contains(entry.item.name()),
                "{entry:?} does not start on row {} ({:?})",
                entry.first,
                rows[entry.first].text
            );
        }
        // Pane order: the roll-up (skills then tools, most loops first), then
        // the newest loop, then the older one.
        let names: Vec<&str> = items.iter().map(|e| e.item.name()).collect();
        assert_eq!(
            names,
            vec!["navis", "tdd", "READ", "GREP", "navis", "tdd", "READ", "GREP", "navis", "READ"]
        );
        assert!(matches!(items[0].item, SkillItem::Skill(_)));
        assert!(matches!(items[2].item, SkillItem::Tool(_)));
        // The first `skill_items` entry is the row the pane paints it on.
        assert_eq!(items[0].first, 1, "the roll-up's skill row");
    }

    #[test]
    fn skill_item_at_resolves_a_painted_cell_and_the_pick_paints_its_rows() {
        let _guard = crate::watch::ansi::color_test_lock();
        set_color_enabled(false);
        let mut vm = empty_vm();
        vm.skill_loads = vec![SkillLoad {
            iteration: 1,
            skills: vec!["navis".into(), "tdd".into()],
            tools: vec!["READ".into()],
            plans: vec![],
        }];
        vm.focus = 4;
        let (cols, rows) = (100usize, 30usize);
        let (r0, _, c0, c1) = panel_regions(&vm, cols, rows)[3];
        let inner_w = c1 - c0 - 1;
        let items = skill_items(&vm, inner_w);
        assert_eq!(items.len(), 6, "{items:?}");

        // Every item's own recorded start cell resolves back to it, even when
        // two names share a row.
        for entry in &items {
            assert_eq!(
                skill_item_at(
                    &vm,
                    cols,
                    rows,
                    c0 + 1 + entry.first_col,
                    r0 + 1 + entry.first
                ),
                Some(entry.item.clone()),
                "{entry:?}"
            );
        }
        // A border cell is not an item, and neither is the row after the last
        // one.
        assert_eq!(skill_item_at(&vm, cols, rows, c0 + 1, r0), None);
        assert_eq!(
            skill_item_at(&vm, cols, rows, c0 + 3, r0 + 1 + items[5].last + 1),
            None
        );

        // The pick paints reverse video on its own rows and nowhere else.
        vm.sel_skill = Some(1);
        let painted = skill_lines(&vm, inner_w);
        for (i, row) in painted.iter().enumerate() {
            let mine = i >= items[1].first && i <= items[1].last;
            assert_eq!(row.selected, mine, "row {i} ({:?})", row.text);
        }
    }

    #[test]
    fn the_read_up_box_only_replaces_the_transcript_while_the_pane_is_focused() {
        let _guard = crate::watch::ansi::color_test_lock();
        set_color_enabled(false);
        let mut vm = empty_vm();
        vm.skill_detail = Some(SkillDetail {
            title: "skill · tdd".into(),
            lines: vec![plain("body", c::white)],
        });

        // With nothing picked and the transcript owning the column, no box.
        assert!(skill_detail_box(&vm, 40, 10).is_none());
        vm.focus = 4;
        let box_lines = skill_detail_box(&vm, 40, 10).expect("the read-up box");
        assert_eq!(
            box_lines.len(),
            10,
            "the box is exactly as tall as its column"
        );
        assert!(box_lines[0].contains("skill · tdd"), "{:?}", box_lines[0]);
        let frame = strip_ansi(&render_frame(&vm, 100, 30));
        assert!(
            frame.contains("skill · tdd"),
            "the frame carries the read-up"
        );

        // Unpicked, the same frame is the transcript again.
        vm.skill_detail = None;
        assert!(!strip_ansi(&render_frame(&vm, 100, 30)).contains("skill · tdd"));
        // An empty read-up still paints a box, never a blank column.
        vm.skill_detail = Some(SkillDetail {
            title: "tool · READ".into(),
            lines: Vec::new(),
        });
        assert_eq!(skill_detail_box(&vm, 40, 10).map(|l| l.len()), Some(10));
    }

    #[test]
    fn the_pick_lights_only_its_own_name() {
        let _guard = crate::watch::ansi::color_test_lock();
        set_color_enabled(true);
        let mut vm = empty_vm();
        vm.skill_loads = vec![SkillLoad {
            iteration: 1,
            skills: vec!["navis".into(), "tdd".into()],
            tools: vec![],
            plans: vec![],
        }];
        vm.focus = 4;
        let inner_w = 30;
        let items = skill_items(&vm, inner_w);
        let pick = items
            .iter()
            .position(|entry| entry.item.name() == "tdd")
            .expect("the rolled-up pick");
        vm.sel_skill = Some(pick);
        let rows = skill_lines(&vm, inner_w);
        let line = rows[items[pick].first].text.clone();
        assert!(
            line.contains("navis"),
            "the picked name shares its row with another: {line:?}"
        );

        let painted = render_pane(
            inner_w + 2,
            3,
            "T",
            true,
            &rows[items[pick].first..=items[pick].first],
            None,
        );
        let body = &painted[1];
        assert!(
            body.contains(&c::on_cyan("tdd")),
            "the pick paints in reverse video: {body:?}"
        );
        assert!(
            !body.contains(&c::on_cyan("navis")),
            "the rest of the row keeps its own colour: {body:?}"
        );
        assert_eq!(body.matches("\x1b[46;30m").count(), 1, "{body:?}");
        assert_eq!(
            strip_ansi(body).matches("navis, tdd").count(),
            1,
            "{body:?}"
        );
    }

    #[test]
    fn the_read_up_scrolls_through_the_whole_listing() {
        let _guard = crate::watch::ansi::color_test_lock();
        set_color_enabled(false);
        let mut vm = empty_vm();
        vm.focus = 4;
        let lines: Vec<RowCell> = (0..40)
            .map(|i| plain(format!("line {i:02}"), c::white))
            .collect();
        vm.skill_detail = Some(SkillDetail {
            title: "skill · navis".into(),
            lines,
        });
        let (cols, rows) = (100usize, 30usize);
        let max = skill_detail_max_scroll(&vm, cols, rows);
        assert!(
            max > 0 && max < 40,
            "the listing overflows the column: {max}"
        );
        let (w, h) = (60usize, 29usize);

        // Unscrolled, the box starts at the top of the listing...
        let first = strip_ansi(&skill_detail_box(&vm, w, h).unwrap().join("\n"));
        assert!(first.contains("line 00"), "{first:?}");
        assert!(!first.contains("line 39"), "{first:?}");

        // ...and at the far end it seats the last line, with the footer saying
        // where in the listing the box sits. An out-of-range offset clamps.
        vm.skill_detail_scroll = 999;
        let last = strip_ansi(&skill_detail_box(&vm, w, h).unwrap().join("\n"));
        assert!(last.contains("line 39"), "{last:?}");
        assert!(!last.contains("line 00"), "{last:?}");
        assert!(
            last.contains("of 40"),
            "the footer counts the whole listing: {last:?}"
        );
    }

    #[test]
    fn loop_start_telemetry_reads_plans_and_omits_an_empty_set() {
        use crate::cli::transcript::{TranscriptEntry, TranscriptEventEntry};
        use crate::core::types::HarnessEventData;

        fn loop_start(iteration: i64, plans: Option<Vec<&str>>) -> TranscriptEntry {
            // A planning loop's plans and its composed skills arrive together,
            // so the fixture keys the skills off the plans: a plans-free loop
            // is skipped by the pane exactly as a bare loop is.
            let plans: Vec<String> = plans
                .unwrap_or_default()
                .into_iter()
                .map(str::to_string)
                .collect();
            let skills: Vec<String> = if plans.is_empty() {
                Vec::new()
            } else {
                vec!["navis".to_string()]
            };
            TranscriptEntry::Event(TranscriptEventEntry {
                at: "2026-01-01T00:00:00Z".into(),
                data: Some(HarnessEventData {
                    plans: Some(plans),
                    skills: Some(skills),
                    ..Default::default()
                }),
                detail: "loop 1".into(),
                goal_id: "g1".into(),
                iteration,
                kind: HarnessEventType::LoopStart,
            })
        }

        let transcript = vec![
            loop_start(1, Some(vec!["goal-shaping"])),
            loop_start(2, None),
            loop_start(3, Some(vec![])),
            loop_start(4, Some(vec!["goal-shaping", "review-brief"])),
        ];
        let loads = skill_loads(&transcript);
        assert_eq!(
            loads,
            vec![
                SkillLoad {
                    iteration: 1,
                    skills: vec!["navis".into()],
                    tools: vec![],
                    plans: vec!["goal-shaping".into()],
                },
                SkillLoad {
                    iteration: 4,
                    skills: vec!["navis".into()],
                    tools: vec![],
                    plans: vec!["goal-shaping".into(), "review-brief".into()],
                },
            ]
        );
        // A loop shaped by a plan but composing no skills is still a surface.
        assert_eq!(skill_loads(&[loop_start(6, Some(vec!["solo"]))]).len(), 1);
        // The roll-up counts loops per plan, most-chosen first (ties by name).
        assert_eq!(
            all_loaded_plans(&loads),
            vec![
                ("goal-shaping".to_string(), 2),
                ("review-brief".to_string(), 1)
            ]
        );
        assert!(all_loaded_plans(&[]).is_empty());
    }

    #[test]
    fn the_skills_and_tools_pane_lists_each_loops_chosen_plans() {
        let _guard = crate::watch::ansi::color_test_lock();
        set_color_enabled(false);
        let mut vm = empty_vm();
        vm.skill_loads = vec![
            SkillLoad {
                iteration: 1,
                skills: vec!["navis".into()],
                tools: vec!["READ".into()],
                plans: vec!["goal-shaping".into()],
            },
            SkillLoad {
                iteration: 2,
                skills: vec!["tdd".into()],
                tools: vec!["PATCH".into()],
                plans: vec![],
            },
        ];
        let texts: Vec<String> = skill_lines(&vm, 40)
            .iter()
            .map(|r| r.text.clone())
            .collect();
        assert!(
            texts.contains(&"all chosen plans (1)".to_string()),
            "{texts:?}"
        );
        assert!(
            texts
                .iter()
                .any(|t| t.starts_with("  plans   goal-shaping")),
            "{texts:?}"
        );
        // Only the loop that chose one prints a plans row.
        assert_eq!(
            texts.iter().filter(|t| t.starts_with("  plans")).count(),
            1,
            "{texts:?}"
        );
        assert!(
            texts.contains(&"all loaded skills (2)".to_string()),
            "{texts:?}"
        );
        assert!(
            texts.contains(&"all available tools (2)".to_string()),
            "{texts:?}"
        );
        assert!(texts.iter().all(|t| string_width(t) <= 40), "{texts:?}");
    }
}
