use crate::native::tasks;
use anyhow::{Context, Result, ensure};
use std::{
    ffi::{OsStr, OsString},
    fs::{File, OpenOptions},
    mem::{size_of, size_of_val},
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
        process::CommandExt as _,
    },
    path::{Path, PathBuf},
    process::Command,
    ptr::{null, null_mut},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{HANDLE, HANDLE_FLAG_INHERIT, LocalFree, SetHandleInformation},
    Security::{
        Authorization::ConvertSidToStringSidW,
        FreeSid,
        Isolation::{CreateAppContainerProfile, DeleteAppContainerProfile},
        PSID, SECURITY_CAPABILITIES,
    },
    System::{JobObjects::*, Threading::*},
};

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain([0]).collect()
}

struct Profile {
    name: Vec<u16>,
    sid: PSID,
}

impl Profile {
    fn create(work: &Path, cancel: &AtomicBool) -> Result<Self> {
        let mut nonce = [0u8; 12];
        getrandom::fill(&mut nonce).map_err(|error| anyhow::anyhow!("{error}"))?;
        let identity = format!(
            "KarryCode.{}",
            nonce.iter().map(|b| format!("{b:02x}")).collect::<String>()
        );
        let name = wide(OsStr::new(&identity));
        let mut sid = null_mut();
        let result = unsafe {
            CreateAppContainerProfile(
                name.as_ptr(),
                name.as_ptr(),
                name.as_ptr(),
                null(),
                0,
                &mut sid,
            )
        };
        ensure!(
            result >= 0 && !sid.is_null(),
            "AppContainer profile failed ({result:#x})"
        );
        let profile = Self { name, sid };
        let mut sid_text = null_mut();
        win(unsafe { ConvertSidToStringSidW(sid, &mut sid_text) })?;
        let length = (0..)
            .take_while(|i| unsafe { *sid_text.add(*i) } != 0)
            .count();
        let sid_text_value =
            String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(sid_text, length) });
        unsafe { LocalFree(sid_text.cast()) };
        let icacls = system_root().join("System32/icacls.exe");
        let grant = format!("*{sid_text_value}:(OI)(CI)M");
        for args in [["/grant", &grant], ["/setintegritylevel", "(OI)(CI)L"]] {
            let mut command = Command::new(&icacls);
            command
                .arg(work)
                .args(args)
                .creation_flags(CREATE_NO_WINDOW);
            let output = tasks::execute_cancellable(command, work, 4, cancel)?;
            ensure!(
                output.success,
                "Could not set access on the temporary AppContainer directory: {}",
                output.text
            );
        }
        Ok(profile)
    }
}

impl Drop for Profile {
    fn drop(&mut self) {
        unsafe {
            DeleteAppContainerProfile(self.name.as_ptr());
            FreeSid(self.sid);
        }
    }
}

struct Attributes {
    storage: Vec<usize>,
}

impl Attributes {
    fn new() -> Result<Self> {
        let mut size = 0;
        unsafe { InitializeProcThreadAttributeList(null_mut(), 3, 0, &mut size) };
        ensure!(
            size > 0,
            "Could not allocate AppContainer startup attributes"
        );
        let mut storage = vec![0; size.div_ceil(size_of::<usize>())];
        win(unsafe {
            InitializeProcThreadAttributeList(storage.as_mut_ptr().cast(), 3, 0, &mut size)
        })?;
        Ok(Self { storage })
    }

    fn set<T>(&mut self, kind: u32, value: &T) -> Result<()> {
        win(unsafe {
            UpdateProcThreadAttribute(
                self.storage.as_mut_ptr().cast(),
                0,
                kind as usize,
                std::ptr::from_ref(value).cast(),
                size_of_val(value),
                null_mut(),
                null_mut(),
            )
        })
    }
}

impl Drop for Attributes {
    fn drop(&mut self) {
        unsafe { DeleteProcThreadAttributeList(self.storage.as_mut_ptr().cast()) };
    }
}

