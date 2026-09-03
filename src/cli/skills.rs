use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Built-in skill pack — embedded at compile time
// ---------------------------------------------------------------------------

const BUILTIN_COMMIT_DISCIPLINE: &str =
    include_str!("../../skills/commit-discipline/SKILL.md");
const BUILTIN_DEBUG_ROOT_CAUSE: &str =
    include_str!("../../skills/debug-root-cause/SKILL.md");
const BUILTIN_MIGRATION_DISCIPLINE: &str =
    include_str!("../../skills/migration-discipline/SKILL.md");
const BUILTIN_REFACTOR_SAFELY: &str =
    include_str!("../../skills/refactor-safely/SKILL.md");
const BUILTIN_REVIEW_INDEPENDENTLY: &str =
    include_str!("../../skills/review-independently/SKILL.md");
const BUILTIN_TDD: &str = include_str!("../../skills/tdd/SKILL.md");
const BUILTIN_VERIFY_BEFORE_DONE: &str =
    include_str!("../../skills/verify-before-done/SKILL.md");

/// Returns the built-in (name, content) pairs in filesystem-sort order.
fn builtin_skill_entries() -> Vec<(&'static str, &'static str)> {
    vec![
        ("commit-discipline", BUILTIN_COMMIT_DISCIPLINE),
        ("debug-root-cause", BUILTIN_DEBUG_ROOT_CAUSE),
        ("migration-discipline", BUILTIN_MIGRATION_DISCIPLINE),
        ("refactor-safely", BUILTIN_REFACTOR_SAFELY),
        ("review-independently", BUILTIN_REVIEW_INDEPENDENTLY),
        ("tdd", BUILTIN_TDD),
        ("verify-before-done", BUILTIN_VERIFY_BEFORE_DONE),
    ]
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CliSkill {
    pub description: String,
    /// Set for marketplace skills: "<marketplace>/<plugin>/<skill>", the key used for enable/disable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub name: String,
    pub path: String,
    pub source: SkillSource,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SkillSource {
    #[serde(rename = "builtin")]
    Builtin,
    #[serde(rename = "marketplace")]
    Marketplace,
    #[serde(rename = "project")]
    Project,
    #[serde(rename = "user")]
    User,
}

/// The provider-agnostic skill-file shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillFileEntry {
    pub description: String,
    pub name: String,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedCliSkill {
    pub content: String,
    pub name: String,
}

/// A declared parameter: required when default is None, optional otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillArgDef {
    pub default: Option<String>,
    pub name: String,
}

/// The parsed args block from a skill's frontmatter.
pub type SkillArgs = Vec<SkillArgDef>;

#[derive(Debug, Default)]
struct SkillFrontmatter {
    args: Option<SkillArgs>,
    description: Option<String>,
    name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CollectSkillFilesResult {
    pub issues: Vec<String>,
    pub skills: Vec<SkillFileEntry>,
}

#[derive(Debug, Clone)]
pub struct CollectSkillsResult {
    pub issues: Vec<String>,
    pub skills: Vec<CliSkill>,
}

#[derive(Debug, Clone)]
pub struct DiscoverSkillsResult {
    pub issues: Vec<String>,
    pub skills: Vec<CliSkill>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillActivationEntry {
    pub args: HashMap<String, String>,
    pub name: String,
}

/// What a run pins on the session: its skill set (with args) and, when the
/// caller chose one explicitly, the model profile — so `--resume` continues
/// with the same discipline AND the same model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRunConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    pub skills: Vec<SkillActivationEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveActivation {
    pub entries: Vec<SkillActivationEntry>,
    /// Stored explicit profile to reuse; unset when the caller chose one or none was stored.
    pub profile: Option<String>,
    /// Set when stored skills were re-activated — the caller should say so on stderr.
    pub reactivated: bool,
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Normalize raw file content: strip UTF-8 BOM and convert CRLF to LF.
fn normalize_content(raw: &str) -> String {
    let stripped = raw.strip_prefix('\u{FEFF}').unwrap_or(raw);
    stripped.replace("\r\n", "\n")
}

fn parse_skill_frontmatter(markdown: &str) -> SkillFrontmatter {
    // Match ---\n...\n---
    let body = if let Some(rest) = markdown.strip_prefix("---\n") {
        if let Some(idx) = rest.find("\n---") {
            &rest[..idx]
        } else {
            return SkillFrontmatter::default();
        }
    } else {
        return SkillFrontmatter::default();
    };

    let mut fields = SkillFrontmatter::default();
    let mut in_args_block = false;
    let mut args: SkillArgs = Vec::new();

    for line in body.split('\n') {
        // Detect the start of the args block
        if line.trim_end() == "args:" {
            in_args_block = true;
            continue;
        }

        // If we're in the args block, parse indented lines
        if in_args_block {
            // Match "  name: value" or "  name:"
            if let Some(rest) = line.strip_prefix("  ") {
                if let Some(colon_idx) = rest.find(':') {
                    let arg_name = &rest[..colon_idx];
                    // Validate arg name: [A-Za-z_][A-Za-z0-9_]*
                    if !arg_name.is_empty()
                        && arg_name
                            .chars()
                            .next()
                            .map(|c| c.is_ascii_alphabetic() || c == '_')
                            .unwrap_or(false)
                        && arg_name
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '_')
                    {
                        let arg_value = rest[colon_idx + 1..].trim();
                        if arg_value.is_empty() {
                            args.push(SkillArgDef {
                                default: None,
                                name: arg_name.to_string(),
                            });
                        } else {
                            args.push(SkillArgDef {
                                default: Some(arg_value.to_string()),
                                name: arg_name.to_string(),
                            });
                        }
                        continue;
                    }
                }
            }
            // A non-indented line ends the args block
            in_args_block = false;
        }

        // Parse name: or description: fields
        if let Some(rest) = line.strip_prefix("name:") {
            let v = rest.trim();
            if !v.is_empty() {
                fields.name = Some(v.to_string());
            }
        } else if let Some(rest) = line.strip_prefix("description:") {
            let v = rest.trim();
            if !v.is_empty() {
                fields.description = Some(v.to_string());
            }
        }
    }

    if !args.is_empty() {
        fields.args = Some(args);
    }

    fields
}

fn first_non_empty_line(markdown: &str) -> String {
    // Strip frontmatter
    let body = if markdown.starts_with("---\n") {
        if let Some(idx) = markdown[4..].find("\n---") {
            &markdown[4 + idx + 4..]
        } else {
            markdown
        }
    } else {
        markdown
    };

    for line in body.split('\n') {
        // Strip leading # characters and whitespace
        let trimmed = line.trim_start_matches('#').trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }

    String::new()
}

// ---------------------------------------------------------------------------
// collectSkillFiles
// ---------------------------------------------------------------------------

pub fn collect_skill_files(skills_dir: &Path) -> Vec<SkillFileEntry> {
    collect_skill_files_inner(skills_dir, false).0
}

pub fn collect_skill_files_with_issues(skills_dir: &Path) -> CollectSkillFilesResult {
    let (skills, issues) = collect_skill_files_inner(skills_dir, true);
    CollectSkillFilesResult { issues, skills }
}

fn collect_skill_files_inner(skills_dir: &Path, with_issues: bool) -> (Vec<SkillFileEntry>, Vec<String>) {
    if !skills_dir.exists() {
        return (vec![], vec![]);
    }

    let mut skills: Vec<SkillFileEntry> = Vec::new();
    let mut issues: Vec<String> = Vec::new();

    // Read directory entries sorted
    let mut entries = match std::fs::read_dir(skills_dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .collect::<Vec<_>>(),
        Err(err) => {
            if with_issues {
                issues.push(format!(
                    "skill directory \"{}\" could not be read: {}",
                    skills_dir.display(),
                    err
                ));
            }
            return (skills, issues);
        }
    };
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let entry_path = entry.path();
        let entry_name = entry.file_name().to_string_lossy().into_owned();

        let mut skill_path: Option<PathBuf> = None;
        let mut default_name = entry_name.clone();

        match std::fs::metadata(&entry_path) {
            Ok(meta) => {
                if meta.is_dir() {
                    let candidate = entry_path.join("SKILL.md");
                    if candidate.exists() {
                        skill_path = Some(candidate);
                    } else {
                        if with_issues {
                            issues.push(format!(
                                "skill directory \"{}\" is missing SKILL.md",
                                entry_path.display()
                            ));
                        }
                        continue;
                    }
                } else if entry_name.to_lowercase().ends_with(".md") {
                    skill_path = Some(entry_path.clone());
                    default_name = entry_name
                        .strip_suffix(".md")
                        .or_else(|| entry_name.strip_suffix(".MD"))
                        .unwrap_or(&entry_name)
                        .to_string();
                }
            }
            Err(err) => {
                if with_issues {
                    issues.push(format!(
                        "skill entry \"{}\" could not be read: {}",
                        entry_path.display(),
                        err
                    ));
                }
                continue;
            }
        }

        let skill_path = match skill_path {
            Some(p) => p,
            None => continue,
        };

        match std::fs::read_to_string(&skill_path) {
            Ok(raw) => {
                let markdown = normalize_content(&raw);
                let frontmatter = parse_skill_frontmatter(&markdown);
                skills.push(SkillFileEntry {
                    description: frontmatter
                        .description
                        .unwrap_or_else(|| first_non_empty_line(&markdown)),
                    name: frontmatter.name.unwrap_or(default_name),
                    path: skill_path.to_string_lossy().into_owned(),
                });
            }
            Err(err) => {
                if with_issues {
                    issues.push(format!(
                        "skill file \"{}\" could not be parsed: {}",
                        skill_path.display(),
                        err
                    ));
                }
                continue;
            }
        }
    }

    (skills, issues)
}

// ---------------------------------------------------------------------------
// collectSkillsFromDir
// ---------------------------------------------------------------------------

fn collect_skills_from_dir(skills_dir: &Path, source: SkillSource) -> Vec<CliSkill> {
    collect_skill_files(skills_dir)
        .into_iter()
        .map(|entry| CliSkill {
            description: entry.description,
            key: None,
            name: entry.name,
            path: entry.path,
            source: source.clone(),
        })
        .collect()
}

fn collect_skills_from_dir_with_issues(skills_dir: &Path, source: SkillSource) -> CollectSkillsResult {
    let result = collect_skill_files_with_issues(skills_dir);
    CollectSkillsResult {
        issues: result.issues,
        skills: result
            .skills
            .into_iter()
            .map(|entry| CliSkill {
                description: entry.description,
                key: None,
                name: entry.name,
                path: entry.path,
                source: source.clone(),
            })
            .collect(),
    }
}

// ---------------------------------------------------------------------------
// Built-in skills (from embedded string constants)
// ---------------------------------------------------------------------------

/// Collect the built-in skill pack from embedded constants.
/// The `builtin_dir` parameter is used as the path prefix for each skill's path field.
pub fn collect_builtin_skills(builtin_dir: Option<&Path>) -> Vec<CliSkill> {
    let base = builtin_dir
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "<builtin>".to_string());

    builtin_skill_entries()
        .into_iter()
        .map(|(name, content)| {
            let markdown = normalize_content(content);
            let frontmatter = parse_skill_frontmatter(&markdown);
            CliSkill {
                description: frontmatter
                    .description
                    .unwrap_or_else(|| first_non_empty_line(&markdown)),
                key: None,
                name: frontmatter.name.unwrap_or_else(|| name.to_string()),
                path: format!("{}/{}/SKILL.md", base, name),
                source: SkillSource::Builtin,
            }
        })
        .collect()
}

