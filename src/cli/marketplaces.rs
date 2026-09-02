// port of src/cli/marketplaces.ts
//
// A marketplace is a repo (git URL or local directory) that ships plugins in
// the Claude Code layout — `.claude-plugin/marketplace.json` listing plugins,
// each with optional `.claude-plugin/plugin.json`, `skills/<name>/SKILL.md`,
// and `agents/*.md` — or, as a fallback, a bare repo whose root `skills/`
// directory is treated as a single plugin named after the marketplace.

use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::core::home::DripHome;
use crate::lib_fs::write_file_atomic;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketplaceRecord {
    pub added_at: String,
    pub kind: String, // "git" | "local"
    pub name: String,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketplacesFile {
    /// Plugin or skill keys explicitly disabled (a skill key overrides its plugin key).
    pub disabled: Vec<String>,
    /// Plugin or skill keys explicitly enabled. Marketplace content is disabled until enabled.
    pub enabled: Vec<String>,
    pub marketplaces: Vec<MarketplaceRecord>,
    pub version: u8,
}

impl Default for MarketplacesFile {
    fn default() -> Self {
        MarketplacesFile {
            disabled: Vec::new(),
            enabled: Vec::new(),
            marketplaces: Vec::new(),
            version: 1,
        }
    }
}

/// Project-scope overrides at <cwd>/.drip/plugins.json — the project wins over the user registry per key.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProjectPluginOverrides {
    pub disabled: Vec<String>,
    pub enabled: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketplaceSkillEntry {
    pub description: String,
    /// "<marketplace>/<plugin>/<skill>" — the enable/disable key.
    pub key: String,
    pub marketplace_name: String,
    pub name: String,
    pub path: String,
    pub plugin_name: String,
}

// A plugin agent file (agents/<name>.md) maps onto a harness role definition:
// the frontmatter carries the name, description, and optional tool allowlist,
// and the body becomes the role's prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketplaceRoleEntry {
    pub description: Option<String>,
    /// "<marketplace>/<plugin>/<agent>" — enable/disable key (plugin key also applies).
    pub key: String,
    pub marketplace_name: String,
    pub name: String,
    pub plugin_name: String,
    pub prompt: String,
    pub tools: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketplacePlugin {
    pub description: String,
    /// "<marketplace>/<plugin>" — the enable/disable key.
    pub key: String,
    pub marketplace_name: String,
    pub name: String,
    pub path: String,
    pub roles: Vec<MarketplaceRoleEntry>,
    pub skills: Vec<MarketplaceSkillEntry>,
}

/// The provider-agnostic skill-file shape: a SKILL.md (or flat .md) with an
/// optional frontmatter name and description. Marketplace plugins and local
/// skill directories share this layout. (Private copy from skills.ts — see task notes.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillFileEntry {
    pub description: String,
    pub name: String,
    pub path: String,
}

// ---------------------------------------------------------------------------
// Registry load/save
// ---------------------------------------------------------------------------

