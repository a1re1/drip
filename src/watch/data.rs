// port of src/cli/watch/data.ts
//
// The pure data side of dripw: session classification and scoping, paging,
// transcript windowing, and the incremental transcript tail.

use std::path::{Path, MAIN_SEPARATOR};

use crate::cli::follow::{parse_transcript_line, read_appended_jsonl_lines};
use crate::cli::transcript::{read_transcript, TranscriptEntry};
use crate::core::home::resolve;
use crate::core::sessions::SessionRecord;

// ---------------------------------------------------------------------------
// classify_sessions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct ClassifiedSessions {
    pub running: Vec<SessionRecord>,
    pub recent: Vec<SessionRecord>,
}

// Split session records into live-lease vs all-others, each sorted by
// updated_at descending. The caller provides is_running so this helper stays
// pure and independently testable without a real lease file.
pub fn classify_sessions(records: &[SessionRecord], is_running: &dyn Fn(&SessionRecord) -> bool) -> ClassifiedSessions {
    let mut running: Vec<SessionRecord> = Vec::new();
    let mut recent: Vec<SessionRecord> = Vec::new();

    for record in records {
        if is_running(record) {
            running.push(record.clone());
        } else {
            recent.push(record.clone());
        }
    }

    // Array.sort with localeCompare on ASCII timestamps: a stable byte order.
    running.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    recent.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));

    ClassifiedSessions { running, recent }
}

// ---------------------------------------------------------------------------
// scope_sessions
// ---------------------------------------------------------------------------

pub struct ScopeOptions<'a> {
    pub worktree_root: &'a str,
    pub linked_worktree_roots: &'a [String],
}

fn resolved(path: &str) -> String {
    resolve(path).to_string_lossy().into_owned()
}

// Restrict the Recent list to sessions started inside the worktree being
// watched. Two subtleties the path check has to get right. First, containment
// is segment-based: worktree `…/abc` must not swallow `…/abc123`, which a bare
// string prefix would. Second, the exclusion of other linked roots only fires
// for roots nested INSIDE the watched worktree — the main checkout keeps its
// linked worktrees under `.worktrees/`, so its root always contains the
// worktree's own sessions and excluding on it would empty the worktree view,
// while ignoring it entirely would let a repo-scoped view re-absorb every
// worktree's sessions. Net effect: the innermost listed root owns a cwd.
// Pure on purpose: like classify_sessions, it stays testable without a tree.
pub fn scope_sessions(records: &[SessionRecord], opts: ScopeOptions<'_>) -> Vec<SessionRecord> {
    let root = resolved(opts.worktree_root);

    let is_inside = |dir: &str, base: &str| -> bool {
        let d = resolved(dir);

        d == base || d.starts_with(&format!("{base}{MAIN_SEPARATOR}"))
    };

    // Linked worktrees sitting under the watched worktree keep their own
    // sessions; roots elsewhere (the main checkout, sibling worktrees) cannot
    // claim anything that lives under `root` anyway.
    let nested_roots: Vec<String> =
        opts.linked_worktree_roots.iter().map(|r| resolved(r)).filter(|r| r != &root && is_inside(r, &root)).collect();

    let mut kept: Vec<SessionRecord> = Vec::new();

    for record in records {
        let cwd = resolved(&record.cwd);

        // Started outside this worktree entirely — not ours.
        if !is_inside(&cwd, &root) {
            continue;
        }
        // But a worktree nested inside us owns its own sessions.
        if nested_roots.iter().any(|r| is_inside(&cwd, r)) {
            continue;
        }

        kept.push(record.clone());
    }

    kept
}

// ---------------------------------------------------------------------------
// paginate
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaginateResult<T> {
    pub page: usize,
    pub page_count: usize,
    pub page_items: Vec<T>,
}

