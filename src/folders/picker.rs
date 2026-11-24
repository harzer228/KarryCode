use super::{accessible, create_folder, display, list, new_folder_path, selectable, visible};
use crate::recent::RecentFolders;
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers,
    },
    execute, queue,
    style::{
        Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
    },
    terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::{
    ffi::OsString,
    io::{self, Stdout, Write},
    path::{Path, PathBuf},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub(super) struct Screen(Stdout);

impl Screen {
    pub(super) fn open() -> io::Result<Self> {
        terminal::size()?;
        terminal::enable_raw_mode()?;
        let mut screen = Self(io::stdout());
        execute!(screen.0, EnterAlternateScreen, Hide)?;
        let _ = execute!(screen.0, EnableBracketedPaste);
        Ok(screen)
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let _ = execute!(
            self.0,
            DisableBracketedPaste,
            SetAttribute(Attribute::Reset),
            ResetColor,
            LeaveAlternateScreen,
            Show
        );
        let _ = terminal::disable_raw_mode();
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Folders,
    Recent,
    Drives,
    Actions,
}

#[derive(Clone, Copy)]
enum Action {
    Home,
    Bundle,
    Drives,
    Path,
    Create,
    Hidden,
}

const ACTIONS: [Action; 6] = [
    Action::Home,
    Action::Bundle,
    Action::Drives,
    Action::Path,
    Action::Create,
    Action::Hidden,
];

enum Row {
    Select,
    Child(OsString),
    Location(PathBuf),
    Action(Action),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Search,
    Path,
    Create,
}

struct Editor {
    field: Field,
    text: Vec<char>,
    caret: usize,
}

impl Editor {
    fn new(field: Field, text: &str) -> Self {
        let text: Vec<_> = text.chars().collect();
        Self {
            field,
            caret: text.len(),
            text,
        }
    }

    fn insert(&mut self, text: impl IntoIterator<Item = char>) {
        let text: Vec<_> = text.into_iter().filter(|c| !c.is_control()).collect();
        let count = text.len();
        self.text.splice(self.caret..self.caret, text);
        self.caret += count;
    }

    fn key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Left => self.caret = self.caret.saturating_sub(1),
            KeyCode::Right => self.caret = (self.caret + 1).min(self.text.len()),
            KeyCode::Home => self.caret = 0,
            KeyCode::End => self.caret = self.text.len(),
            KeyCode::Backspace if self.caret > 0 => {
                self.caret -= 1;
                self.text.remove(self.caret);
            }
            KeyCode::Delete if self.caret < self.text.len() => {
                self.text.remove(self.caret);
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.text.clear();
                self.caret = 0;
            }
            KeyCode::Char(ch) if is_text_key(key) => self.insert([ch]),
            _ => {}
        }
    }

    fn value(&self) -> String {
        self.text.iter().collect()
    }

    fn display(&self, width: usize) -> String {
        let before: String = self.text[..self.caret].iter().collect();
        let after: String = self.text[self.caret..].iter().collect();
        let before = shorten(&before, width.saturating_sub(1), true);
        format!(
            "{before}▏{}",
            shorten(&after, width.saturating_sub(before.width() + 1), false)
        )
    }
}

fn is_text_key(key: KeyEvent) -> bool {
    // Windows can report AltGr text as Right Alt + Ctrl.
    let mask = KeyModifiers::CONTROL | KeyModifiers::ALT;
    !key.modifiers.intersects(mask) || key.modifiers.contains(mask)
}

struct Picker<'a> {
    current: PathBuf,
    root: &'a Path,
    recent: &'a RecentFolders,
    folders: Vec<OsString>,
    hidden: bool,
    view: View,
    filter: String,
    editor: Option<Editor>,
    rows: Vec<Row>,
    selected: usize,
    top: usize,
    notice: Option<String>,
}

enum Flow {
    Continue,
    Close,
    Launch(PathBuf),
}

