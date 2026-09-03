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
use std::time::{Duration, Instant};

use crate::core::home::{list_linked_worktree_roots, DripProject};
use crate::core::lease::{check_lease, LeaseStatus};
use crate::core::sessions::{
    has_any_worktree_session_index, list_all_sessions, open_session_index, session_paths_for, SessionIndex,
    SessionRecord,
};
use crate::watch::ansi::term;
use crate::watch::data::{classify_sessions, scope_sessions, trim_transcript, ScopeOptions, TranscriptTail};
use crate::watch::ps::{descendants, list_processes, PsProc};
use crate::watch::render::{diff_lines, render_frame, Scope, WatchViewModel};
use crate::watch::shelllog::{read_shell_log_files, seed_shell_log, LineFollower, SHELL_TAIL_BYTES};

const LIST_MS: u64 = 2000; // session-list refresh
const TAIL_MS: u64 = 300; // transcript tail poll
const REPLAY_LIMIT: usize = 100; // entries replayed when focusing a session
const PAGE: i64 = 8; // lines per [ ] / h / l scroll step
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
        running: Vec::new(),
        recent: Vec::new(),
        started_at_ms: HashMap::new(),
        focus: 1,
        session_focus: 1,
        sel_running: 0,
        sel_recent: 0,
        transcript: Vec::new(),
        following: true,
        transcript_scroll: 0,
        shells: Vec::new(),
        sel_shell: 0,
        shell_log_lines: Vec::new(),
        shell_log_files: Vec::new(),
        // Repo scope is the neutral default; the constructor narrows it to the
        // current worktree when the cwd sits inside one.
        scope: Scope::Repo,
        scope_label: "all worktrees".to_string(),
    }
}

