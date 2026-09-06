// Transcript entries → row cells for the dripw [0] pane.
//
// The pane used to print every event as `[iter] label <detail flattened to one
// blob>`, which turned tool calls into raw JSON and tool results into 2000-char
// dumps. This module condenses each kind to what a watcher needs at a glance:
// a one-line per-tool summary for calls, the tool's own first line plus a short
// body preview for results, terminal markdown for prose blocks (goal, model
// text, run summary, steering), and a divider per cycle so the rows group.
//
// Pure: no clock, no I/O. Plain rows leave clipping to render_pane (which fits
// them with an ellipsis); rich rows carry their own ANSI and are pre-wrapped
// here so they never exceed `inner_w`.

use std::sync::OnceLock;

use regex::Regex;
use serde_json::{Map, Value};

use crate::cli::transcript::{format_model_route_lines, TranscriptEntry, TranscriptEventEntry};
use crate::core::types::HarnessEventType;
use crate::tui::markdown_ansi::render_markdown_ansi;
use crate::tui::theme::event_label;
use crate::watch::ansi::{c, color_enabled, strip_ansi, wrap_ansi};

/// One rendered cell of a pane row.
#[derive(Debug, Clone, Default)]
pub struct RowCell {
    /// Plain text (no ANSI). Colored after fit inside render_pane.
    pub text: String,
    pub color: Option<fn(&str) -> String>,
    pub selected: bool,
    /// Carries pre-styled ANSI (e.g. terminal-markdown). Passed through
    /// render_pane verbatim (padded to inner_w) instead of fit/color.
    pub rich: bool,
}

impl RowCell {
    fn plain(text: impl Into<String>, color: fn(&str) -> String) -> Self {
        Self { text: text.into(), color: Some(color), selected: false, rich: false }
    }
}

// ── Constants ────────────────────────────────────────────────────────────────

/// Body lines shown under a tool-result header before "… +N lines".
pub const RESULT_PREVIEW_LINES: usize = 4;
/// Rendered markdown lines kept for a goal before "… +N lines".
pub const GOAL_PREVIEW_LINES: usize = 8;
/// Generic key=value summaries cut each value here.
const MAX_ARG_VALUE_CHARS: usize = 60;
const BODY_INDENT: &str = "    ";

fn elision_marker() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\[\.\.\. \d+ chars elided \.\.\.\]$").unwrap())
}

type Args = Map<String, Value>;

// ── Small helpers ────────────────────────────────────────────────────────────

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn cut(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        text.to_string()
    } else {
        format!("{}…", chars[..max.saturating_sub(1)].iter().collect::<String>())
    }
}

fn event_color(kind: HarnessEventType) -> fn(&str) -> String {
    crate::tui::theme::event_paint(kind)
}

/// `[  2] label  ` — the shared row prefix for events.
fn prefix(entry: &TranscriptEventEntry) -> String {
    format!("[{:>3}] {:<7}", entry.iteration, event_label(entry.kind))
}

pub fn fmt_ms(ms: Option<i64>) -> Option<String> {
    let ms = ms?;
    Some(if ms < 1000 { format!("{ms}ms") } else { format!("{:.1}s", ms as f64 / 1000.0) })
}

