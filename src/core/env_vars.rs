// port of src/cli/env-vars.ts
//
// Dotenv-style credential store at <home>/env.vars: KEY=value lines read
// before the process environment, owner-readable only (0600).

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::Result;

// [A-Za-z_][A-Za-z0-9_]* — JS ENV_VAR_NAME_PATTERN fullmatch, as a scanner.
fn is_env_var_name(key: &str) -> bool {
    let bytes = key.as_bytes();

    if bytes.is_empty() || !(bytes[0].is_ascii_alphabetic() || bytes[0] == b'_') {
        return false;
    }

    bytes[1..].iter().all(|b| b.is_ascii_alphanumeric() || *b == b'_')
}

pub const ENV_VARS_TEMPLATE: &str = "# lci credential store — KEY=value lines, read before the process environment.\n\
     # Model profiles reference these via \"apiKeyRef\": \"env:NAME\" in config.json,\n\
     # so tokens for every provider live here instead of shell profiles.\n\
     # Set values with /env KEY=value inside lci, or edit this file directly.\n\
     #\n\
     # Every shipped hosted profile routes through OpenRouter on this one key\n\
     # (https://openrouter.ai/settings/keys); bring your own vendor keys there.\n\
     # OPENROUTER_API_KEY=sk-or-...\n\
     #\n\
     # Only profiles you author against a vendor's own base URL need these:\n\
     # OPENAI_API_KEY=sk-...\n\
     # ANTHROPIC_API_KEY=sk-ant-...\n\
     # GEMINI_API_KEY=...\n\
     # XAI_API_KEY=xai-...\n\
     # CEREBRAS_API_KEY=csk-...\n\
     # ZAI_API_KEY=...            (Z.AI / GLM — https://z.ai/manage-apikey/apikey-list)\n";

// Insertion order of the file (TS objects preserve it; BTreeMap would sort).
// Ported tests assert template/parse ordering, so keep a Vec-backed map.
pub type Vars = Vec<(String, String)>;

pub fn get<'a>(vars: &'a Vars, key: &str) -> Option<&'a str> {
    vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

fn has_non_empty(vars: &Vars, key: &str) -> bool {
    get(vars, key).map_or(false, |v| !v.is_empty())
}

fn strip_export_prefix(line: &str) -> &str {
    match line.strip_prefix("export ") {
        Some(rest) => rest.trim(),
        None => line,
    }
}

// Parses dotenv-style content: KEY=value lines, blank lines and # comments
// ignored, optional `export ` prefix, matching single or double quotes
// stripped. Lines with invalid names or empty values are skipped so template
// placeholders never read as configured.
pub fn parse_env_vars(content: &str) -> Vars {
    let mut vars: Vars = Vec::new();

    for raw_line in content.split('\n') {
        let line = strip_export_prefix(raw_line.trim());

        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let separator_index = match line.find('=') {
            Some(idx) if idx > 0 => idx,
            _ => continue,
        };

        let key = line[..separator_index].trim();

        if !is_env_var_name(key) {
            continue;
        }

        let mut value = line[separator_index + 1..].trim().to_string();

        if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            value = value[1..value.len() - 1].to_string();
        }

        if !value.is_empty() {
            vars.push((key.to_string(), value));
        }
    }

    vars
}

pub fn load_env_vars(env_vars_path: &Path) -> Result<Vars> {
    if !env_vars_path.exists() {
        return Ok(Vec::new());
    }

    Ok(parse_env_vars(&fs::read_to_string(env_vars_path)?))
}

// The names (never the values) of the credentials this lci home manages, used
// to scrub them from tool subprocess environments via LCI_SCRUB_ENV.
pub fn list_env_var_names(env_vars_path: &Path) -> Result<Vec<String>> {
    Ok(load_env_vars(env_vars_path)?.into_iter().map(|(k, _)| k).collect())
}

// The env source used to resolve "env:NAME" credential references: values in
// the env.vars file win, the process environment is the fallback.
pub fn load_merged_env(
    env_vars_path: &Path,
    process_env: Option<&BTreeMap<String, String>>,
) -> BTreeMap<String, String> {
    // env-vars.ts:90 defaults processEnv to process.env: the file overlays
    // the live environment, it does not replace it.
    let mut merged = process_env.cloned().unwrap_or_else(|| std::env::vars().collect());

    for (k, v) in load_env_vars(env_vars_path).unwrap_or_default() {
        merged.insert(k, v);
    }

    merged
}

pub fn lookup_env_var_source(
    env_vars_path: &Path,
    name: &str,
    process_env: Option<&BTreeMap<String, String>>,
) -> &'static str {
    let file_vars = load_env_vars(env_vars_path).unwrap_or_default();

    if has_non_empty(&file_vars, name) {
        return "env-vars-file";
    }

    if let Some(env) = process_env {
        if let Some(value) = env.get(name) {
            if !value.trim().is_empty() {
                return "process-env";
            }
        }
    }

    "missing"
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvVarStatus {
    // Length + last-4 fingerprint: enough to verify an entry against a provider
    // dashboard without ever exposing the secret.
    pub last_four: String,
    pub length: usize,
    pub name: String,
    pub source: &'static str,
}

pub fn describe_env_var(
    env_vars_path: &Path,
    name: &str,
    process_env: Option<&BTreeMap<String, String>>,
) -> Result<EnvVarStatus> {
    let source = lookup_env_var_source(env_vars_path, name, process_env);

    let value = if source == "missing" {
        String::new()
    } else {
        load_merged_env(env_vars_path, process_env)
            .get(name)
            .map_or(String::new(), |v| v.trim().to_string())
    };

    let last_four: String = value
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();

    Ok(EnvVarStatus {
        last_four,
        length: value.chars().count(),
        name: name.to_string(),
        source,
    })
}

