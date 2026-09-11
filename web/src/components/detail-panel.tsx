// Right column (toggled): what the harness is doing right now —
// iteration/loop counters, context pressure, token totals, the task ledger
// from state.json, and where this UI is looking.
import { Fragment, useState, type ReactNode } from "react";
import { latestPromptTokens, sumUsage } from "../../lib/transcript";
import type { Bootstrap, SessionRow, TranscriptEntry } from "../api";
import { Icon, Tag } from "./icons";
import { fmtTokens } from "./timeline";

interface Props {
  row: SessionRow | null;
  state: Record<string, unknown> | null;
  entries: TranscriptEntry[];
  bootstrap: Bootstrap | null;
}

interface LedgerTask {
  id?: string;
  title?: string;
  description?: string;
  status?: string;
}

const STATUS_COLOR: Record<string, string> = {
  completed: "var(--green)",
  in_progress: "var(--accent)",
  blocked: "var(--orange)",
  dropped: "var(--text-tertiary)",
  pending: "var(--text-tertiary)",
};

function Section({ title, aside, children }: { title: string; aside?: string; children: ReactNode }) {
  return (
    <div className="flex flex-col gap-2">
      <div className="flex items-baseline gap-2">
        <span className="section-label">{title}</span>
        {aside && <span className="t-caption c-tertiary">{aside}</span>}
      </div>
      {children}
    </div>
  );
}

function Grid({ rows }: { rows: [string, ReactNode][] }) {
  return (
    <div className="t-footnote grid" style={{ gridTemplateColumns: "1fr auto", rowGap: 6, columnGap: 12 }}>
      {rows.map(([label, value]) => (
        <Fragment key={label}>
          <span className="c-secondary">{label}</span>
          <span className="mono flex justify-end text-right">{value}</span>
        </Fragment>
      ))}
    </div>
  );
}

function TaskCard({ task }: { task: LedgerTask }) {
  const [open, setOpen] = useState(false);
  const text = task.title ?? task.description ?? "";
  return (
    <div className="glass flex flex-col gap-1.5" style={{ padding: "10px 12px", borderRadius: "var(--radius-m)", background: "var(--surface-card)" }}>
      <div className="flex items-center gap-2">
        <Tag>In progress</Tag>
        {task.id && <span className="mono t-caption c-tertiary">{task.id}</span>}
      </div>
      <div
        className="t-footnote"
        style={{ lineHeight: 1.45, display: "-webkit-box", WebkitLineClamp: open ? "unset" : 4, WebkitBoxOrient: "vertical", overflow: "hidden", overflowWrap: "anywhere" }}
      >
        {text}
      </div>
      {text.length > 160 && (
        <button type="button" className="vt-btn vt-btn--plain vt-btn--s self-start" style={{ padding: "0 6px" }} onClick={() => setOpen((value) => !value)}>
          {open ? "Show less" : "Show more"}
        </button>
      )}
    </div>
  );
}

function TaskRow({ task }: { task: LedgerTask }) {
  const status = task.status ?? "pending";
  const color = STATUS_COLOR[status] ?? "var(--text-tertiary)";
  const filled = status === "completed" || status === "blocked";
  return (
    <div className="task-row" title={`${task.id ?? ""} · ${status}`}>
      <span
        className="flex shrink-0 items-center justify-center rounded-full"
        style={{ width: 14, height: 14, marginTop: 2, border: `1.5px solid ${color}`, background: filled ? color : "transparent", color: "var(--material-opaque)" }}
      >
        {status === "completed" && <Icon name="check" size={9} style={{ strokeWidth: 3 }} />}
      </span>
      <span className="t-footnote c-secondary" style={{ textDecoration: status === "dropped" ? "line-through" : undefined, overflowWrap: "anywhere", display: "-webkit-box", WebkitLineClamp: 3, WebkitBoxOrient: "vertical", overflow: "hidden" }}>
        {task.title ?? task.description ?? task.id ?? ""}
      </span>
    </div>
  );
}

