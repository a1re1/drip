---
name: review-independently
description: When reviewing a completed task, re-run the verification commands yourself and read the actual diffs before accepting claims
---

When the goal is to review or verify another agent's or contributor's work:

1. Re-run the claimed verification commands yourself (BASH) — do not accept
   "tests passed" from a summary; only a passing result visible in this loop
   counts.
2. Read the actual diffs with `git diff` or READ the changed files directly;
   compare them against what the summary says changed.
3. Check every factual claim in the summary against the code, not against
   other summaries — if the summary says "X was added", grep or read to
   confirm X is present.
4. Note discrepancies immediately with observe so they survive across cycles;
   name the file and line where the claim and reality diverge.
5. If all claims check out and verification passes, finish_task completed with
   the concrete evidence: the command run, its output, and the diff confirmed.
6. If any claim does not check out or verification fails, finish_task blocked
   naming exactly what failed — never accept work that cannot be independently
   reproduced.
