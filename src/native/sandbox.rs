use super::{changes::Change, permissions, tasks};
#[cfg(unix)]
use anyhow::Context;
use anyhow::{Result, ensure};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::AtomicBool,
};

#[cfg(windows)]
mod windows;

type Snapshot = BTreeMap<PathBuf, String>;

pub(super) struct Workspace {
    temp: tempfile::TempDir,
    before: Snapshot,
}

impl Workspace {
    pub fn prepare(root: &Path) -> Result<Self> {
        let temp = crate::platform::private_dir()?;
        let work = temp.path().join("work");
        fs::create_dir(&work)?;
        let before = snapshot(root, Some(&work))?;
        Ok(Self { temp, before })
    }

    pub fn execute(
        &self,
        relative: &Path,
        command: &Command,
        timeout: u64,
        cancel: &AtomicBool,
    ) -> Result<tasks::Output> {
        #[cfg(windows)]
        return windows::execute(self.temp.path(), relative, command, timeout, cancel);
        #[cfg(unix)]
        {
            let mut shell = linux_command(self.temp.path(), relative)?;
            shell.arg(command.get_program()).args(command.get_args());
            tasks::execute_cancellable(shell, self.temp.path(), timeout, cancel)
        }
    }

    pub fn probe(&self, relative: &Path, command: &Command) -> Result<()> {
        #[cfg(windows)]
        return windows::probe(self.temp.path(), relative, command);
        #[cfg(unix)]
        {
            let _ = command;
            let mut probe = linux_command(self.temp.path(), relative)?;
            probe.arg("/bin/true");
            let result =
                tasks::execute_cancellable(probe, self.temp.path(), 3, &AtomicBool::new(false))?;
            ensure!(
                result.success,
                "Bubblewrap isolation is unavailable: {}",
                result.text
            );
            Ok(())
        }
    }

    pub fn changes(&self) -> Result<Vec<Change>> {
        let work = self.temp.path().join("work");
        let after = snapshot(&work, None)?;
        let mut changes = Vec::new();
        for (path, before) in &self.before {
            let updated = after.get(path);
            if updated == Some(before) || updated.is_none() && work.join(path).exists() {
                continue;
            }
            changes.push(Change::Replace {
                path: path.clone(),
                before: Some(before.clone()),
                after: updated.cloned(),
            });
        }
        for (path, content) in after {
            if !self.before.contains_key(&path) {
                changes.push(Change::Create { path, content });
            }
        }
        Ok(changes)
    }
}

fn snapshot(root: &Path, copy_to: Option<&Path>) -> Result<Snapshot> {
    let mut text = Snapshot::new();
    let mut pending = vec![PathBuf::new()];
    let mut total = 0u64;
    let mut entries = 0usize;
    while let Some(relative) = pending.pop() {
        for result in fs::read_dir(root.join(&relative))? {
            let entry = result?;
            let path = relative.join(entry.file_name());
            let metadata = fs::symlink_metadata(entry.path())?;
            if permissions::protected(&path)
                || crate::safe_files::is_link(&metadata)
                || path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        matches!(
                            name,
                            ".karrycode-tmp"
                                | ".karrycode-runtime"
                                | "target"
                                | "node_modules"
                                | ".venv"
                                | "__pycache__"
                                | ".pytest_cache"
                        )
                    })
            {
                continue;
            }
            entries += 1;
            ensure!(
                entries <= 8_000,
                "Project has too many files for a temporary sandbox"
            );
            if metadata.is_dir() {
                if let Some(copy) = copy_to {
                    fs::create_dir(copy.join(&path))?;
                }
                pending.push(path);
            } else if metadata.is_file() {
                total = total.saturating_add(metadata.len());
                ensure!(
                    total <= 128 * 1024 * 1024,
                    "Project exceeds the temporary sandbox size limit"
                );
                if let Some(copy) = copy_to {
                    fs::copy(entry.path(), copy.join(&path))?;
                }
                if metadata.len() <= 1024 * 1024
                    && let Ok(value) = fs::read_to_string(
                        copy_to.map_or_else(|| entry.path(), |copy| copy.join(&path)),
                    )
                    && !value.contains('\0')
                {
                    text.insert(path, value);
                }
            }
        }
    }
    Ok(text)
}