pub fn fmt_tokens(n: i64) -> String {
    if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

fn parse_args(raw: &str) -> Option<Args> {
    match serde_json::from_str::<Value>(raw) {
        Ok(Value::Object(map)) => Some(map),
        _ => None,
    }
}

fn str_arg<'a>(args: &'a Args, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn num_arg(args: &Args, key: &str) -> Option<f64> {
    match args.get(key) {
        Some(Value::Number(n)) => n.as_f64().filter(|v| v.is_finite()),
        Some(Value::String(s)) if !s.trim().is_empty() => s.trim().parse::<f64>().ok().filter(|v| v.is_finite()),
        _ => None,
    }
}

/// JS `String(number)`: integral values print without a fraction.
fn js_num(n: f64) -> String {
    if n.fract() == 0.0 {
        format!("{}", n as i64)
    } else {
        n.to_string()
    }
}

// Blank rows separate blocks; never stack two, never lead the pane.
fn push_blank(rows: &mut Vec<RowCell>) {
    match rows.last() {
        None => {}
        Some(last) if last.text.is_empty() => {}
        Some(_) => rows.push(RowCell::plain("", c::dim)),
    }
}

fn push_wrapped(rows: &mut Vec<RowCell>, text: &str, inner_w: usize, color: fn(&str) -> String) {
    for line in wrap_ansi(text, inner_w.max(1)) {
        rows.push(RowCell::plain(line, color));
    }
}

// ── Tool calls ───────────────────────────────────────────────────────────────

fn fmt_arg_value(value: &Value) -> String {
    match value {
        Value::String(s) => {
            let flat = collapse(s);
            cut(&if flat.chars().any(char::is_whitespace) { format!("\"{flat}\"") } else { flat }, MAX_ARG_VALUE_CHARS)
        }
        Value::Null => cut("null", MAX_ARG_VALUE_CHARS),
        Value::Bool(b) => cut(&b.to_string(), MAX_ARG_VALUE_CHARS),
        Value::Number(n) => cut(&n.as_f64().map(js_num).unwrap_or_else(|| n.to_string()), MAX_ARG_VALUE_CHARS),
        other => cut(&collapse(&other.to_string()), MAX_ARG_VALUE_CHARS),
    }
}

// PATCH takes either one {path, find, replace | content} or {files: [...]}.
fn patch_targets(args: &Args) -> (Vec<String>, bool) {
    let single = Value::Object(args.clone());
    let files: Vec<&Value> = match args.get("files") {
        Some(Value::Array(files)) => files.iter().collect(),
        _ => vec![&single],
    };
    let mut paths: Vec<String> = Vec::new();
    let mut write = false;
    for file in files {
        let Some(f) = file.as_object() else { continue };
        if let Some(path) = str_arg(f, "path") {
            if !path.is_empty() && !paths.iter().any(|p| p == path) {
                paths.push(path.to_string());
            }
        }
        if f.get("content").is_some_and(Value::is_string) {
            write = true;
        }
    }
    (paths, write)
}

/// One-line, per-tool summary of a tool call's arguments. Unparseable args
/// fall back to the whitespace-collapsed raw string; unknown tools print
/// `key=value` pairs with long values cut.
pub fn summarize_tool_call(tool_name: &str, raw_args: &str) -> String {
    let Some(args) = parse_args(raw_args) else {
        return format!("{tool_name} {}", collapse(raw_args)).trim().to_string();
    };

    let known = match tool_name {
        "READ" => str_arg(&args, "path").filter(|p| !p.is_empty()).map(|path| {
            let offset = num_arg(&args, "offset");
            let limit = num_arg(&args, "limit");
            let span = if offset.is_some() || limit.is_some() {
                format!(
                    ":{}{}",
                    offset.map(js_num).unwrap_or_else(|| "1".to_string()),
                    limit.map(|l| format!("+{}", js_num(l))).unwrap_or_default()
                )
            } else {
                String::new()
            };
            format!("READ {path}{span}")
        }),
        "GREP" => str_arg(&args, "pattern").map(|pattern| {
            let path = str_arg(&args, "path").filter(|p| !p.is_empty());
            format!("GREP /{}/{}", collapse(pattern), path.map(|p| format!(" in {p}")).unwrap_or_default())
        }),
        "BASH" | "VERIFY" | "CHECK" => str_arg(&args, "command").map(|command| format!("{tool_name} {}", collapse(command))),
        "PATCH" | "WRITE" => {
            let (paths, write) = patch_targets(&args);
            (!paths.is_empty()).then(|| format!("{tool_name} {}{}", paths.join(", "), if write { " (write)" } else { "" }))
        }
        "REFERENCE" => match str_arg(&args, "action").as_deref() {
            Some("show") => Some(match (str_arg(&args, "path"), args.get("chunk").and_then(|value| value.as_i64())) {
                (Some(path), _) => format!("REFERENCE show {path}"),
                (None, Some(chunk)) => format!("REFERENCE show chunk {chunk}"),
                (None, None) => "REFERENCE show".to_string(),
            }),
            _ => str_arg(&args, "query").map(|query| format!("REFERENCE search {}", collapse(query))),
        },
        "DIR" => str_arg(&args, "path").filter(|p| !p.is_empty()).map(|path| format!("DIR {path}")),
        _ => None,
    };

    if let Some(text) = known {
        return text;
    }

    let pairs: Vec<String> = args.iter().map(|(key, value)| format!("{key}={}", fmt_arg_value(value))).collect();
    if pairs.is_empty() {
        tool_name.to_string()
    } else {
        format!("{tool_name} {}", pairs.join(" "))
    }
}

fn tool_name_of(entry: &TranscriptEventEntry) -> String {
    if let Some(name) = entry.data.as_ref().and_then(|d| d.tool_name.as_deref()).map(str::trim) {
        if !name.is_empty() {
            return name.to_string();
        }
    }
    entry.detail.split(|ch: char| ch.is_whitespace() || ch == ':').next().unwrap_or("").to_string()
}

fn tool_call_row(entry: &TranscriptEventEntry) -> RowCell {
    let tool = tool_name_of(entry);
    // detail is `<TOOL> <json args>`; strip the name once so the JSON stands alone.
    let raw_args = entry.detail.strip_prefix(&format!("{tool} ")).unwrap_or(&entry.detail);
    RowCell::plain(format!("{}{}", prefix(entry), summarize_tool_call(&tool, raw_args)), event_color(entry.kind))
}

// ── Tool results ─────────────────────────────────────────────────────────────

/// Split a result detail into the tool's own first line and its body.
fn split_result(entry: &TranscriptEventEntry, tool: &str) -> (String, String) {
    let mut text: &str = &entry.detail;
    for lead in [format!("{tool} (failed): "), format!("{tool}: "), format!("{tool} (failed):"), format!("{tool}:")] {
        if let Some(rest) = text.strip_prefix(lead.as_str()) {
            text = rest;
            break;
        }
    }
    match text.find('\n') {
        None => (text.to_string(), String::new()),
        Some(nl) => {
            // The harness writes `<summary>\n\n<output>`; drop the blank separator.
            let mut body = &text[nl + 1..];
            loop {
                let trimmed = body.trim_start_matches([' ', '\t']);
                match trimmed.strip_prefix('\n') {
                    Some(rest) => body = rest,
                    None => break,
                }
            }
            (text[..nl].to_string(), body.to_string())
        }
    }
}

fn body_lines(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in strip_ansi(body).replace('\t', "  ").split('\n') {
        let line = raw.trim_end();
        if line.trim().is_empty() || elision_marker().is_match(line.trim()) {
            continue;
        }
        out.push(line.to_string());
    }
    out
}

fn bash_exit_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^Bash command output from (.+?) — exit code (\d+)$").unwrap())
}

