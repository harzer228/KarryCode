use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
};
mod actions;
mod agent;
mod browser;
mod changes;
mod commands;
mod compact;
mod desktop;
mod files;
mod headless;
mod mcp;
mod models;
mod onboarding;
mod permissions;
mod preview;
mod project;
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

pub use agent::run;
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

#[cfg(test)]
mod tests {
    use super::provider::{
        RequestMode,
        tests::{self as provider_tests, message},
    };
    use super::*;
    use serde_json::json;

    #[test]
    fn planning_schema_and_runtime_use_the_same_allowlist() {
        let mode = RequestMode {
            planning: true,
            ..Default::default()
        };
        assert_eq!(tools::names(mode), permissions::PLAN_ACTIONS);
        let provider = provider_tests::provider("m", "", default_context());
        for tool in tools::request_tools(&provider, mode).as_array().unwrap() {
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
        for tool in tools::request_tools(&provider, mode).as_array().unwrap() {
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
    fn actions_require_complete_json() {
        assert!(parse_action("{\"action\":\"read\",\"path\":\"README.md\"}").is_some());
        assert!(parse_action("{\"action\":").is_none());
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
        });
        save(work.path(), &state)?;
        let restored = models::load_state(fs::File::open(state_path(work.path()))?)?;
        assert!(restored.sessions[0].portable);
        assert_eq!(restored.sessions[0].messages[0].content, "test");
        assert_eq!(restored.sessions[0].mode, DialogMode::Chat);
        Ok(())
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
