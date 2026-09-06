// The CLI-facing role loader: user-authored RoleDefinition objects (config
// profiles, .drip/roles.json, or a marketplace plugin's agents/ directory)
// resolved into the harness's runtime shape (HarnessRoleRuntime), the built-in
// role presets ("reviewed", "research", "team", "planned"), and the --roles flag parser.
//
// NOTE on shared types: the minimal role-definition, preset, and binding
// shapes are defined here; model resolution goes through
// core::inference via
// resolve_model_profile_route at the bottom of this module.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

use crate::cli::marketplaces::MarketplaceRoleEntry;
use crate::cli::skills::{CliSkill, LoadedCliSkill, SkillRoleHints};
use crate::core::config::CliConfig;
use crate::core::inference::EnvSource;
use crate::harness::roles::{
	HarnessRoleBindings, HarnessRoleRuntime, ModelRoute, PartialHarnessLoopConfig,
};

// ---------------------------------------------------------------------------
// Setting-id constants
// ---------------------------------------------------------------------------

pub const MODEL_PROFILES_SETTING_ID: &str = "runtime.model_profiles";
pub const ROLE_PROFILES_SETTING_ID: &str = "runtime.role_profiles";
pub const ROLE_BINDINGS_SETTING_ID: &str = "runtime.role_bindings";

// ---------------------------------------------------------------------------
// RoleDefinition and resolution result shapes
// ---------------------------------------------------------------------------

/// A role definition as users author it (config profiles, .drip/roles.json, or
/// a marketplace plugin's agents/ directory). The CLI resolves skills, tools,
/// and model profile ids into the harness's runtime shape.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoleDefinition {
	#[serde(skip_serializing_if = "Option::is_none")]
	pub description: Option<String>,
	/// Per-role loop budget overrides.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub r#loop: Option<PartialHarnessLoopConfig>,
	/// Model profile id (from Model Profiles) for this role's loops.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub model: Option<String>,
	pub name: String,
	/// Extra system prompt material for this role's loops.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub prompt: Option<String>,
	/// Skill names composed into this role's system prompt.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub skills: Option<Vec<String>>,
	/// Workspace tool allowlist; omit for every tool, [] for harness ops only.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub tools: Option<Vec<String>>,
	/// Role that must review this role's completed tasks.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub verified_by: Option<String>,
}

/// Port of `RoleSetupSource`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoleSetupSource {
	#[serde(skip_serializing_if = "Option::is_none")]
	pub bindings: Option<HarnessRoleBindings>,
	#[serde(default)]
	pub roles: Vec<RoleDefinition>,
}

/// Port of `ResolvedRoleSetup` — `issues` collects every dangling reference so
/// callers can warn without aborting.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedRoleSetup {
	#[serde(skip_serializing_if = "Option::is_none")]
	pub bindings: Option<HarnessRoleBindings>,
	#[serde(default)]
	pub issues: Vec<String>,
	#[serde(default)]
	pub roles: Vec<HarnessRoleRuntime>,
}

// ---------------------------------------------------------------------------
// Tool-name rosters and preset profile pins
// ---------------------------------------------------------------------------

// The workspace tool pack a role allowlist is validated against.
// Harness ops (plan_tasks, finish_task, remember, ...) are deliberately absent:
// the loop appends them to every role unconditionally, so naming them here only
// produces a spurious "unknown tool(s)" issue on every preset run.
pub const ALL_STANDARD_TOOL_NAMES: [&str; 10] = [
	"READ", "PATCH", "DIR", "BASH", "BASH_ASYNC", "GREP", "VERIFY", "FETCH", "CHECK", "REFERENCE",
];

// Tools the pack carries only under a runtime condition (REFERENCE needs a
// configured corpus root). A role may name one, and a run without that
// condition simply drops it from the role's effective tools — naming it is
// not the typo the "unknown tool(s)" issue is meant to catch.
pub const OPTIONAL_TOOL_NAMES: [&str; 1] = ["REFERENCE"];

// Shared by every no-PATCH role so there is exactly one place that decides which
// tool the gate roles are denied. PATCH is the only journaled, --undo-last-able
// write path; BASH stays available, so this is "no journaled edits", not a
// sandbox (see the role prompts and the --roles help text).
pub fn read_only_tool_names() -> Vec<&'static str> {
	ALL_STANDARD_TOOL_NAMES
		.iter()
		.copied()
		.filter(|n| *n != "PATCH")
		.collect()
}