// Slice items into a page window. Page is 0-based and clamped into range.
// page_count is at least 1 even for an empty list so the UI never divides
// by zero.
pub fn paginate<T: Clone>(items: &[T], page: usize, page_size: usize) -> PaginateResult<T> {
    let page_size = page_size.max(1);
    let page_count = items.len().div_ceil(page_size).max(1);
    let clamped = page.min(page_count - 1);
    let start = clamped * page_size;
    let end = (start + page_size).min(items.len());

    PaginateResult { page: clamped, page_count, page_items: items[start.min(items.len())..end].to_vec() }
}

// ---------------------------------------------------------------------------
// transcript window helpers
// ---------------------------------------------------------------------------

// Put the newest "model" entry back at the front of a windowed transcript when
// the window dropped it. The route entry is written once per goal at run
// start, so on a long run it falls out of both the replay window and the
// retention cap — exactly the runs whose model is most worth naming. `all` is
// the full entry list the window came from; when it contains no route entry
// (transcripts written before routes were recorded) the window is returned
// unchanged.
pub fn with_newest_model_entry(all: &[TranscriptEntry], window: &[TranscriptEntry]) -> Vec<TranscriptEntry> {
    if window.iter().any(|entry| matches!(entry, TranscriptEntry::Model(_))) {
        return window.to_vec();
    }

    if let Some(entry) = all.iter().rev().find(|entry| matches!(entry, TranscriptEntry::Model(_))) {
        let mut out = Vec::with_capacity(window.len() + 1);
        out.push(entry.clone());
        out.extend_from_slice(window);
        return out;
    }

    window.to_vec()
}

// Cap retained entries at `max` without losing the run's route line.
pub fn trim_transcript(entries: &[TranscriptEntry], max: usize) -> Vec<TranscriptEntry> {
    if entries.len() <= max {
        return entries.to_vec();
    }

    with_newest_model_entry(entries, &entries[entries.len() - max..])
}

// ---------------------------------------------------------------------------
// TranscriptTail
// ---------------------------------------------------------------------------

// A stateful incremental reader for a session transcript. On first poll it
// replays the last `replay_limit` entries from the existing file (via
// read_transcript), then on subsequent polls returns only new complete lines
// appended since (via read_appended_jsonl_lines + parse_transcript_line). If
// the file does not exist yet both the initial replay and subsequent polls
// return empty vectors gracefully. read_appended_jsonl_lines already handles
// the case where the file is truncated or rewritten between polls (it resets
// the offset to 0).
pub struct TranscriptTail {
    transcript_path: String,
    offset: u64,
    seeded: bool,
    // The initial replay entries are returned on the very first poll call so
    // the app can render immediately without a second async step.
    pending_replay: Vec<TranscriptEntry>,
}

impl TranscriptTail {
    pub fn new(transcript_path: &str, replay_limit: usize) -> TranscriptTail {
        let mut offset = 0u64;
        let mut pending_replay: Vec<TranscriptEntry> = Vec::new();
        let path = Path::new(transcript_path);

        if path.exists() {
            let all = read_transcript(path);
            pending_replay = with_newest_model_entry(&all, &all[all.len().saturating_sub(replay_limit)..]);

            // We do not advance the byte offset from read_transcript because
            // read_transcript is record-based, not byte-based. Instead we grab
            // the current file size via read_appended_jsonl_lines with offset 0
            // to position ourselves at the true end of the file so the next
            // poll reads only appended content.
            offset = read_appended_jsonl_lines(path, 0).next_offset;
        }

        TranscriptTail { transcript_path: transcript_path.to_string(), offset, seeded: false, pending_replay }
    }

