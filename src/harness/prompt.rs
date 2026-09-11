use crate::core::types::{HarnessRunReason, HarnessState, HarnessTask, HarnessTaskStatus};
use crate::harness::chat_types::ChatRoleTag;
use crate::harness::transport::{
    build_multimodal_user_content, TransportContent, TransportRequestMessage,
};

pub const DEFAULT_HARNESS_SYSTEM_PROMPT: &str = concat!(
    "You are one subagent in a relay working inside a solid-state harness. A planning loop builds a common task list; then each task is worked by its own task loop — you are the subagent for the current loop.",
    " A task loop runs as a few short cycles that share this conversation transcript: recent tool results stay hot in the transcript, older ones are folded to one-line digests, and when the loop ends the whole transcript is discarded. Only the shared state store survives between loops.",
    " Each loop starts with the full shared state: the goal, the common task list, saved memory notes, short-lived observations, and warm context (tool results the harness cached because separate loops kept reaching for them).",
    " The goal may be a follow-up to earlier goals; the history section lists what was already accomplished this session. Rely on history and shared memory to stay consistent with earlier work.",
    " The last_activation section shows what the previous loop did and how it ended; use it to continue instead of repeating work.",
    " Do exactly one unit of work per task loop: the CURRENT TASK shown in the prompt.",
    " Use the available tools to inspect and change the workspace.",
    " Anything you do not write to shared state is forgotten when this task loop ends.",
    " To persist knowledge for future loops, call remember for durable facts, or observe for short-lived findings (a failing check's cause, an in-flight hypothesis) — observations expire after a few loops unless re-observed. To persist progress, call finish_task with a summary when the current task is done, or status blocked plus follow-up context when you cannot finish it; note_task records partial progress on the task without ending the loop.",
    " Never claim a verification (tests, build, typecheck) succeeded unless its passing output is visible in this loop's transcript or warm context. When a verification fails, record the failing detail with observe before moving on.",
    " Verification counts establish executed checks, not that the chosen specification or formula is correct. For a numeric deliverable, validate the result by a different method/reference or a meaningful analytical bound or simulation, and state shared assumptions. Repeating the same arithmetic or comparing with hardcoded expected output establishes consistency only.",
    " Known correctness defects or unresolved assumptions that undermine a reported value or goal requirement are blocking P1 findings even in your own caveats/deviations: resolve them or finish_task blocked. Ordinary statistical uncertainty or a justified limitation is not automatically a defect. Do not downgrade a known defect merely because it is documented.",
    " Warm context entries marked failed are calls that did not succeed when last run — do not re-run a call warm context already answers; act on the cached result instead.",
    " A folded transcript entry means that result was already seen this loop — re-run the call only if you truly need the full output again.",
    " Prefer the project's declared commands (package.json scripts, Makefile targets) over improvised equivalents.",
    " When a check fails because of the runner or environment (wrong test command, missing global, unavailable module), fix the invocation — never edit product code to accommodate a different runner (no stubbing globals or modules to make tests run).",
    " If the goal is a question or asks for a status report and needs no workspace changes, answer it from the shared state (history, memory, task summaries) with the respond op instead of planning tasks — verify with tools first only if the answer is not already recorded.",
    " If the task list is empty or exhausted, call plan_tasks to break the goal into small, concrete tasks for the other subagents.",
    " The todo list is shared and yours to keep truthful as you learn: drop_task removes tasks that are no longer needed, revise_task rewrites titles that no longer match reality, and plan_tasks with placement \"next\" inserts newly discovered prerequisite work before the remaining tasks.",
    " Make file edits with PATCH rather than shell in-place editing (sed -i, or inline scripts that rewrite files) — PATCH validates the edit, journals an undo entry, and reports an honest per-edit result; shell edits bypass all three.",
    " Checks are either correctness-class (compared against something the agent did not author — a pre-existing project test, a task-provided fixture, a published constant, or an invariant independent of the implementation) or consistency-class (compared only against the agent's own derivation); completion needs at least one correctness-class check or an explicit anchor=none declaration saying why no external anchor exists for the claim. Declare the anchor on every VERIFY call (anchor.kind external or self; a check that names a file you edited is downgraded to self), state your confidence (low, medium, high) on every finish_task, and when a revision changes a reported output, cite evidence outside the fix that the new value is closer to truth.",
    " When a goal produces a measurable output (a number, count, shape, sign, unit, latency, row count), register the expected value from the domain via plan_tasks.expectations BEFORE computing it, and treat a later mismatch as a defect in the model — finish with status unreconciled and record the anomaly rather than explaining the value away.",
    " Prefer small tasks that one loop can finish. Do not narrate; act through tool calls."
);

#[cfg(test)]
mod anchoring_render_tests {
    use super::*;
    use crate::core::types::{
        ClaimedConfidence, CompletionAnchor, CompletionAnchorKind, HarnessExpectation,
        HarnessExpectationObservation,
    };

    fn state_with_expectations(expectations: Vec<HarnessExpectation>) -> HarnessState {
        let mut state = serde_json::from_value::<HarnessState>(serde_json::json!({
            "createdAt": "2026-01-01T00:00:00.000Z",
            "goal": "add a --verbose flag to the CLI",
            "history": [],
            "iteration": 3,
            "loop": 1,
            "memory": [],
            "observations": [],
            "promotedContext": [],
            "tasks": [],
            "telemetry": {},
            "version": 1
        }))
        .unwrap();
        state.expectations = expectations;
        state
    }

    fn expectation(
        id: &str,
        subject: &str,
        expected: &str,
        observations: Vec<HarnessExpectationObservation>,
    ) -> HarnessExpectation {
        HarnessExpectation {
            id: id.to_string(),
            subject: subject.to_string(),
            expected: expected.to_string(),
            registered_at_iteration: 1,
            observations,
        }
    }

    #[test]
    fn empty_expectations_render_no_section() {
        let state = state_with_expectations(Vec::new());
        assert!(
            !build_iteration_user_message(&state, &iteration_args()).contains("Pre-registered expectations:")
        );
    }

