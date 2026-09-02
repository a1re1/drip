// port of src/cli/review.ts
//
// The --review command: enumerate <base>...HEAD, plan review units, fan the
// units out to reviewer children (one in-process session each, bounded by a
// pool), retry what the first pass did not deliver once, then synthesize one
// report unless every unit came back clean.
//
// Concurrency model. The TS runs its children as concurrent promises on one
// event loop sharing one SessionIndex. A child's run future is not `Send`
// here (tool closures are plain `Box<dyn Fn>`, the index wraps a rusqlite
// connection), so each pool lane is an OS thread that opens its own index on
// the same database file, builds its own tool pack from the factory, and
// drives the child on a private runtime. WAL mode serializes the writers.

use std::collections::HashMap;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::cli::review_report::{
    build_skipped_synthesis_report, build_synthesis_prompt, build_unit_review_prompt, confidence_from_counts,
    format_file_report, looks_like_file_report, looks_like_synthesis_report, parse_file_report, pick_report_body,
    chunk_oversized_units, plan_retry_units, plan_review_units, review_child_budget, unit_part_label, should_skip_synthesis, split_unit_report,
    unit_review_task_title, with_computed_confidence, DiffFile, FileReport, ReportBodySources, ReviewSynthesisMode,
    ReviewUnit, ReviewUnitKind, RetryRun, SkippedSynthesisArgs, SkippedSynthesisFile, SynthesisPromptArgs,
    SynthesisSkipFile, UnitPromptFile, UnitReviewPromptArgs, REVIEW_TOOL_NAMES,
    SKIPPED_FILE_NAMES, SKIPPED_FILE_SUFFIXES, SYNTHESIS_TASK_TITLE,
};
use crate::cli::roles::PRESET_FAST_PROFILE_ID;
use crate::cli::session_run::{run_session_goal, SessionGoalArgs, SessionGoalError, SessionGoalOutcome};
use crate::cli::skills::LoadedCliSkill;
use crate::core::home::DripProject;
use crate::core::inference::ResolvedInferenceConfig;
use crate::core::sessions::{create_session, open_session_index, CreateSessionArgs, ProjectPaths};
use crate::core::types::{serialize_js_number, HarnessEvent, HarnessEventType, HarnessRunReason, HarnessTask, HarnessTaskStatus};
use crate::harness::model_call::AbortSignal;
use crate::tools::types::ChatToolDefinition;

// A reviewer child's cycle budget comes from review_child_budget(unit) (four
// cycles of up to four tool rounds, six for an oversized unit). The synthesis
// pass gets more (it reads every report plus the stat).
const SYNTHESIS_MAX_ITERATIONS: i64 = 8;
// Per-request cap for a reviewer child's model calls: 150 s cuts a stalled
// request in half against the harness default (240 s).
pub const FILE_REVIEW_REQUEST_TIMEOUT_MS: u64 = 150_000;
const DEFAULT_REVIEW_CONCURRENCY: usize = 4;
// Wall-clock cap on one reviewer child; past it the child is aborted and
// treated exactly like one that threw — retried once, then recorded as errored.
pub const UNIT_WALL_CLOCK_MS: u64 = 8 * 60_000;
pub const SYNTHESIS_WALL_CLOCK_MS: u64 = 10 * 60_000;
// Both lanes run on the presets' fast profile (`glm-5-3-flash`).
pub const DEFAULT_REVIEW_FILE_PROFILE: &str = PRESET_FAST_PROFILE_ID;
pub const DEFAULT_REVIEW_SYNTH_PROFILE: &str = PRESET_FAST_PROFILE_ID;

/// Builds a fresh tool pack for one child (tool closures cannot be shared
/// across the pool's threads).
pub type ToolFactory = Arc<dyn Fn() -> Vec<ChatToolDefinition> + Send + Sync>;
/// Test seam: `(baseRef, cwd) -> changed paths`.
pub type ListChangedFilesFn = Arc<dyn Fn(&str, &str) -> Result<Vec<String>, String> + Send + Sync>;
/// Test seam: `(baseRef, path, cwd) -> diff text`.
pub type ReadDiffFn = Arc<dyn Fn(&str, &str, &str) -> Result<String, String> + Send + Sync>;
/// Test seam: `(path, cwd) -> committed content` (None when unreadable).
pub type ReadFileAtHeadFn = Arc<dyn Fn(&str, &str) -> Option<String> + Send + Sync>;
/// Test seam: the child-session runner (defaults to run_session_goal on a private runtime).
pub type RunGoalFn =
    Arc<dyn for<'a> Fn(SessionGoalArgs<'a>) -> Result<SessionGoalOutcome, SessionGoalError> + Send + Sync>;
pub type ProgressFn = Arc<dyn Fn(ReviewProgressEvent) + Send + Sync>;

#[derive(Debug, Clone, Copy, Default)]
pub struct ReviewWallClockMs {
    pub synthesis: Option<u64>,
    pub unit: Option<u64>,
}

