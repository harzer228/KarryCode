macro_rules! tr {
    ($ru:literal, $en:literal) => {
        if crate::language::english() { $en } else { $ru }
    };
}

macro_rules! trf {
    ($ru:literal, $en:literal $(, $arg:expr)* $(,)?) => {
        if crate::language::english() { format!($en $(, $arg)*) } else { format!($ru $(, $arg)*) }
    };
}

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
    io::{self, IsTerminal, Write},
    path::PathBuf,
    process::ExitCode,
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
    let mut args = env::args_os().skip(1);
    let first = args.next();
    ensure!(args.next().is_none(), "Usage: karrycode [folder]");
    if let Some(flag) = first.as_deref().and_then(|value| value.to_str()) {
        match flag {
            "--help" | "-h" => {
                println!(
                    "KarryCode CLI {}\nAI coding in your terminal.\n\nUsage: karrycode [folder]\n       karrycode --version",
                    env!("CARGO_PKG_VERSION")
                );
                return Ok(());
            }
            "--version" | "-V" => {
                println!("KarryCode CLI {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            flag if flag.starts_with('-') => {
                anyhow::bail!("Unknown option: {flag}. Run karrycode --help.")
            }
            _ => {}
        }
    }
    let root = platform::app_dir()?;
    language::load(&root);
    platform::require_standard_user()?;
    ensure!(
        io::stdin().is_terminal() && io::stdout().is_terminal(),
        tr!(
            "Открой KarryCode в интерактивном терминале.",
            "Open KarryCode in an interactive terminal."
        )
    );
    let mut workspace = first
        .map(PathBuf::from)
        .unwrap_or(env::current_dir()?)
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
    if !root.join("profile-id.json").exists()
        && let Err(error) = identity::init(&root)
        && !root.join("profile-id.json").exists()
    {
        return Err(error);
    }
    let mut profile = profile::Profile::open(&root)?;
    profile.recent.remember(&root, &workspace);
    let session = native::run(&mut profile, &root, &mut workspace);
    let saved = profile.save();
    let closed = profile.close();
    saved?;
    closed?;
    session
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
