use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::path::Path;

use crate::cli::transcript::format_model_route;
use crate::cli::transcript::read_jsonl_records;
use crate::cli::transcript::TranscriptEntry;

// One formatted line per transcript entry, matching the headless runner's
// event format so following a session reads like watching the run itself.
pub fn format_transcript_entry_line(entry: &TranscriptEntry) -> String {
	match entry {
		TranscriptEntry::Goal(entry) => format!("=== goal: {}", entry.text),
		TranscriptEntry::Event(entry) => format!(
			"[{:>3}] {} {}",
			entry.iteration,
			serde_json::to_value(&entry.kind).unwrap().as_str().unwrap(),
			entry.detail
		),
		TranscriptEntry::Model(entry) => format!("=== {}", format_model_route(entry)),
		TranscriptEntry::RunEnd(entry) => format!(
			"=== run ended ({}) after {} cycle(s)",
			serde_json::to_value(&entry.reason).unwrap().as_str().unwrap(),
			entry.iterations
		),
		TranscriptEntry::Info(entry) => format!("--- {}", entry.text),
		TranscriptEntry::Error(entry) => format!("!!! {}", entry.text),
		// Any entry type not named above (the skill entry) falls back to the
		// whole entry serialized as JSON after the "--- " marker.
		_ => format!("--- {}", serde_json::to_string(entry).unwrap()),
	}
}

// Incremental reader over an append-only JSONL file: returns complete new
// lines past the previous offset, never a torn tail mid-append.
pub fn read_appended_jsonl_lines(path: &Path, from_offset: u64) -> AppendedJsonlLines {
	let Ok(metadata) = std::fs::metadata(path) else {
		return AppendedJsonlLines { lines: Vec::new(), next_offset: from_offset };
	};

	let size = metadata.len();

	if size <= from_offset {
		// A truncated/rewritten file restarts the cursor rather than pointing
		// past the end forever.
		return if size < from_offset {
			AppendedJsonlLines { lines: Vec::new(), next_offset: 0 }
		} else {
			AppendedJsonlLines { lines: Vec::new(), next_offset: from_offset }
		};
	}

	let mut file = match std::fs::File::open(path) {
		Ok(file) => file,
		Err(_) => return AppendedJsonlLines { lines: Vec::new(), next_offset: from_offset },
	};

	if file.seek(SeekFrom::Start(from_offset)).is_err() {
		return AppendedJsonlLines { lines: Vec::new(), next_offset: from_offset };
	}

	let mut buffer = Vec::new();

	if file.read_to_end(&mut buffer).is_err() {
		return AppendedJsonlLines { lines: Vec::new(), next_offset: from_offset };
	}

	let chunk = String::from_utf8_lossy(&buffer).into_owned();

	let last_newline_index = match chunk.rfind('\n') {
		Some(index) => index,
		None => return AppendedJsonlLines { lines: Vec::new(), next_offset: from_offset },
	};

	let complete = &chunk[..last_newline_index];

	let lines = if !complete.is_empty() {
		complete
			.split('\n')
			.filter(|line| !line.trim().is_empty())
			.map(|line| line.to_string())
			.collect()
	} else {
		Vec::new()
	};

	AppendedJsonlLines {
		lines,
		// Byte length of the consumed prefix through its newline — offsets are
		// always UTF-8 byte counts.
		next_offset: from_offset + (last_newline_index + 1) as u64,
	}
}

#[derive(Debug, Clone, PartialEq)]
pub struct InboxEntry {
	/// When --send appended the message; None on malformed lines.
	pub at: Option<String>,
	pub text: String,
}

// Inbox entries beyond the already-consumed count. Malformed lines return
// text "" (still counted, so the cursor never drifts); the harness skips
// empties. The send timestamp rides along so consumers can report steering
// adoption latency.
pub fn read_inbox_entries(inbox_path: &Path, consumed_count: usize) -> Vec<InboxEntry> {
	if !inbox_path.exists() {
		return Vec::new();
	}

	let records = read_jsonl_records(inbox_path);

	records
		.into_iter()
		.skip(consumed_count)
		.map(|record| match record.parsed {
			None => InboxEntry { at: None, text: String::new() },
			Some(parsed) => InboxEntry {
				at: parsed.get("at").and_then(|value| value.as_str()).map(String::from),
				text: parsed
					.get("text")
					.and_then(|value| value.as_str())
					.unwrap_or("")
					.to_string(),
			},
		})
		.collect()
}