pub fn load_marketplaces_file(marketplaces_path: &Path) -> anyhow::Result<MarketplacesFile> {
    if !marketplaces_path.exists() {
        return Ok(MarketplacesFile::default());
    }

    let raw = fs::read_to_string(marketplaces_path)?;
    let parsed_value: Value = serde_json::from_str(&raw)?;

    let Some(marketplaces) = parsed_value.get("marketplaces").and_then(Value::as_array) else {
        anyhow::bail!(
            "The file at {} is not a valid drip marketplaces file.",
            marketplaces_path.display()
        );
    };

    let string_list = |value: Option<&Value>| -> Vec<String> {
        value
            .and_then(Value::as_array)
            .map(|keys| {
                keys.iter()
                    .filter_map(|key| key.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };

    let records = marketplaces
        .iter()
        .filter_map(|entry| {
            let name = entry.get("name")?.as_str()?.to_string();
            let source = entry.get("source")?.as_str()?.to_string();
            let kind = entry.get("kind")?.as_str()?.to_string();

            if kind != "git" && kind != "local" {
                return None;
            }

            let added_at = entry
                .get("addedAt")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();

            Some(MarketplaceRecord {
                added_at: added_at,
                kind,
                name,
                source,
            })
        })
        .collect();

    Ok(MarketplacesFile {
        disabled: string_list(parsed_value.get("disabled")),
        enabled: string_list(parsed_value.get("enabled")),
        marketplaces: records,
        version: 1,
    })
}

pub fn save_marketplaces_file(marketplaces_path: &Path, file: &MarketplacesFile) -> std::io::Result<()> {
    let json = serde_json::to_string_pretty(file)?;
    write_file_atomic(marketplaces_path, &format!("{json}\n"), true)
}

pub fn load_project_plugin_overrides(cwd: &Path) -> ProjectPluginOverrides {
    let overrides_path = cwd.join(".drip").join("plugins.json");

    if !overrides_path.exists() {
        return ProjectPluginOverrides::default();
    }

    let Ok(raw) = fs::read_to_string(&overrides_path) else {
        return ProjectPluginOverrides::default();
    };

    let Ok(parsed_value) = serde_json::from_str::<Value>(&raw) else {
        return ProjectPluginOverrides::default();
    };

    let string_list = |key: &str| -> Vec<String> {
        parsed_value
            .get(key)
            .and_then(Value::as_array)
            .map(|keys| {
                keys.iter()
                    .filter_map(|key| key.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };

    ProjectPluginOverrides {
        disabled: string_list("disabled"),
        enabled: string_list("enabled"),
    }
}

// ---------------------------------------------------------------------------
// Marketplace source helpers
// ---------------------------------------------------------------------------

pub fn marketplace_name_from_source(source: &str) -> String {
    let without_trailing_slashes = source.trim_end_matches('/');
    let base = Path::new(without_trailing_slashes)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let base = base.strip_suffix(".git").unwrap_or(base);

    let sanitized = replace_marketplace_name_separators(base);

    if sanitized.is_empty() {
        "marketplace".to_string()
    } else {
        sanitized
    }
}

/// `replace(/[^A-Za-z0-9._-]+/g, "-")` — shared by name derivation and add.
fn replace_marketplace_name_separators(value: &str) -> String {
    let re = Regex::new(r"[^A-Za-z0-9._-]+").expect("static regex");
    re.replace_all(value, "-").into_owned()
}

pub fn marketplace_clone_dir(home: &DripHome, name: &str) -> PathBuf {
    Path::new(&home.marketplaces_dir).join(name)
}

fn marketplace_root_dir(home: &DripHome, record: &MarketplaceRecord) -> PathBuf {
    if record.kind == "local" {
        PathBuf::from(&record.source)
    } else {
        marketplace_clone_dir(home, &record.name)
    }
}

// ---------------------------------------------------------------------------
// Git registry operations (port of marketplaces.ts lines 74-79, 160-250)
// ---------------------------------------------------------------------------

/// Injectable `git` runner so tests (and callers) can stub out real git usage.
pub type GitRunner<'a> = &'a dyn Fn(&[String], Option<&Path>) -> anyhow::Result<()>;

pub fn run_git(args: &[String], cwd: Option<&Path>) -> anyhow::Result<()> {
    let mut command = std::process::Command::new("git");
    command.args(args);

    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }

    let output = command.output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(anyhow::anyhow!("{}", stderr));
    }

    Ok(())
}

/// `resolve(source)` — Node's path.resolve without symlink resolution:
/// relative paths are joined onto the current working directory.
/// `path.resolve(source)` — absolutizes against the cwd and collapses `.` /
/// `..`, so `--marketplace-add ./market` registers `<cwd>/market`, not
/// `<cwd>/./market`.
fn resolve_source_path(source: &str) -> anyhow::Result<std::path::PathBuf> {
    Ok(resolve_path(&std::env::current_dir()?, Path::new(source)))
}

fn remove_path_force(path: &Path) -> std::io::Result<()> {
    if !path.exists() {
        return Ok(());
    }

    if path.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

pub struct AddMarketplaceArgs<'a> {
    pub git: Option<GitRunner<'a>>,
    pub home: &'a crate::core::home::DripHome,
    pub name: Option<&'a str>,
    pub now: Option<&'a dyn Fn() -> chrono::DateTime<chrono::Utc>>,
    pub source: &'a str,
}

#[derive(Debug)]
pub struct AddedMarketplace {
    pub file: MarketplacesFile,
    pub record: MarketplaceRecord,
}

pub fn add_marketplace(args: AddMarketplaceArgs<'_>) -> anyhow::Result<AddedMarketplace> {
    let file = load_marketplaces_file(std::path::Path::new(&args.home.marketplaces_path))?;
    let local_path = resolve_source_path(args.source)?;
    let is_local = local_path.is_dir();
    // `args.name?.trim() || marketplaceNameFromSource(args.source)` — an empty
    // or whitespace-only name falls back to the source-derived name.
    let raw_name = match args.name.map(str::trim) {
        Some(name) if !name.is_empty() => name.to_string(),
        _ => marketplace_name_from_source(args.source),
    };
    let name = Regex::new(r"[^A-Za-z0-9._-]+")
        .expect("static regex")
        .replace_all(&raw_name, "-")
        .into_owned();

    if file
        .marketplaces
        .iter()
        .any(|record| record.name == name)
    {
        anyhow::bail!(
            "A marketplace named \"{}\" is already registered. Remove it first or pass a different name.",
            name
        );
    }

    let now = match args.now {
        Some(now) => now(),
        None => chrono::Utc::now(),
    };

    let record = MarketplaceRecord {
        added_at: now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        kind: if is_local { "local".to_string() } else { "git".to_string() },
        name: name.clone(),
        source: if is_local {
            local_path.to_string_lossy().to_string()
        } else {
            args.source.to_string()
        },
    };

    if record.kind == "git" {
        let target = marketplace_clone_dir(args.home, &record.name);

        if target.exists() {
            remove_path_force(&target)?;
        }

        let run: GitRunner = match args.git {
            Some(git) => git,
            None => &run_git,
        };

        run(
            &[
                "clone".to_string(),
                "--depth".to_string(),
                "1".to_string(),
                record.source.clone(),
                target.to_string_lossy().to_string(),
            ],
            None,
        )?;
    }

    let mut file = file;
    file.marketplaces.push(record.clone());
    save_marketplaces_file(std::path::Path::new(&args.home.marketplaces_path), &file)?;

    Ok(AddedMarketplace { file, record })
}

pub fn remove_marketplace(
    home: &crate::core::home::DripHome,
    name: &str,
) -> anyhow::Result<MarketplacesFile> {
    let file = load_marketplaces_file(std::path::Path::new(&home.marketplaces_path))?;
    let record = file
        .marketplaces
        .iter()
        .find(|entry| entry.name == name);

    let Some(record) = record else {
        anyhow::bail!("No marketplace named \"{}\" is registered.", name);
    };

    if record.kind == "git" {
        let target = marketplace_clone_dir(home, &record.name);
        if target.exists() {
            remove_path_force(&target)?;
        }
    }

    let key_prefix = format!("{}/", record.name);
    let next_file = MarketplacesFile {
        disabled: file
            .disabled
            .iter()
            .filter(|key| !key.starts_with(&key_prefix))
            .cloned()
            .collect(),
        enabled: file
            .enabled
            .iter()
            .filter(|key| !key.starts_with(&key_prefix))
            .cloned()
            .collect(),
        marketplaces: file
            .marketplaces
            .iter()
            .filter(|entry| entry.name != name)
            .cloned()
            .collect(),
        version: 1,
    };

    save_marketplaces_file(std::path::Path::new(&home.marketplaces_path), &next_file)?;

    Ok(next_file)
}

pub fn update_marketplaces(
    git: Option<GitRunner<'_>>,
    home: &crate::core::home::DripHome,
    name: Option<&str>,
) -> anyhow::Result<Vec<String>> {
    let file = load_marketplaces_file(std::path::Path::new(&home.marketplaces_path))?;
    let targets: Vec<&MarketplaceRecord> = file
        .marketplaces
        .iter()
        .filter(|record| match name {
            Some(name) => record.name == name,
            None => true,
        })
        .collect();

    if let Some(name) = name {
        if targets.is_empty() {
            anyhow::bail!("No marketplace named \"{}\" is registered.", name);
        }
    }

    let mut updated: Vec<String> = Vec::new();
    let run: &dyn Fn(&[String], Option<&Path>) -> anyhow::Result<()> = match git {
        Some(git) => git,
        None => &run_git as &dyn Fn(&[String], Option<&Path>) -> anyhow::Result<()>,
    };

    for record in targets {
        if record.kind != "git" {
            continue;
        }

        run(
            &["pull".to_string(), "--ff-only".to_string()],
            Some(&marketplace_clone_dir(home, &record.name)),
        )?;
        updated.push(record.name.clone());
    }

    Ok(updated)
}

// ---------------------------------------------------------------------------
// Agent (role) parsing
// ---------------------------------------------------------------------------

struct ParsedAgentFrontmatter {
    body: String,
    description: Option<String>,
    name: Option<String>,
    tools: Option<Vec<String>>,
}

/// Minimal frontmatter reader for agent files: name, description, and a
/// comma-separated tools line. Anything else in the frontmatter is ignored.
fn parse_agent_frontmatter(markdown: &str) -> ParsedAgentFrontmatter {
    let re = Regex::new(r"(?s)^---\n(.*?)\n---\n?").expect("static regex");

    let Some(captures) = re.captures(markdown) else {
        return ParsedAgentFrontmatter {
            body: markdown.trim().to_string(),
            description: None,
            name: None,
            tools: None,
        };
    };

    let full_match = captures.get(0).map(|m| m.as_str()).unwrap_or("");
    let frontmatter = captures.get(1).map(|m| m.as_str()).unwrap_or("");
    let mut fields = ParsedAgentFrontmatter {
        body: String::new(),
        description: None,
        name: None,
        tools: None,
    };

    let field_re = Regex::new(r"^(description|name|tools):\s*(.+)\s*$").expect("static regex");

    for line in frontmatter.split('\n') {
        let Some(field_match) = field_re.captures(line) else {
            continue;
        };

        let field_name = field_match.get(1).map(|m| m.as_str()).unwrap_or("");

        if field_name == "tools" {
            let tool_names: Vec<String> = field_match
                .get(2)
                .map(|m| m.as_str())
                .unwrap_or("")
                .split(',')
                .map(|tool_name| tool_name.trim().to_string())
                .filter(|tool_name| !tool_name.is_empty())
                .collect();

            if !tool_names.is_empty() {
                fields.tools = Some(tool_names);
            }
        } else if field_name == "description" {
            fields.description = Some(field_match.get(2).map(|m| m.as_str()).unwrap_or("").trim().to_string());
        } else {
            fields.name = Some(field_match.get(2).map(|m| m.as_str()).unwrap_or("").trim().to_string());
        }
    }

    fields.body = markdown[full_match.len()..].trim().to_string();

    fields
}

fn collect_agent_roles(agents_dir: &Path, marketplace_name: &str, plugin_name: &str) -> Vec<MarketplaceRoleEntry> {
    if !agents_dir.exists() {
        return Vec::new();
    }

    let mut entry_names: Vec<String> = match fs::read_dir(agents_dir) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect(),
        Err(_) => return Vec::new(),
    };
    entry_names.sort();

    let mut roles = Vec::new();

    for entry_name in entry_names {
        if !entry_name.to_lowercase().ends_with(".md") {
            continue;
        }

        let Ok(markdown) = fs::read_to_string(agents_dir.join(&entry_name)) else {
            continue;
        };

        let parsed = parse_agent_frontmatter(&markdown);
        let re = Regex::new(r"(?i)\.md$").expect("static regex");
        let name = parsed
            .name
            .clone()
            .unwrap_or_else(|| re.replace(&entry_name, "").into_owned());

        if parsed.body.is_empty() {
            continue;
        }

        roles.push(MarketplaceRoleEntry {
            description: parsed.description,
            key: format!("{marketplace_name}/{plugin_name}/{name}"),
            marketplace_name: marketplace_name.to_string(),
            name,
            plugin_name: plugin_name.to_string(),
            prompt: parsed.body,
            tools: parsed.tools,
        });
    }

    roles
}

// ---------------------------------------------------------------------------
// Skill-file collection (private copy of skills.ts collectSkillFiles)
// ---------------------------------------------------------------------------

/// Normalize raw file content: strip UTF-8 BOM and convert CRLF to LF.
fn normalize_content(raw: &str) -> String {
    // Strip UTF-8 BOM (U+FEFF) if present at the start
    let stripped = raw.strip_prefix('\u{FEFF}').unwrap_or(raw);
    // Normalize CRLF to LF
    stripped.replace("\r\n", "\n")
}

struct ParsedSkillFrontmatter {
    description: Option<String>,
    name: Option<String>,
}

fn parse_skill_frontmatter(markdown: &str) -> ParsedSkillFrontmatter {
    let re = Regex::new(r"(?s)^---\n(.*?)\n---").expect("static regex");
    let Some(captures) = re.captures(markdown) else {
        return ParsedSkillFrontmatter {
            description: None,
            name: None,
        };
    };

    let frontmatter = captures.get(1).map(|m| m.as_str()).unwrap_or("");
    let field_re = Regex::new(r"^(description|name):\s*(.+)\s*$").expect("static regex");
    let mut fields = ParsedSkillFrontmatter {
        description: None,
        name: None,
    };

    for line in frontmatter.split('\n') {
        if let Some(field_match) = field_re.captures(line) {
            let value = field_match.get(2).map(|m| m.as_str()).unwrap_or("").trim().to_string();

            match field_match.get(1).map(|m| m.as_str()) {
                Some("description") => fields.description = Some(value),
                Some("name") => fields.name = Some(value),
                _ => {}
            }
        }
    }

    fields
}

fn first_non_empty_line(markdown: &str) -> String {
    let re = Regex::new(r"(?s)^---\n.*?\n---").expect("static regex");
    let body = re.replace(markdown, "");

    for line in body.split('\n') {
        let heading_re = Regex::new(r"^#+\s*").expect("static regex");
        let trimmed_line = heading_re.replace(line, "").trim().to_string();

        if !trimmed_line.is_empty() {
            return trimmed_line;
        }
    }

    String::new()
}

fn collect_skill_files(skills_dir: &Path) -> Vec<SkillFileEntry> {
    if !skills_dir.exists() {
        return Vec::new();
    }

    let mut entry_names: Vec<String> = match fs::read_dir(skills_dir) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect(),
        Err(_) => return Vec::new(),
    };
    entry_names.sort();

    let mut skills = Vec::new();

    for entry_name in entry_names {
        let entry_path = skills_dir.join(&entry_name);

        let skill_path;
        let mut default_name = entry_name.clone();

        let Ok(metadata) = fs::metadata(&entry_path) else {
            continue;
        };

        if metadata.is_dir() {
            // <skills>/<name>/SKILL.md — the layout other harnesses use.
            let candidate = entry_path.join("SKILL.md");

            if candidate.exists() {
                skill_path = candidate;
            } else {
                continue;
            }
        } else if entry_name.to_lowercase().ends_with(".md") {
            // Flat <skills>/<name>.md files also count, for quick one-file skills.
            skill_path = entry_path.clone();
            let re = Regex::new(r"(?i)\.md$").expect("static regex");
            default_name = re.replace(&entry_name, "").into_owned();
        } else {
            continue;
        }

        let Ok(raw) = fs::read_to_string(&skill_path) else {
            continue;
        };

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

    skills
}

// ---------------------------------------------------------------------------
// Plugin + repo parsing
// ---------------------------------------------------------------------------

fn parse_plugin_dir(
    plugin_path: &Path,
    marketplace_name: &str,
    fallback_name: &str,
    fallback_description: &str,
) -> MarketplacePlugin {
    let mut name = fallback_name.to_string();
    let mut description = fallback_description.to_string();
    let manifest_path = plugin_path.join(".claude-plugin").join("plugin.json");

    if manifest_path.exists() {
        if let Ok(raw) = fs::read_to_string(&manifest_path) {
            if let Ok(manifest) = serde_json::from_str::<Value>(&raw) {
                // A broken plugin.json falls back to the marketplace-declared identity.
                if let Some(manifest_name) = manifest.get("name").and_then(Value::as_str) {
                    if !manifest_name.trim().is_empty() {
                        name = manifest_name.trim().to_string();
                    }
                }

                if let Some(manifest_description) = manifest.get("description").and_then(Value::as_str) {
                    description = manifest_description.trim().to_string();
                }
            }
        }
    }

    let plugin_key = format!("{marketplace_name}/{name}");
    let roles = collect_agent_roles(&plugin_path.join("agents"), marketplace_name, &name);
    let skills = collect_skill_files(&plugin_path.join("skills"))
        .into_iter()
        .map(|entry| MarketplaceSkillEntry {
            description: entry.description,
            key: format!("{plugin_key}/{}", entry.name),
            marketplace_name: marketplace_name.to_string(),
            name: entry.name,
            path: entry.path,
            plugin_name: name.clone(),
        })
        .collect();

    MarketplacePlugin {
        description,
        key: plugin_key,
        marketplace_name: marketplace_name.to_string(),
        name,
        path: plugin_path.to_string_lossy().into_owned(),
        roles,
        skills,
    }
}

/// Parses one marketplace repo into its plugins. Claude-compatible repos are
/// read through .claude-plugin/marketplace.json; anything else with a skills/
/// (or agents/) directory at the root is treated as a single bare plugin.
pub struct ParsedMarketplaceRepo {
    pub issues: Vec<String>,
    pub plugins: Vec<MarketplacePlugin>,
}

pub fn parse_marketplace_repo(root_dir: &Path, marketplace_name: &str) -> ParsedMarketplaceRepo {
    let mut issues: Vec<String> = Vec::new();
    let mut plugins: Vec<MarketplacePlugin> = Vec::new();
    let manifest_path = root_dir.join(".claude-plugin").join("marketplace.json");

    if !root_dir.exists() {
        return ParsedMarketplaceRepo {
            issues: vec![format!(
                "marketplace \"{marketplace_name}\": directory {} does not exist (try /marketplace update)",
                root_dir.display()
            )],
            plugins,
        };
    }

    if !manifest_path.exists() {
        let bare_plugin = parse_plugin_dir(root_dir, marketplace_name, marketplace_name, "Bare skills repo.");

        if bare_plugin.skills.is_empty() && bare_plugin.roles.is_empty() {
            issues.push(format!(
                "marketplace \"{marketplace_name}\": no .claude-plugin/marketplace.json and no skills/ or agents/ directory at the repo root"
            ));
        } else {
            plugins.push(bare_plugin);
        }

        return ParsedMarketplaceRepo { issues, plugins };
    }

    let manifest: Value = match fs::read_to_string(&manifest_path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
    {
        Some(parsed) => parsed,
        None => {
            issues.push(format!(
                "marketplace \"{marketplace_name}\": could not parse marketplace.json (unknown error)"
            ));

            return ParsedMarketplaceRepo { issues, plugins };
        }
    };

    let Some(entries) = manifest.get("plugins").and_then(Value::as_array) else {
        issues.push(format!(
            "marketplace \"{marketplace_name}\": marketplace.json has no plugins array"
        ));

        return ParsedMarketplaceRepo { issues, plugins };
    };

    for entry in entries {
        let Some(plugin_name) = entry.get("name").and_then(Value::as_str) else {
            issues.push(format!(
                "marketplace \"{marketplace_name}\": skipped a plugin entry with no name"
            ));
            continue;
        };

        let source = entry.get("source").and_then(Value::as_str);
        let description = entry
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default();

        let Some(source) = source else {
            issues.push(format!(
                "marketplace \"{marketplace_name}\": plugin \"{plugin_name}\" uses a non-path source — only paths inside the marketplace repo are supported"
            ));
            continue;
        };

        let plugin_path = resolve_path(root_dir, Path::new(source));

        // A source like "../../etc" must not escape the marketplace repo.
        if plugin_path != root_dir && !plugin_path.starts_with(format!("{}{}", root_dir.display(), std::path::MAIN_SEPARATOR)) {
            issues.push(format!(
                "marketplace \"{marketplace_name}\": plugin \"{plugin_name}\" source escapes the marketplace repo and was skipped"
            ));
            continue;
        }

        if !plugin_path.exists() {
            issues.push(format!(
                "marketplace \"{marketplace_name}\": plugin \"{plugin_name}\" source {source} does not exist"
            ));
            continue;
        }

        plugins.push(parse_plugin_dir(&plugin_path, marketplace_name, plugin_name, description));
    }

    ParsedMarketplaceRepo { issues, plugins }
}

/// `path.resolve(root, source)` — absolutizes and normalizes `source` against
/// `root` (which must itself already be absolute/normalized, as every caller
/// passes a home-rooted or repo path).
fn resolve_path(root: &Path, source: &Path) -> PathBuf {
    let joined = if source.is_absolute() {
        source.to_path_buf()
    } else {
        root.join(source)
    };

    normalize_path(&joined)
}

/// Lexical path normalization mirroring Node's path.normalize for the shapes
/// used here (no symlink resolution): collapses `.` and `..` components.
fn normalize_path(path: &Path) -> PathBuf {
    let mut components: Vec<std::ffi::OsString> = Vec::new();

    let mut result = PathBuf::new();
    let prefix_len = {
        let text = path.to_string_lossy();
        if text.starts_with('/') {
            1
        } else {
            0
        }
    };

    for component in path.components() {
        match component {
            std::path::Component::RootDir => {}
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                // Node path.normalize: pop the previous component unless it is
                // itself ".." or there is nothing to pop; a relative path keeps
                // leading ".." segments, an absolute path cannot go above root.
                if components.last().map(|c| c.as_os_str() != "..").unwrap_or(false) {
                    components.pop();
                } else if prefix_len == 0 {
                    components.push(std::ffi::OsString::from(".."));
                }
            }
            other => components.push(other.as_os_str().to_os_string()),
        }
    }

    if prefix_len == 1 {
        result.push("/");
    }

    for component in components {
        result.push(component);
    }

    result
}

/// Lists every plugin across the registered marketplaces, with parse issues.
pub struct MarketplacePlugins {
    pub issues: Vec<String>,
    pub plugins: Vec<MarketplacePlugin>,
}

pub fn list_marketplace_plugins(home: &DripHome, file: &MarketplacesFile) -> MarketplacePlugins {
    let mut issues: Vec<String> = Vec::new();
    let mut plugins: Vec<MarketplacePlugin> = Vec::new();

    for record in &file.marketplaces {
        let parsed = parse_marketplace_repo(&marketplace_root_dir(home, record), &record.name);

        issues.extend(parsed.issues);
        plugins.extend(parsed.plugins);
    }

    MarketplacePlugins { issues, plugins }
}

// ---------------------------------------------------------------------------
// Enable/disable keys
// ---------------------------------------------------------------------------

pub fn is_marketplace_key_enabled(
    plugin_key: &str,
    item_key: &str,
    file: &MarketplacesFile,
    overrides: &ProjectPluginOverrides,
) -> bool {
    fn key_state(
        key: &str,
        file: &MarketplacesFile,
        overrides: &ProjectPluginOverrides,
    ) -> Option<bool> {
        if overrides.disabled.iter().any(|existing| existing == key) {
            return Some(false);
        }

        if overrides.enabled.iter().any(|existing| existing == key) {
            return Some(true);
        }

        if file.disabled.iter().any(|existing| existing == key) {
            return Some(false);
        }

        if file.enabled.iter().any(|existing| existing == key) {
            return Some(true);
        }

        None
    }

    key_state(item_key, file, overrides)
        .or_else(|| key_state(plugin_key, file, overrides))
        .unwrap_or(false)
}

// Flips a plugin or skill key in the user registry and persists the file.
pub fn set_marketplace_key_enabled(
    home: &crate::core::home::DripHome,
    key: &str,
    enabled: bool,
) -> anyhow::Result<MarketplacesFile> {
    let marketplaces_path = std::path::Path::new(&home.marketplaces_path);
    let file = load_marketplaces_file(marketplaces_path)?;
    let next_file = MarketplacesFile {
        disabled: if enabled {
            file.disabled
                .iter()
                .filter(|existing| *existing != key)
                .cloned()
                .collect()
        } else {
            // [...new Set([...file.disabled, key])] — append only when absent.
            let mut next = file.disabled.clone();
            if !next.iter().any(|existing| existing == key) {
                next.push(key.to_string());
            }
            next
        },
        enabled: if enabled {
            // [...new Set([...file.enabled, key])] — append only when absent.
            let mut next = file.enabled.clone();
            if !next.iter().any(|existing| existing == key) {
                next.push(key.to_string());
            }
            next
        } else {
            file.enabled
                .iter()
                .filter(|existing| *existing != key)
                .cloned()
                .collect()
        },
        marketplaces: file.marketplaces,
        version: 1,
    };

    save_marketplaces_file(marketplaces_path, &next_file)?;

    Ok(next_file)
}

pub fn list_enabled_marketplace_skills(
    cwd: &std::path::Path,
    home: &crate::core::home::DripHome,
) -> anyhow::Result<Vec<crate::cli::skills::CliSkill>> {
    let marketplaces_path = std::path::Path::new(&home.marketplaces_path);
    let file = load_marketplaces_file(marketplaces_path)?;

    if file.marketplaces.is_empty() {
        return Ok(Vec::new());
    }

    let overrides = load_project_plugin_overrides(cwd);
    let plugins = list_marketplace_plugins(home, &file).plugins;
    let mut skills: Vec<crate::cli::skills::CliSkill> = Vec::new();

    for plugin in &plugins {
        for skill in &plugin.skills {
            if is_marketplace_key_enabled(&plugin.key, &skill.key, &file, &overrides) {
                skills.push(crate::cli::skills::CliSkill {
                    description: skill.description.clone(),
                    key: Some(skill.key.clone()),
                    name: skill.name.clone(),
                    path: skill.path.clone(),
                    source: crate::cli::skills::SkillSource::Marketplace,
                });
            }
        }
    }

    Ok(skills)
}

pub fn list_enabled_marketplace_roles(
    cwd: &std::path::Path,
    home: &crate::core::home::DripHome,
) -> anyhow::Result<Vec<MarketplaceRoleEntry>> {
    let marketplaces_path = std::path::Path::new(&home.marketplaces_path);
    let file = load_marketplaces_file(marketplaces_path)?;

    if file.marketplaces.is_empty() {
        return Ok(Vec::new());
    }

    let overrides = load_project_plugin_overrides(cwd);
    let plugins = list_marketplace_plugins(home, &file).plugins;
    let mut roles: Vec<MarketplaceRoleEntry> = Vec::new();

    for plugin in &plugins {
        for role in &plugin.roles {
            if is_marketplace_key_enabled(&plugin.key, &role.key, &file, &overrides) {
                roles.push(role.clone());
            }
        }
    }

    Ok(roles)
}

// Convenience for the app: the full skill pool with marketplace skills merged
// in under the usual precedence (project > user > marketplace).
pub fn discover_all_skills(
    cwd: &std::path::Path,
    home: &crate::core::home::DripHome,
) -> anyhow::Result<Vec<crate::cli::skills::CliSkill>> {
    let marketplace_skills = list_enabled_marketplace_skills(cwd, home)?;

    Ok(crate::cli::skills::discover_skills(
        cwd,
        std::path::Path::new(&home.skills_dir),
        Some(marketplace_skills),
        None,
    ))
}

pub fn discover_all_skills_with_issues(
    cwd: &std::path::Path,
    home: &crate::core::home::DripHome,
) -> anyhow::Result<crate::cli::skills::DiscoverSkillsResult> {
    let marketplace_skills = list_enabled_marketplace_skills(cwd, home)?;

    Ok(crate::cli::skills::discover_skills_with_issues(
        cwd,
        std::path::Path::new(&home.skills_dir),
        Some(marketplace_skills),
        None,
    ))
}

#[cfg(test)]
mod registry_tests {
    use super::*;

    fn make_home(root: &std::path::Path) -> crate::core::home::DripHome {
        crate::core::home::open_drip_home(root.to_string_lossy().as_ref())
    }

    fn empty_file() -> MarketplacesFile {
        MarketplacesFile {
            disabled: Vec::new(),
            enabled: Vec::new(),
            marketplaces: Vec::new(),
            version: 1,
        }
    }

    fn save_registry(home: &crate::core::home::DripHome, file: &MarketplacesFile) {
        save_marketplaces_file(std::path::Path::new(&home.marketplaces_path), file)
            .expect("save marketplaces file");
    }

    fn write_local_source(root: &std::path::Path, name: &str) -> std::path::PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).expect("create local marketplace dir");
        dir
    }

    #[test]
    fn adds_a_local_marketplace_and_rejects_duplicate_names() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = make_home(temp.path());
        save_registry(&home, &empty_file());
        let source = write_local_source(temp.path(), "skills-src");

        let fixed = chrono::TimeZone::with_ymd_and_hms(&chrono::Utc, 2026, 1, 2, 3, 4, 5).unwrap();
        let closure = move || fixed;
        let now_fn: &dyn Fn() -> chrono::DateTime<chrono::Utc> = &closure;

        let added = add_marketplace(AddMarketplaceArgs {
            git: None,
            home: &home,
            name: None,
            now: Some(now_fn),
            source: source.to_string_lossy().as_ref(),
        })
        .expect("add local marketplace");

        assert_eq!(added.record.kind, "local");
        assert_eq!(added.record.name, "skills-src");
        assert_eq!(added.record.source, source.to_string_lossy().as_ref());
        assert_eq!(added.record.added_at, "2026-01-02T03:04:05.000Z");
        assert_eq!(added.file.marketplaces.len(), 1);

        let duplicate = add_marketplace(AddMarketplaceArgs {
            git: None,
            home: &home,
            name: Some("skills-src"),
            now: None,
            source: source.to_string_lossy().as_ref(),
        });

        let message = format!("{}", duplicate.expect_err("duplicate name must fail"));
        assert_eq!(
            message,
            "A marketplace named \"skills-src\" is already registered. Remove it first or pass a different name."
        );
    }

    #[test]
    fn adds_a_git_marketplace_by_cloning_through_the_injected_runner() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = make_home(temp.path());
        save_registry(&home, &empty_file());
        let source = "https://example.com/acme/skills.git";

        let recorded = std::cell::RefCell::new(Vec::new());
        let recorded_ref = &recorded;
        let fake_git = move |args: &[String], cwd: Option<&Path>| -> anyhow::Result<()> {
            recorded_ref
                .borrow_mut()
                .push((args.to_vec(), cwd.map(|path| path.to_path_buf())));
            Ok(())
        };

        let added = add_marketplace(AddMarketplaceArgs {
            git: Some(&fake_git),
            home: &home,
            name: None,
            now: None,
            source,
        })
        .expect("add git marketplace");

        assert_eq!(added.record.kind, "git");
        assert_eq!(added.record.name, "skills");

        let clone_dir = marketplace_clone_dir(&home, "skills");
        assert_eq!(
            *recorded.borrow(),
            vec![(
                vec![
                    "clone".to_string(),
                    "--depth".to_string(),
                    "1".to_string(),
                    source.to_string(),
                    clone_dir.to_string_lossy().to_string(),
                ],
                None::<std::path::PathBuf>,
            )]
        );
    }

    #[test]
    fn removes_a_marketplace_and_its_enabled_disabled_keys() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = make_home(temp.path());

        let mut file = empty_file();
        file.marketplaces.push(MarketplaceRecord {
            added_at: "2026-01-01T00:00:00.000Z".to_string(),
            kind: "git".to_string(),
            name: "acme".to_string(),
            source: "https://example.com/acme/skills.git".to_string(),
        });
        file.enabled = vec!["acme/skills/deployer".to_string()];
        file.disabled = vec!["acme/skills/legacy".to_string(), "other/skill".to_string()];
        save_registry(&home, &file);

        let next = remove_marketplace(&home, "acme").expect("remove marketplace");

        assert!(next.marketplaces.is_empty());
        assert!(next.enabled.is_empty());
        assert_eq!(next.disabled, vec!["other/skill".to_string()]);
        assert_eq!(next.version, 1);

        let missing = remove_marketplace(&home, "acme");
        let message = format!("{}", missing.expect_err("second remove must fail"));
        assert_eq!(message, "No marketplace named \"acme\" is registered.");
    }

    #[test]
    fn updates_git_marketplaces_only_and_errors_on_unknown_names() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = make_home(temp.path());

        let mut file = empty_file();
        file.marketplaces.push(MarketplaceRecord {
            added_at: "2026-01-01T00:00:00.000Z".to_string(),
            kind: "local".to_string(),
            name: "local-one".to_string(),
            source: temp.path().join("local-one").to_string_lossy().to_string(),
        });
        file.marketplaces.push(MarketplaceRecord {
            added_at: "2026-01-01T00:00:00.000Z".to_string(),
            kind: "git".to_string(),
            name: "acme".to_string(),
            source: "https://example.com/acme/skills.git".to_string(),
        });
        save_registry(&home, &file);

        let recorded = std::cell::RefCell::new(Vec::new());
        let recorded_ref = &recorded;
        let fake_git = move |args: &[String], cwd: Option<&Path>| -> anyhow::Result<()> {
            recorded_ref
                .borrow_mut()
                .push((args.to_vec(), cwd.map(|path| path.to_path_buf())));
            Ok(())
        };

        let updated = update_marketplaces(Some(&fake_git), &home, None).expect("update all");
        assert_eq!(updated, vec!["acme".to_string()]);
        assert_eq!(
            *recorded.borrow(),
            vec![(
                vec!["pull".to_string(), "--ff-only".to_string()],
                Some(marketplace_clone_dir(&home, "acme")),
            )]
        );

        let unknown = update_marketplaces(Some(&fake_git), &home, Some("missing"));
        let message = format!("{}", unknown.expect_err("unknown name must fail"));
        assert_eq!(message, "No marketplace named \"missing\" is registered.");
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod enabled_tests;
