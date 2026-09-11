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

use crate::cli::args::{praeparare_goal_with_context, PRAEPARARE_DEFAULT_MAX_ITERATIONS};
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
use crate::cli::roles::{load_skill_content, resolve_role_setup, ResolveRoleSetupArgs, RoleSetupSource};
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
use crate::core::types::{
    HarnessEvent, HarnessEventType, HarnessSurveyAnswer, HarnessSurveyAnswers, QuestionSurvey,
};
use crate::harness::model_call::AbortSignal;
use crate::tools::pack::{builtin_tool_pack, BuiltinToolOptions};
use crate::tui::pane_title::{FALLBACK_LABEL, PaneTitle, SPINNER_INTERVAL_MS};
use crate::tui::session_name::{persist_session_name, read_session_name, read_session_name_context};
use crate::tui::terminal_title::{
    generate_chat_title, generate_session_title, resolve_session_route, resolve_title_route,
    terminal_title_enabled, terminal_title_timeout_ms,
};
use crate::tui::term::{terminal_size, write_out, RawMode};
use crate::tui::compact::{
	render_compact_cell, render_tool_group, select_compact_tail_start,
	CompactCell, CompactEmitter,
};
use crate::tui::widgets::{render_composer, render_picker, render_status_bar, ComposerProps, PickerItem, StatusBarProps};
use crate::watch::ansi::{string_width, wrap_ansi};

/// What `drip --tui` needs from entry.rs to start.
pub struct TuiBootstrap {
    pub allow_net: bool,
    /// `--ask` opt-in: the ask_user tool surveys the operator via an overlay.
    pub ask: bool,
    /// `--ask-timeout <seconds>` override for the survey answer wait.
    pub ask_timeout_secs: Option<i64>,
    /// oasis corpus roots for the REFERENCE tool (empty = no corpus).
    pub reference_roots: Vec<std::path::PathBuf>,
    pub config: CliConfig,
    pub cwd: String,
    pub home: DripHome,
    pub initial_goal: Option<String>,
    pub max_iterations: Option<i64>,
    pub max_loops: Option<i64>,
    pub no_repo_memory: bool,
    pub project: DripProject,
    pub roles_flag: Option<RoleSetupSource>,
    pub session: SessionRecord,
    /// Opt-in custom status-line command from the persisted drip config; None
    /// keeps the built-in status bar. Never imported from ~/.claude.
    pub status_line: Option<crate::core::config::StatusLineSetting>,
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
            "  /<skill-name> — enable a discovered skill for this session (idempotent; /skill <name> toggles)",
            "  /praeparare — pre-PR pass: clean up, commit, merge the base branch, push, open a DRAFT PR",
            "  typing /<prefix> lists matching skills above the input; up/down select, tab completes, esc clears the line",
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
    Question,
    Sessions,
    ToolModel,
}

struct Overlay {
    items: Vec<PickerItem>,
    kind: OverlayKind,
    selected: usize,
    title: String,
}

/// Sentinel PickerItem id for the survey's free-text escape hatch — a NUL
/// prefix keeps it disjoint from any real option label.
const SURVEY_OTHER_ID: &str = "\u{0}other";

/// A live ask_user survey being answered one question at a time. The harness
/// thread stays blocked on answers.jsonl; the overlay/composer only collect.
struct SurveyState {
    survey: QuestionSurvey,
    /// Question currently shown (0-based).
    current: usize,
    answers: Vec<HarnessSurveyAnswer>,
    /// Some while the operator is typing a free-text "Other…" answer.
    other_input: Option<String>,
    /// answers.jsonl of the session whose run asked — captured at open so a
    /// later session switch cannot redirect the answers to the wrong file.
    answers_path: std::path::PathBuf,
}

enum Msg {
    Error(String),
    Event(HarnessEvent),
    Info(String),
    Input(Vec<u8>),
    Mentions { paths: Vec<String>, seq: u64 },
    RunDone(Result<SessionGoalOutcome, SessionGoalError>),
    /// One-shot title generation finished on the background thread. `label`
    /// is None on any failure; stale epochs are dropped by the handler.
    Title { epoch: u64, label: Option<String> },
    /// An explicit /rename finished on a background thread. Unlike Msg::Title
    /// the name is persisted to session.json when present.
    Rename { epoch: u64, name: Option<String> },
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

// ----- prompt history ------------------------------------------------------

/// Bounded in-memory history of accepted prompts with Up/Down navigation and
/// a saved unsent draft. Pure state, deliberately separate from the
/// transcript/message storage (`cells`, `pending_cells`) and never persisted:
/// a session switch simply resets navigation instead of restoring a draft.
struct PromptHistory {
    /// Accepted prompts, oldest first, bounded to `max_entries`.
    entries: std::collections::VecDeque<String>,
    max_entries: usize,
    /// While browsing: index into `entries` of the currently recalled prompt.
    browsing: Option<usize>,
    /// The composer text saved when navigation started; `newer` restores it
    /// exactly when the user walks back past the newest entry.
    draft: Option<String>,
}

impl PromptHistory {
    fn new(max_entries: usize) -> Self {
        Self {
            entries: std::collections::VecDeque::new(),
            max_entries: max_entries.max(1),
            browsing: None,
            draft: None,
        }
    }

    /// Number of stored entries (introspection for tests).
    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether a navigation walk is in progress (Up started, not yet ended).
    fn is_browsing(&self) -> bool {
        self.browsing.is_some()
    }

    /// Records an accepted prompt. Blank text is ignored, a consecutive
    /// duplicate of the newest entry is suppressed, and any in-flight
    /// navigation (and its saved draft) resets: accepting a new prompt ends
    /// browsing. Entries keep multiline text verbatim apart from the trim.
    fn record(&mut self, text: &str) {
        // Accepting a prompt always ends navigation, even when the text is a
        // duplicate or blank and nothing is stored.
        self.browsing = None;
        self.draft = None;
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        if self.entries.back().map(|last| last.as_str()) == Some(trimmed) {
            return;
        }
        self.entries.push_back(trimmed.to_string());
        while self.entries.len() > self.max_entries {
            self.entries.pop_front();
        }
    }

    /// Up arrow: recall an older prompt. Starts at the newest entry, saving
    /// the composer's current text as the draft; repeated calls walk older and
    /// clamp at the oldest entry. The returned String is a fresh copy, so
    /// editing it never mutates the stored entry.
    fn older(&mut self, current: &str) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }
        let next = match self.browsing {
            Some(index) => index.saturating_sub(1),
            None => {
                self.draft = Some(current.to_string());
                self.entries.len() - 1
            }
        };
        self.browsing = Some(next);
        Some(self.entries[next].clone())
    }

    /// Down arrow: recall a newer prompt; stepping past the newest restores
    /// the exact draft saved when navigation started. A no-op (returns None,
    /// changes nothing) when not currently browsing.
    fn newer(&mut self) -> Option<String> {
        let index = self.browsing?;
        if index + 1 < self.entries.len() {
            self.browsing = Some(index + 1);
            Some(self.entries[index + 1].clone())
        } else {
            self.browsing = None;
            self.draft.take()
        }
    }

    /// Ends navigation and discards the saved draft. Called on session switch
    /// so a draft typed for one session never leaks into another.
    fn reset(&mut self) {
        self.browsing = None;
        self.draft = None;
    }
}

struct TuiApp {
    abort: Option<AbortSignal>,
    active_skills: Vec<LoadedCliSkill>,
    attachments: Vec<GoalImageAttachment>,
    bootstrap: TuiBootstrap,
    /// Every timeline cell rendered so far (for the repaint tail after a resize).
    cells: Vec<TranscriptEntry>,
    /// Presentation-only projection folding tool activity per cycle into the
    /// compact `-- N Tools called: ... --` rows. Never persisted.
    compact: CompactEmitter,
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
    prompt_history: PromptHistory,
    pending_detail: Option<String>,
    quit: bool,
    resize_at: Option<Instant>,
    rows: usize,
    running: bool,
    running_detail: Option<String>,
    selected_skill_index: usize,
    selected_suggestion_index: usize,
    skill_catalog: Vec<(String, String)>,
    skill_suggestions: Vec<(String, String)>,
    /// Live ask_user clarification survey (staged multiple-choice overlay).
    survey: Option<SurveyState>,
    /// Session the current (or last) goal run belongs to — a late Question
    /// event from an old run must not open a survey against a switched
    /// session (mirrors the title/rename epoch guards).
    run_session_id: Option<String>,
    session: SessionRecord,
    status_line_next_refresh: Option<Instant>,
    status_line_output: Option<crate::tui::status_line::StatusLineOutput>,
    status_line_request_width: Option<usize>,
    status_line_runner: Option<crate::tui::status_line::StatusLineRunner>,
    text: String,
    /// OSC 2 title state while an interactive TTY owns stdout; None keeps
    /// headless/redirected runs silent. Pure state lives in pane_title.rs.
    pane_title: Option<PaneTitle>,
    /// One-shot guard: title generation is requested at most once per session.
    title_requested: bool,
    /// Bumped on session switch; in-flight generations from older epochs are
    /// stale and dropped without touching the title.
    title_epoch: u64,
    /// Bumped on each /rename; stale in-flight renames are dropped.
    rename_epoch: u64,
    title_next_tick: Option<Instant>,
    tx: Sender<Msg>,
}

/// Pure render decision: the painted custom status row for a finished job.
/// `None` keeps the default bar (no output, failure, or blank line); a painted
/// row is sanitized and width-fitted at draw time only — no side effects.
fn custom_status_row_from(
    output: Option<&crate::tui::status_line::StatusLineOutput>,
    cols: usize,
    padding: usize,
) -> Option<String> {
    let output = output?;
    if !output.ok || output.line.is_empty() {
        return None;
    }
    Some(crate::tui::status_line::sanitize_status_line(
        &output.line,
        cols,
        padding as u16,
    ))
}

/// A lone "/token" with no whitespace may be a not-yet-enabled skill name;
/// the built-in slash command menu takes priority and suppresses this one.
fn skill_suggestion_query(text: &str) -> Option<String> {
    if !text.starts_with('/') || text.trim().chars().any(char::is_whitespace) {
        return None;
    }
    Some(text[1..].to_ascii_lowercase())
}

/// Prefix filter over the cached skill catalog: full-name prefixes first,
/// then prefixes of the last ':' segment of qualified names, catalog order
/// preserved within each group. Built-in command names are excluded so Tab
/// never completes into a shadowed token. Bounded to keep the menu short.
fn filter_skill_catalog(
    catalog: &[(String, String)],
    query: &str,
    builtins: &[&str],
) -> Vec<(String, String)> {
    let query = query.to_ascii_lowercase();
    let mut exact: Vec<(String, String)> = Vec::new();
    let mut qualified: Vec<(String, String)> = Vec::new();
    for (name, description) in catalog {
        if builtins
            .iter()
            .any(|builtin| builtin.eq_ignore_ascii_case(name.as_str()))
        {
            continue;
        }
        let last = name.rsplit(':').next().unwrap_or(name);
        if name.to_ascii_lowercase().starts_with(&query) {
            exact.push((name.clone(), description.clone()));
        } else if last.to_ascii_lowercase().starts_with(&query) {
            qualified.push((name.clone(), description.clone()));
        }
    }
    exact.extend(qualified);
    exact.truncate(8);
    exact
}

impl TuiApp {
    fn new(bootstrap: TuiBootstrap, tx: Sender<Msg>, mention_tx: Sender<(u64, String)>) -> Self {
        let paths = session_paths_for(&bootstrap.project, &bootstrap.session);
        let cells = read_transcript(Path::new(&paths.transcript_path));
        let (cols, rows) = terminal_size();
        let config = bootstrap.config.clone();
        let session = bootstrap.session.clone();
        let status_line_runner = bootstrap
            .status_line
            .clone()
            .map(crate::tui::status_line::StatusLineRunner::new);

        // Skill catalog is discovered at construction; the edit and draw
        // paths only ever filter this cached copy. Command dispatch refreshes
        // it (refresh_skill_catalog) so skills installed after startup become
        // visible without any per-draw filesystem scan.
        let skill_catalog: Vec<(String, String)> =
            discover_all_skills(Path::new(&bootstrap.cwd), &bootstrap.home)
                .unwrap_or_default()
                .into_iter()
                .map(|skill| (skill.name, skill.description))
                .collect();

        Self {
            abort: None,
            active_skills: Vec::new(),
            attachments: Vec::new(),
            bootstrap,
            cells,
            compact: CompactEmitter::new(),
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
            survey: None,
            run_session_id: None,
            pending_cells: Vec::new(),
            pending_detail: None,
            prompt_history: PromptHistory::new(64),
            quit: false,
            resize_at: None,
            rows,
            running: false,
            running_detail: None,
            selected_skill_index: 0,
            selected_suggestion_index: 0,
            skill_catalog,
            skill_suggestions: Vec::new(),
            session,
            status_line_next_refresh: None,
            pane_title: None,
            title_requested: false,
            title_epoch: 0,
            rename_epoch: 0,
            title_next_tick: None,
            status_line_output: None,
            status_line_request_width: None,
            status_line_runner,
            text: String::new(),
            tx,
        }
    }

    // ----- timeline -------------------------------------------------------

    /// Rendered rows for projected compact cells (no trailing newlines).
    fn projected_rows(&self, cells: &[CompactCell]) -> Vec<String> {
        let mut out = Vec::new();
        for cell in cells {
            out.extend(render_compact_cell(cell, self.cols));
        }
        out
    }

    /// Prints cells above the live region (Ink's <Static>).
    ///
    /// Raw entries still land in `cells` (repaint tail budgeting) and feed
    /// the compact projection, but scrollback shows the COMPACT view: tool
    /// activity folds into one summary row per cycle while goals, model
    /// text and boundaries stay visible. The active group is never painted
    /// here -- it lives in the erasable live region until a boundary
    /// finalizes it, and then it is emitted exactly once.
    fn emit_static(&mut self, entries: Vec<TranscriptEntry>) {
        let cells = self.compact.absorb(&entries);
        if !entries.is_empty() {
            self.cells.extend(entries);
        }

        let rows = self.projected_rows(&cells);
        if rows.is_empty() {
            // Telemetry-only batches change nothing on scrollback, but the
            // live summary may have grown -- repaint it in place.
            self.repaint();
            return;
        }
        let mut out = String::new();
        for row in rows {
            out.push_str(&row);
            out.push('\n');
        }
        self.paint(&out);
    }

