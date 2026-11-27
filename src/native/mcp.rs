//! MCP servers over the stdio transport.
//!
//! KarryCode starts enabled user servers and waits for consent before starting
//! project servers. Existing
//! configurations from Claude Code, Codex and OpenCode can be imported from the
//! `/mcp` screen; only stdio servers are supported, which is what nearly all
//! local MCP servers use.

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

const MAX_RESULT: usize = 64 * 1024;
const MAX_STDERR: usize = 16 * 1024;
const DEFAULT_STARTUP: u64 = 20;
const DEFAULT_TOOL_TIMEOUT: u64 = 120;

#[derive(Clone, PartialEq, Eq)]
pub struct ServerConfig {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub enabled: bool,
    pub startup_timeout: u64,
    pub tool_timeout: u64,
    pub source: String,
    project_path: Option<PathBuf>,
}

#[derive(Clone)]
pub struct Candidate {
    pub source: String,
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub enabled: bool,
    pub startup_timeout: u64,
    pub tool_timeout: u64,
}

#[derive(Clone)]
pub struct ServerStatus {
    pub name: String,
    pub source: String,
    pub running: bool,
    pub tools: usize,
    pub error: Option<String>,
    pub pending: Option<usize>,
}

#[derive(Clone)]
pub struct ToolInfo {
    pub server: String,
    pub name: String,
    pub description: String,
    pub schema: Value,
}

struct Running {
    client: Client,
    tools: Vec<ToolInfo>,
}

/// Owns the configured servers for one agent session.
pub struct Hub {
    configs: Vec<ServerConfig>,
    approved: Vec<ServerConfig>,
    running: BTreeMap<String, Running>,
    statuses: Vec<ServerStatus>,
    errors: Vec<String>,
}

impl Hub {
    /// Loads the configuration and starts only eligible user servers.
    /// Project entries remain pending until explicitly approved.
    pub fn load(directory: &Path) -> Self {
        let mut hub = Self {
            configs: Vec::new(),
            approved: Vec::new(),
            running: BTreeMap::new(),
            statuses: Vec::new(),
            errors: Vec::new(),
        };
        hub.restart(directory);
        hub
    }

    pub fn pending(&self, index: usize) -> Option<&ServerConfig> {
        let config = self.configs.get(index)?;
        (config.enabled && config.project_path.is_some() && !self.approved.contains(config))
            .then_some(config)
    }

    pub fn approve(&mut self, directory: &Path, config: ServerConfig) {
        if config.enabled && config.project_path.is_some() {
            self.approved.push(config);
        }
        // Re-read and discard stale consent before starting anything.
        self.restart(directory);
    }

    fn start(&mut self, directory: &Path) {
        self.running.clear();
        self.statuses.clear();
        for (index, config) in self.configs.iter().enumerate() {
            let pending = self.pending(index).map(|_| index);
            if pending.is_some() || (!config.enabled && config.project_path.is_some()) {
                self.statuses.push(ServerStatus {
                    name: config.name.clone(),
                    source: config.source.clone(),
                    running: false,
                    tools: 0,
                    error: None,
                    pending,
                });
            }
        }
        for config in effective_configs(&self.configs, &self.approved).into_values() {
            if !config.enabled {
                self.statuses.push(ServerStatus {
                    name: config.name.clone(),
                    source: config.source.clone(),
                    running: false,
                    tools: 0,
                    error: None,
                    pending: None,
                });
                continue;
            }
            match Client::start(config, directory).and_then(|mut client| {
                let mut tools = client.list_tools()?;
                for tool in &mut tools {
                    tool.server = config.name.clone();
                }
                Ok((client, tools))
            }) {
                Ok((client, tools)) => {
                    let status = ServerStatus {
                        name: config.name.clone(),
                        source: config.source.clone(),
                        running: true,
                        tools: tools.len(),
                        error: None,
                        pending: None,
                    };
                    self.running
                        .insert(config.name.clone(), Running { client, tools });
                    self.statuses.push(status);
                }
                Err(error) => {
                    let message = error.to_string();
                    self.errors.push(format!("{}: {message}", config.name));
                    self.statuses.push(ServerStatus {
                        name: config.name.clone(),
                        source: config.source.clone(),
                        running: false,
                        tools: 0,
                        error: Some(message),
                        pending: None,
                    });
                }
            }
        }
    }

    pub fn restart(&mut self, directory: &Path) {
        let (configs, mut errors) = load_configs(directory);
        self.approved.retain(|config| configs.contains(config));
        self.configs = configs;
        self.errors = Vec::new();
        self.start(directory);
        errors.append(&mut self.errors);
        self.errors = errors;
    }

    pub fn statuses(&self) -> &[ServerStatus] {
        &self.statuses
    }

    pub fn errors(&self) -> &[String] {
        &self.errors
    }

    pub fn tools(&self) -> Vec<ToolInfo> {
        self.running
            .values()
            .flat_map(|running| running.tools.iter().cloned())
            .collect()
    }