fn collect_builtin_skills_from_dir_with_issues(skills_dir: Option<&Path>) -> CollectSkillsResult {
    if let Some(dir) = skills_dir {
        if dir.exists() {
            return collect_skills_from_dir_with_issues(dir, SkillSource::Builtin);
        }
    }
    // Fall back to embedded constants
    CollectSkillsResult {
        issues: vec![],
        skills: collect_builtin_skills(skills_dir),
    }
}

fn collect_builtin_skills_from_dir(skills_dir: Option<&Path>) -> Vec<CliSkill> {
    if let Some(dir) = skills_dir {
        if dir.exists() {
            return collect_skills_from_dir(dir, SkillSource::Builtin);
        }
    }
    collect_builtin_skills(skills_dir)
}

// ---------------------------------------------------------------------------
// discoverSkills / discoverSkillsWithIssues
// ---------------------------------------------------------------------------

/// Project skills (<cwd>/.drip/skills) shadow user skills (~/.drip/skills), which
/// shadow enabled marketplace skills, which shadow the built-in pack — all by
/// skill name.
pub fn discover_skills(
    cwd: &Path,
    home_skills_dir: &Path,
    marketplace_skills: Option<Vec<CliSkill>>,
    builtin_dir: Option<&Path>,
) -> Vec<CliSkill> {
    let project_skills =
        collect_skills_from_dir(&cwd.join(".drip").join("skills"), SkillSource::Project);
    let mut local_names: HashSet<String> =
        project_skills.iter().map(|s| s.name.clone()).collect();

    let user_skills = collect_skills_from_dir(home_skills_dir, SkillSource::User)
        .into_iter()
        .filter(|s| !local_names.contains(&s.name))
        .collect::<Vec<_>>();
    for s in &user_skills {
        local_names.insert(s.name.clone());
    }

    let marketplace_skills = marketplace_skills
        .unwrap_or_default()
        .into_iter()
        .filter(|s| !local_names.contains(&s.name))
        .collect::<Vec<_>>();
    for s in &marketplace_skills {
        local_names.insert(s.name.clone());
    }

    let builtin_skills = collect_builtin_skills_from_dir(builtin_dir)
        .into_iter()
        .filter(|s| !local_names.contains(&s.name))
        .collect::<Vec<_>>();

    let mut result = project_skills;
    result.extend(user_skills);
    result.extend(marketplace_skills);
    result.extend(builtin_skills);
    result
}

