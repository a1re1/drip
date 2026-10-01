// Conservative, idempotent reaping of unused drip-owned tmux sessions.
//
// BASH_ASYNC runs each background command in a tmux session so the operator
// can attach to it, and `remain-on-exit on` keeps the session (with its dead
// pane) alive after the command exits. When the harness is interrupted, or a
// job is started by a run that dies, those sessions are never cleaned up and
// pile up as dangling `drip-*` sessions.
//
// Reaping is deliberately narrow. A session is killed only when every one of
// these holds:
//   * drip stamped it with its ownership marker (`@drip_owner`) when it
//     created the session — an unmarked `drip-`-prefixed session (a legacy
//     session, or one another tool owns) is never touched;
//   * a fresh server-side probe says the session exists, nobody is attached to
//     it, and every pane in it is dead (the pane command exited);
//   * either the job's exit status is already durably recorded
//     (`@drip_result` — the BASH_ASYNC waiter logged `[tmux-exit]`), or the
//     pane has been dead longer than the grace window, which covers a waiter
//     that is still about to read `#{pane_dead_status}`.
//
// Anything ambiguous — a probe that errors, a session that vanished between
// listing and probe, a live pane, an attached client, an unmarked session —
// is kept, never killed. A session with a live pane survives any number of
// reap passes, so a run ending (or the drip process dying) never cancels live
// background work.
//
// Command output is not lost: the wrapped command already tee'd every byte
// into the job log under the jobs root, so a reaped session gives up only the
// pane scrollback.

use std::process::Command;

/// tmux session option holding the ownership marker value.
pub const TMUX_OWNER_OPTION: &str = "@drip_owner";
/// tmux session option holding the result-recorded marker value.
pub const TMUX_RESULT_OPTION: &str = "@drip_result";
/// The ownership marker value drip writes when it creates a session.
pub const TMUX_OWNER_MARKER: &str = "drip-v1";
/// The marker value written once a job's exit status is durably logged.
pub const TMUX_RESULT_MARKER: &str = "recorded-v1";
/// A pane that died less than this long ago may still have a waiter reading
/// `#{pane_dead_status}`; it is never reaped inside the window.
pub const DEAD_PANE_GRACE_MS: i64 = 120_000;

/// The `tmux ls -F` format the reaper parses. `|` keeps the fields
/// unambiguous (session names are sanitized, so they never contain it).
pub const TMUX_LIST_FORMAT: &str = "#{session_name}|#{session_created}|#{session_activity}|#{session_attached}|#{@drip_owner}|#{@drip_result}";

/// One tmux session as reported by `tmux ls -F <TMUX_LIST_FORMAT>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxSessionListing {
    pub name: String,
    /// Epoch seconds the session was created.
    pub created: i64,
    /// Epoch seconds of the last activity in the session.
    pub activity: i64,
    /// Number of attached clients.
    pub attached: i64,
    /// The `@drip_owner` value (empty for legacy/foreign sessions).
    pub owner: String,
    /// The `@drip_result` value (empty until a waiter recorded the status).
    pub result: String,
}

impl TmuxSessionListing {
    pub fn owned_by_drip(&self) -> bool {
        self.owner == TMUX_OWNER_MARKER
    }
}

/// An EXACT tmux target for one session: tmux resolves `-t <name>` by prefix
/// match, so a bare name like `drip-abc` also matches a live session called
/// `drip-abc12`. Every probe, option write and kill therefore addresses the
/// session by its exact name (`=`) and, where a target may name a pane or a
/// window (`display-message`, `list-panes -s`, `set-option`), also pins the
/// default window (`:`), which tmux does not prefix-match.
pub fn exact_session_target(session_name: &str) -> String {
    format!("={session_name}:")
}

/// An EXACT tmux session id (`=NAME`), the form `kill-session` and
/// `has-session` take. These commands address a session, so they must not
/// carry a window suffix.
pub fn exact_session_id(session_name: &str) -> String {
    format!("={session_name}")
}

/// A fresh server-side probe of one session, taken right before any kill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxProbe {
    /// False when the session is gone (already reaped or exited).
    pub exists: bool,
    /// True when `#{session_attached}` is 0.
    pub detached: bool,
    /// True when the session has at least one pane and every pane is dead.
    pub all_panes_dead: bool,
    /// Epoch seconds of the most recent pane death, when tmux reports one.
    pub last_pane_death: Option<i64>,
    /// A tmux invocation that failed in a way that is not "session missing".
    pub error: Option<String>,
}

