//! The startup message: an operator-editable shell script in the drip home
//! whose output is shown once when an interactive session opens.
//!
//! `~/.drip/startup-message.sh` is drip's own file (nothing here ever reads or
//! executes `~/.claude` settings). It is seeded once with a rainbow
//! water-droplet mascot and a single info line — drip's version, the model
//! profile chosen for startup, and the working directory — and the operator may
//! rewrite it however they like, or delete it to get no banner at all.
//!
//! The script runs like a status-line command: a bounded child process, with
//! the run's facts on stdin as one JSON object and the same facts exported as
//! `DRIP_STARTUP_*` environment variables (the shipped default script uses the
//! environment). Output is stripped of terminal commands and control characters
//! before it reaches the transcript, so a script can never move the cursor,
//! clear the screen or set a title — while SGR color escapes survive, so the
//! banner can be colorful.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::tools::child_env::build_child_process_env;
use crate::tools::child_process::{run_captured_process, CapturedProcessArgs};

/// File name of the operator-editable startup script inside the drip home.
pub const STARTUP_MESSAGE_FILE_NAME: &str = "startup-message.sh";

/// Upper bound on captured banner text, so a chatty script cannot balloon the
/// transcript.
pub const STARTUP_MESSAGE_MAX_CHARS: usize = 4096;

/// How long the script may run before it is killed and the banner is dropped.
pub const STARTUP_MESSAGE_TIMEOUT_MS: u64 = 5000;

/// One model profile as the startup script sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupProfile {
    pub id: String,
    pub model: String,
    pub label: Option<String>,
}

/// Everything one startup-message run needs.
#[derive(Debug, Clone)]
pub struct StartupMessageInput<'a> {
    /// The script to run; `resolve_startup_message_path` produces the default.
    pub path: &'a str,
    pub version: &'a str,
    pub cwd: &'a str,
    pub session_id: &'a str,
    pub profiles: &'a [StartupProfile],
    pub active_profile_id: &'a str,
}

/// The startup script for a home root: `<home>/startup-message.sh`, or an
/// explicit non-empty `$DRIP_STARTUP_MESSAGE` path when one is set.
pub fn resolve_startup_message_path(home_root: &str) -> String {
    std::env::var("DRIP_STARTUP_MESSAGE")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| {
            PathBuf::from(home_root)
                .join(STARTUP_MESSAGE_FILE_NAME)
                .to_string_lossy()
                .into_owned()
        })
}

/// Defaults shipped by earlier versions, byte for byte. A
/// `startup-message.sh` still identical to one of these has never been edited
/// by the operator, so it is upgraded in place; any other content belongs to
/// the operator and is never touched.
const LEGACY_STARTUP_MESSAGE_SCRIPTS: &[&str] = &[include_str!("startup_legacy_default.sh")];

