# drip — Local Code Inference, in Rust

A headless-first coding-agent harness driven from the terminal.  
`drip "goal"` spawns an agent loop, persists its session under `~/.drip/projects/<slug>/sessions/<id>/`,
and exits with a structured result. The TUI (`drip --tui`) and the `dripw`
watcher are monitors layered on the same session store — not the primary
interface.

drip compiles to a single static binary, starts in a few milliseconds, and
needs no runtime.

---

## Install

```sh
cargo install --path .        # puts `drip` and `dripw` on your PATH
# or build in place:
cargo build --release && ./target/release/drip "goal"
```

## Quickstart

```sh
# Run a goal headlessly
drip "add input validation to src/api/users.ts"

# Cap iterations and inject a skill
drip "refactor auth module" \
  --max-iterations 12 \
  --skill verify-before-done

# Machine-readable output (NDJSON; final line is the result)
drip "goal" --json
# → ... {"type":"result","reason":"...","summary":"...","exitCode":0,"taskStats":{...},"lastVerification":"...","continueCommand":"drip --resume <id> \"goal\""}

# Resume / steer / stop a running or paused session
drip --resume <id> "follow-up goal"
drip --send   <id> "please also handle edge case X"
drip --stop   <id>
drip --wait   <id> --timeout-secs 120   # exit code = run's exit code (or 124 on timeout)
drip --inspect <id>                     # run analytics: wall time, per-tool stats, verification timeline
drip --result  <id> --json              # replay result.json

# Run in the background and collect later
drip --detach "goal" --json             # prints {sessionId, pid, waitCommand}

# List sessions in this directory
drip --list
```

For the full flag reference run `drip --help`.

---

## Model providers

