use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use regex::Regex;

use crate::cli::follow::{read_inbox_entries, read_inbox_messages};
use crate::cli::skills::{compose_skill_system_prompt, LoadedCliSkill};
use crate::core::inference::ResolvedInferenceConfig;
use crate::core::lease::{clear_lease, write_lease};
use crate::core::state::{
    add_tasks, create_harness_state, has_unfinished_tasks, load_harness_state, save_harness_state,
    start_follow_up_goal, HarnessTaskInput, HarnessTaskPlacement,
};
use crate::core::types::{HarnessEvent, HarnessEventType, HarnessOperatorMessage, HarnessRunResult, HarnessState};
use crate::harness::harness_tools::RepoMemoryConfig;
use crate::harness::model_call::AbortSignal;
use crate::harness::prompt::compose_harness_system_prompt;
use crate::harness::r#loop::{run_solid_state_harness, EmitFn, OperatorInboxEntry, SolidStateHarnessOptions};
use crate::harness::roles::{HarnessRoleBindings, HarnessRoleRuntime};
use crate::tools::types::{ChatToolDefinition, ChatToolRuntimeServices};

/// Arguments for a goal run: working directory, goal text, and the prior
/// goal context handed to the model.
pub struct CliGoalRunArgs {
    pub cwd: String,
    pub goal: String,
    pub goal_context: Option<String>,
    pub goal_images: Option<Vec<String>>,
    pub hooks: crate::harness::hooks::HooksConfig,
    pub inference: ResolvedInferenceConfig,
    /// Session inbox file (drip --send); polled at cycle boundaries when set.
    pub inbox_path: Option<PathBuf>,
    /// Liveness lease file: written at run start, heartbeated per event, cleared at run end.
    pub lease_path: Option<PathBuf>,
    pub max_iterations: Option<i64>,
    pub on_event: EmitFn,
    /// Archive an unfinished ledger and replan instead of continuing it.
    pub new_goal: bool,
    /// Dry-run: stop once planning yields tasks.
    pub plan_only: bool,
    /// Harness-managed credentials to scrub from tool output (name → value).
    pub redact_secrets: Vec<(String, String)>,
    /// Per-attempt cap on one model HTTP request (see DEFAULT_REQUEST_TIMEOUT_MS).
    pub request_timeout_ms: Option<u64>,
    /// Task titles a FRESH session starts with, so the harness skips its planning
    /// loop and works them directly. For a goal whose shape is known up front
    /// (a --review child: read, grep, report) planning is pure overhead — and
    /// the transcript audit showed children doing the work in the planning loop,
    /// failing finish_task on a task that did not exist yet, then redoing it.
    /// Ignored when the session already has state (a resume keeps its ledger).
    pub seed_tasks: Option<Vec<String>>,
    /// Repo memory bank (.drip/memory): where repo-scoped remember/forget persist, and whose index is injected each activation.
    pub repo_memory: Option<RepoMemoryConfig>,
    /// Which role handles which loop kind (resolved by cli/roles.rs).
    pub role_bindings: Option<HarnessRoleBindings>,
    /// Resolved capability profiles applied per loop.
    pub roles: Option<Vec<HarnessRoleRuntime>>,
    pub signal: Option<AbortSignal>,
    pub skills: Vec<LoadedCliSkill>,
    pub state_path: PathBuf,
    pub summarize_run: Option<bool>,
    pub tools: Vec<ChatToolDefinition>,
    /// Runtime services for async tools (the web server owns one; CLI defaults inside the harness).
    pub tool_services: Option<ChatToolRuntimeServices>,
}

/// Whether a previous run was resumed, with its harness state.
pub struct PreparedGoalState {
    pub resumed_unfinished: bool,
    pub state: Option<HarnessState>,
}

