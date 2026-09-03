// Transcript JSONL reading: a small inline JSONL reader (blank lines
// skipped, parse failures kept as raw-only records) plus the typed entry
// model and its hand-rolled serializer below.
// `read_jsonl_records`, with its provenance in its own comment.

use std::fs;
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::core::types::{HarnessEventData, HarnessEventType, HarnessRunReason};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptGoalEntry {
	pub at: String,
	pub goal_id: String,
	pub images: Vec<String>,
	pub mentions: Vec<String>,
	pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptEventEntry {
	pub at: String,
	/// Structured per-kind payload (toolName/failed/durationMs/waitSeconds/...) — see HarnessEventData.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub data: Option<HarnessEventData>,
	pub detail: String,
	pub goal_id: String,
	pub iteration: i64,
	pub kind: HarnessEventType,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptRunEndEntry {
	pub at: String,
	pub goal_id: String,
	pub iterations: i64,
	pub reason: HarnessRunReason,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptNoteEntry {
	pub at: String,
	pub text: String,
}

// The inference route a run was launched on, written once per goal at run
// start. Records what actually served the run rather than what the config says
// now: profiles get re-pointed between runs, so a transcript read weeks later
// is the only place the model/effort pairing survives. `provider` is a plain
// string (not InferenceProviderId) because this is persisted data — an entry
// written by a newer build must still parse on an older one.
// One role's inference route on a run that activated roles. `model` is absent
// when the role declared no model of its own — it inherits the run's tool
// route, then the base model — and `binding` is set for the role the harness
// bound to a loop kind ("planning" or "task"), which is the only way drip runs
// a distinct planning model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptModelRoleRoute {
	#[serde(skip_serializing_if = "Option::is_none")]
	pub binding: Option<String>,
	/// Every loop kind this role is bound to ("planning", "task") — a role can hold more than one.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub bindings: Option<Vec<String>>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub model: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub provider: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub reasoning_effort: Option<String>,
	/// Serialized last, after the route, when present.
	pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptModelEntry {
	pub at: String,
	pub goal_id: String,
	pub model: String,
	/// Profile id from the resolved route (e.g. "glm-5-2").
	pub profile_id: String,
	pub provider: String,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub reasoning_effort: Option<String>,
	/// Per-role routes, when the run activated roles.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub roles: Option<Vec<TranscriptModelRoleRoute>>,
	/// Set only when a distinct tool-calling route is configured.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub tool_model: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub tool_profile_id: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub tool_reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptSkillEntry {
	pub at: String,
	pub enabled: bool,
	pub name: String,
}

// serde's internally-tagged enum discriminates on the `type` field; each
// variant carries only its own fields. `TranscriptNoteEntry` backs both
// "error" and "info".
//
// Serialization is hand-written (below) rather than derived: the derive puts
// the tag first, but the on-disk entries order their keys alphabetically —
// `type` lands last — and byte-stable output keeps `tail -f` / `diff` of
// transcript.jsonl files meaningful.
// sorts the keys the same way.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type")]
pub enum TranscriptEntry {
	#[serde(rename = "error")]
	Error(TranscriptNoteEntry),
	#[serde(rename = "event")]
	Event(TranscriptEventEntry),
	#[serde(rename = "goal")]
	Goal(TranscriptGoalEntry),
	#[serde(rename = "info")]
	Info(TranscriptNoteEntry),
	#[serde(rename = "model")]
	Model(TranscriptModelEntry),
	#[serde(rename = "run-end")]
	RunEnd(TranscriptRunEndEntry),
	#[serde(rename = "skill")]
	Skill(TranscriptSkillEntry),
}

impl TranscriptEntry {
	/// The `type` discriminant a line carries.
	pub fn tag(&self) -> &'static str {
		match self {
			TranscriptEntry::Error(_) => "error",
			TranscriptEntry::Event(_) => "event",
			TranscriptEntry::Goal(_) => "goal",
			TranscriptEntry::Info(_) => "info",
			TranscriptEntry::Model(_) => "model",
			TranscriptEntry::RunEnd(_) => "run-end",
			TranscriptEntry::Skill(_) => "skill",
		}
	}
}

impl Serialize for TranscriptEntry {
	fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
		let inner = match self {
			TranscriptEntry::Error(e) | TranscriptEntry::Info(e) => serde_json::to_value(e),
			TranscriptEntry::Event(e) => serde_json::to_value(e),
			TranscriptEntry::Goal(e) => serde_json::to_value(e),
			TranscriptEntry::Model(e) => serde_json::to_value(e),
			TranscriptEntry::RunEnd(e) => serde_json::to_value(e),
			TranscriptEntry::Skill(e) => serde_json::to_value(e),
		}
		.map_err(serde::ser::Error::custom)?;
		let mut fields = match inner {
			serde_json::Value::Object(map) => map,
			_ => return Err(serde::ser::Error::custom("transcript entry must serialize to an object")),
		};
		fields.insert("type".to_string(), serde_json::Value::String(self.tag().to_string()));
		// Top-level keys are emitted in alphabetical order (all ASCII, so a
		// byte-wise sort). Nested objects keep their own struct order.
		let mut keys: Vec<String> = fields.keys().cloned().collect();
		keys.sort();
		let mut ordered = serde_json::Map::with_capacity(keys.len());
		for key in keys {
			if let Some(value) = fields.remove(&key) {
				ordered.insert(key, value);
			}
		}
		ordered.serialize(serializer)
	}
}

/// The entry discriminants `read_transcript` accepts; every other line
/// in the file is skipped.
/// `read_transcript` accepts; every other line in the file is skipped.
pub const TRANSCRIPT_ENTRY_TYPES: [&str; 7] = [
	"error",
	"event",
	"goal",
	"info",
	"model",
	"run-end",
	"skill",
];

pub fn append_transcript_entry(
	transcript_path: &Path,
	entry: &TranscriptEntry,
) -> std::io::Result<()> {
	// Create the parent directory too. A bare file name has no parent
	// component: Path::parent then yields "", so "." is substituted.
	if let Some(parent) = transcript_path.parent() {
		let parent = if parent.as_os_str().is_empty() {
			Path::new(".")
		} else {
			parent
		};
		fs::create_dir_all(parent)?;
	}

	// Appends the entry as one JSONL line.
	let mut file = fs::OpenOptions::new()
		.create(true)
		.append(true)
		.open(transcript_path)?;
	writeln!(
		file,
		"{}",
		serde_json::to_string(entry).expect("TranscriptEntry serializes")
	)
}

/// One JSONL record: `raw` is the line verbatim, `parsed` is the decoded
/// value, or None when the line failed to parse.
/// `raw` is the line verbatim, `parsed` is None when it failed JSON.parse.
#[derive(Debug, Clone, PartialEq)]
pub struct JsonlRecord {
	pub parsed: Option<serde_json::Value>,
	pub raw: String,
}

// Thin adapter over crate::lib_fs::read_jsonl_records: a missing file reads
// as empty.
pub fn read_jsonl_records(path: &Path) -> Vec<JsonlRecord> {
	crate::lib_fs::read_jsonl_records::<serde_json::Value>(path)
		.into_iter()
		.map(|record| JsonlRecord { parsed: record.parsed, raw: record.raw })
		.collect()
}

pub fn read_transcript(transcript_path: &Path) -> Vec<TranscriptEntry> {
	let mut entries: Vec<TranscriptEntry> = Vec::new();

	for record in read_jsonl_records(transcript_path) {
		let Some(parsed) = record.parsed else {
			continue;
		};
		// `typeof parsed === "object" && typeof parsed.type === "string"` —
		// serde's Value::get mirrors the `"type" in parsed` lookup.
		if let Some(entry_type) = parsed.get("type").and_then(|t| t.as_str()) {
			if TRANSCRIPT_ENTRY_TYPES.contains(&entry_type) {
				// A malformed (yet known-typed) line must not poison the whole
				// read, so fall back to skipping on a parse error, same as an
				// unknown type.
				// back to skipping on a parse error, same as an unknown type.
				if let Ok(entry) = serde_json::from_value::<TranscriptEntry>(parsed.clone()) {
					entries.push(entry);
				}
			}
		}
	}

	entries
}

// The recorded inference routes as display lines, most general first: the base
// (coding) model, the tool-calling route when it differs, then one line per
// role. Shared by every surface that shows them (drip --follow, the TUI
// timeline, the dripw header) so they never drift apart.
pub fn format_model_route_lines(entry: &TranscriptModelEntry) -> Vec<String> {
	// effort = (value) => value ? ` · effort ${value}` : ""
	let effort = |value: Option<&str>| -> String {
		value
			.map(|v| format!(" · effort {v}"))
			.unwrap_or_default()
	};
	let mut lines = vec![format!(
		"model {}{}",
		entry.model,
		effort(entry.reasoning_effort.as_deref())
	)];

	if let Some(tool_model) = entry.tool_model.as_deref() {
		if tool_model != entry.model {
			lines.push(format!(
				"tools {}{}",
				tool_model,
				effort(entry.tool_reasoning_effort.as_deref())
			));
		}
	}

	for role in entry.roles.as_deref().unwrap_or(&[]) {
		let label = match role.binding.as_deref() {
			Some(binding) => format!("{binding} · {}", role.name),
			None => format!("role {}", role.name),
		};
		lines.push(format!(
			"{} {}{}",
			label,
			role.model.as_deref().unwrap_or("(inherits)"),
			effort(role.reasoning_effort.as_deref())
		));
	}

	lines
}

/// The same routes collapsed onto one line, for line-oriented output.
pub fn format_model_route(entry: &TranscriptModelEntry) -> String {
	format_model_route_lines(entry).join(" | ")
}

#[cfg(test)]
mod tests {
	use super::*;

	fn make_temp_root() -> tempfile::TempDir {
		// A temp dir under the OS temp location; TempDir removes it on drop.
		// dir; TempDir removes it on drop (the vitest afterEach cleanup).
		tempfile::tempdir().expect("makeTempRoot")
	}

	// `type` is the last key in each transcript line (alphabetical key
	// order), and a line round-trips through the tagged deserializer.
	#[test]
	fn serializes_with_canonical_key_order() {
		let entry = TranscriptEntry::RunEnd(TranscriptRunEndEntry {
			at: "2026-07-01T00:00:02.000Z".into(),
			goal_id: "goal-1".into(),
			iterations: 3,
			reason: HarnessRunReason::Completed,
		});
		let line = serde_json::to_string(&entry).unwrap();
		assert_eq!(
			line,
			r#"{"at":"2026-07-01T00:00:02.000Z","goalId":"goal-1","iterations":3,"reason":"completed","type":"run-end"}"#
		);
		assert_eq!(serde_json::from_str::<TranscriptEntry>(&line).unwrap(), entry);

		let event = TranscriptEntry::Event(TranscriptEventEntry {
			at: "2026-07-01T00:00:01.000Z".into(),
			data: Some(HarnessEventData::default()),
			detail: "d".into(),
			goal_id: "goal-1".into(),
			iteration: 1,
			kind: HarnessEventType::ToolCall,
		});
		let keys: Vec<String> = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(
			&serde_json::to_string(&event).unwrap(),
		)
		.unwrap()
		.keys()
		.cloned()
		.collect();
		assert_eq!(keys, ["at", "data", "detail", "goalId", "iteration", "kind", "type"]);
	}

	#[test]
	fn appends_entries_and_replays_them_in_order() {
		let root = make_temp_root();
		let transcript_path = root.path().join("sessions").join("abc").join("transcript.jsonl");
		let goal = TranscriptEntry::Goal(TranscriptGoalEntry {
			at: "2026-07-01T00:00:00.000Z".into(),
			goal_id: "goal-1".into(),
			images: vec![],
			mentions: vec!["@src/dev.ts".into()],
			text: "explain @src/dev.ts".into(),
		});
		let event = TranscriptEntry::Event(TranscriptEventEntry {
			at: "2026-07-01T00:00:01.000Z".into(),
			data: None,
			detail: "task-1: read the file".into(),
			goal_id: "goal-1".into(),
			iteration: 1,
			kind: HarnessEventType::IterationStart,
		});

		append_transcript_entry(&transcript_path, &goal).unwrap();
		append_transcript_entry(&transcript_path, &event).unwrap();

		assert_eq!(read_transcript(&transcript_path), vec![goal, event]);
	}

	#[test]
	fn skips_torn_lines_and_unknown_entry_types() {
		let root = make_temp_root();
		let transcript_path = root.path().join("transcript.jsonl");

		append_transcript_entry(
			&transcript_path,
			&TranscriptEntry::Info(TranscriptNoteEntry {
				at: "2026-07-01T00:00:00.000Z".into(),
				text: "hello".into(),
			}),
		)
		.unwrap();
		// Feeds a truncated two-record line (the second record cut off mid-key).
		let mut file = fs::OpenOptions::new().append(true).open(&transcript_path).unwrap();
		file.write_all(b"{\"type\":\"mystery\",\"at\":\"x\"}\n{\"type\":\"goal\",\"tor").unwrap();

		let entries = read_transcript(&transcript_path);

		assert_eq!(entries.len(), 1);
		assert!(
			matches!(&entries[0], TranscriptEntry::Info(note) if note.text == "hello")
		);
		assert!(fs::read_to_string(&transcript_path).unwrap().contains("mystery"));
		assert_eq!(read_transcript(&root.path().join("missing.jsonl")), Vec::new());
	}
}
