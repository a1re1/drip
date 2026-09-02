use std::path::Path;
use std::thread::sleep;
use std::time::Duration;

use crate::cli::run_record::{load_run_record, RunRecord};
use crate::core::lease::check_lease;

#[derive(Debug, Clone, PartialEq)]
pub enum WaitOutcome {
    Result { record: RunRecord },
    /// The lease died without a new run record — the run crashed mid-flight.
    Crashed,
    /// No run is live and none has ever recorded a result.
    NoRun,
    Timeout,
}

// drip --wait: the blocking primitive --follow deliberately isn't. Attach to a
// session; if a run is live, block until its lease clears, then report the
// run record it persisted. If no run is live, the latest record answers
// immediately — so "start detached, then wait" and "wait on a finished run"
// both work without the caller polling leases themselves.
pub fn wait_for_run_end(args: WaitForRunEndArgs<'_>) -> WaitOutcome {
    let poll_ms = args.poll_ms.unwrap_or(500);
    let now = args.now.unwrap_or(&(|| chrono::Utc::now()));
    let started_at = now().timestamp_millis();
    let record_at_attach = load_run_record(args.result_path);
    let live_at_attach = check_lease(args.lease_path, now).alive();

    if !live_at_attach {
        return match record_at_attach {
            Some(record) => WaitOutcome::Result { record },
            None => WaitOutcome::NoRun,
        };
    }

    loop {
        if let Some(timeout_ms) = args.timeout_ms {
            if now().timestamp_millis() - started_at >= timeout_ms as i64 {
                return WaitOutcome::Timeout;
            }
        }

        sleep(Duration::from_millis(poll_ms));

        if check_lease(args.lease_path, now).alive() {
            continue;
        }

        // The lease cleared. A run that ended cleanly persisted its record after
        // releasing nothing — the record write happens before the lease clears in
        // run_drip_goal's teardown — but poll a short grace window anyway so a
        // record landing between our lease check and record read isn't misread
        // as a crash.
        let grace_deadline =
            now().timestamp_millis() + (args.grace_ms.unwrap_or_else(|| std::cmp::max(poll_ms * 4, 2000)) as i64);

        loop {
            let record = load_run_record(args.result_path);
            let is_new_record = match &record {
                Some(record) => match &record_at_attach {
                    None => true,
                    Some(at_attach) => {
                        record.ended_at != at_attach.ended_at || record.goal_id != at_attach.goal_id
                    }
                },
                None => false,
            };

            if is_new_record {
                if let Some(record) = record {
                    return WaitOutcome::Result { record };
                }
            }

            if now().timestamp_millis() >= grace_deadline {
                return WaitOutcome::Crashed;
            }

            sleep(Duration::from_millis(std::cmp::min(poll_ms, 200)));
        }
    }
}

pub struct WaitForRunEndArgs<'a> {
    pub lease_path: &'a Path,
    pub result_path: &'a Path,
    /// Post-lease-death window to wait for a record before calling it a crash (test seam; defaults to max(pollMs*4, 2000)).
    pub grace_ms: Option<u64>,
    pub poll_ms: Option<u64>,
    pub timeout_ms: Option<u64>,
    pub now: Option<&'a dyn Fn() -> chrono::DateTime<chrono::Utc>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_lease_no_record_returns_no_run() {
        let dir = tempfile::tempdir().unwrap();
        let result = wait_for_run_end(WaitForRunEndArgs {
            lease_path: &dir.path().join("missing.lease"),
            result_path: &dir.path().join("missing-result.json"),
            grace_ms: None,
            poll_ms: Some(10),
            timeout_ms: None,
            now: None,
        });
        assert_eq!(result, WaitOutcome::NoRun);
    }

    #[test]
    fn no_lease_with_record_returns_result() {
        let dir = tempfile::tempdir().unwrap();
        let result_path = dir.path().join("result.json");
        // Full RunRecord shape — load_run_record rejects a partial record.
        std::fs::write(
            &result_path,
            r#"{
                "goalId": "g1",
                "goal": "fix the flake",
                "iterations": 3,
                "lastVerification": null,
                "loops": 2,
                "pendingOperatorMessages": 0,
                "reason": "completed",
                "summary": null,
                "usage": null,
                "taskStats": { "blocked": 0, "completed": 1, "dropped": 0, "pending": 2 },
                "endedAt": "2026-01-01T00:01:00Z"
            }"#,
        )
        .unwrap();
        let result = wait_for_run_end(WaitForRunEndArgs {
            lease_path: &dir.path().join("missing.lease"),
            result_path: &result_path,
            grace_ms: None,
            poll_ms: Some(10),
            timeout_ms: None,
            now: None,
        });
        match result {
            WaitOutcome::Result { record } => assert_eq!(record.goal_id, "g1"),
            other => panic!("expected Result, got {:?}", other),
        }
    }

    #[test]
    fn live_lease_with_timeout_returns_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let lease_path = dir.path().join("session.lease");
        crate::core::lease::write_lease(&lease_path, &|| chrono::Utc::now());
        let result = wait_for_run_end(WaitForRunEndArgs {
            lease_path: &lease_path,
            result_path: &dir.path().join("missing-result.json"),
            grace_ms: None,
            poll_ms: Some(10),
            timeout_ms: Some(50),
            now: None,
        });
        assert_eq!(result, WaitOutcome::Timeout);
    }
}
