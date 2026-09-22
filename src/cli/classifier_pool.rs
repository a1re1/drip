//! Shared skill-classifier pool construction.
//!
//! Both the headless CLI (`src/cli/entry.rs`) and the interactive TUI
//! (`src/tui/app.rs`) resolve the classifier route, discover the candidate
//! skills, run the cached capability-requirements pass, and hand the harness a
//! route plus a pool of `DynamicSkill`s. This module owns that one sequence so
//! the two callers cannot drift apart.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

use indexmap::IndexMap;

use crate::cli::marketplaces::discover_all_skills;
use crate::cli::roles::load_skill_content;
use crate::cli::skills::CliSkill;
use crate::core::home::DripHome;
use crate::core::skill_requirements::{ensure_requirements, Capability, SkillRequirements};
use crate::harness::classifier::{
    classifier_profile_id, load_skill_classifiers, resolve_classifier_route, ClassifierRoute,
    DeclaredRequirements, DynamicSkill, SkillClassifiers,
};
use crate::tools::types::ChatToolDefinition;

/// Everything the pool pass needs from its caller.
pub struct ClassifierPoolArgs<'a> {
    /// Persisted settings (`runtime.classifier_profile_id` / timeout).
    pub settings: &'a IndexMap<String, String>,
    /// Merged environment: the run's credentials live here.
    pub env: &'a HashMap<String, String>,
    /// `--classifier <profile-id>` override for this run.
    pub profile_override: Option<&'a str>,
    /// `--no-classifier`: hard-disable regardless of the configured profile.
    pub disabled: bool,
    pub cwd: &'a str,
    pub home: &'a DripHome,
    /// Skill names already active in the base prompt (excluded from the pool).
    pub active_skill_names: &'a HashSet<String>,
    /// The run's FULL tool pack: the capability list offered to the
    /// requirements pass (the plan-narrowed surface is a runtime filter).
    pub tools: &'a [ChatToolDefinition],
    /// Spawned MCP servers as (server name, tool names).
    pub mcp_servers: Vec<(String, Vec<String>)>,
}

/// The resolved classifier for one run. Every field is inert when `route` is
/// `None`: classification is off, so the pool is empty and the run proceeds
/// with only the explicitly activated skills.
#[derive(Default)]
pub struct ClassifierPool {
    pub route: Option<ClassifierRoute>,
    pub profile_id: String,
    pub skills: Vec<DynamicSkill>,
    /// The operator-facing `announce` line for stderr (or the TUI transcript),
    /// `None` when no route resolved.
    pub announce: Option<String>,
    /// Non-fatal problems, verbatim messages the caller should surface.
    pub warnings: Vec<String>,
}

/// Resolves the route, discovers the pool and caches per-skill requirements.
/// Never fatal: a bad profile, a failed discovery or a failed requirements
/// request warn and the run continues with no classifier skills.
pub async fn build_classifier_pool(args: ClassifierPoolArgs<'_>) -> ClassifierPool {
    let mut pool = ClassifierPool::default();

    if args.disabled {
        return pool;
    }

    let route = match resolve_classifier_route(args.settings, Some(args.env), args.profile_override)
    {
        Ok(route) => route,
        Err(error) => {
            pool.warnings.push(format!(
                "{error} — skill classification is disabled for this run"
            ));
            return pool;
        }
    };

    let Some(route) = route else {
        return pool;
    };

    pool.profile_id = args
        .profile_override
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| classifier_profile_id(args.settings))
        .unwrap_or_default();

    let discovered = match discover_all_skills(Path::new(args.cwd), args.home) {
        Ok(discovered) => discovered,
        Err(error) => {
            pool.warnings.push(format!(
                "classifier: skill discovery failed ({error}) — the pool is empty for this run"
            ));
            Vec::new()
        }
    };

    let mut candidates: Vec<(CliSkill, String, Option<DeclaredRequirements>)> = Vec::new();
    let mut authored: HashMap<String, Option<SkillClassifiers>> = HashMap::new();

    for skill in discovered {
        // Explicit skill activations are already in the base prompt; the pool is
        // only what the classifier may add on top.
        if args.active_skill_names.contains(skill.name.as_str()) {
            continue;
        }

        let loaded = match load_skill_content(&skill, None) {
            Ok(loaded) => loaded,
            Err(error) => {
                pool.warnings
                    .push(format!("classifier: {error} — skipping this skill"));
                continue;
            }
        };

        let classifiers = match load_skill_classifiers(&skill.path) {
            None => None,
            Some(Ok(classifiers)) => Some(classifiers),
            Some(Err(error)) => {
                pool.warnings.push(format!(
                    "classifier: skill \"{}\": {error} — treating it as unauthored",
                    skill.name
                ));
                None
            }
        };

        let declared = classifiers
            .as_ref()
            .and_then(|set| set.requirements.clone());
        authored.insert(skill.name.clone(), classifiers);
        candidates.push((skill, loaded.content, declared));
    }

    // The capability list is every tool the run's FULL pack carries plus
    // DELEGATE, then one entry per MCP server that actually spawned.
    let mut capabilities: Vec<Capability> = args
        .tools
        .iter()
        .map(|tool| Capability::Tool {
            description: tool.description.clone(),
            name: tool.name.clone(),
        })
        .collect();
    capabilities.push(Capability::Tool {
        description: "Delegate a self-contained subtask to a fresh child session".to_string(),
        name: "DELEGATE".to_string(),
    });
    for (name, tool_names) in args.mcp_servers {
        capabilities.push(Capability::McpServer { name, tool_names });
    }

    let (requirements, warnings) = ensure_requirements(
        Path::new(&args.home.skill_requirements_db_path),
        &route,
        &candidates,
        &capabilities,
    )
    .await;
    pool.warnings.extend(warnings);

    let pool_count = candidates.len();

    for (skill, content, _) in candidates {
        let skill_requirements =
            requirements
                .get(&skill.name)
                .cloned()
                .unwrap_or_else(|| SkillRequirements {
                    known: false,
                    required: BTreeSet::new(),
                });
        pool.skills.push(DynamicSkill {
            classifiers: authored.remove(&skill.name).flatten(),
            content,
            description: skill.description.clone(),
            name: skill.name.clone(),
            requirements: skill_requirements,
        });
    }

    pool.announce = Some(format!(
        "classifier: {} ({}) → {} · {} skills in pool",
        pool.profile_id, route.model, route.url, pool_count
    ));
    pool.route = Some(route);

    pool
}

