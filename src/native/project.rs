use super::changes::Change;
use anyhow::{Result, ensure};
use std::{fs, path::Path, process::Command};

pub fn inspect(root: &Path) -> Result<String> {
    let mut lines = vec![super::list_files(root, ".")?];
    lines.push(format!(
        "Available tasks: {}",
        super::tasks::available(root).join(", ")
    ));
    for (file, stack) in [
        ("Cargo.toml", "Rust"),
        ("package.json", "Node.js"),
        ("pyproject.toml", "Python"),
        ("main.py", "Python"),
        ("index.html", "Web"),
    ] {
        if super::safe_path(root, file).is_ok_and(|path| path.is_file()) {
            lines.push(format!("{stack}: {file}"));
        }
    }
    if let Ok(content) = super::read_file(root, "AGENTS.md") {
        lines.push(format!("Project instructions (AGENTS.md):\n{content}"));
    }
    Ok(lines.join("\n"))
}

fn matches(pattern: &str, text: &str) -> bool {
    let mut previous = vec![false; text.len() + 1];
    previous[0] = true;
    let mut chars = pattern.bytes().peekable();
    while let Some(token) = chars.next() {
        let recursive = token == b'*' && chars.next_if_eq(&b'*').is_some();
        let mut next = vec![false; text.len() + 1];
        if token == b'*' {
            next[0] = previous[0];
        }
        for (index, byte) in text.bytes().enumerate() {
            next[index + 1] = match token {
                b'*' => previous[index + 1] || ((recursive || byte != b'/') && next[index]),
                b'?' => byte != b'/' && previous[index],
                _ => token == byte && previous[index],
            };
        }
        previous = next;
    }
    previous[text.len()]
}
pub fn search(root: &Path, folder: &str, pattern: &str, filter: Option<&str>) -> Result<String> {
    ensure!(
        !pattern.is_empty() && pattern.len() <= 256,
        "Expected a pattern up to 256 bytes"
    );
    let folder = super::safe_path(root, if folder.is_empty() { "." } else { folder })?;
    let root = root.canonicalize()?;
    let mut dirs = vec![folder];
    let mut visited = 0usize;
    let mut output = Vec::new();
    'walk: while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if visited == 2000 || output.len() >= 50 {
                output.push("[search truncated]".into());
                break 'walk;
            }
            visited += 1;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if super::permissions::protected(Path::new(name.as_ref()))
                || name.starts_with('.')
                || matches!(
                    name.as_ref(),
                    "node_modules" | "target" | "dist" | "profile" | "auth.json"
                )
            {
                continue;
            }
            let meta = fs::symlink_metadata(entry.path())?;
            if crate::safe_files::is_link(&meta) {
                continue;
            }
            if meta.is_dir() {
                dirs.push(entry.path());
                continue;
            }
            if !meta.is_file() {
                continue;
            }
            let path = entry.path();
            let rel = path.strip_prefix(&root)?.to_string_lossy();
            let portable = rel.replace('\\', "/");
            let matching = |p: &str| {
                matches(p, &portable)
                    || (!p.contains('/') && matches(p, &name))
                    || p.strip_prefix("**/").is_some_and(|p| matches(p, &portable))
            };
            if let Some(glob) = filter {
                if !matching(glob) {
                    continue;
                }
                if let Ok(content) = super::read_file(&root, &rel) {
                    for (line, value) in content.lines().enumerate() {
                        if value.contains(pattern) {
                            output.push(format!(
                                "{portable}:{} {}",
                                line + 1,
                                value.chars().take(240).collect::<String>()
                            ));
                            if output.len() == 50 {
                                break;
                            }
                        }
                    }
                }
            } else if matching(pattern) {
                output.push(portable);
            }
        }
    }
    if output.is_empty() {
        output.push("No matches".into());
    }
    Ok(output.join("\n"))
}