    /// Rebuild the compact projection from raw transcript entries (startup
    /// replay / session switch). Finalized cells are returned as painted
    /// rows; an unfinished trailing group stays in the live region.
    fn rebuild_compact(&mut self, entries: &[TranscriptEntry]) -> Vec<String> {
        self.compact.rebuild(entries);
        self.projected_rows(&self.compact.projection.cells)
    }

    /// End-of-run boundary: finalize any still-open tool group exactly once
    /// and paint it, so the live summary row settles into scrollback.
    fn finalize_compact(&mut self) {
        let cells = self.compact.finalize();
        let rows = self.projected_rows(&cells);
        if rows.is_empty() {
            self.repaint();
            return;
        }
        let mut out = String::new();
        for row in rows {
            out.push_str(&row);
            out.push('\n');
        }
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

        // Erasable in-place summary of the current tool cycle: grows in one
        // row above the composer and is finalized into scrollback exactly
        // once, when a visible boundary flushes the projection.
        if let Some(group) = self.compact.projection.active_group() {
            rows.extend(render_tool_group(group, self.cols));
        }

        if let Some(state) = self.survey.as_ref().filter(|state| state.other_input.is_some()) {
            let question = state
                .survey
                .questions
                .get(state.current)
                .map(|question| question.question.as_str())
                .unwrap_or("");
            let input = state.other_input.as_deref().unwrap_or("");
            rows.push(format!("Other — {question}"));
            rows.push(format!("> {input}▏"));
            rows.push("enter to submit · esc back to choices".to_string());
        } else if let Some(overlay) = &self.overlay {
            rows.extend(render_picker(&overlay.title, &overlay.items, overlay.selected, self.cols));
        } else {
            let slash: Vec<&SlashCommandSpec> = get_slash_command_suggestions(&self.text);
            rows.extend(render_composer(
                &ComposerProps {
                    attachments: &self.attachments,
                    cursor: self.cursor,
                    disabled: self.running,
                    mention_suggestions: &self.mention_suggestions,
                    selected_skill_index: self.selected_skill_index,
                    selected_suggestion_index: self.selected_suggestion_index,
                    skill_suggestions: &self.skill_suggestions,
                    slash_suggestions: &slash,
                    text: &self.text,
                },
                self.cols,
            ));
        }

        let skill_names: Vec<String> = self.active_skills.iter().map(|skill| skill.name.clone()).collect();
        // Custom statusLine: when configured, its output replaces only the
        // presentation status row (essential controls and the composer stay).
        // Disabled, failed, timed-out, or blank output falls back to the
        // default bar; failures never retry or log from the paint path.
        let mut custom_status_rows: Option<usize> = None;
        if self.status_line_runner.is_some() {
            if let Some(row) = self.custom_status_row() {
                if !row.is_empty() {
                    let before = rows.len();
                    rows.push(row);
                    custom_status_rows = Some(rows.len() - before);
                }
            }
        }
        let before_status = rows.len();
        if custom_status_rows.is_none() {
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
        }
        let status_rows = match custom_status_rows {
            Some(count) => count,
            None => rows.len() - before_status,
        };

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
        // Budget the tail over COMPACT cells so raw tool rows never re-appear
        // after a resize; the active group is excluded here -- it is drawn by
        // the live region (and accounted for via its own row there).
        let finalized = &self.compact.projection.cells;
        let start = select_compact_tail_start(finalized, self.rows);
        let mut out = String::from(CLEAR_VISIBLE_SCREEN);
        for row in self.projected_rows(&finalized[start..]) {
            out.push_str(&row);
            out.push('\n');
        }
        self.live_rows = 0;
        self.paint(&out);
    }

    // ----- custom status line ---------------------------------------------

    /// Builds the runner request from real session state. Telemetry drip does
    /// not genuinely know (model id, token/context usage, version) stays None
    /// and serializes as null/absent rather than being invented.
    fn status_line_request(&self) -> crate::tui::status_line::StatusLineRequest {
        crate::tui::status_line::StatusLineRequest {
            session_id: Some(self.session.id.clone()),
            cwd: Some(self.bootstrap.cwd.clone()),
            model_id: None,
            model_display_name: Some(self.model_label()),
            version: None,
            render_width_chars: self.cols,
            context_usage: None,
        }
    }

    /// Paints the custom row from cached output only: no job is started and
    /// nothing is executed here, so drawing stays side-effect-free.
    fn custom_status_row(&self) -> Option<String> {
        let padding = self
            .status_line_runner
            .as_ref()
            .map(|runner| usize::try_from(runner.setting().padding).unwrap_or(0))
            .unwrap_or(0);
        custom_status_row_from(self.status_line_output.as_ref(), self.cols, padding)
    }

