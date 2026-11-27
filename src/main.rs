macro_rules! tr {
    ($ru:literal, $en:literal) => {
        crate::language::translate($ru, $en)
    };
}

macro_rules! trf {
    ($ru:literal, $en:literal $(, $arg:expr)* $(,)?) => {
        if crate::language::custom() {
            crate::language::format_translation($en, crate::language::translate($ru, $en), format!($en $(, $arg)*))
        } else if crate::language::english() {
            format!($en $(, $arg)*)
        } else {
            format!($ru $(, $arg)*)
        }
    };
}

mod cli;
mod folders;
mod identity;
mod language;
mod native;
mod platform;
mod profile;
mod recent;
mod safe_files;
mod vault;

use anyhow::{Context, Result, ensure};
use std::{
    env, fs,
    io::{self, IsTerminal, Read, Write},
    process::ExitCode,
};

pub(crate) const RELEASE_VERSION: &str =
    if matches!(env!("CARGO_PKG_VERSION_PATCH").as_bytes(), b"0") {
        if env!("CARGO_PKG_VERSION_PRE").is_empty() {
            concat!(
                env!("CARGO_PKG_VERSION_MAJOR"),
                ".",
                env!("CARGO_PKG_VERSION_MINOR")
            )
        } else {
            concat!(
                env!("CARGO_PKG_VERSION_MAJOR"),
                ".",
                env!("CARGO_PKG_VERSION_MINOR"),
                "-",
                env!("CARGO_PKG_VERSION_PRE")
            )
        }
    } else {
        env!("CARGO_PKG_VERSION")
    };

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("KarryCode CLI: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    if env::args_os().len() == 2
        && env::args_os()
            .nth(1)
            .is_some_and(|arg| arg == native::extensions::WORKER)
    {
        return native::extensions::worker();
    }
    let (folder, print, safe_ui) = match cli::parse(env::args_os().skip(1))? {
        cli::Command::Help => {
            println!(
                "KarryCode CLI {}\nAI coding in your terminal.\n\n{}",
                RELEASE_VERSION,
                cli::USAGE
            );
            return Ok(());
        }
        cli::Command::Version => {
            println!("KarryCode CLI {RELEASE_VERSION}");
            return Ok(());
        }
        cli::Command::Run {
            folder,
            print,
            safe_ui,
        } => (folder, print, safe_ui),
    };
    let root = platform::app_dir()?;
    language::load(&root);
    platform::require_standard_user()?;
    let task = match print {
        Some(cli::Task::Text(text)) => Some(cli::task_text(&text)?),
        Some(cli::Task::Stdin) => {
            let mut text = String::new();
            io::stdin().read_to_string(&mut text).context(tr!(
                "Не удалось прочитать задачу из stdin",
                "Could not read the task from stdin"
            ))?;
            Some(cli::task_text(&text)?)
        }
        None => {
            ensure!(
                io::stdin().is_terminal() && io::stdout().is_terminal(),
                tr!(
                    "Открой KarryCode в интерактивном терминале или передай задачу через -p.",
                    "Open KarryCode in an interactive terminal, or pass a task with -p."
                )
            );
            None
        }
    };
    let mut workspace = match folder {
        Some(folder) => folder,
        None => env::current_dir()?,
    }
    .canonicalize()
    .context(tr!(
        "Не удалось открыть папку проекта",
        "Could not open the project folder"
    ))?;
    ensure!(
        workspace.is_dir(),
        tr!("Укажи папку проекта.", "Specify a project folder.")
    );
    fs::create_dir_all(&root)?;
    ensure!(
        !safe_files::is_link(&fs::symlink_metadata(&root)?),
        tr!(
            "Каталог профиля является ссылкой.",
            "The profile directory is a link."
        )
    );
    platform::protect(&root)?;
    let root = root.canonicalize()?;
    ensure!(
        !workspace.starts_with(&root),
        tr!(
            "Выбери папку проекта вне профиля KarryCode.",
            "Choose a project folder outside the KarryCode profile."
        )
    );
    ensure!(
        task.is_none() || root.join("profile-id.json").exists(),
        "{}",
        setup_first()
    );
    if !root.join("profile-id.json").exists()
        && let Err(error) = identity::init(&root)
        && !root.join("profile-id.json").exists()
    {
        return Err(error);
    }
    let mut profile = profile::Profile::open(&root, task.is_none())?;
    if task.is_none() {
        profile.recent.remember(&root, &workspace);
    }
    let session = native::run(&mut profile, &root, &mut workspace, task, safe_ui);
    let saved = profile.save();
    let closed = profile.close();
    saved?;
    closed?;
    session
}

pub(crate) fn setup_first() -> &'static str {
    tr!(
        "Провайдер и модель не настроены. Сначала запусти karrycode интерактивно и выбери их.",
        "No provider and model are set up. Run karrycode interactively first to choose them."
    )
}

fn prompt(message: &str) -> Result<Option<String>> {
    print!("{message}");
    io::stdout().flush()?;
    let mut input = String::new();
    if io::stdin().read_line(&mut input)? == 0 {
        return Ok(None);
    }
    Ok(Some(input.trim().to_owned()))
}

pub fn ask(message: &str) -> Result<String> {
    prompt(message)?.context(tr!("Ввод закрыт", "Input closed"))
}
