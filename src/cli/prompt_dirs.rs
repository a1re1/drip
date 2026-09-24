// On-disk system prompt profiles: one directory per profile, holding the
// profile blob and its prompt as separate files.
//
// ~/.drip/prompts/<name>/config.json   the profile — id, label, toolAccess,
//                                      toolNames; never the prompt
// ~/.drip/prompts/<name>/prompt.md     the prompt prose
//
// The layout mirrors the role-profile directories in profile_dirs.rs, so a
// prompt is a Markdown file you can read and edit instead of a JSON string
// buried in ~/.drip/config.json. The directory name is a sanitized form of the
// profile id; the id itself lives in config.json (`id` is optional there and
// defaults to the directory name), and prompt.md wins over any `prompt` field
// left in config.json.
//
// Two pieces keep the legacy setting working:
//   * `migrate_legacy_system_prompt_profiles` lifts the entries still listed in
//     `runtime.system_prompt_profiles` into directories and clears the setting
//     once every entry is accounted for. It is called from load_cli_config. A
//     directory that already names the id but carries no prompt at all adopts
//     the legacy prompt into `prompt.md` first, so clearing the setting never
//     drops prose that only the setting held.
//   * `merge_dir_profiles_into_settings` folds the directory profiles into an
//     in-memory settings map, so every existing resolve path
//     (resolve_cli_inference, the /prompt picker) sees them without having to
//     thread a path through. An entry with the same id in the setting — the
//     single source that existed before directories — wins.

use anyhow::{Context, Result};
use indexmap::IndexMap;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

use crate::cli::profile_dirs::profile_dir_name;
use crate::core::config::{
    normalize_system_prompt_profile, parse_settings_json_array, SystemPromptProfile,
    SYSTEM_PROMPT_PROFILES_SETTING_ID,
};

/// The profiles root under a drip home.
pub const PROMPTS_DIR_NAME: &str = "prompts";
/// The profile blob inside one prompt directory.
pub const PROMPT_CONFIG_FILE: &str = "config.json";
/// The prompt file inside one prompt directory.
pub const PROMPT_PROMPT_FILE: &str = "prompt.md";
/// The origin label used in issues raised while reading the directory.
pub const PROMPTS_ORIGIN: &str = "~/.drip/prompts";

/// Seeded next to the prompts root so the layout is discoverable; an existing
/// file is never overwritten.
pub const PROMPTS_README: &str = "# System prompt profiles

One directory per system prompt profile. Each directory holds:

- `config.json` — the profile without its prompt: `id`, `label`, `toolAccess`,
  `toolNames`. This is the same blob a `runtime.system_prompt_profiles` entry
  carries in config.json.
- `prompt.md` — the prompt itself, as plain prose. Edit it here.

The directory name is a sanitized form of the profile's `id`; the id itself
lives in `config.json` (`id` is optional there and defaults to the directory
name). `prompt.md` wins over any `prompt` field left in `config.json`.

Profiles still listed in `runtime.system_prompt_profiles` inside
`~/.drip/config.json` are migrated into this layout automatically on the next
load, and the setting is cleared. An entry left in that setting with the same
id as a directory profile overrides the directory one.
";

/// `<config dir>/prompts` — the prompt-profile root that sits beside the config
/// file the profiles were loaded from.
pub fn prompts_dir_for(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(PROMPTS_DIR_NAME)
}

