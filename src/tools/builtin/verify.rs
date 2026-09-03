// VERIFY runs a shell command and parses its output into a structured test
// verdict. The parsers (bun test, vitest, pytest, cargo test, go test, tsc)
// are pure functions over the captured output text, so they are unit-tested
// directly at the bottom of this file; the prepare/execute/complete stages
// follow the same shape as read.rs.

use regex::Regex;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

// Input for the VERIFY tool: the command to run and an optional timeout.

/// The structured verdict the execute stage produces
/// and complete() renders. `failed`/`timed_out` are what the harness records
/// as lastVerification.failed, so a failing run must not read as a pass.
#[derive(Debug, Clone, PartialEq)]
pub struct VerifyVerdict {
    pub exit_code: Option<i32>,
    pub failed: i64,
    pub first_failures: Vec<String>,
    pub output: String,
    pub passed: i64,
    pub runner: String,
    pub skipped: i64,
    pub timed_out: bool,
}

/// The shape `parse_verify_output` returns before the execute stage fills in
/// the process-dependent fields (exit_code, output, timed_out).
#[derive(Debug, Clone, PartialEq)]
pub struct VerifyParsed {
    pub runner: String,
    pub passed: i64,
    pub failed: i64,
    pub skipped: i64,
    pub first_failures: Vec<String>,
}

// ---------------------------------------------------------------------------
// Execution helpers (self-contained, mirrors bash-tool pattern)
// ---------------------------------------------------------------------------


