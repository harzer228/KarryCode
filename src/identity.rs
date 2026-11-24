use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::Read,
    path::{Component, Path},
};

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Marker {
    format: u32,
    pub id: String,
}

pub fn read_marker(root: &Path) -> Result<Marker> {
    let file = File::open(root.join("profile-id.json")).context(tr!(
        "Нет profile-id.json в каталоге комплекта",
        "profile-id.json is missing from the bundle"
    ))?;
    let marker: Marker = serde_json::from_reader(file.take(4096))
        .context(tr!("Повреждён profile-id.json", "Invalid profile-id.json"))?;
    ensure!(
        marker.format == 1,
        "{}",
        tr!("Неизвестный формат комплекта", "Unknown bundle format")
    );
    ensure!(
        marker.id.len() == 32 && marker.id.bytes().all(|b| b.is_ascii_hexdigit()),
        "{}",
        tr!("Некорректный ID комплекта", "Invalid bundle ID")
    );
    Ok(marker)
}

pub fn init(root: &Path) -> Result<()> {
    let mut random = [0; 16];
    getrandom::fill(&mut random).map_err(|error| anyhow::anyhow!("{error}"))?;
    let marker = Marker {
        format: 1,
        id: format!("{:032x}", u128::from_be_bytes(random)),
    };
    let mut draft = tempfile::NamedTempFile::new_in(root)?;
    serde_json::to_writer_pretty(draft.as_file_mut(), &marker)?;
    draft.as_file().sync_all()?;
    draft.persist_noclobber(root.join("profile-id.json"))?;
    Ok(())
}

pub fn valid(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path.components().all(|part| {
            matches!(part, Component::Normal(value)
            if value.to_str().is_some_and(|s| !s.contains(['\\', ':'])))
        })
}
