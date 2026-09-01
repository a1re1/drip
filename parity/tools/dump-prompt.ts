#!/usr/bin/env bun
// Dumps the exact prompt text lci builds for a set of fixture states, so the
// Rust port (drip/src/harness/prompt.rs) can be gated on byte equality.
// Run from the repo root:
//   bun drip/parity/tools/dump-prompt.ts > drip/tests/fixtures/prompt.json

import type { HarnessState, HarnessTask } from "../../../src/harness/types";
import {
  buildCycleContinuationMessage,
  buildFallbackRunSummary,
  buildIterationMessages,
  buildIterationUserMessage,
  buildRunSummaryMessages,
  composeHarnessSystemPrompt,
  DEFAULT_HARNESS_SYSTEM_PROMPT,
  looksLikeQuestionGoal,
  RUN_SUMMARY_SYSTEM_PROMPT
} from "../../../src/harness/prompt";

const CURRENT_DATE = "2026-09-01";

function task(partial: Partial<HarnessTask> & { id: string; title: string }): HarnessTask {
  return { createdAtIteration: 1, notes: [], stallCount: 0, status: "pending", ...partial };
}

const empty: HarnessState = {
  createdAt: "2026-09-01T00:00:00.000Z",
  goal: "add a --verbose flag to the CLI",
  history: [],
  iteration: 1,
  loop: 1,
  memory: [],
  observations: [],
  promotedContext: [],
  tasks: [],
  telemetry: {},
  version: 1
};

const question: HarnessState = { ...empty, goal: "what does the lease module do?" };

const rich: HarnessState = {
  ...empty,
  goal: "port the loop\nwith a second goal line",
  history: [
    { archivedAtIteration: 3, goal: "old goal", summary: "did the old thing", tasks: [task({ id: "t0", status: "completed", title: "old task" })] }
  ],
  inboxCursor: 2,
  iteration: 7,
  loop: 3,
  lastActivation: {
    actions: ["READ src/a.ts", "PATCH src/a.ts", "BASH cargo test (failed)"],
    cycles: 3,
    iteration: 6,
    loop: 2,
    outcome: "budget exhausted for this loop",
    taskId: "t2"
  },
  lastVerification: { atIteration: 6, command: "cargo test", failed: true, outputTail: "error[E0308]: mismatched types\n  --> src/a.rs:10:5" },
  memory: [
    { createdAtIteration: 2, id: "note-1", text: "the parser lives in src/parser.ts" },
    { createdAtIteration: 5, id: "note-2", text: "tests run with bun run test" }
  ],
  mutationsSinceVerification: 2,
  observations: [{ createdAtIteration: 6, id: "obs-1", text: "cargo test takes ~40s", ttl: 3 }],
  operatorMessages: [
    { id: "op-1", receivedAtIteration: 4, text: "stop touching the tokenizer" },
    { id: "op-2", receivedAtIteration: 6, text: "focus on src/parser.ts" }
  ],
  promotedContext: [
    {
      dynamic: true,
      inputPreview: "git status",
      key: "BASH:git status",
      output: "On branch main\nnothing to commit",
      promotedAtIteration: 5,
      rawInput: "{\"command\":\"git status\"}",
      reinforcements: 1,
      toolName: "BASH",
      ttl: 4
    }
  ],
  runSummary: { createdAtIteration: 3, reason: "partial", text: "half done" },
  tasks: [
    task({ id: "t1", status: "completed", title: "read the code", finishedAtIteration: 6, summary: "read it" }),
    task({ id: "t2", status: "in_progress", title: "write the port", activations: 2, notes: ["attempt (loop 2): budget; tried: PATCH", "parser is recursive"], stallCount: 1, footprint: ["src/a.ts"] }),
    task({ id: "t3", status: "pending", title: "run the tests", dependsOn: ["t2"], role: "reviewer" }),
    task({ id: "t4", status: "blocked", title: "publish", notes: ["needs credentials"], finishedAtIteration: 6 }),
    task({ id: "t5", status: "dropped", title: "old idea", droppedExhausted: true })
  ],
  verificationStreak: { command: "cargo test", consecutiveFailures: 3, outputTailHash: "abcd1234" }
};

const blockedOnly: HarnessState = { ...empty, iteration: 4, tasks: [task({ id: "t1", status: "blocked", title: "needs a decision", notes: ["which db?"] })] };

const states: Record<string, HarnessState> = { blockedOnly, empty, question, rich };
const currentTask = rich.tasks[1];

