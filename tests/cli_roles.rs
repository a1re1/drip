// End-to-end integration tests that run the full harness loop are not
// included here; this file covers the pure role-resolution layer.

use std::collections::HashMap;

use drip::core::config::CliConfig;
use std::fs;
use std::path::PathBuf;

use drip::cli::marketplaces::MarketplaceRoleEntry;
use drip::cli::roles::{load_skill_content as load_roles_skill_content, RoleDefinition};
use drip::cli::skills::{
    load_skill_content as load_cli_skill_content, CliSkill, SkillSource,
};
use drip::cli::roles::{
    builtin_role_preset, load_roles_from_file, resolve_role_setup,
    resolve_roles_flag, ResolveRoleSetupArgs,
    PRESET_FAST_PROFILE_ID, PRESET_REVIEW_PROFILE_ID, ROLE_BINDINGS_SETTING_ID,
    ROLE_PROFILES_SETTING_ID,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_temp_root(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("{}{}", prefix, uuid::Uuid::new_v4()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn default_config() -> CliConfig {
    CliConfig {
        status_line: None,
    hooks: drip::harness::hooks::HooksConfig::default(),
        path: None,
        settings: indexmap::IndexMap::new(),
        mcp_servers: std::collections::BTreeMap::new(),
        version: Some(1),
    }
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

#[test]
fn builtin_role_preset_returns_none_for_unknown() {
    assert!(builtin_role_preset("nonexistent").is_none());
    assert!(builtin_role_preset("").is_none());
    assert!(builtin_role_preset("REVIEWED").is_none()); // case-sensitive
}

#[test]
fn builtin_role_preset_does_not_treat_prototype_members_as_presets() {
    assert!(builtin_role_preset("constructor").is_none());
    assert!(builtin_role_preset("toString").is_none());
}

#[test]
fn reviewed_preset_returns_planner_author_reviewer() {
    let setup = builtin_role_preset("reviewed").unwrap();
    let names: Vec<&str> = setup.roles.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, vec!["planner", "author", "reviewer"]);
}

#[test]
fn reviewed_preset_planning_role_cannot_patch() {
    let setup = builtin_role_preset("reviewed").unwrap();
    let bindings = setup.bindings.as_ref().unwrap();
    assert_eq!(bindings.planning.as_deref(), Some("planner"));
    assert_eq!(bindings.task.as_deref(), Some("author"));
    let planner = setup.roles.iter().find(|r| r.name == "planner").unwrap();
    let tools = planner.tools.as_ref().unwrap();
    assert!(!tools.contains(&"PATCH".to_string()));
}

#[test]
fn planned_preset_strong_architect_plans_fast_author_executes() {
    let setup = builtin_role_preset("planned").unwrap();
    let bindings = setup.bindings.as_ref().unwrap();
    assert_eq!(bindings.planning.as_deref(), Some("architect"));
    assert_eq!(bindings.task.as_deref(), Some("author"));
    let architect = setup.roles.iter().find(|r| r.name == "architect").unwrap();
    let author = setup.roles.iter().find(|r| r.name == "author").unwrap();
    assert_eq!(architect.model.as_deref(), Some(PRESET_REVIEW_PROFILE_ID));
    assert!(!architect.tools.as_ref().unwrap().contains(&"PATCH".to_string()));
    assert!(architect.prompt.as_deref().unwrap().contains("exact\n  verification command"));
    assert_eq!(author.model.as_deref(), Some(PRESET_FAST_PROFILE_ID));
    assert!(author.tools.is_none());
    assert!(author.verified_by.is_none());
    assert!(author.prompt.as_deref().unwrap().contains("finish_task blocked instead of improvising"));
}

#[test]
fn every_preset_planning_role_is_denied_patch() {
    for preset_name in ["reviewed", "research", "team", "planned"] {
        let setup = builtin_role_preset(preset_name).unwrap();
        let bindings = setup.bindings.as_ref().unwrap();
        let planning_name = bindings.planning.as_ref().unwrap();
        let planning = setup.roles.iter().find(|r| &r.name == planning_name).unwrap();
        let tools = planning.tools.as_ref().unwrap();
        assert!(
            !tools.contains(&"PATCH".to_string()),
            "preset {preset_name} planning role {planning_name} may PATCH"
        );
    }
}

#[test]
fn reviewed_preset_author_has_verified_by_reviewer_and_no_tools_restriction() {
    let roles = builtin_role_preset("reviewed").unwrap().roles;
    let author = roles.iter().find(|r| r.name == "author").unwrap();
    assert_eq!(author.verified_by.as_deref(), Some("reviewer"));
    // No tools field means full tool access
    assert!(author.tools.is_none());
}

#[test]
fn reviewed_preset_reviewer_lacks_patch_but_retains_standard_tools() {
    let roles = builtin_role_preset("reviewed").unwrap().roles;
    let reviewer = roles.iter().find(|r| r.name == "reviewer").unwrap();
    let tools = reviewer.tools.as_ref().unwrap();
    assert!(!tools.contains(&"PATCH".to_string()));
    assert!(tools.contains(&"READ".to_string()));
    assert!(tools.contains(&"BASH".to_string()));
    assert!(tools.contains(&"GREP".to_string()));
    assert!(tools.contains(&"DIR".to_string()));
}

#[test]
fn reviewed_preset_roles_pinned_to_expected_model_profiles() {
    let roles = builtin_role_preset("reviewed").unwrap().roles;
    let author = roles.iter().find(|r| r.name == "author").unwrap();
    let reviewer = roles.iter().find(|r| r.name == "reviewer").unwrap();
    assert_eq!(author.model.as_deref(), Some(PRESET_FAST_PROFILE_ID));
    assert_eq!(reviewer.model.as_deref(), Some(PRESET_REVIEW_PROFILE_ID));
    // Review independence: the verifier is never the drafting model
    assert_ne!(PRESET_REVIEW_PROFILE_ID, PRESET_FAST_PROFILE_ID);
}

#[test]
fn research_preset_returns_one_read_only_researcher_with_researcher_only_bindings() {
    let setup = builtin_role_preset("research").unwrap();
    assert_eq!(setup.roles.len(), 1);
    let researcher = &setup.roles[0];
    assert_eq!(researcher.name, "researcher");
    let tools = researcher.tools.as_ref().unwrap();
    assert!(!tools.contains(&"PATCH".to_string()));
    assert!(tools.contains(&"READ".to_string()));
    assert!(tools.contains(&"GREP".to_string()));
    assert!(tools.contains(&"BASH".to_string()));
    assert!(tools.contains(&"FETCH".to_string()));
    assert!(tools.contains(&"VERIFY".to_string()));
    let bindings = setup.bindings.as_ref().unwrap();
    assert_eq!(bindings.planning.as_deref(), Some("researcher"));
    assert_eq!(bindings.task.as_deref(), Some("researcher"));
}

#[test]
fn research_preset_researcher_pinned_to_fast_lane_model_profile() {
    let researcher = &builtin_role_preset("research").unwrap().roles[0];
    assert_eq!(researcher.model.as_deref(), Some(PRESET_FAST_PROFILE_ID));
}

#[test]
fn team_preset_returns_researcher_coder_reviewer_in_order_with_expected_bindings() {
    let setup = builtin_role_preset("team").unwrap();
    let names: Vec<&str> = setup.roles.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, vec!["researcher", "coder", "reviewer"]);
    let bindings = setup.bindings.as_ref().unwrap();
    assert_eq!(bindings.planning.as_deref(), Some("researcher"));
    assert_eq!(bindings.task.as_deref(), Some("coder"));
}

#[test]
fn team_preset_coder_has_full_tool_access_and_is_verified_by_reviewer() {
    let coder = builtin_role_preset("team")
        .unwrap()
        .roles
        .into_iter()
        .find(|r| r.name == "coder")
        .unwrap();
    // No tools field means full tool access
    assert!(coder.tools.is_none());
    assert_eq!(coder.verified_by.as_deref(), Some("reviewer"));
}

#[test]
fn team_preset_reviewer_lacks_patch_but_retains_other_standard_tools() {
    let reviewer = builtin_role_preset("team")
        .unwrap()
        .roles
        .into_iter()
        .find(|r| r.name == "reviewer")
        .unwrap();
    let tools = reviewer.tools.as_ref().unwrap();
    assert!(!tools.contains(&"PATCH".to_string()));
}

#[test]
fn team_preset_roles_pinned_to_expected_model_profiles() {
    let roles = builtin_role_preset("team").unwrap().roles;
    let researcher = roles.iter().find(|r| r.name == "researcher").unwrap();
    let coder = roles.iter().find(|r| r.name == "coder").unwrap();
    let reviewer = roles.iter().find(|r| r.name == "reviewer").unwrap();
    assert_eq!(researcher.model.as_deref(), Some(PRESET_FAST_PROFILE_ID));
    assert_eq!(coder.model.as_deref(), Some(PRESET_FAST_PROFILE_ID));
    assert_eq!(reviewer.model.as_deref(), Some(PRESET_REVIEW_PROFILE_ID));
}

#[test]
fn every_preset_binds_both_loop_kinds_to_a_role_it_defines() {
    for preset_name in ["reviewed", "research", "team", "planned"] {
        let preset = builtin_role_preset(preset_name).unwrap();
        let names: std::collections::HashSet<&str> =
            preset.roles.iter().map(|r| r.name.as_str()).collect();
        let bindings = preset.bindings.as_ref().unwrap();
        assert!(
            names.contains(bindings.planning.as_deref().unwrap()),
            "preset {preset_name} has no roles"
        );
        assert!(
            names.contains(bindings.task.as_deref().unwrap()),
            "preset {preset_name} has no roles"
        );
    }
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

#[test]
fn load_roles_from_file_reads_bindings_block_alongside_roles() {
    let root = make_temp_root("drip-roles-file-");
    let roles_path = root.join("roles.json");
    fs::write(
        &roles_path,
        r#"{"bindings":{"planning":"architect","task":"author"},"roles":[{"name":"architect","model":"planner-model"},{"name":"author"}]}"#,
    )
    .unwrap();

    let loaded = load_roles_from_file(roles_path.to_str().unwrap()).unwrap();
    let names: Vec<&str> = loaded.roles.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, vec!["architect", "author"]);
    let bindings = loaded.bindings.as_ref().unwrap();
    assert_eq!(bindings.planning.as_deref(), Some("architect"));
    assert_eq!(bindings.task.as_deref(), Some("author"));
}

#[test]
fn load_roles_from_file_reads_the_replanning_binding() {
    let root = make_temp_root("drip-roles-file-");
    let roles_path = root.join("roles.json");
    fs::write(
        &roles_path,
        r#"{"bindings":{"planning":"architect","replanning":"scout","task":"author"},"roles":[{"name":"architect"},{"name":"scout"},{"name":"author"}]}"#,
    )
    .unwrap();

    let loaded = load_roles_from_file(roles_path.to_str().unwrap()).unwrap();
    let bindings = loaded.bindings.as_ref().unwrap();
    assert_eq!(bindings.replanning.as_deref(), Some("scout"));

    // A replanning binding to an unknown role is dropped with an issue, and
    // the other bindings survive.
    let mut config = default_config();
    config.settings.insert(
        ROLE_PROFILES_SETTING_ID.to_string(),
        r#"[{"name":"architect"},{"name":"author"}]"#.to_string(),
    );
    config.settings.insert(
        ROLE_BINDINGS_SETTING_ID.to_string(),
        r#"{"planning":"architect","replanning":"ghost","task":"author"}"#.to_string(),
    );
    let setup = resolve_role_setup(&ResolveRoleSetupArgs {
        config: &config,
        cwd: root.to_str().unwrap().to_string(),
        env: None,
        extra_roles: None,
        extra_bindings: None,
        marketplace_roles: None,
        skills: vec![],
        tool_names: vec![],
        mcp_server_names: vec![],
    });
    let bindings = setup.bindings.as_ref().unwrap();
    assert_eq!(bindings.planning.as_deref(), Some("architect"));
    assert_eq!(bindings.replanning, None);
    assert_eq!(bindings.task.as_deref(), Some("author"));
    assert!(setup.issues.iter().any(|issue| issue.contains("replanning is bound to unknown role \"ghost\"")), "{:?}", setup.issues);

    // A valid config-level replanning binding survives the merge.
    config.settings.insert(
        ROLE_PROFILES_SETTING_ID.to_string(),
        r#"[{"name":"architect"},{"name":"scout"},{"name":"author"}]"#.to_string(),
    );
    config.settings.insert(
        ROLE_BINDINGS_SETTING_ID.to_string(),
        r#"{"planning":"architect","replanning":"scout","task":"author"}"#.to_string(),
    );
    let setup = resolve_role_setup(&ResolveRoleSetupArgs {
        config: &config,
        cwd: root.to_str().unwrap().to_string(),
        env: None,
        extra_roles: None,
        extra_bindings: None,
        marketplace_roles: None,
        skills: vec![],
        tool_names: vec![],
        mcp_server_names: vec![],
    });
    assert_eq!(setup.bindings.as_ref().unwrap().replanning.as_deref(), Some("scout"));
    assert!(setup.issues.is_empty(), "{:?}", setup.issues);
}

#[test]
fn load_roles_from_file_returns_no_bindings_for_file_omitting_the_block() {
    let root = make_temp_root("drip-roles-file-");
    let roles_path = root.join("roles.json");
    fs::write(&roles_path, r#"{"roles":[{"name":"author"}]}"#).unwrap();

    let loaded = load_roles_from_file(roles_path.to_str().unwrap()).unwrap();
    assert!(loaded.bindings.is_none());
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

#[test]
fn resolve_roles_flag_resolves_preset_name_to_setup_with_roles_and_bindings() {
    for preset_name in ["reviewed", "research", "team", "planned"] {
        let resolved = resolve_roles_flag(preset_name).unwrap();
        assert!(!resolved.roles.is_empty());
        let bindings = resolved.bindings.as_ref().unwrap();
        assert!(bindings.planning.is_some());
        assert!(bindings.task.is_some());
    }
}

#[test]
fn resolve_roles_flag_falls_through_to_roles_json_path_bindings_included() {
    let root = make_temp_root("drip-roles-test-");
    let roles_path = root.join("custom-roles.json");
    fs::write(
        &roles_path,
        r#"{"bindings":{"planning":"scout","task":"scout"},"roles":[{"name":"scout","tools":["READ"]}]}"#,
    )
    .unwrap();

    let resolved = resolve_roles_flag(roles_path.to_str().unwrap()).unwrap();
    let names: Vec<&str> = resolved.roles.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, vec!["scout"]);
    let bindings = resolved.bindings.as_ref().unwrap();
    assert_eq!(bindings.planning.as_deref(), Some("scout"));
    assert_eq!(bindings.task.as_deref(), Some("scout"));
}

#[test]
fn resolve_roles_flag_names_builtin_presets_when_preset_shaped_value_is_not_one() {
    let err = resolve_roles_flag("reviewd").unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("not a built-in preset") && msg.contains("reviewed") && msg.contains("research") && msg.contains("team"),
        "unexpected error: {msg}"
    );
}

#[test]
fn resolve_roles_flag_surfaces_underlying_read_error_for_path_shaped_value() {
    let err = resolve_roles_flag("./nope/roles.json").unwrap_err();
    let msg = err.to_string();
    // Should mention a file/IO error (ENOENT equivalent), not "not a built-in preset"
    assert!(
        !msg.contains("not a built-in preset"),
        "should not mention presets for path-shaped value, got: {msg}"
    );
}

#[test]
fn resolve_roles_flag_reports_real_parse_error_not_preset_list_when_file_exists() {
    let root = make_temp_root("drip-roles-test-");
    let roles_path = root.join("extensionless-roles");
    // Malformed JSON (trailing comma)
    fs::write(&roles_path, r#"{"roles": [{"name": "scout"},]}"#).unwrap();

    let err = resolve_roles_flag(roles_path.to_str().unwrap()).unwrap_err();
    let msg = err.to_string();
    assert!(
        !msg.contains("not a built-in preset"),
        "should report parse error, not preset list; got: {msg}"
    );
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

#[test]
fn resolve_role_setup_reports_and_disarms_dangling_references() {
    let cwd = make_temp_root("drip-roles-test-");
    let mut config = default_config();
    config.settings.insert(
        ROLE_PROFILES_SETTING_ID.to_string(),
        r#"[{"name":"worker","model":"no-such-profile","skills":["missing-skill"],"tools":["READ","NO_SUCH_TOOL"],"verifiedBy":"ghost"}]"#.to_string(),
    );
    config.settings.insert(
        ROLE_BINDINGS_SETTING_ID.to_string(),
        r#"{"task":"nobody"}"#.to_string(),
    );

    let setup = resolve_role_setup(&ResolveRoleSetupArgs {
        config: &config,
        cwd: cwd.to_str().unwrap().to_string(),
        env: None,
        extra_roles: None,
        extra_bindings: None,
        marketplace_roles: None,
        skills: vec![],
        tool_names: vec!["READ".to_string()],
        mcp_server_names: vec![],
    });

    let worker = setup.roles.iter().find(|r| r.name == "worker").unwrap();
    // ghost is not a defined role → verifiedBy cleared
    assert!(worker.verified_by.is_none());
    // NO_SUCH_TOOL filtered out; READ kept
    assert_eq!(worker.tool_names.as_deref(), Some(&["READ".to_string()][..]));
    // Model could not resolve → route is None
    assert!(worker.route.is_none());
    // nobody binding → cleared
    assert!(setup.bindings.is_none());
    // Issues mention the problems
    assert!(setup.issues.iter().any(|i| i.contains("unknown skill") && i.contains("missing-skill")));
}

#[test]
fn resolve_role_setup_bindings_precedence_extra_bindings_over_config() {
    let root = make_temp_root("drip-roles-bindings-");
    let mut config = default_config();
    config.settings.insert(
        ROLE_PROFILES_SETTING_ID.to_string(),
        r#"[{"name":"architect"},{"name":"author"}]"#.to_string(),
    );
    config.settings.insert(
        ROLE_BINDINGS_SETTING_ID.to_string(),
        r#"{"planning":"author"}"#.to_string(),
    );

    use drip::harness::roles::HarnessRoleBindings;
    let setup = resolve_role_setup(&ResolveRoleSetupArgs {
        config: &config,
        cwd: root.to_str().unwrap().to_string(),
        env: None,
        extra_roles: None,
        extra_bindings: Some(HarnessRoleBindings {
            planning: Some("architect".to_string()),
            replanning: None,
            task: None,
        }),
        marketplace_roles: None,
        skills: vec![],
        tool_names: vec![],
        mcp_server_names: vec![],
    });

    let bindings = setup.bindings.as_ref().unwrap();
    assert_eq!(bindings.planning.as_deref(), Some("architect"));
}

#[test]
fn resolve_role_setup_project_roles_json_override_config_roles_and_bindings() {
    let cwd = make_temp_root("drip-roles-test-");
    let mut config = default_config();
    config.settings.insert(
        ROLE_PROFILES_SETTING_ID.to_string(),
        r#"[{"name":"reviewer","prompt":"Config reviewer."}]"#.to_string(),
    );
    config.settings.insert(
        ROLE_BINDINGS_SETTING_ID.to_string(),
        r#"{"planning":"planner","task":"worker"}"#.to_string(),
    );

    // Write .drip/roles.json in cwd
    let drip_dir = cwd.join(".drip");
    fs::create_dir_all(&drip_dir).unwrap();
    fs::write(
        drip_dir.join("roles.json"),
        r#"{"bindings":{"task":"reviewer"},"roles":[{"name":"reviewer","prompt":"Project reviewer.","tools":["READ"]},{"name":"planner","prompt":"Plan only.","tools":[]}]}"#,
    )
    .unwrap();

    let marketplace_roles = vec![
        MarketplaceRoleEntry {
            description: None,
            key: "acme/kit/reviewer".to_string(),
            marketplace_name: "acme".to_string(),
            name: "reviewer".to_string(),
            plugin_name: "kit".to_string(),
            prompt: "Marketplace reviewer.".to_string(),
            tools: None,
        },
        MarketplaceRoleEntry {
            description: None,
            key: "acme/kit/scout".to_string(),
            marketplace_name: "acme".to_string(),
            name: "scout".to_string(),
            plugin_name: "kit".to_string(),
            prompt: "Scout the repo.".to_string(),
            tools: Some(vec!["DIR".to_string()]),
        },
    ];

    let setup = resolve_role_setup(&ResolveRoleSetupArgs {
        config: &config,
        cwd: cwd.to_str().unwrap().to_string(),
        env: None,
        extra_roles: None,
        extra_bindings: None,
        marketplace_roles: Some(marketplace_roles),
        skills: vec![],
        tool_names: vec!["READ".to_string(), "DIR".to_string()],
        mcp_server_names: vec![],
    });

    let reviewer = setup.roles.iter().find(|r| r.name == "reviewer").unwrap();
    let planner = setup.roles.iter().find(|r| r.name == "planner").unwrap();
    let scout = setup.roles.iter().find(|r| r.name == "scout").unwrap();

    // Project wins over config, which wins over marketplace
    assert_eq!(
        reviewer.system_prompt_suffix.as_deref(),
        Some("Project reviewer.")
    );
    assert_eq!(reviewer.tool_names.as_deref(), Some(&["READ".to_string()][..]));
    // Empty tools array = harness ops only
    assert_eq!(planner.tool_names.as_deref(), Some(&[][..]));
    // Marketplace-only roles still land
    assert_eq!(
        scout.system_prompt_suffix.as_deref(),
        Some("Scout the repo.")
    );
    // Project bindings override config bindings per key
    // config had planning=planner,task=worker; project has task=reviewer
    // merged: planning=planner (config), task=reviewer (project overrides)
    let bindings = setup.bindings.as_ref().unwrap();
    assert_eq!(bindings.planning.as_deref(), Some("planner"));
    assert_eq!(bindings.task.as_deref(), Some("reviewer"));
}

#[test]
fn resolve_role_setup_bad_json_in_settings_produces_issues() {
    let root = make_temp_root("drip-roles-test-");
    let mut config = default_config();
    config.settings.insert(
        ROLE_PROFILES_SETTING_ID.to_string(),
        "not valid json".to_string(),
    );
    config.settings.insert(
        ROLE_BINDINGS_SETTING_ID.to_string(),
        "also not json".to_string(),
    );

    let setup = resolve_role_setup(&ResolveRoleSetupArgs {
        config: &config,
        cwd: root.to_str().unwrap().to_string(),
        env: None,
        extra_roles: None,
        extra_bindings: None,
        marketplace_roles: None,
        skills: vec![],
        tool_names: vec![],
        mcp_server_names: vec![],
    });

    assert!(setup.roles.is_empty());
    assert!(setup.issues.iter().any(|i| i.contains("role_profiles")));
    assert!(setup.issues.iter().any(|i| i.contains("role_bindings")));
}

// ---------------------------------------------------------------------------
// --roles CLI flag parsing, exercised through the arg parser
// ---------------------------------------------------------------------------

#[test]
fn roles_flag_parses_preset_name() {
    use drip::cli::args::parse_cli_args;
    let parsed = parse_cli_args(&["--roles".to_string(), "reviewed".to_string(), "goal".to_string()]);
    assert!(parsed.errors.is_empty());
    assert_eq!(parsed.roles_preset_or_path.as_deref(), Some("reviewed"));
}

#[test]
fn roles_flag_parses_file_path() {
    use drip::cli::args::parse_cli_args;
    let parsed = parse_cli_args(&[
        "--roles".to_string(),
        "/some/path/roles.json".to_string(),
        "goal".to_string(),
    ]);
    assert!(parsed.errors.is_empty());
    assert_eq!(
        parsed.roles_preset_or_path.as_deref(),
        Some("/some/path/roles.json")
    );
}

#[test]
fn roles_flag_errors_when_given_no_value() {
    use drip::cli::args::parse_cli_args;
    let parsed = parse_cli_args(&["--roles".to_string()]);
    assert!(!parsed.errors.is_empty());
    assert!(parsed.errors.iter().any(|e| e.contains("--roles")));
}

#[test]
fn roles_flag_errors_when_followed_by_another_flag() {
    use drip::cli::args::parse_cli_args;
    let parsed = parse_cli_args(&["--roles".to_string(), "--json".to_string()]);
    assert!(!parsed.errors.is_empty());
}

#[test]
fn roles_preset_or_path_is_none_when_not_passed() {
    use drip::cli::args::parse_cli_args;
    let parsed = parse_cli_args(&["goal text".to_string()]);
    assert!(parsed.roles_preset_or_path.is_none());
}
#[test]
fn resolve_role_setup_extra_roles_override_config_roles() {
    let root = make_temp_root("drip-roles-extra-roles-");
    let mut config = default_config();
    config.settings.insert(
        ROLE_PROFILES_SETTING_ID.to_string(),
        r#"[{"name":"reviewer","prompt":"Config reviewer."},{"name":"planner","prompt":"Config planner."}]"#.to_string(),
    );

    use drip::cli::roles::RoleDefinition;
    let extra_roles = vec![RoleDefinition {
        blind: false,
        mcp_servers: None,
        description: None,
        r#loop: None,
        model: None,
        name: "reviewer".to_string(),
        prompt: Some("Flag reviewer.".to_string()),
        skills: None,
        tools: Some(vec!["READ".to_string()]),
        verified_by: None,
    }];

    let setup = resolve_role_setup(&ResolveRoleSetupArgs {
        config: &config,
        cwd: root.to_str().unwrap().to_string(),
        env: None,
        extra_roles: Some(extra_roles),
        extra_bindings: None,
        marketplace_roles: None,
        skills: vec![],
        tool_names: vec!["READ".to_string(), "DIR".to_string()],
        mcp_server_names: vec![],
    });

    // Flag roles replace same-named config roles wholesale
    let reviewer = setup.roles.iter().find(|r| r.name == "reviewer").unwrap();
    assert_eq!(
        reviewer.system_prompt_suffix.as_deref(),
        Some("Flag reviewer.")
    );
    assert_eq!(reviewer.tool_names.as_deref(), Some(&["READ".to_string()][..]));
    // Config roles the flag does not mention still resolve
    let planner = setup.roles.iter().find(|r| r.name == "planner").unwrap();
    assert_eq!(
        planner.system_prompt_suffix.as_deref(),
        Some("Config planner.")
    );
}

#[test]
fn resolve_role_setup_extra_bindings_override_config_bindings() {
    let root = make_temp_root("drip-roles-extra-bindings-");
    let mut config = default_config();
    config.settings.insert(
        ROLE_PROFILES_SETTING_ID.to_string(),
        r#"[{"name":"architect"},{"name":"author"},{"name":"planner"}]"#.to_string(),
    );
    config.settings.insert(
        ROLE_BINDINGS_SETTING_ID.to_string(),
        r#"{"planning":"planner","task":"author"}"#.to_string(),
    );

    use drip::harness::roles::HarnessRoleBindings;
    let setup = resolve_role_setup(&ResolveRoleSetupArgs {
        config: &config,
        cwd: root.to_str().unwrap().to_string(),
        env: None,
        extra_roles: None,
        extra_bindings: Some(HarnessRoleBindings {
            planning: Some("architect".to_string()),
            replanning: None,
            task: None,
        }),
        marketplace_roles: None,
        skills: vec![],
        tool_names: vec![],
        mcp_server_names: vec![],
    });

    // Extra bindings win per key; keys they leave unset fall back to config
    let bindings = setup.bindings.as_ref().unwrap();
    assert_eq!(bindings.planning.as_deref(), Some("architect"));
    assert_eq!(bindings.task.as_deref(), Some("author"));
}


// ---------------------------------------------------------------------------
// Skill role hints: role-embedded composition + cross-loader parity
// ---------------------------------------------------------------------------

fn write_skill_file(root: &std::path::Path, dir_name: &str, content: &str) -> CliSkill {
    let skill_dir = root.join(dir_name);
    fs::create_dir_all(&skill_dir).unwrap();
    let path = skill_dir.join("SKILL.md");
    fs::write(&path, content).unwrap();
    CliSkill {
        description: "d".to_string(),
        key: None,
        name: dir_name.to_string(),
        path: path.to_string_lossy().into_owned(),
        source: SkillSource::Project,
    }
}

fn reviewer_setup(skill: CliSkill) -> drip::harness::roles::HarnessRoleRuntime {
    let cwd = make_temp_root("drip-role-hints-cwd-");
    let config = default_config();
    let setup = resolve_role_setup(&ResolveRoleSetupArgs {
        config: &config,
        cwd: cwd.to_str().unwrap().to_string(),
        env: None,
        extra_roles: Some(vec![RoleDefinition {
            blind: false,
        mcp_servers: None,
            description: None,
            r#loop: None,
            model: None,
            name: "reviewer".to_string(),
            prompt: None,
            skills: Some(vec![skill.name.clone()]),
            tools: None,
            verified_by: None,
        }]),
        extra_bindings: None,
        marketplace_roles: None,
        skills: vec![skill],
        tool_names: vec![],
        mcp_server_names: vec![],
    });
    assert!(setup.issues.is_empty(), "unexpected issues: {:?}", setup.issues);
    setup.roles.into_iter().find(|r| r.name == "reviewer").unwrap()
}

#[test]
fn role_embedded_skill_with_hints_gets_advisory_guidance_section() {
    let root = make_temp_root("drip-role-hints-");
    let skill = write_skill_file(
        &root,
        "hinted",
        "---\nname: hinted\ndescription: d\nroles:\n  default: author\n  review: reviewer\n  triage: planner\n---\n\nShip body.",
    );

    let reviewer = reviewer_setup(skill);
    let suffix = reviewer.system_prompt_suffix.as_deref().expect("suffix composed");
    // Role-embedded skills go through the shared composer: skill section AND
    // the advisory hints block the README promises.
    assert!(suffix.contains("# Skill: hinted"));
    assert!(suffix.contains("Skill role hints (advisory)"));
    assert!(suffix.contains("default role suggestion: author"));
    assert!(suffix.contains("role suggestion planner"));
}

#[test]
fn role_embedded_skill_without_hints_keeps_legacy_layout() {
    let root = make_temp_root("drip-role-legacy-");
    let skill = write_skill_file(
        &root,
        "plain-skill",
        "---\nname: plain-skill\ndescription: d\n---\n\nJust the body.",
    );

    let reviewer = reviewer_setup(skill);
    let suffix = reviewer.system_prompt_suffix.as_deref().expect("suffix composed");
    assert!(suffix.contains("# Skill: plain-skill"));
    assert!(suffix.contains("Just the body."));
    // No hints declared -> no advisory section, legacy prompt layout intact.
    assert!(!suffix.contains("role hints"));
}

#[test]
fn skill_role_hints_parity_between_cli_and_role_loaders() {
    // One file, both production loaders. BOM + CRLF + a blank line + a
    // tab-indented entry: every hard case that used to diverge.
    let root = make_temp_root("drip-role-parity-");
    let raw = "\u{FEFF}---\r\nname: parity\r\ndescription: d\r\nroles:\r\n  default: author\r\n\treview: reviewer\r\n\r\n  fixes: author\r\n---\r\n\r\nBody.";
    let skill = write_skill_file(&root, "parity", raw);

    let via_cli = load_cli_skill_content(&skill, None).unwrap();
    let via_roles = load_roles_skill_content(&skill, None).unwrap();
    assert_eq!(via_cli.role_hints, via_roles.role_hints, "loaders disagree");
    let hints = via_cli.role_hints.expect("hints present on both paths");
    assert_eq!(hints.default_role(), Some("author"));
    // Tab entry ended the block; the blank line would have too. Both loaders
    // must agree that no stage survived.
    assert!(hints.stages.is_empty());

    // Args-expanded path: same parity + hints carried alongside substitution.
    let skill_args = write_skill_file(
        &root,
        "parity-args",
        "---\nname: parity-args\ndescription: d\nargs:\n  from: vitest\nroles:\n  default: planner\n  review: reviewer\n---\n\nFrom {{from}}.",
    );
    let args: HashMap<String, String> = HashMap::new();
    let a = load_cli_skill_content(&skill_args, Some(&args)).unwrap();
    let b = load_roles_skill_content(&skill_args, Some(&args)).unwrap();
    assert_eq!(a.role_hints, b.role_hints, "args-path loaders disagree");
    assert_eq!(a.role_hints.unwrap().default_role(), Some("planner"));
    assert!(a.content.contains("vitest"));
}
