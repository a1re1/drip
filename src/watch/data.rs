// The pure data side of dripw: session classification, directory filtering,
// paging, transcript windowing, and the incremental transcript tail.

use std::path::{Path, PathBuf};

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
pub fn classify_sessions(
    records: &[SessionRecord],
    is_running: &dyn Fn(&SessionRecord) -> bool,
) -> ClassifiedSessions {
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
// directory filter
// ---------------------------------------------------------------------------

// Canonicalize a directory path when it exists on disk; otherwise fall back to
// resolve(), which makes the path absolute and normalizes separators and
// `.`/`..` segments. Session cwds and the monitor's launch directory are
// handled identically so symlink aliases and spelling differences cannot
// split one directory into two.
pub fn canonical_dir(path: &str) -> PathBuf {
    let resolved_path = resolve(path);
    match std::fs::canonicalize(&resolved_path) {
        Ok(canonical) => canonical,
        Err(_) => resolved_path,
    }
}

// The one visibility rule: keep a session iff its recorded cwd equals the
// watched directory or lives anywhere beneath it. The prefix test is
// path-component based — a watched dir `/a/b` must not claim a sibling like
// `/a/bc` — which Path::starts_with guarantees by comparing whole components
// instead of characters. Both sides are canonicalized first, so symlink
// aliases and trailing slashes cannot defeat the comparison. A session with
// an empty/unresolvable cwd is dropped rather than silently adopted into the
// watched directory. Pure on purpose: like classify_sessions, it stays
// testable without a real session tree.
pub fn sessions_under_dir(records: &[SessionRecord], dir: &str) -> Vec<SessionRecord> {
    if dir.trim().is_empty() {
        return Vec::new();
    }
    let root = canonical_dir(dir);

    records
        .iter()
        .filter(|record| {
            if record.cwd.trim().is_empty() {
                return false;
            }
            let cwd = canonical_dir(&record.cwd);
            cwd.starts_with(&root)
        })
        .cloned()
        .collect()
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

    PaginateResult {
        page: clamped,
        page_count,
        page_items: items[start.min(items.len())..end].to_vec(),
    }
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
pub fn with_newest_model_entry(
    all: &[TranscriptEntry],
    window: &[TranscriptEntry],
) -> Vec<TranscriptEntry> {
    if window
        .iter()
        .any(|entry| matches!(entry, TranscriptEntry::Model(_)))
    {
        return window.to_vec();
    }

    if let Some(entry) = all
        .iter()
        .rev()
        .find(|entry| matches!(entry, TranscriptEntry::Model(_)))
    {
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
            pending_replay =
                with_newest_model_entry(&all, &all[all.len().saturating_sub(replay_limit)..]);

            // We do not advance the byte offset from read_transcript because
            // read_transcript is record-based, not byte-based. Instead we grab
            // the current file size via read_appended_jsonl_lines with offset 0
            // to position ourselves at the true end of the file so the next
            // poll reads only appended content.
            offset = read_appended_jsonl_lines(path, 0).next_offset;
        }

        TranscriptTail {
            transcript_path: transcript_path.to_string(),
            offset,
            seeded: false,
            pending_replay,
        }
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

        appended
            .lines
            .iter()
            .filter_map(|line| parse_transcript_line(line))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// session tree
// ---------------------------------------------------------------------------

/// One rendered Sessions row: the record plus the box-drawing connector that
/// places it under its parent. Roots carry an empty prefix.
#[derive(Debug, Clone)]
pub struct SessionTreeRow {
    pub record: SessionRecord,
    pub prefix: String,
}

// The connector for a node at `depth` whose ancestors' "has a later sibling"
// flags are `ancestor_later` (nearest ancestor last). The node's own line is
// the last element and gets the elbow; the ancestor columns precede it, and
// only an ancestor with later siblings keeps a vertical `│` below its elbow.
fn tree_prefix(ancestor_later: &[bool], last: bool) -> String {
    let mut out = String::new();
    for &later in ancestor_later.iter().rev() {
        out.push_str(if later { "│  " } else { "   " });
    }
    out.push_str(if last { "└─ " } else { "├─ " });
    out
}

/// Flatten `records` into renderable rows: each root (parent_id absent, or a
/// parent outside this slice — dripw only ever holds one project's sessions,
/// so a parent in another project reads as an orphan) keeps its input position
/// and is followed depth-first by its descendants. Input order is newest-first
/// and is preserved at every level, so children stay newest-first too.
///
/// The parent indices are looked up by id in this slice, and a node whose
/// parent chain loops (corrupt data) is dropped from its parent's child list
/// and rendered as a root instead — every record appears exactly once, and
/// nothing recurses forever.
pub fn tree_rows(records: &[SessionRecord]) -> Vec<SessionTreeRow> {
    let index: std::collections::HashMap<&str, usize> = records
        .iter()
        .enumerate()
        .map(|(i, r)| (r.id.as_str(), i))
        .collect();
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); records.len()];
    let mut roots: Vec<usize> = Vec::new();

    for (i, r) in records.iter().enumerate() {
        let parent = r.parent_id.as_deref().and_then(|id| index.get(id).copied());
        match parent {
            // A self-parent is the smallest cycle; treat it like any other.
            Some(p) if p != i && !reaches_self(records, &index, p, i) => children[p].push(i),
            // A cycle is unrenderable as a subtree: the node becomes a root so
            // the row still appears exactly once.
            Some(_) => roots.push(i),
            None => roots.push(i),
        }
    }

    // `children` is acyclic by construction (an edge is only added when the
    // parent's chain does not lead back to the child), and every node is a root
    // or hangs off exactly one parent edge, so the root sweep visits each
    // record exactly once.
    let mut rows: Vec<SessionTreeRow> = Vec::with_capacity(records.len());
    for &root in &roots {
        push_subtree(
            records,
            &children,
            root,
            true,
            &mut Vec::new(),
            true,
            &mut rows,
        );
    }
    debug_assert_eq!(
        rows.len(),
        records.len(),
        "every record renders exactly once"
    );
    rows
}

// True when the parent chain from `start` reaches `target`, which is how a
// would-be child discovers that attaching it would close a loop. A chain that
// loops without touching `target` also answers true: a node hanging off a
// cycle renders as a root rather than under an unrenderable subtree.
fn reaches_self(
    records: &[SessionRecord],
    index: &std::collections::HashMap<&str, usize>,
    start: usize,
    target: usize,
) -> bool {
    let mut cursor = start;
    let mut steps = 0;
    while let Some(parent) = records[cursor]
        .parent_id
        .as_deref()
        .and_then(|id| index.get(id).copied())
    {
        if parent == target {
            return true;
        }
        cursor = parent;
        steps += 1;
        if steps > records.len() {
            return true;
        }
    }
    false
}

// Emit `node` and its descendants. `ancestor_later` keeps one flag per
// ancestor column above this node (nearest ancestor last), excluding roots: a
// root draws no column of its own, so depth-1 rows are just their own elbow.
fn push_subtree(
    records: &[SessionRecord],
    children: &[Vec<usize>],
    node: usize,
    is_root: bool,
    ancestor_later: &mut Vec<bool>,
    last: bool,
    rows: &mut Vec<SessionTreeRow>,
) {
    let prefix = if is_root {
        String::new()
    } else {
        tree_prefix(ancestor_later, last)
    };
    rows.push(SessionTreeRow {
        record: records[node].clone(),
        prefix,
    });
    let kids = &children[node];
    for (position, &child) in kids.iter().enumerate() {
        let child_last = position + 1 == kids.len();
        // This node contributes the deepest column for its own children when it
        // is not a root: descendants of an ancestor that still has later
        // siblings keep that ancestor's vertical line.
        if !is_root {
            ancestor_later.push(!last);
        }
        push_subtree(
            records,
            &children,
            child,
            false,
            ancestor_later,
            child_last,
            rows,
        );
        if !is_root {
            ancestor_later.pop();
        }
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
            parent_id: None,
            project_slug: "p".to_string(),
            status: "idle".to_string(),
            updated_at: updated_at.to_string(),
        }
    }

    fn model_line(model: &str) -> String {
        format!(
            r#"{{"at":"t","goalId":"g","model":"{model}","profileId":"p","provider":"openai","type":"model"}}"#
        )
    }

    #[test]
    fn classify_splits_and_sorts_newest_first() {
        let records = vec![
            record("a", "/r", "2026-01-01T00:00:01Z"),
            record("b", "/r", "2026-01-01T00:00:03Z"),
            record("c", "/r", "2026-01-01T00:00:02Z"),
        ];
        let classified = classify_sessions(&records, &|r| r.id == "a");

        assert_eq!(
            classified
                .running
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            ["a"]
        );
        assert_eq!(
            classified
                .recent
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            ["b", "c"]
        );
    }

    #[test]
    fn dir_filter_keeps_equal_and_child_dirs_only() {
        let records = vec![
            record("equal", "/repo/abc", "t"),
            record("child", "/repo/abc/src", "t"),
            record("deep", "/repo/abc/src/deep/x", "t"),
            record("sibling", "/repo/abc123", "t"),
            record("prefix_prefix", "/repo/abc/srcx", "t"),
            record("parent", "/repo", "t"),
            record("other", "/other", "t"),
            record("empty_cwd", "", "t"),
        ];
        let kept = sessions_under_dir(&records, "/repo/abc");

        assert_eq!(
            kept.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["equal", "child", "deep", "prefix_prefix"]
        );
    }

    #[test]
    fn dir_filter_component_prefix_not_string_prefix() {
        // The watched dir must not claim a sibling whose name merely extends
        // its own last component as a string.
        let records = vec![record("bc", "/a/bc", "t"), record("b2", "/a/b2/c", "t")];

        assert!(sessions_under_dir(&records, "/a/b").is_empty());
        assert_eq!(
            sessions_under_dir(&records, "/a/bc")
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            ["bc"]
        );
    }

    #[test]
    fn dir_filter_trailing_slashes_and_dot_segments_equivalent() {
        let records = vec![
            record("a", "/repo/abc/", "t"),
            record("b", "/repo/abc/./src", "t"),
            record("c", "/repo/abc/../abc", "t"),
        ];

        let kept = sessions_under_dir(&records, "/repo/abc///");
        assert_eq!(
            kept.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
    }

    #[test]
    fn dir_filter_existing_dirs_canonicalize_and_missing_dirs_fall_back() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::create_dir_all(real.join("sub")).unwrap();
        let real_str = real.to_string_lossy().into_owned();

        // Existing directory: a `.`/`..` spelling of it canonicalizes equal.
        let records = vec![record("in", &format!("{real_str}/sub"), "t")];
        let spelled = format!("{}/./", dir.path().join(".").to_string_lossy());
        assert_eq!(sessions_under_dir(&records, &spelled).len(), 1);

        // Missing directory (never created): falls back to the resolved path
        // and still does component-prefix containment.
        let missing = format!("{}/missing", dir.path().to_string_lossy());
        let records = vec![
            record("under", &format!("{missing}/x"), "t"),
            record("outside", "/elsewhere", "t"),
        ];
        let kept = sessions_under_dir(&records, &missing);
        assert_eq!(
            kept.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["under"]
        );
    }

    #[test]
    fn dir_filter_symlink_aliases_agree_on_both_sides() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        #[cfg(unix)]
        {
            let link = dir.path().join("link");
            std::os::unix::fs::symlink(&real, &link).unwrap();

            // Alias on the launch-dir side.
            let records = vec![
                record("in", &real.to_string_lossy(), "t"),
                record("out", "/elsewhere", "t"),
            ];
            let kept = sessions_under_dir(&records, &link.to_string_lossy());
            assert_eq!(
                kept.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
                ["in"]
            );

            // Alias on the record side.
            let records = vec![
                record("in", &link.to_string_lossy(), "t"),
                record("out", "/elsewhere", "t"),
            ];
            let kept = sessions_under_dir(&records, &real.to_string_lossy());
            assert_eq!(
                kept.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
                ["in"]
            );
        }
    }

    #[test]
    fn dir_filter_parent_sees_nested_checkout_child_excludes_parent() {
        // One physical tree: outer/…/inner, plus a sibling. A monitor in the
        // parent sees the nested checkout's sessions; a monitor in the nested
        // checkout sees neither the parent's nor the sibling's.
        let dir = tempfile::tempdir().unwrap();
        let outer = dir.path().join("outer");
        let inner = outer.join("dep/inner");
        let sibling = outer.join("sib");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        let (outer_str, inner_str, sibling_str) = (
            outer.to_string_lossy().into_owned(),
            inner.to_string_lossy().into_owned(),
            sibling.to_string_lossy().into_owned(),
        );

        let records = vec![
            record("inner", &inner_str, "t"),
            record("sibling", &sibling_str, "t"),
            record("elsewhere", "/elsewhere", "t"),
        ];

        let from_outer = sessions_under_dir(&records, &outer_str);
        assert_eq!(
            from_outer.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["inner", "sibling"]
        );

        let from_inner = sessions_under_dir(&records, &inner_str);
        assert_eq!(
            from_inner.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["inner"]
        );
    }

    #[test]
    fn dir_filter_empty_launch_dir_keeps_nothing() {
        let records = vec![record("a", "/repo/abc", "t")];
        assert!(sessions_under_dir(&records, "").is_empty());
    }

    #[test]
    fn paginate_clamps_and_reports_page_count() {
        let items: Vec<i32> = (0..10).collect();

        assert_eq!(
            paginate(&items, 5, 4),
            PaginateResult {
                page: 2,
                page_count: 3,
                page_items: vec![8, 9]
            }
        );
        assert_eq!(paginate(&items, 0, 4).page_items, vec![0, 1, 2, 3]);
        assert_eq!(
            paginate::<i32>(&[], 3, 4),
            PaginateResult {
                page: 0,
                page_count: 1,
                page_items: vec![]
            }
        );
    }

    #[test]
    fn trim_keeps_last_entries_and_pins_the_model_entry() {
        let entries: Vec<TranscriptEntry> = [
            model_line("m"),
            r#"{"at":"t","text":"a","type":"info"}"#.to_string(),
            r#"{"at":"t","text":"b","type":"info"}"#.to_string(),
            r#"{"at":"t","text":"c","type":"info"}"#.to_string(),
        ]
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

        std::fs::write(
            &path,
            format!(
                "{}\n{}\n{}\n",
                model_line("m1"),
                model_line("m2"),
                model_line("m3")
            ),
        )
        .unwrap();
        let mut tail = TranscriptTail::new(&path_str, 2);
        let replay = tail.poll();
        assert_eq!(replay.len(), 2);
        assert!(matches!(&replay[1], TranscriptEntry::Model(m) if m.model == "m3"));
        assert!(tail.poll().is_empty());

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file, r#"{{"at":"t","text":"new","type":"info"}}"#).unwrap();
        let added = tail.poll();
        assert_eq!(added.len(), 1);
        assert!(matches!(&added[0], TranscriptEntry::Info(note) if note.text == "new"));
    }
}

