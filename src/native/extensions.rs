mod runtime;

use super::{changes, model_config, permissions, ui::Screen};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
};

const MAX_PACKAGE: usize = 1024 * 1024;
const MAX_FILES: usize = 64;
const MAX_GRANTS: usize = 32;
const MANIFEST: &str = "extension.toml";

#[derive(Debug)]
pub(super) struct Canceled;

impl std::fmt::Display for Canceled {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str(tr!(
            "Запуск расширения остановлен.",
            "Extension execution stopped."
        ))
    }
}

impl std::error::Error for Canceled {}

pub(super) const GUIDE: &str = include_str!("../../docs/extensions.md");
pub(crate) use runtime::{WORKER, worker};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct Grant {
    id: String,
    workspace: Option<PathBuf>,
    revision: String,
    previous: Option<String>,
    enabled: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    format: u8,
    id: String,
    version: String,
    #[serde(default)]
    tools: Vec<Definition>,
    appearance: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Definition {
    name: String,
    description: String,
    engine: runtime::Engine,
    entry: String,
    schema: String,
    #[serde(default)]
    reads: Vec<String>,
    #[serde(default)]
    writes: Vec<String>,
    #[serde(default = "default_timeout")]
    timeout: u64,
    #[serde(default)]
    headless: bool,
    tests: Vec<TestCase>,
}

fn default_timeout() -> u64 {
    5
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TestCase {
    input: String,
    expected: String,
    #[serde(default)]
    files: BTreeMap<String, String>,
}

#[derive(Clone)]
pub(super) struct Tool {
    pub name: String,
    pub description: String,
    pub schema: Value,
    definition: Definition,
    files: Arc<BTreeMap<String, String>>,
}

struct Package {
    manifest: Manifest,
    revision: String,
    files: Arc<BTreeMap<String, String>>,
    tools: Vec<Tool>,
}

pub(super) fn tool_name(value: &str) -> bool {
    value
        .strip_prefix("ext_")
        .is_some_and(|name| identifier(name, 60))
}

fn identifier(value: &str, max: usize) -> bool {
    (1..=max).contains(&value.len())
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
}

fn package_id(value: &str) -> bool {
    identifier(value, 24) && !value.contains('_')
}

fn file_path(path: &str) -> bool {
    crate::identity::valid(Path::new(path))
        && path.len() <= 160
        && !path.contains(['\\', ':'])
        && !permissions::protected(Path::new(path))
        && path
            .split('/')
            .all(|part| !part.ends_with([' ', '.']) && !part.starts_with('.'))
}

fn checked_folder(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir() && !crate::safe_files::is_link(&metadata),
        "Expected an ordinary folder: {}",
        path.display()
    );
    Ok(())
}

fn directory(root: &Path, id: &str) -> Result<PathBuf> {
    ensure!(
        package_id(id),
        "Invalid package ID (1–24 characters: a–z, 0–9, -)"
    );
    let config = root.join(".karrycode");
    checked_folder(&config)?;
    let directory = config.join("extensions").join(id);
    checked_folder(directory.parent().unwrap())?;
    checked_folder(&directory)?;
    Ok(directory)
}

fn contents(root: &Path) -> Result<BTreeMap<String, String>> {
    checked_folder(root)?;
    let mut files = BTreeMap::new();
    let mut queue = vec![root.to_owned()];
    let mut seen = 0;
    let mut bytes = 0;
    let mut unique_paths = BTreeSet::new();
    while let Some(folder) = queue.pop() {
        for entry in fs::read_dir(folder)? {
            let path = entry?.path();
            let name = path
                .strip_prefix(root)?
                .to_str()
                .context("Non-UTF-8 package path")?
                .replace('\\', "/");
            let metadata = fs::symlink_metadata(&path)?;
            seen += 1;
            ensure!(
                seen <= MAX_FILES && file_path(&name),
                "Invalid package path or package has more than {MAX_FILES} entries: {name}"
            );
            ensure!(
                unique_paths.insert(name.to_ascii_lowercase()),
                "Conflicting package path: {name}"
            );
            ensure!(
                !crate::safe_files::is_link(&metadata),
                "Package links are not followed: {name}"
            );
            if metadata.is_dir() {
                queue.push(path);
            } else {
                ensure!(
                    metadata.is_file() && metadata.len() <= MAX_PACKAGE as u64,
                    "Expected an ordinary text file: {name}"
                );
                let mut text = String::new();
                fs::File::open(&path)?
                    .take((MAX_PACKAGE - bytes + 1) as u64)
                    .read_to_string(&mut text)?;
                bytes += text.len();
                ensure!(
                    bytes <= MAX_PACKAGE && !text.contains('\0'),
                    "Package exceeds 1 MiB or contains binary data"
                );
                files.insert(name, text);
            }
        }
    }
    Ok(files)
}

impl Package {
    fn read(root: &Path, id: &str) -> Result<Self> {
        let files = contents(root)?;
        let revision = model_config::hash(&serde_json::to_vec(&files)?);
        let manifest: Manifest =
            toml::from_str(files.get(MANIFEST).context("Missing extension.toml")?)
                .context("Invalid extension.toml")?;
        ensure!(
            manifest.format == 1 && manifest.id == id && package_id(id),
            "Invalid package ID or format"
        );
        ensure!(
            identifier(
                &manifest
                    .version
                    .to_ascii_lowercase()
                    .replace(['.', '+'], "-"),
                40
            ),
            "Invalid package version"
        );
        ensure!(manifest.tools.len() <= 12, "Use up to 12 tools per package");
        let files = Arc::new(files);
        let mut tools = Vec::new();
        let mut names = BTreeSet::new();
        for definition in &manifest.tools {
            ensure!(
                identifier(&definition.name, 24) && names.insert(&definition.name),
                "Invalid or duplicate tool name"
            );
            ensure!(
                (1..=500).contains(&definition.description.len()),
                "Tool description must contain up to 500 bytes"
            );
            ensure!(
                file_path(&definition.entry) && files.contains_key(&definition.entry),
                "Missing script: {}",
                definition.entry
            );
            ensure!(
                (1..=15).contains(&definition.timeout) && (1..=8).contains(&definition.tests.len()),
                "Each tool needs a 1–15 second timeout and 1–8 tests"
            );
            for paths in [&definition.reads, &definition.writes] {
                let unique: BTreeSet<_> =
                    paths.iter().map(|path| path.to_ascii_lowercase()).collect();
                ensure!(
                    paths.len() <= 12
                        && unique.len() == paths.len()
                        && paths.iter().all(|path| file_path(path)),
                    "Invalid declared project paths"
                );
            }
            let schema: Value =
                serde_json::from_str(&definition.schema).context("Invalid argument schema")?;
            ensure!(
                schema["type"] == "object",
                "Argument schema must describe an object"
            );
            validate_schema(&schema, 0)?;
            for test in &definition.tests {
                validate_input(&schema, &serde_json::from_str(&test.input)?)?;
                let _: runtime::Output = serde_json::from_str(&test.expected)?;
                ensure!(
                    test.files
                        .keys()
                        .all(|name| definition.reads.contains(name)),
                    "Test file is outside declared reads"
                );
            }
            tools.push(Tool {
                name: format!("ext_{id}_{}", definition.name),
                description: definition.description.clone(),
                schema,
                definition: definition.clone(),
                files: Arc::clone(&files),
            });
        }
        if let Some(path) = &manifest.appearance {
            super::appearance::Appearance::load(&files, path)?;
        }
        ensure!(
            !tools.is_empty()
                || manifest.appearance.is_some()
                || files
                    .keys()
                    .any(|name| name.starts_with("skills/") && name.ends_with("/SKILL.md")),
            "Package must contain a tool, skill or appearance"
        );
        Ok(Self {
            manifest,
            revision,
            files,
            tools,
        })
    }

    fn preview(&self, previous: Option<&Package>) -> String {
        let mut output = format!(
            "{} {} · SHA-256 {}\n",
            self.manifest.id, self.manifest.version, self.revision
        );
        for tool in &self.tools {
            let tool_access = &tool.definition;
            output.push_str(&format!(
                "\n{} · {:?} · {}s\nReads: {:?}\nProposed writes: {:?}\n",
                tool.name,
                tool_access.engine,
                tool_access.timeout,
                tool_access.reads,
                tool_access.writes
            ));
        }
        let paths: BTreeSet<_> = self
            .files
            .keys()
            .chain(
                previous
                    .into_iter()
                    .flat_map(|package| package.files.keys()),
            )
            .collect();
        for path in paths {
            let old = previous
                .and_then(|package| package.files.get(path))
                .map_or("", String::as_str);
            let new = self.files.get(path).map_or("", String::as_str);
            if previous.is_none() || old != new {
                let diff = diffy::create_patch(old, new).to_string();
                let mut visible = String::with_capacity(diff.len());
                for ch in diff.chars() {
                    if ch.is_control() && !matches!(ch, '\t' | '\n')
                        || matches!(ch, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2069}')
                    {
                        visible.extend(ch.escape_unicode());
                    } else {
                        visible.push(ch);
                    }
                }
                output.push_str(&format!("\n{path}\n{visible}"));
            }
        }
        output
    }

    fn test(&self, cancel: &AtomicBool) -> Result<String> {
        let mut results = Vec::new();
        for tool in &self.tools {
            for (index, case) in tool.definition.tests.iter().enumerate() {
                let actual = tool.run(serde_json::from_str(&case.input)?, &case.files, cancel)?;
                let expected: runtime::Output = serde_json::from_str(&case.expected)?;
                ensure!(
                    actual == expected,
                    "{} test {} failed\nExpected: {}\nObserved: {}",
                    tool.name,
                    index + 1,
                    case.expected,
                    serde_json::to_string(&actual)?
                );
                results.push(format!("{} · {}: OK", tool.name, index + 1));
            }
        }
        Ok(if results.is_empty() {
            "Data-only package validated; no executable tests.".into()
        } else {
            results.join("\n")
        })
    }
}

fn validate_schema(schema: &Value, depth: usize) -> Result<()> {
    ensure!(depth < 8, "Argument schema is too deeply nested");
    let map = schema.as_object().context("Expected a schema object")?;
    ensure!(
        map.keys().all(|name| matches!(
            name.as_str(),
            "type"
                | "properties"
                | "required"
                | "items"
                | "description"
                | "additionalProperties"
                | "enum"
        )),
        "Unsupported argument schema field"
    );
    match schema["type"].as_str() {
        Some("object") => {
            let properties = schema["properties"]
                .as_object()
                .context("Object schema requires properties")?;
            ensure!(
                properties.len() <= 24 && schema["additionalProperties"] != true,
                "Object schema is too broad"
            );
            if let Some(required) = schema.get("required") {
                ensure!(
                    required
                        .as_array()
                        .is_some_and(|values| values.iter().all(|value| value
                            .as_str()
                            .is_some_and(|key| properties.contains_key(key)))),
                    "Invalid required properties"
                );
            }
            for value in properties.values() {
                validate_schema(value, depth + 1)?;
            }
        }
        Some("array") => validate_schema(&schema["items"], depth + 1)?,
        Some("string" | "number" | "integer" | "boolean" | "null") => {}
        _ => bail!("Unsupported argument type"),
    }
    if let Some(options) = schema.get("enum") {
        ensure!(
            options
                .as_array()
                .is_some_and(|values| (1..=30).contains(&values.len())),
            "Invalid argument enum"
        );
    }
    Ok(())
}

fn validate_input(schema: &Value, input: &Value) -> Result<()> {
    let matches = match schema["type"].as_str() {
        Some("object") => input.as_object().is_some_and(|object| {
            schema["required"]
                .as_array()
                .into_iter()
                .flatten()
                .all(|key| object.contains_key(key.as_str().unwrap_or_default()))
                && object.iter().all(|(name, value)| {
                    schema["properties"]
                        .get(name)
                        .is_some_and(|rule| validate_input(rule, value).is_ok())
                })
        }),
        Some("array") => input.as_array().is_some_and(|array| {
            array.len() <= 100
                && array
                    .iter()
                    .all(|value| validate_input(&schema["items"], value).is_ok())
        }),
        Some("string") => input.is_string(),
        Some("number") => input.is_number(),
        Some("integer") => input.is_i64() || input.is_u64(),
        Some("boolean") => input.is_boolean(),
        Some("null") => input.is_null(),
        _ => false,
    };
    ensure!(
        matches
            && schema["enum"]
                .as_array()
                .is_none_or(|values| values.contains(input)),
        "Arguments do not match the declared schema"
    );
    Ok(())
}

impl Tool {
    fn run(
        &self,
        arguments: Value,
        files: &BTreeMap<String, String>,
        cancel: &AtomicBool,
    ) -> Result<runtime::Output> {
        validate_input(&self.schema, &arguments)?;
        let script = self
            .files
            .get(&self.definition.entry)
            .context("Installed script missing")?;
        let output = runtime::execute(
            self.definition.engine,
            script,
            &arguments,
            files,
            self.definition.timeout,
            cancel,
        )?;
        let mut seen = BTreeSet::new();
        for proposed in &output.changes {
            ensure!(
                self.definition.writes.contains(&proposed.path)
                    && seen.insert(proposed.path.to_ascii_lowercase()),
                "Extension proposed an undeclared or duplicate write: {}",
                proposed.path
            );
        }
        Ok(output)
    }

    pub fn call(
        &self,
        screen: &mut Screen,
        root: &Path,
        args: Value,
        undo: &mut Option<changes::Batch>,
    ) -> Result<String> {
        validate_input(&self.schema, &args)?;
        let mode = screen.permission_mode();
        if mode == permissions::Mode::Ask
            && !screen.review(
                tr!("Запустить расширение?", "Run extension?"),
                &format!(
                    "{}\n{}\n{}",
                    self.name,
                    root.display(),
                    self.definition.reads.join("\n")
                ),
            )?
        {
            return Ok("Extension call declined.".into());
        }
        let files = self
            .definition
            .reads
            .iter()
            .map(|path| Ok((path.clone(), super::read_file(root, path)?)))
            .collect::<Result<BTreeMap<_, _>>>()?;
        let before = self
            .definition
            .writes
            .iter()
            .map(|path| {
                Ok((
                    path.as_str(),
                    changes::snapshot_text(root, Path::new(path))?,
                ))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        let cancel = AtomicBool::new(false);
        let output = std::thread::scope(|scope| {
            let worker = scope.spawn(|| self.run(args, &files, &cancel));
            screen.wait_for_tool(&self.name, &cancel, worker)
        })?;
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(Canceled.into());
        }
        let output = output?;
        if output.changes.is_empty() {
            return Ok(output.text);
        }
        let edits = output
            .changes
            .into_iter()
            .map(|proposed| {
                let previous = before.get(proposed.path.as_str()).cloned().flatten();
                changes::Change::Replace {
                    path: proposed.path.into(),
                    before: previous,
                    after: Some(proposed.content),
                }
            })
            .collect();
        let outcome = super::review_changes(
            screen,
            screen.permission_mode(),
            &Value::Null,
            root,
            edits,
            undo,
        )?;
        Ok(format!("{}\n{outcome}", output.text))
    }
}

#[derive(Default)]
pub(super) struct Registry {
    packages: Vec<Package>,
    pub errors: Vec<String>,
}

fn revisions(root: &Path) -> PathBuf {
    root.join("extensions")
}

fn installed(root: &Path, id: &str, revision: &str) -> Result<Package> {
    ensure!(
        package_id(id)
            && revision.len() == 64
            && revision.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "Invalid extension receipt"
    );
    checked_folder(&revisions(root))?;
    let package = Package::read(&revisions(root).join(format!("{id}-{revision}")), id)?;
    ensure!(
        package.revision == revision,
        "Installed package bytes changed after approval: {id}"
    );
    Ok(package)
}

impl Registry {
    pub fn load(root: &Path, workspace: &Path, approved: &[Grant]) -> Self {
        let mut registry = Self::default();
        let mut ids = BTreeSet::new();
        // A project choice takes precedence over the same package from the profile.
        for grant in approved
            .iter()
            .filter(|grant| grant.workspace.as_deref() == Some(workspace))
            .chain(approved.iter().filter(|grant| grant.workspace.is_none()))
        {
            if !ids.insert(&grant.id) || !grant.enabled {
                continue;
            }
            match installed(root, &grant.id, &grant.revision) {
                Ok(mut package) => {
                    package.tools.retain(|tool| {
                        if let Err(error) = runtime::availability(tool.definition.engine) {
                            registry.errors.push(format!("{}: {error:#}", tool.name));
                            false
                        } else {
                            true
                        }
                    });
                    registry.packages.push(package);
                }
                Err(error) => registry.errors.push(format!("{error:#}")),
            }
        }
        registry
    }

    pub fn tools(&self, mode: super::RequestMode) -> Vec<Tool> {
        if mode.planning || mode.diagnose || mode.worker {
            return Vec::new();
        }
        let mut names = BTreeSet::new();
        self.packages
            .iter()
            .flat_map(|package| &package.tools)
            .filter(|tool| {
                (!mode.headless || tool.definition.headless && tool.definition.writes.is_empty())
                    && names.insert(tool.name.as_str())
            })
            .take(32)
            .cloned()
            .collect()
    }

    pub fn skills(&self, catalog: &mut super::skills::Catalog) {
        for package in &self.packages {
            catalog.add_extension(&package.manifest.id, &package.files);
        }
    }

    pub fn appearances(&self) -> Vec<(String, super::appearance::Appearance)> {
        self.packages
            .iter()
            .filter_map(|package| {
                let file = package.manifest.appearance.as_deref()?;
                super::appearance::Appearance::load(&package.files, file)
                    .ok()
                    .map(|appearance| (package.manifest.id.clone(), appearance))
            })
            .collect()
    }
}

#[derive(Default)]
pub(super) struct Creator {
    tests: BTreeMap<String, String>,
}

impl Creator {
    pub fn act(
        &mut self,
        screen: &mut Screen,
        root: &Path,
        workspace: &Path,
        grants: &mut Vec<Grant>,
        args: &Value,
    ) -> Result<(String, bool)> {
        let operation = args["operation"].as_str().unwrap_or_default();
        if operation == "guide" {
            return Ok((GUIDE.into(), false));
        }
        let id = args["id"].as_str().context("Supply a package ID")?;
        let package = Package::read(&directory(workspace, id)?, id)?;
        match operation {
            "validate" => Ok((
                format!(
                    "{} {} · valid · {}",
                    id, package.manifest.version, package.revision
                ),
                false,
            )),
            "test" => {
                if !screen.review(
                    tr!("Проверить код расширения?", "Test extension code?"),
                    &package.preview(None),
                )? {
                    return Ok(("Extension tests declined.".into(), false));
                }
                let cancel = AtomicBool::new(false);
                let results = std::thread::scope(|scope| {
                    let worker = scope.spawn(|| package.test(&cancel));
                    screen.wait_for_tool(
                        tr!("Тесты расширения", "Extension tests"),
                        &cancel,
                        worker,
                    )
                })?;
                if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                    return Err(Canceled.into());
                }
                let results = results?;
                if self.tests.len() >= MAX_GRANTS {
                    self.tests.clear();
                }
                self.tests.insert(package.revision, results.clone());
                Ok((results, false))
            }
            "install" => {
                let tests = if package.tools.is_empty() {
                    "Data-only package validated."
                } else {
                    self.tests
                        .get(&package.revision)
                        .context("Test this exact revision before installing")?
                };
                let Some(choice) = screen.choose(
                    tr!("Область расширения", "Extension scope"),
                    &[
                        tr!("Текущий проект", "Current project").into(),
                        tr!("Все проекты профиля", "All projects in this profile").into(),
                    ],
                )?
                else {
                    return Ok(("Installation declined.".into(), false));
                };
                let scope = (choice == 0).then(|| workspace.to_owned());
                let current = grants
                    .iter()
                    .position(|item| item.id == id && item.workspace == scope);
                let old =
                    current.and_then(|index| installed(root, id, &grants[index].revision).ok());
                let detail = format!(
                    "{}\n{tests}\n{}",
                    scope
                        .as_deref()
                        .map_or_else(|| "Profile".into(), |path| path.display().to_string()),
                    package.preview(old.as_ref())
                );
                if !screen.review(
                    tr!("Установить эту версию?", "Install this revision?"),
                    &detail,
                )? {
                    return Ok(("Installation declined.".into(), false));
                }
                let revisions = revisions(root);
                fs::create_dir_all(&revisions)?;
                checked_folder(&revisions)?;
                let target = revisions.join(format!("{id}-{}", package.revision));
                if !target.exists() {
                    let stage = tempfile::tempdir_in(&revisions)?;
                    for (path, content) in package.files.iter() {
                        let to = stage.path().join(path);
                        fs::create_dir_all(to.parent().unwrap())?;
                        fs::write(to, content)?;
                    }
                    fs::rename(stage.path(), &target)?;
                }
                installed(root, id, &package.revision)?;
                if let Some(index) = current {
                    let grant = &mut grants[index];
                    if grant.revision != package.revision {
                        grant.previous = Some(grant.revision.clone());
                        grant.revision = package.revision;
                    }
                    grant.enabled = true;
                } else {
                    ensure!(
                        grants.len() < MAX_GRANTS,
                        "The extension limit is {MAX_GRANTS}"
                    );
                    grants.push(Grant {
                        id: id.into(),
                        workspace: scope,
                        revision: package.revision,
                        previous: None,
                        enabled: true,
                    });
                }
                Ok((
                    format!(
                        "Installed {id} {}. Its tools become available on the next model request.",
                        package.manifest.version
                    ),
                    true,
                ))
            }
            _ => bail!("Choose guide, validate, test or install"),
        }
    }
}

pub(super) fn menu(
    screen: &mut Screen,
    root: &Path,
    workspace: &Path,
    grants: &mut [Grant],
    registry: &Registry,
) -> Result<bool> {
    let candidates = workspace.join(".karrycode/extensions");
    let labels = grants
        .iter()
        .enumerate()
        .filter(|(_, grant)| {
            grant
                .workspace
                .as_ref()
                .is_none_or(|path| path == workspace)
        })
        .map(|(index, grant)| {
            let version = installed(root, &grant.id, &grant.revision).map_or_else(
                |_| grant.revision.chars().take(12).collect(),
                |package| package.manifest.version,
            );
            (
                index,
                format!(
                    "{} · {version} · {} · {}",
                    grant.id,
                    if grant.enabled {
                        tr!("вкл.", "enabled")
                    } else {
                        tr!("выкл.", "disabled")
                    },
                    if grant.workspace.is_some() {
                        tr!("проект", "project")
                    } else {
                        tr!("профиль", "profile")
                    }
                ),
            )
        })
        .collect::<Vec<_>>();
    let mut options = labels
        .iter()
        .map(|(_, label)| label.clone())
        .collect::<Vec<_>>();
    options.push(tr!("Черновики и ошибки", "Drafts and errors").into());
    let Some(choice) = screen.choose(tr!("Расширения", "Extensions"), &options)? else {
        return Ok(false);
    };
    if choice == labels.len() {
        let mut details = registry.errors.join("\n");
        if checked_folder(&workspace.join(".karrycode")).is_ok()
            && checked_folder(&candidates).is_ok()
        {
            for entry in fs::read_dir(&candidates)?.flatten().take(MAX_GRANTS) {
                let name = entry.file_name().to_string_lossy().into_owned();
                if package_id(&name) && checked_folder(&entry.path()).is_ok() {
                    details.push_str(&format!("\n{name} · /creator"));
                }
            }
        }
        screen.report(
            &trf!("Черновики · {}", "Drafts · {}", candidates.display()),
            &details,
        )?;
        return Ok(false);
    }
    let grant = &mut grants[labels[choice].0];
    let options = [
        if grant.enabled {
            tr!("Отключить", "Disable")
        } else {
            tr!("Включить одобренную версию", "Enable approved revision")
        }
        .into(),
        tr!("Вернуть предыдущую версию", "Restore the previous revision").into(),
    ];
    match screen.choose(&grant.id, &options)? {
        Some(0) => {
            if !grant.enabled {
                let package = installed(root, &grant.id, &grant.revision)?;
                if !screen.review(
                    tr!("Включить расширение?", "Enable extension?"),
                    &package.preview(None),
                )? {
                    return Ok(false);
                }
            }
            grant.enabled = !grant.enabled;
            Ok(true)
        }
        Some(1) => {
            let previous = grant
                .previous
                .clone()
                .context("No previously approved revision")?;
            let package = installed(root, &grant.id, &previous)?;
            if !screen.review(
                tr!("Восстановить версию?", "Restore revision?"),
                &package.preview(None),
            )? {
                return Ok(false);
            }
            grant.previous = Some(std::mem::replace(&mut grant.revision, previous));
            grant.enabled = true;
            Ok(true)
        }
        _ => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn approval_requires_exact_snapshot_and_scope() -> Result<()> {
        let root = tempfile::tempdir()?;
        let workspace = tempfile::tempdir()?;
        let files: BTreeMap<String, String> = BTreeMap::from([
            (
                "extension.toml".into(),
                "format=1\nid='words'\nversion='1.0.0'\n".into(),
            ),
            (
                "skills/count/SKILL.md".into(),
                "---\nname: count\n---\nCount words.".into(),
            ),
        ]);
        let revision = model_config::hash(&serde_json::to_vec(&files)?);
        let target = revisions(root.path()).join(format!("words-{revision}"));
        fs::create_dir_all(target.join("skills/count"))?;
        for (path, content) in &files {
            fs::write(target.join(path), content)?;
        }
        let grant = Grant {
            id: "words".into(),
            workspace: Some(workspace.path().into()),
            revision,
            previous: None,
            enabled: true,
        };
        assert_eq!(
            Registry::load(root.path(), workspace.path(), std::slice::from_ref(&grant))
                .packages
                .len(),
            1
        );
        assert!(
            Registry::load(root.path(), root.path(), std::slice::from_ref(&grant))
                .packages
                .is_empty()
        );
        fs::write(target.join("skills/count/SKILL.md"), "Changed after review")?;
        let registry = Registry::load(root.path(), workspace.path(), &[grant]);
        assert!(registry.packages.is_empty());
        assert_eq!(registry.errors.len(), 1);
        Ok(())
    }

    #[test]
    fn schema_checks_types_required_fields_and_extra_arguments() -> Result<()> {
        let schema =
            json!({"type":"object","properties":{"name":{"type":"string"}},"required":["name"]});
        validate_schema(&schema, 0)?;
        validate_input(&schema, &json!({"name":"sample"}))?;
        assert!(validate_input(&schema, &json!({"name":3})).is_err());
        assert!(validate_input(&schema, &json!({"name":"sample", "extra":true})).is_err());
        Ok(())
    }
}