/// Every prompt profile directly under `root`, in stable (directory-name)
/// order so which of two same-named profiles wins never depends on readdir.
///
/// A missing root is the normal case for a user who never created one: not an
/// issue, just no profiles.
pub fn load_prompts_from_dir(
    root: &Path,
    origin: &str,
    issues: &mut Vec<String>,
) -> Vec<SystemPromptProfile> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };

    let mut dirs: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();

    let mut profiles: Vec<SystemPromptProfile> = Vec::new();
    // id → the origin label of the directory that defined it, so a collision can
    // name both locations.
    let mut seen_ids: Vec<(String, String)> = Vec::new();
    for dir in dirs {
        let slug = dir
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_string();
        let config_path = dir.join(PROMPT_CONFIG_FILE);
        let origin_label = format!("{origin}/{slug}/{PROMPT_CONFIG_FILE}");

        if !config_path.exists() {
            // A prompt-only directory is a half-written profile: say so rather
            // than silently dropping the prompt the user wrote.
            if dir.join(PROMPT_PROMPT_FILE).is_file() {
                issues.push(format!(
                    "{origin}/{slug}: missing {PROMPT_CONFIG_FILE} (the prompt has no profile to attach to)"
                ));
            }
            continue;
        }

        let raw = match fs::read_to_string(&config_path) {
            Ok(raw) => raw,
            Err(error) => {
                issues.push(format!("{origin_label}: {error}"));
                continue;
            }
        };
        let mut value: Value = match serde_json::from_str(&raw) {
            Ok(value) => value,
            Err(error) => {
                issues.push(format!("{origin_label}: {error}"));
                continue;
            }
        };

        let Some(object) = value.as_object_mut() else {
            issues.push(format!("{origin_label}: not a JSON object"));
            continue;
        };

        // The directory names the profile: `id` is optional in config.json.
        if object
            .get("id")
            .and_then(|id| id.as_str())
            .map(|id| id.trim().is_empty())
            .unwrap_or(true)
        {
            object.insert("id".to_string(), Value::from(slug.clone()));
        }

        // prompt.md is the prompt: it wins over an inline `prompt` field left in
        // config.json, so the prose file is the one place to edit.
        if let Ok(prompt) = fs::read_to_string(dir.join(PROMPT_PROMPT_FILE)) {
            let prompt = prompt.trim_end();
            if !prompt.trim().is_empty() {
                object.insert("prompt".to_string(), Value::from(prompt.to_string()));
            }
        }

        match normalize_system_prompt_profile(&value, profiles.len()) {
            Ok(profile) => {
                // `id` is authoritative, so two directories can claim one
                // profile and the directory names cannot tell them apart. The
                // resolver rejects duplicate ids outright: name both
                // directories here instead of letting it fail later without
                // saying where the collision is.
                match seen_ids.iter().find(|(seen, _)| *seen == profile.id) {
                    Some((_, first)) => issues.push(format!(
                        "{origin_label}: duplicate profile id \"{}\" (already defined by {first}); \
                         rename one of the two directories or ids",
                        profile.id
                    )),
                    None => seen_ids.push((profile.id.clone(), origin_label.clone())),
                }
                profiles.push(profile);
            }
            Err(error) => issues.push(format!("{origin_label}: {error}")),
        }
    }

    profiles
}

/// Writes one prompt directory: config.json (the profile with its prompt
/// stripped) plus prompt.md when the profile has a prompt.
pub fn write_prompt_dir(dir: &Path, profile: &SystemPromptProfile) -> Result<()> {
    let mut blob = serde_json::to_value(profile)
        .with_context(|| format!("serialize prompt profile \"{}\"", profile.id))?;
    let prompt = blob
        .as_object_mut()
        .and_then(|object| object.remove("prompt"))
        .and_then(|prompt| match prompt {
            Value::String(prompt) => Some(prompt),
            _ => None,
        });
    let body = serde_json::to_string_pretty(&blob)
        .with_context(|| format!("serialize prompt profile \"{}\"", profile.id))?;
    crate::lib_fs::write_file_atomic(&dir.join(PROMPT_CONFIG_FILE), &format!("{body}\n"), true)?;

    if let Some(prompt) = prompt
        .as_deref()
        .map(str::trim_end)
        .filter(|prompt| !prompt.trim().is_empty())
    {
        crate::lib_fs::write_file_atomic(
            &dir.join(PROMPT_PROMPT_FILE),
            &format!("{prompt}\n"),
            true,
        )?;
    }

    Ok(())
}

