// Storage is split in three:
//   ~/.drip                          — shared across every project: config,
//                                      credentials, user skills, marketplaces.
//   ~/.drip/projects/<slug>/         — per-project machine-local data
//                                      (sessions, session index), keyed by
//                                      the CHECKOUT the run happened in.
//   <project>/.drip                  — repo-scoped data (skills/, roles.json,
//                                      plugins.json, patches.jsonl,
//                                      async-tools/).
//
// ~/.drip is shared because credentials and config are machine-wide; the
// per-project tree is split by slug so two checkouts of the same repo never
// fight over one session index; and the repo keeps only what is inherently
// repo-scoped. Everything here resolves paths — nothing is created until
// open_drip_home / ensure_drip_project run.

use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, Result};

use crate::core::env_vars::ensure_env_vars_file;

// path.resolve() analogue: lexical only — Node's resolve never expands
// symlinks (a /tmp home stays /tmp, not /private/tmp), and the slug, session
// paths, and every printed path derive from this.
pub fn resolve(path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();

    if path.is_absolute() {
        return normalize_lexical(path);
    }

    match std::env::current_dir() {
        Ok(cwd) => normalize_lexical(&cwd.join(path)),
        Err(_) => normalize_lexical(path),
    }
}

// Collapse `.`/`..`/duplicate separators without touching the filesystem —
// path.resolve() on paths that do not exist yet.
fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();

    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }

    out
}

fn join(base: impl AsRef<Path>, rest: &str) -> PathBuf {
    let mut out = base.as_ref().to_path_buf();
    out.push(rest);
    out
}

fn exists(path: &Path) -> bool {
    path.exists()
}

/// The global home, rooted at ~/.drip (or $DRIP_HOME / --home).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DripHome {
    /// <home>/config.json — the user's settings ({settings, version}).
    pub config_path: String,
    /// <home>/env.vars — credential store read before the process environment.
    pub env_vars_path: String,
    /// <home>/marketplaces/ — cloned marketplace repos.
    pub marketplaces_dir: String,
    /// <home>/marketplaces.json — the marketplace registry.
    pub marketplaces_path: String,
    /// The ~/.drip root every project's homes live under.
    pub home_root: String,
    /// <home>/projects/ — per-project session homes, keyed by checkout slug.
    pub projects_dir: String,
    /// <home>/skills/ — the user's skill library, shared by every project.
    pub skills_dir: String,
    /// The ~/.drip root this home lives under.
    pub root: String,
}

/// Per-project home: session tree under the global home, repo-scoped data in
/// the checkout's .drip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DripProject {
    /// Pre-move session index at <repo>/.drip/index.sqlite, set only when it still exists. Read, never written.
    pub legacy_index_db_path: Option<String>,
    /// Pre-move session tree at <repo>/.drip/sessions, set only when it still exists. Read, never written.
    pub legacy_sessions_dir: Option<String>,
    /// The ~/.drip root this project's homes live under.
    pub home_root: String,
    /// The session registry for this project only, under ~/.drip/projects/<slug>/.
    pub index_db_path: String,
    /// Per-project memory bank stored under ~/.drip/projects/<repoSlug>/memory — keyed by the repo, so
    /// every worktree shares what was learned (outside the repo tree). Sandbox override puts it inside
    /// the override dir instead.
    pub memory_dir: String,
    /// The discovered project root (nearest .drip, else .git, else cwd), not its .drip. Undefined under a --project-dir override.
    pub project_root: Option<String>,
    /// The main checkout every worktree of this repo shares — the memory slug is keyed off this. Equals the
    /// worktree root outside a linked worktree, and projectRoot outside git. Undefined under an override.
    pub repo_root: Option<String>,
    /// The repo-root slug memory is keyed by; equals slug outside a linked worktree and under a --project-dir override.
    pub repo_slug: String,
    /// <cwd>/.drip — hosts skills/, roles.json, plugins.json, patches.jsonl, async-tools/.
    pub root: String,
    /// ~/.drip/projects/<slug>/sessions/<session-id>/ holds each session's state and transcript. Keyed by the
    /// checkout the run happened in, so a linked worktree keeps its own sessions separate from main's.
    pub sessions_dir: String,
    /// The slug keying this project's sessions and index: the worktree root inside a linked worktree,
    /// else the repo root.
    pub slug: String,
    /// The checkout the cwd sits in — the nearest .git ancestor, main or linked worktree alike; falls back
    /// to projectRoot outside git. dripw scopes its default view to sessions started under it. Undefined under an override.
    pub worktree_root: Option<String>,
}

