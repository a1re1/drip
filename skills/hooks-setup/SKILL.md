---
name: hooks-setup
description: Wire drip lifecycle hooks (the config.json "hooks" block) when a goal asks for hooks, event scripts, or notifications on session or tool events
---

When the goal asks to add hooks to drip, run scripts on drip events, or
notify on session/tool activity:

1. Hooks live in drip's persisted config file (`~/.drip/config.json`) as
   a top-level `hooks` object. Read the file first and preserve every
   existing key — edit only the `hooks` block.
2. Pick events from the README "Hooks" table: `session_start`,
   `loop_start` / `loop_finish`, `task_start` / `task_finish`,
   `pre_tool_use`, `post_tool_use`, `stop`, plus the drip-specific
   `relay_start` / `relay_finish` (relay rounds), `memory_write`
   (`remember`/`forget`), and `pr_ready` (the run publishes). The two
   tool events take arrays of `{ "matcher", "command" }` objects
   (matcher is a `|`-separated tool list with optional trailing `*`);
   every other event takes an array of command strings — a bare string
   instead of an array is a malformed block. `timeout_seconds` caps
   every hook (default 10).
3. Write hook commands defensively: each gets one JSON payload on stdin
   (`event`/`cwd`/`tool_name`/`tool_input`/`timestamp`), runs under
   `$SHELL -c` with the session's cwd, and is killed at the deadline.
   Failures only surface as warnings — a hook must never stall a run;
   the one exception is `pre_tool_use` exiting `2`, which vetoes the tool
   call and returns the hook's stderr to the model.
4. Never put untrusted commands in a hooks block: hooks run with the
   user's privileges and are not sandboxed. Prefer small scripts under
   `~/.drip/hooks/` over long inline one-liners.
5. Verify before done: the config still parses (a malformed `hooks`
   block is ignored with a warning), and one hook fires by hand — pipe a
   sample payload into the command exactly as drip would.