/// Creates `<home>/prompts/` and seeds a README inside it, so the layout is
/// discoverable. An existing README is left byte-identical.
pub fn ensure_prompts_dir(home_root: &Path) {
    let root = home_root.join(PROMPTS_DIR_NAME);
    if fs::create_dir_all(&root).is_err() {
        return;
    }
    let readme = root.join("README.md");
    if !readme.exists() {
        let _ = crate::lib_fs::write_file_atomic(&readme, PROMPTS_README, true);
    }
}

/// Lifts the legacy `runtime.system_prompt_profiles` setting into
/// `<config dir>/prompts/<name>/{config.json,prompt.md}` and clears the setting
/// once every profile is on disk.
///
/// Returns true only when every entry in the setting is accounted for on disk:
/// the profiles were written now, or a directory for them already existed (a
/// migration that already ran, or a hand-authored profile with the same id) —
/// and a directory that names the id but carries no prompt at all has adopted
/// the legacy prompt first, so clearing the setting never drops the only copy
/// of that prose.
/// Anything that leaves an entry unaccounted for — a write failure, a duplicate
/// id, or an entry the profile normalizer could not lift — reports its issue and
/// leaves the setting alone, so the next load retries. Clearing the setting
/// while an entry was dropped would destroy that entry with a warning as the
/// only trace.
///
/// Liftable entries are still written on a partial failure, so fixing the
/// offending entry and reloading finishes the migration.
pub fn migrate_legacy_system_prompt_profiles(
    config_path: &Path,
    settings: &mut IndexMap<String, String>,
) -> bool {
    let raw = settings
        .get(SYSTEM_PROMPT_PROFILES_SETTING_ID)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .unwrap_or("[]")
        .to_string();
    if raw == "[]" {
        return false;
    }

    let items = match parse_settings_json_array("System Prompt Profiles", &raw) {
        Ok(items) => items,
        // A malformed value is reported by the profile parser; never migrate it.
        Err(_) => return false,
    };
    if items.is_empty() {
        return false;
    }

    let root = prompts_dir_for(config_path);
    let mut written: Vec<PathBuf> = Vec::new();
    let mut seen_ids: Vec<String> = Vec::new();
    let mut duplicates = false;
    let mut settled = 0usize;

    for (index, item) in items.iter().enumerate() {
        let profile = match normalize_system_prompt_profile(item, index) {
            Ok(profile) => profile,
            Err(error) => {
                eprintln!(
                    "warning: {}: {error}; {SYSTEM_PROMPT_PROFILES_SETTING_ID} is kept until every entry migrates",
                    config_path.display()
                );
                continue;
            }
        };

        // Two entries with one id are one profile to the resolver (and a
        // duplicate-id error when it parses them), so writing only the first
        // directory would drop the second entry. Keep the setting instead.
        if seen_ids.iter().any(|seen| seen == &profile.id) {
            eprintln!(
                "warning: {}: {SYSTEM_PROMPT_PROFILES_SETTING_ID} defines \"{}\" more than once; \
                 merge the duplicate entries so migration can place this profile",
                config_path.display(),
                profile.id
            );
            duplicates = true;
            continue;
        }
        seen_ids.push(profile.id.clone());

        let base = profile_dir_name(&profile.id);
        let dir_path = root.join(&base);
        match dir_holds_prompt(&dir_path, &profile.id) {
            // Already on disk under its own id — hand-authored, or an earlier
            // migration run. Its own files are never overwritten, but a
            // directory that carries no prompt at all adopts the legacy one
            // before the setting may clear: the setting then holds the only
            // copy of that text, and clearing would delete it with no trace.
            Some(true) => {
                if adopt_existing_prompt_dir(&dir_path, &profile) {
                    settled += 1;
                }
                continue;
            }
            // An existing directory whose config.json omits `id`: the loader
            // names it after the directory, so this may or may not be the same
            // profile. Never overwrite it, but never call the legacy entry
            // migrated either — clearing the setting here would discard it with
            // no trace.
            None if dir_path.join(PROMPT_CONFIG_FILE).exists() => {
                eprintln!(
                    "warning: {}: {} has no \"id\" and {} takes its id from the \
                     directory; add an \"id\" (or remove the directory) so migration can place \
                     \"{}\". {SYSTEM_PROMPT_PROFILES_SETTING_ID} is kept until every entry migrates",
                    config_path.display(),
                    dir_path.join(PROMPT_CONFIG_FILE).display(),
                    PROMPT_PROMPT_FILE,
                    profile.id
                );
                continue;
            }
            _ => {}
        }

        match free_prompt_dir(&root, &base, &written) {
            Some(dir) => match write_prompt_dir(&dir, &profile) {
                Ok(()) => {
                    written.push(dir);
                    settled += 1;
                }
                Err(error) => {
                    eprintln!(
                        "warning: {}: could not migrate system prompt profile \"{}\" to {}: {error}",
                        config_path.display(),
                        profile.id,
                        dir.display()
                    );
                }
            },
            None => {
                eprintln!(
                    "warning: {}: could not find a free prompt directory for \"{}\" under {}",
                    config_path.display(),
                    profile.id,
                    root.display()
                );
            }
        }
    }

    let migrated = settled == items.len() && !duplicates;
    if migrated {
        settings.insert(
            SYSTEM_PROMPT_PROFILES_SETTING_ID.to_string(),
            "[]".to_string(),
        );
    }
    migrated
}