/// Resolves the global home root from $DRIP_HOME (or an injected override, for
/// tests) or the user's home directory.
pub fn resolve_drip_home_root_from(home_override: Option<&str>) -> String {
    let override_ = home_override
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(Path::new)
        .map(resolve);

    match override_ {
        Some(v) => v.to_string_lossy().into_owned(),
        None => {
            let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
            join(&home, ".drip").to_string_lossy().into_owned()
        }
    }
}

pub fn resolve_drip_home_root() -> String {
    resolve_drip_home_root_from(std::env::var("DRIP_HOME").ok().as_deref())
}

// Mirrors how other harnesses key per-project storage: the absolute cwd with
// every non-alphanumeric run collapsed to a dash, so one home directory can
// hold every project. The slug shape is stable: existing session directories
// keep working across versions.
use std::borrow::Cow;

pub fn project_slug(cwd: &str) -> String {
    let slug: String = match resolve(cwd).to_str() {
        // Fast path: the resolved cwd is valid UTF-8 — collapse each run of
        // non-alphanumeric characters to a single dash, exactly like the
        // TypeScript regex `/[^A-Za-z0-9]+/g`.
        Some(path) => collapse_non_alnum_runs(path).into_owned(),
        // Non-UTF-8 paths are mapped through a lossy conversion before
        // collapsing, so every cwd yields a slug.
        None => collapse_non_alnum_runs(&resolve(cwd).to_string_lossy()).into_owned(),
    };

    if slug.starts_with('-') {
        slug
    } else {
        format!("-{slug}")
    }
}

// The Rust analogue of the TS regex `.replace(/[^A-Za-z0-9]+/g, "-")`: one dash
// per maximal run of non-alphanumeric bytes, so "weird name (v2)" slugs with
// two dashes, not one per character.
fn collapse_non_alnum_runs(path: &str) -> Cow<'_, str> {
    let bytes = path.as_bytes();
    let mut out = String::with_capacity(path.len());
    let mut in_run = false;

    for (index, byte) in bytes.iter().enumerate() {
        let is_alnum = byte.is_ascii_alphanumeric();

        if is_alnum {
            out.push(*byte as char);
            in_run = false;
        } else if !in_run {
            out.push('-');
            in_run = true;
        } else {
            // Still inside a run — but a multi-byte UTF-8 continuation byte must
            // not be counted as a separate character boundary for prefix checks.
            let _ = index;
        }
    }

    Cow::Owned(out)
}

pub fn open_drip_home(root: &str) -> DripHome {
    let home = DripHome {
        config_path: join(root, "config.json").to_string_lossy().into_owned(),
        env_vars_path: join(root, "env.vars").to_string_lossy().into_owned(),
        marketplaces_dir: join(root, "marketplaces").to_string_lossy().into_owned(),
        marketplaces_path: join(root, "marketplaces.json").to_string_lossy().into_owned(),
        home_root: root.to_string(),
        projects_dir: join(root, "projects").to_string_lossy().into_owned(),
        skills_dir: join(root, "skills").to_string_lossy().into_owned(),
        root: root.to_string(),
    };

    for dir in [&home.root, &home.skills_dir, &home.marketplaces_dir] {
        let _ = fs::create_dir_all(dir);
    }

    let _ = ensure_env_vars_file(Path::new(&home.env_vars_path));

    home
}

// Session data is machine-local (transcripts, sqlite, harness state), so the
// project directory ships its own ignore file rather than requiring every repo
// to add rules. The ignore file ignores itself too: a .drip holding only
// session data is then invisible to git status, while skills/roles/plugins
// configs stay visible the moment a project adds them.
pub const PROJECT_GITIGNORE: &str = ".gitignore\npatches.jsonl\nasync-tools/\n";

// A repo that still holds PRE-MOVE sessions keeps ignoring them: shrinking its
// ignore file would expose a tree of transcripts to git status, which is worse
// than carrying two now-dead rules.
pub const PROJECT_GITIGNORE_WITH_LEGACY_SESSIONS: &str =
    ".gitignore\nsessions/\nindex.sqlite*\npatches.jsonl\nasync-tools/\n";