pub fn read_inbox_messages(inbox_path: &Path, consumed_count: usize) -> Vec<String> {
	read_inbox_entries(inbox_path, consumed_count)
		.into_iter()
		.map(|entry| entry.text)
		.collect()
}

// A malformed line, a non-object, or a `type` outside the transcript enum
// all parse as None.
pub fn parse_transcript_line(line: &str) -> Option<TranscriptEntry> {
	let parsed: serde_json::Value = serde_json::from_str(line).ok()?;

	if !parsed.is_object() {
		return None;
	}

	parsed.get("type")?.as_str()?;

	serde_json::from_str::<TranscriptEntry>(line).ok()
}

pub struct AppendedJsonlLines {
	pub lines: Vec<String>,
	pub next_offset: u64,
}

#[cfg(test)]
mod tests {
	use std::fs;
	use std::path::Path;

	use super::format_transcript_entry_line;
	use super::parse_transcript_line;
	use super::read_appended_jsonl_lines;
	use super::read_inbox_entries;
	use super::read_inbox_messages;
	use crate::cli::transcript::TranscriptEntry;

	fn write_file(path: &Path, contents: &str) {
		if let Some(parent) = path.parent() {
			std::fs::create_dir_all(parent).unwrap();
		}
		fs::write(path, contents).unwrap();
	}

	#[test]
	fn test_read_appended_jsonl_lines_returns_only_complete_lines_and_advances() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("session.jsonl");

		// Missing file: nothing to read, cursor unchanged.
		let result = read_appended_jsonl_lines(&path, 0);
		assert!(result.lines.is_empty());
		assert_eq!(result.next_offset, 0);

		write_file(&path, "{\"n\":1}\n{\"n\":2}\n{\"n\":3}");

		// The trailing line has no newline yet — it is a torn tail mid-append
		// and must not be returned; the cursor stops after line 2.
		let result = read_appended_jsonl_lines(&path, 0);
		assert_eq!(result.lines, vec!["{\"n\":1}", "{\"n\":2}"]);
		assert_eq!(result.next_offset, 16);

		// Completing the line makes it visible from the previous offset.
		write_file(&path, "{\"n\":1}\n{\"n\":2}\n{\"n\":3}\n");
		let result = read_appended_jsonl_lines(&path, 16);
		assert_eq!(result.lines, vec!["{\"n\":3}"]);
		assert_eq!(result.next_offset, 24);

