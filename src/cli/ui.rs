//! `drip --ui`: host the browser UI for every session under the launch
//! directory. The Bun/React app under `web/` ships embedded in the binary
//! (UI_FILES), is unpacked under `<home>/ui/<version>/`, and runs on the
//! user's own `bun`. It is a thin bridge over the drip binary and the on-disk
//! session files, so the sessions it lists and starts are exactly the ones
//! `--tui`, `dripw` and headless runs use — nothing is written into the
//! project directory.
//!
//! Ports and URLs are the Bun side's business (web/server.ts): without
//! `--port` it takes the first free port from 4141, and when a local Caddy
//! is running it registers under the shared hub (web/lib/caddy.ts). This
//! side only launches, forwards the env contract, and waits.
use std::path::{Path, PathBuf};

use indexmap::IndexMap;

macro_rules! ui_file {
    ($path:literal) => {
        ($path, include_str!(concat!("../../web/", $path)))
    };
}

/// Every file under `web/` (relative path, contents), excluding
/// `node_modules/` and `dist/`. A test walks `web/` and fails when the two
/// drift, because a file missing here is silently missing at runtime.
pub const UI_FILES: &[(&str, &str)] = &[
    ui_file!("bun.lock"),
    ui_file!("index.html"),
    ui_file!("lib/caddy.ts"),
    ui_file!("lib/drip.ts"),
    ui_file!("lib/instances.ts"),
    ui_file!("lib/transcript.ts"),
    ui_file!("package.json"),
    ui_file!("server.ts"),
    ui_file!("src/api.ts"),
    ui_file!("src/app.tsx"),
    ui_file!("src/components/composer.tsx"),
    ui_file!("src/components/detail-panel.tsx"),
    ui_file!("src/components/icons.tsx"),
    ui_file!("src/components/sessions-rail.tsx"),
    ui_file!("src/components/status-bar.tsx"),
    ui_file!("src/components/timeline.tsx"),
    ui_file!("src/main.tsx"),
    ui_file!("src/styles.css"),
    ui_file!("test/caddy.test.ts"),
    ui_file!("test/composer.test.ts"),
    ui_file!("test/drip.test.ts"),
    ui_file!("test/instances.test.ts"),
    ui_file!("test/server.test.ts"),
    ui_file!("test/timeline.test.ts"),
    ui_file!("test/transcript.test.ts"),
    ui_file!("tsconfig.json"),
];

/// Where this binary's copy of the web app lives; versioned so an upgrade
/// never runs new server code against a stale `node_modules`.
pub fn ui_dir(home_root: &Path) -> PathBuf {
    home_root.join("ui").join(env!("CARGO_PKG_VERSION"))
}

/// Materialize UI_FILES under the home, rewriting only files whose contents
/// differ. Returns the directory and how many files were written.
pub fn unpack_ui(home_root: &Path) -> std::io::Result<(PathBuf, usize)> {
    let dir = ui_dir(home_root);
    let mut written = 0;
    for (relative, content) in UI_FILES {
        let target = dir.join(relative);
        let unchanged = std::fs::read_to_string(&target).map(|current| current == *content).unwrap_or(false);
        if unchanged {
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, content)?;
        written += 1;
    }
    Ok((dir, written))
}

/// Everything the Bun process needs to know; it reads nothing else.
pub struct UiLaunch {
    pub cwd: String,
    pub home_root: String,
    /// `--port`: pin the listen port. None lets the server pick a free one.
    pub port: Option<u16>,
    pub drip_bin: PathBuf,
    pub max_context_tokens: Option<i64>,
}

/// The env contract with `web/server.ts` (see `dripConfig` there).
pub fn ui_child_env(launch: &UiLaunch) -> Vec<(String, String)> {
    let mut env = vec![
        ("DRIP_BIN".to_string(), launch.drip_bin.to_string_lossy().into_owned()),
        ("DRIP_CWD".to_string(), launch.cwd.clone()),
        ("DRIP_HOME".to_string(), launch.home_root.clone()),
        ("DRIP_UI_VERSION".to_string(), env!("CARGO_PKG_VERSION").to_string()),
    ];
    if let Some(port) = launch.port {
        env.push(("DRIP_UI_PORT".to_string(), port.to_string()));
    }
    if let Some(max) = launch.max_context_tokens {
        env.push(("DRIP_MAX_CONTEXT_TOKENS".to_string(), max.to_string()));
    }
    env
}

