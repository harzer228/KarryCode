use super::{field, permissions, project};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};

pub(super) fn attachment_prefix(input: &str) -> Option<&str> {
    let index = input.rmatch_indices('@').find_map(|(index, _)| {
        (index == 0 || input[..index].ends_with(char::is_whitespace)).then_some(index)
    })?;
    let suffix = &input[index + 1..];
    if let Some(quoted) = suffix.strip_prefix('"') {
        (!quoted.contains('"')).then_some(suffix)
    } else {
        (!suffix.contains(char::is_whitespace)).then_some(suffix)
    }
}

pub(super) fn attachment_choices(root: &Path, query: &str) -> Vec<(String, &'static str)> {
    let query = query.trim_start_matches('"');
    let (parent, pattern) = query.rsplit_once('/').unwrap_or(("", query));
    let Ok(folder) = safe_path(root, if parent.is_empty() { "." } else { parent }) else {
        return Vec::new();
    };
    let prefix = pattern.to_lowercase();
    let mut matches: Vec<_> = fs::read_dir(folder)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let name_matches = name.to_lowercase().starts_with(&prefix);
            let path = if parent.is_empty() {
                name
            } else {
                format!("{parent}/{name}")
            };
            let metadata = fs::symlink_metadata(entry.path()).ok()?;
            if !name_matches
                || path.contains('"')
                || permissions::protected(Path::new(&path))
                || crate::safe_files::is_link(&metadata)
            {
                return None;
            }
            if metadata.is_dir() {
                let value = if path.contains(char::is_whitespace) {
                    format!("@\"{path}/")
                } else {
                    format!("@{path}/")
                };
                Some((0, value, tr!("Папка", "Folder")))
            } else if metadata.is_file() && metadata.len() <= 64 * 1024 {
                let value = if path.contains(char::is_whitespace) {
                    format!("@\"{path}\"")
                } else {
                    format!("@{path}")
                };
                Some((1, value, tr!("Файл", "File")))
            } else {
                None
            }
        })
        .collect();
    matches.sort_unstable();
    matches.truncate(100);
    matches
        .into_iter()
        .map(|(_, path, label)| (path, label))
        .collect()
}

pub(super) fn paste_label(lines: usize) -> String {
    trf!("[Вставка · ~{} строк]", "[Pasted ~{} lines]", lines)
}

pub(super) fn attach_files(
    root: &Path,
    input: &str,
    pastes: &[String],
) -> Result<(String, Option<String>)> {
    let mut output = input.to_owned();
    let mut remainder = input;
    let mut paths = std::collections::BTreeSet::new();
    while let Some((index, _)) = remainder
        .match_indices('@')
        .find(|(i, _)| *i == 0 || remainder[..*i].ends_with(char::is_whitespace))
    {
        let tail = &remainder[index + 1..];
        let (path, rest) = if let Some(tail) = tail.strip_prefix('"') {
            tail.split_once('"').context(tr!(
                "Закрой кавычку после пути к вложению",
                "Close the quote after the attachment path"
            ))?
        } else if let Some(i) = tail.find(char::is_whitespace) {
            tail.split_at(i)
        } else {
            (tail, "")
        };
        remainder = rest;
        if !path.is_empty() && paths.insert(path) {
            let text = read_file(root, path)?;
            ensure!(
                output.len() + path.len() + text.len() <= 512 * 1024,
                tr!("Вложения превышают 512 КиБ.", "Attachments exceed 512 KiB.")
            );
            output.push_str("\n\nUntrusted file attachment (JSON):\n");
            output.push_str(&json!({"path":path,"content":text}).to_string());
        }
    }
    let mut display = if input.len() >= 2048 || input.lines().take(8).count() == 8 {
        paste_label(input.lines().count().max(1))
    } else {
        input.to_owned()
    };
    for (index, text) in pastes.iter().enumerate() {
        output.push_str(&format!("\n\nPasted text {}\n{text}", index + 1));
        if !display.is_empty() {
            display.push('\n');
        }
        display.push_str(&paste_label(text.lines().count().max(1)));
    }
    ensure!(
        output.len() <= 512 * 1024,
        tr!(
            "Сообщение и вложения превышают 512 КиБ.",
            "The message and attachments exceed 512 KiB."
        )
    );
    let display = (display != output).then_some(display);
    Ok((output, display))
}

