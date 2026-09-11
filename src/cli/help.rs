// The --help text. Kept as a single raw string constant so it can be
// compared byte for byte in tests.

pub const HELP: &str = r#"	drip — local code inference, a headless-first coding agent harness

USAGE
	drip "goal text" [options]     Run a goal headlessly in a new session (default)
	drip                           Init a session in this directory and print its paths
	drip --continue "goal"         Run a goal in the latest session for this directory
	drip --resume <id> "goal"      Run a goal in a specific session (id or prefix)
	drip --tui                     Open the interactive TUI (monitoring / follow-along)
	drip --gc [--older-than <days>] Collect-and-compact idle sessions older than N days (default 14)
	drip --review --context "..."  Fan-out code review of the diff: changed files bundled into review
	                              units reviewed in parallel on a fast model, then one synthesis pass
	drip --bash "<cmd>" --context "..."  Run a shell command and print a short distillation of its
	                              output guided by --context, instead of the raw stream
	drip --list                    List sessions recorded for this directory
	drip --list --recursive       List sessions for this directory tree (and below)
	drip --ui                     Serve the browser UI for sessions under this directory
	drip --state [id]              Print harness state summary for the latest (or given) session
	drip --result [id]             Replay the persisted outcome of a session's most recent run
	drip --inspect [id]            Per-goal run analytics (wall time, tool stats, steering)
	drip --wait [id]               Block until the running goal ends, then print its result
	drip --send [id] "message"     Steer a session: a running goal reads it at its next cycle
	drip --follow [id]             Stream a session's transcript live (ctrl+c to stop)
	drip --stop [id]               Stop a session's running goal (state persists; resumable)
	drip --skills                  List the discovered skill pool (project/user/marketplace)
	drip --help                    Show this text
	drip --version                 Print the drip CLI version
	drip --marketplace-list        List marketplaces/plugins (also: --marketplace-add <src> [name],
	                              --marketplace-remove <name>, --marketplace-update [name],
	                              --plugin-enable/--plugin-disable <marketplace/plugin[/skill]>)

