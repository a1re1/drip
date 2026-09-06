// REFERENCE — read-only hybrid search over an oasis-indexed markdown corpus.
//
// Built-ins are plain function modules (see mod.rs): definition() is the
// OpenAI function schema drip sends; prepare() validates the model's
// arguments; execute_prepared() shells out to the `oasis` CLI (one
// `--root <dir>` per configured corpus root; oasis logs INFO lines to
// stderr, so only stdout is parsed); render_search()/render_show() shape
// the model-facing text; execute() is the whole pipeline as a ToolOutcome.
//
// Corpus roots arrive via ToolCtx.reference_roots, resolved once at the CLI
// boundary (--reference-root flags, else DRIP_REFERENCE_ROOTS, else
// OASIS_ROOTS). Relative roots resolve against ctx.cwd; a root that does
// not exist is an execute-time error, not a pack-time one.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::{tool_arguments, ToolCtx, ToolOutcome};

/// Cap on the whole search listing the model sees (a context budget, not a
/// dump). The truncation marker counts against the cap.
const SEARCH_OUTPUT_CAP: usize = 8000;
/// Cap on the whole show text the model sees, marker included.
const SHOW_OUTPUT_CAP: usize = 20000;
/// Per-hit snippet cap, in characters.
const SNIPPET_CAP: usize = 400;
/// Child-process budget: hybrid search takes ~4-8 s; 60 s is generous.
const OASIS_TIMEOUT_MS: u64 = 60_000;
/// How much of the child's stderr a failure message quotes.
const STDERR_DIAGNOSTIC_CHARS: usize = 500;
/// Appended inside the cap when output is truncated.
/// Appended inside the cap when output is truncated. examples/reference_probe.rs
/// matches on this text to drop a half-cut trailing hit line.
const TRUNCATION_MARKER: &str = "\n… (output truncated)";

// ---------------------------------------------------------------------------
// Tool definition
// ---------------------------------------------------------------------------

