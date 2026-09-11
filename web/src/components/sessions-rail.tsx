// Left rail: every session under the launch directory, split into running
// (live lease) and recent, plus the "new session" composer that starts a
// detached drip run.
import { useState } from "react";
import { relativeTime } from "../../lib/transcript";
import type { SessionLists, SessionRow } from "../api";

interface Props {
  lists: SessionLists;
  selectedId: string | null;
  onSelect: (id: string) => void;
  /** Resolves true when the run started; the draft is kept otherwise. */
  onRun: (goal: string, maxIterations?: number) => Promise<boolean>;
  busy: boolean;
}

function taskSummary(row: SessionRow): string | null {
  const stats = row.lastRun?.taskStats as
    | { completed?: number; pending?: number; blocked?: number; dropped?: number }
    | null
    | undefined;
  if (!stats) return null;
  const total = (stats.completed ?? 0) + (stats.pending ?? 0) + (stats.blocked ?? 0) + (stats.dropped ?? 0);
  return total > 0 ? `${stats.completed ?? 0}/${total} tasks` : null;
}

function Row({ row, selected, onSelect, now }: { row: SessionRow; selected: boolean; onSelect: () => void; now: number }) {
  const tasks = taskSummary(row);
  return (
    <button
      type="button"
      onClick={onSelect}
      className={`block w-full rounded-md px-2 py-1.5 text-left transition ${
        selected ? "bg-neutral-800 text-neutral-100" : "hover:bg-neutral-900 text-neutral-300"
      }`}
    >
      <div className="flex items-center gap-2 text-xs">
        <span className={`h-1.5 w-1.5 rounded-full ${row.running ? "bg-emerald-400" : "bg-neutral-600"}`} />
        <span className="font-mono text-neutral-400">{row.id.slice(0, 8)}</span>
        <span className="ml-auto text-neutral-500">{relativeTime(row.updatedAt, now)}</span>
      </div>
      <div className="mt-0.5 truncate text-sm">{row.lastGoal ?? "(no goal yet)"}</div>
      <div className="mt-0.5 flex gap-2 text-[11px] text-neutral-500">
        {row.running && row.pid !== undefined && <span>pid {row.pid}</span>}
        {row.lastRun?.reason && <span>{row.lastRun.reason}</span>}
        {tasks && <span>{tasks}</span>}
        {row.goalCount > 1 && <span>{row.goalCount} goals</span>}
      </div>
    </button>
  );
}

function Section({ title, rows, selectedId, onSelect, now, empty }: {
  title: string;
  rows: SessionRow[];
  selectedId: string | null;
  onSelect: (id: string) => void;
  now: number;
  empty: string;
}) {
  return (
    <section className="mb-3">
      <h2 className="mb-1 px-2 text-[11px] font-semibold uppercase tracking-wide text-neutral-500">
        {title} <span className="text-neutral-600">{rows.length}</span>
      </h2>
      {rows.length === 0 ? (
        <p className="px-2 text-xs text-neutral-600">{empty}</p>
      ) : (
        <div className="space-y-0.5">
          {rows.map((row) => (
            <Row key={row.id} row={row} selected={row.id === selectedId} onSelect={() => onSelect(row.id)} now={now} />
          ))}
        </div>
      )}
    </section>
  );
}

export function SessionsRail({ lists, selectedId, onSelect, onRun, busy }: Props) {
  const [goal, setGoal] = useState("");
  const [maxIterations, setMaxIterations] = useState("");
  const now = Date.now();

  const submit = async () => {
    const text = goal.trim();
    if (!text || busy) return;
    const limit = Number.parseInt(maxIterations, 10);
    if (await onRun(text, Number.isInteger(limit) && limit > 0 ? limit : undefined)) setGoal("");
  };

  return (
    <aside className="flex w-72 shrink-0 flex-col bg-neutral-950">
      <div className="border-b border-neutral-800 px-3 py-2 text-sm font-semibold text-neutral-100">drip sessions</div>
      <div className="min-h-0 flex-1 overflow-y-auto p-2">
        <Section title="Running" rows={lists.running} selectedId={selectedId} onSelect={onSelect} now={now} empty="No live agents." />
        <Section title="Recent" rows={lists.recent} selectedId={selectedId} onSelect={onSelect} now={now} empty="No sessions under this directory yet." />
      </div>
      <form
        className="border-t border-neutral-800 p-2"
        onSubmit={(event) => {
          event.preventDefault();
          void submit();
        }}
      >
        <textarea
          value={goal}
          onChange={(event) => setGoal(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === "Enter" && !event.shiftKey && !event.nativeEvent.isComposing) {
              event.preventDefault();
              void submit();
            }
          }}
          placeholder="New session: describe the goal…"
          rows={3}
          className="w-full resize-none rounded-md border border-neutral-800 bg-neutral-900 px-2 py-1.5 text-sm text-neutral-100 outline-none placeholder:text-neutral-600 focus:border-neutral-600"
        />
        <div className="mt-1.5 flex items-center gap-2">
          <input
            value={maxIterations}
            onChange={(event) => setMaxIterations(event.target.value.replace(/[^0-9]/g, ""))}
            placeholder="max iters"
            className="w-24 rounded-md border border-neutral-800 bg-neutral-900 px-2 py-1 text-xs text-neutral-200 outline-none placeholder:text-neutral-600"
          />
          <button
            type="submit"
            disabled={busy || goal.trim() === ""}
            className="ml-auto rounded-md bg-emerald-600 px-3 py-1 text-xs font-medium text-white disabled:opacity-40"
          >
            Start
          </button>
        </div>
      </form>
    </aside>
  );
}
