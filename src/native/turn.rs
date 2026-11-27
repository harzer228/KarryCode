use super::{
    Message, Provider,
    actions::ToolRound,
    agent::Agent,
    agent::prompt_extra,
    chat_error, compact, field, parse_action, permissions,
    provider::{
        Delta, InterruptedStream, Reply, RequestPlan, attach_image, input_estimate,
        prepare_work_request, request,
    },
    tools, ui,
};
use anyhow::Result;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};

const MAX_TOOL_ROUNDS: usize = 96;

/// Bookkeeping for one user turn, which may span several provider requests.
struct TurnState {
    settings_changed: bool,
    stream_retries: u32,
    compacted_step: Option<usize>,
}

/// What the turn loop does after one provider request.
enum Step {
    /// Tools ran; send their results in the next request.
    Next,
    /// Repeat the same request without counting a step.
    Retry,
    /// The turn is over.
    Stop,
}

struct Streamed {
    output: String,
    reasoning: String,
    /// `None` when the user stopped the reply.
    response: Option<Result<Reply>>,
}

impl Agent<'_> {
    /// Runs provider requests and tool rounds until the model answers, the user stops it or
    /// the step limit is reached. Returns whether settings changed during the turn.
    pub(super) fn run_turn(&mut self, turn_provider: &Provider, label: &str) -> Result<bool> {
        let mut turn = TurnState {
            settings_changed: false,
            stream_retries: 0,
            compacted_step: None,
        };
        let mut step = 0;
        while step < MAX_TOOL_ROUNDS {
            match self.step(turn_provider, label, step, &mut turn)? {
                Step::Next => {}
                Step::Retry => continue,
                Step::Stop => break,
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
        Ok(turn.settings_changed)
    }

    fn step(
        &mut self,
        turn_provider: &Provider,
        label: &str,
        step: usize,
        turn: &mut TurnState,
    ) -> Result<Step> {
        let provider = turn_provider.clone();
        let messages = self.history.clone();
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
        let mode = self
            .state
            .request_mode(&provider, self.diagnose, self.planning);
        let mut prepared = prepare_work_request(&provider, &messages, mode, &extra, &self.wire);
        if turn.compacted_step != Some(step) && compact::needed(&provider, &prepared) {
            turn.compacted_step = Some(step);
            match compact::run(
                &mut self.screen,
                &provider,
                &mut self.history,
                &mut self.wire,
                compact::Trigger::Auto,
                |messages, wire| {
                    input_estimate(&prepare_work_request(
                        &provider, messages, mode, &extra, wire,
                    ))
                },
            )? {
                compact::Outcome::Compacted => {
                    prepared =
                        prepare_work_request(&provider, &self.history, mode, &extra, &self.wire);
                }
                compact::Outcome::Canceled => return Ok(Step::Stop),
                compact::Outcome::Skipped => {}
            }
        }
        self.screen.set_input_estimate(input_estimate(&prepared));
        let mut plan = match prepared {
            Ok(plan) => plan,
            Err(error) => {
                chat_error(
                    &mut self.history,
                    tr!("Параметры запроса", "Request setup"),
                    error,
                );
                return Ok(Step::Stop);
            }
        };
        #[cfg(windows)]
        if self.desktop_access.paused() {
            self.images.clear();
            self.screen.desktop_active(false);
        }
        for screenshot in &self.images {
            attach_image(&provider, &mut plan.body, screenshot);
        }
        let input_tokens = plan.input_tokens;
        let chat_only = provider.chat_only;
        let reply_started = std::time::Instant::now();
        let Streamed {
            output,
            reasoning,
            response,
        } = self.stream_reply(
            provider,
            plan,
            &turn_provider.model,
            &mut turn.settings_changed,
        )?;
        let Some(response) = response else {
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
            self.screen.notice(tr!("Остановлено.", "Stopped."));
            return Ok(Step::Stop);
        };
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
                if interrupted && turn.stream_retries == 0 {
                    turn.stream_retries += 1;
                    self.screen.notice(tr!(
                        "Поток оборвался. Повторяю текущий запрос…",
                        "The stream disconnected. Retrying the current request…"
                    ));
                    return Ok(Step::Retry);
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
                return Ok(Step::Stop);
            }
        };
        if turn.stream_retries > 0 {
            self.screen.notice("");
            turn.stream_retries = 0;
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
            let index = self.history.len();
            let fallback = !chat_only && !native && parse_action(&answer.content).is_some();
            self.history.push(Message {
                role: if native { "narration" } else { "assistant" }.into(),
                content: answer.content.clone(),
                reasoning: reasoning.take(),
                ..Default::default()
            });
            if !native && !fallback {
                if self.planning && !answer.content.trim().is_empty() {
                    self.proposed_plan = Some(answer.content.clone());
                    self.screen
                        .notice(tr!("/code — проверить план.", "/code to review the plan."));
                }
                self.history[index].receipt = self.screen.finish_turn();
                if !answer.raw_reasoning.is_empty() {
                    self.wire.insert(index, answer);
                }
                return Ok(Step::Stop);
            }
        }
        self.tool_round(
            turn_provider,
            label,
            answer,
            native,
            reasoning,
            &mut turn.settings_changed,
        )
    }

    /// Streams one provider reply while serving the slash commands allowed mid-stream.
    fn stream_reply(
        &mut self,
        provider: Provider,
        plan: RequestPlan,
        model: &str,
        settings_changed: &mut bool,
    ) -> Result<Streamed> {
        let canceled = Arc::new(AtomicBool::new(false));
        let cancel = canceled.clone();
        let (sender, receiver) = mpsc::channel::<Delta>();
        let task = std::thread::spawn(move || request(&provider, &plan, &cancel, sender));
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
                model,
                &self.directory,
            )? {
                ui::StreamControl::Continue => {}
                ui::StreamControl::Stop => {
                    canceled.store(true, Ordering::Relaxed);
                    break;
                }
                ui::StreamControl::Command(command)
                    if self.planning && !permissions::plan_allows_command(&command) =>
                {
                    self.screen.notice(tr!(
                        "Команда недоступна в /plan.",
                        "Command unavailable in /plan."
                    ));
                }
                ui::StreamControl::Command(command) => {
                    self.stream_command(&command, settings_changed)?;
                }
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
        // A stopped request is left to finish on its own thread; it is not joined.
        let response = if canceled.load(Ordering::Relaxed) {
            None
        } else {
            Some(
                task.join()
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("Provider worker stopped"))),
            )
        };
        Ok(Streamed {
            output,
            reasoning,
            response,
        })
    }

    /// Runs the tool calls of one reply and records their results in the history.
    fn tool_round(
        &mut self,
        turn_provider: &Provider,
        label: &str,
        answer: Reply,
        native: bool,
        mut reasoning: Option<String>,
        settings_changed: &mut bool,
    ) -> Result<Step> {
        let first = self.history.len();
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
        let mut round = ToolRound::default();
        let mut errors = Vec::new();
        for (arguments, action) in actions {
            if let Some(arguments) = arguments {
                self.history.push(Message {
                    role: "assistant".into(),
                    content: arguments.clone(),
                    reasoning: reasoning.take(),
                    ..Default::default()
                });
            }
            if round.canceled_web_request {
                self.history.push(Message {
                    role: "user".into(),
                    content: "Tool result\nSkipped because the user stopped this turn".into(),
                    reasoning: None,
                    ..Default::default()
                });
                continue;
            }
            self.screen
                .tool_background(&self.history, label, &self.directory)?;
            let permissions = self.screen.permission_mode();
            let root = if let Some(error) = tools::unknown(&action) {
                Err(error)
            } else if self.planning && !permissions::plan_allows_action(&action) {
                Err(anyhow::anyhow!(tr!(
                    "Действие недоступно в /plan.",
                    "Action unavailable in /plan."
                )))
            } else {
                permissions::tool_root(permissions, &self.directory, &action)
            };
            let directory = match root {
                Ok(root) => root,
                Err(error) => {
                    let text = error.to_string();
                    errors.push(text.clone());
                    self.history.push(Message {
                        role: "user".into(),
                        content: format!("Tool result\n{text}"),
                        reasoning: None,
                        ..Default::default()
                    });
                    continue;
                }
            };
            let skill_result = field(&action, "action") == "skill";
            let result = self.dispatch(
                &action,
                &directory,
                permissions,
                turn_provider,
                &mut round,
                settings_changed,
            )?;
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
                round.events.push(Message {
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
            self.history.push(Message {
                role: "user".into(),
                content: format!("{prefix}\n{content}"),
                reasoning: None,
                ..Default::default()
            });
        }
        if native {
            self.wire.insert(first, answer);
        }
        self.history.extend(round.events);
        for error in errors {
            chat_error(
                &mut self.history,
                tr!("Ошибка инструмента", "Tool error"),
                error,
            );
        }
        if round.canceled_web_request {
            self.screen
                .notice(tr!("Веб-запрос отменён.", "Web request canceled."));
            return Ok(Step::Stop);
        }
        Ok(Step::Next)
    }
}
