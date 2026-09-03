// The `drip` binary's dispatch. The bin (src/main.rs) only builds the runtime
// and calls `main(argv)`; keeping the body in the library lets unit tests
// drive it.
//
// Exit sites return the exit code up through `main`; plain `return` sites
// return 0.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::json;

use crate::cli::args::{parse_cli_args, ParsedCliArgs, ReviewSynthesis};
use crate::cli::review::{
    resolve_default_base_ref, run_review_command, synthesis_mode_name, ReviewCommandArgs, ReviewProgressEvent,
    DEFAULT_REVIEW_FILE_PROFILE, DEFAULT_REVIEW_SYNTH_PROFILE,
};
use crate::cli::review_report::ReviewSynthesisMode;
use crate::core::inference::ResolvedInferenceConfig;
use crate::cli::delegate_tool::{build_delegate_tool, DelegateToolWiring};
use crate::cli::follow::{format_transcript_entry_line, parse_transcript_line, read_appended_jsonl_lines};
use crate::cli::gc::{collect_gc_plan, execute_gc_plan, reap_orphan_tmux_sessions, sweep_async_job_logs};
use crate::cli::headless_output::{headless_event_line, headless_result_payload, HeadlessResultArgs};
use crate::cli::help::HELP;
use crate::cli::inspect::{build_inspect_report, format_inspect_report, InspectPaths};
use crate::cli::marketplaces::{
    add_marketplace, discover_all_skills, is_marketplace_key_enabled, list_enabled_marketplace_roles,
    list_marketplace_plugins, load_marketplaces_file, load_project_plugin_overrides, remove_marketplace,
    set_marketplace_key_enabled, update_marketplaces, AddMarketplaceArgs,
};
use crate::cli::mentions::resolve_goal_mentions;
use crate::cli::queue::{append_queued_goal, drain_queued_goals, pending_queued_goals, DrainError, QueuedGoal};
use crate::cli::roles::{resolve_role_setup, resolve_roles_flag, ResolveRoleSetupArgs};
use crate::cli::run_record::{load_run_record, RunRecord};
use crate::cli::session_run::{run_session_goal, SessionGoalArgs, SessionGoalError, SessionGoalOutcome};
use crate::cli::skills::{
    load_skill_activation, load_skill_content, resolve_effective_activation, save_skill_activation,
    LoadedCliSkill, ResolveEffectiveActivationArgs, SessionRunConfig, SkillActivationEntry,
};
use crate::cli::state_summary::{build_state_summary_json, format_state_summary};
use crate::cli::transcript::{append_transcript_entry, read_transcript, TranscriptEntry, TranscriptNoteEntry};
use crate::cli::wait::{wait_for_run_end, WaitForRunEndArgs, WaitOutcome};
use crate::core::config::{load_cli_config, resolve_cli_inference, set_active_cli_profile, set_active_cli_tool_profile, CliConfig};
use crate::core::env_vars::{list_env_var_names, load_env_vars, load_merged_env};
use crate::core::home::{ensure_drip_project, open_drip_home, resolve_drip_project, DripHome, DripProject};
use crate::core::lease::{check_lease, LeaseStatus};
use crate::core::sessions::{
    create_session, has_any_session_index, latest_any_session, list_all_sessions, open_session_index,
    resolve_any_session_ref, session_paths_for, CreateSessionArgs, ProjectPaths, SessionIndex, SessionPaths,
    SessionRecord,
};
use crate::core::state::load_harness_state;
use crate::core::types::HarnessEvent;
use crate::harness::model_call::AbortSignal;
use crate::tools::pack::{builtin_tool_pack, PLAN_MODE_TOOLS};
use crate::tools::patch_journal::{undo_last_patches, UndoOutcome};
use crate::tools::types::ChatToolDefinition;

fn stdin_is_tty() -> bool {
    // SAFETY: isatty on a fixed descriptor.
    unsafe { libc::isatty(libc::STDIN_FILENO) == 1 }
}