/// The OpenAI function definition drip sends for this tool (the
/// {type: "function", function: {...}} envelope).
pub fn definition() -> Value {
    json!({
        "type": "function",
        "function": {
            "description": "Read-only hybrid (BM25 + dense) search over the configured oasis corpus roots — indexed markdown wikis, e.g. a computer-science reference. Workflow: search a topic first; read each hit's path as its citation; show a promising path (or a hit's chunk id) to read that page in full; cite the page path in your answer; if a query misses, rephrase it and search again. Input: {action: \"search\"|\"show\", query?, k?, mode?: \"hybrid\"|\"lexical\", path?, chunk?}.",
            "name": "REFERENCE",
            "parameters": {
                "additionalProperties": false,
                "properties": {
                    "action": {
                        "description": "What to do: \"search\" ranks corpus pages for a query; \"show\" prints one page or one chunk in full.",
                        "enum": ["search", "show"],
                        "type": "string"
                    },
                    "chunk": {
                        "description": "Chunk id from a previous search hit (action=\"show\", exactly one of path/chunk).",
                        "type": "number"
                    },
                    "k": {
                        "description": "Hits to return (default 5, clamped to 1..=20).",
                        "type": "number"
                    },
                    "mode": {
                        "description": "Search mode: \"hybrid\" (BM25 + dense, default) or \"lexical\" (exact-term only, faster).",
                        "enum": ["hybrid", "lexical"],
                        "type": "string"
                    },
                    "path": {
                        "description": "Page path relative to the corpus root, e.g. \"concepts/bm25.md\" (action=\"show\", exactly one of path/chunk).",
                        "type": "string"
                    },
                    "query": {
                        "description": "The search query (required when action=\"search\").",
                        "type": "string"
                    }
                },
                "required": ["action"],
                "type": "object"
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Prepare stage
// ---------------------------------------------------------------------------

/// The validated arguments prepare hands to the execute stage.
#[derive(Debug, PartialEq)]
pub enum ReferenceAction {
    Search {
        query: String,
        k: usize,
        lexical: bool,
    },
    Show {
        path: Option<String>,
        chunk: Option<i64>,
    },
}

#[derive(Debug)]
pub struct ReferencePrepared {
    pub action: ReferenceAction,
}

/// Validate the model's arguments, mirroring the schema above.
pub fn prepare(args: &Value, _ctx: &ToolCtx) -> Result<ReferencePrepared> {
    let args = tool_arguments(args)?;

    let action = args
        .get("action")
        .and_then(|value| value.as_str())
        .ok_or_else(|| anyhow!("Missing required string argument \"action\" (\"search\" or \"show\")."))?;

    match action {
        "search" => {
            let query = args
                .get("query")
                .and_then(|value| value.as_str())
                .map(|text| text.to_string())
                .filter(|text| !text.trim().is_empty())
                .ok_or_else(|| anyhow!("Missing required string argument \"query\" when action=\"search\"."))?;
            let k = clamp_k(args.get("k"));
            let lexical = match args.get("mode").map(|value| value.as_str().unwrap_or_default()) {
                None | Some("") | Some("hybrid") => false,
                Some("lexical") => true,
                Some(other) => {
                    return Err(anyhow!(
                        "Invalid \"mode\" {other:?}: expected \"hybrid\" or \"lexical\"."
                    ))
                }
            };
            Ok(ReferencePrepared {
                action: ReferenceAction::Search {
                    query,
                    k,
                    lexical,
                },
            })
        }
        "show" => {
            let path = match args.get("path") {
                None | Some(Value::Null) => None,
                Some(Value::String(text)) => Some(text.clone()),
                Some(other) => return Err(anyhow!("\"path\" must be a string, got {other}")),
            };
            let chunk = match args.get("chunk") {
                None | Some(Value::Null) => None,
                Some(value) => Some(
                    value
                        .as_i64()
                        .ok_or_else(|| anyhow!("\"chunk\" must be a chunk id number, got {value}"))?,
                ),
            };
            if path.is_some() == chunk.is_some() {
                return Err(anyhow!(
                    "action=\"show\" needs exactly one of \"path\" or \"chunk\"; got {}.",
                    if path.is_none() && chunk.is_none() {
                        "neither"
                    } else {
                        "both"
                    }
                ));
            }
            Ok(ReferencePrepared {
                action: ReferenceAction::Show { path, chunk },
            })
        }
        other => Err(anyhow!(
            "Unknown \"action\" {other:?}: expected \"search\" or \"show\"."
        )),
    }
}

/// k defaults to 5 and clamps into 1..=20.
fn clamp_k(value: Option<&Value>) -> usize {
    let raw = value.and_then(|value| value.as_i64()).unwrap_or(5);
    raw.clamp(1, 20) as usize
}

// ---------------------------------------------------------------------------
// Execute stage
// ---------------------------------------------------------------------------

/// Run the prepared call: spawn oasis with one --root per configured root,
/// parse its stdout (stderr carries INFO logs and is only used for
/// diagnostics), and shape the text the model sees.
pub fn execute_prepared(prepared: &ReferencePrepared, ctx: &ToolCtx) -> Result<String> {
    let roots = resolve_roots(ctx)?;
    let (arguments, label) = match &prepared.action {
        ReferenceAction::Search { query, k, lexical } => {
            let mut arguments = vec![
                "search".to_string(),
                "--json".to_string(),
                "-k".to_string(),
                k.to_string(),
            ];
            if *lexical {
                arguments.push("--lexical-only".to_string());
            }
            // "--" last, so a query like "->" or "-k" is a positional query
            // rather than a flag oasis would reject.
            arguments.push("--".to_string());
            arguments.push(query.clone());
            (arguments, format!("search {query:?}"))
        }
        ReferenceAction::Show { path, chunk } => {
            let arguments = match (path, chunk) {
                (Some(path), _) => vec!["show".to_string(), "--path".to_string(), path.clone()],
                (None, Some(chunk)) => vec![
                    "show".to_string(),
                    "--chunk".to_string(),
                    chunk.to_string(),
                ],
                (None, None) => unreachable!("prepare enforces exactly one of path/chunk"),
            };
            (arguments, show_label(&prepared.action))
        }
    };

    let (stdout, stderr, code) = run_oasis(&roots, &arguments)?;
    if code != Some(0) {
        return Err(anyhow!(
            "oasis {} exited with status {}: {}",
            label,
            code.map(|code| code.to_string()).unwrap_or_else(|| "signal".to_string()),
            stderr_diagnostic(&stderr)
        ));
    }

    match &prepared.action {
        ReferenceAction::Search { query, k, lexical } => {
            let mode = if *lexical { "lexical" } else { "hybrid" };
            let hits: Vec<Value> = serde_json::from_str(stdout.trim()).map_err(|error| {
                anyhow!(
                    "could not parse oasis search output as JSON ({error}): {}",
                    stderr_diagnostic(&format!("stdout: {}", truncate_chars(stdout.trim(), 300)))
                )
            })?;
            render_search(query, *k, mode, &hits)
        }
        ReferenceAction::Show { .. } => Ok(render_show(&label, &stdout)),
    }
}

/// The transcript/header label for a show call — the verb plus its target.
/// render_show prefixes only "REFERENCE", so the verb lives here alone.
fn show_label(action: &ReferenceAction) -> String {
    match action {
        ReferenceAction::Show { path: Some(path), .. } => format!("show {path}"),
        ReferenceAction::Show { chunk: Some(chunk), .. } => format!("show chunk {chunk}"),
        _ => "show".to_string(),
    }
}

/// Validate and absolutize the configured roots against ctx.cwd. Empty
/// roots and nonexistent roots are execute-time errors with actionable
/// guidance.
fn resolve_roots(ctx: &ToolCtx) -> Result<Vec<PathBuf>> {
    if ctx.reference_roots.is_empty() {
        return Err(anyhow!(
            "no reference corpus configured; pass --reference-root <dir> or set DRIP_REFERENCE_ROOTS"
        ));
    }
    let mut resolved = Vec::with_capacity(ctx.reference_roots.len());
    for root in &ctx.reference_roots {
        let root = if root.is_absolute() {
            root.clone()
        } else {
            ctx.cwd.join(root)
        };
        if !root.is_dir() {
            return Err(anyhow!(
                "reference corpus root {} does not exist; pass --reference-root <dir> or set DRIP_REFERENCE_ROOTS to an oasis-indexed markdown directory",
                root.display()
            ));
        }
        resolved.push(root);
    }
    Ok(resolved)
}

/// Spawn `oasis --root <dir> ... <arguments>` with a hard timeout. The
/// child's stdout and stderr are drained on helper threads so full pipes
/// cannot deadlock the spawn; past the deadline the child is killed and the
/// call errors instead of hanging.
fn run_oasis(roots: &[PathBuf], arguments: &[String]) -> Result<(String, String, Option<i32>)> {
    let mut command = Command::new("oasis");
    for root in roots {
        command.arg("--root").arg(root);
    }
    command.args(arguments);
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    let mut child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            anyhow!(
                "the `oasis` binary was not found on PATH; install oasis so REFERENCE can search the corpus"
            )
        } else {
            anyhow!("failed to spawn oasis: {error}")
        }
    })?;

    let mut stdout_pipe = child.stdout.take().expect("oasis stdout was piped");
    let mut stderr_pipe = child.stderr.take().expect("oasis stderr was piped");
    let stdout_handle = std::thread::spawn(move || {
        let mut buffer = String::new();
        let _ = stdout_pipe.read_to_string(&mut buffer);
        buffer
    });
    let stderr_handle = std::thread::spawn(move || {
        let mut buffer = String::new();
        let _ = stderr_pipe.read_to_string(&mut buffer);
        buffer
    });

    let deadline = Instant::now() + Duration::from_millis(OASIS_TIMEOUT_MS);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let stdout = stdout_handle.join().unwrap_or_default();
                let stderr = stderr_handle.join().unwrap_or_default();
                return Ok((stdout, stderr, status.code()));
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(anyhow!(
                        "oasis timed out after {OASIS_TIMEOUT_MS} ms and was killed"
                    ));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(anyhow!("failed to wait for oasis: {error}")),
        }
    }
}

