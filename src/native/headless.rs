//! Console behind `karrycode -p`. The agent loop drives it through the same
//! `ui::Screen` calls as the interactive console: the task is the only input,
//! every approval or question is answered the way a user pressing Esc would,
//! progress goes to stderr and stdout carries only the final reply.

use super::{Message, parse_action};
use anyhow::{Result, anyhow};
use std::io::{self, Write};

pub(super) struct Headless {
    task: Option<String>,
    reported: usize,
    last_notice: String,
    outcome: Option<Result<String>>,
}

impl Headless {
    pub(super) fn new(task: String) -> Self {
        Self {
            task: Some(task),
            reported: 0,
            last_notice: String::new(),
            outcome: None,
        }
    }

    /// Hands out the task once; the next prompt means the turn is over.
    pub(super) fn input(&mut self, history: &[Message]) -> Option<String> {
        if let Some(task) = self.task.take() {
            return Some(task);
        }
        let outcome = outcome(history, &self.last_notice);
        let shown = if outcome.is_err() && history.last().is_some_and(|m| m.role == "error") {
            &history[..history.len() - 1]
        } else {
            history
        };
        self.progress(shown);
        self.outcome = Some(outcome);
        None
    }

    pub(super) fn progress(&mut self, history: &[Message]) {
        let start = self.reported.min(history.len());
        for message in &history[start..] {
            if let Some(line) = progress_line(message) {
                eprintln!("{line}");
            }
        }
        self.reported = history.len();
    }

    pub(super) fn notice(&mut self, text: &str) {
        let text = text.trim();
        // Resume hints name interactive commands that do not exist here; the
        // final error explains why the run stopped.
        if text.is_empty() || text.contains("/continue") {
            return;
        }
        self.last_notice = text.to_owned();
        eprintln!("{text}");
    }

    pub(super) fn declined(&self, title: &str) {
        eprintln!(
            "{}: {}",
            tr!("Отклонено без терминала", "Declined without a terminal"),
            one_line(title)
        );
    }

    pub(super) fn finish(&mut self) -> Result<()> {
        match self.outcome.take() {
            Some(Ok(reply)) => {
                let mut stdout = io::stdout().lock();
                writeln!(stdout, "{}", reply.trim_end())?;
                stdout.flush()?;
                Ok(())
            }
            Some(Err(error)) => Err(error),
            None => Err(anyhow!(tr!(
                "Агент остановился без ответа.",
                "The agent stopped without a reply."
            ))),
        }
    }
}

fn one_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(": ")
}

fn progress_line(message: &Message) -> Option<String> {
    match message.role.as_str() {
        "assistant" => {
            let action = parse_action(&message.content)?;
            let name = action["action"].as_str().unwrap_or("action");
            let detail = ["path", "command", "query", "url", "task", "text", "name"]
                .iter()
                .find_map(|key| action[*key].as_str())
                .or_else(|| action["edit"]["path"].as_str())
                .and_then(|detail| detail.lines().next())
                .unwrap_or("")
                .chars()
                .take(120)
                .collect::<String>();
            Some(if detail.is_empty() {
                format!("· {name}")
            } else {
                format!("· {name} {detail}")
            })
        }
        "error" => Some(format!("! {}", one_line(&message.content))),
        _ => None,
    }
}