    pub fn call(
        &mut self,
        server: &str,
        tool: &str,
        arguments: &Value,
    ) -> Result<(String, Option<super::provider::Visual>)> {
        let running = self.running.get_mut(server).with_context(|| {
            trf!(
                "MCP-сервер {} не запущен",
                "MCP server {} is not running",
                server
            )
        })?;
        ensure!(
            running.tools.iter().any(|known| known.name == tool),
            "{}",
            trf!(
                "У сервера нет инструмента {}",
                "The server has no tool {}",
                tool
            )
        );
        running.client.call_tool(tool, arguments)
    }

    pub fn summary(&self) -> String {
        let running = self.statuses.iter().filter(|status| status.running).count();
        let failed = self
            .statuses
            .iter()
            .filter(|status| status.error.is_some())
            .count();
        let tools: usize = self.statuses.iter().map(|status| status.tools).sum();
        let mut summary = format!(
            "MCP: {running}/{} · {}",
            self.statuses.len(),
            tools_phrase(tools)
        );
        if self.statuses.iter().any(|status| status.pending.is_some()) {
            summary.push_str(tr!(" · согласие в /mcp", " · approve in /mcp"));
        }
        if failed > 0 {
            summary.push_str(&trf!(" · ошибок: {failed}", " · errors: {failed}"));
        }
        summary
    }

    /// Servers found in the other CLIs that KarryCode does not know yet.
    pub fn candidates(&self) -> Vec<Candidate> {
        let configured: Vec<String> = self
            .configs
            .iter()
            .filter(|config| config.project_path.is_none())
            .map(|config| config.name.to_lowercase())
            .collect();
        let mut candidates: Vec<Candidate> = Vec::new();
        for candidate in import_candidates() {
            if configured.contains(&candidate.name.to_lowercase()) {
                continue;
            }
            if candidates
                .iter()
                .any(|existing| existing.name.eq_ignore_ascii_case(&candidate.name))
            {
                continue;
            }
            candidates.push(candidate);
        }
        candidates
    }

    /// Writes chosen servers to the user config and restarts the hub.
    pub fn import(&mut self, directory: &Path, selected: &[Candidate]) -> Result<String> {
        ensure!(
            !selected.is_empty(),
            "{}",
            tr!("Ничего не выбрано", "Nothing selected")
        );
        let path = user_config_path()?;
        let mut document =
            read_json_document(&path)?.unwrap_or_else(|| json!({"version": 1, "servers": {}}));
        if document.get("servers").is_none() {
            document["servers"] = json!({});
        }
        let mut added = 0usize;
        for candidate in selected {
            let servers = document["servers"]
                .as_object_mut()
                .context(tr!("Некорректный mcp.json", "Invalid mcp.json"))?;
            if servers.contains_key(&candidate.name) {
                continue;
            }
            servers.insert(
                candidate.name.clone(),
                json!({
                    "command": candidate.command,
                    "args": candidate.args,
                    "env": candidate.env,
                    "enabled": candidate.enabled,
                    "startup_timeout_sec": candidate.startup_timeout,
                    "tool_timeout_sec": candidate.tool_timeout,
                }),
            );
            added += 1;
        }
        if added > 0 {
            write_private_json(&path, &document)?;
        }
        self.restart(directory);
        Ok(trf!(
            "Импортировано серверов: {}. Файл: {}",
            "Imported servers: {}. File: {}",
            added,
            path.display()
        ))
    }
}

impl ServerConfig {
    pub fn approval_preview(&self) -> String {
        let mut hide_next = false;
        let args: Vec<_> = self
            .args
            .iter()
            .map(|arg| {
                let flag = arg.trim_start_matches('-');
                let secret_option = arg.starts_with('-')
                    && is_secret(flag.split_once('=').map_or(flag, |(key, _)| key));
                let hidden = std::mem::replace(&mut hide_next, secret_option && !arg.contains('='));
                if hidden || arg.contains("://") || secret_option && arg.contains('=') {
                    tr!("<скрыто>", "<hidden>")
                } else {
                    arg.as_str()
                }
            })
            .collect();
        let path = self
            .project_path
            .as_deref()
            .map_or_else(String::new, crate::folders::display);
        let command = json!(self.command);
        let args = json!(args);
        let env = json!(self.env.keys().collect::<Vec<_>>());
        trf!(
            "Процесс запустится с твоими правами на текущий сеанс.\nФайл: {path}\nКоманда: {command}\nАргументы: {args}\nПеременные окружения (только имена): {env}",
            "The process will run with your permissions for this session.\nFile: {path}\nCommand: {command}\nArguments: {args}\nEnvironment variables (names only): {env}"
        )
    }
}

/// Calls signed by the hub and used by the agent loop.
pub fn describe_arguments(arguments: &Value) -> String {
    let mut value = arguments.clone();
    redact(&mut value);
    serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".into())
}

