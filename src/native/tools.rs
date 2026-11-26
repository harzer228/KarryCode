use super::{Provider, RequestMode, permissions};
use serde_json::{Map, Value, json};

/// Name of the single tool used before per-action tools; it carries `action` in its arguments.
pub(super) const LEGACY_TOOL: &str = "project";

const ALL_ACTIONS: &[&str] = &[
    "read",
    "list",
    "search",
    "glob",
    "stat",
    "preview",
    "inspect_project",
    "git_status",
    "git_diff",
    "plan",
    "skill",
    "ask_user",
    "web_search",
    "web_fetch",
    "browser",
    "computer_observe",
    "computer_list",
    "computer_input",
    "edit",
    "create_file",
    "apply_changes",
    "request_permissions",
    "run",
    "run_task",
    "shell",
    "mcp",
    "delegate",
];

const ROOT_TOOLS: &[&str] = &[
    "read",
    "list",
    "search",
    "glob",
    "stat",
    "create_file",
    "apply_changes",
    "edit",
];

const TASKS: &[&str] = &[
    "cargo-test",
    "cargo-check",
    "cargo-build",
    "cargo-run",
    "cargo-fetch",
    "node-test",
    "npm-test",
    "npm-build",
    "npm-lint",
    "npm-start",
    "npm-dev",
    "npm-ci",
    "npm-install",
    "python-test",
    "python-run",
    "python-install",
];

/// Actions the model may request in this mode; the dispatch in `run` handles each of them.
pub(super) fn actions(mode: RequestMode) -> Vec<&'static str> {
    let RequestMode {
        diagnose,
        planning,
        worker,
        delegate_limit,
        desktop,
    } = mode;
    if worker {
        return permissions::WORKER_ACTIONS.to_vec();
    }
    if planning {
        return permissions::PLAN_ACTIONS.to_vec();
    }
    let mut actions = vec![
        "read",
        "list",
        "search",
        "glob",
        "stat",
        "preview",
        "inspect_project",
        "git_status",
        "git_diff",
        "plan",
        "skill",
        "ask_user",
        "web_search",
        "web_fetch",
        "browser",
    ];
    if desktop {
        actions.extend(["computer_observe", "computer_list"]);
    }
    if !diagnose {
        if desktop {
            actions.push("computer_input");
        }
        actions.extend([
            "edit",
            "create_file",
            "apply_changes",
            "request_permissions",
            "run",
            "run_task",
            "shell",
            "mcp",
        ]);
        if delegate_limit > 0 {
            actions.push("delegate");
        }
    }
    actions
}

/// `run` and `run_task` dispatch identically, so the model sees only `run_task`.
fn tool_name(action: &str) -> &str {
    if action == "run" { "run_task" } else { action }
}

