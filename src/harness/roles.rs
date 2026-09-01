// port of src/harness/roles.ts
//
// A role is a capability profile for a loop: which workspace tools that loop's
// subagent may call, what extra system-prompt material (persona + skills) it
// carries, which model route serves it, and how many cycles it gets. Roles are
// resolved by the caller (the CLI composes skills and model profiles into this
// runtime shape) so the harness itself stays free of skill and config formats.
//
// Port note — ModelRoute: src/harness/model-call.ts is not ported yet (its
// module stub is still a TODO), so the canonical ModelRoute type is defined
// here verbatim from model-call.ts:40-51. The `refreshHeaders` function field
// has no serde-representable equivalent in a plain struct; it is kept as a
// documented omission because the harness never serializes a route. When the
// model-call port lands it should re-export / replace this definition.

use std::collections::HashSet;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::core::types::HarnessTask;

/// Port of src/harness/model-call.ts `ModelRoute` (see the module note above).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRoute {
	/// Next gateway attempted when this route's retry ladder is exhausted; its own provider/url/model decide the request surface, and it may carry a fallback of its own (a chain).
	#[serde(skip_serializing_if = "Option::is_none")]
	pub fallback_route: Option<Box<ModelRoute>>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub headers: Option<IndexMap<String, String>>,
	pub model: String,
	/// Inference provider id (e.g. "claude", "openai"); "claude" routes through the native Anthropic API for prompt caching.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub provider: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub reasoning_effort: Option<String>,
	// `refreshHeaders?: () => Record<string, string>` — a dynamic-credential
	// callback ("cmd:" token) re-minted before each request. No data equivalent
	// in a serde struct; the port's callers never hold one at rest.
	pub url: String,
}

/// Per-role loop budget overrides (cycles, rounds, hot window, result caps) —
/// the TS `Partial<HarnessLoopConfig>` shape: every field independently
/// optional, serialized exactly like the full config (camelCase, and dropped
/// entirely when absent so JSON.stringify's undefined-drop still matches).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PartialHarnessLoopConfig {
	#[serde(skip_serializing_if = "Option::is_none")]
	pub hot_tool_results: Option<i64>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub max_cycles: Option<i64>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub max_tool_result_chars: Option<i64>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub max_tool_rounds_per_cycle: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessRoleRuntime {
	#[serde(skip_serializing_if = "Option::is_none")]
	pub description: Option<String>,
	/// Per-role loop budget overrides (cycles, rounds, hot window, result caps).
	#[serde(skip_serializing_if = "Option::is_none")]
	pub r#loop: Option<PartialHarnessLoopConfig>,
	pub name: String,
	/// Model route for this role's tool-bearing calls; falls back to the run's toolRoute, then the base model.
	/// Deliberately the canonical ModelRoute rather than a structural copy: a narrower local shape silently
	/// drops route fields (it cost us the gateway fallbackRoute once already).
	#[serde(skip_serializing_if = "Option::is_none")]
	pub route: Option<ModelRoute>,
	/// Already-composed prompt material (role instructions + skill sections) appended to the system prompt for this role's loops.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub system_prompt_suffix: Option<String>,
	/// Workspace tool allowlist. Omitted means every loaded tool; an empty array means no workspace tools (harness ops always remain).
	#[serde(skip_serializing_if = "Option::is_none")]
	pub tool_names: Option<Vec<String>>,
	/// Role that must review this role's completed tasks: finish_task(completed) spawns a review task under that role.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub verified_by: Option<String>,
}

/// Which role handles which built-in loop kind. Tasks may override per task via
/// their own role field (assigned by plan_tasks).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessRoleBindings {
	#[serde(skip_serializing_if = "Option::is_none")]
	pub planning: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub task: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessRoleSetup {
	#[serde(skip_serializing_if = "Option::is_none")]
	pub bindings: Option<HarnessRoleBindings>,
	/// Rejections a task may absorb before it is blocked for the user instead of reopened (default 2).
	#[serde(skip_serializing_if = "Option::is_none")]
	pub max_review_rounds: Option<f64>,
	pub roles: Vec<HarnessRoleRuntime>,
}

