//! Plans: reusable, prebuilt starting points for the planning loop.
//!
//! A plan is a skill-shaped template that only ever reaches the
//! planning/replanning loop: it shapes the task list the planner builds, never
//! the work itself. Storage mirrors skills —
//!
//! - `<home>/plans/<name>/` — user scope, shared by every project.
//! - `<cwd>/.drip/plans/<name>/` — project scope, shadowing the user's by name.
//!
//! Each plan directory holds two files:
//!
//! - `PLAN.md` — the template the planner receives as a starting point, with an
//!   optional `---`-fenced `name:` / `description:` header.
//! - `classification.json` — the same jev relevance schema a skill carries in
//!   its `classifiers.json` sidecar, answering "is this plan relevant to this
//!   goal?". Optional: a plan without it is offered like an unauthored skill.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::harness::classifier::{SkillCandidate, SkillClassifiers};

/// The template file every plan directory holds.
pub const PLAN_FILE_NAME: &str = "PLAN.md";
/// The optional jev relevance sidecar (the skill `classifiers.json` schema).
pub const PLAN_CLASSIFICATION_FILE_NAME: &str = "classification.json";
/// Marker recording which shipped starter plans were already seeded into a
/// home. It sits beside `plans/`, never inside it, so deleting a plan does not
/// resurrect the template on the next start.
pub const DEFAULT_PLANS_MARKER_FILE: &str = "default-plans.json";

/// One shipped plan: its files are embedded at compile time from
/// `plans/<name>/`, so adding a plan is adding a directory there and one line
/// to `builtin_plan_entries`.
macro_rules! builtin_plan {
    ($name:literal) => {
        (
            $name,
            include_str!(concat!("../../plans/", $name, "/PLAN.md")),
            include_str!(concat!("../../plans/", $name, "/classification.json")),
        )
    };
}

// ---------------------------------------------------------------------------
// Starter plan pack — templates embedded at compile time and copied into
// <home>/plans on the first start. Nothing here is discovered automatically:
// a deleted starter plan stays deleted.
// ---------------------------------------------------------------------------

/// The shipped starter plans as (name, PLAN.md, classification.json) triples,
/// in sorted order.
fn builtin_plan_entries() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        builtin_plan!("build-feature"),
        builtin_plan!("exact-commands"),
        builtin_plan!("explain-repo"),
        builtin_plan!("finish-in-progress"),
        builtin_plan!("fix-bug"),
        builtin_plan!("fix-review-findings"),
        builtin_plan!("investigate-report"),
        builtin_plan!("port-module"),
        builtin_plan!("remove-feature"),
        builtin_plan!("scaffold-project"),
        builtin_plan!("ship-pr"),
        builtin_plan!("sweep-rename"),
        builtin_plan!("verify-only"),
    ]
}

/// The names of every starter plan this binary ships, in sorted order.
pub fn default_plan_names() -> Vec<String> {
    builtin_plan_entries()
        .into_iter()
        .map(|(name, _, _)| name.to_string())
        .collect()
}

/// The shipped template for `name` as (PLAN.md, classification.json).
pub fn default_plan_template(name: &str) -> Option<(&'static str, &'static str)> {
    builtin_plan_entries()
        .into_iter()
        .find(|(entry_name, _, _)| *entry_name == name)
        .map(|(_, plan, classification)| (plan, classification))
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct DefaultPlansMarker {
    #[serde(default)]
    installed: Vec<String>,
    #[serde(default)]
    version: u32,
}

/// `<home_root>/default-plans.json` — the seeding marker's absolute path.
pub fn default_plans_marker_path(home_root: &str) -> PathBuf {
    Path::new(home_root).join(DEFAULT_PLANS_MARKER_FILE)
}

fn read_default_plans_marker(path: &Path) -> DefaultPlansMarker {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn write_default_plans_marker(path: &Path, installed: &[String]) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let marker = DefaultPlansMarker {
        installed: installed.to_vec(),
        version: 1,
    };

    if let Ok(raw) = serde_json::to_string_pretty(&marker) {
        if let Err(err) = std::fs::write(path, raw) {
            eprintln!(
                "drip: could not record the default-plans marker at {}: {err}",
                path.display()
            );
        }
    }
}

/// Writes the shipped starter plans into `plans_dir` and returns the names it
/// wrote. A plan whose directory already exists is left alone (a same-named
/// operator plan wins, forever), and a plan the marker records is never
/// rewritten: deleting a starter plan is permanent, exactly like deleting a
/// default skill.
pub fn ensure_default_plans(home_root: &str, plans_dir: &Path) -> Vec<String> {
    let marker_path = default_plans_marker_path(home_root);
    let mut marker = read_default_plans_marker(&marker_path);
    let before: Vec<String> = marker.installed.clone();
    let mut written: Vec<String> = Vec::new();

    for (name, plan, classification) in builtin_plan_entries() {
        let dir = plans_dir.join(name);
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

        if std::fs::write(dir.join(PLAN_FILE_NAME), plan).is_err() {
            continue;
        }

        let _ = std::fs::write(dir.join(PLAN_CLASSIFICATION_FILE_NAME), classification);
        marker.installed.push(name.to_string());
        written.push(name.to_string());
    }

    if marker.installed != before {
        write_default_plans_marker(&marker_path, &marker.installed);
    }

    written
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// The `<cwd>/.drip/plans` directory of a project.
pub fn project_plans_dir(cwd: &Path) -> PathBuf {
    cwd.join(".drip").join("plans")
}

/// Where a plan was found: project plans shadow user plans of the same name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PlanScope {
    Project,
    User,
}

/// One discovered plan, as the planner's candidate list sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CliPlan {
    pub description: String,
    pub name: String,
    /// Absolute path of the plan's `PLAN.md`.
    pub path: String,
    pub scope: PlanScope,
}

