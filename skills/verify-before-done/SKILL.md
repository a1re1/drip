---
name: verify-before-done
description: Require meaningful verification evidence before completing changed code or artifacts
---

Before calling finish_task with status completed on any task that changed
code, configuration, build files, or deliverable artifacts (including data and
numeric results):

1. Find the project's declared checks — package.json scripts (test, check,
   lint, build), a Makefile, or CI config — and run the relevant ones with
   BASH or VERIFY. Prefer declared commands over improvised equivalents.
2. Read the command's exit status line in the tool result. Only a visible
   passing result with executed checks or compiler/build evidence counts;
   an unknown exit-zero script or an empty/all-skipped suite does not.
   For custom assertions use a supported test runner or emit the documented
   DRIP_VERIFY counts from executed checks. Counts do not prove the checks
   use the correct specification.
3. Record the verification in the finish_task summary with the concrete
   result ("bun test: 24/24 passed"), and observe any failing detail before
   fixing it.
4. If the checks cannot be run (missing dependency, no test runner), say so
   explicitly and finish_task blocked with the missing evidence. Never imply
   verification that did not happen or repeat completed to waive the gate.
5. A task whose whole point is verification must never be completed with the
   checks red; finish_task status blocked with the failing output instead.
6. When the goal quantifies over an input space, verification must include at
   least one input you constructed that differs from what is present in the
   workspace; a check that passes only on the shipped instance does not count
   as verification.
7. Treat a known correctness defect that affects a reported value or a goal
   requirement as a blocking P1: resolve it or finish_task status blocked. Listing
   it in caveats or deviations does not make it non-blocking. Ordinary
   statistical uncertainty and justified limitations are not automatically defects — state
   which you have.
8. Numeric deliverables need an independent validation route — a reference
   method or implementation, an analytical bound, a simulation, or a suitable
   alternate library — with the assumptions both routes share stated.
   Repeating the same arithmetic or checking hardcoded expected output only
   establishes consistency, not correctness.
9. Every check is either correctness-class or consistency-class, and one never
   satisfies the other. A correctness-class check compares the artifact to
   something you did not author: a pre-existing project test, a task-provided
   fixture, a published constant, an invariant that must hold regardless of
   implementation. A consistency-class check compares it to your own
   derivation. Declare the class on every VERIFY call (`anchor.kind` external
   or self, with `source`); a check that names a file you edited is
   self-authored no matter what you declare. Completion needs at least one
   passing correctness-class check, or finish_task `anchor: "none"` with an
   `anchorNote` saying why no external anchor exists for this claim — that
   declaration is recorded and downgrades the completion; hiding it is not an
   option.
10. Pre-register the expected shape before computing anything. When the goal
    produces a measurable output — a sign, a unit, an order of magnitude, a
    row count, an output shape, a latency bound — state the expectation from
    the domain, not from your derivation, in `plan_tasks.expectations` before
    the result exists. Expectations are immutable once written; finishing
    records an `observations` entry for each one. Ordering is the point: an
    expectation formed after the number is a rationalization.
11. A mismatch between an observation and its expectation is a P1 against the
    model, never a caveat on the value. Either fix the model, or finish with
    status `unreconciled` and list the mismatch under `anomalies`. That is a
    legitimate terminal state — complete, internally consistent, cannot
    reconcile with the domain — and it is cheaper than arguing the anomaly
    into plausibility. Two derivations that share a wrong assumption agree
    with each other; agreement is not evidence.
12. A revision that changes a reported output is a higher-evidence event than
    one that does not. When a fix moves a value already observed, the new
    observation must cite `evidence` from outside the fix that the new value
    is closer to truth; otherwise review is a random walk across plausible
    models with confidence rising at every step.
13. State your `confidence` (low, medium, high) honestly on every finish_task.
    It is recorded next to the evidence class in the session's
    calibration.jsonl and later diffed against the verifier's reward, so a
    confident wrong answer costs more than an uncertain one.
