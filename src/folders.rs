mod picker;

use crate::recent::RecentFolders;
use anyhow::{Context, Result, ensure};
use std::{
    ffi::{OsStr, OsString},
    fs, io,
    path::{Component, Path, PathBuf},
};

pub fn pick(start: &Path, root: &Path, recent: &RecentFolders) -> Result<Option<PathBuf>> {
    // A minimal path prompt keeps selection usable in terminals without screen support.
    if !cfg!(windows) && std::env::var_os("TERM").as_deref() == Some(OsStr::new("dumb")) {
        return pick_by_path(start);
    }
    let Ok(screen) = picker::Screen::open() else {
        return pick_by_path(start);
    };
    picker::run(screen, start, root, recent)
}

pub fn resolve_directory(start: &Path, input: &str) -> Result<PathBuf> {
    let path = accessible(
        &start.join(
            input
                .strip_prefix('"')
                .and_then(|path| path.strip_suffix('"'))
                .unwrap_or(input),
        ),
    )?;
    selectable(&path)?;
    Ok(path)
}

fn pick_by_path(start: &Path) -> Result<Option<PathBuf>> {
    println!("{}", display(start));
    let Some(input) = crate::prompt(tr!(
        "Путь к папке [Enter — текущая, 0 — назад]: ",
        "Folder path [Enter for current, 0 to go back]: "
    ))?
    else {
        return Ok(None);
    };
    if input == "0" {
        return Ok(None);
    }
    let path = accessible(&start.join(input.trim_matches('"')))?;
    selectable(&path)?;
    Ok(Some(path))
}

fn list(path: &Path, show_hidden: bool) -> io::Result<Vec<OsString>> {
    let mut folders = Vec::new();
    for entry in fs::read_dir(path)?.filter_map(Result::ok) {
        let name = entry.file_name();
        let hidden = name.to_string_lossy().starts_with('.');
        if show_hidden || !hidden {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() || (kind.is_symlink() && entry.path().is_dir()) {
                folders.push(name);
            }
        }
    }
    folders.sort_by_cached_key(|folder| folder.to_string_lossy().to_lowercase());
    Ok(folders)
}

fn accessible(path: &Path) -> Result<PathBuf> {
    let result = path
        .canonicalize()
        .with_context(|| trf!("Не удалось открыть {}", "Could not open {}", display(path)))?;
    fs::read_dir(&result).with_context(|| {
        trf!(
            "Не удалось прочитать {}",
            "Could not read {}",
            display(path)
        )
    })?;
    Ok(result)
}

fn selectable(path: &Path) -> Result<()> {
    ensure!(
        path.parent().is_some(),
        "{}",
        tr!(
            "Выбери папку внутри диска.",
            "Choose a folder inside the drive."
        )
    );
    Ok(())
}

fn new_folder_path(parent: &Path, input: &str) -> Result<PathBuf> {
    let name = input.trim();
    // A pasted `\name` in this dialog means one child, not a drive-root path.
    // Strip at most one separator: UNC and nested paths must still be rejected.
    #[cfg(windows)]
    let name = name.strip_prefix(['\\', '/']).unwrap_or(name);
    let mut parts = Path::new(name).components();
    ensure!(
        !name.contains(['/', '\\', ':'])
            && matches!(parts.next(), Some(Component::Normal(_)))
            && parts.next().is_none(),
        "{}",
        tr!("Введи имя одной папки", "Enter a single folder name")
    );
    #[cfg(windows)]
    {
        let stem = name
            .split('.')
            .next()
            .unwrap_or_default()
            .trim_end_matches(' ')
            .to_ascii_uppercase();
        let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || stem
                .strip_prefix("COM")
                .or_else(|| stem.strip_prefix("LPT"))
                .is_some_and(|suffix| {
                    matches!(
                        suffix,
                        "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                    )
                });
        ensure!(
            !device
                && !name.ends_with('.')
                && !name.contains(['<', '>', '"', '|', '?', '*'])
                && !name.chars().any(char::is_control),
            "{}",
            tr!(
                "Windows не поддерживает такое имя папки.",
                "Windows does not support this folder name."
            )
        );
    }
    Ok(parent.join(name))
}

fn create_folder(parent: &Path, name: &str) -> Result<PathBuf> {
    let path = new_folder_path(parent, name)?;
    match fs::create_dir(&path) {
        Ok(()) => Ok(path),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => anyhow::bail!(
            "{}",
            tr!(
                "Это имя уже занято. Введи другое.",
                "This name is already taken. Enter another."
            )
        ),
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => Err(error).context(tr!(
            "Нет прав на создание папки в этом месте",
            "No permission to create a folder here"
        )),
        Err(error) => Err(error).context(tr!(
            "Не удалось создать папку",
            "Could not create the folder"
        )),
    }
}

fn visible(value: &OsStr) -> String {
    value
        .to_string_lossy()
        .chars()
        .map(|character| {
            if character.is_control() {
                '�'
            } else {
                character
            }
        })
        .collect()
}

pub fn display(path: &Path) -> String {
    let value = visible(path.as_os_str());
    #[cfg(windows)]
    {
        if let Some(unc) = value.strip_prefix(r"\\?\UNC\") {
            return format!(r"\\{unc}");
        }
        if let Some(disk) = value.strip_prefix(r"\\?\")
            && disk.as_bytes().get(1) == Some(&b':')
        {
            return disk.to_owned();
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_folder_matches_preview_and_keeps_existing_entries() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = new_folder_path(root.path(), "Новый проект")?;
        assert!(!path.exists());
        assert_eq!(create_folder(root.path(), "Новый проект")?, path);
        assert!(path.is_dir());
        assert!(create_folder(root.path(), "Новый проект").is_err());
        fs::write(root.path().join("occupied"), b"data")?;
        assert!(create_folder(root.path(), "occupied").is_err());
        assert_eq!(fs::read(root.path().join("occupied"))?, b"data");
        for name in ["", " ", ".", "..", "../outside", "a/b", "a\\b", "D:folder"] {
            assert!(new_folder_path(root.path(), name).is_err(), "{name:?}");
        }
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn windows_single_leading_separator_creates_a_child_not_a_root_path() -> Result<()> {
        let root = tempfile::tempdir()?;
        let expected = root.path().join("Test");
        assert_eq!(new_folder_path(root.path(), "/Test")?, expected);
        assert_eq!(create_folder(root.path(), r"\Test")?, expected);
        assert!(expected.is_dir());
        for name in [
            r"\\server\share",
            r"\..",
            r"\parent\child",
            r"C:\Test",
            "NUL.txt",
            "COM¹",
            "name.",
            "bad*",
        ] {
            assert!(new_folder_path(root.path(), name).is_err(), "{name:?}");
        }
        Ok(())
    }
}
