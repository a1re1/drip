// port of src/cli/review-report.ts
//
// The pure half of --review: prompt construction, report parsing, and the
// deterministic confidence table. Kept free of session/index imports (which
// pull in sqlite) so this logic — the part where a bug silently corrupts a
// score — is unit-testable.
//
// Part 1 covers ts lines 1-340. `plan_review_units`' tail and
// `build_file_review_prompt`'s delegate `build_unit_review_prompt` are defined
// past line 340 and arrive with part 2.

use regex::Regex;
use std::sync::OnceLock;

pub const SKIPPED_FILE_SUFFIXES: [&str; 10] = [
    ".gif", ".ico", ".jpg", ".jpeg", ".lock", ".pdf", ".png", ".snap", ".woff", ".woff2",
];
pub const SKIPPED_FILE_NAMES: [&str; 2] = ["bun.lock", "package-lock.json"];

// recensio's deterministic confidence table. The model never scores itself —
// an LLM asked for a confidence score drifts; this is arithmetic over the
// parsed P-level counts.
pub fn confidence_from_counts(p0: u32, p1: u32) -> u32 {
    if p0 > 0 {
        return 1;
    }

    if p1 == 0 {
        return 5;
    }

    if p1 <= 2 {
        return 4;
    }

    if p1 <= 4 {
        return 3;
    }

    2
}

// The per-priority finding counts parse_file_report extracts, and the shape
// with_computed_confidence reads (ts: { p0, p1, p2 }).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileReport {
    pub rating: Option<String>,
    pub rating_derived: bool,
    pub p0: u32,
    pub p1: u32,
    pub p2: u32,
}

fn rating_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\*\*Rating\*\*:\s*(✅|⚠️|❌)").unwrap())
}

fn issues_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\*\*Issues\*\*").unwrap())
}

fn file_header_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?m)^###\s*File:").unwrap())
}

fn section_start_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?m)^## Confidence Score[^\n]*\n").unwrap())
}

// JS string .length counts UTF-16 code units; the 200-char hasReport threshold
// is compared against that, so count the same way (astral emojis count as 2).
fn js_length(s: &str) -> usize {
    s.chars().map(|c| if (c as u32) > 0xFFFF { 2 } else { 1 }).sum()
}

// Extracts the rating line and per-priority counts from one child's report
// body. A malformed body is not a crash — the caller marks the file errored.
pub fn parse_file_report(report: &str) -> FileReport {
    let rating_match = rating_re().captures(report);
    // Tolerant on purpose: reviewers write "- P1 (line 42):", "- P1:", or
    // "- **P1** — ...". Anchoring on "(line" undercounted real findings, which
    // would inflate the confidence score computed from these counts.
    let count = |level: &str| -> u32 {
        let pattern = format!(r"(?m)^\s*[-*]\s*\**{}\**\b", level);
        Regex::new(&pattern).map(|re| re.find_iter(report).count() as u32).unwrap_or(0)
    };
    let p0 = count("P0");
    let p1 = count("P1");
    let p2 = count("P2");
    // A block that exists but skipped its Rating line (bundled reviewers do,
    // occasionally) is still a report: derive the rating from the counts the
    // exit code is computed from anyway. Only a body with no report at all
    // stays unrated.
    // "A report" is anything more than a placeholder or an error line: a
    // reviewer that answered in prose instead of the template (the 13-second
    // single-file child that wrote "No P0/P1 issues found") still rated the file.
    let trimmed = report.trim();
    let has_report = issues_re().is_match(report)
        || file_header_re().is_match(report)
        || (js_length(trimmed) >= 200 && !trimmed.starts_with('(') && !trimmed.starts_with("errored:"));
    let derived = if p0 > 0 {
        "❌"
    } else if p1 + p2 > 0 {
        "⚠️"
    } else {
        "✅"
    };

    FileReport {
        p0,
        p1,
        p2,
        rating: match &rating_match {
            Some(caps) => Some(caps[1].to_string()),
            None => has_report.then(|| derived.to_string()),
        },
        // Derived ratings are reported, but they are not evidence of a complete
        // review: a truncated or prose-only answer with no parseable findings
        // derives ✅, and the synthesis-skip decision must not take that as clean.
        rating_derived: rating_match.is_none() && has_report,
    }
}

// Replaces the synthesis pass's own Confidence Score section with the score
// computed from the parsed P0/P1 counts (recensio's deterministic table). A
// model asked to score its own review drifts; the arithmetic does not.
//
// The ts section regex is /^## Confidence Score[^\n]*\n(?:(?!^## )[\s\S])*/m —
// the lookahead isn't supported by the regex crate, so the same span (header
// line through the character before the next top-level "## " line) is found
// with the start regex plus a line scan below.
pub fn with_computed_confidence(report: &str, confidence: u32, counts: &FileReport) -> String {
    let emoji = if confidence == 5 {
        "✅"
    } else if confidence == 4 {
        "🟢"
    } else if confidence == 3 {
        "🟡"
    } else if confidence == 2 {
        "🟠"
    } else {
        "🔴"
    };
    let block = format!(
        "## Confidence Score\n{}/5 {} — {} P0, {} P1, {} P2 across the change.",
        confidence, emoji, counts.p0, counts.p1, counts.p2
    );

    match section_start_re().find(report) {
        Some(m) => {
            // Consume everything up to (not including) the next line that
            // starts with "## " — the span the ts lookahead pattern matched.
            let bytes = report.as_bytes();
            let mut end = m.end();
            while end < report.len() {
                if (end == 0 || bytes[end - 1] == b'\n') && report[end..].starts_with("## ") {
                    break;
                }
                end += 1;
            }
            format!("{}{}\n{}", &report[..m.start()], block, &report[end..])
        }
        None => format!("{}\n\n{}", report, block),
    }
}