// The two model lanes every preset routes through. Pinned so a preset's routing
// is reproducible regardless of the user's profile catalog; the verifier lane
// is a different model (and vendor) from the drafting lane deliberately, so the
// verifying opinion never comes from the model that wrote the code — GLM-5.3
// Flash (Z.AI) drafts, Kimi K3 (Moonshot) reviews, the pairing the fork
// shipped; both lanes are served through OpenRouter, but the weights are still
// different vendors'. Both ids are shipped defaults in the profile catalog
// Both ids are shipped defaults in the profile catalog; --review reuses them
pub const PRESET_FAST_PROFILE_ID: &str = "glm-5-3-flash";
pub const PRESET_REVIEW_PROFILE_ID: &str = "kimi-k3";

// ---------------------------------------------------------------------------
// Shared role definitions reused across presets so the prompt text lives in
// exactly one place.
// ---------------------------------------------------------------------------

fn researcher_role() -> RoleDefinition {
	RoleDefinition {
		model: Some(PRESET_FAST_PROFILE_ID.to_string()),
		name: "researcher".to_string(),
		tools: Some(read_only_tool_names().into_iter().map(String::from).collect()),
		prompt: Some(
			[
				"You are the researcher agent. Your role is to investigate and report findings.",
				"",
				"## Investigate-and-cite skill",
				"- Investigate the assigned question and report findings with citations (file paths and line numbers).",
				"- Read the code yourself with READ/GREP/DIR — do not guess or rely on assumptions.",
				"- State uncertainty explicitly when the evidence is incomplete or ambiguous.",
				"- PATCH is not in your toolset: do not make edits. Report what you find so the other",
				"  roles can act on it. BASH is available for investigation (git log, rg, test runs) —",
				"  never use it to write, move, or delete files.",
			]
			.join("\n"),
		),
		..RoleDefinition::default()
	}
}

// The planning loop is not covered by the verify gate (it fires on finishing a
// TASK), so a planning role that can PATCH could ship unreviewed edits. Denying
// PATCH makes "only plan here" enforced rather than merely requested. "team"
// gets this for free — its planning role is the read-only researcher.
fn planner_role() -> RoleDefinition {
	RoleDefinition {
		model: Some(PRESET_FAST_PROFILE_ID.to_string()),
		name: "planner".to_string(),
		tools: Some(read_only_tool_names().into_iter().map(String::from).collect()),
		prompt: Some(
			[
				"You are the planning agent. Decompose the goal into tasks for the author role to implement.",
				"",
				"- Read enough of the code to plan concretely: name real files and real functions.",
				"- PATCH is not in your toolset; the implementing role makes the edits, so that every",
				"  change goes through the review gate. Plan only.",
			]
			.join("\n"),
		),
		..RoleDefinition::default()
	}
}

fn coder_role() -> RoleDefinition {
	RoleDefinition {
		model: Some(PRESET_FAST_PROFILE_ID.to_string()),
		name: "coder".to_string(),
		prompt: Some("You are the coder agent. Implement the task fully using all available tools, building on the researcher's findings. When you finish, your work will be independently reviewed by the reviewer role before it is accepted.".to_string()),
		verified_by: Some("reviewer".to_string()),
		// no tools field = full tool access
		..RoleDefinition::default()
	}
}

fn reviewer_role() -> RoleDefinition {
	RoleDefinition {
		model: Some(PRESET_REVIEW_PROFILE_ID.to_string()),
		name: "reviewer".to_string(),
		tools: Some(read_only_tool_names().into_iter().map(String::from).collect()),
		prompt: Some(
			[
				"You are the reviewer agent. Your role is to independently verify the implementing role's work.",
				"",
				"## Review-independently skill",
				"- Do NOT simply rubber-stamp the author's output.",
				"- Read all changed files yourself with READ/GREP/DIR — do not rely solely on the implementer's summary.",
				"- Run tests, type-checks, or linters with BASH/VERIFY to confirm correctness.",
				"- Identify regressions, missing edge cases, broken invariants, and style violations.",
				"- PATCH is not in your toolset: do not make edits. If changes are needed, clearly",
				"  describe what must be fixed so the implementing role can address them. BASH is",
				"  available for verification — never use it to modify the tree.",
				"- Accept only when you have verified the work passes all relevant checks.",
			]
			.join("\n"),
		),
		..RoleDefinition::default()
	}
}