fn redact(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                if is_secret(key) {
                    *child = json!("<скрыто>");
                } else {
                    redact(child);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact),
        _ => {}
    }
}

fn tools_phrase(count: usize) -> String {
    if crate::language::english() {
        return format!("{count} tools");
    }
    let word = match count % 100 {
        11..=14 => "инструментов",
        _ => match count % 10 {
            1 => "инструмент",
            2..=4 => "инструмента",
            _ => "инструментов",
        },
    };
    format!("{count} {word}")
}

fn is_secret(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    [
        "key",
        "token",
        "secret",
        "password",
        "credential",
        "authorization",
    ]
    .iter()
    .any(|needle| key.contains(needle))
}

fn effective_configs<'a>(
    configs: &'a [ServerConfig],
    approved: &[ServerConfig],
) -> BTreeMap<&'a str, &'a ServerConfig> {
    let mut selected = BTreeMap::new();
    for config in configs {
        if config.project_path.is_none() || (config.enabled && approved.contains(config)) {
            selected.insert(config.name.as_str(), config);
        }
    }
    selected
}

fn load_configs(directory: &Path) -> (Vec<ServerConfig>, Vec<String>) {
    let mut configs = Vec::new();
    let mut errors = Vec::new();
    let mut paths = Vec::new();
    if let Ok(home) = user_config_path() {
        paths.push((home, tr!("пользователь", "user"), false));
    }
    let directory = directory
        .canonicalize()
        .unwrap_or_else(|_| directory.to_owned());
    for relative in [".mcp.json", ".karrycode/mcp.json"] {
        paths.push((directory.join(relative), relative, true));
    }
    for (path, source, project) in paths {
        if !path.is_file() {
            continue;
        }
        match read_json_document(&path) {
            Ok(Some(document)) => match parse_config_document(&document, source) {
                Ok(parsed) => {
                    for mut config in parsed {
                        if project {
                            config.project_path = Some(path.clone());
                        }
                        configs.push(config);
                    }
                }
                Err(error) => errors.push(format!("{}: {error}", path.display())),
            },
            Ok(None) => {}
            Err(error) => errors.push(format!("{}: {error}", path.display())),
        }
    }
    (configs, errors)
}

/// Whether at least one MCP config file exists, so the UI can show a waiting
/// line before starting servers.
pub fn configured() -> bool {
    user_config_path().is_ok_and(|path| path.is_file())
}

/// Resolves the program the same way a shell would on Windows. `npx` and
/// `uvx` are `.cmd` shims there, and `CreateProcess` cannot run them directly.
fn resolve_program(command: &str) -> (std::ffi::OsString, Vec<std::ffi::OsString>) {
    let direct = || (std::ffi::OsString::from(command), Vec::new());
    #[cfg(windows)]
    {
        let has_separator = command.contains(['\\', '/']);
        if !has_separator {
            let path = std::env::var_os("PATH").unwrap_or_default();
            let extensions = [".exe", ".cmd", ".bat", ".com"];
            for extension in extensions {
                let name = format!("{command}{extension}");
                for folder in std::env::split_paths(&path) {
                    let candidate = folder.join(&name);
                    if !candidate.is_file() {
                        continue;
                    }
                    if extension == ".cmd" || extension == ".bat" {
                        return (
                            std::ffi::OsString::from("cmd.exe"),
                            vec![
                                std::ffi::OsString::from("/D"),
                                std::ffi::OsString::from("/C"),
                                candidate.into_os_string(),
                            ],
                        );
                    }
                    return (candidate.into_os_string(), Vec::new());
                }
            }
        } else if command.ends_with(".cmd") || command.ends_with(".bat") {
            return (
                std::ffi::OsString::from("cmd.exe"),
                vec![
                    std::ffi::OsString::from("/D"),
                    std::ffi::OsString::from("/C"),
                    std::ffi::OsString::from(command),
                ],
            );
        }
    }
    direct()
}

fn parse_config_document(document: &Value, source: &str) -> Result<Vec<ServerConfig>> {
    let map = document
        .get("servers")
        .or_else(|| document.get("mcpServers"))
        .and_then(Value::as_object)
        .context(tr!(
            "Нужен объект servers или mcpServers",
            "Expected a servers or mcpServers object"
        ))?;
    let mut configs = Vec::new();
    for (name, item) in map {
        if item.get("url").is_some() || item.get("type").and_then(Value::as_str) == Some("remote") {
            continue;
        }
        let Some(command) = item.get("command").and_then(Value::as_str).map(str::trim) else {
            continue;
        };
        if command.is_empty() {
            continue;
        }
        configs.push(ServerConfig {
            name: name.clone(),
            command: command.to_owned(),
            args: string_array(&item["args"]),
            env: string_map(item.get("env").or_else(|| item.get("environment"))),
            enabled: item.get("enabled").and_then(Value::as_bool).unwrap_or(true),
            startup_timeout: seconds(item.get("startup_timeout_sec"), DEFAULT_STARTUP, 1, 120),
            tool_timeout: seconds(item.get("tool_timeout_sec"), DEFAULT_TOOL_TIMEOUT, 1, 600),
            source: source.to_owned(),
            project_path: None,
        });
    }
    Ok(configs)
}