/// Defaults shipped by earlier versions — safe to auto-upgrade when found
/// byte-identical. A repo still holding pre-move sessions keeps ignoring them
/// because the older default is only rewritten when it is byte-identical, and a
/// repo that has none no longer needs the rules.
pub const PREVIOUS_PROJECT_GITIGNORES: [&str; 3] = [
    ".gitignore\nsessions/\nindex.sqlite*\n",
    ".gitignore\nsessions/\nindex.sqlite*\npatches.jsonl\n",
    ".gitignore\nsessions/\nindex.sqlite*\npatches.jsonl\nasync-tools/\n",
];

// Running drip from a repo subdirectory should land in the repo's .drip, not
// scatter a second one — so the project root is discovered git-style: the
// nearest ancestor that already has a .drip directory wins, then the nearest
// with a .git, then the cwd itself. The global home root is never a valid
// project (a cwd of $HOME would otherwise adopt ~/.drip's legacy index).
pub fn resolve_drip_project_root(cwd: &str, home_root: &str) -> String {
    let start = resolve(cwd);
    let home_root_resolved = resolve(home_root);
    let mut git_ancestor: Option<PathBuf> = None;

    let mut dir = start.clone();
    loop {
        if join(&dir, ".drip") != home_root_resolved {
            if exists(&join(&dir, ".drip")) {
                return dir.to_string_lossy().into_owned();
            }

            if git_ancestor.is_none() && exists(&join(&dir, ".git")) {
                git_ancestor = Some(dir.clone());
            }
        }

        let parent = dir.join("..");
        let parent_resolved = resolve(&parent);
        if parent_resolved == dir {
            break;
        }
        dir = parent_resolved;
    }

    match git_ancestor {
        Some(anc) => anc.to_string_lossy().into_owned(),
        None => start.to_string_lossy().into_owned(),
    }
}

// The nearest ancestor (or `start` itself) holding a .git entry — a directory
// for a main checkout, a file for a linked worktree. Null when there is none.
pub fn nearest_git_ancestor(start: &str) -> Option<String> {
    let mut dir = resolve(start);
    loop {
        if exists(&join(&dir, ".git")) {
            return Some(dir.to_string_lossy().into_owned());
        }

        let parent = resolve(&dir.join(".."));
        if parent == dir {
            return None;
        }
        dir = parent;
    }
}

// The main checkout a worktree belongs to. A linked worktree's .git is a file
// reading `gitdir: <main>/.git/worktrees/<name>`; following it lands on the
// main checkout. A plain .git directory (or a submodule's `.git/modules/...`
// pointer) means `worktreeRoot` is its own repo. Null when there is no .git.
pub fn resolve_git_repo_root(worktree_root: &str) -> Option<String> {
    let dot_git = join(resolve(worktree_root), ".git");

    if !exists(&dot_git) {
        return None;
    }

    if dot_git.is_dir() {
        return Some(resolve(worktree_root).to_string_lossy().into_owned());
    }

    let contents = fs::read_to_string(&dot_git).unwrap_or_default();
    // ^gitdir:\s*(.+?)\s*$ over the file, multiline — the pointer line.
    let pointer = regex::Regex::new(r"(?m)^gitdir:\s*(.+?)\s*$")
        .ok()
        .and_then(|re| re.captures(&contents))
        .and_then(|caps| caps.get(1).map(|m| m.as_str().to_string()));

    let pointer = match pointer {
        Some(p) => p,
        // A .git file without a recognizable pointer is treated as its own repo.
        None => return Some(resolve(worktree_root).to_string_lossy().into_owned()),
    };

    let git_dir = resolve(Path::new(worktree_root).join(pointer));
    // <anything>/worktrees/<name> — the main checkout is <anything>'s parent
    // when it ends in .git, else <anything> itself.
    let git_dir_str = git_dir.to_string_lossy().into_owned();
    let linked = regex::Regex::new(r"^(.*)[/\\]worktrees[/\\][^/\\]+$")
        .ok()
        .and_then(|re| re.captures(&git_dir_str))
        .and_then(|caps| caps.get(1).map(|m| m.as_str().to_string()));

    let common_dir = match linked {
        Some(c) => c,
        None => return Some(resolve(worktree_root).to_string_lossy().into_owned()),
    };

    let common_path = Path::new(&common_dir);
    let base = common_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();

    if base == ".git" {
        match common_path.parent() {
            Some(parent) => Some(parent.to_string_lossy().into_owned()),
            None => Some(common_dir),
        }
    } else {
        Some(common_dir)
    }
}