fn win(success: i32) -> Result<()> {
    if success == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

fn system_root() -> PathBuf {
    std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
}

fn executable(command: &Command) -> Result<PathBuf> {
    let name = Path::new(command.get_program());
    if name.is_absolute() {
        return Ok(name.to_owned());
    }
    let name = name.with_extension("exe");
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .filter(|path| path.is_absolute())
        .map(|path| path.join(&name))
        .find(|path| path.is_file())
        .with_context(|| format!("Executable is missing: {}", name.display()))
}

fn quote(arg: &OsStr, line: &mut Vec<u16>) {
    if !arg.is_empty()
        && !arg
            .encode_wide()
            .any(|ch| ch == b' ' as u16 || ch == b'\t' as u16 || ch == b'"' as u16)
    {
        line.extend(arg.encode_wide());
        return;
    }
    // Windows command-line parsing requires doubling backslashes before quotes.
    line.push(b'"' as u16);
    let mut backslashes = 0;
    for ch in arg.encode_wide() {
        if ch == b'\\' as u16 {
            backslashes += 1;
            continue;
        }
        let count = if ch == b'"' as u16 {
            backslashes * 2 + 1
        } else {
            backslashes
        };
        line.extend(std::iter::repeat_n(b'\\' as u16, count));
        backslashes = 0;
        line.push(ch);
    }
    line.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2));
    line.push(b'"' as u16);
}

fn env_block(work: &Path, isolated_extension: bool) -> Vec<u16> {
    let mut env = Vec::new();
    for (key, value) in [
        (
            "LOCALAPPDATA",
            if isolated_extension {
                work.as_os_str().to_owned()
            } else {
                std::env::var_os("LOCALAPPDATA").unwrap_or_else(|| {
                    crate::platform::home()
                        .join("AppData/Local")
                        .into_os_string()
                })
            },
        ),
        (
            "PATH",
            if isolated_extension {
                system_root().join("System32").into_os_string()
            } else {
                std::env::var_os("PATH").unwrap_or_default()
            },
        ),
        ("PYTHONIOENCODING", OsString::from("utf-8")),
        ("SystemRoot", system_root().into_os_string()),
        ("TEMP", work.join(".karrycode-tmp").into_os_string()),
        ("TMP", work.join(".karrycode-tmp").into_os_string()),
        ("USERPROFILE", work.as_os_str().to_owned()),
        ("WINDIR", system_root().into_os_string()),
    ] {
        let mut entry = OsString::from(format!("{key}="));
        entry.push(value);
        env.extend(wide(&entry));
    }
    env.push(0);
    env
}

pub(super) fn probe(temp: &Path, relative: &Path, command: &Command) -> Result<()> {
    probe_cancellable(temp, relative, command, &AtomicBool::new(false), 15, false)
}

pub(super) fn probe_cancellable(
    temp: &Path,
    relative: &Path,
    command: &Command,
    cancel: &AtomicBool,
    timeout: u64,
    extension: bool,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(timeout);
    let exe = executable(command)?;
    let first = probe_executable(temp, relative, &exe, cancel, timeout.min(5));
    if first.is_ok() || !can_stage_runtime(&exe) {
        return first;
    }
    let install = exe.parent().context(tr!(
        "Путь установки среды выполнения не найден",
        "Runtime installation path is missing"
    ))?;
    let runtime = temp.join("work/.karrycode-runtime");
    let minimal_python = extension
        && exe.file_name().and_then(OsStr::to_str).is_some_and(|name| {
            name.eq_ignore_ascii_case("python.exe") || name.eq_ignore_ascii_case("python3.exe")
        });
    copy_runtime(install, &runtime, cancel, deadline, minimal_python)?;
    let left = deadline.saturating_duration_since(Instant::now()).as_secs();
    ensure!(left > 0, "AppContainer preparation exceeded its time limit");
    probe_executable(
        temp,
        relative,
        &runtime.join(exe.file_name().unwrap()),
        cancel,
        left.min(5),
    )
}

fn can_stage_runtime(exe: &Path) -> bool {
    let Some(install) = exe.parent() else {
        return false;
    };
    let Some(name) = exe.file_name().and_then(OsStr::to_str) else {
        return false;
    };
    if name.eq_ignore_ascii_case("pwsh.exe") {
        return ["pwsh.dll", "pwsh.runtimeconfig.json", "hostfxr.dll"]
            .iter()
            .all(|name| install.join(name).is_file());
    }
    if name.eq_ignore_ascii_case("python.exe") || name.eq_ignore_ascii_case("python3.exe") {
        return install.join("Lib/os.py").is_file()
            && std::fs::read_dir(install)
                .into_iter()
                .flatten()
                .flatten()
                .any(|entry| {
                    entry.file_name().to_str().is_some_and(|name| {
                        name.strip_prefix("python3")
                            .and_then(|suffix| suffix.strip_suffix(".dll"))
                            .is_some_and(|version| {
                                !version.is_empty() && version.chars().all(|ch| ch.is_ascii_digit())
                            })
                    })
                });
    }
    false
}