// The one task a review child (or the synthesis pass) is seeded with. The
// harness starts in the task loop with this as current_task, so the prompt's
// delivery instruction can name it by id (seeded tasks are numbered from 1).
pub const REVIEW_TASK_ID: &str = "task-1";

pub fn file_review_task_title(path: &str) -> String {
    format!("Review {path}: read the full file, grep its callers and tests, then finish_task completed with the complete report as the summary")
}

pub const SYNTHESIS_TASK_TITLE: &str = "Synthesize the per-file reports into one holistic review, then finish_task completed with the complete report as the summary";

// (ts buildFileReviewPrompt delegates to buildUnitReviewPrompt, defined past
// line 340 — ported with part 2.)

pub struct SynthesisPromptArgs<'a> {
    pub base_ref: &'a str,
    pub context: &'a str,
    pub diff_stat: &'a str,
    pub log: &'a str,
    pub file_reports: &'a [String],
}

pub fn build_synthesis_prompt(args: SynthesisPromptArgs<'_>) -> String {
    [
        "You are the lead code reviewer synthesizing per-file review reports into one holistic review of the whole change. Work read-only — do not edit anything.".to_string(),
        "".to_string(),
        "## What this change is trying to achieve (the intent)".to_string(),
        args.context.to_string(),
        "".to_string(),
        "## The change at a glance".to_string(),
        "```".to_string(),
        args.diff_stat.to_string(),
        "```".to_string(),
        "".to_string(),
        "## Commit log".to_string(),
        "```".to_string(),
        args.log.to_string(),
        "```".to_string(),
        "".to_string(),
        "## Per-file review reports".to_string(),
        args.file_reports.join("\n\n"),
        "".to_string(),
        "## Your job".to_string(),
        "1. Architecture pass: do the changes cohere? Are there cross-file or contract concerns no single-file reviewer could see?".to_string(),
        "2. Deduplicate findings that share one root cause across files — report the root cause once.".to_string(),
        "3. Goal fit: state explicitly whether the change accomplishes the stated intent. A change that is technically clean but misses its goal is a real finding.".to_string(),
        "".to_string(),
        "## Output sections (in this exact order)".to_string(),
        format!("# Code Review: {}...HEAD", args.base_ref),
        "## Summary".to_string(),
        "## Confidence Score".to_string(),
        "## Issues Table".to_string(),
        "## Sequence Diagram (a mermaid block — only when the changes have interactions worth diagramming)".to_string(),
        "## Detailed Findings".to_string(),
        "".to_string(),
        "## Delivery".to_string(),
        format!(
            "Your task ledger already holds this synthesis as {} — do not call plan_tasks. Deliver the report by calling finish_task {{\"taskId\": \"{}\", \"status\": \"completed\", \"summary\": <the COMPLETE report, every section above included, verbatim>}} — that summary is what gets parsed. Text you write alongside a tool call is not recorded, so never rely on saying the report, and never reduce it to a one-line summary.",
            REVIEW_TASK_ID, REVIEW_TASK_ID
        ),
    ]
    .join("\n")
}

// Which text a review child actually produced as its report. Children run
// with summarizeRun off — the harness's run summary is a second model's
// paraphrase of the run ("2 of 2 tasks completed…"), not the report the
// prompt asked for. The prompts ask for the report as finish_task's summary,
// because the harness records a model's text only when the reply is
// text-only (text alongside a tool call is dropped), so the structured op is
// the one channel that survives verbatim. A report the model chose to say
// instead still counts: the newest text turn carrying the template's markers
// wins, then the newest completed-task summary carrying them, then the
// longest text turn, then a respond-op answer, then the longest task summary,
// then whatever run summary exists.
pub struct ReportBodySources<'a, F> {
    pub direct_response: Option<&'a str>,
    pub looks_like_report: F,
    pub model_texts: &'a [&'a str],
    pub summary: Option<&'a str>,
    pub task_summaries: &'a [&'a str],
}

pub fn pick_report_body<'a, F>(sources: ReportBodySources<'a, F>) -> Option<&'a str>
where
    F: Fn(&str) -> bool,
{
    let clean = |list: &[&'a str]| -> Vec<&'a str> {
        list.iter().map(|text| text.trim()).filter(|text| !text.is_empty()).collect()
    };
    let longest = |list: Vec<&'a str>| -> Option<&'a str> {
        list.into_iter().reduce(|best, text| if text.len() > best.len() { text } else { best })
    };
    let texts = clean(sources.model_texts);
    let task_summaries = clean(sources.task_summaries);
    let marked = texts
        .iter()
        .rev()
        .find(|text| (sources.looks_like_report)(text))
        .or_else(|| task_summaries.iter().rev().find(|text| (sources.looks_like_report)(text)))
        .copied();

    if let Some(marked) = marked {
        return Some(marked);
    }

    if !texts.is_empty() {
        return longest(texts);
    }

    let direct = sources.direct_response.map(str::trim).filter(|text| !text.is_empty());

    if direct.is_some() {
        return direct;
    }

    if !task_summaries.is_empty() {
        return longest(task_summaries);
    }

    let summary = sources.summary.map(str::trim).filter(|text| !text.is_empty());

    summary
}

pub fn looks_like_file_report(text: &str) -> bool {
    text.contains("**Rating**")
}

fn synthesis_marker_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?m)^#\s*Code Review\b").unwrap())
}

fn synthesis_confidence_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?m)^## Confidence Score").unwrap())
}

