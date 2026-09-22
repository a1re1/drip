// Pure frame renderer for the dripw watch TUI: render_frame(vm, cols, rows)
// returns exactly `rows` lines, each exactly `cols` visible columns. No I/O,
// no clock — `vm.now` is the only time it may read.

use std::collections::HashMap;

use crate::cli::transcript::{format_model_route_lines, TranscriptEntry};
use crate::core::sessions::SessionRecord;
use crate::core::types::{HarnessTask, HarnessTaskStatus};
use crate::watch::ansi::{c, char_width, fit, string_width, strip_ansi};
use crate::watch::ps::PsProc;
pub use crate::watch::transcript_view::{flatten_transcript, RowCell};

// Below this many columns the panes stack vertically (portrait).
pub const PORTRAIT_MAX_COLS: usize = 90;

const DOT: &str = "●";
const TRANSCRIPT_FLOOR: usize = 6; // content rows floor for [0] in portrait
const MIN_PANE: usize = 3; // border+border + 1 content row

// ── View model ───────────────────────────────────────────────────────────────

/// 1 | 2 | 3
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
    /// The focused session's task ledger (empty when it has none / unreadable).
    pub tasks: Vec<HarnessTask>,
    /// Selection index into the ordered task list (used when focus == 2).
    pub sel_task: usize,
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
    RowCell { text: text.into(), color: Some(color), selected: false, rich: false }
}

fn selectable(text: impl Into<String>, color: fn(&str) -> String, selected: bool) -> RowCell {
    RowCell { text: text.into(), color: Some(color), selected, rich: false }
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
                if row.selected {
                    // on_cyan already pairs cyan bg with dark fg (46;30)
                    c::on_cyan(&fitted)
                } else {
                    (row.color.unwrap_or(c::white))(&fitted)
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
            let label = if tasks[i].title.trim().is_empty() { tasks[i].id.as_str() } else { tasks[i].title.as_str() };
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
    vm.sessions.get(vm.sel_session).is_some_and(|r| vm.started_at_ms.contains_key(&r.id))
}

fn shell_rows(vm: &WatchViewModel, inner_w: usize, inner_h: usize) -> Vec<RowCell> {
    if vm.shells.is_empty() {
        let msg = if focused_is_running(vm) { "  (no shell processes)" } else { "  (no running session focused)" };
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
            selectable(format!("{}{}", fit(&main, left_w, true), right), c::green, focused && i == vm.sel_shell)
        })
        .collect()
}

