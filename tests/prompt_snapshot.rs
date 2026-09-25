// Prompt snapshot: the prompt builders in src/harness/prompt.rs must reproduce,
// byte for byte, the text committed for each fixture state in
// tests/fixtures/prompt.json.

use drip::core::types::{HarnessRunReason, HarnessState};
use drip::harness::prompt::{
    build_cycle_continuation_message, build_fallback_run_summary, build_iteration_messages,
    build_iteration_user_message, build_run_summary_messages, compose_harness_system_prompt,
    looks_like_question_goal, CycleContinuationArgs, HarnessLoopInfo, HarnessLoopRole,
    HarnessRunBudget, IterationMessagesArgs, IterationUserMessageArgs, RunSummaryMessagesArgs,
    DEFAULT_HARNESS_SYSTEM_PROMPT, RUN_SUMMARY_SYSTEM_PROMPT,
};
use serde_json::Value;

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/prompt.json")).expect("fixture parses")
}

fn state(fx: &Value, name: &str) -> HarnessState {
    serde_json::from_value(fx["states"][name].clone())
        .unwrap_or_else(|e| panic!("state {name}: {e}"))
}

fn expect_str(fx: &Value, path: &[&str]) -> String {
    let mut v = fx;
    for p in path {
        v = &v[*p];
    }
    v.as_str()
        .unwrap_or_else(|| panic!("fixture string at {path:?}"))
        .to_string()
}

fn expect_value(fx: &Value, path: &[&str]) -> Value {
    let mut v = fx;
    for p in path {
        v = &v[*p];
    }
    assert!(!v.is_null(), "fixture value at {path:?}");
    v.clone()
}

/// Opt-in regeneration: `DRIP_UPDATE_PROMPT_FIXTURE=1 cargo test --test prompt_snapshot
/// -- --include-ignored regenerate_fixture`. It rewrites the
/// `RUN_SUMMARY_SYSTEM_PROMPT` copy plus the run-summary message/fallback
/// entries (the text this feature changed) and leaves every other entry
/// byte-identical, so an accidental edit elsewhere still fails the snapshot.
#[test]
#[ignore = "writes tests/fixtures/prompt.json when DRIP_UPDATE_PROMPT_FIXTURE=1"]
fn regenerate_fixture() {
    if std::env::var("DRIP_UPDATE_PROMPT_FIXTURE").as_deref() != Ok("1") {
        panic!("set DRIP_UPDATE_PROMPT_FIXTURE=1 to regenerate the fixture");
    }
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/prompt.json");
    let raw = std::fs::read_to_string(&path).expect("fixture reads");
    let mut fx: Value = serde_json::from_str(&raw).expect("fixture parses");
    regenerate_run_summary_entries(&mut fx);
    let text = serde_json::to_string_pretty(&fx).expect("fixture serializes") + "\n";
    std::fs::write(&path, text).expect("fixture writes");
}

/// Rewrites the run-summary fixture entries in place to the current builders'.
fn regenerate_run_summary_entries(fx: &mut Value) {
    // The run-summary system prompt is part of this change; every other
    // top-level constant must still match its frozen copy, so a leak of the
    // regeneration into unrelated entries fails loudly instead of silently
    // re-blessing it.
    assert_eq!(
        fx["DEFAULT_HARNESS_SYSTEM_PROMPT"],
        Value::String(DEFAULT_HARNESS_SYSTEM_PROMPT.to_string()),
        "regeneration must not touch the crate-level harness prompt"
    );
    fx["RUN_SUMMARY_SYSTEM_PROMPT"] = Value::String(RUN_SUMMARY_SYSTEM_PROMPT.to_string());
    let empty: HarnessState =
        serde_json::from_value(fx["states"]["empty"].clone()).expect("empty state");
    let rich: HarnessState =
        serde_json::from_value(fx["states"]["rich"].clone()).expect("rich state");
    for (name, reason) in reasons() {
        let workspace_changes = match name {
            "completed" => Some(" M src/a.rs\n?? new.rs".to_string()),
            _ => None,
        };
        let tool_usage = match name {
            "completed" => Some(
                [("DELEGATE", 1u64), ("PATCH", 3), ("READ", 4), ("VERIFY", 0)]
                    .into_iter()
                    .map(|(name, count)| (name.to_string(), count))
                    .collect(),
            ),
            "partial" => Some(std::collections::BTreeMap::new()),
            _ => None,
        };
        let got = build_run_summary_messages(
            &rich,
            &RunSummaryMessagesArgs {
                current_date: DATE,
                reason,
                tool_usage,
                workspace_changes,
                summary_preferences: None,
            },
        );
        fx["buildRunSummaryMessages"][name] = serde_json::to_value(&got).unwrap();
        fx["buildFallbackRunSummary"][format!("rich:{name}")] =
            Value::String(build_fallback_run_summary(&rich, reason));
        fx["buildFallbackRunSummary"][format!("empty:{name}")] =
            Value::String(build_fallback_run_summary(&empty, reason));
    }
    let got = build_run_summary_messages(
        &empty,
        &RunSummaryMessagesArgs {
            current_date: DATE,
            reason: HarnessRunReason::Completed,
            tool_usage: None,
            workspace_changes: None,
            summary_preferences: None,
        },
    );
    fx["buildRunSummaryMessages_empty"] = serde_json::to_value(&got).unwrap();
}

