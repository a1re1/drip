// The dripw application: a raw-mode, alt-screen, diff-painted watcher over
// the session index. Node drives it from timers and stdin events on one
// loop; here one thread owns the state and multiplexes a channel fed by a
// stdin reader thread, a SIGWINCH flag, and a ps worker, with the two
// timers (session list every 2s, transcript tail every 300ms) folded into
// the channel's receive timeout.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant, SystemTime};

use crate::core::home::DripProject;
use crate::core::lease::{check_lease, LeaseStatus};
use crate::core::sessions::{list_all_home_sessions, session_paths_for, SessionRecord};
use crate::watch::ansi::term;
use crate::watch::data::{classify_sessions, sessions_under_dir, trim_transcript, TranscriptTail};
use crate::watch::ps::{descendants, list_processes, PsProc};
use crate::watch::mouse::parse_sgr_mouse;
use crate::watch::render::{diff_lines, render_frame, transcript_region, SessionsMode, WatchViewModel};
use crate::watch::shelllog::{read_shell_log_files, seed_shell_log, LineFollower, SHELL_TAIL_BYTES};

const LIST_MS: u64 = 2000; // session-list refresh
const TAIL_MS: u64 = 300; // transcript tail poll
const REPLAY_LIMIT: usize = 100; // entries replayed when focusing a session
const PAGE: i64 = 8; // lines per [ ] / h / l scroll step
const WHEEL_LINES: i64 = 3; // lines per scroll-wheel notch
const MAX_TRANSCRIPT: usize = 4000; // cap retained entries
const MAX_SHELL_LOG: usize = 4000; // cap retained raw stdout/stderr lines