/// Written after a complete `bun install`, so a killed install is retried
/// instead of leaving a half-populated node_modules that fails on import.
pub fn install_marker(dir: &Path) -> PathBuf {
    dir.join("node_modules").join(".drip-installed")
}

/// How long bun gets to deregister from the hub after Ctrl-C before it is
/// killed: its cleanup waits at most 2s for the registry lock and makes a
/// handful of Caddy admin calls with a 2s timeout each.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(12);

/// Upper bound on waiting for another launch to finish preparing the tree.
const PREPARE_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(300);
/// A lock older than this belongs to a launch that died mid-install.
const PREPARE_LOCK_STALE: std::time::Duration = std::time::Duration::from_secs(600);

/// Held while unpacking and installing: two `drip --ui` launches started at
/// once (different projects, same version tree) must not both run
/// `bun install` — the second sees a complete tree instead. Removed on drop.
pub struct PrepareLock(PathBuf);

impl Drop for PrepareLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub fn prepare_lock_path(home_root: &Path) -> PathBuf {
    home_root.join("ui").join(format!("{}.lock", env!("CARGO_PKG_VERSION")))
}

pub fn acquire_prepare_lock(home_root: &Path) -> std::io::Result<PrepareLock> {
    acquire_prepare_lock_with(home_root, PREPARE_LOCK_WAIT, PREPARE_LOCK_STALE)
}

fn acquire_prepare_lock_with(
    home_root: &Path,
    wait: std::time::Duration,
    stale: std::time::Duration,
) -> std::io::Result<PrepareLock> {
    use std::io::Write;
    let path = prepare_lock_path(home_root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let started = std::time::Instant::now();
    loop {
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                let _ = writeln!(file, "{}", std::process::id());
                return Ok(PrepareLock(path));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                // Abandoned when its holder is gone (a Ctrl-C during the
                // install kills the launcher before the lock is dropped) or,
                // if the pid cannot be read, when it is old enough.
                let holder_dead = std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|text| text.trim().parse::<u32>().ok())
                    .map(|pid| !pid_alive(pid))
                    .unwrap_or(false);
                let abandoned = holder_dead
                    || std::fs::metadata(&path)
                        .and_then(|meta| meta.modified())
                        .map(|modified| modified.elapsed().map(|age| age >= stale).unwrap_or(false))
                        .unwrap_or(false);
                if abandoned {
                    let _ = std::fs::remove_file(&path);
                    continue;
                }
                if started.elapsed() >= wait {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!("another drip --ui is still preparing {} (lock {})", home_root.display(), path.display()),
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            Err(error) => return Err(error),
        }
    }
}

/// Whether a process with this pid exists (EPERM counts as alive).
#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    // SAFETY: kill with signal 0 only probes for existence.
    if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn pid_alive(_pid: u32) -> bool {
    true
}

/// First `bun` on PATH, if any.
pub fn find_bun() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|dir| dir.join("bun")).find(|candidate| candidate.is_file())
}

