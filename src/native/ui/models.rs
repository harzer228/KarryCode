use super::{Editor, Line, Screen, Style, normalize_key, read_ui_event, render, text_key};
use crate::native::{
    Reasoning, endpoint,
    models::{
        CatalogModel, Connection, MAX_MODEL_TOKENS, ModelSettings, Protocol, ReasoningLevel,
        SavedModel, valid_effort,
    },
};
use anyhow::Result;
use crossterm::{
    event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    terminal,
};
use unicode_width::UnicodeWidthStr;

pub(in crate::native) enum ModelAction {
    Add,
    Catalog,
    Activate(usize),
    Edit(usize),
    Delete(usize),
    Auto(usize),
    Test(usize),
    Close,
}

pub(in crate::native) enum ModelEdit {
    Save(ModelSettings),
    Auto(ModelSettings),
    Catalog(ModelSettings),
    Cancel,
}

enum Field<'a> {
    Text(&'a Editor, &'a str),
    Secret(&'a Editor, &'a str),
    Choice(String),
}

fn cycle_index(length: usize, current: usize, left: bool) -> usize {
    if length == 0 {
        return 0;
    }
    if left {
        (current + length - 1) % length
    } else {
        (current + 1) % length
    }
}

fn cycle_vision(current: Option<bool>, left: bool) -> Option<bool> {
    let values = [None, Some(true), Some(false)];
    let index = values
        .iter()
        .position(|&value| value == current)
        .unwrap_or_default();
    values[cycle_index(values.len(), index, left)]
}

fn marked(text: &str, selected: bool) -> Line {
    let mut line = Line::of(if selected { " › " } else { "   " }, Style::brand());
    line.push(
        text,
        if selected {
            Style::brand().bold()
        } else {
            Style::fg(render::FG)
        },
    );
    line
}

fn panel_width() -> Result<usize> {
    let width = usize::from(terminal::size()?.0);
    Ok(width
        .saturating_sub(8)
        .clamp(36, 96)
        .min(width)
        .saturating_sub(4))
}