impl TmuxProbe {
    pub fn missing() -> Self {
        TmuxProbe {
            exists: false,
            detached: false,
            all_panes_dead: false,
            last_pane_death: None,
            error: None,
        }
    }

    pub fn failed(error: String) -> Self {
        TmuxProbe {
            exists: true,
            detached: false,
            all_panes_dead: false,
            last_pane_death: None,
            error: Some(error),
        }
    }
}

/// Why a session was kept. Every variant is a fail-closed outcome: an
/// ambiguous session is retained rather than killed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepReason {
    /// No `@drip_owner` marker and no `drip-` prefix — not ours at all.
    NotDripOwned,
    /// `drip-`-prefixed but unmarked: created before ownership stamping (or by
    /// another tool). Kept rather than guessed at.
    LegacyUnmarked,
    /// A client is attached (or the probe could not prove it is detached).
    Attached,
    /// The session still has a live pane: a running background job.
    PanesAlive,
    /// The pane died too recently for the waiter to have recorded the status.
    TooYoung,
    /// The probe itself failed — never kill on an unreadable session.
    ProbeFailed,
    /// The session disappeared between listing and probe; nothing to do.
    Vanished,
    /// `tmux kill-session` returned non-zero; the session may still exist.
    KillFailed,
}

impl KeepReason {
    pub fn as_str(self) -> &'static str {
        match self {
            KeepReason::NotDripOwned => "not-drip-owned",
            KeepReason::LegacyUnmarked => "legacy-unmarked",
            KeepReason::Attached => "attached",
            KeepReason::PanesAlive => "panes-alive",
            KeepReason::TooYoung => "pane-death-too-recent",
            KeepReason::ProbeFailed => "probe-failed",
            KeepReason::Vanished => "vanished",
            KeepReason::KillFailed => "kill-failed",
        }
    }
}

/// The eligibility verdict for one session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReapDecision {
    Kill,
    Keep(KeepReason),
}

/// The knobs a reap pass runs with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReapPolicy {
    /// A pane dead for less than this is never reaped unless its result is
    /// already recorded.
    pub dead_pane_grace_ms: i64,
}

impl Default for ReapPolicy {
    fn default() -> Self {
        ReapPolicy {
            dead_pane_grace_ms: DEAD_PANE_GRACE_MS,
        }
    }
}

/// The outcome of one reap pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReapResult {
    /// Sessions killed (or, in a dry run, sessions that would be killed).
    pub killed: Vec<String>,
    /// Sessions left alone.
    pub kept: Vec<String>,
    /// `(session, reason)` for every kept session, in listing order.
    pub kept_reasons: Vec<(String, String)>,
    /// True when the pass only reported eligibility.
    pub dry_run: bool,
}

impl ReapResult {
    fn keep(&mut self, name: &str, reason: KeepReason) {
        self.kept.push(name.to_string());
        self.kept_reasons
            .push((name.to_string(), reason.as_str().to_string()));
    }
}

/// Parses `tmux ls -F <TMUX_LIST_FORMAT>` output. Lines that do not carry at
/// least a name are skipped; missing trailing fields default to empty/zero so
/// a legacy server (which has no such user options) still parses.
pub fn parse_session_listing(listing: &str) -> Vec<TmuxSessionListing> {
    let mut sessions = Vec::new();

    for line in listing.lines() {
        let line = line.trim_end();

        if line.trim().is_empty() {
            continue;
        }

        let mut fields = line.split('|');
        let name = fields.next().unwrap_or("").trim().to_string();

        if name.is_empty() {
            continue;
        }

        let number_field = |raw: Option<&str>| -> i64 {
            raw.map(str::trim)
                .and_then(|value| value.parse::<i64>().ok())
                .unwrap_or(0)
        };

        let created = number_field(fields.next());
        let activity = number_field(fields.next());
        let attached = number_field(fields.next());
        let owner = fields.next().unwrap_or("").trim().to_string();
        let result = fields.next().unwrap_or("").trim().to_string();

        sessions.push(TmuxSessionListing {
            name,
            created,
            activity,
            attached,
            owner,
            result,
        });
    }

    sessions
}

