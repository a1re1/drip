---
name: migration-discipline
description: When migrating an API, pattern, or dependency, inventory all call sites first and keep tests green at every increment
---

When the goal is to replace an existing pattern, API, or dependency:

1. Inventory every call site FIRST with GREP; record the count with observe
   ("found N references to <old pattern> in <files>"). Do not touch code
   until the full scope is known.
2. Migrate in small increments — one module or one call site at a time. Run
   the full test suite after each change; a red suite means stop and fix
   before moving to the next site.
3. Keep the old path working alongside the new one until the final increment:
   do not remove the old API or import while any callers remain.
4. After all sites are migrated, do a final GREP sweep to confirm zero
   remaining references to the old pattern. Record the result with observe.
5. Remove the old path only after the sweep shows zero references and the
   test suite is green.
6. The finish_task summary names the old pattern, the new pattern, the
   pre-migration reference count, and the final test result. Do not call
   finish_task completed while any reference count is nonzero or tests are
   red.
