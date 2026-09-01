use crate::core::types::{HarnessGoalRecord, HarnessRunReason, HarnessState, HarnessTask, HarnessTaskStatus};
use crate::harness::chat_types::ChatRoleTag;
use crate::harness::telemetry::truncate_text;
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
    " Warm context entries marked failed are calls that did not succeed when last run — do not re-run a call warm context already answers; act on the cached result instead.",
    " A folded transcript entry means that result was already seen this loop — re-run the call only if you truly need the full output again.",
    " Prefer the project's declared commands (package.json scripts, Makefile targets) over improvised equivalents.",
    " When a check fails because of the runner or environment (wrong test command, missing global, unavailable module), fix the invocation — never edit product code to accommodate a different runner (no stubbing globals or modules to make tests run).",
    " If the goal is a question or asks for a status report and needs no workspace changes, answer it from the shared state (history, memory, task summaries) with the respond op instead of planning tasks — verify with tools first only if the answer is not already recorded.",
    " If the task list is empty or exhausted, call plan_tasks to break the goal into small, concrete tasks for the other subagents.",
    " The todo list is shared and yours to keep truthful as you learn: drop_task removes tasks that are no longer needed, revise_task rewrites titles that no longer match reality, and plan_tasks with placement \"next\" inserts newly discovered prerequisite work before the remaining tasks.",
    " Prefer small tasks that one loop can finish. Do not narrate; act through tool calls."
);

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
    /// Ground-truth workspace facts gathered by the caller (e.g. git status) at run end.
    pub workspace_changes: Option<String>,
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

    sections.push(format!("current_date: {}", args.current_date));
    sections.push(format!("cycle: {}", state.iteration));
    sections.push(format!("goal: {}", state.goal));

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
        sections.push(format!(
            "task_loop: loop {}, cycle 1 of up to {} — cycles in this loop share this transcript; when the loop ends only shared state survives",
            args.loop_info.as_ref().unwrap().index,
            args.loop_info.as_ref().unwrap().max_cycles
        ));
    }

    if let Some(run_budget) = &args.run_budget {
        sections.push(build_run_budget_line(run_budget));
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
            if verification.failed { "FAILED" } else { "passed" },
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
            "pages live in ~/.lci/projects/<slug>/memory/ (open with READ when relevant); save durable repo learnings with remember scope=repo.".to_string()
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
            "instruction: Complete the current task now, then call finish_task. If you cannot finish it this loop, save what you learned with observe or remember, or call finish_task status blocked.".to_string()
        });
        sections.push(task_sections.join("\n"));
    } else if state.tasks.is_empty() {
        sections.push("instruction: No tasks exist yet. Break the goal into small, concrete tasks and call plan_tasks.".to_string());
    } else if state.tasks.iter().any(|task| task.status == crate::core::types::HarnessTaskStatus::Blocked) {
        sections.push(
            "instruction: No pending tasks remain but blocked tasks exist. Resolve them BY ID: finish_task {taskId, status: completed, summary} when other work (or your own check now) already satisfied one — cite the evidence in the summary; drop_task {taskId, reason} for ones no longer needed; plan_tasks only for genuinely new unblocking work. A task blocked as unverified whose verification has since passed should be completed by id with that result, not replanned.".to_string(),
        );
    } else {
        sections.push("instruction: Every task was dropped without completing any work. Call plan_tasks with tasks that actually accomplish the goal.".to_string());
    }

    sections.join("\n\n")
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
        HarnessRunReason::Aborted => "the run was stopped before the goal completed",
        HarnessRunReason::Completed => "every task completed",
        HarnessRunReason::Error => "the run failed on an infrastructure or endpoint error — the state is persisted and the goal can be resumed",
        HarnessRunReason::Futile => "the run kept stalling with no completed work between recovery escalations — the approach or the goal itself needs to change before resuming",
        HarnessRunReason::MaxIterations => "the cycle budget ran out before the goal completed",
        HarnessRunReason::Partial => "some tasks were dropped without completing — the goal was only partially accomplished",
        HarnessRunReason::Planned => "plan-only mode stopped after task decomposition — resume the session to execute the plan",
    }
}

fn reason_wire_tag(reason: HarnessRunReason) -> &'static str {
    match reason {
        HarnessRunReason::Aborted => "aborted",
        HarnessRunReason::Completed => "completed",
        HarnessRunReason::Error => "error",
        HarnessRunReason::Futile => "futile",
        HarnessRunReason::MaxIterations => "max-iterations",
        HarnessRunReason::Partial => "partial",
        HarnessRunReason::Planned => "planned",
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
                    "last_verification (harness-recorded — cite THIS, not memory): {} → {} (cycle {})",
                    last_verification.command,
                    if last_verification.failed { "FAILED" } else { "passed" },
                    last_verification.at_iteration
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

    sections.push(
        "instruction: The run has ended. Write a short message to the user summarizing the results: what was accomplished, and anything blocked or dropped and why. Report task counts exactly as given in task_stats. Ground every claim in the data above: only state that a verification (tests, build, typecheck) passed if a task summary or memory note above records its output, and never describe the output of a command that no section above records — if you would need to run something to know, say so instead. Never claim that work was not started or a file does not exist unless a task summary, memory note, or workspace_changes confirms that; if the budget ran out with a task unfinished, describe it as not confirmed complete rather than not done, and mention any workspace_changes that suggest partial progress on it. If any task summary or note mentions a failed command, retry, or workaround, include a short Deviations section naming it. Plain markdown text only; no tool calls."
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

    if reason == HarnessRunReason::Partial {
        lines.push(
            "Dropped tasks mean the goal was only partially accomplished — re-run the goal (or give more direction) to finish the rest.".to_string(),
        );
    }

    lines.join("\n")
}

// ---------------------------------------------------------------------------
// History / telemetry formatting helpers (ported from src/harness/prompt.ts)
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
}
