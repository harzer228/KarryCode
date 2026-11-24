use crate::recent::RecentFolders;
use age::{Decryptor, Encryptor, scrypt, secrecy::SecretString};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{self, BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
};

const MAX_SNAPSHOT: u64 = 2 * 1024 * 1024 * 1024;
const METADATA: &str = "snapshot.json";
const MAX_METADATA: usize = 4096;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct Snapshot {
    format: u32,
    id: String,
    revision: u64,
    #[serde(default, skip_serializing_if = "RecentFolders::is_empty")]
    recent: RecentFolders,
}

struct Saved {
    revision: u64,
    slot: u8,
}

pub struct Profile {
    root: PathBuf,
    id: String,
    secret: SecretString,
    _lock: File,
    active: Option<Saved>,
    observed: [Option<String>; 2],
    pub recent: RecentFolders,
    pub work: tempfile::TempDir,
}

impl Profile {
    pub fn open(root: &Path) -> Result<Self> {
        let marker = crate::identity::read_marker(root)?;
        let folder = root.join("profile");
        fs::create_dir_all(&folder)?;
        ensure!(
            !crate::safe_files::is_link(&fs::symlink_metadata(&folder)?),
            "{}",
            tr!(
                "Каталог профиля является ссылкой",
                "The profile folder is a link"
            )
        );
        crate::platform::protect(&folder)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(folder.join("session.lock"))?;
        lock.try_lock().context(tr!(
            "Профиль уже используется или том не поддерживает блокировку",
            "The profile is in use or the drive does not support file locking"
        ))?;
        let observed = [
            fingerprint(&slot_path(&folder, 0))?,
            fingerprint(&slot_path(&folder, 1))?,
        ];
        let fresh = observed.iter().all(Option::is_none);
        let secret = crate::vault::unlock(&folder, &marker.id, fresh)?;
        let mut work = crate::platform::private_dir()?;
        let mut active: Option<Saved> = None;
        let mut recent = RecentFolders::default();
        let mut unreadable = false;
        for (slot, hash) in observed.iter().enumerate() {
            if hash.is_none() {
                continue;
            }
            let path = slot_path(&folder, slot);
            let candidate = crate::platform::private_dir()?;
            match extract(&path, &secret, &marker.id, Some(candidate.path())) {
                Ok(snapshot)
                    if active
                        .as_ref()
                        .is_none_or(|old| snapshot.revision > old.revision) =>
                {
                    work = candidate;
                    recent = snapshot.recent;
                    active = Some(Saved {
                        revision: snapshot.revision,
                        slot: slot as u8,
                    });
                }
                Ok(_) => {}
                Err(_) => unreadable = true,
            }
        }
        ensure!(
            fresh || active.is_some(),
            "{}",
            tr!(
                "Профиль не открылся. Проверь доступ к ключу или паролю профиля.",
                "Could not unlock the profile. Check access to its key or password."
            )
        );
        if unreadable {
            println!(
                "{}",
                tr!(
                    "Один снимок не читается. Доступен другой сохранённый снимок.",
                    "One snapshot is unreadable. Another saved snapshot is available."
                )
            );
            ensure!(
                crate::ask(tr!(
                    "Открыть доступный снимок? [y/N] ",
                    "Open the available snapshot? [y/N] "
                ))?
                .eq_ignore_ascii_case("y"),
                "{}",
                tr!("Разблокировка отменена", "Unlock canceled")
            );
        }
        Ok(Self {
            root: root.to_owned(),
            id: marker.id,
            secret,
            _lock: lock,
            active,
            observed,
            recent,
            work,
        })
    }

    fn checked_folder(&self) -> Result<PathBuf> {
        ensure!(
            crate::identity::read_marker(&self.root)?.id == self.id,
            "{}",
            tr!(
                "Каталог открытого профиля изменился",
                "The open profile directory changed"
            )
        );
        let folder = self.root.join("profile");
        ensure!(
            !fs::symlink_metadata(&folder)?.file_type().is_symlink(),
            "{}",
            tr!(
                "Каталог профиля является ссылкой",
                "The profile folder is a link"
            )
        );
        Ok(folder)
    }