const out: Record<string, unknown> = {
  states,
  DEFAULT_HARNESS_SYSTEM_PROMPT,
  RUN_SUMMARY_SYSTEM_PROMPT,
  composeHarnessSystemPrompt: {
    none: composeHarnessSystemPrompt(),
    empty: composeHarnessSystemPrompt(""),
    persona: composeHarnessSystemPrompt("You are a terse reviewer.\nNever guess.")
  },
  looksLikeQuestionGoal: Object.fromEntries(
    [
      "what does the lease module do?",
      "explain how sessions are stored",
      "add a --verbose flag",
      "fix the bug in parser.ts",
      "why is cargo test slow",
      "how do I run this? add a README section",
      "is the queue module thread safe",
      "",
      "!ls",
      "Describe the architecture"
    ].map((g) => [g, looksLikeQuestionGoal(g)])
  ),
  buildIterationUserMessage: {
    empty_minimal: buildIterationUserMessage(empty, { currentDate: CURRENT_DATE, currentTask: undefined }),
    question_minimal: buildIterationUserMessage(question, { currentDate: CURRENT_DATE, currentTask: undefined }),
    blockedOnly: buildIterationUserMessage(blockedOnly, { currentDate: CURRENT_DATE, currentTask: undefined, stallLimit: 3 }),
    rich_full: buildIterationUserMessage(rich, {
      currentDate: CURRENT_DATE,
      currentTask,
      loopInfo: { index: 3, maxCycles: 4, role: { description: "ports code faithfully", name: "porter" } },
      repoMemoryDir: "/repo/.lci/memory",
      repoMemoryIndex: "- parser.md: parser notes\n- tests.md: test notes",
      runBudget: { total: 10, used: 7 },
      stallLimit: 3,
      workspace: "/repo"
    }),
    rich_nearly_exhausted: buildIterationUserMessage(rich, { currentDate: CURRENT_DATE, currentTask, runBudget: { total: 10, used: 9 } }),
    rich_no_task_role_no_desc: buildIterationUserMessage(rich, {
      currentDate: CURRENT_DATE,
      currentTask: undefined,
      loopInfo: { index: 1, maxCycles: 2, role: { name: "planner" } }
    })
  },
  buildIterationMessages: {
    empty: buildIterationMessages(empty, { currentDate: CURRENT_DATE, currentTask: undefined, systemPrompt: "SYS" }),
    rich_context_images: buildIterationMessages(rich, {
      currentDate: CURRENT_DATE,
      currentTask,
      goalContext: "context line one\ncontext line two",
      goalImages: ["data:image/png;base64,AAAA", "data:image/png;base64,BBBB"],
      loopInfo: { index: 3, maxCycles: 4 },
      repoMemoryDir: "/repo/.lci/memory",
      repoMemoryIndex: "- parser.md",
      runBudget: { total: 10, used: 7 },
      stallLimit: 3,
      systemPrompt: "SYS",
      workspace: "/repo"
    })
  },
  buildCycleContinuationMessage: {
    first: buildCycleContinuationMessage(rich, { currentTask, cycle: 2, maxCycles: 4 }),
    last_budget: buildCycleContinuationMessage(rich, { currentTask, cycle: 4, maxCycles: 4, runBudget: { total: 10, used: 9 } }),
    no_task: buildCycleContinuationMessage(empty, { currentTask: undefined, cycle: 2, maxCycles: 3, runBudget: { total: 5, used: 2 } })
  },
  buildRunSummaryMessages: Object.fromEntries(
    (["aborted", "completed", "error", "futile", "max-iterations", "partial", "planned"] as const).map((reason) => [
      reason,
      buildRunSummaryMessages(rich, { currentDate: CURRENT_DATE, reason, workspaceChanges: reason === "completed" ? " M src/a.rs\n?? new.rs" : reason === "partial" ? null : undefined })
    ])
  ),
  buildRunSummaryMessages_empty: buildRunSummaryMessages(empty, { currentDate: CURRENT_DATE, reason: "completed" }),
  buildFallbackRunSummary: Object.fromEntries(
    (["aborted", "completed", "error", "futile", "max-iterations", "partial", "planned"] as const).flatMap((reason) => [
      [`rich:${reason}`, buildFallbackRunSummary(rich, reason)],
      [`empty:${reason}`, buildFallbackRunSummary(empty, reason)]
    ])
  )
};

console.log(JSON.stringify(out, null, 2));
