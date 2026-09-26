//! Eval runner: run one eval case through the REAL skill classifier and record
//! what it matched.
//!
//! The runner is deliberately session-free — a throwaway workspace: it opens no
//! session, writes no transcript, takes no lease and touches no run state. It
//! builds the same classifier state a loop would (`goal`, `task`, `phase`,
//! `role`, the loop's tool surface), asks the real classifier the same
//! questions through `select_skills`, and writes the outcome into the case's own
//! `verdict.json` (see `src/cli/evals.rs`) so the eval browser can show a run's
//! matches next to the operator's judgment.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::cli::evals::{
    eval_agreement, load_eval_verdict, save_eval_verdict, EvalAgreement, EvalKind, EvalScenario,
    LoadedEval,
};
use crate::cli::plans::PlanPoolEntry;
use crate::cli::skills::CliSkill;
use crate::harness::classifier::{
    load_skill_classifiers, select_skills, ClassifierRoute, SkillCandidate,
};

/// One candidate the classifier matched, with the score that selected it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvalMatch {
    pub name: String,
    pub score: f64,
}

/// The result of running one case: what the classifier matched, how that
/// compares with the case's declared expectations, and every warning the
/// selection surfaced.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvalRunOutcome {
    pub name: String,
    pub kind: EvalKind,
    pub expected: Vec<String>,
    pub scores: Vec<EvalMatch>,
    /// Every candidate the classifier answered for, by score descending,
    /// including the ones that stayed below threshold: the distance a miss
    /// was from matching is what a tuning pass needs to see.
    #[serde(default)]
    pub considered: Vec<EvalMatch>,
    /// Every question's normalized answer per candidate (see
    /// `SkillSelection::answers`), so a tuning pass sees which question moved
    /// a formula without re-asking the classifier.
    #[serde(default)]
    pub answers: BTreeMap<String, BTreeMap<String, f64>>,
    pub agreement: EvalAgreement,
    #[serde(default)]
    pub warnings: Vec<String>,
    /// Set when the case itself could not run (the classifier never answered,
    /// for instance). A case with an error matched nothing and is never
    /// recorded.
    #[serde(default)]
    pub error: Option<String>,
}

impl EvalRunOutcome {
    /// The matched candidate names, in selection order (authored first, then
    /// unauthored, each by score descending — the order `select_skills`
    /// produces).
    pub fn matched(&self) -> Vec<String> {
        self.scores.iter().map(|entry| entry.name.clone()).collect()
    }
}

/// The classifier's view of a case's scenario: the same shape the loop builds
/// in `select_dynamic_skills` (see `src/harness/loop.rs`), so a case measures
/// the state a real loop really sends. A plan case carries no tool surface at
/// all — plans declare no capability requirements, so the tools cannot change
/// their relevance (matching `select_plans`).
fn eval_state(scenario: &EvalScenario, kind: &EvalKind) -> serde_json::Value {
    let role = scenario
        .role
        .clone()
        .unwrap_or_else(|| kind.default_role().to_string());
    let has_task = scenario.task.is_some();
    let task = scenario.task.as_ref().map(|title| {
        serde_json::json!({
            "id": "eval-task",
            "title": title.clone(),
            "notes": scenario.notes.clone(),
        })
    });

    let mut state = serde_json::json!({
        "goal": scenario.goal.clone(),
        "task": task,
        "phase": if has_task { "task" } else { "planning" },
        "role": role,
    });

    if *kind != EvalKind::Plan {
        state["availableTools"] = serde_json::json!(eval_tools(scenario));
    }

    state
}

/// The tool surface a case runs against: the scenario's own list when it names
/// one, else the shipped built-in pack plus DELEGATE — the surface a loop that
/// spawned no MCP server offers.
fn eval_tools(scenario: &EvalScenario) -> Vec<String> {
    if !scenario.tools.is_empty() {
        return scenario.tools.clone();
    }

    let mut tools: Vec<String> = crate::tools::pack::BUILTIN_TOOL_NAMES
        .iter()
        .map(|name| name.to_string())
        .collect();
    tools.push("DELEGATE".to_string());
    tools
}