fn stdout_is_tty() -> bool {
    // SAFETY: isatty on a fixed descriptor.
    unsafe { libc::isatty(libc::STDOUT_FILENO) == 1 }
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// main.tsx:62 — `resolveToolsPath(requested)`. drip ships only the built-in
/// pack (drip/PLAN.md known deviation): the default "./tools" resolves to it
/// and any other path is refused by `load_tools`.
fn resolve_tools_path(requested: &str) -> String {
    requested.to_string()
}

/// JS truthiness for an optional string flag: `--prompt ""` counts as absent.
fn non_empty(value: &Option<String>) -> bool {
    value.as_deref().is_some_and(|text| !text.is_empty())
}

/// `Number.prototype.toFixed(2)`: an exact half rounds up, where Rust's `{:.2}`
/// rounds it to even (131072 bytes is 0.125 MB → "0.13", not "0.12").
fn to_fixed_2(value: f64) -> String {
    let scaled = value * 100.0;

    if scaled.fract() == 0.5 {
        format!("{:.2}", scaled.ceil() / 100.0)
    } else {
        format!("{value:.2}")
    }
}

fn load_tools(tools_path: &str, allow_net: bool) -> Result<Vec<ChatToolDefinition>, String> {
    if tools_path != "./tools" {
        // A missing pack is an error; a pack that exists is a TypeScript
        // module, which drip cannot load.
        crate::tools::loader::resolve_tools_entry_path(tools_path)?;

        return Err(format!(
            "--tools: drip only ships the built-in tool pack; \"{tools_path}\" cannot be loaded (only the built-in pack is supported)."
        ));
    }

    Ok(builtin_tool_pack(allow_net))
}

struct ResolvedSessionRef {
    error: Option<String>,
    record: Option<SessionRecord>,
}

// One resolver for every "which session did you mean" path (--resume, --state):
// an id or unique prefix when given, otherwise the latest session.
fn resolve_session_ref(project: &DripProject, reference: Option<&str>) -> ResolvedSessionRef {
    // Reads every registry for the project, including the pre-move one at
    // <repo>/.drip, so a session recorded before sessions moved into the global
    // home still resolves. The returned record carries the tree it lives in.
    if let Some(reference) = reference.filter(|reference| !reference.is_empty()) {
        return match resolve_any_session_ref(project, Some(reference)) {
            Some(record) => ResolvedSessionRef { error: None, record: Some(record) },
            None => ResolvedSessionRef {
                error: Some(format!("No session found matching \"{reference}\". Try drip --list.")),
                record: None,
            },
        };
    }

    match latest_any_session(project) {
        Some(record) => ResolvedSessionRef { error: None, record: Some(record) },
        None => ResolvedSessionRef {
            error: Some("No sessions recorded yet — start one with plain drip.".to_string()),
            record: None,
        },
    }
}

fn pick_session(index: &SessionIndex, args: &ParsedCliArgs, cwd: &str, project: &DripProject) -> ResolvedSessionRef {
    if args.resume {
        let resolved = resolve_session_ref(project, args.resume_id.as_deref());

        // Bare --resume means "latest"; say which session that turned out to be,
        // because a caller who mistyped a ref would otherwise silently target the
        // wrong session.
        if args.resume_id.is_none() {
            if let Some(record) = &resolved.record {
                eprintln!("Resuming latest session {}.", short_id(&record.id));
            }
        }

        return if resolved.record.is_some() || args.resume_id.is_some() {
            resolved
        } else {
            ResolvedSessionRef {
                error: Some(format!("No sessions recorded for {} yet — start one with plain drip.", project.root)),
                record: None,
            }
        };
    }

    if args.continue_latest {
        if let Some(latest) = latest_any_session(project) {
            return ResolvedSessionRef { error: None, record: Some(latest) };
        }

        // Falling through to a fresh session is convenient interactively but a
        // trap for scripts that believe they resumed prior work — say so.
        eprintln!("No existing session for {} — starting a new one.", project.root);
    }

    let project_paths = ProjectPaths::from(project);
    ResolvedSessionRef {
        error: None,
        record: Some(create_session(
            index,
            CreateSessionArgs {
                cwd: cwd.to_string(),
                project: &project_paths,
                now: "",
            },
        )),
    }
}

fn print_session_list(cwd: &str, json: bool, project: &DripProject) {
    let records = list_all_sessions(project, Some(30), false);

    if json {
        // Same curated shape as bare `drip --json`, so callers get paths without
        // reconstructing them and internal index columns never leak into the API.
        // Liveness comes from the lease (the status column goes stale when a run
        // crashes) and lastRun from the persisted run record — the two facts an
        // orchestrator polls for.
        let rows: Vec<serde_json::Value> = records
            .iter()
            .map(|record| {
                let paths = session_paths_for(project, record);
                let lease = check_lease(Path::new(&paths.lease_path), &chrono::Utc::now);
                let last_run = load_run_record(Path::new(&paths.result_path));
                let mut row = json!({
                    "createdAt": record.created_at,
                    "dir": paths.dir,
                    "goalCount": record.goal_count,
                    "id": record.id,
                    "lastGoal": record.last_goal,
                    "lastRun": last_run.map(|run| json!({ "endedAt": run.ended_at, "reason": run.reason, "taskStats": run.task_stats })),
                });
                if let LeaseStatus::Alive { lease } = &lease {
                    row["pid"] = json!(lease.pid);
                }
                row["running"] = json!(lease.alive());
                row["statePath"] = json!(paths.state_path);
                row["status"] = json!(record.status);
                row["transcriptPath"] = json!(paths.transcript_path);
                row["updatedAt"] = json!(record.updated_at);
                row
            })
            .collect();

        println!("{}", serde_json::to_string_pretty(&rows).unwrap_or_default());
        return;
    }

    if records.is_empty() {
        println!(
            "No sessions recorded for {} yet.",
            project.worktree_root.as_deref().unwrap_or(cwd)
        );
        return;
    }

    for record in &records {
        // The lease is the truth about "running right now"; the status column
        // alone goes stale when a run crashes.
        let live = check_lease(Path::new(&session_paths_for(project, record).lease_path), &chrono::Utc::now).alive();
        let status = if live { "running" } else { record.status.as_str() };

        println!(
            "{}  {}  {:<9}  {} goal(s)  {}",
            record.id,
            record.updated_at,
            status,
            record.goal_count,
            record.last_goal.as_deref().unwrap_or("")
        );
    }
}

// Shared by --result and --wait: print a persisted run record in the exact
// shape a live run's final line has, and hand back the run's exit code so
// `drip --wait; echo $?` reads like the run itself.
fn print_run_record(json: bool, paths: &SessionPaths, record: &RunRecord, session_id: &str) -> i32 {
    let payload = headless_result_payload(HeadlessResultArgs {
        record,
        result_path: &paths.result_path,
        session_id,
        session_id_prefix: &short_id(session_id),
        state_path: &paths.state_path,
        transcript_path: &paths.transcript_path,
    });

    if json {
        println!("{}", serde_json::to_string(&payload).unwrap_or_default());
        return payload.exit_code as i32;
    }

    let stats = payload.task_stats;

    println!(
        "run {} — {} cycle(s), {} loop(s), ended {}",
        payload.reason, payload.iterations, payload.loops, record.ended_at
    );
    println!("goal: {}", payload.goal);
    println!(
        "tasks: {} completed, {} pending, {} blocked, {} dropped",
        stats.completed, stats.pending, stats.blocked, stats.dropped
    );

    if let Some(verification) = &payload.last_verification {
        let staleness = if verification.mutations_after > 0 {
            format!("; STALE — {} edit(s) after it", verification.mutations_after)
        } else {
            String::new()
        };

        println!(
            "verification: {} ({}{staleness})",
            verification.command,
            crate::core::state::describe_verification_outcome(verification.failed, verification.ran_no_tests)
        );
    }

    if let Some(usage) = &payload.usage {
        let waits = if usage.rate_limit_wait_seconds > 0.0 {
            format!(", {}s in retry waits", usage.rate_limit_wait_seconds.round() as i64)
        } else {
            String::new()
        };

        println!(
            "usage: {} model call(s), {} prompt + {} completion tokens, {}s wall{waits}",
            usage.calls,
            usage.prompt_tokens,
            usage.completion_tokens,
            (usage.wall_ms as f64 / 1000.0).round() as i64
        );
    }

    if let Some(summary) = &payload.summary {
        println!();
        println!("{summary}");
    }

    if payload.pending_operator_messages > 0 {
        println!(
            "{} operator message(s) pending — the session's next run consumes them.",
            payload.pending_operator_messages
        );
    }

    if let Some(command) = &payload.continue_command {
        println!();
        println!("continue: {command}");
    }

    payload.exit_code as i32
}

// drip --follow: replay the transcript tail, then stream new entries as the
// session's runs append them — the terminal equivalent of watching the TUI
// timeline. Runs until interrupted.
fn follow_transcript(json: bool, transcript_path: &str) -> ! {
    let path = Path::new(transcript_path);
    let entries = read_transcript(path);
    let replayed = &entries[entries.len().saturating_sub(15)..];

    for entry in replayed {
        if json {
            println!("{}", serde_json::to_string(entry).unwrap_or_default());
        } else {
            println!("{}", format_transcript_entry_line(entry));
        }
    }

    let mut offset: u64 = std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);

    eprintln!("following {transcript_path} — ctrl+c to stop");

    loop {
        std::thread::sleep(std::time::Duration::from_millis(300));

        let appended = read_appended_jsonl_lines(path, offset);

        offset = appended.next_offset;

        for line in appended.lines {
            if json {
                println!("{line}");
                continue;
            }

            if let Some(entry) = parse_transcript_line(&line) {
                println!("{}", format_transcript_entry_line(&entry));
            }
        }
    }
}

// Bare `drip` (no goal, no --tui) initializes a session in this directory and
// prints its coordinates, so a scripted caller — or another agent harness —
// can create a session first and then drive it with --resume.
fn print_session_info(project: &DripProject, session: &SessionRecord, json: bool) {
    let paths = session_paths_for(project, session);

    if json {
        let info = json!({
            "createdAt": session.created_at,
            "dir": paths.dir,
            "goalCount": session.goal_count,
            "id": session.id,
            "lastGoal": session.last_goal,
            "statePath": paths.state_path,
            "status": session.status,
            "transcriptPath": paths.transcript_path
        });
        println!("{}", serde_json::to_string_pretty(&info).unwrap_or_default());
        return;
    }

    println!("session {}", session.id);
    println!("  dir:        {}", paths.dir);
    println!("  state:      {}", paths.state_path);
    println!("  transcript: {}", paths.transcript_path);
    println!();
    println!("Run a goal here:   drip --resume {} \"your goal\"", short_id(&session.id));
    println!("Follow the run:    tail -f {}", paths.transcript_path);
    println!("Full reference:    drip --help");
}

