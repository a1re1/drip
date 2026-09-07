// bin `dripw` — the read-only watch TUI.

use std::io::Write;

use drip::core::home::{resolve_drip_home_root, resolve_drip_project};
use drip::watch::app::run_watch_app;

// ---------------------------------------------------------------------------
// Help text
// ---------------------------------------------------------------------------

const HELP_TEXT: &str = "\
dripw — read-only lazygit-style watch TUI for drip sessions

Usage:
  dripw [options]

Options:
  --help, -h    Print this help text and exit

Keys:
  1 / 2 / 3     Focus the Running, Recent, or Shells panel
  Tab           Cycle between panels
  j / k         Move selection down / up
  ↑ / ↓         Move selection up / down
  [ / ]         Scroll transcript back / forward  (also h / l, ← / →)
  q / Ctrl+C    Quit

dripw shows sessions started in the current directory or any directory beneath it.

Focusing a shell (3) turns the bottom pane into process details (pid, ppid,
command, children) plus a live tail of its stdout/stderr when they point at
regular files.

dripw is read-only — it never modifies sessions or the session index.
";

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print!("{HELP_TEXT}");
        let _ = std::io::stdout().flush();
        std::process::exit(0);
    }

    let cwd = std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| ".".to_string());
    let home_root = resolve_drip_home_root();
    // resolve_drip_project only, never open_drip_home/ensure_drip_project — a
    // watcher must not create the home tree, the project .drip, or anything else.
    // No --project-dir / DRIP_PROJECT_DIR override: the watcher always looks
    // at the cwd's project.
    let project = match resolve_drip_project(&cwd, &home_root, None) {
        Ok(project) => project,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    };

    // Name the terminal tab after the tool.
    // SAFETY: isatty on fd 1 has no preconditions.
    if unsafe { libc::isatty(1) } == 1 {
        print!("\u{1b}]0;dripw\u{7}");
        let _ = std::io::stdout().flush();
    }

    // The app re-lists every project home under the drip home on each refresh
    // and keeps only sessions started in cwd or a directory beneath it.
    run_watch_app(project, cwd);
}