// The [0] column when a shell is focused: details of the selected process — a
// shell has no transcript, so the full (untruncated) command line plus its
// child processes are the useful "log-equivalent" header.
fn shell_detail_rows(vm: &WatchViewModel, inner_w: usize) -> Vec<RowCell> {
    let Some(p) = vm.shells.get(vm.sel_shell) else { return vec![plain("  (no process selected)", c::dim)] };
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
    if out.is_empty() { vec![String::new()] } else { out }
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

const FOOTER_HINT: &str = "1/2/3 focus · tab cycle · r mode · click/j/k move · [/] h/l/wheel scroll log · q quit";

/// Pure full-frame render. Returns a single string of exactly `rows` lines
/// joined by \n, each line exactly `cols` visible columns.
/// The pane geometry `render_frame` paints with, read by every hit test so the
/// rectangles a click is measured against cannot drift from the frame.
struct PaneLayout {
    /// Sessions / Tasks / Shells heights, top to bottom.
    heights: [usize; 3],
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
        let heights = portrait_heights(body_h, &[vm.sessions.len(), vm.tasks.len(), vm.shells.len(), 0]);
        return Some(PaneLayout {
            heights: [heights[0], heights[1], heights[2]],
            list_w: cols,
            zero_h: heights[3],
            zero_w: None,
        });
    }

    // Landscape: left [1]/[2]/Shells (~40% width), right full-height [0].
    // Content-hug sessions and shells; tasks takes the rest of the left column.
    let list_w = (cols * 2 / 5).max(30).min(cols - 20);
    let sessions_desired = vm.sessions.len().max(1) + 2;
    let shells_desired = vm.shells.len().max(1) + 2;
    let mut h1 = sessions_desired.min(MIN_PANE.max(body_h.saturating_sub(2 * MIN_PANE)));
    let mut h3 = shells_desired.min(MIN_PANE.max(body_h.saturating_sub(h1 + MIN_PANE)));
    let mut h2 = body_h as i64 - h1 as i64 - h3 as i64;
    if h2 < MIN_PANE as i64 {
        let capped = split_heights(body_h, &[4, 4, 3]);
        h1 = capped[0];
        h2 = capped[1] as i64;
        h3 = capped[2];
    }
    Some(PaneLayout {
        heights: [h1, h2.max(0) as usize, h3],
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

    // The [0] column: the transcript normally; a shell-detail box over a raw
    // stdout/stderr tail box when the Shells pane is focused (sub-zero's
    // renderRight). Always emits exactly `height` lines.
    let mk_zero = |w: usize, height: usize| -> Vec<String> {
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
    let (h1, h2, h3) = (layout.heights[0], layout.heights[1], layout.heights[2]);

    let body: Vec<String> = match layout.zero_w {
        None => {
            let mut body = mk_sessions(layout.list_w, h1.saturating_sub(2), h1);
            body.extend(mk_tasks(layout.list_w, h2.saturating_sub(2), h2));
            body.extend(mk_shells(layout.list_w, h3));
            body.extend(mk_zero(layout.list_w, layout.zero_h));
            body
        }
        Some(right_w) => {
            let left_w = layout.list_w;
            let mut left = mk_sessions(left_w, h1.saturating_sub(2), h1);
            left.extend(mk_tasks(left_w, h2.saturating_sub(2), h2));
            left.extend(mk_shells(left_w, h3));
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
/// Sessions, `[1]` Tasks, `[2]` Shells — as `(row_start, row_end, col_start,
/// col_end)`.
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
/// last row, or when the terminal is too small to paint the frame.
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
            _ => (vm.shells.len(), vm.sel_shell),
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
            tasks: vec![],
            sel_task: 0,
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
            assert_eq!(regions.len(), 3, "{cols}x{rows}");
            let (sr0, sr1, sc0, sc1) = regions[0];
            // Every pane's rows are bracketed by its own top and bottom border.
            assert_eq!(list_row_at(&vm, cols, rows, sc0 + 3, sr0), None, "top border {cols}x{rows}");
            assert_eq!(list_row_at(&vm, cols, rows, sc0 + 3, sr1), None, "bottom border {cols}x{rows}");
            assert_eq!(list_row_at(&vm, cols, rows, sc0 - 1, sr0 + 1), None, "left of pane");
            assert_eq!(list_row_at(&vm, cols, rows, sc1 + 1, sr0 + 1), None, "right of pane");
            for i in 0..4 {
                assert_eq!(list_row_at(&vm, cols, rows, sc0 + 3, sr0 + 1 + i), Some((0, i)), "session {i} {cols}x{rows}");
            }
            // The row after the last session is blank, not a fourth pane hit.
            assert_eq!(list_row_at(&vm, cols, rows, sc0 + 3, sr0 + 1 + 4), None, "past the last session");

            let (tr0, tr1, _, _) = regions[1];
            assert_eq!(list_row_at(&vm, cols, rows, sc0 + 3, tr0 + 1), Some((1, 0)), "first task");
            assert_eq!(list_row_at(&vm, cols, rows, sc0 + 3, tr0 + 2), Some((1, 1)), "second task");
            assert_eq!(list_row_at(&vm, cols, rows, sc0 + 3, tr1), None, "task bottom border");

            let (kr0, _, kc0, _) = regions[2];
            assert_eq!(list_row_at(&vm, cols, rows, kc0 + 3, kr0 + 1), Some((2, 0)), "first shell");
            assert_eq!(list_row_at(&vm, cols, rows, kc0 + 3, kr0 + 2), None, "no second shell");
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
        let rows = vec![plain("hello", c::white), RowCell { text: "rich".into(), color: None, selected: false, rich: true }];
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
        assert_eq!(task_texts(&rows), vec!["◐ active one", "○ pending one", "● done one", "✗ stuck one", "○ cut one"]);
        assert!(rows[0].selected, "the focused pane highlights the selected task");
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
        assert_eq!(task_count_note(&tasks).as_deref(), Some("2 pending · 1 completed"));
        let mut vm = empty_vm();
        vm.tasks = tasks;
        assert_eq!(tasks_title(&vm), "[2] Tasks (3)");
    }

    #[test]
    fn task_rows_follow_the_scroll_window() {
        let _guard = crate::watch::ansi::color_test_lock();
        set_color_enabled(false);
        let mut vm = empty_vm();
        let mut tasks: Vec<HarnessTask> =
            (0..10).map(|i| task(&format!("task-{i}"), &format!("t{i}"), HarnessTaskStatus::Pending)).collect();
        tasks.push(task("task-x", "active", HarnessTaskStatus::InProgress));
        vm.tasks = tasks;
        // The window starts at the selected row: first row selected shows the
        // floated in_progress task, last row selected shows the ledger's end.
        vm.sel_task = 0;
        assert_eq!(task_texts(&task_rows(&vm, 24, 2)), vec!["◐ active", "○ t0"]);
        vm.sel_task = 10;
        assert_eq!(task_texts(&task_rows(&vm, 24, 3)), vec!["○ t7", "○ t8", "○ t9"]);
    }

    #[test]
    fn empty_task_ledger_renders_the_empty_row_and_no_note() {
        let _guard = crate::watch::ansi::color_test_lock();
        set_color_enabled(false);
        let vm = empty_vm();
        assert_eq!(task_count_note(&vm.tasks), None);
        assert_eq!(task_rows(&vm, 24, 5)[0].text, "  no task ledger yet");
    }
}
