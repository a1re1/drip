// App shell: three columns (sessions rail, timeline + composer, detail panel)
// over a status bar. All data comes from polling — the server is a thin
// bridge over drip's session files, so there is nothing to subscribe to.
import { useCallback, useEffect, useRef, useState } from "react";
import { api, type Bootstrap, type SessionLists, type SessionRow, type StateResponse, type TranscriptEntry } from "./api";
import { Composer } from "./components/composer";
import { DetailPanel } from "./components/detail-panel";
import { SessionsRail } from "./components/sessions-rail";
import { StatusBar } from "./components/status-bar";
import { Timeline } from "./components/timeline";

const EMPTY_LISTS: SessionLists = { running: [], recent: [] };
const SESSIONS_POLL_MS = 2000;
const TRANSCRIPT_POLL_MS = 1000;
const STATE_POLL_MS = 2000;

export function App() {
  const [bootstrap, setBootstrap] = useState<Bootstrap | null>(null);
  const [lists, setLists] = useState<SessionLists>(EMPTY_LISTS);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [entries, setEntries] = useState<TranscriptEntry[]>([]);
  const [sessionState, setSessionState] = useState<StateResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  // A run started from this page, shown until the next list poll knows it.
  const [pending, setPending] = useState<SessionRow | null>(null);
  const [lastPoll, setLastPoll] = useState<number | null>(null);
  const offsetRef = useRef(0);
  const autoSelected = useRef(false);

  useEffect(() => {
    api.bootstrap().then(setBootstrap).catch((e: Error) => setError(e.message));
  }, []);

  // Sessions rail.
  useEffect(() => {
    let cancelled = false;
    const tick = async () => {
      try {
        const next = await api.sessions();
        if (cancelled) return;
        setLists(next);
        setLastPoll(Date.now());
        // First load: open the newest running session, else the newest recent one.
        if (!autoSelected.current) {
          autoSelected.current = true;
          const first = next.running[0] ?? next.recent[0];
          if (first) setSelectedId((current) => current ?? first.id);
        }
      } catch (e) {
        if (!cancelled) setError((e as Error).message);
      }
    };
    void tick();
    const timer = setInterval(tick, SESSIONS_POLL_MS);
    return () => {
      cancelled = true;
      clearInterval(timer);
    };
  }, []);

  // Transcript tail: reset the byte cursor whenever the selection changes.
  useEffect(() => {
    setEntries([]);
    setSessionState(null);
    offsetRef.current = 0;
    if (!selectedId) return;
    let cancelled = false;
    let inFlight = false;
    const tick = async () => {
      if (inFlight) return;
      inFlight = true;
      try {
        const page = await api.transcript(selectedId, offsetRef.current);
        if (cancelled) return;
        if (page.reset) setEntries(page.entries);
        else if (page.entries.length > 0) setEntries((prev) => prev.concat(page.entries));
        offsetRef.current = page.nextOffset;
      } catch (e) {
        if (!cancelled) setError((e as Error).message);
      } finally {
        inFlight = false;
      }
    };
    void tick();
    const timer = setInterval(tick, TRANSCRIPT_POLL_MS);
    return () => {
      cancelled = true;
      clearInterval(timer);
    };
  }, [selectedId]);

  // Task ledger and counters from state.json.
  useEffect(() => {
    if (!selectedId) return;
    let cancelled = false;
    const tick = async () => {
      try {
        const next = await api.state(selectedId);
        if (!cancelled) setSessionState(next);
      } catch (e) {
        if (!cancelled) setError((e as Error).message);
      }
    };
    void tick();
    const timer = setInterval(tick, STATE_POLL_MS);
    return () => {
      cancelled = true;
      clearInterval(timer);
    };
  }, [selectedId]);

  const listed = [...lists.running, ...lists.recent].find((row) => row.id === selectedId) ?? null;
  // A just-started run is selectable and messageable before the 2s list
  // poll has caught up with it.
  const selected = listed ?? (pending !== null && pending.id === selectedId ? pending : null);
  const isRunning = selected?.running ?? sessionState?.isRunning ?? false;
  useEffect(() => {
    if (pending !== null && [...lists.running, ...lists.recent].some((row) => row.id === pending.id)) setPending(null);
  }, [lists, pending]);

  // Runs one operator action; resolves true on success so callers can decide
  // whether to clear a draft. Failures land in the status bar.
  const act = useCallback(async (work: () => Promise<unknown>): Promise<boolean> => {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      await work();
      return true;
    } catch (e) {
      setError((e as Error).message);
      return false;
    } finally {
      setBusy(false);
    }
  }, []);

  const handleRun = (goal: string, maxIterations?: number) =>
    act(async () => {
      const handle = await api.run(goal, maxIterations);
      const now = new Date().toISOString();
      setPending({
        id: handle.sessionId,
        createdAt: now,
        cwd: bootstrap?.cwd ?? "",
        dir: "",
        goalCount: 1,
        lastGoal: goal,
        lastRun: null,
        running: true,
        statePath: handle.statePath ?? "",
        status: "running",
        transcriptPath: handle.transcriptPath ?? "",
        updatedAt: now,
      });
      setSelectedId(handle.sessionId);
    });
  const handleSubmit = (text: string) => {
    if (!selectedId) return Promise.resolve(false);
    return act(async () => {
      const outcome = await api.message(selectedId, text);
      if (outcome.mode === "queued") setNotice("queued: a run is live — it picks the message up at its next cycle");
    });
  };
  const handleStop = () => {
    if (selectedId) void act(() => api.stop(selectedId));
  };

  return (
    <div className="flex h-screen flex-col bg-neutral-950 text-neutral-200">
      <div className="flex min-h-0 flex-1">
        <SessionsRail lists={lists} selectedId={selectedId} onSelect={setSelectedId} onRun={handleRun} busy={busy} />
        <main className="flex min-w-0 flex-1 flex-col border-x border-neutral-800">
          <header className="flex items-center gap-3 border-b border-neutral-800 px-4 py-2 text-sm">
            {selected ? (
              <>
                <span className={`h-2 w-2 rounded-full ${isRunning ? "bg-emerald-400" : "bg-neutral-600"}`} />
                <span className="font-mono text-neutral-400">{selected.id.slice(0, 8)}</span>
                <span className="truncate text-neutral-300">{selected.lastGoal ?? "(no goal yet)"}</span>
                <span className="ml-auto text-xs text-neutral-500">{isRunning ? `running · pid ${selected.pid ?? "?"}` : selected.status}</span>
              </>
            ) : (
              <span className="text-neutral-500">Select a session, or start one from the rail.</span>
            )}
          </header>
          <Timeline entries={entries} />
          <Composer
            key={selectedId ?? "none"}
            disabled={!selectedId || busy}
            isRunning={isRunning}
            onSubmit={handleSubmit}
            onStop={handleStop}
          />
        </main>
        <DetailPanel
          row={selected}
          state={sessionState?.state ?? null}
          entries={entries}
          maxContextTokens={bootstrap?.maxContextTokens ?? null}
        />
      </div>
      <StatusBar bootstrap={bootstrap} lastPoll={lastPoll} error={error} notice={notice} />
    </div>
  );
}
