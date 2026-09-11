// Center column: the session transcript in file order. Event entries are
// grouped under "Iteration N" headers; every other entry type renders as its
// own block. Sticks to the bottom while the reader is already there.
import { useEffect, useRef } from "react";
import type { TranscriptEntry } from "../api";

interface Props {
  entries: TranscriptEntry[];
}

type EventData = {
  promptTokens?: number;
  completionTokens?: number;
  cacheReadTokens?: number;
  cacheCreationTokens?: number;
  model?: string;
  provider?: string;
  durationMs?: number;
  latencyMs?: number;
  loop?: number;
  taskId?: string;
  toolName?: string;
  failed?: boolean;
  waitSeconds?: number;
};

type Section =
  | { kind: "entry"; entry: TranscriptEntry; key: number }
  | { kind: "iteration"; iteration: number; loop: number | null; items: TranscriptEntry[]; key: number };

/**
 * Entry types that mark a run boundary and therefore close the open
 * iteration section: a new goal, the run's end, and a run-level error (the
 * harness ends the run on it, run-end reason `error`).
 */
export const RUN_BOUNDARY = new Set(["goal", "run-end", "error"]);

/**
 * Split the transcript into blocks, keeping file order. An event opens a new
 * iteration section when its iteration is later than the open one; an event
 * carrying an earlier or equal iteration (a replayed tail, an out-of-order
 * write) stays in the open section rather than reopening a duplicate
 * header. Info, model and skill entries logged mid-iteration stay inside
 * the section, while goal, run-end and error entries close it — so the
 * next run's iteration 1 starts a fresh section.
 */
export function sectionsOf(entries: TranscriptEntry[]): Section[] {
  const sections: Section[] = [];
  let open: Extract<Section, { kind: "iteration" }> | null = null;
  entries.forEach((entry, index) => {
    if (entry.type !== "event") {
      if (open && !RUN_BOUNDARY.has(entry.type)) {
        open.items.push(entry);
        return;
      }
      open = null;
      sections.push({ kind: "entry", entry, key: index });
      return;
    }
    const iteration = typeof entry["iteration"] === "number" ? (entry["iteration"] as number) : 0;
    const data = (entry["data"] ?? {}) as EventData;
    if (open && iteration <= open.iteration) {
      open.items.push(entry);
      if (typeof data.loop === "number") open.loop = data.loop;
      return;
    }
    open = { kind: "iteration", iteration, loop: typeof data.loop === "number" ? data.loop : null, items: [entry], key: index };
    sections.push(open);
  });
  return sections;
}

export function fmtTokens(value: number | undefined): string {
  if (!value) return "0";
  if (value >= 1_000_000) return `${(value / 1_000_000).toFixed(1)}M`;
  if (value >= 1_000) return `${(value / 1_000).toFixed(1)}k`;
  return String(value);
}

function timeOf(entry: TranscriptEntry): string {
  const at = entry.at;
  if (typeof at !== "string") return "";
  const parsed = new Date(at);
  return Number.isNaN(parsed.getTime()) ? "" : parsed.toLocaleTimeString();
}

function Detail({ text }: { text: unknown }) {
  if (typeof text !== "string" || text.trim() === "") return null;
  return (
    <details className="mt-1">
      <summary className="cursor-pointer text-[11px] text-neutral-500">detail</summary>
      <pre className="mt-1 max-h-72 overflow-auto whitespace-pre-wrap rounded bg-neutral-900 p-2 text-xs text-neutral-300">{text}</pre>
    </details>
  );
}

function EventLine({ entry }: { entry: TranscriptEntry }) {
  const kind = String(entry["kind"] ?? "event");
  const data = (entry["data"] ?? {}) as EventData;
  const detail = entry["detail"];
  const stamp = <span className="w-16 shrink-0 font-mono text-[10px] text-neutral-600">{timeOf(entry)}</span>;

  switch (kind) {
    case "model-text":
      return (
        <div className="flex gap-2 py-1">
          {stamp}
          <div className="min-w-0 whitespace-pre-wrap text-sm text-neutral-200">{typeof detail === "string" ? detail : ""}</div>
        </div>
      );
    case "tool-call":
      return (
        <div className="flex gap-2 py-0.5">
          {stamp}
          <div className="min-w-0 flex-1 text-xs">
            <span className="font-mono text-sky-300">▸ {data.toolName ?? "tool"}</span>
            {data.taskId && <span className="ml-2 text-neutral-500">{data.taskId}</span>}
            <Detail text={detail} />
          </div>
        </div>
      );
    case "tool-result":
      return (
        <div className="flex gap-2 py-0.5">
          {stamp}
          <div className="min-w-0 flex-1 text-xs">
            <span className={`font-mono ${data.failed ? "text-red-400" : "text-neutral-400"}`}>
              ◂ {data.toolName ?? "tool"} {data.failed ? "failed" : "ok"}
            </span>
            {typeof data.durationMs === "number" && <span className="ml-2 text-neutral-600">{data.durationMs}ms</span>}
            <Detail text={detail} />
          </div>
        </div>
      );
    case "inference":
      return (
        <div className="flex gap-2 py-0.5">
          {stamp}
          <div className="text-[11px] text-neutral-500">
            inference · {data.model ?? "model"} · prompt {fmtTokens(data.promptTokens)} · completion {fmtTokens(data.completionTokens)}
            {typeof data.cacheReadTokens === "number" && data.cacheReadTokens > 0 && <> · cache read {fmtTokens(data.cacheReadTokens)}</>}
            {typeof data.cacheCreationTokens === "number" && data.cacheCreationTokens > 0 && <> · cache write {fmtTokens(data.cacheCreationTokens)}</>}
            {typeof data.latencyMs === "number" && <> · {(data.latencyMs / 1000).toFixed(1)}s</>}
          </div>
        </div>
      );
    case "task-finished":
      return (
        <div className="flex gap-2 py-0.5">
          {stamp}
          <div className="text-xs text-emerald-300">✓ {data.taskId ?? "task"} {typeof detail === "string" ? detail : ""}</div>
        </div>
      );
    case "question":
      return (
        <div className="flex gap-2 py-1">
          {stamp}
          <div className="rounded border border-amber-700/60 bg-amber-950/40 px-2 py-1 text-sm text-amber-200">
            <div className="text-[10px] uppercase tracking-wide text-amber-500">question for the operator</div>
            {typeof detail === "string" ? detail : ""}
          </div>
        </div>
      );
    case "operator-message":
      return (
        <div className="flex gap-2 py-1">
          {stamp}
          <div className="rounded border border-sky-800/60 bg-sky-950/40 px-2 py-1 text-sm text-sky-100">
            <div className="text-[10px] uppercase tracking-wide text-sky-500">operator</div>
            {typeof detail === "string" ? detail : ""}
          </div>
        </div>
      );
    case "rate-limited":
      return (
        <div className="flex gap-2 py-0.5">
          {stamp}
          <div className="text-xs text-orange-300">rate limited{typeof data.waitSeconds === "number" ? ` · waiting ${data.waitSeconds}s` : ""}</div>
        </div>
      );
    default:
      return (
        <div className="flex gap-2 py-0.5">
          {stamp}
          <div className="min-w-0 flex-1 text-xs text-neutral-500">
            {kind}
            {typeof detail === "string" && detail.length <= 160 ? <span className="ml-2 text-neutral-400">{detail}</span> : <Detail text={detail} />}
          </div>
        </div>
      );
  }
}

