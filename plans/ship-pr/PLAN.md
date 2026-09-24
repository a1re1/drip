---
name: ship-pr
description: Ship a change end to end — establish a baseline, open the draft PR up front, implement, verify, commit and push, watch the checks go green, run an independent review, fix its P0/P1 findings, and leave the PR ready for the operator
---

# Plan: ship a PR

Use this plan when the goal is to *deliver* work as a pull request the operator
can review. Adapt it: keep the order, drop steps this repository does not have,
and replace step 3 with the implementation steps this goal actually needs.

1. **Baseline verification.** Before touching anything, run the project's
   declared checks (formatter, linter, tests) on the untouched tree and record
   what was already red. Later verification is only meaningful against that
   baseline, and a pre-existing failure is never the work's to explain away.
2. **Draft PR first.** Commit nothing yet: open (or reuse) a DRAFT PR whose
   description states the operator's goal verbatim, the acceptance criteria,
   and what will change. A PR up front keeps diff, intent and review
   conversation in one place as the work grows. Put the step-1 baseline under
   "Verification".
3. **Implementation steps.** Replace this step with the task list the goal
   needs — one task per coherent change, each naming the files it touches and
   the check that proves it. Keep every task scoped to the goal.
4. **Verify.** After the last edit, run the project's declared verification
   (tests, typecheck, build) and put the counts in the PR body. Never weaken a
   check to make it pass.
5. **Commit and push.** Commit the scoped changes with a clear message and push
   the branch. The PR stays a draft.
6. **Watch the checks.** If the PR has CI, monitor the run to completion; a
   failure is a fix, not a footnote. Re-push and re-watch until the checks are
   green.
7. **Independent review.** Run `drip --review --context "<the operator's goal>"`
   (or the project's review skill) against the branch, so the work is judged by
   something other than its author.
8. **Triage and fix.** Fix every valid P0/P1 finding; for a finding judged
   invalid, record why in one line instead of dropping it silently. Re-run
   verification after each fix batch and push. List P2-and-below findings
   honestly as deferred in the PR body.
9. **Leave the PR reviewable.** Final push, PR body updated with verification
   evidence, review findings and their resolutions, and any blocker. Do not
   mark the PR ready and do not merge — that is the operator's call.