/// Seed `startup-message.sh` with the shipped banner so there is always
/// something to edit. An existing file is left byte-identical — unless it is
/// still byte-identical to a default an earlier version shipped, in which case
/// it was never edited and is upgraded to the current default.
pub fn ensure_startup_message_file(path: &Path) -> Result<()> {
    if path.exists() {
        // A file that cannot be read as UTF-8 is the operator's too.
        if let Ok(existing) = fs::read_to_string(path) {
            if LEGACY_STARTUP_MESSAGE_SCRIPTS.contains(&existing.as_str()) {
                fs::write(path, DEFAULT_STARTUP_MESSAGE_SCRIPT)?;
            }
        }
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    fs::write(path, DEFAULT_STARTUP_MESSAGE_SCRIPT)?;
    Ok(())
}

/// One line per profile — `* id: model (label)` for the active one, `  id: …`
/// for the rest — as `DRIP_STARTUP_PROFILES` and as the `profiles` payload
/// field's readable sibling.
pub fn profiles_display(profiles: &[StartupProfile], active_profile_id: &str) -> String {
    profiles
        .iter()
        .map(|profile| {
            let mut model = profile.model.trim().to_string();
            if let Some(label) = profile
                .label
                .as_deref()
                .map(str::trim)
                .filter(|label| !label.is_empty())
            {
                model = format!("{model} ({label})");
            }
            let marker = if profile.id == active_profile_id {
                "*"
            } else {
                " "
            };
            format!("{marker} {}: {model}", profile.id)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The JSON object written to the script's stdin. Unavailable facts are `null`
/// rather than guessed.
pub fn startup_message_payload_json(input: &StartupMessageInput<'_>) -> String {
    let session_id = input.session_id.trim();
    let profiles: Vec<serde_json::Value> = input
        .profiles
        .iter()
        .map(|profile| {
            serde_json::json!({
                "id": profile.id,
                "model": profile.model,
                "label": profile.label,
            })
        })
        .collect();

    serde_json::json!({
        "version": input.version,
        "cwd": input.cwd,
        "session_id": if session_id.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::Value::String(session_id.to_string())
        },
        "profiles": profiles,
        "active_profile_id": input.active_profile_id,
    })
    .to_string()
}

/// The child environment: the ordinary scrub-aware child env plus the
/// `DRIP_STARTUP_*` facts the default script reads.
pub fn startup_message_env(input: &StartupMessageInput<'_>) -> BTreeMap<String, String> {
    let mut env = build_child_process_env(None);
    env.insert(
        "DRIP_STARTUP_VERSION".to_string(),
        input.version.to_string(),
    );
    env.insert("DRIP_STARTUP_CWD".to_string(), input.cwd.to_string());
    env.insert(
        "DRIP_STARTUP_SESSION_ID".to_string(),
        input.session_id.to_string(),
    );
    env.insert(
        "DRIP_STARTUP_ACTIVE_PROFILE".to_string(),
        input.active_profile_id.to_string(),
    );
    env.insert(
        "DRIP_STARTUP_PROFILES".to_string(),
        profiles_display(input.profiles, input.active_profile_id),
    );
    env
}

/// Drop terminal *commands* and control characters (newline and tab survive),
/// so banner text can never move the cursor, clear the screen or set a title.
/// SGR color runs — `ESC [ … m` whose parameters are only digits, `;`, `:` and
/// spaces — pass through unchanged, so a banner may be colorful.
pub fn strip_control_sequences(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    let mut body = String::new();
                    let mut final_byte = None;
                    for next in chars.by_ref() {
                        if next.is_ascii_alphabetic() || body.chars().count() >= 64 {
                            final_byte = Some(next);
                            break;
                        }
                        body.push(next);
                    }
                    // `ESC [ params m` is a color/style run: keep it. Everything
                    // else (cursor moves, clears, titles, mouse reports) is
                    // dropped. The parameter check matters: it is what stops a
                    // command byte from riding along in the kept sequence.
                    let params_ok = body
                        .chars()
                        .all(|ch| ch.is_ascii_digit() || matches!(ch, ';' | ':' | ' '));
                    if final_byte == Some('m') && params_ok {
                        out.push('\x1b');
                        out.push('[');
                        out.push_str(&body);
                        out.push('m');
                    }
                }
                Some(']') => {
                    chars.next();
                    let mut last_was_esc = false;
                    for next in chars.by_ref() {
                        if next == '\x07' {
                            break;
                        }
                        if last_was_esc && next == '\\' {
                            break;
                        }
                        last_was_esc = next == '\x1b';
                    }
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            }
            continue;
        }
        if c == '\n' || c == '\t' {
            out.push(c);
            continue;
        }
        if c.is_control() {
            continue;
        }
        out.push(c);
    }

    out
}

/// Trim surrounding blank lines/trailing spaces and cap the length.
fn clamp_output(text: &str) -> String {
    let trimmed = text.trim_matches('\n').trim_end();
    if trimmed.chars().count() <= STARTUP_MESSAGE_MAX_CHARS {
        return trimmed.to_string();
    }

    let mut out: String = trimmed.chars().take(STARTUP_MESSAGE_MAX_CHARS).collect();
    out.push('\u{2026}');
    out
}

