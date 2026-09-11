// Right column: what the harness is doing right now — iteration/loop
// counters, context pressure, token totals, and the task ledger from
// state.json.
import { latestPromptTokens, sumUsage } from "../../lib/transcript";
import type { SessionRow, TranscriptEntry } from "../api";
import { fmtTokens } from "./timeline";

interface Props {
  row: SessionRow | null;
  state: Record<string, unknown> | null;
  entries: TranscriptEntry[];
  maxContextTokens: number | null;
}

interface LedgerTask {
  id?: string;
  title?: string;
  description?: string;
  status?: string;
}

const STATUS_STYLE: Record<string, string> = {
  completed: "text-emerald-400",
  in_progress: "text-sky-300",
  blocked: "text-amber-300",
  dropped: "text-neutral-600 line-through",
  pending: "text-neutral-400",
};

function Stat({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex justify-between text-xs">
      <span className="text-neutral-500">{label}</span>
      <span className="font-mono text-neutral-200">{value}</span>
    </div>
  );
}

export function DetailPanel({ row, state, entries, maxContextTokens }: Props) {
  const usage = sumUsage(entries);
  const prompt = latestPromptTokens(entries);
  const contextPct = prompt !== null && maxContextTokens ? Math.min(100, Math.round((prompt / maxContextTokens) * 100)) : null;
  const tasks = Array.isArray(state?.["tasks"]) ? (state?.["tasks"] as LedgerTask[]) : [];
  const counts = tasks.reduce<Record<string, number>>((acc, task) => {
    const status = task.status ?? "pending";
    acc[status] = (acc[status] ?? 0) + 1;
    return acc;
  }, {});

  return (
    <aside className="flex w-80 shrink-0 flex-col overflow-y-auto bg-neutral-950 p-3 text-sm">
      <h2 className="mb-2 text-[11px] font-semibold uppercase tracking-wide text-neutral-500">Run</h2>
      <div className="space-y-1">
        <Stat label="status" value={row ? (row.running ? "running" : row.status) : "—"} />
        <Stat label="iteration" value={String(state?.["iteration"] ?? "—")} />
        <Stat label="loop" value={String(state?.["loop"] ?? "—")} />
        {row?.lastRun && <Stat label="last run" value={row.lastRun.reason} />}
      </div>

      {contextPct !== null && (
        <>
          <h2 className="mt-4 mb-2 text-[11px] font-semibold uppercase tracking-wide text-neutral-500">Context</h2>
          <div className="h-2 w-full overflow-hidden rounded bg-neutral-800">
            <div
              className={`h-full ${contextPct > 85 ? "bg-red-500" : contextPct > 60 ? "bg-amber-400" : "bg-emerald-500"}`}
              style={{ width: `${contextPct}%` }}
            />
          </div>
          <div className="mt-1 text-xs text-neutral-500">
            {fmtTokens(prompt ?? 0)} / {fmtTokens(maxContextTokens ?? 0)} tokens ({contextPct}%)
          </div>
        </>
      )}

      <h2 className="mt-4 mb-2 text-[11px] font-semibold uppercase tracking-wide text-neutral-500">Usage</h2>
      <div className="space-y-1">
        <Stat label="prompt" value={fmtTokens(usage.promptTokens)} />
        <Stat label="completion" value={fmtTokens(usage.completionTokens)} />
        <Stat label="cache read" value={fmtTokens(usage.cacheReadTokens)} />
        <Stat label="cache write" value={fmtTokens(usage.cacheCreationTokens)} />
      </div>

      <h2 className="mt-4 mb-2 text-[11px] font-semibold uppercase tracking-wide text-neutral-500">
        Tasks
        {tasks.length > 0 && (
          <span className="ml-2 font-normal normal-case text-neutral-600">
            {Object.entries(counts)
              .map(([status, count]) => `${count} ${status.replace("_", " ")}`)
              .join(" · ")}
          </span>
        )}
      </h2>
      {tasks.length === 0 ? (
        <p className="text-xs text-neutral-600">No task ledger yet.</p>
      ) : (
        <ol className="space-y-1">
          {tasks.map((task, index) => (
            <li key={task.id ?? index} className="flex gap-2 text-xs">
              <span className={`w-20 shrink-0 font-mono ${STATUS_STYLE[task.status ?? "pending"] ?? "text-neutral-400"}`}>
                {task.status ?? "pending"}
              </span>
              <span className="min-w-0 text-neutral-300">
                <span className="mr-1 font-mono text-neutral-600">{task.id}</span>
                {task.title ?? task.description ?? ""}
              </span>
            </li>
          ))}
        </ol>
      )}
    </aside>
  );
}