/// Header row (`[iter] result  TOOL · first line · 12ms`, red with `failed`
/// on failure) followed by up to `max_body_lines` dim body lines and a
/// `… +N lines` marker. Failed results show their whole message as the body
/// — the error text is the useful part.
pub fn tool_result_rows(entry: &TranscriptEventEntry, inner_w: usize, max_body_lines: usize) -> Vec<RowCell> {
    let tool = tool_name_of(entry);
    let failed = entry.data.as_ref().and_then(|d| d.failed) == Some(true);
    let (first, body) = split_result(entry, &tool);
    let duration = fmt_ms(entry.data.as_ref().and_then(|d| d.duration_ms));

    let mut parts = vec![tool.clone()];
    if !failed {
        let summary = collapse(&strip_ansi(&first));
        let summary = summary.strip_suffix('.').unwrap_or(&summary).to_string();
        // `Bash command output from <cwd> — exit code N` says the same as `exit N`.
        if let Some(bash) = bash_exit_re().captures(&summary) {
            let cwd = &bash[1];
            parts.push(format!("exit {}{}", &bash[2], if cwd == "." { String::new() } else { format!(" in {cwd}") }));
        } else if !summary.is_empty() {
            parts.push(summary);
        }
    }
    if let Some(duration) = duration {
        parts.push(duration);
    }
    if failed {
        parts.push("failed".to_string());
    }
    let mut rows = vec![RowCell::plain(
        format!("{}{}", prefix(entry), parts.join(" · ")),
        if failed { c::red } else { event_color(entry.kind) },
    )];

    // On failure the message may be a single long line: wrap it so it reads.
    let lines: Vec<String> = if failed {
        body_lines(&format!("{first}\n{body}"))
            .iter()
            .flat_map(|line| wrap_ansi(line, inner_w.saturating_sub(BODY_INDENT.len()).max(1)))
            .collect()
    } else {
        body_lines(&body)
    };
    let shown = &lines[..lines.len().min(max_body_lines)];
    for line in shown {
        rows.push(RowCell::plain(format!("{BODY_INDENT}{line}"), c::dim));
    }
    if lines.len() > shown.len() {
        rows.push(RowCell::plain(format!("{BODY_INDENT}… +{} lines", lines.len() - shown.len()), c::dim));
    }
    rows
}

