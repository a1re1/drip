// Calibration trace: one structured line per finished task recording what
// the agent claimed (confidence) next to what the harness can classify about
// its evidence (external anchors vs self-authored checks, expectation tallies,
// output revisions, anomalies). A verifier's reward can be appended later
// with `drip --reward`, so "more rigor bought accuracy" and "more rigor bought
// confidence" stop looking identical on the scoreboard.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::core::types::{HarnessState, VerificationAnchorKind};

pub const CALIBRATION_FILE: &str = "calibration.jsonl";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceClass {
	/// Passing checks anchored to something the agent did not author.
	pub external_anchors: usize,
	/// Passing checks declared (or downgraded to) self-authored.
	pub self_authored: usize,
	/// Passing checks with no declared provenance.
	pub undeclared: usize,
	/// Verifications that failed.
	pub failed: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExpectationTally {
	pub registered: usize,
	pub matched: usize,
	pub mismatched: usize,
	pub unobserved: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CalibrationRecord {
	pub at: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub task_id: Option<String>,
	/// finish_task status that produced this record (completed | unreconciled),
	/// or "run" for the run-level record in the result payload.
	pub status: String,
	/// low | medium | high, as claimed by the agent.
	pub claimed_confidence: String,
	/// external | none: how the completion was anchored.
	pub anchor: String,
	pub evidence_class: EvidenceClass,
	pub expectations: ExpectationTally,
	/// Observations that changed an already-reported value for a subject.
	pub output_revisions: usize,
	pub anomalies: usize,
	/// Verifier reward, when one has been recorded against this record.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub reward: Option<f64>,
}

/// A verifier's score appended by `drip --reward`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RewardRecord {
	pub at: String,
	pub reward: f64,
}

/// One line of calibration.jsonl, discriminated by `kind` so a torn or
/// drifted task record can never be mistaken for a reward.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum CalibrationLine {
	Task(CalibrationRecord),
	Reward(RewardRecord),
}

/// What `read_calibration` found: the parseable lines in file order and how
/// many lines it had to drop, so a consumer can say so instead of scoring an
/// incomplete trace as if it were whole.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CalibrationTrace {
	pub lines: Vec<CalibrationLine>,
	pub dropped_lines: usize,
}

