#[cfg(windows)]
use super::desktop;
use super::{
    Message, Provider,
    agent::Agent,
    browser, changes,
    commands::run_web_search,
    field,
    files::{list_files, read_file, safe_path, search_project},
    mcp, permissions, preview, project,
    provider::Visual,
    sandbox, skills,
    subagents::{self, parse_subagent_tasks, run_subagent_group},
    tasks, ui, web,
};
#[cfg(windows)]
use anyhow::bail;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

fn request_permissions(screen: &mut ui::Screen, action: &Value) -> Result<String> {
    let mode = permissions::Mode::parse(field(action, "mode"))
        .context(tr!("Выбери ask, auto или full", "Choose ask, auto or full"))?;
    let reason = field(action, "reason").trim();
    ensure!(
        !reason.is_empty() && reason.chars().count() <= 600,
        "Give a reason of up to 600 characters"
    );
    if mode == screen.permission_mode() {
        return Ok(tr!(
            "Этот режим разрешений уже включён.",
            "That permission mode is already active."
        )
        .into());
    }
    if screen.request_permission(mode, reason)? {
        Ok(trf!(
            "Разрешения подтверждены: {} ({})",
            "Permission granted: {} ({})",
            mode.label(),
            mode.identifier()
        ))
    } else {
        Ok(trf!(
            "Запрос разрешения отклонён. Режим: {}",
            "Permission request declined. Mode: {}",
            screen.permission_mode().label()
        ))
    }
}

struct CommandRequest<'a> {
    root: &'a Path,
    cwd: &'a Path,
    process: std::process::Command,
    detail: &'a str,
    timeout: u64,
}

fn isolation_reason(error: &anyhow::Error) -> String {
    let detail = format!("{error:#}");
    let lower = detail.to_ascii_lowercase();
    if lower.contains("registry") && lower.contains("powershell") {
        return tr!(
            "AppContainer не даёт оболочке прочитать раздел реестра PowerShell.",
            "AppContainer denied access to the registry key required by PowerShell."
        )
        .into();
    }
    if lower.contains("psetwlog") {
        return tr!(
            "PowerShell не смог запустить журнал событий в AppContainer.",
            "PowerShell could not initialize event logging in AppContainer."
        )
        .into();
    }
    if lower.contains("0xc0000135") {
        return tr!(
            "В изоляции недоступна DLL установленного рантайма.",
            "The installed runtime requires a DLL unavailable inside the sandbox."
        )
        .into();
    }
    if lower.contains("0xc0000022") {
        return tr!(
            "Windows заблокировал программу или её зависимости внутри песочницы.",
            "Windows denied access to the program or its dependencies inside the sandbox."
        )
        .into();
    }
    detail
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("Unknown error")
        .chars()
        .take(240)
        .collect()
}