/// Whether the interactive TUI (`drip --tui`) builds a classifier pool: the
/// config gate AND the `--no-classifier` hard off. The headless path applies
/// the same precedence — the flag wins over both the setting and
/// `--classifier` — so a `--no-classifier` run behaves identically on both
/// surfaces.
pub fn tui_classifier_pool_enabled(
    settings: &IndexMap<String, String>,
    no_classifier_flag: bool,
) -> bool {
    !no_classifier_flag && classifier_enabled_in_tui(settings)
}

/// Whether the interactive TUI (`drip --tui`) runs the classifier too.
///
/// The decision is config-driven on both surfaces: a resolved profile is what
/// turns classification on, and `runtime.classifier_in_tui = "false"` keeps
/// the TUI on its explicit `/skill` toggles only. An absent setting leaves the
/// TUI enabled whenever a profile resolves, matching the headless CLI.
pub fn classifier_enabled_in_tui(settings: &IndexMap<String, String>) -> bool {
    match settings
        .get(crate::core::config::CLASSIFIER_IN_TUI_SETTING_ID)
        .map(|value| value.trim().to_ascii_lowercase())
    {
        Some(value) if matches!(value.as_str(), "0" | "false" | "no" | "off") => false,
        _ => true,
    }
}

#[cfg(test)]
mod classifier_pool_tests {
    use super::*;

    fn settings(pairs: &[(&str, &str)]) -> IndexMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn tui_classifier_is_on_by_default() {
        assert!(classifier_enabled_in_tui(&settings(&[])));
        let explicit = settings(&[(crate::core::config::CLASSIFIER_IN_TUI_SETTING_ID, "true")]);
        assert!(classifier_enabled_in_tui(&explicit));
    }

    #[test]
    fn tui_no_classifier_flag_beats_every_setting() {
        // `--no-classifier` is a hard off, exactly as headless.
        assert!(!tui_classifier_pool_enabled(&settings(&[]), true));
        assert!(!tui_classifier_pool_enabled(
            &settings(&[(crate::core::config::CLASSIFIER_IN_TUI_SETTING_ID, "true")]),
            true
        ));
        // Without the flag the config gate is unchanged.
        assert!(tui_classifier_pool_enabled(&settings(&[]), false));
        assert!(!tui_classifier_pool_enabled(
            &settings(&[(crate::core::config::CLASSIFIER_IN_TUI_SETTING_ID, "off")]),
            false
        ));
    }

    #[test]
    fn disabled_args_build_an_inert_pool_without_touching_disk_or_network() {
        // `disabled: true` returns before route resolution, discovery and the
        // requirements pass, so a bogus home/cwd is never consulted.
        let settings = settings(&[]);
        let env = std::collections::HashMap::new();
        let active = HashSet::new();
        let home = crate::core::home::open_drip_home("/nonexistent-drip-home-for-tests");
        let tools: Vec<ChatToolDefinition> = Vec::new();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let pool = runtime.block_on(build_classifier_pool(ClassifierPoolArgs {
            active_skill_names: &active,
            cwd: "/nonexistent-drip-cwd-for-tests",
            disabled: true,
            env: &env,
            home: &home,
            mcp_servers: Vec::new(),
            profile_override: Some("jev"),
            settings: &settings,
            tools: &tools,
        }));
        assert!(pool.route.is_none());
        assert!(pool.profile_id.is_empty());
        assert!(pool.skills.is_empty());
        assert!(pool.announce.is_none());
        assert!(pool.warnings.is_empty());
    }

    #[test]
    fn tui_classifier_honors_explicit_off_values() {
        for value in ["false", "FALSE ", "0", "no", "off"] {
            let s = settings(&[(crate::core::config::CLASSIFIER_IN_TUI_SETTING_ID, value)]);
            assert!(!classifier_enabled_in_tui(&s), "{value:?} must disable");
        }
    }
}
