---
name: refactor-safely
description: When refactoring, establish a green baseline first and take only small reversible steps that preserve behavior
---

When the goal is to refactor existing code:

1. Run the full test suite FIRST and record the result with observe. A red
   baseline must be fixed before refactoring begins — never refactor on top
   of pre-existing failures.
2. Take one small, reversible step at a time: rename, extract, or move a
   single thing. Commit or note the change before taking the next step.
3. Re-run the tests after each step. Green means proceed; red means revert
   the step immediately — do not patch forward to compensate.
4. Preserve the public API and all observable behavior unless the goal
   explicitly says otherwise. If an API change is required, call it out in
   the task summary, not silently.
5. When all steps are done, run the full test suite one final time and
   confirm it still matches the baseline pass count.
6. finish_task completed only after the final test run is green. Include the
   baseline count and the final count in the summary.