/// Tool names exposed to the model in this mode.
pub(super) fn names(mode: RequestMode) -> Vec<&'static str> {
    let mut names: Vec<&'static str> = Vec::new();
    for action in actions(mode) {
        let name = tool_name(action);
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

pub(super) fn known(name: &str) -> bool {
    name.is_empty() || name == LEGACY_TOOL || ALL_ACTIONS.contains(&name)
}

fn object(properties: Value, required: &[&str]) -> Value {
    let mut schema = json!({"type":"object","properties":properties});
    if !required.is_empty() {
        schema["required"] = json!(required);
    }
    schema
}

fn string(description: &str) -> Value {
    json!({"type":"string","description":description})
}

fn definition(name: &str, mode: RequestMode) -> (String, Value) {
    let text = json!({"type":"string"});
    let integer = json!({"type":"integer"});
    let (description, mut schema) = match name {
        "read" => (
            "Read a project file; offset (0-based line) and limit select lines.",
            object(
                json!({"path":text,"offset":integer,"limit":integer}),
                &["path"],
            ),
        ),
        "list" => (
            "List a project folder (default: project root).",
            object(json!({"path":text}), &[]),
        ),
        "search" => (
            "Search text in project files. path: file or folder; pattern: glob filter; regex: query is a regex.",
            object(
                json!({"query":text,"path":text,"pattern":text,"regex":{"type":"boolean"}}),
                &["query"],
            ),
        ),
        "glob" => (
            "Find project files by glob, e.g. **/*.rs.",
            object(json!({"pattern":text,"path":text}), &["pattern"]),
        ),
        "stat" => (
            "Show file or folder type and size.",
            object(json!({"path":text}), &["path"]),
        ),
        "preview" => (
            "Start or stop the local web preview.",
            object(
                json!({"operation":{"type":"string","enum":["start","stop"]}}),
                &[],
            ),
        ),
        "inspect_project" => (
            "Summarize the project and read AGENTS.md.",
            object(json!({}), &[]),
        ),
        "git_status" => ("Show git status.", object(json!({}), &[])),
        "git_diff" => (
            "Show the git diff.",
            object(json!({"staged":{"type":"boolean"}}), &[]),
        ),
        "plan" => (
            "Record your step-by-step plan.",
            object(json!({"text":text}), &["text"]),
        ),
        "skill" => (
            "Load a skill by name, or its resource file by path.",
            object(json!({"name":text,"path":text}), &["name"]),
        ),
        "ask_user" => (
            "Ask the user a question, optionally with answer options.",
            object(
                json!({"text":text,"options":{"type":"array","items":text,"maxItems":5}}),
                &["text"],
            ),
        ),
        "web_search" => (
            "Search the internet. Keep queries free of private project data.",
            object(json!({"query":text}), &["query"]),
        ),
        "web_fetch" => (
            "Fetch a web page as text.",
            object(json!({"url":text}), &["url"]),
        ),
        "browser" => (
            "Control a headless browser. For click, type and fill, prefer a node ID from the latest snapshot, else a CSS selector. To check rendering, use a screenshot (if images are enabled) with the snapshot.",
            object(
                json!({
                    "operation":{"type":"string","enum":["navigate","snapshot","console","tabs","new_tab","select_tab","close_tab","click","type","fill","scroll","screenshot"]},
                    "url":text,"node":integer,"selector":text,"text":text,"tab":text,"delta":integer
                }),
                &["operation"],
            ),
        ),
        "computer_list" => (
            "List desktop windows and their IDs.",
            object(json!({}), &[]),
        ),
        "computer_observe" => (
            "Observe a desktop window as a tree or screenshot before computer_input.",
            object(
                json!({
                    "window":string("ID from computer_list"),
                    "operation":{"type":"string","enum":["tree","screenshot"]}
                }),
                &["window", "operation"],
            ),
        ),
        "computer_input" => (
            "Send input to the observed window by element index from the tree or window-relative x/y. Observe again after input.",
            object(
                json!({
                    "observation":{"type":"integer","description":"ID from computer_observe"},
                    "operation":{"type":"string","enum":["click","scroll","drag","type","key"]},
                    "window":text,"element":integer,"x":integer,"y":integer,"end_x":integer,"end_y":integer,
                    "delta":integer,"text":text,"key":text
                }),
                &["observation", "operation"],
            ),
        ),
        "edit" => (
            "Replace one exact text fragment in a file.",
            object(
                json!({"path":text,"before":text,"after":text,"reason":text}),
                &["path", "before", "after", "reason"],
            ),
        ),
        "create_file" => (
            "Create a file with the given content.",
            object(json!({"path":text,"content":text}), &["path", "content"]),
        ),
        "apply_changes" => (
            "Apply several file changes: create (content), edit (before, after), copy or move (to), delete.",
            object(
                json!({"changes":{"type":"array","minItems":1,"items":object(
                    json!({
                        "kind":{"type":"string","enum":["create","edit","copy","move","delete"]},
                        "path":text,"to":text,"content":text,"before":text,"after":text
                    }),
                    &["kind", "path"],
                )}}),
                &["changes"],
            ),
        ),
        "request_permissions" => (
            "Ask the user to change the permission mode; respect a refusal.",
            object(
                json!({"mode":{"type":"string","enum":["ask","auto","full"]},"reason":text}),
                &["mode", "reason"],
            ),
        ),
        "run_task" => (
            "Run a standard project task; omit task to list them.",
            object(
                json!({"task":{"type":"string","enum":TASKS},"path":text}),
                &[],
            ),
        ),
        "shell" => (
            "Run a shell command in the project.",
            object(
                json!({
                    "command":text,
                    "path":string("Working folder"),
                    "timeout":{"type":"integer","description":"Seconds, max 300"}
                }),
                &["command"],
            ),
        ),
        "mcp" => (
            "Call a tool of a connected MCP server.",
            object(
                json!({"server":text,"tool":text,"arguments":{"type":"object"}}),
                &["server", "tool"],
            ),
        ),
        "delegate" => {
            let limit = mode.delegate_limit;
            return (
                format!(
                    "Propose up to {limit} read-only subagents for parallel investigation; the user approves."
                ),
                object(
                    json!({"tasks":{"type":"array","minItems":1,"maxItems":limit,"items":object(
                        json!({
                            "role":{"type":"string","enum":["researcher","reviewer"]},
                            "task":{"type":"string","minLength":1,"maxLength":1000}
                        }),
                        &["role", "task"],
                    )}}),
                    &["tasks"],
                ),
            );
        }
        _ => unreachable!("tool without a definition: {name}"),
    };
    if ROOT_TOOLS.contains(&name) && !mode.planning && !mode.worker {
        schema["properties"]["root"] = json!({"type":"string"});
    }
    (description.to_owned(), schema)
}

/// The `tools` array of a request in the provider's wire format.
pub(super) fn request_tools(provider: &Provider, mode: RequestMode) -> Value {
    names(mode)
        .into_iter()
        .map(|name| {
            let (description, parameters) = definition(name, mode);
            if provider.responses() {
                json!({"type":"function","name":name,"description":description,"parameters":parameters,"strict":false})
            } else if provider.anthropic() {
                json!({"name":name,"description":description,"input_schema":parameters})
            } else {
                json!({"type":"function","function":{"name":name,"description":description,"parameters":parameters}})
            }
        })
        .collect()
}

/// Converts a native tool call into the action value handled by `run`.
/// Returns `None` when the arguments are not a JSON object.
pub(super) fn decode(name: &str, arguments: &str) -> Option<Value> {
    if name.is_empty() || name == LEGACY_TOOL {
        return super::parse_action(arguments);
    }
    let mut value = match arguments.trim() {
        "" => json!({}),
        text => serde_json::from_str(text).ok()?,
    };
    if value.is_null() {
        value = json!({});
    }
    let object = value.as_object_mut()?;
    if name == "edit" && !object.get("edit").is_some_and(Value::is_object) {
        let edit: Map<String, Value> = ["path", "before", "after", "reason"]
            .into_iter()
            .filter_map(|key| object.remove(key).map(|value| (key.to_owned(), value)))
            .collect();
        object.insert("edit".into(), Value::Object(edit));
    }
    object.insert("action".into(), json!(name));
    Some(value)
}

fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

/// Name and arguments to send back to the provider for a recorded call. Older sessions
/// recorded every call as `project` with `action` inside its arguments.
pub(super) fn replay(name: &str, arguments: &str) -> (String, Value) {
    let mut input = serde_json::from_str::<Value>(arguments)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    let Value::Object(object) = &mut input else {
        unreachable!()
    };
    let action = object
        .remove("action")
        .and_then(|action| action.as_str().map(str::to_owned));
    let name = match action.as_deref() {
        Some(action) if ALL_ACTIONS.contains(&action) => tool_name(action).to_owned(),
        _ if valid_name(name) && name != LEGACY_TOOL => name.to_owned(),
        Some(action) if valid_name(action) => action.to_owned(),
        _ => LEGACY_TOOL.to_owned(),
    };
    if name == "edit"
        && let Some(Value::Object(edit)) = object.remove("edit")
    {
        for (key, value) in edit {
            object.entry(key).or_insert(value);
        }
    }
    (name, input)
}

/// Error for a call whose tool name is not one of KarryCode's tools.
pub(super) fn unknown(action: &Value) -> Option<anyhow::Error> {
    let name = action["action"].as_str().unwrap_or_default();
    (!ALL_ACTIONS.contains(&name)).then(|| {
        anyhow::anyhow!(trf!(
            "Неизвестный инструмент «{}». Вызови один из доступных инструментов.",
            "Unknown tool '{}'. Call one of the provided tools.",
            name
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_action_has_a_tool_and_every_tool_decodes_to_an_allowed_action() {
        for mode in [
            RequestMode::default(),
            RequestMode {
                desktop: true,
                ..Default::default()
            },
            RequestMode {
                diagnose: true,
                desktop: true,
                ..Default::default()
            },
            RequestMode {
                planning: true,
                ..Default::default()
            },
            RequestMode {
                worker: true,
                ..Default::default()
            },
            RequestMode {
                delegate_limit: 0,
                ..Default::default()
            },
        ] {
            let actions = actions(mode);
            let names = names(mode);
            for action in &actions {
                assert!(ALL_ACTIONS.contains(action));
                assert!(names.contains(&tool_name(action)), "{action}");
            }
            for name in &names {
                let action = decode(name, "{}").unwrap();
                assert!(actions.contains(&action["action"].as_str().unwrap()));
                let (description, schema) = definition(name, mode);
                assert!(!description.is_empty() && description.len() <= 260);
                assert_eq!(schema["type"], "object");
                for required in schema["required"].as_array().into_iter().flatten() {
                    assert!(
                        schema["properties"]
                            .get(required.as_str().unwrap())
                            .is_some()
                    );
                }
            }
        }
    }

    #[test]
    fn root_is_offered_exactly_where_tool_root_accepts_it() {
        let project = std::env::temp_dir();
        for name in names(RequestMode {
            desktop: true,
            ..Default::default()
        }) {
            let mut action = decode(name, "{}").unwrap();
            action["root"] = json!("relative");
            let error = permissions::tool_root(permissions::Mode::Full, &project, &action)
                .unwrap_err()
                .to_string();
            let accepted = error != "External folders are available for file tools";
            let offered = definition(name, RequestMode::default()).1["properties"]
                .get("root")
                .is_some();
            assert_eq!(accepted, offered, "{name}");
        }
    }

    #[test]
    fn edit_arguments_are_flat_on_the_wire_and_nested_for_dispatch() {
        let action = decode(
            "edit",
            r#"{"path":"a.rs","before":"x","after":"y","reason":"fix","root":"/srv/p"}"#,
        )
        .unwrap();
        assert_eq!(
            action,
            json!({"action":"edit","root":"/srv/p","edit":{"path":"a.rs","before":"x","after":"y","reason":"fix"}})
        );
        let (name, input) = replay("edit", &action.to_string());
        assert_eq!(name, "edit");
        assert_eq!(
            input,
            json!({"path":"a.rs","before":"x","after":"y","reason":"fix","root":"/srv/p"})
        );
        let legacy =
            r#"{"action":"edit","edit":{"path":"a.rs","before":"x","after":"y","reason":"fix"}}"#;
        assert_eq!(
            decode(LEGACY_TOOL, legacy).unwrap(),
            decode("edit", legacy).unwrap()
        );
    }

    #[test]
    fn legacy_and_unknown_calls_replay_under_valid_names() {
        assert_eq!(
            replay(LEGACY_TOOL, r#"{"action":"read","path":"README.md"}"#),
            ("read".into(), json!({"path":"README.md"}))
        );
        assert_eq!(
            replay(LEGACY_TOOL, r#"{"action":"run","task":"cargo-test"}"#),
            ("run_task".into(), json!({"task":"cargo-test"}))
        );
        assert_eq!(
            replay(LEGACY_TOOL, r#"{"action":"git status"}"#),
            (LEGACY_TOOL.into(), json!({}))
        );
        assert_eq!(
            replay("teleport", r#"{"action":"teleport","to":"moon"}"#),
            ("teleport".into(), json!({"to":"moon"}))
        );
        assert_eq!(replay("read", "not json"), ("read".into(), json!({})));
        let action = decode("teleport", r#"{"to":"moon"}"#).unwrap();
        assert!(unknown(&action).is_some());
        assert!(unknown(&decode("git_status", "").unwrap()).is_none());
        assert!(decode("read", "[1]").is_none());
        assert!(decode("read", r#"{"path":"#).is_none());
        assert!(known("run") && known(LEGACY_TOOL) && !known("teleport"));
    }
}