fn outcome(history: &[Message], last_notice: &str) -> Result<String> {
    match history.last() {
        Some(message) if message.role == "assistant" && message.receipt.is_some() => {
            Ok(message.content.clone())
        }
        Some(message) if message.role == "error" => Err(anyhow!(one_line(&message.content))),
        Some(_) => Err(anyhow!(tr!(
            "Агент остановился без итогового ответа: достигнут лимит шагов с инструментами.",
            "The agent stopped without a final reply: the tool-step limit was reached."
        ))),
        None if !last_notice.is_empty() => Err(anyhow!(last_notice.to_owned())),
        None => Err(anyhow!(tr!(
            "Агент остановился без ответа.",
            "The agent stopped without a reply."
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::super::ui;
    use super::*;
    use std::{fs, time::Instant};

    fn message(role: &str, content: &str) -> Message {
        Message {
            role: role.into(),
            content: content.into(),
            ..Default::default()
        }
    }

    fn final_reply(content: &str) -> Message {
        let mut screen = ui::Screen::headless(String::new());
        screen.begin_turn(Instant::now());
        Message {
            receipt: screen.finish_turn(),
            ..message("assistant", content)
        }
    }

    #[test]
    fn every_prompt_is_declined() -> Result<()> {
        let mut screen = ui::Screen::headless("task".into());
        assert!(!screen.review("Run shell command", "rm -rf build")?);
        assert!(!screen.review_plan("1. Do it")?);
        assert!(!screen.confirm("MCP: files.write", "{}")?);
        assert_eq!(screen.choose("Pick", &["a".into(), "b".into()])?, None);
        assert_eq!(screen.ask_text("Question", false)?, None);
        let before = screen.permission_mode();
        assert!(!screen.request_permission(super::super::permissions::Mode::Full, "need it")?);
        assert!(screen.permission_mode() == before);
        Ok(())
    }

    #[test]
    fn the_task_is_the_only_input_and_the_turn_result_decides_the_exit() -> Result<()> {
        let mut screen = ui::Screen::headless("explain".into());
        let path = std::path::Path::new(".");
        assert_eq!(
            screen.input(&[], "model", path, false)?.as_deref(),
            Some("explain")
        );
        let history = vec![message("user", "explain"), final_reply("Done.")];
        assert_eq!(screen.input(&history, "model", path, false)?, None);
        assert!(screen.finish_headless().is_ok());

        let mut screen = ui::Screen::headless("explain".into());
        screen.input(&[], "model", path, false)?;
        let history = vec![
            message("user", "explain"),
            message("error", "Provider error\nHTTP 401"),
        ];
        assert_eq!(screen.input(&history, "model", path, false)?, None);
        let error = screen.finish_headless().unwrap_err().to_string();
        assert_eq!(error, "Provider error: HTTP 401");
        Ok(())
    }

    #[test]
    fn a_turn_without_a_final_reply_fails() {
        let tool = message("assistant", r#"{"action":"read","path":"a.rs"}"#);
        let history = vec![
            message("user", "task"),
            tool.clone(),
            message("user", "Tool result\nx"),
        ];
        assert!(outcome(&history, "").is_err());
        assert!(outcome(&[message("user", "task"), message("assistant", "text")], "").is_err());
        assert_eq!(
            outcome(&[], "Skill not found.").unwrap_err().to_string(),
            "Skill not found."
        );
        assert_eq!(progress_line(&tool).as_deref(), Some("· read a.rs"));
        assert_eq!(
            progress_line(&message("error", "Tool error\nNo such file")).as_deref(),
            Some("! Tool error: No such file")
        );
        assert_eq!(progress_line(&message("assistant", "plain text")), None);
    }

    fn read_request(stream: &mut std::net::TcpStream) -> Result<String> {
        use std::io::{BufRead, BufReader, Read};
        let mut reader = BufReader::new(stream);
        let mut length = 0;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line)?;
            if line.trim().is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse()?;
            }
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body)?;
        Ok(String::from_utf8(body)?)
    }

    #[test]
    fn one_headless_turn_runs_against_a_loopback_provider() -> Result<()> {
        use super::super::{State, models, run, save, state_path};
        use serde_json::json;
        if crate::platform::is_elevated()? {
            return Ok(());
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let url = format!("http://{}/v1", listener.local_addr()?);
        let replies = [
            json!({"action": "create_file", "path": "hello.txt", "content": "hi\n"}).to_string(),
            json!({"action": "request_permissions", "mode": "full", "reason": "Network"})
                .to_string(),
            "All done.".to_owned(),
        ];
        let server = std::thread::spawn(move || -> Result<Vec<String>> {
            let mut bodies = Vec::new();
            for reply in replies {
                let (mut stream, _) = listener.accept()?;
                bodies.push(read_request(&mut stream)?);
                let chunk =
                    json!({"choices": [{"delta": {"content": reply}, "finish_reason": "stop"}]});
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {chunk}\n\ndata: [DONE]\n\n"
                )?;
            }
            Ok(bodies)
        });
        let bundle = tempfile::tempdir()?;
        let project = tempfile::tempdir()?;
        let mut profile = crate::profile::Profile::scratch(bundle.path())?;
        let state = State {
            connections: vec![models::Connection {
                id: 1,
                name: "Loopback".into(),
                url,
                key: "fixture-key".into(),
                ..Default::default()
            }],
            models: vec![models::SavedModel {
                connection: 1,
                settings: models::ModelSettings {
                    name: "Fixture".into(),
                    model: "fixture".into(),
                    context: 32_000,
                    input: 28_000,
                    output: 4_000,
                    ..Default::default()
                },
            }],
            active_model: Some(0),
            ..State::default()
        };
        save(profile.work.path(), &state)?;
        let saved = fs::read(state_path(profile.work.path()))?;
        let mut workspace = project.path().to_path_buf();
        run(
            &mut profile,
            bundle.path(),
            &mut workspace,
            Some("Create hello.txt".into()),
            super::super::Launch::Normal,
        )?;
        let bodies = server.join().unwrap()?;
        assert_eq!(bodies.len(), 3);
        assert!(bodies[0].contains("Create hello.txt"));
        assert_eq!(
            fs::read_to_string(project.path().join("hello.txt"))?,
            "hi\n"
        );
        assert!(
            [
                "Tool unavailable in this mode",
                "Инструмент недоступен в этом режиме"
            ]
            .iter()
            .any(|text| bodies[2].contains(text))
        );
        assert_eq!(fs::read(state_path(profile.work.path()))?, saved);
        Ok(())
    }
}