/// main.tsx:963-1052 — the --review branch: resolve both lanes' routes,
/// stream progress to stderr, print the report (or the JSON object) and exit
/// with the review's contractual code.
fn run_review(cli_args: &ParsedCliArgs, config: &CliConfig, home: &DripHome, project: &DripProject, cwd: &str) -> i32 {
    // Both routes are resolved here, where the config (and any --profile
    // override) is already settled, through resolve_cli_inference so each
    // child inherits the full config — url, headers, credentials, system
    // prompt, fallback profile — and the pinned model is the one that answers.
    let merged_env: std::collections::HashMap<String, String> =
        load_merged_env(Path::new(&home.env_vars_path), None).into_iter().collect();
    let resolve_route = |profile_id: &str, flag: &str| -> Result<ResolvedInferenceConfig, i32> {
        set_active_cli_profile(config.clone(), profile_id)
            .and_then(|config| set_active_cli_tool_profile(config, profile_id))
            .and_then(|config| resolve_cli_inference(&config, Some(&merged_env)))
            .map_err(|_| {
                eprintln!(
                    "{flag} names an unknown model profile \"{profile_id}\". Profiles live under \"Model Profiles\" in ~/.drip/config.json (or the web settings)."
                );
                1
            })
    };
    let file_profile_id = cli_args.review_file_profile.clone().unwrap_or_else(|| DEFAULT_REVIEW_FILE_PROFILE.to_string());
    let synth_profile_id = cli_args.review_synth_profile.clone().unwrap_or_else(|| DEFAULT_REVIEW_SYNTH_PROFILE.to_string());
    let file_inference = match resolve_route(&file_profile_id, "--file-profile") {
        Ok(inference) => inference,
        Err(code) => return code,
    };
    let synth_inference = match resolve_route(&synth_profile_id, "--synth-profile") {
        Ok(inference) => inference,
        Err(code) => return code,
    };
    let project = ensure_drip_project(project);
    let base_ref = cli_args.review_base.clone().unwrap_or_else(|| resolve_default_base_ref(cwd));
    let tools_path = resolve_tools_path(&cli_args.tools_path);
    let allow_net = cli_args.allow_net || std::env::var("DRIP_ALLOW_NET").as_deref() == Ok("1");

    if let Err(error) = load_tools(&tools_path, allow_net) {
        eprintln!("Review failed: {error}");
        return 1;
    }

    // stderr, so --json stdout stays a single parseable object.
    eprintln!(
        "review: {base_ref}...HEAD · files on {file_profile_id} ({}) · synthesis on {synth_profile_id} ({})",
        file_inference.model, synth_inference.model
    );

    let started_at = std::time::Instant::now();
    // Ctrl-C / SIGTERM stop every child (each is time-boxed against this
    // signal) instead of leaving orphaned reviewers running to their caps.
    let stop = AbortSignal::new();
    let stop_for_handler = stop.clone();

    install_stop_signals(move || stop_for_handler.abort());

    let seconds = |ms: u64| format!("{}s", (ms as f64 / 1000.0).round() as u64);
    let elapsed = move || seconds(started_at.elapsed().as_millis() as u64);
    let synth_profile_for_progress = synth_profile_id.clone();
    let outcome = tokio::task::block_in_place(|| {
        run_review_command(ReviewCommandArgs {
            base_ref,
            concurrency: cli_args.review_concurrency.map(|n| n.max(0) as usize),
            context: cli_args.review_context.clone().unwrap_or_default(),
            cwd: cwd.to_string(),
            file_inference,
            index_db_path: project.index_db_path.clone(),
            project: project.clone(),
            list_changed_files: None,
            read_diff: None,
            read_file_at_head: None,
            read_head: None,
            // Progress on stderr: a review runs for minutes with nothing on
            // the terminal otherwise, and --json stdout must stay one object.
            on_progress: Some(Arc::new(move |event: ReviewProgressEvent| match event {
                ReviewProgressEvent::Planned { units, file_count } => {
                    let labels: Vec<String> = units.iter().map(|unit| format!("{} [{} lines]", unit.label, unit.diff_lines)).collect();

                    eprintln!("review: {} unit(s) for {file_count} file(s) — {}", units.len(), labels.join(" · "));
                }
                ReviewProgressEvent::UnitDone { unit, errored, elapsed_ms, counts, retry } => {
                    let counts = format!("{} P0 · {} P1 · {} P2", counts.p0, counts.p1, counts.p2);

                    eprintln!(
                        "review: {} {}{} in {}{}",
                        if errored { "✗ errored" } else { "✓" },
                        if retry { "(retry) " } else { "" },
                        unit.label,
                        seconds(elapsed_ms),
                        if errored { String::new() } else { format!(" — {counts}") }
                    );
                }
                ReviewProgressEvent::UnitRetry { unit, reason } => {
                    eprintln!("review: ↻ retrying {} — {reason}", unit.label);
                }
                ReviewProgressEvent::SynthesisStart => {
                    eprintln!("review: synthesizing on {synth_profile_for_progress} ({} elapsed)", elapsed());
                }
                ReviewProgressEvent::SynthesisSkipped { mode } => {
                    eprintln!("review: synthesis skipped (--synthesis {}) — {} total", synthesis_mode_name(mode), elapsed());
                }
                ReviewProgressEvent::SynthesisDone { elapsed_ms, errored } => {
                    eprintln!(
                        "review: synthesis {} in {} — {} total",
                        if errored { "failed" } else { "done" },
                        seconds(elapsed_ms),
                        elapsed()
                    );
                }
            })),
            wall_clock_ms: None,
            run_goal: None,
            signal: Some(stop),
            skills: Vec::new(),
            synth_inference,
            synthesis: cli_args.review_synthesis.map(|mode| match mode {
                ReviewSynthesis::Auto => ReviewSynthesisMode::Auto,
                ReviewSynthesis::Always => ReviewSynthesisMode::Always,
                ReviewSynthesis::Never => ReviewSynthesisMode::Never,
            }),
            tools: Arc::new(move || builtin_tool_pack(allow_net)),
        })
    });

    match outcome {
        Ok(outcome) => {
            eprintln!(
                "review: done in {}s — {} unit(s), {} model call(s), {}K prompt / {}K completion tokens{}",
                (outcome.timing.total_ms as f64 / 1000.0).round() as u64,
                outcome.units.len(),
                outcome.usage.calls,
                (outcome.usage.prompt_tokens as f64 / 1000.0).round() as i64,
                (outcome.usage.completion_tokens as f64 / 1000.0).round() as i64,
                if outcome.usage.retry_wait_seconds > 0.0 {
                    format!(", {}s waiting on retries", outcome.usage.retry_wait_seconds.round() as i64)
                } else {
                    String::new()
                }
            );

            if cli_args.json {
                println!("{}", serde_json::to_string_pretty(&outcome).unwrap_or_else(|_| "null".to_string()));
            } else {
                println!("{}", outcome.report);
            }

            outcome.exit_code
        }
        Err(error) => {
            eprintln!("Review failed: {error}");
            1
        }
    }
}

/// Installs SIGINT/SIGTERM handlers that fire `on_signal` once per kind —
/// `process.once(...)` in main.tsx. Node restores the default disposition once
/// the one-shot listener is consumed, so a second Ctrl-C kills a run that is
/// stuck past its safe point; tokio keeps capturing, so the default is put
/// back by hand after the first delivery.
fn install_stop_signals(on_signal: impl Fn() + Send + Sync + 'static) {
    use tokio::signal::unix::{signal, SignalKind};

    let on_signal: Arc<dyn Fn() + Send + Sync> = Arc::new(on_signal);

    for (kind, signum) in [(SignalKind::interrupt(), libc::SIGINT), (SignalKind::terminate(), libc::SIGTERM)] {
        let on_signal = on_signal.clone();

        if let Ok(mut stream) = signal(kind) {
            tokio::spawn(async move {
                stream.recv().await;
                // SAFETY: resetting a signal disposition to SIG_DFL has no
                // preconditions; the tokio stream is dropped right after.
                unsafe {
                    libc::signal(signum, libc::SIG_DFL);
                }
                on_signal();
            });
        }
    }
}

struct HeadlessArgs<'a> {
    cli_args: &'a ParsedCliArgs,
    config: CliConfig,
    cwd: &'a str,
    goal: &'a str,
    home: &'a DripHome,
    index: &'a SessionIndex,
    project: &'a DripProject,
    session: &'a SessionRecord,
}