fn command_in_chat(
    screen: &mut ui::Screen,
    request: CommandRequest<'_>,
    undo: &mut Option<changes::Batch>,
) -> Result<String> {
    crate::platform::require_standard_user()?;
    let CommandRequest {
        root,
        cwd,
        process,
        detail,
        timeout,
    } = request;
    let relative = cwd.strip_prefix(root.canonicalize()?)?;
    let cancel = AtomicBool::new(false);
    let sandbox = std::thread::scope(|scope| {
        let process = &process;
        let worker = scope.spawn(move || {
            let workspace = sandbox::Workspace::prepare(root)?;
            workspace.probe(relative, process)?;
            Ok(workspace)
        });
        screen.wait_for_tool(
            tr!("Проверяю песочницу", "Checking isolation"),
            &cancel,
            worker,
        )
    })?;
    if cancel.load(Ordering::Relaxed) {
        return Ok(tr!("Команда отменена.", "Command canceled.").into());
    }
    let mode = match &sandbox {
        Ok(_) => tr!(
            "Изолировано · временная копия проекта",
            "Isolated · temporary project copy"
        )
        .to_owned(),
        Err(error) => trf!(
            "Изоляция недоступна. {}\nОднократный запуск с твоими правами.",
            "Isolation unavailable. {}\nRun once with your permissions.",
            isolation_reason(error)
        ),
    };
    if (sandbox.is_err() || screen.permission_mode() == permissions::Mode::Ask)
        && !screen.review(
            if sandbox.is_err() {
                tr!("Запустить без изоляции?", "Run without isolation?")
            } else {
                tr!("Выполнить команду", "Run shell command")
            },
            &trf!(
                "{}\nПапка: {}\nЛимит: {} с\n\n{}",
                "{}\nFolder: {}\nTimeout: {} s\n\n{}",
                mode,
                crate::folders::display(cwd),
                timeout,
                detail
            ),
        )?
    {
        return Ok(if sandbox.is_err() {
            tr!(
                "Запуск без изоляции отклонён. Для проверки файлов используй встроенные read, search или glob.",
                "Running without isolation was declined. Use built-in read, search, or glob to inspect project files."
            )
        } else {
            tr!("Команда отменена.", "Command canceled.")
        }.into());
    }
    let container = sandbox.ok();
    let mut output = std::thread::scope(|scope| {
        let sandbox = container.as_ref();
        let cancel = &cancel;
        let worker = scope.spawn(move || match sandbox {
            Some(workspace) => workspace.execute(relative, &process, timeout, cancel),
            None => tasks::execute_cancellable(process, cwd, timeout, cancel),
        });
        screen.wait_for_tool(tr!("Выполняется", "Running"), cancel, worker)?
    })?;
    if let Some(workspace) = container
        && output.success
    {
        let edits = workspace.changes()?;
        if !edits.is_empty() {
            output.text.push('\n');
            let current = screen.permission_mode();
            output.text.push_str(&review_changes(
                screen,
                current,
                &Value::Null,
                root,
                edits,
                undo,
            )?);
        }
    }
    Ok(output.text)
}

pub(super) fn review_changes(
    screen: &mut ui::Screen,
    permissions: permissions::Mode,
    action: &Value,
    directory: &Path,
    items: Vec<changes::Change>,
    undo: &mut Option<changes::Batch>,
) -> Result<String> {
    let destructive = items.iter().any(|change| {
        matches!(
            change,
            changes::Change::Delete { .. } | changes::Change::Move { .. }
        )
    });
    let batch = changes::Batch::prepare(directory, items)?;
    let reason = field(&action["edit"], "reason");
    if (permissions == permissions::Mode::Ask
        || destructive && permissions != permissions::Mode::Full)
        && !screen.review(
            tr!("Изменения проекта", "Project changes"),
            &format!("{reason}\n{}", batch.preview()),
        )?
    {
        return Ok(tr!("Изменения отменены.", "Changes canceled.").into());
    }
    *undo = Some(batch.apply()?);
    Ok(tr!("Изменения применены.", "Changes applied.").into())
}

/// Effects of a tool round that outlive a single action.
#[derive(Default)]
pub(super) struct ToolRound {
    pub(super) events: Vec<Message>,
    pub(super) canceled_web_request: bool,
}