// A preset can carry bindings alongside its roles so the CLI can wire loop
// kinds to preset roles without the user hand-editing .drip/roles.json.
// "planned": a stronger model writes the contracts, the fast lane executes
// them. The architect gets the review-lane model (kimi-k3) because planning
// for a small executor is where judgment pays; the author gets no reviewer
// (the finish gate's verification requirement still applies), so the run costs
// one strong planning loop plus fast task loops.
fn architect_role() -> RoleDefinition {
	RoleDefinition {
		model: Some(PRESET_REVIEW_PROFILE_ID.to_string()),
		name: "architect".to_string(),
		tools: Some(read_only_tool_names().into_iter().map(String::from).collect()),
		prompt: Some(
			[
				"You are the architect agent: a stronger model planning for a small, fast implementing model.",
				"Decompose the goal into tasks the author role can finish one at a time without judgment calls.",
				"",
				"- Read enough of the code to plan concretely: name real files, real functions, and the exact",
				"  verification command for each task.",
				"- Write each task title as a contract: what to change, where, what must not change, and how",
				"  it is verified (cargo test, bun run test, or the goal's own check).",
				"- Keep tasks small (one file or one behaviour each) and ordered so every task leaves the",
				"  build green.",
				"- PATCH is not in your toolset; the author role makes every edit. Plan only."
			]
			.join("\n"),
		),
		..RoleDefinition::default()
	}
}

fn planned_author_role() -> RoleDefinition {
	RoleDefinition {
		model: Some(PRESET_FAST_PROFILE_ID.to_string()),
		name: "author".to_string(),
		prompt: Some(
			[
				"You are the author agent. Implement the current task exactly as its contract says — the plan",
				"was written by a stronger model. If the contract mismatches what you find in the code, record",
				"the mismatch with observe and finish_task blocked instead of improvising. Run the named",
				"verification command before finish_task."
			]
			.join("\n"),
		),
		// no tools field = full tool access; no verified_by = no reviewer loop
		..RoleDefinition::default()
	}
}

fn builtin_preset(name: &str) -> Option<RoleSetupSource> {
	match name {
		"reviewed" => Some(RoleSetupSource {
			// Without bindings the preset was inert: planning and every task ran on the
			// base model with no role prompt, and the verify gate never fired unless the
			// planner happened to assign role: "author" itself.
			bindings: Some(HarnessRoleBindings {
				planning: Some("planner".to_string()),
				task: Some("author".to_string()),
			}),
			roles: vec![
				planner_role(),
				RoleDefinition {
					// Authors draft on the fast lane; the reviewer below pins a
					// different model so verification is not self-review.
					model: Some(PRESET_FAST_PROFILE_ID.to_string()),
					name: "author".to_string(),
					prompt: Some(
						[
							"You are the author agent. Implement the task fully using all available tools.",
							"When you finish, your work will be independently reviewed by the reviewer role",
							"before it is accepted.",
						]
						.join("\n"),
					),
					verified_by: Some("reviewer".to_string()),
					// no tools field = full tool access
					..RoleDefinition::default()
				},
				reviewer_role(),
			],
		}),
		"research" => Some(RoleSetupSource {
			bindings: Some(HarnessRoleBindings {
				planning: Some("researcher".to_string()),
				task: Some("researcher".to_string()),
			}),
			roles: vec![researcher_role()],
		}),
		"team" => Some(RoleSetupSource {
			bindings: Some(HarnessRoleBindings {
				planning: Some("researcher".to_string()),
				task: Some("coder".to_string()),
			}),
			roles: vec![researcher_role(), coder_role(), reviewer_role()],
		}),
		"planned" => Some(RoleSetupSource {
			bindings: Some(HarnessRoleBindings {
				planning: Some("architect".to_string()),
				task: Some("author".to_string()),
			}),
			roles: vec![architect_role(), planned_author_role()],
		}),
		_ => None,
	}
}

/// Returns the built-in role setup (roles plus optional loop-kind bindings) for
/// a named preset, or None if the preset name is not recognised.
pub fn builtin_role_preset(name: &str) -> Option<RoleSetupSource> {
	// hasOwn, not a bare index: `--roles constructor` would otherwise resolve to an
	// Object.prototype member instead of falling through to the path branch —
	// Rust's match arms are not prototype-pollutable, so a plain match suffices.
	builtin_preset(name)
}

/// The built-in preset names, for help text and error messages.
pub fn builtin_role_preset_names() -> Vec<&'static str> {
	vec!["reviewed", "research", "team", "planned"]
}

