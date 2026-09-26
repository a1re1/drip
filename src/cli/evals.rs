//! Eval cases: reusable scenarios the skill classifier is measured against.
//!
//! An eval case is a directory holding two files:
//!
//! - `case.json` — the case's identity: `name`, `description`, `kind`
//!   (`prompt`, `task`, or `plan`), the candidates the operator EXPECTS to be
//!   applicable, and, when it was lifted out of a real run, its provenance.
//! - `scenario.json` — the loop context the classifier is given: the goal, the
//!   task title and its notes, the role, the tool surface, and an optional
//!   explicit candidate pool.
//!
//! Storage mirrors skills and plans:
//!
//! - `<home>/evals/<name>/` — user scope, shared by every project.
//! - `<cwd>/.drip/evals/<name>/` — project scope, shadowing the user's by name.
//!
//! A third file, `verdict.json`, is where the eval browser records the
//! operator's judgment after a run: which of the matched candidates were
//! actually applicable. It is written by the tool, never authored by hand.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// `case.json` — the file every eval-case directory must hold.
pub const EVAL_CASE_FILE_NAME: &str = "case.json";
/// `scenario.json` — the classifier input a case is run against.
pub const EVAL_SCENARIO_FILE_NAME: &str = "scenario.json";
/// `verdict.json` — the operator's judgment, written by the eval browser.
pub const EVAL_VERDICT_FILE_NAME: &str = "verdict.json";
/// Marker recording which shipped starter cases were already seeded into a
/// home. It sits beside `evals/`, never inside it, so deleting a case does not
/// resurrect the template on the next start.
pub const DEFAULT_EVALS_MARKER_FILE: &str = "default-evals.json";

/// One shipped case: its files are embedded at compile time from
/// `evals/cases/<name>/`, so adding a case is adding a directory there and one
/// line to `builtin_eval_entries`.
macro_rules! builtin_case {
    ($name:literal) => {
        (
            $name,
            include_str!(concat!("../../evals/cases/", $name, "/case.json")),
            include_str!(concat!("../../evals/cases/", $name, "/scenario.json")),
        )
    };
}

// ---------------------------------------------------------------------------
// Starter case pack — templates embedded at compile time and copied into
// <home>/evals on the first start. Nothing here is discovered automatically:
// a deleted starter case stays deleted.
// ---------------------------------------------------------------------------

/// The shipped starter cases as (name, case.json, scenario.json) triples, in
/// sorted order. Every skill case scopes its `candidates` to the shipped
/// default skills and every plan case to the shipped plans, so the pack scores
/// the same on any machine with the defaults installed.
fn builtin_eval_entries() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        builtin_case!("commit-and-push"),
        builtin_case!("dependency-migration"),
        builtin_case!("explain-btree"),
        builtin_case!("explain-classifier-plan"),
        builtin_case!("feature-to-pr-plan"),
        builtin_case!("feature-with-tests"),
        builtin_case!("flaky-test-fixup"),
        builtin_case!("git-precommit-hook"),
        builtin_case!("list-open-prs"),
        builtin_case!("mutex-vs-rwlock"),
        builtin_case!("pattern-migration"),
        builtin_case!("prepare-draft-pr"),
        builtin_case!("rebase-and-force-push"),
        builtin_case!("rename-for-clarity"),
        builtin_case!("review-finished-task"),
        builtin_case!("review-pr-diff"),
        builtin_case!("session-end-notification"),
        builtin_case!("ship-this-branch"),
        builtin_case!("split-large-module"),
        builtin_case!("startup-panic"),
        builtin_case!("summarize-open-prs-plan"),
        builtin_case!("tune-classifier"),
        builtin_case!("what-does-flag-do"),
    ]
}

/// The names of every starter case this binary ships, in sorted order.
pub fn default_eval_names() -> Vec<String> {
    builtin_eval_entries()
        .into_iter()
        .map(|(name, _, _)| name.to_string())
        .collect()
}

