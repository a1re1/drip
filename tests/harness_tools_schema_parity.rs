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
        "harness tool definitions drifted from tests/fixtures/harness-tools.json"
    );
}

#[test]
fn fixture_covers_exactly_the_twelve_harness_tools_in_declaration_order() {
    let raw = include_str!("fixtures/harness-tools.json");
    let fixture: Vec<serde_json::Value> =
        serde_json::from_str(raw).expect("tests/fixtures/harness-tools.json must be a JSON array");

    let names: Vec<&str> = fixture
        .iter()
        .map(|entry| {
            entry["function"]["name"]
                .as_str()
                .expect("entry missing function.name")
        })
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
            "ask_user",
            "report",
        ]
    );
    assert_eq!(harness_tool_definitions().len(), names.len());
}

/// Opt-in regeneration of the committed tool fixture:
/// `DRIP_UPDATE_HARNESS_TOOLS_FIXTURE=1 cargo test --test
/// harness_tools_schema_parity -- --include-ignored regenerate_fixture`.
#[test]
#[ignore = "writes tests/fixtures/harness-tools.json when DRIP_UPDATE_HARNESS_TOOLS_FIXTURE=1"]
fn regenerate_fixture() {
    if std::env::var("DRIP_UPDATE_HARNESS_TOOLS_FIXTURE").as_deref() != Ok("1") {
        panic!("set DRIP_UPDATE_HARNESS_TOOLS_FIXTURE=1 to regenerate the fixture");
    }
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/harness-tools.json");
    let text = serde_json::to_string_pretty(&harness_tool_definitions()).unwrap() + "\n";
    std::fs::write(&path, text).expect("fixture writes");
}