fn basename(path: &str) -> String {
    Path::new(path).file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_else(|| path.to_string())
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

pub struct WatchApp {
    project: DripProject,
    index: Option<SessionIndex>,
    vm: WatchViewModel,
    running: bool,
    tail: Option<TranscriptTail>,
    focused_id: String,
    /// Lease pid per running session id, refreshed with the session list.
    pid_by_id: HashMap<String, i32>,
    ps_in_flight: bool,
    /// Pid whose stdout/stderr is currently seeded/followed (focus == 3).
    cur_shell_pid: i64,
    shell_followers: Vec<LineFollower>,
    auto_focused: bool,
    last_frame: String,
    last_lines: Vec<String>,
    force_full: bool, // first paint + resize
    stopping: bool,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
}

impl WatchApp {
    pub fn new(project: DripProject) -> Self {
        let (tx, rx) = mpsc::channel();
        let mut vm = empty_vm(now_ms());

        if let Some(worktree_root) = &project.worktree_root {
            vm.scope = Scope::Worktree;
            vm.scope_label = basename(worktree_root);
        }

        Self {
            project,
            index: None,
            vm,
            running: false,
            tail: None,
            focused_id: String::new(),
            pid_by_id: HashMap::new(),
            ps_in_flight: false,
            cur_shell_pid: 0,
            shell_followers: Vec::new(),
            auto_focused: false,
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

        write_out(&format!("{}{}{}", term::ALT_SCREEN, term::HIDE_CURSOR, term::CLEAR));

        let mut raw = RawMode::enable();

        // A panic anywhere (render, data, sqlite) must not strand the shell on
        // a cursor-less alt screen: leave the screen before the message prints.
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            write_out(&format!("{}{}", term::SHOW_CURSOR, term::MAIN_SCREEN));
            default_hook(info);
        }));

        self.setup_input();
        self.setup_resize();
        self.setup_signals();

        // Try the index immediately; if missing, refresh_sessions re-checks every 2s.
        self.try_open_index();
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
                Ok(Msg::Key(key)) => self.on_key(&key),
                Ok(Msg::Procs(procs)) => self.on_procs(procs),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => self.stop(),
            }
        }

        raw.restore();
        write_out(&format!("{}{}", term::SHOW_CURSOR, term::MAIN_SCREEN));

        self.index = None;
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

    // ── index lifecycle ──────────────────────────────────────────────────────

    fn try_open_index(&mut self) {
        if self.index.is_some() {
            return;
        }
        if !Path::new(&self.project.index_db_path).exists() {
            return;
        }
        // app.ts wraps openSessionIndex in try/catch; the Rust opener panics
        // on a broken file, so the catch is a catch_unwind.
        let path = self.project.index_db_path.clone();
        self.index = std::panic::catch_unwind(move || open_session_index(&path)).ok();
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
        self.try_open_index();
        self.vm.now = now_ms();

        // A worktree that never ran drip has no index of its own, but its siblings
        // may: Running/Recent union every worktree home of the repo, so only a repo
        // with no index anywhere is empty.
        if self.index.is_none() && !has_any_worktree_session_index(&self.project) {
            self.vm.running = Vec::new();
            self.vm.recent = Vec::new();
            self.vm.started_at_ms = HashMap::new();
            self.vm.shells = Vec::new();
            self.pid_by_id = HashMap::new();
            self.clamp_selections();
            return;
        }

        // The TS wraps the listing in try/catch (an index wiped mid-run):
        // drop the index and re-open next tick, showing empty lists meanwhile.
        let project = self.project.clone();
        let records: Vec<SessionRecord> = match std::panic::catch_unwind(move || list_all_sessions(&project, Some(200), true)) {
            Ok(records) => records,
            Err(_) => {
                self.index = None;
                self.vm.running = Vec::new();
                self.vm.recent = Vec::new();
                self.vm.started_at_ms = HashMap::new();
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
        self.vm.running = classified.running.clone();
        self.vm.recent = classified.recent.clone();
        self.vm.started_at_ms = started_at_ms;

        // In worktree scope the Recent list narrows to sessions started under the
        // checkout being watched; Running is never filtered, so a live run in
        // another worktree still shows. The linked roots are re-listed every tick
        // (one readdir) rather than cached — worktrees come and go.
        if self.vm.scope == Scope::Worktree {
            if let (Some(repo_root), Some(worktree_root)) = (&self.project.repo_root, &self.project.worktree_root) {
                let linked = list_linked_worktree_roots(repo_root);
                self.vm.recent =
                    scope_sessions(&self.vm.recent, ScopeOptions { worktree_root, linked_worktree_roots: &linked });
            }
        }

        // Opening with nothing running would focus an empty Running panel; start on
        // Recent instead so the bottom pane shows something immediately. Once only.
        if !self.auto_focused && (!classified.running.is_empty() || !classified.recent.is_empty()) {
            self.auto_focused = true;
            if classified.running.is_empty() {
                self.vm.focus = 2;
                self.vm.session_focus = 2;
            }
        }

        self.clamp_selections();
        self.sync_focused();
    }

    fn clamp_selections(&mut self) {
        self.vm.sel_running = clamp_sel(self.vm.sel_running as i64, self.vm.running.len());
        self.vm.sel_recent = clamp_sel(self.vm.sel_recent as i64, self.vm.recent.len());
        self.vm.sel_shell = clamp_sel(self.vm.sel_shell as i64, self.vm.shells.len());
    }

    fn selected_record(&self) -> Option<SessionRecord> {
        if self.vm.session_focus == 1 {
            return self.vm.running.get(self.vm.sel_running).cloned();
        }
        self.vm.recent.get(self.vm.sel_recent).cloned()
    }

    fn sync_focused(&mut self) {
        let next = self.selected_record();
        let next_id = next.as_ref().map(|r| r.id.clone()).unwrap_or_default();
        if next_id == self.focused_id {
            return;
        }
        self.focused_id = next_id;
        self.switch_transcript(next.as_ref());
        // Don't show the previous session's processes while the next ps runs.
        self.vm.shells = Vec::new();
        self.refresh_shells();
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
            libc::signal(libc::SIGWINCH, on_winch as libc::sighandler_t);
        }
    }

    fn setup_signals(&mut self) {
        // process.on("SIGINT"/"SIGTERM", halt): the handler only raises a
        // flag; the loop (which wakes at least every TAIL_MS) performs the stop.
        // SAFETY: installing an async-signal-safe handler that only stores a flag.
        unsafe {
            libc::signal(libc::SIGINT, on_halt as libc::sighandler_t);
            libc::signal(libc::SIGTERM, on_halt as libc::sighandler_t);
        }
    }

    fn on_key(&mut self, key: &str) {
        // Quit: q or Ctrl+C
        if key == "q" || key == "\x03" {
            self.stop();
            return;
        }

        // Focus panel 1 / 2
        if key == "1" {
            self.vm.focus = 1;
            self.vm.session_focus = 1;
            self.sync_focused();
            self.draw();
            return;
        }
        if key == "2" {
            self.vm.focus = 2;
            self.vm.session_focus = 2;
            self.sync_focused();
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

        // Tab cycles Running → Recent → Shells
        if key == "\t" {
            self.vm.focus = match self.vm.focus {
                1 => 2,
                2 => 3,
                _ => 1,
            };
            if self.vm.focus == 3 {
                self.sync_shell_log();
            } else {
                self.vm.session_focus = self.vm.focus;
                self.sync_focused();
            }
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
            return;
        }

        // Toggle Recent between this worktree and every worktree of the repo.
        // The selection resets (the list is about to change) and the refresh is
        // immediate so the toggle reads as instant rather than next tick.
        if key == "w" && self.project.worktree_root.is_some() {
            self.vm.scope = if self.vm.scope == Scope::Worktree { Scope::Repo } else { Scope::Worktree };
            self.vm.scope_label = match (&self.vm.scope, &self.project.worktree_root) {
                (Scope::Worktree, Some(root)) => basename(root),
                _ => "all worktrees".to_string(),
            };
            self.vm.sel_recent = 0;
            self.refresh_sessions();
            self.draw();
        }
    }

    fn mv(&mut self, delta: i64) {
        if self.vm.focus == 1 {
            self.vm.sel_running = clamp_sel(self.vm.sel_running as i64 + delta, self.vm.running.len());
        } else if self.vm.focus == 2 {
            self.vm.sel_recent = clamp_sel(self.vm.sel_recent as i64 + delta, self.vm.recent.len());
        } else {
            self.vm.sel_shell = clamp_sel(self.vm.sel_shell as i64 + delta, self.vm.shells.len());
            self.sync_shell_log();
            return;
        }
        self.sync_focused();
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
pub fn run_watch_app(project: DripProject) {
    let mut app = WatchApp::new(project);
    app.start();
}