// ── Markdown blocks ──────────────────────────────────────────────────────────

/// Render markdown to ANSI and wrap it to `inner_w` as rich rows. With
/// `max_lines`, keeps the first `max_lines` rows and appends `… +N lines`.
pub fn markdown_rows(source: &str, inner_w: usize, max_lines: Option<usize>) -> Vec<RowCell> {
    let mut rendered = render_markdown_ansi(source);
    if !color_enabled() {
        rendered = strip_ansi(&rendered);
    }

    let mut lines: Vec<String> = Vec::new();
    for line in wrap_ansi(&rendered, inner_w.max(1)) {
        let blank = strip_ansi(&line).trim().is_empty();
        // Collapse runs of blank lines; the pane is a log, not a page.
        if blank && lines.last().is_none_or(|last| strip_ansi(last).trim().is_empty()) {
            continue;
        }
        lines.push(if blank { String::new() } else { line });
    }
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }

    let kept = match max_lines {
        Some(max) if lines.len() > max => &lines[..max],
        _ => &lines[..],
    };
    let mut rows: Vec<RowCell> = kept
        .iter()
        .map(|text| {
            if text.is_empty() {
                RowCell::plain("", c::dim)
            } else {
                RowCell { text: text.clone(), color: None, selected: false, rich: true }
            }
        })
        .collect();
    if kept.len() < lines.len() {
        rows.push(RowCell::plain(format!("… +{} lines", lines.len() - kept.len()), c::dim));
    }
    rows
}

// ── Events ───────────────────────────────────────────────────────────────────

fn inference_row(entry: &TranscriptEventEntry) -> RowCell {
    let color = event_color(entry.kind);
    if let Some(d) = &entry.data {
        if let (Some(model), Some(prompt), Some(completion)) = (d.model.as_deref().filter(|m| !m.is_empty()), d.prompt_tokens, d.completion_tokens) {
            let mut parts = vec![model.to_string(), format!("{}→{} tok", fmt_tokens(prompt), fmt_tokens(completion))];
            if let Some(latency) = fmt_ms(d.latency_ms) {
                parts.push(latency);
            }
            return RowCell::plain(format!("{}{}", prefix(entry), parts.join(" · ")), color);
        }
    }
    RowCell::plain(format!("{}{}", prefix(entry), collapse(&entry.detail)), color)
}

fn context_event_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)^([A-Za-z_]+) (\{.*\})( \(.*\))?$").unwrap())
}

