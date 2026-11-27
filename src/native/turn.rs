use super::actions::ToolRound;
use super::agent::{Agent, recovery_allows, turn_context};
use super::commands::{
    choose_reasoning, edit_agent_settings, live_reasoning, search_menu, show_web_search,
};
use super::*;

const MAX_TOOL_ROUNDS: usize = 96;

impl Agent<'_> {
    pub(super) fn run_turn(&mut self, turn_provider: &Provider, label: &str) -> Result<bool> {
        let directory_copy = self.directory.clone();
        let directory = directory_copy.as_path();
        let preferences = self.preferences();
        let mut draft = self.plan.clone();
        let mut settings_changed = false;
        let mut repeated_failure: Option<(String, String)> = None;
        let mut mode = self
            .state
            .request_mode(turn_provider, self.diagnose, self.planning);
        mode.headless = self.screen.is_headless();
        mode.creator = self.authoring && !self.diagnose && !self.planning;
        mode.browser = self.browser_enabled;
        mode.mcp = !self.hub.tools().is_empty();
        let mut step = 0;
        let mut stream_retries = 0;
        let mut compacted_step = None;
        while step < MAX_TOOL_ROUNDS {
            let provider = turn_provider.clone();
            let installed_tools = if provider.chat_only || self.state.settings.extensions.is_empty()
            {
                Vec::new()
            } else {
                extensions::Registry::load(self.root, directory, &self.state.settings.extensions)
                    .tools(mode)
            };
            let messages = self.history.clone();
            let extra = turn_context(
                &self.skills,
                &self.hub,
                directory,
                self.screen.permission_mode(),
                &draft,
                mode,
                &preferences,
            );
            let mut prepared = prepare_work_request_with_extensions(
                &provider,
                &messages,
                mode,
                &extra,
                &self.wire,
                &installed_tools,
            );
            if compacted_step != Some(step) && compact::needed(&provider, &prepared) {
                compacted_step = Some(step);
                match compact::run(
                    &mut self.screen,
                    &provider,
                    &mut self.history,
                    &mut self.wire,
                    compact::Trigger::Auto,
                    |messages, wire| {
                        input_estimate(&prepare_work_request_with_extensions(
                            &provider,
                            messages,
                            mode,
                            &extra,
                            wire,
                            &installed_tools,
                        ))
                    },
                )? {
                    compact::Outcome::Compacted => {
                        prepared = prepare_work_request_with_extensions(
                            &provider,
                            &self.history,
                            mode,
                            &extra,
                            &self.wire,
                            &installed_tools,
                        );
                    }
                    compact::Outcome::Canceled => break,
                    compact::Outcome::Skipped => {}
                }
            }
            self.screen.set_input_estimate(input_estimate(&prepared));
            let mut request_plan = match prepared {
                Ok(plan) => plan,
                Err(error) => {
                    chat_error(
                        &mut self.history,
                        tr!("Параметры запроса", "Request setup"),
                        error,
                    );
                    break;
                }
            };
            #[cfg(windows)]
            if self.desktop_access.paused() {
                self.images.clear();
                self.screen.desktop_active(false);
            }
            for screenshot in &self.images {
                attach_image(&provider, &mut request_plan.body, screenshot);
            }
            let input_tokens = request_plan.input_tokens;
            let canceled = Arc::new(AtomicBool::new(false));
            let cancel = canceled.clone();
            let (sender, receiver) = mpsc::channel::<Delta>();
            let chat_only = provider.chat_only;
            let reply_started = std::time::Instant::now();
            let task =
                std::thread::spawn(move || request(&provider, &request_plan, &cancel, sender));
            self.screen.reset_stream_escape();
            self.screen.track("", "");
            let mut output = String::new();
            let mut reasoning = String::new();
            while !task.is_finished() {
                for delta in receiver.try_iter() {
                    match delta {
                        Delta::Content(text) => output.push_str(&text),
                        Delta::Reasoning(text) => reasoning.push_str(&text),
                    }
                }
                match self.screen.streaming(
                    &self.history,
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
                        if self.recovery && !recovery_allows(&command) =>
                    {
                        self.screen.notice(tr!(
                            "Команда выключена в безопасном запуске.",
                            "Command disabled in recovery mode."
                        ));
                    }
                    ui::StreamControl::Command(command)
                        if self.planning && !permissions::plan_allows_command(&command) =>
                    {
                        self.screen.notice(tr!(
                            "Команда недоступна в /plan.",
                            "Command unavailable in /plan."
                        ));
                    }
                    ui::StreamControl::Command(command) => match command.as_str() {
                        "/permissions" => self.screen.permissions_menu()?,
                        "/help" => self.screen.help()?,
                        "/model" | "/models" | "/provider" | "/providers" => {
                            let old_choice = self.choice.clone();
                            if edit_agent_settings(
                                &mut self.screen,
                                &mut self.state,
                                &mut self.history,
                                &command,
                                &mut self.model_files,
                                &mut self.choice,
                            )? || old_choice != self.choice
                            {
                                settings_changed = true;
                                self.screen.notice(tr!(
                                    "Настройки подключения и модели применятся со следующего сообщения.",
                                    "Connection and model settings apply to the next message."
                                ));
                            }
                        }
                        "/reasoning" => {
                            if let Some(level) = choose_reasoning(
                                &mut self.screen,
                                &self.state,
                                self.choice.as_ref(),
                            )? {
                                settings_changed |= live_reasoning(
                                    &mut self.screen,
                                    &mut self.state,
                                    &mut self.model_files,
                                    self.choice.as_ref(),
                                    level,
                                );
                            }
                        }
                        value if value.starts_with("/reasoning ") => {
                            if let Some(level) =
                                Reasoning::parse(value.trim_start_matches("/reasoning ").trim())
                            {
                                settings_changed |= live_reasoning(
                                    &mut self.screen,
                                    &mut self.state,
                                    &mut self.model_files,
                                    self.choice.as_ref(),
                                    level,
                                );
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
                            if search_menu(
                                &mut self.screen,
                                &mut self.state.web,
                                &mut self.history,
                            )? {
                                save(self.profile.work.path(), &self.state)?;
                                self.profile.save()?;
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
                    },
                }
            }
            for delta in receiver.try_iter() {
                match delta {
                    Delta::Content(text) => output.push_str(&text),
                    Delta::Reasoning(text) => reasoning.push_str(&text),
                }
            }
            self.screen.reset_stream_escape();
            self.screen.track(&output, &reasoning);
            if canceled.load(Ordering::Relaxed) {
                self.images.clear();
                let partial = output.trim_start();
                let tool_text = (partial.starts_with('{') || partial.starts_with("```json"))
                    && partial.contains("\"action\"");
                if !partial.is_empty() && !tool_text {
                    self.history.push(Message {
                        role: "interrupted".into(),
                        content: output,
                        reasoning: None,
                        ..Default::default()
                    });
                }
                self.resume_turn = true;
                self.screen.notice(tr!(
                    "Запрос остановлен. /continue — продолжить.",
                    "Request stopped. Use /continue to resume."
                ));
                break;
            }
            let response = task
                .join()
                .unwrap_or_else(|_| Err(anyhow::anyhow!("Provider worker stopped")));
            let answer = match response {
                Ok(value) => value,
                Err(error) => {
                    let estimated = (output.len() + reasoning.len()).div_ceil(4) as u32;
                    self.screen.record_reply(
                        input_tokens,
                        estimated,
                        true,
                        reply_started.elapsed().as_secs_f32(),
                    );
                    let interrupted = error.is::<InterruptedStream>();
                    if interrupted && stream_retries == 0 {
                        stream_retries += 1;
                        self.screen.notice(tr!(
                            "Поток оборвался. Повторяю текущий запрос…",
                            "The stream disconnected. Retrying the current request…"
                        ));
                        continue;
                    }
                    self.images.clear();
                    chat_error(
                        &mut self.history,
                        tr!("Ошибка провайдера", "Provider error"),
                        error,
                    );
                    self.resume_turn = true;
                    self.screen.notice(tr!(
                        "Введи /continue, чтобы продолжить с текущего шага.",
                        "Enter /continue to resume from the current step."
                    ));
                    break;
                }
            };
            if stream_retries > 0 {
                self.screen.notice("");
                stream_retries = 0;
            }
            self.images.clear();
            let output_tokens = (answer.content.len()
                + reasoning.len().max(answer.raw_reasoning.len())
                + answer
                    .calls
                    .iter()
                    .map(|call| call.arguments.len())
                    .sum::<usize>())
            .div_ceil(4) as u32;
            self.screen.record_reply(
                answer.usage.input.unwrap_or(input_tokens),
                answer.usage.output.unwrap_or(output_tokens),
                answer.usage.input.is_none() || answer.usage.output.is_none(),
                reply_started.elapsed().as_secs_f32(),
            );
            let native = !answer.calls.is_empty() && !chat_only;
            let mut reasoning = answer.reasoning.clone();
            if !native || !answer.content.is_empty() {
                let entry = self.history.len();
                let fallback = !chat_only && !native && parse_action(&answer.content).is_some();
                self.history.push(Message {
                    role: if native { "narration" } else { "assistant" }.into(),
                    content: answer.content.clone(),
                    reasoning: reasoning.take(),
                    ..Default::default()
                });
                if !native && !fallback {
                    if self.planning && (!answer.content.trim().is_empty() || !draft.is_empty()) {
                        self.proposed_plan = Some(if draft.is_empty() {
                            answer.content.clone()
                        } else {
                            format!("{}\n\n{}", draft.render(), answer.content.trim())
                        });
                        self.screen
                            .notice(tr!("/code — проверить план.", "/code to review the plan."));
                    }
                    self.history[entry].receipt = self.screen.finish_turn();
                    if !answer.raw_reasoning.is_empty() {
                        self.wire.insert(entry, answer);
                    }
                    break;
                }
            }

            let (canceled_tool, changed) = self.tool_round(ToolRound {
                answer,
                native,
                reasoning,
                directory,
                label,
                turn_provider,
                mode,
                installed_tools: &installed_tools,
                draft: &mut draft,
                repeated_failure: &mut repeated_failure,
            })?;
            settings_changed |= changed;
            if canceled_tool {
                self.resume_turn = true;
                self.screen.notice(tr!(
                    "Шаг остановлен. /tasks — прогресс, /continue — продолжить.",
                    "Step stopped. Use /tasks for progress or /continue to resume."
                ));
                break;
            }
            step += 1;
            if step == MAX_TOOL_ROUNDS {
                self.resume_turn = true;
                self.screen.notice(tr!(
                    "Достигнут лимит шагов с инструментами. /continue — продолжить.",
                    "Tool-step limit reached. Use /continue to resume."
                ));
            }
        }

        self.plan = draft;
        Ok(settings_changed)
    }
}