fn string_array(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn string_map(value: Option<&Value>) -> BTreeMap<String, String> {
    value
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .map(|(key, value)| {
                    let value = match value {
                        Value::String(text) => text.clone(),
                        other => other.to_string(),
                    };
                    (key.clone(), value)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn seconds(value: Option<&Value>, default: u64, min: u64, max: u64) -> u64 {
    value
        .and_then(Value::as_f64)
        .map(|number| number.round() as u64)
        .unwrap_or(default)
        .clamp(min, max)
}

pub fn user_config_path() -> Result<PathBuf> {
    Ok(crate::platform::home().join(".karrycode/mcp.json"))
}

fn read_json_document(path: &Path) -> Result<Option<Value>> {
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(path)?;
    ensure!(
        bytes.len() <= 1024 * 1024,
        "{}",
        tr!("mcp.json больше 1 МиБ", "mcp.json is larger than 1 MiB")
    );
    let text =
        String::from_utf8(bytes).context(tr!("mcp.json не в UTF-8", "mcp.json is not UTF-8"))?;
    Ok(Some(serde_json::from_str(&text)?))
}

fn write_private_json(path: &Path, document: &Value) -> Result<()> {
    let parent = path.parent().context(tr!("Нет каталога", "No folder"))?;
    fs::create_dir_all(parent)?;
    let mut draft = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(draft.as_file_mut(), document)?;
    draft.as_file_mut().write_all(b"\n")?;
    draft.as_file().sync_all()?;
    draft.persist(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

// ───────────────────────── import from other CLIs ─────────────────────────

fn import_candidates() -> Vec<Candidate> {
    let mut candidates = Vec::new();
    if let Some(candidate) = claude_candidates() {
        candidates.extend(candidate);
    }
    candidates.extend(codex_candidates());
    candidates.extend(opencode_candidates());
    candidates
}

fn claude_candidates() -> Option<Vec<Candidate>> {
    let home = crate::platform::home();
    let path = home.join(".claude.json");
    let document = read_json_document(&path).ok()??;
    let map = document.get("mcpServers")?.as_object()?;
    Some(
        map.iter()
            .filter_map(|(name, item)| candidate("Claude Code", name, item))
            .collect(),
    )
}

fn candidate(source: &str, name: &str, item: &Value) -> Option<Candidate> {
    if item.get("url").is_some() {
        return None;
    }
    let command = item.get("command")?.as_str()?.trim();
    if command.is_empty() {
        return None;
    }
    Some(Candidate {
        source: source.to_owned(),
        name: name.to_owned(),
        command: command.to_owned(),
        args: string_array(&item["args"]),
        env: string_map(item.get("env").or_else(|| item.get("environment"))),
        enabled: item.get("enabled").and_then(Value::as_bool).unwrap_or(true),
        startup_timeout: DEFAULT_STARTUP,
        tool_timeout: DEFAULT_TOOL_TIMEOUT,
    })
}

fn codex_candidates() -> Vec<Candidate> {
    let path = crate::platform::home().join(".codex/config.toml");
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    parse_codex_mcp(&text)
        .into_iter()
        .map(|mut candidate| {
            candidate.source = "Codex".to_owned();
            candidate
        })
        .collect()
}

fn opencode_candidates() -> Vec<Candidate> {
    let path = crate::platform::home().join(".config/opencode/opencode.jsonc");
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let text = strip_jsonc(&text);
    let Ok(document) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    let Some(map) = document.get("mcp").and_then(Value::as_object) else {
        return Vec::new();
    };
    map.iter()
        .filter_map(|(name, item)| {
            if item.get("type").and_then(Value::as_str) == Some("remote")
                || item.get("url").is_some()
            {
                return None;
            }
            let mut command = item.get("command")?.as_array()?.iter();
            let program = command.next()?.as_str()?.trim();
            if program.is_empty() {
                return None;
            }
            let args = command
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect();
            Some(Candidate {
                source: "OpenCode".to_owned(),
                name: name.clone(),
                command: program.to_owned(),
                args,
                env: string_map(item.get("environment").or_else(|| item.get("env"))),
                enabled: item.get("enabled").and_then(Value::as_bool).unwrap_or(true),
                startup_timeout: DEFAULT_STARTUP,
                tool_timeout: DEFAULT_TOOL_TIMEOUT,
            })
        })
        .collect()
}

/// A small TOML subset for Codex `[mcp_servers.*]` tables. It is only used for
/// import; malformed lines are skipped instead of failing the whole file.
fn parse_codex_mcp(text: &str) -> Vec<Candidate> {
    let mut entries: BTreeMap<String, Candidate> = BTreeMap::new();
    let mut section: Vec<String> = Vec::new();
    for raw in text.lines() {
        let stripped = strip_toml_comment(raw);
        let line = stripped.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1]
                .split('.')
                .map(|part| unquote(part.trim()).to_owned())
                .collect();
            continue;
        }
        if section.len() < 2 || section[0] != "mcp_servers" {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = unquote(key.trim()).to_owned();
        let name = section[1].clone();
        let entry = entries.entry(name.clone()).or_insert_with(|| Candidate {
            source: "Codex".to_owned(),
            name,
            command: String::new(),
            args: Vec::new(),
            env: BTreeMap::new(),
            enabled: true,
            startup_timeout: DEFAULT_STARTUP,
            tool_timeout: DEFAULT_TOOL_TIMEOUT,
        });
        if section.len() >= 3 && section[2] == "env" {
            entry.env.insert(key, scalar(value));
            continue;
        }
        if section.len() != 2 {
            continue;
        }
        match key.as_str() {
            "command" => entry.command = scalar(value),
            "args" => entry.args = string_array_text(value),
            "enabled" => entry.enabled = scalar(value).eq_ignore_ascii_case("true"),
            "startup_timeout_sec" => entry.startup_timeout = seconds_text(value).min(120),
            "tool_timeout_sec" => entry.tool_timeout = seconds_text(value),
            _ => {}
        }
    }
    entries
        .into_values()
        .filter(|candidate| !candidate.command.trim().is_empty())
        .collect()
}

fn strip_toml_comment(line: &str) -> String {
    let mut result = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for character in line.chars() {
        if escaped {
            result.push(character);
            escaped = false;
            continue;
        }
        if character == '\\' && quote == Some('"') {
            result.push(character);
            escaped = true;
            continue;
        }
        if let Some(active) = quote {
            result.push(character);
            if character == active {
                quote = None;
            }
            continue;
        }
        if character == '"' || character == '\'' {
            quote = Some(character);
            result.push(character);
            continue;
        }
        if character == '#' {
            break;
        }
        result.push(character);
    }
    result
}

fn seconds_text(value: &str) -> u64 {
    value
        .trim()
        .parse::<f64>()
        .map(|number| (number.round() as u64).clamp(1, 600))
        .unwrap_or(DEFAULT_STARTUP)
}

fn scalar(value: &str) -> String {
    let value = value.trim();
    let double = value.starts_with('"') && value.ends_with('"');
    let inner = unquote(value);
    if double {
        let mut result = String::new();
        let mut escaped = false;
        for character in inner.chars() {
            if escaped {
                result.push(match character {
                    'n' => '\n',
                    't' => '\t',
                    'r' => '\r',
                    other => other,
                });
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else {
                result.push(character);
            }
        }
        result
    } else {
        inner.to_owned()
    }
}

fn unquote(value: &str) -> &str {
    let value = value.trim();
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        if (bytes[0] == b'"' && bytes[value.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[value.len() - 1] == b'\'')
        {
            return &value[1..value.len() - 1];
        }
    }
    value
}

fn string_array_text(value: &str) -> Vec<String> {
    let value = value.trim();
    let Some(inner) = value
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
    else {
        return Vec::new();
    };
    let mut result = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for character in inner.chars() {
        if escaped {
            current.push(match character {
                'n' => '\n',
                't' => '\t',
                'r' => '\r',
                other => other,
            });
            escaped = false;
            continue;
        }
        if let Some(active) = quote {
            if active == '"' && character == '\\' {
                escaped = true;
                continue;
            }
            if character == active {
                quote = None;
            } else {
                current.push(character);
            }
            continue;
        }
        match character {
            '"' | '\'' => quote = Some(character),
            ',' => {
                let item = current.trim().to_owned();
                if !item.is_empty() {
                    result.push(item);
                }
                current.clear();
            }
            other => current.push(other),
        }
    }
    let item = current.trim().to_owned();
    if !item.is_empty() {
        result.push(item);
    }
    result
}

/// Strips `//` and `/* */` comments while keeping string contents intact.
fn strip_jsonc(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;
    while let Some(character) = characters.next() {
        if in_string {
            result.push(character);
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        match character {
            '"' => {
                in_string = true;
                result.push(character);
            }
            '/' if characters.peek() == Some(&'/') => {
                for next in characters.by_ref() {
                    if next == '\n' {
                        result.push('\n');
                        break;
                    }
                }
            }
            '/' if characters.peek() == Some(&'*') => {
                characters.next();
                let mut previous = '\0';
                for next in characters.by_ref() {
                    if previous == '*' && next == '/' {
                        break;
                    }
                    if next == '\n' {
                        result.push('\n');
                    }
                    previous = next;
                }
            }
            _ => result.push(character),
        }
    }
    result
}

// ───────────────────────────── stdio client ─────────────────────────────

struct Client {
    _process: ProcessTree,
    stdin: ChildStdin,
    messages: mpsc::Receiver<Value>,
    next_id: i64,
    startup: u64,
    tool_timeout: u64,
    stderr: Arc<Mutex<String>>,
}

impl Client {
    fn start(config: &ServerConfig, directory: &Path) -> Result<Self> {
        let (program, prefix) = resolve_program(&config.command);
        let mut command = Command::new(program);
        command
            .args(prefix)
            .args(&config.args)
            .current_dir(directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in std::env::vars_os() {
            let name = key.to_string_lossy();
            if is_secret(&name) || name == "KARRYCODE_PROFILE" {
                command.env_remove(key);
                continue;
            }
            command.env(key, value);
        }
        for (key, value) in &config.env {
            command.env(key, value);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let mut process = ProcessTree::spawn(command)?;
        let stdin = process.child_mut().stdin.take().context(tr!(
            "Входной поток MCP недоступен",
            "MCP stdin is unavailable"
        ))?;
        let stdout = process.child_mut().stdout.take().context(tr!(
            "Выходной поток MCP недоступен",
            "MCP stdout is unavailable"
        ))?;
        let stderr_pipe = process.child_mut().stderr.take().context(tr!(
            "Поток ошибок MCP недоступен",
            "MCP stderr is unavailable"
        ))?;
        let (sender, messages) = mpsc::channel();
        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if let Ok(value) = serde_json::from_str::<Value>(line)
                    && sender.send(value).is_err()
                {
                    break;
                }
            }
        });
        let stderr = Arc::new(Mutex::new(String::new()));
        let sink = stderr.clone();
        thread::spawn(move || {
            let mut reader = BufReader::new(stderr_pipe);
            let mut buffer = [0u8; 4096];
            while let Ok(read) = reader.read(&mut buffer) {
                if read == 0 {
                    break;
                }
                let text = String::from_utf8_lossy(&buffer[..read]);
                if let Ok(mut stored) = sink.lock() {
                    stored.push_str(&text);
                    if stored.len() > MAX_STDERR {
                        let keep = stored.len() - MAX_STDERR;
                        stored.drain(..keep);
                    }
                }
            }
        });
        let mut client = Self {
            _process: process,
            stdin,
            messages,
            next_id: 0,
            startup: config.startup_timeout,
            tool_timeout: config.tool_timeout,
            stderr,
        };
        client.request(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "karrycode", "version": env!("CARGO_PKG_VERSION")},
            }),
            client.startup,
        )?;
        client.notify("notifications/initialized", json!({}))?;
        Ok(client)
    }

    fn send(&mut self, value: &Value) -> Result<()> {
        let mut line = serde_json::to_string(value)?;
        line.push('\n');
        self.stdin.write_all(line.as_bytes())?;
        self.stdin.flush()?;
        Ok(())
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
    }

    fn request(&mut self, method: &str, params: Value, timeout: u64) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        let deadline = Instant::now() + Duration::from_secs(timeout.max(1));
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!(
                    "{}",
                    trf!(
                        "MCP не ответил на {} за {} с",
                        "MCP did not answer {} in {} s",
                        method,
                        timeout
                    )
                );
            }
            match self.messages.recv_timeout(remaining) {
                Ok(message) => {
                    if message.get("method").is_some() && message.get("id").is_some() {
                        let _ = self.send(&json!({
                            "jsonrpc": "2.0",
                            "id": message["id"],
                            "error": {"code": -32601, "message": "Method not found"}
                        }));
                        continue;
                    }
                    if message.get("id").and_then(Value::as_i64) != Some(id) {
                        continue;
                    }
                    if let Some(error) = message.get("error") {
                        let text = error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown");
                        bail!("{}", trf!("MCP: {}", "MCP: {}", text));
                    }
                    return Ok(message.get("result").cloned().unwrap_or(Value::Null));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    bail!(
                        "{}",
                        trf!(
                            "MCP не ответил на {} за {} с",
                            "MCP did not answer {} in {} s",
                            method,
                            timeout
                        )
                    );
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let log = self.stderr();
                    bail!(
                        "{}",
                        trf!(
                            "MCP-сервер остановился. {}",
                            "MCP server stopped. {}",
                            log.trim()
                        )
                    );
                }
            }
        }
    }

    fn stderr(&self) -> String {
        self.stderr
            .lock()
            .map(|text| text.clone())
            .unwrap_or_default()
    }

    fn list_tools(&mut self) -> Result<Vec<ToolInfo>> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..10 {
            let mut params = json!({});
            if let Some(cursor) = &cursor {
                params["cursor"] = json!(cursor);
            }
            let result = self.request("tools/list", params, self.startup)?;
            for item in result["tools"].as_array().cloned().unwrap_or_default() {
                let Some(name) = item["name"].as_str() else {
                    continue;
                };
                tools.push(ToolInfo {
                    server: String::new(),
                    name: name.to_owned(),
                    description: item["description"].as_str().unwrap_or("").to_owned(),
                    schema: item["inputSchema"].clone(),
                });
            }
            cursor = result["nextCursor"].as_str().map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        Ok(tools)
    }

    fn call_tool(
        &mut self,
        name: &str,
        arguments: &Value,
    ) -> Result<(String, Option<super::provider::Visual>)> {
        let result = self.request(
            "tools/call",
            json!({"name": name, "arguments": arguments}),
            self.tool_timeout,
        )?;
        let is_error = result["isError"].as_bool().unwrap_or(false);
        let mut text = content_text(&result);
        if text.len() > MAX_RESULT {
            let boundary = (0..=MAX_RESULT)
                .rev()
                .find(|&index| text.is_char_boundary(index))
                .unwrap_or(0);
            text.truncate(boundary);
            text.push_str("\n…MCP result truncated at 64 KiB");
        }
        if is_error {
            return Ok((format!("MCP tool reported an error:\n{text}"), None));
        }
        let image = result["content"]
            .as_array()
            .into_iter()
            .flatten()
            .find_map(|item| {
                if item["type"] != "image" {
                    return None;
                }
                let mime = item["mimeType"].as_str()?;
                let data = item["data"].as_str()?;
                (matches!(mime, "image/png" | "image/jpeg" | "image/webp")
                    && data.len() <= 512 * 1024
                    && {
                        use base64::Engine as _;
                        base64::engine::general_purpose::STANDARD
                            .decode(data)
                            .is_ok()
                    })
                .then(|| super::provider::Visual {
                    mime: mime.into(),
                    data: data.into(),
                })
            });
        if image.is_some() {
            text = text.replace("[image content omitted]", "[image available]");
        }
        Ok((text, image))
    }
}

