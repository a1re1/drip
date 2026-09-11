// Pure helpers over drip transcript entries. Wire shape mirrors
// src/cli/transcript.rs: internally tagged with `type` ("goal", "event",
// "model", "run-end", "skill", "info", "error"), camelCase fields, and
// events carry `kind` (src/core/types.rs HarnessEventType) plus optional
// `data` (HarnessEventData) with promptTokens/completionTokens/
// cacheReadTokens/cacheCreationTokens/loop/failed/toolName.

export interface TranscriptEntry {
  type: string;
  at?: string;
  [key: string]: unknown;
}

export interface InferenceTokens {
  promptTokens: number;
  completionTokens: number;
  cacheReadTokens: number;
  cacheCreationTokens: number;
}

const EMPTY_TOKENS: InferenceTokens = {
  promptTokens: 0,
  completionTokens: 0,
  cacheReadTokens: 0,
  cacheCreationTokens: 0,
};

function asNumber(v: unknown): number {
  return typeof v === "number" && Number.isFinite(v) ? v : 0;
}

/** Group event entries by their `iteration` field (missing iteration → 0). */
export function groupByIteration(
  entries: TranscriptEntry[],
): Map<number, TranscriptEntry[]> {
  const groups = new Map<number, TranscriptEntry[]>();
  for (const entry of entries) {
    const iteration = asNumber(entry["iteration"]);
    const list = groups.get(iteration);
    if (list) list.push(entry);
    else groups.set(iteration, [entry]);
  }
  return groups;
}

/** Sum the token fields of every `inference` event's data. */
export function sumUsage(entries: TranscriptEntry[]): InferenceTokens {
  const totals: InferenceTokens = { ...EMPTY_TOKENS };
  for (const entry of entries) {
    if (entry["kind"] !== "inference") continue;
    const data = entry["data"];
    if (typeof data !== "object" || data === null) continue;
    const d = data as Record<string, unknown>;
    totals.promptTokens += asNumber(d["promptTokens"]);
    totals.completionTokens += asNumber(d["completionTokens"]);
    totals.cacheReadTokens += asNumber(d["cacheReadTokens"]);
    totals.cacheCreationTokens += asNumber(d["cacheCreationTokens"]);
  }
  return totals;
}

/** promptTokens of the most recent `inference` event, or null when none. */
export function latestPromptTokens(entries: TranscriptEntry[]): number | null {
  for (let i = entries.length - 1; i >= 0; i--) {
    const entry = entries[i];
    if (!entry) continue;
    if (entry["kind"] !== "inference") continue;
    const data = entry["data"];
    if (typeof data === "object" && data !== null) {
      const v = (data as Record<string, unknown>)["promptTokens"];
      if (typeof v === "number" && Number.isFinite(v)) return v;
    }
  }
  return null;
}

/**
 * Human "3m ago" style relative time between an ISO timestamp and now.
 * Returns the ISO string itself for unparseable input, "just now" under 10s.
 */
export function relativeTime(iso: string, now: number): string {
  const then = Date.parse(iso);
  if (Number.isNaN(then)) return iso;
  const seconds = Math.max(0, Math.round((now - then) / 1000));
  if (seconds < 10) return "just now";
  if (seconds < 60) return `${seconds}s ago`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  return `${days}d ago`;
}