pub fn looks_like_synthesis_report(text: &str) -> bool {
    synthesis_marker_re().is_match(text) || synthesis_confidence_re().is_match(text)
}

// The per-file section header. The template already opens with "### File:",
// so a body that carries it is used as-is rather than headed twice.
pub fn format_file_report(path: &str, body: &str) -> String {
    let trimmed = body.trim();

    if Regex::new(r"^###\s*File:").unwrap().is_match(trimmed) {
        trimmed.to_string()
    } else {
        format!("### File: {path}\n{trimmed}")
    }
}

// --- Review unit planning -------------------------------------------------------
// One reviewer child per file was the dominant cost of --review: the transcript
// audit (docs/review-mode.md, item 3) found even a version bump paid ~15 model
// calls and ~150K prompt tokens across package.json, CHANGELOG.md, README.md
// and a comment-only hunk, each a full-price child whose report said only "✅".
// This planner batches related files into review units — every docs/manifest
// into one cheap unit, small related code files (a source file and its test)
// into shared units, oversized files alone — so --review spawns one child per
// unit instead of one per file.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewUnitKind {
    Docs,
    Code,
}

#[derive(Debug, Clone)]
pub struct ReviewUnit {
    pub kind: ReviewUnitKind,
    pub label: String,
    pub paths: Vec<String>,
    pub diff_lines: u32,
}

pub const MAX_UNIT_FILES: usize = 3;
pub const MAX_UNIT_DIFF_LINES: u32 = 400;

// Prose and manifests need a skim, not a code review, so they all land in one
// unit regardless of size. Case-insensitive on the extension; manifest names
// are matched exactly (no extension).
const DOCS_EXTENSIONS: [&str; 4] = [".md", ".mdx", ".txt", ".rst"];
const DOCS_BASENAMES: [&str; 6] = [
    "package.json",
    "LICENSE",
    "CODEOWNERS",
    ".gitignore",
    ".npmrc",
    ".editorconfig",
];

pub fn is_docs_path(path: &str) -> bool {
    let basename = path.rsplit('/').next().unwrap_or(path);
    let dot = basename.rfind('.');

    if let Some(dot) = dot {
        if dot > 0 && DOCS_EXTENSIONS.contains(&basename[dot..].to_lowercase().as_str()) {
            return true;
        }
    }

    DOCS_BASENAMES.contains(&basename)
}

// The pairing key that lets a source file share a unit with its test:
// strip a leading src/ or test(s)/ segment, drop the extension and a trailing
// .test/.spec suffix, then flatten the remaining segments with "-".
// "src/cli/roles.ts" and "test/cli-roles.test.ts" both reduce to "cli-roles".
pub fn review_unit_key(path: &str) -> String {
    let segments: Vec<&str> = path.split('/').collect();
    let stripped: Vec<&str> = if segments[0] == "src" || segments[0] == "test" || segments[0] == "tests" {
        segments[1..].to_vec()
    } else {
        segments
    };
    let basename = stripped.last().copied().unwrap_or("");
    let dot = basename.rfind('.');
    let stem = if let Some(dot) = dot {
        if dot > 0 {
            &basename[..dot]
        } else {
            basename
        }
    } else {
        basename
    };
    let unsuffixed = if let Some(stripped_stem) = stem.strip_suffix(".test") {
        stripped_stem
    } else if let Some(stripped_stem) = stem.strip_suffix(".spec") {
        stripped_stem
    } else {
        stem
    };

    let mut parts: Vec<&str> = stripped[..stripped.len() - 1].to_vec();
    parts.push(unsuffixed);
    parts.join("-")
}

fn top_level_directory(path: &str) -> &str {
    path.split('/').next().unwrap_or(path)
}

pub struct DiffFile<'a> {
    pub path: &'a str,
    pub diff_lines: u32,
}

