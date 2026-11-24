use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

const DISPLAY_LIMIT: usize = 5;
const HISTORY_LIMIT: usize = 15;

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct RecentFolders(Vec<Entry>);

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "path", rename_all = "lowercase")]
enum Entry {
    Bundle(String),
    Windows(String),
    Linux(String),
}

impl RecentFolders {
    pub fn remember(&mut self, root: &Path, path: &Path) {
        let Ok(path) = path.canonicalize() else {
            return;
        };
        let Some(entry) = Entry::from_path(root, &path) else {
            return;
        };
        self.0
            .retain(|old| old != &entry && old.resolve(root).as_ref() != Some(&path));
        self.0.insert(0, entry);
        self.0.truncate(HISTORY_LIMIT);
    }

    pub fn available(&self, root: &Path) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        for path in self.0.iter().filter_map(|entry| entry.resolve(root)) {
            if !paths.contains(&path) && path.parent().is_some() && fs::read_dir(&path).is_ok() {
                paths.push(path);
                if paths.len() == DISPLAY_LIMIT {
                    break;
                }
            }
        }
        paths
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn forget_oldest(&mut self) {
        self.0.pop();
    }
}

impl Entry {
    fn from_path(root: &Path, path: &Path) -> Option<Self> {
        let root = root.canonicalize().ok()?;
        if let Ok(relative) = path.strip_prefix(root) {
            if relative.as_os_str().is_empty() {
                return Some(Self::Bundle(String::new()));
            }
            if crate::identity::valid(relative) {
                let portable = relative
                    .iter()
                    .map(|part| part.to_str().unwrap())
                    .collect::<Vec<_>>();
                return Some(Self::Bundle(portable.join("/")));
            }
            return None;
        }
        let absolute = path.to_str()?.to_owned();
        Some(if cfg!(windows) {
            Self::Windows(absolute)
        } else {
            Self::Linux(absolute)
        })
    }

    fn resolve(&self, root: &Path) -> Option<PathBuf> {
        match self {
            Self::Bundle(relative) => {
                let root = root.canonicalize().ok()?;
                let relative = Path::new(relative);
                if !relative.as_os_str().is_empty() && !crate::identity::valid(relative) {
                    return None;
                }
                let path = root.join(relative).canonicalize().ok()?;
                path.starts_with(&root).then_some(path)
            }
            Self::Windows(path) if cfg!(windows) => resolve_absolute(path),
            Self::Linux(path) if cfg!(target_os = "linux") => resolve_absolute(path),
            _ => None,
        }
    }
}

fn resolve_absolute(path: &str) -> Option<PathBuf> {
    let path = Path::new(path);
    path.is_absolute()
        .then(|| path.canonicalize().ok())
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portable_folders_follow_bundle_and_skip_stale_entries() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let old_root = temp.path().join("drive-a");
        let new_root = temp.path().join("drive-b");
        let first = old_root.join("Projects/испытание one");
        let second = old_root.join("Projects/two");
        let local = temp.path().join("Local folder");
        for path in [&first, &second, &local] {
            fs::create_dir_all(path)?;
        }
        let mut recent = RecentFolders::default();
        recent.remember(&old_root, &first);
        recent.remember(&old_root, &second);
        recent.remember(&old_root, &local);
        recent.remember(&old_root, &first);
        assert_eq!(
            recent.available(&old_root),
            [
                first.canonicalize()?,
                local.canonicalize()?,
                second.canonicalize()?,
            ]
        );
        fs::remove_dir(&second)?;
        fs::rename(&old_root, &new_root)?;
        let mut recent: RecentFolders = serde_json::from_str(&serde_json::to_string(&recent)?)?;
        recent.0.insert(0, Entry::Bundle("../Local folder".into()));
        recent.0.insert(
            0,
            if cfg!(windows) {
                Entry::Linux(local.to_string_lossy().into_owned())
            } else {
                Entry::Windows(local.to_string_lossy().into_owned())
            },
        );
        assert_eq!(
            recent.available(&new_root),
            [
                new_root.join("Projects/испытание one").canonicalize()?,
                local.canonicalize()?,
            ]
        );
        for number in 0..7 {
            let path = new_root.join(format!("folder-{number}"));
            fs::create_dir(&path)?;
            recent.remember(&new_root, &path);
        }
        assert_eq!(recent.available(&new_root).len(), DISPLAY_LIMIT);
        Ok(())
    }
}
