//! Agent skills in the Claude Code `SKILL.md` format.
//!
//! A skill is a directory with `SKILL.md` whose YAML front matter carries a
//! `name` and a `description`; the rest is the instruction body that the model
//! loads on demand. KarryCode reads its own folders plus the shared folders
//! used by Claude Code, Codex and the DSH agent set.

use anyhow::{Context, Result, ensure};
use std::{
    fs,
    path::{Path, PathBuf},
};

const MAX_FILE: u64 = 256 * 1024;
const MAX_BODY: usize = 64 * 1024;
const MAX_DESCRIPTION: usize = 400;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    Project,
    Bundle,
    User,
    Shared,
    Claude,
    Codex,
    Builtin,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Self::Project => tr!("проект", "project"),
            Self::Bundle => tr!("комплект", "bundle"),
            Self::User => tr!("пользователь", "user"),
            Self::Shared => tr!("общие", "shared"),
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
            Self::Builtin => tr!("встроенный", "builtin"),
        }
    }
}

#[derive(Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub source: Source,
    path: Option<PathBuf>,
    builtin: Option<&'static str>,
}

pub struct Catalog {
    pub skills: Vec<Skill>,
}

impl Catalog {
    pub fn find(&self, name: &str) -> Option<&Skill> {
        self.skills
            .iter()
            .find(|skill| skill.name.eq_ignore_ascii_case(name.trim()))
    }

    pub fn get(&self, index: usize) -> Option<&Skill> {
        self.skills.get(index)
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }
}

struct Builtin {
    name: &'static str,
    body: &'static str,
}

const BUILTINS: &[Builtin] = &[
    Builtin {
        name: "create-project",
        body: include_str!("../../skills/create-project/SKILL.md"),
    },
    Builtin {
        name: "implement-feature",
        body: include_str!("../../skills/implement-feature/SKILL.md"),
    },
    Builtin {
        name: "write-tests",
        body: include_str!("../../skills/write-tests/SKILL.md"),
    },
    Builtin {
        name: "run-and-verify",
        body: include_str!("../../skills/run-and-verify/SKILL.md"),
    },
    Builtin {
        name: "browser-check",
        body: include_str!("../../skills/browser-check/SKILL.md"),
    },
    Builtin {
        name: "desktop-check",
        body: include_str!("../../skills/desktop-check/SKILL.md"),
    },
    Builtin {
        name: "project-runbook",
        body: include_str!("../../skills/project-runbook/SKILL.md"),
    },
    Builtin {
        name: "simplify",
        body: include_str!("../../skills/simplify/SKILL.md"),
    },
    Builtin {
        name: "research-docs",
        body: include_str!("../../skills/research-docs/SKILL.md"),
    },
    Builtin {
        name: "report-issue",
        body: include_str!("../../skills/report-issue/SKILL.md"),
    },
    Builtin {
        name: "code-review",
        body: include_str!("../../skills/code-review/SKILL.md"),
    },
    Builtin {
        name: "debug-project",
        body: include_str!("../../skills/debug-project/SKILL.md"),
    },
    Builtin {
        name: "skill-writer",
        body: include_str!("../../skills/skill-writer/SKILL.md"),
    },
];