// Every linked worktree registered under a main checkout, as checkout roots.
// Read from `<repo>/.git/worktrees/<name>/gitdir` (each names `<root>/.git`);
// entries whose checkout has been deleted are skipped. Empty for a repo with
// no linked worktrees, a bare directory, or anything unreadable.
pub fn list_linked_worktree_roots(repo_root: &str) -> Vec<String> {
    let registry = join(resolve(repo_root).join(".git"), "worktrees");

    if !exists(&registry) {
        return vec![];
    }

    let entries = match fs::read_dir(&registry) {
        Ok(entries) => entries,
        Err(_) => return vec![],
    };

    let mut roots: Vec<String> = vec![];

    for entry in entries.flatten() {
        let gitdir_path = entry.path().join("gitdir");

        if let Ok(contents) = fs::read_to_string(&gitdir_path) {
            // The gitdir file names <checkout>/.git; its parent is the checkout.
            if let Some(root) = resolve(contents.trim()).parent().map(Path::to_path_buf) {
                if exists(&root) {
                    roots.push(root.to_string_lossy().into_owned());
                }
            }
        }
        // A half-pruned entry: no gitdir file, or one we cannot read.
    }

    roots.sort();
    roots
}

// Resolves paths only — nothing is created until ensure_drip_project, so
// read-only commands (--list, --help, usage errors) never mutate the cwd.
pub fn resolve_drip_project(
    cwd: &str,
    home_root: &str,
    override_dir: Option<&str>,
) -> Result<DripProject> {
    let project_root = match override_dir {
        None => Some(resolve_drip_project_root(cwd, home_root)),
        Some(_) => None,
    };
    let data_dir: PathBuf = match &project_root {
        None => resolve(override_dir.unwrap()),
        Some(root) => join(root, ".drip"),
    };

    // Guarded for BOTH shapes before anything else: an override aimed at the
    // global home, or a cwd whose .drip *is* the global home, would otherwise
    // mix the shared home into a project tree.
    let home_root_resolved = resolve(home_root);
    let project_root_is_home = project_root
        .as_deref()
        .map(|root| join(root, ".drip") == home_root_resolved)
        .unwrap_or(false);

    if data_dir == home_root_resolved || project_root_is_home {
        return Err(anyhow!(
            "The global drip home ({}) cannot be a project. Run drip from a project directory, or point --project-dir / DRIP_PROJECT_DIR somewhere else.",
            home_root_resolved.to_string_lossy()
        ));
    }

    // A --project-dir override must stay fully self-contained: sandboxes and
    // eval harnesses point it at a temp dir precisely so nothing leaks into the
    // real ~/.drip, so an override keeps sessions, index and memory inside
    // itself and drops the worktree plumbing entirely.
    let project_root = match project_root {
        None => {
            return Ok(DripProject {
                home_root: home_root.to_string(),
                index_db_path: join(&data_dir, "index.sqlite").to_string_lossy().into_owned(),
                legacy_index_db_path: None,
                legacy_sessions_dir: None,
                memory_dir: join(&data_dir, "memory").to_string_lossy().into_owned(),
                project_root: None,
                repo_root: None,
                repo_slug: project_slug(&data_dir.to_string_lossy()),
                root: data_dir.to_string_lossy().into_owned(),
                sessions_dir: join(&data_dir, "sessions").to_string_lossy().into_owned(),
                slug: project_slug(&data_dir.to_string_lossy()),
                worktree_root: None,
            });
        }
        Some(root) => root,
    };

    // Sessions and the index are keyed by the CHECKOUT the run happened in, so
    // each worktree gets a home of its own: a run started in
    // <repo>/.worktrees/abc123 lands in ~/.drip/projects/<slug of that path>/,
    // separate from main's. Worktrees are disposable — what a session learned
    // should outlive them — so memory stays keyed by the REPOSITORY's slug,
    // shared by every worktree of the repo.
    let worktree_root = nearest_git_ancestor(cwd).unwrap_or_else(|| project_root.clone());
    let repo_root = resolve_git_repo_root(&worktree_root).unwrap_or_else(|| worktree_root.clone());
    let slug = project_slug(&worktree_root);
    let repo_slug = project_slug(&repo_root);
    let project_home = join(&home_root, &format!("projects/{slug}"));
    let legacy_sessions_dir = join(&data_dir, "sessions");
    let legacy_index_db_path = join(&data_dir, "index.sqlite");

    Ok(DripProject {
        home_root: home_root.to_string(),
        index_db_path: join(&project_home, "index.sqlite").to_string_lossy().into_owned(),
        // Legacy locations are advertised only when they actually exist, so every
        // dual-read path can treat "absent" as "nothing to merge" without probing.
        legacy_index_db_path: if exists(&legacy_index_db_path) {
            Some(legacy_index_db_path.to_string_lossy().into_owned())
        } else {
            None
        },
        legacy_sessions_dir: if exists(&legacy_sessions_dir) {
            Some(legacy_sessions_dir.to_string_lossy().into_owned())
        } else {
            None
        },
        memory_dir: join(&home_root, &format!("projects/{repo_slug}/memory"))
            .to_string_lossy()
            .into_owned(),
        project_root: Some(project_root),
        repo_root: Some(repo_root),
        repo_slug,
        root: data_dir.to_string_lossy().into_owned(),
        sessions_dir: join(&project_home, "sessions").to_string_lossy().into_owned(),
        slug,
        worktree_root: Some(worktree_root),
    })
}