fn check(name: &str, actual: &str, expected: &str) {
    if actual != expected {
        let a: Vec<&str> = actual.lines().collect();
        let e: Vec<&str> = expected.lines().collect();
        let first = a
            .iter()
            .zip(e.iter())
            .position(|(x, y)| x != y)
            .unwrap_or(a.len().min(e.len()));
        panic!(
            "{name}: mismatch at line {} (actual {} lines, expected {} lines)\n  actual:   {:?}\n  expected: {:?}",
            first + 1,
            a.len(),
            e.len(),
            a.get(first),
            e.get(first)
        );
    }
}

fn reasons() -> Vec<(&'static str, HarnessRunReason)> {
    vec![
        ("aborted", HarnessRunReason::Aborted),
        ("completed", HarnessRunReason::Completed),
        ("error", HarnessRunReason::Error),
        ("futile", HarnessRunReason::Futile),
        ("max-iterations", HarnessRunReason::MaxIterations),
        ("partial", HarnessRunReason::Partial),
        ("planned", HarnessRunReason::Planned),
    ]
}

const DATE: &str = "2026-09-01";

#[test]
fn system_prompt_constants_match() {
    let fx = fixture();
    check(
        "DEFAULT_HARNESS_SYSTEM_PROMPT",
        DEFAULT_HARNESS_SYSTEM_PROMPT,
        &expect_str(&fx, &["DEFAULT_HARNESS_SYSTEM_PROMPT"]),
    );
    check(
        "RUN_SUMMARY_SYSTEM_PROMPT",
        RUN_SUMMARY_SYSTEM_PROMPT,
        &expect_str(&fx, &["RUN_SUMMARY_SYSTEM_PROMPT"]),
    );
    check(
        "compose none",
        &compose_harness_system_prompt(None),
        &expect_str(&fx, &["composeHarnessSystemPrompt", "none"]),
    );
    check(
        "compose empty",
        &compose_harness_system_prompt(Some("")),
        &expect_str(&fx, &["composeHarnessSystemPrompt", "empty"]),
    );
    check(
        "compose persona",
        &compose_harness_system_prompt(Some("You are a terse reviewer.\nNever guess.")),
        &expect_str(&fx, &["composeHarnessSystemPrompt", "persona"]),
    );
}

#[test]
fn looks_like_question_goal_matches() {
    let fx = fixture();
    for (goal, expected) in fx["looksLikeQuestionGoal"].as_object().unwrap() {
        assert_eq!(
            looks_like_question_goal(goal),
            expected.as_bool().unwrap(),
            "looksLikeQuestionGoal({goal:?})"
        );
    }
}