    /// Returns any newly available complete entries since the last poll.
    pub fn poll(&mut self) -> Vec<TranscriptEntry> {
        // First call: return the replayed tail synchronously.
        if !self.seeded {
            self.seeded = true;

            return std::mem::take(&mut self.pending_replay);
        }

        // Subsequent calls: read only newly appended lines.
        let path = Path::new(&self.transcript_path);

        if !path.exists() {
            return Vec::new();
        }

        let appended = read_appended_jsonl_lines(path, self.offset);

        self.offset = appended.next_offset;

        appended.lines.iter().filter_map(|line| parse_transcript_line(line)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn record(id: &str, cwd: &str, updated_at: &str) -> SessionRecord {
        SessionRecord {
            sessions_dir: None,
            created_at: updated_at.to_string(),
            cwd: cwd.to_string(),
            goal_count: 0,
            id: id.to_string(),
            last_goal: None,
            project_slug: "p".to_string(),
            status: "idle".to_string(),
            updated_at: updated_at.to_string(),
        }
    }

    fn model_line(model: &str) -> String {
        format!(r#"{{"at":"t","goalId":"g","model":"{model}","profileId":"p","provider":"openai","type":"model"}}"#)
    }

    #[test]
    fn classify_splits_and_sorts_newest_first() {
        let records = vec![record("a", "/r", "2026-01-01T00:00:01Z"), record("b", "/r", "2026-01-01T00:00:03Z"), record("c", "/r", "2026-01-01T00:00:02Z")];
        let classified = classify_sessions(&records, &|r| r.id == "a");

        assert_eq!(classified.running.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["a"]);
        assert_eq!(classified.recent.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["b", "c"]);
    }

    #[test]
    fn scope_uses_segment_containment_and_innermost_root() {
        let records = vec![
            record("own", "/repo/abc/src", "t"),
            record("root", "/repo/abc", "t"),
            record("sibling", "/repo/abc123", "t"),
            record("nested", "/repo/abc/.worktrees/x/src", "t"),
            record("elsewhere", "/other", "t"),
        ];
        let linked = vec!["/repo".to_string(), "/repo/abc/.worktrees/x".to_string()];
        let kept = scope_sessions(&records, ScopeOptions { worktree_root: "/repo/abc", linked_worktree_roots: &linked });

        assert_eq!(kept.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["own", "root"]);
    }

    #[test]
    fn paginate_clamps_and_reports_page_count() {
        let items: Vec<i32> = (0..10).collect();

        assert_eq!(paginate(&items, 5, 4), PaginateResult { page: 2, page_count: 3, page_items: vec![8, 9] });
        assert_eq!(paginate(&items, 0, 4).page_items, vec![0, 1, 2, 3]);
        assert_eq!(paginate::<i32>(&[], 3, 4), PaginateResult { page: 0, page_count: 1, page_items: vec![] });
    }

    #[test]
    fn trim_keeps_last_entries_and_pins_the_model_entry() {
        let entries: Vec<TranscriptEntry> = [model_line("m"), r#"{"at":"t","text":"a","type":"info"}"#.to_string(), r#"{"at":"t","text":"b","type":"info"}"#.to_string(), r#"{"at":"t","text":"c","type":"info"}"#.to_string()]
            .iter()
            .map(|line| parse_transcript_line(line).unwrap())
            .collect();
        let trimmed = trim_transcript(&entries, 2);

        assert_eq!(trimmed.len(), 3);
        assert!(matches!(trimmed[0], TranscriptEntry::Model(_)));
        assert!(matches!(&trimmed[2], TranscriptEntry::Info(note) if note.text == "c"));
        assert_eq!(trim_transcript(&entries, 10).len(), 4);
    }

    #[test]
    fn tail_replays_then_returns_only_appended_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript.jsonl");
        let path_str = path.to_string_lossy().into_owned();

        let mut missing = TranscriptTail::new(&path_str, 2);
        assert!(missing.poll().is_empty());
        assert!(missing.poll().is_empty());

        std::fs::write(&path, format!("{}\n{}\n{}\n", model_line("m1"), model_line("m2"), model_line("m3"))).unwrap();
        let mut tail = TranscriptTail::new(&path_str, 2);
        let replay = tail.poll();
        assert_eq!(replay.len(), 2);
        assert!(matches!(&replay[1], TranscriptEntry::Model(m) if m.model == "m3"));
        assert!(tail.poll().is_empty());

        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, r#"{{"at":"t","text":"new","type":"info"}}"#).unwrap();
        let added = tail.poll();
        assert_eq!(added.len(), 1);
        assert!(matches!(&added[0], TranscriptEntry::Info(note) if note.text == "new"));
    }
}
