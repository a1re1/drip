// port of src/cli/args.ts — hand-written CLI parser (no clap, per drip/PLAN.md).
//
// Faithful port of parseCliArgs: every flag, the same validation rules and the
// same usage-error strings (with the lci→drip rename only). Usage errors are
// collected into `errors` — the caller prints them to stderr and exits 1
// (src/cli/main.tsx:616-622).

/// --synthesis: run the holistic review pass always, never, or (default) only
/// when a per-file unit reported something.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewSynthesis {
    Auto,
    Always,
    Never,
}

impl ReviewSynthesis {
    #[allow(dead_code)] // parity helper for later waves (core/harness consume the enum)
    fn as_str(self) -> &'static str {
        match self {
            ReviewSynthesis::Auto => "auto",
            ReviewSynthesis::Always => "always",
            ReviewSynthesis::Never => "never",
        }
    }
}

/// Port of the TS `ParsedCliArgs` type (src/cli/args.ts:1-125). Booleans
/// default to false; optional fields are Option. Field names mirror the TS
/// names (snake_case here — this struct is never JSON-serialized).
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedCliArgs {
    pub continue_latest: bool,
    /// Fatal usage problems found while parsing — the CLI prints these and exits 1 instead of guessing.
    pub errors: Vec<String>,
    pub goal: Option<String>,
    pub home: Option<String>,
    pub list: bool,
    pub max_iterations: Option<i64>,
    pub profile: Option<String>,
    pub resume: bool,
    pub resume_id: Option<String>,
    pub tools_path: String,
    pub version: bool,
    /// Goal text supplied via --prompt (alternative to the positional goal).
    pub prompt: Option<String>,
    /// Opt into the interactive TUI (requires a TTY).
    pub tui: bool,
    /// Override the project data dir (sessions + index) — the sandboxing escape hatch.
    pub project_dir: Option<String>,
    /// Disable the repo memory bank for this run.
    pub no_repo_memory: bool,
    /// Emit machine-readable JSON for informational commands (bare init, --list).
    pub json: bool,
    /// Send a steering message to a session's inbox (consumed by its running goal).
    pub send: bool,
    /// Optional session id or prefix for --send (defaults to the latest session).
    pub send_id: Option<String>,
    /// Stream a session's transcript, following new entries until interrupted.
    pub follow: bool,
    /// Optional session id or prefix for --follow (defaults to the latest session).
    pub follow_id: Option<String>,
    /// Print the full CLI reference and exit.
    pub help: bool,
    /// Skill names to activate for this run (repeatable --skill).
    pub skill_names: Vec<String>,
    /// Run with no skills and clear the session's stored activation.
    pub no_skills: bool,
    /// Activate a built-in role preset (e.g. "reviewed") or load a roles.json file path.
    pub roles_preset_or_path: Option<String>,
    /// Downgrade destructive-command policy blocks to warnings for this run.
    pub allow_destructive: bool,
    /// Opt this run into network access (enables the FETCH tool).
    pub allow_net: bool,
    /// Queue the goal behind a live run instead of refusing (LiveRunError).
    pub enqueue: bool,
    /// Dry-run: plan tasks with read-only tools, stop before executing.
    pub plan: bool,
    /// Archive an unfinished ledger and replan instead of continuing it.
    pub new_goal: bool,
    /// Start the goal in a background process and print the session handle.
    pub detach: bool,
    /// With --gc: compute and report without touching disk.
    pub dry_run: bool,
    /// Undo the last N journaled PATCH edits and exit.
    pub undo_last: bool,
    pub undo_last_count: Option<i64>,
    /// List the discovered skill pool and exit.
    pub skills: bool,
    /// Stop the session's running goal (SIGTERM to the leased pid).
    pub stop: bool,
    /// Optional session id or prefix for --stop (defaults to the latest session).
    pub stop_id: Option<String>,
    /// Print a human-readable summary of harness state and exit.
    pub state: bool,
    /// Optional session id or prefix for --state.
    pub state_id: Option<String>,
    /// With --state --json: dump the raw harness state instead of the curated summary.
    pub full: bool,
    /// Print per-goal run analytics from the session transcript and exit.
    pub inspect: bool,
    /// Optional session id or prefix for --inspect.
    pub inspect_id: Option<String>,
    /// Replay the persisted outcome of a session's most recent run and exit.
    pub result: bool,
    /// Optional session id or prefix for --result.
    pub result_id: Option<String>,
    /// Block until the session's running goal ends, then print its result.
    pub wait: bool,
    /// Optional session id or prefix for --wait.
    pub wait_id: Option<String>,
    /// Give up on --wait after this many seconds.
    pub timeout_secs: Option<i64>,
    /// List registered marketplaces and their plugins/skills.
    pub marketplace_list: bool,
    /// Clone+register a new marketplace (git URL or local path).
    pub marketplace_add: bool,
    /// Source (URL or path) for --marketplace-add.
    pub marketplace_add_source: Option<String>,
    /// Optional display name for --marketplace-add.
    pub marketplace_add_name: Option<String>,
    /// Remove a marketplace by name.
    pub marketplace_remove: bool,
    /// Name argument for --marketplace-remove.
    pub marketplace_remove_name: Option<String>,
    /// Pull updates for all (or a named) marketplace.
    pub marketplace_update: bool,
    /// Optional name argument for --marketplace-update.
    pub marketplace_update_name: Option<String>,
    /// Enable a plugin/skill key.
    pub plugin_enable: bool,
    /// Key argument for --plugin-enable.
    pub plugin_enable_key: Option<String>,
    /// Disable a plugin/skill key.
    pub plugin_disable: bool,
    /// Key argument for --plugin-disable.
    pub plugin_disable_key: Option<String>,
    /// Garbage-collect old idle session data (images, logs, long transcripts).
    pub gc: bool,
    /// Delete sessions whose updatedAt is older than this many days (default 14).
    pub older_than: Option<i64>,
    /// Review the working-tree diff file-by-file and print a holistic report (read-only).
    pub review: bool,
    /// Diff base ref for --review (defaults to the repo's default branch).
    pub review_base: Option<String>,
    /// Intended outcome of the change, required by --review so findings can be judged against the goal.
    pub review_context: Option<String>,
    /// Max simultaneous per-file review children (default 4).
    pub review_concurrency: Option<i64>,
    /// Model profile for the holistic synthesis pass (default "glm-5-3-flash").
    pub review_synth_profile: Option<String>,
    /// --synthesis mode for the holistic review pass.
    pub review_synthesis: Option<ReviewSynthesis>,
    /// Model profile for the per-file review children (default "glm-5-3-flash").
    pub review_file_profile: Option<String>,
    /// --migrate-from-lci: copy an lci home into the drip home (drip-only
    /// feature; no TS counterpart — see drip/PLAN.md and migrate.rs).
    pub migrate_from_lci: bool,
    /// Source lci home for --migrate-from-lci (default $LCI_HOME or ~/.lci).
    pub migrate_from: Option<String>,
    /// With --migrate-from-lci: also copy the current project's .lci → .drip.
    pub migrate_project: bool,
}