		// No new bytes: nothing to report, cursor unchanged.
		let result = read_appended_jsonl_lines(&path, 24);
		assert!(result.lines.is_empty());
		assert_eq!(result.next_offset, 24);
	}

	#[test]
	fn test_read_appended_jsonl_lines_skips_blank_lines() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("session.jsonl");

		write_file(&path, "\n\n{\"n\":1}\n\n");

		let result = read_appended_jsonl_lines(&path, 0);
		assert_eq!(result.lines, vec!["{\"n\":1}"]);
		assert_eq!(result.next_offset, 11);
	}

	#[test]
	fn test_read_appended_jsonl_lines_restarts_on_shrunk_file() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("session.jsonl");

		write_file(&path, "{\"n\":1}\n{\"n\":2}\n");
		assert_eq!(read_appended_jsonl_lines(&path, 0).next_offset, 16);

		// A truncated/rewritten file is shorter than the cursor — the cursor
		// restarts at 0 rather than pointing past the end forever.
		write_file(&path, "{\"n\":1}\n");
		let result = read_appended_jsonl_lines(&path, 16);
		assert!(result.lines.is_empty());
		assert_eq!(result.next_offset, 0);

		// From the reset cursor the rewritten content reads normally.
		let result = read_appended_jsonl_lines(&path, 0);
		assert_eq!(result.lines, vec!["{\"n\":1}"]);
		assert_eq!(result.next_offset, 8);
	}

	#[test]
	fn test_read_inbox_entries_skips_consumed_records() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("inbox.jsonl");

		// Missing inbox: no entries.
		assert!(read_inbox_entries(&path, 0).is_empty());

		write_file(
			&path,
			concat!(
				"{\"at\":\"2026-01-01T00:00:00.000Z\",\"text\":\"first\"}\n",
				"{\"at\":\"2026-01-01T00:00:01.000Z\",\"text\":\"second\"}\n",
				"{\"at\":\"2026-01-01T00:00:02.000Z\",\"text\":\"third\"}\n"
			),
		);

		let entries = read_inbox_entries(&path, 1);
		assert_eq!(entries.len(), 2);
		assert_eq!(entries[0].at.as_deref(), Some("2026-01-01T00:00:01.000Z"));
		assert_eq!(entries[0].text, "second");
		assert_eq!(entries[1].text, "third");

		assert_eq!(read_inbox_messages(&path, 2), vec!["third".to_string()]);
		assert!(read_inbox_entries(&path, 99).is_empty());
	}

	#[test]
	fn test_read_inbox_entries_malformed_lines_yield_empty_entry() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("inbox.jsonl");

		write_file(
			&path,
			concat!(
				"not json at all\n",
				"{\"text\":\"no timestamp\"}\n",
				"{\"at\":42,\"text\":7}\n",
				"{\"at\":\"2026-01-01T00:00:00.000Z\",\"text\":\"ok\"}\n"
			),
		);

		let entries = read_inbox_entries(&path, 0);
		assert_eq!(entries.len(), 4);
		// Malformed line: still counted, but at/text are empty so the harness
		// skips it and the cursor never drifts.
		assert_eq!(entries[0], super::InboxEntry { at: None, text: String::new() });
		assert_eq!(entries[1].at, None);
		assert_eq!(entries[1].text, "no timestamp");
		// Non-string at/text fall back to null/"".
		assert_eq!(entries[2], super::InboxEntry { at: None, text: String::new() });
		assert_eq!(entries[3].at.as_deref(), Some("2026-01-01T00:00:00.000Z"));
		assert_eq!(entries[3].text, "ok");
	}

	#[test]
	fn test_format_transcript_entry_line() {
		let parse = |line: &str| -> TranscriptEntry { parse_transcript_line(line).unwrap() };

		// goal
		let entry = parse(
			"{\"type\":\"goal\",\"at\":\"t\",\"goalId\":\"g1\",\"images\":[],\"mentions\":[],\"text\":\"ship it\"}",
		);
		assert_eq!(format_transcript_entry_line(&entry), "=== goal: ship it");

		// event — iteration is zero-padded to 3 columns.
		let entry = parse(
			"{\"type\":\"event\",\"at\":\"t\",\"goalId\":\"g1\",\"iteration\":7,\"kind\":\"tool-call\",\"detail\":\"ran bash\"}",
		);
		assert_eq!(format_transcript_entry_line(&entry), "[  7] tool-call ran bash");

		// model — delegated to format_model_route.
		let entry = parse(
			"{\"type\":\"model\",\"at\":\"t\",\"goalId\":\"g1\",\"model\":\"m\",\"profileId\":\"p\",\"provider\":\"anthropic\"}",
		);
		let line = format_transcript_entry_line(&entry);
		assert!(line.starts_with("=== "), "model line should reuse formatModelRoute: {line}");

		// run-end
		let entry = parse(
			"{\"type\":\"run-end\",\"at\":\"t\",\"goalId\":\"g1\",\"iterations\":3,\"reason\":\"completed\"}",
		);
		let line = format_transcript_entry_line(&entry);
		assert!(
			line.starts_with("=== run ended (") && line.ends_with(") after 3 cycle(s)"),
			"unexpected run-end line: {line}"
		);

		// info / error
		let entry = parse("{\"type\":\"info\",\"at\":\"t\",\"text\":\"heads up\"}");
		assert_eq!(format_transcript_entry_line(&entry), "--- heads up");

		let entry = parse("{\"type\":\"error\",\"at\":\"t\",\"text\":\"boom\"}");
		assert_eq!(format_transcript_entry_line(&entry), "!!! boom");
	}

	#[test]
	fn test_parse_transcript_line() {
		// Valid entry parses.
		let entry = parse_transcript_line("{\"type\":\"info\",\"at\":\"t\",\"text\":\"hi\"}");
		assert!(matches!(entry, Some(TranscriptEntry::Info(_))));

		// Not JSON.
		assert!(parse_transcript_line("nope").is_none());
		// JSON but not an object.
		assert!(parse_transcript_line("[1,2]").is_none());
		assert!(parse_transcript_line("\"str\"").is_none());
		// Object without a string type.
		assert!(parse_transcript_line("{\"text\":\"hi\"}").is_none());
		assert!(parse_transcript_line("{\"type\":7}").is_none());
		// Unknown type: valid object+string but outside the enum.
		assert!(parse_transcript_line("{\"type\":\"nope\"}").is_none());
	}
}
