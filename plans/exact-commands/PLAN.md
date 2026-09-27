---
name: exact-commands
description: Run precisely the commands the operator wrote, in order, stopping at the first failure, and return their output verbatim — no edits, no interpretation, no extra investigation
---

# Plan: run the exact commands

Use this plan when the goal spells out the shell or git commands to run and
wants their output back, sometimes with a commit or a PR created by those
very commands. Adapt it: keep the order, and replace step 2 with the
commands the goal gives, in the goal's order.

1. **Confirm the location.** Run from the directory the goal names (or the
   workspace root) and do not `cd` elsewhere; confirm the branch when the
   goal names one, and stop with a report if it differs.
2. **Run each command as written.** Replace this step with one task per
   command or command group, exactly as the goal wrote it — same flags, same
   quoting, same order. Stop at the first nonzero exit and report its output;
   do not retry with a variation or fix the cause.
3. **Return the output verbatim.** Put the raw stdout/stderr in the final
   summary in a fenced block, untruncated unless the goal set a bound, with
   no commentary the goal did not ask for.
4. **Do nothing else.** No file edits, no extra commands, no verification
   beyond what the commands themselves do, no review tasks.
