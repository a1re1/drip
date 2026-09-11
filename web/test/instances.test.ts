// Instance registry: files under <home>/ui/instances, liveness by pid.
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { existsSync, mkdtempSync, readdirSync, rmSync, utimesSync, writeFileSync, mkdirSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { findInstance, instancesDir, liveInstances, pidAlive, removeInstance, withRegistryLock, writeInstance, type InstanceRecord } from "../lib/instances";

let home = "";

beforeEach(() => {
  home = mkdtempSync(join(tmpdir(), "drip-ui-instances-"));
});

afterEach(() => {
  rmSync(home, { recursive: true, force: true });
});

function record(label: string, overrides: Partial<InstanceRecord> = {}): InstanceRecord {
  return {
    label,
    cwd: `/work/${label}`,
    port: 4141,
    pid: 1,
    startedAt: "2026-09-10T10:00:00.000Z",
    version: "0.103.0",
    ...overrides,
  };
}

describe("instance registry", () => {
  test("write, list sorted, remove", () => {
    writeInstance(home, record("zeta", { port: 4142, pid: 2 }));
    writeInstance(home, record("alpha", { port: 4141, pid: 3 }));
    const alive = () => true;
    expect(liveInstances(home, alive).map((instance) => instance.label)).toEqual(["alpha", "zeta"]);
    removeInstance(home, "alpha");
    expect(liveInstances(home, alive).map((instance) => instance.label)).toEqual(["zeta"]);
    // Removing twice is fine — an exiting instance never fails on cleanup.
    removeInstance(home, "alpha");
  });

  test("dead pids and unreadable files are dropped and deleted", () => {
    writeInstance(home, record("live", { pid: 10 }));
    writeInstance(home, record("dead", { pid: 11 }));
    mkdirSync(instancesDir(home), { recursive: true });
    writeFileSync(join(instancesDir(home), "junk.json"), "{not json");
    writeFileSync(join(instancesDir(home), "partial.json"), JSON.stringify({ label: "partial" }));
    const alive = (pid: number) => pid === 10;
    expect(liveInstances(home, alive).map((instance) => instance.label)).toEqual(["live"]);
    expect(readdirSync(instancesDir(home)).sort()).toEqual(["live.json"]);
  });

  test("findInstance answers only for a live record and leaves no temp files", () => {
    writeInstance(home, record("mine", { pid: 10, port: 4143 }));
    writeInstance(home, record("gone", { pid: 11 }));
    const alive = (pid: number) => pid === 10;
    expect(findInstance(home, "mine", alive)?.port).toBe(4143);
    expect(findInstance(home, "gone", alive)).toBeNull();
    expect(findInstance(home, "never", alive)).toBeNull();
    expect(readdirSync(instancesDir(home)).sort()).toEqual(["mine.json"]);
  });

  test("removeInstance with a pid only deletes its own or a dead record", () => {
    writeInstance(home, record("proj", { pid: 10 }));
    const alive = (pid: number) => pid === 10;
    removeInstance(home, "proj", 20, alive); // the loser of a same-directory race
    expect(existsSync(join(instancesDir(home), "proj.json"))).toBe(true);
    removeInstance(home, "proj", 10, alive);
    expect(existsSync(join(instancesDir(home), "proj.json"))).toBe(false);
    writeInstance(home, record("proj", { pid: 10 }));
    removeInstance(home, "proj", 20, () => false); // owner is dead: anyone may clean up
    expect(existsSync(join(instancesDir(home), "proj.json"))).toBe(false);
  });

  test("a holder reclaimed while stalled does not release its successor's lock", async () => {
    const lock = join(instancesDir(home), ".lock");
    await withRegistryLock(home, async () => {
      // Simulate a peer that judged us stale: it renamed our lock aside and
      // took a fresh one under the same path.
      rmSync(lock, { recursive: true, force: true });
      mkdirSync(lock);
      writeFileSync(join(lock, "owner"), "peer.token");
    });
    expect(existsSync(lock)).toBe(true);
    expect(readdirSync(lock)).toEqual(["owner"]);
    rmSync(lock, { recursive: true, force: true });
  });

  test("a missing registry directory is an empty list", () => {
    expect(existsSync(instancesDir(home))).toBe(false);
    expect(liveInstances(home)).toEqual([]);
  });

  test("registry lock serializes concurrent work and is released after it", async () => {
    const order: string[] = [];
    const first = withRegistryLock(home, async () => {
      order.push("first:start");
      await new Promise((resolve) => setTimeout(resolve, 150));
      order.push("first:end");
      return 1;
    });
    const second = withRegistryLock(home, async () => {
      order.push("second:start");
      return 2;
    });
    expect(await Promise.all([first, second])).toEqual([1, 2]);
    expect(order).toEqual(["first:start", "first:end", "second:start"]);
    expect(existsSync(join(instancesDir(home), ".lock"))).toBe(false);
    // A failure inside still releases the lock.
    await expect(withRegistryLock(home, async () => { throw new Error("boom"); })).rejects.toThrow("boom");
    expect(existsSync(join(instancesDir(home), ".lock"))).toBe(false);
  });

  test("a stale registry lock is reclaimed by exactly one of several waiters", async () => {
    mkdirSync(instancesDir(home), { recursive: true });
    const lock = join(instancesDir(home), ".lock");
    mkdirSync(lock);
    const old = new Date(Date.now() - 5 * 60_000);
    utimesSync(lock, old, old);
    let holders = 0;
    let overlap = false;
    const work = async () => {
      holders += 1;
      if (holders > 1) overlap = true;
      await new Promise((resolve) => setTimeout(resolve, 30));
      holders -= 1;
    };
    await Promise.all([withRegistryLock(home, work), withRegistryLock(home, work), withRegistryLock(home, work)]);
    expect(overlap).toBe(false);
    // No lock and no renamed-aside remnants are left behind.
    expect(readdirSync(instancesDir(home))).toEqual([]);
  });

  test("pidAlive: our own pid is alive, an impossible pid is not", () => {
    expect(pidAlive(process.pid)).toBe(true);
    expect(pidAlive(2 ** 22 + 12345)).toBe(false);
  });
});
