use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
};
mod actions;
mod agent;
mod appearance;
mod browser;
mod changes;
mod commands;
mod compact;
mod desktop;
pub(crate) mod extensions;
mod files;
mod headless;
mod mcp;
mod model_config;
mod models;
mod onboarding;
mod permissions;
mod preview;
mod project;
mod project_tools;
mod prompts;
mod provider;
mod sandbox;
mod settings;
mod skills;
mod subagents;
mod tasks;
mod tools;
mod turn;
mod ui;
mod web;
mod work_plan;

use models::{Connection, Protocol, ReasoningLevel};
use permissions::DialogMode;

fn default_context() -> u32 {
    128_000
}

const DEFAULT_OUTPUT: u32 = 4096;

#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, Debug)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Reasoning {
    #[default]
    #[serde(rename = "default")]
    ProviderDefault,
    Off,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
    Ultra,
}

impl Reasoning {
    const LEVELS: [Self; 7] = [
        Self::Off,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::Xhigh,
        Self::Max,
        Self::Ultra,
    ];

    fn identifier(self) -> &'static str {
        self.level()
            .unwrap_or(if self == Self::Off { "off" } else { "default" })
    }

    fn level(self) -> Option<&'static str> {
        match self {
            Self::ProviderDefault | Self::Off => None,
            Self::Low => Some("low"),
            Self::Medium => Some("medium"),
            Self::High => Some("high"),
            Self::Xhigh => Some("xhigh"),
            Self::Max => Some("max"),
            Self::Ultra => Some("ultra"),
        }
    }

    fn budget(self) -> Option<u32> {
        match self {
            Self::ProviderDefault | Self::Off => None,
            Self::Low => Some(1024),
            Self::Medium => Some(2048),
            Self::High => Some(4096),
            Self::Xhigh | Self::Max | Self::Ultra => None,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::ProviderDefault => tr!("по умолчанию", "provider default"),
            Self::Off => tr!("выкл", "off"),
            Self::Low => tr!("низкий", "low"),
            Self::Medium => tr!("средний", "medium"),
            Self::High => tr!("высокий", "high"),
            Self::Xhigh => "xhigh",
            Self::Max => "max",
            Self::Ultra => "ultra",
        }
    }

    fn parse(input: &str) -> Option<Self> {
        match input.trim().to_lowercase().as_str() {
            "default" | "по умолчанию" => Some(Self::ProviderDefault),
            "off" | "0" | "выкл" => Some(Self::Off),
            "low" | "1" | "низкий" => Some(Self::Low),
            "medium" | "med" | "2" | "средний" => Some(Self::Medium),
            "high" | "3" | "высокий" => Some(Self::High),
            "xhigh" => Some(Self::Xhigh),
            "max" => Some(Self::Max),
            "ultra" => Some(Self::Ultra),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, Debug)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ReasoningApi {
    #[default]
    Auto,
    Effort,
    Object,
    Adaptive,
    DeepSeek,
    Custom,
    None,
}

impl ReasoningApi {
    pub(crate) const ALL: [Self; 7] = [
        Self::Auto,
        Self::Effort,
        Self::Object,
        Self::Adaptive,
        Self::DeepSeek,
        Self::Custom,
        Self::None,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Auto => tr!("авто", "auto"),
            Self::Effort => "reasoning_effort",
            Self::Object => "reasoning.effort",
            Self::Adaptive => "Anthropic adaptive",
            Self::DeepSeek => "DeepSeek thinking",
            Self::Custom => "reasoning_effort (custom)",
            Self::None => tr!("не отправлять", "do not send"),
        }
    }

    pub(crate) fn previous(self) -> Self {
        let index = Self::ALL.iter().position(|&item| item == self).unwrap_or(0);
        Self::ALL[(index + Self::ALL.len() - 1) % Self::ALL.len()]
    }

    pub(crate) fn next(self) -> Self {
        match self {
            Self::Auto => Self::Effort,
            Self::Effort => Self::Object,
            Self::Object => Self::Adaptive,
            Self::Adaptive => Self::DeepSeek,
            Self::DeepSeek => Self::Custom,
            Self::Custom => Self::None,
            Self::None => Self::Auto,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
struct Provider {
    connection: Connection,
    chat_only: bool,
    vision: bool,
    model: String,
    context: u32,
    input: Option<u32>,
    output: Option<u32>,
    reasoning: Reasoning,
    reasoning_api: ReasoningApi,
    reasoning_levels: Option<Vec<ReasoningLevel>>,
}

impl Provider {
    fn anthropic(&self) -> bool {
        self.connection.protocol == Protocol::AnthropicMessages
    }

    fn responses(&self) -> bool {
        self.connection.protocol == Protocol::Responses
    }

    fn context_limit(&self) -> u32 {
        self.context.max(1)
    }

    fn output_limit(&self) -> u32 {
        self.output
            .unwrap_or(DEFAULT_OUTPUT)
            .clamp(1, self.context_limit())
    }

    fn uses_deepseek_reasoning(&self) -> bool {
        self.reasoning_api == ReasoningApi::DeepSeek
            || (self.reasoning_api == ReasoningApi::Auto
                && self.connection.host_is("api.deepseek.com"))
    }

    fn input_limit(&self) -> u32 {
        let context = self.context_limit();
        self.input.unwrap_or(context).clamp(1, context)
    }
}
#[derive(Clone, Default, Serialize, Deserialize)]
struct Session {
    project: String,
    portable: bool,
    messages: Vec<Message>,
    #[serde(default)]
    mode: DialogMode,
    #[serde(default)]
    plan: work_plan::WorkPlan,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<model_config::Choice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    proposal: Option<String>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
struct Message {
    role: String,
    content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    display: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    receipt: Option<ui::Receipt>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    summary: bool,
}
#[derive(Serialize, Deserialize)]
struct State {
    version: u8,
    connections: Vec<Connection>,
    models: Vec<models::SavedModel>,
    active_model: Option<usize>,
    sessions: Vec<Session>,
    #[serde(default)]
    web: web::Settings,
    #[serde(default)]
    settings: settings::Settings,
}

impl Default for State {
    fn default() -> Self {
        Self {
            version: models::STATE_VERSION,
            connections: Vec::new(),
            models: Vec::new(),
            active_model: None,
            sessions: Vec::new(),
            web: web::Settings::default(),
            settings: settings::Settings::default(),
        }
    }
}

fn state_path(work: &Path) -> PathBuf {
    work.join("oc-state/karrycode-agent/state.json")
}
fn save(work: &Path, state: &State) -> Result<()> {
    let path = state_path(work);
    ensure!(
        serde_json::to_vec(state)?.len() <= 32 * 1024 * 1024,
        tr!(
            "История агента достигла 32 МиБ. Удали старые диалоги перед сохранением.",
            "Agent history reached 32 MiB. Remove old conversations before saving."
        )
    );
    fs::create_dir_all(path.parent().unwrap())?;
    let mut draft = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    serde_json::to_writer(draft.as_file_mut(), state)?;
    draft.as_file().sync_all()?;
    draft.persist(path)?;
    Ok(())
}
fn endpoint(input: &str) -> Result<String> {
    let url = url::Url::parse(input).context(tr!("Некорректный URL", "Invalid URL"))?;
    let host = url
        .host_str()
        .context(tr!("В URL отсутствует сервер", "URL needs a host"))?;
    ensure!(
        !input.contains('@') && !input.contains('?') && !input.contains('#'),
        tr!(
            "URL должен быть без логина, параметров и фрагмента",
            "URL must not contain credentials, query or fragment"
        )
    );
    ensure!(
        url.scheme() == "https"
            || (url.scheme() == "http" && matches!(host, "localhost" | "127.0.0.1" | "[::1]")),
        tr!(
            "Используй HTTPS или HTTP на localhost",
            "Use HTTPS, or HTTP on localhost"
        )
    );
    Ok(url.as_str().trim_end_matches('/').to_owned())
}
fn field<'a>(action: &'a Value, key: &str) -> &'a str {
    action[key].as_str().unwrap_or("")
}
fn parse_action(text: &str) -> Option<Value> {
    let text = text
        .trim()
        .strip_prefix("```json")
        .unwrap_or(text.trim())
        .trim()
        .strip_suffix("```")
        .unwrap_or(text.trim().strip_prefix("```json").unwrap_or(text.trim()))
        .trim();
    let value: Value = serde_json::from_str(text).ok()?;
    value.get("action")?.as_str()?;
    Some(value)
}
impl Message {
    fn for_provider(&self) -> bool {
        matches!(self.role.as_str(), "assistant" | "user")
    }
}

fn chat_error(history: &mut Vec<Message>, title: &str, error: impl std::fmt::Display) {
    history.push(Message {
        role: "error".into(),
        content: format!("{title}\n{error}"),
        reasoning: None,
        ..Default::default()
    });
}

use actions::review_changes;
pub use agent::run;
use commands::slash_command;
use files::paste_label;
use files::{attachment_choices, attachment_prefix, read_file, safe_path};
use provider::{
    Delta, InputLimitExceeded, InterruptedStream, OutputSpaceExceeded, Reply, RequestMode,
    RequestPlan, Visual, WireCalls, apply_reasoning, attach_image, available_output,
    constrain_request, input_estimate, prepare_work_request, prepare_work_request_with_extensions,
    probe_provider, provider_transport_error, request,
};
use subagents::{parse_subagent_tasks, run_subagent_group};

#[derive(Debug)]
pub(crate) struct StartupError(pub anyhow::Error);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Launch {
    Normal,
    BuiltinUi,
    Recovery,
}

impl Launch {
    pub(crate) fn recovery(self) -> bool {
        self == Self::Recovery
    }
}
impl std::fmt::Display for StartupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.0)
    }
}
impl std::error::Error for StartupError {}

