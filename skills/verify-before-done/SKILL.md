---
name: verify-before-done
description: Never mark code work completed without running the project's own checks and seeing them pass
---

Before calling finish_task with status completed on any task that changed
code, configuration, or build files:

1. Find the project's declared checks — package.json scripts (test, check,
   lint, build), a Makefile, or CI config — and run the relevant ones with
   BASH. Prefer declared commands over improvised equivalents.
2. Read the command's exit status line in the tool result. Only a visible
   passing result counts; "it should pass" does not.
3. Record the verification in the finish_task summary with the concrete
   result ("bun test: 24/24 passed"), and observe any failing detail before
   fixing it.
4. If the checks cannot be run (missing dependency, no test runner), say so
   explicitly in the summary as "not verified: <reason>" — never imply
   verification that did not happen.
5. A task whose whole point is verification must never be completed with the
   checks red; finish_task status blocked with the failing output instead.
