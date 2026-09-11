use std::path::Path;

use crate::cli::follow::read_inbox_messages;
use crate::cli::run_record::{build_run_record, save_run_record, BuildRunRecordArgs, RunRecord};
use crate::cli::runner::{run_cli_goal, CliGoalRunArgs};
use crate::cli::skills::LoadedCliSkill;
use crate::cli::transcript::{
    append_transcript_entry, TranscriptEntry, TranscriptEventEntry, TranscriptGoalEntry, TranscriptModelEntry,
    TranscriptModelRoleRoute, TranscriptRunEndEntry,
};
use crate::core::home::DripProject;
use crate::core::inference::ResolvedInferenceConfig;
use crate::core::lease::{check_lease, LeaseStatus};
use crate::core::sessions::{
    list_recent_project_memories, session_paths_for, sync_session_memories, touch_session, SessionIndex,
    SessionRecord,
};
use crate::core::types::{HarnessEvent, HarnessRunReason, HarnessRunResult};
use crate::harness::harness_tools::RepoMemoryConfig;
use crate::harness::model_call::AbortSignal;
use crate::harness::r#loop::EmitFn;
use crate::harness::roles::{HarnessRoleBindings, HarnessRoleRuntime};
use crate::tools::types::{ChatToolDefinition, ChatToolRuntimeServices};

/// Another process currently holds the session lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveRunError {
    pub pid: i64,
    pub session_id: String,
}

impl LiveRunError {
    pub fn message(&self) -> String {
        format!(
            "A goal is already running in session {} (pid {}). Steer it with --send, watch it with --follow, or stop it with --stop.",
            self.session_id.chars().take(8).collect::<String>(),
            self.pid
        )
    }
}

impl std::fmt::Display for LiveRunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

/// What [`run_session_goal`] can fail with: a live lease, or the harness
/// run itself (an infrastructure error message).
/// the harness run itself (an infrastructure error message).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionGoalError {
    LiveRun(LiveRunError),
    Run(String),
}

impl std::fmt::Display for SessionGoalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionGoalError::LiveRun(error) => f.write_str(&error.message()),
            SessionGoalError::Run(message) => f.write_str(message),
        }
    }
}

/// Inputs to a session run: the session directory, the goal, and the model
/// route override.
pub struct SessionGoalArgs<'a> {
    /// Opt-in `--ask` clarification surveys (the ask_user tool).
    pub ask_user_enabled: bool,
    /// `--ask-timeout <seconds>` override for the survey answer wait.
    pub ask_user_timeout_seconds: Option<i64>,
    pub cwd: String,
    pub goal: String,
    pub goal_context: Option<String>,
    pub goal_images: Option<Vec<String>>,
    pub hooks: crate::harness::hooks::HooksConfig,
    pub index: &'a SessionIndex,
    pub inference: ResolvedInferenceConfig,
    pub max_iterations: Option<i64>,
    pub max_loops: Option<i64>,
    /// Run-level MCP gate (`--mcp` / `--no-mcp`), threaded into the harness options.
    pub mcp_servers: Option<Vec<String>>,
    /// `@name` mentions extracted from the goal, recorded on the transcript goal entry.
    pub mentions: Option<Vec<String>>,
    /// Archive an unfinished ledger and replan instead of continuing it.
    pub new_goal: bool,
    /// Skip the repo memory bank for this run.
    pub no_repo_memory: bool,
    pub on_event: EmitFn,
    /// Dry-run: plan, then stop.
    pub plan_only: bool,
    /// Credentials the harness scrubs from tool output.
    pub redact_secrets: Vec<(String, String)>,
    /// Per-attempt model request cap.
    pub request_timeout_ms: Option<u64>,
    /// Task titles a fresh session starts with (see CliGoalRunArgs.seed_tasks).
    pub seed_tasks: Option<Vec<String>>,
    pub project: &'a DripProject,
    pub role_bindings: Option<HarnessRoleBindings>,
    pub roles: Option<Vec<HarnessRoleRuntime>>,
    pub session: &'a SessionRecord,
    pub signal: Option<AbortSignal>,
    pub skills: Vec<LoadedCliSkill>,
    pub summarize_run: Option<bool>,
    /// Draft mode (--lite): terminal reason "draft" and no run summary.
    pub lite: bool,
    /// Operator review/verify opt-out (implied by lite): no reviewer chain,
    /// no completion-anchor gate.
    pub no_review: bool,
    pub tools: Vec<ChatToolDefinition>,
    pub tool_services: Option<ChatToolRuntimeServices>,
}

