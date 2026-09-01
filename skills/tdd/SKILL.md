---
name: tdd
description: Test-first discipline — encode the expected behavior in a failing test before implementing
---

For each task that adds or changes behavior:

1. Write or extend a test that encodes the expected behavior FIRST, using
   the project's existing test framework and conventions (look at a
   neighboring test file before writing).
2. Run it and confirm it FAILS for the expected reason — a test that passes
   before the change proves nothing. Note the failing output with observe.
3. Implement the minimal change that makes it pass.
4. Run the project's full test command; fix regressions you caused before
   finishing the task.
5. The finish_task summary names the test(s) added and the final pass count.
   If the task genuinely has no testable surface, say so explicitly instead
   of skipping silently.