// Headless is the default: a goal runs with plain line output while persisting
// the session, transcript, and memory index exactly like the TUI does.
async fn run_headless(args: HeadlessArgs<'_>) -> i32 {
    let paths = session_paths_for(args.project, args.session);
    let resolved = resolve_goal_mentions(args.goal, args.cwd);

    for issue in &resolved.issues {
        eprintln!("mention: {issue}");
    }

    // fetch-tool.ts:228 reads the env var, so an operator export counts as
    // much as the flag (which entry sets into the env for children).
    let allow_net = args.cli_args.allow_net || std::env::var("DRIP_ALLOW_NET").as_deref() == Ok("1");
    let tools_path = resolve_tools_path(&args.cli_args.tools_path);
    let loaded_tools = match load_tools(&tools_path, allow_net) {
        Ok(tools) => tools,
        Err(message) => {
            eprintln!("{message}");
            return 1;
        }
    };
    // --plan explores but never mutates: the tool surface shrinks to readers.
    let plan = args.cli_args.plan;
    let base_tools_factory: Arc<dyn Fn() -> Vec<ChatToolDefinition>> = Arc::new(move || {
        let pack = builtin_tool_pack(allow_net);
        if plan {
            pack.into_iter().filter(|tool| PLAN_MODE_TOOLS.contains(&tool.name.as_str())).collect()
        } else {
            pack
        }
    });
    let merged_env: std::collections::HashMap<String, String> =
        load_merged_env(Path::new(&args.home.env_vars_path), None).into_iter().collect();

    // Skills requested for this run join the system prompt exactly like the
    // TUI's /skill toggles; an unknown explicit name is a hard error, not a
    // silent no-op. Resume-like invocations without --skill flags re-activate
    // the session's stored set (with args) so discipline survives `--resume`
    // (a stored name that has since vanished warns and is skipped instead of
    // failing the resume). An explicitly chosen model profile is pinned the
    // same way, so exec-ing a continueCommand keeps the same model.
    let stored = load_skill_activation(Path::new(&paths.activation_path));
    let effective = resolve_effective_activation(ResolveEffectiveActivationArgs {
        explicit: &args.cli_args.skill_names,
        explicit_profile: args.cli_args.profile.as_deref(),
        no_skills: args.cli_args.no_skills,
        resume_like: args.cli_args.resume || args.cli_args.continue_latest,
        stored: stored.as_ref(),
    });

    let mut config = args.config.clone();

    if let Some(profile) = &effective.profile {
        eprintln!("Re-using model profile \"{profile}\" from this session's last run (override with --profile).");
        config = match set_active_cli_profile(config, profile).and_then(|config| set_active_cli_tool_profile(config, profile)) {
            Ok(config) => config,
            Err(error) => {
                eprintln!("{error}");
                return 1;
            }
        };
    }

    let inference = match resolve_cli_inference(&config, Some(&merged_env)) {
        Ok(inference) => inference,
        Err(error) => {
            eprintln!("{error}");
            return 1;
        }
    };
    // Announce the resolved model on stderr (keeps --json stdout clean) so every
    // run makes plain which profile/model/endpoint it is about to use.
    let tool_route_note = match &inference.tool_route {
        Some(route) if route.profile_id != inference.profile_id => {
            format!(" · tools: {} ({})", route.profile_id, route.model)
        }
        _ => String::new(),
    };
    eprintln!(
        "model: {} ({}) → {}{tool_route_note}",
        inference.profile_id, inference.model, inference.url
    );

    if effective.reactivated {
        eprintln!(
            "Re-activating skills from this session's last run: {} (override with --skill <name>, or --no-skills to run bare).",
            effective.entries.iter().map(|entry| entry.name.as_str()).collect::<Vec<_>>().join(", ")
        );
    }

    let mut active_skills: Vec<LoadedCliSkill> = Vec::new();
    let mut activated_entries: Vec<SkillActivationEntry> = Vec::new();

    if !effective.entries.is_empty() {
        let pool = match discover_all_skills(Path::new(args.cwd), args.home) {
            Ok(pool) => pool,
            Err(error) => {
                eprintln!("{error}");
                return 1;
            }
        };

        for requested in &effective.entries {
            let Some(matched) = pool.iter().find(|skill| skill.name == requested.name) else {
                if effective.reactivated {
                    eprintln!("Stored skill \"{}\" no longer resolves — skipping it for this run.", requested.name);
                    continue;
                }

                let available = pool.iter().map(|skill| skill.name.as_str()).collect::<Vec<_>>().join(", ");
                eprintln!(
                    "Unknown skill \"{}\". Available: {}.",
                    requested.name,
                    if available.is_empty() { "none".to_string() } else { available }
                );
                return 1;
            };

            match load_skill_content(matched, Some(&requested.args)) {
                Ok(loaded) => active_skills.push(loaded),
                Err(error) => {
                    // Bad skill args (missing required, unknown key) are usage errors.
                    eprintln!("{error}");
                    return 1;
                }
            }

            activated_entries.push(requested.clone());
        }
    }

    let pinned_profile = args.cli_args.profile.clone().or_else(|| effective.profile.clone());

    let _ = save_skill_activation(
        Path::new(&paths.activation_path),
        &SessionRunConfig {
            profile: pinned_profile,
            skills: activated_entries,
        },
    );

    // Resolve --roles <preset-or-path> into extra role definitions that take
    // highest precedence over config/project/marketplace roles (name-keyed
    // override merge inside resolveRoleSetup).
    let mut extra_roles = None;
    let mut extra_bindings = None;

    if let Some(preset_or_path) = &args.cli_args.roles_preset_or_path {
        // A preset and a roles.json file carry the same shape (roles plus optional
        // loop-kind bindings), so both --roles sources thread through identically.
        match resolve_roles_flag(preset_or_path) {
            Ok(resolved) => {
                extra_roles = Some(resolved.roles);
                extra_bindings = resolved.bindings;
            }
            Err(error) => {
                eprintln!("--roles: could not load \"{preset_or_path}\": {error}");
                return 1;
            }
        }
    }

    let skills_pool = discover_all_skills(Path::new(args.cwd), args.home).unwrap_or_default();
    let marketplace_roles = list_enabled_marketplace_roles(Path::new(args.cwd), args.home).unwrap_or_default();
    let role_setup = resolve_role_setup(&ResolveRoleSetupArgs {
        config: &args.config,
        cwd: args.cwd.to_string(),
        env: Some(&merged_env),
        extra_bindings,
        extra_roles,
        marketplace_roles: Some(marketplace_roles),
        skills: skills_pool,
        // Validate allowlists against the FULL pack, not the plan-narrowed one:
        // --plan shrinks the surface to readers, so validating against it reported
        // every role's BASH/VERIFY/FETCH as an "unknown tool". Narrowing still
        // happens at execution time via filterToolsForRole.
        tool_names: loaded_tools
            .iter()
            .map(|tool| tool.name.clone())
            .chain(std::iter::once("DELEGATE".to_string()))
            .collect(),
    });

    for issue in &role_setup.issues {
        eprintln!("roles: {issue}");
    }

    let json = args.cli_args.json;
    let print_event: Arc<dyn Fn(HarnessEvent) + Send + Sync> = Arc::new(move |event: HarnessEvent| {
        if let Some(line) = headless_event_line(&event, json) {
            println!("{line}");
        }
    });

    // SIGTERM/SIGINT abort the run at the next safe point; the harness persists
    // state and reports reason "aborted" instead of dying mid-write.
    let controller = AbortSignal::new();
    let signal_for_handler = controller.clone();
    install_stop_signals(move || {
        eprintln!("stop requested — aborting at the next safe point");
        signal_for_handler.abort();
    });

    // Sub-delegation: the DELEGATE tool runs child goals in-process through
    // runSessionGoal (own session, inherited inference, chained abort). Plan
    // mode stays read-only, so it never delegates.
    let redact_secrets = load_env_vars(Path::new(&args.home.env_vars_path)).unwrap_or_default();
    let build_tools = || -> Vec<ChatToolDefinition> {
        let base_tools = base_tools_factory();
        if plan {
            base_tools
        } else {
            let mut tools = base_tools;
            tools.push(build_delegate_tool(DelegateToolWiring {
                cwd: args.cwd.to_string(),
                index_db_path: args.project.index_db_path.clone(),
                inference: inference.clone(),
                parent_session_id: args.session.id.clone(),
                project: args.project.clone(),
                redact_secrets: redact_secrets.clone(),
                signal: Some(controller.clone()),
                skills: active_skills.clone(),
                tool_services: None,
                tools: base_tools_factory.clone(),
            }));
            tools
        }
    };

    let mut outcome: SessionGoalOutcome = match run_session_goal(SessionGoalArgs {
        cwd: args.cwd.to_string(),
        goal: args.goal.to_string(),
        goal_context: resolved.context_block.clone(),
        goal_images: None,
        index: args.index,
        inference: inference.clone(),
        max_iterations: args.cli_args.max_iterations,
        mentions: Some(resolved.mentions.clone()),
        new_goal: args.cli_args.new_goal,
        no_repo_memory: args.cli_args.no_repo_memory,
        on_event: print_event.clone(),
        plan_only: args.cli_args.plan,
        redact_secrets: redact_secrets.clone(),
        request_timeout_ms: None,
        seed_tasks: None,
        project: args.project,
        role_bindings: role_setup.bindings.clone(),
        roles: if role_setup.roles.is_empty() { None } else { Some(role_setup.roles.clone()) },
        session: args.session,
        signal: Some(controller.clone()),
        skills: active_skills.clone(),
        summarize_run: None,
        tools: build_tools(),
        tool_services: None,
    })
    .await
    {
        Ok(outcome) => outcome,
        Err(SessionGoalError::LiveRun(error)) => {
            eprintln!("{}", error.message());
            return 1;
        }
        Err(SessionGoalError::Run(message)) => {
            // main().catch: the error message on stderr, exit 1.
            eprintln!("{message}");
            return 1;
        }
    };

    // One emitter for every goal this invocation runs (the CLI goal plus any
    // drained queue entries): builds the payload, prints the JSON line or the
    // human block, and hands back the exit code.
    let emit_outcome = |finished: &SessionGoalOutcome| -> i32 {
        let payload = headless_result_payload(HeadlessResultArgs {
            record: &finished.record,
            result_path: &paths.result_path,
            session_id: &args.session.id,
            session_id_prefix: &short_id(&args.session.id),
            state_path: &paths.state_path,
            transcript_path: &paths.transcript_path,
        });

        if json {
            println!("{}", serde_json::to_string(&payload).unwrap_or_default());
            return payload.exit_code as i32;
        }

        let finished_result = &finished.result;
        let usage = &finished_result.usage;
        let reason = serde_json::to_value(finished_result.reason)
            .ok()
            .and_then(|value| value.as_str().map(str::to_string))
            .unwrap_or_default();

        println!();
        println!(
            "Run finished ({reason}) after {} cycle(s) across {} task loop(s).",
            finished_result.iterations, finished_result.r#loops
        );
        println!("Session: {}", args.session.id);
        println!("State: {}", paths.state_path);
        println!(
            "Usage: {} model call(s), {} prompt + {} completion tokens, {}s wall{}",
            usage.calls,
            usage.prompt_tokens,
            usage.completion_tokens,
            (usage.wall_ms as f64 / 1000.0).round() as i64,
            if usage.rate_limit_wait_seconds > 0.0 {
                format!(", {}s in retry waits", usage.rate_limit_wait_seconds.round() as i64)
            } else {
                String::new()
            }
        );

        if let Some(summary) = &finished_result.state.run_summary {
            println!();
            println!("{}", summary.text);
        }

        if payload.pending_operator_messages > 0 {
            println!();
            println!(
                "{} operator message(s) arrived too late for this run — the session's next run consumes them (re-run or resume to apply).",
                payload.pending_operator_messages
            );
        }

        if let Some(command) = &payload.continue_command {
            // An unfinished goal resumes when re-submitted to the same session. The
            // session index is per-project, so the command only resolves from the same
            // directory — name it.
            println!();
            println!("Continue this run (from {}): {command}", args.cwd);
        }

        payload.exit_code as i32
    };

    let first_exit_code = emit_outcome(&outcome);

    // Drain goals queued behind this run (--enqueue): control flow lives in
    // drainQueuedGoals (tested directly); this callback owns the wiring.
    let queue_path = PathBuf::from(&paths.queue_path);
    let aborted = || controller.is_aborted();
    let mut on_refusal = |message: &str| eprintln!("{message}");
    let mut run_goal = |queued: &QueuedGoal| -> Result<i32, DrainError> {
        eprintln!(
            "Running queued goal ({} more behind it): {}",
            pending_queued_goals(&queue_path).len(),
            queued.goal.chars().take(120).collect::<String>()
        );

        // Queued goals get the same @mention resolution as the CLI goal.
        let queued_mentions = resolve_goal_mentions(&queued.goal, args.cwd);

        for issue in &queued_mentions.issues {
            eprintln!("mention: {issue}");
        }

        let result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(run_session_goal(SessionGoalArgs {
                cwd: args.cwd.to_string(),
                goal: queued.goal.clone(),
                goal_context: queued_mentions.context_block.clone(),
                goal_images: None,
                index: args.index,
                inference: inference.clone(),
                max_iterations: queued.max_iterations.or(args.cli_args.max_iterations),
                mentions: Some(queued_mentions.mentions.clone()),
                new_goal: false,
                no_repo_memory: args.cli_args.no_repo_memory,
                on_event: print_event.clone(),
                plan_only: false,
                redact_secrets: redact_secrets.clone(),
                request_timeout_ms: None,
                seed_tasks: None,
                project: args.project,
                role_bindings: role_setup.bindings.clone(),
                roles: if role_setup.roles.is_empty() { None } else { Some(role_setup.roles.clone()) },
                session: args.session,
                signal: Some(controller.clone()),
                skills: active_skills.clone(),
                summarize_run: None,
                tools: build_tools(),
                tool_services: None,
            }))
        });

        match result {
            Ok(finished) => {
                outcome = finished;
                Ok(emit_outcome(&outcome))
            }
            Err(SessionGoalError::LiveRun(error)) => Err(DrainError::LiveRunError(error.message())),
            Err(SessionGoalError::Run(message)) => Err(DrainError::Other(message)),
        }
    };

    match drain_queued_goals(&queue_path, first_exit_code, Some(&mut on_refusal), &mut run_goal, &aborted) {
        Ok(drained) => drained.exit_code,
        Err(DrainError::LiveRunError(message)) | Err(DrainError::Other(message)) => {
            eprintln!("{message}");
            1
        }
    }
}

