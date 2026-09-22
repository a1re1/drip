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
/// Digest lines carried into the next loop's last_activation. Every tool
/// call the loop made gets one (see `compact_tool_input`), so a loop that
/// explored 20-30 files hands the next loop the list instead of losing it
/// at the transcript reset; 40 keeps a three-cycle loop's calls in view.
pub const MAX_DIGEST_ACTIONS: usize = 40;
pub const MAX_DIGEST_ACTION_CHARS: usize = 200;
/// Extra cycles a task loop may earn past its `max_cycles` budget: one per
/// cycle that edited or verified the workspace, so a loop in the middle of
/// productive work is not reset (transcript discarded, workspace re-read)
/// just because its fixed budget ran out. Loops that only read never extend.
pub const MAX_CYCLE_EXTENSIONS: i64 = 2;
/// A single-task run whose goal-declared check the harness ran and passed
/// after the last edit skips the reviewer loop when the change outside test
/// files (tracked diff plus new files) is at most this many lines. Test
/// lines count toward REVIEW_WAIVER_MAX_TOTAL_LINES only: the check the
/// waiver rests on just ran them, and on the recorded bench every reviewer
/// loop that fired did so on 110–161 total lines of which 38–75 were code,
/// confirmed 7 of 7 with no finding, and in 5 of 7 re-ran the check.
pub const REVIEW_WAIVER_MAX_LINES: usize = 100;
/// The whole change, tests included, must still fit this many lines for the
/// waiver.
pub const REVIEW_WAIVER_MAX_TOTAL_LINES: usize = 500;
/// Output lines carried in a harness report of a settled background job.
pub const BACKGROUND_REPORT_TAIL_LINES: i64 = 40;
pub const BACKGROUND_REPORT_MAX_CHARS: usize = 4_000;
pub const BACKGROUND_REPORT_PREFIX: &str = "harness:";
pub const MAX_RESULT_EVENT_CHARS: usize = 2000;
pub const FOLDED_RESULT_MARKER: &str = "[folded]";
pub const MAX_FOLDED_PREVIEW_CHARS: usize = 240;

/// The freshest READ of up to this many distinct files is kept unfolded past
/// the hot window (see `fold_cold_tool_results`). Transcript audits of the
/// hundreds-of-cycles regime show the model re-reading one unchanged file every
/// round once its earlier read folds out of the hot window (a 273-round session
/// re-read a single file 274 times, 153 of them in consecutive no-edit rounds):
/// keeping current file state visible removes the whole re-read cycle.
pub const MAX_PINNED_READ_FILES: usize = 5;
/// A read result larger than this is not worth pinning — retaining it would
/// spend more context than the re-read it saves — so it folds normally.
pub const PIN_MAX_READ_CHARS: usize = 8_000;

/// The file path a READ tool result names, from its
/// `Read lines A-B of N from <path>.` header (path may contain dots; only the
/// single trailing period is stripped).
fn read_result_path(text: &str) -> Option<String> {
    let first_line = text.lines().next()?;
    let rest = first_line.strip_prefix("Read lines ")?;
    let from = rest.rfind(" from ")?;
    let path = rest[from + " from ".len()..].trim_end().trim_end_matches('.');
    (!path.is_empty()).then(|| path.to_string())
}

/// Paths a PATCH tool result wrote, read from the `+++ b/<path>` lines of the
/// unified diff it echoes (covers single- and multi-file patches). Used to
/// refuse pinning a read the file has since moved past.
fn patch_result_paths(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| line.strip_prefix("+++ b/").map(str::to_string))
        .collect()
}

/// (message index, command text) for every mutating BASH tool call in the
/// transcript. A file edited through the shell (`sed -i`, `> file`, `tee`)
/// leaves no PATCH diff, so a read of that path must not be pinned if a later
/// mutating command names it — the read would show pre-edit content.
fn mutating_shell_commands(messages: &[TransportRequestMessage]) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        let Some(calls) = &message.tool_calls else { continue };
        for call in calls {
            let Some(function) = &call.function else { continue };
            if function.name.as_deref() != Some("BASH") {
                continue;
            }
            let command = function
                .arguments
                .as_deref()
                .and_then(|args| serde_json::from_str::<serde_json::Value>(args).ok())
                .and_then(|value| value.get("command").and_then(|c| c.as_str()).map(str::to_string));
            if let Some(command) = command {
                if !is_read_only_shell_command(&command) {
                    out.push((index, command));
                }
            }
        }
    }
    out
}