/// First ~500 characters of the child's stderr, for failure messages.
fn stderr_diagnostic(stderr: &str) -> String {
    truncate_chars(stderr.trim(), STDERR_DIAGNOSTIC_CHARS)
}

/// Truncate to at most max_chars characters (Unicode-safe), appending an
/// ellipsis when anything was cut.
fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let kept: String = text.chars().take(max_chars).collect();
    format!("{kept}…")
}

// ---------------------------------------------------------------------------
// Output shaping (pure, testable from strings/JSON)
// ---------------------------------------------------------------------------

/// One hit of an oasis `search --json` array.
#[derive(Debug, PartialEq)]
struct SearchHit {
    chunk_id: Option<i64>,
    heading_path: Vec<String>,
    path: String,
    snippet: String,
}

fn parse_hit(value: &Value) -> Option<SearchHit> {
    Some(SearchHit {
        chunk_id: value.get("chunk_id").and_then(|value| value.as_i64()),
        heading_path: value
            .get("heading_path")
            .and_then(|value| value.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(|text| text.to_string()))
                    .collect()
            })
            .unwrap_or_default(),
        path: value.get("path")?.as_str()?.to_string(),
        snippet: value
            .get("snippet")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string(),
    })
}

/// Render the search listing: header line, then one block per hit with the
/// snippet capped at SNIPPET_CAP characters and the whole output capped at
/// SEARCH_OUTPUT_CAP characters including the truncation marker.
fn render_search(query: &str, k: usize, mode: &str, hits: &[Value]) -> Result<String> {
    let parsed: Vec<SearchHit> = hits.iter().filter_map(parse_hit).collect();
    if parsed.len() != hits.len() {
        // Every oasis hit carries a path; a hit without one means the output
        // shape changed, which must not read as a thinner ranking.
        return Err(anyhow!(
            "oasis returned {} hit(s) without a \"path\" field — the search output shape changed",
            hits.len() - parsed.len()
        ));
    }
    if parsed.is_empty() {
        return Ok(format!(
            "REFERENCE search \"{query}\" — 0 hits (k={k}, {mode}).\nNo hits for this query. \
Rephrase the query with different or more general terms and search again."
        ));
    }

    let header = format!("REFERENCE search \"{query}\" — {} hits (k={k}, {mode})", parsed.len());
    let mut lines: Vec<String> = Vec::new();
    for (index, hit) in parsed.iter().enumerate() {
        let rank = index + 1;
        let chunk = match hit.chunk_id {
            Some(chunk_id) => format!("  [chunk {chunk_id}]"),
            None => String::new(),
        };
        let heading = if hit.heading_path.is_empty() {
            String::new()
        } else {
            format!("  {}", hit.heading_path.join(" > "))
        };
        lines.push(format!("{rank}. {}{chunk}{heading}", hit.path));
        for snippet_line in truncate_chars(hit.snippet.trim(), SNIPPET_CAP).lines() {
            lines.push(format!("    {snippet_line}"));
        }
    }
    let body = lines.join("\n");
    Ok(cap_output(format!("{header}\n{body}"), SEARCH_OUTPUT_CAP))
}