/// Runs one case through the real classifier.
///
/// `candidates` is the pool this case offers — built by `skill_candidates` or
/// `plan_candidates`, in the scenario's own order so a case's expectations are
/// stated against a stable pool. Only a name this pool actually offered can
/// match: the gate is structural, exactly like the loop's.
pub async fn run_eval(
    route: &ClassifierRoute,
    loaded: &LoadedEval,
    candidates: &[SkillCandidate],
) -> EvalRunOutcome {
    let mut outcome = EvalRunOutcome {
        name: loaded.name.clone(),
        kind: loaded.case.kind.clone(),
        expected: loaded.case.expected.clone(),
        ..Default::default()
    };

    if candidates.is_empty() {
        outcome
            .warnings
            .push("no candidates are installed for this case".to_string());
        return outcome;
    }

    let state = eval_state(&loaded.scenario, &loaded.case.kind);
    let selection = select_skills(route, state, candidates).await;
    outcome.warnings = selection.warnings.clone();

    for (name, score) in selection.selected {
        if !candidates.iter().any(|candidate| candidate.name == name) {
            continue;
        }
        outcome.scores.push(EvalMatch { name, score });
    }

    for (name, score) in selection.scored {
        if !candidates.iter().any(|candidate| candidate.name == name) {
            continue;
        }
        outcome.considered.push(EvalMatch { name, score });
    }

    outcome.answers = selection
        .answers
        .into_iter()
        .filter(|(name, _)| candidates.iter().any(|candidate| candidate.name == *name))
        .collect();

    outcome.agreement = eval_agreement(&outcome.matched(), &outcome.expected);
    outcome
}

/// Records a run's matches next to the case, at the case's own scope: the
/// operator's judgment (`applicable` / `notApplicable` / `judgedAt`) is
/// preserved, only `matched` changes.
pub fn record_eval_run(dir: &Path, outcome: &EvalRunOutcome) -> std::io::Result<()> {
    let mut verdict = load_eval_verdict(dir).unwrap_or_default();
    verdict.matched = outcome.matched();
    save_eval_verdict(dir, &verdict)
}

/// The candidate pool a skill case offers: every discovered skill when the
/// scenario names none, else exactly the names it names, in that order. A
/// declared name that is not installed is reported as missing — the case is
/// still run against what IS installed, never silently against a different
/// pool.
#[allow(clippy::type_complexity)]
pub fn skill_candidates(
    discovered: &[CliSkill],
    declared: &[String],
) -> (Vec<SkillCandidate>, Vec<String>) {
    candidates_in_order(
        declared,
        discovered.len(),
        |index| {
            let skill = &discovered[index];
            SkillCandidate {
                name: skill.name.clone(),
                description: skill.description.clone(),
                classifiers: load_skill_classifiers(&skill.path).and_then(Result::ok),
            }
        },
        |declared_name| {
            discovered
                .iter()
                .position(|skill| skill.name == *declared_name)
        },
    )
}

/// The candidate pool a plan case offers, over the plan pool the planner would
/// build for the same cwd and home (see `crate::cli::plans::build_plan_pool`).
pub fn plan_candidates(
    pool: &[PlanPoolEntry],
    declared: &[String],
) -> (Vec<SkillCandidate>, Vec<String>) {
    candidates_in_order(
        declared,
        pool.len(),
        |index| {
            let plan = &pool[index];
            SkillCandidate {
                name: plan.name.clone(),
                description: plan.description.clone(),
                classifiers: plan.classifiers.clone(),
            }
        },
        |declared_name| pool.iter().position(|plan| plan.name == *declared_name),
    )
}

/// Shared pool assembly: `declared` empty means every entry, in catalog order;
/// otherwise the declared order, dropping (and reporting) names that are not
/// installed.
fn candidates_in_order<F, G>(
    declared: &[String],
    count: usize,
    build: F,
    find: G,
) -> (Vec<SkillCandidate>, Vec<String>)
where
    F: Fn(usize) -> SkillCandidate,
    G: Fn(&str) -> Option<usize>,
{
    if declared.is_empty() {
        return ((0..count).map(&build).collect(), Vec::new());
    }

    let mut candidates: Vec<SkillCandidate> = Vec::new();
    let mut missing: Vec<String> = Vec::new();

    for name in declared {
        match find(name) {
            Some(index) => candidates.push(build(index)),
            None => missing.push(name.clone()),
        }
    }

    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    candidates.retain(|candidate| seen.insert(candidate.name.clone()));

    (candidates, missing)
}