fn content_text(result: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(items) = result.get("content").and_then(Value::as_array) {
        for item in items {
            match item.get("type").and_then(Value::as_str) {
                Some("text") => parts.push(item["text"].as_str().unwrap_or("").to_owned()),
                Some(other) => parts.push(format!("[{other} content omitted]")),
                None => {}
            }
        }
    }
    if parts.is_empty() {
        serde_json::to_string_pretty(result).unwrap_or_default()
    } else {
        parts.join("\n")
    }
}

struct ProcessTree(
    Child,
    #[cfg(windows)] windows_sys::Win32::Foundation::HANDLE,
);

impl ProcessTree {
    fn spawn(mut command: Command) -> Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let child = command.spawn()?;
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::{Foundation::CloseHandle, System::JobObjects::*};
            let child = child;
            let mut job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let attached = !job.is_null()
                && unsafe {
                    SetInformationJobObject(
                        job,
                        JobObjectExtendedLimitInformation,
                        &limits as *const _ as _,
                        std::mem::size_of_val(&limits) as u32,
                    )
                } != 0
                && unsafe { AssignProcessToJobObject(job, child.as_raw_handle() as _) } != 0;
            if !attached {
                // A parent job can forbid nesting; the child still works and is
                // killed directly on drop, so the session must not fail here.
                if !job.is_null() {
                    unsafe { CloseHandle(job) };
                }
                job = std::ptr::null_mut();
            }
            Ok(Self(child, job))
        }
        #[cfg(unix)]
        Ok(Self(child))
    }

    fn child_mut(&mut self) -> &mut Child {
        &mut self.0
    }
}