// Folds tool results older than the hot window into one-line digests, so a
// task loop's transcript cannot grow without bound across its cycles. The most
// recent results stay verbatim ("hot"); telemetry keeps an ends-kept copy of
// each call's last result capped at maxPromotedOutputChars, so folded
// information stays recoverable up to that bound without re-running the call.
// Folded state is tracked structurally in foldedIndexes (messages only ever
// append within a loop, so indexes are stable) — never inferred from content,
// which a tool output could accidentally imitate.
//
// `max_pinned_reads` keeps the freshest READ of up to that many distinct files
// unfolded even once it falls out of the hot window, so current file state
// stays visible and the model does not re-read it every round; a read the file
// has since been PATCHed past — or that a later mutating shell command names —
// is never pinned (it would show stale content).
// Pass 0 to disable (overflow-recovery and carryover paths, which must shrink).
pub fn fold_cold_tool_results(
    messages: &mut [TransportRequestMessage],
    hot_tool_results: usize,
    folded_indexes: &mut HashSet<usize>,
    max_pinned_reads: usize,
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
    let mut indexes_to_fold: Vec<usize> = unfolded_tool_indexes[..fold_count].to_vec();

    if max_pinned_reads > 0 {
        // Latest PATCH index per path, so a read superseded by an edit is not pinned.
        let mut patched_at: HashMap<String, usize> = HashMap::new();
        for (index, message) in messages.iter().enumerate() {
            if message.name.as_deref() == Some("PATCH") {
                for path in patch_result_paths(message_text(message)) {
                    patched_at.insert(path, index);
                }
            }
        }
        // Freshest still-current READ per path among the results that would fold.
        let mut freshest_read: HashMap<String, usize> = HashMap::new();
        for &index in &indexes_to_fold {
            if messages[index].name.as_deref() != Some("READ") {
                continue;
            }
            let text = message_text(&messages[index]);
            if text.chars().count() > PIN_MAX_READ_CHARS {
                continue;
            }
            if let Some(path) = read_result_path(text) {
                if patched_at.get(&path).is_some_and(|&patch_index| patch_index > index) {
                    continue; // the file moved on after this read
                }
                freshest_read.insert(path, index); // ascending scan keeps the latest
            }
        }
        // Drop any read a later mutating shell command may have written: the
        // shell leaves no PATCH diff, so this is the only signal that a pinned
        // read would now be stale.
        let shell_edits = mutating_shell_commands(messages);
        freshest_read.retain(|path, read_index| {
            !shell_edits
                .iter()
                .any(|(command_index, command)| *command_index > *read_index && command.contains(path.as_str()))
        });
        let mut pins: Vec<usize> = freshest_read.into_values().collect();
        pins.sort_unstable_by(|a, b| b.cmp(a)); // most-recently-read first
        pins.truncate(max_pinned_reads);
        let pinned: HashSet<usize> = pins.into_iter().collect();
        indexes_to_fold.retain(|index| !pinned.contains(index));
    }

    for &index in &indexes_to_fold {
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

    indexes_to_fold.len()
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

/// What earned the cycle-budget extension for a cycle, rendered into the
/// "cycle budget extended" harness event.
pub fn describe_cycle_progress(edit: bool, verification: bool) -> String {
    match (edit, verification) {
        (true, true) => "edits and verification this cycle".to_string(),
        (true, false) => "edits this cycle".to_string(),
        (false, true) => "verification this cycle".to_string(),
        (false, false) => "edited or verified the workspace".to_string(),
    }
}

/// The tail of a loop transcript worth replaying into the next loop for the
/// same task: whole assistant→tool exchanges (never a dangling tool result)
/// covering at most `hot_tool_results` unfolded tool results and
/// MAX_CARRYOVER_CHARS, with the loop's own user/system messages left out.
/// Empty when the loop made no tool calls.
#[cfg(test)]
mod cycle_progress_tests {
    use super::describe_cycle_progress;

    #[test]
    fn cycle_progress_detail_names_edits_verification_or_both() {
        assert_eq!(describe_cycle_progress(true, false), "edits this cycle");
        assert_eq!(
            describe_cycle_progress(false, true),
            "verification this cycle"
        );
        assert_eq!(
            describe_cycle_progress(true, true),
            "edits and verification this cycle"
        );
    }
}

#[cfg(test)]
mod goal_check_tests {
    use super::{expand_patch_finishes, explicit_finish_check, finish_after_failed_call, finish_recheck_reason, goal_declared_check_commands, FinishRecheck, NormalizedCall};

    #[test]
    fn a_failed_check_is_rechecked_only_after_an_edit() {
        let failed = "harness: not accepted yet — this task edited the workspace but the most recent verification (python3 -m unittest) FAILED and nothing has passed since.";
        assert_eq!(finish_recheck_reason(failed, 1), Some(FinishRecheck::Stale));
        assert_eq!(finish_recheck_reason(failed, 0), None);
        let stale = "harness: not accepted yet — this task edited the workspace but 2 workspace edit(s) landed after the last verification (cargo test).";
        assert_eq!(finish_recheck_reason(stale, 2), Some(FinishRecheck::Stale));
        assert_eq!(finish_recheck_reason(stale, 0), None);
        let unchecked = "harness: not accepted yet — this task edited the workspace but no verification command (test/build/typecheck) has run at any point in this run.";
        assert_eq!(finish_recheck_reason(unchecked, 0), Some(FinishRecheck::Unchecked));
        assert_eq!(finish_recheck_reason("Task task-1 marked completed.", 3), None);
        let zero = "harness: not accepted yet — this task edited the workspace but the most recent verification (cargo test rect::tests 2>&1) exited green but executed zero tests — run the suite that actually covers this change.";
        assert_eq!(finish_recheck_reason(zero, 1), Some(FinishRecheck::Stale));
        assert_eq!(finish_recheck_reason(zero, 0), None);
    }

    #[test]
    fn a_patch_carrying_finish_expands_into_a_finish_task_after_the_edit() {
        let call = |id: &str, name: &str, raw: &str| NormalizedCall {
            call_id: id.to_string(),
            normalized: crate::harness::transport::OpenAICompatibleToolCall {
                id: Some(id.to_string()),
                tool_type: Some("function".to_string()),
                function: Some(crate::harness::transport::OpenAICompatibleToolCallFunction { name: Some(name.to_string()), arguments: Some(raw.to_string()) }),
            },
            raw_input: raw.to_string(),
            tool_name: name.to_string(),
        };
        let mut used: std::collections::HashSet<String> = ["c1".to_string(), "c1-finish".to_string()].into_iter().collect();
        let calls = vec![
            call("c0", "READ", r#"{"path":"a.py"}"#),
            call("c1", "PATCH", r#"{"path":"a.py","find":"x","replace":"y","finish":{"summary":"Renamed x.","check":"python3 -m unittest"}}"#),
        ];
        let (calls, expanded) = expand_patch_finishes(calls, &mut used);
        assert_eq!(expanded, 1);
        assert_eq!(calls.iter().map(|c| c.tool_name.as_str()).collect::<Vec<_>>(), ["READ", "PATCH", "finish_task"]);
        let patch: serde_json::Value = serde_json::from_str(&calls[1].raw_input).unwrap();
        assert!(patch.get("finish").is_none(), "the PATCH runs without the key");
        assert_eq!(calls[1].normalized.function.as_ref().unwrap().arguments.as_deref(), Some(calls[1].raw_input.as_str()));
        let finish: serde_json::Value = serde_json::from_str(&calls[2].raw_input).unwrap();
        assert_eq!(finish["status"], "completed");
        assert_eq!(finish["summary"], "Renamed x.");
        assert_eq!(finish["check"], "python3 -m unittest");
        assert_eq!(calls[2].call_id, "c1-finishx", "a taken id is extended");
        assert!(used.contains("c1-finishx"));

        let (calls, expanded) = expand_patch_finishes(vec![call("c2", "PATCH", r#"{"path":"a.py","content":"z","finish":{"summary":"  "}}"#)], &mut used);
        assert_eq!(expanded, 0, "an empty finish is dropped, not expanded");
        assert_eq!(calls.len(), 1);
        assert!(!calls[0].raw_input.contains("finish"));

        // A finish nested in files[] beside a no-op entry, and an identical
        // find/replace at the top level: both are the finish alone.
        let (calls, expanded) = expand_patch_finishes(
            vec![call("c4", "PATCH", r#"{"files":[{"path":"a.py","find":"same","replace":"same","finish":{"summary":"Done.","check":"cargo test -q"}}]}"#)],
            &mut used,
        );
        assert_eq!(expanded, 1);
        assert_eq!(calls.iter().map(|c| (c.tool_name.as_str(), c.call_id.as_str())).collect::<Vec<_>>(), [("finish_task", "c4")]);
        let (calls, _) = expand_patch_finishes(
            vec![call("c5", "PATCH", r#"{"path":"a.py","find":"x","replace":"x","finish":{"summary":"Done."}}"#)],
            &mut used,
        );
        assert_eq!(calls[0].tool_name, "finish_task");
        // A real edit beside a no-op entry keeps the edit and the finish.
        let (calls, _) = expand_patch_finishes(
            vec![call("c6", "PATCH", r#"{"files":[{"path":"__noop__","content":"summary text"},{"path":"a.py","find":"x","replace":"y"}],"finish":{"summary":"Done."}}"#)],
            &mut used,
        );
        assert_eq!(calls.iter().map(|c| c.tool_name.as_str()).collect::<Vec<_>>(), ["PATCH", "finish_task"]);
        let files: serde_json::Value = serde_json::from_str(&calls[0].raw_input).unwrap();
        assert_eq!(files["files"].as_array().unwrap().len(), 1);

        // Nothing to edit: the PATCH is the finish, under its own id.
        let (calls, expanded) = expand_patch_finishes(vec![call("c3", "PATCH", r#"{"path":"a.py","files":[],"finish":{"summary":"Done.","check":"cargo test -q"}}"#)], &mut used);
        assert_eq!(expanded, 1);
        assert_eq!(calls.iter().map(|c| (c.tool_name.as_str(), c.call_id.as_str())).collect::<Vec<_>>(), [("finish_task", "c3")]);
        assert_eq!(calls[0].normalized.function.as_ref().unwrap().name.as_deref(), Some("finish_task"));
        assert!(!used.contains("c3-finish"));
    }

    #[test]
    fn a_completed_finish_after_a_failed_call_in_the_same_response_is_bounced() {
        let failed = vec!["PATCH".to_string(), "PATCH".to_string()];
        let bounce = finish_after_failed_call(r#"{"status":"completed","summary":"done"}"#, &failed).expect("bounced");
        assert!(bounce.starts_with("harness: not accepted — PATCH failed earlier in this same response"), "{bounce}");
        assert!(finish_after_failed_call(r#"{"summary":"done"}"#, &failed).is_some(), "a missing status means completed");
        assert_eq!(finish_after_failed_call(r#"{"status":"blocked","summary":"stuck"}"#, &failed), None);
        assert_eq!(finish_after_failed_call(r#"{"status":"completed"}"#, &[]), None);
    }

    #[test]
    fn a_finish_names_its_check_only_with_a_short_non_blank_string() {
        assert_eq!(explicit_finish_check(r#"{"status":"completed","check":" cargo test -q "}"#).as_deref(), Some("cargo test -q"));
        assert!(explicit_finish_check(r#"{"status":"completed","check":"   "}"#).is_none());
        assert!(explicit_finish_check(r#"{"status":"completed","check":3}"#).is_none());
        assert!(explicit_finish_check(r#"{"status":"completed"}"#).is_none());
        assert!(explicit_finish_check("not json").is_none());
        let long = format!(r#"{{"check":"{}"}}"#, "x".repeat(400));
        assert!(explicit_finish_check(&long).is_none());
    }

    #[test]
    fn narration_completes_only_a_verified_workspace() {
        use super::{narration_reads_as_completion, verified_after_last_edit};
        use crate::core::types::{HarnessState, HarnessVerificationRecord};
        assert!(narration_reads_as_completion("The `count` subcommand is added with `--prefix` and the unit tests pass."));
        assert!(!narration_reads_as_completion("Done?"));
        assert!(!narration_reads_as_completion("Next I will add the tests for the prefix path."));
        assert!(!narration_reads_as_completion("The server starts but the suite is still failing on port reuse."));
        assert!(!narration_reads_as_completion("ok"));
        let edited = vec!["kvstore/cli.py".to_string()];
        let goal = "Add a `count` subcommand to kvstore/cli.py and a unit test in tests/test_cli.py.";
        assert!(!super::goal_named_paths_all_edited(goal, &edited));
        let edited = vec!["kvstore/cli.py".to_string(), "tests/test_cli.py".to_string()];
        assert!(super::goal_named_paths_all_edited(goal, &edited));
        assert!(super::goal_named_paths_all_edited("Make the suite pass.", &[]));
        let mut state = HarnessState::default();
        assert!(!verified_after_last_edit(&state), "no edits");
        state.workspace_edits = Some(1);
        state.mutations_since_verification = Some(0);
        assert!(!verified_after_last_edit(&state), "no record");
        let mut record = HarnessVerificationRecord {
            at_iteration: 1,
            command: "python3 -m unittest discover -s tests -q".to_string(),
            failed: false,
            output_tail: String::new(),
            ran_no_tests: None,
            evidence: Some(crate::core::types::VerificationEvidence {
                anchor: Some(crate::core::types::VerificationAnchor {
                    kind: crate::core::types::VerificationAnchorKind::External,
                    source: Some("goal-declared acceptance check, run by the harness after an edit".to_string()),
                    downgraded_reason: None,
                    coverage: None,
                    expectation_subject: None,
                }),
                kind: crate::core::types::VerificationEvidenceKind::Tests,
                executed: 3, passed: 3, failed: 0, skipped: None, detail: None,
            }),
            id: None,
        };
        state.last_verification = Some(record.clone());
        assert!(verified_after_last_edit(&state));
        state.mutations_since_verification = Some(1);
        assert!(!verified_after_last_edit(&state), "edited since");
        state.mutations_since_verification = Some(0);
        record.failed = true;
        state.last_verification = Some(record.clone());
        assert!(!verified_after_last_edit(&state), "failed check");
        record.failed = false;
        record.evidence.as_mut().unwrap().anchor.as_mut().unwrap().kind = crate::core::types::VerificationAnchorKind::SelfAuthored;
        state.last_verification = Some(record);
        assert!(!verified_after_last_edit(&state), "self-authored probe is not the goal's check");
    }

    #[test]
    fn edit_check_runs_for_fast_or_interpreted_checks_only() {
        use super::{edit_check_allowed, edit_check_note};
        assert!(edit_check_allowed("python3 -m unittest discover -s tests -q", None, false));
        assert!(edit_check_allowed("npm test", None, false));
        assert!(!edit_check_allowed("cargo test -q", None, false), "compile-first runner, unmeasured");
        assert!(edit_check_allowed("cargo test -q", None, true), "unmeasured, but the warm-up build is done");
        assert!(!edit_check_allowed("cargo test -q", Some(super::EDIT_CHECK_MAX_KNOWN_MS + 1), true), "a measured slow check stays skipped");
        use super::check_duration_measurable;
        assert!(!check_duration_measurable(Some("cargo test features --no-run --quiet"), false, "cargo test features::"), "compiling: not a measure of the check");
        assert!(check_duration_measurable(Some("cargo test features --no-run --quiet"), true, "cargo test features::"));
        assert!(check_duration_measurable(Some("cargo test --no-run"), false, "python3 -m unittest"), "another runner is unaffected");
        assert!(check_duration_measurable(None, false, "cargo test"));
        assert!(edit_check_allowed("cargo test -q", Some(3_000), false), "measured fast");
        assert!(!edit_check_allowed("python3 -m pytest", Some(9_000), false), "measured slow");
        let passed = edit_check_note("pytest -q", true, "passed", "");
        assert!(passed.starts_with("\n\n[harness] ran the goal-declared check after this edit: pytest -q -> passed."), "{passed}");
        assert!(passed.contains("finish_task now"), "{passed}");
        let failed = edit_check_note("pytest -q", false, "FAILED", "E  assert 1 == 2");
        assert!(failed.contains("Fix it in the next PATCH and put finish") && failed.ends_with("E  assert 1 == 2"), "{failed}");
    }

    #[test]
    fn goal_declared_check_commands_keeps_backticked_test_runners_only() {
        let goal = "Add a `--count` flag to `kvstore`. Acceptance: `python3 -m unittest discover -s tests -q` must pass and `cargo test` too. Do not touch `README.md`.";
        assert_eq!(
            goal_declared_check_commands(goal),
            vec!["python3 -m unittest discover -s tests -q".to_string(), "cargo test".to_string()]
        );
        assert!(goal_declared_check_commands("Fix the bug in `page`; run `ls -la` first").is_empty());
        assert_eq!(goal_declared_check_commands("no backticks: python3 -m unittest"), vec!["python3 -m unittest".to_string()]);
    }

    #[test]
    fn every_declared_check_runs_as_one_chain() {
        use super::{command_is_goal_declared, goal_declared_check_chain};
        let goal = "Fix the parser. `cargo test --lib harness` and `cargo test --test loop_smoke` must pass.";
        assert_eq!(goal_declared_check_chain(goal).as_deref(), Some("cargo test --lib harness && cargo test --test loop_smoke"));
        assert!(command_is_goal_declared(goal, "cargo test --lib harness && cargo test --test loop_smoke 2>&1 | tail -20"));
        assert!(command_is_goal_declared(goal, "cargo test --test loop_smoke"));
        assert!(!command_is_goal_declared(goal, "cargo test"));
        // A chain the goal spells out subsumes its own parts.
        let spelled = "Verify with `bun run typecheck && bun test hub` (the tests alone are not enough).";
        assert_eq!(goal_declared_check_chain(spelled).as_deref(), Some("bun run typecheck && bun test hub"));
        assert_eq!(goal_declared_check_chain("Fix the bug in `page`."), None);
    }

    #[test]
    fn an_overlapping_reread_of_an_unedited_file_is_flagged() {
        use super::{overlapping_read_note, patched_paths, read_range_of};
        assert_eq!(read_range_of(r#"{"path":"a.rs","offset":100,"limit":40}"#), Some(("a.rs".to_string(), (100, 140))));
        assert_eq!(read_range_of(r#"{"path":"a.rs"}"#), Some(("a.rs".to_string(), (0, i64::MAX))));
        assert_eq!(read_range_of(r#"{"command":"ls"}"#), None);
        // A shifted window that overlaps an earlier read is flagged.
        let note = overlapping_read_note(&[(100, 140)], (120, 160));
        assert!(note.as_deref().is_some_and(|note| note.contains("lines 121-160 of this file overlaps your earlier READ of lines 101-140")), "{note:?}");
        // Disjoint windows are not.
        assert_eq!(overlapping_read_note(&[(100, 140)], (200, 240)), None);
        // A whole-file read overlaps any prior range.
        assert!(overlapping_read_note(&[(100, 140)], (0, i64::MAX)).is_some());
        assert_eq!(patched_paths(r#"{"files":[{"path":"a.rs","find":"x","replace":"y"},{"path":"b.rs","content":"z"}]}"#), vec!["a.rs".to_string(), "b.rs".to_string()]);
        assert_eq!(patched_paths(r#"{"path":"c.rs","append":"t"}"#), vec!["c.rs".to_string()]);
    }

    #[test]
    fn an_append_takes_the_placement_the_goal_names() {
        use super::{anchor_append_to_goal, goal_placement_anchor};
        assert_eq!(goal_placement_anchor("Add a unit test named new_case right after `old_case` in src/x.rs."), Some((true, "old_case".to_string())));
        assert_eq!(goal_placement_anchor("Insert the helper below the test parse_flags."), Some((true, "parse_flags".to_string())));
        assert_eq!(goal_placement_anchor("Put it before fn runCommand."), Some((false, "runCommand".to_string())));
        assert_eq!(goal_placement_anchor("Run it after the tests pass and before merging."), None);
        let (input, note) = anchor_append_to_goal(r#"{"path":"src/x.rs","append":"fn new_case() {}"}"#, "Add new_case right after `old_case`.");
        assert!(input.contains(r#""after":"old_case""#), "{input}");
        assert!(note.is_some_and(|note| note.contains("after `old_case`")));
        let (input, note) = anchor_append_to_goal(r#"{"files":[{"path":"a.rs","append":"x","before":"z"},{"path":"b.rs","append":"y"},{"path":"c.rs","find":"1","replace":"2"}]}"#, "before `old_case`");
        assert!(note.is_some() && input.contains(r#""before":"z""#) && input.matches("old_case").count() == 1, "{input}");
        let raw = r#"{"path":"src/x.rs","find":"a","replace":"b"}"#;
        assert_eq!(anchor_append_to_goal(raw, "right after `old_case`"), (raw.to_string(), None));
    }

    #[test]
    fn a_stale_finish_reruns_the_declared_chain_unless_the_last_check_was_it() {
        use super::rerun_keeps_goal_standing;
        use crate::core::types::HarnessVerificationRecord;
        let goal = "Fix the parser. `cargo test --lib harness` and `cargo test --test loop_smoke` must pass.";
        let record = |command: &str| HarnessVerificationRecord {
            at_iteration: 1,
            command: command.to_string(),
            failed: false,
            output_tail: String::new(),
            ran_no_tests: None,
            evidence: None,
            id: None,
        };
        assert!(!rerun_keeps_goal_standing(goal, &record("cargo test --lib harness::r#loop::one_test -- --nocapture"), &[]));
        assert!(rerun_keeps_goal_standing(goal, &record("cargo test --lib harness"), &[]));
        assert!(rerun_keeps_goal_standing(goal, &record("cargo test --lib harness && cargo test --test loop_smoke 2>&1 | tail -20"), &[]));
        assert!(rerun_keeps_goal_standing(goal, &record("pytest -q"), &["cargo test --lib harness && cargo test --test loop_smoke".to_string()]));
        assert!(rerun_keeps_goal_standing("Fix the bug in `page`.", &record("pytest -q"), &[]));
    }

    #[test]
    fn plain_prose_check_commands_read_to_the_end_of_the_clause() {
        use super::plain_prose_check_commands;
        let goal = "Add the helper. Verify with cargo test --release --lib tools::builtin::patch. Do not commit, push, or open PRs.";
        assert_eq!(plain_prose_check_commands(goal), vec!["cargo test --release --lib tools::builtin::patch".to_string()]);
        let goal = "Run python3 -m unittest discover -s tests -q and make sure it passes; then bun test, typecheck and the UI drift test stay green.";
        assert_eq!(plain_prose_check_commands(goal), vec!["python3 -m unittest discover -s tests -q".to_string(), "bun test".to_string()]);
        let goal = "every command must exit 0: cargo check --all-targets; cargo test --lib github; cargo test --lib roles";
        assert_eq!(
            plain_prose_check_commands(goal),
            vec!["cargo check --all-targets".to_string(), "cargo test --lib github".to_string(), "cargo test --lib roles".to_string()]
        );
        let goal = "Verify with bun run typecheck && bun test hub/test/discovery.test.ts. Do not commit, push, or open PRs.";
        assert_eq!(plain_prose_check_commands(goal), vec!["bun run typecheck && bun test hub/test/discovery.test.ts".to_string()]);
        let goal = "run cargo build && cargo test --lib harness and then stop";
        assert_eq!(plain_prose_check_commands(goal), vec!["cargo build && cargo test --lib harness".to_string()]);
        assert_eq!(plain_prose_check_commands("cargo test --lib alpha && echo done"), vec!["cargo test --lib alpha".to_string()], "a chain onto a non-runner ends the command");
        let goal = "add a test: check_duration_measurable(Some(\"cargo test --no-run\"), false, \"cargo test --lib\") must be true. Verify with cargo test --release --lib. Do not commit.";
        assert_eq!(plain_prose_check_commands(goal), vec!["cargo test --release --lib".to_string()], "quoted code samples are not commands");
        assert_eq!(plain_prose_check_commands("run cargo test --lib\" now"), vec!["cargo test".to_string()], "a quote inside an argument ends the command before it");
        assert_eq!(plain_prose_check_commands("then (cd drip && cargo test) green and finish_task"), vec!["cargo test".to_string()]);
        assert!(plain_prose_check_commands("Acceptance: `cargo test -q` must pass.").is_empty(), "backticked spans belong to the other scan");
        assert!(plain_prose_check_commands("a pytest-style fixture and the mycargo tester").is_empty(), "word boundaries");
        assert_eq!(plain_prose_check_commands("run pytest tests/test_cli.py -q when done"), vec!["pytest tests/test_cli.py -q".to_string()]);
    }

    #[test]
    fn direct_task_title_follows_the_plan_mode_and_goal_shape() {
        use super::{direct_task_title, PlanMode};
        let small = "Add a `count` subcommand to kvstore/cli.py. Run `python3 -m unittest discover -s tests -q`.";
        assert_eq!(direct_task_title(small, PlanMode::Always, false), None);
        assert_eq!(direct_task_title(small, PlanMode::Auto, false).as_deref(), Some(small));
        assert!(direct_task_title(small, PlanMode::Direct, false).is_some());
        // No declared check: direct only when the workspace has a detectable project suite.
        let unchecked = "Rename the helper in kvstore/cli.py and update its callers.";
        assert_eq!(direct_task_title(unchecked, PlanMode::Auto, false), None);
        assert_eq!(direct_task_title(unchecked, PlanMode::Auto, true).as_deref(), Some(unchecked));
        // No declared check: auto plans.
        assert_eq!(direct_task_title("Add a `count` subcommand to kvstore/cli.py.", PlanMode::Auto, false), None);
        // Too long: auto plans.
        let long = format!("{} {}", small, "and more ".repeat(300));
        assert_eq!(direct_task_title(&long, PlanMode::Auto, true), None);
        assert!(direct_task_title(&long, PlanMode::Direct, false).is_some());
        assert_eq!(direct_task_title("   ", PlanMode::Direct, true), None);
    }

    #[test]
    fn compact_tool_input_names_what_the_call_touched() {
        assert_eq!(super::compact_tool_input(r#"{"command":"grep -n   foo\n src/ | head"}"#, 90), "grep -n foo src/ | head");
        assert_eq!(super::compact_tool_input(r#"{"path":"src/a.rs","content":"..."}"#, 90), "src/a.rs");
        assert_eq!(super::compact_tool_input("not json", 90), "not json");
        assert!(super::compact_tool_input(&format!(r#"{{"command":"{}"}}"#, "x".repeat(300)), 90).chars().count() <= 91);
    }

    #[test]
    fn a_bash_run_of_the_goal_declared_check_carries_the_external_anchor() {
        use super::declared_verification_anchor_for_goal;
        use crate::core::types::VerificationAnchorKind;
        let goal = "Add scopeLabel. Verify with bun run typecheck && bun test hub/test/discovery.test.ts. Do not commit.";
        let raw = r#"{"command":"cd /w/tw-df80 && bun run typecheck && bun test hub/test/discovery.test.ts 2>&1"}"#;
        let anchor = declared_verification_anchor_for_goal(raw, &["hub/src/discovery.ts".to_string()], goal).expect("anchored");
        assert_eq!(anchor.kind, VerificationAnchorKind::External);
        assert!(anchor.source.as_deref().unwrap_or("").contains("run by the agent"), "{:?}", anchor.source);
        assert!(declared_verification_anchor_for_goal(r#"{"command":"bun test hub/test/other.test.ts"}"#, &[], goal).is_none());
    }

    #[test]
    fn goal_declared_checks_stay_external_when_they_name_edited_files() {
        let edited = vec!["tests/test_new.py".to_string()];
        let plain = r#"{"command":"python3 -m unittest tests/test_new.py","anchor":{"kind":"external","source":"suite"}}"#;
        let downgraded = super::declared_verification_anchor(plain, &edited).unwrap();
        assert_eq!(downgraded.kind, crate::core::types::VerificationAnchorKind::SelfAuthored);
        let marked = r#"{"command":"python3 -m unittest tests/test_new.py","anchor":{"kind":"external","source":"goal"},"harnessGoalDeclaredCheck":true}"#;
        let kept = super::declared_verification_anchor(marked, &edited).unwrap();
        assert_eq!(kept.kind, crate::core::types::VerificationAnchorKind::External);
        assert!(kept.downgraded_reason.is_none());
    }
}

#[cfg(test)]
mod review_brief_tests {
    use super::*;

    fn git(dir: &std::path::Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@e").env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@e")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    /// The brief diffs against the run-start HEAD (so a commit made during
    /// the run stays visible), lists untracked files, and carries the run's
    /// verification records.
    #[test]
    fn the_fourth_read_window_of_a_small_file_becomes_a_whole_file_read() {
        let dir = std::env::temp_dir().join(format!("drip-read-promote-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let text: String = (1..=120).map(|n| format!("line {n}\n")).collect();
        std::fs::write(dir.join("a.rs"), &text).unwrap();
        let input = r#"{"path":"a.rs","offset":41,"limit":40}"#;
        // Under the threshold: untouched.
        assert_eq!(promote_read_to_whole_file(input, 2, &dir), (input.to_string(), None));
        // At the threshold: the whole file, with a note.
        let (promoted, note) = promote_read_to_whole_file(input, 3, &dir);
        let value: serde_json::Value = serde_json::from_str(&promoted).unwrap();
        assert_eq!(value["offset"], 1);
        assert_eq!(value["limit"], 120);
        assert!(note.unwrap().contains("window 4 of a.rs"));
        // Already a whole-file read, a missing file, or a huge file: untouched.
        let whole = r#"{"path":"a.rs","offset":1,"limit":400}"#;
        assert_eq!(promote_read_to_whole_file(whole, 5, &dir), (whole.to_string(), None));
        let missing = r#"{"path":"nope.rs","offset":41,"limit":40}"#;
        assert_eq!(promote_read_to_whole_file(missing, 5, &dir), (missing.to_string(), None));
        let big: String = (1..=READ_WHOLE_FILE_MAX_LINES + 1).map(|n| format!("{n}\n")).collect();
        std::fs::write(dir.join("big.rs"), big).unwrap();
        let big_input = r#"{"path":"big.rs","offset":41,"limit":40}"#;
        assert_eq!(promote_read_to_whole_file(big_input, 5, &dir), (big_input.to_string(), None));
        // Few lines but too many chars: untouched.
        let wide: String = (1..=200).map(|_| format!("{}\n", "x".repeat(300))).collect();
        std::fs::write(dir.join("wide.rs"), wide).unwrap();
        let wide_input = r#"{"path":"wide.rs","offset":41,"limit":40}"#;
        assert_eq!(promote_read_to_whole_file(wide_input, 5, &dir), (wide_input.to_string(), None));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_long_bash_command_gets_a_generation_cost_note() {
        assert!(long_bash_command_note("cargo test -q").is_none());
        let script = "x".repeat(LONG_BASH_COMMAND_CHARS + 1);
        let note = long_bash_command_note(&script).expect("note");
        assert!(note.contains("1201 chars") && note.contains("separate calls"), "{note}");
    }

    #[test]
    fn a_runner_bash_call_loses_its_trailing_tail_filter() {
        let (input, dropped) = drop_runner_tail_filter(r#"{"command":"cargo test --lib shape 2>&1 | tail -3","timeoutMs":300000}"#);
        assert_eq!(dropped.as_deref(), Some("| tail -3"));
        let value: serde_json::Value = serde_json::from_str(&input).unwrap();
        assert_eq!(value["command"], "cargo test --lib shape 2>&1");
        assert_eq!(value["timeoutMs"], 300000);
        // Not a runner, or no filter: untouched.
        let plain = r#"{"command":"ls | tail -3"}"#;
        assert_eq!(drop_runner_tail_filter(plain), (plain.to_string(), None));
        let bare = r#"{"command":"cargo test -q"}"#;
        assert_eq!(drop_runner_tail_filter(bare), (bare.to_string(), None));
    }

    #[test]
    fn a_repeated_verify_reuses_a_current_passing_record_only() {
        use crate::core::types::{HarnessVerificationRecord, VerificationEvidence, VerificationEvidenceKind};
        let tests = VerificationEvidence { anchor: None, kind: VerificationEvidenceKind::Tests, executed: 1, passed: 1, failed: 0, skipped: None, detail: None };
        let record = HarnessVerificationRecord { at_iteration: 3, command: "cargo test --lib native_runner 2>&1 | tail -20".into(), failed: false, output_tail: String::new(), ran_no_tests: None, evidence: Some(tests.clone()), id: Some("v2".into()) };
        // Same shape (tail filter and verbosity ignored), nothing edited: reused, citing the record.
        let text = repeated_verify_reuse("cargo test --lib native_runner", Some(&record), 0).expect("reused");
        assert!(text.contains("record v2") && text.contains("iteration 3"), "{text}");
        // An edit since, a different command, a failed run, or no executed evidence: runs again.
        assert!(repeated_verify_reuse("cargo test --lib native_runner", Some(&record), 1).is_none());
        assert!(repeated_verify_reuse("cargo test --lib other", Some(&record), 0).is_none());
        let failed = HarnessVerificationRecord { failed: true, ..record.clone() };
        assert!(repeated_verify_reuse("cargo test --lib native_runner", Some(&failed), 0).is_none());
        let empty = HarnessVerificationRecord { evidence: Some(VerificationEvidence { executed: 0, passed: 0, ..tests }), ..record.clone() };
        assert!(repeated_verify_reuse("cargo test --lib native_runner", Some(&empty), 0).is_none());
        assert!(repeated_verify_reuse("cargo test", None, 0).is_none());
    }

    #[test]
    fn a_native_runner_suite_naming_no_edited_file_is_promoted_to_external() {
        use crate::core::types::{VerificationAnchor, VerificationAnchorKind, VerificationEvidence, VerificationEvidenceKind};
        let tests = VerificationEvidence { anchor: None, kind: VerificationEvidenceKind::Tests, executed: 12, passed: 12, failed: 0, skipped: None, detail: None };
        let edited = vec!["src/lib.rs".to_string(), "tests/test_store.py".to_string()];
        // Undeclared anchor on a plain suite run: promoted.
        let promoted = promote_native_runner_anchor(None, "cargo test --release -q", &tests, &edited).expect("promoted");
        assert_eq!(promoted.kind, VerificationAnchorKind::External);
        assert!(promoted.source.as_deref().unwrap_or("").contains("cargo test"), "{:?}", promoted.source);
        // A "self" label without a harness downgrade: promoted, label kept in the source.
        let declared = VerificationAnchor { kind: VerificationAnchorKind::SelfAuthored, source: Some("I added a test".into()), downgraded_reason: None, coverage: None, expectation_subject: None };
        let promoted = promote_native_runner_anchor(Some(declared), "python3 -m unittest discover -s tests -q", &tests, &edited).expect("promoted");
        assert_eq!(promoted.kind, VerificationAnchorKind::External);
        assert!(promoted.source.as_deref().unwrap_or("").contains("I added a test"));
        // Names an edited file: stays as it was.
        assert!(promote_native_runner_anchor(None, "python3 -m pytest tests/test_store.py -q", &tests, &edited).is_none());
        // A harness downgrade is never undone.
        let downgraded = VerificationAnchor { kind: VerificationAnchorKind::SelfAuthored, source: None, downgraded_reason: Some("names an edited file".into()), coverage: None, expectation_subject: None };
        assert_eq!(promote_native_runner_anchor(Some(downgraded), "cargo test", &tests, &[]).unwrap().kind, VerificationAnchorKind::SelfAuthored);
        // Not a native runner, or nothing executed: unchanged.
        assert!(promote_native_runner_anchor(None, "python3 probe.py", &tests, &[]).is_none());
        let none_ran = VerificationEvidence { executed: 0, passed: 0, kind: VerificationEvidenceKind::Unverified, ..tests.clone() };
        assert!(promote_native_runner_anchor(None, "cargo test", &none_ran, &[]).is_none());
        // Newer runners map through native_runner_name to their short names.
        for (command, name) in [
            ("mix test", "mix test"),
            ("dotnet test --logger trx", "dotnet test"),
            ("mvn test -q", "mvn test"),
            ("gradle test", "gradle test"),
            ("./gradlew test --tests core.*", "gradle test"),
        ] {
            let promoted = promote_native_runner_anchor(None, command, &tests, &edited).expect("promoted");
            assert_eq!(promoted.kind, VerificationAnchorKind::External);
            assert!(promoted.source.as_deref().unwrap_or("").contains(name), "{:?}", promoted.source);
        }
    }

    #[test]
    fn a_self_labelled_goal_declared_check_is_upgraded_to_external() {
        use crate::core::types::VerificationAnchorKind;
        let goal = "Fix the bug. Run `python3 -m unittest discover -s tests -q` to confirm.";
        let edited = vec!["tests/test_store.py".to_string()];
        let upgraded = declared_verification_anchor_for_goal(
            r#"{"command":"python3 -m unittest discover -s tests -q","anchor":{"kind":"self","source":"project suite"}}"#,
            &edited,
            goal,
        )
        .unwrap();
        assert_eq!(upgraded.kind, VerificationAnchorKind::External);
        assert!(upgraded.source.unwrap().starts_with("goal-declared acceptance check: python3 -m unittest"));
        let other = declared_verification_anchor_for_goal(
            r#"{"command":"python3 -m unittest tests.test_store","anchor":{"kind":"self","source":"my test"}}"#,
            &edited,
            goal,
        )
        .unwrap();
        assert_eq!(other.kind, VerificationAnchorKind::SelfAuthored, "a different command stays self");
        let no_check = declared_verification_anchor_for_goal(
            r#"{"command":"python3 -m unittest discover -s tests -q","anchor":{"kind":"self"}}"#,
            &edited,
            "Fix the bug.",
        )
        .unwrap();
        assert_eq!(no_check.kind, VerificationAnchorKind::SelfAuthored, "no declared check, no upgrade");
    }

    #[test]
    fn workspace_changed_lines_counts_tracked_hunks_and_new_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
        git(dir.path(), &["init", "-q"]);
        git(dir.path(), &["add", "a.txt"]);
        git(dir.path(), &["commit", "-q", "-m", "base"]);
        let cwd = dir.path().to_string_lossy().to_string();
        let head = git_head(&cwd).expect("head");
        assert_eq!(workspace_changed_lines(&cwd, &head), Some(0));
        std::fs::write(dir.path().join("a.txt"), "one\n2\nthree\nfour\n").unwrap();
        std::fs::write(dir.path().join("new.txt"), "x\ny\n").unwrap();
        std::fs::write(dir.path().join("blob.bin"), [0u8, 159, 146, 150, 255]).unwrap();
        std::fs::create_dir_all(dir.path().join(".dripdata/sessions")).unwrap();
        std::fs::write(dir.path().join(".dripdata/sessions/state.json"), "{}\n".repeat(500)).unwrap();
        assert_eq!(workspace_changed_lines(&cwd, &head), Some(1 + 2 + 2), "one deleted, two added, two untracked; the binary blob and the dot-directory count nothing");
        std::fs::create_dir_all(dir.path().join("tests")).unwrap();
        std::fs::write(dir.path().join("tests/test_a.py"), "import unittest\n".repeat(30)).unwrap();
        assert_eq!(workspace_changed_lines_split(&cwd, &head), Some((1 + 2 + 2, 30)), "test files count on their own side");
        assert_eq!(workspace_changed_lines(&cwd, &head), Some(1 + 2 + 2 + 30));
        assert_eq!(workspace_changed_lines(&std::env::temp_dir().to_string_lossy(), "HEAD"), None);
    }

    #[test]
    fn changes_so_far_lists_edited_and_new_files_outside_dot_directories() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        git(dir.path(), &["init", "-q"]);
        git(dir.path(), &["add", "a.txt"]);
        git(dir.path(), &["commit", "-q", "-m", "base"]);
        let cwd = dir.path().to_string_lossy().to_string();
        let head = git_head(&cwd).expect("head");
        assert!(changes_so_far(&cwd, &head).is_none(), "nothing changed yet");
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(dir.path().join("new.txt"), "x\n").unwrap();
        std::fs::create_dir_all(dir.path().join(".dripdata")).unwrap();
        std::fs::write(dir.path().join(".dripdata/state.json"), "{}").unwrap();
        let (section, files) = changes_so_far(&cwd, &head).expect("changes");
        assert!(section.starts_with("changes so far this run"), "{section}");
        assert!(section.contains("a.txt | 1 +"), "{section}");
        assert!(section.contains("new files: new.txt"), "{section}");
        assert_eq!(files, vec!["a.txt".to_string(), "new.txt".to_string()]);
    }

    #[test]
    fn review_brief_shows_the_change_set_since_run_start() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        git(dir.path(), &["init", "-q"]);
        git(dir.path(), &["add", "a.txt"]);
        git(dir.path(), &["commit", "-q", "-m", "base"]);
        let cwd = dir.path().to_string_lossy().to_string();
        let head = git_head(&cwd).expect("head");
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        git(dir.path(), &["commit", "-q", "-am", "during the run"]);
        std::fs::write(dir.path().join("new.txt"), "fresh\n").unwrap();
        std::fs::write(dir.path().join("big.txt"), "line\n".repeat(REVIEW_BRIEF_MAX_INLINE_LINES + 1)).unwrap();
        let mut state = crate::core::state::create_harness_state("goal");
        state.verifications = Some(vec![crate::core::types::HarnessVerificationRecord {
            at_iteration: 2,
            command: "python3 -m unittest -q".into(),
            failed: false,
            output_tail: "".into(),
            ran_no_tests: None,
            evidence: Some(crate::tools::builtin::verify::verification_evidence("python3 -m unittest -q", "Ran 5 tests\n\nOK")),
            id: Some("v1".into()),
        }]);
        let brief = build_review_brief(&cwd, Some(&head), &state);
        assert!(brief.starts_with(REVIEW_BRIEF_PREFIX), "{brief}");
        assert!(brief.contains("+two"), "committed change is in the diff: {brief}");
        assert!(brief.contains("new file new.txt (1 lines, in full — do not READ it again):\n```\nfresh\n```"), "small new files ride along: {brief}");
        assert!(brief.contains("new file big.txt (READ it; not in the diff)"), "big new files are only listed: {brief}");
        assert!(brief.contains(&format!("{REVIEW_BRIEF_EDITED_HEADER}\n== a.txt (2 lines)\n1\tone\n2\ttwo")), "edited tracked files ride along whole: {brief}");
        assert!(brief.contains("do not READ a file that appears there"), "{brief}");
        assert!(brief.contains("v1 passed — python3 -m unittest -q"), "{brief}");
        let settled = build_review_brief_with(&cwd, Some(&head), &state, Some("the harness ran the goal-declared check (cargo test --lib harness)"));
        assert!(settled.contains("verification settled: the harness ran the goal-declared check (cargo test --lib harness) after the last edit"), "{settled}");
        assert!(settled.contains("already settle verification; finish_task once the diff reads correct"), "{settled}");
        assert!(!settled.contains("One VERIFY of the project's own check is enough"), "{settled}");
        assert!(brief.contains("One VERIFY of the project's own check is enough"), "{brief}");
        let outside = build_review_brief(&std::env::temp_dir().to_string_lossy(), None, &state);
        assert!(outside.contains("diff: none"), "{outside}");
    }
}

pub const REVIEW_BRIEF_PREFIX: &str = "review brief (harness-captured)";
/// Diff text kept in a review brief; the reviewer can READ a file for more.
pub const REVIEW_BRIEF_MAX_DIFF_CHARS: usize = 16_000;
/// New (untracked) files inlined in full in the review brief: at most this
/// many files, each within these line and character bounds.
pub const REVIEW_BRIEF_MAX_INLINE_FILES: usize = 4;
pub const REVIEW_BRIEF_MAX_INLINE_LINES: usize = 400;
pub const REVIEW_BRIEF_MAX_INLINE_CHARS: usize = 16_000;
/// Header of the brief section that carries the edited tracked files whole,
/// line-numbered as READ returns them: 74 of 81 recorded reviewer loops
/// opened with READs of the files the diff had just shown them hunks of.
pub const REVIEW_BRIEF_EDITED_HEADER: &str = "edited files, full current text (line-numbered exactly as a READ returns it — the diff above shows what changed, this is the context; do not READ these again):";

fn git_output(cwd: &str, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git").args(args).current_dir(cwd).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Lines changed in the workspace since `base`: added + deleted lines of the
/// tracked diff plus every line of each untracked file. None outside git.
/// Files changed since run start (tracked edits plus untracked files outside
/// dot-directories), for a later author task's prompt. The list feeds file
/// outlines; the text is a `git diff --stat` capped at CHANGES_SO_FAR_MAX_LINES.
pub const CHANGES_SO_FAR_MAX_LINES: usize = 20;

pub fn changes_so_far(cwd: &str, base: &str) -> Option<(String, Vec<String>)> {
    let stat = git_output(cwd, &["diff", "--stat", base]).map(|text| text.trim().to_string()).unwrap_or_default();
    let mut files: Vec<String> = git_output(cwd, &["diff", "--name-only", base])
        .map(|text| text.lines().map(str::trim).filter(|line| !line.is_empty()).map(str::to_string).collect())
        .unwrap_or_default();
    let untracked: Vec<String> = git_output(cwd, &["ls-files", "--others", "--exclude-standard"])
        .map(|text| {
            text.lines()
                .map(str::trim)
                .filter(|line| !line.is_empty() && !line.split('/').any(|part| part.starts_with('.')))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    for path in &untracked {
        if !files.contains(path) {
            files.push(path.clone());
        }
    }
    if files.is_empty() {
        return None;
    }
    let mut lines: Vec<String> = vec!["changes so far this run (git diff --stat vs run start; the files below are already edited — build on them, do not redo their work):".to_string()];
    let stat_lines: Vec<&str> = stat.lines().collect();
    for line in stat_lines.iter().take(CHANGES_SO_FAR_MAX_LINES) {
        lines.push(line.to_string());
    }
    if stat_lines.len() > CHANGES_SO_FAR_MAX_LINES {
        lines.push(format!("… +{} more", stat_lines.len() - CHANGES_SO_FAR_MAX_LINES));
    }
    if !untracked.is_empty() {
        lines.push(format!("new files: {}", untracked.join(", ")));
    }
    Some((lines.join("\n"), files))
}

pub fn workspace_changed_lines(cwd: &str, base: &str) -> Option<usize> {
    workspace_changed_lines_split(cwd, base).map(|(code, tests)| code + tests)
}

/// Changed lines since `base` as (outside test files, in test files): the
/// tracked diff's added plus deleted lines and every line of each new file,
/// test-likeness by path (tests/ trees, test_*.py, *.test.ts, …).
pub fn workspace_changed_lines_split(cwd: &str, base: &str) -> Option<(usize, usize)> {
    let numstat = git_output(cwd, &["diff", "--numstat", base])?;
    let mut code = 0usize;
    let mut tests = 0usize;
    let mut count = |path: &str, lines: usize| {
        if crate::harness::outline::test_like_path(path) {
            tests += lines;
        } else {
            code += lines;
        }
    };
    for row in numstat.lines() {
        let mut cols = row.split('\t');
        let added = cols.next().and_then(|v| v.trim().parse::<usize>().ok());
        let deleted = cols.next().and_then(|v| v.trim().parse::<usize>().ok());
        let path = cols.next().map(str::trim).unwrap_or("");
        // Binary rows show "-": a reviewer could not read them either, and
        // in practice they are build artefacts (a tracked __pycache__), so
        // they do not count toward the bound.
        if let (Some(a), Some(d)) = (added, deleted) {
            count(path, a + d);
        }
    }
    if let Some(untracked) = git_output(cwd, &["ls-files", "--others", "--exclude-standard"]) {
        for path in untracked.lines().map(str::trim).filter(|line| !line.is_empty()) {
            // Dot-directories and dotfiles (a `.dripdata/` project dir inside
            // the workspace, a `.venv`) are tooling state, not the change.
            if path.split('/').any(|component| component.starts_with('.')) {
                continue;
            }
            // An unreadable (binary) new file counts nothing, like a binary hunk.
            let lines = std::fs::read_to_string(std::path::Path::new(cwd).join(path))
                .map(|text| text.lines().count())
                .unwrap_or(0);
            count(path, lines);
        }
    }
    Some((code, tests))
}

pub fn git_head(cwd: &str) -> Option<String> {
    git_output(cwd, &["rev-parse", "HEAD"]).map(|text| text.trim().to_string()).filter(|text| !text.is_empty())
}

/// The reviewer's opening note: the run's change set (diff against the
/// run-start HEAD, plus untracked files) and its verification records.
pub fn build_review_brief(cwd: &str, run_start_head: Option<&str>, state: &HarnessState) -> String {
    build_review_brief_with(cwd, run_start_head, state, None)
}

/// `settled` names the goal-declared check that already passed after the last
/// edit (see HarnessRun::verified_after_last_edit); the brief then tells the
/// reviewer not to spend a round re-running it.
pub fn build_review_brief_with(cwd: &str, run_start_head: Option<&str>, state: &HarnessState, settled: Option<&str>) -> String {
    let mut sections = vec![REVIEW_BRIEF_PREFIX.to_string()];
    let base = run_start_head.unwrap_or("HEAD");
    if let Some(stat) = git_output(cwd, &["diff", "--stat", base]).map(|text| text.trim().to_string()).filter(|text| !text.is_empty()) {
        sections.push(format!("changed since run start ({}):\n{stat}", &base[..base.len().min(12)]));
    }
    if let Some(untracked) = git_output(cwd, &["ls-files", "--others", "--exclude-standard"]).map(|text| text.trim().to_string()).filter(|text| !text.is_empty()) {
        // New files are not in the diff; small ones ride along in full so the
        // reviewer does not spend a round per file re-reading what it was
        // handed (bench reviewers READ every changed file before VERIFY).
        let mut listed: Vec<String> = Vec::new();
        let mut inlined = 0usize;
        for path in untracked.lines().map(str::trim).filter(|line| !line.is_empty()) {
            let full = std::path::Path::new(cwd).join(path);
            let text = std::fs::read_to_string(&full).ok();
            match text {
                Some(text)
                    if inlined < REVIEW_BRIEF_MAX_INLINE_FILES
                        && text.lines().count() <= REVIEW_BRIEF_MAX_INLINE_LINES
                        && text.chars().count() <= REVIEW_BRIEF_MAX_INLINE_CHARS =>
                {
                    inlined += 1;
                    listed.push(format!("new file {path} ({} lines, in full — do not READ it again):\n```\n{}\n```", text.lines().count(), text.trim_end()));
                }
                _ => listed.push(format!("new file {path} (READ it; not in the diff)")),
            }
        }
        sections.push(format!("untracked files:\n{}", listed.join("\n")));
    }
    match git_output(cwd, &["diff", base]).map(|text| text.trim().to_string()).filter(|text| !text.is_empty()) {
        Some(diff) if diff.chars().count() <= REVIEW_BRIEF_MAX_DIFF_CHARS => sections.push(format!("diff:\n{diff}")),
        Some(diff) => sections.push(format!(
            "diff (first {REVIEW_BRIEF_MAX_DIFF_CHARS} chars; READ the files for the rest):\n{}",
            diff.chars().take(REVIEW_BRIEF_MAX_DIFF_CHARS).collect::<String>()
        )),
        None => sections.push("diff: none (no git repository, or nothing changed since run start)".to_string()),
    }
    // The edited tracked files ride along whole (the same caps as the
    // author's named-file carry) so the reviewer starts from the text
    // instead of paging it back in; the diff alone shows hunks, not context.
    if let Some(edited) = git_output(cwd, &["diff", "--name-only", base]) {
        let paths: Vec<String> = edited
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.split('/').any(|segment| segment.starts_with('.')))
            .map(str::to_string)
            .collect();
        if let (Some(bodies), _) = crate::harness::outline::file_bodies_for_paths(cwd, &paths, REVIEW_BRIEF_EDITED_HEADER) {
            sections.push(bodies);
        }
    }
    let records: Vec<String> = state
        .verifications
        .iter()
        .flatten()
        .rev()
        .take(6)
        .map(|record| {
            format!(
                "{} {} — {}{}",
                record.id.as_deref().unwrap_or("v?"),
                if record.failed { "FAILED" } else { "passed" },
                truncate_text(&record.command, 120),
                crate::core::state::describe_verification_evidence(record.evidence.as_ref())
            )
        })
        .collect();
    if !records.is_empty() {
        sections.push(format!("verification records this run (newest first):\n{}", records.join("\n")));
    }
    if let Some(settled) = settled {
        sections.push(format!("verification settled: {settled} after the last edit and it passed. Do not re-run it — it costs a round and the same minutes again and answers nothing new. Spend the rounds on the diff; run a check only for something the records above do not cover."));
    }
    let verify_guidance = if settled.is_some() {
        "The records above already settle verification; finish_task once the diff reads correct."
    } else {
        "One VERIFY of the project's own check is enough to confirm the records above; spend rounds on what the diff shows, not on re-deriving it."
    };
    sections.push(format!("Judge the diff against the goal and the task contracts. The diff and the new files above ARE the change: do not READ a file that appears there unless a hunk's surrounding context is genuinely insufficient, and never READ it just to confirm the diff applied. {verify_guidance} Defects in code the diff did not touch are pre-existing and out of scope: mention them in a note_task, do not raise them as anomalies or block on them."));
    sections.join("\n\n")
}

pub fn extract_loop_carryover(messages: &[TransportRequestMessage], hot_tool_results: usize) -> Vec<TransportRequestMessage> {
    extract_loop_carryover_excluding(messages, hot_tool_results, &[])
}

/// The same tail selection, dropping every exchange whose call named one of
/// `exclude_tools`. Used when the next loop already carries that tool's output
/// another way — a review loop's brief holds the diff and every new file in
/// full, so replaying PATCH calls would only double the reviewer's prompt.
pub fn extract_loop_carryover_excluding(
    messages: &[TransportRequestMessage],
    hot_tool_results: usize,
    exclude_tools: &[&str],
) -> Vec<TransportRequestMessage> {
    // Blocks: an assistant message plus every tool result that follows it.
    let mut blocks: Vec<Vec<&TransportRequestMessage>> = Vec::new();
    for message in messages {
        match message.role {
            ChatRoleTag::Assistant => {
                let excluded = message
                    .tool_calls
                    .iter()
                    .flatten()
                    .filter_map(|call| call.function.as_ref().and_then(|function| function.name.as_deref()))
                    .any(|name| exclude_tools.contains(&name));
                if !excluded {
                    blocks.push(vec![message]);
                }
            }
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
/// Identical anomaly-family bounces of a finish before the harness re-applies
/// it as unreconciled (see HarnessRun::auto_unreconcile_repeated_bounce).
pub const FINISH_BOUNCE_AUTO_UNRECONCILE_AT: u32 = 2;

/// The anomaly family a finish bounce belongs to, if any: only bounces whose
/// remedy is "finish unreconciled with the anomalies" qualify. Evidence and
/// verification bounces have a different remedy (run a check) and never do.
pub fn finish_bounce_family(text: &str) -> Option<&'static str> {
    if !text.starts_with("harness: not accepted yet") {
        return None;
    }
    if text.contains("support-gap anomal") {
        Some("support gap")
    } else if text.contains("mismatched (expected") {
        Some("expectation mismatch")
    } else if text.contains("have no observation") {
        Some("unobserved expectation")
    } else {
        None
    }
}

/// The original finish_task input re-shaped as an unreconciled finish: the
/// status flips, the summary carries the harness note, and the anomalies
/// list is filled from the state's recorded anomalies plus every expectation
/// whose latest observation mismatched or that was never observed.
pub fn unreconciled_finish_input(raw_input: &str, state: &HarnessState, count: u32, bounce: &str) -> Option<String> {
    let mut input: serde_json::Value = serde_json::from_str(raw_input).ok()?;
    let object = input.as_object_mut()?;
    let mut anomalies: Vec<serde_json::Value> = state
        .anomalies
        .iter()
        .map(|anomaly| serde_json::json!({ "subject": anomaly.subject, "expected": anomaly.expected, "observed": anomaly.observed, "note": anomaly.note }))
        .collect();
    for expectation in &state.expectations {
        let latest = expectation.observations.last();
        let unresolved = latest.map_or(true, |observation| !observation.matches);
        if unresolved && !anomalies.iter().any(|item| item.get("subject").and_then(|value| value.as_str()) == Some(expectation.subject.as_str())) {
            anomalies.push(serde_json::json!({
                "subject": expectation.subject,
                "expected": expectation.expected,
                "observed": latest.map(|observation| observation.observed.clone()).unwrap_or_else(|| "never observed".to_string()),
                "note": format!("recorded by the harness after {count} identical finish bounces")
            }));
        }
    }
    if anomalies.is_empty() {
        return None;
    }
    let summary = object.get("summary").and_then(|value| value.as_str()).unwrap_or("").to_string();
    object.insert("status".to_string(), serde_json::json!("unreconciled"));
    object.insert("anomalies".to_string(), serde_json::json!(anomalies));
    object.insert(
        "summary".to_string(),
        serde_json::json!(format!("{summary} [harness: finished unreconciled after {count} identical bounces — {}]", truncate_text(bounce, 160)).trim().to_string()),
    );
    Some(input.to_string())
}

/// A native runner run through BASH is the project suite whichever tool
/// carried it: recorded runs ran `cargo test` via BASH, finished, were
/// bounced for missing evidence, and re-ran the same suite as VERIFY. The
/// command must name a runner and its output must parse as an executed test
/// result; the record's anchor is then promoted like any native-runner suite.
pub fn bash_native_runner_verification(tool_name: &str, raw_input: &str, output: &str) -> Option<String> {
    if tool_name != "BASH" {
        return None;
    }
    let command = extract_bash_command(raw_input)?;
    native_runner_name(&command)?;
    let evidence = crate::tools::builtin::verify::verification_evidence(&command, output);
    (evidence.kind == crate::core::types::VerificationEvidenceKind::Tests && evidence.executed > 0).then_some(command)
}

pub fn extract_bash_command(raw_input: &str) -> Option<String> {
    let parsed: Value = serde_json::from_str(raw_input).ok()?;
    parsed.get("command").and_then(Value::as_str).map(str::to_string)
}

/// The build warm-up for a workspace, if any, when a Cargo.toml sits at
/// the root. Recorded Rust dogfoods paid 20-60s of compile inside their
/// first `cargo test`, after 20-40s of orientation in which the CPU sat
/// idle. The warm-up compiles what the goal's check will run: a
/// goal-declared `cargo test …` keeps its profile, targets and packages
/// and gets `--no-run` (six dogfoods declared `cargo test --release --lib
/// <module>` while the warm-up built the dev profile, so every check still
/// paid a 27s release compile); with no such check it is `cargo build
/// --tests`. DRIP_NO_WARMUP=1 disables it.
pub fn build_warmup_command(cwd: &str, goal: &str) -> Option<(String, Vec<String>)> {
    if std::env::var_os("DRIP_NO_WARMUP").is_some() {
        return None;
    }
    if !Path::new(cwd).join("Cargo.toml").is_file() {
        return None;
    }
    if let Some(args) = goal_declared_check_commands(goal).iter().find_map(|command| cargo_test_warmup_args(command)) {
        return Some(("cargo".to_string(), args));
    }
    Some(("cargo".to_string(), vec!["build".to_string(), "--tests".to_string(), "--quiet".to_string()]))
}

/// The `cargo test` arguments of a check command with `--no-run` added and
/// the shell tail dropped (pipes, `&&` chains, redirections, the `--`
/// runner arguments), so the warm-up compiles exactly the profile, targets
/// and packages the check will run. None when the command is not a cargo
/// test.
pub fn cargo_test_warmup_args(command: &str) -> Option<Vec<String>> {
    let head = command.split(['|', ';']).next()?.split("&&").next()?;
    let tokens: Vec<&str> = head.split_whitespace().collect();
    let start = tokens.windows(2).position(|pair| pair[0] == "cargo" && pair[1] == "test")?;
    let mut args = vec!["test".to_string()];
    for token in &tokens[start + 2..] {
        if *token == "--" {
            break;
        }
        if *token == "--no-run" || token.starts_with("2>") || token.starts_with('>') || token.starts_with('<') {
            continue;
        }
        args.push((*token).to_string());
    }
    args.push("--no-run".to_string());
    if !args.iter().any(|arg| arg == "--quiet" || arg == "-q") {
        args.push("--quiet".to_string());
    }
    Some(args)
}

/// The project's own test command when the goal declares none, from the
/// workspace layout: Cargo.toml, go.mod, package.json with a test script, a
/// pytest configuration, or a tests/ directory. Used by the unchecked-finish
/// re-verify so a finish with no check behind it gets the project suite run
/// once by the harness instead of a bounce the agent answers by guessing.
/// Reasoning effort for base-model calls whose profile sets none. A profile
/// with no effort leaves the provider's default thinking on, and on GLM
/// that is where the run's time went: in the sessions of 10–12 September,
/// 12% of the calls emitted 4,000+ completion tokens — almost all hidden
/// reasoning ahead of one small tool call — and took 51% of all inference
/// time; the same runs at "low" show none. A profile that sets an effort
/// keeps it; a provider that rejects the field gets one retry without it.
pub use crate::harness::model_call::BASE_MODEL_DEFAULT_REASONING_EFFORT;

/// (effort to send, whether it is the harness default rather than the
/// profile's own setting).
pub fn base_model_reasoning_effort(configured: Option<&str>) -> (Option<String>, bool) {
    match configured.map(str::trim).filter(|effort| !effort.is_empty()) {
        Some(effort) => (Some(effort.to_string()), false),
        None => (Some(BASE_MODEL_DEFAULT_REASONING_EFFORT.to_string()), true),
    }
}

pub fn detect_project_check_command(cwd: &str) -> Option<(String, &'static str)> {
    let root = Path::new(cwd);
    if root.join("Cargo.toml").is_file() {
        return Some(("cargo test -q".to_string(), "Cargo.toml"));
    }
    if root.join("go.mod").is_file() {
        return Some(("go test ./...".to_string(), "go.mod"));
    }
    if let Ok(text) = std::fs::read_to_string(root.join("package.json")) {
        let has_test_script = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|json| json.get("scripts")?.get("test")?.as_str().map(|script| !script.trim().is_empty() && !script.contains("no test specified")))
            .unwrap_or(false);
        if has_test_script {
            let runner = if root.join("bun.lockb").is_file() || root.join("bun.lock").is_file() {
                "bun test"
            } else if root.join("pnpm-lock.yaml").is_file() {
                "pnpm test"
            } else if root.join("yarn.lock").is_file() {
                "yarn test"
            } else {
                "npm test --silent"
            };
            return Some((runner.to_string(), "package.json test script"));
        }
    }
    if root.join("pytest.ini").is_file() || root.join("conftest.py").is_file() || root.join("tests/conftest.py").is_file() {
        return Some(("python3 -m pytest -q".to_string(), "pytest configuration"));
    }
    if root.join("tests").is_dir() {
        let has_python_tests = std::fs::read_dir(root.join("tests"))
            .map(|entries| entries.flatten().any(|entry| entry.file_name().to_string_lossy().ends_with(".py")))
            .unwrap_or(false);
        if has_python_tests {
            return Some(("python3 -m unittest discover -s tests -q".to_string(), "tests/ directory"));
        }
    }
    None
}

/// True when a BASH/VERIFY result says the command was killed at its timeout.
pub fn output_reports_hang(tool_content: &str) -> bool {
    tool_content.contains("TIMED OUT after") || tool_content.contains("HUNG: the command did not finish")
}

/// Consecutive runs of one command shape (no edit between) before the result
/// carries a flailing nudge.
pub const REPEATED_COMMAND_NUDGE_AT: u32 = 3;

/// A command reduced to what it runs: leading `timeout N`, `2>&1`, trailing
/// `| tail/head …` and `; echo …` suffixes, and -v/-q verbosity flags are
/// dropped, so `timeout 60 X -v 2>&1 | tail -50; echo "exit=$?"` and `X`
/// count as the same shape.
pub fn normalize_command_shape(command: &str) -> String {
    let mut text = command.trim().to_string();
    // `; echo …` / `; printf …` reporting suffix.
    if let Some(index) = text.rfind("; echo ").or_else(|| text.rfind("; printf ")) {
        text.truncate(index);
    }
    // Trailing `| tail …` / `| head …` filters (any number of them).
    loop {
        let Some(index) = text.rfind('|') else { break };
        let tail = text[index + 1..].trim_start();
        if tail.starts_with("tail") || tail.starts_with("head") {
            text.truncate(index);
        } else {
            break;
        }
    }
    let mut words: Vec<&str> = text.split_whitespace().collect();
    // Leading `cd <dir> &&` prefix and `VAR=value` environment assignments.
    let is_env_assignment = |word: &str| match word.split_once('=') {
        Some((name, _)) => {
            !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        None => false,
    };
    loop {
        if words.len() >= 4 && words[0] == "cd" {
            if let Some(index) = words.iter().position(|word| *word == "&&") {
                words.drain(0..=index);
                continue;
            }
        }
        if words.len() > 1 && is_env_assignment(words[0]) {
            words.remove(0);
            continue;
        }
        break;
    }
    if words.len() > 2 && matches!(words[0], "timeout" | "gtimeout") && words[1].chars().all(|c| c.is_ascii_digit() || c == 's' || c == 'm') {
        words.drain(0..2);
    }
    // Trailing `2>/dev/null` redirect.
    if words.last() == Some(&"2>/dev/null") {
        words.pop();
    }
    // Trailing `--nocapture` flag.
    if words.last() == Some(&"--nocapture") {
        words.pop();
        // Drop the `--` test-harness separator that introduced it.
        if words.last() == Some(&"--") {
            words.pop();
        }
    }
    words.retain(|word| !matches!(*word, "2>&1" | "-v" | "-vv" | "-q" | "--verbose" | "--quiet"));
    words.join(" ")
}

/// Appended to a BASH/VERIFY result once the same command shape has run
/// `count` times in a row with no edit in between.
pub fn repeated_command_nudge(count: u32) -> String {
    format!(
        "[harness] this command shape has now run {count} times in a row with no workspace edit in between — running it again will give the same result. If it hangs or times out, find the hanging piece (run one module or test at a time under `timeout 20`) and fix it; if it fails, read the failure and PATCH; if it passes, VERIFY it once and finish_task."
    )
}

/// The result handed back instead of running a command that already hung
/// this run: a hanging test or blocking command hangs the same way every
/// time, and each re-run costs the full timeout (recorded runs spent three
/// two-minute timeouts on one unchanged `unittest discover`).
pub fn hung_command_refusal(tool_name: &str, command: &str) -> String {
    format!(
        "harness: not run — this exact {tool_name} command already hung and was killed at its timeout earlier in this run: {}\nRe-running it unchanged would hang again. Change the command: run a narrower target (one test module or -k pattern), fix the hang (a test that starts a server must shut it down; bind port 0; add socket timeouts), or wrap it in a shorter timeout (`timeout 30 …`).",
        truncate_text(command, 200)
    )
}

/// Timeout for a re-run of a command shape that already hung this run.
/// Recorded runs re-ran a hanging `unittest discover` under `timeout 60`,
/// `timeout 90`, `-v`, and `| tail` variants eight times in one run, each
/// costing its full timeout; a hang that is not fixed hangs just as well in
/// thirty seconds, and a fixed one finishes in a fraction of that.
pub const HUNG_SHAPE_LEASH_MS: u64 = 30_000;

/// The raw BASH/VERIFY input with its timeout replaced by `leash_ms`, or
/// None when the call already sets a timeout at or below the leash.
pub fn leash_timeout(raw_input: &str, tool_name: &str, leash_ms: u64) -> Option<String> {
    let mut value: serde_json::Value = serde_json::from_str(raw_input).ok()?;
    let object = value.as_object_mut()?;
    let key = if tool_name == "VERIFY" { "timeout" } else { "timeoutMs" };
    if object.get(key).and_then(serde_json::Value::as_f64).is_some_and(|current| current <= leash_ms as f64) {
        return None;
    }
    object.insert(key.to_string(), serde_json::json!(leash_ms));
    Some(value.to_string())
}

/// The note appended to a result that ran under the hung-shape leash.
pub fn hung_shape_leash_note(leash_ms: u64, hung_before: &str) -> String {
    format!(
        "\n[harness] this command has the same shape as one that hung earlier in this run ({}), so it ran under a {}s leash instead of the default timeout. A hang that is not fixed does not need the full timeout to prove it; if the command legitimately needs longer, pass an explicit timeout.",
        truncate_text(hung_before, 120),
        leash_ms / 1000
    )
}

/// The nudge appended to a read-only result when a loop keeps reading without persisting.
pub fn build_read_only_loop_nudge(read_only_calls: i64, cycle: i64, max_cycles: i64) -> String {
    format!(
        "[harness] {read_only_calls} read-only calls this loop (cycle {cycle}/{max_cycles}) and nothing written or recorded yet. What this loop has read is dropped when the loop ends — act on it now: PATCH the change you can already make, record the facts you need with remember/observe, or finish_task blocked with what is missing. If the goal names code that does not exist in this workspace, stop searching for it: say what exists instead and either make the smallest reasonable version of the change or finish_task blocked naming the discrepancy."
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
            || paired("bun", &["test"])
            || paired("deno", &["test", "check", "lint"])
            || paired("mix", &["test"])
            || paired("dotnet", &["test", "build"])
            || paired("swift", &["test", "build"])
            || paired("zig", &["test", "build"])
            || paired("gradle", &["test", "check", "build"])
            || paired("mvn", &["test", "verify"])
            || ["pytest", "vitest", "jest", "tsc", "unittest", "mocha", "rspec", "phpunit", "tox", "nox"]
                .iter()
                .any(|word| word_at(bytes, start, end, word))
        {
            return true;
        }
    }

    false
}

/// Stable, goal-unique verification record identity ("v<n>"): the next number
/// after the highest id already present, so replaying identical command text
/// still yields a new, distinct record id. Cycle-agnostic by construction.
pub fn next_verification_record_id(records: &Option<Vec<crate::core::types::HarnessVerificationRecord>>) -> String {
    let highest = records
        .iter()
        .flatten()
        .filter_map(|record| {
            record
                .id
                .as_deref()
                .and_then(|id| id.strip_prefix('v'))
                .and_then(|n| n.parse::<usize>().ok())
        })
        .max()
        .unwrap_or(0);
    format!("v{}", highest + 1)
}

/// One-shot corrective push for narration-only replies (exported for tests).
/// Output cap on the retry after a truncated, tool-less reply: enough for
/// a sentence plus a few hundred lines of PATCH, far below the 16k-token
/// runaway that the first attempt produced.
pub const TRUNCATION_RETRY_MAX_TOKENS: u64 = 6_000;

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
/// Marker the harness sets on the VERIFY input when it runs the goal's own
/// acceptance command: that check is task-provided, so it stays external
/// even when its command names a file this run edited (the goal told us to
/// run it against those files).
/// Finish-time checks the harness runs per loop before it stops offering
/// them and asks for a VERIFY instead.
pub const FINISH_CHECKS_MAX_PER_LOOP: u32 = 3;

/// Checks the harness runs after an edit round (see run_edit_check) per
/// loop; a workspace that keeps failing is the model's to fix, not the
/// harness's to keep measuring.
pub const EDIT_CHECKS_MAX_PER_LOOP: u32 = 3;
/// A check that took longer than this the last time it ran is not run
/// after every edit: the round it would save is cheaper than the wait.
pub const EDIT_CHECK_MAX_KNOWN_MS: i64 = 8_000;
/// Runners that compile before they test; unknown durations for these are
/// assumed slow.
pub const EDIT_CHECK_SLOW_RUNNERS: &[&str] = &["cargo test", "cargo nextest", "go test", "dotnet test", "mvn test", "gradle test", "mix test"];

/// Whether the goal's check is cheap enough to run after an edit round.
/// `build_warm`: the run's background warm-up build for this runner has
/// finished, so the compile that makes the runner slow is already paid and
/// an unmeasured check is an incremental one. A recorded pwrde run had its
/// warm-up done seconds in, yet spent three PATCH rounds and a READ
/// self-repairing a `]`-for-`}` slip without a check between them, because
/// cargo was skipped as compile-first; the finish-time check then took 7s.
pub fn edit_check_allowed(command: &str, known_ms: Option<i64>, build_warm: bool) -> bool {
    match known_ms {
        Some(ms) => ms <= EDIT_CHECK_MAX_KNOWN_MS,
        None => build_warm || !native_runner_name(command).is_some_and(|runner| EDIT_CHECK_SLOW_RUNNERS.contains(&runner)),
    }
}

/// Whether a check's duration is a fair measure of the check: not while the
/// run's warm-up build for the same runner is still compiling, because the
/// check then waits on the build lock and its time is the compile's. A
/// recorded pwrde run measured its first `cargo test features::` at 34s
/// that way and skipped every later edit check as "measured slow", though
/// the same check took 5s once the build was warm.
pub fn check_duration_measurable(warmup_command: Option<&str>, warmup_done: bool, command: &str) -> bool {
    match warmup_command {
        Some(warmup) if native_runner_name(warmup).is_some() && native_runner_name(warmup) == native_runner_name(command) => warmup_done,
        _ => true,
    }
}

/// The trailer on a PATCH result that carries the check's verdict.
pub fn edit_check_note(command: &str, passed: bool, verdict: &str, failure: &str) -> String {
    if passed {
        format!(
            "\n\n[harness] ran the goal-declared check after this edit: {command} -> {verdict}. That is the verification for the workspace as it stands: finish_task now (or put finish on the next PATCH if an edit remains); nothing needs to run it again."
        )
    } else {
        format!(
            "\n\n[harness] ran the goal-declared check after this edit: {command} -> {verdict}. Fix it in the next PATCH and put finish (summary + check) on that PATCH; the harness re-runs the check for the finish.\n{failure}"
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FinishRecheck {
    /// Nothing has ever run, or nothing correctness-class passed.
    Unchecked,
    /// Edits landed after the last check, or the last check failed and the
    /// workspace changed since — a re-run can settle the finish.
    Stale,
}

/// Why a bounced finish deserves a harness-run check, from the bounce text
/// and the edits since the last verification. A failed last check with no
/// edit since is not rechecked: the model has to change something first.
/// Whether a text-only reply reads as a completion report rather than a
/// plan, a question or a blocker: long enough to say something, no closing
/// question, none of the in-progress phrasings.
pub fn narration_reads_as_completion(text: &str) -> bool {
    const IN_PROGRESS: &[&str] = &[
        "next i", "i will", "i'll", "let me", "now i", "todo", "remaining", "not yet", "cannot", "can't", "unable",
        "blocked", "need to", "needs to", "should i", "would you", "waiting", "still failing", "does not pass", "doesn't pass",
    ];
    let trimmed = text.trim();
    if trimmed.chars().count() < 20 || trimmed.ends_with('?') {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    !IN_PROGRESS.iter().any(|marker| lower.contains(marker))
}

/// Whether the workspace is verified as it stands: edits landed, nothing
/// changed since the last check, and that check was an external
/// (goal-declared or project) suite that passed with tests executed.
pub fn verified_after_last_edit(state: &HarnessState) -> bool {
    if state.workspace_edits.unwrap_or(0) == 0 || state.mutations_since_verification.unwrap_or(1) != 0 {
        return false;
    }
    let Some(record) = state.last_verification.as_ref() else { return false };
    if record.failed || record.ran_no_tests == Some(true) {
        return false;
    }
    record
        .evidence
        .as_ref()
        .and_then(|evidence| evidence.anchor.as_ref())
        .is_some_and(|anchor| anchor.kind == crate::core::types::VerificationAnchorKind::External)
}

/// Whether every path the goal names has been edited this run. A
/// completion report that arrives with the edits ("the subcommand and its
/// test are in place") is only trusted as the finish when the files the
/// goal asked for were all touched: a goal that names tests/test_cli.py
/// is not done by a source-only round, however the reply reads.
pub fn goal_named_paths_all_edited(goal: &str, edited_paths: &[String]) -> bool {
    crate::harness::outline::named_paths_for_texts(&[goal])
        .iter()
        .all(|path| edited_paths.iter().any(|edited| edited == path))
}

pub fn finish_recheck_reason(bounce: &str, mutations_since: i64) -> Option<FinishRecheck> {
    if bounce.contains("no verification command (test/build/typecheck) has run") || bounce.contains("no correctness-class evidence") {
        return Some(FinishRecheck::Unchecked);
    }
    let edited_since = mutations_since > 0;
    // A green run that executed zero tests before the edit (a reviewer's
    // `cargo test rect::tests` on a file that had no tests yet) is as stale
    // as a failed one once the edit lands: re-run it rather than bounce.
    if edited_since
        && (bounce.contains("workspace edit(s) landed after the last verification")
            || bounce.contains("FAILED and nothing has passed since")
            || bounce.contains("exited green but executed zero tests"))
    {
        return Some(FinishRecheck::Stale);
    }
    None
}

/// The `check` a finish_task call names: the command the harness should
/// run before judging the finish when nothing fresh has passed. 107 of 115
/// recorded runs ended with a passing VERIFY round followed by a finish
/// round for the same task; naming the check in the finish folds the two
/// into one model turn. Blank and non-string values are ignored.
pub fn explicit_finish_check(raw_input: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(raw_input).ok()?;
    let command = value.get("check")?.as_str()?.trim();
    (!command.is_empty() && command.chars().count() < 400).then(|| command.to_string())
}

pub const GOAL_DECLARED_CHECK_MARKER: &str = "harnessGoalDeclaredCheck";

/// The one-call form of "edit, then finish": a PATCH whose input carries
/// `finish` ({summary, check} or a summary string) runs without the key,
/// and a synthetic finish_task call (status completed, that summary and
/// check) is dispatched right after it — so the edit lands first, the
/// finish-time check runs on it, and a failed edit bounces the finish. The
/// synthetic call joins the assistant turn's tool_calls, so the transcript
/// stays consistent for strict providers. A PATCH that carries a finish
/// and nothing to edit (no content, no find, an empty files list — the
/// model reached for the only tool with a `finish` field) is the finish
/// itself: it is replaced, under its own id, rather than run and failed.
/// Returns the calls and how many finishes were expanded. Models that will
/// not send two tool calls in one response (GLM sent 0 of 19 finishes with
/// its last PATCH when asked) do set a field on the call they are already
/// making.
pub fn expand_patch_finishes(calls: Vec<NormalizedCall>, used_ids: &mut HashSet<String>) -> (Vec<NormalizedCall>, usize) {
    let mut out = Vec::with_capacity(calls.len() + 1);
    let mut expanded = 0usize;
    for mut call in calls {
        if call.tool_name != "PATCH" {
            out.push(call);
            continue;
        }
        let Ok(mut input) = serde_json::from_str::<Value>(&call.raw_input) else {
            out.push(call);
            continue;
        };
        let Some(finish) = lift_patch_finish(&mut input) else {
            out.push(call);
            continue;
        };
        let (summary, check) = match &finish {
            Value::Object(fields) => (
                fields.get("summary").and_then(Value::as_str).unwrap_or("").trim().to_string(),
                fields.get("check").and_then(Value::as_str).map(str::trim).filter(|check| !check.is_empty()).map(str::to_string),
            ),
            Value::String(summary) => (summary.trim().to_string(), None),
            _ => (String::new(), None),
        };
        let stripped = input.to_string();
        call.raw_input = stripped.clone();
        if let Some(function) = call.normalized.function.as_mut() {
            function.arguments = Some(stripped);
        }
        if summary.is_empty() && check.is_none() {
            out.push(call);
            continue;
        }
        let mut finish_id = format!("{}-finish", call.call_id);
        while used_ids.contains(&finish_id) {
            finish_id.push('x');
        }
        used_ids.insert(finish_id.clone());
        let mut finish_input = serde_json::json!({ "status": "completed", "summary": summary });
        if let Some(check) = check {
            finish_input["check"] = Value::String(check);
        }
        let raw_input = finish_input.to_string();
        let edit_less = !patch_input_carries_an_edit(&input);
        let finish_id = if edit_less {
            used_ids.remove(&finish_id);
            call.call_id.clone()
        } else {
            finish_id
        };
        if !edit_less {
            out.push(call);
        }
        out.push(NormalizedCall {
            call_id: finish_id.clone(),
            normalized: crate::harness::transport::OpenAICompatibleToolCall {
                id: Some(finish_id),
                tool_type: Some("function".to_string()),
                function: Some(crate::harness::transport::OpenAICompatibleToolCallFunction {
                    name: Some("finish_task".to_string()),
                    arguments: Some(raw_input.clone()),
                }),
            },
            raw_input,
            tool_name: "finish_task".to_string(),
        });
        expanded += 1;
    }
    (out, expanded)
}

/// Takes the `finish` out of a PATCH input: from the top level, or from
/// the first files[] entry that carries one (recorded models nest it
/// there). Entries that would change nothing once a finish is carried —
/// identical find and replace, a `__noop__` path — are dropped, since they
/// only existed to hold the finish and would fail the edit and bounce it.
fn lift_patch_finish(input: &mut Value) -> Option<Value> {
    let mut finish = input.as_object_mut()?.remove("finish");
    if let Some(Value::Array(entries)) = input.get_mut("files") {
        for entry in entries.iter_mut() {
            if let Some(nested) = entry.as_object_mut().and_then(|fields| fields.remove("finish")) {
                finish.get_or_insert(nested);
            }
        }
        if finish.is_some() {
            entries.retain(|entry| !patch_entry_is_a_noop(entry));
        }
    }
    if finish.is_some() && patch_entry_is_a_noop(input) {
        if let Some(object) = input.as_object_mut() {
            for key in ["find", "replace", "content"] {
                object.remove(key);
            }
        }
    }
    finish
}

fn patch_entry_is_a_noop(entry: &Value) -> bool {
    let path_is_noop = entry.get("path").and_then(Value::as_str).is_some_and(|path| path.contains("__noop__"));
    let identical = match (entry.get("find"), entry.get("replace")) {
        (Some(Value::String(find)), Some(Value::String(replace))) => find == replace,
        _ => false,
    };
    path_is_noop || identical
}

/// Whether a PATCH input has anything to write: content, or find with
/// replace, at the top level or in a non-empty files list.
fn patch_input_carries_an_edit(input: &Value) -> bool {
    let has_edit = |object: &Value| {
        object.get("content").and_then(Value::as_str).is_some()
            || (object.get("find").and_then(Value::as_str).is_some() && object.get("replace").is_some())
    };
    if has_edit(input) {
        return true;
    }
    match input.get("files") {
        Some(Value::Array(entries)) => !entries.is_empty(),
        Some(Value::String(text)) => !text.trim().is_empty() && text.trim() != "[]",
        _ => false,
    }
}

/// The bounce for a completed finish_task sent in the same model response
/// as a workspace call that failed (`failed` holds those tools' names):
/// the finish counted on an edit or command that did not land. A blocked
/// or unreconciled finish, or a response with no failed call, passes.
pub fn finish_after_failed_call(raw_input: &str, failed: &[String]) -> Option<String> {
    if failed.is_empty() {
        return None;
    }
    let status = serde_json::from_str::<Value>(raw_input)
        .ok()
        .and_then(|input| input.get("status").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| "completed".to_string());
    if status != "completed" {
        return None;
    }
    let mut names: Vec<&str> = failed.iter().map(String::as_str).collect();
    names.dedup();
    Some(format!(
        "harness: not accepted — {} failed earlier in this same response, so the work this finish counts on is not in place. Fix that call, then finish_task (in the same response as the fix is fine).",
        names.join(" and ")
    ))
}

/// `declared_verification_anchor` plus one upgrade: a check the agent
/// labelled "self" whose command is one of the goal's own declared checks
/// is external evidence — the operator declared it, the agent only ran it.
/// Weak models label the goal's suite "self" because they added a test to
/// it; the harness then bounced the finish and re-ran the very same
/// command itself, one round and one suite run for nothing.
pub fn declared_verification_anchor_for_goal(
    raw_input: &str,
    edited_paths: &[String],
    goal: &str,
) -> Option<crate::core::types::VerificationAnchor> {
    let command = serde_json::from_str::<serde_json::Value>(raw_input)
        .ok()
        .and_then(|input| input.get("command").and_then(|value| value.as_str()).map(str::to_string))
        .unwrap_or_default();
    let declared = goal_declared_check_commands(goal);
    let goal_check = declared.iter().find(|check| command.contains(check.trim()));
    let Some(mut anchor) = declared_verification_anchor(raw_input, edited_paths) else {
        // No anchor field at all (BASH carries none): a command that runs
        // the goal-declared check is that check whoever typed it. A
        // recorded claude-web run ran `cd <root> && bun run typecheck &&
        // bun test …` through BASH, the record stayed "undeclared", and
        // the finish bounced for lack of an external anchor although the
        // harness had just reused that very record as the goal's check.
        let check = goal_check?;
        return Some(crate::core::types::VerificationAnchor {
            kind: crate::core::types::VerificationAnchorKind::External,
            source: Some(format!("goal-declared acceptance check: {check} (run by the agent; the goal declares this command)")),
            downgraded_reason: None,
            coverage: None,
            expectation_subject: None,
        });
    };
    if anchor.kind == crate::core::types::VerificationAnchorKind::SelfAuthored && anchor.downgraded_reason.is_none() {
        if let Some(check) = goal_check {
            anchor.kind = crate::core::types::VerificationAnchorKind::External;
            anchor.source = Some(format!("goal-declared acceptance check: {check} (declared self by the agent; the goal declares this command)"));
        }
    }
    Some(anchor)
}

/// A project test suite run through a native runner (cargo test, pytest,
/// unittest, go test, vitest, bun test, npm test) whose command names no file
/// this run edited is correctness-class evidence whoever labelled it: the
/// suite pre-exists the run even when the agent added a test to it. Recorded
/// sessions show "no correctness-class evidence" as the most common finish
/// rejection before a run hit its iteration cap (44 of 192 rejections), with
/// the agent having run the project suite and left the anchor undeclared or
/// labelled "self". Undeclared and undowngraded "self" anchors on such a
/// command are promoted to external; a check the harness downgraded because
/// its command names an edited file stays self-authored.
pub fn promote_native_runner_anchor(
    anchor: Option<crate::core::types::VerificationAnchor>,
    command: &str,
    evidence: &crate::core::types::VerificationEvidence,
    edited_paths: &[String],
) -> Option<crate::core::types::VerificationAnchor> {
    use crate::core::types::{VerificationAnchor, VerificationAnchorKind, VerificationEvidenceKind};
    if evidence.kind != VerificationEvidenceKind::Tests || evidence.executed <= 0 {
        return anchor;
    }
    let names_edited_file = edited_paths.iter().any(|path| {
        let name = std::path::Path::new(path).file_name().and_then(|name| name.to_str()).unwrap_or(path.as_str());
        !name.is_empty() && command.contains(name)
    });
    if names_edited_file {
        return anchor;
    }
    let runner = native_runner_name(command);
    let Some(runner) = runner else { return anchor };
    match anchor {
        None => Some(VerificationAnchor {
            kind: VerificationAnchorKind::External,
            source: Some(format!("project test suite via {runner} (anchor undeclared by the agent; promoted by the harness)")),
            downgraded_reason: None,
            coverage: None,
            expectation_subject: None,
        }),
        Some(mut declared)
            if declared.kind != VerificationAnchorKind::External && declared.downgraded_reason.is_none() =>
        {
            declared.kind = VerificationAnchorKind::External;
            declared.source = Some(match declared.source.take() {
                Some(source) => format!("project test suite via {runner} (declared self by the agent: {source}; promoted by the harness)"),
                None => format!("project test suite via {runner} (declared self by the agent; promoted by the harness)"),
            });
            Some(declared)
        }
        other => other,
    }
}

/// The text a VERIFY gets instead of running when it repeats the last
/// verification record's command with no edit in between: the record is
/// still current, so re-running proves nothing the run does not already
/// hold (dogfood #42 re-ran a 12s `cargo test` filter it had just recorded
/// from BASH, then finished on the duplicate). Only a passing record with
/// executed evidence is reused; a failed, empty, or stale record lets the
/// VERIFY run so the model sees fresh output.
/// Prefix of the text a reused VERIFY returns; the recorder skips such a
/// result so the current record stays instead of being replaced by an
/// empty one parsed from this text.
pub const VERIFY_REUSED_PREFIX: &str = "VERIFY not re-run:";

pub fn repeated_verify_reuse(
    command: &str,
    record: Option<&crate::core::types::HarnessVerificationRecord>,
    mutations_since: i64,
) -> Option<String> {
    let record = record?;
    if record.failed || record.ran_no_tests == Some(true) || mutations_since > 0 {
        return None;
    }
    let evidence = record.evidence.as_ref()?;
    if evidence.executed == 0 {
        return None;
    }
    let same = normalize_command_shape(&crate::tools::builtin::verify::strip_trailing_tail_pipe(command))
        == normalize_command_shape(&crate::tools::builtin::verify::strip_trailing_tail_pipe(&record.command));
    if !same {
        return None;
    }
    let id = record.id.as_deref().unwrap_or("the last record");
    Some(format!(
        "{VERIFY_REUSED_PREFIX} the same command already passed as verification record {id} at iteration {} ({} executed, {} failed) and nothing was edited since — that record is current, so cite it in finish_task instead of re-running. Re-run only after an edit.",
        record.at_iteration, evidence.executed, evidence.failed
    ))
}

/// A BASH call whose command is a native test runner piped into a trailing
/// `| tail -N` / `| head -N` gets the filter dropped, the way VERIFY does:
/// the filter hides the failure block (a recorded run saw only "FAILED. 0
/// passed; 1 failed" through `| tail -3` and spent the next round on
/// `| grep -A6 panicked`), while the harness already bounds runner output
/// and excerpts failures. Returns the rewritten input and the dropped
/// filter text, or the input untouched.
/// A BASH command longer than this gets a note in its result: a recorded
/// "prepare the PR" run spent 739s of its 1202s of inference on 24 calls
/// whose 1200-4000-token shell scripts (printf banners, a dozen sections)
/// each waited 20-45s to be generated before running.
pub const LONG_BASH_COMMAND_CHARS: usize = 1200;

pub fn long_bash_command_note(command: &str) -> Option<String> {
    let chars = command.chars().count();
    (chars > LONG_BASH_COMMAND_CHARS).then(|| {
        format!(
            "[harness] this command was {chars} chars (~{} output tokens, ~{}s to generate before it ran). Keep BASH calls to one command or a short pipeline and put independent checks in separate calls in the same round.",
            chars / 4,
            chars / 4 * 13 / 1000
        )
    })
}

/// READ windows of one file in one loop before the next READ of it returns
/// the whole file (when the file is at most READ_WHOLE_FILE_MAX_LINES): a
/// recorded run paged through an 845-line file in 66 windows, one round
/// each, when three or four full reads would have carried the same text.
pub const READ_WHOLE_FILE_AFTER: u32 = 3;
pub const READ_WHOLE_FILE_MAX_LINES: usize = 1500;
/// A promoted whole-file read bypasses the per-result truncation up to this
/// many chars (about 10K tokens): under the default 8000-char cap the first
/// dogfood of this lever handed the model the head and tail of a 32KB file
/// and it re-read the whole file twice more. One bounded read replaces the
/// pages the model would otherwise request one round at a time.
pub const READ_WHOLE_FILE_MAX_CHARS: usize = 40_000;

/// Rewrites a READ of a path already paged `prior_windows` times this loop
/// into a whole-file read, returning the new input and the note to append;
/// otherwise the input untouched.
/// The definition a goal places new code next to — "right after
/// `alpha`", "below the test beta", "before fn gamma" — as (after?, name).
/// Only identifier-shaped names count (an underscore inside or a camel
/// hump), so "after the tests pass" names nothing.
pub fn goal_placement_anchor(goal: &str) -> Option<(bool, String)> {
    let re = regex::Regex::new(
        r"(?i)\b(?:right|immediately|directly|just)?\s*(after|below|following|before|above)\s+(?:the\s+)?(?:existing\s+)?(?:test|fn|function|method|def|struct|class|impl|const|enum|block|definition|helper)?\s*`?([A-Za-z_][A-Za-z0-9_]*)`?",
    )
    .ok()?;
    for capture in re.captures_iter(goal) {
        let name = capture.get(2)?.as_str();
        let inner = &name[1..name.len().saturating_sub(1).max(1)];
        let identifier_shaped = name.len() >= 4
            && (inner.contains('_') || name.chars().zip(name.chars().skip(1)).any(|(a, b)| a.is_ascii_lowercase() && b.is_ascii_uppercase()));
        if !identifier_shaped {
            continue;
        }
        let word = capture.get(1)?.as_str().to_ascii_lowercase();
        let after = matches!(word.as_str(), "after" | "below" | "following");
        return Some((after, name.to_string()));
    }
    None
}

/// A PATCH append with no placement of its own takes the goal's: 3 of 3
/// recorded appends under a goal that said "right after `X`" landed at
/// the end of the file (or before its closing brace), and the model spent
/// READ + PATCH rounds moving the text — or left it there.
pub fn anchor_append_to_goal(raw_input: &str, goal: &str) -> (String, Option<String>) {
    let Some((after, name)) = goal_placement_anchor(goal) else { return (raw_input.to_string(), None) };
    let Ok(mut input) = serde_json::from_str::<serde_json::Value>(raw_input) else { return (raw_input.to_string(), None) };
    let key = if after { "after" } else { "before" };
    let mut anchored = false;
    let anchor_entry = |entry: &mut serde_json::Value, anchored: &mut bool| {
        let has_append = entry.get("append").and_then(|value| value.as_str()).is_some_and(|text| !text.is_empty());
        let placed = ["after", "before", "find"].iter().any(|field| entry.get(field).and_then(|value| value.as_str()).is_some_and(|text| !text.is_empty()));
        if has_append && !placed {
            entry[key] = serde_json::Value::String(name.clone());
            *anchored = true;
        }
    };
    if let Some(entries) = input.get_mut("files").and_then(|value| value.as_array_mut()) {
        for entry in entries.iter_mut() {
            anchor_entry(entry, &mut anchored);
        }
    } else {
        anchor_entry(&mut input, &mut anchored);
    }
    if !anchored {
        return (raw_input.to_string(), None);
    }
    (input.to_string(), Some(format!("append placed {key} `{name}` as the goal asks (pass after or before yourself to choose the place)")))
}

/// The workspace paths a PATCH input writes to (single-file and files[]).
pub fn patched_paths(raw_input: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw_input) else { return Vec::new() };
    let mut paths: Vec<String> = Vec::new();
    let mut push = |path: Option<&str>| {
        if let Some(path) = path.filter(|path| !path.is_empty()) {
            if !paths.iter().any(|known| known == path) {
                paths.push(path.to_string());
            }
        }
    };
    push(value.get("path").and_then(serde_json::Value::as_str));
    if let Some(entries) = value.get("files").and_then(serde_json::Value::as_array) {
        for entry in entries {
            push(entry.get("path").and_then(serde_json::Value::as_str));
        }
    }
    paths
}

/// The line range a READ covers: (offset, offset+limit), or the whole file
/// (0, i64::MAX) when it names no offset. None when the input is not a READ
/// of a path.
pub fn read_range_of(raw_input: &str) -> Option<(String, (i64, i64))> {
    let value = serde_json::from_str::<serde_json::Value>(raw_input).ok()?;
    let path = value.get("path")?.as_str()?.to_string();
    let range = match value.get("offset").and_then(serde_json::Value::as_i64) {
        Some(offset) => {
            let limit = value.get("limit").and_then(serde_json::Value::as_i64).unwrap_or(2000).max(1);
            (offset, offset.saturating_add(limit))
        }
        None => (0, i64::MAX),
    };
    Some((path, range))
}

/// A note when `range` overlaps a range already read this run: the lines are
/// still verbatim in the conversation, so re-reading an unedited file spends a
/// round for nothing. None when there is no overlap.
pub fn overlapping_read_note(seen: &[(i64, i64)], range: (i64, i64)) -> Option<String> {
    let (lo, hi) = range;
    let overlap = seen.iter().find(|(s, e)| lo < *e && *s < hi)?;
    let describe = |(s, e): (i64, i64)| if e == i64::MAX { "the whole file".to_string() } else { format!("lines {}-{}", s + 1, e) };
    Some(format!(
        "[harness] READ: {} of this file overlaps your earlier READ of {} this run, still verbatim in the conversation above — the file has not been edited since, so those lines are unchanged. Re-reading an unedited file spends a round; READ only a region you have not seen yet, or act on the copy above.",
        describe(range),
        describe(*overlap)
    ))
}

pub fn promote_read_to_whole_file(raw_input: &str, prior_windows: u32, cwd: &std::path::Path) -> (String, Option<String>) {
    if prior_windows < READ_WHOLE_FILE_AFTER {
        return (raw_input.to_string(), None);
    }
    let Ok(mut input) = serde_json::from_str::<serde_json::Value>(raw_input) else {
        return (raw_input.to_string(), None);
    };
    let Some(path) = input.get("path").and_then(|value| value.as_str()).map(str::to_string) else {
        return (raw_input.to_string(), None);
    };
    let full = if std::path::Path::new(&path).is_absolute() { std::path::PathBuf::from(&path) } else { cwd.join(&path) };
    let Ok(text) = std::fs::read_to_string(&full) else {
        return (raw_input.to_string(), None);
    };
    let lines = text.lines().count();
    if lines == 0 || lines > READ_WHOLE_FILE_MAX_LINES || text.chars().count() > READ_WHOLE_FILE_MAX_CHARS {
        return (raw_input.to_string(), None);
    }
    let offset = input.get("offset").and_then(|value| value.as_f64()).unwrap_or(1.0);
    let limit = input.get("limit").and_then(|value| value.as_f64()).unwrap_or(400.0);
    if offset <= 1.0 && limit as usize >= lines {
        return (raw_input.to_string(), None);
    }
    input["offset"] = serde_json::Value::from(1);
    input["limit"] = serde_json::Value::from(lines as u64);
    (
        input.to_string(),
        Some(format!(
            "[harness] READ window {} of {path} in this loop — the whole file ({lines} lines) is returned instead of another page, so read it once and stop paging.",
            prior_windows + 1
        )),
    )
}

pub fn drop_runner_tail_filter(raw_input: &str) -> (String, Option<String>) {
    let Ok(mut input) = serde_json::from_str::<serde_json::Value>(raw_input) else {
        return (raw_input.to_string(), None);
    };
    let Some(command) = input.get("command").and_then(|value| value.as_str()).map(str::to_string) else {
        return (raw_input.to_string(), None);
    };
    if native_runner_name(&command).is_none() {
        return (raw_input.to_string(), None);
    }
    let stripped = crate::tools::builtin::verify::strip_trailing_tail_pipe(&command);
    if stripped == command.trim() {
        return (raw_input.to_string(), None);
    }
    let dropped = command.trim()[stripped.len()..].trim().to_string();
    input["command"] = serde_json::Value::String(stripped);
    (input.to_string(), Some(dropped))
}

pub fn native_runner_name(command: &str) -> Option<&'static str> {
    const RUNNERS: &[(&str, &str)] = &[
        ("cargo test", "cargo test"),
        ("cargo nextest", "cargo nextest"),
        ("pytest", "pytest"),
        ("-m unittest", "unittest"),
        ("-m pytest", "pytest"),
        ("go test", "go test"),
        ("vitest", "vitest"),
        ("bun test", "bun test"),
        ("npm test", "npm test"),
        ("npm run test", "npm test"),
        ("pnpm test", "pnpm test"),
        ("yarn test", "yarn test"),
        ("jest", "jest"),
        ("mix test", "mix test"),
        ("dotnet test", "dotnet test"),
        ("mvn test", "mvn test"),
        ("gradle test", "gradle test"),
        ("./gradlew test", "gradle test"),
    ];
    RUNNERS.iter().find(|(needle, _)| command.contains(needle)).map(|(_, name)| *name)
}

pub fn declared_verification_anchor(
    raw_input: &str,
    edited_paths: &[String],
) -> Option<crate::core::types::VerificationAnchor> {
    use crate::core::types::{VerificationAnchor, VerificationAnchorKind};
    let input: serde_json::Value = serde_json::from_str(raw_input).ok()?;
    let goal_declared = input.get(GOAL_DECLARED_CHECK_MARKER).and_then(|value| value.as_bool()).unwrap_or(false);
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
    // Declared coverage: does the producer say this check covers the reported
    // claim itself, or only inputs/components? The harness enforces declared
    // coverage and record references only, not the semantic truth of the
    // declaration. Absent stays undeclared (legacy records).
    let coverage = match anchor.get("coverage").and_then(|value| value.as_str()) {
        Some("reportedClaim") => Some(crate::core::types::CoverageGranularity::ReportedClaim),
        Some("inputOrComponent") => Some(crate::core::types::CoverageGranularity::InputOrComponent),
        _ => None,
    };
    // Optional binding of this verification's evidence to a registered
    // expectation (id or subject). The harness enforces the record reference
    // exists, not the model's semantic claim about what its check covers.
    let expectation_subject = anchor
        .get("expectationSubject")
        .or_else(|| anchor.get("expectation_subject"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string);
    if kind != VerificationAnchorKind::External {
        return Some(VerificationAnchor {
            kind,
            source,
            downgraded_reason: None,
            coverage,
            expectation_subject: expectation_subject.clone(),
        });
    }
    // Only the command decides the downgrade: a suite run that happens to
    // include a test file this run touched is still mostly pre-existing
    // checks, and the free-text source must be allowed to mention that file
    // honestly without turning the whole run's evidence self-authored.
    let command = input.get("command").and_then(|value| value.as_str()).unwrap_or_default();
    let named = if goal_declared { None } else { edited_paths.iter().find(|path| command_names_path(command, path)) };
    match named {
        Some(path) => Some(VerificationAnchor {
            kind: VerificationAnchorKind::SelfAuthored,
            source,
            downgraded_reason: Some(format!(
                "the check names {path}, which this run edited; a check the agent authored is consistency, not correctness"
            )),
            coverage,
            expectation_subject,
        }),
        None => Some(VerificationAnchor {
            kind,
            source,
            downgraded_reason: None,
            coverage,
            expectation_subject,
        }),
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
    // The stem rule exists for runner targets that are test files
    // (`--test test_totals` ↔ tests/test_totals.rs). A source module's stem
    // used as a runner filter (`cargo test --lib child_process` ↔
    // src/tools/child_process.rs) runs that module's mostly pre-existing
    // tests: a recorded run had its 36-test filtered run downgraded to
    // self-authored on that match and then paid a full-suite re-run.
    let test_like_path = path.starts_with("tests/")
        || path.contains("/tests/")
        || path.starts_with("test/")
        || path.contains("/test/")
        || file_name.starts_with("test")
        || stem.ends_with("_test")
        || stem.ends_with("_tests")
        || stem.ends_with(".test")
        || stem.ends_with(".spec");
    let test_shaped_stem = test_like_path && stem.len() >= 4 && (stem.contains('_') || stem.contains('-'));
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

/// Check commands the goal itself declares in backticks (`python3 -m unittest
/// discover -s tests -q`, `cargo test`, `npm test`): task-provided acceptance
/// checks the harness may run on the agent's behalf.
/// How a run gets its first task list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanMode {
    /// The planner role always runs first.
    Always,
    /// Default. Small goals that declare their own acceptance check skip
    /// the planner: one direct task is seeded from the goal and the author
    /// starts at once. The planner run cost 13-20s on every run in the
    /// speed bench — half the wall time of a small task — while the goal
    /// already said what to do and how to check it; with auto the bench's
    /// S/M tasks ran 25-60% faster at the same hidden-test pass rate.
    Auto,
    /// Always seed the direct task, never plan.
    Direct,
}

impl PlanMode {
    pub fn parse(raw: Option<&str>) -> PlanMode {
        match raw.map(|value| value.trim().to_ascii_lowercase()).as_deref() {
            Some("always") => PlanMode::Always,
            Some("direct") => PlanMode::Direct,
            _ => PlanMode::Auto,
        }
    }
}

/// Goal size under which `PlanMode::Auto` skips the planner.
pub const DIRECT_PLAN_MAX_GOAL_CHARS: usize = 2500;
/// Paths a goal may name explicitly and still count as small.
pub const DIRECT_PLAN_MAX_PATHS: usize = 10;

/// The title of the direct task a run seeds instead of planning, or None
/// when this goal should be planned: `Always` never seeds; `Direct` always
/// does; `Auto` seeds only for a short goal naming few paths whose change
/// the harness can still verify — the goal declares its own backticked
/// acceptance check, or `project_check` says the workspace has a detectable
/// suite (see detect_project_check_command). Most real goals declare no
/// check, and every one of them paid a 13-16s planner call for a plan of
/// one task.
pub fn direct_task_title(goal: &str, mode: PlanMode, project_check: bool) -> Option<String> {
    let goal = goal.trim();
    if goal.is_empty() {
        return None;
    }
    let small = goal.chars().count() <= DIRECT_PLAN_MAX_GOAL_CHARS
        && extract_goal_paths(goal).len() <= DIRECT_PLAN_MAX_PATHS
        && (!goal_declared_check_commands(goal).is_empty() || project_check);
    let seed = match mode {
        PlanMode::Always => false,
        PlanMode::Direct => true,
        PlanMode::Auto => small,
    };
    if !seed {
        return None;
    }
    let first_line = goal.lines().next().unwrap_or(goal);
    Some(truncate_text(&collapse_whitespace(first_line), 200))
}

/// Every check the goal declares, joined as one `A && B` chain, so an
/// edit check or an unchecked finish runs all of them in one round. 498 of
/// 2,548 recorded goals named two or more commands (`cargo test --lib
/// harness` and `cargo test --test loop_smoke`); only the first ever ran
/// as the harness check, and the finish then bounced for the second.
pub fn goal_declared_check_chain(goal: &str) -> Option<String> {
    let mut commands = goal_declared_check_commands(goal);
    // A command that already contains another declared one (a chain the
    // goal spelled out) subsumes it.
    let full: Vec<String> = commands.clone();
    commands.retain(|command| !full.iter().any(|other| other != command && other.contains(command.as_str())));
    if commands.is_empty() {
        return None;
    }
    Some(commands.join(" && "))
}

/// Whether `command` has the shape of a goal-declared check: one declared
/// command, or the chain of all of them.
pub fn command_is_goal_declared(goal: &str, command: &str) -> bool {
    let shape = normalize_command_shape(command);
    goal_declared_check_commands(goal).iter().any(|declared| normalize_command_shape(declared) == shape)
        || goal_declared_check_chain(goal).is_some_and(|chain| normalize_command_shape(&chain) == shape)
}

/// Whether a stale finish should re-run the last record rather than the
/// goal-declared chain: the record already is the declared check, the goal
/// declares none, or the declared chain hung earlier.
pub fn rerun_keeps_goal_standing(goal: &str, record: &crate::core::types::HarnessVerificationRecord, hung_commands: &[String]) -> bool {
    match goal_declared_check_chain(goal) {
        None => true,
        Some(chain) => command_is_goal_declared(goal, &record.command) || hung_commands.iter().any(|hung| *hung == chain),
    }
}

pub fn goal_declared_check_commands(goal: &str) -> Vec<String> {
    let mut commands: Vec<String> = Vec::new();
    for span in goal.split('`').skip(1).step_by(2) {
        let candidate = collapse_whitespace(span.trim());
        if candidate.chars().count() < 8 || candidate.chars().count() > 200 || !candidate.contains(' ') || candidate.contains('\n') {
            continue;
        }
        if verification_pattern_matches(&candidate) && !commands.contains(&candidate) {
            commands.push(candidate);
        }
    }
    for candidate in plain_prose_check_commands(goal) {
        if !commands.contains(&candidate) {
            commands.push(candidate);
        }
    }
    commands
}

/// Runner phrases a goal may name in plain prose, longest first so
/// `python3 -m pytest` wins over `pytest`.
const PROSE_RUNNERS: &[&str] = &[
    "python3 -m unittest", "python -m unittest", "python3 -m pytest", "python -m pytest", "cargo nextest run", "cargo nextest",
    "cargo test", "npm run test", "npm test", "pnpm test", "yarn test", "bun test", "go test", "mix test", "dotnet test",
    "mvn test", "gradle test", "pytest",
    // Type checks, builds and lints a goal names beside its tests: a recorded
    // claude-web goal said "Verify with bun run typecheck && bun test …" and
    // only the bun test half was run, so a pre-existing type error never
    // reached the finish gate.
    "bun run typecheck", "npm run typecheck", "pnpm run typecheck", "pnpm typecheck", "yarn typecheck", "bunx tsc", "npx tsc",
    "tsc --noEmit", "bun run lint", "npm run lint", "bun run check", "npm run check", "cargo check", "cargo clippy", "cargo build",
    "go build", "go vet", "ruff check", "mypy",
];
/// Words that end a prose command: the argument list stops where the
/// sentence resumes.
const PROSE_STOP_WORDS: &[&str] = &[
    "the", "to", "and", "then", "must", "should", "pass", "passes", "passing", "green", "exit", "exits", "with", "for", "in",
    "is", "are", "before", "after", "that", "so", "which", "until", "once", "stays", "stay", "clean", "cleanly", "when", "if",
    "or", "as", "on", "at", "from", "by", "a", "an", "it", "this", "all", "again", "first", "last", "still", "also", "too",
    "of", "succeeds", "succeed", "runs", "run", "ok", "works", "without", "verify", "verifies", "check", "checks", "there",
    "here", "now", "finally", "please", "make", "sure", "keep", "keeps", "remains", "remain", "against",
];

/// Check commands a goal names in plain prose — `Run cargo test --release
/// --lib patch to check.` — read from the runner word to the end of the
/// clause: a token that ends with sentence punctuation is the last one, and
/// an English stop word ends the argument list. Text inside backticks is
/// left to the backticked scan. 272 of 2,522 recorded goals named a runner
/// this way and got the detected whole-project suite instead: a targeted
/// release module test became `cargo test -q` over everything, in the
/// debug profile the warm-up had built.
pub fn plain_prose_check_commands(goal: &str) -> Vec<String> {
    let outside: String = goal.split('`').step_by(2).collect::<Vec<&str>>().join(" ");
    let mut commands: Vec<String> = Vec::new();
    for line in outside.lines() {
        let mut from = 0usize;
        loop {
            let rest = &line[from..];
            let Some((offset, runner)) = PROSE_RUNNERS
                .iter()
                .filter_map(|runner| rest.find(runner).map(|offset| (offset, *runner)))
                .min_by_key(|(offset, runner)| (*offset, std::cmp::Reverse(runner.len())))
            else {
                break;
            };
            let start = from + offset;
            from = start + runner.len();
            let preceded_by_word = start > 0 && line.as_bytes()[start - 1].is_ascii_alphanumeric();
            // A runner opening a quoted string is a code sample in the prose
            // (`Some("cargo test --no-run")` in a goal describing a test),
            // not a command to run: a recorded run warmed up on
            // `cargo test --lib"` from such a sample instead of the declared
            // `cargo test --release --lib`, compiling the wrong profile.
            let preceded_by_quote = start > 0 && matches!(line.as_bytes()[start - 1], b'"' | b'\'');
            let followed_by_word = line.as_bytes().get(from).is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'-' || *byte == b'_');
            if preceded_by_word || preceded_by_quote || followed_by_word {
                continue;
            }
            // Runner words are exempt from the stop-word rule ("bun run
            // typecheck" keeps its "run"); a `&&` followed by another runner
            // joins the chain into one command, so "A && B" runs as A && B.
            let mut exempt_until = runner.split_whitespace().count();
            let mut tokens: Vec<String> = Vec::new();
            let mut cursor = start;
            let mut end = start;
            for (index, token) in line[start..].split_whitespace().enumerate() {
                let token_start = line[cursor..].find(token).map(|offset| cursor + offset).unwrap_or(cursor);
                cursor = token_start + token.len();
                let clean = token.trim_end_matches(|c: char| ".,;:)".contains(c));
                if clean == "&&" {
                    let rest = line[cursor..].trim_start();
                    match PROSE_RUNNERS.iter().filter(|next| rest.starts_with(*next)).max_by_key(|next| next.len()) {
                        Some(next) if !tokens.is_empty() => {
                            tokens.push("&&".to_string());
                            exempt_until = index + 1 + next.split_whitespace().count();
                            continue;
                        }
                        _ => break,
                    }
                }
                if clean.is_empty() || clean.starts_with('(') || clean == "||" || clean == "|" || clean.contains('"') {
                    break;
                }
                if index >= exempt_until && PROSE_STOP_WORDS.contains(&clean.to_ascii_lowercase().as_str()) {
                    break;
                }
                tokens.push(clean.to_string());
                end = token_start + clean.len();
                if clean.len() != token.len() {
                    break;
                }
            }
            if end > from {
                from = end;
            }
            let command = tokens.join(" ");
            let chars = command.chars().count();
            if tokens.len() >= 2 && chars >= 8 && chars <= 200 && verification_pattern_matches(&command) && !commands.contains(&command) {
                commands.push(command);
            }
        }
    }
    commands
}

/// One-line summary of a tool call's input for the activation digest: the
/// field that identifies what the call touched (command, path, pattern,
/// find, query, url), whitespace-collapsed and cut to `limit` chars.
pub fn compact_tool_input(raw_input: &str, limit: usize) -> String {
    let text = serde_json::from_str::<serde_json::Value>(raw_input)
        .ok()
        .and_then(|value| {
            ["command", "path", "pattern", "find", "query", "url", "title", "name"]
                .iter()
                .find_map(|key| value.get(key).and_then(|field| field.as_str()).map(str::to_string))
        })
        .unwrap_or_else(|| raw_input.to_string());
    truncate_text(&collapse_whitespace(text.trim()), limit)
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

    // Record identity: replaying the same command still allocates a new,
    // distinct record id, so citing "the same record" is detectable by id.
    #[test]
    fn next_verification_record_id_is_goal_unique_across_identical_replays() {
        use crate::core::types::{HarnessVerificationRecord, VerificationEvidence, VerificationEvidenceKind};
        let record = |id: Option<String>| HarnessVerificationRecord {
            at_iteration: 1,
            command: "cargo test".to_string(),
            failed: false,
            output_tail: String::new(),
            ran_no_tests: None,
            evidence: Some(VerificationEvidence {
                kind: VerificationEvidenceKind::Tests,
                executed: 1,
                passed: 1,
                failed: 0,
                skipped: None,
                detail: None,
                anchor: None,
            }),
            id,
        };
        let mut records = Some(vec![record(Some("v3".to_string())), record(None)]);
        assert_eq!(next_verification_record_id(&records), "v4");
        let v4 = next_verification_record_id(&records).to_string();
        records.as_mut().unwrap().push(record(Some(v4)));
        assert_eq!(next_verification_record_id(&records), "v5");
        // Legacy record with no id does not disturb numbering: the next id
        // still follows the highest explicit id (v4), not the record count.
        records.as_mut().unwrap().push(record(None));
        assert_eq!(next_verification_record_id(&records), "v5");
        // Empty timeline starts at v1.
        assert_eq!(next_verification_record_id(&None), "v1");
        assert_eq!(next_verification_record_id(&Some(vec![])), "v1");
    }

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

        let folded_count = fold_cold_tool_results(&mut messages, 1, &mut folded, 0);

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
    fn fold_pins_the_freshest_read_of_each_file_past_the_hot_window() {
        let read = |path: &str| {
            tool_message("READ", &format!("Read lines 1-3 of 3 from {path}.\n1\tone\n2\ttwo\n3\tthree"))
        };
        let mut messages = vec![
            read("src/a.rs"),                    // 0: superseded by the later read of a
            read("src/b.rs"),                    // 1: freshest read of b -> pinned
            read("src/a.rs"),                    // 2: freshest read of a -> pinned
            tool_message("BASH", "test output"), // 3: hot (window of 1)
        ];
        let mut folded = HashSet::new();

        let folded_count =
            fold_cold_tool_results(&mut messages, 1, &mut folded, MAX_PINNED_READ_FILES);

        // Only the stale earlier read of a folds; the freshest read of each file
        // stays verbatim so the model does not re-read it next round.
        assert_eq!(folded_count, 1);
        assert_eq!(folded, HashSet::from([0usize]));
        assert!(message_text(&messages[1]).starts_with("Read lines 1-3 of 3 from src/b.rs."));
        assert!(message_text(&messages[2]).starts_with("Read lines 1-3 of 3 from src/a.rs."));
    }

    #[test]
    fn fold_does_not_pin_a_read_the_file_was_patched_past() {
        let mut messages = vec![
            tool_message("READ", "Read lines 1-2 of 2 from src/c.rs.\n1\tx\n2\ty"), // 0: now stale
            tool_message(
                "PATCH",
                "Applied change to src/c.rs.\n--- a/src/c.rs\n+++ b/src/c.rs\n@@ -1,1 +1,1 @@\n-x\n+z",
            ), // 1: edits c after the read
            tool_message("BASH", "hot"),                                            // 2: hot
        ];
        let mut folded = HashSet::new();

        let folded_count =
            fold_cold_tool_results(&mut messages, 1, &mut folded, MAX_PINNED_READ_FILES);

        // The read is superseded by the patch, so it is not pinned and folds
        // like the patch result — the model never sees stale file content pinned.
        assert_eq!(folded_count, 2);
        assert!(folded.contains(&0));
    }

    #[test]
    fn fold_does_not_pin_a_read_a_later_shell_command_edited() {
        let bash_call = |command: &str| TransportRequestMessage {
            role: ChatRoleTag::Assistant,
            tool_calls: Some(vec![crate::harness::transport::OpenAICompatibleToolCall {
                function: Some(crate::harness::transport::OpenAICompatibleToolCallFunction {
                    arguments: Some(serde_json::json!({ "command": command }).to_string()),
                    name: Some("BASH".to_string()),
                }),
                id: Some("c".to_string()),
                tool_type: Some("function".to_string()),
            }]),
            ..Default::default()
        };
        let mut messages = vec![
            tool_message("READ", "Read lines 1-2 of 2 from src/d.rs.\n1\tx\n2\ty"), // 0: read of d
            bash_call("sed -i '' 's/x/z/' src/d.rs"),                              // 1: shell edit of d
            tool_message("BASH", "done"),                                          // 2
            tool_message("BASH", "hot"),                                           // 3: hot
        ];
        let mut folded = HashSet::new();

        let folded_count =
            fold_cold_tool_results(&mut messages, 1, &mut folded, MAX_PINNED_READ_FILES);

        // The read (0) is not pinned — a later mutating shell command named d.rs.
        assert!(folded.contains(&0), "stale read must fold, folded={folded:?}");
        assert!(folded_count >= 1);
        // A read of a DIFFERENT file the command did not name stays pinned.
        let mut messages2 = vec![
            tool_message("READ", "Read lines 1-2 of 2 from src/other.rs.\n1\tx\n2\ty"), // 0
            bash_call("sed -i '' 's/x/z/' src/d.rs"),                                   // 1: edits d, not other
            tool_message("BASH", "hot"),                                                // 2
        ];
        let mut folded2 = HashSet::new();
        fold_cold_tool_results(&mut messages2, 1, &mut folded2, MAX_PINNED_READ_FILES);
        assert!(!folded2.contains(&0), "unrelated read should stay pinned");
    }

    #[test]
    fn read_result_path_reads_the_header_and_patch_paths_read_the_diff() {
        assert_eq!(
            super::read_result_path("Read lines 1-9 of 9 from src/web/settings.ts.\n1\tx").as_deref(),
            Some("src/web/settings.ts")
        );
        assert_eq!(super::read_result_path("no header here"), None);
        assert_eq!(
            super::patch_result_paths("--- a/x.rs\n+++ b/x.rs\n+++ b/y.rs\n line"),
            vec!["x.rs".to_string(), "y.rs".to_string()]
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

    // "a review loop's carryover drops PATCH exchanges and keeps the rest"
    #[test]
    fn review_carryover_excludes_patch_exchanges() {
        fn exchange(id: &str, tool: &str, content: &str) -> Vec<TransportRequestMessage> {
            vec![
                TransportRequestMessage {
                    role: ChatRoleTag::Assistant,
                    tool_calls: Some(vec![crate::harness::transport::OpenAICompatibleToolCall {
                        function: Some(crate::harness::transport::OpenAICompatibleToolCallFunction {
                            arguments: Some("{}".to_string()),
                            name: Some(tool.to_string()),
                        }),
                        id: Some(id.to_string()),
                        tool_type: Some("function".to_string()),
                    }]),
                    ..Default::default()
                },
                TransportRequestMessage {
                    content: Some(TransportContent::Text(content.to_string())),
                    name: Some(tool.to_string()),
                    role: ChatRoleTag::Tool,
                    tool_call_id: Some(id.to_string()),
                    ..Default::default()
                },
            ]
        }
        let mut messages = Vec::new();
        messages.extend(exchange("p", "PATCH", "whole new file body"));
        messages.extend(exchange("r", "READ", "file contents"));
        messages.extend(exchange("v", "VERIFY", "cargo test ok"));

        let kept = extract_loop_carryover_excluding(&messages, 6, &["PATCH"]);
        let tools: Vec<&str> = kept.iter().filter_map(|m| m.name.as_deref()).collect();
        assert_eq!(tools, vec!["READ", "VERIFY"]);

        // Unfiltered selection is unchanged.
        assert_eq!(extract_loop_carryover(&messages, 6).len(), 6);
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

        // The source text naming an edited file is not a downgrade on its
        // own: only the command decides what the check exercises.
        let by_source_only = declared_verification_anchor(
            r#"{"command":"pytest -k totals","anchor":{"kind":"external","source":"test_totals.py fixture"}}"#,
            &edited,
        )
        .expect("anchor declared");
        assert_eq!(by_source_only.kind, VerificationAnchorKind::External);
        let by_basename = declared_verification_anchor(
            r#"{"command":"pytest test_totals.py","anchor":{"kind":"external","source":"project suite"}}"#,
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
        // A source module's stem as a runner filter is not the file's own test.
        assert!(!command_names_path("cargo test --lib child_process", "src/tools/child_process.rs"));
        assert!(!command_names_path("cargo test --lib builtin::bash", "src/tools/builtin/bash.rs"));
        assert!(command_names_path("cargo test --test child_process_test", "tests/child_process_test.rs"));
        assert!(command_names_path("bun test session_name.test", "hub/test/session_name.test.ts"));

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
        // Declared fields the raw anchor omits stay absent; a self-authored
        // declaration is recorded as-is, never downgraded.
        assert_eq!(declared_self.coverage, None);
        assert_eq!(declared_self.expectation_subject, None);

        // No anchor object at all: the parser records an absent anchor
        // (None) rather than fabricating a default.
        assert!(declared_verification_anchor(r#"{"command":"cargo test"}"#, &edited).is_none());

    }

    // Declared coverage and the expectation binding ride inside the anchor
    // object (no schema change) and must parse into the recorded anchor.
    #[test]
    fn anchor_coverage_and_expectation_binding_are_parsed() {
        use crate::core::types::CoverageGranularity;
        let edited: Vec<String> = Vec::new();
        let component = declared_verification_anchor(
            r#"{"command":"cargo build","anchor":{"kind":"external","source":"toolchain","coverage":"inputOrComponent"}}"#,
            &edited,
        )
        .unwrap();
        assert_eq!(component.coverage, Some(CoverageGranularity::InputOrComponent));
        let bound = declared_verification_anchor(
            r#"{"command":"cargo test","anchor":{"kind":"external","source":"suite","coverage":"reportedClaim","expectationSubject":"e2"}}"#,
            &edited,
        )
        .unwrap();
        assert_eq!(bound.coverage, Some(CoverageGranularity::ReportedClaim));
        assert_eq!(bound.expectation_subject.as_deref(), Some("e2"));
        let bare = declared_verification_anchor(
            r#"{"command":"cargo test","anchor":{"kind":"external","source":"suite"}}"#,
            &edited,
        )
        .unwrap();
        assert_eq!(bare.coverage, None);
        assert_eq!(bare.expectation_subject, None);
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
    record_tool_telemetry, tool_telemetry_key, truncate_text_keeping_ends,
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

/// The MCP server set a loop may call: the role's `mcpServers` when it sets
/// one, else the run-level `--mcp` set. A run-level empty set (`--no-mcp`) is
/// a hard off that wins over any role.
pub fn effective_mcp_servers(
    role: Option<&HarnessRoleRuntime>,
    run_gate: Option<Vec<String>>,
) -> Option<Vec<String>> {
    if run_gate.as_ref().map(|servers| servers.is_empty()).unwrap_or(false) {
        return Some(Vec::new());
    }
    match role.and_then(|role| role.mcp_servers.as_ref()) {
        Some(servers) => Some(servers.clone()),
        None => run_gate,
    }
}

/// Whether a workspace tool belongs in a loop's tool surface — the per-loop
/// half of filterToolsForRole. Non-MCP tools follow the role's `tool_names`
/// allowlist (None = every tool). MCP tools (`MCP__<server>__<tool>`, see
/// `mcp_server_of`) ignore that allowlist: `mcpServers` is their opt-in, so
/// they are in scope iff their server is in the loop's effective set.
pub fn loop_allows_tool(
    tool_name: &str,
    role_tool_names: Option<&[String]>,
    mcp_servers_for_loop: Option<&[String]>,
) -> bool {
    match crate::tools::mcp::mcp_server_of(tool_name) {
        None => role_tool_names.map_or(true, |names| names.iter().any(|name| name == tool_name)),
        Some(server) => mcp_servers_for_loop.map_or(false, |servers| servers.iter().any(|allowed| allowed == server)),
    }
}

/// The loop's effective tool surface: `filterToolsForRole` plus the MCP gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopToolScope {
    /// Tool names this loop may call (an MCP tool is here only when its
    /// server is in `mcp_servers`).
    pub allowed: Vec<String>,
    pub mcp_servers: Option<Vec<String>>,
}

/// Resolves a loop's tool surface from a tool-name list, the loop's role, and
/// the run-level MCP gate. `begin_loop` builds its tool set from this and the
/// classifier's requirements gate asks the same question, so the two can never
/// disagree about what a loop can actually call.
pub fn loop_tool_scope(
    tool_names: &[String],
    role: Option<&HarnessRoleRuntime>,
    run_gate: Option<Vec<String>>,
) -> LoopToolScope {
    let mcp_servers = effective_mcp_servers(role, run_gate);
    let role_tool_names = role.and_then(|role| role.tool_names.as_deref());
    let allowed = tool_names
        .iter()
        .filter(|name| loop_allows_tool(name, role_tool_names, mcp_servers.as_deref()))
        .cloned()
        .collect();

    LoopToolScope { allowed, mcp_servers }
}

/// Whether every requirement a skill is KNOWN to have is present in the loop's
/// surface. Unknown requirements (`known == false`) are treated as satisfied:
/// the classifier being down must never hide a skill from a loop.
pub fn requirements_satisfied(
    requirements: &crate::core::skill_requirements::SkillRequirements,
    available: &std::collections::BTreeSet<String>,
) -> bool {
    if !requirements.known {
        return true;
    }

    requirements.required.iter().all(|name| available.contains(name))
}

#[cfg(test)]
mod dynamic_skills_tests {
    use super::*;
    use crate::core::skill_requirements::SkillRequirements;
    use crate::harness::classifier::DynamicSkill;
    use std::collections::BTreeSet;

    fn requirement(required: &[&str], known: bool) -> SkillRequirements {
        SkillRequirements {
            required: required.iter().map(|name| name.to_string()).collect(),
            known,
        }
    }

    fn dynamic(name: &str, required: &[&str], known: bool) -> DynamicSkill {
        DynamicSkill {
            name: name.to_string(),
            description: format!("{name} description"),
            content: format!("# {name}"),
            classifiers: None,
            requirements: requirement(required, known),
        }
    }

    fn mcp_role(mcp_servers: Option<Vec<&str>>, tool_names: Option<Vec<&str>>) -> HarnessRoleRuntime {
        HarnessRoleRuntime {
            description: None,
            r#loop: None,
            name: "author".to_string(),
            route: None,
            system_prompt_suffix: None,
            tool_names: tool_names.map(|names| names.into_iter().map(String::from).collect()),
            verified_by: None,
            blind: false,
            mcp_servers: mcp_servers.map(|names| names.into_iter().map(String::from).collect()),
        }
    }

    fn strings(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    fn names(skills: &[DynamicSkill]) -> Vec<&str> {
        skills.iter().map(|skill| skill.name.as_str()).collect()
    }

    // The scope the classifier gates on is the same one begin_loop builds its
    // tool surface from: MCP servers are in scope only through the role's
    // mcpServers or the run-level --mcp set.
    #[test]
    fn loop_tool_scope_matches_the_loops_role_and_mcp_gate() {
        let tools = strings(&["READ", "PATCH", "MCP__github__search", "MCP__files__read"]);

        let scoped =
            loop_tool_scope(&tools, Some(&mcp_role(Some(vec!["github"]), Some(vec!["READ"]))), None);
        assert_eq!(scoped.allowed, strings(&["READ", "MCP__github__search"]));
        assert_eq!(scoped.mcp_servers, Some(strings(&["github"])));

        // No role: the run-level gate decides MCP, and every builtin is in scope.
        let ungated = loop_tool_scope(&tools, None, Some(strings(&["files"])));
        assert_eq!(ungated.allowed, strings(&["READ", "PATCH", "MCP__files__read"]));

        // --no-mcp: no MCP tool belongs to any loop.
        let closed = loop_tool_scope(&tools, None, Some(Vec::new()));
        assert_eq!(closed.allowed, strings(&["READ", "PATCH"]));
        assert_eq!(closed.mcp_servers, Some(Vec::new()));
    }

    // The requirements gate: known-but-unsatisfiable is filtered out before any
    // classifier call (this is a pure fn, so the filter is tested without a
    // server); unknown requirements fail open so a classifier outage can never
    // hide a skill.
    #[test]
    fn the_requirements_filter_drops_only_known_unsatisfiable_skills() {
        let available: BTreeSet<String> =
            strings(&["READ", "PATCH", "github"]).into_iter().collect();

        assert!(requirements_satisfied(&requirement(&[], false), &available));
        assert!(requirements_satisfied(&requirement(&["MISSING"], false), &available));
        assert!(requirements_satisfied(&requirement(&[], true), &available));
        assert!(requirements_satisfied(&requirement(&["READ", "github"], true), &available));
        assert!(!requirements_satisfied(&requirement(&["MISSING"], true), &available));
        assert!(!requirements_satisfied(&requirement(&["READ", "BASH"], true), &available));

        let pool = vec![
            dynamic("satisfiable", &["READ"], true),
            dynamic("unsatisfiable", &["BASH"], true),
            dynamic("unknown", &["BASH"], false),
        ];
        let offered: Vec<DynamicSkill> = pool
            .into_iter()
            .filter(|skill| requirements_satisfied(&skill.requirements, &available))
            .collect();

        assert_eq!(names(&offered), vec!["satisfiable", "unknown"]);
    }

    /// A minimal real HarnessRun over a temp state path (the same shape
    /// `role_inference_tests::role_inference_test_run` uses; that helper is
    /// private to its own test module).
    async fn test_run_for_dynamic_skills() -> HarnessRun {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::mem::forget(dir); // keep the backing dir alive for the run's duration
        HarnessRun::new(SolidStateHarnessOptions {
            goal: "test goal".into(),
            state_path: Some(path),
            ..SolidStateHarnessOptions::default()
        })
        .await
        .unwrap()
    }

    // A no-classifier run must not compose anything, and a loop that previously
    // selected skills must clear them when nothing is selected this time.
    #[tokio::test]
    async fn select_dynamic_skills_is_a_noop_without_a_classifier_or_a_pool() {
        let mut run = test_run_for_dynamic_skills().await;
        run.dynamic_skills = vec![crate::cli::skills::LoadedCliSkill {
            content: "body".to_string(),
            name: "stale".to_string(),
            role_hints: None,
        }];
        run.options.classifier = None;
        run.options.skill_pool = vec![dynamic("pooled", &[], true)];

        run.select_dynamic_skills().await;

        assert!(
            run.dynamic_skills.is_empty(),
            "a loop without a classifier result must compose none"
        );
        assert!(run.dynamic_skill_cache.is_empty());
    }

    // An outage is a one-loop miss, never a whole-task one: the failed
    // selection composes nothing now, but it is not cached, so the next loop
    // on the same task asks the classifier again.
    #[tokio::test]
    async fn a_failed_selection_is_not_cached_for_the_task() {
        let mut run = test_run_for_dynamic_skills().await;
        run.tools = Vec::new();
        run.options.classifier = Some(crate::harness::classifier::ClassifierRoute {
            // Nothing listens on port 1: the request fails at connect.
            url: "http://127.0.0.1:1/alpha/decisions".to_string(),
            model: "jev-test".to_string(),
            headers: Vec::new(),
            timeout_ms: 500,
        });
        run.options.skill_pool = vec![dynamic("offered", &[], true)];

        run.select_dynamic_skills().await;

        assert!(run.dynamic_skills.is_empty());
        assert!(
            run.dynamic_skill_cache.is_empty(),
            "a warning-tainted selection must not be remembered for the task"
        );
    }

    #[tokio::test]
    async fn a_pool_with_a_route_but_no_satisfiable_skill_composes_none() {
        let mut run = test_run_for_dynamic_skills().await;
        run.tools = Vec::new();
        run.options.classifier = Some(crate::harness::classifier::ClassifierRoute {
            url: "http://127.0.0.1:1/alpha/decisions".to_string(),
            model: "jev-test".to_string(),
            headers: Vec::new(),
            timeout_ms: 200,
        });
        // No tool this loop can call, so the only pooled skill is filtered
        // before any request would have been made.
        run.options.skill_pool = vec![dynamic("needs-patch", &["PATCH"], true)];

        run.select_dynamic_skills().await;

        assert!(run.dynamic_skills.is_empty());
        assert!(
            run.dynamic_skill_cache.is_empty(),
            "a skill filtered by requirements must be decided without a classifier call"
        );
    }
}

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
    /// Cap on task loops for this run. Each loop is at least one model call
    /// (a replanning loop is exactly one planner call), so this bounds the
    /// expensive event directly where --max-iterations bounds cycles.
    pub max_loops: Option<i64>,
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
    /// Task loops one task may consume before the harness blocks it
    /// (default DEFAULT_TASK_LOOP_LIMIT).
    pub task_loop_limit: Option<i64>,
    /// Override for REVIEW_WAIVER_MAX_LINES (None = default; 0 disables the waiver).
    pub review_waiver_lines: Option<usize>,
    /// Planning mode: "auto" (default) seeds a direct task for small goals
    /// that declare their own check; "always" runs the planner first;
    /// "direct" always seeds one (see PlanMode).
    pub plan_mode: Option<String>,
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
    /// Optional per-loop skill classifier route. `None` (the default) disables
    /// the feature entirely: no pool, no requests, only the explicit --skill
    /// activations.
    pub classifier: Option<crate::harness::classifier::ClassifierRoute>,
    /// Discovered skills the classifier may compose into a loop. Empty by
    /// default, and never the explicit --skill activations (those are already
    /// in the base prompt).
    pub skill_pool: Vec<crate::harness::classifier::DynamicSkill>,
    pub system_prompt: Option<String>,
    pub telemetry: Option<PartialHarnessTelemetryConfig>,
    pub redact_secrets: Vec<(String, String)>,
    pub tool_route: Option<ModelRoute>,
    pub fallback_route: Option<ModelRoute>,
    pub tool_services: Option<ChatToolRuntimeServices>,
    pub tools: Vec<ChatToolDefinition>,
    pub url: Option<String>,
    /// Run-level MCP gate: `Some(empty)` = `--no-mcp` (no loop sees MCP tools); `Some(names)` =
    /// `--mcp` servers for loops whose role does not set `mcpServers`; `None` = roles decide.
    pub mcp_servers: Option<Vec<String>>,
    /// Persisted per-model latency samples path (None = LATENCY_STORE_FILE under
    /// the drip home). Tests point this at their temp dir so a run never reads
    /// or writes the operator's real latency memory.
    pub latency_store: Option<PathBuf>,
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

/// Per-role inference accounting: one bucket per loop role ("default" when
/// the loop has no role), summed from every model-call usage record.
pub use crate::core::types::RoleInferenceTotals;

/// Run-scoped state: options, config, state, usage, and the model caller.
pub struct HarnessRun {
    pub usage_inbox: Arc<UsageInbox>,
    /// Current loop role name; set in begin_loop, taken for accounting.
    pub active_role: Option<String>,
    /// Accumulated per-role inference totals (calls / latency / completion
    /// tokens), keyed by loop role name; "default" for roleless loops.
    pub role_inference: std::collections::BTreeMap<String, RoleInferenceTotals>,
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
    /// `git rev-parse HEAD` at run start (None outside a git repo): the base
    /// the reviewer's brief is diffed against, so the review sees the run's
    /// whole change set even when the author committed along the way.
    pub run_start_head: Option<String>,
    /// `Number.POSITIVE_INFINITY` when unset → `i64::MAX`.
    pub max_iterations: i64,
    /// `i64::MAX` when unset.
    pub max_loops: i64,
    pub loop_config: HarnessLoopConfig,
    pub stall_limit: i64,
    pub task_loop_limit: i64,
    pub review_waiver_lines: Option<usize>,
    /// BASH/VERIFY command texts that hung (timed out) this run; an identical
    /// re-run is refused instead of hanging again.
    pub hung_commands: Vec<String>,
    /// Wall time of the last run of each check command shape this run
    /// (normalize_command_shape); run_edit_check consults it.
    pub check_durations_ms: HashMap<String, i64>,
    /// Normalized shapes (see normalize_command_shape) of commands that hung
    /// this run. A re-run of the same shape under a different spelling
    /// (`timeout 90 …`, `-v`, another tail filter) is not refused but runs
    /// under `hung_shape_leash_ms` instead of the default timeout, unless
    /// the call sets its own timeout. Not cleared by edits: a fixed hang
    /// finishes well inside the leash, a live one is cut short.
    pub hung_shapes: Vec<(String, String)>,
    /// Timeout applied to a re-run of a hung shape (tests shorten it).
    pub hung_shape_leash_ms: u64,
    /// The build warm-up started at run start (see build_warmup_command); dropped (killed) with the run.
    pub warmup: Option<crate::tools::child_process::WarmupJob>,
    /// The last BASH/VERIFY command shape and how many times in a row it ran
    /// with no workspace edit in between (see repeated_command_nudge).
    pub repeated_command: Option<(String, u32)>,
    pub plan_mode: PlanMode,
    pub max_task_reopens: i64,
    pub system_prompt: String,
    pub telemetry_config: HarnessTelemetryConfig,
    pub dynamic_tool_names: HashSet<String>,
    /// This loop's classifier-selected skills. Cleared and rebuilt by
    /// `select_dynamic_skills` on every loop, so a loop without a classifier
    /// result composes none.
    pub dynamic_skills: Vec<crate::cli::skills::LoadedCliSkill>,
    /// Selections already paid for, keyed by the loop's task id (`None` for
    /// planning loops): a task re-activated in a later loop reuses its
    /// selection instead of calling the classifier again.
    pub dynamic_skill_cache: HashMap<Option<String>, Vec<crate::cli::skills::LoadedCliSkill>>,
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
    /// Nothing workable remains and a task is blocked on operator input.
    pub blocked_on_input: bool,
    /// The last replanning loop left the ledger unworkable: the next one runs
    /// under the planning role instead of the (cheaper) replanning role.
    pub replan_escalated: bool,
    /// The direct-plan decision runs once per run, before the first loop.
    pub direct_plan_checked: bool,
    pub ask_user_awaiting: bool,
    /// Planning ask window: true at run start and again right after a fresh
    /// operator message, false once a task loop starts (ask_user is useless
    /// while a plan already executes).
    pub ask_window_open: bool,
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
    /// READ windows opened on each path this loop, so the fourth window of
    /// one file returns the whole file instead of another page.
    pub read_windows: HashMap<String, u32>,
    /// Line ranges already READ this run per path, cleared when the file is
    /// edited. A READ overlapping one of these (without an edit since) re-sends
    /// lines already in the conversation: 359 of 1,525 recorded READs did this
    /// with a shifted window the identical-call check never caught.
    pub read_ranges: HashMap<String, Vec<(i64, i64)>>,
    pub used_tool_call_ids: HashSet<String>,
    pub affordable_cycles: i64,
    /// The cycle currently running (1-based; 0 before the first begins).
    pub cycle: i64,
    pub task_finished: bool,
    pub made_progress: bool,
    /// A workspace edit or verification happened in the current cycle —
    /// the cycle-extension signal (reset in begin_cycle).
    pub progress_this_cycle: bool,
    /// What earned that progress: a workspace edit, a verification record, or
    /// both (reset in begin_cycle).
    pub edit_progress_this_cycle: bool,
    pub verification_progress_this_cycle: bool,
    /// Cycles granted past `max_cycles` this loop (≤ MAX_CYCLE_EXTENSIONS).
    pub cycle_extensions: i64,
    /// The loop works a review task: reads are its job, so the read-only
    /// nudge stays quiet.
    pub review_loop: bool,
    /// Successful READ/GREP/DIR calls so far this loop, and whether anything
    /// has been written or recorded — the read-only nudge's inputs.
    pub read_only_calls_this_loop: i64,
    pub persisted_this_loop: bool,
    pub verification_stuck_this_loop: bool,
    pub narration_nudge_used: bool,
    pub truncation_nudge_used: bool,
    /// Set by the truncation nudge: the next model call forces a tool call
    /// (tool_choice "required") under a tight output cap, so the retry
    /// cannot run away into another multi-minute narration.
    pub force_tool_call_next_round: bool,
    pub tool_calls_this_loop: i64,
    pub overflow_retried_this_loop: bool,
    pub concluded_naturally: bool,
    pub planned_and_yielded: bool,
    /// Set when a planning loop's plan_tasks landed work: the round loop
    /// ends after this dispatch instead of paying for one more model call
    /// that can only narrate the plan.
    pub plan_yield_requested: bool,
    /// The harness ran the goal-declared check on the agent's behalf once
    /// this loop; a second bounce is the model's to handle.
    /// Checks the harness ran for finish_task calls this loop (explicit
    /// `check`, goal-declared, or detected project check); capped so a
    /// finish that keeps failing cannot burn the loop on re-runs.
    pub finish_checks_run: u32,
    /// Checks the harness ran after an edit round this loop (run_edit_check).
    pub edit_checks_run: u32,
    /// Workspace tools that failed earlier in the model response being
    /// dispatched (cleared per response); a completed finish_task after one
    /// is bounced, since the edit it counted on is not in place.
    pub failed_calls_this_response: Vec<String>,
    /// Consecutive finish_task bounces of one anomaly family this loop
    /// (family key, count); see auto_unreconcile_repeated_bounce.
    pub finish_bounce: Option<(&'static str, u32)>,
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
    /// Minimal in-memory run for unit tests of accounting-only paths. Built
    /// from a real HarnessRun whose heavy collaborators were reset: avoids
    /// enumerating 39 fields that only `new` initialises.
    /// Resolve options into run-scoped config, load or create the state,
    /// build the role map / harness tool specs / transport tools, and create
    /// the model caller.
    /// A deferred review becomes due when no author work remains (the last
    /// task finished, was dropped by the model, or was dropped as exhausted
    /// by stall recovery): spawn the one review task covering everything
    /// that awaits review before the completion check can see a finished
    /// ledger.
    fn spawn_due_deferred_review(&mut self) {
        if self.state.review_opt_out.unwrap_or(false) {
            return;
        }
        let Some(gate) = self.role_gate.clone() else { return };
        if crate::harness::harness_tools::author_work_remains(&self.state)
            || !self.state.tasks.iter().any(|task| task.awaiting_review_by.is_some())
        {
            return;
        }
        if let Some((review_task_id, covered)) = crate::harness::harness_tools::spawn_deferred_review(&mut self.state, &gate) {
            self.emit(HarnessEvent {
                data: None,
                detail: format!("deferred review {review_task_id} spawned covering {covered}"),
                iteration: self.state.iteration,
                r#type: HarnessEventType::HarnessOp,
            });
            self.persist();
        }
    }

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
        let max_loops = options.max_loops.unwrap_or(i64::MAX);
        let run_start_head = git_head(&cwd);
        let warmup = build_warmup_command(&cwd, &options.goal)
            .and_then(|(program, args)| crate::tools::child_process::WarmupJob::spawn(&program, &args, &cwd).ok());
        if let Some(job) = &warmup {
            (emit_fn)(HarnessEvent {
                data: None,
                detail: format!("warm-up: `{}` started in the background so the first test run finds the build done", job.command),
                iteration: 0,
                r#type: HarnessEventType::HarnessOp,
            });
        }

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
        let plan_mode = PlanMode::parse(options.plan_mode.as_deref());
        let task_loop_limit = options
            .task_loop_limit
            .unwrap_or(crate::core::types::DEFAULT_TASK_LOOP_LIMIT)
            .max(1);
        let review_waiver_lines = options.review_waiver_lines;
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
        let (base_reasoning_effort, base_reasoning_effort_defaulted) =
            base_model_reasoning_effort(options.reasoning_effort.as_deref());
        let call_model = crate::harness::model_call::create_model_caller(
            crate::harness::model_call::ModelCallerDeps {
                codex_executable: None,
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
                reasoning_effort: base_reasoning_effort,
                reasoning_effort_defaulted: base_reasoning_effort_defaulted,
                request_timeout_ms: options.request_timeout_ms,
                hedge_floor_ms: None,
                latency_store: Some(options.latency_store.clone().unwrap_or_else(|| {
                    PathBuf::from(crate::core::home::resolve_drip_home_root())
                        .join(crate::harness::model_call::LATENCY_STORE_FILE)
                })),
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
            active_role: None,
            role_inference: Default::default(),
            options,
            now,
            emit_fn,
            run_started_at_ms,
            run_start_head,
            abort_requested_at_ms: None,
            run_usage,
            redact,
            url,
            model,
            cwd,
            max_iterations,
            max_loops,
            loop_config,
            stall_limit,
            task_loop_limit,
            review_waiver_lines,
            hung_commands: Vec::new(),
            check_durations_ms: HashMap::new(),
            hung_shapes: Vec::new(),
            hung_shape_leash_ms: HUNG_SHAPE_LEASH_MS,
            warmup,
            repeated_command: None,
            plan_mode,
            max_task_reopens,
            system_prompt,
            telemetry_config,
            dynamic_tool_names,
            dynamic_skills: Vec::new(),
            dynamic_skill_cache: HashMap::new(),
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
            blocked_on_input: false,
            replan_escalated: false,
            direct_plan_checked: false,
            ask_user_awaiting: false,
            ask_window_open: true,
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
        // Per-role inference accounting: bucket this call under its loop role
        // (falls back to "default" when usage arrives outside a role loop).
        let role_key = self.active_role.clone().unwrap_or_else(|| "default".to_string());
        let role_bucket = self
            .role_inference
            .entry(role_key)
            .or_insert_with(RoleInferenceTotals::default);
        role_bucket.calls += 1;
        if call.hedged {
            role_bucket.hedges_fired += 1;
        }
        if call.hedge_won {
            role_bucket.hedges_won += 1;
        }
        role_bucket.latency_ms += call.latency_ms.max(0) as u64;
        role_bucket.completion_tokens +=
            usage
                .and_then(|usage| usage.completion_tokens)
                .unwrap_or(0)
                .max(0) as u64;

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
        role_bucket.prompt_tokens += prompt_tokens.max(0) as u64;
        role_bucket.cache_read_tokens += cache_read_tokens.max(0) as u64;

        let reasoning_tokens = usage
            .and_then(|usage| usage.completion_tokens_details.as_ref())
            .and_then(|details| details.reasoning_tokens);
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
                "{} — {} prompt ({} cached, {} written), {} completion{} in {}ms{}",
                call.model,
                prompt_tokens,
                cache_read_tokens,
                cache_creation_tokens,
                completion_tokens,
                match reasoning_tokens {
                    Some(reasoning) if reasoning > 0 => format!(" ({reasoning} reasoning)"),
                    _ => String::new(),
                },
                call.latency_ms,
                match call.first_token_ms {
                    Some(first) => format!(" (first token {first}ms)"),
                    None => String::new(),
                }
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
            role_inference: self.role_inference.clone(),
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
            role_inference: self.role_inference.clone(),
        }
    }

    /// run one workspace tool through the tool framework
    /// (`execute_tool_call`), redacting the model-facing text.
    /// Record a tool execution as verification evidence when the call was a
    /// check (VERIFY/CHECK, or a BASH command that ran a known runner or
    /// emitted DRIP_VERIFY counts): the record, the last-verification pointer,
    /// the mutation watermark, and the stuck-failure streak.
    pub fn record_verification_outcome(
        &mut self,
        scope: &mut LoopScope,
        tool_name: &str,
        raw_input: &str,
        execution: &mut WorkspaceToolExecution,
    ) -> Option<String> {
        let tool_name = tool_name.to_string();
        let raw_input = raw_input.to_string();
            // A reused VERIFY ran nothing: the record it cites stays current.
            if tool_name == "VERIFY" && !execution.dispatched && execution.tool_content.starts_with(VERIFY_REUSED_PREFIX) {
                return None;
            }
            let verification_command = extract_verification_command_for_goal(&tool_name, &raw_input, &self.state.goal)
                .or_else(|| {
                    (tool_name == "BASH" && execution.tool_content.lines().any(|line| line.starts_with(crate::tools::builtin::verify::CUSTOM_RESULT_PREFIX)))
                        .then(|| extract_bash_command(&raw_input)).flatten()
                        // An echoed marker is not a check that ran (see parse_verify_output).
                        .filter(|command| !crate::tools::builtin::verify::marker_is_fabricated(command))
                })
                .or_else(|| bash_native_runner_verification(&tool_name, &raw_input, &execution.tool_content));
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
                    if tool_name == "VERIFY" || tool_name == "BASH" {
                        // BASH input carries no anchor field: the declared
                        // anchor is None and only the native-runner
                        // promotion below can make it external.
                        evidence.anchor = declared_verification_anchor_for_goal(&raw_input, &self.state.edited_paths, &self.state.goal);
                        let before = evidence.anchor.as_ref().map(|anchor| anchor.kind.clone());
                        evidence.anchor = promote_native_runner_anchor(evidence.anchor.take(), &verification_command, &evidence, &self.state.edited_paths);
                        if evidence.anchor.as_ref().map(|anchor| anchor.kind.clone()) != before
                            && evidence.anchor.as_ref().map_or(false, |anchor| anchor.kind == crate::core::types::VerificationAnchorKind::External)
                        {
                            self.emit(HarnessEvent {
                                data: None,
                                detail: format!("verification anchor promoted to external: project suite run by a native runner ({})", truncate_text(&verification_command, 80)),
                                iteration: self.state.iteration,
                                r#type: HarnessEventType::HarnessOp,
                            });
                        }
                        // The tool built its result text before the harness
                        // attached the declared anchor, so it reads
                        // "anchor: undeclared" for every call; a model that
                        // sees that re-declares the same anchor round after
                        // round. Print the anchor the record actually carries.
                        if evidence.anchor.is_some() {
                            execution.tool_content = execution.tool_content.replacen(
                                "; anchor: undeclared",
                                &format!("; anchor: {}", crate::core::state::describe_verification_anchor(evidence.anchor.as_ref())),
                                1,
                            );
                        }
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
                    id: Some(next_verification_record_id(&self.state.verifications)),
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

                // The record ref is how the model cites this evidence later
                // (finish_task evidence on a value revision), so it must be
                // visible in the tool result itself, not only in state.
                let record_ref = verification_record.id.clone();
                if tool_name == "VERIFY" {
                    if let Some(record_id) = record_ref.as_deref() {
                        execution.tool_content = format!(
                            "{}\nverification record: {record_id}",
                            execution.tool_content
                        );
                    }
                } else if tool_name == "BASH" {
                    if let (Some(record_id), Some(evidence)) = (record_ref.as_deref(), verification_record.evidence.as_ref()) {
                        execution.tool_content = format!(
                            "{}\n[harness] recorded as verification record {record_id}: {} executed, {} failed; anchor: {} — no separate VERIFY of the same suite is needed before finish_task",
                            execution.tool_content,
                            evidence.executed,
                            evidence.failed,
                            crate::core::state::describe_verification_anchor(evidence.anchor.as_ref())
                        );
                    }
                }
                self.state.last_verification = Some(verification_record.clone());
                scope.progress_this_cycle = true;
                scope.verification_progress_this_cycle = true;
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

        verification_command
    }

    /// finish_task bounced only because edits landed after the last check:
    /// the harness re-runs that same check itself (the agent already ran it
    /// once, so it is policy-vetted) and, when it passes, re-applies the
    /// finish. One model round and a rejection message saved per stale
    /// finish; a failing re-run surfaces the failure tail instead.
    /// Why this finish may skip its reviewer loop, or None. Two ways to earn
    /// it: the harness itself just ran the goal-declared check on the
    /// finished workspace (`harness_ran`), or the agent's own last VERIFY was
    /// one of the goal's declared checks, passed, and nothing was edited
    /// since. Either way the whole change must fit REVIEW_WAIVER_MAX_LINES;
    /// harness_tools adds the task-shape conditions (single-task run).
    pub fn review_waiver_reason(&self, harness_ran: Option<&str>) -> Option<String> {
        let base = self.run_start_head.as_deref()?;
        let (how, command) = self.verified_after_last_edit(harness_ran)?;
        let bound = self.review_waiver_lines.unwrap_or(REVIEW_WAIVER_MAX_LINES);
        if bound == 0 {
            return None;
        }
        let (code, tests) = workspace_changed_lines_split(&self.cwd, base)?;
        if code > bound || code + tests > REVIEW_WAIVER_MAX_TOTAL_LINES.max(bound) {
            return None;
        }
        Some(format!(
            "{how} ({}) after the last edit and it passed, and the whole change is {code} line(s) outside tests plus {tests} in tests (waiver bound {bound} outside tests)",
            truncate_text(&command, 80)
        ))
    }

    /// The goal-declared check that settled the current change: run by the
    /// harness (`harness_ran`) or as the agent's last VERIFY, passed, not
    /// self-authored, with no workspace edit since. The waiver adds a size
    /// bound on top; the review brief uses it as-is so the reviewer does not
    /// spend a round re-running a check that already settled the change.
    pub fn verified_after_last_edit(&self, harness_ran: Option<&str>) -> Option<(&'static str, String)> {
        let (how, command) = match harness_ran {
            Some(command) => ("the harness ran the goal-declared check", command.to_string()),
            None => {
                let record = self.state.last_verification.as_ref()?;
                if record.failed || record.ran_no_tests == Some(true) || self.state.mutations_since_verification.unwrap_or(0) > 0 {
                    return None;
                }
                if record
                    .evidence
                    .as_ref()
                    .and_then(|evidence| evidence.anchor.as_ref())
                    .is_some_and(|anchor| anchor.kind == crate::core::types::VerificationAnchorKind::SelfAuthored)
                {
                    return None;
                }
                let declared = goal_declared_check_commands(&self.state.goal);
                let command = record.command.trim().to_string();
                if declared.iter().any(|check| command.contains(check.trim())) {
                    ("the last VERIFY was the goal-declared check", command)
                } else if record
                    .evidence
                    .as_ref()
                    .and_then(|evidence| evidence.anchor.as_ref())
                    .is_some_and(|anchor| anchor.kind == crate::core::types::VerificationAnchorKind::External)
                    && native_runner_name(&command).is_some()
                {
                    // No declared check to match: an external-anchored run of
                    // the project's own suite through a native runner settles
                    // the change just as well (most real goals declare no
                    // check; their reviews re-ran the same suite).
                    let named_in_finish = record
                        .evidence
                        .as_ref()
                        .and_then(|evidence| evidence.anchor.as_ref())
                        .and_then(|anchor| anchor.source.as_deref())
                        .is_some_and(|source| source.starts_with("check named in finish_task"));
                    (
                        if named_in_finish { "the harness ran the project suite named in finish_task" } else { "the last VERIFY was the project suite with an external anchor" },
                        command,
                    )
                } else {
                    return None;
                }
            }
        };
        Some((how, command))
    }

    /// A finish bounced twice in a row for the same anomaly-family reason
    /// (support gaps, an expectation mismatch, an unobserved expectation) is
    /// re-applied as `unreconciled` with the anomalies on record: the bounce
    /// text already tells the agent to do exactly that, and recorded runs
    /// instead re-sent `completed` until the iteration cap. The run still
    /// ends unreconciled — the state is visible, not hidden.
    pub fn auto_unreconcile_repeated_bounce(
        &mut self,
        scope: &mut LoopScope,
        tool_name: &str,
        raw_input: &str,
        op_context: &crate::harness::harness_tools::HarnessOpContext,
        outcome: crate::harness::harness_tools::HarnessOpOutcome,
    ) -> crate::harness::harness_tools::HarnessOpOutcome {
        if tool_name != "finish_task" {
            return outcome;
        }
        let Some(family) = finish_bounce_family(&outcome.text) else {
            if outcome.task_finished || !outcome.text.starts_with("harness: not accepted yet") {
                scope.finish_bounce = None;
            }
            return outcome;
        };
        let count = match scope.finish_bounce {
            Some((seen, count)) if seen == family => count + 1,
            _ => 1,
        };
        scope.finish_bounce = Some((family, count));
        if count < FINISH_BOUNCE_AUTO_UNRECONCILE_AT {
            return outcome;
        }
        let Some(input) = unreconciled_finish_input(raw_input, &self.state, count, &outcome.text) else {
            return outcome;
        };
        let op = match parse_harness_op_with_gate(tool_name, &input, self.role_gate.as_ref()) {
            Ok(op) => op,
            Err(_) => return outcome,
        };
        let reapplied = apply_harness_op(&mut self.state, op, op_context);
        if !reapplied.task_finished {
            return outcome;
        }
        self.emit(HarnessEvent {
            data: Some(HarnessEventData { r#loop: Some(self.state.r#loop), task_id: scope.current_task_id.clone(), ..Default::default() }),
            detail: format!(
                "finish auto-downgraded to unreconciled after {count} identical bounces ({family}) — the anomalies stay on record: {}",
                truncate_text(&outcome.text, 160)
            ),
            iteration: self.state.iteration,
            r#type: HarnessEventType::HarnessOp,
        });
        scope.finish_bounce = None;
        crate::harness::harness_tools::HarnessOpOutcome {
            text: format!(
                "{}\n[harness] this finish was re-applied as status=\"unreconciled\" after {count} identical bounces; the anomalies are recorded with the task.",
                reapplied.text
            ),
            ..reapplied
        }
    }

    pub fn auto_reverify_stale_finish(
        &mut self,
        scope: &mut LoopScope,
        tool_name: &str,
        raw_input: &str,
        call_id: &str,
        op_context: &crate::harness::harness_tools::HarnessOpContext,
        outcome: crate::harness::harness_tools::HarnessOpOutcome,
    ) -> crate::harness::harness_tools::HarnessOpOutcome {
        if tool_name != "finish_task" {
            return outcome;
        }
        let recheck = finish_recheck_reason(&outcome.text, self.state.mutations_since_verification.unwrap_or(0));
        let stale = recheck == Some(FinishRecheck::Stale);
        let unchecked = recheck == Some(FinishRecheck::Unchecked);
        // Stale finish: re-run the record the agent already made. Unchecked
        // finish (nothing ran, or nothing external passed): run the check the
        // goal itself declares, once per loop, as task-provided evidence.
        // Records keep a 200-char truncation of the command; a cut command
        // (or a CHECK alias) cannot be re-run faithfully, so a stale finish
        // whose last check is unusable falls back to the goal-declared one.
        let rerunnable = self
            .state
            .last_verification
            .clone()
            .filter(|record| record.command.chars().count() < 200 && !record.command.starts_with("CHECK "));
        let mut detected_source: Option<&'static str> = None;
        let mut explicit_source: Option<String> = None;
        let explicit = explicit_finish_check(raw_input);
        let budget_left = scope.finish_checks_run < FINISH_CHECKS_MAX_PER_LOOP;
        let (record, goal_declared) = if (unchecked || stale) && explicit.is_some() && budget_left {
            // The finish names its own check: run it now instead of bouncing
            // the finish and paying a VERIFY round for the same command.
            let command = explicit.unwrap_or_default();
            let declared = command_is_goal_declared(&self.state.goal, &command);
            if !declared {
                explicit_source = Some(format!("check named in finish_task ({}), run by the harness", truncate_text(&command, 120)));
            }
            scope.finish_checks_run += 1;
            (
                HarnessVerificationRecord {
                    at_iteration: self.state.iteration,
                    command,
                    failed: false,
                    output_tail: String::new(),
                    ran_no_tests: None,
                    evidence: None,
                    id: None,
                },
                declared,
            )
        } else if stale && rerunnable.is_some() && budget_left && rerun_keeps_goal_standing(&self.state.goal, rerunnable.as_ref().unwrap(), &self.hung_commands) {
            // A re-run of a goal-declared command keeps its goal-declared
            // standing (anchor, review waiver): the bench showed a finish
            // whose harness-run check failed, then a fix, then a finish that
            // bounced for the failed record and cost a manual VERIFY round.
            let record = rerunnable.unwrap();
            let declared = command_is_goal_declared(&self.state.goal, &record.command);
            scope.finish_checks_run += 1;
            (record, declared)
        } else if (unchecked || stale) && budget_left {
            let declared = goal_declared_check_chain(&self.state.goal);
            if let (Some(chain), Some(record)) = (&declared, rerunnable.as_ref()) {
                // The last check was narrower than the goal's own: a recorded
                // run re-ran one test the model had picked, accepted the
                // finish, and then spent a review loop running the two
                // declared suites — the check that waives that review.
                self.emit(HarnessEvent {
                    data: Some(HarnessEventData { r#loop: Some(self.state.r#loop), task_id: scope.current_task_id.clone(), ..Default::default() }),
                    detail: format!(
                        "stale finish: the last check ({}) is not the goal-declared check, so the harness runs the declared one ({}) instead — it carries the goal's acceptance and waives the review",
                        truncate_text(&record.command, 100),
                        truncate_text(chain, 160)
                    ),
                    iteration: self.state.iteration,
                    r#type: HarnessEventType::HarnessOp,
                });
            }
            let detected = if declared.is_none() { detect_project_check_command(&self.cwd) } else { None };
            let Some(command) = declared.or_else(|| detected.as_ref().map(|(command, _)| command.clone())) else { return outcome };
            if let Some((_, source)) = &detected {
                detected_source = Some(source);
                self.emit(HarnessEvent {
                    data: Some(HarnessEventData { r#loop: Some(self.state.r#loop), task_id: scope.current_task_id.clone(), ..Default::default() }),
                    detail: format!("project check detected: {command} ({source}) — the goal declares no check, so the harness runs the project suite for this unchecked finish"),
                    iteration: self.state.iteration,
                    r#type: HarnessEventType::HarnessOp,
                });
            }
            scope.finish_checks_run += 1;
            (
                HarnessVerificationRecord {
                    at_iteration: self.state.iteration,
                    command,
                    failed: false,
                    output_tail: String::new(),
                    ran_no_tests: None,
                    evidence: None,
                    id: None,
                },
                true,
            )
        } else {
            return outcome;
        };
        let mut input = serde_json::json!({ "command": record.command });
        if goal_declared {
            input["anchor"] = serde_json::json!({
                "kind": "external",
                "source": detected_source
                    .map(|source| format!("project test suite detected by the harness ({source}), run by the harness"))
                    .unwrap_or_else(|| "goal-declared acceptance check, run by the harness".to_string()),
                "coverage": "reportedClaim"
            });
            input[GOAL_DECLARED_CHECK_MARKER] = serde_json::json!(true);
        } else if let Some(source) = &explicit_source {
            input["anchor"] = serde_json::json!({ "kind": "external", "source": source, "coverage": "reportedClaim" });
        }
        if let Some(anchor) = record.evidence.as_ref().and_then(|evidence| evidence.anchor.as_ref()) {
            let kind = match anchor.kind {
                crate::core::types::VerificationAnchorKind::External => "external",
                crate::core::types::VerificationAnchorKind::SelfAuthored => "self",
                crate::core::types::VerificationAnchorKind::Undeclared => "",
            };
            if !kind.is_empty() {
                input["anchor"] = serde_json::json!({ "kind": kind, "source": anchor.source.clone().unwrap_or_default() });
                if let Some(coverage) = &anchor.coverage {
                    input["anchor"]["coverage"] = serde_json::json!(match coverage {
                        crate::core::types::CoverageGranularity::ReportedClaim => "reportedClaim",
                        crate::core::types::CoverageGranularity::InputOrComponent => "inputOrComponent",
                    });
                }
                if let Some(subject) = &anchor.expectation_subject {
                    input["anchor"]["expectationSubject"] = serde_json::json!(subject);
                }
            }
        }
        let verify_input = input.to_string();
        let reverify_id = format!("{call_id}-reverify");
        let mut execution = self.execute_workspace_tool(&reverify_id, &verify_input, Some(&scope.loop_tool_indexes), "VERIFY");
        self.record_verification_outcome(scope, "VERIFY", &verify_input, &mut execution);
        let verdict = match &self.state.last_verification {
            Some(latest) => core_state::describe_verification_outcome(latest.failed, latest.ran_no_tests, latest.evidence.as_ref()),
            None => core_state::describe_verification_outcome(execution.failed, None, None),
        };
        self.emit(HarnessEvent {
            data: Some(HarnessEventData {
                r#loop: Some(self.state.r#loop),
                task_id: scope.current_task_id.clone(),
                ..Default::default()
            }),
            detail: if goal_declared {
                format!("harness ran the goal-declared check for the finish: {} -> {verdict}", truncate_text(&record.command, 120))
            } else if explicit_source.is_some() {
                format!("harness ran the check named in finish_task: {} -> {verdict}", truncate_text(&record.command, 120))
            } else {
                format!("harness re-ran the last check after workspace edits: {} -> {verdict}", truncate_text(&record.command, 120))
            },
            iteration: self.state.iteration,
            r#type: HarnessEventType::HarnessOp,
        });
        scope.digest_actions.push(format!("harness ran {} -> {verdict}", truncate_text(&record.command, 80)));
        if execution.failed {
            return crate::harness::harness_tools::HarnessOpOutcome {
                text: format!(
                    "harness: not accepted yet — the harness ran the {} ({}) and it FAILED. Fix the failure, then finish_task.\n{}",
                    if detected_source.is_some() { "project check the harness detected (the goal declares none)" } else if goal_declared { "goal-declared check" } else if explicit_source.is_some() { "check named in finish_task" } else { "last check again after your edits" },
                    truncate_text(&record.command, 120),
                    truncate_text_keeping_ends(&execution.tool_content, 1200)
                ),
                state_changed: true,
                task_finished: false,
                ended_loop: false,
                direct_response: None,
            };
        }
        let op = match parse_harness_op_with_gate(tool_name, raw_input, self.role_gate.as_ref()) {
            Ok(op) => op,
            Err(_) => return outcome,
        };
        // The harness just established the goal's own check passes on the
        // finished workspace; a small single-task change needs no reviewer
        // loop on top of that (harness_tools decides the task-shape part).
        let mut op_context = op_context.clone();
        if goal_declared {
            op_context.review_waived = self.review_waiver_reason(Some(&record.command));
        } else if explicit_source.is_some() {
            // A named check the harness just ran is the run's last
            // verification; the project-suite waiver judges it as such
            // (native runner, external anchor, small change). Dogfood #67
            // paid a four-call reviewer loop for a passing `cargo test`.
            op_context.review_waived = self.review_waiver_reason(None);
        }
        let reapplied = apply_harness_op(&mut self.state, op, &op_context);
        if reapplied.text.contains("Review waived:") {
            self.emit(HarnessEvent {
                data: Some(HarnessEventData { r#loop: Some(self.state.r#loop), task_id: scope.current_task_id.clone(), ..Default::default() }),
                detail: format!("review waived — {}", op_context.review_waived.clone().unwrap_or_default()),
                iteration: self.state.iteration,
                r#type: HarnessEventType::HarnessOp,
            });
            scope.digest_actions.push("review waived: harness-verified small change".to_string());
        }
        crate::harness::harness_tools::HarnessOpOutcome {
            text: format!(
                "harness {} {} -> {verdict}. {}",
                if goal_declared { "ran the goal-declared check:" } else if explicit_source.is_some() { "ran the check named in finish_task:" } else { "re-ran the last check after your edits:" },
                truncate_text(&record.command, 120),
                reapplied.text
            ),
            state_changed: true,
            task_finished: reapplied.task_finished,
            ended_loop: reapplied.ended_loop,
            direct_response: reapplied.direct_response,
        }
    }

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
        let command = matches!(tool_name, "BASH" | "VERIFY").then(|| extract_bash_command(raw_input)).flatten();
        if let Some(command) = command.as_deref().filter(|command| self.hung_commands.iter().any(|hung| hung == command)) {
            let text = hung_command_refusal(tool_name, command);
            self.emit(HarnessEvent {
                data: Some(HarnessEventData { r#loop: Some(self.state.r#loop), ..Default::default() }),
                detail: format!("refused re-run of a hung {tool_name} command: {}", truncate_text(command, 120)),
                iteration: self.state.iteration,
                r#type: HarnessEventType::HarnessOp,
            });
            return WorkspaceToolExecution { dispatched: false, failed: true, tool_content: text };
        }
        // Same shape as an earlier hang: run, but on a short leash.
        let leashed: Option<(String, String)> = command.as_deref().and_then(|command| {
            let shape = normalize_command_shape(command);
            let hung_before = self.hung_shapes.iter().find(|(hung, _)| *hung == shape).map(|(_, command)| command.clone())?;
            leash_timeout(raw_input, tool_name, self.hung_shape_leash_ms).map(|rewritten| (rewritten, hung_before))
        });
        let raw_input: &str = leashed.as_ref().map(|(rewritten, _)| rewritten.as_str()).unwrap_or(raw_input);
        if let Some((_, hung_before)) = &leashed {
            self.emit(HarnessEvent {
                data: Some(HarnessEventData { r#loop: Some(self.state.r#loop), ..Default::default() }),
                detail: format!("hung-shape leash: {} runs under {}s (same shape as {})", truncate_text(command.as_deref().unwrap_or(""), 100), self.hung_shape_leash_ms / 1000, truncate_text(hung_before, 80)),
                iteration: self.state.iteration,
                r#type: HarnessEventType::HarnessOp,
            });
        }
        if tool_name == "VERIFY" {
            if let Some(text) = command.as_deref().and_then(|command| {
                repeated_verify_reuse(command, self.state.last_verification.as_ref(), self.state.mutations_since_verification.unwrap_or(0))
            }) {
                self.emit(HarnessEvent {
                    data: Some(HarnessEventData { r#loop: Some(self.state.r#loop), ..Default::default() }),
                    detail: format!("repeated VERIFY reused the current record instead of re-running: {}", truncate_text(command.as_deref().unwrap_or(""), 120)),
                    iteration: self.state.iteration,
                    r#type: HarnessEventType::HarnessOp,
                });
                // The harness running the goal-declared check and reusing
                // the record makes that record the goal's check: its anchor
                // is external from here on, so the finish it serves is not
                // bounced for "self-authored or undeclared".
                let goal_declared = serde_json::from_str::<serde_json::Value>(raw_input)
                    .ok()
                    .and_then(|input| input.get(GOAL_DECLARED_CHECK_MARKER).and_then(|value| value.as_bool()))
                    .unwrap_or(false);
                if goal_declared {
                    if let Some(record) = self.state.last_verification.as_mut() {
                        if let Some(evidence) = record.evidence.as_mut() {
                            let external = evidence.anchor.as_ref().is_some_and(|anchor| anchor.kind == crate::core::types::VerificationAnchorKind::External);
                            if !external {
                                evidence.anchor = Some(crate::core::types::VerificationAnchor {
                                    kind: crate::core::types::VerificationAnchorKind::External,
                                    source: Some(format!(
                                        "goal-declared acceptance check: {} (record reused by the harness)",
                                        truncate_text(command.as_deref().unwrap_or(""), 120)
                                    )),
                                    downgraded_reason: None,
                                    coverage: None,
                                    expectation_subject: None,
                                });
                                self.emit(HarnessEvent {
                                    data: None,
                                    detail: "verification anchor promoted to external: the reused record is the goal-declared check".to_string(),
                                    iteration: self.state.iteration,
                                    r#type: HarnessEventType::HarnessOp,
                                });
                            }
                        }
                    }
                }
                return WorkspaceToolExecution { dispatched: false, failed: false, tool_content: text };
            }
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
        let mut tool_content = (self.redact)(&executed.tool_content);
        if let Some((_, hung_before)) = &leashed {
            tool_content.push_str(&hung_shape_leash_note(self.hung_shape_leash_ms, hung_before));
        }
        if let Some(command) = command.as_ref().filter(|_| failed && output_reports_hang(&tool_content)) {
            self.hung_commands.push(command.clone());
            let shape = normalize_command_shape(command);
            if !self.hung_shapes.iter().any(|(hung, _)| *hung == shape) {
                self.hung_shapes.push((shape, command.clone()));
            }
        }
        // Flailing detector: the same command shape run again and again with
        // nothing edited in between is not going to change its result.
        if tool_name == "PATCH" {
            // An edit may have fixed the hang: both memories reset.
            self.repeated_command = None;
            self.hung_commands.clear();
        } else if let Some(command) = command.as_deref() {
            let shape = normalize_command_shape(command);
            let count = match self.repeated_command.take() {
                Some((last, count)) if last == shape => count + 1,
                _ => 1,
            };
            self.repeated_command = Some((shape.clone(), count));
            if count >= REPEATED_COMMAND_NUDGE_AT {
                tool_content.push_str("\n\n");
                tool_content.push_str(&repeated_command_nudge(count));
                self.emit(HarnessEvent {
                    data: Some(HarnessEventData { r#loop: Some(self.state.r#loop), ..Default::default() }),
                    detail: format!("flailing nudge: {shape} ran {count} times with no edit between"),
                    iteration: self.state.iteration,
                    r#type: HarnessEventType::HarnessOp,
                });
            }
        }
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
        // A resume prompt is the answer an operator-blocked task was waiting
        // for: those tasks go back to pending before the first loop.
        let reopened = core_state::reopen_operator_blocked_tasks(&mut self.state);
        if !reopened.is_empty() {
            self.emit(HarnessEvent {
                data: None,
                detail: format!(
                    "reopened {} task(s) blocked on operator input — the resume prompt is their answer",
                    reopened.len()
                ),
                iteration: self.state.iteration,
                r#type: HarnessEventType::StallRecovery,
            });
            self.persist();
        }
        while self.state.iteration - self.start_iteration < self.max_iterations
            && self.state.r#loop - self.start_loop < self.max_loops
        {
            if self.signal_aborted() {
                self.aborted = true;
                break;
            }

            self.spawn_due_deferred_review();
            if core_state::is_goal_complete(&self.state) {
                break;
            }

            // Blocked on input the run cannot obtain: with nothing else
            // workable there is no loop worth spending — end here with the
            // work so far intact instead of replanning around the gap.
            if core_state::get_current_task(&self.state).is_none()
                && !core_state::operator_blocked_tasks(&self.state).is_empty()
            {
                self.blocked_on_input = true;
                break;
            }

            // Direct planning: a small goal with its own acceptance check
            // becomes one task without a planner loop (PlanMode).
            if !self.direct_plan_checked {
                self.direct_plan_checked = true;
                if self.state.tasks.is_empty() {
                    let project_check = if goal_declared_check_commands(&self.state.goal).is_empty() {
                        detect_project_check_command(&self.cwd)
                    } else {
                        None
                    };
                    if let Some(title) = direct_task_title(&self.state.goal, self.plan_mode, project_check.is_some()) {
                        let added = core_state::add_tasks(
                            &mut self.state,
                            vec![core_state::HarnessTaskInput { depends_on: None, review_of: None, role: None, title }],
                            core_state::HarnessTaskPlacement::End,
                        );
                        if let Some(task) = added.first().and_then(|task| core_state::get_task_by_id_mut(&mut self.state, &task.id)) {
                            core_state::append_task_note(
                                task,
                                "direct task: the planner was skipped (plan mode); the goal text is the contract — read what it names, make the change, run the check it declares.",
                            );
                        }
                        let detail = format!(
                            "direct task seeded, planner skipped (plan mode {:?}): {}{}",
                            self.plan_mode,
                            added.first().map(|task| task.id.as_str()).unwrap_or("?"),
                            project_check
                                .as_ref()
                                .map(|(command, source)| format!(" — the goal declares no check; the project suite {command} ({source}) verifies it"))
                                .unwrap_or_default()
                        );
                        self.emit(HarnessEvent {
                            data: Some(HarnessEventData { task_id: added.first().map(|task| task.id.clone()), ..Default::default() }),
                            detail,
                            iteration: self.state.iteration,
                            r#type: HarnessEventType::HarnessOp,
                        });
                        self.persist();
                    }
                }
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

            // Decide which discovered skills THIS loop composes, immediately
            // before it opens (and before begin_loop mutates task status).
            self.select_dynamic_skills().await;
            let mut scope = self.begin_loop();
            self.fire_hook(crate::harness::hooks::HookEvent::LoopStart, None);
            self.fire_hook(crate::harness::hooks::HookEvent::TaskStart, None);

            let mut cycle: i64 = 0;
            loop {
                cycle += 1;
                if cycle > scope.loop_budget.max_cycles + scope.cycle_extensions {
                    break;
                }
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

                // Progress extension: a cycle that edited or verified earns
                // the loop one more cycle (bounded), instead of a transcript
                // reset in the middle of productive work.
                if cycle == scope.loop_budget.max_cycles + scope.cycle_extensions
                    && scope.progress_this_cycle
                    && !scope.task_finished
                    && !scope.concluded_naturally
                    && scope.cycle_extensions < MAX_CYCLE_EXTENSIONS
                    && self.state.iteration - self.start_iteration < self.max_iterations
                {
                    scope.cycle_extensions += 1;
                    scope.affordable_cycles += 1;
                    let earned = describe_cycle_progress(
                        scope.edit_progress_this_cycle,
                        scope.verification_progress_this_cycle,
                    );
                    let detail = format!(
                        "cycle budget extended to {} (cycle {cycle} {earned}; {} extension(s) left)",
                        scope.loop_budget.max_cycles + scope.cycle_extensions,
                        MAX_CYCLE_EXTENSIONS - scope.cycle_extensions
                    );
                    scope.digest_actions.push(detail.clone());
                    self.emit(HarnessEvent {
                        data: Some(HarnessEventData { r#loop: Some(self.state.r#loop), task_id: scope.current_task_id.clone(), ..Default::default() }),
                        detail,
                        iteration: self.state.iteration,
                        r#type: HarnessEventType::HarnessOp,
                    });
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
    /// The loop's current task id and role, resolved before `begin_loop`
    /// mutates any task status — so the classifier pass and the loop it
    /// precedes always agree on the task and the role.
    fn loop_role_for_state(&self) -> (Option<String>, Option<HarnessRoleRuntime>) {
        let current_task_id = core_state::get_current_task(&self.state).map(|task| task.id.clone());
        let current_task = current_task_id
            .as_deref()
            .and_then(|id| core_state::get_task_by_id(&self.state, id));
        let role = match current_task {
            Some(_) => crate::harness::roles::resolve_loop_role(
                &self.role_map,
                current_task,
                self.options.role_bindings.as_ref(),
            ),
            None => crate::harness::roles::resolve_planning_role(
                &self.role_map,
                self.options.role_bindings.as_ref(),
                !self.state.tasks.is_empty(),
                self.replan_escalated,
            ),
        };

        (current_task_id, role)
    }

    /// Decides which pooled skills THIS loop composes. Called immediately
    /// before the loop opens. Never fatal: no classifier or no pool is a
    /// no-op, and every classifier failure warns and leaves the explicit
    /// skills (already in the base prompt) untouched.
    pub async fn select_dynamic_skills(&mut self) {
        // A loop without a classifier result composes no dynamic skills.
        self.dynamic_skills.clear();

        let Some(route) = self.options.classifier.clone() else {
            return;
        };
        if self.options.skill_pool.is_empty() {
            return;
        }

        let (task_id, role) = self.loop_role_for_state();
        let loop_tool_names: Vec<String> = self.tools.iter().map(|tool| tool.name.clone()).collect();
        let scope = loop_tool_scope(&loop_tool_names, role.as_ref(), self.options.mcp_servers.clone());
        let mut available: std::collections::BTreeSet<String> = scope.allowed.iter().cloned().collect();
        if let Some(servers) = scope.mcp_servers.as_ref() {
            available.extend(servers.iter().cloned());
        }

        // Requirements gate: a skill this loop cannot satisfy is never offered
        // to the classifier, so it can never come back selected.
        let candidates: Vec<crate::harness::classifier::SkillCandidate> = self
            .options
            .skill_pool
            .iter()
            .filter(|skill| requirements_satisfied(&skill.requirements, &available))
            .map(|skill| crate::harness::classifier::SkillCandidate {
                name: skill.name.clone(),
                description: skill.description.clone(),
                classifiers: skill.classifiers.clone(),
            })
            .collect();

        if candidates.is_empty() {
            return;
        }

        // A task re-activated in a later loop keeps the selection it already
        // paid for — but the capability gate is re-applied, because this
        // loop's role can widen or narrow the tool surface.
        if let Some(cached) = self.dynamic_skill_cache.get(&task_id).cloned() {
            self.dynamic_skills = cached
                .into_iter()
                .filter(|skill| candidates.iter().any(|candidate| candidate.name == skill.name))
                .collect();
            return;
        }

        // The state the classifier sees: the goal, this task's id and title
        // and its last three notes, the phase, the role, and the
        // tool names. No repository contents, no transcript.
        let task = task_id.as_deref().and_then(|id| core_state::get_task_by_id(&self.state, id));
        let notes: Vec<String> = task
            .map(|task| task.notes.iter().rev().take(3).rev().cloned().collect())
            .unwrap_or_default();
        // A task carries no description of its own: its title is the brief
        // and its notes are the accumulated context.
        let task_json = task.map(|task| {
            serde_json::json!({
                "id": task.id.clone(),
                "title": task.title.clone(),
                "notes": notes.clone(),
            })
        });
        let state = serde_json::json!({
            "goal": self.state.goal.clone(),
            "task": task_json,
            "phase": if task.is_some() { "task" } else { "planning" },
            "role": role.as_ref().map(|role| role.name.clone()),
            "availableTools": scope.allowed,
        });

        let selection =
            crate::harness::classifier::select_skills(&route, state, &candidates).await;

        for warning in &selection.warnings {
            self.emit(HarnessEvent {
                data: None,
                detail: warning.clone(),
                iteration: self.state.iteration,
                r#type: HarnessEventType::RunWarning,
            });
        }

        // Only a name this loop actually offered can be composed: the gate is
        // structural, not a property of what the classifier happened to return.
        let mut selected: Vec<crate::cli::skills::LoadedCliSkill> = Vec::new();
        for (name, _score) in &selection.selected {
            if !candidates.iter().any(|candidate| &candidate.name == name) {
                continue;
            }
            if let Some(skill) = self.options.skill_pool.iter().find(|skill| &skill.name == name) {
                selected.push(crate::cli::skills::LoadedCliSkill {
                    name: skill.name.clone(),
                    content: skill.content.clone(),
                    role_hints: None,
                });
            }
        }

        // A selection the classifier could not fully answer (timeout, HTTP
        // error, dropped skill) is used for this loop but never cached: the
        // next loop on this task asks again, so an outage hides a skill for
        // one loop at most.
        if selection.warnings.is_empty() {
            self.dynamic_skill_cache.insert(task_id, selected.clone());
        }
        self.dynamic_skills = selected;
    }

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
                task.loops_run = Some(task.loops_run.unwrap_or(0) + 1);
            }
            // A review loop opens with the change set in hand: the diff since
            // run start plus the run's verification records, so the reviewer
            // judges the artifact instead of spending rounds re-reading files
            // and re-running checks the author already ran.
            let wants_brief = core_state::get_task_by_id(&self.state, task_id).is_some_and(|task| {
                task.review_of.is_some() && !task.notes.iter().any(|note| note.starts_with(REVIEW_BRIEF_PREFIX))
            });
            if wants_brief {
                let settled = self
                    .verified_after_last_edit(None)
                    .map(|(how, command)| format!("{how} ({})", truncate_text(&command, 80)));
                let brief = build_review_brief_with(&self.cwd, self.run_start_head.as_deref(), &self.state, settled.as_deref());
                if let Some(task) = core_state::get_task_by_id_mut(&mut self.state, task_id) {
                    crate::core::state::append_task_note(task, &brief);
                }
            }
        }

        // A task loop shuts the ask window: only planning loops — the ones
        // the operator's goal or message just opened — may ask questions.
        if current_task_id.is_some() {
            self.ask_window_open = false;
        }
        // this loop's capability profile — the same resolution
        // `select_dynamic_skills` used before the loop opened, so the two can
        // never disagree about the task, the role, or the tool surface.
        let current_task = current_task_id
            .as_deref()
            .and_then(|id| core_state::get_task_by_id(&self.state, id));
        let (_, role) = self.loop_role_for_state();
        // filterToolsForRole, in one place with the classifier's gate.
        // Non-MCP tools keep the plain tool_names behaviour; MCP tools
        // (MCP__<server>__<tool>) additionally need their server in the loop's
        // effective set: the role's mcpServers when it sets one, else the
        // run-level --mcp set, else none.
        let loop_tool_names: Vec<String> = self.tools.iter().map(|tool| tool.name.clone()).collect();
        let loop_scope = loop_tool_scope(&loop_tool_names, role.as_ref(), self.options.mcp_servers.clone());
        let loop_tool_indexes: Vec<usize> = self
            .tools
            .iter()
            .enumerate()
            .filter(|(_, tool)| loop_scope.allowed.iter().any(|name| name == &tool.name))
            .map(|(index, _)| index)
            .collect();
        // transport tools.
        // The transport list must agree with the index list: the cached
        // default covers the full pack only, so rebuild whenever the role
        // allowlist or the MCP gate narrowed it.
        let loop_transport_tools: Vec<OpenAICompatibleRequestTool> = if loop_tool_indexes.len() != self.tools.len() {
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
        // Role first, then this loop's classifier-selected skills, so the
        // skill guidance reads inside the role's context.
        let loop_system_prompt = crate::cli::skills::compose_skill_system_prompt(
            &crate::harness::roles::compose_role_system_prompt(&self.system_prompt, role.as_ref()),
            &self.dynamic_skills,
        );
        // Per-role inference accounting keys off this for every model call in
        // the loop; roleless loops fall back to "default" at accumulation.
        self.active_role = role.as_ref().map(|role| role.name.clone());
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
        // The skills this loop composed, kept both as prose (the detail a
        // human reads) and as structure (the loop-start event's `skills`,
        // which dripw and the browser UI read instead of parsing the prose).
        let loaded_skills: Vec<String> =
            self.dynamic_skills.iter().map(|skill| skill.name.clone()).collect();
        let skills_suffix = if loaded_skills.is_empty() {
            String::new()
        } else {
            format!(" [skills: {}]", loaded_skills.join(", "))
        };
        let detail = format!(
            "loop {}{}{} — {}",
            self.state.r#loop,
            skills_suffix,
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
                skills: if loaded_skills.is_empty() { None } else { Some(loaded_skills) },
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

        let review_loop = current_task_id
            .as_deref()
            .and_then(|id| core_state::get_task_by_id(&self.state, id))
            .is_some_and(|task| task.review_of.is_some());
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
            read_windows: HashMap::new(),
            read_ranges: HashMap::new(),
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
            force_tool_call_next_round: false,
            tool_calls_this_loop: 0,
            overflow_retried_this_loop: false,
            concluded_naturally: false,
            planned_and_yielded: false,
            plan_yield_requested: false,
            finish_checks_run: 0,
            edit_checks_run: 0,
            failed_calls_this_response: Vec::new(),
            finish_bounce: None,
            cycles_run: 0,
            digest_actions: Vec::new(),
            progress_this_cycle: false,
            edit_progress_this_cycle: false,
            verification_progress_this_cycle: false,
            cycle_extensions: 0,
            review_loop,
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
        scope.progress_this_cycle = false;
        scope.edit_progress_this_cycle = false;
        scope.verification_progress_this_cycle = false;
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
            // A fresh operator message reopens the planning ask window: the
            // model may clarify before it replans.
            if !fresh_operator_messages.is_empty() {
                self.ask_window_open = true;
            }
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
            // Definition maps for the files the task names: no inference, and
            // they land before the first round instead of after a page of READs.
            let file_outlines = match current_task {
                Some(task) if !scope.review_loop => {
                    let notes = task.notes.join("\n");
                    let texts = [task.title.as_str(), notes.as_str(), self.state.goal.as_str()];
                    let mut parts: Vec<String> = Vec::new();
                    // Short named files travel whole (the text a READ would
                    // return); the rest get outlines.
                    if let Some(tree) = crate::harness::outline::repo_tree_for_prompt(&self.cwd) {
                        self.emit(HarnessEvent {
                            data: Some(HarnessEventData { r#loop: Some(self.state.r#loop), ..Default::default() }),
                            detail: format!("repository file list carried into {}'s first prompt ({} chars)", task.id, tree.chars().count()),
                            iteration: self.state.iteration,
                            r#type: HarnessEventType::HarnessOp,
                        });
                        parts.push(tree);
                    }
                    let named_paths = crate::harness::outline::named_paths_for_texts(&texts);
                    let mut carry_paths = named_paths.clone();
                    carry_paths.extend(crate::harness::outline::definition_files_for_texts(&self.cwd, &texts, &named_paths));
                    let hit_files = crate::harness::outline::symbol_hit_files_for_texts(&self.cwd, &texts, &carry_paths);
                    carry_paths.extend(hit_files);
                    carry_paths.extend(crate::harness::outline::named_directory_files(&self.cwd, &texts, &carry_paths));
                    let (bodies, carried) = crate::harness::outline::named_file_bodies_for_paths(&self.cwd, &carry_paths);
                    if let Some(bodies) = bodies {
                        self.emit(HarnessEvent {
                            data: Some(HarnessEventData { r#loop: Some(self.state.r#loop), ..Default::default() }),
                            detail: format!(
                                "named files carried whole into {}'s first prompt: {} ({} chars)",
                                task.id,
                                carried.join(", "),
                                bodies.chars().count()
                            ),
                            iteration: self.state.iteration,
                            r#type: HarnessEventType::HarnessOp,
                        });
                        parts.push(bodies);
                    }
                    let outline_paths: Vec<String> = named_paths.iter().filter(|path| !carried.contains(path)).cloned().collect();
                    parts.extend(crate::harness::outline::outlines_for_paths(&self.cwd, &outline_paths));
                    parts.extend(crate::harness::outline::symbol_hits_for_texts(&self.cwd, &texts));
                    parts.extend(crate::harness::outline::definition_hits_for_texts(&self.cwd, &texts));
                    let mut span_paths: Vec<String> = Vec::new();
                    if let Some(spans) = crate::harness::outline::definition_spans_for_texts(&self.cwd, &texts, &carried) {
                        let names: Vec<&str> = spans.lines().filter_map(|line| line.strip_prefix("== ")).map(|line| line.split(' ').next().unwrap_or(line)).collect();
                        span_paths.extend(names.iter().filter_map(|name| name.rsplit_once(':').map(|(path, _)| path.to_string())));
                        self.emit(HarnessEvent {
                            data: Some(HarnessEventData { r#loop: Some(self.state.r#loop), ..Default::default() }),
                            detail: format!("definition bodies carried into {}'s first prompt: {} ({} chars)", task.id, names.join(", "), spans.chars().count()),
                            iteration: self.state.iteration,
                            r#type: HarnessEventType::HarnessOp,
                        });
                        parts.push(spans);
                    }
                    // The small tests that pair with the carried files by
                    // name: the goal usually asks to extend them, and runs
                    // spent a READ round on them to match their style.
                    let mut sibling_sources = carry_paths.clone();
                    sibling_sources.extend(span_paths);
                    let siblings = crate::harness::outline::sibling_test_files(&self.cwd, &sibling_sources, &carried);
                    if let (Some(bodies), tests) = crate::harness::outline::file_bodies_for_paths(&self.cwd, &siblings, crate::harness::outline::SIBLING_TESTS_HEADER) {
                        self.emit(HarnessEvent {
                            data: Some(HarnessEventData { r#loop: Some(self.state.r#loop), ..Default::default() }),
                            detail: format!("sibling tests carried into {}'s first prompt: {} ({} chars)", task.id, tests.join(", "), bodies.chars().count()),
                            iteration: self.state.iteration,
                            r#type: HarnessEventType::HarnessOp,
                        });
                        parts.push(bodies);
                    }
                    // A later author task of the run sees what earlier tasks
                    // changed, with outlines of those files: planned runs
                    // spent 2-3× the author time of direct runs on the same
                    // task count because each task loop re-explored the work.
                    let earlier_author_work = self.state.tasks.iter().any(|other| {
                        other.id != task.id && other.review_of.is_none() && other.status == HarnessTaskStatus::Completed
                    });
                    if earlier_author_work {
                        if let Some((section, files)) = self.run_start_head.as_deref().and_then(|base| changes_so_far(&self.cwd, base)) {
                            self.emit(HarnessEvent {
                                data: Some(HarnessEventData { r#loop: Some(self.state.r#loop), ..Default::default() }),
                                detail: format!("changes so far: {} file(s) edited by earlier tasks carried into {}'s prompt with their outlines", files.len(), task.id),
                                iteration: self.state.iteration,
                                r#type: HarnessEventType::HarnessOp,
                            });
                            parts.push(section);
                            parts.extend(crate::harness::outline::outlines_for_paths(&self.cwd, &files));
                        }
                    }
                    if parts.is_empty() {
                        None
                    } else {
                        let text = parts.join("\n");
                        self.emit(HarnessEvent {
                            data: Some(HarnessEventData { r#loop: Some(self.state.r#loop), ..Default::default() }),
                            detail: format!(
                                "file outline: {} section(s), {} chars injected into {}'s first prompt (repository files, named file bodies, definition maps, symbol hits, sibling tests, changes so far)",
                                parts.len(),
                                text.chars().count(),
                                task.id
                            ),
                            iteration: self.state.iteration,
                            r#type: HarnessEventType::HarnessOp,
                        });
                        Some(text)
                    }
                }
                _ => None,
            };
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
                    file_outlines: file_outlines.as_deref(),
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
                    task_loop_limit: Some(self.task_loop_limit),
                    workspace: Some(&self.cwd),
                },
            );

            let blind_role = scope.role.as_ref().filter(|role| role.blind).map(|role| role.name.clone());
            let carried = match (&self.carryover, &scope.current_task_id) {
                (Some(carryover), Some(task_id)) if carryover.r#loop == self.state.r#loop - 1 && !carryover.messages.is_empty() => {
                    let mut carryover = carryover.clone();
                    // A review loop's brief already carries the diff and every
                    // new file in full; replaying PATCH exchanges (the whole
                    // file the author wrote) would double the reviewer's prompt.
                    let next_is_review = crate::core::state::get_task_by_id(&self.state, task_id).is_some_and(|task| task.review_of.is_some());
                    if next_is_review {
                        let before = carryover.messages.len();
                        carryover.messages = extract_loop_carryover_excluding(&carryover.messages, before, &["PATCH"]);
                        if carryover.messages.len() < before {
                            self.emit(HarnessEvent {
                                data: Some(HarnessEventData { r#loop: Some(self.state.r#loop), task_id: Some(task_id.clone()), ..Default::default() }),
                                detail: format!("review carryover: dropped {} PATCH exchange(s) from loop {} — the review brief already carries the diff", (before - carryover.messages.len()) / 2, carryover.r#loop),
                                iteration: self.state.iteration,
                                r#type: HarnessEventType::HarnessOp,
                            });
                        }
                    }
                    (!carryover.messages.is_empty()).then_some(carryover)
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
                MAX_PINNED_READ_FILES,
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
    /// Background jobs that settled since the model last looked at them are
    /// reported before the next model call, output tail included, so a round
    /// is never spent on ASYNC_WAIT / "sleep N; cat log" for a job that has
    /// already finished.
    fn report_settled_background_jobs(&mut self, scope: &mut LoopScope) {
        let settled = self.tool_services.async_jobs.take_settled_unreported();
        for job in settled {
            let tail = self
                .tool_services
                .async_jobs
                .tail_job(&job.id, Some(BACKGROUND_REPORT_TAIL_LINES))
                .map(|result| result.output)
                .unwrap_or_default();
            let status = match job.status {
                crate::tools::types::ChatAsyncToolJobStatus::Completed => "completed",
                crate::tools::types::ChatAsyncToolJobStatus::Failed => "failed",
                crate::tools::types::ChatAsyncToolJobStatus::Running => "running",
            };
            let exit = job.exit_code.flatten().map(|code| format!(" (exit {code})")).unwrap_or_default();
            let headline = format!("background job {} ({}) finished: {status}{exit}", job.id, truncate_text(&job.title, 80));
            let tail = tail.trim();
            let text = if tail.is_empty() {
                format!("{BACKGROUND_REPORT_PREFIX} {headline}. It produced no output. No ASYNC_WAIT or ASYNC_TAIL is needed for this job.")
            } else {
                format!(
                    "{BACKGROUND_REPORT_PREFIX} {headline}. Last {} line(s) of its output:\n{}\nNo ASYNC_WAIT or ASYNC_TAIL is needed for this job.",
                    tail.lines().count(),
                    truncate_text(tail, BACKGROUND_REPORT_MAX_CHARS)
                )
            };
            scope.transport_messages.push(TransportRequestMessage {
                content: Some(TransportContent::Text(text)),
                role: ChatRoleTag::User,
                ..Default::default()
            });
            scope.digest_actions.push(headline.clone());
            self.emit(HarnessEvent {
                data: Some(HarnessEventData { r#loop: Some(self.state.r#loop), task_id: scope.current_task_id.clone(), ..Default::default() }),
                detail: format!("{headline} — reported to the model"),
                iteration: self.state.iteration,
                r#type: HarnessEventType::HarnessOp,
            });
        }
    }

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
                fold_cold_tool_results(&mut scope.transport_messages, 0, &mut scope.folded_message_indexes, 0);

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

        self.report_settled_background_jobs(scope);

        let task_id = scope.current_task_id.clone();

        // The retry call reuses the same options; build them once.
        let forced_tool_call = std::mem::take(&mut scope.force_tool_call_next_round);
        let call_options = ModelCallOptions {
            include_tools: None,
            route: scope
                .role
                .as_ref()
                .and_then(|role| role.route.as_ref())
                .map(model_route_from_role_route),
            transport_tools: Some(if self.ask_window_open {
                scope.loop_transport_tools.clone()
            } else {
                // A closed ask window drops the ask_user spec from the model's
                // tool list: the op handler refuses it either way, but the
                // model should not be invited to call it.
                scope
                    .loop_transport_tools
                    .iter()
                    .filter(|tool| tool.function.name != "ask_user")
                    .cloned()
                    .collect()
            }),
            usage_task_id: task_id.clone(),
            max_tokens: forced_tool_call.then_some(TRUNCATION_RETRY_MAX_TOKENS),
            tool_choice: forced_tool_call.then(|| "required".to_string()),
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
                    0,
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
                scope.force_tool_call_next_round = true;
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
            let trimmed = response_text.trim().to_string();
            // A completion report on a verified workspace is the finish the
            // model forgot to call: the goal's check passed after the last
            // edit and nothing changed since, so the narration cannot claim
            // work that is not there (the concern behind the non-feature
            // below). Without this the reply concluded the loop and the
            // unfinished task was re-seeded with a fresh first prompt — a
            // recorded count-cmd run paid the whole orientation carry twice.
            if !scope.task_finished
                && !scope.review_loop
                && scope.current_task_id.is_some()
                && verified_after_last_edit(&self.state)
                && narration_reads_as_completion(&trimmed)
            {
                self.accept_narration_as_finish(scope, round, &response_text, false).await;
                return RoundOutcome::Continue;
            }
            // A narration-only first reply with an unfinished task gets ONE
            // corrective push and the round loop continues; a second
            // text-only reply concludes the loop as before.
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
        let (normalized_calls, expanded_finishes) = expand_patch_finishes(normalized_calls, &mut scope.used_tool_call_ids);
        if expanded_finishes > 0 {
            self.emit(HarnessEvent {
                data: Some(HarnessEventData { r#loop: Some(self.state.r#loop), task_id: scope.current_task_id.clone(), ..Default::default() }),
                detail: if normalized_calls.iter().any(|call| call.tool_name == "PATCH") {
                    "PATCH carried a finish: finish_task runs right after the edit in this same turn".to_string()
                } else {
                    "PATCH carried only a finish: finish_task runs in its place".to_string()
                },
                iteration: self.state.iteration,
                r#type: HarnessEventType::HarnessOp,
            });
        }

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

        let edits_only = !normalized_calls.is_empty() && normalized_calls.iter().all(|call| call.tool_name == "PATCH");
        self.dispatch_tool_calls(scope, normalized_calls).await;
        if self.ask_user_awaiting {
            return RoundOutcome::Break;
        }
        if scope.plan_yield_requested {
            scope.digest_actions.push("plan landed; the planning loop yields to the first task without another round".to_string());
            return RoundOutcome::Break;
        }
        // A completion report sent with the edits themselves: the goal's
        // check passed right after this round's last PATCH, every call
        // landed, every file the goal names was touched, and the text reads
        // as done. Recorded bench runs spent a whole round on a lone
        // finish_task after exactly this shape (10 of 28 in one pass).
        if edits_only
            && !scope.task_finished
            && !scope.review_loop
            && scope.current_task_id.is_some()
            && scope.failed_calls_this_response.is_empty()
            && verified_after_last_edit(&self.state)
        {
            let reads_as_done = narration_reads_as_completion(&response_text);
            let named_edited = goal_named_paths_all_edited(&self.state.goal, &self.state.edited_paths);
            if reads_as_done && named_edited {
                self.accept_narration_as_finish(scope, round, &response_text, true).await;
            } else {
                // The shape that costs a lone finish round; say why the
                // text with the edits was not taken as the finish, so
                // transcripts show what the model actually sent.
                let why = if response_text.trim().is_empty() {
                    "the response carried no text".to_string()
                } else if !reads_as_done {
                    format!("the text does not read as a completion report: {}", truncate_text(response_text.trim(), 160))
                } else {
                    format!(
                        "a path the goal names has not been edited (edited: {})",
                        if self.state.edited_paths.is_empty() { "none".to_string() } else { self.state.edited_paths.join(", ") }
                    )
                };
                self.emit(HarnessEvent {
                    data: Some(HarnessEventData { r#loop: Some(self.state.r#loop), task_id: scope.current_task_id.clone(), ..Default::default() }),
                    detail: format!("edits landed and the goal-declared check passed, but not taken as the finish: {why}"),
                    iteration: self.state.iteration,
                    r#type: HarnessEventType::HarnessOp,
                });
            }
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
    /// After the response's last successful PATCH, when the response carries
    /// no finish, run the goal's check once so its verdict rides on the PATCH
    /// result. The recorded bench paid a bounce round for every finish whose
    /// harness-run check failed (4 of 16 runs) and an author's own check
    /// round between edit and finish in 3 more; with the verdict already in
    /// hand the next round is the fix or the finish. Gated to checks that
    /// ran within EDIT_CHECK_MAX_KNOWN_MS (or, unmeasured, are not
    /// compile-first runners), never a command that hung this run, at most
    /// EDIT_CHECKS_MAX_PER_LOOP per loop.
    /// See check_duration_measurable: the warm-up's state right now.
    pub fn check_duration_is_measurable(&mut self, command: &str) -> bool {
        let (warmup_command, done) = match self.warmup.as_mut() {
            Some(job) => (Some(job.command.clone()), job.finished()),
            None => (None, true),
        };
        check_duration_measurable(warmup_command.as_deref(), done, command)
    }

    pub fn run_edit_check(&mut self, scope: &mut LoopScope, call_id: &str) -> Option<String> {
        if scope.edit_checks_run >= EDIT_CHECKS_MAX_PER_LOOP {
            return None;
        }
        let declared = goal_declared_check_chain(&self.state.goal);
        let detected = if declared.is_none() { detect_project_check_command(&self.cwd) } else { None };
        let command = declared.or_else(|| detected.as_ref().map(|(command, _)| command.clone()))?;
        if self.hung_commands.iter().any(|hung| *hung == command) {
            return None;
        }
        let shape = normalize_command_shape(&command);
        let known = self.check_durations_ms.get(&shape).copied();
        let build_warm = self
            .warmup
            .as_mut()
            .filter(|job| native_runner_name(&job.command).is_some() && native_runner_name(&job.command) == native_runner_name(&command))
            // Finished, not succeeded: the model edits the crate while the
            // warm-up compiles it, so the warm-up can fail on the crate's
            // own error having already paid for the dependencies.
            .map(|job| job.finished())
            .unwrap_or(false);
        if !edit_check_allowed(&command, known, build_warm) {
            return None;
        }
        if known.is_none() && build_warm {
            self.emit(HarnessEvent {
                data: Some(HarnessEventData { r#loop: Some(self.state.r#loop), task_id: scope.current_task_id.clone(), ..Default::default() }),
                detail: format!("edit check allowed for a compile-first runner: the warm-up build finished, so {} runs incrementally", truncate_text(&command, 80)),
                iteration: self.state.iteration,
                r#type: HarnessEventType::HarnessOp,
            });
        }
        scope.edit_checks_run += 1;
        let mut input = serde_json::json!({ "command": command });
        input["anchor"] = serde_json::json!({
            "kind": "external",
            "source": detected
                .as_ref()
                .map(|(_, source)| format!("project test suite detected by the harness ({source}), run by the harness after an edit"))
                .unwrap_or_else(|| "goal-declared acceptance check, run by the harness after an edit".to_string()),
            "coverage": "reportedClaim"
        });
        input[GOAL_DECLARED_CHECK_MARKER] = serde_json::json!(true);
        let verify_input = input.to_string();
        let started = (self.now)().timestamp_millis();
        let mut execution = self.execute_workspace_tool(&format!("{call_id}-editcheck"), &verify_input, Some(&scope.loop_tool_indexes), "VERIFY");
        let took = (self.now)().timestamp_millis() - started;
        if self.check_duration_is_measurable(&command) {
            self.check_durations_ms.insert(shape, took);
        }
        self.record_verification_outcome(scope, "VERIFY", &verify_input, &mut execution);
        let verdict = match &self.state.last_verification {
            Some(latest) => core_state::describe_verification_outcome(latest.failed, latest.ran_no_tests, latest.evidence.as_ref()),
            None => core_state::describe_verification_outcome(execution.failed, None, None),
        };
        self.emit(HarnessEvent {
            data: Some(HarnessEventData { r#loop: Some(self.state.r#loop), task_id: scope.current_task_id.clone(), ..Default::default() }),
            detail: format!("harness ran the goal-declared check after this round's edits: {} -> {verdict} ({took}ms)", truncate_text(&command, 120)),
            iteration: self.state.iteration,
            r#type: HarnessEventType::HarnessOp,
        });
        scope.digest_actions.push(format!("harness ran {} after the edit -> {verdict}", truncate_text(&command, 80)));
        Some(edit_check_note(&command, !execution.failed, &verdict, &truncate_text_keeping_ends(&execution.tool_content, 1200)))
    }

    /// Dispatches a synthetic finish_task whose summary is the model's own
    /// completion report. `after_tool_calls`: the response's assistant
    /// message (with its tool calls) is already in the transcript, so the
    /// finish joins that message's calls instead of opening a new one.
    pub async fn accept_narration_as_finish(&mut self, scope: &mut LoopScope, round: i64, response_text: &str, after_tool_calls: bool) {
        use crate::harness::transport::{normalize_openai_compatible_tool_call, OpenAICompatibleToolCall, OpenAICompatibleToolCallFunction};
        let trimmed = response_text.trim().to_string();
        let raw_input = serde_json::json!({ "status": "completed", "summary": truncate_text(&trimmed, 600) }).to_string();
        let mut call_id = format!("narration-finish-{}-{}", self.state.iteration, round);
        while scope.used_tool_call_ids.contains(&call_id) {
            call_id.push('x');
        }
        scope.used_tool_call_ids.insert(call_id.clone());
        let normalized = normalize_openai_compatible_tool_call(OpenAICompatibleToolCall {
            function: Some(OpenAICompatibleToolCallFunction { arguments: Some(raw_input.clone()), name: Some("finish_task".to_string()) }),
            id: Some(call_id.clone()),
            tool_type: Some("function".to_string()),
        });
        self.emit(HarnessEvent {
            data: Some(HarnessEventData { r#loop: Some(self.state.r#loop), task_id: scope.current_task_id.clone(), ..Default::default() }),
            detail: format!(
                "narration accepted as finish_task: the goal-declared check passed after the last edit and the reply reads as a completion report{} — {}",
                if after_tool_calls { " (sent with this round's edits)" } else { "" },
                truncate_text(&trimmed, 160)
            ),
            iteration: self.state.iteration,
            r#type: HarnessEventType::HarnessOp,
        });
        scope.digest_actions.push(format!("said (accepted as finish): {}", truncate_text(&trimmed, MAX_DIGEST_ACTION_CHARS)));
        let joined = after_tool_calls
            && scope
                .transport_messages
                .iter_mut()
                .rev()
                .find(|message| message.role == ChatRoleTag::Assistant)
                .map(|message| {
                    message.tool_calls.get_or_insert_with(Vec::new).push(normalized.clone());
                    // Native content blocks would not carry the added call.
                    message.anthropic_content = None;
                })
                .is_some();
        if !joined {
            scope.transport_messages.push(TransportRequestMessage {
                anthropic_content: None,
                content: Some(TransportContent::Text(response_text.to_string())),
                role: ChatRoleTag::Assistant,
                tool_calls: Some(vec![normalized.clone()]),
                ..Default::default()
            });
        }
        scope.tool_calls_this_loop += 1;
        self.dispatch_tool_calls(scope, vec![NormalizedCall { call_id, normalized, raw_input, tool_name: "finish_task".to_string() }]).await;
    }

    pub async fn dispatch_tool_calls(&mut self, scope: &mut LoopScope, calls: Vec<NormalizedCall>) {
        scope.failed_calls_this_response.clear();
        let response_has_finish = calls.iter().any(|call| call.tool_name == "finish_task");
        let last_patch_index = calls.iter().rposition(|call| call.tool_name == "PATCH");
        for (call_index, call) in calls.into_iter().enumerate() {
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

            // A finish sent in the same response as its final edit is the
            // fast path (no extra round for "done"); one that follows a
            // failed PATCH or command in that response would finish on an
            // edit that never landed, so it comes back instead.
            if tool_name == "finish_task" {
                if let Some(bounce) = finish_after_failed_call(&raw_input, &scope.failed_calls_this_response) {
                    scope.digest_actions.push("finish_task: bounced — a call failed earlier in the same response".to_string());
                    self.emit(HarnessEvent {
                        data: Some(HarnessEventData { r#loop: Some(self.state.r#loop), task_id: scope.current_task_id.clone(), ..Default::default() }),
                        detail: format!("finish_task: {bounce}"),
                        iteration: self.state.iteration,
                        r#type: HarnessEventType::HarnessOp,
                    });
                    scope.transport_messages.push(crate::harness::transport::TransportRequestMessage {
                        content: Some(crate::harness::transport::TransportContent::Text(bounce)),
                        name: Some(tool_name),
                        role: ChatRoleTag::Tool,
                        tool_call_id: Some(call_id),
                        tool_calls: None,
                        anthropic_content: None,
                    });
                    continue;
                }
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
                    ask_window_open: self.ask_window_open,
                    // Sticky opt-out decided at the cycle boundary (goal
                    // phrase, operator message, --no-review, --lite); also
                    // rejects delayed Review*/reviewer task work mid-run.
                    review_opt_out: self.state.review_opt_out == Some(true),
                    review_waived: if tool_name == "finish_task" { self.review_waiver_reason(None) } else { None },
                };
                let outcome = match op {
                    Ok(op) => {
                        let ask_user_pending = matches!(
                            op,
                            crate::harness::harness_tools::HarnessOp::AskUser { .. }
                        );
                        let outcome = apply_harness_op(&mut self.state, op, &op_context);
                        // The waiver's event was only emitted on the harness-run
                        // re-verify path; a finish the agent's own goal-declared
                        // VERIFY earned was waived silently (bench counters missed it).
                        if outcome.text.contains("Review waived:") {
                            self.emit(HarnessEvent {
                                data: Some(HarnessEventData { r#loop: Some(self.state.r#loop), task_id: scope.current_task_id.clone(), ..Default::default() }),
                                detail: format!("review waived — {}", op_context.review_waived.clone().unwrap_or_default()),
                                iteration: self.state.iteration,
                                r#type: HarnessEventType::HarnessOp,
                            });
                            scope.digest_actions.push("review waived: harness-verified small change".to_string());
                        }
                        let outcome = self.auto_reverify_stale_finish(scope, &tool_name, &raw_input, &call_id, &op_context, outcome);
                        let outcome = self.auto_unreconcile_repeated_bounce(scope, &tool_name, &raw_input, &op_context, outcome);
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
                if tool_name == "plan_tasks"
                    && outcome.state_changed
                    && scope.current_task_id.is_none()
                    && self.state.tasks.iter().any(|task| matches!(task.status, HarnessTaskStatus::Pending | HarnessTaskStatus::InProgress))
                {
                    scope.plan_yield_requested = true;
                }
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

            let (raw_input, dropped_tail_filter) = if tool_name == "BASH" { drop_runner_tail_filter(&raw_input) } else { (raw_input, None) };
            let (raw_input, whole_file_note) = if tool_name == "READ" {
                let path = serde_json::from_str::<serde_json::Value>(&raw_input)
                    .ok()
                    .and_then(|value| value.get("path").and_then(|path| path.as_str()).map(str::to_string));
                let prior = path.as_ref().and_then(|path| scope.read_windows.get(path).copied()).unwrap_or(0);
                let (raw_input, whole_note) = promote_read_to_whole_file(&raw_input, prior, std::path::Path::new(&self.cwd));
                // Whole-file promotion wins the note; otherwise flag an overlap.
                let overlap_note = if whole_note.is_none() {
                    read_range_of(&raw_input).and_then(|(range_path, range)| {
                        let note = scope.read_ranges.get(&range_path).and_then(|seen| overlapping_read_note(seen, range));
                        scope.read_ranges.entry(range_path).or_default().push(range);
                        note
                    })
                } else {
                    None
                };
                if let Some(path) = path {
                    *scope.read_windows.entry(path).or_insert(0) += 1;
                }
                (raw_input, whole_note.or(overlap_note))
            } else {
                (raw_input, None)
            };
            let (raw_input, anchor_note) = if tool_name == "PATCH" { anchor_append_to_goal(&raw_input, &self.state.goal) } else { (raw_input, None) };
            if let Some(note) = anchor_note.as_deref() {
                self.emit(HarnessEvent {
                    data: Some(HarnessEventData { r#loop: Some(self.state.r#loop), task_id: scope.current_task_id.clone(), ..Default::default() }),
                    detail: note.to_string(),
                    iteration: self.state.iteration,
                    r#type: HarnessEventType::HarnessOp,
                });
            }
            let execution_started_at_ms = (self.now)().timestamp_millis();
            let mut execution = self.execute_workspace_tool(&call_id, &raw_input, Some(&scope.loop_tool_indexes), &tool_name);
            let execution_duration_ms = (self.now)().timestamp_millis() - execution_started_at_ms;
            if tool_name == "VERIFY" || tool_name == "BASH" {
                if let Some(command) = extract_bash_command(&raw_input) {
                    if self.check_duration_is_measurable(&command) {
                        self.check_durations_ms.insert(normalize_command_shape(&command), execution_duration_ms);
                    }
                }
            }

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
            let edit_check_note =
                if tool_name == "PATCH" && !execution.failed && !response_has_finish && last_patch_index == Some(call_index) && !scope.review_loop {
                    self.run_edit_check(scope, &call_id)
                } else {
                    None
                };
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

            let verification_command = self.record_verification_outcome(scope, &tool_name, &raw_input, &mut execution);

            // A promoted whole-file READ is the one result allowed past the
            // per-result cap: it replaces the pages the model would request.
            let result_cap = if whole_file_note.is_some() {
                (scope.loop_budget.max_tool_result_chars as usize).max(READ_WHOLE_FILE_MAX_CHARS + 512)
            } else {
                scope.loop_budget.max_tool_result_chars as usize
            };
            let mut tool_content = truncate_text_keeping_ends(&execution.tool_content, result_cap);
            if let Some(note) = bash_command.as_deref().and_then(long_bash_command_note) {
                tool_content.push_str("\n");
                tool_content.push_str(&note);
            }
            if let Some(note) = whole_file_note.as_deref() {
                tool_content.push_str("\n");
                tool_content.push_str(note);
            }
            if let Some(note) = anchor_note.as_deref().filter(|_| !execution.failed) {
                tool_content.push_str("\n");
                tool_content.push_str(note);
            }
            if let Some(note) = edit_check_note.as_deref() {
                tool_content.push_str(note);
            }
            if let Some(filter) = dropped_tail_filter.as_deref() {
                tool_content.push_str(&format!(
                    "\n[harness] the trailing `{filter}` was dropped: runner output is bounded here and its failure block is kept, so the summary and any panic are both visible without a re-run."
                ));
            }
            // `cargo test` takes one filter; two positional filters fail before
            // any test runs (a recorded VERIFY lost a round to it).
            if execution.failed
                && execution.tool_content.contains("unexpected argument")
                && execution.tool_content.contains("Usage: cargo test")
            {
                tool_content.push_str("\n[harness] cargo test accepts one TESTNAME filter; run the modules as separate commands chained with && (cargo test --lib a && cargo test --lib b), or use a shared prefix.");
            }
            // A truncated runner failure keeps its panic block: the ends-kept
            // cut drops the middle, which is where the assertion lives.
            if execution.failed
                && bash_command.as_deref().or(verification_command.as_deref()).is_some_and(|command| native_runner_name(command).is_some())
                && execution.tool_content.chars().count() > scope.loop_budget.max_tool_result_chars as usize
            {
                if let Some(excerpt) = crate::tools::builtin::verify::runner_failure_excerpt(&execution.tool_content) {
                    let first = excerpt.lines().next().unwrap_or("");
                    if !first.is_empty() && !tool_content.contains(first) {
                        tool_content.push_str(&format!("\n\n[harness] failure excerpt from the elided middle:\n{excerpt}"));
                    }
                }
            }

            // Oversized output spills to a file beside the state store so the
            // elided middle stays recoverable with READ/GREP.
            if execution.tool_content.chars().count() > result_cap {
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
                    if execution.failed && output_unchanged {
                        // A recorded big-file run re-sent one failing PATCH
                        // four times in a row; the read-only wording above
                        // ("act on the result") is the wrong advice for a
                        // failure.
                        "It failed the same way: re-sending it cannot succeed. Change the call — READ the region the error names and copy its exact text — or take a different route."
                    } else if output_unchanged {
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

            if tool_name == "PATCH" && !execution.failed {
                // The file changed: earlier reads of it are stale, so a later
                // READ of the same region is not a redundant re-read.
                for path in patched_paths(&raw_input) {
                    scope.read_ranges.remove(&path);
                }
            }
            let read_only_bash = bash_command.as_deref().is_some_and(is_read_only_shell_command);

            if (deduped_tool || read_only_bash) && !execution.failed {
                scope.read_only_calls_this_loop += 1;
                // Eight reads in the first cycle of a fresh loop is normal
                // orientation, not drift: the nudge waits for the second
                // cycle (or sixteen reads), so it lands when it means something.
                let orientation = scope.cycle <= 1 && scope.read_only_calls_this_loop < READ_ONLY_NUDGE_EVERY * 2;
                if !scope.persisted_this_loop && !scope.review_loop && !orientation && scope.read_only_calls_this_loop % READ_ONLY_NUDGE_EVERY == 0 {
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

            // One digest line per workspace call — what it touched and how
            // it came out — so the next loop sees what was already read,
            // grepped, or edited instead of re-exploring.
            if execution.failed && may_mutate {
                scope.failed_calls_this_response.push(tool_name.clone());
            }
            scope.digest_actions.push(format!(
                "{tool_name} {}{} → {}",
                compact_tool_input(&raw_input, 90),
                if execution.failed { " (failed)" } else { "" },
                truncate_text(&collapse_whitespace(execution.tool_content.trim()), 90)
            ));

            if let Some(task_id) = scope.current_task_id.clone() {
                if may_mutate && !execution.failed && tool_name != "PATCH" && !shell_write {
                    if let Some(task) = core_state::get_task_by_id_mut(&mut self.state, &task_id) {
                        scope.progress_this_cycle = true;
                        scope.edit_progress_this_cycle = true;
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
                            scope.progress_this_cycle = true;
                            scope.edit_progress_this_cycle = true;
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
                let mut auto_block_budget: Option<i64> = None;

                if let Some(task) = crate::core::state::get_task_by_id_mut(&mut self.state, &task_id) {
                    // Loop budget: a task that keeps editing but never
                    // finishes is bounded here, independent of stalls.
                    let loops_run = task.loops_run.unwrap_or(0);
                    if loops_run >= self.task_loop_limit && task.status == HarnessTaskStatus::InProgress {
                        auto_block_budget = Some(loops_run);
                    }
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

                if let Some(loops_run) = auto_block_budget.filter(|_| auto_block_stalls.is_none()) {
                    let summary = format!(
                        "Auto-blocked: {loops_run} task loops (budget {}) without finish_task. Needs a smaller decomposition or operator guidance.",
                        self.task_loop_limit
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
                            "{task_id} auto-blocked after {loops_run} task loops (task loop budget {})",
                            self.task_loop_limit
                        ),
                        iteration: self.state.iteration,
                        r#type: crate::core::types::HarnessEventType::TaskFinished,
                    });
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
        } else if core_state::get_current_task(&self.state).is_some()
            || core_state::is_goal_complete(&self.state)
        {
            // A planning loop counts as progress only when it left the ledger
            // workable (plan_tasks added a task, or a blocked task was
            // resolved by id). Notes, observations, and re-blocking a task
            // used to count too, so a run could replan around a dead end
            // forever without the idle counter ever firing.
            self.idle_loops = 0;
            self.replan_escalated = false;
        } else {
            // The replanning role got its try and the ledger is no more
            // workable than before: the next replanning loop escalates to the
            // planning role.
            self.replan_escalated = true;
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

        // Anomalies that do not block a completed run (informational ones,
        // or whose expectation matched, or whose own text reports success)
        // become notes: the run completes instead of ending unreconciled.
        if core_state::is_goal_complete(&self.state) && !self.state.anomalies.is_empty() {
            let blocking = self
                .state
                .anomalies
                .iter()
                .any(|anomaly| crate::harness::harness_tools::anomaly_blocks_completion(&self.state, anomaly));
            if !blocking {
                let notes: Vec<String> = self
                    .state
                    .anomalies
                    .iter()
                    .map(|anomaly| format!("non-blocking anomaly (informational, or its observation matched): {} — expected {}; observed {}", anomaly.subject, anomaly.expected, anomaly.observed))
                    .collect();
                // The note lands on the work task, not the review that covered it.
                let target = self
                    .state
                    .tasks
                    .iter()
                    .rev()
                    .find(|task| task.status == HarnessTaskStatus::Completed && task.review_of.is_none())
                    .or_else(|| self.state.tasks.iter().rev().find(|task| task.status == HarnessTaskStatus::Completed))
                    .map(|task| task.id.clone());
                if let Some(task) = target.and_then(|id| core_state::get_task_by_id_mut(&mut self.state, &id)) {
                    for note in &notes {
                        core_state::append_task_note(task, note);
                    }
                }
                let count = self.state.anomalies.len();
                self.state.anomalies.clear();
                self.emit(HarnessEvent {
                    data: None,
                    detail: format!("{count} anomaly(ies) did not block completion (informational, or their observation matched); kept as task notes"),
                    iteration: self.state.iteration,
                    r#type: HarnessEventType::HarnessOp,
                });
            }
        }

        let reason: HarnessRunReason = if self.run_error.is_some() {
            HarnessRunReason::Error
        } else if self.ask_user_awaiting {
            HarnessRunReason::AwaitingInput
        } else if self.plan_stopped {
            HarnessRunReason::Planned
        } else if self.blocked_on_input {
            // Waiting on the operator outranks futile: the resume command that
            // carries their reply is the way forward, not a fresh goal.
            HarnessRunReason::BlockedOnInput
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
        } else if self.state.r#loop - self.start_loop >= self.max_loops {
            // An explicit --max-loops wins over --max-iterations when both run
            // out on the same loop: it is the cap the operator chose to set.
            HarnessRunReason::MaxLoops
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
            HarnessRunReason::MaxLoops => "max-loops",
            HarnessRunReason::BlockedOnInput => "blocked-on-input",
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
        // Blocked on operator input: the resume prompt IS the answer, so the
        // continue command names where it goes.
        if reason == HarnessRunReason::BlockedOnInput && self.continue_command.is_none() {
            if let Some(session_id) = self.resume_target() {
                self.continue_command = Some(format!(
                    "drip --resume {session_id} --prompt \"<the input the blocked task asks for>\""
                ));
            }
        }
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
            role_inference: self.role_inference.clone(),
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
        // A small completed run already has its summary: each finish_task
        // carried one, and a reviewer confirmed it. Composing those beats a
        // model call that restates them (typically 3-10s on a fast model,
        // 5-10% of a one-task run).
        if reason == HarnessRunReason::Completed && self.options.summarize_run.is_none() {
            if let Some(text) = crate::harness::prompt::build_composed_run_summary(&self.state, 6) {
                self.record_run_summary(reason, text);
                return;
            }
        }

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
    if let Some(chat) = answers.chat.as_deref().filter(|text| !text.trim().is_empty()) {
        // "Chat about this": the operator answered the WHOLE survey in one
        // free-form message, so echo every question (and its options) back
        // before the message itself.
        let mut lines = vec![
            "The operator chose to chat about your clarification questions instead of answering them one by one. Their message follows; take it as the answer to the whole survey, ask ONE refined survey only if something essential is still open, then revise the plan with plan_tasks/revise_task before continuing.".to_string(),
        ];
        for (index, question) in survey.questions.iter().enumerate() {
            lines.push(format!("Q{} [{}]: {}", index + 1, question.header, question.question));
            for option in &question.options {
                lines.push(format!("  - {}", option.label));
            }
        }
        lines.push(format!("Operator: {chat}"));
        return lines.join("\n");
    }
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
            chat: None,
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
            chat: None,
        }
    }

    /// The planning ask window: open at run start, shut as soon as begin_loop
    /// picks up a task, and reopened by a fresh operator message at the next
    /// cycle boundary.
    #[tokio::test]
    async fn the_ask_window_opens_at_start_shuts_for_a_task_and_reopens_on_an_operator_message() {
        let dir = tempfile::tempdir().unwrap();
        let mut run = test_run_in(&dir, |options| {
            options.collect_operator_messages = Some(Box::new(|_| Vec::new()));
        })
        .await;
        assert!(run.ask_window_open, "the window starts open for planning");
        crate::core::state::add_tasks(
            &mut run.state,
            vec![crate::core::state::HarnessTaskInput::from("do the work")],
            crate::core::state::HarnessTaskPlacement::End,
        );
        let _scope = run.begin_loop();
        assert!(!run.ask_window_open, "a task loop must shut the ask window");
        // A fresh operator message reopens it at the next cycle boundary. The
        // collector answers once, then reports nothing more.
        let once = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        run.options.collect_operator_messages = Some(Box::new(move |_| {
            if once.swap(true, std::sync::atomic::Ordering::SeqCst) {
                Vec::new()
            } else {
                vec![OperatorInboxEntry { at: None, text: "use the other crate".into() }]
            }
        }));
        let mut scope = run.begin_loop();
        assert!(!run.ask_window_open, "a task loop starts with the window shut");
        let _ = run.begin_cycle(&mut scope, 1);
        assert!(run.ask_window_open, "the operator message reopens the window");
        // An empty message is not a fresh instruction and must not reopen it.
        run.ask_window_open = false;
        run.options.collect_operator_messages = Some(Box::new(|_| {
            vec![OperatorInboxEntry { at: None, text: "   ".into() }]
        }));
        let mut empty_scope = run.begin_loop();
        let _ = run.begin_cycle(&mut empty_scope, 1);
        assert!(!run.ask_window_open, "a blank operator message leaves it shut");
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
            chat: None,
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
            chat: None,
        };
        let other = |index: i64, text: &str| HarnessSurveyAnswer { index, choice: None, other: Some(text.into()) };
        let pick = |index: i64, label: &str| HarnessSurveyAnswer { index, choice: Some(label.into()), other: None };
        // allow_other: question 0 accepts free text, question 1 does not.
        assert!(validate(&survey, &pair(other(0, "do it my way"), pick(1, "Yes"))).is_ok());
        assert!(validate(&survey, &pair(pick(0, "Poll"), other(1, "surprise me"))).is_err());
        // Choices must be listed labels.
        assert!(validate(&survey, &pair(pick(0, "Poll"), pick(1, "NotListed"))).is_err());
        assert!(validate(&survey, &pair(pick(0, "Channel"), pick(1, "No"))).is_ok());
        let empty = HarnessSurveyAnswers { at: "2026-01-01T00:00:00Z".into(), answers: vec![], chat: None };
        assert!(validate(&survey, &empty).is_err());
    }

    /// The injected operator feedback must carry the exact mandated
    /// plan-revision directive.
    #[test]
    /// A chat record renders the whole-survey directive, every question and
    /// option, and the operator's message — never the per-answer directive.
    #[test]
    fn render_survey_answers_echoes_a_chat_record_with_every_question() {
        let survey = two_question_survey();
        let answers = HarnessSurveyAnswers {
            at: "2026-01-01T00:00:00Z".into(),
            answers: Vec::new(),
            chat: Some("Let us talk this through: polls please".into()),
        };
        let rendered = render_survey_answers(&survey, &answers);
        assert!(!rendered.contains(crate::harness::prompt::ASK_USER_ANSWER_DIRECTIVE));
        assert!(rendered.contains("chose to chat about your clarification questions"));
        assert!(rendered.contains("ONE refined survey"));
        for question in &survey.questions {
            assert!(rendered.contains(&question.header), "header missing: {}", question.header);
            assert!(rendered.contains(&question.question), "question missing: {}", question.question);
            for option in &question.options {
                assert!(rendered.contains(&option.label), "option missing: {}", option.label);
            }
        }
        assert!(rendered.contains("Operator: Let us talk this through: polls please"));
    }

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
mod role_inference_tests {
    use super::*;

    /// Minimal run for unit tests of accounting-only paths: real HarnessRun
    /// via `test_run_in`'s temp-state construction.
    async fn role_inference_test_run() -> HarnessRun {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::mem::forget(dir); // keep backing dir alive for the run's duration
        let mut options = SolidStateHarnessOptions {
            goal: "test goal".into(),
            state_path: Some(path),
            ..SolidStateHarnessOptions::default()
        };
        HarnessRun::new(options).await.unwrap()
    }


    #[test]
    fn warmup_command_only_for_cargo_workspaces() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_string_lossy().into_owned();
        assert_eq!(build_warmup_command(&cwd, "Fix the bug; run `cargo test --release`"), None);
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        assert_eq!(
            build_warmup_command(&cwd, "Fix the bug in page"),
            Some(("cargo".to_string(), vec!["build".to_string(), "--tests".to_string(), "--quiet".to_string()]))
        );
        assert_eq!(
            build_warmup_command(&cwd, "Add the test. Acceptance: `cargo test --release --lib harness::model_call` passes."),
            Some((
                "cargo".to_string(),
                vec!["test".to_string(), "--release".to_string(), "--lib".to_string(), "harness::model_call".to_string(), "--no-run".to_string(), "--quiet".to_string()]
            )),
            "the warm-up compiles the profile and targets the goal's check runs"
        );
    }

    #[test]
    fn cargo_test_warmup_args_keep_selection_flags_and_drop_the_shell_tail() {
        let args = |command: &str| cargo_test_warmup_args(command).map(|args| args.join(" "));
        assert_eq!(args("cargo test -q 2>&1 | tail -20"), Some("test -q --no-run".to_string()));
        assert_eq!(
            args("timeout 120 cargo test --release -p drip --lib tools::patch -- --nocapture && echo ok"),
            Some("test --release -p drip --lib tools::patch --no-run --quiet".to_string())
        );
        assert_eq!(args("cargo test --no-run --quiet"), Some("test --quiet --no-run".to_string()));
        assert_eq!(args("cargo build --release"), None);
        assert_eq!(args("python3 -m unittest"), None);
    }

    #[test]
    fn command_shape_ignores_timeout_wrappers_filters_and_verbosity() {
        let base = "python3 -m unittest discover -s tests";
        for variant in [
            "python3 -m unittest discover -s tests -q 2>&1 | tail -20",
            "timeout 60 python3 -m unittest discover -s tests -v 2>&1 | tail -50; echo \"exit=$?\"",
            "timeout 90s python3 -m unittest discover -s tests 2>&1 | tail -60 | head -5",
            "cd /tmp && python3 -m unittest discover -s tests",
            "RUST_BACKTRACE=1 python3 -m unittest discover -s tests",
            "cd crate && RUST_BACKTRACE=1 python3 -m unittest discover -s tests -v",
            "python3 -m unittest discover -s tests 2>/dev/null",
            "python3 -m unittest discover -s tests -- --nocapture",
        ] {
            assert_eq!(normalize_command_shape(variant), base, "{variant}");
        }
        assert_eq!(normalize_command_shape("cargo test foo 2>/dev/null"), "cargo test foo");
        assert_eq!(normalize_command_shape("cargo test foo -- --nocapture"), "cargo test foo");
        assert_ne!(normalize_command_shape("python3 -m unittest tests.test_server"), base);
    }

    #[tokio::test]
    async fn repeated_command_shape_gets_a_nudge_until_an_edit_resets_it() {
        let dir = tempfile::tempdir().unwrap();
        let options = SolidStateHarnessOptions {
            goal: "test goal".into(),
            state_path: Some(dir.path().join("state.json")),
            cwd: Some(dir.path().to_string_lossy().into_owned()),
            tools: crate::tools::pack::builtin_tool_pack(crate::tools::pack::BuiltinToolOptions::with_allow_net(false)),
            ..SolidStateHarnessOptions::default()
        };
        let mut run = HarnessRun::new(options).await.unwrap();
        let first = run.execute_workspace_tool("c1", r#"{"command":"false 2>&1 | tail -3"}"#, None, "BASH");
        let second = run.execute_workspace_tool("c2", r#"{"command":"timeout 5 false -v"}"#, None, "BASH");
        assert!(!first.tool_content.contains("[harness] this command shape") && !second.tool_content.contains("[harness] this command shape"));
        let third = run.execute_workspace_tool("c3", r#"{"command":"false; echo \"exit=$?\""}"#, None, "BASH");
        assert!(third.tool_content.contains("has now run 3 times in a row"), "{}", third.tool_content);
        std::fs::write(dir.path().join("a.txt"), "x\n").unwrap();
        let _ = run.execute_workspace_tool("c4", r#"{"path":"a.txt","find":"x","replace":"y"}"#, None, "PATCH");
        let after_edit = run.execute_workspace_tool("c5", r#"{"command":"false"}"#, None, "BASH");
        assert!(!after_edit.tool_content.contains("[harness] this command shape"), "{}", after_edit.tool_content);
    }

    #[tokio::test]
    async fn an_external_native_runner_suite_settles_the_change_without_a_declared_check() {
        use crate::core::types::{HarnessVerificationRecord, VerificationAnchor, VerificationAnchorKind, VerificationEvidence, VerificationEvidenceKind};
        let dir = tempfile::tempdir().unwrap();
        let options = SolidStateHarnessOptions {
            goal: "tidy the helper; no check declared".into(),
            state_path: Some(dir.path().join("state.json")),
            cwd: Some(dir.path().to_string_lossy().into_owned()),
            ..SolidStateHarnessOptions::default()
        };
        let mut run = HarnessRun::new(options).await.unwrap();
        let record = |command: &str, kind: VerificationAnchorKind| HarnessVerificationRecord {
            at_iteration: 1,
            command: command.to_string(),
            failed: false,
            output_tail: String::new(),
            ran_no_tests: None,
            evidence: Some(VerificationEvidence {
                kind: VerificationEvidenceKind::Tests,
                executed: 3,
                passed: 3,
                failed: 0,
                skipped: None,
                detail: None,
                anchor: Some(VerificationAnchor { kind, source: None, downgraded_reason: None, coverage: None, expectation_subject: None }),
            }),
            id: Some("v1".into()),
        };
        run.state.mutations_since_verification = Some(0);
        run.state.last_verification = Some(record("cargo test -q", VerificationAnchorKind::External));
        let (how, _) = run.verified_after_last_edit(None).expect("external suite settles the change");
        assert!(how.contains("project suite"), "{how}");
        run.state.last_verification = Some(record("cargo test -q", VerificationAnchorKind::SelfAuthored));
        assert!(run.verified_after_last_edit(None).is_none(), "self-authored never settles");
        run.state.last_verification = Some(record("python3 probe.py", VerificationAnchorKind::External));
        assert!(run.verified_after_last_edit(None).is_none(), "an ad-hoc probe is not the project suite");
        run.state.last_verification = Some(record("cargo test -q", VerificationAnchorKind::External));
        run.state.mutations_since_verification = Some(1);
        assert!(run.verified_after_last_edit(None).is_none(), "an edit after the check unsettles it");
    }

    #[test]
    fn project_check_is_detected_from_the_workspace_layout() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_string_lossy().to_string();
        assert!(detect_project_check_command(&cwd).is_none(), "empty workspace");
        std::fs::create_dir_all(dir.path().join("tests")).unwrap();
        assert!(detect_project_check_command(&cwd).is_none(), "tests/ without python files");
        std::fs::write(dir.path().join("tests/test_a.py"), "").unwrap();
        assert_eq!(detect_project_check_command(&cwd).unwrap().0, "python3 -m unittest discover -s tests -q");
        std::fs::write(dir.path().join("pytest.ini"), "[pytest]\n").unwrap();
        assert_eq!(detect_project_check_command(&cwd).unwrap().0, "python3 -m pytest -q");
        std::fs::write(dir.path().join("package.json"), r#"{"scripts": {"test": "echo \"Error: no test specified\" && exit 1"}}"#).unwrap();
        assert_eq!(detect_project_check_command(&cwd).unwrap().0, "python3 -m pytest -q", "a placeholder npm test script is ignored");
        std::fs::write(dir.path().join("package.json"), r#"{"scripts": {"test": "vitest run"}}"#).unwrap();
        assert_eq!(detect_project_check_command(&cwd).unwrap().0, "npm test --silent");
        std::fs::write(dir.path().join("bun.lockb"), "").unwrap();
        assert_eq!(detect_project_check_command(&cwd).unwrap().0, "bun test");
        std::fs::write(dir.path().join("go.mod"), "module x\n").unwrap();
        assert_eq!(detect_project_check_command(&cwd).unwrap().0, "go test ./...");
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\n").unwrap();
        assert_eq!(detect_project_check_command(&cwd).unwrap(), ("cargo test -q".to_string(), "Cargo.toml"));
    }

    #[test]
    fn repeated_anomaly_bounces_reshape_the_finish_as_unreconciled() {
        assert_eq!(finish_bounce_family("harness: not accepted yet — 2 support-gap anomaly(ies) on record: x"), Some("support gap"));
        assert_eq!(finish_bounce_family("harness: not accepted yet — unresolved support-gap anomalies prevent a clean review verdict"), Some("support gap"));
        assert_eq!(finish_bounce_family("harness: not accepted yet — expectation 'exit' mismatched (expected 0, observed 1)"), Some("expectation mismatch"));
        assert_eq!(finish_bounce_family("harness: not accepted yet — registered expectation(s) have no observation: e1"), Some("unobserved expectation"));
        assert_eq!(finish_bounce_family("harness: not accepted yet — no correctness-class evidence"), None);
        assert_eq!(finish_bounce_family("Task task-1 marked completed."), None);

        let mut state = crate::core::state::create_harness_state("goal");
        state.anomalies.push(crate::core::types::HarnessAnomaly { subject: "exit code".into(), expected: "0".into(), observed: "1".into(), note: "support gap: unsupported revision".into() });
        state.expectations = vec![serde_json::from_value(serde_json::json!({
            "id": "e2", "subject": "files changed", "expected": "two", "registeredAtIteration": 1,
            "observations": [{"atIteration": 3, "observed": "three", "matches": false}]
        })).unwrap()];
        let raw = r#"{"status": "completed", "summary": "done", "taskId": "task-1"}"#;
        let input = unreconciled_finish_input(raw, &state, 2, "harness: not accepted yet — expectation 'files changed' mismatched (expected two)").expect("reshaped");
        let value: serde_json::Value = serde_json::from_str(&input).unwrap();
        assert_eq!(value["status"], "unreconciled");
        assert_eq!(value["taskId"], "task-1");
        assert!(value["summary"].as_str().unwrap().starts_with("done [harness: finished unreconciled after 2 identical bounces"), "{}", value["summary"]);
        let anomalies = value["anomalies"].as_array().unwrap();
        assert_eq!(anomalies.len(), 2, "{anomalies:?}");
        assert_eq!(anomalies[1]["subject"], "files changed");
        assert_eq!(anomalies[1]["observed"], "three");
        // Nothing on record and nothing unresolved: no reshape.
        let empty = crate::core::state::create_harness_state("goal");
        assert!(unreconciled_finish_input(raw, &empty, 2, "x").is_none());
    }

    #[test]
    fn a_native_runner_run_through_bash_counts_as_a_verification() {
        let cargo = r#"{"command": "cargo test -q --lib 2>&1 | tail -3"}"#;
        let output = "Bash command output from . — exit code 0.\n\ntest result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n";
        assert_eq!(bash_native_runner_verification("BASH", cargo, output).as_deref(), Some("cargo test -q --lib 2>&1 | tail -3"));
        // Not BASH, no runner, or output that is not a test result: nothing.
        assert!(bash_native_runner_verification("VERIFY", cargo, output).is_none());
        assert!(bash_native_runner_verification("BASH", r#"{"command": "ls -la"}"#, output).is_none());
        assert!(bash_native_runner_verification("BASH", cargo, "Bash command output — exit code 101.\n\nerror[E0425]: cannot find value").is_none());
        let unittest = r#"{"command": "python3 -m unittest discover -s tests -q"}"#;
        assert!(bash_native_runner_verification("BASH", unittest, "----\nRan 5 tests in 0.01s\n\nOK\n").is_some());
    }

    #[tokio::test]
    async fn repeated_command_nudge_also_emits_a_harness_op_event() {
        let dir = tempfile::tempdir().unwrap();
        let events: Arc<std::sync::Mutex<Vec<HarnessEvent>>> = Arc::default();
        let sink = events.clone();
        let options = SolidStateHarnessOptions {
            goal: "test goal".into(),
            state_path: Some(dir.path().join("state.json")),
            cwd: Some(dir.path().to_string_lossy().into_owned()),
            tools: crate::tools::pack::builtin_tool_pack(crate::tools::pack::BuiltinToolOptions::with_allow_net(false)),
            on_event: Some(Arc::new(move |event: HarnessEvent| {
                sink.lock().unwrap().push(event);
            })),
            ..SolidStateHarnessOptions::default()
        };
        let mut run = HarnessRun::new(options).await.unwrap();
        for call in ["c1", "c2", "c3"] {
            let _ = run.execute_workspace_tool(call, r#"{"command":"false 2>&1 | tail -3"}"#, None, "BASH");
        }
        let events = events.lock().unwrap();
        let nudges: Vec<&HarnessEvent> = events.iter().filter(|event| event.r#type == HarnessEventType::HarnessOp && event.detail.contains("flailing nudge")).collect();
        assert_eq!(nudges.len(), 1, "one nudge event after three identical runs");
        assert_eq!(nudges[0].detail, "flailing nudge: false ran 3 times with no edit between");
    }

    #[tokio::test]
    async fn hung_command_is_refused_on_an_identical_rerun() {
        let dir = tempfile::tempdir().unwrap();
        let options = SolidStateHarnessOptions {
            goal: "test goal".into(),
            state_path: Some(dir.path().join("state.json")),
            cwd: Some(dir.path().to_string_lossy().into_owned()),
            tools: crate::tools::pack::builtin_tool_pack(crate::tools::pack::BuiltinToolOptions::with_allow_net(false)),
            ..SolidStateHarnessOptions::default()
        };
        let mut run = HarnessRun::new(options).await.unwrap();
        // A BASH that hits its timeout is remembered as hung …
        let hung = run.execute_workspace_tool("c1", r#"{"command":"sleep 5","timeoutMs":1000}"#, None, "BASH");
        assert!(hung.failed, "{}", hung.tool_content);
        assert!(output_reports_hang(&hung.tool_content), "{}", hung.tool_content);
        assert_eq!(run.hung_commands, vec!["sleep 5".to_string()]);
        // … and the identical command is refused without running.
        let again = run.execute_workspace_tool("c2", r#"{"command":"sleep 5","timeoutMs":1000}"#, None, "BASH");
        assert!(!again.dispatched && again.failed);
        assert!(again.tool_content.contains("already hung"), "{}", again.tool_content);
        // A changed command runs normally.
        let changed = run.execute_workspace_tool("c3", r#"{"command":"timeout 1 sleep 5; true"}"#, None, "BASH");
        assert!(changed.dispatched, "{}", changed.tool_content);
        // After an edit the fix may have landed: the original command runs again.
        std::fs::write(dir.path().join("a.txt"), "x\n").unwrap();
        let _ = run.execute_workspace_tool("c4", r#"{"path":"a.txt","find":"x","replace":"y"}"#, None, "PATCH");
        assert!(run.hung_commands.is_empty());
    }

    #[tokio::test]
    async fn a_rerun_of_a_hung_shape_runs_under_the_leash() {
        let dir = tempfile::tempdir().unwrap();
        let options = SolidStateHarnessOptions {
            goal: "test goal".into(),
            state_path: Some(dir.path().join("state.json")),
            cwd: Some(dir.path().to_string_lossy().into_owned()),
            tools: crate::tools::pack::builtin_tool_pack(crate::tools::pack::BuiltinToolOptions::with_allow_net(false)),
            ..SolidStateHarnessOptions::default()
        };
        let mut run = HarnessRun::new(options).await.unwrap();
        run.hung_shape_leash_ms = 1_000;
        let hung = run.execute_workspace_tool("c1", r#"{"command":"sleep 4","timeoutMs":1000}"#, None, "BASH");
        assert!(hung.failed && output_reports_hang(&hung.tool_content), "{}", hung.tool_content);
        assert_eq!(run.hung_shapes.len(), 1);
        // Same shape under a different spelling: not refused, but cut at the leash.
        let started = std::time::Instant::now();
        let again = run.execute_workspace_tool("c2", r#"{"command":"timeout 9 sleep 4 2>&1 | tail -1"}"#, None, "BASH");
        assert!(again.dispatched, "{}", again.tool_content);
        assert!(started.elapsed() < std::time::Duration::from_secs(3), "leash did not apply: {:?}", started.elapsed());
        assert!(again.tool_content.contains("ran under a 1s leash"), "{}", again.tool_content);
        // An explicit timeout at or under the leash is left alone; a longer one is leashed.
        assert!(leash_timeout(r#"{"command":"x","timeoutMs":500}"#, "BASH", 1_000).is_none());
        assert_eq!(leash_timeout(r#"{"command":"x","timeoutMs":5000}"#, "BASH", 1_000).as_deref(), Some(r#"{"command":"x","timeoutMs":1000}"#));
        assert_eq!(leash_timeout(r#"{"command":"x"}"#, "VERIFY", 1_000).as_deref(), Some(r#"{"command":"x","timeout":1000}"#));
        // Edits do not clear the shape memory: the leash still applies after a PATCH.
        std::fs::write(dir.path().join("a.txt"), "x\n").unwrap();
        let _ = run.execute_workspace_tool("c3", r#"{"path":"a.txt","find":"x","replace":"y"}"#, None, "PATCH");
        assert!(run.hung_commands.is_empty() && run.hung_shapes.len() == 1);
    }

    fn usage(completion: i64) -> Option<crate::harness::model_call::OpenAICompatibleResponseUsage> {
        Some(crate::harness::model_call::OpenAICompatibleResponseUsage {
            completion_tokens_details: None,
            completion_tokens: Some(completion),
            ..Default::default()
        })
    }

    fn call(latency_ms: i64) -> ModelCallRecord {
        ModelCallRecord { latency_ms, ..Default::default() }
    }

    #[tokio::test]
    async fn hedge_flags_count_into_role_totals() {
        let mut run = role_inference_test_run().await;
        run.active_role = Some("author".to_string());
        // Plain call: no hedge.
        run.record_model_usage(usage(10).as_ref(), &call(100));
        // Hedged call where the second request answered first.
        run.record_model_usage(
            usage(5).as_ref(),
            &ModelCallRecord { hedged: true, hedge_won: true, ..call(50) },
        );
        // Hedged call where the first request won.
        run.record_model_usage(
            usage(5).as_ref(),
            &ModelCallRecord { hedged: true, hedge_won: false, ..call(60) },
        );
        let totals = run.role_inference.get("author").unwrap();
        assert_eq!(totals.hedges_fired, 2);
        assert_eq!(totals.hedges_won, 1);
    }

    #[tokio::test]
    async fn repeated_role_sums_calls_latency_and_tokens() {
        let mut run = role_inference_test_run().await;
        run.active_role = Some("author".to_string());
        run.record_model_usage(usage(10).as_ref(), &call(100));
        run.record_model_usage(usage(5).as_ref(), &call(50));
        let totals = run.role_inference.get("author").unwrap();
        assert_eq!(
            (totals.calls, totals.latency_ms, totals.completion_tokens),
            (2, 150, 15)
        );
        // prompt/cache-read are summed from each call's usage (Default usage()
        // helper sets none, so seed an explicit one for the cache-read path).
        let mut cached_usage = usage(10).unwrap();
        cached_usage.prompt_tokens = Some(700);
                cached_usage.prompt_tokens_details = Some(
            crate::harness::model_call::OpenAICompatibleResponsePromptTokensDetails {
                cached_tokens: Some(400),
            },
        );
        run.record_model_usage(Some(&cached_usage), &call(10));
        let totals = run.role_inference.get("author").unwrap();
        assert_eq!((totals.prompt_tokens, totals.cache_read_tokens), (700, 400));
    }

    #[tokio::test]
    async fn distinct_roles_stay_separate() {
        let mut run = role_inference_test_run().await;
        run.active_role = Some("author".to_string());
        run.record_model_usage(usage(10).as_ref(), &call(100));
        run.active_role = Some("reviewer".to_string());
        run.record_model_usage(usage(3).as_ref(), &call(30));
        assert_eq!(run.role_inference.get("author").unwrap().completion_tokens, 10);
        assert_eq!(run.role_inference.get("reviewer").unwrap().completion_tokens, 3);
        assert_eq!(run.role_inference.len(), 2);
    }

    #[tokio::test]
    async fn missing_role_buckets_under_default() {
        let mut run = role_inference_test_run().await;
        run.record_model_usage(usage(7).as_ref(), &call(20));
        let totals = run.role_inference.get("default").unwrap();
        assert_eq!((totals.calls, totals.latency_ms, totals.completion_tokens), (1, 20, 7));
    }

    #[tokio::test]
    async fn missing_usage_adds_zero_tokens_but_counts_call() {
        let mut run = role_inference_test_run().await;
        run.record_model_usage(None, &call(40));
        let totals = run.role_inference.get("default").unwrap();
        assert_eq!((totals.calls, totals.latency_ms, totals.completion_tokens), (1, 40, 0));
    }

    #[tokio::test]
    async fn role_inference_serialises_camel_case() {
        let mut run = role_inference_test_run().await;
        run.active_role = Some("planner".to_string());
        run.record_model_usage(usage(9).as_ref(), &call(120));
        let json = serde_json::to_value(&run.role_inference).unwrap();
        let entry = json.get("planner").unwrap();
        assert!(entry.get("latencyMs").is_some(), "camelCase latencyMs expected: {json}");
        assert!(entry.get("completionTokens").is_some(), "camelCase completionTokens expected: {json}");
        assert!(entry.get("calls").is_some());
    }
}

#[cfg(test)]
mod review_opt_out_tests {

    #[test]
    fn base_model_effort_defaults_to_low_only_when_the_profile_sets_none() {
        assert_eq!(base_model_reasoning_effort(None), (Some("low".to_string()), true));
        assert_eq!(base_model_reasoning_effort(Some("  ")), (Some("low".to_string()), true));
        assert_eq!(base_model_reasoning_effort(Some("high")), (Some("high".to_string()), false));
        assert_eq!(base_model_reasoning_effort(Some(" medium ")), (Some("medium".to_string()), false));
    }
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

    fn mcp_role(mcp_servers: Option<Vec<&str>>, tool_names: Option<Vec<&str>>) -> HarnessRoleRuntime {
        HarnessRoleRuntime {
            description: None,
            r#loop: None,
            name: "author".to_string(),
            route: None,
            system_prompt_suffix: None,
            tool_names: tool_names.map(|names| names.into_iter().map(String::from).collect()),
            verified_by: None,
            blind: false,
            mcp_servers: mcp_servers.map(|names| names.into_iter().map(String::from).collect()),
        }
    }

    fn strings(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    // The role's mcpServers wins over the run-level --mcp set; a role without
    // one inherits the run-level set; --no-mcp (Some(empty)) beats both.
    #[test]
    fn effective_mcp_servers_prefers_the_role_then_the_run_gate() {
        let role = mcp_role(Some(vec!["github"]), None);
        assert_eq!(
            effective_mcp_servers(Some(&role), Some(strings(&["files"]))),
            Some(strings(&["github"]))
        );
        let unscoped = mcp_role(None, None);
        assert_eq!(
            effective_mcp_servers(Some(&unscoped), Some(strings(&["files"]))),
            Some(strings(&["files"]))
        );
        assert_eq!(effective_mcp_servers(None, Some(strings(&["files"]))), Some(strings(&["files"])));
        assert_eq!(effective_mcp_servers(Some(&unscoped), None), None);
        assert_eq!(effective_mcp_servers(Some(&role), Some(Vec::new())), Some(Vec::new()));
    }

    // MCP tools are gated by their server alone — the role's tool allowlist
    // never has to name them — while builtins follow the allowlist as before.
    #[test]
    fn loop_allows_tool_scopes_mcp_tools_by_server_and_builtins_by_allowlist() {
        let allowlist = strings(&["READ"]);
        let scope = strings(&["github"]);
        assert!(loop_allows_tool("READ", Some(&allowlist), Some(&scope)));
        assert!(!loop_allows_tool("PATCH", Some(&allowlist), Some(&scope)));
        assert!(loop_allows_tool("PATCH", None, Some(&scope)));
        assert!(loop_allows_tool("MCP__github__search", Some(&allowlist), Some(&scope)));
        assert!(!loop_allows_tool("MCP__files__read", Some(&allowlist), Some(&scope)));
        assert!(!loop_allows_tool("MCP__github__search", None, None));
        assert!(!loop_allows_tool("MCP__github__search", None, Some(&[])));
    }
}
