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
use crate::cli::commands::{
    get_slash_command_suggestions, parse_slash_command, SlashCommandSpec, SLASH_COMMANDS,
};
use crate::cli::file_suggestions::{
    get_workspace_file_suggestions, WorkspaceFileSource, DEFAULT_FILE_SUGGESTION_LIMIT,
};
use crate::cli::images::{
    attachment_from_data_url, attachment_from_image_file, capture_clipboard_image,
    looks_like_image_paste, GoalImageAttachment,
};
use crate::cli::marketplaces::{
    add_marketplace, discover_all_skills, is_marketplace_key_enabled,
    list_enabled_marketplace_roles, list_marketplace_plugins, load_marketplaces_file,
    load_project_plugin_overrides, remove_marketplace, set_marketplace_key_enabled,
    update_marketplaces, AddMarketplaceArgs,
};
use crate::cli::mentions::{
    get_active_chat_file_mention, replace_active_chat_file_mention, resolve_goal_mentions,
};
use crate::cli::paste::{sanitize_pasted_input, DISABLE_BRACKETED_PASTE, ENABLE_BRACKETED_PASTE};
use crate::cli::roles::{resolve_role_setup, ResolveRoleSetupArgs, RoleSetupSource};
use crate::cli::session_run::{
    run_session_goal, SessionGoalArgs, SessionGoalError, SessionGoalOutcome,
};
use crate::cli::skills::{CliSkill, LoadedCliSkill, SkillSource};
use crate::cli::state_summary::format_state_summary;
use crate::cli::transcript::{
    append_transcript_entry, read_transcript, TranscriptEntry, TranscriptEventEntry,
    TranscriptGoalEntry, TranscriptNoteEntry, TranscriptRunEndEntry, TranscriptSkillEntry,
};
use crate::core::config::{
    get_active_cli_profile_id, get_active_cli_tool_profile_id, list_cli_model_profiles,
    list_cli_system_prompt_profiles, resolve_cli_inference, save_cli_config,
    set_active_cli_profile, set_active_cli_system_prompt, set_active_cli_tool_profile, CliConfig,
};
use crate::core::env_vars::{
    load_env_vars, load_merged_env, lookup_env_var_source, upsert_env_var,
};
use crate::core::home::{DripHome, DripProject};
use crate::core::sessions::{
    create_session, list_all_sessions, open_session_index, resolve_any_session_ref,
    session_paths_for, CreateSessionArgs, ProjectPaths, SessionEnvScope, SessionPaths,
    SessionRecord,
};
use crate::core::types::{
    HarnessEvent, HarnessEventType, HarnessSurveyAnswer, HarnessSurveyAnswers, QuestionSurvey,
};
use crate::harness::model_call::AbortSignal;
use crate::tools::async_jobs::{
    create_chat_tool_runtime_services, session_age_ms, CreateChatToolRuntimeServicesOptions,
};
use crate::tools::pack::{builtin_tool_pack, BuiltinToolOptions};
use crate::tools::types::{ChatAsyncToolJob, ChatToolRuntimeServices};
use crate::tui::compact::{
    render_compact_cell, render_cycle_transition, render_tool_group, select_compact_tail_start,
    CompactCell, CompactEmitter,
};
use crate::tui::jobs::{
    background_counter, job_counts, render_job_detail, render_jobs_list, JOBS_REFRESH_MS,
};
use crate::tui::pane_title::{PaneTitle, FALLBACK_LABEL, SPINNER_INTERVAL_MS};
use crate::tui::session_name::{
    persist_session_name, read_session_name, read_session_name_context,
};
use crate::tui::term::{terminal_size, write_out, RawMode};
use crate::tui::terminal_title::{
    generate_chat_title, generate_session_title, resolve_session_route, resolve_title_route,
    terminal_title_enabled, terminal_title_timeout_ms,
};
use crate::tui::widgets::{
    composer_cursor_at, composer_cursor_position, composer_lines, composer_text_width,
    render_composer, render_picker, render_skill_picker, render_status_bar, render_survey,
    render_working_line, ComposerProps, PickerItem, SkillPickerItem, StatusBarProps,
};
use crate::watch::ansi::term::{DISABLE_MOUSE, ENABLE_MOUSE};
use crate::watch::ansi::{string_width, strip_ansi, wrap_ansi};
use crate::watch::mouse::{parse_mouse_event, MouseEvent};

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
    pub task_loop_limit: Option<i64>,
    pub review_waiver_lines: Option<usize>,
    pub plan_mode: Option<String>,
    pub no_repo_memory: bool,
    pub project: DripProject,
    pub roles_flag: Option<RoleSetupSource>,
    pub session: SessionRecord,
    /// Opt-in custom status-line command from the persisted drip config; None
    /// keeps the built-in status bar. Never imported from ~/.claude.
    pub status_line: Option<crate::core::config::StatusLineSetting>,
    /// `--classifier <profile-id>` override for this TUI run (headless
    /// parity: it wins over `runtime.classifier_profile_id`).
    pub classifier: Option<String>,
    /// `--no-classifier`: hard off inside the TUI too, whatever the setting
    /// says.
    pub no_classifier: bool,
}

// Harness events can arrive far faster than the terminal can usefully paint;
// they queue briefly and land in a single repaint.
const EVENT_BATCH_MS: u64 = 48;
const RESIZE_SETTLE_MS: u64 = 150;
// Transient activity + notice rows kept on the live line above the composer at
// once: the block blinks in one or two rows instead of growing with a run.
const ACTIVITY_NOTICE_LIMIT: usize = 2;
// Debounce window for the transient activity block: the rows above the working
// line swap at most this often, so a burst of fast-arriving ops, cycle
// transitions or warnings coalesces into one update instead of blinking.
const ACTIVITY_DEBOUNCE_MS: u64 = 500;

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
            command
                .args
                .map(|args| format!(" {args}"))
                .unwrap_or_default(),
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
            "  while a goal runs — enter queues the message for the next run (the queue is listed above the input); ctrl+s steers the running goal with what you typed, or with the whole queue when the input is empty ",
            "  /rename [name] — rename this session (also while a goal runs; applies immediately instead of queuing)",
            "  /jobs (or ctrl+b) — browse the background monitors and async shells; the status bar counts them while they run",
            "  esc — clear the composer, or stop the running goal and its commands",
            "  ctrl+c — exit",
        ]
        .iter()
        .map(|line| line.to_string()),
    );
    lines.join("\n")
}

/// Spawns the MCP clients a TUI run needs: one per name in `names` (the union
/// of every server any role in play names and the `/mcp` run gate — see
/// `TuiApp::mcp_spawn_names`), looked up in the config-defined server map.
/// Returns the clients plus one warning line per server that could not be
/// started: MCP is opt-in and never fatal, so a dead or unconfigured server
/// only costs its tools for this run.
fn spawn_mcp_clients_for_run(
    names: &[String],
    configured: &crate::tools::mcp::config::McpServerMap,
    cwd: &Path,
) -> (
    Vec<Arc<std::sync::Mutex<crate::tools::mcp::client::McpClient>>>,
    Vec<String>,
) {
    let mut clients = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    for name in names {
        let name = name.as_str();
        let Some(server_config) = configured.get(name) else {
            warnings.push(format!(
                "mcp: server \"{name}\": not in mcpServers config — its tools are unavailable"
            ));
            continue;
        };
        match crate::tools::mcp::client::McpClient::spawn(name, server_config, cwd) {
            Ok(client) => clients.push(Arc::new(std::sync::Mutex::new(client))),
            Err(error) => warnings.push(format!(
                "mcp: server \"{name}\": {error} — its tools are unavailable"
            )),
        }
    }
    (clients, warnings)
}

/// Cuts a painted row to `width` columns without dropping its SGR codes.
fn clip_ansi(row: &str, width: usize) -> String {
    if string_width(row) <= width {
        return row.to_string();
    }
    wrap_ansi(row, width).into_iter().next().unwrap_or_default()
}

use std::collections::VecDeque;

/// Append one operator message to a session's inbox — the same handoff
/// `drip --send` uses. The running goal consumes it at its next cycle boundary
/// and treats it as operator steering that outranks the original goal.
fn append_operator_message(inbox_path: &Path, text: &str) -> std::io::Result<()> {
    let line = format!("{}\n", serde_json::json!({ "at": now_iso(), "text": text }));
    // A live TUI session may not have materialized its directory yet; the
    // inbox is worthless if a steer silently disappears.
    if let Some(parent) = inbox_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(inbox_path)?;
    std::io::Write::write_all(&mut file, line.as_bytes())
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum OverlayKind {
    Jobs,
    Model,
    Prompt,
    Question,
    Sessions,
    Skills,
    ToolModel,
}

struct Overlay {
    /// Live search text for the `/skills` picker; empty for every other
    /// overlay (they have no search line).
    filter: String,
    items: Vec<PickerItem>,
    kind: OverlayKind,
    selected: usize,
    title: String,
}

/// One discovered skill as the `/skills` picker shows it: display metadata
/// only (name, description, origin, rough size), never loaded content.
#[derive(Clone, Debug)]
struct SkillRow {
    description: String,
    /// Marketplace enable/disable key; None for project/user/builtin rows.
    key: Option<String>,
    /// True for a marketplace skill the registry currently gates: shown so the
    /// picker can explain what is installed, never toggled here.
    locked: bool,
    name: String,
    source: SkillSource,
    /// Rough token size (byte length / 4).
    tokens: usize,
}

/// The origin label a picker row shows: the marketplace key when the skill has
/// one, otherwise the skill's source directory.
fn skill_origin(row: &SkillRow) -> String {
    row.key
        .clone()
        .unwrap_or_else(|| source_label(&row.source).to_string())
}

fn source_label(source: &SkillSource) -> &'static str {
    match source {
        SkillSource::Builtin => "builtin",
        SkillSource::Marketplace => "marketplace",
        SkillSource::Project => "project",
        SkillSource::User => "user",
    }
}

/// Rough token size of a skill file: byte length / 4, the same order of
/// magnitude Claude Code prints. Built-in skills have no file on disk, so
/// their embedded content is measured through the skills loader.
fn skill_tokens(skill: &CliSkill) -> usize {
    if skill.path.starts_with("<builtin>") {
        return crate::cli::skills::load_skill_content(skill, None)
            .map(|loaded| (loaded.content.len() / 4).max(1))
            .unwrap_or(1);
    }
    path_tokens(&skill.path)
}

fn path_tokens(path: &str) -> usize {
    std::fs::read(path)
        .map(|raw| (raw.len() / 4).max(1))
        .unwrap_or(1)
}

/// Build the picker's row cache: every enabled project/user/builtin and
/// marketplace skill plus the marketplace skills the registry currently gates
/// (shown locked). Files are read here only, on dispatch; the paint path
/// always renders from this cache.
fn collect_skill_rows(cwd: &Path, home: &crate::core::home::DripHome) -> Vec<SkillRow> {
    let registry = load_marketplaces_file(Path::new(&home.marketplaces_path)).unwrap_or_default();
    let overrides = load_project_plugin_overrides(cwd);
    let mut rows: Vec<SkillRow> = discover_all_skills(cwd, home)
        .unwrap_or_default()
        .into_iter()
        .map(|skill| SkillRow {
            tokens: skill_tokens(&skill),
            description: skill.description,
            key: skill.key,
            locked: false,
            name: skill.name,
            source: skill.source,
        })
        .collect();
    for plugin in list_marketplace_plugins(home, &registry).plugins {
        for skill in plugin.skills {
            if is_marketplace_key_enabled(&plugin.key, &skill.key, &registry, &overrides) {
                continue;
            }
            rows.push(SkillRow {
                tokens: path_tokens(&skill.path),
                description: skill.description,
                key: Some(skill.key),
                locked: true,
                name: skill.name,
                source: SkillSource::Marketplace,
            });
        }
    }
    rows
}

/// Sentinel PickerItem id for the survey's free-text escape hatch — a NUL
/// prefix keeps it disjoint from any real option label.
const SURVEY_OTHER_ID: &str = "\u{0}other";

/// Sentinel PickerItem id for the survey's last row: chat about the whole
/// survey instead of answering the questions one by one.
const SURVEY_CHAT_ID: &str = "\u{0}chat";

/// Sentinel PickerItem id for the confirm row of a "select all that apply"
/// question: Enter there records every option marked with space.
const SURVEY_CONFIRM_ID: &str = "\u{0}confirm";

/// A live ask_user survey being answered one question at a time. The harness
/// thread stays blocked on answers.jsonl; the overlay/composer only collect.
struct SurveyState {
    survey: QuestionSurvey,
    /// Question currently shown (0-based).
    current: usize,
    answers: Vec<HarnessSurveyAnswer>,
    /// Some while the operator is typing a free-text "Other…" answer.
    other_input: Option<String>,
    /// Option indices marked with space on a "select all that apply" question
    /// (always empty for a single-choice question).
    multiple_picks: Vec<usize>,
    /// The last question the survey showed; a failed chat write reopens it.
    last_question: usize,
    /// Set when the operator chose "Chat about this": the survey stays alive
    /// with no overlay, and the next composer submit stands as the answer to
    /// every question (written as a chat record).
    chat_mode: bool,
    /// answers.jsonl of the session whose run asked — captured at open so a
    /// later session switch cannot redirect the answers to the wrong file.
    answers_path: std::path::PathBuf,
}

enum Msg {
    Error(String),
    Event(HarnessEvent),
    Info(String),
    Input(Vec<u8>),
    Mentions {
        paths: Vec<String>,
        seq: u64,
    },
    RunDone(Result<SessionGoalOutcome, SessionGoalError>),
    /// One-shot title generation finished on the background thread. `label`
    /// is None on any failure; stale epochs are dropped by the handler.
    Title {
        epoch: u64,
        label: Option<String>,
    },
    /// An explicit /rename finished on a background thread. Unlike Msg::Title
    /// the name is persisted to session.json when present.
    Rename {
        epoch: u64,
        name: Option<String>,
    },
}

/// One decoded terminal input.
enum Key {
    Backspace,
    Ctrl(char),
    Delete,
    Down,
    Escape,
    Left,
    /// One SGR mouse report (a wheel notch or a left-button click). The only
    /// click the TUI acts on is the status-line background chip; everything
    /// else a pointer can do is ignored.
    Mouse(MouseEvent),
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
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
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
        // SGR mouse reports (`ESC [ < Cb ; Cx ; Cy M`) ride the same CSI path
        // as the arrow keys. Parsing them into a real event is what makes a
        // click routable; a report that is not a click (wheel, release, other
        // buttons) stays `Key::Ignored`, exactly as it decoded before mouse
        // reporting existed.
        if let Ok(report) = std::str::from_utf8(sequence) {
            if let Some(event) = parse_mouse_event(report) {
                let mut keys = vec![Key::Mouse(event)];
                keys.extend(decode_plain(rest));
                return keys;
            }
        }
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
    #[cfg(test)]
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
    /// Prompts typed while a run was in flight. `enter` stacks them for the
    /// NEXT run and the composer lists them; `ctrl+s` on an empty
    /// composer steers the running goal with the whole queue at once (the
    /// inbox handoff), so a queue never becomes a dead end.
    queued_prompts: VecDeque<String>,
    pending_detail: Option<String>,
    quit: bool,
    resize_at: Option<Instant>,
    rows: usize,
    running: bool,
    running_detail: Option<String>,
    /// When the current (or last) goal run began — the activity line above the
    /// composer counts its clock up from here. `None` whenever no run is in
    /// flight, so nothing is painted above an idle composer.
    run_started_at: Option<Instant>,
    /// Next repaint deadline of the activity line animation; `None` when idle
    /// (or when the run already ended), so the loop falls back to its plain
    /// wait instead of busy-polling.
    activity_next_tick: Option<Instant>,
    /// Transient rows of the current run (folded tool summary, cycle
    /// transition, ops, warnings) painted on the activity line above the
    /// composer. Cleared at the run boundary: they are a TUI-only view and
    /// never reach scrollback (the transcript JSONL keeps every one of them
    /// for `dripw`, the headless output and the logs).
    activity_notices: Vec<CompactCell>,
    /// Transient rows that arrived inside the current debounce window: they
    /// replace `activity_notices` once that window elapses (see
    /// `ACTIVITY_DEBOUNCE_MS`), so a burst of fast messages coalesces into a
    /// single swap instead of blinking across the line.
    activity_pending: Vec<CompactCell>,
    /// When the displayed transient block was last swapped. `None` means no
    /// block is on screen yet, so the next batch lands immediately.
    activity_shown_at: Option<Instant>,
    selected_skill_index: usize,
    selected_suggestion_index: usize,
    skill_catalog: Vec<(String, String)>,
    /// Display rows for the interactive `/skills` picker (source, rough size,
    /// locked-in-marketplace flag). Kept beside `skill_catalog` so the
    /// composer suggestion menu keeps its exact tuple shape.
    skill_rows: Vec<SkillRow>,
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
    /// Run-level MCP gate set by `/mcp` — the TUI's stand-in for `--mcp`:
    /// `None` leaves the decision to each loop's role (`mcpServers`), a
    /// non-empty list is the set for loops whose role names none, and an empty
    /// list is a hard off, exactly like `--no-mcp`.
    mcp_run_gate: Option<Vec<String>>,
    /// OSC 2 title state while an interactive TTY owns stdout; None keeps
    /// headless/redirected runs silent. Pure state lives in pane_title.rs.
    pane_title: Option<PaneTitle>,
    /// The session's explicit /rename name (manual or generated), mirrored
    /// from session.json so the composer can caption it. None until renamed.
    session_name: Option<String>,
    /// One-shot guard: title generation is requested at most once per session.
    title_requested: bool,
    /// Bumped on session switch; in-flight generations from older epochs are
    /// stale and dropped without touching the title.
    title_epoch: u64,
    /// Bumped on each /rename; stale in-flight renames are dropped.
    rename_epoch: u64,
    title_next_tick: Option<Instant>,
    /// The runtime the background tools (MONITOR, BASH_ASYNC) start their jobs
    /// on. The TUI holds the ONE instance the run thread is handed, so a job
    /// that outlives its run -- a monitor still checking, an async shell still
    /// building -- stays visible to the status-line counter and the /jobs
    /// browser instead of dying with the run's own runtime.
    tool_services: ChatToolRuntimeServices,
    /// Live background jobs by kind (monitors, shells). Refreshed from
    /// `tool_services` on a loop deadline -- never in the paint path.
    job_counts: (usize, usize),
    /// The running snapshot in display order, read alongside the counts.
    job_rows: Vec<ChatAsyncToolJob>,
    /// Which running job the /jobs browser is showing in detail; `None` is
    /// the list itself.
    job_detail: Option<usize>,
    /// Next refresh deadline of the running-job snapshot.
    jobs_next_refresh: Option<Instant>,
    tx: Sender<Msg>,
    /// Test injection: when Some, replaces the process environment in
    /// merged_env so credential resolution is deterministic regardless of
    /// what the developer's shell exports (e.g. OPENROUTER_API_KEY).
    env_overlay: Option<BTreeMap<String, String>>,
}

/// Event kinds whose TUI rows are TRANSIENT: they blink on the activity line
/// above the composer while the run needs them and never settle into
/// scrollback. The transcript JSONL keeps every one of them, so `dripw`, the
/// headless output and the logs are untouched -- this is a TUI-only view.
fn is_transient_kind(kind: HarnessEventType) -> bool {
    matches!(
        kind,
        HarnessEventType::ContextExpired
            | HarnessEventType::ContextPromoted
            | HarnessEventType::ContextRefreshed
            | HarnessEventType::ContextWithheld
            | HarnessEventType::HarnessOp
            | HarnessEventType::Inference
            | HarnessEventType::IterationStart
            | HarnessEventType::LoopStart
            | HarnessEventType::RateLimited
            | HarnessEventType::RunComplete
            | HarnessEventType::RunWarning
            | HarnessEventType::StallRecovery
            | HarnessEventType::TaskFinished
            | HarnessEventType::ToolCall
            | HarnessEventType::ToolResult
    )
}

/// Whether a transcript entry is a durable TUI row. Goals, model text, run
/// summaries/ends, operator notices (info/error) and survey questions stay in
/// scrollback; tool activity, cycle transitions, ops and warnings do not.
fn is_scrollback_entry(entry: &TranscriptEntry) -> bool {
    match entry {
        TranscriptEntry::Event(event) => !is_transient_kind(event.kind),
        // Skill activation is an op; its notice is transient too.
        TranscriptEntry::Skill(_) => false,
        _ => true,
    }
}

/// Whether a projected cell is a durable scrollback row. Folded tool groups
/// and the transient op/cycle/warning rows are not: they render on the live
/// activity line.
fn is_scrollback_cell(cell: &CompactCell) -> bool {
    match cell {
        CompactCell::ToolGroup(_) => false,
        CompactCell::Passthrough(entry) => is_scrollback_entry(entry),
    }
}

/// The projected cells that may reach scrollback: folded tool groups and the
/// transient rows are dropped here (they render on the live activity line).
fn scrollback_cells(cells: &[CompactCell]) -> Vec<CompactCell> {
    cells
        .iter()
        .filter(|cell| is_scrollback_cell(cell))
        .cloned()
        .collect()
}