    #[test]
    fn unobserved_and_latest_observations_render() {
        let state = state_with_expectations(vec![
            expectation("e1", "line count", "12", Vec::new()),
            expectation(
                "e3",
                "file count",
                "7",
                vec![HarnessExpectationObservation {
                    at_iteration: 3,
                    observed: "7".to_string(),
                    matches: true,
                    evidence: None,
                    observed_after_records: Some(0),
                }],
            ),
            expectation(
                "e2",
                "row count",
                "40",
                vec![HarnessExpectationObservation {
                    at_iteration: 2,
                    observed: "41".to_string(),
                    matches: false,
                    evidence: None,
                    observed_after_records: Some(0),
                }],
            ),
        ]);
        let message = build_iteration_user_message(&state, &iteration_args());
        assert!(message.contains("Pre-registered expectations:"));
        assert!(
            message.contains(
                "expectation e1 \"line count\": expected \"12\" — unobserved"
            )
        );
        assert!(
            message.contains(
                "expectation e3 \"file count\": expected \"7\" — observed \"7\" (matched at iteration 3)"
            )
        );
        assert!(
            message.contains(
                "expectation e2 \"row count\": expected \"40\" — observed \"41\" (mismatched at iteration 2)"
            )
        );
    }

    #[test]
    fn completion_anchor_renders_when_recorded() {
        let mut state = state_with_expectations(Vec::new());
        state.completion_anchor = Some(CompletionAnchor {
            kind: CompletionAnchorKind::External,
            note: Some("pre-existing project test".to_string()),
            claimed_confidence: Some(ClaimedConfidence::High),
        });
        let message = build_iteration_user_message(&state, &iteration_args());
        assert!(
            message.contains("completion anchored externally (correctness-class check passed)")
        );
    }

    #[test]
    fn no_completion_anchor_renders_nothing() {
        let state = state_with_expectations(Vec::new());
        assert!(
            !build_iteration_user_message(&state, &iteration_args()).contains("completion anchor")
        );
    }

    fn iteration_args() -> IterationUserMessageArgs<'static> {
        IterationUserMessageArgs {
            current_date: "2026-01-01",
            current_task: None,
            loop_info: None,
            repo_memory_dir: None,
            repo_memory_index: None,
            run_budget: None,
            stall_limit: None,
            workspace: None,
        }
    }
}

pub const RUN_SUMMARY_SYSTEM_PROMPT: &str = concat!(
    "You are the reporting step at the end of a solid-state harness run.",
    " You are given the goal, the final todo list with per-task summaries, shared memory notes, session history from earlier goals, and (when available) ground-truth workspace changes.",
    " Write a concise message directly to the user: summarize what was accomplished, and call out anything blocked or dropped and why.",
    " Reply with plain markdown text. Do not call tools."
);

// Load-bearing prompt fragments (debt audit T3): integration tests assert
// through these constants instead of prose copies.
pub const OPERATOR_MESSAGES_HEADER: &str = "operator_messages";
pub const STALL_WARNING_PREFIX: &str = "stall_warning:";
pub const LAST_VERIFICATION_PREFIX: &str = "last_verification";
pub const VERIFICATION_FAILED_DIRECTIVE: &str = "do not call finish_task status completed";
pub const VERIFICATION_STUCK_PREFIX: &str = "verification_stuck:";
/// Opt-in clarification-survey guidance, appended to the system prompt ONLY
/// when ask_user is enabled for the run (`--ask`). Absent entirely when
/// disabled, so the disabled-path system prompt stays byte-identical.
pub const ASK_USER_GUIDANCE_FRAGMENT: &str = concat!(
    "# Clarification questions (ask_user)",
    " ask_user is enabled for this run. When the goal is ambiguous or an approach tradeoff needs the operator's decision, ask early — preferably during planning, before implementing.",
    " Batch ALL of your questions into a single ask_user call as one survey, and put your best-guess option FIRST in each option list.",
    " Never ask what the repo itself answers: read files and run tools first.",
    " After the operator's answers arrive, revise the plan with plan_tasks/revise_task to reflect them before implementing."
);
/// Exact directive prefixed to the injected Q->A summary when the operator
/// answers a pending ask_user survey (live or on resume).
pub const ASK_USER_ANSWER_DIRECTIVE: &str = "The operator answered your clarification questions. Revise the plan now with plan_tasks/revise_task to reflect these answers before continuing.";

#[derive(Clone, Debug, PartialEq)]
pub struct HarnessRunBudget {
    pub total: i64,
    pub used: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HarnessLoopRole {
    pub description: Option<String>,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HarnessLoopInfo {
    pub index: i64,
    pub max_cycles: i64,
    /// The role (capability profile) this loop's subagent runs under, when the run configures roles.
    pub role: Option<HarnessLoopRole>,
}

#[derive(Clone, Debug, Default)]
pub struct IterationUserMessageArgs<'a> {
    pub current_date: &'a str,
    pub current_task: Option<&'a HarnessTask>,
    pub loop_info: Option<HarnessLoopInfo>,
    pub repo_memory_dir: Option<&'a str>,
    pub repo_memory_index: Option<&'a str>,
    pub run_budget: Option<HarnessRunBudget>,
    pub stall_limit: Option<i64>,
    pub workspace: Option<&'a str>,
}

#[derive(Clone, Debug, Default)]
pub struct IterationMessagesArgs<'a> {
    pub current_date: &'a str,
    pub current_task: Option<&'a HarnessTask>,
    pub goal_context: Option<&'a str>,
    pub goal_images: Option<Vec<String>>,
    pub loop_info: Option<HarnessLoopInfo>,
    pub repo_memory_dir: Option<&'a str>,
    pub repo_memory_index: Option<&'a str>,
    pub run_budget: Option<HarnessRunBudget>,
    pub stall_limit: Option<i64>,
    pub system_prompt: &'a str,
    pub workspace: Option<&'a str>,
}

#[derive(Clone, Debug)]
pub struct CycleContinuationArgs<'a> {
    pub current_task: Option<&'a HarnessTask>,
    pub cycle: i64,
    pub max_cycles: i64,
    pub run_budget: Option<HarnessRunBudget>,
}

const MAX_HISTORY_GOALS: usize = 5;
const MAX_HISTORY_TASK_LINES: usize = 8;
const MAX_HISTORY_SUMMARY_CHARS: usize = 400;

pub struct RunSummaryMessagesArgs<'a> {
    pub current_date: &'a str,
    pub reason: HarnessRunReason,
    /// Harness-recorded workspace tool calls this run, by tool name (DELEGATE included).
    pub tool_usage: Option<std::collections::BTreeMap<String, u64>>,
    /// Ground-truth workspace facts gathered by the caller (e.g. git status) at run end.
    pub workspace_changes: Option<String>,
}

/// Builds the tool-usage line — names sorted by code point, zero counts dropped.
pub fn build_tool_usage_line(tool_usage: &std::collections::BTreeMap<String, u64>) -> String {
    let entries: Vec<String> = tool_usage
        .iter()
        .filter(|(_, count)| **count > 0)
        .map(|(name, count)| format!("{name} {count}"))
        .collect();
    let delegated = tool_usage.get("DELEGATE").copied().unwrap_or(0);
    let tail = if delegated > 0 {
        format!("{delegated} DELEGATE child session(s) were spawned")
    } else {
        "no DELEGATE child sessions were spawned".to_string()
    };

    if entries.is_empty() {
        return format!("tool_usage (harness-recorded for this run): no workspace tools were called; {tail}");
    }

    format!("tool_usage (harness-recorded for this run): {}; {tail}", entries.join(", "))
}