function EntryBlock({ entry }: { entry: TranscriptEntry }) {
  const text = typeof entry["text"] === "string" ? (entry["text"] as string) : "";
  switch (entry.type) {
    case "goal":
      return (
        <div className="my-3 ml-16 rounded-lg border border-emerald-800/60 bg-emerald-950/40 px-3 py-2">
          <div className="text-[10px] uppercase tracking-wide text-emerald-500">goal</div>
          <div className="whitespace-pre-wrap text-sm text-emerald-50">{text}</div>
        </div>
      );
    case "model":
      return (
        <div className="my-1 ml-16 text-[11px] text-neutral-500">
          model {String(entry["model"] ?? "")} · {String(entry["provider"] ?? "")}
          {entry["profileId"] ? <> · profile {String(entry["profileId"])}</> : null}
        </div>
      );
    case "run-end":
      return (
        <div className="my-2 ml-16 rounded border border-neutral-700 bg-neutral-900 px-3 py-1.5 text-xs text-neutral-300">
          run ended · {String(entry["reason"] ?? "")} · {String(entry["iterations"] ?? "?")} iterations
        </div>
      );
    case "error":
      return <div className="my-1 ml-16 whitespace-pre-wrap text-sm text-red-300">{text}</div>;
    case "skill":
      return (
        <div className="my-1 ml-16 text-[11px] text-neutral-500">
          skill {String(entry["name"] ?? "")} {entry["enabled"] ? "enabled" : "disabled"}
        </div>
      );
    default:
      return <div className="my-1 ml-16 whitespace-pre-wrap text-xs text-neutral-400">{text}</div>;
  }
}

export function Timeline({ entries }: Props) {
  const container = useRef<HTMLDivElement>(null);
  const content = useRef<HTMLDivElement>(null);
  const stick = useRef(true);
  const sections = sectionsOf(entries);

  // Follow the tail on any content growth — new pages, expanded details,
  // fonts settling — not just when the entry count changes.
  useEffect(() => {
    const scroller = container.current;
    const inner = content.current;
    if (!scroller || !inner) return;
    const follow = () => {
      if (stick.current) scroller.scrollTop = scroller.scrollHeight;
    };
    follow();
    const observer = new ResizeObserver(follow);
    observer.observe(inner);
    return () => observer.disconnect();
  }, []);

  return (
    <div
      ref={container}
      onScroll={(event) => {
        const node = event.currentTarget;
        stick.current = node.scrollHeight - node.scrollTop - node.clientHeight < 40;
      }}
      className="min-h-0 flex-1 overflow-y-auto px-4 py-3"
    >
      <div ref={content}>
        {entries.length === 0 && <p className="text-sm text-neutral-600">No transcript yet.</p>}
        {sections.map((section) =>
          section.kind === "entry" ? (
            <EntryBlock key={section.key} entry={section.entry} />
          ) : (
            <section key={section.key} className="my-2">
              <h3 className="sticky top-0 -mx-4 bg-neutral-950/95 px-4 py-1 text-[11px] font-semibold uppercase tracking-wide text-neutral-500">
                {section.iteration === 0 ? "Run" : `Iteration ${section.iteration}`}
                {section.loop !== null && <span className="ml-2 font-normal normal-case text-neutral-600">loop {section.loop}</span>}
                <span className="ml-2 font-normal normal-case text-neutral-600">{section.items.length} entries</span>
              </h3>
              {section.items.map((item, index) =>
                item.type === "event" ? (
                  <EventLine key={`${section.key}-${index}`} entry={item} />
                ) : (
                  <EntryBlock key={`${section.key}-${index}`} entry={item} />
                ),
              )}
            </section>
          ),
        )}
      </div>
    </div>
  );
}