/// The `--ui` mode: unpack, install once, print the URL, and run bun until it
/// exits or the operator presses Ctrl-C.
pub async fn run_ui(cwd: &str, home_root: &str, settings: &IndexMap<String, String>, port: Option<u16>) -> i32 {
    let Some(bun) = find_bun() else {
        eprintln!("drip --ui needs bun on PATH — install from https://bun.sh");
        return 1;
    };

    let prepare_lock = match acquire_prepare_lock(Path::new(home_root)) {
        Ok(lock) => lock,
        Err(error) => {
            eprintln!("drip --ui could not lock the web app directory: {error}");
            return 1;
        }
    };
    let (dir, written) = match unpack_ui(Path::new(home_root)) {
        Ok(unpacked) => unpacked,
        Err(error) => {
            eprintln!("drip --ui could not unpack the web app: {error}");
            return 1;
        }
    };
    if written > 0 {
        eprintln!("drip ui: unpacked {written} file(s) into {}", dir.display());
    }

    let marker = install_marker(&dir);
    if !marker.is_file() {
        eprintln!("drip ui: installing web dependencies (first run for this version)…");
        match std::process::Command::new(&bun).arg("install").current_dir(&dir).status() {
            Ok(status) if status.success() => {
                if let Err(error) = std::fs::write(&marker, env!("CARGO_PKG_VERSION")) {
                    eprintln!("drip ui: could not record the install: {error}");
                }
            }
            Ok(status) => {
                eprintln!("bun install failed ({status}) in {}", dir.display());
                return 1;
            }
            Err(error) => {
                eprintln!("could not run bun install: {error}");
                return 1;
            }
        }
    }

    drop(prepare_lock);

    let launch = UiLaunch {
        cwd: cwd.to_string(),
        home_root: home_root.to_string(),
        port,
        drip_bin: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("drip")),
        max_context_tokens: match crate::core::inference::resolve_active_profile_max_context_tokens(settings) {
            Ok(max) => Some(max),
            Err(error) => {
                eprintln!("drip ui: context bar disabled — active profile has no usable max_context_tokens: {error}");
                None
            }
        },
    };
    let mut command = tokio::process::Command::new(&bun);
    // Run the server file directly rather than `bun run dev`: the script
    // runner is a separate process, so a signal aimed at our child would
    // stop the wrapper and orphan the server that holds the port.
    command.arg("server.ts").current_dir(&dir).envs(ui_child_env(&launch));
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            eprintln!("could not start bun: {error}");
            return 1;
        }
    };

    tokio::select! {
        status = child.wait() => match status {
            Ok(status) => status.code().unwrap_or(1),
            Err(error) => {
                eprintln!("bun exited abnormally: {error}");
                1
            }
        },
        _ = tokio::signal::ctrl_c() => {
            // Bun uses SIGINT to leave the Caddy hub and the instance
            // registry. A terminal Ctrl-C reaches it through the process
            // group; when we were signalled directly, forward it ourselves.
            // Bun's handler is idempotent (a second SIGINT during cleanup
            // is ignored), so forwarding in both cases is safe. Then give
            // the cleanup a bounded moment before the kill.
            #[cfg(unix)]
            if let Some(pid) = child.id() {
                // SAFETY: plain kill(2) on our own child's pid.
                unsafe { libc::kill(pid as libc::pid_t, libc::SIGINT) };
            }
            if tokio::time::timeout(SHUTDOWN_GRACE, child.wait()).await.is_err() {
                let _ = child.kill().await;
            }
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shipped_web_files() -> Vec<String> {
        let root = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/web"));
        let mut files: Vec<String> = walkdir::WalkDir::new(&root)
            .into_iter()
            .filter_entry(|entry| {
                let name = entry.file_name().to_string_lossy();
                !(entry.file_type().is_dir() && (name == "node_modules" || name == "dist")) && name != ".DS_Store"
            })
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_type().is_file())
            .map(|entry| {
                entry
                    .path()
                    .strip_prefix(&root)
                    .expect("under web/")
                    .components()
                    .map(|component| component.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/")
            })
            .collect();
        files.sort();
        files
    }

    // A file added under web/ without a UI_FILES entry is silently absent
    // from the unpacked app at runtime.
    #[test]
    fn every_web_file_is_embedded() {
        let mut registered: Vec<String> = UI_FILES.iter().map(|(path, _)| path.to_string()).collect();
        registered.sort();
        assert_eq!(shipped_web_files(), registered, "web/ and UI_FILES disagree");
    }

    #[test]
    fn unpack_writes_every_file_once_and_is_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let (dir, first) = unpack_ui(temp.path()).unwrap();
        assert_eq!(first, UI_FILES.len());
        assert!(dir.starts_with(temp.path().join("ui")));
        for (relative, content) in UI_FILES {
            assert_eq!(std::fs::read_to_string(dir.join(relative)).unwrap(), *content, "{relative}");
        }

        let (_, second) = unpack_ui(temp.path()).unwrap();
        assert_eq!(second, 0, "unchanged files must not be rewritten");

        // A locally edited file is restored to the embedded copy.
        std::fs::write(dir.join("package.json"), "{}").unwrap();
        let (_, third) = unpack_ui(temp.path()).unwrap();
        assert_eq!(third, 1);
    }

    #[test]
    fn child_env_carries_the_bridge_contract() {
        let launch = UiLaunch {
            cwd: "/work/repo".to_string(),
            home_root: "/home/me/.drip".to_string(),
            port: Some(4199),
            drip_bin: PathBuf::from("/usr/local/bin/drip"),
            max_context_tokens: Some(200_000),
        };
        let env = ui_child_env(&launch);
        let get = |key: &str| env.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str());
        assert_eq!(get("DRIP_BIN"), Some("/usr/local/bin/drip"));
        assert_eq!(get("DRIP_CWD"), Some("/work/repo"));
        assert_eq!(get("DRIP_HOME"), Some("/home/me/.drip"));
        assert_eq!(get("DRIP_UI_VERSION"), Some(env!("CARGO_PKG_VERSION")));
        assert_eq!(get("DRIP_UI_PORT"), Some("4199"));
        assert_eq!(get("DRIP_MAX_CONTEXT_TOKENS"), Some("200000"));

        // No --port and no usable profile: the server picks the port itself
        // and the context bar stays off — neither key is sent at all.
        let without = ui_child_env(&UiLaunch { port: None, max_context_tokens: None, ..launch });
        assert!(without.iter().all(|(k, _)| k != "DRIP_MAX_CONTEXT_TOKENS" && k != "DRIP_UI_PORT"));
        assert_eq!(without.len(), 4);
    }

    #[test]
    fn prepare_lock_held_by_a_dead_process_is_reclaimed_at_once() {
        let temp = tempfile::tempdir().unwrap();
        let path = prepare_lock_path(temp.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // A pid no live process can have (beyond the platform's pid_max).
        std::fs::write(&path, "4194304999\n").unwrap();
        let started = std::time::Instant::now();
        let lock = acquire_prepare_lock_with(temp.path(), std::time::Duration::from_secs(5), std::time::Duration::from_secs(3600)).unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(2), "dead holder should be reclaimed immediately");
        assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), std::process::id().to_string());
        drop(lock);
        assert!(!path.exists());
        // Our own pid is alive: the lock is honoured and the wait times out.
        std::fs::write(&path, format!("{}\n", std::process::id())).unwrap();
        let kind = acquire_prepare_lock_with(temp.path(), std::time::Duration::from_millis(50), std::time::Duration::from_secs(3600))
            .err()
            .map(|error| error.kind());
        assert_eq!(kind, Some(std::io::ErrorKind::TimedOut));
    }

    #[test]
    fn prepare_lock_is_exclusive_released_on_drop_and_reclaimed_when_stale() {
        let temp = tempfile::tempdir().unwrap();
        let short = std::time::Duration::from_millis(50);
        let never_stale = std::time::Duration::from_secs(3600);
        let lock = acquire_prepare_lock_with(temp.path(), short, never_stale).unwrap();
        assert!(prepare_lock_path(temp.path()).is_file());
        // A second launch waits, then gives up after `wait`.
        let contended = acquire_prepare_lock_with(temp.path(), short, never_stale);
        assert_eq!(contended.err().map(|e| e.kind()), Some(std::io::ErrorKind::TimedOut));
        drop(lock);
        assert!(!prepare_lock_path(temp.path()).exists(), "lock must go away with its holder");

        // A lock left by a launch that died is reclaimed once it is older than `stale`.
        std::fs::write(prepare_lock_path(temp.path()), "999999\n").unwrap();
        let reclaimed = acquire_prepare_lock_with(temp.path(), short, std::time::Duration::ZERO);
        assert!(reclaimed.is_ok());
    }

    #[test]
    fn install_marker_lives_inside_node_modules() {
        let dir = PathBuf::from("/x/ui/1.0.0");
        assert_eq!(install_marker(&dir), dir.join("node_modules").join(".drip-installed"));
    }
}
