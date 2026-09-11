// Transcript grouping for the timeline: fixtures shaped per
// src/cli/transcript.rs (internally tagged, camelCase) — not copies of Rust
// values. Rendering itself is not exercised; sectionsOf is the pure part.
import { describe, expect, test } from "bun:test";
import type { TranscriptEntry } from "../lib/transcript";
import { RUN_BOUNDARY, blocksOf, fmtTokens, sectionsOf } from "../src/components/timeline";

const event = (iteration: number, kind = "inference", data: Record<string, unknown> = {}): TranscriptEntry => ({
  type: "event",
  at: "2026-09-10T10:00:00.000Z",
  iteration,
  kind,
  data,
});

const shape = (entries: TranscriptEntry[]) =>
  sectionsOf(entries).map((section) =>
    section.kind === "entry" ? section.entry.type : `iteration ${section.iteration}×${section.items.length}${section.loop === null ? "" : ` loop ${section.loop}`}`,
  );

describe("sectionsOf", () => {
  test("groups consecutive events by iteration and keeps mid-iteration notes inside", () => {
    const entries: TranscriptEntry[] = [
      { type: "goal", goal: "ship it" },
      event(1, "inference"),
      { type: "info", text: "operator message queued: go" },
      event(1, "tool-call"),
      event(2, "inference", { loop: 1 }),
      { type: "run-end", reason: "completed", iterations: 2 },
    ];
    expect(shape(entries)).toEqual(["goal", "iteration 1×3", "iteration 2×1 loop 1", "run-end"]);
  });

  test("a run-level error closes the open iteration; the next run starts fresh at iteration 1", () => {
    const entries: TranscriptEntry[] = [
      event(1),
      event(2),
      { type: "error", text: "inference endpoint down" },
      { type: "goal", goal: "again" },
      event(1),
      event(2),
    ];
    expect(shape(entries)).toEqual(["iteration 1×1", "iteration 2×1", "error", "goal", "iteration 1×1", "iteration 2×1"]);
    expect(RUN_BOUNDARY.has("error")).toBe(true);
  });

  test("an earlier or repeated iteration inside a run never reopens a duplicate header", () => {
    const entries: TranscriptEntry[] = [event(1), event(2), event(1), event(2), event(3)];
    expect(shape(entries)).toEqual(["iteration 1×1", "iteration 2×3", "iteration 3×1"]);
  });

  test("section keys are the file positions, stable across re-renders", () => {
    const entries: TranscriptEntry[] = [{ type: "goal" }, event(1), event(1), event(2)];
    expect(sectionsOf(entries).map((section) => section.key)).toEqual([0, 1, 3]);
  });
});

describe("fmtTokens", () => {
  test("abbreviates thousands and millions, and treats missing as 0", () => {
    expect(fmtTokens(undefined)).toBe("0");
    expect(fmtTokens(999)).toBe("999");
    expect(fmtTokens(12_345)).toBe("12.3k");
    expect(fmtTokens(2_500_000)).toBe("2.5M");
  });
});

// blocksOf is the stream's render model: goals become bubbles, model prose
// breaks an iteration's event card, and turn ids are dense so the rail can
// index its turn list by id.
const turnsOf = (entries: TranscriptEntry[]) =>
  blocksOf(sectionsOf(entries)).flatMap((block) => ("turn" in block ? [`${block.turn.id}:${block.turn.label}`] : []));

describe("blocksOf", () => {
  const goal: TranscriptEntry = { type: "goal", at: "2026-09-10T10:00:00.000Z", text: "ship it" };

  test("a goal renders as a time caption followed by a user bubble", () => {
    const kinds = blocksOf(sectionsOf([goal])).map((block) => block.kind);
    expect(kinds).toEqual(["caption", "user"]);
  });

  test("model prose splits an iteration into two cards and numbers turns densely", () => {
    const entries: TranscriptEntry[] = [
      goal,
      event(1, "tool-call", { toolName: "BASH" }),
      event(1, "tool-result", { toolName: "BASH", durationMs: 5 }),
      { ...event(1, "model-text"), detail: "Looked around.\nMore detail." },
      event(1, "inference", { model: "m" }),
    ];
    const kinds = blocksOf(sectionsOf(entries)).map((block) => block.kind);
    expect(kinds).toEqual(["caption", "user", "caption", "card", "agent", "card"]);
    expect(turnsOf(entries)).toEqual(["0:You", "1:Iteration 1", "2:Agent", "3:Iteration 1"]);
  });

  test("block keys are stable as the transcript grows", () => {
    const first: TranscriptEntry[] = [goal, event(1, "tool-call", { toolName: "READ" })];
    const later = [...first, event(1, "tool-result", { toolName: "READ" }), event(2, "inference")];
    const keysOf = (entries: TranscriptEntry[]) => blocksOf(sectionsOf(entries)).map((block) => block.key);
    expect(keysOf(later).slice(0, keysOf(first).length)).toEqual(keysOf(first));
  });

  test("run-end and question entries render as caption and question blocks", () => {
    const entries: TranscriptEntry[] = [
      goal,
      { ...event(1, "question"), detail: "which branch?" },
      { type: "run-end", at: "2026-09-10T10:01:00.000Z", reason: "blocked", iterations: 1 },
    ];
    const kinds = blocksOf(sectionsOf(entries)).map((block) => block.kind);
    expect(kinds).toEqual(["caption", "user", "caption", "question", "caption"]);
  });
});