/// Resolves a --roles value to its role setup: a built-in preset name first, then
/// a path to a roles.json file. Kept out of the CLI entry path so the
/// flag's wiring — roles AND bindings, from both sources — is unit-testable.
/// Returns the underlying read/parse error for a path that cannot be loaded.
pub fn resolve_roles_flag(preset_or_path: &str) -> Result<RoleSetupSource> {
	if let Some(preset) = builtin_role_preset(preset_or_path) {
		return Ok(preset);
	}

	match load_roles_from_file(preset_or_path) {
		Ok(source) => Ok(source),
		Err(error) => {
			// A typo'd preset name is not a path the user ever meant to open, so a bare
			// ENOENT reads as nonsense. Name the presets when the value looks like one —
			// but only for a missing file: a file that WAS found and failed to parse must
			// report its own syntax error, not a list of preset names.
			let not_found = error
				.root_cause()
				.downcast_ref::<std::io::Error>()
				.map(|e| e.kind() == std::io::ErrorKind::NotFound)
				.unwrap_or(false);

			if not_found && !preset_or_path.contains('/') && !preset_or_path.ends_with(".json") {
				return Err(anyhow!(
					"\"{}\" is not a built-in preset ({}) and could not be read as a roles.json path",
					preset_or_path,
					builtin_role_preset_names().join(", ")
				));
			}

			Err(error)
		}
	}
}

/// Load role definitions from an external JSON file (same schema as
/// .drip/roles.json).  Returns the roles array on success; returns the read /
/// parse error so the caller can emit a clear CLI error.
pub fn load_roles_from_file(file_path: &str) -> Result<RoleSetupSource> {
	let raw =
		std::fs::read_to_string(file_path).with_context(|| format!("reading {file_path}"))?;
	let parsed: serde_json::Value =
		serde_json::from_str(&raw).with_context(|| format!("parsing {file_path}"))?;
	Ok(parse_role_setup_source(&parsed, file_path))
}

/// Shared body of loadRolesFromFile / loadProjectRoles: pull the roles array
/// through normalizeRoleDefinition and honour the file's bindings block.
fn parse_role_setup_source(parsed: &serde_json::Value, origin: &str) -> RoleSetupSource {
	let mut issues: Vec<String> = Vec::new();
	let mut roles: Vec<RoleDefinition> = Vec::new();

	if let Some(entries) = parsed.get("roles").and_then(|v| v.as_array()) {
		for entry in entries {
			if let Some(role) = normalize_role_definition(entry, origin, &mut issues) {
				roles.push(role);
			}
		}
	}

	// The file's bindings block is honoured like .drip/roles.json's: without it a
	// --roles file could define a planning role but never bind it, so the run
	// would silently plan on the base model.
	let bindings = normalize_bindings(parsed.get("bindings"));

	RoleSetupSource { bindings, roles }
}

fn normalize_role_definition(
	value: &serde_json::Value,
	origin: &str,
	issues: &mut Vec<String>,
) -> Option<RoleDefinition> {
	let name = value
		.get("name")
		.and_then(|v| v.as_str())
		.filter(|s| !s.trim().is_empty());
	let name = match name {
		Some(n) => n.to_string(),
		None => {
			issues.push(format!("{origin}: skipped a role with no name"));
			return None;
		}
	};
	let input = value;

	let mut r#loop = PartialHarnessLoopConfig::default();
	let mut has_loop = false;
	// `loop` must be a plain object, not an array must be a JSON object mapping, not an array.
	if let Some(loop_input) = input.get("loop").filter(|v| v.is_object()) {
		for field in ["hotToolResults", "maxCycles", "maxToolResultChars", "maxToolRoundsPerCycle"] {
			if let Some(n) = loop_input.get(field).and_then(|v| v.as_f64()) {
				match field {
					"hotToolResults" => r#loop.hot_tool_results = Some(n as i64),
					"maxCycles" => r#loop.max_cycles = Some(n as i64),
					"maxToolResultChars" => r#loop.max_tool_result_chars = Some(n as i64),
					"maxToolRoundsPerCycle" => r#loop.max_tool_rounds_per_cycle = Some(n as i64),
					_ => {}
				}
				has_loop = true;
			}
		}
	}

	fn string_list(field_value: Option<&serde_json::Value>) -> Option<Vec<String>> {
		let entries = field_value?.as_array()?;
		Some(
			entries
				.iter()
				.filter_map(|e| e.as_str())
				.map(|s| s.trim().to_string())
				.collect(),
		)
	}

	Some(RoleDefinition {
		name,
		description: input
			.get("description")
			.and_then(|v| v.as_str())
			.map(String::from),
		r#loop: if has_loop { Some(r#loop) } else { None },
		model: input
			.get("model")
			.and_then(|v| v.as_str())
			.map(str::trim)
			.filter(|s| !s.is_empty())
			.map(String::from),
		prompt: input
			.get("prompt")
			.and_then(|v| v.as_str())
			.map(String::from),
		skills: string_list(input.get("skills")),
		tools: input
			.get("tools")
			.and_then(|v| v.as_array())
			.map(|_| string_list(input.get("tools")).unwrap_or_default()),
		verified_by: input
			.get("verifiedBy")
			.and_then(|v| v.as_str())
			.map(str::trim)
			.filter(|s| !s.is_empty())
			.map(String::from),
	})
}

