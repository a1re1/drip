// Process discovery for the Shells pane — which commands the focused running
// session is executing. Ported from sub-zero's ps.ts: parse `ps` output, walk
// the parent/child table to a session pid's descendants. Pure and testable;
// only list_processes touches the system.

use std::collections::{HashMap, HashSet};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PsProc {
    pub pid: i64,
    pub ppid: i64,
    /// elapsedSec?: number — None when ps printed no etime column.
    pub etime_sec: Option<i64>,
    pub command: String,
}

// Parse `ps -axo pid=,ppid=,etime=,command=` output. Each line is:
//   <pid> <ppid> <etime> <command with spaces...>
// The etime column is peeled only when the third token looks like an elapsed
// time ([[dd-]hh:]mm:ss); otherwise it's treated as part of the command, so
// output that omits etime still parses correctly.
pub fn parse_ps(output: &str) -> Vec<PsProc> {
    let mut procs: Vec<PsProc> = Vec::new();
    for raw in output.split('\n') {
        let line = raw.trim_start();
        if line.is_empty() {
            continue;
        }
        // Match ^(\d+)\s+(\d+)\s+(.*)$ — first two tokens must be integers.
        // Runs of whitespace separate the fields, so skip each run rather
        // than splitting on single spaces (column-aligned output pads with
        // multiple spaces).
        let mut s = line;
        let pid_tok = {
            s = s.trim_start();
            let end = match s.find(char::is_whitespace) {
                Some(e) if e > 0 => e,
                _ => continue,
            };
            let tok = &s[..end];
            s = &s[end..];
            tok
        };
        if !pid_tok.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let ppid_tok = {
            s = s.trim_start();
            let end = match s.find(char::is_whitespace) {
                Some(e) if e > 0 => e,
                _ => continue,
            };
            let tok = &s[..end];
            s = &s[end..];
            tok
        };
        if !ppid_tok.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let mut command = s.trim_start().to_string();
        let pid: i64 = match pid_tok.parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        let ppid: i64 = match ppid_tok.parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        let mut etime_sec: Option<i64> = None;
        // Peel the etime token when it looks like elapsed time
        // ([dd-]hh:]mm:ss): ^(non-space+)\s+(.*)$ with the first part
        // matching ^(?:\d+-)?\d+(?::\d+)+$.
        let trimmed = command.trim_start();
        let mut it = trimmed.splitn(2, char::is_whitespace);
        if let (Some(first), Some(rest)) = (it.next(), it.next()) {
            if looks_like_etime(first) {
                etime_sec = Some(parse_etime(first));
                command = rest.trim_start().to_string();
            }
        }
        procs.push(PsProc {
            pid,
            ppid,
            etime_sec,
            command,
        });
    }
    procs
}

// Check whether a token matches ^(?:\d+-)?\d+(?::\d+)+$ — an optional day
// prefix followed by at least one colon-separated component.
fn looks_like_etime(s: &str) -> bool {
    let body = match s.split_once('-') {
        Some((days, rest)) => {
            if days.is_empty() || !days.chars().all(|c| c.is_ascii_digit()) {
                return false;
            }
            if !rest.chars().all(|c| c.is_ascii_digit() || c == ':') {
                return false;
            }
            rest
        }
        None => s,
    };
    if body.is_empty() || !body.chars().all(|c| c.is_ascii_digit() || c == ':') {
        return false;
    }
    // Must have at least one ':' and only digit groups around it.
    let groups: Vec<&str> = body.split(':').collect();
    if groups.len() < 2 {
        return false;
    }
    groups.iter().all(|g| !g.is_empty() && g.chars().all(|c| c.is_ascii_digit()))
}

// Parse a `ps` etime token ([[dd-]hh:]mm:ss) into seconds.
pub fn parse_etime(s: &str) -> i64 {
    let mut days: i64 = 0;
    let mut rest = s;
    if let Some(dash) = s.find('-') {
        days = s[..dash].parse::<i64>().unwrap_or(0);
        rest = &s[dash + 1..];
    }
    let parts: Vec<i64> = rest.split(':').map(|x| x.parse::<i64>().unwrap_or(0)).collect();
    let sec = match parts.len() {
        3 => parts[0] * 3600 + parts[1] * 60 + parts[2],
        2 => parts[0] * 60 + parts[1],
        _ => parts[0],
    };
    days * 86400 + sec
}

// Walk the ps table to collect all descendants of a root pid: the shells,
// scripts, and helpers a running session has spawned.
pub fn descendants(root_pid: i64, procs: &[PsProc]) -> Vec<PsProc> {
    let mut children: HashMap<i64, Vec<&PsProc>> = HashMap::new();
    for p in procs {
        children.entry(p.ppid).or_default().push(p);
    }
    let mut out: Vec<PsProc> = Vec::new();
    let mut seen: HashSet<i64> = HashSet::new();
    seen.insert(root_pid);
    let mut stack = vec![root_pid];
    while let Some(pid) = stack.pop() {
        if let Some(children) = children.get(&pid) {
            for child in children {
                if seen.contains(&child.pid) {
                    continue;
                }
                seen.insert(child.pid);
                out.push((*child).clone());
                stack.push(child.pid);
            }
        }
    }
    out
}

/// Snapshot the live process table (pid/ppid/etime/command).
pub fn list_processes() -> Vec<PsProc> {
    match Command::new("ps").args(["-axo", "pid=,ppid=,etime=,command="]).output() {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            parse_ps(&stdout)
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_etime() {
        assert_eq!(parse_etime("05:03"), 303);
        assert_eq!(parse_etime("01:02:03"), 3723);
        assert_eq!(parse_etime("2-01:02:03"), 176523);
        assert_eq!(parse_etime("garbage"), 0);
    }

    #[test]
    fn test_parse_ps() {
        let output = "\
  PID  PPID    ELAPSED COMMAND\n\
  100     1    00:01:02 /bin/bash -l -c echo hello world\n\
  101   100       05:03 sleep 100\n";
        let procs = parse_ps(output);
        assert_eq!(procs.len(), 2);
        assert_eq!(
            procs[0],
            PsProc {
                pid: 100,
                ppid: 1,
                etime_sec: Some(62),
                command: "/bin/bash -l -c echo hello world".to_string()
            }
        );
        assert_eq!(
            procs[1],
            PsProc {
                pid: 101,
                ppid: 100,
                etime_sec: Some(303),
                command: "sleep 100".to_string()
            }
        );
    }

    #[test]
    fn test_descendants() {
        let procs = vec![
            PsProc { pid: 1, ppid: 0, etime_sec: None, command: "init".to_string() },
            PsProc { pid: 10, ppid: 1, etime_sec: None, command: "session".to_string() },
            PsProc { pid: 20, ppid: 10, etime_sec: None, command: "sh".to_string() },
            PsProc { pid: 30, ppid: 20, etime_sec: None, command: "grandchild".to_string() },
            PsProc { pid: 40, ppid: 999, etime_sec: None, command: "unrelated".to_string() },
        ];
        let kids = descendants(10, &procs);
        let pids: Vec<i64> = kids.iter().map(|p| p.pid).collect();
        assert!(pids.contains(&20));
        assert!(pids.contains(&30));
        assert!(!pids.contains(&10));
        assert!(!pids.contains(&40));
    }
}
