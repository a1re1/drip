// Schema-parity test: the Rust definition() output for each built-in
// read-only tool must byte-match (as serde_json::Value) the OpenAI function
// definition the TS oracle emits via drip/parity/tools/dump-tools.ts, whose
// output is committed at tests/fixtures/tools.json.
//
// The lci→drip rename rule is applied to the fixture's description text before
// comparison. As of this port NO description string in tools/read-tool.ts,
// tools/grep-tool.ts or tools/dir-tool.ts contains "lci"/"LCI" (the only
// .lci occurrence is a code comment in dir-tool.ts, which is not part of the
// schema), so the rename is a no-op today — the normalization is kept so the
// test keeps working if a future description ever mentions the product name.

use drip::tools::builtin::{dir, grep, read};

const RENAMES: &[(&str, &str)] = &[
    ("lciw", "dripw"),
    ("LCI_", "DRIP_"),
    ("LCI", "DRIP"),
    ("lci", "drip"),
];

/// Apply the user-visible rename rule to a JSON value's description strings
/// (and any other string field, harmlessly — names/paths never contain lci).
fn apply_renames(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::String(text) => {
            let mut text = text.clone();
            for (from, to) in RENAMES {
                text = text.replace(from, to);
            }
            serde_json::Value::String(text)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(apply_renames).collect())
        }
        serde_json::Value::Object(map) => {
            serde_json::Value::Object(map.iter().map(|(k, v)| (k.clone(), apply_renames(v))).collect())
        }
        other => other.clone(),
    }
}

fn load_fixture() -> Vec<serde_json::Value> {
    let raw = include_str!("fixtures/tools.json");
    serde_json::from_str(raw).expect("tests/fixtures/tools.json must be a JSON array")
}

fn fixture_entry(name: &str) -> serde_json::Value {
    load_fixture()
        .into_iter()
        .find(|entry| entry["function"]["name"] == name)
        .unwrap_or_else(|| panic!("tools.json is missing a {name} entry"))
}

#[test]
fn read_definition_matches_the_ts_oracle() {
    assert_eq!(
        read::definition(),
        apply_renames(&fixture_entry("READ")),
        "READ definition drifted from tools/read-tool.ts"
    );
}

#[test]
fn grep_definition_matches_the_ts_oracle() {
    assert_eq!(
        grep::definition(),
        apply_renames(&fixture_entry("GREP")),
        "GREP definition drifted from tools/grep-tool.ts"
    );
}

#[test]
fn dir_definition_matches_the_ts_oracle() {
    assert_eq!(
        dir::definition(),
        apply_renames(&fixture_entry("DIR")),
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
fn bash_definition_matches_the_ts_oracle() {
    assert_eq!(
        drip::tools::builtin::bash::definition(),
        apply_renames(&fixture_entry("BASH")),
        "BASH definition drifted from tools/bash-tool.ts"
    );
}

#[test]
fn fetch_definition_matches_the_ts_oracle() {
    assert_eq!(
        drip::tools::builtin::fetch::definition(),
        apply_renames(&fixture_entry("FETCH")),
        "FETCH definition drifted from tools/fetch-tool.ts"
    );
}

#[test]
fn check_definition_matches_the_ts_oracle() {
    assert_eq!(
        drip::tools::builtin::check::definition(),
        apply_renames(&fixture_entry("CHECK")),
        "CHECK definition drifted from tools/check-tool.ts"
    );
}