pub fn discover_skills_with_issues(
    cwd: &Path,
    home_skills_dir: &Path,
    marketplace_skills: Option<Vec<CliSkill>>,
    builtin_dir: Option<&Path>,
) -> DiscoverSkillsResult {
    let mut all_issues: Vec<String> = Vec::new();

    let project_result = collect_skills_from_dir_with_issues(
        &cwd.join(".drip").join("skills"),
        SkillSource::Project,
    );
    all_issues.extend(project_result.issues);

    let mut local_names: HashSet<String> =
        project_result.skills.iter().map(|s| s.name.clone()).collect();

    let user_result =
        collect_skills_from_dir_with_issues(home_skills_dir, SkillSource::User);
    all_issues.extend(user_result.issues);
    let user_skills = user_result
        .skills
        .into_iter()
        .filter(|s| !local_names.contains(&s.name))
        .collect::<Vec<_>>();
    for s in &user_skills {
        local_names.insert(s.name.clone());
    }

    let marketplace_skills = marketplace_skills
        .unwrap_or_default()
        .into_iter()
        .filter(|s| !local_names.contains(&s.name))
        .collect::<Vec<_>>();
    for s in &marketplace_skills {
        local_names.insert(s.name.clone());
    }

    let builtin_result = collect_builtin_skills_from_dir_with_issues(builtin_dir);
    all_issues.extend(builtin_result.issues);
    let builtin_skills = builtin_result
        .skills
        .into_iter()
        .filter(|s| !local_names.contains(&s.name))
        .collect::<Vec<_>>();

    let mut skills = project_result.skills;
    skills.extend(user_skills);
    skills.extend(marketplace_skills);
    skills.extend(builtin_skills);

    DiscoverSkillsResult {
        issues: all_issues,
        skills,
    }
}

// ---------------------------------------------------------------------------
// loadSkillContent — parameterized substitution
// ---------------------------------------------------------------------------

pub fn load_skill_content(
    skill: &CliSkill,
    args: Option<&HashMap<String, String>>,
) -> Result<LoadedCliSkill, String> {
    // Read from disk (or from embedded if path is <builtin>)
    let raw = if skill.path.starts_with("<builtin>") {
        // Find in embedded constants
        let name = &skill.name;
        builtin_skill_entries()
            .into_iter()
            .find(|(n, _)| *n == name.as_str())
            .map(|(_, c)| c.to_string())
            .ok_or_else(|| format!("Skill \"{}\": built-in content not found", skill.name))?
    } else {
        std::fs::read_to_string(&skill.path)
            .map_err(|e| format!("Skill \"{}\": could not read file: {}", skill.name, e))?
    };

    let markdown = normalize_content(&raw);
    let frontmatter = parse_skill_frontmatter(&markdown);

    // No args block — return content as-is (trimmed)
    let Some(arg_defs) = frontmatter.args else {
        return Ok(LoadedCliSkill {
            content: markdown.trim().to_string(),
            name: skill.name.clone(),
        });
    };

    // Validate supplied args against declared args.
    let declared: HashMap<&str, Option<&str>> = arg_defs
        .iter()
        .map(|d| (d.name.as_str(), d.default.as_deref()))
        .collect();

    if let Some(supplied) = args {
        for key in supplied.keys() {
            if !declared.contains_key(key.as_str()) {
                return Err(format!(
                    "Skill \"{}\": unknown arg \"{}\"",
                    skill.name, key
                ));
            }
        }
    }

    // Build the resolved values map and check for missing required args.
    let mut resolved: HashMap<String, String> = HashMap::new();
    for def in &arg_defs {
        if let Some(supplied) = args.and_then(|a| a.get(&def.name)) {
            resolved.insert(def.name.clone(), supplied.clone());
        } else if let Some(default) = &def.default {
            resolved.insert(def.name.clone(), default.clone());
        } else {
            return Err(format!(
                "Skill \"{}\": missing required arg \"{}\"",
                skill.name, def.name
            ));
        }
    }

    // Substitute {{name}} placeholders in the content.
    let trimmed = markdown.trim();
    let mut content = String::with_capacity(trimmed.len());
    let bytes = trimmed.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            // Look for closing }}
            if let Some(end_offset) = trimmed[i + 2..].find("}}") {
                let key = &trimmed[i + 2..i + 2 + end_offset];
                // Validate placeholder name: [A-Za-z_][A-Za-z0-9_]*
                let valid = !key.is_empty()
                    && key
                        .chars()
                        .next()
                        .map(|c| c.is_ascii_alphabetic() || c == '_')
                        .unwrap_or(false)
                    && key
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_');
                if valid {
                    if let Some(val) = resolved.get(key) {
                        content.push_str(val);
                    } else {
                        // Key not in resolved — leave as-is
                        content.push_str(&format!("{{{{{}}}}}", key));
                    }
                    i += 2 + end_offset + 2;
                    continue;
                }
            }
        }
        content.push(bytes[i] as char);
        i += 1;
    }

    Ok(LoadedCliSkill {
        content,
        name: skill.name.clone(),
    })
}