/// A plan's template text, ready to compose into a loop's system prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedCliPlan {
    pub content: String,
    pub name: String,
}

/// Project plans (`<cwd>/.drip/plans`) shadow user plans (`<home>/plans`) by
/// name, exactly like skills.
pub fn discover_plans(cwd: &Path, home_plans_dir: &Path) -> Vec<CliPlan> {
    let project = collect_plans_from_dir(&project_plans_dir(cwd), PlanScope::Project);
    let names: HashSet<String> = project.iter().map(|plan| plan.name.clone()).collect();

    let mut result = project;
    result.extend(
        collect_plans_from_dir(home_plans_dir, PlanScope::User)
            .into_iter()
            .filter(|plan| !names.contains(&plan.name)),
    );
    result
}

fn collect_plans_from_dir(plans_dir: &Path, scope: PlanScope) -> Vec<CliPlan> {
    let Ok(entries) = std::fs::read_dir(plans_dir) else {
        return Vec::new();
    };

    let mut plans: Vec<CliPlan> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .filter_map(|dir| read_plan_entry(&dir, scope.clone()))
        .collect();
    plans.sort_by(|left, right| left.name.cmp(&right.name));
    plans
}

fn read_plan_entry(dir: &Path, scope: PlanScope) -> Option<CliPlan> {
    let path = dir.join(PLAN_FILE_NAME);
    let raw = std::fs::read_to_string(&path).ok()?;
    let frontmatter = parse_plan_frontmatter(&raw);
    let dir_name = dir.file_name()?.to_string_lossy().into_owned();

    Some(CliPlan {
        description: frontmatter
            .description
            .unwrap_or_else(|| first_non_empty_line(plan_body(&raw))),
        name: frontmatter.name.unwrap_or(dir_name),
        path: path.to_string_lossy().into_owned(),
        scope,
    })
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// Reads a plan's template text.
pub fn load_plan_content(plan: &CliPlan) -> Result<LoadedCliPlan, String> {
    let raw = std::fs::read_to_string(&plan.path)
        .map_err(|error| format!("could not read {}: {error}", plan.path))?;

    Ok(LoadedCliPlan {
        content: plan_body(&raw).to_string(),
        name: plan.name.clone(),
    })
}

/// Reads `<dir of PLAN.md>/classification.json` with the skill classifier
/// schema. `None` when the file is absent (an unauthored plan); `Some(Err)`
/// when it is malformed, which the caller warns about once.
pub fn load_plan_classification(plan: &CliPlan) -> Option<Result<SkillClassifiers, String>> {
    let path = Path::new(&plan.path)
        .parent()?
        .join(PLAN_CLASSIFICATION_FILE_NAME);

    if !path.exists() {
        return None;
    }

    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) => {
            return Some(Err(format!(
                "could not read {}: {error}",
                path.to_string_lossy()
            )))
        }
    };

    Some(
        serde_json::from_str::<SkillClassifiers>(&raw)
            .map_err(|error| format!("malformed {}: {error}", path.to_string_lossy())),
    )
}

// ---------------------------------------------------------------------------
// Composition
// ---------------------------------------------------------------------------