impl Agent<'_> {
    /// Runs one tool action inside `directory`. The outer error ends the session; the inner
    /// one is returned to the model as the tool result.
    pub(super) fn dispatch(
        &mut self,
        action: &Value,
        directory: &Path,
        permissions: permissions::Mode,
        turn_provider: &Provider,
        round: &mut ToolRound,
        settings_changed: &mut bool,
    ) -> Result<Result<String>> {
        let chat_only = turn_provider.chat_only;
        let vision = turn_provider.vision;
        Ok(match action["action"].as_str().unwrap_or("") {
            "delegate" if !chat_only && !self.diagnose && !self.planning => {
                self.delegate(action, directory, turn_provider, settings_changed)?
            }
            "plan" => Ok(format!(
                "Plan recorded: {}",
                action["text"].as_str().unwrap_or("")
            )),
            "read" => read_file(directory, field(action, "path")).map(|text| {
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
            "list" => list_files(directory, field(action, "path")),
            "search" => search_project(directory, action),
            "glob" => project::search(
                directory,
                field(action, "path"),
                field(action, "pattern"),
                None,
                false,
            ),
            "stat" => safe_path(directory, field(action, "path")).and_then(|path| {
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
            "request_permissions" if !self.diagnose => {
                let result = request_permissions(&mut self.screen, action);
                if let Ok(text) = &result {
                    round.events.push(Message {
                        role: "permission".into(),
                        content: format!("{text}\n{}", field(action, "reason").trim()),
                        reasoning: None,
                        ..Default::default()
                    });
                }
                result
            }
            "ask_user" => self.ask_user(action)?,
            "create_file" | "apply_changes" if !self.diagnose => {
                let changes = if action["action"] == "create_file" {
                    Ok(vec![changes::Change::Create {
                        path: field(action, "path").into(),
                        content: field(action, "content").into(),
                    }])
                } else {
                    serde_json::from_value(action["changes"].clone()).map_err(anyhow::Error::from)
                };
                changes.and_then(|changes| {
                    review_changes(
                        &mut self.screen,
                        permissions,
                        action,
                        directory,
                        changes,
                        &mut self.batch_undo,
                    )
                })
            }
            "web_search" => match run_web_search(
                &mut self.screen,
                &self.state.web,
                field(action, "query"),
                tr!("Ищу в интернете…", "Searching the web…"),
            )? {
                Some(result) => result,
                None => {
                    round.canceled_web_request = true;
                    Ok("User canceled the web search".into())
                }
            },
            "web_fetch" => self.web_fetch(action, permissions, round)?,
            "browser" => self.browser_action(action, permissions, vision)?,
            "preview" => self.preview_action(action, directory),
            #[cfg(windows)]
            "computer_list" | "computer_observe" | "computer_input" => {
                self.computer(action, permissions, vision)
            }
            "shell" if !self.diagnose => self.shell(action, directory),
            "skill" => self.skill(action),
            "mcp" if !self.diagnose => self.mcp_call(action, vision)?,
            "run" | "run_task" if !self.diagnose => self.run_task(action, directory),
            "edit" if !self.diagnose => {
                let edit = &action["edit"];
                let change = changes::Change::Edit {
                    path: field(edit, "path").into(),
                    before: field(edit, "before").into(),
                    after: field(edit, "after").into(),
                };
                review_changes(
                    &mut self.screen,
                    permissions,
                    action,
                    directory,
                    vec![change],
                    &mut self.batch_undo,
                )
            }
            _ => Err(anyhow::anyhow!("Action is unavailable in this mode")),
        })
    }

    fn delegate(
        &mut self,
        action: &Value,
        directory: &Path,
        turn_provider: &Provider,
        settings_changed: &mut bool,
    ) -> Result<Result<String>> {
        let limit = self
            .state
            .settings
            .delegate_limit(turn_provider.connection.id);
        Ok(match parse_subagent_tasks(&action["tasks"], limit) {
            Err(error) => Err(error),
            Ok(tasks) => {
                let details = tasks
                    .iter()
                    .map(|(role, task)| format!("{role}: {task}"))
                    .collect::<Vec<_>>()
                    .join("\n\n");
                let approved = self.screen.review(
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
                        let worker = scope
                            .spawn(|| run_subagent_group(provider, root, tasks, &cancel, &batch));
                        self.screen
                            .wait_for_tool(tr!("Саб-агенты", "Subagents"), &cancel, worker)
                    })?;
                    if let Some(learned) = batch.learned() {
                        let limits = &mut self.state.settings.provider_limits;
                        if limits.get(&connection).is_none_or(|current| {
                            learned.count < current.count
                                || (learned.count == current.count
                                    && current.estimated
                                    && !learned.estimated)
                        }) {
                            limits.insert(connection, learned);
                            *settings_changed = true;
                            self.screen.notice(&trf!(
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
                            Ok(text) => format!(
                                "## {role}\n{}",
                                text.chars().take(1024).collect::<String>()
                            ),
                            Err(error) => {
                                trf!(
                                    "## {role}\nСаб-агент завершился с ошибкой: {error}",
                                    "## {role}\nSubagent failed: {error}"
                                )
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n\n"))
                }
            }
        })
    }

    fn ask_user(&mut self, action: &Value) -> Result<Result<String>> {
        let options: Vec<String> = action["options"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .take(5)
            .map(|value| value.chars().take(180).collect())
            .collect();
        let question = field(action, "text");
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
            self.screen.choose(question, &menu)?
        };
        Ok(match choice {
            Some(index) if index < options.len() => Ok(options[index].clone()),
            Some(_) => Ok(self
                .screen
                .ask_text(question, false)?
                .unwrap_or_else(|| "User canceled the question".into())),
            None => Ok("User canceled the question".into()),
        })
    }

    fn web_fetch(
        &mut self,
        action: &Value,
        permissions: permissions::Mode,
        round: &mut ToolRound,
    ) -> Result<Result<String>> {
        Ok(
            if permissions == permissions::Mode::Ask
                && !self.screen.review(
                    tr!("Открыть страницу", "Open web page"),
                    field(action, "url"),
                )?
            {
                Ok("User declined the web request".into())
            } else {
                let url = field(action, "url").to_owned();
                match self.screen.wait_for_web(
                    tr!("Читаю страницу…", "Reading the page…"),
                    move |cancel| web::fetch(&url, cancel),
                )? {
                    Some(result) => result,
                    None => {
                        round.canceled_web_request = true;
                        Ok("User canceled the page request".into())
                    }
                }
            },
        )
    }

    fn browser_action(
        &mut self,
        action: &Value,
        permissions: permissions::Mode,
        vision: bool,
    ) -> Result<Result<String>> {
        let operation = field(action, "operation");
        Ok(
            if self.diagnose
                && !matches!(
                    operation,
                    "navigate" | "snapshot" | "tabs" | "console" | "screenshot" | "select_tab"
                )
            {
                Err(anyhow::anyhow!(
                    "Browser input is unavailable in diagnosis mode"
                ))
            } else if operation == "screenshot" && !vision {
                Err(anyhow::anyhow!(
                    "Enable image input in /model to use screenshots"
                ))
            } else if self.browser_enabled {
                let title = tr!("Действие в браузере", "Browser action");
                let detail = if operation == "screenshot" {
                    tr!(
                        "Снимок страницы будет отправлен выбранной модели.",
                        "The page screenshot will be sent to the selected model."
                    )
                    .to_owned()
                } else {
                    format!("{operation}\n{}", mcp::describe_arguments(action))
                };
                let reviewed = browser::review_required(permissions, operation, || {
                    self.preview
                        .as_ref()
                        .zip(self.browser.as_mut())
                        .is_some_and(|(preview, browser)| browser.is_local_preview(preview.url()))
                });
                if !reviewed || self.screen.review(title, &detail)? {
                    let started = if self.browser.is_none() {
                        browser::Browser::start().map(|session| {
                            self.browser = Some(session);
                        })
                    } else {
                        Ok(())
                    };
                    started
                        .and_then(|()| self.browser.as_mut().unwrap().call(operation, action))
                        .map(|(text, screenshot)| {
                            if let Some(data) = screenshot {
                                self.images.push(Visual {
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
            },
        )
    }

    fn preview_action(&mut self, action: &Value, directory: &Path) -> Result<String> {
        if field(action, "operation") == "stop" {
            self.preview = None;
            Ok("Preview stopped".into())
        } else if let Some(server) = &self.preview {
            Ok(format!("Preview: {}", preview::Preview::url(server)))
        } else if !self.diagnose {
            match preview::Preview::start(directory) {
                Ok(server) => {
                    let url = server.url().to_owned();
                    self.preview = Some(server);
                    Ok(format!("Preview: {url}"))
                }
                Err(error) => Err(error),
            }
        } else {
            Err(anyhow::anyhow!("Starting a preview requires code mode"))
        }
    }

    #[cfg(windows)]
    fn computer(
        &mut self,
        action: &Value,
        permissions: permissions::Mode,
        vision: bool,
    ) -> Result<String> {
        let operation = field(action, "action");
        ensure!(
            operation != "computer_input" || !self.diagnose,
            tr!(
                "Ввод недоступен в диагностике.",
                "Input is unavailable in diagnose mode."
            )
        );
        let stop = self.desktop_access.authorize(
            &mut self.screen,
            permissions,
            self.state.settings.desktop,
        )?;
        match operation {
            "computer_list" => {
                let list = desktop::list()
                    .iter()
                    .map(desktop::Window::describe)
                    .collect::<Vec<_>>();
                if list.is_empty() {
                    Ok(tr!("Нет доступных окон.", "No available windows.").into())
                } else {
                    Ok(list.join("\n"))
                }
            }
            "computer_observe" => {
                self.desktop = None;
                let screenshot = match field(action, "operation") {
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
                let window = desktop::find(field(action, "window")).context(tr!(
                    "Окно недоступно или найдено несколько совпадений. Вызови computer_list и передай ID окна.",
                    "The window is unavailable or ambiguous. Call computer_list and pass a window ID."
                ))?;
                let mut session = desktop::Session::new(window);
                let (text, image) = session.observe(screenshot, &stop)?;
                if let Some(data) = image {
                    self.images.push(Visual {
                        mime: "image/jpeg".into(),
                        data,
                    });
                }
                self.desktop = Some(session);
                Ok(text)
            }
            _ => self
                .desktop
                .as_mut()
                .context(tr!("Сначала прочитай окно.", "Observe the window first."))?
                .input(action, &stop),
        }
    }

    fn shell(&mut self, action: &Value, directory: &Path) -> Result<String> {
        let script = field(action, "command");
        let timeout = action["timeout"].as_u64().unwrap_or(120).clamp(1, 300);
        let path = action["path"]
            .as_str()
            .filter(|p| !p.is_empty())
            .unwrap_or(".");
        safe_path(directory, path).and_then(|cwd| {
            ensure!(cwd.is_dir(), "Expected a directory inside the project");
            let (command, detail) = tasks::shell(script)?;
            command_in_chat(
                &mut self.screen,
                CommandRequest {
                    root: directory,
                    cwd: &cwd,
                    process: command,
                    detail: &detail,
                    timeout,
                },
                &mut self.batch_undo,
            )
        })
    }

    fn skill(&self, action: &Value) -> Result<String> {
        let name = action["name"].as_str().unwrap_or("");
        match self.skills.find(name) {
            Some(skill) if !field(action, "path").is_empty() => {
                skills::resource(skill, field(action, "path"))
            }
            Some(skill) => {
                skills::body(skill).map(|body| format!("Skill loaded: {}\n\n{body}", skill.name))
            }
            None => Err(anyhow::anyhow!(
                "Unknown skill '{name}'. Use /skills to list the available ones."
            )),
        }
    }

    fn mcp_call(&mut self, action: &Value, vision: bool) -> Result<Result<String>> {
        let server = action["server"].as_str().unwrap_or("");
        let tool = action["tool"].as_str().unwrap_or("");
        let arguments = action
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        Ok(if server.is_empty() || tool.is_empty() {
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
            if self.screen.confirm(&title, &detail)? {
                self.hub
                    .call(server, tool, &arguments)
                    .and_then(|(text, screenshot)| {
                        if let Some(screenshot) = screenshot
                            && vision
                            && self.screen.confirm(
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
                            self.images.push(screenshot);
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
        })
    }

    fn run_task(&mut self, action: &Value, directory: &Path) -> Result<String> {
        let task = field(action, "task");
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
                &mut self.screen,
                CommandRequest {
                    root: directory,
                    cwd: &cwd,
                    process: command,
                    detail: &detail,
                    timeout: 300,
                },
                &mut self.batch_undo,
            )
        })
    }
}
