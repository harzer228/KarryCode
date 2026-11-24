use super::*;
use age::secrecy::ExposeSecret;
use anyhow::Context;
use std::{
    fs,
    io::{self, Read, Write},
    ptr, slice,
};
use windows_sys::Win32::{
    Foundation::LocalFree,
    Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    },
};

pub fn unlock(folder: &Path, _: &str, fresh: bool) -> Result<SecretString> {
    let file = folder.join("local-key.dpapi");
    match fs::symlink_metadata(&file) {
        Ok(metadata) => {
            ensure!(
                metadata.is_file() && !crate::safe_files::is_link(&metadata),
                "Invalid Windows key file"
            );
            let mut protected = Vec::new();
            fs::File::open(&file)?
                .take(16_385)
                .read_to_end(&mut protected)?;
            ensure!(
                (1..=16_384).contains(&protected.len()),
                "Invalid Windows key size"
            );
            let plaintext = crypt(&protected, false).context(tr!(
                "Профиль недоступен в этой учётной записи Windows.",
                "The profile cannot be unlocked under this Windows account."
            ))?;
            let secret = SecretString::from(String::from_utf8(plaintext)?);
            ensure!(secret.expose_secret().len() == 43, "Invalid profile key");
            Ok(secret)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            ensure!(
                fresh,
                tr!(
                    "Ключ Windows отсутствует. Восстанови файл local-key.dpapi для этого профиля.",
                    "The Windows key is missing. Restore the profile's local-key.dpapi file."
                )
            );
            let secret = generate()?;
            let protected = crypt(secret.expose_secret().as_bytes(), true)?;
            let mut draft = tempfile::NamedTempFile::new_in(folder)?;
            draft.write_all(&protected)?;
            draft.as_file().sync_all()?;
            draft.persist_noclobber(&file)?;
            Ok(secret)
        }
        Err(error) => Err(error.into()),
    }
}

fn crypt(input: &[u8], encrypt: bool) -> Result<Vec<u8>> {
    let source = CRYPT_INTEGER_BLOB {
        cbData: input.len().try_into()?,
        pbData: input.as_ptr().cast_mut(),
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    let ok = unsafe {
        if encrypt {
            CryptProtectData(
                &source,
                ptr::null(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        } else {
            CryptUnprotectData(
                &source,
                ptr::null_mut(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        }
    };
    if ok == 0 {
        return Err(io::Error::last_os_error().into());
    }
    ensure!(!output.pbData.is_null(), "Windows returned an empty key");
    let result = unsafe {
        let bytes = slice::from_raw_parts_mut(output.pbData, output.cbData as usize);
        let copied = bytes.to_vec();
        bytes.fill(0);
        LocalFree(output.pbData.cast());
        copied
    };
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reopens_a_windows_key_and_rejects_a_damaged_key_without_rotating() -> Result<()> {
        let folder = tempfile::tempdir()?;
        let first = unlock(folder.path(), "fixture", true)?;
        let second = unlock(folder.path(), "fixture", false)?;
        assert_eq!(first.expose_secret(), second.expose_secret());
        let file = folder.path().join("local-key.dpapi");
        let encrypted = fs::read(&file)?;
        assert!(
            !encrypted
                .windows(43)
                .any(|window| window == first.expose_secret().as_bytes())
        );
        fs::write(&file, b"damaged")?;
        assert!(unlock(folder.path(), "fixture", false).is_err());
        assert_eq!(fs::read(&file)?, b"damaged");
        Ok(())
    }
}
