use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::cli::run_record::RunRecord;
use crate::core::types::{HarnessEvent, HarnessEventType, HarnessLeakedJob, HarnessRunUsage, TaskStats, VerificationSummary};
use crate::harness::telemetry::draft_harden_goal;

// The headless run's stdout is a contract for orchestrating agents: with
// --json every line is one JSON object — a stream of {type:"event"} lines
// followed by exactly one {type:"result"} line — so a caller can follow
// progress and parse the outcome without scraping prose. The same payload is
// persisted per session (result.json) and replayed by --result / --wait.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeadlessContinuation {
    pub goal: String,
    pub resume_id_prefix: String,
    pub session_id: String,
    pub suggested_max_iterations: Option<i64>,
}

/// Serialized key order is part of the stdout/result.json contract.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeadlessResultPayload {
    /// Structured resume coordinates for callers that build their own command.
    pub continuation: Option<HeadlessContinuation>,
    /// Harness-recorded outcome of the goal's most recent verification command (tests/typecheck/build), or null. mutationsAfter > 0 = STALE.
    pub last_verification: Option<VerificationSummary>,
    /// Operator messages that arrived too late for this run — the session's next run consumes them.
    pub pending_operator_messages: i64,
    /// Final ledger tally — lets a caller distinguish "5/5 done" from "3/5, 2 blocked" without parsing prose.
    pub task_stats: TaskStats,
    /// Present when reason is "error": what killed the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// Ready-to-exec resume command (real goal text, shell-escaped) — null when the run completed.
    pub continue_command: Option<String>,
    pub exit_code: i64,
    /// The goal this run executed — so a captured result line is self-describing.
    pub goal: String,
    pub goal_id: String,
    pub iterations: i64,
    pub loops: i64,
    pub reason: String,
    pub result_path: String,
    pub session_id: String,
    pub state_path: String,
    /// Background jobs (tmux) still running at run end — the driver should stop or adopt them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leaked_jobs: Option<Vec<HarnessLeakedJob>>,
    /// Present when a stop was requested: ms from the abort signal to run end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_latency_ms: Option<i64>,
    pub summary: Option<String>,
    pub transcript_path: String,
    #[serde(rename = "type")]
    pub payload_type: String,
    /// Token/latency economics (calls, tokens, per-task attribution, retry waits, wall time); null on pre-0.31 records.
    pub usage: Option<HarnessRunUsage>,
    /// How the last completion was anchored (external check vs declared none) and the confidence the agent claimed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_anchor: Option<crate::core::types::CompletionAnchor>,
    /// Expectations the run could not reconcile (reason "unreconciled", exit 0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anomalies: Option<Vec<crate::core::types::HarnessAnomaly>>,
}

