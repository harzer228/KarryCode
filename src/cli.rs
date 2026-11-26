use anyhow::{Result, bail, ensure};
use std::{ffi::OsString, path::PathBuf};

pub const USAGE: &str = "Usage: karrycode [folder]
       karrycode [folder] -p <task>
       karrycode --version

Options:
  -p, --print <task>  Run one task without the interactive console, print the
                      final reply to stdout and progress to stderr. Use - to
                      read the task from stdin. Prompts that need approval are
                      declined.
  -h, --help          Show this help.
  -V, --version       Show the version.";

#[derive(Debug, PartialEq, Eq)]
pub enum Task {
    Text(String),
    Stdin,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Help,
    Version,
    Run {
        folder: Option<PathBuf>,
        print: Option<Task>,
    },
}

pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Command> {
    let args: Vec<OsString> = args.into_iter().collect();
    if let [only] = args.as_slice() {
        match only.to_str() {
            Some("--help" | "-h") => return Ok(Command::Help),
            Some("--version" | "-V") => return Ok(Command::Version),
            _ => {}
        }
    }
    let mut folder = None;
    let mut print = None;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("-p" | "--print") => {
                ensure!(
                    print.is_none(),
                    "-p/--print can only be given once. Run karrycode --help."
                );
                let Some(value) = args.next() else {
                    bail!("-p/--print needs a task. Run karrycode --help.");
                };
                let Some(value) = value.to_str() else {
                    bail!("The task must be valid UTF-8.");
                };
                print = Some(if value == "-" {
                    Task::Stdin
                } else {
                    Task::Text(value.to_owned())
                });
            }
            Some(flag) if flag.starts_with('-') && flag != "-" => {
                bail!("Unknown option: {flag}. Run karrycode --help.")
            }
            _ => {
                ensure!(
                    folder.is_none(),
                    "Only one project folder can be given. Run karrycode --help."
                );
                folder = Some(PathBuf::from(arg));
            }
        }
    }
    Ok(Command::Run { folder, print })
}

/// Normalizes a headless task and rejects input the one-shot runner cannot
/// serve: interactive slash commands open menus, so only `/skill` is allowed.
pub fn task_text(raw: &str) -> Result<String> {
    let text = raw.trim();
    ensure!(
        !text.is_empty(),
        "{}",
        tr!("Задача пуста.", "The task is empty.")
    );
    ensure!(
        text.contains('\n') || !text.starts_with('/') || text.starts_with("/skill "),
        "{}",
        tr!(
            "Команды с / недоступны в режиме -p, кроме /skill <имя> <задача>.",
            "Slash commands are unavailable with -p, except /skill <name> <task>."
        )
    );
    Ok(text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) -> Result<Command> {
        parse(args.iter().map(OsString::from))
    }

    #[test]
    fn help_and_version_are_unchanged() {
        assert_eq!(run(&["--help"]).unwrap(), Command::Help);
        assert_eq!(run(&["-h"]).unwrap(), Command::Help);
        assert_eq!(run(&["--version"]).unwrap(), Command::Version);
        assert_eq!(run(&["-V"]).unwrap(), Command::Version);
        assert!(run(&["--version", "extra"]).is_err());
        assert!(run(&["project", "--help"]).is_err());
        assert!(USAGE.starts_with("Usage: karrycode [folder]\n"));
    }

    #[test]
    fn folder_and_print_are_accepted_in_either_order() {
        let expected = Command::Run {
            folder: Some("project".into()),
            print: Some(Task::Text("fix the tests".into())),
        };
        assert_eq!(run(&["project", "-p", "fix the tests"]).unwrap(), expected);
        assert_eq!(run(&["-p", "fix the tests", "project"]).unwrap(), expected);
        assert_eq!(
            run(&["--print", "fix the tests", "project"]).unwrap(),
            expected
        );
        assert_eq!(
            run(&[]).unwrap(),
            Command::Run {
                folder: None,
                print: None
            }
        );
        assert_eq!(
            run(&["project"]).unwrap(),
            Command::Run {
                folder: Some("project".into()),
                print: None
            }
        );
    }

    #[test]
    fn a_dash_reads_the_task_from_stdin_and_values_may_look_like_flags() {
        assert_eq!(
            run(&["-p", "-"]).unwrap(),
            Command::Run {
                folder: None,
                print: Some(Task::Stdin)
            }
        );
        assert_eq!(
            run(&["-p", "--explain"]).unwrap(),
            Command::Run {
                folder: None,
                print: Some(Task::Text("--explain".into()))
            }
        );
    }

    #[test]
    fn bad_combinations_are_rejected() {
        assert!(run(&["-p"]).is_err());
        assert!(run(&["project", "--print"]).is_err());
        assert!(run(&["-p", "one", "-p", "two"]).is_err());
        assert!(run(&["one", "two"]).is_err());
        assert!(run(&["one", "-p", "task", "two"]).is_err());
        assert!(run(&["--unknown"]).is_err());
        assert!(run(&["-p", "task", "--help"]).is_err());
        assert!(run(&["-p", "task", "--version"]).is_err());
    }

    #[test]
    fn headless_tasks_reject_empty_text_and_interactive_commands() {
        assert_eq!(
            task_text("  explain main.rs \n").unwrap(),
            "explain main.rs"
        );
        assert!(task_text(" \n ").is_err());
        assert!(task_text("/model").is_err());
        assert!(task_text("/continue").is_err());
        assert_eq!(
            task_text("/skill review src/main.rs").unwrap(),
            "/skill review src/main.rs"
        );
        assert!(task_text("/plan\nthen do it").is_ok());
    }
}
