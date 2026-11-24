use super::*;
use age::secrecy::ExposeSecret;
use anyhow::{Context, bail};
use std::{
    collections::HashMap,
    fs,
    io::{self, Read, Write},
};
use zbus::{
    blocking::{Connection, Proxy},
    zvariant::{OwnedObjectPath, OwnedValue, Value},
};

const SERVICE: &str = "org.freedesktop.secrets";
type Secret = (OwnedObjectPath, Vec<u8>, Vec<u8>, String);

pub fn unlock(folder: &Path, identity: &str, fresh: bool) -> Result<SecretString> {
    let file = folder.join("unlock-method");
    match fs::symlink_metadata(&file) {
        Ok(metadata) => {
            ensure!(
                metadata.is_file() && !crate::safe_files::is_link(&metadata),
                "Invalid profile unlock method"
            );
            let mut method = String::new();
            fs::File::open(file)?.take(32).read_to_string(&mut method)?;
            match method.trim() {
                "password" => password(fresh),
                "keyring" => keyring_key(identity, false).context(tr!(
                    "Разблокируй системное хранилище ключей KarryCode.",
                    "Unlock the system keyring containing the KarryCode profile key."
                )),
                _ => bail!("Unknown profile unlock method"),
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            ensure!(
                fresh,
                tr!(
                    "Отсутствуют настройки разблокировки профиля.",
                    "The profile unlock settings are missing."
                )
            );
            let (method, key) = match keyring_key(identity, true) {
                Ok(key) => ("keyring", key),
                Err(_) => {
                    println!(
                        "{}",
                        tr!(
                            "Хранилище ключей недоступно. Создай пароль для этого профиля.",
                            "A system keyring is unavailable. Create a password for this profile."
                        )
                    );
                    ("password", password(true)?)
                }
            };
            let mut draft = tempfile::NamedTempFile::new_in(folder)?;
            writeln!(draft, "{method}")?;
            draft.as_file().sync_all()?;
            draft.persist_noclobber(file)?;
            crate::platform::sync_directory(folder)?;
            Ok(key)
        }
        Err(error) => Err(error.into()),
    }
}

fn keyring_key(identity: &str, create: bool) -> Result<SecretString> {
    let bus = Connection::session()?;
    let service = Proxy::new(
        &bus,
        SERVICE,
        "/org/freedesktop/secrets",
        "org.freedesktop.Secret.Service",
    )?;
    let attrs = HashMap::from([("application", "karrycode"), ("profile", identity)]);
    let (unlocked, locked): (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>) =
        service.call("SearchItems", &(&attrs,))?;
    ensure!(
        locked.is_empty() && unlocked.len() <= 1,
        "KarryCode keyring item is locked or ambiguous"
    );
    let (_, session): (OwnedValue, OwnedObjectPath) =
        service.call("OpenSession", &("plain", Value::from("")))?;
    let result = if let Some(item) = unlocked.first() {
        let item = Proxy::new(&bus, SERVICE, item.as_str(), "org.freedesktop.Secret.Item")?;
        let (_, _, bytes, _): Secret = item.call("GetSecret", &(&session,))?;
        let value = SecretString::from(String::from_utf8(bytes)?);
        ensure!(
            value.expose_secret().len() == 43,
            "Invalid keyring profile key"
        );
        Ok(value)
    } else {
        ensure!(create, "The KarryCode profile key is missing");
        let collection: OwnedObjectPath = service.call("ReadAlias", &("default",))?;
        ensure!(collection.as_str() != "/", "No default keyring collection");
        let collection = Proxy::new(
            &bus,
            SERVICE,
            collection.as_str(),
            "org.freedesktop.Secret.Collection",
        )?;
        ensure!(
            !collection.get_property::<bool>("Locked")?,
            "The default keyring is locked"
        );
        let secret = generate()?;
        let properties = HashMap::from([
            (
                "org.freedesktop.Secret.Item.Label",
                Value::from("KarryCode CLI profile"),
            ),
            ("org.freedesktop.Secret.Item.Attributes", Value::from(attrs)),
        ]);
        let value = (
            session.as_ref(),
            Vec::<u8>::new(),
            secret.expose_secret().as_bytes(),
            "text/plain",
        );
        let (item, prompt): (OwnedObjectPath, OwnedObjectPath) =
            collection.call("CreateItem", &(&properties, value, false))?;
        ensure!(
            item.as_str() != "/" && prompt.as_str() == "/",
            "The keyring requires user interaction"
        );
        Ok(secret)
    };
    if let Ok(proxy) = Proxy::new(
        &bus,
        SERVICE,
        session.as_str(),
        "org.freedesktop.Secret.Session",
    ) {
        let _ = proxy.call::<_, _, ()>("Close", &());
    }
    result
}

fn password(fresh: bool) -> Result<SecretString> {
    let secret = read_secret(if fresh {
        tr!("Новый пароль профиля: ", "New profile password: ")
    } else {
        tr!("Пароль профиля: ", "Profile password: ")
    })?;
    ensure!(
        !secret.expose_secret().is_empty(),
        "{}",
        tr!(
            "Пароль не может быть пустым",
            "The password cannot be empty"
        )
    );
    if fresh {
        let again = read_secret(tr!("Повтори пароль: ", "Repeat the password: "))?;
        ensure!(
            secret.expose_secret() == again.expose_secret(),
            "{}",
            tr!("Пароли отличаются", "The passwords do not match")
        );
    }
    Ok(secret)
}

fn read_secret(message: &str) -> Result<SecretString> {
    print!("{message}");
    io::stdout().flush()?;
    Ok(SecretString::from(rpassword::read_password()?))
}
