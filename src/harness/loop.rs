// port of src/harness/loop.ts — chunk 1: pure helpers only.
//
// Constants and the stateless helpers the loop driver leans on (transcript
// folding, djb2 hashing, output spilling, footprint/verification extraction).
// The loop driver itself is ported separately.

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
        // `/\s+/g → " "` plus trim, as in the TS preview.
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

/// Fold the whole loop transcript past this size instead of waiting for the endpoint to reject it.
pub const MAX_LOOP_TRANSCRIPT_CHARS: usize = 300_000;

// Commands whose outcome IS the verification story of the run: the harness
// records the most recent one so summaries and results cite ground truth.
// Hand-rolled scan of the TS VERIFICATION_COMMAND_PATTERN:
// /\b(?:bun|npm|pnpm|yarn)\s+(?:run\s+)?(?:test|check|lint|typecheck|build)\b|\b(?:pytest|vitest|jest|tsc|cargo\s+(?:test|check)|go\s+(?:test|vet)|make\s+(?:test|check|lint))\b/
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
// Iterates UTF-16 code units to match the TS charCodeAt() exactly.
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
        // A non-array `files` throws inside the TS for..of and is caught → [].
        Some(_) => return Vec::new(),
        None => {}
    }

    paths
}

pub const MAX_FOOTPRINT_ENTRIES: usize = 20;