pub fn compose_harness_system_prompt(persona_prompt: Option<&str>) -> String {
    match persona_prompt {
        Some(persona) if !persona.trim().is_empty() => format!("{persona}\n\n{}", DEFAULT_HARNESS_SYSTEM_PROMPT),
        _ => DEFAULT_HARNESS_SYSTEM_PROMPT.to_string(),
    }
}

pub fn looks_like_question_goal(goal: &str) -> bool {
    use regex::Regex;
    use std::sync::OnceLock;

    fn question_regexes() -> &'static (Regex, Regex) {
        static REGEXES: OnceLock<(Regex, Regex)> = OnceLock::new();
        REGEXES.get_or_init(|| {
            (
                Regex::new(r"\?\s*$").expect("valid regex"),
                Regex::new(r"(?i)^(what|which|who|whose|when|where|why|how|is|are|was|were|do|does|did|can|could|should|would|has|have|had|summarize|describe|explain|report|tell me|list)\b").expect("valid regex"),
            )
        })
    }

    let trimmed = goal.trim();

    question_regexes().0.is_match(trimmed) || question_regexes().1.is_match(trimmed)
}

pub fn build_iteration_user_message(state: &HarnessState, args: &IterationUserMessageArgs<'_>) -> String {
    let mut sections: Vec<String> = Vec::new();

    // Stable-first ordering: the goal leads so the provider's
    // prompt cache keeps hitting across cycles; the per-cycle fields sit just
    // above the instruction.
    sections.push(format!("goal: {}", state.goal));
    let mut cycle_sections: Vec<String> = vec![
        format!("current_date: {}", args.current_date),
        format!("cycle: {}", state.iteration),
    ];

    if !state.operator_messages.as_ref().map_or(true, Vec::is_empty) {
        // Steering sent to a live session outranks the original goal text: the
        // operator is the same authority that set the goal, speaking later.
        let mut operator_section = format!(
            "{} (fresh instructions from the operator, sent while this run executes — they take precedence over the original goal where they conflict; incorporate them now via plan_tasks/drop_task/revise_task or respond):",
            OPERATOR_MESSAGES_HEADER
        );
        for message in state.operator_messages.iter().flatten() {
            operator_section.push_str(&format!("\n- [cycle {}] {}", message.received_at_iteration, message.text));
        }
        sections.push(operator_section);
    }

    if let Some(loop_role) = args.loop_info.as_ref().and_then(|info| info.role.as_ref()) {
        let mut role_section = format!("role: this loop runs as the \"{}\" subagent", loop_role.name);
        if let Some(description) = &loop_role.description {
            role_section.push_str(&format!(" — {}", description));
        }
        sections.push(role_section);
    }

    // Without this line models guess their location (cd /root, cd /tmp/<name>)
    // and burn tool rounds on failed commands — seen in the 2026-07-06 e2e eval.
    if let Some(workspace) = &args.workspace {
        sections.push(format!(
            "workspace: {} — tools already run from this directory; use relative paths and never cd to a guessed absolute path",
            workspace
        ));
    }

    if args.loop_info.is_some() {
        cycle_sections.push(format!(
            "task_loop: loop {}, cycle 1 of up to {} — cycles in this loop share this transcript; when the loop ends only shared state survives",
            args.loop_info.as_ref().unwrap().index,
            args.loop_info.as_ref().unwrap().max_cycles
        ));
    }

    if let Some(run_budget) = &args.run_budget {
        cycle_sections.push(build_run_budget_line(run_budget));
    }

    if !state.history.is_empty() {
        sections.push(build_history_section(state));
    }

    if !state.tasks.is_empty() {
        let mut tasks_section = String::from("tasks:");
        for task in &state.tasks {
            tasks_section.push('\n');
            tasks_section.push_str(&format_task_line(task, args.current_task.as_ref().map(|task| task.id.as_str())));
        }
        sections.push(tasks_section);
    } else {
        sections.push("tasks: none yet".to_string());

        // Weak models plan investigation tasks for goals that are plainly
        // questions; a targeted nudge at the planning moment outperforms the
        // general system-prompt line (round-2 finding: a pure status question
        // burned its whole budget on redundant tool calls).
        if looks_like_question_goal(&state.goal) {
            sections.push(
                "instruction: this goal reads as a question or report request. If the history, memory, and task summaries above already answer it, call respond with the answer NOW instead of planning tasks; only investigate first if the answer is genuinely not recorded.".to_string(),
            );
        }
    }

    if args.current_task.map_or(false, |task| task.stall_count > 0) {
        let stall_count = args.current_task.as_ref().unwrap().stall_count;
        let limit = args.stall_limit.unwrap_or(3);
        let remaining = limit.saturating_sub(stall_count);
        sections.push(format!(
            "{} the previous {} task loop(s) recorded no progress. After {} more loop(s) without progress this task will be AUTO-BLOCKED. Progress = editing files (PATCH) or persisting findings (note_task, observe, remember, finish_task). Start by persisting what you already know, then edit — do not re-read files warm_context already shows.",
            STALL_WARNING_PREFIX, stall_count, remaining
        ));
    }

    // The verification story travels to every loop, not just the end-of-run
    // summary: a worker starting a fresh loop must know the last test run
    // FAILED (or that edits landed after the last pass, making it stale).
    if let Some(verification) = &state.last_verification {
        let mutations_after = state.mutations_since_verification.unwrap_or(0);
        let mut verification_text = format!(
            "{} (harness-recorded — trust THIS over memory or summaries): {} → {} (cycle {}{})",
            LAST_VERIFICATION_PREFIX,
            verification.command,
            crate::core::state::describe_verification_outcome(verification.failed, verification.ran_no_tests, verification.evidence.as_ref()),
            verification.at_iteration,
            if mutations_after > 0 {
                format!("; STALE — {} workspace edit(s) landed after it, re-run before relying on it", mutations_after)
            } else {
                String::new()
            }
        );

        verification_text.push_str(&format!("\n{}", verification.output_tail));

        if verification.failed {
            verification_text.push_str(&format!(
                "\ninstruction: the most recent verification FAILED — {} for build/fix work until a newer run of it passes.",
                VERIFICATION_FAILED_DIRECTIVE
            ));
        }

        sections.push(verification_text);
    }

    // Pre-registered expectations (the registered-before-results contract)
    // and the latest completion anchor ride along with the verification
    // story, so a fresh loop sees the registered values before it computes
    // anything and sees how the last completion was anchored. Both renderings
    // come from the shared core/state helpers.
    let expectations_text = crate::core::state::describe_expectations(state);
    if !expectations_text.is_empty() {
        sections.push(format!(
            "Pre-registered expectations:\n{}",
            expectations_text
        ));
    }

    if state.completion_anchor.is_some() {
        sections.push(crate::core::state::describe_completion_anchor(state));
    }

    if let Some(streak) = &state.verification_streak {
        if streak.consecutive_failures >= 2 {
            sections.push(format!(
                "{} {} has now failed {} consecutive times with UNCHANGED output — the edits between runs are not affecting the failure, and such loops count as stalled. Stop patching the same spot: re-read the failing output, form a new hypothesis (record it with observe), try a different location, or finish_task status blocked with what you ruled out.",
                VERIFICATION_STUCK_PREFIX, streak.command, streak.consecutive_failures
            ));
        }
    }

    if !state.memory.is_empty() {
        let memory_lines: Vec<String> = state
            .memory
            .iter()
            .map(|note| format!("- ({}) {}", note.id, note.text))
            .collect();
        sections.push(format!("memory:\n{}", memory_lines.join("\n")));
    }

    if !args.repo_memory_index.as_deref().unwrap_or("").trim().is_empty() {
        let repo_memory_guidance = if args.repo_memory_dir.is_some() {
            format!(
                "pages live in {} (open with READ using the absolute path); save durable repo learnings with remember scope=repo.",
                args.repo_memory_dir.as_deref().unwrap()
            )
        } else {
            "pages live in ~/.drip/projects/<slug>/memory/ (open with READ when relevant); save durable repo learnings with remember scope=repo.".to_string()
        };
        sections.push(format!(
            "repo_memory (durable repo-level notes saved across sessions):\n{}\n{}",
            args.repo_memory_index.as_deref().unwrap(),
            repo_memory_guidance
        ));
    }

    if !state.observations.is_empty() {
        let mut observations_section = String::from(
            "observations (short-lived findings — ttl decays once per task loop; each expires at 0 unless re-observed with observe):",
        );
        for observation in &state.observations {
            observations_section.push_str(&format!("\n- ({}, ttl {}) {}", observation.id, observation.ttl, observation.text));
        }
        sections.push(observations_section);
    }

    if !state.promoted_context.is_empty() {
        let mut warm_section = String::from(
            "warm_context (cached tool results, auto-managed — do not re-run these calls unless the cached output looks stale):",
        );
        for entry in &state.promoted_context {
            warm_section.push_str(&format!(
                "\n[{} {}] (ttl {}{}{})",
                entry.tool_name,
                entry.input_preview,
                entry.ttl,
                if entry.dynamic { ", refreshed live" } else { "" },
                if entry.last_failed == Some(true) { ", FAILED when last run" } else { "" }
            ));
            warm_section.push('\n');
            warm_section.push_str(&entry.output);
        }
        sections.push(warm_section);
    }

    let last_activation_section = build_last_activation_section(state);

    if !last_activation_section.is_empty() {
        sections.push(last_activation_section);
    }

    sections.push(cycle_sections.join("\n"));

    if let Some(current_task) = args.current_task.as_ref() {
        let mut task_sections: Vec<String> = Vec::new();
        task_sections.push(format!("current_task: {}: {}", current_task.id, current_task.title));

        if !current_task.notes.is_empty() {
            let notes_lines: Vec<String> = current_task
                .notes
                .iter()
                .map(|note| format!("- {}", note))
                .collect();
            task_sections.push(format!("notes from earlier loops:\n{}", notes_lines.join("\n")));
        }

        let plan_review_nudge = build_plan_review_nudge(state);

        if !plan_review_nudge.is_empty() {
            task_sections.push(plan_review_nudge);
        }

        task_sections.push(if current_task.review_of.is_some() {
            format!(
                "instruction: This is a REVIEW task: independently verify the work claimed by {} using your own tools — do not take its summary on faith. Call finish_task completed to confirm the work, or finish_task blocked with exactly what is wrong to send {} back for rework.",
                current_task.review_of.as_deref().unwrap(),
                current_task.review_of.as_deref().unwrap()
            )
        } else {
            "instruction: Complete the current task now, then call finish_task. If you cannot finish it this loop, save what you learned with observe or remember, or call finish_task status blocked. If what is missing can only come from the operator (original data, credentials, a decision), block with blockedOn: \"operator\" and say exactly what is needed — do not keep searching for it.".to_string()
        });
        for task in &state.tasks {
            if let Some(recovery_line) = format_task_recovery_line(task) {
                task_sections.push(recovery_line);
            }
        }

        sections.push(task_sections.join("\n"));
    } else if state.tasks.is_empty() {
        sections.push("instruction: No tasks exist yet. Break the goal into small, concrete tasks and call plan_tasks.".to_string());
    } else if state.tasks.iter().any(|task| task.status == crate::core::types::HarnessTaskStatus::Blocked) {
        sections.push(
            "instruction: No pending tasks remain but blocked tasks exist. Resolve them BY ID: finish_task {taskId, status: completed, summary} when other work (or your own check now) already satisfied one — cite the evidence in the summary; drop_task {taskId, reason} for ones no longer needed; plan_tasks only for genuinely new unblocking work. A task blocked as unverified whose verification has since passed should be completed by id with that result, not replanned. If a task waits on something only the operator can supply, re-block it with blockedOn: \"operator\" stating what is needed and stop — the run ends awaiting that input with the finished work intact; do not plan more search or workaround tasks for it.".to_string(),
        );
    } else {
        sections.push("instruction: Every task was dropped without completing any work. Call plan_tasks with tasks that actually accomplish the goal.".to_string());
    }

    sections.join("\n\n")
}