// Mirrors the web server's follow-up semantics: a new goal (or an explicit re-run
// of a finished one) archives the previous goal's tasks into history, while the
// same goal with unfinished tasks resumes in place.
pub fn prepare_state_for_goal(state_path: &Path, goal: &str, new_goal: bool) -> Result<PreparedGoalState, String> {
    // A corrupt or foreign state.json is allowed to abort the run here:
    // fail loudly rather than silently replan over it and lose history.
    // state.json aborts the run instead of being replanned over and lost.
    let Some(mut existing_state) = load_harness_state(state_path).map_err(|error| error.to_string())? else {
        return Ok(PreparedGoalState {
            resumed_unfinished: false,
            state: None,
        });
    };

    if existing_state.goal == goal && has_unfinished_tasks(&existing_state) {
        return Ok(PreparedGoalState {
            resumed_unfinished: true,
            state: Some(existing_state),
        });
    }

    // A DIFFERENT prompt against an unfinished ledger continues the ledger
    // with the prompt applied as steering — the additive-resume pattern the
    // delegation recipe teaches. Archiving used to require byte-identical
    // text, so "complete the remaining tasks" silently replanned from scratch
    // (debt audit N6). --new-goal is the explicit archive-and-replan path.
    if has_unfinished_tasks(&existing_state) && !new_goal {
        let mut messages: Vec<HarnessOperatorMessage> = existing_state.operator_messages.clone().unwrap_or_default();
        let keep_from = messages.len().saturating_sub(7);
        messages = messages.split_off(keep_from);
        messages.push(HarnessOperatorMessage {
            id: format!("resume-{}", existing_state.iteration),
            received_at_iteration: existing_state.iteration,
            text: goal.to_string(),
        });
        existing_state.operator_messages = Some(messages);
        save_harness_state(state_path, &existing_state).map_err(|error| error.to_string())?;

        return Ok(PreparedGoalState {
            resumed_unfinished: true,
            state: Some(existing_state),
        });
    }

    start_follow_up_goal(&mut existing_state, goal);
    save_harness_state(state_path, &existing_state).map_err(|error| error.to_string())?;

    Ok(PreparedGoalState {
        resumed_unfinished: false,
        state: Some(existing_state),
    })
}

/// The first 12 characters of a git object id, or the whole string when git
/// printed something shorter (a slice would panic on it).
fn short_sha(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

fn git_publish_pattern() -> &'static Regex {
    static PATTERN: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| Regex::new(r"\bgit\s+(commit|push)\b|\bgh\s+pr\s+create\b").unwrap())
}

// The PR #19 incident shape: a run pushes a branch, then keeps editing the
// working tree and never commits the follow-up — the pushed code and the local
// code silently diverge. When a run published commits, a dirty tree at run end
// is worth a loud warning in the timeline and transcript.
pub fn check_published_run_left_dirty_tree(cwd: &Path) -> Option<String> {
    // Not a git repository (or git unavailable): nothing to reconcile.
    let stdout = run_git(cwd, &["status", "--porcelain"])?;
    let dirty_paths: Vec<String> = stdout
        .split('\n')
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect();

    if dirty_paths.is_empty() {
        return None;
    }

    let shown: Vec<String> = dirty_paths
        .iter()
        .take(5)
        .map(|line| {
            // line.replace(/^\S+\s+/, "")
            let trimmed = line.trim_start_matches(|c: char| !c.is_whitespace());
            trimmed.trim_start().to_string()
        })
        .collect();

    Some(format!(
        "this run committed or pushed, but the working tree still has {} uncommitted change(s) ({}{}). The pushed branch may not match the local files — reconcile before relying on it.",
        dirty_paths.len(),
        shown.join(", "),
        if dirty_paths.len() > 5 { ", ..." } else { "" }
    ))
}

// Ground truth for the run summary: what actually changed on disk, so the
// summary reconciles the task ledger against reality instead of guessing.
pub fn collect_workspace_run_facts(cwd: &Path) -> Option<String> {
    // Not a git repository (or git unavailable): no facts to add.
    let stdout = run_git(cwd, &["status", "--porcelain"])?;
    let lines: Vec<&str> = stdout
        .split('\n')
        .map(|line| line.trim_end())
        .filter(|line| !line.is_empty())
        .collect();

    if lines.is_empty() {
        return Some("git reports a clean working tree (no uncommitted changes).".to_string());
    }

    let shown: Vec<&str> = lines.iter().take(40).copied().collect();
    let omitted = lines.len() - shown.len();
    let mut out: Vec<String> = vec!["git status --porcelain (uncommitted changes):".to_string()];
    out.extend(shown.iter().map(|line| line.to_string()));

    if omitted > 0 {
        out.push(format!("... and {omitted} more"));
    }

    Some(out.join("\n"))
}

