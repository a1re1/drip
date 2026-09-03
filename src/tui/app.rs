// Ink's model was: a <Static> timeline that scrolls into the terminal's
// scrollback, and a live region (composer or picker, then the status bar)
// re-painted in place at the bottom. This port keeps exactly that shape with
// raw ANSI: timeline cells are printed once, above the live region; the live
// region is erased (cursor-up + clear-to-end) and redrawn on every state
// change; frames are wrapped in synchronized-output markers so they never
// tear. A width-changing resize clears the screen and re-emits the most
// recent screenful of the timeline (repaint tail), like the StableTerminal.
//
// Threads: stdin reader → Msg::Input; a goal run (own tokio runtime) →
// Msg::Event / Msg::RunDone; a mention indexer → Msg::Mentions; marketplace
// clones → Msg::Info / Msg::Error. The main loop owns all state.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::cli::commands::{get_slash_command_suggestions, parse_slash_command, SlashCommandSpec, SLASH_COMMANDS};
use crate::cli::file_suggestions::{get_workspace_file_suggestions, WorkspaceFileSource, DEFAULT_FILE_SUGGESTION_LIMIT};
use crate::cli::images::{
    attachment_from_data_url, attachment_from_image_file, capture_clipboard_image, looks_like_image_paste, GoalImageAttachment,
};
use crate::cli::marketplaces::{
    add_marketplace, discover_all_skills, is_marketplace_key_enabled, list_enabled_marketplace_roles, list_marketplace_plugins,
    load_marketplaces_file, load_project_plugin_overrides, remove_marketplace, set_marketplace_key_enabled, update_marketplaces,
    AddMarketplaceArgs,
};
use crate::cli::mentions::{get_active_chat_file_mention, replace_active_chat_file_mention, resolve_goal_mentions};
use crate::cli::paste::{sanitize_pasted_input, DISABLE_BRACKETED_PASTE, ENABLE_BRACKETED_PASTE};
use crate::cli::roles::{load_skill_content, resolve_role_setup, ResolveRoleSetupArgs};
use crate::cli::session_run::{run_session_goal, SessionGoalArgs, SessionGoalError, SessionGoalOutcome};
use crate::cli::skills::LoadedCliSkill;
use crate::cli::state_summary::format_state_summary;
use crate::cli::transcript::{
    append_transcript_entry, read_transcript, TranscriptEntry, TranscriptEventEntry, TranscriptGoalEntry, TranscriptNoteEntry,
    TranscriptRunEndEntry, TranscriptSkillEntry,
};
use crate::core::config::{
    get_active_cli_profile_id, get_active_cli_tool_profile_id, list_cli_model_profiles, list_cli_system_prompt_profiles,
    resolve_cli_inference, save_cli_config, set_active_cli_profile, set_active_cli_system_prompt, set_active_cli_tool_profile,
    CliConfig,
};
use crate::core::env_vars::{load_env_vars, load_merged_env, lookup_env_var_source, upsert_env_var};
use crate::core::home::{DripHome, DripProject};
use crate::core::sessions::{
    create_session, list_all_sessions, open_session_index, resolve_any_session_ref, session_paths_for, CreateSessionArgs,
    ProjectPaths, SessionPaths, SessionRecord,
};
use crate::core::types::HarnessEvent;
use crate::harness::model_call::AbortSignal;
use crate::tools::pack::builtin_tool_pack;
use crate::tui::term::{terminal_size, write_out, RawMode};
use crate::tui::timeline::{render_timeline_cell, select_repaint_tail_start};
use crate::tui::widgets::{render_composer, render_picker, render_status_bar, ComposerProps, PickerItem, StatusBarProps};
use crate::watch::ansi::{string_width, wrap_ansi};

/// What `drip --tui` needs from entry.rs to start.
pub struct TuiBootstrap {
    pub allow_net: bool,
    pub config: CliConfig,
    pub cwd: String,
    pub home: DripHome,
    pub initial_goal: Option<String>,
    pub max_iterations: Option<i64>,
    pub no_repo_memory: bool,
    pub project: DripProject,
    pub session: SessionRecord,
}

// Harness events can arrive far faster than the terminal can usefully paint;
// they queue briefly and land in a single repaint.
const EVENT_BATCH_MS: u64 = 48;
const RESIZE_SETTLE_MS: u64 = 150;

const SYNC_UPDATE_START: &str = "\u{1b}[?2026h";
const SYNC_UPDATE_END: &str = "\u{1b}[?2026l";
const CLEAR_VISIBLE_SCREEN: &str = "\u{1b}[2J\u{1b}[H";
const CLEAR_TO_END: &str = "\u{1b}[J";
const HIDE_CURSOR: &str = "\u{1b}[?25l";
const SHOW_CURSOR: &str = "\u{1b}[?25h";

static WINCH: AtomicBool = AtomicBool::new(false);
static HALT: AtomicBool = AtomicBool::new(false);

extern "C" fn on_winch(_: libc::c_int) {
    WINCH.store(true, Ordering::SeqCst);
}

extern "C" fn on_halt(_: libc::c_int) {
    HALT.store(true, Ordering::SeqCst);
}

fn help_text() -> String {
    let mut lines = vec!["commands:".to_string()];
    for command in SLASH_COMMANDS {
        lines.push(format!(
            "  /{}{} — {}",
            command.name,
            command.args.map(|args| format!(" {args}")).unwrap_or_default(),
            command.description
        ));
    }
    lines.extend(
        [
            "",
            "composer:",
            "  @path or @path#12:40 — inline a file (or directory tree) into the goal",
            "  ctrl+v — attach the clipboard image; pasting an image path or data URL also attaches",
            "  esc — clear the composer, or stop the running goal",
            "  ctrl+c — exit",
        ]
        .iter()
        .map(|line| line.to_string()),
    );
    lines.join("\n")
}