/// The shipped files for `name` as (case.json, scenario.json).
pub fn default_eval_template(name: &str) -> Option<(&'static str, &'static str)> {
    builtin_eval_entries()
        .into_iter()
        .find(|(entry_name, _, _)| *entry_name == name)
        .map(|(_, case, scenario)| (case, scenario))
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct DefaultEvalsMarker {
    #[serde(default)]
    installed: Vec<String>,
    #[serde(default)]
    version: u32,
}

/// `<home_root>/default-evals.json` — the seeding marker's absolute path.
pub fn default_evals_marker_path(home_root: &str) -> PathBuf {
    Path::new(home_root).join(DEFAULT_EVALS_MARKER_FILE)
}

fn read_default_evals_marker(path: &Path) -> DefaultEvalsMarker {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn write_default_evals_marker(path: &Path, installed: &[String]) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let marker = DefaultEvalsMarker {
        installed: installed.to_vec(),
        version: 1,
    };

    if let Ok(raw) = serde_json::to_string_pretty(&marker) {
        if let Err(err) = std::fs::write(path, raw) {
            eprintln!(
                "drip: could not record the default-evals marker at {}: {err}",
                path.display()
            );
        }
    }
}

/// Writes the shipped starter cases into `evals_dir` and returns the names it
/// wrote. A case whose directory already exists is left alone (a same-named
/// operator case wins, forever), and a case the marker records is never
/// rewritten: deleting a starter case is permanent, exactly like deleting a
/// default skill or plan.
pub fn ensure_default_evals(home_root: &str, evals_dir: &Path) -> Vec<String> {
    let marker_path = default_evals_marker_path(home_root);
    let mut marker = read_default_evals_marker(&marker_path);
    let before: Vec<String> = marker.installed.clone();
    let mut written: Vec<String> = Vec::new();

    for (name, case, scenario) in builtin_eval_entries() {
        let dir = evals_dir.join(name);
        let recorded = marker.installed.iter().any(|installed| installed == name);

        if dir.exists() {
            if !recorded {
                marker.installed.push(name.to_string());
            }
            continue;
        }

        if recorded {
            continue;
        }

        if std::fs::create_dir_all(&dir).is_err() {
            continue;
        }

        if std::fs::write(dir.join(EVAL_CASE_FILE_NAME), case).is_err() {
            continue;
        }

        let _ = std::fs::write(dir.join(EVAL_SCENARIO_FILE_NAME), scenario);
        marker.installed.push(name.to_string());
        written.push(name.to_string());
    }

    if marker.installed != before {
        write_default_evals_marker(&marker_path, &marker.installed);
    }

    written
}

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

/// What a case exercises: a bare prompt, a goal plus one task, or a planning
/// loop (where the classifier's candidates are plans, not skills).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EvalKind {
    #[default]
    Prompt,
    Task,
    Plan,
}

impl EvalKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            EvalKind::Prompt => "prompt",
            EvalKind::Task => "task",
            EvalKind::Plan => "plan",
        }
    }

    /// The role a loop of this kind runs under when the scenario does not name
    /// one: a plan case is a planning loop, everything else is author work.
    pub fn default_role(&self) -> &'static str {
        match self {
            EvalKind::Plan => "planner",
            _ => "author",
        }
    }
}

/// Where a case came from when it was lifted out of a real run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvalSource {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default, rename = "loop")]
    pub loop_number: Option<u32>,
    #[serde(default)]
    pub at: Option<String>,
}

/// `case.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvalCase {
    #[serde(default)]
    pub version: u32,
    /// Defaults to the directory name.
    #[serde(default)]
    pub name: Option<String>,
    /// Defaults to the scenario's goal when absent.
    #[serde(default)]
    pub description: Option<String>,
    pub kind: EvalKind,
    /// The candidates the operator expects to be applicable — the blind target
    /// a tuning session scores the classifier against.
    #[serde(default)]
    pub expected: Vec<String>,
    #[serde(default)]
    pub source: Option<EvalSource>,
}

/// `scenario.json` — exactly the state a loop hands the classifier, plus the
/// candidate pool the case wants it offered.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvalScenario {
    #[serde(default)]
    pub version: u32,
    /// The goal text in force for the loop.
    pub goal: String,
    /// The task title, for a `task` case.
    #[serde(default)]
    pub task: Option<String>,
    #[serde(default)]
    pub notes: Vec<String>,
    /// The role the loop runs under; `None` means the kind's default role.
    #[serde(default)]
    pub role: Option<String>,
    /// The tool surface the classifier sees. Empty means the runner uses the
    /// harness's own default surface.
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub mcp_servers: Vec<String>,
    /// An explicit candidate pool (skill names, or plan names for a plan case).
    /// Empty means "classify over everything installed at this scope".
    #[serde(default)]
    pub candidates: Vec<String>,
}