fn event_rows(entry: &TranscriptEventEntry, inner_w: usize) -> Vec<RowCell> {
    let color = event_color(entry.kind);
    match entry.kind {
        HarnessEventType::ToolCall => vec![tool_call_row(entry)],
        HarnessEventType::ToolResult => tool_result_rows(entry, inner_w, RESULT_PREVIEW_LINES),
        HarnessEventType::ModelText | HarnessEventType::OperatorMessage => {
            let mut rows = vec![RowCell::plain(prefix(entry).trim_end(), color)];
            rows.extend(markdown_rows(&entry.detail, inner_w, None));
            rows
        }
        HarnessEventType::RunSummary => {
            let mut rows = vec![RowCell::plain("── run summary ──", color)];
            rows.extend(markdown_rows(&entry.detail, inner_w, None));
            rows
        }
        HarnessEventType::IterationStart => vec![RowCell::plain(format!("── {} ──", collapse(&entry.detail)), color)],
        HarnessEventType::Inference => vec![inference_row(entry)],
        HarnessEventType::LoopStart | HarnessEventType::HarnessOp | HarnessEventType::TaskFinished => {
            vec![RowCell::plain(format!("{}{}", prefix(entry), collapse(&entry.detail)), color)]
        }
        HarnessEventType::ContextExpired | HarnessEventType::ContextPromoted | HarnessEventType::ContextRefreshed => {
            // `<TOOL> <json input>[ (ttl …)]` — the same shape as a call, so the
            // same summary; anything else (fold notices) prints as-is.
            let text = match context_event_re().captures(&entry.detail) {
                Some(m) => format!("{}{}", summarize_tool_call(&m[1], &m[2]), m.get(3).map(|s| s.as_str()).unwrap_or("")),
                None => collapse(&entry.detail),
            };
            vec![RowCell::plain(format!("{}{}", prefix(entry), text), color)]
        }
        _ => {
            // Warnings, stalls, waits, context events, run-complete: keep every word.
            let mut rows = Vec::new();
            push_wrapped(&mut rows, &format!("{}{}", prefix(entry), entry.detail), inner_w, color);
            rows
        }
    }
}

// Kinds whose rows end with a blank separator so the next block stands apart.
fn is_block_kind(kind: HarnessEventType) -> bool {
    matches!(kind, HarnessEventType::ModelText | HarnessEventType::RunSummary | HarnessEventType::OperatorMessage)
}

fn run_reason_name(reason: &crate::core::types::HarnessRunReason) -> String {
    serde_json::to_value(reason).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
}

// ── Entry point ──────────────────────────────────────────────────────────────

