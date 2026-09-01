---
name: debug-root-cause
description: When a failure appears, reproduce it with a command before touching code, then fix the cause not the symptom
---

When a task involves a failing test, error, or unexpected behavior:

1. Reproduce the failure with a BASH command BEFORE touching any code.
   Record the exact output with observe. If you cannot reproduce it, stop
   and say so — do not guess at a fix.
2. State a hypothesis: one sentence naming the suspected root cause. Record
   it with observe so later cycles can see it.
3. Prove the hypothesis with a targeted observation — read the relevant
   source, grep for the suspected pattern, or add a minimal diagnostic.
   Do not change behavior yet.
4. Fix the cause, not the symptom. A symptom-patch hides the real problem;
   touch only the code the hypothesis identified.
5. Re-run the exact reproduction command from step 1. Only a visible passing
   result in this loop's output counts as confirmed.
6. finish_task completed only after the reproduction command passes. If the
   fix does not make it pass, finish_task blocked with the remaining
   failure output.