/// `verdict.json` — what the operator judged after a run.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvalVerdict {
    /// Candidates the operator judged applicable for this scenario.
    #[serde(default)]
    pub applicable: Vec<String>,
    #[serde(default)]
    pub not_applicable: Vec<String>,
    /// What the classifier actually matched when the verdict was recorded.
    #[serde(default)]
    pub matched: Vec<String>,
    #[serde(default)]
    pub judged_at: Option<String>,
}

/// Agreement between a run's matches and the operator's judgment.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvalAgreement {
    pub agreed: Vec<String>,
    /// Applicable but not matched: the classifier's misses.
    pub missed: Vec<String>,
    /// Matched but not applicable: the classifier's spurious picks.
    pub spurious: Vec<String>,
}

/// Splits two candidate lists into agreement, misses and spurious picks. Both
/// sides are compared as a set: order is never meaningful, and duplicates are
/// collapsed.
pub fn eval_agreement(matched: &[String], applicable: &[String]) -> EvalAgreement {
    let matched: HashSet<&str> = matched.iter().map(String::as_str).collect();
    let applicable: HashSet<&str> = applicable.iter().map(String::as_str).collect();

    let mut agreed: Vec<String> = matched
        .intersection(&applicable)
        .map(|name| name.to_string())
        .collect();
    let mut missed: Vec<String> = applicable
        .difference(&matched)
        .map(|name| name.to_string())
        .collect();
    let mut spurious: Vec<String> = matched
        .difference(&applicable)
        .map(|name| name.to_string())
        .collect();

    agreed.sort();
    missed.sort();
    spurious.sort();

    EvalAgreement {
        agreed,
        missed,
        spurious,
    }
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// The `<cwd>/.drip/evals` directory of a project.
pub fn project_evals_dir(cwd: &Path) -> PathBuf {
    cwd.join(".drip").join("evals")
}

/// Where a case was found: project cases shadow user cases of the same name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EvalScope {
    Project,
    User,
}

/// One discovered case, as a listing sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CliEval {
    pub description: String,
    pub kind: EvalKind,
    pub name: String,
    /// Absolute path of the case's `case.json`.
    pub path: String,
    pub scope: EvalScope,
}

/// A case with both files read.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedEval {
    pub case: EvalCase,
    pub scenario: EvalScenario,
    /// Absolute path of the case directory.
    pub dir: String,
    pub name: String,
    pub scope: EvalScope,
}

/// Project cases (`<cwd>/.drip/evals`) shadow user cases (`<home>/evals`) by
/// name, exactly like skills and plans.
pub fn discover_evals(cwd: &Path, home_evals_dir: &Path) -> Vec<CliEval> {
    let project = collect_evals_from_dir(&project_evals_dir(cwd), EvalScope::Project);
    let names: HashSet<String> = project.iter().map(|item| item.name.clone()).collect();

    let mut result = project;
    result.extend(
        collect_evals_from_dir(home_evals_dir, EvalScope::User)
            .into_iter()
            .filter(|item| !names.contains(&item.name)),
    );
    result
}

fn collect_evals_from_dir(evals_dir: &Path, scope: EvalScope) -> Vec<CliEval> {
    let Ok(entries) = std::fs::read_dir(evals_dir) else {
        return Vec::new();
    };

    let mut evals: Vec<CliEval> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .filter_map(|dir| read_eval_entry(&dir, scope.clone()))
        .collect();
    evals.sort_by(|left, right| left.name.cmp(&right.name));
    evals
}

