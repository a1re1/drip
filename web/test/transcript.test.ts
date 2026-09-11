// Independently constructed fixtures shaped per src/cli/transcript.rs
// (TranscriptEntry: internally tagged, camelCase) and src/core/types.rs
// (HarnessEventData / HarnessEventType). These are NOT copies of Rust values.
import { describe, expect, test } from "bun:test";
import {
  groupByIteration,
  latestPromptTokens,
  relativeTime,
  sumUsage,
  type TranscriptEntry,
} from "../lib/transcript";

function event(kind: string, data?: Record<string, unknown>, iteration = 1): TranscriptEntry {
  return {
    type: "event",
    at: "2026-09-10T12:00:00.000Z",
    goalId: "g1",
    detail: `${kind} happened`,
    iteration,
    kind,
    ...(data ? { data } : {}),
  };
}

describe("groupByIteration", () => {
  test("groups event entries by iteration and keeps non-events in iteration 0", () => {
    const a = event("tool-call", { toolName: "READ" }, 1);
    const b = event("tool-result", { toolName: "READ" }, 1);
    const c = event("model-text", undefined, 2);
    const goal: TranscriptEntry = { type: "goal", at: "t", text: "do it", goalId: "g1" };
    const groups = groupByIteration([a, goal, b, c]);
    expect(groups.size).toBe(3);
    expect(groups.get(1)).toEqual([a, b]);
    expect(groups.get(2)).toEqual([c]);
    expect(groups.get(0)).toEqual([goal]);
  });

  test("missing iteration folds into 0", () => {
    const e: TranscriptEntry = { type: "event", kind: "run-complete", detail: "done" };
    expect(groupByIteration([e]).get(0)).toEqual([e]);
  });
});

describe("sumUsage", () => {
  test("sums inference events only and ignores other kinds", () => {
    const entries: TranscriptEntry[] = [
      event("inference", { promptTokens: 100, completionTokens: 20, cacheReadTokens: 30, cacheCreationTokens: 5 }, 1),
      event("inference", { promptTokens: 50, completionTokens: 10 }, 2),
      event("tool-call", { promptTokens: 999 }),
    ];
    expect(sumUsage(entries)).toEqual({
      promptTokens: 150,
      completionTokens: 30,
      cacheReadTokens: 30,
      cacheCreationTokens: 5,
    });
  });

  test("returns zeros for empty or data-less input", () => {
    expect(sumUsage([])).toEqual({
      promptTokens: 0, completionTokens: 0, cacheReadTokens: 0, cacheCreationTokens: 0,
    });
    expect(sumUsage([event("inference")])).toEqual(sumUsage([]));
  });
});

describe("latestPromptTokens", () => {
  test("returns the last inference promptTokens, scanning newest first", () => {
    const entries: TranscriptEntry[] = [
      event("inference", { promptTokens: 100 }, 1),
      event("inference", { promptTokens: 250 }, 2),
      event("tool-call", { promptTokens: 999 }, 2),
    ];
    expect(latestPromptTokens(entries)).toBe(250);
  });

  test("returns null when there are no inference events", () => {
    expect(latestPromptTokens([event("tool-call", { promptTokens: 5 })])).toBeNull();
    expect(latestPromptTokens([])).toBeNull();
  });
});

describe("relativeTime", () => {
  const now = Date.parse("2026-09-10T12:00:00.000Z");
  test("buckets seconds/minutes/hours/days", () => {
    expect(relativeTime("2026-09-10T11:59:55.000Z", now)).toBe("just now");
    expect(relativeTime("2026-09-10T11:59:30.000Z", now)).toBe("30s ago");
    expect(relativeTime("2026-09-10T11:30:00.000Z", now)).toBe("30m ago");
    expect(relativeTime("2026-09-10T09:00:00.000Z", now)).toBe("3h ago");
    expect(relativeTime("2026-09-08T12:00:00.000Z", now)).toBe("2d ago");
  });
  test("future timestamps clamp to just now; bad input echoes the string", () => {
    expect(relativeTime("2026-09-10T12:05:00.000Z", now)).toBe("just now");
    expect(relativeTime("not-a-time", now)).toBe("not-a-time");
  });
});
