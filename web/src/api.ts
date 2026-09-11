// Browser-side client for the routes in server.ts. Every call resolves to the
// parsed JSON body or throws an Error carrying the server's `error` text, so
// components never inspect Response objects.
import type { SessionLists, SessionRow } from "../lib/drip";
import type { TranscriptEntry } from "../lib/transcript";

export type { SessionLists, SessionRow, TranscriptEntry };

export interface Bootstrap {
  cwd: string;
  home: string;
  maxContextTokens: number | null;
  dripBin: string;
}

export interface StateResponse {
  state: Record<string, unknown> | null;
  isRunning: boolean;
}

export interface TranscriptResponse {
  entries: TranscriptEntry[];
  nextOffset: number;
  /** The transcript was truncated or rewritten: `entries` replace the tail. */
  reset: boolean;
  isRunning: boolean;
}

async function call<T>(path: string, init?: RequestInit): Promise<T> {
  const response = await fetch(path, init);
  const text = await response.text();
  let body: unknown = null;
  try {
    body = text ? JSON.parse(text) : null;
  } catch {
    body = { error: text };
  }
  if (!response.ok) {
    const message = (body as { error?: string } | null)?.error ?? `${response.status} ${response.statusText}`;
    throw new Error(message);
  }
  return body as T;
}

function post<T>(path: string, payload: unknown): Promise<T> {
  return call<T>(path, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(payload),
  });
}

const session = (id: string) => `api/sessions/${encodeURIComponent(id)}`;

export const api = {
  bootstrap: () => call<Bootstrap>("api/bootstrap"),
  sessions: () => call<SessionLists>("api/sessions"),
  state: (id: string) => call<StateResponse>(`${session(id)}/state`),
  transcript: (id: string, offset: number) =>
    call<TranscriptResponse>(`${session(id)}/transcript?offset=${offset}`),
  result: (id: string) => call<unknown>(`${session(id)}/result`),
  run: (goal: string, maxIterations?: number) =>
    post<{ sessionId: string; transcriptPath?: string; statePath?: string }>(
      "api/run",
      maxIterations === undefined ? { goal } : { goal, maxIterations },
    ),
  // The server decides send-vs-resume from a fresh lease reading; the
  // browser's notion of "running" is a poll or two behind.
  message: (id: string, text: string) =>
    post<{ ok: boolean; mode: "send" | "resume" | "queued"; detail?: string }>(`${session(id)}/message`, { text }),
  stop: (id: string) => post<{ ok: boolean }>(`${session(id)}/stop`, {}),
};
