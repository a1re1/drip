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
    serde_json::from_value(fx["states"][name].clone()).unwrap_or_else(|e| panic!("state {name}: {e}"))
}

fn expect_str(fx: &Value, path: &[&str]) -> String {
    let mut v = fx;
    for p in path {
        v = &v[*p];
    }
    v.as_str().unwrap_or_else(|| panic!("fixture string at {path:?}")).to_string()
}

fn expect_value(fx: &Value, path: &[&str]) -> Value {
    let mut v = fx;
    for p in path {
        v = &v[*p];
    }
    assert!(!v.is_null(), "fixture value at {path:?}");
    v.clone()
}

fn check(name: &str, actual: &str, expected: &str) {
    if actual != expected {
        let a: Vec<&str> = actual.lines().collect();
        let e: Vec<&str> = expected.lines().collect();
        let first = a.iter().zip(e.iter()).position(|(x, y)| x != y).unwrap_or(a.len().min(e.len()));
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
    check("DEFAULT_HARNESS_SYSTEM_PROMPT", DEFAULT_HARNESS_SYSTEM_PROMPT, &expect_str(&fx, &["DEFAULT_HARNESS_SYSTEM_PROMPT"]));
    check("RUN_SUMMARY_SYSTEM_PROMPT", RUN_SUMMARY_SYSTEM_PROMPT, &expect_str(&fx, &["RUN_SUMMARY_SYSTEM_PROMPT"]));
    check("compose none", &compose_harness_system_prompt(None), &expect_str(&fx, &["composeHarnessSystemPrompt", "none"]));
    check("compose empty", &compose_harness_system_prompt(Some("")), &expect_str(&fx, &["composeHarnessSystemPrompt", "empty"]));
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
        assert_eq!(looks_like_question_goal(goal), expected.as_bool().unwrap(), "looksLikeQuestionGoal({goal:?})");
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
    let base = IterationUserMessageArgs { current_date: DATE, ..Default::default() };

    check("empty_minimal", &build_iteration_user_message(&empty, &base), &expect_str(&fx, &["buildIterationUserMessage", "empty_minimal"]));
    check("question_minimal", &build_iteration_user_message(&question, &base), &expect_str(&fx, &["buildIterationUserMessage", "question_minimal"]));
    check(
        "blockedOnly",
        &build_iteration_user_message(&blocked, &IterationUserMessageArgs { stall_limit: Some(3), ..base.clone() }),
        &expect_str(&fx, &["buildIterationUserMessage", "blockedOnly"]),
    );
    let full = IterationUserMessageArgs {
        current_date: DATE,
        current_task: Some(&current),
        loop_info: Some(HarnessLoopInfo {
            index: 3,
            max_cycles: 4,
            role: Some(HarnessLoopRole { description: Some("ports code faithfully".into()), name: "porter".into() }),
        }),
        repo_memory_dir: Some("/repo/.drip/memory"),
        repo_memory_index: Some("- parser.md: parser notes\n- tests.md: test notes"),
        run_budget: Some(HarnessRunBudget { total: 10, used: 7 }),
        stall_limit: Some(3),
        workspace: Some("/repo"),
    };
    check("rich_full", &build_iteration_user_message(&rich, &full), &expect_str(&fx, &["buildIterationUserMessage", "rich_full"]));
    check(
        "rich_nearly_exhausted",
        &build_iteration_user_message(
            &rich,
            &IterationUserMessageArgs { current_task: Some(&current), run_budget: Some(HarnessRunBudget { total: 10, used: 9 }), ..base.clone() },
        ),
        &expect_str(&fx, &["buildIterationUserMessage", "rich_nearly_exhausted"]),
    );
    check(
        "rich_no_task_role_no_desc",
        &build_iteration_user_message(
            &rich,
            &IterationUserMessageArgs {
                loop_info: Some(HarnessLoopInfo { index: 1, max_cycles: 2, role: Some(HarnessLoopRole { description: None, name: "planner".into() }) }),
                ..base.clone()
            },
        ),
        &expect_str(&fx, &["buildIterationUserMessage", "rich_no_task_role_no_desc"]),
    );
}

#[test]
fn iteration_messages_match() {
    let fx = fixture();
    let empty = state(&fx, "empty");
    let rich = state(&fx, "rich");
    let current = rich.tasks[1].clone();

    let got = build_iteration_messages(&empty, &IterationMessagesArgs { current_date: DATE, system_prompt: "SYS", ..Default::default() });
    assert_eq!(serde_json::to_value(&got).unwrap(), expect_value(&fx, &["buildIterationMessages", "empty"]), "buildIterationMessages empty");

    let got = build_iteration_messages(
        &rich,
        &IterationMessagesArgs {
            current_date: DATE,
            current_task: Some(&current),
            goal_context: Some("context line one\ncontext line two"),
            goal_images: Some(vec!["data:image/png;base64,AAAA".into(), "data:image/png;base64,BBBB".into()]),
            loop_info: Some(HarnessLoopInfo { index: 3, max_cycles: 4, role: None }),
            repo_memory_dir: Some("/repo/.drip/memory"),
            repo_memory_index: Some("- parser.md"),
            run_budget: Some(HarnessRunBudget { total: 10, used: 7 }),
            stall_limit: Some(3),
            system_prompt: "SYS",
            workspace: Some("/repo"),
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
        &build_cycle_continuation_message(&rich, &CycleContinuationArgs { current_task: Some(&current), cycle: 2, max_cycles: 4, run_budget: None }),
        &expect_str(&fx, &["buildCycleContinuationMessage", "first"]),
    );
    check(
        "last_budget",
        &build_cycle_continuation_message(
            &rich,
            &CycleContinuationArgs { current_task: Some(&current), cycle: 4, max_cycles: 4, run_budget: Some(HarnessRunBudget { total: 10, used: 9 }) },
        ),
        &expect_str(&fx, &["buildCycleContinuationMessage", "last_budget"]),
    );
    check(
        "no_task",
        &build_cycle_continuation_message(&empty, &CycleContinuationArgs { current_task: None, cycle: 2, max_cycles: 3, run_budget: Some(HarnessRunBudget { total: 5, used: 2 }) }),
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
        let got = build_run_summary_messages(&rich, &RunSummaryMessagesArgs { current_date: DATE, reason, tool_usage, workspace_changes });
        assert_eq!(serde_json::to_value(&got).unwrap(), expect_value(&fx, &["buildRunSummaryMessages", name]), "buildRunSummaryMessages {name}");
    }
    let got = build_run_summary_messages(&empty, &RunSummaryMessagesArgs { current_date: DATE, reason: HarnessRunReason::Completed, tool_usage: None, workspace_changes: None });
    assert_eq!(serde_json::to_value(&got).unwrap(), expect_value(&fx, &["buildRunSummaryMessages_empty"]), "buildRunSummaryMessages empty");
}

#[test]
fn fallback_run_summary_matches() {
    let fx = fixture();
    let empty = state(&fx, "empty");
    let rich = state(&fx, "rich");
    for (name, reason) in reasons() {
        check(&format!("rich:{name}"), &build_fallback_run_summary(&rich, reason), &expect_str(&fx, &["buildFallbackRunSummary", &format!("rich:{name}")]));
        check(&format!("empty:{name}"), &build_fallback_run_summary(&empty, reason), &expect_str(&fx, &["buildFallbackRunSummary", &format!("empty:{name}")]));
    }
}