fn clamp_sel(sel: i64, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    if sel < 0 {
        return 0;
    }
    if sel as usize >= len {
        return len - 1;
    }
    sel as usize
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn empty_vm(now: i64) -> WatchViewModel {
    WatchViewModel {
        now,
        mode: SessionsMode::Running,
        sessions: Vec::new(),
        started_at_ms: HashMap::new(),
        focus: 1,
        sel_session: 0,
        transcript: Vec::new(),
        following: true,
        transcript_scroll: 0,
        shells: Vec::new(),
        sel_shell: 0,
        shell_log_lines: Vec::new(),
        shell_log_files: Vec::new(),
        tasks: Vec::new(),
        sel_task: 0,
    }
}

// ── terminal plumbing ────────────────────────────────────────────────────────

static WINCH: AtomicBool = AtomicBool::new(false);

extern "C" fn on_winch(_: libc::c_int) {
    WINCH.store(true, Ordering::SeqCst);
}

static HALT: AtomicBool = AtomicBool::new(false);

extern "C" fn on_halt(_: libc::c_int) {
    HALT.store(true, Ordering::SeqCst);
}

/// Raw mode for the lifetime of the app; restored on drop (and on stop).
use crate::tui::term::{terminal_size, write_out, RawMode};

enum Msg {
    Key(String),
    Procs(Vec<PsProc>),
}

// Order-sensitive equality of two string lists (the discovered fd files).
fn same_list(a: &[String], b: &[String]) -> bool {
    a == b
}

/// The rows visible under `mode`: Running → running only, Recent → recent
/// only, All → running followed by recent. Both inputs are already sorted
/// newest-first, so All is a plain concatenation with no re-sort.
fn visible_sessions(mode: SessionsMode, running: &[SessionRecord], recent: &[SessionRecord]) -> Vec<SessionRecord> {
    match mode {
        SessionsMode::Running => running.to_vec(),
        SessionsMode::Recent => recent.to_vec(),
        SessionsMode::All => {
            let mut out = running.to_vec();
            out.extend_from_slice(recent);
            out
        }
    }
}

pub struct WatchApp {
    project: DripProject,
    /// Directory dripw was launched from; the only visibility rule there is.
    watch_dir: String,
    vm: WatchViewModel,
    running: bool,
    /// Sessions with a live lease, newest-first. Cached so `r` can re-filter
    /// the Sessions pane without re-listing every home registry.
    running_sessions: Vec<SessionRecord>,
    /// Every other visible session, newest-first.
    recent_sessions: Vec<SessionRecord>,
    /// state.json path and mtime behind `vm.tasks`, so an unchanged ledger is
    /// not reparsed on every tail tick.
    tasks_src: Option<(String, SystemTime)>,
    tail: Option<TranscriptTail>,
    focused_id: String,
    /// Lease pid per running session id, refreshed with the session list.
    pid_by_id: HashMap<String, i32>,
    ps_in_flight: bool,
    /// Pid whose stdout/stderr is currently seeded/followed (focus == 3).
    cur_shell_pid: i64,
    shell_followers: Vec<LineFollower>,
    last_frame: String,
    last_lines: Vec<String>,
    force_full: bool, // first paint + resize
    stopping: bool,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
}

impl WatchApp {
    pub fn new(project: DripProject, watch_dir: String) -> Self {
        let (tx, rx) = mpsc::channel();
        let vm = empty_vm(now_ms());

        Self {
            project,
            watch_dir,
            vm,
            running: false,
            running_sessions: Vec::new(),
            recent_sessions: Vec::new(),
            tasks_src: None,
            tail: None,
            focused_id: String::new(),
            pid_by_id: HashMap::new(),
            ps_in_flight: false,
            cur_shell_pid: 0,
            shell_followers: Vec::new(),
            last_frame: String::new(),
            last_lines: Vec::new(),
            force_full: true,
            stopping: false,
            tx,
            rx,
        }
    }

    /// Blocks until the user quits (q / Ctrl+C / SIGINT / SIGTERM).
    pub fn start(&mut self) {
        if self.running {
            return;
        }
        self.running = true;

        // Mouse tracking is on for the app's lifetime so the wheel reports the
        // hovered cell; every exit path below turns it back off.
        write_out(&format!("{}{}{}{}", term::ALT_SCREEN, term::HIDE_CURSOR, term::CLEAR, term::ENABLE_MOUSE));

        let mut raw = RawMode::enable();

        // A panic anywhere (render, data, sqlite) must not strand the shell on
        // a cursor-less alt screen: leave the screen before the message prints.
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            write_out(&format!("{}{}{}", term::DISABLE_MOUSE, term::SHOW_CURSOR, term::MAIN_SCREEN));
            default_hook(info);
        }));

        self.setup_input();
        self.setup_resize();
        self.setup_signals();

        self.refresh_sessions();
        self.draw();

        let mut next_list = Instant::now() + Duration::from_millis(LIST_MS);
        let mut next_tail = Instant::now() + Duration::from_millis(TAIL_MS);

        while self.running {
            let now = Instant::now();

            if now >= next_list {
                next_list = now + Duration::from_millis(LIST_MS);
                self.tick_list();
            }
            if now >= next_tail {
                next_tail = now + Duration::from_millis(TAIL_MS);
                self.tick_tail();
            }
            if WINCH.swap(false, Ordering::SeqCst) {
                self.force_full = true;
                self.draw();
            }
            if HALT.load(Ordering::SeqCst) {
                self.stop();
                break;
            }

            let wait = next_list.min(next_tail).saturating_duration_since(Instant::now());

            match self.rx.recv_timeout(wait) {
                Ok(Msg::Key(key)) => self.on_input(&key),
                Ok(Msg::Procs(procs)) => self.on_procs(procs),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => self.stop(),
            }
        }

        raw.restore();
        write_out(&format!("{}{}{}", term::DISABLE_MOUSE, term::SHOW_CURSOR, term::MAIN_SCREEN));

        std::process::exit(0);
    }

    pub fn stop(&mut self) {
        if self.stopping {
            return;
        }
        self.stopping = true;
        self.running = false;
        self.tail = None;
    }

    // ── polling ──────────────────────────────────────────────────────────────

    fn tick_list(&mut self) {
        self.refresh_sessions();
        self.refresh_shells();
        self.sync_shell_log();
        self.draw();
    }

    // Keep the drilled-in shell view pointed at the selected process: rediscover
    // the files behind its fd 1/2 (they change when the shell re-execs or
    // redirects) and reseed the tail whenever the pid or file list moves.
    fn sync_shell_log(&mut self) {
        if self.vm.focus != 3 {
            return;
        }
        let Some(p) = self.vm.shells.get(self.vm.sel_shell).cloned() else {
            self.cur_shell_pid = 0;
            self.shell_followers = Vec::new();
            self.vm.shell_log_lines = Vec::new();
            self.vm.shell_log_files = Vec::new();
            return;
        };
        let files = read_shell_log_files(p.pid);
        if p.pid == self.cur_shell_pid && same_list(&files, &self.vm.shell_log_files) {
            return;
        }
        self.cur_shell_pid = p.pid;
        self.vm.shell_log_files = files.clone();
        let seed = seed_shell_log(&files, SHELL_TAIL_BYTES);
        let keep_from = seed.lines.len().saturating_sub(MAX_SHELL_LOG);
        self.vm.shell_log_lines = seed.lines[keep_from..].to_vec();
        self.shell_followers = seed.followers;
        self.vm.following = true;
        self.vm.transcript_scroll = 0;
    }

    fn focused_pid(&self) -> Option<i32> {
        if self.focused_id.is_empty() {
            return None;
        }
        self.pid_by_id.get(&self.focused_id).copied()
    }

    /// Snapshot the focused running session's child processes (async, 2s tick).
    fn refresh_shells(&mut self) {
        if self.focused_pid().is_none() {
            self.vm.shells = Vec::new();
            return;
        }
        if self.ps_in_flight {
            return;
        }
        self.ps_in_flight = true;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Procs(list_processes()));
        });
    }

    fn on_procs(&mut self, procs: Vec<PsProc>) {
        self.ps_in_flight = false;
        if !self.running {
            return;
        }
        // The focus may have moved while ps ran; only apply a still-current answer.
        match self.focused_pid() {
            None => self.vm.shells = Vec::new(),
            Some(pid) => self.vm.shells = descendants(i64::from(pid), &procs),
        }
        self.vm.sel_shell = clamp_sel(self.vm.sel_shell as i64, self.vm.shells.len());
        self.draw();
    }

    fn tick_tail(&mut self) {
        let mut dirty = false;

        if let Some(tail) = self.tail.as_mut() {
            let added = tail.poll();
            if !added.is_empty() {
                let mut all = std::mem::take(&mut self.vm.transcript);
                all.extend(added);
                self.vm.transcript = trim_transcript(&all, MAX_TRANSCRIPT);
                dirty = true;
            }
        }

        if self.vm.focus == 3 && !self.shell_followers.is_empty() {
            let mut added: Vec<String> = Vec::new();
            for follower in self.shell_followers.iter_mut() {
                added.extend(follower.poll());
            }
            if !added.is_empty() {
                self.vm.shell_log_lines.extend(added);
                let keep_from = self.vm.shell_log_lines.len().saturating_sub(MAX_SHELL_LOG);
                self.vm.shell_log_lines.drain(..keep_from);
                dirty = true;
            }
        }

        // A running session's ledger grows with no transcript line appended, so
        // re-read it every tail tick and repaint when it moved.
        let selected = self.selected_record();
        let before = self.vm.tasks.clone();
        self.load_tasks(selected.as_ref());
        if self.vm.tasks != before {
            dirty = true;
        }

        if !dirty {
            return;
        }
        // New lines pin to the bottom only while following.
        if self.vm.following {
            self.vm.transcript_scroll = 0;
        }
        self.draw();
    }

    fn refresh_sessions(&mut self) {
        self.vm.now = now_ms();

        // Every project registry under the drip home is a source; the launch
        // directory decides visibility (equal dir or any directory beneath it).
        // The filter runs before leases/classification so Running and Recent see
        // exactly the same sessions, with no row cap ahead of the filter.
        // A listing panic (a home wiped mid-run) must not kill the tick: show
        // empty lists and retry next tick.
        let home_root = self.project.home_root.clone();
        let watch_dir = self.watch_dir.clone();
        let records: Vec<SessionRecord> = match std::panic::catch_unwind(move || {
            sessions_under_dir(&list_all_home_sessions(Path::new(&home_root)), &watch_dir)
        }) {
            Ok(records) => records,
            Err(_) => {
                // Drop every cached view of the sessions that just vanished:
                // rows, timers, pids, and the focused transcript/ledger.
                self.running_sessions = Vec::new();
                self.recent_sessions = Vec::new();
                self.vm.sessions = Vec::new();
                self.vm.started_at_ms = HashMap::new();
                self.pid_by_id = HashMap::new();
                self.vm.shells = Vec::new();
                self.vm.transcript = Vec::new();
                self.vm.tasks = Vec::new();
                self.tasks_src = None;
                self.focused_id = String::new();
                self.tail = None;
                self.clamp_selections();
                return;
            }
        };

        let mut started_at_ms: HashMap<String, i64> = HashMap::new();
        let mut pid_by_id: HashMap<String, i32> = HashMap::new();
        let mut alive: HashSet<String> = HashSet::new();
        let now = || chrono::Utc::now();

        for r in &records {
            let paths = session_paths_for(&self.project, r);
            if let LeaseStatus::Alive { lease } = check_lease(Path::new(&paths.lease_path), &now) {
                alive.insert(r.id.clone());
                pid_by_id.insert(r.id.clone(), lease.pid);
                if let Some(ms) = lease
                    .started_at
                    .as_deref()
                    .and_then(|iso| chrono::DateTime::parse_from_rfc3339(iso).ok())
                    .map(|t| t.timestamp_millis())
                {
                    started_at_ms.insert(r.id.clone(), ms);
                }
            }
        }
        self.pid_by_id = pid_by_id;

        let classified = classify_sessions(&records, &|r: &SessionRecord| alive.contains(&r.id));
        self.running_sessions = classified.running;
        self.recent_sessions = classified.recent;
        self.vm.started_at_ms = started_at_ms;
        self.rebuild_sessions();

        // The Sessions pane opens in Running mode and stays there even when
        // nothing is running — an empty pane beats a pane that moves the
        // selection out from under the user.
        self.clamp_selections();
        self.sync_focused();
    }

    /// Rebuild the visible Sessions rows from the cached classified lists.
    fn rebuild_sessions(&mut self) {
        self.vm.sessions = visible_sessions(self.vm.mode, &self.running_sessions, &self.recent_sessions);
    }

    fn clamp_selections(&mut self) {
        self.vm.sel_session = clamp_sel(self.vm.sel_session as i64, self.vm.sessions.len());
        self.vm.sel_task = clamp_sel(self.vm.sel_task as i64, self.vm.tasks.len());
        self.vm.sel_shell = clamp_sel(self.vm.sel_shell as i64, self.vm.shells.len());
    }

    fn selected_record(&self) -> Option<SessionRecord> {
        self.vm.sessions.get(self.vm.sel_session).cloned()
    }

    /// Read `record`'s task ledger into the view model. Called on every list
    /// refresh, tail tick and selection change so a live ledger tracks the
    /// session; a missing, unreadable or invalid state file leaves it empty.
    fn load_tasks(&mut self, record: Option<&SessionRecord>) {
        // Reparsing a large state.json every tail tick is wasted work: skip
        // when the same file is unchanged since the last load. A missing file
        // (no mtime) clears the cache so its later appearance is picked up.
        let src = record.map(|r| {
            let state_path = session_paths_for(&self.project, r).state_path;
            let mtime = std::fs::metadata(&state_path).and_then(|m| m.modified()).ok();
            (state_path, mtime)
        });
        if let Some((path, Some(mtime))) = &src {
            if self.tasks_src.as_ref().is_some_and(|(p, m)| p == path && m == mtime) {
                return;
            }
        }
        let tasks = src
            .as_ref()
            .and_then(|(path, _)| crate::core::state::load_harness_state(Path::new(path)).ok().flatten())
            .map(|state| state.tasks)
            .unwrap_or_default();
        self.tasks_src = src.and_then(|(path, mtime)| mtime.map(|m| (path, m)));
        self.vm.tasks = tasks;
        self.vm.sel_task = clamp_sel(self.vm.sel_task as i64, self.vm.tasks.len());
    }

    fn sync_focused(&mut self) {
        let next = self.selected_record();
        let next_id = next.as_ref().map(|r| r.id.clone()).unwrap_or_default();
        if next_id != self.focused_id {
            self.focused_id = next_id;
            // A new session owns a new ledger: start at its first task.
            self.vm.sel_task = 0;
            self.switch_transcript(next.as_ref());
            // Don't show the previous session's processes while the next ps runs.
            self.vm.shells = Vec::new();
            self.refresh_shells();
        }
        // Reload even when the transcript did not move: a task-only ledger
        // change must still reach the pane.
        self.load_tasks(next.as_ref());
    }

    fn switch_transcript(&mut self, record: Option<&SessionRecord>) {
        self.vm.transcript = Vec::new();
        self.vm.following = true;
        self.vm.transcript_scroll = 0;
        self.tail = None;

        let Some(record) = record else { return };

        let paths = session_paths_for(&self.project, record);
        let mut tail = TranscriptTail::new(&paths.transcript_path, REPLAY_LIMIT);
        // Consume the replay batch immediately so the pane fills on focus.
        self.vm.transcript = trim_transcript(&tail.poll(), MAX_TRANSCRIPT);
        self.tail = Some(tail);
    }

    // ── input ────────────────────────────────────────────────────────────────

    fn setup_input(&mut self) {
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let mut stdin = std::io::stdin().lock();
            let mut buf = [0u8; 64];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(Msg::Key(String::from_utf8_lossy(&buf[..n]).into_owned())).is_err() {
                            break;
                        }
                    }
                }
            }
        });
    }

    fn setup_resize(&mut self) {
        // SAFETY: installing an async-signal-safe handler that only stores a flag.
        unsafe {
            libc::signal(libc::SIGWINCH, on_winch as *const () as libc::sighandler_t);
        }
    }

    fn setup_signals(&mut self) {
        // process.on("SIGINT"/"SIGTERM", halt): the handler only raises a
        // flag; the loop (which wakes at least every TAIL_MS) performs the stop.
        // SAFETY: installing an async-signal-safe handler that only stores a flag.
        unsafe {
            libc::signal(libc::SIGINT, on_halt as *const () as libc::sighandler_t);
            libc::signal(libc::SIGTERM, on_halt as *const () as libc::sighandler_t);
        }
    }

    fn on_key(&mut self, key: &str) {
        // Quit: q or Ctrl+C
        if key == "q" || key == "\x03" {
            self.stop();
            return;
        }

        // Focus panel 1 / 2 — the session selection (and the transcript it
        // owns) is untouched: the Sessions pane owns it whatever the focus is.
        if key == "1" {
            self.vm.focus = 1;
            self.draw();
            return;
        }
        if key == "2" {
            self.vm.focus = 2;
            self.draw();
            return;
        }

        // Focus the Shells pane — the [0] column becomes the drilled-in process
        // detail + stdout/stderr tail. The session focus (and its transcript,
        // which the shells belong to) is left untouched for the way back.
        if key == "3" {
            self.vm.focus = 3;
            self.sync_shell_log();
            self.draw();
            return;
        }

        // Tab cycles Sessions → Tasks → Shells
        if key == "\t" {
            self.vm.focus = match self.vm.focus {
                1 => 2,
                2 => 3,
                _ => 1,
            };
            if self.vm.focus == 3 {
                self.sync_shell_log();
            }
            self.draw();
            return;
        }

        // r cycles the Sessions filter: running → recent → all.
        if key == "r" {
            self.vm.mode = self.vm.mode.next();
            self.rebuild_sessions();
            self.vm.sel_session = clamp_sel(self.vm.sel_session as i64, self.vm.sessions.len());
            self.sync_focused();
            self.draw();
            return;
        }

        // Move selection: j/k or arrows
        if key == "j" || key == "\x1b[B" {
            self.mv(1);
            self.draw();
            return;
        }
        if key == "k" || key == "\x1b[A" {
            self.mv(-1);
            self.draw();
            return;
        }

        // Page / scroll: [ ] or h / l — scroll the transcript pane
        if key == "[" || key == "h" || key == "\x1b[D" {
            self.scroll_transcript(-PAGE);
            self.draw();
            return;
        }
        if key == "]" || key == "l" || key == "\x1b[C" {
            self.scroll_transcript(PAGE);
            self.draw();
        }
    }

    /// A stdin chunk is either a mouse report or ordinary key text.
    fn on_input(&mut self, chunk: &str) {
        match parse_sgr_mouse(chunk) {
            Some(scroll) => self.on_mouse_scroll(scroll),
            None => self.on_key(chunk),
        }
    }

    /// Wheel over the `[0]` pane scrolls it — older lines on wheel up, newer
    /// on wheel down — but only while the pointer hovers the pane, so a wheel
    /// over the list panes does not move the log ("hovering" behaviour).
    fn on_mouse_scroll(&mut self, scroll: crate::watch::mouse::MouseScroll) {
        let (cols, rows) = terminal_size();
        let Some(region) = transcript_region(&self.vm, cols, rows) else {
            return;
        };
        if !scroll.inside(region) {
            return;
        }
        self.scroll_transcript(scroll.delta(WHEEL_LINES));
        self.draw();
    }

    fn mv(&mut self, delta: i64) {
        if self.vm.focus == 1 {
            self.vm.sel_session = clamp_sel(self.vm.sel_session as i64 + delta, self.vm.sessions.len());
            self.sync_focused();
        } else if self.vm.focus == 2 {
            // Selecting a task only moves the highlight.
            self.vm.sel_task = clamp_sel(self.vm.sel_task as i64 + delta, self.vm.tasks.len());
        } else {
            self.vm.sel_shell = clamp_sel(self.vm.sel_shell as i64 + delta, self.vm.shells.len());
            self.sync_shell_log();
        }
    }

    // +delta scrolls toward newer lines (down), -delta toward older.
    fn scroll_transcript(&mut self, delta: i64) {
        let len = if self.vm.focus == 3 { self.vm.shell_log_lines.len() } else { self.vm.transcript.len() } as i64;
        let scroll = self.vm.transcript_scroll as i64;
        if delta < 0 {
            self.vm.following = false;
            self.vm.transcript_scroll = (scroll - delta).min((len - 1).max(0)) as usize;
        } else {
            self.vm.transcript_scroll = (scroll - delta).max(0) as usize;
            if self.vm.transcript_scroll == 0 {
                self.vm.following = true;
            }
        }
    }

    // ── paint ────────────────────────────────────────────────────────────────

    fn draw(&mut self) {
        if !self.running {
            return;
        }

        let (cols, rows) = terminal_size();
        // Keep now fresh so timer cells tick even between list refreshes when
        // something else (tail/key) triggers a repaint.
        self.vm.now = now_ms();

        let frame = render_frame(&self.vm, cols, rows);
        if frame == self.last_frame && !self.force_full {
            return;
        }

        let mut lines: Vec<String> = frame.split('\n').map(str::to_string).collect();
        // render_frame returns exactly `rows` lines joined by \n — no trailing blank.
        // Guard: if split yields rows+1 empty trailing, drop it.
        if lines.len() == rows + 1 && lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }

        if self.force_full || self.last_lines.len() != lines.len() {
            write_out(&format!("{}{}", term::HOME, lines.join("\n")));
            self.force_full = false;
        } else {
            let dirty = diff_lines(&self.last_lines, &lines);
            if dirty.is_empty() {
                self.last_frame = frame;
                return;
            }
            let mut out = String::new();
            for i in dirty {
                // move_to is 1-based row/col
                out.push_str(&term::move_to(i + 1, 1));
                out.push_str(&lines[i]);
            }
            write_out(&out);
        }

        self.last_frame = frame;
        self.last_lines = lines;
    }
}