/// Bounded recovery-evidence line for the replanner: the most recent recorded
/// outcome for this task (blocked/dropped/exhausted/reopened/operator reply)
/// plus the total event count. It names what happened — it is not proof of
/// which code or tools were tried — so the planner resolves existing task ids
/// with changed evidence instead of re-deriving identical work. None when the
/// task has no recovery history (e.g. fresh planning runs).
fn format_task_recovery_line(task: &HarnessTask) -> Option<String> {
    let history = task.recovery_history.as_ref()?;
    let last = history.last()?;

    let action = match last.action {
        crate::core::types::HarnessRecoveryAction::Blocked => "blocked",
        crate::core::types::HarnessRecoveryAction::Dropped => "dropped",
        crate::core::types::HarnessRecoveryAction::Exhausted => "retries exhausted",
        crate::core::types::HarnessRecoveryAction::Reopened => "reopened for a retry that then ran",
        crate::core::types::HarnessRecoveryAction::OperatorReply => "operator replied",
    };

    let mut line = format!(
        "recovery evidence: task \"{}\" was last {} at iteration {}",
        last.task_title.as_deref().unwrap_or(&task.title),
        action,
        last.at_iteration
    );

    if let Some(detail) = last.detail.as_deref() {
        line.push_str(": ");
        line.push_str(detail);
    }

    if history.len() > 1 {
        line.push_str(&format!(" ({} recovery events recorded)", history.len()));
    }

    Some(line)
}