/// Appends the selected plans to a planning loop's system prompt. The plans go
/// last: they are the starting point the planner adapts, not a second persona.
pub fn compose_plan_system_prompt(base_prompt: &str, plans: &[LoadedCliPlan]) -> String {
    if plans.is_empty() {
        return base_prompt.to_string();
    }

    let mut out = String::from(base_prompt);
    out.push_str("\n\n# Plan templates\n\n");
    out.push_str(
        "The operator keeps prebuilt plans for the work they repeat. The plan(s) below were classified as relevant to this goal: use them as the starting shape of the task list — keep their steps and their order, drop or reorder what does not fit, and add the steps this goal needs. A plan is a template, never a substitute for reading the goal.\n",
    );

    for plan in plans {
        out.push_str(&format!("\n## Plan: {}\n\n{}\n", plan.name, plan.content));
    }

    out
}

/// The `--plans`-style human listing.
pub fn format_plans_human(plans: &[CliPlan]) -> String {
    if plans.is_empty() {
        return "No plans found.\n".to_string();
    }

    let mut out = String::new();

    for plan in plans {
        let scope = match plan.scope {
            PlanScope::Project => "project",
            PlanScope::User => "user",
        };
        out.push_str(&format!(
            "{:<20} {:<8} {}\n",
            plan.name, scope, plan.description
        ));
    }

    out
}

// ---------------------------------------------------------------------------
// Frontmatter
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct PlanFrontmatter {
    description: Option<String>,
    name: Option<String>,
}

/// Reads the optional `---`-fenced `name:` / `description:` header. Unknown
/// keys are ignored: the header is metadata, the body is the template.
fn parse_plan_frontmatter(markdown: &str) -> PlanFrontmatter {
    let mut frontmatter = PlanFrontmatter::default();

    let Some(header) = frontmatter_header(markdown) else {
        return frontmatter;
    };

    for line in header.lines() {
        let line = line.trim();

        if let Some(value) = line.strip_prefix("name:") {
            frontmatter.name = non_empty(value);
        } else if let Some(value) = line.strip_prefix("description:") {
            frontmatter.description = non_empty(value);
        }
    }

    frontmatter
}

/// The template body: the markdown with its frontmatter header removed.
pub fn plan_body(markdown: &str) -> &str {
    match frontmatter_end(markdown) {
        Some(end) => markdown[end..].trim(),
        None => markdown.trim(),
    }
}

/// Byte offset just past a closing `---` fence, when the markdown opens with an
/// `---` fence that a later `\n---` closes.
fn frontmatter_end(markdown: &str) -> Option<usize> {
    let rest = markdown.strip_prefix("---")?;
    // 3 bytes for the opening fence, plus the header, plus "\n---".
    let end = rest.find("\n---")?;
    Some(3 + end + 4)
}

/// The frontmatter header text (without its fences).
fn frontmatter_header(markdown: &str) -> Option<&str> {
    let rest = markdown.strip_prefix("---")?;
    let end = rest.find("\n---")?;
    Some(&rest[..end])
}