#[test]
fn iteration_user_message_matches() {
    let fx = fixture();
    let empty = state(&fx, "empty");
    let question = state(&fx, "question");
    let blocked = state(&fx, "blockedOnly");
    let rich = state(&fx, "rich");
    let current = rich.tasks[1].clone();
    let base = IterationUserMessageArgs {
        current_date: DATE,
        ..Default::default()
    };

    check(
        "empty_minimal",
        &build_iteration_user_message(&empty, &base),
        &expect_str(&fx, &["buildIterationUserMessage", "empty_minimal"]),
    );
    check(
        "question_minimal",
        &build_iteration_user_message(&question, &base),
        &expect_str(&fx, &["buildIterationUserMessage", "question_minimal"]),
    );
    check(
        "blockedOnly",
        &build_iteration_user_message(
            &blocked,
            &IterationUserMessageArgs {
                stall_limit: Some(3),
                ..base.clone()
            },
        ),
        &expect_str(&fx, &["buildIterationUserMessage", "blockedOnly"]),
    );
    let full = IterationUserMessageArgs {
        current_date: DATE,
        current_task: Some(&current),
        file_outlines: None,
        loop_info: Some(HarnessLoopInfo {
            index: 3,
            max_cycles: 4,
            role: Some(HarnessLoopRole {
                description: Some("ports code faithfully".into()),
                name: "porter".into(),
            }),
        }),
        repo_memory_dir: Some("/repo/.drip/memory"),
        repo_memory_index: Some("- parser.md: parser notes\n- tests.md: test notes"),
        run_budget: Some(HarnessRunBudget { total: 10, used: 7 }),
        stall_limit: Some(3),
        task_loop_limit: None,
        workspace: Some("/repo"),
        active_skills: None,
    };
    check(
        "rich_full",
        &build_iteration_user_message(&rich, &full),
        &expect_str(&fx, &["buildIterationUserMessage", "rich_full"]),
    );
    check(
        "rich_nearly_exhausted",
        &build_iteration_user_message(
            &rich,
            &IterationUserMessageArgs {
                current_task: Some(&current),
                run_budget: Some(HarnessRunBudget { total: 10, used: 9 }),
                ..base.clone()
            },
        ),
        &expect_str(&fx, &["buildIterationUserMessage", "rich_nearly_exhausted"]),
    );
    check(
        "rich_no_task_role_no_desc",
        &build_iteration_user_message(
            &rich,
            &IterationUserMessageArgs {
                loop_info: Some(HarnessLoopInfo {
                    index: 1,
                    max_cycles: 2,
                    role: Some(HarnessLoopRole {
                        description: None,
                        name: "planner".into(),
                    }),
                }),
                ..base.clone()
            },
        ),
        &expect_str(
            &fx,
            &["buildIterationUserMessage", "rich_no_task_role_no_desc"],
        ),
    );
}

#[test]
fn iteration_messages_match() {
    let fx = fixture();
    let empty = state(&fx, "empty");
    let rich = state(&fx, "rich");
    let current = rich.tasks[1].clone();

    let got = build_iteration_messages(
        &empty,
        &IterationMessagesArgs {
            current_date: DATE,
            system_prompt: "SYS",
            ..Default::default()
        },
    );
    assert_eq!(
        serde_json::to_value(&got).unwrap(),
        expect_value(&fx, &["buildIterationMessages", "empty"]),
        "buildIterationMessages empty"
    );

    let got = build_iteration_messages(
        &rich,
        &IterationMessagesArgs {
            current_date: DATE,
            current_task: Some(&current),
            file_outlines: None,
            goal_context: Some("context line one\ncontext line two"),
            goal_images: Some(vec![
                "data:image/png;base64,AAAA".into(),
                "data:image/png;base64,BBBB".into(),
            ]),
            loop_info: Some(HarnessLoopInfo {
                index: 3,
                max_cycles: 4,
                role: None,
            }),
            repo_memory_dir: Some("/repo/.drip/memory"),
            repo_memory_index: Some("- parser.md"),
            run_budget: Some(HarnessRunBudget { total: 10, used: 7 }),
            stall_limit: Some(3),
            task_loop_limit: None,
            system_prompt: "SYS",
            workspace: Some("/repo"),
            active_skills: None,
        },
    );
    assert_eq!(
        serde_json::to_value(&got).unwrap(),
        expect_value(&fx, &["buildIterationMessages", "rich_context_images"]),
        "buildIterationMessages rich_context_images"
    );
}

#[test]
fn cycle_continuation_message_matches() {
    let fx = fixture();
    let empty = state(&fx, "empty");
    let rich = state(&fx, "rich");
    let current = rich.tasks[1].clone();
    check(
        "first",
        &build_cycle_continuation_message(
            &rich,
            &CycleContinuationArgs {
                current_task: Some(&current),
                cycle: 2,
                max_cycles: 4,
                run_budget: None,
            },
        ),
        &expect_str(&fx, &["buildCycleContinuationMessage", "first"]),
    );
    check(
        "last_budget",
        &build_cycle_continuation_message(
            &rich,
            &CycleContinuationArgs {
                current_task: Some(&current),
                cycle: 4,
                max_cycles: 4,
                run_budget: Some(HarnessRunBudget { total: 10, used: 9 }),
            },
        ),
        &expect_str(&fx, &["buildCycleContinuationMessage", "last_budget"]),
    );
    check(
        "no_task",
        &build_cycle_continuation_message(
            &empty,
            &CycleContinuationArgs {
                current_task: None,
                cycle: 2,
                max_cycles: 3,
                run_budget: Some(HarnessRunBudget { total: 5, used: 2 }),
            },
        ),
        &expect_str(&fx, &["buildCycleContinuationMessage", "no_task"]),
    );
}

