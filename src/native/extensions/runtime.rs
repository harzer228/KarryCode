use super::super::{sandbox, tasks};
use anyhow::{Context, Result, ensure};
use rhai::{
    Engine as Rhai, Scope,
    packages::{Package as _, StandardPackage},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    io::{Read as _, Write as _},
    path::Path,
    process::Command,
    sync::atomic::{AtomicBool, Ordering},
};

const MAX_PAYLOAD: u64 = 2 * 1024 * 1024;
pub(crate) const WORKER: &str = "--karrycode-extension-worker";

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Engine {
    Rhai,
    Python,
    Powershell,
    Bash,
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Proposed {
    pub path: String,
    pub content: String,
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Output {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub changes: Vec<Proposed>,
}

#[derive(Serialize)]
struct Input<'a> {
    arguments: &'a Value,
    files: &'a BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct OwnedInput {
    script: String,
    arguments: Value,
    files: BTreeMap<String, String>,
}

#[derive(Serialize)]
struct WorkerInput<'a> {
    script: &'a str,
    #[serde(flatten)]
    input: Input<'a>,
}

pub(super) fn execute(
    engine: Engine,
    script: &str,
    arguments: &Value,
    files: &BTreeMap<String, String>,
    timeout: u64,
    cancel: &AtomicBool,
) -> Result<Output> {
    ensure!(
        !cancel.load(Ordering::Relaxed),
        "Extension canceled before execution"
    );
    availability(engine)?;
    let input = Input { arguments, files };
    let response = if matches!(engine, Engine::Rhai) {
        let work = crate::platform::private_dir()?;
        let mut command = Command::new(std::env::current_exe()?);
        command.arg(WORKER).env_clear();
        #[cfg(windows)]
        if let Some(system) = std::env::var_os("SystemRoot") {
            command.env("SystemRoot", system);
        }
        let payload = serde_json::to_vec(&WorkerInput { script, input })?;
        ensure!(
            payload.len() as u64 <= MAX_PAYLOAD,
            "Extension input is larger than 2 MiB"
        );
        tasks::execute_payload(command, work.path(), timeout, cancel, &payload)?
    } else {
        let (command, name) = interpreter(engine)?;
        let workspace = sandbox::Workspace::extension(script, name, files)?;
        let payload = serde_json::to_vec(&input)?;
        ensure!(
            payload.len() as u64 <= MAX_PAYLOAD,
            "Extension input is larger than 2 MiB"
        );
        workspace
            .execute_extension(&command, timeout, cancel, &payload)
            .map_err(|error| anyhow::anyhow!("Extension runtime requires an available interpreter and working isolation: {error:#}"))?
    };
    ensure!(
        response.success,
        "Extension execution failed.\n{}",
        response.text
    );
    let output: Output = serde_json::from_str(&response.stdout).context(
        "Extension must return a JSON object with text and optional changes (up to 32 KiB)",
    )?;
    ensure!(
        output.changes.len() <= 12,
        "Extension proposed more than 12 changes"
    );
    Ok(output)
}

fn interpreter(engine: Engine) -> Result<(Command, &'static str)> {
    Ok(match engine {
        Engine::Python => {
            let name = ".karrycode-tool.py";
            let mut command = Command::new(if cfg!(windows) {
                "python"
            } else {
                "/usr/bin/python3"
            });
            command.args(["-I", "-X", "utf8", name]);
            (command, name)
        }
        Engine::Powershell => {
            let name = ".karrycode-tool.ps1";
            let command = if cfg!(windows) {
                "pwsh"
            } else if Path::new("/usr/bin/pwsh").is_file() {
                "/usr/bin/pwsh"
            } else {
                "/bin/pwsh"
            };
            let mut command = Command::new(command);
            command.args(["-NoLogo", "-NoProfile", "-NonInteractive", "-File", name]);
            (command, name)
        }
        Engine::Bash => {
            let name = ".karrycode-tool.sh";
            #[cfg(windows)]
            let mut command = Command::new(native_bash()?);
            #[cfg(unix)]
            let mut command = Command::new("/bin/bash");
            command.args(["--noprofile", "--norc", name]);
            (command, name)
        }
        Engine::Rhai => unreachable!("Rhai runs in its own worker"),
    })
}

