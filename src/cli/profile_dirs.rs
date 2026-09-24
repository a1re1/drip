// On-disk role profiles: one directory per profile, holding the role
// definition and its prompt as separate files.
//
//   ~/.drip/profiles/<name>/config.json   the role blob — every field the
//                                         config's `runtime.role_profiles`
//                                         entry used to carry except `prompt`
//   ~/.drip/profiles/<name>/prompt.md     the role prompt, as prose
//   <repo>/.drip/profiles/<name>/...      the same shape, repo-scoped
//
// The split exists because the prompt is the one field users actually edit,
// and a prompt embedded in a one-line JSON string is hostile to that. A
// profile directory is also the unit the legacy `runtime.role_profiles`
// setting migrates into: load_cli_config lifts every entry once (see
// migrate_legacy_role_profiles) and then clears the setting, so an existing
// config keeps working with no user action.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use indexmap::IndexMap;
use serde_json::Value;

use crate::cli::roles::{normalize_role_definition, RoleDefinition};
use crate::core::config::ROLE_PROFILES_SETTING_ID;

/// The profiles root under a drip home or a repo's `.drip`.
pub const PROFILES_DIR_NAME: &str = "profiles";
/// The role-definition file inside one profile directory.
pub const PROFILE_CONFIG_FILE: &str = "config.json";
/// The prompt file inside one profile directory.
pub const PROFILE_PROMPT_FILE: &str = "prompt.md";

/// Seeded next to the profiles root so the layout is discoverable; an existing
/// file is never overwritten.
pub const PROFILES_README: &str = "# Role profiles

One directory per role profile. Each directory holds:

- `config.json` — the role definition (name, model, tools, mcpServers, ...),
  the same blob `runtime.role_profiles` entries used to carry in config.json.
- `prompt.md` — this role's prompt, as plain prose. Edit it here.

The directory name is a sanitized form of the role's `name`; the name itself
lives in `config.json` (`name` is optional there and defaults to the directory
name). A repo can add its own profiles at `<repo>/.drip/profiles/<name>/` in
the same shape — those win over the user-level profile with the same name.

Precedence, lowest to highest: marketplace agents, `~/.drip/profiles/`,
`<repo>/.drip/roles.json`, `<repo>/.drip/profiles/`, `--roles`.

Profiles that were still listed in `runtime.role_profiles` inside
`~/.drip/config.json` are migrated into this layout automatically on the next
load; the setting is then cleared.
";

/// `Foo Bar` -> `foo-bar`. The directory name is a sanitized form of the
/// profile name; the authoritative name stays inside config.json.
pub fn profile_dir_name(name: &str) -> String {
    let mut out = String::new();
    let mut pending_dash = false;
    for ch in name.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            out.push(ch.to_ascii_lowercase());
            pending_dash = false;
        } else if ch == '_' || ch == '-' || ch == '.' {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            out.push(ch);
            pending_dash = false;
        } else {
            pending_dash = true;
        }
    }

    let trimmed = out.trim_matches(|c| c == '-' || c == '.').to_string();
    if trimmed.is_empty() {
        "profile".to_string()
    } else {
        trimmed
    }
}

