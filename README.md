# drip — Local Code Inference, in Rust

A headless-first coding-agent harness driven from the terminal.  
`drip "goal"` spawns an agent loop, persists its session under `~/.drip/projects/<slug>/sessions/<id>/`,
and exits with a structured result. The TUI (`drip --tui`) and the `dripw`
watcher are monitors layered on the same session store — not the primary
interface.

drip is a Rust port of [lci](https://github.com/worldbreakstudios/local-code-inference)
(TypeScript/Bun): same flags, same `--json` contract, same on-disk session
format, same tool pack — checked scenario-by-scenario against lci with a
differential harness. It compiles to a single static binary, starts in a few
milliseconds, and needs no runtime.

---

## Install

```sh
cargo install --path .        # puts `drip` and `dripw` on your PATH
# or build in place:
cargo build --release && ./target/release/drip "goal"
```

Coming from lci? Copy your config, keys, skills, marketplaces and sessions
across (nothing is moved or deleted; existing destination files are kept):

```sh
drip --migrate-from-lci --dry-run   # show the plan
drip --migrate-from-lci             # ~/.lci → ~/.drip (or $LCI_HOME → $DRIP_HOME)
```

---

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

---

## Relationship to lci

lci is the reference implementation the port was checked against: the
differential parity harness in the
[local-code-inference](https://github.com/worldbreakstudios/local-code-inference)
repo runs each scenario against both binaries and diffs the normalized
artifacts — stdout, stderr, exit code, `state.json`, `transcript.jsonl`,
`result.json`, session index rows, and every request the model saw. Each
Rust module names the TypeScript file it ports in a header comment.

---

## Contributing

PRs welcome. Please run `cargo build --release && cargo test` before submitting.

---

## License

MIT
