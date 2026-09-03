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

`lci` is the oracle. The differential harness (`drip/parity/` in
[local-code-inference](https://github.com/a1re1/local-code-inference)) runs
every scenario against **both** binaries with a scripted mock model server
and diffs the normalized artifacts: stdout, stderr, exit code, `state.json`,
`transcript.jsonl`, `result.json`, the sqlite session index rows, every
request the mock received, and the fixture's `git status`/`git diff`. That
harness is not part of this crate; here:

```
cargo test    # unit + fixture tests (tool schemas, prompt snapshots, help)
```

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