/// The operator-facing run listing.
/// The whole run scored as one number set. A case passes when the matched set
/// equals its expected set exactly (a case that expects nothing passes only
/// when nothing matched); precision and recall are pooled over every run so a
/// suite of many small cases reads like one classifier measurement.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvalRunSummary {
    /// Distinct case names that ran.
    pub cases: usize,
    /// Case runs in total (`cases` × the repeat count, minus nothing).
    pub runs: usize,
    /// Runs whose matched set equalled the expected set.
    pub passed: usize,
    /// Runs that could not be answered at all.
    pub errors: usize,
    pub agreed: usize,
    pub missed: usize,
    pub spurious: usize,
    /// agreed / (agreed + spurious); 1.0 when nothing matched at all.
    pub precision: f64,
    /// agreed / (agreed + missed); 1.0 when nothing was expected at all.
    pub recall: f64,
    /// Cases whose runs did not all agree with each other (repeat runs only),
    /// as (name, passed runs, total runs).
    pub unstable: Vec<(String, usize, usize)>,
}

/// One run passes when it matched exactly what it expected.
pub fn eval_run_passes(outcome: &EvalRunOutcome) -> bool {
    outcome.error.is_none()
        && outcome.agreement.missed.is_empty()
        && outcome.agreement.spurious.is_empty()
}

pub fn summarize_eval_run(outcomes: &[EvalRunOutcome]) -> EvalRunSummary {
    let mut summary = EvalRunSummary::default();
    let mut per_case: Vec<(String, usize, usize)> = Vec::new();

    for outcome in outcomes {
        summary.runs += 1;
        let passed = eval_run_passes(outcome);

        if outcome.error.is_some() {
            summary.errors += 1;
        }
        if passed {
            summary.passed += 1;
        }
        summary.agreed += outcome.agreement.agreed.len();
        summary.missed += outcome.agreement.missed.len();
        summary.spurious += outcome.agreement.spurious.len();

        match per_case.iter_mut().find(|(name, _, _)| *name == outcome.name) {
            Some((_, ok, total)) => {
                *ok += usize::from(passed);
                *total += 1;
            }
            None => per_case.push((outcome.name.clone(), usize::from(passed), 1)),
        }
    }

    summary.cases = per_case.len();
    summary.precision = ratio(summary.agreed, summary.agreed + summary.spurious);
    summary.recall = ratio(summary.agreed, summary.agreed + summary.missed);
    summary.unstable = per_case
        .into_iter()
        .filter(|(_, ok, total)| *total > 1 && *ok != 0 && ok != total)
        .collect();

    summary
}

fn ratio(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        1.0
    } else {
        numerator as f64 / denominator as f64
    }
}

fn format_scores(entries: &[EvalMatch]) -> String {
    if entries.is_empty() {
        return "none".to_string();
    }

    entries
        .iter()
        .map(|entry| format!("{} ({:.2})", entry.name, entry.score))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The verdict a run's block and its progress line both print: a case that
/// expected nothing and matched nothing passes trivially, otherwise it passes
/// only when it matched exactly what it expected.
fn eval_verdict(outcome: &EvalRunOutcome) -> &'static str {
    if (outcome.expected.is_empty() && outcome.scores.is_empty()) || eval_run_passes(outcome) {
        "PASS"
    } else {
        "FAIL"
    }
}

