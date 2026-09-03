// Shell stdout/stderr discovery + tailing for the drilled-in Shells view.
// A shell process has no transcript to
// tail, but its stdout/stderr are often redirected to files — `lsof -p <pid>`
// names the file behind each open fd (there is no /proc/<pid>/fd on macOS),
// so we discover fds 1/2 and tail the regular files behind them. Pure parsing
// and the byte-offset follower are testable; only read_shell_log_files touches
// the system.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::process::Command;

// One parsed row of `lsof -p <pid>`, whose columns are:
//   COMMAND PID USER FD TYPE DEVICE SIZE/OFF NODE NAME...
// Only FD/TYPE/DEVICE/NODE/NAME matter here; NAME (a path) can contain spaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LsofFd {
    pub fd: String,     // e.g. "1w", "2w", "cwd", "txt", "3u"
    pub type_: String,  // REG, CHR, PIPE, KQUEUE, ...
    pub device: String, // e.g. "1,17"
    pub node: String,   // inode
    pub name: String,   // path (for REG) — may contain spaces
}

pub fn parse_lsof(output: &str) -> Vec<LsofFd> {
    let mut out: Vec<LsofFd> = Vec::new();
    for raw in output.split('\n') {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let t: Vec<&str> = line.split_whitespace().collect();
        // Need at least through NODE plus one NAME token. The header row (which
        // starts with the literal "COMMAND") has no numeric device/node and is
        // dropped naturally by the REG/fd filters downstream, but guard length too.
        if t.len() < 9 {
            continue;
        }
        out.push(LsofFd {
            fd: t[3].to_string(),
            type_: t[4].to_string(),
            device: t[5].to_string(),
            node: t[7].to_string(),
            name: t[8..].join(" "),
        });
    }
    out
}

// The leading run of digits of a fd field, e.g. "1w" -> "1", "3u" -> "3".
// Non-numeric fd fields ("cwd", "txt") yield an empty string.
fn fd_number(fd: &str) -> String {
    fd.chars().take_while(|c| c.is_ascii_digit()).collect()
}

// The regular files behind fd 1 (stdout) then fd 2 (stderr), de-duplicated by
// inode so a process that points both fds at the same file is tailed once.
// Non-REG fds (ttys, pipes, /dev/null) can't be tailed and are dropped.
pub fn stdout_stderr_files(fds: &[LsofFd]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut files: Vec<String> = Vec::new();
    for want in ["1", "2"] {
        for f in fds {
            if f.type_ != "REG" {
                continue;
            }
            if fd_number(&f.fd) != want {
                continue;
            }
            let key = format!("{}:{}", f.device, f.node);
            if !seen.insert(key) {
                continue;
            }
            files.push(f.name.clone());
        }
    }
    files
}

