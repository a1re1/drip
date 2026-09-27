---
name: verify-only
description: Independently verify work that is already built without changing it — build it, run the declared checks and the goal's acceptance command, exercise the feature the way an operator would, and report evidence as commands and outputs, never edits
---

# Plan: verify without editing

Use this plan when the goal asks for verification, a smoke check, or an
independent confirmation of something already implemented, and says not to
modify files. Adapt it: keep the order, and replace step 3 with the
behaviors this work actually claims.

1. **Name the claims.** From the goal, the PR body, or the commit message,
   list what the work claims to do — each as one checkable sentence.
2. **Build and run the declared checks.** Build the tree as-is and run the
   project's formatter, linter, and tests plus any acceptance command the goal
   names. Record pass/fail counts and the tail of any failure verbatim.
3. **Exercise each claim.** Replace this step with one task per claim from
   step 1: the command, request, or keystroke that shows it, run in an
   isolated place (a temp dir, a throwaway home, a dev instance) so nothing
   the operator uses is touched. Record the observed output next to the
   claim.
4. **Report the evidence.** For each claim: verified, not verified, or could
   not be exercised — with the command and output that decides it. State
   plainly what was not checked.
5. **Change nothing.** No edits, no commits, no pushes, no PRs; if a defect is
   found, it goes in the report for the author, not into the tree.
