//! Saved connections and models, legacy profiles, and provider catalogs.

use super::{
    Message, Provider, Reasoning, ReasoningApi, Session, State, chat_error, endpoint,
    permissions::DialogMode,
    provider::{apply_reasoning, probe_provider, provider_transport_error},
    ui::{ModelAction, ModelEdit, Screen},
    web,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashSet, io::Read, time::Duration};

pub(super) const STATE_VERSION: u8 = 2;
pub(super) const MAX_MODEL_TOKENS: u32 = 10_000_000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Protocol {
    #[default]
    ChatCompletions,
    Responses,
    AnthropicMessages,
}

impl Protocol {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::ChatCompletions => "OpenAI Chat Completions",
            Self::Responses => "OpenAI Responses",
            Self::AnthropicMessages => "Anthropic Messages",
        }
    }

    pub(super) fn next(self, backwards: bool) -> Self {
        match (self, backwards) {
            (Self::ChatCompletions, false) | (Self::AnthropicMessages, true) => Self::Responses,
            (Self::Responses, false) | (Self::ChatCompletions, true) => Self::AnthropicMessages,
            (Self::AnthropicMessages, false) | (Self::Responses, true) => Self::ChatCompletions,
        }
    }
}

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Connection {
    pub(super) id: u32,
    pub(super) name: String,
    pub(super) url: String,
    pub(super) protocol: Protocol,
    pub(super) key: String,
}

impl Connection {
    pub(super) fn display_name(&self, connections: &[Self]) -> String {
        if connections
            .iter()
            .any(|other| other.id != self.id && other.name == self.name)
        {
            format!("{} #{}", self.name, self.id)
        } else {
            self.name.clone()
        }
    }

    pub(super) fn authorize(&self, mut request: ureq::Request) -> ureq::Request {
        if self.protocol == Protocol::AnthropicMessages {
            request = request.set("anthropic-version", "2023-06-01");
            if !self.key.is_empty() {
                request = request.set("x-api-key", &self.key);
            }
        } else if !self.key.is_empty() {
            request = request.set("Authorization", &format!("Bearer {}", self.key));
        }
        request
    }

    pub(super) fn host_is(&self, host: &str) -> bool {
        url::Url::parse(&self.url).is_ok_and(|url| url.host_str() == Some(host))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct ReasoningLevel {
    pub(super) level: Reasoning,
    pub(super) wire: Option<String>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct ModelSettings {
    pub(super) name: String,
    pub(super) model: String,
    pub(super) context: u32,
    pub(super) input: u32,
    pub(super) output: u32,
    #[serde(default)]
    pub(super) vision: Option<bool>,
    #[serde(default)]
    pub(super) reasoning: Reasoning,
    #[serde(default)]
    pub(super) reasoning_api: ReasoningApi,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) reasoning_levels: Option<Vec<ReasoningLevel>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) enabled_reasoning: Option<Vec<Reasoning>>,
}

impl ModelSettings {
    pub(super) fn missing_limits(&self) -> bool {
        [self.context, self.input, self.output].contains(&0)
    }

    pub(super) fn validate_limits(&self) -> Result<()> {
        if self.context != 0 {
            ensure!(
                self.input <= self.context,
                tr!(
                    "Максимальный вход больше окна контекста.",
                    "Maximum input exceeds the context window."
                )
            );
            ensure!(
                self.output <= self.context,
                tr!(
                    "Максимальный выход больше окна контекста.",
                    "Maximum output exceeds the context window."
                )
            );
        }
        Ok(())
    }

    pub(super) fn available_reasoning(&self, connection: &Connection) -> Vec<Reasoning> {
        let mut levels = self.configured_reasoning(connection);
        levels.retain(|level| {
            *level == Reasoning::ProviderDefault
                || self
                    .enabled_reasoning
                    .as_ref()
                    .is_none_or(|enabled| enabled.contains(level))
        });
        levels
    }

    pub(super) fn configured_reasoning(&self, connection: &Connection) -> Vec<Reasoning> {
        if self.reasoning_api == ReasoningApi::None {
            return vec![Reasoning::ProviderDefault];
        }
        let mut levels = vec![Reasoning::ProviderDefault];
        let deepseek = self.reasoning_api == ReasoningApi::DeepSeek
            || self.reasoning_api == ReasoningApi::Auto && connection.host_is("api.deepseek.com");
        if let Some(declared) = &self.reasoning_levels {
            levels.extend(Reasoning::LEVELS.iter().copied().filter(|level| {
                declared.iter().any(|option| option.level == *level)
                    || deepseek && *level == Reasoning::Off && !declared.is_empty()
            }));
        } else if deepseek {
            levels.extend([
                Reasoning::Off,
                Reasoning::Low,
                Reasoning::High,
                Reasoning::Max,
            ]);
        } else {
            levels.extend(Reasoning::LEVELS.into_iter().filter(|level| {
                if connection.protocol == Protocol::AnthropicMessages {
                    self.reasoning_api == ReasoningApi::Adaptive && *level != Reasoning::Ultra
                        || matches!(
                            level,
                            Reasoning::Off | Reasoning::Low | Reasoning::Medium | Reasoning::High
                        )
                } else {
                    *level != Reasoning::Ultra || self.reasoning_api == ReasoningApi::Custom
                }
            }));
        }
        levels
    }

    pub(super) fn set_reasoning_mapping(
        &mut self,
        connection: &Connection,
        level: Reasoning,
        option: Option<ReasoningLevel>,
    ) {
        if self.reasoning_levels.is_none() {
            self.reasoning_levels = Some(
                self.configured_reasoning(connection)
                    .into_iter()
                    .filter(|level| *level != Reasoning::ProviderDefault)
                    .map(|level| ReasoningLevel { level, wire: None })
                    .collect(),
            );
        }
        if let Some(levels) = &mut self.reasoning_levels {
            levels.retain(|option| option.level != level);
            levels.extend(option);
        }
    }

