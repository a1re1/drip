# drip — Rust port of lci

`drip/` is a ground-up Rust rewrite of the `lci` harness (the TypeScript code
under `src/`) with the goal of **behavioral parity**: the same CLI flags, the
same on-disk artifacts, the same prompts sent to the model, the same exit
codes. The web UI (`src/client`, `src/web`, `src/server`) is intentionally
out of scope. Everything else is in scope:

| lci                              | drip                                    |
| -------------------------------- | --------------------------------------- |
| `lci` (headless CLI harness)     | `drip` (binary `drip`)                  |
| `lciw` (watch TUI)               | `dripw` (binary `dripw`)                |
| `lci --tui` (Ink TUI)            | `drip --tui` (ratatui) — last phase     |
| `~/.lci`, `$LCI_HOME`            | `~/.drip`, `$DRIP_HOME`                 |
| `<project>/.lci`, `$LCI_PROJECT_DIR` | `<project>/.drip`, `$DRIP_PROJECT_DIR` |
| `.lci/policy.json`, `.lci/skills`, `.lci/roles.json`, `.lci/plugins.json`, `.lci/patches.jsonl` | same names under `.drip/` |
| —                                | `drip --migrate-from-lci [--from <dir>] [--dry-run] [--project]` |

Rename rule: every user-visible occurrence of `lci`/`LCI` becomes
`drip`/`DRIP` (`lci --resume` → `drip --resume`, `LCI_HOME` → `DRIP_HOME`,
`.lci` → `.drip`). File formats, JSON shapes, sqlite schema, prompt text,
exit codes and error messages are otherwise identical.

## Layout

```
drip/
  Cargo.toml                 single crate `drip`, bins `drip` + `dripw`
  src/
    main.rs                  bin drip: dispatch (port of src/cli/main.tsx)
    bin/dripw.rs             bin dripw (port of src/cli/watch/*)
    lib.rs                   module tree
    cli/                     args, help, runner, session_run, gc, inspect, review,
                             review_report, skills, roles, marketplaces, images,
                             mentions, queue, transcript, delegate_tool, terminal
    core/                    home, config (settings + shipped profiles), env_vars,
                             sessions (sqlite), lease, state, types
    harness/                 loop, prompt, harness_tools, model_call, anthropic,
                             transport, telemetry
    tools/                   types, execute, helpers, child_process, command_policy,
                             patch_journal, async_jobs, builtin/{read,grep,dir,bash,
                             patch,fetch,check,verify}
    watch/                   data, ps, shelllog, render, transcript_view, app
    tui/                     port of src/cli/ui (phase 5)
    migrate.rs               lci → drip migration
  PLAN.md                    this file
```

Each Rust module names the TS file it ports in a header comment. Port
comments and constants verbatim where they explain behavior.

## Parity strategy — lci is the oracle

> The `drip/parity/` harness described below lives in the local-code-inference
> monorepo next to the lci source; it was dropped from this standalone crate.

There are no golden files. `drip/parity/run.ts` (Bun) runs every scenario
under `drip/parity/scenarios/<name>/` twice — once with `lci`, once with
`drip` — in fresh temporary homes/projects against the same **mock model
server**, then diffs the normalized results:

1. stdout, stderr, exit code
2. `state.json`, `transcript.jsonl` (event kinds + payloads), `result.json`,
   `session.json`, the sqlite session index rows
3. every request the mock server received (system prompt, messages, tools,
   model parameters) — byte-identical after normalization
4. the fixture repo's working tree after the run (PATCH/BASH effects)

Normalization: session ids/uuids → `<UUID>`, ISO timestamps → `<TS>`,
durations/ms → `<MS>`, absolute temp paths → `<HOME>`/`<PROJECT>`, `lci` →
`drip` (word-bounded, plus `LCI_`→`DRIP_`, `.lci`→`.drip`), versions →
`<VERSION>`.

A scenario is a directory with:

```
args.json        ["--json", "--max-iterations", "3", "do the thing"]
responses.jsonl  one mock model reply per line, served in order (a line may
                 carry {"when":{"contains":"..."}} to gate on the request)
fixture/         files copied into the temp project (git init + commit first)
env.json         optional extra env vars
```

The mock server (`drip/parity/mock-model.ts`) serves OpenAI-compatible
`POST /v1/chat/completions` and Anthropic `POST /v1/messages`, records every
request to `requests.jsonl`, and returns canned replies (tool calls
included). Both homes get a `config.json` whose only profile `mock` points
at it.

Static contracts checked by dedicated scenarios: `--help` (byte parity after
rename), `--version` shape, usage errors (exit 1 + message), bare init,
`--list`/`--state`/`--result`/`--inspect` on seeded sessions, `--gc
--dry-run`, `--skills`, `--marketplace-list`.

Unit-level parity: every `test/*.test.ts` that exercises a ported module is
ported to a Rust test (`cargo test`) alongside the module. The port is not
done until the TS test's assertions hold in Rust.

Run everything with `drip/parity/check.sh` (cargo build + cargo test +
parity runner). CI-verbatim commands:

```
cd drip && cargo build --release && cargo test
bun run drip/parity/run.ts            # all scenarios; --only <name>
```

## Work plan (waves; each item is one PR built by lci, reviewed, merged)

- **W1a scaffold**: Cargo crate, module tree, `core/types.rs` (serde port of
  `src/harness/types.ts`), `cli/args.rs` + `cli/help.rs` with the full flag
  set and byte-parity `--help`, `--version`; ports of `test/cli-baseline`,
  `test/cli-help-drift`.
- **W1b parity harness**: mock model server, runner, normalizer, first
  scenarios (help, version, usage errors).
- **W2 (parallel)**: `core/home` + `core/config` + `core/env_vars` (shipped
  profiles from `src/web/settings.ts`, upgrades/migrations included);
  `core/sessions` + `lease` + `queue` + `transcript` + `state`;
  `harness/prompt` + `model_call` + `anthropic` + `transport` + `telemetry`;
  `tools/*` (built-in pack, policy, patch journal, async jobs/tmux).
- **W3**: `harness/harness_tools` + `harness/loop`; `cli/skills` + `roles` +
  `marketplaces`; `cli/gc` + `inspect` + `images` + `mentions`;
  `cli/review` + `review_report`.
- **W4**: `main.rs` dispatch + `runner` + `session_run` (goal runs, `--json`
  NDJSON, `--detach`/`--wait`/`--result`/`--follow`/`--send`/`--stop`,
  `--enqueue`, `--plan`, `--undo-last`, delegate tool); `dripw`;
  `--migrate-from-lci`.
- **W5**: parity scenarios covering goal runs end to end (plan → tools →
  finish, steering, stop, resume, review); `--tui`; docs + README section.

Known deviations (to be listed in `drip/README.md`): `--tools <path>`
loads TypeScript tool packs in lci; drip accepts the flag but only ships
the built-in pack (a non-default path is a usage error naming the
limitation).
