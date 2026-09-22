//! Cached per-skill capability requirements for the optional skill classifier.
//!
//! Whether following a skill needs a tool (or an MCP server) is a property of
//! the skill, not of the loop, so asking the Decisions API once per skill and
//! caching the answer makes the per-loop relevance pass cheap. The cache lives
//! at `<drip home root>/skill-requirements.sqlite` and is keyed by
//! `(skill body hash, capability hash)`: editing a skill's markdown or a
//! capability's descriptor re-asks only for that pair, and deleting the file
//! costs at most one extra classification pass.
//!
//! Nothing here is fatal. A skill whose requirements could not be verified is
//! reported `known = false` and treated as satisfiable by everything — the
//! classifier being down must never hide a skill from the loop.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use tokio::task::JoinSet;

use crate::cli::skills::CliSkill;
use crate::harness::classifier::{answer_value, ask, ClassifierRoute, DeclaredRequirements};

/// A cached capability counts as required at or above this probability.
pub const REQUIREMENT_PROBABILITY_THRESHOLD: f64 = 0.7;

/// The cache schema. Kept in one place so the schema and the statements agree.
const CREATE_TABLE: &str = "CREATE TABLE IF NOT EXISTS skill_capabilities (\n            skill_hash TEXT NOT NULL,\n            capability_hash TEXT NOT NULL,\n            kind TEXT NOT NULL,\n            name TEXT NOT NULL,\n            probability REAL NOT NULL,\n            model TEXT NOT NULL,\n            classified_at TEXT NOT NULL,\n            PRIMARY KEY (skill_hash, capability_hash)\n        );";

/// sha256 hex of a skill's markdown body: the cache key for "this skill's
/// requirements".
pub fn skill_body_hash(markdown: &str) -> String {
    sha256_hex(markdown)
}

/// sha256 hex of a capability descriptor.
pub fn capability_hash(descriptor: &str) -> String {
    sha256_hex(descriptor)
}

fn sha256_hex(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// One thing a skill might need: a workspace tool, or an MCP server (with the
/// tool names that server exposes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Capability {
    Tool {
        name: String,
        description: String,
    },
    McpServer {
        name: String,
        tool_names: Vec<String>,
    },
}

impl Capability {
    /// The text hashed into the cache key. Editing any part of it (a tool's
    /// description, an MCP server's tool list) makes a NEW key, so an edited
    /// capability never inherits an older answer.
    pub fn descriptor(&self) -> String {
        match self {
            Capability::Tool { name, description } => format!("tool:{name}\n{description}"),
            Capability::McpServer { name, tool_names } => {
                let mut names: Vec<String> = tool_names.clone();
                names.sort();
                names.dedup();
                format!("mcp:{name}\n{}", names.join("\n"))
            }
        }
    }

    /// The name a skill's requirements are expressed in: the tool name, or the
    /// MCP server name.
    pub fn key_name(&self) -> String {
        match self {
            Capability::Tool { name, .. } => name.clone(),
            Capability::McpServer { name, .. } => name.clone(),
        }
    }

    fn kind_str(&self) -> &'static str {
        match self {
            Capability::Tool { .. } => "tool",
            Capability::McpServer { .. } => "mcp",
        }
    }
}

/// What one skill needs to be usable, and whether that is actually known.
/// `known == false` means the classifier could not answer: the caller must not
/// filter the skill out on requirements it never learned.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillRequirements {
    /// Capability key names with probability >= 0.7.
    pub required: BTreeSet<String>,
    pub known: bool,
}

// ---------------------------------------------------------------------------
// Cache
// ---------------------------------------------------------------------------

fn open_db(path: &Path) -> Result<Connection, String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|error| {
                format!(
                    "skill requirements: could not create {}: {error}",
                    parent.display()
                )
            })?;
        }
    }

    let conn = Connection::open(path).map_err(|error| {
        format!(
            "skill requirements: could not open {}: {error}",
            path.display()
        )
    })?;

    conn.execute_batch("PRAGMA journal_mode = WAL;")
        .map_err(|error| format!("skill requirements: could not enable WAL: {error}"))?;
    conn.execute_batch("PRAGMA busy_timeout = 5000;")
        .map_err(|error| format!("skill requirements: could not set the busy timeout: {error}"))?;
    conn.execute_batch(CREATE_TABLE).map_err(|error| {
        format!("skill requirements: could not create the cache table: {error}")
    })?;

    Ok(conn)
}