fn normalize_bindings(value: Option<&serde_json::Value>) -> Option<HarnessRoleBindings> {
	let value = value.filter(|v| v.is_object())?;
	let planning = value
		.get("planning")
		.and_then(|v| v.as_str())
		.map(str::trim)
		.filter(|s| !s.is_empty())
		.map(String::from);
	let task = value
		.get("task")
		.and_then(|v| v.as_str())
		.map(str::trim)
		.filter(|s| !s.is_empty())
		.map(String::from);

	// An empty bindings object is dropped entirely.
	(planning.is_some() || task.is_some()).then_some(HarnessRoleBindings { planning, task })
}

pub fn load_roles_from_config(config: &CliConfig, issues: &mut Vec<String>) -> RoleSetupSource {
	let mut roles: Vec<RoleDefinition> = Vec::new();
	let raw_profiles = config
		.settings
		.get(ROLE_PROFILES_SETTING_ID)
		.map(|s| s.trim())
		.filter(|s| !s.is_empty())
		.unwrap_or("[]");
	let raw_bindings = config
		.settings
		.get(ROLE_BINDINGS_SETTING_ID)
		.map(|s| s.trim())
		.filter(|s| !s.is_empty())
		.unwrap_or("{}");
	let mut bindings: Option<HarnessRoleBindings> = None;

	match serde_json::from_str::<serde_json::Value>(raw_profiles) {
		Ok(parsed_value) => {
			if let Some(entries) = parsed_value.as_array() {
				for entry in entries {
					if let Some(role) =
						normalize_role_definition(entry, "config role_profiles", issues)
					{
						roles.push(role);
					}
				}
			} else {
				issues.push("config role_profiles: expected a JSON array of role definitions".to_string());
			}
		}
		Err(_) => {
			issues.push("config role_profiles: not valid JSON".to_string());
		}
	}

	match serde_json::from_str::<serde_json::Value>(raw_bindings) {
		Ok(parsed_value) => {
			bindings = normalize_bindings(Some(&parsed_value));
		}
		Err(_) => {
			issues.push("config role_bindings: not valid JSON".to_string());
		}
	}

	RoleSetupSource { bindings, roles }
}

// Project roles live at <cwd>/.drip/roles.json: {"roles": [...], "bindings": {...}}.
pub fn load_project_roles(cwd: &str, issues: &mut Vec<String>) -> RoleSetupSource {
	let roles_path = Path::new(cwd).join(".drip").join("roles.json");

	if !roles_path.exists() {
		return RoleSetupSource::default();
	}

	match std::fs::read_to_string(&roles_path)
		.map_err(anyhow::Error::from)
		.and_then(|raw| {
			serde_json::from_str::<serde_json::Value>(&raw).map_err(anyhow::Error::from)
		}) {
		Ok(parsed_value) => parse_role_setup_source(&parsed_value, ".drip/roles.json"),
		Err(error) => {
			issues.push(format!(".drip/roles.json: {error}"));
			RoleSetupSource::default()
		}
	}
}

// A plugin agent file is the lowest-precedence role source: its body is the
// role prompt and its frontmatter tools line is the tool allowlist.
pub fn marketplace_role_to_definition(entry: &MarketplaceRoleEntry) -> RoleDefinition {
	RoleDefinition {
		description: entry.description.clone(),
		name: entry.name.clone(),
		prompt: Some(entry.prompt.clone()),
		tools: entry.tools.clone(),
		..RoleDefinition::default()
	}
}

/// Port of `ResolveRoleSetupArgs` — the inputs resolveRoleSetup merges.
#[derive(Debug, Clone)]
pub struct ResolveRoleSetupArgs<'a> {
	pub config: &'a CliConfig,
	pub cwd: String,
	/// Env source for model profile credential references ("env:NAME").
	pub env: EnvSource<'a>,
	/// Extra role definitions (from --roles preset or file) merged after all
	/// other sources — highest precedence, role names override config/project.
	pub extra_roles: Option<Vec<RoleDefinition>>,
	/// Loop-kind bindings from --roles, merged over config/project bindings.
	pub extra_bindings: Option<HarnessRoleBindings>,
	pub marketplace_roles: Option<Vec<MarketplaceRoleEntry>>,
	/// The discovered skill pool role skill references resolve against.
	pub skills: Vec<CliSkill>,
	/// Loaded workspace tool names, for allowlist validation.
	pub tool_names: Vec<String>,
}