/// One transient notice row: a folded tool summary keeps its
/// `── N Tools called: … ──` row, a cycle transition keeps its numbered
/// `[  2 14:23:41]` preview (`render_cycle_transition`), every other transient
/// event renders as the ordinary timeline row.
fn activity_notice_rows(cell: &CompactCell, width: usize) -> Vec<String> {
    match cell {
        CompactCell::ToolGroup(group) => render_tool_group(group, width),
        CompactCell::Passthrough(TranscriptEntry::Event(event))
            if event.kind == HarnessEventType::IterationStart =>
        {
            render_cycle_transition(event, width)
        }
        CompactCell::Passthrough(other) => crate::tui::timeline::render_timeline_cell(other, width),
    }
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
        let session_name = read_session_name(Path::new(&paths.meta_path));
        let (cols, rows) = terminal_size();
        let config = bootstrap.config.clone();
        let session = bootstrap.session.clone();
        let status_line_runner = bootstrap
            .status_line
            .clone()
            .map(crate::tui::status_line::StatusLineRunner::new);

        // One runtime for the whole TUI session: it is handed to every run
        // (see run_goal) so background jobs started by one run keep living in
        // the counter and the browser after that run ends.
        let tool_services =
            create_chat_tool_runtime_services(CreateChatToolRuntimeServicesOptions {
                cwd: Some(std::path::PathBuf::from(&bootstrap.cwd)),
                jobs_root: None,
            });
        let job_rows = tool_services.async_jobs.running_jobs();
        let job_counts = job_counts(&job_rows);

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
        let skill_rows = collect_skill_rows(Path::new(&bootstrap.cwd), &bootstrap.home);

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
            queued_prompts: VecDeque::new(),
            quit: false,
            resize_at: None,
            rows,
            running: false,
            running_detail: None,
            run_started_at: None,
            activity_next_tick: None,
            activity_notices: Vec::new(),
            activity_pending: Vec::new(),
            activity_shown_at: None,
            selected_skill_index: 0,
            selected_suggestion_index: 0,
            skill_catalog,
            skill_rows,
            skill_suggestions: Vec::new(),
            session,
            mcp_run_gate: None,
            status_line_next_refresh: None,
            pane_title: None,
            session_name,
            title_requested: false,
            title_epoch: 0,
            rename_epoch: 0,
            title_next_tick: None,
            tool_services,
            job_counts,
            job_rows,
            job_detail: None,
            jobs_next_refresh: Some(Instant::now() + Duration::from_millis(JOBS_REFRESH_MS)),
            status_line_output: None,
            status_line_request_width: None,
            status_line_runner,
            text: String::new(),
            tx,
            env_overlay: None,
        }
    }

    // ----- timeline -------------------------------------------------------

    /// Rendered rows for projected compact cells (no trailing newlines).
    ///
    /// Only DURABLE cells reach scrollback. Folded tool groups and the
    /// transient op/cycle/warning rows are skipped here -- they render on the
    /// live activity line instead (see `scrollback_cells`).
    fn projected_rows(&self, cells: &[CompactCell]) -> Vec<String> {
        let mut out = Vec::new();
        for cell in scrollback_cells(cells) {
            out.extend(render_compact_cell(&cell, self.cols));
        }
        out
    }

    /// Keeps the transient rows of a batch for the live activity line: they
    /// blink above the composer while the run is in flight and are dropped at
    /// the run boundary, so no tool/op/warning noise settles into scrollback.
    ///
    /// The swap is debounced by `ACTIVITY_DEBOUNCE_MS`: a batch that lands
    /// inside the window opened by the last swap is queued instead of
    /// replacing what is on screen, so messages that come through extremely
    /// quickly coalesce into one update rather than blinking.
    fn remember_activity_notices(&mut self, now: Instant, cells: &[CompactCell]) {
        let mut fresh: Vec<CompactCell> = Vec::new();
        for cell in cells {
            if !is_scrollback_cell(cell) {
                fresh.push(cell.clone());
            }
        }
        if fresh.is_empty() {
            return;
        }
        let inside_window = self
            .activity_shown_at
            .is_some_and(|shown| now < shown + Duration::from_millis(ACTIVITY_DEBOUNCE_MS));
        if inside_window {
            // Coalesce. The deadline stays anchored to the last swap, so a
            // continuous stream still updates once per window instead of
            // starving the block.
            self.activity_pending.extend(fresh);
            let drop = self
                .activity_pending
                .len()
                .saturating_sub(ACTIVITY_NOTICE_LIMIT);
            if drop > 0 {
                self.activity_pending.drain(..drop);
            }
            return;
        }
        self.activity_notices = fresh;
        self.activity_shown_at = Some(now);
        self.activity_pending.clear();
    }

    /// Applies the queued transient batch once its debounce window has
    /// elapsed. Called from the event loop -- never from the paint path, which
    /// stays side-effect-free. Returns whether the block changed.
    fn settle_activity_notices(&mut self, now: Instant) -> bool {
        if self.activity_pending.is_empty() {
            return false;
        }
        let due = self
            .activity_shown_at
            .is_none_or(|shown| now >= shown + Duration::from_millis(ACTIVITY_DEBOUNCE_MS));
        if !due {
            return false;
        }
        self.activity_notices = std::mem::take(&mut self.activity_pending);
        self.activity_shown_at = Some(now);
        true
    }

    /// When a queued transient batch goes up, if one is waiting for its
    /// debounce window to close.
    fn activity_debounce_deadline(&self) -> Option<Instant> {
        if self.activity_pending.is_empty() {
            return None;
        }
        self.activity_shown_at
            .map(|shown| shown + Duration::from_millis(ACTIVITY_DEBOUNCE_MS))
    }

    /// Prints cells above the live region (Ink's <Static>).
    ///
    /// Raw entries still land in `cells` (repaint tail budgeting) and feed
    /// the compact projection, but scrollback shows the COMPACT view: tool
    /// activity, cycle transitions, ops and warnings blink on the live
    /// activity line and never settle here; goals, model text, run summaries,
    /// run ends and the operator info/error notices do. The transient rows of
    /// a live batch are kept for the next frame by `remember_activity_notices`.
    fn emit_static(&mut self, entries: Vec<TranscriptEntry>) {
        let cells = self.compact.absorb(&entries);
        if !entries.is_empty() {
            self.cells.extend(entries);
        }
        if self.running {
            self.remember_activity_notices(Instant::now(), &cells);
        }

        let rows = self.projected_rows(&cells);
        if rows.is_empty() {
            // Transient-only batches (tool activity, cycle transitions, ops,
            // warnings) change nothing on scrollback, but the activity line
            // may have changed -- repaint it in place.
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
        // Replay is not a live run: no transient row of an old cycle may
        // flash on the activity line of the next one -- and the debounce
        // window restarts with the fresh run.
        self.activity_notices.clear();
        self.activity_pending.clear();
        self.activity_shown_at = None;
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
        self.push_cell(
            TranscriptEntry::Info(TranscriptNoteEntry {
                at: now_iso(),
                text: text.into(),
            }),
            true,
        );
    }

    fn push_error(&mut self, text: impl Into<String>) {
        self.push_cell(
            TranscriptEntry::Error(TranscriptNoteEntry {
                at: now_iso(),
                text: text.into(),
            }),
            true,
        );
    }

    // ----- painting -------------------------------------------------------

    /// The stepped survey layout for the live Question overlay: the survey
    /// state supplies the header, question, progress and the option list (the
    /// overlay keeps the items the key handling consumes).
    // ----- background jobs -------------------------------------------------

    /// Re-reads the running-job snapshot from the shared runtime and reports
    /// whether anything the status line shows changed. Called from the event
    /// loop on its own deadline -- never from the paint path.
    fn refresh_jobs(&mut self, now: Instant) -> bool {
        self.jobs_next_refresh = Some(now + Duration::from_millis(JOBS_REFRESH_MS));
        let rows = self.tool_services.async_jobs.running_jobs();
        let counts = job_counts(&rows);
        let changed = counts != self.job_counts;
        self.job_rows = rows;
        self.job_counts = counts;
        // A snapshot that shrank under an open detail frame (the job settled
        // while being read) drops back to the list instead of showing a row
        // that is no longer there.
        if let Some(index) = self.job_detail {
            if index >= self.job_rows.len() {
                self.job_detail = None;
            }
        }
        changed
    }

    /// One message for a settled background job: what finished, how it
    /// finished, and where its full output lives, so whoever reads it next can
    /// act on it without hunting for the job. A MONITOR reports whether its
    /// signal fired; an async shell reports its exit status.
    fn background_report_message(job: &ChatAsyncToolJob) -> String {
        let exit = job.exit_code.flatten();
        let is_monitor = job.tool_name == "MONITOR";
        let outcome = match (exit, job.error.as_deref()) {
            (_, Some(error)) => format!("failed - {}", error.trim()),
            (Some(0), None) if is_monitor => "the signal fired (exit 0)".to_string(),
            (Some(code), None) if is_monitor => {
                format!("ended with exit {code} without the signal")
            }
            (Some(code), None) => format!("finished with exit {code}"),
            (None, None) if is_monitor => "settled without an exit status".to_string(),
            (None, None) => "finished without an exit status".to_string(),
        };
        let kind = if is_monitor {
            "background monitor"
        } else {
            "background shell"
        };
        format!(
            "[{kind}] {} {}: {outcome} - full output: {}",
            job.id, job.title, job.log_path
        )
    }

    /// Claims the settled background jobs nobody has read yet, as one steering
    /// report. `None` while a run is live -- the harness drains its own reports
    /// at the next round and a second reader would steal them -- and `None`
    /// when nothing has settled.
    fn take_idle_background_reports(&mut self) -> Option<String> {
        if self.running {
            return None;
        }
        let settled = self.tool_services.async_jobs.take_settled_unreported();
        if settled.is_empty() {
            return None;
        }
        Some(
            settled
                .iter()
                .map(Self::background_report_message)
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }

    /// A background job settles on its own schedule, which is usually after the
    /// run that started it has ended. Once no run is live there is nobody to
    /// hand the result to, so wake the session back up with it: the settled
    /// report becomes the next message, exactly as if the operator had waited
    /// for the job to finish and then sent the result themselves. The queued
    /// prompts go with it -- the report answers what the queue was waiting on,
    /// so replaying both as separate goals would run the same thought twice.
    fn handoff_settled_background_jobs(&mut self) {
        let Some(report) = self.take_idle_background_reports() else {
            return;
        };
        self.queued_prompts.clear();
        self.push_info(
            "a background job finished while the session was idle - waking it up with the result",
        );
        self.run_goal(report);
    }

    /// `/jobs`: the browser. Nothing running still opens it -- the empty frame
    /// says so, which is a better answer than a silent no-op.
    fn jobs_command(&mut self, args: &str) {
        self.refresh_jobs(Instant::now());
        let index = args.trim().parse::<usize>().unwrap_or(0);
        self.open_jobs();
        if index >= 1 && index <= self.job_rows.len() {
            self.open_job_detail(index - 1);
        }
    }

    /// Opens the list frame over the current running snapshot.
    fn open_jobs(&mut self) {
        self.refresh_jobs(Instant::now());
        self.job_detail = None;
        let items = self.job_items();
        self.overlay = Some(Overlay {
            filter: String::new(),
            items,
            kind: OverlayKind::Jobs,
            selected: 0,
            title: "Background jobs".to_string(),
        });
        self.repaint();
    }

    /// One picker row per running job, in the snapshot's order. The id is the
    /// row's index so `on_pick` re-reads the same position.
    fn job_items(&self) -> Vec<PickerItem> {
        self.job_rows
            .iter()
            .enumerate()
            .map(|(index, job)| {
                let kind = crate::tui::jobs::job_kind(job);
                PickerItem {
                    detail: Some(format!(
                        "{} · {} · {}",
                        kind,
                        crate::tui::jobs::running_label(session_age_ms(&job.started_at)),
                        job.id.chars().take(8).collect::<String>()
                    )),
                    id: index.to_string(),
                    label: job.title.clone(),
                }
            })
            .collect()
    }

    /// The browser's frame: the list, or the detail frame of the row it is
    /// showing. Empty while no overlay is up.
    fn jobs_rows(&self, overlay: &Overlay) -> Vec<String> {
        match self.job_detail.and_then(|index| self.job_rows.get(index)) {
            Some(job) => {
                let output = self
                    .tool_services
                    .async_jobs
                    .tail_job(&job.id, Some(60))
                    .map(|tail| tail.output)
                    .unwrap_or_default();
                render_job_detail(job, session_age_ms(&job.started_at), &output, self.cols)
            }
            None => render_jobs_list(&self.job_rows, overlay.selected, self.cols),
        }
    }

    /// Opens the detail frame of the running job at `index`, re-reading it so
    /// a settled job is never shown as running.
    fn open_job_detail(&mut self, index: usize) {
        self.refresh_jobs(Instant::now());
        if index >= self.job_rows.len() {
            self.push_info("that background job is no longer running");
            return;
        }
        self.job_detail = Some(index);
        self.repaint();
    }

    /// Key handling for the browser. Returns true when the key was consumed.
    /// Esc steps out one level at a time (detail, then the list), enter/space
    /// opens the highlighted job, and the arrows move the selection.
    fn on_jobs_key(&mut self, key: &Key) -> bool {
        if self.overlay.as_ref().map(|overlay| overlay.kind) != Some(OverlayKind::Jobs) {
            return false;
        }
        match key {
            Key::Escape => {
                if self.job_detail.take().is_some() {
                    self.repaint();
                } else {
                    self.overlay = None;
                    self.repaint();
                }
            }
            Key::Left => {
                if self.job_detail.take().is_some() {
                    self.repaint();
                }
            }
            // Enter (or space) opens the highlighted job; on the detail frame
            // the same key closes it, matching the frame's own footer.
            Key::Return => {
                if self.job_detail.is_some() {
                    self.overlay = None;
                    self.job_detail = None;
                    self.repaint();
                } else if let Some(index) = self.overlay.as_ref().map(|overlay| overlay.selected) {
                    self.open_job_detail(index);
                }
            }
            Key::Text(text) if text == " " => {
                if self.job_detail.is_some() {
                    self.overlay = None;
                    self.job_detail = None;
                    self.repaint();
                } else if let Some(index) = self.overlay.as_ref().map(|overlay| overlay.selected) {
                    self.open_job_detail(index);
                }
            }
            Key::Up => {
                if let Some(overlay) = self.overlay.as_mut() {
                    overlay.selected = overlay.selected.saturating_sub(1);
                }
                self.job_detail = None;
                self.repaint();
            }
            Key::Down | Key::Tab => {
                if let Some(overlay) = self.overlay.as_mut() {
                    overlay.selected =
                        (overlay.selected + 1).min(overlay.items.len().saturating_sub(1));
                }
                self.job_detail = None;
                self.repaint();
            }
            Key::Ctrl('c') => self.quit = true,
            _ => {}
        }
        true
    }

    /// Mouse clicks (SGR reports; the TUI turns mouse reporting on while it
    /// owns the terminal). The one click that does anything is a left press
    /// on the status-line background chip: it opens the same jobs browser
    /// `ctrl+b` and `/jobs` open, so the counter is a target and not just a
    /// readout. Wheel notches, button releases and clicks on any other cell
    /// are ignored.
    fn on_mouse(&mut self, event: MouseEvent) {
        let MouseEvent::Click { col, row } = event else {
            return;
        };
        if self.background_chip_at(col, row) {
            self.open_jobs();
        }
    }

    /// Whether the 1-based screen cell `(col, row)` sits on the background
    /// counter the status bar painted. The live region is the bottom of the
    /// screen, so a screen row maps by its distance from the last painted
    /// row; the chip is then located by searching that painted row for the
    /// exact counter text, which keeps the hit box aligned with whatever the
    /// bar actually drew at the current width -- a custom `statusLine` paints
    /// no chip, so it has no clickable cell at all.
    fn background_chip_at(&self, col: usize, row: usize) -> bool {
        let Some(counts) = background_counter(self.job_counts.0, self.job_counts.1) else {
            return false;
        };
        let live = self.live_region();
        let from_bottom = self.rows.saturating_sub(row);
        let Some(index) = live.len().checked_sub(from_bottom + 1) else {
            return false;
        };
        let Some(painted) = live.get(index) else {
            return false;
        };
        let plain = strip_ansi(painted);
        let Some(offset) = plain.find(&counts) else {
            return false;
        };
        let start = string_width(&plain[..offset]) + 1;
        col >= start && col < start + string_width(&counts)
    }

    fn survey_rows(&self, overlay: &Overlay) -> Vec<String> {
        let Some(state) = &self.survey else {
            return render_picker(&overlay.title, &overlay.items, overlay.selected, self.cols);
        };
        let Some(question) = state.survey.questions.get(state.current) else {
            return render_picker(&overlay.title, &overlay.items, overlay.selected, self.cols);
        };
        // The escape-hatch rows carry sentinel ids starting with NUL; every
        // row before the first sentinel is a listed option, whatever the mix
        // of confirm/free-text/chat rows this question generated.
        let listed = overlay
            .items
            .iter()
            .position(|item| item.id.starts_with('\u{0}'))
            .unwrap_or(overlay.items.len());
        // An empty slice on a single-choice question: render_survey reads a
        // non-empty slice as "this is a select-all-that-apply question".
        let toggled: Vec<bool> = if question.multiple {
            (0..listed)
                .map(|index| state.multiple_picks.contains(&index))
                .collect()
        } else {
            Vec::new()
        };
        render_survey(
            &question.header,
            &question.question,
            &overlay.items[..listed],
            &toggled,
            question.allow_other,
            overlay.selected,
            state.current + 1,
            state.survey.questions.len(),
            self.cols,
        )
    }

    fn live_region(&self) -> Vec<String> {
        let mut rows = vec![String::new()]; // marginTop 1

        if let Some(state) = self
            .survey
            .as_ref()
            .filter(|state| state.other_input.is_some())
        {
            let question = state
                .survey
                .questions
                .get(state.current)
                .map(|question| question.question.as_str())
                .unwrap_or("");
            let input = state.other_input.as_deref().unwrap_or("");
            rows.push(format!("Type something — {question}"));
            rows.push(format!("> {input}▏"));
            rows.push("enter to submit · esc back to choices".to_string());
        } else if let Some(overlay) = &self.overlay {
            // The survey overlay draws its own stepped layout; every other
            // picker keeps the generic renderer (and its exact output).
            if overlay.kind == OverlayKind::Question {
                rows.extend(self.survey_rows(overlay));
            } else if overlay.kind == OverlayKind::Skills {
                rows.extend(self.skill_picker_rows(overlay));
            } else if overlay.kind == OverlayKind::Jobs {
                rows.extend(self.jobs_rows(overlay));
            } else {
                rows.extend(render_picker(
                    &overlay.title,
                    &overlay.items,
                    overlay.selected,
                    self.cols,
                ));
            }
        } else {
            // Claude-style transient activity block directly above the chat:
            // the folded tool summary and the most recent op/cycle/warning
            // notice blink here beside the braille spinner and the clock
            // counting up from the run's start, and are erased when the run
            // ends. Nothing here is persisted into scrollback -- the full
            // record stays in the transcript JSONL for `dripw` and the logs.
            if self.running {
                let mut transient: Vec<String> = Vec::new();
                if let Some(group) = self.compact.projection.active_group() {
                    transient.extend(render_tool_group(group, self.cols));
                }
                for cell in &self.activity_notices {
                    transient.extend(activity_notice_rows(cell, self.cols));
                }
                if transient.len() > ACTIVITY_NOTICE_LIMIT {
                    // Newest rows only: the block blinks in one or two lines
                    // instead of growing with the run.
                    transient = transient.split_off(transient.len() - ACTIVITY_NOTICE_LIMIT);
                }
                let status_rows = !transient.is_empty();
                rows.extend(transient);
                if status_rows && self.run_started_at.is_some() {
                    // One blank row between the in-progress status lines and
                    // the working line, so the block is not flush against it.
                    rows.push(String::new());
                }
                if let Some(started) = self.run_started_at {
                    let elapsed = started.elapsed();
                    rows.push(render_working_line(
                        crate::tui::pane_title::frame_for(elapsed),
                        elapsed,
                        self.cols,
                    ));
                }
            }
            let slash: Vec<&SlashCommandSpec> = get_slash_command_suggestions(&self.text);
            let queued: Vec<String> = self.queued_prompts.iter().cloned().collect();
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
                    queued: &queued,
                    session_name: self.session_name.as_deref(),
                },
                self.cols,
            ));
        }

        let skill_names: Vec<String> = self
            .active_skills
            .iter()
            .map(|skill| skill.name.clone())
            .collect();
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
                    running: self.running,
                    running_detail: self.running_detail.as_deref(),
                    session_id: &self.session.id,
                    monitors: self.job_counts.0,
                    shells: self.job_counts.1,
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
        // Budget the tail over the DURABLE compact cells so tool groups and
        // transient op/warning rows never re-appear after a resize; whatever
        // the live activity line shows is not part of this budget.
        let finalized = scrollback_cells(&self.compact.projection.cells);
        let start = select_compact_tail_start(&finalized, self.rows);
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
        self.status_line_next_refresh = Some(Instant::now() + Duration::from_millis(interval_ms));
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
            Some(profile) => format!(
                "{} ({})",
                profile.label.clone().unwrap_or_else(|| profile.id.clone()),
                profile.model
            ),
            None if active_id.is_empty() => "no profile".to_string(),
            None => active_id.clone(),
        };
        let tool_profile_id = get_active_cli_tool_profile_id(settings);
        let tool_profile = if !tool_profile_id.is_empty() && tool_profile_id != active_id {
            profiles
                .iter()
                .find(|candidate| candidate.id == tool_profile_id)
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

    /// Display columns the composer body has at this terminal width, the
    /// same layout `render_composer` draws.
    fn composer_text_width(&self) -> usize {
        composer_text_width(self.cols)
    }

    /// Up arrow. Moving one VISUAL line up inside the wrapped text takes
    /// precedence, keeping the column (clamped to the shorter line); only on
    /// the top visual line does Up recall the previous prompt, saving the
    /// unsent composer text as the draft. Once browsing, repeated Up presses
    /// keep walking older entries from the top line.
    fn recall_older_prompt(&mut self) {
        let width = self.composer_text_width();
        let (line, column) = composer_cursor_position(&self.text, width, self.cursor);
        if line > 0 {
            let cursor = composer_cursor_at(&self.text, width, line - 1, column);
            self.apply_edit(self.text.clone(), cursor);
            return;
        }
        if let Some(text) = self.prompt_history.older(&self.text) {
            let cursor = text.chars().count();
            self.apply_edit(text, cursor);
        }
    }

    /// Down arrow. Moving one VISUAL line down inside the wrapped text takes
    /// precedence, so a multi-line recalled entry keeps the cursor inside it;
    /// only from the last visual line does Down step to the newer entry, or
    /// restore the exact draft past the newest one (`newer()` is itself a
    /// no-op returning None when not browsing).
    fn recall_newer_prompt(&mut self) {
        let width = self.composer_text_width();
        let (line, column) = composer_cursor_position(&self.text, width, self.cursor);
        let last_line = composer_lines(&self.text, width).len() - 1;
        if line < last_line {
            let cursor = composer_cursor_at(&self.text, width, line + 1, column);
            self.apply_edit(self.text.clone(), cursor);
            return;
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
                let path = self.mention_suggestions[self
                    .selected_suggestion_index
                    .min(self.mention_suggestions.len() - 1)]
                .clone();
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

        // "Chat about this": the composer's next message answers the whole
        // survey, so it is recorded instead of starting a goal or steering.
        if self.survey.as_ref().is_some_and(|state| state.chat_mode) {
            if !submitted.is_empty() {
                self.record_chat_reply(submitted);
            }
            return;
        }

        let command = parse_slash_command(&submitted);

        // A run in flight can still be typed to, but enter must not start a
        // second run against the same session: it queues the prompt instead.
        // /rename is the exception: it never touches the run (only the pane
        // title and session.json), so it applies immediately instead of
        // sitting in the queue until the goal finishes.
        if self.running {
            match command {
                Some(command) if command.name == "rename" => {
                    self.dispatch_command(&command.name, &command.args);
                }
                _ => self.queue_prompt(submitted),
            }
            return;
        }

        if let Some(command) = command {
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

    /// Write the operator's free-form survey reply as a chat record: the
    /// message stands as the answer to every question, so the answers array
    /// stays empty. Retries like finish_survey; a failure keeps chat mode on
    /// and puts the reply back in the composer so Enter retries it.
    fn record_chat_reply(&mut self, text: String) {
        let Some(state) = self.survey.as_ref() else {
            return;
        };
        let path = state.answers_path.clone();
        let mut outcome = crate::core::state::answers::append_chat(&path, &text);
        for _ in 0..2 {
            if outcome.is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
            outcome = crate::core::state::answers::append_chat(&path, &text);
        }
        match outcome {
            Ok(()) => {
                self.survey = None;
                self.overlay = None;
                self.set_title_waiting(false);
                self.push_info("chat reply recorded — the run continues");
            }
            Err(error) => {
                // Stay in chat mode and put the reply back in the composer:
                // Enter retries the append instead of losing what was typed.
                self.push_error(format!(
                    "could not record the chat reply: {error} — press Enter to retry"
                ));
                let cursor = text.chars().count();
                self.apply_edit(text, cursor);
            }
        }
        self.repaint();
    }

    /// Enter while a run is in flight: hold the prompt for the NEXT run. It is
    /// deliberately not written to the session inbox — that handoff is what
    /// steering is — so a queued message only reaches the agent once the
    /// running goal has ended (or the operator promotes it with ctrl+s).
    /// The queue is shown above the composer, not in the timeline, so nothing
    /// is logged here.
    fn queue_prompt(&mut self, text: String) {
        if text.trim().is_empty() {
            return;
        }
        self.prompt_history.record(&text);
        self.queued_prompts.push_back(text);
        self.repaint();
    }

    /// Ctrl+s while a run is in flight: steer the live session through
    /// its inbox. Typed text steers on its own and leaves the queue alone; an
    /// empty composer steers with the WHOLE queue, in order, and flushes it.
    /// The harness picks the messages up at its next cycle boundary and
    /// treats them as steering that outranks the original goal, so a queue is
    /// never a dead end while the run is still going.
    fn steer_running_goal(&mut self) {
        let typed = self.text.trim().to_string();
        if !typed.is_empty() {
            self.apply_edit(String::new(), 0);
            self.prompt_history.record(&typed);
            match append_operator_message(Path::new(&self.paths.inbox_path), &typed) {
                Ok(()) => self.push_info(format!(
                    "steering the running goal: {typed} (it lands at the next cycle boundary)"
                )),
                Err(error) => {
                    // Losing the message silently would be the worst outcome:
                    // put it at the head of the queue and say so loudly.
                    self.push_error(format!(
                        "could not steer the running goal ({error}) — kept in the queue"
                    ));
                    self.queued_prompts.push_front(typed);
                }
            }
            self.repaint();
            return;
        }

        if self
            .queued_prompts
            .iter()
            .all(|queued| queued.trim().is_empty())
        {
            self.queued_prompts.clear();
            self.push_info(
                "nothing to steer with — type a message, or queue one with enter first.",
            );
            self.repaint();
            return;
        }

        let mut sent = 0usize;
        while let Some(text) = self.queued_prompts.pop_front() {
            if text.trim().is_empty() {
                continue;
            }
            if let Err(error) = append_operator_message(Path::new(&self.paths.inbox_path), &text) {
                // Whatever did not reach the inbox stays queued, in order.
                self.queued_prompts.push_front(text);
                self.push_error(format!(
                    "could not steer the running goal ({error}) — {} kept in the queue",
                    self.queued_prompts.len()
                ));
                break;
            }
            sent += 1;
        }
        if sent > 0 {
            let noun = if sent == 1 { "message" } else { "messages" };
            self.push_info(format!(
                "steering the running goal with {sent} queued {noun} (they land at the next cycle boundary)"
            ));
        }
        self.repaint();
    }

    // ----- keys -----------------------------------------------------------

    fn on_key(&mut self, key: Key) {
        // Mouse reports are handled before every text entry point: a click can
        // never be typed into the composer, a survey answer or a search field.
        if let Key::Mouse(event) = &key {
            self.on_mouse(*event);
            return;
        }

        // Free-text "Other…" survey answer: collected before the overlay arm
        // and the running guard so typing works mid-run.
        if self
            .survey
            .as_ref()
            .is_some_and(|state| state.other_input.is_some())
        {
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
                    } else {
                        self.push_info("type a free-text answer, or esc to go back to the choices");
                    }
                }
                Key::Escape => {
                    if let Some(state) = self.survey.as_mut() {
                        state.other_input = None;
                    }
                    self.open_survey_question();
                }
                Key::Backspace => {
                    if let Some(input) = self
                        .survey
                        .as_mut()
                        .and_then(|state| state.other_input.as_mut())
                    {
                        input.pop();
                    }
                }
                Key::Text(text) | Key::Paste(text) => {
                    if let Some(input) = self
                        .survey
                        .as_mut()
                        .and_then(|state| state.other_input.as_mut())
                    {
                        input.push_str(&text);
                    }
                }
                Key::Ctrl('c') => self.quit = true,
                _ => {}
            }
            return;
        }

        // The skills picker owns its keys: typing edits the search filter,
        // enter/space toggles the highlighted skill, arrows slide the
        // selection, and esc closes it without writing anything to the
        // transcript. Every other overlay keeps the shared arms below.
        if self.on_skill_picker_key(&key) {
            return;
        }

        // The jobs browser owns its keys too: esc closes the detail frame
        // first and the browser second, enter opens the highlighted job, and
        // the arrows keep sliding the list from either level.
        if self.on_jobs_key(&key) {
            return;
        }

        // ctrl+b opens the jobs browser from both composer states -- idle and
        // mid-run -- so the live background work is one key away whenever
        // there is something to look at.
        if matches!(key, Key::Ctrl('b')) && self.overlay.is_none() {
            self.open_jobs();
            return;
        }

        if let Some(overlay) = self.overlay.as_mut() {
            match key {
                Key::Escape => {
                    let kind = overlay.kind;
                    self.overlay = None;
                    if kind == OverlayKind::Question {
                        self.survey = None;
                        self.set_title_waiting(false);
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
                // Space marks an option on a "select all that apply"
                // question: it toggles the highlighted row and leaves the
                // overlay open, so several marks can be built up before the
                // confirm row records them all.
                Key::Text(text) if overlay.kind == OverlayKind::Question && text == " " => {
                    let selected = overlay.selected;
                    self.toggle_multi_pick(selected);
                }
                Key::Up => overlay.selected = overlay.selected.saturating_sub(1),
                Key::Down | Key::Tab => {
                    overlay.selected =
                        (overlay.selected + 1).min(overlay.items.len().saturating_sub(1));
                }
                // Digit keys jump: the question overlay confirms that row
                // immediately, every other picker just highlights it.
                Key::Text(text)
                    if !text.is_empty() && text.chars().all(|ch| ch.is_ascii_digit()) =>
                {
                    let number: usize = text.parse().unwrap_or(0);
                    if number >= 1 && number <= overlay.items.len() {
                        overlay.selected = number - 1;
                        if overlay.kind == OverlayKind::Question {
                            let selected = overlay.items[number - 1].clone();
                            self.overlay = None;
                            self.on_pick(OverlayKind::Question, selected);
                        }
                    }
                }
                Key::Ctrl('c') => self.quit = true,
                _ => {}
            }
            return;
        }

        // "Chat about this": no overlay is up, so Esc dismisses the survey
        // exactly like Esc on the overlay does today.
        // Every other key falls through to the composer arms below, so the
        // reply is typed like any message and Enter reaches submit().
        if matches!(key, Key::Escape) && self.survey.as_ref().is_some_and(|state| state.chat_mode) {
            self.survey = None;
            self.set_title_waiting(false);
            self.push_info(
                "survey dismissed — answer with `drip --answer` or the run ends at its ask timeout",
            );
            return;
        }

        if self.running {
            match key {
                Key::Escape => {
                    if let Some(abort) = &self.abort {
                        abort.abort();
                    }
                    // The abort signal alone is only observed BETWEEN harness
                    // steps, so an in-flight BASH/VERIFY child kept running to
                    // its own timeout (an operator had to wait out a `sleep`).
                    // Esc now stops those children here, with exactly the
                    // process-group SIGTERM/SIGKILL sweep a `drip --stop`
                    // signal performs - the registry is process-wide, so every
                    // running command of this run is reached.
                    let stopped = crate::tools::child_process::terminate_active_processes();
                    if stopped > 0 {
                        self.push_info(format!(
                            "esc — aborting the run and stopping {stopped} in-flight command(s)"
                        ));
                    }
                }
                Key::Ctrl('c') => self.quit = true,
                // Enter queues for the next run; ctrl+s steers the LIVE run
                // with the typed text, or with the whole queue when empty.
                // Ctrl+s is one raw-mode byte every terminal delivers, unlike
                // shift+enter, which most terminals report as plain enter.
                Key::Return => self.submit(),
                Key::Ctrl('s') => self.steer_running_goal(),
                // The composer stays editable while a run is in flight, so a
                // message can be composed (and corrected) before queueing.
                Key::Backspace | Key::Delete => {
                    let chars: Vec<char> = self.text.chars().collect();
                    let cursor = self.cursor.min(chars.len());
                    if cursor > 0 {
                        let mut next: String = chars[..cursor - 1].iter().collect();
                        next.extend(chars[cursor..].iter());
                        self.apply_edit(next, cursor - 1);
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
                Key::Paste(raw) => self.on_paste(&raw),
                Key::Text(text) => self.insert_text(&text),
                // A draft typed mid-run gets the same visual-line cursor
                // movement and history recall as an idle composer.
                Key::Up => self.recall_older_prompt(),
                Key::Down => self.recall_newer_prompt(),
                Key::Tab | Key::Ctrl(_) | Key::Ignored | Key::Mouse(_) => {}
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
            // Idle ctrl+s: drain the queue into a run, else it just sends
            // what is typed (there is no running goal to steer).
            Key::Ctrl('s') => match self.queued_prompts.pop_front() {
                Some(next) if !next.trim().is_empty() => self.run_goal(next),
                Some(_) => {}
                None => self.submit(),
            },
            Key::Return => {
                // Enter accepts an open menu selection unless the text already matches it exactly.
                if menu_length > 0 {
                    let slash = get_slash_command_suggestions(&self.text);
                    let exact_slash =
                        slash.len() == 1 && format!("/{}", slash[0].name) == self.text.trim();
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
                Some(attachment) => {
                    self.attachments.push(attachment);
                    let count = self.attachments.len();
                    let status = crate::tui::images::inline_status();
                    self.push_info(format!("{count} image(s) attached. {status}"));
                }
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
            Key::Ctrl(_) | Key::Ignored | Key::Mouse(_) => {}
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
                if let Some(attachment) =
                    attachment_from_data_url(&pasted, Path::new(&self.paths.images_dir))
                {
                    self.attachments.push(attachment);
                    return;
                }
            }
            Some("file-path") => {
                if let Some(attachment) = attachment_from_image_file(
                    pasted.trim(),
                    (&self.bootstrap.cwd, Path::new(&self.paths.images_dir)),
                ) {
                    self.attachments.push(attachment);
                    return;
                }
            }
            _ => {}
        }

        let normalized = pasted.replace("\r\n", "\n").replace('\r', "\n");
        let submits_on_newline = normalized.ends_with('\n');
        let insertion = if submits_on_newline {
            &normalized[..normalized.len() - 1]
        } else {
            normalized.as_str()
        };
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
            // The skills picker carries live rows and a search filter — opened
            // by open_skill_picker, never through this static menu path.
            OverlayKind::Skills => return,
            // The jobs browser carries a live running-job snapshot — opened
            // by open_jobs, never through this static menu path.
            OverlayKind::Jobs => return,
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
                            detail: Some(
                                "route every request through the active model".to_string(),
                            ),
                            id: String::new(),
                            label: format!(
                                "{}no split — use the active model",
                                if active_tool_id.is_empty() {
                                    "● "
                                } else {
                                    ""
                                }
                            ),
                        }];
                        items.extend(profiles.into_iter().map(|profile| PickerItem {
                            detail: Some(format!("{} · {}", profile.provider, profile.model)),
                            label: format!(
                                "{}{}",
                                if profile.id == active_tool_id {
                                    "● "
                                } else {
                                    ""
                                },
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
                        detail: Some(
                            record
                                .last_goal
                                .clone()
                                .unwrap_or_else(|| "(no goal yet)".to_string()),
                        ),
                        label: format!(
                            "{}{} · {} · {} goal(s)",
                            if record.id == self.session.id {
                                "● "
                            } else {
                                ""
                            },
                            short_id(&record.id),
                            record.status,
                            record.goal_count
                        ),
                        id: record.id,
                    })
                    .collect(),
            ),
        };
        self.overlay = Some(Overlay {
            filter: String::new(),
            items,
            kind,
            selected: 0,
            title: title.to_string(),
        });
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
            multiple_picks: Vec::new(),
            last_question: 0,
            chat_mode: false,
            // The exact file the blocked harness thread polls (loop.rs answers_path()).
            answers_path: Path::new(&self.paths.state_path).with_file_name("answers.jsonl"),
        });
        self.set_title_waiting(true);
        self.open_survey_question();
    }

    fn open_survey_question(&mut self) {
        let Some(state) = &self.survey else { return };
        let Some(question) = state.survey.questions.get(state.current) else {
            return;
        };
        let mut items: Vec<PickerItem> = question
            .options
            .iter()
            .map(|option| PickerItem {
                detail: Some(option.description.clone()),
                id: option.label.clone(),
                label: option.label.clone(),
            })
            .collect();
        if question.multiple {
            // "Select all that apply": space toggles the option rows and this
            // row records the marks (render_survey numbers it identically).
            items.push(PickerItem {
                detail: Some("enter records the options marked [x]".to_string()),
                id: SURVEY_CONFIRM_ID.to_string(),
                label: "Confirm selection".to_string(),
            });
        }
        if question.allow_other {
            items.push(PickerItem {
                detail: Some("answer with free text instead".to_string()),
                id: SURVEY_OTHER_ID.to_string(),
                label: "Type something.".to_string(),
            });
        }
        // Always last: hand the whole survey back to the operator in prose.
        items.push(PickerItem {
            detail: None,
            id: SURVEY_CHAT_ID.to_string(),
            label: "Chat about this".to_string(),
        });
        let title = format!(
            "clarification {}/{} · {} — {}",
            state.current + 1,
            state.survey.questions.len(),
            question.header,
            question.question
        );
        self.overlay = Some(Overlay {
            filter: String::new(),
            items,
            kind: OverlayKind::Question,
            selected: 0,
            title,
        });
    }

    /// Space on a "select all that apply" question: flip one option's mark.
    /// A row that is not a listed option (or a plain question) is a no-op.
    fn toggle_multi_pick(&mut self, index: usize) {
        let Some(state) = self.survey.as_mut() else {
            return;
        };
        let Some(question) = state.survey.questions.get(state.current) else {
            return;
        };
        if !question.multiple || index >= question.options.len() {
            return;
        }
        if state.multiple_picks.contains(&index) {
            state.multiple_picks.retain(|picked| *picked != index);
        } else {
            state.multiple_picks.push(index);
            state.multiple_picks.sort_unstable();
        }
    }

    /// Enter on the "Confirm selection" row: record every marked option of a
    /// "select all that apply" question as one comma-joined choice — the same
    /// shape `validate_survey_answers` splits apart at the harness end.
    fn confirm_multi_picks(&mut self) {
        let labels: Vec<String> = {
            let Some(state) = self.survey.as_ref() else {
                return;
            };
            let Some(question) = state.survey.questions.get(state.current) else {
                return;
            };
            if !question.multiple {
                return;
            }
            state
                .multiple_picks
                .iter()
                .filter_map(|index| {
                    question
                        .options
                        .get(*index)
                        .map(|option| option.label.clone())
                })
                .collect()
        };
        if labels.is_empty() {
            self.push_info("mark at least one option with space before confirming");
            return;
        }
        self.record_survey_answer(Some(labels.join(", ")), None);
    }

    fn record_survey_answer(&mut self, choice: Option<String>, other: Option<String>) {
        let done = {
            let Some(state) = self.survey.as_mut() else {
                return;
            };
            state.answers.push(HarnessSurveyAnswer {
                index: state.current as i64,
                choice,
                other,
            });
            state.other_input = None;
            state.multiple_picks.clear();
            state.last_question = state.current;
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
        if self
            .overlay
            .as_ref()
            .is_some_and(|overlay| overlay.kind == OverlayKind::Question)
        {
            self.overlay = None;
        }
        self.set_title_waiting(false);
        self.push_info(message.to_string());
    }

    fn finish_survey(&mut self) {
        let Some(state) = self.survey.as_ref() else {
            return;
        };
        let record = HarnessSurveyAnswers {
            at: now_iso(),
            answers: state.answers.clone(),
            chat: None,
        };
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
                self.set_title_waiting(false);
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

    /// Enter on "Chat about this": close the overlay, keep the survey alive in
    /// chat mode, and echo every question so the operator can answer the whole
    /// survey in one message (the next composer submit writes the chat record).
    fn begin_survey_chat(&mut self) {
        let lines = {
            let Some(state) = self.survey.as_mut() else {
                return;
            };
            state.chat_mode = true;
            state.last_question = state.current;
            let total = state.survey.questions.len();
            let mut lines = vec![format!(
                "chat about the survey — your next message answers all {total} questions"
            )];
            for (index, question) in state.survey.questions.iter().enumerate() {
                lines.push(format!(
                    "{}. {} — {}",
                    index + 1,
                    question.header,
                    question.question
                ));
                for (option_index, option) in question.options.iter().enumerate() {
                    lines.push(format!(
                        "   {}. {} — {}",
                        option_index + 1,
                        option.label,
                        option.description
                    ));
                }
            }
            lines.push(
                "esc dismisses the survey (answer it later with `drip --answer`)".to_string(),
            );
            lines
        };
        self.overlay = None;
        self.push_info(lines.join("\n"));
        self.repaint();
    }

    fn on_pick(&mut self, kind: OverlayKind, item: PickerItem) {
        match kind {
            OverlayKind::Question => {
                if item.id == SURVEY_CONFIRM_ID {
                    self.confirm_multi_picks();
                } else if item.id == SURVEY_OTHER_ID {
                    if let Some(state) = self.survey.as_mut() {
                        state.other_input = Some(String::new());
                    }
                } else if item.id == SURVEY_CHAT_ID {
                    self.begin_survey_chat();
                } else {
                    self.record_survey_answer(Some(item.id), None);
                }
            }
            OverlayKind::Model => match set_active_cli_profile(self.config.clone(), &item.id) {
                Ok(next) => self.save_config(next, format!("model profile set to {}", item.id)),
                Err(error) => self.push_error(error.to_string()),
            },
            OverlayKind::ToolModel => {
                match set_active_cli_tool_profile(self.config.clone(), &item.id) {
                    Ok(next) => {
                        let note = if item.id.is_empty() {
                            "tool-calling model cleared — every request uses the active model"
                                .to_string()
                        } else {
                            format!("tool-calling model set to {}", item.id)
                        };
                        self.save_config(next, note);
                    }
                    Err(error) => self.push_error(error.to_string()),
                }
            }
            OverlayKind::Prompt => {
                match set_active_cli_system_prompt(self.config.clone(), &item.id) {
                    Ok(next) => self.save_config(next, format!("system prompt set to {}", item.id)),
                    Err(error) => self.push_error(error.to_string()),
                }
            }
            OverlayKind::Skills => {
                // The picker's own key handling toggles and stays open; this
                // arm keeps the match exhaustive for any future caller.
                let name = item.id.clone();
                self.skills_toggle(&name);
            }
            OverlayKind::Jobs => {
                // Index into the running snapshot; a job that settled since
                // the list was drawn is re-read there, so a stale row cannot
                // open a job that is already gone.
                if let Ok(index) = item.id.parse::<usize>() {
                    self.open_job_detail(index);
                }
            }
            OverlayKind::Sessions => {
                if let Some(record) =
                    resolve_any_session_ref(&self.bootstrap.project, Some(&item.id))
                {
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
        self.drop_survey(
            "survey dismissed by session switch — answer that run with `drip --answer`",
        );
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
        self.session_name = read_session_name(Path::new(&self.paths.meta_path));
        if let Some(name) = self.session_name.clone() {
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
        self.skill_rows = collect_skill_rows(Path::new(&self.bootstrap.cwd), &self.bootstrap.home);
    }

    /// The picker rows the live search filter currently keeps.
    fn visible_skill_rows(&self) -> Vec<SkillRow> {
        let Some(overlay) = self
            .overlay
            .as_ref()
            .filter(|overlay| overlay.kind == OverlayKind::Skills)
        else {
            return Vec::new();
        };
        let query = overlay.filter.to_ascii_lowercase();
        self.skill_rows
            .iter()
            .filter(|row| {
                query.is_empty()
                    || row.name.to_ascii_lowercase().contains(&query)
                    || row.description.to_ascii_lowercase().contains(&query)
            })
            .cloned()
            .collect()
    }

    /// Open the interactive `/skills` picker from the cached catalog: nothing
    /// is written to the transcript, the filter starts empty and the rows keep
    /// the live enabled state.
    fn open_skill_picker(&mut self) {
        self.overlay = Some(Overlay {
            filter: String::new(),
            items: Vec::new(),
            kind: OverlayKind::Skills,
            selected: 0,
            title: "Skills".to_string(),
        });
        self.refresh_skill_picker_items();
        self.repaint();
    }

    /// Re-derive the picker's selectable rows from `skill_rows` and the live
    /// filter, keeping the highlight inside the new list.
    fn refresh_skill_picker_items(&mut self) {
        let items: Vec<PickerItem> = self
            .visible_skill_rows()
            .iter()
            .map(|row| PickerItem {
                detail: Some(format!(
                    "{} · ~{} tok{}",
                    skill_origin(row),
                    row.tokens,
                    if row.locked {
                        " · locked by plugin"
                    } else {
                        ""
                    }
                )),
                id: row.name.clone(),
                label: row.name.clone(),
            })
            .collect();
        if let Some(overlay) = self.overlay.as_mut() {
            overlay.selected = overlay.selected.min(items.len().saturating_sub(1));
            overlay.items = items;
        }
    }

    /// Painted rows for the skills picker: the cached rows joined with the
    /// live enabled state, handed to the pure renderer.
    fn skill_picker_rows(&self, overlay: &Overlay) -> Vec<String> {
        let active: HashSet<&str> = self
            .active_skills
            .iter()
            .map(|skill| skill.name.as_str())
            .collect();
        let items: Vec<SkillPickerItem> = self
            .visible_skill_rows()
            .iter()
            .map(|row| SkillPickerItem {
                description: row.description.clone(),
                enabled: active.contains(row.name.as_str()),
                locked: row.locked,
                name: row.name.clone(),
                source: skill_origin(row),
                tokens: row.tokens,
            })
            .collect();
        render_skill_picker(
            &overlay.filter,
            &items,
            overlay.selected,
            self.skill_rows.len(),
            self.cols,
        )
    }

    /// Key handling for the live `/skills` picker. Returns true when the key
    /// was consumed; every consumed branch repaints, so the picker stays live
    /// without writing a single row to the transcript.
    fn on_skill_picker_key(&mut self, key: &Key) -> bool {
        if self.overlay.as_ref().map(|overlay| overlay.kind) != Some(OverlayKind::Skills) {
            return false;
        }
        match key {
            Key::Escape => {
                self.overlay = None;
                self.repaint();
            }
            Key::Return => {
                if let Some(name) = self.selected_skill_name() {
                    self.skills_toggle(&name);
                }
            }
            Key::Text(text) if text.as_str() == " " => {
                if let Some(name) = self.selected_skill_name() {
                    self.skills_toggle(&name);
                }
            }
            Key::Text(text) => self.push_skill_query(text),
            Key::Paste(text) => self.push_skill_query(text),
            Key::Backspace | Key::Delete => {
                if let Some(overlay) = self.overlay.as_mut() {
                    overlay.filter.pop();
                }
                self.refresh_skill_picker_items();
                self.repaint();
            }
            Key::Up => {
                if let Some(overlay) = self.overlay.as_mut() {
                    overlay.selected = overlay.selected.saturating_sub(1);
                }
                self.repaint();
            }
            Key::Down | Key::Tab => {
                let last = self
                    .overlay
                    .as_ref()
                    .map(|overlay| overlay.items.len())
                    .unwrap_or(0);
                if let Some(overlay) = self.overlay.as_mut() {
                    overlay.selected = (overlay.selected + 1).min(last.saturating_sub(1));
                }
                self.repaint();
            }
            Key::Ctrl('c') => self.quit = true,
            _ => return false,
        }
        true
    }

    fn push_skill_query(&mut self, text: &str) {
        if let Some(overlay) = self.overlay.as_mut() {
            overlay.filter.push_str(text);
        }
        self.refresh_skill_picker_items();
        self.repaint();
    }

    fn selected_skill_name(&self) -> Option<String> {
        let overlay = self.overlay.as_ref()?;
        overlay
            .items
            .get(overlay.selected)
            .map(|item| item.id.clone())
    }

    /// Toggle one picker row. A locked marketplace row never toggles — the
    /// picker already shows its `×` mark, `locked by plugin` detail and the
    /// `/marketplace` footer, so refusing here keeps the picker from writing
    /// anything into the transcript — and the picker stays open either way.
    fn skills_toggle(&mut self, skill_name: &str) {
        if self
            .skill_rows
            .iter()
            .any(|row| row.name == skill_name && row.locked)
        {
            return;
        }
        self.toggle_skill(skill_name);
    }

    fn toggle_skill(&mut self, skill_name: &str) {
        if self
            .active_skills
            .iter()
            .any(|skill| skill.name == skill_name)
        {
            self.active_skills.retain(|skill| skill.name != skill_name);
            self.push_cell(
                TranscriptEntry::Skill(TranscriptSkillEntry {
                    at: now_iso(),
                    enabled: false,
                    name: skill_name.to_string(),
                }),
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
        let loaded = crate::cli::skills::load_any_skill_content(&skill, None);
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
                        parent_id: None,
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
                                if record.id == self.session.id {
                                    "▸"
                                } else {
                                    " "
                                },
                                short_id(&record.id),
                                record.updated_at,
                                record.goal_count,
                                record
                                    .last_goal
                                    .clone()
                                    .unwrap_or_else(|| "(no goal yet)".to_string())
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
                if summary.starts_with("Could not read harness state at ")
                    || summary.starts_with("The file at ")
                {
                    self.push_error(summary);
                } else {
                    self.push_info(summary);
                }
            }
            "jobs" => self.jobs_command(args),
            "skills" => {
                // Interactive picker instead of a transcript dump: search,
                // cursor movement and on/off toggles, and nothing lands in
                // scrollback when it closes.
                self.refresh_skill_catalog();
                if self.skill_rows.is_empty() {
                    self.push_info(format!(
                        "no skills found. Add SKILL.md files under {}/<name>/ or ./.drip/skills/<name>/, or register a marketplace with /marketplace add.",
                        self.bootstrap.home.skills_dir
                    ));
                } else {
                    self.open_skill_picker();
                }
            }
            "skill" => {
                if args.is_empty() {
                    self.push_error("Usage: /skill <name>");
                } else {
                    let name = args.split_whitespace().next().unwrap_or("").to_string();
                    self.toggle_skill(&name);
                }
            }
            "mcp" => self.mcp_command(args),
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
                            lines.push(format!(
                                "tool-calling model: {} ({})",
                                route.model, route.url
                            ));
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
        let process_env: Option<BTreeMap<String, String>> = self
            .env_overlay
            .clone()
            .or_else(|| Some(std::env::vars().collect()));
        load_merged_env(
            Path::new(&self.bootstrap.home.env_vars_path),
            process_env.as_ref(),
        )
        .into_iter()
        .collect()
    }

    // ----- mcp ------------------------------------------------------------

    /// The MCP servers a TUI run spawns: every server any role in play names
    /// (`referenced_mcp_servers`, the CLI's rule) plus the run gate `/mcp` set
    /// — the same union the CLI spawns for `--mcp`. `/mcp off` (an explicit
    /// empty gate) spawns nothing at all, and an absent gate leaves the
    /// decision to the roles alone.
    fn mcp_spawn_names(&self, role_args: &ResolveRoleSetupArgs<'_>) -> Vec<String> {
        if self.mcp_gate_off() {
            return Vec::new();
        }
        let mut names = crate::cli::roles::referenced_mcp_servers(role_args);
        for name in self.mcp_run_gate.iter().flatten() {
            if !names.contains(name) {
                names.push(name.clone());
            }
        }
        names
    }

    /// True when `/mcp off` closed the run gate: no server is spawned and no
    /// MCP tool is in scope for any loop, however the roles are configured.
    fn mcp_gate_off(&self) -> bool {
        self.mcp_run_gate
            .as_ref()
            .map(|names| names.is_empty())
            .unwrap_or(false)
    }

    /// The MCP servers configured right now: the global `mcpServers` section
    /// merged with `<cwd>/.drip/mcp.json` (project wins). Read fresh so an edit
    /// to either file shows up in `/mcp` and in the next run without a restart.
    fn configured_mcp_servers(&self) -> crate::tools::mcp::config::McpServerMap {
        crate::tools::mcp::config::load_mcp_servers(
            &self.config.mcp_servers,
            Path::new(&self.bootstrap.cwd),
        )
    }

    /// `/mcp` — the TUI's MCP affordance. With no argument it lists every
    /// configured server with its status; with names it toggles those servers
    /// for the whole run, exactly like `--mcp`; `off` spawns nothing (like
    /// `--no-mcp`) and `roles` hands the decision back to each loop's role.
    fn mcp_command(&mut self, args: &str) {
        let configured = self.configured_mcp_servers();
        let argument = args.trim().to_lowercase();
        if argument.is_empty() {
            self.report_mcp_status(&configured);
            return;
        }
        match argument.as_str() {
            "off" | "none" => {
                self.mcp_run_gate = Some(Vec::new());
                self.push_info(
                    "mcp: every server is off for this run (like --no-mcp). /mcp roles hands the decision back to the roles."
                        .to_string(),
                );
            }
            "roles" | "auto" => {
                self.mcp_run_gate = None;
                self.push_info(
                    "mcp: run gate cleared — each loop sees a server exactly when its role names it in mcpServers."
                        .to_string(),
                );
            }
            _ => {
                let mut unknown: Vec<String> = Vec::new();
                let mut enabled: Vec<String> = self.mcp_run_gate.clone().unwrap_or_default();
                for raw_name in
                    args.split(|character: char| character == ',' || character.is_whitespace())
                {
                    let name = raw_name.trim();
                    if name.is_empty() {
                        continue;
                    }
                    if !configured.contains_key(name) {
                        if !unknown.iter().any(|seen| seen == name) {
                            unknown.push(name.to_string());
                        }
                        continue;
                    }
                    match enabled.iter().position(|seen| seen == name) {
                        Some(index) => {
                            enabled.remove(index);
                        }
                        None => enabled.push(name.to_string()),
                    }
                }
                if !unknown.is_empty() {
                    self.push_error(format!(
                        "mcp: no configured server named {} — /mcp lists the configured servers",
                        unknown.join(", ")
                    ));
                    return;
                }
                // An explicit gate is the set for loops whose role names no
                // server (`--mcp` semantics); a role that names its own keeps
                // them. Toggling everything off leaves an empty gate, i.e. off.
                self.mcp_run_gate = Some(enabled.clone());
                if enabled.is_empty() {
                    self.push_info(
                        "mcp: every server is off for this run (like --no-mcp). /mcp roles hands the decision back to the roles."
                            .to_string(),
                    );
                } else {
                    self.push_info(format!(
                        "mcp: {} on for this run (like --mcp) — the next goal spawns them.",
                        enabled.join(", ")
                    ));
                }
            }
        }
    }

    /// Renders the `/mcp` listing: every configured server with the command it
    /// would run and whether this run may use it, then the gate in one line.
    fn report_mcp_status(&mut self, configured: &crate::tools::mcp::config::McpServerMap) {
        if configured.is_empty() {
            self.push_info(
                "no MCP servers configured. Declare them under a top-level \"mcpServers\" section of ~/.drip/config.json or in ./.drip/mcp.json (see README), then /mcp lists them."
                    .to_string(),
            );
            return;
        }
        let gate = self.mcp_run_gate.clone();
        let mut lines = vec![format!("mcp servers ({} configured):", configured.len())];
        for (name, server) in configured {
            let status = match &gate {
                Some(names) if names.is_empty() => "off for this run".to_string(),
                Some(names) if names.iter().any(|gate| gate == name) => {
                    "on for this run (gate)".to_string()
                }
                Some(_) | None => "role opt-in only".to_string(),
            };
            let mut command = server.command.clone();
            if !server.args.is_empty() {
                command.push(' ');
                command.push_str(&server.args.join(" "));
            }
            lines.push(format!("  {name} — {command} — {status}"));
        }
        lines.push(match &gate {
            Some(names) if names.is_empty() => {
                "run gate: off (like --no-mcp) — no loop gets MCP tools".to_string()
            }
            Some(names) => format!(
                "run gate: {} — the set for loops whose role sets no mcpServers",
                names.join(", ")
            ),
            None => "run gate: none — each loop sees a server when its role names it in mcpServers"
                .to_string(),
        });
        lines.push(
            "/mcp <server>[,<server>] toggles servers for this run (like --mcp); /mcp off turns all off; /mcp roles gives the decision back to the roles."
                .to_string(),
        );
        self.push_info(lines.join("\n"));
    }

    fn marketplace_command(&mut self, args: &str) {
        let parts: Vec<String> = args
            .split_whitespace()
            .map(|part| part.to_string())
            .collect();
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
                    lines.push(format!(
                        "{} ({}: {})",
                        record.name, record.kind, record.source
                    ));
                    for plugin in listing
                        .plugins
                        .iter()
                        .filter(|candidate| candidate.marketplace_name == record.name)
                    {
                        let plugin_enabled =
                            is_marketplace_key_enabled(&plugin.key, &plugin.key, &file, &overrides);
                        lines.push(format!(
                            "  {} {}{}",
                            if plugin_enabled { "●" } else { "○" },
                            plugin.key,
                            if plugin.description.is_empty() {
                                String::new()
                            } else {
                                format!(" — {}", plugin.description)
                            }
                        ));
                        for skill in &plugin.skills {
                            lines.push(format!(
                                "      {} skill {} — {}",
                                if is_marketplace_key_enabled(
                                    &plugin.key,
                                    &skill.key,
                                    &file,
                                    &overrides
                                ) {
                                    "●"
                                } else {
                                    "○"
                                },
                                skill.name,
                                skill.description
                            ));
                        }
                        for role in &plugin.roles {
                            lines.push(format!(
                                "      {} role {}{}",
                                if is_marketplace_key_enabled(
                                    &plugin.key,
                                    &role.key,
                                    &file,
                                    &overrides
                                ) {
                                    "●"
                                } else {
                                    "○"
                                },
                                role.name,
                                role.description
                                    .as_ref()
                                    .map(|text| format!(" — {text}"))
                                    .unwrap_or_default()
                            ));
                        }
                    }
                }
                lines.extend(listing.issues.iter().map(|issue| format!("! {issue}")));
                lines.push(String::new());
                lines.push(
                    "toggle with /plugin enable|disable <marketplace/plugin[/skill]>.".to_string(),
                );
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
                            let own: Vec<_> = listing
                                .plugins
                                .iter()
                                .filter(|plugin| plugin.marketplace_name == record.name)
                                .collect();
                            let skill_count: usize =
                                own.iter().map(|plugin| plugin.skills.len()).sum();
                            let role_count: usize =
                                own.iter().map(|plugin| plugin.roles.len()).sum();
                            let mut lines = vec![format!(
                                "registered \"{}\" ({}): {} plugin(s), {} skill(s), {} role(s).",
                                record.name,
                                record.kind,
                                own.len(),
                                skill_count,
                                role_count
                            )];
                            let needle = format!("\"{}\"", record.name);
                            lines.extend(
                                listing
                                    .issues
                                    .iter()
                                    .filter(|issue| issue.contains(&needle))
                                    .map(|issue| format!("! {issue}")),
                            );
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
                        Ok(updated) if updated.is_empty() => Msg::Info(
                            "nothing to update (local marketplaces read in place).".to_string(),
                        ),
                        Ok(updated) => Msg::Info(format!("updated: {}", updated.join(", "))),
                        Err(error) => Msg::Error(error.to_string()),
                    };
                    let _ = tx.send(message);
                });
            }
            _ => self.push_error(
                "Usage: /marketplace [add <repo> [name] | remove <name> | update [name] | list]",
            ),
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
            extra_bindings: self
                .bootstrap
                .roles_flag
                .as_ref()
                .and_then(|flag| flag.bindings.clone()),
            extra_roles: self
                .bootstrap
                .roles_flag
                .as_ref()
                .map(|flag| flag.roles.clone()),
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
                    parts.push(format!(
                        "loop: {}",
                        serde_json::to_string(loop_config).unwrap_or_default()
                    ));
                }
                if let Some(verified_by) = &role.verified_by {
                    parts.push(format!("verifiedBy: {verified_by}"));
                }
                format!(
                    "{}{}\n    {}",
                    role.name,
                    role.description
                        .as_ref()
                        .map(|text| format!(" — {text}"))
                        .unwrap_or_default(),
                    parts.join(" · ")
                )
            })
            .collect();
        let bindings = role_setup.bindings.as_ref();
        lines.push(format!(
            "bindings: planning={} task={} — tasks may carry their own role from plan_tasks",
            bindings
                .and_then(|b| b.planning.clone())
                .unwrap_or_else(|| "(default)".to_string()),
            bindings
                .and_then(|b| b.task.clone())
                .unwrap_or_else(|| "(default)".to_string())
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
                if let Some(env_name) = profile
                    .api_key_ref
                    .as_deref()
                    .and_then(|reference| reference.strip_prefix("env:"))
                {
                    let env_name = env_name.trim().to_string();
                    if !referenced_by.contains_key(&env_name) {
                        order.push(env_name.clone());
                    }
                    referenced_by
                        .entry(env_name)
                        .or_default()
                        .push(profile.id.clone());
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
                let value = if source == "missing" {
                    String::new()
                } else {
                    merged
                        .get(env_name)
                        .cloned()
                        .unwrap_or_default()
                        .trim()
                        .to_string()
                };
                let fingerprint = if value.is_empty() {
                    String::new()
                } else {
                    let tail: String = value
                        .chars()
                        .rev()
                        .take(4)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect();
                    format!(" ({} chars, …{tail})", value.chars().count())
                };
                lines.push(format!(
                    "{mark}  {env_name}{fingerprint} — {}",
                    referenced_by
                        .get(env_name)
                        .map(|ids| ids.join(", "))
                        .unwrap_or_default()
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
        builtin_tool_pack(self.tool_options())
            .iter()
            .map(|tool| tool.name.clone())
            .collect()
    }

    // ----- goals ----------------------------------------------------------

    fn run_goal(&mut self, goal_text: String) {
        let goal_images = std::mem::take(&mut self.attachments);
        self.run_session_id = Some(self.session.id.clone());
        self.running = true;
        // The activity line above the composer starts its clock here and
        // animates on the shared spinner cadence while the run is in flight.
        self.run_started_at = Some(Instant::now());
        self.activity_next_tick = Some(Instant::now() + Duration::from_millis(SPINNER_INTERVAL_MS));
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
                images: goal_images
                    .iter()
                    .map(|attachment| attachment.path.clone())
                    .collect(),
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
        let cwd_path = self.bootstrap.cwd.clone();
        let cwd = Path::new(&cwd_path);
        // The configured MCP server set (global `mcpServers` merged with
        // <cwd>/.drip/mcp.json, project wins) is read once per run: roles
        // validate their `mcpServers` against its names, and the spawn helper
        // below looks commands up in it.
        let mcp_configured = crate::tools::mcp::config::load_mcp_servers(
            &self.config.mcp_servers,
            Path::new(&cwd_path),
        );
        let mut role_args = ResolveRoleSetupArgs {
            config: &self.config,
            cwd: self.bootstrap.cwd.clone(),
            env: Some(&env),
            extra_bindings: self
                .bootstrap
                .roles_flag
                .as_ref()
                .and_then(|flag| flag.bindings.clone()),
            extra_roles: self
                .bootstrap
                .roles_flag
                .as_ref()
                .map(|flag| flag.roles.clone()),
            marketplace_roles: Some(
                list_enabled_marketplace_roles(cwd, &self.bootstrap.home).unwrap_or_default(),
            ),
            skills: discover_all_skills(cwd, &self.bootstrap.home).unwrap_or_default(),
            tool_names: self.tool_names(),
            // Same merged set the CLI validates against (global mcpServers plus
            // <cwd>/.drip/mcp.json), so both callers report the same unknowns.
            mcp_server_names: mcp_configured.keys().cloned().collect(),
        };
        // MCP clients, by the CLI's rule: every server any role in play names,
        // plus the ones `/mcp` turned on for this run (the TUI's --mcp), is
        // spawned once here, so the pack the roles are validated against already
        // carries its tools. Never fatal: a dead or unconfigured server becomes
        // a transcript line and only costs its tools for this run.
        let mcp_gate = self.mcp_run_gate.clone();
        let mcp_names = self.mcp_spawn_names(&role_args);
        let (mcp_clients, mcp_warnings) =
            spawn_mcp_clients_for_run(&mcp_names, &mcp_configured, cwd);
        // Role `tools` allowlists may name MCP__<server>__<tool> entries.
        role_args.tool_names.extend(
            crate::tools::mcp::mcp_tool_definitions(&mcp_clients)
                .into_iter()
                .map(|tool| tool.name),
        );
        let role_setup = resolve_role_setup(&role_args);
        // The resolver still holds `&self.config`; release it before the
        // transcript pushes below need `&mut self`.
        drop(role_args);
        for warning in &mcp_warnings {
            self.push_info(warning.clone());
        }
        if !mcp_names.is_empty() {
            self.push_info(format!(
                "mcp: spawning {} for this run",
                mcp_names.join(", ")
            ));
        } else if self.mcp_gate_off() {
            self.push_info(
                "mcp: off for this run (/mcp roles re-enables role opt-ins)".to_string(),
            );
        }
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
        let task_loop_limit = self.bootstrap.task_loop_limit;
        let review_waiver_lines = self.bootstrap.review_waiver_lines;
        let plan_mode = self.bootstrap.plan_mode.clone();
        let no_repo_memory = self.bootstrap.no_repo_memory;
        let ask_user_enabled = self.bootstrap.ask;
        let ask_user_timeout_seconds = self.bootstrap.ask_timeout_secs;
        let tool_options = self.tool_options();
        let skills = self.active_skills.clone();
        let redact_secrets =
            load_env_vars(Path::new(&self.bootstrap.home.env_vars_path)).unwrap_or_default();
        let goal_context = resolved.context_block.clone();
        let mentions = resolved.mentions.clone();
        let images: Vec<String> = goal_images
            .iter()
            .map(|attachment| attachment.data_url.clone())
            .collect();
        let hooks = self.config.hooks.clone();
        // Cloned before the run thread starts: the thread owns its own handle
        // to the SAME runtime, so jobs it starts stay visible after it exits.
        let tool_services = self.tool_services.clone();

        // The skill classifier is config-driven on every surface, the TUI
        // included: a resolved `runtime.classifier_profile_id` turns it on here
        // exactly as it does headless, and `runtime.classifier_in_tui = "false"`
        // keeps the TUI on its explicit /skill toggles only. Resolved before
        // the run thread starts so the announce line and any warnings land in
        // this run's transcript — at the cost of a one-time stall before the
        // first paint on a cold requirements cache (one HTTP round-trip per
        // unknown skill); moving the build behind the run task is a follow-up.
        let classifier_pool = {
            let settings = self.config.settings.clone();
            if crate::cli::classifier_pool::tui_classifier_pool_enabled(
                &settings,
                self.bootstrap.no_classifier,
            ) {
                let env = self.merged_env();
                let active_names: std::collections::HashSet<String> =
                    skills.iter().map(|skill| skill.name.clone()).collect();
                let tools = builtin_tool_pack(tool_options.clone());
                let pool_args = crate::cli::classifier_pool::ClassifierPoolArgs {
                    active_skill_names: &active_names,
                    cwd: &self.bootstrap.cwd,
                    disabled: false,
                    env: &env,
                    home: &self.bootstrap.home,
                    // This pass is handed the built-in pack and no MCP
                    // entries, so a skill whose requirements need MCP is
                    // classified as unsatisfiable here. The headless path
                    // passes one entry per server it spawned (see entry.rs,
                    // which loops its `mcp_clients`); threading this run's
                    // `mcp_clients` through is left as a separate change.
                    mcp_servers: Vec::new(),
                    profile_override: self.bootstrap.classifier.as_deref(),
                    settings: &settings,
                    tools: &tools,
                };
                match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime.block_on(
                        crate::cli::classifier_pool::build_classifier_pool(pool_args),
                    ),
                    // Every other failure in this sequence reports through the
                    // transcript; a runtime that cannot be built must not be the
                    // one exception, or classification silently disappears.
                    Err(error) => {
                        self.push_error(format!(
                            "classifier: tokio runtime unavailable ({error}) — skill classification is off for this run"
                        ));
                        Default::default()
                    }
                }
            } else {
                Default::default()
            }
        };
        for warning in &classifier_pool.warnings {
            self.push_info(warning.clone());
        }
        if let Some(announce) = classifier_pool.announce.clone() {
            self.push_info(announce);
        }

        std::thread::spawn(move || {
            // A panic anywhere below must still release the composer: the
            // guard reports it as a failed run unless the thread finishes normally.
            let mut guard = RunDoneGuard {
                tx: tx.clone(),
                armed: true,
            };
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    guard.armed = false;
                    let _ = tx.send(Msg::RunDone(Err(SessionGoalError::Run(error.to_string()))));
                    return;
                }
            };
            let index = open_session_index(&index_db_path);
            // Same nested attribution as the headless path: a skill that shells
            // out to `drip ...` from this run records this session as its
            // parent through the DRIP_SESSION_ID fallback in create_session.
            // Scoped to the run: once it ends the variable goes back to what it
            // was, so a session `/new` creates afterwards is a root, not a child
            // of a finished run.
            let session_env = SessionEnvScope::enter(&session.id);
            let on_event: Arc<dyn Fn(HarnessEvent) + Send + Sync> =
                Arc::new(move |event: HarnessEvent| {
                    let _ = event_tx.send(Msg::Event(event));
                });
            let result = runtime.block_on(run_session_goal(SessionGoalArgs {
                ask_user_enabled,
                ask_user_timeout_seconds,
                // Config-driven like the headless path: the route and the
                // pool were resolved (settings + env) before this run thread
                // started.
                classifier: classifier_pool.route.clone(),
                skill_pool: classifier_pool.skills.clone(),
                cwd,
                goal: goal_text,
                goal_context,
                goal_images: if images.is_empty() {
                    None
                } else {
                    Some(images)
                },
                hooks,
                index: &index,
                inference,
                max_iterations,
                max_loops,
                task_loop_limit,
                review_waiver_lines,
                plan_mode,
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
                roles: if role_setup.roles.is_empty() {
                    None
                } else {
                    Some(role_setup.roles.clone())
                },
                session: &session,
                signal: Some(signal),
                skills,
                summarize_run: None,
                lite: false,
                no_review: false,
                tools: {
                    // MCP server tools ride on every goal run, exactly as they do
                    // headless: the definitions come from the clients spawned
                    // above (zero when none survived).
                    let mut pack = builtin_tool_pack(tool_options.clone());
                    pack.extend(crate::tools::mcp::mcp_tool_definitions(&mcp_clients));
                    pack
                },
                // The TUI keeps this registry past the run and hands a
                // settled monitor back to the session, so the run must not
                // hold for one: see `handoff_settled_background_jobs`.
                monitor_background_handoff: true,
                tool_services: Some(tool_services),
                // `/mcp` sets the run gate — the CLI's `--mcp`: `None` leaves
                // the decision to the roles (a loop sees an MCP server exactly
                // when its role names it in `mcpServers`), a list is the set for
                // loops whose role names none, and an empty list is `/mcp off`.
                mcp_servers: mcp_gate.clone(),
            }));
            index.close();
            // Restored before the main thread learns the run is over, so no
            // command it handles next can see the finished run's id.
            drop(session_env);
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
                self.push_error(format!(
                    "{} (Another process owns this session's run right now.)",
                    error.message()
                ));
            }
            Err(SessionGoalError::Run(message)) => self.push_error(message),
        }
        self.finish_run();
        // A prompt queued while the run was in flight is what the operator
        // wanted next: the run ending is the moment the queue drains into a
        // fresh goal. Steering (ctrl+s) is the other way out of it.
        if let Some(next) = self.queued_prompts.pop_front() {
            if !next.trim().is_empty() {
                self.run_goal(next);
            }
        }
        // A background job (a MONITOR above all) may have settled while this
        // run was finishing. Hand a settled one to the session as its next
        // message instead of leaving the result with no reader.
        self.handoff_settled_background_jobs();
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
    fn begin_title_with(&mut self, goal_text: &str, env: &HashMap<String, String>, is_tty: bool) {
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
    /// or generated-name word rules. Works while a goal runs too: the new
    /// label lands on the busy title (spinner intact), and the epoch bumps
    /// make any in-flight auto-title reply stale so it cannot clobber it.
    fn rename(&mut self, args: &str) {
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
        self.session_name = Some(name.to_string());
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
        self.session_name = Some(name.clone());
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
        // The activity block goes with the run: its clock has nothing left to
        // count, its transient rows are erased (a live view, never scrollback)
        // and its animation must stop repainting.
        self.activity_notices.clear();
        self.activity_pending.clear();
        self.activity_shown_at = None;
        self.run_started_at = None;
        self.activity_next_tick = None;
        // Idle title (bare label, no spinner) whatever ended the run:
        // completion, cancel, or error.
        self.title_next_tick = None;
        if let Some(title) = self.pane_title.as_mut() {
            let escape = title.set_busy(false, Instant::now());
            crate::tui::pane_title::emit(escape.as_deref());
        }
    }

    /// Mirrors the operator-blocked state onto the pane title: while an
    /// ask_user survey waits for an answer the spinner becomes `?` (a spinner
    /// would claim progress the blocked run is not making); recording the
    /// answers, dismissing the survey, or ending the run restores it.
    fn set_title_waiting(&mut self, waiting: bool) -> Option<String> {
        let escape = self
            .pane_title
            .as_mut()
            .and_then(|title| title.set_waiting(waiting, Instant::now()));
        crate::tui::pane_title::emit(escape.as_deref());
        escape
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
            // A queued transient batch goes up when its own window closes,
            // not only when the spinner tick happens to land.
            if let Some(at) = self.activity_debounce_deadline() {
                if now >= at && self.running && self.settle_activity_notices(now) {
                    self.repaint();
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

            // Activity line above the composer: consume a due tick here and
            // repaint, so the spinner frame and the counting clock advance
            // without the paint path re-arming anything. Re-armed only while
            // a run is in flight; the loop otherwise waits as before.
            if let Some(at) = self.activity_next_tick {
                if now >= at {
                    self.activity_next_tick = None;
                    if self.running {
                        // A queued transient batch goes up at its debounce
                        // deadline, from the loop, never from the paint path.
                        self.settle_activity_notices(now);
                        self.repaint();
                        self.activity_next_tick =
                            Some(now + Duration::from_millis(SPINNER_INTERVAL_MS));
                    }
                }
            }

            // Custom status line: adopt finished jobs and re-arm the
            // interval refresh here, never in the paint path (drawing stays
            // side-effect-free). Non-blocking; failures never retry or log.
            self.poll_status_line();

            // A background job that settles while the session sits idle has no
            // run to report to; hand its result back to the session as the
            // next message from here rather than only counting it.
            self.handoff_settled_background_jobs();

            // Background jobs: re-read the shared runtime's running set on its
            // own deadline (drawing stays side-effect-free), and repaint when
            // the status-line counter changed or the browser is looking at a
            // list that just moved under it.
            if self.jobs_next_refresh.is_none_or(|at| now >= at) {
                let changed = self.refresh_jobs(now);
                let browsing = self
                    .overlay
                    .as_ref()
                    .is_some_and(|overlay| overlay.kind == OverlayKind::Jobs);
                if changed || browsing {
                    self.repaint();
                }
            }

            let mut wait = Duration::from_millis(300);
            for deadline in [
                self.flush_deadline,
                self.resize_at,
                self.status_line_next_refresh,
                self.jobs_next_refresh,
                self.title_next_tick,
                self.activity_next_tick,
                self.activity_debounce_deadline(),
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
                        if let Some(survey) = event
                            .data
                            .as_ref()
                            .and_then(|data| data.question_survey.clone())
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
                    if matches!(&result, Err(SessionGoalError::Run(message)) if message == RUN_THREAD_PANIC)
                    {
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
            while let Ok(message) =
                rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
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
            let _ = self.tx.send(Msg::RunDone(Err(SessionGoalError::Run(
                RUN_THREAD_PANIC.to_string(),
            ))));
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
                Ok(files) => {
                    get_workspace_file_suggestions(&files, &query, DEFAULT_FILE_SUGGESTION_LIMIT)
                }
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
    // Mouse reporting (normal tracking + SGR coordinates) so a left click on
    // the status-line background chip reaches the input path as
    // `ESC [ < 0 ; col ; row M`. Only for an interactive stdout: redirected
    // output must not collect the escapes, and there is nothing to click.
    let mouse_reporting = stdout_is_tty();
    if mouse_reporting {
        write_out(ENABLE_MOUSE);
    }

    // Inline images are opted into exactly once, for an interactive stdout:
    // detection is a no-op for piped output, and `DRIP_IMAGE_PROTOCOL`
    // overrides it either way. Nothing is announced at startup — the session
    // opens without debug noise; the active protocol is reported on demand
    // (attaching an image with Ctrl+V) and documented in the README.
    crate::tui::images::set_inline_images(crate::tui::images::detect_image_protocol());

    // The terminal is restored even if a panic unwinds through the loop.
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let mouse = if mouse_reporting { DISABLE_MOUSE } else { "" };
        write_out(&format!("{SHOW_CURSOR}{mouse}{DISABLE_BRACKETED_PASTE}\n"));
        previous_hook(info);
    }));

    spawn_stdin_reader(tx.clone());
    spawn_mention_indexer(cwd, mention_rx, tx.clone());

    let mut app = TuiApp::new(bootstrap, tx, mention_tx);
    let code = app.run(rx);

    if mouse_reporting {
        write_out(DISABLE_MOUSE);
    }
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
                Key::Mouse(_) => "mouse",
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
        assert_eq!(
            kinds(&decode_input("é".as_bytes(), &mut paste)),
            vec!["text"]
        );
        // Ctrl+s is the steer key: a single raw-mode byte on every terminal.
        match &decode_input(b"\x13", &mut paste)[0] {
            Key::Ctrl(c) => assert_eq!(*c, 's'),
            _ => panic!("ctrl+s expected"),
        }
        // A bare LF (ctrl+enter on some terminals) stays a plain return so a
        // stray newline can never steer.
        assert_eq!(kinds(&decode_input(b"\n", &mut paste)), vec!["return"]);
        assert_eq!(kinds(&decode_input(b"a\r", &mut paste)), vec!["paste"]);
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
        assert_eq!(
            kinds(&decode_input(b"\x1b[A\x1b[A", &mut paste)),
            vec!["up", "up"]
        );
        assert_eq!(
            kinds(&decode_input(b"\x1b[Da", &mut paste)),
            vec!["left", "text"]
        );
        assert_eq!(
            kinds(&decode_input(b"\x1b[1;5C", &mut paste)),
            vec!["ignored"]
        );
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
            assert!(
                text.contains(&format!("/{}", command.name)),
                "{}",
                command.name
            );
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
        let row = custom_status_row_from(Some(&output("\x1b[32mok\x1b[0m", true)), 10, 0).unwrap();
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
        let mut runner = crate::tui::status_line::StatusLineRunner::new(command_setting("true"));
        runner.shutdown();
        assert!(!runner.request_refresh(bare_request()));
    }
}

#[cfg(test)]
mod pane_title_lifecycle_tests {
    use super::*;
    use crate::tui::pane_title::fallback_title;

    fn enabled_settings() -> indexmap::IndexMap<String, String> {
        // Baseline (empty profile lists), not the first-run seed: these tests
        // must not depend on the compiled-in catalogs.
        crate::core::config::baseline_setting_values()
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
        assert!(
            apply_title_result(None, 0, 0, Some("unused".to_string()), Instant::now()).is_none()
        );
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
        assert!(
            !label.contains('\x1b') && !label.contains('\x07'),
            "{label:?}"
        );
        let title = crate::tui::pane_title::osc2(&label);
        assert!(
            title.starts_with("\x1b]2;") && title.ends_with('\x07'),
            "{title:?}"
        );
        assert!(
            !title.contains("pwn") && !title.contains("\x1b[2J"),
            "{title:?}"
        );
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
    pub(super) fn rename_app(dir: &Path) -> TuiApp {
        let root = dir.to_string_lossy().to_string();
        let home = crate::core::home::open_drip_home(&root);
        let project =
            crate::core::home::resolve_drip_project("/tmp", &root, None).expect("project resolves");
        let project = crate::core::home::ensure_drip_project(&project);
        let index = crate::core::sessions::open_session_index(&project.index_db_path);
        let session = crate::core::sessions::create_session(
            &index,
            crate::core::sessions::CreateSessionArgs {
                cwd: "/tmp".to_string(),
                parent_id: None,
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
            task_loop_limit: None,
            review_waiver_lines: None,
            plan_mode: None,
            no_repo_memory: false,
            project,
            roles_flag: None,
            session,
            status_line: None,
            classifier: None,
            no_classifier: false,
        };
        let (tx, _rx) = mpsc::channel::<Msg>();
        let (mention_tx, _mention_rx) = mpsc::channel::<(u64, String)>();
        let mut app = TuiApp::new(bootstrap, tx, mention_tx);
        // Strip the shell's credentials: with OPENROUTER_API_KEY present the
        // /rename auto-name path resolves a route and spawns a real request
        // thread instead of the "no profile configured" error branch.
        app.env_overlay = Some(BTreeMap::new());
        app
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
        assert!(!should_request_auto_title(
            true,
            &settings,
            false,
            Some("User Chosen Name")
        ));
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
    fn rename_command_is_recognized_and_never_starts_a_goal() {
        let dir = temp_dir("dispatch");
        let _home = TempHome(dir.clone());
        let mut app = rename_app(&dir);
        // /rename is recognized (it never starts a goal). With no inference
        // profile configured it refuses before scheduling anything.
        app.dispatch_command("rename", "");
        assert!(!app.running);
        assert_eq!(app.rename_epoch, 0);
    }

    #[test]
    fn manual_rename_applies_while_a_goal_is_running() {
        let dir = temp_dir("busy-manual");
        let _home = TempHome(dir.clone());
        let mut app = rename_app(&dir);
        app.running = true;
        app.pane_title = Some(PaneTitle::new("goal fallback"));
        app.pane_title
            .as_mut()
            .unwrap()
            .set_busy(true, Instant::now());
        let title_epoch = app.title_epoch;
        app.dispatch_command("rename", "Mid-run name");
        assert!(app.running, "/rename must not end or restart the run");
        assert_eq!(
            read_session_name(Path::new(&app.paths.meta_path)).as_deref(),
            Some("Mid-run name"),
            "the literal name persists to session.json during the run"
        );
        let title = app.pane_title.as_ref().unwrap();
        assert_eq!(title.label(), "Mid-run name");
        assert!(title.is_busy(), "renaming keeps the spinner running");
        assert!(
            app.title_epoch > title_epoch,
            "an in-flight auto-title reply must become stale"
        );
    }

    #[test]
    fn submit_dispatches_rename_immediately_while_running_but_queues_other_input() {
        let dir = temp_dir("busy-submit");
        let _home = TempHome(dir.clone());
        let mut app = rename_app(&dir);
        app.running = true;
        app.text = "/rename Renamed live".to_string();
        app.submit();
        assert!(
            app.queued_prompts.is_empty(),
            "/rename must not wait in the queue"
        );
        assert_eq!(
            read_session_name(Path::new(&app.paths.meta_path)).as_deref(),
            Some("Renamed live")
        );
        // Other slash commands and plain text still queue for the next run.
        app.text = "/help".to_string();
        app.submit();
        app.text = "next goal".to_string();
        app.submit();
        assert_eq!(app.queued_prompts.len(), 2);
        assert_eq!(app.queued_prompts[0], "/help");
        assert_eq!(app.queued_prompts[1], "next goal");
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
        assert_ne!(
            label(&app),
            stale_name,
            "stale rename must not clobber /new"
        );
        assert_eq!(label(&app), fresh);

        // /resume is the other switch path; the same protection applies.
        let index = crate::core::sessions::open_session_index(&app.bootstrap.project.index_db_path);
        let other = crate::core::sessions::create_session(
            &index,
            crate::core::sessions::CreateSessionArgs {
                cwd: app.bootstrap.cwd.clone(),
                parent_id: None,
                project: &crate::core::sessions::ProjectPaths::from(&app.bootstrap.project),
                now: "",
            },
        );
        index.close();
        app.rename_epoch = 10;
        app.dispatch_command("resume", &other.id);
        assert_eq!(app.rename_epoch, 11, "/resume must bump the rename epoch");
        app.apply_rename_result(10, Some(stale_name.to_string()));
        assert_eq!(
            label(&app),
            FALLBACK_LABEL,
            "stale rename must not clobber /resume"
        );
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
        assert_eq!(
            label(&app),
            multiword,
            "resume must restore the manual name"
        );
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
            parent_id: None,
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
            task_loop_limit: None,
            review_waiver_lines: None,
            plan_mode: None,
            no_repo_memory: true,
            project: drip_project,
            roles_flag: None,
            session,
            status_line: None,
            classifier: None,
            no_classifier: false,
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
            "/skills must open the picker, never activate the same-named skill"
        );
        assert_eq!(
            fixture.app.overlay.as_ref().map(|overlay| overlay.kind),
            Some(OverlayKind::Skills),
            "/skills must open the interactive picker"
        );
        fixture.app.overlay = None;
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
    fn slash_skills_opens_an_interactive_picker_without_writing_history() {
        let mut fixture = make_app_with_skills(&["navis", "nada"]);
        let roots = fixture.app.cells.len();
        fixture.app.dispatch_command("skills", "");
        assert_eq!(
            fixture.app.overlay.as_ref().map(|overlay| overlay.kind),
            Some(OverlayKind::Skills),
            "/skills opens the picker"
        );
        assert_eq!(
            fixture.app.cells.len(),
            roots,
            "the picker must not dump a skill list into the transcript"
        );
        assert!(
            fixture.app.overlay.as_ref().unwrap().items.len() >= 2,
            "the project skills are listed"
        );

        // typing filters the list live
        fixture.app.on_key(Key::Text("nad".to_string()));
        let items = &fixture.app.overlay.as_ref().unwrap().items;
        assert_eq!(items.len(), 1, "only the nada row matches: {items:?}");
        assert_eq!(items[0].id, "nada");
        for _ in 0..3 {
            fixture.app.on_key(Key::Backspace);
        }
        assert!(
            fixture.app.overlay.as_ref().unwrap().items.len() >= 2,
            "backspace widens the list again"
        );

        // enter toggles the highlighted skill on and keeps the picker open
        fixture.app.on_key(Key::Text("navis".to_string()));
        assert_eq!(
            fixture.app.overlay.as_ref().unwrap().items.len(),
            1,
            "navis is the only match"
        );
        fixture.app.on_key(Key::Return);
        assert_eq!(fixture.app.active_skills.len(), 1);
        assert_eq!(fixture.app.active_skills[0].name, "navis");
        assert!(
            fixture.app.overlay.is_some(),
            "toggling keeps the picker open"
        );

        // space toggles it back off, still without closing
        fixture.app.on_key(Key::Text(" ".to_string()));
        assert!(fixture.app.active_skills.is_empty());
        assert!(fixture.app.overlay.is_some());

        // esc closes leaving no list in the transcript
        fixture.app.on_key(Key::Escape);
        assert!(fixture.app.overlay.is_none());
        let dumps = fixture
            .app
            .cells
            .iter()
            .filter(|entry| matches!(entry, TranscriptEntry::Info(note) if note.text.contains("test skill")))
            .count();
        assert_eq!(dumps, 0, "no skill list may land in the transcript");
    }

    #[test]
    fn skill_picker_rows_carry_origin_and_rough_size() {
        let fixture = make_app_with_skills(&["navis"]);
        let row = fixture
            .app
            .skill_rows
            .iter()
            .find(|row| row.name == "navis")
            .expect("the project skill has a picker row");
        assert_eq!(row.source, SkillSource::Project);
        assert!(row.tokens >= 1, "every row carries a size estimate");
        assert!(
            fixture
                .app
                .skill_rows
                .iter()
                .any(|row| row.source == SkillSource::Builtin),
            "built-in skills are listed too"
        );
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

    #[test]
    fn enter_while_running_queues_instead_of_starting_a_second_run() {
        let mut fixture = super::prompt_history_wiring_tests::make_history_app(&[]);
        fixture.app.running = true;
        fixture.app.text = "next goal".to_string();
        fixture.app.on_key(Key::Return);
        assert_eq!(fixture.app.queued_prompts.len(), 1);
        assert_eq!(fixture.app.queued_prompts[0], "next goal");
        assert!(
            fixture.app.text.is_empty(),
            "the composer clears once queued"
        );
        assert!(fixture.app.abort.is_none(), "queuing must not start a run");
        assert!(
            !Path::new(&fixture.app.paths.inbox_path).exists(),
            "a queued prompt must not steer the live run"
        );
    }

    #[test]
    fn queued_prompts_stay_out_of_the_timeline() {
        let mut fixture = super::prompt_history_wiring_tests::make_history_app(&[]);
        fixture.app.running = true;
        fixture.app.text = "next goal".to_string();
        fixture.app.on_key(Key::Return);
        assert!(
            !fixture.app.cells.iter().chain(fixture.app.pending_cells.iter()).any(
                |entry| matches!(entry, TranscriptEntry::Info(note) if note.text.contains("queued"))
            ),
            "the queue is listed above the composer, never logged as history"
        );
    }

    #[test]
    fn ctrl_s_on_an_empty_composer_steers_with_the_whole_queue_and_flushes_it() {
        let mut fixture = super::prompt_history_wiring_tests::make_history_app(&[]);
        fixture.app.running = true;
        fixture.app.text = "first".to_string();
        fixture.app.on_key(Key::Return);
        fixture.app.text = "second".to_string();
        fixture.app.on_key(Key::Return);
        assert_eq!(fixture.app.queued_prompts.len(), 2);

        fixture.app.on_key(Key::Ctrl('s'));

        assert!(
            fixture.app.queued_prompts.is_empty(),
            "an empty steer flushes the whole queue"
        );
        let raw = std::fs::read_to_string(&fixture.app.paths.inbox_path)
            .expect("steering writes the session inbox");
        let texts: Vec<String> = raw
            .lines()
            .map(|line| {
                serde_json::from_str::<serde_json::Value>(line).unwrap()["text"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(
            texts,
            vec!["first", "second"],
            "every queued message steers, in order"
        );
        assert!(
            fixture.app.cells.iter().any(
                |entry| matches!(entry, TranscriptEntry::Info(note) if note.text.contains("2 queued messages"))
            ),
            "the steer is reported once"
        );
    }

    #[test]
    fn ctrl_s_with_typed_text_steers_only_that_text_and_keeps_the_queue() {
        let mut fixture = super::prompt_history_wiring_tests::make_history_app(&[]);
        fixture.app.running = true;
        fixture.app.text = "queued".to_string();
        fixture.app.on_key(Key::Return);
        fixture.app.text = "steer me now".to_string();
        fixture.app.on_key(Key::Ctrl('s'));

        assert_eq!(
            fixture.app.queued_prompts.len(),
            1,
            "the queue is untouched"
        );
        assert_eq!(fixture.app.queued_prompts[0], "queued");
        let raw = std::fs::read_to_string(&fixture.app.paths.inbox_path)
            .expect("steering writes the session inbox");
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 1);
        let parsed: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(parsed["text"], "steer me now");
        assert!(parsed["at"].as_str().is_some(), "{parsed}");
        assert!(fixture.app.text.is_empty());
    }

    #[test]
    fn ctrl_s_while_running_with_an_empty_queue_steers_the_typed_message() {
        let mut fixture = super::prompt_history_wiring_tests::make_history_app(&[]);
        fixture.app.running = true;
        fixture.app.text = "steer me now".to_string();
        fixture.app.on_key(Key::Ctrl('s'));
        let raw = std::fs::read_to_string(&fixture.app.paths.inbox_path)
            .expect("steering writes the session inbox");
        let parsed: serde_json::Value = serde_json::from_str(raw.lines().next().unwrap()).unwrap();
        assert_eq!(parsed["text"], "steer me now");
        assert!(fixture.app.text.is_empty());
        assert!(fixture.app.queued_prompts.is_empty());
    }

    #[test]
    fn ctrl_s_with_nothing_to_steer_reports_it_and_writes_no_inbox() {
        let mut fixture = super::prompt_history_wiring_tests::make_history_app(&[]);
        fixture.app.running = true;
        fixture.app.on_key(Key::Ctrl('s'));
        assert!(!Path::new(&fixture.app.paths.inbox_path).exists());
        assert!(
            fixture
                .app
                .cells
                .iter()
                .any(|entry| matches!(entry, TranscriptEntry::Info(note) if note.text.contains("nothing to steer"))),
            "an empty steer must say so"
        );
    }

    #[test]
    fn run_end_drains_the_queue_into_the_next_goal() {
        let mut fixture = super::prompt_history_wiring_tests::make_history_app(&[]);
        fixture.app.running = true;
        fixture.app.text = "queued goal".to_string();
        fixture.app.on_key(Key::Return);
        fixture
            .app
            .on_run_done(Err(SessionGoalError::Run("boom".to_string())));
        assert!(
            fixture.app.queued_prompts.is_empty(),
            "the run ending drains the queue"
        );
        assert!(
            fixture.app.cells.iter().any(
                |entry| matches!(entry, TranscriptEntry::Goal(goal) if goal.text == "queued goal")
            ),
            "the queued prompt becomes the next goal"
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

    pub(super) struct HistoryFixture {
        pub(super) app: TuiApp,
        _cwd: tempfile::TempDir,
        _home: tempfile::TempDir,
        _project: tempfile::TempDir,
        _rx: mpsc::Receiver<Msg>,
        _mention_rx: mpsc::Receiver<(u64, String)>,
    }

    pub(super) fn make_history_app(skills: &[&str]) -> HistoryFixture {
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
            parent_id: None,
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
            task_loop_limit: None,
            review_waiver_lines: None,
            plan_mode: None,
            no_repo_memory: true,
            project: drip_project,
            roles_flag: None,
            session,
            status_line: None,
            classifier: None,
            no_classifier: false,
        };
        let (tx, rx) = mpsc::channel::<Msg>();
        let (mention_tx, mention_rx) = mpsc::channel::<(u64, String)>();
        let mut app = TuiApp::new(bootstrap, tx, mention_tx);
        // Strip the shell's credentials (e.g. OPENROUTER_API_KEY): with one
        // present the goal run resolves a route and keeps `running` true,
        // which breaks the deterministic Up/Down recall assertions.
        app.env_overlay = Some(BTreeMap::new());
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
    fn up_on_a_wrapped_continuation_line_moves_the_cursor_and_does_not_recall() {
        let mut fixture = make_history_app(&[]);
        fixture.app.cols = 20;
        type_into(&mut fixture.app, "recorded goal");
        fixture.app.submit();
        let wrapped = "aaaa bbbb cccc dddd eeee";
        type_into(&mut fixture.app, wrapped);
        // Body width 17 at 20 columns: the draft wraps into "aaaa bbbb cccc " +
        // the tail, so the end-of-text cursor sits on the second visual line.
        assert_eq!(composer_text_width(20), 17);
        assert_eq!(composer_lines(wrapped, 17), vec![0..15, 15..24]);
        fixture.app.on_key(Key::Up);
        assert_eq!(fixture.app.text, wrapped, "no recall from a lower line");
        assert!(!fixture.app.prompt_history.is_browsing());
        assert_eq!(fixture.app.cursor, 9, "same column on the line above");

        // From the top visual line the same key recalls the older prompt.
        fixture.app.apply_edit(wrapped.to_string(), 3);
        fixture.app.on_key(Key::Up);
        assert_eq!(fixture.app.text, "recorded goal");
    }

    #[test]
    fn up_on_a_lower_line_of_a_multiline_draft_moves_instead_of_recalling() {
        let mut fixture = make_history_app(&[]);
        fixture.app.cols = 40;
        type_into(&mut fixture.app, "recorded goal");
        fixture.app.submit();
        let draft = "first line\nsecond line";
        type_into(&mut fixture.app, draft);
        fixture.app.on_key(Key::Up);
        assert_eq!(fixture.app.text, draft, "no recall from the second line");
        assert!(!fixture.app.prompt_history.is_browsing());
        assert_eq!(fixture.app.cursor, 10, "column clamped to the shorter line");

        fixture.app.on_key(Key::Up);
        assert_eq!(fixture.app.text, "recorded goal", "top line recalls");
    }

    #[test]
    fn down_moves_within_a_recalled_entry_before_stepping_newer() {
        let mut fixture = make_history_app(&[]);
        fixture.app.cols = 40;
        type_into(&mut fixture.app, "newest line");
        fixture.app.submit();
        let older = "alpha\nbeta\ngamma";
        type_into(&mut fixture.app, older);
        fixture.app.submit();
        type_into(&mut fixture.app, "unsent draft");

        fixture.app.on_key(Key::Up);
        assert_eq!(fixture.app.text, older);
        // Cursor at the end of "alpha": Down stays inside the entry.
        fixture.app.apply_edit(older.to_string(), 5);
        fixture.app.on_key(Key::Down);
        assert_eq!(fixture.app.text, older, "still inside the recalled entry");
        assert_eq!(fixture.app.cursor, 10, "one visual line down");
        fixture.app.on_key(Key::Down);
        assert_eq!(fixture.app.text, older, "still inside the recalled entry");
        assert_eq!(fixture.app.cursor, 15, "second line down, column clamped");

        // On the last visual line Down leaves the entry: this entry IS the
        // newest, so it restores the exact draft in one step.
        fixture.app.on_key(Key::Down);
        assert_eq!(fixture.app.text, "unsent draft", "exact draft restored");
        fixture.app.on_key(Key::Down);
        assert_eq!(fixture.app.text, "unsent draft", "browsing ended: no-op");
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
        // Recall leaves the cursor at the end of the multiline entry, which is
        // its LAST visual line: one Up moves to the first line before the walk
        // continues older.
        fixture.app.on_key(Key::Up);
        assert_eq!(
            fixture.app.text, older_multiline,
            "second Up moves inside the entry"
        );
        assert_eq!(
            fixture.app.cursor, 12,
            "top line, column clamped to its end"
        );
        fixture.app.on_key(Key::Up);
        assert_eq!(
            fixture.app.text, "newest single line",
            "Up from the first line walks older"
        );
        fixture.app.on_key(Key::Up);
        assert_eq!(fixture.app.text, "newest single line", "clamped at oldest");

        fixture.app.on_key(Key::Down);
        assert_eq!(fixture.app.text, older_multiline, "Down steps newer");
        // A recalled multiline entry leaves the cursor on its last visual line,
        // so the next Down restores the exact draft.
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
        // While browsing, Down first walks the cursor down the recalled entry;
        // only from its last visual line does it step newer and restore the
        // exact draft.
        fixture.app.apply_edit("alpha\nbeta".to_string(), 2);
        fixture.app.on_key(Key::Down);
        assert_eq!(
            fixture.app.text, "alpha\nbeta",
            "browsing Down mid-entry: stays inside"
        );
        assert_eq!(fixture.app.cursor, 8, "second visual line, column kept");
        fixture.app.on_key(Key::Down);
        assert_eq!(
            fixture.app.text, multiline,
            "last line steps newer: draft restored"
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
            parent_id: None,
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

/// Focused tests for the stepped clarification survey: the renderer wiring,
/// digit-key answers, and the "Chat about this" protocol end to end.
#[cfg(test)]
mod survey_tests {
    use super::*;
    use crate::core::types::{HarnessSurveyOption, HarnessSurveyQuestion};

    /// A "select all that apply" question: same shape, `multiple: true`.
    fn multiple_question(
        header: &str,
        prompt: &str,
        options: &[(&str, &str)],
        allow_other: bool,
    ) -> HarnessSurveyQuestion {
        let mut question = question(header, prompt, options, allow_other);
        question.multiple = true;
        question
    }

    fn multiple_survey() -> QuestionSurvey {
        QuestionSurvey {
            answers_cursor: None,
            questions: vec![multiple_question(
                "Scope",
                "Which parts should change?",
                &[
                    ("Docs", "update the readme"),
                    ("Picker", "the survey overlay"),
                    ("Tests", "new coverage"),
                ],
                true,
            )],
        }
    }

    fn question(
        header: &str,
        prompt: &str,
        options: &[(&str, &str)],
        allow_other: bool,
    ) -> HarnessSurveyQuestion {
        HarnessSurveyQuestion {
            header: header.to_string(),
            question: prompt.to_string(),
            options: options
                .iter()
                .map(|(label, description)| HarnessSurveyOption {
                    label: label.to_string(),
                    description: description.to_string(),
                })
                .collect(),
            allow_other,
            multiple: false,
        }
    }

    fn survey() -> QuestionSurvey {
        QuestionSurvey {
            answers_cursor: None,
            questions: vec![
                question(
                    "Approach",
                    "Poll or channel?",
                    &[("Poll", "watch the file"), ("Channel", "read the pipe")],
                    true,
                ),
                question(
                    "Scope",
                    "Include tests?",
                    &[("Yes", "with tests"), ("No", "without")],
                    false,
                ),
            ],
        }
    }

    fn plain(rows: &[String]) -> Vec<String> {
        rows.iter()
            .map(|row| crate::watch::ansi::strip_ansi(row))
            .collect()
    }

    fn type_text(app: &mut TuiApp, text: &str) {
        app.apply_edit(text.to_string(), text.chars().count());
    }

    #[test]
    fn question_overlays_render_through_the_survey_layout() {
        let mut fixture = super::prompt_history_wiring_tests::make_history_app(&[]);
        fixture.app.begin_survey(survey());
        let rows = plain(&fixture.app.live_region());
        assert!(rows
            .iter()
            .any(|row| row.contains("Approach") && row.contains("question 1 of 2")));
        assert!(rows.iter().any(|row| row.contains("Poll or channel?")));
        assert!(rows.iter().any(|row| row.contains("❯ 1. Poll")));
        assert!(rows.iter().any(|row| row.contains("   watch the file")));
        assert!(rows.iter().any(|row| row.contains("3. Type something.")));
        assert!(rows.iter().any(|row| row.contains("4. Chat about this")));
        assert!(rows
            .iter()
            .any(|row| row
                .contains("Enter to select · ↑/↓ to navigate · 1-9 to jump · Esc to cancel")));
    }

    #[test]
    fn survey_waiting_flips_the_pane_title_between_question_mark_and_spinner() {
        let mut fixture = super::prompt_history_wiring_tests::make_history_app(&[]);
        // A live run with a spinning title, then the ask_user survey arrives.
        fixture.app.pane_title = Some(PaneTitle::new("add authentication"));
        assert!(fixture
            .app
            .pane_title
            .as_mut()
            .expect("pane title")
            .set_busy(true, Instant::now())
            .is_some());
        fixture.app.begin_survey(survey());
        assert!(
            fixture
                .app
                .pane_title
                .as_ref()
                .expect("pane title")
                .is_waiting(),
            "a pending survey shows the waiting marker instead of the spinner"
        );
        // Esc dismisses the survey (nothing written): the spinner returns.
        fixture.app.on_key(Key::Escape);
        assert!(
            !fixture
                .app
                .pane_title
                .as_ref()
                .expect("pane title")
                .is_waiting(),
            "dismissing the survey resumes the spinner"
        );
    }

    #[test]
    fn a_digit_key_confirms_that_option_immediately() {
        let mut fixture = super::prompt_history_wiring_tests::make_history_app(&[]);
        fixture.app.begin_survey(survey());
        fixture.app.on_key(Key::Text("2".to_string()));
        let state = fixture.app.survey.as_ref().expect("survey still open");
        assert_eq!(state.current, 1, "advanced to the next question");
        assert_eq!(state.answers.len(), 1);
        assert_eq!(state.answers[0].choice.as_deref(), Some("Channel"));
        // Question 2 has no allow_other, so its chat row is the last item.
        let rows = plain(&fixture.app.live_region());
        assert!(rows
            .iter()
            .any(|row| row.contains("Scope") && row.contains("question 2 of 2")));
        assert!(rows.iter().any(|row| row.contains("3. Chat about this")));
    }

    #[test]
    fn choosing_chat_then_submitting_writes_a_chat_record_and_clears_the_survey() {
        let mut fixture = super::prompt_history_wiring_tests::make_history_app(&[]);
        fixture.app.begin_survey(survey());
        // The chat row is last: 2 listed options + "Type something." + chat.
        fixture.app.on_key(Key::Text("4".to_string()));
        assert!(fixture.app.overlay.is_none(), "the overlay closes");
        assert!(fixture
            .app
            .survey
            .as_ref()
            .is_some_and(|state| state.chat_mode));
        assert!(fixture.app.cells.iter().any(|entry| matches!(
            entry,
            TranscriptEntry::Info(note)
                if note.text.contains("answers all 2 questions")
                    && note.text.contains("Poll or channel?")
                    && note.text.contains("Include tests?")
                    && note.text.contains("watch the file")
        )));

        type_text(&mut fixture.app, "use the channel, no tests");
        fixture.app.submit();
        assert!(fixture.app.survey.is_none(), "the survey clears");
        assert!(fixture.app.cells.iter().any(|entry| matches!(
            entry,
            TranscriptEntry::Info(note) if note.text == "chat reply recorded — the run continues"
        )));
        let path =
            std::path::Path::new(&fixture.app.paths.state_path).with_file_name("answers.jsonl");
        let written = std::fs::read_to_string(&path).expect("answers.jsonl written");
        assert!(
            written.contains("\"chat\":\"use the channel, no tests\""),
            "{written}"
        );
        assert!(written.contains("\"answers\":[]"), "{written}");
    }

    #[test]
    fn escape_in_chat_mode_dismisses_the_survey_without_writing() {
        let mut fixture = super::prompt_history_wiring_tests::make_history_app(&[]);
        fixture.app.begin_survey(survey());
        fixture.app.on_key(Key::Text("4".to_string()));
        fixture.app.on_key(Key::Escape);
        assert!(fixture.app.survey.is_none());
        let path =
            std::path::Path::new(&fixture.app.paths.state_path).with_file_name("answers.jsonl");
        assert!(!path.exists(), "a dismissal writes nothing");
    }

    #[test]
    fn a_failed_chat_write_keeps_chat_mode_and_restores_the_reply() {
        let mut fixture = super::prompt_history_wiring_tests::make_history_app(&[]);
        fixture.app.begin_survey(survey());
        fixture.app.on_key(Key::Text("1".to_string()));
        fixture.app.on_key(Key::Text("3".to_string()));
        assert!(fixture
            .app
            .survey
            .as_ref()
            .is_some_and(|state| state.chat_mode));
        // The answers path now names a directory, so every append fails.
        let blocked =
            std::path::Path::new(&fixture.app.paths.state_path).with_file_name("answers.jsonl");
        let _ = std::fs::remove_file(&blocked);
        std::fs::create_dir_all(&blocked).expect("block the answers path");
        type_text(&mut fixture.app, "still thinking");
        fixture.app.submit();
        let state = fixture
            .app
            .survey
            .as_ref()
            .expect("survey survives the failure");
        assert!(state.chat_mode, "chat mode stays on for the retry");
        assert_eq!(
            fixture.app.text, "still thinking",
            "the reply is back in the composer"
        );
        assert!(fixture.app.cells.iter().any(|entry| matches!(
            entry,
            TranscriptEntry::Error(note) if note.text.contains("press Enter to retry")
        )));
        // Unblock and retry with Enter: the same text lands as the chat record.
        std::fs::remove_dir_all(&blocked).expect("unblock the answers path");
        fixture.app.on_key(Key::Return);
        assert!(fixture.app.survey.is_none(), "the retry clears the survey");
        let written = std::fs::read_to_string(&blocked).expect("answers.jsonl written");
        assert!(written.contains("\"chat\":\"still thinking\""), "{written}");
    }

    /// The real keystroke path: characters typed through on_key reach the
    /// composer while chat mode is on, and Enter submits them as the reply.
    #[test]
    fn chat_mode_typing_through_on_key_reaches_the_composer_and_enter_submits() {
        let mut fixture = super::prompt_history_wiring_tests::make_history_app(&[]);
        fixture.app.begin_survey(survey());
        fixture.app.on_key(Key::Text("4".to_string()));
        for ch in ["u", "s", "e", " ", "p", "o", "l", "l"] {
            fixture.app.on_key(Key::Text(ch.to_string()));
        }
        assert_eq!(
            fixture.app.text, "use poll",
            "typed text reaches the composer in chat mode"
        );
        fixture.app.on_key(Key::Backspace);
        assert_eq!(fixture.app.text, "use pol");
        fixture.app.on_key(Key::Return);
        assert!(
            fixture.app.survey.is_none(),
            "Enter records the reply and clears the survey"
        );
        let path =
            std::path::Path::new(&fixture.app.paths.state_path).with_file_name("answers.jsonl");
        let written = std::fs::read_to_string(&path).expect("answers.jsonl written");
        assert!(written.contains("\"chat\":\"use pol\""), "{written}");
    }

    #[test]
    fn a_multiple_question_renders_marks_a_confirm_row_and_the_toggle_footer() {
        let mut fixture = super::prompt_history_wiring_tests::make_history_app(&[]);
        fixture.app.begin_survey(multiple_survey());
        let rows = plain(&fixture.app.live_region());
        assert!(
            rows.iter().any(|row| row.contains("[ ] 1. Docs")),
            "{rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("❯ [ ] 1. Docs")),
            "{rows:?}"
        );
        // Three options + confirm + "Type something." + chat.
        assert!(
            rows.iter().any(|row| row.contains("4. Confirm selection")),
            "{rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("5. Type something.")),
            "{rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("6. Chat about this")),
            "{rows:?}"
        );
        assert!(
            rows.iter()
                .any(|row| row.contains("Space toggles · Enter confirms the selection")),
            "{rows:?}"
        );
    }

    #[test]
    fn space_marks_options_and_the_confirm_row_records_them_joined() {
        let mut fixture = super::prompt_history_wiring_tests::make_history_app(&[]);
        let survey = multiple_survey();
        fixture.app.begin_survey(survey.clone());
        // Space on the highlighted row marks it, then Down+Space marks a second.
        fixture.app.on_key(Key::Text(" ".to_string()));
        fixture.app.on_key(Key::Down);
        fixture.app.on_key(Key::Down);
        fixture.app.on_key(Key::Text(" ".to_string()));
        let rows = plain(&fixture.app.live_region());
        assert!(
            rows.iter().any(|row| row.contains("[x] 1. Docs")),
            "{rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("[ ] 2. Picker")),
            "{rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("[x] 3. Tests")),
            "{rows:?}"
        );
        // Down to the confirm row (index 3) and Enter: one joined choice.
        fixture.app.on_key(Key::Down);
        fixture.app.on_key(Key::Return);
        assert!(
            fixture.app.survey.is_none(),
            "the last question finishes the survey"
        );
        let path =
            std::path::Path::new(&fixture.app.paths.state_path).with_file_name("answers.jsonl");
        let written = std::fs::read_to_string(&path).expect("answers.jsonl written");
        assert!(written.contains("\"choice\":\"Docs, Tests\""), "{written}");
        // The harness end validates that exact shape against the survey.
        let record: HarnessSurveyAnswers =
            serde_json::from_str(written.lines().next().expect("one line")).expect("valid record");
        assert!(
            crate::harness::harness_tools::validate_survey_answers(&survey, &record).is_ok(),
            "a joined multi-select choice validates: {record:?}"
        );
    }

    #[test]
    fn confirming_with_nothing_marked_keeps_the_survey_open() {
        let mut fixture = super::prompt_history_wiring_tests::make_history_app(&[]);
        fixture.app.begin_survey(multiple_survey());
        fixture.app.on_key(Key::Down);
        fixture.app.on_key(Key::Down);
        fixture.app.on_key(Key::Down);
        fixture.app.on_key(Key::Return);
        let state = fixture.app.survey.as_ref().expect("survey stays open");
        assert!(state.answers.is_empty(), "nothing is recorded");
        assert!(fixture.app.cells.iter().any(|entry| matches!(
            entry,
            TranscriptEntry::Info(note)
                if note.text == "mark at least one option with space before confirming"
        )));
        let path =
            std::path::Path::new(&fixture.app.paths.state_path).with_file_name("answers.jsonl");
        assert!(!path.exists(), "nothing is written");
    }

    #[test]
    fn space_is_a_no_op_on_a_single_choice_question() {
        let mut fixture = super::prompt_history_wiring_tests::make_history_app(&[]);
        fixture.app.begin_survey(survey());
        fixture.app.on_key(Key::Text(" ".to_string()));
        let state = fixture.app.survey.as_ref().expect("survey still open");
        assert_eq!(
            state.current, 0,
            "space never advances a single-choice survey"
        );
        assert!(state.answers.is_empty());
    }
}

/// Focused tests for the activity line above the composer: it is painted only
/// while a run is in flight, it sits immediately above the composer, and its
/// clock counts up from the run's start with the shared spinner cadence.
#[cfg(test)]
mod activity_line_tests {
    use super::*;
    use crate::watch::ansi::strip_ansi;

    fn plain(rows: &[String]) -> Vec<String> {
        rows.iter().map(|row| strip_ansi(row)).collect()
    }

    fn elapsed(secs: u64) -> Instant {
        Instant::now()
            .checked_sub(Duration::from_secs(secs))
            .expect("the monotonic clock is older than the run")
    }

    #[test]
    fn working_line_sits_immediately_above_the_composer_while_running() {
        let mut fixture = super::prompt_history_wiring_tests::make_history_app(&[]);
        fixture.app.running = true;
        fixture.app.run_started_at = Some(elapsed(65));
        let rows = plain(&fixture.app.live_region());
        let index = rows
            .iter()
            .position(|row| row.contains("working for 1m 05s"))
            .expect("a live run must paint the activity line");
        assert!(
            crate::tui::pane_title::SPINNER_FRAMES
                .iter()
                .any(|frame| rows[index].starts_with(frame)),
            "the line starts with a spinner frame: {:?}",
            rows[index]
        );
        assert!(
            rows[index + 1].starts_with('─') && rows[index + 2].contains('❯'),
            "the activity line belongs directly above the composer box: {:?}",
            &rows[index..=index + 2]
        );
    }

    #[test]
    fn no_working_line_is_painted_when_idle() {
        let mut fixture = super::prompt_history_wiring_tests::make_history_app(&[]);
        // A stale start time with no run in flight must paint nothing.
        fixture.app.run_started_at = Some(elapsed(3));
        let rows = plain(&fixture.app.live_region());
        assert!(
            !rows.iter().any(|row| row.contains("working for")),
            "{rows:?}"
        );
    }

    #[test]
    fn finish_run_clears_the_activity_line_state() {
        let mut fixture = super::prompt_history_wiring_tests::make_history_app(&[]);
        fixture.app.running = true;
        fixture.app.run_started_at = Some(Instant::now());
        fixture.app.activity_next_tick = Some(Instant::now());
        fixture.app.finish_run();
        assert!(fixture.app.run_started_at.is_none());
        assert!(fixture.app.activity_next_tick.is_none());
        let rows = plain(&fixture.app.live_region());
        assert!(!rows.iter().any(|row| row.contains("working for")));
    }

    #[test]
    fn activity_tick_stops_rearming_once_the_run_ends() {
        let mut fixture = super::prompt_history_wiring_tests::make_history_app(&[]);
        fixture.app.running = true;
        fixture.app.activity_next_tick = Some(Instant::now());
        fixture.app.finish_run();
        assert!(
            fixture.app.activity_next_tick.is_none(),
            "an idle composer must not keep repainting for the spinner"
        );
    }

    #[test]
    fn escape_stops_the_run_and_its_in_flight_commands() {
        // `terminate_active_processes` sweeps EVERY in-flight child in the
        // process, so hold the registry lock the other registry-firing tests
        // hold before firing it from the esc key.
        let _guard = crate::tools::child_process::REGISTRY_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut fixture = super::prompt_history_wiring_tests::make_history_app(&[]);
        fixture.app.running = true;
        let signal = crate::harness::model_call::AbortSignal::new();
        fixture.app.abort = Some(signal.clone());
        // Stands in for the terminator a running BASH child registers: esc must
        // run it, not merely flip the harness abort flag.
        let stopped = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stopped);
        let unregister =
            crate::tools::child_process::register_process_terminator(Box::new(move || {
                flag.store(true, Ordering::SeqCst);
            }));

        fixture.app.on_key(Key::Escape);

        assert!(signal.is_aborted(), "esc must abort the run");
        assert!(
            stopped.load(Ordering::SeqCst),
            "esc must stop the in-flight command, not only signal the harness"
        );
        assert!(
            fixture.app.running,
            "the run ends when the harness reports back, not on the keypress"
        );
        // The stop is visible: an operator who hits esc must see that the
        // running command was killed rather than infer it from silence.
        let note = fixture
            .app
            .pending_cells
            .iter()
            .chain(fixture.app.cells.iter())
            .find_map(|entry| match entry {
                TranscriptEntry::Info(note) if note.text.contains("in-flight command") => {
                    Some(note.text.clone())
                }
                _ => None,
            })
            .expect("the killed command is reported in the transcript");
        assert!(note.contains("aborting the run"), "unexpected note: {note}");
        unregister();
    }
}

/// Durable vs transient TUI rows: tool activity, cycle transitions, ops and
/// warnings blink on the activity line above the composer and never settle
/// into scrollback, while the transcript keeps every one of them for `dripw`.
#[cfg(test)]
mod compact_ephemeral_tests {
    use super::*;
    use crate::watch::ansi::strip_ansi;

    fn event(
        kind: HarnessEventType,
        iteration: i64,
        detail: &str,
        tool: Option<&str>,
    ) -> TranscriptEntry {
        TranscriptEntry::Event(TranscriptEventEntry {
            at: "2026-01-01T00:00:00Z".to_string(),
            data: tool.map(|name| crate::core::types::HarnessEventData {
                tool_name: Some(name.to_string()),
                ..Default::default()
            }),
            detail: detail.to_string(),
            goal_id: "g".to_string(),
            iteration,
            kind,
        })
    }

    /// The exact exchange the goal names: a cycle transition, a tool call, a
    /// warning and the model's answer.
    fn one_cycle() -> Vec<TranscriptEntry> {
        vec![
            TranscriptEntry::Goal(crate::cli::transcript::TranscriptGoalEntry {
                at: "2026-01-01T00:00:00Z".to_string(),
                goal_id: "g".to_string(),
                images: Vec::new(),
                mentions: Vec::new(),
                text: "fix the flaky test".to_string(),
            }),
            event(
                HarnessEventType::IterationStart,
                1,
                "cycle 1/2 — task-1: think",
                None,
            ),
            event(
                HarnessEventType::ToolCall,
                1,
                "READ {\"path\":\"x\"}",
                Some("READ"),
            ),
            event(
                HarnessEventType::RunWarning,
                1,
                "rate limited, waiting 2s",
                None,
            ),
            event(HarnessEventType::ModelText, 1, "All done.", None),
        ]
    }

    fn feed(app: &mut TuiApp) -> Vec<CompactCell> {
        let cells = app.compact.absorb(&one_cycle());
        app.remember_activity_notices(Instant::now(), &cells);
        cells
    }

    fn plain(rows: &[String]) -> Vec<String> {
        rows.iter().map(|row| strip_ansi(row)).collect()
    }

    #[test]
    fn tool_ops_and_warnings_never_settle_into_scrollback() {
        let mut fixture = super::prompt_history_wiring_tests::make_history_app(&[]);
        fixture.app.running = true;
        let cells = feed(&mut fixture.app);
        let rows = plain(&fixture.app.projected_rows(&cells));

        assert!(
            rows.iter().any(|row| row.contains("fix the flaky test")),
            "{rows:?}"
        );
        assert!(rows.iter().any(|row| row.contains("All done.")), "{rows:?}");
        for noise in ["cycle 1/2", "Tool called", "Tools called", "rate limited"] {
            assert!(
                !rows.iter().any(|row| row.contains(noise)),
                "{noise} leaked into scrollback: {rows:?}"
            );
        }
    }

    #[test]
    fn transient_rows_blink_above_the_composer_then_vanish_with_the_run() {
        let mut fixture = super::prompt_history_wiring_tests::make_history_app(&[]);
        fixture.app.running = true;
        fixture.app.run_started_at = Some(Instant::now());
        feed(&mut fixture.app);

        let live = plain(&fixture.app.live_region());
        let spinner = live
            .iter()
            .position(|row| row.contains("working for"))
            .expect("the activity line is painted while running");
        assert!(
            live.iter().any(|row| row.contains("Tool called")),
            "the folded tool summary blinks on the activity line: {live:?}"
        );
        assert!(
            live[..spinner]
                .iter()
                .any(|row| row.contains("rate limited")),
            "the newest op/warning row sits above the working line: {live:?}"
        );
        assert!(
            live[spinner + 1].starts_with('─') && live[spinner + 2].contains('❯'),
            "the transient block stays directly above the composer: {:?}",
            &live[spinner..=spinner + 2]
        );

        fixture.app.finish_run();
        let after = plain(&fixture.app.live_region());
        for noise in ["Tool called", "Tools called", "rate limited", "cycle 1/2"] {
            assert!(
                !after.iter().any(|row| row.contains(noise)),
                "{noise} outlived the run: {after:?}"
            );
        }
    }

    #[test]
    fn resize_tail_budgets_only_durable_rows() {
        let mut fixture = super::prompt_history_wiring_tests::make_history_app(&[]);
        let cells = fixture.app.compact.absorb(&one_cycle());
        let durable = scrollback_cells(&cells);
        assert_eq!(durable.len(), 2, "goal + model text only: {durable:?}");
        assert!(durable.iter().all(is_scrollback_cell));
        // The dropped rows are the ones the activity line shows: the folded
        // tool summary, the cycle transition and the warning.
        assert_eq!(cells.len() - durable.len(), 3, "{cells:?}");
    }

    /// Transient cells of a batch, exactly as `emit_static` would keep them.
    fn transient_of(emitter: &mut CompactEmitter, entries: &[TranscriptEntry]) -> Vec<CompactCell> {
        emitter
            .absorb(entries)
            .into_iter()
            .filter(|cell| !is_scrollback_cell(cell))
            .collect()
    }

    fn warning_batch(emitter: &mut CompactEmitter, detail: &str) -> Vec<CompactCell> {
        let entries = vec![event(HarnessEventType::RunWarning, 1, detail, None)];
        let cells = transient_of(emitter, &entries);
        assert!(!cells.is_empty(), "{detail} must project one transient row");
        cells
    }

    fn notice_detail(cell: &CompactCell) -> String {
        match cell {
            CompactCell::Passthrough(TranscriptEntry::Event(event)) => event.detail.clone(),
            other => format!("{other:?}"),
        }
    }

    #[test]
    fn fast_transient_batches_coalesce_behind_the_debounce_window() {
        let mut fixture = super::prompt_history_wiring_tests::make_history_app(&[]);
        let app = &mut fixture.app;
        app.running = true;
        let t0 = Instant::now();

        let first = warning_batch(&mut app.compact, "first");
        app.remember_activity_notices(t0, &first);
        assert_eq!(
            app.activity_shown_at,
            Some(t0),
            "the first batch of a run lands immediately"
        );
        assert_eq!(notice_detail(&app.activity_notices[0]), "first");

        // 100 ms later -- well inside the 500 ms window -- the new row may not
        // replace what is on screen yet.
        let soon = t0 + Duration::from_millis(100);
        let second = warning_batch(&mut app.compact, "second");
        app.remember_activity_notices(soon, &second);
        assert_eq!(
            notice_detail(&app.activity_notices[0]),
            "first",
            "a fast batch must not blink over the displayed one"
        );
        assert!(
            !app.settle_activity_notices(soon),
            "the window is still open"
        );

        // Deadline reached: the coalesced batch swaps in exactly once.
        assert!(app.settle_activity_notices(t0 + Duration::from_millis(500)));
        assert_eq!(notice_detail(&app.activity_notices[0]), "second");
        assert!(app.activity_pending.is_empty());

        // The deadline is anchored to the swap, not pushed back by arrivals:
        // a continuous stream still updates once per window.
        let third = warning_batch(&mut app.compact, "third");
        app.remember_activity_notices(t0 + Duration::from_millis(600), &third);
        assert_eq!(notice_detail(&app.activity_notices[0]), "second");
        assert!(app.settle_activity_notices(t0 + Duration::from_millis(1000)));
        assert_eq!(notice_detail(&app.activity_notices[0]), "third");
    }

    #[test]
    fn the_debounce_state_dies_with_the_run() {
        let mut fixture = super::prompt_history_wiring_tests::make_history_app(&[]);
        let app = &mut fixture.app;
        app.running = true;
        let t0 = Instant::now();
        let first = warning_batch(&mut app.compact, "first");
        app.remember_activity_notices(t0, &first);
        let queued = warning_batch(&mut app.compact, "queued");
        app.remember_activity_notices(t0 + Duration::from_millis(50), &queued);
        assert!(!app.activity_pending.is_empty(), "the batch is queued");
        app.finish_run();
        assert!(app.activity_notices.is_empty(), "no row outlives the run");
        assert!(
            app.activity_pending.is_empty(),
            "no queued row leaks into the next run"
        );
        assert!(
            app.activity_shown_at.is_none(),
            "the next run opens a fresh window"
        );
    }

    #[test]
    fn a_blank_row_gaps_the_status_lines_from_the_working_line() {
        let mut fixture = super::prompt_history_wiring_tests::make_history_app(&[]);
        fixture.app.running = true;
        fixture.app.run_started_at = Some(Instant::now());
        feed(&mut fixture.app);
        let live = plain(&fixture.app.live_region());
        let spinner = live
            .iter()
            .position(|row| row.contains("working for"))
            .expect("the activity line is painted while running");
        assert!(spinner >= 2, "{live:?}");
        assert_eq!(
            live[spinner - 1],
            "",
            "one blank row separates the block from the working line: {live:?}"
        );
        assert!(
            live[..spinner - 1]
                .iter()
                .any(|row| row.contains("Tool called")),
            "the status lines stay above the gap: {live:?}"
        );
    }
}

#[cfg(test)]
mod background_jobs_tests {
    use super::*;
    use crate::tools::types::{
        ChatAsyncToolCommandRequest, ChatAsyncToolJob, ChatAsyncToolJobStatus,
        ChatAsyncToolRuntime, ChatAsyncToolTailResult, ChatAsyncToolTaskRequest,
        ChatAsyncToolWaitResult, ChatTmuxSessionRuntime, ChatToolRuntimeServices,
    };
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};

    /// A runtime whose running set the test drives directly — the app under
    /// test reads it exactly like it reads the real manager.
    struct FakeJobs {
        running: Mutex<Vec<ChatAsyncToolJob>>,
        reported: Mutex<Vec<String>>,
    }

    impl FakeJobs {
        fn new(jobs: Vec<ChatAsyncToolJob>) -> Arc<Self> {
            Arc::new(Self {
                running: Mutex::new(jobs),
                reported: Mutex::new(Vec::new()),
            })
        }

        fn set(&self, jobs: Vec<ChatAsyncToolJob>) {
            *self.running.lock().unwrap() = jobs;
        }
    }

    impl ChatAsyncToolRuntime for FakeJobs {
        fn get_job(&self, job_id: &str) -> Option<ChatAsyncToolJob> {
            self.running
                .lock()
                .unwrap()
                .iter()
                .find(|job| job.id == job_id)
                .cloned()
        }

        fn start_command(
            &self,
            _request: ChatAsyncToolCommandRequest,
        ) -> anyhow::Result<ChatAsyncToolJob> {
            anyhow::bail!("the fake runtime starts no jobs")
        }

        fn start_task(
            &self,
            _request: ChatAsyncToolTaskRequest,
        ) -> anyhow::Result<ChatAsyncToolJob> {
            anyhow::bail!("the fake runtime starts no jobs")
        }

        fn tail_job(
            &self,
            job_id: &str,
            _lines: Option<i64>,
        ) -> anyhow::Result<ChatAsyncToolTailResult> {
            let job = self
                .get_job(job_id)
                .ok_or_else(|| anyhow::anyhow!("no job {job_id}"))?;
            Ok(ChatAsyncToolTailResult {
                job,
                lines: 60,
                output: "the build is still going".to_string(),
            })
        }

        fn wait_for_job(
            &self,
            _job_id: &str,
            _timeout_ms: Option<i64>,
        ) -> anyhow::Result<ChatAsyncToolWaitResult> {
            anyhow::bail!("the fake runtime never waits")
        }

        /// Mirrors `AsyncToolJobManager::take_settled_unreported`: a settled
        /// job nobody has read yet comes back once, then counts as reported.
        fn take_settled_unreported(&self) -> Vec<ChatAsyncToolJob> {
            let reported = self.reported.lock().unwrap().clone();
            let mut jobs = self.running.lock().unwrap();
            let mut settled = Vec::new();
            for job in jobs.iter_mut() {
                if job.status != ChatAsyncToolJobStatus::Running && !reported.contains(&job.id) {
                    self.reported.lock().unwrap().push(job.id.clone());
                    settled.push(job.clone());
                }
            }
            settled
        }

        /// Mirrors `AsyncToolJobManager::running_jobs`: settled jobs are
        /// never part of the running snapshot.
        fn running_jobs(&self) -> Vec<ChatAsyncToolJob> {
            self.running
                .lock()
                .unwrap()
                .iter()
                .filter(|job| job.status == ChatAsyncToolJobStatus::Running)
                .cloned()
                .collect()
        }
    }

    struct FakeTmux;

    impl ChatTmuxSessionRuntime for FakeTmux {
        fn get_session(&self, _session_name: &str) -> Option<crate::tools::types::ChatTmuxSession> {
            None
        }

        fn list_sessions(&self) -> Vec<crate::tools::types::ChatTmuxSession> {
            Vec::new()
        }

        fn register_session(&self, _session: crate::tools::types::ChatTmuxSession) {}
    }

    fn job(id: &str, tool_name: &str, title: &str) -> ChatAsyncToolJob {
        ChatAsyncToolJob {
            command: None,
            cwd: "/tmp".to_string(),
            error: None,
            exit_code: None,
            finished_at: None,
            id: id.to_string(),
            log_path: "/tmp/job.log".to_string(),
            started_at: "2026-01-01T00:00:00Z".to_string(),
            status: ChatAsyncToolJobStatus::Running,
            title: title.to_string(),
            tool_name: tool_name.to_string(),
        }
    }

    struct JobsFixture {
        app: TuiApp,
        jobs: Arc<FakeJobs>,
        _cwd: tempfile::TempDir,
        _home: tempfile::TempDir,
        _project: tempfile::TempDir,
        _rx: mpsc::Receiver<Msg>,
        _mention_rx: mpsc::Receiver<(u64, String)>,
    }

    fn make_app(jobs: Vec<ChatAsyncToolJob>) -> JobsFixture {
        let cwd = tempfile::tempdir().expect("cwd tempdir");
        let home = tempfile::tempdir().expect("home tempdir");
        let project = tempfile::tempdir().expect("project tempdir");
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
            id: "sess-jobs-test".to_string(),
            last_goal: None,
            parent_id: None,
            project_slug: "jobs-test".to_string(),
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
            task_loop_limit: None,
            review_waiver_lines: None,
            plan_mode: None,
            no_repo_memory: true,
            project: drip_project,
            roles_flag: None,
            session,
            status_line: None,
            classifier: None,
            no_classifier: false,
        };
        let (tx, rx) = mpsc::channel::<Msg>();
        let (mention_tx, mention_rx) = mpsc::channel::<(u64, String)>();
        let mut app = TuiApp::new(bootstrap, tx, mention_tx);
        let jobs = FakeJobs::new(jobs);
        app.tool_services = ChatToolRuntimeServices {
            async_jobs: jobs.clone(),
            tmux_sessions: Arc::new(FakeTmux),
        };
        JobsFixture {
            app,
            jobs,
            _cwd: cwd,
            _home: home,
            _project: project,
            _rx: rx,
            _mention_rx: mention_rx,
        }
    }

    #[test]
    fn a_settled_monitor_is_handed_to_an_idle_session_as_its_next_message() {
        let mut fixture = make_app(vec![]);
        let mut settled = job("monitor-1", "MONITOR", "monitor: watch the build");
        settled.status = ChatAsyncToolJobStatus::Completed;
        settled.exit_code = Some(Some(0));
        settled.finished_at = Some("2026-01-01T00:00:05Z".to_string());
        fixture.jobs.set(vec![settled]);

        let report = fixture
            .app
            .take_idle_background_reports()
            .expect("an idle session takes the settled report");
        assert!(
            report.contains("[background monitor] monitor-1"),
            "{report}"
        );
        assert!(report.contains("the signal fired"), "{report}");
        assert!(report.contains("/tmp/job.log"), "{report}");

        // Claimed exactly once: the next poll has nothing left to hand over.
        assert!(fixture.app.take_idle_background_reports().is_none());
    }

    #[test]
    fn a_live_run_keeps_its_own_background_reports() {
        let mut fixture = make_app(vec![]);
        let mut settled = job("monitor-1", "MONITOR", "monitor: watch the build");
        settled.status = ChatAsyncToolJobStatus::Failed;
        settled.error = Some("monitor timed out after 1000ms".to_string());
        fixture.jobs.set(vec![settled]);
        fixture.app.running = true;

        assert!(fixture.app.take_idle_background_reports().is_none());
        // Still unclaimed: the running harness drains it at its next round.
        assert_eq!(fixture.jobs.take_settled_unreported().len(), 1);
    }

    fn strip(rows: &[String]) -> Vec<String> {
        let pattern = regex::Regex::new("\u{1b}\\[[0-9;]*m").expect("ansi regex");
        rows.iter()
            .map(|row| pattern.replace_all(row, "").into_owned())
            .collect()
    }

    /// The status row the app actually paints, read off the live region.
    fn status_row(app: &TuiApp) -> String {
        strip(&app.live_region())
            .into_iter()
            .rev()
            .find(|row| row.contains("session "))
            .expect("the status bar is painted")
    }

    #[test]
    fn the_status_bar_counts_live_monitors_and_shells() {
        let mut fixture = make_app(vec![
            job("monitor-1", "MONITOR", "monitor: watch the build"),
            job("shell-1", "BASH_ASYNC", "cargo test --lib"),
            job("shell-2", "BASH_ASYNC", "bun test"),
        ]);
        assert!(fixture.app.refresh_jobs(Instant::now()));
        assert_eq!(fixture.app.job_counts, (1, 2));
        let row = status_row(&fixture.app);
        assert!(row.contains("1 monitor · 2 shells"), "{row}");
    }

    #[test]
    fn the_counter_ignores_settled_jobs_and_clears_when_nothing_runs() {
        let mut fixture = make_app(vec![job(
            "monitor-1",
            "MONITOR",
            "monitor: watch the build",
        )]);
        fixture.app.refresh_jobs(Instant::now());
        assert!(status_row(&fixture.app).contains("1 monitor"));

        // A completed and a failed job both leave the running set: only live
        // work is counted.
        let mut completed = job("shell-done", "BASH_ASYNC", "cargo test");
        completed.status = ChatAsyncToolJobStatus::Completed;
        let mut failed = job("shell-failed", "BASH_ASYNC", "bun test");
        failed.status = ChatAsyncToolJobStatus::Failed;
        fixture.jobs.set(vec![completed, failed]);
        assert!(fixture.app.refresh_jobs(Instant::now()));
        assert_eq!(fixture.app.job_counts, (0, 0));
        let row = status_row(&fixture.app);
        assert!(!row.contains("monitor"), "{row}");
        assert!(!row.contains("shell"), "{row}");
    }

    #[test]
    fn ctrl_b_opens_the_browser_over_the_running_jobs() {
        let mut fixture = make_app(vec![
            job("monitor-1", "MONITOR", "monitor: watch the build"),
            job("shell-1", "BASH_ASYNC", "cargo test --lib"),
        ]);
        fixture.app.on_key(Key::Ctrl('b'));
        let overlay = fixture.app.overlay.as_ref().expect("the browser opened");
        assert_eq!(overlay.kind, OverlayKind::Jobs);
        assert_eq!(overlay.items.len(), 2);
        assert_eq!(overlay.items[0].id, "0");
        assert!(overlay.items[0]
            .detail
            .as_deref()
            .unwrap_or("")
            .contains("monitor"));
        let rows = strip(&fixture.app.live_region()).join("\n");
        assert!(
            rows.contains("Background jobs · 1 monitor · 1 shell"),
            "{rows}"
        );
        assert!(rows.contains("monitor: watch the build"), "{rows}");

        // Esc closes it without writing anything to the transcript.
        let before = fixture.app.cells.len();
        fixture.app.on_key(Key::Escape);
        assert!(fixture.app.overlay.is_none());
        assert_eq!(fixture.app.cells.len(), before);
    }

    #[test]
    fn enter_opens_the_detail_frame_and_esc_steps_back_out() {
        let mut fixture = make_app(vec![job("shell-1", "BASH_ASYNC", "cargo test --lib")]);
        fixture.app.dispatch_command("jobs", "");
        fixture.app.on_key(Key::Return);
        assert_eq!(fixture.app.job_detail, Some(0));
        let rows = strip(&fixture.app.live_region()).join("\n");
        assert!(rows.contains("Shell details"), "{rows}");
        assert!(rows.contains("Status: running"), "{rows}");
        assert!(rows.contains("cargo test --lib"), "{rows}");
        assert!(rows.contains("the build is still going"), "{rows}");

        // Esc drops the detail frame first, the browser second.
        fixture.app.on_key(Key::Escape);
        assert_eq!(fixture.app.job_detail, None);
        assert!(fixture.app.overlay.is_some());
        fixture.app.on_key(Key::Escape);
        assert!(fixture.app.overlay.is_none());
    }

    #[test]
    fn a_job_that_settles_while_open_drops_the_stale_row() {
        let mut fixture = make_app(vec![job("shell-1", "BASH_ASYNC", "cargo test --lib")]);
        fixture.app.dispatch_command("jobs", "");
        fixture.app.on_key(Key::Return);
        assert_eq!(fixture.app.job_detail, Some(0));

        // The job finishes: the next refresh reads an empty running set and
        // the detail frame must not keep showing it as running.
        fixture.jobs.set(Vec::new());
        fixture.app.refresh_jobs(Instant::now());
        assert_eq!(fixture.app.job_detail, None);
        assert_eq!(fixture.app.job_counts, (0, 0));
        let rows = strip(&fixture.app.live_region()).join("\n");
        assert!(rows.contains("nothing running"), "{rows}");
    }

    #[test]
    fn the_jobs_command_jumps_straight_into_a_row() {
        let mut fixture = make_app(vec![
            job("monitor-1", "MONITOR", "monitor: watch the build"),
            job("shell-1", "BASH_ASYNC", "cargo test --lib"),
        ]);
        fixture.app.dispatch_command("jobs", "2");
        assert_eq!(fixture.app.job_detail, Some(1));
        let rows = strip(&fixture.app.live_region()).join("\n");
        assert!(rows.contains("Shell details"), "{rows}");
        assert!(rows.contains("Script: cargo test --lib"), "{rows}");
    }

    #[test]
    fn the_help_text_lists_the_jobs_command_and_its_key() {
        let help = help_text();
        assert!(help.contains("/jobs"), "{help}");
        assert!(help.contains("ctrl+b"), "{help}");
    }

    #[test]
    fn an_sgr_click_report_decodes_into_a_mouse_key() {
        let mut paste = None;
        let keys = decode_input(b"\x1b[<0;20;24M", &mut paste);
        assert_eq!(keys.len(), 1);
        match &keys[0] {
            Key::Mouse(MouseEvent::Click { col, row }) => assert_eq!((*col, *row), (20, 24)),
            _ => panic!("a click report must decode to a mouse key"),
        }
        // A wheel notch rides the same report path but is never a click, and
        // the other buttons stay ignored.
        assert!(matches!(
            decode_input(b"\x1b[<64;20;24M", &mut paste).first(),
            Some(Key::Mouse(MouseEvent::Wheel(_)))
        ));
        assert!(matches!(
            decode_input(b"\x1b[<2;20;24M", &mut paste).first(),
            Some(Key::Ignored)
        ));
        // A click still decodes when a key follows it in the same read.
        let keys = decode_input(b"\x1b[<0;3;4Ma", &mut paste);
        assert!(matches!(
            keys.first(),
            Some(Key::Mouse(MouseEvent::Click { .. }))
        ));
        assert!(matches!(keys.get(1), Some(Key::Text(text)) if text == "a"));
    }

    #[test]
    fn clicking_the_status_line_counter_opens_the_browser() {
        let mut fixture = make_app(vec![
            job("monitor-1", "MONITOR", "monitor: watch the build"),
            job("shell-1", "BASH_ASYNC", "cargo test --lib"),
        ]);
        assert!(fixture.app.refresh_jobs(Instant::now()));
        // The live region sits at the bottom of the screen: a terminal exactly
        // as tall as the frame makes the status row the last painted one.
        let rows = fixture.app.live_region().len();
        fixture.app.rows = rows;
        let status = status_row(&fixture.app);
        let col = status.find("1 monitor").expect("the chip is painted") + 1;
        assert!(fixture.app.background_chip_at(col, rows), "{status}");

        fixture
            .app
            .on_key(Key::Mouse(MouseEvent::Click { col, row: rows }));
        let overlay = fixture.app.overlay.as_ref().expect("the browser opened");
        assert_eq!(overlay.kind, OverlayKind::Jobs);
        assert_eq!(overlay.items.len(), 2);
        // The click opened a frame -- it never typed itself into the composer.
        assert!(fixture.app.text.is_empty());
    }

    #[test]
    fn a_click_off_the_chip_or_with_nothing_running_does_nothing() {
        let mut fixture = make_app(vec![job(
            "monitor-1",
            "MONITOR",
            "monitor: watch the build",
        )]);
        fixture.app.refresh_jobs(Instant::now());
        let rows = fixture.app.live_region().len();
        fixture.app.rows = rows;
        // The session-id field left of the chip, and the row above it.
        fixture
            .app
            .on_key(Key::Mouse(MouseEvent::Click { col: 3, row: rows }));
        fixture.app.on_key(Key::Mouse(MouseEvent::Click {
            col: 20,
            row: rows - 1,
        }));
        assert!(fixture.app.overlay.is_none());

        // Nothing running means no chip on the row, so no cell is a target.
        fixture.jobs.set(Vec::new());
        assert!(fixture.app.refresh_jobs(Instant::now()));
        let rows = fixture.app.live_region().len();
        fixture.app.rows = rows;
        assert!(!fixture.app.background_chip_at(20, rows));
        fixture
            .app
            .on_key(Key::Mouse(MouseEvent::Click { col: 20, row: rows }));
        assert!(fixture.app.overlay.is_none());
        assert!(fixture.app.text.is_empty());
    }
}

#[cfg(all(test, unix))]
mod mcp_run_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    // A minimal stdio MCP server: it answers `initialize` and `tools/list`
    // with one `echo` tool, so a client spawned from it advertises a
    // `MCP__fake__echo` definition. Same shape as tests/mcp_client.rs, trimmed
    // to the two requests a spawn makes.
    const FAKE_SERVER: &str = r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  method=$(printf '%s' "$line" | sed -n 's/.*"method":"\([^"]*\)".*/\1/p')
  case "$method" in
    initialize)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"fake","version":"0"}}}\n' "$id" ;;
    tools/list)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","description":"Echo text back","inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}}]}}\n' "$id" ;;
    *) ;;
  esac
done
"#;

    #[test]
    fn tui_run_spawns_a_role_referenced_config_server() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("fake-mcp-server");
        std::fs::write(&script, format!("#!/bin/sh\n{FAKE_SERVER}\n")).expect("write fake server");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake server");

        let mut configured = crate::tools::mcp::config::McpServerMap::new();
        configured.insert(
            "fake".to_string(),
            crate::tools::mcp::config::McpServerConfig {
                command: script.to_string_lossy().into_owned(),
                timeout_secs: 10,
                ..crate::tools::mcp::config::McpServerConfig::default()
            },
        );

        let env: HashMap<String, String> = HashMap::new();
        let cwd = dir.path().to_string_lossy().into_owned();
        let config = crate::core::config::create_default_cli_config();
        let args = ResolveRoleSetupArgs {
            config: &config,
            cwd: cwd.clone(),
            env: Some(&env),
            extra_bindings: None,
            extra_roles: Some(vec![crate::cli::roles::RoleDefinition {
                name: "author".to_string(),
                mcp_servers: Some(vec!["fake".to_string()]),
                ..crate::cli::roles::RoleDefinition::default()
            }]),
            marketplace_roles: None,
            skills: Vec::new(),
            tool_names: vec!["READ".to_string()],
            mcp_server_names: vec!["fake".to_string()],
        };

        let names = crate::cli::roles::referenced_mcp_servers(&args);
        assert_eq!(
            names,
            vec!["fake".to_string()],
            "the role opted the server in"
        );
        let (clients, warnings) = spawn_mcp_clients_for_run(&names, &configured, Path::new(&cwd));
        assert!(warnings.is_empty(), "spawn warnings: {warnings:?}");
        assert_eq!(clients.len(), 1, "one server was named by the role");

        let names: Vec<String> = crate::tools::mcp::mcp_tool_definitions(&clients)
            .into_iter()
            .map(|tool| tool.name)
            .collect();
        let mcp_name = names
            .iter()
            .find(|name| name.starts_with("MCP__fake__"))
            .cloned()
            .expect("the spawned server must contribute at least one MCP tool");

        // The role's `mcpServers` opt-in is what makes those tools callable in
        // a loop: with the role attached, the same names the pack carries pass
        // the loop gate.
        let role_setup = crate::cli::roles::resolve_role_setup(&args);
        let opened = role_setup.roles.iter().find(|role| {
            role.mcp_servers
                .as_ref()
                .map(|servers| servers.iter().any(|server| server == "fake"))
                .unwrap_or(false)
        });
        assert!(opened.is_some(), "role setup kept no mcpServers opt-in");
        let scope = crate::harness::r#loop::loop_tool_scope(&names, opened, None);
        assert!(
            scope.allowed.contains(&mcp_name),
            "the opted-in loop must see {mcp_name}: {:?}",
            scope.allowed
        );
    }

    // The newest Info line in the timeline, for asserting on what `/mcp` said.
    fn last_info(app: &TuiApp) -> String {
        app.cells
            .iter()
            .rev()
            .find_map(|entry| match entry {
                TranscriptEntry::Info(note) => Some(note.text.clone()),
                _ => None,
            })
            .expect("an info line landed in the timeline")
    }

    // `/mcp <server>` sets the run gate, which is what the TUI was missing: the
    // CLI's `--mcp` semantics, reachable without restarting the session.
    #[test]
    fn the_run_gate_spawns_servers_no_role_names_and_off_spawns_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("fake-mcp-server");
        std::fs::write(&script, format!("#!/bin/sh\n{FAKE_SERVER}\n")).expect("write fake server");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let mut configured = crate::tools::mcp::config::McpServerMap::new();
        configured.insert(
            "fake".to_string(),
            crate::tools::mcp::config::McpServerConfig {
                command: script.to_string_lossy().into_owned(),
                timeout_secs: 10,
                ..crate::tools::mcp::config::McpServerConfig::default()
            },
        );
        let home = tempfile::tempdir().expect("home tempdir");
        let mut app = super::rename_tests::rename_app(home.path());
        let cwd = app.bootstrap.cwd.clone();

        // `/mcp fake`: the gate names a server NO role in play mentions.
        app.mcp_run_gate = Some(vec!["fake".to_string()]);
        let (names, clients, warnings) = {
            let env: HashMap<String, String> = HashMap::new();
            let args = ResolveRoleSetupArgs {
                config: &app.config,
                cwd: cwd.clone(),
                env: Some(&env),
                extra_bindings: None,
                extra_roles: None,
                marketplace_roles: None,
                skills: Vec::new(),
                tool_names: vec!["READ".to_string()],
                mcp_server_names: configured.keys().cloned().collect(),
            };
            let names = app.mcp_spawn_names(&args);
            let (clients, warnings) =
                spawn_mcp_clients_for_run(&names, &configured, Path::new(&cwd));
            (names, clients, warnings)
        };
        assert_eq!(names, vec!["fake".to_string()], "the gate alone names it");
        assert!(warnings.is_empty(), "spawn warnings: {warnings:?}");
        assert_eq!(
            clients.len(),
            1,
            "an ungated role still gets the gate server"
        );
        let tools: Vec<String> = crate::tools::mcp::mcp_tool_definitions(&clients)
            .into_iter()
            .map(|tool| tool.name)
            .collect();
        assert!(
            tools.iter().any(|name| name.starts_with("MCP__fake__")),
            "{tools:?}"
        );
        let scope = crate::harness::r#loop::loop_tool_scope(&tools, None, Some(names.clone()));
        assert!(
            scope
                .allowed
                .iter()
                .any(|name| name.starts_with("MCP__fake__")),
            "a loop covered by the gate must see its tools: {:?}",
            scope.allowed
        );

        // `/mcp off`: a hard off that no role's own `mcpServers` can reopen.
        app.mcp_run_gate = Some(Vec::new());
        let off_names = {
            let env: HashMap<String, String> = HashMap::new();
            let args = ResolveRoleSetupArgs {
                config: &app.config,
                cwd: cwd.clone(),
                env: Some(&env),
                extra_bindings: None,
                extra_roles: Some(vec![crate::cli::roles::RoleDefinition {
                    name: "author".to_string(),
                    mcp_servers: Some(vec!["fake".to_string()]),
                    ..crate::cli::roles::RoleDefinition::default()
                }]),
                marketplace_roles: None,
                skills: Vec::new(),
                tool_names: vec!["READ".to_string()],
                mcp_server_names: configured.keys().cloned().collect(),
            };
            app.mcp_spawn_names(&args)
        };
        assert!(
            off_names.is_empty(),
            "/mcp off spawns nothing: {off_names:?}"
        );
    }

    // The affordance itself: `/mcp` lists the configured servers with their
    // status and toggles the run gate, and an unknown name never opens it.
    #[test]
    fn the_mcp_command_lists_servers_and_toggles_the_run_gate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = super::rename_tests::rename_app(dir.path());
        app.config.mcp_servers.insert(
            "fake".to_string(),
            crate::tools::mcp::config::McpServerConfig {
                command: "fake-mcp-server".to_string(),
                ..crate::tools::mcp::config::McpServerConfig::default()
            },
        );

        app.dispatch_command("mcp", "");
        let listed = last_info(&app);
        assert!(listed.contains("fake"), "{listed}");
        assert!(listed.contains("fake-mcp-server"), "{listed}");
        assert!(listed.contains("role opt-in only"), "{listed}");
        assert!(app.mcp_run_gate.is_none(), "listing never changes the gate");

        app.dispatch_command("mcp", "fake");
        assert_eq!(app.mcp_run_gate, Some(vec!["fake".to_string()]));
        assert!(
            last_info(&app).contains("on for this run"),
            "{}",
            last_info(&app)
        );
        app.dispatch_command("mcp", "");
        assert!(
            last_info(&app).contains("on for this run (gate)"),
            "{}",
            last_info(&app)
        );

        // The same name toggles back off, which is exactly `/mcp off`.
        app.dispatch_command("mcp", "fake");
        assert_eq!(app.mcp_run_gate, Some(Vec::new()));
        assert!(
            last_info(&app).contains("off for this run"),
            "{}",
            last_info(&app)
        );

        app.dispatch_command("mcp", "roles");
        assert!(app.mcp_run_gate.is_none());
        assert!(
            last_info(&app).contains("each loop sees a server"),
            "{}",
            last_info(&app)
        );

        app.dispatch_command("mcp", "ghost");
        assert!(
            app.mcp_run_gate.is_none(),
            "an unknown name must not open the gate"
        );
    }
}