/// Which of these capability hashes already have a usable cached answer for the
/// skill. A non-finite cached probability counts as missing: it is re-asked.
fn cached_hashes<'a>(
    conn: &Connection,
    skill_hash: &str,
    capability_hashes: impl Iterator<Item = &'a str>,
) -> Result<BTreeSet<String>, String> {
    let mut statement = conn
        .prepare("SELECT probability FROM skill_capabilities WHERE skill_hash = ?1 AND capability_hash = ?2")
        .map_err(|error| format!("skill requirements: could not read the cache: {error}"))?;
    let mut found: BTreeSet<String> = BTreeSet::new();

    for hash in capability_hashes {
        let probability: Option<f64> = statement
            .query_row(params![skill_hash, hash], |row| row.get(0))
            .optional()
            .map_err(|error| format!("skill requirements: could not read the cache: {error}"))?;

        if probability.is_some_and(f64::is_finite) {
            found.insert(hash.to_string());
        }
    }

    Ok(found)
}

/// The requirements already on record for one skill, restricted to the
/// capabilities the caller currently has: a stale descriptor's row can never
/// leak a requirement into a run that no longer has that capability shape.
fn cached_required(
    conn: &Connection,
    skill_hash: &str,
    current: &BTreeMap<String, String>,
) -> Result<BTreeSet<String>, String> {
    let mut statement = conn
        .prepare(
            "SELECT capability_hash, probability FROM skill_capabilities WHERE skill_hash = ?1",
        )
        .map_err(|error| format!("skill requirements: could not read the cache: {error}"))?;
    let rows = statement
        .query_map(params![skill_hash], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
        })
        .map_err(|error| format!("skill requirements: could not read the cache: {error}"))?;
    let mut required: BTreeSet<String> = BTreeSet::new();

    for row in rows {
        let (hash, probability) =
            row.map_err(|error| format!("skill requirements: could not read the cache: {error}"))?;

        if let Some(name) = current.get(&hash) {
            if probability.is_finite() && probability >= REQUIREMENT_PROBABILITY_THRESHOLD {
                required.insert(name.clone());
            }
        }
    }

    Ok(required)
}

