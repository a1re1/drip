// Module-level marketplace tests. Temp roots are tempfile::tempdir with a
// "drip-" prefix (TempDir owns/removes the dir); load/save return Result, so
// tests unwrap. CLI-dispatch cases live with the entry-point tests.
use super::*;
use crate::core::home::open_drip_home;
use std::fs;

fn make_temp_root(prefix: &str) -> tempfile::TempDir {
    let _ = prefix;
    tempfile::tempdir().expect("tempdir")
}

fn write_skill(repo: &Path, name: &str, description: &str) {
    let dir = repo.join("skills").join(name);
    fs::create_dir_all(&dir).expect("mkdir");
    fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\n\nBody.\n"),
    )
    .expect("write");
}

const REVIEWER_PROMPT: &str = "Never trust a summary. Read the diff yourself.";

fn write_reviewer_agent(repo: &Path) {
    let dir = repo.join("agents");
    fs::create_dir_all(&dir).expect("mkdir");
    fs::write(
        dir.join("reviewer.md"),
        format!(
            "---\nname: reviewer\ndescription: Skeptical review subagent\ntools: READ, DIR\n---\n\n{REVIEWER_PROMPT}\n"
        ),
    )
    .expect("write");
}

fn write_manifest(repo: &Path, json: &str) {
    fs::create_dir_all(repo.join(".claude-plugin")).expect("mkdir");
    fs::write(repo.join(".claude-plugin").join("marketplace.json"), json).expect("write");
}

#[test]
fn parses_a_manifest_repo_into_plugins_with_skills_and_agents() {
    let tmp = make_temp_root("drip-marketplace-test-");
    let repo = tmp.path();

    write_manifest(
        repo,
        r#"{
  "plugins": [
    { "name": "reviewer-kit", "description": "Code review skills.", "source": "./" },
    { "name": "builder-kit", "source": "./plugins/builder" },
    { "name": "remote-kit", "source": "https://example.com/x/y.git" },
    { "name": "escape-kit", "source": "../escape" }
  ]
}"#,
    );
    write_skill(repo, "strict-review", "Review a diff strictly.");
    write_skill(repo, "test-audit", "Audit test coverage.");
    write_skill(&repo.join("plugins").join("builder"), "scaffold", "Scaffold a module.");
    write_reviewer_agent(repo);

    let parsed = parse_marketplace_repo(repo, "acme");

    let plugin_keys: Vec<&str> = parsed.plugins.iter().map(|plugin| plugin.key.as_str()).collect();

    assert_eq!(plugin_keys, vec!["acme/reviewer-kit", "acme/builder-kit"]);

    let reviewer_kit = &parsed.plugins[0];

    assert_eq!(reviewer_kit.description, "Code review skills.");
    assert_eq!(
        reviewer_kit
            .skills
            .iter()
            .map(|skill| skill.key.as_str())
            .collect::<Vec<_>>(),
        vec!["acme/reviewer-kit/strict-review", "acme/reviewer-kit/test-audit"]
    );
    assert_eq!(reviewer_kit.roles.len(), 1);
    assert_eq!(
        reviewer_kit.roles[0].description.as_deref(),
        Some("Skeptical review subagent")
    );
    assert_eq!(reviewer_kit.roles[0].key, "acme/reviewer-kit/reviewer");
    assert_eq!(reviewer_kit.roles[0].name, "reviewer");
    assert_eq!(
        reviewer_kit.roles[0].tools.as_deref(),
        Some(&["READ".to_string(), "DIR".to_string()][..])
    );
    assert!(reviewer_kit.roles[0].prompt.contains("Never trust a summary"));

    // The remote-source plugin and the repo-escaping path are refused, loudly.
    assert!(parsed.issues.iter().any(|issue| issue.contains("remote-kit")));
    assert!(
        parsed
            .issues
            .iter()
            .any(|issue| issue.contains("escape-kit") && issue.contains("escapes"))
    );
}

#[test]
fn treats_a_bare_repo_with_a_skills_directory_as_a_single_plugin_named_after_the_marketplace() {
    let tmp = make_temp_root("drip-marketplace-test-");
    let repo = tmp.path();

    write_skill(repo, "quick-fix", "Fix small bugs fast.");

    let parsed = parse_marketplace_repo(repo, "toolbox");

    assert_eq!(parsed.issues, Vec::<String>::new());
    assert_eq!(parsed.plugins.len(), 1);
    assert_eq!(parsed.plugins[0].key, "toolbox/toolbox");
    assert_eq!(
        parsed.plugins[0]
            .skills
            .iter()
            .map(|skill| skill.key.as_str())
            .collect::<Vec<_>>(),
        vec!["toolbox/toolbox/quick-fix"]
    );
}

#[test]
fn reports_a_repo_with_neither_a_manifest_nor_skills() {
    let tmp = make_temp_root("drip-marketplace-test-");
    let repo = tmp.path();
    let parsed = parse_marketplace_repo(repo, "empty");

    assert_eq!(parsed.plugins, Vec::<super::MarketplacePlugin>::new());
    assert_eq!(parsed.issues.len(), 1);
}

#[test]
fn derives_a_marketplace_name_from_a_git_url() {
    assert_eq!(
        marketplace_name_from_source("https://example.com/acme/skills.git/"),
        "skills"
    );
    assert_eq!(
        marketplace_name_from_source("/tmp/repos/my kit/../my kit.git"),
        "my-kit"
    );
    assert_eq!(marketplace_name_from_source(""), "marketplace");
}

#[test]
fn loads_a_missing_registry_as_empty_and_saves_it_with_a_trailing_newline() {
    let home = make_temp_root("drip-registry-test-");
    let path = home.path().join("marketplaces.json");

    assert!(load_marketplaces_file(&path)
        .expect("load")
        .marketplaces
        .is_empty());

    super::save_marketplaces_file(
        &path,
        &super::MarketplacesFile {
            enabled: vec!["acme/kit".to_string()],
            ..Default::default()
        },
    )
    .expect("save");

    let raw = fs::read_to_string(&path).expect("read");

    assert!(raw.ends_with('\n'));
    assert_eq!(
        load_marketplaces_file(&path).expect("load").enabled,
        vec!["acme/kit"]
    );
}

#[test]
fn resolves_clone_dirs_under_the_home_marketplaces_dir() {
    let root = make_temp_root("drip-clone-dir-test-");
    let home = open_drip_home(root.path().to_string_lossy().as_ref());
    let dir = marketplace_clone_dir(&home, "acme");

    assert!(dir.starts_with(&home.marketplaces_dir));
    assert!(dir.ends_with("acme"));
}