pub fn build_role_map(roles: Option<&[HarnessRoleRuntime]>) -> IndexMap<String, HarnessRoleRuntime> {
	let mut role_map = IndexMap::new();

	for role in roles.into_iter().flatten() {
		let name = role.name.trim().to_string();

		if !name.is_empty() && !role_map.contains_key(&name) {
			role_map.insert(name, role.clone());
		}
	}

	role_map
}

/// The role a specific loop runs under: the task's own assignment first, then
/// the binding for the loop kind (planning loops are the ones with no current
/// task). An unknown name resolves to undefined so the loop falls back to the
/// full default capability set instead of failing the run.
pub fn resolve_loop_role(
	role_map: &IndexMap<String, HarnessRoleRuntime>,
	current_task: Option<&HarnessTask>,
	bindings: Option<&HarnessRoleBindings>,
) -> Option<HarnessRoleRuntime> {
	let role_name = match current_task {
		Some(task) => task.role.clone().or_else(|| bindings.and_then(|b| b.task.clone())),
		None => bindings.and_then(|b| b.planning.clone()),
	};

	role_name.and_then(|name| role_map.get(&name).cloned())
}

/// The system prompt for a loop under a role: the composed base prompt plus the
/// role's own section, so every subagent still speaks the harness protocol while
/// carrying its role's persona, skills, and constraints.
pub fn compose_role_system_prompt(base_prompt: &str, role: Option<&HarnessRoleRuntime>) -> String {
	let Some(role) = role else {
		return base_prompt.to_string();
	};

	let heading = match &role.description {
		Some(description) => format!("# Role: {} — {}", role.name, description),
		None => format!("# Role: {}", role.name),
	};
	let body = role.system_prompt_suffix.as_deref().map(str::trim);
	let constraint = role.tool_names.as_deref().map(|tool_names| {
		if tool_names.is_empty() {
			"Your workspace tools are limited to this role's set (none — harness ops only); do not claim abilities outside it.".to_string()
		} else {
			format!(
				"Your workspace tools are limited to this role's set ({}); do not claim abilities outside it.",
				tool_names.join(", ")
			)
		}
	});
	let section_lines = [Some(heading), body.map(str::to_string), constraint]
		.into_iter()
		.flatten()
		.filter(|line| !line.is_empty())
		.collect::<Vec<_>>();

	format!("{}\n\n{}", base_prompt, section_lines.join("\n"))
}

/// Restricts the loaded workspace tools to a role's allowlist. Unknown names in
/// the allowlist are ignored here — the caller validates and reports them when
/// the role is defined, not on every loop.
pub fn filter_tools_for_role<T: NamedTool>(tools: Vec<T>, role: Option<&HarnessRoleRuntime>) -> Vec<T> {
	let Some(tool_names) = role.and_then(|role| role.tool_names.as_deref()) else {
		return tools;
	};

	let allowed: HashSet<&str> = tool_names.iter().map(String::as_str).collect();

	tools.into_iter().filter(|tool| allowed.contains(tool.name())).collect()
}

/// The TS generic bound `ToolType extends { name: string }`.
pub trait NamedTool {
	fn name(&self) -> &str;
}

#[cfg(test)]
mod tests {
	use super::*;

	fn role(name: &str) -> HarnessRoleRuntime {
		HarnessRoleRuntime {
			name: name.to_string(),
			..HarnessRoleRuntime {
				description: None,
				r#loop: None,
				name: String::new(),
				route: None,
				system_prompt_suffix: None,
				tool_names: None,
				verified_by: None,
			}
		}
	}