/// How the script is invoked. The file is passed to the shell as an argument
/// (never word-split as a command), so a home path with spaces works and the
/// script needs no executable bit — drip writes it mode 0644 like every other
/// seeded file.
fn runner_argv(path: &str) -> (&'static str, Vec<String>) {
    if cfg!(windows) {
        ("cmd", vec!["/C".to_string(), path.to_string()])
    } else {
        ("/bin/sh", vec![path.to_string()])
    }
}

/// Run the startup script once and return the (plain-text) banner, or `None`
/// when there is nothing to show.
///
/// `None` covers: no path, a missing/empty script, a timeout, and a run that
/// printed nothing and exited 0. A script that exists but fails returns one
/// short diagnostic line naming its status and first stderr line — a broken
/// banner should be visible, never fatal.
pub fn run_startup_message(input: &StartupMessageInput<'_>) -> Option<String> {
    let path = input.path.trim();
    if path.is_empty() || !Path::new(path).exists() {
        return None;
    }

    let payload = startup_message_payload_json(input);
    let env = startup_message_env(input);
    let (shell, process_args) = runner_argv(path);
    let args = CapturedProcessArgs {
        command: shell,
        cwd: Some(input.cwd),
        env: Some(&env),
        process_args: &process_args,
        timeout_ms: Some(STARTUP_MESSAGE_TIMEOUT_MS),
        stdin_payload: Some(payload.as_str()),
    };

    let result = run_captured_process(&args).ok()?;

    if result.timed_out {
        return Some(format!(
            "startup message timed out after {STARTUP_MESSAGE_TIMEOUT_MS} ms: {path}"
        ));
    }

    let text = clamp_output(&strip_control_sequences(&result.stdout));
    if !text.trim().is_empty() {
        return Some(text);
    }

    if result.exit_code != Some(0) {
        let stderr = strip_control_sequences(&result.stderr);
        let first = stderr
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("no output");
        let status = match result.exit_code {
            Some(code) => format!("exit {code}"),
            None => "killed".to_string(),
        };
        return Some(format!("startup message failed ({status}): {first}"));
    }

    None
}

/// The shipped startup script: a rainbow droplet and one info line. It uses
/// only the `DRIP_STARTUP_*` environment variables, so it is a working example
/// of how a rewrite gets its facts.
pub const DEFAULT_STARTUP_MESSAGE_SCRIPT: &str = r#"#!/bin/sh
# drip's startup message - printed once when an interactive session opens.
#
# Edit this file to change what drip shows at startup, or delete it for no
# banner at all. Facts about this run arrive as environment variables (the same
# fields also arrive as one JSON object on stdin):
#
#   DRIP_STARTUP_VERSION         drip's version, e.g. 0.1.0
#   DRIP_STARTUP_CWD             the directory this session runs in
#   DRIP_STARTUP_SESSION_ID      this session's id
#   DRIP_STARTUP_ACTIVE_PROFILE  the model profile chosen for startup
#   DRIP_STARTUP_PROFILES        one "id: model" line per configured profile,
#                                the active one marked with *
#
# Color is fine: escapes that move the cursor, clear the screen or set a title
# are stripped before the banner is shown; colors pass through.

# One styled fragment: sgr 1;36 'text'.
sgr() {
  printf '\033[%sm%s\033[0m' "$1" "$2"
}

# Paint one line through a blue -> cyan -> violet ramp.
rainbow() {
  printf '%s' "$1" | awk 'BEGIN { n = split("45 51 81 111 147 183 219", ramp); cur = "" }
  {
    len = split($0, ch, "")
    for (i = 1; i <= len; i++) {
      if (ch[i] == " ") { printf " "; continue }
      code = ramp[int((i - 1) * n / len) + 1]
      if (code != cur) { printf "%c[38;5;%sm", 27, code; cur = code }
      printf "%s", ch[i]
    }
    printf "%c[0m\n", 27
  }'
}