#[cfg(windows)]
fn native_bash() -> Result<std::path::PathBuf> {
    let windows = std::env::var_os("SystemRoot")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| r"C:\Windows".into())
        .canonicalize()?;
    std::env::var_os("PATH").into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .filter(|dir| dir.is_absolute())
        .filter_map(|dir| dir.join("bash.exe").canonicalize().ok())
        .find(|program| program.is_file() && !program.starts_with(&windows))
        .context("A native Windows Bash executable is required. The WSL launcher cannot run in AppContainer.")
}

pub(super) fn availability(engine: Engine) -> Result<()> {
    if matches!(engine, Engine::Rhai) {
        return Ok(());
    }
    #[cfg(unix)]
    ensure!(
        ["/usr/bin/bwrap", "/bin/bwrap"]
            .iter()
            .any(|path| Path::new(path).is_file()),
        "Bubblewrap is required for this interpreter"
    );
    let (command, _) = interpreter(engine)?;
    let program = Path::new(command.get_program());
    let found = if program.is_absolute() {
        program.is_file()
    } else {
        std::env::var_os("PATH").is_some_and(|path| {
            std::env::split_paths(&path).any(|folder| {
                folder.is_absolute() && folder.join(program.with_extension("exe")).is_file()
            })
        })
    };
    ensure!(found, "Interpreter unavailable: {}", program.display());
    Ok(())
}

fn evaluate(request: OwnedInput) -> Result<Output> {
    let mut engine = Rhai::new_raw();
    engine
        .register_global_module(StandardPackage::new().as_shared_module())
        .disable_symbol("eval")
        .set_max_operations(250_000)
        .set_max_call_levels(24)
        .set_max_expr_depths(48, 24)
        .set_max_variables(128)
        .set_max_strings_interned(128)
        .set_max_string_size(64 * 1024)
        .set_max_array_size(1_024)
        .set_max_map_size(256)
        .on_print(|_| ())
        .on_debug(|_, _, _| ());
    let mut scope = Scope::new();
    scope.push_constant_dynamic(
        "arguments",
        rhai::serde::to_dynamic(request.arguments).map_err(|error| anyhow::anyhow!("{error}"))?,
    );
    scope.push_constant_dynamic(
        "files",
        rhai::serde::to_dynamic(request.files).map_err(|error| anyhow::anyhow!("{error}"))?,
    );
    let result = engine
        .eval_with_scope::<rhai::Dynamic>(&mut scope, &request.script)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    rhai::serde::from_dynamic(&result).map_err(|error| anyhow::anyhow!("{error}"))
}

pub(crate) fn worker() -> Result<()> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(MAX_PAYLOAD + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_PAYLOAD,
        "Extension input exceeds 2 MiB"
    );
    let request = serde_json::from_slice(&bytes)?;
    let output = serde_json::to_vec(&evaluate(request)?)?;
    ensure!(output.len() <= 32 * 1024, "Extension output exceeds 32 KiB");
    std::io::stdout().lock().write_all(&output)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(script: &str) -> OwnedInput {
        OwnedInput {
            script: script.into(),
            arguments: json!({"prefix":"Read: "}),
            files: BTreeMap::from([("example.txt".into(), "sample".into())]),
        }
    }

    #[test]
    fn rhai_tool_reads_only_explicit_payload_and_has_operation_limits() -> Result<()> {
        let output = evaluate(request(
            r#"#{ text: arguments.prefix + files["example.txt"] }"#,
        ))?;
        assert_eq!(output.text, "Read: sample");
        assert!(evaluate(request("loop {}")).is_err());
        assert!(evaluate(request(r#"import "outside"; #{ text: "unreachable" }"#)).is_err());
        Ok(())
    }
}