/// Collects skills from the project, the bundle, the user profile and the
/// other agents' folders. The first folder that provides a name wins.
pub fn discover(directory: &Path, bundle: &Path) -> Catalog {
    let home = crate::platform::home();
    let roots = [
        (directory.join(".karrycode/skills"), Source::Project),
        (directory.join(".opencode/skills"), Source::Project),
        (directory.join(".claude/skills"), Source::Project),
        (directory.join(".agents/skills"), Source::Project),
        (bundle.join("skills"), Source::Bundle),
        (home.join(".karrycode/skills"), Source::User),
        (home.join(".agents/skills"), Source::Shared),
        (home.join(".claude/skills"), Source::Claude),
        (home.join(".config/opencode/skills"), Source::Shared),
        (home.join(".codex/skills"), Source::Codex),
    ];
    let mut skills: Vec<Skill> = Vec::new();
    for (root, source) in roots {
        for skill in read_root(&root, source) {
            let duplicate = skills
                .iter()
                .any(|existing| existing.name.eq_ignore_ascii_case(&skill.name));
            if !duplicate {
                skills.push(skill);
            }
        }
    }
    for builtin in BUILTINS {
        let parsed = parse(builtin.body, Path::new(""), builtin.name)
            .unwrap_or_else(|_| fallback(builtin.name, Source::Builtin));
        let duplicate = skills
            .iter()
            .any(|existing| existing.name.eq_ignore_ascii_case(&parsed.name));
        if !duplicate {
            skills.push(Skill {
                builtin: Some(builtin.body),
                path: None,
                source: Source::Builtin,
                ..parsed
            });
        }
    }
    Catalog { skills }
}

fn read_root(root: &Path, source: Source) -> Vec<Skill> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut folders: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            !name.starts_with('.')
                && entry
                    .file_type()
                    .map(|kind| kind.is_dir() && !kind.is_symlink())
                    .unwrap_or(false)
        })
        .map(|entry| entry.path())
        .filter(|path| path.join("SKILL.md").is_file())
        .collect();
    folders.sort();
    folders
        .into_iter()
        .filter_map(|folder| {
            let name = folder
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            let text = read_text(&folder.join("SKILL.md")).ok()?;
            let mut skill = parse(&text, &folder, &name).ok()?;
            skill.source = source;
            Some(skill)
        })
        .collect()
}

pub fn parse(text: &str, path: &Path, fallback_name: &str) -> Result<Skill> {
    let (meta, body) = split_front_matter(text);
    let name = meta
        .iter()
        .find(|(key, _)| key == "name")
        .map(|(_, value)| value.clone())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| fallback_name.to_owned());
    let description = meta
        .iter()
        .find(|(key, _)| key == "description")
        .map(|(_, value)| value.clone())
        .unwrap_or_default();
    ensure!(!name.is_empty(), "Skill name is empty");
    let name: String = name
        .chars()
        .filter(|character| !character.is_control())
        .take(96)
        .collect();
    let description: String = description
        .chars()
        .filter(|character| !character.is_control() || *character == ' ')
        .take(MAX_DESCRIPTION)
        .collect();
    ensure!(body.len() <= MAX_BODY, "Skill is larger than 64 KiB");
    Ok(Skill {
        name,
        description: description.trim().to_owned(),
        source: Source::Project,
        path: (!path.as_os_str().is_empty()).then(|| path.to_owned()),
        builtin: None,
    })
}

