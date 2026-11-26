use anyhow::{Result, ensure};
use serde_json::Value;
use std::path::{Component, Path, PathBuf};

pub(super) const WORKER_ACTIONS: &[&str] = &["read", "list", "stat", "search", "glob"];

pub(super) const PLAN_ACTIONS: &[&str] = &[
    "read",
    "list",
    "search",
    "glob",
    "stat",
    "inspect_project",
    "ask_user",
];

pub(super) fn plan_allows_action(action: &Value) -> bool {
    action.get("root").is_none_or(Value::is_null)
        && action["action"]
            .as_str()
            .is_some_and(|name| PLAN_ACTIONS.contains(&name))
}

pub(super) fn plan_allows_command(command: &str) -> bool {
    !command.starts_with('/')
        || matches!(
            command,
            "/plan"
                | "/plan cancel"
                | "/code"
                | "/help"
                | "/header"
                | "/theme"
                | "/agents"
                | "/tools"
                | "/setting"
                | "/cd"
                | "/status"
                | "/continue"
                | "/quit"
                | "/exit"
                | "/model"
                | "/models"
                | "/provider"
                | "/providers"
                | "/reasoning"
                | "/new"
                | "/sessions"
                | "/temporary"
                | "/clear"
                | "/compact"
        )
        || command.starts_with("/reasoning ")
        || command.starts_with("/cd ")
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum DialogMode {
    #[default]
    Agent,
    Chat,
}

impl DialogMode {
    pub(super) const ALL: [Self; 2] = [Self::Agent, Self::Chat];

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Agent => tr!("Агент", "Agent"),
            Self::Chat => tr!("Только чат", "Chat only"),
        }
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum Mode {
    Ask,
    #[default]
    Auto,
    Full,
}

impl Mode {
    pub const ALL: [Self; 3] = [Self::Ask, Self::Auto, Self::Full];

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "ask" => Some(Self::Ask),
            "auto" => Some(Self::Auto),
            "full" => Some(Self::Full),
            _ => None,
        }
    }

    pub fn identifier(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Auto => "auto",
            Self::Full => "full",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Ask => tr!(
                "Подтверждай правки, команды и открытие страниц. Разрешение на рабочий стол запрашивается один раз на чат.",
                "Confirm edits, commands and page access. Desktop control asks for permission once per chat."
            ),
            Self::Auto => tr!(
                "Обратимые правки, команды в песочнице и действия на локальном предпросмотре выполняются автоматически. Доступ к рабочему столу подтверждается один раз на чат.",
                "Allow reversible edits, isolated commands and local preview input automatically. Approve desktop control once per chat."
            ),
            Self::Full => tr!(
                "Доступ к пользовательским файлам, сети и включённому управлению рабочим столом в этом чате. Запуск вне песочницы требует отдельного согласия.",
                "Allow user files, network and enabled desktop control in this chat. Commands outside isolation need separate approval."
            ),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Ask => tr!("Запрашивать разрешение", "Ask for approval"),
            Self::Auto => tr!("Подтверждать за меня", "Approve for me"),
            Self::Full => tr!("Полный доступ", "Full access"),
        }
    }
}

pub(super) fn protected(path: &Path) -> bool {
    path.components().any(|component| {
        let Component::Normal(name) = component else {
            return false;
        };
        let name = name.to_string_lossy().to_ascii_lowercase();
        matches!(
            name.as_str(),
            ".git"
                | ".ssh"
                | ".gnupg"
                | ".aws"
                | ".kube"
                | ".codex"
                | ".env"
                | ".agents"
                | ".config"
                | ".netrc"
                | ".npmrc"
                | ".pypirc"
                | "profile"
                | "auth.json"
                | "credentials.json"
                | "oc-state"
                | "appdata"
                | "system volume information"
                | "$recycle.bin"
        ) || name.starts_with(".env.")
            || [".pem", ".key", ".pfx", ".p12"]
                .iter()
                .any(|suffix| name.ends_with(suffix))
    })
}

pub(super) fn tool_root(mode: Mode, project: &Path, action: &Value) -> Result<PathBuf> {
    let Some(root) = action["root"].as_str().filter(|root| !root.is_empty()) else {
        return Ok(project.to_owned());
    };
    ensure!(
        mode == Mode::Full,
        "An external folder requires Full access in /permissions"
    );
    ensure!(
        matches!(
            action["action"].as_str(),
            Some(
                "read"
                    | "list"
                    | "stat"
                    | "glob"
                    | "search"
                    | "create_file"
                    | "apply_changes"
                    | "edit"
            )
        ),
        "External folders are available for file tools"
    );
    let path = Path::new(root);
    ensure!(
        path.is_absolute() && !protected(path),
        "Expected an ordinary user folder"
    );
    #[cfg(windows)]
    ensure!(
        matches!(path.components().next(), Some(Component::Prefix(prefix))
        if matches!(prefix.kind(), std::path::Prefix::Disk(_) | std::path::Prefix::VerbatimDisk(_))),
        "Expected a local volume"
    );
    for ancestor in path.ancestors().filter(|p| !p.as_os_str().is_empty()) {
        ensure!(
            !crate::safe_files::is_link(&std::fs::symlink_metadata(ancestor)?),
            "Linked folders are protected"
        );
    }
    let resolved = path.canonicalize()?;
    ensure!(
        resolved.is_dir() && resolved.parent().is_some() && !protected(&resolved),
        "Expected an ordinary user folder"
    );
    #[cfg(windows)]
    {
        use std::path::Prefix;
        ensure!(
            matches!(resolved.components().next(),
            Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))),
            "Expected a local volume"
        );
        let top = resolved
            .components()
            .filter_map(|component| match component {
                Component::Normal(name) => Some(name.to_string_lossy().to_ascii_lowercase()),
                _ => None,
            })
            .next();
        ensure!(
            top.is_some_and(|name| !matches!(
                name.as_str(),
                "windows"
                    | "program files"
                    | "program files (x86)"
                    | "programdata"
                    | "recovery"
                    | "boot"
            )),
            "System folders are protected"
        );
    }
    #[cfg(unix)]
    ensure!(
        ["/home", "/mnt", "/media", "/run/media", "/srv", "/opt"]
            .iter()
            .any(|base| resolved.starts_with(base)),
        "Expected a user or mounted folder"
    );
    Ok(resolved)
}
