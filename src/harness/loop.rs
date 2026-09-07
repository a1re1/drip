// drip harness — chunk 1: pure helpers only.
//
// Constants and the stateless helpers the loop driver leans on (transcript
// folding, djb2 hashing, output spilling, footprint/verification extraction).
// The loop driver itself lives later in this module.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::core::state as core_state;
use crate::core::types::{
    HarnessEvent, HarnessEventData, HarnessEventType, HarnessRunReason, HarnessRunResult,
    HarnessRunUsage, HarnessTaskStatus,
};
use crate::harness::telemetry::truncate_text;
use crate::harness::chat_types::ChatRoleTag;
use crate::harness::transport::{TransportContent, TransportRequestMessage};

// Placeholder marker so the append below lands after the existing helpers.
pub const DEFAULT_DYNAMIC_TOOL_NAMES: [&str; 1] = ["DIR"];
pub const MAX_DIGEST_ACTIONS: usize = 12;
pub const MAX_DIGEST_ACTION_CHARS: usize = 200;
pub const MAX_RESULT_EVENT_CHARS: usize = 2000;
pub const FOLDED_RESULT_MARKER: &str = "[folded]";
pub const MAX_FOLDED_PREVIEW_CHARS: usize = 240;

// Folds tool results older than the hot window into one-line digests, so a
// task loop's transcript cannot grow without bound across its cycles. The most
// recent results stay verbatim ("hot"); telemetry keeps an ends-kept copy of
// each call's last result capped at maxPromotedOutputChars, so folded
// information stays recoverable up to that bound without re-running the call.
// Folded state is tracked structurally in foldedIndexes (messages only ever
// append within a loop, so indexes are stable) — never inferred from content,
// which a tool output could accidentally imitate.
pub fn fold_cold_tool_results(
    messages: &mut [TransportRequestMessage],
    hot_tool_results: usize,
    folded_indexes: &mut HashSet<usize>,
) -> usize {
    let mut unfolded_tool_indexes: Vec<usize> = Vec::new();

    for (index, message) in messages.iter().enumerate() {
        if message.role == ChatRoleTag::Tool
            && matches!(message.content, Some(TransportContent::Text(_)))
            && !folded_indexes.contains(&index)
        {
            unfolded_tool_indexes.push(index);
        }
    }

    let fold_count = unfolded_tool_indexes
        .len()
        .saturating_sub(hot_tool_results);
    let indexes_to_fold = &unfolded_tool_indexes[..fold_count];

    for &index in indexes_to_fold {
        let raw = match &messages[index].content {
            Some(TransportContent::Text(text)) => text.clone(),
            _ => continue,
        };
        // Collapse whitespace runs to single spaces, then trim the preview.
        let preview = truncate_text(&raw.split_whitespace().collect::<Vec<_>>().join(" "), MAX_FOLDED_PREVIEW_CHARS);
        let name = messages[index]
            .name
            .clone()
            .unwrap_or_else(|| "tool".to_string());

        messages[index].content = Some(TransportContent::Text(format!(
            "{} {} result from an earlier cycle of this loop, digest: {} — recover the full cached output with recall {{toolName, query}} instead of re-running",
            FOLDED_RESULT_MARKER, name, preview
        )));
        folded_indexes.insert(index);
    }

    fold_count
}

// Every task loop starts with a fresh transcript, and — transcript audits of
// flash lanes show — the next loop re-reads the files the previous one just
// read: the same task after running out of cycles (loop 3 read 11 files,
// loop 4 re-read 10 of them, then wrote), or the next task of the same goal
// ("survey the tools" then "write the doc" re-reads every tool file). The
// hot tail of the ended loop is replayed into the next task loop instead:
// the last few tool exchanges, verbatim, capped.
pub const MAX_CARRYOVER_CHARS: usize = 24_000;

#[derive(Clone, Debug)]
pub struct LoopCarryover {
    /// Whether the ended loop finished its task (the replay then feeds the next task).
    pub finished: bool,
    pub r#loop: i64,
    pub messages: Vec<TransportRequestMessage>,
    /// The ended loop's task, or None for a planning loop.
    pub task_id: Option<String>,
    pub task_title: Option<String>,
}

fn message_chars(message: &TransportRequestMessage) -> usize {
    match &message.content {
        Some(TransportContent::Text(text)) => text.chars().count(),
        // JSON.stringify(content ?? "").length + JSON.stringify(tool_calls ?? []).length
        other => {
            serde_json::to_string(&other.clone().unwrap_or(TransportContent::Text(String::new())))
                .map(|t| t.chars().count())
                .unwrap_or(0)
                + serde_json::to_string(&message.tool_calls.clone().unwrap_or_default())
                    .map(|t| t.chars().count())
                    .unwrap_or(0)
        }
    }
}

fn message_text(message: &TransportRequestMessage) -> &str {
    match &message.content {
        Some(TransportContent::Text(text)) => text.as_str(),
        _ => "",
    }
}

/// The tail of a loop transcript worth replaying into the next loop for the
/// same task: whole assistant→tool exchanges (never a dangling tool result)
/// covering at most `hot_tool_results` unfolded tool results and
/// MAX_CARRYOVER_CHARS, with the loop's own user/system messages left out.
/// Empty when the loop made no tool calls.
pub fn extract_loop_carryover(messages: &[TransportRequestMessage], hot_tool_results: usize) -> Vec<TransportRequestMessage> {
    // Blocks: an assistant message plus every tool result that follows it.
    let mut blocks: Vec<Vec<&TransportRequestMessage>> = Vec::new();
    for message in messages {
        match message.role {
            ChatRoleTag::Assistant => blocks.push(vec![message]),
            ChatRoleTag::Tool => {
                if let Some(last) = blocks.last_mut() {
                    last.push(message);
                }
            }
            _ => {}
        }
    }

    let mut kept: Vec<&Vec<&TransportRequestMessage>> = Vec::new();
    let mut tool_results = 0usize;
    let mut chars = 0usize;
    for block in blocks.iter().rev() {
        let block_results = block
            .iter()
            .filter(|message| message.role == ChatRoleTag::Tool && !message_text(message).starts_with(FOLDED_RESULT_MARKER))
            .count();
        let block_chars: usize = block.iter().map(|message| message_chars(message)).sum();
        if !kept.is_empty() && (tool_results + block_results > hot_tool_results || chars + block_chars > MAX_CARRYOVER_CHARS) {
            break;
        }
        kept.insert(0, block);
        tool_results += block_results;
        chars += block_chars;
    }

    let carried: Vec<TransportRequestMessage> = kept
        .into_iter()
        .flat_map(|block| block.iter().copied())
        .filter(|message| matches!(message.role, ChatRoleTag::Assistant | ChatRoleTag::Tool))
        .cloned()
        .collect();

    // Harness ops (plan_tasks, finish_task, observe, …) already live in the
    // state store the next loop is prompted from; a tail holding nothing but
    // those is not worth replaying.
    let has_workspace_result = carried
        .iter()
        .any(|message| message.role == ChatRoleTag::Tool && message.name.as_deref().is_some_and(|name| !crate::harness::harness_tools::is_harness_tool(name)));
    if has_workspace_result {
        carried
    } else {
        Vec::new()
    }
}

/// The user note that follows replayed exchanges.
pub fn build_carryover_note(carryover: &LoopCarryover, next_task_id: &str, exchanges: usize) -> String {
    let replayed = format!("its last {exchanges} tool exchange(s) are replayed above, verbatim");
    if carryover.task_id.as_deref() == Some(next_task_id) {
        return format!(
            "harness: loop {} worked this same task and ended before finish_task; {replayed}, so you continue from there instead of re-reading. Act on what they show now.",
            carryover.r#loop
        );
    }
    let previous = match &carryover.task_id {
        Some(task_id) => format!("{task_id} (\"{}\")", truncate_text(carryover.task_title.as_deref().unwrap_or(""), 80)),
        None => "the planning step".to_string(),
    };
    format!(
        "harness: loop {} worked {previous}{}; {replayed}, so this task builds on what was already read instead of re-reading. Act on what they show now.",
        carryover.r#loop,
        if carryover.finished { " and finished it" } else { "" }
    )
}

/// Fold the whole loop transcript past this size instead of waiting for the endpoint to reject it.
pub const MAX_LOOP_TRANSCRIPT_CHARS: usize = 300_000;

// Read-only tools whose identical repeat, on an unchanged workspace, returns
// exactly what an earlier (still verbatim) result in this loop's transcript
// already says. Transcript audits of flash runs found 38% of all READ calls
// were exact same-cycle repeats — the file's content was already in context.
pub const DEDUPED_READ_ONLY_TOOLS: [&str; 3] = ["READ", "GREP", "DIR"];

/// The stub that replaces a repeated read-only result whose earlier, identical
/// output is still verbatim in the transcript.
pub fn build_repeated_read_stub(tool_name: &str, prior_call_count: i64) -> String {
    format!(
        "[harness] {tool_name}: identical call #{} this run and the output is unchanged — it is verbatim in this conversation above (an earlier {tool_name} result). Use that copy; re-reading adds nothing. Act on it, or record the finding with observe/remember/finish_task.",
        prior_call_count + 1
    )
}

// A loop's transcript is discarded when the loop ends, so read-only calls
// that never turn into an edit or a recorded fact are pure waste — and the
// next loop re-reads the same files. Transcript audits of flash port lanes
// found 48% of loops were read-only end to end (530 of 894 READ/GREP/DIR
// calls in one 118-loop run), with a median of 1–5 reads before the first
// write in the healthy loops. Every READ_ONLY_NUDGE_EVERY-th read-only call
// in a loop that has not yet written or recorded anything gets one line.
pub const READ_ONLY_NUDGE_EVERY: i64 = 8;

// Shell commands that only inspect the workspace. Flash reads through BASH
// as much as through READ (`cat a.rs b.rs`, `sed -n '1,140p' x.rs`, `for f in
// …; do awk … $f; done`), so the read-only accounting has to see those too.
// Conservative: a command is read-only only when every segment's program is
// on this list and nothing is redirected to a file; anything unrecognized is
// treated as a write (no nudge).
const READ_ONLY_SHELL_PROGRAMS: [&str; 33] = [
    "[", "awk", "basename", "cat", "command", "cut", "diff", "dirname", "du", "echo", "file", "find", "grep", "head", "jq", "ls", "nl", "printf", "pwd",
    "realpath", "rg", "sed", "sort", "stat", "tail", "test", "tr", "tree", "true", "type", "uniq", "wc", "which",
];
const READ_ONLY_GIT_SUBCOMMANDS: [&str; 8] = ["blame", "diff", "grep", "log", "ls-files", "rev-parse", "show", "status"];
const SHELL_KEYWORD_SEGMENTS: [&str; 10] = ["", "do", "done", "else", "fi", "then", "{", "}", "(", ")"];

fn shell_res() -> &'static [regex::Regex; 6] {
    static RE: std::sync::OnceLock<[regex::Regex; 6]> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        [
            // Redirects that never touch a file: fd merges (`2>&1`, `1>&2`) and
            // /dev/null sinks in either direction.
            regex::Regex::new(r"[12&]?>&[12]|[12&]?>\s*/dev/null|<\s*/dev/null").expect("redirect regex"),
            regex::Regex::new(r"\|\|?|&&|;|\n").expect("segment regex"),
            regex::Regex::new(r"^for\s+\w+\s+in\b").expect("for regex"),
            regex::Regex::new(r"^[({\s]+").expect("wrapper regex"),
            regex::Regex::new(r"^(?:if|then|do|else)\s+").expect("keyword regex"),
            regex::Regex::new(r"^-[a-zA-Z]*i").expect("sed -i regex"),
        ]
    })
}

fn assignment_res() -> &'static [regex::Regex; 3] {
    static RE: std::sync::OnceLock<[regex::Regex; 3]> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        [
            regex::Regex::new(r"^\w+=\$\(").expect("assign subshell regex"),
            regex::Regex::new(r"^\w+=\S*\s+").expect("env assign regex"),
            regex::Regex::new(r"^\$\(").expect("subshell regex"),
        ]
    })
}

/// True when a BASH command only inspects the workspace (see READ_ONLY_SHELL_PROGRAMS).
pub fn is_read_only_shell_command(command: &str) -> bool {
    let [redirect_re, segment_re, for_re, wrapper_re, keyword_re, sed_i_re] = shell_res();
    // stderr merges and null redirects are fine; any other redirection or a
    // heredoc means the command writes somewhere.
    let stripped = redirect_re.replace_all(command, " ");
    if stripped.contains('>') || stripped.contains("<<") {
        return false;
    }
    let mut saw_program = false;
    for raw_segment in segment_re.split(&stripped) {
        let mut segment = raw_segment.trim();
        // Loop / conditional scaffolding and subshell wrappers carry no program.
        if SHELL_KEYWORD_SEGMENTS.contains(&segment) || for_re.is_match(segment) {
            continue;
        }
        let without_wrapper = wrapper_re.replace(segment, "");
        let without_keyword = keyword_re.replace(&without_wrapper, "");
        segment = without_keyword.as_ref();
        // `n=$(grep …)` and `FOO=bar cmd`: look at the program, not the assignment.
        let [assign_subshell_re, env_assign_re, subshell_re] = assignment_res();
        let a = assign_subshell_re.replace(segment, "");
        let b = env_assign_re.replace(&a, "");
        let c = subshell_re.replace(&b, "");
        let words: Vec<&str> = c.split_whitespace().collect();
        let program = words.first().copied().unwrap_or("");
        saw_program = true;
        if program == "git" {
            if !READ_ONLY_GIT_SUBCOMMANDS.contains(&words.get(1).copied().unwrap_or("")) {
                return false;
            }
            continue;
        }
        if !READ_ONLY_SHELL_PROGRAMS.contains(&program) {
            return false;
        }
        if program == "sed" && words.iter().any(|word| sed_i_re.is_match(word) || *word == "--in-place") {
            return false;
        }
        if program == "find" && words.iter().any(|word| matches!(*word, "-delete" | "-exec" | "-execdir" | "-ok")) {
            return false;
        }
    }
    saw_program
}

// Shell commands that plainly write to the workspace: a file redirection or
// heredoc, an in-place sed, or a program whose job is to create/move/remove
// files. Flash writes whole files through `cat > f <<'EOF'`, and such a loop
// has persisted something even though no PATCH ran (see WRITING_SHELL_PROGRAMS).
const WRITING_SHELL_PROGRAMS: [&str; 14] =
    ["chmod", "chown", "cp", "dd", "install", "ln", "mkdir", "mv", "patch", "rm", "rmdir", "tee", "touch", "truncate"];
const WRITING_GIT_SUBCOMMANDS: [&str; 15] = [
    "add", "am", "apply", "checkout", "cherry-pick", "commit", "merge", "mv", "rebase", "reset", "restore", "revert", "rm", "stash", "switch",
];

/// True when a BASH command plainly writes to the workspace (see WRITING_SHELL_PROGRAMS).
pub fn is_writing_shell_command(command: &str) -> bool {
    let [redirect_re, segment_re, _for_re, wrapper_re, keyword_re, sed_i_re] = shell_res();
    let stripped = redirect_re.replace_all(command, " ");
    if stripped.contains('>') || stripped.contains("<<") {
        return true;
    }
    let [assign_subshell_re, env_assign_re, subshell_re] = assignment_res();
    for raw_segment in segment_re.split(&stripped) {
        let without_wrapper = wrapper_re.replace(raw_segment.trim(), "");
        let without_keyword = keyword_re.replace(&without_wrapper, "");
        let a = assign_subshell_re.replace(&without_keyword, "");
        let b = env_assign_re.replace(&a, "");
        let c = subshell_re.replace(&b, "");
        let words: Vec<&str> = c.split_whitespace().collect();
        let program = words.first().copied().unwrap_or("");
        if WRITING_SHELL_PROGRAMS.contains(&program) {
            return true;
        }
        if program == "git" && WRITING_GIT_SUBCOMMANDS.contains(&words.get(1).copied().unwrap_or("")) {
            return true;
        }
        if program == "sed" && words.iter().any(|word| sed_i_re.is_match(word) || *word == "--in-place") {
            return true;
        }
    }
    false
}

/// The BASH command text from a raw tool input, or None.
pub fn extract_bash_command(raw_input: &str) -> Option<String> {
    let parsed: Value = serde_json::from_str(raw_input).ok()?;
    parsed.get("command").and_then(Value::as_str).map(str::to_string)
}

/// The nudge appended to a read-only result when a loop keeps reading without persisting.
pub fn build_read_only_loop_nudge(read_only_calls: i64, cycle: i64, max_cycles: i64) -> String {
    format!(
        "[harness] {read_only_calls} read-only calls this loop (cycle {cycle}/{max_cycles}) and nothing written or recorded yet. What this loop has read is dropped when the loop ends — act on it now: PATCH the change you can already make, record the facts you need with remember/observe, or finish_task blocked with what is missing."
    )
}

// Workspace-relative file paths a goal names explicitly ("NEW FILE
// src/lib/widget.rs", "update `drip/src/cli/entry.rs`"). Requires a directory
// separator so prose like "v1.2" or "README.md" never counts. The boundary
// check after each match lives in `extract_goal_paths` (the regex crate has
// no lookaround).
fn goal_path_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r#"(?:^|[\s`'"(\[<])((?:\.{0,2}/)?(?:[0-9A-Za-z_@.-]+/)+[0-9A-Za-z_@.-]+\.[A-Za-z0-9]{1,8})"#).expect("goal path regex")
    })
}

const GOAL_PATH_TRAILERS: &str = "`'\"),.]>:;";

/// Explicit file paths in the goal text, normalized (no leading ./), deduplicated.
pub fn extract_goal_paths(goal: &str) -> Vec<String> {
    let mut paths: Vec<String> = Vec::new();
    let mut from = 0;
    while let Some(caps) = goal_path_re().captures_at(goal, from) {
        let whole = caps.get(0).expect("match");
        let path = caps.get(1).expect("group");
        from = whole.end();
        // JS `(?=$|[\s`'"),.\]>:;])`: the path must end the text or be
        // followed by whitespace / closing punctuation.
        let boundary_ok = goal[path.end()..]
            .chars()
            .next()
            .is_none_or(|next| next.is_whitespace() || GOAL_PATH_TRAILERS.contains(next));
        if !boundary_ok {
            continue;
        }
        let normalized = path.as_str().strip_prefix("./").unwrap_or(path.as_str()).to_string();
        if !paths.contains(&normalized) {
            paths.push(normalized);
        }
    }
    paths
}

// A successful PATCH result names every file it created: the whole-file form
// says "Created <path> with N line(s)." and the files[] transaction form lists
// "<path>: created N line(s)".
fn patch_created_res() -> &'static [regex::Regex; 2] {
    static RE: std::sync::OnceLock<[regex::Regex; 2]> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        [
            regex::Regex::new(r"^Created (.+?) with \d+ line\(s\)\.$").expect("created regex"),
            regex::Regex::new(r"^(.+?): created \d+ line\(s\)$").expect("created regex"),
        ]
    })
}

/// Paths a successful PATCH result reports as newly created.
pub fn extract_created_paths(patch_output: &str) -> Vec<String> {
    let mut created = Vec::new();
    for line in patch_output.split('\n') {
        let line = line.trim();
        for re in patch_created_res() {
            if let Some(caps) = re.captures(line) {
                let path = caps.get(1).expect("group").as_str();
                created.push(path.strip_prefix("./").unwrap_or(path).to_string());
                break;
            }
        }
    }
    created
}

/// The note appended to a PATCH result that created a file at a path the goal
/// never named, when the goal does name paths. Small models under a precise
/// contract still invent a sibling file (a "result_payload.rs" beside the
/// "headless_output.rs" the goal asked for) and then build on it; the first
/// PATCH to an unnamed new path is the earliest signal. None when the goal
/// names no paths, or every created path is one of them (or under a directory
/// the goal names).
pub fn build_unnamed_path_note(goal: &str, patch_output: &str) -> Option<String> {
    let goal_paths = extract_goal_paths(goal);
    if goal_paths.is_empty() {
        return None;
    }
    let unnamed: Vec<String> = extract_created_paths(patch_output)
        .into_iter()
        .filter(|path| {
            !goal_paths.contains(path) && !goal_paths.iter().any(|goal_path| path.starts_with(&format!("{goal_path}/")))
        })
        .collect();
    if unnamed.is_empty() {
        return None;
    }
    let mut shown = goal_paths.iter().take(4).cloned().collect::<Vec<_>>().join(", ");
    if goal_paths.len() > 4 {
        shown.push_str(&format!(", … ({} paths)", goal_paths.len()));
    }
    Some(format!(
        "[harness] PATCH created {}, which the goal does not name (it names {shown}). If the goal wanted this code at one of its named paths, move it there now instead of building on the new file; if the new file is intentional, say why in your next finish_task note.",
        unnamed.join(", ")
    ))
}

// Commands whose outcome IS the verification story of the run: the harness
// records the most recent one so summaries and results cite ground truth.
// Hand-rolled scan recognizing package-manager runners (bun/npm/pnpm/yarn)
// followed by test/check/lint/typecheck/build, direct tools like
// pytest/vitest/jest/tsc, and cargo/go/make subcommands.
fn is_word_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn word_at(bytes: &[u8], start: usize, end: usize, word: &str) -> bool {
    start < end && &bytes[start..end] == word.as_bytes()
}

fn verification_pattern_matches(command: &str) -> bool {
    let bytes = command.as_bytes();
    let len = bytes.len();
    let mut index = 0;

    while index < len {
        if !is_word_char(bytes[index]) {
            index += 1;
            continue;
        }

        let start = index;
        while index < len && is_word_char(bytes[index]) {
            index += 1;
        }
        let end = index;

        let is_runner = ["bun", "npm", "pnpm", "yarn"]
            .iter()
            .any(|word| word_at(bytes, start, end, word));
        if is_runner {
            let mut cursor = end;
            if cursor < len && (bytes[cursor] as char).is_whitespace() {
                while cursor < len && (bytes[cursor] as char).is_whitespace() {
                    cursor += 1;
                }
                let verb_start = cursor;
                while cursor < len && is_word_char(bytes[cursor]) {
                    cursor += 1;
                }
                if word_at(bytes, verb_start, cursor, "run") {
                    let mut tail = cursor;
                    if tail < len && (bytes[tail] as char).is_whitespace() {
                        while tail < len && (bytes[tail] as char).is_whitespace() {
                            tail += 1;
                        }
                        let target_start = tail;
                        while tail < len && is_word_char(bytes[tail]) {
                            tail += 1;
                        }
                        if ["test", "check", "lint", "typecheck", "build"]
                            .iter()
                            .any(|word| word_at(bytes, target_start, tail, word))
                        {
                            return true;
                        }
                    }
                } else if ["test", "check", "lint", "typecheck", "build"]
                    .iter()
                    .any(|word| word_at(bytes, verb_start, cursor, word))
                {
                    return true;
                }
            }
        }

        let paired = |tool: &str, targets: &[&str]| -> bool {
            if !word_at(bytes, start, end, tool) {
                return false;
            }
            let mut cursor = end;
            if !(cursor < len && (bytes[cursor] as char).is_whitespace()) {
                return false;
            }
            while cursor < len && (bytes[cursor] as char).is_whitespace() {
                cursor += 1;
            }
            let target_start = cursor;
            while cursor < len && is_word_char(bytes[cursor]) {
                cursor += 1;
            }
            targets
                .iter()
                .any(|word| word_at(bytes, target_start, cursor, word))
        };

        if paired("cargo", &["test", "check"])
            || paired("go", &["test", "vet"])
            || paired("make", &["test", "check", "lint"])
            || ["pytest", "vitest", "jest", "tsc"]
                .iter()
                .any(|word| word_at(bytes, start, end, word))
        {
            return true;
        }
    }

    false
}

/// One-shot corrective push for narration-only replies (exported for tests).
pub const TRUNCATION_NUDGE_MESSAGE: &str = "harness: that reply was cut off at the provider's output-token limit before any tool call, so nothing was done. Reply again with far less text: one sentence at most, then the tool call. If you are writing a large file, split it into two or three PATCH calls of a few hundred lines each.";
pub const NARRATION_NUDGE_MESSAGE: &str = "harness: that reply was narration, not work — it was recorded as a task note. Act through tool calls now (BASH/READ/PATCH/...), or call finish_task if the task is genuinely done; a second text-only reply ends this loop.";

/// Verification runs kept in the state timeline (newest last).
pub const MAX_VERIFICATION_TIMELINE: usize = 20;

/// Identical consecutive failures at which the harness calls the loop stuck.
pub const VERIFICATION_STUCK_THRESHOLD: u32 = 2;

// djb2 — stable, dependency-free fingerprint for "did the failure change".
// Iterates UTF-16 code units so the hash is stable regardless of encoding.
pub fn hash_text(text: &str) -> String {
    let mut hash: u32 = 5381;

    for unit in text.encode_utf16() {
        hash = (hash << 5).wrapping_add(hash).wrapping_add(u32::from(unit));
    }

    format!("{:x}", hash)
}

// Full tool output for truncated results, written beside the session state
// (…/<session>/outputs/<loop>-<callId>.log). Best-effort: spill failures
// must never fail the tool round.
pub fn spill_tool_output(state_path: &str, loop_number: u32, call_id: &str, content: &str) -> Option<String> {
    let result = (|| -> Option<String> {
        let dir = Path::new(state_path)
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("outputs");

        fs::create_dir_all(&dir).ok()?;

        let mut safe_call_id: String = call_id
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
            .collect();
        safe_call_id = safe_call_id.chars().take(60).collect();
        let spill_path = dir.join(format!("{}-{}.log", loop_number, safe_call_id));

        fs::write(&spill_path, content).ok()?;

        Some(spill_path.to_string_lossy().into_owned())
    })();

    result
}

