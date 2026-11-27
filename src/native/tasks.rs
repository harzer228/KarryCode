use anyhow::{Context, Result, bail, ensure};
use std::sync::atomic::{AtomicBool, Ordering};
use std::{
    io::Read,
    path::Path,
    process::{Command, Stdio},
};
fn command(task: &str) -> Result<(&'static str, Vec<&'static str>)> {
    Ok(match task {
        "cargo-test" => ("cargo", vec!["test", "--locked"]),
        "cargo-fetch" => ("cargo", vec!["fetch", "--locked"]),
        "npm-ci" => ("npm", vec!["ci", "--ignore-scripts"]),
        "npm-install" => ("npm", vec!["install", "--ignore-scripts"]),
        "python-install" => (
            "python",
            vec!["-m", "pip", "install", "-r", "requirements.txt"],
        ),
        "cargo-build" => ("cargo", vec!["build", "--locked"]),
        "cargo-run" => ("cargo", vec!["run", "--locked"]),
        "node-test" => ("node", vec!["--test"]),
        "npm-lint" => ("npm", vec!["run", "lint"]),
        "npm-start" => ("npm", vec!["run", "start"]),
        "npm-dev" => ("npm", vec!["run", "dev"]),
        "cargo-check" => ("cargo", vec!["check", "--locked"]),
        "npm-test" => ("npm", vec!["test"]),
        "npm-build" => ("npm", vec!["run", "build"]),
        "python-run" => ("python", vec!["main.py"]),
        "python-test" => ("python", vec!["-m", "unittest", "discover"]),
        _ => bail!(tr!("Неизвестная задача", "Unknown task")),
    })
}
pub(super) fn available(directory: &Path) -> Vec<&'static str> {
    let has = |name: &str| super::safe_path(directory, name).is_ok_and(|path| path.is_file());
    let mut tasks = Vec::new();
    if has("Cargo.toml") {
        tasks.extend([
            "cargo-check",
            "cargo-test",
            "cargo-build",
            "cargo-run",
            "cargo-fetch",
        ]);
    }
    if let Ok(text) = super::read_file(directory, "package.json")
        && let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&text)
    {
        for (name, task) in [
            ("test", "npm-test"),
            ("build", "npm-build"),
            ("lint", "npm-lint"),
            ("start", "npm-start"),
            ("dev", "npm-dev"),
        ] {
            if manifest["scripts"][name].is_string() {
                tasks.push(task);
            }
        }
        tasks.push(if has("package-lock.json") {
            "npm-ci"
        } else {
            "npm-install"
        });
    }
    if has("index.html") || has("package.json") {
        tasks.push("node-test");
    }
    if has("main.py")
        || has("pyproject.toml")
        || has("requirements.txt")
        || directory.join("tests").is_dir()
        || std::fs::read_dir(directory)
            .into_iter()
            .flatten()
            .flatten()
            .any(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with("test") && name.ends_with(".py"))
            })
    {
        tasks.push("python-test");
    }
    if has("main.py") {
        tasks.push("python-run");
    }
    if has("requirements.txt") {
        tasks.push("python-install");
    }
    tasks
}

pub(super) fn task(directory: &Path, task: &str) -> Result<(Command, String)> {
    let (program, args) = command(task)?;
    ensure!(
        available(directory).contains(&task),
        "Task is unavailable in the selected folder"
    );
    let detail = format!("{program} {}", args.join(" "));
    let mut child = Command::new(program);
    #[cfg(windows)]
    if program == "npm" {
        child = Command::new("cmd.exe");
        child.args(["/d", "/c", "npm.cmd"]);
    }
    child.args(&args);
    child.env("PYTHONIOENCODING", "utf-8");
    Ok((child, detail))
}

