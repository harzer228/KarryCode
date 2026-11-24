#[cfg(windows)]
use crate::folders;
use crate::platform;
#[cfg(not(windows))]
use anyhow::ensure;
use anyhow::{Context, Result};
#[cfg(windows)]
use std::io;
use std::{
    fs::{self, File, Metadata},
    path::Path,
};

pub(crate) fn is_link(metadata: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
    }
    #[cfg(not(windows))]
    metadata.is_symlink()
}

#[cfg(target_os = "linux")]
pub(crate) fn replace(path: &Path, draft: &Path, _stage: tempfile::TempDir) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let metadata = fs::metadata(path)?;
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o7000 == 0,
        "{}",
        tr!(
            "Выбери обычный файл, которым владеет твой пользователь.",
            "Choose an ordinary file owned by your user."
        )
    );
    let draft_metadata = fs::metadata(draft)?;
    if metadata.gid() != draft_metadata.gid() {
        std::os::unix::fs::chown(draft, None, Some(metadata.gid()))?;
    }
    if metadata.permissions() != draft_metadata.permissions() {
        fs::set_permissions(draft, fs::Permissions::from_mode(metadata.mode() & 0o777))?;
    }
    match xattr::list(path) {
        Ok(keys) => {
            for key in keys {
                let value = xattr::get(path, &key)?
                    .context(tr!("Атрибут файла изменился", "A file attribute changed"))?;
                xattr::set(draft, key, &value)?;
            }
        }
        Err(error) if error.raw_os_error() == Some(libc::ENOTSUP) => {}
        Err(error) => return Err(error.into()),
    }
    File::open(draft)?.sync_all()?;
    fs::rename(draft, path)?;
    platform::sync_directory(path.parent().unwrap()).context(tr!(
        "Замена записана, но не удалось подтвердить её сохранение на диске.",
        "The replacement was written, but its persistence could not be verified."
    ))
}

#[cfg(windows)]
pub(crate) fn replace(path: &Path, draft: &Path, stage: tempfile::TempDir) -> Result<()> {
    use std::ptr::null_mut;
    use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;
    let backup = stage.path().join("original");
    let success = unsafe {
        ReplaceFileW(
            platform::wide(path.as_os_str()).as_ptr(),
            platform::wide(draft.as_os_str()).as_ptr(),
            platform::wide(backup.as_os_str()).as_ptr(),
            0,
            null_mut(),
            null_mut(),
        )
    };
    if success == 0 {
        let error = io::Error::last_os_error();
        if backup.is_file() && (path.exists() || fs::rename(&backup, path).is_err()) {
            let recovery = stage.keep().join("original");
            return Err(error).with_context(|| {
                trf!(
                    "Исходный файл сохранён в {}",
                    "The original file is available at {}",
                    folders::display(&recovery)
                )
            });
        }
        return Err(error).context(tr!(
            "Не удалось заменить файл с сохранением его разрешений",
            "Could not replace the file while preserving its permissions"
        ));
    }
    File::options()
        .write(true)
        .open(path)?
        .sync_all()
        .context(tr!(
            "Замена записана, но не удалось подтвердить её сохранение на диске.",
            "The replacement was written, but its persistence could not be verified."
        ))?;
    Ok(())
}