/// Decides whether one probed session may be killed. Pure, so the whole safety
/// matrix is unit-testable without a tmux server.
pub fn decide_reap(
    session: &TmuxSessionListing,
    probe: &TmuxProbe,
    now_ms: i64,
    policy: ReapPolicy,
) -> ReapDecision {
    if probe.error.is_some() {
        return ReapDecision::Keep(KeepReason::ProbeFailed);
    }

    if !probe.exists {
        return ReapDecision::Keep(KeepReason::Vanished);
    }

    if !session.owned_by_drip() {
        return ReapDecision::Keep(if session.owner.is_empty() {
            KeepReason::LegacyUnmarked
        } else {
            KeepReason::NotDripOwned
        });
    }

    if session.attached > 0 || !probe.detached {
        return ReapDecision::Keep(KeepReason::Attached);
    }

    if !probe.all_panes_dead {
        return ReapDecision::Keep(KeepReason::PanesAlive);
    }

    // A recorded result means a waiter already logged the exit status, so the
    // dead session can go immediately. Otherwise wait out the grace window that
    // covers a waiter which is still about to read #{pane_dead_status}.
    if session.result == TMUX_RESULT_MARKER {
        return ReapDecision::Kill;
    }

    match probe.last_pane_death {
        Some(dead_at) => {
            if now_ms - dead_at.saturating_mul(1000) <= policy.dead_pane_grace_ms {
                ReapDecision::Keep(KeepReason::TooYoung)
            } else {
                ReapDecision::Kill
            }
        }
        // A dead pane with no reported death time cannot be aged, so it is
        // left alone rather than killed on an unverifiable assumption.
        None => ReapDecision::Keep(KeepReason::TooYoung),
    }
}

/// Lists the sessions of the default tmux server. A missing tmux binary or a
/// server with no sessions is an empty list, not an error.
pub fn list_tmux_sessions() -> Result<Vec<TmuxSessionListing>, String> {
    let output = match Command::new("tmux")
        .args(["ls", "-F", TMUX_LIST_FORMAT])
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            // `NotFound` means tmux is not installed: there is nothing to reap.
            if error.kind() == std::io::ErrorKind::NotFound {
                return Ok(Vec::new());
            }

            return Err(format!("Unable to run tmux: {error}"));
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);

        // "no server running" is the ordinary empty case.
        if stderr.contains("no server running") || stderr.contains("no sessions") {
            return Ok(Vec::new());
        }

        return Err(format!("Unable to list tmux sessions: {}", stderr.trim()));
    }

    Ok(parse_session_listing(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

/// Probes one session immediately before a kill. Any unexpected tmux failure
/// comes back as `error`, which `decide_reap` turns into a keep.
pub fn probe_tmux_session(session_name: &str) -> TmuxProbe {
    let target = exact_session_target(session_name);
    let info = Command::new("tmux")
        .args([
            "display-message",
            "-p",
            "-t",
            target.as_str(),
            "#{session_attached}",
        ])
        .output();

    let output = match info {
        Ok(output) => output,
        Err(error) => return TmuxProbe::failed(format!("tmux display-message failed: {error}")),
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);

        if stderr.contains("can't find session") || stderr.contains("no server running") {
            return TmuxProbe::missing();
        }

        return TmuxProbe::failed(format!(
            "tmux display-message failed for \"{session_name}\": {}",
            stderr.trim()
        ));
    }

    let attached = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<i64>()
        .unwrap_or(0);

    // `-s` lists the panes of EVERY window in the session, so a session with
    // a second window (or a second pane) that is still alive is never mistaken
    // for a fully dead one.
    let panes = Command::new("tmux")
        .args([
            "list-panes",
            "-s",
            "-t",
            target.as_str(),
            "-F",
            "#{pane_dead}|#{pane_dead_time}",
        ])
        .output();

    let panes = match panes {
        Ok(output) => output,
        Err(error) => return TmuxProbe::failed(format!("tmux list-panes failed: {error}")),
    };

    if !panes.status.success() {
        let stderr = String::from_utf8_lossy(&panes.stderr);

        if stderr.contains("can't find session") || stderr.contains("no server running") {
            return TmuxProbe::missing();
        }

        return TmuxProbe::failed(format!(
            "tmux list-panes failed for \"{session_name}\": {}",
            stderr.trim()
        ));
    }

    let stdout = String::from_utf8_lossy(&panes.stdout);
    let mut pane_count = 0usize;
    let mut dead_count = 0usize;
    let mut last_pane_death: Option<i64> = None;

    for line in stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        pane_count += 1;

        let mut fields = line.split('|');
        let dead = fields.next().unwrap_or("").trim() == "1";
        let dead_time = fields
            .next()
            .map(str::trim)
            .and_then(|value| value.parse::<i64>().ok());

        if dead {
            dead_count += 1;

            if let Some(dead_time) = dead_time {
                last_pane_death = Some(match last_pane_death {
                    Some(previous) => previous.max(dead_time),
                    None => dead_time,
                });
            }
        }
    }

    TmuxProbe {
        exists: true,
        detached: attached == 0,
        all_panes_dead: pane_count > 0 && dead_count == pane_count,
        last_pane_death,
        error: None,
    }
}