/// The per-outcome block: one case run as the report prints it. `repeat_index`
/// is the zero-based index of this run among the runs of the same case name
/// (`0` is the first), so a later repeat is labelled `run 2`, `run 3`, ... The
/// first run carries the case name.
pub fn format_eval_outcome_human(outcome: &EvalRunOutcome, repeat_index: usize) -> String {
    let label = if repeat_index == 0 {
        outcome.name.clone()
    } else {
        format!("  run {}", repeat_index + 1)
    };

    let mut out = String::new();

    if let Some(error) = &outcome.error {
        out.push_str(&format!(
            "{:<24} {:<6} error: {error}\n",
            label,
            outcome.kind.as_str()
        ));
        return out;
    }

    let verdict = eval_verdict(outcome);
    out.push_str(&format!(
        "{:<24} {:<6} {verdict}  matched: {}\n",
        label,
        outcome.kind.as_str(),
        format_scores(&outcome.scores)
    ));

    let below: Vec<EvalMatch> = outcome
        .considered
        .iter()
        .filter(|entry| !outcome.scores.iter().any(|hit| hit.name == entry.name))
        .cloned()
        .collect();
    if !below.is_empty() {
        // The nearest misses only: an unscoped pool answers for every
        // installed skill, and the tail says nothing a tuning pass needs.
        const SHOWN: usize = 8;
        let more = below.len().saturating_sub(SHOWN);
        let mut line = format_scores(&below[..below.len().min(SHOWN)]);
        if more > 0 {
            line.push_str(&format!(", +{more} more"));
        }
        out.push_str(&format!("{:<24} {:<6}       below: {line}\n", "", ""));
    }

    // The answers behind the candidates that matter to this case: every
    // expected one and every one that matched.
    for (name, answers) in &outcome.answers {
        let relevant = outcome.expected.iter().any(|expected| expected == name)
            || outcome.scores.iter().any(|hit| hit.name == *name);
        if !relevant || answers.is_empty() {
            continue;
        }
        let rendered = answers
            .iter()
            .map(|(question, value)| format!("{question}={value:.2}"))
            .collect::<Vec<_>>()
            .join(" ");
        out.push_str(&format!("{:<24} {:<6}       {name}: {rendered}\n", "", ""));
    }

    if !outcome.expected.is_empty() || !outcome.scores.is_empty() {
        let mut parts: Vec<String> = Vec::new();
        if !outcome.agreement.missed.is_empty() {
            parts.push(format!("missed {}", outcome.agreement.missed.join(", ")));
        }
        if !outcome.agreement.spurious.is_empty() {
            parts.push(format!("spurious {}", outcome.agreement.spurious.join(", ")));
        }
        if parts.is_empty() {
            parts.push(format!(
                "agreed {} of {}",
                outcome.agreement.agreed.len(),
                outcome.expected.len()
            ));
        }
        out.push_str(&format!(
            "{:<24} {:<6}       {}\n",
            "",
            "",
            parts.join(" · ")
        ));
    }

    for warning in &outcome.warnings {
        out.push_str(&format!("{:<24} {:<6}       warning: {warning}\n", "", ""));
    }

    out
}

/// The one-line-per-run progress report written to stderr while `--json` keeps
/// stdout a single JSON document. `index` is this run's 1-based position in the
/// whole suite and `total` the suite's run count.
pub fn format_eval_progress_line(
    outcome: &EvalRunOutcome,
    repeat_index: usize,
    index: usize,
    total: usize,
) -> String {
    let name = if repeat_index == 0 {
        outcome.name.clone()
    } else {
        format!("{} (run {})", outcome.name, repeat_index + 1)
    };

    let verdict = if let Some(error) = &outcome.error {
        format!("error: {error}")
    } else {
        eval_verdict(outcome).to_string()
    };

    format!("[{index}/{total}] {name} - {verdict}\n")
}

