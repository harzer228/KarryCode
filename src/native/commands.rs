use super::actions::review_changes;
use super::agent::{Agent, restore_appearance, turn_context};
use super::*;

pub(super) enum Flow {
    Next,
    Quit,
    Turn(Option<String>),
}

impl Agent<'_> {
    pub(super) fn command(
        &mut self,
        command: &str,
        label: &str,
        current_connection: Option<u32>,
    ) -> Result<Flow> {
        let directory_copy = self.directory.clone();
        let directory = directory_copy.as_path();
        let preferences = self.preferences();
        let mut implementation = None;
        match command {
            "/setting" => {
                #[cfg(windows)]
                let paused = self.desktop_access.paused();
                #[cfg(not(windows))]
                let paused = false;
                match settings::menu(
                    &mut self.screen,
                    &mut self.state,
                    &mut self.theme,
                    settings::MenuContext {
                        work: self.profile.work.path(),
                        root: self.root,
                        packs: (!self.safe_ui).then_some(self.packs.as_slice()),
                        connection: current_connection,
                    },
                    paused,
                    |screen, state| {
                        let label = if current_connection.is_some() {
                            label
                        } else {
                            no_model_label(state)
                        };
                        screen.redraw_language(&self.history, label, directory)
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
                return Ok(Flow::Next);
            }
            "/tasks" => {
                self.history.push(Message {
                    role: "event".into(),
                    content: self.plan.render(),
                    ..Default::default()
                });
                return Ok(Flow::Next);
            }
            "/creator" => {
                if self.diagnose || self.screen.dialog_mode() == DialogMode::Chat {
                    self.screen.notice(tr!(
                        "Для Creator выбери режим агента и /code.",
                        "Switch to Agent and /code for Creator."
                    ));
                } else {
                    self.authoring = !self.authoring;
                    if self.authoring {
                        self.screen.localized_notice(
                            "Creator включён. Опиши инструмент, скилл или оформление, которое хочешь добавить.",
                            "Creator is on. Describe the tool, skill or appearance you want to add.",
                        );
                    } else {
                        self.screen
                            .notice(tr!("Creator выключен.", "Creator is off."));
                    }
                }
                return Ok(Flow::Next);
            }
            "/extensions" => {
                match extensions::menu(
                    &mut self.screen,
                    self.root,
                    directory,
                    &mut self.state.settings.extensions,
                    &self.registry,
                ) {
                    Ok(true) => {
                        save(self.profile.work.path(), &self.state)?;
                        self.profile.save()?;
                        self.registry = extensions::Registry::load(
                            self.root,
                            directory,
                            &self.state.settings.extensions,
                        );
                        self.skills = skills::discover(directory, self.root);
                        self.registry.skills(&mut self.skills);
                        if !self.safe_ui {
                            self.packs = self.registry.appearances();
                            restore_appearance(
                                &mut self.screen,
                                &self.packs,
                                &self.state.settings,
                            )?;
                        }
                    }
                    Ok(false) => {}
                    Err(error) => chat_error(
                        &mut self.history,
                        tr!("Ошибка расширения", "Extension error"),
                        format_args!("{error:#}"),
                    ),
                }
                return Ok(Flow::Next);
            }
            "/config" => {
                let revision =
                    model_config::accept_project(&mut self.screen, &mut self.state, directory);
                match revision {
                    Ok(true) => {
                        self.wire.clear();
                        self.images.clear();
                        save(self.profile.work.path(), &self.state)?;
                        self.profile.save()?;
                    }
                    Err(error) => chat_error(
                        &mut self.history,
                        tr!("Конфиг проекта", "Project configuration"),
                        format_args!("{error:#}"),
                    ),
                    _ => {}
                }
                let effective = match self.project_models.resolve(
                    &self.state,
                    directory,
                    self.choice.as_ref(),
                    self.screen.dialog_mode(),
                ) {
                    Ok(Some(model)) => format!(
                        "{} / {} · {} · {} / {} / {}",
                        model.connection.name,
                        model.model,
                        model.reasoning.identifier(),
                        model.context,
                        model.input_limit(),
                        model.output_limit()
                    ),
                    Ok(None) => no_model_label(&self.state).into(),
                    Err(error) => format!("{error:#}"),
                };
                let error = self.last_config_error.as_deref().unwrap_or("");
                self.screen.report(
                    tr!("Конфигурация моделей", "Model configuration"),
                    &trf!(
                        "Профиль: {}\nПроект: {}\nТекущая модель: {effective}\n{error}",
                        "Profile: {}\nProject: {}\nEffective model: {effective}\n{error}",
                        self.model_files.location().display(),
                        model_config::project_path(directory).display()
                    ),
                )?;
                return Ok(Flow::Next);
            }
            command if command == "/cd" || command.starts_with("/cd ") => {
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
                    Ok(Some(next)) if next != self.directory => {
                        if !self.temporary {
                            let old_index = self.index;
                            let previous = self.state.sessions[self.index].clone();
                            let recent = self.profile.recent.clone();
                            self.profile.recent.remember(self.root, &next);
                            self.index = move_session(
                                &mut self.state.sessions,
                                self.index,
                                &self.bundle,
                                &next,
                            );
                            let mut moved_plan = self.plan.clone();
                            moved_plan.recheck();
                            self.state.sessions[self.index].plan = moved_plan;
                            self.state.sessions[self.index].proposal = None;
                            self.state.sessions[self.index].model = self.choice.clone();
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
                                return Ok(Flow::Next);
                            }
                            *self.workspace = next.clone();
                        }
                        self.directory = next;
                        self.plan.recheck();
                        self.results = project_tools::Results::default();
                        self.last_check = None;
                        self.skills = skills::discover(&self.directory, self.root);
                        self.registry = extensions::Registry::load(
                            self.root,
                            &self.directory,
                            &self.state.settings.extensions,
                        );
                        self.registry.skills(&mut self.skills);
                        if !self.safe_ui {
                            self.packs = self.registry.appearances();
                            restore_appearance(
                                &mut self.screen,
                                &self.packs,
                                &self.state.settings,
                            )?;
                        }
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
                return Ok(Flow::Next);
            }
            "/agents" => {
                self.screen.report(tr!("Встроенные саб-агенты", "Built-in subagents"), tr!(
                    "Исследователь — изучает назначенную тему и указывает файлы и строки.\nРевьюер — независимо ищет дефекты в выбранной области.\n\nПо команде delegate можно запустить группу задач параллельно в пределах лимита из /setting после подтверждения. Доступ только на чтение к выбранному проекту; результаты отправляются активному провайдеру. Без shell, MCP, записи и вложенных агентов.",
                    "Researcher — investigates the assigned question and cites files and lines.\nReviewer — independently looks for defects in the assigned scope.\n\nThe delegate tool can run a group of tasks in parallel up to the limit in /setting after confirmation. Workers get read-only access to the selected project; findings are sent to the active provider. No shell, MCP, writes, or nested agents."
                ))?;
                return Ok(Flow::Next);
            }
            "/theme" => {
                match settings::appearance_menu(
                    &mut self.screen,
                    &mut self.state.settings,
                    &mut self.theme,
                    self.profile.work.path(),
                    (!self.safe_ui).then_some(self.packs.as_slice()),
                ) {
                    Ok(true) => {
                        save(self.profile.work.path(), &self.state)?;
                        self.profile.save()?;
                        self.screen
                            .redraw_language(&self.history, label, directory)?;
                    }
                    Ok(false) => {}
                    Err(error) => self.screen.notice(&format!("{error:#}")),
                }
                return Ok(Flow::Next);
            }
            "/header" => {
                self.screen.toggle_header();
                return Ok(Flow::Next);
            }
            "/plan" | "/plan cancel" if self.planning => {
                self.proposed_plan = None;
                self.plan.cancel_draft();
                self.planning = false;
                self.resume_turn = false;
                self.screen.set_planning(false);
                self.screen
                    .notice(tr!("План не принят.", "Plan not accepted."));
                if !self.temporary {
                    self.state.sessions[self.index].proposal = None;
                    self.state.sessions[self.index].plan = self.plan.clone();
                    save(self.profile.work.path(), &self.state)?;
                    self.profile.save()?;
                }
                return Ok(Flow::Next);
            }
            "/plan" => {
                self.proposed_plan = None;
                self.resume_turn = false;
                self.images.clear();
                self.planning = true;
                self.diagnose = false;
                self.screen.set_planning(true);
                self.screen.notice("");
                return Ok(Flow::Next);
            }
            "/model default" => {
                self.choice = None;
                self.wire.clear();
                self.images.clear();
                if !self.temporary {
                    self.state.sessions[self.index].model = None;
                    save(self.profile.work.path(), &self.state)?;
                    self.profile.save()?;
                }
                self.screen.notice(tr!(
                    "Модель выбирается из конфига.",
                    "Model selection follows configuration."
                ));
                return Ok(Flow::Next);
            }
            "/quit" | "/exit" => return Ok(Flow::Quit),
            "/undo" => {
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
                return Ok(Flow::Next);
            }
            "/help" => {
                self.screen.help()?;
                return Ok(Flow::Next);
            }
            "/model" | "/models" | "/provider" | "/providers" => {
                let before = self.state.active_provider(DialogMode::Agent);
                let old_choice = self.choice.clone();
                let changed = edit_agent_settings(
                    &mut self.screen,
                    &mut self.state,
                    &mut self.history,
                    command,
                    &mut self.model_files,
                    &mut self.choice,
                )?;
                if changed || old_choice != self.choice {
                    if self.state.active_provider(DialogMode::Agent) != before {
                        self.wire.clear();
                        self.images.clear();
                    }
                    if !self.temporary {
                        self.state.sessions[self.index].model = self.choice.clone();
                    }
                    save(self.profile.work.path(), &self.state)?;
                    self.profile.save()?;
                    if matches!(command, "/model" | "/models") {
                        self.screen
                            .notice(tr!("Список моделей сохранён.", "Models saved."));
                    }
                }
                return Ok(Flow::Next);
            }
            "/skills" => {
                self.skills = skills::discover(directory, self.root);
                self.registry.skills(&mut self.skills);
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
                return Ok(Flow::Next);
            }
            "/mcp" => {
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
                        ui::McpAction::Approve(entry) => {
                            if let Some(config) = self.hub.pending(entry).cloned()
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
                                .filter_map(|entry| candidates.get(*entry))
                                .cloned()
                                .collect();
                            match self.hub.import(directory, &selected) {
                                Ok(message) => self.screen.notice(&message),
                                Err(error) => self.screen.notice(&error.to_string()),
                            }
                        }
                    }
                }
                return Ok(Flow::Next);
            }
            "/reasoning" => {
                if let Some(level) =
                    choose_reasoning(&mut self.screen, &self.state, self.choice.as_ref())?
                {
                    match change_reasoning(
                        &mut self.state,
                        &mut self.model_files,
                        self.choice.as_ref(),
                        level,
                    ) {
                        Ok(()) => {
                            save(self.profile.work.path(), &self.state)?;
                            self.profile.save()?;
                            self.screen.notice(&trf!(
                                "Reasoning: {}",
                                "Reasoning: {}",
                                level.label()
                            ));
                        }
                        Err(error) => self.screen.notice(&error.to_string()),
                    }
                }
                return Ok(Flow::Next);
            }
            command if command.starts_with("/reasoning ") => {
                let value = command.trim_start_matches("/reasoning ").trim();
                match Reasoning::parse(value) {
                    Some(level) => {
                        match change_reasoning(
                            &mut self.state,
                            &mut self.model_files,
                            self.choice.as_ref(),
                            level,
                        ) {
                            Ok(()) => {
                                save(self.profile.work.path(), &self.state)?;
                                self.profile.save()?;
                                self.screen.notice(&trf!(
                                    "Reasoning: {}",
                                    "Reasoning: {}",
                                    level.label()
                                ));
                            }
                            Err(error) => self.screen.notice(&error.to_string()),
                        }
                    }
                    None => self.screen.notice(tr!(
                        "Уровни: default, off, low, medium, high, xhigh, max, ultra",
                        "Levels: default, off, low, medium, high, xhigh, max, ultra"
                    )),
                }
                return Ok(Flow::Next);
            }
            "/init" => {
                match init_document(directory) {
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
                return Ok(Flow::Next);
            }
            "/diagnose" => {
                self.diagnose = true;
                return Ok(Flow::Next);
            }
            "/code" => {
                if self.planning {
                    let Some(proposal) = self.proposed_plan.as_deref() else {
                        self.screen.notice(tr!(
                            "Сначала получи план или введи /plan cancel.",
                            "Get a plan first or enter /plan cancel."
                        ));
                        return Ok(Flow::Next);
                    };
                    if !matches!(
                        self.project_models.resolve(
                            &self.state,
                            directory,
                            self.choice.as_ref(),
                            self.screen.dialog_mode()
                        ),
                        Ok(Some(_))
                    ) {
                        self.screen.notice(tr!(
                            "Проверь подключение и модель через /config.",
                            "Check the connection and model through /config."
                        ));
                        return Ok(Flow::Next);
                    }
                    let review = proposal.to_owned();
                    if !self.screen.review_plan(&review)? {
                        return Ok(Flow::Next);
                    }
                    if self.screen.dialog_mode() == DialogMode::Chat {
                        self.screen.notice(tr!(
                            "Реализация недоступна в режиме «Только чат».",
                            "Implementation is not available in Chat only mode."
                        ));
                        return Ok(Flow::Next);
                    }
                    self.plan.approve();
                    self.proposed_plan = None;
                    implementation = Some(review);
                    self.planning = false;
                    self.resume_turn = false;
                    self.screen.set_planning(false);
                    self.screen.notice("");
                }
                self.diagnose = false;
                if implementation.is_none() {
                    return Ok(Flow::Next);
                }
            }
            "/new" => {
                let (project, portable) = project_location(&self.bundle, directory);
                self.state.sessions.push(Session {
                    project,
                    portable,
                    messages: vec![],
                    mode: DialogMode::Agent,
                    ..Default::default()
                });
                self.index = self.state.sessions.len() - 1;
                self.history.clear();
                self.wire.clear();
                self.images.clear();
                self.proposed_plan = None;
                self.plan = work_plan::WorkPlan::default();
                self.choice = None;
                self.results = project_tools::Results::default();
                self.authoring = false;
                self.last_check = None;
                self.resume_turn = false;
                #[cfg(windows)]
                {
                    self.desktop_access.reset_chat();
                    self.desktop = None;
                    self.screen.desktop_active(false);
                }
                self.screen.reset_conversation();
                self.screen
                    .tool_background(&self.history, label, directory)?;
                self.temporary = false;
                save(self.profile.work.path(), &self.state)?;
                self.profile.save()?;
                return Ok(Flow::Next);
            }
            "/sessions" => {
                let (project, portable) = project_location(&self.bundle, directory);
                let candidates: Vec<_> = self
                    .state
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
                    self.plan = self.state.sessions[self.index].plan.clone();
                    self.proposed_plan = self.state.sessions[self.index].proposal.clone();
                    self.planning |= self.proposed_plan.is_some();
                    self.choice = self.state.sessions[self.index].model.clone();
                    self.results = project_tools::Results::default();
                    self.authoring = false;
                    self.last_check = None;
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
                    self.screen.set_planning(self.planning);
                    self.temporary = false;
                }
                return Ok(Flow::Next);
            }
            "/temporary" => {
                self.temporary = true;
                self.history.clear();
                self.wire.clear();
                self.images.clear();
                self.proposed_plan = None;
                self.plan = work_plan::WorkPlan::default();
                self.choice = None;
                self.results = project_tools::Results::default();
                self.authoring = false;
                self.last_check = None;
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
                return Ok(Flow::Next);
            }
            "/clear" => {
                self.history.clear();
                self.wire.clear();
                self.images.clear();
                self.proposed_plan = None;
                self.plan = work_plan::WorkPlan::default();
                self.results = project_tools::Results::default();
                self.last_check = None;
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
                    self.state.sessions[self.index].plan = self.plan.clone();
                    self.state.sessions[self.index].proposal = None;
                    save(self.profile.work.path(), &self.state)?;
                    self.profile.save()?;
                }
                return Ok(Flow::Next);
            }
            "/compact" => {
                let Ok(Some(provider)) = self.project_models.resolve(
                    &self.state,
                    directory,
                    self.choice.as_ref(),
                    self.screen.dialog_mode(),
                ) else {
                    self.screen
                        .notice(tr!("Выбери модель в /model.", "Choose a model in /model."));
                    return Ok(Flow::Next);
                };
                let mode = self
                    .state
                    .request_mode(&provider, self.diagnose, self.planning);
                let extra = turn_context(
                    &self.skills,
                    &self.hub,
                    directory,
                    self.screen.permission_mode(),
                    &self.plan,
                    mode,
                    &preferences,
                );
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
                    save(self.profile.work.path(), &self.state)?;
                    self.profile.save()?;
                }
                return Ok(Flow::Next);
            }
            "/permissions" => {
                self.screen.permissions_menu()?;
                return Ok(Flow::Next);
            }
            "/tools" => {
                if self.screen.dialog_mode() == DialogMode::Chat {
                    self.screen.report(tr!("Инструменты", "Tools"), tr!(
                        "В режиме «Только чат» инструменты модели выключены. Режим меняется в /permissions.",
                        "Model tools are off in Chat only. Change the conversation mode in /permissions."
                    ))?;
                    return Ok(Flow::Next);
                }
                let mut tool_mode = self
                    .state
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
                    );
                tool_mode.creator = self.authoring;
                tool_mode.browser = self.browser_enabled;
                tool_mode.search = self.state.web.enabled();
                tool_mode.mcp = !self.hub.tools().is_empty();
                self.registry = extensions::Registry::load(
                    self.root,
                    directory,
                    &self.state.settings.extensions,
                );
                let mut details = tools::names(tool_mode).join("\n");
                for tool in self.registry.tools(tool_mode) {
                    details.push_str(&format!("\n{} · {}", tool.name, tool.description));
                }
                for error in &self.registry.errors {
                    details.push_str(&format!("\n{error}"));
                }
                if self.planning {
                    self.screen.report(tr!("Инструменты", "Tools"), &details)?;
                    return Ok(Flow::Next);
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
                self.screen.report(tr!("Инструменты", "Tools"), &details)?;
                return Ok(Flow::Next);
            }
            #[cfg(windows)]
            desktop::STOP_COMMAND => {
                self.desktop = None;
                self.desktop_access.stop();
                self.images.clear();
                self.screen.desktop_active(false);
                self.screen
                    .notice(tr!("Управление остановлено.", "Window control stopped."));
                return Ok(Flow::Next);
            }
            "/browser" => {
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
                            Err(error) => {
                                chat_error(&mut self.history, tr!("Браузер", "Browser"), error)
                            }
                        }
                    }
                    Some(1) => {
                        self.browser = None;
                        self.browser_enabled = false;
                        self.images.clear();
                    }
                    _ => {}
                }
                return Ok(Flow::Next);
            }
            "/web" | "/search" => {
                if search_menu(&mut self.screen, &mut self.state.web, &mut self.history)? {
                    save(self.profile.work.path(), &self.state)?;
                    self.profile.save()?;
                }
                return Ok(Flow::Next);
            }
            text if text.starts_with("/search ") => {
                show_web_search(
                    &mut self.screen,
                    &self.state.web,
                    &mut self.history,
                    text.trim_start_matches("/search ").trim(),
                )?;
                return Ok(Flow::Next);
            }
            "/project" => {
                let choices = vec![
                    "Web".into(),
                    "Python".into(),
                    "Rust".into(),
                    tr!("С нуля", "From scratch").into(),
                ];
                if let Some(starter) = self
                    .screen
                    .choose(tr!("Создать проект", "Create project"), &choices)?
                {
                    if starter == 3 {
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
                            directory,
                            project::starter(starter),
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
                return Ok(Flow::Next);
            }
            "/continue" if self.resume_turn => {}
            "/continue" => {
                self.screen
                    .notice(tr!("Нет запроса для продолжения.", "No request to resume."));
                return Ok(Flow::Next);
            }
            command if command.starts_with("/skill ") => {}
            command if command.starts_with('/') => {
                self.screen.notice(tr!(
                    "Неизвестная команда. /help — справка.",
                    "Unknown command. /help lists commands."
                ));
                return Ok(Flow::Next);
            }
            _ => {}
        }

        Ok(Flow::Turn(implementation))
    }
}
pub(super) fn edit_agent_settings(
    screen: &mut ui::Screen,
    state: &mut State,
    history: &mut Vec<Message>,
    command: &str,
    files: &mut model_config::ModelFiles,
    choice: &mut Option<model_config::Choice>,
) -> Result<bool> {
    let before = model_config::Snapshot::capture(state);
    let old_choice = choice.clone();
    let edited = if matches!(command, "/provider" | "/providers") {
        models::provider_menu(screen, state)
    } else {
        models::model_menu(screen, state, history, choice)
    };
    let changed = match edited {
        Ok(changed) => changed,
        Err(error) => {
            before.restore(state);
            *choice = old_choice;
            chat_error(
                history,
                tr!("Настройки модели", "Model settings"),
                format_args!("{error:#}"),
            );
            return Ok(false);
        }
    };
    if changed && let Err(error) = files.write(state) {
        before.restore(state);
        *choice = old_choice;
        screen.notice("");
        chat_error(history, tr!("Конфиг моделей", "Model configuration"), error);
        return Ok(false);
    }
    Ok(changed)
}