fn non_empty(value: &str) -> Option<String> {
    let value = value.trim().trim_matches('"').trim();

    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn first_non_empty_line(markdown: &str) -> String {
    markdown
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_plan(
        plans_dir: &Path,
        name: &str,
        plan_md: &str,
        classification: Option<&str>,
    ) -> PathBuf {
        let dir = plans_dir.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(PLAN_FILE_NAME), plan_md).unwrap();

        if let Some(classification) = classification {
            std::fs::write(dir.join(PLAN_CLASSIFICATION_FILE_NAME), classification).unwrap();
        }

        dir
    }

    #[test]
    fn project_plan_shadows_the_user_plan_of_the_same_name() {
        let user = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write_plan(
            user.path(),
            "ship-pr",
            "---\nname: ship-pr\ndescription: user version\n---\n\nuser body\n",
            None,
        );
        write_plan(
            &project_plans_dir(project.path()),
            "ship-pr",
            "---\nname: ship-pr\ndescription: project version\n---\n\nproject body\n",
            None,
        );
        write_plan(
            user.path(),
            "only-user",
            "---\ndescription: user only\n---\n\nbody\n",
            None,
        );

        let plans = discover_plans(project.path(), user.path());
        assert_eq!(plans.len(), 2);

        let ship = plans.iter().find(|plan| plan.name == "ship-pr").unwrap();
        assert_eq!(ship.description, "project version");
        assert_eq!(ship.scope, PlanScope::Project);

        // No `name:` in the header: the directory name is the plan's name.
        let only_user = plans.iter().find(|plan| plan.name == "only-user").unwrap();
        assert_eq!(only_user.description, "user only");
        assert_eq!(only_user.scope, PlanScope::User);
    }

    #[test]
    fn a_directory_without_plan_md_is_not_a_plan() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("empty")).unwrap();
        std::fs::write(dir.path().join("loose.md"), "not a plan\n").unwrap();

        assert!(discover_plans(dir.path(), dir.path()).is_empty());
    }

    #[test]
    fn loading_a_plan_strips_frontmatter_and_keeps_the_template_body() {
        let dir = tempfile::tempdir().unwrap();
        write_plan(
            dir.path(),
            "ship-pr",
            "---\nname: ship-pr\ndescription: ship it\n---\n\n[1] verify\n[2] draft pr\n",
            None,
        );

        let plans = discover_plans(dir.path(), dir.path());
        let loaded = load_plan_content(&plans[0]).unwrap();

        assert_eq!(loaded.name, "ship-pr");
        assert_eq!(loaded.content, "[1] verify\n[2] draft pr");
    }

    #[test]
    fn plan_classification_sidecar_is_read_and_a_malformed_one_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        write_plan(
            dir.path(),
            "authored",
            "---\nname: authored\n---\n\nbody\n",
            Some("{\"relevance\":{\"threshold\":0.7,\"questions\":{},\"formula\":\"1.0\"}}"),
        );
        write_plan(
            dir.path(),
            "broken",
            "---\nname: broken\n---\n\nbody\n",
            Some("{ not json"),
        );
        write_plan(
            dir.path(),
            "unauthored",
            "---\nname: unauthored\n---\n\nbody\n",
            None,
        );

        let plans = discover_plans(dir.path(), dir.path());
        let find = |name: &str| plans.iter().find(|plan| plan.name == name).unwrap().clone();

        let authored = load_plan_classification(&find("authored"))
            .expect("sidecar present")
            .expect("sidecar parses");
        assert_eq!(authored.relevance.unwrap().threshold, Some(0.7));

        assert!(load_plan_classification(&find("broken"))
            .expect("sidecar present")
            .is_err());
        assert!(load_plan_classification(&find("unauthored")).is_none());
    }

    #[test]
    fn the_shipped_starter_plan_parses_with_the_skill_classifier_schema() {
        let (_, classification) = default_plan_template("ship-pr").expect("shipped plan");
        let classifiers: SkillClassifiers =
            serde_json::from_str(classification).expect("starter classification");
        let relevance = classifiers.relevance.expect("relevance block");

        assert_eq!(relevance.threshold, Some(0.67));
        assert!(relevance.questions.len() >= 3);
        assert!(relevance.formula.is_some());
    }

    #[test]
    fn every_shipped_plan_parses_and_names_itself() {
        // Every plans/<name> directory is registered, in sorted order: a plan
        // added without a registry line would never reach a home.
        let names = default_plan_names();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "the pack stays in sorted order");
        let mut shipped: Vec<String> =
            std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/plans"))
                .expect("plans exists")
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.path().is_dir())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect();
        shipped.sort();
        assert_eq!(names, shipped);

        for name in &names {
            let (plan, classification) = default_plan_template(name).unwrap();
            let frontmatter = parse_plan_frontmatter(plan);
            assert_eq!(frontmatter.name.as_deref(), Some(name.as_str()), "{name}");
            assert!(frontmatter.description.is_some(), "{name} has a description");
            assert!(plan_body(plan).contains("1. **"), "{name} is a numbered step template");

            let classifiers: SkillClassifiers = serde_json::from_str(classification)
                .unwrap_or_else(|error| panic!("{name}: {error}"));
            let relevance = classifiers.relevance.expect("relevance block");
            assert!(relevance.threshold.is_some(), "{name} owns its threshold");
            assert!(relevance.formula.is_some(), "{name} has a formula");
        }
    }

    #[test]
    fn the_shipped_starter_plan_reads_as_a_step_template() {
        let (plan, _) = default_plan_template("ship-pr").expect("shipped plan");
        let frontmatter = parse_plan_frontmatter(plan);
        assert_eq!(frontmatter.name.as_deref(), Some("ship-pr"));
        assert!(frontmatter.description.is_some());

        let body = plan_body(plan);
        assert!(body.to_lowercase().contains("draft pr"));
        assert!(body.contains("Baseline verification"));
        assert!(!body.contains("name: ship-pr"));
    }

    #[test]
    fn ensure_default_plans_seeds_once_and_a_deleted_plan_stays_deleted() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().to_str().unwrap();
        let plans_dir = home.path().join("plans");

        let written = ensure_default_plans(root, &plans_dir);
        assert_eq!(written, default_plan_names());
        assert!(plans_dir.join("ship-pr").join(PLAN_FILE_NAME).exists());
        assert!(plans_dir
            .join("ship-pr")
            .join(PLAN_CLASSIFICATION_FILE_NAME)
            .exists());

        // A second call is a no-op...
        assert!(ensure_default_plans(root, &plans_dir).is_empty());

        // ...and deleting a seeded plan never resurrects it.
        std::fs::remove_dir_all(plans_dir.join("ship-pr")).unwrap();
        assert!(ensure_default_plans(root, &plans_dir).is_empty());
        assert!(!plans_dir.join("ship-pr").exists());
    }

    #[test]
    fn an_operator_plan_of_a_shipped_name_is_never_overwritten() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().to_str().unwrap();
        let plans_dir = home.path().join("plans");
        let dir = plans_dir.join("ship-pr");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(PLAN_FILE_NAME), "operator plan\n").unwrap();

        // The other starters are seeded; the operator's ship-pr is not touched.
        let written = ensure_default_plans(root, &plans_dir);
        assert!(!written.iter().any(|name| name == "ship-pr"));
        assert_eq!(written.len(), default_plan_names().len() - 1);
        assert_eq!(
            std::fs::read_to_string(dir.join(PLAN_FILE_NAME)).unwrap(),
            "operator plan\n"
        );

        let plans = discover_plans(home.path(), &plans_dir);
        let ship = plans.iter().find(|plan| plan.name == "ship-pr").unwrap();
        assert_eq!(ship.description, "operator plan");
    }

    #[test]
    fn compose_plan_system_prompt_appends_a_plan_section() {
        let base = "base prompt";
        assert_eq!(compose_plan_system_prompt(base, &[]), base);

        let out = compose_plan_system_prompt(
            base,
            &[LoadedCliPlan {
                content: "[1] verify".to_string(),
                name: "ship-pr".to_string(),
            }],
        );

        assert!(out.starts_with(base));
        assert!(out.contains("# Plan templates"));
        assert!(out.contains("## Plan: ship-pr"));
        assert!(out.contains("[1] verify"));
    }
}