    /// Non-blocking: adopts any finished status-line job and re-arms the
    /// interval refresh. Width changes (resize) refresh immediately; both are
    /// rate limited by the configured interval plus the runner's own spacing.
    /// Input and streaming are never blocked.
    fn poll_status_line(&mut self) {
        if self.status_line_runner.is_none() {
            return;
        }
        if let Some(runner) = self.status_line_runner.as_ref() {
            if let Some(output) = runner.poll_output() {
                self.status_line_output = Some(output);
            }
        }
        let due = self
            .status_line_next_refresh
            .map_or(true, |at| Instant::now() >= at)
            || self.status_line_request_width != Some(self.cols);
        if !due {
            return;
        }
        let interval_ms = self
            .status_line_runner
            .as_ref()
            .map(|runner| runner.setting().update_interval_ms)
            .unwrap_or(300);
        self.status_line_next_refresh =
            Some(Instant::now() + Duration::from_millis(interval_ms));
        self.status_line_request_width = Some(self.cols);
        let request = self.status_line_request();
        if let Some(runner) = self.status_line_runner.as_mut() {
            runner.request_refresh(request);
        }
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
            self.refresh_skill_suggestions();
        }
        self.refresh_mentions();
    }

    /// Recalls the previous prompt into the composer. Navigation is entered
    /// only when the cursor sits on the first line (the unsent composer text
    /// is saved as the draft); once browsing, repeated Up presses walk older
    /// entries regardless of the cursor line.
    fn recall_older_prompt(&mut self) {
        if !self.prompt_history.is_browsing() {
            let chars: Vec<char> = self.text.chars().collect();
            let cursor = self.cursor.min(chars.len());
            if chars[..cursor].iter().any(|&c| c == '\n') {
                return;
            }
        }
        if let Some(text) = self.prompt_history.older(&self.text) {
            let cursor = text.chars().count();
            self.apply_edit(text, cursor);
        }
    }

    /// Steps toward newer prompts while browsing, restoring the exact draft
    /// past the newest entry. Navigation is entered only from the last line;
    /// once browsing, Down walks newer regardless of the cursor line, and it
    /// stays a no-op when not browsing.
    fn recall_newer_prompt(&mut self) {
        // `newer()` itself is a no-op returning None when not browsing.
        if !self.prompt_history.is_browsing() {
            let chars: Vec<char> = self.text.chars().collect();
            let cursor = self.cursor.min(chars.len());
            if chars[cursor..].iter().any(|&c| c == '\n') {
                return;
            }
        }
        if let Some(text) = self.prompt_history.newer() {
            let cursor = text.chars().count();
            self.apply_edit(text, cursor);
        }
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

    /// Skill menu mirrors the slash/mention menus: refreshed on every text
    /// edit from the catalog cached at construction (no filesystem scans in
    /// the draw or edit path).
    fn refresh_skill_suggestions(&mut self) {
        self.skill_suggestions = match skill_suggestion_query(&self.text) {
            Some(query) if get_slash_command_suggestions(&self.text).is_empty() => {
                let builtins: Vec<&str> =
                    SLASH_COMMANDS.iter().map(|command| command.name).collect();
                filter_skill_catalog(&self.skill_catalog, &query, &builtins)
            }
            _ => Vec::new(),
        };
        self.selected_skill_index = 0;
    }

    /// Tab/Enter completion: inserts "/<name> " WITHOUT submitting or
    /// activating the skill.
    fn accept_skill_suggestion(&mut self) -> bool {
        if self.skill_suggestions.is_empty() {
            return false;
        }
        let (name, _) = self.skill_suggestions[self
            .selected_skill_index
            .min(self.skill_suggestions.len() - 1)]
        .clone();
        let next_text = format!("/{name} ");
        let len = next_text.chars().count();
        self.apply_edit(next_text, len);
        true
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

        if !self.skill_suggestions.is_empty() {
            return self.accept_skill_suggestion();
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

        // Skill names allow characters the slash-command grammar rejects
        // (qualified names like "spellcraft:navis", digits, dots), so a lone
        // "/<token>" that parse_slash_command rejected still activates a
        // discovered skill; anything else falls through to the goal run.
        if submitted.starts_with('/') && self.enable_skill_if_discovered(&submitted[1..]) {
            return;
        }

        self.submit_goal_text(submitted);
    }

    /// Shared tail of the ordinary typed-input path: prompt history,
    /// attachment images, and the run_goal handoff. Slash commands dispatch
    /// above and never reach it, so accepted prompts are recorded exactly
    /// once; /praeparare calls it directly with the shared canned goal so its
    /// run looks identical to a typed goal.
    fn submit_goal_text(&mut self, submitted: String) {
        // Only a goal that actually reaches run_goal enters history: UI-only
        // command and skill submissions returned above, and blank prompts were
        // rejected at the top, so each accepted prompt is recorded exactly
        // once. (Recorded before `goal` moves `submitted`.)
        if !submitted.is_empty() {
            self.prompt_history.record(&submitted);
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
        // Free-text "Other…" survey answer: collected before the overlay arm
        // and the running guard so typing works mid-run.
        if self.survey.as_ref().is_some_and(|state| state.other_input.is_some()) {
            match key {
                Key::Return => {
                    let text = self
                        .survey
                        .as_ref()
                        .and_then(|state| state.other_input.clone())
                        .unwrap_or_default()
                        .trim()
                        .to_string();
                    if !text.is_empty() {
                        self.record_survey_answer(None, Some(text));
                    }
                }
                Key::Escape => {
                    if let Some(state) = self.survey.as_mut() {
                        state.other_input = None;
                    }
                    self.open_survey_question();
                }
                Key::Backspace => {
                    if let Some(input) = self.survey.as_mut().and_then(|state| state.other_input.as_mut()) {
                        input.pop();
                    }
                }
                Key::Text(text) | Key::Paste(text) => {
                    if let Some(input) = self.survey.as_mut().and_then(|state| state.other_input.as_mut()) {
                        input.push_str(&text);
                    }
                }
                Key::Ctrl('c') => self.quit = true,
                _ => {}
            }
            return;
        }

        if let Some(overlay) = self.overlay.as_mut() {
            match key {
                Key::Escape => {
                    let kind = overlay.kind;
                    self.overlay = None;
                    if kind == OverlayKind::Question {
                        self.survey = None;
                        self.push_info(
                            "survey dismissed — answer with `drip --answer` or the run ends at its ask timeout",
                        );
                    }
                }
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
        let skill_len = self.skill_suggestions.len();
        let menu_length = if slash_len > 0 {
            slash_len
        } else if skill_len > 0 {
            skill_len
        } else {
            self.mention_suggestions.len()
        };

        match key {
            Key::Escape => self.apply_edit(String::new(), 0),
            Key::Return => {
                // Enter accepts an open menu selection unless the text already matches it exactly.
                if menu_length > 0 {
                    let slash = get_slash_command_suggestions(&self.text);
                    let exact_slash = slash.len() == 1 && format!("/{}", slash[0].name) == self.text.trim();
                    let exact_skill = slash_len == 0
                        && self.skill_suggestions.len() == 1
                        && format!("/{}", self.skill_suggestions[0].0) == self.text.trim();
                    if !exact_slash && !exact_skill && self.accept_suggestion() {
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
                    if skill_len > 0 && slash_len == 0 {
                        self.selected_skill_index = self.selected_skill_index.saturating_sub(1);
                    } else {
                        self.selected_suggestion_index =
                            self.selected_suggestion_index.saturating_sub(1);
                    }
                } else {
                    self.recall_older_prompt();
                }
            }
            Key::Down => {
                if menu_length > 0 {
                    if skill_len > 0 && slash_len == 0 {
                        self.selected_skill_index =
                            (self.selected_skill_index + 1).min(menu_length - 1);
                    } else {
                        self.selected_suggestion_index =
                            (self.selected_suggestion_index + 1).min(menu_length - 1);
                    }
                } else {
                    self.recall_newer_prompt();
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
            // Question overlays carry live survey data — opened by
            // open_survey_question, never through this static menu path.
            OverlayKind::Question => return,
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
                list_all_sessions(&self.bootstrap.project, Some(15))
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
        self.overlay = Some(Overlay { items, kind, selected: 0, title: title.to_string() });
    }

    // ----- ask_user surveys ----------------------------------------------

    fn begin_survey(&mut self, survey: QuestionSurvey) {
        if survey.questions.is_empty() {
            return;
        }
        if let Some(live) = self.survey.as_ref() {
            // A resume re-emits the same pending survey; keep the operator's
            // staged progress instead of silently restarting from question 1.
            if live.survey.questions == survey.questions {
                return;
            }
            self.push_info("a new clarification survey replaced the one in progress");
        }
        self.survey = Some(SurveyState {
            survey,
            current: 0,
            answers: Vec::new(),
            other_input: None,
            // The exact file the blocked harness thread polls (loop.rs answers_path()).
            answers_path: Path::new(&self.paths.state_path).with_file_name("answers.jsonl"),
        });
        self.open_survey_question();
    }

    fn open_survey_question(&mut self) {
        let Some(state) = &self.survey else { return };
        let Some(question) = state.survey.questions.get(state.current) else { return };
        let mut items: Vec<PickerItem> = question
            .options
            .iter()
            .map(|option| PickerItem {
                detail: Some(option.description.clone()),
                id: option.label.clone(),
                label: option.label.clone(),
            })
            .collect();
        if question.allow_other {
            items.push(PickerItem {
                detail: Some("answer with free text instead".to_string()),
                id: SURVEY_OTHER_ID.to_string(),
                label: "Other…".to_string(),
            });
        }
        let title = format!(
            "clarification {}/{} · {} — {}",
            state.current + 1,
            state.survey.questions.len(),
            question.header,
            question.question
        );
        self.overlay = Some(Overlay { items, kind: OverlayKind::Question, selected: 0, title });
    }

    fn record_survey_answer(&mut self, choice: Option<String>, other: Option<String>) {
        let done = {
            let Some(state) = self.survey.as_mut() else { return };
            state.answers.push(HarnessSurveyAnswer {
                index: state.current as i64,
                choice,
                other,
            });
            state.other_input = None;
            state.current += 1;
            state.current >= state.survey.questions.len()
        };
        if done {
            self.finish_survey();
        } else {
            self.open_survey_question();
        }
    }

    /// Tear down a live survey and its overlay (session switch, run end).
    /// The blocked run, if still alive, waits for `drip --answer` or times out.
    fn drop_survey(&mut self, message: &str) {
        if self.survey.take().is_none() {
            return;
        }
        if self.overlay.as_ref().is_some_and(|overlay| overlay.kind == OverlayKind::Question) {
            self.overlay = None;
        }
        self.push_info(message.to_string());
    }

    fn finish_survey(&mut self) {
        let Some(state) = self.survey.as_ref() else { return };
        let record = HarnessSurveyAnswers { at: now_iso(), answers: state.answers.clone() };
        // A single local append syscall: transient failures are worth two
        // cheap retries before falling back to the manual re-answer path.
        let mut outcome = crate::core::state::answers::append_answers(&state.answers_path, &record);
        for _ in 0..2 {
            if outcome.is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
            outcome = crate::core::state::answers::append_answers(&state.answers_path, &record);
        }
        match outcome {
            Ok(()) => {
                self.survey = None;
                self.overlay = None;
                self.push_info("clarification answers recorded — the run continues");
            }
            Err(error) => {
                // Keep the staged answers: reopen the last question so
                // re-answering retries the write instead of losing the survey.
                self.push_error(format!(
                    "could not record survey answers: {error} — re-answer the last question to retry"
                ));
                if let Some(state) = self.survey.as_mut() {
                    state.answers.pop();
                    state.current = state.current.saturating_sub(1);
                }
                self.open_survey_question();
            }
        }
    }

    fn on_pick(&mut self, kind: OverlayKind, item: PickerItem) {
        match kind {
            OverlayKind::Question => {
                if item.id == SURVEY_OTHER_ID {
                    if let Some(state) = self.survey.as_mut() {
                        state.other_input = Some(String::new());
                    }
                } else {
                    self.record_survey_answer(Some(item.id), None);
                }
            }
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
        // A survey belongs to the session whose run asked it; never carry it
        // (or write its answers) across a switch.
        self.drop_survey("survey dismissed by session switch — answer that run with `drip --answer`");
        // History stays in memory across sessions, but browsing state and the
        // saved draft must not leak into the newly loaded session.
        self.prompt_history.reset();
        self.paths = session_paths_for(&self.bootstrap.project, &record);
        self.session = record;
        // New session: invalidate any in-flight title generation for the old
        // session (epoch mismatch drops it) and reset to the neutral fallback
        // label; the new session's first goal may request a fresh title.
        self.title_epoch = self.title_epoch.wrapping_add(1);
        self.title_requested = false;
        self.title_next_tick = None;
        // Invalidate any in-flight /rename for the old session too: its late
        // reply must fail the epoch guard in apply_rename_result instead of
        // clobbering this session's restored or fallback label.
        self.rename_epoch = self.rename_epoch.wrapping_add(1);
        if let Some(title) = self.pane_title.as_mut() {
            let escape = title.set_label(FALLBACK_LABEL, Instant::now());
            crate::tui::pane_title::emit(escape.as_deref());
        }
        // Restore this session's explicit /rename name, if it has one —
        // materializing the pane title when no goal has created it yet, so
        // a resumed session shows its persisted name immediately.
        if let Some(name) = read_session_name(Path::new(&self.paths.meta_path)) {
            apply_rename_label(&mut self.pane_title, &name, stdout_is_tty());
        }
        // Replay the new transcript from the top, like remounting <Static>:
        // the previous session's rows stay in scrollback (ink cannot take
        // static output back) and the new transcript is printed below them.
        self.cells = read_transcript(Path::new(&self.paths.transcript_path));
        // Same projection as the live path: raw tool rows never re-appear on
        // session switch; an unfinished trailing group stays in the live row.
        let cells = std::mem::take(&mut self.cells);
        let out = self.rebuild_compact(&cells);
        self.cells = cells;
        if out.is_empty() {
            self.repaint();
        } else {
            let mut text = String::new();
            for row in out {
                text.push_str(&row);
                text.push('\n');
            }
            self.paint(&text);
        }
    }

    // ----- skills ---------------------------------------------------------

    /// Reload the cached skill catalog from disk.
    ///
    /// Only command dispatch calls this: the edit and draw paths must stay
    /// free of filesystem scans (they only filter the cached copy). Refreshing
    /// on /skills and /marketplace keeps the suggestion menu and direct
    /// `/name` activation in sync with skills installed after the TUI started.
    fn refresh_skill_catalog(&mut self) {
        self.skill_catalog =
            discover_all_skills(Path::new(&self.bootstrap.cwd), &self.bootstrap.home)
                .unwrap_or_default()
                .into_iter()
                .map(|skill| (skill.name, skill.description))
                .collect();
    }

    fn toggle_skill(&mut self, skill_name: &str) {
        if self.active_skills.iter().any(|skill| skill.name == skill_name) {
            self.active_skills.retain(|skill| skill.name != skill_name);
            self.push_cell(
                TranscriptEntry::Skill(TranscriptSkillEntry { at: now_iso(), enabled: false, name: skill_name.to_string() }),
                true,
            );
            return;
        }

        // Not currently active: enabling is the same path as direct `/name`
        // activation (idempotent, never disables).
        self.enable_skill(skill_name);
    }

    /// Enable a skill for this session without ever disabling it.
    ///
    /// Direct `/navis` activation is idempotent: repeating it never disables
    /// or reloads a skill that is already active. Only `/skill <name>`
    /// toggles a skill off. Enabling a skill never starts a prompt run.
    fn enable_skill(&mut self, skill_name: &str) {
        if self
            .active_skills
            .iter()
            .any(|skill| skill.name == skill_name)
        {
            self.push_info(format!(
                "skill \"{skill_name}\" is already enabled for this session."
            ));
            return;
        }

        let discovered = discover_all_skills(Path::new(&self.bootstrap.cwd), &self.bootstrap.home)
            .unwrap_or_default();
        let Some(skill) = discovered
            .into_iter()
            .find(|candidate| candidate.name == skill_name)
        else {
            self.push_error(format!(
                "No skill named \"{skill_name}\". Try /skills to list what is available."
            ));
            return;
        };

        // Built-in skills ship embedded in the binary (their path is the
        // "<builtin>/..." pseudo-path, with no file on disk), so they must load
        // through the skills loader, which understands that pseudo-path; the
        // roles loader reads project/user/marketplace skills from real files.
        let loaded = if skill.path.starts_with("<builtin>") {
            crate::cli::skills::load_skill_content(&skill, None)
        } else {
            load_skill_content(&skill, None).map_err(|error| error.to_string())
        };
        match loaded {
            Ok(loaded) => {
                self.active_skills.push(loaded);
                self.push_cell(
                    TranscriptEntry::Skill(TranscriptSkillEntry {
                        at: now_iso(),
                        enabled: true,
                        name: skill_name.to_string(),
                    }),
                    true,
                );
            }
            Err(error) => {
                self.push_error(format!("Could not load skill \"{skill_name}\": {error}"))
            }
        }
    }

    /// Try to treat an unbuilt-in slash token as a direct skill activation.
    /// Returns true when `name` matches a discovered skill (now enabled);
    /// false leaves the caller free to report an unknown command.
    fn enable_skill_if_discovered(&mut self, name: &str) -> bool {
        // Case-insensitive so a catalog name with uppercase (e.g. "Navis") can
        // be direct-activated, matching the menu filter and parse_slash_command,
        // which both lowercase the typed token. The canonical catalog spelling
        // wins so active_skills and the load path agree with discovery.
        let canonical = match self
            .skill_catalog
            .iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
        {
            Some((candidate, _)) => candidate.clone(),
            None => return false,
        };
        self.enable_skill(&canonical);
        true
    }

    // ----- commands -------------------------------------------------------

    fn dispatch_command(&mut self, name: &str, args: &str) {
        match name {
            "help" => self.push_info(help_text()),
            "quit" | "exit" => self.quit = true,
            "model" => self.open_overlay(OverlayKind::Model),
            "rename" => self.rename(args),
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
                let records = list_all_sessions(&self.bootstrap.project, Some(15));
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
                // which is surfaced as an error entry.
                let summary = format_state_summary(Path::new(&self.paths.state_path));
                if summary.starts_with("Could not read harness state at ") || summary.starts_with("The file at ") {
                    self.push_error(summary);
                } else {
                    self.push_info(summary);
                }
            }
            "skills" => {
                self.refresh_skill_catalog();
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
            "marketplace" => {
                self.marketplace_command(args);
                self.refresh_skill_catalog();
            }
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
            "praeparare" => {
                // The TUI face of `drip --praeparare`: activate the praeparare
                // skill for the session (built-in pack, or a same-named
                // discovered skill — the same mechanism as --skill) AND submit
                // the shared canned goal through the ordinary run path.
                // Activation alone would leave the run unstarted.
                self.enable_skill_if_discovered("praeparare");
                // Non-empty args are extra operator context, appended through
                // the SAME helper the CLI face uses — never silently dropped
                // (empty/whitespace args keep the bare canned goal).
                let goal = praeparare_goal_with_context(Some(args));
                // No goal-local budget plumbing exists (submit_goal_text
                // carries only text), so mirror the CLI default session-wide:
                // only a still-unset budget becomes 15; an explicit value wins.
                if self.bootstrap.max_iterations.is_none() {
                    self.bootstrap.max_iterations = Some(PRAEPARARE_DEFAULT_MAX_ITERATIONS);
                }
                self.submit_goal_text(goal);
            }
            "env" => self.env_command(args),
            // Not a built-in: a bare token matching a discovered skill (e.g.
            // "/navis") enables that skill for the session. Built-in command
            // names always win because their arms match first, and "/skill"
            // keeps its toggle behavior unchanged.
            _ => {
                // A discovered skill name enables for the session — but only
                // as a lone token. With arguments this is not a skill command,
                // so keep the unknown-command error instead of silently
                // dropping the arguments.
                if !args.trim().is_empty() || !self.enable_skill_if_discovered(name) {
                    self.push_error(format!("Unknown command /{name}. Try /help."));
                }
            }
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
            extra_bindings: self.bootstrap.roles_flag.as_ref().and_then(|flag| flag.bindings.clone()),
            extra_roles: self.bootstrap.roles_flag.as_ref().map(|flag| flag.roles.clone()),
            marketplace_roles: Some(list_enabled_marketplace_roles(cwd, home).unwrap_or_default()),
            skills: discover_all_skills(cwd, home).unwrap_or_default(),
            tool_names: self.tool_names(),
            // Same merged set the CLI validates against (global mcpServers plus
            // <cwd>/.drip/mcp.json), so both callers report the same unknowns.
            mcp_server_names: crate::tools::mcp::config::load_mcp_servers(
                &self.config.mcp_servers,
                std::path::Path::new(&self.bootstrap.cwd),
            )
            .keys()
            .cloned()
            .collect(),
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
            // Insertion-ordered.
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

    /// The built-in pack knobs this session runs with: the network gate and
    /// the oasis corpus roots REFERENCE searches.
    fn tool_options(&self) -> BuiltinToolOptions {
        BuiltinToolOptions {
            allow_net: self.bootstrap.allow_net,
            reference_roots: self.bootstrap.reference_roots.clone(),
        }
    }

    fn tool_names(&self) -> Vec<String> {
        builtin_tool_pack(self.tool_options()).iter().map(|tool| tool.name.clone()).collect()
    }

    // ----- goals ----------------------------------------------------------

    fn run_goal(&mut self, goal_text: String) {
        let goal_images = std::mem::take(&mut self.attachments);
        self.run_session_id = Some(self.session.id.clone());
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
        self.begin_title(&goal_text, &env);
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
            extra_bindings: self.bootstrap.roles_flag.as_ref().and_then(|flag| flag.bindings.clone()),
            extra_roles: self.bootstrap.roles_flag.as_ref().map(|flag| flag.roles.clone()),
            marketplace_roles: Some(list_enabled_marketplace_roles(cwd, &self.bootstrap.home).unwrap_or_default()),
            skills: discover_all_skills(cwd, &self.bootstrap.home).unwrap_or_default(),
            tool_names: self.tool_names(),
            // Same merged set the CLI validates against (global mcpServers plus
            // <cwd>/.drip/mcp.json), so both callers report the same unknowns.
            mcp_server_names: crate::tools::mcp::config::load_mcp_servers(
                &self.config.mcp_servers,
                std::path::Path::new(&self.bootstrap.cwd),
            )
            .keys()
            .cloned()
            .collect(),
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
        let max_loops = self.bootstrap.max_loops;
        let no_repo_memory = self.bootstrap.no_repo_memory;
        let ask_user_enabled = self.bootstrap.ask;
        let ask_user_timeout_seconds = self.bootstrap.ask_timeout_secs;
        let tool_options = self.tool_options();
        let skills = self.active_skills.clone();
        let redact_secrets = load_env_vars(Path::new(&self.bootstrap.home.env_vars_path)).unwrap_or_default();
        let goal_context = resolved.context_block.clone();
        let mentions = resolved.mentions.clone();
        let images: Vec<String> = goal_images.iter().map(|attachment| attachment.data_url.clone()).collect();
        let hooks = self.config.hooks.clone();

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
                ask_user_enabled,
                ask_user_timeout_seconds,
                cwd,
                goal: goal_text,
                goal_context,
                goal_images: if images.is_empty() { None } else { Some(images) },
                hooks,
                index: &index,
                inference,
                max_iterations,
                max_loops,
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
                lite: false,
                no_review: false,
                tools: builtin_tool_pack(tool_options.clone()),
                tool_services: None,
                // The watch view spawns no MCP clients, so its pack carries no
                // MCP tools; None just leaves the (empty) gate to the roles.
                mcp_servers: None,
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

    // ----- terminal pane title --------------------------------------------

    /// Starts a goal run: shows a readable fallback (or the persisted /rename
    /// name) immediately with the spinner running. On an interactive TTY with
    /// the feature enabled the busy spinner is armed for EVERY goal run; only
    /// the background model-label request is one-shot per session, so later
    /// goals reuse the existing title and never re-request a label. TTY-gated:
    /// headless, JSON, and redirected runs never emit escapes or make a model
    /// call.
    fn begin_title(&mut self, goal_text: &str, env: &HashMap<String, String>) {
        self.begin_title_with(goal_text, env, stdout_is_tty());
    }

    /// `is_tty` is injected so lifecycle tests can drive the spinner wiring
    /// without a real terminal.
    fn begin_title_with(
        &mut self,
        goal_text: &str,
        env: &HashMap<String, String>,
        is_tty: bool,
    ) {
        let settings = self.config.settings.clone();
        // An explicit /rename name wins over a generated title: when
        // session.json already carries one, the one-shot auto-title never
        // runs, so the next goal cannot overwrite the user's choice.
        let persisted_name = read_session_name(Path::new(&self.paths.meta_path));
        // The busy spinner is per-run, not per-title-request: every goal on
        // a real terminal spins, even when a persisted /rename name means no
        // auto-title request is made this run (or one was already made).
        // Escapes stay TTY- and feature-gated; headless runs materialize
        // nothing and arm no tick.
        if is_tty && terminal_title_enabled(&settings) {
            let mut title = match self.pane_title.take() {
                Some(title) => title,
                None => {
                    let mut fresh = PaneTitle::new(goal_text);
                    // A fresh title on a session that already carries a
                    // /rename name shows that name, not the goal fallback.
                    if let Some(name) = persisted_name.as_deref() {
                        fresh.set_label(name, Instant::now());
                    }
                    fresh
                }
            };
            if let Some(escape) = title.set_busy(true, Instant::now()) {
                crate::tui::pane_title::emit(Some(&escape));
                self.title_next_tick =
                    Some(Instant::now() + Duration::from_millis(SPINNER_INTERVAL_MS));
            }
            self.pane_title = Some(title);
        }
        if !should_request_auto_title(
            is_tty,
            &settings,
            self.title_requested,
            persisted_name.as_deref(),
        ) {
            return;
        }
        self.title_requested = true;

        // Background, nonblocking, one-shot: the thread only sends a message
        // and never writes title escapes itself.
        let tx = self.tx.clone();
        let epoch = self.title_epoch;
        let goal = goal_text.to_string();
        let env = env.clone();
        let timeout_ms = terminal_title_timeout_ms(&settings);
        std::thread::spawn(move || {
            let label = generate_title_label(settings, env, goal, timeout_ms);
            let _ = tx.send(Msg::Title { epoch, label });
        });
    }

    // ----- /rename ---------------------------------------------------------

    /// `/rename` entry point: with no non-whitespace argument the session is
    /// renamed from its transcript (background model call); with an argument
    /// the user's literal name is applied directly — no profile, transcript,
    /// or generated-name word rules.
    fn rename(&mut self, args: &str) {
        if self.running {
            self.push_error("A goal is running; /rename is disabled until it finishes.");
            return;
        }
        let manual = args.trim();
        if manual.is_empty() {
            self.begin_rename();
        } else {
            self.apply_manual_rename(manual);
        }
    }

    /// Names the session from its transcript with a one-shot model call:
    /// the UI never blocks, the call runs on its own thread, and the reply
    /// lands via Msg::Rename.
    fn begin_rename(&mut self) {
        // A rename takes over the shared pane title: drop any in-flight
        // auto-title reply now so a late Msg::Title cannot clobber the
        // freshly applied name (apply_title_result rejects epoch mismatches).
        self.title_epoch = self.title_epoch.wrapping_add(1);
        let goal = self.session.last_goal.clone().unwrap_or_default();
        let digest = read_session_name_context(&goal, &self.cells);
        let env = self.merged_env();
        let settings = self.config.settings.clone();
        let Some(route) = resolve_session_route(&settings, Some(&env)) else {
            self.push_error("/rename needs a configured inference profile (see /model).");
            return;
        };
        self.rename_epoch = self.rename_epoch.wrapping_add(1);
        let epoch = self.rename_epoch;
        let timeout_ms = terminal_title_timeout_ms(&settings);
        self.push_info("Renaming this session from its transcript…");
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let name = generate_rename_name(route, &goal, &digest, timeout_ms);
            let _ = tx.send(Msg::Rename { epoch, name });
        });
    }

    /// `/rename <name>`: persist and display the user's literal name verbatim.
    /// Runs on the UI thread after bumping both epochs, so an in-flight
    /// auto-rename or auto-title reply is stale by the time it arrives and
    /// can never overwrite the manual name on screen or on disk.
    fn apply_manual_rename(&mut self, name: &str) {
        self.title_epoch = self.title_epoch.wrapping_add(1);
        self.rename_epoch = self.rename_epoch.wrapping_add(1);
        if !persist_session_name(Path::new(&self.paths.meta_path), name) {
            self.push_error("Could not save the new session name; keeping the current one.");
            return;
        }
        apply_rename_label(&mut self.pane_title, name, stdout_is_tty());
        self.push_info(format!("Session renamed to \"{name}\"."));
    }

    /// Applies a background /rename result: stale epochs (from /new or /resume
    /// while the call was in flight) are dropped, the visible pane title is
    /// updated, and any failure keeps the current name.
    fn apply_rename_result(&mut self, msg_epoch: u64, name: Option<String>) {
        if msg_epoch != self.rename_epoch {
            return;
        }
        let Some(name) = name else {
            self.push_error("Could not generate a session name; keeping the current one.");
            return;
        };
        // Acceptance and persistence both happen here on the UI thread after
        // the epoch guard: a stale auto-rename reply can never write the
        // stored session name behind a newer rename's back.
        if !persist_session_name(Path::new(&self.paths.meta_path), &name) {
            self.push_error("Could not generate a session name; keeping the current one.");
            return;
        }
        apply_rename_label(&mut self.pane_title, &name, stdout_is_tty());
        self.push_info(format!("Session renamed to \"{name}\"."));
    }

    fn finish_run(&mut self) {
        self.abort = None;
        self.pending_detail = None;
        // The asking run is over — an unanswered survey has no one to answer to.
        self.drop_survey("the run ended before the survey was answered — resume to be asked again");
        self.flush_pending_cells();
        // Run end is a visible boundary: settle any still-open tool group
        // into scrollback exactly once (completion, cancel, or error).
        self.finalize_compact();
        self.running = false;
        self.running_detail = None;
        // Idle title (bare label, no spinner) whatever ended the run:
        // completion, cancel, or error.
        self.title_next_tick = None;
        if let Some(title) = self.pane_title.as_mut() {
            let escape = title.set_busy(false, Instant::now());
            crate::tui::pane_title::emit(escape.as_deref());
        }
    }

    // ----- main loop ------------------------------------------------------

    fn run(&mut self, rx: Receiver<Msg>) -> i32 {
        // SAFETY: installing async-signal-safe handlers that only store flags.
        unsafe {
            libc::signal(libc::SIGWINCH, on_winch as *const () as libc::sighandler_t);
            libc::signal(libc::SIGINT, on_halt as *const () as libc::sighandler_t);
            libc::signal(libc::SIGTERM, on_halt as *const () as libc::sighandler_t);
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

            // Pane-title spinner: consume a due tick here, never in the paint
            // path. The deadline is taken before ticking so a throttled frame
            // cannot leave a past deadline behind, and it is re-armed only
            // while the title is busy - idle ticks never re-arm, so the loop
            // falls back to its plain wait instead of busy-polling.
            if let Some(at) = self.title_next_tick {
                if now >= at {
                    self.title_next_tick = None;
                    let escape = self.pane_title.as_mut().and_then(|title| title.tick(now));
                    crate::tui::pane_title::emit(escape.as_deref());
                    if should_rearm_title_tick(self.pane_title.as_ref()) {
                        self.title_next_tick =
                            Some(Instant::now() + Duration::from_millis(SPINNER_INTERVAL_MS));
                    }
                }
            }

            // Custom status line: adopt finished jobs and re-arm the
            // interval refresh here, never in the paint path (drawing stays
            // side-effect-free). Non-blocking; failures never retry or log.
            self.poll_status_line();

            let mut wait = Duration::from_millis(300);
            for deadline in [
                self.flush_deadline,
                self.resize_at,
                self.status_line_next_refresh,
                self.title_next_tick,
            ]
            .into_iter()
            .flatten()
            {
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
                    // ask_user surveys open the staged answer overlay; the
                    // harness thread is blocked on answers.jsonl meanwhile.
                    if event.r#type == HarnessEventType::Question
                        // A late event from a run that started before a session
                        // switch must not bind a survey to the new session.
                        && self.run_session_id.as_deref() == Some(self.session.id.as_str())
                    {
                        if let Some(survey) =
                            event.data.as_ref().and_then(|data| data.question_survey.clone())
                        {
                            self.begin_survey(survey);
                        }
                    }
                    // The survey was satisfied elsewhere (drip --answer):
                    // close the overlay instead of collecting a dead batch.
                    // Gated on the accept event's full identity, not just the
                    // payload field, so no future survey-answers-bearing event
                    // can close a live overlay by accident.
                    if event.r#type == HarnessEventType::HarnessOp
                        && event.data.as_ref().is_some_and(|data| {
                            data.survey_answers.is_some()
                                && data.tool_name.as_deref() == Some("ask_user")
                        })
                    {
                        self.drop_survey("survey answered via drip --answer — the run continues");
                    }
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
                Ok(Msg::Title { epoch, label }) => {
                    let escape = apply_title_result(
                        self.pane_title.as_mut(),
                        self.title_epoch,
                        epoch,
                        label,
                        Instant::now(),
                    );
                    crate::tui::pane_title::emit(escape.as_deref());
                }
                Ok(Msg::Rename { epoch, name }) => {
                    self.apply_rename_result(epoch, name);
                    self.repaint();
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
        // Leave an idle title on exit (quit, ctrl-c, or halt): bare label,
        // no spinner left behind.
        if let Some(title) = self.pane_title.as_mut() {
            let escape = title.set_busy(false, Instant::now());
            crate::tui::pane_title::emit(escape.as_deref());
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
fn stdout_is_tty() -> bool {
    // SAFETY: isatty on a fixed descriptor.
    unsafe { libc::isatty(libc::STDOUT_FILENO) == 1 }
}

/// Gate for the one-shot background title request: interactive TTY only,
/// config-enabled, and not already requested this session. Pure so lifecycle
/// tests can cover the decision without a terminal.
fn should_request_title(
    is_tty: bool,
    settings: &indexmap::IndexMap<String, String>,
    requested: bool,
) -> bool {
    is_tty && terminal_title_enabled(settings) && !requested
}

/// Whether the next goal may request the one-shot auto-title: the usual
/// TTY/enabled/one-shot gate, and never when an explicit /rename name is
/// persisted — the user's chosen name wins over a generated one.
fn should_request_auto_title(
    is_tty: bool,
    settings: &indexmap::IndexMap<String, String>,
    requested: bool,
    persisted_name: Option<&str>,
) -> bool {
    persisted_name.is_none() && should_request_title(is_tty, settings, requested)
}

/// Applies a /rename (or a restored session name) to the visible pane title:
/// updates the existing title, or materializes one on a real terminal when
/// no goal has created it yet. Headless runs never emit title escapes.
fn apply_rename_label(pane_title: &mut Option<PaneTitle>, name: &str, is_tty: bool) {
    if let Some(title) = pane_title.as_mut() {
        let escape = title.set_label(name, Instant::now());
        crate::tui::pane_title::emit(escape.as_deref());
    } else if is_tty {
        let mut title = PaneTitle::new(name);
        let escape = title.set_label(name, Instant::now());
        crate::tui::pane_title::emit(escape.as_deref());
        *pane_title = Some(title);
    }
}

/// Whether a consumed title tick re-arms the spinner deadline: only while a
/// title exists and is busy. Idle and headless states never re-arm, so the
/// event loop falls back to its plain wait instead of busy-polling.
fn should_rearm_title_tick(title: Option<&PaneTitle>) -> bool {
    title.map_or(false, PaneTitle::is_busy)
}

/// The single bounded, non-tool inference request for a short chat title.
/// Missing credentials, offline restrictions, timeouts, and malformed output
/// all return None so the sanitized fallback stays; this never fails the
/// chat and stays outside conversation/history state.
fn generate_title_label(
    settings: indexmap::IndexMap<String, String>,
    env: HashMap<String, String>,
    goal: String,
    timeout_ms: u64,
) -> Option<String> {
    let route = resolve_title_route(&settings, Some(&env))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    runtime.block_on(generate_chat_title(route, &goal, timeout_ms))
}

/// Thread-side /rename half: resolve through the shared session route and run
/// the one-shot naming request; every failure is None.
fn generate_rename_name(
    route: crate::harness::model_call::ModelRoute,
    goal: &str,
    digest: &str,
    timeout_ms: u64,
) -> Option<String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    runtime.block_on(generate_session_title(route, goal, digest, timeout_ms))
}

/// Applies a background title result: only the current session's epoch is
/// accepted, and a label identical to the current title dedupes to nothing.
fn apply_title_result(
    title: Option<&mut PaneTitle>,
    current_epoch: u64,
    msg_epoch: u64,
    label: Option<String>,
    now: Instant,
) -> Option<String> {
    if msg_epoch != current_epoch {
        return None;
    }
    let title = title?;
    match label {
        Some(raw) => title.set_label(&raw, now),
        None => None,
    }
}

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
mod prompt_history_tests {
    use super::PromptHistory;

    fn history(entries: &[&str]) -> PromptHistory {
        let mut history = PromptHistory::new(8);
        for entry in entries {
            history.record(entry);
        }
        history
    }

    #[test]
    fn empty_history_never_recalls_and_never_saves_a_draft() {
        let mut history = PromptHistory::new(8);
        assert_eq!(history.older("current draft"), None);
        assert_eq!(history.newer(), None);
        assert_eq!(history.len(), 0);
    }

    #[test]
    fn up_starts_at_newest_and_repeated_up_clamps_at_oldest() {
        let mut history = history(&["first", "second", "third"]);
        assert_eq!(history.older("").as_deref(), Some("third"));
        assert_eq!(history.older("").as_deref(), Some("second"));
        assert_eq!(history.older("").as_deref(), Some("first"));
        // Clamped: still the oldest entry, never wrapping or panicking.
        assert_eq!(history.older("").as_deref(), Some("first"));
    }

    #[test]
    fn down_moves_newer_then_restores_the_exact_draft() {
        let mut history = history(&["one", "two"]);
        assert_eq!(history.older("draft text").as_deref(), Some("two"));
        assert_eq!(history.older("").as_deref(), Some("one"));
        assert_eq!(history.newer().as_deref(), Some("two"));
        // Stepping past the newest restores the draft saved at navigation
        // start, byte for byte.
        assert_eq!(history.newer().as_deref(), Some("draft text"));
        // Down outside navigation is a no-op.
        assert_eq!(history.newer(), None);
    }

    #[test]
    fn multiline_entries_and_drafts_are_kept_verbatim() {
        let mut history = PromptHistory::new(8);
        history.record("line one\nline two\n\nline four");
        assert_eq!(
            history.older("a\nb\nc").as_deref(),
            Some("line one\nline two\n\nline four")
        );
        assert_eq!(history.newer().as_deref(), Some("a\nb\nc"));
    }

    #[test]
    fn blank_and_consecutive_duplicate_prompts_are_not_recorded() {
        let mut history = PromptHistory::new(8);
        history.record("   ");
        history.record("\n\t");
        assert_eq!(history.older(""), None);
        assert_eq!(history.len(), 0);

        history.record("same");
        history.record("same");
        assert_eq!(history.len(), 1);

        // A non-consecutive duplicate is kept.
        history.record("other");
        history.record("same");
        assert_eq!(history.len(), 3);
        assert_eq!(history.older("").as_deref(), Some("same"));
        assert_eq!(history.older("").as_deref(), Some("other"));
    }

    #[test]
    fn history_is_bounded_to_the_capacity_oldest_dropped_first() {
        let mut history = PromptHistory::new(3);
        for n in 0..5 {
            history.record(&format!("p{n}"));
        }
        assert_eq!(history.len(), 3);
        assert_eq!(history.older("").as_deref(), Some("p4"));
        assert_eq!(history.older("").as_deref(), Some("p3"));
        assert_eq!(history.older("").as_deref(), Some("p2"));
    }

    #[test]
    fn editing_a_recalled_prompt_never_mutates_the_stored_entry() {
        let mut history = history(&["original"]);
        let recalled = history.older("").expect("recall");
        let mut edited = recalled;
        edited.push_str(" plus an edit");
        // The stored entry is untouched by the caller's edit...
        history.reset();
        assert_eq!(history.older("").as_deref(), Some("original"));
        // ...and walking navigation again still returns the stored text.
        let again = history.older("").expect("recall again");
        assert_eq!(again, "original");
    }

    #[test]
    fn recording_a_new_prompt_resets_navigation_and_draft() {
        let mut history = history(&["one", "two"]);
        assert_eq!(history.older("draft").as_deref(), Some("two"));
        history.record("three");
        // Navigation ended: Down is a no-op and the old draft is gone.
        assert_eq!(history.newer(), None);
        assert_eq!(history.older("fresh").as_deref(), Some("three"));
    }

    #[test]
    fn reset_discards_navigation_and_the_saved_draft() {
        let mut history = history(&["one"]);
        assert!(history.older("draft").is_some());
        history.reset();
        assert_eq!(history.newer(), None);
        // History itself survives the reset; only navigation state clears.
        assert_eq!(history.len(), 1);
        assert_eq!(history.older("").as_deref(), Some("one"));
    }
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

#[cfg(test)]
mod status_line_tui_tests {
    use super::*;

    fn output(line: &str, ok: bool) -> crate::tui::status_line::StatusLineOutput {
        crate::tui::status_line::StatusLineOutput {
            line: line.to_string(),
            ok,
            fresh: true,
            finished_at: Instant::now(),
        }
    }

    fn bare_request() -> crate::tui::status_line::StatusLineRequest {
        crate::tui::status_line::StatusLineRequest {
            session_id: None,
            cwd: None,
            model_id: None,
            model_display_name: None,
            version: None,
            render_width_chars: 80,
            context_usage: None,
        }
    }

    fn command_setting(command: &str) -> crate::core::config::StatusLineSetting {
        crate::core::config::StatusLineSetting {
            kind: "command".to_string(),
            command: command.to_string(),
            padding: 0,
            update_interval_ms: 300,
            timeout_ms: 5_000,
        }
    }

    #[test]
    fn no_custom_output_keeps_default_bar() {
        // Not configured, or configured but nothing finished yet: default bar.
        assert!(custom_status_row_from(None, 80, 0).is_none());
        assert!(custom_status_row_from(Some(&output("", true)), 80, 0).is_none());
    }

    #[test]
    fn failed_or_timed_out_output_falls_back_to_default_bar() {
        assert!(custom_status_row_from(Some(&output("ignored", false)), 80, 0).is_none());
    }

    #[test]
    fn stale_success_row_is_dropped_on_any_later_failure_or_blank() {
        // The runner delivers every finished job; the renderer keeps only the
        // newest: a success paints, but any later failure, timeout, blank, or
        // missing output renders the built-in bar, never a stale custom row.
        assert!(custom_status_row_from(Some(&output("current", true)), 40, 0).is_some());
        assert!(custom_status_row_from(Some(&output("stale", false)), 40, 0).is_none());
        assert!(custom_status_row_from(Some(&output("", true)), 40, 0).is_none());
        assert!(custom_status_row_from(None, 40, 0).is_none());
    }

    #[test]
    fn custom_ansi_output_is_exact_width_with_reset() {
        let row =
            custom_status_row_from(Some(&output("\x1b[32mok\x1b[0m", true)), 10, 0).unwrap();
        assert!(row.starts_with("\x1b[32mok\x1b[0m"), "{row:?}");
        assert_eq!(string_width(&row), 10);
        assert!(row.ends_with("\x1b[0m"));
    }

    #[test]
    fn custom_row_survives_narrow_unicode_and_padding() {
        let wide = custom_status_row_from(Some(&output("コード drip", true)), 4, 0).unwrap();
        assert_eq!(string_width(&wide), 4);
        // Width>0 pads to exactly the requested cells; padding 2 shows as two
        // leading spaces before the content.
        let row = custom_status_row_from(Some(&output("ab", true)), 80, 2).unwrap();
        assert_eq!(string_width(&row), 80);
        assert!(row.starts_with("  ab"), "{row:?}");
    }

    #[test]
    fn shutdown_runner_ignores_refresh_requests() {
        let mut runner =
            crate::tui::status_line::StatusLineRunner::new(command_setting("true"));
        runner.shutdown();
        assert!(!runner.request_refresh(bare_request()));
    }
}

#[cfg(test)]
mod pane_title_lifecycle_tests {
    use super::*;
    use crate::tui::pane_title::fallback_title;

    fn enabled_settings() -> indexmap::IndexMap<String, String> {
        crate::core::config::default_setting_values()
    }

    #[test]
    fn title_request_gate_is_tty_enabled_and_one_shot() {
        let settings = enabled_settings();
        assert!(should_request_title(true, &settings, false));
        // Headless, redirected, or worker children: never request.
        assert!(!should_request_title(false, &settings, false));
        // Explicit opt-out disables the request entirely.
        let mut off = settings.clone();
        off.insert(
            crate::core::config::TERMINAL_TITLE_ENABLED_SETTING_ID.to_string(),
            "false".to_string(),
        );
        assert!(!should_request_title(true, &off, false));
        // Exactly one request per session.
        assert!(!should_request_title(true, &settings, true));
    }

    #[test]
    fn generated_label_replaces_fallback_with_sanitized_bounded_escape() {
        let mut title = PaneTitle::new("fix the login bug");
        let escape = apply_title_result(
            Some(&mut title),
            0,
            0,
            Some("\x1b]2;pwn\x07fix the login bug and the signup flow too".to_string()),
            Instant::now(),
        )
        .expect("escape expected");
        assert!(escape.starts_with("\x1b]2;") && escape.ends_with('\x07'));
        assert!(!escape.contains("pwn"));
        assert!(title.label().split_whitespace().count() <= 5);
    }

    #[test]
    fn stale_generation_result_from_previous_session_is_rejected() {
        // switch_session bumps the epoch before the old session's result lands.
        let mut title = PaneTitle::new("old goal");
        assert!(apply_title_result(
            Some(&mut title),
            1,
            0,
            Some("stale title".to_string()),
            Instant::now(),
        )
        .is_none());
        assert_eq!(title.label(), fallback_title("old goal"));
    }

    #[test]
    fn failed_generation_keeps_fallback_and_headless_title_is_none() {
        // Generation failure (None label): the fallback label survives.
        let mut title = PaneTitle::new("my goal");
        assert!(apply_title_result(Some(&mut title), 0, 0, None, Instant::now()).is_none());
        assert_eq!(title.label(), fallback_title("my goal"));
        // No pane title at all (headless/no-TTY): results are dropped safely.
        assert!(apply_title_result(None, 0, 0, Some("unused".to_string()), Instant::now()).is_none());
    }

    #[test]
    fn busy_ticks_produce_distinct_spinner_frames() {
        // The main loop's tick consumption must actually animate the busy
        // title: two due ticks, one spinner interval apart, emit different
        // frames around the same stable label.
        let mut title = PaneTitle::new("fix the login bug");
        let start = Instant::now();
        assert!(title.set_busy(true, start).is_some());
        let step = Duration::from_millis(SPINNER_INTERVAL_MS);
        let first = title.tick(start + step).expect("first due tick emits");
        let second = title.tick(start + 2 * step).expect("second due tick emits");
        assert_ne!(first, second, "spinner frames must advance between ticks");
        assert!(first.starts_with("\x1b]2;") && first.ends_with('\x07'));
        assert!(second.ends_with("fix the login bug\x07"));
    }

    #[test]
    fn idle_ticks_never_rearm_the_spinner_deadline() {
        // A fired tick while idle must not re-arm the deadline: the re-arm
        // gate is busy-only, so title ticks can never drive the event loop
        // into a zero-wait busy poll.
        let at = Instant::now();
        let step = Duration::from_millis(SPINNER_INTERVAL_MS);
        // Headless (no pane title): nothing to re-arm.
        assert!(!should_rearm_title_tick(None));
        // A title that never went busy: its ticks are inert.
        let mut idle = PaneTitle::new("another goal");
        assert!(idle.tick(at).is_none());
        assert!(!should_rearm_title_tick(Some(&idle)));
        // The finish_run path: busy then idled — later ticks stay inert and
        // the gate stays closed.
        let mut done = PaneTitle::new("finish the run");
        assert!(done.set_busy(true, at).is_some());
        assert!(done.set_busy(false, at + step).is_some());
        assert!(done.tick(at + 2 * step).is_none());
        assert!(!should_rearm_title_tick(Some(&done)));
        // The only live re-arm case: a title that is busy.
        let mut busy = PaneTitle::new("run the goal");
        assert!(busy.set_busy(true, at).is_some());
        assert!(should_rearm_title_tick(Some(&busy)));
    }

    #[test]
    fn offline_or_missing_profile_title_generation_silently_returns_none() {
        // Disabled: route resolution refuses before any network work.
        let mut off = enabled_settings();
        off.insert(
            crate::core::config::TERMINAL_TITLE_ENABLED_SETTING_ID.to_string(),
            "false".to_string(),
        );
        assert!(generate_title_label(off, HashMap::new(), "goal".into(), 1000).is_none());
        // Enabled but pointing at a profile that does not exist: silent None,
        // deterministic offline, no request is ever attempted.
        let mut nop = enabled_settings();
        nop.insert(
            crate::core::config::TERMINAL_TITLE_PROFILE_SETTING_ID.to_string(),
            "no-such-profile".to_string(),
        );
        assert!(generate_title_label(nop, HashMap::new(), "goal".into(), 1000).is_none());
    }

    #[test]
    fn title_escapes_and_custom_status_line_coexist_sanitized() {
        // The OSC 2 pane-title stream and the custom status line sanitize
        // independently: neither leaks control sequences into the other.
        // The label is sanitized BEFORE wrapping, so the escape carries a
        // clean payload; the status-line sanitizer runs on the plain label
        // text and strips any residual control sequences.
        let raw = "\x1b]2;pwn\x07 \u{24b8}fix\tlogin\nbug\x1b[2J";
        let label = crate::tui::pane_title::sanitize(raw);
        assert!(!label.contains('\x1b') && !label.contains('\x07'), "{label:?}");
        let title = crate::tui::pane_title::osc2(&label);
        assert!(title.starts_with("\x1b]2;") && title.ends_with('\x07'), "{title:?}");
        assert!(!title.contains("pwn") && !title.contains("\x1b[2J"), "{title:?}");
        let row = crate::tui::status_line::sanitize_status_line(&label, 20, 0);
        assert!(row.contains("fix login bug"), "{row:?}");
        assert!(!row.contains('\x1b') && !row.contains('\x07'), "{row:?}");
        assert!(row.chars().count() <= 20);
    }
}

/// Focused tests for the /rename wiring: command recognition, busy-session
/// refusal, failure keeping the current title, stale-epoch protection, and
/// resume restoring the persisted name.
#[cfg(test)]
mod rename_tests {
    use super::*;

    /// A throwaway DRIP_HOME removed on drop.
    struct TempHome(std::path::PathBuf);

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp_dir(kind: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("drip-rename-{kind}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// A TuiApp whose project + session live in an isolated temp home.
    fn rename_app(dir: &Path) -> TuiApp {
        let root = dir.to_string_lossy().to_string();
        let home = crate::core::home::open_drip_home(&root);
        let project = crate::core::home::resolve_drip_project("/tmp", &root, None)
            .expect("project resolves");
        let project = crate::core::home::ensure_drip_project(&project);
        let index = crate::core::sessions::open_session_index(&project.index_db_path);
        let session = crate::core::sessions::create_session(
            &index,
            crate::core::sessions::CreateSessionArgs {
                cwd: "/tmp".to_string(),
                project: &crate::core::sessions::ProjectPaths::from(&project),
                now: "",
            },
        );
        index.close();
        let bootstrap = TuiBootstrap {
            ask: false,
            ask_timeout_secs: None,
            allow_net: false,
            reference_roots: Vec::new(),
            config: crate::core::config::create_default_cli_config(),
            cwd: "/tmp".to_string(),
            home,
            initial_goal: Some("ship the release".to_string()),
            max_iterations: None,
            max_loops: None,
            no_repo_memory: false,
            project,
            roles_flag: None,
            session,
            status_line: None,
        };
        let (tx, _rx) = mpsc::channel::<Msg>();
        let (mention_tx, _mention_rx) = mpsc::channel::<(u64, String)>();
        TuiApp::new(bootstrap, tx, mention_tx)
    }

    fn label(app: &TuiApp) -> String {
        app.pane_title
            .as_ref()
            .expect("pane title exists")
            .label()
            .to_string()
    }

    #[test]
    fn auto_title_yields_to_a_persisted_rename_name() {
        let settings = crate::core::config::default_setting_values();
        // An explicit /rename name wins: no auto-title even on a fresh,
        // TTY-visible, enabled session.
        assert!(!should_request_auto_title(true, &settings, false, Some("User Chosen Name")));
        // Without one, the usual gate applies unchanged.
        assert!(should_request_auto_title(true, &settings, false, None));
        assert!(!should_request_auto_title(false, &settings, false, None));
        assert!(!should_request_auto_title(true, &settings, true, None));
    }

    #[test]
    fn rename_attempt_drops_in_flight_auto_title_replies() {
        let dir = temp_dir("stale-title");
        let _home = TempHome(dir.clone());
        let mut app = rename_app(&dir);
        app.pane_title = Some(PaneTitle::new("ship the release"));
        let before = app.title_epoch;
        // No profile is configured, so begin_rename refuses after the epoch
        // bump — the bump guards every attempt, not just scheduled ones.
        app.begin_rename();
        assert_eq!(app.title_epoch, before.wrapping_add(1));
        assert_eq!(app.rename_epoch, 0);
        // The late auto-title reply for the old epoch is rejected and the
        // current label survives.
        assert!(apply_title_result(
            app.pane_title.as_mut(),
            before,
            app.title_epoch,
            Some("Late Auto Title".to_string()),
            Instant::now(),
        )
        .is_none());
        assert_eq!(
            label(&app),
            crate::tui::pane_title::fallback_title("ship the release")
        );
    }

    #[test]
    fn rename_label_materializes_a_title_only_on_a_tty() {
        // Before any goal there is no pane title: on a real terminal the
        // chosen name materializes one...
        let mut pane_title: Option<PaneTitle> = None;
        apply_rename_label(&mut pane_title, "Alpha Beta Gamma", true);
        let title = pane_title.expect("materialized on a tty");
        assert_eq!(title.label(), "Alpha Beta Gamma");
        // ...headless runs never materialize one (no escapes on piped
        // stdout).
        let mut pane_title: Option<PaneTitle> = None;
        apply_rename_label(&mut pane_title, "Alpha Beta Gamma", false);
        assert!(pane_title.is_none());
    }

    #[test]
    fn rename_command_is_recognized_and_busy_sessions_are_refused() {
        let dir = temp_dir("dispatch");
        let _home = TempHome(dir.clone());
        let mut app = rename_app(&dir);
        // /rename is recognized (it never starts a goal). With no inference
        // profile configured it refuses before scheduling anything.
        app.dispatch_command("rename", "");
        assert!(!app.running);
        assert_eq!(app.rename_epoch, 0);
        // A busy session refuses /rename without scheduling another rename.
        app.running = true;
        app.dispatch_command("rename", "");
        assert!(app.running);
        assert_eq!(app.rename_epoch, 0);
    }

    #[test]
    fn rename_failure_keeps_the_current_title() {
        let dir = temp_dir("failure");
        let _home = TempHome(dir.clone());
        let mut app = rename_app(&dir);
        app.pane_title = Some(PaneTitle::new("ship the release"));
        let before = label(&app);
        // A failed generation keeps the current name...
        app.apply_rename_result(0, None);
        assert_eq!(label(&app), before);
        // ...and so does a success that could not be persisted: point the
        // metadata path at a parent that is a regular file.
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, b"not a directory").unwrap();
        app.paths.meta_path = blocker.join("session.json").to_string_lossy().into_owned();
        app.apply_rename_result(0, Some("Alpha Beta Gamma".to_string()));
        assert_eq!(label(&app), before);
    }

    #[test]
    fn rename_success_updates_the_title_and_stale_epochs_are_ignored() {
        let dir = temp_dir("success");
        let _home = TempHome(dir.clone());
        let mut app = rename_app(&dir);
        app.pane_title = Some(PaneTitle::new("ship the release"));
        let before = label(&app);
        let name = "Ship the release candidate today";
        app.apply_rename_result(0, Some(name.to_string()));
        let after = label(&app);
        assert_eq!(after, name);
        assert_ne!(after, before);
        // A late reply from an earlier epoch (the session changed meanwhile)
        // is dropped instead of clobbering the live title.
        app.apply_rename_result(99, Some("Totally Stale Name Here".to_string()));
        assert_eq!(label(&app), after);
    }

    #[test]
    fn resume_restores_the_persisted_rename() {
        let dir = temp_dir("resume");
        let _home = TempHome(dir.clone());
        let mut app = rename_app(&dir);
        app.pane_title = Some(PaneTitle::new("ship the release"));
        let session = app.session.clone();
        app.switch_session(session.clone());
        let fallback = label(&app);
        let name = "Ship the release candidate today";
        let meta = std::path::PathBuf::from(&app.paths.meta_path);
        assert!(crate::tui::session_name::persist_session_name(&meta, name));
        app.switch_session(session);
        let restored = label(&app);
        assert_eq!(restored, name, "resume must restore the persisted rename");
    }

    #[test]
    fn switching_sessions_drops_in_flight_renames() {
        let dir = temp_dir("switch-epoch");
        let _home = TempHome(dir.clone());
        let mut app = rename_app(&dir);
        app.pane_title = Some(PaneTitle::new("ship the release"));
        // A /rename issued in session A is in flight: the worker captured
        // epoch 1 before the user switches away.
        app.rename_epoch = 1;
        let stale_name = "Session A Name Right Here";

        // /new routes through switch_session, which must invalidate the
        // in-flight rename so its late reply cannot clobber the new label.
        app.dispatch_command("new", "");
        assert_eq!(app.rename_epoch, 2, "/new must bump the rename epoch");
        let fresh = label(&app);
        app.apply_rename_result(1, Some(stale_name.to_string()));
        assert_ne!(label(&app), stale_name, "stale rename must not clobber /new");
        assert_eq!(label(&app), fresh);

        // /resume is the other switch path; the same protection applies.
        let index = crate::core::sessions::open_session_index(&app.bootstrap.project.index_db_path);
        let other = crate::core::sessions::create_session(
            &index,
            crate::core::sessions::CreateSessionArgs {
                cwd: app.bootstrap.cwd.clone(),
                project: &crate::core::sessions::ProjectPaths::from(&app.bootstrap.project),
                now: "",
            },
        );
        index.close();
        app.rename_epoch = 10;
        app.dispatch_command("resume", &other.id);
        assert_eq!(app.rename_epoch, 11, "/resume must bump the rename epoch");
        app.apply_rename_result(10, Some(stale_name.to_string()));
        assert_eq!(label(&app), FALLBACK_LABEL, "stale rename must not clobber /resume");
    }

    #[test]
    fn manual_renames_without_model_configuration() {
        let dir = temp_dir("manual");
        let _home = TempHome(dir.clone());
        let mut app = rename_app(&dir);
        app.pane_title = Some(PaneTitle::new("ship the release"));
        // A short manual name works with no inference profile: no model call,
        // no transcript, no generated-name word rules.
        app.dispatch_command("rename", "Ops");
        assert_eq!(app.rename_epoch, 1, "manual rename bumps the epoch");
        assert_eq!(label(&app), "Ops");
        let meta = std::path::PathBuf::from(&app.paths.meta_path);
        assert_eq!(
            crate::tui::session_name::read_session_name(&meta).as_deref(),
            Some("Ops"),
            "manual name must persist verbatim"
        );
        // A multiword name keeps its interior spacing (trimmed only at the
        // ends) and is not held to the generated 5-7 word rule.
        let multiword = "Ops: incident 42 (follow-up)";
        app.dispatch_command("rename", &format!("  {multiword}  "));
        assert_eq!(app.rename_epoch, 2);
        assert_eq!(label(&app), multiword);
        assert_eq!(
            crate::tui::session_name::read_session_name(&meta).as_deref(),
            Some(multiword)
        );
        // Resume (switch away and back) restores the manual name verbatim.
        let session = app.session.clone();
        app.switch_session(session);
        assert_eq!(label(&app), multiword, "resume must restore the manual name");
    }

    #[test]
    fn busy_sessions_refuse_manual_renames() {
        let dir = temp_dir("manual-busy");
        let _home = TempHome(dir.clone());
        let mut app = rename_app(&dir);
        app.pane_title = Some(PaneTitle::new("ship the release"));
        app.running = true;
        app.dispatch_command("rename", "Ops");
        assert!(app.running);
        assert_eq!(app.rename_epoch, 0, "busy refusal must not bump the epoch");
        assert_eq!(label(&app), "ship the release");
        let meta = std::path::PathBuf::from(&app.paths.meta_path);
        assert_eq!(crate::tui::session_name::read_session_name(&meta), None);
    }

    #[test]
    fn whitespace_only_args_route_to_auto_rename() {
        let dir = temp_dir("manual-blank");
        let _home = TempHome(dir.clone());
        let mut app = rename_app(&dir);
        app.pane_title = Some(PaneTitle::new("ship the release"));
        let before = label(&app);
        // Whitespace-only args are not a manual name: they route to the auto
        // path, which without a profile refuses before any epoch bump or
        // persistence.
        app.dispatch_command("rename", "   ");
        assert_eq!(app.rename_epoch, 0);
        assert_eq!(label(&app), before);
        let meta = std::path::PathBuf::from(&app.paths.meta_path);
        assert_eq!(crate::tui::session_name::read_session_name(&meta), None);
    }

    #[test]
    fn manual_rename_beats_stale_auto_results() {
        let dir = temp_dir("manual-stale");
        let _home = TempHome(dir.clone());
        let mut app = rename_app(&dir);
        app.pane_title = Some(PaneTitle::new("ship the release"));
        // An auto /rename is in flight (its worker captured epoch 0), then
        // the user applies a manual name, which bumps both epochs on the UI
        // thread before anything else can land.
        app.dispatch_command("rename", "Ops");
        assert_eq!(app.rename_epoch, 1);
        // The stale auto-rename reply arrives: dropped by the epoch guard,
        // leaving the manual name on screen AND on disk.
        app.apply_rename_result(0, Some("Auto Generated Name Right Here".to_string()));
        assert_eq!(label(&app), "Ops");
        let meta = std::path::PathBuf::from(&app.paths.meta_path);
        assert_eq!(
            crate::tui::session_name::read_session_name(&meta).as_deref(),
            Some("Ops"),
            "stale auto rename must not clobber the persisted manual name"
        );
        // A stale auto-title reply is likewise rejected after the bump.
        if let Some(pt) = app.pane_title.as_mut() {
            assert!(apply_title_result(
                Some(pt),
                1,
                0,
                Some("Late Auto Title".to_string()),
                std::time::Instant::now()
            )
            .is_none());
        }
        assert_eq!(label(&app), "Ops");
    }

    #[test]
    fn persisted_rename_name_still_spins_and_settles_across_runs() {
        let dir = temp_dir("busy-spinner");
        let _home = TempHome(dir.clone());
        let mut app = rename_app(&dir);
        app.pane_title = Some(PaneTitle::new("ship the release"));
        app.dispatch_command("rename", "Ops");
        assert_eq!(label(&app), "Ops");

        // A goal on a session with a persisted manual name: the one-shot
        // auto-title is suppressed, but the spinner must still start...
        app.begin_title_with("second goal", &HashMap::new(), true);
        let title = app.pane_title.as_ref().expect("title exists");
        assert!(title.is_busy(), "spinner must run even with a manual name");
        assert!(app.title_next_tick.is_some(), "spinner tick must be armed");
        assert_eq!(title.label(), "Ops", "manual name keeps the label");
        // ...tick while busy...
        let escape = app.pane_title.as_mut().and_then(|title| {
            title.tick(Instant::now() + Duration::from_millis(2 * SPINNER_INTERVAL_MS))
        });
        assert!(escape.is_some(), "a due tick advances the spinner");
        // ...and the run end settles busy -> idle with the name intact.
        app.finish_run();
        let title = app.pane_title.as_ref().expect("title exists");
        assert!(!title.is_busy());
        assert_eq!(title.label(), "Ops");
        assert!(app.title_next_tick.is_none());

        // The next goal spins again on the same label, then settles again.
        app.begin_title_with("third goal", &HashMap::new(), true);
        assert!(app.pane_title.as_ref().expect("title exists").is_busy());
        app.finish_run();
        assert!(!app.pane_title.as_ref().expect("title exists").is_busy());
        assert_eq!(label(&app), "Ops");
    }

    #[test]
    fn headless_runs_never_materialize_a_title_or_arm_the_spinner() {
        let dir = temp_dir("busy-headless");
        let _home = TempHome(dir.clone());
        let mut app = rename_app(&dir);
        // Tests run headless (no TTY): begin_title materializes nothing and
        // arms no tick, and never spawns the auto-title worker.
        app.begin_title("first goal", &HashMap::new());
        assert!(app.pane_title.is_none());
        assert!(app.title_next_tick.is_none());
    }
}

#[cfg(test)]
mod skill_activation_tests {
    use super::*;
    use crate::cli::skills::PRAEPARARE_GOAL;
    use std::sync::mpsc;

    /// A TuiApp wired against throwaway directories: `cwd` holds project
    /// skills under .drip/skills, `home` is the drip home root, and `project`
    /// is an explicit override so resolve_drip_project stays deterministic.
    struct SkillFixture {
        app: TuiApp,
        _cwd: tempfile::TempDir,
        _home: tempfile::TempDir,
        _project: tempfile::TempDir,
        _rx: mpsc::Receiver<Msg>,
        _mention_rx: mpsc::Receiver<(u64, String)>,
    }

    fn write_skill(cwd: &Path, name: &str, markdown: &str) {
        let dir = cwd.join(".drip").join("skills").join(name);
        std::fs::create_dir_all(&dir).expect("create skill dir");
        std::fs::write(dir.join("SKILL.md"), markdown).expect("write SKILL.md");
    }

    fn skill_markdown(name: &str) -> String {
        format!("---\nname: {name}\ndescription: test skill {name}\n---\n\nBody for {name}.\n")
    }

    fn make_app_with_skills(skills: &[&str]) -> SkillFixture {
        let cwd = tempfile::tempdir().expect("cwd tempdir");
        let home = tempfile::tempdir().expect("home tempdir");
        let project = tempfile::tempdir().expect("project tempdir");
        for name in skills {
            write_skill(cwd.path(), name, &skill_markdown(name));
        }
        let home_root = home.path().to_string_lossy().into_owned();
        let cwd_str = cwd.path().to_string_lossy().into_owned();
        let project_str = project.path().to_string_lossy().into_owned();

        let drip_home = crate::core::home::open_drip_home(&home_root);
        let drip_project =
            crate::core::home::resolve_drip_project(&cwd_str, &home_root, Some(&project_str))
                .expect("resolve drip project");
        let session = crate::core::sessions::SessionRecord {
            sessions_dir: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            cwd: cwd_str.clone(),
            goal_count: 0,
            id: "sess-skill-test".to_string(),
            last_goal: None,
            project_slug: "skill-test".to_string(),
            status: "active".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
        };
        let bootstrap = TuiBootstrap {
            ask: false,
            ask_timeout_secs: None,
            allow_net: false,
            reference_roots: Vec::new(),
            config: crate::core::config::create_default_cli_config(),
            cwd: cwd_str,
            home: drip_home,
            initial_goal: None,
            max_iterations: None,
            max_loops: None,
            no_repo_memory: true,
            project: drip_project,
            roles_flag: None,
            session,
            status_line: None,
        };
        let (tx, rx) = mpsc::channel::<Msg>();
        let (mention_tx, mention_rx) = mpsc::channel::<(u64, String)>();
        let app = TuiApp::new(bootstrap, tx, mention_tx);
        SkillFixture {
            app,
            _cwd: cwd,
            _home: home,
            _project: project,
            _rx: rx,
            _mention_rx: mention_rx,
        }
    }

    #[test]
    fn direct_slash_token_activates_skill_for_session() {
        let mut fixture = make_app_with_skills(&["navis"]);
        fixture.app.text = "/navis".to_string();
        fixture.app.submit();
        assert_eq!(
            fixture.app.active_skills.len(),
            1,
            "/navis should enable the navis skill"
        );
        assert_eq!(fixture.app.active_skills[0].name, "navis");
        assert!(
            !fixture.app.running,
            "enabling a skill must not start a prompt run"
        );
        assert!(
            fixture.app.text.is_empty(),
            "submitting a slash command clears the composer"
        );
    }

    #[test]
    fn repeat_direct_activation_is_idempotent() {
        let mut fixture = make_app_with_skills(&["navis"]);
        fixture.app.dispatch_command("navis", "");
        fixture.app.dispatch_command("navis", "");
        assert_eq!(
            fixture.app.active_skills.len(),
            1,
            "repeat activation must not duplicate, disable, or reload the skill"
        );
        assert_eq!(fixture.app.active_skills[0].name, "navis");
    }

    /// Helper for the praeparare tests: prove the canned goal actually entered
    /// the user-turn path by inspecting the transcript goal cell (mirrors the
    /// non_skill_slash_text_still_falls_through_to_the_goal_run precedent).
    fn assert_praeparare_goal_entered_the_run_path(app: &TuiApp) {
        assert!(
            app.cells.iter().any(
                |entry| matches!(entry, TranscriptEntry::Goal(goal) if goal.text == PRAEPARARE_GOAL)
            ),
            "the shared canned goal must reach run_goal via the ordinary user-turn path"
        );
    }

    #[test]
    fn praeparare_slash_command_activates_skill_and_runs_canned_goal() {
        let mut fixture = make_app_with_skills(&["navis"]);
        fixture.app.text = "/praeparare".to_string();
        fixture.app.submit();
        assert_eq!(
            fixture.app.active_skills.len(),
            1,
            "/praeparare must activate the praeparare skill for the session"
        );
        assert_eq!(fixture.app.active_skills[0].name, "praeparare");
        assert!(
            fixture.app.text.is_empty(),
            "submit must clear the composer: the goal reaches the run path, not the editor"
        );
        assert_praeparare_goal_entered_the_run_path(&fixture.app);
    }

    #[test]
    fn praeparare_project_skill_cannot_reduce_the_command_to_activation_only() {
        // A project skill named praeparare shadows the built-in pack, but
        // "/praeparare" must still submit the shared canned goal — not degrade
        // to the plain "/<skill>" activation-only path.
        let mut fixture = make_app_with_skills(&["praeparare"]);
        fixture.app.text = "/praeparare".to_string();
        fixture.app.submit();
        assert_eq!(fixture.app.active_skills.len(), 1);
        assert_eq!(fixture.app.active_skills[0].name, "praeparare");
        assert!(
            fixture.app.text.is_empty(),
            "submit must clear the composer: the goal reaches the run path, not the editor"
        );
        assert_praeparare_goal_entered_the_run_path(&fixture.app);
    }

    #[test]
    fn praeparare_args_become_operator_context_via_the_shared_cli_helper() {
        // Distinct constructed context (not the shipped fixture strings):
        // nonempty args must extend the goal through the SAME helper the CLI
        // uses, not be dropped.
        let mut fixture = make_app_with_skills(&["navis"]);
        fixture
            .app
            .dispatch_command("praeparare", "the notes in NOTES.md are intentional");
        let expected = crate::cli::args::praeparare_goal_with_context(Some(
            "the notes in NOTES.md are intentional",
        ));
        assert_ne!(
            expected, PRAEPARARE_GOAL,
            "the constructed context must actually extend the canned goal"
        );
        assert!(
            fixture
                .app
                .cells
                .iter()
                .any(|entry| matches!(entry, TranscriptEntry::Goal(goal) if goal.text == expected)),
            "nonempty /praeparare args must reach the run path as operator context"
        );
    }

    #[test]
    fn praeparare_blank_args_leave_the_canned_goal_unchanged() {
        let mut fixture = make_app_with_skills(&["navis"]);
        fixture.app.dispatch_command("praeparare", "   ");
        assert_praeparare_goal_entered_the_run_path(&fixture.app);
        assert!(
            !fixture
                .app
                .cells
                .iter()
                .any(|entry| matches!(entry, TranscriptEntry::Goal(goal)
                    if goal.text.contains("Operator context"))),
            "whitespace-only args must not append an operator-context block"
        );
    }

    #[test]
    fn praeparare_unset_budget_defaults_to_fifteen() {
        let mut fixture = make_app_with_skills(&["navis"]);
        assert_eq!(fixture.app.bootstrap.max_iterations, None);
        fixture.app.dispatch_command("praeparare", "");
        assert_eq!(
            fixture.app.bootstrap.max_iterations,
            Some(PRAEPARARE_DEFAULT_MAX_ITERATIONS),
            "an unset budget must default to the shared 15-iteration constant"
        );
    }

    #[test]
    fn praeparare_preserves_an_explicit_budget() {
        let mut fixture = make_app_with_skills(&["navis"]);
        fixture.app.bootstrap.max_iterations = Some(7);
        fixture.app.dispatch_command("praeparare", "");
        assert_eq!(
            fixture.app.bootstrap.max_iterations,
            Some(7),
            "an explicitly chosen budget must not be overridden by the default"
        );
    }

    #[test]
    fn built_in_commands_win_over_same_named_skills() {
        let mut fixture = make_app_with_skills(&["skills", "help"]);
        fixture.app.dispatch_command("skills", "");
        assert!(
            fixture.app.active_skills.is_empty(),
            "/skills must keep its list behavior"
        );
        fixture.app.dispatch_command("help", "");
        assert!(
            fixture.app.active_skills.is_empty(),
            "/help must keep its help behavior"
        );
        // /skill keeps its toggle path even for a name that collides with a
        // built-in command, so such a skill is still usable.
        fixture.app.dispatch_command("skill", "help");
        assert_eq!(fixture.app.active_skills.len(), 1);
        assert_eq!(fixture.app.active_skills[0].name, "help");
    }

    #[test]
    fn unknown_slash_token_stays_inactive() {
        let mut fixture = make_app_with_skills(&["navis"]);
        fixture.app.dispatch_command("definitely-not-a-skill", "");
        assert!(fixture.app.active_skills.is_empty());
        assert!(!fixture.app.running);
    }

    #[test]
    fn skill_that_fails_to_load_is_not_activated() {
        let mut fixture = make_app_with_skills(&[]);
        // An args frontmatter entry with no default is a required arg; loading
        // without supplying it must fail and leave nothing half-activated.
        let markdown = "---\nname: needsarg\ndescription: requires an arg\nargs:\n  - target:\n---\n\nUse {{target}}.\n";
        write_skill(fixture._cwd.path(), "needsarg", markdown);
        fixture.app.dispatch_command("needsarg", "");
        assert!(
            fixture.app.active_skills.is_empty(),
            "a failed load must not activate the skill"
        );
        assert!(!fixture.app.running);
    }

    fn type_text(app: &mut TuiApp, text: &str) {
        app.apply_edit(text.to_string(), text.chars().count());
    }

    #[test]
    fn skill_menu_suggests_by_prefix_with_description() {
        let mut fixture = make_app_with_skills(&["navis", "other"]);
        type_text(&mut fixture.app, "/na");
        assert_eq!(fixture.app.skill_suggestions.len(), 1);
        assert_eq!(fixture.app.skill_suggestions[0].0, "navis");
        assert_eq!(fixture.app.skill_suggestions[0].1, "test skill navis");
        assert!(fixture.app.skill_suggestions.iter().all(|s| s.0 != "other"));
    }

    #[test]
    fn skill_menu_orders_exact_names_before_qualified_segments() {
        let mut fixture = make_app_with_skills(&["spellcraft:navis", "navis"]);
        type_text(&mut fixture.app, "/na");
        let names: Vec<&str> = fixture
            .app
            .skill_suggestions
            .iter()
            .map(|s| s.0.as_str())
            .collect();
        assert_eq!(names, vec!["navis", "spellcraft:navis"]);
    }

    #[test]
    fn skill_menu_excludes_built_in_names_and_slash_menu_wins() {
        let mut fixture = make_app_with_skills(&["help"]);
        type_text(&mut fixture.app, "/he");
        assert!(!get_slash_command_suggestions(&fixture.app.text).is_empty());
        assert!(fixture.app.skill_suggestions.is_empty());
    }

    #[test]
    fn arrows_move_skill_selection_and_tab_completes_without_submitting() {
        let mut fixture = make_app_with_skills(&["navis", "nada"]);
        type_text(&mut fixture.app, "/na");
        assert_eq!(fixture.app.skill_suggestions.len(), 2);
        assert_eq!(fixture.app.selected_skill_index, 0);
        fixture.app.on_key(Key::Down);
        assert_eq!(fixture.app.selected_skill_index, 1);
        fixture.app.on_key(Key::Up);
        assert_eq!(fixture.app.selected_skill_index, 0);
        let selected = fixture.app.skill_suggestions[fixture.app.selected_skill_index]
            .0
            .clone();
        fixture.app.on_key(Key::Tab);
        assert_eq!(fixture.app.text, format!("/{selected} "));
        assert!(!fixture.app.running, "Tab completes; it must not submit");
        assert!(
            fixture.app.active_skills.is_empty(),
            "Tab must not activate the skill"
        );
    }

    #[test]
    fn enter_after_completion_activates_for_session() {
        let mut fixture = make_app_with_skills(&["navis"]);
        type_text(&mut fixture.app, "/na");
        fixture.app.on_key(Key::Tab);
        fixture.app.on_key(Key::Return);
        assert_eq!(fixture.app.active_skills.len(), 1);
        assert_eq!(fixture.app.active_skills[0].name, "navis");
        assert!(!fixture.app.running);
    }

    #[test]
    fn editing_resets_the_selection_and_closes_the_menu_on_no_match() {
        let mut fixture = make_app_with_skills(&["navis", "nada"]);
        type_text(&mut fixture.app, "/na");
        fixture.app.on_key(Key::Down);
        type_text(&mut fixture.app, "/zz");
        assert!(fixture.app.skill_suggestions.is_empty());
        type_text(&mut fixture.app, "/na");
        assert_eq!(
            fixture.app.selected_skill_index, 0,
            "an edit resets the selection"
        );
    }

    #[test]
    fn escape_clears_the_composer_and_closes_the_skill_menu() {
        let mut fixture = make_app_with_skills(&["navis"]);
        type_text(&mut fixture.app, "/na");
        assert!(!fixture.app.skill_suggestions.is_empty());
        fixture.app.on_key(Key::Escape);
        assert!(fixture.app.skill_suggestions.is_empty());
        assert!(fixture.app.text.is_empty());
    }

    #[test]
    fn skill_menu_uses_a_cached_catalog_not_a_scan_per_edit() {
        let mut fixture = make_app_with_skills(&["navis"]);
        let dir = fixture._cwd.path().join(".drip").join("skills");
        std::fs::remove_dir_all(&dir).expect("remove skills dir");
        type_text(&mut fixture.app, "/na");
        assert_eq!(
            fixture.app.skill_suggestions.len(),
            1,
            "catalog is cached at construction; edits must not rescan"
        );
    }

    #[test]
    fn skill_menu_selection_clamps_at_both_list_edges() {
        let mut fixture = make_app_with_skills(&["navis", "nada"]);
        type_text(&mut fixture.app, "/na");
        assert_eq!(fixture.app.skill_suggestions.len(), 2);
        for _ in 0..5 {
            fixture.app.on_key(Key::Down);
        }
        assert_eq!(
            fixture.app.selected_skill_index, 1,
            "Down clamps at the last row"
        );
        for _ in 0..5 {
            fixture.app.on_key(Key::Up);
        }
        assert_eq!(
            fixture.app.selected_skill_index, 0,
            "Up clamps at the first row"
        );
        assert_eq!(
            fixture.app.skill_suggestions.len(),
            2,
            "clamping must not dismiss the menu"
        );
    }

    #[test]
    fn tab_completes_the_highlighted_row_not_the_first() {
        let mut fixture = make_app_with_skills(&["navis", "nada"]);
        type_text(&mut fixture.app, "/na");
        assert_eq!(fixture.app.skill_suggestions.len(), 2);
        fixture.app.on_key(Key::Down);
        assert_eq!(
            fixture.app.selected_skill_index, 1,
            "Down moves to the second row"
        );
        let first = format!("/{} ", fixture.app.skill_suggestions[0].0);
        let expected = format!("/{} ", fixture.app.skill_suggestions[1].0);
        fixture.app.on_key(Key::Tab);
        assert_eq!(
            fixture.app.text, expected,
            "Tab completes the highlighted row"
        );
        assert_ne!(
            fixture.app.text, first,
            "Tab must not complete row 0 after Down"
        );
        assert!(!fixture.app.running, "Tab must not submit");
        assert!(
            fixture.app.active_skills.is_empty(),
            "Tab must not activate a skill"
        );
    }

    #[test]
    fn draw_path_serves_the_menu_from_cached_state_without_rescanning() {
        let mut fixture = make_app_with_skills(&["navis"]);
        type_text(&mut fixture.app, "/na");
        let dir = fixture._cwd.path().join(".drip").join("skills");
        std::fs::remove_dir_all(&dir).expect("remove skills dir");
        let rows = fixture.app.live_region();
        let menu_rows = rows.iter().filter(|row| row.contains("/navis")).count();
        assert_eq!(
            menu_rows, 1,
            "live_region must paint the menu from the cached catalog even after the skills dir is gone"
        );
    }

    #[test]
    fn qualified_skill_name_activates_instead_of_starting_a_goal_run() {
        let mut fixture = make_app_with_skills(&["spellcraft:navis"]);
        type_text(&mut fixture.app, "/spellcraft:navis");
        fixture.app.on_key(Key::Return);
        assert_eq!(
            fixture.app.active_skills.len(),
            1,
            "/spellcraft:navis must enable the skill, not run it as a goal"
        );
        assert_eq!(fixture.app.active_skills[0].name, "spellcraft:navis");
        assert!(
            !fixture.app.running,
            "activation must not start a prompt run"
        );
    }

    #[test]
    fn direct_activation_matches_catalog_case_insensitively() {
        let mut fixture = make_app_with_skills(&["Navis"]);
        fixture.app.text = "/navis".to_string();
        fixture.app.submit();
        assert_eq!(
            fixture.app.active_skills.len(),
            1,
            "lowercase /navis must reach the catalog name Navis"
        );
        assert!(
            fixture.app.active_skills[0]
                .name
                .eq_ignore_ascii_case("navis"),
            "activation must use the canonical catalog spelling"
        );
    }

    #[test]
    fn non_skill_slash_text_still_falls_through_to_the_goal_run() {
        let mut fixture = make_app_with_skills(&["navis"]);
        // run_goal fails fast (before spawning a session) when the active
        // inference profile is unknown; the reported error is the observable
        // proof that path-like slash text still reaches the goal-run path.
        fixture.app.config.settings.insert(
            crate::core::config::ACTIVE_INFERENCE_PROFILE_SETTING_ID.to_string(),
            "no-such-profile".to_string(),
        );
        type_text(&mut fixture.app, "/usr/bin/env");
        fixture.app.submit();
        assert!(
            fixture.app.active_skills.is_empty(),
            "a path-like token is not a skill"
        );
        assert!(!fixture.app.running);
        assert!(
            matches!(
                fixture.app.cells.first(),
                Some(TranscriptEntry::Goal(goal)) if goal.text == "/usr/bin/env"
            ),
            "the token must fall through to run_goal, which records it as a goal"
        );
        assert!(
            fixture
                .app
                .cells
                .iter()
                .any(|entry| matches!(entry, TranscriptEntry::Error(_))),
            "run_goal reports the unresolvable inference profile as an error"
        );
    }

    #[test]
    fn dispatching_a_skill_with_arguments_errors_instead_of_activating() {
        let mut fixture = make_app_with_skills(&["navis"]);
        fixture.app.dispatch_command("navis", "extra");
        assert!(
            fixture.app.active_skills.is_empty(),
            "a skill dispatched with arguments must not activate"
        );
        assert!(!fixture.app.running);
        assert!(
            fixture
                .app
                .cells
                .iter()
                .any(|entry| matches!(entry, TranscriptEntry::Error(_))),
            "the dispatch must report an unknown-command error, not drop the args silently"
        );
        assert!(
            fixture
                .app
                .cells
                .iter()
                .all(|entry| !matches!(entry, TranscriptEntry::Skill(_))),
            "no skill activation may be recorded"
        );
    }
}

/// Focused tests for prompt-recall wiring: Up/Down order, exact draft
/// restoration, edited resend through the normal submit path, multiline
/// first/last-line boundaries, menu precedence, what submit records, and
/// session switches clearing browsing state.
#[cfg(test)]
mod prompt_history_wiring_tests {
    use super::*;
    use std::sync::mpsc;

    struct HistoryFixture {
        app: TuiApp,
        _cwd: tempfile::TempDir,
        _home: tempfile::TempDir,
        _project: tempfile::TempDir,
        _rx: mpsc::Receiver<Msg>,
        _mention_rx: mpsc::Receiver<(u64, String)>,
    }

    fn make_history_app(skills: &[&str]) -> HistoryFixture {
        let cwd = tempfile::tempdir().expect("cwd tempdir");
        let home = tempfile::tempdir().expect("home tempdir");
        let project = tempfile::tempdir().expect("project tempdir");
        for name in skills {
            let dir = cwd.path().join(".drip").join("skills").join(name);
            std::fs::create_dir_all(&dir).expect("create skill dir");
            std::fs::write(
                dir.join("SKILL.md"),
                format!(
                    "---\nname: {name}\ndescription: test skill {name}\n---\n\nBody for {name}.\n"
                ),
            )
            .expect("write SKILL.md");
        }
        let home_root = home.path().to_string_lossy().into_owned();
        let cwd_str = cwd.path().to_string_lossy().into_owned();
        let project_str = project.path().to_string_lossy().into_owned();
        let drip_home = crate::core::home::open_drip_home(&home_root);
        let drip_project =
            crate::core::home::resolve_drip_project(&cwd_str, &home_root, Some(&project_str))
                .expect("resolve drip project");
        let session = crate::core::sessions::SessionRecord {
            sessions_dir: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            cwd: cwd_str.clone(),
            goal_count: 0,
            id: "sess-history-test".to_string(),
            last_goal: None,
            project_slug: "history-test".to_string(),
            status: "active".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
        };
        let bootstrap = TuiBootstrap {
            ask: false,
            ask_timeout_secs: None,
            allow_net: false,
            reference_roots: Vec::new(),
            config: crate::core::config::create_default_cli_config(),
            cwd: cwd_str,
            home: drip_home,
            initial_goal: None,
            max_iterations: None,
            max_loops: None,
            no_repo_memory: true,
            project: drip_project,
            roles_flag: None,
            session,
            status_line: None,
        };
        let (tx, rx) = mpsc::channel::<Msg>();
        let (mention_tx, mention_rx) = mpsc::channel::<(u64, String)>();
        let app = TuiApp::new(bootstrap, tx, mention_tx);
        HistoryFixture {
            app,
            _cwd: cwd,
            _home: home,
            _project: project,
            _rx: rx,
            _mention_rx: mention_rx,
        }
    }

    fn type_into(app: &mut TuiApp, text: &str) {
        app.apply_edit(text.to_string(), text.chars().count());
    }

    #[test]
    fn up_recalls_newest_first_then_older_and_clamps() {
        let mut fixture = make_history_app(&[]);
        type_into(&mut fixture.app, "first goal");
        fixture.app.submit();
        type_into(&mut fixture.app, "second goal");
        fixture.app.submit();
        assert_eq!(fixture.app.prompt_history.len(), 2);
        fixture.app.on_key(Key::Up);
        assert_eq!(fixture.app.text, "second goal");
        fixture.app.on_key(Key::Up);
        assert_eq!(fixture.app.text, "first goal");
        fixture.app.on_key(Key::Up);
        assert_eq!(fixture.app.text, "first goal", "clamps at oldest");
    }

    #[test]
    fn down_restores_exact_draft_and_noops_outside_browsing() {
        let mut fixture = make_history_app(&[]);
        type_into(&mut fixture.app, "recorded goal");
        fixture.app.submit();
        type_into(&mut fixture.app, "unsent draft");
        fixture.app.on_key(Key::Down);
        assert_eq!(fixture.app.text, "unsent draft", "no-op outside browsing");
        fixture.app.on_key(Key::Up);
        assert_eq!(fixture.app.text, "recorded goal");
        fixture.app.on_key(Key::Down);
        assert_eq!(fixture.app.text, "unsent draft", "exact draft restored");
        fixture.app.on_key(Key::Down);
        assert_eq!(fixture.app.text, "unsent draft", "browsing ended");
    }

    #[test]
    fn recalled_prompt_edits_and_resends_through_enter_path() {
        let mut fixture = make_history_app(&[]);
        type_into(&mut fixture.app, "original wording");
        fixture.app.submit();
        fixture.app.on_key(Key::Up);
        assert_eq!(fixture.app.text, "original wording");
        type_into(&mut fixture.app, "edited wording");
        fixture.app.submit();
        assert_eq!(fixture.app.prompt_history.len(), 2);
        fixture.app.on_key(Key::Up);
        assert_eq!(fixture.app.text, "edited wording");
        fixture.app.on_key(Key::Up);
        assert_eq!(fixture.app.text, "original wording", "stored entry intact");
    }

    #[test]
    fn repeated_up_walks_older_through_multiline_entries() {
        let mut fixture = make_history_app(&[]);
        type_into(&mut fixture.app, "newest single line");
        fixture.app.submit();
        let older_multiline = "older prompt\nwith a second line";
        type_into(&mut fixture.app, older_multiline);
        fixture.app.submit();
        type_into(&mut fixture.app, "draft being typed");

        fixture.app.on_key(Key::Up);
        assert_eq!(fixture.app.text, older_multiline, "first Up recalls newest");
        // Recall leaves the cursor at the end of the multiline entry, but the
        // walk continues: browsing bypasses the first-line entry gate.
        fixture.app.on_key(Key::Up);
        assert_eq!(
            fixture.app.text, "newest single line",
            "second Up walks past a multiline entry"
        );
        fixture.app.on_key(Key::Up);
        assert_eq!(fixture.app.text, "newest single line", "clamped at oldest");

        fixture.app.on_key(Key::Down);
        assert_eq!(fixture.app.text, older_multiline);
        // Off the last line mid-browsing, Down still walks newer.
        for _ in 0..25 {
            fixture.app.on_key(Key::Left);
        }
        fixture.app.on_key(Key::Down);
        assert_eq!(
            fixture.app.text, "draft being typed",
            "exact draft restored"
        );
    }

    #[test]
    fn arrows_respect_multiline_boundaries() {
        let mut fixture = make_history_app(&[]);
        type_into(&mut fixture.app, "old prompt");
        fixture.app.submit();
        let multiline = "line one\nline two";
        type_into(&mut fixture.app, multiline);
        fixture.app.on_key(Key::Up);
        assert_eq!(fixture.app.text, multiline, "cursor on line two: no recall");
        fixture.app.on_key(Key::Down);
        assert_eq!(fixture.app.text, multiline, "no browsing was started");
        fixture.app.apply_edit(multiline.to_string(), 0);
        fixture.app.on_key(Key::Up);
        assert_eq!(fixture.app.text, "old prompt", "first line: recall");
        fixture.app.on_key(Key::Down);
        assert_eq!(fixture.app.text, multiline, "exact draft restored");
        type_into(&mut fixture.app, "alpha\nbeta");
        fixture.app.submit();
        fixture.app.apply_edit(multiline.to_string(), 0);
        fixture.app.on_key(Key::Up);
        assert_eq!(fixture.app.text, "alpha\nbeta");
        // While browsing, the last-line gate no longer applies: a mid-text
        // Down still walks newer and restores the exact draft.
        fixture.app.apply_edit("alpha\nbeta".to_string(), 2);
        fixture.app.on_key(Key::Down);
        assert_eq!(
            fixture.app.text, multiline,
            "browsing Down mid-text: draft restored"
        );
        fixture.app.on_key(Key::Down);
        assert_eq!(fixture.app.text, multiline, "browsing ended: Down no-op");
    }

    #[test]
    fn menu_arrows_take_precedence_over_history() {
        let mut fixture = make_history_app(&["navis"]);
        type_into(&mut fixture.app, "recorded goal");
        fixture.app.submit();
        type_into(&mut fixture.app, "/na");
        assert_eq!(fixture.app.skill_suggestions.len(), 1);
        fixture.app.on_key(Key::Up);
        assert_eq!(fixture.app.text, "/na", "menu open: no recall");
        assert_eq!(fixture.app.selected_skill_index, 0);
        fixture.app.on_key(Key::Down);
        assert_eq!(fixture.app.text, "/na", "menu open: no recall");
        assert_eq!(fixture.app.selected_skill_index, 0);
    }

    #[test]
    fn submit_records_goals_once_but_not_blank_or_commands() {
        let mut fixture = make_history_app(&[]);
        type_into(&mut fixture.app, "real goal");
        fixture.app.submit();
        assert_eq!(fixture.app.prompt_history.len(), 1);
        type_into(&mut fixture.app, "/help");
        fixture.app.submit();
        assert_eq!(
            fixture.app.prompt_history.len(),
            1,
            "slash command not recorded"
        );
        type_into(&mut fixture.app, "   ");
        fixture.app.submit();
        assert_eq!(fixture.app.prompt_history.len(), 1, "blank not recorded");
        type_into(&mut fixture.app, "real goal");
        fixture.app.submit();
        assert_eq!(fixture.app.prompt_history.len(), 1, "consecutive duplicate");
    }

    #[test]
    fn switch_session_drops_browsing_and_draft_but_keeps_entries() {
        let mut fixture = make_history_app(&[]);
        type_into(&mut fixture.app, "session one goal");
        fixture.app.submit();
        type_into(&mut fixture.app, "pending draft");
        fixture.app.on_key(Key::Up);
        assert_eq!(fixture.app.text, "session one goal");
        let record = crate::core::sessions::SessionRecord {
            sessions_dir: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            cwd: fixture.app.bootstrap.cwd.clone(),
            goal_count: 0,
            id: "sess-history-two".to_string(),
            last_goal: None,
            project_slug: "history-test".to_string(),
            status: "active".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
        };
        fixture.app.switch_session(record);
        fixture.app.on_key(Key::Down);
        assert_eq!(
            fixture.app.text, "session one goal",
            "no draft restored from the previous session"
        );
        assert_eq!(fixture.app.prompt_history.len(), 1, "entries kept");
    }
}
