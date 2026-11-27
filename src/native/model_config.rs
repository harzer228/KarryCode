use super::{
    DialogMode, Provider, Reasoning, ReasoningApi, State, apply_reasoning, endpoint,
    models::{Connection, MAX_MODEL_TOKENS, ModelSettings, Protocol, ReasoningLevel, SavedModel},
    settings::Settings,
    ui::Screen,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};

const MAX_FILE: u64 = 256 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Choice {
    pub provider: String,
    pub model: String,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Auth {
    #[default]
    Profile,
    None,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Route {
    name: String,
    url: String,
    protocol: Protocol,
    #[serde(default)]
    auth: Auth,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Model {
    provider: String,
    id: String,
    #[serde(default)]
    name: String,
    context: u32,
    input: Option<u32>,
    output: u32,
    vision: Option<bool>,
    #[serde(default)]
    reasoning: Reasoning,
    #[serde(default)]
    reasoning_api: ReasoningApi,
    reasoning_levels: Option<Vec<ReasoningLevel>>,
    enabled_reasoning: Option<Vec<Reasoning>>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileFile {
    version: u8,
    default: Option<Choice>,
    providers: BTreeMap<String, Route>,
    #[serde(default)]
    models: Vec<Model>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectFile {
    version: u8,
    provider: String,
    model: String,
    context: Option<u32>,
    input: Option<u32>,
    output: Option<u32>,
    vision: Option<bool>,
    reasoning: Option<Reasoning>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct ProjectApproval {
    revision: String,
    file: ProjectFile,
}

fn alias(state: &State, id: u32) -> String {
    state
        .settings
        .model_aliases
        .iter()
        .find_map(|(name, &value)| (value == id).then(|| name.clone()))
        .unwrap_or_else(|| {
            let mut name = format!("p-{id}");
            while state.settings.model_aliases.contains_key(&name) {
                name.push_str("-local");
            }
            name
        })
}

pub(super) fn choice_at(state: &State, index: usize) -> Option<Choice> {
    let model = state.models.get(index)?;
    Some(Choice {
        provider: alias(state, model.connection),
        model: model.settings.model.clone(),
    })
}

fn index_of(state: &State, choice: &Choice) -> Result<usize> {
    state
        .models
        .iter()
        .position(|model| {
            choice.model == model.settings.model
                && choice.provider == alias(state, model.connection)
        })
        .with_context(|| format!("Unknown model {}/{}", choice.provider, choice.model))
}

pub(super) fn selected_index(state: &State, choice: Option<&Choice>) -> Result<Option<usize>> {
    choice
        .map(|choice| index_of(state, choice))
        .transpose()
        .map(|index| index.or(state.active_model))
}

fn identifier(value: &str, max: usize) -> bool {
    (1..=max).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

impl Model {
    fn settings(self) -> Result<ModelSettings> {
        ensure!(
            !self.id.trim().is_empty()
                && self.id.chars().count() <= 160
                && self.name.chars().count() <= 64,
            "Invalid model ID or name"
        );
        let settings = ModelSettings {
            name: if self.name.is_empty() {
                self.id.chars().take(64).collect()
            } else {
                self.name
            },
            model: self.id,
            context: self.context,
            input: self.input.unwrap_or(self.context),
            output: self.output,
            vision: self.vision,
            reasoning: self.reasoning,
            reasoning_api: self.reasoning_api,
            reasoning_levels: self.reasoning_levels,
            enabled_reasoning: self.enabled_reasoning,
        };
        ensure!(
            (1..=MAX_MODEL_TOKENS).contains(&settings.context)
                && settings.input > 0
                && settings.output > 0,
            "Context, input and output must be positive token limits"
        );
        settings.validate_limits()?;
        Ok(settings)
    }
}

impl ProfileFile {
    fn export(state: &State) -> Self {
        Self {
            version: 1,
            default: state.active_model.and_then(|index| choice_at(state, index)),
            providers: state
                .connections
                .iter()
                .map(|route| {
                    (
                        alias(state, route.id),
                        Route {
                            name: route.name.clone(),
                            url: route.url.clone(),
                            protocol: route.protocol,
                            auth: if route.key.is_empty()
                                && !state.settings.needs_model_key.contains(&route.id)
                            {
                                Auth::None
                            } else {
                                Auth::Profile
                            },
                        },
                    )
                })
                .collect(),
            models: state
                .models
                .iter()
                .map(|model| {
                    let settings = &model.settings;
                    Model {
                        provider: alias(state, model.connection),
                        id: settings.model.clone(),
                        name: settings.name.clone(),
                        context: settings.context,
                        input: Some(settings.input),
                        output: settings.output,
                        vision: settings.vision,
                        reasoning: settings.reasoning,
                        reasoning_api: settings.reasoning_api,
                        reasoning_levels: settings.reasoning_levels.clone(),
                        enabled_reasoning: settings.enabled_reasoning.clone(),
                    }
                })
                .collect(),
        }
    }

    fn apply(self, state: &mut State) -> Result<bool> {
        ensure!(self.version == 1, "Unsupported models.toml version");
        ensure!(
            self.providers.len() <= 128 && self.models.len() <= 1024,
            "Too many providers or models"
        );
        let mut next = state.next_connection_id()?;
        let mut ids = BTreeMap::new();
        let mut unresolved = BTreeSet::new();
        let mut connections = BTreeMap::new();
        let mut settings = state.settings.clone();
        for (name, route) in self.providers {
            ensure!(identifier(&name, 40), "Invalid provider ID {name}");
            ensure!(
                !route.name.trim().is_empty() && route.name.chars().count() <= 64,
                "Invalid name for provider {name}"
            );
            let url = endpoint(&route.url).with_context(|| format!("providers.{name}.url"))?;
            let previous = state
                .connections
                .iter()
                .find(|route| alias(state, route.id) == name);
            let id = if let Some(previous) = previous {
                previous.id
            } else {
                let id = next;
                next = next.checked_add(1).context("No more provider IDs")?;
                id
            };
            let bound = previous.filter(|old| old.url == url && old.protocol == route.protocol);
            let key = if route.auth == Auth::Profile {
                bound.map(|old| old.key.clone()).unwrap_or_default()
            } else {
                String::new()
            };
            if route.auth == Auth::Profile && key.is_empty() {
                unresolved.insert(id);
            }
            if bound.is_none_or(|old| old.key != key) {
                settings.provider_limits.remove(&id);
            }
            ids.insert(name, id);
            connections.insert(
                id,
                Connection {
                    id,
                    name: route.name,
                    url,
                    protocol: route.protocol,
                    key,
                },
            );
        }
        let mut seen = BTreeSet::new();
        let mut models = Vec::with_capacity(self.models.len());
        for entry in self.models {
            let id = *ids
                .get(&entry.provider)
                .with_context(|| format!("Unknown provider {}", entry.provider))?;
            let label = format!("{}/{}", entry.provider, entry.id);
            let model = entry
                .settings()
                .with_context(|| format!("models.{label}"))?;
            ensure!(
                seen.insert((id, model.model.clone())),
                "Duplicate model {label}"
            );
            apply_reasoning(
                &model.resolve(&connections[&id], DialogMode::Agent),
                &mut json!({}),
            )
            .with_context(|| format!("models.{label}.reasoning"))?;
            models.push(SavedModel {
                connection: id,
                settings: model,
            });
        }
        let active = self
            .default
            .map(|choice| {
                let id = ids
                    .get(&choice.provider)
                    .context("Unknown default provider")?;
                models
                    .iter()
                    .position(|model| {
                        model.connection == *id && model.settings.model == choice.model
                    })
                    .context("Unknown default model")
            })
            .transpose()?;
        let removed: Vec<_> = state
            .connections
            .iter()
            .filter_map(|connection| {
                (!connections.contains_key(&connection.id)).then_some(connection.id)
            })
            .collect();
        for id in removed {
            settings.remove_connection(id);
        }
        let connections: Vec<_> = connections.into_values().collect();
        let changed = state.connections != connections
            || state.models != models
            || state.active_model != active
            || state.settings.model_aliases != ids
            || state.settings.needs_model_key != unresolved;
        settings.model_aliases = ids;
        settings.needs_model_key = unresolved;
        state.settings = settings;
        state.connections = connections;
        state.models = models;
        state.active_model = active;
        Ok(changed)
    }
}

pub(super) struct Snapshot {
    connections: Vec<Connection>,
    models: Vec<SavedModel>,
    active: Option<usize>,
    settings: Settings,
}

impl Snapshot {
    pub fn capture(state: &State) -> Self {
        Self {
            connections: state.connections.clone(),
            models: state.models.clone(),
            active: state.active_model,
            settings: state.settings.clone(),
        }
    }

    pub fn restore(self, state: &mut State) {
        state.connections = self.connections;
        state.models = self.models;
        state.active_model = self.active;
        state.settings = self.settings;
    }
}

pub(super) struct ModelFiles {
    path: PathBuf,
    applied: Option<String>,
}

impl ModelFiles {
    pub fn new(root: &Path) -> Self {
        Self {
            path: root.join("models.toml"),
            applied: None,
        }
    }

    pub fn refresh(&mut self, state: &mut State) -> Result<bool> {
        let Some((data, hash)) = bytes(&self.path)? else {
            self.applied = None;
            if !state.connections.is_empty() {
                self.write(state)?;
            }
            return Ok(false);
        };
        if self.applied.as_deref() == Some(&hash) {
            return Ok(false);
        }
        let changed = parse::<ProfileFile>(&self.path, &data)?
            .apply(state)
            .with_context(|| self.path.display().to_string())?;
        self.applied = Some(hash);
        Ok(changed)
    }

    pub fn write(&mut self, state: &State) -> Result<()> {
        let before = bytes(&self.path)?.map(|(_, hash)| hash);
        ensure!(
            before == self.applied,
            "models.toml changed on disk. Reload it through /config and retry"
        );
        let data = toml::to_string_pretty(&ProfileFile::export(state))?;
        ensure!(data.len() as u64 <= MAX_FILE, "models.toml exceeds 256 KiB");
        let parent = self
            .path
            .parent()
            .context("Missing models.toml directory")?;
        let mut draft = tempfile::NamedTempFile::new_in(parent)?;
        draft.write_all(data.as_bytes())?;
        draft.as_file().sync_all()?;
        ensure!(
            bytes(&self.path)?.map(|(_, hash)| hash) == before,
            "models.toml changed while saving"
        );
        if before.is_none() {
            draft.persist_noclobber(&self.path)?;
        } else {
            draft.persist(&self.path)?;
        }
        self.applied = Some(hash(data.as_bytes()));
        Ok(())
    }

    pub fn location(&self) -> &Path {
        &self.path
    }
}

pub(super) fn project_path(directory: &Path) -> PathBuf {
    directory.join(".karrycode/models.toml")
}

fn project(directory: &Path) -> Result<Option<(ProjectFile, String)>> {
    let path = project_path(directory);
    if let Some(parent) = path.parent()
        && let Ok(metadata) = fs::symlink_metadata(parent)
    {
        ensure!(
            metadata.is_dir() && !crate::safe_files::is_link(&metadata),
            "Linked project configuration directory"
        );
    }
    let Some((data, hash)) = bytes(&path)? else {
        return Ok(None);
    };
    let file = parse::<ProjectFile>(&path, &data)?;
    ensure!(file.version == 1, "{}: unsupported version", path.display());
    Ok(Some((file, hash)))
}

fn resolve_project(state: &State, file: ProjectFile, mode: DialogMode) -> Result<Provider> {
    let choice = Choice {
        provider: file.provider,
        model: file.model,
    };
    let index = index_of(state, &choice)?;
    let saved = &state.models[index];
    let route = state
        .connection(saved.connection)
        .context("Unknown project provider")?;
    let mut model = saved.settings.clone();
    let context = file.context.unwrap_or(model.context);
    let input = file.input.unwrap_or(model.input.min(context));
    let output = file.output.unwrap_or(model.output.min(context));
    ensure!(
        (1..=model.context).contains(&context),
        "context exceeds the profile model limit"
    );
    ensure!(
        (1..=model.input.min(context)).contains(&input),
        "input exceeds the project context or profile input limit"
    );
    ensure!(
        (1..=model.output.min(context)).contains(&output),
        "output exceeds the project context or profile output limit"
    );
    model.context = context;
    model.input = input;
    model.output = output;
    if let Some(vision) = file.vision {
        ensure!(
            !vision || model.vision == Some(true),
            "The profile model must already support images"
        );
        model.vision = Some(vision);
    }
    if let Some(reasoning) = file.reasoning {
        model.reasoning = reasoning;
    }
    ready(state, model.resolve(route, mode))
}

fn ready(state: &State, provider: Provider) -> Result<Provider> {
    ensure!(
        !state
            .settings
            .needs_model_key
            .contains(&provider.connection.id),
        "{}",
        tr!(
            "Добавь ключ для подключения через /provider или выбери режим без ключа.",
            "Add a key in /provider for this route or explicitly choose keyless access."
        )
    );
    apply_reasoning(&provider, &mut json!({}))?;
    Ok(provider)
}

fn effective(state: &State, choice: Option<&Choice>, mode: DialogMode) -> Result<Option<Provider>> {
    if let Some(choice) = choice {
        let saved = &state.models[index_of(state, choice)?];
        let route = state
            .connection(saved.connection)
            .context("Unknown selected provider")?;
        return ready(state, saved.settings.resolve(route, mode)).map(Some);
    }
    state
        .active_provider(mode)
        .map(|provider| ready(state, provider))
        .transpose()
}

#[derive(Default)]
pub(super) struct ProjectModels {
    pub warning: Option<String>,
}

impl ProjectModels {
    pub fn resolve(
        &mut self,
        state: &State,
        directory: &Path,
        choice: Option<&Choice>,
        mode: DialogMode,
    ) -> Result<Option<Provider>> {
        let approval = state
            .settings
            .approved_model_projects
            .get(directory.to_string_lossy().as_ref());
        self.warning = None;
        let selected = match project(directory) {
            Ok(Some((_, revision))) => {
                if approval.is_none_or(|item| item.revision != revision) {
                    self.warning = Some(trf!(
                        "{} изменён. Проверь и одобри его через /config.",
                        "{} changed. Review and approve it through /config.",
                        project_path(directory).display()
                    ));
                }
                approval
            }
            Ok(None) => None,
            Err(error) => {
                self.warning = Some(format!("{error:#}"));
                approval
            }
        };
        if let Some(item) = selected.filter(|_| choice.is_none()) {
            return resolve_project(state, item.file.clone(), mode)
                .with_context(|| project_path(directory).display().to_string())
                .map(Some);
        }
        effective(state, choice, mode)
    }
}

pub(super) fn accept_project(
    screen: &mut Screen,
    state: &mut State,
    directory: &Path,
) -> Result<bool> {
    let Some((file, revision)) = project(directory)? else {
        return Ok(false);
    };
    let workspace = directory.to_string_lossy().into_owned();
    if state
        .settings
        .approved_model_projects
        .get(&workspace)
        .is_some_and(|item| item.revision == revision)
    {
        return Ok(false);
    }
    let model = resolve_project(state, file.clone(), DialogMode::Agent)
        .with_context(|| project_path(directory).display().to_string())?;
    let text = trf!(
        "{}\nМодель: {} / {}\nАдрес: {}\nВход: {} · Выход: {} · Reasoning: {}",
        "{}\nModel: {} / {}\nEndpoint: {}\nInput: {} · Output: {} · Reasoning: {}",
        project_path(directory).display(),
        model.connection.name,
        model.model,
        model.connection.url,
        model.input_limit(),
        model.output_limit(),
        model.reasoning.identifier()
    );
    if !screen.confirm(
        tr!(
            "Применить конфиг проекта?",
            "Apply project model configuration?"
        ),
        &text,
    )? {
        return Ok(false);
    }
    ensure!(
        bytes(&project_path(directory))?.is_some_and(|(_, value)| value == revision),
        "Project configuration changed during review"
    );
    state
        .settings
        .approved_model_projects
        .insert(workspace, ProjectApproval { revision, file });
    Ok(true)
}

pub(super) fn unapproved_project(state: &State, directory: &Path) -> Result<bool> {
    let workspace = directory.to_string_lossy();
    Ok(project(directory)?.is_some_and(|(_, hash)| {
        state
            .settings
            .approved_model_projects
            .get(workspace.as_ref())
            .is_none_or(|item| item.revision != hash)
    }))
}

pub(super) fn hash(data: &[u8]) -> String {
    format!("{:x}", Sha256::digest(data))
}

fn bytes(path: &Path) -> Result<Option<(Vec<u8>, String)>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| path.display().to_string()),
    };
    ensure!(
        metadata.is_file() && !crate::safe_files::is_link(&metadata) && metadata.len() <= MAX_FILE,
        "{}: expected a regular file up to 256 KiB",
        path.display()
    );
    let mut data = Vec::new();
    File::open(path)?
        .take(MAX_FILE + 1)
        .read_to_end(&mut data)?;
    ensure!(
        data.len() as u64 <= MAX_FILE,
        "{}: file exceeds 256 KiB",
        path.display()
    );
    let revision = hash(&data);
    Ok(Some((data, revision)))
}

fn parse<T: DeserializeOwned>(path: &Path, data: &[u8]) -> Result<T> {
    // TOML errors can include a source excerpt. Only show the location so a key
    // accidentally pasted into a config never reaches the chat or stderr.
    let text =
        std::str::from_utf8(data).with_context(|| format!("{}: expected UTF-8", path.display()))?;
    toml::from_str(text).map_err(|error| {
        let (line, column) = error
            .span()
            .and_then(|span| text.get(..span.start))
            .map(|prefix| {
                (
                    prefix.bytes().filter(|&byte| byte == b'\n').count() + 1,
                    prefix.rsplit('\n').next().unwrap_or("").chars().count() + 1,
                )
            })
            .unwrap_or((1, 1));
        anyhow::anyhow!(
            "{}:{line}:{column}: invalid TOML or unexpected field",
            path.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changing_the_endpoint_does_not_rebind_the_original_secret() -> Result<()> {
        let mut state = State::default();
        state.connections.push(Connection {
            id: 3,
            name: "Gateway".into(),
            url: "https://example.org/v1".into(),
            protocol: Protocol::ChatCompletions,
            key: "test-secret".into(),
        });
        let mut file = ProfileFile::export(&state);
        let exported = toml::to_string(&file)?;
        assert!(!exported.contains("test-secret"));
        file.providers.get_mut("p-3").unwrap().url = "https://elsewhere.example/v1".into();
        file.apply(&mut state)?;
        assert_eq!(state.connections[0].id, 3);
        assert!(state.connections[0].key.is_empty());
        assert!(state.settings.needs_model_key.contains(&3));
        Ok(())
    }

    #[test]
    fn failed_configuration_does_not_replace_the_last_good_model() -> Result<()> {
        let root = tempfile::tempdir()?;
        let mut files = ModelFiles::new(root.path());
        let mut state = State::default();
        fs::write(files.location(), "version=1\n[providers]\n")?;
        assert!(!files.refresh(&mut state)?);
        fs::write(files.location(), "version=1\napi_key='synthetic'\n")?;
        let error = files.refresh(&mut state).unwrap_err();
        assert!(!error.to_string().contains("synthetic"));
        assert!(files.write(&state).is_err());
        Ok(())
    }

    #[test]
    fn installed_project_choice_survives_an_invalid_edit_and_respects_chat_precedence() -> Result<()>
    {
        let root = tempfile::tempdir()?;
        let path = project_path(root.path());
        fs::create_dir_all(path.parent().unwrap())?;
        let mut state = State::default();
        toml::from_str::<ProfileFile>(
            r#"
version = 1
[default]
provider = "first"
model = "shared"
[providers.first]
name = "First"
url = "http://127.0.0.1:8000/v1"
protocol = "chat_completions"
auth = "none"
[providers.second]
name = "Second"
url = "http://127.0.0.1:9000/v1"
protocol = "chat_completions"
auth = "none"
[[models]]
provider = "first"
id = "shared"
context = 1048576
input = 1048576
output = 1048576
[[models]]
provider = "second"
id = "shared"
context = 32000
output = 4000
"#,
        )?
        .apply(&mut state)?;
        fs::write(
            &path,
            "version=1\nprovider='second'\nmodel='shared'\noutput=1000",
        )?;
        let (file, revision) = project(root.path())?.unwrap();
        assert!(unapproved_project(&state, root.path())?);
        state.settings.approved_model_projects.insert(
            root.path().to_string_lossy().into_owned(),
            ProjectApproval { revision, file },
        );
        let snapshot = serde_json::to_vec(&state)?;
        let state: State = serde_json::from_slice(&snapshot)?;
        let mut model = ProjectModels::default();
        assert_eq!(
            model
                .resolve(&state, root.path(), None, DialogMode::Agent)?
                .unwrap()
                .output_limit(),
            1000
        );
        fs::write(path, "invalid [ TOML")?;
        assert_eq!(
            model
                .resolve(&state, root.path(), None, DialogMode::Agent)?
                .unwrap()
                .output_limit(),
            1000
        );
        assert!(model.warning.is_some());
        assert!(unapproved_project(&state, root.path()).is_err());
        let explicit = Choice {
            provider: "first".into(),
            model: "shared".into(),
        };
        let provider = model
            .resolve(&state, root.path(), Some(&explicit), DialogMode::Agent)?
            .unwrap();
        assert_eq!(provider.connection.name, "First");
        assert_eq!(provider.output_limit(), 1_048_576);
        Ok(())
    }
}