#[test]
fn run_summary_messages_match() {
    let fx = fixture();
    let empty = state(&fx, "empty");
    let rich = state(&fx, "rich");
    for (name, reason) in reasons() {
        let workspace_changes = match name {
            "completed" => Some(" M src/a.rs\n?? new.rs".to_string()),
            _ => None,
        };
        let tool_usage = match name {
            "completed" => Some(
                [("DELEGATE", 1u64), ("PATCH", 3), ("READ", 4), ("VERIFY", 0)]
                    .into_iter()
                    .map(|(name, count)| (name.to_string(), count))
                    .collect(),
            ),
            "partial" => Some(std::collections::BTreeMap::new()),
            _ => None,
        };
        let got = build_run_summary_messages(
            &rich,
            &RunSummaryMessagesArgs {
                current_date: DATE,
                reason,
                tool_usage,
                workspace_changes,
                summary_preferences: None,
            },
        );
        assert_eq!(
            serde_json::to_value(&got).unwrap(),
            expect_value(&fx, &["buildRunSummaryMessages", name]),
            "buildRunSummaryMessages {name}"
        );
    }
    let got = build_run_summary_messages(
        &empty,
        &RunSummaryMessagesArgs {
            current_date: DATE,
            reason: HarnessRunReason::Completed,
            tool_usage: None,
            workspace_changes: None,
            summary_preferences: None,
        },
    );
    assert_eq!(
        serde_json::to_value(&got).unwrap(),
        expect_value(&fx, &["buildRunSummaryMessages_empty"]),
        "buildRunSummaryMessages empty"
    );
}

#[test]
fn fallback_run_summary_matches() {
    let fx = fixture();
    let empty = state(&fx, "empty");
    let rich = state(&fx, "rich");
    for (name, reason) in reasons() {
        check(
            &format!("rich:{name}"),
            &build_fallback_run_summary(&rich, reason),
            &expect_str(&fx, &["buildFallbackRunSummary", &format!("rich:{name}")]),
        );
        check(
            &format!("empty:{name}"),
            &build_fallback_run_summary(&empty, reason),
            &expect_str(&fx, &["buildFallbackRunSummary", &format!("empty:{name}")]),
        );
    }
}

#[test]
fn an_edited_preferences_file_changes_the_summary_system_prompt() {
    let fx = fixture();
    let rich = state(&fx, "rich");
    let args = |summary_preferences| RunSummaryMessagesArgs {
        current_date: DATE,
        reason: HarnessRunReason::Completed,
        tool_usage: None,
        workspace_changes: None,
        summary_preferences,
    };
    let base = build_run_summary_messages(&rich, &args(None));
    let edited = build_run_summary_messages(&rich, &args(Some("Lead with the failing test name.")));

    let system_of =
        |messages: &[drip::harness::transport::TransportRequestMessage]| match &messages[0].content
        {
            Some(drip::harness::transport::TransportContent::Text(text)) => text.clone(),
            other => panic!("unexpected system content: {other:?}"),
        };
    let base_system = system_of(&base);
    let edited_system = system_of(&edited);

    // The built-in contract is unedited and still comes first; the operator's
    // preferences are what changed.
    assert_eq!(base_system, RUN_SUMMARY_SYSTEM_PROMPT);
    assert!(edited_system.starts_with(RUN_SUMMARY_SYSTEM_PROMPT));
    assert!(edited_system.ends_with("Lead with the failing test name."));

    // Everything else about the call is untouched.
    assert_eq!(base.len(), edited.len());
    assert_eq!(base[1].content, edited[1].content);
}

#[test]
fn harness_prompt_documents_inline_image_presentation() {
    let prompt = DEFAULT_HARNESS_SYSTEM_PROMPT;
    assert!(
        prompt.contains("![what it shows](/absolute/path/shot.png)"),
        "the harness prompt must tell the agent how to present a local image"
    );
    assert!(
        prompt.contains("screencapture"),
        "the harness prompt must name a way to take the screenshot it presents"
    );
}
