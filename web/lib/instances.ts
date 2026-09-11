// Registry of running `drip --ui` instances: one JSON file per instance under
// <home>/ui/instances/. It is what the hub page lists and what the Caddy
// reconciliation treats as the truth about who is alive — liveness is the
// pid, so an instance that was killed without cleaning up is dropped the next
// time anyone reads the registry.
import { mkdirSync, readdirSync, readFileSync, renameSync, rmSync, rmdirSync, statSync, writeFileSync } from "node:fs";
import { join } from "node:path";

export interface InstanceRecord {
  label: string;
  cwd: string;
  port: number;
  pid: number;
  startedAt: string;
  version: string;
}

export function instancesDir(home: string): string {
  return join(home, "ui", "instances");
}

function recordPath(home: string, label: string): string {
  return join(instancesDir(home), `${label}.json`);
}

export function writeInstance(home: string, record: InstanceRecord): void {
  mkdirSync(instancesDir(home), { recursive: true });
  // Write-then-rename so a concurrent reader never sees a half-written
  // record (which it would judge unreadable and delete).
  const path = recordPath(home, record.label);
  const temp = `${path}.${process.pid}.tmp`;
  writeFileSync(temp, `${JSON.stringify(record, null, 2)}\n`);
  renameSync(temp, path);
}

/** The live record for `label`, if a running instance owns it. */
export function findInstance(home: string, label: string, isAlive: (pid: number) => boolean = pidAlive): InstanceRecord | null {
  return liveInstances(home, isAlive).find((instance) => instance.label === label) ?? null;
}

/**
 * Delete `label`'s record — but only the caller's own (`pid` matches) or a
 * dead one, so an instance that lost a same-directory race at startup
 * cannot delete the record of the one still serving.
 */
export function removeInstance(home: string, label: string, pid?: number, isAlive: (pid: number) => boolean = pidAlive): void {
  const path = recordPath(home, label);
  if (pid !== undefined) {
    let record: InstanceRecord | null = null;
    try {
      record = parseRecord(readFileSync(path, "utf8"));
    } catch {
      record = null;
    }
    if (record !== null && record.pid !== pid && isAlive(record.pid)) return;
  }
  rmSync(path, { force: true });
}

export function pidAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    // EPERM means the process exists but is not ours; only ESRCH means gone.
    return (error as NodeJS.ErrnoException).code === "EPERM";
  }
}

function parseRecord(text: string): InstanceRecord | null {
  try {
    const value = JSON.parse(text) as Partial<InstanceRecord>;
    if (typeof value.label !== "string" || typeof value.cwd !== "string") return null;
    if (typeof value.port !== "number" || typeof value.pid !== "number") return null;
    return {
      label: value.label,
      cwd: value.cwd,
      port: value.port,
      pid: value.pid,
      startedAt: typeof value.startedAt === "string" ? value.startedAt : "",
      version: typeof value.version === "string" ? value.version : "",
    };
  } catch {
    return null;
  }
}

/**
 * Every instance whose process is still alive, sorted by label. Records for
 * dead pids (and unreadable files) are deleted as a side effect.
 */
export function liveInstances(home: string, isAlive: (pid: number) => boolean = pidAlive): InstanceRecord[] {
  let names: string[];
  try {
    names = readdirSync(instancesDir(home)).filter((name) => name.endsWith(".json"));
  } catch {
    return [];
  }
  const live: InstanceRecord[] = [];
  for (const name of names) {
    const path = join(instancesDir(home), name);
    let record: InstanceRecord | null = null;
    try {
      record = parseRecord(readFileSync(path, "utf8"));
    } catch {
      record = null;
    }
    if (record !== null && isAlive(record.pid)) {
      live.push(record);
    } else {
      rmSync(path, { force: true });
    }
  }
  return live.sort((a, b) => (a.label < b.label ? -1 : a.label > b.label ? 1 : 0));
}

/** A lock older than this belongs to a dead holder (live work takes seconds). */
const REGISTRY_LOCK_STALE_MS = 30_000;
/** Longer than the stale age, so a waiter outlives a dead holder and reclaims it. */
const REGISTRY_LOCK_WAIT_MS = 45_000;

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/**
 * Run `work` while holding the registry lock (an atomically created
 * directory). Instances starting or stopping at the same moment claim
 * their identity and reconcile the Caddy hub one after another instead of
 * racing to write the same record or create the same server and hub
 * route. A lock older than a minute belongs to a dead process and is
 * reclaimed; the holder's token inside the lock keeps a reclaimed holder
 * from releasing its successor's lock.
 */
export async function withRegistryLock<T>(home: string, work: () => Promise<T>, waitMs = REGISTRY_LOCK_WAIT_MS): Promise<T> {
  const dir = instancesDir(home);
  mkdirSync(dir, { recursive: true });
  const lock = join(dir, ".lock");
  const token = `${process.pid}.${Math.random().toString(36).slice(2, 10)}`;
  const deadline = Date.now() + waitMs;
  for (;;) {
    try {
      mkdirSync(lock);
      writeFileSync(join(lock, "owner"), token);
      break;
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== "EEXIST") throw error;
      let age = 0;
      try {
        age = Date.now() - statSync(lock).mtimeMs;
      } catch {
        continue; // released between the mkdir and the stat
      }
      if (age >= REGISTRY_LOCK_STALE_MS) {
        // Reclaim by renaming the stale lock aside: rename is atomic, so of
        // two peers judging the same lock stale only one succeeds, and
        // neither can delete a fresh lock a peer created in the meantime.
        const aside = `${lock}.stale.${process.pid}.${Math.random().toString(36).slice(2, 8)}`;
        try {
          renameSync(lock, aside);
          rmSync(aside, { recursive: true, force: true });
        } catch {
          // Lost the rename to a peer (or the holder released it): retry.
        }
        if (Date.now() >= deadline) throw new Error(`registry lock ${lock} could not be reclaimed`);
        continue;
      }
      if (Date.now() >= deadline) throw new Error(`registry lock ${lock} is held by another drip --ui`);
      await sleep(100);
    }
  }
  try {
    return await work();
  } finally {
    try {
      // Release only our own lock: after a stall past the stale age a peer
      // may have reclaimed it and be holding a fresh one under this path.
      if (readFileSync(join(lock, "owner"), "utf8") === token) rmSync(lock, { recursive: true, force: true });
    } catch {
      // Already reclaimed by a peer that judged us stale; nothing to release.
    }
  }
}