    fn lock_for_save(&self, folder: &Path) -> Result<File> {
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(folder.join("save.lock"))?;
        lock.try_lock().context(tr!(
            "Профиль уже сохраняется",
            "The profile is already being saved"
        ))?;
        self.check_observed(folder)?;
        Ok(lock)
    }

    fn check_observed(&self, folder: &Path) -> Result<()> {
        for (slot, old) in self.observed.iter().enumerate() {
            ensure!(
                fingerprint(&slot_path(folder, slot))? == *old,
                "{}",
                tr!(
                    "Профиль обновился в другом сеансе",
                    "Another session updated the profile"
                )
            );
        }
        Ok(())
    }

    fn next_revision(&self) -> Result<u64> {
        self.active
            .as_ref()
            .map_or(Some(1), |old| old.revision.checked_add(1))
            .context(tr!(
                "Переполнение номера снимка",
                "Snapshot revision overflow"
            ))
    }

    pub fn save(&mut self) -> Result<()> {
        let folder = self.checked_folder()?;
        let _lock = self.lock_for_save(&folder)?;
        let (slot, revision) = match &self.active {
            Some(old) => (1 - old.slot, self.next_revision()?),
            None => (0, 1),
        };
        let path = slot_path(&folder, slot as usize);
        let mut draft = tempfile::NamedTempFile::new_in(&folder)?;
        write_snapshot(
            self.work.path(),
            &self.secret,
            &self.id,
            revision,
            &self.recent,
            draft.as_file_mut(),
        )?;
        draft.as_file().sync_all()?;
        let digest = fingerprint(draft.path())?.context(tr!(
            "Временная копия отсутствует",
            "Temporary snapshot is missing"
        ))?;
        let committed = draft.persist(&path)?;
        ensure!(
            fingerprint(&path)?.as_ref() == Some(&digest),
            "{}",
            tr!(
                "Записанный снимок не прошёл проверку",
                "The saved snapshot failed verification"
            )
        );
        self.observed[slot as usize] = Some(digest);
        committed.sync_all()?;
        crate::platform::sync_directory(&folder)?;
        self.active = Some(Saved { revision, slot });
        Ok(())
    }

    pub fn close(self) -> Result<()> {
        self.work.close()?;
        Ok(())
    }
}

fn slot_path(folder: &Path, slot: usize) -> PathBuf {
    folder.join(format!("snapshot-{slot}.age"))
}

fn fingerprint(path: &Path) -> Result<Option<String>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        metadata.is_file() && metadata.len() <= MAX_SNAPSHOT + 32 * 1024 * 1024,
        "{}",
        trf!(
            "Необычный файл снимка {}",
            "Unexpected snapshot file {}",
            path.display()
        )
    );
    let mut digest = Sha256::new();
    io::copy(&mut File::open(path)?, &mut digest)?;
    Ok(Some(format!("{:x}", digest.finalize())))
}

fn permitted(path: &Path) -> bool {
    if !crate::identity::valid(path) {
        return false;
    }
    let entries: Vec<_> = path.iter().filter_map(|part| part.to_str()).collect();
    if entries.iter().any(|name| {
        matches!(
            *name,
            "node_modules"
                | "cache"
                | "log"
                | "logs"
                | "repos"
                | ".sandbox"
                | "tmp"
                | "sock"
                | "sockets"
                | "bin"
        ) || name.ends_with(".sock")
            || name.ends_with(".log")
    }) {
        return false;
    }
    matches!(entries.first().copied(), Some("oc-state"))
}

fn append_files<W: Write>(
    tar: &mut tar::Builder<W>,
    base: &Path,
    relative: &Path,
    total: &mut u64,
) -> Result<()> {
    let mut children = fs::read_dir(base.join(relative))?.collect::<io::Result<Vec<_>>>()?;
    children.sort_by_key(|child| child.file_name());
    for child in children {
        let path = relative.join(child.file_name());
        if !permitted(&path) {
            continue;
        }
        let kind = child.file_type()?;
        if kind.is_dir() {
            append_files(tar, base, &path, total)?;
        } else if kind.is_file() {
            let file = File::open(child.path())?;
            let size = file.metadata()?.len();
            *total = total
                .checked_add(size)
                .context(tr!("Слишком большой профиль", "The profile is too large"))?;
            ensure!(
                *total <= MAX_SNAPSHOT,
                "{}",
                tr!("Профиль превышает 2 ГиБ", "The profile exceeds 2 GiB")
            );
            let mut header = tar::Header::new_gnu();
            header.set_size(size);
            header.set_mode(0o600);
            tar.append_data(&mut header, path, file)?;
        }
    }
    Ok(())
}