/// One streamed run: the `[index/total]` progress prefix plus the run's block,
/// with every line after the first indented to the prefix's width so the
/// report's columns stay aligned when the blocks are printed as they arrive.
pub fn format_eval_progress_block(
    outcome: &EvalRunOutcome,
    repeat_index: usize,
    index: usize,
    total: usize,
) -> String {
    let prefix = format!("[{index}/{total}] ");
    let indent = " ".repeat(prefix.len());
    let block = format_eval_outcome_human(outcome, repeat_index);

    let mut out = String::new();
    for (position, line) in block.lines().enumerate() {
        out.push_str(if position == 0 {
            prefix.as_str()
        } else {
            indent.as_str()
        });
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// The pooled closing block: the summary line plus any unstable cases. It is
/// printed once, after the per-run blocks.
pub fn format_eval_summary_human(outcomes: &[EvalRunOutcome]) -> String {
    let summary = summarize_eval_run(outcomes);
    let mut out = String::new();
    out.push_str(&format!(
        "summary: {}/{} runs pass across {} case{} · precision {:.2} · recall {:.2}",
        summary.passed,
        summary.runs,
        summary.cases,
        if summary.cases == 1 { "" } else { "s" },
        summary.precision,
        summary.recall,
    ));
    if summary.errors > 0 {
        out.push_str(&format!(
            " · {} error{}",
            summary.errors,
            if summary.errors == 1 { "" } else { "s" }
        ));
    }
    out.push('\n');
    if !summary.unstable.is_empty() {
        out.push_str(&format!(
            "unstable: {}\n",
            summary
                .unstable
                .iter()
                .map(|(name, ok, total)| format!("{name} ({ok}/{total})"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    out
}

/// The whole report: every run's block in order, then the pooled summary. It is
/// the composition of `format_eval_outcome_human` and
/// `format_eval_summary_human`, so a streaming run prints exactly what this
/// one-shot report shows.
///
/// `--run-evals` streams the blocks itself and calls the two halves directly,
/// so this is the report as one string — used by tests and any one-shot caller;
/// the empty-suite message below is its zero-outcome case (the CLI never gets
/// there, it exits before the loop when no case is discovered).
pub fn format_eval_run_human(outcomes: &[EvalRunOutcome]) -> String {
    if outcomes.is_empty() {
        return "No eval cases ran.\n".to_string();
    }

    let mut out = String::new();
    let mut seen: Vec<String> = Vec::new();

    for outcome in outcomes {
        // A repeated case prints its later runs under the first one, labelled
        // by run number, so the eye lands on the name once.
        let repeat = seen.iter().filter(|name| **name == outcome.name).count();
        seen.push(outcome.name.clone());
        out.push_str(&format_eval_outcome_human(outcome, repeat));
    }

    out.push('\n');
    out.push_str(&format_eval_summary_human(outcomes));

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// HTTP/1.1 mock over a std TcpListener: one canned response per accepted
    /// connection, written back on the SAME stream the request arrived on. The
    /// accept loop is bounded by a deadline so a request that never comes fails
    /// the test's assertions instead of hanging the suite.
    fn spawn_mock(bodies: Vec<&'static str>) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let mut seen: Vec<String> = Vec::new();

            for body in bodies {
                let accepted = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break Some(stream),
                        Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            if std::time::Instant::now() >= deadline {
                                break None;
                            }
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                        Err(_) => break None,
                    }
                };

                let Some(mut stream) = accepted else { break };
                seen.push(read_request(&mut stream));
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                std::io::Write::write_all(&mut stream, response.as_bytes()).unwrap();
                let _ = std::io::Write::flush(&mut stream);
            }

            seen
        });

        (format!("http://127.0.0.1:{port}"), handle)
    }

    /// Reads one full HTTP/1.1 request: headers, then exactly `content-length`
    /// bytes of body.
    fn read_request(stream: &mut std::net::TcpStream) -> String {
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut data: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 4096];

        let body_start = loop {
            let read = std::io::Read::read(stream, &mut chunk).unwrap_or(0);
            assert!(read > 0, "client closed before sending a full request");
            data.extend_from_slice(&chunk[..read]);

            if let Some(pos) = data.windows(4).position(|window| window == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&data[..pos]).to_ascii_lowercase();
                let length = head
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("content-length:")
                            .and_then(|value| value.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);

                if data.len() >= pos + 4 + length {
                    break pos + 4;
                }
            }
        };

        String::from_utf8_lossy(&data[body_start..]).to_string()
    }

    fn hand_route(base: &str) -> ClassifierRoute {
        ClassifierRoute {
            url: format!("{base}/alpha/decisions"),
            model: "~typesafe/jev-latest".to_string(),
            headers: vec![("Authorization".to_string(), "Bearer test-key".to_string())],
            timeout_ms: 10_000,
        }
    }

    /// The response body must outlive the request, so it is leaked into a
    /// 'static &str for the mock server (test-only, one small string).
    fn leak(body: String) -> &'static str {
        Box::leak(body.into_boxed_str())
    }

    fn noul_response(entries: &[(&str, f64)]) -> String {
        let answers: Vec<String> = entries
            .iter()
            .map(|(id, value)| format!("\"{id}\":{{\"type\":\"noul\",\"noul\":{value}}}"))
            .collect();
        format!(
            "{{\"model\":\"jev\",\"answers\":{{{}}}}}",
            answers.join(",")
        )
    }

    fn unauthored(name: &str) -> SkillCandidate {
        SkillCandidate {
            name: name.to_string(),
            description: format!("{name} description"),
            classifiers: None,
        }
    }

    /// Writes a case directory and loads it back through the discovery +
    /// loading path the CLI uses.
    fn write_and_load(dir: &Path, case: &str, scenario: &str) -> LoadedEval {
        let case_dir = dir.join("flaky");
        std::fs::create_dir_all(&case_dir).unwrap();
        std::fs::write(case_dir.join("case.json"), case).unwrap();
        std::fs::write(case_dir.join("scenario.json"), scenario).unwrap();

        let discovered = crate::cli::evals::discover_evals(dir, dir);
        assert_eq!(discovered.len(), 1, "the case is discovered");
        crate::cli::evals::load_eval(&discovered[0]).unwrap()
    }

    fn outcome(name: &str, expected: &[&str], matched: &[&str]) -> EvalRunOutcome {
        let expected: Vec<String> = expected.iter().map(|s| s.to_string()).collect();
        let matched: Vec<String> = matched.iter().map(|s| s.to_string()).collect();
        EvalRunOutcome {
            name: name.to_string(),
            expected: expected.clone(),
            scores: matched
                .iter()
                .map(|name| EvalMatch {
                    name: name.clone(),
                    score: 0.9,
                })
                .collect(),
            agreement: eval_agreement(&matched, &expected),
            ..Default::default()
        }
    }

    #[test]
    fn the_summary_pools_agreement_and_flags_cases_whose_repeats_disagree() {
        let outcomes = vec![
            outcome("a", &["tdd", "verify-before-done"], &["tdd", "verify-before-done"]),
            outcome("a", &["tdd", "verify-before-done"], &["tdd"]),
            outcome("b", &[], &[]),
            outcome("b", &[], &[]),
            outcome("c", &["tdd"], &["praeparare"]),
            EvalRunOutcome {
                name: "d".to_string(),
                error: Some("no answer".to_string()),
                ..Default::default()
            },
        ];

        let summary = summarize_eval_run(&outcomes);
        assert_eq!(summary.cases, 4);
        assert_eq!(summary.runs, 6);
        assert_eq!(summary.passed, 3);
        assert_eq!(summary.errors, 1);
        assert_eq!((summary.agreed, summary.missed, summary.spurious), (3, 2, 1));
        assert!((summary.precision - 0.75).abs() < 1e-9);
        assert!((summary.recall - 0.6).abs() < 1e-9);
        assert_eq!(summary.unstable, vec![("a".to_string(), 1, 2)]);

        let listing = format_eval_run_human(&outcomes);
        assert!(listing.contains("a                        prompt PASS"), "{listing}");
        assert!(listing.contains("  run 2                  prompt FAIL"), "{listing}");
        assert!(listing.contains("b                        prompt PASS  matched: none"), "{listing}");
        assert!(listing.contains("missed tdd · spurious praeparare"), "{listing}");
        assert!(listing.contains("d                        prompt error: no answer"), "{listing}");
        assert!(
            listing.contains("summary: 3/6 runs pass across 4 cases · precision 0.75 · recall 0.60 · 1 error"),
            "{listing}"
        );
        assert!(listing.contains("unstable: a (1/2)"), "{listing}");
    }

    #[test]
    fn an_empty_expectation_passes_only_when_nothing_matched() {
        assert!(eval_run_passes(&outcome("quiet", &[], &[])));
        assert!(!eval_run_passes(&outcome("noisy", &[], &["tdd"])));
        let summary = summarize_eval_run(&[outcome("quiet", &[], &[])]);
        assert!((summary.precision - 1.0).abs() < 1e-9);
        assert!((summary.recall - 1.0).abs() < 1e-9);
    }

    #[test]
    fn the_state_matches_the_shape_a_loop_sends() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = write_and_load(
            dir.path(),
            r#"{"kind":"task","expected":["tdd"]}"#,
            r#"{"goal":"fix the flake","task":"task-1: reproduce it",
                "notes":["it is a SIGTERM"],"role":"author"}"#,
        );

        let state = eval_state(&loaded.scenario, &loaded.case.kind);
        assert_eq!(state["goal"], "fix the flake");
        assert_eq!(state["phase"], "task");
        assert_eq!(state["role"], "author");
        assert_eq!(state["task"]["title"], "task-1: reproduce it");
        assert_eq!(state["task"]["notes"][0], "it is a SIGTERM");
        // `task: null` for a bare prompt, and no tool list for a plan case.
        let prompt = EvalScenario {
            goal: "fix the flake".to_string(),
            ..Default::default()
        };
        let state = eval_state(&prompt, &EvalKind::Prompt);
        assert!(state["task"].is_null());
        assert_eq!(state["phase"], "planning");
        assert_eq!(state["role"], "author");
        assert!(state["availableTools"][0] == "READ");

        let state = eval_state(&prompt, &EvalKind::Plan);
        assert_eq!(state["role"], "planner");
        assert!(state.get("availableTools").is_none());
    }

    #[tokio::test]
    async fn a_case_runs_the_real_classifier_and_records_its_matches() {
        // Two unauthored candidates share ONE batched request; tdd clears the
        // 0.6 threshold and verify-before-done does not.
        let (base, server) = spawn_mock(vec![leak(noul_response(&[
            ("skill_0", 0.9),
            ("skill_1", 0.3),
        ]))]);
        let route = hand_route(&base);
        let dir = tempfile::tempdir().unwrap();
        let loaded = write_and_load(
            dir.path(),
            r#"{"kind":"task","expected":["tdd"]}"#,
            r#"{"goal":"fix the flake","task":"task-1: reproduce it"}"#,
        );
        let candidates = vec![unauthored("tdd"), unauthored("verify-before-done")];

        let outcome = run_eval(&route, &loaded, &candidates).await;

        assert!(outcome.error.is_none());
        assert_eq!(outcome.matched(), vec!["tdd"]);
        assert_eq!(outcome.scores[0].score, 0.9);
        assert_eq!(outcome.agreement.agreed, vec!["tdd"]);
        assert!(outcome.agreement.missed.is_empty());
        assert!(outcome.agreement.spurious.is_empty());
        assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);

        // The run is recorded INSIDE the case directory, preserving any
        // operator judgment already there.
        let case_dir = dir.path().join("flaky");
        save_eval_verdict(
            &case_dir,
            &crate::cli::evals::EvalVerdict {
                applicable: vec!["tdd".to_string()],
                ..Default::default()
            },
        )
        .unwrap();
        record_eval_run(&case_dir, &outcome).unwrap();

        let verdict = load_eval_verdict(&case_dir).unwrap();
        assert_eq!(verdict.matched, vec!["tdd"]);
        assert_eq!(
            verdict.applicable,
            vec!["tdd"],
            "recording a run never erases the operator's judgment"
        );
        assert!(format_eval_run_human(&[outcome]).contains("matched: tdd (0.90)"));

        let bodies = server.join().unwrap();
        assert_eq!(
            bodies.len(),
            1,
            "one batched request for the unauthored pool"
        );
        assert!(bodies[0].contains("fix the flake"), "{}", bodies[0]);
    }

    #[tokio::test]
    async fn a_case_reports_a_missing_expected_candidate_as_a_miss() {
        let (base, server) = spawn_mock(vec![leak(noul_response(&[
            ("skill_0", 0.9),
            ("skill_1", 0.1),
        ]))]);
        let route = hand_route(&base);
        let dir = tempfile::tempdir().unwrap();
        let loaded = write_and_load(
            dir.path(),
            r#"{"kind":"task","expected":["verify-before-done"]}"#,
            r#"{"goal":"fix the flake","task":"task-1"}"#,
        );

        let outcome = run_eval(
            &route,
            &loaded,
            &[unauthored("tdd"), unauthored("verify-before-done")],
        )
        .await;

        assert_eq!(outcome.matched(), vec!["tdd"]);
        assert_eq!(outcome.agreement.missed, vec!["verify-before-done"]);
        assert_eq!(outcome.agreement.spurious, vec!["tdd"]);
        let listing = format_eval_run_human(&[outcome]);
        assert!(listing.contains("FAIL"), "{listing}");
        assert!(listing.contains("missed verify-before-done"), "{listing}");
        assert!(listing.contains("spurious tdd"), "{listing}");
        assert!(listing.contains("summary: 0/1 runs pass across 1 case"), "{listing}");
        server.join().unwrap();
    }

    #[test]
    fn a_declared_pool_keeps_the_scenarios_order_and_reports_missing_names() {
        let pool: Vec<PlanPoolEntry> = ["ship-pr", "triage", "refactor"]
            .iter()
            .map(|name| PlanPoolEntry {
                name: name.to_string(),
                description: format!("{name} description"),
                content: String::new(),
                classifiers: None,
            })
            .collect();

        let (candidates, missing) = plan_candidates(
            &pool,
            &[
                "refactor".to_string(),
                "ship-pr".to_string(),
                "nope".to_string(),
                "refactor".to_string(),
            ],
        );
        assert_eq!(
            candidates
                .iter()
                .map(|candidate| candidate.name.as_str())
                .collect::<Vec<_>>(),
            vec!["refactor", "ship-pr"],
            "declared order wins and duplicates collapse"
        );
        assert_eq!(missing, vec!["nope"]);

        // No declared pool: every installed entry, in catalog order.
        let (candidates, missing) = plan_candidates(&pool, &[]);
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0].name, "ship-pr");
        assert!(missing.is_empty());
    }

    #[test]
    fn an_empty_pool_is_reported_and_never_recorded_as_a_match() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = write_and_load(
            dir.path(),
            r#"{"kind":"prompt","expected":["tdd"]}"#,
            r#"{"goal":"fix the flake"}"#,
        );

        let outcome = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(run_eval(&hand_route("http://127.0.0.1:1"), &loaded, &[]));

        assert!(outcome.scores.is_empty());
        assert_eq!(
            outcome.warnings,
            vec!["no candidates are installed for this case"]
        );
        assert!(format_eval_run_human(&[outcome]).contains("matched: none"));
    }
    #[test]
    fn the_per_run_block_and_summary_compose_the_whole_report() {
        let outcomes = vec![
            outcome("a", &["tdd"], &["tdd"]),
            outcome("a", &["tdd"], &["praeparare"]),
            EvalRunOutcome {
                name: "b".to_string(),
                error: Some("no answer".to_string()),
                ..Default::default()
            },
        ];

        let mut composed = String::new();
        let mut seen: Vec<String> = Vec::new();
        for outcome in &outcomes {
            let repeat = seen.iter().filter(|name| **name == outcome.name).count();
            seen.push(outcome.name.clone());
            composed.push_str(&format_eval_outcome_human(outcome, repeat));
        }
        composed.push('\n');
        composed.push_str(&format_eval_summary_human(&outcomes));

        assert_eq!(composed, format_eval_run_human(&outcomes));
    }

    #[test]
    fn an_empty_report_says_no_case_ran() {
        assert_eq!(format_eval_run_human(&[]), "No eval cases ran.\n");
    }

    #[test]
    fn a_repeat_run_is_labelled_run_twice() {
        let first = outcome("a", &["tdd"], &["tdd"]);
        let second = outcome("a", &["tdd"], &["tdd"]);

        let first_block = format_eval_outcome_human(&first, 0);
        assert!(first_block.starts_with("a "), "{first_block}");

        let second_block = format_eval_outcome_human(&second, 1);
        assert!(second_block.contains("run 2"), "{second_block}");
        assert!(!second_block.contains("run 1"), "{second_block}");
    }

    #[test]
    fn the_progress_line_names_the_case_verdict_and_position() {
        let passing = format_eval_progress_line(&outcome("a", &["tdd"], &["tdd"]), 0, 1, 4);
        assert!(passing.contains("[1/4]"), "{passing}");
        assert!(passing.contains("a - PASS"), "{passing}");

        let failing = format_eval_progress_line(&outcome("b", &["tdd"], &["praeparare"]), 0, 3, 4);
        assert!(failing.contains("[3/4]"), "{failing}");
        assert!(failing.contains("b - FAIL"), "{failing}");

        let repeat = format_eval_progress_line(&outcome("b", &["tdd"], &["tdd"]), 1, 4, 4);
        assert!(repeat.contains("b (run 2) - PASS"), "{repeat}");

        let errored = EvalRunOutcome {
            name: "c".to_string(),
            error: Some("no answer".to_string()),
            ..Default::default()
        };
        let line = format_eval_progress_line(&errored, 0, 4, 4);
        assert!(line.contains("error: no answer"), "{line}");
    }

    #[test]
    fn a_streamed_block_indents_every_line_under_the_prefix() {
        let outcome = EvalRunOutcome {
            name: "a".to_string(),
            expected: vec!["tdd".to_string()],
            scores: vec![EvalMatch {
                name: "praeparare".to_string(),
                score: 0.4,
            }],
            considered: vec![
                EvalMatch {
                    name: "praeparare".to_string(),
                    score: 0.4,
                },
                EvalMatch {
                    name: "tdd".to_string(),
                    score: 0.35,
                },
            ],
            warnings: vec!["careful".to_string()],
            ..Default::default()
        };

        let prefix = "[2/5] ";
        let block = format_eval_progress_block(&outcome, 0, 2, 5);
        let lines: Vec<&str> = block.lines().collect();

        assert!(lines[0].starts_with(prefix), "{block}");
        assert!(lines.len() > 1, "the case has continuation lines: {block}");
        for line in &lines[1..] {
            assert!(
                line.starts_with(&" ".repeat(prefix.len())),
                "continuation lines align under the prefix: {block}"
            );
        }
        assert!(lines.iter().any(|line| line.contains("below:")), "{block}");
        assert!(lines.iter().any(|line| line.contains("warning:")), "{block}");
    }
}