// The directory a group compares on: a source/test pair spans "src" and
// "test", so judge it by the source file's top-level directory, falling back
// to the group's first path (a test-only group lives under "test").
pub fn group_directory(group: &[&DiffFile<'_>]) -> String {
    let source = group
        .iter()
        .find(|file| {
            let top = top_level_directory(file.path);
            top != "test" && top != "tests"
        })
        .unwrap_or(&group[0]);
    top_level_directory(source.path).to_string()
}

// One file keeps the path as its label; otherwise the longest common leading
// directory of the unit's paths, or "mixed" when the paths share none (a
// source file and its test, say).
pub fn unit_label(paths: &[String]) -> String {
    if paths.len() == 1 {
        return paths[0].clone();
    }
    let segments: Vec<Vec<&str>> = paths.iter().map(|p| p.split('/').collect()).collect();
    let first = &segments[0];
    let mut common: Vec<&str> = Vec::new();
    for index in 0..first.len().saturating_sub(1) {
        let segment = first[index];
        if segments.iter().all(|parts| parts.get(index) == Some(&segment)) {
            common.push(segment);
        } else {
            break;
        }
    }
    if !common.is_empty() {
        return format!("{} ({} files)", common.join("/"), paths.len());
    }
    // A source file with its tests shares no prefix; name the unit after the
    // source's directory so the label still says where the change lives.
    let source = paths
        .iter()
        .find(|p| {
            let re = Regex::new(r"^tests?/").unwrap();
            !re.is_match(p)
        })
        .map(String::as_str)
        .unwrap_or_else(|| paths.first().map(String::as_str).unwrap_or(""));
    let source_directory = source.split('/').next_back().map(|_| {
        source.split('/').collect::<Vec<&str>>()[..source.split('/').count() - 1].join("/")
    });
    let source_directory = source_directory.unwrap_or_default();
    format!("{} + tests ({} files)", if source_directory.is_empty() { "mixed" } else { &source_directory }, paths.len())
}

// planReviewUnits (tail — the docs bucket was built above the marker; the
// code-unit grouping follows). A group rides with the current unit while the
// combined unit fits MAX_UNIT_FILES and MAX_UNIT_DIFF_LINES and stays inside
// one top-level directory; an oversized multi-file group splits into one unit
// per file. Source files and their tests travel together while they fit the
// diff budget — a reviewer should see a change and its tests in one prompt —
// but a group over MAX_UNIT_DIFF_LINES splits into one unit per file: the
// benchmark's 851-line settings.ts + test pair exhausted even the oversized
// six-cycle budget without delivering a report, twice in one run
// (docs/review-mode.md, Results).
pub fn plan_review_units(files: &[DiffFile<'_>]) -> Vec<ReviewUnit> {
    let mut docs_units: Vec<ReviewUnit> = Vec::new();
    let docs: Vec<&DiffFile<'_>> = files.iter().filter(|file| is_docs_path(file.path)).collect();

    if !docs.is_empty() {
        docs_units.push(ReviewUnit {
            kind: ReviewUnitKind::Docs,
            label: "docs & manifests".to_string(),
            paths: docs.iter().map(|file| file.path.to_string()).collect(),
            diff_lines: docs.iter().map(|file| file.diff_lines).sum(),
        });
    }

    let emitted: std::collections::HashSet<&str> = docs.iter().map(|file| file.path).collect();
    let mut groups: Vec<Vec<&DiffFile<'_>>> = Vec::new();
    let mut group_by_key: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

    for file in files {
        if emitted.contains(file.path) {
            continue;
        }

        let key = review_unit_key(file.path);
        let group_index = match group_by_key.get(&key) {
            Some(index) => *index,
            None => {
                groups.push(Vec::new());
                let index = groups.len() - 1;
                group_by_key.insert(key, index);
                index
            }
        };

        groups[group_index].push(file);
    }

    let mut code_units: Vec<ReviewUnit> = Vec::new();

    for group in &groups {
        let group_diff_lines: u32 = group.iter().map(|file| file.diff_lines).sum();
        let fits_current_unit = match code_units.last_mut() {
            Some(current) => {
                current.paths.len() + group.len() <= MAX_UNIT_FILES
                    && current.diff_lines + group_diff_lines <= MAX_UNIT_DIFF_LINES
                    && top_level_directory(current.paths.first().map(String::as_str).unwrap_or(""))
                        == group_directory(group)
            }
            None => false,
        };

        if fits_current_unit {
            let current = code_units.last_mut().expect("unit exists");
            if fits_current_unit {
                current.paths.extend(group.iter().map(|file| file.path.to_string()));
                current.diff_lines += group_diff_lines;
                current.label = unit_label(&current.paths);
                continue;
            }
        }

        if group.len() > 1 && group_diff_lines > MAX_UNIT_DIFF_LINES {
            for file in group {
                code_units.push(ReviewUnit {
                    kind: ReviewUnitKind::Code,
                    label: unit_label(std::slice::from_ref(&file.path.to_string())),
                    paths: vec![file.path.to_string()],
                    diff_lines: file.diff_lines,
                });
            }
            continue;
        }

        let paths: Vec<String> = group.iter().map(|file| file.path.to_string()).collect();
        code_units.push(ReviewUnit {
            kind: ReviewUnitKind::Code,
            label: unit_label(&paths),
            paths,
            diff_lines: group_diff_lines,
        });
    }

    docs_units.extend(code_units);
    docs_units
}

// The per-unit prompt: the single-file template generalised to one "### File:"
// block per path. The docs unit is asked for a skim — the audit's docs
// reviewers spent ~150s fact-checking a changelog line by line and reported
// column alignment as findings; prose earns a P0–P2 only when it is wrong.
// Files up to this many lines ride into the prompt in full (numbered like READ
// output), so the reviewer cites lines from text it already holds instead of
// spending tool rounds re-reading the hunk's surroundings: the audit counted
// ~10 READ calls per file, most of them on the file under review.
pub const MAX_INLINE_FILE_LINES: usize = 300;

// A child's budget, scaled to its unit. The harness cap is in cycles of up
// to four tool rounds; the tool-call figure is the soft budget the prompt
// states. Four cycles / 12 calls fit every ordinary unit on the benchmark
// (its children used 4–8 calls under this budget), but an oversized unit — a
// source file and its tests kept together past MAX_UNIT_DIFF_LINES — ran out
// of cycles before delivering and had to be retried, so it keeps the older
// six-cycle budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReviewChildBudget {
    pub cycles: u32,
    pub tool_calls: u32,
}

pub const REVIEW_CHILD_BUDGET: ReviewChildBudget = ReviewChildBudget { cycles: 4, tool_calls: 12 };
pub const REVIEW_CHILD_BUDGET_OVERSIZED: ReviewChildBudget = ReviewChildBudget { cycles: 6, tool_calls: 18 };

pub fn review_child_budget(unit: &ReviewUnit) -> ReviewChildBudget {
    if unit.kind == ReviewUnitKind::Code && unit.diff_lines > MAX_UNIT_DIFF_LINES {
        REVIEW_CHILD_BUDGET_OVERSIZED
    } else {
        REVIEW_CHILD_BUDGET
    }
}

pub struct UnitPromptFile<'a> {
    pub path: &'a str,
    pub diff: &'a str,
    pub content: Option<&'a str>,
}

