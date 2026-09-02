// port of src/chat/file-suggestions-server.ts (the workspace-file half; the
// tmux-session suggestions belong to the web chat and are not ported).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const DEFAULT_FILE_SUGGESTION_LIMIT: usize = 8;
const FILE_INDEX_TTL: Duration = Duration::from_millis(5_000);
const IGNORED_DIRECTORY_NAMES: &[&str] = &[".git", ".next", ".turbo", "build", "coverage", "dist", "node_modules"];

fn format_relative_path(cwd: &Path, target: &Path) -> String {
    match target.strip_prefix(cwd) {
        Ok(relative) if !relative.as_os_str().is_empty() => relative.to_string_lossy().to_string(),
        _ => target
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default(),
    }
}

/// Every regular file under `cwd`, depth-first, siblings sorted by name,
/// skipping the usual build/dependency directories. Paths are cwd-relative.
pub fn collect_workspace_files(cwd: &Path) -> std::io::Result<Vec<String>> {
    let mut collected = Vec::new();
    walk(cwd, cwd, &mut collected)?;
    Ok(collected)
}

fn walk(cwd: &Path, current: &Path, collected: &mut Vec<String>) -> std::io::Result<()> {
    let mut entries: Vec<(String, PathBuf, std::fs::FileType)> = std::fs::read_dir(current)?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            let file_type = entry.file_type().ok()?;
            Some((name, entry.path(), file_type))
        })
        .filter(|(name, _, _)| !IGNORED_DIRECTORY_NAMES.contains(&name.as_str()))
        .collect();
    entries.sort_by(|left, right| left.0.cmp(&right.0));

    for (_, path, file_type) in entries {
        if file_type.is_dir() {
            walk(cwd, &path, collected)?;
        } else if file_type.is_file() {
            collected.push(format_relative_path(cwd, &path));
        }
    }
    Ok(())
}

fn file_suggestion_score(path: &str, query: &str) -> Option<f64> {
    let normalized_path = path.to_lowercase();
    let normalized_query = query.trim().to_lowercase();
    let base_name = normalized_path.rsplit('/').next().unwrap_or("").to_string();

    if normalized_query.is_empty() {
        return Some(path.split('/').count() as f64 * 10.0 + normalized_path.chars().count() as f64 / 100.0);
    }
    if normalized_path == normalized_query {
        return Some(0.0);
    }
    if base_name == normalized_query {
        return Some(1.0);
    }
    if normalized_path.starts_with(&normalized_query) {
        return Some(2.0);
    }
    if base_name.starts_with(&normalized_query) {
        return Some(3.0);
    }
    if let Some(index) = normalized_path.find(&format!("/{normalized_query}")) {
        return Some(4.0 + char_index(&normalized_path, index) as f64 / 1_000.0);
    }
    if let Some(index) = normalized_path.find(&normalized_query) {
        return Some(5.0 + char_index(&normalized_path, index) as f64 / 1_000.0);
    }
    None
}

fn char_index(text: &str, byte_index: usize) -> usize {
    text[..byte_index].chars().count()
}

/// Ranked cwd-relative paths for a mention query (best first, at most `limit`).
pub fn get_workspace_file_suggestions(files: &[String], query: &str, limit: usize) -> Vec<String> {
    let mut scored: Vec<(f64, &String)> = files
        .iter()
        .filter_map(|path| file_suggestion_score(path, query).map(|score| (score, path)))
        .collect();
    scored.sort_by(|left, right| {
        left.0
            .partial_cmp(&right.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(left.1.chars().count().cmp(&right.1.chars().count()))
            .then(left.1.cmp(right.1))
    });
    scored.into_iter().take(limit.max(1)).map(|(_, path)| path.clone()).collect()
}

/// A cwd-scoped file index with a short TTL, so a burst of keystrokes shares
/// one directory walk.
pub struct WorkspaceFileSource {
    cwd: PathBuf,
    cached: Option<(Instant, Vec<String>)>,
}

impl WorkspaceFileSource {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self { cwd: cwd.into(), cached: None }
    }

    pub fn load(&mut self) -> std::io::Result<Vec<String>> {
        if let Some((at, files)) = &self.cached {
            if at.elapsed() < FILE_INDEX_TTL {
                return Ok(files.clone());
            }
        }
        let files = collect_workspace_files(&self.cwd)?;
        self.cached = Some((Instant::now(), files.clone()));
        Ok(files)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files() -> Vec<String> {
        ["src/app.ts", "src/lib/app-helpers.ts", "README.md", "docs/app.md", "app.ts"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    #[test]
    fn ranks_exact_then_basename_then_prefix_then_segment() {
        let ranked = get_workspace_file_suggestions(&files(), "app.ts", 8);
        assert_eq!(ranked[0], "app.ts");
        assert_eq!(ranked[1], "src/app.ts");
        assert!(!ranked.contains(&"README.md".to_string()));
        let prefix = get_workspace_file_suggestions(&files(), "app", 8);
        assert_eq!(prefix[0], "app.ts");
        assert!(prefix.contains(&"src/lib/app-helpers.ts".to_string()));
    }

    #[test]
    fn empty_query_prefers_shallow_short_paths_and_respects_limit() {
        let ranked = get_workspace_file_suggestions(&files(), "", 2);
        assert_eq!(ranked, vec!["app.ts".to_string(), "README.md".to_string()]);
    }

    #[test]
    fn collects_files_relative_to_cwd_skipping_ignored_dirs() {
        let dir = std::env::temp_dir().join(format!("drip-file-suggestions-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules/x")).unwrap();
        std::fs::write(dir.join("src/a.rs"), "").unwrap();
        std::fs::write(dir.join("b.md"), "").unwrap();
        std::fs::write(dir.join("node_modules/x/ignored.js"), "").unwrap();
        let collected = collect_workspace_files(&dir).unwrap();
        assert_eq!(collected, vec!["b.md".to_string(), "src/a.rs".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
