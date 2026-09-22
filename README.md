# drip — Goal based coding harness

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
cargo install --path .        # puts `drip`, `dripw`, and `drip-mcp` on your PATH
# or build in place:
cargo build --release && ./target/release/drip "goal"
```

## MCP server (drip-mcp)

Claude Code normally drives drip through the Bash tool. With Bash disabled,
`drip-mcp` keeps drip reachable: it is a stdio MCP server that passes each
call straight through to the `drip` CLI and returns its stdout, stderr, and
exit code as the tool result.

Register the server in `.mcp.json`:

```json
{"mcpServers":{"drip":{"command":"drip-mcp"}}}
```

or with the Claude CLI:

```sh
claude mcp add --scope user drip -- drip-mcp
```

To turn Bash off while keeping drip access, add this to Claude Code's
`settings.json`:

```json
{"permissions":{"deny":["Bash"]}}
```

The server exposes one tool, `drip`. Its `args` array is passed to the
`drip` executable verbatim — no shell in between:

```json
{"name":"drip","arguments":{"args":["--json","--max-iterations","10","fix the bug"]}}
{"name":"drip","arguments":{"args":["--detach","long-running goal","--json"]}}
{"name":"drip","arguments":{"args":["--wait","<sessionId>","--timeout-secs","600"]}}
```

Long goals should not block the tool call: start with `--detach` (it returns
`{sessionId, pid, waitCommand}` immediately) and collect with
`--wait <id> --timeout-secs N` in a follow-up call; the wait exits 124 on
timeout. Calls also accept `cwd` (working directory) and `timeout_secs`
(default 600), which kills a runaway `drip` process and reports it as a
tool error.

`drip-mcp` is a passthrough, not a sandbox: denying `Bash` removes the
shell tool from the model's reach, but anything `drip` itself is able to
execute, it still can.

## MCP client (per-role servers)

drip can also *consume* MCP servers: it spawns stdio servers declared in
config, lists their tools, and offers them to the model next to the built-in
pack as `MCP__<server>__<tool>`. Which servers a loop sees is decided by the
loop's **role**, so an author can reach a `github` server while the reviewer
that checks its work never can.

Declare servers once, under a top-level `mcpServers` section of
`~/.drip/config.json` (the same shape as Claude Code's `.mcp.json`), or per
project in `.drip/mcp.json` with a `{"mcpServers": {...}}` body. Project
entries win on name collision:

```json
{
  "mcpServers": {
    "github": {"command": "github-mcp-server", "args": ["stdio"], "env": {"GITHUB_TOKEN": "..."}, "timeoutSecs": 60}
  }
}
```

`args`, `env`, and `timeoutSecs` (per-call and handshake timeout, default 60)
are optional. Only stdio transport is supported. An entry without a `command`
(for example a remote `{"type":"sse","url":...}` entry), a malformed entry, or
a server name containing `__` (it separates `MCP__<server>__<tool>`) is
skipped with a warning; its siblings still load.

Roles opt in with `mcpServers` — a list of server names — in any role source
(`runtime.role_profiles` in the config, `.drip/roles.json`, or a `--roles`
file). A role's `tools` allowlist never has to name MCP tools: `mcpServers`
is their opt-in, and a role without one sees no MCP tools at all. For a run
without roles, or for loops whose role sets no `mcpServers`, pass
`--mcp <name>[,<name>...]` (repeatable) to enable servers run-wide;
`--no-mcp` spawns nothing and exposes nothing regardless of roles.

```sh
drip --mcp github "triage the open issues labelled bug and draft fixes"
drip --roles ./roles.json "..."   # roles.json: {"roles":[{"name":"author","mcpServers":["github"]}]}
```

Every server any role in play asks for is spawned once per invocation and
killed when the run ends. Startup is never fatal: a missing binary, a failed
handshake, or a name absent from `mcpServers` prints a `mcp: server "<name>"`
warning and exposes fewer tools. MCP calls never count as workspace progress
for stall accounting, `--plan` keeps its reader-only surface, and `--review`
children, `DELEGATE` children, and the watch TUI get no MCP surface.

## Quickstart

```sh
# Run a goal headlessly
drip "add input validation to src/api/users.ts"

# Cap iterations and inject a skill
drip "refactor auth module" \
  --max-iterations 12 \
  --skill verify-before-done

# Cap task loops instead of cycles: every loop is at least one model call and a
# replanning loop is exactly one planner call, so this bounds planner spend
# directly (an unfinished run resumes with the same cap)
drip "repair the corrupted shards" --max-loops 30

# Other budget/planning flags
# --task-loop-limit N: task loops one task may consume before the harness blocks it (default 6)
# --plan-mode always|auto|direct: how a run gets its first task list; auto skips the planner
# for small goals that declare their own check

