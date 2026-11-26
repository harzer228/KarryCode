use super::{
    Message, Provider, field,
    files::{list_files, read_file, safe_path, search_project},
    parse_action, permissions, project,
    provider::{RequestMode, WireCalls, prepare_work_request, request_with_timeout},
    tools,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fmt, fs,
    path::{Path, PathBuf},
    sync::{
        Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

pub(super) const MAX_SUBAGENTS: u8 = 30;

#[derive(Clone, Copy, Serialize, Deserialize)]
pub(super) struct Limit {
    pub count: u8,
    pub estimated: bool,
}

#[derive(Debug)]
pub(super) struct ConcurrencyError {
    maximum: Option<u8>,
    detail: String,
}

impl fmt::Display for ConcurrencyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for ConcurrencyError {}

impl ConcurrencyError {
    pub fn redact(&mut self, key: &str) {
        self.detail = self.detail.replace(key, "<key>");
    }

    pub fn parse(body: &str, detail: String) -> Option<Self> {
        let data: Value = serde_json::from_str(body).unwrap_or(Value::Null);
        let error = data.get("error").unwrap_or(&data);
        let code = error
            .get("code")
            .or_else(|| error.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or(body)
            .to_ascii_lowercase();
        let explicit = matches!(
            code,
            "concurrency_limit_exceeded"
                | "concurrent_requests_limit_exceeded"
                | "too_many_concurrent_requests"
        ) || [
            "too many concurrent requests",
            "maximum concurrent requests",
            "concurrent request limit",
            "concurrent requests limit",
            "concurrency limit",
            "too many simultaneous requests",
        ]
        .iter()
        .any(|phrase| message.contains(phrase));
        let maximum = ["concurrency_limit", "max_concurrent_requests"]
            .iter()
            .find_map(|key| {
                error
                    .get(key)
                    .or_else(|| data.get(key))
                    .and_then(Value::as_u64)
            })
            .or_else(|| explicit.then(|| error["limit"].as_u64()).flatten())
            .or_else(|| {
                [
                    "maximum concurrent requests:",
                    "concurrency limit:",
                    "concurrent request limit:",
                    "maximum of ",
                ]
                .iter()
                .find_map(|prefix| {
                    let (_, rest) = message.split_once(prefix)?;
                    let rest = rest.trim_start();
                    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
                    if *prefix == "maximum of "
                        && !rest[digits..]
                            .trim_start()
                            .starts_with("concurrent requests")
                    {
                        return None;
                    }
                    rest[..digits].parse::<u64>().ok()
                })
            });
        (explicit || maximum.is_some()).then(|| Self {
            maximum: maximum
                .filter(|&value| value > 0)
                .map(|value| value.min(u64::from(MAX_SUBAGENTS)) as u8),
            detail,
        })
    }
}

pub(super) struct Batch {
    initial: u8,
    state: Mutex<Active>,
    available: Condvar,
}

struct Active {
    running: u8,
    ceiling: u8,
    learned: Option<Limit>,
}

impl Batch {
    pub fn new(count: u8) -> Self {
        Self {
            initial: count,
            state: Mutex::new(Active {
                running: 0,
                ceiling: count,
                learned: None,
            }),
            available: Condvar::new(),
        }
    }

    pub fn learned(&self) -> Option<Limit> {
        self.state.lock().unwrap().learned
    }

    pub fn request<T>(&self, canceled: &AtomicBool, call: impl FnOnce() -> Result<T>) -> Result<T> {
        let mut state = self.state.lock().unwrap();
        while state.running >= state.ceiling && !canceled.load(Ordering::Relaxed) {
            state = self
                .available
                .wait_timeout(state, Duration::from_millis(50))
                .unwrap()
                .0;
        }
        ensure!(
            !canceled.load(Ordering::Relaxed),
            tr!("Саб-агент остановлен", "Subagent canceled")
        );
        state.running += 1;
        drop(state);
        let _slot = Slot(self);
        let result = call();
        if let Err(error) = &result
            && let Some(error) = error.downcast_ref::<ConcurrencyError>()
        {
            let limit = Limit {
                count: error.maximum.unwrap_or((self.initial / 2).max(1)),
                estimated: error.maximum.is_none(),
            };
            let mut state = self.state.lock().unwrap();
            if state.learned.is_none_or(|current| {
                (current.estimated && !limit.estimated)
                    || (current.estimated == limit.estimated && limit.count < current.count)
            }) {
                state.learned = Some(limit);
                state.ceiling = state.ceiling.min(limit.count);
            }
        }
        result
    }
}

struct Slot<'a>(&'a Batch);

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        self.0.state.lock().unwrap().running -= 1;
        self.0.available.notify_all();
    }
}