// The bank's index (MEMORY.md) is read once at run start — pages the model
// wants ride in via READ, and notes saved mid-run become visible next run.
pub fn load_repo_memory_index(repo_memory: Option<&RepoMemoryConfig>) -> Option<String> {
    let repo_memory = repo_memory?;

    if repo_memory.disabled {
        return None;
    }

    // Missing index means an empty bank: inject nothing.
    let index = std::fs::read_to_string(Path::new(&repo_memory.memory_dir).join("MEMORY.md")).ok()?;

    if index.trim().is_empty() {
        None
    } else {
        Some(index)
    }
}

// A fresh state carrying the caller's tasks: the loop finds a current task on
// its first pass and never plans. Null when there is nothing to seed, so the
// harness creates its own empty state exactly as before.
pub fn seed_initial_state(goal: &str, seed_tasks: Option<&[String]>) -> Option<HarnessState> {
    let titles: Vec<String> = seed_tasks
        .unwrap_or(&[])
        .iter()
        .map(|title| title.trim().to_string())
        .filter(|title| !title.is_empty())
        .collect();

    if titles.is_empty() {
        return None;
    }

    let mut state = create_harness_state(goal);

    add_tasks(
        &mut state,
        titles
            .into_iter()
            .map(|title| HarnessTaskInput {
                depends_on: None,
                review_of: None,
                role: None,
                title,
            })
            .collect(),
        HarnessTaskPlacement::End,
    );

    Some(state)
}

