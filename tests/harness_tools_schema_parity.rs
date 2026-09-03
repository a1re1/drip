// Snapshot test: the harness tool definitions drip sends to the model must
// match the JSON committed at tests/fixtures/harness-tools.json.

use drip::harness::harness_tools::harness_tool_definitions;

#[test]
fn harness_tool_definitions_match_the_fixture() {
    let raw = include_str!("fixtures/harness-tools.json");
    let fixture: Vec<serde_json::Value> =
        serde_json::from_str(raw).expect("tests/fixtures/harness-tools.json must be a JSON array");

    let rust: Vec<serde_json::Value> = harness_tool_definitions();


    // Serialized, so key order inside each definition is compared too.
    assert_eq!(
        serde_json::to_string(&rust).unwrap(),
        serde_json::to_string(&fixture).unwrap(),
        "harness tool definitions drifted from src/harness/harness-tools.ts"
    );
}

#[test]
fn fixture_covers_exactly_the_ten_harness_tools_in_declaration_order() {
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
            "plan_tasks",
            "drop_task",
            "revise_task",
            "finish_task",
            "respond",
            "observe",
            "recall",
            "note_task",
            "remember",
            "forget",
        ]
    );
    assert_eq!(harness_tool_definitions().len(), names.len());
}