/// Paths a PATCH call touches — single path or the files[] transaction form.
pub fn extract_patched_paths(raw_input: &str) -> Vec<String> {
    let parsed = match serde_json::from_str::<Value>(raw_input) {
        Ok(parsed) => parsed,
        Err(_) => return Vec::new(),
    };

    let mut paths: Vec<String> = Vec::new();

    if let Some(path) = parsed.get("path").and_then(Value::as_str) {
        paths.push(path.to_string());
    }

    match parsed.get("files") {
        Some(Value::Array(files)) => {
            for file in files {
                if let Some(path) = file.get("path").and_then(Value::as_str) {
                    paths.push(path.to_string());
                }
            }
        }
        // A non-array `files` yields no paths.
        Some(_) => return Vec::new(),
        None => {}
    }

    paths
}

pub const MAX_EDITED_PATHS: usize = 500;

/// Remember a workspace path the run edited (deduplicated, bounded).
pub fn record_edited_path(edited_paths: &mut Vec<String>, path: &str) {
    let normalized = path.trim().trim_start_matches("./").to_string();
    if normalized.is_empty() || edited_paths.iter().any(|known| known == &normalized) {
        return;
    }
    if edited_paths.len() >= MAX_EDITED_PATHS {
        return;
    }
    edited_paths.push(normalized);
}

/// The anchor a VERIFY call declared, downgraded to self-authored when the
/// command names a file this run edited: a check the agent wrote only shows
/// the artifact agrees with the agent's own derivation.
pub fn declared_verification_anchor(
    raw_input: &str,
    edited_paths: &[String],
) -> Option<crate::core::types::VerificationAnchor> {
    use crate::core::types::{VerificationAnchor, VerificationAnchorKind};
    let input: serde_json::Value = serde_json::from_str(raw_input).ok()?;
    let anchor = input.get("anchor")?;
    let source = anchor
        .get("source")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string);
    let kind = match anchor.get("kind").and_then(|value| value.as_str()) {
        Some("external") => VerificationAnchorKind::External,
        Some("self") => VerificationAnchorKind::SelfAuthored,
        _ => VerificationAnchorKind::Undeclared,
    };
    if kind != VerificationAnchorKind::External {
        return Some(VerificationAnchor { kind, source, downgraded_reason: None });
    }
    let command = input.get("command").and_then(|value| value.as_str()).unwrap_or_default();
    let haystack = format!("{command} {}", source.as_deref().unwrap_or_default());
    let named = edited_paths.iter().find(|path| command_names_path(&haystack, path));
    match named {
        Some(path) => Some(VerificationAnchor {
            kind: VerificationAnchorKind::SelfAuthored,
            source,
            downgraded_reason: Some(format!(
                "the check names {path}, which this run edited; a check the agent authored is consistency, not correctness"
            )),
        }),
        None => Some(VerificationAnchor { kind, source, downgraded_reason: None }),
    }
}

/// Whether a command/source text names an edited path, matched on token
/// boundaries: the full path, a suffix of a longer path, the file name, or
/// (for runner forms like `--test test_totals`) a test-shaped stem. Plain
/// substring matching would downgrade `pytest tests/` for any edited file
/// under tests/ and miss extension-less runner targets.
pub fn command_names_path(haystack: &str, path: &str) -> bool {
    let path = path.trim().trim_start_matches("./");
    if path.is_empty() {
        return false;
    }
    let file_name = std::path::Path::new(path).file_name().and_then(|name| name.to_str()).unwrap_or_default();
    let stem = std::path::Path::new(path).file_stem().and_then(|name| name.to_str()).unwrap_or_default();
    let test_shaped_stem = stem.len() >= 4 && (stem.contains('_') || stem.contains('-'));
    haystack
        .split(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '(' | ')' | ';' | '|' | '&' | ',' | '='))
        .map(|token| token.trim_start_matches("./"))
        .filter(|token| !token.is_empty())
        .any(|token| {
            token == path
                || token.ends_with(&format!("/{path}"))
                || (!file_name.is_empty() && token == file_name)
                || (test_shaped_stem && token == stem)
        })
}

/// Paths a writing shell command names as its targets: redirect targets
/// (`> f`, `>> f`, `2> f`), `tee [-a] f`, and the file operands of
/// `sed -i`/`sed --in-place`. Best-effort — heredoc bodies and other
/// writers (`cp`, `mv`, `git`) are not resolved to paths.
pub fn extract_shell_write_targets(command: &str) -> Vec<String> {
    let mut targets: Vec<String> = Vec::new();
    let mut push = |token: &str| {
        let token = token.trim_matches(|c| matches!(c, '"' | '\'' | ';' | ')' | '(')).trim_start_matches("./");
        if !token.is_empty() && !token.starts_with('-') && !token.starts_with('$') && token != "/dev/null" && !targets.iter().any(|known| known == token) {
            targets.push(token.to_string());
        }
    };
    for segment in strip_heredoc_bodies(command).split(|c| matches!(c, ';' | '|' | '&' | '\n')) {
        let words: Vec<&str> = segment.split_whitespace().collect();
        for (index, word) in words.iter().enumerate() {
            let trimmed = word.trim_start_matches(|c: char| c.is_ascii_digit());
            if trimmed == ">" || trimmed == ">>" {
                if let Some(next) = words.get(index + 1) {
                    push(next);
                }
            } else if let Some(rest) = trimmed.strip_prefix(">>").or_else(|| trimmed.strip_prefix('>')) {
                if !rest.is_empty() {
                    push(rest);
                }
            }
        }
        let program = words.first().copied().unwrap_or("");
        if program == "tee" {
            for word in words[1..].iter().filter(|word| !word.contains('>') && !word.contains('<')) {
                push(word);
            }
        }
        if program == "sed" && words.iter().any(|word| *word == "-i" || word.starts_with("-i") || *word == "--in-place") {
            // Operands after the script: every non-flag word past the first
            // non-flag word (the script itself).
            let mut seen_script = false;
            for word in &words[1..] {
                if word.starts_with('-') {
                    continue;
                }
                if !seen_script {
                    seen_script = true;
                    continue;
                }
                push(word);
            }
        }
    }
    targets
}

pub const MAX_FOOTPRINT_ENTRIES: usize = 20;

pub fn record_task_footprint(footprint: &mut Option<Vec<String>>, entry: &str) {
    let list = footprint.get_or_insert_with(Vec::new);

    if list.last().map(String::as_str) == Some(entry) {
        return;
    }

    list.push(entry.to_string());

    if list.len() > MAX_FOOTPRINT_ENTRIES {
        let had_edits = list.iter().any(|entry| entry.starts_with("edited "));
        let overflow = list.len() - MAX_FOOTPRINT_ENTRIES;
        list.drain(0..overflow);
        if had_edits && !list.iter().any(|entry| entry.starts_with("edited ")) {
            list[0] = "edited workspace (earlier in this task)".into();
        }
    }
}

/// Test runners whose output says how many tests actually executed. A green
/// exit from one of these with zero tests is the classic false verification:
/// a crate with no #[test]s, a glob that matched nothing, a filter typo.
fn is_test_runner_command(command: &str) -> bool {
    let bytes = command.as_bytes();
    for word in ["test", "vitest", "jest", "pytest"] {
        let mut from = 0;
        while let Some(pos) = command[from..].find(word) {
            let start = from + pos;
            let end = start + word.len();
            let before_ok = start == 0 || !is_word_byte(bytes[start - 1]);
            let after_ok = end == bytes.len() || !is_word_byte(bytes[end]);
            if before_ok && after_ok {
                return true;
            }
            from = end;
        }
    }
    false
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// True when a passing test-shaped command demonstrably executed no tests —
/// decided from the runner's own summary lines, never from silence.
pub fn detect_empty_test_run(command: &str, output: &str) -> bool {
    if !is_test_runner_command(command) {
        return false;
    }

    // cargo test / libtest: a "test result:" summary that counts executed tests
    // is proof by itself — the "running N tests" headers above it are the first
    // thing a `| tail` cuts off, and the doc-test block that closes cargo's
    // output always says "running 0 tests".
    let executed_by_summary = output.lines().any(|line| {
        let Some(rest) = line.strip_prefix("test result: ") else { return false };
        let Some(rest) = rest.strip_prefix("ok. ").or_else(|| rest.strip_prefix("FAILED. ")) else { return false };
        let mut counts = rest.split("; ").filter_map(|part| {
            let (count, label) = part.split_once(' ')?;
            (label == "passed" || label == "failed").then(|| count.parse::<u64>().ok()).flatten()
        });
        counts.any(|count| count > 0)
    });
    if executed_by_summary {
        return false;
    }

    // Otherwise one "running N tests" header per test binary.
    let cargo_runs: Vec<u64> = output
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("running ")?;
            let rest = rest.strip_suffix(" tests").or_else(|| rest.strip_suffix(" test"))?;
            rest.parse::<u64>().ok()
        })
        .collect();
    if !cargo_runs.is_empty() && cargo_runs.iter().all(|count| *count == 0) {
        return true;
    }

    // bun test summary: " 0 pass" / " 0 fail".
    let has_line = |needle: &str| output.lines().any(|line| line.trim_start() == needle);
    if has_line("0 pass") && has_line("0 fail") {
        return true;
    }

    // vitest / jest / pytest / go test spell it out.
    let lower = output.to_lowercase();
    if ["no test files found", "no tests found", "no tests ran", "collected 0 items"]
        .iter()
        .any(|marker| lower.contains(marker))
    {
        return true;
    }

    let go_packages: Vec<&str> = output
        .lines()
        .filter(|line| {
            let mut parts = line.split_whitespace();
            matches!(parts.next(), Some("ok") | Some("FAIL") | Some("?")) && parts.next().is_some()
        })
        .collect();
    if !go_packages.is_empty() && go_packages.iter().all(|line| line.contains("[no test files]")) {
        return true;
    }

    false
}

fn heredoc_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r#"<<-?\s*(?:'([^']+)'|"([^"]+)"|(\w+))"#).expect("heredoc regex"))
}

/// A command with its heredoc bodies removed (`cat > f <<'EOF' … EOF` keeps
/// only the `cat > f <<'EOF'` line). Flash writes whole files through BASH
/// heredocs, and a body that merely *mentions* "bun test" or "cargo check"
/// must not record the write as the run's verification.
pub fn strip_heredoc_bodies(command: &str) -> String {
    let mut kept: Vec<&str> = Vec::new();
    let mut terminator: Option<String> = None;
    for line in command.split('\n') {
        if let Some(word) = &terminator {
            if line.trim() == word {
                terminator = None;
            }
            continue;
        }
        kept.push(line);
        if let Some(caps) = heredoc_re().captures(line) {
            terminator = caps
                .get(1)
                .or_else(|| caps.get(2))
                .or_else(|| caps.get(3))
                .map(|m| m.as_str().to_string());
        }
    }
    kept.join("\n")
}