impl Default for ParsedCliArgs {
    fn default() -> Self {
        ParsedCliArgs {
            continue_latest: false,
            errors: Vec::new(),
            goal: None,
            home: None,
            list: false,
            max_iterations: None,
            profile: None,
            resume: false,
            resume_id: None,
            tools_path: "./tools".to_string(),
            version: false,
            prompt: None,
            tui: false,
            project_dir: None,
            no_repo_memory: false,
            json: false,
            send: false,
            send_id: None,
            follow: false,
            follow_id: None,
            help: false,
            skill_names: Vec::new(),
            no_skills: false,
            roles_preset_or_path: None,
            allow_destructive: false,
            allow_net: false,
            enqueue: false,
            plan: false,
            new_goal: false,
            detach: false,
            dry_run: false,
            undo_last: false,
            undo_last_count: None,
            skills: false,
            stop: false,
            stop_id: None,
            state: false,
            state_id: None,
            full: false,
            inspect: false,
            inspect_id: None,
            result: false,
            result_id: None,
            wait: false,
            wait_id: None,
            timeout_secs: None,
            marketplace_list: false,
            marketplace_add: false,
            marketplace_add_source: None,
            marketplace_add_name: None,
            marketplace_remove: false,
            marketplace_remove_name: None,
            marketplace_update: false,
            marketplace_update_name: None,
            plugin_enable: false,
            plugin_enable_key: None,
            plugin_disable: false,
            plugin_disable_key: None,
            gc: false,
            older_than: None,
            review: false,
            review_base: None,
            review_context: None,
            review_concurrency: None,
            review_synth_profile: None,
            review_synthesis: None,
            review_file_profile: None,
            migrate_from_lci: false,
            migrate_from: None,
            migrate_project: false,
        }
    }
}