/// Whether `dir` is already the prompt directory of `id`.
///
/// `Some(true)` — its config.json names this id. `Some(false)` — it names a
/// different id, or its config.json is unreadable/malformed (the loader reports
/// that one; migration must not treat it as migrated). `None` — no config.json
/// at all, or one that omits `id`, so the loader would call it whatever the
/// directory is called: callers must not assume it is this profile.
fn dir_holds_prompt(dir: &Path, id: &str) -> Option<bool> {
    let Ok(raw) = fs::read_to_string(dir.join(PROMPT_CONFIG_FILE)) else {
        return None;
    };
    let Ok(value) = serde_json::from_str::<Value>(&raw) else {
        return Some(false);
    };
    let existing = value.get("id").and_then(|value| value.as_str())?;
    if existing.trim().is_empty() {
        return None;
    }
    Some(existing.trim() == id.trim())
}

/// Adopts an existing `<root>/<base>` directory that already names `id` as the
/// home of the legacy entry.
///
/// The directory is never rewritten — a prompt the user authored is theirs, and
/// an inline `prompt` in its `config.json` counts as prose too. What this does
/// cover is the shape a half-finished migration leaves behind: the profile blob
/// landed, `prompt.md` did not (a full disk, a crash), or a hand-authored
/// directory claims the id without ever holding a prompt. The legacy entry is
/// then the only copy of that text, so it is written out as `prompt.md` before
/// the setting is allowed to clear.
///
/// Returns true when the directory and the legacy entry now agree; false only
/// when the prompt could not be written, which keeps the setting in place for
/// the next load to retry.
fn adopt_existing_prompt_dir(dir: &Path, profile: &SystemPromptProfile) -> bool {
    let prompt = profile.prompt.trim_end();
    if prompt.trim().is_empty() || dir_has_a_prompt(dir) {
        return true;
    }
    match crate::lib_fs::write_file_atomic(
        &dir.join(PROMPT_PROMPT_FILE),
        &format!("{prompt}\n"),
        true,
    ) {
        Ok(()) => true,
        Err(error) => {
            eprintln!(
                "warning: {}: {} names \"{}\" but {PROMPT_PROMPT_FILE} could not be written: {error}; \
                 {SYSTEM_PROMPT_PROFILES_SETTING_ID} is kept until every entry migrates",
                dir.display(),
                PROMPT_CONFIG_FILE,
                profile.id
            );
            false
        }
    }
}