/// One plan the planner may be offered: the template body plus the jev
/// questions that decide whether the plan is relevant to the goal.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlanPoolEntry {
    pub name: String,
    pub description: String,
    /// The template body (frontmatter stripped) the planner receives.
    pub content: String,
    /// The plan's `classification.json`; None for an unauthored plan, which the
    /// classifier is asked about on its own.
    pub classifiers: Option<SkillClassifiers>,
}

/// Discovers the plans visible from `cwd` and loads each one's template body
/// into a pool entry. A plan whose `PLAN.md` cannot be read is skipped: the
/// pool is an input to planning, never fatal to the run.
pub fn build_plan_pool(cwd: &Path, home_plans_dir: &Path) -> Vec<PlanPoolEntry> {
    discover_plans(cwd, home_plans_dir)
        .iter()
        .filter_map(|plan| {
            let loaded = load_plan_content(plan).ok()?;
            let (classifiers, warning) = pool_classifiers(plan);

            if let Some(warning) = warning {
                eprintln!("{warning}");
            }

            Some(PlanPoolEntry {
                name: plan.name.clone(),
                description: plan.description.clone(),
                content: loaded.content,
                classifiers,
            })
        })
        .collect()
}

/// A plan's sidecar as the pool stores it, plus the warning to surface when it
/// is malformed. A malformed `classification.json` never drops the plan and is
/// never silently ignored: the plan is pooled unauthored — exactly like a skill
/// with a broken `classifiers.json` — and the operator is told once.
fn pool_classifiers(plan: &CliPlan) -> (Option<SkillClassifiers>, Option<String>) {
    match load_plan_classification(plan) {
        None => (None, None),
        Some(Ok(classifiers)) => (Some(classifiers), None),
        Some(Err(error)) => (
            None,
            Some(format!(
                "plans: \"{}\": {error} — treating it as unauthored",
                plan.name
            )),
        ),
    }
}

/// The pool as classifier candidates. A plan declares no capability
/// requirements: it shapes the task list rather than doing the work, so it is
/// always eligible whatever tools this run carries.
pub fn plan_candidates(pool: &[PlanPoolEntry]) -> Vec<SkillCandidate> {
    pool.iter()
        .map(|entry| SkillCandidate {
            name: entry.name.clone(),
            description: entry.description.clone(),
            classifiers: entry.classifiers.clone(),
        })
        .collect()
}

