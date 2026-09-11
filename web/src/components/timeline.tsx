// Center column: the session transcript as a message stream. Operator goals
// are bubbles on the right, model prose is plain text on the left, and the
// tool/inference events of one iteration fold into a single glass card of
// expandable rows. A tick rail on the left peeks and jumps between turns.
// Sticks to the bottom while the reader is already there.
import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import type { TranscriptEntry } from "../api";
import { Tag } from "./icons";

interface Props {
  entries: TranscriptEntry[];
  isRunning: boolean;
  /** Rendered under the stream, right-aligned (poll health). */
  footer?: ReactNode;
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

function dateOf(entry: TranscriptEntry): Date | null {
  const at = entry.at;
  if (typeof at !== "string") return null;
  const parsed = new Date(at);
  return Number.isNaN(parsed.getTime()) ? null : parsed;
}

function clockOf(entry: TranscriptEntry, seconds: boolean): string {
  const date = dateOf(entry);
  if (!date) return "";
  return date.toLocaleTimeString([], seconds ? { hour: "2-digit", minute: "2-digit", second: "2-digit", hour12: false } : { hour: "numeric", minute: "2-digit" });
}

function dayOf(entry: TranscriptEntry): string {
  const date = dateOf(entry);
  if (!date) return "";
  const today = new Date();
  const sameDay = date.toDateString() === today.toDateString();
  const clock = clockOf(entry, false);
  return sameDay ? `Today ${clock}` : `${date.toLocaleDateString([], { month: "short", day: "numeric" })} ${clock}`;
}

const str = (value: unknown): string => (typeof value === "string" ? value : "");
const firstLine = (value: string): string => value.split("\n", 1)[0] ?? "";
const clip = (value: string, max: number): string => (value.length > max ? `${value.slice(0, max - 1)}…` : value);

interface Row {
  key: string;
  time: string;
  tag: string;
  color: string;
  text: string;
  meta: string;
  detail: string;
}

const KIND_TAG: Record<string, string> = { "iteration-start": "start", "context-expired": "context", "task-finished": "done", "rate-limited": "wait" };

function colorForKind(kind: string): string {
  if (kind.startsWith("context")) return "var(--purple)";
  if (kind.includes("memory") || kind.startsWith("remember")) return "var(--orange)";
  if (kind.startsWith("task")) return "var(--green)";
  return "var(--text-tertiary)";
}

/**
 * The one clipping rule for card rows: up to 200 characters of the first
 * line stay inline; a longer or multiline payload moves whole into the
 * expandable detail. `prefix` (a task id) and `fallback` (the event kind)
 * keep the row readable when the payload is short or empty.
 */
function splitRow(payload: string, prefix = "", fallback = ""): { text: string; detail: string } {
  const expandable = payload.length > 200 || payload.includes("\n");
  const text = [prefix, expandable ? clip(firstLine(payload), 200) : payload].filter(Boolean).join(" · ") || fallback;
  return { text, detail: expandable ? payload : "" };
}

function rowOf(entry: TranscriptEntry, key: string): Row {
  const time = clockOf(entry, true);
  if (entry.type !== "event") {
    let payload = str(entry["text"]);
    if (entry.type === "model") payload = [str(entry["model"]), str(entry["provider"])].filter(Boolean).join(" · ");
    if (entry.type === "skill") payload = `${str(entry["name"])} ${entry["enabled"] ? "enabled" : "disabled"}`;
    return { key, time, tag: entry.type, color: "var(--text-tertiary)", meta: "", ...splitRow(payload) };
  }
  const kind = str(entry["kind"]) || "event";
  const data = (entry["data"] ?? {}) as EventData;
  const detail = str(entry["detail"]);
  switch (kind) {
    case "tool-call":
      return { key, time, tag: data.toolName ?? "tool", color: "var(--teal)", meta: "", ...splitRow(detail, data.taskId ?? "") };
    case "tool-result":
      return {
        key,
        time,
        tag: data.failed ? "failed" : "ok",
        color: data.failed ? "var(--red)" : "var(--green)",
        text: `${data.toolName ?? "tool"} ${data.failed ? "failed" : "ok"}`,
        meta: typeof data.durationMs === "number" ? `${data.durationMs}ms` : "",
        detail,
      };
    case "inference": {
      const lines = [
        `model ${data.model ?? "?"}${data.provider ? ` via ${data.provider}` : ""}`,
        `prompt ${(data.promptTokens ?? 0).toLocaleString()} · completion ${(data.completionTokens ?? 0).toLocaleString()} · cache read ${(data.cacheReadTokens ?? 0).toLocaleString()} · cache write ${(data.cacheCreationTokens ?? 0).toLocaleString()}`,
      ];
      if (typeof data.latencyMs === "number") lines.push(`latency ${(data.latencyMs / 1000).toFixed(1)}s`);
      const meta = [`prompt ${fmtTokens(data.promptTokens)}`, `completion ${fmtTokens(data.completionTokens)}`];
      if (typeof data.latencyMs === "number") meta.push(`${(data.latencyMs / 1000).toFixed(1)}s`);
      return { key, time, tag: "infer", color: "var(--accent)", text: data.model ?? "model", meta: meta.join(" · "), detail: [...lines, detail].filter(Boolean).join("\n") };
    }
    case "rate-limited":
      return { key, time, tag: "wait", color: "var(--orange)", text: `rate limited${typeof data.waitSeconds === "number" ? ` · waiting ${data.waitSeconds}s` : ""}`, meta: "", detail };
    default:
      return {
        key,
        time,
        tag: KIND_TAG[kind] ?? kind.split("-")[0] ?? kind,
        color: colorForKind(kind),
        meta: "",
        ...splitRow(detail, data.taskId ?? "", kind),
      };
  }
}

interface Turn {
  id: number;
  label: string;
  time: string;
  title: string;
  excerpt: string;
}

type Block =
  | { kind: "caption"; key: string; text: string }
  | { kind: "user"; key: string; text: string; turn: Turn }
  | { kind: "agent"; key: string; text: string; turn: Turn }
  | { kind: "question"; key: string; text: string; turn: Turn }
  | { kind: "card"; key: string; rows: Row[]; turn: Turn }
  | { kind: "note"; key: string; text: string; tone: "error" | "muted" };

/**
 * Flatten sections into renderable blocks and number the turns the rail can
 * jump to. Turn ids are dense (0..n-1) in stream order, so the rail can index
 * its `turns` array by id; block keys stay stable as the transcript grows
 * because they derive from entry positions, not turn numbers.
 */
export function blocksOf(sections: Section[]): Block[] {
  const blocks: Block[] = [];
  let nextTurn = 0;
  const turn = (label: string, entry: TranscriptEntry, title: string, excerpt: string): Turn => ({
    id: nextTurn++,
    label,
    time: clockOf(entry, false),
    title: clip(firstLine(title), 120),
    excerpt: clip(excerpt, 240),
  });
  for (const section of sections) {
    if (section.kind === "entry") {
      const entry = section.entry;
      const key = `e${section.key}`;
      const text = str(entry["text"]);
      switch (entry.type) {
        case "goal":
          blocks.push({ kind: "caption", key: `${key}-t`, text: dayOf(entry) });
          blocks.push({ kind: "user", key, text, turn: turn("You", entry, text, "") });
          break;
        case "run-end":
          blocks.push({ kind: "caption", key, text: `Run ended · ${str(entry["reason"]) || "?"} · ${String(entry["iterations"] ?? "?")} iterations` });
          break;
        case "error":
          blocks.push({ kind: "note", key, text, tone: "error" });
          break;
        case "model":
          blocks.push({ kind: "caption", key, text: `${str(entry["model"])} · ${str(entry["provider"])}${entry["profileId"] ? ` · profile ${str(entry["profileId"])}` : ""}` });
          break;
        case "skill":
          blocks.push({ kind: "caption", key, text: `skill ${str(entry["name"])} ${entry["enabled"] ? "enabled" : "disabled"}` });
          break;
        default:
          blocks.push({ kind: "note", key, text, tone: "muted" });
      }
      continue;
    }
    const label = section.iteration === 0 ? "Run" : `Iteration ${section.iteration}`;
    const meta = [label, section.loop !== null ? `loop ${section.loop}` : "", `${section.items.length} entries`].filter(Boolean).join(" · ");
    blocks.push({ kind: "caption", key: `s${section.key}`, text: meta });
    let rows: Row[] = [];
    let first: TranscriptEntry | null = null;
    const flush = () => {
      if (rows.length === 0 || !first) return;
      const excerpt = rows
        .slice(0, 3)
        .map((row) => `${row.tag} ${row.text}`)
        .join(" · ");
      blocks.push({ kind: "card", key: `c${rows[0]?.key ?? section.key}`, rows, turn: turn(label, first, `${rows.length} entries`, excerpt) });
      rows = [];
      first = null;
    };
    section.items.forEach((item, index) => {
      const key = `${section.key}-${index}`;
      const kind = item.type === "event" ? str(item["kind"]) : "";
      const detail = str(item["detail"]);
      if (kind === "model-text") {
        flush();
        blocks.push({ kind: "agent", key, text: detail, turn: turn("Agent", item, detail, detail.slice(firstLine(detail).length).trim()) });
        return;
      }
      if (kind === "question") {
        flush();
        blocks.push({ kind: "question", key, text: detail, turn: turn("Question", item, detail, "") });
        return;
      }
      if (kind === "operator-message") {
        flush();
        blocks.push({ kind: "user", key, text: detail, turn: turn("You", item, detail, "") });
        return;
      }
      first ??= item;
      rows.push(rowOf(item, key));
    });
    flush();
  }
  return blocks;
}

function EventCard({ rows }: { rows: Row[] }) {
  const [open, setOpen] = useState<Record<string, boolean>>({});
  return (
    <div className="glass flex flex-col py-1" style={{ width: "min(100%, 720px)" }}>
      {rows.map((row) => {
        const expandable = row.detail !== "";
        const isOpen = expandable && !!open[row.key];
        return (
          <div key={row.key} className="flex flex-col">
            <div
              className={`event-row${expandable ? " clickable" : ""}`}
              onClick={expandable ? () => setOpen((prev) => ({ ...prev, [row.key]: !prev[row.key] })) : undefined}
            >
              <span className="mono t-caption c-tertiary shrink-0" style={{ width: 62 }}>
                {row.time}
              </span>
              <span className="flex shrink-0" style={{ width: 64 }}>
                <Tag color={row.color}>{row.tag}</Tag>
              </span>
              <span className="t-footnote min-w-0 flex-1 truncate">{row.text}</span>
              {row.meta && <span className="t-caption c-tertiary whitespace-nowrap">{row.meta}</span>}
            </div>
            {isOpen && (
              <pre
                className="inset mono t-caption c-secondary whitespace-pre-wrap"
                style={{ margin: "2px 12px 8px 84px", padding: "9px 11px", lineHeight: 1.55, maxHeight: 320, overflow: "auto" }}
              >
                {row.detail}
              </pre>
            )}
          </div>
        );
      })}
    </div>
  );
}

function BlockView({ block }: { block: Block }) {
  switch (block.kind) {
    case "caption":
      return (
        <div className="t-caption c-tertiary text-center" style={{ padding: "8px 0 4px" }}>
          {block.text}
        </div>
      );
    case "user":
      return (
        <div id={`turn-${block.turn.id}`} className="flex justify-end" style={{ paddingBottom: 10 }}>
          <div className="bubble-user">{block.text}</div>
        </div>
      );
    case "agent":
      return (
        <div id={`turn-${block.turn.id}`} className="agent-text">
          {block.text}
        </div>
      );
    case "question":
      return (
        <div id={`turn-${block.turn.id}`} className="flex justify-start" style={{ padding: "4px 0 10px" }}>
          <div
            className="glass flex flex-col gap-1"
            style={{ maxWidth: "min(88%, 720px)", padding: "9px 13px", borderColor: "color-mix(in oklab, var(--orange) 45%, transparent)" }}
          >
            <span className="section-label" style={{ color: "var(--orange)" }}>
              Question for the operator
            </span>
            <span className="t-callout whitespace-pre-wrap" style={{ overflowWrap: "anywhere" }}>
              {block.text}
            </span>
          </div>
        </div>
      );
    case "card":
      return (
        <div id={`turn-${block.turn.id}`} className="flex justify-start">
          <EventCard rows={block.rows} />
        </div>
      );
    case "note":
      return (
        <div
          className={`whitespace-pre-wrap ${block.tone === "error" ? "t-callout" : "t-footnote c-secondary"}`}
          style={{ padding: "4px 4px", overflowWrap: "anywhere", color: block.tone === "error" ? "var(--red)" : undefined }}
        >
          {block.text}
        </div>
      );
  }
}

function lastModel(entries: TranscriptEntry[]): string {
  for (let index = entries.length - 1; index >= 0; index--) {
    const entry = entries[index];
    if (!entry) continue;
    if (entry.type === "event" && str(entry["kind"]) === "inference") {
      const model = (entry["data"] as EventData | undefined)?.model;
      if (model) return model;
    }
    if (entry.type === "model" && str(entry["model"])) return str(entry["model"]);
  }
  return "";
}

const HEADER_CLEARANCE = 84;

export function Timeline({ entries, isRunning, footer }: Props) {
  const container = useRef<HTMLDivElement>(null);
  const content = useRef<HTMLDivElement>(null);
  const stick = useRef(true);
  const [current, setCurrent] = useState(0);
  const [peek, setPeek] = useState<number | null>(null);
  const rail = useRef<HTMLDivElement>(null);
  // Peek and scroll state re-render often; the block model only changes with the transcript.
  const blocks = useMemo(() => blocksOf(sectionsOf(entries)), [entries]);
  const turns = useMemo(() => blocks.flatMap((block) => ("turn" in block ? [block.turn] : [])), [blocks]);
  const model = lastModel(entries);

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

  const jump = (id: number) => {
    const scroller = container.current;
    const target = scroller?.querySelector<HTMLElement>(`#turn-${id}`);
    if (!scroller || !target) return;
    stick.current = false;
    scroller.scrollTo({ top: target.offsetTop - scroller.offsetTop - HEADER_CLEARANCE, behavior: "smooth" });
  };

  const peeked = peek !== null ? turns[peek] : undefined;

  // Long sessions have more ticks than the rail is tall: the list scrolls
  // (scrollbar hidden) and the active tick is kept in view. Turn ids are
  // dense, so the tick for a turn is the child at its id.
  useEffect(() => {
    const tick = rail.current?.children[current];
    if (tick instanceof HTMLElement) tick.scrollIntoView({ block: "nearest" });
  }, [current, turns.length]);

  return (
    <div className="relative flex min-h-0 flex-1">
      {turns.length > 0 && (
        <div className="absolute left-0 top-0 z-[4] flex items-center justify-center" style={{ bottom: 0, width: 36 }} onMouseLeave={() => setPeek(null)}>
          <div ref={rail} className="flex max-h-full flex-col items-start" style={{ gap: 5, padding: "10px 0", overflowY: "auto", scrollbarWidth: "none" }}>
            {turns.map((turn) => {
              const active = turn.id === current;
              const hot = turn.id === peek;
              return (
                <div
                  key={turn.id}
                  className="flex shrink-0 cursor-pointer items-center justify-center"
                  style={{ width: 36, height: 9 }}
                  onMouseEnter={() => setPeek(turn.id)}
                  onClick={() => jump(turn.id)}
                >
                  <span
                    className="block"
                    style={{
                      height: 2,
                      borderRadius: 1,
                      transition: "width var(--dur-fast) var(--ease-out), background var(--dur-fast) var(--ease-out)",
                      width: active ? 24 : hot ? 18 : 12,
                      background: active ? "var(--text-primary)" : hot ? "var(--text-secondary)" : "var(--text-quaternary)",
                    }}
                  />
                </div>
              );
            })}
          </div>
          {peeked && (
            <div className="popover pointer-events-none absolute flex flex-col gap-1" style={{ left: 38, top: "50%", transform: "translateY(-50%)", width: 300, padding: "11px 13px" }}>
              <div className="t-caption c-tertiary flex items-center gap-1.5">
                <span>{peeked.label}</span>
                <span className="ml-auto">{peeked.time}</span>
              </div>
              <div className="t-footnote font-semibold" style={{ lineHeight: 1.45 }}>
                {peeked.title || "(empty)"}
              </div>
              {peeked.excerpt && (
                <div className="t-footnote c-secondary" style={{ lineHeight: 1.45, display: "-webkit-box", WebkitLineClamp: 3, WebkitBoxOrient: "vertical", overflow: "hidden" }}>
                  {peeked.excerpt}
                </div>
              )}
            </div>
          )}
        </div>
      )}
      <div
        ref={container}
        onScroll={(event) => {
          const node = event.currentTarget;
          stick.current = node.scrollHeight - node.scrollTop - node.clientHeight < 40;
          const mid = node.scrollTop + node.clientHeight * 0.45;
          let active = 0;
          for (const target of node.querySelectorAll<HTMLElement>('[id^="turn-"]')) {
            if (target.offsetTop - node.offsetTop <= mid) active = Number(target.id.slice("turn-".length));
          }
          if (active !== current) setCurrent(active);
        }}
        className="flex min-h-0 flex-1 flex-col overflow-y-auto"
        style={{ padding: "78px 48px 10px" }}
      >
        <div ref={content} className="flex flex-col" style={{ margin: "auto auto 0", width: "min(100%, 760px)", gap: 6 }}>
          {entries.length === 0 && (
            <p className="t-footnote c-tertiary text-center" style={{ padding: "24px 0" }}>
              No transcript yet.
            </p>
          )}
          {blocks.map((block) => (
            <BlockView key={block.key} block={block} />
          ))}
          {isRunning && (
            <div className="flex justify-start" style={{ paddingTop: 4 }}>
              <div className="glass t-footnote c-secondary inline-flex items-center gap-2" style={{ padding: "7px 12px", boxShadow: "none" }}>
                <span className="pulse rounded-full" style={{ width: 6, height: 6, background: "var(--accent)" }} />
                Working{model ? ` · ${model}` : ""}
              </div>
            </div>
          )}
          {footer && (
            <div className="t-caption c-tertiary text-right" style={{ padding: "2px 4px" }}>
              {footer}
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