// ---------------------------------------------------------------------------
// composeSkillSystemPrompt
// ---------------------------------------------------------------------------

pub fn compose_skill_system_prompt(base_prompt: &str, skills: &[LoadedCliSkill]) -> String {
    if skills.is_empty() {
        return base_prompt.to_string();
    }

    let skill_sections: Vec<String> = skills
        .iter()
        .map(|s| format!("# Skill: {}\n\n{}", s.name, s.content))
        .collect();

    std::iter::once(base_prompt)
        .chain(skill_sections.iter().map(|s| s.as_str()))
        .collect::<Vec<_>>()
        .join("\n\n")
}

// ---------------------------------------------------------------------------
// Session skill continuity
// ---------------------------------------------------------------------------

pub fn save_skill_activation(path: &Path, config: &SessionRunConfig) -> std::io::Result<()> {
    let json = serde_json::to_string_pretty(config)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    // Atomic write: write to tmp then rename
    let dir = path.parent().unwrap_or(Path::new("."));
    let tmp = dir.join(format!(
        ".tmp-skills-{}.json",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos()
    ));
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn load_skill_activation(path: &Path) -> Option<SessionRunConfig> {
    if !path.exists() {
        return None;
    }

    let text = std::fs::read_to_string(path).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&text).ok()?;

    let skills_val = parsed.get("skills")?;
    let skills_arr = skills_val.as_array()?;

    let mut skills: Vec<SkillActivationEntry> = Vec::new();
    for entry in skills_arr {
        if let Some(s) = entry.as_str() {
            // v0.34 stored bare names; entries since carry args.
            skills.push(SkillActivationEntry {
                args: HashMap::new(),
                name: s.to_string(),
            });
        } else if let Some(obj) = entry.as_object() {
            let name = obj.get("name")?.as_str()?.to_string();
            let args = if let Some(args_val) = obj.get("args") {
                if let Some(args_obj) = args_val.as_object() {
                    args_obj
                        .iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect()
                } else {
                    HashMap::new()
                }
            } else {
                HashMap::new()
            };
            skills.push(SkillActivationEntry { args, name });
        } else {
            return None;
        }
    }

    let profile = parsed.get("profile").and_then(|v| v.as_str()).map(|s| s.to_string());

    Some(SessionRunConfig { profile, skills })
}

// ---------------------------------------------------------------------------
// parseSkillFlagValue
// ---------------------------------------------------------------------------

/// Parse a raw --skill flag value into a name and args map.
///
/// Grammar:
/// - "tdd"                         → { name: "tdd", args: {} }
/// - "migrate:from=jest"           → { name: "migrate", args: { from: "jest" } }
/// - "migrate:from=jest,to=vitest" → { name: "migrate", args: { from: "jest", to: "vitest" } }
/// - "x:key=a=b"                   → { name: "x", args: { key: "a=b" } }
/// - Values may contain "=" after the first (only the first "=" splits key from value)
/// - Commas separate key=value pairs
pub fn parse_skill_flag_value(raw: &str) -> (String, HashMap<String, String>) {
    let colon_idx = raw.find(':');

    if colon_idx.is_none() {
        return (raw.to_string(), HashMap::new());
    }

    let colon_idx = colon_idx.unwrap();
    let name = raw[..colon_idx].to_string();
    let args_part = &raw[colon_idx + 1..];
    let mut args: HashMap<String, String> = HashMap::new();

    if !args_part.is_empty() {
        for pair in args_part.split(',') {
            if let Some(eq_idx) = pair.find('=') {
                let key = pair[..eq_idx].to_string();
                let value = pair[eq_idx + 1..].to_string();
                args.insert(key, value);
            } else {
                // bare key with no value — treat as empty string
                args.insert(pair.to_string(), String::new());
            }
        }
    }

    (name, args)
}

// ---------------------------------------------------------------------------
// resolveEffectiveActivation
// ---------------------------------------------------------------------------

pub struct ResolveEffectiveActivationArgs<'a> {
    /// Raw --skill flag values ("tdd" or "migration:from=jest,to=vitest").
    pub explicit: &'a [String],
    pub explicit_profile: Option<&'a str>,
    /// --no-skills: run bare and clear the stored activation.
    pub no_skills: bool,
    /// Resume-like invocations (--resume/--continue) inherit; fresh sessions never do.
    pub resume_like: bool,
    pub stored: Option<&'a SessionRunConfig>,
}

