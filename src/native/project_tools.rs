use super::{changes, field, model_config, read_file};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{collections::VecDeque, path::Path};

const INLINE: usize = 6 * 1024;
const RESULT_BYTES: usize = 1024 * 1024;

fn utf8_start(text: &str, mut index: usize) -> usize {
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn utf8_end(text: &str, mut index: usize) -> usize {
    while !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

pub(super) fn read(root: &Path, arguments: &Value) -> Result<String> {
    let text = read_file(root, field(arguments, "path"))?;
    if arguments.get("offset").is_none() && arguments.get("limit").is_none() {
        return Ok(text);
    }
    let offset = arguments["offset"].as_u64().unwrap_or(0).min(100_000) as usize;
    let limit = arguments["limit"].as_u64().unwrap_or(200).clamp(1, 2_000) as usize;
    Ok(text
        .lines()
        .enumerate()
        .skip(offset)
        .take(limit)
        .map(|(index, line)| format!("{}: {line}", index + 1))
        .collect::<Vec<_>>()
        .join("\n"))
}

pub(super) fn read_many(root: &Path, arguments: &Value) -> Result<String> {
    let files = arguments["files"]
        .as_array()
        .context("Supply files to read")?;
    ensure!((1..=12).contains(&files.len()), "Supply 1–12 files");
    let mut output = String::new();
    for file in files {
        let path = field(file, "path");
        let result = read(root, file).unwrap_or_else(|error| format!("Could not read: {error:#}"));
        output.push_str(&format!("\n## {path}\n{result}\n"));
    }
    Ok(output)
}

pub(super) fn patch(root: &Path, arguments: &Value) -> Result<Vec<changes::Change>> {
    let files = arguments["files"]
        .as_array()
        .context("Supply files to patch")?;
    ensure!((1..=24).contains(&files.len()), "Supply 1–24 patches");
    files
        .iter()
        .map(|file| {
            changes::unified_patch(root, Path::new(field(file, "path")), field(file, "patch"))
        })
        .collect()
}

#[derive(Default)]
pub(super) struct Results {
    entries: VecDeque<(String, String)>,
    bytes: usize,
}

impl Results {
    pub fn inline(&mut self, mut text: String) -> String {
        if text.len() <= INLINE {
            return text;
        }
        let truncated = text.len() > RESULT_BYTES;
        text.truncate(utf8_start(&text, RESULT_BYTES.min(text.len())));
        let id = model_config::hash(text.as_bytes())[..16].to_owned();
        let preview = format!(
            "{}\n\n[Result {id} · {} bytes{} · use read_tool_result(id, offset, limit) for more]",
            &text[..utf8_start(&text, INLINE)],
            text.len(),
            if truncated { ", capped at 1 MiB" } else { "" },
        );
        if !self.entries.iter().any(|(current, _)| current == &id) {
            while self.bytes + text.len() > RESULT_BYTES {
                if let Some((_, removed)) = self.entries.pop_front() {
                    self.bytes -= removed.len();
                }
            }
            self.bytes += text.len();
            self.entries.push_back((id, text));
        }
        preview
    }

    pub fn read(&self, arguments: &Value) -> Result<String> {
        let id = field(arguments, "id");
        let (_, text) = self.entries.iter().find(|(key, _)| key == id).context(
            "Result expired or is unavailable in this chat. Rerun the source tool if appropriate.",
        )?;
        let offset = arguments["offset"]
            .as_u64()
            .unwrap_or(0)
            .min(text.len() as u64) as usize;
        let limit = arguments["limit"]
            .as_u64()
            .unwrap_or(INLINE as u64)
            .clamp(1, INLINE as u64) as usize;
        let start = utf8_start(text, offset);
        let end = utf8_end(text, (start + limit).min(text.len()));
        Ok(format!(
            "[Result {id}, bytes {start}–{end} of {}]\n{}",
            text.len(),
            &text[start..end]
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn preserves_unicode_across_cached_result_pages() -> Result<()> {
        let data = "ёж".repeat(4_000);
        let id = model_config::hash(data.as_bytes());
        let mut cache = Results::default();
        assert!(cache.inline(data.clone()).contains(&id[..16]));
        let tail = cache.read(&json!({"id": &id[..16], "offset":3_001, "limit":7}))?;
        assert!(tail.ends_with(&data[3_000..3_008]));
        Ok(())
    }

    #[test]
    fn prepared_patch_conflicts_if_file_changes_before_review() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("file.txt");
        std::fs::write(&path, "old\n")?;
        let changes = patch(
            root.path(),
            &json!({"files":[{"path":"file.txt","patch":"--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-old\n+new\n"}]}),
        )?;
        let batch = changes::Batch::prepare(root.path(), changes)?;
        std::fs::write(&path, "external edit\n")?;
        assert!(batch.apply().is_err());
        assert_eq!(std::fs::read_to_string(path)?, "external edit\n");
        Ok(())
    }
}