pub struct UnitReviewPromptArgs<'a> {
    pub base_ref: &'a str,
    pub context: &'a str,
    pub files: &'a [UnitPromptFile<'a>],
    pub unit: &'a ReviewUnit,
}

fn numbered(content: &str) -> String {
    content
        .strip_suffix('\n')
        .unwrap_or(content)
        .split('\n')
        .enumerate()
        .map(|(i, line)| format!("{}\t{}", i + 1, line))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn build_unit_review_prompt(args: UnitReviewPromptArgs<'_>) -> String {
    let docs = args.unit.kind == ReviewUnitKind::Docs;
    let many = args.files.len() > 1;
    let inlined_count = args.files.iter().filter(|file| file.content.is_some()).count();
    let mut diffs: Vec<String> = Vec::new();

    for file in args.files {
        let mut block = vec![format!("### {}", file.path), "```diff".to_string(), file.diff.to_string(), "```".to_string()];

        if let Some(content) = file.content {
            let line_count = content.strip_suffix('\n').unwrap_or(content).split('\n').count();
            block.push(format!(
                "Full file after the change ({} lines, numbered — do not READ it again):",
                line_count
            ));
            block.push("```".to_string());
            block.push(numbered(content));
            block.push("```".to_string());
        }

        block.push(String::new());
        diffs.push(block.join("\n"));
    }

    let mut output_blocks: Vec<String> = Vec::new();

    for file in args.files {
        output_blocks.extend([
            format!("### File: {}", file.path),
            "**Role**: <one sentence>".to_string(),
            "**Changes**: <one or two sentences>".to_string(),
            "**Rating**: (one of ✅ / ⚠️ / ❌)".to_string(),
            "**Issues**:".to_string(),
            "- <P0|P1|P2> (line NN): <what is wrong and why it matters> — evidence: `<the offending line, quoted>`".to_string(),
            "**Context**: <at most three sentences: what you verified (callers, tests) and the one residual risk, if any>".to_string(),
            String::new(),
        ]);
    }

    let mut lines: Vec<String> = vec![
        if docs {
            format!(
                "You are reviewing the documentation and manifest files of a larger change ({} file(s)). Work read-only — do not edit anything.",
                args.files.len()
            )
        } else if many {
            format!(
                "You are a code reviewer for {} related files of a larger change. Work read-only: read what you need, GREP for callers, dependents, and tests, and judge the diffs — do not edit anything.",
                args.files.len()
            )
        } else {
            "You are a code reviewer for a single file of a larger change. Work read-only: read what you need, GREP for callers, dependents, and tests, and judge the diffs — do not edit anything.".to_string()
        },
        String::new(),
        "## What this change is trying to achieve (the intent — judge whether the change serves this goal, not only whether it is correct)".to_string(),
        args.context.to_string(),
        String::new(),
        if many { "## The diffs under review (one per file)".to_string() } else { "## The diff under review".to_string() },
    ];
    lines.extend(diffs);
    lines.push("## Method".to_string());

    if docs {
        lines.extend([
            "1. Skim each file's hunk for claims about the code (names, defaults, commands, behaviour) and verify the load-bearing ones with at most a few GREP/READ calls — do not re-derive the whole changelog.".to_string(),
            "2. Report only prose that is WRONG or misleading about the change (P0–P2). Wording, column alignment, plural/singular, and tone are not findings.".to_string(),
            "3. A version bump or a manifest edit that matches the change is a clean file: say so in one line.".to_string(),
            "4. Cite a file path or symbol only if you opened it in this session; otherwise write \"not verified\" — a made-up path is worse than no citation.".to_string(),
        ]);
    } else {
        lines.push(if inlined_count == args.files.len() {
            "1. The full file(s) are above — read them there. READ other files only when a finding depends on them.".to_string()
        } else {
            "1. READ each full file that is not included above, not just the hunk — hunks hide surrounding invariants.".to_string()
        });
        lines.push("2. GREP for callers, dependents, and tests of the changed code before judging impact — one or two targeted greps, not a survey.".to_string());
        lines.push("3. When a hunk widens, narrows, or duplicates a predicate, constant, mapping, or gate (a `startsWith`, an allowlist, a provider check), GREP for its other copies: a sibling left on the old rule is a P1 — the two paths now disagree — and the diff never shows it.".to_string());
        if many {
            lines.push("4. These files were bundled because they are related (a source file and its tests, or one directory): judge them together — a test that no longer pins what the source changed is a finding.".to_string());
        }
        lines.push(format!(
            "{}. Judge every finding against the stated intent above: a technically clean change that misses the goal is still a finding.",
            if many { 5 } else { 4 }
        ));
        lines.push(format!(
            "{}. Budget: at most {} tool calls in total. Prefer one GREP over READing a whole file; when the budget is spent, write the report from what you have verified — a report delivered on budget beats a survey that never ends.",
            if many { 6 } else { 5 },
            review_child_budget(args.unit).tool_calls
        ));
    }

    lines.push(String::new());
    lines.push("## What counts as a finding".to_string());
    lines.push("- P0 blocks the change: a bug that breaks the stated intent, a security hole, data loss.".to_string());
    lines.push("- P1 important: a logic error, missing error handling on a path that will be hit, a real performance risk, a test that no longer pins the behaviour it names.".to_string());
    lines.push("- P2 worth fixing before merge: a missing test for new behaviour, a misleading contract or comment, a pattern the codebase avoids.".to_string());
    lines.push("- Nothing below P2 is a finding. Do not list naming, wording, formatting, comment alignment, or style preferences at all — not as P3, not as P4, not in the Context.".to_string());
    lines.push(String::new());
    lines.push("## Rules".to_string());
    lines.push("- Every finding must be actionable and must quote the line it is about as evidence — never invent code, line numbers, or test names; if you did not read it, you cannot cite it.".to_string());
    lines.push("- Zero findings is a valid and expected result for a clean file: rate it ✅ and write `(none)` under Issues.".to_string());
    lines.push("- Keep Role, Changes, and Context short (see the template) — the synthesis pass reads every block; restating what the diff already shows is noise.".to_string());
    if many {
        lines.push("- Produce exactly one \"### File:\" block per file, in the order given, even for a clean file.".to_string());
    }
    lines.push(String::new());
    lines.push("## Output (follow this template exactly)".to_string());
    lines.extend(output_blocks);
    lines.push(String::new());
    lines.push("## Delivery".to_string());
    lines.push(format!(
        "Your task ledger already holds this review as {} — do not call plan_tasks. Deliver the report by calling finish_task {{\"taskId\": \"{}\", \"status\": \"completed\", \"summary\": <the COMPLETE report above, every file block included, verbatim>}} — that summary is what gets parsed. Text you write alongside a tool call is not recorded, so never rely on saying the report. Read, grep, then finish_task with the report.",
        REVIEW_TASK_ID, REVIEW_TASK_ID
    ));
    if many {
        let paths: Vec<String> = args.files.iter().map(|file| file.path.to_string()).collect();
        lines.push(delivery_checklist(&paths));
    }

    lines.join("\n")
}

// The last thing a bundled reviewer reads before it delivers: the exact
// headings its summary must contain. Reviewers of 2–4 file units skipped a
// block in three of four benchmark runs — usually the file they judged
// clean — and each skip costs a full retry child (11–215 s); the rule near
// the top of the prompt was not enough on its own.
pub fn delivery_checklist(paths: &[String]) -> String {
    let headings: Vec<String> = paths.iter().map(|path| format!("\"### File: {path}\"")).collect();
    format!(
        "Before calling finish_task, check the summary contains all {} of these headings, in this order — a missing one is re-reviewed from scratch: {}. A clean file still gets its block (Rating ✅, Issues (none)).",
        paths.len(),
        headings.join(", ")
    )
}

pub fn unit_review_task_title(unit: &ReviewUnit) -> String {
    if unit.paths.len() == 1 {
        file_review_task_title(unit.paths.first().map(String::as_str).unwrap_or(&unit.label))
    } else {
        format!(
            "Review {}: {} — read them in full, grep their callers and tests, then finish_task completed with one \"### File:\" block per file as the summary",
            unit.label,
            unit.paths.join(", ")
        )
    }
}

// Splits a unit's report back into per-file sections keyed by path, so the
// JSON contract stays per file. A path the model skipped (or misnamed) maps
// to null — the caller decides how to represent the gap.
pub fn split_unit_report(report: &str, paths: &[String]) -> Vec<(String, Option<String>)> {
    let mut sections: Vec<(String, Option<String>)> = paths.iter().map(|path| (path.clone(), None)).collect();
    let header = file_header_re();
    let matches: Vec<(usize, String)> = header
        .captures_iter(report)
        .map(|caps| {
            let whole = caps.get(0).expect("header match");
            let named = caps.get(1).map(|g| g.as_str().trim().trim_start_matches('`').trim_end_matches('`')).unwrap_or("");
            (whole.start(), named.to_string())
        })
        .collect();

    for (i, (start, named)) in matches.iter().enumerate() {
        let start = *start;
        let end = matches.get(i + 1).map_or(report.len(), |(next_start, _)| *next_start);
        // Exact first; otherwise a suffix match at a path-segment boundary, and
        // only when exactly one candidate matches — "a.ts" against a bundled
        // src/a.ts + test/a.ts pair is ambiguous and must not bind to either.
        let by_suffix: Vec<&String> = paths
            .iter()
            .filter(|candidate| candidate.as_str() != named && (candidate.ends_with(&format!("/{named}")) || named.ends_with(&format!("/{}", candidate))))
            .collect();
        let path = paths
            .iter()
            .find(|candidate| candidate.as_str() == named)
            .or_else(|| if by_suffix.len() == 1 { by_suffix.first().copied() } else { None });

        if let Some(path) = path {
            if let Some(slot) = sections.iter_mut().find(|(bound, _)| bound == path) {
                if slot.1.is_none() {
                    slot.1 = Some(report[start..end].trim().to_string());
                }
            }
        }
    }

    sections
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewSynthesisMode {
    Auto,
    Always,
    Never,
}

pub struct SynthesisSkipFile<'a> {
    pub errored: bool,
    pub p0: u32,
    pub p1: u32,
    pub p2: u32,
    pub rating: Option<&'a str>,
    pub rating_derived: bool,
}

// Whether the holistic pass has anything to synthesize. In auto mode a review
// where every unit came back clean — no findings, nothing errored, every
// file rated — skips the stronger model entirely: the synthesizer's job is
// to deduplicate findings and judge goal fit, and with zero findings the
// first is empty and the second was already answered per file against the
// same intent. One errored or unrated file is a gap, not a clean bill, so
// the synthesis still runs to read around it.
pub fn should_skip_synthesis(mode: ReviewSynthesisMode, files: &[SynthesisSkipFile<'_>]) -> bool {
    if mode == ReviewSynthesisMode::Never {
        return true;
    }

    if mode == ReviewSynthesisMode::Always {
        return false;
    }

    // A derived rating means the reviewer never wrote its Rating line — a
    // truncated answer looks the same — so it is a gap here, not a clean bill.
    files
        .iter()
        .all(|file| !file.errored && file.rating.is_some() && !file.rating_derived && file.p0 + file.p1 + file.p2 == 0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindingLevel {
    P0,
    P1,
    P2,
}

#[derive(Debug, Clone)]
pub struct ExtractedFinding {
    pub level: FindingLevel,
    pub text: String,
}

fn extract_findings_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?m)^\s*[-*]\s*\**(P[012])(?!\d)\**[:\s—–-]*(.*)$").unwrap())
}

// The finding bullets of one per-file report, for a table that lists what
// the per-file reviewers actually reported when no synthesizer runs.
pub fn extract_findings(report: &str) -> Vec<ExtractedFinding> {
    let mut findings: Vec<ExtractedFinding> = Vec::new();

    // Tolerates "- P1 (line 3): …", "- P1: …", and "- **P1** — …"; the lookahead
    // keeps "P10" from reading as P1 with a stray digit in its text.
    for caps in extract_findings_re().captures_iter(report) {
        let level = match caps.get(1).map(|g| g.as_str()) {
            Some("P0") => FindingLevel::P0,
            Some("P1") => FindingLevel::P1,
            _ => FindingLevel::P2,
        };
        let text = caps.get(2).map(|g| g.as_str().trim().replace('|', "\\|")).unwrap_or_default();
        findings.push(ExtractedFinding { level, text });
    }

    findings
}

pub struct SkippedSynthesisFile<'a> {
    pub path: &'a str,
    pub errored: bool,
    pub rating: Option<&'a str>,
    pub rating_derived: bool,
    pub report: Option<&'a str>,
}

pub struct SkippedSynthesisArgs<'a> {
    pub base_ref: &'a str,
    pub files: &'a [SkippedSynthesisFile<'a>],
    pub mode: ReviewSynthesisMode,
    pub unit_count: usize,
}