/// Whether the directory already carries a prompt: `prompt.md` with text in it,
/// or an inline `prompt` field in its `config.json`, which the loader falls back
/// to when `prompt.md` is empty.
fn dir_has_a_prompt(dir: &Path) -> bool {
    if let Ok(prompt) = fs::read_to_string(dir.join(PROMPT_PROMPT_FILE)) {
        if !prompt.trim().is_empty() {
            return true;
        }
    }
    let Ok(raw) = fs::read_to_string(dir.join(PROMPT_CONFIG_FILE)) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<Value>(&raw) else {
        return false;
    };
    value
        .get("prompt")
        .and_then(|prompt| prompt.as_str())
        .map(|prompt| !prompt.trim().is_empty())
        .unwrap_or(false)
}

/// The first free `<root>/<base>[-N]` directory name, ignoring names already
/// claimed by this migration run.
fn free_prompt_dir(root: &Path, base: &str, taken: &[PathBuf]) -> Option<PathBuf> {
    for suffix in 1..1000u32 {
        let candidate = if suffix == 1 {
            root.join(base)
        } else {
            root.join(format!("{base}-{suffix}"))
        };
        if !candidate.exists() && !taken.iter().any(|seen| seen == &candidate) {
            return Some(candidate);
        }
    }
    None
}