// Optional-value flags (--resume/--state/--send/--follow/…) take a session ref
// only when the next token actually looks like one (uuid or hex-ish prefix),
// so goal text like `--send "stop and commit"` is never swallowed as an id.
// Port of SESSION_REF_PATTERN = /^[0-9a-fA-F][0-9a-fA-F-]{3,35}$/.
fn is_session_ref(token: &str) -> bool {
    let mut chars = token.chars();
    match chars.next() {
        Some(first) if first.is_ascii_hexdigit() => {}
        _ => return false,
    }
    let rest: Vec<char> = chars.collect();
    rest.len() >= 3 && rest.len() <= 35 && rest.iter().all(|&c| c.is_ascii_hexdigit() || c == '-')
}

fn take_session_ref(argv: &[String], index: usize) -> Option<String> {
    argv.get(index + 1)
        .filter(|next| is_session_ref(next))
        .cloned()
}

/// A typo'd flag must never silently become goal text: `--max-iteration 20`
/// once ran a full uncapped inference run with the goal "20". Unknown dashed
/// tokens are fatal; goals that genuinely start with "-" can be passed via
/// --prompt, which takes its next token verbatim.
///
/// Emulates `Number.parseInt(raw, 10)` followed by
/// `String(parsed) === raw.trim()`: only a plain non-negative decimal literal
/// (no sign, no leading zeros like "020", no trailing garbage, no overflow)
/// round-trips through JS number formatting.
fn parse_positive_int(raw: &str) -> Option<i64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || !trimmed.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }

    // JS: String(parseInt("020", 10)) === "020" is false → rejected. So any
    // leading-zero literal must error, matching `String(parsed) === raw.trim()`.
    if trimmed.len() > 1 && trimmed.starts_with('0') {
        return None;
    }

    trimmed.parse::<i64>().ok().filter(|&value| value > 0)
}

fn take_required_value(
    argv: &[String],
    index: usize,
    flag: &str,
    errors: &mut Vec<String>,
) -> Option<String> {
    match argv.get(index + 1) {
        None => {
            errors.push(format!("{} requires a value.", flag));
            None
        }
        Some(next) if next.starts_with('-') => {
            // A following token that starts with a dash is almost always a
            // missing value (a flag was provided instead). Flags that
            // legitimately allow dash-leading values handle that case
            // explicitly (see --prompt).
            errors.push(format!("{} requires a value.", flag));
            None
        }
        Some(next) => Some(next.clone()),
    }
}

fn set_session_ref(
    slot: &mut Option<String>,
    flag: &str,
    value: &str,
    errors: &mut Vec<String>,
) {
    if value.trim().is_empty() {
        errors.push(format!(
            "{}= needs a session id or prefix after the equals sign.",
            flag
        ));
    } else {
        *slot = Some(value.trim().to_string());
    }
}