// The report a skipped synthesis produces: the same top-level sections the
// synthesis template promises, filled from the computed facts, so a reader or
// a parser sees one shape regardless of which path ran.
pub fn build_skipped_synthesis_report(args: SkippedSynthesisArgs<'_>) -> String {
    let reason = if args.mode == ReviewSynthesisMode::Never {
        "synthesis disabled (--synthesis never)"
    } else {
        "every unit came back clean, so the synthesis pass was skipped (--synthesis auto)"
    };
    // The table is built from what the per-file reviewers reported, so it is
    // true under --synthesis never with real findings — not a hardcoded "no
    // findings" beside a 1/5 confidence stamp. A file whose section never
    // bound carries the whole unit report as its body (see review.ts); its
    // findings belong to sibling files and are not re-attributed here.
    let bound = |file: &SkippedSynthesisFile<'_>| file.rating.is_some() && !file.report.unwrap_or("").starts_with('(');
    let mut table_rows: Vec<String> = Vec::new();
    let mut index = 0usize;

    for file in args.files {
        if !bound(file) {
            continue;
        }

        for finding in extract_findings(file.report.unwrap_or("")) {
            index += 1;
            let level = match finding.level {
                FindingLevel::P0 => "P0",
                FindingLevel::P1 => "P1",
                FindingLevel::P2 => "P2",
            };
            table_rows.push(format!("| {} | {} | {} | {} |", index, level, file.path, finding.text));
        }
    }

    // The same definition of "not fully reviewed" the skip decision uses: a
    // derived rating means the reviewer never wrote its Rating line.
    let gaps: Vec<String> = args
        .files
        .iter()
        .filter(|file| file.errored || file.rating.is_none() || file.rating_derived)
        .map(|file| file.path.to_string())
        .collect();

    // Checked before `table_rows` is moved into the issues table below.
    let empty = table_rows.is_empty();

    [
        format!("# Code Review: {}...HEAD", args.base_ref),
        "## Summary".to_string(),
        format!(
            "{} file(s) reviewed in {} unit(s); {}. {}.{} Per-file reports follow.",
            args.files.len(),
            args.unit_count,
            reason,
            if table_rows.is_empty() {
                "No P0/P1/P2 findings.".to_string()
            } else {
                format!("{} finding(s) from the per-file reviewers, undeduplicated.", table_rows.len())
            },
            if gaps.is_empty() {
                String::new()
            } else {
                format!(" Not reviewed (errored or unrated): {}.", gaps.join(", "))
            }
        ),
        "## Confidence Score".to_string(),
        "## Issues Table".to_string(),
        "| # | Priority | File | Issue |".to_string(),
        "|---|----------|------|-------|".to_string(),
    ]
    .into_iter()
    .chain(if table_rows.is_empty() {
        vec!["| – | – | – | no findings |".to_string()]
    } else {
        table_rows
    })
    .chain([
        "## Detailed Findings".to_string(),
        if empty {
            "None.".to_string()
        } else {
            "See the per-file reports below — no synthesis pass ran to deduplicate or rank them.".to_string()
        },
    ])
    .collect::<Vec<_>>()
    .join("\n")
}

pub struct RetryUnit {
    pub original_index: usize,
    pub reason: String,
    pub retry_of: String,
    pub unit: ReviewUnit,
}

pub struct RetryRun<'a> {
    pub errored: bool,
    pub missing: &'a [String],
    pub unit: &'a ReviewUnit,
}

