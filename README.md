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
  check the agent wrote is consistency, not correctness. Finishing a task that
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

### Clarification questions (`--ask`)

`--ask` opts a run into the `ask_user` harness tool: when the goal is ambiguous or an
approach tradeoff needs the operator's call, the model asks a staged survey of
multiple-choice questions (each with suggested options plus a free-text "other"),
then revises its plan around the answers before implementing. Off by default — most
goals should one-shot; enable it when accuracy matters more than autonomy. The opt-in
is pinned on the session, so `--resume` keeps it.

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
failure the current name is kept.

---

## Compact TUI timeline

In the interactive TUI (`drip --tui`), back-to-back tool activity within a
cycle is folded into a single summary row such as

```
[  3] ── 5 Tools called: READ, PATCH, BASH ──
```

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

## Watch TUI (`dripw`)

`dripw` is a read-only, lazygit-style watcher for drip sessions — run it in a
second terminal while a session works and watch it live. It never modifies
sessions or the session index.

```sh
dripw
```

Panels: `[1]` Running, `[2]` Recent, `[3]` Shells, plus the
transcript. Keys: `1`/`2`/`3` focus a panel, `Tab` cycles through them,
`j`/`k` move the selection, `[/]` (or `h`/`l`) scroll the transcript, `q` quits.

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

Each loop prompt also carries a `file_outline` section for the files the task
(title, notes, goal) names: line-numbered top-level definitions of any such
file over 200 lines, up to four files and sixty entries each, generated by the
harness with no inference. A worker on a 6000-line file reads the range it
needs instead of paging through it. The same section lists `git grep -nw`
hits for the identifiers the task names (snake_case and CamelCase tokens, up
to eight names and six hits each), so the first rounds start from the call
sites and definitions instead of discovering them one GREP per round.

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

Auto plan mode now skips the planner for goals up to 1,400 characters naming
up to six paths (was 700 and three) when the goal declares a backticked
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

The review brief a reviewer loop opens with carries small new files in full
(up to four files of at most 200 lines) next to the diff, and tells the
reviewer not to READ files it was already handed; bench reviewers spent two of
their four rounds re-reading changed files before verifying.

A single-task run whose goal-declared check the harness ran after the last
edit, and which passed, skips the reviewer loop when the whole change (tracked
diff plus new files) is at most a hundred lines: the finish reads `Review waived:
…` and a `review waived` event records the check and the line count. Any task
still awaiting review, or any remaining author work, keeps the review gate;
so does a goal without a backticked check, since then the harness never ran
one. The agent's own VERIFY of a goal-declared check counts the same way
(passed, nothing edited since), and a VERIFY the agent labelled "self" whose
command is one of the goal's declared checks is upgraded to external
evidence: the operator declared it, the agent only ran it. Before that
upgrade the harness bounced such a finish and re-ran the very same command.

A run ends `unreconciled` only for a blocking anomaly: an unresolved support
gap, or one whose expectation's latest observation mismatched, or whose own
observed text reports a failure. Anomalies that call themselves informational,
whose observation matched, or whose observed text reports success (exit 0,
0 failures) become task notes and the run completes. "Process" expectations
(an exit status, a merge outcome, a check result) are observed by any passing
verification record, without anchor bookkeeping.

`--plan-mode auto|always|direct` decides how a run gets its first task list.
`auto` (default) skips the planner for a small goal (≤700 chars, ≤3 named paths) that declares its own backticked
acceptance check: one direct task is seeded from the goal text and the author
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