pub(super) fn shell(script: &str) -> Result<(Command, String)> {
    anyhow::ensure!(
        !script.trim().is_empty()
            && script.len() <= 8192
            && !script
                .chars()
                .any(|c| (c.is_control() && !matches!(c, '\r' | '\n' | '\t'))
                    || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')),
        "Expected a command up to 8 KiB"
    );
    #[cfg(windows)]
    {
        let (executable, legacy) = windows_shell();
        let label = if legacy {
            "Windows PowerShell 5.1"
        } else {
            "PowerShell 7"
        };
        let rewritten = legacy.then(|| legacy_chain(script)).flatten();
        let effective = rewritten.as_deref().unwrap_or(script);
        let source = if legacy {
            format!(
                "[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false); $OutputEncoding = [Console]::OutputEncoding; $ProgressPreference = 'SilentlyContinue'; $PSDefaultParameterValues['Get-Content:Encoding'] = 'UTF8';\n{effective}"
            )
        } else {
            format!(
                "[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false); $ProgressPreference = 'SilentlyContinue';\n{effective}"
            )
        };
        use base64::Engine as _;
        let utf16: Vec<_> = source.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let encoded = base64::engine::general_purpose::STANDARD.encode(utf16);
        let mut command = Command::new(executable);
        command.args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-OutputFormat",
            "Text",
            "-EncodedCommand",
            &encoded,
        ]);
        command.env("PYTHONIOENCODING", "utf-8");
        let detail = if rewritten.is_some() {
            trf!(
                "{} · совместимый синтаксис\n{}",
                "{} · compatibility syntax\n{}",
                label,
                effective
            )
        } else {
            format!("{label}\n{effective}")
        };
        Ok((command, detail))
    }
    #[cfg(unix)]
    {
        let mut command = Command::new("/bin/bash");
        command.args(["--noprofile", "--norc", "-c", script]);
        command.env_remove("BASH_ENV").env_remove("ENV");
        Ok((command, format!("Bash\n{script}")))
    }
}

#[cfg(windows)]
fn windows_shell() -> (&'static Path, bool) {
    use std::path::PathBuf;
    static SHELL: std::sync::OnceLock<(PathBuf, bool)> = std::sync::OnceLock::new();
    let (path, legacy) = SHELL.get_or_init(|| {
        let modern = std::env::var_os("ProgramFiles")
            .map(PathBuf::from)
            .map(|path| path.join("PowerShell/7/pwsh.exe"))
            .filter(|path| path.is_file())
            .or_else(|| {
                std::env::var_os("PATH").and_then(|path| {
                    std::env::split_paths(&path)
                        .filter(|path| path.is_absolute())
                        .map(|path| path.join("pwsh.exe"))
                        .find(|path| path.is_file())
                })
            });
        if let Some(path) = modern {
            (path, false)
        } else {
            let system = std::env::var_os("SystemRoot")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
            (
                system.join("System32/WindowsPowerShell/v1.0/powershell.exe"),
                true,
            )
        }
    });
    (path, *legacy)
}

pub(super) fn shell_guidance() -> &'static str {
    #[cfg(windows)]
    if windows_shell().1 {
        return "The shell is Windows PowerShell 5.1. Separate independent commands with ;. Use if ($?) for conditional chaining. Bash syntax is unavailable.";
    }
    #[cfg(windows)]
    return "The shell is PowerShell 7. Use PowerShell syntax.";
    #[cfg(unix)]
    return "The shell is Bash.";
}

#[cfg(any(windows, test))]
fn legacy_chain(script: &str) -> Option<String> {
    if !script.contains("&&") {
        return None;
    }
    let segments: Vec<_> = script.split("&&").map(str::trim).collect();
    if segments.len() > 12
        || segments.iter().any(|command| {
            command.is_empty()
                || !command
                    .chars()
                    .all(|ch| ch.is_alphanumeric() || ch == ' ' || "_-./:\\=,+%".contains(ch))
        })
    {
        return None;
    }
    // Preserve the condition and the native exit status of each simple command.
    Some(segments.iter().map(|command| format!(
        "$LASTEXITCODE = 0; {command}; if (-not $?) {{ if ($LASTEXITCODE) {{ exit $LASTEXITCODE }}; exit 1 }}"
    )).collect::<Vec<_>>().join("\n"))
}

#[cfg(windows)]
pub(super) fn suppress_loader_dialogs() {
    use windows_sys::Win32::System::Diagnostics::Debug::{
        GetErrorMode, SEM_FAILCRITICALERRORS, SEM_NOGPFAULTERRORBOX, SEM_NOOPENFILEERRORBOX,
        SetErrorMode,
    };
    static INITIALIZE: std::sync::Once = std::sync::Once::new();
    INITIALIZE.call_once(|| unsafe {
        SetErrorMode(
            GetErrorMode()
                | SEM_FAILCRITICALERRORS
                | SEM_NOGPFAULTERRORBOX
                | SEM_NOOPENFILEERRORBOX,
        );
    });
}