fn copy_runtime(
    source: &Path,
    target: &Path,
    cancel: &AtomicBool,
    deadline: Instant,
    minimal_python: bool,
) -> Result<()> {
    let mut directories = vec![(source.to_owned(), target.to_owned())];
    let mut total = 0u64;
    let mut count = 0;
    while let Some((folder, target)) = directories.pop() {
        std::fs::create_dir(&target)?;
        for entry in std::fs::read_dir(&folder)? {
            ensure!(
                !cancel.load(Ordering::Relaxed) && Instant::now() < deadline,
                "AppContainer runtime staging stopped or timed out"
            );
            let entry = entry?;
            let meta = std::fs::symlink_metadata(entry.path())?;
            if minimal_python
                && !python_runtime_entry(
                    folder.strip_prefix(source)?,
                    &entry.file_name(),
                    meta.is_dir(),
                )
            {
                continue;
            }
            if entry.file_name() == "__pycache__"
                || crate::safe_files::is_link(&meta)
                || super::permissions::protected(Path::new(&entry.file_name()))
            {
                continue;
            }
            count += 1;
            ensure!(
                count <= 6000,
                "Runtime installation has too many files to stage"
            );
            let destination = target.join(entry.file_name());
            if meta.is_dir() {
                directories.push((entry.path(), destination));
            } else if meta.is_file() {
                total = total.saturating_add(meta.len());
                ensure!(
                    total <= 256 * 1024 * 1024,
                    "Runtime installation is too large to stage"
                );
                // Give the copies the staging directory's permissions.
                let mut input = File::open(entry.path())?;
                let mut output = File::create_new(destination)?;
                std::io::copy(&mut input, &mut output)?;
            }
        }
    }
    Ok(())
}

fn python_runtime_entry(directory: &Path, entry: &OsStr, is_dir: bool) -> bool {
    let name = entry.to_string_lossy().to_ascii_lowercase();
    if directory.as_os_str().is_empty() {
        return if is_dir {
            matches!(name.as_str(), "lib" | "dlls")
        } else {
            matches!(name.as_str(), "python.exe" | "python3.exe")
                || [".dll", ".zip", "._pth"]
                    .iter()
                    .any(|suffix| name.ends_with(suffix))
        };
    }
    if is_dir {
        !matches!(
            name.as_str(),
            "site-packages" | "__pycache__" | "test" | "tests"
        )
    } else {
        !name.ends_with(".pyc")
    }
}

fn probe_executable(
    temp: &Path,
    relative: &Path,
    exe: &Path,
    cancel: &AtomicBool,
    timeout: u64,
) -> Result<()> {
    let mut probe = Command::new(exe);
    if exe.file_name().and_then(OsStr::to_str).is_some_and(|file| {
        file.eq_ignore_ascii_case("powershell.exe") || file.eq_ignore_ascii_case("pwsh.exe")
    }) {
        probe.args(["-NoProfile", "-NonInteractive", "-Command", "exit 0"]);
    } else if exe
        .file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case("cmd.exe"))
    {
        probe.args(["/d", "/c", "exit", "0"]);
    } else {
        probe.arg("--version");
    }
    let output = execute(temp, relative, &probe, timeout, cancel)?;
    ensure!(
        output.success,
        "AppContainer probe could not run the selected executable: {}",
        output.text
    );
    Ok(())
}

pub(super) fn execute(
    temp: &Path,
    relative: &Path,
    command: &Command,
    timeout: u64,
    cancel: &AtomicBool,
) -> Result<tasks::Output> {
    execute_input(temp, relative, command, timeout, cancel, None)
}