/// Flatten transcript entries into row cells for the [0] pane.
pub fn flatten_transcript(entries: &[TranscriptEntry], inner_w: usize) -> Vec<RowCell> {
    // The newest route entry is already pinned by model_header_rows; drop it from
    // the scrolling body so it is not printed twice. Older route entries stay —
    // they are where the model changed mid-session.
    let newest_model = entries.iter().rposition(|entry| matches!(entry, TranscriptEntry::Model(_)));
    let mut rows: Vec<RowCell> = Vec::new();

    for (i, entry) in entries.iter().enumerate() {
        match entry {
            TranscriptEntry::Goal(goal) => {
                push_blank(&mut rows);
                rows.push(RowCell::plain("❯ goal", c::accent));
                rows.extend(markdown_rows(&goal.text, inner_w, Some(GOAL_PREVIEW_LINES)));
                push_blank(&mut rows);
            }
            TranscriptEntry::RunEnd(end) => {
                rows.push(RowCell::plain(format!("── run end ({}) ──", run_reason_name(&end.reason)), c::dim));
            }
            TranscriptEntry::Model(model) => {
                if Some(i) == newest_model {
                    continue;
                }
                for route in format_model_route_lines(model) {
                    push_wrapped(&mut rows, &route, inner_w, c::dim);
                }
            }
            TranscriptEntry::Skill(skill) => {
                rows.push(RowCell::plain(
                    format!("skill {} {}", skill.name, if skill.enabled { "enabled" } else { "disabled" }),
                    c::dim,
                ));
            }
            TranscriptEntry::Error(note) => push_wrapped(&mut rows, &note.text, inner_w, c::red),
            TranscriptEntry::Info(note) => push_wrapped(&mut rows, &note.text, inner_w, c::dim),
            TranscriptEntry::Event(event) => {
                if event.kind == HarnessEventType::IterationStart {
                    push_blank(&mut rows);
                }
                rows.extend(event_rows(event, inner_w));
                if is_block_kind(event.kind) {
                    push_blank(&mut rows);
                }
            }
        }
    }

    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::transcript::{TranscriptGoalEntry, TranscriptRunEndEntry};
    use crate::core::types::{HarnessEventData, HarnessRunReason};

    fn event(kind: HarnessEventType, detail: &str, data: Option<HarnessEventData>) -> TranscriptEventEntry {
        TranscriptEventEntry { at: "t".into(), data, detail: detail.into(), goal_id: "g".into(), iteration: 2, kind }
    }

    #[test]
    fn token_and_ms_formats_match_the_ts() {
        assert_eq!(fmt_tokens(950), "950");
        assert_eq!(fmt_tokens(12_300), "12.3k");
        assert_eq!(fmt_ms(None), None);
        assert_eq!(fmt_ms(Some(12)).as_deref(), Some("12ms"));
        assert_eq!(fmt_ms(Some(1500)).as_deref(), Some("1.5s"));
    }

    #[test]
    fn summarizes_known_tools_and_falls_back_for_others() {
        assert_eq!(summarize_tool_call("READ", r#"{"path":"a.rs","offset":10,"limit":5}"#), "READ a.rs:10+5");
        assert_eq!(summarize_tool_call("GREP", r#"{"pattern":"fn  main","path":"src"}"#), "GREP /fn main/ in src");
        assert_eq!(summarize_tool_call("PATCH", r#"{"files":[{"path":"a","content":"x"},{"path":"b","find":"1","replace":"2"}]}"#), "PATCH a, b (write)");
        assert_eq!(summarize_tool_call("BASH", "{not json"), "BASH {not json");
        assert_eq!(summarize_tool_call("FETCH", r#"{"url":"http://x","maxBytes":10}"#), "FETCH url=http://x maxBytes=10");
    }

    #[test]
    fn tool_result_rows_summarize_bash_and_preview_body() {
        let _guard = crate::watch::ansi::color_test_lock();
        crate::watch::ansi::set_color_enabled(false);
        let data = HarnessEventData { duration_ms: Some(12), tool_name: Some("BASH".into()), ..Default::default() };
        let entry = event(HarnessEventType::ToolResult, "BASH: Bash command output from . — exit code 0.\n\nl1\nl2\nl3\nl4\nl5\nl6", Some(data));
        let rows = tool_result_rows(&entry, 80, RESULT_PREVIEW_LINES);
        assert_eq!(rows[0].text, "[  2] result BASH · exit 0 · 12ms");
        assert_eq!(rows[1].text, "    l1");
        assert_eq!(rows.len(), 6);
        assert_eq!(rows[5].text, "    … +2 lines");
    }

    #[test]
    fn markdown_rows_collapse_blank_runs_and_cap_lines() {
        let _guard = crate::watch::ansi::color_test_lock();
        crate::watch::ansi::set_color_enabled(false);
        let rows = markdown_rows("# T\n\n\n\nbody\nmore", 40, Some(2));
        assert!(rows[0].rich);
        assert_eq!(rows[0].text, "T");
        assert_eq!(rows[1].text, "");
        assert_eq!(rows[2].text, "… +2 lines");
    }

    #[test]
    fn flatten_orders_blocks_and_drops_newest_model_entry() {
        let _guard = crate::watch::ansi::color_test_lock();
        crate::watch::ansi::set_color_enabled(false);
        let goal = TranscriptEntry::Goal(TranscriptGoalEntry { at: "t".into(), goal_id: "g".into(), images: vec![], mentions: vec![], text: "do it".into() });
        let call = TranscriptEntry::Event(event(HarnessEventType::ToolCall, r#"READ {"path":"a.rs"}"#, None));
        let end = TranscriptEntry::RunEnd(TranscriptRunEndEntry { at: "t".into(), goal_id: "g".into(), iterations: 3, reason: HarnessRunReason::Completed });
        let model = TranscriptEntry::Model(crate::cli::transcript::TranscriptModelEntry {
            at: "t".into(), goal_id: "g".into(), model: "m".into(), profile_id: "p".into(), provider: "openai".into(),
            reasoning_effort: None, roles: None, tool_model: None, tool_profile_id: None, tool_reasoning_effort: None,
        });
        let rows = flatten_transcript(&[model, goal, call, end], 60);
        let texts: Vec<&str> = rows.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(texts[0], "❯ goal");
        assert!(texts.contains(&"[  2] tool   READ a.rs"));
        assert_eq!(*texts.last().unwrap(), "── run end (completed) ──");
        assert!(!texts.iter().any(|t| t.contains("profile")));
    }
}