	fn task(id: &str, role_name: Option<&str>) -> HarnessTask {
		HarnessTask {
			id: id.to_string(),
			role: role_name.map(str::to_string),
			..serde_json::from_str::<HarnessTask>(
				r#"{"id":"","createdAtIteration":1,"notes":[],"stallCount":0,"status":"pending","title":""}"#,
			)
			.unwrap()
		}
	}

	#[derive(Debug, Clone, PartialEq)]
	struct Tool {
		name: String,
	}

	impl NamedTool for Tool {
		fn name(&self) -> &str {
			&self.name
		}
	}

	#[test]
	fn build_role_map_keeps_first_definition_per_trimmed_name() {
		let mut first = role(" reviewer ");
		first.name = " reviewer ".to_string();
		let mut second = role("reviewer");
		second.tool_names = Some(vec!["READ".to_string()]);
		let map = build_role_map(Some(&[first, second]));
		assert_eq!(map.len(), 1);
		assert!(map.get("reviewer").unwrap().tool_names.is_none());
	}

	#[test]
	fn build_role_map_handles_undefined() {
		assert!(build_role_map(None).is_empty());
	}

	#[test]
	fn resolve_loop_role_prefers_task_assignment_then_bindings() {
		let reviewer = role("reviewer");
		let planner = role("planner");
		let map = build_role_map(Some(&[reviewer.clone(), planner.clone()]));
		let bindings = HarnessRoleBindings {
			planning: Some("planner".to_string()),
			task: Some("reviewer".to_string()),
		};

		assert_eq!(
			resolve_loop_role(&map, Some(&task("t1", Some("reviewer"))), Some(&bindings)).map(|r| r.name),
			Some("reviewer".to_string())
		);
		assert_eq!(
			resolve_loop_role(&map, Some(&task("t1", None)), Some(&bindings)).map(|r| r.name),
			Some("reviewer".to_string())
		);
		assert_eq!(
			resolve_loop_role(&map, None, Some(&bindings)).map(|r| r.name),
			Some("planner".to_string())
		);
		assert_eq!(resolve_loop_role(&map, None, None), None);
		assert_eq!(resolve_loop_role(&map, Some(&task("t1", None)), None), None);
	}

	#[test]
	fn compose_role_system_prompt_appends_role_section() {
		assert_eq!(compose_role_system_prompt("base", None), "base");

		let mut reviewer = role("reviewer");
		reviewer.description = Some("independent verification".to_string());
		reviewer.system_prompt_suffix = Some("  Read it yourself.  ".to_string());
		reviewer.tool_names = Some(vec!["READ".to_string(), "GREP".to_string()]);
		let composed = compose_role_system_prompt("base", Some(&reviewer));
		assert_eq!(
			composed,
			"base\n\n# Role: reviewer — independent verification\nRead it yourself.\nYour workspace tools are limited to this role's set (READ, GREP); do not claim abilities outside it."
		);
	}

	#[test]
	fn compose_role_system_prompt_empty_allowlist_says_none() {
		let mut sandbox = role("sandbox");
		sandbox.tool_names = Some(vec![]);
		assert!(compose_role_system_prompt("base", Some(&sandbox))
			.contains("(none — harness ops only); do not claim abilities outside it."));
	}

	#[test]
	fn filter_tools_for_role_restricts_to_allowlist() {
		let tools = vec![
			Tool { name: "READ".to_string() },
			Tool { name: "PATCH".to_string() },
			Tool { name: "GREP".to_string() },
		];

		// No role / role without an allowlist keeps every tool.
		assert_eq!(filter_tools_for_role(tools.clone(), None), tools);
		assert_eq!(filter_tools_for_role(tools.clone(), Some(&role("open"))), tools);

		let mut reviewer = role("reviewer");
		reviewer.tool_names = Some(vec!["READ".to_string(), "MISSING".to_string()]);
		assert_eq!(
			filter_tools_for_role(tools, Some(&reviewer)),
			vec![Tool { name: "READ".to_string() }]
		);
	}
}
