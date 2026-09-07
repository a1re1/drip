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