#[cfg(test)]
use std::io::BufReader;
#[cfg(test)]
use {
    commands::{init_document, move_session},
    files::{attach_files, search_project},
    provider::http::{http_error_text, probe_payload},
    provider::{ToolCall, Usage, decode_stream_mode, prepare_request},
    provider::{request::build_request, stream::decode_stream},
    subagents::run_worker_read,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changing_directory_rekeys_only_the_current_session_and_keeps_history() -> Result<()> {
        let root = tempfile::tempdir()?;
        let first = root.path().join("Project A");
        let second = root.path().join("Project B");
        fs::create_dir(&first)?;
        fs::create_dir(&second)?;
        let selected = crate::folders::resolve_directory(&first, r#""../Project B""#)?;
        assert_eq!(selected, second.canonicalize()?);
        assert!(crate::folders::resolve_directory(&first, "missing").is_err());
        let mut sessions = vec![
            Session {
                project: "Project A".into(),
                portable: true,
                messages: vec![Message {
                    role: "user".into(),
                    content: "Keep this context".into(),
                    ..Default::default()
                }],
                mode: DialogMode::Chat,
                ..Default::default()
            },
            Session {
                project: "Project B".into(),
                portable: true,
                ..Default::default()
            },
        ];
        let index = move_session(&mut sessions, 0, &root.path().canonicalize()?, &selected);
        assert_eq!(index, 1);
        assert_eq!(sessions[index].project, "Project B");
        assert!(sessions[index].portable);
        assert_eq!(sessions[index].messages[0].content, "Keep this context");
        assert_eq!(sessions[index].mode, DialogMode::Chat);
        assert!(sessions[0].messages.is_empty());
        Ok(())
    }

    #[test]
    fn learned_limits_roundtrip_and_apply_only_to_the_selected_connection() -> Result<()> {
        let mut state = State::default();
        state.settings.provider_limits.insert(
            42,
            subagents::Limit {
                count: 1,
                estimated: true,
            },
        );
        let state = models::load_state(serde_json::to_vec(&state)?.as_slice())?;
        assert_eq!(state.settings.delegate_limit(42), 1);
        assert_eq!(state.settings.delegate_limit(43), 2);
        let tasks = json!(vec![json!({"role":"researcher","task":"Read project"}); 30]);
        assert!(parse_subagent_tasks(&tasks, 30).is_ok());
        assert!(parse_subagent_tasks(&tasks, state.settings.delegate_limit(42)).is_err());
        assert!(parse_subagent_tasks(&tasks, 0).is_err());
        let provider = provider_tests::provider("m", "", default_context());
        let delegate = |delegate_limit| {
            tools::request_tools(
                &provider,
                RequestMode {
                    delegate_limit,
                    ..Default::default()
                },
                &[],
            )
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["function"]["name"] == "delegate")
            .cloned()
        };
        assert_eq!(
            delegate(30).unwrap()["function"]["parameters"]["properties"]["tasks"]["maxItems"],
            30
        );
        assert!(delegate(0).is_none());
        Ok(())
    }
    #[test]
    fn delegate_requires_one_or_two_well_typed_tasks() {
        assert!(
            parse_subagent_tasks(&json!([{"role":"researcher","task":"Inspect save"}]), 2).is_ok()
        );
        assert!(
            parse_subagent_tasks(
                &json!([
                    {"role":"researcher","task":"Inspect save"},
                    {"role":"reviewer","task":"Review parsing"}
                ]),
                2
            )
            .is_ok()
        );
        for value in [
            json!([]),
            json!([{"role":"shell","task":"run"}]),
            json!([{"role":"reviewer","task":" "}]),
            json!([{}, {}, {}]),
        ] {
            assert!(parse_subagent_tasks(&value, 2).is_err());
        }
    }

    #[test]
    fn planning_schema_and_runtime_use_the_same_allowlist() {
        let mode = RequestMode {
            planning: true,
            ..Default::default()
        };
        assert_eq!(tools::names(mode), permissions::PLAN_ACTIONS);
        let provider = provider_tests::provider("m", "", default_context());
        for tool in tools::request_tools(&provider, mode, &[])
            .as_array()
            .unwrap()
        {
            let name = tool["function"]["name"].as_str().unwrap();
            assert!(
                tool["function"]["parameters"]["properties"]
                    .get("root")
                    .is_none()
            );
            assert!(permissions::plan_allows_action(
                &tools::decode(name, "{}").unwrap()
            ));
        }
        for name in [
            "shell",
            "run_task",
            "edit",
            "apply_changes",
            "mcp",
            "browser",
            "git_diff",
            "preview",
            "request_permissions",
            "unknown",
        ] {
            assert!(!permissions::plan_allows_action(&json!({"action": name})));
        }
        assert!(!permissions::plan_allows_action(
            &json!({"action": "read", "root": "/other"})
        ));
        for command in [
            "/init",
            "/project",
            "/undo",
            "/mcp",
            "/permissions",
            "/browser",
            "/search query",
            "/diagnose",
        ] {
            assert!(!permissions::plan_allows_command(command));
        }
        for command in [
            "/code",
            "/plan cancel",
            "/compact",
            "/exit",
            "Read the project",
        ] {
            assert!(permissions::plan_allows_command(command));
        }
    }

    #[test]
    fn worker_schema_exposes_only_local_read_actions() {
        let mode = RequestMode {
            worker: true,
            ..Default::default()
        };
        assert_eq!(tools::names(mode), permissions::WORKER_ACTIONS);
        let provider = provider_tests::provider("m", "", default_context());
        for tool in tools::request_tools(&provider, mode, &[])
            .as_array()
            .unwrap()
        {
            let properties = &tool["function"]["parameters"]["properties"];
            assert!(properties.get("tasks").is_none() && properties.get("root").is_none());
        }
        assert!(tools::names(RequestMode::default()).contains(&"delegate"));
        assert!(
            !tools::names(RequestMode {
                planning: true,
                ..Default::default()
            })
            .contains(&"delegate")
        );
    }

    #[test]
    fn subagent_read_dispatch_reuses_project_path_guards() -> Result<()> {
        let root = tempfile::tempdir()?;
        fs::create_dir(root.path().join("src"))?;
        fs::write(root.path().join("src/lib.rs"), "pub fn helper() {}\n")?;
        assert!(
            run_worker_read(root.path(), &json!({"action":"read","path":"src/lib.rs"}))?
                .contains("pub fn helper")
        );
        assert!(
            run_worker_read(root.path(), &json!({"action":"read","path":"../outside"})).is_err()
        );
        assert!(run_worker_read(root.path(), &json!({"action":"read","path":".env"})).is_err());
        assert!(
            run_worker_read(
                root.path(),
                &json!({"action":"shell","command":"echo nope"})
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn actions_require_complete_json() {
        assert!(parse_action("{\"action\":\"read\",\"path\":\"README.md\"}").is_some());
        assert!(parse_action("{\"action\":").is_none());
    }
    #[test]
    fn project_paths_reject_escape_and_secrets() -> Result<()> {
        let root = tempfile::tempdir()?;
        fs::write(root.path().join("readme.txt"), "hello")?;
        fs::write(root.path().join(".env"), "secret")?;
        assert_eq!(read_file(root.path(), "readme.txt")?, "hello");
        assert!(read_file(root.path(), "../outside").is_err());
        assert!(read_file(root.path(), ".env").is_err());
        Ok(())
    }

    #[test]
    fn attachment_completion_hides_credentials_and_expands_quoted_paths() -> Result<()> {
        let root = tempfile::tempdir()?;
        fs::create_dir(root.path().join("Some Folder"))?;
        fs::write(root.path().join("Some Folder/notes.txt"), "hello")?;
        fs::write(root.path().join(".env"), "secret")?;
        assert_eq!(attachment_choices(root.path(), "")[0].0, "@\"Some Folder/");
        assert_eq!(
            attachment_prefix("draft @\"Some Folder/n"),
            Some("\"Some Folder/n")
        );
        assert!(
            attach_files(root.path(), "Check @\"Some Folder/notes.txt\"", &[])?
                .0
                .contains(r#""content":"hello""#)
        );
        assert!(attach_files(root.path(), "Check @.env", &[]).is_err());
        Ok(())
    }

    #[test]
    fn searches_a_single_project_file_without_starting_the_shell() -> Result<()> {
        let root = tempfile::tempdir()?;
        fs::write(
            root.path().join("chat.html"),
            "ready\nPLACEHOLDER and задача\nnext\n",
        )?;
        fs::write(root.path().join(".env"), "PLACEHOLDER")?;
        let action = json!({"action":"search", "path":"chat.html", "query":r"(?i)placeholder|задача|var\(--\s","regex":true});
        let found = search_project(root.path(), &action)?;
        assert!(found.starts_with("chat.html:2 "), "{found}");
        assert!(
            search_project(
                root.path(),
                &json!({"action":"search","path":".env","query":"PLACEHOLDER","regex":true})
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn read_file_keeps_the_64_kib_limit() -> Result<()> {
        let root = tempfile::tempdir()?;
        fs::write(root.path().join("ok.txt"), vec![b'x'; 64 * 1024])?;
        fs::write(root.path().join("big.txt"), vec![b'x'; 64 * 1024 + 1])?;
        assert_eq!(read_file(root.path(), "ok.txt")?.len(), 64 * 1024);
        assert!(read_file(root.path(), "big.txt").is_err());
        Ok(())
    }

    #[test]
    fn init_writes_agents_md_once() -> Result<()> {
        let project = tempfile::tempdir()?;
        fs::write(
            project.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\n",
        )?;
        fs::create_dir(project.path().join("src"))?;
        let (path, content) = init_document(project.path())?.expect("document");
        assert_eq!(path, project.path().join("AGENTS.md"));
        assert!(content.contains("# AGENTS.md"));
        assert!(content.contains("cargo test --locked --workspace"));
        assert!(content.contains("`src/`"));
        fs::write(&path, &content)?;
        assert!(init_document(project.path())?.is_none());
        Ok(())
    }
}

#[cfg(test)]
mod provider_tests {
    use super::*;
    pub(super) fn provider(name: &str, key: &str, context: u32) -> Provider {
        Provider {
            connection: Connection {
                name: name.into(),
                url: "https://example.org/v1".into(),
                key: key.into(),
                ..Connection::default()
            },
            chat_only: false,
            vision: false,
            model: "test".into(),
            context,
            input: None,
            output: None,
            reasoning: Reasoning::Off,
            reasoning_api: ReasoningApi::Auto,
            reasoning_levels: None,
        }
    }

    fn decode(stream: &str, anthropic: bool) -> Result<Reply> {
        let (sender, _) = mpsc::channel::<Delta>();
        decode_stream(
            std::io::Cursor::new(stream),
            anthropic,
            &AtomicBool::new(false),
            sender,
        )
    }
    #[test]
    fn openai_stream_handles_text_and_rejects_disconnect() {
        let stream =
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\ndata: [DONE]\n\n";
        assert_eq!(decode(stream, false).unwrap().content, "Hello");
        assert!(decode(stream.split("data: [DONE]").next().unwrap(), false).is_err());
    }

    #[test]
    fn a_cut_event_or_reset_is_retryable_but_never_returns_partial_tool_calls() {
        struct Reset;
        impl Read for Reset {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof))
            }
        }
        let tool = json!({"choices":[{"delta":{"tool_calls":[{
            "index":0,"id":"one","function":{"name":"project","arguments":"{\"action\":\"create_file\""}
        }]}}]});
        let part = format!("data: {tool}\n\n");
        for tail in [
            "data: {\"choices\":[{\"delta\":{\"content\":\"cut",
            "data: [DONE]\n",
        ] {
            assert!(
                decode(&format!("{part}{tail}"), false)
                    .unwrap_err()
                    .is::<InterruptedStream>()
            );
        }
        let reader = BufReader::new(std::io::Cursor::new(part).chain(Reset));
        let (sender, _) = mpsc::channel();
        assert!(
            decode_stream(reader, false, &AtomicBool::new(false), sender)
                .unwrap_err()
                .is::<InterruptedStream>()
        );
    }

    #[test]
    fn explicit_stream_finish_preserves_usage_without_a_done_marker() -> Result<()> {
        let stream = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Finished\"},\"finish_reason\":\"stop\"}]}\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":120,\"completion_tokens\":13}}\n"
        );
        let reply = decode(stream, false)?;
        assert_eq!(reply.content, "Finished");
        assert_eq!(reply.usage.input, Some(120));
        assert_eq!(reply.usage.output, Some(13));
        Ok(())
    }

    #[test]
    fn project_attachments_and_pastes_have_a_compact_display() -> Result<()> {
        let project = tempfile::tempdir()?;
        fs::write(project.path().join("notes.md"), "attachment body")?;
        let pasted = "@this-is-literal\n".repeat(60);
        let (content, display) = attach_files(
            project.path(),
            "Read @notes.md",
            std::slice::from_ref(&pasted),
        )?;
        assert!(content.contains("attachment body") && content.contains(&pasted));
        let display = display.unwrap();
        assert!(display.contains("@notes.md") && display.contains('6'));
        assert!(!display.contains("attachment body") && !display.contains("@this-is-literal"));
        Ok(())
    }

    #[test]
    fn tool_batches_keep_results_and_reasoning_with_the_correct_response() -> Result<()> {
        let messages = [
            json!({"choices":[{"delta":{"reasoning_content":"reason","tool_calls":[
                {"index":1,"id":"second","function":{"name":"project","arguments":"{\"action\":\"stat\""}},
                {"index":0,"id":"first","function":{"name":"read","arguments":"{\"offset\":0"}}
            ]}}]}),
            json!({"choices":[{"delta":{"reasoning_content":"ing","tool_calls":[
                {"index":0,"function":{"arguments":",\"path\":\"README.md\"}"}},
                {"index":1,"function":{"arguments":",\"path\":\"Cargo.toml\"}"}}
            ]}}]}),
        ];
        let stream = messages
            .iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect::<String>()
            + "data: [DONE]\n\n";
        let reply = decode(&stream, false)?;
        assert_eq!(reply.calls[0].id, "first");
        assert_eq!(reply.calls[1].id, "second");
        let mut history = Vec::new();
        for call in &reply.calls {
            history.push(Message {
                role: "assistant".into(),
                content: call.arguments.clone(),
                reasoning: None,
                ..Default::default()
            });
            history.push(Message {
                role: "user".into(),
                content: format!("Result for {}", call.id),
                reasoning: None,
                ..Default::default()
            });
        }
        chat_error(&mut history, "Local error", "not sent");
        let wire = WireCalls::from([(0, reply)]);
        let mut provider = provider("Demo", "", default_context());
        provider.reasoning_api = ReasoningApi::DeepSeek;
        let plan = prepare_request(&provider, &history, false, "", &wire)?;
        let messages = plan.body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[1]["reasoning_content"], "reasoning");
        assert_eq!(messages[1]["tool_calls"][1]["id"], "second");
        assert_eq!(messages[1]["tool_calls"][0]["function"]["name"], "read");
        assert_eq!(messages[1]["tool_calls"][1]["function"]["name"], "stat");
        assert_eq!(
            messages[1]["tool_calls"][0]["function"]["arguments"],
            r#"{"offset":0,"path":"README.md"}"#
        );
        assert_eq!(messages[2]["tool_call_id"], "first");
        assert_eq!(messages[3]["content"], "Result for second");

        provider.chat_only = true;
        let plan = prepare_request(&provider, &history, false, "", &wire)?;
        assert!(plan.body.get("tools").is_none());
        let messages = plan.body["messages"].as_array().unwrap();
        assert!(messages.iter().all(|message| {
            message.get("tool_calls").is_none()
                && message.get("tool_call_id").is_none()
                && message["role"] != "tool"
        }));
        Ok(())
    }

    #[test]
    fn anthropic_stream_assembles_fragmented_tools() {
        let mut stream = String::new();
        for event in [
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","name":"read","id":"use_read","input":{}}}),
            json!({"type":"content_block_delta","index":0,"delta":{"partial_json":"{\"path\":"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"partial_json":"\"README.md\"}"}}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","name":"git_status","id":"use_status","input":{}}}),
            json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","name":"project","id":"use_old"}}),
            json!({"type":"content_block_delta","index":2,"delta":{"partial_json":"{\"action\":\"stat\","}}),
            json!({"type":"content_block_delta","index":2,"delta":{"partial_json":"\"path\":\"src\"}"}}),
            json!({"type":"message_stop"}),
        ] {
            stream.push_str(&format!("data: {event}\n\n"));
        }
        let calls = decode(&stream, true).unwrap().calls;
        let actions: Vec<_> = calls
            .iter()
            .map(|call| parse_action(&call.arguments).unwrap())
            .collect();
        assert_eq!(actions[0], json!({"action":"read","path":"README.md"}));
        assert_eq!(actions[1], json!({"action":"git_status"}));
        assert_eq!(actions[2], json!({"action":"stat","path":"src"}));
    }

    fn native_provider(protocol: Protocol) -> Provider {
        let mut provider = provider("m", "", default_context());
        provider.connection.protocol = protocol;
        provider
    }

    const PROTOCOLS: [Protocol; 3] = [
        Protocol::ChatCompletions,
        Protocol::Responses,
        Protocol::AnthropicMessages,
    ];

    fn tool_names(body: &Value) -> Vec<String> {
        body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| {
                tool.get("function")
                    .unwrap_or(tool)
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap()
                    .to_owned()
            })
            .collect()
    }

    #[test]
    fn every_protocol_exposes_one_tool_per_action_for_each_mode() -> Result<()> {
        let base = [
            "read",
            "read_many",
            "read_tool_result",
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
        let writes = [
            "edit",
            "create_file",
            "apply_changes",
            "apply_patch",
            "request_permissions",
            "run_task",
            "shell",
            "mcp",
        ];
        let modes = [
            (
                RequestMode {
                    desktop: false,
                    ..Default::default()
                },
                [&base[..], &writes, &["delegate"]].concat(),
            ),
            (
                RequestMode {
                    desktop: true,
                    ..Default::default()
                },
                [
                    &base[..],
                    &["computer_observe", "computer_list", "computer_input"],
                    &writes,
                    &["delegate"],
                ]
                .concat(),
            ),
            (
                RequestMode {
                    desktop: false,
                    delegate_limit: 0,
                    ..Default::default()
                },
                [&base[..], &writes].concat(),
            ),
            (
                RequestMode {
                    diagnose: true,
                    desktop: false,
                    ..Default::default()
                },
                base.to_vec(),
            ),
            (
                RequestMode {
                    diagnose: true,
                    desktop: true,
                    ..Default::default()
                },
                [&base[..], &["computer_observe", "computer_list"]].concat(),
            ),
            (
                RequestMode {
                    planning: true,
                    ..Default::default()
                },
                permissions::PLAN_ACTIONS.to_vec(),
            ),
            (
                RequestMode {
                    worker: true,
                    ..Default::default()
                },
                permissions::WORKER_ACTIONS.to_vec(),
            ),
        ];
        for protocol in PROTOCOLS {
            let provider = native_provider(protocol);
            for (mode, expected) in &modes {
                let plan = prepare_work_request(&provider, &[], *mode, "", &WireCalls::new())?;
                assert_eq!(tool_names(&plan.body), *expected, "{protocol:?}");
                for tool in plan.body["tools"].as_array().unwrap() {
                    let schema = match protocol {
                        Protocol::AnthropicMessages => &tool["input_schema"],
                        Protocol::Responses => &tool["parameters"],
                        _ => &tool["function"]["parameters"],
                    };
                    assert_eq!(schema["type"], "object");
                    assert!(schema["properties"].get("action").is_none());
                }
            }
        }
        Ok(())
    }

    #[test]
    fn per_action_calls_decode_to_the_same_actions_as_project_calls() {
        for old in [
            r#"{"action":"read","path":"README.md","offset":10,"limit":20}"#,
            r#"{"action":"search","query":"fn main","regex":true,"root":"/srv/x"}"#,
            r#"{"action":"git_status"}"#,
            r#"{"action":"browser","operation":"click","node":7}"#,
            r#"{"action":"edit","edit":{"path":"a.rs","reason":"r","before":"x","after":"y"}}"#,
            r#"{"action":"apply_changes","changes":[{"kind":"delete","path":"tmp"}]}"#,
            r#"{"action":"run_task","task":"cargo-test"}"#,
            r#"{"action":"run","task":"cargo-test"}"#,
            r#"{"action":"mcp","server":"s","tool":"t","arguments":{"a":1}}"#,
            r#"{"action":"delegate","tasks":[{"role":"researcher","task":"Find"}]}"#,
            r#"{"action":"computer_input","observation":3,"operation":"key","key":"Enter"}"#,
        ] {
            let expected = parse_action(old).unwrap();
            let (name, input) = tools::replay(tools::LEGACY_TOOL, old);
            let name = if expected["action"] == "run" {
                "run"
            } else {
                name.as_str()
            };
            assert_eq!(
                tools::decode(name, &input.to_string()).unwrap(),
                expected,
                "{old}"
            );
            assert_eq!(tools::decode(tools::LEGACY_TOOL, old).unwrap(), expected);
        }
    }

    #[test]
    fn legacy_project_calls_replay_as_valid_per_action_calls() -> Result<()> {
        let old = [
            r#"{"action":"read","path":"README.md"}"#,
            r#"{"action":"edit","edit":{"path":"a.rs","reason":"r","before":"x","after":"y"}}"#,
            r#"{"action":"run","task":"cargo-test"}"#,
        ];
        let calls: Vec<_> = old
            .iter()
            .enumerate()
            .map(|(index, arguments)| ToolCall {
                id: format!("call_{index}"),
                name: "project".into(),
                arguments: (*arguments).into(),
            })
            .collect();
        let mut history = vec![message("Fix it")];
        for call in &calls {
            history.push(Message {
                role: "assistant".into(),
                content: call.arguments.clone(),
                ..Default::default()
            });
            history.push(message(&format!("Tool result\n{}", call.id)));
        }
        let wire = WireCalls::from([(
            1,
            Reply {
                content: String::new(),
                reasoning: None,
                raw_reasoning: String::new(),
                calls,
                wire_blocks: Vec::new(),
                usage: Usage::default(),
            },
        )]);
        let expected = [
            ("read", json!({"path":"README.md"})),
            (
                "edit",
                json!({"path":"a.rs","reason":"r","before":"x","after":"y"}),
            ),
            ("run_task", json!({"task":"cargo-test"})),
        ];
        for protocol in PROTOCOLS {
            let plan = prepare_request(&native_provider(protocol), &history, false, "", &wire)?;
            let names = tool_names(&plan.body);
            let replayed: Vec<(String, String, Value)> = match protocol {
                Protocol::Responses => plan.body["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|item| item["type"] == "function_call")
                    .map(|item| {
                        (
                            field(item, "call_id").to_owned(),
                            field(item, "name").to_owned(),
                            serde_json::from_str(field(item, "arguments")).unwrap(),
                        )
                    })
                    .collect(),
                Protocol::AnthropicMessages => plan.body["messages"][1]["content"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|block| {
                        assert_eq!(block["type"], "tool_use");
                        (
                            field(block, "id").to_owned(),
                            field(block, "name").to_owned(),
                            block["input"].clone(),
                        )
                    })
                    .collect(),
                _ => plan.body["messages"][2]["tool_calls"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|call| {
                        (
                            field(call, "id").to_owned(),
                            field(&call["function"], "name").to_owned(),
                            serde_json::from_str(field(&call["function"], "arguments")).unwrap(),
                        )
                    })
                    .collect(),
            };
            assert_eq!(replayed.len(), 3, "{protocol:?}");
            for (index, ((id, name, input), (expected_name, expected_input))) in
                replayed.iter().zip(&expected).enumerate()
            {
                assert_eq!(id, &format!("call_{index}"));
                assert_eq!(name, expected_name, "{protocol:?}");
                assert!(names.contains(name), "{protocol:?} {name}");
                assert_eq!(input, expected_input, "{protocol:?}");
            }
            let results = plan.body.to_string();
            for index in 0..3 {
                assert!(results.contains(&format!("Tool result\\ncall_{index}")));
            }
        }
        Ok(())
    }

    #[test]
    fn unknown_tool_names_reach_dispatch_as_tool_errors() -> Result<()> {
        let event = json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"id":"x","function":{"name":"teleport","arguments":"{\"to\":\"moon\"}"}}
        ]},"finish_reason":"tool_calls"}]});
        let reply = decode(&format!("data: {event}\n\n"), false)?;
        let action = parse_action(&reply.calls[0].arguments).unwrap();
        assert_eq!(action, json!({"action":"teleport","to":"moon"}));
        let error = tools::unknown(&action).unwrap().to_string();
        assert!(error.contains("teleport"), "{error}");
        Ok(())
    }
    #[test]
    fn provider_urls_reject_credentials_and_remote_plaintext() {
        assert!(endpoint("http://api.example.org/v1").is_err());
        assert!(endpoint("https://user:secret@example.org/v1").is_err());
        assert!(endpoint("https://example.org/v1?key=secret").is_err());
        assert!(endpoint("http://127.0.0.1:1234/v1").is_ok());
    }
    #[test]
    fn agent_state_uses_existing_profile_namespace() -> Result<()> {
        let work = tempfile::tempdir()?;
        let mut state = models::load_state(
            br#"{"provider":{"anthropic":false,"url":"https://example.org/v1","model":"test","key":"synthetic-secret"},"sessions":[]}"#.as_slice(),
        )?;
        state.sessions.push(Session {
            project: "demo".into(),
            portable: true,
            messages: vec![message("test")],
            mode: DialogMode::Chat,
            ..Default::default()
        });
        save(work.path(), &state)?;
        let restored = models::load_state(fs::File::open(state_path(work.path()))?)?;
        assert!(restored.sessions[0].portable);
        assert_eq!(restored.sessions[0].messages[0].content, "test");
        assert_eq!(restored.sessions[0].mode, DialogMode::Chat);
        Ok(())
    }
    fn message(text: &str) -> Message {
        Message {
            role: "user".into(),
            content: text.into(),
            reasoning: None,
            ..Default::default()
        }
    }

    #[test]
    fn openai_reasoning_is_streamed_separately() -> Result<()> {
        let stream = "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"plan \"}}]}\n\n\
                      data: {\"choices\":[{\"delta\":{\"reasoning\":\"more\"}}]}\n\n\
                      data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\n\
                      data: [DONE]\n\n";
        let reply = decode(stream, false)?;
        assert_eq!(reply.content, "Hello");
        assert_eq!(reply.reasoning.as_deref(), Some("plan more"));
        Ok(())
    }

    #[test]
    fn anthropic_thinking_is_streamed_separately() -> Result<()> {
        let stream = "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"consider\"}}\n\n\
                      data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Answer\"}}\n\n\
                      data: {\"type\":\"message_stop\"}\n\n";
        let reply = decode(stream, true)?;
        assert_eq!(reply.content, "Answer");
        assert_eq!(reply.reasoning.as_deref(), Some("consider"));
        Ok(())
    }

    #[test]
    fn reasoning_only_reply_is_complete() -> Result<()> {
        let stream = "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"thinking\"}}]}\n\ndata: [DONE]\n\n";
        let reply = decode(stream, false)?;
        assert!(reply.content.is_empty());
        assert_eq!(reply.reasoning.as_deref(), Some("thinking"));
        Ok(())
    }

    #[test]
    fn output_limit_is_actionable() {
        let stream =
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\ndata: [DONE]\n\n";
        let error = decode(stream, false).unwrap_err().to_string();
        assert!(error.contains("Выход") || error.contains("output"));
    }

    #[test]
    fn openai_reasoning_payload_uses_effort_and_output_limit() -> Result<()> {
        let mut provider = provider("m", "k", 128_000);
        provider.reasoning = Reasoning::High;
        provider.output = Some(8192);
        let plan = build_request(&provider, &[message("hi")], false, "")?;
        assert_eq!(plan.body["reasoning_effort"], "high");
        assert_eq!(plan.body["max_tokens"], 8192);
        assert!(plan.input_tokens > 0);
        Ok(())
    }

    #[test]
    fn openrouter_reasoning_payload_uses_object() -> Result<()> {
        let mut provider = provider("m", "k", 128_000);
        provider.connection.url = "https://openrouter.ai/api/v1".into();
        provider.reasoning = Reasoning::Medium;
        let plan = build_request(&provider, &[message("hi")], false, "")?;
        assert_eq!(plan.body["reasoning"]["effort"], "medium");
        assert!(plan.body.get("reasoning_effort").is_none());
        Ok(())
    }

    #[test]
    fn anthropic_reasoning_payload_uses_budget() -> Result<()> {
        let mut provider = provider("m", "k", 128_000);
        provider.connection.protocol = Protocol::AnthropicMessages;
        provider.reasoning = Reasoning::Medium;
        provider.output = Some(8192);
        let plan = build_request(&provider, &[message("hi")], false, "")?;
        assert_eq!(plan.body["thinking"]["type"], "enabled");
        assert_eq!(plan.body["thinking"]["budget_tokens"], 2048);
        assert_eq!(plan.body["max_tokens"], 8192);
        Ok(())
    }

    #[test]
    fn anthropic_reasoning_rejects_small_output() {
        let mut provider = provider("m", "k", 128_000);
        provider.connection.protocol = Protocol::AnthropicMessages;
        provider.reasoning = Reasoning::High;
        provider.output = Some(4096);
        assert!(build_request(&provider, &[message("hi")], false, "").is_err());
    }

    #[test]
    fn equal_maxima_allow_a_reply_capped_by_the_current_request() -> Result<()> {
        for (protocol, expected_key, host) in [
            (
                Protocol::ChatCompletions,
                "max_tokens",
                "https://example.org/v1",
            ),
            (
                Protocol::ChatCompletions,
                "max_completion_tokens",
                "https://api.openai.com/v1",
            ),
            (
                Protocol::Responses,
                "max_output_tokens",
                "https://example.org/v1",
            ),
            (
                Protocol::AnthropicMessages,
                "max_tokens",
                "https://example.org/v1",
            ),
        ] {
            let mut provider = provider("m", "", 1_048_576);
            provider.connection.protocol = protocol;
            provider.connection.url = host.into();
            provider.input = Some(1_048_576);
            provider.output = Some(1_048_576);
            let history = [message(&"x".repeat(1200)), message("continue")];
            let baseline = build_request(&provider, &[], false, "")?;
            let plan = build_request(&provider, &history, false, &"instructions ".repeat(120))?;
            assert!(plan.input_tokens > baseline.input_tokens + 600);
            assert_eq!(
                plan.body[expected_key],
                provider.context_limit() - plan.input_tokens
            );

            let next_limit = plan.input_tokens + 1;
            provider.context = next_limit;
            provider.input = Some(next_limit);
            provider.output = Some(next_limit);
            let plan = build_request(&provider, &history, false, &"instructions ".repeat(120))?;
            assert_eq!(plan.body[expected_key], 1);
            provider.context -= 1;
            let error = build_request(&provider, &history, false, &"instructions ".repeat(120))
                .unwrap_err();
            assert!(error.downcast_ref::<OutputSpaceExceeded>().is_some());
        }
        Ok(())
    }

    #[test]
    fn context_shortage_blocks_anthropic_reasoning_before_network() -> Result<()> {
        let mut provider = provider("m", "", 8_000);
        provider.connection.protocol = Protocol::AnthropicMessages;
        provider.reasoning = Reasoning::Medium;
        provider.output = Some(4_096);
        let history = [message(&"x".repeat(2_000))];
        let estimate = build_request(&provider, &history, false, "")?.input_tokens;
        provider.context = estimate + 2_048;
        let result = build_request(&provider, &history, false, "");
        assert_eq!(input_estimate(&result), Some(estimate));
        let error = result.unwrap_err();
        let error = error.downcast_ref::<OutputSpaceExceeded>().unwrap();
        assert_eq!(error.available, 2_048);
        assert_eq!(error.reasoning_budget, 2_048);
        Ok(())
    }

    #[test]
    fn deepseek_off_and_manual_reasoning_use_each_protocols_fields() -> Result<()> {
        for (protocol, enable, effort, disable) in [
            (
                Protocol::ChatCompletions,
                Some("/thinking/type"),
                "/reasoning_effort",
                "/thinking/type",
            ),
            (
                Protocol::Responses,
                None,
                "/reasoning/effort",
                "/reasoning/effort",
            ),
            (
                Protocol::AnthropicMessages,
                Some("/thinking/type"),
                "/output_config/effort",
                "/thinking/type",
            ),
        ] {
            let mut provider = provider("m", "", 64_000);
            provider.connection.protocol = protocol;
            provider.connection.url = "https://api.deepseek.com/v1".into();
            provider.reasoning_levels = Some(vec![ReasoningLevel {
                level: Reasoning::Xhigh,
                wire: Some("high".into()),
            }]);
            provider.reasoning = Reasoning::Xhigh;
            let plan = build_request(&provider, &[], false, "")?;
            assert_eq!(plan.body.pointer(effort), Some(&json!("high")));
            if let Some(field) = enable {
                assert_eq!(plan.body.pointer(field), Some(&json!("enabled")));
            }
            provider.reasoning = Reasoning::Off;
            let plan = build_request(&provider, &[], false, "")?;
            let disabled = if protocol == Protocol::Responses {
                "none"
            } else {
                "disabled"
            };
            assert_eq!(plan.body.pointer(disable), Some(&json!(disabled)));

            provider.connection.url = "https://gateway.example/v1".into();
            provider.reasoning_levels = Some(vec![ReasoningLevel {
                level: Reasoning::Ultra,
                wire: Some("deep".into()),
            }]);
            provider.reasoning = Reasoning::Ultra;
            if protocol == Protocol::AnthropicMessages {
                provider.reasoning_api = ReasoningApi::Adaptive;
            }
            let plan = build_request(&provider, &[], false, "")?;
            assert_eq!(plan.body.pointer(effort), Some(&json!("deep")));
        }
        Ok(())
    }

    #[test]
    fn reasoning_off_and_none_send_nothing() -> Result<()> {
        let mut provider = provider("m", "k", 128_000);
        let plan = build_request(&provider, &[message("hi")], false, "")?;
        assert!(plan.body.get("reasoning_effort").is_none());
        provider.reasoning = Reasoning::High;
        provider.reasoning_api = ReasoningApi::None;
        let plan = build_request(&provider, &[message("hi")], false, "")?;
        assert!(plan.body.get("reasoning_effort").is_none());
        assert!(plan.body.get("reasoning").is_none());
        Ok(())
    }

    #[test]
    fn input_limit_is_enforced_before_network() {
        let mut provider = provider("m", "k", 128_000);
        let initial = prepare_request(&provider, &[], false, "", &WireCalls::new());
        assert!(input_estimate(&initial).is_some_and(|tokens| tokens > 0));

        provider.input = Some(1_000);
        let too_large = build_request(&provider, &[message(&"x".repeat(20_000))], false, "");
        assert!(input_estimate(&too_large).is_some_and(|tokens| tokens > 5_000));
        let error = too_large.unwrap_err().to_string();
        assert!(error.contains("1000") || error.contains("1 000"));
        assert!(error.contains("лимит") || error.contains("limit"));

        let invalid = build_request(&provider, &[message(&"x".repeat(600_000))], false, "");
        assert!(invalid.is_err());
        assert_eq!(input_estimate(&invalid), None);
    }

    #[test]
    fn provider_error_exposes_body_and_redacts_key() {
        let text = http_error_text(
            401,
            "{\"error\":{\"message\":\"bad key sk-secret-123\"}}",
            "sk-secret-123",
        );
        assert!(text.contains("401"));
        assert!(text.contains("bad key"));
        assert!(!text.contains("sk-secret-123"));
        assert!(text.contains("<key>"));
    }

    #[test]
    fn probe_payload_is_small_and_has_no_tools() {
        let provider = provider("m", "k", 128_000);
        let body = probe_payload(&provider);
        assert_eq!(body["max_tokens"], 16);
        assert!(body.get("tools").is_none());
        assert_eq!(body["messages"][0]["content"], "ping");
    }

    #[test]
    fn old_state_without_new_fields_still_loads() -> Result<()> {
        let state = models::load_state(
            br#"{"provider":{"anthropic":false,"url":"https://example.org/v1","model":"m","key":"k"},"sessions":[]}"#.as_slice(),
        )?;
        let provider = state
            .active_provider(DialogMode::Agent)
            .expect("migrated active model");
        assert_eq!(provider.context, default_context());
        assert_eq!(provider.output_limit(), DEFAULT_OUTPUT);
        assert_eq!(provider.reasoning, Reasoning::Off);
        assert_eq!(provider.reasoning_api, ReasoningApi::Auto);
        let message: Message = serde_json::from_str("{\"role\":\"user\",\"content\":\"x\"}")?;
        assert!(message.reasoning.is_none());
        assert!(!message.summary);
        Ok(())
    }
}