pub fn resolve_effective_activation(args: ResolveEffectiveActivationArgs<'_>) -> EffectiveActivation {
    let stored_profile = if args.explicit_profile.is_some() {
        None
    } else if args.resume_like {
        args.stored.and_then(|s| s.profile.clone())
    } else {
        None
    };

    if args.no_skills {
        return EffectiveActivation {
            entries: vec![],
            profile: stored_profile,
            reactivated: false,
        };
    }

    if !args.explicit.is_empty() {
        let entries = args
            .explicit
            .iter()
            .map(|raw| {
                let (name, skill_args) = parse_skill_flag_value(raw);
                SkillActivationEntry {
                    args: skill_args,
                    name,
                }
            })
            .collect();
        return EffectiveActivation {
            entries,
            profile: stored_profile,
            reactivated: false,
        };
    }

    if args.resume_like {
        if let Some(stored) = args.stored {
            if !stored.skills.is_empty() {
                return EffectiveActivation {
                    entries: stored.skills.clone(),
                    profile: stored_profile,
                    reactivated: true,
                };
            }
        }
    }

    EffectiveActivation {
        entries: vec![],
        profile: stored_profile,
        reactivated: false,
    }
}

// ---------------------------------------------------------------------------
// --skills listing helpers
// ---------------------------------------------------------------------------

pub fn format_skills_human(skills: &[CliSkill]) -> String {
    if skills.is_empty() {
        return "No skills available.\n".to_string();
    }

    let mut lines = Vec::new();
    for skill in skills {
        let source_label = match skill.source {
            SkillSource::Builtin => "builtin",
            SkillSource::Marketplace => "marketplace",
            SkillSource::Project => "project",
            SkillSource::User => "user",
        };
        lines.push(format!(
            "{} [{}] — {}",
            skill.name, source_label, skill.description
        ));
    }
    lines.join("\n") + "\n"
}