/// One reap pass over a known listing with injected probe/kill callbacks — the
/// seam the unit tests use. Nothing is killed twice: each listing entry is
/// judged exactly once, and a successful kill removes the session from the
/// server, so a following pass finds no session to kill.
pub fn reap_with(
    sessions: &[TmuxSessionListing],
    now_ms: i64,
    dry_run: bool,
    policy: ReapPolicy,
    probe: &dyn Fn(&str) -> TmuxProbe,
    kill_session: &dyn Fn(&str) -> Result<(), String>,
) -> ReapResult {
    let mut result = ReapResult {
        dry_run,
        ..ReapResult::default()
    };

    for session in sessions {
        let probed = probe(&session.name);

        match decide_reap(session, &probed, now_ms, policy) {
            ReapDecision::Keep(reason) => result.keep(&session.name, reason),
            ReapDecision::Kill => {
                if dry_run {
                    result.killed.push(session.name.clone());
                    continue;
                }

                match kill_session(&session.name) {
                    Ok(()) => result.killed.push(session.name.clone()),
                    Err(_) => result.keep(&session.name, KeepReason::KillFailed),
                }
            }
        }
    }

    result
}

/// Lists, probes and reaps the unused drip-owned tmux sessions of the default
/// server. Safe to run ad hoc, from `--gc`, or from cron.
pub fn reap_tmux_sessions(dry_run: bool) -> Result<ReapResult, String> {
    reap_tmux_sessions_with_policy(dry_run, ReapPolicy::default())
}

pub fn reap_tmux_sessions_with_policy(
    dry_run: bool,
    policy: ReapPolicy,
) -> Result<ReapResult, String> {
    let sessions = list_tmux_sessions()?;

    Ok(reap_with(
        &sessions,
        now_epoch_ms(),
        dry_run,
        policy,
        &probe_tmux_session,
        &|name: &str| {
            crate::tools::builtin::bash::kill_tmux_session(name).map_err(|error| error.to_string())
        },
    ))
}

/// Wall-clock milliseconds since the epoch.
pub fn now_epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// Stamps a freshly created tmux session as drip-owned. Failure is reported to
/// the caller, which decides whether the session may still be used.
pub fn stamp_tmux_owner(session_name: &str) -> Result<(), String> {
    set_session_option(session_name, TMUX_OWNER_OPTION, TMUX_OWNER_MARKER)
}

/// Marks a session's exit status as durably recorded, which lets the reaper
/// collect the dead session without waiting out the grace window.
pub fn mark_result_recorded(session_name: &str) {
    let _ = set_session_option(session_name, TMUX_RESULT_OPTION, TMUX_RESULT_MARKER);
}

