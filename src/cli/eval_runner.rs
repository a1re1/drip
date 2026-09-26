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
pub fn format_eval_run_human(outcomes: &[EvalRunOutcome]) -> String {
    if outcomes.is_empty() {
        return "No eval cases ran.\n".to_string();
    }

    let mut out = String::new();

    for outcome in outcomes {
        if let Some(error) = &outcome.error {
            out.push_str(&format!(
                "{:<24} {:<6} error: {error}\n",
                outcome.name,
                outcome.kind.as_str()
            ));
            continue;
        }

        let matched = if outcome.scores.is_empty() {
            "none".to_string()
        } else {
            outcome
                .scores
                .iter()
                .map(|entry| format!("{} ({:.2})", entry.name, entry.score))
                .collect::<Vec<_>>()
                .join(", ")
        };
        out.push_str(&format!(
            "{:<24} {:<6} matched: {matched}\n",
            outcome.name,
            outcome.kind.as_str()
        ));

        if !outcome.expected.is_empty() {
            let plural = |count: usize| if count == 1 { "" } else { "es" };
            let agreed = format!(
                "{} match{}",
                outcome.agreement.agreed.len(),
                plural(outcome.agreement.agreed.len())
            );
            out.push_str(&format!(
                "{:<24} {:<6} agreed {} · missed {} · spurious {}\n",
                "",
                "",
                agreed,
                outcome.agreement.missed.len(),
                outcome.agreement.spurious.len(),
            ));
        }

        for warning in &outcome.warnings {
            out.push_str(&format!("{:<24} {:<6} warning: {warning}\n", "", ""));
        }
    }

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
        assert!(listing.contains("missed 1"), "{listing}");
        assert!(listing.contains("spurious 1"), "{listing}");
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
}