fn write_snapshot(
    work: &Path,
    secret: &SecretString,
    id: &str,
    revision: u64,
    recent: &RecentFolders,
    output: &mut File,
) -> Result<()> {
    let mut snapshot = Snapshot {
        format: 1,
        id: id.to_owned(),
        revision,
        recent: recent.clone(),
    };
    let metadata = loop {
        let bytes = serde_json::to_vec(&snapshot)?;
        if bytes.len() <= MAX_METADATA {
            break bytes;
        }
        ensure!(
            !snapshot.recent.is_empty(),
            "{}",
            tr!(
                "Слишком большие метаданные профиля",
                "Profile metadata is too large"
            )
        );
        snapshot.recent.forget_oldest();
    };
    let mut recipient = scrypt::Recipient::new(secret.clone());
    recipient.set_work_factor(18);
    let encryptor = Encryptor::with_recipients(std::iter::once(&recipient as &dyn age::Recipient))?;
    let encrypted = encryptor.wrap_output(BufWriter::new(output))?;
    let mut archive = tar::Builder::new(encrypted);
    let mut header = tar::Header::new_gnu();
    header.set_size(metadata.len() as u64);
    header.set_mode(0o600);
    archive.append_data(&mut header, METADATA, metadata.as_slice())?;
    append_files(&mut archive, work, Path::new(""), &mut 0)?;
    archive.into_inner()?.finish()?.flush()?;
    Ok(())
}

fn extract(
    path: &Path,
    secret: &SecretString,
    id: &str,
    destination: Option<&Path>,
) -> Result<Snapshot> {
    let mut age_identity = scrypt::Identity::new(secret.clone());
    age_identity.set_max_work_factor(18);
    let decryptor = Decryptor::new_buffered(BufReader::new(File::open(path)?))?;
    let decrypted = decryptor.decrypt(std::iter::once(&age_identity as &dyn age::Identity))?;
    let mut archive = tar::Archive::new(decrypted);
    let mut entries = archive.entries()?;
    let metadata = entries
        .next()
        .context(tr!("Пустой снимок", "Empty snapshot"))??;
    ensure!(
        metadata.path()? == Path::new(METADATA) && metadata.size() <= MAX_METADATA as u64,
        "{}",
        tr!("Неизвестный формат снимка", "Unknown snapshot format")
    );
    let metadata: Snapshot = serde_json::from_reader(metadata)?;
    ensure!(
        metadata.format == 1 && metadata.id == id && metadata.revision > 0,
        "{}",
        tr!(
            "Снимок относится к другому комплекту",
            "The snapshot belongs to another bundle"
        )
    );
    let mut size = 0_u64;
    let mut seen = HashSet::new();
    for entry in entries {
        let mut entry = entry?;
        let relative = entry.path()?.into_owned();
        ensure!(
            entry.header().entry_type().is_file() && permitted(&relative),
            "{}",
            tr!("Недопустимый путь в профиле", "Invalid path in the profile")
        );
        let key = relative.to_string_lossy().to_lowercase();
        ensure!(
            seen.insert(key),
            "{}",
            tr!("Повтор пути в снимке", "Duplicate path in the snapshot")
        );
        size = size
            .checked_add(entry.size())
            .context(tr!("Слишком большой профиль", "The profile is too large"))?;
        ensure!(
            size <= MAX_SNAPSHOT && seen.len() <= 100_000,
            "{}",
            tr!("Слишком большой профиль", "The profile is too large")
        );
        let copied = if let Some(destination) = destination {
            let output = destination.join(relative);
            fs::create_dir_all(
                output
                    .parent()
                    .context(tr!("Недопустимый путь", "Invalid path"))?,
            )?;
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(output)?;
            io::copy(&mut entry, &mut file)?
        } else {
            io::copy(&mut entry, &mut io::sink())?
        };
        ensure!(
            copied == entry.size(),
            "{}",
            tr!("Оборванный файл в снимке", "Truncated file in the snapshot")
        );
    }
    // Reading through EOF also authenticates ciphertext after the tar end marker.
    let mut decrypted = archive.into_inner();
    let mut tail = (&mut decrypted).take(4097);
    let remaining = io::copy(&mut tail, &mut io::sink())?;
    if remaining > 4096 {
        bail!(
            "{}",
            tr!(
                "Лишние данные после снимка",
                "Unexpected data after the snapshot"
            )
        )
    }
    Ok(metadata)
}

