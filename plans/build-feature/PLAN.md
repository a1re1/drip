---
name: build-feature
description: Add a capability to an existing codebase — learn the current behavior and its seams first, implement in scoped tasks that each name their files and their check, cover the new behavior with tests, verify against the baseline, and leave the tree the way the operator asked for it
---

# Plan: build a feature

Use this plan when the goal asks for new behavior in software that already
exists: a command, flag, mode, panel, tool, integration, or option. Adapt it:
keep the order, and replace step 3 with the tasks this feature actually needs.

1. **Learn the current behavior.** Find where the neighbouring behavior lives
   (the command table, the flag parser, the panel, the event) and read only
   those ranges. Name the seams the feature plugs into and any existing helper
   it should reuse instead of duplicating. If the goal names a reference
   ("like Claude does", "similar to X"), write down the observable behavior
   being imitated in one line so every task targets the same thing.
2. **Baseline.** Run the project's declared checks on the untouched tree and
   record what was already red; later verification is only meaningful against
   this.
3. **Implementation tasks.** Replace this step with one task per coherent
   change — data model, wiring, surface, docs — each naming the files it
   touches and the check that proves it. Keep the existing behavior working
   through every task; the feature is additive unless the goal says otherwise.
4. **Tests for the new behavior.** Add unit tests next to the code, in the
   existing test module and style, pinning the behavior the goal asked for and
   at least one edge (empty input, disabled flag, missing config).
5. **Verify.** Run the declared checks again and compare with the baseline;
   exercise the feature once the way the operator would (the CLI command, the
   keystroke, the request) and record what happened. Never weaken a check to
   make it pass.
6. **Leave it as asked.** Update help text and docs the feature touches. Commit
   or open a PR only if the goal asked for it; otherwise leave the changes in
   the working tree and say exactly what was verified and what was not.