export function DetailPanel({ row, state, entries, bootstrap }: Props) {
  const usage = sumUsage(entries);
  const prompt = latestPromptTokens(entries);
  const max = bootstrap?.maxContextTokens ?? null;
  const contextPct = prompt !== null && max ? Math.min(100, Math.round((prompt / max) * 100)) : null;
  const tasks = Array.isArray(state?.["tasks"]) ? (state?.["tasks"] as LedgerTask[]) : [];
  const counts = tasks.reduce<Record<string, number>>((acc, task) => {
    const status = task.status ?? "pending";
    acc[status] = (acc[status] ?? 0) + 1;
    return acc;
  }, {});
  const active = tasks.filter((task) => task.status === "in_progress");
  const rest = tasks.filter((task) => task.status !== "in_progress");
  const running = row?.running ?? false;
  const status = row ? (running ? "running" : row.status) : "—";

  return (
    <aside className="sidebar-surface hairline-l flex min-h-0 shrink-0 flex-col" style={{ width: 280 }}>
      <div className="flex min-h-0 flex-1 flex-col gap-5 overflow-y-auto" style={{ padding: 16 }}>
        <Section title="Run">
          <Grid
            rows={[
              ["Status", <Tag key="s" color={running ? "var(--green)" : "var(--text-tertiary)"}>{status}</Tag>],
              ["Iteration", String(state?.["iteration"] ?? "—")],
              ["Loop", String(state?.["loop"] ?? "—")],
              ...(running && row?.pid !== undefined ? [["pid", String(row.pid)] as [string, ReactNode]] : []),
              ...(row?.lastRun ? [["Last run", <Tag key="r" color="var(--orange)">{row.lastRun.reason}</Tag>] as [string, ReactNode]] : []),
            ]}
          />
        </Section>

        {contextPct !== null && (
          <Section title="Context">
            <div className="t-footnote flex justify-between">
              <span className="mono">
                {fmtTokens(prompt ?? 0)} / {fmtTokens(max ?? 0)}
              </span>
              <span className="c-secondary">{contextPct}%</span>
            </div>
            <div className="inset overflow-hidden" style={{ height: 6, borderRadius: 3 }}>
              <div
                style={{
                  width: `${contextPct}%`,
                  minWidth: 6,
                  height: "100%",
                  borderRadius: 3,
                  background: contextPct > 85 ? "var(--red)" : contextPct > 60 ? "var(--orange)" : "var(--accent)",
                }}
              />
            </div>
          </Section>
        )}

        <Section title="Usage">
          <Grid
            rows={[
              ["Prompt", fmtTokens(usage.promptTokens)],
              ["Completion", fmtTokens(usage.completionTokens)],
              ["Cache read", fmtTokens(usage.cacheReadTokens)],
              ["Cache write", fmtTokens(usage.cacheCreationTokens)],
            ]}
          />
        </Section>

        <Section
          title="Tasks"
          aside={
            tasks.length > 0
              ? Object.entries(counts)
                  .map(([name, count]) => `${count} ${name.replace("_", " ")}`)
                  .join(" · ")
              : undefined
          }
        >
          {tasks.length === 0 ? (
            <p className="t-footnote c-tertiary">No task ledger yet.</p>
          ) : (
            <>
              {active.map((task, index) => (
                <TaskCard key={task.id ?? `active-${index}`} task={task} />
              ))}
              <div className="flex flex-col" style={{ gap: 2 }}>
                {rest.map((task, index) => (
                  <TaskRow key={task.id ?? `task-${index}`} task={task} />
                ))}
              </div>
            </>
          )}
        </Section>

        <div className="mono t-caption2 c-tertiary" style={{ lineHeight: 1.5, wordBreak: "break-all" }}>
          cwd {bootstrap?.cwd ?? "…"}
          <br />
          home {bootstrap?.home ?? "…"}
          <br />
          drip {bootstrap?.dripBin ?? "…"}
        </div>
      </div>
    </aside>
  );
}
