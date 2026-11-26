use super::{changes, field, permissions, sandbox, tasks, ui};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

pub(super) fn request_permissions(screen: &mut ui::Screen, action: &Value) -> Result<String> {
    let mode = permissions::Mode::parse(field(action, "mode"))
        .context(tr!("Выбери ask, auto или full", "Choose ask, auto or full"))?;
    let reason = field(action, "reason").trim();
    ensure!(
        !reason.is_empty() && reason.chars().count() <= 600,
        "Give a reason of up to 600 characters"
    );
    if mode == screen.permission_mode() {
        return Ok(tr!(
            "Этот режим разрешений уже включён.",
            "That permission mode is already active."
        )
        .into());
    }
    if screen.request_permission(mode, reason)? {
        Ok(trf!(
            "Разрешения подтверждены: {} ({})",
            "Permission granted: {} ({})",
            mode.label(),
            mode.identifier()
        ))
    } else {
        Ok(trf!(
            "Запрос разрешения отклонён. Режим: {}",
            "Permission request declined. Mode: {}",
            screen.permission_mode().label()
        ))
    }
}

pub(super) struct CommandRequest<'a> {
    pub(super) root: &'a Path,
    pub(super) cwd: &'a Path,
    pub(super) process: std::process::Command,
    pub(super) detail: &'a str,
    pub(super) timeout: u64,
}

fn isolation_reason(error: &anyhow::Error) -> String {
    let detail = format!("{error:#}");
    let lower = detail.to_ascii_lowercase();
    if lower.contains("registry") && lower.contains("powershell") {
        return tr!(
            "AppContainer не даёт оболочке прочитать раздел реестра PowerShell.",
            "AppContainer denied access to the registry key required by PowerShell."
        )
        .into();
    }
    if lower.contains("psetwlog") {
        return tr!(
            "PowerShell не смог запустить журнал событий в AppContainer.",
            "PowerShell could not initialize event logging in AppContainer."
        )
        .into();
    }
    if lower.contains("0xc0000135") {
        return tr!(
            "В изоляции недоступна DLL установленного рантайма.",
            "The installed runtime requires a DLL unavailable inside the sandbox."
        )
        .into();
    }
    if lower.contains("0xc0000022") {
        return tr!(
            "Windows заблокировал программу или её зависимости внутри песочницы.",
            "Windows denied access to the program or its dependencies inside the sandbox."
        )
        .into();
    }
    detail
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("Unknown error")
        .chars()
        .take(240)
        .collect()
}

pub(super) fn command_in_chat(
    screen: &mut ui::Screen,
    request: CommandRequest<'_>,
    undo: &mut Option<changes::Batch>,
) -> Result<String> {
    crate::platform::require_standard_user()?;
    let CommandRequest {
        root,
        cwd,
        process,
        detail,
        timeout,
    } = request;
    let relative = cwd.strip_prefix(root.canonicalize()?)?;
    let cancel = AtomicBool::new(false);
    let sandbox = std::thread::scope(|scope| {
        let process = &process;
        let worker = scope.spawn(move || {
            let workspace = sandbox::Workspace::prepare(root)?;
            workspace.probe(relative, process)?;
            Ok(workspace)
        });
        screen.wait_for_tool(
            tr!("Проверяю песочницу", "Checking isolation"),
            &cancel,
            worker,
        )
    })?;
    if cancel.load(Ordering::Relaxed) {
        return Ok(tr!("Команда отменена.", "Command canceled.").into());
    }
    let mode = match &sandbox {
        Ok(_) => tr!(
            "Изолировано · временная копия проекта",
            "Isolated · temporary project copy"
        )
        .to_owned(),
        Err(error) => trf!(
            "Изоляция недоступна. {}\nОднократный запуск с твоими правами.",
            "Isolation unavailable. {}\nRun once with your permissions.",
            isolation_reason(error)
        ),
    };
    if (sandbox.is_err() || screen.permission_mode() == permissions::Mode::Ask)
        && !screen.review(
            if sandbox.is_err() {
                tr!("Запустить без изоляции?", "Run without isolation?")
            } else {
                tr!("Выполнить команду", "Run shell command")
            },
            &trf!(
                "{}\nПапка: {}\nЛимит: {} с\n\n{}",
                "{}\nFolder: {}\nTimeout: {} s\n\n{}",
                mode,
                crate::folders::display(cwd),
                timeout,
                detail
            ),
        )?
    {
        return Ok(if sandbox.is_err() {
            tr!(
                "Запуск без изоляции отклонён. Для проверки файлов используй встроенные read, search или glob.",
                "Running without isolation was declined. Use built-in read, search, or glob to inspect project files."
            )
        } else {
            tr!("Команда отменена.", "Command canceled.")
        }.into());
    }
    let container = sandbox.ok();
    let mut output = std::thread::scope(|scope| {
        let sandbox = container.as_ref();
        let cancel = &cancel;
        let worker = scope.spawn(move || match sandbox {
            Some(workspace) => workspace.execute(relative, &process, timeout, cancel),
            None => tasks::execute_cancellable(process, cwd, timeout, cancel),
        });
        screen.wait_for_tool(tr!("Выполняется", "Running"), cancel, worker)?
    })?;
    if let Some(workspace) = container
        && output.success
    {
        let edits = workspace.changes()?;
        if !edits.is_empty() {
            output.text.push('\n');
            let current = screen.permission_mode();
            output.text.push_str(&review_changes(
                screen,
                current,
                &Value::Null,
                root,
                edits,
                undo,
            )?);
        }
    }
    Ok(output.text)
}

pub(super) fn review_changes(
    screen: &mut ui::Screen,
    permissions: permissions::Mode,
    action: &Value,
    directory: &Path,
    items: Vec<changes::Change>,
    undo: &mut Option<changes::Batch>,
) -> Result<String> {
    let destructive = items.iter().any(|change| {
        matches!(
            change,
            changes::Change::Delete { .. } | changes::Change::Move { .. }
        )
    });
    let batch = changes::Batch::prepare(directory, items)?;
    let reason = field(&action["edit"], "reason");
    if (permissions == permissions::Mode::Ask
        || destructive && permissions != permissions::Mode::Full)
        && !screen.review(
            tr!("Изменения проекта", "Project changes"),
            &format!("{reason}\n{}", batch.preview()),
        )?
    {
        return Ok(tr!("Изменения отменены.", "Changes canceled.").into());
    }
    *undo = Some(batch.apply()?);
    Ok(tr!("Изменения применены.", "Changes applied.").into())
}