/// Runs a goal end to end: prepare state, spawn the harness loop, then
/// record the outcome (see [`SessionGoalOutcome`]).
pub async fn run_cli_goal(args: CliGoalRunArgs) -> Result<HarnessRunResult, String> {
    let PreparedGoalState {
        resumed_unfinished,
        state: prepared_state,
    } = prepare_state_for_goal(&args.state_path, &args.goal, args.new_goal)?;
    let state = prepared_state.or_else(|| seed_initial_state(&args.goal, args.seed_tasks.as_deref()));
    let persona_with_skills = compose_skill_system_prompt(&args.inference.system_prompt, &args.skills);
    let repo_memory_index = load_repo_memory_index(args.repo_memory.as_ref());
    let published_from_workspace = Arc::new(AtomicBool::new(false));

    // Session id from the state path (.../sessions/<id>/state.json) — good
    // enough for the ref name and avoids widening the arg surface.
    let session_dir_name = args
        .state_path
        .parent()
        .and_then(|dir| dir.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let cwd_path = PathBuf::from(&args.cwd);
    let baseline = capture_workspace_baseline(&cwd_path, &session_dir_name);

    if let Some(baseline) = &baseline {
        (args.on_event)(HarnessEvent {
            data: None,
            detail: baseline.note.clone(),
            iteration: 0,
            r#type: HarnessEventType::RunWarning,
        });
    }

    // (Mutex<bool>, Condvar): the run end wakes the heartbeat immediately
    // instead of waiting out its sleep — a 250ms tick was ~250ms of pure
    // overhead on every short run (review children, DELEGATE calls).
    let heartbeat_stop: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)> =
        Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let mut heartbeat_thread: Option<std::thread::JoinHandle<()>> = None;

    if let Some(lease_path) = &args.lease_path {
        let _ = write_lease(lease_path, &chrono::Utc::now);
        // The heartbeat must not depend on harness events: a single BASH call can
        // run for minutes with no event traffic.
        let lease_path = lease_path.clone();
        let stop = heartbeat_stop.clone();
        heartbeat_thread = Some(std::thread::spawn(move || {
            let (lock, condvar) = &*stop;
            let mut stopped = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            loop {
                let (guard, timeout) = condvar
                    .wait_timeout(stopped, std::time::Duration::from_millis(30_000))
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                stopped = guard;
                if *stopped {
                    break;
                }
                if timeout.timed_out() {
                    // A failed heartbeat must never kill the run.
                    let _ = write_lease(&lease_path, &chrono::Utc::now);
                }
            }
        }));
    }

    let goal_context = match &baseline {
        Some(baseline) => Some(
            [
                args.goal_context.clone().unwrap_or_default(),
                format!("workspace_baseline: {}", baseline.context_note),
            ]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"),
        ),
        None => args.goal_context.clone(),
    };

    let on_event = args.on_event.clone();
    let published_flag = published_from_workspace.clone();
    let emit: EmitFn = Arc::new(move |event: HarnessEvent| {
        if event.r#type == HarnessEventType::ToolCall && git_publish_pattern().is_match(&event.detail) {
            published_flag.store(true, Ordering::Relaxed);
        }

        on_event(event);
    });

    let inbox_path = args.inbox_path.clone();
    let facts_cwd = cwd_path.clone();
    let options = SolidStateHarnessOptions {
        collect_operator_messages: inbox_path.as_ref().map(|path| {
            let path = path.clone();
            Box::new(move |consumed_count: i64| -> Vec<OperatorInboxEntry> {
                read_inbox_entries(&path, consumed_count.max(0) as usize)
                    .into_iter()
                    .map(|entry| OperatorInboxEntry {
                        at: entry.at,
                        text: entry.text,
                    })
                    .collect()
            }) as Box<dyn FnMut(i64) -> Vec<OperatorInboxEntry> + Send>
        }),
        // A brand-new state must not consume the session's historical inbox
        // as fresh steering.
        initial_inbox_cursor: inbox_path
            .as_ref()
            .map(|path| read_inbox_messages(path, 0).len() as i64),
        collect_run_facts: Some(Box::new(move || collect_workspace_run_facts(&facts_cwd))),
        cwd: Some(args.cwd.clone()),
        // When an unfinished ledger continues under a new prompt, the harness
        // keeps working the ORIGINAL goal; the new prompt rides in as steering.
        goal: match (&state, resumed_unfinished) {
            (Some(state), true) => state.goal.clone(),
            _ => args.goal.clone(),
        },
        goal_context,
        goal_images: args.goal_images.clone(),
        headers: args.inference.headers.clone(),
        initial_state: state,
        max_iterations: args.max_iterations,
        request_timeout_ms: args.request_timeout_ms,
        model: Some(args.inference.model.clone()),
        on_event: Some(emit),
        provider: Some(args.inference.provider.clone()),
        reasoning_effort: args.inference.reasoning_effort.clone(),
        refresh_headers: args.inference.refresh_headers.clone(),
        plan_only: args.plan_only,
        prompt_cache_key: Some(format!("drip:{session_dir_name}")),
        redact_secrets: args.redact_secrets.clone(),
        repo_memory: args.repo_memory.clone(),
        repo_memory_index,
        role_bindings: args.role_bindings.clone(),
        roles: args.roles.clone().filter(|roles| !roles.is_empty()),
        signal: args.signal.clone(),
        state_path: Some(args.state_path.clone()),
        summarize_run: args.summarize_run,
        system_prompt: Some(compose_harness_system_prompt(Some(&persona_with_skills))),
        fallback_route: args.inference.fallback_route.as_ref().map(|route| route.to_model_route()),
        tool_route: args.inference.tool_route.as_ref().map(|route| route.to_model_route()),
        tools: args.tools,
        tool_services: args.tool_services.clone(),
        url: Some(args.inference.url.clone()),
        hooks: args.hooks.clone(),
        ..SolidStateHarnessOptions::default()
    };

    let result = run_solid_state_harness(options).await;

    if let Ok(result) = &result {
        if published_from_workspace.load(Ordering::Relaxed) {
            if let Some(warning) = check_published_run_left_dirty_tree(&cwd_path) {
                (args.on_event)(HarnessEvent {
                    data: None,
                    detail: warning,
                    iteration: result.state.iteration,
                    r#type: HarnessEventType::RunWarning,
                });
            }
        }
    }

    // finally { clearInterval(heartbeatTimer); clearLease(leasePath) }
    {
        let (lock, condvar) = &*heartbeat_stop;
        *lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
        condvar.notify_all();
    }
    if let Some(handle) = heartbeat_thread {
        let _ = handle.join();
    }
    if let Some(lease_path) = &args.lease_path {
        clear_lease(lease_path);
    }

    result
}