// Rewrites KEY's existing assignment in place (comments and ordering intact)
// or appends it. Creates the file if needed.
pub fn upsert_env_var(env_vars_path: &Path, key: &str, value: &str) -> Result<()> {
    if !is_env_var_name(key) {
        return Err(anyhow::anyhow!(
            "\"{}\" is not a valid environment variable name.",
            key
        ));
    }

    let existing = if env_vars_path.exists() {
        fs::read_to_string(env_vars_path)?
    } else {
        String::new()
    };

    let lines: Vec<&str> = if existing.is_empty() {
        Vec::new()
    } else {
        existing.split('\n').collect()
    };

    let assignment = format!("{}={}", key, value);
    let mut replaced = false;
    let mut next_lines: Vec<String> = Vec::new();

    for raw_line in &lines {
        let line = strip_export_prefix(raw_line.trim());
        let assignment_prefix = format!("{}=", key);

        if !replaced && !line.starts_with('#') && line.starts_with(&assignment_prefix) {
            replaced = true;
            next_lines.push(assignment.clone());
        } else {
            next_lines.push((*raw_line).to_string());
        }
    }

    if !replaced {
        while next_lines.last().map_or(false, |l| l.trim().is_empty()) {
            next_lines.pop();
        }

        next_lines.push(assignment);
    }

    if let Some(parent) = env_vars_path.parent() {
        fs::create_dir_all(parent)?;
    }

    write_private(
        env_vars_path,
        &format!("{}\n", next_lines.join("\n").trim_end_matches('\n')),
    )
}

// Seeds a commented template so the file is discoverable; tokens make it
// owner-readable only.
pub fn ensure_env_vars_file(env_vars_path: &Path) -> Result<()> {
    if env_vars_path.exists() {
        return Ok(());
    }

    if let Some(parent) = env_vars_path.parent() {
        fs::create_dir_all(parent)?;
    }

    write_private(env_vars_path, ENV_VARS_TEMPLATE)
}

// writeFileSync(..., { mode: 0o600 }) equivalent: create owner-only, and force
// the mode if a pre-existing file was wider.
fn write_private(path: &Path, contents: &str) -> Result<()> {
    fs::write(path, contents)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn parse_env_vars_reads_values_skips_comments_and_placeholders() {
        let vars = parse_env_vars(
            "# OPENROUTER_API_KEY=sk-or-...\n\nexport OPENAI_API_KEY=\"sk-...\"\nANTHROPIC_API_KEY='sk-ant-...'\nBAD-NAME=x\nEMPTY=\n=orphan\n",
        );

        assert_eq!(vars.len(), 2);
        assert_eq!(get(&vars, "OPENROUTER_API_KEY"), None);
        assert_eq!(get(&vars, "OPENAI_API_KEY"), Some("sk-..."));
        assert_eq!(get(&vars, "ANTHROPIC_API_KEY"), Some("sk-ant-..."));
    }

    #[test]
    fn merged_env_prefers_file_over_process() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("env.vars");
        std::fs::write(&path, "OPENAI_API_KEY=from-file\n").unwrap();

        let merged = load_merged_env(&path, Some(&env_map(&[("OPENAI_API_KEY", "from-env"), ("OTHER", "kept")])));
        assert_eq!(merged.get("OPENAI_API_KEY").unwrap(), "from-file");
        assert_eq!(merged.get("OTHER").unwrap(), "kept");
    }

    #[test]
    fn lookup_source_and_describe() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("env.vars");
        std::fs::write(&path, "TOKEN=abcdef\n").unwrap();

        assert_eq!(lookup_env_var_source(&path, "TOKEN", None), "env-vars-file");
        assert_eq!(
            lookup_env_var_source(&path, "OTHER", Some(&env_map(&[("OTHER", "  ")]))),
            "missing"
        );
        assert_eq!(
            lookup_env_var_source(&path, "OTHER", Some(&env_map(&[("OTHER", "env-value")]))),
            "process-env"
        );

        let status = describe_env_var(&path, "TOKEN", None).unwrap();
        assert_eq!(status.source, "env-vars-file");
        assert_eq!(status.length, 6);
        assert_eq!(status.last_four, "cdef");
    }

    #[test]
    fn upsert_replaces_in_place_appends_and_rejects_bad_names() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("env.vars");

        upsert_env_var(&path, "B_KEY", "second").unwrap();
        upsert_env_var(&path, "A_KEY", "first").unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents, "B_KEY=second\nA_KEY=first\n");

        // Replace in place, comments and ordering intact.
        upsert_env_var(&path, "B_KEY", "changed").unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents, "B_KEY=changed\nA_KEY=first\n");

        assert!(upsert_env_var(&path, "bad-name", "x").is_err());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn ensure_env_vars_file_seeds_template_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("env.vars");

        ensure_env_vars_file(&path).unwrap();
        let first = std::fs::read_to_string(&path).unwrap();
        assert_eq!(first, ENV_VARS_TEMPLATE);
        assert!(first.contains("# OPENROUTER_API_KEY=sk-or-..."));

        // The seeded template must parse as empty so placeholders never read
        // as configured credentials.
        assert!(parse_env_vars(&first).is_empty());

        std::fs::write(&path, "TOKEN=x\n").unwrap();
        ensure_env_vars_file(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "TOKEN=x\n");
    }
}