/// How a session run ended: with a record, or with an error.
pub struct SessionGoalOutcome {
    pub goal_id: String,
    /// Steering that arrived too late for this run; the session's next run consumes it.
    pub pending_operator_messages: i64,
    /// What --result / --wait replay.
    pub record: RunRecord,
    pub result: HarnessRunResult,
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub fn current_pid() -> i64 {
    std::process::id() as i64
}

/// Runs one goal against a session directory and returns how it ended.
pub async fn run_session_goal(args: SessionGoalArgs<'_>) -> Result<SessionGoalOutcome, SessionGoalError> {
    let paths = session_paths_for(args.project, args.session);

    // A second concurrent run against one session would interleave
    // last-write-wins state saves; refuse while another process's lease lives.
    let lease_status = check_lease(Path::new(&paths.lease_path), &chrono::Utc::now);

    if let LeaseStatus::Alive { lease } = &lease_status {
        if lease.pid as i64 != current_pid() {
            return Err(SessionGoalError::LiveRun(LiveRunError {
                pid: lease.pid as i64,
                session_id: args.session.id.clone(),
            }));
        }
    }

    let goal_id = uuid::Uuid::new_v4().to_string();

    // What earlier sessions in this project learned rides into the prompt as
    // context: the memory mirror was written for exactly this and never read.
    let prior_notes = list_recent_project_memories(args.index, &args.session.id, None);
    let prior_notes_block = if prior_notes.is_empty() {
        None
    } else {
        let mut lines = vec![
            "notes_from_recent_sessions (earlier drip sessions in this project recorded these; verify against the current code before relying on them):".to_string(),
        ];
        lines.extend(prior_notes.iter().map(|note| format!("- {note}")));
        Some(lines.join("\n"))
    };

    // Record every resolved route before the goal so the transcript says which
    // models actually served the run: the base (coding) model, the tool-calling
    // route when a distinct one is configured, and each activated role — the run
    // is the only place a role's binding and its model appear together. Profiles
    // can be re-pointed between runs, so nothing else preserves the pairing.
    let role_routes: Vec<TranscriptModelRoleRoute> = args
        .roles
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|role| {
            // A role can be bound to more than one loop kind (the research preset binds
            // the researcher role to both planning and task). The single `binding`
            // ternary this replaced stamped only one of them and silently dropped the
            // rest, so every binding is now recorded; `binding` stays populated (first
            // match) because older transcript readers key off it.
            let bindings: Vec<String> = ["planning", "task"]
                .iter()
                .filter(|kind| {
                    let bound = match **kind {
                        "planning" => args.role_bindings.as_ref().and_then(|b| b.planning.as_deref()),
                        _ => args.role_bindings.as_ref().and_then(|b| b.task.as_deref()),
                    };
                    bound == Some(role.name.as_str())
                })
                .map(|kind| kind.to_string())
                .collect();

            TranscriptModelRoleRoute {
                binding: bindings.first().cloned(),
                bindings: if bindings.is_empty() { None } else { Some(bindings) },
                model: role.route.as_ref().map(|route| route.model.clone()),
                name: role.name.clone(),
                provider: role.route.as_ref().and_then(|route| route.provider.clone()),
                reasoning_effort: role.route.as_ref().and_then(|route| route.reasoning_effort.clone()),
            }
        })
        .collect();

    let transcript_path = Path::new(&paths.transcript_path);
    let _ = append_transcript_entry(
        transcript_path,
        &TranscriptEntry::Model(TranscriptModelEntry {
            at: now_iso(),
            goal_id: goal_id.clone(),
            model: args.inference.model.clone(),
            profile_id: args.inference.profile_id.clone(),
            provider: args.inference.provider.clone(),
            reasoning_effort: args.inference.reasoning_effort.clone(),
            roles: if role_routes.is_empty() { None } else { Some(role_routes) },
            tool_model: args.inference.tool_route.as_ref().map(|route| route.model.clone()),
            tool_profile_id: args.inference.tool_route.as_ref().map(|route| route.profile_id.clone()),
            tool_reasoning_effort: args.inference.tool_route.as_ref().and_then(|route| route.reasoning_effort.clone()),
        }),
    );

