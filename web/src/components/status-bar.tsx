// Poll health under the stream, and a floating toast for the most recent
// error or notice (a message queued for a live run, say).
import { relativeTime } from "../../lib/transcript";

export function PollCaption({ lastPoll }: { lastPoll: number | null }) {
  const stale = lastPoll !== null && Date.now() - lastPoll > 10_000;
  return (
    <span className="inline-flex items-center gap-1.5">
      <span
        className="rounded-full"
        style={{ width: 6, height: 6, background: lastPoll === null ? "var(--text-quaternary)" : stale ? "var(--orange)" : "var(--green)" }}
      />
      {lastPoll === null ? "connecting" : `polled ${relativeTime(new Date(lastPoll).toISOString(), Date.now())}`}
    </span>
  );
}

export function StatusToast({ error, notice }: { error: string | null; notice: string | null }) {
  const text = error ?? notice;
  if (!text) return null;
  return (
    <div className="pointer-events-none absolute left-0 right-0 z-10 flex justify-center" style={{ bottom: 96 }}>
      <div className="toast pointer-events-auto" title={text} style={{ color: error ? "var(--red)" : "var(--text-primary)" }}>
        <span className="shrink-0 rounded-full" style={{ width: 7, height: 7, background: error ? "var(--red)" : "var(--orange)" }} />
        <span className="truncate">{text}</span>
      </div>
    </div>
  );
}