OPTIONS
	--continue, -c                Reuse the latest session for this project
	                              (starts a new one with a stderr notice if none exists)
	--resume, -r [id]             Reuse a session by id (or unique id prefix); errors if not found
	--gc                          Delete images/ and *.log files, truncate transcript.jsonl to the
	                              last 200 lines for sessions whose status is not running and whose
	                              last update is older than --older-than days (default 14). Sessions
	                              with a live lease are never touched. Combine with --dry-run to
	                              preview without writing and --json for machine-readable output.
	--older-than <days>           Age threshold for --gc (positive integer, default 14)
	--ui                          Serve the browser UI for the sessions under this
	                              directory: drip unpacks a small Bun/React app under
	                              ~/.drip/ui/ and runs it with bun (installed from
	                              https://bun.sh, needed on PATH). Sessions it starts
	                              are ordinary detached runs, shared with --tui and
	                              dripw. With a local Caddy running, every UI also
	                              registers under one hub (http://drip.localhost:4140/)
	                              so several projects share a stable address. Nothing
	                              is written into this directory.
	--port <port>                 Pin the port --ui listens on. Without it the UI
	                              takes the first free port from 4141, so several
	                              projects can run at once; only valid with --ui.
	--recursive                   With --list, also include sessions whose launch
	                              directory is a subdirectory of this one (from every
	                              registry under the drip home); only valid together
	                              with --list. Add --json for the same row shape with
	                              a "cwd" field on every row.
	--review                      Review the diff against --base in review units, then synthesize
	                              one holistic review. Read-only: it never edits, commits, or posts
	                              to GitHub. Changed files are planned into units — docs/manifests
	                              in one, a source file with its tests, small same-directory files
	                              together (<=3 files, <=400 diff lines), big files alone, a file
	                              over 800 diff lines as hunk chunks reviewed in parallel — and
	                              each unit gets one budgeted, time-boxed, read-only child session
	                              (an errored or incomplete unit is retried once); the JSON stays
	                              per file (chunks carry "part": "i/n"). Findings are P0 (blocks), P1 (important), or P2 (fix before
	                              merge) — nothing below P2 is reported — and the confidence
	                              score (5/5 down to 1/5) is computed from the P0/P1 counts, not by
	                              a model. Exits 0 when no P0/P1 findings remain, 4 when some do,
	                              so a fix loop can branch on it. With --json: one object with the
	                              score, per-file ratings/counts, the units, and the full report.
	--context <text>              REQUIRED by --review (min 20 chars): what the change is trying to
	                              achieve. Every reviewer judges the diff against this intent, so a
	                              change that is technically clean but misses its goal is a finding.
	                              Also REQUIRED by --bash: what you expect from the command, what
	                              counts as success or failure, and what to report back.
	--bash <command>              Run <command> via the shell for a calling agent and print a
	                              context-guided distillation of its output instead of the raw
	                              stream (stdout); a one-line header goes to stderr. Output under
	                              --distill-min-lines lines and 3 KB is printed verbatim; larger
	                              output is distilled by one tool-free model call. If distillation
	                              fails, a head+tail excerpt is printed instead (fail-open). Exits
	                              with the command's own exit code (124 on timeout, 128+n when a
	                              signal killed it, e.g. 139 for SIGSEGV). With --json: one object
	                              with exitCode, timedOut, signal, distilled, line/byte counts,
	                              truncated/bypassed flags, model, usage, and timings.
	--distill-profile <id>        Model profile for --bash distillation (default glm-5-3-flash)
	--distill-min-lines <n>       --bash bypass threshold: output with fewer than n lines and under
	                              3 KB is returned verbatim with no model call (default 30)
	--timeout-ms <n>              Timeout for the --bash command in milliseconds (default 120000)
	--base <ref>                  Diff base for --review (default: the repo's default branch).
	                              Compared as <base>...HEAD, i.e. from the merge base
	--concurrency <n>             Max review units reviewed at once (positive integer,
	                              default 4). The cap is what keeps a wide diff from tripping
	                              provider rate limits
	--file-profile <id>           Model profile for the unit reviewers (default glm-5-3-flash)
	--synthesis <mode>            When the holistic synthesis pass runs: "auto" (default) skips it
	                              when every unit came back clean — no P0/P1/P2, nothing errored
	                              or unrated — and writes a short computed report instead;
	                              "always" runs it regardless; "never" returns the per-file
	                              reports only
	--synth-profile <id>          Model profile for the holistic synthesis pass (default
	                              glm-5-3-flash, the same lane as --file-profile; pass kimi-k3
	                              for the stronger reviewer lane)
	--list                        Print sessions for this project, newest first
	--state [id]                  Print a human-readable summary of a session's harness state
	                              (goal, tasks, memory notes, warm context, history count);
	                              uses the latest session if no id is given. With --json:
	                              a curated summary (goal, running, taskStats, tasks,
	                              lastVerification, pendingOperatorMessages); add --full
	                              for the raw state dump
	--result [id]                 Print the persisted outcome of the session's most recent
	                              run — the same payload the run's final --json line had —
	                              and exit with that run's exit code (0/2/3). Survives lost
	                              stdout: every run persists result.json at run-end
	--wait [id]                   Block until the session's running goal ends, then behave
	                              like --result. No live run answers immediately from the
	                              last result. A lease that dies without a result reports
	                              "crashed" (exit 3)
	--timeout-secs N              Give up on --wait after N seconds (exit 124; the run
	                              keeps going — re-run --wait to keep waiting)
	--full                        With --state --json: raw HarnessState instead of the
	                              curated summary
	--inspect [id]                Analytics from the session's transcript: per-goal wall
	                              time and outcome, tool call/failure/duration stats,
	                              rate-limit waits, steering adoption latency, the
	                              verification timeline, and last-run usage
	--send [id] "message"         Append a steering message to the session's inbox. A running
	                              goal consumes it at its next cycle boundary (the model treats
	                              it as fresh operator instructions that outrank the original
	                              goal); otherwise the session's next run consumes it
	--answer [id] <json|text>     Answer a pending ask_user clarification survey: appends
	                              {"answers":[{"index":0,"choice":"...","other":null}]} to the
	                              session's answers.jsonl (plain text = free-text answer to
	                              question 0); the blocked run picks it up within ~500ms
	--follow [id]                 Replay the transcript tail, then stream new entries as they
	                              are appended (with --json: raw JSONL passthrough)
	--stop [id]                   SIGTERM the session's running goal — it aborts at the next
	                              safe point with state persisted. Liveness comes from a pid +
	                              heartbeat lease, so crashed runs read as idle, a second
	                              concurrent run against a live session is refused, and
	                              --send's runActive tells you if steering applies now
	--tui, --interactive          Launch the Ink TUI (errors without a terminal)
	--prompt "goal text"          Goal as a flag instead of a positional (for scripts)
	--max-iterations N            Cap harness cycles for this run
	--max-loops N                 Cap task loops for this run (each loop is at least one model
	                              call; a replanning loop is exactly one planner call)
	--no-repo-memory              Disable the repo memory bank (~/.drip/projects/<slug>/memory) for this run:
	                              no index injection, repo-scoped remember/forget refused
	--profile <id>                Model profile for this invocation — applies to both the
	                              general and tool-call routes (see ~/.drip/config.json)
	--skill <name>                Activate a skill for this run (repeatable); its SKILL.md
	                              joins the system prompt like the TUI's /skill toggle.
	                              Parameterized: --skill migration:from=jest,to=vitest
	                              fills {{from}}/{{to}} placeholders (frontmatter args:
	                              block declares names + defaults; missing required or
	                              unknown args are usage errors). Activation persists on
	                              the session: --resume/--continue without --skill flags
	                              re-activates the last run's set (args included), and an
	                              explicit --profile is pinned and re-used the same way
	--roles <preset-or-path>      Activate a built-in role preset or load a roles file for
	                              this run. Built-in presets: "reviewed" (a read-only
	                              planner, an author, and an independent reviewer that
	                              cannot use PATCH, enforcing review independence);
	                              "research" (a single researcher role that investigates
	                              and reports findings with citations); "team" (a
	                              researcher hands findings to a coder, whose work is then
	                              verified by an independent reviewer); "planned" (a
	                              stronger architect model writes each task as a contract
	                              — files, functions, verification command — and the fast
	                              author lane implements them; no reviewer loop).
	                              Researcher and
	                              reviewer roles, and every preset's planning role, are
	                              denied PATCH — no journaled edits — but keep BASH, so
	                              they are not a write sandbox. Each
	                              preset role also pins its own model profile. Any other
	                              value is treated as a path to a
	                              roles.json file using the same schema as config-sourced
	                              roles. Preset roles are merged with config-sourced roles
	                              by name (preset takes precedence on name collision).
	--no-skills                   Run bare and clear the session's stored skill activation
	--undo-last [n]               Revert the last n journaled PATCH edits (default 1).
	                              Every PATCH is written atomically and journaled to
	                              .drip/patches.jsonl with its pre-image; undo refuses
	                              files that changed after the patch
	--new-goal                    With --resume/--continue: archive the unfinished task
	                              ledger and replan for the new prompt. Without it, a
	                              different prompt against unfinished tasks CONTINUES the
	                              ledger with the prompt applied as steering
	--plan                        Dry-run: plan the task decomposition with read-only tools
	                              (READ/GREP/DIR) and stop before executing — the run ends
	                              with reason "planned" (exit 2), the plan persists on the
	                              session, and resuming the same goal executes it
	--enqueue                     With a goal against a busy session: queue it instead of
	                              refusing — the running process drains the queue at run
	                              end, emitting one result line per goal
	--detach                      Start the goal in a background process and print
	                              {sessionId, pid, waitCommand} immediately; pair with
	                              --wait/--result to collect the outcome
	--allow-net                   Opt the run into network access: enables the FETCH
	                              tool (bounded readable-text web fetch, off by default)
	--reference-root <dir>        Point the REFERENCE tool at an oasis-indexed markdown
	                              corpus (repeatable; also DRIP_REFERENCE_ROOTS or
	                              OASIS_ROOTS, colon-separated). Without a root the tool
	                              is left out of the pack; requires the `oasis` binary
	--ask                         Opt the run into operator clarification surveys: enables the
	                              ask_user tool (staged multiple-choice questions; the run
	                              blocks for answers via --answer or the TUI overlay, then
	                              replans). Pinned on the session, so --resume keeps it
	--ask-timeout <seconds>       How long a blocked survey waits for answers before the run
	                              ends with reason awaiting-input (default 900); resume later
	                              after answering
	--allow-destructive           Downgrade destructive-command policy blocks (rm -rf
	                              outside the workspace, git push --force / reset --hard /
	                              clean -f, sudo, curl|sh, device writes) to warnings for
	                              this run; a repo can allowlist specific commands in
	                              .drip/policy.json {"allowCommands": ["<substring>"]}
	--skills                      List discovered skills with source and description;
	                              a built-in pack ships with drip
	                              (verify-before-done, tdd, commit-discipline,
	                              debug-root-cause, refactor-safely,
	                              review-independently, migration-discipline,
	                              praeparare),
	                              shadowable by name
	--tools <path>                Tools directory (default ./tools, falls back to built-in)
	--home <path>                 Override the global home (default ~/.drip or $DRIP_HOME)
	--project-dir <path>          Override the session data dir (default <project>/.drip);
	                              DRIP_PROJECT_DIR does the same — use either to sandbox runs
	--json                        Machine-readable output everywhere: bare init, --list,
	                              --state, and goal runs (NDJSON event stream + one final
	                              {"type":"result"} line with reason/summary/exit code)
	--marketplace-list [--json]   List registered marketplaces, their plugins/skills, enabled state
	--marketplace-add <src> [nm]  Clone+register a marketplace (git URL or local path)
	--marketplace-remove <name>   Unregister a marketplace (its clone is removed)
	--marketplace-update [name]   git pull registered marketplace clones (all, or one by name)
	--plugin-enable <key>         Enable a plugin or single skill: marketplace/plugin[/skill]
	--plugin-disable <key>        Disable a plugin or single skill
	--lite                        Draft mode: single author lane, no reviewer task, no
	                              completion anchor gate, no run summary; ends with reason
	                              "draft" and a continueCommand for the full-rigor pass
	                              (--resume <session> --roles reviewed --skill verify-before-done)
	--no-review                   Skip the verified_by review chain and the finish_task
	                              completion-anchor gate for this run; usable with any
	                              --roles preset (implied by --lite)
	--praeparare                  Prepare the current branch for a pull request: an agentic
	                              pre-PR pass — cleanup, checks, commit, base merge, push, and a
	                              DRAFT PR; refuses on the base branch. Takes no goal; a
	                              positional goal is appended as extra operator context.
	--version                     Print the drip CLI version (with --json: {"version":"<version>"})
	--help, -h                    Show this text

STORAGE
	The project root is discovered git-style from the cwd upward: the nearest
	directory with an existing .drip wins, then the nearest with .git, then the
	cwd itself — so running drip from a subdirectory lands in the repo's .drip.
	Session data is machine-local and keyed by the project root's path slug, so
	it lives under the global home rather than in the repo:
	~/.drip/projects/<slug>/sessions/<session-id>/   One directory per session:
		session.json                    Which repo the session belongs to (id, cwd, slug)
		state.json                      Harness state store (tasks, memory, telemetry)
		transcript.jsonl                Append-only timeline; tail -f to follow a run
		result.json                     Outcome of the most recent run (--result replays it)
		images/                         Attached images
	~/.drip/projects/<slug>/index.sqlite   Session registry for this project
	~/.drip/projects/<slug>/memory/        Project memory bank
	Sessions recorded before they moved out of the repo stay readable at
	<project>/.drip/sessions/ — --list, --resume, --inspect and --gc see both.
	<project>/.drip/patches.jsonl, async-tools/          Repo-scoped run data
	<project>/.drip/skills/, roles.json, plugins.json   Project-scoped configuration
	(skill precedence: project > user > marketplace > built-in pack)
	~/.drip/config.json                  Model + prompt profiles (shared with the web app)
	~/.drip/env.vars                     API keys (dotenv format, 0600)

AUTOMATION RECIPES
	Start a run and capture the session id:
		drip                         # prints "session <id>" plus paths
		drip --resume <id> "do the thing" --max-iterations 20
	Follow a run from another terminal (or another agent):
		drip --follow <id>          # formatted; or tail -f ~/.drip/projects/<slug>/sessions/<id>/transcript.jsonl
	Steer a running goal without stopping it:
		drip --send <id> "change of plans: ..."
	Continue an unfinished run: re-submit the same goal to the same session —
		drip --resume <id> "do the thing"
	Inspect results: state.json holds tasks/memory/run summary; the run's final
	summary is also printed to stdout. With --json a run emits NDJSON events and
	ends with one {"type":"result","reason":...,"summary":...,"exitCode":...,
	"continueCommand":...} line — parse that instead of scraping prose. The
	continueCommand embeds the real goal text, ready to exec. Lost the stdout?
	drip --result <id> --json replays the same payload from result.json.
	Wait for a run another process started (exit code = the run's):
		drip --wait <id> --json --timeout-secs 600

EXIT CODES
	0   run completed, or finished unreconciled — every task done but an expectation
	    left visibly unreconciled (see anomalies) — or informational command succeeded
	1   usage / setup error
	2   run ended without completing (max-iterations, max-loops, blocked-on-input, or stopped)
	3   run failed on an infrastructure error (endpoint unreachable/5xx after
		retries) or --wait saw the run die without a result — state is
		persisted; resume the same goal when healthy
	124 --wait gave up after --timeout-secs (the run keeps going)
	--bash exits with the wrapped command's own code instead (124 on timeout, 128+n
	    when a signal killed it); its stderr header says which case applies
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_text_carries_drip_branding_only() {
        assert!(HELP.contains("drip — local code inference"));
    }

    // --- help text contract ---
    // (The flag-roster drift check lives on the args.rs side, where the
    // roster can be cross-checked in both directions against the real parser.)

    #[test]
    fn help_documents_every_cli_flag() {
        let flags = [
            "--continue",
            "--resume",
            "--list",
            "--recursive",
            "--ui",
            "--port",
            "--state",
            "--send",
            "--follow",
            "--stop",
            "--tui",
            "--prompt",
            "--max-iterations",
            "--max-loops",
            "--lite",
            "--no-review",
            "--profile",
            "--skill",
            "--roles",
            "--skills",
            "--no-skills",
            "--answer",
            "--ask",
            "--ask-timeout",
            "--allow-destructive",
            "--allow-net",
            "--reference-root",
            "--enqueue",
            "--plan",
            "--praeparare",
            "--detach",
            "--undo-last",
            "--tools",
            "--home",
            "--project-dir",
            "--json",
            "--marketplace-list",
            "--marketplace-add",
            "--marketplace-remove",
            "--marketplace-update",
            "--plugin-enable",
            "--plugin-disable",
            "--result",
            "--wait",
            "--inspect",
            "--timeout-secs",
            "--full",
            "--gc",
            "--older-than",
            "--help",
            "--version",
            "--base",
            "--context",
            "--concurrency",
            "--file-profile",
            "--synthesis",
            "--synth-profile",
            "--no-repo-memory",
            "--dry-run",
            "--bash",
            "--distill-profile",
            "--distill-min-lines",
            "--timeout-ms",
        ];

        for flag in flags {
            assert!(HELP.contains(flag), "help text must document {flag}");
        }

        // The scripted-caller contract sections must survive too.
        assert!(HELP.contains("EXIT CODES"));
        assert!(HELP.contains("STORAGE"));
        assert!(HELP.contains("AUTOMATION RECIPES"));
    }

    #[test]
    fn help_text_is_printable_verbatim() {
        // The template ends with exactly one newline; println! appends the
        // second, so stdout ends in "\n\n".
        assert!(HELP.ends_with("says which case applies\n"));
        assert!(!HELP.ends_with("\n\n"));
        assert!(HELP.starts_with('\t'));
    }
}