pub fn ensure_drip_project(project: &DripProject) -> DripProject {
    for dir in [&project.root, &project.sessions_dir, &project.memory_dir] {
        let _ = fs::create_dir_all(dir);
    }

    let gitignore_path = join(&project.root, ".gitignore");
    let desired = if project.legacy_sessions_dir.is_some() {
        PROJECT_GITIGNORE_WITH_LEGACY_SESSIONS
    } else {
        PROJECT_GITIGNORE
    };

    if !exists(&gitignore_path) {
        let _ = fs::write(&gitignore_path, desired);
    } else {
        // Upgrade path: ONLY a byte-exact older default is rewritten (entries
        // added later, e.g. patches.jsonl). A file the user customized survives
        // byte-identical — that invariant is tested.
        if let Ok(existing) = fs::read_to_string(&gitignore_path) {
            if PREVIOUS_PROJECT_GITIGNORES.contains(&existing.as_str()) && existing != desired {
                let _ = fs::write(&gitignore_path, desired);
            }
        }
    }

    project.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    // realpath'd temp root — the analogue of the TS tests' makeTempRoot.
    fn temp_root(tag: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap().join(tag);
        fs::create_dir_all(&root).unwrap();
        (dir, root)
    }

    fn s(path: &Path) -> String {
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn resolves_the_home_root_from_drip_home_or_the_home_directory() {
        assert_eq!(
            resolve_drip_home_root_from(Some("/custom/drip")),
            resolve("/custom/drip").to_string_lossy()
        );

        let root = resolve_drip_home_root_from(None);
        assert!(root.ends_with(".drip"), "{root}");
    }

    #[test]
    fn slugs_a_cwd_into_a_single_flat_directory_name() {
        assert_eq!(project_slug("/Users/tyler/src/my-app"), "-Users-tyler-src-my-app");
        assert_eq!(
            project_slug("/Users/tyler/src/weird name (v2)"),
            "-Users-tyler-src-weird-name-v2-"
        );
    }

    #[test]
    fn creates_the_global_home_layout() {
        let (_guard, root) = temp_root("drip-test-");
        let home = open_drip_home(&s(&join(&root, ".drip")));

        assert!(Path::new(&home.root).exists());
        assert!(Path::new(&home.skills_dir).exists());
        assert!(Path::new(&home.marketplaces_dir).exists());
        assert_eq!(home.skills_dir, s(&join(&root, ".drip/skills")));
        assert_eq!(home.marketplaces_dir, s(&join(&root, ".drip/marketplaces")));
    }

    #[test]
    fn keeps_sessions_and_the_index_under_the_global_home_repo_scoped_data_in_cwd_drip() {
        let (_guard, cwd) = temp_root("drip-test-");
        let home_root = s(&join(&cwd, "fake-home"));
        let project = resolve_drip_project(&s(&cwd), &home_root, None).unwrap();
        let project_home = join(&home_root, &format!("projects/{}", project_slug(&s(&cwd))));

        // Machine-local per-project data lives beside memory under the global home,
        // keyed by the project root's slug — one place a viewer can be pointed at.
        assert_eq!(project.sessions_dir, s(&join(&project_home, "sessions")));
        assert_eq!(project.index_db_path, s(&join(&project_home, "index.sqlite")));
        assert_eq!(project.memory_dir, s(&join(&project_home, "memory")));
        assert_eq!(project.slug, project_slug(&s(&cwd)));
        assert_eq!(project.project_root.as_deref(), Some(s(&cwd).as_str()));
        // Repo-scoped data (patches.jsonl, async-tools/, skills/, roles.json) stays
        // in the repo itself, so it commits and travels with checkouts.
        assert_eq!(project.root, s(&join(&cwd, ".drip")));
        assert_eq!(project.repo_root.as_deref(), Some(s(&cwd).as_str()));
        assert_eq!(project.repo_slug, project_slug(&s(&cwd)));
        assert_eq!(project.worktree_root.as_deref(), Some(s(&cwd).as_str()));

        let ensured = ensure_drip_project(&project);
        assert!(Path::new(&ensured.root).exists());
        assert!(Path::new(&ensured.sessions_dir).exists());
        assert!(Path::new(&ensured.memory_dir).exists());
        // Fresh project: the default ignore file, and no pre-move tree advertised.
        assert_eq!(
            fs::read_to_string(join(&ensured.root, ".gitignore")).unwrap(),
            PROJECT_GITIGNORE
        );
        assert!(ensured.legacy_sessions_dir.is_none());
        assert!(ensured.legacy_index_db_path.is_none());

        // A user-customized ignore file survives byte-identical.
        fs::write(join(&ensured.root, ".gitignore"), "custom/\n").unwrap();
        ensure_drip_project(&ensured);
        assert_eq!(fs::read_to_string(join(&ensured.root, ".gitignore")).unwrap(), "custom/\n");

        // A byte-exact older default is upgraded to the current one.
        fs::write(join(&ensured.root, ".gitignore"), PREVIOUS_PROJECT_GITIGNORES[1]).unwrap();
        ensure_drip_project(&ensured);
        assert_eq!(fs::read_to_string(join(&ensured.root, ".gitignore")).unwrap(), PROJECT_GITIGNORE);
    }

    #[test]
    fn discovers_the_project_root_from_the_nearest_drip_then_git_then_cwd() {
        let (_guard, root) = temp_root("drip-test-");
        let home_root = s(&join(&root, "fake-home"));
        let nested = join(&root, "repo/src/deep");

        // No markers anywhere: the cwd itself is the project root.
        assert_eq!(resolve_drip_project_root(&s(&nested), &home_root), s(&nested));

        // The nearest ancestor with a .git wins over the cwd…
        fs::create_dir_all(join(&root, "repo/.git")).unwrap();
        assert_eq!(resolve_drip_project_root(&s(&nested), &home_root), s(&join(&root, "repo")));

        // …but the nearest ancestor already holding a .drip wins over that.
        fs::create_dir_all(join(&root, "repo/src/.drip")).unwrap();
        assert_eq!(resolve_drip_project_root(&s(&nested), &home_root), s(&join(&root, "repo/src")));
    }

    #[test]
    fn keys_sessions_to_the_checkout_and_memory_to_the_repo() {
        let (_guard, root) = temp_root("drip-test-");
        let home_root = s(&join(&root, "fake-home"));
        let main = join(&root, "repo");
        let linked = join(&main, ".worktrees/abc123");
        let elsewhere = join(&root, "detached-wt");

        // A linked worktree's .git is a FILE pointing into the main checkout's
        // .git/worktrees/<name>, whose gitdir file points back at the checkout.
        for (name, checkout) in [("abc123", &linked), ("detached", &elsewhere)] {
            fs::create_dir_all(join(&main, &format!(".git/worktrees/{name}"))).unwrap();
            fs::write(
                join(&main, &format!(".git/worktrees/{name}/gitdir")),
                format!("{}\n", s(&join(checkout, ".git"))),
            )
            .unwrap();
            fs::create_dir_all(join(checkout, "src")).unwrap();
            fs::write(
                join(checkout, ".git"),
                format!("gitdir: {}\n", s(&join(&main, &format!(".git/worktrees/{name}")))),
            )
            .unwrap();
        }

        assert_eq!(resolve_git_repo_root(&s(&main)).as_deref(), Some(s(&main).as_str()));
        assert_eq!(resolve_git_repo_root(&s(&linked)).as_deref(), Some(s(&main).as_str()));
        assert_eq!(resolve_git_repo_root(&s(&elsewhere)).as_deref(), Some(s(&main).as_str()));
        assert_eq!(resolve_git_repo_root(&s(&root)), None);
        let mut expected = vec![s(&linked), s(&elsewhere)];
        expected.sort();
        assert_eq!(list_linked_worktree_roots(&s(&main)), expected);
        assert_eq!(list_linked_worktree_roots(&s(&linked)), Vec::<String>::new());

        // Whether or not the main checkout holds a .drip, every checkout keeps
        // its own session home (from a subdirectory too) while memory is shared
        // under the main checkout's slug.
        for main_has_drip in [false, true] {
            if main_has_drip {
                fs::create_dir(join(&main, ".drip")).unwrap();
            }

            for (cwd, worktree) in [
                (main.clone(), main.clone()),
                (linked.clone(), linked.clone()),
                (join(&linked, "src"), linked.clone()),
                (elsewhere.clone(), elsewhere.clone()),
            ] {
                let project = resolve_drip_project(&s(&cwd), &home_root, None).unwrap();

                assert_eq!(project.slug, project_slug(&s(&worktree)));
                assert_eq!(project.worktree_root.as_deref(), Some(s(&worktree).as_str()));
                assert_eq!(
                    project.sessions_dir,
                    s(&join(
                        &home_root,
                        &format!("projects/{}/sessions", project_slug(&s(&worktree)))
                    ))
                );
                assert_eq!(
                    project.index_db_path,
                    s(&join(
                        &home_root,
                        &format!("projects/{}/index.sqlite", project_slug(&s(&worktree)))
                    ))
                );
                assert_eq!(project.repo_root.as_deref(), Some(s(&main).as_str()));
                assert_eq!(project.repo_slug, project_slug(&s(&main)));
                assert_eq!(
                    project.memory_dir,
                    s(&join(
                        &home_root,
                        &format!("projects/{}/memory", project_slug(&s(&main)))
                    ))
                );
                assert_eq!(project.home_root, home_root);
            }
        }

        // Outside git entirely the repo root is simply the project root.
        let plain = join(&root, "plain");
        fs::create_dir(&plain).unwrap();
        let project = resolve_drip_project(&s(&plain), &home_root, None).unwrap();
        assert_eq!(project.repo_root.as_deref(), Some(s(&plain).as_str()));
        assert_eq!(project.worktree_root.as_deref(), Some(s(&plain).as_str()));
        assert_eq!(project.slug, project_slug(&s(&plain)));
        assert_eq!(project.repo_slug, project_slug(&s(&plain)));
    }

    #[test]
    fn refuses_to_use_the_global_home_as_the_project_data_dir() {
        let (_guard, root) = temp_root("drip-test-");
        let home_root = s(&join(&root, ".drip"));
        fs::create_dir_all(&home_root).unwrap();

        // cwd == parent of the global home (the $HOME case): the home's .drip is
        // skipped by discovery, and an explicit override into it throws.
        assert_eq!(resolve_drip_project_root(&s(&root), &home_root), s(&root));
        assert!(resolve_drip_project(&s(&root), &home_root, None)
            .unwrap_err()
            .to_string()
            .contains("global drip home"));
        assert!(resolve_drip_project(&s(&root), &home_root, Some(&home_root))
            .unwrap_err()
            .to_string()
            .contains("global drip home"));
    }

    #[test]
    fn a_project_dir_override_is_fully_self_contained() {
        let (_guard, root) = temp_root("drip-test-");
        let home_root = s(&join(&root, "fake-home"));
        let override_dir = s(&join(&root, "sandbox"));

        let project = resolve_drip_project(&s(&root), &home_root, Some(&override_dir)).unwrap();

        assert_eq!(project.root, override_dir);
        assert_eq!(project.sessions_dir, s(&join(&override_dir, "sessions")));
        assert_eq!(project.index_db_path, s(&join(&override_dir, "index.sqlite")));
        assert_eq!(project.memory_dir, s(&join(&override_dir, "memory")));
        assert_eq!(project.slug, project_slug(&override_dir));
        assert_eq!(project.repo_slug, project_slug(&override_dir));
        assert!(project.project_root.is_none());
        assert!(project.worktree_root.is_none());

        let ensured = ensure_drip_project(&project);
        assert!(Path::new(&ensured.root).exists());
        assert!(Path::new(&ensured.sessions_dir).exists());
    }
}