/// The settings map with the `<config dir>/prompts/` profiles folded in, for
/// the resolve paths that only ever see a settings map.
///
/// A directory profile is merged in unless the setting already holds an entry
/// with the same id — the inline entry wins, which keeps an explicit
/// `runtime.system_prompt_profiles` value authoritative. A malformed setting
/// (or no config path at all) leaves the map untouched, so the existing error
/// surfaces from the original value instead of being swallowed here.
pub fn merge_dir_profiles_into_settings(
    settings: &IndexMap<String, String>,
    config_path: Option<&Path>,
) -> IndexMap<String, String> {
    let mut merged = settings.clone();
    let Some(config_path) = config_path else {
        return merged;
    };

    let mut issues: Vec<String> = Vec::new();
    let dir_profiles =
        load_prompts_from_dir(&prompts_dir_for(config_path), PROMPTS_ORIGIN, &mut issues);
    if dir_profiles.is_empty() {
        for issue in &issues {
            eprintln!("warning: {issue}");
        }
        return merged;
    }

    let raw = merged
        .get(SYSTEM_PROMPT_PROFILES_SETTING_ID)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .unwrap_or("[]");
    let inline_items = match parse_settings_json_array("System Prompt Profiles", raw) {
        Ok(items) => items,
        Err(_) => return merged,
    };

    let mut items: Vec<Value> = Vec::new();
    let mut ids: Vec<String> = Vec::new();
    for profile in &dir_profiles {
        ids.push(profile.id.clone());
        match serde_json::to_value(profile) {
            Ok(value) => items.push(value),
            Err(_) => {
                ids.pop();
            }
        }
    }
    for item in inline_items {
        let id = item
            .get("id")
            .and_then(|id| id.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if !id.is_empty() {
            if let Some(position) = ids.iter().position(|seen| *seen == id) {
                items[position] = item;
                continue;
            }
            ids.push(id);
        }
        items.push(item);
    }

    match serde_json::to_string(&Value::Array(items)) {
        Ok(serialized) => {
            merged.insert(SYSTEM_PROMPT_PROFILES_SETTING_ID.to_string(), serialized);
        }
        Err(_) => return settings.clone(),
    }

    for issue in &issues {
        eprintln!("warning: {issue}");
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "drip-prompts-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn migration_writes_config_and_prompt_files_then_clears_the_setting() {
        let dir = temp_dir("migrate");
        let config_path = dir.join("config.json");
        fs::write(&config_path, "{}").unwrap();

        let mut settings: IndexMap<String, String> = IndexMap::new();
        settings.insert(
            SYSTEM_PROMPT_PROFILES_SETTING_ID.to_string(),
            r#"[{"id":"code-reviewer","label":"Code reviewer","prompt":"Review hard.","toolAccess":"all"},{"id":"Scout One","prompt":"Scout it."}]"#
                .to_string(),
        );

        assert!(migrate_legacy_system_prompt_profiles(
            &config_path,
            &mut settings
        ));
        assert_eq!(
            settings
                .get(SYSTEM_PROMPT_PROFILES_SETTING_ID)
                .map(String::as_str),
            Some("[]"),
            "the legacy setting is cleared once every profile is on disk"
        );

        let reviewer = dir.join("prompts").join("code-reviewer");
        let blob: Value =
            serde_json::from_str(&fs::read_to_string(reviewer.join("config.json")).unwrap())
                .unwrap();
        assert_eq!(blob["id"], "code-reviewer");
        assert_eq!(blob["label"], "Code reviewer");
        assert_eq!(blob["toolAccess"], "all");
        // The prompt lives only in prompt.md.
        assert!(blob.get("prompt").is_none(), "{blob}");
        assert_eq!(
            fs::read_to_string(reviewer.join("prompt.md")).unwrap(),
            "Review hard.\n"
        );

        // A profile id that is not a slug still gets a directory.
        assert_eq!(
            fs::read_to_string(dir.join("prompts").join("scout-one").join("prompt.md")).unwrap(),
            "Scout it.\n"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn migration_never_overwrites_a_directory_and_keeps_unusable_entries() {
        let dir = temp_dir("migrate-keep");
        let config_path = dir.join("config.json");
        fs::write(&config_path, "{}").unwrap();

        let existing = dir.join("prompts").join("code-reviewer");
        fs::create_dir_all(&existing).unwrap();
        fs::write(
            existing.join("config.json"),
            r#"{"id":"code-reviewer","prompt":"Mine."}"#,
        )
        .unwrap();

        let mut settings: IndexMap<String, String> = IndexMap::new();
        settings.insert(
            SYSTEM_PROMPT_PROFILES_SETTING_ID.to_string(),
            r#"[{"id":"code-reviewer","prompt":"Theirs."},{"label":"No id at all"}]"#.to_string(),
        );

        // The unusable entry is reported, so the setting survives and the
        // hand-authored directory is left byte-identical.
        assert!(!migrate_legacy_system_prompt_profiles(
            &config_path,
            &mut settings
        ));
        assert!(settings
            .get(SYSTEM_PROMPT_PROFILES_SETTING_ID)
            .map(|value| value.trim() != "[]")
            .unwrap_or(false));
        assert!(fs::read_to_string(existing.join("config.json"))
            .unwrap()
            .contains("Mine."));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_directory_names_the_profile_and_prompt_md_wins_over_an_inline_prompt() {
        let root = temp_dir("dir-name").join(PROMPTS_DIR_NAME);
        let target = root.join("code-reviewer");
        fs::create_dir_all(&target).unwrap();
        fs::write(
            target.join(PROMPT_CONFIG_FILE),
            r#"{"prompt":"inline","label":"Code reviewer"}"#,
        )
        .unwrap();
        fs::write(target.join(PROMPT_PROMPT_FILE), "From prompt.md.\n").unwrap();

        let mut issues = Vec::new();
        let profiles = load_prompts_from_dir(&root, PROMPTS_ORIGIN, &mut issues);
        assert!(issues.is_empty(), "{issues:?}");
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].id, "code-reviewer");
        assert_eq!(profiles[0].prompt, "From prompt.md.");
    }

    #[test]
    fn a_prompt_only_directory_is_reported_not_silently_dropped() {
        let root = temp_dir("half").join(PROMPTS_DIR_NAME);
        fs::create_dir_all(root.join("orphan")).unwrap();
        fs::write(
            root.join("orphan").join(PROMPT_PROMPT_FILE),
            "No config here.\n",
        )
        .unwrap();

        let mut issues = Vec::new();
        let profiles = load_prompts_from_dir(&root, PROMPTS_ORIGIN, &mut issues);
        assert!(profiles.is_empty());
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert!(issues[0].contains(PROMPT_CONFIG_FILE), "{issues:?}");
    }

    #[test]
    fn dir_profiles_merge_in_and_an_inline_entry_with_the_same_id_wins() {
        let dir = temp_dir("merge");
        let config_path = dir.join("config.json");
        let root = prompts_dir_for(&config_path);
        for (slug, id, prompt) in [
            ("dir-only", "dir-only", "From the directory."),
            ("shared", "shared", "Directory text."),
        ] {
            let target = root.join(slug);
            fs::create_dir_all(&target).unwrap();
            fs::write(
                target.join(PROMPT_CONFIG_FILE),
                format!(r#"{{"id":"{id}","label":"Dir"}}"#),
            )
            .unwrap();
            fs::write(target.join(PROMPT_PROMPT_FILE), format!("{prompt}\n")).unwrap();
        }

        let mut settings: IndexMap<String, String> = IndexMap::new();
        settings.insert(
            SYSTEM_PROMPT_PROFILES_SETTING_ID.to_string(),
            r#"[{"id":"shared","prompt":"Inline text."},{"id":"inline-only","prompt":"Inline."}]"#
                .to_string(),
        );

        let merged = merge_dir_profiles_into_settings(&settings, Some(&config_path));
        let profiles = crate::core::config::parse_system_prompt_profiles(&merged).unwrap();
        let ids: Vec<&str> = profiles.iter().map(|profile| profile.id.as_str()).collect();
        assert_eq!(ids, vec!["dir-only", "shared", "inline-only"]);
        assert_eq!(profiles[0].prompt, "From the directory.");
        assert_eq!(profiles[1].prompt, "Inline text.");
        // The caller's map is never mutated: the merge is an in-memory view.
        assert_eq!(
            crate::core::config::parse_system_prompt_profiles(&settings)
                .unwrap()
                .len(),
            2
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_prompts_dir_seeds_a_readme_once() {
        let dir = temp_dir("ensure");
        ensure_prompts_dir(&dir);
        let readme = dir.join(PROMPTS_DIR_NAME).join("README.md");
        assert!(readme.exists());

        fs::write(&readme, "mine\n").unwrap();
        ensure_prompts_dir(&dir);
        assert_eq!(fs::read_to_string(&readme).unwrap(), "mine\n");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_id_match_directory_without_a_prompt_adopts_the_legacy_prompt() {
        let dir = temp_dir("adopt-prompt");
        let config_path = dir.join("config.json");
        fs::write(&config_path, "{}").unwrap();

        // The shape a half-finished migration leaves behind: the profile blob
        // landed, prompt.md did not. The legacy setting holds the only copy of
        // the prompt, so clearing it would delete that text.
        let existing = dir.join(PROMPTS_DIR_NAME).join("code-reviewer");
        fs::create_dir_all(&existing).unwrap();
        fs::write(
            existing.join(PROMPT_CONFIG_FILE),
            r#"{"id":"code-reviewer","label":"Code reviewer","toolAccess":"all"}"#,
        )
        .unwrap();

        let mut settings: IndexMap<String, String> = IndexMap::new();
        settings.insert(
            SYSTEM_PROMPT_PROFILES_SETTING_ID.to_string(),
            r#"[{"id":"code-reviewer","label":"Code reviewer","prompt":"The only copy.","toolAccess":"all"}]"#
                .to_string(),
        );

        assert!(migrate_legacy_system_prompt_profiles(
            &config_path,
            &mut settings
        ));
        assert_eq!(
            settings
                .get(SYSTEM_PROMPT_PROFILES_SETTING_ID)
                .map(String::as_str),
            Some("[]")
        );
        assert_eq!(
            fs::read_to_string(existing.join(PROMPT_PROMPT_FILE)).unwrap(),
            "The only copy.\n",
            "the prompt the setting alone carried is written out before the setting clears"
        );
        // The directory's own profile blob is untouched.
        let blob: Value =
            serde_json::from_str(&fs::read_to_string(existing.join(PROMPT_CONFIG_FILE)).unwrap())
                .unwrap();
        assert_eq!(blob["label"], "Code reviewer");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_id_match_directory_that_already_has_a_prompt_is_never_rewritten() {
        let dir = temp_dir("adopt-keep");
        let config_path = dir.join("config.json");
        fs::write(&config_path, "{}").unwrap();

        let prose = dir.join(PROMPTS_DIR_NAME).join("code-reviewer");
        fs::create_dir_all(&prose).unwrap();
        fs::write(
            prose.join(PROMPT_CONFIG_FILE),
            r#"{"id":"code-reviewer","label":"Code reviewer"}"#,
        )
        .unwrap();
        fs::write(prose.join(PROMPT_PROMPT_FILE), "Mine, keep it.\n").unwrap();

        // An inline prompt counts too: prompt.md is absent, and the loader falls
        // back to the field, so nothing is missing from this directory either.
        let inline = dir.join(PROMPTS_DIR_NAME).join("scout");
        fs::create_dir_all(&inline).unwrap();
        let inline_blob = r#"{"id":"scout","prompt":"Inline, keep it.","label":"Scout"}"#;
        fs::write(inline.join(PROMPT_CONFIG_FILE), inline_blob).unwrap();

        let mut settings: IndexMap<String, String> = IndexMap::new();
        settings.insert(
            SYSTEM_PROMPT_PROFILES_SETTING_ID.to_string(),
            r#"[{"id":"code-reviewer","prompt":"Theirs."},{"id":"scout","prompt":"Theirs too."}]"#
                .to_string(),
        );

        assert!(migrate_legacy_system_prompt_profiles(
            &config_path,
            &mut settings
        ));
        assert_eq!(
            fs::read_to_string(prose.join(PROMPT_PROMPT_FILE)).unwrap(),
            "Mine, keep it.\n",
            "a prompt the user wrote is never overwritten"
        );
        assert_eq!(
            fs::read_to_string(inline.join(PROMPT_CONFIG_FILE)).unwrap(),
            inline_blob,
            "an inline prompt is left byte-identical"
        );
        assert!(!inline.join(PROMPT_PROMPT_FILE).exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_directories_with_one_id_are_reported_with_both_locations() {
        let root = temp_dir("dup-id").join(PROMPTS_DIR_NAME);
        // A profile needs a prompt: it lives in prompt.md here so both
        // directories are otherwise valid profiles that collide on `id`.
        for (slug, prompt) in [("shared", "First."), ("shared-two", "Second.")] {
            let target = root.join(slug);
            fs::create_dir_all(&target).unwrap();
            fs::write(
                target.join(PROMPT_CONFIG_FILE),
                r#"{"id":"shared","label":"Shared"}"#,
            )
            .unwrap();
            fs::write(target.join(PROMPT_PROMPT_FILE), format!("{prompt}\n")).unwrap();
        }

        let mut issues = Vec::new();
        let profiles = load_prompts_from_dir(&root, PROMPTS_ORIGIN, &mut issues);
        assert_eq!(profiles.len(), 2);
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(profiles[0].id, profiles[1].id);
        assert!(issues[0].contains("shared"), "{issues:?}");
        assert!(issues[0].contains("shared-two"), "{issues:?}");

        let _ = fs::remove_dir_all(&root);
    }
}