fn now() -> String {
	chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub fn evidence_class(state: &HarnessState) -> EvidenceClass {
	let mut class = EvidenceClass::default();
	for record in state.verifications.iter().flatten() {
		if record.failed {
			class.failed += 1;
			continue;
		}
		let Some(evidence) = record.evidence.as_ref().filter(|evidence| evidence.verifies_work()) else {
			continue;
		};
		match evidence.anchor.as_ref().map(|anchor| &anchor.kind) {
			Some(VerificationAnchorKind::External) => class.external_anchors += 1,
			Some(VerificationAnchorKind::SelfAuthored) => class.self_authored += 1,
			Some(VerificationAnchorKind::Undeclared) | None => class.undeclared += 1,
		}
	}
	class
}

pub fn expectation_tally(state: &HarnessState) -> ExpectationTally {
	let mut tally = ExpectationTally { registered: state.expectations.len(), ..Default::default() };
	for expectation in &state.expectations {
		match expectation.observations.last() {
			None => tally.unobserved += 1,
			Some(observation) if observation.matches => tally.matched += 1,
			Some(_) => tally.mismatched += 1,
		}
	}
	tally
}

/// Consecutive observations of one subject whose observed value differs:
/// each is a reported value that a later finish revised.
pub fn output_revisions(state: &HarnessState) -> usize {
	state
		.expectations
		.iter()
		.map(|expectation| {
			expectation
				.observations
				.windows(2)
				.filter(|pair| pair[0].observed != pair[1].observed)
				.count()
		})
		.sum()
}

/// Build the record for a task that just finished as completed or
/// unreconciled (or the run-level record, status "run"). None when the state
/// carries no completion anchor (a blocked finish, or a legacy state).
pub fn derive_calibration(state: &HarnessState, task_id: Option<&str>, status: &str) -> Option<CalibrationRecord> {
	let anchor = state.completion_anchor.as_ref()?;
	let claimed_confidence = match anchor.claimed_confidence {
		crate::core::types::ClaimedConfidence::Low => "low",
		crate::core::types::ClaimedConfidence::Medium => "medium",
		crate::core::types::ClaimedConfidence::High => "high",
	};
	Some(CalibrationRecord {
		at: now(),
		task_id: task_id.map(str::to_string),
		status: status.to_string(),
		claimed_confidence: claimed_confidence.to_string(),
		anchor: match anchor.kind {
			crate::core::types::CompletionAnchorKind::External => "external".to_string(),
			crate::core::types::CompletionAnchorKind::None => "none".to_string(),
		},
		evidence_class: evidence_class(state),
		expectations: expectation_tally(state),
		output_revisions: output_revisions(state),
		anomalies: state.anomalies.len(),
		reward: None,
	})
}

pub fn calibration_path(session_dir: &Path) -> PathBuf {
	session_dir.join(CALIBRATION_FILE)
}

fn append_line(session_dir: &Path, line: &str) -> std::io::Result<()> {
	std::fs::create_dir_all(session_dir)?;
	// True O_APPEND append (like the patch journal): concurrent writers never
	// clobber each other and earlier lines are never rewritten.
	let mut file = std::fs::OpenOptions::new().create(true).append(true).open(calibration_path(session_dir))?;
	use std::io::Write;
	file.write_all(line.as_bytes())?;
	file.write_all(b"\n")
}

/// Append a task record. Returns the io error so the caller can surface it
/// (the harness reports it as a run warning; it never fails the run).
pub fn append_calibration(session_dir: &Path, record: &CalibrationRecord) -> std::io::Result<()> {
	let line = serde_json::to_string(&CalibrationLine::Task(record.clone())).map_err(std::io::Error::other)?;
	append_line(session_dir, &line)
}

pub fn append_reward(session_dir: &Path, reward: f64) -> std::io::Result<RewardRecord> {
	let record = RewardRecord { at: now(), reward };
	let line = serde_json::to_string(&CalibrationLine::Reward(record.clone())).map_err(std::io::Error::other)?;
	append_line(session_dir, &line)?;
	Ok(record)
}

pub fn read_calibration(session_dir: &Path) -> std::io::Result<CalibrationTrace> {
	let text = std::fs::read_to_string(calibration_path(session_dir))?;
	let mut trace = CalibrationTrace::default();
	for line in text.lines().filter(|line| !line.trim().is_empty()) {
		match serde_json::from_str::<CalibrationLine>(line) {
			Ok(parsed) => trace.lines.push(parsed),
			Err(_) => trace.dropped_lines += 1,
		}
	}
	Ok(trace)
}

/// Task records with rewards applied: a reward line scores the records that
/// precede it and have not been scored by an earlier reward, so records
/// appended after the last reward stay unscored instead of inheriting a
/// score for work the verifier never saw.
pub fn merge_rewards(lines: &[CalibrationLine]) -> Vec<CalibrationRecord> {
	let mut merged: Vec<CalibrationRecord> = Vec::new();
	let mut first_unscored = 0;
	for line in lines {
		match line {
			CalibrationLine::Task(record) => merged.push(record.clone()),
			CalibrationLine::Reward(reward) => {
				for record in &mut merged[first_unscored..] {
					if record.reward.is_none() {
						record.reward = Some(reward.reward);
					}
				}
				first_unscored = merged.len();
			}
		}
	}
	merged
}

pub fn format_calibration(records: &[CalibrationRecord]) -> String {
	records
		.iter()
		.map(|record| {
			format!(
				"task {}: {} claimed {}, anchor {}, evidence external={} self={} undeclared={} failed={}, expectations {}/{} matched ({} mismatched, {} unobserved), revisions {}, anomalies {}, reward {}",
				record.task_id.as_deref().unwrap_or("-"),
				record.status,
				record.claimed_confidence,
				record.anchor,
				record.evidence_class.external_anchors,
				record.evidence_class.self_authored,
				record.evidence_class.undeclared,
				record.evidence_class.failed,
				record.expectations.matched,
				record.expectations.registered,
				record.expectations.mismatched,
				record.expectations.unobserved,
				record.output_revisions,
				record.anomalies,
				record.reward.map(|reward| format!("{reward}")).unwrap_or_else(|| "unscored".to_string()),
			)
		})
		.collect::<Vec<_>>()
		.join("\n")
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::core::types::{
		ClaimedConfidence, CompletionAnchor, CompletionAnchorKind, HarnessExpectation, HarnessExpectationObservation,
		HarnessVerificationRecord, VerificationAnchor,
	};

	fn verification(failed: bool, anchor: Option<VerificationAnchorKind>) -> HarnessVerificationRecord {
		let mut evidence = crate::tools::builtin::verify::verification_evidence(
			"python check.py",
			"DRIP_VERIFY {\"executed\":1,\"passed\":1,\"failed\":0}",
		);
		evidence.anchor = anchor.map(|kind| VerificationAnchor { kind, source: None, downgraded_reason: None });
		HarnessVerificationRecord {
			at_iteration: 1,
			command: "python check.py".into(),
			failed,
			output_tail: String::new(),
			ran_no_tests: None,
			evidence: Some(evidence),
		}
	}

	fn observation(observed: &str, matches: bool) -> HarnessExpectationObservation {
		HarnessExpectationObservation { at_iteration: 2, observed: observed.into(), matches, evidence: None }
	}

	#[test]
	fn derives_evidence_class_expectations_and_revisions_from_state() {
		let mut state = crate::core::state::create_harness_state("measure");
		state.verifications = Some(vec![
			verification(false, Some(VerificationAnchorKind::External)),
			verification(false, Some(VerificationAnchorKind::SelfAuthored)),
			verification(false, None),
			verification(true, Some(VerificationAnchorKind::External)),
		]);
		state.expectations = vec![
			HarnessExpectation {
				id: "e1".into(), subject: "total".into(), expected: "about 100".into(), registered_at_iteration: 1,
				observations: vec![observation("98", true), observation("104", true), observation("104", true)],
			},
			HarnessExpectation {
				id: "e2".into(), subject: "sign".into(), expected: "positive".into(), registered_at_iteration: 1,
				observations: vec![observation("negative", false)],
			},
			HarnessExpectation {
				id: "e3".into(), subject: "rows".into(), expected: "12".into(), registered_at_iteration: 1,
				observations: vec![],
			},
		];
		assert_eq!(derive_calibration(&state, Some("task-1"), "completed"), None, "no anchor, no record");

		state.completion_anchor = Some(CompletionAnchor {
			kind: CompletionAnchorKind::None,
			note: Some("no fixture".into()),
			claimed_confidence: ClaimedConfidence::High,
		});
		let record = derive_calibration(&state, Some("task-1"), "unreconciled").expect("record");
		assert_eq!(record.evidence_class, EvidenceClass { external_anchors: 1, self_authored: 1, undeclared: 1, failed: 1 });
		assert_eq!(record.expectations, ExpectationTally { registered: 3, matched: 1, mismatched: 1, unobserved: 1 });
		assert_eq!(record.output_revisions, 1);
		assert_eq!(record.claimed_confidence, "high");
		assert_eq!(record.anchor, "none");
		assert_eq!(record.status, "unreconciled");
	}

	/// Rewards score the records before them; later records stay unscored;
	/// a torn line is counted as dropped, never parsed as a reward.
	#[test]
	fn appends_tagged_lines_scopes_rewards_and_counts_dropped_lines() {
		let dir = tempfile::tempdir().unwrap();
		let mut state = crate::core::state::create_harness_state("measure");
		state.completion_anchor = Some(CompletionAnchor {
			kind: CompletionAnchorKind::External,
			note: None,
			claimed_confidence: ClaimedConfidence::Medium,
		});
		let record = derive_calibration(&state, Some("task-1"), "completed").unwrap();
		append_calibration(dir.path(), &record).unwrap();
		append_calibration(dir.path(), &record).unwrap();
		append_reward(dir.path(), 0.5).unwrap();
		append_calibration(dir.path(), &record).unwrap();
		{
			use std::io::Write;
			let mut file = std::fs::OpenOptions::new().append(true).open(calibration_path(dir.path())).unwrap();
			file.write_all(b"{\"at\":\"2026-01-01T00:00:00Z\",\"reward\":0.9}\n{\"kind\":\"task\",\"at\":\"torn").unwrap();
		}

		let text = std::fs::read_to_string(calibration_path(dir.path())).unwrap();
		assert!(text.starts_with("{\"kind\":\"task\","), "{text}");
		assert!(text.contains("{\"kind\":\"reward\","), "{text}");
		let trace = read_calibration(dir.path()).unwrap();
		assert_eq!(trace.lines.len(), 4);
		assert_eq!(trace.dropped_lines, 2, "an untagged reward and a torn line are dropped, not misread");
		let merged = merge_rewards(&trace.lines);
		assert_eq!(merged.len(), 3);
		assert_eq!(merged[0].reward, Some(0.5));
		assert_eq!(merged[1].reward, Some(0.5));
		assert_eq!(merged[2].reward, None, "appended after the reward: unscored");
		let formatted = format_calibration(&merged);
		assert!(formatted.starts_with("task task-1: completed claimed medium, anchor external, evidence external=0"), "{formatted}");
		assert!(formatted.contains("reward 0.5") && formatted.ends_with("reward unscored"), "{formatted}");
	}
}