pub fn parse_cli_args(argv: &[String]) -> ParsedCliArgs {
    let mut parsed = ParsedCliArgs::default();
    let mut positional: Vec<String> = Vec::new();
    let mut index: usize = 0;

    while index < argv.len() {
        let arg = argv[index].as_str();

        // --flag=value splits before dispatch, so session refs can be attached
        // explicitly (--state=1ea4) instead of relying on the next-token
        // heuristic that goal-like tokens can defeat.
        if arg.starts_with("--") && arg.contains('=') {
            let equals_index = arg.find('=').unwrap();
            let flag_name = &arg[..equals_index];
            let value = &arg[equals_index + 1..];

            match flag_name {
                "--follow" => {
                    parsed.follow = true;
                    set_session_ref(&mut parsed.follow_id, flag_name, value, &mut parsed.errors);
                }
                "--inspect" => {
                    parsed.inspect = true;
                    set_session_ref(&mut parsed.inspect_id, flag_name, value, &mut parsed.errors);
                }
                "--result" => {
                    parsed.result = true;
                    set_session_ref(&mut parsed.result_id, flag_name, value, &mut parsed.errors);
                }
                "--resume" => {
                    parsed.resume = true;
                    set_session_ref(&mut parsed.resume_id, flag_name, value, &mut parsed.errors);
                }
                "--send" => {
                    parsed.send = true;
                    set_session_ref(&mut parsed.send_id, flag_name, value, &mut parsed.errors);
                }
                "--state" => {
                    parsed.state = true;
                    set_session_ref(&mut parsed.state_id, flag_name, value, &mut parsed.errors);
                }
                "--stop" => {
                    parsed.stop = true;
                    set_session_ref(&mut parsed.stop_id, flag_name, value, &mut parsed.errors);
                }
                "--wait" => {
                    parsed.wait = true;
                    set_session_ref(&mut parsed.wait_id, flag_name, value, &mut parsed.errors);
                }
                _ => {
                    parsed.errors.push(format!(
                        "Unknown flag \"{}=...\" — only session-ref flags accept the equals form. Run drip --help.",
                        flag_name
                    ));
                }
            }

            index += 1;
            continue;
        }

        match arg {
            "--continue" | "-c" => {
                parsed.continue_latest = true;
            }
            "--resume" | "-r" => {
                parsed.resume = true;

                if let Some(reference) = take_session_ref(argv, index) {
                    parsed.resume_id = Some(reference);
                    index += 1;
                }
            }
            "--state" => {
                parsed.state = true;

                if let Some(reference) = take_session_ref(argv, index) {
                    parsed.state_id = Some(reference);
                    index += 1;
                }
            }
            "--inspect" => {
                parsed.inspect = true;

                if let Some(reference) = take_session_ref(argv, index) {
                    parsed.inspect_id = Some(reference);
                    index += 1;
                }
            }
            "--result" => {
                parsed.result = true;

                if let Some(reference) = take_session_ref(argv, index) {
                    parsed.result_id = Some(reference);
                    index += 1;
                }
            }
            "--wait" => {
                parsed.wait = true;

                if let Some(reference) = take_session_ref(argv, index) {
                    parsed.wait_id = Some(reference);
                    index += 1;
                }
            }
            "--timeout-secs" => {
                if let Some(raw) = take_required_value(argv, index, "--timeout-secs", &mut parsed.errors) {
                    match parse_positive_int(&raw) {
                        Some(value) => parsed.timeout_secs = Some(value),
                        None => parsed.errors.push(format!(
                            "--timeout-secs needs a positive integer, got \"{}\".",
                            raw
                        )),
                    }

                    index += 1;
                }
            }
            "--gc" => {
                parsed.gc = true;
            }
            "--older-than" => {
                if let Some(raw) = take_required_value(argv, index, "--older-than", &mut parsed.errors) {
                    match parse_positive_int(&raw) {
                        Some(value) => parsed.older_than = Some(value),
                        None => parsed.errors.push(format!(
                            "--older-than needs a positive integer (days), got \"{}\".",
                            raw
                        )),
                    }

                    index += 1;
                }
            }
            "--full" => {
                parsed.full = true;
            }
            "--send" => {
                parsed.send = true;

                if let Some(reference) = take_session_ref(argv, index) {
                    parsed.send_id = Some(reference);
                    index += 1;
                }
            }
            "--follow" => {
                parsed.follow = true;

                if let Some(reference) = take_session_ref(argv, index) {
                    parsed.follow_id = Some(reference);
                    index += 1;
                }
            }
            "--stop" => {
                parsed.stop = true;

                if let Some(reference) = take_session_ref(argv, index) {
                    parsed.stop_id = Some(reference);
                    index += 1;
                }
            }
            "--list" => {
                parsed.list = true;
            }
            "--prompt" => {
                match argv.get(index + 1) {
                    None => {
                        parsed.errors
                            .push("--prompt requires the goal text: --prompt \"goal\".".to_string());
                    }
                    Some(next) if next.starts_with('-') => {
                        // A dash-leading next token is almost always a forgotten
                        // value; a goal that really starts with "-" must be
                        // disambiguated by quoting it as the positional
                        // argument instead.
                        parsed.errors.push(format!(
                            "--prompt is followed by \"{}\", which looks like a flag — quote the goal text or pass it positionally.",
                            next
                        ));
                    }
                    Some(next) => {
                        parsed.prompt = Some(next.clone());
                        index += 1;
                    }
                }
            }
            "--no-repo-memory" => {
                parsed.no_repo_memory = true;
            }
            "--tui" | "--interactive" => {
                parsed.tui = true;
            }
            "--help" | "-h" => {
                parsed.help = true;
            }
            "--version" => {
                parsed.version = true;
            }
            "--json" => {
                parsed.json = true;
            }
            "--skills" => {
                parsed.skills = true;
            }
            "--dry-run" => {
                parsed.dry_run = true;
            }
            "--undo-last" => {
                parsed.undo_last = true;

                if let Some(next) = argv.get(index + 1) {
                    // JS: /^\d+$/.test(next)
                    if !next.is_empty() && next.bytes().all(|b| b.is_ascii_digit()) {
                        match next.parse::<i64>() {
                            Ok(value) if value > 0 => {
                                parsed.undo_last_count = Some(value);
                                index += 1;
                            }
                            _ => {
                                parsed.errors
                                    .push("--undo-last needs a positive count when one is given.".to_string());
                                index += 1;
                            }
                        }
                    }
                }
            }
            "--allow-destructive" => {
                parsed.allow_destructive = true;
            }
            "--allow-net" => {
                parsed.allow_net = true;
            }
            "--enqueue" => {
                parsed.enqueue = true;
            }
            "--plan" => {
                parsed.plan = true;
            }
            "--new-goal" => {
                parsed.new_goal = true;
            }
            "--detach" => {
                parsed.detach = true;
            }
            "--no-skills" => {
                parsed.no_skills = true;
            }
            "--skill" => {
                if let Some(value) = take_required_value(argv, index, "--skill", &mut parsed.errors) {
                    parsed.skill_names.push(value);
                    index += 1;
                }
            }
            "--roles" => {
                if let Some(value) = take_required_value(argv, index, "--roles", &mut parsed.errors) {
                    parsed.roles_preset_or_path = Some(value);
                    index += 1;
                }
            }
            "--home" => {
                if let Some(value) = take_required_value(argv, index, "--home", &mut parsed.errors) {
                    parsed.home = Some(value);
                    index += 1;
                }
            }
            "--project-dir" => {
                if let Some(value) = take_required_value(argv, index, "--project-dir", &mut parsed.errors) {
                    parsed.project_dir = Some(value);
                    index += 1;
                }
            }
            "--profile" => {
                if let Some(value) = take_required_value(argv, index, "--profile", &mut parsed.errors) {
                    parsed.profile = Some(value);
                    index += 1;
                }
            }
            "--tools" => {
                if let Some(value) = take_required_value(argv, index, "--tools", &mut parsed.errors) {
                    parsed.tools_path = value;
                    index += 1;
                }
            }
            "--max-iterations" => {
                if let Some(raw) = take_required_value(argv, index, "--max-iterations", &mut parsed.errors) {
                    match parse_positive_int(&raw) {
                        Some(value) => parsed.max_iterations = Some(value),
                        None => parsed.errors.push(format!(
                            "--max-iterations needs a positive integer, got \"{}\".",
                            raw
                        )),
                    }

                    index += 1;
                }
            }
            "--marketplace-list" => {
                parsed.marketplace_list = true;
            }
            "--marketplace-add" => {
                if let Some(source) = take_required_value(argv, index, "--marketplace-add", &mut parsed.errors) {
                    parsed.marketplace_add = true;
                    parsed.marketplace_add_source = Some(source);
                    index += 1;

                    // Optional second positional: the display name (non-flag token)
                    if let Some(maybe_next) = argv.get(index + 1) {
                        if !maybe_next.starts_with('-') {
                            parsed.marketplace_add_name = Some(maybe_next.clone());
                            index += 1;
                        }
                    }
                }
            }
            "--marketplace-remove" => {
                if let Some(name) = take_required_value(argv, index, "--marketplace-remove", &mut parsed.errors) {
                    parsed.marketplace_remove = true;
                    parsed.marketplace_remove_name = Some(name);
                    index += 1;
                }
            }
            "--marketplace-update" => {
                parsed.marketplace_update = true;

                // Optional name: next non-flag token
                if let Some(maybe_next) = argv.get(index + 1) {
                    if !maybe_next.starts_with('-') {
                        parsed.marketplace_update_name = Some(maybe_next.clone());
                        index += 1;
                    }
                }
            }
            "--plugin-enable" => {
                if let Some(key) = take_required_value(argv, index, "--plugin-enable", &mut parsed.errors) {
                    parsed.plugin_enable = true;
                    parsed.plugin_enable_key = Some(key);
                    index += 1;
                }
            }
            "--plugin-disable" => {
                if let Some(key) = take_required_value(argv, index, "--plugin-disable", &mut parsed.errors) {
                    parsed.plugin_disable = true;
                    parsed.plugin_disable_key = Some(key);
                    index += 1;
                }
            }
            "--review" => {
                parsed.review = true;
            }
            "--base" => {
                if let Some(value) = take_required_value(argv, index, "--base", &mut parsed.errors) {
                    parsed.review_base = Some(value);
                    index += 1;
                }
            }
            "--context" => {
                if let Some(value) = take_required_value(argv, index, "--context", &mut parsed.errors) {
                    parsed.review_context = Some(value);
                    index += 1;
                }
            }
            "--concurrency" => {
                if let Some(raw) = take_required_value(argv, index, "--concurrency", &mut parsed.errors) {
                    match parse_positive_int(&raw) {
                        Some(value) => parsed.review_concurrency = Some(value),
                        None => parsed.errors.push(format!(
                            "--concurrency needs a positive integer, got \"{}\".",
                            raw
                        )),
                    }

                    index += 1;
                }
            }
            "--synth-profile" => {
                if let Some(value) = take_required_value(argv, index, "--synth-profile", &mut parsed.errors) {
                    parsed.review_synth_profile = Some(value);
                    index += 1;
                }
            }
            "--synthesis" => {
                if let Some(raw) = take_required_value(argv, index, "--synthesis", &mut parsed.errors) {
                    match raw.trim() {
                        "auto" => parsed.review_synthesis = Some(ReviewSynthesis::Auto),
                        "always" => parsed.review_synthesis = Some(ReviewSynthesis::Always),
                        "never" => parsed.review_synthesis = Some(ReviewSynthesis::Never),
                        _ => parsed.errors.push(format!(
                            "--synthesis needs one of auto, always, never — got \"{}\".",
                            raw
                        )),
                    }

                    index += 1;
                }
            }
            "--file-profile" => {
                if let Some(value) = take_required_value(argv, index, "--file-profile", &mut parsed.errors) {
                    parsed.review_file_profile = Some(value);
                    index += 1;
                }
            }
            // drip-only: lci → drip migration flags (drip/PLAN.md, migrate.rs).
            // --dry-run is shared with plan mode and parsed above.
            "--migrate-from-lci" => {
                parsed.migrate_from_lci = true;
            }
            "--from" => {
                if let Some(value) = take_required_value(argv, index, "--from", &mut parsed.errors) {
                    parsed.migrate_from = Some(value);
                    index += 1;
                }
            }
            "--project" => {
                parsed.migrate_project = true;
            }
            other if other.starts_with('-') && other != "-" => {
                parsed.errors.push(format!(
                    "Unknown flag \"{}\". Run drip --help for the full reference.",
                    other
                ));
            }
            other => {
                positional.push(other.to_string());
            }
        }

        index += 1;
    }

    parsed.goal = {
        let joined = positional.join(" ");
        let trimmed = joined.trim();

        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    };

    // A review without a usable statement of intent cannot judge whether the
    // change accomplishes its goal — that is the failure mode --review exists
    // to design out — so a short or missing --context is fatal to the mode.
    if parsed.review {
        let short = match &parsed.review_context {
            None => true,
            Some(context) => context.trim().chars().count() < 20,
        };

        if short {
            parsed.errors.push(
                "--review needs --context \"<what this change is trying to achieve>\" so the review can judge whether the change accomplishes its goal (at least 20 characters)."
                    .to_string(),
            );
        }
    }

    parsed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(flags: &[&str]) -> ParsedCliArgs {
        let argv: Vec<String> = flags.iter().map(|s| s.to_string()).collect();
        parse_cli_args(&argv)
    }

    // --- port of test/cli-headless.test.ts "strict argument validation" ---

    #[test]
    fn rejects_unknown_flags_instead_of_treating_them_as_goal_text() {
        let typo = parse(&["--max-iteration", "20"]);

        assert!(typo.errors.iter().any(|problem| problem.contains("--max-iteration")));
        assert!(!parse(&["-x"]).errors.is_empty());
        assert!(parse(&["do the work"]).errors.is_empty());
        assert_eq!(parse(&["do the work"]).goal.as_deref(), Some("do the work"));
    }

    #[test]
    fn rejects_invalid_max_iterations_values_instead_of_silently_dropping_the_cap() {
        assert!(!parse(&["--max-iterations", "abc"]).errors.is_empty());
        assert!(!parse(&["--max-iterations", "0"]).errors.is_empty());
        assert!(!parse(&["--max-iterations"]).errors.is_empty());
        assert_eq!(parse(&["--max-iterations", "7"]).max_iterations, Some(7));
    }

    #[test]
    fn errors_when_prompt_is_missing_its_value_or_followed_by_a_flag() {
        assert!(!parse(&["--prompt"]).errors.is_empty());
        assert!(!parse(&["--prompt", "--json"]).errors.is_empty());
        assert!(parse(&["--prompt", "real goal"]).errors.is_empty());
    }

    #[test]
    fn errors_when_value_taking_flags_are_left_dangling() {
        assert!(!parse(&["--profile"]).errors.is_empty());
        assert!(!parse(&["--tools"]).errors.is_empty());
        assert!(!parse(&["--home"]).errors.is_empty());
        assert!(!parse(&["--project-dir"]).errors.is_empty());
    }

    // --- port of test/cli-headless.test.ts "skill flags" ---

    #[test]
    fn collects_repeatable_skill_names_and_parses_skills() {
        let parsed = parse(&["--skill", "tdd", "--skill", "verify", "goal text"]);

        assert_eq!(parsed.skill_names, vec!["tdd", "verify"]);
        assert_eq!(parsed.goal.as_deref(), Some("goal text"));
        assert!(parse(&["--skills"]).skills);
        assert!(!parse(&["--skill"]).errors.is_empty());
        assert!(parse(&["--no-skills"]).no_skills);
    }

    // --- port of test/cli-headless.test.ts "stop flag" ---

    #[test]
    fn parses_stop_with_and_without_a_session_ref() {
        assert!(parse(&["--stop"]).stop);
        assert_eq!(parse(&["--stop", "abc123"]).stop_id.as_deref(), Some("abc123"));
        assert_eq!(parse(&["--stop", "not a ref"]).stop_id, None);
    }

    // --- port of test/cli-headless.test.ts "session-ref equals syntax" ---

    #[test]
    fn attaches_refs_explicitly_with_flag_equals_ref() {
        let parsed = parse(&["--state=1ea4beef"]);

        assert!(parsed.state);
        assert_eq!(parsed.state_id.as_deref(), Some("1ea4beef"));
        assert_eq!(
            parse(&["--resume=abc123", "--prompt", "goal"]).resume_id.as_deref(),
            Some("abc123")
        );
        assert_eq!(parse(&["--wait=abc123"]).wait_id.as_deref(), Some("abc123"));
        assert!(!parse(&["--state="]).errors.is_empty());
        // Non-ref flags reject the equals form loudly instead of parsing oddly.
        assert!(!parse(&["--max-iterations=5"]).errors.is_empty());
    }

    // --- port of test/cli-headless.test.ts "gc dry-run flag" + "result/wait flags" ---

    #[test]
    fn parses_gc_with_dry_run_and_older_than() {
        let parsed = parse(&["--gc", "--dry-run", "--older-than", "7"]);

        assert!(parsed.gc);
        assert!(parsed.dry_run);
        assert_eq!(parsed.older_than, Some(7));
    }

    #[test]
    fn parses_result_and_wait_with_and_without_a_session_ref() {
        assert!(parse(&["--result"]).result);
        assert_eq!(parse(&["--result", "abc123"]).result_id.as_deref(), Some("abc123"));
        assert!(parse(&["--wait"]).wait);
        assert_eq!(parse(&["--wait", "abc123"]).wait_id.as_deref(), Some("abc123"));
    }

    // --- port of test/cli-gc.test.ts "Flag parsing" ---

    #[test]
    fn parses_gc_alone() {
        let parsed = parse(&["--gc"]);

        assert!(parsed.gc);
        assert!(parsed.errors.is_empty());
    }

    #[test]
    fn parses_gc_with_older_than() {
        let parsed = parse(&["--gc", "--older-than", "30"]);

        assert!(parsed.gc);
        assert_eq!(parsed.older_than, Some(30));
        assert!(parsed.errors.is_empty());
    }

    // --- exact usage-error text (usage errors exit 1; strings pinned here) ---

    #[test]
    fn exact_error_strings_match_lci_verbatim_with_drip_rename() {
        assert_eq!(
            parse(&["--profile"]).errors,
            vec!["--profile requires a value.".to_string()]
        );
        assert_eq!(
            parse(&["-x"]).errors,
            vec!["Unknown flag \"-x\". Run drip --help for the full reference.".to_string()]
        );
        assert_eq!(
            parse(&["--max-iterations=5"]).errors,
            vec!["Unknown flag \"--max-iterations=...\" — only session-ref flags accept the equals form. Run drip --help.".to_string()]
        );
        assert_eq!(
            parse(&["--max-iterations", "0"]).errors,
            vec!["--max-iterations needs a positive integer, got \"0\".".to_string()]
        );
        assert_eq!(
            parse(&["--state="]).errors,
            vec!["--state= needs a session id or prefix after the equals sign.".to_string()]
        );
        assert_eq!(
            parse(&["--prompt", "--json"]).errors,
            vec!["--prompt is followed by \"--json\", which looks like a flag — quote the goal text or pass it positionally.".to_string()]
        );
        assert_eq!(
            parse(&["--synthesis", "sometimes"]).errors,
            vec!["--synthesis needs one of auto, always, never — got \"sometimes\".".to_string()]
        );
        assert_eq!(
            parse(&["--review", "--context", "too short"]).errors,
            vec!["--review needs --context \"<what this change is trying to achieve>\" so the review can judge whether the change accomplishes its goal (at least 20 characters).".to_string()]
        );
    }

    #[test]
    fn session_refs_are_not_swallowed_from_goal_text_and_defaults_hold() {
        // "abc123" looks like a ref; "not a ref" does not.
        assert_eq!(parse(&["--resume", "abc123"]).resume_id.as_deref(), Some("abc123"));
        assert_eq!(parse(&["--resume", "goal text"]).resume_id, None);
        assert_eq!(parse(&["--resume", "goal text"]).goal.as_deref(), Some("goal text"));

        let defaulted = parse(&[]);
        assert!(!defaulted.help);
        assert!(!defaulted.version);
        assert_eq!(defaulted.tools_path, "./tools");
        assert_eq!(defaulted.goal, None);
        assert_eq!(defaulted.errors, Vec::<String>::new());

        // "020" round-trips through JS String(parsed) === raw.trim() as false.
        assert!(parse(&["--max-iterations", "020"]).errors.is_empty() == false);
        assert_eq!(parse(&["--max-iterations", "020"]).max_iterations, None);
    }

    // --- drip-only migration flags (no TS counterpart; see migrate.rs) ---

    #[test]
    fn parses_migration_flags_with_defaults() {
        let parsed = parse(&["--migrate-from-lci"]);

        assert!(parsed.migrate_from_lci);
        assert_eq!(parsed.migrate_from, None);
        assert!(!parsed.dry_run);
        assert!(!parsed.migrate_project);
        assert_eq!(parsed.errors, Vec::<String>::new());
    }

    #[test]
    fn parses_the_full_migration_flag_combination() {
        let parsed = parse(&[
            "--migrate-from-lci",
            "--from",
            "/tmp/old-lci-home",
            "--dry-run",
            "--project",
        ]);

        assert!(parsed.migrate_from_lci);
        assert_eq!(parsed.migrate_from.as_deref(), Some("/tmp/old-lci-home"));
        assert!(parsed.dry_run);
        assert!(parsed.migrate_project);
        assert_eq!(parsed.goal, None);
        assert_eq!(parsed.errors, Vec::<String>::new());
    }

    #[test]
    fn migration_from_flag_requires_a_value() {
        assert_eq!(
            parse(&["--from"]).errors,
            vec!["--from requires a value.".to_string()]
        );
        // A dash-leading next token is treated as a missing value, matching
        // every other required-value flag.
        assert_eq!(
            parse(&["--from", "--dry-run"]).errors,
            vec!["--from requires a value.".to_string()]
        );
        // The flags are independent: --from is not gated on --migrate-from-lci.
        assert_eq!(
            parse(&["--from", "/old/lci"]).migrate_from.as_deref(),
            Some("/old/lci")
        );
        assert!(!parse(&["--project"]).migrate_from_lci);
    }

    // --- port of test/cli-help-drift.test.ts (adapted): parser ↔ help text ---
    // The TS drift test derives a flag roster from src/cli/args.ts and asserts
    // each flag appears in CLI_HELP_TEXT. In Rust the parser is the source of
    // truth, so the check runs in both directions: every flag HELP mentions is
    // one the parser accepts (this test), and every flag the parser accepts is
    // documented in HELP (help.rs::help_documents_every_cli_flag).

    #[test]
    fn every_flag_in_the_help_text_is_accepted_by_the_parser() {
        let help = crate::cli::help::HELP;

        // Extract candidate flags from the help text. Only OPTION-definition
        // lines count — lines whose trimmed text starts with "--" — because
        // prose paragraphs (e.g. the review-warnings blurb's "git push
        // --force / reset --hard") mention flag-shaped tokens that are not
        // CLI flags. This mirrors the TS drift test's intent: its roster is
        // derived from args.ts, never from prose. Splitting on whitespace
        // and slashes covers "--plugin-enable/--plugin-disable" and the
        // continuation indents under the USAGE block.
        let mut flags: Vec<String> = Vec::new();

        for line in help.lines() {
            let trimmed_line = line.trim_start();

            if !trimmed_line.starts_with("--") {
                continue;
            }

            for word in trimmed_line.split(|c: char| c == ' ' || c == '\t' || c == '/') {
                // A flag token runs from "--" through lowercase letters and
                // dashes; anything else ends it ("--send's" possessive on a
                // definition line, "<id>", "[name]", trailing comma).
                // trim_matches alone kept "'s" because 's' is lowercase.
                if let Some(rest) = word.strip_prefix("--") {
                    let token: String = rest
                        .chars()
                        .take_while(|c| c.is_ascii_lowercase() || *c == '-')
                        .collect();

                    if token.len() >= 2 {
                        flags.push(format!("--{token}"));
                    }
                }
            }
        }

        flags.sort();
        flags.dedup();
        assert!(
            flags.len() >= 40,
            "expected to extract a real flag roster from HELP, got {flags:?}"
        );

        for flag in &flags {
            let args = [flag.as_str()];
            let parsed = parse(&args);
            assert!(
                !parsed
                    .errors
                    .iter()
                    .any(|problem| problem.contains("Unknown flag")),
                "HELP documents {flag} but the parser rejects it: {:?}",
                parsed.errors
            );
        }
    }
}