pub(super) fn slash_command(text: &str) -> bool {
    !text.contains('\n') && text.trim_start().starts_with('/')
}

pub(super) fn choose_reasoning(
    screen: &mut ui::Screen,
    state: &State,
    choice: Option<&model_config::Choice>,
) -> Result<Option<Reasoning>> {
    let Some(model) =
        model_config::selected_index(state, choice)?.and_then(|index| state.models.get(index))
    else {
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

pub(super) fn set_reasoning(
    state: &mut State,
    choice: Option<&model_config::Choice>,
    level: Reasoning,
) -> Result<()> {
    let index = model_config::selected_index(state, choice)?
        .context(tr!("Модель не выбрана", "No model selected"))?;
    let saved = &state.models[index];
    let connection = state.connection(saved.connection).context(tr!(
        "Подключение модели не найдено",
        "Missing model connection"
    ))?;
    let mut provider = saved.settings.resolve(connection, DialogMode::Agent);
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

pub(super) fn init_document(directory: &Path) -> Result<Option<(PathBuf, String)>> {
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

pub(super) fn move_session(
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

pub(super) fn change_reasoning(
    state: &mut State,
    files: &mut model_config::ModelFiles,
    choice: Option<&model_config::Choice>,
    level: Reasoning,
) -> Result<()> {
    let before = model_config::Snapshot::capture(state);
    set_reasoning(state, choice, level)?;
    if let Err(error) = files.write(state) {
        before.restore(state);
        return Err(error);
    }
    Ok(())
}

pub(super) fn live_reasoning(
    screen: &mut ui::Screen,
    state: &mut State,
    files: &mut model_config::ModelFiles,
    choice: Option<&model_config::Choice>,
    level: Reasoning,
) -> bool {
    match change_reasoning(state, files, choice, level) {
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

pub(super) fn search_menu(
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

pub(super) fn show_web_search(
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
