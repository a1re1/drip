# drip — the Rust port of lci

`drip` is a behavior-for-behavior port of the `lci` harness (TypeScript/Bun,
`src/`) to Rust. Same CLI, same flags, same session storage, same wire
requests, same transcripts — with `lci` renamed `drip`, `LCI_*` env vars
renamed `DRIP_*`, and `~/.lci` become `~/.drip`. `dripw` is the port of
`lciw`, the read-only watch TUI.

```
cd drip && cargo build --release   # drip/target/release/{drip,dripw}
ln -sfn "$PWD/target/release/drip" ~/.cargo/bin/drip     # same model as the global lci:
ln -sfn "$PWD/target/release/dripw" ~/.cargo/bin/dripw   # a merged PR goes live after git pull + cargo build --release
drip --help                        # identical to lci --help modulo the rename
drip --migrate-from-lci            # copy ~/.lci (config, env.vars, sessions, index) into ~/.drip
```

`drip --version` reports the lci version it is a port of (`drip/Cargo.toml`
and `package.json` are kept in lockstep by `drip/tests/version_lockstep.rs`).

## How parity is checked

`lci` is the oracle. `drip/parity/run.ts` runs every scenario under
`drip/parity/scenarios/<name>/` against **both** binaries with a scripted
mock model server (`drip/parity/mock-model.ts`) and diffs the normalized
artifacts: stdout, stderr, exit code, `state.json`, `transcript.jsonl`,
`result.json`, the sqlite session index rows, every request the mock
received, and the fixture's `git status`/`git diff`.

```
bun run drip/parity/run.ts --drip-bin drip/target/debug/drip [--only <name>] [--keep]
bun run drip/parity/run.ts --self-check         # lci vs lci (runner sanity)
cd drip && cargo test                            # unit + fixture tests (tool schemas, prompt snapshots, help)
```

A scenario is `args.json` (one invocation) or `steps.json` (several, in the
same project/home; `["@sleep", "<ms>"]` pauses between them), plus
`responses.jsonl` for the mock and an optional `fixture/` (committed as the
project; `fixture-base/` is committed first and tagged `base` for `--review`).
Timestamps, ids, pids, durations, and paths are masked by
`drip/parity/normalize.ts`; everything else must match byte for byte.

Byte-exact fixtures dumped from the TS (`drip/tests/fixtures/*.json`) pin the
tool schemas, harness tool schemas, and prompt text in declaration order.

## Known deviations

These are the places drip is deliberately not byte-identical to lci. Each is
either a platform difference or an lci feature that only makes sense in Bun.

- **`--tools <path>`** — lci loads TypeScript tool packs; drip only ships the
  built-in pack and rejects any other path with a usage error.
- **`--tui`** — ported without Ink: the timeline scrolls into the terminal's
  scrollback and the composer/picker/status bar are repainted in place with
  raw ANSI (`src/tui/app.rs`). Same slash commands, keys, mention and slash
  menus, image paste/ctrl+v, bracketed paste, and per-session transcript
  replay. Visual differences: box-drawing borders are painted by
  `tui::widgets::boxed` (Ink's `round` border), text wrapping is the port's
  own, and the help/status strings still say "lci" where the TS does. drip's
  TUI also scrubs `env.vars` secrets from tool output like headless runs do
  (the Ink TUI passes no redaction list).
- **CHECK** shells out to `tsc` like lci, and **PATCH**'s syntax check runs
  through `bun` when it is on PATH; without Bun the syntax pass is skipped.
- **Markdown in `dripw`** — lci renders through `marked-terminal`; dripw has a
  small renderer of its own (`src/tui/markdown_ansi.rs`): same headings,
  emphasis, code, lists and quotes, not the same bytes.
- **Built-in skill paths** print as `<builtin>/<name>/SKILL.md` in `--skills`
  output (lci prints the path inside its source tree).
- **`state.json` key order** follows the Rust struct; lci writes keys in
  object-mutation order. The files are structurally identical (`--state
  --json --full` output differs in key order only).
- **`--migrate-from-lci`** was dogfooded against a live `~/.lci` (446 MB,
  30 sessions, 9928 files): `--list`, `--state`, `--inspect` and `--skills`
  output from the migrated `~/.drip` is byte-identical to lci's.
- **Error text from the runtime** (a malformed tool-call JSON, a failed
  `kill`) uses Rust's messages rather than Bun's `SyntaxError: …` /
  `kill ESRCH` strings.
- **`--detach`** children run in a new process group rather than a new session
  (`setsid`); they still survive the parent and log to `run.log`.
- **`--migrate-from-lci`** exists only in drip (its `--help` block is stripped
  by the parity normalizer).

## Layout

See `drip/PLAN.md` for the module map (`src/core`, `src/harness`, `src/tools`,
`src/cli`, `src/watch`, `src/tui`) and the wave plan the port followed.