# In the TUI: type `/` followed by a skill prefix (e.g. `/na`) — matching
# skills appear above the input line; up/down selects, tab completes, and
# enter enables the skill for that session. Typing a full skill name as a
# command (e.g. `/navis`) also enables it without starting a run.

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
shell profiles. The config file is the only source of model and system-prompt
profiles: a starter catalog is written into it once, when the file is first
created, and after that drip never merges, backfills, or looks up profiles
from its compiled-in defaults — a profile id absent from your config fails
with an error telling you to add it there. Every hosted profile in that
starter catalog routes through [OpenRouter](https://openrouter.ai/) on a
single `OPENROUTER_API_KEY` — add your vendor keys to your OpenRouter account
(BYOK) and drip needs only the one:

```sh
echo 'OPENROUTER_API_KEY=sk-or-...' >> ~/.drip/env.vars   # or /env KEY=value inside drip
drip "goal"                                              # glm-5-3-flash, the default lane
drip --profile claude-opus-46 "goal"
```

A minimal profile entry (settings are nested JSON; this one routes an
OpenRouter model through your key without authoring any vendor key into the
file):

```jsonc
"runtime.model_profiles": [
  {
    "id": "glm-5-3-flash",
    "model": "z-ai/glm-5.3-flash",
    "provider": "openrouter",
    "apiKeyRef": "env:OPENROUTER_API_KEY"
  }
]
```

Starter profile ids (see `~/.drip/config.json` for the full seeded list):

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

No starter profile carries a fallback chain: OpenRouter fails over between
upstream providers itself, and the harness's retry ladder keeps its full
backoff against a route with nothing behind it. To bypass the aggregator for
one model, author a profile against the vendor's own base URL and key in
`~/.drip/config.json` — your config is the only source, so an entry there is
just that profile, and `fallbackProfileId` still chains profiles within the
same list.
`"provider": "openrouter"` is a first-class provider (default base URL
`https://openrouter.ai/api/v1`, OpenAI-compatible on the wire), so adding
another OpenRouter model is one profile entry with its `vendor/model` slug.

### Codex (ChatGPT subscription, no API key)

The starter profile `gpt-5.6-luna-high` — written into your config on first
run, like every other profile — runs model `gpt-5.6-luna` with
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
  "reason": "completed",          // or "unreconciled" | "max-iterations" | "max-loops" | "blocked-on-input" | "blocked" | "stopped" | "error"
  "summary": "...",
  "exitCode": 0,
  "taskStats": { "total": 5, "completed": 5, "blocked": 0 },
  "lastVerification": "cargo test: 24/24 passed",
  "continueCommand": "drip --resume <id> \"next goal\"",
  // present when the run used the anchoring mechanisms (see "Anchoring, expectations, and anomalies"):
  "completionAnchor": { "kind": "none", "note": "...", "claimedConfidence": "medium" },
  "anomalies": [ { "subject": "...", "expected": "...", "observed": "...", "note": "..." } ]
}
```

---

## Exit codes

| Code | Meaning |
|------|---------|
| 0    | Run completed, or finished `unreconciled` (every task done, an expectation left visibly unreconciled) — or an informational command succeeded |
| 1    | Usage / setup error |
| 2    | Run ended without completing (max-iterations, max-loops, blocked-on-input, blocked, or stopped) |
| 3    | Infrastructure error (endpoint unreachable / 5xx after retries); state persisted, resume when healthy |
| 124  | `--wait` gave up after `--timeout-secs` (run keeps going) |

`--bash` is the exception: it exits with the wrapped command's own code (124 on
timeout, `128+n` when a signal killed it), and its stderr header says which case
applies.

---

## Tool pack (9 tools)

| Tool | What it does |
|------|-------------|
| READ | Read file contents with line-based paging |
| PATCH | Apply targeted find/replace or full-content writes to files |

PATCH repairs the argument shapes small models get wrong instead of
spending a round on the error: a `content` sent beside `replace` with no
`find` is the old text; `content` beside `find` is the replacement; an entry
of `files` with no `path` edits the call's top-level `path` or the nearest
earlier entry's file; and a `find` that misses only by indentation (copied
from a READ window at a different depth) matches the one window of the file
with the same lines, with the replacement shifted by the same indentation
and the summary saying so. Two windows, or a tab-versus-space mix, still
report "not found".
| DIR | Show project structure as a tree view |
| BASH | Run a bash command and wait for it to finish |
| BASH_ASYNC | Run a bash command in a detached tmux session (background) |
| GREP | Search files with a regex |
| VERIFY | Run a shell command and parse structured test verdicts (bun, vitest, pytest, unittest, cargo, go, tsc) |
| FETCH | HTTP GET a URL and return text content (requires `DRIP_ALLOW_NET=1`) |
| CHECK | Run incremental TypeScript diagnostics (semantic + syntactic) |
| REFERENCE | Hybrid (BM25 + dense) search over an oasis-indexed markdown corpus — only in the pack when a corpus root is configured |

### Verification evidence

An exit code of zero alone does not verify an artifact. VERIFY records executed
test/assertion counts, compiler/build evidence, or an unverified outcome in
`lastVerification.evidence`. Unknown scripts, empty/all-skipped suites and legacy
records without evidence cannot satisfy completion after workspace edits.
Failed mutating calls also invalidate earlier checks because they may have
changed files before failing. Repeating `finish_task completed` does not waive missing, failed or stale checks;
use `blocked` when the necessary evidence cannot be obtained.

A finish that is stale only because edits landed after the last check is
re-verified by the harness itself: it re-runs that same check (the agent
already ran it, so it is policy-vetted), records the result, and accepts the
finish in the same round when it passes; a failing re-run refuses the finish
with the failure tail. This saves a model round per stale finish.

A finish with no check run at all (or only self-authored passes) gets the
same treatment when the goal itself names a check in backticks, such as
`` `python3 -m unittest discover -s tests -q` `` or `` `cargo test` ``: the
harness runs that goal-declared command once per loop as an external,
task-provided anchor and accepts the finish if it passes; because the goal
itself designated that command, it stays external even when it names files
the run edited. Put the acceptance command in the goal text to enable this.

For a custom checker, emit exactly one line after executing its assertions:

```text
DRIP_VERIFY {"executed":3,"passed":3,"failed":0}
```

Counts must be nonnegative integers with `executed = passed + failed`; at least
one check must execute. Malformed or multiple records fail verification. A
nonzero process exit, timeout or recognized failing assertion overrides a
success record. VERIFY enables shell `pipefail` so piping output through `tail`
does not hide a checker's failing exit. With BASH, preserve pipeline exit status
explicitly (for example, `set -o pipefail`). Both tools consume this protocol; launching
a background process is not verification. Use a test framework such as unittest
when possible, or collect the counters from actual checks rather than printing
hardcoded counts. Check formatting and values against the task's requirements.

Direct compiler commands (for example `cargo check`, `cargo build`, `tsc --noEmit`
and `go build`) provide build/typecheck evidence without inventing test counts.
Arbitrary package scripts do not become evidence just because they are named
`build` or `check`: use recognized runner output or invoke the underlying compiler
directly. CHECK records compiler evidence for its requested scope. Build evidence
does not establish runtime behavior or numerical correctness.

These are reported checks, not proof that the checker chose the correct formula,
constants or specification. Numeric deliverables need a different validation
route and an explicit account of shared assumptions. Repeating the same arithmetic
or comparing to a hardcoded expected file establishes internal consistency only.
Known defects affecting a reported value or goal requirement must be resolved or
reported as blocked, even if discovered in the author's own closing notes.
Ordinary statistical uncertainty or a justified limitation is not itself a defect.

### Anchoring, expectations, and anomalies

Verification evidence says whether the agent did work; it says nothing about
whether the work is anchored to anything the agent did not author. Two routes
that share one wrong assumption agree with each other, and 21/21 assertions
confirm a wrong specification precisely. The harness therefore types evidence
and closes the exits that let an anomaly be explained away:

- **Correctness vs consistency.** `VERIFY` accepts
  `anchor: {"kind": "external" | "self", "source": "..."}`. External means the
  check compares against something the agent did not author — a pre-existing
  project test, a task-provided fixture, a published constant, an invariant
  independent of the implementation. Self means the check derives from the
  agent's own implementation. An `external` claim on a command that names a file
  the run has edited is downgraded to self-authored (with a run warning): a
  check the agent wrote is consistency, not correctness. The reverse promotion
  also holds: a project suite run through a native runner (`cargo test`,
  `pytest`, `unittest`, `go test`, `vitest`, `bun test`, `npm test`) whose
  command names no file the run edited is external evidence even when the
  agent left the anchor undeclared or labelled it self — the suite pre-exists
  the run — and the harness records the promotion as an event (recorded
  sessions hit the iteration cap re-finishing behind "no correctness-class
  evidence" after running exactly that suite). Finishing a task that
  edited the workspace needs one passing external-anchored check, or
  `finish_task` `anchor: "none"` plus an `anchorNote` saying why no external
  anchor exists. The declaration is recorded as `completionAnchor` and
  downgrades the completion in the result rather than hiding it.
- **Pre-registered expectations.** `plan_tasks` accepts
  `expectations: [{subject, expected}]` — the expected sign, unit, order of
  magnitude, row count, output shape, or latency bound, stated from the domain
  before any result exists. Expectations are immutable once written: a later
  call may add subjects or repeat one verbatim, never rewrite one.
  `finish_task` records `observations: [{subject, observed, matches, evidence?}]`
  and every registered expectation must be observed before a task can finish.
- **Output-changing revisions.** An observation whose value differs from an
  earlier one for the same subject must cite `evidence` from outside the fix
  that the new value is closer to truth; otherwise it is refused. The history
  is append-only, so the walk is visible.
- **`unreconciled`.** A mismatched observation refuses `completed` — it is a
  P1 against the model, not a caveat on the value — and names the outlet:
  `finish_task` `status: "unreconciled"` with a non-empty `anomalies` list.
  The task ends as done, the run's reason is `unreconciled`, the exit code is
  0, and `anomalies` ride in the result payload. Surfacing an anomaly is
  cheaper than arguing it into plausibility.
- **Blind roles.** A role with `blind: true` (the reviewer preset defaults to
  it; set it on any role in `~/.drip/config.json` or `.drip/roles.json`) starts its loops without the
  previous loop's tool exchanges and without the author's footprint, emitting a
  `context-withheld` event instead of `context-refreshed`. The reviewer sees the
  goal and the artifact, not the derivation, so its agreement is independent by
  construction rather than by request.
- **Confidence self-report.** `finish_task` requires `confidence: low | medium | high`,
  persisted on the finished task record in state.json as the agent's own
  self-report, so a confident wrong answer costs more than an uncertain one.

### Pointing REFERENCE at a corpus

REFERENCE searches a markdown knowledge base through the
[`oasis`](https://github.com/a1re1/o-cs) search engine, so a run can answer from a
reference corpus (for example the o-cs computer-science wiki) with citable page paths
instead of from the model's memory. It needs the `oasis` binary on PATH and at least one
corpus root:

```sh
drip --reference-root ~/src/o-cs/wiki "explain why our queue needs backpressure"
export DRIP_REFERENCE_ROOTS=~/src/o-cs/wiki   # same thing, colon-separated for several
```

`--reference-root` is repeatable, `DRIP_REFERENCE_ROOTS` and oasis' own `OASIS_ROOTS`
are read as fallbacks, and with no root configured the tool is left out of the pack
entirely — the model never sees a tool that cannot work. Pair it with
`--skill cs-reference` to prime the search → read → cite loop. The wiring's retrieval
quality and its effect on answers are measured by the benchmark in
[`evals/reference/`](evals/reference/README.md).

### Clarification questions (`--ask` / `--no-ask`)

Clarification surveys are on by default: the `ask_user` harness tool lets the model
ask a staged survey of multiple-choice questions (each with suggested options, a
free-text "Type something" answer, and "Chat about this" for one free-form reply to
the whole survey) when the goal is ambiguous or an approach tradeoff needs the
operator's call, then revise its plan around the answers before implementing. The
model may only ask while planning — right after the goal or a fresh operator message
(`--send`, or a queued/steered TUI message) and before the first task loop starts;
once the plan is executing the tool is withheld and the run finishes on its own.
The defaults live in `~/.drip/config.json`: `runtime.ask_user_interactive` (the TUI)
and `runtime.ask_user_headless` (headless runs), each `"true"` unless set to `"false"`.
`--no-ask` turns surveys off for a run and `--ask` forces them on; either explicit
choice is pinned on the session, so `--resume` keeps it.

Headless, the survey arrives as a `question` event and the run blocks until answers
land (or `--ask-timeout`, default 900s, ends it with reason `awaiting-input` — answer
later and resume):

```sh
drip --json --ask "migrate the config loader"        # emits a question event, blocks
drip --answer '{"answers":[{"index":0,"choice":"Keep JSON"},{"index":1,"other":"only the CLI half"}]}'
drip --answer "just do whatever is least invasive"   # plain text = free-text answer (single-question surveys only)
```

In the TUI (`--tui --ask`), the survey opens as a picker overlay — arrow keys choose
an option per question, `Other…` drops into free-text entry — and the answers feed the
same channel.

A task that needs material only the operator can supply (original data,
credentials, a decision) is finished with `finish_task {status: "blocked",
blockedOn: "operator"}`. Such a task is never auto-reopened, and once nothing
else is workable the run ends with reason `blocked-on-input` (exit 2) instead
of replanning around the gap — the finished work stays intact. Resume the
session with the missing input as the prompt: it lands on the task's notes and
the task goes back to pending.

## praeparare (pre-PR prep)

`drip --praeparare ["extra operator context"]` — or `/praeparare` in the TUI — runs an
agentic pre-PR pass on the current branch: it detects the project's formatter, linter,
and test commands, runs them, and fixes failures; removes stray debug or scratch
changes; commits everything with a descriptive message; merges the base branch; pushes
with `-u`; and opens a DRAFT PR (or pushes to the existing one) whose body carries
**Goal**, **Changes**, and **Testing** sections. The optional argument is extra operator
context appended to the shared canned goal; in the TUI, `/praeparare <context>` runs it
through the same helper, so both faces append it identically.

```bash
drip --praeparare "the scratch notes in NOTES.md are intentional, leave them"
```

The pass gets a default budget of 15 iterations unless `--max-iterations` is given.
The TUI command has no per-goal flag, so it applies that same default to the whole
session — and only when no budget is already set; an explicitly chosen budget always
wins.
It never squashes, rebases, amends, or force-pushes, and only ever creates DRAFT PRs —
never ready-for-review ones. It refuses to run on the base branch itself and requires
`gh auth status` to succeed before making any change.

## Lite mode and review opt-out

`drip --lite` runs a draft pass: a single author lane, no reviewer task, no completion
anchor gate, and no end-of-run summary. The run ends with reason `draft` (exit code 0)
and prints a `continueCommand` for the full-rigor pass. `--no-review` skips just the
verified_by review chain and the finish_task completion-anchor gate for this run; it is
usable with any `--roles` preset and is implied by `--lite`.

Draft, then harden:

```sh
drip --lite "draft the parser migration"
# ends with reason "draft" and prints:
drip --resume <sessionId> --roles reviewed --skill verify-before-done --new-goal "Harden the draft: draft the parser migration"
```

---

## Built-in role presets

A role a later source defines again (`.drip/roles.json` over the config,
a `--roles` file over both) overrides only the fields it sets, so
`{"roles":[{"name":"planner","reasoningEffort":"medium"}]}` keeps the config
planner's model and prompt; replacing the whole definition used to drop the
model silently and run the planning loop on the base model.

A role bound to planning or replanning runs at medium reasoning effort when
it sets none and its model profile says high or nothing (a planning route
never goes out with no effort, which the model caller would otherwise read
as a tool round and send at low): a same-window A/B (five tasks
× 2, planner gpt-6-astra via codex) halved the planning call at medium
(6.6-12.4s vs 12.6-24.6s) with the same hidden-test pass rate and the same
plans, so planned runs finished 30-40% sooner. Set `reasoningEffort` on the
role to keep high.

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

The review is deferred: finishing a task under a role with a `verifiedBy`
reviewer marks it as awaiting review, and one review task covering every
finished task is spawned once no author work remains (the last task finished
or was dropped). A run with N tasks pays for one reviewer loop instead of N;
the review task's title names every task it covers and its note carries each
task's recorded footprint. A rejection reopens the most recently finished task
with the reviewer's summary as its rework note.

Researcher, planner, and reviewer roles are denied `PATCH`, drip's only journaled
(`--undo-last`-able) write path. They keep `BASH`, so treat them as
"no journaled edits" rather than a write sandbox — a role can still shell out to
`sed -i`. If a role's model pin cannot be resolved (e.g. the profile was deleted
from `~/.drip/config.json`, or its provider key is missing from `~/.drip/env.vars`),
the role falls back to the run's base model and `--roles` prints a warning; when
the affected role is a verifier, that warning says review independence is no
longer enforced.

The `reviewer` role is also `blind`: its loops never inherit the author's tool
exchanges or footprint, so it judges the artifact against the goal and against
anchors the author did not write. Any role definition (`~/.drip/config.json`
or `.drip/roles.json`) can set `blind: true`.

Any role can also set `reasoningEffort` (`"low"`, `"medium"`, `"high"`) to
override the model profile's own setting for that role's loops. Tool-round
latency is decode-bound (roughly 130 tokens/s on GLM-5.3 Flash, with most calls
spending their time on reasoning tokens), so an author on a fast model usually
wants `"low"` while the planner keeps the profile default.

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

## Skill classifier (jev)

Discovered skills are not all useful at once. With a classifier configured, drip
asks a [jev](https://typesafe.ai) Decisions model one small question per loop —
"would this skill help with the current task?" — and composes only the skills
that pass into that loop's system prompt. The classifier **never** replaces
explicit `--skill` activations: those are always in the prompt, and they are
excluded from the candidate pool. Every failure (bad profile, HTTP error,
timeout, malformed answer, unreadable `classifiers.json`) is non-fatal: drip
warns on stderr and runs with the explicit skills only.

### Settings

| Setting | Default | Meaning |
| --- | --- | --- |
| `runtime.classifier_profile_id` | `""` | Model profile id used for classification; empty disables the feature |
| `runtime.classifier_timeout_ms` | `"8000"` | Whole-selection budget per loop (and per requirements pass) |

### Flags

- `--classifier <profile-id>` — classify with this profile for this run
  (overrides the setting).
- `--no-classifier` — hard off, regardless of flag order.

### Model profiles

The classifier profile is an ordinary model profile; point it at jev:

```json
{"id":"jev","label":"Jev (OpenRouter)","model":"~typesafe/jev-latest","provider":"openrouter","apiKeyRef":"env:OPENROUTER_API_KEY"}
```

```json
{"id":"jev-direct","model":"jev-latest","provider":"typesafe","apiKeyRef":"env:TYPESAFE_API_KEY"}
```

The `typesafe` provider (`baseUrl` defaults to `https://api.typesafe.ai/v1`) is
only valid on the classifier profile. The decisions endpoint is derived from the
profile's base URL: `/v1` is stripped and `/alpha/decisions` appended for
`openrouter`, `/systemone` for `typesafe`. Credentials resolve exactly like any
other profile (`apiKey` / `apiKeyRef`, `env:` refs reading `~/.drip/env.vars`)
and are never logged.

