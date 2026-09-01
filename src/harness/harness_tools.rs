// port of src/harness/harness-tools.ts
//
// This module ports the *definitions + argument parsing* half of
// harness-tools.ts: HARNESS_TOOL_SPECS (as harness_tool_definitions),
// isHarnessTool, HarnessRoleGate, DEFAULT_MAX_REVIEW_ROUNDS, and the pure
// argument extraction/validation of applyHarnessToolCall (parseToolInput +
// parsePlannedTasks + the per-branch field coercion) as parse_harness_op.
// The state-mutating op handlers (addTasks/dropTask/finishTask/... and their
// resultText strings) are ported in a follow-up chunk.
//
// Rename rule applied to user-visible text: `~/.lci` -> `~/.drip` (three
// description sites: remember, remember scope param, forget). Everything else
// (names, field names, enum values, required arrays) is verbatim.
//
// The definitions are byte-identical to the TS oracle fixture
// drip/tests/fixtures/harness-tools.json (dumped from
// src/harness/harness-tools.ts via drip/parity/tools/dump-harness-tools.ts,
// then renamed lci->drip) — see drip/tests/harness_tools_schema_parity.rs.

use std::collections::HashSet;

use serde_json::json;

/// port of DEFAULT_MAX_REVIEW_ROUNDS
pub const DEFAULT_MAX_REVIEW_ROUNDS: u32 = 2;

/// port of HarnessRoleGate
#[derive(Debug, Clone, Default)]
pub struct HarnessRoleGate {
    pub roles: HashSet<String>,
    pub max_review_rounds: Option<u32>,
}

/// port of HarnessTaskInput (the parsePlannedTasks entry shape)
#[derive(Debug, Clone, PartialEq)]
pub struct HarnessTaskInput {
    pub title: String,
    pub role: Option<String>,
    pub depends_on: Vec<String>,
}

/// port of finish_task's `status` argument ("completed" | "blocked")
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishTaskStatus {
    Completed,
    Blocked,
}

impl FinishTaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            FinishTaskStatus::Completed => "completed",
            FinishTaskStatus::Blocked => "blocked",
        }
    }
}

/// port of remember/forget's `scope` argument ("session" | "repo")
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryScope {
    Session,
    Repo,
}

/// One parsed harness tool call: one variant per harness tool, carrying the
/// arguments applyHarnessToolCall's matching branch would use. Field
/// extraction follows the TS branch code exactly (`typeof input.x ===
/// "string" ? input.x : <default>`); the state-dependent emptiness checks
/// ("Provide the note text.", ...) stay in the op handlers.
#[derive(Debug, Clone, PartialEq)]
pub enum HarnessOp {
    /// plan_tasks: entries from parsePlannedTasks, placement "next"|"end".
    PlanTasks {
        entries: Vec<HarnessTaskInput>,
        placement: String,
        /// Roles stripped because they are not in the role gate (reported back
        /// to the model by the handler).
        unknown_roles: Vec<String>,
    },
    /// drop_task
    DropTask { task_id: String, reason: String },
    /// revise_task
    ReviseTask { task_id: String, title: String },
    /// finish_task
    FinishTask {
        status: FinishTaskStatus,
        summary: String,
        task_id: Option<String>,
    },
    /// respond
    Respond { text: String },
    /// observe
    Observe { note: String, ttl: Option<f64> },
    /// recall
    Recall { tool_name: String, query: String },
    /// note_task
    NoteTask { note: String, task_id: Option<String> },
    /// remember
    Remember {
        scope: MemoryScope,
        note: String,
        topic: Option<String>,
        hook: Option<String>,
    },
    /// forget
    Forget { scope: MemoryScope, note_id: String },
}

