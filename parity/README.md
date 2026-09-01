# drip/parity — differential parity harness

Compares a future Rust `drip` binary against `lci` (the TypeScript CLI at
`src/cli/main.tsx`, run as `bun run src/cli/main.tsx`), which is the oracle.

Each **scenario** is a fixed CLI invocation plus a scripted mock-model. The
harness runs the same scenario twice — once with `lci`, once with `drip` —
against identical throwaway environments (HOME + git project + mock server)
and diffs every artifact it can observe: stdout, stderr, exit code, session
files, the sqlite session index, the mock request log, and `git status/diff`
of the project. All nondeterminism (UUIDs, timestamps, temp paths, ports,
versions, binary name) is removed by the normalizer before comparing; the
two sides must then be byte-identical.

## Quick start

```sh
drip/parity/check.sh --self-check   # lci vs lci — validates the harness itself
drip/parity/check.sh                # lci vs drip (default bin: drip/target/debug/drip)
drip/parity/check.sh --only help    # a single scenario
bun run vitest run drip/parity      # unit tests for the normalizer only
```

`check.sh` is just: `bun run vitest run drip/parity` then
`bun run drip/parity/run.ts "$@"`.

- `--self-check` runs **lci on both sides**. Every scenario must PASS; a
  failure means the harness/normalizer is broken, not `drip`.
- Without `--self-check`, `--drip-bin` (default `drip/target/debug/drip`)
  must exist; otherwise run.ts prints a clear message and exits 2.
- `--keep` leaves the per-scenario temp roots in place and prints their
  paths (see "Debugging a FAIL").
- Exit code is 1 if any scenario FAILs, 2 for a missing drip binary,
  0 otherwise.

## Scenario layout

One directory per scenario under `drip/parity/scenarios/`:

```
scenarios/<name>/
  args.json         # JSON array of CLI args, e.g. ["--json", "--max-iterations", "3", "Say hello"]
  responses.jsonl   # one mock-model reply per line; may be empty
  fixture/          # optional; copied into the temp project dir
```

Response lines are JSON objects:

```jsonc
{"content": "plain text reply"}
{"toolCalls": [{"name": "respond", "arguments": {"text": "Hello"}}]}
{"when": {"contains": "hello.txt"}, "content": "..."}   // only served if the request body contains the string
{"status": 500, "error": {"message": "boom"}}           // non-200 reply
```

Replies are served in file order. A line with `when` is only consumed when
the request body contains `contains`; otherwise the next unconditional line
is served (the gated line stays queued). When the list is exhausted the mock
returns `{"content":"(mock exhausted)"}`. Every request is appended to the
mock's log file as one `{"seq","path","body"}` JSON line; the log becomes a
compared artifact (order-normalized as a multiset, since the two sides may
poll at different times).

## How a scenario runs

`run.ts` builds two identical throwaway roots, `<side>/home` and
`<side>/project` (`side` = `lci` or `drip`):

- `home/config.json` — settings with exactly one model profile
  `{"id":"mock", "provider":"openai-compatible",
  "baseUrl":"http://127.0.0.1:<port>/v1"}`, active profile and active tool
  profile both `mock`; `home/env.vars` empty. The config's only profile
  points at the per-side mock server — no real network is ever contacted.
- `project/` — the scenario's `fixture/` copied in, then
  `git init && git add -A && git commit -m fixture`.
- One mock server per side (`drip/parity/mock-model.ts`), fed
  `responses.jsonl`, logging to `<side>/mock-log.jsonl`.
- The CLI runs with cwd = `project`, env `HOME`/`LCI_HOME` = `home`
  (`DRIP_HOME` for the drip side), `LCI_PROJECT_DIR` unset, plus any
  `env.json` in the scenario dir. Hard timeout: 120 s, child killed and
  reported as `FAIL <name> timeout`.

Collected per side: stdout, stderr, exit code, every
`home/projects/*/sessions/*/` file (`state.json`, `transcript.jsonl`,
`result.json`, `session.json`), the sqlite session-index rows, the mock
request log, and `git status --porcelain` + `git diff` of `project/`.

## Normalization

`normalize.ts` maps each artifact to a canonical form before diffing:

- UUIDs → `<UUID>`, ISO timestamps → `<TS>`, epoch-millis → `<MS>`
- loopback ports → `<PORT>`, temp paths anchored at the roots → `<HOME>`/`<PROJECT>`
- version strings → `<VERSION>`, the word `lci` → `drip` (and `LCI_`/`DRIP_` env names)
- session JSON: keys sorted case-insensitively, timestamp/duration keys masked,
  error payloads collapsed to `<ERROR>`
- transcripts: pretty-printed event re-join; mock logs: `seq` stripped,
  per-request ids scrubbed, deduped + sorted (multiset compare)

Unit tests live in `normalize.test.ts` (`bun run vitest run drip/parity`).
When `drip` produces a legitimately new artifact shape, extend the
normalizer **and its tests** — never weaken a rule to force a pass.

## Adding a scenario

1. `mkdir drip/parity/scenarios/<name>` with `args.json` (JSON array) and an
   (optionally empty) `responses.jsonl`; add `fixture/` if the args reference files.
2. For model-driven scenarios, script replies that exercise the harness tools
   (`respond`, `finish_task`, `plan_tasks`; workspace tools READ/PATCH/BASH/…);
   keep `--max-iterations` small so a broken loop ends quickly.
3. `drip/parity/check.sh --self-check --only <name>` — must PASS (lci vs lci
   is the harness's own sanity check).
4. Once a Rust `drip` exists: `drip/parity/check.sh --only <name>` and fix the
   real differences it surfaces.

## Debugging a FAIL

```sh
bun run drip/parity/run.ts --self-check --only run-read-patch --keep
# run.ts prints the unified diff per differing artifact, then e.g.:
#   kept: /tmp/drip-parity-<ts>/run-read-patch/{lci,drip}
```

- `lci/stdout`, `lci/stderr`, `exit-code` — how the CLI itself behaved.
- `lci/mock-log.jsonl` — what the model actually received (the raw request
  bodies, in `seq` order) when replies look mis-gated.
- `lci/home/projects/*/sessions/*/state.json` / `transcript.jsonl` — task
  state and the event stream mid-run.
- `git-status` / `git-diff` — what the run changed in the fixture.

Because `--self-check` runs lci on both sides, any diff it shows is
harness nondeterminism the normalizer missed — capture it, add a rule +
unit test, and re-run.
