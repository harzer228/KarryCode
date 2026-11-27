use super::{extensions, ui::Screen, work_plan::WorkPlan};
use anyhow::{Context, Result, ensure};
use crossterm::style::Color;
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use unicode_width::UnicodeWidthStr as _;

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Border {
    #[default]
    Rounded,
    Square,
    Ascii,
}

pub(super) struct Frame {
    pub tl: &'static str,
    pub tr: &'static str,
    pub bl: &'static str,
    pub br: &'static str,
    pub line: &'static str,
    pub side: &'static str,
}

impl Border {
    pub fn frame(self) -> Frame {
        match self {
            Self::Rounded => Frame {
                tl: "╭",
                tr: "╮",
                bl: "╰",
                br: "╯",
                line: "─",
                side: "│",
            },
            Self::Square => Frame {
                tl: "┌",
                tr: "┐",
                bl: "└",
                br: "┘",
                line: "─",
                side: "│",
            },
            Self::Ascii => Frame {
                tl: "+",
                tr: "+",
                bl: "+",
                br: "+",
                line: "-",
                side: "|",
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Header {
    #[default]
    Full,
    Compact,
    Hidden,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct Layout {
    pub header: Header,
    pub border: Border,
    pub compact_status: bool,
    pub compact_spacing: bool,
    pub input_lines: usize,
    pub max_width: usize,
    pub input_inset: usize,
    pub logo: Vec<String>,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            header: Header::Full,
            border: Border::Rounded,
            compact_status: false,
            compact_spacing: false,
            input_lines: 3,
            max_width: 120,
            input_inset: 0,
            logo: vec![],
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Package {
    name: String,
    palette: BTreeMap<String, String>,
    layout: Layout,
    locale: Option<String>,
    panels: Vec<Panel>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Locale {
    name: String,
    messages: BTreeMap<String, String>,
}

#[derive(Clone, Debug)]
pub(super) struct Appearance {
    config: Package,
    locale: Option<Locale>,
    pub palette: [Option<Color>; 7],
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Panel {
    id: String,
    title: String,
    tabs: Vec<Tab>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Tab {
    title: String,
    widgets: Vec<Widget>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Widget {
    Text {
        text: String,
    },
    List {
        items: Vec<String>,
    },
    Table {
        rows: Vec<Vec<String>>,
    },
    Plan,
    LastCheck,
    Progress {
        label: String,
        percent: u8,
    },
    Button {
        label: String,
        command: Option<String>,
        tool: Option<String>,
        arguments: Option<Value>,
    },
}

pub(super) enum PanelAction {
    Command(String),
    Tool(String, Value),
}

fn label(text: &str, columns: usize) -> bool {
    !text.is_empty()
        && !text.chars().any(char::is_control)
        && text.width() <= columns
        && !text
            .chars()
            .any(|ch| matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
}

const ROLES: [&str; 7] = [
    "brand",
    "foreground",
    "muted",
    "border",
    "code_background",
    "code_text",
    "warning",
];
const COMMANDS: &[&str] = &[
    "/tasks",
    "/tools",
    "/status",
    "/extensions",
    "/config",
    "/skills",
    "/help",
    "/setting",
    "/theme",
];

impl Appearance {
    pub fn load(files: &BTreeMap<String, String>, name: &str) -> Result<Self> {
        let config: Package = toml::from_str(files.get(name).context("Appearance file missing")?)?;
        ensure!(label(&config.name, 46), "Appearance needs a short name");
        let layout = &config.layout;
        ensure!(
            (1..=6).contains(&layout.input_lines)
                && (68..=180).contains(&layout.max_width)
                && layout.input_inset <= 4
                && layout.logo.len() <= 8
                && layout.logo.iter().all(|line| label(line, 16)),
            "Invalid console layout bounds"
        );
        let mut palette = [None; 7];
        for (role, hex) in &config.palette {
            let index = ROLES
                .iter()
                .position(|name| name == role)
                .context("Unknown palette color role")?;
            let raw = hex
                .strip_prefix('#')
                .context("Use #RRGGBB palette colors")?;
            ensure!(
                raw.len() == 6 && raw.bytes().all(|ch| ch.is_ascii_hexdigit()),
                "Use #RRGGBB palette colors"
            );
            let rgb = u32::from_str_radix(raw, 16)?;
            palette[index] = Some(Color::Rgb {
                r: (rgb >> 16) as u8,
                g: (rgb >> 8) as u8,
                b: rgb as u8,
            });
        }
        let locale = config
            .locale
            .as_ref()
            .map(|name| -> Result<Locale> {
                let dictionary: Locale =
                    toml::from_str(files.get(name).context("Locale file missing")?)?;
                ensure!(label(&dictionary.name, 46), "Locale needs a short name");
                let keys: BTreeMap<_, _> = crate::language::MESSAGES
                    .iter()
                    .map(|&(source, formatted)| {
                        (crate::language::message_id(source), (source, formatted))
                    })
                    .collect();
                ensure!(
                    dictionary.messages.len() <= keys.len()
                        && dictionary.messages.values().map(String::len).sum::<usize>()
                            <= 128 * 1024,
                    "Locale is too large"
                );
                for (id, translated) in &dictionary.messages {
                    let &(source, formatted) = keys
                        .get(id)
                        .with_context(|| format!("Unknown message ID {id}"))?;
                    ensure!(
                        crate::language::validate_translation(source, translated, formatted),
                        "Invalid placeholders or text width for {id}"
                    );
                }
                Ok(dictionary)
            })
            .transpose()?;
        let mut ids = BTreeSet::new();
        ensure!(config.panels.len() <= 8, "Use up to eight panels");
        for panel in &config.panels {
            ensure!(
                ids.insert(&panel.id)
                    && (1..=32).contains(&panel.id.len())
                    && panel
                        .id
                        .bytes()
                        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == b'-')
                    && label(&panel.title, 60)
                    && (1..=8).contains(&panel.tabs.len()),
                "Invalid panel ID, title or tabs"
            );
            for tab in &panel.tabs {
                ensure!(
                    label(&tab.title, 60) && tab.widgets.len() <= 24,
                    "Invalid panel tab"
                );
                for widget in &tab.widgets {
                    ensure!(widget.valid(), "Invalid widget in panel {}", panel.id);
                }
            }
        }
        Ok(Self {
            config,
            locale,
            palette,
        })
    }

    pub fn name(&self) -> &str {
        &self.config.name
    }

    pub fn layout(&self) -> &Layout {
        &self.config.layout
    }

    pub fn apply_language(&self) -> Result<()> {
        crate::language::set_dictionary(self.locale.as_ref().map(|locale| &locale.messages))
    }

    pub fn has_locale(&self) -> bool {
        self.locale.is_some()
    }

    fn description(&self) -> String {
        let layout = self.layout();
        let language = self
            .locale
            .as_ref()
            .map_or(tr!("встроенный", "built-in"), |locale| {
                locale.name.as_str()
            });
        let panels = self
            .config
            .panels
            .iter()
            .map(|panel| panel.title.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        trf!(
            "{}\nЯзык · {language}\nПоле ввода · до {} строк\nШирина · до {} колонок\nПанели · {panels}",
            "{}\nLanguage · {language}\nInput · up to {} rows\nWidth · up to {} columns\nPanels · {panels}",
            self.name(),
            layout.input_lines,
            layout.max_width
        )
    }
}

impl Widget {
    fn valid(&self) -> bool {
        match self {
            Self::Text { text } => {
                text.len() <= 4096 && text.lines().all(|line| label(line, 160) || line.is_empty())
            }
            Self::List { items } => items.len() <= 24 && items.iter().all(|text| label(text, 120)),
            Self::Table { rows } => {
                rows.len() <= 24
                    && rows.iter().all(|row| {
                        (1..=6).contains(&row.len()) && row.iter().all(|text| label(text, 42))
                    })
            }
            Self::Plan | Self::LastCheck => true,
            Self::Progress {
                label: name,
                percent,
            } => label(name, 60) && *percent <= 100,
            Self::Button {
                label: name,
                command: Some(command),
                tool: None,
                arguments: None,
            } => label(name, 60) && COMMANDS.contains(&command.as_str()),
            Self::Button {
                label: name,
                command: None,
                tool: Some(tool),
                arguments,
            } => {
                label(name, 60)
                    && extensions::tool_name(tool)
                    && arguments.as_ref().is_none_or(Value::is_object)
            }
            Self::Button { .. } => false,
        }
    }
}

pub(super) fn choose(
    screen: &mut Screen,
    packs: &[(String, Appearance)],
    applied: Option<&str>,
) -> Result<Option<Option<String>>> {
    let mut options = vec![
        tr!("Встроенные темы", "Built-in themes").into(),
        tr!("Словарь интерфейса", "Interface strings").into(),
    ];
    options.extend(packs.iter().map(|(id, pack)| {
        format!(
            "{} {}",
            if Some(id.as_str()) == applied {
                "●"
            } else {
                " "
            },
            pack.name()
        )
    }));
    match screen.choose(
        tr!("Оформление и язык", "Appearance and language"),
        &options,
    )? {
        None => Ok(None),
        Some(0) => Ok(Some(None)),
        Some(1) => {
            let text = crate::language::MESSAGES
                .iter()
                .map(|(source, _)| {
                    format!("\"{}\" = {:?}", crate::language::message_id(source), source)
                })
                .collect::<Vec<_>>()
                .join("\n");
            screen.report(tr!("ID строк интерфейса", "Interface string IDs"), &text)?;
            Ok(None)
        }
        Some(index) => {
            let (id, pack) = &packs[index - 2];
            if screen.preview_appearance(pack, &pack.description())? {
                Ok(Some(Some(id.clone())))
            } else {
                Ok(None)
            }
        }
    }
}

pub(super) fn panel(
    screen: &mut Screen,
    packs: &[(String, Appearance)],
    requested: &str,
    plan: &WorkPlan,
    last_check: Option<&str>,
) -> Result<Option<PanelAction>> {
    let panels: Vec<_> = packs
        .iter()
        .flat_map(|(id, pack)| {
            pack.config
                .panels
                .iter()
                .map(move |panel| (format!("{id}/{}", panel.id), panel))
        })
        .collect();
    if panels.is_empty() {
        screen.notice(tr!("Нет установленных панелей.", "No installed panels."));
        return Ok(None);
    }
    let panel = if requested.is_empty() {
        let options: Vec<_> = panels
            .iter()
            .map(|(id, panel)| format!("{} · {id}", panel.title))
            .collect();
        let Some(choice) = screen.choose(tr!("Панели", "Panels"), &options)? else {
            return Ok(None);
        };
        panels[choice].1
    } else {
        panels
            .iter()
            .find(|(id, _)| id == requested)
            .context("Panel not found")?
            .1
    };
    loop {
        let tabs: Vec<_> = panel.tabs.iter().map(|tab| tab.title.clone()).collect();
        let index = if tabs.len() == 1 {
            0
        } else {
            let Some(index) = screen.choose(&panel.title, &tabs)? else {
                return Ok(None);
            };
            index
        };
        let tab = &panel.tabs[index];
        let mut content = Vec::new();
        let mut buttons = Vec::new();
        for widget in &tab.widgets {
            match widget {
                Widget::Text { text } => content.push(text.clone()),
                Widget::List { items } => {
                    content.extend(items.iter().map(|item| format!("  · {item}")))
                }
                Widget::Table { rows } => content.extend(rows.iter().map(|row| row.join("  │  "))),
                Widget::Plan => content.push(plan.render()),
                Widget::LastCheck => content.push(
                    last_check
                        .unwrap_or(tr!("Проверок пока нет.", "No checks yet."))
                        .into(),
                ),
                Widget::Progress { label, percent } => {
                    content.push(format!("{label} · {percent}%"))
                }
                Widget::Button {
                    label,
                    command,
                    tool,
                    arguments,
                } => {
                    if let Some(command) = command {
                        buttons.push((label.clone(), PanelAction::Command(command.clone())));
                    } else if let Some(tool) = tool {
                        buttons.push((
                            label.clone(),
                            PanelAction::Tool(
                                tool.clone(),
                                arguments.clone().unwrap_or_else(|| serde_json::json!({})),
                            ),
                        ));
                    }
                }
            }
        }
        let choices = buttons
            .iter()
            .map(|(label, _)| label.clone())
            .collect::<Vec<_>>();
        if let Some(index) = screen.custom_panel(&tab.title, &content.join("\n"), &choices)? {
            return Ok(Some(buttons.swap_remove(index).1));
        }
        if tabs.len() == 1 {
            return Ok(None);
        }
    }
}