pub(super) fn run(
    mut screen: Screen,
    start: &Path,
    root: &Path,
    recent: &RecentFolders,
) -> anyhow::Result<Option<PathBuf>> {
    let mut picker = Picker {
        current: start.canonicalize().unwrap_or_else(|_| start.to_owned()),
        root,
        recent,
        folders: Vec::new(),
        hidden: false,
        view: View::Folders,
        filter: String::new(),
        editor: None,
        rows: Vec::new(),
        selected: 0,
        top: 0,
        notice: None,
    };
    picker.reload();
    loop {
        let (width, height) = terminal::size()?;
        let rows = usize::from(height.saturating_sub(7)).max(1);
        picker.draw(&mut screen.0, width, height, rows)?;
        match event::read()? {
            Event::Key(key) if !usable(width, height) && key.code == KeyCode::Esc => {
                return Ok(None);
            }
            _ if !usable(width, height) => {}
            Event::Key(key) if key.kind != KeyEventKind::Release => match picker.key(key, rows) {
                Flow::Continue => {}
                Flow::Close => return Ok(None),
                Flow::Launch(path) => return Ok(Some(path)),
            },
            Event::Paste(text) => {
                if let Some(editor) = &mut picker.editor {
                    editor.insert(text.chars());
                    picker.refresh_search();
                }
            }
            _ => {}
        }
    }
}