### Requirements cache

The first pass asks, per skill and per capability (one tool, or one MCP server),
whether following the skill requires it. Answers land in
`~/.drip/skill-requirements.sqlite` keyed on **the skill body hash plus the
capability descriptor hash**, so editing the skill markdown (or a tool
description, or an MCP server's tool list) re-asks, and nothing else does. A
skill whose classifier call fails is treated as satisfiable by everything — the
feature never hides a skill because the classifier was down. The cache is
incremental and safe to delete.

A skill that declares `requirements` in `classifiers.json` skips the classifier
entirely and stores no cache row: the author stated it.

### Selection rules

- **Unauthored skills** (no `relevance` block): one batched request, one yes/no
  question each, against the loop state. A skill is included when its
  probability is ≥ 0.6; included skills are ranked by score and **capped to
  the top 4**.
- **Authored skills** (`relevance` in `classifiers.json`): one request each,
  with the author's questions verbatim and the same state. The score is
  `clamp(<formula>, 0, 1)` (or, with no `formula`, the maximum answer value).
  The skill is included when the score is ≥ its own `threshold` (default 0.6).
  **No cap applies** to authored skills.
- Selected skills come first by score, then unauthored ones by score. All
  requests for one selection run concurrently inside the timeout.
- A skill whose requirements (known, at probability ≥ 0.7) are not all present
  in the loop's tool/MCP surface is filtered out before any request is made.

### `classifiers.json`

Optional sidecar next to a skill's `SKILL.md`. Built-in skills can never have
one.

```json
{
  "relevance": {
    "threshold": 0.65,
    "questions": {
      "is_migration": { "type": "noul", "instructions": "Does the task change a database schema?" },
      "risk": { "type": "score", "instructions": "How risky is the change?", "criteria": ["trivial", "moderate", "data-loss possible"] },
      "phase": { "type": "choice", "instructions": "Which phase is this?", "criteria": { "design": null, "implement": null, "review": null } }
    },
    "formula": "0.5 * is_migration + 0.3 * risk + 0.2 * max(phase.implement, phase.review)"
  },
  "requirements": { "tools": ["BASH", "PATCH"], "mcpServers": [] }
}
```

`questions` are native jev questions, sent verbatim. In a formula, a bare
question id resolves to that answer normalized to 0..1, and `id.member`
resolves to one option/level probability:

| Question type | Bare id | `id.member` |
| --- | --- | --- |
| `noul` | probability of yes | — |
| `choice` | probability of the chosen (top) option | probability of that named option |
| `score` | chosen level position / (levels − 1) | probability of that numeric level |

Formula grammar: decimal literals; identifiers `[A-Za-z_][A-Za-z0-9_]*` with an
optional `.member` (`phase.implement`, `risk.0`); `+ - * /` with the usual
precedence; unary minus; parentheses; and `max(a,b,...)`, `min(a,b,...)`,
`clamp(x,lo,hi)`, `abs(x)`. An unknown variable or a syntax error drops that
skill with a warning; division by zero yields 0.

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

## Bash distillation (`--bash`)

`drip --bash` runs a shell command on behalf of a calling agent (Claude Code,
Codex, or any MCP client) and returns a short, context-guided distillation of
the output instead of the raw stream, so a `cargo test` or `grep -r` with
hundreds of lines of output costs the agent a few lines of context instead of
all of them. `--context` is required: it tells the distiller what the agent
expects, what counts as success or failure, and which facts to report back.

```sh
drip --bash "cargo test 2>&1" \
  --context "Expect all tests to pass. Report the pass/fail counts and, for any failure, the test name, file:line, and the assertion message verbatim."
drip --bash "grep -rn TODO src" --context "..." --json
```

- Output under `--distill-min-lines` lines (default 30) **and** under 3 KB is
  printed verbatim with no model call — tiny outputs cost more to distill than
  to read.
- Larger output goes through one tool-free call on `--distill-profile`
  (default `glm-5-3-flash`, cheap and fast). Output over 150 KB is trimmed to
  its head and tail around an omission marker before the call. The reply
  leads with a success/failure verdict, then the facts `--context` asked for
  (error messages, paths, and line numbers quoted exactly), then anything
  unexpected.
- If the model call fails or times out, the command still returns: a head+tail
  excerpt of the raw output is printed after a `(distillation failed: ...)`
  note (fail-open).
- A one-line header goes to stderr (`bash: exit 1 · 4s · 312 lines / 18204
  bytes → distilled on glm-5-3-flash (z-ai/glm-5.3-flash)`); the distilled
  text goes to stdout. With `--json`, stdout is one object: `command`, `cwd`,
  `exitCode`, `timedOut`, `signal` (only when a signal killed the command),
  `durationMs`, `totalLines`, `totalBytes`, `truncated`, `bypassed`,
  `distilled`, `profile`, `model`, `distillErrored`, `usage` (only when a
  model call ran), `distillMs`.
- The exit code is the wrapped command's own, so an agent's existing pass/fail
  handling keeps working; distillation failures never change it. A timeout
  exits 124 and the header says `exit timeout`; a signal death exits `128+n`
  like the shell (139 for SIGSEGV, 137 for SIGKILL) and the header says
  `exit killed by SIGSEGV`. `--timeout-ms` caps the command (default 120000).

Through `drip-mcp`, pass `["--bash", "<cmd>", "--context", "..."]` as the
tool's `args`.

---

## Session storage

Sessions are stored under `~/.drip/projects/<slug>/sessions/<id>/`, keyed by the
project root's path slug — the same scheme the memory bank uses. Repo-scoped
data (`patches.jsonl`, `async-tools/`, `skills/`, `roles.json`, `plugins.json`)
stays in `<repo>/.drip/`. `DRIP_HOME` relocates the home directory;
`DRIP_PROJECT_DIR` / `--project-dir` pins the project root.

**Renaming a session.** Type `/rename` in the TUI composer and the configured model distills the
session transcript into a short 5-7 word name. Or name it yourself with `/rename My session name`:
the literal text is used as-is, with no model call and no word restriction. Either way the name
replaces the window title and is persisted verbatim to the session's `session.json` metadata, so
resume keeps it; the visible pane title trims it to at most 5 words / 48 characters. On any
failure the current name is kept. Both forms work while a goal is running: `/rename` applies
immediately rather than queuing for the next run, and the busy spinner keeps going under the new name.

---

## Compact TUI timeline

In the interactive TUI (`drip --tui`), back-to-back tool activity within a
cycle is folded into a single summary row such as

```
[  3 14:22:41] ── 5 Tools called: READ, PATCH, BASH ──
```

Every numbered row's block carries the local wall-clock time (`HH:MM:SS`)
alongside the cycle number, so the scrollback shows when each op, task,
warn or tool summary settled as the run advances. The clock of a folded
tool row is the first call in that group.

The count updates in place while the tools run, and the row is finalized
once the cycle ends. Each new cycle begins with a short transition line
(numbered cycle, task preview, budget) instead of per-tool chatter, so the
scrollback reads as goals → tool summaries → model responses.

This is a presentation-only projection: compaction is a TUI default and
nothing is deleted from the record. The full transcript — every tool call,
arguments, results and inference telemetry — is still written to the
session JSONL and remains visible in `dripw`, the headless output and the
logs. Replays, session switches and terminal resizes use the same compact
projection, so raw tool rows are not re-revealed.

## Queueing and steering mid-run (TUI)

While a run is in flight the TUI composer stays editable. `Enter` does not
start a second run against the same session: it **queues** the composer text,
listed above the input in send order (never in the timeline). Queued messages
are drained into the next goal once the running run finishes.

`Ctrl+S` **steers** the run that is going right now through the session inbox
— the same handoff `drip --send` uses — which the harness consumes as operator
steering. With text in the composer it steers with that text and leaves the
queue alone; with an empty composer it steers with the **whole queue**, in
order, and flushes it. Whatever fails to reach the inbox stays queued, so a
steer is never lost silently.

The TUI runs the terminal in raw mode with flow control off, so `Ctrl+S`
arrives as a single byte instead of pausing output; shift+enter is deliberately
not a binding because most terminals report it as plain Enter.

## Watch TUI (`dripw`)

`dripw` is a read-only, lazygit-style watcher for drip sessions — run it in a
second terminal while a session works and watch it live. It never modifies
sessions or the session index.

```sh
dripw
```

Panels: `[1]` Sessions, `[2]` Tasks, `[3]` Shells, `[4]` Skills, plus the
transcript. Keys: `1`/`2`/`3`/`4` focus a panel, `Tab` cycles through them,
`j`/`k` move the selection, `[/]` (or `h`/`l`) scroll the transcript, `q` quits.
Clicking a row of the Sessions, Tasks or Shells pane focuses that pane and
selects the row under the pointer (clicking a session also focuses its
transcript); the `[4]` Skills pane is read-only, so a click on it just focuses
it. Hovering the transcript (or the shell log with `[3]` focused) and rolling
the scroll wheel scrolls it too — older lines up, newer down.

The `[4]` Skills pane shows what the focused session's loops actually loaded:
the harness writes the composed skill set on every loop-start telemetry event
(the classifier's picks plus the run's base-prompt `--skill` activations,
deduped in composition order), and dripw reads those events straight out of the
session transcript. The pane lists a roll-up of every skill loaded across the
run with how many loops loaded it, then each loop's own set in order, so you can
see what a run had access to at a given point and how the sets moved as it ran.
Sessions recorded before this telemetry existed just show the empty state.

dripw shows sessions started in the current directory or any directory beneath it.

## Browser UI (`drip --ui`)

`drip --ui` serves a browser UI for every session under the current directory
— the same sessions `drip --tui`, `dripw`, and headless runs use, read from the
same files. Run it from a repo root and open the printed URL:

```sh
drip --ui              # first free port from 4141, e.g. http://127.0.0.1:4141/
drip --ui --port 4200  # pin the port
```

Run it in several projects at once and each takes the next free port. If a
local [Caddy](https://caddyserver.com) is running (its admin API on
`127.0.0.1:2019`, as `brew services start caddy` does), every instance also
registers itself under one shared hub, so the addresses stay stable no matter
which port each one landed on:

```
http://drip.localhost:4140/                 hub: every live UI, with links
http://drip.localhost:4140/<dir>-<hash>/    one project (label printed at start)
```

The hub lives in Caddy only while a UI is running: instances add and remove
their own `@id`-tagged routes through the admin API and never touch your
Caddyfile; the last one out removes the hub server. `DRIP_UI_HUB` changes the
hub address (default `drip.localhost:4140`), `DRIP_CADDY_ADMIN` the admin
endpoint (`off` disables the hub). Without Caddy nothing changes — the direct
URL is printed either way.

The page lists running and recent sessions (running is lease-derived, exactly
like `dripw`), tails the selected transcript grouped by iteration, and shows
the task ledger, token totals, and context pressure (latest prompt tokens
against the active profile's `max_context_tokens`). From the UI you can start
a goal, message a running agent, stop it, or resume an idle session — each is
an ordinary `drip` invocation (`--json --detach`, `--send`, `--stop`,
`--resume`), so anything the UI starts is visible to every other surface.

How it runs: the web app ships inside the `drip` binary and is unpacked to
`~/.drip/ui/<version>/` on first use, where `bun install` runs once; nothing is
written into the project directory. `bun` must be on your PATH
(https://bun.sh). The Bun server is a thin bridge — it reads session files and
invokes the drip binary; there is no HTTP server in the Rust crate. Its
configuration is the environment drip hands it: `DRIP_BIN`, `DRIP_CWD`,
`DRIP_HOME`, `DRIP_UI_VERSION`, `DRIP_UI_PORT` (only with `--port`), and
`DRIP_MAX_CONTEXT_TOKENS`, plus the hub settings above. The page is bundled by
Bun on first request with relative asset paths, which is what lets one
instance serve both its own root and its hub prefix.

The session list comes from `drip --list --json --recursive`, which is also
available on its own: `--recursive` widens `--list` from this project to every
session started in the current directory or any directory beneath it (across
projects, so worktrees count), and each JSON row carries its `cwd`.

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


## Hooks

drip can run your own shell commands at fixed points in a session's
lifecycle — the same idea as Claude Code's hooks and Codex hooks. Hook
commands live in drip's persisted config file, `~/.drip/config.json`
(`DRIP_HOME` relocates the home directory), as a top-level `hooks` object
next to `settings` and `statusLine`:

```json
{
  "hooks": {
    "timeout_seconds": 10,
    "pre_tool_use": [
      { "matcher": "BASH|BASH_ASYNC", "command": "~/.drip/hooks/guard.sh" }
    ],
    "post_tool_use": [
      { "matcher": "PATCH", "command": "~/.drip/hooks/fmt-changed.sh" }
    ],
    "task_start":   ["date '+task start %H:%M:%S' >> ~/.drip/hooks.log"],
    "memory_write": ["~/.drip/hooks/memory-changed.sh"],
    "pr_ready":     ["~/.drip/hooks/announce-pr.sh"],
    "stop":         ["~/.drip/hooks/notify-done.sh"]
  }
}
```

| Key | Fires when | Value |
| --- | --- | --- |
| `session_start` | the run loop starts for a session | array of commands |
| `loop_start` / `loop_finish` | a harness loop begins / ends | array of commands |
| `task_start` / `task_finish` | a task inside a loop begins / ends | array of commands |
| `pre_tool_use` | just before a workspace tool executes | array of `{ "matcher", "command" }` |
| `post_tool_use` | just after a workspace tool executed | array of `{ "matcher", "command" }` |
| `relay_start` / `relay_finish` | a subagent relay round begins / ends (drip-specific) | array of commands |
| `memory_write` | a `remember` or `forget` actually changed the memory bank — failed calls stay silent (drip-specific) | array of commands |
| `pr_ready` | a workspace tool that looks like a publish (`git commit`/`git push` or `gh pr create`) ran successfully — detection is by command text, so it is a publish attempt, not a verified PR; failed or vetoed calls stay silent (drip-specific; see the latency note below) | array of commands |
| `stop` | the run finishes | array of commands |
| `timeout_seconds` | — (not an event) | per-hook deadline in seconds, default `10` |

The `matcher` of the two tool events is a `|`-separated list of tool names
with an optional trailing `*` wildcard (`PATCH|READ`, `BASH*`); an empty or
missing matcher matches every tool. Matching is case-sensitive against
drip's uppercase canonical tool names (`BASH`, `BASH_ASYNC`, `PATCH`,
`READ`, `GREP`, `DIR`, `VERIFY`, `FETCH`, `CHECK`, `REFERENCE`) and never sees the
command text — `"Bash"` or `"rm *"` will never match anything. Lifecycle
hooks always fire.

Every event value is an array: when several commands match an event they
all run, in array order, each with its own JSON payload on stdin. A bare
string instead of an array makes the whole `hooks` block malformed (see
the warning behavior below).

Behavior:

- Each hook command runs under your user shell (`$SHELL -c`) with the
  session's working directory as its cwd and one JSON document piped to
  its stdin:

```json
{
  "event": "PreToolUse",
  "cwd": "/path/to/repo",
  "tool_name": "BASH",
  "tool_input": "{\"command\":\"cargo test\"}",
  "timestamp": "<RFC 3339 timestamp>"
}
```

  `tool_name` and `tool_input` are present for `pre_tool_use`,
  `post_tool_use`, and `memory_write`. `tool_input` is always a JSON
  string (the raw input text), not an embedded object — pipe it through
  `fromjson` if you need structure. It is truncated at 24,000 characters,
  which can leave the embedded JSON unparsable, so parse defensively. One
  known divergence: for `post_tool_use` the string carries the tool's
  output text rather than its input; there is no separate `tool_output`
  field. For `memory_write` it is the redacted note plus the
  `remember`/`forget` tool name. Hook output is capped (first 4 KiB) —
  it is only surfaced in warnings.
- Hooks never block a run: a missing binary, a non-zero exit, or a
  timeout (the process is killed at the deadline) becomes a `RunWarning`
  naming the event and the hook, and the session continues. The one
  exception is Claude Code's veto: a `pre_tool_use` hook that exits `2`
  blocks the tool call — the tool never executes and the hook's stderr
  is returned to the model as the tool result.
- Hooks run synchronously and sequentially at the event point: one shell
  spawn per command, each bounded by `timeout_seconds` (default 10), plus
  bounded cleanup after a kill — a slow hook delays the run by up to its
  timeout. Fire timing follows the event: `memory_write` fires when a
  harness op actually changes the memory bank, and `pr_ready` fires only
  after a publish-matching tool call executes successfully (failed and
  vetoed publishes stay silent; a failing `pr_ready` hook is reported as
  a `RunWarning`). The publish pattern matches command text — a
  heuristic, not a verified PR.
- Payload strings pass through drip's secret redactor before they reach
  a hook's stdin.
- Hooks are commands from your own config, so they run with your user's
  privileges in your session directory — drip does not sandbox them and
  does not require `DRIP_ALLOW_NET` to run them. Only add entries you
  trust, and treat a shared config's `hooks` block like a Makefile you
  did not write. Hooks are therefore not a sandbox or a transitive
  policy boundary: delegated child runs and the internal review child
  currently omit user hooks entirely (so a `pre_tool_use` veto can be
  bypassed by delegating), and lifecycle payloads for `task_*`/`loop_*`
  events do not yet carry task or iteration identity — the event names
  are the only distinguishing signal.
- A malformed `hooks` block prints one nonfatal warning (naming the
  config file) and is ignored; the rest of the config still loads.

`relay_*`, `memory_write`, and `pr_ready` are drip-specific — they exist
because drip has machinery other harnesses don't: subagent relay rounds,
the persistent memory bank, and the /navis publish flow.

This mirrors Claude Code's hooks
([code.claude.com/docs/en/hooks](https://code.claude.com/docs/en/hooks)):
`session_start` → `SessionStart`, `pre_tool_use` → `PreToolUse`,
`post_tool_use` → `PostToolUse`, `stop` → `Stop`; `loop_*` and `task_*`
are drip's own loop/task lifecycle, which Claude Code does not have a
direct equivalent for. Payload field names and the matcher concept follow
Claude Code; Codex' hook surface
([developers.openai.com/codex/hooks](https://developers.openai.com/codex/hooks))
covers a similar lifecycle with different payload conventions.

---
## Config file: nested JSON and automatic migration

Structured settings inside `~/.drip/config.json` are stored as real nested
JSON — `runtime.role_profiles`, `runtime.model_profiles`,
`runtime.system_prompt_profiles`, and `credentials.stored_api_keys` as JSON
arrays, and `runtime.role_bindings` as a JSON object mapping role names to
their loop bindings (`task`, `planning`, and optionally `replanning` — the role
for loops that replan an existing ledger; a replanning loop that leaves nothing
workable escalates the next one to the `planning` role, so a cheap replanner
gets the first try and the expensive planner only runs when it gets nowhere):

```json
{
  "settings": {
    "runtime.active_profile_id": "glm-5-3-flash",
    "runtime.role_profiles": [
      { "id": "planner", "description": "Plans the loop", "model": "glm-5-3-flash" },
      { "id": "author", "model": "glm-5-3-flash", "mcpServers": ["github"] }
    ],
    "runtime.role_bindings": { "planner": "author" },
    "credentials.stored_api_keys": []
  },
  "mcpServers": { "github": { "command": "github-mcp-server", "args": ["stdio"] } },
  "version": 1
}
```

Older versions of drip flattened these values into single-line JSON strings.
Both forms are accepted on load, and drip migrates legacy files automatically:
the first time a config containing valid encoded strings is loaded, those
values are rewritten as nested containers (pretty-printed, atomically) while
everything else — unknown keys, ordinary settings, `version`, and
`statusLine` — is preserved as-is. The migration is one-time and idempotent:
values that are already nested and malformed legacy strings are never
written back, so the file only changes when an actual legacy value is
unflattened. Any string setting that
merely *looks* like JSON (prompts, key references, notes) is always left
untouched. Profile lists are never filled in from drip's compiled-in
catalogs — an absent or empty list stays empty, and missing profile ids must
be added to `~/.drip/config.json` by hand.

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

The `summary` a finish_task carries is asked to be one to three short
sentences (about 300 characters): what changed and what proved it. Across the
recorded runs on the current builds, the author's finish round was 12% of all
inference time and the reviewer's another 7%, with median summaries of 480
and 800 characters; the summary is generated at the very end of a task and is
read only by the next loop and the run report, so its length is pure latency.
The reviewer's instruction asks for one sentence: the verdict and the check
behind it.

The finish can ride on the final edit. A model that knows the PATCH it is
sending is the last one puts `finish` on it — `{"summary": …, "check":
<the acceptance command>}` — and the harness runs the PATCH without the
key, then a synthetic `finish_task` (status completed, that summary and
check) right after it in the same turn: the edit lands, the finish-time
check runs on it, and the "done" round that used to follow (a full prompt
turn and a first-token wait for about a hundred tokens — 37 of 37 finishes
in the 0.154 bench were their own round) is gone. The synthetic call joins
the assistant turn's tool calls, so the transcript stays consistent for
strict providers, and the `PATCH carried a finish` event marks it. A PATCH
that carries a finish and nothing to edit (the model reached for the one
tool with a `finish` field) is the finish itself, under its own id, rather
than a failed empty edit — dogfood #75 sent exactly that and paid a bounce
for it before this rule. The same goes for the other shapes recorded models
reach for when they only want to finish: a `finish` nested in a `files[]`
entry is lifted out, and entries that would change nothing (identical find
and replace, a `__noop__` path) are dropped once a finish is carried.

The PATCH description asks for every edit of the task in one call — all
files, several entries per file in order — because a second call is a
model round: in a recorded bench 24 of 36 extra PATCH rounds edited the
file the previous round had just patched, and the tool text at the time
told the model to use separate calls for one file. The
two-call form (finish_task after the PATCH in one response) works too, but
GLM sent 0 of 19 finishes that way when asked; a field on the call it is
already making is the form it takes. A completed finish that follows a
failed PATCH or command in the same response is bounced (`finish_task:
harness: not accepted — PATCH failed earlier in this same response…`),
since the work it counted on is not in place; blocked and unreconciled
finishes pass.

Two more carries feed the first prompt from the goal's own words. A small
file (60 lines, 2,500 characters) that a goal symbol hits under `git grep
-nw` and that nothing else carried travels whole, at most two of them,
source trees before test trees — on the recorded bench every ttl run spent
a round reading `kvstore/cli.py` (44 lines, hit by `set`) and every
http-serve run one reading `kvstore/store.py` (46 lines, hit by `Store`),
neither of them named. And a goal-named symbol with exactly one definition
in a file nothing carried gets that definition's body, line-numbered and
capped at 60 lines, under `definitions the goal names, already read` — every
big-file run had opened with `READ textutil.py offset 1605` because the
outline located `truncate_middle` in a 2,691-line file it could not carry.
The `definition bodies carried` event records which. Named-file carry now
allows five files under the same 40K-character budget, so the small hit
files ride along with the named ones.

The first prompt also carries the repository's file list — every file grouped
by directory for a project of up to 120 files, else each top-level directory
with its count and immediate subdirectories — and the small test files whose
names pair with the carried sources (`tests/test_cli.py` beside
`kvstore/cli.py`, up to two, 80 lines each). Five of 26 recorded bench runs
had opened with a DIR round (`tests`, `kvstore`, `.`), and the ttl and
big-file runs with a READ of the sibling test to match its style before
extending it; both are now already in hand.

A directory the goal names (`kvstore`, `tests/`) contributes its small
direct files too — up to four across two directories, 60 lines each —
under the same already-read header. Both http-serve runs on 0.161 had
opened with a READ of `kvstore/store.py`, which the goal never names but
which sits in the package it does.

The goal's check runs after the edit, not only at the finish. When a
response's last PATCH lands and the response carries no finish, the harness
runs the goal-declared (or detected) project check once and puts its verdict
on the PATCH result — `-> passed`, with the note that nothing needs to run it
again, or `-> FAILED` with the failure block and the ask to fix it in the
next PATCH with finish on it. The recorded bench paid a bounce round for every
finish whose harness-run check failed (4 of 16 runs) and an author's own
check round in 3 more; with the verdict in hand the next round is the fix or
the finish. The check runs only when its last measured run was under 8s
(unmeasured interpreted runners qualify; `cargo test` and other compile-first
runners do not until measured), never for a command that hung this run, and
at most three times per loop.

A check the goal names in plain prose counts too. `Verify with cargo test
--release --lib tools::builtin::patch.` reads to the end of the clause the
same way a backticked command does, so the warm-up builds that profile, the
finish runs that command, and the planner is skipped for the single-task
goal. 272 of 2,522 recorded goals named their runner this way; a dogfood
that did had paid a `cargo test -q` over the whole crate in the debug
profile, after a warm-up that had built the wrong one.

A completion report on a verified workspace is the finish. When the goal's
check passed after the last edit and nothing changed since, a text-only
reply that reads as done ("the count subcommand is added and the tests
pass") is accepted as `finish_task` with that text as its summary, in the
same round. It used to conclude the loop with the task unfinished, and the
next loop re-seeded the task with a fresh first prompt — a recorded
count-cmd run paid the whole orientation carry twice for one sentence.
An unverified text reply still concludes the loop as before.

PATCH has an `append` mode. `{"path": "tests/test_store.py", "append": "..."}` A real `find`+`replace` sent in the same entry as an `append` is two edits and both run — the pair first, then the append — unless the `replace` already contains the append text, in which case the append is the same insertion sent twice and is dropped (a recorded pwrde run duplicated a test and left a stray brace, three edits to recover).

A find text that misses because its JSON escapes arrived as literal text (`\u2026` for `…`, `\n` for a newline) is decoded and matched, with a note on the summary. When a find text is not found, the error says which line of it the file does not hold and names the closest line in the file (by shared prefix and suffix, at least 60% of the line), or that every line is present but not as one block, so the next call can copy the actual text instead of guessing again. A later entry whose find matches the file on disk but not the text the earlier entries in the same call leave is told that entries apply in order. A call repeated verbatim after failing is told that re-sending cannot succeed. An append to a Python file whose last top-level statement is the `if __name__ == "__main__":` guard goes before the guard (one blank line for an indented continuation such as a test method, two for a new definition): appended after it, a method sits inside the guard, is never collected, and the goal's check passes without running it.

A completion report sent in the same response as the edits is accepted as the finish when the round was PATCH calls only, every call landed, the goal's check passed right after the last of them, every path the goal names has been edited this run, and the text reads as done (no closing question, none of the in-progress phrasings). The finish joins that response's tool calls in the transcript, so a task that ends with its edits does not spend a further round on a lone `finish_task`. When the shape appears but a gate holds, a harness event says which (no text with the calls, text that does not read as done, or a goal-named path not yet edited).

An indented append to a brace-language file (a two-space `test(...)` for a bun suite, a four-space `#[test]` for a Rust module) goes before the file's trailing run of bare closers (`});`, `}`) so it lands inside the outermost block rather than after it; a top-level append still goes at the end. A task whose title names a workspace path and carries a build verb anywhere ("In src/rect.rs add a test module") counts as a code change for the no-edit finish gate, not only a title that opens with the verb. A finish whose last check ran green with zero tests executed before the edit landed is rechecked by the harness like a stale one, instead of bouncing for the model to re-run it.

A check the goal names in plain prose may be a chain: "Verify with bun run typecheck && bun test hub/test/x.test.ts" is one command, run as written, so the type check is enforced alongside the tests (a recorded goal had only its bun test half run). A command the model runs through BASH that contains the goal-declared check carries the external anchor even with no anchor field, and when the harness's own run of the goal-declared check reuses the current record, that record's anchor is promoted to external, so the finish it serves is not bounced as self-authored or undeclared. A runner that opens a quoted string in the goal (`Some("cargo test --no-run")` in a goal that describes a test) is a code sample, not a command, and a quote inside an argument ends the command: a recorded run had warmed up on `cargo test --lib"` from such a sample instead of the declared `cargo test --release --lib`, compiling the wrong profile. Type checks, builds and lints count as runners in prose too: bun/npm/pnpm/yarn typecheck, bunx or npx tsc, tsc --noEmit, bun/npm run lint or check, cargo check, cargo clippy, cargo build, go build, go vet, ruff check, mypy.

When a goal declares two or more distinct checks ("`cargo test --lib harness` and `cargo test --test loop_smoke` must pass"), the harness runs all of them as one `A && B` chain for the edit check and for an unchecked or stale finish, and that chain counts as the goal-declared check (external anchor, review waiver) just as each part does. About a fifth of recorded goals named more than one command; only the first ever ran as the harness check, so the finish bounced for the second and cost a VERIFY round. A chain the goal spells out itself subsumes its parts, and the compile warm-up still starts from the first cargo test in the goal.

A stale finish (edits since the last check) re-runs the last check only when that check is the goal-declared one or the goal declares none; otherwise the harness runs the declared chain, which carries the goal's acceptance and waives the review. A recorded run had re-run the single test the model picked, accepted the finish, and then spent a review loop running the two declared suites. The outline's goal-symbol scan now accepts names up to 120 characters (descriptive test names in 247 of 2,643 recorded goals ran past the old 60), so a definition named as a placement anchor is carried into the first prompt instead of costing a READ.

An append can name its place: `after` (or `before`) with a definition name puts the text right after that definition's block (or above it and its attributes), indented to match, and the summary says where it landed ("after `alpha` (the new text starts at line 11)"). The anchor may be a bare name or the call the model reads in the file — `describe("readRegistry")`, `test('x')` — which matches that line by its literal opening; a recorded run passed the whole call expression and the strict lookup missed it, dropping the append at the end of the file. A multi-line anchor — the whole function or block the model pasted from the file — is reduced to its first line (the signature) and resolved from that. When the goal itself places the new code ("right after `old_case`", "below the test parse_flags") and the model sends a plain append, the harness fills the anchor in from the goal and says so; an anchor that is missing or ambiguous falls back to the end of the file with the reason in the summary. Recorded appends under such goals had landed at the end of the file every time, and the model then spent READ + PATCH rounds moving the text, or left it there.

A READ whose line range overlaps a range already read this run — a shifted or widened window, which the identical-call check never caught — gets a note: the lines are still verbatim in the conversation and the file has not been edited since, so re-reading spends a round for nothing. 359 of 1,525 recorded READs re-read an overlapping window this way. The tracked ranges for a file are cleared when a PATCH edits it, so a genuine re-read after a change is never flagged.

When the harness spawns the deferred review and a check the harness trusts already passed after the last edit (an external or goal-declared anchor, at least one test executed, none failed, no edit since), the review task carries a note naming that command and asking the reviewer to confirm the change by reading the diff rather than re-running it. In a recent window 50 of 68 review loops re-ran a test command and none of the 68 changed a line, so the re-run was pure cost. The review task's own instruction drops "independently verify with your own tools" for a diff review in the same case, so the title does not pull the reviewer back into the re-run the note tells it to skip. The reviewer still judges the diff and may re-run on a specific doubt; a blind reviewer never gets either.

The check that runs after a PATCH round skips compile-first runners (cargo test, go test, dotnet test, mvn, gradle, mix) until their cost is measured, but once the run's background warm-up build for that runner has finished the compile is already paid, so the check runs after each edit round instead of waiting for the finish. A recorded pwrde run had its warm-up done within seconds and still spent three PATCH rounds and a READ repairing a bracket slip blind. A check's duration is recorded only when no warm-up for the same runner is still compiling: measured under the build lock, a 5s check reads as 34s and would disqualify every later edit check.
adds text at the end of a file (a newline first when the file lacks one;
the file is created if missing), alone or chained after find + replace
entries for the same file in one call. New tests and functions usually go
at the end, and the whole-file `content` form was re-sending the rest: 39
recorded overwrites of existing files re-emitted 59% of their lines
unchanged, and one run re-sent a 44-line module four times. An overwrite
that re-sends half or more of a file (20+ lines) now says so in its
result and names the cheaper forms.

A check that hangs names itself. The timeout sends SIGABRT before SIGTERM
and SIGKILL, and every tool command runs with `PYTHONFAULTHANDLER=1` unless
the parent environment sets it, so a Python test that never returns dumps
every thread's traceback to stderr as it dies. The HUNG result then carries
the last test that started and never finished, each thread's innermost
frames with user files named, and — when the hung thread is joining under
unittest's cleanups while another thread sits in `serve_forever` — the
explanation that cleanups run LIFO, so `addCleanup(thread.join)` registered
before `addCleanup(server.shutdown)` joins a server that was never told to
stop. A recorded http-serve run had exactly that deadlock and spent 1,000s
and 44 shell probes finding it; the fault handler's dump had shown it at
the first failure, but only inside a probe the model ran itself, 300s in.
The forensics lead the result and the raw dump is trimmed to three frames
per thread (the C stack dropped), so the middle truncation of a long result
keeps them; the first probe lost them to it.

## Skills picker (TUI)

`/skills` in the TUI opens an interactive picker in the live region instead of
printing the list into the transcript — nothing it shows stays in the history
once it closes:

- Each row shows the skill's on/off mark, its name, where it came from
  (`project`, `user`, `builtin`, or the `<marketplace>/<plugin>/<skill>` key)
  and a rough size (`~N tok`, the skill file's byte length divided by four).
  The highlighted row adds its description.
- Type to filter by name or description, `Backspace` to widen the search
  again, and `Up`/`Down` (or `Tab`) to move the cursor. The list shows eight
  rows at a time and the window slides to keep the cursor visible.
- `Enter` or `Space` toggles the highlighted skill for the session and keeps
  the picker open, so several skills can be turned on or off in one visit.
  `Esc` closes it.
- A marketplace skill the registry currently gates is shown dimmed with a `×`
  and `locked by plugin`: enable it with `/marketplace` instead of toggling it
  here.

## Prompt history (TUI)

The TUI input line keeps a bounded in-memory history of prompts you have
accepted, so recent prompts can be reused without retyping:

- Press `Up` to recall the previous prompt and `Up` again to walk to older
  ones; navigation clamps at the oldest entry. Press `Down` to move back
  toward newer prompts; stepping past the newest restores the draft you had
  typed before navigating, exactly as it was.
- The first `Up` starts navigation only when the cursor is on the first
  line of a multiline prompt (and the first `Down` only from the last
  line); everywhere else the arrows keep their normal multiline cursor
  motion. Once navigation has started, `Up` and `Down` walk the history
  regardless of cursor position. Slash-command, skill, and mention
  suggestion menus keep priority over history navigation, and arrows are
  inactive while a goal is running.
- Each accepted prompt is recorded once. Blank submissions, slash commands,
  and skill activations are never recorded, and consecutive duplicates are
  collapsed. Recalled prompts are fully editable and resend with the normal
  `Enter` path.
- History is in-memory only (the last ~64 prompts for the drip process) and
  is never persisted to disk. Switching sessions keeps the entries but drops
  any in-progress navigation and the saved draft, so a draft never leaks
  into another session.

## Contributing

PRs welcome. Please run `cargo build --release && cargo test` before submitting.
The Codex protocol tests also require Python 3 for their local mock server.

## Recovery memory and retry semantics

When a task is finished blocked or dropped, drip records a bounded recovery
event on the task (blocked, reopened, dropped, retries exhausted, operator
reply). History is capped at 8 events per task with the oldest evicted, and
every text field is clamped to 200 characters on Unicode char boundaries, so
state growth stays bounded and older state files without recovery history
load unchanged.

`BASH_ASYNC` waits up to `waitMs` (default 15s) for its command and returns
the output inline when it finishes in time; only genuinely long commands stay
in the background, and the result then says to use `ASYNC_WAIT` (blocks) or
`ASYNC_TAIL` (peeks) rather than sleep-and-poll probes. A role whose tool
list includes `BASH_ASYNC` always gets `ASYNC_WAIT` and `ASYNC_TAIL` too —
recorded sessions without them spent whole loops on "sleep 12; cat log"
rounds. The read-only nudge ("N reads and nothing written") waits for the
second cycle of a loop (or sixteen reads): eight reads in a fresh loop's first
cycle is orientation, not drift.

A Cargo workspace gets a build warm-up in the background at run start (its
output goes nowhere; the job is killed with the run), so the author's first
`cargo test` finds the compile done instead of paying it after 20-40s of
orientation during which the CPU sat idle. The warm-up compiles what the
goal's check will run: a goal-declared `cargo test …` keeps its profile,
targets and packages and gets `--no-run` (`cargo test --release --lib
harness::model_call` warms up as `cargo test --release --lib
harness::model_call --no-run --quiet`); with no such check it is `cargo
build --tests --quiet`. Six recorded dogfoods declared a `--release` check
while the warm-up built the dev profile, so every check still paid a 27s
release compile. `DRIP_NO_WARMUP=1` disables it.

The warm-up cannot remove the recompile an edit forces: a release
recompile of this crate after one edit takes 17–18s (the release profile
turns incremental compilation off, and `sccache` refuses
`CARGO_INCREMENTAL=1` outright), so a check after the last edit pays that
once. What the warm-up removes is everything else the check would compile
cold: the dev-dependencies and test targets of the profile it names.

When the goal-declared check already passed after the author's last edit
(the same anchor the review waiver uses, without its size bound), the review
brief says so under `verification settled:` and tells the reviewer not to
re-run it — a reviewer re-running `cargo test` cost a round and the suite's
minutes again on every recorded Rust dogfood.

A `BASH` or `VERIFY` command that hits its timeout comes back marked as hung
(`HUNG:` / `TIMED OUT`) with what to change, and the harness refuses to run
the identical command text again until an edit lands: a test that starts a
server and never returns hangs the same way every time, and a recorded run
spent three two-minute timeouts on one unchanged `unittest discover`.
A VERIFY that merely echoes a `DRIP_VERIFY` marker, with no real check
`&&`-chained before it, is rejected as fabricated rather than counted.
The same command shape (ignoring `timeout N` wrappers, `2>&1`, trailing
`| tail`/`| head` filters, `; echo` suffixes and -v/-q flags) run three times
in a row with no edit in between gets a flailing nudge on its result: the
recorded run above went on to re-run the hanging suite under `timeout 60`
five more times, one minute each, changing only its flags.

Each loop prompt also carries a `file_outline` section for the files the task
(title, notes, goal) names. A named file of at most 400 lines travels whole:
the section carries its line-numbered text in the form a READ returns (up to
three files and 40,000 characters, first-mentioned first), so the first round
starts from PATCH instead of the READ that 350 of 712 recorded runs spent it
on. Longer named files get line-numbered top-level definitions instead, up to
four files each: sixty full signatures, then up to
three hundred more as `name@line`, generated by the
harness with no inference. A worker on a 6000-line file reads the range it
needs instead of paging through it. The same section lists `git grep -nw`
hits for the identifiers the task names (snake_case and CamelCase tokens, up
to eight names and six hits each), so the first rounds start from the call
sites and definitions instead of discovering them one GREP per round.
A plain word the goal quotes in backticks (`title`, in "the predicate whose
name contains `title`") is not an identifier, so instead of a whole-word grep
the section lists the definitions whose name contains it (`fn
should_request_title`, `struct TitleRoute`, `def make_title`), grouped by
file with source trees before test, fixture, eval and vendor trees (the
first four files with up to six definitions each, the rest by count), for up
to four words; the recorded runs of such a goal spent their first round on
exactly that search.

Tool arguments are repaired before a call is refused. Some models leak
their native tool-call syntax into the JSON they were asked for
(`"head</arg_value><arg_key>pattern": "fn x"`), and the call then fails on a
missing required argument and costs a round: the segment after the last
marker is used as the key (unless the intact key is also present), stray
`<arg_key>` tags are stripped, and a `timeout` sent instead of `timeoutMs`
is honoured (values under 1000 read as seconds). Thirty-four calls in this
machine's transcripts since 10 September carried the marker, and several
hundred sent `timeout`.

A GREP hit names the definition it sits in (`  [in run_goal]`), and a GREP
whose whole search matches at most three lines returns the body of every hit
that is a definition line, numbered like a READ (up to 80 lines each, 6000
characters in all, with a pointer to the READ that shows the rest of a longer
body). Nine hundred of the twelve thousand GREPs in this machine's transcripts
were followed by a READ starting at the line the GREP had just found — one
extra model round each; the body arrives in the GREP's round instead.

Inside a git checkout, GREP searches the files git knows about (tracked plus
untracked-but-not-ignored, via `git ls-files -co --exclude-standard`) instead
of walking the tree, so `target/`, `node_modules/`, a virtualenv or a stray
build directory never get read. On this repo's checkout that is 265 files
instead of 444,000: a repo-wide GREP with no glob had been taking eight
seconds per call, and the model in one recorded run made twelve of them.
Outside a work tree the walker still scans the directory.

A base-model call whose profile sets no reasoning effort is sent at `low`.
A profile with no effort leaves the provider's default thinking on, and on
GLM that was where the run's time went: in this machine's sessions of 10–12
September, 12% of the calls emitted 4,000 or more completion tokens — almost
all hidden reasoning ahead of one small tool call — and those calls took 51%
of all inference time, while the same model at `low` shows none. A profile
or role that sets an effort keeps it; a provider that answers the field with
a 400 gets one retry without it and the rest of the run omits it. Inference
events now also report the provider's `reasoning_tokens` when it exposes
them, so a long reply can be told from a long thought.

Model calls are bounded per attempt (240s by default). Once a model has three
completed calls behind it, the first attempt of each call is bounded by eight
times that model's recent median latency instead (never below 45s, never above
the base bound); a call that blows that bound is retried at once and the retry
gets the full bound, so a stalled upstream costs about a minute rather than
four while a legitimately long completion still lands.

The latency tail is hedged as well. Across two twelve-run benches, calls over
15s were 55% of all inference time while the median call took 2s, and those
slow calls produced 3-14 tokens/s against the usual 59: queueing, not
generation. So once a model has three completed calls, a first attempt that
runs past twice its median latency (never under 8s) is raced against a
second identical request; the first answer wins and the other is dropped.
The `hedged model request` event marks each race; the duplicate is billed but
only the winner's usage is recorded. One-shot helper calls (session names,
terminal titles, bash distillation) do not hedge.

OpenAI-compatible providers are asked to stream (`stream: true` with usage in
the final chunk), and the chunks are folded back into one response body
before parsing, so nothing after the transport changes. Streaming lets the
hedge watch the reply instead of the clock: a first attempt that has sent no
first token by twice the model's median first-token time this run (never
under 4s, never past the wall-clock point) is raced, as is one whose stream
goes quiet for 15s mid-reply, while a reply that is streaming normally is
never raced however long it takes. The point rises with the median so a
provider that is uniformly queued (first tokens at 5–10s) is not raced on
every call. The
inference event records the first-token time (`in 6200ms (first token
900ms)`). A provider that answers a streaming request with a 400 naming
streaming gets non-streaming requests for the rest of the run (the
`rejected the streaming request` warning marks the switch); the Anthropic
native transport does not stream.

Both bounds start warm: each model's last eight latencies are written to
`~/.drip/latency.json` (under `DRIP_HOME` when set) and seeded into the next
run, so the stall bound and the hedge point apply from the first call rather
than after three warm-up calls. The `latency memory: seeded` event marks a
run that started from the store.

A background job the model started (`BASH_ASYNC`, or any async tool) that
settles between rounds is reported by the harness before the next model call:
a `harness: background job … finished: completed (exit 0)` message carrying
the last forty output lines, plus a `harness-op` event. A job whose result the
model already saw (a completed `ASYNC_WAIT`, an `ASYNC_TAIL` after it settled,
the `BASH_ASYNC` grace wait) is not reported again. Recorded sessions spent
whole loops on "sleep 12; cat log" probes for jobs that had long finished.

`drip --review` runs its per-file reviewers at low reasoning effort unless the
file profile (`--file-profile`) sets an effort of its own: the reviewers fill a
fixed report format from a diff they were handed, and on a profile without an
effort setting they produced a median 1,162 completion tokens per call (3,782
on the slow ones) against about 105 for the same model at low effort, taking
75s per file. The review banner names the effort in use.

A cycle allows sixteen tool rounds (was eight). A cycle boundary folds the
transcript's cold tool results and adds a continuation message, so every
boundary costs the model a page of re-orientation READs; with cold results
folded as the transcript grows anyway, longer cycles are the cheaper way to
keep context bounded. The multi-cycle bench tasks ran 11-29% faster at
sixteen rounds in a three-repeat A/B.

Auto plan mode now skips the planner for goals up to 2,500 characters naming
up to ten paths (was 700 and three) when the goal declares a backticked
check. A three-repeat A/B on the two largest bench tasks: backend-refactor
53s → 21s and http-serve 96s → 33s at the same pass rate, with inferences
15 → 9 and 26 → 12; the planner's 15-18s call plus its task decomposition
(each task its own loops and review) cost two to three times the wall.

Two observations from the speed benches that are configuration, not code.
The planner role on `gpt-6-astra` costs 15-18s per call; the same bench with
the planner on `glm-5-3-flash` (low effort) planned in about 5s at the same
pass rate, so a fast profile for the planner suits small and medium goals
(keep the stronger model for goals that need real design). And across 31
hedged requests in seven benches the second request won 18 times (resolved in
9-18s where the first would have taken longer) and lost 13, mostly within two
seconds of the 8s floor; the floor is right where it is.

The review brief a reviewer loop opens with carries new files in full (up to
four files of at most 400 lines and 16,000 characters) next to the diff, and
the tracked files the author edited as their full current text, line-numbered
exactly as a READ returns them (up to three files, 400 lines each, 40,000
characters in total), and tells the reviewer not to READ files it was already
handed. Before the edited-file carry, 74 of 81 recorded reviewer loops opened
with READs of the files whose hunks the diff had just shown them.

A BASH or VERIFY command that hits its timeout is remembered two ways. The
exact text is refused on an identical re-run (`harness: not run — this exact
… command already hung`) until an edit lands. Its *shape* (the command with a
leading `timeout N`, verbosity flags and tail filters stripped) is kept for
the rest of the run, and a re-run of that shape under another spelling runs
under a thirty-second leash instead of the default two minutes, unless the
call sets its own timeout; the result says so. Recorded runs re-ran one
hanging `unittest discover` eight times as `timeout 60 …`, `timeout 90 …`,
`-v` and `| tail` variants, each for its full timeout.

A finish_task call may name its own check (`check: "python3 -m unittest -q"`):
when the task edited the workspace and nothing has passed since the last
edit, the harness runs that command before judging the finish, exactly as it
runs a goal-declared check, and completes the task in the same turn when it
passes (a failure comes back with the output). 107 of 115 recorded runs ended
with a passing VERIFY round followed by a finish round for the same command;
naming it in the finish folds those into one model turn. A named check that
matches a goal-declared command counts as goal-declared (review waiver
included); any other is external evidence labelled as run by the harness, and
the usual anchor downgrade applies when it names a file the run edited — a
named native-runner suite (`cargo test`, `pytest`, …) that passes earns the
same small-change review waiver as the agent's own VERIFY of it would. When
a finish-time check fails, the next finish after an edit re-runs it (a
re-run of a goal-declared command keeps its goal-declared standing), so a
fix-and-finish never needs a manual VERIFY round in between. The harness
runs at most three finish-time checks per loop.

A single-task run whose goal-declared check the harness ran after the last
edit, and which passed, skips the reviewer loop when the change outside test
files (tracked diff plus new files) is at most a hundred lines, and the whole
change including tests at most five hundred: the finish reads `Review waived:
…` and a `review waived` event records the check and both line counts. Test
lines do not count against the bound because the check the waiver rests on
just ran them; on the recorded bench every reviewer loop fired on 110–161
total lines of which 38–75 were code, confirmed 7 of 7 with no finding, and
in 5 of 7 re-ran the check the harness had already passed. Any task
still awaiting review, or any remaining author work, keeps the review gate;
so does a goal without a backticked check, since then the harness never ran
one. The agent's own VERIFY of a goal-declared check counts the same way
(passed, nothing edited since), and a VERIFY the agent labelled "self" whose
command is one of the goal's declared checks is upgraded to external
evidence: the operator declared it, the agent only ran it. Before that
upgrade the harness bounced such a finish and re-ran the very same command.
When the goal declares no check at all, an external-anchored VERIFY of the
project's own suite through a native runner (`cargo test`, `pytest`,
`unittest`, `go test`, `vitest`, `bun test`, `npm test`), passed with nothing
edited since, settles the change the same way — for the waiver's size-bounded
skip and for the reviewer's `verification settled` note — since most real
goals declare no check and their reviews re-ran exactly that suite. The same
suite run through BASH instead of VERIFY is recorded as a verification too
(the result text says `recorded as verification record v<n>`), so a finish
after `cargo test` via BASH is not bounced into re-running it as VERIFY.
And a VERIFY that repeats the current record's command (same shape, nothing
edited since, the record passed with executed tests) does not run again: the
result says `VERIFY not re-run` and names the record to cite (`mix test`,
`dotnet test`, `mvn test` and `gradle test` count as native runners too).
A BASH runner command piped into a trailing `| tail -N` / `| head -N` loses
the filter the way VERIFY does (the result says so): the filter hid the panic
block behind "FAILED. 0 passed; 1 failed" and cost the next round a
`| grep -A6 panicked`. When a failing runner's output is long enough to be
cut in the middle, the failure block (from the first panic / assertion / FAIL
line) is appended as `failure excerpt from the elided middle`.
A role route whose codex executable cannot be spawned (missing binary) no
longer ends the run: the call falls through to the run's base model once,
with a run warning naming the route (a recorded run died at its replanning
loop on "codex executable not found").
A `DELEGATE` child has a wall-clock budget as well as an iteration cap
(`wallSeconds`, default 1200, max 3600): two recorded parents that looked
like 75-79 calls each hid a child that ran to max-iterations for 2.5-3.3
hours behind one DELEGATE call. At the deadline the child is stopped and the
result reads `DELEGATE wall budget of Ns exhausted (…)` with what it
finished, so the parent can resume it narrower or do the rest directly.
The deadline also terminates whatever process the child had in flight (the
first dogfood of the budget saw a full `cargo test` run on for 26s past it).
The fourth READ window of one file in a loop returns the whole file when it
is 1500 lines and 40K chars or fewer (the result says so), and that one
result is allowed past the per-result cap — under the default 8000-char cap
the first dogfood of this lever handed the model the head and tail of a 32KB
file and it re-read the whole file twice more. A recorded run paged through
an 845-line file in 66 windows, one round each, when a handful of full reads
would have carried the same text.
A native-runner filter that is a source module's stem (`cargo test --lib
child_process` against an edited `src/tools/child_process.rs`) no longer
downgrades the run to self-authored — the stem rule applies to test-like
paths only (`tests/`, `test_*`, `*_test`, `*.test`/`*.spec`), since a module
filter runs that module's mostly pre-existing tests; a recorded run had its
36-test filtered run downgraded on that match and then paid a full-suite
re-run at finish. A `cargo test` given two positional filters fails before
any test runs; the result now says to chain separate commands.
A BASH command over 1200 chars gets a note with its generation cost: a
recorded "prepare the PR" run spent 739s of its 1202s of inference on 24
calls whose 1200-4000-token shell scripts each waited 20-45s to be written
before they ran (the system prompt now asks for one command or a short
pipeline per call, with independent checks as separate calls in one round).

When a finish arrives with no check behind it and the goal declares none,
the harness detects the project's own suite from the workspace layout
(`Cargo.toml` → `cargo test -q`, `go.mod` → `go test ./...`, a `package.json`
test script → `npm`/`bun`/`pnpm`/`yarn test`, a pytest configuration →
`python3 -m pytest -q`, a `tests/` directory of Python files → `unittest
discover`) and runs it once as the task-provided check — event `project
check detected` — instead of bouncing the finish for the agent to guess.

A finish bounced twice in a row for the same anomaly-family reason (a
support gap, an expectation mismatch, an unobserved expectation) is
re-applied by the harness as `unreconciled` with the anomalies on record —
the bounce text already asks for exactly that, and recorded runs instead
re-sent `completed` until the iteration cap. The event `finish
auto-downgraded to unreconciled` marks it; evidence bounces (run a check)
are never downgraded this way.

A run ends `unreconciled` only for a blocking anomaly: an unresolved support
gap, or one whose expectation's latest observation mismatched, or whose own
observed text reports a failure. Anomalies that call themselves informational,
whose observation matched, or whose observed text reports success (exit 0,
0 failures) become task notes and the run completes. "Process" expectations
(an exit status, a merge outcome, a check result) are observed by any passing
verification record, without anchor bookkeeping.

`--plan-mode auto|always|direct` decides how a run gets its first task list.
`auto` (default) skips the planner for a small goal (≤2500 chars, ≤10 named paths) that the harness can still
verify — it declares its own backticked acceptance check, or the workspace has
a detectable project suite (`Cargo.toml`, `go.mod`, a `package.json` test
script, pytest config, or a `tests/` of `.py` files; the seeded event names
it): one direct task is seeded from the goal text and the author
starts at once — the planner cost 13-20s on every speed-bench run, half the
wall time of a small task, while the goal already said what to do and how to
check it, and with `auto` the bench's small and medium tasks ran 25-60% faster at
the same hidden-test pass rate. `always` runs the planner role first for every
goal; `direct` always seeds the direct task. The reviewer still verifies.

A task loop's cycle budget stretches with progress: a cycle that edited or
verified the workspace earns the loop one more cycle (at most two per loop,
`MAX_CYCLE_EXTENSIONS`), so productive work is not cut off by a transcript
reset; loops that only read never extend. Review loops get no read-only
nudge — reading is their job.

Every task also carries a **task loop budget** (`--task-loop-limit <n>`, default 6
task loops): the prompt counts loops from the third one, warns on the last,
and if that loop ends without `finish_task` the harness blocks the task
itself ("Auto-blocked: N task loops (budget 6) without finish_task"). Stall
accounting only sees loops with no progress; the budget bounds tasks that
keep editing but never finish — the single biggest source of 10-17-loop tasks
in recorded sessions.

The loop uses this history to keep retries bounded and honest:

- An unchanged failing task gets at most **one automatic reopen per recovery
  episode**; after that it stays blocked so the replanner sees the failure
  evidence instead of silently retrying forever.
- Exhausted episodes are cleared only by an **independently observed change**
  — a real completion or an operator reply — not by note churn, re-blocking,
  retitling, or drop/re-add of the same work.
- Re-adding a task whose title matches an existing task (including a dropped
  one) is refused: identical replacement work cannot mint a fresh id and
  reset retry accounting.
- Tasks blocked on operator input are never reopened automatically; the run
  ends awaiting your reply, and resuming with a reply starts a fresh episode.
- Dropped exhausted tasks keep an honest dropped outcome (never a fake
  completion), and the global futility exit still terminates runs without
  useful work.

Replanning prompts surface these recent outcomes per task (including for
dropped tasks) so the planner resolves existing task ids with changed
evidence instead of re-deriving identical work.

---

## License

MIT

### Build-evidence detection sees through `cd` and redirections (#118)

A clean `cargo check` or `cargo build` is real build evidence, but the model
naturally runs it as `cd crate && cargo check` or `cargo build 2>&1`. Both forms
used to fall to an UNVERIFIED "unknown runner" — the `&&` and the `2>&1` each
tripped the `&` guard that keeps `echo cargo build` from being misread — so the
model had to re-run the check a second, plainer way. Build-evidence detection
now strips a leading `cd DIR &&` / `pushd DIR &&` (and one paren layer) and
ignores redirection operators, while still rejecting any real second command in
the chain. `tsc` clean-pass detection inherits the same normalization.

### `npm/bun run typecheck` counts as the tsc it wraps (#119)

A bare `tsc` / `bunx tsc` already counted as typecheck evidence, but the same
check run through a package script — `bun run typecheck`, `npm --prefix web run
check-types` — fell to an UNVERIFIED "unknown runner", so a standalone
type-check pass earned no credit and the model re-ran it another way. Build-
evidence detection now recognizes a `npm/bun/pnpm/yarn run <script>` indirection
whose script is a conventional type-check (typecheck, check-types, tsc, …) and
credits it identically. `build` and `test` scripts are deliberately excluded: a
build script wraps a bundler rather than a recognized compiler, and test
assertions are read from the runner's output.

### Keep the freshest read of each file visible across cycles (#120)

Cold tool results fold to one-line digests past a hot window so a loop's
transcript stays bounded. But in the hundreds-of-cycles regime that folds a
file's contents out of view, and the model re-reads it — a transcript audit
found one 273-round session that re-read a single unchanged file 274 times, 153
of those in consecutive rounds with no edit between them; across the 80 highest-
round sessions, 83% of all reads were the same file read five or more times.
Folding now keeps the freshest READ of up to five distinct files verbatim past
the hot window, so current file state stays visible and the re-read cycle never
starts. A read the file has since been PATCHed past is not pinned (it would show
stale content), and the overflow-recovery path still folds everything.

### Don't pin a read a shell command edited (#121)

The read-pinning from #120 drops a read the file was PATCHed past, but a file
edited through the shell — `sed -i`, `> file`, `tee` — leaves no PATCH diff, so
its pinned read could show pre-edit content. Pinning now also inspects the
mutating BASH commands in the transcript (classified with the existing read-only
detector) and refuses to pin any read a later shell command names. Sixty percent
of the high-round sessions in the audit ran at least one such shell edit, so
this closes the one stale-content gap #120 left open; reads of files the command
did not touch stay pinned.