// Which units the retry pass runs: an errored unit whole; a unit whose
// reviewer skipped some files' blocks for just those files. Pure so the
// policy is testable apart from the pool that executes it.
pub fn plan_retry_units(runs: &[RetryRun<'_>], diff_lines_of: &dyn Fn(&str) -> u32) -> Vec<RetryUnit> {
    let mut retries: Vec<RetryUnit> = Vec::new();

    // originalIndex, not the label, identifies the run a retry replaces:
    // labels collide ("src/cli (3 files)" twice when one directory fills two
    // units), and a retry merged into the wrong run would clear the wrong
    // errored flag.
    for (original_index, run) in runs.iter().enumerate() {
        if run.errored {
            retries.push(RetryUnit {
                original_index,
                reason: "the child errored or timed out".to_string(),
                retry_of: run.unit.label.clone(),
                unit: run.unit.clone(),
            });
        } else if !run.missing.is_empty() {
            retries.push(RetryUnit {
                original_index,
                reason: format!("no \"### File:\" block for {}", run.missing.join(", ")),
                retry_of: run.unit.label.clone(),
                unit: ReviewUnit {
                    diff_lines: run.missing.iter().map(|path| diff_lines_of(path)).sum(),
                    kind: run.unit.kind,
                    label: if run.missing.len() == 1 {
                        run.missing.first().cloned().unwrap_or_else(|| run.unit.label.clone())
                    } else {
                        format!("{} — {} skipped files", run.unit.label, run.missing.len())
                    },
                    paths: run.missing.to_vec(),
                },
            });
        }
    }

    retries
}

// The workspace tools a reviewer child is offered. Read-only by construction
// (no PATCH), no fan-out (no DELEGATE), and no verification or network
// surface: the audit's children ran VERIFY 33 times, FETCH 27 times, and
// CHECK 6 times — a reviewer's job is to read and grep, and every tool in
// the schema is prompt tokens on every call. BASH stays for read-only git
// (blame, log) that READ/GREP cannot express.
pub const REVIEW_TOOL_NAMES: [&str; 4] = ["READ", "GREP", "DIR", "BASH"];

// The filter itself lives here, not in the orchestrator, so a test can pin
// the behaviour (a tool outside the set never reaches a child) and not just
// the set's contents.
pub fn review_tools_from<T: Clone>(tools: &[T], name_of: &dyn Fn(&T) -> &str) -> Vec<T> {
    tools
        .iter()
        .filter(|tool| REVIEW_TOOL_NAMES.contains(&name_of(tool)))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_well_formed_two_file_report() {
        let file_a = "### File: src/cli/roles.ts\n\n**Rating**: ⚠️\n\n## Issues\n\n- P1 (line 42): unchecked unwrap can panic\n- P2: missing doc comment\n";
        let file_b = "### File: src/core/state.rs\n\n**Rating**: ✅\n\n- P2: consider a stronger type\n";

        let a = parse_file_report(file_a);
        assert_eq!(a.rating.as_deref(), Some("⚠️"));
        assert!(!a.rating_derived);
        assert_eq!((a.p0, a.p1, a.p2), (0, 1, 1));

        let b = parse_file_report(file_b);
        assert_eq!(b.rating.as_deref(), Some("✅"));
        assert!(!b.rating_derived);
        assert_eq!((b.p0, b.p1, b.p2), (0, 0, 1));
    }

    #[test]
    fn missing_rating_line_derives_the_rating_from_counts() {
        let body = "### File: src/cli/roles.ts\n\n## Issues\n\n- P1: an off-by-one in the range\n- P1: another\n- P2: nit\n";
        let report = parse_file_report(body);
        assert_eq!(report.rating.as_deref(), Some("⚠️"));
        assert!(report.rating_derived);
        assert_eq!((report.p0, report.p1, report.p2), (0, 2, 1));

        // A P0 present derives ❌.
        let p0_report = parse_file_report("**Issues**\n\n- P0: data loss on interrupted write\n");
        assert_eq!(p0_report.rating.as_deref(), Some("❌"));
        assert!(p0_report.rating_derived);

        // A body with no report at all stays unrated.
        let errored = parse_file_report("errored: child crashed before writing a report");
        assert_eq!(errored.rating, None);
        assert!(!errored.rating_derived);
        assert_eq!((errored.p0, errored.p1, errored.p2), (0, 0, 0));
    }

    #[test]
    fn confidence_from_counts_follows_the_deterministic_table() {
        // (P0, P1) pairs from the ts table: any P0 pins 1; P1 tiers 0→5, 1-2→4, 3-4→3, 5+→2.
        assert_eq!(confidence_from_counts(0, 0), 5);
        assert_eq!(confidence_from_counts(0, 1), 4);
        assert_eq!(confidence_from_counts(1, 0), 1);
        assert_eq!(confidence_from_counts(2, 3), 1);
    }

    #[test]
    fn p2_only_file_rates_as_the_ts_implementation_does() {
        // Explicit Rating line: taken as parsed, not derived, and P2s never lower
        // the confidence (the table reads P0/P1 only).
        let explicit = parse_file_report(
            "### File: src/core/home.rs\n\n**Rating**: ⚠️\n\n- P2: name shadowing\n- P2: duplicated helper\n",
        );
        assert_eq!(explicit.rating.as_deref(), Some("⚠️"));
        assert!(!explicit.rating_derived);
        assert_eq!((explicit.p0, explicit.p1, explicit.p2), (0, 0, 2));
        assert_eq!(confidence_from_counts(explicit.p0, explicit.p1), 5);

        // Missing the Rating line: a P2-only report derives ⚠️ and stays derived.
        let derived = parse_file_report(
            "### File: src/core/home.rs\n\n- P2: name shadowing\n- P2: duplicated helper\n",
        );
        assert_eq!(derived.rating.as_deref(), Some("⚠️"));
        assert!(derived.rating_derived);
        assert_eq!(confidence_from_counts(derived.p0, derived.p1), 5);
    }
}