/// Entry point used by bin/dripw — blocks until the user quits.
pub fn run_watch_app(project: DripProject, watch_dir: String) {
    let mut app = WatchApp::new(project, watch_dir);
    app.start();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str) -> SessionRecord {
        SessionRecord {
            sessions_dir: None,
            created_at: "2026-01-01T00:00:00.000Z".into(),
            cwd: "/r".into(),
            goal_count: 1,
            id: id.into(),
            last_goal: Some("a goal".into()),
            project_slug: "p".into(),
            status: "idle".into(),
            updated_at: "2026-01-01T00:00:00.000Z".into(),
        }
    }

    fn ids(list: &[SessionRecord]) -> Vec<String> {
        list.iter().map(|r| r.id.clone()).collect()
    }

    #[test]
    fn all_mode_concatenates_running_then_recent_in_order() {
        let running = vec![record("run-1"), record("run-2")];
        let recent = vec![record("old-1"), record("old-2")];

        assert_eq!(ids(&visible_sessions(SessionsMode::Running, &running, &recent)), vec!["run-1", "run-2"]);
        assert_eq!(ids(&visible_sessions(SessionsMode::Recent, &running, &recent)), vec!["old-1", "old-2"]);
        assert_eq!(
            ids(&visible_sessions(SessionsMode::All, &running, &recent)),
            vec!["run-1", "run-2", "old-1", "old-2"]
        );
        assert!(visible_sessions(SessionsMode::Running, &[], &recent).is_empty());
        assert_eq!(ids(&visible_sessions(SessionsMode::All, &[], &recent)), vec!["old-1", "old-2"]);
    }

    #[test]
    fn empty_vm_opens_on_running_with_empty_panes() {
        let vm = empty_vm(0);
        assert_eq!(vm.mode, SessionsMode::Running);
        assert!(vm.sessions.is_empty());
        assert!(vm.tasks.is_empty());
        assert_eq!(vm.sel_session, 0);
        assert_eq!(vm.sel_task, 0);
    }
}
