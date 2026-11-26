#[cfg(windows)]
use anyhow::bail;
use anyhow::{Context, Result, ensure};
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
mod ui;
mod web;

use actions::{CommandRequest, command_in_chat, request_permissions, review_changes};
use agent::prompt_extra;
use commands::{
    choose_reasoning, edit_agent_settings, init_document, live_reasoning, move_session,
    no_model_label, project_location, run_web_search, search_menu, set_reasoning, show_web_search,
};
use files::{attach_files, list_files, read_file, safe_path, search_project};
use models::{Connection, Protocol, ReasoningLevel};
use permissions::DialogMode;
use provider::{
    Delta, InterruptedStream, RequestMode, Visual, attach_image, input_estimate,
    prepare_work_request, request,
};
use subagents::{parse_subagent_tasks, run_subagent_group};

fn default_context() -> u32 {
    128_000
}

const DEFAULT_OUTPUT: u32 = 4096;
const MAX_TOOL_ROUNDS: usize = 96;

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
fn sync_limits(screen: &mut ui::Screen, provider: &Provider) {
    screen.set_limits(
        provider.context_limit(),
        provider.input_limit(),
        provider.output_limit(),
        provider.reasoning,
    );
}

pub fn run(
    profile: &mut crate::profile::Profile,
    root: &Path,
    workspace: &mut PathBuf,
    task: Option<String>,
) -> Result<()> {
    crate::platform::require_standard_user()?;
    let path = state_path(profile.work.path());
    let mut state: State = if path.exists() {
        models::load_state(fs::File::open(path)?.take(32 * 1024 * 1024))?
    } else {
        State {
            web: web::Settings::builtin(),
            ..State::default()
        }
    };
    ensure!(
        task.is_none() || state.active_provider(DialogMode::Agent).is_some(),
        "{}",
        crate::setup_first()
    );
    #[cfg(windows)]
    tasks::suppress_loader_dialogs();
    let mut screen = match task {
        Some(task) => ui::Screen::headless(task),
        None => ui::Screen::open()?,
    };
    let mut theme = ui::theme::Theme::load(profile.work.path());
    screen.set_theme(theme);
    let mut current_directory = workspace.canonicalize()?;
    let directory = current_directory.as_path();
    let bundle = root.canonicalize()?;
    if state.connections.is_empty()
        && let Some(setup) = onboarding::run(&mut screen, &state)?
    {
        setup.apply(&mut state);
        save(profile.work.path(), &state)?;
        profile.save()?;
    }
    let (project, portable) = project_location(&bundle, directory);
    let index = state
        .sessions
        .iter()
        .rposition(|session| session.project == project && session.portable == portable);
    let mut index = index.unwrap_or_else(|| {
        state.sessions.push(Session {
            project,
            portable,
            messages: vec![],
            mode: DialogMode::Agent,
        });
        state.sessions.len() - 1
    });
    let mut batch_undo: Option<changes::Batch> = None;
    let mut browser_enabled = true;
    #[cfg(windows)]
    let mut desktop_access = desktop::Access::default();
    let mut browser = None;
    let mut preview: Option<preview::Preview> = None;
    #[cfg(windows)]
    let mut desktop: Option<desktop::Session> = None;
    let mut images = Vec::new();
    let mut resume_turn = false;
    let mut wire = std::collections::BTreeMap::new();
    // Headless runs behave like /temporary: a fresh Agent chat that is never saved.
    let mut temporary = screen.is_headless();
    let mut history = if temporary {
        Vec::new()
    } else {
        state.sessions[index].messages.clone()
    };
    if !temporary {
        screen.set_dialog_mode(state.sessions[index].mode);
    }
    let mut diagnose = false;
    let mut planning = false;
    let mut proposed_plan = None::<String>;
    let mut skills = skills::discover(directory, root);
    if mcp::configured() {
        screen.busy(tr!("Запускаю MCP-серверы…", "Starting MCP servers…"))?;
    }
    let mut hub = mcp::Hub::load(directory);
    if state.connections.is_empty() {
        screen.localized_notice(
            "Сначала добавь подключение в /provider, затем модель в /model.",
            "Add a connection in /provider, then add a model in /model.",
        );
    } else if hub.statuses().is_empty() && hub.errors().is_empty() {
        screen.localized_notice(
            "Готов. /help — команды. История сохраняется на компьютере.",
            "Ready. /help for commands. History is saved locally.",
        );
    } else {
        let mut summary = hub.summary();
        for error in hub.errors() {
            summary.push_str(" · ");
            summary.push_str(error);
        }
        screen.notice(&summary);
    }
    loop {
        let directory = current_directory.as_path();
        if !temporary && state.sessions[index].mode != screen.dialog_mode() {
            state.sessions[index].mode = screen.dialog_mode();
            save(profile.work.path(), &state)?;
            profile.save()?;
        }
        let label = if let Some(provider) = state.active_provider(screen.dialog_mode()) {
            sync_limits(&mut screen, &provider);
            let extra = if planning {
                String::new()
            } else {
                prompt_extra(&skills, &hub, directory, screen.permission_mode())
            };
            let request = prepare_work_request(
                &provider,
                &history,
                state.request_mode(&provider, diagnose, planning),
                &extra,
                &wire,
            );
            screen.set_input_estimate(input_estimate(&request));
            format!(
                "{} / {}",
                provider.connection.display_name(&state.connections),
                provider.model
            )
        } else {
            screen.set_limits(0, 0, 0, Reasoning::ProviderDefault);
            screen.set_input_estimate(None);
            no_model_label(&state).into()
        };
        let Some(input) = screen.input(&history, &label, directory, diagnose)? else {
            break;
        };
        let submitted_at = std::time::Instant::now();
        let command = if input.contains('\n') {
            ""
        } else {
            input.trim_end()
        };
        if planning && !permissions::plan_allows_command(command) {
            screen.notice(tr!(
                "Команда недоступна в /plan.",
                "Command unavailable in /plan."
            ));
            continue;
        }
        let mut implementation = None;
        match command {
            "/setting" => {
                #[cfg(windows)]
                let paused = desktop_access.paused();
                #[cfg(not(windows))]
                let paused = false;
                match settings::menu(
                    &mut screen,
                    &mut state,
                    &mut theme,
                    profile.work.path(),
                    root,
                    paused,
                    |screen, state| {
                        let label = if state.active_provider(screen.dialog_mode()).is_some() {
                            &label
                        } else {
                            no_model_label(state)
                        };
                        screen.redraw_language(&history, label, directory)
                    },
                ) {
                    Ok(resume_desktop) => {
                        #[cfg(windows)]
                        if resume_desktop || !state.settings.desktop {
                            desktop_access.resume();
                            desktop = None;
                            screen.desktop_active(false);
                            images.clear();
                        }
                        #[cfg(not(windows))]
                        let _ = resume_desktop;
                        profile.save()?;
                    }
                    Err(error) => screen.notice(&format!("{error:#}")),
                }
                continue;
            }
            command if command == "/cd" || command.starts_with("/cd ") => {
                let target = command.strip_prefix("/cd").unwrap_or_default().trim();
                let selected = if target.is_empty() {
                    screen.pick_directory(directory, root, &profile.recent)
                } else {
                    crate::folders::resolve_directory(directory, target).map(Some)
                };
                match selected {
                    Ok(Some(next)) if next.starts_with(&bundle) => {
                        screen.notice(tr!(
                            "Выбери папку вне профиля KarryCode.",
                            "Choose a folder outside the KarryCode profile."
                        ));
                    }
                    Ok(Some(next)) if next != directory => {
                        if !temporary {
                            let old_index = index;
                            let previous = state.sessions[index].clone();
                            let recent = profile.recent.clone();
                            profile.recent.remember(root, &next);
                            index = move_session(&mut state.sessions, index, &bundle, &next);
                            if let Err(error) =
                                save(profile.work.path(), &state).and_then(|()| profile.save())
                            {
                                state.sessions.pop();
                                state.sessions.insert(old_index, previous);
                                index = old_index;
                                profile.recent = recent;
                                // Restore the in-memory profile's state file if snapshot creation failed.
                                save(profile.work.path(), &state)?;
                                screen.notice(&format!("{error:#}"));
                                continue;
                            }
                            *workspace = next.clone();
                        }
                        current_directory = next;
                        skills = skills::discover(&current_directory, root);
                        hub = mcp::Hub::load(&current_directory);
                        preview = None;
                        proposed_plan = None;
                        batch_undo = None;
                        resume_turn = false;
                        images.clear();
                        history.push(Message {
                            role: "event".into(),
                            content: trf!(
                                "Папка проекта · {}",
                                "Project folder · {}",
                                crate::folders::display(&current_directory)
                            ),
                            ..Default::default()
                        });
                        screen.notice(tr!("Папка проекта изменена.", "Project folder changed."));
                    }
                    Ok(_) => {}
                    Err(error) => screen.notice(&format!("{error:#}")),
                }
                continue;
            }
            "/agents" => {
                screen.report(tr!("Встроенные саб-агенты", "Built-in subagents"), tr!(
                    "Исследователь — изучает назначенную тему и указывает файлы и строки.\nРевьюер — независимо ищет дефекты в выбранной области.\n\nПо команде delegate можно запустить группу задач параллельно в пределах лимита из /setting после подтверждения. Доступ только на чтение к выбранному проекту; результаты отправляются активному провайдеру. Без shell, MCP, записи и вложенных агентов.",
                    "Researcher — investigates the assigned question and cites files and lines.\nReviewer — independently looks for defects in the assigned scope.\n\nThe delegate tool can run a group of tasks in parallel up to the limit in /setting after confirmation. Workers get read-only access to the selected project; findings are sent to the active provider. No shell, MCP, writes, or nested agents."
                ))?;
                continue;
            }
            "/theme" => {
                if let Some(next) = screen.choose_theme(theme)? {
                    match next.save(profile.work.path()).and_then(|()| profile.save()) {
                        Ok(()) => {
                            theme = next;
                            screen.set_theme(theme);
                        }
                        Err(error) => screen.notice(&format!("{error:#}")),
                    }
                }
                continue;
            }
            "/header" => {
                screen.toggle_header();
                continue;
            }
            "/plan" | "/plan cancel" if planning => {
                proposed_plan = None;
                planning = false;
                resume_turn = false;
                screen.set_planning(false);
                screen.notice(tr!("План не принят.", "Plan not accepted."));
                continue;
            }
            "/plan" => {
                proposed_plan = None;
                resume_turn = false;
                images.clear();
                planning = true;
                diagnose = false;
                screen.set_planning(true);
                screen.notice("");
                continue;
            }
            "/quit" | "/exit" => break,
            "/undo" => {
                if diagnose {
                    screen.notice(tr!(
                        "Переключись в /code для отмены правок.",
                        "Switch to /code to revert changes."
                    ));
                } else if let Some(change) = batch_undo.take() {
                    if screen.review(
                        tr!("Отменить изменения", "Revert changes"),
                        &change.preview(),
                    )? {
                        match change.apply() {
                            Ok(_) => screen.notice(tr!("Изменения отменены.", "Changes reverted.")),
                            Err(error) => screen.notice(&error.to_string()),
                        }
                    } else {
                        batch_undo = Some(change);
                    }
                }
                continue;
            }
            "/help" => {
                screen.help()?;
                continue;
            }
            "/model" | "/models" | "/provider" | "/providers" => {
                let before = state.active_provider(DialogMode::Agent);
                if edit_agent_settings(&mut screen, &mut state, &mut history, command)? {
                    if state.active_provider(DialogMode::Agent) != before {
                        wire.clear();
                        images.clear();
                    }
                    save(profile.work.path(), &state)?;
                    profile.save()?;
                    if matches!(command, "/model" | "/models") {
                        screen.notice(tr!("Список моделей сохранён.", "Models saved."));
                    }
                }
                continue;
            }
            "/skills" => {
                skills = skills::discover(directory, root);
                let options = [
                    tr!("Посмотреть скиллы", "Browse skills").into(),
                    tr!("Использовать скилл", "Use a skill").into(),
                ];
                match screen.choose(tr!("Скиллы", "Skills"), &options)? {
                    Some(0) => screen.skills(&skills)?,
                    Some(1) => {
                        let choices: Vec<_> = skills
                            .skills
                            .iter()
                            .map(|skill| skill.name.clone())
                            .collect();
                        if let Some(chosen) =
                            screen.choose(tr!("Использовать скилл", "Use a skill"), &choices)?
                            && let Some(task) = screen
                                .ask_text(tr!("Задача для скилла", "Task for the skill"), false)?
                        {
                            screen.stage_input(format!("/skill {} {task}", choices[chosen]));
                        }
                    }
                    _ => {}
                }
                continue;
            }
            "/mcp" => {
                loop {
                    let statuses = hub.statuses().to_vec();
                    let tools = hub.tools();
                    let candidates = hub.candidates();
                    match screen.mcp(&statuses, &tools, &candidates)? {
                        ui::McpAction::Close => break,
                        ui::McpAction::Restart => {
                            hub.restart(directory);
                            let summary = hub.summary();
                            screen.notice(&summary);
                        }
                        ui::McpAction::Approve(index) => {
                            if let Some(config) = hub.pending(index).cloned()
                                && screen.review(
                                    &trf!("Запустить MCP: {}", "Start MCP: {}", config.name),
                                    &config.approval_preview(),
                                )?
                            {
                                screen
                                    .busy(tr!("Запускаю MCP-серверы…", "Starting MCP servers…"))?;
                                hub.approve(directory, config);
                            }
                        }
                        ui::McpAction::Import(indices) => {
                            let selected: Vec<mcp::Candidate> = indices
                                .iter()
                                .filter_map(|index| candidates.get(*index))
                                .cloned()
                                .collect();
                            match hub.import(directory, &selected) {
                                Ok(message) => screen.notice(&message),
                                Err(error) => screen.notice(&error.to_string()),
                            }
                        }
                    }
                }
                continue;
            }
            "/reasoning" => {
                if let Some(level) = choose_reasoning(&mut screen, &state)? {
                    match set_reasoning(&mut state, level) {
                        Ok(()) => {
                            save(profile.work.path(), &state)?;
                            profile.save()?;
                            screen.notice(&trf!("Reasoning: {}", "Reasoning: {}", level.label()));
                        }
                        Err(error) => screen.notice(&error.to_string()),
                    }
                }
                continue;
            }
            command if command.starts_with("/reasoning ") => {
                let value = command.trim_start_matches("/reasoning ").trim();
                match Reasoning::parse(value) {
                    Some(level) => match set_reasoning(&mut state, level) {
                        Ok(()) => {
                            save(profile.work.path(), &state)?;
                            profile.save()?;
                            screen.notice(&trf!("Reasoning: {}", "Reasoning: {}", level.label()));
                        }
                        Err(error) => screen.notice(&error.to_string()),
                    },
                    None => screen.notice(tr!(
                        "Уровни: default, off, low, medium, high, xhigh, max, ultra",
                        "Levels: default, off, low, medium, high, xhigh, max, ultra"
                    )),
                }
                continue;
            }
            "/init" => {
                match init_document(directory) {
                    Ok(Some((path, content))) => {
                        if screen.review(tr!("Новый AGENTS.md", "New AGENTS.md"), &content)? {
                            ensure!(
                                !path.exists(),
                                tr!("AGENTS.md уже существует", "AGENTS.md already exists")
                            );
                            use std::io::Write as _;
                            fs::File::create_new(&path)?.write_all(content.as_bytes())?;
                            screen.notice(&trf!("Создан {}", "Created {}", path.display()));
                        }
                    }
                    Ok(None) => screen.notice(tr!(
                        "AGENTS.md уже существует. Открой его в редакторе.",
                        "AGENTS.md already exists. Open it in an editor."
                    )),
                    Err(error) => screen.notice(&error.to_string()),
                }
                continue;
            }
            "/diagnose" => {
                diagnose = true;
                continue;
            }
            "/code" => {
                if planning {
                    let Some(plan) = proposed_plan.as_deref() else {
                        screen.notice(tr!(
                            "Сначала получи план или введи /plan cancel.",
                            "Get a plan first or enter /plan cancel."
                        ));
                        continue;
                    };
                    if !screen.review_plan(plan)? {
                        continue;
                    }
                    if screen.dialog_mode() == DialogMode::Chat {
                        screen.notice(tr!(
                            "Реализация недоступна в режиме «Только чат».",
                            "Implementation is not available in Chat only mode."
                        ));
                        continue;
                    }
                    implementation = proposed_plan.take();
                    planning = false;
                    resume_turn = false;
                    screen.set_planning(false);
                    screen.notice("");
                }
                diagnose = false;
                if implementation.is_none() {
                    continue;
                }
            }
            "/new" => {
                let (project, portable) = project_location(&bundle, directory);
                state.sessions.push(Session {
                    project,
                    portable,
                    messages: vec![],
                    mode: DialogMode::Agent,
                });
                index = state.sessions.len() - 1;
                history.clear();
                wire.clear();
                images.clear();
                proposed_plan = None;
                resume_turn = false;
                #[cfg(windows)]
                {
                    desktop_access.reset_chat();
                    desktop = None;
                    screen.desktop_active(false);
                }
                screen.reset_conversation();
                screen.tool_background(&history, &label, directory)?;
                temporary = false;
                save(profile.work.path(), &state)?;
                profile.save()?;
                continue;
            }
            "/sessions" => {
                let (project, portable) = project_location(&bundle, directory);
                let candidates: Vec<_> = state
                    .sessions
                    .iter()
                    .enumerate()
                    .filter(|(_, session)| {
                        session.project == project && session.portable == portable
                    })
                    .map(|(i, session)| {
                        let name: String = session
                            .messages
                            .iter()
                            .find(|msg| msg.role == "user" && !msg.summary)
                            .and_then(|msg| msg.content.lines().next())
                            .unwrap_or(tr!("Новый чат", "New chat"))
                            .chars()
                            .filter(|ch| !ch.is_control())
                            .take(72)
                            .collect();
                        let mark = if i == index && !temporary { "●" } else { " " };
                        (i, format!("{mark} {name}"))
                    })
                    .collect();
                let names: Vec<_> = candidates.iter().map(|(_, name)| name.clone()).collect();
                if let Some(selected) = screen.choose(tr!("Диалоги", "Conversations"), &names)?
                {
                    index = candidates[selected].0;
                    history = state.sessions[index].messages.clone();
                    wire.clear();
                    images.clear();
                    proposed_plan = None;
                    resume_turn = false;
                    #[cfg(windows)]
                    {
                        desktop_access.reset_chat();
                        desktop = None;
                        screen.desktop_active(false);
                    }
                    screen.reset_conversation();
                    screen.set_dialog_mode(state.sessions[index].mode);
                    temporary = false;
                }
                continue;
            }
            "/temporary" => {
                temporary = true;
                history.clear();
                wire.clear();
                images.clear();
                proposed_plan = None;
                resume_turn = false;
                #[cfg(windows)]
                {
                    desktop_access.reset_chat();
                    desktop = None;
                    screen.desktop_active(false);
                }
                screen.reset_conversation();
                screen.notice(tr!(
                    "Временный чат. История не сохраняется.",
                    "Temporary chat. History will not be saved."
                ));
                continue;
            }
            "/clear" => {
                history.clear();
                wire.clear();
                images.clear();
                proposed_plan = None;
                resume_turn = false;
                let mode = screen.dialog_mode();
                #[cfg(windows)]
                {
                    desktop_access.reset_chat();
                    desktop = None;
                    screen.desktop_active(false);
                }
                screen.reset_conversation();
                screen.set_dialog_mode(mode);
                if !temporary {
                    state.sessions[index].messages.clear();
                    save(profile.work.path(), &state)?;
                    profile.save()?;
                }
                continue;
            }
            "/compact" => {
                let Some(provider) = state.active_provider(screen.dialog_mode()) else {
                    screen.notice(tr!("Выбери модель в /model.", "Choose a model in /model."));
                    continue;
                };
                let mode = state.request_mode(&provider, diagnose, planning);
                let extra = if planning {
                    String::new()
                } else {
                    prompt_extra(&skills, &hub, directory, screen.permission_mode())
                };
                let outcome = compact::run(
                    &mut screen,
                    &provider,
                    &mut history,
                    &mut wire,
                    compact::Trigger::Manual,
                    |messages, wire| {
                        input_estimate(&prepare_work_request(
                            &provider, messages, mode, &extra, wire,
                        ))
                    },
                )?;
                if outcome == compact::Outcome::Compacted && !temporary {
                    state.sessions[index].messages = history.clone();
                    save(profile.work.path(), &state)?;
                    profile.save()?;
                }
                continue;
            }
            "/permissions" => {
                screen.permissions_menu()?;
                continue;
            }
            "/tools" => {
                if screen.dialog_mode() == DialogMode::Chat {
                    screen.report(tr!("Инструменты", "Tools"), tr!(
                        "В режиме «Только чат» инструменты модели выключены. Режим меняется в /permissions.",
                        "Model tools are off in Chat only. Change the conversation mode in /permissions."
                    ))?;
                    continue;
                }
                let mut details = tools::names(state.active_provider(screen.dialog_mode()).map_or(
                    RequestMode {
                        diagnose,
                        planning,
                        desktop: cfg!(windows) && state.settings.desktop,
                        ..RequestMode::default()
                    },
                    |provider| state.request_mode(&provider, diagnose, planning),
                ))
                .join("\n");
                if planning {
                    screen.report(tr!("Инструменты", "Tools"), &details)?;
                    continue;
                }
                if browser.is_none() {
                    details.push_str(tr!(
                        "\n\nБраузер запустится по первому запросу, если включён.",
                        "\n\nBrowser starts on first use when enabled."
                    ));
                }
                if !state.web.enabled() {
                    details.push_str(tr!(
                        "\nВеб-поиск: выбери сервис в /search.",
                        "\nWeb search: choose a service in /search."
                    ));
                }
                if !state
                    .active_provider(DialogMode::Agent)
                    .is_some_and(|provider| provider.vision)
                {
                    details.push_str(tr!(
                        "\nСнимки: включи изображения в /model.",
                        "\nScreenshots: enable images in /model."
                    ));
                }
                screen.report(tr!("Инструменты", "Tools"), &details)?;
                continue;
            }
            #[cfg(windows)]
            desktop::STOP_COMMAND => {
                desktop = None;
                desktop_access.stop();
                images.clear();
                screen.desktop_active(false);
                screen.notice(tr!("Управление остановлено.", "Window control stopped."));
                continue;
            }
            "/browser" => {
                let options = [
                    tr!("Запустить скрытый браузер", "Start hidden browser").into(),
                    tr!("Отключить браузер", "Disable browser").into(),
                ];
                match screen.choose(tr!("Браузер", "Browser"), &options)? {
                    Some(0) if browser.is_none() => {
                        browser_enabled = true;
                        screen.busy(tr!("Запускаю браузер…", "Starting browser…"))?;
                        match browser::Browser::start() {
                            Ok(session) => {
                                browser = Some(session);
                                screen.notice(tr!("Браузер готов.", "Browser ready."));
                            }
                            Err(error) => {
                                chat_error(&mut history, tr!("Браузер", "Browser"), error)
                            }
                        }
                    }
                    Some(1) => {
                        browser = None;
                        browser_enabled = false;
                        images.clear();
                    }
                    _ => {}
                }
                continue;
            }
            "/web" | "/search" => {
                if search_menu(&mut screen, &mut state.web, &mut history)? {
                    save(profile.work.path(), &state)?;
                    profile.save()?;
                }
                continue;
            }
            text if text.starts_with("/search ") => {
                show_web_search(
                    &mut screen,
                    &state.web,
                    &mut history,
                    text.trim_start_matches("/search ").trim(),
                )?;
                continue;
            }
            "/project" => {
                let choices = vec![
                    "Web".into(),
                    "Python".into(),
                    "Rust".into(),
                    tr!("С нуля", "From scratch").into(),
                ];
                if let Some(choice) =
                    screen.choose(tr!("Создать проект", "Create project"), &choices)?
                {
                    if choice == 3 {
                        screen.notice(tr!(
                            "Опиши проект в чате.",
                            "Describe the project in the chat."
                        ));
                    } else if diagnose {
                        screen.notice(tr!("Переключись в /code.", "Switch to /code."));
                    } else {
                        match review_changes(
                            &mut screen,
                            permissions::Mode::Ask,
                            &Value::Null,
                            directory,
                            project::starter(choice),
                            &mut batch_undo,
                        ) {
                            Ok(message) => {
                                screen.notice(&message);
                            }
                            Err(error) => chat_error(
                                &mut history,
                                tr!("Создание проекта", "Create project"),
                                error,
                            ),
                        }
                    }
                }
                continue;
            }
            "/continue" if resume_turn => {}
            "/continue" => {
                screen.notice(tr!("Нет запроса для продолжения.", "No request to resume."));
                continue;
            }
            command if command.starts_with("/skill ") => {}
            command if command.starts_with('/') => {
                screen.notice(tr!(
                    "Неизвестная команда. /help — справка.",
                    "Unknown command. /help lists commands."
                ));
                continue;
            }
            _ => {}
        }
        let turn_mode = screen.dialog_mode();
        let Some(turn_provider) = state.active_provider(turn_mode) else {
            chat_error(
                &mut history,
                tr!("Модель", "Model"),
                if state.connections.is_empty() {
                    tr!(
                        "Создай подключение в /provider, затем добавь модель в /model.",
                        "Add a connection in /provider, then add a model in /model."
                    )
                } else {
                    tr!("Выбери модель в /model.", "Choose a model in /model.")
                },
            );
            continue;
        };
        let continuing = command == "/continue";
        if planning && !continuing {
            proposed_plan = None;
        }
        let pastes = screen.take_pastes();
        let (content, display) = if let Some(plan) = implementation {
            (
                trf!(
                    "Реализуй утверждённый план. Следуй текущим разрешениям и проверь результат.\n\n{plan}",
                    "Implement the approved plan. Follow the current permissions and verify the result.\n\n{plan}"
                ),
                Some(tr!("Начать реализацию плана", "Start implementing the plan").into()),
            )
        } else if let Some(skill_request) = command.strip_prefix("/skill ") {
            let (name, task) = skill_request.split_once(' ').unwrap_or((skill_request, ""));
            let Some(skill) = skills.find(name) else {
                screen.notice(tr!("Скилл не найден.", "Skill not found."));
                continue;
            };
            match skills::body(skill) {
                Ok(guide) => (
                    format!("Use the {name} skill. Task: {task}\n\nSkill instructions:\n{guide}"),
                    Some(input.clone()),
                ),
                Err(error) => {
                    screen.notice(&error.to_string());
                    continue;
                }
            }
        } else {
            match attach_files(directory, &input, &pastes) {
                Ok(message) => message,
                Err(error) => {
                    chat_error(&mut history, tr!("Вложение", "Attachment"), error);
                    screen.restore_prompt(&input, pastes);
                    continue;
                }
            }
        };
        if !continuing {
            history.push(Message {
                role: "user".into(),
                content,
                reasoning: None,
                display,
                ..Default::default()
            });
        }
        resume_turn = false;
        screen.begin_turn(submitted_at);
        let mut settings_changed = false;
        let mut step = 0;
        let mut stream_retries = 0;
        let mut compacted_step = None;
        while step < MAX_TOOL_ROUNDS {
            let provider = turn_provider.clone();
            let messages = history.clone();
            let extra = if planning {
                String::new()
            } else {
                prompt_extra(&skills, &hub, directory, screen.permission_mode())
            };
            let mode = state.request_mode(&provider, diagnose, planning);
            let mut prepared = prepare_work_request(&provider, &messages, mode, &extra, &wire);
            if compacted_step != Some(step) && compact::needed(&provider, &prepared) {
                compacted_step = Some(step);
                match compact::run(
                    &mut screen,
                    &provider,
                    &mut history,
                    &mut wire,
                    compact::Trigger::Auto,
                    |messages, wire| {
                        input_estimate(&prepare_work_request(
                            &provider, messages, mode, &extra, wire,
                        ))
                    },
                )? {
                    compact::Outcome::Compacted => {
                        prepared = prepare_work_request(&provider, &history, mode, &extra, &wire);
                    }
                    compact::Outcome::Canceled => break,
                    compact::Outcome::Skipped => {}
                }
            }
            screen.set_input_estimate(input_estimate(&prepared));
            let mut plan = match prepared {
                Ok(plan) => plan,
                Err(error) => {
                    chat_error(
                        &mut history,
                        tr!("Параметры запроса", "Request setup"),
                        error,
                    );
                    break;
                }
            };
            #[cfg(windows)]
            if desktop_access.paused() {
                images.clear();
                screen.desktop_active(false);
            }
            for screenshot in &images {
                attach_image(&provider, &mut plan.body, screenshot);
            }
            let input_tokens = plan.input_tokens;
            let canceled = Arc::new(AtomicBool::new(false));
            let cancel = canceled.clone();
            let (sender, receiver) = mpsc::channel::<Delta>();
            let vision = provider.vision;
            let chat_only = provider.chat_only;
            let reply_started = std::time::Instant::now();
            let task = std::thread::spawn(move || request(&provider, &plan, &cancel, sender));
            screen.reset_stream_escape();
            screen.track("", "");
            let mut output = String::new();
            let mut reasoning = String::new();
            while !task.is_finished() {
                for delta in receiver.try_iter() {
                    match delta {
                        Delta::Content(text) => output.push_str(&text),
                        Delta::Reasoning(text) => reasoning.push_str(&text),
                    }
                }
                match screen.streaming(
                    &history,
                    &output,
                    &reasoning,
                    &turn_provider.model,
                    directory,
                )? {
                    ui::StreamControl::Continue => {}
                    ui::StreamControl::Stop => {
                        canceled.store(true, Ordering::Relaxed);
                        break;
                    }
                    ui::StreamControl::Command(command)
                        if planning && !permissions::plan_allows_command(&command) =>
                    {
                        screen.notice(tr!(
                            "Команда недоступна в /plan.",
                            "Command unavailable in /plan."
                        ));
                    }
                    ui::StreamControl::Command(command) => match command.as_str() {
                        "/permissions" => screen.permissions_menu()?,
                        "/help" => screen.help()?,
                        "/model" | "/models" | "/provider" | "/providers" => {
                            if edit_agent_settings(&mut screen, &mut state, &mut history, &command)?
                            {
                                settings_changed = true;
                                screen.notice(tr!(
                                    "Настройки подключения и модели применятся со следующего сообщения.",
                                    "Connection and model settings apply to the next message."
                                ));
                            }
                        }
                        "/reasoning" => {
                            if let Some(level) = choose_reasoning(&mut screen, &state)? {
                                settings_changed |= live_reasoning(&mut screen, &mut state, level);
                            }
                        }
                        value if value.starts_with("/reasoning ") => {
                            if let Some(level) =
                                Reasoning::parse(value.trim_start_matches("/reasoning ").trim())
                            {
                                settings_changed |= live_reasoning(&mut screen, &mut state, level);
                            } else {
                                screen.notice(tr!(
                                    "Неизвестный уровень рассуждения.",
                                    "Unknown reasoning level."
                                ));
                            }
                        }
                        #[cfg(windows)]
                        desktop::STOP_COMMAND => {
                            desktop = None;
                            desktop_access.stop();
                            screen.desktop_active(false);
                            images.clear();
                            screen
                                .notice(tr!("Управление остановлено.", "Window control stopped."));
                        }
                        "/search" | "/web" => {
                            if search_menu(&mut screen, &mut state.web, &mut history)? {
                                save(profile.work.path(), &state)?;
                                profile.save()?;
                            }
                        }
                        command if command.starts_with("/search ") => {
                            show_web_search(
                                &mut screen,
                                &state.web,
                                &mut history,
                                command.trim_start_matches("/search ").trim(),
                            )?;
                        }
                        other => {
                            screen.stage_input(other.into());
                            screen.notice(tr!(
                                "Команда откроется после ответа.",
                                "The command will open after the reply."
                            ));
                        }
                    },
                }
            }
            for delta in receiver.try_iter() {
                match delta {
                    Delta::Content(text) => output.push_str(&text),
                    Delta::Reasoning(text) => reasoning.push_str(&text),
                }
            }
            screen.reset_stream_escape();
            screen.track(&output, &reasoning);
            if canceled.load(Ordering::Relaxed) {
                images.clear();
                let partial = output.trim_start();
                let tool_text = (partial.starts_with('{') || partial.starts_with("```json"))
                    && partial.contains("\"action\"");
                if !partial.is_empty() && !tool_text {
                    history.push(Message {
                        role: "interrupted".into(),
                        content: output,
                        reasoning: None,
                        ..Default::default()
                    });
                }
                screen.notice(tr!("Остановлено.", "Stopped."));
                break;
            }
            let response = task
                .join()
                .unwrap_or_else(|_| Err(anyhow::anyhow!("Provider worker stopped")));
            let answer = match response {
                Ok(value) => value,
                Err(error) => {
                    let estimated = (output.len() + reasoning.len()).div_ceil(4) as u32;
                    screen.record_reply(
                        input_tokens,
                        estimated,
                        true,
                        reply_started.elapsed().as_secs_f32(),
                    );
                    let interrupted = error.is::<InterruptedStream>();
                    if interrupted && stream_retries == 0 {
                        stream_retries += 1;
                        screen.notice(tr!(
                            "Поток оборвался. Повторяю текущий запрос…",
                            "The stream disconnected. Retrying the current request…"
                        ));
                        continue;
                    }
                    images.clear();
                    chat_error(
                        &mut history,
                        tr!("Ошибка провайдера", "Provider error"),
                        error,
                    );
                    resume_turn = true;
                    screen.notice(tr!(
                        "Введи /continue, чтобы продолжить с текущего шага.",
                        "Enter /continue to resume from the current step."
                    ));
                    break;
                }
            };
            if stream_retries > 0 {
                screen.notice("");
                stream_retries = 0;
            }
            images.clear();
            let output_tokens = (answer.content.len()
                + reasoning.len().max(answer.raw_reasoning.len())
                + answer
                    .calls
                    .iter()
                    .map(|call| call.arguments.len())
                    .sum::<usize>())
            .div_ceil(4) as u32;
            screen.record_reply(
                answer.usage.input.unwrap_or(input_tokens),
                answer.usage.output.unwrap_or(output_tokens),
                answer.usage.input.is_none() || answer.usage.output.is_none(),
                reply_started.elapsed().as_secs_f32(),
            );
            let native = !answer.calls.is_empty() && !chat_only;
            let mut reasoning = answer.reasoning.clone();
            if !native || !answer.content.is_empty() {
                let index = history.len();
                let fallback = !chat_only && !native && parse_action(&answer.content).is_some();
                history.push(Message {
                    role: if native { "narration" } else { "assistant" }.into(),
                    content: answer.content.clone(),
                    reasoning: reasoning.take(),
                    ..Default::default()
                });
                if !native && !fallback {
                    if planning && !answer.content.trim().is_empty() {
                        proposed_plan = Some(answer.content.clone());
                        screen.notice(tr!("/code — проверить план.", "/code to review the plan."));
                    }
                    history[index].receipt = screen.finish_turn();
                    if !answer.raw_reasoning.is_empty() {
                        wire.insert(index, answer);
                    }
                    break;
                }
            }
            let first = history.len();
            let actions: Vec<_> = if native {
                answer
                    .calls
                    .iter()
                    .filter_map(|call| {
                        parse_action(&call.arguments).map(|action| (Some(&call.arguments), action))
                    })
                    .collect()
            } else {
                parse_action(&answer.content)
                    .into_iter()
                    .map(|action| (None, action))
                    .collect()
            };
            let mut events = Vec::new();
            let mut errors = Vec::new();
            let mut canceled_web_request = false;
            for (arguments, action) in actions {
                if let Some(arguments) = arguments {
                    history.push(Message {
                        role: "assistant".into(),
                        content: arguments.clone(),
                        reasoning: reasoning.take(),
                        ..Default::default()
                    });
                }
                if canceled_web_request {
                    history.push(Message {
                        role: "user".into(),
                        content: "Tool result\nSkipped because the user stopped this turn".into(),
                        reasoning: None,
                        ..Default::default()
                    });
                    continue;
                }
                screen.tool_background(&history, &label, directory)?;
                let permissions = screen.permission_mode();
                let root = if let Some(error) = tools::unknown(&action) {
                    Err(error)
                } else if planning && !permissions::plan_allows_action(&action) {
                    Err(anyhow::anyhow!(tr!(
                        "Действие недоступно в /plan.",
                        "Action unavailable in /plan."
                    )))
                } else {
                    permissions::tool_root(permissions, directory, &action)
                };
                let directory = match root {
                    Ok(root) => root,
                    Err(error) => {
                        let text = error.to_string();
                        errors.push(text.clone());
                        history.push(Message {
                            role: "user".into(),
                            content: format!("Tool result\n{text}"),
                            reasoning: None,
                            ..Default::default()
                        });
                        continue;
                    }
                };
                let directory = directory.as_path();
                let mut skill_result = false;
                let result = match action["action"].as_str().unwrap_or("") {
                    "delegate" if !chat_only && !diagnose && !planning => {
                        let limit = state.settings.delegate_limit(turn_provider.connection.id);
                        match parse_subagent_tasks(&action["tasks"], limit) {
                            Err(error) => Err(error),
                            Ok(tasks) => {
                                let details = tasks
                                    .iter()
                                    .map(|(role, task)| format!("{role}: {task}"))
                                    .collect::<Vec<_>>()
                                    .join("\n\n");
                                let approved = screen.review(
                                    tr!("Запустить саб-агентов?", "Start subagents?"),
                                    &format!("{}\n\n{}", tr!("Каждая задача получит доступ только для чтения к выбранному проекту. Фрагменты файлов будут отправлены выбранному провайдеру.", "Each task gets read-only access to the selected project. File excerpts will be sent to the selected provider."), details),
                                )?;
                                if !approved {
                                    Ok(tr!(
                                        "Пользователь отменил запуск саб-агентов.",
                                        "The user canceled subagent launch."
                                    )
                                    .into())
                                } else {
                                    let provider = turn_provider.clone();
                                    let connection = provider.connection.id;
                                    let root = directory.to_path_buf();
                                    let cancel = AtomicBool::new(false);
                                    let batch = subagents::Batch::new(tasks.len() as u8);
                                    let result = std::thread::scope(|scope| {
                                        let worker = scope.spawn(|| {
                                            run_subagent_group(
                                                provider, root, tasks, &cancel, &batch,
                                            )
                                        });
                                        screen.wait_for_tool(
                                            tr!("Саб-агенты", "Subagents"),
                                            &cancel,
                                            worker,
                                        )
                                    })?;
                                    if let Some(learned) = batch.learned() {
                                        let limits = &mut state.settings.provider_limits;
                                        if limits.get(&connection).is_none_or(|current| {
                                            learned.count < current.count
                                                || (learned.count == current.count
                                                    && current.estimated
                                                    && !learned.estimated)
                                        }) {
                                            limits.insert(connection, learned);
                                            settings_changed = true;
                                            screen.notice(&trf!(
                                                "Предел параллельных запросов подключения · {}{}",
                                                "Concurrent request limit for connection · {}{}",
                                                learned.count,
                                                if learned.estimated {
                                                    tr!(" (оценка)", " (estimate)")
                                                } else {
                                                    ""
                                                },
                                            ));
                                        }
                                    }
                                    Ok(result
                                        .into_iter()
                                        .map(|(role, result)| match result {
                                            Ok(text) => format!("## {role}\n{}", text.chars().take(1024).collect::<String>()),
                                            Err(error) => {
                                                trf!("## {role}\nСаб-агент завершился с ошибкой: {error}", "## {role}\nSubagent failed: {error}")
                                            }
                                        })
                                        .collect::<Vec<_>>()
                                        .join("\n\n"))
                                }
                            }
                        }
                    }
                    "plan" => Ok(format!(
                        "Plan recorded: {}",
                        action["text"].as_str().unwrap_or("")
                    )),
                    "read" => read_file(directory, field(&action, "path")).map(|text| {
                        let offset = action["offset"].as_u64().unwrap_or(0).min(100_000) as usize;
                        match action["limit"].as_u64() {
                            Some(limit) => text
                                .lines()
                                .enumerate()
                                .skip(offset)
                                .take(limit.clamp(1, 2000) as usize)
                                .map(|(i, line)| format!("{}: {line}", i + 1))
                                .collect::<Vec<_>>()
                                .join("\n"),
                            None => text,
                        }
                    }),
                    "list" => list_files(directory, field(&action, "path")),
                    "search" => search_project(directory, &action),
                    "glob" => project::search(
                        directory,
                        field(&action, "path"),
                        field(&action, "pattern"),
                        None,
                        false,
                    ),
                    "stat" => safe_path(directory, field(&action, "path")).and_then(|path| {
                        let meta = fs::metadata(path)?;
                        Ok(format!(
                            "{} · {} bytes",
                            if meta.is_dir() { "Directory" } else { "File" },
                            meta.len()
                        ))
                    }),
                    "inspect_project" => project::inspect(directory),
                    "git_status" | "git_diff" => project::git(
                        directory,
                        action["action"] == "git_diff",
                        action["staged"] == true,
                    ),
                    "request_permissions" if !diagnose => {
                        let result = request_permissions(&mut screen, &action);
                        if let Ok(text) = &result {
                            events.push(Message {
                                role: "permission".into(),
                                content: format!("{text}\n{}", field(&action, "reason").trim()),
                                reasoning: None,
                                ..Default::default()
                            });
                        }
                        result
                    }
                    "ask_user" => {
                        let options: Vec<String> = action["options"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str)
                            .take(5)
                            .map(|value| value.chars().take(180).collect())
                            .collect();
                        let question = field(&action, "text");
                        let question = if question.trim().is_empty() {
                            tr!("Вопрос агента", "Agent question")
                        } else {
                            question
                        };
                        let choice = if options.is_empty() {
                            Some(0)
                        } else {
                            let mut menu = options.clone();
                            menu.push(tr!("Свой ответ", "Custom answer").into());
                            screen.choose(question, &menu)?
                        };
                        match choice {
                            Some(index) if index < options.len() => Ok(options[index].clone()),
                            Some(_) => Ok(screen
                                .ask_text(question, false)?
                                .unwrap_or_else(|| "User canceled the question".into())),
                            None => Ok("User canceled the question".into()),
                        }
                    }
                    "create_file" | "apply_changes" if !diagnose => {
                        let changes = if action["action"] == "create_file" {
                            Ok(vec![changes::Change::Create {
                                path: field(&action, "path").into(),
                                content: field(&action, "content").into(),
                            }])
                        } else {
                            serde_json::from_value(action["changes"].clone())
                                .map_err(anyhow::Error::from)
                        };
                        changes.and_then(|changes| {
                            review_changes(
                                &mut screen,
                                permissions,
                                &action,
                                directory,
                                changes,
                                &mut batch_undo,
                            )
                        })
                    }
                    "web_search" => match run_web_search(
                        &mut screen,
                        &state.web,
                        field(&action, "query"),
                        tr!("Ищу в интернете…", "Searching the web…"),
                    )? {
                        Some(result) => result,
                        None => {
                            canceled_web_request = true;
                            Ok("User canceled the web search".into())
                        }
                    },
                    "web_fetch" => {
                        if permissions == permissions::Mode::Ask
                            && !screen.review(
                                tr!("Открыть страницу", "Open web page"),
                                field(&action, "url"),
                            )?
                        {
                            Ok("User declined the web request".into())
                        } else {
                            let url = field(&action, "url").to_owned();
                            match screen.wait_for_web(
                                tr!("Читаю страницу…", "Reading the page…"),
                                move |cancel| web::fetch(&url, cancel),
                            )? {
                                Some(result) => result,
                                None => {
                                    canceled_web_request = true;
                                    Ok("User canceled the page request".into())
                                }
                            }
                        }
                    }
                    "browser" => {
                        let operation = field(&action, "operation");
                        if diagnose
                            && !matches!(
                                operation,
                                "navigate"
                                    | "snapshot"
                                    | "tabs"
                                    | "console"
                                    | "screenshot"
                                    | "select_tab"
                            )
                        {
                            Err(anyhow::anyhow!(
                                "Browser input is unavailable in diagnosis mode"
                            ))
                        } else if operation == "screenshot" && !vision {
                            Err(anyhow::anyhow!(
                                "Enable image input in /model to use screenshots"
                            ))
                        } else if browser_enabled {
                            let title = tr!("Действие в браузере", "Browser action");
                            let detail = if operation == "screenshot" {
                                tr!(
                                    "Снимок страницы будет отправлен выбранной модели.",
                                    "The page screenshot will be sent to the selected model."
                                )
                                .to_owned()
                            } else {
                                format!("{operation}\n{}", mcp::describe_arguments(&action))
                            };
                            let reviewed = browser::review_required(permissions, operation, || {
                                preview.as_ref().zip(browser.as_mut()).is_some_and(
                                    |(preview, browser)| browser.is_local_preview(preview.url()),
                                )
                            });
                            if !reviewed || screen.review(title, &detail)? {
                                let started = if browser.is_none() {
                                    browser::Browser::start().map(|session| {
                                        browser = Some(session);
                                    })
                                } else {
                                    Ok(())
                                };
                                started
                                    .and_then(|()| {
                                        browser.as_mut().unwrap().call(operation, &action)
                                    })
                                    .map(|(text, screenshot)| {
                                        if let Some(data) = screenshot {
                                            images.push(Visual {
                                                mime: "image/jpeg".into(),
                                                data,
                                            });
                                        }
                                        text
                                    })
                            } else {
                                Ok("User canceled the browser action".into())
                            }
                        } else {
                            Err(anyhow::anyhow!(
                                "Browser is disabled. Enable it in /browser."
                            ))
                        }
                    }
                    "preview" => {
                        if field(&action, "operation") == "stop" {
                            preview = None;
                            Ok("Preview stopped".into())
                        } else if let Some(server) = &preview {
                            Ok(format!("Preview: {}", preview::Preview::url(server)))
                        } else if !diagnose {
                            match preview::Preview::start(directory) {
                                Ok(server) => {
                                    let url = server.url().to_owned();
                                    preview = Some(server);
                                    Ok(format!("Preview: {url}"))
                                }
                                Err(error) => Err(error),
                            }
                        } else {
                            Err(anyhow::anyhow!("Starting a preview requires code mode"))
                        }
                    }
                    #[cfg(windows)]
                    "computer_list" | "computer_observe" | "computer_input" => {
                        (|| -> Result<String> {
                            let operation = field(&action, "action");
                            ensure!(
                                operation != "computer_input" || !diagnose,
                                tr!(
                                    "Ввод недоступен в диагностике.",
                                    "Input is unavailable in diagnose mode."
                                )
                            );
                            let stop = desktop_access.authorize(
                                &mut screen,
                                permissions,
                                state.settings.desktop,
                            )?;
                            match operation {
                                "computer_list" => {
                                    let list = desktop::list()
                                        .iter()
                                        .map(desktop::Window::describe)
                                        .collect::<Vec<_>>();
                                    if list.is_empty() {
                                        Ok(tr!("Нет доступных окон.", "No available windows.")
                                            .into())
                                    } else {
                                        Ok(list.join("\n"))
                                    }
                                }
                                "computer_observe" => {
                                    desktop = None;
                                    let screenshot = match field(&action, "operation") {
                                        "screenshot" => {
                                            ensure!(
                                                vision,
                                                tr!(
                                                    "Для снимков включи изображения в /model.",
                                                    "Enable image input in /model to use screenshots."
                                                )
                                            );
                                            true
                                        }
                                        "tree" => false,
                                        _ => bail!(tr!(
                                            "Выбери tree или screenshot.",
                                            "Choose tree or screenshot."
                                        )),
                                    };
                                    let window = desktop::find(field(&action, "window")).context(tr!(
                                    "Окно недоступно или найдено несколько совпадений. Вызови computer_list и передай ID окна.",
                                    "The window is unavailable or ambiguous. Call computer_list and pass a window ID."
                                ))?;
                                    let mut session = desktop::Session::new(window);
                                    let (text, image) = session.observe(screenshot, &stop)?;
                                    if let Some(data) = image {
                                        images.push(Visual {
                                            mime: "image/jpeg".into(),
                                            data,
                                        });
                                    }
                                    desktop = Some(session);
                                    Ok(text)
                                }
                                _ => desktop
                                    .as_mut()
                                    .context(tr!(
                                        "Сначала прочитай окно.",
                                        "Observe the window first."
                                    ))?
                                    .input(&action, &stop),
                            }
                        })()
                    }
                    "shell" if !diagnose => {
                        let script = field(&action, "command");
                        let timeout = action["timeout"].as_u64().unwrap_or(120).clamp(1, 300);
                        let path = action["path"]
                            .as_str()
                            .filter(|p| !p.is_empty())
                            .unwrap_or(".");
                        safe_path(directory, path).and_then(|cwd| {
                            ensure!(cwd.is_dir(), "Expected a directory inside the project");
                            let (command, detail) = tasks::shell(script)?;
                            command_in_chat(
                                &mut screen,
                                CommandRequest {
                                    root: directory,
                                    cwd: &cwd,
                                    process: command,
                                    detail: &detail,
                                    timeout,
                                },
                                &mut batch_undo,
                            )
                        })
                    }
                    "skill" => {
                        skill_result = true;
                        let name = action["name"].as_str().unwrap_or("");
                        match skills.find(name) {
                            Some(skill) if !field(&action, "path").is_empty() => {
                                skills::resource(skill, field(&action, "path"))
                            }
                            Some(skill) => skills::body(skill)
                                .map(|body| format!("Skill loaded: {}\n\n{body}", skill.name)),
                            None => Err(anyhow::anyhow!(
                                "Unknown skill '{name}'. Use /skills to list the available ones."
                            )),
                        }
                    }
                    "mcp" if !diagnose => {
                        let server = action["server"].as_str().unwrap_or("");
                        let tool = action["tool"].as_str().unwrap_or("");
                        let arguments = action
                            .get("arguments")
                            .cloned()
                            .unwrap_or_else(|| json!({}));
                        if server.is_empty() || tool.is_empty() {
                            Err(anyhow::anyhow!("MCP needs both server and tool"))
                        } else if !arguments.is_object() {
                            Err(anyhow::anyhow!("MCP arguments must be an object"))
                        } else {
                            let title = format!("MCP: {server}.{tool}");
                            let detail = format!(
                                "{}\n\n{}",
                                tr!(
                                    "Сервер и инструмент получат эти аргументы. Вызов выполняется от твоего имени.",
                                    "The server and tool receive these arguments. The call runs as you."
                                ),
                                mcp::describe_arguments(&arguments)
                            );
                            if screen.confirm(&title, &detail)? {
                                hub.call(server, tool, &arguments)
                                    .and_then(|(text, screenshot)| {
                                        if let Some(screenshot) = screenshot
                                            && vision
                                            && screen.confirm(
                                                tr!(
                                                    "Отправить изображение модели?",
                                                    "Send the image to the model?"
                                                ),
                                                tr!(
                                                    "Изображение MCP используется один раз.",
                                                    "The MCP image is used once."
                                                ),
                                            )?
                                        {
                                            images.push(screenshot);
                                        }
                                        Ok(text)
                                    })
                            } else {
                                Ok(tr!(
                                    "Вызов MCP отклонён пользователем.",
                                    "The user declined the MCP call."
                                )
                                .to_owned())
                            }
                        }
                    }
                    "run" | "run_task" if !diagnose => {
                        let task = field(&action, "task");
                        let path = action["path"]
                            .as_str()
                            .filter(|p| !p.is_empty())
                            .unwrap_or(".");
                        safe_path(directory, path).and_then(|cwd| {
                            ensure!(cwd.is_dir(), "Expected a directory inside the project");
                            if task.is_empty() {
                                return Ok(format!(
                                    "Available tasks: {}",
                                    tasks::available(&cwd).join(", ")
                                ));
                            }
                            let (command, detail) = tasks::task(&cwd, task)?;
                            command_in_chat(
                                &mut screen,
                                CommandRequest {
                                    root: directory,
                                    cwd: &cwd,
                                    process: command,
                                    detail: &detail,
                                    timeout: 300,
                                },
                                &mut batch_undo,
                            )
                        })
                    }
                    "edit" if !diagnose => {
                        let edit = &action["edit"];
                        let change = changes::Change::Edit {
                            path: field(edit, "path").into(),
                            before: field(edit, "before").into(),
                            after: field(edit, "after").into(),
                        };
                        review_changes(
                            &mut screen,
                            permissions,
                            &action,
                            directory,
                            vec![change],
                            &mut batch_undo,
                        )
                    }
                    _ => Err(anyhow::anyhow!("Action is unavailable in this mode")),
                };
                let prefix = if skill_result {
                    "Tool result (skill instructions)"
                } else {
                    "Tool result (untrusted file content)"
                };
                let content = match result {
                    Ok(text) => text,
                    Err(error) => {
                        let text = error.to_string();
                        errors.push(text.clone());
                        text
                    }
                };
                if matches!(
                    field(&action, "action"),
                    "shell" | "run" | "run_task" | "edit" | "create_file" | "apply_changes"
                ) {
                    events.push(Message {
                        role: "event".into(),
                        content: format!(
                            "{}\n{}{}",
                            field(&action, "action"),
                            action["command"]
                                .as_str()
                                .map_or_else(String::new, |command| format!("{command}\n")),
                            content
                        ),
                        reasoning: None,
                        ..Default::default()
                    });
                }
                history.push(Message {
                    role: "user".into(),
                    content: format!("{prefix}\n{content}"),
                    reasoning: None,
                    ..Default::default()
                });
            }
            if native {
                wire.insert(first, answer);
            }
            history.extend(events);
            for error in errors {
                chat_error(&mut history, tr!("Ошибка инструмента", "Tool error"), error);
            }
            if canceled_web_request {
                screen.notice(tr!("Веб-запрос отменён.", "Web request canceled."));
                break;
            }
            step += 1;
            if step == MAX_TOOL_ROUNDS {
                resume_turn = true;
                screen.notice(tr!(
                    "Достигнут лимит шагов с инструментами. /continue — продолжить.",
                    "Tool-step limit reached. Use /continue to resume."
                ));
            }
        }
        let _ = screen.finish_turn();
        #[cfg(windows)]
        {
            desktop_access.end_turn();
            desktop = None;
            screen.desktop_active(false);
        }
        if state.active_provider(turn_mode).as_ref() != Some(&turn_provider) {
            wire.clear();
            images.clear();
        }
        if !temporary {
            state.sessions[index].messages = history.clone();
            state.sessions[index].mode = screen.dialog_mode();
            save(profile.work.path(), &state)?;
            profile.save()?;
        } else if settings_changed {
            save(profile.work.path(), &state)?;
            profile.save()?;
        }
    }
    screen.finish_headless()
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
    use super::provider::tests::{self as provider_tests, message};
    use super::*;

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
