// Left sidebar (toggled): every session under the launch directory, split
// into running (live lease) and recent, with a filter box and a shortcut
// into the new-session composer.
import { useState } from "react";
import { relativeTime } from "../../lib/transcript";
import type { SessionLists, SessionRow } from "../api";
import { Icon, IconButton } from "./icons";

interface Props {
  lists: SessionLists;
  selectedId: string | null;
  onSelect: (id: string) => void;
  onNew: () => void;
  onHide: () => void;
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
    <button type="button" onClick={onSelect} className="side-item flex flex-col" style={{ gap: 4 }} data-selected={selected}>
      <div className="flex items-center gap-2">
        <span
          className={`shrink-0 rounded-full${row.running ? " pulse" : ""}`}
          style={{ width: 7, height: 7, background: row.running ? "var(--green)" : "var(--text-quaternary)" }}
        />
        <span className="mono t-caption c-secondary">{row.id.slice(0, 8)}</span>
        <span className="t-caption c-tertiary ml-auto">{relativeTime(row.updatedAt, now)}</span>
      </div>
      <div className="side-title t-body truncate font-medium">{row.lastGoal ?? "(no goal yet)"}</div>
      <div className="t-caption c-secondary flex flex-wrap" style={{ gap: "4px 10px" }}>
        {row.running && row.pid !== undefined && <span>pid {row.pid}</span>}
        {row.lastRun?.reason && <span>{row.lastRun.reason}</span>}
        {tasks && <span>{tasks}</span>}
        {row.goalCount > 1 && <span>{row.goalCount} goals</span>}
      </div>
    </button>
  );
}

function Section({ title, rows, selectedId, onSelect, now, empty, accent }: {
  title: string;
  rows: SessionRow[];
  selectedId: string | null;
  onSelect: (id: string) => void;
  now: number;
  empty: string;
  accent: boolean;
}) {
  return (
    <div className="flex flex-col" style={{ gap: 6 }}>
      <div className="flex items-center gap-1.5" style={{ padding: "0 6px" }}>
        <span className="section-label">{title}</span>
        <span
          className="t-caption inline-flex items-center justify-center font-semibold"
          style={{
            minWidth: 18,
            height: 18,
            padding: "0 5px",
            borderRadius: "var(--radius-capsule)",
            background: accent && rows.length > 0 ? "var(--accent)" : "var(--text-tertiary)",
            color: "#fff",
          }}
        >
          {rows.length}
        </span>
      </div>
      {rows.length === 0 ? (
        <p className="t-footnote c-tertiary" style={{ padding: "6px 12px" }}>
          {empty}
        </p>
      ) : (
        rows.map((row) => <Row key={row.id} row={row} selected={row.id === selectedId} onSelect={() => onSelect(row.id)} now={now} />)
      )}
    </div>
  );
}

export function SessionsRail({ lists, selectedId, onSelect, onNew, onHide }: Props) {
  const [filter, setFilter] = useState("");
  const now = Date.now();
  const needle = filter.trim().toLowerCase();
  const matches = (row: SessionRow) => needle === "" || row.id.toLowerCase().includes(needle) || (row.lastGoal ?? "").toLowerCase().includes(needle);
  const running = lists.running.filter(matches);
  const recent = lists.recent.filter(matches);

  return (
    <aside className="sidebar-surface hairline-r flex min-h-0 shrink-0 flex-col" style={{ width: 250 }}>
      <div className="flex items-center" style={{ padding: "10px 10px 6px 14px", gap: 7 }}>
        <span className="t-body font-semibold">Sessions</span>
        <span className="ml-auto" />
        <IconButton icon="panel-left" label="Hide sidebar" variant="muted" size="m" onClick={onHide} />
      </div>
      <div style={{ padding: "0 10px 10px" }}>
        <label className="vt-input">
          <Icon name="search" size={13} style={{ color: "var(--text-tertiary)" }} />
          <input value={filter} onChange={(event) => setFilter(event.target.value)} placeholder="Search sessions" />
        </label>
      </div>
      <div className="flex min-h-0 flex-1 flex-col overflow-y-auto" style={{ padding: "0 10px", gap: 14 }}>
        <Section title="Running" rows={running} selectedId={selectedId} onSelect={onSelect} now={now} empty="No live agents." accent />
        <Section
          title="Recent"
          rows={recent}
          selectedId={selectedId}
          onSelect={onSelect}
          now={now}
          empty={needle ? "No matching sessions." : "No sessions under this directory yet."}
          accent={false}
        />
      </div>
      <div className="hairline-t" style={{ padding: 10 }}>
        <button type="button" className="vt-btn vt-btn--glass w-full" onClick={onNew}>
          <Icon name="plus" size={14} />
          New Session
        </button>
      </div>
    </aside>
  );
}