// Merges role definitions (project > config > marketplace agents, by name),
// validates their references, and resolves them into the harness runtime
// shape: composed prompt material, tool allowlists, and model routes.
//
// `definition.model` resolution goes through resolve_model_profile_route at
// the bottom of this module (core::inference); an unresolvable profile
// reports the fallback issue to the caller,
// same as any other validation problem.
pub fn resolve_role_setup(args: &ResolveRoleSetupArgs) -> ResolvedRoleSetup {
	let mut issues: Vec<String> = Vec::new();
	let config_source = load_roles_from_config(args.config, &mut issues);
	let project_source = load_project_roles(&args.cwd, &mut issues);
	// First definition per name wins in merge order (marketplace < config <
	// project < extra), last write wins under a duplicate name: later
	// sources replace earlier ones under the same key.
	let mut definitions: indexmap::IndexMap<String, RoleDefinition> = indexmap::IndexMap::new();

	if let Some(entries) = &args.marketplace_roles {
		for entry in entries {
			definitions.insert(entry.name.clone(), marketplace_role_to_definition(entry));
		}
	}

	for role in &config_source.roles {
		definitions.insert(role.name.clone(), role.clone());
	}

	for role in &project_source.roles {
		definitions.insert(role.name.clone(), role.clone());
	}

	// Extra roles (from --roles flag) have the highest precedence.
	if let Some(extra) = &args.extra_roles {
		for role in extra {
			definitions.insert(role.name.clone(), role.clone());
		}
	}

	let mut skills_by_name: HashMap<String, &CliSkill> = HashMap::new();
	for skill in &args.skills {
		skills_by_name.insert(skill.name.clone(), skill);
	}
	// Roles whose model pin failed to resolve. A verifier in that set silently
	// reverts to the base model — i.e. the same model that produced the work —
	// so the independence the pin was buying is gone and must be said out loud.
	let mut unresolved_model_roles: HashSet<String> = HashSet::new();
	let known_tool_names: HashSet<&str> = args.tool_names.iter().map(String::as_str).collect();
	let mut roles: Vec<HarnessRoleRuntime> = Vec::new();

	for definition in definitions.values() {
		let mut prompt_sections: Vec<String> = Vec::new();

		if let Some(prompt) = definition.prompt.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
			prompt_sections.push(prompt.to_string());
		}

		if let Some(skill_names) = &definition.skills {
			for skill_name in skill_names {
				let Some(skill) = skills_by_name.get(skill_name) else {
					issues.push(format!(
						"role \"{}\": unknown skill \"{}\" (is its plugin enabled?)",
						definition.name, skill_name
					));
					continue;
				};

				match load_skill_content(skill, None) {
					Ok(loaded) => {
						// Compose through the shared composer so role-embedded
						// skills get the same "# Skill role hints (advisory)"
						// section as CLI/slash-activated skills (README contract).
						prompt_sections.push(crate::cli::skills::compose_skill_system_prompt(
							"", std::slice::from_ref(&loaded),
						));
					}
					Err(error) => {
						issues.push(format!(
							"role \"{}\": skill \"{}\" could not be loaded ({})",
							definition.name, skill_name, error
						));
					}
				}
			}
		}

		let mut tool_names: Option<Vec<String>> = None;

		if let Some(defs_tools) = &definition.tools {
			let unknown_tools: Vec<&str> = defs_tools
				.iter()
				.map(String::as_str)
				.filter(|tool_name| !known_tool_names.contains(tool_name))
				.filter(|tool_name| !OPTIONAL_TOOL_NAMES.contains(tool_name))
				.collect();

			if !unknown_tools.is_empty() {
				issues.push(format!(
					"role \"{}\": unknown tool(s) {} — not in the loaded tool pack",
					definition.name,
					unknown_tools.join(", ")
				));
			}

			tool_names = Some(
				defs_tools
					.iter()
					.filter(|tool_name| known_tool_names.contains(tool_name.as_str()))
					.cloned()
					.collect(),
			);
		}

		let mut route: Option<ModelRoute> = None;

		if let Some(model) = &definition.model {
			match resolve_model_profile_route(&args.config.settings, model, args.env) {
				Ok(resolved) => {
					// Spread rather than enumerate: a field-by-field copy silently drops
					// route fields the resolver adds (it dropped fallbackRoute once
					// already, quietly disabling failover for role models).
					route = Some(resolved);
				}
				Err(error) => {
					issues.push(format!(
						"role \"{}\": model profile \"{}\" could not be resolved ({}) — the role falls back to the run's base model",
						definition.name, model, error
					));
					unresolved_model_roles.insert(definition.name.clone());
				}
			}
		}

		roles.push(HarnessRoleRuntime {
			name: definition.name.clone(),
			description: definition.description.clone(),
			r#loop: definition.r#loop.clone(),
			route,
			system_prompt_suffix: (!prompt_sections.is_empty())
				.then(|| prompt_sections.join("\n\n")),
			tool_names,
			verified_by: definition.verified_by.clone(),
		});
	}

	// verifiedBy and bindings must point at roles that actually exist, or the
	// gate would silently never fire.
	let role_names: HashSet<String> = roles.iter().map(|role| role.name.clone()).collect();

	for role in &mut roles {
		if let Some(verified_by) = &role.verified_by {
			if role_names.contains(verified_by) && unresolved_model_roles.contains(verified_by) {
				issues.push(format!(
					"WARNING role \"{}\": its model pin did not resolve, so it now reviews \"{}\" on the run's base model — review independence is NOT enforced for this run",
					verified_by, role.name
				));
			}
		}

		if let Some(verified_by) = &role.verified_by {
			if !role_names.contains(verified_by) {
				issues.push(format!(
					"role \"{}\": verifiedBy \"{}\" is not a defined role — the verify gate is disabled for it",
					role.name, verified_by
				));
				role.verified_by = None;
			}
		}
	}

	// Bindings are merged from config, project, and extra —
	// later sources win, so extra > project > config.
	let mut merged_bindings = HarnessRoleBindings {
		planning: args
			.extra_bindings
			.as_ref()
			.and_then(|b| b.planning.clone())
			.or(project_source.bindings.as_ref().and_then(|b| b.planning.clone()))
			.or(config_source.bindings.as_ref().and_then(|b| b.planning.clone())),
		task: args
			.extra_bindings
			.as_ref()
			.and_then(|b| b.task.clone())
			.or(project_source.bindings.as_ref().and_then(|b| b.task.clone()))
			.or(config_source.bindings.as_ref().and_then(|b| b.task.clone())),
	};

	for kind in ["planning", "task"] {
		let bound = match kind {
			"planning" => merged_bindings.planning.clone(),
			_ => merged_bindings.task.clone(),
		};

		if let Some(bound) = bound {
			if !role_names.contains(&bound) {
				issues.push(format!(
					"role bindings: {kind} is bound to unknown role \"{bound}\" — the binding is ignored"
				));
				if kind == "planning" {
					merged_bindings.planning = None;
				} else {
					merged_bindings.task = None;
				}
			}
		}
	}

	let bindings = (merged_bindings.planning.is_some() || merged_bindings.task.is_some())
		.then_some(merged_bindings);

	ResolvedRoleSetup { bindings, issues, roles }
}