/// Render the show output: a header with the target and size, then the full
/// text capped at SHOW_OUTPUT_CAP characters including the marker. `label`
/// already carries the verb ("show concepts/bm25.md" / "show chunk 12").
fn render_show(label: &str, text: &str) -> String {
    cap_output(
        format!("REFERENCE {label} ({} chars)\n{text}", text.chars().count()),
        SHOW_OUTPUT_CAP,
    )
}

/// Append the truncation marker inside the cap when the text exceeds it.
fn cap_output(text: String, cap: usize) -> String {
    if text.chars().count() <= cap {
        return text;
    }
    let budget = cap.saturating_sub(TRUNCATION_MARKER.chars().count());
    let kept: String = text.chars().take(budget).collect();
    format!("{kept}{TRUNCATION_MARKER}")
}

// ---------------------------------------------------------------------------
// Transcript display + the whole pipeline
// ---------------------------------------------------------------------------

/// The one-line transcript form of a REFERENCE call.
pub fn display_input(args: &Value, _ctx: &ToolCtx) -> Option<String> {
    let args = tool_arguments(args).ok()?;
    let action = args.get("action")?.as_str()?;
    match action {
        "search" => {
            let query = args.get("query")?.as_str()?;
            Some(format!("REFERENCE search \"{query}\""))
        }
        "show" => {
            if let Some(path) = args.get("path").and_then(|value| value.as_str()) {
                Some(format!("REFERENCE show {path}"))
            } else if let Some(chunk) = args.get("chunk").and_then(|value| value.as_i64()) {
                Some(format!("REFERENCE show chunk {chunk}"))
            } else {
                Some("REFERENCE show".to_string())
            }
        }
        _ => None,
    }
}

