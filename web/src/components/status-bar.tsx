// Footer: where this UI is looking (cwd, home, drip binary), poll health,
// and the most recent error.
import { relativeTime } from "../../lib/transcript";
import type { Bootstrap } from "../api";

interface Props {
  bootstrap: Bootstrap | null;
  lastPoll: number | null;
  error: string | null;
  /** A non-error outcome the operator should still see (e.g. a message queued for a live run). */
  notice?: string | null;
}

function Chip({ label, value }: { label: string; value: string }) {
  return (
    <span className="flex min-w-0 items-center gap-1">
      <span className="text-neutral-600">{label}</span>
      <span className="truncate font-mono text-neutral-400" title={value}>
        {value}
      </span>
    </span>
  );
}

export function StatusBar({ bootstrap, lastPoll, error, notice = null }: Props) {
  const stale = lastPoll !== null && Date.now() - lastPoll > 10_000;
  return (
    <footer className="flex items-center gap-4 border-t border-neutral-800 px-3 py-1 text-[11px]">
      <Chip label="cwd" value={bootstrap?.cwd ?? "…"} />
      <Chip label="home" value={bootstrap?.home ?? "…"} />
      <Chip label="drip" value={bootstrap?.dripBin ?? "…"} />
      <span className="ml-auto flex shrink-0 items-center gap-2">
        {!error && notice && (
          <span className="max-w-md truncate text-amber-300" title={notice}>
            {notice}
          </span>
        )}
        {error && (
          <span className="max-w-md truncate text-red-300" title={error}>
            {error}
          </span>
        )}
        <span className={`h-1.5 w-1.5 rounded-full ${lastPoll === null ? "bg-neutral-600" : stale ? "bg-amber-400" : "bg-emerald-400"}`} />
        <span className="text-neutral-500">{lastPoll === null ? "connecting" : `polled ${relativeTime(new Date(lastPoll).toISOString(), Date.now())}`}</span>
      </span>
    </footer>
  );
}