impl Drop for ProcessTree {
    fn drop(&mut self) {
        #[cfg(windows)]
        if !self.1.is_null() {
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(self.1);
            }
        }
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.0.id() as i32), libc::SIGKILL);
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_servers_only_override_after_exact_session_approval() {
        let mut configs = parse_config_document(
            &json!({"servers": {"demo": {"command": "user-server"}}}),
            "user",
        )
        .unwrap();
        let mut project = configs[0].clone();
        project.project_path = Some(PathBuf::from("project/.mcp.json"));
        project.command = "project-server".into();
        configs.push(project.clone());
        assert_eq!(
            effective_configs(&configs, &[])["demo"].command,
            "user-server"
        );
        let approvals = [project.clone()];
        assert_eq!(
            effective_configs(&configs, &approvals)["demo"].command,
            "project-server"
        );
        configs[1].env.insert("TOKEN".into(), "synthetic".into());
        assert_eq!(
            effective_configs(&configs, &approvals)["demo"].command,
            "user-server"
        );
        configs[1] = project;
        configs[1].project_path = Some(PathBuf::from("other/.mcp.json"));
        assert_eq!(
            effective_configs(&configs, &approvals)["demo"].command,
            "user-server"
        );
    }

    #[test]
    fn hub_does_not_attempt_to_start_a_project_server_without_approval() -> Result<()> {
        let project = tempfile::tempdir()?;
        let path = project.path().join(".mcp.json");
        fs::write(
            &path,
            json!({"mcpServers": {"karrycode-test-project-only": {
                "command": project.path().join("nonexistent-test-server.exe")
            }}})
            .to_string(),
        )?;
        let (configs, _) = load_configs(project.path());
        let config = configs
            .into_iter()
            .find(|config| config.project_path.is_some())
            .unwrap();
        let mut hub = Hub {
            configs: vec![config.clone()],
            approved: Vec::new(),
            running: BTreeMap::new(),
            statuses: Vec::new(),
            errors: Vec::new(),
        };
        hub.start(project.path());
        assert_eq!(hub.statuses[0].pending, Some(0));
        assert!(hub.errors.is_empty());
        hub.approved.push(config);
        hub.start(project.path());
        assert_eq!(hub.statuses[0].pending, None);
        assert!(hub.statuses[0].error.is_some());
        Ok(())
    }

    #[test]
    fn project_approval_preview_hides_secret_arguments_and_environment_values() {
        let document = json!({"servers": {"demo": {
            "command": "node",
            "args": ["server.js", "--token", "hidden-one", "--password=hidden-two"],
            "env": {"TOKEN": "hidden-three"}
        }}});
        let preview = parse_config_document(&document, "project").unwrap()[0].approval_preview();
        assert!(preview.contains("node") && preview.contains("server.js"));
        assert!(preview.contains("TOKEN"));
        assert!(!preview.contains("hidden-one"));
        assert!(!preview.contains("hidden-two"));
        assert!(!preview.contains("hidden-three"));
    }

    #[test]
    fn codex_toml_extracts_servers_and_env() {
        let text = r#"
model = "gpt"
[mcp_servers.rea]
command = 'C:\Program Files\nodejs\node.exe'
args = ['rea.mjs', "mcp"]
startup_timeout_sec = 30.0
tool_timeout_sec = 450.0
[mcp_servers.rea.env]
JAVA_HOME = 'C:\Java'
TOKENISH = "x # not a comment"
[mcp_servers.disabled]
command = "x"
enabled = false
"#;
        let servers = parse_codex_mcp(text);
        assert_eq!(servers.len(), 2);
        let rea = servers
            .iter()
            .find(|server| server.name == "rea")
            .expect("rea");
        assert_eq!(rea.args, vec!["rea.mjs", "mcp"]);
        assert_eq!(
            rea.env.get("JAVA_HOME").map(String::as_str),
            Some(r"C:\Java")
        );
        assert_eq!(
            rea.env.get("TOKENISH").map(String::as_str),
            Some("x # not a comment")
        );
        assert!(rea.enabled);
        assert!(
            !servers
                .iter()
                .find(|s| s.name == "disabled")
                .expect("disabled")
                .enabled
        );
    }

    #[test]
    fn single_quoted_toml_paths_keep_backslashes() {
        let text = r#"
[mcp_servers.paths]
command = "x"
args = ['C:\Program Files\a b\x.exe', "two"]
"#;
        let servers = parse_codex_mcp(text);
        assert_eq!(servers.len(), 1);
        assert_eq!(
            servers[0].args,
            vec![r"C:\Program Files\a b\x.exe".to_owned(), "two".to_owned()]
        );
    }

    #[test]
    fn jsonc_comments_do_not_eat_urls_or_strings() {
        let text = r#"{
  // comment
  "url": "https://example.org/v1", /* block */
  "command": ["node", "C:\\mcp\\rea.mjs"],
  "enabled": true
}"#;
        let stripped = strip_jsonc(text);
        let value: Value = serde_json::from_str(&stripped).expect("json");
        assert_eq!(value["url"], "https://example.org/v1");
        assert_eq!(value["command"][1], r"C:\mcp\rea.mjs");
    }

    #[test]
    fn config_document_accepts_codex_style_fields() {
        let document = json!({"servers": {
            "demo": {
                "command": "node",
                "args": ["server.js"],
                "env": {"A": "1", "B": 2},
                "startup_timeout_sec": 500,
                "tool_timeout_sec": -1
            }
        }});
        let configs = parse_config_document(&document, "test").expect("configs");
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].startup_timeout, 120);
        assert_eq!(configs[0].tool_timeout, 1);
        assert_eq!(configs[0].env.get("B").map(String::as_str), Some("2"));
        assert!(configs[0].enabled);
    }

    #[test]
    fn tool_content_is_joined_and_binary_marks_are_explicit() {
        let result = json!({"content": [
            {"type": "text", "text": "line one"},
            {"type": "image", "data": "..."},
            {"type": "text", "text": "line two"}
        ]});
        assert_eq!(
            content_text(&result),
            "line one\n[image content omitted]\nline two"
        );
        let empty = json!({"ok": true});
        assert!(content_text(&empty).contains("\"ok\""));
    }

    #[test]
    fn secret_arguments_are_redacted_for_confirmation() {
        let arguments = json!({"path": "src/main.rs", "token": "abc", "nested": {"api_key": "x"}});
        let text = describe_arguments(&arguments);
        assert!(text.contains("src/main.rs"));
        assert!(!text.contains("abc"));
        assert!(!text.contains("\"x\""));
        assert!(text.contains("<скрыто>"));
    }
}