fn run_git(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).current_dir(cwd).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Result of [`capture_workspace_baseline`]: the stash commit sha, a
/// human note, and the status context line.
/// `{ contextNote, note, sha }`.
pub struct WorkspaceBaseline {
    pub context_note: String,
    pub note: String,
    pub sha: String,
}

// A dirty tree at run start is the operator uncommitted work sitting in
// the model blast zone. `git stash create` builds a stash commit WITHOUT
// touching the worktree; pinning it under refs/drip/baseline/ gives the
// operator a guaranteed restore point (git stash apply <sha>) at zero cost.
//
// The git calls run synchronously and block the caller; any failure (not a
// git repo, git unavailable) is best-effort: returns None instead of failing.
// repo, git unavailable) is best-effort: returns None instead of failing.
pub fn capture_workspace_baseline(cwd: &Path, session_id: &str) -> Option<WorkspaceBaseline> {
    let status = run_git(cwd, &["status", "--porcelain"])?;

    if status.trim().is_empty() {
        return None;
    }

    let stash_out = run_git(
        cwd,
        &[
            "stash",
            "create",
            &format!("drip baseline before session {session_id}"),
        ],
    )?;
    let sha = stash_out.trim().to_string();

    if sha.is_empty() {
        return None;
    }

    run_git(
        cwd,
        &[
            "update-ref",
            &format!("refs/drip/baseline/{session_id}"),
            &sha,
        ],
    )?;

    Some(WorkspaceBaseline {
        // The model-visible variant carries no restore command: a dogfooded
        // worker executed the "git stash apply <sha>" from the old note
        // mid-run (N9) — re-applying a baseline over in-progress edits would
        // silently revert work. The command stays on the operator-facing
        // event note below.
        context_note: format!(
            "The working tree had uncommitted changes at run start; a baseline snapshot is pinned at refs/drip/baseline/{session_id} for the operator to recover pre-run state after the run. Never apply or restore it during the run — treat it as read-only bookkeeping."
        ),
        note: format!(
            "The working tree had uncommitted changes at run start; a baseline snapshot is pinned at refs/drip/baseline/{session_id} ({}) — restorable with: git stash apply {}",
            short_sha(&sha),
            short_sha(&sha)
        ),
        sha,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    use super::capture_workspace_baseline;

    fn run_git(cwd: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git should be available");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    #[test]
    fn pins_a_stash_ref_for_dirty_trees_and_skips_clean_ones() {
        let root = tempfile::TempDir::with_prefix("drip-baseline-").unwrap();

        run_git(root.path(), &["init", "-q"]);
        run_git(
            root.path(),
            &[
                "-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "--allow-empty", "-m", "init",
            ],
        );

        assert!(capture_workspace_baseline(root.path(), "session-1").is_none());

        fs::write(root.path().join("work.txt"), "uncommitted").unwrap();
        run_git(root.path(), &["add", "work.txt"]);

        let baseline =
            capture_workspace_baseline(root.path(), "session-1").expect("dirty tree should pin a baseline");

        assert!(baseline.note.contains("refs/drip/baseline/session-1"));

        let ref_sha = run_git(root.path(), &["rev-parse", "refs/drip/baseline/session-1"]);
        assert_eq!(ref_sha.trim(), baseline.sha);
        // The worktree itself was not touched.
        assert_eq!(
            fs::read_to_string(root.path().join("work.txt")).unwrap(),
            "uncommitted"
        );
    }

    #[test]
    fn returns_null_outside_a_git_repo_instead_of_failing_the_run() {
        let root = tempfile::TempDir::with_prefix("drip-baseline-").unwrap();

        assert!(capture_workspace_baseline(root.path(), "session-2").is_none());
    }
}