pub fn build_iteration_messages(state: &HarnessState, args: &IterationMessagesArgs<'_>) -> Vec<TransportRequestMessage> {
    let base_user_text = build_iteration_user_message(
        state,
        &IterationUserMessageArgs {
            current_date: args.current_date,
            current_task: args.current_task,
            loop_info: args.loop_info.clone(),
            repo_memory_dir: args.repo_memory_dir,
            repo_memory_index: args.repo_memory_index,
            run_budget: args.run_budget.clone(),
            stall_limit: args.stall_limit,
            workspace: args.workspace,
        },
    );
    // Goal context (e.g. @mentioned files) rides along each activation instead of
    // living in state.goal, so archived goal history lines stay one line long.
    let goal_context = args.goal_context.map(str::trim).unwrap_or("");
    let user_text = if goal_context.is_empty() {
        base_user_text
    } else {
        format!("{base_user_text}\n\n{goal_context}")
    };

    vec![
        TransportRequestMessage {
            content: Some(TransportContent::Text(args.system_prompt.to_string())),
            role: ChatRoleTag::System,
            ..Default::default()
        },
        TransportRequestMessage {
            // Goal images ride along on every activation: activations share no message
            // history, so an image the goal refers to must be re-presented each time.
            content: Some(build_multimodal_user_content(
                &user_text,
                args.goal_images.as_deref().unwrap_or(&[]),
            )),
            role: ChatRoleTag::User,
            ..Default::default()
        },
    ]
}

// The user message injected at each cycle boundary inside a task loop. The
// transcript above it carries the working context, so this stays small: where
// the loop stands, what still has to be persisted, and the current task.
pub fn build_cycle_continuation_message(state: &HarnessState, args: &CycleContinuationArgs<'_>) -> String {
    let mut sections: Vec<String> = vec![format!(
        "cycle: {} of up to {} in this task loop — the transcript above is this loop's earlier work; older tool results may have been folded to digests.",
        args.cycle, args.max_cycles
    )];

    if let Some(run_budget) = &args.run_budget {
        sections.push(build_run_budget_line(run_budget));
    }

    if let Some(current_task) = args.current_task {
        sections.push(format!("current_task: {}: {}", current_task.id, current_task.title));
    }

    // One line, not the full block: the continuation shares the loop's
    // transcript, so it only needs the verdict kept in sight.
    if let Some(last_verification) = &state.last_verification {
        if last_verification.failed {
            sections.push(format!(
                "reminder: {} FAILED at cycle {} — completed claims need a newer passing run.",
                last_verification.command, last_verification.at_iteration
            ));
        }
    }

    sections.push(if args.cycle >= args.max_cycles {
        "instruction: This is the FINAL cycle of this loop — the transcript is discarded when it ends. Persist everything worth keeping NOW: finish_task if the task is done or blocked, note_task for where you left off, observe/remember for findings, plan_tasks for follow-up work."
            .to_string()
    } else if args.current_task.is_some() {
        "instruction: Continue the current task from where the transcript leaves off. Call finish_task when it is done; persist partial findings with observe or remember.".to_string()
    } else {
        "instruction: Continue planning. Call plan_tasks with small, concrete tasks for the goal.".to_string()
    });

    sections.join("\n\n")
}

fn run_reason_description(reason: HarnessRunReason) -> &'static str {
    match reason {
        HarnessRunReason::AwaitingInput => "the run is paused while the operator answers the pending clarification questions — answer them (drip --answer) and resume the session",
        HarnessRunReason::Aborted => "the run was stopped before the goal completed",
        HarnessRunReason::Completed => "every task completed",
        HarnessRunReason::Draft => "every task completed in --lite draft mode — the work is a draft; resume with the printed harden command to review and harden it",
        HarnessRunReason::Error => "the run failed on an infrastructure or endpoint error — the state is persisted and the goal can be resumed",
        HarnessRunReason::Futile => "the run kept stalling with no completed work between recovery escalations — the approach or the goal itself needs to change before resuming",
        HarnessRunReason::MaxIterations => "the cycle budget ran out before the goal completed",
        HarnessRunReason::MaxLoops => "the task-loop budget ran out before the goal completed",
        HarnessRunReason::BlockedOnInput => "nothing workable remains and at least one task is blocked on input only the operator can supply — the run stopped with the work so far intact; resume the session with that input as the prompt",
        HarnessRunReason::Partial => "some tasks were dropped without completing — the goal was only partially accomplished",
        HarnessRunReason::Planned => "plan-only mode stopped after task decomposition — resume the session to execute the plan",
        HarnessRunReason::Unreconciled => "the goal finished but at least one expectation was left unreconciled — the state is persisted and the recorded anomalies should be reviewed before trusting the results",
    }
}

fn reason_wire_tag(reason: HarnessRunReason) -> &'static str {
    match reason {
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
    }
}