// Discover the tailable stdout/stderr files for a live pid via lsof. Read-only
// and best-effort. lsof routinely exits non-zero on macOS when it can't stat
// every fd (partial-stat), yet still prints the rows we want to stdout — so we
// read the captured stdout even when the exit status is non-zero rather than
// discarding a good result.
pub fn read_shell_log_files(pid: i64) -> Vec<String> {
    // Capture stdout and drop lsof's stderr warnings.
    match Command::new("lsof")
        .args(["-p", &pid.to_string()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
    {
        Ok(out) => stdout_stderr_files(&parse_lsof(&String::from_utf8_lossy(&out.stdout))),
        Err(_) => Vec::new(), // spawn failed — nothing to salvage
    }
}

// Read exactly buf.len() bytes at `offset` (read may return short).
fn read_full(file: &mut File, buf: &mut [u8], offset: u64) {
    let mut done = 0usize;
    while done < buf.len() {
        if file.seek(SeekFrom::Start(offset + done as u64)).is_err() {
            break;
        }
        match file.read(&mut buf[done..]) {
            Ok(0) => break,
            Ok(n) => done += n,
            Err(_) => break,
        }
    }
}

// A byte-offset follower that yields whole appended LINES of text. The
// trailing partial line is buffered as BYTES — not a decoded string — so a
// multibyte UTF-8 character split across two poll reads survives whole rather
// than decoding into replacement chars. Resets when the file shrinks
// (truncation / rotation). `skip_to_newline` discards
// bytes up to the first newline before emitting anything, used to skip an
// unrecoverable mid-line head left by a bounded seed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineFollower {
    pub path: String,
    pub(crate) offset: u64,
    pub(crate) buffer: Vec<u8>, // leftover partial line
    pub(crate) skip_to_newline: bool,
}

impl LineFollower {
    /// New follower starting at offset 0 (the common case).
    pub fn new(path: &str) -> LineFollower {
        LineFollower::with_start(path, 0, false)
    }

    /// Full constructor.
    pub fn with_start(path: &str, start_offset: u64, skip_to_newline: bool) -> LineFollower {
        LineFollower {
            path: path.to_string(),
            offset: start_offset,
            buffer: Vec::new(),
            skip_to_newline,
        }
    }

    pub fn poll(&mut self) -> Vec<String> {
        let size: u64 = match std::fs::metadata(&self.path).map(|m| m.len()) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        if size < self.offset {
            // File shrank: truncation or rotation. Re-read from the start.
            self.offset = 0;
            self.buffer = Vec::new();
            self.skip_to_newline = false;
        }
        if size == self.offset {
            return Vec::new();
        }
        let len = (size - self.offset) as usize;
        let mut chunk = vec![0u8; len];
        match File::open(&self.path) {
            Ok(mut file) => read_full(&mut file, &mut chunk, self.offset),
            Err(_) => return Vec::new(),
        }
        self.offset = size;
        self.consume(chunk)
    }

    fn consume(&mut self, chunk: Vec<u8>) -> Vec<String> {
        let mut buf = std::mem::take(&mut self.buffer);
        buf.extend_from_slice(&chunk);
        let mut from = 0usize;
        if self.skip_to_newline {
            match find_byte(&buf, 0x0a, 0) {
                None => {
                    // whole buffer is still the unfinished head — keep waiting
                    self.buffer = buf;
                    return Vec::new();
                }
                Some(nl) => {
                    from = nl + 1;
                    self.skip_to_newline = false;
                }
            }
        }

        let mut lines: Vec<String> = Vec::new();
        let mut line_start = from;
        for i in from..buf.len() {
            if buf[i] == 0x0a {
                lines.push(decode_line(&buf, line_start, i));
                line_start = i + 1;
            }
        }
        self.buffer = if line_start < buf.len() {
            buf[line_start..].to_vec()
        } else {
            Vec::new()
        };
        lines
    }
}

fn find_byte(buf: &[u8], byte: u8, from: usize) -> Option<usize> {
    (from..buf.len()).find(|&i| buf[i] == byte)
}

fn rfind_byte(buf: &[u8], byte: u8) -> Option<usize> {
    (0..buf.len()).rev().find(|&i| buf[i] == byte)
}

// Decode one line [start, end), trimming a trailing \r so CRLF logs read clean.
fn decode_line(buf: &[u8], start: usize, end: usize) -> String {
    let mut e = end;
    if e > start && buf[e - 1] == 0x0d {
        e -= 1;
    }
    String::from_utf8_lossy(&buf[start..e]).into_owned()
}

pub const SHELL_TAIL_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellSeed {
    pub lines: Vec<String>,
    pub followers: Vec<LineFollower>,
}

// Seed the shell-log view from the TAIL of each discovered file, returning the
// complete lines plus a follower per file primed to pick up further appends. A
// file's in-progress last line (no trailing newline) is left for the follower,
// so it surfaces once whole rather than as a truncated fragment now.
pub fn seed_shell_log(files: &[String], max_bytes: u64) -> ShellSeed {
    let mut lines: Vec<String> = Vec::new();
    let mut followers: Vec<LineFollower> = Vec::new();
    for path in files {
        let size: u64 = match std::fs::metadata(path).map(|m| m.len()) {
            Ok(s) => s,
            Err(_) => continue, // unreadable — skip this file entirely
        };
        let start = if size > max_bytes { size - max_bytes } else { 0 };
        let len = size - start;
        let follower = if len > 0 {
            let mut buf = vec![0u8; len as usize];
            match File::open(path) {
                Ok(mut file) => {
                    read_full(&mut file, &mut buf, start);
                    // Split on the LAST newline byte: everything before it is
                    // complete lines; the remainder is an in-progress line the
                    // follower will read whole later.
                    match rfind_byte(&buf, 0x0a) {
                        Some(last_nl) => {
                            let text =
                                String::from_utf8_lossy(&buf[..last_nl]).into_owned();
                            let mut parts: Vec<String> =
                                text.split('\n').map(|s| s.to_string()).collect();
                            // Note: unlike JS, Rust's split never yields a
                            // trailing "", and unlike JS's split on "" it
                            // yields no leading "" either — "" has no parts.
                            // (JS: "".split("\n") === [""]; that case only
                            // arises when lastNl === 0, i.e. the window starts
                            // with '\n', where JS emits one empty line. Emit
                            // it too for parity.)
                            if text.is_empty() {
                                parts = vec![String::new()];
                            }
                            // Drop a partial first line only when we started
                            // mid-file.
                            if start > 0 && !parts.is_empty() {
                                parts.remove(0);
                            }
                            for l in parts {
                                lines.push(l);
                            }
                            let mut f = LineFollower::new(path);
                            f.offset = start + last_nl as u64 + 1;
                            f
                        }
                        None => {
                            if start > 0 {
                                // The whole bounded window is one over-long line
                                // whose head we truncated off — unrecoverable.
                                // Follow from EOF and skip to the next newline so
                                // the next surfaced line is a complete one.
                                LineFollower::with_start(path, size, true)
                            } else {
                                // Small file that's all one unfinished line —
                                // follow from the start so the follower surfaces
                                // it whole once its newline lands.
                                LineFollower::with_start(path, 0, false)
                            }
                        }
                    }
                }
                Err(_) => LineFollower::with_start(path, size, false),
            }
        } else {
            LineFollower::with_start(path, size, false)
        };
        followers.push(follower);
    }
    ShellSeed { lines, followers }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const SAMPLE_LSOF: &str = "\
COMMAND   PID  USER   FD   TYPE   DEVICE SIZE/OFF NODE NAME
zsh      4242  tyler  cwd      DIR   1,17    1024    2 /Users/tyler
zsh      4242  tyler    0u     CHR   16,4     0t0 1234 /dev/ttys004
zsh      4242  tyler    1w     REG   1,17     512   99 /tmp/log with space.txt
zsh      4242  tyler    2w     REG   1,17     512   99 /tmp/log with space.txt
zsh      4242  tyler    3u    PIPE   0x9ab      0    0 ->somepipe
";

    #[test]
    fn parse_lsof_sample() {
        let fds = parse_lsof(SAMPLE_LSOF);
        // The header row passes the >=9 guard and is kept (it is filtered
        // later by the REG/fd checks).
        assert_eq!(fds.len(), 6);
        assert_eq!(fds[1].fd, "cwd");
        assert_eq!(fds[1].type_, "DIR");
        assert_eq!(fds[3].fd, "1w");
        assert_eq!(fds[3].type_, "REG");
        assert_eq!(fds[3].device, "1,17");
        assert_eq!(fds[3].node, "99");
        assert_eq!(fds[3].name, "/tmp/log with space.txt");
        assert_eq!(fds[5].fd, "3u");
        assert_eq!(fds[5].type_, "PIPE");
    }

    #[test]
    fn stdout_stderr_files_filters_and_dedups() {
        let fds = parse_lsof(SAMPLE_LSOF);
        // fds 1 and 2 point at the same file: one entry, in fd order. The
        // tty, cwd, and pipe rows are all dropped.
        assert_eq!(
            stdout_stderr_files(&fds),
            vec!["/tmp/log with space.txt".to_string()]
        );
    }

    #[test]
    fn stdout_stderr_files_distinguishes_two_files() {
        let out = "\
COMMAND PID USER FD TYPE DEVICE SIZE/OFF NODE NAME
a 1 u 1w REG 1,17 1 11 /tmp/a
a 1 u 2w REG 1,17 2 22 /tmp/b
";
        let fds = parse_lsof(out);
        assert_eq!(
            stdout_stderr_files(&fds),
            vec!["/tmp/a".to_string(), "/tmp/b".to_string()]
        );
    }

    #[test]
    fn line_follower_lines_appends_and_partial_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.txt");
        std::fs::write(&path, b"one\ntwo\n").unwrap();
        let path = path.to_str().unwrap();

        // Lines already present when the first poll happens are emitted once.
        let mut f = LineFollower::new(&path);
        assert_eq!(f.poll(), vec!["one".to_string(), "two".to_string()]);
        assert_eq!(f.poll(), Vec::<String>::new()); // no new data

        // A partial line is withheld until its newline arrives.
        let mut app = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        app.write_all(b"thr").unwrap();
        drop(app);
        assert_eq!(f.poll(), Vec::<String>::new());

        app = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        app.write_all(b"ee\nfour\n").unwrap();
        drop(app);
        assert_eq!(
            f.poll(),
            vec!["three".to_string(), "four".to_string()]
        );
        assert_eq!(f.poll(), Vec::<String>::new());
    }

    #[test]
    fn line_follower_survives_file_shrink() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.txt");
        std::fs::write(&path, b"one\ntwo\n").unwrap();
        let path = path.to_str().unwrap();
        let mut f = LineFollower::new(&path);
        assert_eq!(f.poll(), vec!["one".to_string(), "two".to_string()]);

        // Truncate back to empty, then append fresh lines.
        std::fs::write(&path, b"").unwrap();
        assert_eq!(f.poll(), Vec::<String>::new());
        std::fs::write(&path, b"new\n").unwrap();
        assert_eq!(f.poll(), vec!["new".to_string()]);
    }

    #[test]
    fn seed_shell_log_tail_skips_partial_first_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.txt");
        let big = format!("{}one\ntwo\nthr", "a".repeat(40));
        std::fs::write(&path, big.as_bytes()).unwrap();
        let path = path.to_str().unwrap().to_string();

        let seed = seed_shell_log(&[path.clone()], 16);
        // Window is the last 16 bytes: "aaaaaone\ntwo\nthr" — the partial
        // first line "aaaaaone" is dropped, "two" is complete.
        assert_eq!(seed.lines, vec!["two".to_string()]);

        // The in-progress "thr" is still in the file, not yet reported.
        let mut f = seed.followers.into_iter().next().unwrap();
        assert_eq!(f.poll(), Vec::<String>::new());
        let mut app = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        app.write_all(b"ee\n").unwrap();
        drop(app);
        assert_eq!(f.poll(), vec!["three".to_string()]);
    }

    #[test]
    fn seed_shell_log_small_file_keeps_unfinished_line_for_follower() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.txt");
        std::fs::write(&path, b"partial line").unwrap();
        let path = path.to_str().unwrap().to_string();

        let seed = seed_shell_log(&[path.clone()], 16);
        assert!(seed.lines.is_empty());
        let mut f = seed.followers.into_iter().next().unwrap();
        let mut app = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        app.write_all(b" done\n").unwrap();
        drop(app);
        assert_eq!(f.poll(), vec!["partial line done".to_string()]);
    }
}