// ---------------------------------------------------------------------------
// Minimal local helpers for reading skill files and their frontmatter,
// ---------------------------------------------------------------------------

// Normalizes skill file content: strip a UTF-8 BOM, normalize CRLF to LF.
fn normalize_content(raw: &str) -> String {
	let stripped = raw.strip_prefix('\u{FEFF}').unwrap_or(raw);
	stripped.replace("\r\n", "\n")
}

/// Loads a skill's content: reads the skill file, then resolves the
/// frontmatter args block. No args block → content verbatim; with one, missing
/// required args error and `{{name}}` placeholders are substituted.
pub fn load_skill_content(
	skill: &CliSkill,
	args: Option<&HashMap<String, String>>,
) -> Result<LoadedCliSkill> {
	let raw = normalize_content(&std::fs::read_to_string(&skill.path)?);
	let arg_defs = parse_skill_frontmatter_args(&raw);

	// If the skill has NO args block, behave exactly as before (no substitution, no errors).
	let Some(arg_defs) = arg_defs else {
		return Ok(LoadedCliSkill {
			content: raw.trim().to_string(),
			name: skill.name.clone(),
			role_hints: parse_skill_roles_hints(&raw),
		});
	};

	// Validate supplied args against declared args.
	let mut declared: HashMap<String, Option<String>> = HashMap::new();
	for def in &arg_defs {
		declared.insert(def.name.clone(), def.default.clone());
	}

	if let Some(args) = args {
		for key in args.keys() {
			if !declared.contains_key(key) {
				return Err(anyhow!("Skill \"{}\": unknown arg \"{}\"", skill.name, key));
			}
		}
	}

	// Build the resolved values map and check for missing required args.
	let mut resolved: HashMap<String, String> = HashMap::new();
	for def in &arg_defs {
		match args.and_then(|a| a.get(&def.name)) {
			Some(supplied) => {
				resolved.insert(def.name.clone(), supplied.clone());
			}
			None if def.default.is_some() => {
				resolved.insert(def.name.clone(), def.default.clone().unwrap());
			}
			None => {
				return Err(anyhow!(
					"Skill \"{}\": missing required arg \"{}\"",
					skill.name,
					def.name
				));
			}
		}
	}

	// Substitute {{name}} placeholders in the content.
	let mut content = String::new();
	let mut rest = raw.trim();
	while let Some(start) = rest.find("{{") {
		content.push_str(&rest[..start]);
		let after = &rest[start + 2..];
		if let Some(end) = after.find("}}") {
			let key = &after[..end];
			let is_ident = !key.is_empty()
				&& (key.chars().next().unwrap().is_ascii_alphabetic() || key.chars().next().unwrap() == '_')
				&& key
					.chars()
					.all(|c| c.is_ascii_alphanumeric() || c == '_');
			if is_ident {
				content.push_str(resolved.get(key).map(String::as_str).unwrap_or(&format!("{{{{{key}}}}}")));
			} else {
				content.push_str("{{");
				content.push_str(key);
				rest = after;
				continue;
			}
			rest = &after[end + 2..];
		} else {
			content.push_str("{{");
			content.push_str(after);
			break;
		}
	}
	content.push_str(rest);

	Ok(LoadedCliSkill {
		content,
		name: skill.name.clone(),
		role_hints: parse_skill_roles_hints(&raw),
	})
}

