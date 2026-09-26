---
name: fix-review-findings
description: Address findings from a code review — triage each finding as valid or not with a one-line reason, fix the valid P0/P1s at their cause, verify after each batch, and record the resolution of every finding including the ones declined
---

# Plan: fix review findings

Use this plan when the goal hands over findings from a review (P0/P1/P2
items, reviewer comments, a `drip --review` report) and asks for them to be
addressed. Adapt it: keep the order, and replace step 3 with one task per
finding or batch of related findings.

1. **List the findings.** Enumerate every finding with its severity, file, and
   claim, in the reviewer's order. Nothing is fixed or dropped until it is on
   this list.
2. **Triage.** For each finding, read the cited code and decide: valid, invalid,
   or out of scope — with a one-line reason. A finding judged invalid is
   recorded, never silently skipped. P0/P1 valid findings are the work; P2 and
   below are fixed only when trivial and listed as deferred otherwise.
3. **Fix tasks.** Replace this step with one task per valid finding (or per
   batch touching the same file), each naming the file, the change, and the
   check that proves it. Fix at the cause the reviewer identified; do not
   widen into unrelated cleanup.
4. **Verify after each batch.** Run the project's declared checks and any test
   the finding cited. A fix that breaks something else is not done.
5. **Commit as instructed.** If the goal says amend, amend; if it says a new
   commit, commit; if it says nothing, leave the tree uncommitted and say so.
6. **Resolution record.** For every finding on the step-1 list, state fixed /
   declined (why) / deferred, so the reviewer can check the list against the
   diff.
