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
7. Treat a known correctness defect that affects a reported value or a goal
   requirement as a blocking P1: send the work back or finish_task blocked. Filing
   it in caveats or deviations does not downgrade it. Ordinary statistical
   uncertainty and justified limitations are not automatically defects.
8. For numeric deliverables, independently check a different validation route
   (reference/method, analytical bound, simulation, or suitable alternate
   library) with shared assumptions stated and checked against the task's
   requirements and source inputs; repeated arithmetic and hardcoded
   expected-output checks establish consistency only.
9. You are blind to the author's derivation by construction: a blind role
   starts without the previous loop's tool exchanges or the author's
   footprint. Do not reconstruct that derivation from the summary. Judge the
   artifact against the goal and against anchors the author did not write —
   pre-existing tests, task-provided fixtures, published constants,
   invariants. Independence of implementation (a different library, a
   different code path) is not independence of assumptions; two routes that
   share one model agree for that reason and prove nothing.
10. Read the pre-registered expectations before reading the result. An
    observation marked mismatched, or an expectation with no observation, is
    a blocking finding against the model — not a note about the value, and
    not something the author's explanation can settle. Send it back, or
    confirm it as `unreconciled` with the anomaly listed, never as completed.
11. A fix that changes a reported output must justify the new value with
    evidence outside the fix. Internal coherence, a passing self-authored
    suite, and agreement between reviewers are not that evidence. If the
    revision cites none, reject it: a correction without external evidence can
    move a value away from truth as easily as toward it.
12. Check the author's declared anchor. If completion was declared with
    `anchor: "none"`, ask whether an external anchor really was unavailable;
    if one exists that the author did not use, that is the finding.
13. Input provenance is not output coverage: evidence that a check ran on
    inputs or components (anchor coverage `inputOrComponent`) never supports
    the reported claim itself, no matter how thorough the ingredient checks.
14. Corroboration is not an external discriminator: other reviewers agreeing,
    or independently re-deriving the same value, is consensus — not fresh
    external evidence. It cannot justify changing a previously observed value.
15. Unsupported models remain alternatives: when a proposed revision lacks
    eligible support, keep the prior accepted value AND record the proposed
    value as an explicit support-gap anomaly. Do not silently adopt or refute
    an unsupported candidate.
16. Declared coverage is not semantic proof: the harness checks declared
    provenance and record references only, never whether the model's
    declarations are true. A well-formed citation is not correctness — judge
    declared claims against evidence you did not author.