pub struct ReviewCommandArgs {
    /// The merge-base diff base (explicit --base, or the derived default branch).
    pub base_ref: String,
    /// Max simultaneous review children (from --concurrency).
    pub concurrency: Option<usize>,
    /// The --context text: what this change is trying to achieve.
    pub context: String,
    pub cwd: String,
    /// Fully resolved route for the per-file reviewers.
    pub file_inference: ResolvedInferenceConfig,
    /// The project's session index database; every lane opens its own connection.
    pub index_db_path: String,
    pub project: DripProject,
    pub list_changed_files: Option<ListChangedFilesFn>,
    pub read_diff: Option<ReadDiffFn>,
    pub read_file_at_head: Option<ReadFileAtHeadFn>,
    /// Progress sink (the CLI prints these to stderr): the plan, then one line per finished unit.
    pub on_progress: Option<ProgressFn>,
    pub wall_clock_ms: Option<ReviewWallClockMs>,
    pub run_goal: Option<RunGoalFn>,
    /// Stops the whole review (every child sees it).
    pub signal: Option<AbortSignal>,
    pub skills: Vec<LoadedCliSkill>,
    /// Fully resolved route for the holistic synthesis pass.
    pub synth_inference: ResolvedInferenceConfig,
    /// When the synthesis pass runs (default auto: only when a unit reported something).
    pub synthesis: Option<ReviewSynthesisMode>,
    pub tools: ToolFactory,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileReviewResult {
    pub path: String,
    /// `"2/4"` when this row is one chunk of an oversized file.
    pub part: Option<String>,
    /// The report body the child produced, or an error description.
    pub report: String,
    pub rating: Option<String>,
    /// True when the rating was derived from the counts because the reviewer wrote no Rating line.
    pub rating_derived: bool,
    pub p0: u32,
    pub p1: u32,
    pub p2: u32,
    pub session_id: Option<String>,
    pub errored: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct ReviewCounts {
    pub p0: u32,
    pub p1: u32,
    pub p2: u32,
}

#[derive(Debug, Clone)]
pub enum ReviewProgressEvent {
    Planned { units: Vec<ReviewUnit>, file_count: usize },
    UnitDone { unit: ReviewUnit, errored: bool, elapsed_ms: u64, counts: ReviewCounts, retry: bool },
    UnitRetry { unit: ReviewUnit, reason: String },
    SynthesisStart,
    SynthesisSkipped { mode: ReviewSynthesisMode },
    SynthesisDone { elapsed_ms: u64, errored: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewUsage {
    pub calls: u64,
    pub completion_tokens: i64,
    pub prompt_tokens: i64,
    #[serde(serialize_with = "serialize_js_number")]
    pub retry_wait_seconds: f64,
}

impl ReviewUsage {
    fn add(&mut self, from: &ReviewUsage) {
        self.calls += from.calls;
        self.completion_tokens += from.completion_tokens;
        self.prompt_tokens += from.prompt_tokens;
        self.retry_wait_seconds += from.retry_wait_seconds;
    }
}

impl Serialize for ReviewUnitKind {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(match self {
            ReviewUnitKind::Docs => "docs",
            ReviewUnitKind::Code => "code",
        })
    }
}

impl Serialize for ReviewSynthesisMode {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(synthesis_mode_name(*self))
    }
}

pub fn synthesis_mode_name(mode: ReviewSynthesisMode) -> &'static str {
    match mode {
        ReviewSynthesisMode::Auto => "auto",
        ReviewSynthesisMode::Always => "always",
        ReviewSynthesisMode::Never => "never",
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewUnitResult {
    /// Wall-clock time of the child, in ms.
    pub elapsed_ms: u64,
    pub errored: bool,
    pub kind: ReviewUnitKind,
    pub label: String,
    pub paths: Vec<String>,
    /// Set when this unit is the one retry of an errored or incomplete unit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_of: Option<String>,
    pub session_id: Option<String>,
    pub usage: ReviewUsage,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewOutcomeFile {
    pub errored: bool,
    pub p0: u32,
    pub p1: u32,
    pub p2: u32,
    /// `"2/4"` when this row is one chunk of an oversized file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub part: Option<String>,
    pub path: String,
    pub rating: Option<String>,
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewTiming {
    pub synthesis_ms: u64,
    pub total_ms: u64,
    pub units_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewOutcome {
    pub base: String,
    pub confidence: u32,
    pub counts: ReviewCounts,
    pub exit_code: i32,
    /// Per changed file, whichever unit reviewed it — the contract callers key on.
    pub files: Vec<ReviewOutcomeFile>,
    pub report: String,
    /// Wall-clock split, in ms: the whole command, the unit fan-out (both passes), and the synthesis.
    pub timing: ReviewTiming,
    /// The reviewer children that actually ran: one per unit, not per file.
    pub units: Vec<ReviewUnitResult>,
    /// Model usage summed over every child plus the synthesis pass.
    pub usage: ReviewUsage,
}

// ---------------------------------------------------------------------------
// git helpers — execFileAsync("git", ...) equivalents. A non-zero exit is an
// error carrying Node's "Command failed: <cmd>\n<stderr>" shape.
// ---------------------------------------------------------------------------

fn git_stdout(args: &[&str], cwd: &str) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|error| format!("spawn git: {error}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);

        return Err(format!("Command failed: git {}\n{}", args.join(" "), stderr));
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn default_list_changed_files(base_ref: &str, cwd: &str) -> Result<Vec<String>, String> {
    let stdout = git_stdout(&["diff", "--name-only", &format!("{base_ref}...HEAD")], cwd)?;

    Ok(stdout.split('\n').map(str::trim).filter(|line| !line.is_empty()).map(String::from).collect())
}

// The committed content, not the working tree: the review is of <base>...HEAD,
// and an uncommitted edit must not leak into what the reviewer is told is the
// change. None (not an error) for anything git cannot show as text.
fn default_read_file_at_head(path: &str, cwd: &str) -> Option<String> {
    let output = Command::new("git").args(["show", &format!("HEAD:{path}")]).current_dir(cwd).output().ok()?;

    if !output.status.success() || output.stdout.contains(&0u8) {
        return None;
    }

    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn default_read_diff(base_ref: &str, path: &str, cwd: &str) -> Result<String, String> {
    git_stdout(&["diff", &format!("{base_ref}...HEAD"), "--", path], cwd)
}

fn head_has_path(path: &str, cwd: &str) -> bool {
    Command::new("git")
        .args(["cat-file", "-e", &format!("HEAD:{path}")])
        .current_dir(cwd)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn ref_exists(reference: &str, cwd: &str) -> bool {
    git_stdout(&["rev-parse", "--verify", "--quiet", &format!("{reference}^{{commit}}")], cwd).is_ok()
}

// The default base is the remote-tracking ref of origin's HEAD branch
// (origin/main), not the local branch of the same name: in a worktree the
// local main is whatever the main checkout last had. The local branch is the
// fallback when the remote-tracking ref does not exist.
pub fn resolve_default_base_ref(cwd: &str) -> String {
    if let Ok(stdout) = git_stdout(&["remote", "show", "origin"], cwd) {
        if let Some(head_branch) = stdout.split('\n').map(str::trim).find(|line| line.starts_with("HEAD branch:")) {
            let branch = head_branch.replacen("HEAD branch:", "", 1).trim().to_string();

            return if ref_exists(&format!("origin/{branch}"), cwd) { format!("origin/{branch}") } else { branch };
        }
    }

    if git_stdout(&["rev-parse", "--verify", "main"], cwd).is_ok() {
        "main".to_string()
    } else {
        "master".to_string()
    }
}

// ---------------------------------------------------------------------------
// child plumbing
// ---------------------------------------------------------------------------

// Every plain-text turn a child emits, in order (pick_report_body chooses the
// report among them once the run ends), plus the child's model usage: each
// inference event carries the call's token counts, each rate-limited event
// the seconds it waited.
fn collect_child_events(
    texts: Arc<Mutex<Vec<String>>>,
    usage: Arc<Mutex<ReviewUsage>>,
) -> Arc<dyn Fn(HarnessEvent) + Send + Sync> {
    Arc::new(move |event: HarnessEvent| match event.r#type {
        HarnessEventType::ModelText if !event.detail.trim().is_empty() => {
            texts.lock().unwrap().push(event.detail);
        }
        HarnessEventType::Inference => {
            let mut usage = usage.lock().unwrap();
            let data = event.data.as_ref();

            usage.calls += 1;
            usage.completion_tokens += data.and_then(|data| data.completion_tokens).unwrap_or(0);
            usage.prompt_tokens += data.and_then(|data| data.prompt_tokens).unwrap_or(0);
        }
        HarnessEventType::RateLimited => {
            usage.lock().unwrap().retry_wait_seconds += event.data.as_ref().and_then(|data| data.wait_seconds).unwrap_or(0.0);
        }
        _ => {}
    })
}

fn completed_task_summaries(tasks: &[HarnessTask]) -> Vec<String> {
    tasks
        .iter()
        .filter(|task| task.status == HarnessTaskStatus::Completed)
        .filter_map(|task| task.summary.clone())
        .collect()
}

// The one real hazard of concurrent children is racing on the repo memory
// bank — children run with no_repo_memory, and each owns its session dir and
// index connection, so a bounded pool of lanes is safe.
fn run_pool<T, R, F>(items: Vec<T>, concurrency: usize, worker: F) -> Vec<R>
where
    T: Send,
    R: Send,
    F: Fn(T) -> R + Sync,
{
    let count = items.len();
    let queue: Mutex<Vec<(usize, T)>> = Mutex::new(items.into_iter().enumerate().rev().collect());
    let results: Mutex<Vec<Option<R>>> = Mutex::new((0..count).map(|_| None).collect());
    let lanes = concurrency.min(count).max(1);

    std::thread::scope(|scope| {
        for _ in 0..lanes {
            scope.spawn(|| loop {
                let next = queue.lock().unwrap().pop();
                let Some((index, item)) = next else { break };
                let result = worker(item);

                results.lock().unwrap()[index] = Some(result);
            });
        }
    });

    results.into_inner().unwrap().into_iter().map(|result| result.expect("pool lane filled every slot")).collect()
}

// review-report.ts timeboxed(): a child signal that fires after `ms` or when
// the parent aborts; `release` stops the watcher once the child has ended.
struct Timebox {
    signal: AbortSignal,
    timed_out: Arc<AtomicBool>,
    released: Arc<AtomicBool>,
}

impl Timebox {
    fn new(ms: u64, parent: Option<&AbortSignal>) -> Self {
        let signal = AbortSignal::new();
        let timed_out = Arc::new(AtomicBool::new(false));
        let released = Arc::new(AtomicBool::new(false));

        if parent.is_some_and(AbortSignal::is_aborted) {
            signal.abort();
        } else {
            let watcher_signal = signal.clone();
            let watcher_timed_out = timed_out.clone();
            let watcher_released = released.clone();
            let parent = parent.cloned();
            let deadline = Instant::now() + Duration::from_millis(ms);

            std::thread::spawn(move || {
                while !watcher_released.load(Ordering::SeqCst) {
                    if parent.as_ref().is_some_and(AbortSignal::is_aborted) {
                        watcher_signal.abort();
                        break;
                    }

                    if Instant::now() >= deadline {
                        watcher_timed_out.store(true, Ordering::SeqCst);
                        watcher_signal.abort();
                        break;
                    }

                    std::thread::sleep(Duration::from_millis(50));
                }
            });
        }

        Self { signal, timed_out, released }
    }

    fn timed_out(&self) -> bool {
        self.timed_out.load(Ordering::SeqCst)
    }

    fn release(&self) {
        self.released.store(true, Ordering::SeqCst);
    }
}

impl Drop for Timebox {
    fn drop(&mut self) {
        self.release();
    }
}

fn default_run_goal(args: SessionGoalArgs<'_>) -> Result<SessionGoalOutcome, SessionGoalError> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .map_err(|error| SessionGoalError::Run(format!("review child runtime: {error}")))?;

    runtime.block_on(run_session_goal(args))
}

fn count_diff_lines(diff: &str) -> u32 {
    diff.split('\n').count() as u32
}

fn elapsed_ms(since: Instant) -> u64 {
    since.elapsed().as_millis() as u64
}

fn seconds_label(ms: u64) -> String {
    format!("{}s", (ms as f64 / 1000.0).round() as u64)
}

fn sum_counts(files: &[FileReviewResult]) -> ReviewCounts {
    files.iter().fold(ReviewCounts::default(), |acc, file| ReviewCounts {
        p0: acc.p0 + file.p0,
        p1: acc.p1 + file.p1,
        p2: acc.p2 + file.p2,
    })
}

fn outcome_files(files: &[FileReviewResult]) -> Vec<ReviewOutcomeFile> {
    files
        .iter()
        .map(|file| ReviewOutcomeFile {
            errored: file.errored,
            p0: file.p0,
            p1: file.p1,
            p2: file.p2,
            part: file.part.clone(),
            path: file.path.clone(),
            rating: file.rating.clone(),
            session_id: file.session_id.clone(),
        })
        .collect()
}

fn report_from_outcome(outcome: &SessionGoalOutcome, model_texts: &[String], looks_like_report: fn(&str) -> bool) -> String {
    let model_texts: Vec<&str> = model_texts.iter().map(String::as_str).collect();
    let task_summaries = completed_task_summaries(&outcome.result.state.tasks);
    let task_summaries: Vec<&str> = task_summaries.iter().map(String::as_str).collect();
    let summary = outcome
        .record
        .summary
        .as_deref()
        .or_else(|| outcome.result.state.run_summary.as_ref().map(|summary| summary.text.as_str()));

    pick_report_body(ReportBodySources {
        direct_response: outcome.result.state.direct_response.as_ref().map(|response| response.text.as_str()),
        looks_like_report,
        model_texts: &model_texts,
        summary,
        task_summaries: &task_summaries,
    })
    .unwrap_or("(no report produced)")
    .to_string()
}

fn errored_file(path: &str, description: &str) -> FileReviewResult {
    FileReviewResult {
        errored: true,
        p0: 0,
        p1: 0,
        p2: 0,
        part: None,
        path: path.to_string(),
        rating: None,
        rating_derived: false,
        report: description.to_string(),
        session_id: None,
    }
}

struct UnitRun {
    elapsed_ms: u64,
    errored: bool,
    files: Vec<FileReviewResult>,
    missing: Vec<String>,
    retry_of: Option<String>,
    session_id: Option<String>,
    unit: ReviewUnit,
    usage: ReviewUsage,
}

struct ReviewContext<'a> {
    args: &'a ReviewCommandArgs,
    contents: HashMap<String, String>,
    diffs: HashMap<String, String>,
    run_goal: RunGoalFn,
    unit_wall_clock_ms: u64,
}

impl ReviewContext<'_> {
    fn progress(&self, event: ReviewProgressEvent) {
        if let Some(on_progress) = &self.args.on_progress {
            on_progress(event);
        }
    }

    // Read-only review: an allowlist (REVIEW_TOOL_NAMES), not a denylist — a
    // new workspace tool must opt in to reach a reviewer.
    fn review_tools(&self) -> Vec<ChatToolDefinition> {
        (self.args.tools)().into_iter().filter(|tool| REVIEW_TOOL_NAMES.contains(&tool.name.as_str())).collect()
    }

    fn run_child(
        &self,
        goal: String,
        inference: ResolvedInferenceConfig,
        max_iterations: i64,
        request_timeout_ms: Option<u64>,
        seed_task: String,
        wall_clock_ms: u64,
        looks_like_report: fn(&str) -> bool,
    ) -> Result<(String, Option<String>, ReviewUsage), String> {
        let args = self.args;
        let index = open_session_index(&args.index_db_path);
        let project_paths = ProjectPaths::from(&args.project);
        let child_session = create_session(&index, CreateSessionArgs { cwd: args.cwd.clone(), project: &project_paths, now: "" });
        let timebox = Timebox::new(wall_clock_ms, args.signal.as_ref());
        let usage = Arc::new(Mutex::new(ReviewUsage::default()));
        let model_texts = Arc::new(Mutex::new(Vec::new()));
        let outcome = (self.run_goal)(SessionGoalArgs {
            cwd: args.cwd.clone(),
            goal,
            goal_context: None,
            goal_images: None,
            index: &index,
            inference,
            max_iterations: Some(max_iterations),
            mentions: None,
            new_goal: false,
            no_repo_memory: true,
            on_event: collect_child_events(model_texts.clone(), usage.clone()),
            plan_only: false,
            redact_secrets: Vec::new(),
            request_timeout_ms,
            // The child's one task is known before the model is asked
            // anything: seeding it skips the planning loop.
            seed_tasks: Some(vec![seed_task]),
            project: &args.project,
            role_bindings: None,
            roles: None,
            session: &child_session,
            signal: Some(timebox.signal.clone()),
            skills: args.skills.clone(),
            // The child's own final text is the report; a run summary would
            // be a second model's paraphrase of it.
            summarize_run: Some(false),
            tools: self.review_tools(),
            tool_services: None,
        });

        timebox.release();

        let usage_now = *usage.lock().unwrap();
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) => return Err(error.to_string()),
        };

        if outcome.result.reason == HarnessRunReason::Aborted {
            return Err(if timebox.timed_out() {
                format!("timed out after {}", seconds_label(wall_clock_ms))
            } else {
                "the review was stopped".to_string()
            });
        }

        let report = report_from_outcome(&outcome, &model_texts.lock().unwrap(), looks_like_report);

        Ok((report, Some(child_session.id.clone()), usage_now))
    }

    fn review_unit(&self, unit: ReviewUnit, retry_of: Option<String>) -> UnitRun {
        let args = self.args;
        let started_at = Instant::now();
        let files: Vec<UnitPromptFile<'_>> = unit
            .paths
            .iter()
            .map(|path| UnitPromptFile {
                path,
                diff: unit.part.as_ref().map(|part| part.diff.as_str()).or_else(|| self.diffs.get(path).map(String::as_str)).unwrap_or(""),
                content: self.contents.get(path).map(String::as_str),
            })
            .collect();
        let prompt = build_unit_review_prompt(UnitReviewPromptArgs { base_ref: &args.base_ref, context: &args.context, files: &files, unit: &unit });
        let result = self.run_child(
            prompt,
            args.file_inference.clone(),
            i64::from(review_child_budget(&unit).cycles),
            Some(FILE_REVIEW_REQUEST_TIMEOUT_MS),
            unit_review_task_title(&unit),
            self.unit_wall_clock_ms,
            looks_like_file_report,
        );

        match result {
            Ok((report, session_id, usage)) => {
                // The unit's report carries one "### File:" block per path;
                // the JSON contract stays per file. A path the reviewer
                // skipped keeps the whole unit report as its body (so the
                // synthesis still sees it) with no rating — it is not
                // errored, the child ran; it just did not answer for that
                // file. The caller retries those paths once.
                let sections = split_unit_report(&report, &unit.paths);
                let single = unit.paths.len() == 1;
                let missing: Vec<String> =
                    sections.iter().filter(|(_, section)| !single && section.is_none()).map(|(path, _)| path.clone()).collect();
                let files: Vec<FileReviewResult> = sections
                    .into_iter()
                    .map(|(path, section)| {
                        let body = match &section {
                            Some(section) => section.clone(),
                            None if single => report.clone(),
                            None => format!("(no \"### File:\" block for {path} in the unit report)\n\n{report}"),
                        };
                        let parsed = parse_file_report(match &section {
                            Some(section) => section.as_str(),
                            None if single => report.as_str(),
                            None => "",
                        });

                        FileReviewResult {
                            errored: false,
                            p0: parsed.p0,
                            p1: parsed.p1,
                            p2: parsed.p2,
                            part: unit_part_label(&unit),
                            path,
                            rating: parsed.rating,
                            rating_derived: parsed.rating_derived,
                            report: body,
                            session_id: session_id.clone(),
                        }
                    })
                    .collect();

                self.progress(ReviewProgressEvent::UnitDone {
                    counts: sum_counts(&files),
                    elapsed_ms: elapsed_ms(started_at),
                    errored: false,
                    retry: retry_of.is_some(),
                    unit: unit.clone(),
                });

                UnitRun { elapsed_ms: elapsed_ms(started_at), errored: false, files, missing, retry_of, session_id, unit, usage }
            }
            Err(error) => {
                // One failing child must not abort the run — record and continue.
                let description = format!("errored: {error}");

                self.progress(ReviewProgressEvent::UnitDone {
                    counts: ReviewCounts::default(),
                    elapsed_ms: elapsed_ms(started_at),
                    errored: true,
                    retry: retry_of.is_some(),
                    unit: unit.clone(),
                });

                UnitRun {
                    elapsed_ms: elapsed_ms(started_at),
                    errored: true,
                    files: unit.paths.iter().map(|path| errored_file(path, &description)).collect(),
                    missing: Vec::new(),
                    retry_of,
                    session_id: None,
                    unit,
                    usage: ReviewUsage::default(),
                }
            }
        }
    }
}

fn is_skipped_path(path: &str) -> bool {
    if SKIPPED_FILE_NAMES.contains(&path) || SKIPPED_FILE_SUFFIXES.iter().any(|suffix| path.ends_with(suffix)) {
        return true;
    }

    // /(^|\/)[^/]*\.lock$/
    path.rsplit('/').next().is_some_and(|basename| basename.ends_with(".lock"))
}

fn assemble_report(head: String, file_reports: &[String]) -> String {
    [head, String::new(), "## Per-file reports".to_string(), file_reports.join("\n\n")].join("\n")
}

pub fn run_review_command(args: ReviewCommandArgs) -> Result<ReviewOutcome, String> {
    let base_ref = args.base_ref.clone();
    let concurrency = args.concurrency.unwrap_or(DEFAULT_REVIEW_CONCURRENCY);
    let list_changed_files: ListChangedFilesFn = args.list_changed_files.clone().unwrap_or_else(|| Arc::new(default_list_changed_files));
    let read_diff: ReadDiffFn = args.read_diff.clone().unwrap_or_else(|| Arc::new(default_read_diff));
    let read_file_at_head: ReadFileAtHeadFn = args.read_file_at_head.clone().unwrap_or_else(|| Arc::new(default_read_file_at_head));
    let run_goal: RunGoalFn = args.run_goal.clone().unwrap_or_else(|| Arc::new(default_run_goal));

    let all_paths = list_changed_files(&base_ref, &args.cwd)?;
    let reviewable_paths: Vec<String> = all_paths
        .into_iter()
        // Deleted files cannot be read for context; lockfiles, snapshots, and
        // binary noise cannot be reviewed meaningfully.
        .filter(|path| head_has_path(path, &args.cwd) && !is_skipped_path(path))
        .collect();

    if reviewable_paths.is_empty() {
        return Ok(ReviewOutcome {
            base: base_ref.clone(),
            confidence: 5,
            counts: ReviewCounts::default(),
            exit_code: 0,
            files: Vec::new(),
            report: format!("No reviewable changed files against {base_ref}."),
            timing: ReviewTiming { synthesis_ms: 0, total_ms: 0, units_ms: 0 },
            units: Vec::new(),
            usage: ReviewUsage::default(),
        });
    }

    // Diffs are read once, up front: the planner sizes units by diff lines,
    // and the prompt embeds the same text. Small files ride into the prompt
    // whole (see MAX_INLINE_FILE_LINES). A path whose diff cannot be read is
    // one errored file, not a dead review. Bounded like the reviewer pool: two
    // git subprocesses per path, and a wide diff must not fork hundreds at once.
    enum Loaded {
        Ok { path: String, diff: String, content: Option<String> },
        Unreadable(FileReviewResult),
    }

    let loaded = run_pool(reviewable_paths.clone(), 8, |path: String| match read_diff(&base_ref, &path, &args.cwd) {
        Ok(diff) => {
            // Every readable file is kept: the prompt inlines small ones whole
            // and big ones as excerpts around their hunks (see MAX_INLINE_FILE_LINES).
            let content = read_file_at_head(&path, &args.cwd);

            Loaded::Ok { path, diff, content }
        }
        Err(error) => Loaded::Unreadable(errored_file(&path, &format!("errored: could not read the diff ({error})"))),
    });
    let mut diffs: HashMap<String, String> = HashMap::new();
    let mut contents: HashMap<String, String> = HashMap::new();
    let mut unreadable: Vec<FileReviewResult> = Vec::new();

    for entry in loaded {
        match entry {
            Loaded::Ok { path, diff, content } => {
                diffs.insert(path.clone(), diff);

                if let Some(content) = content {
                    contents.insert(path, content);
                }
            }
            Loaded::Unreadable(file) => unreadable.push(file),
        }
    }

    let diff_files: Vec<DiffFile<'_>> = reviewable_paths
        .iter()
        .filter_map(|path| diffs.get(path).map(|diff| DiffFile { path: path.as_str(), diff_lines: count_diff_lines(diff) }))
        .collect();
    let units = chunk_oversized_units(plan_review_units(&diff_files), |path| diffs.get(path).cloned().unwrap_or_default());
    let context = ReviewContext {
        args: &args,
        contents,
        diffs,
        run_goal,
        unit_wall_clock_ms: args.wall_clock_ms.and_then(|clock| clock.unit).unwrap_or(UNIT_WALL_CLOCK_MS),
    };

    context.progress(ReviewProgressEvent::Planned { file_count: reviewable_paths.len(), units: units.clone() });

    let command_started_at = Instant::now();
    let mut first_pass = run_pool(units, concurrency, |unit| context.review_unit(unit, None));

    // One retry for what the first pass did not deliver: a unit that errored
    // or timed out is re-run whole; a unit whose reviewer skipped some files'
    // blocks is re-run for just those files. A retry that fails again stands.
    let retry_units = if args.signal.as_ref().is_some_and(AbortSignal::is_aborted) {
        Vec::new()
    } else {
        let runs: Vec<RetryRun<'_>> =
            first_pass.iter().map(|run| RetryRun { errored: run.errored, missing: &run.missing, unit: &run.unit }).collect();
        let diff_lines_of = |path: &str| context.diffs.get(path).map(|diff| count_diff_lines(diff)).unwrap_or(1);
        let retries = plan_retry_units(&runs, &diff_lines_of);

        for retry in &retries {
            context.progress(ReviewProgressEvent::UnitRetry { reason: retry.reason.clone(), unit: retry.unit.clone() });
        }

        retries
    };
    let retries = run_pool(
        retry_units.iter().map(|entry| (entry.unit.clone(), entry.retry_of.clone())).collect(),
        concurrency,
        |(unit, retry_of)| context.review_unit(unit, Some(retry_of)),
    );
    let mut retry_records: Vec<UnitRun> = Vec::new();

    for (mut retry, entry) in retries.into_iter().zip(retry_units.iter()) {
        let Some(original) = first_pass.get_mut(entry.original_index) else { continue };

        // A successful retry replaces the original's rows for the paths it
        // covered; a failed retry leaves the original's (already errored or
        // unrated) rows in place and is recorded as its own unit.
        if !retry.errored {
            let covered = &retry.unit.paths;

            original.files.retain(|file| !covered.contains(&file.path));
            original.files.append(&mut retry.files);
            original.missing.retain(|path| !covered.contains(path));

            if original.errored && retry.unit.paths.len() == original.unit.paths.len() {
                original.errored = false;
            }
        }

        // The retry's rows (when it succeeded) now live on the original; the
        // retry unit itself is recorded for units[] with no rows of its own.
        retry.files = Vec::new();
        retry_records.push(retry);
    }

    let mut total_usage = ReviewUsage::default();
    let mut unit_results: Vec<ReviewUnitResult> = Vec::new();
    let mut file_results: Vec<FileReviewResult> = unreadable;

    for result in first_pass.into_iter().chain(retry_records) {
        unit_results.push(ReviewUnitResult {
            elapsed_ms: result.elapsed_ms,
            errored: result.errored,
            kind: result.unit.kind,
            label: result.unit.label.clone(),
            paths: result.unit.paths.clone(),
            retry_of: result.retry_of.clone(),
            session_id: result.session_id.clone(),
            usage: result.usage,
        });
        total_usage.add(&result.usage);
        file_results.extend(result.files);
    }

    let units_ms = elapsed_ms(command_started_at);

    // Back to diff order: units are planned docs-first and grouped, but the
    // per-file reports (and files[]) should read in the order the diff lists.
    let order: HashMap<&str, usize> = reviewable_paths.iter().enumerate().map(|(i, path)| (path.as_str(), i)).collect();

    file_results.sort_by_key(|file| order.get(file.path.as_str()).copied().unwrap_or(0));

    let file_reports: Vec<String> = file_results.iter().map(|file| format_file_report(&file.path, &file.report, file.part.as_deref())).collect();
    let counts = sum_counts(&file_results);
    let confidence = confidence_from_counts(counts.p0, counts.p1);
    let counts_report = FileReport { rating: None, rating_derived: false, p0: counts.p0, p1: counts.p1, p2: counts.p2 };
    let exit_code = if counts.p0 == 0 && counts.p1 == 0 { 0 } else { 4 };
    let synth_started_at = Instant::now();
    let synthesis_mode = args.synthesis.unwrap_or(ReviewSynthesisMode::Auto);
    let skip_files: Vec<SynthesisSkipFile<'_>> = file_results
        .iter()
        .map(|file| SynthesisSkipFile {
            errored: file.errored,
            p0: file.p0,
            p1: file.p1,
            p2: file.p2,
            rating: file.rating.as_deref(),
            rating_derived: file.rating_derived,
        })
        .collect();

    if should_skip_synthesis(synthesis_mode, &skip_files) {
        context.progress(ReviewProgressEvent::SynthesisSkipped { mode: synthesis_mode });

        let skipped_files: Vec<SkippedSynthesisFile<'_>> = file_results
            .iter()
            .map(|file| SkippedSynthesisFile {
                path: &file.path,
                errored: file.errored,
                rating: file.rating.as_deref(),
                rating_derived: file.rating_derived,
                report: Some(file.report.as_str()),
            })
            .collect();
        let skipped = build_skipped_synthesis_report(SkippedSynthesisArgs {
            base_ref: &base_ref,
            files: &skipped_files,
            mode: synthesis_mode,
            unit_count: unit_results.len(),
        });

        return Ok(ReviewOutcome {
            base: base_ref,
            confidence,
            counts,
            exit_code,
            files: outcome_files(&file_results),
            report: assemble_report(with_computed_confidence(&skipped, confidence, &counts_report), &file_reports),
            timing: ReviewTiming { synthesis_ms: 0, total_ms: elapsed_ms(command_started_at), units_ms },
            units: unit_results,
            usage: total_usage,
        });
    }

    context.progress(ReviewProgressEvent::SynthesisStart);

    let synthesis_wall_clock_ms = args.wall_clock_ms.and_then(|clock| clock.synthesis).unwrap_or(SYNTHESIS_WALL_CLOCK_MS);
    let synthesize = || -> Result<(String, ReviewUsage), String> {
        let diff_stat = git_stdout(&["diff", "--stat", &format!("{base_ref}...HEAD")], &args.cwd)?;
        let log = git_stdout(&["log", "--oneline", &format!("{base_ref}...HEAD")], &args.cwd)?;
        let synth_prompt = build_synthesis_prompt(SynthesisPromptArgs {
            base_ref: &base_ref,
            context: &args.context,
            diff_stat: &diff_stat,
            log: &log,
            file_reports: &file_reports,
        });
        let (report, _session_id, usage) = context.run_child(
            synth_prompt,
            args.synth_inference.clone(),
            SYNTHESIS_MAX_ITERATIONS,
            None,
            SYNTHESIS_TASK_TITLE.to_string(),
            synthesis_wall_clock_ms,
            looks_like_synthesis_report,
        )?;

        Ok((report, usage))
    };

    // The synthesis model writes its own "## Confidence Score" prose, which is
    // free to contradict the arithmetic. The computed score is authoritative,
    // so it is stamped over the model's section rather than appended beside it.
    let report = match synthesize() {
        Ok((synth_report, synth_usage)) => {
            total_usage.add(&synth_usage);
            context.progress(ReviewProgressEvent::SynthesisDone { elapsed_ms: elapsed_ms(synth_started_at), errored: false });

            assemble_report(with_computed_confidence(&synth_report, confidence, &counts_report), &file_reports)
        }
        Err(error) => {
            // The synthesis child failing must not lose the per-file findings.
            context.progress(ReviewProgressEvent::SynthesisDone { elapsed_ms: elapsed_ms(synth_started_at), errored: true });

            let mut lines = vec![format!("(synthesis pass failed: {error})"), String::new()];

            lines.extend(file_reports.iter().cloned());
            lines.join("\n")
        }
    };

    Ok(ReviewOutcome {
        base: base_ref,
        confidence,
        counts,
        exit_code,
        files: outcome_files(&file_results),
        report,
        timing: ReviewTiming { synthesis_ms: elapsed_ms(synth_started_at), total_ms: elapsed_ms(command_started_at), units_ms },
        units: unit_results,
        usage: total_usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::{
        default_setting_values, ACTIVE_INFERENCE_PROFILE_SETTING_ID, ACTIVE_TOOL_PROFILE_SETTING_ID, MODEL_PROFILES_SETTING_ID,
    };
    use crate::core::inference::resolve_inference_config;

    fn mock_inference() -> ResolvedInferenceConfig {
        let profiles = r#"[{"id":"mock","label":"Mock","model":"m","provider":"openai-compatible","baseUrl":"http://127.0.0.1:9/v1/","apiKeyRef":"env:MOCK_KEY"}]"#;
        let mut settings = default_setting_values();

        settings.insert(MODEL_PROFILES_SETTING_ID.to_string(), profiles.to_string());
        settings.insert(ACTIVE_INFERENCE_PROFILE_SETTING_ID.to_string(), "mock".to_string());
        settings.insert(ACTIVE_TOOL_PROFILE_SETTING_ID.to_string(), String::new());

        let mut env = HashMap::new();

        env.insert("MOCK_KEY".to_string(), "k".to_string());
        resolve_inference_config(&settings, Some(&env)).unwrap()
    }

    fn mock_project(dir: &std::path::Path) -> DripProject {
        let root = dir.to_string_lossy().into_owned();

        DripProject {
            legacy_index_db_path: None,
            legacy_sessions_dir: None,
            home_root: format!("{root}/home"),
            index_db_path: format!("{root}/home/index.sqlite"),
            memory_dir: format!("{root}/home/memory"),
            project_root: Some(root.clone()),
            repo_root: Some(root.clone()),
            repo_slug: "slug".into(),
            root: format!("{root}/.drip"),
            sessions_dir: format!("{root}/home/sessions"),
            slug: "slug".into(),
            worktree_root: Some(root),
        }
    }

    #[test]
    fn run_pool_preserves_item_order_across_lanes() {
        let results = run_pool((0..20).collect(), 4, |n: i32| {
            std::thread::sleep(Duration::from_millis((20 - n as u64) % 5));
            n * 2
        });

        assert_eq!(results, (0..20).map(|n| n * 2).collect::<Vec<_>>());
    }

    #[test]
    fn run_pool_with_no_items_returns_empty() {
        let results: Vec<i32> = run_pool(Vec::<i32>::new(), 4, |n| n);

        assert!(results.is_empty());
    }

    #[test]
    fn timebox_fires_after_deadline_and_reports_timeout() {
        let timebox = Timebox::new(60, None);

        assert!(!timebox.signal.is_aborted());
        std::thread::sleep(Duration::from_millis(250));
        assert!(timebox.signal.is_aborted());
        assert!(timebox.timed_out());
    }

    #[test]
    fn timebox_follows_parent_abort_without_timing_out() {
        let parent = AbortSignal::new();
        let timebox = Timebox::new(10_000, Some(&parent));

        parent.abort();
        std::thread::sleep(Duration::from_millis(200));
        assert!(timebox.signal.is_aborted());
        assert!(!timebox.timed_out());
    }

    #[test]
    fn timebox_released_before_deadline_never_fires() {
        let timebox = Timebox::new(60, None);

        timebox.release();
        std::thread::sleep(Duration::from_millis(200));
        assert!(!timebox.signal.is_aborted());
    }

    #[test]
    fn skipped_paths_match_the_ts_predicate() {
        assert!(is_skipped_path("bun.lock"));
        assert!(is_skipped_path("vendor/Cargo.lock"));
        assert!(is_skipped_path("a/b/x.lock"));
        assert!(is_skipped_path("img/logo.png"));
        assert!(!is_skipped_path("src/lock.ts"));
        assert!(!is_skipped_path("src/main.rs"));
    }

    #[test]
    fn usage_serializes_with_ts_keys_and_integral_seconds() {
        let usage = ReviewUsage { calls: 2, completion_tokens: 30, prompt_tokens: 400, retry_wait_seconds: 0.0 };

        assert_eq!(serde_json::to_string(&usage).unwrap(), r#"{"calls":2,"completionTokens":30,"promptTokens":400,"retryWaitSeconds":0}"#);
    }

    #[test]
    fn outcome_json_matches_ts_key_order() {
        let outcome = ReviewOutcome {
            base: "origin/main".into(),
            confidence: 5,
            counts: ReviewCounts::default(),
            exit_code: 0,
            files: vec![ReviewOutcomeFile { errored: false, p0: 0, p1: 0, p2: 1, part: None, path: "a.rs".into(), rating: Some("⚠️".into()), session_id: Some("s1".into()) }],
            report: "r".into(),
            timing: ReviewTiming { synthesis_ms: 1, total_ms: 3, units_ms: 2 },
            units: vec![ReviewUnitResult {
                elapsed_ms: 2,
                errored: false,
                kind: ReviewUnitKind::Code,
                label: "a.rs".into(),
                paths: vec!["a.rs".into()],
                retry_of: None,
                session_id: Some("s1".into()),
                usage: ReviewUsage::default(),
            }],
            usage: ReviewUsage::default(),
        };
        let json = serde_json::to_string(&outcome).unwrap();

        assert_eq!(
            json,
            r#"{"base":"origin/main","confidence":5,"counts":{"p0":0,"p1":0,"p2":0},"exitCode":0,"files":[{"errored":false,"p0":0,"p1":0,"p2":1,"path":"a.rs","rating":"⚠️","sessionId":"s1"}],"report":"r","timing":{"synthesisMs":1,"totalMs":3,"unitsMs":2},"units":[{"elapsedMs":2,"errored":false,"kind":"code","label":"a.rs","paths":["a.rs"],"sessionId":"s1","usage":{"calls":0,"completionTokens":0,"promptTokens":0,"retryWaitSeconds":0}}],"usage":{"calls":0,"completionTokens":0,"promptTokens":0,"retryWaitSeconds":0}}"#
        );
    }

    #[test]
    fn empty_diff_short_circuits_without_children() {
        let dir = tempfile::tempdir().unwrap();
        let project = mock_project(dir.path());
        let outcome = run_review_command(ReviewCommandArgs {
            base_ref: "origin/main".into(),
            concurrency: None,
            context: "ctx".into(),
            cwd: dir.path().to_string_lossy().into_owned(),
            file_inference: mock_inference(),
            index_db_path: project.index_db_path.clone(),
            project,
            list_changed_files: Some(Arc::new(|_, _| Ok(vec![]))),
            read_diff: None,
            read_file_at_head: None,
            on_progress: None,
            wall_clock_ms: None,
            run_goal: None,
            signal: None,
            skills: vec![],
            synth_inference: mock_inference(),
            synthesis: None,
            tools: Arc::new(Vec::new),
        })
        .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.report, "No reviewable changed files against origin/main.");
        assert!(outcome.units.is_empty());
    }
}