fn insert_rows(
    conn: &Connection,
    skill_hash: &str,
    model: &str,
    rows: &[(String, String, String, f64)],
) -> Result<(), String> {
    let classified_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

    for (capability_hash, kind, name, probability) in rows {
        conn.execute(
            "INSERT OR REPLACE INTO skill_capabilities (skill_hash, capability_hash, kind, name, probability, model, classified_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![skill_hash, capability_hash, kind, name, probability, model, classified_at],
        )
        .map_err(|error| format!("skill requirements: could not cache a row: {error}"))?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// The pass
// ---------------------------------------------------------------------------

fn capability_question(capability: &Capability) -> Value {
    match capability {
        Capability::Tool { name, description } => json!({
            "kind": "tool",
            "name": name,
            "description": description,
        }),
        Capability::McpServer { name, tool_names } => {
            let mut names: Vec<String> = tool_names.clone();
            names.sort();
            names.dedup();
            json!({
                "kind": "mcpServer",
                "name": name,
                "tools": names,
            })
        }
    }
}

/// ONE decisions request for one skill: a noul per capability the cache does
/// not know yet. The state is the skill markdown (never a repository file or a
/// transcript).
async fn ask_requirements(
    route: &ClassifierRoute,
    markdown: String,
    missing: Vec<(Capability, String)>,
) -> Result<Vec<(String, String, String, f64)>, String> {
    let mut questions: Map<String, Value> = Map::new();

    for (capability, hash) in &missing {
        questions.insert(
            hash.clone(),
            json!({
                "type": "noul",
                "instructions": {
                    "question": "Does following this skill require the capability `capability` to be available?",
                    "capability": capability_question(capability),
                }
            }),
        );
    }

    let response = ask(route, Value::String(markdown), questions).await?;
    let mut rows: Vec<(String, String, String, f64)> = Vec::new();

    for (capability, hash) in &missing {
        let answer = response.answers.get(hash).ok_or_else(|| {
            format!(
                "the response carried no answer for capability \"{}\"",
                capability.key_name()
            )
        })?;
        let value = answer_value(answer);

        if !value.is_finite() {
            return Err(format!(
                "the response gave a non-finite probability for capability \"{}\"",
                capability.key_name()
            ));
        }

        rows.push((
            hash.clone(),
            capability.kind_str().to_string(),
            capability.key_name(),
            value.clamp(0.0, 1.0),
        ));
    }

    Ok(rows)
}

struct PendingSkill {
    index: usize,
    name: String,
    hash: String,
    markdown: String,
    missing: Vec<(Capability, String)>,
}

/// Resolves every skill's capability requirements, asking the classifier only
/// for the (skill, capability) pairs the cache does not already hold.
///
/// - A skill with `DeclaredRequirements` needs no classifier call and writes no
///   cache row: the author stated the answer.
/// - Skills that do need work get ONE request each, concurrently, and the whole
///   pass is bounded by the route's timeout.
/// - A failed request leaves that skill `known = false` (never hidden from the
///   loop) and adds a warning.
///
/// Returns (requirements by skill name, warnings).
pub async fn ensure_requirements(
    db_path: &Path,
    route: &ClassifierRoute,
    skills: &[(CliSkill, String, Option<DeclaredRequirements>)],
    capabilities: &[Capability],
) -> (BTreeMap<String, SkillRequirements>, Vec<String>) {
    let mut results: BTreeMap<String, SkillRequirements> = BTreeMap::new();
    let mut warnings: Vec<String> = Vec::new();

    // Current capabilities, hashed once: the cache is keyed by descriptor hash.
    let cap_entries: Vec<(Capability, String)> = capabilities
        .iter()
        .map(|capability| {
            (
                capability.clone(),
                capability_hash(&capability.descriptor()),
            )
        })
        .collect();
    let current: BTreeMap<String, String> = cap_entries
        .iter()
        .map(|(capability, hash)| (hash.clone(), capability.key_name()))
        .collect();

    let conn = match open_db(db_path) {
        Ok(conn) => conn,
        Err(error) => {
            warnings.push(error);

            for (skill, _markdown, declared) in skills {
                if declared.is_none() {
                    results.insert(
                        skill.name.clone(),
                        SkillRequirements {
                            required: BTreeSet::new(),
                            known: false,
                        },
                    );
                }
            }

            return (results, warnings);
        }
    };

    let mut pending: Vec<PendingSkill> = Vec::new();

    for (index, (skill, markdown, declared)) in skills.iter().enumerate() {
        if let Some(declared) = declared {
            let mut required: BTreeSet<String> = BTreeSet::new();
            required.extend(declared.tools.iter().cloned());
            required.extend(declared.mcp_servers.iter().cloned());
            results.insert(
                skill.name.clone(),
                SkillRequirements {
                    required,
                    known: true,
                },
            );
            continue;
        }

        let hash = skill_body_hash(markdown);
        let cached = match cached_hashes(
            &conn,
            &hash,
            cap_entries.iter().map(|(_, hash)| hash.as_str()),
        ) {
            Ok(cached) => cached,
            Err(error) => {
                warnings.push(error);
                BTreeSet::new()
            }
        };
        let missing: Vec<(Capability, String)> = cap_entries
            .iter()
            .filter(|(_, capability_hash)| !cached.contains(capability_hash))
            .cloned()
            .collect();

        pending.push(PendingSkill {
            index,
            name: skill.name.clone(),
            hash,
            markdown: markdown.clone(),
            missing,
        });
    }

    let mut answers: BTreeMap<usize, Result<Vec<(String, String, String, f64)>, String>> =
        BTreeMap::new();

    if pending.iter().any(|entry| !entry.missing.is_empty()) {
        let mut set: JoinSet<(usize, Result<Vec<(String, String, String, f64)>, String>)> =
            JoinSet::new();

        for entry in pending.iter().filter(|entry| !entry.missing.is_empty()) {
            let route = route.clone();
            let markdown = entry.markdown.clone();
            let missing = entry.missing.clone();
            let index = entry.index;
            set.spawn(async move { (index, ask_requirements(&route, markdown, missing).await) });
        }

        let bound = Duration::from_millis(route.timeout_ms.max(1));
        let collected = tokio::time::timeout(bound, async move {
            let mut collected: Vec<(usize, Result<Vec<(String, String, String, f64)>, String>)> =
                Vec::new();

            while let Some(joined) = set.join_next().await {
                match joined {
                    Ok(entry) => collected.push(entry),
                    Err(error) => collected.push((
                        usize::MAX,
                        Err(format!("requirements task failed: {error}")),
                    )),
                }
            }

            collected
        })
        .await;

        match collected {
            Ok(entries) => {
                for (index, outcome) in entries {
                    if index != usize::MAX {
                        answers.insert(index, outcome);
                    }
                }
            }
            Err(_) => warnings.push(format!(
                "classifier: the skill requirements pass timed out after {}ms",
                route.timeout_ms
            )),
        }
    }

    for entry in &pending {
        let inline: Vec<(String, String, String, f64)> = match answers.remove(&entry.index) {
            Some(Ok(rows)) => {
                if let Err(error) = insert_rows(&conn, &entry.hash, &route.model, &rows) {
                    // The answers are still usable for this run even when the
                    // cache write failed; next run simply re-asks.
                    warnings.push(error);
                }

                rows
            }
            Some(Err(error)) => {
                warnings.push(format!(
                    "classifier: skill \"{}\" requirements could not be verified — treating every capability as satisfied: {error}",
                    entry.name
                ));
                results.insert(
                    entry.name.clone(),
                    SkillRequirements {
                        required: BTreeSet::new(),
                        known: false,
                    },
                );
                continue;
            }
            None if entry.missing.is_empty() => Vec::new(),
            None => {
                warnings.push(format!(
                    "classifier: skill \"{}\" requirements could not be verified — treating every capability as satisfied: the request did not complete",
                    entry.name
                ));
                results.insert(
                    entry.name.clone(),
                    SkillRequirements {
                        required: BTreeSet::new(),
                        known: false,
                    },
                );
                continue;
            }
        };

        let mut required = match cached_required(&conn, &entry.hash, &current) {
            Ok(set) => set,
            Err(error) => {
                warnings.push(error);
                BTreeSet::new()
            }
        };

        for (_, _, name, probability) in &inline {
            if *probability >= REQUIREMENT_PROBABILITY_THRESHOLD {
                required.insert(name.clone());
            }
        }

        results.insert(
            entry.name.clone(),
            SkillRequirements {
                required,
                known: true,
            },
        );
    }

    (results, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::skills::SkillSource;

    fn skill(name: &str) -> CliSkill {
        CliSkill {
            description: format!("{name} description"),
            key: None,
            name: name.to_string(),
            path: format!("/tmp/{name}/SKILL.md"),
            source: SkillSource::User,
        }
    }

    fn tool(name: &str, description: &str) -> Capability {
        Capability::Tool {
            name: name.to_string(),
            description: description.to_string(),
        }
    }

    fn mcp(name: &str, tools: &[&str]) -> Capability {
        Capability::McpServer {
            name: name.to_string(),
            tool_names: tools.iter().map(|tool| tool.to_string()).collect(),
        }
    }

    /// HTTP/1.1 mock over a std TcpListener: one canned response per connection,
    /// returning the request bodies it received.
    fn spawn_mock(responses: Vec<String>) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let mut bodies: Vec<String> = Vec::new();

            for body in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut data: Vec<u8> = Vec::new();
                let mut chunk = [0u8; 4096];
                let body_start = loop {
                    let read = std::io::Read::read(&mut stream, &mut chunk).unwrap_or(0);
                    assert!(read > 0, "client closed before sending a full request");
                    data.extend_from_slice(&chunk[..read]);

                    if let Some(pos) = data.windows(4).position(|window| window == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&data[..pos]).to_ascii_lowercase();
                        let length = head
                            .lines()
                            .find_map(|line| {
                                line.strip_prefix("content-length:")
                                    .and_then(|value| value.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);

                        if data.len() >= pos + 4 + length {
                            break pos + 4;
                        }
                    }
                };
                bodies.push(String::from_utf8_lossy(&data[body_start..]).to_string());
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                std::io::Write::write_all(&mut stream, response.as_bytes()).unwrap();
            }

            bodies
        });

        (format!("http://127.0.0.1:{port}"), handle)
    }

    fn hand_route(base: &str, timeout_ms: u64) -> ClassifierRoute {
        ClassifierRoute {
            url: format!("{base}/alpha/decisions"),
            model: "jev-test".to_string(),
            headers: Vec::new(),
            timeout_ms,
        }
    }

    fn noul_body(entries: &[(String, f64)]) -> String {
        let answers: Vec<String> = entries
            .iter()
            .map(|(id, value)| format!("\"{id}\":{{\"type\":\"noul\",\"noul\":{value}}}"))
            .collect();
        format!(
            "{{\"model\":\"jev-test\",\"answers\":{{{}}}}}",
            answers.join(",")
        )
    }

    fn temp_db() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("skill-requirements.sqlite");
        (dir, path)
    }

    #[test]
    fn hashes_are_stable_and_descriptor_sensitive() {
        assert_eq!(skill_body_hash("body"), skill_body_hash("body"));
        assert_ne!(skill_body_hash("body"), skill_body_hash("body "));
        assert_eq!(skill_body_hash("body").len(), 64);

        assert_eq!(capability_hash("a"), capability_hash("a"));
        assert_ne!(capability_hash("a"), capability_hash("b"));
        assert_eq!(
            mcp("files", &["b", "a", "a"]).descriptor(),
            "mcp:files\na\nb",
            "MCP descriptors sort and dedupe their tool names"
        );
        assert_eq!(
            tool("BASH", "run commands").descriptor(),
            "tool:BASH\nrun commands"
        );
        assert_eq!(tool("BASH", "run commands").key_name(), "BASH");
        assert_eq!(mcp("files", &["read"]).key_name(), "files");
    }

    #[tokio::test]
    async fn the_first_pass_asks_and_the_second_hits_the_cache() {
        let (_dir, db) = temp_db();
        let markdown = "# Skill\n\nneeds a shell";
        let capabilities = vec![tool("BASH", "run commands")];
        let hash = capability_hash(&capabilities[0].descriptor());
        let body = noul_body(&[(hash.clone(), 0.95)]);
        let (base, server) = spawn_mock(vec![body.clone()]);
        let route = hand_route(&base, 10_000);
        let skills = vec![(skill("migrate"), markdown.to_string(), None)];

        let (first, warnings) = ensure_requirements(&db, &route, &skills, &capabilities).await;

        assert!(warnings.is_empty(), "{warnings:?}");
        let requirements = first.get("migrate").unwrap();
        assert!(requirements.known);
        assert!(requirements.required.contains("BASH"));
        // A probability below the threshold is recorded but not required.
        assert_eq!(requirements.required.len(), 1);

        // Same body: every pair is cached, so the second pass asks nothing.
        let (second, warnings) = ensure_requirements(&db, &route, &skills, &capabilities).await;
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(second.get("migrate"), first.get("migrate"));

        let bodies = server.join().unwrap();
        assert_eq!(bodies.len(), 1, "a cache hit must make no request");
        let sent: Value = serde_json::from_str(&bodies[0]).unwrap();
        assert_eq!(sent["state"], serde_json::json!(markdown));
        assert_eq!(sent["questions"][&hash]["type"], serde_json::json!("noul"));
        assert_eq!(
            sent["questions"][&hash]["instructions"]["capability"]["kind"],
            serde_json::json!("tool")
        );
    }

    #[tokio::test]
    async fn a_changed_skill_body_re_asks() {
        let (_dir, db) = temp_db();
        let capabilities = vec![tool("BASH", "run commands")];
        let hash = capability_hash(&capabilities[0].descriptor());
        let (base, server) = spawn_mock(vec![
            noul_body(&[(hash.clone(), 0.9)]),
            noul_body(&[(hash.clone(), 0.2)]),
        ]);
        let route = hand_route(&base, 10_000);

        let (first, _) = ensure_requirements(
            &db,
            &route,
            &[(skill("migrate"), "body v1".to_string(), None)],
            &capabilities,
        )
        .await;
        assert!(first.get("migrate").unwrap().required.contains("BASH"));

        let (second, _) = ensure_requirements(
            &db,
            &route,
            &[(skill("migrate"), "body v2".to_string(), None)],
            &capabilities,
        )
        .await;

        assert!(
            second.get("migrate").unwrap().required.is_empty(),
            "the edited body must use its own answers, not the cached ones"
        );
        assert_eq!(server.join().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_capability_that_is_no_longer_current_never_leaks_a_requirement() {
        let (_dir, db) = temp_db();
        let capabilities = vec![tool("BASH", "run commands")];
        let hash = capability_hash(&capabilities[0].descriptor());
        let (base, server) = spawn_mock(vec![noul_body(&[(hash, 0.95)])]);
        let route = hand_route(&base, 500);

        let (first, _) = ensure_requirements(
            &db,
            &route,
            &[(skill("migrate"), "body".to_string(), None)],
            &capabilities,
        )
        .await;
        assert!(first.get("migrate").unwrap().required.contains("BASH"));

        // The run no longer has any capability: the cached row must not come
        // back as a requirement, and nothing needs asking.
        let (second, warnings) = ensure_requirements(
            &db,
            &route,
            &[(skill("migrate"), "body".to_string(), None)],
            &[],
        )
        .await;
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(second.get("migrate").unwrap().required.is_empty());
        assert!(second.get("migrate").unwrap().known);
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn declared_requirements_skip_the_classifier_and_the_cache() {
        let (_dir, db) = temp_db();
        let (base, server) = spawn_mock(Vec::new());
        let route = hand_route(&base, 500);
        let declared = DeclaredRequirements {
            tools: vec!["BASH".to_string(), "PATCH".to_string()],
            mcp_servers: vec!["files".to_string()],
        };

        let (requirements, warnings) = ensure_requirements(
            &db,
            &route,
            &[(skill("explicit"), "body".to_string(), Some(declared))],
            &[tool("BASH", "run commands")],
        )
        .await;

        assert!(warnings.is_empty(), "{warnings:?}");
        let entry = requirements.get("explicit").unwrap();
        assert!(entry.known);
        assert_eq!(
            entry.required.iter().cloned().collect::<Vec<_>>(),
            vec!["BASH".to_string(), "PATCH".to_string(), "files".to_string()]
        );
        assert_eq!(
            server.join().unwrap().len(),
            0,
            "no classifier call for declared requirements"
        );

        // Nothing was cached either: the same skill asked without the
        // declaration (and with no capabilities in this run) still resolves
        // from the cache-free path without hanging on the empty cache.
        let (base, _server) = spawn_mock(Vec::new());
        let route = hand_route(&base, 500);
        let (requirements, warnings) = ensure_requirements(
            &db,
            &route,
            &[(skill("explicit"), "body".to_string(), None)],
            &[],
        )
        .await;
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(requirements.get("explicit").unwrap().known);
    }

    #[tokio::test]
    async fn a_failed_request_leaves_the_skill_known_false() {
        let (_dir, db) = temp_db();
        let (base, server) = spawn_mock(vec!["this is not json at all".to_string()]);
        let route = hand_route(&base, 2_000);

        let (requirements, warnings) = ensure_requirements(
            &db,
            &route,
            &[(skill("migrate"), "body".to_string(), None)],
            &[tool("BASH", "run commands")],
        )
        .await;

        let entry = requirements.get("migrate").unwrap();
        assert!(
            !entry.known,
            "an unanswerable pass must be treated as satisfiable"
        );
        assert!(entry.required.is_empty());
        assert!(
            warnings.iter().any(|warning| warning.contains("migrate")),
            "a failed pass warns: {warnings:?}"
        );
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn mcp_capabilities_are_asked_about_by_server() {
        let (_dir, db) = temp_db();
        let capabilities = vec![mcp("files", &["read", "write"])];
        let hash = capability_hash(&capabilities[0].descriptor());
        let (base, server) = spawn_mock(vec![noul_body(&[(hash.clone(), 0.8)])]);
        let route = hand_route(&base, 10_000);

        let (requirements, warnings) = ensure_requirements(
            &db,
            &route,
            &[(skill("files-skill"), "body".to_string(), None)],
            &capabilities,
        )
        .await;

        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(requirements
            .get("files-skill")
            .unwrap()
            .required
            .contains("files"));

        let sent: Value = serde_json::from_str(&server.join().unwrap()[0]).unwrap();
        assert_eq!(
            sent["questions"][&hash]["instructions"]["capability"]["kind"],
            serde_json::json!("mcpServer")
        );
        assert_eq!(
            sent["questions"][&hash]["instructions"]["capability"]["tools"],
            serde_json::json!(["read", "write"])
        );
    }
}