impl Picker<'_> {
    fn reload(&mut self) {
        self.folders = match list(&self.current, self.hidden) {
            Ok(folders) => folders,
            Err(error) => {
                self.notice = Some(trf!(
                    "Нет доступа к папке ({error})",
                    "Cannot read this folder ({error})"
                ));
                Vec::new()
            }
        };
        self.show(View::Folders);
    }

    fn show(&mut self, view: View) {
        self.view = view;
        self.selected = 0;
        self.top = 0;
        self.rows = match view {
            View::Folders => {
                let query = self
                    .editor
                    .as_ref()
                    .filter(|editor| editor.field == Field::Search)
                    .map_or_else(|| self.filter.clone(), Editor::value)
                    .to_lowercase();
                let mut rows = Vec::new();
                if query.is_empty() && self.current.parent().is_some() {
                    rows.push(Row::Select);
                }
                rows.extend(
                    self.folders
                        .iter()
                        .filter(|name| {
                            query.is_empty()
                                || name.to_string_lossy().to_lowercase().contains(&query)
                        })
                        .cloned()
                        .map(Row::Child),
                );
                rows
            }
            View::Recent => self
                .recent
                .available(self.root)
                .into_iter()
                .map(Row::Location)
                .collect(),
            View::Drives => drives().into_iter().map(Row::Location).collect(),
            View::Actions => ACTIONS.into_iter().map(Row::Action).collect(),
        };
    }

    fn open_folder(&mut self, path: &Path) -> anyhow::Result<()> {
        self.current = accessible(path)?;
        self.filter.clear();
        self.reload();
        Ok(())
    }

    fn open(&mut self, path: &Path) {
        if let Err(error) = self.open_folder(path) {
            self.notice = Some(error.to_string());
        }
    }

    fn parent(&mut self) {
        if let Some(parent) = self.current.parent().map(Path::to_owned) {
            let child = self.current.file_name().map(OsString::from);
            if let Err(error) = self.open_folder(&parent) {
                self.notice = Some(error.to_string());
                return;
            }
            if let Some(index) = self
                .rows
                .iter()
                .position(|row| matches!(row, Row::Child(name) if Some(name) == child.as_ref()))
            {
                self.selected = index;
            }
        } else {
            self.show(View::Drives);
        }
    }

    fn select(&mut self) -> Flow {
        match accessible(&self.current).and_then(|path| {
            selectable(&path)?;
            Ok(path)
        }) {
            Ok(path) => Flow::Launch(path),
            Err(error) => {
                self.notice = Some(error.to_string());
                Flow::Continue
            }
        }
    }

    fn activate(&mut self) -> Flow {
        match self.rows.get(self.selected) {
            Some(Row::Select) => return self.select(),
            Some(Row::Child(name)) => self.open(&self.current.join(name)),
            Some(Row::Location(path)) => self.open(&path.clone()),
            Some(Row::Action(action)) => self.action(*action),
            None => {}
        }
        Flow::Continue
    }

    fn action(&mut self, action: Action) {
        match action {
            Action::Home => self.open(&crate::platform::home()),
            Action::Bundle => self.open(self.root),
            Action::Drives => self.show(View::Drives),
            Action::Path => self.editor = Some(Editor::new(Field::Path, "")),
            Action::Create => self.editor = Some(Editor::new(Field::Create, "")),
            Action::Hidden => {
                self.hidden = !self.hidden;
                self.reload();
            }
        }
    }

    fn refresh_search(&mut self) {
        if self
            .editor
            .as_ref()
            .is_some_and(|editor| editor.field == Field::Search)
        {
            self.show(View::Folders);
        }
    }

    fn edit_key(&mut self, key: KeyEvent, mut editor: Editor) {
        match (editor.field, key.code) {
            (Field::Search, KeyCode::Esc) => self.show(View::Folders),
            (_, KeyCode::Esc) => {}
            (Field::Search, KeyCode::Enter | KeyCode::Down) => {
                self.filter = editor.value();
                self.show(View::Folders);
            }
            (field, KeyCode::Enter) => {
                let value = editor.value();
                let value = value.trim();
                if field == Field::Create {
                    match create_folder(&self.current, value) {
                        Ok(path) => {
                            if let Err(error) = self.open_folder(&path) {
                                self.notice = Some(trf!(
                                    "Папка создана, но не открылась: {error}",
                                    "Folder created but could not open it: {error}"
                                ));
                            }
                        }
                        Err(error) => {
                            self.notice = Some(error.to_string());
                            self.editor = Some(editor);
                        }
                    }
                    return;
                }
                if value.is_empty() {
                    return;
                }
                let path = self.current.join(value.trim_matches('"'));
                if let Err(error) = self.open_folder(&path) {
                    self.notice = Some(trf!(
                        "Не удалось открыть или создать папку: {error}",
                        "Could not open or create the folder: {error}"
                    ));
                    self.editor = Some(editor);
                }
            }
            _ => {
                editor.key(key);
                self.editor = Some(editor);
                self.refresh_search();
            }
        }
    }

    fn key(&mut self, key: KeyEvent, page: usize) -> Flow {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Flow::Close;
        }
        self.notice = None;
        if let Some(editor) = self.editor.take() {
            self.edit_key(key, editor);
            return Flow::Continue;
        }
        let last = self.rows.len().saturating_sub(1);
        match key.code {
            KeyCode::Esc if self.view == View::Folders && !self.filter.is_empty() => {
                self.filter.clear();
                self.show(View::Folders);
            }
            KeyCode::Esc | KeyCode::Char('0') if self.view == View::Folders => return Flow::Close,
            KeyCode::Esc | KeyCode::Char('0') => self.show(View::Folders),
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => self.selected = (self.selected + 1).min(last),
            KeyCode::PageUp => self.selected = self.selected.saturating_sub(page),
            KeyCode::PageDown => self.selected = (self.selected + page).min(last),
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = last,
            KeyCode::Enter => return self.activate(),
            KeyCode::Right if !matches!(self.rows.get(self.selected), Some(Row::Select)) => {
                return self.activate();
            }
            KeyCode::F(2) if self.view == View::Folders => return self.select(),
            KeyCode::Left | KeyCode::Backspace if self.view == View::Folders => self.parent(),
            KeyCode::Left | KeyCode::Backspace => self.show(View::Folders),
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Char('r' | 'к') => {
                self.show(if self.view == View::Recent {
                    View::Folders
                } else {
                    View::Recent
                });
            }
            KeyCode::F(10) | KeyCode::Char('?') => self.show(if self.view == View::Actions {
                View::Folders
            } else {
                View::Actions
            }),
            KeyCode::F(5) => self.reload(),
            KeyCode::Char('/') => {
                self.editor = Some(Editor::new(Field::Search, &self.filter));
                self.show(View::Folders);
            }
            KeyCode::Char('p') => self.action(Action::Path),
            KeyCode::Char('d') => self.action(Action::Drives),
            KeyCode::Char('h') => self.action(Action::Home),
            KeyCode::F(7) | KeyCode::Char('n' | 'т') => self.action(Action::Create),
            KeyCode::Char('.') => self.action(Action::Hidden),
            _ => {}
        }
        Flow::Continue
    }

    fn label(&self, row: &Row) -> String {
        match row {
            Row::Select => tr!("Выбрать эту папку", "Choose this folder").to_owned(),
            Row::Child(name) => format!("{}/", visible(name)),
            Row::Location(path) => display(path),
            Row::Action(action) => match action {
                Action::Home => tr!("Домашняя папка", "Home folder"),
                Action::Bundle => tr!("Папка KarryCode", "KarryCode folder"),
                Action::Drives => tr!("Диски", "Drives"),
                Action::Path => tr!("Ввести путь…", "Go to a path…"),
                Action::Create => tr!("Создать папку…", "New folder…"),
                Action::Hidden if self.hidden => tr!("Скрыть скрытые папки", "Hide hidden folders"),
                Action::Hidden => tr!("Показать скрытые папки", "Show hidden folders"),
            }
            .to_owned(),
        }
    }

    fn draw_create(&self, out: &mut Stdout, width: usize, editor: &Editor) -> io::Result<()> {
        let accent = Color::Rgb {
            r: 181,
            g: 214,
            b: 178,
        };
        let ink = Color::Rgb {
            r: 30,
            g: 37,
            b: 36,
        };
        queue!(
            out,
            SetBackgroundColor(accent),
            SetForegroundColor(ink),
            SetAttribute(Attribute::Bold)
        )?;
        line(out, 0, tr!(" Новая папка ", " New folder "))?;
        queue!(out, SetAttribute(Attribute::Reset), ResetColor)?;
        line(out, 2, tr!("Имя папки", "Folder name"))?;
        queue!(out, SetBackgroundColor(ink), SetForegroundColor(accent))?;
        line(
            out,
            3,
            &format!(" {}", editor.display(width.saturating_sub(2))),
        )?;
        queue!(out, ResetColor)?;
        let input = editor.value();
        let target = new_folder_path(&self.current, &input);
        let preview = match &target {
            Ok(path) => display(path),
            Err(_) => display(&self.current),
        };
        line(out, 4, tr!("Место создания", "Destination"))?;
        line(out, 5, &shorten(&preview, width, true))?;
        if let Some(error) = self.notice.as_deref() {
            queue!(out, SetForegroundColor(Color::Yellow))?;
            line(out, 6, &shorten(error, width, false))?;
            queue!(out, ResetColor)?;
        } else if !input.trim().is_empty()
            && let Err(error) = target
        {
            line(out, 6, &shorten(&error.to_string(), width, false))?;
        }
        line(
            out,
            8,
            &shorten(
                tr!("Enter Создать  Esc Назад", "Enter Create  Esc Back"),
                width,
                false,
            ),
        )?;
        out.flush()
    }

    fn draw(&mut self, out: &mut Stdout, width: u16, height: u16, page: usize) -> io::Result<()> {
        queue!(out, Clear(ClearType::All))?;
        let space = usize::from(width.saturating_sub(4));
        if !usable(width, height) {
            queue!(
                out,
                MoveTo(0, 0),
                Print(shorten(
                    tr!("Увеличь окно (Esc)", "Enlarge window (Esc)"),
                    usize::from(width.saturating_sub(1)),
                    false
                ))
            )?;
            return out.flush();
        }
        if let Some(editor) = &self.editor
            && editor.field == Field::Create
        {
            return self.draw_create(out, space, editor);
        }
        queue!(
            out,
            MoveTo(2, 0),
            SetAttribute(Attribute::Bold),
            Print(tr!("Выбор папки", "Choose a folder")),
            SetAttribute(Attribute::Reset)
        )?;
        line(out, 1, &shorten(&display(&self.current), space, true))?;
        let heading = match self.view {
            View::Folders => tr!("[Папки]  Недавние", "[Folders]  Recent"),
            View::Recent => tr!("Папки  [Недавние]", "Folders  [Recent]"),
            View::Drives => tr!("Диски", "Drives"),
            View::Actions => tr!("Действия", "Actions"),
        };
        if let Some(editor) = &self.editor {
            let label = match editor.field {
                Field::Search => tr!("Поиск ", "Search "),
                Field::Path => tr!("Путь ", "Path "),
                Field::Create => tr!("Имя папки ", "Folder name "),
            };
            line(
                out,
                2,
                &format!(
                    "{label}{}",
                    editor.display(space.saturating_sub(label.width()))
                ),
            )?;
        } else if self.view == View::Folders && !self.filter.is_empty() {
            line(
                out,
                2,
                &shorten(&trf!("Поиск: {}", "Search: {}", self.filter), space, false),
            )?;
        } else {
            line(out, 2, heading)?;
        }
        self.top = self
            .top
            .min(self.rows.len().saturating_sub(page))
            .clamp(self.selected.saturating_sub(page - 1), self.selected);
        for (index, row) in self.rows.iter().enumerate().skip(self.top).take(page) {
            let y = 4 + (index - self.top) as u16;
            let label = shorten(&self.label(row), space - 2, matches!(row, Row::Location(_)));
            if index == self.selected && self.editor.is_none() {
                queue!(
                    out,
                    SetBackgroundColor(Color::Rgb {
                        r: 52,
                        g: 80,
                        b: 68
                    }),
                    SetForegroundColor(Color::Rgb {
                        r: 236,
                        g: 247,
                        b: 238
                    }),
                    SetAttribute(Attribute::Bold)
                )?;
                line(
                    out,
                    y,
                    &format!(
                        "› {label}{}",
                        " ".repeat((space - 2).saturating_sub(label.width()))
                    ),
                )?;
                queue!(out, SetAttribute(Attribute::Reset), ResetColor)?;
            } else {
                line(out, y, &format!("  {label}"))?;
            }
        }
        if self.rows.is_empty() {
            let message = if self.view == View::Recent {
                tr!("Недавних папок пока нет.", "No recent folders yet.")
            } else {
                tr!("Папок нет.", "No folders found.")
            };
            line(out, 4, &shorten(message, space, false))?;
        }
        if let Some(notice) = &self.notice {
            line(out, height - 3, &shorten(notice, space, false))?;
        } else if self.rows.len() > page {
            line(
                out,
                height - 3,
                &format!(
                    "{}–{} / {}",
                    self.top + 1,
                    (self.top + page).min(self.rows.len()),
                    self.rows.len()
                ),
            )?;
        }
        let controls = if self.editor.is_some() {
            tr!(
                "Enter Подтвердить    Esc Отмена",
                "Enter Confirm    Esc Cancel"
            )
        } else if self.view != View::Folders {
            tr!(
                "↑↓ Навигация    Enter Открыть    Esc Назад",
                "↑↓ Navigate    Enter Open    Esc Back"
            )
        } else if space < 57 {
            tr!("↑↓  Enter  F2 Выбрать  Esc", "↑↓  Enter  F2 Choose  Esc")
        } else if matches!(self.rows.get(self.selected), Some(Row::Select)) {
            tr!(
                "↑↓ Навигация    Enter Выбрать папку    Esc Назад",
                "↑↓ Navigate    Enter Choose folder    Esc Back"
            )
        } else {
            tr!(
                "↑↓ Навигация    Enter Открыть    F2 Выбрать    Esc Назад",
                "↑↓ Navigate    Enter Open    F2 Choose    Esc Back"
            )
        };
        line(out, height - 2, &shorten(controls, space, false))?;
        if self.editor.is_none() && self.view == View::Folders {
            let shortcuts = if space < 51 {
                tr!("F7 Новая папка · F10", "F7 New folder · F10")
            } else {
                tr!(
                    "F7 Новая папка · F10 Действия · / Поиск · Tab Недавние",
                    "F7 New folder · F10 Actions · / Search · Tab Recent"
                )
            };
            line(out, height - 1, &shorten(shortcuts, space, false))?;
        }
        out.flush()
    }
}

fn line(out: &mut Stdout, y: u16, text: &str) -> io::Result<()> {
    queue!(out, MoveTo(2, y), Print(text))
}

fn usable(width: u16, height: u16) -> bool {
    width >= 25 && height >= 9
}

fn drives() -> Vec<PathBuf> {
    #[cfg(windows)]
    return crate::platform::folder_roots();
    #[cfg(not(windows))]
    vec![PathBuf::from("/")]
}

fn shorten(text: &str, limit: usize, keep_end: bool) -> String {
    if text.width() <= limit {
        return text.to_owned();
    }
    if limit == 0 {
        return String::new();
    }
    let mut available = limit - 1;
    let mut chars = text.chars();
    let mut result = Vec::new();
    while let Some(ch) = if keep_end {
        chars.next_back()
    } else {
        chars.next()
    } {
        let width = ch.width().unwrap_or(0);
        if width > available {
            break;
        }
        available -= width;
        result.push(ch);
    }
    if keep_end {
        std::iter::once('…')
            .chain(result.into_iter().rev())
            .collect()
    } else {
        result.into_iter().chain(std::iter::once('…')).collect()
    }
}