pub(super) fn execute_input(
    temp: &Path,
    relative: &Path,
    command: &Command,
    timeout: u64,
    cancel: &AtomicBool,
    payload: Option<&[u8]>,
) -> Result<tasks::Output> {
    tasks::suppress_loader_dialogs();
    ensure!(
        !cancel.load(Ordering::Relaxed),
        "Command canceled before start"
    );
    let work = temp.join("work");
    std::fs::create_dir_all(work.join(".karrycode-tmp"))?;
    let profile = Profile::create(&work, cancel)?;
    let mut exe = executable(command)?;
    let staged = work
        .join(".karrycode-runtime")
        .join(exe.file_name().context(tr!(
            "Имя исполняемого файла не найдено",
            "Executable filename is missing"
        ))?);
    if staged.is_file() {
        exe = staged;
    }
    let mut line = vec![b'"' as u16];
    line.extend(exe.as_os_str().encode_wide());
    line.push(b'"' as u16);
    for arg in command.get_args() {
        line.push(b' ' as u16);
        quote(arg, &mut line);
    }
    line.push(0);
    let mut env = env_block(&work, payload.is_some());
    let out = tempfile::tempfile()?;
    let err = tempfile::tempfile()?;
    let input = match payload {
        Some(value) => tasks::stdin_file(value)?,
        None => OpenOptions::new().read(true).open("NUL")?,
    };
    let redirect = |file: &File| file.as_raw_handle() as HANDLE;
    let handles = [redirect(&input), redirect(&out), redirect(&err)];
    for handle in handles {
        win(unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) })?;
    }
    let security = SECURITY_CAPABILITIES {
        AppContainerSid: profile.sid,
        ..Default::default()
    };
    let no_general_packages = 1u32;
    let mut attributes = Attributes::new()?;
    attributes.set(PROC_THREAD_ATTRIBUTE_HANDLE_LIST, &handles)?;
    attributes.set(PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, &security)?;
    attributes.set(
        PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY,
        &no_general_packages,
    )?;
    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = handles[0];
    startup.StartupInfo.hStdOutput = handles[1];
    startup.StartupInfo.hStdError = handles[2];
    startup.lpAttributeList = attributes.storage.as_mut_ptr().cast();
    let job = unsafe { CreateJobObjectW(null(), null()) };
    ensure!(
        !job.is_null(),
        "Could not create a process job for AppContainer"
    );
    let job_owner = unsafe { OwnedHandle::from_raw_handle(job) };
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    if payload.is_some() {
        limits.BasicLimitInformation.LimitFlags |=
            JOB_OBJECT_LIMIT_JOB_MEMORY | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
        limits.BasicLimitInformation.ActiveProcessLimit = 8;
        limits.JobMemoryLimit = 512 * 1024 * 1024;
    }
    win(unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            std::ptr::from_ref(&limits).cast(),
            size_of_val(&limits) as u32,
        )
    })?;
    let cwd = wide(work.join(relative).as_os_str());
    let mut process = PROCESS_INFORMATION::default();
    win(unsafe {
        CreateProcessW(
            wide(exe.as_os_str()).as_ptr(),
            line.as_mut_ptr(),
            null(),
            null(),
            1,
            EXTENDED_STARTUPINFO_PRESENT
                | CREATE_NO_WINDOW
                | CREATE_SUSPENDED
                | CREATE_UNICODE_ENVIRONMENT,
            env.as_mut_ptr().cast(),
            cwd.as_ptr(),
            &startup.StartupInfo,
            &mut process,
        )
    })
    .context(tr!(
        "Не удалось запустить процесс в AppContainer",
        "Could not launch AppContainer process"
    ))?;
    let _process = unsafe { OwnedHandle::from_raw_handle(process.hProcess) };
    let _thread = unsafe { OwnedHandle::from_raw_handle(process.hThread) };
    if let Err(error) = win(unsafe { AssignProcessToJobObject(job, process.hProcess) }) {
        unsafe { TerminateProcess(process.hProcess, 1) };
        return Err(error);
    }
    ensure!(
        unsafe { ResumeThread(process.hThread) } != u32::MAX,
        "Could not start AppContainer thread"
    );
    let started = Instant::now();
    let status = loop {
        if unsafe { WaitForSingleObject(process.hProcess, 45) } == 0 {
            let mut code = 0;
            win(unsafe { GetExitCodeProcess(process.hProcess, &mut code) })?;
            break Ok(code);
        }
        if cancel.load(Ordering::Relaxed) {
            break Err("stopped by user");
        }
        if started.elapsed() >= Duration::from_secs(timeout) {
            break Err("time limit reached");
        }
        if out.metadata()?.len().saturating_add(err.metadata()?.len()) > 16 * 1024 * 1024 {
            break Err("16 MiB output limit reached");
        }
    };
    drop(job_owner);
    unsafe { WaitForSingleObject(process.hProcess, 1000) };
    tasks::collected(
        out,
        err,
        status.map(|code| format!("exit code: {code}")),
        status.ok(),
        status == Ok(0),
    )
}