/// Cuts a painted row to `width` columns without dropping its SGR codes.
fn clip_ansi(row: &str, width: usize) -> String {
    if string_width(row) <= width {
        return row.to_string();
    }
    wrap_ansi(row, width).into_iter().next().unwrap_or_default()
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OverlayKind {
    Model,
    Prompt,
    Sessions,
    ToolModel,
}

struct Overlay {
    items: Vec<PickerItem>,
    kind: OverlayKind,
    selected: usize,
    title: &'static str,
}

enum Msg {
    Error(String),
    Event(HarnessEvent),
    Info(String),
    Input(Vec<u8>),
    Mentions { paths: Vec<String>, seq: u64 },
    RunDone(Result<SessionGoalOutcome, SessionGoalError>),
}

/// One decoded terminal input.
enum Key {
    Backspace,
    Ctrl(char),
    Delete,
    Down,
    Escape,
    Left,
    Paste(String),
    Return,
    Right,
    Tab,
    Text(String),
    Up,
    Ignored,
}

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

/// Splits a raw stdin chunk into keys. Bracketed pastes may span chunks, so
/// the caller keeps `paste_buffer` between calls.
fn decode_input(chunk: &[u8], paste_buffer: &mut Option<Vec<u8>>) -> Vec<Key> {
    let mut keys = Vec::new();
    let mut rest = chunk;

    loop {
        if let Some(buffer) = paste_buffer.as_mut() {
            if let Some(end) = find(rest, PASTE_END) {
                buffer.extend_from_slice(&rest[..end]);
                let pasted = String::from_utf8_lossy(buffer).into_owned();
                *paste_buffer = None;
                keys.push(Key::Paste(pasted));
                rest = &rest[end + PASTE_END.len()..];
                continue;
            }
            buffer.extend_from_slice(rest);
            return keys;
        }

        if rest.is_empty() {
            return keys;
        }

        if let Some(start) = find(rest, PASTE_START) {
            if start > 0 {
                keys.extend(decode_plain(&rest[..start]));
            }
            *paste_buffer = Some(Vec::new());
            rest = &rest[start + PASTE_START.len()..];
            continue;
        }

        keys.extend(decode_plain(rest));
        return keys;
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

// Ink's useInput semantics: a single byte is a key; an escape sequence is an
// arrow/delete/meta key; any longer plain chunk is delivered as one "paste"
// (fast typing and pasted commands both arrive that way).
fn decode_plain(chunk: &[u8]) -> Vec<Key> {
    if chunk.is_empty() {
        return Vec::new();
    }

    if chunk[0] == 0x1b {
        if chunk.len() == 1 {
            return vec![Key::Escape];
        }
        // Consume one CSI (ESC [ params final) or SS3 (ESC O x) sequence, then
        // decode whatever followed it in the same read (key repeat, a typed
        // character right after an arrow).
        let end = match chunk.get(1) {
            Some(b'[') => chunk
                .iter()
                .enumerate()
                .skip(2)
                .find(|(_, byte)| (0x40..=0x7e).contains(*byte))
                .map(|(index, _)| index + 1)
                .unwrap_or(chunk.len()),
            Some(b'O') => 3.min(chunk.len()),
            _ => 2,
        };
        let (sequence, rest) = chunk.split_at(end);
        let key = match sequence {
            b"\x1b[A" | b"\x1bOA" => Key::Up,
            b"\x1b[B" | b"\x1bOB" => Key::Down,
            b"\x1b[C" | b"\x1bOC" => Key::Right,
            b"\x1b[D" | b"\x1bOD" => Key::Left,
            b"\x1b[3~" => Key::Delete,
            _ => Key::Ignored,
        };
        let mut keys = vec![key];
        keys.extend(decode_plain(rest));
        return keys;
    }

    if chunk.len() == 1 {
        return vec![match chunk[0] {
            b'\r' | b'\n' => Key::Return,
            b'\t' => Key::Tab,
            0x7f | 0x08 => Key::Backspace,
            byte @ 0x01..=0x1a => Key::Ctrl((b'a' + byte - 1) as char),
            byte if byte < 0x20 => Key::Ignored,
            _ => Key::Text(String::from_utf8_lossy(chunk).into_owned()),
        }];
    }

    let text = String::from_utf8_lossy(chunk).into_owned();
    if text.chars().count() == 1 {
        return vec![Key::Text(text)];
    }
    vec![Key::Paste(text)]
}

struct TuiApp {
    abort: Option<AbortSignal>,
    active_skills: Vec<LoadedCliSkill>,
    attachments: Vec<GoalImageAttachment>,
    bootstrap: TuiBootstrap,
    /// Every timeline cell rendered so far (for the repaint tail after a resize).
    cells: Vec<TranscriptEntry>,
    cols: usize,
    config: CliConfig,
    cursor: usize,
    exit_code: i32,
    flush_deadline: Option<Instant>,
    /// Rows the live region occupied at the last paint.
    live_rows: usize,
    mention_seq: u64,
    mention_suggestions: Vec<String>,
    mention_tx: Sender<(u64, String)>,
    overlay: Option<Overlay>,
    paste_buffer: Option<Vec<u8>>,
    paths: SessionPaths,
    pending_cells: Vec<TranscriptEntry>,
    pending_detail: Option<String>,
    quit: bool,
    resize_at: Option<Instant>,
    rows: usize,
    running: bool,
    running_detail: Option<String>,
    selected_suggestion_index: usize,
    session: SessionRecord,
    text: String,
    tx: Sender<Msg>,
}

impl TuiApp {
    fn new(bootstrap: TuiBootstrap, tx: Sender<Msg>, mention_tx: Sender<(u64, String)>) -> Self {
        let paths = session_paths_for(&bootstrap.project, &bootstrap.session);
        let cells = read_transcript(Path::new(&paths.transcript_path));
        let (cols, rows) = terminal_size();
        let config = bootstrap.config.clone();
        let session = bootstrap.session.clone();

        Self {
            abort: None,
            active_skills: Vec::new(),
            attachments: Vec::new(),
            bootstrap,
            cells,
            cols,
            config,
            cursor: 0,
            exit_code: 0,
            flush_deadline: None,
            live_rows: 0,
            mention_seq: 0,
            mention_suggestions: Vec::new(),
            mention_tx,
            overlay: None,
            paste_buffer: None,
            paths,
            pending_cells: Vec::new(),
            pending_detail: None,
            quit: false,
            resize_at: None,
            rows,
            running: false,
            running_detail: None,
            selected_suggestion_index: 0,
            session,
            text: String::new(),
            tx,
        }
    }

    // ----- timeline -------------------------------------------------------

    /// Prints cells above the live region (Ink's <Static>).
    fn emit_static(&mut self, entries: Vec<TranscriptEntry>) {
        if entries.is_empty() {
            return;
        }
        let mut out = String::new();
        for entry in &entries {
            for row in render_timeline_cell(entry, self.cols) {
                out.push_str(&row);
                out.push('\n');
            }
        }
        self.cells.extend(entries);
        self.paint(&out);
    }

    fn push_cell(&mut self, entry: TranscriptEntry, persist: bool) {
        if persist {
            let _ = append_transcript_entry(Path::new(&self.paths.transcript_path), &entry);
        }
        self.pending_cells.push(entry);
        self.flush_pending_cells();
    }

    /// Persists immediately (durability) but defers the paint so a burst of
    /// harness events becomes one frame instead of one per event.
    fn queue_cell(&mut self, entry: TranscriptEntry) {
        self.pending_cells.push(entry);
        if self.flush_deadline.is_none() {
            self.flush_deadline = Some(Instant::now() + Duration::from_millis(EVENT_BATCH_MS));
        }
    }

    fn flush_pending_cells(&mut self) {
        self.flush_deadline = None;
        if let Some(detail) = self.pending_detail.take() {
            self.running_detail = Some(detail);
        }
        let pending = std::mem::take(&mut self.pending_cells);
        self.emit_static(pending);
    }

    fn push_info(&mut self, text: impl Into<String>) {
        self.push_cell(TranscriptEntry::Info(TranscriptNoteEntry { at: now_iso(), text: text.into() }), true);
    }

    fn push_error(&mut self, text: impl Into<String>) {
        self.push_cell(TranscriptEntry::Error(TranscriptNoteEntry { at: now_iso(), text: text.into() }), true);
    }

    // ----- painting -------------------------------------------------------

    fn live_region(&self) -> Vec<String> {
        let mut rows = vec![String::new()]; // marginTop 1

        if let Some(overlay) = &self.overlay {
            rows.extend(render_picker(overlay.title, &overlay.items, overlay.selected, self.cols));
        } else {
            let slash: Vec<&SlashCommandSpec> = get_slash_command_suggestions(&self.text);
            rows.extend(render_composer(
                &ComposerProps {
                    attachments: &self.attachments,
                    cursor: self.cursor,
                    disabled: self.running,
                    mention_suggestions: &self.mention_suggestions,
                    selected_suggestion_index: self.selected_suggestion_index,
                    slash_suggestions: &slash,
                    text: &self.text,
                },
                self.cols,
            ));
        }

        let skill_names: Vec<String> = self.active_skills.iter().map(|skill| skill.name.clone()).collect();
        let before_status = rows.len();
        rows.extend(render_status_bar(
            &StatusBarProps {
                active_skill_names: &skill_names,
                cwd: &self.bootstrap.cwd,
                model_label: &self.model_label(),
                running: self.running,
                running_detail: self.running_detail.as_deref(),
                session_id: &self.session.id,
            },
            self.cols,
        ));
        let status_rows = rows.len() - before_status;

        // Live rows must never wrap: the cursor-up arithmetic that erases the
        // previous frame counts logical rows. Clipping keeps SGR codes (the
        // cursor block, menu highlights, borders) — only the text is cut.
        let width = self.cols.max(1);
        let mut rows: Vec<String> = rows.into_iter().map(|row| clip_ansi(&row, width)).collect();

        // A terminal shorter than the frame drops rows above the status bar
        // (menus, overlay tail) rather than the status bar itself.
        let max_rows = self.rows.max(1);
        if rows.len() > max_rows {
            let excess = rows.len() - max_rows;
            let keep_tail = status_rows.min(rows.len());
            let cut_at = rows.len() - keep_tail;
            rows.drain(cut_at - excess..cut_at);
        }
        rows
    }

    /// Erases the previous live region, prints `static_rows` (already
    /// newline-terminated) in its place, then the fresh live region.
    fn paint(&mut self, static_rows: &str) {
        let live = self.live_region();
        let mut frame = String::from(SYNC_UPDATE_START);
        frame.push('\r');
        if self.live_rows > 1 {
            frame.push_str(&format!("\u{1b}[{}A", self.live_rows - 1));
        }
        frame.push_str(CLEAR_TO_END);
        frame.push_str(static_rows);
        frame.push_str(&live.join("\n"));
        frame.push_str(SYNC_UPDATE_END);
        self.live_rows = live.len();
        write_out(&frame);
    }

    fn repaint(&mut self) {
        self.paint("");
    }

    /// After a settle-repaint clears the screen, only the most recent cells
    /// that fit above the live region are re-emitted.
    fn full_repaint(&mut self) {
        let start = select_repaint_tail_start(&self.cells, self.rows);
        let mut out = String::from(CLEAR_VISIBLE_SCREEN);
        for entry in &self.cells[start..] {
            for row in render_timeline_cell(entry, self.cols) {
                out.push_str(&row);
                out.push('\n');
            }
        }
        self.live_rows = 0;
        self.paint(&out);
    }

    fn model_label(&self) -> String {
        let settings = &self.config.settings;
        let active_id = get_active_cli_profile_id(settings);
        let profiles = match list_cli_model_profiles(settings) {
            Ok(profiles) => profiles,
            Err(_) => return "invalid profile config".to_string(),
        };
        let base_label = match profiles.iter().find(|candidate| candidate.id == active_id) {
            Some(profile) => format!("{} ({})", profile.label.clone().unwrap_or_else(|| profile.id.clone()), profile.model),
            None if active_id.is_empty() => "no profile".to_string(),
            None => active_id.clone(),
        };
        let tool_profile_id = get_active_cli_tool_profile_id(settings);
        let tool_profile = if !tool_profile_id.is_empty() && tool_profile_id != active_id {
            profiles.iter().find(|candidate| candidate.id == tool_profile_id)
        } else {
            None
        };
        match tool_profile {
            Some(profile) => format!("{base_label} · tools: {}", profile.model),
            None => base_label,
        }
    }

    // ----- composer -------------------------------------------------------

    fn apply_edit(&mut self, next_text: String, next_cursor: usize) {
        let len = next_text.chars().count();
        let text_changed = next_text != self.text;
        self.text = next_text;
        self.cursor = next_cursor.min(len);
        if text_changed {
            self.selected_suggestion_index = 0;
        }
        self.refresh_mentions();
    }

    fn insert_text(&mut self, insertion: &str) {
        let chars: Vec<char> = self.text.chars().collect();
        let cursor = self.cursor.min(chars.len());
        let mut next: String = chars[..cursor].iter().collect();
        next.push_str(insertion);
        next.extend(chars[cursor..].iter());
        let next_cursor = cursor + insertion.chars().count();
        self.apply_edit(next, next_cursor);
    }

    /// Mention autocomplete tracks the token under the cursor; the index walk
    /// happens off-thread and answers by sequence number so stale results are
    /// dropped.
    fn refresh_mentions(&mut self) {
        match get_active_chat_file_mention(&self.text, self.cursor) {
            Some(mention) => {
                self.mention_seq += 1;
                let _ = self.mention_tx.send((self.mention_seq, mention.path_text));
            }
            None => {
                // Bump the sequence so an in-flight walk cannot re-open the menu.
                self.mention_seq += 1;
                self.mention_suggestions.clear();
            }
        }
    }

    fn accept_suggestion(&mut self) -> bool {
        let slash = get_slash_command_suggestions(&self.text);
        if !slash.is_empty() {
            let command = slash[self.selected_suggestion_index.min(slash.len() - 1)];
            let next_text = format!("/{} ", command.name);
            let len = next_text.chars().count();
            self.apply_edit(next_text, len);
            return true;
        }

        if let Some(mention) = get_active_chat_file_mention(&self.text, self.cursor) {
            if !self.mention_suggestions.is_empty() {
                let path = self.mention_suggestions[self.selected_suggestion_index.min(self.mention_suggestions.len() - 1)].clone();
                let next = replace_active_chat_file_mention(&self.text, &mention, &path);
                let next_cursor = mention.path_start + path.chars().count();
                self.apply_edit(next, next_cursor);
                return true;
            }
        }

        false
    }

    fn submit(&mut self) {
        let submitted = self.text.trim().to_string();
        if submitted.is_empty() && self.attachments.is_empty() {
            return;
        }

        self.apply_edit(String::new(), 0);

        if let Some(command) = parse_slash_command(&submitted) {
            self.dispatch_command(&command.name, &command.args);
            return;
        }

        let goal = if submitted.is_empty() {
            "Describe the attached image(s) in the context of this workspace.".to_string()
        } else {
            submitted
        };
        self.run_goal(goal);
    }

    // ----- keys -----------------------------------------------------------

    fn on_key(&mut self, key: Key) {
        if let Some(overlay) = self.overlay.as_mut() {
            match key {
                Key::Escape => self.overlay = None,
                Key::Return => {
                    let selected = overlay.items.get(overlay.selected).cloned();
                    let kind = overlay.kind;
                    self.overlay = None;
                    match selected {
                        Some(item) => self.on_pick(kind, item),
                        None => {}
                    }
                }
                Key::Up => overlay.selected = overlay.selected.saturating_sub(1),
                Key::Down | Key::Tab => {
                    overlay.selected = (overlay.selected + 1).min(overlay.items.len().saturating_sub(1));
                }
                Key::Ctrl('c') => self.quit = true,
                _ => {}
            }
            return;
        }

        if self.running {
            match key {
                Key::Escape => {
                    if let Some(abort) = &self.abort {
                        abort.abort();
                    }
                }
                Key::Ctrl('c') => self.quit = true,
                _ => {}
            }
            return;
        }

        let slash_len = get_slash_command_suggestions(&self.text).len();
        let menu_length = if slash_len > 0 { slash_len } else { self.mention_suggestions.len() };

        match key {
            Key::Escape => self.apply_edit(String::new(), 0),
            Key::Return => {
                // Enter accepts an open menu selection unless the text already matches it exactly.
                if menu_length > 0 {
                    let slash = get_slash_command_suggestions(&self.text);
                    let exact_slash = slash.len() == 1 && format!("/{}", slash[0].name) == self.text.trim();
                    if !exact_slash && self.accept_suggestion() {
                        return;
                    }
                }
                self.submit();
            }
            Key::Tab => {
                self.accept_suggestion();
            }
            Key::Up => {
                if menu_length > 0 {
                    self.selected_suggestion_index = self.selected_suggestion_index.saturating_sub(1);
                }
            }
            Key::Down => {
                if menu_length > 0 {
                    self.selected_suggestion_index = (self.selected_suggestion_index + 1).min(menu_length - 1);
                }
            }
            Key::Left => {
                let text = self.text.clone();
                let cursor = self.cursor.saturating_sub(1);
                self.apply_edit(text, cursor);
            }
            Key::Right => {
                let text = self.text.clone();
                let cursor = self.cursor + 1;
                self.apply_edit(text, cursor);
            }
            Key::Ctrl('a') => {
                let text = self.text.clone();
                self.apply_edit(text, 0);
            }
            Key::Ctrl('e') => {
                let text = self.text.clone();
                let len = text.chars().count();
                self.apply_edit(text, len);
            }
            Key::Ctrl('u') => self.apply_edit(String::new(), 0),
            Key::Ctrl('c') => self.quit = true,
            Key::Ctrl('v') => match capture_clipboard_image(Path::new(&self.paths.images_dir)) {
                Some(attachment) => self.attachments.push(attachment),
                None => self.push_info("no image found on the clipboard."),
            },
            Key::Backspace | Key::Delete => {
                let chars: Vec<char> = self.text.chars().collect();
                let cursor = self.cursor.min(chars.len());
                if cursor > 0 {
                    let mut next: String = chars[..cursor - 1].iter().collect();
                    next.extend(chars[cursor..].iter());
                    self.apply_edit(next, cursor - 1);
                } else if chars.is_empty() && !self.attachments.is_empty() {
                    self.attachments.pop();
                }
            }
            Key::Paste(raw) => self.on_paste(&raw),
            Key::Text(text) => self.insert_text(&text),
            Key::Ctrl(_) | Key::Ignored => {}
        }
    }

    // Multi-character input is a paste; pasted image payloads become
    // attachments. A chunk ending in a newline (fast typing or a pasted
    // command) submits; interior newlines stay literal so multi-line goals
    // paste cleanly.
    fn on_paste(&mut self, raw: &str) {
        let pasted = sanitize_pasted_input(raw);
        if pasted.is_empty() {
            return;
        }

        match looks_like_image_paste(&pasted) {
            Some("data-url") => {
                if let Some(attachment) = attachment_from_data_url(&pasted, Path::new(&self.paths.images_dir)) {
                    self.attachments.push(attachment);
                    return;
                }
            }
            Some("file-path") => {
                if let Some(attachment) =
                    attachment_from_image_file(pasted.trim(), (&self.bootstrap.cwd, Path::new(&self.paths.images_dir)))
                {
                    self.attachments.push(attachment);
                    return;
                }
            }
            _ => {}
        }

        let normalized = pasted.replace("\r\n", "\n").replace('\r', "\n");
        let submits_on_newline = normalized.ends_with('\n');
        let insertion = if submits_on_newline { &normalized[..normalized.len() - 1] } else { normalized.as_str() };
        if !insertion.is_empty() {
            self.insert_text(insertion);
        }
        if submits_on_newline {
            self.submit();
        }
    }

    // ----- overlays -------------------------------------------------------

    fn open_overlay(&mut self, kind: OverlayKind) {
        let settings = &self.config.settings;
        let (title, items): (&'static str, Vec<PickerItem>) = match kind {
            OverlayKind::Model => (
                "model profiles",
                list_cli_model_profiles(settings)
                    .map(|profiles| {
                        let active = get_active_cli_profile_id(settings);
                        profiles
                            .into_iter()
                            .map(|profile| PickerItem {
                                detail: Some(format!("{} · {}", profile.provider, profile.model)),
                                label: format!(
                                    "{}{}",
                                    if profile.id == active { "● " } else { "" },
                                    profile.label.clone().unwrap_or_else(|| profile.id.clone())
                                ),
                                id: profile.id,
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            ),
            OverlayKind::ToolModel => (
                "tool-calling model profiles",
                list_cli_model_profiles(settings)
                    .map(|profiles| {
                        let active_tool_id = get_active_cli_tool_profile_id(settings);
                        let mut items = vec![PickerItem {
                            detail: Some("route every request through the active model".to_string()),
                            id: String::new(),
                            label: format!("{}no split — use the active model", if active_tool_id.is_empty() { "● " } else { "" }),
                        }];
                        items.extend(profiles.into_iter().map(|profile| PickerItem {
                            detail: Some(format!("{} · {}", profile.provider, profile.model)),
                            label: format!(
                                "{}{}",
                                if profile.id == active_tool_id { "● " } else { "" },
                                profile.label.clone().unwrap_or_else(|| profile.id.clone())
                            ),
                            id: profile.id,
                        }));
                        items
                    })
                    .unwrap_or_default(),
            ),
            OverlayKind::Prompt => (
                "system prompt profiles",
                list_cli_system_prompt_profiles(settings)
                    .map(|profiles| {
                        profiles
                            .into_iter()
                            .map(|profile| PickerItem {
                                detail: None,
                                label: profile.label.clone().unwrap_or_else(|| profile.id.clone()),
                                id: profile.id,
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            ),
            OverlayKind::Sessions => (
                "resume a session",
                list_all_sessions(&self.bootstrap.project, Some(15), false)
                    .into_iter()
                    .map(|record| PickerItem {
                        detail: Some(record.last_goal.clone().unwrap_or_else(|| "(no goal yet)".to_string())),
                        label: format!(
                            "{}{} · {} · {} goal(s)",
                            if record.id == self.session.id { "● " } else { "" },
                            short_id(&record.id),
                            record.status,
                            record.goal_count
                        ),
                        id: record.id,
                    })
                    .collect(),
            ),
        };
        self.overlay = Some(Overlay { items, kind, selected: 0, title });
    }

    fn on_pick(&mut self, kind: OverlayKind, item: PickerItem) {
        match kind {
            OverlayKind::Model => match set_active_cli_profile(self.config.clone(), &item.id) {
                Ok(next) => self.save_config(next, format!("model profile set to {}", item.id)),
                Err(error) => self.push_error(error.to_string()),
            },
            OverlayKind::ToolModel => match set_active_cli_tool_profile(self.config.clone(), &item.id) {
                Ok(next) => {
                    let note = if item.id.is_empty() {
                        "tool-calling model cleared — every request uses the active model".to_string()
                    } else {
                        format!("tool-calling model set to {}", item.id)
                    };
                    self.save_config(next, note);
                }
                Err(error) => self.push_error(error.to_string()),
            },
            OverlayKind::Prompt => match set_active_cli_system_prompt(self.config.clone(), &item.id) {
                Ok(next) => self.save_config(next, format!("system prompt set to {}", item.id)),
                Err(error) => self.push_error(error.to_string()),
            },
            OverlayKind::Sessions => {
                if let Some(record) = resolve_any_session_ref(&self.bootstrap.project, Some(&item.id)) {
                    self.switch_session(record);
                }
            }
        }
    }

    fn save_config(&mut self, next: CliConfig, note: String) {
        match save_cli_config(Path::new(&self.bootstrap.home.config_path), &next) {
            Ok(()) => {
                self.config = next;
                self.push_info(note);
            }
            Err(error) => self.push_error(error.to_string()),
        }
    }

    // ----- sessions -------------------------------------------------------

    fn switch_session(&mut self, record: SessionRecord) {
        self.flush_pending_cells();
        self.paths = session_paths_for(&self.bootstrap.project, &record);
        self.session = record;
        // Replay the new transcript from the top, like remounting <Static>:
        // the previous session's rows stay in scrollback (ink cannot take
        // static output back) and the new transcript is printed below them.
        self.cells = read_transcript(Path::new(&self.paths.transcript_path));
        let mut out = String::new();
        for entry in &self.cells {
            for row in render_timeline_cell(entry, self.cols) {
                out.push_str(&row);
                out.push('\n');
            }
        }
        self.paint(&out);
    }

    // ----- skills ---------------------------------------------------------

    fn toggle_skill(&mut self, skill_name: &str) {
        if self.active_skills.iter().any(|skill| skill.name == skill_name) {
            self.active_skills.retain(|skill| skill.name != skill_name);
            self.push_cell(
                TranscriptEntry::Skill(TranscriptSkillEntry { at: now_iso(), enabled: false, name: skill_name.to_string() }),
                true,
            );
            return;
        }

        let discovered = discover_all_skills(Path::new(&self.bootstrap.cwd), &self.bootstrap.home).unwrap_or_default();
        let Some(skill) = discovered.into_iter().find(|candidate| candidate.name == skill_name) else {
            self.push_error(format!("No skill named \"{skill_name}\". Try /skills to list what is available."));
            return;
        };

        match load_skill_content(&skill, None) {
            Ok(loaded) => {
                self.active_skills.push(loaded);
                self.push_cell(
                    TranscriptEntry::Skill(TranscriptSkillEntry { at: now_iso(), enabled: true, name: skill_name.to_string() }),
                    true,
                );
            }
            Err(error) => self.push_error(format!("Could not load skill \"{skill_name}\": {error}")),
        }
    }

    // ----- commands -------------------------------------------------------

    fn dispatch_command(&mut self, name: &str, args: &str) {
        match name {
            "help" => self.push_info(help_text()),
            "quit" | "exit" => self.quit = true,
            "model" => self.open_overlay(OverlayKind::Model),
            "toolmodel" => self.open_overlay(OverlayKind::ToolModel),
            "prompt" => self.open_overlay(OverlayKind::Prompt),
            "new" => {
                let index = open_session_index(&self.bootstrap.project.index_db_path);
                let record = create_session(
                    &index,
                    CreateSessionArgs {
                        cwd: self.bootstrap.cwd.clone(),
                        project: &ProjectPaths::from(&self.bootstrap.project),
                        now: "",
                    },
                );
                index.close();
                let id = record.id.clone();
                self.switch_session(record);
                self.push_info(format!("started session {id}"));
            }
            "resume" => {
                if !args.is_empty() {
                    match resolve_any_session_ref(&self.bootstrap.project, Some(args)) {
                        Some(record) => self.switch_session(record),
                        None => self.push_error(format!("No session found matching \"{args}\".")),
                    }
                } else {
                    self.open_overlay(OverlayKind::Sessions);
                }
            }
            "sessions" => {
                let records = list_all_sessions(&self.bootstrap.project, Some(15), false);
                let text = if records.is_empty() {
                    "no sessions recorded for this directory yet.".to_string()
                } else {
                    records
                        .iter()
                        .map(|record| {
                            format!(
                                "{} {} · {} · {} goal(s) · {}",
                                if record.id == self.session.id { "▸" } else { " " },
                                short_id(&record.id),
                                record.updated_at,
                                record.goal_count,
                                record.last_goal.clone().unwrap_or_else(|| "(no goal yet)".to_string())
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                self.push_info(text);
            }
            "state" => {
                // A corrupt state file comes back as the loader's error text,
                // which the TS surfaces through pushError.
                let summary = format_state_summary(Path::new(&self.paths.state_path));
                if summary.starts_with("Could not read harness state at ") || summary.starts_with("The file at ") {
                    self.push_error(summary);
                } else {
                    self.push_info(summary);
                }
            }
            "skills" => {
                let discovered = discover_all_skills(Path::new(&self.bootstrap.cwd), &self.bootstrap.home).unwrap_or_default();
                let active_names: HashSet<String> = self.active_skills.iter().map(|skill| skill.name.clone()).collect();
                let text = if discovered.is_empty() {
                    format!(
                        "no skills found. Add SKILL.md files under {}/<name>/ or ./.drip/skills/<name>/, or register a marketplace with /marketplace add.",
                        self.bootstrap.home.skills_dir
                    )
                } else {
                    discovered
                        .iter()
                        .map(|skill| {
                            let source = skill
                                .key
                                .clone()
                                .unwrap_or_else(|| format!("{:?}", skill.source).to_lowercase());
                            format!(
                                "{} {} ({}) — {}",
                                if active_names.contains(&skill.name) { "●" } else { "○" },
                                skill.name,
                                source,
                                skill.description
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                self.push_info(text);
            }
            "skill" => {
                if args.is_empty() {
                    self.push_error("Usage: /skill <name>");
                } else {
                    let name = args.split_whitespace().next().unwrap_or("").to_string();
                    self.toggle_skill(&name);
                }
            }
            "marketplace" => self.marketplace_command(args),
            "plugin" => {
                let parts: Vec<&str> = args.split_whitespace().collect();
                let action = parts.first().copied().unwrap_or("");
                let key = parts.get(1).copied().unwrap_or("");
                if (action != "enable" && action != "disable") || key.is_empty() {
                    self.push_error("Usage: /plugin <enable|disable> <marketplace/plugin[/skill]>");
                    return;
                }
                let home = &self.bootstrap.home;
                let file = match load_marketplaces_file(Path::new(&home.marketplaces_path)) {
                    Ok(file) => file,
                    Err(error) => {
                        self.push_error(error.to_string());
                        return;
                    }
                };
                let known_keys: HashSet<String> = Some(list_marketplace_plugins(home, &file))
                    .map(|listing| {
                        listing
                            .plugins
                            .iter()
                            .flat_map(|plugin| {
                                std::iter::once(plugin.key.clone())
                                    .chain(plugin.skills.iter().map(|skill| skill.key.clone()))
                                    .chain(plugin.roles.iter().map(|role| role.key.clone()))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                match set_marketplace_key_enabled(home, key, action == "enable") {
                    Ok(_) => {
                        let note = if known_keys.contains(key) {
                            String::new()
                        } else {
                            format!(" Note: no registered plugin or skill currently matches \"{key}\" — check /marketplace list.")
                        };
                        self.push_info(format!("{key} {action}d (user scope; project overrides live in .drip/plugins.json).{note}"));
                    }
                    Err(error) => self.push_error(error.to_string()),
                }
            }
            "roles" => self.roles_command(),
            "config" => {
                let env = self.merged_env();
                match resolve_cli_inference(&self.config, Some(&env)) {
                    Ok(inference) => {
                        let mut lines = vec![
                            format!("config: {}", self.bootstrap.home.config_path),
                            format!("env vars: {}", self.bootstrap.home.env_vars_path),
                            format!("model profile: {}", self.model_label()),
                            format!("endpoint: {}", inference.route.url),
                        ];
                        if let Some(route) = &inference.tool_route {
                            lines.push(format!("tool-calling model: {} ({})", route.model, route.url));
                        }
                        if let Some(warning) = &inference.tool_route_warning {
                            lines.push(format!("tool-calling model: {warning}"));
                        }
                        lines.push(format!("sessions: {}", self.paths.dir));
                        lines.push(
                            "edit the config file directly, or use /model, /toolmodel, /prompt, and /env to switch profiles and tokens."
                                .to_string(),
                        );
                        self.push_info(lines.join("\n"));
                    }
                    Err(error) => self.push_info(format!(
                        "config: {}\nprofile error: {error}",
                        self.bootstrap.home.config_path
                    )),
                }
            }
            "env" => self.env_command(args),
            _ => self.push_error(format!("Unknown command /{name}. Try /help.")),
        }
    }

    fn merged_env(&self) -> HashMap<String, String> {
        load_merged_env(Path::new(&self.bootstrap.home.env_vars_path), None).into_iter().collect()
    }

    fn marketplace_command(&mut self, args: &str) {
        let parts: Vec<String> = args.split_whitespace().map(|part| part.to_string()).collect();
        let subcommand = parts.first().map(|part| part.as_str()).unwrap_or("list");
        let home = self.bootstrap.home.clone();

        match subcommand {
            "list" => {
                let file = match load_marketplaces_file(Path::new(&home.marketplaces_path)) {
                    Ok(file) => file,
                    Err(error) => {
                        self.push_error(error.to_string());
                        return;
                    }
                };
                if file.marketplaces.is_empty() {
                    self.push_info("no marketplaces registered. Add one with /marketplace add <git-url-or-path> [name].");
                    return;
                }
                let overrides = load_project_plugin_overrides(Path::new(&self.bootstrap.cwd));
                let listing = list_marketplace_plugins(&home, &file);
                let mut lines: Vec<String> = Vec::new();
                for record in &file.marketplaces {
                    lines.push(format!("{} ({}: {})", record.name, record.kind, record.source));
                    for plugin in listing.plugins.iter().filter(|candidate| candidate.marketplace_name == record.name) {
                        let plugin_enabled = is_marketplace_key_enabled(&plugin.key, &plugin.key, &file, &overrides);
                        lines.push(format!(
                            "  {} {}{}",
                            if plugin_enabled { "●" } else { "○" },
                            plugin.key,
                            if plugin.description.is_empty() { String::new() } else { format!(" — {}", plugin.description) }
                        ));
                        for skill in &plugin.skills {
                            lines.push(format!(
                                "      {} skill {} — {}",
                                if is_marketplace_key_enabled(&plugin.key, &skill.key, &file, &overrides) { "●" } else { "○" },
                                skill.name,
                                skill.description
                            ));
                        }
                        for role in &plugin.roles {
                            lines.push(format!(
                                "      {} role {}{}",
                                if is_marketplace_key_enabled(&plugin.key, &role.key, &file, &overrides) { "●" } else { "○" },
                                role.name,
                                role.description.as_ref().map(|text| format!(" — {text}")).unwrap_or_default()
                            ));
                        }
                    }
                }
                lines.extend(listing.issues.iter().map(|issue| format!("! {issue}")));
                lines.push(String::new());
                lines.push("toggle with /plugin enable|disable <marketplace/plugin[/skill]>.".to_string());
                self.push_info(lines.join("\n"));
            }
            "add" => {
                let Some(source) = parts.get(1).cloned() else {
                    self.push_error("Usage: /marketplace add <git-url-or-path> [name]");
                    return;
                };
                self.push_info(format!("adding marketplace from {source} ..."));
                let name = parts.get(2).cloned();
                let tx = self.tx.clone();
                std::thread::spawn(move || {
                    let outcome = add_marketplace(AddMarketplaceArgs {
                        git: None,
                        home: &home,
                        name: name.as_deref(),
                        now: None,
                        source: &source,
                    });
                    let message = match outcome {
                        Ok(added) => {
                            let record = added.record;
                            let listing = list_marketplace_plugins(&home, &added.file);
                            let own: Vec<_> =
                                listing.plugins.iter().filter(|plugin| plugin.marketplace_name == record.name).collect();
                            let skill_count: usize = own.iter().map(|plugin| plugin.skills.len()).sum();
                            let role_count: usize = own.iter().map(|plugin| plugin.roles.len()).sum();
                            let mut lines = vec![format!(
                                "registered \"{}\" ({}): {} plugin(s), {} skill(s), {} role(s).",
                                record.name,
                                record.kind,
                                own.len(),
                                skill_count,
                                role_count
                            )];
                            let needle = format!("\"{}\"", record.name);
                            lines.extend(listing.issues.iter().filter(|issue| issue.contains(&needle)).map(|issue| format!("! {issue}")));
                            lines.push("everything starts disabled — enable with /plugin enable <key> (see /marketplace list).".to_string());
                            Msg::Info(lines.join("\n"))
                        }
                        Err(error) => Msg::Error(error.to_string()),
                    };
                    let _ = tx.send(message);
                });
            }
            "remove" => {
                let Some(name) = parts.get(1) else {
                    self.push_error("Usage: /marketplace remove <name>");
                    return;
                };
                match remove_marketplace(&home, name) {
                    Ok(_) => self.push_info(format!("removed marketplace \"{name}\".")),
                    Err(error) => self.push_error(error.to_string()),
                }
            }
            "update" => {
                self.push_info("updating marketplace clone(s) ...");
                let name = parts.get(1).cloned();
                let tx = self.tx.clone();
                std::thread::spawn(move || {
                    let message = match update_marketplaces(None, &home, name.as_deref()) {
                        Ok(updated) if updated.is_empty() => {
                            Msg::Info("nothing to update (local marketplaces read in place).".to_string())
                        }
                        Ok(updated) => Msg::Info(format!("updated: {}", updated.join(", "))),
                        Err(error) => Msg::Error(error.to_string()),
                    };
                    let _ = tx.send(message);
                });
            }
            _ => self.push_error("Usage: /marketplace [add <repo> [name] | remove <name> | update [name] | list]"),
        }
    }

    fn roles_command(&mut self) {
        let env = self.merged_env();
        let cwd = Path::new(&self.bootstrap.cwd);
        let home = &self.bootstrap.home;
        let role_setup = resolve_role_setup(&ResolveRoleSetupArgs {
            config: &self.config,
            cwd: self.bootstrap.cwd.clone(),
            env: Some(&env),
            extra_bindings: None,
            extra_roles: None,
            marketplace_roles: Some(list_enabled_marketplace_roles(cwd, home).unwrap_or_default()),
            skills: discover_all_skills(cwd, home).unwrap_or_default(),
            tool_names: self.tool_names(),
        });

        if role_setup.roles.is_empty() && role_setup.issues.is_empty() {
            self.push_info(
                "no roles defined. Define them in .drip/roles.json ({\"roles\": [...], \"bindings\": {...}}), the Harness Role Profiles config setting, or an enabled plugin's agents/ directory.",
            );
            return;
        }

        let mut lines: Vec<String> = role_setup
            .roles
            .iter()
            .map(|role| {
                let mut parts = vec![match &role.tool_names {
                    Some(names) if names.is_empty() => "tools: (harness ops only)".to_string(),
                    Some(names) => format!("tools: {}", names.join(", ")),
                    None => "tools: all".to_string(),
                }];
                if let Some(route) = &role.route {
                    parts.push(format!("model: {}", route.model));
                }
                if let Some(loop_config) = &role.r#loop {
                    parts.push(format!("loop: {}", serde_json::to_string(loop_config).unwrap_or_default()));
                }
                if let Some(verified_by) = &role.verified_by {
                    parts.push(format!("verifiedBy: {verified_by}"));
                }
                format!(
                    "{}{}\n    {}",
                    role.name,
                    role.description.as_ref().map(|text| format!(" — {text}")).unwrap_or_default(),
                    parts.join(" · ")
                )
            })
            .collect();
        let bindings = role_setup.bindings.as_ref();
        lines.push(format!(
            "bindings: planning={} task={} — tasks may carry their own role from plan_tasks",
            bindings.and_then(|b| b.planning.clone()).unwrap_or_else(|| "(default)".to_string()),
            bindings.and_then(|b| b.task.clone()).unwrap_or_else(|| "(default)".to_string())
        ));
        lines.extend(role_setup.issues.iter().map(|issue| format!("! {issue}")));
        self.push_info(lines.join("\n"));
    }

    fn env_command(&mut self, args: &str) {
        let env_path = self.bootstrap.home.env_vars_path.clone();
        if args.is_empty() {
            let profiles = match list_cli_model_profiles(&self.config.settings) {
                Ok(profiles) => profiles,
                Err(error) => {
                    self.push_error(error.to_string());
                    return;
                }
            };
            // Insertion-ordered like the TS Map.
            let mut order: Vec<String> = Vec::new();
            let mut referenced_by: HashMap<String, Vec<String>> = HashMap::new();
            for profile in &profiles {
                if let Some(env_name) = profile.api_key_ref.as_deref().and_then(|reference| reference.strip_prefix("env:")) {
                    let env_name = env_name.trim().to_string();
                    if !referenced_by.contains_key(&env_name) {
                        order.push(env_name.clone());
                    }
                    referenced_by.entry(env_name).or_default().push(profile.id.clone());
                }
            }

            let mut lines = vec![format!("env file: {env_path}"), String::new()];
            if order.is_empty() {
                lines.push("no model profiles reference env vars.".to_string());
            }
            let merged: BTreeMap<String, String> = load_merged_env(Path::new(&env_path), None);
            for env_name in &order {
                let source = lookup_env_var_source(Path::new(&env_path), env_name, None);
                let mark = match source {
                    "env-vars-file" => "● env.vars   ",
                    "process-env" => "○ process env",
                    _ => "✗ missing    ",
                };
                // Length + last-4 fingerprint so a corrupted or stale token is
                // visible without ever printing the secret.
                let value = if source == "missing" { String::new() } else { merged.get(env_name).cloned().unwrap_or_default().trim().to_string() };
                let fingerprint = if value.is_empty() {
                    String::new()
                } else {
                    let tail: String = value.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
                    format!(" ({} chars, …{tail})", value.chars().count())
                };
                lines.push(format!(
                    "{mark}  {env_name}{fingerprint} — {}",
                    referenced_by.get(env_name).map(|ids| ids.join(", ")).unwrap_or_default()
                ));
            }
            lines.push(String::new());
            lines.push("set one with /env KEY=value — it lands in the env file and applies to the next run.".to_string());
            self.push_info(lines.join("\n"));
            return;
        }

        let Some(separator) = args.find('=').filter(|index| *index > 0) else {
            self.push_error("Usage: /env KEY=value");
            return;
        };
        let key = args[..separator].trim().to_string();
        let value = args[separator + 1..].trim().to_string();
        match upsert_env_var(Path::new(&env_path), &key, &value) {
            Ok(()) => self.push_info(format!("{key} saved to {env_path}")),
            Err(error) => self.push_error(error.to_string()),
        }
    }

    fn tool_names(&self) -> Vec<String> {
        builtin_tool_pack(self.bootstrap.allow_net).iter().map(|tool| tool.name.clone()).collect()
    }

    // ----- goals ----------------------------------------------------------

    fn run_goal(&mut self, goal_text: String) {
        let goal_images = std::mem::take(&mut self.attachments);
        self.running = true;
        self.running_detail = Some("resolving context".to_string());
        // Mention resolution can read a whole directory tree; show the
        // "running" state before it starts.
        self.repaint();

        let resolved = resolve_goal_mentions(&goal_text, &self.bootstrap.cwd);
        for issue in &resolved.issues {
            self.push_info(format!("mention: {issue}"));
        }

        // run_session_goal owns the transcript file (goal/event/run-end
        // entries); the cells below are display-only.
        self.push_cell(
            TranscriptEntry::Goal(TranscriptGoalEntry {
                at: now_iso(),
                goal_id: "live".to_string(),
                images: goal_images.iter().map(|attachment| attachment.path.clone()).collect(),
                mentions: resolved.mentions.clone(),
                text: goal_text.clone(),
            }),
            false,
        );

        let env = self.merged_env();
        let inference = match resolve_cli_inference(&self.config, Some(&env)) {
            Ok(inference) => inference,
            Err(error) => {
                self.push_error(error.to_string());
                self.finish_run();
                return;
            }
        };

        // Roles resolve fresh per run so marketplace, config, and .drip/roles.json
        // edits apply to the next goal without restarting the session.
        let cwd = Path::new(&self.bootstrap.cwd);
        let role_setup = resolve_role_setup(&ResolveRoleSetupArgs {
            config: &self.config,
            cwd: self.bootstrap.cwd.clone(),
            env: Some(&env),
            extra_bindings: None,
            extra_roles: None,
            marketplace_roles: Some(list_enabled_marketplace_roles(cwd, &self.bootstrap.home).unwrap_or_default()),
            skills: discover_all_skills(cwd, &self.bootstrap.home).unwrap_or_default(),
            tool_names: self.tool_names(),
        });
        for issue in &role_setup.issues {
            self.push_info(format!("roles: {issue}"));
        }

        let signal = AbortSignal::new();
        self.abort = Some(signal.clone());

        let tx = self.tx.clone();
        let event_tx = self.tx.clone();
        let index_db_path = self.bootstrap.project.index_db_path.clone();
        let project = self.bootstrap.project.clone();
        let session = self.session.clone();
        let cwd = self.bootstrap.cwd.clone();
        let max_iterations = self.bootstrap.max_iterations;
        let no_repo_memory = self.bootstrap.no_repo_memory;
        let allow_net = self.bootstrap.allow_net;
        let skills = self.active_skills.clone();
        let redact_secrets = load_env_vars(Path::new(&self.bootstrap.home.env_vars_path)).unwrap_or_default();
        let goal_context = resolved.context_block.clone();
        let mentions = resolved.mentions.clone();
        let images: Vec<String> = goal_images.iter().map(|attachment| attachment.data_url.clone()).collect();

        std::thread::spawn(move || {
            // A panic anywhere below must still release the composer: the
            // guard reports it as a failed run unless the thread finishes normally.
            let mut guard = RunDoneGuard { tx: tx.clone(), armed: true };
            let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
                Ok(runtime) => runtime,
                Err(error) => {
                    guard.armed = false;
                    let _ = tx.send(Msg::RunDone(Err(SessionGoalError::Run(error.to_string()))));
                    return;
                }
            };
            let index = open_session_index(&index_db_path);
            let on_event: Arc<dyn Fn(HarnessEvent) + Send + Sync> = Arc::new(move |event: HarnessEvent| {
                let _ = event_tx.send(Msg::Event(event));
            });
            let result = runtime.block_on(run_session_goal(SessionGoalArgs {
                cwd,
                goal: goal_text,
                goal_context,
                goal_images: if images.is_empty() { None } else { Some(images) },
                index: &index,
                inference,
                max_iterations,
                mentions: Some(mentions),
                new_goal: false,
                no_repo_memory,
                on_event,
                plan_only: false,
                redact_secrets,
                request_timeout_ms: None,
                seed_tasks: None,
                project: &project,
                role_bindings: role_setup.bindings.clone(),
                roles: if role_setup.roles.is_empty() { None } else { Some(role_setup.roles.clone()) },
                session: &session,
                signal: Some(signal),
                skills,
                summarize_run: None,
                tools: builtin_tool_pack(allow_net),
                tool_services: None,
            }));
            index.close();
            guard.armed = false;
            let _ = tx.send(Msg::RunDone(result));
        });
    }

    fn on_run_done(&mut self, result: Result<SessionGoalOutcome, SessionGoalError>) {
        match result {
            Ok(outcome) => {
                self.push_cell(
                    TranscriptEntry::RunEnd(TranscriptRunEndEntry {
                        at: now_iso(),
                        goal_id: outcome.goal_id.clone(),
                        iterations: outcome.result.iterations,
                        reason: outcome.result.reason,
                    }),
                    false,
                );
            }
            Err(SessionGoalError::LiveRun(error)) => {
                self.push_error(format!("{} (Another process owns this session's run right now.)", error.message()));
            }
            Err(SessionGoalError::Run(message)) => self.push_error(message),
        }
        self.finish_run();
    }

    fn finish_run(&mut self) {
        self.abort = None;
        self.pending_detail = None;
        self.flush_pending_cells();
        self.running = false;
        self.running_detail = None;
    }

    // ----- main loop ------------------------------------------------------

    fn run(&mut self, rx: Receiver<Msg>) -> i32 {
        // SAFETY: installing async-signal-safe handlers that only store flags.
        unsafe {
            libc::signal(libc::SIGWINCH, on_winch as libc::sighandler_t);
            libc::signal(libc::SIGINT, on_halt as libc::sighandler_t);
            libc::signal(libc::SIGTERM, on_halt as libc::sighandler_t);
        }

        // Replay the transcript from the top, then the live region.
        let replay = std::mem::take(&mut self.cells);
        write_out(HIDE_CURSOR);
        self.emit_static(replay);
        self.repaint();

        if let Some(goal) = self.bootstrap.initial_goal.take() {
            self.run_goal(goal);
            self.repaint();
        }

        while !self.quit {
            if HALT.swap(false, Ordering::SeqCst) {
                self.quit = true;
                break;
            }

            if WINCH.swap(false, Ordering::SeqCst) {
                self.resize_at = Some(Instant::now() + Duration::from_millis(RESIZE_SETTLE_MS));
            }

            let now = Instant::now();
            if let Some(at) = self.resize_at {
                if now >= at {
                    self.resize_at = None;
                    let (cols, rows) = terminal_size();
                    let width_changed = cols != self.cols;
                    self.cols = cols;
                    self.rows = rows;
                    if width_changed {
                        self.full_repaint();
                    } else {
                        self.repaint();
                    }
                }
            }
            if let Some(deadline) = self.flush_deadline {
                if now >= deadline {
                    self.flush_pending_cells();
                }
            }

            let mut wait = Duration::from_millis(300);
            for deadline in [self.flush_deadline, self.resize_at].into_iter().flatten() {
                wait = wait.min(deadline.saturating_duration_since(now));
            }

            match rx.recv_timeout(wait) {
                Ok(Msg::Input(bytes)) => {
                    let keys = decode_input(&bytes, &mut self.paste_buffer);
                    for key in keys {
                        self.on_key(key);
                        if self.quit {
                            break;
                        }
                    }
                    self.repaint();
                }
                Ok(Msg::Event(event)) => {
                    self.pending_detail = Some(format!("cycle {}", event.iteration));
                    self.queue_cell(TranscriptEntry::Event(TranscriptEventEntry {
                        at: now_iso(),
                        data: None,
                        detail: event.detail,
                        goal_id: "live".to_string(),
                        iteration: event.iteration,
                        kind: event.r#type,
                    }));
                }
                Ok(Msg::RunDone(result)) => {
                    if matches!(&result, Err(SessionGoalError::Run(message)) if message == RUN_THREAD_PANIC) {
                        // The panic hook restored the terminal for a crash that
                        // did not happen on this thread; take it back.
                        write_out(&format!("{ENABLE_BRACKETED_PASTE}{HIDE_CURSOR}"));
                    }
                    self.on_run_done(result);
                    self.repaint();
                }
                Ok(Msg::Mentions { paths, seq }) => {
                    if seq == self.mention_seq {
                        self.mention_suggestions = paths;
                        self.repaint();
                    }
                }
                Ok(Msg::Info(text)) => self.push_info(text),
                Ok(Msg::Error(text)) => self.push_error(text),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => self.quit = true,
            }
        }

        // A run in flight is asked to stop at its next safe point; the exit
        // does not wait for it (matching ink's exitOnCtrlC).
        if let Some(abort) = self.abort.take() {
            abort.abort();
            // Give the harness a moment to reach its safe point and write the
            // run record; a stuck request is not worth more than a few seconds.
            let deadline = Instant::now() + Duration::from_secs(3);
            while let Ok(message) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                if let Msg::RunDone(result) = message {
                    self.on_run_done(result);
                    break;
                }
            }
        }
        self.flush_pending_cells();
        write_out(&format!("\n{SHOW_CURSOR}"));
        self.exit_code
    }
}

struct RunDoneGuard {
    armed: bool,
    tx: Sender<Msg>,
}

impl Drop for RunDoneGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.tx.send(Msg::RunDone(Err(SessionGoalError::Run(RUN_THREAD_PANIC.to_string()))));
        }
    }
}

const RUN_THREAD_PANIC: &str = "the run thread panicked — see the message above; the session state on disk is whatever the last save wrote";

fn spawn_stdin_reader(tx: Sender<Msg>) {
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 4096];
        let mut pending: Vec<u8> = Vec::new();
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => {
                    // EOF (closed pty, `</dev/null`): behave like ctrl+c.
                    let _ = tx.send(Msg::Input(b"\x03".to_vec()));
                    break;
                }
                Ok(n) => {
                    pending.extend_from_slice(&buf[..n]);
                    // A multibyte character split across reads waits for its tail.
                    let complete = match std::str::from_utf8(&pending) {
                        Ok(_) => pending.len(),
                        Err(error) if error.error_len().is_none() => error.valid_up_to(),
                        Err(_) => pending.len(),
                    };
                    if complete == 0 {
                        continue;
                    }
                    let chunk: Vec<u8> = pending.drain(..complete).collect();
                    if tx.send(Msg::Input(chunk)).is_err() {
                        break;
                    }
                }
            }
        }
    });
}

fn spawn_mention_indexer(cwd: String, requests: Receiver<(u64, String)>, tx: Sender<Msg>) {
    std::thread::spawn(move || {
        let mut source = WorkspaceFileSource::new(cwd);
        while let Ok((mut seq, mut query)) = requests.recv() {
            // Only the newest request matters; drain anything that queued up
            // while the previous walk ran.
            while let Ok((newer_seq, newer_query)) = requests.try_recv() {
                seq = newer_seq;
                query = newer_query;
            }
            let paths = match source.load() {
                Ok(files) => get_workspace_file_suggestions(&files, &query, DEFAULT_FILE_SUGGESTION_LIMIT),
                Err(_) => Vec::new(),
            };
            if tx.send(Msg::Mentions { paths, seq }).is_err() {
                break;
            }
        }
    });
}

/// Runs the interactive session on the current terminal; returns the exit code.
pub fn run_tui_app(bootstrap: TuiBootstrap) -> i32 {
    let (tx, rx) = mpsc::channel::<Msg>();
    let (mention_tx, mention_rx) = mpsc::channel::<(u64, String)>();
    let cwd = bootstrap.cwd.clone();

    let mut raw = RawMode::enable();
    write_out(ENABLE_BRACKETED_PASTE);

    // The terminal is restored even if a panic unwinds through the loop.
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        write_out(&format!("{SHOW_CURSOR}{DISABLE_BRACKETED_PASTE}\n"));
        previous_hook(info);
    }));

    spawn_stdin_reader(tx.clone());
    spawn_mention_indexer(cwd, mention_rx, tx.clone());

    let mut app = TuiApp::new(bootstrap, tx, mention_tx);
    let code = app.run(rx);

    write_out(DISABLE_BRACKETED_PASTE);
    raw.restore();
    let _ = std::panic::take_hook();
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(keys: &[Key]) -> Vec<&'static str> {
        keys.iter()
            .map(|key| match key {
                Key::Backspace => "backspace",
                Key::Ctrl(_) => "ctrl",
                Key::Delete => "delete",
                Key::Down => "down",
                Key::Escape => "escape",
                Key::Left => "left",
                Key::Paste(_) => "paste",
                Key::Return => "return",
                Key::Right => "right",
                Key::Tab => "tab",
                Key::Text(_) => "text",
                Key::Up => "up",
                Key::Ignored => "ignored",
            })
            .collect()
    }

    #[test]
    fn decodes_single_keys_and_escape_sequences() {
        let mut paste = None;
        assert_eq!(kinds(&decode_input(b"a", &mut paste)), vec!["text"]);
        assert_eq!(kinds(&decode_input(b"\r", &mut paste)), vec!["return"]);
        assert_eq!(kinds(&decode_input(b"\x1b[A", &mut paste)), vec!["up"]);
        assert_eq!(kinds(&decode_input(b"\x1b[3~", &mut paste)), vec!["delete"]);
        assert_eq!(kinds(&decode_input(b"\x1b", &mut paste)), vec!["escape"]);
        assert_eq!(kinds(&decode_input(b"\x7f", &mut paste)), vec!["backspace"]);
        match &decode_input(b"\x16", &mut paste)[0] {
            Key::Ctrl(c) => assert_eq!(*c, 'v'),
            _ => panic!("ctrl+v expected"),
        }
        assert_eq!(kinds(&decode_input("é".as_bytes(), &mut paste)), vec!["text"]);
    }

    #[test]
    fn multi_character_chunks_are_pastes_and_bracketed_pastes_span_chunks() {
        let mut paste = None;
        match &decode_input(b"hello\n", &mut paste)[0] {
            Key::Paste(text) => assert_eq!(text, "hello\n"),
            _ => panic!("paste expected"),
        }

        let first = decode_input(b"\x1b[200~one ", &mut paste);
        assert!(first.is_empty());
        assert!(paste.is_some());
        let second = decode_input(b"two\x1b[201~x", &mut paste);
        assert_eq!(kinds(&second), vec!["paste", "text"]);
        match &second[0] {
            Key::Paste(text) => assert_eq!(text, "one two"),
            _ => panic!("paste expected"),
        }
        assert!(paste.is_none());
    }

    #[test]
    fn a_chunk_with_several_escape_sequences_decodes_each() {
        let mut paste = None;
        assert_eq!(kinds(&decode_input(b"\x1b[A\x1b[A", &mut paste)), vec!["up", "up"]);
        assert_eq!(kinds(&decode_input(b"\x1b[Da", &mut paste)), vec!["left", "text"]);
        assert_eq!(kinds(&decode_input(b"\x1b[1;5C", &mut paste)), vec!["ignored"]);
    }

    #[test]
    fn clip_ansi_keeps_sgr_codes() {
        let row = format!("\x1b[7m{}\x1b[27m", "x".repeat(20));
        let clipped = clip_ansi(&row, 5);
        assert!(clipped.starts_with("\x1b[7m"));
        assert_eq!(string_width(&clipped), 5);
        assert_eq!(clip_ansi("short", 10), "short");
    }

    #[test]
    fn help_text_lists_every_slash_command() {
        let text = help_text();
        for command in SLASH_COMMANDS {
            assert!(text.contains(&format!("/{}", command.name)), "{}", command.name);
        }
        assert!(text.contains("ctrl+c — exit"));
    }
}
