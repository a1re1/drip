// Snapshot test: the built-in tool pack's OpenAI function definitions must
// match the JSON committed at tests/fixtures/tools.json.

use drip::tools::builtin::{dir, grep, read};

fn load_fixture() -> Vec<serde_json::Value> {
    let raw = include_str!("fixtures/tools.json");
    serde_json::from_str(raw).expect("tests/fixtures/tools.json must be a JSON array")
}

/// The wire shape drip sends (function{description,name,parameters}, type),
/// serialized so key ORDER inside `parameters` is compared too — the model
/// sees the JSON text, and serde_json Value equality would ignore order.
fn canonical(definition: &serde_json::Value) -> String {
    let function = &definition["function"];
    serde_json::to_string(&serde_json::json!({
        "function": {
            "description": function["description"],
            "name": function["name"],
            "parameters": function["parameters"]
        },
        "type": "function"
    }))
    .unwrap()
}

fn fixture_entry(name: &str) -> serde_json::Value {
    load_fixture()
        .into_iter()
        .find(|entry| entry["function"]["name"] == name)
        .unwrap_or_else(|| panic!("tools.json is missing a {name} entry"))
}

#[test]
fn read_definition_matches_the_fixture() {
    assert_eq!(
        canonical(&read::definition()),
        canonical(&fixture_entry("READ")),
        "READ definition drifted from tools/read-tool.ts"
    );
}

#[test]
fn grep_definition_matches_the_fixture() {
    assert_eq!(
        canonical(&grep::definition()),
        canonical(&fixture_entry("GREP")),
        "GREP definition drifted from tools/grep-tool.ts"
    );
}

#[test]
fn dir_definition_matches_the_fixture() {
    assert_eq!(
        canonical(&dir::definition()),
        canonical(&fixture_entry("DIR")),
        "DIR definition drifted from tools/dir-tool.ts"
    );
}

#[test]
fn fixture_covers_exactly_the_built_in_pack() {
    let mut names: Vec<String> = load_fixture()
        .into_iter()
        .map(|entry| entry["function"]["name"].as_str().unwrap().to_string())
        .collect();
    names.sort();
    assert_eq!(names, vec!["BASH", "BASH_ASYNC", "CHECK", "DIR", "FETCH", "GREP", "PATCH", "READ", "VERIFY"]);
}

#[test]
fn bash_definition_matches_the_fixture() {
    assert_eq!(
        canonical(&drip::tools::builtin::bash::definition()),
        canonical(&fixture_entry("BASH")),
        "BASH definition drifted from tools/bash-tool.ts"
    );
}

#[test]
fn fetch_definition_matches_the_fixture() {
    assert_eq!(
        canonical(&drip::tools::builtin::fetch::definition()),
        canonical(&fixture_entry("FETCH")),
        "FETCH definition drifted from tools/fetch-tool.ts"
    );
}

#[test]
fn check_definition_matches_the_fixture() {
    assert_eq!(
        canonical(&drip::tools::builtin::check::definition()),
        canonical(&fixture_entry("CHECK")),
        "CHECK definition drifted from tools/check-tool.ts"
    );
}

#[test]
fn verify_definition_matches_the_fixture() {
    assert_eq!(
        canonical(&drip::tools::builtin::verify::definition()),
        canonical(&fixture_entry("VERIFY")),
        "VERIFY definition drifted from tools/verify-tool.ts"
    );
}

#[test]
fn bash_async_definition_matches_the_fixture() {
    assert_eq!(
        canonical(&drip::tools::builtin::bash::async_definition()),
        canonical(&fixture_entry("BASH_ASYNC")),
        "BASH_ASYNC definition drifted from tools/bash-tool.ts"
    );
}

#[test]
fn patch_definition_matches_the_fixture() {
    assert_eq!(
        canonical(&drip::tools::builtin::patch::definition()),
        canonical(&fixture_entry("PATCH")),
        "PATCH definition drifted from tools/patch-tool.ts"
    );
}