#[cfg(test)]
mod tree_tests {
    use super::*;

    fn record(id: &str, parent_id: Option<&str>) -> SessionRecord {
        SessionRecord {
            sessions_dir: None,
            created_at: "2026-01-01T00:00:00.000Z".into(),
            cwd: "/r".into(),
            goal_count: 0,
            id: id.into(),
            last_goal: None,
            parent_id: parent_id.map(str::to_string),
            project_slug: "p".into(),
            status: "idle".into(),
            updated_at: "2026-01-01T00:00:00.000Z".into(),
        }
    }

    // (id, expected prefix) pairs, in the order tree_rows must emit them.
    fn shaped(rows: &[SessionTreeRow]) -> Vec<(String, String)> {
        rows.iter()
            .map(|row| (row.record.id.clone(), row.prefix.clone()))
            .collect()
    }

    #[test]
    fn a_flat_list_is_unchanged_with_empty_prefixes() {
        let records = vec![record("c", None), record("b", None), record("a", None)];
        let rows = tree_rows(&records);
        assert_eq!(
            shaped(&rows),
            vec![
                ("c".to_string(), String::new()),
                ("b".to_string(), String::new()),
                ("a".to_string(), String::new()),
            ]
        );
    }

    #[test]
    fn one_parent_with_two_children_gets_an_elbow_then_a_last_elbow() {
        let records = vec![
            record("p", None),
            record("c1", Some("p")),
            record("c2", Some("p")),
        ];
        let rows = tree_rows(&records);
        assert_eq!(
            shaped(&rows),
            vec![
                ("p".to_string(), String::new()),
                ("c1".to_string(), "├─ ".to_string()),
                ("c2".to_string(), "└─ ".to_string()),
            ]
        );
    }

