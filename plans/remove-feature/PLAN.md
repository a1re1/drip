---
name: remove-feature
description: Remove a feature, page, flag, or subsystem completely — enumerate every reference before deleting, take out the surface and its dead helpers while keeping shared infrastructure, fix what the removal breaks, update docs and tests, and prove nothing else changed
---

# Plan: remove a feature

Use this plan when the goal asks to delete something that exists — a page, a
mode, a flag, a provisioning step, a scoring mechanism — usually while keeping
the machinery around it. Adapt it: keep the order, and replace step 3 with the
deletions this removal actually needs.

1. **Enumerate references.** Grep for every name, flag, config key, event,
   route, and doc mention of the thing being removed and list them by file.
   Separate what belongs only to the feature from what is shared
   infrastructure the goal says to keep.
2. **Baseline.** Run the project's declared checks on the untouched tree and
   record what was already red.
3. **Delete outward-in.** Replace this step with one task per layer — the
   user-facing surface (command, UI, flag), then the wiring, then the
   now-dead helpers and types — each naming the files it edits and ending
   with the build passing. Deleting a whole file beats leaving an empty one.
4. **Fix what broke.** Compile errors and test failures caused by the removal
   are the work: remove the dead tests, update the callers, drop the config
   key. Do not stub the feature back in to keep something compiling.
5. **Docs and help.** Remove the feature from README sections, help text, and
   changelogs the way the operator would expect.
6. **Verify nothing else changed.** Run the declared checks and compare with
   the baseline; grep once more for every name from step 1 and show zero
   hits. Commit or PR only if the goal asked for it.