pub fn git(root: &Path, diff: bool, staged: bool) -> Result<String> {
    let repo = root.ancestors().any(|dir| {
        std::fs::symlink_metadata(dir.join(".git")).is_ok_and(|info| {
            !crate::safe_files::is_link(&info) && (info.is_dir() || info.is_file())
        })
    });
    if !repo {
        return Ok(tr!(
            "В этой папке нет Git-репозитория.",
            "This folder is outside a Git repository."
        )
        .into());
    }
    let mut command = Command::new("git");
    command
        .current_dir(root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(["--no-pager", "-c", "core.fsmonitor=false"]);
    if diff {
        command.args(["diff", "--no-ext-diff", "--no-textconv", "--no-color"]);
        if staged {
            command.arg("--cached");
        }
        command.args([
            "--",
            ".",
            ":(exclude,glob)**/.env*",
            ":(exclude,glob)**/auth.json",
            ":(exclude,glob)**/profile/**",
        ]);
    } else {
        command.args(["status", "--porcelain=v1", "--untracked-files=normal"]);
    }
    super::tasks::execute(command, root, 10)
}

pub fn starter(kind: usize) -> Vec<Change> {
    let files: &[(&str, &str)] = match kind {
        0 => &[
            (
                "index.html",
                "<!doctype html>\n<html lang=\"en\"><meta charset=\"utf-8\"><title>Counter</title><link rel=\"stylesheet\" href=\"style.css\"><main><h1>Counter</h1><output id=\"count\">0</output> <button id=\"add\">Add one</button></main><script type=\"module\" src=\"app.mjs\"></script></html>\n",
            ),
            (
                "style.css",
                "body { font: 1.125rem system-ui; max-width: 42rem; margin: 3rem auto; padding: 1rem; }\n",
            ),
            (
                "app.mjs",
                "import { increment } from './counter.mjs';\nconst count = document.querySelector('#count');\ndocument.querySelector('#add').addEventListener('click', () => { count.value = increment(Number(count.value)); });\n",
            ),
            (
                "counter.mjs",
                "export const increment = (number) => number + 1;\n",
            ),
            (
                "counter.test.mjs",
                "import { test } from 'node:test';\nimport assert from 'node:assert/strict';\nimport { increment } from './counter.mjs';\ntest('increments the count', () => assert.equal(increment(0), 1));\n",
            ),
            (
                "README.md",
                "# Web counter\n\nRun `node --test`. Serve the folder over HTTP and open `index.html`.\n",
            ),
        ],
        1 => &[
            (
                "main.py",
                "def increment(number):\n    return number + 1\n\n\nif __name__ == '__main__':\n    print(increment(0))\n",
            ),
            (
                "test_main.py",
                "import unittest\nfrom main import increment\n\n\nclass CounterTest(unittest.TestCase):\n    def test_increments_the_count(self):\n        self.assertEqual(increment(0), 1)\n",
            ),
            (
                "README.md",
                "# Python counter\n\nRun `python main.py` and `python -m unittest discover`.\n",
            ),
        ],
        2 => &[
            (
                "Cargo.toml",
                "[package]\nname = \"karrycode-project\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            (
                "Cargo.lock",
                "version = 3\n\n[[package]]\nname = \"karrycode-project\"\nversion = \"0.1.0\"\n",
            ),
            (
                "src/main.rs",
                "fn increment(number: u32) -> u32 {\n    number + 1\n}\n\nfn main() {\n    println!(\"{}\", increment(0));\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn increments_the_count() {\n        assert_eq!(super::increment(0), 1);\n    }\n}\n",
            ),
            (
                "README.md",
                "# Rust counter\n\nRun `cargo run --locked` and `cargo test --locked`.\n",
            ),
        ],
        _ => &[],
    };
    files
        .iter()
        .map(|(path, content)| Change::Create {
            path: (*path).into(),
            content: (*content).into(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires Node.js, Python and Cargo on PATH"]
    fn starters_pass_their_real_test_commands() -> Result<()> {
        for (kind, program, arguments) in [
            (0, "node", &["--test"][..]),
            (
                1,
                if cfg!(windows) { "python" } else { "python3" },
                &["-m", "unittest", "discover"][..],
            ),
            (2, "cargo", &["test", "--locked", "--offline"][..]),
        ] {
            let root = tempfile::tempdir()?;
            super::super::changes::Batch::prepare(root.path(), starter(kind))?.apply()?;
            let result = Command::new(program)
                .args(arguments)
                .current_dir(root.path())
                .env_remove("CARGO_TARGET_DIR")
                .output()?;
            ensure!(
                result.status.success(),
                "{program}: {}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
        }
        Ok(())
    }
}
