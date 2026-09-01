// Schema-parity test: the Rust harness_tool_definitions() output must
// byte-match (as serde_json::Value) the OpenAI function definitions the TS
// oracle emits via drip/parity/tools/dump-harness-tools.ts, whose output is
// committed at tests/fixtures/harness-tools.json.
//
// The lci→drip rename rule is applied to the fixture's description text before
// comparison. As of this port the only rename sites in the fixture are the
// `~/.lci/projects/<project>/memory` paths in the remember/forget descriptions.

use drip::harness::harness_tools::harness_tool_definitions;

const RENAMES: &[(&str, &str)] = &[
    ("lciw", "dripw"),
    ("LCI_", "DRIP_"),
    ("LCI", "DRIP"),
    ("lci", "drip"),
];

/// Apply the user-visible rename rule to a JSON value's string fields
/// (tool names and JSON schema keys never contain lci, so this is
/// description-scoped in practice).
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
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), apply_renames(v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

#[test]
fn harness_tool_definitions_match_the_ts_oracle() {
    let raw = include_str!("fixtures/harness-tools.json");
    let fixture: Vec<serde_json::Value> =
        serde_json::from_str(raw).expect("tests/fixtures/harness-tools.json must be a JSON array");

    let rust: Vec<serde_json::Value> = harness_tool_definitions();

    let fixture_renamed: Vec<serde_json::Value> =
        fixture.iter().map(apply_renames).collect();

    assert_eq!(
        rust, fixture_renamed,
        "harness tool definitions drifted from src/harness/harness-tools.ts"
    );
}

#[test]
fn fixture_covers_exactly_the_ten_harness_tools_sorted_by_name() {
    let raw = include_str!("fixtures/harness-tools.json");
    let fixture: Vec<serde_json::Value> =
        serde_json::from_str(raw).expect("tests/fixtures/harness-tools.json must be a JSON array");

    let names: Vec<&str> = fixture
        .iter()
        .map(|entry| entry["function"]["name"].as_str().expect("entry missing function.name"))
        .collect();

    assert_eq!(
        names,
        vec![
            "drop_task",
            "finish_task",
            "forget",
            "note_task",
            "observe",
            "plan_tasks",
            "recall",
            "remember",
            "respond",
            "revise_task",
        ]
    );
    assert_eq!(harness_tool_definitions().len(), names.len());
}
