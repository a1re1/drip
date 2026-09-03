// Filesystem helpers: atomic file writes and JSONL reading.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Atomically write `content` to `path` using a temp-file-then-rename strategy.
/// The temp file is placed in the same directory as `path` so the rename is
/// always on the same filesystem (guaranteed atomic on POSIX).
///
/// Temp-file naming: `.{millis}-{pid}.drip-tmp` — collision-safe across
/// concurrent processes and time.
///
/// * `path`    — destination file path.
/// * `content` — string content to write (caller controls trailing newline).
/// * `mkdir`   — create the parent directory (recursive) before writing.
pub fn write_file_atomic(path: &Path, content: &str, mkdir: bool) -> std::io::Result<()> {
    if mkdir {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
    }
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let temp_path: PathBuf = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(".{}-{}.drip-tmp", millis, std::process::id()));
    fs::write(&temp_path, content)?;
    fs::rename(&temp_path, path)
}

/// One entry per non-empty, complete line of a JSONL file. `parsed` is `None`
/// for lines that fail to parse; `raw` is always preserved.
#[derive(Debug, PartialEq)]
pub struct JsonlRecord<T> {
    pub parsed: Option<T>,
    pub raw: String,
}

/// Read a JSONL file and return one entry per non-empty, complete line.
///
/// - Missing file → returns [].
/// - "Torn tail" (content after the last newline with no terminating newline)
///   is ignored — only lines that end with `\n` are considered complete.
/// - Empty / whitespace-only lines are skipped.
/// - Lines that fail JSON.parse get `parsed: None`; `raw` is always preserved.
///
/// Callers apply their own policies by mapping over the returned entries.
pub fn read_jsonl_records<T: serde::de::DeserializeOwned>(path: &Path) -> Vec<JsonlRecord<T>> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        // Only a missing file reads as empty; any other IO error surfaces as
        // a panic.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => panic!("failed to read {}: {error}", path.display()),
    };

    // Only process complete lines (those before the last newline).
    let Some(last_newline) = content.rfind('\n') else {
        // No complete lines at all (entire content is a torn tail).
        return Vec::new();
    };

    let mut records = Vec::new();

    for line in content[..last_newline].split('\n') {
        if line.trim().is_empty() {
            continue;
        }
        let parsed = serde_json::from_str(line).ok();
        records.push(JsonlRecord {
            parsed,
            raw: line.to_string(),
        });
    }

    records
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Deserialize, PartialEq, Debug)]
    struct Rec {
        a: i64,
    }

    // --- writeFileAtomic ---

    #[test]
    fn round_trip_writes_content_and_reads_it_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("output.txt");
        write_file_atomic(&path, "hello world", false).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "hello world");
    }

    #[test]
    fn no_leftover_temp_files_after_successful_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("output.json");
        write_file_atomic(&path, r#"{"key":"value"}"#, false).unwrap();
        let entries: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        // Only the destination file should exist, no .drip-tmp files
        let tmp_files: Vec<_> = entries.iter().filter(|e| e.ends_with(".drip-tmp")).collect();
        assert!(tmp_files.is_empty());
        assert!(entries.contains(&"output.json".to_string()));
    }

    #[test]
    fn mkdir_option_creates_parent_directory_before_writing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("deep").join("output.txt");
        write_file_atomic(&path, "nested content", true).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "nested content");
    }

    #[test]
    fn without_mkdir_option_writing_to_missing_directory_throws() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent").join("output.txt");
        assert!(write_file_atomic(&path, "content", false).is_err());
    }

    #[test]
    fn preserves_content_exactly_caller_controls_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        // Without trailing newline
        let no_newline = dir.path().join("no-newline.txt");
        write_file_atomic(&no_newline, "abc", false).unwrap();
        assert_eq!(fs::read_to_string(&no_newline).unwrap(), "abc");

        // With trailing newline
        let with_newline = dir.path().join("with-newline.txt");
        write_file_atomic(&with_newline, "abc\n", false).unwrap();
        assert_eq!(fs::read_to_string(&with_newline).unwrap(), "abc\n");
    }

    // --- readJsonlRecords ---

    #[test]
    fn missing_file_returns_empty_vec() {
        let records: Vec<JsonlRecord<serde_json::Value>> =
            read_jsonl_records(Path::new("/nonexistent/path/that/does/not/exist.jsonl"));
        assert!(records.is_empty());
    }

    #[test]
    fn torn_tail_no_trailing_newline_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("torn.jsonl");
        // Two complete lines + torn tail (no trailing newline after third entry)
        fs::write(&path, r#"{"a":1}
{"b":2}
{"c":3}"#)
        .unwrap();
        let records = read_jsonl_records::<serde_json::Value>(&path);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].parsed.as_ref().unwrap()["a"], 1);
        assert_eq!(records[1].parsed.as_ref().unwrap()["b"], 2);
        // The torn tail {"c":3} must NOT appear
        assert!(!records.iter().any(|r| r.raw.contains("\"c\"")));
    }

    #[test]
    fn malformed_line_returns_parsed_none_with_raw_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("malformed.jsonl");
        fs::write(
            &path,
            "{\"good\":1}\nnot valid json\n{\"also_good\":2}\n",
        )
        .unwrap();
        let records = read_jsonl_records::<serde_json::Value>(&path);
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].parsed.as_ref().unwrap()["good"], 1);
        assert_eq!(records[1].parsed, None);
        assert_eq!(records[1].raw, "not valid json");
        assert_eq!(records[2].parsed.as_ref().unwrap()["also_good"], 2);
    }

    #[test]
    fn empty_lines_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty-lines.jsonl");
        fs::write(&path, "{\"a\":1}\n\n   \n{\"a\":2}\n").unwrap();
        let records = read_jsonl_records::<Rec>(&path);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].parsed, Some(Rec { a: 1 }));
        assert_eq!(records[1].parsed, Some(Rec { a: 2 }));
    }

    #[test]
    fn file_with_only_a_torn_tail_returns_empty_vec() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("only-torn.jsonl");
        fs::write(&path, r#"{"no":"newline"}"#).unwrap();
        let records = read_jsonl_records::<serde_json::Value>(&path);
        assert!(records.is_empty());
    }

    #[test]
    fn empty_file_returns_empty_vec() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.jsonl");
        fs::write(&path, "").unwrap();
        let records = read_jsonl_records::<serde_json::Value>(&path);
        assert!(records.is_empty());
    }
}
