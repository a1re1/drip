//! Run one REFERENCE search through the tool's real code path and print the
//! result as one line of JSON — the probe the retrieval benchmark in
//! `evals/reference/` drives, so it measures the harness wiring rather than a
//! re-implementation of it.
//!
//! ```sh
//! cargo run --quiet --example reference_probe -- \
//!     --root ~/src/o-cs/wiki --k 10 --mode lexical "how does BM25 rank documents"
//! ```
//!
//! Output: {"query":…,"failed":bool,"paths":[…ranked page paths…],"text":"…"}

use std::path::PathBuf;

use drip::tools::builtin::reference;
use drip::tools::builtin::ToolCtx;
use serde_json::{json, Value};

/// The marker the tool appends when it caps its output (reference.rs).
const TRUNCATION_MARKER: &str = "… (output truncated)";

fn main() {
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut k: i64 = 10;
    let mut mode = "lexical".to_string();
    let mut query: Option<String> = None;

    let mut argv = std::env::args().skip(1);
    while let Some(argument) = argv.next() {
        match argument.as_str() {
            "--root" => match argv.next() {
                Some(value) => roots.push(PathBuf::from(value)),
                None => fail("--root needs a directory"),
            },
            "--k" => match argv.next().and_then(|value| value.parse::<i64>().ok()) {
                // The tool clamps out-of-range k silently; the benchmark must
                // not report a k it did not actually search with.
                Some(value) if (1..=20).contains(&value) => k = value,
                Some(value) => fail(&format!("--k must be 1..=20, got {value}")),
                None => fail("--k needs a number"),
            },
            "--mode" => match argv.next() {
                Some(value) if value == "hybrid" || value == "lexical" => mode = value,
                _ => fail("--mode needs \"hybrid\" or \"lexical\""),
            },
            other if other.starts_with("--") => fail(&format!("unknown flag {other}")),
            other => query = Some(other.to_string()),
        }
    }

    let query = match query {
        Some(query) if !query.trim().is_empty() => query,
        _ => return fail("a query is required"),
    };
    if roots.is_empty() {
        return fail("at least one --root is required");
    }

    // Exactly what the pack builds for a tool call, so the probe exercises the
    // registered tool rather than a copy of its logic.
    let ctx = ToolCtx {
        cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        allow_net: false,
        reference_roots: roots,
    };
    let arguments = json!({
        "action": "search",
        "query": query,
        "k": k,
        "mode": mode,
    })
    .to_string();

    let outcome = reference::execute(&Value::String(arguments), &ctx);
    let paths = ranked_paths(&outcome.text);

    println!(
        "{}",
        json!({
            "query": query,
            "failed": outcome.failed,
            "paths": paths,
            "text": outcome.text,
        })
    );
}

/// The page paths of a rendered search listing, best first. Hit lines look
/// like `1. concepts/bm25.md  [chunk 234]  BM25 > Related`, so the path is the
/// second whitespace-separated field of a line starting with `<rank>. `.
///
/// The tool caps its own output, and the cut can land mid-path, so a truncated
/// tail is dropped rather than scored as a ranked hit.
fn ranked_paths(text: &str) -> Vec<String> {
    let text = match text.rsplit_once(TRUNCATION_MARKER) {
        Some((kept, _)) => kept.rsplit_once('\n').map(|(head, _)| head).unwrap_or(kept),
        None => text,
    };
    text.lines()
        .filter_map(|line| {
            let (rank, rest) = line.split_once(". ")?;
            if rank.is_empty() || !rank.chars().all(|character| character.is_ascii_digit()) {
                return None;
            }
            rest.split_whitespace().next().map(|path| path.to_string())
        })
        .collect()
}

fn fail(message: &str) {
    eprintln!("reference_probe: {message}");
    std::process::exit(2);
}