    #[test]
    fn a_grandchild_under_the_first_of_two_children_keeps_the_vertical_line() {
        let records = vec![
            record("p", None),
            record("c1", Some("p")),
            record("g", Some("c1")),
            record("c2", Some("p")),
        ];
        let rows = tree_rows(&records);
        assert_eq!(
            shaped(&rows),
            vec![
                ("p".to_string(), String::new()),
                ("c1".to_string(), "├─ ".to_string()),
                ("g".to_string(), "│  └─ ".to_string()),
                ("c2".to_string(), "└─ ".to_string()),
            ]
        );
    }

    #[test]
    fn an_orphan_whose_parent_is_absent_renders_as_a_root() {
        let records = vec![record("solo", Some("missing")), record("root", None)];
        let rows = tree_rows(&records);
        assert_eq!(
            shaped(&rows),
            vec![
                ("solo".to_string(), String::new()),
                ("root".to_string(), String::new())
            ]
        );
    }

    #[test]
    fn a_two_record_cycle_renders_both_records_exactly_once() {
        let records = vec![record("a", Some("b")), record("b", Some("a"))];
        let rows = tree_rows(&records);
        let shape = shaped(&rows);
        assert_eq!(shape.len(), 2, "both records render: {shape:?}");
        // Neither node can be a descendant of the other, so both are broken
        // out as roots — in input order, each exactly once.
        assert_eq!(
            shape,
            vec![
                ("a".to_string(), String::new()),
                ("b".to_string(), String::new())
            ]
        );
    }