    pub(super) fn resolve(&self, connection: &Connection, mode: DialogMode) -> Provider {
        Provider {
            connection: connection.clone(),
            chat_only: mode == DialogMode::Chat,
            model: self.model.clone(),
            context: self.context,
            input: Some(self.input),
            output: Some(self.output),
            vision: self.vision.unwrap_or_default(),
            reasoning: self.reasoning,
            reasoning_api: self.reasoning_api,
            reasoning_levels: self.reasoning_levels.clone(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct SavedModel {
    pub(super) connection: u32,
    #[serde(flatten)]
    pub(super) settings: ModelSettings,
}

#[derive(Deserialize, PartialEq, Eq)]
struct LegacyModel {
    #[serde(default)]
    name: String,
    #[serde(default)]
    chat_only: bool,
    anthropic: bool,
    #[serde(default)]
    responses: bool,
    #[serde(default)]
    vision: bool,
    url: String,
    model: String,
    key: String,
    #[serde(default = "super::default_context")]
    context: u32,
    #[serde(default)]
    input: Option<u32>,
    #[serde(default)]
    output: Option<u32>,
    #[serde(default = "legacy_reasoning")]
    reasoning: Reasoning,
    #[serde(default)]
    reasoning_api: ReasoningApi,
}

fn legacy_reasoning() -> Reasoning {
    Reasoning::Off
}

#[derive(Deserialize)]
struct LegacyState {
    #[serde(default)]
    providers: Vec<LegacyModel>,
    provider: Option<LegacyModel>,
    sessions: Vec<Session>,
    #[serde(default)]
    web: web::Settings,
}

pub(super) fn load_state(reader: impl Read) -> Result<State> {
    let data: Value = serde_json::from_reader(reader)?;
    let state = if let Some(version) = data.get("version") {
        ensure!(
            version.as_u64() == Some(u64::from(STATE_VERSION)),
            tr!(
                "Неизвестный формат профиля агента.",
                "Unknown agent profile format."
            )
        );
        serde_json::from_value(data)?
    } else {
        migrate(serde_json::from_value(data)?)?
    };
    let ids: HashSet<_> = state
        .connections
        .iter()
        .map(|connection| connection.id)
        .collect();
    ensure!(
        ids.len() == state.connections.len()
            && state
                .models
                .iter()
                .all(|model| ids.contains(&model.connection))
            && state
                .active_model
                .is_none_or(|index| index < state.models.len()),
        tr!(
            "Нарушены связи в профиле агента.",
            "Agent profile references are inconsistent."
        )
    );
    Ok(state)
}

fn migrate(mut legacy: LegacyState) -> Result<State> {
    let mode = if legacy
        .provider
        .as_ref()
        .is_some_and(|model| model.chat_only)
    {
        DialogMode::Chat
    } else {
        DialogMode::Agent
    };
    for session in &mut legacy.sessions {
        session.mode = mode;
    }
    let active_model = legacy.provider.take().map(|active| {
        if let Some(index) = legacy.providers.iter().position(|model| model == &active) {
            index
        } else {
            let index = legacy.providers.len();
            legacy.providers.push(active);
            index
        }
    });
    let mut state = State {
        sessions: legacy.sessions,
        web: legacy.web,
        active_model,
        ..State::default()
    };
    for model in legacy.providers {
        let url = endpoint(&model.url)?;
        let protocol = if model.responses {
            Protocol::Responses
        } else if model.anthropic {
            Protocol::AnthropicMessages
        } else {
            Protocol::ChatCompletions
        };
        let name = if model.name.is_empty() {
            model.model.clone()
        } else {
            model.name
        };
        let connection = if let Some(existing) = state.connections.iter().find(|connection| {
            connection.url == url && connection.protocol == protocol && connection.key == model.key
        }) {
            existing.id
        } else {
            let id = state.next_connection_id()?;
            state.connections.push(Connection {
                id,
                name: name.clone(),
                url,
                protocol,
                key: model.key,
            });
            id
        };
        let context = model.context.max(1_000);
        let output = model
            .output
            .unwrap_or(super::DEFAULT_OUTPUT)
            .max(256)
            .min(context);
        let input = model
            .input
            .unwrap_or_else(|| context.saturating_sub(output))
            .clamp(1, context);
        state.models.push(SavedModel {
            connection,
            settings: ModelSettings {
                name,
                model: model.model,
                context,
                input,
                output,
                vision: Some(model.vision),
                reasoning: model.reasoning,
                reasoning_api: model.reasoning_api,
                reasoning_levels: None,
                enabled_reasoning: None,
            },
        });
    }
    Ok(state)
}

impl State {
    pub(super) fn next_connection_id(&self) -> Result<u32> {
        self.connections
            .iter()
            .map(|connection| connection.id)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .context(tr!(
                "Лимит подключений достигнут.",
                "Connection limit reached."
            ))
    }

    pub(super) fn connection(&self, id: u32) -> Option<&Connection> {
        self.connections
            .iter()
            .find(|connection| connection.id == id)
    }

    pub(super) fn active_provider(&self, mode: DialogMode) -> Option<Provider> {
        let model = self.models.get(self.active_model?)?;
        Some(
            model
                .settings
                .resolve(self.connection(model.connection)?, mode),
        )
    }

    pub(super) fn remove_model(&mut self, index: usize) {
        self.models.remove(index);
        self.active_model = match self.active_model {
            Some(active) if active == index => (!self.models.is_empty()).then_some(0),
            Some(active) if active > index => Some(active - 1),
            active => active,
        };
    }
}

pub(super) struct CatalogModel {
    pub(super) id: String,
    pub(super) name: String,
    metadata: Value,
}

pub(super) fn catalog(connection: &Connection) -> Result<Vec<CatalogModel>> {
    let mut url = url::Url::parse(&format!("{}/models", connection.url))?;
    if connection.host_is("api.melious.ai") {
        url.query_pairs_mut().append_pair("include_meta", "true");
    }
    catalog_payload(connection, &url)?.map_or_else(|| Ok(Vec::new()), parse_catalog)
}

fn model_details(connection: &Connection, id: &str) -> Result<Option<CatalogModel>> {
    let mut url = url::Url::parse(&format!("{}/models", connection.url))?;
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!(tr!("Некорректный URL каталога.", "Invalid catalog URL.")))?
        .push(id);
    url.query_pairs_mut().append_pair("include_meta", "true");
    let Some(raw) = catalog_payload(connection, &url)? else {
        return Ok(None);
    };
    Ok(parse_catalog(serde_json::json!({"data": [raw]}))?
        .into_iter()
        .find(|item| item.id == id))
}

fn catalog_payload(connection: &Connection, url: &url::Url) -> Result<Option<Value>> {
    let client = ureq::AgentBuilder::new()
        .redirects(0)
        .timeout(Duration::from_secs(12))
        .build();
    let request = connection
        .authorize(client.get(url.as_str()))
        .set("Accept", "application/json");
    let response = match request.call() {
        Ok(response) => response,
        Err(ureq::Error::Status(404 | 405 | 501, _)) => return Ok(None),
        Err(error) => return Err(provider_transport_error(error, &connection.key)),
    };
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(2 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 2 * 1024 * 1024,
        tr!("Каталог больше 2 МиБ.", "The model catalog exceeds 2 MiB.")
    );
    Ok(Some(serde_json::from_slice(&bytes)?))
}

fn parse_catalog(payload: Value) -> Result<Vec<CatalogModel>> {
    let items: Vec<_> = if let Some(data) = payload["data"].as_array() {
        data.iter()
            .map(|value| (value["id"].as_str(), value))
            .collect()
    } else if let Some(models) = payload["models"].as_object() {
        models
            .iter()
            .map(|(id, value)| (Some(id.as_str()), value))
            .collect()
    } else {
        bail!(tr!(
            "Неизвестный формат каталога. Введи ID модели вручную.",
            "Unrecognized catalog format. Enter the model ID manually."
        ));
    };
    let mut ids = HashSet::new();
    Ok(items
        .into_iter()
        .filter_map(|(id, raw)| {
            let id = id?.trim();
            if !raw.is_object()
                || id.is_empty()
                || id.chars().count() > 160
                || id.chars().any(char::is_control)
                || !ids.insert(id)
            {
                return None;
            }
            let name: String = ["display_name", "displayName", "name"]
                .iter()
                .find_map(|field| {
                    raw[field]
                        .as_str()
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                })
                .unwrap_or(id)
                .chars()
                .filter(|character| !character.is_control())
                .take(64)
                .collect();
            Some(CatalogModel {
                id: id.to_owned(),
                name: if name.is_empty() {
                    id.chars().take(64).collect()
                } else {
                    name
                },
                metadata: raw.clone(),
            })
        })
        .take(500)
        .collect())
}

fn capacity(min: u32, candidates: &[&Value]) -> Option<u32> {
    candidates.iter().find_map(|value| {
        value
            .as_u64()
            .and_then(|number| u32::try_from(number).ok())
            .filter(|number| (min..=MAX_MODEL_TOKENS).contains(number))
    })
}

pub(super) fn fill_limits(model: &mut ModelSettings, catalog: Option<&CatalogModel>) {
    let data = catalog.map_or(&Value::Null, |model| &model.metadata);
    if model.context == 0 {
        model.context = capacity(
            1,
            &[
                &data["contextWindow"],
                &data["context_window"],
                &data["context_length"],
                &data["limit"]["context"],
                &data["_meta"]["context_length"],
            ],
        )
        .unwrap_or_default();
    }
    if model.input == 0 {
        model.input = capacity(
            1,
            &[
                &data["max_input_tokens"],
                &data["maxInputTokens"],
                &data["limit"]["input"],
            ],
        )
        .unwrap_or(model.context);
    }
    if model.output == 0 {
        model.output = capacity(
            1,
            &[
                &data["max_output_tokens"],
                &data["maxOutputTokens"],
                &data["max_completion_tokens"],
                &data["max_tokens"],
                &data["maxTokens"],
                &data["limit"]["output"],
                &data["top_provider"]["max_completion_tokens"],
                &data["_meta"]["max_output_tokens"],
            ],
        )
        .unwrap_or_default();
    }
}

fn reasoning_levels(data: &Value) -> Option<Vec<ReasoningLevel>> {
    let declared = data
        .get("reasoningEfforts")
        .or_else(|| data.get("reasoning_efforts"));
    if let Some(declared) = declared {
        if declared == &Value::Bool(false) {
            return Some(Vec::new());
        }
        if let Some(map) = declared.as_object() {
            return Some(
                Reasoning::LEVELS
                    .iter()
                    .filter_map(|&level| {
                        let raw = map.get(level.identifier())?;
                        let wire = raw
                            .as_str()
                            .map(str::trim)
                            .filter(|value| valid_effort(value));
                        if wire.is_none()
                            && (level != Reasoning::Off || !raw.is_null() && raw != "")
                        {
                            return None;
                        }
                        Some(ReasoningLevel {
                            level,
                            wire: wire.map(str::to_owned),
                        })
                    })
                    .collect(),
            );
        }
    }
    let levels = declared
        .and_then(Value::as_array)
        .or_else(|| data["effort"]["supported_levels"].as_array())
        .or_else(|| data["capabilities"]["reasoning"]["supported_levels"].as_array())?;
    Some(
        Reasoning::LEVELS
            .iter()
            .filter_map(|&level| {
                let wire = levels
                    .iter()
                    .filter_map(Value::as_str)
                    .find(|value| Reasoning::parse(value) == Some(level))?;
                Some(ReasoningLevel {
                    level,
                    wire: (level != Reasoning::Off).then(|| wire.to_owned()),
                })
            })
            .collect(),
    )
}

pub(super) fn valid_effort(input: &str) -> bool {
    !input.is_empty()
        && input.len() <= 64
        && input
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

impl CatalogModel {
    pub(super) fn settings(&self, connection: &Connection) -> ModelSettings {
        let data = &self.metadata;
        let vision = data["input_modalities"]
            .as_array()
            .or_else(|| data["architecture"]["input_modalities"].as_array())
            .map(|values| values.iter().any(|value| value == "image"))
            .or_else(|| data["capabilities"]["image_input"]["supported"].as_bool());
        let reasoning_api =
            if data["owned_by"] == "deepseek" && data["effort"]["supported_levels"].is_array() {
                ReasoningApi::DeepSeek
            } else if connection.protocol == Protocol::AnthropicMessages
                && data["capabilities"]["thinking"]["types"]["adaptive"]["supported"] == true
            {
                ReasoningApi::Adaptive
            } else if connection.protocol == Protocol::ChatCompletions
                && data["supported_parameters"]
                    .as_array()
                    .is_some_and(|values| values.iter().any(|value| value == "reasoning"))
            {
                ReasoningApi::Object
            } else {
                ReasoningApi::Auto
            };
        let mut model = ModelSettings {
            name: self.name.clone(),
            model: self.id.clone(),
            vision,
            reasoning_api,
            reasoning_levels: reasoning_levels(data),
            ..ModelSettings::default()
        };
        fill_limits(&mut model, Some(self));
        model
    }
}

pub(super) fn provider_menu(screen: &mut Screen, state: &mut State) -> Result<bool> {
    let mut options = vec![tr!("Добавить провайдера", "Add provider").into()];
    options.extend(state.connections.iter().map(|connection| {
        format!(
            "{} · {}",
            connection.display_name(&state.connections),
            connection.url
        )
    }));
    let Some(choice) = screen.choose(tr!("Провайдеры", "Providers"), &options)? else {
        return Ok(false);
    };
    if choice == 0 {
        if let Some(connection) = screen.edit_connection(state.next_connection_id()?, None)? {
            state.connections.push(connection);
            screen.notice(tr!(
                "Провайдер сохранён. Добавь его модели в /model.",
                "Provider saved. Add its models in /model."
            ));
            return Ok(true);
        }
        return Ok(false);
    }
    let index = choice - 1;
    let connection = &state.connections[index];
    let choices = [
        tr!("Изменить подключение", "Edit connection").into(),
        tr!("Удалить провайдера", "Delete provider").into(),
    ];
    match screen.choose(&connection.name, &choices)? {
        Some(0) => {
            if let Some(edited) = screen.edit_connection(connection.id, Some(connection))? {
                if (
                    connection.url.as_str(),
                    connection.protocol,
                    connection.key.as_str(),
                ) != (edited.url.as_str(), edited.protocol, edited.key.as_str())
                {
                    state.settings.provider_limits.remove(&connection.id);
                }
                state.connections[index] = edited;
                screen.notice(tr!("Провайдер сохранён.", "Provider saved."));
                return Ok(true);
            }
        }
        Some(1) => {
            if state
                .models
                .iter()
                .any(|model| model.connection == connection.id)
            {
                screen.report(
                    tr!("Удаление провайдера", "Delete provider"),
                    tr!(
                        "Сначала удали сохранённые модели этого провайдера через /model.",
                        "Remove this provider's saved models through /model first."
                    ),
                )?;
            } else if screen.confirm(
                tr!(
                    "Удалить подключение и его ключ?",
                    "Delete the connection and its key?"
                ),
                &connection.name,
            )? {
                state.settings.remove_connection(connection.id);
                state.connections.remove(index);
                return Ok(true);
            }
        }
        _ => {}
    }
    Ok(false)
}

pub(super) fn choose_connection(screen: &mut Screen, state: &State) -> Result<Option<u32>> {
    if state.connections.is_empty() {
        screen.notice(tr!(
            "Сначала добавь подключение через /provider.",
            "Add a connection through /provider first."
        ));
        return Ok(None);
    }
    let choices: Vec<_> = state
        .connections
        .iter()
        .map(|connection| {
            format!(
                "{} · {} · {}",
                connection.display_name(&state.connections),
                connection.protocol.label(),
                connection.url
            )
        })
        .collect();
    Ok(screen
        .choose(tr!("Выбери провайдера", "Choose a provider"), &choices)?
        .map(|index| state.connections[index].id))
}

fn load_catalog(
    screen: &mut Screen,
    connection: &Connection,
    history: &mut Vec<Message>,
) -> Result<Vec<CatalogModel>> {
    screen.busy(tr!(
        "Загружаю каталог моделей…",
        "Loading the model catalog…"
    ))?;
    match catalog(connection) {
        Ok(models) => Ok(models),
        Err(error) => {
            chat_error(history, tr!("Каталог моделей", "Model catalog"), error);
            Ok(Vec::new())
        }
    }
}

fn model_preview(settings: &ModelSettings, connection: &Connection) -> String {
    let levels = settings.available_reasoning(connection);
    let reasoning = levels
        .iter()
        .map(|level| level.identifier())
        .collect::<Vec<_>>()
        .join(", ");
    trf!(
        "{} · {}\nКонтекст: {}\nМакс. вход: {}\nМакс. выход: {}\nReasoning: {}",
        "{} · {}\nContext: {}\nMax input: {}\nMax output: {}\nReasoning: {}",
        connection.name,
        settings.model,
        settings.context,
        settings.input,
        settings.output,
        reasoning
    )
}

fn auto_fill(settings: &mut ModelSettings, connection: &Connection, models: &[CatalogModel]) {
    let metadata = models.iter().find(|item| item.id == settings.model);
    fill_limits(settings, metadata);
    if let Some(item) = metadata {
        let declared = item.settings(connection);
        if settings.name.is_empty() {
            settings.name = declared.name;
        }
        settings.vision = settings.vision.or(declared.vision);
        if settings.reasoning_levels.is_none()
            && declared.reasoning_levels.as_ref().is_some_and(|levels| {
                matches!(
                    settings.reasoning,
                    Reasoning::ProviderDefault | Reasoning::Off
                ) || levels
                    .iter()
                    .any(|option| option.level == settings.reasoning)
            })
        {
            settings.reasoning_levels = declared.reasoning_levels;
        }
        if settings.reasoning_api == ReasoningApi::Auto {
            settings.reasoning_api = declared.reasoning_api;
        }
    }
}

enum EditResult {
    Saved(ModelSettings),
    Catalog(ModelSettings),
    Cancel,
}

enum ModelOpening<'a> {
    New,
    Edit { autofill: bool },
    Imported(&'a [CatalogModel]),
    Setup(&'a [CatalogModel]),
}

fn edit_settings(
    screen: &mut Screen,
    connection: &Connection,
    mut settings: ModelSettings,
    opening: ModelOpening<'_>,
    history: &mut Vec<Message>,
) -> Result<EditResult> {
    let setup = matches!(opening, ModelOpening::Setup(_));
    let (editing, allow_catalog, mut autofill, mut cached) = match opening {
        ModelOpening::New => (false, true, false, None),
        ModelOpening::Edit { autofill } => (true, false, autofill, None),
        ModelOpening::Imported(catalog) => (false, false, true, Some(catalog)),
        ModelOpening::Setup(catalog) => (false, false, false, Some(catalog)),
    };
    let mut fetched = None;
    let mut message = settings.missing_limits().then(|| {
        if setup {
            tr!(
                "Укажи ID и лимиты, которых нет в каталоге.",
                "Enter the model ID and any limits missing from the catalog."
            )
        } else if settings.model.is_empty() {
            tr!(
                "Введи ID модели или открой каталог через F4.",
                "Enter a model ID or open the catalog with F4."
            )
        } else {
            tr!(
                "Заполни параметры вручную или нажми F5.",
                "Enter the model settings or press F5."
            )
        }
        .to_owned()
    });
    loop {
        if autofill {
            if cached.is_none() && fetched.is_none() {
                fetched = Some(load_catalog(screen, connection, history)?);
            }
            let models = cached.or(fetched.as_deref()).unwrap_or_default();
            if connection.host_is("api.melious.ai")
                && settings.missing_limits()
                && !settings.model.is_empty()
            {
                auto_fill(&mut settings, connection, models);
                if settings.missing_limits() {
                    screen.busy(tr!("Загружаю параметры модели…", "Loading model details…"))?;
                    match model_details(connection, &settings.model) {
                        Ok(Some(details)) => auto_fill(&mut settings, connection, &[details]),
                        Ok(None) => {}
                        Err(error) => {
                            chat_error(history, tr!("Параметры модели", "Model details"), error)
                        }
                    }
                }
            }
            auto_fill(&mut settings, connection, models);
            message = if settings.missing_limits() {
                Some(
                    tr!(
                        "Каталог не сообщил все лимиты. Заполни пустые поля вручную.",
                        "The catalog did not report all limits. Fill in the blank fields."
                    )
                    .into(),
                )
            } else {
                settings
                    .validate_limits()
                    .err()
                    .map(|error| error.to_string())
            };
            if message.is_none() {
                screen.report(
                    tr!("Параметры модели", "Model settings preview"),
                    &model_preview(&settings, connection),
                )?;
                message = Some(
                    tr!(
                        "Проверь значения и нажми Enter, чтобы сохранить.",
                        "Review the values and press Enter to save."
                    )
                    .into(),
                );
            }
            autofill = false;
        }
        match screen.edit_model(connection, settings, editing, allow_catalog, message.take())? {
            ModelEdit::Cancel => return Ok(EditResult::Cancel),
            ModelEdit::Catalog(draft) => return Ok(EditResult::Catalog(draft)),
            ModelEdit::Auto(draft) => {
                settings = draft;
                fetched = None;
                if setup && cached.is_some_and(<[CatalogModel]>::is_empty) {
                    cached = None;
                }
                autofill = true;
            }
            ModelEdit::Save(draft) => {
                settings = draft;
                if settings.missing_limits() {
                    if cached.is_none() && fetched.is_none() {
                        autofill = true;
                    } else {
                        message =
                            Some(tr!("Заполни пустые лимиты.", "Fill in the blank limits.").into());
                    }
                    continue;
                }
                let resolved = settings.resolve(connection, DialogMode::Agent);
                if let Err(error) =
                    apply_reasoning(&resolved, &mut Value::Object(Default::default()))
                {
                    message = Some(error.to_string());
                } else {
                    if settings.name.is_empty() {
                        settings.name = settings.model.chars().take(64).collect();
                    }
                    return Ok(EditResult::Saved(settings));
                }
            }
        }
    }
}

pub(super) fn setup_model(
    screen: &mut Screen,
    connection: &Connection,
) -> Result<Option<ModelSettings>> {
    let modes = [
        tr!(
            "Выбрать модель из каталога",
            "Choose from the model catalog"
        )
        .into(),
        tr!("Ввести ID модели вручную", "Enter the model ID manually").into(),
        tr!("Пропустить настройку", "Skip setup").into(),
    ];
    let (draft, catalog) = loop {
        match screen.choose(
            tr!("3/3 · Модель по умолчанию", "3/3 · Default model"),
            &modes,
        )? {
            Some(0) => {
                let title = tr!("Каталог моделей", "Model catalog");
                screen.busy(tr!(
                    "Загружаю каталог моделей…",
                    "Loading the model catalog…"
                ))?;
                match catalog(connection) {
                    Ok(models) if !models.is_empty() => {
                        let available: Vec<_> = models.iter().collect();
                        if let Some(indices) = screen.catalog_picker(&available, false)? {
                            break (models[indices[0]].settings(connection), models);
                        }
                    }
                    Ok(_) => {
                        screen.report(
                            title,
                            tr!(
                                "Провайдер не вернул список моделей. Введи ID вручную.",
                                "The provider returned no models. Enter the model ID manually."
                            ),
                        )?;
                        break (ModelSettings::default(), Vec::new());
                    }
                    Err(error) => {
                        screen.report(
                            title,
                            &trf!(
                                "Не удалось загрузить каталог. Введи ID вручную.\n\n{error}",
                                "Couldn't load the catalog. Enter the model ID manually.\n\n{error}"
                            ),
                        )?;
                        break (ModelSettings::default(), Vec::new());
                    }
                }
            }
            Some(1) => break (ModelSettings::default(), Vec::new()),
            _ => return Ok(None),
        }
    };
    let mut errors = Vec::new();
    let choice = edit_settings(
        screen,
        connection,
        draft,
        ModelOpening::Setup(&catalog),
        &mut errors,
    )?;
    for error in &errors {
        screen.report(tr!("Каталог моделей", "Model catalog"), &error.content)?;
    }
    match choice {
        EditResult::Saved(settings) => Ok(Some(settings)),
        _ => Ok(None),
    }
}

fn already_saved(state: &State, connection: u32, id: &str, except: Option<usize>) -> bool {
    state.models.iter().enumerate().any(|(index, model)| {
        Some(index) != except && model.connection == connection && model.settings.model == id
    })
}

fn import_catalog(
    screen: &mut Screen,
    state: &State,
    connection: &Connection,
    history: &mut Vec<Message>,
) -> Result<Vec<ModelSettings>> {
    let catalog = load_catalog(screen, connection, history)?;
    let available: Vec<_> = catalog
        .iter()
        .filter(|model| !already_saved(state, connection.id, &model.id, None))
        .collect();
    if available.is_empty() {
        screen.notice(if catalog.is_empty() {
            tr!(
                "Каталог недоступен или пуст. Введи ID модели вручную через A.",
                "The catalog is unavailable or empty. Press A to enter a model ID."
            )
        } else {
            tr!(
                "Все модели каталога уже добавлены.",
                "All models from this catalog have been added."
            )
        });
        return Ok(Vec::new());
    }
    let Some(indices) = screen.catalog_picker(&available, true)? else {
        return Ok(Vec::new());
    };
    let mut drafts = Vec::new();
    for index in indices {
        let settings = available[index].settings(connection);
        if settings.missing_limits() || settings.validate_limits().is_err() {
            match edit_settings(
                screen,
                connection,
                settings,
                ModelOpening::Imported(&catalog),
                history,
            )? {
                EditResult::Saved(settings) => drafts.push(settings),
                _ => return Ok(Vec::new()),
            }
        } else {
            drafts.push(settings);
        }
    }
    let mut ids = HashSet::new();
    if drafts.iter().any(|model| {
        already_saved(state, connection.id, &model.model, None) || !ids.insert(&model.model)
    }) {
        screen.report(
            tr!("Каталог моделей", "Model catalog"),
            tr!(
                "В выбранных настройках повторяется сохранённый или добавляемый ID.",
                "The selected settings repeat a saved or newly added model ID."
            ),
        )?;
        return Ok(Vec::new());
    }
    let preview = drafts
        .iter()
        .map(|settings| model_preview(settings, connection))
        .collect::<Vec<_>>()
        .join("\n\n");
    if !screen.review(tr!("Добавить модели", "Add models"), &preview)? {
        drafts.clear();
    }
    Ok(drafts)
}

pub(super) fn add_models(state: &mut State, connection: u32, settings: Vec<ModelSettings>) -> bool {
    if settings.is_empty() {
        return false;
    }
    if state.active_model.is_none() {
        state.active_model = Some(state.models.len());
    }
    state
        .models
        .extend(settings.into_iter().map(|settings| SavedModel {
            connection,
            settings,
        }));
    true
}

pub(super) fn model_menu(
    screen: &mut Screen,
    state: &mut State,
    history: &mut Vec<Message>,
) -> Result<bool> {
    let action = screen.models(&state.connections, &state.models, state.active_model)?;
    match action {
        ModelAction::Close => Ok(false),
        ModelAction::Activate(index) => {
            let changed = state.active_model != Some(index);
            state.active_model = Some(index);
            Ok(changed)
        }
        ModelAction::Add | ModelAction::Catalog => {
            let Some(id) = choose_connection(screen, state)? else {
                return Ok(false);
            };
            let connection = state
                .connection(id)
                .context(tr!("Провайдер не найден", "Missing provider"))?
                .clone();
            if matches!(action, ModelAction::Catalog) {
                let settings = import_catalog(screen, state, &connection, history)?;
                return Ok(add_models(state, id, settings));
            }
            let mut draft = ModelSettings::default();
            loop {
                match edit_settings(screen, &connection, draft, ModelOpening::New, history)? {
                    EditResult::Saved(settings) => {
                        if already_saved(state, id, &settings.model, None) {
                            screen.report(
                                tr!("Модель", "Model"),
                                tr!(
                                    "У этого провайдера уже есть модель с таким ID.",
                                    "This provider already has a model with this ID."
                                ),
                            )?;
                            draft = settings;
                            continue;
                        }
                        return Ok(add_models(state, id, vec![settings]));
                    }
                    EditResult::Catalog(settings) => {
                        let added = import_catalog(screen, state, &connection, history)?;
                        if !added.is_empty() {
                            return Ok(add_models(state, id, added));
                        }
                        draft = settings;
                    }
                    EditResult::Cancel => return Ok(false),
                }
            }
        }
        ModelAction::Delete(index) => {
            let model = &state.models[index].settings;
            if screen.confirm(tr!("Удалить модель?", "Delete model?"), &model.name)? {
                state.remove_model(index);
                return Ok(true);
            }
            Ok(false)
        }
        ModelAction::Edit(index) | ModelAction::Auto(index) => {
            let saved = &state.models[index];
            let connection = state
                .connection(saved.connection)
                .context(tr!("Провайдер не найден", "Missing provider"))?
                .clone();
            let original = saved.settings.model.clone();
            let mut draft = saved.settings.clone();
            let mut auto = matches!(action, ModelAction::Auto(_));
            loop {
                match edit_settings(
                    screen,
                    &connection,
                    draft,
                    ModelOpening::Edit { autofill: auto },
                    history,
                )? {
                    EditResult::Saved(settings) => {
                        if settings.model != original
                            && already_saved(state, connection.id, &settings.model, Some(index))
                        {
                            screen.report(
                                tr!("Модель", "Model"),
                                tr!(
                                    "У этого провайдера уже есть модель с таким ID.",
                                    "This provider already has a model with this ID."
                                ),
                            )?;
                            draft = settings;
                            auto = false;
                            continue;
                        }
                        state.models[index].settings = settings;
                        return Ok(true);
                    }
                    _ => return Ok(false),
                }
            }
        }
        ModelAction::Test(index) => {
            let saved = &state.models[index];
            let connection = state
                .connection(saved.connection)
                .context(tr!("Провайдер не найден", "Missing provider"))?;
            let provider = saved.settings.resolve(connection, DialogMode::Chat);
            screen.busy(tr!("Проверяю модель…", "Testing the model…"))?;
            match probe_provider(&provider) {
                Ok(probe) => {
                    let result = if probe.reasoning {
                        tr!(
                            "В ответе есть признаки reasoning.",
                            "The response included evidence of reasoning."
                        )
                    } else {
                        tr!(
                            "Ответ не подтвердил reasoning.",
                            "The response did not confirm reasoning."
                        )
                    };
                    screen.report(
                        tr!("Проверка модели", "Model test"),
                        &trf!(
                            "API принял запрос за {:.1} с.\n{}",
                            "The API accepted the request in {:.1}s.\n{}",
                            probe.seconds,
                            result
                        ),
                    )?;
                }
                Err(error) => chat_error(history, tr!("Проверка модели", "Model test"), error),
            }
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{
        fs,
        io::{BufRead, BufReader, Write},
        net::TcpListener,
        thread,
        time::Instant,
    };

    #[test]
    fn legacy_encrypted_profile_keeps_accounts_models_and_dialogs() -> Result<()> {
        let active = json!({
            "name": "Second account", "chat_only": true, "anthropic": false,
            "url": "https://gateway.example/v1", "model": "shared", "key": "key-b",
            "context": 1_048_576, "input": 1_048_576, "output": 128_000,
            "vision": true, "reasoning": "max", "reasoning_api": "custom"
        });
        let same_route = json!({
            "anthropic": false, "url": "https://gateway.example/v1/",
            "model": "shared", "key": "key-a", "context": 32_000
        });
        let mut another_model = same_route.clone();
        another_model["url"] = json!("https://gateway.example/v1");
        another_model["model"] = json!("another");
        let mut another_protocol = same_route.clone();
        another_protocol["responses"] = json!(true);
        let legacy = json!({
            "provider": active, "providers": [same_route, another_model, active, another_protocol],
            "sessions": [
                {"project":"demo", "portable":true, "messages":[{"role":"user","content":"retained history"}]},
                {"project":"other", "portable":false, "messages":[]}
            ]
        });
        let work = crate::profile::restore_test_agent_state(legacy.to_string().as_bytes())?;
        let state = load_state(fs::File::open(super::super::state_path(work.path()))?)?;
        assert_eq!(state.active_model, Some(2));
        assert_eq!(state.models.len(), 4);
        assert_eq!(state.connections.len(), 3);
        assert_eq!(state.models[0].connection, state.models[1].connection);
        assert_ne!(state.models[0].connection, state.models[2].connection);
        assert_ne!(state.models[0].connection, state.models[3].connection);
        assert_eq!(
            state.models[1].settings.input,
            32_000 - super::super::DEFAULT_OUTPUT
        );
        assert!(
            state
                .sessions
                .iter()
                .all(|session| session.mode == DialogMode::Chat)
        );
        assert_eq!(state.sessions[0].messages[0].content, "retained history");
        let provider = state.active_provider(state.sessions[0].mode).unwrap();
        assert!(provider.chat_only && provider.vision);
        assert_eq!(provider.connection.key, "key-b");
        assert_eq!(provider.context_limit(), 1_048_576);
        assert_eq!(provider.input_limit(), 1_048_576);
        assert_eq!(provider.output_limit(), 128_000);
        assert_eq!(provider.reasoning, Reasoning::Max);
        assert_eq!(provider.reasoning_api, ReasoningApi::Custom);
        super::super::save(work.path(), &state)?;
        let next = fs::read_to_string(super::super::state_path(work.path()))?;
        assert_eq!(next.matches("key-a").count(), 2); // Two distinct protocols.
        assert_eq!(next.matches("key-b").count(), 1);
        let roundtrip = load_state(next.as_bytes())?;
        assert_eq!(roundtrip.sessions[0].mode, DialogMode::Chat);
        assert_eq!(roundtrip.active_model, state.active_model);
        Ok(())
    }

    #[test]
    fn manual_limits_are_independent_and_imports_keep_separate_accounts() -> Result<()> {
        let mut one = ModelSettings {
            name: "Manual".into(),
            model: "shared".into(),
            context: 1_048_576,
            input: 1_048_576,
            output: 1_048_576,
            ..ModelSettings::default()
        };
        one.validate_limits()?;
        one.output = 128_000;
        one.validate_limits()?;
        let mut invalid = one.clone();
        invalid.input += 1;
        assert!(invalid.validate_limits().is_err());
        invalid.input = one.input;
        invalid.output = one.context + 1;
        assert!(invalid.validate_limits().is_err());

        let mut state = State::default();
        for key in ["synthetic-first", "synthetic-second"] {
            state.connections.push(Connection {
                id: state.next_connection_id()?,
                key: key.into(),
                ..Connection::default()
            });
        }
        assert!(add_models(&mut state, 1, vec![one.clone()]));
        assert!(already_saved(&state, 1, "shared", None));
        assert!(!already_saved(&state, 2, "shared", None));
        let mut two = one.clone();
        two.output = 3000;
        assert!(add_models(&mut state, 2, vec![two]));
        state.active_model = Some(1);
        assert_eq!(
            state
                .active_provider(DialogMode::Agent)
                .unwrap()
                .connection
                .key,
            "synthetic-second"
        );
        state.remove_model(1);
        let restored = state.active_provider(DialogMode::Agent).unwrap();
        assert_eq!(restored.connection.key, "synthetic-first");
        assert_eq!(restored.output_limit(), one.output);
        Ok(())
    }

    #[test]
    fn catalog_autofill_preserves_manual_choices_and_unknown_limits() -> Result<()> {
        let first = parse_catalog(json!({"data":[
            {"id":"model", "context_length":128_000, "input_modalities":["image"], "display_name":"Catalog",
             "reasoningEfforts":{"off":"none", "high":"thoughtful", "ultra":"deep"}},
            {"id":"model","name":"duplicate"}, {"id":"id-only"}
        ]}))?;
        assert_eq!(first.len(), 2);
        let connection = Connection::default();
        let mut model = ModelSettings {
            name: "Manual".into(),
            model: "model".into(),
            context: 64_000,
            vision: Some(false),
            ..ModelSettings::default()
        };
        auto_fill(&mut model, &connection, &first);
        assert_eq!(model.name, "Manual");
        assert_eq!(model.vision, Some(false));
        assert_eq!(
            (model.context, model.input, model.output),
            (64_000, 64_000, 0)
        );
        assert_eq!(
            model.available_reasoning(&connection),
            [
                Reasoning::ProviderDefault,
                Reasoning::Off,
                Reasoning::High,
                Reasoning::Ultra,
            ]
        );
        let second = parse_catalog(json!({"models":{
            "model":{"limit":{"input":128_000, "output":8_192}, "reasoningEfforts":false},
            "vendor":"descriptor"
        }}))?;
        auto_fill(&mut model, &connection, &second);
        assert_eq!(
            (model.context, model.input, model.output),
            (64_000, 64_000, 8_192)
        );
        assert_eq!(model.reasoning_levels.as_ref().unwrap().len(), 3);
        let unknown = first
            .iter()
            .find(|item| item.id == "id-only")
            .unwrap()
            .settings(&connection);
        assert_eq!((unknown.context, unknown.input, unknown.output), (0, 0, 0));
        assert_eq!(unknown.vision, None);
        let output_only = second[0].settings(&connection);
        assert_eq!(
            (output_only.context, output_only.input, output_only.output),
            (0, 128_000, 8_192)
        );
        assert_eq!(
            output_only.available_reasoning(&connection),
            [Reasoning::ProviderDefault]
        );
        Ok(())
    }

    struct HttpRequest {
        line: String,
        headers: String,
        body: Vec<u8>,
    }

    fn serve_json(response: &Value) -> Result<(String, thread::JoinHandle<Result<HttpRequest>>)> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let base_url = format!("http://{}/v1", listener.local_addr()?);
        listener.set_nonblocking(true)?;
        let response = response.to_string();
        let handle = thread::spawn(move || {
            let end = Instant::now() + Duration::from_secs(5);
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < end =>
                    {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => return Err(error.into()),
                }
            };
            socket.set_nonblocking(false)?;
            socket.set_read_timeout(Some(Duration::from_secs(5)))?;
            let mut reader = BufReader::new(&mut socket);
            let mut line = String::new();
            reader.read_line(&mut line)?;
            let mut headers = String::new();
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header)? == 0 || header == "\r\n" {
                    break;
                }
                headers.push_str(&header.to_ascii_lowercase());
            }
            let size = headers
                .lines()
                .find_map(|header| header.strip_prefix("content-length:"))
                .map_or(Ok(0), |size| size.trim().parse::<usize>())?;
            let mut body = vec![0; size];
            reader.read_exact(&mut body)?;
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                response.len()
            )?;
            Ok(HttpRequest {
                line,
                headers,
                body,
            })
        });
        Ok((base_url, handle))
    }

    #[test]
    fn melious_details_fill_blanks_without_guessing_or_replacing_manual_values() -> Result<()> {
        let (url, handle) = serve_json(&json!({"id":"vendor/example", "_meta": {
            "context_length": 120_000, "max_output_tokens": 16_000, "reasoning_type":"effort"
        }}))?;
        let connection = Connection {
            url,
            key: "synthetic".into(),
            ..Connection::default()
        };
        let details = model_details(&connection, "vendor/example")?.unwrap();
        let request = handle.join().unwrap()?;
        assert!(
            request
                .line
                .starts_with("GET /v1/models/vendor%2Fexample?include_meta=true ")
        );
        assert!(request.headers.contains("authorization: bearer synthetic"));
        let mut model = ModelSettings {
            model: details.id.clone(),
            output: 8_000,
            ..ModelSettings::default()
        };
        auto_fill(&mut model, &connection, &[details]);
        assert_eq!(
            (model.context, model.input, model.output),
            (120_000, 120_000, 8_000)
        );
        assert!(model.reasoning_levels.is_none());
        Ok(())
    }

    #[test]
    fn enabled_reasoning_round_trips_without_changing_api_mappings() -> Result<()> {
        let connection = Connection::default();
        let model = ModelSettings {
            enabled_reasoning: Some(vec![Reasoning::Low, Reasoning::High]),
            reasoning_levels: Some(vec![
                ReasoningLevel {
                    level: Reasoning::Low,
                    wire: Some("low".into()),
                },
                ReasoningLevel {
                    level: Reasoning::High,
                    wire: Some("high".into()),
                },
                ReasoningLevel {
                    level: Reasoning::Max,
                    wire: Some("max".into()),
                },
            ]),
            ..ModelSettings::default()
        };
        let saved: ModelSettings = serde_json::from_value(serde_json::to_value(&model)?)?;
        assert_eq!(
            saved.available_reasoning(&connection),
            [Reasoning::ProviderDefault, Reasoning::Low, Reasoning::High]
        );
        assert_eq!(saved.reasoning_levels, model.reasoning_levels);
        Ok(())
    }

    #[test]
    fn removing_one_preset_level_keeps_the_other_levels() {
        let connection = Connection::default();
        let mut model = ModelSettings::default();
        let mut expected = model.available_reasoning(&connection);
        expected.retain(|level| *level != Reasoning::Medium);
        model.set_reasoning_mapping(&connection, Reasoning::Medium, None);
        assert_eq!(model.available_reasoning(&connection), expected);
    }

    #[test]
    fn catalog_requests_use_selected_account_and_both_formats() -> Result<()> {
        for (protocol, response, auth) in [
            (
                Protocol::ChatCompletions,
                json!({"data":[{"id":"shared"}]}),
                "authorization: bearer account-one",
            ),
            (
                Protocol::AnthropicMessages,
                json!({"models":{"shared":{"contextWindow":100_000}}}),
                "x-api-key: account-two",
            ),
        ] {
            let (url, handle) = serve_json(&response)?;
            let connection = Connection {
                url,
                protocol,
                key: auth.split_whitespace().last().unwrap().into(),
                ..Connection::default()
            };
            let models = catalog(&connection)?;
            assert_eq!(models[0].id, "shared");
            let captured = handle.join().unwrap()?;
            assert!(captured.line.starts_with("GET /v1/models "));
            assert!(captured.headers.contains(auth));
            assert!(captured.body.is_empty());
        }
        Ok(())
    }

    #[test]
    fn model_probe_sends_one_minimal_request_and_reports_reasoning_separately() -> Result<()> {
        for (protocol, route, response, pointer, wire, reasoned) in [
            (
                Protocol::ChatCompletions,
                "chat/completions",
                json!({
                    "choices":[{"message":{"content":"357","reasoning_content":"synthetic reasoning"}}]
                }),
                "/reasoning_effort",
                json!("medium"),
                true,
            ),
            (
                Protocol::Responses,
                "responses",
                json!({
                    "output":[{"type":"message","content":[{"text":"357"}]}]
                }),
                "/reasoning/effort",
                json!("medium"),
                false,
            ),
            (
                Protocol::AnthropicMessages,
                "messages",
                json!({
                    "content":[{"type":"thinking"},{"type":"text","text":"357"}]
                }),
                "/thinking/budget_tokens",
                json!(2048),
                true,
            ),
        ] {
            let (url, handle) = serve_json(&response)?;
            let connection = Connection {
                url,
                protocol,
                ..Connection::default()
            };
            let settings = ModelSettings {
                model: "shared".into(),
                context: 100_000,
                input: 100_000,
                output: 8_192,
                reasoning: Reasoning::Medium,
                ..ModelSettings::default()
            };
            let probe = probe_provider(&settings.resolve(&connection, DialogMode::Chat))?;
            assert_eq!(probe.reasoning, reasoned);
            let captured = handle.join().unwrap()?;
            assert!(captured.line.starts_with(&format!("POST /v1/{route} ")));
            let sent: Value = serde_json::from_slice(&captured.body)?;
            assert_eq!(sent.pointer(pointer), Some(&wire));
            assert!(
                sent.get("tools").is_none()
                    && sent.get("system").is_none()
                    && sent.get("instructions").is_none()
            );
            if protocol == Protocol::Responses {
                assert!(sent["input"].is_string());
            } else {
                assert_eq!(sent["messages"].as_array().unwrap().len(), 1);
            }
        }
        Ok(())
    }
}
