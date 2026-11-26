use super::{
    Message, Reasoning, Session, State, chat_error, models, permissions::DialogMode,
    provider::apply_reasoning, ui, web,
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub(super) fn edit_agent_settings(
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

pub(super) fn choose_reasoning(
    screen: &mut ui::Screen,
    state: &State,
) -> Result<Option<Reasoning>> {
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

pub(super) fn set_reasoning(state: &mut State, level: Reasoning) -> Result<()> {
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

pub(super) fn live_reasoning(screen: &mut ui::Screen, state: &mut State, level: Reasoning) -> bool {
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
