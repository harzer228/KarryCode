use age::secrecy::SecretString;
use anyhow::{Result, ensure};
use std::path::Path;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "linux")]
pub use linux::unlock;
#[cfg(windows)]
pub use windows::unlock;

fn generate() -> Result<SecretString> {
    use base64::{Engine, engine::general_purpose::STANDARD_NO_PAD};
    let mut key = [0; 32];
    getrandom::fill(&mut key).map_err(|error| anyhow::anyhow!("{error}"))?;
    let encoded = STANDARD_NO_PAD.encode(key);
    key.fill(0);
    Ok(SecretString::from(encoded))
}