/// Every profile directory directly under `root`, in stable (directory-name)
/// order so which of two same-named profiles wins never depends on readdir.
///
/// A missing root is the normal case for a user who never created one: not an
/// issue, just no profiles.
pub fn load_profiles_from_dir(
    root: &Path,
    origin: &str,
    issues: &mut Vec<String>,
) -> Vec<RoleDefinition> {
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

    let mut roles: Vec<RoleDefinition> = Vec::new();
    for dir in dirs {
        let slug = dir
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_string();
        let config_path = dir.join(PROFILE_CONFIG_FILE);
        let origin_label = format!("{origin}/{slug}/{PROFILE_CONFIG_FILE}");

        if !config_path.exists() {
            // A prompt-only directory is a half-written profile: say so rather
            // than silently dropping the prompt the user wrote.
            if dir.join(PROFILE_PROMPT_FILE).is_file() {
                issues.push(format!(
                    "{origin}/{slug}: missing {PROFILE_CONFIG_FILE} (the prompt has no role to attach to)"
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

        // The directory names the profile: `name` is optional in config.json.
        if value
            .get("name")
            .and_then(|name| name.as_str())
            .map(|name| name.trim().is_empty())
            .unwrap_or(true)
        {
            if let Some(object) = value.as_object_mut() {
                object.insert("name".to_string(), Value::from(slug.clone()));
            }
        }

        let Some(mut role) = normalize_role_definition(&value, &origin_label, issues) else {
            continue;
        };

        // prompt.md is the prompt: it wins over a `prompt` field left in
        // config.json, so the prose file is the one place to edit.
        if let Ok(prompt) = fs::read_to_string(dir.join(PROFILE_PROMPT_FILE)) {
            let prompt = prompt.trim_end();
            if !prompt.trim().is_empty() {
                role.prompt = Some(prompt.to_string());
            }
        }

        roles.push(role);
    }

    roles
}

/// Writes one profile directory: config.json (the role with its prompt
/// stripped) plus prompt.md when the role has a prompt.
pub fn write_profile_dir(dir: &Path, role: &RoleDefinition) -> Result<()> {
    let mut blob = role.clone();
    let prompt = blob.prompt.take();
    let body = serde_json::to_string_pretty(&blob)
        .with_context(|| format!("serialize profile \"{}\"", role.name))?;
    crate::lib_fs::write_file_atomic(&dir.join(PROFILE_CONFIG_FILE), &format!("{body}\n"), true)?;

    if let Some(prompt) = prompt
        .as_deref()
        .map(str::trim_end)
        .filter(|prompt| !prompt.trim().is_empty())
    {
        crate::lib_fs::write_file_atomic(
            &dir.join(PROFILE_PROMPT_FILE),
            &format!("{prompt}\n"),
            true,
        )?;
    }

    Ok(())
}

/// Creates `<home>/profiles/` and seeds a README inside it, so the layout is
/// discoverable. An existing README is left byte-identical.
pub fn ensure_profiles_dir(home_root: &Path) {
    let root = home_root.join(PROFILES_DIR_NAME);
    if fs::create_dir_all(&root).is_err() {
        return;
    }
    let readme = root.join("README.md");
    if !readme.exists() {
        let _ = crate::lib_fs::write_file_atomic(&readme, PROFILES_README, true);
    }
}

/// Lifts the legacy `runtime.role_profiles` setting into
/// `<config dir>/profiles/<name>/{config.json,prompt.md}` and clears the
/// setting once every profile is on disk.
///
/// Returns true only when every entry in the setting is accounted for on
/// disk: the profiles were written now, or a directory for them already
/// existed (a migration that already ran, or a hand-authored profile of the
/// same name). Anything that leaves an entry unaccounted for — a write
/// failure, or an entry the role normalizer could not lift — reports its issue
/// and leaves the setting alone, so the next load retries. Clearing the
/// setting while an entry was dropped would destroy that entry with a warning
/// as the only trace.
///
/// Liftable entries are still written on a partial failure, so fixing the
/// offending entry and reloading finishes the migration.
pub fn migrate_legacy_role_profiles(
    config_path: &Path,
    settings: &mut IndexMap<String, String>,
) -> bool {
    let raw = settings
        .get(ROLE_PROFILES_SETTING_ID)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .unwrap_or("[]")
        .to_string();
    if raw == "[]" {
        return false;
    }

    let parsed: Value = match serde_json::from_str(&raw) {
        Ok(parsed) => parsed,
        // A malformed value is reported by the role loader; never migrate it.
        Err(_) => return false,
    };
    let Some(entries) = parsed.as_array() else {
        return false;
    };
    if entries.is_empty() {
        return false;
    }

    let root = config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(PROFILES_DIR_NAME);

    // The normalizer's issues were previously discarded here, which let a
    // partially liftable setting drop its unusable entries and still clear the
    // setting below. Surface them instead, and let them keep the setting alive.
    let mut ignored_issues: Vec<String> = Vec::new();
    let mut roles: Vec<RoleDefinition> = Vec::new();
    for entry in entries {
        if let Some(role) =
            normalize_role_definition(entry, "config role_profiles", &mut ignored_issues)
        {
            roles.push(role);
        }
    }
    for issue in &ignored_issues {
        eprintln!(
            "warning: {}: {issue}; {ROLE_PROFILES_SETTING_ID} is kept until every entry migrates",
            config_path.display()
        );
    }
    if roles.is_empty() {
        // Nothing liftable (every entry was unusable): leave the setting for
        // the loader to report.
        return false;
    }

    let mut written: Vec<PathBuf> = Vec::new();
    let mut seen_names: Vec<String> = Vec::new();
    let mut settled = 0usize;
    for role in &roles {
        let name = role.name.trim();
        // Two entries with one name are one role to the loader (they merge by
        // name), so writing only the first directory would drop the second
        // entry's fields. Keep the setting instead and say so.
        if seen_names.iter().any(|seen| seen == name) {
            eprintln!(
                "warning: {}: {ROLE_PROFILES_SETTING_ID} defines \"{}\" more than once; \
                 merge the duplicate entries so migration can place this profile",
                config_path.display(),
                role.name
            );
            continue;
        }
        seen_names.push(name.to_string());

        let base = profile_dir_name(&role.name);
        match profile_dir_holds(&root.join(&base), &role.name) {
            // Already on disk under its own name — hand-authored, or an earlier
            // migration run. Never overwrite it.
            Some(true) => {
                settled += 1;
                continue;
            }
            // An existing directory whose config.json omits `name`: the loader
            // names it after the directory, so this may or may not be the same
            // profile. Never overwrite it, but never call the legacy entry
            // migrated either — clearing the setting here would discard it
            // with no trace.
            None if root.join(&base).join(PROFILE_CONFIG_FILE).exists() => {
                eprintln!(
                    "warning: {}: {} has no \"name\" and {} takes its name from the \
                     directory; add a \"name\" (or remove the directory) so migration can place \
                     \"{}\". {ROLE_PROFILES_SETTING_ID} is kept until every entry migrates",
                    config_path.display(),
                    root.join(&base).join(PROFILE_CONFIG_FILE).display(),
                    PROFILE_PROMPT_FILE,
                    role.name
                );
                continue;
            }
            _ => {}
        }

        match free_profile_dir(&root, &base, &written) {
            Some(dir) => match write_profile_dir(&dir, role) {
                Ok(()) => {
                    written.push(dir);
                    settled += 1;
                }
                Err(error) => {
                    eprintln!(
                        "warning: {}: could not migrate role profile \"{}\" to {}: {error}",
                        config_path.display(),
                        role.name,
                        dir.display()
                    );
                }
            },
            None => {
                eprintln!(
                    "warning: {}: could not find a free profile directory for \"{}\" under {}",
                    config_path.display(),
                    role.name,
                    root.display()
                );
            }
        }
    }

    // `roles.len() < entries.len()` means the normalizer skipped an entry: the
    // setting must survive so that entry is neither lost nor silently ignored.
    let migrated = settled == roles.len() && roles.len() == entries.len();
    if migrated {
        settings.insert(ROLE_PROFILES_SETTING_ID.to_string(), "[]".to_string());
    }
    migrated
}

/// Whether `dir` is already the profile directory of `name`.
///
/// `Some(true)` — its config.json names this profile. `Some(false)` — it names
/// a different profile, or its config.json is unreadable/malformed (the loader
/// reports that one; migration must not treat it as migrated). `None` — no
/// config.json at all, or one that omits `name`, so the loader would call it
/// whatever the directory is called: callers must not assume it is this
/// profile.
fn profile_dir_holds(dir: &Path, name: &str) -> Option<bool> {
    let Ok(raw) = fs::read_to_string(dir.join(PROFILE_CONFIG_FILE)) else {
        return None;
    };
    let Ok(value) = serde_json::from_str::<Value>(&raw) else {
        return Some(false);
    };
    let existing = value.get("name").and_then(|value| value.as_str())?;
    if existing.trim().is_empty() {
        return None;
    }
    Some(existing.trim() == name.trim())
}

/// The first free `<root>/<base>[-N]` directory name, ignoring names already
/// claimed by this migration run.
fn free_profile_dir(root: &Path, base: &str, taken: &[PathBuf]) -> Option<PathBuf> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "drip-profiles-{label}-{}-{}",
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
    fn profile_dir_name_sanitizes_but_keeps_slug_shaped_names() {
        assert_eq!(profile_dir_name("author"), "author");
        assert_eq!(profile_dir_name("Reviewer One"), "reviewer-one");
        assert_eq!(
            profile_dir_name(
                "  Deep
Seek  "
            ),
            "deep-seek"
        );
        assert_eq!(profile_dir_name("a/b"), "a-b");
        // A name that would escape the profiles root falls back to a literal.
        assert_eq!(profile_dir_name(".."), "profile");
        assert_eq!(profile_dir_name("..."), "profile");
        assert_eq!(profile_dir_name(""), "profile");
        assert_eq!(profile_dir_name("planner-2"), "planner-2");
    }

    #[test]
    fn migration_writes_config_and_prompt_files_then_clears_the_setting() {
        let dir = temp_dir("migrate");
        let config_path = dir.join("config.json");
        fs::write(&config_path, "{}").unwrap();

        let mut settings: IndexMap<String, String> = IndexMap::new();
        settings.insert(
            ROLE_PROFILES_SETTING_ID.to_string(),
            r#"[{"name":"author","model":"fast","tools":["READ"],"prompt":"You are the author."},{"name":"Reviewer One","prompt":"Review hard."}]"#
                .to_string(),
        );

        assert!(migrate_legacy_role_profiles(&config_path, &mut settings));
        assert_eq!(
            settings.get(ROLE_PROFILES_SETTING_ID).map(String::as_str),
            Some("[]"),
            "the legacy setting is cleared once every profile is on disk"
        );

        let author = dir.join("profiles").join("author");
        let blob: Value =
            serde_json::from_str(&fs::read_to_string(author.join("config.json")).unwrap()).unwrap();
        assert_eq!(blob["name"], "author");
        assert_eq!(blob["model"], "fast");
        assert_eq!(blob["tools"][0], "READ");
        // The prompt lives only in prompt.md.
        assert!(blob.get("prompt").is_none(), "{blob}");
        assert_eq!(
            fs::read_to_string(author.join("prompt.md")).unwrap(),
            "You are the author.\n"
        );

        // A name with a space still lands in its own directory.
        let reviewer = dir.join("profiles").join("reviewer-one");
        assert!(reviewer.join("config.json").is_file());
        assert_eq!(
            fs::read_to_string(reviewer.join("prompt.md")).unwrap(),
            "Review hard.\n"
        );

        // Idempotent: a second run touches nothing and re-reports settled.
        let mut again: IndexMap<String, String> = IndexMap::new();
        again.insert(
            ROLE_PROFILES_SETTING_ID.to_string(),
            r#"[{"name":"author","model":"changed-by-hand"}]"#.to_string(),
        );
        assert!(migrate_legacy_role_profiles(&config_path, &mut again));
        let blob: Value =
            serde_json::from_str(&fs::read_to_string(author.join("config.json")).unwrap()).unwrap();
        assert_eq!(
            blob["model"], "fast",
            "an existing profile directory is never overwritten"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_partially_liftable_setting_keeps_the_entries_it_could_not_lift() {
        let dir = temp_dir("partial");
        let config_path = dir.join("config.json");
        fs::write(&config_path, "{}").unwrap();

        // `{"id": ...}` is the pre-change README's own example shape: no
        // `name`, so the normalizer skips it. The other entry lifts fine.
        let raw = r#"[{"id":"planner","prompt":"PLAN PROMPT"},{"name":"Reviewer One","prompt":"Review hard."}]"#;
        let mut settings: IndexMap<String, String> = IndexMap::new();
        settings.insert(ROLE_PROFILES_SETTING_ID.to_string(), raw.to_string());

        assert!(
            !migrate_legacy_role_profiles(&config_path, &mut settings),
            "an entry that could not be lifted keeps the setting alive"
        );
        assert_eq!(
            settings.get(ROLE_PROFILES_SETTING_ID).map(String::as_str),
            Some(raw),
            "the un-liftable entry survives for the loader to report and the user to fix"
        );

        // The liftable entry still landed, so a fixed setting migrates cleanly
        // and the retry does not rewrite it.
        let reviewer = dir.join("profiles").join("reviewer-one");
        assert!(reviewer.join("config.json").is_file());
        assert_eq!(
            fs::read_to_string(reviewer.join("prompt.md")).unwrap(),
            "Review hard.\n"
        );
        let mut fixed: IndexMap<String, String> = IndexMap::new();
        fixed.insert(
            ROLE_PROFILES_SETTING_ID.to_string(),
            r#"[{"name":"planner","prompt":"PLAN PROMPT"},{"name":"Reviewer One","prompt":"Review hard."}]"#.to_string(),
        );
        assert!(migrate_legacy_role_profiles(&config_path, &mut fixed));
        assert_eq!(
            fs::read_to_string(reviewer.join("prompt.md")).unwrap(),
            "Review hard.\n",
            "the already-written profile is not rewritten by the retry"
        );
        assert!(dir.join("profiles/planner/prompt.md").is_file());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_nameless_profile_directory_keeps_the_legacy_entry_unsettled() {
        let dir = temp_dir("nameless-collision");
        let config_path = dir.join("config.json");
        fs::write(&config_path, "{}").unwrap();

        // A hand-authored profile whose slug matches the legacy entry's, with
        // no `name` — the loader calls it "reviewer-one" after the directory.
        let hand = dir.join("profiles").join("reviewer-one");
        fs::create_dir_all(&hand).unwrap();
        fs::write(hand.join("config.json"), r#"{"model":"hand-written"}"#).unwrap();

        let raw = r#"[{"name":"Reviewer One","prompt":"Review hard."}]"#;
        let mut settings: IndexMap<String, String> = IndexMap::new();
        settings.insert(ROLE_PROFILES_SETTING_ID.to_string(), raw.to_string());

        assert!(
            !migrate_legacy_role_profiles(&config_path, &mut settings),
            "a directory that only *might* be this profile must not settle the entry"
        );
        assert_eq!(
            settings.get(ROLE_PROFILES_SETTING_ID).map(String::as_str),
            Some(raw),
            "the legacy entry is never discarded into an ambiguous directory"
        );
        // The hand-authored profile is untouched — no -2 duplicate either.
        assert_eq!(
            fs::read_to_string(hand.join("config.json")).unwrap(),
            r#"{"model":"hand-written"}"#
        );
        assert!(!dir.join("profiles").join("reviewer-one-2").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_repeated_role_name_keeps_the_setting_until_it_is_deduplicated() {
        let dir = temp_dir("duplicate-name");
        let config_path = dir.join("config.json");
        fs::write(&config_path, "{}").unwrap();

        let raw = r#"[{"name":"author","model":"fast"},{"name":"author","tools":["READ"]}]"#;
        let mut settings: IndexMap<String, String> = IndexMap::new();
        settings.insert(ROLE_PROFILES_SETTING_ID.to_string(), raw.to_string());

        assert!(!migrate_legacy_role_profiles(&config_path, &mut settings));
        assert_eq!(
            settings.get(ROLE_PROFILES_SETTING_ID).map(String::as_str),
            Some(raw),
            "writing only the first of two same-named entries would drop the second's fields"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn migration_leaves_a_malformed_setting_alone() {
        let dir = temp_dir("malformed");
        let config_path = dir.join("config.json");
        fs::write(&config_path, "{}").unwrap();

        let mut settings: IndexMap<String, String> = IndexMap::new();
        settings.insert(ROLE_PROFILES_SETTING_ID.to_string(), "not json".to_string());
        assert!(!migrate_legacy_role_profiles(&config_path, &mut settings));
        assert_eq!(
            settings.get(ROLE_PROFILES_SETTING_ID).map(String::as_str),
            Some("not json")
        );
        assert!(!dir.join("profiles").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_reads_prompt_md_and_defaults_the_name_to_the_directory() {
        let dir = temp_dir("load");
        let root = dir.join("profiles");
        let author = root.join("author");
        fs::create_dir_all(&author).unwrap();
        fs::write(author.join("config.json"), r#"{"model":"fast"}"#).unwrap();
        fs::write(author.join("prompt.md"), "From the prompt file.\n").unwrap();
        // config.json's own prompt is ignored when prompt.md exists.
        let legacy = root.join("legacy");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(
            legacy.join("config.json"),
            r#"{"name":"legacy","prompt":"inline prompt"}"#,
        )
        .unwrap();

        let mut issues: Vec<String> = Vec::new();
        let roles = load_profiles_from_dir(&root, "profiles", &mut issues);
        assert!(issues.is_empty(), "{issues:?}");
        assert_eq!(roles.len(), 2, "{roles:?}");

        let author_role = roles.iter().find(|role| role.name == "author").unwrap();
        assert_eq!(author_role.model.as_deref(), Some("fast"));
        assert_eq!(author_role.prompt.as_deref(), Some("From the prompt file."));

        let legacy_role = roles.iter().find(|role| role.name == "legacy").unwrap();
        assert_eq!(legacy_role.prompt.as_deref(), Some("inline prompt"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_prompt_only_directory_reports_an_issue() {
        let dir = temp_dir("prompt-only");
        let root = dir.join("profiles");
        fs::create_dir_all(root.join("orphan")).unwrap();
        fs::write(root.join("orphan").join("prompt.md"), "orphan\n").unwrap();

        let mut issues: Vec<String> = Vec::new();
        let roles = load_profiles_from_dir(&root, "profiles", &mut issues);
        assert!(roles.is_empty());
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert!(issues[0].contains("missing config.json"), "{issues:?}");
        let _ = fs::remove_dir_all(&dir);
    }
}