/// Whitespace-collapsed, trimmed text — the shape goal/command containment is checked in.
fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A goal contract usually names its own verification ("Verification: test -s
/// docs/TOOLS.md && grep -c '^## ' docs/TOOLS.md prints 8"), and that command
/// is rarely a test runner the VERIFICATION_COMMAND_PATTERN knows. A BASH
/// command the goal text contains verbatim (whitespace-collapsed; at least one
/// space and 8 characters, so `ls` does not qualify) is the goal's declared
/// verification and counts like one.
pub fn is_goal_declared_verification(goal: &str, command: &str) -> bool {
    let collapsed = collapse_whitespace(&strip_heredoc_bodies(command));
    collapsed.chars().count() >= 8 && collapsed.contains(' ') && collapse_whitespace(goal).contains(&collapsed)
}

pub fn extract_verification_command(tool_name: &str, raw_input: &str) -> Option<String> {
    extract_verification_command_for_goal(tool_name, raw_input, "")
}

pub fn extract_verification_command_for_goal(tool_name: &str, raw_input: &str, goal: &str) -> Option<String> {
    if tool_name == "CHECK" {
        return Some(format!("CHECK {}", raw_input));
    }
    // BASH_ASYNC is excluded: its "success" is the launch, not the tests — an
    // async test run would record as passed the moment it started.
    if tool_name != "BASH" && tool_name != "VERIFY" {
        return None;
    }

    let parsed = serde_json::from_str::<Value>(raw_input).ok()?;
    let command = parsed
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    // Calling VERIFY *is* the declaration that this command verifies the work
    // — no pattern sniffing; BASH still gets the heuristic.
    if tool_name == "VERIFY" {
        return if command.is_empty() { None } else { Some(command) };
    }

    if verification_pattern_matches(&strip_heredoc_bodies(&command)) {
        Some(command)
    } else if !goal.is_empty() && is_goal_declared_verification(goal, &command) {
        Some(command)
    } else {
        None
    }
}

pub fn extract_response_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| match part {
                Value::String(text) => Some(text.clone()),
                other => other
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

#[cfg(test)]
mod loop_helpers_tests {
    use super::*;
    use crate::harness::transport::TransportRequestMessage;

    fn tool_message(name: &str, content: &str) -> TransportRequestMessage {
        TransportRequestMessage {
            content: Some(TransportContent::Text(content.to_string())),
            name: Some(name.to_string()),
            role: ChatRoleTag::Tool,
            ..Default::default()
        }
    }

    #[test]
    fn fold_marks_cold_results_with_marker_and_preview() {
        let long_output = "line one\n\nline   two\ttab".to_string() + &" x".repeat(300);
        let mut messages = vec![
            tool_message("BASH", &long_output),
            tool_message("READ", "older result"),
            tool_message("BASH", "hottest result"),
        ];
        let mut folded = HashSet::new();

        let folded_count = fold_cold_tool_results(&mut messages, 1, &mut folded);

        assert_eq!(folded_count, 2);
        assert_eq!(folded, HashSet::from([0usize, 1]));

        let digest = match &messages[0].content {
            Some(TransportContent::Text(text)) => text.clone(),
            other => panic!("expected text content, got {:?}", other),
        };
        assert!(digest.starts_with("[folded] BASH result from an earlier cycle of this loop, digest: "));
        assert!(digest.contains("line one line two tab"));
        assert!(digest.contains("recover the full cached output with recall {toolName, query}"));

        // The hot result and non-tool messages stay untouched.
        assert_eq!(
            messages[2].content,
            Some(TransportContent::Text("hottest result".to_string()))
        );
    }

    // "classifies the shell commands flash reads with as read-only"
    #[test]
    fn read_only_shell_commands_are_classified_like_the_ts() {
        for command in [
            "ls drip/src/tools/builtin/ && ls drip/ 2>/dev/null; ls tools/ 2>/dev/null",
            "wc -l drip/src/tools/builtin/*.rs",
            "cat tools/read-tool.ts tools/dir-tool.ts",
            "cat tools/grep-tool.ts | sed -n '1,80p'; echo ===; grep -n \"name:\" tools/*.ts",
            "for f in grep dir patch; do echo \"=== $f ===\"; awk '/pub fn definition\\(\\)/,/^}/' drip/src/tools/builtin/$f.rs; done",
            "n=$(grep -n \"fn definition\" drip/src/tools/builtin/bash.rs | cut -d: -f1); sed -n \"$n,$((n+80))p\" drip/src/tools/builtin/bash.rs",
            "test -s drip/docs/TOOLS.md && grep -c \"^## \" drip/docs/TOOLS.md",
            "git log --oneline -5 | head -3",
            "rg -n 'fn main' src/ 2>&1 | head",
            "find . -name '*.rs' | wc -l",
            "ls missing-dir >/dev/null; cat a.ts 1>&2; grep -q x a.ts &>/dev/null",
        ] {
            assert!(is_read_only_shell_command(command), "{command}");
        }
        for command in [
            "mkdir -p drip/docs && cat > drip/docs/TOOLS.md <<'EOF'\n# x\nEOF",
            "sed -i '' 's/a/b/' src/x.ts",
            "sed -ni 's/a/b/p' src/x.ts",
            "cargo test 2>&1 | tail -20",
            "echo hi > out.txt",
            "find . -name '*.tmp' -delete",
            "git commit -am wip",
            "cat a.ts | tee b.ts",
            "python3 -c 'print(1)'",
            "",
        ] {
            assert!(!is_read_only_shell_command(command), "{command}");
        }
        for command in [
            "mkdir -p drip/docs && cat > drip/docs/TOOLS.md <<'EOF'\n# x\nEOF",
            "sed -i '' 's/a/b/' src/x.ts",
            "echo hi > out.txt",
            "git add -A && git commit -m wip",
            "cat a.ts | tee b.ts",
            "for f in a b; do touch $f; done",
        ] {
            assert!(is_writing_shell_command(command), "{command}");
        }
        for command in ["cargo test 2>&1 | tail -20", "cat tools/read-tool.ts", "git status", "python3 -c 'print(1)'", "", "cargo build >/dev/null 2>&1", "echo x 1>&2"] {
            assert!(!is_writing_shell_command(command), "{command}");
        }
        assert_eq!(extract_bash_command(r#"{"command":"ls"}"#).as_deref(), Some("ls"));
        assert_eq!(extract_bash_command("{"), None);
    }

    // "a command the goal names verbatim is its declared verification"
    #[test]
    fn goal_declared_commands_count_as_verification() {
        let goal = "NEW FILE docs/TOOLS.md … Verification: test -s docs/TOOLS.md && grep -c \"^## \" docs/TOOLS.md prints 8. Scope: docs only.";
        let declared = r#"test -s docs/TOOLS.md   && grep -c "^## " docs/TOOLS.md"#;
        assert!(is_goal_declared_verification(goal, declared));
        assert!(!is_goal_declared_verification(goal, "ls docs"));
        assert!(!is_goal_declared_verification(goal, "grep -c \"^## \" other.md"));
        let raw = serde_json::json!({ "command": declared }).to_string();
        assert_eq!(extract_verification_command_for_goal("BASH", &raw, goal).as_deref(), Some(declared));
        assert_eq!(extract_verification_command_for_goal("BASH", &raw, ""), None);
        assert_eq!(extract_verification_command("BASH", &raw), None);
        // VERIFY is the declaration itself — the goal heuristic never changes it.
        assert_eq!(extract_verification_command_for_goal("VERIFY", r#"{"command":"ls"}"#, "").as_deref(), Some("ls"));
        assert_eq!(extract_verification_command_for_goal("VERIFY", r#"{"command":""}"#, goal), None);
    }


    // "keeps whole exchanges within the hot-result and size caps, newest first"
    #[test]
    fn loop_carryover_keeps_whole_exchanges_within_caps_newest_first() {
        fn exchange(id: &str, content: &str) -> Vec<TransportRequestMessage> {
            vec![
                TransportRequestMessage {
                    role: ChatRoleTag::Assistant,
                    tool_calls: Some(vec![crate::harness::transport::OpenAICompatibleToolCall {
                        function: Some(crate::harness::transport::OpenAICompatibleToolCallFunction {
                            arguments: Some("{}".to_string()),
                            name: Some("READ".to_string()),
                        }),
                        id: Some(id.to_string()),
                        tool_type: Some("function".to_string()),
                    }]),
                    ..Default::default()
                },
                TransportRequestMessage {
                    content: Some(TransportContent::Text(content.to_string())),
                    name: Some("READ".to_string()),
                    role: ChatRoleTag::Tool,
                    tool_call_id: Some(id.to_string()),
                    ..Default::default()
                },
            ]
        }
        fn user(text: &str) -> TransportRequestMessage {
            TransportRequestMessage { content: Some(TransportContent::Text(text.to_string())), role: ChatRoleTag::User, ..Default::default() }
        }
        let mut messages = vec![TransportRequestMessage { content: Some(TransportContent::Text("sys".into())), role: ChatRoleTag::System, ..Default::default() }, user("iteration")];
        messages.extend(exchange("a", "one"));
        messages.extend(exchange("b", "two"));
        messages.push(user("continue"));
        messages.extend(exchange("c", "three"));

        let shape = |kept: Vec<TransportRequestMessage>| -> Vec<String> {
            kept.iter().map(|m| if m.role == ChatRoleTag::Tool { message_text(m).to_string() } else { "call".to_string() }).collect()
        };
        assert_eq!(shape(extract_loop_carryover(&messages, 2)), vec!["call", "two", "call", "three"]);
        assert!(extract_loop_carryover(&messages, 6).iter().all(|m| m.role != ChatRoleTag::User));
        assert!(extract_loop_carryover(&messages[..1], 6).is_empty());
        // One oversized exchange is still carried (never an empty handoff for a loop that read something).
        assert_eq!(extract_loop_carryover(&exchange("big", &"x".repeat(30_000)), 6).len(), 2);
        let same = LoopCarryover { finished: false, r#loop: 3, messages: vec![], task_id: Some("task-1".into()), task_title: Some("read then finish".into()) };
        assert_eq!(
            build_carryover_note(&same, "task-1", 1),
            "harness: loop 3 worked this same task and ended before finish_task; its last 1 tool exchange(s) are replayed above, verbatim, so you continue from there instead of re-reading. Act on what they show now."
        );
        let finished = LoopCarryover { finished: true, r#loop: 2, messages: vec![], task_id: Some("task-1".into()), task_title: Some("survey".into()) };
        assert_eq!(
            build_carryover_note(&finished, "task-2", 4),
            "harness: loop 2 worked task-1 (\"survey\") and finished it; its last 4 tool exchange(s) are replayed above, verbatim, so this task builds on what was already read instead of re-reading. Act on what they show now."
        );
        let planning = LoopCarryover { finished: false, r#loop: 1, messages: vec![], task_id: None, task_title: None };
        assert_eq!(
            build_carryover_note(&planning, "task-1", 2),
            "harness: loop 1 worked the planning step; its last 2 tool exchange(s) are replayed above, verbatim, so this task builds on what was already read instead of re-reading. Act on what they show now."
        );
    }

    #[test]
    fn extract_goal_paths_finds_explicit_workspace_paths() {
        let goal = "Definition of done: 1. NEW FILE drip/src/cli/headless_output.rs — port of src/cli/headless-output.ts.\n2. UPDATED `drip/src/cli/mod.rs` (see ./drip/PLAN.md). Verify with cargo test; v1.2.3 and README.md are not paths.";
        assert_eq!(
            extract_goal_paths(goal),
            vec!["drip/src/cli/headless_output.rs", "src/cli/headless-output.ts", "drip/src/cli/mod.rs", "drip/PLAN.md"]
        );
        assert!(extract_goal_paths("fix the flaky test").is_empty());
        // A path glued to a following word is not a path (JS lookahead parity).
        assert!(extract_goal_paths("src/a.ts_x").is_empty());
    }

    #[test]
    fn extract_created_paths_reads_both_patch_result_forms() {
        assert_eq!(extract_created_paths("Created src/new.ts with 12 line(s)."), vec!["src/new.ts"]);
        assert!(extract_created_paths("Overwrote src/old.ts with 3 line(s).").is_empty());
        assert_eq!(
            extract_created_paths("Applied 2 file(s):\n  src/a.ts: replaced 1 occurrence(s)\n  src/b.ts: created 4 line(s)\n"),
            vec!["src/b.ts"]
        );
    }

    #[test]
    fn unnamed_path_note_fires_only_for_new_files_outside_the_goal() {
        let goal = "NEW FILE drip/src/cli/headless_output.rs mirroring src/cli/headless-output.ts";
        let note = build_unnamed_path_note(goal, "Created drip/src/result_payload.rs with 40 line(s).").unwrap();
        assert!(note.contains("[harness] PATCH created drip/src/result_payload.rs, which the goal does not name"));
        assert!(note.contains("it names drip/src/cli/headless_output.rs, src/cli/headless-output.ts"));
        assert!(build_unnamed_path_note(goal, "Created drip/src/cli/headless_output.rs with 40 line(s).").is_none());
        assert!(build_unnamed_path_note(goal, "Overwrote drip/src/result_payload.rs with 40 line(s).").is_none());
        assert!(build_unnamed_path_note("port the module", "Created drip/src/anything.rs with 1 line(s).").is_none());
        assert!(build_unnamed_path_note("add fixtures under test/fixtures/widgets", "Created test/fixtures/widgets/a.json with 1 line(s).").is_none());
    }

    #[test]
    fn extract_patched_paths_reads_files_array_and_single_path() {
        let raw = r#"{"files":[{"path":"a.rs"},{"path":"b.rs"},{"nope":1}],"path":"c.rs"}"#;
        assert_eq!(
            extract_patched_paths(raw),
            vec!["c.rs".to_string(), "a.rs".to_string(), "b.rs".to_string()]
        );

        assert!(extract_patched_paths("not json {").is_empty());
        assert!(extract_patched_paths(r#"{"files":5}"#).is_empty());
    }

    #[test]
    fn detect_empty_test_run_flags_runners_that_executed_zero_tests() {
        assert!(detect_empty_test_run("cargo test", "running 0 tests\n\ntest result: ok. 0 passed\n\nrunning 0 tests\n"));
        // A `| tail` that kept only the doc-test block's header but also the lib
        // block's summary: the summary proves tests ran.
        assert!(!detect_empty_test_run(
            "cargo test 2>&1 | tail -20",
            "test plan::b ... ok\n\ntest result: ok. 13 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n\n   Doc-tests reviewkit\n\nrunning 0 tests\n\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n"
        ));
        assert!(!detect_empty_test_run("cargo test", "running 0 tests\n\ntest result: FAILED. 0 passed; 2 failed; 0 ignored\n"));
        assert!(detect_empty_test_run("bun test", "bun test v1.2\n\n 0 pass\n 0 fail\n"));
        assert!(detect_empty_test_run("bun run test", "No test files found, exiting with code 1"));
        assert!(detect_empty_test_run("pytest -k widget", "collected 0 items\n\nno tests ran in 0.01s"));
        assert!(detect_empty_test_run("go test ./...", "?   \texample.com/a\t[no test files]\n?   \texample.com/b\t[no test files]\n"));
    }

    #[test]
    fn detect_empty_test_run_leaves_real_runs_and_non_test_commands_alone() {
        assert!(!detect_empty_test_run("cargo test", "running 0 tests\n\nrunning 12 tests\ntest result: ok. 12 passed"));
        assert!(!detect_empty_test_run("bun test", " 34 pass\n 0 fail\n"));
        assert!(!detect_empty_test_run("go test ./...", "?   \texample.com/a\t[no test files]\nok  \texample.com/b\t0.01s\n"));
        assert!(!detect_empty_test_run("cargo check", "running 0 tests"));
        assert!(!detect_empty_test_run("bun run typecheck", "No tests found"));
        assert!(!detect_empty_test_run("attest run", "no tests ran"));
    }

    // "a heredoc body that mentions a test runner is not a verification"
    #[test]
    fn heredoc_bodies_do_not_make_a_write_a_verification() {
        let write = "mkdir -p docs && cat > docs/TOOLS.md <<'EOF'\n# Tools\nit detects bun test, vitest, pytest, cargo test\nEOF";
        assert_eq!(strip_heredoc_bodies(write), "mkdir -p docs && cat > docs/TOOLS.md <<'EOF'");
        let raw = serde_json::json!({ "command": write }).to_string();
        assert_eq!(extract_verification_command("BASH", &raw), None);
        let real = "cat > t.sh <<EOF\necho hi\nEOF\ncargo test";
        assert_eq!(extract_verification_command("BASH", &serde_json::json!({ "command": real }).to_string()).as_deref(), Some(real));
        assert_eq!(strip_heredoc_bodies("cargo test"), "cargo test");
    }

    #[test]
    fn extract_verification_command_matches_test_commands_and_rejects_others() {
        assert_eq!(
            extract_verification_command("BASH", r#"{"command":"cargo test"}"#),
            Some("cargo test".to_string())
        );
        assert_eq!(
            extract_verification_command("BASH", r#"{"command":"bun run test"}"#),
            Some("bun run test".to_string())
        );
        assert_eq!(
            extract_verification_command("BASH", r#"{"command":"bun run check"}"#),
            Some("bun run check".to_string())
        );
        assert_eq!(extract_verification_command("BASH", r#"{"command":"ls"}"#), None);
        // BASH_ASYNC never records verification.
        assert_eq!(
            extract_verification_command("BASH_ASYNC", r#"{"command":"cargo test"}"#),
            None
        );
        // VERIFY declares verification without pattern sniffing.
        assert_eq!(
            extract_verification_command("VERIFY", r#"{"command":"ls"}"#),
            Some("ls".to_string())
        );
        assert_eq!(extract_verification_command("READ", r#"{"command":"cargo test"}"#), None);
    }

    #[test]
    fn verification_history_cannot_evict_the_fact_that_a_task_edited_files() {
        let mut footprint = None;
        record_task_footprint(&mut footprint, "edited result.txt");
        for index in 0..MAX_FOOTPRINT_ENTRIES + 5 {
            record_task_footprint(&mut footprint, &format!("ran checker {index}"));
        }
        assert!(footprint.as_ref().unwrap().iter().any(|entry| entry.starts_with("edited ")));
        assert_eq!(footprint.unwrap().len(), MAX_FOOTPRINT_ENTRIES);
    }

    /// An "external" anchor that names a file this run edited is downgraded
    /// to self-authored with the reason; other anchors pass through.
    #[test]
    fn external_anchor_is_downgraded_when_the_check_names_an_edited_file() {
        use crate::core::types::VerificationAnchorKind;
        let mut edited = Vec::new();
        record_edited_path(&mut edited, "./tests/test_totals.py");
        record_edited_path(&mut edited, "tests/test_totals.py");
        record_edited_path(&mut edited, "src/lib.rs");
        assert_eq!(edited, vec!["tests/test_totals.py".to_string(), "src/lib.rs".to_string()]);

        let downgraded = declared_verification_anchor(
            r#"{"command":"pytest tests/test_totals.py","anchor":{"kind":"external","source":"project suite"}}"#,
            &edited,
        )
        .expect("anchor declared");
        assert_eq!(downgraded.kind, VerificationAnchorKind::SelfAuthored);
        assert!(downgraded.downgraded_reason.as_deref().unwrap_or_default().contains("tests/test_totals.py"));
        assert_eq!(downgraded.source.as_deref(), Some("project suite"));

        let by_basename = declared_verification_anchor(
            r#"{"command":"pytest -k totals","anchor":{"kind":"external","source":"test_totals.py fixture"}}"#,
            &edited,
        )
        .expect("anchor declared");
        assert_eq!(by_basename.kind, VerificationAnchorKind::SelfAuthored);

        // Runner forms name the test by stem; a directory that merely
        // contains an edited file is not a match.
        assert!(command_names_path("cargo test --test test_totals", "tests/test_totals.rs"));
        assert!(command_names_path("pytest ./tests/test_totals.py::test_sum", "tests/test_totals.py") || command_names_path("pytest tests/test_totals.py", "tests/test_totals.py"));
        assert!(!command_names_path("pytest tests/", "tests/test_totals.py"));
        assert!(!command_names_path("cargo test --lib", "src/lib.rs"));

        let mut shell_edited = Vec::new();
        for target in extract_shell_write_targets("cat > tests/test_shell.py <<'EOF'\nassert 1\nEOF\n && sed -i 's/a/b/' src/a.rs src/b.rs; echo ok | tee -a notes.txt >/dev/null; printf x 2>err.log") {
            record_edited_path(&mut shell_edited, &target);
        }
        assert_eq!(shell_edited, vec!["tests/test_shell.py", "src/a.rs", "src/b.rs", "notes.txt", "err.log"].into_iter().map(String::from).collect::<Vec<_>>());

        let external = declared_verification_anchor(
            r#"{"command":"pytest tests/test_invariants.py","anchor":{"kind":"external","source":"pre-existing suite"}}"#,
            &edited,
        )
        .expect("anchor declared");
        assert_eq!(external.kind, VerificationAnchorKind::External);
        assert_eq!(external.downgraded_reason, None);

        let declared_self = declared_verification_anchor(
            r#"{"command":"python check.py","anchor":{"kind":"self"}}"#,
            &edited,
        )
        .expect("anchor declared");
        assert_eq!(declared_self.kind, VerificationAnchorKind::SelfAuthored);
        assert_eq!(declared_self.source, None);

        assert_eq!(declared_verification_anchor(r#"{"command":"cargo test"}"#, &edited), None);
    }

    #[test]
    fn record_task_footprint_caps_at_max_entries_and_skips_duplicates() {
        let mut footprint: Option<Vec<String>> = None;

        record_task_footprint(&mut footprint, "edited src/a.rs");
        record_task_footprint(&mut footprint, "edited src/a.rs");
        assert_eq!(footprint.as_ref().unwrap().len(), 1);

        for index in 0..25 {
            record_task_footprint(&mut footprint, &format!("edited src/file-{}.rs", index));
        }

        let list = footprint.as_ref().unwrap();
        assert_eq!(list.len(), MAX_FOOTPRINT_ENTRIES);
        assert_eq!(list.last().unwrap(), "edited src/file-24.rs");
        assert!(!list.contains(&"edited src/a.rs".to_string()));
    }

    #[test]
    fn repeated_read_stub_matches_the_ts_text() {
        assert_eq!(
            build_repeated_read_stub("READ", 1),
            "[harness] READ: identical call #2 this run and the output is unchanged — it is verbatim in this conversation above (an earlier READ result). Use that copy; re-reading adds nothing. Act on it, or record the finding with observe/remember/finish_task."
        );
        assert!(DEDUPED_READ_ONLY_TOOLS.contains(&"GREP") && !DEDUPED_READ_ONLY_TOOLS.contains(&"BASH"));
    }

    #[test]
    fn hash_text_is_stable_hex() {
        assert_eq!(hash_text("cargo build failed"), hash_text("cargo build failed"));
        assert_ne!(hash_text("cargo build"), hash_text("cargo build failed"));
        assert_eq!(hash_text(""), "1505");
        let digest = hash_text("boom");
        assert!(digest.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }
}
// ---------------------------------------------------------------------------
// Run driver: the solid-state harness run loop.
//
// The run hoists closure-captured state into `HarnessRun`
// (run-scoped: options, config, state, usage, model caller) and `LoopScope`
// (loop-scoped locals: transcript, budgets, digest, progress flags). Each
// logical step of the loop becomes a method on those scopes.
// ---------------------------------------------------------------------------

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use indexmap::IndexMap;

use crate::core::types::{
    HarnessActivationDigest, HarnessLoopConfig, HarnessOperatorMessage,
    HarnessState, HarnessTask, HarnessTelemetryConfig, HarnessUsageByTask,
};
use crate::harness::harness_tools::{
    apply_harness_op, is_harness_tool, parse_harness_op_with_gate, HarnessRoleGate, RepoMemoryConfig,
};
use crate::harness::telemetry::{
    canonicalize_tool_input, record_tool_telemetry, tool_telemetry_key, truncate_text_keeping_ends,
};
use crate::core::types::{HarnessVerificationRecord, HarnessVerificationStreak};
use crate::harness::model_call::{
    AbortSignal, ModelCallRecord, ModelCaller, ModelRoute, SleepFn,
};
use crate::harness::roles::{HarnessRoleBindings, HarnessRoleRuntime};
use crate::harness::transport::OpenAICompatibleRequestTool;
use crate::harness::prompt::{
    build_cycle_continuation_message, build_iteration_messages, CycleContinuationArgs,
    HarnessLoopInfo, HarnessLoopRole, HarnessRunBudget, IterationMessagesArgs,
};
use crate::tools::types::{ChatToolDefinition, ChatToolRuntimeServices};

/// Rate-limit backoff defaults live in model_call; the loop only reads
/// the defaults below.
pub const DEFAULT_MAX_REVIEW_ROUNDS: i64 = 2;

pub fn default_loop_config() -> HarnessLoopConfig {
    crate::core::types::DEFAULT_LOOP_CONFIG.clone()
}

pub fn default_telemetry_config() -> HarnessTelemetryConfig {
    crate::core::types::DEFAULT_TELEMETRY_CONFIG.clone()
}

/// A partial telemetry override applied on top of `options.telemetry` defaults.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PartialHarnessTelemetryConfig {
    pub base_ttl: Option<i64>,
    pub max_observations: Option<i64>,
    pub max_observation_ttl: Option<i64>,
    pub max_promoted_entries: Option<i64>,
    pub max_promoted_output_chars: Option<i64>,
    pub max_ttl: Option<i64>,
    pub observation_base_ttl: Option<i64>,
    pub promote_threshold: Option<i64>,
    pub recency_window: Option<i64>,
    pub telemetry_retention: Option<i64>,
}

/// One operator inbox entry as `collectOperatorMessages` returns it
/// (`string | { at?: string | null; text: string }`).
/// Operator phrases that opt a run out of the review/verification lane.
/// One constant so CLI docs, tests, and harness detection share the exact
/// list; matched case-insensitively as substrings of the goal or a delivered
/// operator (--send/--enqueue) message.
pub const REVIEW_OPT_OUT_PHRASES: [&str; 7] = [
    "no review task",
    "do not create a review task",
    "don't create a review task",
    "no reviewer",
    "no verify",
    "do not run verify",
    "don't run verify",
];

/// Case-insensitive detection of the operator opt-out phrases. Returns the
/// first matched phrase in list order. Ordinary review requests like
/// "review the design" never match.
pub fn detect_review_opt_out(text: &str) -> Option<String> {
    let lowered = text.to_lowercase();
    REVIEW_OPT_OUT_PHRASES
        .iter()
        .find(|phrase| lowered.contains(&phrase.to_lowercase()))
        .map(|phrase| phrase.to_string())
}

/// Sticky opt-out decision for a piece of operator text: (opted out, matched
/// phrase). Explicit --no-review/--lite runs carry no invented match.
pub fn review_opt_out_from(text: &str) -> (bool, Option<String>) {
    match detect_review_opt_out(text) {
        Some(phrase) => (true, Some(phrase)),
        None => (false, None),
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct OperatorInboxEntry {
    pub at: Option<String>,
    pub text: String,
}

/// Clamp a role override between a floor and the run-level cap.
pub type NowFn = Arc<dyn Fn() -> chrono::DateTime<chrono::Utc> + Send + Sync>;
pub type EmitFn = Arc<dyn Fn(HarnessEvent) + Send + Sync>;

/// Options for constructing a `SolidStateHarness`. Every optional field is
/// an `Option`; function-typed fields are trait objects.
#[derive(Default)]
pub struct SolidStateHarnessOptions {
    pub cwd: Option<String>,
    pub dynamic_tool_names: Option<Vec<String>>,
    pub goal: String,
    pub goal_context: Option<String>,
    pub goal_images: Option<Vec<String>>,
    pub headers: Vec<(String, String)>,
    pub hooks: crate::harness::hooks::HooksConfig,
    pub initial_state: Option<HarnessState>,
    pub r#loop: Option<crate::harness::roles::PartialHarnessLoopConfig>,
    pub max_iterations: Option<i64>,
    pub plan_only: bool,
    pub max_review_rounds: Option<i64>,
    pub max_task_reopens: Option<i64>,
    pub max_tool_rounds_per_iteration: Option<i64>,
    pub model: Option<String>,
    pub now: Option<NowFn>,
    pub on_event: Option<EmitFn>,
    pub provider: Option<String>,
    pub prompt_cache_key: Option<String>,
    pub reasoning_effort: Option<String>,
    pub request_timeout_ms: Option<u64>,
    pub refresh_headers: Option<Arc<dyn Fn() -> Result<Vec<(String, String)>, String> + Send + Sync>>,
    pub repo_memory: Option<RepoMemoryConfig>,
    pub repo_memory_index: Option<String>,
    pub role_bindings: Option<HarnessRoleBindings>,
    pub roles: Option<Vec<HarnessRoleRuntime>>,
    pub signal: Option<AbortSignal>,
    pub sleep_impl: Option<SleepFn>,
    /// Session answers.jsonl path for ask_user surveys (None = no ask_user).
    pub answers_path: Option<PathBuf>,
    pub stall_limit: Option<i64>,
    pub state_path: Option<PathBuf>,
    pub summarize_run: Option<bool>,
    /// Draft mode (--lite): single author lane; terminal reason "draft",
    /// no end-of-run summary, and (with no_review) no completion-anchor gate.
    pub lite: bool,
    /// Operator review/verify opt-out (--no-review, --lite, or a detected
    /// opt-out phrase): the verified_by reviewer chain is skipped and a
    /// missing/self-authored completion anchor does not block finish_task.
    pub no_review: bool,
    /// `collectRunFacts`: ground-truth workspace facts (e.g. git status) for the run summary.
    pub collect_run_facts: Option<Box<dyn FnMut() -> Option<String> + Send>>,
    pub initial_inbox_cursor: Option<i64>,
    /// `collectOperatorMessages(consumedCount)`: steering that arrived while the run executes.
    pub collect_operator_messages: Option<Box<dyn FnMut(i64) -> Vec<OperatorInboxEntry> + Send>>,
    /// Opt-in `--ask` clarification surveys: when false, the ask_user spec is
    /// not sent to the model and calls to it register as failed tool calls.
    pub ask_user_enabled: bool,
    /// `--ask-timeout <seconds>`: how long ask_user waits for answers.jsonl
    /// before persisting the pending survey and ending with awaiting-input
    /// (default DEFAULT_ASK_USER_TIMEOUT_SECONDS = 900).
    pub ask_user_timeout_seconds: Option<i64>,
    pub system_prompt: Option<String>,
    pub telemetry: Option<PartialHarnessTelemetryConfig>,
    pub redact_secrets: Vec<(String, String)>,
    pub tool_route: Option<ModelRoute>,
    pub fallback_route: Option<ModelRoute>,
    pub tool_services: Option<ChatToolRuntimeServices>,
    pub tools: Vec<ChatToolDefinition>,
    pub url: Option<String>,
}

/// Bridge for the model caller's `onUsage` / `onRetryWait` / `getIteration`
/// hooks: `Arc<dyn Fn>`s cannot borrow `HarnessRun`, so they post into
/// this inbox and the loop drains it right after every call_model.
#[derive(Default)]
pub struct UsageInbox {
    pub usages: Mutex<Vec<(Option<crate::harness::model_call::OpenAICompatibleResponseUsage>, ModelCallRecord)>>,
    pub retry_waits: Mutex<Vec<f64>>,
    pub iteration: std::sync::atomic::AtomicI64,
}

/// Run-scoped state: options, config, state, usage, and the model caller.
pub struct HarnessRun {
    pub usage_inbox: Arc<UsageInbox>,
    pub options: SolidStateHarnessOptions,
    pub now: NowFn,
    pub emit_fn: EmitFn,
    pub run_started_at_ms: i64,
    /// Stamped when a stop was requested.
    pub abort_requested_at_ms: Option<i64>,
    pub run_usage: HarnessRunUsage,
    pub redact: Arc<dyn Fn(&str) -> String + Send + Sync>,
    pub url: String,
    pub model: String,
    pub cwd: String,
    /// `Number.POSITIVE_INFINITY` when unset → `i64::MAX`.
    pub max_iterations: i64,
    pub loop_config: HarnessLoopConfig,
    pub stall_limit: i64,
    pub max_task_reopens: i64,
    pub system_prompt: String,
    pub telemetry_config: HarnessTelemetryConfig,
    pub dynamic_tool_names: HashSet<String>,
    /// `tools` in insertion order; `tool_registry` maps name → index (Map<name, tool>).
    pub tools: Vec<ChatToolDefinition>,
    pub tool_registry: HashMap<String, usize>,
    pub role_map: IndexMap<String, HarnessRoleRuntime>,
    pub role_gate: Option<HarnessRoleGate>,
    pub harness_tool_specs: Vec<OpenAICompatibleRequestTool>,
    pub default_transport_tools: Vec<OpenAICompatibleRequestTool>,
    pub tool_services: ChatToolRuntimeServices,
    pub state: HarnessState,
    pub start_iteration: i64,
    pub start_loop: i64,
    pub call_model: ModelCaller,
    // Outer-loop locals.
    pub idle_loops: i64,
    pub escalations_without_progress: i64,
    pub run_futile: bool,
    pub ask_user_awaiting: bool,
    pub answers_path: Option<PathBuf>,
    pub continue_command: Option<String>,
    pub aborted: bool,
    pub run_error: Option<String>,
    pub plan_stopped: bool,
    /// The hot tail of the previous loop when it left its task unfinished —
    /// replayed into the next loop for the same task (see extract_loop_carryover).
    pub carryover: Option<LoopCarryover>,
    /// Workspace tool calls this run, by tool name — the run summary cites
    /// them so it cannot claim a delegation or a tool the run never used.
    pub run_tool_usage: std::collections::BTreeMap<String, u64>,
}

/// Loop-scoped locals: one instance per task loop.
pub struct LoopScope {
    pub loop_start_iteration: i64,
    pub current_task_id: Option<String>,
    pub role: Option<HarnessRoleRuntime>,
    /// Indexes into `HarnessRun::tools` this loop may execute (role-filtered).
    pub loop_tool_indexes: Vec<usize>,
    pub loop_transport_tools: Vec<OpenAICompatibleRequestTool>,
    pub loop_system_prompt: String,
    pub loop_budget: HarnessLoopConfig,
    pub transport_messages: Vec<TransportRequestMessage>,
    pub folded_message_indexes: HashSet<usize>,
    /// Where each read-only call's verbatim result sits in this loop's transcript
    /// (telemetry key → message index + content hash), so an identical repeat can
    /// point at it instead of sending the content again — only while that
    /// message is still unfolded.
    pub hot_read_only_results: HashMap<String, (usize, String)>,
    pub used_tool_call_ids: HashSet<String>,
    pub affordable_cycles: i64,
    /// The cycle currently running (1-based; 0 before the first begins).
    pub cycle: i64,
    pub task_finished: bool,
    pub made_progress: bool,
    /// Successful READ/GREP/DIR calls so far this loop, and whether anything
    /// has been written or recorded — the read-only nudge's inputs.
    pub read_only_calls_this_loop: i64,
    pub persisted_this_loop: bool,
    pub verification_stuck_this_loop: bool,
    pub narration_nudge_used: bool,
    pub truncation_nudge_used: bool,
    pub tool_calls_this_loop: i64,
    pub overflow_retried_this_loop: bool,
    pub concluded_naturally: bool,
    pub planned_and_yielded: bool,
    pub cycles_run: i64,
    pub digest_actions: Vec<String>,
}

/// Outcome of one cycle's tool-round loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoundOutcome {
    /// Keep going with the next round.
    Continue,
    /// End the round loop early (natural conclusion, task finished, overflow).
    Break,
    /// `aborted = true; break`.
    Aborted,
}

/// A tool call after id de-duplication.
pub struct NormalizedCall {
    pub call_id: String,
    pub normalized: crate::harness::transport::OpenAICompatibleToolCall,
    pub raw_input: String,
    pub tool_name: String,
}

/// roles::ModelRoute (the serializable roles.json shape) → the model
/// caller's route (which can also carry a refresh_headers closure).
pub fn model_route_from_role_route(route: &crate::harness::roles::ModelRoute) -> ModelRoute {
    ModelRoute {
        fallback_route: route
            .fallback_route
            .as_ref()
            .map(|inner| Box::new(model_route_from_role_route(inner))),
        headers: route
            .headers
            .as_ref()
            .map(|headers| headers.iter().map(|(k, v)| (k.clone(), v.clone())).collect()),
        model: route.model.clone(),
        provider: route.provider.clone(),
        reasoning_effort: route.reasoning_effort.clone(),
        refresh_headers: None,
        url: route.url.clone(),
    }
}

/// Digits with comma thousands separators (en-US locale formatting).
pub fn format_thousands(value: usize) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Result of executing one workspace tool.
pub struct WorkspaceToolExecution {
    /// False when a hook or role restriction prevented tool dispatch.
    pub dispatched: bool,
    pub failed: bool,
    pub tool_content: String,
}

/// Wrap the caller-supplied workspace tools as transport specs.
fn build_transport_tools(tools: &[ChatToolDefinition]) -> Vec<OpenAICompatibleRequestTool> {
    tools
        .iter()
        .map(|tool| {
            crate::harness::transport::create_request_tool(
                &tool.name,
                &tool.description,
                serde_json::to_value(&tool.parameters).unwrap_or(serde_json::json!({})),
            )
        })
        .collect()
}

/// The harness (framework) tool specs. Starts from `harness_tool_definitions()`
/// JSON converted with create_request_tool; when roles are configured,
/// plan_tasks gains the `role` enum property (order dependsOn, role, title).
fn build_harness_tool_specs(
    role_names: &[String],
    ask_user_enabled: bool,
) -> Vec<OpenAICompatibleRequestTool> {
    let mut specs: Vec<OpenAICompatibleRequestTool> =
        crate::harness::harness_tools::harness_tool_definitions()
            .iter()
            // The opt-in ask_user survey tool is only offered to the model
            // when the run enables it (--ask); every other spec is unchanged.
            .filter(|definition| {
                ask_user_enabled
                    || definition["function"]["name"].as_str() != Some("ask_user")
            })
            .map(|definition| {
                let function = &definition["function"];
                let name = function["name"].as_str().unwrap_or_default();
                let description = function["description"].as_str().unwrap_or_default();
                let parameters = function
                    .get("parameters")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({}));
                crate::harness::transport::create_request_tool(name, description, parameters)
            })
            .collect();

    if !role_names.is_empty() {
        let role_property = serde_json::json!({
            "description": format!(
                "Role whose subagent works this task. Available roles: {}.",
                role_names.join(", ")
            ),
            "enum": role_names,
            "type": "string"
        });
        for spec in specs.iter_mut() {
            if spec.function.name != "plan_tasks" {
                continue;
            }
            let object_variant = spec
                .function
                .parameters
                .get_mut("properties")
                .and_then(|properties| properties.get_mut("tasks"))
                .and_then(|tasks| tasks.get_mut("items"))
                .and_then(|items| items.get_mut("anyOf"))
                .and_then(|any_of| any_of.as_array_mut())
                .and_then(|variants| {
                    variants.iter_mut().find(|variant| {
                        variant.get("type").and_then(|value| value.as_str()) == Some("object")
                    })
                });
            if let Some(object_variant) = object_variant {
                if let Some(properties) = object_variant
                    .get_mut("properties")
                    .and_then(|value| value.as_object_mut())
                {
                    // Re-insert title after role so the property order is
                    // dependsOn, role, title.
                    let title = properties.remove("title");
                    properties.insert("role".to_string(), role_property.clone());
                    if let Some(title) = title {
                        properties.insert("title".to_string(), title);
                    }
                }
            }
        }
    }

    specs
}

/// A zero/negative budget would spin the outer run loop without ever
/// calling the model or advancing the iteration counter. i64 is always
/// finite, so only the floor applies; `fallback` is unused and kept for
/// call-site symmetry with the other clamps.
fn clamp_loop_value(value: i64, minimum: i64, fallback: i64) -> i64 {
    let _ = fallback;
    value.max(minimum)
}

impl HarnessRun {
    /// Resolve options into run-scoped config, load or create the state,
    /// build the role map / harness tool specs / transport tools, and create
    /// the model caller.
    pub async fn new(mut options: SolidStateHarnessOptions) -> Result<HarnessRun, String> {
        let now: NowFn = options.now.clone().unwrap_or_else(|| Arc::new(chrono::Utc::now));
        let emit_fn: EmitFn = options.on_event.clone().unwrap_or_else(|| Arc::new(|_event| {}));
        let run_started_at_ms = now().timestamp_millis();

        // Providers return usage on every completion; discarding it left drivers
        // blind to what a run cost. Accumulated per run and per task, surfaced on
        // the result (and from there the persisted run record / result line).
        let run_usage = HarnessRunUsage {
            by_task: IndexMap::new(),
            cache_creation_tokens: Some(0),
            cache_read_tokens: Some(0),
            calls: 0,
            completion_tokens: 0,
            prompt_tokens: 0,
            rate_limit_wait_seconds: 0.0,
            retries: 0,
            wall_ms: 0,
        };
        let redactor = crate::tools::command_policy::build_redactor(options.redact_secrets.clone());
        let redact: Arc<dyn Fn(&str) -> String + Send + Sync> = Arc::new(move |text| redactor(text));

        let url = options
            .url
            .clone()
            .unwrap_or_else(|| crate::harness::transport::DEFAULT_CHAT_COMPLETIONS_URL.to_string());
        let model = options
            .model
            .clone()
            .unwrap_or_else(|| crate::harness::transport::DEFAULT_CHAT_MODEL.to_string());
        let cwd = options.cwd.clone().unwrap_or_else(|| {
            std::env::current_dir()
                .map(|path| path.to_string_lossy().to_string())
                .unwrap_or_else(|_| ".".to_string())
        });
        let max_iterations = options.max_iterations.unwrap_or(i64::MAX);

        // DEFAULT_LOOP_CONFIG overlaid with options.loop; maxToolRoundsPerCycle
        // falls back to maxToolRoundsPerIteration when the loop block does not
        // set it.
        let defaults = default_loop_config();
        let overrides = options.r#loop.unwrap_or_default();
        let mut loop_config = HarnessLoopConfig {
            hot_tool_results: overrides.hot_tool_results.unwrap_or(defaults.hot_tool_results),
            max_cycles: overrides.max_cycles.unwrap_or(defaults.max_cycles),
            max_tool_result_chars: overrides
                .max_tool_result_chars
                .unwrap_or(defaults.max_tool_result_chars),
            max_tool_rounds_per_cycle: overrides
                .max_tool_rounds_per_cycle
                .unwrap_or(defaults.max_tool_rounds_per_cycle),
        };
        if let (Some(rounds), None) = (options.max_tool_rounds_per_iteration, overrides.max_tool_rounds_per_cycle) {
            loop_config.max_tool_rounds_per_cycle = rounds;
        }
        // A zero, negative, or NaN cycle/round budget would spin the outer run loop
        // without ever calling the model or advancing the iteration counter.
        loop_config.max_cycles = clamp_loop_value(loop_config.max_cycles, 1, defaults.max_cycles);
        loop_config.max_tool_rounds_per_cycle =
            clamp_loop_value(loop_config.max_tool_rounds_per_cycle, 1, defaults.max_tool_rounds_per_cycle);
        loop_config.hot_tool_results = clamp_loop_value(loop_config.hot_tool_results, 0, defaults.hot_tool_results);
        loop_config.max_tool_result_chars =
            clamp_loop_value(loop_config.max_tool_result_chars, 1, defaults.max_tool_result_chars);

        let stall_limit = options.stall_limit.unwrap_or(3);
        let max_task_reopens = options.max_task_reopens.unwrap_or(2);
        let mut system_prompt = options
            .system_prompt
            .clone()
            .unwrap_or_else(|| crate::harness::prompt::DEFAULT_HARNESS_SYSTEM_PROMPT.to_string());
        // Opt-in clarification guidance: appended ONLY when ask_user is
        // enabled, so the disabled-path system prompt stays byte-identical.
        if options.ask_user_enabled {
            system_prompt.push_str("\n\n");
            system_prompt.push_str(crate::harness::prompt::ASK_USER_GUIDANCE_FRAGMENT);
        }

        let telemetry_defaults = default_telemetry_config();
        let telemetry_overrides = options.telemetry.unwrap_or_default();
        let telemetry_config = HarnessTelemetryConfig {
            base_ttl: telemetry_overrides.base_ttl.unwrap_or(telemetry_defaults.base_ttl),
            max_observations: telemetry_overrides
                .max_observations
                .unwrap_or(telemetry_defaults.max_observations),
            max_observation_ttl: telemetry_overrides
                .max_observation_ttl
                .unwrap_or(telemetry_defaults.max_observation_ttl),
            max_promoted_entries: telemetry_overrides
                .max_promoted_entries
                .unwrap_or(telemetry_defaults.max_promoted_entries),
            max_promoted_output_chars: telemetry_overrides
                .max_promoted_output_chars
                .unwrap_or(telemetry_defaults.max_promoted_output_chars),
            max_ttl: telemetry_overrides.max_ttl.unwrap_or(telemetry_defaults.max_ttl),
            observation_base_ttl: telemetry_overrides
                .observation_base_ttl
                .unwrap_or(telemetry_defaults.observation_base_ttl),
            promote_threshold: telemetry_overrides
                .promote_threshold
                .unwrap_or(telemetry_defaults.promote_threshold),
            recency_window: telemetry_overrides
                .recency_window
                .unwrap_or(telemetry_defaults.recency_window),
            telemetry_retention: telemetry_overrides
                .telemetry_retention
                .unwrap_or(telemetry_defaults.telemetry_retention),
        };

        let dynamic_tool_names: HashSet<String> = options
            .dynamic_tool_names
            .clone()
            .unwrap_or_else(|| DEFAULT_DYNAMIC_TOOL_NAMES.iter().map(|name| name.to_string()).collect())
            .into_iter()
            .collect();

        let tools = std::mem::take(&mut options.tools);
        let tool_registry: HashMap<String, usize> = tools
            .iter()
            .enumerate()
            .map(|(index, tool)| (tool.name.clone(), index))
            .collect();

        let role_map = crate::harness::roles::build_role_map(options.roles.as_deref());
        let role_gate = if role_map.is_empty() {
            None
        } else {
            Some(HarnessRoleGate {
                roles: role_map
                    .iter()
                    .map(|(name, role)| {
                        (
                            name.clone(),
                            crate::harness::harness_tools::HarnessRoleSpec { verified_by: role.verified_by.clone(), blind: role.blind },
                        )
                    })
                    .collect(),
                max_review_rounds: Some(
                    options.max_review_rounds.unwrap_or(DEFAULT_MAX_REVIEW_ROUNDS).max(0) as u32,
                ),
                default_task_role: options.role_bindings.as_ref().and_then(|bindings| bindings.task.clone()),
            })
        };
        let role_names: Vec<String> = role_map.keys().cloned().collect();
        let harness_tool_specs =
            build_harness_tool_specs(&role_names, options.ask_user_enabled);
        let mut default_transport_tools = build_transport_tools(&tools);
        default_transport_tools.extend(harness_tool_specs.iter().cloned());

        let mut headers = options.headers.clone();
        if !headers.iter().any(|(key, _)| key.eq_ignore_ascii_case("content-type")) {
            headers.insert(0, ("content-type".to_string(), "application/json".to_string()));
        }

        // `options.toolServices ?? createChatToolRuntimeServices({ cwd })`:
        // the CLI runs without a web server, so the harness owns the async-job
        // runtime for the run.
        let tool_services = match options.tool_services.take() {
            Some(services) => services,
            None => crate::tools::async_jobs::create_chat_tool_runtime_services(
                crate::tools::async_jobs::CreateChatToolRuntimeServicesOptions {
                    cwd: Some(std::path::PathBuf::from(&cwd)),
                    jobs_root: None,
                },
            ),
        };

        let mut state = match options.initial_state.take() {
            Some(state) => state,
            None => match options.state_path.as_ref() {
                Some(state_path) => core_state::load_harness_state(state_path)
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| core_state::create_harness_state(&options.goal)),
                None => core_state::create_harness_state(&options.goal),
            },
        };
        if state.inbox_cursor.is_none() {
            if let Some(cursor) = options.initial_inbox_cursor {
                state.inbox_cursor = Some(cursor);
            }
        }
        // options.initialState bypasses loadHarnessState's migrations; the Rust
        // loop field is always numeric, so nothing needs repair.
        let start_iteration = state.iteration;
        let start_loop = state.r#loop;

        let usage_inbox = Arc::new(UsageInbox::default());
        usage_inbox
            .iteration
            .store(state.iteration, std::sync::atomic::Ordering::Relaxed);
        let iteration_cell = usage_inbox.clone();
        let usage_sink = usage_inbox.clone();
        let retry_sink = usage_inbox.clone();
        let call_model = crate::harness::model_call::create_model_caller(
            crate::harness::model_call::ModelCallerDeps {
                cwd: Some(cwd.clone()),
                default_transport_tools: default_transport_tools.clone(),
                emit: emit_fn.clone(),
                fallback_route: options.fallback_route.clone(),
                get_iteration: Arc::new(move || {
                    iteration_cell.iteration.load(std::sync::atomic::Ordering::Relaxed)
                }),
                headers,
                model: model.clone(),
                on_retry_wait: Arc::new(move |wait_seconds| {
                    retry_sink.retry_waits.lock().unwrap().push(wait_seconds);
                }),
                on_usage: Arc::new(move |response, record| {
                    usage_sink.usages.lock().unwrap().push((response.usage.clone(), record));
                }),
                provider: options.provider.clone(),
                refresh_headers: options.refresh_headers.clone(),
                prompt_cache_key: options.prompt_cache_key.clone(),
                reasoning_effort: options.reasoning_effort.clone(),
                request_timeout_ms: options.request_timeout_ms,
                signal: options.signal.clone(),
                sleep_impl: options.sleep_impl.clone(),
                tool_route: options.tool_route.clone(),
                url: url.clone(),
                http_client: None,
            },
        );

        let run_answers_path = options.answers_path.clone();
        Ok(HarnessRun {
            usage_inbox,
            options,
            now,
            emit_fn,
            run_started_at_ms,
            abort_requested_at_ms: None,
            run_usage,
            redact,
            url,
            model,
            cwd,
            max_iterations,
            loop_config,
            stall_limit,
            max_task_reopens,
            system_prompt,
            telemetry_config,
            dynamic_tool_names,
            tools,
            tool_registry,
            role_map,
            role_gate,
            harness_tool_specs,
            default_transport_tools,
            tool_services,
            state,
            start_iteration,
            start_loop,
            call_model,
            idle_loops: 0,
            escalations_without_progress: 0,
            run_futile: false,
            ask_user_awaiting: false,
            answers_path: run_answers_path,
            continue_command: None,
            aborted: false,
            run_error: None,
            plan_stopped: false,
            carryover: None,
            run_tool_usage: std::collections::BTreeMap::new(),
        })
    }

    pub fn stop_latency_ms(&self) -> Option<i64> {
        self.abort_requested_at_ms
            .map(|requested| ((self.now)().timestamp_millis() - requested).max(0))
    }

    pub fn finalize_usage(&mut self) -> HarnessRunUsage {
        self.run_usage.wall_ms = ((self.now)().timestamp_millis() - self.run_started_at_ms).max(0);
        self.run_usage.clone()
    }

    /// accumulate usage and emit the `inference` event.
    pub fn record_model_usage(
        &mut self,
        usage: Option<&crate::harness::model_call::OpenAICompatibleResponseUsage>,
        call: &ModelCallRecord,
    ) {
        // Treat an empty string like an absent value.
        let task_id = call.task_id.clone().filter(|task_id| !task_id.is_empty());
        let provider = call.provider.clone().filter(|provider| !provider.is_empty());

        let prompt_tokens = usage.and_then(|usage| usage.prompt_tokens).unwrap_or(0);
        let completion_tokens = usage.and_then(|usage| usage.completion_tokens).unwrap_or(0);
        // Anthropic native reports explicit cache writes/reads; OpenAI-compatible
        // providers with automatic caching (OpenAI, Cerebras, xAI, Gemini) report
        // hits under prompt_tokens_details.cached_tokens.
        let cache_creation_tokens = usage
            .and_then(|usage| usage.cache_creation_input_tokens)
            .unwrap_or(0);
        let cache_read_tokens = usage
            .and_then(|usage| usage.cache_read_input_tokens)
            .or_else(|| {
                usage
                    .and_then(|usage| usage.prompt_tokens_details.as_ref())
                    .and_then(|details| details.cached_tokens)
            })
            .unwrap_or(0);

        self.run_usage.calls += 1;
        self.run_usage.prompt_tokens += prompt_tokens;
        self.run_usage.completion_tokens += completion_tokens;
        self.run_usage.cache_creation_tokens = Some(
            self.run_usage.cache_creation_tokens.unwrap_or(0) + cache_creation_tokens,
        );
        self.run_usage.cache_read_tokens = Some(
            self.run_usage.cache_read_tokens.unwrap_or(0) + cache_read_tokens,
        );

        if let Some(task_id) = task_id.as_deref() {
            let bucket = self
                .run_usage
                .by_task
                .entry(task_id.to_string())
                .or_insert(HarnessUsageByTask {
                    calls: 0,
                    completion_tokens: 0,
                    prompt_tokens: 0,
                });

            bucket.calls += 1;
            bucket.prompt_tokens += prompt_tokens;
            bucket.completion_tokens += completion_tokens;
        }

        // Run totals answer "what did this cost"; they cannot answer "when did the
        // cache go cold" or "how did the context grow". Emitting one event per call
        // puts the same four numbers on the timeline, which is what a tracing UI
        // needs to plot context growth and per-turn cache hit rate.
        self.emit(HarnessEvent {
            data: Some(HarnessEventData {
                cache_creation_tokens: Some(cache_creation_tokens),
                cache_read_tokens: Some(cache_read_tokens),
                completion_tokens: Some(completion_tokens),
                latency_ms: Some(call.latency_ms),
                model: Some(call.model.clone()),
                prompt_tokens: Some(prompt_tokens),
                provider,
                task_id,
                ..Default::default()
            }),
            detail: format!(
                "{} — {} prompt ({} cached, {} written), {} completion in {}ms",
                call.model,
                prompt_tokens,
                cache_read_tokens,
                cache_creation_tokens,
                completion_tokens,
                call.latency_ms
            ),
            iteration: self.state.iteration,
            r#type: HarnessEventType::Inference,
        });
    }

    pub fn record_retry_wait(&mut self, wait_seconds: f64) {
        self.run_usage.retries += 1;
        self.run_usage.rate_limit_wait_seconds += wait_seconds;
    }

    /// Drain the caller's usage/retry hooks posted during the last call_model.
    pub fn drain_usage_inbox(&mut self) {
        let usages: Vec<_> = std::mem::take(&mut *self.usage_inbox.usages.lock().unwrap());
        for (usage, record) in usages {
            self.record_model_usage(usage.as_ref(), &record);
        }
        let waits: Vec<f64> = std::mem::take(&mut *self.usage_inbox.retry_waits.lock().unwrap());
        for wait_seconds in waits {
            self.record_retry_wait(wait_seconds);
        }
    }

    /// Mirror state.iteration into the caller's getIteration hook.
    pub fn sync_iteration_cell(&self) {
        self.usage_inbox
            .iteration
            .store(self.state.iteration, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn emit(&self, event: HarnessEvent) {
        (self.emit_fn)(event)
    }

    /// persist the state when a state path is configured.
    pub fn persist(&self) {
        if let Some(state_path) = &self.options.state_path {
            let _ = crate::core::state::save_harness_state(state_path, &self.state);
        }
    }

    /// Directory-side answers.jsonl path for ask_user surveys: an explicit
    /// option when set, otherwise next to the run's state.json.
    fn answers_path(&self) -> Option<PathBuf> {
        if let Some(path) = &self.answers_path {
            return Some(path.clone());
        }
        self.options
            .state_path
            .as_ref()
            .map(|state_path| state_path.with_file_name("answers.jsonl"))
    }

    /// `drip --resume <id>` target: the persisted session directory name when
    /// derivable, else the state path itself.
    fn resume_target(&self) -> Option<String> {
        self.options
            .state_path
            .as_ref()
            .and_then(|state_path| state_path.parent())
            .and_then(|dir| dir.file_name())
            .map(|name| name.to_string_lossy().to_string())
            .or_else(|| {
                self.options
                    .state_path
                    .as_ref()
                    .map(|state_path| state_path.display().to_string())
            })
    }

    /// Run end while an ask_user survey is still pending: persist the survey
    /// and finish cleanly with reason "awaiting-input" so `drip --resume`
    /// picks the session (and its answers.jsonl cursor) back up.
    fn awaiting_input_result(&mut self) -> HarnessRunResult {
        self.continue_command = self
            .resume_target()
            .map(|target| format!("drip --resume {target}"));
        self.persist();
        HarnessRunResult {
            continue_command: self.continue_command.clone(),
            error_message: None,
            iterations: self.state.iteration - self.start_iteration,
            r#loops: self.state.r#loop - self.start_loop,
            leaked_jobs: None,
            reason: HarnessRunReason::AwaitingInput,
            state: self.state.clone(),
            usage: self.finalize_usage(),
            stop_latency_ms: self.stop_latency_ms(),
        }
    }

    /// Question-event emission shared by live dispatch and resume re-emission.
    fn emit_question_event(&mut self, survey: &crate::core::types::QuestionSurvey) {
        self.emit(crate::core::types::HarnessEvent {
            data: Some(HarnessEventData {
                r#loop: Some(self.state.r#loop),
                question_survey: Some(survey.clone()),
                ..Default::default()
            }),
            detail: format!(
                "ask_user: {} question(s) pending operator answers",
                survey.questions.len()
            ),
            iteration: self.state.iteration,
            r#type: HarnessEventType::Question,
        });
    }

    /// Block after a question event until a complete matching answer batch
    /// arrives in answers.jsonl (~500 ms poll), the run aborts, or the
    /// ask_user timeout expires. On timeout the pending survey is preserved
    /// and the run is flagged to end with reason "awaiting-input".
    async fn run_survey_block(
        &mut self,
        survey: crate::core::types::QuestionSurvey,
    ) -> SurveyWait {
        let Some(answers_path) = self.answers_path() else {
            self.ask_user_awaiting = true;
            return SurveyWait::Aborted;
        };
        // A boundary right before the wait marks every earlier line stale for
        // this survey; persisting the cursor keeps a timeout+resume from
        // replaying pre-boundary records.
        let boundary_cursor = crate::core::state::answers::append_boundary(&answers_path)
            .ok()
            .and_then(|()| crate::core::state::answers::line_count(&answers_path).ok());
        if let Some(boundary_cursor) = boundary_cursor {
            if let Some(pending) = self.state.pending_questions.as_mut() {
                if pending.answers_cursor.is_none() {
                    pending.answers_cursor = Some(boundary_cursor);
                }
            }
            self.persist();
        }
        let mut cursor = self
            .state
            .pending_questions
            .as_ref()
            .and_then(|pending| pending.answers_cursor)
            .or(boundary_cursor)
            // Boundary append failed (IO): a fresh line count still fences off
            // earlier surveys' lines — never fall back to replaying from 0.
            .or_else(|| crate::core::state::answers::line_count(&answers_path).ok())
            .unwrap_or(0);
        let timeout_seconds = self
            .options
            .ask_user_timeout_seconds
            .unwrap_or(crate::harness::harness_tools::DEFAULT_ASK_USER_TIMEOUT_SECONDS)
            .max(0) as u64;
        let deadline_ms = (self.now)().timestamp_millis() + (timeout_seconds as i64) * 1000;
        loop {
            if self
                .options
                .signal
                .as_ref()
                .is_some_and(|signal| signal.is_aborted())
            {
                return SurveyWait::Aborted;
            }
            if let Some(answers) = poll_survey_answers(&answers_path, &survey, &mut cursor) {
                if let Some(pending) = self.state.pending_questions.as_mut() {
                    pending.answers_cursor = Some(cursor);
                }
                self.accept_survey_answers(survey, answers);
                return SurveyWait::Answered;
            }
            if (self.now)().timestamp_millis() >= deadline_ms {
                // Keep the pending survey (with its cursor) for --resume.
                if let Some(pending) = self.state.pending_questions.as_mut() {
                    pending.answers_cursor = Some(cursor);
                }
                self.ask_user_awaiting = true;
                self.persist();
                return SurveyWait::TimedOut;
            }
            crate::harness::model_call::sleep_unless_aborted(500, self.options.signal.as_ref())
                .await;
        }
    }

    /// Resume path: a persisted pending survey is processed before the next
    /// model/implementation step — consume an already-matching answer batch
    /// from answers.jsonl, or re-emit the question event and block again.
    async fn resume_pending_survey(&mut self) {
        let Some(survey) = self.state.pending_questions.clone() else {
            return;
        };
        let Some(answers_path) = self.answers_path() else {
            self.ask_user_awaiting = true;
            return;
        };
        // A missing cursor means the survey never reached its boundary append
        // (crash or IO failure before run_survey_block persisted it), so there
        // is no fence to respect: scan from 0 and let validate_survey_answers
        // filter stale batches — fencing at end-of-file here would silently
        // skip an answer that legitimately arrived while the run was down.
        let mut cursor = survey.answers_cursor.unwrap_or(0);
        // A batch that arrived while the run was down is consumed first; the
        // question event is only re-emitted when we actually have to wait.
        for entry in crate::core::state::answers::read_answer_batches_after(&answers_path, cursor) {
            cursor = cursor.max(entry.line_number + 1);
            if crate::harness::harness_tools::validate_survey_answers(&survey, &entry.record)
                .is_ok()
            {
                if let Some(pending) = self.state.pending_questions.as_mut() {
                    pending.answers_cursor = Some(cursor);
                }
                self.accept_survey_answers(survey, entry.record);
                return;
            }
            // Rejected batches advance the persisted cursor too, so the next
            // resume does not re-read (and re-log) the same dead lines.
            if let Some(pending) = self.state.pending_questions.as_mut() {
                pending.answers_cursor = Some(cursor);
            }
            self.persist();
        }
        self.emit_question_event(&survey);
        self.run_survey_block(survey).await;
    }

    /// Record accepted answers: clear the pending survey, inject the rendered
    /// Q->A summary as an operator message (the plan-revision directive is
    /// part of the rendered text), persist, and emit the completed harness-op
    /// event.
    fn accept_survey_answers(
        &mut self,
        survey: crate::core::types::QuestionSurvey,
        answers: crate::core::types::HarnessSurveyAnswers,
    ) {
        self.state.pending_questions = None;
        let mut operator_messages = self.state.operator_messages.take().unwrap_or_default();
        operator_messages.push(HarnessOperatorMessage {
            // Iteration disambiguates two accepts stamped in the same instant.
            id: format!("ask-user-{}-{}", self.state.iteration, answers.at),
            received_at_iteration: self.state.iteration,
            text: render_survey_answers(&survey, &answers),
        });
        self.state.operator_messages = Some(operator_messages);
        self.persist();
        self.emit(crate::core::types::HarnessEvent {
            data: Some(HarnessEventData {
                survey_answers: Some(answers.clone()),
                tool_name: Some("ask_user".to_string()),
                ..Default::default()
            }),
            detail: format!("ask_user: {} answer(s) received", answers.answers.len()),
            iteration: self.state.iteration,
            r#type: HarnessEventType::HarnessOp,
        });
    }

    pub fn aborted_result(&mut self) -> HarnessRunResult {
        self.persist();
        HarnessRunResult {
            continue_command: None,
            error_message: None,
            iterations: self.state.iteration - self.start_iteration,
            r#loops: self.state.r#loop - self.start_loop,
            leaked_jobs: None,
            reason: HarnessRunReason::Aborted,
            state: self.state.clone(),
            usage: self.finalize_usage(),
            stop_latency_ms: self.stop_latency_ms(),
        }
    }

    /// run one workspace tool through the tool framework
    /// (`execute_tool_call`), redacting the model-facing text.
    pub fn execute_workspace_tool(
        &mut self,
        call_id: &str,
        raw_input: &str,
        registry: Option<&[usize]>,
        tool_name: &str,
    ) -> WorkspaceToolExecution {
        use crate::chat::types::{
            ChatMessageBlock, ChatRuntimeContext, ToolCallStatus, WorkingFileContext,
            WorkingFileScope,
        };
        use crate::tools::execute::{execute_tool_call, ToolExecutionContext};

        let iteration_message = crate::chat::types::ChatMessage {
            blocks: vec![ChatMessageBlock::Text(crate::chat::types::TextBlock {
                context_state: None,
                tags: None,
                text: self.state.goal.clone(),
            })],
            context_files: None,
            context_state: None,
            created_at: Some(
                (self.now)().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            ),
            failed: None,
            id: format!("harness-activation-{}", self.state.iteration),
            pending: None,
            reply_to_message_id: None,
            role: crate::chat::types::ChatRole::User,
            tags: None,
            transport_state: None,
        };
        // Resolve the tool by name, then treat it as absent when this loop's role
        // restricts tools and the resolved index is not in the allowed list —
        // a disallowed call fails cleanly inside `execute_tool_call`.
        let tool = self
            .tool_registry
            .get(tool_name)
            .copied()
            .filter(|index| registry.map_or(true, |allowed| allowed.contains(index)))
            .and_then(|index| self.tools.get(index));

        // Privacy: hook payloads go to user-configured commands over stdin,
        // so redact the tool input before it leaves the process.
        let redacted_input = (self.redact)(raw_input);
        // Claude Code semantics: a pre_tool_use hook that exits 2 vetoes the
        // tool call — the tool never runs and the hook's stderr goes back to
        // the model as the tool result so it can adapt.
        let pre_tool_use_outcomes = self.fire_hook_checked(
            crate::harness::hooks::HookEvent::PreToolUse,
            Some((tool_name, redacted_input.as_str())),
        );
        if let Some(veto) = pre_tool_use_outcomes
            .iter()
            .find(|outcome| outcome.exit_code == Some(2))
        {
            let stderr_excerpt = if veto.stderr_excerpt.is_empty() {
                "(no stderr output)".to_string()
            } else {
                veto.stderr_excerpt.clone()
            };
            return WorkspaceToolExecution {
                dispatched: false,
                failed: true,
                tool_content: format!(
                    "tool call blocked by pre_tool_use hook: {stderr_excerpt}"
                ),
            };
        }
        let dispatched = tool.is_some();
        let executed = execute_tool_call(ToolExecutionContext {
            call_id,
            history: &[],
            message: &iteration_message,
            raw_input,
            runtime_context: ChatRuntimeContext {
                cwd: self.cwd.clone(),
                // No file is open: the default no-file state.
                working_file: WorkingFileContext {
                    exists: false,
                    path: String::new(),
                    scope: WorkingFileScope::Cwd,
                    text: None,
                },
            },
            services: self.tool_services.clone(),
            tool,
            tool_name,
        });

        let failed = executed
            .blocks
            .iter()
            .any(|block| matches!(block, ChatMessageBlock::ToolCall(block) if block.status == ToolCallStatus::Failed));
        // The one choke point every consumer shares: context injection,
        // telemetry, transcript events, and the NDJSON stream all read this.
        let tool_content = (self.redact)(&executed.tool_content);
        self.fire_hook(crate::harness::hooks::HookEvent::PostToolUse, Some((tool_name, &tool_content)));
        // drip-specific: memory-bank writes (remember/forget) get their own
        // event, but harness ops dispatch in dispatch_tool_calls and never
        // reach execute_workspace_tool, so MemoryWrite fires there — gated
        // on apply_harness_op's state_changed so failures and no-ops stay
        // silent.
        WorkspaceToolExecution {
            dispatched,
            failed,
            tool_content,
        }
    }

    /// re-run dynamic warm-context entries at loop start.
    pub fn refresh_dynamic_entries(&mut self) {
        // Collect first (the entries borrow `self.state`), then execute each
        // dynamic entry whose tool is registered, then write the refreshed
        // output back and emit; a failed refresh keeps the stale output.
        let pending: Vec<(usize, String, String, String, String)> = self
            .state
            .promoted_context
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                if !entry.dynamic || !self.tool_registry.contains_key(&entry.tool_name) {
                    None
                } else {
                    Some((
                        index,
                        entry.key.clone(),
                        entry.raw_input.clone(),
                        entry.tool_name.clone(),
                        entry.input_preview.clone(),
                    ))
                }
            })
            .collect();

        for (index, key, raw_input, tool_name, input_preview) in pending {
            let execution = self.execute_workspace_tool(
                &format!("refresh-{}-{}", self.state.iteration, key),
                &raw_input,
                None,
                &tool_name,
            );
            if execution.failed {
                continue;
            }
            let max_chars = self.telemetry_config.max_promoted_output_chars.max(0) as usize;
            if let Some(entry) = self.state.promoted_context.get_mut(index) {
                entry.output = truncate_text(&execution.tool_content, max_chars);
            }
            self.emit(HarnessEvent {
                data: None,
                detail: format!("{tool_name} {input_preview}"),
                iteration: self.state.iteration + 1,
                r#type: HarnessEventType::ContextRefreshed,
            });
        }
    }

    /// Fire user-configured hooks for `event`. A hook failure or timeout
    /// never blocks the run: it is only surfaced as a RunWarning event.
    /// Deliberately blocking (bounded by the configured timeout) so the sync
    /// tool-dispatch sites can call it. The one exception is the PreToolUse
    /// exit-2 veto in [`Self::execute_workspace_tool`], which consumes the
    /// outcomes returned by [`Self::fire_hook_checked`].
    fn fire_hook(&self, event: crate::harness::hooks::HookEvent, tool: Option<(&str, &str)>) {
        self.fire_hook_checked(event, tool);
    }

    /// Like [`Self::fire_hook`], but returns every hook's outcome so the
    /// caller can act on it. Failures still surface as RunWarning events
    /// here; the caller only needs to look for the veto exit code (2).
    fn fire_hook_checked(
        &self,
        event: crate::harness::hooks::HookEvent,
        tool: Option<(&str, &str)>,
    ) -> Vec<crate::harness::hooks::HookOutcome> {
        let commands = self.options.hooks.commands_for(event, tool.map(|(name, _)| name));
        if commands.is_empty() {
            return Vec::new();
        }
        let timeout = self.options.hooks.timeout();
        let timestamp = (self.now)().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        commands
            .iter()
            .map(|command| {
                let payload =
                    crate::harness::hooks::build_hook_payload(event, &self.cwd, tool, &timestamp);
                let outcome =
                    crate::harness::hooks::run_hook_command(command, &self.cwd, &payload, timeout);
                if !outcome.succeeded() {
                    self.emit(HarnessEvent {
                        data: None,
                        detail: format!("hook {}: {}", event.as_str(), outcome.describe()),
                        iteration: self.state.iteration,
                        r#type: HarnessEventType::RunWarning,
                    });
                }
                outcome
            })
            .collect()
    }

    /// the outer `while` loop: one task loop per iteration
    /// of this method's loop, delegating to `begin_loop` / `run_cycle` /
    /// `end_loop`. Returns Some(result) when the run ends inside the loop
    /// (abort/error paths); None when the outer loop exits normally.
    pub async fn run_loops(&mut self) -> Option<HarnessRunResult> {
        self.fire_hook(crate::harness::hooks::HookEvent::SessionStart, None);
        // Resume: a pending survey is processed before the next model step.
        self.resume_pending_survey().await;
        if self.ask_user_awaiting {
            return Some(self.awaiting_input_result());
        }
        while self.state.iteration - self.start_iteration < self.max_iterations {
            if self.signal_aborted() {
                self.aborted = true;
                break;
            }

            if core_state::is_goal_complete(&self.state) {
                break;
            }

            // One task loop: a subagent takes the current task (or planning duty) and
            // works it in short cycles that share this loop's transcript. Between
            // loops nothing survives but the shared state store, so every loop starts
            // with a fresh, bounded context window.
            self.state.r#loop += 1;

            let has_current = core_state::get_current_task(&self.state).is_some();

            // --plan: the decomposition IS the deliverable. Stop before the first
            // task loop would start (and before the task is marked picked up); the
            // persisted plan executes on resume.
            if self.options.plan_only && has_current {
                self.plan_stopped = true;
                self.state.r#loop -= 1;
                break;
            }

            let mut scope = self.begin_loop();
            self.fire_hook(crate::harness::hooks::HookEvent::LoopStart, None);
            self.fire_hook(crate::harness::hooks::HookEvent::TaskStart, None);

            for cycle in 1..=scope.loop_budget.max_cycles {
                if scope.task_finished || scope.concluded_naturally || self.aborted {
                    break;
                }

                if !self.begin_cycle(&mut scope, cycle) {
                    break;
                }

                for round in 0..scope.loop_budget.max_tool_rounds_per_cycle {
                    if scope.task_finished {
                        break;
                    }

                    match self.run_round(&mut scope, cycle, round).await {
                        RoundOutcome::Continue => {}
                        RoundOutcome::Break => break,
                        RoundOutcome::Aborted => {
                            self.aborted = true;
                            break;
                        }
                    }

                    if self.run_error.is_some() {
                        break;
                    }
                }

                if self.run_error.is_some() || self.aborted || self.ask_user_awaiting {
                    break;
                }

                self.persist();

                if scope.current_task_id.is_none()
                    && self.state.tasks.iter().any(|task| {
                        matches!(
                            task.status,
                            HarnessTaskStatus::Pending | HarnessTaskStatus::InProgress
                        )
                    })
                {
                    scope.planned_and_yielded = true;
                    break;
                }
            }

            // A thrown run error aborts the
            // run (with an aborted result when the stop was requested) and
            // otherwise surfaces as a run warning before the run ends.
            if let Some(message) = self.run_error.clone() {
                if self.signal_aborted() {
                    self.record_loop_digest(&mut scope, "the run was stopped mid-loop");
                    return Some(self.aborted_result());
                }

                self.record_loop_digest(&mut scope, &format!("the loop failed: {message}"));
                self.persist();
                self.emit(HarnessEvent {
                    data: None,
                    detail: format!("run error: {message}"),
                    iteration: self.state.iteration,
                    r#type: HarnessEventType::RunWarning,
                });
                break;
            }

            self.end_loop(&mut scope);
            self.fire_hook(crate::harness::hooks::HookEvent::LoopFinish, None);
            self.fire_hook(crate::harness::hooks::HookEvent::TaskFinish, None);

            if self.ask_user_awaiting {
                return Some(self.awaiting_input_result());
            }
            if self.aborted {
                return Some(self.aborted_result());
            }

            self.after_loop(&scope);

            if self.run_futile {
                break;
            }
        }

        if self.ask_user_awaiting {
            return Some(self.awaiting_input_result());
        }

        None
    }

    /// Abort listener: the first time a stop is
    /// observed, stamp `abort_requested_at_ms`.
    fn signal_aborted(&mut self) -> bool {
        if self
            .options
            .signal
            .as_ref()
            .is_some_and(|signal| signal.is_aborted())
        {
            if self.abort_requested_at_ms.is_none() {
                self.abort_requested_at_ms = Some((self.now)().timestamp_millis());
            }
            true
        } else {
            false
        }
    }

    /// pick up the current task, resolve the loop role,
    /// derive the loop budget, emit `loop-start`, refresh dynamic entries and
    /// build the loop scope.
    pub fn begin_loop(&mut self) -> LoopScope {
        // pending → in_progress; activations counts pickups.
        let current_task_id = core_state::get_current_task(&self.state)
            .map(|task| task.id.clone());
        if let Some(task_id) = current_task_id.as_deref() {
            if let Some(task) = core_state::get_task_by_id_mut(&mut self.state, task_id) {
                if task.status == HarnessTaskStatus::Pending {
                    task.status = HarnessTaskStatus::InProgress;
                }
                task.activations = Some(task.activations.unwrap_or(0) + 1);
            }
        }

        // this loop's capability profile.
        let current_task = current_task_id
            .as_deref()
            .and_then(|id| {
                self.state.tasks.iter().find(|task| task.id == id)
            });
        let role = crate::harness::roles::resolve_loop_role(
            &self.role_map,
            current_task,
            self.options.role_bindings.as_ref(),
        );
        // filterToolsForRole.
        let loop_tool_indexes: Vec<usize> = match role
            .as_ref()
            .and_then(|r| r.tool_names.as_ref())
        {
            None => (0..self.tools.len()).collect(),
            Some(tool_names) => self
                .tools
                .iter()
                .enumerate()
                .filter(|(_, tool)| tool_names.contains(&tool.name))
                .map(|(index, _)| index)
                .collect(),
        };
        // transport tools.
        let loop_transport_tools: Vec<OpenAICompatibleRequestTool> =
            if role.as_ref().and_then(|r| r.tool_names.as_ref()).is_some() {
                let mut tools = self
                    .tools
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| loop_tool_indexes.contains(index))
                    .map(|(_, tool)| {
                        crate::harness::transport::create_request_tool(
                            &tool.name,
                            &tool.description,
                            serde_json::to_value(&tool.parameters)
                                .unwrap_or(serde_json::json!({})),
                        )
                    })
                    .collect::<Vec<_>>();
                tools.extend(self.harness_tool_specs.iter().cloned());
                tools
            } else {
                self.default_transport_tools.clone()
            };
        let loop_system_prompt =
            crate::harness::roles::compose_role_system_prompt(&self.system_prompt, role.as_ref());
        // role loop budget clamps.
        let loop_budget = match role.as_ref().and_then(|r| r.r#loop.as_ref()) {
            Some(role_loop) => HarnessLoopConfig {
                hot_tool_results: clamp_loop_value(
                    role_loop.hot_tool_results.unwrap_or(self.loop_config.hot_tool_results),
                    0,
                    self.loop_config.hot_tool_results,
                ),
                max_cycles: clamp_loop_value(
                    role_loop.max_cycles.unwrap_or(self.loop_config.max_cycles),
                    1,
                    self.loop_config.max_cycles,
                ),
                max_tool_result_chars: clamp_loop_value(
                    role_loop
                        .max_tool_result_chars
                        .unwrap_or(self.loop_config.max_tool_result_chars),
                    1,
                    self.loop_config.max_tool_result_chars,
                ),
                max_tool_rounds_per_cycle: clamp_loop_value(
                    role_loop
                        .max_tool_rounds_per_cycle
                        .unwrap_or(self.loop_config.max_tool_rounds_per_cycle),
                    1,
                    self.loop_config.max_tool_rounds_per_cycle,
                ),
            },
            None => self.loop_config.clone(),
        };

        // the loop-start event.
        let detail = format!(
            "loop {}{} — {}",
            self.state.r#loop,
            role.as_ref()
                .map(|r| format!(" [role: {}]", r.name))
                .unwrap_or_default(),
            match current_task {
                Some(task) => format!("{}: {}", task.id, task.title),
                None => {
                    if self.state.tasks.is_empty() {
                        "planning".to_string()
                    } else {
                        "replanning blocked tasks".to_string()
                    }
                }
            }
        );
        self.emit(HarnessEvent {
            data: Some(HarnessEventData {
                r#loop: Some(self.state.r#loop),
                task_id: current_task_id.clone(),
                ..Default::default()
            }),
            detail,
            iteration: self.state.iteration + 1,
            r#type: HarnessEventType::LoopStart,
        });

        // refresh dynamic warm-context entries.
        self.refresh_dynamic_entries();

        // cycles this loop can actually afford.
        let affordable_cycles = if self.max_iterations != i64::MAX {
            std::cmp::max(
                1,
                std::cmp::min(
                    loop_budget.max_cycles,
                    self.max_iterations - (self.state.iteration - self.start_iteration),
                ),
            )
        } else {
            loop_budget.max_cycles
        };

        LoopScope {
            loop_start_iteration: self.state.iteration,
            current_task_id,
            role,
            loop_tool_indexes,
            loop_transport_tools,
            loop_system_prompt,
            loop_budget,
            transport_messages: Vec::new(),
            folded_message_indexes: HashSet::new(),
            hot_read_only_results: HashMap::new(),
            used_tool_call_ids: HashSet::new(),
            affordable_cycles,
            cycle: 0,
            task_finished: false,
            made_progress: false,
            read_only_calls_this_loop: 0,
            persisted_this_loop: false,
            verification_stuck_this_loop: false,
            narration_nudge_used: false,
            truncation_nudge_used: false,
            tool_calls_this_loop: 0,
            overflow_retried_this_loop: false,
            concluded_naturally: false,
            planned_and_yielded: false,
            cycles_run: 0,
            digest_actions: Vec::new(),
        }
    }

    /// `recordLoopDigest(outcome)`.
    pub fn record_loop_digest(&mut self, scope: &mut LoopScope, outcome: &str) {
        let current_task_id = scope.current_task_id.clone();
        let cycles_run = scope.cycles_run;
        let task_finished = scope.task_finished;

        // lastActivation with actions overflow handling.
        let overflow_count = scope
            .digest_actions
            .len()
            .saturating_sub(MAX_DIGEST_ACTIONS);
        let actions: Vec<String> = if overflow_count > 0 {
            let mut actions = vec![format!(
                "({} earlier action(s) omitted)",
                overflow_count
            )];
            actions.extend(
                scope.digest_actions[scope.digest_actions.len() - MAX_DIGEST_ACTIONS..]
                    .iter()
                    .cloned(),
            );
            actions
        } else {
            scope.digest_actions.clone()
        };

        self.state.last_activation = Some(HarnessActivationDigest {
            actions,
            cycles: Some(cycles_run),
            iteration: self.state.iteration,
            r#loop: Some(self.state.r#loop),
            outcome: outcome.to_string(),
            task_id: current_task_id.clone(),
        });

        // a task that keeps failing needs its earlier
        // attempts in view: append a compact attempt digest (newest three
        // attempts bounded).
        if let Some(task_id) = current_task_id.as_deref() {
            if !task_finished && !scope.digest_actions.is_empty() {
                let loop_number = self.state.r#loop;
                if let Some(task) = core_state::get_task_by_id_mut(&mut self.state, task_id) {
                    let attempt = format!(
                        "attempt (loop {}): {}; tried: {}",
                        loop_number,
                        outcome,
                        scope
                            .digest_actions
                            .iter()
                            .rev()
                            .take(3)
                            .rev()
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(" · ")
                    );
                    let prior_attempts: Vec<String> = task
                        .notes
                        .iter()
                        .filter(|note| note.starts_with("attempt (loop "))
                        .cloned()
                        .collect();
                    if prior_attempts.len() >= 3 {
                        let oldest = prior_attempts[0].clone();
                        task.notes.retain(|note| *note != oldest);
                    }
                    core_state::append_task_note(task, &truncate_text(&attempt, 600));
                }
            }
        }
    }

    /// cycle start: budget/abort checks, operator
    /// messages, `iteration-start`, transport messages for cycle 1 or the
    /// continuation message + fold for later cycles. Returns false when the
    /// cycle must not run (budget exhausted / aborted).
    pub fn begin_cycle(&mut self, scope: &mut LoopScope, cycle: i64) -> bool {
        scope.cycle = cycle;
        if self
            .options
            .signal
            .as_ref()
            .is_some_and(|signal| signal.is_aborted())
        {
            self.aborted = true;
            return false;
        }

        if self.state.iteration - self.start_iteration >= self.max_iterations {
            return false;
        }

        self.state.iteration += 1;
        self.sync_iteration_cell();
        scope.cycles_run = cycle;

        // Steering sent while the run executes lands at the next cycle
        // boundary: recorded on state (so every later prompt sees it),
        // emitted to the timeline, and — mid-loop — appended to the live
        // transcript so the current subagent reacts without waiting for a
        // fresh loop.
        let mut fresh_operator_messages: Vec<OperatorInboxEntry> = Vec::new();

        // Steering goes stale: after ~12 cycles of being acted on it stops
        // outranking everything else in the prompt.
        let stale_before_iteration = self.state.iteration - 12;
        let operator_messages_stale = self
            .state
            .operator_messages
            .as_ref()
            .is_some_and(|messages| {
                messages
                    .iter()
                    .any(|message| message.received_at_iteration < stale_before_iteration)
            });
        if operator_messages_stale {
            let kept: Vec<HarnessOperatorMessage> = self
                .state
                .operator_messages
                .take()
                .unwrap_or_default()
                .into_iter()
                .filter(|message| message.received_at_iteration >= stale_before_iteration)
                .collect();
            self.state.operator_messages = if kept.is_empty() { None } else { Some(kept) };
        }

        if self.options.collect_operator_messages.is_some() {
            // Steering lands here, at the cycle boundary; inbox trouble must
            // never kill a run.
            let collected: Vec<OperatorInboxEntry> = {
                let collect = self.options.collect_operator_messages.as_mut().unwrap();
                let consumed = self.state.inbox_cursor.unwrap_or(0);
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| collect(consumed)))
                    .unwrap_or_default()
            };
            fresh_operator_messages.extend(collected);

            for entry in &fresh_operator_messages {
                let next_cursor = self.state.inbox_cursor.unwrap_or(0) + 1;
                self.state.inbox_cursor = Some(next_cursor);

                if entry.text.trim().is_empty() {
                    continue;
                }

                let mut operator_messages = self.state.operator_messages.take().unwrap_or_default();
                if operator_messages.len() > 7 {
                    operator_messages.drain(..operator_messages.len() - 7);
                }
                operator_messages.push(HarnessOperatorMessage {
                    id: format!("op-{}", next_cursor),
                    received_at_iteration: self.state.iteration,
                    text: entry.text.clone(),
                });
                self.state.operator_messages = Some(operator_messages);

                // Steering adoption latency: --send stamps `at`; consumption
                // happens here, at a cycle boundary.
                let sent_at_ms = entry
                    .at
                    .as_deref()
                    .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
                    .map(|parsed| parsed.timestamp_millis());
                let latency_ms =
                    sent_at_ms.map(|sent_at| ((self.now)().timestamp_millis() - sent_at).max(0));

                let data = HarnessEventData {
                    latency_ms,
                    sent_at: match &entry.at {
                        Some(at) if !at.is_empty() => Some(at.clone()),
                        _ => None,
                    },
                    ..Default::default()
                };
                self.emit(HarnessEvent {
                    data: Some(data),
                    detail: truncate_text(&entry.text, 240),
                    iteration: self.state.iteration,
                    r#type: HarnessEventType::OperatorMessage,
                });
            }

            if !fresh_operator_messages.is_empty() {
                self.persist();
            }

            fresh_operator_messages.retain(|entry| !entry.text.trim().is_empty());
        }

        // Operator review/verify opt-out (sticky). Explicit --no-review/--lite
        // flips it without an invented matched phrase; a phrase in the goal or
        // in any delivered operator message flips it and emits ONE run-warning
        // naming the first matched phrase. Detection happens here, at the
        // cycle boundary, before the steering can drive planning or finishes;
        // state persistence makes the decision visible to every later cycle.
        if self.state.review_opt_out.is_none() && (self.options.lite || self.options.no_review) {
            self.state.review_opt_out = Some(true);
            self.persist();
        }
        if self.state.opt_out_warning_emitted.is_none() {
            let (opt_out, matched) = review_opt_out_from(&self.state.goal);
            if opt_out {
                self.state.review_opt_out = Some(true);
                self.state.opt_out_warning_emitted = matched.clone();
                self.emit(HarnessEvent {
                    data: Some(HarnessEventData {
                        reason: matched.clone(),
                        ..Default::default()
                    }),
                    detail: format!(
                        "operator review/verify opt-out detected (matched phrase: {:?}); review chain and completion anchor gate disabled for this run",
                        matched.unwrap_or_default()
                    ),
                    iteration: self.state.iteration,
                    r#type: HarnessEventType::RunWarning,
                });
                self.persist();
            }
        }
        for entry in &fresh_operator_messages {
            let (opt_out, matched) = review_opt_out_from(&entry.text);
            if opt_out {
                self.state.review_opt_out = Some(true);
                if self.state.opt_out_warning_emitted.is_none() {
                    self.state.opt_out_warning_emitted = matched.clone();
                    self.emit(HarnessEvent {
                        data: Some(HarnessEventData {
                            reason: matched.clone(),
                            ..Default::default()
                        }),
                        detail: format!(
                            "operator message opted the run out of review/verification (matched phrase: {:?})",
                            matched.unwrap_or_default()
                        ),
                        iteration: self.state.iteration,
                        r#type: HarnessEventType::RunWarning,
                    });
                }
            }
        }

        let used = self.state.iteration - self.start_iteration;
        // The transcript shows the run-level budget alongside the loop-local
        // cycle count — followers used to see "cycle 1/3" while the run died
        // on a global cap they could not see coming.
        let budget_suffix = if self.max_iterations != i64::MAX {
            if used >= self.max_iterations {
                format!(
                    " [run {}/{} — budget exhausted after this cycle]",
                    used, self.max_iterations
                )
            } else {
                format!(" [run {}/{}]", used, self.max_iterations)
            }
        } else {
            String::new()
        };

        let current_task: Option<&HarnessTask> = scope
            .current_task_id
            .as_deref()
            .and_then(|task_id| self.state.tasks.iter().find(|task| task.id == task_id));

        let (task_id, task_suffix) = match current_task {
            Some(task) => (
                Some(task.id.clone()),
                format!(" — {}: {}", task.id, task.title),
            ),
            None => (
                None,
                if self.state.tasks.is_empty() {
                    " — planning".to_string()
                } else {
                    " — replanning blocked tasks".to_string()
                },
            ),
        };

        self.emit(HarnessEvent {
            data: Some(HarnessEventData {
                cycle: Some(cycle),
                r#loop: Some(self.state.r#loop),
                task_id,
                ..Default::default()
            }),
            detail: format!(
                "cycle {}/{}{}{}",
                cycle, scope.loop_budget.max_cycles, task_suffix, budget_suffix
            ),
            iteration: self.state.iteration,
            r#type: HarnessEventType::IterationStart,
        });

        let run_budget = if self.max_iterations != i64::MAX {
            Some(HarnessRunBudget {
                total: self.max_iterations,
                used,
            })
        } else {
            None
        };

        if cycle == 1 {
            let goal_context = self.options.goal_context.clone();
            let goal_images = self.options.goal_images.clone();
            let repo_memory_index = self.options.repo_memory_index.clone();
            let repo_memory_dir = self
                .options
                .repo_memory
                .as_ref()
                .map(|memory| memory.memory_dir.clone());
            let role = scope.role.as_ref().map(|role| HarnessLoopRole {
                name: role.name.clone(),
                description: role.description.clone(),
            });
            let loop_system_prompt = scope.loop_system_prompt.clone();

            scope.transport_messages = build_iteration_messages(
                &self.state,
                &IterationMessagesArgs {
                    current_date: &(self.now)().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                    current_task,
                    goal_context: goal_context.as_deref(),
                    goal_images,
                    loop_info: Some(HarnessLoopInfo {
                        index: self.state.r#loop,
                        max_cycles: scope.affordable_cycles,
                        role,
                    }),
                    repo_memory_dir: repo_memory_dir.as_deref(),
                    repo_memory_index: repo_memory_index.as_deref(),
                    run_budget,
                    stall_limit: Some(self.stall_limit),
                    system_prompt: &loop_system_prompt,
                    workspace: Some(&self.cwd),
                },
            );

            let blind_role = scope.role.as_ref().filter(|role| role.blind).map(|role| role.name.clone());
            let carried = match (&self.carryover, &scope.current_task_id) {
                (Some(carryover), Some(_)) if carryover.r#loop == self.state.r#loop - 1 && !carryover.messages.is_empty() => {
                    Some(carryover.clone())
                }
                _ => None,
            };
            // A blind role starts from the goal and the artifact only: replaying
            // the author's exchanges would make two loops share one derivation.
            let carried = match (carried, blind_role) {
                (Some(carryover), Some(role_name)) => {
                    let exchanges = carryover.messages.iter().filter(|message| message.role == ChatRoleTag::Tool).count();
                    self.emit(HarnessEvent {
                        data: Some(HarnessEventData {
                            r#loop: Some(self.state.r#loop),
                            task_id: scope.current_task_id.clone(),
                            ..Default::default()
                        }),
                        detail: format!(
                            "blind {role_name}: withheld {exchanges} tool exchange(s) from loop {}; the reviewer sees the goal and the artifact, not the author's derivation",
                            carryover.r#loop
                        ),
                        iteration: self.state.iteration,
                        r#type: HarnessEventType::ContextWithheld,
                    });
                    None
                }
                (carried, _) => carried,
            };
            if let Some(carryover) = carried {
                let first_carried = scope.transport_messages.len();
                scope.transport_messages.extend(carryover.messages.iter().cloned());

                // Replayed read-only results are as good as this loop's own for
                // the repeat stub; replayed folds stay folded; replayed call ids
                // stay taken.
                for index in first_carried..scope.transport_messages.len() {
                    let message = scope.transport_messages[index].clone();
                    match message.role {
                        ChatRoleTag::Assistant => {
                            for call in message.tool_calls.iter().flatten() {
                                if let Some(id) = &call.id {
                                    scope.used_tool_call_ids.insert(id.clone());
                                }
                            }
                        }
                        ChatRoleTag::Tool => {
                            if let Some(TransportContent::Text(content)) = &message.content {
                                if content.starts_with(FOLDED_RESULT_MARKER) {
                                    scope.folded_message_indexes.insert(index);
                                } else if let Some(name) = message.name.as_deref().filter(|name| DEDUPED_READ_ONLY_TOOLS.contains(name)) {
                                    let arguments = scope.transport_messages[first_carried..index]
                                        .iter()
                                        .flat_map(|earlier| earlier.tool_calls.iter().flatten())
                                        .find(|candidate| candidate.id.is_some() && candidate.id == message.tool_call_id)
                                        .and_then(|call| call.function.as_ref().and_then(|function| function.arguments.clone()));
                                    if let Some(arguments) = arguments {
                                        scope
                                            .hot_read_only_results
                                            .insert(tool_telemetry_key(name, &arguments), (index, hash_text(content)));
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }

                let exchanges = carryover.messages.iter().filter(|message| message.role == ChatRoleTag::Assistant).count();
                let task_id = scope.current_task_id.clone().unwrap_or_default();
                scope.transport_messages.push(TransportRequestMessage {
                    content: Some(TransportContent::Text(build_carryover_note(&carryover, &task_id, exchanges))),
                    role: ChatRoleTag::User,
                    ..Default::default()
                });
                self.emit(HarnessEvent {
                    data: Some(HarnessEventData {
                        r#loop: Some(self.state.r#loop),
                        task_id: Some(task_id.clone()),
                        ..Default::default()
                    }),
                    detail: format!(
                        "replayed {exchanges} tool exchange(s) from loop {} into this loop for {task_id}",
                        carryover.r#loop
                    ),
                    iteration: self.state.iteration,
                    r#type: HarnessEventType::ContextRefreshed,
                });
            }

            self.carryover = None;
        } else {
            // Continuing the same loop: keep the shared transcript, fold its
            // cold tool results, and reorient with a small continuation
            // message instead of a fresh full snapshot.
            let folded_count = fold_cold_tool_results(
                &mut scope.transport_messages,
                scope.loop_budget.hot_tool_results.max(0) as usize,
                &mut scope.folded_message_indexes,
            );

            if folded_count > 0 {
                self.emit(HarnessEvent {
                    data: None,
                    detail: format!(
                        "folded {} cold tool result(s) in the loop transcript",
                        folded_count
                    ),
                    iteration: self.state.iteration,
                    r#type: HarnessEventType::ContextExpired,
                });
            }

            let continuation = build_cycle_continuation_message(
                &self.state,
                &CycleContinuationArgs {
                    current_task,
                    cycle,
                    max_cycles: scope.affordable_cycles,
                    run_budget,
                },
            );
            scope.transport_messages.push(TransportRequestMessage {
                content: Some(TransportContent::Text(continuation)),
                role: ChatRoleTag::User,
                ..Default::default()
            });

            if !fresh_operator_messages.is_empty() {
                let mut block = String::from(
                    "operator_messages (fresh instructions from the operator, just received — they take precedence over the original goal where they conflict; incorporate them now):",
                );
                for entry in &fresh_operator_messages {
                    block.push_str("\n- ");
                    block.push_str(&entry.text);
                }
                scope.transport_messages.push(TransportRequestMessage {
                    content: Some(TransportContent::Text(block)),
                    role: ChatRoleTag::User,
                    ..Default::default()
                });
            }
        }

        true
    }

    /// one tool round: fold on transcript size, call the
    /// model (with the context-overflow retry), then parse the reply: a
    /// text-only reply concludes (or is nudged once); tool calls are
    /// normalized and dispatched via `dispatch_tool_calls`.
    /// Fires `RelayStart` on entry and `RelayFinish` on every exit path.
    pub async fn run_round(&mut self, scope: &mut LoopScope, cycle: i64, round: i64) -> RoundOutcome {
        self.fire_hook(crate::harness::hooks::HookEvent::RelayStart, None);
        let outcome = self.run_round_inner(scope, cycle, round).await;
        self.fire_hook(crate::harness::hooks::HookEvent::RelayFinish, None);
        outcome
    }

    async fn run_round_inner(&mut self, scope: &mut LoopScope, _cycle: i64, round: i64) -> RoundOutcome {
        use crate::harness::model_call::ModelCallOptions;

        if self.options.signal.as_ref().map(AbortSignal::is_aborted) == Some(true) {
            self.aborted = true;
            return RoundOutcome::Aborted;
        }

        // Proactive budget: past this many characters the transcript is
        // folded wholesale BEFORE the endpoint has to reject it — small
        // local models never see the 400 at all.
        let approximate_chars = scope
            .transport_messages
            .iter()
            .map(|message| match &message.content {
                Some(TransportContent::Text(text)) => text.chars().count(),
                _ => 0,
            })
            .sum::<usize>();

        if approximate_chars > MAX_LOOP_TRANSCRIPT_CHARS {
            let folded_count =
                fold_cold_tool_results(&mut scope.transport_messages, 0, &mut scope.folded_message_indexes);

            if folded_count > 0 {
                self.emit(HarnessEvent {
                    data: None,
                    detail: format!(
                        "loop transcript reached ~{}k chars — folded {} tool result(s) to stay inside the context window",
                        (approximate_chars as f64 / 1000.0).round() as i64,
                        folded_count
                    ),
                    iteration: self.state.iteration,
                    r#type: HarnessEventType::RunWarning,
                });
            }
        }

        let task_id = scope.current_task_id.clone();

        // The retry call reuses the same options; build them once.
        let call_options = ModelCallOptions {
            include_tools: None,
            route: scope
                .role
                .as_ref()
                .and_then(|role| role.route.as_ref())
                .map(model_route_from_role_route),
            transport_tools: Some(scope.loop_transport_tools.clone()),
            usage_task_id: task_id.clone(),
        };

        let response = match self
            .call_model
            .call_model(scope.transport_messages.clone(), Some(call_options.clone()))
            .await
        {
            Ok(response) => response,
            Err(error) => {
                self.drain_usage_inbox();
                if !error.is_context_overflow() {
                    self.run_error = Some(error.message().to_string());
                    return RoundOutcome::Break;
                }

                // First overflow this loop: fold EVERY tool result to a digest
                // and retry once. A second overflow (or nothing to fold) ends
                // the LOOP, not the run — the next loop starts from a fresh,
                // bounded snapshot.
                let folded_count = fold_cold_tool_results(
                    &mut scope.transport_messages,
                    0,
                    &mut scope.folded_message_indexes,
                );

                if scope.overflow_retried_this_loop || folded_count == 0 {
                    self.emit(HarnessEvent {
                        data: None,
                        detail: format!(
                            "context window overflowed twice in loop {} — ending the loop early; shared state is intact and the next loop starts fresh",
                            self.state.r#loop
                        ),
                        iteration: self.state.iteration,
                        r#type: HarnessEventType::RunWarning,
                    });
                    scope
                        .digest_actions
                        .push("context overflow ended this loop early".to_string());
                    scope.concluded_naturally = true;
                    return RoundOutcome::Break;
                }

                scope.overflow_retried_this_loop = true;
                self.emit(HarnessEvent {
                    data: None,
                    detail: format!(
                        "context window overflowed — folded {} tool result(s) to digests and retrying",
                        folded_count
                    ),
                    iteration: self.state.iteration,
                    r#type: HarnessEventType::RunWarning,
                });

                match self
                    .call_model
                    .call_model(scope.transport_messages.clone(), Some(call_options))
                    .await
                {
                    Ok(response) => response,
                    Err(retry_error) => {
                        if !retry_error.is_context_overflow() {
                            self.run_error = Some(retry_error.message().to_string());
                            return RoundOutcome::Break;
                        }

                        self.emit(HarnessEvent {
                            data: None,
                            detail: format!(
                                "context window still overflows after folding — ending loop {} early; the next loop starts fresh",
                                self.state.r#loop
                            ),
                            iteration: self.state.iteration,
                            r#type: HarnessEventType::RunWarning,
                        });
                        scope
                            .digest_actions
                            .push("context overflow ended this loop early".to_string());
                        scope.concluded_naturally = true;
                        return RoundOutcome::Break;
                    }
                }
            }
        };

        self.drain_usage_inbox();

        let response_message = response
            .choices
            .as_ref()
            .and_then(|choices| choices.first())
            .and_then(|choice| choice.message.as_ref());
        let tool_calls = response_message
            .and_then(|message| message.tool_calls.as_ref())
            .cloned()
            .unwrap_or_default();
        let response_text = extract_response_text(
            response_message
                .and_then(|message| message.content.as_ref())
                .unwrap_or(&Value::Null),
        );

        let truncated = response
            .choices
            .as_ref()
            .and_then(|choices| choices.first())
            .and_then(|choice| choice.finish_reason.as_deref())
            == Some("length");

        if tool_calls.is_empty() && truncated {
            // The warning lands even when no round is left to nudge into, so
            // the transcript says why the loop concluded on cut-off text.
            let can_nudge = !scope.truncation_nudge_used
                && round < scope.loop_budget.max_tool_rounds_per_cycle - 1;
            let completion_tokens = response
                .usage
                .as_ref()
                .and_then(|usage| usage.completion_tokens)
                .map(|tokens| tokens.to_string())
                .unwrap_or_else(|| "?".to_string());

            self.emit(HarnessEvent {
                data: None,
                detail: format!(
                    "reply cut off at the provider's output-token cap ({completion_tokens} completion tokens) before any tool call{}",
                    if can_nudge { " — asking for a shorter turn" } else { "" }
                ),
                iteration: self.state.iteration,
                r#type: HarnessEventType::RunWarning,
            });

            if can_nudge {
                scope.truncation_nudge_used = true;
                scope.transport_messages.push(TransportRequestMessage {
                    anthropic_content: response_message
                        .and_then(|message| message.anthropic_content.clone()),
                    content: Some(TransportContent::Text(if response_text.trim().is_empty() {
                        "(reply truncated at the output-token cap)".to_string()
                    } else {
                        response_text.clone()
                    })),
                    role: ChatRoleTag::Assistant,
                    ..Default::default()
                });
                scope.transport_messages.push(TransportRequestMessage {
                    content: Some(TransportContent::Text(TRUNCATION_NUDGE_MESSAGE.to_string())),
                    role: ChatRoleTag::User,
                    ..Default::default()
                });
                scope
                    .digest_actions
                    .push("reply truncated at the output cap (nudged to say less and act)".to_string());
                return RoundOutcome::Continue;
            }
        }

        if tool_calls.is_empty() {
            // A narration-only first reply with an unfinished task gets ONE
            // corrective push and the round loop continues; a second
            // text-only reply concludes the loop as before.
            let trimmed = response_text.trim().to_string();
            if !scope.narration_nudge_used
                && scope.tool_calls_this_loop == 0
                && scope.current_task_id.is_some()
                && !scope.task_finished
                && !trimmed.is_empty()
                && round < scope.loop_budget.max_tool_rounds_per_cycle - 1
            {
                scope.narration_nudge_used = true;
                scope.transport_messages.push(TransportRequestMessage {
                    anthropic_content: response_message
                        .and_then(|message| message.anthropic_content.clone()),
                    content: Some(TransportContent::Text(response_text.clone())),
                    role: ChatRoleTag::Assistant,
                    ..Default::default()
                });
                scope
                    .transport_messages
                    .push(TransportRequestMessage {
                        content: Some(TransportContent::Text(
                            NARRATION_NUDGE_MESSAGE.to_string(),
                        )),
                        role: ChatRoleTag::User,
                        ..Default::default()
                    });

                if let Some(task) = scope
                    .current_task_id
                    .as_deref()
                    .and_then(|id| core_state::get_task_by_id_mut(&mut self.state, id))
                {
                    core_state::append_task_note(task, &trimmed);
                }

                scope.digest_actions.push(format!(
                    "said (nudged to act): {}",
                    truncate_text(&trimmed, MAX_DIGEST_ACTION_CHARS)
                ));
                return RoundOutcome::Continue;
            }

            // A text-only reply concludes the whole loop, not just the cycle:
            // the model considers its unit of work done.
            scope.concluded_naturally = true;

            if !trimmed.is_empty() && scope.current_task_id.is_some() {
                if let Some(task) = scope
                    .current_task_id
                    .as_deref()
                    .and_then(|id| core_state::get_task_by_id_mut(&mut self.state, id))
                {
                    core_state::append_task_note(task, &trimmed);
                }
                scope.made_progress = true;
            }

            if !trimmed.is_empty() {
                scope.digest_actions.push(format!(
                    "said: {}",
                    truncate_text(&trimmed, MAX_DIGEST_ACTION_CHARS)
                ));
                self.emit(HarnessEvent {
                    data: None,
                    detail: trimmed,
                    iteration: self.state.iteration,
                    r#type: HarnessEventType::ModelText,
                });
            }

            // Deliberate non-feature: a bare first-cycle text answer is NOT
            // auto-coerced into a respond — narration from weak models
            // ("I will now build X") would complete build goals with the
            // narration as the result. Question goals go through the respond
            // op, which capable models call per the system prompt.
            return RoundOutcome::Break;
        }

        scope.tool_calls_this_loop += tool_calls.len() as i64;

        let normalized_calls = self.normalize_tool_calls(scope, round, &tool_calls);

        // replay the assistant turn. Native Anthropic
        // content blocks are only replayed when every tool-call id survived
        // de-duplication (a renamed id would not match its tool_use block).
        let native_ids_preserved = response_message
            .and_then(|message| message.anthropic_content.as_ref())
            .is_some()
            && normalized_calls
                .iter()
                .enumerate()
                .all(|(index, entry)| tool_calls.get(index).and_then(|call| call.id.as_deref()) == Some(entry.call_id.as_str()));
        scope.transport_messages.push(TransportRequestMessage {
            anthropic_content: if native_ids_preserved {
                response_message.and_then(|message| message.anthropic_content.clone())
            } else {
                None
            },
            content: if response_text.is_empty() {
                None
            } else {
                Some(TransportContent::Text(response_text.clone()))
            },
            role: ChatRoleTag::Assistant,
            tool_calls: Some(normalized_calls.iter().map(|entry| entry.normalized.clone()).collect()),
            ..Default::default()
        });

        self.dispatch_tool_calls(scope, normalized_calls).await;
        if self.ask_user_awaiting {
            return RoundOutcome::Break;
        }

        RoundOutcome::Continue
    }

    /// de-duplicate tool call ids and normalize.
    pub fn normalize_tool_calls(
        &self,
        scope: &mut LoopScope,
        round: i64,
        tool_calls: &[crate::harness::transport::OpenAICompatibleToolCall],
    ) -> Vec<NormalizedCall> {
        use crate::harness::transport::{normalize_openai_compatible_tool_call, OpenAICompatibleToolCall};

        let mut normalized_calls: Vec<NormalizedCall> = Vec::new();

        for (index, tool_call) in tool_calls.iter().enumerate() {
            let mut call_id = match tool_call
                .id
                .as_deref()
                .map(str::trim)
                .filter(|trimmed| !trimmed.is_empty())
            {
                Some(id) => id.to_string(),
                None => format!("tool-call-{}-{}-{}", self.state.iteration, round, index + 1),
            };

            // Providers with deterministic per-response ids (call_0, call_1)
            // would collide across the cycles of a shared transcript; strict
            // endpoints reject duplicate tool_call_id pairs in history. The
            // remap is re-checked until unique (degenerate parsers can emit
            // the same id several times in one response).
            while scope.used_tool_call_ids.contains(&call_id) {
                call_id = format!(
                    "{}-c{}-{}-{}",
                    call_id, self.state.iteration, round, index + 1
                );
            }

            scope.used_tool_call_ids.insert(call_id.clone());

            let normalized = normalize_openai_compatible_tool_call(OpenAICompatibleToolCall {
                id: Some(call_id.clone()),
                ..tool_call.clone()
            });

            let function = tool_call.function.clone().unwrap_or_default();
            let tool_name = {
                let trimmed = function.name.as_deref().map(str::trim).unwrap_or("");
                if trimmed.is_empty() {
                    "tool_call".to_string()
                } else {
                    trimmed.to_string()
                }
            };

            normalized_calls.push(NormalizedCall {
                call_id,
                normalized,
                raw_input: function.arguments.clone().unwrap_or_else(|| "{}".to_string()),
                tool_name,
            });
        }

        normalized_calls
    }

    /// execute each normalized call: skipped-after-end,
    /// harness ops, or workspace tools (verification tracking, spill,
    /// telemetry, footprint, tool-call/tool-result events), appending the
    /// tool-role messages to the transcript.
    pub async fn dispatch_tool_calls(&mut self, scope: &mut LoopScope, calls: Vec<NormalizedCall>) {
        for call in calls {
            let NormalizedCall { call_id, raw_input, tool_name, .. } = call;

            // A survey timeout or a mid-wait abort ends the run: the calls
            // after it in this same model response must not execute — they
            // could mutate the workspace past run end or overwrite the
            // pending survey with a second ask_user.
            if self.ask_user_awaiting || self.signal_aborted() {
                break;
            }

            // Once a call ends the loop, later task-terminal calls in the same
            // response are not executed: a worker emitting finish_task twice
            // could otherwise complete its task AND deliver the verdict on its
            // own freshly spawned review (or drop it). Additive ops (plan_tasks,
            // notes, memory) still run.
            if scope.task_finished
                && (tool_name == "finish_task" || tool_name == "drop_task" || tool_name == "revise_task")
            {
                scope.digest_actions.push(format!("{tool_name}: skipped after loop end"));
                self.emit(HarnessEvent {
                    data: None,
                    detail: format!("{tool_name}: skipped — the loop already ended"),
                    iteration: self.state.iteration,
                    r#type: HarnessEventType::HarnessOp,
                });
                scope
                    .transport_messages
                    .push(crate::harness::transport::TransportRequestMessage {
                        content: Some(crate::harness::transport::TransportContent::Text(format!(
                            "The loop already ended (a previous call in this response finished the task) — {tool_name} was not executed. Re-issue it from the next loop if still needed."
                        ))),
                        name: Some(tool_name),
                        role: ChatRoleTag::Tool,
                        tool_call_id: Some(call_id),
                        tool_calls: None,
                        anthropic_content: None,
                    });
                continue;
            }

            if is_harness_tool(&tool_name) {
                let op = parse_harness_op_with_gate(&tool_name, &raw_input, self.role_gate.as_ref());
                let op_context = crate::harness::harness_tools::HarnessOpContext {
                    loop_number: self.state.r#loop as u32,
                    current_task_id: scope.current_task_id.clone(),
                    loop_ended: scope.task_finished,
                    gate: self.role_gate.clone(),
                    repo_memory: self
                        .options
                        .repo_memory
                        .clone()
                        .unwrap_or(RepoMemoryConfig {
                            memory_dir: String::new(),
                            disabled: true,
                        }),
                    ask_user_enabled: self.options.ask_user_enabled,
                    // Sticky opt-out decided at the cycle boundary (goal
                    // phrase, operator message, --no-review, --lite); also
                    // rejects delayed Review*/reviewer task work mid-run.
                    review_opt_out: self.state.review_opt_out == Some(true),
                };
                let outcome = match op {
                    Ok(op) => {
                        let ask_user_pending = matches!(
                            op,
                            crate::harness::harness_tools::HarnessOp::AskUser { .. }
                        );
                        let outcome = apply_harness_op(&mut self.state, op, &op_context);
                        // ask_user accepted: expose the survey as a question
                        // event (the blocking answers.jsonl wait and the
                        // awaiting-input timeout land with the lifecycle task).
                        if ask_user_pending && outcome.state_changed {
                            if let Some(survey) = self.state.pending_questions.clone() {
                                let question_count = survey.questions.len();
                                self.emit(HarnessEvent {
                                    data: Some(HarnessEventData {
                                        r#loop: Some(self.state.r#loop),
                                        question_survey: Some(survey.clone()),
                                        task_id: scope.current_task_id.clone(),
                                        ..Default::default()
                                    }),
                                    detail: format!(
                                        "ask_user: {question_count} question(s) pending operator answers"
                                    ),
                                    iteration: self.state.iteration,
                                    r#type: HarnessEventType::Question,
                                });
                                let wait = self.run_survey_block(survey.clone()).await;
                                if !matches!(wait, SurveyWait::Answered) {
                                    scope.digest_actions.push(
                                        "ask_user: no answers — run ending (awaiting-input or abort)"
                                            .to_string(),
                                    );
                                }
                            }
                        }
                        outcome
                    }
                    Err(err) => {
                        // Parse failure keeps the run alive: surface the
                        // model-facing error as this call's tool result.
                        scope.digest_actions.push(format!(
                            "{}: {}",
                            tool_name,
                            truncate_text(&err, MAX_DIGEST_ACTION_CHARS)
                        ));
                        self.emit(HarnessEvent {
                            data: Some(HarnessEventData {
                                r#loop: Some(self.state.r#loop),
                                tool_name: Some(tool_name.clone()),
                                task_id: scope.current_task_id.clone(),
                                ..Default::default()
                            }),
                            detail: format!("{tool_name}: {err}"),
                            iteration: self.state.iteration,
                            r#type: HarnessEventType::HarnessOp,
                        });
                        scope
                            .transport_messages
                            .push(crate::harness::transport::TransportRequestMessage {
                                content: Some(crate::harness::transport::TransportContent::Text(err)),
                                name: Some(tool_name),
                                role: ChatRoleTag::Tool,
                                tool_call_id: Some(call_id),
                                tool_calls: None,
                                anthropic_content: None,
                            });
                        continue;
                    }
                };
                scope.digest_actions.push(format!(
                    "{}: {}",
                    tool_name,
                    truncate_text(&outcome.text, MAX_DIGEST_ACTION_CHARS)
                ));
                self.emit(HarnessEvent {
                    data: Some(HarnessEventData {
                        r#loop: Some(self.state.r#loop),
                        tool_name: Some(tool_name.clone()),
                        task_id: scope.current_task_id.clone(),
                        ..Default::default()
                    }),
                    detail: format!("{}: {}", tool_name, outcome.text),
                    iteration: self.state.iteration,
                    r#type: if outcome.task_finished {
                        HarnessEventType::TaskFinished
                    } else {
                        HarnessEventType::HarnessOp
                    },
                });
                scope.task_finished = scope.task_finished || outcome.task_finished;
                scope.made_progress = scope.made_progress || outcome.state_changed;
                scope.persisted_this_loop = scope.persisted_this_loop || outcome.state_changed;
                // drip-specific: memory-bank writes (remember/forget) get their
                // own event, gated on apply_harness_op's state_changed so a
                // failed, denied, or no-op op never announces a state change.
                // Fires at this choke point (not execute_workspace_tool) because
                // harness ops never reach it. Payload = redacted input (the note).
                if (tool_name == "remember" || tool_name == "forget") && outcome.state_changed {
                    let redacted_input = (self.redact)(&raw_input);
                    self.fire_hook(
                        crate::harness::hooks::HookEvent::MemoryWrite,
                        Some((tool_name.as_str(), redacted_input.as_str())),
                    );
                }
                scope
                    .transport_messages
                    .push(crate::harness::transport::TransportRequestMessage {
                        content: Some(crate::harness::transport::TransportContent::Text(outcome.text)),
                        name: Some(tool_name),
                        role: ChatRoleTag::Tool,
                        tool_call_id: Some(call_id),
                        tool_calls: None,
                        anthropic_content: None,
                    });
                continue;
            }

            // Workspace tool execution. Snapshot the prior record first so the
            // repeat note can say whether this exact call has run before and
            // what it produced.
            let telemetry_key = tool_telemetry_key(&tool_name, &raw_input);
            let prior_record = self.state.telemetry.get(&telemetry_key).cloned();
            let prior_call_count = prior_record.as_ref().map(|record| record.call_count).unwrap_or(0);
            let prior_loop_text = prior_record.as_ref().map(|record| record.last_used_iteration.to_string());
            let prior_output = prior_record.map(|record| record.last_output);

            let execution_started_at_ms = (self.now)().timestamp_millis();
            let mut execution = self.execute_workspace_tool(&call_id, &raw_input, Some(&scope.loop_tool_indexes), &tool_name);
            let execution_duration_ms = (self.now)().timestamp_millis() - execution_started_at_ms;

            // A successful workspace mutation counts as task progress even if
            // finish_task is not called this loop, so the stall counter does
            // not increment for loops that land real edits.
            let bash_command = if tool_name == "BASH" { Some(extract_bash_command(&raw_input).unwrap_or_default()) } else { None };
            // Flash writes whole files with `cat > f <<'EOF'` and edits with
            // `sed -i`: a plain shell write mutates the workspace exactly
            // like a PATCH and must count the same way, or the loop registers
            // no progress (stall accounting), the verification staleness
            // counter stays at 0, and the task footprint never says "edited".
            let shell_write = bash_command.as_deref().is_some_and(is_writing_shell_command);
            let may_mutate = execution.dispatched && (shell_write || self.tool_registry.get(&tool_name)
                .is_some_and(|index| self.tools[*index].mutates_workspace));
            // A failed writer may have changed files before failing. Its old
            // verification must become stale even if side effects are unknown.
            if may_mutate {
                scope.made_progress |= !execution.failed;
                scope.persisted_this_loop = true;
                self.state.mutations_since_verification = Some(self.state.mutations_since_verification.unwrap_or(0) + 1);
                self.state.workspace_edits = Some(self.state.workspace_edits.unwrap_or(0) + 1);
            }
            // Paths the run has edited decide whether a later "external"
            // verification anchor is honest: a check that names a file the
            // agent wrote is consistency with its own work, not correctness.
            // Fed from the same mutation accounting as above — shell writes
            // and failed writers count, best-effort on what paths they name.
            if tool_name == "PATCH" {
                for patched_path in extract_patched_paths(&raw_input) {
                    record_edited_path(&mut self.state.edited_paths, &patched_path);
                }
            }
            if shell_write {
                for target in extract_shell_write_targets(bash_command.as_deref().unwrap_or_default()) {
                    record_edited_path(&mut self.state.edited_paths, &target);
                }
            }

            let verification_command = extract_verification_command_for_goal(&tool_name, &raw_input, &self.state.goal)
                .or_else(|| {
                    (tool_name == "BASH" && execution.tool_content.lines().any(|line| line.starts_with(crate::tools::builtin::verify::CUSTOM_RESULT_PREFIX)))
                        .then(|| extract_bash_command(&raw_input)).flatten()
                });
            if let Some(verification_command) = verification_command.clone() {
                let truncated_command = truncate_text(&verification_command, 200);
                let output_tail = truncate_text_keeping_ends(&execution.tool_content, 500);
                let ran_no_tests =
                    !execution.failed && detect_empty_test_run(&verification_command, &execution.tool_content);
                let evidence = if tool_name == "CHECK" {
                    crate::core::types::VerificationEvidence {
                        anchor: None,
                        kind: crate::core::types::VerificationEvidenceKind::Typecheck,
                        executed: 0, passed: 0, failed: i64::from(execution.failed), skipped: None,
                        detail: Some("Compiler diagnostics for the requested scope; no tests executed.".into()),
                    }
                } else {
                    let mut evidence = crate::tools::builtin::verify::verification_evidence(&verification_command, &execution.tool_content);
                    if tool_name == "VERIFY" {
                        evidence.anchor = declared_verification_anchor(&raw_input, &self.state.edited_paths);
                        if let Some(reason) = evidence.anchor.as_ref().and_then(|anchor| anchor.downgraded_reason.clone()) {
                            self.emit(HarnessEvent {
                                data: None,
                                detail: format!("verification anchor downgraded to self-authored: {reason}"),
                                iteration: self.state.iteration,
                                r#type: HarnessEventType::RunWarning,
                            });
                        }
                    }
                    evidence
                };
                execution.failed |= evidence.failed > 0;
                let verification_record = HarnessVerificationRecord {
                    at_iteration: self.state.iteration,
                    command: truncated_command.clone(),
                    failed: execution.failed,
                    output_tail: output_tail.clone(),
                    ran_no_tests: ran_no_tests.then_some(true),
                    evidence: Some(evidence),
                };

                if ran_no_tests {
                    self.emit(HarnessEvent {
                        data: None,
                        detail: format!(
                            "verification passed without executing any test: {truncated_command} — it does not count as evidence until a run executes tests"
                        ),
                        iteration: self.state.iteration,
                        r#type: HarnessEventType::RunWarning,
                    });
                }

                self.state.last_verification = Some(verification_record.clone());
                self.state.verifications = {
                    let mut timeline = self.state.verifications.clone().unwrap_or_default();
                    timeline.push(verification_record);
                    if timeline.len() > MAX_VERIFICATION_TIMELINE {
                        let excess = timeline.len() - MAX_VERIFICATION_TIMELINE;
                        timeline.drain(0..excess);
                    }
                    Some(timeline)
                };
                self.state.mutations_since_verification = Some(0);

                if execution.failed {
                    let failure_hash = hash_text(&output_tail);
                    let prior = self.state.verification_streak.take();
                    let streak = match prior {
                        Some(prior) if prior.command == truncated_command && prior.output_tail_hash == failure_hash => {
                            HarnessVerificationStreak {
                                command: truncated_command,
                                consecutive_failures: prior.consecutive_failures + 1,
                                output_tail_hash: failure_hash,
                            }
                        }
                        _ => HarnessVerificationStreak {
                            command: truncated_command,
                            consecutive_failures: 1,
                            output_tail_hash: failure_hash,
                        },
                    };
                    self.state.verification_streak = Some(streak);
                    if self.state.verification_streak.as_ref().map(|s| s.consecutive_failures).unwrap_or(0) >= VERIFICATION_STUCK_THRESHOLD as i64 {
                        scope.verification_stuck_this_loop = true;
                    }
                } else {
                    self.state.verification_streak = None;
                }
            }

            let mut tool_content = truncate_text_keeping_ends(
                &execution.tool_content,
                scope.loop_budget.max_tool_result_chars as usize,
            );

            // Oversized output spills to a file beside the state store so the
            // elided middle stays recoverable with READ/GREP.
            if execution.tool_content.chars().count() > scope.loop_budget.max_tool_result_chars as usize {
                if let Some(state_path) = self.options.state_path.clone() {
                    let spill_path = spill_tool_output(&state_path.to_string_lossy(), self.state.r#loop as u32, &call_id, &execution.tool_content);
                    if let Some(spill_path) = spill_path {
                        tool_content = format!(
                            "{}\n\n[harness] {} chars total — the full output is saved at {}; READ or GREP that exact absolute path (it is outside the workspace) instead of re-running the command.",
                            tool_content,
                            format_thousands(execution.tool_content.chars().count()),
                            spill_path
                        );
                    }
                }
            }

            // Failed calls are recorded too: a call the model keeps retrying is
            // exactly the one future loops most need a memory of.
            let record = record_tool_telemetry(
                &mut self.state,
                crate::harness::telemetry::RecordToolTelemetryArgs {
                    failed: Some(execution.failed),
                    output: &execution.tool_content,
                    raw_input: &raw_input,
                    tool_name: &tool_name,
                },
                &self.telemetry_config,
            );

            let content_hash = hash_text(&execution.tool_content);
            let deduped_tool = DEDUPED_READ_ONLY_TOOLS.contains(&tool_name.as_str());
            let repeats_hot_result = deduped_tool
                && !execution.failed
                && scope
                    .hot_read_only_results
                    .get(&telemetry_key)
                    .is_some_and(|(index, hash)| *hash == content_hash && !scope.folded_message_indexes.contains(index));

            // Identical re-runs get one line of feedback in the tool result.
            if repeats_hot_result {
                // The earlier result is still verbatim in the transcript: a
                // stub keeps the model's context and the request small.
                tool_content = build_repeated_read_stub(&tool_name, prior_call_count);
            } else if prior_call_count > 0 {
                let output_unchanged = prior_output.as_deref() == Some(record.last_output.as_str());
                let prior_loop = prior_loop_text.clone().unwrap_or_else(|| "undefined".to_string());
                tool_content = format!(
                    "{tool_content}\n\n[harness] Identical call #{} this run (previously in loop {}). {}",
                    prior_call_count + 1,
                    prior_loop,
                    if output_unchanged {
                        "The output is unchanged — re-running this again will not add information. Act on the result, or record the finding with observe/remember/finish_task."
                    } else {
                        "The output changed since the previous run."
                    }
                );
            }

            if deduped_tool && !execution.failed && !repeats_hot_result {
                // The message pushed at the end of this call lands at this index.
                scope.hot_read_only_results.insert(telemetry_key.clone(), (scope.transport_messages.len(), content_hash));
            }

            let read_only_bash = bash_command.as_deref().is_some_and(is_read_only_shell_command);

            if (deduped_tool || read_only_bash) && !execution.failed {
                scope.read_only_calls_this_loop += 1;
                if !scope.persisted_this_loop && scope.read_only_calls_this_loop % READ_ONLY_NUDGE_EVERY == 0 {
                    tool_content = format!(
                        "{tool_content}\n\n{}",
                        build_read_only_loop_nudge(scope.read_only_calls_this_loop, scope.cycle, scope.affordable_cycles)
                    );
                    // Surfaced in the event stream so transcript audits can see
                    // when the nudge fired and what the model did next.
                    self.emit(HarnessEvent {
                        data: Some(HarnessEventData {
                            r#loop: Some(self.state.r#loop),
                            tool_name: Some(tool_name.clone()),
                            ..Default::default()
                        }),
                        detail: format!(
                            "read-only nudge: {} read-only calls this loop (cycle {}/{}) with nothing written or recorded",
                            scope.read_only_calls_this_loop, scope.cycle, scope.affordable_cycles
                        ),
                        iteration: self.state.iteration,
                        r#type: HarnessEventType::RunWarning,
                    });
                }
            }

            // A new file at a path the goal never named gets one line of
            // feedback while it is still cheap to move.
            if !execution.failed && tool_name == "PATCH" {
                if let Some(note) = build_unnamed_path_note(&self.state.goal, &execution.tool_content) {
                    tool_content = format!("{tool_content}\n\n{note}");
                    self.emit(HarnessEvent {
                        data: Some(HarnessEventData {
                            r#loop: Some(self.state.r#loop),
                            tool_name: Some(tool_name.clone()),
                            ..Default::default()
                        }),
                        detail: format!("unnamed-path note: {}", note.trim_start_matches("[harness] ")),
                        iteration: self.state.iteration,
                        r#type: HarnessEventType::RunWarning,
                    });
                }
            }

            scope.digest_actions.push(format!(
                "{} {}{}",
                tool_name,
                truncate_text(&canonicalize_tool_input(&raw_input), MAX_DIGEST_ACTION_CHARS),
                if execution.failed { " (failed)" } else { "" }
            ));

            if let Some(task_id) = scope.current_task_id.clone() {
                if may_mutate && !execution.failed && tool_name != "PATCH" && !shell_write {
                    if let Some(task) = core_state::get_task_by_id_mut(&mut self.state, &task_id) {
                        record_task_footprint(&mut task.footprint, &format!("edited via {tool_name}"));
                    }
                }
                if may_mutate && execution.failed {
                    if let Some(task) = core_state::get_task_by_id_mut(&mut self.state, &task_id) {
                        record_task_footprint(&mut task.footprint, &format!("edited state uncertain: {tool_name} failed; reverify possible side effects"));
                    }
                }
                if !execution.failed && tool_name == "PATCH" {
                    let patched = extract_patched_paths(&raw_input);
                    if let Some(task) = core_state::get_task_by_id_mut(&mut self.state, &task_id) {
                        for patched_path in patched {
                            record_task_footprint(&mut task.footprint, &format!("edited {patched_path}"));
                        }
                    }
                }
                if shell_write && !execution.failed {
                    let command = bash_command.as_deref().unwrap_or_default();
                    let collapsed = strip_heredoc_bodies(command).split_whitespace().collect::<Vec<_>>().join(" ");
                    if let Some(task) = core_state::get_task_by_id_mut(&mut self.state, &task_id) {
                        record_task_footprint(&mut task.footprint, &format!("edited via shell: {}", truncate_text(&collapsed, 120)));
                    }
                }
                if let Some(command) = verification_command.as_deref() {
                    let entry = format!(
                        "ran {} -> {}",
                        truncate_text(command, 120),
                        match &self.state.last_verification {
                            Some(record) => core_state::describe_verification_outcome(record.failed, record.ran_no_tests, record.evidence.as_ref()),
                            None => core_state::describe_verification_outcome(execution.failed, None, None),
                        }
                    );
                    if let Some(task) = core_state::get_task_by_id_mut(&mut self.state, &task_id) {
                        record_task_footprint(&mut task.footprint, &entry);
                    }
                }
            }

            self.emit(HarnessEvent {
                data: Some(HarnessEventData {
                    call_id: Some(call_id.clone()),
                    r#loop: Some(self.state.r#loop),
                    tool_name: Some(tool_name.clone()),
                    task_id: scope.current_task_id.clone(),
                    ..Default::default()
                }),
                detail: format!("{tool_name} {raw_input}"),
                iteration: self.state.iteration,
                r#type: HarnessEventType::ToolCall,
            });
            *self.run_tool_usage.entry(tool_name.clone()).or_insert(0) += 1;
            self.emit(HarnessEvent {
                data: Some(HarnessEventData {
                    call_id: Some(call_id.clone()),
                    duration_ms: Some(execution_duration_ms),
                    failed: Some(execution.failed),
                    r#loop: Some(self.state.r#loop),
                    tool_name: Some(tool_name.clone()),
                    task_id: scope.current_task_id.clone(),
                    ..Default::default()
                }),
                detail: format!(
                    "{}{}: {}",
                    tool_name,
                    if execution.failed { " (failed)" } else { "" },
                    truncate_text_keeping_ends(&execution.tool_content, MAX_RESULT_EVENT_CHARS)
                ),
                iteration: self.state.iteration,
                r#type: HarnessEventType::ToolResult,
            });

            // drip-specific: PRReady fires only after a publish-matching tool
            // call actually succeeded (git commit/push, `gh pr create`). The
            // veto and failure paths return failed:true above, so they stay
            // silent. Lifecycle payload (no tool fields); reuses fire_hook so
            // hook failures surface as RunWarning in the transcript.
            if !execution.failed
                && crate::harness::hooks::git_publish_pattern().is_match(&raw_input)
            {
                self.fire_hook(crate::harness::hooks::HookEvent::PRReady, None);
            }

            scope
                .transport_messages
                .push(crate::harness::transport::TransportRequestMessage {
                    content: Some(crate::harness::transport::TransportContent::Text(tool_content)),
                    name: Some(tool_name),
                    role: ChatRoleTag::Tool,
                    tool_call_id: Some(call_id),
                    tool_calls: None,
                    anthropic_content: None,
                });
        }
    }

    /// the loop digest outcome text + abort/budget checks.
    pub fn end_loop(&mut self, scope: &mut LoopScope) {
        let outcome: String = if self.aborted {
            "the run was stopped mid-loop".to_string()
        } else if scope.task_finished {
            "a task was finished".to_string()
        } else if scope.planned_and_yielded {
            "the plan was updated — the next loop starts on the first workable task".to_string()
        } else if !scope.concluded_naturally {
            format!(
                "the loop ran out of cycles ({} cycle(s) of {} tool rounds) before finish_task — persist progress with finish_task, observe, remember, or plan_tasks earlier",
                scope.cycles_run,
                scope.loop_budget.max_tool_rounds_per_cycle
            )
        } else if scope.made_progress {
            "progress was recorded, but the current task is not finished".to_string()
        } else {
            "no progress was recorded — nothing from this loop was persisted".to_string()
        };

        // Every loop hands its hot tail to the next task loop (see
        // extract_loop_carryover); an aborted loop or one with no exchanges
        // hands nothing.
        self.carryover = if self.aborted {
            None
        } else {
            let messages = extract_loop_carryover(&scope.transport_messages, scope.loop_budget.hot_tool_results.max(0) as usize);
            if messages.is_empty() {
                None
            } else {
                let task_title = scope
                    .current_task_id
                    .as_deref()
                    .and_then(|task_id| self.state.tasks.iter().find(|task| task.id == task_id))
                    .map(|task| task.title.clone());
                Some(LoopCarryover {
                    finished: scope.task_finished,
                    r#loop: self.state.r#loop,
                    messages,
                    task_id: scope.current_task_id.clone(),
                    task_title,
                })
            }
        };

        self.record_loop_digest(scope, &outcome);
    }

    /// stall accounting, auto-block, reopen/drop
    /// escalation, futile detection, telemetry maintenance, observation decay.
    /// stall accounting, auto-block, reopen/drop
    /// escalation, futile detection, telemetry maintenance, observation decay.
    pub fn after_loop(&mut self, scope: &LoopScope) {
        // a loop cut short by the run budget never got
        // its full cycle allowance; penalizing the task for that would let
        // repeated short-budget resumes auto-block healthy work.
        let budget_truncated = !scope.concluded_naturally
            && scope.cycles_run < scope.loop_budget.max_cycles
            && self.state.iteration - self.start_iteration >= self.max_iterations;

        if let Some(task_id) = scope.current_task_id.clone() {
            // An abort is a user action, not a stall: a loop cut short must
            // not count toward auto-blocking the task it was working.
            if !scope.task_finished && !self.aborted && !budget_truncated {
                let mut auto_block_stalls: Option<i64> = None;

                if let Some(task) = crate::core::state::get_task_by_id_mut(&mut self.state, &task_id) {
                    // Edits made while the same verification keeps failing
                    // identically are churn: the stall counter must see
                    // through them or a model can PATCH forever without ever
                    // moving the failure.
                    if scope.made_progress && !scope.verification_stuck_this_loop {
                        task.stall_count = 0;
                    } else {
                        task.stall_count += 1;

                        if task.stall_count >= self.stall_limit {
                            auto_block_stalls = Some(task.stall_count);
                        }
                    }
                }

                if let Some(stall_count) = auto_block_stalls {
                    let summary = format!(
                        "Auto-blocked after {} task loop(s) with no recorded progress.",
                        stall_count
                    );
                    crate::core::state::finish_task(
                        &mut self.state,
                        crate::core::state::HarnessFinishArgs {
                            status: crate::core::types::HarnessTaskStatus::Blocked,
                            summary: &summary,
                            task_id: Some(&task_id),
                            confidence: None,
                        },
                    );
                    self.emit(crate::core::types::HarnessEvent {
                        data: Some(crate::core::types::HarnessEventData {
                            status: Some("blocked".to_string()),
                            task_id: Some(task_id.clone()),
                            ..Default::default()
                        }),
                        detail: format!(
                            "{} auto-blocked after {} stalled task loop(s)",
                            task_id, stall_count
                        ),
                        iteration: self.state.iteration,
                        r#type: crate::core::types::HarnessEventType::TaskFinished,
                    });
                }
            }

            self.idle_loops = 0;
            self.escalations_without_progress = 0;
        } else if scope.made_progress {
            // A planning loop with no current task (e.g. the model called
            // plan_tasks) counts as progress.
            self.idle_loops = 0;
        } else {
            // No task to work and nothing changed. The run never gives up on
            // the goal: after stallLimit idle loops, force blocked tasks back
            // to pending so the model returns to concrete work instead of
            // spinning in replanning mode.
            self.idle_loops += 1;

            if self.idle_loops >= self.stall_limit {
                let idle_count = self.idle_loops;

                self.idle_loops = 0;

                self.escalations_without_progress += 1;

                if self.escalations_without_progress >= 3 {
                    self.run_futile = true;
                }

                let loop_number = self.state.r#loop;
                let reopened = crate::core::state::reopen_blocked_tasks(
                    &mut self.state,
                    &format!(
                        "Auto-reopened at loop {}: the run stalled with no workable tasks. The previous approach did not record progress — try a different one.",
                        loop_number
                    ),
                    Some(self.max_task_reopens),
                );

                if !reopened.is_empty() {
                    self.emit(crate::core::types::HarnessEvent {
                        data: None,
                        detail: format!(
                            "reopened {} blocked task(s) after {} loop(s) with no progress",
                            reopened.len(),
                            idle_count
                        ),
                        iteration: self.state.iteration,
                        r#type: crate::core::types::HarnessEventType::StallRecovery,
                    });
                } else {
                    // Every blocked task has exhausted its reopen budget.
                    // Reopening them again would replay the same stall loop,
                    // so escalate: drop them with an honest summary and let
                    // the run either finish or replan fresh.
                    let dropped_tasks = crate::core::state::drop_exhausted_blocked_tasks(&mut self.state);

                    self.emit(crate::core::types::HarnessEvent {
                        data: None,
                        detail: if !dropped_tasks.is_empty() {
                            format!(
                                "dropped {} blocked task(s) that exhausted {} reopen cycle(s) — a different approach or user input is needed",
                                dropped_tasks.len(),
                                self.max_task_reopens
                            )
                        } else {
                            format!(
                                "no progress for {} loop(s) and no blocked tasks to reopen — continuing",
                                idle_count
                            )
                        },
                        iteration: self.state.iteration,
                        r#type: crate::core::types::HarnessEventType::StallRecovery,
                    });
                }
            }
        }

        if self.run_futile {
            self.emit(crate::core::types::HarnessEvent {
                data: None,
                detail: "the run is stuck in a replan→stall→drop cycle with no completed work between escalations — ending as futile".to_string(),
                iteration: self.state.iteration,
                r#type: crate::core::types::HarnessEventType::RunWarning,
            });
            self.persist();
            // The outer loop ends before telemetry maintenance;
            // run_loops reads self.run_futile and ends the run.
            return;
        }

        // Hot → warm ingestion happens at loop boundaries: results this loop
        // reached for join telemetry above; here warm entries decay one ttl
        // per loop and telemetry that separate loops kept reaching for is
        // promoted.
        let dynamic_tool_names = self.dynamic_tool_names.clone();
        let maintenance = crate::harness::telemetry::run_telemetry_maintenance(
            &mut self.state,
            crate::harness::telemetry::RunTelemetryMaintenanceArgs { dynamic_tool_names },
            &self.telemetry_config,
        );

        for entry in maintenance.promoted {
            self.emit(crate::core::types::HarnessEvent {
                data: None,
                detail: format!(
                    "{} {} (ttl {}, reinforcements {})",
                    entry.tool_name, entry.input_preview, entry.ttl, entry.reinforcements
                ),
                iteration: self.state.iteration,
                r#type: crate::core::types::HarnessEventType::ContextPromoted,
            });
        }

        for entry in maintenance.expired {
            self.emit(crate::core::types::HarnessEvent {
                data: None,
                detail: format!("{} {}", entry.tool_name, entry.input_preview),
                iteration: self.state.iteration,
                r#type: crate::core::types::HarnessEventType::ContextExpired,
            });
        }

        for observation in crate::core::state::decay_observations(&mut self.state, Some(scope.loop_start_iteration)) {
            self.emit(crate::core::types::HarnessEvent {
                data: None,
                detail: format!(
                    "observation {}: {}",
                    observation.id,
                    crate::harness::telemetry::truncate_text(&observation.text, 120)
                ),
                iteration: self.state.iteration,
                r#type: crate::core::types::HarnessEventType::ContextExpired,
            });
        }

        self.persist();
    }

    /// decide the run reason, generate the run summary,
    /// report leaked tmux jobs, emit `run-complete`, persist, build the result.
    pub async fn finish(mut self) -> HarnessRunResult {
        self.fire_hook(crate::harness::hooks::HookEvent::Stop, None);
        if self.aborted {
            return self.aborted_result();
        }

        let start_iteration = self.start_iteration;

        let reason: HarnessRunReason = if self.run_error.is_some() {
            HarnessRunReason::Error
        } else if self.ask_user_awaiting {
            HarnessRunReason::AwaitingInput
        } else if self.plan_stopped {
            HarnessRunReason::Planned
        } else if self.run_futile {
            HarnessRunReason::Futile
        } else if core_state::is_goal_complete(&self.state) {
            // Model-pruned drops (drop_task) are healthy plan revision; only
            // tasks the harness dropped as exhausted mark the run as
            // partially accomplished.
            if self.state.tasks.iter().any(|task| {
                task.status == crate::core::types::HarnessTaskStatus::Dropped
                    && task.dropped_exhausted == Some(true)
            }) {
                HarnessRunReason::Partial
            } else if !self.state.anomalies.is_empty() {
                // Every task finished, but at least one pre-registered
                // expectation could not be reconciled: complete work with a
                // visible anomaly, not a failure and not a caveat.
                HarnessRunReason::Unreconciled
            } else if self.options.lite {
                // Draft mode (--lite): a completed run is a draft — the same
                // terminal, exit-0 state as "completed", but the result
                // carries the harden resume command and skips the summary.
                HarnessRunReason::Draft
            } else {
                HarnessRunReason::Completed
            }
        } else {
            HarnessRunReason::MaxIterations
        };

        // Draft runs skip the end-of-run summary model call: the operator
        // reads the result and hardens it via the continue command.
        if reason != HarnessRunReason::Draft
            && self.options.summarize_run.unwrap_or(true)
            && (self.state.iteration > start_iteration || self.run_error.is_some())
        {
            self.generate_run_summary(reason).await;
        }

        // Background jobs the run started and never tore down: report them so
        // the driver knows a dev server/watcher is still holding the port
        // (and how to kill it) instead of discovering it three runs later.
        // Background tmux jobs that outlived the run.
        let leaked_jobs: Vec<crate::core::types::HarnessLeakedJob> = self
            .tool_services
            .tmux_sessions
            .list_sessions()
            .into_iter()
            .map(|session| crate::core::types::HarnessLeakedJob {
                command: session.title,
                kill_command: session.kill_command,
                session_name: session.session_name,
                started_at: session.started_at,
            })
            .collect();

        if !leaked_jobs.is_empty() {
            let running = leaked_jobs
                .iter()
                .map(|job| {
                    format!(
                        "{} ({})",
                        job.session_name,
                        truncate_text(&job.command, 60)
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            self.emit(HarnessEvent {
                data: None,
                detail: format!(
                    "{} background job(s) from this run are still running: {} — stop with the killCommand in the result payload if not wanted",
                    leaked_jobs.len(),
                    running
                ),
                iteration: self.state.iteration,
                r#type: HarnessEventType::RunWarning,
            });
        }

        let reason_wire = match &reason {
            HarnessRunReason::AwaitingInput => "awaiting-input",
            HarnessRunReason::Aborted => "aborted",
            HarnessRunReason::Completed => "completed",
            HarnessRunReason::Draft => "draft",
            HarnessRunReason::Error => "error",
            HarnessRunReason::Futile => "futile",
            HarnessRunReason::MaxIterations => "max-iterations",
            HarnessRunReason::Partial => "partial",
            HarnessRunReason::Planned => "planned",
            HarnessRunReason::Unreconciled => "unreconciled",
        };

        self.emit(HarnessEvent {
            data: Some(HarnessEventData {
                reason: Some(reason_wire.to_string()),
                ..Default::default()
            }),
            detail: reason_wire.to_string(),
            iteration: self.state.iteration,
            r#type: HarnessEventType::RunComplete,
        });

        // Draft mode (--lite): hand the operator the exact harden resume
        // command — same session, reviewed role, verify-before-done skill —
        // so one paste finishes the draft.
        if reason == HarnessRunReason::Draft && self.continue_command.is_none() {
            if let Some(session_id) = self.resume_target() {
                self.continue_command = Some(crate::harness::telemetry::draft_continue_command(
                    &session_id,
                    &self.state.goal,
                ));
            }
        }
        self.persist();

        HarnessRunResult {
            continue_command: self.continue_command.clone(),
            error_message: self.run_error.clone(),
            // Per-run counts: resumed and follow-up runs report their own
            // work, not the session-cumulative counters.
            iterations: self.state.iteration - start_iteration,
            r#loops: self.state.r#loop - self.start_loop,
            leaked_jobs: if leaked_jobs.is_empty() {
                None
            } else {
                Some(leaked_jobs)
            },
            reason,
            state: self.state.clone(),
            usage: self.finalize_usage(),
            stop_latency_ms: self.stop_latency_ms(),
        }
    }
}

impl HarnessRun {
    /// One tool-free model call that
    /// writes the user-facing recap, with three hard rules the loop used to
    /// hold inline: an error run never calls the endpoint that just failed, a
    /// respond answer is delivered verbatim rather than re-summarized, and a
    /// failed summary call falls back deterministically so a finished run is
    /// never failed by its own recap.
    pub async fn generate_run_summary(&mut self, reason: HarnessRunReason) {
        // An error run skips the summary model call — the endpoint is the thing
        // that just failed — and reports deterministically instead.
        if let Some(run_error) = self.run_error.clone() {
            let text = format!(
                "Run failed: {}\n\n{}\n\nThe session state is persisted — re-submit the same goal to resume once the endpoint is healthy.",
                run_error,
                crate::harness::prompt::build_fallback_run_summary(&self.state, reason)
            );
            self.record_run_summary(reason, text);
            return;
        }

        // A direct answer from the respond op IS the result — deliver it verbatim
        // instead of paying for (and risking drift from) a second summarization.
        if let Some(direct) = self.state.direct_response.clone() {
            if direct.created_at_iteration > self.start_iteration {
                self.record_run_summary(reason, direct.text);
                return;
            }
        }

        // Facts are best-effort; the summary still runs without them.
        let workspace_changes = self
            .options
            .collect_run_facts
            .as_mut()
            .and_then(|collect| collect());

        let messages = crate::harness::prompt::build_run_summary_messages(
            &self.state,
            &crate::harness::prompt::RunSummaryMessagesArgs {
                current_date: &(self.now)().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                reason,
                tool_usage: Some(self.run_tool_usage.clone()),
                workspace_changes,
            },
        );
        // The summary must never turn a finished run into a failure; fall back
        // to a deterministic recap.
        let summary_text = match self
            .call_model
            .call_model(
                messages,
                Some(crate::harness::model_call::ModelCallOptions {
                    include_tools: Some(false),
                    ..Default::default()
                }),
            )
            .await
        {
            Ok(response) => response
                .choices
                .as_ref()
                .and_then(|choices| choices.first())
                .and_then(|choice| choice.message.as_ref())
                .and_then(|message| message.content.as_ref())
                .map(extract_response_text)
                .unwrap_or_default()
                .trim()
                .to_string(),
            Err(_) => String::new(),
        };
        self.drain_usage_inbox();

        let text = if summary_text.is_empty() {
            crate::harness::prompt::build_fallback_run_summary(&self.state, reason)
        } else {
            summary_text
        };
        self.record_run_summary(reason, text);
    }

    /// Record a run summary: persist on state and emit `run-summary`.
    fn record_run_summary(&mut self, reason: HarnessRunReason, text: String) {
        self.state.run_summary = Some(crate::core::types::HarnessRunSummaryNote {
            created_at_iteration: self.state.iteration,
            reason,
            text: text.clone(),
        });
        self.emit(HarnessEvent {
            data: None,
            detail: text,
            iteration: self.state.iteration,
            r#type: HarnessEventType::RunSummary,
        });
    }
}

/// Outcome of a blocking ask_user survey wait.
enum SurveyWait {
    Answered,
    TimedOut,
    Aborted,
}

/// One answers.jsonl poll: return the first complete batch that validates
/// against the pending survey, or None to keep waiting. Stale, malformed, and
/// mismatched batches only advance the cursor — they are never accepted.
fn poll_survey_answers(
    answers_path: &Path,
    survey: &crate::core::types::QuestionSurvey,
    cursor: &mut usize,
) -> Option<crate::core::types::HarnessSurveyAnswers> {
    for entry in crate::core::state::answers::read_answer_batches_after(answers_path, *cursor) {
        *cursor = (*cursor).max(entry.line_number + 1);
        if crate::harness::harness_tools::validate_survey_answers(survey, &entry.record).is_ok() {
            return Some(entry.record);
        }
    }
    None
}

/// Rendered Q->A summary injected into the conversation as an operator
/// message; the leading directive tells the model to revise its plan with
/// plan_tasks/revise_task before continuing.
fn render_survey_answers(
    survey: &crate::core::types::QuestionSurvey,
    answers: &crate::core::types::HarnessSurveyAnswers,
) -> String {
    let mut lines = vec![
        crate::harness::prompt::ASK_USER_ANSWER_DIRECTIVE.to_string(),
    ];
    for answer in &answers.answers {
        if answer.index < 0 {
            continue;
        }
        let Some(question) = survey.questions.get(answer.index as usize) else {
            continue;
        };
        let response = match (&answer.choice, &answer.other) {
            (Some(choice), Some(other)) => format!("{choice} (free text: {other})"),
            (Some(choice), None) => choice.clone(),
            (None, Some(other)) => format!("Other: {other}"),
            (None, None) => continue,
        };
        lines.push(format!("Q: {} — A: {}", question.question, response));
    }
    lines.join("\n")
}

/// the public entry point.
pub async fn run_solid_state_harness(options: SolidStateHarnessOptions) -> Result<HarnessRunResult, String> {
    let mut run = HarnessRun::new(options).await?;
    if let Some(result) = run.run_loops().await {
        return Ok(result);
    }
    Ok(run.finish().await)
}

#[cfg(test)]
mod ask_user_survey_tests {
    use super::*;
    use crate::core::types::{
        HarnessEventType, HarnessSurveyAnswer, HarnessSurveyAnswers, HarnessSurveyOption,
        HarnessSurveyQuestion, QuestionSurvey,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn survey() -> QuestionSurvey {
        QuestionSurvey {
            answers_cursor: None,
            questions: vec![HarnessSurveyQuestion {
                header: "Approach".into(),
                question: "Which approach should the run take?".into(),
                options: vec![
                    HarnessSurveyOption {
                        label: "Poll".into(),
                        description: "poll answers.jsonl".into(),
                    },
                    HarnessSurveyOption {
                        label: "Channel".into(),
                        description: "in-process channel".into(),
                    },
                ],
                allow_other: true,
            }],
        }
    }

    fn two_question_survey() -> QuestionSurvey {
        QuestionSurvey {
            answers_cursor: None,
            questions: vec![
                HarnessSurveyQuestion {
                    header: "Approach".into(),
                    question: "Poll or channel?".into(),
                    options: vec![
                        HarnessSurveyOption { label: "Poll".into(), description: "file".into() },
                        HarnessSurveyOption { label: "Channel".into(), description: "pipe".into() },
                    ],
                    allow_other: true,
                },
                HarnessSurveyQuestion {
                    header: "Scope".into(),
                    question: "Include tests?".into(),
                    options: vec![
                        HarnessSurveyOption { label: "Yes".into(), description: "with tests".into() },
                        HarnessSurveyOption { label: "No".into(), description: "without tests".into() },
                    ],
                    allow_other: false,
                },
            ],
        }
    }

    fn batch(index: i64, choice: &str) -> HarnessSurveyAnswers {
        HarnessSurveyAnswers {
            at: "2026-01-01T00:00:00Z".into(),
            answers: vec![HarnessSurveyAnswer {
                index,
                choice: Some(choice.into()),
                other: None,
            }],
        }
    }

    fn other_batch(index: i64, text: &str) -> HarnessSurveyAnswers {
        HarnessSurveyAnswers {
            at: "2026-01-01T00:00:00Z".into(),
            answers: vec![HarnessSurveyAnswer {
                index,
                choice: None,
                other: Some(text.into()),
            }],
        }
    }

    /// A HarnessRun against a temp session dir with fake monotonic clock
    /// (60s per now() call) and a 1s survey timeout: deadlines trip
    /// deterministically with no live waits.
    async fn test_run_in(
        dir: &tempfile::TempDir,
        configure: impl FnOnce(&mut SolidStateHarnessOptions),
    ) -> HarnessRun {
        std::fs::create_dir_all(dir.path().join("session")).unwrap();
        let ticks = Arc::new(AtomicUsize::new(0));
        let base = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let now: NowFn = Arc::new(move || {
            let step = ticks.fetch_add(1, Ordering::SeqCst) as i64 * 60;
            base + chrono::Duration::seconds(step)
        });
        let mut options = SolidStateHarnessOptions {
            goal: "test goal".into(),
            now: Some(now),
            state_path: Some(dir.path().join("session/state.json")),
            ask_user_enabled: true,
            ask_user_timeout_seconds: Some(1),
            ..SolidStateHarnessOptions::default()
        };
        configure(&mut options);
        HarnessRun::new(options).await.unwrap()
    }

    #[tokio::test]
    async fn timeout_persists_pending_survey_and_ends_awaiting_input() {
        let dir = tempfile::tempdir().unwrap();
        let mut run = test_run_in(&dir, |_| {}).await;
        run.state.pending_questions = Some(survey());
        let outcome = run.run_survey_block(survey()).await;
        assert!(matches!(outcome, SurveyWait::TimedOut));
        assert!(
            run.state.pending_questions.is_some(),
            "timeout must keep the pending survey for --resume"
        );
        assert!(
            run.state.pending_questions.as_ref().unwrap().answers_cursor.is_some(),
            "timeout must persist the replay cursor with the survey"
        );
        let result = run.awaiting_input_result();
        assert_eq!(result.reason, HarnessRunReason::AwaitingInput);
        assert_ne!(result.reason, HarnessRunReason::Completed);
        assert!(
            result
                .continue_command
                .as_deref()
                .unwrap_or("")
                .starts_with("drip --resume "),
            "awaiting-input result needs a usable continueCommand, got {:?}",
            result.continue_command
        );
        let loaded = crate::core::state::load_harness_state(
            &dir.path().join("session/state.json"),
        )
        .unwrap()
        .unwrap();
        assert!(
            loaded.pending_questions.is_some(),
            "pending survey must be persisted to state.json on timeout"
        );
    }

    #[tokio::test]
    async fn abort_exits_the_survey_poll_promptly() {
        let dir = tempfile::tempdir().unwrap();
        let signal = AbortSignal::new();
        signal.abort();
        let mut run = test_run_in(&dir, |o| o.signal = Some(signal)).await;
        run.state.pending_questions = Some(survey());
        let started = std::time::Instant::now();
        let outcome = run.run_survey_block(survey()).await;
        assert!(matches!(outcome, SurveyWait::Aborted));
        assert!(
            started.elapsed().as_secs() < 2,
            "abort must exit the poll promptly, took {:?}",
            started.elapsed()
        );
        assert!(run.state.pending_questions.is_some());
    }

    #[tokio::test]
    async fn resume_with_answers_consumes_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut run = test_run_in(&dir, |_| {}).await;
        run.state.pending_questions = Some(survey());
        let path = run.answers_path().unwrap();
        crate::core::state::answers::append_answers(&path, &batch(0, "Poll")).unwrap();
        run.resume_pending_survey().await;
        assert!(
            run.state.pending_questions.is_none(),
            "a matching answer batch must consume the pending survey"
        );
        let messages = run.state.operator_messages.as_ref().expect("accepted answers must record an operator feedback message");
        assert_eq!(messages.len(), 1);
        assert!(
            messages[0].text.contains("Q: Which approach should the run take?"),
            "operator feedback must carry the rendered Q->A summary, got: {}",
            messages[0].text
        );
        // Resuming again with the cursor already past the consumed line must
        // not replay the same answers.
        let cursor = crate::core::state::answers::line_count(&path).unwrap();
        run.state.pending_questions = Some(survey());
        run.state.pending_questions.as_mut().unwrap().answers_cursor = Some(cursor);
        run.resume_pending_survey().await;
        assert!(
            run.state.pending_questions.is_some(),
            "already-consumed answers must not be replayed"
        );
        assert_eq!(
            run.state.operator_messages.as_ref().map(|m| m.len()),
            Some(1),
            "exactly one feedback message despite a second resume"
        );
    }

    #[tokio::test]
    async fn resume_without_answers_reemits_the_question_event() {
        let dir = tempfile::tempdir().unwrap();
        let events: Arc<Mutex<Vec<HarnessEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let mut run = test_run_in(&dir, move |o| {
            o.on_event = Some(Arc::new(move |event| {
                sink.lock().unwrap().push(event);
            }));
        })
        .await;
        run.state.pending_questions = Some(survey());
        let path = run.answers_path().unwrap();
        std::fs::write(&path, "").unwrap();
        run.resume_pending_survey().await;
        assert!(
            run.state.pending_questions.is_some(),
            "without answers the survey must stay pending"
        );
        assert!(
            events
                .lock()
                .unwrap()
                .iter()
                .any(|event| matches!(event.r#type, HarnessEventType::Question)),
            "resume without answers must re-emit the question event"
        );
    }

    #[tokio::test]
    async fn stale_and_invalid_batches_are_not_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let mut run = test_run_in(&dir, |_| {}).await;
        run.state.pending_questions = Some(survey());
        let path = run.answers_path().unwrap();
        crate::core::state::answers::append_line(&path, "not json at all").unwrap();
        crate::core::state::answers::append_answers(&path, &batch(5, "Poll")).unwrap();
        run.resume_pending_survey().await;
        assert!(
            run.state.pending_questions.is_some(),
            "malformed and out-of-range records must be skipped, not consumed"
        );
        assert!(run.state.operator_messages.is_none());
        // A complete valid batch arriving afterwards is still consumed.
        crate::core::state::answers::append_answers(&path, &batch(0, "Channel")).unwrap();
        run.state.pending_questions = Some(survey());
        run.resume_pending_survey().await;
        assert!(
            run.state.pending_questions.is_none(),
            "a complete valid batch after invalid ones must be consumed"
        );
    }

    #[test]
    fn validate_survey_answers_rejects_out_of_range_duplicate_and_partial() {
        let survey = two_question_survey();
        assert!(crate::harness::harness_tools::validate_survey_answers(&survey, &batch(0, "Poll")).is_err());
        assert!(crate::harness::harness_tools::validate_survey_answers(&survey, &batch(2, "Poll")).is_err());
        assert!(crate::harness::harness_tools::validate_survey_answers(&survey, &batch(-1, "Poll")).is_err());
        let duplicate = HarnessSurveyAnswers {
            at: "2026-01-01T00:00:00Z".into(),
            answers: vec![
                HarnessSurveyAnswer { index: 0, choice: Some("Poll".into()), other: None },
                HarnessSurveyAnswer { index: 0, choice: Some("Channel".into()), other: None },
            ],
        };
        assert!(crate::harness::harness_tools::validate_survey_answers(&survey, &duplicate).is_err());
    }

    #[test]
    fn validate_survey_answers_enforces_allow_other_and_full_coverage() {
        let validate = crate::harness::harness_tools::validate_survey_answers;
        let survey = two_question_survey();
        // Full coverage first: one answer in a two-question survey is partial.
        assert!(validate(&survey, &other_batch(0, "do it my way")).is_err());
        assert!(validate(&survey, &batch(0, "Poll")).is_err());
        assert!(validate(&survey, &batch(1, "Yes")).is_err());
        let pair = |first: HarnessSurveyAnswer, second: HarnessSurveyAnswer| HarnessSurveyAnswers {
            at: "2026-01-01T00:00:00Z".into(),
            answers: vec![first, second],
        };
        let other = |index: i64, text: &str| HarnessSurveyAnswer { index, choice: None, other: Some(text.into()) };
        let pick = |index: i64, label: &str| HarnessSurveyAnswer { index, choice: Some(label.into()), other: None };
        // allow_other: question 0 accepts free text, question 1 does not.
        assert!(validate(&survey, &pair(other(0, "do it my way"), pick(1, "Yes"))).is_ok());
        assert!(validate(&survey, &pair(pick(0, "Poll"), other(1, "surprise me"))).is_err());
        // Choices must be listed labels.
        assert!(validate(&survey, &pair(pick(0, "Poll"), pick(1, "NotListed"))).is_err());
        assert!(validate(&survey, &pair(pick(0, "Channel"), pick(1, "No"))).is_ok());
        let empty = HarnessSurveyAnswers { at: "2026-01-01T00:00:00Z".into(), answers: vec![] };
        assert!(validate(&survey, &empty).is_err());
    }

    /// The injected operator feedback must carry the exact mandated
    /// plan-revision directive.
    #[test]
    fn ask_user_answer_directive_matches_the_mandated_text() {
        assert_eq!(
            crate::harness::prompt::ASK_USER_ANSWER_DIRECTIVE,
            "The operator answered your clarification questions. Revise the plan now with plan_tasks/revise_task to reflect these answers before continuing."
        );
    }

    /// Enabled runs append the clarification guidance; disabled runs append
    /// nothing at all — the enabled prompt must be exactly the disabled
    /// prompt plus the fragment, proving the disabled path is unchanged.
    #[tokio::test]
    async fn ask_user_guidance_is_appended_only_when_enabled() {
        let disabled_dir = tempfile::tempdir().unwrap();
        let disabled = test_run_in(&disabled_dir, |o| o.ask_user_enabled = false).await;
        assert!(
            !disabled.system_prompt.contains("Clarification questions"),
            "disabled run: the guidance fragment must be absent entirely"
        );
        let enabled_dir = tempfile::tempdir().unwrap();
        let enabled = test_run_in(&enabled_dir, |_| {}).await;
        let fragment = crate::harness::prompt::ASK_USER_GUIDANCE_FRAGMENT;
        assert_eq!(
            enabled.system_prompt,
            format!("{}\n\n{}", disabled.system_prompt, fragment),
            "enabling ask_user may only append the guidance fragment"
        );
        for mandated in [
            "planning",
            "single ask_user call",
            "best-guess option FIRST",
            "revise the plan with plan_tasks/revise_task",
        ] {
            assert!(
                enabled.system_prompt.contains(mandated),
                "guidance must instruct the model to {mandated}"
            );
        }
    }

    /// Live delivery: an answer batch appended DURING the blocking wait (the
    /// real live path, after the stale-line boundary) injects the directive-
    /// prefixed summary exactly once. The clock holds t0 for the first 8
    /// now() calls so the poll survives a few real 500 ms sleeps; a writer
    /// thread appends at ~150 ms. A pathological environment still times out
    /// via the 60 s jumps instead of hanging.
    #[tokio::test]
    async fn live_answers_inject_the_directive_prefixed_summary_once() {
        let dir = tempfile::tempdir().unwrap();
        let ticks = Arc::new(AtomicUsize::new(0));
        let clock_ticks = ticks.clone();
        let base = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let now: NowFn = Arc::new(move || {
            let step = clock_ticks.fetch_add(1, Ordering::SeqCst) as i64;
            base + chrono::Duration::seconds(if step < 8 { 0 } else { 60 * (step - 7) })
        });
        let mut run = test_run_in(&dir, move |o| o.now = Some(now)).await;
        run.state.pending_questions = Some(survey());
        let path = run.answers_path().unwrap();
        let writer_path = path.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            crate::core::state::answers::append_answers(&writer_path, &batch(0, "Channel"))
                .unwrap();
        });
        let outcome = run.run_survey_block(survey()).await;
        assert!(
            matches!(outcome, SurveyWait::Answered),
            "an answer appended during the wait must be accepted live"
        );
        assert!(run.state.pending_questions.is_none());
        let messages = run
            .state
            .operator_messages
            .as_ref()
            .expect("live answers must inject an operator feedback message");
        assert_eq!(messages.len(), 1, "live delivery must happen exactly once");
        assert!(
            messages[0].text.starts_with(crate::harness::prompt::ASK_USER_ANSWER_DIRECTIVE),
            "injected summary must start with the plan-revision directive, got: {}",
            messages[0].text
        );
        assert!(
            messages[0].text.contains("Q: Which approach should the run take?")
                && messages[0].text.contains("Channel"),
            "injected summary must render the question and the chosen answer, got: {}",
            messages[0].text
        );
    }
}

#[cfg(test)]
mod review_opt_out_tests {
    use super::*;
    use crate::core::state::{create_harness_state, start_follow_up_goal};

    #[test]
    fn detects_every_opt_out_phrase_verbatim_and_mixed_case() {
        for phrase in REVIEW_OPT_OUT_PHRASES {
            assert_eq!(detect_review_opt_out(phrase), Some(phrase.to_string()));
            let shouting = format!("PLEASE {}, THANKS", phrase.to_uppercase());
            assert_eq!(
                detect_review_opt_out(&shouting),
                Some(phrase.to_string()),
                "phrase must match case-insensitively: {phrase}"
            );
        }
    }

    #[test]
    fn ordinary_review_requests_do_not_opt_out() {
        assert_eq!(detect_review_opt_out("review the design"), None);
        assert_eq!(
            detect_review_opt_out("Please review the design doc and run the test suite."),
            None
        );
        assert_eq!(
            detect_review_opt_out("A reviewer will look at this later."),
            None
        );
        assert_eq!(
            detect_review_opt_out("The verifier should verify the claims."),
            None
        );
        assert_eq!(detect_review_opt_out(""), None);
    }

    #[test]
    fn const_list_order_picks_the_named_phrase() {
        // Detection scans the phrase list in order (not text position), so the
        // warning names a deterministic phrase regardless of wording layout.
        let (opt_out, matched) = review_opt_out_from("no verify. Also: NO REVIEWER please");
        assert!(opt_out);
        assert_eq!(matched.as_deref(), Some("no reviewer"));
        // a text with only one phrase still names that phrase
        assert_eq!(
            review_opt_out_from("please, no verify on this one").1,
            Some("no verify".to_string())
        );
    }

    #[test]
    fn opt_out_state_persists_and_old_states_default_to_none() {
        let mut state = create_harness_state("g");
        state.review_opt_out = Some(true);
        state.opt_out_warning_emitted = Some("no verify".to_string());
        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("\"reviewOptOut\":true"));
        let restored: HarnessState = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.review_opt_out, Some(true));
        assert_eq!(
            restored.opt_out_warning_emitted.as_deref(),
            Some("no verify")
        );

        // a state serialized by an older binary (without the new fields)
        // restores cleanly with the opt-out unset
        let legacy = serde_json::to_value(&create_harness_state("old goal")).unwrap();
        let old: HarnessState = serde_json::from_value(legacy).unwrap();
        assert_eq!(old.review_opt_out, None);
        assert_eq!(old.opt_out_warning_emitted, None);
    }

    #[test]
    fn defaults_omit_fields_and_follow_up_goal_clears_sticky_opt_out() {
        let mut state = create_harness_state("g");
        let clean = serde_json::to_string(&state).unwrap();
        assert!(!clean.contains("reviewOptOut"));
        assert!(!clean.contains("optOutWarningEmitted"));

        state.review_opt_out = Some(true);
        state.opt_out_warning_emitted = Some("no reviewer".to_string());
        start_follow_up_goal(&mut state, "Harden the draft: full rigor");
        assert_eq!(state.review_opt_out, None);
        assert_eq!(state.opt_out_warning_emitted, None);
    }
}