/// A directory is a case when its `case.json` reads and parses: an
/// unreadable or malformed `case.json` is not a case at all, exactly like a
/// directory without a `PLAN.md`. A missing or malformed `scenario.json` still
/// lists — the case exists and the runner reports the broken scenario — so a
/// typo in one file never hides the case from a listing.
fn read_eval_entry(dir: &Path, scope: EvalScope) -> Option<CliEval> {
    let path = dir.join(EVAL_CASE_FILE_NAME);
    let raw = std::fs::read_to_string(&path).ok()?;
    let case: EvalCase = serde_json::from_str(&raw).ok()?;
    let dir_name = dir.file_name()?.to_string_lossy().into_owned();

    let description = case.description.clone().unwrap_or_else(|| {
        read_eval_scenario(dir)
            .map(|scenario| scenario.goal)
            .unwrap_or_default()
    });

    Some(CliEval {
        description,
        kind: case.kind,
        name: case.name.unwrap_or(dir_name),
        path: path.to_string_lossy().into_owned(),
        scope,
    })
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// Reads a case's `case.json`.
pub fn read_eval_case(dir: &Path) -> Result<EvalCase, String> {
    let path = dir.join(EVAL_CASE_FILE_NAME);
    let raw = std::fs::read_to_string(&path)
        .map_err(|error| format!("could not read {}: {error}", path.to_string_lossy()))?;

    serde_json::from_str(&raw)
        .map_err(|error| format!("malformed {}: {error}", path.to_string_lossy()))
}

/// Reads a case's `scenario.json`.
pub fn read_eval_scenario(dir: &Path) -> Result<EvalScenario, String> {
    let path = dir.join(EVAL_SCENARIO_FILE_NAME);
    let raw = std::fs::read_to_string(&path)
        .map_err(|error| format!("could not read {}: {error}", path.to_string_lossy()))?;

    serde_json::from_str(&raw)
        .map_err(|error| format!("malformed {}: {error}", path.to_string_lossy()))
}

/// Reads both files of a discovered case. A discovery entry is the input, so
/// the error names the file that failed, never a bare "not found".
pub fn load_eval(eval: &CliEval) -> Result<LoadedEval, String> {
    let dir = Path::new(&eval.path)
        .parent()
        .ok_or_else(|| format!("{} has no directory", eval.path))?;

    Ok(LoadedEval {
        case: read_eval_case(dir)?,
        dir: dir.to_string_lossy().into_owned(),
        name: eval.name.clone(),
        scenario: read_eval_scenario(dir)?,
        scope: eval.scope.clone(),
    })
}

/// `<case dir>/verdict.json`.
pub fn eval_verdict_path(dir: &Path) -> PathBuf {
    dir.join(EVAL_VERDICT_FILE_NAME)
}

/// The operator's recorded judgment for a case, when one exists. A malformed
/// verdict is no verdict: it is written by the tool, so a hand-broken file is
/// simply judged again.
pub fn load_eval_verdict(dir: &Path) -> Option<EvalVerdict> {
    let raw = std::fs::read_to_string(eval_verdict_path(dir)).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Writes the operator's judgment next to the case, at whatever scope the case
/// lives in — the same directory the case was discovered in.
pub fn save_eval_verdict(dir: &Path, verdict: &EvalVerdict) -> std::io::Result<()> {
    let path = eval_verdict_path(dir);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let raw = serde_json::to_string_pretty(verdict)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    std::fs::write(path, format!("{raw}\n"))
}

// ---------------------------------------------------------------------------
// Listing
// ---------------------------------------------------------------------------

/// The `--plans`-style human listing.
pub fn format_evals_human(evals: &[CliEval]) -> String {
    if evals.is_empty() {
        return "No eval cases found.\n".to_string();
    }

    let mut out = String::new();

    for eval in evals {
        let scope = match eval.scope {
            EvalScope::Project => "project",
            EvalScope::User => "user",
        };
        out.push_str(&format!(
            "{:<24} {:<7} {:<8} {}\n",
            eval.name,
            eval.kind.as_str(),
            scope,
            eval.description
        ));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROMPT_CASE: &str = r#"{
        "version": 1,
        "name": "prompt-case",
        "description": "a bare prompt",
        "kind": "prompt",
        "expected": ["tdd"]
    }"#;
    const PROMPT_SCENARIO: &str =
        r#"{"version":1,"goal":"fix the flaky test","candidates":["tdd"]}"#;

    fn write_case(
        evals_dir: &Path,
        name: &str,
        case_json: &str,
        scenario: Option<&str>,
    ) -> PathBuf {
        let dir = evals_dir.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(EVAL_CASE_FILE_NAME), case_json).unwrap();

        if let Some(scenario) = scenario {
            std::fs::write(dir.join(EVAL_SCENARIO_FILE_NAME), scenario).unwrap();
        }

        dir
    }

    #[test]
    fn project_case_shadows_the_user_case_of_the_same_name() {
        let user = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write_case(
            user.path(),
            "prompt-case",
            r#"{"kind":"prompt","description":"user version"}"#,
            Some(PROMPT_SCENARIO),
        );
        write_case(
            &project_evals_dir(project.path()),
            "prompt-case",
            r#"{"kind":"prompt","description":"project version"}"#,
            Some(PROMPT_SCENARIO),
        );
        write_case(
            user.path(),
            "only-user",
            r#"{"kind":"task","description":"user only"}"#,
            Some(PROMPT_SCENARIO),
        );

        let evals = discover_evals(project.path(), user.path());
        assert_eq!(evals.len(), 2);

        let shadowed = evals
            .iter()
            .find(|eval| eval.name == "prompt-case")
            .unwrap();
        assert_eq!(shadowed.description, "project version");
        assert_eq!(shadowed.scope, EvalScope::Project);

        let only_user = evals.iter().find(|eval| eval.name == "only-user").unwrap();
        assert_eq!(only_user.description, "user only");
        assert_eq!(only_user.kind, EvalKind::Task);
        assert_eq!(only_user.scope, EvalScope::User);
    }

    #[test]
    fn a_directory_without_a_case_json_is_not_a_case() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("empty")).unwrap();
        std::fs::write(dir.path().join("loose.json"), "not a case\n").unwrap();

        assert!(discover_evals(dir.path(), dir.path()).is_empty());
    }

    #[test]
    fn a_case_without_a_scenario_lists_with_its_goal_and_does_not_load() {
        let dir = tempfile::tempdir().unwrap();
        write_case(dir.path(), "no-scenario", PROMPT_CASE, None);

        // The listing falls back to the scenario goal for a description; with
        // no scenario there is nothing to fall back to.
        let evals = discover_evals(dir.path(), dir.path());
        assert_eq!(evals.len(), 1);
        assert_eq!(evals[0].description, "a bare prompt");

        let error = load_eval(&evals[0]).expect_err("a case without a scenario cannot load");
        assert!(error.contains(EVAL_SCENARIO_FILE_NAME), "{error}");
    }

    #[test]
    fn a_case_description_falls_back_to_the_scenario_goal() {
        let dir = tempfile::tempdir().unwrap();
        write_case(
            dir.path(),
            "no-description",
            r#"{"kind":"task"}"#,
            Some(PROMPT_SCENARIO),
        );

        let evals = discover_evals(dir.path(), dir.path());
        assert_eq!(evals[0].description, "fix the flaky test");
        // No `name:` in the case: the directory name is the case's name.
        assert_eq!(evals[0].name, "no-description");
    }

    #[test]
    fn loading_reads_the_case_and_its_scenario() {
        let dir = tempfile::tempdir().unwrap();
        write_case(
            dir.path(),
            "flaky",
            r#"{"version":1,"kind":"task","expected":["tdd","verify-before-done"],
               "source":{"sessionId":"abc","loop":4,"at":"2026-09-23T02:00:00Z"}}"#,
            Some(
                r#"{"version":1,"goal":"fix the flake","task":"task-1: reproduce it",
                    "notes":["it is a SIGTERM"],"role":"author","tools":["BASH"],
                    "candidates":["tdd"]}"#,
            ),
        );

        let evals = discover_evals(dir.path(), dir.path());
        let loaded = load_eval(&evals[0]).unwrap();

        assert_eq!(loaded.name, "flaky");
        assert_eq!(loaded.case.kind, EvalKind::Task);
        assert_eq!(loaded.case.expected, vec!["tdd", "verify-before-done"]);
        assert_eq!(
            loaded.case.source.unwrap().loop_number,
            Some(4),
            "a pinned case keeps the loop it came from"
        );
        assert_eq!(loaded.scenario.goal, "fix the flake");
        assert_eq!(
            loaded.scenario.task.as_deref(),
            Some("task-1: reproduce it")
        );
        assert_eq!(loaded.scenario.notes, vec!["it is a SIGTERM"]);
        assert_eq!(loaded.scenario.role.as_deref(), Some("author"));
        assert_eq!(loaded.scenario.tools, vec!["BASH"]);
        assert_eq!(loaded.scenario.candidates, vec!["tdd"]);
        assert_eq!(loaded.dir, dir.path().join("flaky").to_string_lossy());
    }

    #[test]
    fn an_unknown_kind_is_rejected_and_names_the_file() {
        let dir = tempfile::tempdir().unwrap();
        write_case(
            dir.path(),
            "weird",
            r#"{"kind":"conversation","description":"nope"}"#,
            Some(PROMPT_SCENARIO),
        );
        assert!(discover_evals(dir.path(), dir.path()).is_empty());

        // Loading the directory directly reports the same rejection, naming
        // the file so a hand-authored case is fixable.
        let error = read_eval_case(&dir.path().join("weird")).expect_err("unknown kind");
        assert!(error.contains(EVAL_CASE_FILE_NAME), "{error}");
        assert!(error.contains("conversation"), "{error}");
    }

    #[test]
    fn a_malformed_scenario_loads_as_an_error_naming_the_file() {
        let dir = tempfile::tempdir().unwrap();
        write_case(dir.path(), "broken", PROMPT_CASE, Some("{ not json"));

        // The case still lists — a broken scenario is reported by the runner,
        // not hidden from the operator.
        let evals = discover_evals(dir.path(), dir.path());
        assert_eq!(evals.len(), 1);

        let error = load_eval(&evals[0]).expect_err("a malformed scenario cannot load");
        assert!(error.contains(EVAL_SCENARIO_FILE_NAME), "{error}");
    }

    #[test]
    fn a_verdict_round_trips_and_a_missing_one_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let case_dir = write_case(dir.path(), "flaky", PROMPT_CASE, Some(PROMPT_SCENARIO));

        assert!(load_eval_verdict(&case_dir).is_none());

        let verdict = EvalVerdict {
            applicable: vec!["tdd".to_string()],
            not_applicable: vec!["hooks-setup".to_string()],
            matched: vec!["hooks-setup".to_string()],
            judged_at: Some("2026-09-25T12:00:00Z".to_string()),
        };
        save_eval_verdict(&case_dir, &verdict).unwrap();
        assert_eq!(load_eval_verdict(&case_dir), Some(verdict));
        assert!(eval_verdict_path(&case_dir).exists());

        // The verdict lives INSIDE the case directory, so it follows the case's
        // scope: a project case is judged in the repo, a user case in the home.
        assert_eq!(
            eval_verdict_path(&case_dir),
            case_dir.join(EVAL_VERDICT_FILE_NAME)
        );
    }

    #[test]
    fn agreement_splits_matches_and_judgments_into_misses_and_spurious_picks() {
        let matched = vec![
            "verify-before-done".to_string(),
            "hooks-setup".to_string(),
            "tdd".to_string(),
        ];
        let applicable = vec![
            "tdd".to_string(),
            "verify-before-done".to_string(),
            "verify-before-done".to_string(),
        ];

        let agreement = eval_agreement(&matched, &applicable);
        assert_eq!(
            agreement.agreed,
            vec!["tdd", "verify-before-done"],
            "order and duplicates never change agreement"
        );
        assert!(agreement.missed.is_empty());
        assert_eq!(agreement.spurious, vec!["hooks-setup"]);

        // The other direction: a candidate the operator wanted and the
        // classifier never matched is a miss.
        let agreement = eval_agreement(
            &["tdd".to_string()],
            &["tdd".to_string(), "probatio".to_string()],
        );
        assert_eq!(agreement.missed, vec!["probatio"]);
        assert!(agreement.spurious.is_empty());
    }

    #[test]
    fn the_shipped_starter_cases_parse_with_the_case_schema() {
        for (name, case_json, scenario_json) in builtin_eval_entries() {
            let case: EvalCase = serde_json::from_str(case_json)
                .unwrap_or_else(|error| panic!("starter case {name} does not parse: {error}"));
            let scenario: EvalScenario = serde_json::from_str(scenario_json)
                .unwrap_or_else(|error| panic!("starter scenario {name} does not parse: {error}"));

            assert_eq!(
                case.name.as_deref(),
                Some(name),
                "starter {name} names itself"
            );
            assert!(
                case.description.is_some(),
                "starter {name} describes itself"
            );
            assert!(
                !scenario.goal.trim().is_empty(),
                "starter {name} carries the goal its classifier run needs"
            );
        }

        let names = default_eval_names();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "the pack stays in sorted order");
        assert!(names.iter().any(|name| name == "flaky-test-fixup"));
        assert!(names.iter().any(|name| name == "ship-this-branch"));
        assert!(default_eval_template("nope").is_none());

        // Every evals/cases/<name> directory is registered: a case added
        // without a registry line would never reach a home.
        let mut shipped: Vec<String> =
            std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/evals/cases"))
                .expect("evals/cases exists")
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.path().is_dir())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect();
        shipped.sort();
        assert_eq!(names, shipped);
    }

    #[test]
    fn the_shipped_plan_case_is_a_planning_loop() {
        let (case_json, scenario_json) = default_eval_template("ship-this-branch").unwrap();
        let case: EvalCase = serde_json::from_str(case_json).unwrap();
        let scenario: EvalScenario = serde_json::from_str(scenario_json).unwrap();

        assert_eq!(case.kind, EvalKind::Plan);
        assert_eq!(case.kind.default_role(), "planner");
        // No role in the scenario: the runner uses the kind's default.
        assert!(scenario.role.is_none());
        assert_eq!(case.expected, vec!["ship-pr"]);
        assert_eq!(EvalKind::Prompt.default_role(), "author");
    }

    #[test]
    fn ensure_default_evals_seeds_once_and_a_deleted_case_stays_deleted() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().to_str().unwrap();
        let evals_dir = home.path().join("evals");

        let written = ensure_default_evals(root, &evals_dir);
        assert_eq!(written, default_eval_names());
        assert!(evals_dir
            .join("flaky-test-fixup")
            .join(EVAL_CASE_FILE_NAME)
            .exists());
        assert!(evals_dir
            .join("flaky-test-fixup")
            .join(EVAL_SCENARIO_FILE_NAME)
            .exists());

        // A second call is a no-op...
        assert!(ensure_default_evals(root, &evals_dir).is_empty());

        // ...and deleting a seeded case never resurrects it.
        std::fs::remove_dir_all(evals_dir.join("ship-this-branch")).unwrap();
        assert!(ensure_default_evals(root, &evals_dir).is_empty());
        assert!(!evals_dir.join("ship-this-branch").exists());

        // The marker sits beside evals/, so it is never discovered as a case.
        assert!(default_evals_marker_path(root).exists());
        assert!(discover_evals(home.path(), &evals_dir)
            .iter()
            .all(|eval| eval.name != DEFAULT_EVALS_MARKER_FILE));
    }

    #[test]
    fn seeding_never_overwrites_an_operator_case_of_the_same_name() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().to_str().unwrap();
        let evals_dir = home.path().join("evals");
        let mine = write_case(
            &evals_dir,
            "flaky-test-fixup",
            r#"{"kind":"prompt","description":"mine"}"#,
            Some(PROMPT_SCENARIO),
        );

        let written = ensure_default_evals(root, &evals_dir);
        assert!(!written.iter().any(|name| name == "flaky-test-fixup"));
        assert_eq!(written.len(), default_eval_names().len() - 1);
        assert_eq!(
            std::fs::read_to_string(mine.join(EVAL_CASE_FILE_NAME)).unwrap(),
            r#"{"kind":"prompt","description":"mine"}"#
        );
    }

    #[test]
    fn format_evals_human_lists_name_kind_scope_and_description() {
        assert_eq!(format_evals_human(&[]), "No eval cases found.\n");

        let dir = tempfile::tempdir().unwrap();
        // A case under the project's own `.drip/evals` lists at project scope;
        // the same directory passed as both roots would be the user scope.
        write_case(
            &project_evals_dir(dir.path()),
            "flaky",
            PROMPT_CASE,
            Some(PROMPT_SCENARIO),
        );

        let evals = discover_evals(dir.path(), dir.path());
        let listing = format_evals_human(&evals);
        assert!(listing.starts_with("prompt-case"), "{listing}");
        assert!(listing.contains("prompt"), "{listing}");
        assert!(listing.contains("project"), "{listing}");
        assert!(listing.contains("a bare prompt"), "{listing}");
    }
}