    let _ = append_transcript_entry(
        transcript_path,
        &TranscriptEntry::Goal(TranscriptGoalEntry {
            at: now_iso(),
            goal_id: goal_id.clone(),
            images: args.goal_images.clone().unwrap_or_default(),
            mentions: args.mentions.clone().unwrap_or_default(),
            text: args.goal.clone(),
        }),
    );
    touch_session(args.index, &args.session.id, Some(&args.goal), None);

    let goal_context = [args.goal_context.clone(), prior_notes_block]
        .into_iter()
        .flatten()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");

    let transcript_path_owned = paths.transcript_path.clone();
    let event_goal_id = goal_id.clone();
    let outer_on_event = args.on_event.clone();
    let on_event: EmitFn = std::sync::Arc::new(move |event: HarnessEvent| {
        let _ = append_transcript_entry(
            Path::new(&transcript_path_owned),
            &TranscriptEntry::Event(TranscriptEventEntry {
                at: now_iso(),
                data: event.data.clone(),
                detail: event.detail.clone(),
                goal_id: event_goal_id.clone(),
                iteration: event.iteration,
                kind: event.r#type,
            }),
        );
        outer_on_event(event);
    });

    let result = run_cli_goal(CliGoalRunArgs {
        ask_user_enabled: args.ask_user_enabled,
        ask_user_timeout_seconds: args.ask_user_timeout_seconds,
        cwd: args.cwd.clone(),
        goal: args.goal.clone(),
        goal_context: if goal_context.is_empty() { None } else { Some(goal_context) },
        goal_images: args.goal_images.clone(),
        hooks: args.hooks.clone(),
        inbox_path: Some(paths.inbox_path.clone().into()),
        inference: args.inference.clone(),
        lease_path: Some(paths.lease_path.clone().into()),
        max_iterations: args.max_iterations,
        max_loops: args.max_loops,
        new_goal: args.new_goal,
        on_event,
        repo_memory: Some(RepoMemoryConfig {
            disabled: args.no_repo_memory,
            memory_dir: args.project.memory_dir.clone(),
        }),
        plan_only: args.plan_only,
        redact_secrets: args.redact_secrets.clone(),
        request_timeout_ms: args.request_timeout_ms,
        seed_tasks: args.seed_tasks.clone().filter(|tasks| !tasks.is_empty()),
        role_bindings: args.role_bindings.clone(),
        roles: args.roles.clone().filter(|roles| !roles.is_empty()),
        mcp_servers: args.mcp_servers.clone(),
        signal: args.signal.clone(),
        skills: args.skills,
        state_path: paths.state_path.clone().into(),
        summarize_run: args.summarize_run,
        lite: args.lite,
        no_review: args.no_review || args.lite,
        tools: args.tools,
        tool_services: args.tool_services.clone(),
    })
    .await
    .map_err(SessionGoalError::Run)?;

    let _ = append_transcript_entry(
        transcript_path,
        &TranscriptEntry::RunEnd(TranscriptRunEndEntry {
            at: now_iso(),
            goal_id: goal_id.clone(),
            iterations: result.iterations,
            reason: result.reason,
        }),
    );
    sync_session_memories(args.index, &args.session.id, &result.state.memory);
    touch_session(
        args.index,
        &args.session.id,
        None,
        Some(if matches!(result.reason, HarnessRunReason::Completed | HarnessRunReason::Unreconciled) { "completed" } else { "idle" }),
    );

    let pending_operator_messages = read_inbox_messages(
        Path::new(&paths.inbox_path),
        result.state.inbox_cursor.unwrap_or(0).max(0) as usize,
    )
    .len() as i64;
    let record = build_run_record(&BuildRunRecordArgs {
        ended_at: &now_iso(),
        goal: &args.goal,
        goal_id: &goal_id,
        max_iterations: args.max_iterations,
        max_loops: args.max_loops,
        pending_operator_messages,
        result: &result,
    });

    // The run's outcome must survive the process: a driver that lost this
    // process's stdout recovers the same payload via drip --result / --wait.
    let _ = save_run_record(Path::new(&paths.result_path), &record);

    Ok(SessionGoalOutcome {
        goal_id,
        pending_operator_messages,
        record,
        result,
    })
}