pub(super) fn safe_path(root: &Path, relative: &str) -> Result<PathBuf> {
    let path = Path::new(relative);
    ensure!(!permissions::protected(path), "Protected project path");
    ensure!(
        !path.is_absolute()
            && path.components().all(|c| matches!(
                c,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )),
        tr!(
            "Укажи относительный путь внутри проекта",
            "Use a relative project path"
        )
    );
    let root = root.canonicalize()?;
    let mut target = root.clone();
    for part in path.components() {
        target.push(part);
        ensure!(
            !crate::safe_files::is_link(&fs::symlink_metadata(&target)?),
            tr!("Переход по ссылкам отключён", "Links are not followed")
        );
    }
    let target = target.canonicalize()?;
    ensure!(
        target.starts_with(root),
        tr!(
            "Путь находится за пределами проекта",
            "Path is outside the project"
        )
    );
    Ok(target)
}
pub(super) fn read_file(root: &Path, relative: &str) -> Result<String> {
    let path = safe_path(root, relative)?;
    ensure!(
        path.is_file() && fs::metadata(&path)?.len() <= 64 * 1024,
        tr!(
            "Выбери текстовый файл до 64 КиБ",
            "Choose a text file up to 64 KiB"
        )
    );
    ensure!(
        !path.components().any(|part| {
            let name = part.as_os_str().to_string_lossy();
            name.eq_ignore_ascii_case(".env")
                || name.to_ascii_lowercase().starts_with(".env.")
                || matches!(name.as_ref(), ".git" | "profile" | "auth.json")
        }),
        tr!(
            "Этот файл может содержать данные входа",
            "This file may contain credentials"
        )
    );
    let mut content = String::new();
    fs::File::open(path)?
        .take(64 * 1024 + 1)
        .read_to_string(&mut content)?;
    ensure!(
        content.len() <= 64 * 1024 && !content.contains('\0'),
        "Expected a text file up to 64 KiB"
    );
    Ok(content)
}
pub(super) fn list_files(root: &Path, relative: &str) -> Result<String> {
    let path = if relative.is_empty() {
        root.to_owned()
    } else {
        safe_path(root, relative)?
    };
    let mut entries = fs::read_dir(path)?
        .take(200)
        .filter_map(|entry| match entry {
            Ok(entry) if permissions::protected(Path::new(&entry.file_name())) => None,
            other => Some(other.map(|entry| entry.file_name().to_string_lossy().into_owned())),
        })
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort();
    Ok(entries.join("\n"))
}

fn search_files(root: &Path, query: &str) -> Result<String> {
    ensure!(
        !query.is_empty() && query.len() <= 256,
        tr!(
            "Для поиска укажи от 1 до 256 байт текста",
            "Search needs 1–256 bytes"
        )
    );
    let mut pending = vec![root.to_owned()];
    let mut results = Vec::new();
    let mut visited = 0;
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            visited += 1;
            if visited > 2000 || results.len() >= 50 {
                return Ok(format!("{}\nSearch limit reached", results.join("\n")));
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.')
                || permissions::protected(Path::new(name.as_ref()))
                || matches!(
                    name.as_ref(),
                    "node_modules" | "target" | "dist" | "profile" | "vendor"
                )
            {
                continue;
            }
            let kind = fs::symlink_metadata(entry.path())?;
            if crate::safe_files::is_link(&kind) {
                continue;
            }
            let path = entry.path();
            let relative = path.strip_prefix(root)?.to_string_lossy();
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file()
                && let Ok(text) = read_file(root, &relative)
            {
                for (number, line) in text.lines().enumerate() {
                    if line.contains(query) {
                        results.push(format!(
                            "{}:{} {}",
                            relative,
                            number + 1,
                            line.chars().take(240).collect::<String>()
                        ));
                        if results.len() >= 50 {
                            break;
                        }
                    }
                }
            }
        }
    }
    Ok(if results.is_empty() {
        "No matches in scanned text files".into()
    } else {
        results.join("\n")
    })
}