Model profiles live in `~/.drip/config.json` and reference credentials by name
(`"apiKeyRef": "env:NAME"`) from `~/.drip/env.vars`, so tokens never sit in
shell profiles. Every shipped hosted profile routes through
[OpenRouter](https://openrouter.ai/) on a single `OPENROUTER_API_KEY` — add
your vendor keys to your OpenRouter account (BYOK) and drip needs only the one:

```sh
echo 'OPENROUTER_API_KEY=sk-or-...' >> ~/.drip/env.vars   # or /env KEY=value inside drip
drip "goal"                                              # glm-5-3-flash, the default lane
drip --profile claude-opus-46 "goal"
```

| Profile id | OpenRouter model | Notes |
|------------|------------------|-------|
| `glm-5-3-flash` | `z-ai/glm-5.3-flash` | default active + tool-calling profile; the drafting lane every preset pins, and both `--review` lanes |
| `glm-5-3` | `z-ai/glm-5.3` | |
| `kimi-k3` | `moonshotai/kimi-k3` | the presets' reviewing lane; `--synth-profile kimi-k3` for the stronger `--review` synthesizer |
| `claude-opus-46` / `claude-sonnet-46` / `claude-haiku-45` | `anthropic/claude-*` | native Anthropic wire via OpenRouter's `/messages`, so `cache_control` prompt caching carries through |
| `openai-gpt54` / `-mini` / `-nano` | `openai/gpt-5.4*` | `reasoningEffort` passes through |
| `gemini-flash` | `google/gemini-2.5-flash` | |
| `xai-grok-46` / `-45` / `-43` | `x-ai/grok-*` | |
| `gpt-oss-120b` | `openai/gpt-oss-120b:nitro` | `:nitro` sorts upstreams by throughput |
| `local-default` / `ollama-local` | — | local runtimes, no key |

No shipped profile carries a fallback chain: OpenRouter fails over between
upstream providers itself, and the harness's retry ladder keeps its full
backoff against a route with nothing behind it. To bypass the aggregator for
one model, author a profile against the vendor's own base URL and key in
`~/.drip/config.json` — a saved entry always wins over the shipped default of
the same id, and `fallbackProfileId` still chains user-authored profiles.
`"provider": "openrouter"` is a first-class provider (default base URL
`https://openrouter.ai/api/v1`, OpenAI-compatible on the wire), so adding
another OpenRouter model is one profile entry with its `vendor/model` slug.

### Codex (ChatGPT subscription, no API key)

The built-in profile `gpt-5.6-luna-high` runs model `gpt-5.6-luna` with
`reasoningEffort: "high"` through the `codex` provider. There is no HTTP
endpoint and no Node SDK: drip is Rust and speaks the Codex CLI app-server
protocol directly (JSON-RPC over stdio, the experimental `dynamicTools` API,
developed against Codex CLI 0.153.2), resolving the route to the sentinel
`codex://local`. No API key, base URL, or headers are configured, and a codex
profile that sets any of `apiKey`/`apiKeyRef`/`baseUrl`/`headers` is rejected
at config load; the bridge also scrubs `OPENAI_API_KEY` from the subprocess
environment and refuses an API-key-billed Codex account, so this lane never
silently spends OpenAI API credits.

```sh
npm install -g @openai/codex   # official Codex CLI
codex login                    # authenticate with your ChatGPT account
drip --profile gpt-5.6-luna-high "goal"
```

Codex manages your ChatGPT subscription limits and additional ChatGPT credits;
these are separate from OpenAI API billing. Drip session resumes replay the
saved conversation into a fresh Codex thread. Codex profiles are ordinary
entries in `~/.drip/config.json`; author your
own with any model slug and `reasoningEffort`, and chain `fallbackProfileId`
like any other provider:

```jsonc
{
  "id": "my-codex",
  "model": "gpt-5.6-luna",
  "provider": "codex",
  "reasoningEffort": "high"
}
```

References: [ChatGPT login/auth for the Codex CLI](https://learn.chatgpt.com/docs/auth)
and the [app-server protocol](https://learn.chatgpt.com/docs/app-server).

---

## --json result contract

When `--json` is passed, drip emits NDJSON.  The **final line** is always:

```jsonc
{
  "type": "result",
  "reason": "completed",          // or "max-iterations" | "blocked" | "stopped" | "error"
  "summary": "...",
  "exitCode": 0,
  "taskStats": { "total": 5, "completed": 5, "blocked": 0 },
  "lastVerification": "cargo test: 24/24 passed",
  "continueCommand": "drip --resume <id> \"next goal\""
}
```

---

## Exit codes

| Code | Meaning |
|------|---------|
| 0    | Run completed (or informational command succeeded) |
| 1    | Usage / setup error |
| 2    | Run ended without completing (max-iterations, blocked, or stopped) |
| 3    | Infrastructure error (endpoint unreachable / 5xx after retries); state persisted, resume when healthy |
| 124  | `--wait` gave up after `--timeout-secs` (run keeps going) |

---

## Tool pack (9 tools)

| Tool | What it does |
|------|-------------|
| READ | Read file contents with line-based paging |
| PATCH | Apply targeted find/replace or full-content writes to files |
| DIR | Show project structure as a tree view |
| BASH | Run a bash command and wait for it to finish |
| BASH_ASYNC | Run a bash command in a detached tmux session (background) |
| GREP | Search files with a regex |
| VERIFY | Run a shell command and parse structured test verdicts (bun, vitest, pytest, unittest, cargo, go, tsc) |
| FETCH | HTTP GET a URL and return text content (requires `DRIP_ALLOW_NET=1`) |
| CHECK | Run incremental TypeScript diagnostics (semantic + syntactic) |

---

## Built-in role presets

The `--roles` flag accepts a built-in preset name (or a path to a roles.json
file using the same schema as config-sourced roles). Preset roles are merged
with config-sourced roles by name, with the preset taking precedence on name
collision. Four presets ship out of the box:

| Preset | Roles | Model pins |
|--------|-------|-----------|
| `reviewed` | A read-only planner, an author, and an independent reviewer that cannot use PATCH, enforcing review independence | planner/author `glm-5-3-flash`, reviewer `kimi-k3` |
| `research` | A single researcher role, denied PATCH, that investigates and reports findings with citations | researcher `glm-5-3-flash` |
| `team` | A researcher hands findings to a coder, whose work is then verified by an independent reviewer | researcher/coder `glm-5-3-flash`, reviewer `kimi-k3` |
| `planned` | A stronger architect writes each task as a contract (files, functions, verification command) for a small executor; the author implements them with no reviewer loop — "big model plans, fast model executes" | architect `kimi-k3`, author `glm-5-3-flash` |

Every preset's planning role is denied `PATCH` as well. The verify gate fires
when a *task* is finished, so a planning loop that could edit files would ship
changes no reviewer ever sees — denying PATCH there makes "plan only" enforced by
tool filtering rather than requested in a prompt.

Researcher, planner, and reviewer roles are denied `PATCH`, drip's only journaled
(`--undo-last`-able) write path. They keep `BASH`, so treat them as
"no journaled edits" rather than a write sandbox — a role can still shell out to
`sed -i`. If a role's model pin cannot be resolved (e.g. the profile was deleted
from `~/.drip/config.json`, or its provider key is missing from `~/.drip/env.vars`),
the role falls back to the run's base model and `--roles` prints a warning; when
the affected role is a verifier, that warning says review independence is no
longer enforced.

Every preset role pins its own model profile, so a preset routes reproducibly
regardless of the caller's active profile or `--profile`. Reviewer roles pin a
*different* model from the role they verify, so the verifying opinion never
comes from the model that wrote the code. Override a pin by defining a role of
the same name in `~/.drip/config.json` or `.drip/roles.json` — except that a
`--roles` preset takes precedence on name collision, so to re-pin a preset
role, copy the preset into a roles.json file and pass its path instead.

```sh
drip --roles reviewed "refactor the parser to stream tokens"
drip --roles research "map every call site of resolveBindings and report what each assumes"
drip --roles team "migrate the test suite from jest to vitest, verified end to end"
```

---

## Skill role hints (frontmatter)

Skills may carry an optional `roles:` block in their frontmatter that hints
which roles should handle which stages of the skill. Hints are **advisory
only** — drip does not route work automatically. A short guidance block is
added to the composed session prompt (for skills activated via `--skill`,
`--roles`-loaded roles, or slash activation), and every role change happens
the way it always does: through explicitly role-tagged `plan_tasks` entries.

The contract, as rendered into the prompt:

- An explicit role the user assigned, or one a task already carries, always
  wins over a hint.
- Only roles that already exist in the session's role configuration are used.
  If a suggested role is unknown or unavailable there, the task keeps the
  configured default role — hints never invent roles or grant extra tools.
- A stage hint applies to that skill's own stage. With multiple skills active,
  take each hint from the most relevant skill; if two skills suggest
  different roles for the same stage, surface the conflict and pick
  explicitly rather than silently overriding.
- Realize stage transitions by scheduling explicitly role-tagged tasks per
  stage — for example, a separate planner triage task after a review
  produces findings — so every role change goes through the existing
  scheduler.

### Frontmatter syntax

The `roles:` block is intentionally lightweight — no YAML engine is used:

- `roles:` alone on its own line opens the block; entries are
  two-space-indented `key: value` lines (`default: <role>` and/or
  `<stage>: <role>`).
- A blank line, a non-indented line, or a deeper-indented line (nested YAML)
  ends the block; later entries are not read as hints.
- The first `default:` wins, and a repeated stage name keeps its first role.
- The scalar form `roles: author` is not supported; it is ignored and the
  skill simply carries no hints.

One shared parser backs every activation path (`--skill`, `--roles`-loaded
roles, and slash activation), so a given file yields the same hints however
the skill was activated.

### Example: a ship skill with per-stage roles

`examples/skills/navis/SKILL.md` shows the full pattern. Copy it into your
project at `.drip/skills/navis/SKILL.md` (or `~/.drip/skills/navis/` for
user-wide use) — it is an example file, not a built-in. The frontmatter:

```markdown
---
name: navis
description: Full ship workflow — implement, verify, review, triage findings, fix, and open a draft PR
roles:
  default: author
  planning: planner
  implementation: author
  review: reviewer
  triage: planner
  fixes: author
  shipping: author
---
```

The stages mirror the ship loop: planning and the post-review triage want the
planner, review wants a reviewer, and the rest stay with the author. Activate
with `drip --skill navis "ship this goal"` (or `/navis` in a session). After
the independent review runs, the planner triages its findings and queues the
warranted fixes for the author, all through the normal task list:

```json
{ "placement": "next", "tasks": [
  { "title": "Read saved review report; verify findings; queue warranted fixes as author tasks; do not implement", "role": "planner", "dependsOn": ["<review-task-id>"] },
  { "title": "Fix <finding>", "role": "author", "dependsOn": ["<triage-task-id>"] }
] }
```

### Role names are configuration, not built-ins

Hints reference role names, so they port across presets and custom configs:
the `reviewed` preset provides `planner`/`author`/`reviewer`, while `planned`
provides `architect`/`author` (no reviewer). A skill suggesting
`review: reviewer` under `planned` simply falls back to the configured
default for that stage until you add a `reviewer` role to your `roles.json`.

## Code review (`--review`)

`drip --review` reviews the branch's diff against the repo's default branch
(`--base <ref>` to override) without editing anything: the changed files are
planned into review units — docs and manifests in one, a source file with its
tests, small same-directory files together (at most 3 files / 400 diff lines
per unit), big files alone, and a single file over 800 diff lines split into
consecutive-hunk chunks of at most 400 lines (`path [i/n]`, reviewed in
parallel; its `files[]` rows carry `part: "i/n"`) — and each unit gets its own
concurrent read-only child session on a fast model, budgeted (a stated tool-call budget, a cycle
cap, 150 s per request, 8 min wall clock) and retried once if it errors or
comes back incomplete. One holistic synthesis pass on a stronger model then
deduplicates the findings, checks that the change accomplishes its stated
intent, and writes the report — unless every unit came back clean, in which
case `--synthesis auto` (the default) skips it. `--context` is required — it is the statement
of intent every reviewer judges the diff against. Progress (the plan, each
finished unit, the synthesis) is printed to stderr as it happens.

```sh
drip --review --context "stream tokens through the parser instead of buffering the whole file"
drip --review --context "..." --base origin/main --concurrency 2 --json
```

Findings are P0 (blocks the change), P1 (important), or P2 (worth fixing
before merge) — nothing below P2 is reported — and the confidence score (5/5
down to 1/5) is computed from the P0/P1 counts, not by a model. The command exits 0 when no
P0/P1 findings remain and 4 when some do, so a fix loop can branch on it.
`--file-profile` / `--synth-profile` re-point the two lanes (both default to
`glm-5-3-flash` via OpenRouter; `--synth-profile kimi-k3` is the stronger
synthesizer).

---

## Session storage

Sessions are stored under `~/.drip/projects/<slug>/sessions/<id>/`, keyed by the
project root's path slug — the same scheme the memory bank uses. Repo-scoped
data (`patches.jsonl`, `async-tools/`, `skills/`, `roles.json`, `plugins.json`)
stays in `<repo>/.drip/`. `DRIP_HOME` relocates the home directory;
`DRIP_PROJECT_DIR` / `--project-dir` pins the project root.

**Renaming a session.** Type `/rename` in the TUI composer and the configured model distills the
session transcript into a short 5-7 word name. The name replaces the window title and is persisted
to the session's `session.json` metadata, so resume keeps it; on any failure the
current name is kept.

---

## Custom status line


In the interactive TUI you can replace the built-in bottom status bar with the
output of a shell command — the same idea as Claude Code's status line. The
setting lives in drip's own persisted config file, `~/.drip/config.json`
(`DRIP_HOME` relocates the home directory), as a top-level `statusLine` object
next to `settings`:

```json
{
  "statusLine": {
    "type": "command",
    "command": "~/.drip/statusline.sh",
    "padding": 0,
    "updateIntervalMs": 300,
    "timeoutMs": 5000
  }
}
```

| Key | Default | Notes |
| --- | --- | --- |
| `type` | `"command"` | `"command"` is the only supported kind. |
| `command` | (required) | Shell command, at most 4096 characters; `~` is expanded. |
| `padding` | `0` | Blank cells added on both sides of the row; clamped to 0-4. |
| `updateIntervalMs` | `300` | Minimum milliseconds between runs; clamped to 100-60000. |
| `timeoutMs` | `5000` | Kill a run after this many milliseconds; clamped to 100-30000. |

Behavior:

- No `statusLine` key (or `null`) keeps the built-in status bar unchanged. An
  invalid `statusLine` (wrong `type`, empty command, values out of range,
  wrong JSON shape) prints one nonfatal warning naming the config file and the
  offending key, and drip starts with the built-in bar — an invalid command is
  never executed.
- Each run gets the session's working directory as its cwd and one JSON
  document on stdin (stdin is closed immediately, so readers observe EOF):

```json
{
  "session_id": "…",
  "workspace": { "current_dir": "/path/to/repo" },
  "model": { "id": "…", "display_name": "…" },
  "version": "…",
  "render_width_chars": 120,
  "context_usage": 0.35
}
```

  `session_id`, `workspace.current_dir`, `model.display_name` and
  `render_width_chars` (terminal width) carry real values. Everything drip
  does not genuinely know is `null` or omitted rather than invented:
  the `workspace` and `model` objects are always present but their
  fields are omitted when unavailable, and `version`, `model.id` and
  `context_usage` are reserved fields that are currently always
  `null`/omitted.
- The command runs under `/bin/sh -c` (POSIX) or `cmd /C` (Windows). Only its
  stdout becomes the status row: output is capped at 8,192 characters, only
  the first line is shown, SGR color escapes are kept (a reset is appended so
  colors cannot bleed into the TUI), and OSC sequences, cursor movement and
  other control characters are stripped. The row is truncated to the terminal
  width (Unicode-aware: East-Asian wide characters count as two cells) with
  `padding` applied inside that width, and re-fits on resize without
  re-running the command.
- While the TUI is open the command re-runs no more often than
  `updateIntervalMs` (plus immediately on resize), with at most one run in
  flight. Runs that exceed `timeoutMs` are killed, together with their
  descendants where the platform supports process-group kills.
- Fallbacks: a nonzero exit or a timeout shows the built-in bar for that
  refresh; empty output and exit 127 (command not found) count as "nothing to
  show" and also fall back to the built-in bar, with no error spam.
- **Trust boundary:** `statusLine.command` is arbitrary local code that drip
  executes repeatedly while the TUI runs. Only put commands there that you
  control. drip never reads `~/.claude/settings.json`; to reuse an existing
  Claude status-line script, point drip's command at it explicitly without
  touching your Claude settings:

```json
{ "statusLine": { "type": "command", "command": "~/.claude/statusline.sh" } }
```

  Claude Code payload fields drip does not provide: `transcript_path`,
  `workspace.project_dir`, `output_style`, `cost` and `exceeds_200k_tokens`
  (scripts receive `null`/omitted values instead). drip additionally offers
  `updateIntervalMs`/`timeoutMs`, and its `padding` is a number (0-4), not
  Claude Code's boolean. A working POSIX example lives in
  `examples/statusline/statusline.sh`, and `tests/statusline_example.rs`
  validates it against a payload produced by the real implementation.

---


## Terminal pane title

While a goal runs, drip sets the terminal window title (OSC 2) so activity is
visible at a glance in the terminal tab or tmux pane, similar to Claude Code
or Codex. The title is a short stable label plus a time-driven braille
spinner while the harness runs — no fabricated percentage. On completion,
cancel, or exit the spinner is removed and the bare label remains.

- The label starts as a deterministic 3–5-word summary of your goal (its
  first usable words, or `drip` when nothing usable remains).
- If the lightweight title profile is reachable, drip replaces the label
  with a 3–5-word title generated from the initial goal of the session
  (default profile: `glm-5-3-flash` via OpenRouter). This is one short
  background request per session, run after the goal starts so it never
  blocks the chat. Only the initial goal is sent — never repository, tool,
  or transcript content — and the result is sanitized and capped at 5
  words before it reaches the terminal.
- Missing credentials or profile, offline restrictions, timeouts, and
  malformed or empty model output are all silent: the deterministic
  fallback label stays and the chat is never affected. The title request
  happens once per session — later turns and spinner ticks never repeat it.

Settings (in drip's `settings.json`, under the `runtime` keys):

| Key | Default | Meaning |
| --- | --- | --- |
| `runtime.terminal_title_enabled` | `true` | Set to `false` to disable all pane-title updates |
| `runtime.terminal_title_profile_id` | `glm-5-3-flash` | Model profile used for the one-shot title generation |
| `runtime.terminal_title_timeout_ms` | `8000` | Timeout bound (ms) for the single title request |

Limitations: title escapes are written only when stdout is an interactive
terminal. Headless runs, `--json`, and redirected/piped output never receive
title escapes, and background or worker threads never write the title (the
generation thread only sends a message to the event loop). OSC 2 window
titles are supported by most terminals (iTerm2, Terminal.app, Windows
Terminal, kitty, WezTerm, Alacritty); terminals that ignore OSC 2 simply
show nothing. Under tmux the escape updates the pane title; mirror it onto
the outer terminal window with `set -g set-titles on` (plus
`allow-passthrough on` on tmux 3.3+ if needed). drip emits only OSC 2
window-title escapes — no progress-bar protocols such as OSC 9;4.

## Contributing

PRs welcome. Please run `cargo build --release && cargo test` before submitting.
The Codex protocol tests also require Python 3 for their local mock server.

---

## License

MIT
