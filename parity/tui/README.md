# TUI parity check (pty + terminal emulator)

`drive.py` runs `<bin> --tui` inside a pty, feeds it keystrokes, and renders
the output through [pyte](https://github.com/selectel/pyte) — a real VT
emulator — so what is compared is the screen a human would see, not the
raw escape stream. Run the same script against `bun src/cli/main.tsx` and
`drip/target/release/drip`, then diff the snapshots.

```
python3 -m venv .venv && .venv/bin/pip install pyte
bun run drip/parity/mock-model.ts --responses drip/parity/tui/responses-abort.jsonl --log /tmp/req.jsonl --port 0
# responses-basic.jsonl is the script for /help, /model (↓ enter), a run, /state, /sessions, typing, "/mo" menu
# point <home>/config.json's mock profile at the printed port, then:
.venv/bin/python drip/parity/tui/drive.py drip/target/release/drip <home> <repo> \
  "Read hello.txt then think slowly\r" @sleep @sleep "@snap running" "\x1b" @sleep "@snap after-esc" \
  "@resize 24x80" "@snap resized" "@paste line one\nline two" "@snap pasted" "\x1b" "@hel" "@snap mention" \
  "\x1b" "/resume\r" @sleep "@snap resume-picker" "\x1b" "/new\r" @sleep "@snap new"
```

Steps: text is keystrokes with `\r` enter, `\e`/`\x1b` esc, `\x1b[B` down,
`\x7f` backspace, `\uHHHH` escapes (other text is sent as UTF-8), `@sleep` waits 1s, `@snap <label>` prints the 30×100
screen, `@resize <rows>x<cols>` sends SIGWINCH, `@paste <text>` sends a
bracketed paste. Normalize session ids/timestamps/durations before diffing.

Known, accepted differences (2026-09-02): ink leaves one trailing blank row
after the status line, so lci's screen is scrolled one row further; after a
resize ink's clear-and-repaint leaves the two rows above the repaint start on
screen, drip's does not.