fn set_session_option(session_name: &str, option: &str, value: &str) -> Result<(), String> {
    let target = exact_session_target(session_name);
    let output = Command::new("tmux")
        .args(["set-option", "-t", target.as_str(), option, value])
        .output()
        .map_err(|error| format!("Unable to run tmux set-option: {error}"))?;

    if !output.status.success() {
        return Err(format!(
            "Unable to set {option} on tmux session \"{session_name}\": {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn listing(name: &str, owner: &str, result: &str, attached: i64) -> TmuxSessionListing {
        TmuxSessionListing {
            name: name.to_string(),
            created: 1_000,
            activity: 2_000,
            attached,
            owner: owner.to_string(),
            result: result.to_string(),
        }
    }

    fn owned(name: &str, result: &str) -> TmuxSessionListing {
        listing(name, TMUX_OWNER_MARKER, result, 0)
    }

    fn dead_probe(at_seconds: i64) -> TmuxProbe {
        TmuxProbe {
            exists: true,
            detached: true,
            all_panes_dead: true,
            last_pane_death: Some(at_seconds),
            error: None,
        }
    }

    #[test]
    fn parse_reads_marked_lines_and_tolerates_missing_trailing_fields() {
        let listing_text = "drip-a|1000|1000|0|drip-v1|recorded-v1\ndrip-legacy|2000|2000|0\n";

        let sessions = parse_session_listing(listing_text);

        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].owner, "drip-v1");
        assert_eq!(sessions[0].result, "recorded-v1");
        assert!(sessions[0].owned_by_drip());
        assert_eq!(sessions[1].owner, "");
        assert_eq!(sessions[1].result, "");
        assert!(!sessions[1].owned_by_drip());
    }

    #[test]
    fn decide_never_kills_unowned_legacy_attached_live_or_unprobeable_sessions() {
        let policy = ReapPolicy::default();
        let now = 10_000_000;

        assert_eq!(
            decide_reap(&listing("scratch", "", "", 0), &dead_probe(1), now, policy),
            ReapDecision::Keep(KeepReason::LegacyUnmarked)
        );
        assert_eq!(
            decide_reap(
                &listing("other-tool", "someone-else", "", 0),
                &dead_probe(1),
                now,
                policy
            ),
            ReapDecision::Keep(KeepReason::NotDripOwned)
        );
        assert_eq!(
            decide_reap(
                &listing("drip-a", TMUX_OWNER_MARKER, "", 1),
                &dead_probe(1),
                now,
                policy
            ),
            ReapDecision::Keep(KeepReason::Attached)
        );

        let mut attached_probe = dead_probe(1);
        attached_probe.detached = false;
        assert_eq!(
            decide_reap(
                &owned("drip-a", TMUX_RESULT_MARKER),
                &attached_probe,
                now,
                policy
            ),
            ReapDecision::Keep(KeepReason::Attached)
        );

        let mut live_probe = dead_probe(1);
        live_probe.all_panes_dead = false;
        assert_eq!(
            decide_reap(
                &owned("drip-a", TMUX_RESULT_MARKER),
                &live_probe,
                now,
                policy
            ),
            ReapDecision::Keep(KeepReason::PanesAlive)
        );

        assert_eq!(
            decide_reap(
                &owned("drip-a", TMUX_RESULT_MARKER),
                &TmuxProbe::failed("boom".into()),
                now,
                policy
            ),
            ReapDecision::Keep(KeepReason::ProbeFailed)
        );
        assert_eq!(
            decide_reap(
                &owned("drip-a", TMUX_RESULT_MARKER),
                &TmuxProbe::missing(),
                now,
                policy
            ),
            ReapDecision::Keep(KeepReason::Vanished)
        );
    }

    #[test]
    fn decide_kills_only_recorded_or_long_dead_owned_sessions() {
        let policy = ReapPolicy::default();
        let now = 10_000_000;

        // Recorded result: reaped without waiting out the grace window.
        assert_eq!(
            decide_reap(
                &owned("drip-a", TMUX_RESULT_MARKER),
                &dead_probe(9_999),
                now,
                policy
            ),
            ReapDecision::Kill
        );

        // Unrecorded result inside the grace window: kept for the waiter.
        assert_eq!(
            decide_reap(&owned("drip-b", ""), &dead_probe(9_999), now, policy),
            ReapDecision::Keep(KeepReason::TooYoung)
        );

        // Unrecorded result past the grace window: the waiter is gone.
        let old_enough = now / 1000 - (DEAD_PANE_GRACE_MS / 1000) - 1;
        assert_eq!(
            decide_reap(&owned("drip-c", ""), &dead_probe(old_enough), now, policy),
            ReapDecision::Kill
        );

        // A dead pane with no reported death time is never aged out.
        let mut no_death_time = dead_probe(0);
        no_death_time.last_pane_death = None;
        assert_eq!(
            decide_reap(&owned("drip-d", ""), &no_death_time, now, policy),
            ReapDecision::Keep(KeepReason::TooYoung)
        );
    }

    #[test]
    fn exact_targets_pin_the_session_name_and_default_window() {
        // A bare name is prefix-matched by tmux; the reaper must never rely
        // on that (session `drip-abc` would otherwise address `drip-abc12`).
        assert_eq!(exact_session_target("drip-abc"), "=drip-abc:");
        assert_eq!(exact_session_id("drip-abc"), "=drip-abc");
        assert_ne!(exact_session_id("drip-abc"), "drip-abc");
    }

    #[test]
    fn reap_with_kills_only_eligible_sessions_and_keeps_the_rest() {
        let now = 10_000_000;
        let old_enough = now / 1000 - (DEAD_PANE_GRACE_MS / 1000) - 1;
        let sessions = vec![
            owned("drip-dead-recorded", TMUX_RESULT_MARKER),
            owned("drip-dead-aged", ""),
            owned("drip-running", ""),
            owned("drip-fresh", ""),
            listing("drip-legacy", "", "", 0),
            listing("unrelated", "", "", 0),
        ];

        let killed: RefCell<Vec<String>> = RefCell::new(Vec::new());
        let probe = |name: &str| -> TmuxProbe {
            match name {
                "drip-running" => {
                    let mut probed = dead_probe(old_enough);
                    probed.all_panes_dead = false;
                    probed
                }
                "drip-dead-aged" | "drip-legacy" | "unrelated" => dead_probe(old_enough),
                "drip-fresh" => dead_probe(now / 1000),
                _ => dead_probe(9_999),
            }
        };
        let kill = |name: &str| -> Result<(), String> {
            killed.borrow_mut().push(name.to_string());
            Ok(())
        };

        let result = reap_with(&sessions, now, false, ReapPolicy::default(), &probe, &kill);

        assert_eq!(
            result.killed,
            vec![
                "drip-dead-recorded".to_string(),
                "drip-dead-aged".to_string()
            ]
        );
        assert_eq!(killed.borrow().len(), 2);
        assert!(result.kept.contains(&"drip-running".to_string()));
        assert!(result.kept.contains(&"drip-legacy".to_string()));
        assert!(result.kept.contains(&"unrelated".to_string()));
        assert!(result
            .kept_reasons
            .contains(&("drip-running".to_string(), "panes-alive".to_string())));

        // Idempotence: a second pass over the survivors (the killed sessions
        // are gone from the server) kills nothing new.
        let survivors: Vec<TmuxSessionListing> = sessions
            .into_iter()
            .filter(|session| !result.killed.contains(&session.name))
            .collect();
        let second = reap_with(&survivors, now, false, ReapPolicy::default(), &probe, &kill);

        assert!(second.killed.is_empty());
        assert_eq!(killed.borrow().len(), 2);
    }

    #[test]
    fn reap_with_dry_run_reports_eligibility_without_killing() {
        let now = 10_000_000;
        let sessions = vec![owned("drip-dead", TMUX_RESULT_MARKER)];
        let kill_calls = RefCell::new(0usize);
        let kill = |_: &str| -> Result<(), String> {
            *kill_calls.borrow_mut() += 1;
            Ok(())
        };

        let result = reap_with(
            &sessions,
            now,
            true,
            ReapPolicy::default(),
            &|_: &str| dead_probe(1),
            &kill,
        );

        assert_eq!(result.killed, vec!["drip-dead".to_string()]);
        assert!(result.dry_run);
        assert_eq!(*kill_calls.borrow(), 0);
    }

    #[test]
    fn a_failed_kill_keeps_the_session_and_is_reported() {
        let now = 10_000_000;
        let sessions = vec![owned("drip-dead", TMUX_RESULT_MARKER)];

        let result = reap_with(
            &sessions,
            now,
            false,
            ReapPolicy::default(),
            &|_: &str| dead_probe(1),
            &|_: &str| Err("tmux exploded".to_string()),
        );

        assert!(result.killed.is_empty());
        assert_eq!(
            result.kept_reasons,
            vec![("drip-dead".to_string(), "kill-failed".to_string())]
        );
    }
}

/// Reaps one named session immediately, but only when the same conservative
/// policy a reap pass uses says it is unused. `Ok(false)` means the session
/// was kept (live pane, attached client, unmarked, too fresh, vanished, probe
/// failed) and is therefore still there; the next ad-hoc, `--gc` or cron pass
/// judges it again.
///
/// The BASH_ASYNC waiter calls this once a job's exit status is durable: a
/// session nobody is attached to is collected right away, while a session an
/// operator is watching keeps its pane until they detach.
pub fn reap_session_if_eligible(session_name: &str) -> Result<bool, String> {
    let sessions = list_tmux_sessions()?;

    let Some(session) = sessions.iter().find(|session| session.name == session_name) else {
        return Ok(false);
    };

    let probe = probe_tmux_session(session_name);

    match decide_reap(session, &probe, now_epoch_ms(), ReapPolicy::default()) {
        ReapDecision::Keep(_) => Ok(false),
        ReapDecision::Kill => {
            crate::tools::builtin::bash::kill_tmux_session(session_name)
                .map_err(|error| error.to_string())?;

            Ok(true)
        }
    }
}