/// port of buildHarnessToolSpecs() with no roleNames: the 10 harness (framework)
/// tool definitions as OpenAI function-call specs, in HARNESS_TOOL_SPECS order
/// (alphabetical by function name — the same order the fixture dumps them in).
pub fn harness_tool_definitions() -> Vec<serde_json::Value> {
    vec![
        json!({
            "type": "function",
            "function": {
                "name": "drop_task",
                "description": "Drop a task from the todo list when new information makes it unnecessary, redundant, or wrong. The task stays visible as dropped with your reason.",
                "parameters": {
                    "properties": {
                        "reason": {
                            "description": "Why this task is no longer needed.",
                            "type": "string"
                        },
                        "taskId": {
                            "type": "string"
                        }
                    },
                    "required": ["taskId", "reason"],
                    "type": "object"
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "finish_task",
                "description": "Finish the current task (or the task named by taskId). Use status completed with a summary of what was done, or status blocked with a summary of why and what is needed.",
                "parameters": {
                    "properties": {
                        "status": {
                            "enum": ["blocked", "completed"],
                            "type": "string"
                        },
                        "summary": {
                            "description": "What was done, or why the task is blocked and what would unblock it.",
                            "type": "string"
                        },
                        "taskId": {
                            "description": "Optional task id. Defaults to the current task.",
                            "type": "string"
                        }
                    },
                    "required": ["status", "summary"],
                    "type": "object"
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "forget",
                "description": "Remove a shared memory note by id when it is stale or wrong.\n\nOptional scope parameter:\n- 'session' (default): removes a note from in-memory state by noteId.\n- 'repo': removes a topic page from the project's memory bank stored under ~/.drip/projects/<project>/memory (outside the repo tree) by slug. Use when repo-scoped knowledge is no longer accurate.",
                "parameters": {
                    "properties": {
                        "noteId": {
                            "description": "For scope=session: the memory note id to remove. For scope=repo: the slug of the topic page to remove (e.g. 'build-commands').",
                            "type": "string"
                        },
                        "scope": {
                            "description": "Where to forget: 'session' (default) or 'repo'.",
                            "enum": ["session", "repo"],
                            "type": "string"
                        }
                    },
                    "required": ["noteId"],
                    "type": "object"
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "note_task",
                "description": "Append a progress note to a task (defaults to the current task) without ending the loop — partial findings, decisions, or where you left off. Use finish_task when the task is actually done or blocked.",
                "parameters": {
                    "properties": {
                        "note": {
                            "type": "string"
                        },
                        "taskId": {
                            "description": "Optional task id. Defaults to the current task.",
                            "type": "string"
                        }
                    },
                    "required": ["note"],
                    "type": "object"
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "observe",
                "description": "Save a short-lived observation for the next few task loops: a failing check's detail, an in-flight hypothesis, a result being verified. Observations expire after a few loops unless re-observed (which refreshes their ttl). Use remember instead for durable facts.",
                "parameters": {
                    "properties": {
                        "note": {
                            "type": "string"
                        },
                        "ttl": {
                            "description": "Optional number of task loops to keep the observation alive (default 4, capped).",
                            "type": "number"
                        }
                    },
                    "required": ["note"],
                    "type": "object"
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "plan_tasks",
                "description": "Add new tasks to the shared todo list. Use small, concrete tasks that a single task loop can finish.",
                "parameters": {
                    "properties": {
                        "placement": {
                            "description": "Where to insert the new tasks: \"next\" places them immediately after the current task, before other pending work; \"end\" (default) appends them.",
                            "enum": ["end", "next"],
                            "type": "string"
                        },
                        "tasks": {
                            "description": "Tasks to append to the todo list, in execution order.",
                            "items": {
                                "anyOf": [
                                    {
                                        "description": "A task title (worked in list order under the default role).",
                                        "type": "string"
                                    },
                                    {
                                        "additionalProperties": false,
                                        "properties": {
                                            "dependsOn": {
                                                "description": "Ids of tasks (e.g. task-2) that must complete or drop before this task becomes workable. Use for work that genuinely cannot start earlier — not for ordinary ordering, which list position already expresses.",
                                                "items": {
                                                    "type": "string"
                                                },
                                                "type": "array"
                                            },
                                            "title": {
                                                "type": "string"
                                            }
                                        },
                                        "required": ["title"],
                                        "type": "object"
                                    }
                                ]
                            },
                            "type": "array"
                        }
                    },
                    "required": ["tasks"],
                    "type": "object"
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "recall",
                "description": "Recover the cached output of an earlier tool call this run (telemetry keeps an ends-kept copy of each call's last result) instead of re-running it. Match by tool name plus a fragment of the original input.",
                "parameters": {
                    "properties": {
                        "query": {
                            "description": "A fragment of the original call's input (a path, a command substring) to pick the right record.",
                            "type": "string"
                        },
                        "toolName": {
                            "description": "The workspace tool whose earlier result you need (e.g. READ, BASH, GREP).",
                            "type": "string"
                        }
                    },
                    "required": ["toolName", "query"],
                    "type": "object"
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "remember",
                "description": "Save a note to shared memory so future task loops can see it. Use for durable facts, decisions, and discovered constraints that matter for the rest of the run; for short-lived findings use observe.\n\nOptional scope parameter:\n- 'session' (default): saves to in-memory state for this run only.\n- 'repo': persists the note into the project's memory bank stored under ~/.drip/projects/<project>/memory (outside the repo tree) so future sessions see it. Requires topic (a short title/slug for the page) and note. Optionally provide hook (one-line summary for the index). Use repo scope for durable, repo-specific knowledge: gotchas, key commands, architecture facts, persistent ideas.",
                "parameters": {
                    "properties": {
                        "note": {
                            "description": "The note text to save.",
                            "type": "string"
                        },
                        "scope": {
                            "description": "Where to save: 'session' (default, this run only) or 'repo' (persists to ~/.drip/projects/<project>/memory across sessions).",
                            "enum": ["session", "repo"],
                            "type": "string"
                        },
                        "topic": {
                            "description": "Required for scope=repo. Short title for the memory page (e.g. 'Build Commands', 'Architecture'). Used as the page filename slug.",
                            "type": "string"
                        },
                        "hook": {
                            "description": "Optional for scope=repo. One-line summary shown in the MEMORY.md index entry.",
                            "type": "string"
                        }
                    },
                    "required": ["note"],
                    "type": "object"
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "respond",
                "description": "Answer the goal directly and complete the run, when the goal is a question or asks for a status report and needs no workspace changes. The text is delivered to the user verbatim as the run's result. Not allowed while unfinished tasks exist.",
                "parameters": {
                    "properties": {
                        "text": {
                            "description": "The complete answer or report, in plain markdown.",
                            "type": "string"
                        }
                    },
                    "required": ["text"],
                    "type": "object"
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "revise_task",
                "description": "Rewrite a task's title when what you have learned changed what the task should actually do. Keeps its id, status, and notes.",
                "parameters": {
                    "properties": {
                        "taskId": {
                            "type": "string"
                        },
                        "title": {
                            "description": "The new task title.",
                            "type": "string"
                        }
                    },
                    "required": ["taskId", "title"],
                    "type": "object"
                }
            }
        }),
    ]
}

/// port of isHarnessTool
pub fn is_harness_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "drop_task"
            | "finish_task"
            | "forget"
            | "note_task"
            | "observe"
            | "plan_tasks"
            | "recall"
            | "remember"
            | "respond"
            | "revise_task"
    )
}

// --- argument parsing (parseToolInput + the per-branch coercion) ---

/// port of parseToolInput's wrapper text: a misleading missing-field error
/// sends weak models down the wrong repair path, so name the real problem.
fn not_valid_json(parse_error: &str) -> String {
    format!(
        "The tool arguments were not valid JSON ({parse_error}). Re-send the same call with well-formed JSON arguments."
    )
}

/// port of parseToolInput: JSON-parse the raw arguments and require an object.
/// Note: the embedded parse-error detail comes from the JSON runtime; serde's
/// messages differ from V8's, the wrapper text is identical.
fn parse_tool_input(raw_input: &str) -> Result<serde_json::Value, String> {
    match serde_json::from_str::<serde_json::Value>(raw_input) {
        Ok(parsed) if parsed.is_object() => Ok(parsed),
        Ok(_) => Err(not_valid_json("the arguments were not a JSON object")),
        Err(error) => Err(not_valid_json(&error.to_string())),
    }
}

/// port of the per-branch `typeof input.x === "string" ? input.x : default`
/// coercion.
fn string_or_default(input: &serde_json::Value, key: &str, default: &str) -> String {
    match input.get(key) {
        Some(v) if v.is_string() => v.as_str().unwrap_or(default).to_string(),
        _ => default.to_string(),
    }
}

/// port of the optional-string coercion (`typeof input.x === "string" ? input.x : undefined`).
fn string_or_none(input: &serde_json::Value, key: &str) -> Option<String> {
    match input.get(key) {
        Some(v) if v.is_string() => Some(v.as_str().unwrap_or_default().to_string()),
        _ => None,
    }
}

/// port of parsePlannedTasks: accepts both plan_tasks entry shapes — bare
/// titles, and {title, role, dependsOn} objects when roles are configured.
/// Unknown role names are stripped (the task still lands, under the default
/// role) and reported back to the model via `unknown_roles`.
fn parse_planned_tasks(
    raw_tasks: Option<&serde_json::Value>,
    gate: Option<&HarnessRoleGate>,
) -> (Vec<HarnessTaskInput>, Vec<String>) {
    let mut entries: Vec<HarnessTaskInput> = Vec::new();
    let mut unknown_roles: Vec<String> = Vec::new();

    let Some(raw_tasks) = raw_tasks.and_then(|v| v.as_array()) else {
        return (entries, unknown_roles);
    };

    for raw_task in raw_tasks {
        if let Some(title) = raw_task.as_str() {
            entries.push(HarnessTaskInput {
                title: title.to_string(),
                role: None,
                depends_on: Vec::new(),
            });
            continue;
        }

        let Some(task_object) = raw_task.as_object() else {
            continue;
        };
        let Some(title) = task_object.get("title").and_then(|v| v.as_str()) else {
            continue;
        };

        let role = task_object
            .get("role")
            .and_then(|v| v.as_str())
            .map(|role| role.trim().to_string())
            .unwrap_or_default();
        let depends_on: Vec<String> = task_object
            .get("dependsOn")
            .and_then(|v| v.as_array())
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| id.as_str())
                    .filter(|id| !id.trim().is_empty())
                    .map(|id| id.to_string())
                    .collect()
            })
            .unwrap_or_default();

        if !role.is_empty() && !gate.map(|gate| gate.roles.contains(&role)).unwrap_or(false) {
            unknown_roles.push(role);
            entries.push(HarnessTaskInput {
                title: title.to_string(),
                role: None,
                depends_on,
            });
        } else {
            entries.push(HarnessTaskInput {
                title: title.to_string(),
                role: if role.is_empty() { None } else { Some(role) },
                depends_on,
            });
        }
    }

    (entries, unknown_roles)
}

/// port of the remember/forget scope coercion ("session" default).
fn parse_scope(input: &serde_json::Value) -> MemoryScope {
    if string_or_default(input, "scope", "session") == "repo" {
        MemoryScope::Repo
    } else {
        MemoryScope::Session
    }
}

/// port of applyHarnessToolCall's argument handling: parse the raw JSON
/// arguments into a typed HarnessOp. This is the no-role-gate entry point —
/// plan_tasks role entries are stripped into `unknown_roles` exactly as the
/// TS does when no gate is configured. Handlers with a gate should call
/// [`parse_harness_op_with_gate`].
pub fn parse_harness_op(tool_name: &str, raw_input: &str) -> Result<HarnessOp, String> {
    parse_harness_op_with_gate(tool_name, raw_input, None)
}

/// parse_harness_op with the HarnessRoleGate the TS handler receives (it
/// shapes plan_tasks' entry parsing).
pub fn parse_harness_op_with_gate(
    tool_name: &str,
    raw_input: &str,
    gate: Option<&HarnessRoleGate>,
) -> Result<HarnessOp, String> {
    let input = parse_tool_input(raw_input)?;

    match tool_name {
        "plan_tasks" => {
            let (entries, unknown_roles) = parse_planned_tasks(input.get("tasks"), gate);
            let placement = if string_or_default(&input, "placement", "end") == "next" {
                "next".to_string()
            } else {
                "end".to_string()
            };
            Ok(HarnessOp::PlanTasks {
                entries,
                placement,
                unknown_roles,
            })
        }
        "drop_task" => Ok(HarnessOp::DropTask {
            task_id: string_or_default(&input, "taskId", ""),
            reason: string_or_default(&input, "reason", ""),
        }),
        "revise_task" => Ok(HarnessOp::ReviseTask {
            task_id: string_or_default(&input, "taskId", ""),
            title: string_or_default(&input, "title", ""),
        }),
        "finish_task" => {
            let status = match input.get("status").and_then(|v| v.as_str()) {
                Some("blocked") => FinishTaskStatus::Blocked,
                Some("completed") => FinishTaskStatus::Completed,
                _ => {
                    return Err("The status field must be exactly \"completed\" or \"blocked\".".to_string());
                }
            };
            Ok(HarnessOp::FinishTask {
                status,
                summary: string_or_default(&input, "summary", ""),
                task_id: string_or_none(&input, "taskId"),
            })
        }
        "respond" => Ok(HarnessOp::Respond {
            text: string_or_default(&input, "text", ""),
        }),
        "observe" => Ok(HarnessOp::Observe {
            note: string_or_default(&input, "note", ""),
            ttl: match input.get("ttl") {
                Some(v) if v.is_number() => v.as_f64(),
                _ => None,
            },
        }),
        "recall" => Ok(HarnessOp::Recall {
            tool_name: string_or_default(&input, "toolName", ""),
            query: string_or_default(&input, "query", ""),
        }),
        "note_task" => Ok(HarnessOp::NoteTask {
            note: string_or_default(&input, "note", ""),
            task_id: string_or_none(&input, "taskId"),
        }),
        "remember" => Ok(HarnessOp::Remember {
            scope: parse_scope(&input),
            note: string_or_default(&input, "note", ""),
            topic: string_or_none(&input, "topic"),
            hook: string_or_none(&input, "hook"),
        }),
        "forget" => Ok(HarnessOp::Forget {
            scope: parse_scope(&input),
            note_id: string_or_default(&input, "noteId", ""),
        }),
        _ => Err(format!("Unknown harness tool \"{tool_name}\".")),
    }
}