pub fn build_run_summary_messages(state: &HarnessState, args: &RunSummaryMessagesArgs<'_>) -> Vec<TransportRequestMessage> {
    let mut sections: Vec<String> = vec![
        format!("current_date: {}", args.current_date),
        format!("goal: {}", state.goal),
        format!("run_ended: {} — {}", reason_wire_tag(args.reason), run_reason_description(args.reason)),
    ];

    if !state.tasks.is_empty() {
        let completed_count = state.tasks.iter().filter(|task| task.status == HarnessTaskStatus::Completed).count();
        let blocked_count = state.tasks.iter().filter(|task| task.status == HarnessTaskStatus::Blocked).count();
        let dropped_count = state.tasks.iter().filter(|task| task.status == HarnessTaskStatus::Dropped).count();

        sections.push(
            ["tasks:".to_string()]
                .into_iter()
                .chain(state.tasks.iter().map(|task| format_task_line(task, None)))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        // The summary model must not be able to contradict the ledger: the counts it
        // has to report are stated as data, not left for it to infer.
        sections.push(format!(
            "task_stats: {completed_count} completed, {blocked_count} blocked, {dropped_count} dropped of {} total",
            state.tasks.len()
        ));
    } else {
        sections.push("tasks: none were planned".to_string());
    }

    if !state.memory.is_empty() {
        sections.push(
            ["memory:".to_string()]
                .into_iter()
                .chain(state.memory.iter().map(|note| format!("- ({}) {}", note.id, note.text)))
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }

    // Follow-up goals build on earlier ones; without the session history the
    // report contradicts work (and verifications) recorded by previous goals.
    if !state.history.is_empty() {
        sections.push(build_history_section(state));
    }

    // When the budget died mid-task, the last activation digest is the only
    // record of in-progress work the ledger never captured — without it the
    // report calls that work "not done".
    if args.reason != HarnessRunReason::Completed {
        let last_activation_section = build_last_activation_section(state);

        if !last_activation_section.is_empty() {
            sections.push(last_activation_section);
        }
    }

    if let Some(last_verification) = &state.last_verification {
        sections.push(
            [
                format!(
                    "last_verification (harness-recorded — cite THIS, not memory): {} → {} (cycle {}){}",
                    last_verification.command,
                    crate::core::state::describe_verification_outcome(last_verification.failed, last_verification.ran_no_tests, last_verification.evidence.as_ref()),
                    last_verification.at_iteration,
                    crate::core::state::describe_verification_evidence(last_verification.evidence.as_ref())
                ),
                last_verification.output_tail.clone(),
            ]
            .join("\n"),
        );
    }

    if let Some(workspace_changes) = &args.workspace_changes {
        if !workspace_changes.is_empty() {
            // The ledger can lag reality in both directions (work done but never
            // finish_task'd, or claimed done without artifacts) — give the summary the
            // actual workspace delta so it reconciles instead of guessing.
            sections.push(format!("workspace_changes (ground truth at run end):\n{workspace_changes}"));
        }
    }

    // A tainted-fixture run showed flash claiming "delegated to a DELEGATE child
    // session" with no DELEGATE call in the transcript; the counts make that
    // claim checkable the same way task_stats makes the counts checkable.
    if let Some(tool_usage) = &args.tool_usage {
        sections.push(build_tool_usage_line(tool_usage));
    }

    sections.push(
        "instruction: The run has ended. Write a short message to the user summarizing the results: what was accomplished, and anything blocked or dropped and why. Report task counts exactly as given in task_stats. Ground every claim in the data above: only state that a verification (tests, build, typecheck) passed if a task summary or memory note above records its output, and never describe the output of a command that no section above records — if you would need to run something to know, say so instead. Mention a tool, a child session, or a delegation only if tool_usage counts it. Never claim that work was not started or a file does not exist unless a task summary, memory note, or workspace_changes confirms that; if the budget ran out with a task unfinished, describe it as not confirmed complete rather than not done, and mention any workspace_changes that suggest partial progress on it. If any task summary or note mentions a failed command, retry, or workaround, include a short Deviations section naming it. Plain markdown text only; no tool calls."
            .to_string(),
    );

    vec![
        TransportRequestMessage {
            content: Some(TransportContent::Text(RUN_SUMMARY_SYSTEM_PROMPT.to_string())),
            role: ChatRoleTag::System,
            ..Default::default()
        },
        TransportRequestMessage {
            content: Some(TransportContent::Text(sections.join("\n\n"))),
            role: ChatRoleTag::User,
            ..Default::default()
        },
    ]
}

pub fn build_fallback_run_summary(state: &HarnessState, reason: HarnessRunReason) -> String {
    if state.tasks.is_empty() {
        return format!(
            "Run ended ({}): no tasks were planned. {}.",
            reason_wire_tag(reason),
            run_reason_description(reason)
        );
    }

    let completed_count = state.tasks.iter().filter(|task| task.status == HarnessTaskStatus::Completed).count();
    let blocked_tasks: Vec<&HarnessTask> = state.tasks.iter().filter(|task| task.status == HarnessTaskStatus::Blocked).collect();
    let dropped_count = state.tasks.iter().filter(|task| task.status == HarnessTaskStatus::Dropped).count();
    let blocked_suffix = if !blocked_tasks.is_empty() {
        format!(", {} blocked", blocked_tasks.len())
    } else {
        String::new()
    };
    let dropped_suffix = if dropped_count > 0 { format!(", {dropped_count} dropped") } else { String::new() };
    let mut lines = vec![format!(
        "Run ended ({}): {}/{} task(s) completed{}{}.",
        reason_wire_tag(reason),
        completed_count,
        state.tasks.len(),
        blocked_suffix,
        dropped_suffix
    )];

    for task in &blocked_tasks {
        lines.push(match &task.summary {
            Some(summary) => format!("- {} blocked: {} — {}", task.id, task.title, summary),
            None => format!("- {} blocked: {}", task.id, task.title),
        });
    }

    if reason == HarnessRunReason::MaxIterations {
        lines.push("The cycle budget ran out — resume the goal to continue.".to_string());
    }

    if reason == HarnessRunReason::MaxLoops {
        lines.push("The task-loop budget ran out — resume the goal to continue.".to_string());
    }

    if reason == HarnessRunReason::BlockedOnInput {
        lines.push(
            "The run is blocked on input only the operator can supply (see the blocked tasks above) — resume the session with that input as the prompt.".to_string(),
        );
    }

    if reason == HarnessRunReason::Partial {
        lines.push(
            "Dropped tasks mean the goal was only partially accomplished — re-run the goal (or give more direction) to finish the rest.".to_string(),
        );
    }

    lines.join("\n")
}

// ---------------------------------------------------------------------------
// History / telemetry formatting helpers
// ---------------------------------------------------------------------------



pub(crate) fn format_task_telemetry(task: &HarnessTask) -> String {
    if task.status == HarnessTaskStatus::Completed || task.status == HarnessTaskStatus::Dropped {
        return String::new();
    }

    let mut signals: Vec<String> = Vec::new();

    if task.activations.unwrap_or(0) > 1 {
        signals.push(format!("{} loop(s) so far", task.activations.unwrap_or(0)));
    }

    if task.stall_count > 0 {
        signals.push(format!("stalled x{}", task.stall_count));
    }

    if signals.is_empty() {
        String::new()
    } else {
        format!(" ({})", signals.join(", "))
    }
}

pub(crate) fn format_task_line(task: &HarnessTask, current_task_id: Option<&str>) -> String {
    let status_mark = if task.status == HarnessTaskStatus::Completed {
        "[x]"
    } else if task.status == HarnessTaskStatus::Blocked {
        "[!]"
    } else if task.status == HarnessTaskStatus::Dropped {
        "[-]"
    } else if current_task_id == Some(task.id.as_str()) {
        "[>]"
    } else {
        "[ ]"
    };
    let summary_suffix = task
        .summary
        .as_deref()
        .map(|summary| format!(" — {}", summary))
        .unwrap_or_default();
    let role_suffix = task
        .role
        .as_deref()
        .map(|role| format!(" [role: {}]", role))
        .unwrap_or_default();
    let review_suffix = task
        .review_of
        .as_deref()
        .map(|review_of| format!(" [reviews {}]", review_of))
        .unwrap_or_default();
    let depends_suffix = match &task.depends_on {
        Some(depends_on) if !depends_on.is_empty() => format!(" [after {}]", depends_on.join(", ")),
        _ => String::new(),
    };

    let telemetry_suffix = format_task_telemetry(task);

    format!(
        "{} {}: {}{}{}{}{}{}",
        status_mark, task.id, task.title, summary_suffix, role_suffix, review_suffix, depends_suffix,
        telemetry_suffix
    )
}

pub(crate) fn format_history_outcome(tasks: &[HarnessTask]) -> String {
    let completed_count = tasks
        .iter()
        .filter(|task| task.status == HarnessTaskStatus::Completed)
        .count();
    let blocked_count = tasks
        .iter()
        .filter(|task| task.status == HarnessTaskStatus::Blocked)
        .count();
    let dropped_count = tasks
        .iter()
        .filter(|task| task.status == HarnessTaskStatus::Dropped)
        .count();
    let suffixes = format!(
        "{}{}",
        if blocked_count > 0 {
            format!(", {} blocked", blocked_count)
        } else {
            String::new()
        },
        if dropped_count > 0 {
            format!(", {} dropped", dropped_count)
        } else {
            String::new()
        }
    );

    format!("{}/{} tasks completed{}", completed_count, tasks.len(), suffixes)
}

pub(crate) fn build_history_section(state: &HarnessState) -> String {
    let record_start = state.history.len().saturating_sub(MAX_HISTORY_GOALS);
    let recent_records = &state.history[record_start..];
    let mut lines: Vec<String> = vec!["history (earlier goals this session, oldest first):".to_string()];

    for (record_index, record) in recent_records.iter().enumerate() {
        lines.push(format!(
            "- \"{}\" — {}",
            record.goal,
            format_history_outcome(&record.tasks)
        ));

        // Only the most recent earlier goal gets task detail, to keep the prompt bounded.
        if record_index + 1 == recent_records.len() {
            let task_start = record.tasks.len().saturating_sub(MAX_HISTORY_TASK_LINES);
            for task in &record.tasks[task_start..] {
                lines.push(format!("  {}", format_task_line(task, None)));
            }

            if let Some(summary) = record.summary.as_deref().filter(|summary| !summary.is_empty()) {
                let truncated: String = summary.chars().take(MAX_HISTORY_SUMMARY_CHARS).collect();
                lines.push(format!("  run summary: {}", truncated));
            }
        }
    }

    lines.join("\n")
}

pub(crate) fn build_run_budget_line(budget: &HarnessRunBudget) -> String {
    let remaining = budget.total - budget.used;
    let warning = if remaining <= 2 {
        " — the budget is nearly exhausted; prioritize finish_task and prune the todo list over starting new work"
    } else {
        ""
    };

    format!("run_budget: cycle {} of {} for this run{}", budget.used, budget.total, warning)
}

pub(crate) fn build_last_activation_section(state: &HarnessState) -> String {
    let digest = match &state.last_activation {
        Some(digest) => digest,
        None => return String::new(),
    };

    let cycles_suffix = if digest.cycles.unwrap_or(0) > 1 {
        format!(" over {} cycles", digest.cycles.unwrap_or(0))
    } else {
        String::new()
    };
    let task_suffix = digest
        .task_id
        .as_deref()
        .map(|task_id| format!(", working {}", task_id))
        .unwrap_or_default();
    let heading = format!("last_activation (previous loop{}{}):", cycles_suffix, task_suffix);
    let mut lines: Vec<String> = vec![heading];

    for action in &digest.actions {
        lines.push(format!("- {}", action));
    }
    lines.push(format!("outcome: {}", digest.outcome));

    lines.join("\n")
}

pub(crate) fn build_plan_review_nudge(state: &HarnessState) -> String {
    let just_finished_tasks: Vec<&HarnessTask> = state
        .tasks
        .iter()
        .filter(|task| {
            (task.status == HarnessTaskStatus::Completed || task.status == HarnessTaskStatus::Blocked)
                && task.finished_at_iteration == Some(state.iteration - 1)
        })
        .collect();

    if just_finished_tasks.is_empty() {
        return String::new();
    }

    let task_ids = just_finished_tasks
        .iter()
        .map(|task| task.id.as_str())
        .collect::<Vec<_>>()
        .join(", ");

    format!(
        "plan_review: {} just finished. Before working the current task, reconcile the todo list with what you learned: drop_task tasks made unnecessary, revise_task titles that no longer match reality, plan_tasks (placement \"next\") for newly discovered prerequisite work. Skip this if the list is still accurate.",
        task_ids
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn default_system_prompt_favors_patch_over_shell_edits() {
        assert!(DEFAULT_HARNESS_SYSTEM_PROMPT.contains("Make file edits with PATCH rather than shell in-place editing (sed -i, or inline scripts that rewrite files)"));
        assert!(DEFAULT_HARNESS_SYSTEM_PROMPT.contains("PATCH validates the edit, journals an undo entry, and reports an honest per-edit result; shell edits bypass all three"));
    }

    #[test]
    fn format_task_line_pending_task_with_no_telemetry() {
        let task = serde_json::from_value::<HarnessTask>(json!({
            "createdAtIteration": 1,
            "stallCount": 0,
            "notes": [],
            "status": "pending",
            "title": "Add helper fns",
            "id": "task-2",
            "activations": 0,
            "finishedAtIteration": null
        }))
        .unwrap();

        assert_eq!(format_task_line(&task, None), "[ ] task-2: Add helper fns");
    }

    #[test]
    fn format_task_line_in_progress_current_task_with_telemetry() {
        let task = serde_json::from_value::<HarnessTask>(json!({
            "createdAtIteration": 2,
            "stallCount": 1,
            "notes": [],
            "status": "in_progress",
            "title": "Write docs",
            "id": "task-3",
            "activations": 3,
            "finishedAtIteration": null
        }))
        .unwrap();

        assert_eq!(
            format_task_line(&task, Some("task-3")),
            "[>] task-3: Write docs (3 loop(s) so far, stalled x1)"
        );
    }

    #[test]
    fn build_history_section_renders_recent_goals() {
        let state = serde_json::from_value::<HarnessState>(json!({
            "createdAt": "2026-01-01T00:00:00.000Z",
            "goal": "port the helpers",
            "history": [
                {
                    "archivedAtIteration": 3,
                    "goal": "older goal",
                    "summary": null,
                    "tasks": [
                        {
                            "createdAtIteration": 1,
                            "stallCount": 0,
                            "notes": [],
                            "status": "completed",
                            "title": "Old task",
                            "id": "task-1",
                            "activations": 1,
                            "finishedAtIteration": 2
                        }
                    ]
                },
                {
                    "archivedAtIteration": 9,
                    "goal": "newer goal",
                    "summary": "all green",
                    "tasks": [
                        {
                            "createdAtIteration": 4,
                            "stallCount": 0,
                            "notes": [],
                            "status": "pending",
                            "title": "New task",
                            "id": "task-2",
                            "activations": 0,
                            "finishedAtIteration": null
                        }
                    ]
                }
            ],
            "iteration": 5,
            "loop": 1,
            "memory": [],
            "observations": [],
            "promotedContext": [],
            "tasks": [],
            "telemetry": {},
            "version": 1
        }))
        .unwrap();

        assert_eq!(
            build_history_section(&state),
            "history (earlier goals this session, oldest first):\n\
             - \"older goal\" — 1/1 tasks completed\n\
             - \"newer goal\" — 0/1 tasks completed\n\
             \x20 [ ] task-2: New task\n\
             \x20 run summary: all green"
        );
    }

    fn recovery_task(history: serde_json::Value) -> HarnessTask {
        serde_json::from_value(json!({
            "createdAtIteration": 1,
            "stallCount": 0,
            "notes": [],
            "status": "blocked",
            "title": "Flaky step",
            "id": "task-1",
            "activations": 0,
            "finishedAtIteration": null,
            "recoveryHistory": history
        }))
        .unwrap()
    }

    #[test]
    fn recovery_line_absent_without_history() {
        let task = serde_json::from_value::<HarnessTask>(json!({
            "createdAtIteration": 1,
            "stallCount": 0,
            "notes": [],
            "status": "pending",
            "title": "Fresh work",
            "id": "task-1",
            "activations": 0,
            "finishedAtIteration": null
        }))
        .unwrap();

        assert!(format_task_recovery_line(&task).is_none());
    }

    #[test]
    fn recovery_line_reports_blocked_failure_with_detail() {
        let task = recovery_task(json!([{
            "action": "blocked",
            "taskTitle": "Flaky step",
            "detail": "tests failed the same way",
            "atIteration": 7
        }]));

        assert_eq!(
            format_task_recovery_line(&task).as_deref(),
            Some("recovery evidence: task \"Flaky step\" was last blocked at iteration 7: tests failed the same way")
        );
    }

    #[test]
    fn recovery_line_covers_dropped_tasks_with_event_count() {
        let task = recovery_task(json!([
            {"action": "blocked", "taskTitle": "Flaky step", "detail": "fail a", "atIteration": 3},
            {"action": "reopened", "taskTitle": "Flaky step", "atIteration": 4},
            {"action": "dropped", "taskTitle": "Flaky step", "detail": "no longer needed", "atIteration": 5}
        ]));
        let line = format_task_recovery_line(&task).unwrap();

        assert!(line.contains("was last dropped"), "line: {line}");
        assert!(line.contains("(3 recovery events recorded)"), "line: {line}");
    }

    #[test]
    fn recovery_line_labels_exhausted_and_operator_reply() {
        let exhausted = recovery_task(json!([
            {"action": "exhausted", "taskTitle": "Flaky step", "atIteration": 9}
        ]));
        assert!(
            format_task_recovery_line(&exhausted)
                .unwrap()
                .contains("was last retries exhausted")
        );

        let replied = recovery_task(json!([
            {"action": "operator-reply", "taskTitle": "Flaky step", "detail": "originals are in /backup", "atIteration": 11}
        ]));
        assert!(
            format_task_recovery_line(&replied)
                .unwrap()
                .contains("was last operator replied")
        );
    }

    #[test]
    fn planner_prompt_carries_recovery_evidence_only_when_it_exists() {
        let mut state = HarnessState::default();
        state.tasks = serde_json::from_value(json!([
                {
                    "createdAtIteration": 1,
                    "stallCount": 0,
                    "notes": [],
                    "status": "blocked",
                    "title": "Flaky step",
                    "id": "task-1",
                    "activations": 2,
                    "finishedAtIteration": null,
                    "recoveryHistory": [
                        {"action": "blocked", "taskTitle": "Flaky step", "detail": "tests failed the same way", "atIteration": 5},
                        {"action": "reopened", "taskTitle": "Flaky step", "atIteration": 6}
                    ]
                },
                {
                    "createdAtIteration": 1,
                    "stallCount": 0,
                    "notes": [],
                    "status": "pending",
                    "title": "Fresh work",
                    "id": "task-2",
                    "activations": 0,
                    "finishedAtIteration": null
                }
        ]))
        .unwrap();

        let prompt = build_iteration_user_message(
            &state,
            &IterationUserMessageArgs {
                current_task: Some(&state.tasks[0]),
                ..Default::default()
            },
        );

        assert!(
            prompt.contains("recovery evidence: task \"Flaky step\" was last reopened"),
            "planner prompt must carry the task's recovery evidence"
        );
        assert!(
            prompt.contains("(2 recovery events recorded)"),
            "the count of recorded outcomes is surfaced to the planner"
        );
        assert!(
            !prompt.contains("tests failed the same way"),
            "only the most recent outcome is rendered, not a full history dump"
        );
        assert_eq!(
            prompt.matches("recovery evidence:").count(),
            1,
            "tasks without history must not produce recovery lines"
        );
    }

    #[test]
    fn recovery_line_stays_bounded_with_full_history() {
        let long_detail = "x".repeat(200);
        let events: Vec<serde_json::Value> = (1..=8)
            .map(|i| {
                json!({
                    "action": "blocked",
                    "taskTitle": "Bounded",
                    "detail": long_detail.clone(),
                    "atIteration": i
                })
            })
            .collect();
        let task = recovery_task(json!(events));
        let line = format_task_recovery_line(&task).unwrap();

        assert!(line.contains("(8 recovery events recorded)"), "line: {line}");
        assert!(line.len() < 350, "line should stay bounded, got {}", line.len());
    }
}
