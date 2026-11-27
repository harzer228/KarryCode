use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{
    cell::RefCell,
    collections::{BTreeMap, HashSet},
    fs::File,
    io::Read,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

thread_local! {
    static CUSTOM: RefCell<Option<BTreeMap<u64, &'static str>>> = const { RefCell::new(None) };
    // tr! returns a static string throughout the renderer. Reuse existing strings
    // and cap the pool so repeatedly switching or editing a pack stays bounded.
    static STRINGS: RefCell<(usize, HashSet<&'static str>)> = RefCell::new((0, HashSet::new()));
}

pub fn message_id(english: &str) -> String {
    format!("ui.{:016x}", message_key(english))
}

fn message_key(english: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in english.bytes() {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
    }
    hash
}

pub fn set_dictionary(dictionary: Option<&BTreeMap<String, String>>) -> Result<()> {
    let translated = dictionary
        .map(|dictionary| {
            STRINGS.with_borrow_mut(|(bytes, strings)| {
                let mut approved = BTreeMap::new();
                for (key, value) in dictionary {
                    let text = match strings.get(value.as_str()) {
                        Some(&existing) => existing,
                        None => {
                            anyhow::ensure!(
                                *bytes + value.len() <= 2 * 1024 * 1024,
                                "Locale cache reached its limit. Restart KarryCode."
                            );
                            *bytes += value.len();
                            let text: &'static str = Box::leak(value.clone().into_boxed_str());
                            strings.insert(text);
                            text
                        }
                    };
                    let key = key
                        .strip_prefix("ui.")
                        .and_then(|key| u64::from_str_radix(key, 16).ok())
                        .ok_or_else(|| anyhow::anyhow!("Invalid message ID {key}"))?;
                    approved.insert(key, text);
                }
                Ok::<_, anyhow::Error>(approved)
            })
        })
        .transpose()?;
    CUSTOM.replace(translated);
    Ok(())
}

pub fn custom() -> bool {
    CUSTOM.with_borrow(Option::is_some)
}

pub fn translate(ru: &'static str, en: &'static str) -> &'static str {
    CUSTOM.with_borrow(|dictionary| match dictionary {
        Some(dictionary) => dictionary.get(&message_key(en)).copied().unwrap_or(en),
        None if english() => en,
        None => ru,
    })
}

#[derive(Clone, Debug)]
pub struct Template {
    literals: Vec<String>,
    fields: Vec<String>,
}

pub fn template(text: &str) -> Option<Template> {
    let mut fields = Vec::new();
    let mut literals = Vec::new();
    let mut remaining = text;
    let mut literal = String::new();
    while let Some(index) = remaining.find(['{', '}']) {
        literal.push_str(&remaining[..index]);
        let control = remaining.as_bytes()[index];
        remaining = &remaining[index + 1..];
        if remaining.as_bytes().first() == Some(&control) {
            literal.push(char::from(control));
            remaining = &remaining[1..];
        } else if control == b'{' {
            let (field, rest) = remaining.split_once('}')?;
            if field.contains('{') {
                return None;
            }
            literals.push(std::mem::take(&mut literal));
            fields.push(field.to_owned());
            remaining = rest;
        } else {
            return None;
        }
    }
    literal.push_str(remaining);
    literals.push(literal);
    Some(Template { literals, fields })
}

pub fn validate_translation(source: &str, translation: &str, formatted: bool) -> bool {
    use unicode_width::UnicodeWidthStr as _;
    if translation.is_empty()
        || translation.len() > 4096
        || translation.chars().any(|ch| {
            ch.is_control() && !matches!(ch, '\n' | '\t')
                || matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        || translation.lines().any(|line| line.width() > 240)
    {
        return false;
    }
    if !formatted {
        return true;
    }
    let Some(original) = template(source) else {
        return false;
    };
    let Some(translated) = template(translation) else {
        return false;
    };
    original.fields == translated.fields
}

pub fn format_translation(original: &str, translated: &str, rendered: String) -> String {
    if original == translated {
        return rendered;
    }
    let Some(source) = template(original) else {
        return rendered;
    };
    let Some(target) = template(translated) else {
        return rendered;
    };
    if source.fields != target.fields {
        return rendered;
    }
    if source.fields.is_empty() {
        return target.literals[0].clone();
    }
    let Some(mut remainder) = rendered
        .strip_prefix(&source.literals[0])
        .and_then(|text| text.strip_suffix(source.literals.last().unwrap()))
    else {
        return rendered;
    };
    let mut replacement = target.literals[0].clone();
    for (index, delimiter) in source
        .literals
        .iter()
        .skip(1)
        .take(source.fields.len() - 1)
        .enumerate()
    {
        if delimiter.is_empty() {
            return rendered;
        }
        let Some((argument, rest)) = remainder.split_once(delimiter) else {
            return rendered;
        };
        replacement.push_str(argument);
        replacement.push_str(&target.literals[index + 1]);
        remainder = rest;
    }
    replacement.push_str(remainder);
    replacement.push_str(target.literals.last().unwrap());
    replacement
}

pub const MESSAGES: &[(&str, bool)] = include!(concat!(env!("OUT_DIR"), "/messages.rs"));

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dictionaries_preserve_placeholders_and_fall_back_to_english() -> Result<()> {
        let message = "Processed {count} items in {}";
        let translation = "Обработано {count} шт. за {}";
        assert!(validate_translation(message, translation, true));
        assert!(!validate_translation(message, "Missing {}", true));
        assert!(!validate_translation(message, "\u{1b}[2J{count} {}", true));
        assert_eq!(
            format_translation(message, translation, "Processed 3 items in 2s".into()),
            "Обработано 3 шт. за 2s"
        );
        set_dictionary(Some(&BTreeMap::from([(
            message_id("Close"),
            "Fermer".into(),
        )])))?;
        assert_eq!(translate("Закрыть", "Close"), "Fermer");
        assert_eq!(translate("Открыть", "Open"), "Open");
        set_dictionary(None)?;
        Ok(())
    }
}