    #[test]
    fn a_self_parent_renders_once_as_a_root() {
        let rows = tree_rows(&[record("loop", Some("loop"))]);
        assert_eq!(shaped(&rows), vec![("loop".to_string(), String::new())]);
    }

    #[test]
    fn a_cycle_reached_through_a_valid_parent_is_broken_not_dropped() {
        // c -> b -> a -> c: the root sweep never enters the loop, so a and b
        // are emitted by the safety net; every record still appears once.
        let records = vec![
            record("x", None),
            record("a", Some("c")),
            record("b", Some("a")),
            record("c", Some("b")),
        ];
        let rows = tree_rows(&records);
        let mut ids: Vec<String> = rows.iter().map(|row| row.record.id.clone()).collect();
        assert_eq!(ids.len(), 4);
        ids.sort();
        assert_eq!(ids, vec!["a", "b", "c", "x"]);
    }

    #[test]
    fn deep_chains_keep_one_column_per_ancestor_with_later_siblings() {
        // Two roots, each with a 3-deep chain: the ancestors that are not their
        // parent's last child contribute a `│  ` column.
        let records = vec![
            record("p1", None),
            record("p2", None),
            record("a", Some("p2")),
            record("b", Some("a")),
            record("d", Some("b")),
            record("c", Some("p1")),
        ];
        let rows = tree_rows(&records);
        assert_eq!(
            shaped(&rows),
            vec![
                ("p1".to_string(), String::new()),
                ("c".to_string(), "└─ ".to_string()),
                ("p2".to_string(), String::new()),
                ("a".to_string(), "└─ ".to_string()),
                ("b".to_string(), "   └─ ".to_string()),
                ("d".to_string(), "      └─ ".to_string()),
            ]
        );
    }
}