/// The discovered plans as a per-loop pool of `DynamicSkill`s: exactly what
/// the classifier selects from on a planning loop.
///
/// A plan declares no capability requirements — it shapes the task list rather
/// than doing the work — so every plan is eligible whatever tools the loop
/// carries (`SkillRequirements::known = false` is what
/// `requirements_satisfied` admits on any surface).
pub fn build_plan_pool_skills(
    cwd: &Path,
    home_plans_dir: &Path,
) -> Vec<crate::harness::classifier::DynamicSkill> {
    build_plan_pool(cwd, home_plans_dir)
        .into_iter()
        .map(|entry| crate::harness::classifier::DynamicSkill {
            name: entry.name,
            description: entry.description,
            content: entry.content,
            classifiers: entry.classifiers,
            requirements: crate::core::skill_requirements::SkillRequirements {
                known: false,
                required: std::collections::BTreeSet::new(),
            },
        })
        .collect()
}

/// Selects the plans relevant to ONE loop through the shared classifier
/// machinery: a plan's `classification.json` IS the skill `classifiers.json`
/// schema, so the whole pass (native jev questions, formula, threshold) is
/// `select_skills` unchanged.
///
/// Never fatal: an empty pool is a no-op, and every classifier failure comes
/// back as a warning for the caller to surface.
pub async fn select_plans(
    route: &crate::harness::classifier::ClassifierRoute,
    state: serde_json::Value,
    pool: &[crate::harness::classifier::DynamicSkill],
) -> crate::harness::classifier::SkillSelection {
    if pool.is_empty() {
        return Default::default();
    }

    let candidates = plan_candidates_as_skills(pool);
    let selection = crate::harness::classifier::select_skills(route, state, &candidates).await;

    // Only a plan this pool actually offered is composed: the gate is
    // structural, not a property of what the classifier returned.
    let offered = |name: &String| pool.iter().any(|plan| &plan.name == name);
    let selected = selection
        .selected
        .into_iter()
        .filter(|(name, _score)| offered(name))
        .collect();
    let scored = selection
        .scored
        .into_iter()
        .filter(|(name, _score)| offered(name))
        .collect();

    let mut answers = selection.answers;
    answers.retain(|name, _| offered(name));

    crate::harness::classifier::SkillSelection {
        selected,
        scored,
        answers,
        warnings: selection.warnings,
    }
}

fn plan_candidates_as_skills(
    pool: &[crate::harness::classifier::DynamicSkill],
) -> Vec<SkillCandidate> {
    pool.iter()
        .map(|plan| SkillCandidate {
            name: plan.name.clone(),
            description: plan.description.clone(),
            classifiers: plan.classifiers.clone(),
        })
        .collect()
}

/// The selected plans as the templates `compose_plan_system_prompt` composes.
pub fn selected_plans(
    pool: &[crate::harness::classifier::DynamicSkill],
    selection: &crate::harness::classifier::SkillSelection,
) -> Vec<LoadedCliPlan> {
    selection
        .selected
        .iter()
        .filter_map(|(name, _score)| {
            pool.iter()
                .find(|plan| &plan.name == name)
                .map(|plan| LoadedCliPlan {
                    name: plan.name.clone(),
                    content: plan.content.clone(),
                })
        })
        .collect()
}

#[cfg(test)]
mod plan_pool_tests {
    use super::*;

