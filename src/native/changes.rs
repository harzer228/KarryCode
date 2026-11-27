use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use std::{
    collections::HashSet,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

const MAX_FILE: u64 = 1024 * 1024;
const MAX_CHANGES: usize = 24;

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Change {
    Create {
        path: PathBuf,
        content: String,
    },
    Edit {
        path: PathBuf,
        before: String,
        after: String,
    },
    #[serde(skip)]
    Replace {
        path: PathBuf,
        before: Option<String>,
        after: Option<String>,
    },
    Copy {
        path: PathBuf,
        to: PathBuf,
    },
    Move {
        path: PathBuf,
        to: PathBuf,
    },
    Delete {
        path: PathBuf,
    },
}

struct FileChange {
    path: PathBuf,
    before: Option<String>,
    after: Option<String>,
}

pub struct Batch {
    root: PathBuf,
    files: Vec<FileChange>,
}

fn checked(root: &Path, rel: &Path) -> Result<PathBuf> {
    ensure!(crate::identity::valid(rel), "Use a relative project path");
    ensure!(
        !super::permissions::protected(rel),
        "Protected project path"
    );
    let mut path = root.to_owned();
    for part in rel.components() {
        let name = part.as_os_str().to_string_lossy().to_ascii_lowercase();
        ensure!(
            !name.contains([':', '\\', '\0'])
                && !name.ends_with(['.', ' '])
                && !matches!(name.as_str(), ".git" | ".env" | "profile" | "auth.json")
                && !name.starts_with(".env."),
            "Protected project path"
        );
        path.push(part);
        match fs::symlink_metadata(&path) {
            Ok(meta) => ensure!(!crate::safe_files::is_link(&meta), "Links are not followed"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(path)
}

fn read(path: &Path) -> Result<Option<String>> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            ensure!(
                meta.is_file() && !crate::safe_files::is_link(&meta),
                "Expected a regular file"
            );
            let mut data = String::new();
            fs::File::open(path)?
                .take(MAX_FILE + 1)
                .read_to_string(&mut data)?;
            ensure!(
                data.len() as u64 <= MAX_FILE && !data.contains('\0'),
                "Expected UTF-8 text up to 1 MiB"
            );
            Ok(Some(data))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn snapshot_text(root: &Path, path: &Path) -> Result<Option<String>> {
    read(&checked(root, path)?)
}

pub(super) fn unified_patch(root: &Path, path: &Path, text: &str) -> Result<Change> {
    ensure!(text.len() <= MAX_FILE as usize, "Patch exceeds 1 MiB");
    let before = snapshot_text(root, path)?.context("Patch target does not exist")?;
    let patch = diffy::Patch::from_str(text).context("Expected a unified diff for one file")?;
    let after = diffy::apply(&before, &patch).context("Patch does not match the current file")?;
    Ok(Change::Replace {
        path: path.to_owned(),
        before: Some(before),
        after: Some(after),
    })
}

impl Batch {
    pub fn prepare(root: &Path, changes: Vec<Change>) -> Result<Self> {
        ensure!(
            !changes.is_empty() && changes.len() <= MAX_CHANGES,
            "Expected between 1 and 24 changes"
        );
        let root = root.canonicalize()?;
        let mut files = Vec::new();
        for change in changes {
            let item = match change {
                Change::Create { path, content } => {
                    ensure!(
                        read(&checked(&root, &path)?)?.is_none(),
                        "File already exists: {}",
                        path.display()
                    );
                    FileChange {
                        path,
                        before: None,
                        after: Some(content),
                    }
                }
                Change::Edit {
                    path,
                    before,
                    after,
                } => {
                    let original = read(&checked(&root, &path)?)?
                        .context(tr!("Файл не существует", "File does not exist"))?;
                    let (before, after) = if original.contains("\r\n") {
                        (
                            before.replace("\r\n", "\n").replace('\n', "\r\n"),
                            after.replace("\r\n", "\n").replace('\n', "\r\n"),
                        )
                    } else {
                        (before, after)
                    };
                    ensure!(
                        !before.is_empty()
                            && before != after
                            && original.match_indices(&before).count() == 1,
                        "The original excerpt must be unique and different from the replacement"
                    );
                    let updated = original.replacen(&before, &after, 1);
                    FileChange {
                        path,
                        before: Some(original),
                        after: Some(updated),
                    }
                }
                Change::Delete { path } => {
                    let original = read(&checked(&root, &path)?)?
                        .context(tr!("Файл не существует", "File does not exist"))?;
                    FileChange {
                        path,
                        before: Some(original),
                        after: None,
                    }
                }
                Change::Replace {
                    path,
                    before,
                    after,
                } => {
                    ensure!(
                        read(&checked(&root, &path)?)? == before,
                        "File changed while the command ran: {}",
                        path.display()
                    );
                    FileChange {
                        path,
                        before,
                        after,
                    }
                }
                Change::Copy { ref path, ref to } | Change::Move { ref path, ref to } => {
                    let original = read(&checked(&root, path)?)?.context(tr!(
                        "Исходный файл не существует",
                        "Source file does not exist"
                    ))?;
                    ensure!(
                        read(&checked(&root, to)?)?.is_none(),
                        "Destination already exists"
                    );
                    files.push(FileChange {
                        path: to.clone(),
                        before: None,
                        after: Some(original.clone()),
                    });
                    if matches!(change, Change::Copy { .. }) {
                        continue;
                    }
                    FileChange {
                        path: path.clone(),
                        before: Some(original),
                        after: None,
                    }
                }
            };
            files.push(item);
        }
        let mut used = HashSet::new();
        for file in &files {
            let name = file
                .path
                .to_string_lossy()
                .to_lowercase()
                .replace('\\', "/");
            ensure!(used.insert(name), "A file can only appear once in a group");
            if let Some(content) = &file.after {
                ensure!(
                    content.len() as u64 <= MAX_FILE && !content.contains('\0'),
                    "Expected UTF-8 text up to 1 MiB"
                );
            }
        }
        Ok(Self { root, files })
    }

    pub fn preview(&self) -> String {
        let mut result = String::new();
        for file in &self.files {
            use std::fmt::Write as _;
            let before = file.before.as_deref().unwrap_or("");
            let after = file.after.as_deref().unwrap_or("");
            let _ = writeln!(
                result,
                "\n{} {}",
                match (&file.before, &file.after) {
                    (None, _) => "+",
                    (_, None) => "-",
                    _ => "~",
                },
                file.path.display()
            );
            let old: Vec<_> = before.lines().collect();
            let new: Vec<_> = after.lines().collect();
            let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
            let suffix = old[prefix..]
                .iter()
                .rev()
                .zip(new[prefix..].iter().rev())
                .take_while(|(a, b)| a == b)
                .count();
            for line in &old[prefix..old.len() - suffix] {
                let _ = writeln!(result, "- {line}");
            }
            for line in &new[prefix..new.len() - suffix] {
                let _ = writeln!(result, "+ {line}");
            }
            if before != after && old == new {
                result.push_str(tr!(
                    "Изменены окончания строк.\n",
                    "Line endings changed.\n"
                ));
            }
        }
        result
    }

    pub fn apply(self) -> Result<Self> {
        self.apply_inner()
    }

    fn apply_inner(mut self) -> Result<Self> {
        for file in &self.files {
            ensure!(
                read(&checked(&self.root, &file.path)?)? == file.before,
                "File changed since preview: {}",
                file.path.display()
            );
        }
        let mut dirs = Vec::new();
        for (index, file) in self.files.iter().enumerate() {
            if let Err(error) = self.write(file, false, &mut dirs) {
                let mut failures = Vec::new();
                for done in self.files[..index].iter().rev() {
                    if let Err(error) = self.write(done, true, &mut dirs) {
                        failures.push(format!("{}: {error}", done.path.display()));
                    }
                }
                for dir in dirs.iter().rev() {
                    let _ = fs::remove_dir(dir);
                }
                if failures.is_empty() {
                    bail!(
                        "{}: {error}. Earlier changes rolled back.",
                        file.path.display()
                    );
                }
                bail!(
                    "{}: {error}. Could not roll back: {}",
                    file.path.display(),
                    failures.join("; ")
                );
            }
        }
        for file in &mut self.files {
            std::mem::swap(&mut file.before, &mut file.after);
        }
        self.files.reverse();
        Ok(self)
    }

    fn write(&self, item: &FileChange, reverse: bool, dirs: &mut Vec<PathBuf>) -> Result<()> {
        let (before, after) = if reverse {
            (&item.after, &item.before)
        } else {
            (&item.before, &item.after)
        };
        let path = checked(&self.root, &item.path)?;
        ensure!(&read(&path)? == before, "File changed since preview");
        let Some(content) = after else {
            fs::remove_file(path)?;
            return Ok(());
        };
        let parent = path.parent().context(tr!(
            "Родительская папка не найдена",
            "Missing parent directory"
        ))?;
        let mut current = self.root.clone();
        for component in parent.strip_prefix(&self.root)?.components() {
            current.push(component);
            match fs::create_dir(&current) {
                Ok(()) => dirs.push(current.clone()),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
            let meta = fs::symlink_metadata(&current)?;
            ensure!(
                meta.is_dir() && !crate::safe_files::is_link(&meta),
                "Invalid parent directory"
            );
        }
        let stage = tempfile::tempdir_in(parent)?;
        crate::platform::protect(stage.path())?;
        let draft = stage.path().join("draft");
        let mut file = fs::File::create_new(&draft)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        drop(file);
        ensure!(
            checked(&self.root, &item.path)? == path && &read(&path)? == before,
            "File changed since preview"
        );
        if before.is_some() {
            crate::safe_files::replace(&path, &draft, stage)?;
        } else {
            fs::hard_link(&draft, &path).or_else(|error| {
                // FAT and exFAT do not implement hard links. Use the same exclusive
                // operation on a temporary file that can be renamed without overwrite.
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    return Err(error);
                }
                let mut draft = tempfile::NamedTempFile::new_in(parent)?;
                draft.write_all(content.as_bytes())?;
                draft.as_file().sync_all()?;
                draft
                    .persist_noclobber(&path)
                    .map(|_| ())
                    .map_err(|error| error.error)
            })?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_batch_failure_rolls_back_prior_creates() -> Result<()> {
        let root = tempfile::tempdir()?;
        let items = ["first.txt", "blocking-item", "blocking-item/child.txt"]
            .into_iter()
            .map(|path| Change::Create {
                path: path.into(),
                content: "fixture".into(),
            })
            .collect();
        let batch = Batch::prepare(root.path(), items)?;
        assert!(batch.apply().is_err());
        assert!(!root.path().join("first.txt").exists());
        assert!(!root.path().join("blocking-item").exists());
        Ok(())
    }

    #[test]
    fn batch_checks_conflicts_before_any_write_and_reverts_files() -> Result<()> {
        let root = tempfile::tempdir()?;
        fs::write(root.path().join("old.txt"), "one")?;
        let changes = || {
            vec![
                Change::Create {
                    path: "sub/new.txt".into(),
                    content: "two".into(),
                },
                Change::Edit {
                    path: "old.txt".into(),
                    before: "one".into(),
                    after: "three".into(),
                },
            ]
        };
        let batch = Batch::prepare(root.path(), changes())?;
        fs::write(root.path().join("old.txt"), "changed")?;
        assert!(batch.apply().is_err());
        assert!(!root.path().join("sub").exists());
        fs::write(root.path().join("old.txt"), "one")?;
        let undo = Batch::prepare(root.path(), changes())?.apply()?;
        assert_eq!(fs::read_to_string(root.path().join("sub/new.txt"))?, "two");
        undo.apply()?;
        assert_eq!(fs::read_to_string(root.path().join("old.txt"))?, "one");
        assert!(!root.path().join("sub/new.txt").exists());
        assert!(
            Batch::prepare(
                root.path(),
                vec![Change::Create {
                    path: "../escape".into(),
                    content: "bad".into()
                }]
            )
            .is_err()
        );
        Ok(())
    }
}
