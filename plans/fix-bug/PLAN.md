---
name: fix-bug
description: Fix a reported failure — reproduce it first, find the root cause rather than the symptom, fix it at the cause, pin it with a regression test, and re-run the original reproduction to prove it is gone
---

# Plan: fix a bug

Use this plan when the goal reports something that is failing: an error
message, a crash, a panic, a hang, a flaky test, or wrong output. Adapt it:
keep the order, and replace step 3 with the fix this defect actually needs.

1. **Reproduce.** Run the failing thing the way the operator hit it (the pasted
   command, the test, the keystroke) and capture the exact failure text. If it
   cannot be reproduced, say so and stop the plan at a report — do not fix
   what has not been seen.
2. **Root cause.** Trace from the failure text to the code that produced it:
   read the error site, then its callers, then the state that got it there.
   Write the cause in one sentence before changing anything. A fix that only
   removes the symptom (a retry, a catch-all, a widened match) is not a fix.
3. **Fix at the cause.** Replace this step with the minimal change that removes
   the cause, naming the file and function. Do not refactor around it or fix
   neighbouring things the goal did not report.
4. **Regression test.** Add a test in the existing test module that fails on
   the old code and passes on the new — the reproduction from step 1 turned
   into a unit test where possible.
5. **Verify.** Re-run the original reproduction and the project's declared
   checks. Compare with a baseline taken before the fix so pre-existing
   failures are not confused with this one.
6. **Report.** State the cause, the fix, and the proof in three lines. Commit
   or open a PR only if the goal asked for it.