pub(super) fn execute(child: Command, directory: &Path, timeout: u64) -> Result<String> {
    Ok(execute_cancellable(child, directory, timeout, &AtomicBool::new(false))?.text)
}

pub(super) struct Output {
    pub success: bool,
    pub text: String,
    pub stdout: String,
}

pub(super) fn stdin_file(payload: &[u8]) -> Result<std::fs::File> {
    use std::io::{Seek, Write};
    let mut file = tempfile::tempfile()?;
    file.write_all(payload)?;
    file.rewind()?;
    Ok(file)
}

fn discard_inherited_env(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    name.contains("KEY")
        || name.contains("TOKEN")
        || name.contains("SECRET")
        || name.contains("PASSWORD")
        || matches!(name.as_str(), "GIT_CONFIG_COUNT" | "GIT_CONFIG_PARAMETERS")
        || name.starts_with("GIT_CONFIG_VALUE_")
}

pub(super) fn execute_cancellable(
    child: Command,
    directory: &Path,
    timeout: u64,
    cancel: &AtomicBool,
) -> Result<Output> {
    execute_inner(child, directory, timeout, cancel, None)
}

pub(super) fn execute_payload(
    child: Command,
    directory: &Path,
    timeout: u64,
    cancel: &AtomicBool,
    payload: &[u8],
) -> Result<Output> {
    execute_inner(child, directory, timeout, cancel, Some(payload))
}

fn execute_inner(
    mut child: Command,
    directory: &Path,
    timeout: u64,
    cancel: &AtomicBool,
    payload: Option<&[u8]>,
) -> Result<Output> {
    #[cfg(windows)]
    suppress_loader_dialogs();
    let stdout = tempfile::tempfile()?;
    let stderr = tempfile::tempfile()?;
    child
        .current_dir(directory)
        .stdin(match payload {
            Some(value) => Stdio::from(stdin_file(value)?),
            None => Stdio::null(),
        })
        .stdout(stdout.try_clone()?)
        .stderr(stderr.try_clone()?);
    for (key, _) in std::env::vars_os() {
        if discard_inherited_env(&key.to_string_lossy()) {
            child.env_remove(key);
        }
    }
    crate::platform::require_standard_user()?;
    let status = run_process(child, &stdout, &stderr, timeout, cancel, payload.is_some())?;
    collected(
        stdout,
        stderr,
        status
            .as_ref()
            .map(ToString::to_string)
            .map_err(|reason| *reason),
        status
            .as_ref()
            .ok()
            .and_then(|status| status.code())
            .map(|code| code as u32),
        status.as_ref().is_ok_and(|status| status.success()),
    )
}

pub(super) fn collected(
    stdout: std::fs::File,
    stderr: std::fs::File,
    status: Result<String, &str>,
    code: Option<u32>,
    success: bool,
) -> Result<Output> {
    let stdout = limited_output(stdout)?;
    let mut stderr = limited_output(stderr)?;
    if cfg!(windows) && code == Some(0xc0000135) {
        stderr.push_str(tr!(
            "\nWindows не смог загрузить DLL программы (0xC0000135). Проверь доступ к установленному рантайму.",
            "\nWindows could not load a required DLL (0xC0000135). Check access to the installed runtime."
        ));
    }
    if cfg!(windows) && code == Some(0xc0000022) {
        stderr.push_str(tr!(
            "\nWindows отказал в доступе к программе или её зависимостям (0xC0000022).",
            "\nWindows denied access to the program or its dependencies (0xC0000022)."
        ));
    }
    if cfg!(windows) && stderr.contains("ParserError") && stderr.contains("'&&'") {
        stderr.push_str(tr!(
            "\nДля && используй PowerShell 7. В 5.1 после первой команды проверь $? перед второй.",
            "\nUse PowerShell 7 for &&. In 5.1, check $? before running the second command."
        ));
    }
    Ok(Output {
        success,
        text: format!(
            "Exit: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            status.unwrap_or_else(str::to_owned)
        ),
        stdout,
    })
}