rainbow '       __'
rainbow '      /  \'
rainbow '     /    \'
rainbow '     |    |'
rainbow '     \    /'
rainbow '      \  /'
rainbow '       \/'

active_model=$(printf '%s\n' "$DRIP_STARTUP_PROFILES" | grep '^\* ' | head -n 1)
active_model=${active_model#*: }
[ -n "$active_model" ] || active_model=$DRIP_STARTUP_ACTIVE_PROFILE
printf '  %s %s %s %s %s %s\n' \
  "$(sgr '1;36' '❯')" \
  "$(sgr '1;36' 'drip') $(sgr '38;5;45' "$DRIP_STARTUP_VERSION")" \
  "$(sgr '38;5;240' '·')" \
  "$(sgr '1;38;5;183' "$active_model")" \
  "$(sgr '38;5;240' '·')" \
  "$(sgr '38;5;250' "$DRIP_STARTUP_CWD")"
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(id: &str, model: &str, label: Option<&str>) -> StartupProfile {
        StartupProfile {
            id: id.to_string(),
            model: model.to_string(),
            label: label.map(str::to_string),
        }
    }

    fn write_script(dir: &Path, body: &str) -> String {
        let path = dir.join("startup-message.sh");
        std::fs::write(&path, body).unwrap();
        path.to_string_lossy().into_owned()
    }

    fn run(path: &str, cwd: &str, profiles: &[StartupProfile]) -> Option<String> {
        run_startup_message(&StartupMessageInput {
            path,
            version: "9.9.9",
            cwd,
            session_id: "sess-abc",
            profiles,
            active_profile_id: "default",
        })
    }

    /// The banner as a terminal would show it: every escape removed.
    fn plain(text: &str) -> String {
        crate::watch::ansi::strip_ansi(text)
    }

    /// Every escape run in `text`.
    fn escape_runs(text: &str) -> Vec<String> {
        let mut runs = Vec::new();
        let mut chars = text.chars().peekable();

        while let Some(ch) = chars.next() {
            if ch != '\x1b' {
                continue;
            }
            let mut run = String::from(ch);
            while let Some(next) = chars.next() {
                run.push(next);
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
            runs.push(run);
        }

        runs
    }

    #[test]
    fn the_default_script_draws_a_colorful_droplet_and_one_info_line() {
        let dir = tempfile::tempdir().unwrap();
        let cwd_dir = tempfile::tempdir().unwrap();
        let cwd = cwd_dir.path().to_string_lossy().into_owned();
        let path = write_script(dir.path(), DEFAULT_STARTUP_MESSAGE_SCRIPT);
        let profiles = [
            profile("default", "anthropic/claude-sonnet-4-5", None),
            profile("fast", "anthropic/claude-haiku-4-5", Some("quick")),
        ];

        let out = run(&path, &cwd, &profiles).expect("the default script prints a banner");
        let text = plain(&out);

        // The mascot is a droplet: flat crown, sloping shoulders, a straight
        // body and a tapered tip.
        assert!(
            text.contains("       __"),
            "droplet crown missing: {text:?}"
        );
        assert!(
            text.contains("      /  \\"),
            "droplet shoulder missing: {text:?}"
        );
        assert!(
            text.contains("     |    |"),
            "droplet body missing: {text:?}"
        );
        assert!(text.contains("       \\/"), "droplet tip missing: {text:?}");
        // It is painted through a 256-color ramp, and the info line is styled.
        assert!(out.contains("\x1b[38;5;45m"), "no rainbow ramp: {out:?}");
        assert!(out.contains("\x1b[38;5;219m"), "ramp is too short: {out:?}");
        assert!(out.contains("\x1b[1;36m"), "no accent: {out:?}");
        // One info line carrying version, the startup profile and the cwd.
        assert!(out.contains("drip"), "{out:?}");
        assert!(out.contains("9.9.9"), "version missing: {out:?}");
        assert!(
            out.contains("anthropic/claude-sonnet-4-5"),
            "startup profile missing: {out:?}"
        );
        assert!(out.contains(&cwd), "cwd missing: {out:?}");
        assert!(
            !text.contains("session ") && !text.contains("model profiles"),
            "the info block is still verbose: {text:?}"
        );
        assert_eq!(
            text.trim_end().lines().count(),
            8,
            "expected 7 mascot rows + 1 info row: {text:?}"
        );
        // Only color runs survive: every escape printed ends a style, so
        // nothing in the banner can move the cursor or clear the screen.
        let runs = escape_runs(&out);
        assert!(!runs.is_empty(), "the banner lost its color: {out:?}");
        for rune in runs {
            assert!(rune.ends_with('m'), "non-color escape leaked: {rune:?}");
        }
    }

    #[test]
    fn colors_survive_but_terminal_commands_are_stripped() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_script(
            dir.path(),
            "printf '\\033[1;36mred\\033[0m \\033[2J\\033[3;4H\\033]0;t\\007\\007done\\n'",
        );

        let out = run(&path, "/tmp", &[]).expect("prints text");

        assert_eq!(out, "\x1b[1;36mred\x1b[0m done");
    }

    #[test]
    fn an_empty_script_shows_no_banner() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_script(dir.path(), "\n:\n");

        assert_eq!(run(&path, "/tmp", &[]), None);
    }

    #[test]
    fn a_missing_script_shows_no_banner() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-there.sh");

        assert_eq!(run(&path.to_string_lossy(), "/tmp", &[]), None);
    }

    #[test]
    fn a_failing_script_reports_one_short_diagnostic_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_script(dir.path(), "printf 'boom\\n' >&2\nexit 3\n");

        let out = run(&path, "/tmp", &[]).expect("a failure is visible, not silent");

        assert_eq!(out, "startup message failed (exit 3): boom");
    }

    #[test]
    fn the_script_receives_the_json_payload_on_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_script(dir.path(), "cat\n");
        let profiles = [profile("default", "openrouter/x", None)];

        let out = run(&path, "/tmp", &profiles).expect("cat echoes the payload");
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();

        assert_eq!(value["version"], "9.9.9");
        assert_eq!(value["cwd"], "/tmp");
        assert_eq!(value["session_id"], "sess-abc");
        assert_eq!(value["active_profile_id"], "default");
        assert_eq!(value["profiles"][0]["model"], "openrouter/x");
    }

    #[test]
    fn the_seeded_file_is_written_once_and_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STARTUP_MESSAGE_FILE_NAME);

        ensure_startup_message_file(&path).unwrap();
        let seeded = std::fs::read_to_string(&path).unwrap();
        assert_eq!(seeded, DEFAULT_STARTUP_MESSAGE_SCRIPT);

        std::fs::write(&path, "echo mine\n").unwrap();
        ensure_startup_message_file(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "echo mine\n");
    }

    #[test]
    fn an_unedited_previous_default_is_upgraded_and_an_edited_file_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STARTUP_MESSAGE_FILE_NAME);
        let legacy = LEGACY_STARTUP_MESSAGE_SCRIPTS[0];

        // Never edited, so it is replaced by the default this build ships.
        std::fs::write(&path, legacy).unwrap();
        ensure_startup_message_file(&path).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            DEFAULT_STARTUP_MESSAGE_SCRIPT,
            "an unedited earlier default was not upgraded"
        );

        // Any other content belongs to the operator and is left byte-identical.
        std::fs::write(&path, "echo mine\n").unwrap();
        ensure_startup_message_file(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "echo mine\n");
    }

    #[test]
    fn the_profiles_display_marks_the_active_profile() {
        let profiles = [
            profile("default", "a/model", None),
            profile("fast", "b/model", Some("quick")),
        ];

        assert_eq!(
            profiles_display(&profiles, "default"),
            "* default: a/model\n  fast: b/model (quick)"
        );
    }
}
