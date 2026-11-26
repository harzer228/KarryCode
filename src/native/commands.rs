#[cfg(windows)]
use super::desktop;
use super::{
    Message, Reasoning, Session, State,
    actions::review_changes,
    agent::{Agent, prompt_extra},
    browser, chat_error, compact, mcp, models,
    permissions::{self, DialogMode},
    project,
    provider::{RequestMode, apply_reasoning, input_estimate, prepare_work_request},
    save, settings, skills, tools, ui, web,
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
};

fn edit_agent_settings(
    screen: &mut ui::Screen,
    state: &mut State,
    history: &mut Vec<Message>,
    command: &str,
) -> Result<bool> {
    if matches!(command, "/provider" | "/providers") {
        models::provider_menu(screen, state)
    } else {
        models::model_menu(screen, state, history)
    }
}

pub(super) fn slash_command(text: &str) -> bool {
    !text.contains('\n') && text.trim_start().starts_with('/')
}

fn choose_reasoning(screen: &mut ui::Screen, state: &State) -> Result<Option<Reasoning>> {
    let Some(model) = state.active_model.and_then(|index| state.models.get(index)) else {
        screen.notice(tr!("Выбери модель в /model.", "Choose a model in /model."));
        return Ok(None);
    };
    let connection = state.connection(model.connection).context(tr!(
        "Подключение модели не найдено",
        "Missing model connection"
    ))?;
    screen.reasoning(
        model.settings.reasoning,
        &model.settings.available_reasoning(connection),
    )
}

fn set_reasoning(state: &mut State, level: Reasoning) -> Result<()> {
    let index = state
        .active_model
        .context(tr!("Модель не выбрана", "No model selected"))?;
    let mut provider = state.active_provider(DialogMode::Agent).context(tr!(
        "Подключение модели не найдено",
        "Missing model connection"
    ))?;
    ensure!(
        state.models[index]
            .settings
            .available_reasoning(&provider.connection)
            .contains(&level),
        "{}",
        tr!(
            "Этот уровень не включён для модели. Настрой его через /model → F3.",
            "This level is not enabled for the model. Configure it in /model → F3."
        )
    );
    provider.reasoning = level;
    apply_reasoning(&provider, &mut json!({}))?;
    state.models[index].settings.reasoning = level;
    Ok(())
}