    #[test]
    fn build_plan_pool_loads_each_plan_body_and_treats_a_malformed_classification_as_unauthored() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let dir = home.path().join("ship-pr");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(PLAN_FILE_NAME),
            "---\nname: ship-pr\ndescription: ship a pr\n---\n\n[1] verify the repo\n",
        )
        .unwrap();
        std::fs::write(dir.join(PLAN_CLASSIFICATION_FILE_NAME), "{ not json").unwrap();

        let pool = build_plan_pool(project.path(), home.path());
        assert_eq!(pool.len(), 1);
        assert_eq!(pool[0].name, "ship-pr");
        assert_eq!(pool[0].description, "ship a pr");
        assert!(pool[0].content.contains("[1] verify the repo"));
        // A malformed sidecar is reported, never fatal: the plan is pooled
        // unauthored, exactly like a skill with a broken classifiers.json.
        assert!(pool[0].classifiers.is_none());

        let candidates = plan_candidates(&pool);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].name, "ship-pr");
        assert_eq!(candidates[0].description, "ship a pr");
        assert!(candidates[0].classifiers.is_none());
    }

    #[test]
    fn an_unreadable_plan_is_skipped_and_an_unauthored_plan_is_still_pooled() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        // A directory where PLAN.md belongs: discovery sees the path, reading it
        // fails, so the plan never reaches the pool.
        let broken = home.path().join("broken");
        std::fs::create_dir_all(broken.join(PLAN_FILE_NAME)).unwrap();
        // No classification.json: the plan is offered with no questions of its own.
        let plain = home.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        std::fs::write(plain.join(PLAN_FILE_NAME), "just a body\n").unwrap();

        let pool = build_plan_pool(project.path(), home.path());
        assert_eq!(
            pool.iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["plain"]
        );
        assert_eq!(pool[0].content.trim(), "just a body");
        assert!(pool[0].classifiers.is_none());
    }

    fn pool_skill(name: &str, content: &str) -> crate::harness::classifier::DynamicSkill {
        crate::harness::classifier::DynamicSkill {
            name: name.to_string(),
            description: format!("{name} description"),
            content: content.to_string(),
            classifiers: None,
            requirements: crate::core::skill_requirements::SkillRequirements {
                known: false,
                required: std::collections::BTreeSet::new(),
            },
        }
    }

    #[test]
    fn selected_plans_carries_only_the_offered_and_selected_templates() {
        let pool = vec![
            pool_skill("ship-pr", "[1] baseline\n"),
            pool_skill("docs", "[1] read\n"),
        ];
        let selection = crate::harness::classifier::SkillSelection {
            selected: vec![
                ("ship-pr".to_string(), 0.9),
                // A name the pool never offered is dropped, exactly as the
                // skill path drops it.
                ("ghost".to_string(), 0.8),
            ],
            scored: Vec::new(),
            answers: Default::default(),
            warnings: Vec::new(),
        };

        let selected = selected_plans(&pool, &selection);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].name, "ship-pr");
        assert!(selected[0].content.contains("[1] baseline"));
    }

    #[test]
    fn build_plan_pool_skills_pools_plans_without_capability_requirements() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let dir = home.path().join("ship-pr");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(PLAN_FILE_NAME),
            "---\nname: ship-pr\ndescription: ship a pr\n---\n\n[1] baseline\n",
        )
        .unwrap();

        let pool = build_plan_pool_skills(project.path(), home.path());
        assert_eq!(pool.len(), 1);
        assert_eq!(pool[0].name, "ship-pr");
        assert!(pool[0].content.contains("[1] baseline"));
        assert!(pool[0].classifiers.is_none());
        // known:false is what `requirements_satisfied` admits, so a plan is
        // always eligible whatever tools the loop carries.
        assert!(!pool[0].requirements.known);
        assert!(pool[0].requirements.required.is_empty());
    }

    #[tokio::test]
    async fn plan_selection_is_a_noop_with_an_empty_pool() {
        let route = crate::harness::classifier::ClassifierRoute {
            url: "http://127.0.0.1:1/alpha/decisions".to_string(),
            model: "~typesafe/jev-latest".to_string(),
            headers: Vec::new(),
            timeout_ms: 5_000,
        };
        // No pool means no request: the call returns without touching the
        // unreachable route.
        let selection = select_plans(&route, serde_json::json!({"goal": "x"}), &[]).await;
        assert!(selection.selected.is_empty());
        assert!(selection.warnings.is_empty());
    }

    // --- the shipped ship-pr sidecar ---

    /// Scores the SHIPPED `plans/ship-pr/classification.json` for a set of
    /// classifier answers, resolving formula variables exactly as the docs
    /// define (README "In a formula, a bare question id resolves to that answer
    /// normalized to 0..1, and `id.member` resolves to one option/level
    /// probability").
    fn shipped_ship_pr_score(answers: &str) -> (f64, f64) {
        let sidecar: serde_json::Value =
            serde_json::from_str(default_plan_template("ship-pr").unwrap().1).unwrap();
        let relevance = &sidecar["relevance"];
        let formula = relevance["formula"].as_str().unwrap();
        let threshold = relevance["threshold"].as_f64().unwrap();
        let response: crate::harness::classifier::DecisionsResponse =
            serde_json::from_str(answers).unwrap();

        let resolve = |var: &str| match var.split_once('.') {
            Some((id, member)) => response
                .answers
                .get(id)
                .and_then(|answer| crate::harness::classifier::answer_member(answer, member)),
            None => response
                .answers
                .get(var)
                .map(crate::harness::classifier::answer_value),
        };

        (
            crate::harness::classifier::eval_formula(formula, &resolve).unwrap(),
            threshold,
        )
    }

    /// The shipped sidecar must actually REJECT a goal whose classifier answers
    /// say it does not end in a pull request: an all-NO goal has to land under
    /// the sidecar's own threshold, or the ship-pr template composes into a
    /// run that ships nothing. (An earlier formula read a `choice` question
    /// bare, which resolves to the chosen option's probability, so a unanimous
    /// "unrelated" answer still contributed a near-constant term.)
    #[test]
    fn the_shipped_ship_pr_formula_rejects_a_goal_that_does_not_deliver_a_pr() {
        let (score, threshold) = shipped_ship_pr_score(
            r#"{"model":"jev","answers":{
                "delivers_pr":{"type":"noul","noul":0.0},
                "build_then_pr":{"type":"noul","noul":0.0},
                "pr_mention":{"type":"noul","noul":0.0},
                "ship_language":{"type":"noul","noul":0.0}},"usage":{}}"#,
        );

        assert!(
            score < threshold,
            "an all-NO goal scored {score} >= the sidecar threshold {threshold}"
        );
    }

    /// A goal that merely MENTIONS pull requests (list them, summarize them)
    /// must not compose the template: the mention question is a minority
    /// weight, so on its own it stays under the threshold.
    #[test]
    fn the_shipped_ship_pr_formula_rejects_a_goal_that_only_mentions_prs() {
        let (score, threshold) = shipped_ship_pr_score(
            r#"{"model":"jev","answers":{
                "delivers_pr":{"type":"noul","noul":0.4},
                "build_then_pr":{"type":"noul","noul":0.1},
                "pr_mention":{"type":"noul","noul":1.0},
                "ship_language":{"type":"noul","noul":0.1}},"usage":{}}"#,
        );

        assert!(
            score < threshold,
            "a PR-mentioning goal scored {score} >= the sidecar threshold {threshold}"
        );
    }

    /// The positive control for the tests above: a goal every question agrees is
    /// a PR does compose the template, so the narrowing kept the formula alive.
    #[test]
    fn the_shipped_ship_pr_formula_selects_a_goal_that_delivers_a_pr() {
        let (score, threshold) = shipped_ship_pr_score(
            r#"{"model":"jev","answers":{
                "delivers_pr":{"type":"noul","noul":0.95},
                "build_then_pr":{"type":"noul","noul":0.9},
                "pr_mention":{"type":"noul","noul":0.9},
                "ship_language":{"type":"noul","noul":0.85}},"usage":{}}"#,
        );

        assert!(
            score >= threshold,
            "a ship-the-PR goal scored {score} < the sidecar threshold {threshold}"
        );
    }

    #[test]
    fn a_malformed_sidecar_is_warned_about_and_pooled_unauthored() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let dir = home.path().join("broken");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(PLAN_FILE_NAME), "body\n").unwrap();
        std::fs::write(dir.join(PLAN_CLASSIFICATION_FILE_NAME), "{ not json").unwrap();

        let plan = discover_plans(project.path(), home.path())
            .into_iter()
            .find(|plan| plan.name == "broken")
            .unwrap();
        let (classifiers, warning) = pool_classifiers(&plan);

        assert!(classifiers.is_none());
        let warning = warning.expect("a malformed sidecar must warn, not be swallowed");
        assert!(
            warning.contains("broken"),
            "warning names the plan: {warning}"
        );
        assert!(warning.contains("treating it as unauthored"));

        // None when absent, and no warning for a well-formed sidecar.
        let plain = home.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        std::fs::write(plain.join(PLAN_FILE_NAME), "body\n").unwrap();
        let plain_plan = discover_plans(project.path(), home.path())
            .into_iter()
            .find(|plan| plan.name == "plain")
            .unwrap();
        assert_eq!(pool_classifiers(&plain_plan), (None, None));

        let ok = home.path().join("authored");
        std::fs::create_dir_all(&ok).unwrap();
        std::fs::write(ok.join(PLAN_FILE_NAME), "body\n").unwrap();
        std::fs::write(
            ok.join(PLAN_CLASSIFICATION_FILE_NAME),
            r#"{"relevance":{"threshold":0.7,"questions":{},"formula":"1.0"}}"#,
        )
        .unwrap();
        let authored = discover_plans(project.path(), home.path())
            .into_iter()
            .find(|plan| plan.name == "authored")
            .unwrap();
        let (classifiers, warning) = pool_classifiers(&authored);
        assert!(classifiers.is_some());
        assert!(warning.is_none());
    }
}