fn form_lines(
    fields: &[(&str, Field<'_>)],
    focus: usize,
    width: usize,
) -> (Vec<Line>, Option<(usize, usize)>) {
    let label_width = if width >= 52 { 17 } else { 13 };
    let value_width = width.saturating_sub(label_width + 2).max(1);
    let mut cursor = None;
    let lines = fields
        .iter()
        .enumerate()
        .map(|(row, (label, field))| {
            let selected = row == focus;
            let color = if selected {
                Style::brand().bold()
            } else {
                Style::dim()
            };
            let mut line = Line::of(" ", Style::plain());
            let mut name = Line::of(*label, color).truncate(label_width);
            name.pad_to(label_width + 1, Style::plain());
            line.extend(name);
            match field {
                Field::Text(editor, empty) | Field::Secret(editor, empty) => {
                    let (mut text, caret) = editor.view(value_width);
                    if matches!(field, Field::Secret(..)) {
                        text = "•".repeat(text.width());
                    }
                    if selected {
                        cursor = Some((row, label_width + 2 + caret));
                    }
                    if text.is_empty() {
                        line.push(*empty, Style::dim());
                    } else {
                        line.push(text, Style::fg(render::FG));
                    }
                }
                Field::Choice(text) => line.push(
                    text.as_str(),
                    if selected {
                        Style::brand()
                    } else {
                        Style::fg(render::FG)
                    },
                ),
            }
            line
        })
        .collect();
    (lines, cursor)
}

fn visible_rows() -> Result<usize> {
    Ok(usize::from(terminal::size()?.1).saturating_sub(4).max(2))
}

fn scroll_to(row: usize, current: usize, lines: usize, visible: usize) -> usize {
    current
        .min(row)
        .max(row.saturating_add(1).saturating_sub(visible))
        .min(lines.saturating_sub(visible))
}

fn edit(editor: &mut Editor, key: KeyEvent, number: bool) {
    match key.code {
        KeyCode::Left => editor.left(),
        KeyCode::Right => editor.right(),
        KeyCode::Home => editor.home(),
        KeyCode::End => editor.end(),
        KeyCode::Backspace => editor.backspace(),
        KeyCode::Delete => editor.delete(),
        KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => editor.clear(),
        KeyCode::Char(c) if text_key(key) && !c.is_control() && (!number || c.is_ascii_digit()) => {
            editor.insert([c]);
        }
        _ => {}
    }
}

fn paste(editor: &mut Editor, value: &str, number: bool) {
    editor.insert(
        value
            .chars()
            .filter(|c| !c.is_control() && (!number || c.is_ascii_digit())),
    );
}

fn number(editor: &Editor, label: &str, minimum: u32) -> std::result::Result<u32, String> {
    let text = editor.text();
    if text.trim().is_empty() {
        return Ok(0);
    }
    text.trim()
        .parse::<u32>()
        .ok()
        .filter(|n| (minimum..=MAX_MODEL_TOKENS).contains(n))
        .ok_or_else(|| {
            trf!(
                "{}: целое число от {} до 10 000 000 или пустое поле для авто.",
                "{}: enter an integer from {} to 10,000,000 or leave it blank for auto.",
                label,
                minimum
            )
        })
}

struct ModelForm {
    name: Editor,
    id: Editor,
    context: Editor,
    input: Editor,
    output: Editor,
    settings: ModelSettings,
    focus: usize,
}

impl ModelForm {
    const FIELDS: usize = 8;

    fn new(settings: ModelSettings, focus_missing: bool) -> Self {
        let limit = |value: u32| {
            if value == 0 {
                Editor::new()
            } else {
                Editor::from(&value.to_string())
            }
        };
        let focus = if settings.model.is_empty() {
            1
        } else if focus_missing {
            [settings.context, settings.input, settings.output]
                .iter()
                .position(|value| *value == 0)
                .map_or(0, |index| index + 2)
        } else {
            0
        };
        Self {
            name: Editor::from(&settings.name),
            id: Editor::from(&settings.model),
            context: limit(settings.context),
            input: limit(settings.input),
            output: limit(settings.output),
            settings,
            focus,
        }
    }

    fn editor(&mut self) -> Option<&mut Editor> {
        match self.focus {
            0 => Some(&mut self.name),
            1 => Some(&mut self.id),
            2 => Some(&mut self.context),
            3 => Some(&mut self.input),
            4 => Some(&mut self.output),
            _ => None,
        }
    }

    fn entry(&self, allow_empty_id: bool) -> std::result::Result<ModelSettings, String> {
        let mut settings = self.settings.clone();
        settings.model = self.id.text().trim().to_owned();
        if (!allow_empty_id && settings.model.is_empty()) || settings.model.chars().count() > 160 {
            return Err(tr!(
                "Укажи ID модели от 1 до 160 символов.",
                "Enter a model ID of 1 to 160 characters."
            )
            .into());
        }
        settings.name = self.name.text().trim().to_owned();
        if settings.name.chars().count() > 64 {
            return Err(tr!(
                "Имя модели длиннее 64 символов.",
                "The model name exceeds 64 characters."
            )
            .into());
        }
        settings.context = number(&self.context, tr!("Контекст", "Context"), 1)?;
        settings.input = number(&self.input, tr!("Макс. вход", "Max input"), 1)?;
        settings.output = number(&self.output, tr!("Макс. выход", "Max output"), 1)?;
        settings
            .validate_limits()
            .map_err(|error| error.to_string())?;
        Ok(settings)
    }
}

impl Screen {
    pub(in crate::native) fn edit_connection(
        &mut self,
        id: u32,
        saved: Option<&Connection>,
    ) -> Result<Option<Connection>> {
        let mut name = Editor::from(saved.map_or("", |provider| provider.name.as_str()));
        let mut url = Editor::from(saved.map_or("", |provider| provider.url.as_str()));
        let mut protocol = saved.map_or(Protocol::default(), |provider| provider.protocol);
        let mut key = Editor::new();
        let mut focus = 0;
        let mut without_key = false;
        let mut error = None;
        loop {
            let same_route = saved.is_some_and(|provider| {
                endpoint(url.text().trim()).is_ok_and(|url| url == provider.url)
                    && protocol == provider.protocol
            });
            let previous_key = saved
                .filter(|_| same_route && !without_key)
                .map(|provider| provider.key.as_str());
            let hint = if previous_key.is_some_and(|key| !key.is_empty()) {
                tr!("сохранён ••••", "saved ••••")
            } else if without_key || previous_key == Some("") {
                tr!("без ключа", "no key")
            } else {
                tr!("введи ключ или F4 без ключа", "enter key or F4 for no key")
            };
            let fields = [
                (
                    tr!("Название", "Name"),
                    Field::Text(&name, tr!("имя подключения", "connection name")),
                ),
                (
                    tr!("Протокол", "Protocol"),
                    Field::Choice(format!("‹ {} ›", protocol.label())),
                ),
                (
                    tr!("Адрес", "Base URL"),
                    Field::Text(&url, "https://example.org/v1"),
                ),
                ("API key", Field::Secret(&key, hint)),
            ];
            let (mut lines, cursor) = form_lines(&fields, focus, panel_width()?);
            if let Some(message) = &error {
                lines.push(Line::of(format!(" ! {message}"), Style::fg(render::WARN)));
            }
            self.draw_panel(
                if saved.is_some() {
                    tr!("Изменить провайдера", "Edit provider")
                } else {
                    tr!("Новый провайдер", "New provider")
                },
                tr!(
                    " Tab · ←→ протокол · F4 без ключа · Enter сохранить · Esc ",
                    " Tab · ←→ protocol · F4 no key · Enter save · Esc "
                ),
                &lines,
                0,
                cursor,
            )?;
            match read_ui_event()? {
                Event::Key(event) if event.kind != KeyEventKind::Release => {
                    let event = normalize_key(event);
                    match event.code {
                        KeyCode::Esc => return Ok(None),
                        KeyCode::Tab | KeyCode::Down => focus = (focus + 1) % fields.len(),
                        KeyCode::BackTab | KeyCode::Up => {
                            focus = (focus + fields.len() - 1) % fields.len()
                        }
                        KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if focus == 1 => {
                            protocol = protocol.next(event.code == KeyCode::Left);
                            key.clear();
                            without_key = false;
                        }
                        KeyCode::F(4) => {
                            key.clear();
                            without_key = true;
                            error = None;
                        }
                        KeyCode::Enter => {
                            let name = name.text().trim().to_owned();
                            if name.is_empty() || name.chars().count() > 64 {
                                error = Some(
                                    tr!(
                                        "Укажи название от 1 до 64 символов.",
                                        "Enter a name of 1 to 64 characters."
                                    )
                                    .into(),
                                );
                                continue;
                            }
                            let url = match endpoint(url.text().trim()) {
                                Ok(url) => url,
                                Err(problem) => {
                                    error = Some(problem.to_string());
                                    continue;
                                }
                            };
                            let key = key.text().trim().to_owned();
                            let key = if !key.is_empty() {
                                key
                            } else if let Some(previous_key) = previous_key {
                                previous_key.to_owned()
                            } else if without_key {
                                String::new()
                            } else {
                                focus = 3;
                                error = Some(tr!("Введи ключ для этого адреса или нажми F4 для работы без ключа.",
                                    "Enter a key for this endpoint or press F4 to connect without one.").into());
                                continue;
                            };
                            return Ok(Some(Connection {
                                id,
                                name,
                                url,
                                protocol,
                                key,
                            }));
                        }
                        _ => match focus {
                            0 => edit(&mut name, event, false),
                            2 => {
                                let address = url.text();
                                edit(&mut url, event, false);
                                if url.text() != address {
                                    key.clear();
                                    without_key = false;
                                }
                            }
                            3 => {
                                edit(&mut key, event, false);
                                without_key = false;
                            }
                            _ => {}
                        },
                    }
                }
                Event::Paste(value) => match focus {
                    0 => paste(&mut name, &value, false),
                    2 => {
                        paste(&mut url, &value, false);
                        key.clear();
                        without_key = false;
                    }
                    3 => {
                        paste(&mut key, &value, false);
                        without_key = false;
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    }

    pub(in crate::native) fn models(
        &mut self,
        connections: &[Connection],
        models: &[SavedModel],
        active: Option<usize>,
    ) -> Result<ModelAction> {
        let entries: Vec<_> = connections
            .iter()
            .flat_map(|connection| {
                models
                    .iter()
                    .enumerate()
                    .filter(move |(_, model)| model.connection == connection.id)
                    .map(move |(index, model)| (connection, index, &model.settings))
            })
            .collect();
        let mut selected = entries
            .iter()
            .position(|(_, index, _)| Some(*index) == active)
            .unwrap_or(0);
        let mut scroll = 0;
        loop {
            let mut lines = Vec::new();
            let mut group = None;
            let mut focus = 0;
            for (row, (connection, index, settings)) in entries.iter().enumerate() {
                if group != Some(connection.id) {
                    lines.push(Line::of(
                        format!(" {}", connection.display_name(connections)),
                        Style::brand().bold(),
                    ));
                    group = Some(connection.id);
                }
                if row == selected {
                    focus = lines.len();
                }
                let mark = if Some(*index) == active { "●" } else { " " };
                let model = if settings.name == settings.model {
                    settings.name.clone()
                } else {
                    format!("{} · {}", settings.name, settings.model)
                };
                lines.push(marked(&format!("{mark} {model}"), row == selected));
            }
            if lines.is_empty() {
                lines.push(Line::of(
                    if connections.is_empty() {
                        tr!(
                            "Сначала добавь подключение через /provider.",
                            "Add a connection through /provider first."
                        )
                    } else {
                        tr!(
                            "A — добавить модель · F4 — загрузить каталог",
                            "A adds a model · F4 loads a catalog"
                        )
                    },
                    Style::dim(),
                ));
            }
            scroll = scroll_to(focus, scroll, lines.len(), visible_rows()?);
            self.draw_panel(
                tr!("Модели", "Models"),
                tr!(" A добавить · F4 каталог · F5 авто · E правка · D удалить · T тест · Enter выбрать ",
                    " A add · F4 catalog · F5 auto · E edit · D delete · T test · Enter select "),
                &lines, scroll, None,
            )?;
            if let Event::Key(event) = read_ui_event()?
                && event.kind != KeyEventKind::Release
            {
                match normalize_key(event).code {
                    KeyCode::Up => selected = selected.saturating_sub(1),
                    KeyCode::Down => selected = (selected + 1).min(entries.len().saturating_sub(1)),
                    KeyCode::Home => selected = 0,
                    KeyCode::End => selected = entries.len().saturating_sub(1),
                    KeyCode::Char('a' | 'A' | 'ф' | 'Ф') => return Ok(ModelAction::Add),
                    KeyCode::F(4) => return Ok(ModelAction::Catalog),
                    KeyCode::Esc => return Ok(ModelAction::Close),
                    code => {
                        if let Some((_, index, _)) = entries.get(selected) {
                            match code {
                                KeyCode::Enter => return Ok(ModelAction::Activate(*index)),
                                KeyCode::Char('e' | 'E' | 'у' | 'У') => {
                                    return Ok(ModelAction::Edit(*index));
                                }
                                KeyCode::Char('d' | 'D' | 'в' | 'В') => {
                                    return Ok(ModelAction::Delete(*index));
                                }
                                KeyCode::Char('t' | 'T' | 'е' | 'Е') => {
                                    return Ok(ModelAction::Test(*index));
                                }
                                KeyCode::F(5) => return Ok(ModelAction::Auto(*index)),
                                _ => {}
                            }
                        }
                    }
                }
            }
        }
    }

    pub(in crate::native) fn edit_model(
        &mut self,
        connection: &Connection,
        settings: ModelSettings,
        editing: bool,
        allow_catalog: bool,
        mut message: Option<String>,
    ) -> Result<ModelEdit> {
        let mut form = ModelForm::new(settings, message.is_some());
        let mut scroll = 0;
        loop {
            let levels = form.settings.available_reasoning(connection);
            let fields = [
                (
                    tr!("Название", "Name"),
                    Field::Text(&form.name, tr!("по ID модели", "use model ID")),
                ),
                (
                    tr!("Модель · ID", "Model ID"),
                    Field::Text(
                        &form.id,
                        tr!("например glm-5.3-flash", "e.g. glm-5.3-flash"),
                    ),
                ),
                (
                    tr!("Контекст", "Context"),
                    Field::Text(&form.context, tr!("авто", "auto")),
                ),
                (
                    tr!("Макс. вход", "Max input"),
                    Field::Text(&form.input, tr!("авто", "auto")),
                ),
                (
                    tr!("Макс. выход", "Max output"),
                    Field::Text(&form.output, tr!("авто", "auto")),
                ),
                (
                    "Reasoning",
                    Field::Choice(format!("‹ {} ›", form.settings.reasoning.label())),
                ),
                (
                    tr!("Формат API", "API format"),
                    Field::Choice(format!("‹ {} ›", form.settings.reasoning_api.label())),
                ),
                (
                    tr!("Изображения", "Images"),
                    Field::Choice(
                        match form.settings.vision {
                            Some(true) => tr!("вкл", "on"),
                            Some(false) => tr!("выкл", "off"),
                            None => tr!("авто", "auto"),
                        }
                        .into(),
                    ),
                ),
            ];
            let (form_rows, cursor) = form_lines(&fields, form.focus, panel_width()?);
            let mut lines = vec![Line::of(
                format!(" {} · {}", connection.name, connection.protocol.label()),
                Style::dim(),
            )];
            lines.extend(form_rows);
            if let Some(message) = &message {
                lines.push(Line::of(format!(" {message}"), Style::fg(render::WARN)));
            }
            scroll = scroll_to(form.focus + 1, scroll, lines.len(), visible_rows()?);
            self.draw_panel(
                if editing {
                    tr!("Изменить модель", "Edit model")
                } else {
                    tr!("Новая модель", "New model")
                },
                if allow_catalog {
                    tr!(
                        " F3 рассуждения · F4 каталог · F5 авто · Enter сохранить · Esc ",
                        " F3 reasoning · F4 catalog · F5 auto · Enter save · Esc "
                    )
                } else {
                    tr!(
                        " Tab/↑↓ поле · F3 рассуждения · F5 авто · Enter сохранить · Esc ",
                        " Tab/↑↓ field · F3 reasoning · F5 auto · Enter save · Esc "
                    )
                },
                &lines,
                scroll,
                cursor.map(|(row, col)| (row + 1, col)),
            )?;
            match read_ui_event()? {
                Event::Key(event) if event.kind != KeyEventKind::Release => {
                    let event = normalize_key(event);
                    match event.code {
                        KeyCode::Esc => return Ok(ModelEdit::Cancel),
                        KeyCode::Tab | KeyCode::Down => {
                            form.focus = (form.focus + 1) % ModelForm::FIELDS
                        }
                        KeyCode::BackTab | KeyCode::Up => {
                            form.focus = (form.focus + ModelForm::FIELDS - 1) % ModelForm::FIELDS
                        }
                        KeyCode::F(3) => {
                            self.edit_reasoning_levels(&mut form.settings, connection)?
                        }
                        KeyCode::F(4) if allow_catalog => match form.entry(true) {
                            Ok(settings) => return Ok(ModelEdit::Catalog(settings)),
                            Err(error) => message = Some(error),
                        },
                        KeyCode::F(5) | KeyCode::Enter => match form.entry(false) {
                            Ok(settings) if event.code == KeyCode::F(5) => {
                                return Ok(ModelEdit::Auto(settings));
                            }
                            Ok(settings) => return Ok(ModelEdit::Save(settings)),
                            Err(error) => message = Some(error),
                        },
                        KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if form.focus >= 5 => {
                            match form.focus {
                                5 => {
                                    let at = levels
                                        .iter()
                                        .position(|level| *level == form.settings.reasoning)
                                        .unwrap_or(0);
                                    let offset = if event.code == KeyCode::Left {
                                        levels.len() - 1
                                    } else {
                                        1
                                    };
                                    form.settings.reasoning = levels[(at + offset) % levels.len()];
                                }
                                6 => {
                                    let current = form.settings.reasoning_api;
                                    form.settings.reasoning_api = if event.code == KeyCode::Left {
                                        current.previous()
                                    } else {
                                        current.next()
                                    };
                                }
                                7 => {
                                    form.settings.vision = cycle_vision(
                                        form.settings.vision,
                                        event.code == KeyCode::Left,
                                    );
                                }
                                _ => {}
                            }
                        }
                        _ => {
                            let limit = form.focus >= 2;
                            if let Some(editor) = form.editor() {
                                edit(editor, event, limit);
                            }
                        }
                    }
                }
                Event::Paste(value) => {
                    let limit = form.focus >= 2;
                    if let Some(editor) = form.editor() {
                        paste(editor, &value, limit);
                    }
                }
                _ => {}
            }
        }
    }

    fn edit_reasoning_levels(
        &mut self,
        settings: &mut ModelSettings,
        connection: &Connection,
    ) -> Result<()> {
        let mut draft = settings.clone();
        let mut selected = 0;
        let mut scroll = 0;
        loop {
            let levels: Vec<_> = draft
                .configured_reasoning(connection)
                .into_iter()
                .filter(|level| *level != Reasoning::ProviderDefault)
                .collect();
            selected = selected.min(levels.len().saturating_sub(1));
            let enabled = draft.available_reasoning(connection);
            let source = if draft.reasoning_levels.is_some() {
                tr!("уровни модели", "model levels")
            } else {
                tr!(
                    "предустановки API (не проверены)",
                    "API presets (unverified)"
                )
            };
            let mut lines = vec![Line::of(
                trf!(
                    "  Для /reasoning: {} из {} · {source}",
                    "  For /reasoning: {} of {} · {source}",
                    enabled.len().saturating_sub(1),
                    levels.len()
                ),
                Style::dim(),
            )];
            let visible = visible_rows()?.saturating_sub(1).max(1);
            scroll = scroll_to(selected, scroll, levels.len(), visible);
            for (index, &level) in levels.iter().enumerate().skip(scroll).take(visible) {
                let check = if enabled.contains(&level) {
                    "[x]"
                } else {
                    "[ ]"
                };
                lines.push(marked(
                    &format!("{check}  {}", level.label()),
                    index == selected,
                ));
            }
            if levels.is_empty() {
                lines.push(Line::of(
                    tr!("  Только по умолчанию", "  Provider default only"),
                    Style::dim(),
                ));
            }
            self.draw_panel(
                tr!("Доступные уровни рассуждений", "Available reasoning levels"),
                tr!(
                    " ↑↓ · Пробел отметить · Enter применить · F2 API · Esc ",
                    " ↑↓ · Space toggle · Enter apply · F2 API · Esc "
                ),
                &lines,
                0,
                None,
            )?;
            if let Event::Key(key) = read_ui_event()?
                && key.kind == KeyEventKind::Press
            {
                match normalize_key(key).code {
                    KeyCode::Up => selected = selected.saturating_sub(1),
                    KeyCode::Down => selected = (selected + 1).min(levels.len().saturating_sub(1)),
                    KeyCode::Char(' ') | KeyCode::Tab => {
                        if let Some(&level) = levels.get(selected) {
                            let was_enabled = enabled.contains(&level);
                            let mut chosen = enabled;
                            chosen.retain(|&item| {
                                item != Reasoning::ProviderDefault && item != level
                            });
                            if !was_enabled {
                                chosen.push(level);
                            }
                            draft.enabled_reasoning = Some(chosen);
                        }
                    }
                    KeyCode::F(2) => self.edit_reasoning_mapping(&mut draft, connection)?,
                    KeyCode::Enter => {
                        if !enabled.contains(&draft.reasoning) {
                            draft.reasoning = Reasoning::ProviderDefault;
                        }
                        *settings = draft;
                        return Ok(());
                    }
                    KeyCode::Esc => return Ok(()),
                    _ => {}
                }
            }
        }
    }

    fn edit_reasoning_mapping(
        &mut self,
        settings: &mut ModelSettings,
        connection: &Connection,
    ) -> Result<()> {
        let mut draft = settings.clone();
        loop {
            let available = draft.configured_reasoning(connection);
            let mut entries = vec![
                tr!("Применить настройки", "Apply settings").into(),
                tr!("Восстановить настройки формата API", "Restore API presets").into(),
            ];
            entries.extend(Reasoning::LEVELS.iter().map(|&level| {
                let option = draft
                    .reasoning_levels
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .find(|option| option.level == level);
                let status = if !available.contains(&level) {
                    tr!("не включён", "not enabled")
                } else if let Some(option) = option {
                    option
                        .wire
                        .as_deref()
                        .unwrap_or(tr!("по формату API", "API preset"))
                } else {
                    tr!("по формату API", "API preset")
                };
                format!("{}  —  {status}", level.label())
            }));
            let Some(index) = self.choose(
                tr!("Настройка уровней API", "Configure API levels"),
                &entries,
            )?
            else {
                return Ok(());
            };
            match index {
                0 => {
                    if !available.contains(&draft.reasoning) {
                        draft.reasoning = Reasoning::ProviderDefault;
                        self.notice(tr!(
                            "Выбранный уровень убран. Теперь решает провайдер.",
                            "Selected level removed. The provider now decides."
                        ));
                    }
                    *settings = draft;
                    return Ok(());
                }
                1 => {
                    draft.reasoning_levels = None;
                }
                _ => {
                    let level = Reasoning::LEVELS[index - 2];
                    let mut choices = vec![
                        tr!("Указать значение API", "Set API value").into(),
                        tr!("Убрать уровень", "Remove level").into(),
                    ];
                    if level == Reasoning::Off {
                        choices.push(tr!("Без значения API", "No API value").into());
                    }
                    let Some(choice) = self.choose(level.label(), &choices)? else {
                        continue;
                    };
                    let wire = if choice == 0 {
                        let Some(value) =
                            self.ask_text(tr!("Значение для API", "API value"), false)?
                        else {
                            continue;
                        };
                        let value = value.trim();
                        if !valid_effort(value) {
                            self.report(tr!("Значение для API", "API value"), tr!("Используй до 64 латинских букв, цифр, точек, подчёркиваний или дефисов.", "Use up to 64 ASCII letters, digits, dots, underscores or hyphens."))?;
                            continue;
                        }
                        Some(value.to_owned())
                    } else {
                        None
                    };
                    draft.set_reasoning_mapping(
                        connection,
                        level,
                        (choice != 1).then_some(ReasoningLevel { level, wire }),
                    );
                }
            }
        }
    }

    pub(in crate::native) fn reasoning(
        &mut self,
        current: Reasoning,
        levels: &[Reasoning],
    ) -> Result<Option<Reasoning>> {
        let entries: Vec<_> = levels
            .iter()
            .map(|&level| {
                format!(
                    "{} {}",
                    if current == level { "●" } else { " " },
                    level.label()
                )
            })
            .collect();
        Ok(self
            .choose("Reasoning", &entries)?
            .map(|index| levels[index]))
    }

    pub(in crate::native) fn catalog_picker(
        &mut self,
        models: &[&CatalogModel],
    ) -> Result<Option<Vec<usize>>> {
        let mut query = Editor::new();
        let mut selected = 0usize;
        let mut scroll = 0;
        let mut marked_rows = vec![false; models.len()];
        loop {
            let needle = query.text().to_lowercase();
            let matches: Vec<_> = models
                .iter()
                .enumerate()
                .filter(|(_, model)| {
                    let name = format!("{} {}", model.name, model.id).to_lowercase();
                    needle.split_whitespace().all(|word| name.contains(word))
                })
                .collect();
            selected = selected.min(matches.len().saturating_sub(1));
            let badge = trf!(
                "Выбрано {} · Поиск: ",
                "{} selected · Search: ",
                marked_rows.iter().filter(|&&item| item).count()
            );
            let (shown, caret) = query.view(panel_width()?.saturating_sub(badge.width()).max(1));
            let mut heading = Line::of(&badge, Style::dim());
            heading.push(shown, Style::fg(render::FG));
            let mut lines = vec![heading];
            let visible = visible_rows()? - 1;
            scroll = scroll_to(selected, scroll, matches.len(), visible);
            for (row, (index, model)) in matches.iter().enumerate().skip(scroll).take(visible) {
                let check = if marked_rows[*index] { "[x]" } else { "[ ]" };
                let text = if model.name == model.id {
                    format!("{check} {}", model.id)
                } else {
                    format!("{check} {} · {}", model.id, model.name)
                };
                lines.push(marked(&text, row == selected));
            }
            if matches.is_empty() {
                lines.push(Line::of(
                    tr!("Модели не найдены.", "No matching models."),
                    Style::dim(),
                ));
            }
            self.draw_panel(
                tr!("Каталог моделей", "Model catalog"),
                tr!(
                    " Текст поиск · ↑↓ выбор · Tab отметить · Enter добавить · Esc ",
                    " Type to search · ↑↓ select · Tab mark · Enter add · Esc "
                ),
                &lines,
                0,
                Some((0, badge.width() + caret)),
            )?;
            match read_ui_event()? {
                Event::Key(event) if event.kind != KeyEventKind::Release => {
                    match normalize_key(event).code {
                        KeyCode::Up => selected = selected.saturating_sub(1),
                        KeyCode::Down => {
                            selected = (selected + 1).min(matches.len().saturating_sub(1))
                        }
                        KeyCode::PageUp => selected = selected.saturating_sub(visible),
                        KeyCode::PageDown => {
                            selected = (selected + visible).min(matches.len().saturating_sub(1))
                        }
                        KeyCode::Tab => {
                            if let Some((index, _)) = matches.get(selected) {
                                marked_rows[*index] = !marked_rows[*index];
                            }
                        }
                        KeyCode::Enter => {
                            let mut indices: Vec<_> = marked_rows
                                .iter()
                                .enumerate()
                                .filter_map(|(index, &marked)| marked.then_some(index))
                                .collect();
                            if indices.is_empty()
                                && let Some((index, _)) = matches.get(selected)
                            {
                                indices.push(*index);
                            }
                            if !indices.is_empty() {
                                return Ok(Some(indices));
                            }
                        }
                        KeyCode::Esc => return Ok(None),
                        _ => {
                            edit(&mut query, normalize_key(event), false);
                            selected = 0;
                        }
                    }
                }
                Event::Paste(value) => {
                    paste(&mut query, &value, false);
                    selected = 0;
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::ReasoningApi;

    #[test]
    fn left_and_right_cycle_fields_in_opposite_directions() {
        assert_eq!(cycle_index(7, 0, true), 6);
        assert_eq!(cycle_index(7, 6, false), 0);
        assert_eq!(cycle_index(3, 0, false), 1);
        assert_eq!(cycle_index(0, 0, true), 0);
        assert_eq!(cycle_vision(None, false), Some(true));
        assert_eq!(cycle_vision(None, true), Some(false));
        assert_eq!(cycle_vision(Some(true), true), None);
        assert_eq!(ReasoningApi::Auto.previous(), ReasoningApi::None);
        assert_eq!(ReasoningApi::None.next(), ReasoningApi::Auto);
    }

    #[test]
    fn new_model_focuses_id_and_accepts_equal_token_limits() {
        let mut form = ModelForm::new(ModelSettings::default(), true);
        assert_eq!(form.focus, 1);
        form.id = Editor::from("manual");
        form.context = Editor::from("1048576");
        form.input = Editor::from("1048576");
        form.output = Editor::from("1048576");
        let caps = form.entry(false).unwrap();
        assert_eq!(
            (caps.context, caps.input, caps.output),
            (1_048_576, 1_048_576, 1_048_576)
        );
        form.output = Editor::from("128000");
        assert_eq!(form.entry(false).unwrap().output, 128_000);
    }
}