pub fn headless_event_line(event: &HarnessEvent, json: bool) -> Option<String> {
    if json {
        let mut line: Map<String, Value> = Map::new();
        line.insert(
            "at".to_string(),
            Value::String(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
        );
        if let Some(data) = &event.data {
            line.insert("data".to_string(), serde_json::to_value(data).unwrap_or(Value::Null));
        }
        line.insert("detail".to_string(), Value::String(event.detail.clone()));
        line.insert("iteration".to_string(), Value::from(event.iteration));
        line.insert("kind".to_string(), serde_json::to_value(event.r#type).unwrap_or(Value::Null));
        line.insert("type".to_string(), Value::String("event".to_string()));

        return Some(serde_json::to_string(&Value::Object(line)).unwrap_or_default());
    }

    // The run summary is delivered once, formatted, in the result block — not
    // as an event line too.
    if event.r#type == HarnessEventType::RunSummary {
        return None;
    }

    let kind = serde_json::to_value(event.r#type)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_default();

    Some(format!("[{:>3}] {} {}", event.iteration, kind, event.detail))
}

// POSIX single-quote escaping: the only character that needs handling inside
// single quotes is the single quote itself.
pub fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

pub struct HeadlessResultArgs<'a> {
    pub record: &'a RunRecord,
    pub result_path: &'a str,
    pub session_id: &'a str,
    pub session_id_prefix: &'a str,
    pub state_path: &'a str,
    pub transcript_path: &'a str,
}

/// Reasons whose work is complete: "unreconciled" finished every task but
/// left a pre-registered expectation it could not reconcile — a visible
/// anomaly, not a failure, so it exits 0 like "completed". "draft" is a
/// --lite run's finished draft: exit 0 too, but it keeps its harden continue
/// command (see headless_result_payload).
pub fn reason_is_complete(reason: &str) -> bool {
    reason == "completed" || reason == "unreconciled" || reason == "draft"
}

pub fn headless_result_payload(args: HeadlessResultArgs<'_>) -> HeadlessResultPayload {
    let record = args.record;
    let completed = reason_is_complete(&record.reason);
    // Resuming with the budget the caller already chose is the best default; a
    // run that never had a cap resumes uncapped too.
    let suggested_max_iterations = record.max_iterations;
    let draft = record.reason == "draft";
    let continue_command = if completed && !draft {
        None
    } else if let Some(command) = record.continue_command.clone() {
        // The run itself proposed a continuation (e.g. awaiting-input's
        // "drip --resume <id>") — the record is the source of truth.
        Some(command)
    } else {
        Some(format!(
            "drip --resume {} --prompt {}{} --json",
            args.session_id_prefix,
            shell_quote(&record.goal),
            // A cap of 0 prints nothing, same as no cap at all.
            match suggested_max_iterations {
                Some(cap) if cap != 0 => format!(" --max-iterations {cap}"),
                _ => String::new(),
            }
        ))
    };

    HeadlessResultPayload {
        continuation: if completed && !draft {
            None
        } else {
            Some(HeadlessContinuation {
                // Draft runs hand the operator the harden goal, not the
                // original one — the resume command above already carries it.
                goal: if draft {
                    draft_harden_goal(&record.goal)
                } else {
                    record.goal.clone()
                },
                resume_id_prefix: args.session_id_prefix.to_string(),
                session_id: args.session_id.to_string(),
                suggested_max_iterations,
            })
        },
        last_verification: record.last_verification.clone(),
        pending_operator_messages: record.pending_operator_messages,
        task_stats: record.task_stats,
        error_message: record.error_message.clone().filter(|message| !message.is_empty()),
        continue_command,
        exit_code: if completed {
            0
        } else if record.reason == "error" {
            3
        } else {
            2
        },
        goal: record.goal.clone(),
        goal_id: record.goal_id.clone(),
        iterations: record.iterations,
        loops: record.loops,
        reason: record.reason.clone(),
        result_path: args.result_path.to_string(),
        session_id: args.session_id.to_string(),
        state_path: args.state_path.to_string(),
        leaked_jobs: record.leaked_jobs.clone().filter(|jobs| !jobs.is_empty()),
        stop_latency_ms: record.stop_latency_ms,
        summary: record.summary.clone(),
        transcript_path: args.transcript_path.to_string(),
        payload_type: "result".to_string(),
        usage: record.usage.clone(),
        completion_anchor: record.completion_anchor.clone(),
        anomalies: record.anomalies.clone().filter(|anomalies| !anomalies.is_empty()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(reason: &str, max_iterations: Option<i64>) -> RunRecord {
        RunRecord {
            continue_command: None,
            ended_at: "2026-01-01T00:00:00.000Z".to_string(),
            error_message: None,
            goal: "do it".to_string(),
            goal_id: "g1".to_string(),
            iterations: 3,
            leaked_jobs: None,
            last_verification: None,
            loops: 2,
            max_iterations,
            pending_operator_messages: 0,
            reason: reason.to_string(),
            stop_latency_ms: None,
            summary: Some("done".to_string()),
            usage: None,
            task_stats: TaskStats::default(),
            completion_anchor: None,
            anomalies: None,
        }
    }

    fn payload(reason: &str, max_iterations: Option<i64>) -> HeadlessResultPayload {
        let record = record(reason, max_iterations);
        headless_result_payload(HeadlessResultArgs {
            record: &record,
            result_path: "/r",
            session_id: "abcdefgh-1234",
            session_id_prefix: "abcdefgh",
            state_path: "/s",
            transcript_path: "/t",
        })
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn draft_run_exits_zero_and_prints_the_harden_continue_command() {
        let mut draft = record("draft", Some(5));
        draft.continue_command = Some(crate::harness::telemetry::draft_continue_command(
            "abcdefgh-1234",
            &draft.goal,
        ));
        let payload = headless_result_payload(HeadlessResultArgs {
            record: &draft,
            result_path: "/r",
            session_id: "abcdefgh-1234",
            session_id_prefix: "abcdefgh",
            state_path: "/s",
            transcript_path: "/t",
        });
        assert_eq!(payload.exit_code, 0);
        assert_eq!(
            payload.continue_command.as_deref(),
            Some(
                "drip --resume abcdefgh-1234 --roles reviewed --skill verify-before-done --new-goal 'Harden the draft: do it'"
            )
        );
        let continuation = payload.continuation.expect("draft keeps a continuation");
        assert_eq!(continuation.goal, "Harden the draft: do it");
        assert_eq!(continuation.session_id, "abcdefgh-1234");
    }

    #[test]
    fn draft_harden_goal_truncates_to_two_hundred_chars() {
        let long = "x".repeat(250);
        let harden = draft_harden_goal(&long);
        assert!(harden.starts_with("Harden the draft: "));
        assert_eq!(harden.chars().count(), 18 + 200);
    }

    #[test]
    fn event_line_pads_iteration_and_skips_run_summary() {
        let event = HarnessEvent {
            data: None,
            detail: "hello".to_string(),
            iteration: 7,
            r#type: HarnessEventType::ModelText,
        };
        assert_eq!(headless_event_line(&event, false).as_deref(), Some("[  7] model-text hello"));
        let summary = HarnessEvent { r#type: HarnessEventType::RunSummary, ..event.clone() };
        assert_eq!(headless_event_line(&summary, false), None);
        let json = headless_event_line(&summary, true).unwrap();
        assert!(json.starts_with("{\"at\":\""), "{json}");
        assert!(json.ends_with("\"detail\":\"hello\",\"iteration\":7,\"kind\":\"run-summary\",\"type\":\"event\"}"), "{json}");
    }

    #[test]
    fn exit_codes_and_continue_command() {
        let completed = payload("completed", Some(5));
        assert_eq!(completed.exit_code, 0);
        assert!(completed.continue_command.is_none());
        assert!(completed.continuation.is_none());

        let budget = payload("max-iterations", Some(5));
        assert_eq!(budget.exit_code, 2);
        assert_eq!(
            budget.continue_command.as_deref(),
            Some("drip --resume abcdefgh --prompt 'do it' --max-iterations 5 --json")
        );
        assert_eq!(budget.continuation.as_ref().unwrap().suggested_max_iterations, Some(5));

        let uncapped = payload("aborted", None);
        assert_eq!(uncapped.continue_command.as_deref(), Some("drip --resume abcdefgh --prompt 'do it' --json"));

        assert_eq!(payload("error", None).exit_code, 3);

        // awaiting-input (ask_user timeout) exits 2, and the run's own
        // persisted continue command wins over the recomputed default.
        let mut awaiting_record = record("awaiting-input", None);
        awaiting_record.continue_command = Some("drip --resume abcdefgh".to_string());
        let awaiting = headless_result_payload(HeadlessResultArgs {
            record: &awaiting_record,
            result_path: "/r",
            session_id: "abcdefgh-1234",
            session_id_prefix: "abcdefgh",
            state_path: "/s",
            transcript_path: "/t",
        });
        assert_eq!(awaiting.exit_code, 2);
        assert_eq!(awaiting.continue_command.as_deref(), Some("drip --resume abcdefgh"));
    }

    /// Unreconciled work is complete work with a visible anomaly: exit 0, no
    /// continuation, and the anomalies ride along in the payload.
    #[test]
    fn unreconciled_exits_zero_and_carries_its_anomalies() {
        let mut unreconciled_record = record("unreconciled", Some(5));
        unreconciled_record.anomalies = Some(vec![crate::core::types::HarnessAnomaly {
            subject: "output sign".to_string(),
            expected: "positive".to_string(),
            observed: "negative".to_string(),
            note: "two derivations agree and neither explains the sign".to_string(),
        }]);
        let unreconciled = headless_result_payload(HeadlessResultArgs {
            record: &unreconciled_record,
            result_path: "/r",
            session_id: "abcdefgh-1234",
            session_id_prefix: "abcdefgh",
            state_path: "/s",
            transcript_path: "/t",
        });
        assert_eq!(unreconciled.exit_code, 0);
        assert!(unreconciled.continue_command.is_none());
        assert!(unreconciled.continuation.is_none());
        assert_eq!(unreconciled.anomalies.as_ref().map(|anomalies| anomalies.len()), Some(1));
        let json = serde_json::to_string(&unreconciled).unwrap();
        assert!(json.contains("\"reason\":\"unreconciled\""), "{json}");
        assert!(json.contains("\"anomalies\":[{\"subject\":\"output sign\""), "{json}");
    }

    #[test]
    fn serialized_key_order_matches_ts() {
        let json = serde_json::to_string(&payload("completed", None)).unwrap();
        assert!(json.starts_with("{\"continuation\":null,\"lastVerification\":null,\"pendingOperatorMessages\":0,\"taskStats\":{"), "{json}");
        assert!(json.contains("\"continueCommand\":null,\"exitCode\":0,\"goal\":\"do it\",\"goalId\":\"g1\",\"iterations\":3,\"loops\":2,\"reason\":\"completed\",\"resultPath\":\"/r\",\"sessionId\":\"abcdefgh-1234\",\"statePath\":\"/s\",\"summary\":\"done\",\"transcriptPath\":\"/t\",\"type\":\"result\",\"usage\":null}"), "{json}");
        assert!(!json.contains("errorMessage"));
    }
}
