// App shell: a message stream with a floating header, flanked by a toggled
// sessions sidebar on the left and a toggled details column on the right.
// All data comes from polling — the server is a thin bridge over drip's
// session files, so there is nothing to subscribe to.
import { useCallback, useEffect, useRef, useState } from "react";
import { latestPromptTokens } from "../lib/transcript";
import { api, type Bootstrap, type SessionLists, type SessionRow, type StateResponse, type TranscriptEntry } from "./api";
import { Composer, type ComposerMode } from "./components/composer";
import { DetailPanel } from "./components/detail-panel";
import { Icon, IconButton } from "./components/icons";
import { SessionsRail } from "./components/sessions-rail";
import { PollCaption, StatusToast } from "./components/status-bar";
import { fmtTokens, Timeline } from "./components/timeline";

const EMPTY_LISTS: SessionLists = { running: [], recent: [] };
const SESSIONS_POLL_MS = 2000;
const TRANSCRIPT_POLL_MS = 1000;
const STATE_POLL_MS = 2000;

/** A per-browser layout preference; storage may be unavailable or throw. */
function usePreference(key: string, fallback: boolean): [boolean, (next: boolean) => void] {
  const [value, setValue] = useState(() => {
    try {
      const stored = localStorage.getItem(key);
      return stored === null ? fallback : stored === "1";
    } catch {
      return fallback;
    }
  });
  const update = (next: boolean) => {
    setValue(next);
    try {
      localStorage.setItem(key, next ? "1" : "0");
    } catch {
      // A private window or blocked storage: the toggle still works for this page load.
    }
  };
  return [value, update];
}

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
  const [showSidebar, setShowSidebar] = usePreference("drip-ui.sidebar", false);
  const [showDetails, setShowDetails] = usePreference("drip-ui.details", false);
  const offsetRef = useRef(0);
  const autoSelected = useRef(false);

  useEffect(() => {
    api.bootstrap().then(setBootstrap).catch((e: Error) => setError(e.message));
  }, []);

  // Sessions list.
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
  // whether to clear a draft. Failures land in the toast.
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
  const handleMessage = (text: string) => {
    if (!selectedId) return Promise.resolve(false);
    return act(async () => {
      const outcome = await api.message(selectedId, text);
      if (outcome.mode === "queued") setNotice("queued: a run is live — it picks the message up at its next cycle");
    });
  };
  const handleStop = () => {
    if (selectedId) void act(() => api.stop(selectedId));
  };

  const mode: ComposerMode = selectedId === null ? "new" : isRunning ? "running" : "idle";
  const state = sessionState?.state ?? null;
  const prompt = latestPromptTokens(entries);
  const statusLine = selected
    ? [
        isRunning ? "Running" : selected.status,
        state?.["iteration"] !== undefined ? `iteration ${String(state["iteration"])}` : "",
        state?.["loop"] !== undefined ? `loop ${String(state["loop"])}` : "",
        prompt !== null ? `${fmtTokens(prompt)} ctx` : "",
      ]
        .filter(Boolean)
        .join(" · ")
    : "Describe a goal below to start one";

  return (
    <div className="fixed inset-0 flex" style={{ background: "var(--material-opaque)" }}>
      {showSidebar && (
        <SessionsRail
          lists={lists}
          selectedId={selectedId}
          onSelect={setSelectedId}
          onNew={() => setSelectedId(null)}
          onHide={() => setShowSidebar(false)}
        />
      )}

      <div className="relative flex min-h-0 min-w-0 flex-1 flex-col">
        {/* Floating header: controls left and right, a status pill in the middle. */}
        <div
          className="pointer-events-none absolute left-0 right-0 top-0 z-[5] grid items-start"
          style={{ gridTemplateColumns: "1fr auto 1fr", gap: 12, padding: "10px 14px" }}
        >
          <div className="pointer-events-auto flex gap-1.5">
            {!showSidebar && <IconButton icon="panel-left" label="Show sessions" size="l" onClick={() => setShowSidebar(true)} />}
            <IconButton icon="plus" label="New session" size="l" pressed={selectedId === null} onClick={() => setSelectedId(null)} />
          </div>
          <div
            className="glass glass-blur pointer-events-auto flex flex-col items-center"
            style={{ gap: 1, padding: "6px 14px", borderRadius: "var(--radius-m)", borderColor: "var(--stroke-control)", maxWidth: "min(60vw, 440px)" }}
          >
            <div className="t-footnote flex max-w-full items-center gap-1.5 font-semibold whitespace-nowrap">
              <span
                className={`shrink-0 rounded-full${isRunning ? " pulse" : ""}`}
                style={{ width: 7, height: 7, background: isRunning ? "var(--green)" : selected ? "var(--text-tertiary)" : "var(--accent)" }}
              />
              <span className="truncate">{selected ? (selected.lastGoal ?? "(no goal yet)") : "New session"}</span>
              {selected && (
                <button
                  type="button"
                  className="flex shrink-0 items-center"
                  title="Toggle details"
                  aria-label="Toggle details"
                  onClick={() => setShowDetails(!showDetails)}
                  style={{ color: "var(--text-tertiary)", background: "none", border: 0, padding: 0, cursor: "pointer" }}
                >
                  <Icon name="chevron-right" size={11} />
                </button>
              )}
            </div>
            <div className="t-caption c-secondary whitespace-nowrap">{statusLine}</div>
          </div>
          <div className="pointer-events-auto flex justify-end gap-1.5">
            {isRunning && <IconButton icon="pause" label="Stop agent" size="l" disabled={busy} onClick={handleStop} />}
            <IconButton icon="list" label="Toggle details" size="l" pressed={showDetails} onClick={() => setShowDetails(!showDetails)} />
          </div>
        </div>

        {/* The toast anchors to the stream's bottom edge, so it floats just above the composer whatever its height. */}
        <div className="relative flex min-h-0 flex-1 flex-col">
          <Timeline entries={entries} isRunning={isRunning} footer={<PollCaption lastPoll={lastPoll} />} />
          <StatusToast error={error} notice={notice} />
        </div>
        <Composer
          key={selectedId ?? "none"}
          mode={mode}
          disabled={busy}
          onSubmit={mode === "new" ? handleRun : handleMessage}
          onStop={handleStop}
        />
      </div>

      {showDetails && <DetailPanel row={selected} state={state} entries={entries} bootstrap={bootstrap} />}
    </div>
  );
}