#[cfg(test)]
mod vision_tests {
    use super::{provider_tests::provider, *};
    #[test]
    fn screenshot_is_attached_to_the_next_request_only() {
        for (protocol, path) in [
            (Protocol::ChatCompletions, "messages"),
            (Protocol::AnthropicMessages, "messages"),
            (Protocol::Responses, "input"),
        ] {
            let mut provider = provider("test", "", default_context());
            provider.connection.protocol = protocol;
            provider.vision = true;
            let mut body = json!({path:[]});
            attach_image(
                &provider,
                &mut body,
                &Visual {
                    mime: "image/jpeg".into(),
                    data: "/9j/".into(),
                },
            );
            let content = body[path].as_array().unwrap();
            assert_eq!(content.len(), 1);
            assert!(content[0]["content"].to_string().contains("/9j/"));
            provider.vision = false;
            body[path] = json!([]);
            attach_image(
                &provider,
                &mut body,
                &Visual {
                    mime: "image/jpeg".into(),
                    data: "/9j/".into(),
                },
            );
            assert!(body[path].as_array().unwrap().is_empty());
        }
    }

    #[test]
    fn responses_continues_native_tools_with_the_exact_identifier() -> Result<()> {
        let payload = [
            json!({"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_test","name":"read"}}),
            json!({"type":"response.function_call_arguments.delta","delta":"{\"path\":"}),
            json!({"type":"response.function_call_arguments.delta","delta":"\"README.md\"}"}),
            json!({"type":"response.completed"}),
        ];
        let stream = payload
            .iter()
            .map(|value| format!("data: {value}\n\n"))
            .collect::<String>();
        let (tx, _rx) = mpsc::channel();
        let result = decode_stream_mode(
            std::io::Cursor::new(stream),
            false,
            true,
            &AtomicBool::new(false),
            tx,
        )?;
        assert_eq!(result.calls[0].id, "call_test");
        let history = vec![
            Message {
                role: "assistant".into(),
                content: result.calls[0].arguments.clone(),
                reasoning: None,
                ..Default::default()
            },
            Message {
                role: "user".into(),
                content: "File contents".into(),
                reasoning: None,
                ..Default::default()
            },
        ];
        let wire = WireCalls::from([(0, result)]);
        let mut provider = provider("test", "", default_context());
        provider.connection.protocol = Protocol::Responses;
        let plan = prepare_request(&provider, &history, false, "", &wire)?;
        assert_eq!(plan.body["input"][0]["call_id"], "call_test");
        assert_eq!(plan.body["input"][0]["name"], "read");
        assert_eq!(
            plan.body["input"][0]["arguments"],
            r#"{"path":"README.md"}"#
        );
        assert_eq!(plan.body["input"][1]["call_id"], "call_test");
        assert_eq!(plan.body["input"][1]["output"], "File contents");
        Ok(())
    }
}
