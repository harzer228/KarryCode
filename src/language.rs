use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::Read,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    Ru,
    #[default]
    En,
}

#[derive(Default, Deserialize, Serialize)]
struct Preferences {
    language: Language,
}

static ENGLISH: AtomicBool = AtomicBool::new(false);
const FILE: &str = "preferences.json";

pub fn english() -> bool {
    ENGLISH.load(Ordering::Relaxed)
}

pub fn load(root: &Path) {
    let preferences: Preferences = File::open(root.join(FILE))
        .ok()
        .and_then(|file| serde_json::from_reader(file.take(4096)).ok())
        .unwrap_or_default();
    ENGLISH.store(
        matches!(preferences.language, Language::En),
        Ordering::Relaxed,
    );
}

pub fn save(root: &Path, language: Language) -> Result<()> {
    let mut draft = tempfile::NamedTempFile::new_in(root)?;
    serde_json::to_writer_pretty(draft.as_file_mut(), &Preferences { language })?;
    draft.as_file().sync_all()?;
    draft.persist(root.join(FILE))?;
    ENGLISH.store(matches!(language, Language::En), Ordering::Relaxed);
    Ok(())
}