fn limited_output(mut file: std::fs::File) -> Result<String> {
    use std::io::{Seek, SeekFrom};
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.take(32 * 1024 + 1).read_to_end(&mut bytes)?;
    let truncated = bytes.len() > 32 * 1024;
    bytes.truncate(32 * 1024);
    let decoded = if bytes.starts_with(&[0xff, 0xfe]) {
        let (pairs, _) = bytes[2..].as_chunks::<2>();
        String::from_utf16_lossy(
            &pairs
                .iter()
                .copied()
                .map(u16::from_le_bytes)
                .collect::<Vec<_>>(),
        )
    } else {
        String::from_utf8_lossy(bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes))
            .into_owned()
    };
    let decoded = if decoded.starts_with("#< CLIXML") {
        clixml(&decoded)
    } else {
        decoded
    };
    let mut text: String = decoded
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect();
    if truncated {
        text.push_str("\n[output truncated]");
    }
    Ok(text)
}

fn clixml(input: &str) -> String {
    let Some((plain, xml)) = input.split_once("<Objs") else {
        return input.to_owned();
    };
    let mut text = plain.trim_start_matches("#< CLIXML").trim().to_owned();
    for part in xml.split("<S S=\"").skip(1) {
        let Some((stream, value)) = part.split_once("\">") else {
            continue;
        };
        let Some((value, _)) = value.split_once("</S>") else {
            continue;
        };
        if matches!(
            stream,
            "Error" | "Warning" | "Verbose" | "Debug" | "Information"
        ) {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(
                &value
                    .replace("_x000D__x000A_", "\n")
                    .replace("_x000A_", "\n")
                    .replace("&lt;", "<")
                    .replace("&gt;", ">")
                    .replace("&amp;", "&"),
            );
        }
    }
    text
}

fn run_process(
    command: Command,
    stdout: &std::fs::File,
    stderr: &std::fs::File,
    timeout: u64,
    cancel: &AtomicBool,
    limited: bool,
) -> Result<Result<std::process::ExitStatus, &'static str>> {
    ensure!(
        !cancel.load(Ordering::Relaxed),
        "Command canceled before start"
    );
    let mut process = ProcessTree::spawn_limited(command, limited).context(tr!(
        "Не удалось запустить процесс",
        "Could not start the process"
    ))?;
    let started = std::time::Instant::now();
    loop {
        if let Some(status) = process.0.try_wait()? {
            return Ok(Ok(status));
        }
        let reason = if cancel.load(Ordering::Relaxed) {
            Some("stopped by user")
        } else if started.elapsed().as_secs() >= timeout {
            Some("time limit reached")
        } else if stdout
            .metadata()?
            .len()
            .saturating_add(stderr.metadata()?.len())
            > 16 * 1024 * 1024
        {
            Some("16 MiB output limit reached")
        } else {
            None
        };
        if let Some(reason) = reason {
            return Ok(Err(reason));
        }
        std::thread::sleep(std::time::Duration::from_millis(45));
    }
}
pub(super) struct ProcessTree(
    std::process::Child,
    #[cfg(windows)] windows_sys::Win32::Foundation::HANDLE,
);
impl ProcessTree {
    pub(super) fn child_mut(&mut self) -> &mut std::process::Child {
        &mut self.0
    }
    pub(super) fn spawn(command: Command) -> Result<Self> {
        Self::spawn_limited(command, false)
    }

