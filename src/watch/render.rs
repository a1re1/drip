// Pure frame renderer for the dripw watch TUI: render_frame(vm, cols, rows)
// returns exactly `rows` lines, each exactly `cols` visible columns. No I/O,
// no clock — `vm.now` is the only time it may read.

use std::collections::HashMap;

use crate::cli::transcript::{format_model_route_lines, TranscriptEntry};
use crate::core::sessions::SessionRecord;
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

/// Which session list owns the transcript/shells: Running or Recent. Unlike
/// `focus`, this never becomes 3 — drilling into the Shells pane must not move
/// the session selection out from under the shells being inspected.
pub type SessionFocus = u8;

#[derive(Debug, Clone)]
pub struct WatchViewModel {
    /// Epoch ms — the only clock render_frame may read.
    pub now: i64,
    pub running: Vec<SessionRecord>,
    pub recent: Vec<SessionRecord>,
    /// Lease started_at as epoch ms, keyed by session id (running only).
    pub started_at_ms: HashMap<String, i64>,
    pub focus: FocusPane,
    pub session_focus: SessionFocus,
    /// Absolute index into running / recent (clamped by the app).
    pub sel_running: usize,
    pub sel_recent: usize,
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

fn running_row(r: &SessionRecord, now: i64, started_at_ms: Option<i64>, inner_w: usize, selected: bool) -> RowCell {
    let elapsed_sec = started_at_ms.map(|started| ((now - started) / 1000).max(0));
    let right = format!(" {}", fmt_duration(elapsed_sec));
    let main = format!("{DOT} {} {}", short_id(&r.id), goal_text(r));
    let left_w = inner_w.saturating_sub(string_width(&right));
    selectable(format!("{}{}", fit(&main, left_w, true), right), c::green, selected)
}

fn recent_row(r: &SessionRecord, now: i64, inner_w: usize, selected: bool) -> RowCell {
    let right = format!(" {}", rel_time(Some(&r.updated_at), now));
    let main = format!("{DOT} {} {}", short_id(&r.id), goal_text(r));
    let left_w = inner_w.saturating_sub(string_width(&right));
    selectable(format!("{}{}", fit(&main, left_w, true), right), status_dot_color(&r.status), selected)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ListKind {
    Running,
    Recent,
}

#[allow(clippy::too_many_arguments)]
fn session_rows(
    list: &[SessionRecord],
    sel: usize,
    focused: bool,
    inner_w: usize,
    inner_h: usize,
    kind: ListKind,
    now: i64,
    started_at_ms: &HashMap<String, i64>,
) -> Vec<RowCell> {
    if list.is_empty() {
        return vec![plain("  no sessions yet", c::dim)];
    }
    let start = scroll_start(sel, list.len(), inner_h);
    (start..list.len().min(start + inner_h))
        .map(|i| {
            let r = &list[i];
            let selected = focused && i == sel;
            match kind {
                ListKind::Running => running_row(r, now, started_at_ms.get(&r.id).copied(), inner_w, selected),
                ListKind::Recent => recent_row(r, now, inner_w, selected),
            }
        })
        .collect()
}

// ── Shell rows ───────────────────────────────────────────────────────────────

/// True when the currently focused selection is a running (live-lease) session.
fn focused_is_running(vm: &WatchViewModel) -> bool {
    let (list, sel) = if vm.session_focus == 1 { (&vm.running, vm.sel_running) } else { (&vm.recent, vm.sel_recent) };
    list.get(sel).is_some_and(|r| vm.started_at_ms.contains_key(&r.id))
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

fn running_title(vm: &WatchViewModel) -> String {
    format!("[1] Running ({})", vm.running.len())
}

fn recent_title(vm: &WatchViewModel) -> String {
    format!("[2] Recent ({})", vm.recent.len())
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
    let (list, sel) = if vm.session_focus == 1 { (&vm.running, vm.sel_running) } else { (&vm.recent, vm.sel_recent) };
    let Some(r) = list.get(sel) else { return "[0] Transcript".to_string() };
    let status = if vm.session_focus == 1 && vm.started_at_ms.contains_key(&r.id) { "running" } else { r.status.as_str() };
    format!("[0] {} · {status} · {}", short_id(&r.id), goal_text(r))
}

fn pos_note(sel: usize, len: usize) -> Option<String> {
    if len == 0 {
        return None;
    }
    Some(format!("{}/{len}", (sel + 1).min(len)))
}

// ── Frame ────────────────────────────────────────────────────────────────────

const FOOTER_HINT: &str = "1/2/3 focus · tab cycle · j/k move · [/] h/l scroll log · q quit";

/// Pure full-frame render. Returns a single string of exactly `rows` lines
/// joined by \n, each line exactly `cols` visible columns.
pub fn render_frame(vm: &WatchViewModel, cols: usize, rows: usize) -> String {
    let cols = cols.max(1);
    let rows = rows.max(1);

    if cols < 20 || rows < 8 {
        let msg = fit(" dripw: terminal too small", cols, true);
        let lines: Vec<String> = (0..rows).map(|i| if i == 0 { msg.clone() } else { fit("", cols, false) }).collect();
        return lines.join("\n");
    }

    let footer = c::dim(&fit(FOOTER_HINT, cols, true));
    let body_h = rows - 1;

    let run_focused = vm.focus == 1;
    let recent_focused = vm.focus == 2;

    let mk_running = |w: usize, h: usize, height: usize| -> Vec<String> {
        render_pane(
            w,
            height,
            &running_title(vm),
            run_focused,
            &session_rows(&vm.running, vm.sel_running, run_focused, w.saturating_sub(2), h, ListKind::Running, vm.now, &vm.started_at_ms),
            pos_note(vm.sel_running, vm.running.len()).as_deref(),
        )
    };

    let mk_recent = |w: usize, h: usize, height: usize| -> Vec<String> {
        render_pane(
            w,
            height,
            &recent_title(vm),
            recent_focused,
            &session_rows(&vm.recent, vm.sel_recent, recent_focused, w.saturating_sub(2), h, ListKind::Recent, vm.now, &vm.started_at_ms),
            pos_note(vm.sel_recent, vm.recent.len()).as_deref(),
        )
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

    let body: Vec<String> = if cols < PORTRAIT_MAX_COLS {
        // Portrait: [1] / [2] / Shells / [0] stacked full-width; lists hug,
        // transcript absorbs the reclaimed rows.
        let heights = portrait_heights(body_h, &[vm.running.len(), vm.recent.len(), vm.shells.len(), 0]);
        let (h1, h2, h3, h0) = (heights[0], heights[1], heights[2], heights[3]);
        let mut body = mk_running(cols, h1.saturating_sub(2), h1);
        body.extend(mk_recent(cols, h2.saturating_sub(2), h2));
        body.extend(mk_shells(cols, h3));
        body.extend(mk_zero(cols, h0));
        body
    } else {
        // Landscape: left [1]/[2]/Shells (~40% width), right full-height [0].
        let left_w = (cols * 2 / 5).max(30).min(cols - 20);
        let right_w = cols - left_w;
        // Content-hug running and shells; recent takes the rest of the left column.
        let run_desired = vm.running.len().max(1) + 2;
        let shells_desired = vm.shells.len().max(1) + 2;
        let mut h1 = run_desired.min(MIN_PANE.max(body_h.saturating_sub(2 * MIN_PANE)));
        let mut h3 = shells_desired.min(MIN_PANE.max(body_h.saturating_sub(h1 + MIN_PANE)));
        let mut h2 = body_h as i64 - h1 as i64 - h3 as i64;
        if h2 < MIN_PANE as i64 {
            let capped = split_heights(body_h, &[4, 4, 3]);
            h1 = capped[0];
            h2 = capped[1] as i64;
            h3 = capped[2];
        }
        let h2 = h2.max(0) as usize;
        let mut left = mk_running(left_w, h1.saturating_sub(2), h1);
        left.extend(mk_recent(left_w, h2.saturating_sub(2), h2));
        left.extend(mk_shells(left_w, h3));
        let right = mk_zero(right_w, body_h);
        hconcat(&left, &right)
    };

    let mut all = body;
    all.push(footer);
    while all.len() < rows {
        all.push(fit("", cols, false));
    }
    all.truncate(rows);
    all.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watch::ansi::set_color_enabled;

    fn empty_vm() -> WatchViewModel {
        WatchViewModel {
            now: 0,
            running: vec![],
            recent: vec![],
            started_at_ms: HashMap::new(),
            focus: 1,
            session_focus: 1,
            sel_running: 0,
            sel_recent: 0,
            transcript: vec![],
            following: true,
            transcript_scroll: 0,
            shells: vec![],
            sel_shell: 0,
            shell_log_lines: vec![],
            shell_log_files: vec![],
        }
    }

    fn record(id: &str, goal: &str) -> SessionRecord {
        SessionRecord {
            sessions_dir: None,
            created_at: "2026-01-01T00:00:00.000Z".into(),
            cwd: "/r".into(),
            goal_count: 1,
            id: id.into(),
            last_goal: Some(goal.into()),
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
        vm.running.push(record("aaaaaaaa-1", "run a goal that is rather long so it gets clipped by the pane"));
        vm.recent.push(record("bbbbbbbb-2", "old goal"));
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
}
