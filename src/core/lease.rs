// port of src/cli/lease.ts
//
// TS notes kept for diffing: `now: () => Date` default params become explicit
// `&dyn Fn() -> DateTime<Utc>` arguments (Rust has no default args); callers
// that used the TS default pass `&chrono::Utc::now`.

use std::fs;
use std::path::Path;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

// A run's liveness lease: written next to state.json when a goal starts,
// heartbeat-refreshed while it executes, removed when it ends. Lets --list,
// --send, and double-launch guards distinguish "running right now" from
// "crashed and left the index saying active".
//
// Field order matters: TS writes JSON.stringify({ heartbeatAt, pid, startedAt })
// and serde serializes struct fields in declaration order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionLease {
    pub heartbeat_at: String,
    pub pid: i32,
    // TS readLease only validates pid/heartbeatAt, so a hand-written lease
    // without startedAt still parses; Option mirrors that loose read while
    // writeLease always emits the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
}

/// A live pid is the liveness truth — heartbeats pause during long tool calls
/// (a BASH sleep 120 outlives any reasonable freshness window), so staleness
/// alone must never read as dead. Only an extremely old heartbeat defeats the
/// pid check, as a guard against pid reuse across reboots/days.
pub const LEASE_STALE_MS: i64 = 24 * 60 * 60 * 1000;

pub fn write_lease(lease_path: &Path, now: &dyn Fn() -> DateTime<Utc>) -> std::io::Result<()> {
    let timestamp = now().to_rfc3339_opts(SecondsFormat::Millis, true);
    let existing = read_lease(lease_path);

    let started_at = match existing {
        Some(lease) if lease.pid == std::process::id() as i32 => lease.started_at,
        _ => None,
    };
    let lease = SessionLease {
        heartbeat_at: timestamp.clone(),
        pid: std::process::id() as i32,
        started_at: Some(started_at.unwrap_or(timestamp)),
    };

    // Plain writeFileSync — no atomic rename in the TS either.
    fs::write(
        lease_path,
        format!("{}\n", serde_json::to_string(&lease).expect("SessionLease serializes")),
    )
}

pub fn read_lease(lease_path: &Path) -> Option<SessionLease> {
    if !lease_path.exists() {
        return None;
    }

    let contents = fs::read_to_string(lease_path).ok()?;
    // The TS guard (parsed is an object with numeric pid and string
    // heartbeatAt) is exactly what serde enforces on this struct; unknown
    // fields are ignored, malformed JSON and wrong types read as null.
    serde_json::from_str(&contents).ok()
}

pub fn clear_lease(lease_path: &Path) {
    // Only the owning process may clear: a second drip invocation racing a live
    // run must not delete the runner's lease on its way out.
    let lease = read_lease(lease_path);

    if let Some(lease) = lease {
        if lease.pid != std::process::id() as i32 {
            return;
        }
    }

    // rmSync(leasePath, { force: true }): a missing file is not an error.
    let _ = fs::remove_file(lease_path);
}