#[cfg(unix)]
fn linux_command(temp: &Path, relative: &Path) -> Result<Command> {
    let executable = ["/usr/bin/bwrap", "/bin/bwrap"]
        .into_iter()
        .find(|path| Path::new(path).is_file())
        .context(tr!(
            "Bubblewrap не установлен",
            "Bubblewrap is not installed"
        ))?;
    let mut command = Command::new(executable);
    command.args([
        "--unshare-all",
        "--unshare-user",
        "--disable-userns",
        "--new-session",
        "--die-with-parent",
        "--clearenv",
    ]);
    for path in ["/usr", "/bin", "/lib", "/lib64", "/sbin"] {
        if Path::new(path).exists() {
            command.args(["--ro-bind", path, path]);
        }
    }
    command
        .args([
            "--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp", "--bind",
        ])
        .arg(temp.join("work"))
        .args(["/work", "--chdir"])
        .arg(Path::new("/work").join(relative))
        .args([
            "--setenv",
            "PATH",
            "/usr/bin:/bin",
            "--setenv",
            "HOME",
            "/tmp",
            "--setenv",
            "PYTHONIOENCODING",
            "utf-8",
            "--setenv",
            "LANG",
            "C.UTF-8",
            "--",
        ]);
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staged_edits_hide_credentials_and_abort_on_parallel_changes() -> Result<()> {
        let root = tempfile::tempdir()?;
        let temp = tempfile::tempdir()?;
        let stage = temp.path().join("work");
        fs::create_dir(&stage)?;
        fs::write(root.path().join("notes.txt"), "before")?;
        fs::write(root.path().join(".env"), "synthetic")?;
        let before = snapshot(root.path(), Some(&stage))?;
        assert!(!stage.join(".env").exists());
        fs::write(stage.join("notes.txt"), "changed")?;
        let workspace = Workspace { temp, before };
        fs::write(root.path().join("notes.txt"), "parallel change")?;
        assert!(super::super::changes::Batch::prepare(root.path(), workspace.changes()?).is_err());
        Ok(())
    }

    #[test]
    #[ignore = "requires the platform sandbox and a standard user"]
    fn platform_sandbox_can_write_to_stage_and_cannot_read_the_host_fixture() -> Result<()> {
        let root = tempfile::tempdir()?;
        let outside = root.path().join(".env");
        fs::write(&outside, "synthetic")?;
        let workspace = Workspace::prepare(root.path())?;
        #[cfg(windows)]
        let command = {
            let system = std::env::var_os("SystemRoot")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
            let mut command = Command::new(system.join("System32").join("cmd.exe"));
            command.args(["/d", "/c", "echo ok>safe.txt"]);
            command
        };
        #[cfg(unix)]
        let (command, _) = tasks::shell(&format!(
            "if test -r '{}'; then exit 17; fi; printf ok > safe.txt",
            outside.display().to_string().replace('\'', "'\"'\"'"),
        ))?;
        workspace.probe(Path::new(""), &command)?;
        let output = workspace.execute(Path::new(""), &command, 8, &AtomicBool::new(false))?;
        assert!(output.success, "{}", output.text);
        assert_eq!(
            fs::read_to_string(workspace.temp.path().join("work/safe.txt"))?.trim(),
            "ok"
        );
        #[cfg(windows)]
        {
            let system = std::env::var_os("SystemRoot")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
            let mut compare = Command::new(system.join("System32").join("fc.exe"));
            compare.arg("/b").arg("safe.txt").arg(&outside);
            let output = workspace.execute(Path::new(""), &compare, 8, &AtomicBool::new(false))?;
            assert!(output.text.contains("exit code: 2"), "{}", output.text);
        }
        assert!(!root.path().join("safe.txt").exists());
        Ok(())
    }
}