/// main.tsx:613 — `main()`. Returns the process exit code.
pub async fn main(argv: Vec<String>) -> i32 {
    let cli_args = parse_cli_args(&argv);

    if !cli_args.errors.is_empty() {
        for problem in &cli_args.errors {
            eprintln!("{problem}");
        }

        return 1;
    }

    if cli_args.help {
        // The template ends with one newline and println! appends another,
        // so stdout ends "\n\n".
        println!("{HELP}");
        return 0;
    }

    if cli_args.version {
        // drip embeds the Cargo package version at compile time.
        let version = env!("CARGO_PKG_VERSION");

        if cli_args.json {
            println!("{{\"version\":\"{version}\"}}");
            return 0;
        }

        println!("drip {version}");
        return 0;
    }

    let cwd = std::env::current_dir()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".to_string());
    let home = open_drip_home(&cli_args.home.clone().unwrap_or_else(crate::core::home::resolve_drip_home_root));

    // Paths only — the .drip directory is not created until a command actually
    // needs session storage, so read-only invocations never mutate the cwd.
    let project_override = cli_args.project_dir.clone().or_else(|| std::env::var("DRIP_PROJECT_DIR").ok());
    let project = match resolve_drip_project(&cwd, &home.root, project_override.as_deref()) {
        Ok(project) => project,
        Err(error) => {
            eprintln!("{error}");
            return 1;
        }
    };

    // Tool subprocesses (BASH and friends) scrub these credential names from
    // their environment so model-driven commands never inherit the keys drip
    // itself runs on. Names only — the values stay in the env.vars file.
    let scrub_names = list_env_var_names(Path::new(&home.env_vars_path)).unwrap_or_default();

    if !scrub_names.is_empty() {
        std::env::set_var("DRIP_SCRUB_ENV", scrub_names.join(","));
    }

    if cli_args.undo_last {
        if non_empty(&cli_args.goal) || non_empty(&cli_args.prompt) {
            eprintln!("--undo-last only reverts edits — run goals separately.");
            return 1;
        }

        let outcomes = undo_last_patches(Path::new(&project.root), cli_args.undo_last_count.unwrap_or(1).max(0) as usize);

        if cli_args.json {
            let rows: Vec<serde_json::Value> = outcomes
                .iter()
                .map(|outcome| match outcome {
                    UndoOutcome::Undone { path } => json!({ "kind": "undone", "path": path }),
                    UndoOutcome::Deleted { path } => json!({ "kind": "deleted", "path": path }),
                    UndoOutcome::Refused { path, why } => json!({ "kind": "refused", "path": path, "why": why }),
                    UndoOutcome::Empty => json!({ "kind": "empty" }),
                })
                .collect();
            println!("{}", serde_json::to_string(&rows).unwrap_or_default());
        } else {
            for outcome in &outcomes {
                match outcome {
                    UndoOutcome::Empty => println!("Nothing to undo — the patch journal is empty."),
                    UndoOutcome::Refused { path, why } => println!("refused {path}: {why}"),
                    UndoOutcome::Deleted { path } => println!("deleted (was created by the patch) {path}"),
                    UndoOutcome::Undone { path } => println!("restored {path}"),
                }
            }
        }

        return if outcomes.iter().any(|outcome| matches!(outcome, UndoOutcome::Refused { .. })) { 1 } else { 0 };
    }

    if cli_args.allow_destructive {
        // The bash/verify tools read this to downgrade policy blocks to warnings.
        std::env::set_var("DRIP_ALLOW_DESTRUCTIVE", "1");
    }

    if cli_args.allow_net {
        // The FETCH tool refuses to run without this opt-in.
        std::env::set_var("DRIP_ALLOW_NET", "1");
    }

    // One exclusion table for every non-goal mode (debt audit S1): the ad-hoc
    // per-handler conflict lists had already drifted apart.
    let exclusive_modes: [(&str, bool); 12] = [
        ("--follow", cli_args.follow),
        ("--gc", cli_args.gc),
        ("--inspect", cli_args.inspect),
        ("--list", cli_args.list),
        ("--result", cli_args.result),
        ("--review", cli_args.review),
        ("--send", cli_args.send),
        ("--skills", cli_args.skills),
        ("--state", cli_args.state),
        ("--stop", cli_args.stop),
        ("--undo-last", cli_args.undo_last),
        ("--wait", cli_args.wait),
    ];
    let active_modes: Vec<&str> = exclusive_modes.iter().filter(|(_, active)| *active).map(|(name, _)| *name).collect();
    // JS truthiness: `--prompt ""` is no goal at all (the empty-goal error comes later).
    let has_goal_like = non_empty(&cli_args.goal) || non_empty(&cli_args.prompt) || cli_args.tui;

    if active_modes.len() > 1 {
        eprintln!("Pass one of {} — they are separate modes.", active_modes.join(", "));
        return 1;
    }

    if active_modes.len() == 1 && has_goal_like && active_modes[0] != "--send" {
        eprintln!("{} cannot be combined with a goal or --tui — run them separately.", active_modes[0]);
        return 1;
    }

    if cli_args.list {
        if !has_any_session_index(&project) {
            if cli_args.json {
                println!("[]");
                return 0;
            }

            println!("No sessions recorded for {} yet.", project.worktree_root.as_deref().unwrap_or(&cwd));
            return 0;
        }

        print_session_list(&cwd, cli_args.json, &project);
        return 0;
    }

    if cli_args.gc {
        // --gc is an inspector mode: refuse to combine with a goal or run flags.
        if has_goal_like {
            eprintln!("--gc only inspects/compacts sessions — run the goal separately.");
            return 1;
        }

        if !has_any_session_index(&project) {
            if cli_args.json {
                println!("{}", json!({ "compactedSessions": 0, "deletedBytes": 0, "sessions": [] }));
                return 0;
            }

            println!("No sessions recorded for {} yet.", project.worktree_root.as_deref().unwrap_or(&cwd));
            return 0;
        }

        let older_than_days = cli_args.older_than.unwrap_or(14);
        // Saturate rather than overflow: JS keeps an absurd day count as a
        // float and simply reaps everything.
        let older_than_ms = older_than_days.saturating_mul(24 * 60 * 60 * 1000);
        let index = open_session_index(&project.index_db_path);
        let plan = collect_gc_plan(&index, &project, older_than_ms, &chrono::Utc::now);
        let result = execute_gc_plan(&plan, cli_args.dry_run);
        // Orphaned drip- tmux sessions older than the cutoff die with the same sweep.
        let listing = std::process::Command::new("tmux")
            .args(["ls", "-F", "#{session_name} #{session_created}"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
            .unwrap_or_default();
        let reaped = reap_orphan_tmux_sessions(
            &listing,
            older_than_ms,
            cli_args.dry_run,
            chrono::Utc::now().timestamp_millis(),
            &|name: &str| {
                crate::tools::builtin::bash::kill_tmux_session(name).map_err(|error| error.to_string())
            },
        );
        let swept_logs = sweep_async_job_logs(&project, older_than_ms, cli_args.dry_run, &chrono::Utc::now);

        index.close();

        if cli_args.json {
            let sessions: Vec<serde_json::Value> = plan
                .sessions
                .iter()
                .map(|session| json!({ "bytes": session.bytes, "dir": session.dir, "id": session.id }))
                .collect();
            println!(
                "{}",
                json!({
                    "compactedSessions": result.compacted_sessions,
                    "deletedBytes": result.deleted_bytes,
                    "reapedJobs": reaped.killed,
                    "sessions": sessions,
                    "sweptJobLogs": { "deletedBytes": swept_logs.deleted_bytes, "deletedFiles": swept_logs.deleted_files }
                })
            );
            return 0;
        }

        if plan.sessions.is_empty() && reaped.killed.is_empty() {
            println!("No idle sessions older than {older_than_days} day(s) found.");
            return 0;
        }

        let mb = |bytes: u64| to_fixed_2(bytes as f64 / 1_048_576.0);

        println!(
            "GC{}: {} session(s), {} MB eligible",
            if cli_args.dry_run { " (dry run)" } else { "" },
            plan.sessions.len(),
            mb(plan.total_bytes)
        );
        println!(
            "{}: {} MB across {} session(s)",
            if cli_args.dry_run { "Would free" } else { "Freed" },
            mb(result.deleted_bytes),
            result.compacted_sessions
        );

        if !reaped.killed.is_empty() {
            println!(
                "Reaped {} orphaned background job(s): {}",
                reaped.killed.len(),
                reaped.killed.join(", ")
            );
        }

        if swept_logs.deleted_files > 0 {
            println!(
                "{} {} settled async job log(s).",
                if cli_args.dry_run { "Would sweep" } else { "Swept" },
                swept_logs.deleted_files
            );
        }

        return 0;
    }

    if cli_args.result || cli_args.wait || cli_args.inspect {
        if has_goal_like {
            eprintln!("--result/--wait/--inspect only inspect a session — run the goal separately.");
            return 1;
        }

        if !has_any_session_index(&project) {
            eprintln!("No sessions recorded for {cwd} yet.");
            return 1;
        }

        let reference = if cli_args.result {
            cli_args.result_id.as_deref()
        } else if cli_args.wait {
            cli_args.wait_id.as_deref()
        } else {
            cli_args.inspect_id.as_deref()
        };
        let resolved = resolve_session_ref(&project, reference);

        let Some(record) = resolved.record else {
            eprintln!("{}", resolved.error.unwrap_or_else(|| "No session available.".to_string()));
            return 1;
        };

        let paths = session_paths_for(&project, &record);

        if cli_args.inspect {
            let report = build_inspect_report(&InspectPaths {
                result_path: Path::new(&paths.result_path),
                state_path: Path::new(&paths.state_path),
                transcript_path: Path::new(&paths.transcript_path),
            });

            if cli_args.json {
                println!("{}", serde_json::to_string_pretty(&report).unwrap_or_default());
            } else {
                println!("{}", format_inspect_report(&report));
            }
            return 0;
        }

        if cli_args.result {
            let Some(run_record) = load_run_record(Path::new(&paths.result_path)) else {
                // Older sessions predate result.json; point at what still works.
                eprintln!(
                    "No run outcome recorded for session {} yet (runs before v0.29 did not persist one — see --state or the transcript).",
                    short_id(&record.id)
                );
                return 1;
            };

            return print_run_record(cli_args.json, &paths, &run_record, &record.id);
        }

        let outcome = wait_for_run_end(WaitForRunEndArgs {
            lease_path: Path::new(&paths.lease_path),
            result_path: Path::new(&paths.result_path),
            grace_ms: None,
            poll_ms: None,
            timeout_ms: cli_args.timeout_secs.map(|secs| (secs * 1000).max(0) as u64),
            now: None,
        });

        return match outcome {
            WaitOutcome::Result { record: run_record } => print_run_record(cli_args.json, &paths, &run_record, &record.id),
            WaitOutcome::NoRun => {
                eprintln!(
                    "No run is live and none has recorded a result for session {}.",
                    short_id(&record.id)
                );
                1
            }
            WaitOutcome::Crashed => {
                if cli_args.json {
                    println!("{}", json!({ "reason": "crashed", "sessionId": record.id, "type": "wait-crashed" }));
                } else {
                    eprintln!(
                        "The running goal in session {} died without recording a result (crash or kill -9). State on disk is whatever the last save left.",
                        short_id(&record.id)
                    );
                }
                3
            }
            WaitOutcome::Timeout => {
                if cli_args.json {
                    println!("{}", json!({ "sessionId": record.id, "timeoutSecs": cli_args.timeout_secs, "type": "wait-timeout" }));
                } else {
                    eprintln!(
                        "Still running after {}s — gave up waiting (the run continues; re-run --wait to keep waiting).",
                        cli_args.timeout_secs.map(|secs| secs.to_string()).unwrap_or_else(|| "undefined".to_string())
                    );
                }
                124
            }
        };
    }

    if cli_args.state {
        // --state is a pure inspector; combining it with anything that would start
        // work or another mode is ambiguous, so it errors instead of one side
        // silently winning (mirrors the --prompt/goal conflict below).
        if has_goal_like {
            eprintln!("--state only inspects a session — run the goal separately (and --tui has its own /state).");
            return 1;
        }

        if !has_any_session_index(&project) {
            // An explicit id that cannot exist is an error, same as an unknown id.
            if let Some(state_id) = &cli_args.state_id {
                eprintln!("No session found matching \"{state_id}\" — no sessions recorded for {cwd} yet.");
                return 1;
            }

            if cli_args.json {
                println!("null");
                return 0;
            }

            println!("No sessions recorded for {} yet.", project.worktree_root.as_deref().unwrap_or(&cwd));
            return 0;
        }

        let resolved = resolve_session_ref(&project, cli_args.state_id.as_deref());

        let Some(record) = resolved.record else {
            if cli_args.state_id.is_none() {
                // No sessions at all is an empty result, not a failure.
                if cli_args.json {
                    println!("null");
                    return 0;
                }

                println!("No sessions recorded for {} yet.", project.worktree_root.as_deref().unwrap_or(&cwd));
                return 0;
            }

            eprintln!("{}", resolved.error.unwrap_or_else(|| "No session available.".to_string()));
            return 1;
        };

        let paths = session_paths_for(&project, &record);

        // main.tsx:936-946 wraps every --state shape in one try/catch: a
        // corrupt state.json is the same error message whichever view asked.
        if let Err(error) = load_harness_state(Path::new(&paths.state_path)) {
            eprintln!("Could not read harness state at {}: {error}", paths.state_path);
            return 1;
        }

        if cli_args.json && cli_args.full {
            // The raw HarnessState dump — history, telemetry, promoted context —
            // for debugging; drivers should use the curated summary below.
            match load_harness_state(Path::new(&paths.state_path)) {
                Ok(state) => println!("{}", serde_json::to_string_pretty(&state).unwrap_or_else(|_| "null".to_string())),
                Err(error) => {
                    eprintln!("Could not read harness state at {}: {error}", paths.state_path);
                    return 1;
                }
            }
        } else if cli_args.json {
            let summary = build_state_summary_json(
                Path::new(&paths.inbox_path),
                Path::new(&paths.lease_path),
                Path::new(&paths.state_path),
            );
            println!(
                "{}",
                summary
                    .map(|map| serde_json::to_string_pretty(&map).unwrap_or_else(|_| "null".to_string()))
                    .unwrap_or_else(|| "null".to_string())
            );
        } else {
            println!("{}", format_state_summary(Path::new(&paths.state_path)));
        }

        return 0;
    }

    let mut config = match load_cli_config(Path::new(&home.config_path)) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("{error}");
            return 1;
        }
    };

    if let Some(profile) = &cli_args.profile {
        // A --profile flag applies for this invocation without rewriting the
        // config file — and it selects the model that actually does the work:
        // tool-bearing activations follow the tool route, so both routes move.
        config = match set_active_cli_profile(config, profile).and_then(|config| set_active_cli_tool_profile(config, profile)) {
            Ok(config) => config,
            Err(error) => {
                eprintln!("{error}");
                return 1;
            }
        };
    }

    if cli_args.review {
        return run_review(&cli_args, &config, &home, &project, &cwd);
    }

    // The goal comes from the positional argument or --prompt (equivalent for the
    // headless runner; --prompt exists so scripted callers avoid shell-quoting a
    // positional). Supplying both is ambiguous, so it errors instead of one
    // silently winning.
    if non_empty(&cli_args.prompt) && non_empty(&cli_args.goal) {
        eprintln!("Provide the goal either as a positional argument or via --prompt, not both.");
        return 1;
    }

    let goal_text = cli_args.prompt.clone().or_else(|| cli_args.goal.clone());

    // Headless is the default. The TUI is opt-in via --tui and needs a real
    // terminal on both ends; failing loudly beats silently degrading, because a
    // caller who asked for the TUI wants the TUI.
    if cli_args.tui && !(stdin_is_tty() && stdout_is_tty()) {
        eprintln!("--tui needs an interactive terminal (stdin and stdout must be TTYs).");
        return 1;
    }

    if cli_args.skills {
        let pool = match discover_all_skills(Path::new(&cwd), &home) {
            Ok(pool) => pool,
            Err(error) => {
                eprintln!("{error}");
                return 1;
            }
        };

        if cli_args.json {
            println!("{}", serde_json::to_string_pretty(&pool).unwrap_or_default());
            return 0;
        }

        if pool.is_empty() {
            println!(
                "No skills found. Add SKILL.md files under {}/<name>/ or ./.drip/skills/<name>/, or enable a marketplace plugin.",
                home.skills_dir
            );
            return 0;
        }

        for skill in &pool {
            let source = serde_json::to_value(&skill.source)
                .ok()
                .and_then(|value| value.as_str().map(str::to_string))
                .unwrap_or_default();
            println!("{}  [{}]  {}", skill.name, source, skill.description);
        }

        return 0;
    }

    let marketplace_command = cli_args.marketplace_list
        || cli_args.marketplace_add
        || cli_args.marketplace_remove
        || cli_args.marketplace_update
        || cli_args.plugin_enable
        || cli_args.plugin_disable;

    if marketplace_command {
        // Config commands manage the global home only: no session, no project
        // .drip, and no mixing with run/monitor modes.
        if has_goal_like || cli_args.state || cli_args.send || cli_args.follow || cli_args.stop || cli_args.list {
            eprintln!("Marketplace/plugin commands cannot be combined with goals or other modes.");
            return 1;
        }

        return match run_marketplace_command(&cli_args, &cwd, &home) {
            Ok(code) => code,
            Err(error) => {
                eprintln!("{error}");
                1
            }
        };
    }

    if cli_args.stop {
        if !has_any_session_index(&project) {
            eprintln!("No sessions recorded for {cwd} yet — nothing to stop.");
            return 1;
        }

        let resolved = resolve_session_ref(&project, cli_args.stop_id.as_deref());

        let Some(record) = resolved.record else {
            eprintln!("{}", resolved.error.unwrap_or_else(|| "No session available.".to_string()));
            return 1;
        };

        let paths = session_paths_for(&project, &record);
        let status = check_lease(Path::new(&paths.lease_path), &chrono::Utc::now);

        let lease = match status {
            LeaseStatus::Alive { lease } => lease,
            LeaseStatus::NotAlive { lease } => {
                eprintln!(
                    "No goal is running in session {}{}.",
                    short_id(&record.id),
                    if lease.is_some() { " (its last run left a stale lease)" } else { "" }
                );
                return 1;
            }
        };

        // Best-effort identity check before signalling: a recycled pid should
        // not eat a SIGTERM meant for a drip run.
        if let Ok(output) = std::process::Command::new("ps")
            .args(["-p", &lease.pid.to_string(), "-o", "command="])
            .output()
        {
            let command = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let looks_like_run = regex::Regex::new(r"(?i)drip")
                .map(|pattern| pattern.is_match(&command))
                .unwrap_or(true);

            if output.status.success() && !command.is_empty() && !looks_like_run {
                eprintln!(
                    "Refusing to stop pid {}: it does not look like a drip run ({}). The lease may be stale from a pid reuse.",
                    lease.pid,
                    command.chars().take(80).collect::<String>()
                );
                return 1;
            }
        }

        // SAFETY: kill(2) with SIGTERM on a pid we just validated.
        let killed = unsafe { libc::kill(lease.pid as libc::pid_t, libc::SIGTERM) } == 0;

        if !killed {
            eprintln!(
                "Could not signal pid {}: {}",
                lease.pid,
                std::io::Error::last_os_error()
            );
            return 1;
        }

        if cli_args.json {
            println!("{}", json!({ "pid": lease.pid, "sessionId": record.id, "type": "stopped" }));
        } else {
            println!(
                "Sent SIGTERM to the running goal (pid {}) in session {} — it aborts at the next safe point, state persisted.",
                lease.pid,
                short_id(&record.id)
            );
        }

        return 0;
    }

    if cli_args.send || cli_args.follow {
        if cli_args.send && cli_args.follow {
            eprintln!("Pass either --send or --follow, not both.");
            return 1;
        }

        if cli_args.tui || cli_args.state || cli_args.list {
            eprintln!("--send/--follow cannot be combined with --tui, --state, or --list.");
            return 1;
        }

        // Both target an existing session; neither should mint .drip state.
        if !has_any_session_index(&project) {
            eprintln!(
                "No sessions recorded for {cwd} yet — nothing to {}.",
                if cli_args.send { "send to" } else { "follow" }
            );
            return 1;
        }

        let reference = if cli_args.send { cli_args.send_id.as_deref() } else { cli_args.follow_id.as_deref() };
        let resolved = resolve_session_ref(&project, reference);

        let Some(record) = resolved.record else {
            eprintln!("{}", resolved.error.unwrap_or_else(|| "No session available.".to_string()));
            return 1;
        };

        let paths = session_paths_for(&project, &record);

        if cli_args.send {
            let Some(text) = goal_text.as_deref().filter(|text| !text.is_empty()) else {
                eprintln!("Provide the message: drip --send [id] \"message text\" (or --prompt).");
                return 1;
            };

            let line = json!({ "at": now_iso(), "text": text }).to_string();
            // A message that could not be queued is a failed --send (exit 1),
            // not a silent "Queued".
            if let Err(error) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&paths.inbox_path)
                .and_then(|mut file| std::io::Write::write_all(&mut file, format!("{line}\n").as_bytes()))
            {
                eprintln!("Could not queue the message at {}: {error}", paths.inbox_path);
                return 1;
            }
            // Queued vs consumed matter to followers: the harness emits an
            // operator-message event only when a running goal picks this up.
            let _ = append_transcript_entry(
                Path::new(&paths.transcript_path),
                &TranscriptEntry::Info(TranscriptNoteEntry {
                    at: now_iso(),
                    text: format!("operator message queued: {text}"),
                }),
            );

            // Liveness comes from the lease (pid + fresh heartbeat), not the index
            // status column — a crashed run leaves the index saying "active".
            let run_active = check_lease(Path::new(&paths.lease_path), &chrono::Utc::now).alive();

            if cli_args.json {
                println!(
                    "{}",
                    json!({ "inboxPath": paths.inbox_path, "runActive": run_active, "sessionId": record.id, "text": text, "type": "sent" })
                );
            } else if run_active {
                println!(
                    "Queued for session {} — the running goal picks it up at its next cycle boundary (if the run ends first, the next run consumes it; the result line reports pendingOperatorMessages).",
                    short_id(&record.id)
                );
            } else {
                println!(
                    "Queued for session {} — no goal is running; the session's next run consumes it.",
                    short_id(&record.id)
                );
            }

            return 0;
        }

        follow_transcript(cli_args.json, &paths.transcript_path);
    }

    let project = ensure_drip_project(&project);

    let index = open_session_index(&project.index_db_path);
    let picked = pick_session(&index, &cli_args, &cwd, &project);

    let Some(session) = picked.record else {
        eprintln!("{}", picked.error.unwrap_or_else(|| "No session available.".to_string()));
        index.close();
        return 1;
    };

    if !cli_args.tui {
        let Some(goal_text) = goal_text.filter(|text| !text.is_empty()) else {
            // Bare drip: initialize (or select, with --continue/--resume) a session and
            // print its coordinates for follow-up invocations.
            print_session_info(&project, &session, cli_args.json);
            index.close();
            return 0;
        };

        let dispatch_paths = session_paths_for(&project, &session);

        // --enqueue: a live run on the session means "run this next" instead of
        // LiveRunError. The owning process drains the queue at run end. With no
        // live run the flag is a no-op and the goal runs now.
        if cli_args.enqueue && check_lease(Path::new(&dispatch_paths.lease_path), &chrono::Utc::now).alive() {
            let position = append_queued_goal(Path::new(&dispatch_paths.queue_path), &goal_text, cli_args.max_iterations);

            if cli_args.json {
                println!("{}", json!({ "position": position, "sessionId": session.id, "type": "queued" }));
            } else {
                println!(
                    "Queued behind the running goal (position {position}) in session {} — the owning run picks it up when the current goal ends.",
                    short_id(&session.id)
                );
            }

            index.close();
            return 0;
        }

        // --detach: hand the run to a background process and print the handle.
        // The lease prevents double-runs; --wait/--result complete the lifecycle.
        if cli_args.detach {
            let run_log_path = Path::new(&dispatch_paths.dir).join("run.log");
            let log_file = match std::fs::OpenOptions::new().create(true).append(true).open(&run_log_path) {
                Ok(file) => file,
                Err(error) => {
                    eprintln!("{error}");
                    index.close();
                    return 1;
                }
            };
            let mut child_args: Vec<String> = argv.iter().filter(|arg| *arg != "--detach").cloned().collect();

            if !child_args.iter().any(|arg| arg == "--resume" || arg == "-r") {
                child_args.insert(0, session.id.clone());
                child_args.insert(0, "--resume".to_string());
            }

            let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("drip"));
            let child = std::process::Command::new(exe)
                .args(&child_args)
                .stdin(std::process::Stdio::null())
                .stdout(log_file.try_clone().map(std::process::Stdio::from).unwrap_or_else(|_| std::process::Stdio::null()))
                .stderr(std::process::Stdio::from(log_file))
                .process_group(0)
                .spawn();

            let child = match child {
                Ok(child) => child,
                Err(error) => {
                    eprintln!("{error}");
                    index.close();
                    return 1;
                }
            };

            let pid = child.id();
            let wait_command = format!("drip --wait {} --json", short_id(&session.id));
            let handle = json!({
                "logPath": run_log_path.to_string_lossy(),
                "pid": pid,
                "resultCommand": format!("drip --result {} --json", short_id(&session.id)),
                "sessionId": session.id,
                "type": "started",
                "waitCommand": wait_command
            });

            if cli_args.json {
                println!("{handle}");
            } else {
                println!(
                    "Started in the background (pid {pid}). Watch: drip --follow {} · wait: {wait_command}",
                    short_id(&session.id)
                );
            }

            index.close();
            return 0;
        }

        let exit_code = run_headless(HeadlessArgs {
            cli_args: &cli_args,
            config,
            cwd: &cwd,
            goal: &goal_text,
            home: &home,
            index: &index,
            project: &project,
            session: &session,
        })
        .await;

        index.close();
        return exit_code;
    }

    // The interactive session owns the terminal from here; it opens its own
    // index handle per goal run, so the bootstrap one is released first.
    index.close();
    let allow_net = cli_args.allow_net || std::env::var("DRIP_ALLOW_NET").as_deref() == Ok("1");
    tokio::task::block_in_place(|| {
        crate::tui::app::run_tui_app(crate::tui::app::TuiBootstrap {
            allow_net,
            config,
            cwd: cwd.clone(),
            home: home.clone(),
            initial_goal: goal_text.filter(|text| !text.is_empty()),
            max_iterations: cli_args.max_iterations,
            no_repo_memory: cli_args.no_repo_memory,
            project: project.clone(),
            session,
        })
    })
}