pub fn record_task_footprint(footprint: &mut Option<Vec<String>>, entry: &str) {
    let list = footprint.get_or_insert_with(Vec::new);

    if list.last().map(String::as_str) == Some(entry) {
        return;
    }

    list.push(entry.to_string());

    if list.len() > MAX_FOOTPRINT_ENTRIES {
        let overflow = list.len() - MAX_FOOTPRINT_ENTRIES;
        list.drain(0..overflow);
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

    // cargo test / libtest: one "running N tests" header per test binary.
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

pub fn extract_verification_command(tool_name: &str, raw_input: &str) -> Option<String> {
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

    if verification_pattern_matches(&command) {
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
    fn hash_text_is_stable_hex() {
        assert_eq!(hash_text("cargo build failed"), hash_text("cargo build failed"));
        assert_ne!(hash_text("cargo build"), hash_text("cargo build failed"));
        assert_eq!(hash_text(""), "1505");
        let digest = hash_text("boom");
        assert!(digest.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }
}
// ---------------------------------------------------------------------------
// Run driver (port of runSolidStateHarness, src/harness/loop.ts:336-1536).
//
// The TS function is one 1200-line closure nest. The Rust port keeps the
// same control flow but hoists the closure-captured state into `HarnessRun`
// (run-scoped: options, config, state, usage, model caller) and `LoopScope`
// (loop-scoped locals: transcript, budgets, digest, progress flags). Each TS
// closure becomes a method; the TS line ranges are noted on every method so
// the port can be checked side by side.
// ---------------------------------------------------------------------------

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use indexmap::IndexMap;

use crate::core::types::{
    HarnessActivationDigest, HarnessLeakedJob, HarnessLoopConfig, HarnessOperatorMessage,
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
    AbortSignal, ModelCallRecord, ModelCaller, ModelRoute, OpenAICompatibleResponse, SleepFn,
};
use crate::harness::roles::{HarnessRoleBindings, HarnessRoleRuntime};
use crate::harness::transport::OpenAICompatibleRequestTool;
use crate::harness::prompt::{
    build_cycle_continuation_message, build_iteration_messages, CycleContinuationArgs,
    HarnessLoopInfo, HarnessLoopRole, HarnessRunBudget, IterationMessagesArgs,
};
use crate::tools::types::{ChatToolDefinition, ChatToolRuntimeServices};

/// TS `RATE_LIMIT_BACKOFF_SECONDS` lives in model_call; the loop only reads
/// the defaults below (loop.ts:35-56).
pub const DEFAULT_MAX_REVIEW_ROUNDS: i64 = 2;

pub fn default_loop_config() -> HarnessLoopConfig {
    crate::core::types::DEFAULT_LOOP_CONFIG.clone()
}

pub fn default_telemetry_config() -> HarnessTelemetryConfig {
    crate::core::types::DEFAULT_TELEMETRY_CONFIG.clone()
}

/// TS `Partial<HarnessTelemetryConfig>` (options.telemetry).
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
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OperatorInboxEntry {
    pub at: Option<String>,
    pub text: String,
}

/// TS `clampLoopValue` (loop.ts:561-563) — clamp a role override between a
/// floor and the run-level cap.
pub type NowFn = Arc<dyn Fn() -> chrono::DateTime<chrono::Utc> + Send + Sync>;
pub type EmitFn = Arc<dyn Fn(HarnessEvent) + Send + Sync>;

/// Port of the TS `SolidStateHarnessOptions` (loop.ts:58-104). Every
/// optional TS field is an `Option`; function-typed fields are trait objects.
#[derive(Default)]
pub struct SolidStateHarnessOptions {
    pub cwd: Option<String>,
    pub dynamic_tool_names: Option<Vec<String>>,
    pub goal: String,
    pub goal_context: Option<String>,
    pub goal_images: Option<Vec<String>>,
    pub headers: Vec<(String, String)>,
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
    pub refresh_headers: Option<Arc<dyn Fn() -> Vec<(String, String)> + Send + Sync>>,
    pub repo_memory: Option<RepoMemoryConfig>,
    pub repo_memory_index: Option<String>,
    pub role_bindings: Option<HarnessRoleBindings>,
    pub roles: Option<Vec<HarnessRoleRuntime>>,
    pub signal: Option<AbortSignal>,
    pub sleep_impl: Option<SleepFn>,
    pub stall_limit: Option<i64>,
    pub state_path: Option<PathBuf>,
    pub summarize_run: Option<bool>,
    /// `collectRunFacts`: ground-truth workspace facts (e.g. git status) for the run summary.
    pub collect_run_facts: Option<Box<dyn FnMut() -> Option<String> + Send>>,
    pub initial_inbox_cursor: Option<i64>,
    /// `collectOperatorMessages(consumedCount)`: steering that arrived while the run executes.
    pub collect_operator_messages: Option<Box<dyn FnMut(i64) -> Vec<OperatorInboxEntry> + Send>>,
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
/// hooks: the TS closures capture the run scope directly; the Rust caller
/// holds `Arc<dyn Fn>`s that cannot borrow `HarnessRun`, so they post into
/// this inbox and the loop drains it right after every call_model.
#[derive(Default)]
pub struct UsageInbox {
    pub usages: Mutex<Vec<(Option<crate::harness::model_call::OpenAICompatibleResponseUsage>, ModelCallRecord)>>,
    pub retry_waits: Mutex<Vec<f64>>,
    pub iteration: std::sync::atomic::AtomicI64,
}

/// Run-scoped state: everything the TS closures capture from the enclosing
/// runSolidStateHarness scope (loop.ts:336-600).
pub struct HarnessRun {
    pub usage_inbox: Arc<UsageInbox>,
    pub options: SolidStateHarnessOptions,
    pub now: NowFn,
    pub emit_fn: EmitFn,
    pub run_started_at_ms: i64,
    /// Stamped when a stop was requested (loop.ts:343-350).
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
    // Outer-loop locals (loop.ts:594-602).
    pub idle_loops: i64,
    pub escalations_without_progress: i64,
    pub run_futile: bool,
    pub aborted: bool,
    pub run_error: Option<String>,
    pub plan_stopped: bool,
}

/// Loop-scoped locals (loop.ts:660-745): one instance per task loop.
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
    pub used_tool_call_ids: HashSet<String>,
    pub affordable_cycles: i64,
    pub task_finished: bool,
    pub made_progress: bool,
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

/// Outcome of one cycle's tool-round loop (the `break`/`continue` targets of
/// the TS `for (round ...)` body).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoundOutcome {
    /// Keep going with the next round.
    Continue,
    /// The TS `break` out of the round loop (natural conclusion, task finished, overflow).
    Break,
    /// `aborted = true; break`.
    Aborted,
}

/// A tool call after id de-duplication (loop.ts:1013-1027).
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

/// JS `Number.prototype.toLocaleString()` for a char count: digits with
/// comma thousands separators (en-US, which is what lci's callers see).
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

/// Result of `executeWorkspaceTool` (loop.ts:547-578).
pub struct WorkspaceToolExecution {
    pub failed: bool,
    pub tool_content: String,
}

/// loop.ts:311-322 — wrap the caller-supplied workspace tools as transport specs.
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

/// harness-tools.ts:148-190 — the harness (framework) tool specs. Starts from
/// `harness_tool_definitions()` JSON converted with create_request_tool; when
/// roles are configured, plan_tasks gains the `role` enum property exactly as
/// the TS spread does (property order dependsOn, role, title).
fn build_harness_tool_specs(role_names: &[String]) -> Vec<OpenAICompatibleRequestTool> {
    let mut specs: Vec<OpenAICompatibleRequestTool> =
        crate::harness::harness_tools::harness_tool_definitions()
            .iter()
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
                    // Re-insert title after role so key order matches the TS
                    // object spread (dependsOn, role, title).
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

/// loop.ts:370-372 — a zero/negative budget would spin the outer run loop
/// without ever calling the model or advancing the iteration counter. i64 is
/// always finite (the TS NaN fallback cannot occur), so only the floor
/// applies; `fallback` is kept for signature parity with the TS helper.
fn clamp_loop_value(value: i64, minimum: i64, fallback: i64) -> i64 {
    let _ = fallback;
    value.max(minimum)
}

impl HarnessRun {
    /// loop.ts:336-556 — resolve options into run-scoped config, load or
    /// create the state, build the role map / harness tool specs / transport
    /// tools, and create the model caller.
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

        // { ...DEFAULT_LOOP_CONFIG, ...options.loop, maxToolRoundsPerCycle from
        // maxToolRoundsPerIteration when the loop block does not set it }
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
        let system_prompt = options
            .system_prompt
            .clone()
            .unwrap_or_else(|| crate::harness::prompt::DEFAULT_HARNESS_SYSTEM_PROMPT.to_string());

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
                            crate::harness::harness_tools::HarnessRoleSpec { verified_by: role.verified_by.clone() },
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
        let harness_tool_specs = build_harness_tool_specs(&role_names);
        let mut default_transport_tools = build_transport_tools(&tools);
        default_transport_tools.extend(harness_tool_specs.iter().cloned());

        let mut headers = options.headers.clone();
        if !headers.iter().any(|(key, _)| key.eq_ignore_ascii_case("content-type")) {
            headers.insert(0, ("content-type".to_string(), "application/json".to_string()));
        }

        // loop.ts:476-485 — `options.toolServices ?? createChatToolRuntimeServices({ cwd })`:
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
        // options.initialState bypasses loadHarnessState's migrations: the TS
        // repairs a missing loop clock here (state.loop = iteration when NaN);
        // the Rust loop field is always numeric, so nothing to repair.
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
            aborted: false,
            run_error: None,
            plan_stopped: false,
        })
    }

    /// loop.ts:348-350
    pub fn stop_latency_ms(&self) -> Option<i64> {
        self.abort_requested_at_ms
            .map(|requested| ((self.now)().timestamp_millis() - requested).max(0))
    }

    /// loop.ts:368-371
    pub fn finalize_usage(&mut self) -> HarnessRunUsage {
        self.run_usage.wall_ms = ((self.now)().timestamp_millis() - self.run_started_at_ms).max(0);
        self.run_usage.clone()
    }

    /// loop.ts:373-420 — accumulate usage and emit the `inference` event.
    pub fn record_model_usage(
        &mut self,
        usage: Option<&crate::harness::model_call::OpenAICompatibleResponseUsage>,
        call: &ModelCallRecord,
    ) {
        // TS truthiness guards: `if (taskId)` / `...(call.provider ? ... : {})`
        // treat an empty string like undefined.
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

    /// loop.ts:422-425
    pub fn record_retry_wait(&mut self, wait_seconds: f64) {
        self.run_usage.retries += 1;
        self.run_usage.rate_limit_wait_seconds += wait_seconds;
    }

    /// Drain the caller's usage/retry hooks posted during the last call_model
    /// (the TS onUsage/onRetryWait callbacks run inside callModel itself).
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

    /// loop.ts:499-501
    pub fn emit(&self, event: HarnessEvent) {
        (self.emit_fn)(event)
    }

    /// loop.ts:503-507 — persist the state when a state path is configured.
    pub fn persist(&self) {
        if let Some(state_path) = &self.options.state_path {
            let _ = crate::core::state::save_harness_state(state_path, &self.state);
        }
    }

    /// loop.ts:509-522
    pub fn aborted_result(&mut self) -> HarnessRunResult {
        self.persist();
        HarnessRunResult {
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

    /// loop.ts:547-578 — run one workspace tool through the tool framework
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
        // TS `(args.registry ?? toolRegistry).get(args.toolName)`: resolve the
        // tool by name, then treat it as absent when this loop's role
        // restricts tools and the resolved index is not in the allowed list —
        // a disallowed call fails cleanly inside `execute_tool_call`.
        let tool = self
            .tool_registry
            .get(tool_name)
            .copied()
            .filter(|index| registry.map_or(true, |allowed| allowed.contains(index)))
            .and_then(|index| self.tools.get(index));

        let executed = execute_tool_call(ToolExecutionContext {
            call_id,
            history: &[],
            message: &iteration_message,
            raw_input,
            runtime_context: ChatRuntimeContext {
                cwd: self.cwd.clone(),
                // TS passes the run's runtimeContext; the no-file default state.
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

        WorkspaceToolExecution {
            failed: executed
                .blocks
                .iter()
                .any(|block| matches!(block, ChatMessageBlock::ToolCall(block) if block.status == ToolCallStatus::Failed)),
            // The one choke point every consumer shares: context injection,
            // telemetry, transcript events, and the NDJSON stream all read this.
            tool_content: (self.redact)(&executed.tool_content),
        }
    }

    /// loop.ts:580-592 — re-run dynamic warm-context entries at loop start.
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

    /// loop.ts:604-1250 — the outer `while` loop: one task loop per iteration
    /// of this method's loop, delegating to `begin_loop` / `run_cycle` /
    /// `end_loop`. Returns Some(result) when the run ends inside the loop
    /// (abort/error paths); None when the outer loop exits normally.
    pub async fn run_loops(&mut self) -> Option<HarnessRunResult> {
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

                if self.run_error.is_some() || self.aborted {
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

            // The TS try/catch around the cycles: a thrown run error aborts the
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

            if self.aborted {
                return Some(self.aborted_result());
            }

            self.after_loop(&scope);

            if self.run_futile {
                break;
            }
        }

        None
    }

    /// The TS abort listener (loop.ts:345-347): the first time a stop is
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

    /// loop.ts:660-745 — pick up the current task, resolve the loop role,
    /// derive the loop budget, emit `loop-start`, refresh dynamic entries and
    /// build the loop scope.
    pub fn begin_loop(&mut self) -> LoopScope {
        // loop.ts:661-667 — pending → in_progress; activations counts pickups.
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

        // loop.ts:669-677 — this loop's capability profile.
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
        // loop.ts:678 — filterToolsForRole.
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
        // loop.ts:679-681 — transport tools.
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
        // loop.ts:682-692 — role loop budget clamps.
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

        // loop.ts:694-701 — the loop-start event.
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

        // loop.ts:703 — refresh dynamic warm-context entries.
        self.refresh_dynamic_entries();

        // loop.ts:687-690 — cycles this loop can actually afford.
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
            used_tool_call_ids: HashSet::new(),
            affordable_cycles,
            task_finished: false,
            made_progress: false,
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

    /// loop.ts:704-737 — `recordLoopDigest(outcome)`.
    pub fn record_loop_digest(&mut self, scope: &mut LoopScope, outcome: &str) {
        let current_task_id = scope.current_task_id.clone();
        let cycles_run = scope.cycles_run;
        let task_finished = scope.task_finished;

        // loop.ts:706-715 — lastActivation with actions overflow handling.
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

        // loop.ts:718-737 — a task that keeps failing needs its earlier
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

    /// loop.ts:748-868 — cycle start: budget/abort checks, operator
    /// messages, `iteration-start`, transport messages for cycle 1 or the
    /// continuation message + fold for later cycles. Returns false when the
    /// cycle must not run (budget exhausted / aborted).
    pub fn begin_cycle(&mut self, scope: &mut LoopScope, cycle: i64) -> bool {
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

    /// loop.ts:870-1010 — one tool round: fold on transcript size, call the
    /// model (with the context-overflow retry), then parse the reply: a
    /// text-only reply concludes (or is nudged once); tool calls are
    /// normalized and dispatched via `dispatch_tool_calls`.
    pub async fn run_round(&mut self, scope: &mut LoopScope, cycle: i64, round: i64) -> RoundOutcome {
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

        // loop.ts:1029-1037 — replay the assistant turn. Native Anthropic
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

        self.dispatch_tool_calls(scope, normalized_calls);

        RoundOutcome::Continue
    }

    /// loop.ts:1013-1027 — de-duplicate tool call ids and normalize.
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

    /// loop.ts:1039-1212 — execute each normalized call: skipped-after-end,
    /// harness ops, or workspace tools (verification tracking, spill,
    /// telemetry, footprint, tool-call/tool-result events), appending the
    /// tool-role messages to the transcript.
    pub fn dispatch_tool_calls(&mut self, scope: &mut LoopScope, calls: Vec<NormalizedCall>) {
        for call in calls {
            let NormalizedCall { call_id, raw_input, tool_name, .. } = call;

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
                };
                let outcome = match op {
                    Ok(op) => apply_harness_op(&mut self.state, op, &op_context),
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
            let execution = self.execute_workspace_tool(&call_id, &raw_input, Some(&scope.loop_tool_indexes), &tool_name);
            let execution_duration_ms = (self.now)().timestamp_millis() - execution_started_at_ms;

            // A successful workspace mutation counts as task progress even if
            // finish_task is not called this loop, so the stall counter does
            // not increment for loops that land real edits.
            if !execution.failed
                && self
                    .tool_registry
                    .get(&tool_name)
                    .map(|index| self.tools[*index].mutates_workspace)
                    .unwrap_or(false)
            {
                scope.made_progress = true;
                self.state.mutations_since_verification = Some(self.state.mutations_since_verification.unwrap_or(0) + 1);
                self.state.workspace_edits = Some(self.state.workspace_edits.unwrap_or(0) + 1);
            }

            let verification_command = extract_verification_command(&tool_name, &raw_input);
            if let Some(verification_command) = verification_command.clone() {
                let truncated_command = truncate_text(&verification_command, 200);
                let output_tail = truncate_text_keeping_ends(&execution.tool_content, 500);
                let ran_no_tests =
                    !execution.failed && detect_empty_test_run(&verification_command, &execution.tool_content);
                let verification_record = HarnessVerificationRecord {
                    at_iteration: self.state.iteration,
                    command: truncated_command.clone(),
                    failed: execution.failed,
                    output_tail: output_tail.clone(),
                    ran_no_tests: ran_no_tests.then_some(true),
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
                            "{}\n\n[harness] {} chars total — the full output is saved at {}; READ or GREP it instead of re-running the command.",
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

            // Identical re-runs get one line of feedback in the tool result.
            if prior_call_count > 0 {
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

            scope.digest_actions.push(format!(
                "{} {}{}",
                tool_name,
                truncate_text(&canonicalize_tool_input(&raw_input), MAX_DIGEST_ACTION_CHARS),
                if execution.failed { " (failed)" } else { "" }
            ));

            if let Some(task_id) = scope.current_task_id.clone() {
                if !execution.failed && tool_name == "PATCH" {
                    let patched = extract_patched_paths(&raw_input);
                    if let Some(task) = core_state::get_task_by_id_mut(&mut self.state, &task_id) {
                        for patched_path in patched {
                            record_task_footprint(&mut task.footprint, &format!("edited {patched_path}"));
                        }
                    }
                }
                if let Some(command) = verification_command.as_deref() {
                    let entry = format!(
                        "ran {} -> {}",
                        truncate_text(command, 120),
                        match &self.state.last_verification {
                            Some(record) => core_state::describe_verification_outcome(record.failed, record.ran_no_tests),
                            None => core_state::describe_verification_outcome(execution.failed, None),
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

    /// loop.ts:1225-1250 — the loop digest outcome text + abort/budget checks.
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

        self.record_loop_digest(scope, &outcome);
    }

    /// loop.ts:1250-1420 — stall accounting, auto-block, reopen/drop
    /// escalation, futile detection, telemetry maintenance, observation decay.
    /// loop.ts:1250-1420 — stall accounting, auto-block, reopen/drop
    /// escalation, futile detection, telemetry maintenance, observation decay.
    pub fn after_loop(&mut self, scope: &LoopScope) {
        // loop.ts:1310-1311 — a loop cut short by the run budget never got
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
            // TS `break` — the outer loop ends before telemetry maintenance;
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

    /// loop.ts:1422-1536 — decide the run reason, generate the run summary,
    /// report leaked tmux jobs, emit `run-complete`, persist, build the result.
    pub async fn finish(mut self) -> HarnessRunResult {
        if self.aborted {
            return self.aborted_result();
        }

        let start_iteration = self.start_iteration;

        let reason: HarnessRunReason = if self.run_error.is_some() {
            HarnessRunReason::Error
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
            } else {
                HarnessRunReason::Completed
            }
        } else {
            HarnessRunReason::MaxIterations
        };

        if self.options.summarize_run.unwrap_or(true)
            && (self.state.iteration > start_iteration || self.run_error.is_some())
        {
            self.generate_run_summary(reason).await;
        }

        // Background jobs the run started and never tore down: report them so
        // the driver knows a dev server/watcher is still holding the port
        // (and how to kill it) instead of discovering it three runs later.
        // Background tmux jobs that outlived the run (loop.ts:1494-1512).
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
            HarnessRunReason::Aborted => "aborted",
            HarnessRunReason::Completed => "completed",
            HarnessRunReason::Error => "error",
            HarnessRunReason::Futile => "futile",
            HarnessRunReason::MaxIterations => "max-iterations",
            HarnessRunReason::Partial => "partial",
            HarnessRunReason::Planned => "planned",
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
        self.persist();

        HarnessRunResult {
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
    /// Port of src/harness/run-summary.ts — one tool-free model call that
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

    /// run-summary.ts `record(text)`: persist on state and emit `run-summary`.
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

/// loop.ts:336 — the public entry point.
pub async fn run_solid_state_harness(options: SolidStateHarnessOptions) -> Result<HarnessRunResult, String> {
    let mut run = HarnessRun::new(options).await?;
    if let Some(result) = run.run_loops().await {
        return Ok(result);
    }
    Ok(run.finish().await)
}