fn init_document(directory: &Path) -> Result<Option<(PathBuf, String)>> {
    let target = directory.join("AGENTS.md");
    if target.exists() {
        return Ok(None);
    }
    let name = directory
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "project".into());
    let mut stacks: Vec<&str> = Vec::new();
    let mut commands: Vec<String> = Vec::new();
    if directory.join("Cargo.toml").is_file() {
        stacks.push("Rust / Cargo");
        commands.push("cargo fmt --all --check".into());
        commands.push("cargo clippy --locked --workspace --all-targets -- -D warnings".into());
        commands.push("cargo test --locked --workspace".into());
    }
    if let Ok(text) = fs::read_to_string(directory.join("package.json"))
        && let Ok(value) = serde_json::from_str::<Value>(&text)
    {
        stacks.push("Node.js");
        let before = commands.len();
        let scripts = value.get("scripts").and_then(Value::as_object);
        for key in ["test", "build", "lint", "check"] {
            if scripts.is_some_and(|scripts| scripts.contains_key(key)) {
                commands.push(format!("npm run {key}"));
            }
        }
        if commands.len() == before {
            commands.push("npm test".into());
        }
    }
    if ["pyproject.toml", "requirements.txt", "setup.py"]
        .iter()
        .any(|file| directory.join(file).exists())
    {
        stacks.push("Python");
        commands.push("python -m pytest".into());
    }
    if directory.join("go.mod").exists() {
        stacks.push("Go");
        commands.push("go test ./...".into());
    }
    let mut entries = Vec::new();
    if let Ok(read) = fs::read_dir(directory) {
        for entry in read.filter_map(Result::ok).take(40) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.')
                || matches!(
                    name.as_str(),
                    "target" | "node_modules" | "dist" | "profile" | "vendor"
                )
            {
                continue;
            }
            let suffix = if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                "/"
            } else {
                ""
            };
            entries.push(format!("`{name}{suffix}`"));
            if entries.len() >= 24 {
                break;
            }
        }
    }
    let stack = if stacks.is_empty() {
        tr!("не определён", "not detected").to_owned()
    } else {
        stacks.join(", ")
    };
    let commands = if commands.is_empty() {
        tr!(
            "Проверки не обнаружены. Уточни команды у пользователя.",
            "No checks detected. Ask the user for the commands."
        )
        .to_owned()
    } else {
        commands
            .iter()
            .map(|command| format!("- `{command}`"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let structure = if entries.is_empty() {
        tr!("пусто", "empty").to_owned()
    } else {
        entries.join(" · ")
    };
    let content = format!(
        "# AGENTS.md\n\n\
         Инструкции для ИИ-агентов, работающих в этом проекте. Файл создан командой `/init`\n\
         в KarryCode. Его читает встроенный агент перед началом работы.\n\n\
         ## Проект\n\n\
         - Имя: `{name}`\n\
         - Стек: {stack}\n\n\
         ## Команды\n\n\
         {commands}\n\n\
         ## Структура\n\n\
         {structure}\n\n\
         ## Правила работы\n\n\
         - Прочитай README.md и этот файл перед первой правкой.\n\
         - Меняй только то, что нужно для задачи; не переформатируй чужие файлы.\n\
         - Перед правкой прочитай файл; для замены используй точный фрагмент.\n\
         - После изменений предложи проверку из списка выше; команды выполняются с\n\
           подтверждением пользователя.\n\
         - Не выводи содержимое `.env`, ключей, профиля и других файлов с секретами.\n\
         - В ответе отделяй факты от гипотез; если данных не хватает, перечисли, каких.\n"
    );
    Ok(Some((target, content)))
}

pub(super) fn project_location(bundle: &Path, directory: &Path) -> (String, bool) {
    match directory.strip_prefix(bundle) {
        Ok(relative) => (relative.to_string_lossy().replace('\\', "/"), true),
        Err(_) => (directory.to_string_lossy().into_owned(), false),
    }
}

fn move_session(
    sessions: &mut Vec<Session>,
    index: usize,
    bundle: &Path,
    directory: &Path,
) -> usize {
    let mut session = sessions.remove(index);
    (session.project, session.portable) = project_location(bundle, directory);
    sessions.push(session);
    sessions.len() - 1
}

pub(super) fn no_model_label(state: &State) -> &'static str {
    if state.connections.is_empty() {
        tr!(
            "Добавь провайдера в /provider",
            "Add a provider in /provider"
        )
    } else {
        tr!("Модель не выбрана · /model", "No model selected · /model")
    }
}

fn live_reasoning(screen: &mut ui::Screen, state: &mut State, level: Reasoning) -> bool {
    match set_reasoning(state, level) {
        Ok(()) => {
            screen.notice(tr!(
                "Рассуждение применится к следующему запросу.",
                "Reasoning applies to the next request."
            ));
            true
        }
        Err(error) => {
            screen.notice(&error.to_string());
            false
        }
    }
}

fn search_menu(
    screen: &mut ui::Screen,
    settings: &mut web::Settings,
    history: &mut Vec<Message>,
) -> Result<bool> {
    let options = [
        tr!("Найти в интернете", "Search the web").into(),
        tr!(
            "Встроенный поиск · Mwmbl без ключа",
            "Built-in search · Mwmbl without an API key"
        )
        .into(),
        tr!(
            "Локальный SearXNG · подключиться к серверу на 127.0.0.1:8080",
            "Local SearXNG · connect to the server at 127.0.0.1:8080"
        )
        .into(),
        tr!("Другой URL SearXNG", "Custom SearXNG URL").into(),
        "Brave Search".into(),
        tr!("Выключить веб-поиск", "Disable web search").into(),
    ];
    match screen.choose(tr!("Веб-поиск", "Web search"), &options)? {
        Some(0) => {
            if let Some(query) =
                screen.ask_text(tr!("Найти в интернете", "Search the web"), false)?
            {
                show_web_search(screen, settings, history, &query)?;
            }
            Ok(false)
        }
        Some(1) => {
            *settings = web::Settings::builtin();
            screen.notice(tr!(
                "Mwmbl выбран. Запросы будут отправляться на api.mwmbl.org.",
                "Mwmbl selected. Queries will be sent to api.mwmbl.org."
            ));
            Ok(true)
        }
        Some(2) => {
            let candidate = web::Settings::searx(web::LOCAL_SEARX.to_owned());
            match run_web_search(
                screen,
                &candidate,
                "Rust programming language",
                tr!("Проверяю локальный SearXNG…", "Checking local SearXNG…"),
            )? {
                Some(Ok(_)) => {
                    *settings = candidate;
                    screen.notice(tr!(
                        "Локальный SearXNG подключён · 127.0.0.1:8080",
                        "Local SearXNG connected · 127.0.0.1:8080"
                    ));
                    Ok(true)
                }
                Some(Err(error)) => {
                    chat_error(history, tr!("Локальный SearXNG", "Local SearXNG"), error);
                    Ok(false)
                }
                None => Ok(false),
            }
        }
        Some(3) => {
            let Some(address) = screen.ask_text(tr!("URL SearXNG", "SearXNG URL"), false)? else {
                return Ok(false);
            };
            match web::parse_searx(address.trim()) {
                Ok(address) => {
                    *settings = web::Settings::searx(address);
                    Ok(true)
                }
                Err(error) => {
                    chat_error(history, tr!("Веб-поиск", "Web search"), error);
                    Ok(false)
                }
            }
        }
        Some(4) => {
            let Some(key) = screen.ask_text("Brave API key", true)? else {
                return Ok(false);
            };
            *settings = web::Settings::brave(key.trim().to_owned());
            Ok(true)
        }
        Some(5) => {
            *settings = web::Settings::default();
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn show_web_search(
    screen: &mut ui::Screen,
    settings: &web::Settings,
    history: &mut Vec<Message>,
    query: &str,
) -> Result<()> {
    let title = tr!("Веб-поиск", "Web search");
    match run_web_search(
        screen,
        settings,
        query,
        tr!("Ищу в интернете…", "Searching the web…"),
    )? {
        Some(Ok(text)) => screen.report(title, &text),
        Some(Err(error)) => {
            chat_error(history, title, error);
            Ok(())
        }
        None => {
            screen.notice(tr!("Поиск отменён.", "Search canceled."));
            Ok(())
        }
    }
}

pub(super) fn run_web_search(
    screen: &mut ui::Screen,
    settings: &web::Settings,
    query: &str,
    label: &str,
) -> Result<Option<Result<String>>> {
    let settings = settings.clone();
    let query = query.to_owned();
    screen.wait_for_web(label, move |_| web::search(&settings, &query))
}

/// What the main loop does after a slash command.
pub(super) enum Flow {
    /// Wait for the next input.
    Next,
    Quit,
    /// Send the input to the model, optionally implementing an approved plan.
    Turn(Option<String>),
}

impl Agent<'_> {
    /// Handles a slash command; plain input and turn-starting commands return `Flow::Turn`.
    pub(super) fn command(&mut self, command: &str, label: &str) -> Result<Flow> {
        match command {
            "/setting" => self.settings_menu(label)?,
            command if command == "/cd" || command.starts_with("/cd ") => {
                self.change_directory(command)?
            }
            "/agents" => {
                self.screen.report(tr!("Встроенные саб-агенты", "Built-in subagents"), tr!(
                    "Исследователь — изучает назначенную тему и указывает файлы и строки.\nРевьюер — независимо ищет дефекты в выбранной области.\n\nПо команде delegate можно запустить группу задач параллельно в пределах лимита из /setting после подтверждения. Доступ только на чтение к выбранному проекту; результаты отправляются активному провайдеру. Без shell, MCP, записи и вложенных агентов.",
                    "Researcher — investigates the assigned question and cites files and lines.\nReviewer — independently looks for defects in the assigned scope.\n\nThe delegate tool can run a group of tasks in parallel up to the limit in /setting after confirmation. Workers get read-only access to the selected project; findings are sent to the active provider. No shell, MCP, writes, or nested agents."
                ))?;
            }
            "/theme" => self.choose_theme()?,
            "/header" => self.screen.toggle_header(),
            "/plan" | "/plan cancel" if self.planning => {
                self.proposed_plan = None;
                self.planning = false;
                self.resume_turn = false;
                self.screen.set_planning(false);
                self.screen
                    .notice(tr!("План не принят.", "Plan not accepted."));
            }
            "/plan" => {
                self.proposed_plan = None;
                self.resume_turn = false;
                self.images.clear();
                self.planning = true;
                self.diagnose = false;
                self.screen.set_planning(true);
                self.screen.notice("");
            }
            "/quit" | "/exit" => return Ok(Flow::Quit),
            "/undo" => self.undo()?,
            "/help" => self.screen.help()?,
            "/model" | "/models" | "/provider" | "/providers" => self.edit_models(command)?,
            "/skills" => self.skills_menu()?,
            "/mcp" => self.mcp_menu()?,
            "/reasoning" => {
                if let Some(level) = choose_reasoning(&mut self.screen, &self.state)? {
                    self.apply_reasoning_level(level)?;
                }
            }
            command if command.starts_with("/reasoning ") => {
                let value = command.trim_start_matches("/reasoning ").trim();
                match Reasoning::parse(value) {
                    Some(level) => self.apply_reasoning_level(level)?,
                    None => self.screen.notice(tr!(
                        "Уровни: default, off, low, medium, high, xhigh, max, ultra",
                        "Levels: default, off, low, medium, high, xhigh, max, ultra"
                    )),
                }
            }
            "/init" => self.init()?,
            "/diagnose" => self.diagnose = true,
            "/code" => return self.code(),
            "/new" => self.new_chat(label)?,
            "/sessions" => self.sessions()?,
            "/temporary" => self.temporary_chat(),
            "/clear" => self.clear()?,
            "/compact" => self.compact()?,
            "/permissions" => self.screen.permissions_menu()?,
            "/tools" => self.tools_report()?,
            #[cfg(windows)]
            desktop::STOP_COMMAND => {
                self.desktop = None;
                self.desktop_access.stop();
                self.images.clear();
                self.screen.desktop_active(false);
                self.screen
                    .notice(tr!("Управление остановлено.", "Window control stopped."));
            }
            "/browser" => self.browser_menu()?,
            "/web" | "/search" => {
                if search_menu(&mut self.screen, &mut self.state.web, &mut self.history)? {
                    self.save_state()?;
                }
            }
            text if text.starts_with("/search ") => {
                show_web_search(
                    &mut self.screen,
                    &self.state.web,
                    &mut self.history,
                    text.trim_start_matches("/search ").trim(),
                )?;
            }
            "/project" => self.create_project()?,
            "/continue" if self.resume_turn => return Ok(Flow::Turn(None)),
            "/continue" => {
                self.screen
                    .notice(tr!("Нет запроса для продолжения.", "No request to resume."));
            }
            command if command.starts_with("/skill ") => return Ok(Flow::Turn(None)),
            command if command.starts_with('/') => {
                self.screen.notice(tr!(
                    "Неизвестная команда. /help — справка.",
                    "Unknown command. /help lists commands."
                ));
            }
            _ => return Ok(Flow::Turn(None)),
        }
        Ok(Flow::Next)
    }

    /// Handles a slash command typed while a reply is streaming.
    pub(super) fn stream_command(
        &mut self,
        command: &str,
        settings_changed: &mut bool,
    ) -> Result<()> {
        match command {
            "/permissions" => self.screen.permissions_menu()?,
            "/help" => self.screen.help()?,
            "/model" | "/models" | "/provider" | "/providers" => {
                if edit_agent_settings(
                    &mut self.screen,
                    &mut self.state,
                    &mut self.history,
                    command,
                )? {
                    *settings_changed = true;
                    self.screen.notice(tr!(
                        "Настройки подключения и модели применятся со следующего сообщения.",
                        "Connection and model settings apply to the next message."
                    ));
                }
            }
            "/reasoning" => {
                if let Some(level) = choose_reasoning(&mut self.screen, &self.state)? {
                    *settings_changed |= live_reasoning(&mut self.screen, &mut self.state, level);
                }
            }
            value if value.starts_with("/reasoning ") => {
                if let Some(level) =
                    Reasoning::parse(value.trim_start_matches("/reasoning ").trim())
                {
                    *settings_changed |= live_reasoning(&mut self.screen, &mut self.state, level);
                } else {
                    self.screen.notice(tr!(
                        "Неизвестный уровень рассуждения.",
                        "Unknown reasoning level."
                    ));
                }
            }
            #[cfg(windows)]
            desktop::STOP_COMMAND => {
                self.desktop = None;
                self.desktop_access.stop();
                self.screen.desktop_active(false);
                self.images.clear();
                self.screen
                    .notice(tr!("Управление остановлено.", "Window control stopped."));
            }
            "/search" | "/web" => {
                if search_menu(&mut self.screen, &mut self.state.web, &mut self.history)? {
                    self.save_state()?;
                }
            }
            command if command.starts_with("/search ") => {
                show_web_search(
                    &mut self.screen,
                    &self.state.web,
                    &mut self.history,
                    command.trim_start_matches("/search ").trim(),
                )?;
            }
            other => {
                self.screen.stage_input(other.into());
                self.screen.notice(tr!(
                    "Команда откроется после ответа.",
                    "The command will open after the reply."
                ));
            }
        }
        Ok(())
    }

    fn save_state(&mut self) -> Result<()> {
        save(self.profile.work.path(), &self.state)?;
        self.profile.save()
    }

    fn settings_menu(&mut self, label: &str) -> Result<()> {
        #[cfg(windows)]
        let paused = self.desktop_access.paused();
        #[cfg(not(windows))]
        let paused = false;
        let history = &self.history;
        let directory = self.directory.as_path();
        match settings::menu(
            &mut self.screen,
            &mut self.state,
            &mut self.theme,
            self.profile.work.path(),
            self.root,
            paused,
            |screen, state| {
                let label = if state.active_provider(screen.dialog_mode()).is_some() {
                    label
                } else {
                    no_model_label(state)
                };
                screen.redraw_language(history, label, directory)
            },
        ) {
            Ok(resume_desktop) => {
                #[cfg(windows)]
                if resume_desktop || !self.state.settings.desktop {
                    self.desktop_access.resume();
                    self.desktop = None;
                    self.screen.desktop_active(false);
                    self.images.clear();
                }
                #[cfg(not(windows))]
                let _ = resume_desktop;
                self.profile.save()?;
            }
            Err(error) => self.screen.notice(&format!("{error:#}")),
        }
        Ok(())
    }

    fn change_directory(&mut self, command: &str) -> Result<()> {
        let directory = self.directory.as_path();
        let target = command.strip_prefix("/cd").unwrap_or_default().trim();
        let selected = if target.is_empty() {
            self.screen
                .pick_directory(directory, self.root, &self.profile.recent)
        } else {
            crate::folders::resolve_directory(directory, target).map(Some)
        };
        match selected {
            Ok(Some(next)) if next.starts_with(&self.bundle) => {
                self.screen.notice(tr!(
                    "Выбери папку вне профиля KarryCode.",
                    "Choose a folder outside the KarryCode profile."
                ));
            }
            Ok(Some(next)) if next != directory => {
                if !self.temporary {
                    let old_index = self.index;
                    let previous = self.state.sessions[self.index].clone();
                    let recent = self.profile.recent.clone();
                    self.profile.recent.remember(self.root, &next);
                    self.index =
                        move_session(&mut self.state.sessions, self.index, &self.bundle, &next);
                    if let Err(error) = save(self.profile.work.path(), &self.state)
                        .and_then(|()| self.profile.save())
                    {
                        self.state.sessions.pop();
                        self.state.sessions.insert(old_index, previous);
                        self.index = old_index;
                        self.profile.recent = recent;
                        // Restore the in-memory profile's state file if snapshot creation failed.
                        save(self.profile.work.path(), &self.state)?;
                        self.screen.notice(&format!("{error:#}"));
                        return Ok(());
                    }
                    *self.workspace = next.clone();
                }
                self.directory = next;
                self.skills = skills::discover(&self.directory, self.root);
                self.hub = mcp::Hub::load(&self.directory);
                self.preview = None;
                self.proposed_plan = None;
                self.batch_undo = None;
                self.resume_turn = false;
                self.images.clear();
                self.history.push(Message {
                    role: "event".into(),
                    content: trf!(
                        "Папка проекта · {}",
                        "Project folder · {}",
                        crate::folders::display(&self.directory)
                    ),
                    ..Default::default()
                });
                self.screen
                    .notice(tr!("Папка проекта изменена.", "Project folder changed."));
            }
            Ok(_) => {}
            Err(error) => self.screen.notice(&format!("{error:#}")),
        }
        Ok(())
    }

    fn choose_theme(&mut self) -> Result<()> {
        if let Some(next) = self.screen.choose_theme(self.theme)? {
            match next
                .save(self.profile.work.path())
                .and_then(|()| self.profile.save())
            {
                Ok(()) => {
                    self.theme = next;
                    self.screen.set_theme(self.theme);
                }
                Err(error) => self.screen.notice(&format!("{error:#}")),
            }
        }
        Ok(())
    }

    fn undo(&mut self) -> Result<()> {
        if self.diagnose {
            self.screen.notice(tr!(
                "Переключись в /code для отмены правок.",
                "Switch to /code to revert changes."
            ));
        } else if let Some(change) = self.batch_undo.take() {
            if self.screen.review(
                tr!("Отменить изменения", "Revert changes"),
                &change.preview(),
            )? {
                match change.apply() {
                    Ok(_) => self
                        .screen
                        .notice(tr!("Изменения отменены.", "Changes reverted.")),
                    Err(error) => self.screen.notice(&error.to_string()),
                }
            } else {
                self.batch_undo = Some(change);
            }
        }
        Ok(())
    }

    fn edit_models(&mut self, command: &str) -> Result<()> {
        let before = self.state.active_provider(DialogMode::Agent);
        if edit_agent_settings(
            &mut self.screen,
            &mut self.state,
            &mut self.history,
            command,
        )? {
            if self.state.active_provider(DialogMode::Agent) != before {
                self.wire.clear();
                self.images.clear();
            }
            self.save_state()?;
            if matches!(command, "/model" | "/models") {
                self.screen
                    .notice(tr!("Список моделей сохранён.", "Models saved."));
            }
        }
        Ok(())
    }

    fn skills_menu(&mut self) -> Result<()> {
        self.skills = skills::discover(&self.directory, self.root);
        let options = [
            tr!("Посмотреть скиллы", "Browse skills").into(),
            tr!("Использовать скилл", "Use a skill").into(),
        ];
        match self.screen.choose(tr!("Скиллы", "Skills"), &options)? {
            Some(0) => self.screen.skills(&self.skills)?,
            Some(1) => {
                let choices: Vec<_> = self
                    .skills
                    .skills
                    .iter()
                    .map(|skill| skill.name.clone())
                    .collect();
                if let Some(chosen) = self
                    .screen
                    .choose(tr!("Использовать скилл", "Use a skill"), &choices)?
                    && let Some(task) = self
                        .screen
                        .ask_text(tr!("Задача для скилла", "Task for the skill"), false)?
                {
                    self.screen
                        .stage_input(format!("/skill {} {task}", choices[chosen]));
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn mcp_menu(&mut self) -> Result<()> {
        let directory = self.directory.as_path();
        loop {
            let statuses = self.hub.statuses().to_vec();
            let tools = self.hub.tools();
            let candidates = self.hub.candidates();
            match self.screen.mcp(&statuses, &tools, &candidates)? {
                ui::McpAction::Close => break,
                ui::McpAction::Restart => {
                    self.hub.restart(directory);
                    let summary = self.hub.summary();
                    self.screen.notice(&summary);
                }
                ui::McpAction::Approve(index) => {
                    if let Some(config) = self.hub.pending(index).cloned()
                        && self.screen.review(
                            &trf!("Запустить MCP: {}", "Start MCP: {}", config.name),
                            &config.approval_preview(),
                        )?
                    {
                        self.screen
                            .busy(tr!("Запускаю MCP-серверы…", "Starting MCP servers…"))?;
                        self.hub.approve(directory, config);
                    }
                }
                ui::McpAction::Import(indices) => {
                    let selected: Vec<mcp::Candidate> = indices
                        .iter()
                        .filter_map(|index| candidates.get(*index))
                        .cloned()
                        .collect();
                    match self.hub.import(directory, &selected) {
                        Ok(message) => self.screen.notice(&message),
                        Err(error) => self.screen.notice(&error.to_string()),
                    }
                }
            }
        }
        Ok(())
    }

    fn apply_reasoning_level(&mut self, level: Reasoning) -> Result<()> {
        match set_reasoning(&mut self.state, level) {
            Ok(()) => {
                self.save_state()?;
                self.screen
                    .notice(&trf!("Reasoning: {}", "Reasoning: {}", level.label()));
            }
            Err(error) => self.screen.notice(&error.to_string()),
        }
        Ok(())
    }

    fn init(&mut self) -> Result<()> {
        match init_document(&self.directory) {
            Ok(Some((path, content))) => {
                if self
                    .screen
                    .review(tr!("Новый AGENTS.md", "New AGENTS.md"), &content)?
                {
                    ensure!(
                        !path.exists(),
                        tr!("AGENTS.md уже существует", "AGENTS.md already exists")
                    );
                    use std::io::Write as _;
                    fs::File::create_new(&path)?.write_all(content.as_bytes())?;
                    self.screen
                        .notice(&trf!("Создан {}", "Created {}", path.display()));
                }
            }
            Ok(None) => self.screen.notice(tr!(
                "AGENTS.md уже существует. Открой его в редакторе.",
                "AGENTS.md already exists. Open it in an editor."
            )),
            Err(error) => self.screen.notice(&error.to_string()),
        }
        Ok(())
    }

    fn code(&mut self) -> Result<Flow> {
        let mut implementation = None;
        if self.planning {
            let Some(plan) = self.proposed_plan.as_deref() else {
                self.screen.notice(tr!(
                    "Сначала получи план или введи /plan cancel.",
                    "Get a plan first or enter /plan cancel."
                ));
                return Ok(Flow::Next);
            };
            if !self.screen.review_plan(plan)? {
                return Ok(Flow::Next);
            }
            if self.screen.dialog_mode() == DialogMode::Chat {
                self.screen.notice(tr!(
                    "Реализация недоступна в режиме «Только чат».",
                    "Implementation is not available in Chat only mode."
                ));
                return Ok(Flow::Next);
            }
            implementation = self.proposed_plan.take();
            self.planning = false;
            self.resume_turn = false;
            self.screen.set_planning(false);
            self.screen.notice("");
        }
        self.diagnose = false;
        if implementation.is_none() {
            return Ok(Flow::Next);
        }
        Ok(Flow::Turn(implementation))
    }

    fn new_chat(&mut self, label: &str) -> Result<()> {
        let (project, portable) = project_location(&self.bundle, &self.directory);
        self.state.sessions.push(Session {
            project,
            portable,
            messages: vec![],
            mode: DialogMode::Agent,
        });
        self.index = self.state.sessions.len() - 1;
        self.history.clear();
        self.wire.clear();
        self.images.clear();
        self.proposed_plan = None;
        self.resume_turn = false;
        #[cfg(windows)]
        {
            self.desktop_access.reset_chat();
            self.desktop = None;
            self.screen.desktop_active(false);
        }
        self.screen.reset_conversation();
        self.screen
            .tool_background(&self.history, label, &self.directory)?;
        self.temporary = false;
        self.save_state()
    }

    fn sessions(&mut self) -> Result<()> {
        let (project, portable) = project_location(&self.bundle, &self.directory);
        let candidates: Vec<_> = self
            .state
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, session)| session.project == project && session.portable == portable)
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
                let mark = if i == self.index && !self.temporary {
                    "●"
                } else {
                    " "
                };
                (i, format!("{mark} {name}"))
            })
            .collect();
        let names: Vec<_> = candidates.iter().map(|(_, name)| name.clone()).collect();
        if let Some(selected) = self
            .screen
            .choose(tr!("Диалоги", "Conversations"), &names)?
        {
            self.index = candidates[selected].0;
            self.history = self.state.sessions[self.index].messages.clone();
            self.wire.clear();
            self.images.clear();
            self.proposed_plan = None;
            self.resume_turn = false;
            #[cfg(windows)]
            {
                self.desktop_access.reset_chat();
                self.desktop = None;
                self.screen.desktop_active(false);
            }
            self.screen.reset_conversation();
            self.screen
                .set_dialog_mode(self.state.sessions[self.index].mode);
            self.temporary = false;
        }
        Ok(())
    }

    fn temporary_chat(&mut self) {
        self.temporary = true;
        self.history.clear();
        self.wire.clear();
        self.images.clear();
        self.proposed_plan = None;
        self.resume_turn = false;
        #[cfg(windows)]
        {
            self.desktop_access.reset_chat();
            self.desktop = None;
            self.screen.desktop_active(false);
        }
        self.screen.reset_conversation();
        self.screen.notice(tr!(
            "Временный чат. История не сохраняется.",
            "Temporary chat. History will not be saved."
        ));
    }

    fn clear(&mut self) -> Result<()> {
        self.history.clear();
        self.wire.clear();
        self.images.clear();
        self.proposed_plan = None;
        self.resume_turn = false;
        let mode = self.screen.dialog_mode();
        #[cfg(windows)]
        {
            self.desktop_access.reset_chat();
            self.desktop = None;
            self.screen.desktop_active(false);
        }
        self.screen.reset_conversation();
        self.screen.set_dialog_mode(mode);
        if !self.temporary {
            self.state.sessions[self.index].messages.clear();
            self.save_state()?;
        }
        Ok(())
    }

    fn compact(&mut self) -> Result<()> {
        let Some(provider) = self.state.active_provider(self.screen.dialog_mode()) else {
            self.screen
                .notice(tr!("Выбери модель в /model.", "Choose a model in /model."));
            return Ok(());
        };
        let mode = self
            .state
            .request_mode(&provider, self.diagnose, self.planning);
        let extra = if self.planning {
            String::new()
        } else {
            prompt_extra(
                &self.skills,
                &self.hub,
                &self.directory,
                self.screen.permission_mode(),
            )
        };
        let outcome = compact::run(
            &mut self.screen,
            &provider,
            &mut self.history,
            &mut self.wire,
            compact::Trigger::Manual,
            |messages, wire| {
                input_estimate(&prepare_work_request(
                    &provider, messages, mode, &extra, wire,
                ))
            },
        )?;
        if outcome == compact::Outcome::Compacted && !self.temporary {
            self.state.sessions[self.index].messages = self.history.clone();
            self.save_state()?;
        }
        Ok(())
    }

    fn tools_report(&mut self) -> Result<()> {
        if self.screen.dialog_mode() == DialogMode::Chat {
            self.screen.report(tr!("Инструменты", "Tools"), tr!(
                "В режиме «Только чат» инструменты модели выключены. Режим меняется в /permissions.",
                "Model tools are off in Chat only. Change the conversation mode in /permissions."
            ))?;
            return Ok(());
        }
        let mut details = tools::names(
            self.state
                .active_provider(self.screen.dialog_mode())
                .map_or(
                    RequestMode {
                        diagnose: self.diagnose,
                        planning: self.planning,
                        desktop: cfg!(windows) && self.state.settings.desktop,
                        ..RequestMode::default()
                    },
                    |provider| {
                        self.state
                            .request_mode(&provider, self.diagnose, self.planning)
                    },
                ),
        )
        .join("\n");
        if self.planning {
            self.screen.report(tr!("Инструменты", "Tools"), &details)?;
            return Ok(());
        }
        if self.browser.is_none() {
            details.push_str(tr!(
                "\n\nБраузер запустится по первому запросу, если включён.",
                "\n\nBrowser starts on first use when enabled."
            ));
        }
        if !self.state.web.enabled() {
            details.push_str(tr!(
                "\nВеб-поиск: выбери сервис в /search.",
                "\nWeb search: choose a service in /search."
            ));
        }
        if !self
            .state
            .active_provider(DialogMode::Agent)
            .is_some_and(|provider| provider.vision)
        {
            details.push_str(tr!(
                "\nСнимки: включи изображения в /model.",
                "\nScreenshots: enable images in /model."
            ));
        }
        self.screen.report(tr!("Инструменты", "Tools"), &details)
    }

    fn browser_menu(&mut self) -> Result<()> {
        let options = [
            tr!("Запустить скрытый браузер", "Start hidden browser").into(),
            tr!("Отключить браузер", "Disable browser").into(),
        ];
        match self.screen.choose(tr!("Браузер", "Browser"), &options)? {
            Some(0) if self.browser.is_none() => {
                self.browser_enabled = true;
                self.screen
                    .busy(tr!("Запускаю браузер…", "Starting browser…"))?;
                match browser::Browser::start() {
                    Ok(session) => {
                        self.browser = Some(session);
                        self.screen.notice(tr!("Браузер готов.", "Browser ready."));
                    }
                    Err(error) => chat_error(&mut self.history, tr!("Браузер", "Browser"), error),
                }
            }
            Some(1) => {
                self.browser = None;
                self.browser_enabled = false;
                self.images.clear();
            }
            _ => {}
        }
        Ok(())
    }

    fn create_project(&mut self) -> Result<()> {
        let choices = vec![
            "Web".into(),
            "Python".into(),
            "Rust".into(),
            tr!("С нуля", "From scratch").into(),
        ];
        if let Some(choice) = self
            .screen
            .choose(tr!("Создать проект", "Create project"), &choices)?
        {
            if choice == 3 {
                self.screen.notice(tr!(
                    "Опиши проект в чате.",
                    "Describe the project in the chat."
                ));
            } else if self.diagnose {
                self.screen
                    .notice(tr!("Переключись в /code.", "Switch to /code."));
            } else {
                match review_changes(
                    &mut self.screen,
                    permissions::Mode::Ask,
                    &Value::Null,
                    &self.directory,
                    project::starter(choice),
                    &mut self.batch_undo,
                ) {
                    Ok(message) => {
                        self.screen.notice(&message);
                    }
                    Err(error) => chat_error(
                        &mut self.history,
                        tr!("Создание проекта", "Create project"),
                        error,
                    ),
                }
            }
        }
        Ok(())
    }
}

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