    fn spawn_limited(mut command: Command, limited: bool) -> Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
            if limited {
                // Inherited by the interpreter and all sandbox descendants.
                unsafe {
                    command.pre_exec(|| {
                        for (resource, amount) in [
                            (libc::RLIMIT_AS, 512 * 1024 * 1024),
                            (libc::RLIMIT_FSIZE, 16 * 1024 * 1024),
                            (libc::RLIMIT_CPU, 15),
                        ] {
                            let bounds = libc::rlimit {
                                rlim_cur: amount,
                                rlim_max: amount,
                            };
                            if libc::setrlimit(resource, &bounds) != 0 {
                                return Err(std::io::Error::last_os_error());
                            }
                        }
                        Ok(())
                    });
                }
            }
        }
        #[cfg(windows)]
        if limited {
            use std::os::windows::process::CommandExt as _;
            command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
        }
        let child = command.spawn()?;
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::{Foundation::CloseHandle, System::JobObjects::*};
            let mut child = child;
            let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if limited {
                limits.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_JOB_MEMORY;
                limits.JobMemoryLimit = 512 * 1024 * 1024;
            }
            if job.is_null()
                || unsafe {
                    SetInformationJobObject(
                        job,
                        JobObjectExtendedLimitInformation,
                        &limits as *const _ as _,
                        std::mem::size_of_val(&limits) as u32,
                    )
                } == 0
                || unsafe { AssignProcessToJobObject(job, child.as_raw_handle() as _) } == 0
            {
                let error = std::io::Error::last_os_error();
                let _ = child.kill();
                let _ = child.wait();
                if !job.is_null() {
                    unsafe {
                        CloseHandle(job);
                    }
                }
                return Err(error.into());
            }
            Ok(Self(child, job))
        }
        #[cfg(unix)]
        Ok(Self(child))
    }
}
impl Drop for ProcessTree {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.1);
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

    #[cfg(windows)]
    #[test]
    #[ignore = "requires an installed Windows Python runtime"]
    fn missing_python_dll_is_reported_without_a_system_dialog() -> Result<()> {
        let paths = std::env::var_os("PATH")
            .context(tr!("Переменная PATH отсутствует", "PATH is missing"))?;
        let installed = std::env::split_paths(&paths)
            .map(|folder| folder.join("python.exe"))
            .find(|path| path.is_file())
            .context(tr!(
                "Требуется установленный Python",
                "An installed Python is required"
            ))?;
        let temp = tempfile::tempdir()?;
        let incomplete = temp.path().join("python.exe");
        std::fs::copy(installed, &incomplete)?;
        let mut command = Command::new(incomplete);
        command.arg("--version").env_remove("PATH");
        let output = execute_cancellable(command, temp.path(), 3, &AtomicBool::new(false))?;
        assert!(!output.success);
        assert!(output.text.contains("0xC0000135"), "{}", output.text);
        Ok(())
    }

    #[test]
    fn power_shell_keeps_errors_and_discards_progress_records() {
        let xml = "#< CLIXML\n<Objs><S S=\"progress\">Module loading</S><S S=\"Error\">Cyrillic: ошибка_x000D__x000A_</S></Objs>";
        assert_eq!(clixml(xml), "Cyrillic: ошибка\n");
    }

    #[test]
    fn command_output_decodes_utf16_le_and_ignores_an_incomplete_code_unit() -> Result<()> {
        use std::io::Write as _;

        let mut file = tempfile::tempfile()?;
        file.write_all(&[0xff, 0xfe])?;
        let output = "Привет 🌿\n";
        for unit in output.encode_utf16() {
            file.write_all(&unit.to_le_bytes())?;
        }
        file.write_all(&[0xff])?;
        assert_eq!(limited_output(file)?, output);
        Ok(())
    }

    #[test]
    fn legacy_chain_rewrites_only_simple_commands() {
        let command = legacy_chain("python --version && echo shell-ok").unwrap();
        assert!(command.contains("python --version; if (-not $?"));
        assert!(command.contains("exit $LASTEXITCODE"));
        assert!(command.contains("echo shell-ok"));
        assert!(legacy_chain("Write-Output 'literal && unchanged'").is_none());
        assert!(legacy_chain("Get-ChildItem | Select-Object Name && echo finished").is_none());
    }

    #[test]
    fn git_command_overrides_are_removed_together() {
        for key in [
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_KEY_0",
            "GIT_CONFIG_VALUE_0",
            "git_config_value_11",
            "GIT_CONFIG_PARAMETERS",
        ] {
            assert!(discard_inherited_env(key), "{key}");
        }
        assert!(!discard_inherited_env("GIT_CONFIG_NOSYSTEM"));
    }

    #[test]
    fn task_names_cannot_inject_shell_commands() {
        assert!(command("npm-test && echo bad").is_err());
        assert_eq!(command("cargo-test").unwrap().1, vec!["test", "--locked"]);
    }
}