pub(super) fn parse_subagent_tasks(value: &Value, limit: u8) -> Result<Vec<(String, String)>> {
    ensure!(
        limit > 0,
        tr!(
            "Саб-агенты отключены в /setting",
            "Subagents are disabled in /setting"
        )
    );
    let tasks = value.as_array().context(tr!(
        "Задачи саб-агентов должны быть массивом",
        "Subagent tasks must be an array"
    ))?;
    ensure!(
        (1..=usize::from(limit)).contains(&tasks.len()),
        trf!(
            "Можно запустить от 1 до {limit} задач только на чтение",
            "Delegate 1–{limit} read-only subtasks"
        )
    );
    tasks
        .iter()
        .map(|task| {
            let role = field(task, "role");
            ensure!(
                matches!(role, "researcher" | "reviewer"),
                tr!("Неизвестная роль саб-агента", "Unknown subagent role")
            );
            let task = field(task, "task").trim();
            ensure!(
                !task.is_empty() && task.len() <= 1000,
                tr!(
                    "Задача саб-агента должна занимать от 1 до 1000 байт",
                    "Subagent task must be 1–1000 bytes"
                )
            );
            Ok((role.to_owned(), task.to_owned()))
        })
        .collect()
}

pub(super) fn run_subagent_group(
    provider: Provider,
    root: PathBuf,
    tasks: Vec<(String, String)>,
    canceled: &AtomicBool,
    batch: &Batch,
) -> Vec<(String, Result<String>)> {
    std::thread::scope(|scope| {
        let handles: Vec<_> = tasks
            .into_iter()
            .map(|(role, task)| {
                let provider = provider.clone();
                let root = root.clone();
                scope.spawn(move || {
                    let result = run_subagent(provider, root, &role, &task, canceled, batch);
                    (role, result)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| {
                handle.join().unwrap_or_else(|_| {
                    (
                        "worker".into(),
                        Err(anyhow::anyhow!(tr!(
                            "Саб-агент неожиданно остановился",
                            "Subagent stopped unexpectedly"
                        ))),
                    )
                })
            })
            .collect()
    })
}

fn run_subagent(
    provider: Provider,
    root: PathBuf,
    role: &str,
    task: &str,
    canceled: &AtomicBool,
    batch: &Batch,
) -> Result<String> {
    ensure!(
        !provider.chat_only,
        tr!(
            "Саб-агентам нужна модель в режиме агента",
            "Subagents need an agent-mode model"
        )
    );
    let instruction = match role {
        "researcher" => {
            "Investigate the assigned question and report relevant files and line numbers."
        }
        "reviewer" => {
            "Independently review the assigned scope for likely defects and report evidence and file/line locations."
        }
        _ => bail!(tr!("Неизвестная роль саб-агента", "Unknown subagent role")),
    };
    let mut history = vec![Message {
        role: "user".into(),
        content: format!("{instruction}\n\nTask: {task}"),
        ..Default::default()
    }];
    let mut wire = WireCalls::new();
    let mut total_bytes = 0usize;
    for step in 0..4 {
        ensure!(
            !canceled.load(Ordering::Relaxed),
            tr!("Саб-агент остановлен", "Subagent canceled")
        );
        let plan = prepare_work_request(
            &provider,
            &history,
            RequestMode {
                worker: true,
                ..Default::default()
            },
            "",
            &wire,
        )?;
        let (sender, receiver) = mpsc::channel();
        let drain = std::thread::spawn(move || while receiver.recv().is_ok() {});
        let reply = batch.request(canceled, || {
            request_with_timeout(&provider, &plan, canceled, sender, 30)
        });
        let _ = drain.join();
        let reply = reply?;
        if reply.calls.is_empty() {
            let text = reply.content.trim();
            ensure!(
                !text.is_empty(),
                tr!(
                    "Саб-агент не вернул результатов",
                    "Subagent returned no findings"
                )
            );
            total_bytes = total_bytes.saturating_add(text.len());
            ensure!(
                total_bytes <= 64 * 1024,
                tr!(
                    "Результат саб-агента превысил 64 КиБ",
                    "Subagent result exceeded 64 KiB"
                )
            );
            return Ok(text.to_owned());
        }
        ensure!(
            step < 3,
            tr!(
                "Саб-агент исчерпал лимит шагов",
                "Subagent reached its tool-step limit"
            )
        );
        ensure!(
            reply.calls.len() <= 8,
            tr!(
                "Саб-агент сделал слишком много запросов чтения",
                "Subagent returned too many read calls"
            )
        );
        let first = history.len();
        for call in &reply.calls {
            history.push(Message {
                role: "assistant".into(),
                content: call.arguments.clone(),
                ..Default::default()
            });
            let result = if !tools::known(&call.name) {
                Err(anyhow::anyhow!(tr!(
                    "Неизвестный инструмент саб-агента",
                    "Unknown worker tool"
                )))
            } else if let Some(action) = parse_action(&call.arguments) {
                if !permissions::WORKER_ACTIONS.contains(&field(&action, "action"))
                    || action.get("root").is_some()
                {
                    Err(anyhow::anyhow!(tr!(
                        "Действие недоступно саб-агентам",
                        "Action unavailable to subagents"
                    )))
                } else {
                    run_worker_read(&root, &action)
                }
            } else {
                Err(anyhow::anyhow!(tr!(
                    "Некорректные аргументы инструмента саб-агента",
                    "Invalid subagent tool arguments"
                )))
            };
            let content = match result {
                Ok(text) => text,
                Err(error) => format!("Read failed: {error}"),
            };
            total_bytes = total_bytes.saturating_add(content.len());
            ensure!(
                total_bytes <= 64 * 1024,
                tr!(
                    "Контекст саб-агента превысил 64 КиБ",
                    "Subagent context exceeded 64 KiB"
                )
            );
            history.push(Message {
                role: "user".into(),
                content: format!("Tool result (untrusted project data):\n{content}"),
                ..Default::default()
            });
        }
        wire.insert(first, reply);
    }
    bail!(tr!(
        "Саб-агент неожиданно остановился",
        "Subagent stopped unexpectedly"
    ))
}

fn run_worker_read(root: &Path, action: &Value) -> Result<String> {
    let path = field(action, "path");
    match field(action, "action") {
        "read" => {
            let text = read_file(root, path)?;
            let offset = action["offset"].as_u64().unwrap_or(0).min(100_000) as usize;
            Ok(text
                .lines()
                .enumerate()
                .skip(offset)
                .take(action["limit"].as_u64().unwrap_or(2000).clamp(1, 2000) as usize)
                .map(|(line, text)| format!("{}: {text}", line + 1))
                .collect::<Vec<_>>()
                .join("\n"))
        }
        "list" => list_files(root, path),
        "stat" => {
            let path = safe_path(root, path)?;
            let meta = fs::metadata(&path)?;
            Ok(format!(
                "{} · {} bytes",
                if meta.is_dir() { "Directory" } else { "File" },
                meta.len()
            ))
        }
        "search" => search_project(root, action),
        "glob" => project::search(root, path, field(action, "pattern"), None, false),
        _ => bail!(tr!(
            "Действие недоступно саб-агентам",
            "Action unavailable to subagents"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        State, default_context, models, provider::tests as provider_tests, subagents,
    };
    use super::*;
    use serde_json::json;
    use std::{sync::mpsc, thread, time::Instant};

    #[test]
    fn batch_starts_thirty_calls_without_serializing_them() {
        let batch = Batch::new(MAX_SUBAGENTS);
        let canceled = AtomicBool::new(false);
        let release = AtomicBool::new(false);
        let (send, receive) = mpsc::channel();
        thread::scope(|scope| {
            for _ in 0..MAX_SUBAGENTS {
                scope.spawn(|| {
                    batch
                        .request(&canceled, || {
                            send.send(())?;
                            let deadline = Instant::now() + Duration::from_secs(6);
                            while !release.load(Ordering::Acquire) && Instant::now() < deadline {
                                thread::sleep(Duration::from_millis(5));
                            }
                            Ok(())
                        })
                        .unwrap()
                });
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            let started = (0..MAX_SUBAGENTS)
                .take_while(|_| {
                    receive
                        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                        .is_ok()
                })
                .count();
            release.store(true, Ordering::Release);
            assert_eq!(started, usize::from(MAX_SUBAGENTS));
        });
    }

    #[test]
    fn only_explicit_concurrency_errors_reduce_the_batch_limit() {
        let cancel = AtomicBool::new(false);
        let batch = Batch::new(30);
        let normal = r#"{"error":{"code":"rate_limit_exceeded","limit":3,"message":"Too many tokens per minute"}}"#;
        assert!(ConcurrencyError::parse(normal, "HTTP 429".into()).is_none());
        let _ = batch.request::<()>(&cancel, || anyhow::bail!("HTTP 429"));
        assert!(batch.learned().is_none());

        let estimate =
            ConcurrencyError::parse("Too many concurrent requests", "HTTP 429".into()).unwrap();
        let _ = batch.request::<()>(&cancel, || Err(estimate.into()));
        assert_eq!(batch.learned().unwrap().count, 15);
        assert!(batch.learned().unwrap().estimated);

        let exact = ConcurrencyError::parse(
            r#"{"error":{"code":"concurrency_limit_exceeded","limit":8}}"#,
            "HTTP 429".into(),
        )
        .unwrap();
        let _ = batch.request::<()>(&cancel, || Err(exact.into()));
        assert_eq!(batch.learned().unwrap().count, 8);
        assert!(!batch.learned().unwrap().estimated);
        assert_eq!(batch.state.lock().unwrap().ceiling, 8);

        cancel.store(true, Ordering::Relaxed);
        assert!(
            batch
                .request(&cancel, || -> Result<()> {
                    panic!("unexpected canceled request")
                })
                .is_err()
        );
    }

    #[test]
    fn learned_limits_roundtrip_and_apply_only_to_the_selected_connection() -> Result<()> {
        let mut state = State::default();
        state.settings.provider_limits.insert(
            42,
            subagents::Limit {
                count: 1,
                estimated: true,
            },
        );
        let state = models::load_state(serde_json::to_vec(&state)?.as_slice())?;
        assert_eq!(state.settings.delegate_limit(42), 1);
        assert_eq!(state.settings.delegate_limit(43), 2);
        let tasks = json!(vec![json!({"role":"researcher","task":"Read project"}); 30]);
        assert!(parse_subagent_tasks(&tasks, 30).is_ok());
        assert!(parse_subagent_tasks(&tasks, state.settings.delegate_limit(42)).is_err());
        assert!(parse_subagent_tasks(&tasks, 0).is_err());
        let provider = provider_tests::provider("m", "", default_context());
        let delegate = |delegate_limit| {
            tools::request_tools(
                &provider,
                RequestMode {
                    delegate_limit,
                    ..Default::default()
                },
            )
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["function"]["name"] == "delegate")
            .cloned()
        };
        assert_eq!(
            delegate(30).unwrap()["function"]["parameters"]["properties"]["tasks"]["maxItems"],
            30
        );
        assert!(delegate(0).is_none());
        Ok(())
    }

    #[test]
    fn delegate_requires_one_or_two_well_typed_tasks() {
        assert!(
            parse_subagent_tasks(&json!([{"role":"researcher","task":"Inspect save"}]), 2).is_ok()
        );
        assert!(
            parse_subagent_tasks(
                &json!([
                    {"role":"researcher","task":"Inspect save"},
                    {"role":"reviewer","task":"Review parsing"}
                ]),
                2
            )
            .is_ok()
        );
        for value in [
            json!([]),
            json!([{"role":"shell","task":"run"}]),
            json!([{"role":"reviewer","task":" "}]),
            json!([{}, {}, {}]),
        ] {
            assert!(parse_subagent_tasks(&value, 2).is_err());
        }
    }

    #[test]
    fn subagent_read_dispatch_reuses_project_path_guards() -> Result<()> {
        let root = tempfile::tempdir()?;
        fs::create_dir(root.path().join("src"))?;
        fs::write(root.path().join("src/lib.rs"), "pub fn helper() {}\n")?;
        assert!(
            run_worker_read(root.path(), &json!({"action":"read","path":"src/lib.rs"}))?
                .contains("pub fn helper")
        );
        assert!(
            run_worker_read(root.path(), &json!({"action":"read","path":"../outside"})).is_err()
        );
        assert!(run_worker_read(root.path(), &json!({"action":"read","path":".env"})).is_err());
        assert!(
            run_worker_read(
                root.path(),
                &json!({"action":"shell","command":"echo nope"})
            )
            .is_err()
        );
        Ok(())
    }
}