#[cfg(test)]
pub(crate) fn restore_test_agent_state(legacy: &[u8]) -> Result<tempfile::TempDir> {
    let source = tempfile::tempdir()?;
    let path = source.path().join("oc-state/karrycode-agent/state.json");
    fs::create_dir_all(path.parent().unwrap())?;
    fs::write(path, legacy)?;

    let secret = SecretString::from("synthetic migration password".to_owned());
    let destination = tempfile::tempdir()?;
    let snapshot = destination.path().join("legacy.age");
    write_snapshot(
        source.path(),
        &secret,
        "migration-test",
        1,
        &RecentFolders::default(),
        &mut File::create(&snapshot)?,
    )?;
    extract(
        &snapshot,
        &secret,
        "migration-test",
        Some(destination.path()),
    )?;
    Ok(destination)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypted_profile_roundtrip_rejects_wrong_identity_and_truncated_data() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let work = dir.path().join("source");
        fs::create_dir_all(work.join("codex"))?;
        fs::create_dir_all(work.join("oc-data/opencode"))?;
        fs::create_dir_all(work.join("tmp"))?;
        fs::create_dir_all(work.join("oc-state/karrycode-agent"))?;
        fs::write(
            work.join("oc-state/karrycode-agent/state.json"),
            b"synthetic-agent-state",
        )?;
        fs::write(work.join("codex/auth.json"), b"synthetic-token")?;
        fs::write(work.join("oc-data/opencode/opencode.db"), b"synthetic-db")?;
        fs::write(work.join("tmp/private.txt"), b"session scratch")?;
        let secret = SecretString::from("a synthetic password for the test".to_owned());
        let file = dir.path().join("snapshot.age");
        write_snapshot(
            &work,
            &secret,
            "test-marker",
            3,
            &RecentFolders::default(),
            &mut File::create(&file)?,
        )?;
        let destination = dir.path().join("output");
        fs::create_dir(&destination)?;
        let snapshot = extract(&file, &secret, "test-marker", Some(&destination))?;
        assert_eq!(snapshot.revision, 3);
        assert!(snapshot.recent.is_empty());
        assert!(!destination.join("codex").exists());
        assert!(!destination.join("oc-data").exists());
        assert_eq!(
            fs::read(destination.join("oc-state/karrycode-agent/state.json"))?,
            b"synthetic-agent-state"
        );
        assert!(!destination.join("tmp").exists());
        assert!(extract(&file, &secret, "other-marker", None).is_err());
        let bytes = fs::read(&file)?;
        assert!(!bytes.windows(15).any(|bytes| bytes == b"synthetic-token"));
        fs::write(&file, &bytes[..bytes.len() - 17])?;
        assert!(extract(&file, &secret, "test-marker", None).is_err());
        Ok(())
    }

    #[test]
    fn archive_paths_stay_within_expected_profile() {
        for value in [
            "../codex/auth.json",
            "oc-data/../other/file",
            "tmp/key",
            "oc-data/opencode/log/last.log",
            "/codex/auth.json",
            "oc-config/opencode/node_modules/p/file",
        ] {
            assert!(!permitted(Path::new(value)), "{value}");
        }
        assert!(!permitted(Path::new("oc-data/opencode/opencode.db-wal")));
        assert!(permitted(Path::new("oc-state/karrycode-agent/state.json")));
    }
}