// process.kill(pid, 0) in Node: signal 0 never delivers, and *any* failure —
// ESRCH for a dead pid, EPERM for a live pid owned by someone else — throws,
// so the TS try/catch reads both as "not alive". kill(2) returning 0 is the
// only liveness signal.
fn process_alive(pid: i32) -> bool {
    // SAFETY: kill(2) with signal 0 performs a permission/liveness check only.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

// { alive: false; lease: SessionLease | null } | { alive: true; lease: SessionLease }
#[derive(Debug, Clone, PartialEq)]
pub enum LeaseStatus {
    NotAlive { lease: Option<SessionLease> },
    Alive { lease: SessionLease },
}

impl LeaseStatus {
    pub fn alive(&self) -> bool {
        matches!(self, LeaseStatus::Alive { .. })
    }
}

// Alive means: the lease exists, its process exists, and the heartbeat is
// fresh. A dead-process or stale-heartbeat lease reads as not alive (the
// caller may clean it up).
pub fn check_lease(lease_path: &Path, now: &dyn Fn() -> DateTime<Utc>) -> LeaseStatus {
    let lease = read_lease(lease_path);

    let Some(lease) = lease else {
        return LeaseStatus::NotAlive { lease: None };
    };

    let fresh = match DateTime::parse_from_rfc3339(&lease.heartbeat_at) {
        // toISOString() stamps are RFC 3339 with milliseconds and Z; JS
        // new Date(garbage).getTime() is NaN and NaN < LEASE_STALE_MS is
        // false, so an unparseable heartbeat reads as stale.
        Ok(heartbeat) => {
            now().timestamp_millis() - heartbeat.with_timezone(&Utc).timestamp_millis() < LEASE_STALE_MS
        }
        Err(_) => false,
    };

    if !fresh || !process_alive(lease.pid) {
        return LeaseStatus::NotAlive { lease: Some(lease) };
    }

    LeaseStatus::Alive { lease }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    // Port of test/fixtures.ts makeTempRoot: mkdtemp under the OS temp dir;
    // TempDir removes it on drop (the vitest afterEach cleanup).
    fn make_lease_path() -> (tempfile::TempDir, std::path::PathBuf) {
        let root = tempfile::tempdir().expect("makeTempRoot");
        let lease_path = root.path().join("lease.json");
        (root, lease_path)
    }

    fn fixed_now(secs: (i32, u32, u32, u32, u32, u32)) -> impl Fn() -> DateTime<Utc> {
        move || Utc.with_ymd_and_hms(secs.0, secs.1, secs.2, secs.3, secs.4, secs.5).unwrap()
    }

    // it("writes, reads, refreshes, and clears a lease for this process")
    #[test]
    fn writes_reads_refreshes_and_clears_a_lease_for_this_process() {
        let (_root, lease_path) = make_lease_path();

        write_lease(&lease_path, &fixed_now((2026, 7, 8, 10, 0, 0))).unwrap();

        let first = read_lease(&lease_path).unwrap();

        assert_eq!(first.pid, std::process::id() as i32);
        assert_eq!(first.started_at.as_deref(), Some("2026-07-08T10:00:00.000Z"));

        // A heartbeat refresh keeps the original startedAt.
        write_lease(&lease_path, &fixed_now((2026, 7, 8, 10, 0, 30))).unwrap();

        let second = read_lease(&lease_path).unwrap();

        assert_eq!(second.started_at.as_deref(), Some("2026-07-08T10:00:00.000Z"));
        assert_eq!(second.heartbeat_at, "2026-07-08T10:00:30.000Z");

        // Byte parity with the TS writer: JSON.stringify({heartbeatAt, pid,
        // startedAt}) + "\n", field order and camelCase keys included.
        assert_eq!(
            fs::read_to_string(&lease_path).unwrap(),
            format!(
                "{{\"heartbeatAt\":\"2026-07-08T10:00:30.000Z\",\"pid\":{},\"startedAt\":\"2026-07-08T10:00:00.000Z\"}}\n",
                std::process::id()
            )
        );

        clear_lease(&lease_path);
        assert!(read_lease(&lease_path).is_none());
    }

    // it("treats a live pid as alive through long heartbeat gaps, but not extreme staleness")
    #[test]
    fn treats_a_live_pid_as_alive_through_long_heartbeat_gaps_but_not_extreme_staleness() {
        let (_root, lease_path) = make_lease_path();

        assert!(!check_lease(&lease_path, &Utc::now).alive());

        // Own pid + fresh heartbeat: alive.
        write_lease(&lease_path, &Utc::now).unwrap();
        assert!(check_lease(&lease_path, &Utc::now).alive());

        // A busy run heartbeats nothing during long tool calls (BASH sleep 120):
        // 10 minutes of silence with a live pid must still read as alive.
        fs::write(
            &lease_path,
            format!(
                "{{\"heartbeatAt\":\"2026-07-08T00:00:00.000Z\",\"pid\":{},\"startedAt\":\"2026-07-08T00:00:00.000Z\"}}",
                std::process::id()
            ),
        )
        .unwrap();
        assert!(check_lease(&lease_path, &fixed_now((2026, 7, 8, 0, 10, 0))).alive());

        // Extreme staleness defeats the pid check (pid reuse across days).
        assert!(!check_lease(&lease_path, &fixed_now((2026, 7, 10, 0, 0, 1))).alive());

        // Dead pid: not alive, lease still surfaced for diagnostics.
        let now_iso = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        fs::write(
            &lease_path,
            format!("{{\"heartbeatAt\":\"{now_iso}\",\"pid\":999999,\"startedAt\":\"{now_iso}\"}}"),
        )
        .unwrap();

        let status = check_lease(&lease_path, &Utc::now);

        assert!(!status.alive());
        match status {
            LeaseStatus::NotAlive { lease } => assert_eq!(lease.map(|l| l.pid), Some(999999)),
            LeaseStatus::Alive { .. } => panic!("dead pid must not read as alive"),
        }
    }

    // it("clearLease only removes a lease owned by this process")
    #[test]
    fn clear_lease_only_removes_a_lease_owned_by_this_process() {
        let (_root, lease_path) = make_lease_path();

        let now_iso = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        fs::write(
            &lease_path,
            format!(
                "{{\"heartbeatAt\":\"{now_iso}\",\"pid\":{pid},\"startedAt\":\"{now_iso}\"}}",
                pid = std::process::id() as i32 + 1
            ),
        )
        .unwrap();
        clear_lease(&lease_path);
        assert_eq!(
            read_lease(&lease_path).unwrap().pid,
            std::process::id() as i32 + 1
        );

        write_lease(&lease_path, &Utc::now).unwrap();
        // Now owned by us (write_lease stamps our pid) — clearing works.
        clear_lease(&lease_path);
        assert!(read_lease(&lease_path).is_none());
    }

    // it("treats garbage lease files as absent")
    #[test]
    fn treats_garbage_lease_files_as_absent() {
        let (_root, lease_path) = make_lease_path();

        fs::write(&lease_path, "{torn").unwrap();
        assert!(read_lease(&lease_path).is_none());
        assert!(!check_lease(&lease_path, &Utc::now).alive());
    }
}