/// The OpenAI function definition drip sends for this tool (the
/// {type: "function", function: {...}} envelope).
pub fn definition() -> Value {
    json!({
        "type": "function",
        "function": {
            "description": "Run a shell command and parse its output into a structured test verdict. Detects bun test, vitest, pytest, python unittest, cargo test, go test, and tsc output formats.",
            "name": "VERIFY",
            "parameters": {
                "additionalProperties": false,
                "properties": {
                    "command": {
                        "description": "The shell command to run.",
                        "type": "string"
                    },
                    "timeout": {
                        "description": "Optional timeout in milliseconds. Defaults to 120000.",
                        "type": "number"
                    }
                },
                "required": ["command"],
                "type": "object"
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Parsers
// ---------------------------------------------------------------------------

/// Extract up to N non-empty lines from text that match a predicate.
fn extract_lines(text: &str, predicate: impl Fn(&str) -> bool, max: usize) -> Vec<String> {
    let mut results: Vec<String> = Vec::new();

    for line in text.split('\n') {
        if results.len() >= max {
            break;
        }
        if predicate(line) {
            results.push(line.trim().to_string());
        }
    }

    results
}

// bun test: "X pass" and "X fail" on their own lines
// bun summary lines look like "  3 pass" or "  1 fail" — standalone, no trailing "in Xs"
fn parse_bun_test(output: &str) -> Option<VerifyVerdict> {
    // bun outputs lines like " 3 pass" / " 0 fail" — no trailing "in Xs"
    let pass_match = Regex::new(r"(?im)^\s*(\d+)\s+pass\s*$")
        .unwrap()
        .captures(output);
    let fail_match = Regex::new(r"(?im)^\s*(\d+)\s+fail\s*$")
        .unwrap()
        .captures(output);
    let skip_match = Regex::new(r"(?im)^\s*(\d+)\s+skip\s*$")
        .unwrap()
        .captures(output);

    if pass_match.is_none() && fail_match.is_none() {
        return None;
    }

    let passed = pass_match
        .as_ref()
        .map(|m| m[1].parse().unwrap_or(0))
        .unwrap_or(0);
    let failed = fail_match
        .as_ref()
        .map(|m| m[1].parse().unwrap_or(0))
        .unwrap_or(0);
    let skipped = skip_match
        .as_ref()
        .map(|m| m[1].parse().unwrap_or(0))
        .unwrap_or(0);

    // Collect failing test names: lines starting with "✗" or "× " or "FAIL" markers
    let fail_marker_re = Regex::new(r"^[✗×✕]\s").unwrap();
    let fail_word_re = Regex::new(r"^\s*(FAIL|●)\s+").unwrap();
    let diff_line_re = Regex::new(r"^\s+\d+\s+\|").unwrap();
    let first_failures = extract_lines(
        output,
        |line| {
            fail_marker_re.is_match(line)
                || fail_word_re.is_match(line)
                || diff_line_re.is_match(line)
        },
        5,
    );

    Some(VerifyVerdict {
        exit_code: None,
        failed,
        first_failures,
        output: output.to_string(),
        passed,
        runner: "bun test".to_string(),
        skipped,
        timed_out: false,
    })
}

// vitest: "Tests  N passed | N failed | N skipped"
fn parse_vitest(output: &str) -> Option<VerifyVerdict> {
    // Match the summary line like: "Tests  3 passed | 1 failed | 2 skipped (6)"
    let summary_match = Regex::new(
        r"(?i)Tests\s+(\d+)\s+passed(?:\s+\|\s+(\d+)\s+failed)?(?:\s+\|\s+(\d+)\s+skipped)?",
    )
    .unwrap()
    .captures(output)?;

    let passed = summary_match[1].parse().unwrap_or(0);
    let failed = summary_match
        .get(2)
        .map(|m| m.as_str().parse().unwrap_or(0))
        .unwrap_or(0);
    let skipped = summary_match
        .get(3)
        .map(|m| m.as_str().parse().unwrap_or(0))
        .unwrap_or(0);

    // vitest marks failures with "× " or "FAIL" lines
    let marker_re = Regex::new(r"^\s*[×✕✗]\s").unwrap();
    let fail_re = Regex::new(r"^\s*FAIL\s").unwrap();
    let first_failures = extract_lines(
        output,
        |line| marker_re.is_match(line) || fail_re.is_match(line),
        5,
    );

    Some(VerifyVerdict {
        exit_code: None,
        failed,
        first_failures,
        output: output.to_string(),
        passed,
        runner: "vitest".to_string(),
        skipped,
        timed_out: false,
    })
}

// pytest: "N passed, N failed, N warning"
fn parse_pytest(output: &str) -> Option<VerifyVerdict> {
    // e.g. "5 passed, 2 failed, 1 warning in 0.45s"
    // Find ALL =+...=+ lines and pick the last one that contains pass/fail/error
    let mut all_matches: Vec<String> = Regex::new(r"={3,}\s*(.*?)\s*={3,}")
        .unwrap()
        .captures_iter(output)
        .map(|captures| captures[1].to_string())
        .collect();
    all_matches.reverse();
    let keyword_re = Regex::new(r"(?i)passed|failed|error").unwrap();
    let summary = all_matches.into_iter().find(|m| keyword_re.is_match(m))?;

    if !keyword_re.is_match(&summary) {
        return None;
    }

    let passed_match = Regex::new(r"(\d+)\s+passed").unwrap().captures(&summary);
    let failed_match = Regex::new(r"(\d+)\s+(?:failed|error)")
        .unwrap()
        .captures(&summary);
    let skipped_match = Regex::new(r"(\d+)\s+(?:skipped|warning)")
        .unwrap()
        .captures(&summary);

    let passed = passed_match
        .map(|m| m[1].parse().unwrap_or(0))
        .unwrap_or(0);
    let failed = failed_match
        .map(|m| m[1].parse().unwrap_or(0))
        .unwrap_or(0);
    let skipped = skipped_match
        .map(|m| m[1].parse().unwrap_or(0))
        .unwrap_or(0);

    // pytest FAILED lines: "FAILED test_foo.py::test_bar - ..."
    let failed_line_re = Regex::new(r"^FAILED\s").unwrap();
    let first_failures = extract_lines(output, |line| failed_line_re.is_match(line), 5);

    Some(VerifyVerdict {
        exit_code: None,
        failed,
        first_failures,
        output: output.to_string(),
        passed,
        runner: "pytest".to_string(),
        skipped,
        timed_out: false,
    })
}

// python unittest: "Ran N tests in 0.002s" then "OK", "OK (skipped=1)" or
// "FAILED (failures=1, errors=2, skipped=1)".
fn parse_unittest(output: &str) -> Option<VerifyVerdict> {
    let ran_match = Regex::new(r"(?m)^Ran (\d+) tests? in [\d.]+s$")
        .unwrap()
        .captures(output)?;
    let total: i64 = ran_match[1].parse().unwrap_or(0);
    let status_match = Regex::new(r"(?m)^(OK|FAILED)(?: \(([^)]*)\))?$")
        .unwrap()
        .captures(output)?;

    let mut counts: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    let pair_re = Regex::new(r"^(\w+)=(\d+)$").unwrap();
    for part in status_match
        .get(2)
        .map(|m| m.as_str())
        .unwrap_or("")
        .split(',')
    {
        if let Some(pair) = pair_re.captures(part.trim()) {
            counts.insert(pair[1].to_string(), pair[2].parse().unwrap_or(0));
        }
    }

    let failed = counts.get("failures").copied().unwrap_or(0) + counts.get("errors").copied().unwrap_or(0);
    let skipped = counts.get("skipped").copied().unwrap_or(0);
    let passed = (total - failed - skipped).max(0);
    let failure_line_re = Regex::new(r"^(FAIL|ERROR): ").unwrap();
    let first_failures = extract_lines(output, |line| failure_line_re.is_match(line), 5);

    Some(VerifyVerdict {
        exit_code: None,
        failed,
        first_failures,
        output: output.to_string(),
        passed,
        runner: "unittest".to_string(),
        skipped,
        timed_out: false,
    })
}

// cargo test: "test result: ok. N passed; N failed; N ignored"
fn parse_cargo_test(output: &str) -> Option<VerifyVerdict> {
    let result_match = Regex::new(
        r"(?i)test result:.*?(\d+)\s+passed;\s+(\d+)\s+failed(?:;\s+(\d+)\s+ignored)?",
    )
    .unwrap()
    .captures(output)?;

    let passed = result_match[1].parse().unwrap_or(0);
    let failed = result_match[2].parse().unwrap_or(0);
    let skipped = result_match
        .get(3)
        .map(|m| m.as_str().parse().unwrap_or(0))
        .unwrap_or(0);

    // cargo test failure lines: "test foo::bar ... FAILED"
    let failed_re = Regex::new(r"\bFAILED$").unwrap();
    let first_failures = extract_lines(output, |line| failed_re.is_match(line.trim()), 5);

    Some(VerifyVerdict {
        exit_code: None,
        failed,
        first_failures,
        output: output.to_string(),
        passed,
        runner: "cargo test".to_string(),
        skipped,
        timed_out: false,
    })
}

// go test: count "--- FAIL:" lines, check for "ok" / "FAIL" verdict
fn parse_go_test(output: &str) -> Option<VerifyVerdict> {
    // go test outputs lines like: "--- FAIL: TestFoo (0.00s)"
    // and summary lines like: "ok  \tpkg\t0.001s" or "FAIL\tpkg\t..."
    let fail_re = Regex::new(r"^--- FAIL:").unwrap();
    let pass_re = Regex::new(r"^--- PASS:").unwrap();
    let fail_lines: Vec<&str> = output
        .split('\n')
        .filter(|line| fail_re.is_match(line))
        .collect();
    let pass_lines: Vec<&str> = output
        .split('\n')
        .filter(|line| pass_re.is_match(line))
        .collect();
    let has_verdict = Regex::new(r"(?m)^(ok|FAIL)\s").unwrap().is_match(output);

    if !has_verdict && fail_lines.is_empty() && pass_lines.is_empty() {
        return None;
    }

    let passed = pass_lines.len() as i64;
    let failed = fail_lines.len() as i64;
    // go test doesn't report skipped count explicitly
    let skipped = 0;

    let first_failures: Vec<String> = fail_lines
        .iter()
        .take(5)
        .map(|line| line.trim().to_string())
        .collect();

    Some(VerifyVerdict {
        exit_code: None,
        failed,
        first_failures,
        output: output.to_string(),
        passed,
        runner: "go test".to_string(),
        skipped,
        timed_out: false,
    })
}

// tsc: count the compiler error lines in the output
fn parse_tsc(output: &str) -> Option<VerifyVerdict> {
    let error_line_re = Regex::new(r"error TS\d+").unwrap();
    let error_lines: Vec<&str> = output
        .split('\n')
        .filter(|line| error_line_re.is_match(line))
        .collect();

    // tsc has no explicit success output — this parser returns None unless the
    // output contains compiler error lines; a clean pass (tsc command, no
    // errors) is detected separately by command hint (see parse_tsc_clean)
    if error_lines.is_empty() && !output.contains("error TS") {
        return None;
    }

    let failed = error_lines.len() as i64;
    let passed = if failed == 0 { 1 } else { 0 }; // 1 "passed" = no errors
    let first_failures: Vec<String> = error_lines
        .iter()
        .take(5)
        .map(|line| line.trim().to_string())
        .collect();

    Some(VerifyVerdict {
        exit_code: None,
        failed,
        first_failures,
        output: output.to_string(),
        passed,
        runner: "tsc".to_string(),
        skipped: 0,
        timed_out: false,
    })
}

// tsc clean pass: command was tsc and the output has no compiler errors —
// detect by command hint
fn parse_tsc_clean(command: &str, output: &str) -> Option<VerifyVerdict> {
    if !Regex::new(r"\btsc\b").unwrap().is_match(command) {
        return None;
    }
    if Regex::new(r"error TS\d+").unwrap().is_match(output) {
        return None; // handled by parse_tsc
    }

    // tsc exits 0 with no output on clean pass
    Some(VerifyVerdict {
        exit_code: None,
        failed: 0,
        first_failures: vec![],
        output: output.to_string(),
        passed: 1,
        runner: "tsc".to_string(),
        skipped: 0,
        timed_out: false,
    })
}

/// Pure, no process execution: tries each runner
/// parser in order and returns the first match's counts; falls back to the
/// tsc-clean-pass check (recognized by the command text), then to the
/// "unknown" runner.
pub fn parse_verify_output(command: &str, output: &str) -> VerifyParsed {
    let parsers: [fn(&str) -> Option<VerifyVerdict>; 7] = [
        parse_bun_test,
        parse_vitest,
        parse_pytest,
        parse_unittest,
        parse_cargo_test,
        parse_go_test,
        parse_tsc,
    ];

    for parser in parsers {
        if let Some(result) = parser(output) {
            return VerifyParsed {
                runner: result.runner,
                passed: result.passed,
                failed: result.failed,
                skipped: result.skipped,
                first_failures: result.first_failures,
            };
        }
    }

    // tsc clean pass (no output, recognized by command)
    if let Some(tsc_clean) = parse_tsc_clean(command, output) {
        return VerifyParsed {
            runner: tsc_clean.runner,
            passed: tsc_clean.passed,
            failed: tsc_clean.failed,
            skipped: tsc_clean.skipped,
            first_failures: tsc_clean.first_failures,
        };
    }

    VerifyParsed {
        runner: "unknown".to_string(),
        passed: 0,
        failed: 0,
        skipped: 0,
        first_failures: vec![],
    }
}

// ---------------------------------------------------------------------------
// Tests for parse_verify_output (the pure parsing cases; end-to-end cases
// that drive real processes belong to the harness loop instead)
// ---------------------------------------------------------------------------

// ===================== VERIFY tool stages =====================

pub const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MIN_TIMEOUT_MS: u64 = 1_000;
const MAX_TIMEOUT_MS: u64 = 3_600_000;

pub struct VerifyToolInput {
    pub command: String,
    pub cwd: String,
    pub timeout_ms: u64,
}

pub struct VerifyToolPrepared {
    pub input: VerifyToolInput,
    pub display_input: String,
}

/// Run a shell command and parse its output into a structured test verdict. Detects bun test, vitest, pytest, python unittest, cargo test, go test, and tsc output formats.
pub fn prepare(args: &serde_json::Value, ctx: &super::ToolCtx) -> anyhow::Result<VerifyToolPrepared> {
    let args = super::tool_arguments(args)?;

    let raw_command = args.get("command").and_then(|v| v.as_str());
    if raw_command.map_or(true, |s| s.trim().is_empty()) {
        return Err(anyhow::anyhow!("Missing required string argument \"command\"."));
    }
    let command = raw_command.unwrap().trim().to_string();

    let policy_verdict = crate::tools::command_policy::evaluate_command_policy(
        &command,
        &ctx.cwd.to_string_lossy(),
    );

    if policy_verdict.verdict() == "block"
        && !crate::tools::command_policy::allow_destructive_enabled()
    {
        let (rule, why) = match &policy_verdict {
            crate::tools::command_policy::CommandPolicyVerdict::Block { rule, why } => {
                (rule.clone(), why.clone())
            }
            crate::tools::command_policy::CommandPolicyVerdict::Allow => {
                (String::new(), String::new())
            }
        };
        return Err(anyhow::anyhow!(crate::tools::command_policy::format_policy_refusal(
            &command,
            &rule,
            &why
        )));
    }

    let raw_timeout =
        crate::tools::helpers::get_optional_number_argument(&args, "timeout")?;
    let timeout_ms = match raw_timeout {
        None => DEFAULT_TIMEOUT_MS,
        Some(raw) => {
            (raw.floor() as i64).clamp(MIN_TIMEOUT_MS as i64, MAX_TIMEOUT_MS as i64) as u64
        }
    };

    let display_input = format!("command={}", command);
    Ok(VerifyToolPrepared {
        input: VerifyToolInput {
            command,
            cwd: ctx.cwd.to_string_lossy().to_string(),
            timeout_ms,
        },
        display_input,
    })
}

pub fn execute_prepared(prepared: &VerifyToolPrepared) -> anyhow::Result<VerifyVerdict> {
    use crate::tools::child_process::{build_combined_output, run_captured_process, CapturedProcessArgs};
    let input = &prepared.input;

    let captured = run_captured_process(&CapturedProcessArgs {
        command: "bash",
        cwd: Some(input.cwd.as_str()),
        env: None,
        process_args: &["-lc".to_string(), input.command.clone()],
        timeout_ms: Some(input.timeout_ms),
    })
    .map_err(|e| anyhow::anyhow!(e))?;

    let combined_output = build_combined_output(&captured.stdout, &captured.stderr);
    let parsed = parse_verify_output(&input.command, &combined_output);

    Ok(VerifyVerdict {
        runner: parsed.runner,
        passed: parsed.passed,
        failed: parsed.failed,
        skipped: parsed.skipped,
        first_failures: parsed.first_failures,
        exit_code: captured.exit_code,
        output: combined_output,
        timed_out: captured.timed_out,
    })
}

pub fn complete(prepared: &VerifyToolPrepared, verdict: &VerifyVerdict) -> super::ToolCompletion {
    let verdict_line = format!(
        "VERIFY {}: {} passed, {} failed (exit {})",
        verdict.runner,
        verdict.passed,
        verdict.failed,
        verdict
            .exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "null".to_string())
    );
    let mut body_lines: Vec<String> = vec![verdict_line.clone()];

    if !verdict.first_failures.is_empty() {
        body_lines.push(String::new());
        body_lines.push("First failures:".to_string());
        for f in &verdict.first_failures {
            body_lines.push(format!("  {}", f));
        }
    }

    if !verdict.output.is_empty() {
        body_lines.push(String::new());
        body_lines.push(verdict.output.clone());
    }

    let tool_content = body_lines.join("\n");

    super::ToolCompletion {
        blocks: vec![super::ToolCompletionBlock {
            code: tool_content.clone(),
            description: verdict_line,
            language: "text".to_string(),
            path: std::path::PathBuf::from(&prepared.input.cwd),
        }],
        tool_content,
    }
}

/// The transcript's display string for this call — the `display_input` value
/// prepare() produces — or None when the arguments do not parse (the execute
/// path reports that error).
pub fn display_input(args: &serde_json::Value, ctx: &super::ToolCtx) -> Option<String> {
    prepare(args, ctx).ok().map(|prepared| prepared.display_input)
}

pub fn execute(args: &serde_json::Value, ctx: &super::ToolCtx) -> super::ToolOutcome {
    let prepared = match prepare(args, ctx) {
        Ok(p) => p,
        Err(e) => return super::ToolOutcome::error(e),
    };

    let verdict = match execute_prepared(&prepared) {
        Ok(v) => v,
        Err(e) => return super::ToolOutcome::error(e),
    };

    let completion = complete(&prepared, &verdict);
    // The tool call itself must read failed when the verification failed —
    // this status is what the harness records as lastVerification.failed,
    // so a text-only verdict would let a failing run register as a pass.
    let failed =
        verdict.timed_out || verdict.failed > 0 || (verdict.exit_code.unwrap_or(1)) != 0;
    if failed {
        super::ToolOutcome {
            text: completion.tool_content,
            failed: true,
        }
    } else {
        super::ToolOutcome::success(completion.tool_content)
    }
}


mod tests {
    use super::parse_verify_output;

    // -- bun test ----------------------------------------------------------

    #[test]
    fn parses_passing_bun_test_output() {
        let output = "bun test v1.1.0 (abc1234)\n\nsrc/foo.test.ts:\n✓ adds numbers (2ms)\n✓ subtracts numbers\n\n 3 pass\n 0 fail\n\nRan 3 tests across 1 files. [10ms]";

        let result = parse_verify_output("bun test src/foo.test.ts", output);
        assert_eq!(result.runner, "bun test");
        assert_eq!(result.passed, 3);
        assert_eq!(result.failed, 0);
        assert_eq!(result.skipped, 0);
        assert_eq!(result.first_failures, Vec::<String>::new());
    }

    #[test]
    fn parses_failing_bun_test_output_with_first_failures() {
        let output = "bun test v1.1.0 (abc1234)\n\nsrc/foo.test.ts:\n✓ adds numbers (1ms)\n✗ subtracts numbers\n  Expected: 5\n  Received: 4\n\n 1 pass\n 1 fail\n\nRan 2 tests across 1 files. [8ms]";

        let result = parse_verify_output("bun test src/foo.test.ts", output);
        assert_eq!(result.runner, "bun test");
        assert_eq!(result.passed, 1);
        assert_eq!(result.failed, 1);
        assert!(result.first_failures.len() >= 1);
        assert!(result.first_failures[0].contains("subtracts numbers"));
    }

    #[test]
    fn parses_bun_test_output_with_skips() {
        let output = " 5 pass\n 0 fail\n 2 skip";

        let result = parse_verify_output("bun test", output);
        assert_eq!(result.runner, "bun test");
        assert_eq!(result.passed, 5);
        assert_eq!(result.failed, 0);
        assert_eq!(result.skipped, 2);
    }

    // -- vitest ------------------------------------------------------------

    #[test]
    fn parses_all_passing_vitest_output() {
        let output = " ✓ src/math.test.ts (3)\n   ✓ adds correctly\n   ✓ subtracts correctly\n   ✓ multiplies correctly\n\n Test Files  1 passed (1)\n Tests  3 passed (3)\n Duration  42ms";

        let result = parse_verify_output("vitest run", output);
        assert_eq!(result.runner, "vitest");
        assert_eq!(result.passed, 3);
        assert_eq!(result.failed, 0);
        assert_eq!(result.skipped, 0);
    }

    #[test]
    fn parses_vitest_output_with_failures() {
        let output = " FAIL src/math.test.ts\n   × subtracts correctly\n   ✓ adds correctly\n\n Test Files  1 failed (1)\n Tests  1 passed | 1 failed (2)\n Duration  55ms";

        let result = parse_verify_output("vitest run", output);
        assert_eq!(result.runner, "vitest");
        assert_eq!(result.passed, 1);
        assert_eq!(result.failed, 1);
        assert!(result.first_failures.len() >= 1);
    }

    #[test]
    fn parses_vitest_output_with_skipped() {
        let output = " Tests  4 passed | 2 skipped (6)";

        let result = parse_verify_output("vitest run", output);
        assert_eq!(result.runner, "vitest");
        assert_eq!(result.passed, 4);
        assert_eq!(result.failed, 0);
        assert_eq!(result.skipped, 2);
    }

    // -- pytest ------------------------------------------------------------

    #[test]
    fn parses_a_passing_unittest_run() {
        let output = ".....\n----------------------------------------------------------------------\nRan 5 tests in 0.002s\n\nOK";

        let result = parse_verify_output("python3 -m unittest scripts/test_ab_lanes.py", output);
        assert_eq!(result.runner, "unittest");
        assert_eq!(result.passed, 5);
        assert_eq!(result.failed, 0);
        assert_eq!(result.skipped, 0);
    }

    #[test]
    fn unittest_counts_failures_and_errors_and_lists_their_lines() {
        let output = "F.Es\n======================================================================\nERROR: test_boom (t.T.test_boom)\n----------------------------------------------------------------------\nTraceback (most recent call last):\n  ZeroDivisionError: division by zero\n======================================================================\nFAIL: test_add (t.T.test_add)\n----------------------------------------------------------------------\nAssertionError: 3 != 5\n----------------------------------------------------------------------\nRan 4 tests in 0.003s\n\nFAILED (failures=1, errors=1, skipped=1)";

        let result = parse_verify_output("python3 -m unittest", output);
        assert_eq!(result.runner, "unittest");
        assert_eq!(result.passed, 1);
        assert_eq!(result.failed, 2);
        assert_eq!(result.skipped, 1);
        assert_eq!(
            result.first_failures,
            vec!["ERROR: test_boom (t.T.test_boom)".to_string(), "FAIL: test_add (t.T.test_add)".to_string()]
        );
    }

    #[test]
    fn help_output_that_mentions_tests_is_not_a_unittest_run() {
        let result = parse_verify_output(
            "python3 scripts/ab-lanes.py --help",
            "usage: ab-lanes.py [-h] --spec SPEC\n\nRan tests are summarized per lane.",
        );
        assert_eq!(result.runner, "unknown");
    }

    #[test]
    fn parses_all_passing_pytest_output() {
        let output = "============================= test session starts ==============================\nplatform linux -- Python 3.11.0\ncollected 5 items\n\ntest_math.py .....                                                       [100%]\n\n============================== 5 passed in 0.45s ==============================";

        let result = parse_verify_output("pytest", output);
        assert_eq!(result.runner, "pytest");
        assert_eq!(result.passed, 5);
        assert_eq!(result.failed, 0);
        assert_eq!(result.skipped, 0);
    }

    #[test]
    fn parses_pytest_output_with_failures() {
        let output = "============================= test session starts ==============================\ncollected 4 items\n\ntest_math.py ..FF                                                        [ 75%]\n\nFAILED test_math.py::test_subtract - AssertionError: assert 3 == 5\nFAILED test_math.py::test_divide - ZeroDivisionError\n\n=========================== 2 failed, 2 passed in 1.2s ==========================";

        let result = parse_verify_output("pytest test_math.py", output);
        assert_eq!(result.runner, "pytest");
        assert_eq!(result.passed, 2);
        assert_eq!(result.failed, 2);
        assert!(result
            .first_failures
            .contains(&"FAILED test_math.py::test_subtract - AssertionError: assert 3 == 5".to_string()));
        assert!(result
            .first_failures
            .contains(&"FAILED test_math.py::test_divide - ZeroDivisionError".to_string()));
    }

    #[test]
    fn parses_pytest_output_with_skipped() {
        let output = "=================== 3 passed, 1 skipped, 1 warning in 0.8s ====================";

        let result = parse_verify_output("pytest", output);
        assert_eq!(result.runner, "pytest");
        assert_eq!(result.passed, 3);
        assert_eq!(result.failed, 0);
        assert_eq!(result.skipped, 1);
    }

    // -- cargo test ----------------------------------------------------------

    #[test]
    fn parses_all_passing_cargo_test_output() {
        let output = "running 4 tests\ntest math::test_add ... ok\ntest math::test_sub ... ok\ntest math::test_mul ... ok\ntest math::test_div ... ok\n\ntest result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out";

        let result = parse_verify_output("cargo test", output);
        assert_eq!(result.runner, "cargo test");
        assert_eq!(result.passed, 4);
        assert_eq!(result.failed, 0);
        assert_eq!(result.skipped, 0);
    }

    #[test]
    fn parses_cargo_test_output_with_failures_and_ignored() {
        let output = "running 5 tests\ntest math::test_add ... ok\ntest math::test_sub ... FAILED\ntest math::test_mul ... ok\ntest math::test_div ... ignored\ntest math::test_rem ... FAILED\n\nfailures:\n\n---- math::test_sub stdout ----\nthread 'main' panicked at 'assertion failed'\n\ntest result: FAILED. 2 passed; 2 failed; 1 ignored; 0 measured";

        let result = parse_verify_output("cargo test", output);
        assert_eq!(result.runner, "cargo test");
        assert_eq!(result.passed, 2);
        assert_eq!(result.failed, 2);
        assert_eq!(result.skipped, 1);
        assert_eq!(result.first_failures.len(), 2);
        assert!(result.first_failures[0].contains("FAILED"));
    }

    // -- go test -------------------------------------------------------------

    #[test]
    fn parses_all_passing_go_test_output_ok_verdict() {
        let output = "--- PASS: TestAdd (0.00s)\n--- PASS: TestSub (0.00s)\n--- PASS: TestMul (0.00s)\nok  \texample.com/math\t0.003s";

        let result = parse_verify_output("go test ./...", output);
        assert_eq!(result.runner, "go test");
        assert_eq!(result.passed, 3);
        assert_eq!(result.failed, 0);
        assert_eq!(result.skipped, 0);
        assert_eq!(result.first_failures, Vec::<String>::new());
    }

    #[test]
    fn parses_go_test_output_with_failures() {
        let output = "--- PASS: TestAdd (0.00s)\n--- FAIL: TestSub (0.00s)\n    math_test.go:15: Expected 3 got 5\n--- FAIL: TestDiv (0.00s)\n    math_test.go:22: Division by zero\nFAIL\texample.com/math\t0.002s";

        let result = parse_verify_output("go test ./...", output);
        assert_eq!(result.runner, "go test");
        assert_eq!(result.passed, 1);
        assert_eq!(result.failed, 2);
        assert!(result.first_failures.contains(&"--- FAIL: TestSub (0.00s)".to_string()));
        assert!(result.first_failures.contains(&"--- FAIL: TestDiv (0.00s)".to_string()));
    }

    #[test]
    fn parses_go_test_with_only_the_fail_verdict_line_no_individual_fail_lines() {
        let output = "FAIL\texample.com/math\t[build failed]";

        let result = parse_verify_output("go test ./...", output);
        assert_eq!(result.runner, "go test");
        assert_eq!(result.passed, 0);
        assert_eq!(result.failed, 0); // no --- FAIL: lines
    }

    // -- tsc -----------------------------------------------------------------

    #[test]
    fn parses_tsc_output_with_errors() {
        let output = "src/foo.ts(10,5): error TS2322: Type 'string' is not assignable to type 'number'.\nsrc/bar.ts(3,1): error TS2345: Argument of type 'X' is not assignable to parameter of type 'Y'.";

        let result = parse_verify_output("bunx tsc --noEmit", output);
        assert_eq!(result.runner, "tsc");
        assert_eq!(result.passed, 0);
        assert_eq!(result.failed, 2);
        assert_eq!(result.first_failures.len(), 2);
        assert!(result.first_failures[0].contains("error TS2322"));
        assert!(result.first_failures[1].contains("error TS2345"));
    }

    #[test]
    fn parses_tsc_clean_pass_empty_output_with_tsc_in_command() {
        let result = parse_verify_output("bunx tsc --noEmit", "");
        assert_eq!(result.runner, "tsc");
        assert_eq!(result.passed, 1);
        assert_eq!(result.failed, 0);
        assert_eq!(result.first_failures, Vec::<String>::new());
    }

    #[test]
    fn caps_first_failures_at_5_errors() {
        let errors: Vec<String> = (0..8)
            .map(|i| format!("src/file{i}.ts(1,1): error TS2304: Cannot find name 'x{i}'."))
            .collect();
        let output = errors.join("\n");

        let result = parse_verify_output("tsc", &output);
        assert_eq!(result.runner, "tsc");
        assert_eq!(result.failed, 8);
        assert_eq!(result.first_failures.len(), 5);
    }

    // -- unknown ---------------------------------------------------------------

    #[test]
    fn returns_runner_unknown_for_unrecognized_output() {
        let result = parse_verify_output("echo hello", "hello world");
        assert_eq!(result.runner, "unknown");
        assert_eq!(result.passed, 0);
        assert_eq!(result.failed, 0);
        assert_eq!(result.first_failures, Vec::<String>::new());
    }
}