/// The whole prepare/execute pipeline as a ToolOutcome.
pub fn execute(args: &Value, ctx: &ToolCtx) -> ToolOutcome {
    let prepared = match prepare(args, ctx) {
        Ok(prepared) => prepared,
        Err(error) => return ToolOutcome::error(error),
    };
    match execute_prepared(&prepared, ctx) {
        Ok(text) => ToolOutcome::success(text),
        Err(error) => ToolOutcome::error(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::Path;

    fn ctx(roots: &[&str]) -> ToolCtx {
        ToolCtx {
            cwd: Path::new("/tmp/ws").to_path_buf(),
            allow_net: false,
            reference_roots: roots.iter().map(PathBuf::from).collect(),
        }
    }

    fn hit(path: &str, chunk_id: i64, heading: &str, snippet: &str) -> Value {
        json!({
            "chunk_id": chunk_id,
            "path": path,
            "heading_path": [heading],
            "line_start": 1,
            "line_end": 3,
            "score": 0.0163,
            "snippet": snippet,
            "chunks_matched": 3
        })
    }

    #[test]
    fn missing_roots_names_flag_and_env_var() {
        let error = prepare(
            &json!({"action": "search", "query": "bm25"}),
            &ctx(&[]),
        )
        .unwrap();
        let text = execute_prepared(&error, &ctx(&[])).unwrap_err().to_string();
        assert!(text.contains("--reference-root"), "got: {text}");
        assert!(text.contains("DRIP_REFERENCE_ROOTS"), "got: {text}");
    }

    #[test]
    fn unknown_action_is_rejected() {
        let error = prepare(&json!({"action": "delete"}), &ctx(&["wiki"])).unwrap_err();
        assert!(error.to_string().contains("Unknown"), "got: {error}");
    }

    #[test]
    fn k_clamps_into_one_to_twenty() {
        let low = prepare(&json!({"action": "search", "query": "q", "k": 0}), &ctx(&["wiki"])).unwrap();
        let high = prepare(&json!({"action": "search", "query": "q", "k": 99}), &ctx(&["wiki"])).unwrap();
        let default = prepare(&json!({"action": "search", "query": "q"}), &ctx(&["wiki"])).unwrap();
        assert_eq!(low.action, ReferenceAction::Search { query: "q".into(), k: 1, lexical: false });
        assert_eq!(high.action, ReferenceAction::Search { query: "q".into(), k: 20, lexical: false });
        assert_eq!(default.action, ReferenceAction::Search { query: "q".into(), k: 5, lexical: false });
    }

    #[test]
    fn show_requires_exactly_one_of_path_and_chunk() {
        let neither = prepare(&json!({"action": "show"}), &ctx(&["wiki"])).unwrap_err();
        assert!(neither.to_string().contains("neither"), "got: {neither}");
        let both = prepare(
            &json!({"action": "show", "path": "concepts/bm25.md", "chunk": 3}),
            &ctx(&["wiki"]),
        )
        .unwrap_err();
        assert!(both.to_string().contains("both"), "got: {both}");
        let path = prepare(&json!({"action": "show", "path": "concepts/bm25.md"}), &ctx(&["wiki"])).unwrap();
        let chunk = prepare(&json!({"action": "show", "chunk": 234}), &ctx(&["wiki"])).unwrap();
        assert_eq!(
            path.action,
            ReferenceAction::Show { path: Some("concepts/bm25.md".into()), chunk: None }
        );
        assert_eq!(chunk.action, ReferenceAction::Show { path: None, chunk: Some(234) });
    }

    #[test]
    fn search_requires_a_query() {
        let error = prepare(&json!({"action": "search"}), &ctx(&["wiki"])).unwrap_err();
        assert!(error.to_string().contains("\"query\""), "got: {error}");
    }

    #[test]
    fn invalid_mode_is_rejected() {
        let error = prepare(
            &json!({"action": "search", "query": "q", "mode": "fuzzy"}),
            &ctx(&["wiki"]),
        )
        .unwrap_err();
        assert!(error.to_string().contains("mode"), "got: {error}");
    }

    #[test]
    fn hand_written_json_renders_ranking_order_paths_and_snippets() {
        let hits = vec![
            hit("concepts/bm25.md", 234, "BM25", "## Related\n- tf-idf ranking"),
            hit("algorithms/kmp.md", 77, "KMP", "prefix function"),
        ];
        let text = render_search("bm25 ranking", 5, "hybrid", &hits).unwrap();
        assert!(
            text.starts_with("REFERENCE search \"bm25 ranking\" — 2 hits (k=5, hybrid)"),
            "got: {text}"
        );
        assert!(text.contains("1. concepts/bm25.md  [chunk 234]  BM25"), "got: {text}");
        assert!(text.contains("2. algorithms/kmp.md  [chunk 77]  KMP"), "got: {text}");
        let ranking_before = text.find("concepts/bm25.md").unwrap();
        assert!(ranking_before < text.find("algorithms/kmp.md").unwrap());
        assert!(text.contains("    ## Related"), "got: {text}");
    }

    #[test]
    fn long_snippets_are_truncated_to_four_hundred_chars() {
        let long_snippet: String = "x".repeat(1000);
        let hits = vec![hit("a.md", 1, "A", &long_snippet)];
        let text = render_search("q", 5, "hybrid", &hits).unwrap();
        let snippet_line = text.lines().nth(2).expect("snippet line");
        // "    " indent + 400 chars + the ellipsis.
        assert!(snippet_line.chars().count() <= 405, "got {} chars", snippet_line.chars().count());
        assert!(text.contains('…'));
    }

    #[test]
    fn search_output_is_capped_at_eight_thousand_chars() {
        let hits: Vec<Value> = (0..40)
            .map(|index| hit(&format!("page-{index}.md"), index, "H", &"y".repeat(900)))
            .collect();
        let text = render_search("q", 20, "lexical", &hits).unwrap();
        assert!(text.chars().count() <= 8000, "got {} chars", text.chars().count());
        assert!(text.ends_with("(output truncated)"), "got: {text}");
    }

    #[test]
    fn hit_without_a_path_is_reported_as_schema_drift() {
        let hits = vec![hit("concepts/bm25.md", 1, "BM25", "text"), json!({"chunk_id": 2})];
        let error = render_search("q", 5, "hybrid", &hits).unwrap_err().to_string();
        assert!(error.contains("without a \"path\""), "got: {error}");
    }

    // A query that looks like a flag must reach oasis as the positional query.
    #[test]
    fn queries_starting_with_a_dash_are_passed_after_a_separator() {
        let prepared = prepare(&json!({"action": "search", "query": "-> operator"}), &ctx(&["wiki"])).unwrap();
        let error = execute_prepared(&prepared, &ctx(&["/nope/missing"])).unwrap_err().to_string();
        assert!(error.contains("does not exist"), "got: {error}");
    }

    #[test]
    fn wrong_typed_show_arguments_name_the_argument() {
        let path = prepare(&json!({"action": "show", "path": 7}), &ctx(&["wiki"])).unwrap_err().to_string();
        assert!(path.contains("\"path\" must be a string"), "got: {path}");
        let chunk = prepare(&json!({"action": "show", "chunk": "twelve"}), &ctx(&["wiki"])).unwrap_err().to_string();
        assert!(chunk.contains("\"chunk\" must be a chunk id number"), "got: {chunk}");
    }

    #[test]
    fn empty_json_array_renders_no_hits_success() {
        let text = render_search("obscure topic", 5, "hybrid", &[]).unwrap();
        assert!(text.contains("0 hits"), "got: {text}");
        assert!(text.contains("Rephrase"), "got: {text}");
    }

    // The labels here are exactly the ones execute_prepared builds, so the
    // header cannot drift into a doubled verb ("REFERENCE show show <path>").
    #[test]
    fn show_output_has_header_and_cap() {
        let text = render_show("show concepts/bm25.md", "# BM25\nbody");
        assert!(
            text.starts_with("REFERENCE show concepts/bm25.md (11 chars)"),
            "got: {text}"
        );
        assert!(!text.contains("show show"), "doubled verb: {text}");
        assert!(text.contains("# BM25"), "got: {text}");

        let chunk = render_show("show chunk 12", "passage");
        assert!(chunk.starts_with("REFERENCE show chunk 12 (7 chars)"), "got: {chunk}");

        let big = "z".repeat(30_000);
        let capped = render_show("show big.md", &big);
        assert!(capped.chars().count() <= 20_000, "got {} chars", capped.chars().count());
        assert!(capped.ends_with("(output truncated)"));
    }

    // The verb in the header comes from the same label the failure path names,
    // so this pins the label shape execute_prepared passes to render_show.
    #[test]
    fn show_labels_carry_exactly_one_verb() {
        for (arguments, expected) in [
            (json!({"action": "show", "path": "concepts/bm25.md"}), "show concepts/bm25.md"),
            (json!({"action": "show", "chunk": 12}), "show chunk 12"),
        ] {
            let prepared = prepare(&arguments, &ctx(&["wiki"])).unwrap();
            assert_eq!(show_label(&prepared.action), expected);
        }
    }

    #[test]
    fn display_input_renders_readable_strings() {
        let search = display_input(
            &json!({"action": "search", "query": "bm25"}),
            &ctx(&["wiki"]),
        )
        .unwrap();
        assert_eq!(search, "REFERENCE search \"bm25\"");
        let show_path = display_input(
            &json!({"action": "show", "path": "concepts/bm25.md"}),
            &ctx(&["wiki"]),
        )
        .unwrap();
        assert_eq!(show_path, "REFERENCE show concepts/bm25.md");
        let show_chunk = display_input(&json!({"action": "show", "chunk": 234}), &ctx(&["wiki"])).unwrap();
        assert_eq!(show_chunk, "REFERENCE show chunk 234");
    }

    #[test]
    fn stderr_diagnostic_is_capped_at_five_hundred_chars() {
        let stderr = "INFO: loading model\n".repeat(200);
        let diagnostic = stderr_diagnostic(&stderr);
        assert!(diagnostic.chars().count() <= 501, "got {} chars", diagnostic.chars().count());
        assert!(diagnostic.contains("INFO"), "got: {diagnostic}");
    }

    #[test]
    fn nonexistent_root_is_an_execute_time_error() {
        let prepared = prepare(&json!({"action": "search", "query": "q"}), &ctx(&["missing"])).unwrap();
        let error = execute_prepared(&prepared, &ctx(&["missing"])).unwrap_err();
        assert!(error.to_string().contains("does not exist"), "got: {error}");
    }
}