fn run_marketplace_command(cli_args: &ParsedCliArgs, cwd: &str, home: &DripHome) -> anyhow::Result<i32> {
    if cli_args.marketplace_add {
        let Some(source) = cli_args.marketplace_add_source.as_deref().filter(|source| !source.is_empty()) else {
            eprintln!("Usage: drip --marketplace-add <git-url-or-path> [name]");
            return Ok(1);
        };

        let added = add_marketplace(AddMarketplaceArgs {
            git: None,
            home,
            name: cli_args.marketplace_add_name.as_deref(),
            now: None,
            source,
        })?;
        let record = added.record;

        if cli_args.json {
            println!(
                "{}",
                json!({ "kind": record.kind, "name": record.name, "source": record.source, "type": "marketplace-added" })
            );
        } else {
            println!(
                "Registered marketplace \"{}\" ({}) from {}. Enable its plugins with drip --plugin-enable <{}/plugin>.",
                record.name, record.kind, record.source, record.name
            );
        }

        return Ok(0);
    }

    if cli_args.marketplace_remove {
        let Some(name) = cli_args.marketplace_remove_name.as_deref().filter(|name| !name.is_empty()) else {
            eprintln!("Usage: drip --marketplace-remove <name>");
            return Ok(1);
        };

        remove_marketplace(home, name)?;

        if cli_args.json {
            println!("{}", json!({ "name": name, "type": "marketplace-removed" }));
        } else {
            println!("Removed marketplace \"{name}\".");
        }

        return Ok(0);
    }

    if cli_args.marketplace_update {
        let updated = update_marketplaces(
            None,
            home,
            cli_args.marketplace_update_name.as_deref().filter(|name| !name.is_empty()),
        )?;

        if cli_args.json {
            println!("{}", json!({ "type": "marketplaces-updated", "updated": updated }));
        } else if updated.is_empty() {
            println!("Nothing to update.");
        } else {
            println!("Updated: {}.", updated.join(", "));
        }

        return Ok(0);
    }

    if cli_args.plugin_enable || cli_args.plugin_disable {
        let key = if cli_args.plugin_enable {
            cli_args.plugin_enable_key.as_deref()
        } else {
            cli_args.plugin_disable_key.as_deref()
        };

        let Some(key) = key.filter(|key| !key.is_empty() && key.contains('/')) else {
            eprintln!(
                "Usage: drip --plugin-{} <marketplace/plugin[/skill]>",
                if cli_args.plugin_enable { "enable" } else { "disable" }
            );
            return Ok(1);
        };

        set_marketplace_key_enabled(home, key, cli_args.plugin_enable)?;

        if cli_args.json {
            println!("{}", json!({ "enabled": cli_args.plugin_enable, "key": key, "type": "plugin-toggled" }));
        } else {
            println!("{} {key}.", if cli_args.plugin_enable { "Enabled" } else { "Disabled" });
        }

        return Ok(0);
    }

    // --marketplace-list
    let file = load_marketplaces_file(Path::new(&home.marketplaces_path))?;
    let listed = list_marketplace_plugins(home, &file);
    let overrides = load_project_plugin_overrides(Path::new(cwd));

    if cli_args.json {
        let plugins: Vec<serde_json::Value> = listed
            .plugins
            .iter()
            .map(|plugin| {
                json!({
                    "description": plugin.description,
                    "enabled": is_marketplace_key_enabled(&plugin.key, &plugin.key, &file, &overrides),
                    "key": plugin.key,
                    "skills": plugin.skills.iter().map(|skill| skill.name.clone()).collect::<Vec<_>>()
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "issues": listed.issues, "plugins": plugins })).unwrap_or_default()
        );
        return Ok(0);
    }

    for issue in &listed.issues {
        eprintln!("issue: {issue}");
    }

    if listed.plugins.is_empty() {
        println!("No marketplaces registered. Add one with drip --marketplace-add <git-url-or-path>.");
        return Ok(0);
    }

    for plugin in &listed.plugins {
        let enabled = is_marketplace_key_enabled(&plugin.key, &plugin.key, &file, &overrides);

        println!("{} {}  {}", if enabled { "[x]" } else { "[ ]" }, plugin.key, plugin.description);

        for skill in &plugin.skills {
            println!("      skill: {}", skill.name);
        }
    }

    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::{non_empty, to_fixed_2};

    #[test]
    fn non_empty_follows_js_truthiness() {
        assert!(!non_empty(&None));
        assert!(!non_empty(&Some(String::new())));
        assert!(non_empty(&Some(" ".to_string())));
        assert!(non_empty(&Some("goal".to_string())));
    }

    #[test]
    fn to_fixed_2_rounds_an_exact_half_up_like_js() {
        assert_eq!(to_fixed_2(0.125), "0.13");
        assert_eq!(to_fixed_2(0.375), "0.38");
        assert_eq!(to_fixed_2(1.005), "1.00");
        assert_eq!(to_fixed_2(0.0), "0.00");
        assert_eq!(to_fixed_2(12.3456), "12.35");
        assert_eq!(to_fixed_2(131072.0 / 1_048_576.0), "0.13");
    }
}