/// A skill argument definition; `name` is required when no default is given.
#[derive(Debug, Clone, PartialEq)]
struct SkillArgDef {
	default: Option<String>,
	name: String,
}

/// Skill frontmatter args-block parser: reads
/// `args:` entries of the form `- name: value` / `- name` (string values only,
/// bare booleans are coerced to their string form).
fn parse_skill_frontmatter_args(markdown: &str) -> Option<Vec<SkillArgDef>> {
	let frontmatter = extract_frontmatter(markdown)?;
	let mut defs: Vec<SkillArgDef> = Vec::new();
	let mut in_args = false;

	for line in frontmatter.lines() {
		if line.starts_with("args:") {
			in_args = true;
			continue;
		}
		if in_args {
			let trimmed = line.trim();
			if trimmed.is_empty() {
				continue;
			}
			if !line.starts_with(' ') && !line.starts_with('\t') {
				// A new top-level frontmatter key ends the args block.
				break;
			}
			let Some(entry) = trimmed.strip_prefix("- ") else {
				break;
			};
			match entry.split_once(':') {
				Some((name, value)) => {
					defs.push(SkillArgDef {
						name: name.trim().to_string(),
						default: coerce_skill_arg_value(value),
					});
				}
				None => {
					defs.push(SkillArgDef {
						name: entry.trim().to_string(),
						default: None,
					});
				}
			}
		}
	}

	Some(defs)
}

/// Parse the advisory `roles:` block from a skill's frontmatter via the one
/// shared parser (`parse_skill_frontmatter`), so CLI, slash, and role-loaded
/// skills all agree on block syntax (two-space indent, blank line ends the
/// block, first `default:`/first occurrence of each stage wins).
fn parse_skill_roles_hints(markdown: &str) -> Option<SkillRoleHints> {
	crate::cli::skills::parse_skill_frontmatter(markdown).roles
}

fn extract_frontmatter(markdown: &str) -> Option<String> {
	let rest = markdown.strip_prefix("---\n")?;
	let end = rest.find("\n---")?;
	Some(rest[..end].to_string())
}

// Skills only ever author plain strings and booleans, so no YAML parser.
fn coerce_skill_arg_value(value: &str) -> Option<String> {
	let value = value.trim();
	let unquoted = value
		.strip_prefix('"')
		.and_then(|v| v.strip_suffix('"'))
		.or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')));
	match unquoted.unwrap_or(value) {
		"true" => Some("true".to_string()),
		"false" => Some("false".to_string()),
		"" => None,
		v => Some(v.to_string()),
	}
}

// ---------------------------------------------------------------------------
// Model-profile resolution lives in core::inference (see the
// resolve_role_setup note above).
// ---------------------------------------------------------------------------

// Model-profile route resolution, via core::inference; the resolved
// route is re-shaped into the serializable roles::ModelRoute (headers as an
// ordered map, fallback chained recursively).
fn resolve_model_profile_route(
	settings: &indexmap::IndexMap<String, String>,
	model_profile_id: &str,
	env: EnvSource<'_>,
) -> Result<ModelRoute> {
	fn convert(route: &crate::core::inference::ResolvedModelRoute) -> ModelRoute {
		ModelRoute {
			fallback_route: route.fallback_route.as_ref().map(|inner| Box::new(convert(inner))),
			headers: Some(route.headers.iter().cloned().collect()),
			model: route.model.clone(),
			provider: Some(route.provider.clone()),
			reasoning_effort: route.reasoning_effort.clone(),
			url: route.url.clone(),
		}
	}

	let resolved = crate::core::inference::resolve_model_profile_route(settings, model_profile_id, env)?;

	Ok(convert(&resolved))
}