fn split_front_matter(text: &str) -> (Vec<(String, String)>, String) {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some((first, rest)) = text.split_once('\n') else {
        return (Vec::new(), text.to_owned());
    };
    if first.trim() != "---" {
        return (Vec::new(), text.to_owned());
    }
    let mut meta = Vec::new();
    let mut offset = first.len() + 1;
    for line in rest.split_inclusive('\n') {
        offset += line.len();
        let trimmed = line.trim_end_matches(['\r', '\n']).trim();
        if trimmed == "---" {
            let body = text.get(offset..).unwrap_or("").trim_start().to_owned();
            return (meta, body);
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() || line.starts_with(' ') {
            continue;
        }
        meta.push((key.to_owned(), unquote(value.trim())));
    }
    (Vec::new(), text.to_owned())
}

fn unquote(value: &str) -> String {
    let value = value.trim();
    let value = value
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .or_else(|| {
            value
                .strip_prefix('\'')
                .and_then(|rest| rest.strip_suffix('\''))
        })
        .unwrap_or(value);
    value.replace("\\\"", "\"").replace("\\\\", "\\")
}

fn fallback(name: &str, source: Source) -> Skill {
    Skill {
        name: name.to_owned(),
        description: String::new(),
        source,
        path: None,
        builtin: None,
    }
}

fn read_text(path: &Path) -> Result<String> {
    let meta = fs::metadata(path)?;
    ensure!(
        meta.len() <= MAX_FILE,
        tr!(
            "Файл скилла больше 256 КиБ",
            "Skill file is larger than 256 KiB"
        )
    );
    fs::read_to_string(path).with_context(|| {
        trf!(
            "Не удалось прочитать {}",
            "Could not read {}",
            path.display()
        )
    })
}

/// Full instruction body for a skill; built-ins stay inside the binary.
pub fn resource(skill: &Skill, relative: &str) -> Result<String> {
    let base = skill.path.as_deref().and_then(Path::parent).context(tr!(
        "У этого скилла нет дополнительных файлов",
        "This skill has no companion files"
    ))?;
    super::files::read_file(base, relative)
}

pub fn body(skill: &Skill) -> Result<String> {
    if let Some(builtin) = skill.builtin {
        let (_, body) = split_front_matter(builtin);
        return Ok(body);
    }
    let path = skill
        .path
        .as_ref()
        .context(tr!("У скилла нет файла", "Skill has no file"))?
        .join("SKILL.md");
    let text = read_text(&path)?;
    let (_, body) = split_front_matter(&text);
    Ok(body.chars().take(MAX_BODY).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn front_matter_is_parsed_with_quotes_and_colons() {
        let text = "---\nname: \"demo\"\ndescription: \"Use when: the thing\"\nmetadata:\n  version: 1\n---\n\nBody line\n";
        let skill = parse(text, Path::new(""), "fallback").expect("skill");
        assert_eq!(skill.name, "demo");
        assert_eq!(skill.description, "Use when: the thing");
        let body = split_front_matter(text).1;
        assert_eq!(body, "Body line\n");
    }

    #[test]
    fn a_skill_without_front_matter_still_loads() {
        let skill = parse("# Title\nText", Path::new(""), "plain").expect("skill");
        assert_eq!(skill.name, "plain");
        assert_eq!(skill.description, "");
    }

    #[test]
    fn built_in_skills_are_always_available() {
        let root = tempfile::tempdir().expect("temp");
        let catalog = discover(root.path(), root.path());
        for name in ["code-review", "debug-project", "skill-writer"] {
            assert!(catalog.find(name).is_some(), "missing {name}");
        }
        let skill = catalog.find("code-review").expect("skill");
        assert!(body(skill).expect("body").contains("KarryCode"));
    }

    #[test]
    fn project_skills_shadow_built_ins_and_duplicates() {
        let project = tempfile::tempdir().expect("project");
        let folder = project.path().join(".karrycode/skills/code-review");
        fs::create_dir_all(&folder).expect("dirs");
        fs::write(
            folder.join("SKILL.md"),
            "---\nname: code-review\ndescription: Project review\n---\nProject body\n",
        )
        .expect("write");
        let catalog = discover(project.path(), project.path());
        let matches: Vec<_> = catalog
            .skills
            .iter()
            .filter(|skill| skill.name == "code-review")
            .collect();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].source, Source::Project);
        assert_eq!(body(matches[0]).expect("body"), "Project body\n");
    }

    #[test]
    fn hidden_and_broken_skill_folders_are_skipped() {
        let project = tempfile::tempdir().expect("project");
        let root = project.path().join(".karrycode/skills");
        fs::create_dir_all(root.join(".system")).expect("dirs");
        fs::create_dir_all(root.join("broken")).expect("dirs");
        fs::write(root.join(".system/SKILL.md"), "---\nname: hidden\n---\nx").expect("write");
        fs::write(root.join("broken/SKILL.md"), "no front matter").expect("write");
        let catalog = discover(project.path(), project.path());
        assert!(catalog.find("hidden").is_none());
        assert!(catalog.find("broken").is_some());
    }
}