pub fn format_skills_json(skills: &[CliSkill]) -> String {
    serde_json::to_string_pretty(skills).unwrap_or_else(|_| "[]".to_string())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_temp_dir() -> TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn write_file(dir: &Path, name: &str, content: &str) {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, content).unwrap();
    }

    // -----------------------------------------------------------------------
    // Tests ported from test/cli-skills-lint.test.ts
    // -----------------------------------------------------------------------

    // --- collectSkillFiles ---

    #[test]
    fn collect_skill_files_nonexistent_dir_returns_empty() {
        // "returns empty array when the directory does not exist"
        let tmp = make_temp_dir();
        let result = collect_skill_files(&tmp.path().join("nonexistent"));
        assert!(result.is_empty());
    }

    #[test]
    fn collect_skill_files_nonexistent_with_issues_returns_empty() {
        let tmp = make_temp_dir();
        let result = collect_skill_files_with_issues(&tmp.path().join("nonexistent"));
        assert!(result.skills.is_empty());
        assert!(result.issues.is_empty());
    }

    #[test]
    fn collect_skill_files_flat_md_file() {
        // "parses a flat .md file as a skill"
        let tmp = make_temp_dir();
        write_file(
            tmp.path(),
            "my-skill.md",
            "---\nname: my-skill\ndescription: A great skill\n---\n\nBody here.",
        );
        let skills = collect_skill_files(tmp.path());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "my-skill");
        assert_eq!(skills[0].description, "A great skill");
    }

    #[test]
    fn collect_skill_files_skill_md_in_subdir() {
        // "parses a directory with SKILL.md"
        let tmp = make_temp_dir();
        let skill_dir = tmp.path().join("my-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        write_file(
            &skill_dir,
            "SKILL.md",
            "---\nname: my-skill\ndescription: Dir skill\n---\n\nContent.",
        );
        let skills = collect_skill_files(tmp.path());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "my-skill");
    }

    #[test]
    fn collect_skill_files_dir_without_skill_md_issues() {
        // "reports an issue for a directory missing SKILL.md"
        let tmp = make_temp_dir();
        fs::create_dir_all(tmp.path().join("bad-skill")).unwrap();
        let result = collect_skill_files_with_issues(tmp.path());
        assert!(result.skills.is_empty());
        assert_eq!(result.issues.len(), 1);
        assert!(result.issues[0].contains("missing SKILL.md"));
    }

    #[test]
    fn collect_skill_files_non_md_files_ignored() {
        // "ignores files that are not .md"
        let tmp = make_temp_dir();
        write_file(tmp.path(), "not-a-skill.txt", "ignored");
        write_file(tmp.path(), "also-not.json", "{}");
        let skills = collect_skill_files(tmp.path());
        assert!(skills.is_empty());
    }

    #[test]
    fn collect_skill_files_sorted_alphabetically() {
        // "returns skills sorted alphabetically"
        let tmp = make_temp_dir();
        write_file(tmp.path(), "zz.md", "---\nname: zz\n---\n\nZ skill.");
        write_file(tmp.path(), "aa.md", "---\nname: aa\n---\n\nA skill.");
        write_file(tmp.path(), "mm.md", "---\nname: mm\n---\n\nM skill.");
        let skills = collect_skill_files(tmp.path());
        assert_eq!(skills.len(), 3);
        assert_eq!(skills[0].name, "aa");
        assert_eq!(skills[1].name, "mm");
        assert_eq!(skills[2].name, "zz");
    }

    #[test]
    fn collect_skill_files_description_falls_back_to_first_non_empty_line() {
        // "falls back to first non-empty body line when no description in frontmatter"
        let tmp = make_temp_dir();
        write_file(
            tmp.path(),
            "skill.md",
            "---\nname: skill\n---\n\n# My heading\n\nSome description here.",
        );
        let skills = collect_skill_files(tmp.path());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].description, "My heading");
    }

    #[test]
    fn collect_skill_files_name_falls_back_to_filename() {
        // "falls back to filename stem when no name in frontmatter"
        let tmp = make_temp_dir();
        write_file(tmp.path(), "my-tool.md", "---\ndescription: My tool\n---\n\nBody.");
        let skills = collect_skill_files(tmp.path());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "my-tool");
    }

    // --- discoverSkills ---

    #[test]
    fn discover_skills_project_shadows_user() {
        // "project skills shadow user skills of the same name"
        let tmp = make_temp_dir();
        let cwd = tmp.path().join("project");
        let home_skills = tmp.path().join("home_skills");

        // User skill "tdd"
        fs::create_dir_all(&home_skills).unwrap();
        write_file(
            &home_skills,
            "tdd.md",
            "---\nname: tdd\ndescription: User TDD\n---\n\nUser version.",
        );

        // Project skill "tdd" (shadows user)
        let proj_skills = cwd.join(".drip").join("skills");
        fs::create_dir_all(&proj_skills).unwrap();
        write_file(
            &proj_skills,
            "tdd.md",
            "---\nname: tdd\ndescription: Project TDD\n---\n\nProject version.",
        );

        let skills = discover_skills(&cwd, &home_skills, None, None);
        let tdd: Vec<_> = skills.iter().filter(|s| s.name == "tdd").collect();
        assert_eq!(tdd.len(), 1);
        assert_eq!(tdd[0].source, SkillSource::Project);
    }

    #[test]
    fn discover_skills_user_shadows_marketplace() {
        // "user skills shadow marketplace skills"
        let tmp = make_temp_dir();
        let cwd = tmp.path().join("project");
        let home_skills = tmp.path().join("home_skills");

        fs::create_dir_all(&home_skills).unwrap();
        write_file(
            &home_skills,
            "tdd.md",
            "---\nname: tdd\ndescription: User TDD\n---\n\nUser.",
        );

        let marketplace = vec![CliSkill {
            description: "Marketplace TDD".to_string(),
            key: Some("acme/tdd-kit/tdd".to_string()),
            name: "tdd".to_string(),
            path: "/fake/tdd".to_string(),
            source: SkillSource::Marketplace,
        }];

        let skills = discover_skills(&cwd, &home_skills, Some(marketplace), None);
        let tdd: Vec<_> = skills.iter().filter(|s| s.name == "tdd").collect();
        assert_eq!(tdd.len(), 1);
        assert_eq!(tdd[0].source, SkillSource::User);
    }

    #[test]
    fn discover_skills_builtin_pack_present_when_no_override() {
        // "built-in skills appear when not shadowed"
        let tmp = make_temp_dir();
        let cwd = tmp.path().join("project");
        let home_skills = tmp.path().join("home_skills");
        fs::create_dir_all(&home_skills).unwrap();

        let skills = discover_skills(&cwd, &home_skills, None, None);
        // Should include built-ins
        assert!(skills.iter().any(|s| s.name == "tdd"));
        assert!(skills.iter().any(|s| s.name == "verify-before-done"));
        assert!(skills
            .iter()
            .all(|s| s.source != SkillSource::Project && s.source != SkillSource::User
                || s.source == SkillSource::Builtin));
    }

    #[test]
    fn discover_skills_with_issues_collects_issues() {
        // "collects issues from all sources"
        let tmp = make_temp_dir();
        let cwd = tmp.path().join("project");
        let home_skills = tmp.path().join("home_skills");

        // Create a dir with missing SKILL.md in project skills
        let proj_skills = cwd.join(".drip").join("skills");
        fs::create_dir_all(&proj_skills).unwrap();
        fs::create_dir_all(proj_skills.join("bad-skill")).unwrap();

        let result = discover_skills_with_issues(&cwd, &home_skills, None, None);
        assert!(result.issues.iter().any(|i| i.contains("missing SKILL.md")));
    }

    // --- resolveEffectiveActivation ---

    #[test]
    fn resolve_effective_activation_no_skills_flag() {
        // "--no-skills clears skills"
        let result = resolve_effective_activation(ResolveEffectiveActivationArgs {
            explicit: &[],
            explicit_profile: None,
            no_skills: true,
            resume_like: true,
            stored: Some(&SessionRunConfig {
                profile: Some("claude-sonnet-46".to_string()),
                skills: vec![SkillActivationEntry {
                    args: HashMap::new(),
                    name: "tdd".to_string(),
                }],
            }),
        });
        assert!(result.entries.is_empty());
        // --no-skills but still reuses the pinned profile
        assert_eq!(result.profile.as_deref(), Some("claude-sonnet-46"));
        assert!(!result.reactivated);
    }

    #[test]
    fn resolve_effective_activation_resume_reactivates() {
        // "resume-like invocations re-activate stored skills"
        let stored = SessionRunConfig {
            profile: Some("claude-sonnet-46".to_string()),
            skills: vec![SkillActivationEntry {
                args: HashMap::new(),
                name: "tdd".to_string(),
            }],
        };

        let result = resolve_effective_activation(ResolveEffectiveActivationArgs {
            explicit: &[],
            explicit_profile: None,
            no_skills: false,
            resume_like: true,
            stored: Some(&stored),
        });
        assert_eq!(result.entries.len(), 1);
        assert_eq!(result.entries[0].name, "tdd");
        assert!(result.reactivated);
        assert_eq!(result.profile.as_deref(), Some("claude-sonnet-46"));
    }

    #[test]
    fn resolve_effective_activation_fresh_session_no_inherit() {
        // "Fresh sessions never inherit another run's activation file"
        let stored = SessionRunConfig {
            profile: Some("claude-sonnet-46".to_string()),
            skills: vec![SkillActivationEntry {
                args: HashMap::new(),
                name: "tdd".to_string(),
            }],
        };
        let result = resolve_effective_activation(ResolveEffectiveActivationArgs {
            explicit: &[],
            explicit_profile: None,
            no_skills: false,
            resume_like: false,
            stored: Some(&stored),
        });
        assert!(result.entries.is_empty());
        assert!(result.profile.is_none());
    }

    #[test]
    fn resolve_effective_activation_explicit_replaces_stored() {
        // "Explicit flags replace the stored set"
        let stored = SessionRunConfig {
            profile: Some("claude-sonnet-46".to_string()),
            skills: vec![SkillActivationEntry {
                args: HashMap::new(),
                name: "tdd".to_string(),
            }],
        };
        let explicit = vec!["migration:from=jest,to=vitest".to_string()];
        let result = resolve_effective_activation(ResolveEffectiveActivationArgs {
            explicit: &explicit,
            explicit_profile: None,
            no_skills: false,
            resume_like: true,
            stored: Some(&stored),
        });
        assert_eq!(result.entries.len(), 1);
        assert_eq!(result.entries[0].name, "migration");
        assert_eq!(result.entries[0].args.get("from").map(|s| s.as_str()), Some("jest"));
        assert_eq!(result.entries[0].args.get("to").map(|s| s.as_str()), Some("vitest"));
        assert!(!result.reactivated);
    }

    #[test]
    fn resolve_effective_activation_explicit_profile_suppresses_stored() {
        // "An explicit --profile suppresses the stored one"
        let stored = SessionRunConfig {
            profile: Some("claude-sonnet-46".to_string()),
            skills: vec![],
        };
        let result = resolve_effective_activation(ResolveEffectiveActivationArgs {
            explicit: &[],
            explicit_profile: Some("openai-gpt5-mini"),
            no_skills: false,
            resume_like: true,
            stored: Some(&stored),
        });
        assert!(result.profile.is_none());
    }

    // -----------------------------------------------------------------------
    // Tests ported from test/cli-skills-params.test.ts
    // -----------------------------------------------------------------------

    // --- Frontmatter args block parsing ---

    #[test]
    fn parse_frontmatter_args_with_defaults_and_required() {
        let content = "---\nname: migrate\ndescription: Migration skill\nargs:\n  from:\n  to: vitest\n---\n\nBody.";
        let fm = parse_skill_frontmatter(content);
        assert_eq!(fm.name.as_deref(), Some("migrate"));
        let args = fm.args.unwrap();
        assert_eq!(args.len(), 2);
        assert_eq!(args[0].name, "from");
        assert!(args[0].default.is_none()); // required
        assert_eq!(args[1].name, "to");
        assert_eq!(args[1].default.as_deref(), Some("vitest"));
    }

    #[test]
    fn load_skill_content_substitutes_placeholders() {
        // "loadSkillContent substitution happy path"
        let tmp = make_temp_dir();
        let skill_dir = tmp.path().join("migrate");
        fs::create_dir_all(&skill_dir).unwrap();
        write_file(
            &skill_dir,
            "SKILL.md",
            "---\nname: migrate\ndescription: Migration\nargs:\n  from:\n  to: vitest\n---\n\nMigrate from {{from}} to {{to}}.",
        );

        let skill = CliSkill {
            description: "Migration".to_string(),
            key: None,
            name: "migrate".to_string(),
            path: skill_dir.join("SKILL.md").to_string_lossy().into_owned(),
            source: SkillSource::User,
        };

        let mut args = HashMap::new();
        args.insert("from".to_string(), "jest".to_string());
        let loaded = load_skill_content(&skill, Some(&args)).unwrap();
        assert!(loaded.content.contains("from jest to vitest"));
    }

    #[test]
    fn load_skill_content_missing_required_arg_errors() {
        // "Missing required arg throws"
        let tmp = make_temp_dir();
        let skill_dir = tmp.path().join("migrate");
        fs::create_dir_all(&skill_dir).unwrap();
        write_file(
            &skill_dir,
            "SKILL.md",
            "---\nname: migrate\nargs:\n  from:\n---\n\nMigrate from {{from}}.",
        );

        let skill = CliSkill {
            description: "Migration".to_string(),
            key: None,
            name: "migrate".to_string(),
            path: skill_dir.join("SKILL.md").to_string_lossy().into_owned(),
            source: SkillSource::User,
        };

        let result = load_skill_content(&skill, None);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("missing required arg"), "got: {}", err);
        assert!(err.contains("\"from\""), "got: {}", err);
    }

    #[test]
    fn load_skill_content_unknown_arg_errors() {
        // "Unknown supplied arg throws"
        let tmp = make_temp_dir();
        let skill_dir = tmp.path().join("migrate");
        fs::create_dir_all(&skill_dir).unwrap();
        write_file(
            &skill_dir,
            "SKILL.md",
            "---\nname: migrate\nargs:\n  from:\n---\n\nMigrate from {{from}}.",
        );

        let skill = CliSkill {
            description: "Migration".to_string(),
            key: None,
            name: "migrate".to_string(),
            path: skill_dir.join("SKILL.md").to_string_lossy().into_owned(),
            source: SkillSource::User,
        };

        let mut args = HashMap::new();
        args.insert("from".to_string(), "jest".to_string());
        args.insert("unknown_key".to_string(), "oops".to_string());
        let result = load_skill_content(&skill, Some(&args));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("unknown arg"), "got: {}", err);
        assert!(err.contains("\"unknown_key\""), "got: {}", err);
    }

    #[test]
    fn load_skill_content_no_args_block_returned_as_is() {
        // "No-args-block skills untouched (plain and with literal {{x}} in body)"
        let tmp = make_temp_dir();
        let skill_dir = tmp.path().join("plain");
        fs::create_dir_all(&skill_dir).unwrap();
        write_file(
            &skill_dir,
            "SKILL.md",
            "---\nname: plain\n---\n\nPlain content with {{literal}} braces.",
        );

        let skill = CliSkill {
            description: "Plain".to_string(),
            key: None,
            name: "plain".to_string(),
            path: skill_dir.join("SKILL.md").to_string_lossy().into_owned(),
            source: SkillSource::User,
        };

        let loaded = load_skill_content(&skill, None).unwrap();
        assert!(loaded.content.contains("{{literal}}"));
    }

    // --- parseSkillFlagValue ---

    #[test]
    fn parse_skill_flag_value_bare_name() {
        // "bare skill name"
        let (name, args) = parse_skill_flag_value("tdd");
        assert_eq!(name, "tdd");
        assert!(args.is_empty());
    }

    #[test]
    fn parse_skill_flag_value_single_pair() {
        // "single key=value pair"
        let (name, args) = parse_skill_flag_value("migrate:from=jest");
        assert_eq!(name, "migrate");
        assert_eq!(args.get("from").map(|s| s.as_str()), Some("jest"));
    }

    #[test]
    fn parse_skill_flag_value_multiple_pairs() {
        // "multiple key=value pairs separated by commas"
        let (name, args) = parse_skill_flag_value("migrate:from=jest,to=vitest");
        assert_eq!(name, "migrate");
        assert_eq!(args.get("from").map(|s| s.as_str()), Some("jest"));
        assert_eq!(args.get("to").map(|s| s.as_str()), Some("vitest"));
    }

    #[test]
    fn parse_skill_flag_value_value_with_equals() {
        // "value containing '=' after the first (only first '=' splits key/value)"
        let (name, args) = parse_skill_flag_value("x:key=a=b");
        assert_eq!(name, "x");
        assert_eq!(args.get("key").map(|s| s.as_str()), Some("a=b"));
    }

    #[test]
    fn parse_skill_flag_value_multiple_pairs_one_with_equals() {
        // "multiple pairs where one value contains '='"
        let (name, args) = parse_skill_flag_value("x:k1=a=b,k2=c");
        assert_eq!(name, "x");
        assert_eq!(args.get("k1").map(|s| s.as_str()), Some("a=b"));
        assert_eq!(args.get("k2").map(|s| s.as_str()), Some("c"));
    }

    #[test]
    fn parse_skill_flag_value_colon_but_empty_args() {
        // "skill name with colon but empty args part produces empty args"
        let (name, args) = parse_skill_flag_value("skill:");
        assert_eq!(name, "skill");
        assert!(args.is_empty());
    }

    // --- composeSkillSystemPrompt ---

    #[test]
    fn compose_skill_system_prompt_no_skills_returns_base() {
        let base = "You are a helpful assistant.";
        let result = compose_skill_system_prompt(base, &[]);
        assert_eq!(result, base);
    }

    #[test]
    fn compose_skill_system_prompt_with_skills() {
        let base = "You are a helpful assistant.";
        let skills = vec![LoadedCliSkill {
            content: "Always write tests first.".to_string(),
            name: "tdd".to_string(),
        }];
        let result = compose_skill_system_prompt(base, &skills);
        assert!(result.starts_with(base));
        assert!(result.contains("# Skill: tdd"));
        assert!(result.contains("Always write tests first."));
    }

    // --- Built-in skills embedded ---

    #[test]
    fn builtin_skills_all_seven_present() {
        let skills = collect_builtin_skills(None);
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        for expected in &[
            "commit-discipline",
            "debug-root-cause",
            "migration-discipline",
            "refactor-safely",
            "review-independently",
            "tdd",
            "verify-before-done",
        ] {
            assert!(names.contains(expected), "missing builtin: {}", expected);
        }
    }

    #[test]
    fn builtin_skills_all_have_descriptions() {
        let skills = collect_builtin_skills(None);
        for s in &skills {
            assert!(
                !s.description.is_empty(),
                "builtin {} has empty description",
                s.name
            );
        }
    }

    // --- save/load skill activation ---

    #[test]
    fn save_and_load_skill_activation_roundtrip() {
        let tmp = make_temp_dir();
        let path = tmp.path().join("skills.json");

        let config = SessionRunConfig {
            profile: Some("claude-sonnet-46".to_string()),
            skills: vec![
                SkillActivationEntry {
                    args: {
                        let mut m = HashMap::new();
                        m.insert("from".to_string(), "jest".to_string());
                        m
                    },
                    name: "migration".to_string(),
                },
                SkillActivationEntry {
                    args: HashMap::new(),
                    name: "tdd".to_string(),
                },
            ],
        };

        save_skill_activation(&path, &config).unwrap();
        let loaded = load_skill_activation(&path).unwrap();
        assert_eq!(loaded.profile.as_deref(), Some("claude-sonnet-46"));
        assert_eq!(loaded.skills.len(), 2);
        assert_eq!(loaded.skills[0].name, "migration");
        assert_eq!(
            loaded.skills[0].args.get("from").map(|s| s.as_str()),
            Some("jest")
        );
        assert_eq!(loaded.skills[1].name, "tdd");
    }

    #[test]
    fn load_skill_activation_missing_file_returns_none() {
        let tmp = make_temp_dir();
        let result = load_skill_activation(&tmp.path().join("nonexistent.json"));
        assert!(result.is_none());
    }

    #[test]
    fn load_skill_activation_v034_bare_names() {
        // v0.34 stored bare names
        let tmp = make_temp_dir();
        let path = tmp.path().join("skills.json");
        fs::write(&path, r#"{"skills": ["tdd", "refactor-safely"]}"#).unwrap();
        let config = load_skill_activation(&path).unwrap();
        assert_eq!(config.skills.len(), 2);
        assert_eq!(config.skills[0].name, "tdd");
        assert!(config.skills[0].args.is_empty());
    }

    #[test]
    fn load_skill_activation_invalid_returns_none() {
        let tmp = make_temp_dir();
        let path = tmp.path().join("skills.json");
        fs::write(&path, "not json").unwrap();
        let result = load_skill_activation(&path);
        assert!(result.is_none());
    }
}