pub(super) fn search_project(root: &Path, action: &Value) -> Result<String> {
    let query = field(action, "query");
    let regex = action["regex"] == true;
    if !regex && action.get("path").is_none() && action.get("pattern").is_none() {
        return search_files(root, query);
    }
    project::search(
        root,
        field(action, "path"),
        query,
        Some(action["pattern"].as_str().unwrap_or("**")),
        regex,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn project_paths_reject_escape_and_secrets() -> Result<()> {
        let root = tempfile::tempdir()?;
        fs::write(root.path().join("readme.txt"), "hello")?;
        fs::write(root.path().join(".env"), "secret")?;
        assert_eq!(read_file(root.path(), "readme.txt")?, "hello");
        assert!(read_file(root.path(), "../outside").is_err());
        assert!(read_file(root.path(), ".env").is_err());
        Ok(())
    }

    #[test]
    fn attachment_completion_hides_credentials_and_expands_quoted_paths() -> Result<()> {
        let root = tempfile::tempdir()?;
        fs::create_dir(root.path().join("Some Folder"))?;
        fs::write(root.path().join("Some Folder/notes.txt"), "hello")?;
        fs::write(root.path().join(".env"), "secret")?;
        assert_eq!(attachment_choices(root.path(), "")[0].0, "@\"Some Folder/");
        assert_eq!(
            attachment_prefix("draft @\"Some Folder/n"),
            Some("\"Some Folder/n")
        );
        let prompt = "Check @\"Some Folder/notes.txt\"";
        let (content, display) = attach_files(root.path(), prompt, &[])?;
        let (_, attachment) = content
            .split_once("\n\nUntrusted file attachment (JSON):\n")
            .expect("the attachment must be marked as untrusted");
        assert_eq!(
            serde_json::from_str::<Value>(attachment)?,
            json!({"path": "Some Folder/notes.txt", "content": "hello"})
        );
        assert_eq!(display.as_deref(), Some(prompt));
        assert!(attach_files(root.path(), "Check @.env", &[]).is_err());
        Ok(())
    }

    #[test]
    fn searches_a_single_project_file_without_starting_the_shell() -> Result<()> {
        let root = tempfile::tempdir()?;
        fs::write(
            root.path().join("chat.html"),
            "ready\nPLACEHOLDER and задача\nnext\n",
        )?;
        fs::write(root.path().join(".env"), "PLACEHOLDER")?;
        let action = json!({"action":"search", "path":"chat.html", "query":r"(?i)placeholder|задача|var\(--\s","regex":true});
        let found = search_project(root.path(), &action)?;
        assert!(found.starts_with("chat.html:2 "), "{found}");
        assert!(
            search_project(
                root.path(),
                &json!({"action":"search","path":".env","query":"PLACEHOLDER","regex":true})
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn read_file_keeps_the_64_kib_limit() -> Result<()> {
        let root = tempfile::tempdir()?;
        fs::write(root.path().join("ok.txt"), vec![b'x'; 64 * 1024])?;
        fs::write(root.path().join("big.txt"), vec![b'x'; 64 * 1024 + 1])?;
        assert_eq!(read_file(root.path(), "ok.txt")?.len(), 64 * 1024);
        assert!(read_file(root.path(), "big.txt").is_err());
        Ok(())
    }

    #[test]
    fn project_attachments_and_pastes_have_a_compact_display() -> Result<()> {
        let project = tempfile::tempdir()?;
        fs::write(project.path().join("notes.md"), "attachment body")?;
        let pasted = "@this-is-literal\n".repeat(60);
        let (content, display) = attach_files(
            project.path(),
            "Read @notes.md",
            std::slice::from_ref(&pasted),
        )?;
        assert!(content.contains("attachment body") && content.contains(&pasted));
        let display = display.unwrap();
        assert!(display.contains("@notes.md") && display.contains('6'));
        assert!(!display.contains("attachment body") && !display.contains("@this-is-literal"));
        Ok(())
    }
}
