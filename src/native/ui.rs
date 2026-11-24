//! Full-screen console for the built-in agent.
//!
//! Layout, top to bottom: the mark and session header, the conversation,
//! the boxed prompt, then the slash menu, the live spinner or the status line.
//! The screen follows the agent console design and keeps every existing
//! interaction: history, `/` menu, F2 tool details, scrollback and the
//! suspend/resume handshake used while a reviewed edit or a task runs.

mod models;
mod render;
mod selection;
pub(super) mod theme;

use super::{Message, permissions::DialogMode};
use anyhow::Result;
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
        MouseEventKind,
    },
    execute, queue,
    style::{
        Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
    },
    terminal::{
        self, BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate,
        EnterAlternateScreen, LeaveAlternateScreen,
    },
};
pub(super) use models::{ModelAction, ModelEdit};
use render::{Line, Style};
use selection::{Selection, Viewport};
use std::{
    io::{self, Write},
    path::Path,
    time::{Duration, Instant},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const MAX_CONTENT: usize = 120;
const MENU_LIMIT: usize = 6;
const CTX_TOKENS: u32 = 128_000;
const INPUT_LIMIT: usize = 64 * 1024;
const PASTE_LIMIT: usize = 192 * 1024;

/// Commands offered by the slash menu. Descriptions are localized.
fn commands() -> [(&'static str, &'static str); 29] {
    [
        ("/theme", tr!("Оформление агента", "Agent appearance")),
        (
            "/agents",
            tr!("Встроенные саб-агенты", "Built-in subagents"),
        ),
        (
            "/continue",
            tr!(
                "Продолжить прерванный запрос",
                "Resume an interrupted request"
            ),
        ),
        (
            "/header",
            tr!("Скрыть или показать шапку", "Toggle the header"),
        ),
        ("/project", tr!("Создать проект", "Create project")),
        ("/browser", tr!("Скрытый браузер", "Hidden browser")),
        ("/cd", tr!("Сменить папку проекта", "Change project folder")),
        ("/setting", tr!("Настройки агента", "Agent settings")),
        ("/tools", tr!("Доступные инструменты", "Available tools")),
        ("/search", tr!("Поиск в интернете", "Search the web")),
        (
            "/permissions",
            tr!(
                "Режим диалога и разрешения",
                "Conversation mode and permissions"
            ),
        ),
        (
            "/provider",
            tr!("Подключения к провайдерам", "Provider connections"),
        ),
        (
            "/model",
            tr!(
                "Модели: добавить, изменить, подключить",
                "Models: add, edit, connect"
            ),
        ),
        (
            "/skills",
            tr!(
                "Скиллы: список и инструкции",
                "Skills: list and instructions"
            ),
        ),
        (
            "/mcp",
            tr!(
                "MCP-серверы: статус, инструменты, импорт",
                "MCP servers: status, tools, import"
            ),
        ),
        (
            "/plan",
            tr!(
                "Подготовить план без правок",
                "Prepare a plan without editing"
            ),
        ),
        (
            "/diagnose",
            tr!(
                "Чтение проекта и поиск причины",
                "Read the project and investigate"
            ),
        ),
        (
            "/code",
            tr!(
                "Правки и команды после подтверждения",
                "Reviewed edits and approved commands"
            ),
        ),
        (
            "/sessions",
            tr!(
                "Сохранённые диалоги этого проекта",
                "Saved conversations for this project"
            ),
        ),
        (
            "/new",
            tr!(
                "Начать новый сохраняемый диалог",
                "Start a new saved conversation"
            ),
        ),
        (
            "/temporary",
            tr!(
                "Диалог без сохранения истории",
                "Chat without saving history"
            ),
        ),
        (
            "/clear",
            tr!("Удалить текущую историю", "Delete the current history"),
        ),
        (
            "/undo",
            tr!("Отменить последнюю правку", "Revert the last applied edit"),
        ),
        (
            "/status",
            tr!(
                "Модель, токены, скорость, время сеанса",
                "Model, tokens, speed, session time"
            ),
        ),
        (
            "/reasoning",
            tr!("Уровень рассуждений модели", "Model reasoning level"),
        ),
        (
            "/init",
            tr!(
                "Создать AGENTS.md по структуре проекта",
                "Create AGENTS.md from the project layout"
            ),
        ),
        ("/help", tr!("Команды и клавиши", "Commands and keys")),
        ("/quit", tr!("Вернуться в меню", "Return to the launcher")),
        ("/exit", tr!("Выйти в главное меню", "Exit to the launcher")),
    ]
}

/// Slash menu contents for the current buffer.
fn menu_items(text: &str, closed: bool, navigating: bool) -> Vec<(&'static str, &'static str)> {
    let typed = text.split_whitespace().last().unwrap_or("").to_lowercase();
    if closed || navigating || !typed.starts_with('/') || text.ends_with(char::is_whitespace) {
        return Vec::new();
    }
    commands()
        .into_iter()
        .filter(|(name, _)| name.starts_with(&typed))
        .collect()
}

fn text_key(key: KeyEvent) -> bool {
    // AltGr arrives as Right Alt + Ctrl on Windows and still types text.
    let mask = KeyModifiers::CONTROL | KeyModifiers::ALT;
    !key.modifiers.intersects(mask) || key.modifiers.contains(mask)
}

/// Windows reports Ctrl+A..Ctrl+Z as control characters on a Latin layout and
/// as layout letters on a Cyrillic one (`Ctrl+C` arrives as `с`). Normalize
/// both to the Latin letter with the CONTROL modifier.
fn normalize_key(key: KeyEvent) -> KeyEvent {
    if !key.modifiers.contains(KeyModifiers::CONTROL) || key.modifiers.contains(KeyModifiers::ALT) {
        return key;
    }
    let KeyCode::Char(character) = key.code else {
        return key;
    };
    let value = character as u32;
    let letter = if (1..=26).contains(&value) {
        char::from(b'a' + (value as u8 - 1))
    } else if character.is_ascii() {
        character.to_ascii_lowercase()
    } else {
        match character.to_lowercase().next() {
            // The ЙЦУКЕН layout puts Latin control letters on these keys.
            Some('ф') => 'a',
            Some('и') => 'b',
            Some('с') => 'c',
            Some('в') => 'd',
            Some('у') => 'e',
            Some('а') => 'f',
            Some('п') => 'g',
            Some('р') => 'h',
            Some('ш') => 'i',
            Some('о') => 'j',
            Some('л') => 'k',
            Some('д') => 'l',
            Some('ь') => 'm',
            Some('т') => 'n',
            Some('щ') => 'o',
            Some('з') => 'p',
            Some('й') => 'q',
            Some('к') => 'r',
            Some('ы') => 's',
            Some('е') => 't',
            Some('г') => 'u',
            Some('м') => 'v',
            Some('ц') => 'w',
            Some('ч') => 'x',
            Some('н') => 'y',
            Some('я') => 'z',
            _ => return key,
        }
    };
    KeyEvent::new(KeyCode::Char(letter), KeyModifiers::CONTROL)
}

fn paste_text(text: &str) -> String {
    let mut out = String::new();
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\r' => {
                if characters.peek() == Some(&'\n') {
                    characters.next();
                }
                out.push('\n');
            }
            '\n' => out.push('\n'),
            '\t' => out.push(' '),
            other if !other.is_control() => out.push(other),
            _ => {}
        }
    }
    out
}

struct Editor {
    buffer: Vec<char>,
    caret: usize,
}

impl Editor {
    fn new() -> Self {
        Self {
            buffer: Vec::new(),
            caret: 0,
        }
    }

    fn from(text: &str) -> Self {
        let buffer: Vec<char> = text
            .chars()
            .filter(|character| !character.is_control() || *character == '\n')
            .collect();
        let caret = buffer.len();
        Self { buffer, caret }
    }

    fn prefix(&self) -> String {
        self.buffer[..self.caret].iter().collect()
    }

    fn replace_tail(&mut self, chars: usize, replacement: &str) {
        let start = self.caret.saturating_sub(chars);
        self.buffer.drain(start..self.caret);
        self.caret = start;
        self.insert(replacement.chars());
    }

    fn text(&self) -> String {
        self.buffer.iter().collect()
    }

    fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    fn clear(&mut self) {
        self.buffer.clear();
        self.caret = 0;
    }

    fn paste(&mut self, text: &str) {
        self.insert(paste_text(text).chars());
    }

    fn insert(&mut self, text: impl IntoIterator<Item = char>) {
        if self.buffer.len() >= INPUT_LIMIT {
            return;
        }
        let text: Vec<char> = text
            .into_iter()
            .filter(|c| !c.is_control() || *c == '\n')
            .take(INPUT_LIMIT - self.buffer.len())
            .collect();
        let count = text.len();
        self.buffer.splice(self.caret..self.caret, text);
        self.caret += count;
    }

    fn backspace(&mut self) {
        if self.caret > 0 {
            self.caret -= 1;
            self.buffer.remove(self.caret);
        }
    }

    fn delete(&mut self) {
        if self.caret < self.buffer.len() {
            self.buffer.remove(self.caret);
        }
    }

    fn left(&mut self) {
        self.caret = self.caret.saturating_sub(1);
    }

    fn right(&mut self) {
        self.caret = (self.caret + 1).min(self.buffer.len());
    }

    fn home(&mut self) {
        self.caret = 0;
    }

    fn end(&mut self) {
        self.caret = self.buffer.len();
    }

    fn kill_to_start(&mut self) {
        self.buffer.drain(..self.caret);
        self.caret = 0;
    }

    fn kill_to_end(&mut self) {
        self.buffer.truncate(self.caret);
    }

    fn kill_word(&mut self) {
        let mut start = self.caret;
        while start > 0 && self.buffer[start - 1] == ' ' {
            start -= 1;
        }
        while start > 0 && self.buffer[start - 1] != ' ' {
            start -= 1;
        }
        self.buffer.drain(start..self.caret);
        self.caret = start;
    }

    /// Visible slice and caret column (in cells) for a boxed single-line editor.
    fn view(&self, width: usize) -> (String, usize) {
        if width == 0 {
            return (String::new(), 0);
        }
        let mut start = self.caret;
        let mut caret = 0;
        while let Some(&character) = start.checked_sub(1).and_then(|i| self.buffer.get(i)) {
            let cell = editor_width(character);
            if caret + cell >= width {
                break;
            }
            caret += cell;
            start -= 1;
        }
        let mut shown = String::new();
        let mut used = 0;
        for character in &self.buffer[start..] {
            let cell = editor_width(*character);
            if used + cell > width {
                break;
            }
            shown.push(if *character == '\n' {
                '↵'
            } else {
                *character
            });
            used += cell;
        }
        (shown, caret)
    }

    fn composer(&self, width: usize, max_rows: usize) -> (Vec<String>, usize, usize) {
        let text = self.text();
        let row = self.buffer[..self.caret]
            .iter()
            .filter(|&&c| c == '\n')
            .count();
        let column = self.buffer[..self.caret]
            .iter()
            .rev()
            .take_while(|&&c| c != '\n')
            .count();
        let all: Vec<_> = text.split('\n').collect();
        let start = row
            .saturating_sub(max_rows - 1)
            .min(all.len().saturating_sub(max_rows));
        let mut caret = 0;
        let lines = all
            .iter()
            .enumerate()
            .skip(start)
            .take(max_rows)
            .map(|(i, text)| {
                if i == row {
                    let mut editor = Editor::from(text);
                    editor.caret = column;
                    let (text, col) = editor.view(width);
                    caret = col;
                    text
                } else {
                    clip(text, width)
                }
            })
            .collect();
        (lines, row - start, caret)
    }

    fn completion_prefix(&self) -> String {
        self.buffer[self.caret.saturating_sub(512)..self.caret]
            .iter()
            .collect()
    }
}

fn editor_width(character: char) -> usize {
    if character == '\n' {
        1
    } else {
        character.width().unwrap_or(0)
    }
}

#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
pub(super) struct Receipt {
    input: u32,
    output: u32,
    estimated: bool,
    duration: f32,
    speed: f32,
    ttft: Option<f32>,
}

#[derive(Clone, Copy, Default)]
struct Stats {
    tin: u32,
    tout: u32,
    turns: u32,
    last: Option<Receipt>,
}

#[derive(Clone, Copy)]
struct Turn {
    started: Instant,
    input: u32,
    output: u32,
    estimated: bool,
    first: Option<Instant>,
    request_seconds: f32,
    writing: bool,
    mode: DialogMode,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct CacheKey {
    messages: usize,
    bytes: usize,
    reasoning_bytes: usize,
    details: bool,
    reasoning_details: bool,
    width: usize,
}

struct Cache {
    key: CacheKey,
    lines: Vec<Line>,
}

pub(super) enum StreamControl {
    Continue,
    Stop,
    Command(String),
}

#[derive(Default)]
struct EscapeGate {
    since: Option<Instant>,
    held: bool,
}

impl EscapeGate {
    fn active(&self, now: Instant) -> bool {
        self.since
            .is_some_and(|start| now.saturating_duration_since(start) < Duration::from_secs(7))
    }

    fn press(&mut self, now: Instant, release_events: bool) -> bool {
        if release_events && self.held {
            return false;
        }
        self.held = release_events;
        let stop = self.active(now);
        self.since = (!stop).then_some(now);
        stop
    }
}

fn newline_key(key: KeyEvent) -> bool {
    key.code == KeyCode::Enter
        && key
            .modifiers
            .intersects(KeyModifiers::SHIFT | KeyModifiers::CONTROL)
        || key.code == KeyCode::Char('j') && key.modifiers == KeyModifiers::CONTROL
}

pub struct Screen {
    editor: Editor,
    pastes: Vec<String>,
    submitted_pastes: Vec<String>,
    notice: String,
    scroll: usize,
    details: bool,
    queued: String,
    submit_queued: bool,
    submitted: Vec<String>,
    nav: Option<usize>,
    selected: usize,
    menu_closed: bool,
    armed: bool,
    diagnose: bool,
    planning: bool,
    header_hidden: bool,
    escape: EscapeGate,
    #[cfg(unix)]
    enhanced_keys: bool,
    theme: theme::Theme,
    tick: usize,
    turn: Option<Turn>,
    stats: Stats,
    cache: Option<Cache>,
    context_limit: u32,
    input_limit: u32,
    output_limit: u32,
    input_estimate: Option<u32>,
    reasoning: super::Reasoning,
    reasoning_details: bool,
    background: Vec<u8>,
    desktop_active: bool,
    panel_frame: Option<PanelFrame>,
    viewport: Option<Viewport>,
    selection: Option<Selection>,
    permission_mode: super::permissions::Mode,
    dialog_mode: DialogMode,
    approval_card: bool,
}

impl Screen {
    pub fn open() -> Result<Self> {
        terminal::enable_raw_mode()?;
        let screen = Self {
            editor: Editor::new(),
            pastes: Vec::new(),
            submitted_pastes: Vec::new(),
            notice: String::new(),
            scroll: 0,
            details: false,
            queued: String::new(),
            submit_queued: false,
            submitted: Vec::new(),
            nav: None,
            selected: 0,
            menu_closed: false,
            armed: false,
            diagnose: false,
            planning: false,
            header_hidden: false,
            escape: EscapeGate::default(),
            #[cfg(unix)]
            enhanced_keys: false,
            theme: theme::Theme::Sage,
            tick: 0,
            turn: None,
            stats: Stats::default(),
            cache: None,
            context_limit: CTX_TOKENS,
            input_limit: CTX_TOKENS.saturating_sub(4096),
            output_limit: 4096,
            input_estimate: None,
            reasoning: crate::native::Reasoning::Off,
            reasoning_details: false,
            background: Vec::new(),
            desktop_active: false,
            panel_frame: None,
            viewport: None,
            selection: None,
            permission_mode: Default::default(),
            dialog_mode: DialogMode::Agent,
            approval_card: false,
        };
        if let Err(error) = enter() {
            let _ = terminal::disable_raw_mode();
            return Err(error.into());
        }
        #[cfg(unix)]
        let screen = {
            use crossterm::event::{
                KeyboardEnhancementFlags as Flags, PushKeyboardEnhancementFlags,
            };
            let mut screen = screen;
            screen.enhanced_keys = terminal::supports_keyboard_enhancement().unwrap_or(false)
                && execute!(
                    io::stdout(),
                    PushKeyboardEnhancementFlags(
                        Flags::DISAMBIGUATE_ESCAPE_CODES
                            | Flags::REPORT_EVENT_TYPES
                            | Flags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
                    )
                )
                .is_ok();
            screen
        };
        Ok(screen)
    }

    pub(super) fn pick_directory(
        &mut self,
        directory: &Path,
        root: &Path,
        recent: &crate::recent::RecentFolders,
    ) -> Result<Option<std::path::PathBuf>> {
        self.leave()?;
        let picked = crate::folders::pick(directory, root, recent);
        terminal::enable_raw_mode()?;
        enter()?;
        #[cfg(unix)]
        if self.enhanced_keys {
            use crossterm::event::{
                KeyboardEnhancementFlags as Flags, PushKeyboardEnhancementFlags,
            };
            execute!(
                io::stdout(),
                PushKeyboardEnhancementFlags(
                    Flags::DISAMBIGUATE_ESCAPE_CODES
                        | Flags::REPORT_EVENT_TYPES
                        | Flags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
                )
            )?;
        }
        self.cache = None;
        self.background.clear();
        self.panel_frame = None;
        self.viewport = None;
        self.selection = None;
        picked
    }

    /// Sets the context window of the active model, used by every ctx readout.
    pub fn set_limits(
        &mut self,
        context: u32,
        input: u32,
        output: u32,
        reasoning: super::Reasoning,
    ) {
        self.context_limit = context;
        self.input_limit = input;
        self.output_limit = output;
        self.reasoning = reasoning;
    }

    #[cfg(windows)]
    pub fn desktop_active(&mut self, active: bool) {
        self.desktop_active = active;
    }

    pub fn set_input_estimate(&mut self, tokens: Option<u32>) {
        self.input_estimate = tokens;
    }

    pub fn report(&mut self, title: &str, body: &str) -> Result<()> {
        let (terminal_width, _) = terminal::size()?;
        let width = (usize::from(terminal_width).saturating_sub(8).clamp(36, 96)).saturating_sub(4);
        let mut lines = Vec::new();
        for paragraph in body.lines() {
            if paragraph.trim().is_empty() {
                lines.push(Line::new());
                continue;
            }
            let mut line = Line::new();
            line.push(paragraph, Style::fg(render::FG));
            lines.extend(render::wrap(&line, width));
        }
        self.overlay(title, lines)
    }

    pub fn reset_conversation(&mut self) {
        self.permission_mode = Default::default();
        self.dialog_mode = DialogMode::Agent;
        self.stats = Stats::default();
        self.turn = None;
        self.input_estimate = None;
        self.notice.clear();
        self.editor.clear();
        self.pastes.clear();
        self.submitted_pastes.clear();
        self.queued.clear();
        self.submit_queued = false;
        self.submitted.clear();
        self.nav = None;
        self.scroll = 0;
        self.selected = 0;
        self.menu_closed = false;
        self.armed = false;
        self.cache = None;
        self.selection = None;
        self.viewport = None;
        self.panel_frame = None;
        self.background.clear();
    }
    pub fn notice(&mut self, text: &str) {
        self.notice = render::sanitize(text);
    }

    pub fn help(&mut self) -> Result<()> {
        self.overlay(tr!("Команды и клавиши", "Commands and keys"), help_lines())
    }

    pub fn input(
        &mut self,
        history: &[Message],
        model: &str,
        directory: &Path,
        diagnose: bool,
    ) -> Result<Option<String>> {
        self.diagnose = diagnose;
        if std::mem::take(&mut self.submit_queued) {
            let line = std::mem::take(&mut self.queued);
            if line == "/status" {
                self.status_panel(model, directory)?;
            } else {
                return Ok(Some(line));
            }
        }
        self.draw(history, None, model, directory)?;
        let mut redraw = false;
        loop {
            if redraw && !event::poll(Duration::ZERO)? {
                self.draw(history, None, model, directory)?;
                redraw = false;
            }
            let event = event::read()?;
            if ignored_event(&event) {
                continue;
            }
            redraw = true;
            match event {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    let key = normalize_key(key);
                    let items = self.menu(directory);
                    let count = items.len();
                    let mut edited = false;
                    match key.code {
                        KeyCode::F(6) => {
                            return Ok(Some("/permissions".into()));
                        }
                        _ if newline_key(key) => {
                            self.editor.insert(['\n']);
                            edited = true;
                        }
                        KeyCode::Enter | KeyCode::Tab if count > 0 => {
                            let name = &items[self.selected.min(count - 1)].0;
                            if key.code == KeyCode::Tab && name.starts_with('/') {
                                let prefix = self.editor.prefix();
                                let token = prefix.split_whitespace().last().unwrap_or("");
                                self.editor
                                    .replace_tail(token.chars().count(), &format!("{name} "));
                                self.menu_closed = true;
                            } else if let Some(command) = self.complete(name) {
                                if command == "/status" {
                                    self.status_panel(model, directory)?;
                                } else {
                                    return Ok(Some(command));
                                }
                            }
                            self.selected = 0;
                            continue;
                        }
                        KeyCode::Enter => {
                            let line = self.editor.text();
                            let line = if line.contains('\n') {
                                line
                            } else {
                                line.trim().to_owned()
                            };
                            if line.trim().is_empty() && self.pastes.is_empty() {
                                continue;
                            }
                            if line == "/status" {
                                let _ = self.submit(&line);
                                self.status_panel(model, directory)?;
                                continue;
                            }
                            return Ok(Some(self.submit(&line)));
                        }
                        KeyCode::F(8) if self.desktop_active => {
                            return Ok(Some(super::desktop::STOP_COMMAND.into()));
                        }
                        #[cfg(windows)]
                        KeyCode::F(7) => self.paste_clipboard(),
                        #[cfg(windows)]
                        KeyCode::Char('v') if key.modifiers == KeyModifiers::CONTROL => {
                            self.paste_clipboard();
                        }
                        KeyCode::Esc => {
                            if self.selection.take().is_some() {
                                continue;
                            }
                            if count > 0 {
                                self.menu_closed = true;
                            } else if !self.editor.is_empty() || !self.pastes.is_empty() {
                                self.editor.clear();
                                self.pastes.clear();
                                edited = true;
                            }
                            self.armed = false;
                        }
                        KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
                            if self.copy_selection() {
                                continue;
                            }
                            if !self.editor.is_empty() || !self.pastes.is_empty() {
                                self.editor.clear();
                                self.pastes.clear();
                                edited = true;
                            } else if self.armed {
                                return Ok(None);
                            } else {
                                self.armed = true;
                            }
                        }
                        KeyCode::Char('d') if key.modifiers == KeyModifiers::CONTROL => {
                            if self.editor.is_empty() && self.pastes.is_empty() {
                                return Ok(None);
                            }
                        }
                        KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => {
                            self.scroll = self.scroll.saturating_add(5);
                        }
                        KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => {
                            self.scroll = self.scroll.saturating_sub(5);
                        }
                        KeyCode::Up => {
                            if count > 0 {
                                self.selected = (self.selected.min(count - 1) + count - 1) % count;
                            } else if !self.submitted.is_empty() {
                                let next = self.nav.map_or(self.submitted.len() - 1, |index| {
                                    index.saturating_sub(1)
                                });
                                self.editor = Editor::from(&self.submitted[next]);
                                self.nav = Some(next);
                                edited = true;
                            }
                        }
                        KeyCode::Down => {
                            if count > 0 {
                                self.selected = (self.selected.min(count - 1) + 1) % count;
                            } else if let Some(index) = self.nav {
                                if index + 1 < self.submitted.len() {
                                    self.editor = Editor::from(&self.submitted[index + 1]);
                                    self.nav = Some(index + 1);
                                } else {
                                    self.editor.clear();
                                    self.nav = None;
                                }
                                edited = true;
                            }
                        }
                        KeyCode::Left => self.editor.left(),
                        KeyCode::Right => self.editor.right(),
                        KeyCode::Home => self.editor.home(),
                        KeyCode::End => self.editor.end(),
                        KeyCode::Backspace => {
                            if self.editor.is_empty() {
                                self.pastes.pop();
                            } else {
                                self.editor.backspace();
                            }
                            edited = true;
                        }
                        KeyCode::Delete => {
                            self.editor.delete();
                            edited = true;
                        }
                        KeyCode::Char('a') if key.modifiers == KeyModifiers::CONTROL => {
                            self.editor.home();
                        }
                        KeyCode::Char('e') if key.modifiers == KeyModifiers::CONTROL => {
                            self.editor.end();
                        }
                        KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => {
                            self.editor.kill_to_start();
                            edited = true;
                        }
                        KeyCode::Char('k') if key.modifiers == KeyModifiers::CONTROL => {
                            self.editor.kill_to_end();
                            edited = true;
                        }
                        KeyCode::Char('w') if key.modifiers == KeyModifiers::CONTROL => {
                            self.editor.kill_word();
                            edited = true;
                        }
                        KeyCode::F(2) => {
                            self.selection = None;
                            self.details = !self.details;
                        }
                        KeyCode::F(3) => {
                            self.selection = None;
                            self.reasoning_details = !self.reasoning_details;
                        }
                        KeyCode::PageUp => self.scroll = self.scroll.saturating_add(10),
                        KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(10),
                        KeyCode::Char(character) if text_key(key) && !character.is_control() => {
                            self.editor.insert([character]);
                            edited = true;
                        }
                        _ => {}
                    }
                    if edited {
                        self.selection = None;
                        self.selected = 0;
                        self.menu_closed = false;
                        self.armed = false;
                        if !matches!(key.code, KeyCode::Up | KeyCode::Down) {
                            self.nav = None;
                        }
                    }
                }
                Event::Mouse(mouse) => self.chat_mouse(mouse),
                Event::Paste(text) => {
                    self.selection = None;
                    self.paste_prompt(&text);
                    self.selected = 0;
                    self.menu_closed = false;
                    self.nav = None;
                    self.armed = false;
                }
                _ => {}
            }
        }
    }

    pub fn streaming(
        &mut self,
        history: &[Message],
        output: &str,
        reasoning: &str,
        model: &str,
        directory: &Path,
    ) -> Result<StreamControl> {
        self.track(output, reasoning);
        self.tick = self.tick.wrapping_add(1);
        self.draw(history, Some((output, reasoning)), model, directory)?;
        if event::poll(Duration::from_millis(40))? {
            match event::read()? {
                Event::Key(key) if key.code == KeyCode::Esc => {
                    let release = cfg!(windows);
                    #[cfg(unix)]
                    let release = release || self.enhanced_keys;
                    if key.kind == KeyEventKind::Release {
                        self.escape.held = false;
                    }
                    if key.kind == KeyEventKind::Press && self.escape.press(Instant::now(), release)
                    {
                        return Ok(StreamControl::Stop);
                    }
                }
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    let key = normalize_key(key);
                    let items = self.menu(directory);
                    match key.code {
                        KeyCode::F(8) if self.desktop_active => {
                            return Ok(StreamControl::Command(super::desktop::STOP_COMMAND.into()));
                        }
                        _ if newline_key(key) => {
                            self.editor.insert(['\n']);
                            self.menu_closed = false;
                            self.selected = 0;
                        }
                        #[cfg(windows)]
                        KeyCode::F(7) => self.paste_clipboard(),
                        #[cfg(windows)]
                        KeyCode::Char('v') if key.modifiers == KeyModifiers::CONTROL => {
                            self.paste_clipboard();
                        }
                        KeyCode::F(6) => return Ok(StreamControl::Command("/permissions".into())),
                        KeyCode::F(2) => {
                            self.selection = None;
                            self.details = !self.details;
                        }
                        KeyCode::F(3) => {
                            self.selection = None;
                            self.reasoning_details = !self.reasoning_details;
                        }
                        KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
                            if !self.copy_selection() {
                                return Ok(StreamControl::Stop);
                            }
                        }
                        KeyCode::Enter | KeyCode::Tab if !items.is_empty() => {
                            if let Some(command) =
                                self.complete(&items[self.selected.min(items.len() - 1)].0)
                            {
                                return Ok(StreamControl::Command(command));
                            }
                            self.selected = 0;
                        }
                        KeyCode::Enter if super::slash_command(&self.editor.text()) => {
                            let command = self.editor.text().trim().to_owned();
                            self.editor.clear();
                            return Ok(StreamControl::Command(command));
                        }
                        KeyCode::Up if !items.is_empty() && key.modifiers == KeyModifiers::NONE => {
                            self.selected = (self.selected + items.len() - 1) % items.len()
                        }
                        KeyCode::Down
                            if !items.is_empty() && key.modifiers == KeyModifiers::NONE =>
                        {
                            self.selected = (self.selected + 1) % items.len()
                        }
                        KeyCode::PageUp | KeyCode::Up => {
                            self.scroll = self.scroll.saturating_add(5)
                        }
                        KeyCode::PageDown | KeyCode::Down => {
                            self.scroll = self.scroll.saturating_sub(5)
                        }
                        KeyCode::Left => self.editor.left(),
                        KeyCode::Right => self.editor.right(),
                        KeyCode::Home => self.editor.home(),
                        KeyCode::End => self.editor.end(),
                        KeyCode::Backspace => {
                            if self.editor.is_empty() {
                                self.pastes.pop();
                            } else {
                                self.editor.backspace();
                            }
                            self.selected = 0;
                            self.menu_closed = false;
                        }
                        KeyCode::Delete => {
                            self.editor.delete();
                            self.selected = 0;
                            self.menu_closed = false;
                        }
                        KeyCode::Char(character) if text_key(key) && !character.is_control() => {
                            self.editor.insert([character]);
                            self.selected = 0;
                            self.menu_closed = false;
                        }
                        _ => {}
                    }
                }
                Event::Mouse(mouse) => self.chat_mouse(mouse),
                Event::Paste(text) => {
                    self.paste_prompt(&text);
                    self.selected = 0;
                    self.menu_closed = false;
                }
                _ => {}
            }
        }
        Ok(StreamControl::Continue)
    }

    fn submit(&mut self, line: &str) -> String {
        let line = line.to_owned();
        if !super::slash_command(&line) {
            self.submitted_pastes = std::mem::take(&mut self.pastes);
        }
        if self.submitted.last() != Some(&line) {
            self.submitted.push(line.clone());
        }
        self.editor.clear();
        self.nav = None;
        self.menu_closed = false;
        self.selected = 0;
        self.scroll = 0;
        self.armed = false;
        self.notice.clear();
        line
    }

    pub(super) fn take_pastes(&mut self) -> Vec<String> {
        std::mem::take(&mut self.submitted_pastes)
    }

    pub(super) fn restore_prompt(&mut self, input: &str, pastes: Vec<String>) {
        self.editor = Editor::from(input);
        self.pastes = pastes;
    }

    fn paste_prompt(&mut self, text: &str) {
        if text.len() < 2048 && text.lines().take(8).count() < 8 {
            self.editor.paste(text);
            return;
        }
        if self.pastes.iter().map(String::len).sum::<usize>() + text.len() > PASTE_LIMIT {
            self.notice(tr!(
                "Вставка больше 192 КиБ. Раздели текст на части или приложи файл через @.",
                "The paste exceeds 192 KiB. Split the text or attach a file with @."
            ));
            return;
        }
        self.pastes.push(text.to_owned());
        self.notice.clear();
    }

    #[cfg(windows)]
    fn paste_clipboard(&mut self) {
        match selection::paste() {
            Ok(Some(text)) => {
                self.paste_prompt(&text);
                self.selection = None;
                self.selected = 0;
                self.menu_closed = false;
                self.armed = false;
                self.nav = None;
            }
            Ok(None) => self.notice(tr!(
                "В буфере обмена нет текста.",
                "There is no text on the clipboard."
            )),
            Err(error) => self.notice(&error.to_string()),
        }
    }

    pub(super) fn begin_turn(&mut self, started: Instant) {
        self.notice.clear();
        self.turn = Some(Turn {
            started,
            input: 0,
            output: 0,
            estimated: false,
            first: None,
            request_seconds: 0.0,
            writing: false,
            mode: self.dialog_mode,
        });
    }

    pub(super) fn track(&mut self, output: &str, reasoning: &str) {
        if let Some(turn) = &mut self.turn {
            if !output.is_empty() || !reasoning.is_empty() {
                turn.first.get_or_insert_with(Instant::now);
            }
            turn.writing = !output.is_empty();
        }
    }

    pub(super) fn record_reply(&mut self, input: u32, output: u32, estimated: bool, elapsed: f32) {
        if let Some(turn) = &mut self.turn {
            turn.input = turn.input.saturating_add(input);
            turn.output = turn.output.saturating_add(output);
            turn.estimated |= estimated;
            turn.request_seconds += elapsed;
            turn.writing = false;
        }
    }

    pub(super) fn finish_turn(&mut self) -> Option<Receipt> {
        let turn = self.turn.take()?;
        let duration = turn.started.elapsed().as_secs_f32();
        let speed = turn.output as f32 / turn.request_seconds.max(0.05);
        let ttft = turn
            .first
            .map(|first| first.duration_since(turn.started).as_secs_f32());
        self.stats.tin = self.stats.tin.saturating_add(turn.input);
        self.stats.tout = self.stats.tout.saturating_add(turn.output);
        self.stats.turns = self.stats.turns.saturating_add(1);
        let receipt = Receipt {
            input: turn.input,
            output: turn.output,
            estimated: turn.estimated,
            duration,
            speed,
            ttft,
        };
        self.stats.last = Some(receipt);
        Some(receipt)
    }

    fn menu(&self, directory: &Path) -> Vec<(String, &'static str)> {
        if self.menu_closed || self.nav.is_some() {
            return Vec::new();
        }
        let prefix = self.editor.completion_prefix();
        if let Some(query) = super::attachment_prefix(prefix.rsplit('\n').next().unwrap_or("")) {
            return super::attachment_choices(directory, query);
        }
        if self.editor.buffer.contains(&'\n') {
            return Vec::new();
        }
        menu_items(&prefix, false, false)
            .into_iter()
            .map(|(name, description)| (name.to_owned(), description))
            .collect()
    }

    fn complete(&mut self, item: &str) -> Option<String> {
        let prefix = self.editor.completion_prefix();
        if item.starts_with('@') {
            if let Some(query) = super::attachment_prefix(prefix.rsplit('\n').next().unwrap_or(""))
            {
                self.editor.replace_tail(query.chars().count() + 1, item);
                self.menu_closed = !item.ends_with('/');
            }
            return None;
        }
        let token = prefix.split_whitespace().last().unwrap_or("");
        self.editor.replace_tail(token.chars().count(), "");
        self.menu_closed = true;
        Some(item.to_owned())
    }

    fn chat_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll = self.scroll.saturating_add(5),
            MouseEventKind::ScrollDown => self.scroll = self.scroll.saturating_sub(5),
            MouseEventKind::Down(MouseButton::Left) => {
                self.selection = self.viewport.as_ref().and_then(|view| view.begin(mouse));
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if let (Some(view), Some(selection)) = (&self.viewport, &mut self.selection) {
                    let url = selection.release_link(view, mouse);
                    selection.extend(view, mouse);
                    if let Some(url) = url {
                        self.selection = None;
                        if let Err(error) = selection::open_url(&url) {
                            self.notice(&error.to_string());
                        }
                    }
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let (Some(view), Some(selection)) = (&self.viewport, &mut self.selection) {
                    selection.extend(view, mouse);
                    if usize::from(mouse.row) <= view.top {
                        self.scroll = self.scroll.saturating_add(1);
                    } else if usize::from(mouse.row) >= view.top + view.rows.len().saturating_sub(1)
                    {
                        self.scroll = self.scroll.saturating_sub(1);
                    }
                }
            }
            _ => {}
        }
    }

    fn copy_selection(&mut self) -> bool {
        let (Some(view), Some(selection)) = (&self.viewport, &self.selection) else {
            return false;
        };
        let cached = self
            .cache
            .as_ref()
            .map_or(&[][..], |cache| cache.lines.as_slice());
        let live = view
            .rows
            .iter()
            .enumerate()
            .map(|(i, line)| (view.start + i, line))
            .filter(|(row, _)| *row >= cached.len());
        let Some(text) = selection.text(cached.iter().enumerate().chain(live)) else {
            return false;
        };
        match selection::copy(&text) {
            Ok(()) => self.notice(tr!("Скопировано.", "Copied.")),
            Err(error) => self.notice(&error.to_string()),
        }
        self.armed = false;
        true
    }

    fn draw(
        &mut self,
        history: &[Message],
        pending: Option<(&str, &str)>,
        model: &str,
        directory: &Path,
    ) -> Result<()> {
        let (terminal_width, terminal_height) = terminal::size()?;
        let width = usize::from(terminal_width);
        let height = usize::from(terminal_height);
        self.panel_frame = None;
        self.viewport = None;
        let mut out = Vec::new();
        queue!(
            out,
            BeginSynchronizedUpdate,
            MoveTo(0, 0),
            Clear(ClearType::All)
        )?;
        if width < 40 || height < 14 {
            put_line(
                &mut out,
                0,
                0,
                &Line::of(
                    tr!(
                        "Увеличь окно: нужно минимум 40×14",
                        "Enlarge the window: at least 40×14"
                    ),
                    Style::dim(),
                ),
                width,
            )?;
            queue!(out, EndSynchronizedUpdate)?;
            let mut stdout = io::stdout().lock();
            stdout.write_all(&out)?;
            stdout.flush()?;
            self.background = out;
            return Ok(());
        }
        let content = width.saturating_sub(4).clamp(40, MAX_CONTENT);
        let margin = (width - content) / 2;
        let show_mark = content >= 70 && height >= 26;
        let header_height = if self.header_hidden {
            1 // Keep the top row for the scroll position.
        } else {
            self.header(&mut out, margin, content, model, directory, show_mark)?;
            if show_mark { 10 } else { 5 }
        };

        let streaming = pending.is_some();
        let notice = if self.notice.is_empty() && !self.pastes.is_empty() {
            super::paste_label(
                self.pastes
                    .iter()
                    .map(|text| text.lines().count().max(1))
                    .sum(),
            )
        } else {
            self.notice.clone()
        };
        let menu = self.menu(directory);
        let mut menu_rows = menu.len().clamp(1, MENU_LIMIT);
        menu_rows = menu_rows.min(height.saturating_sub(header_height + 7).max(1));
        let mut notice_rows = usize::from(!notice.is_empty());
        let mut gap_rows = 1usize;
        let composer_rows = (self.editor.buffer.iter().filter(|&&c| c == '\n').count() + 1)
            .min(3)
            .min(height.saturating_sub(header_height + menu_rows + 5).max(1));
        let (conv_rows, notice_y, box_top) = loop {
            let top = height.saturating_sub(2 + composer_rows + menu_rows);
            let gap = gap_rows.min(top.saturating_sub(header_height + 2));
            let extras_end = top.saturating_sub(gap);
            let notice = if notice_rows > 0 {
                Some(extras_end.saturating_sub(notice_rows))
            } else {
                None
            };
            let conv_end = notice.unwrap_or(extras_end);
            let rows = conv_end.saturating_sub(header_height);
            if rows >= 2 {
                break (rows, notice, top);
            }
            if notice_rows > 0 {
                notice_rows = 0;
                continue;
            }
            if gap_rows > 0 {
                gap_rows = 0;
                continue;
            }
            if menu_rows > 1 {
                menu_rows -= 1;
                continue;
            }
            break (rows, notice, top);
        };

        if conv_rows > 0 {
            let key = CacheKey {
                messages: history.len(),
                bytes: history.iter().map(|message| message.content.len()).sum(),
                reasoning_bytes: history
                    .iter()
                    .map(|message| {
                        message
                            .reasoning
                            .as_deref()
                            .map_or(0, |reasoning| reasoning.len())
                    })
                    .sum(),
                details: self.details,
                reasoning_details: self.reasoning_details,
                width: content,
            };
            if self.cache.as_ref().map(|cache| cache.key) != Some(key) {
                let lines = history_lines(history, content, self.details, self.reasoning_details);
                self.cache = Some(Cache { key, lines });
            }
            let mut pending_lines = Vec::new();
            if let Some((text, reasoning)) = pending {
                pending_lines.extend(reasoning_lines(reasoning, content, false, true));
                if !text.is_empty() {
                    pending_lines.extend(render::assistant(text, content));
                }
            }
            let total =
                self.cache.as_ref().map_or(0, |cache| cache.lines.len()) + pending_lines.len();
            let max_scroll = total.saturating_sub(conv_rows);
            let scroll = self.scroll.min(max_scroll);
            self.scroll = scroll;
            let end = total - scroll;
            let start = end.saturating_sub(conv_rows);
            if let Some(cache) = &self.cache {
                let viewport = Viewport {
                    top: header_height,
                    left: margin,
                    width: content,
                    start,
                    rows: cache
                        .lines
                        .iter()
                        .chain(pending_lines.iter())
                        .skip(start)
                        .take(conv_rows)
                        .cloned()
                        .collect(),
                };
                if self
                    .selection
                    .as_ref()
                    .is_some_and(|range| range.width() != content)
                {
                    self.selection = None;
                }
                for (row, line) in viewport.rows.iter().enumerate() {
                    if let Some(highlight) = self
                        .selection
                        .as_ref()
                        .and_then(|range| range.highlight(start + row, line))
                    {
                        put_line(&mut out, margin, header_height + row, &highlight, content)?;
                    } else {
                        put_line(&mut out, margin, header_height + row, line, content)?;
                    }
                }
                self.viewport = Some(viewport);
            }
            if start > 0 {
                let hint = Line::of(format!("↑ {start}"), Style::dim());
                let used = hint.width();
                put_line(
                    &mut out,
                    margin + content.saturating_sub(used),
                    header_height.saturating_sub(1),
                    &hint,
                    used,
                )?;
            }
        }

        if let Some(y) = notice_y {
            let mut line = Line::of(
                format!("  {notice}"),
                if self.notice.is_empty() {
                    Style::fg(render::WARN)
                } else {
                    Style::dim()
                },
            );
            if line.width() > content {
                line = line.truncate(content.saturating_sub(1));
                line.push("…", Style::dim());
            }
            put_line(&mut out, margin, y, &line, content)?;
        }
        let inner = content - 6;
        let (texts, caret_row, caret) = self.editor.composer(inner, composer_rows);
        let border = Style::fg(render::LINE);
        let mut top = Line::new();
        top.push("╭", border);
        if self.planning {
            top.push("─", border);
            top.push(
                " PLAN MODE ",
                Style::fg(render::CODE).with_bg(render::BRAND).bold(),
            );
        }
        top.push("─".repeat(content.saturating_sub(top.width() + 1)), border);
        top.push("╮", border);
        put_line(&mut out, margin, box_top, &top, content)?;
        for (index, text) in texts.iter().enumerate() {
            let mut row = Line::new();
            row.push("│ ", border);
            row.push(if index == 0 { "› " } else { "  " }, Style::brand());
            row.push(text.as_str(), Style::fg(render::FG));
            row.pad_to(content - 2, Style::plain());
            row.push(" │", border);
            put_line(&mut out, margin, box_top + 1 + index, &row, content)?;
        }
        let mut foot = Line::new();
        foot.push("╰", border);
        foot.push("─".repeat(content.saturating_sub(2)), border);
        foot.push("╯", border);
        put_line(
            &mut out,
            margin,
            box_top + composer_rows + 1,
            &foot,
            content,
        )?;

        if streaming && self.escape.active(Instant::now()) {
            put_line(
                &mut out,
                margin,
                box_top + composer_rows + 2,
                &Line::of(
                    tr!(
                        "  Esc ещё раз — прервать ответ",
                        "  Press Esc again to stop the reply"
                    ),
                    Style::fg(render::WARN),
                ),
                content,
            )?;
        } else if menu.is_empty() {
            put_line(
                &mut out,
                margin,
                box_top + composer_rows + 2,
                &self.footer(model, content, streaming),
                content,
            )?;
        } else {
            let window = self.selected.saturating_sub(menu_rows.saturating_sub(1));
            for (index, (name, description)) in menu.iter().enumerate().skip(window).take(menu_rows)
            {
                let selected = index == self.selected;
                let mut line = Line::new();
                line.push("  ", Style::plain());
                line.push(
                    if selected { "› " } else { "  " },
                    if selected {
                        Style::brand()
                    } else {
                        Style::plain()
                    },
                );
                line.push(
                    format!("{name:<14}"),
                    if selected {
                        Style::brand()
                    } else {
                        Style::fg(render::FG)
                    },
                );
                line.push(
                    *description,
                    if selected {
                        Style::brand()
                    } else {
                        Style::dim()
                    },
                );
                put_line(
                    &mut out,
                    margin,
                    box_top + composer_rows + 2 + index - window,
                    &line,
                    content,
                )?;
            }
        }

        let column = margin + 4 + caret;
        queue!(
            out,
            MoveTo(column as u16, (box_top + 1 + caret_row) as u16),
            Show
        )?;
        queue!(out, EndSynchronizedUpdate)?;
        let mut stdout = io::stdout().lock();
        stdout.write_all(&out)?;
        stdout.flush()?;
        self.background = out;
        Ok(())
    }

    fn header(
        &self,
        out: &mut impl Write,
        margin: usize,
        content: usize,
        model: &str,
        directory: &Path,
        mark: bool,
    ) -> io::Result<()> {
        let path = render::tail(&crate::folders::display(directory), content);
        let hint = if self.desktop_active {
            tr!(
                "F8 · остановить управление окном",
                "F8 · stop window control"
            )
        } else {
            tr!(
                "/ команды · Esc ×2 прервать · Ctrl+C выйти",
                "/ commands · Esc twice to interrupt · Ctrl+C to quit"
            )
        };
        if !mark {
            put_line(out, margin, 0, &self.brand_line(), content)?;
            put_line(out, margin, 1, &self.model_line(model), content)?;
            put_line(out, margin, 2, &Line::of(path, Style::dim()), content)?;
            put_line(out, margin, 3, &Line::of(hint, Style::dim()), content)?;
            return Ok(());
        }
        let brand = self.brand_line();
        let model_line = self.model_line(model);
        let path_line = Line::of(
            render::tail(&path, content.saturating_sub(20)),
            Style::dim(),
        );
        let hint_line = Line::of(hint, Style::dim());
        let info = |row: usize| match row {
            2 => Some(&brand),
            3 => Some(&model_line),
            4 => Some(&path_line),
            6 => Some(&hint_line),
            _ => None,
        };
        for (row, mark_line) in render::logo().iter().enumerate() {
            put_line(out, margin, 1 + row, mark_line, 16)?;
            if let Some(line) = info(row) {
                put_line(out, margin + 18, 1 + row, line, content.saturating_sub(18))?;
            }
        }
        Ok(())
    }

    fn brand_line(&self) -> Line {
        let mut line = Line::new();
        line.push("Karry", Style::fg(render::FG).bold());
        line.push("Code", Style::fg(render::BRAND).bold());
        line.push(format!(" v{}", crate::RELEASE_VERSION), Style::dim());
        line
    }

    fn model_line(&self, model: &str) -> Line {
        let mut line = Line::new();
        line.push(model, Style::dim());
        if self.context_limit > 0 {
            line.push(
                format!(
                    " · ctx {} · in {} · out {}",
                    render::human(self.context_limit),
                    render::human(self.input_limit),
                    render::human(self.output_limit)
                ),
                Style::dim(),
            );
        }
        if !matches!(
            self.reasoning,
            super::Reasoning::Off | super::Reasoning::ProviderDefault
        ) {
            line.push(
                format!(" · r {}", self.reasoning.label()),
                Style::fg(render::BRAND),
            );
        }
        if self.diagnose {
            line.push(
                tr!(" · диагностика", " · diagnose"),
                Style::fg(render::BRAND),
            );
        }
        if self.turn.map_or(self.dialog_mode, |turn| turn.mode) == DialogMode::Chat {
            line.push(format!(" · {}", DialogMode::Chat.label()), Style::dim());
        }
        line
    }

    fn input_percentage(&self) -> Option<u64> {
        self.input_estimate
            .map(|used| u64::from(used) * 100 / u64::from(self.input_limit.max(1)))
    }

    fn footer(&self, model: &str, width: usize, streaming: bool) -> Line {
        if self.armed && !streaming {
            return self.status_line(model, width);
        }
        let over_limit = self
            .input_estimate
            .is_some_and(|used| used > self.input_limit);
        let indicator = render::context_indicator(self.input_percentage(), over_limit);
        let indicator_start = width.saturating_sub(indicator.width());
        let left_width = indicator_start.saturating_sub(2);
        let mut line = if streaming {
            self.spinner_line(left_width)
        } else {
            self.status_line(model, left_width)
        };
        line.pad_to(indicator_start, Style::plain());
        line.extend(indicator);
        line.truncate(width)
    }

    fn status_line(&self, model: &str, width: usize) -> Line {
        let model = if width < 60 {
            model.rsplit(" / ").next().unwrap_or(model)
        } else {
            model
        };
        if self.armed {
            let mut line = Line::new();
            line.push("  ", Style::plain());
            line.push(
                tr!(
                    "Нажми Ctrl-C ещё раз, чтобы выйти",
                    "Press Ctrl-C again to exit"
                ),
                Style::dim(),
            );
            return line.truncate(width);
        }
        let mut segments: Vec<Line> = Vec::new();
        let mut head = Line::new();
        head.push("  ", Style::plain());
        head.push("● ", Style::brand());
        head.push(model, Style::fg(render::FG));
        segments.push(head);
        if !matches!(
            self.reasoning,
            super::Reasoning::Off | super::Reasoning::ProviderDefault
        ) {
            segments.push(Line::of(
                format!("r {}", self.reasoning.label()),
                Style::dim(),
            ));
        }
        if self.scroll > 0 {
            segments.push(Line::of(format!("↑{}", self.scroll), Style::dim()));
        }
        join_segments(&segments, width)
    }

    pub(super) fn reset_stream_escape(&mut self) {
        self.escape = EscapeGate::default();
    }

    fn spinner_line(&self, width: usize) -> Line {
        let frame = render::SPIN[(self.tick / 2) % render::SPIN.len()];
        let Some(turn) = self.turn else {
            return Line::of(tr!("  Думаю…", "  Thinking…"), Style::brand());
        };
        let elapsed = render::dur(turn.started.elapsed().as_secs());
        let stage = if turn.writing {
            tr!("Пишу", "Writing")
        } else {
            tr!("Думаю", "Thinking")
        };
        if width < 58 {
            let mut line = Line::new();
            line.push("  ", Style::plain());
            line.push(frame, Style::brand());
            line.push(format!(" {stage}… {elapsed} · Esc ×2"), Style::dim());
            return line.truncate(width);
        }
        let mut line = Line::new();
        line.push("  ", Style::plain());
        line.push(frame, Style::brand());
        line.push(format!(" {stage}…"), Style::brand());
        line.push(
            trf!(
                " ({elapsed} · Esc ×2 — прервать)",
                " ({elapsed} · Esc twice to interrupt)"
            ),
            Style::dim(),
        );
        line
    }

    fn status_panel(&mut self, model: &str, directory: &Path) -> Result<()> {
        let show_limit = |limit| {
            if limit == 0 {
                "—".into()
            } else {
                render::human(limit)
            }
        };
        let used = self
            .input_estimate
            .map_or("—".into(), |used| format!("~{used}"));
        let remaining = self.input_estimate.map_or("—".into(), |used| {
            format!("~{}", self.input_limit.saturating_sub(used))
        });
        let available = self.input_estimate.map_or("—".into(), |used| {
            format!(
                "~{}",
                super::available_output(
                    self.context_limit,
                    self.input_limit,
                    self.output_limit,
                    used
                )
            )
        });
        let mode = if self.planning {
            tr!("планирование", "plan")
        } else if self.diagnose {
            tr!("диагностика", "diagnose")
        } else {
            tr!("кодинг", "code")
        };
        let rows = [
            (tr!("модель", "model"), model.to_owned()),
            (tr!("окно", "window"), show_limit(self.context_limit)),
            (
                tr!("лимит входа", "input limit"),
                show_limit(self.input_limit),
            ),
            (
                tr!("лимит выхода", "output limit"),
                show_limit(self.output_limit),
            ),
            (tr!("занято (оценка)", "used (estimate)"), used),
            (tr!("осталось", "remaining"), remaining),
            (tr!("доступный выход", "available output"), available),
            (
                tr!("reasoning", "reasoning"),
                if self.reasoning == super::Reasoning::Off {
                    tr!("выкл", "off").to_owned()
                } else {
                    self.reasoning.label().to_owned()
                },
            ),
            (
                tr!("токены", "tokens"),
                format!(
                    "↑ ~{}  ↓ ~{}",
                    render::human(self.stats.tin),
                    render::human(self.stats.tout)
                ),
            ),
            (
                tr!("скорость", "speed"),
                self.stats
                    .last
                    .map_or("—".into(), |reply| format!("{:.0} tok/s", reply.speed)),
            ),
            (
                tr!("последний", "last"),
                self.stats
                    .last
                    .map_or("—".into(), |reply| format!("{:.1}s", reply.duration)),
            ),
            (
                tr!("сеанс", "session"),
                trf!("{} запросов", "{} turns", self.stats.turns),
            ),
            (tr!("режим", "mode"), mode.to_owned()),
            (
                tr!("диалог", "conversation"),
                self.turn
                    .map_or(self.dialog_mode, |turn| turn.mode)
                    .label()
                    .into(),
            ),
            (tr!("проект", "project"), crate::folders::display(directory)),
        ];
        let mut body = Vec::new();
        for (key, value) in rows {
            let mut line = Line::new();
            line.push(format!("{key:<16} "), Style::dim());
            line.push(value, Style::fg(render::FG));
            body.push(line);
        }
        self.overlay(tr!("Состояние сеанса", "Session status"), body)
    }

    fn overlay(&mut self, title: &str, body: Vec<Line>) -> Result<()> {
        let mut scroll = 0usize;
        loop {
            let panel = self.draw_panel(
                title,
                tr!("Esc / Enter — закрыть", "Esc / Enter — close"),
                &body,
                scroll,
                None,
            )?;
            scroll = scroll.min(body.len().saturating_sub(panel.visible));
            match read_ui_event()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    let key = normalize_key(key);
                    match key.code {
                        KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => return Ok(()),
                        KeyCode::Up => scroll = scroll.saturating_sub(1),
                        KeyCode::Down => scroll = scroll.saturating_add(1),
                        KeyCode::PageUp => scroll = scroll.saturating_sub(10),
                        KeyCode::PageDown => scroll = scroll.saturating_add(10),
                        KeyCode::Home => scroll = 0,
                        KeyCode::End => scroll = body.len(),
                        KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
                            return Ok(());
                        }
                        _ => {}
                    }
                }
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::ScrollUp => scroll = scroll.saturating_sub(3),
                    MouseEventKind::ScrollDown => scroll = scroll.saturating_add(3),
                    _ => {}
                },
                _ => {}
            }
        }
    }
}

/// Border geometry of a centered panel, enough for cursor placement.
struct Panel {
    visible: usize,
}

struct PanelFrame {
    size: (u16, u16),
    left: usize,
    top: usize,
    width: usize,
    rows: Vec<Line>,
    cursor: Option<(usize, usize)>,
}

impl Screen {
    fn draw_panel(
        &mut self,
        title: &str,
        footer: &str,
        body: &[Line],
        scroll: usize,
        cursor: Option<(usize, usize)>,
    ) -> Result<Panel> {
        let (terminal_width, terminal_height) = terminal::size()?;
        let width = usize::from(terminal_width);
        let height = usize::from(terminal_height);
        let panel_width = width.saturating_sub(8).clamp(36, 96).min(width);
        let inner = panel_width.saturating_sub(4);
        let panel_height = (body.len() + 2).min(height.saturating_sub(2)).max(4);
        let visible = panel_height.saturating_sub(2);
        let scroll = scroll.min(body.len().saturating_sub(visible));
        let left = width.saturating_sub(panel_width) / 2;
        let top = if self.approval_card {
            height.saturating_sub(panel_height + 4)
        } else {
            height.saturating_sub(panel_height) / 2
        };
        let border = Style::fg(if self.approval_card {
            render::WARN
        } else {
            render::LINE
        });
        let edge = |left: &str, right: &str, text: &str, style: Style| {
            let mut line = Line::of(left, border);
            let limit = panel_width.saturating_sub(3);
            let mut label = Line::of(format!(" {text} "), style);
            if label.width() > limit && limit > 0 {
                label = label.truncate(limit - 1);
                label.push("…", style);
            }
            line.extend(label);
            let tail = panel_width.saturating_sub(line.width() + 1);
            if tail > 0 {
                line.push("─".repeat(tail), border);
            }
            line.push(right, border);
            line
        };
        let mut rows = Vec::with_capacity(visible + 2);
        let title_style = if self.approval_card {
            border.bold()
        } else {
            Style::brand().bold()
        };
        rows.push(edge("╭─", "╮", title, title_style));
        for row in 0..visible {
            let mut line = Line::of("│ ", border);
            if let Some(source) = body.get(scroll + row) {
                if source.width() > inner && inner > 1 {
                    line.extend(source.truncate(inner - 1));
                    line.push("…", Style::dim());
                } else {
                    line.extend(source.clone());
                }
            }
            line.pad_to(panel_width.saturating_sub(2), Style::plain());
            line.push(" │", border);
            rows.push(line);
        }
        rows.push(edge("╰─", "╯", footer.trim(), Style::dim()));
        let cursor = cursor.and_then(|(row, column)| {
            let row = row.checked_sub(scroll)?;
            (row < visible).then_some((
                left + 2 + column.min(inner.saturating_sub(1)),
                top + 1 + row,
            ))
        });
        let size = (terminal_width, terminal_height);
        let previous = self.panel_frame.as_ref().filter(|previous| {
            previous.size == size
                && previous.left == left
                && previous.top == top
                && previous.width == panel_width
                && previous.rows.len() == rows.len()
        });
        let changed = |row: usize| previous.is_none_or(|frame| frame.rows[row] != rows[row]);
        if (0..rows.len()).any(changed) || previous.is_none_or(|frame| frame.cursor != cursor) {
            let mut out = io::BufWriter::new(io::stdout().lock());
            queue!(out, BeginSynchronizedUpdate)?;
            if previous.is_none() {
                if self.background.is_empty()
                    || self
                        .panel_frame
                        .as_ref()
                        .is_some_and(|frame| frame.size != size)
                {
                    queue!(out, MoveTo(0, 0), Clear(ClearType::All))?;
                } else {
                    out.write_all(&self.background)?;
                    queue!(out, BeginSynchronizedUpdate)?;
                }
            }
            for (row, line) in rows.iter().enumerate().filter(|(row, _)| changed(*row)) {
                put_line(&mut out, left, top + row, line, panel_width)?;
            }
            if let Some((x, y)) = cursor {
                queue!(out, MoveTo(x as u16, y as u16), Show)?;
            } else {
                queue!(out, Hide)?;
            }
            queue!(out, EndSynchronizedUpdate)?;
            out.flush()?;
            self.panel_frame = Some(PanelFrame {
                size,
                left,
                top,
                width: panel_width,
                rows,
                cursor,
            });
        }
        Ok(Panel { visible })
    }

    /// Yes/no panel for destructive or privileged actions. Enter allows, Esc
    /// declines; the body is wrapped and capped so it can never push the hints
    /// off screen.
    pub fn confirm(&mut self, title: &str, body: &str) -> Result<bool> {
        let (terminal_width, _) = terminal::size()?;
        let width = (usize::from(terminal_width).saturating_sub(8).clamp(36, 96)).saturating_sub(4);
        let mut lines = Vec::new();
        for paragraph in body.lines() {
            if paragraph.trim().is_empty() {
                lines.push(Line::new());
                continue;
            }
            let mut line = Line::new();
            line.push(paragraph, Style::fg(render::FG));
            lines.extend(render::wrap(&line, width));
        }
        if lines.len() > 10 {
            lines.truncate(9);
            let mut line = Line::new();
            line.push("…", Style::dim());
            lines.push(line);
        }
        loop {
            self.draw_panel(
                title,
                tr!(" Enter — да · Esc — нет ", " Enter — yes · Esc — no "),
                &lines,
                0,
                None,
            )?;
            match read_ui_event()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    let key = normalize_key(key);
                    match key.code {
                        KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('д') => {
                            return Ok(true);
                        }
                        KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('н') => {
                            return Ok(false);
                        }
                        KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
                            return Ok(false);
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }
}

impl Screen {
    /// A transient panel for a bounded provider or MCP request.
    pub fn busy(&mut self, text: &str) -> Result<()> {
        self.draw_panel(text, "", &[Line::new()], 0, None)?;
        Ok(())
    }
}

pub enum McpAction {
    Close,
    Restart,
    Approve(usize),
    Import(Vec<usize>),
}

impl Screen {
    /// Scrollable preview used by `/init`: Enter accepts, Esc declines.
    pub fn stage_input(&mut self, input: String) {
        self.queued = input;
        self.submit_queued = true;
    }

    pub fn ask_text(&mut self, title: &str, secret: bool) -> Result<Option<String>> {
        let mut input = String::new();
        loop {
            let mut line = Line::new();
            let shown = if secret {
                "•".repeat(input.chars().count())
            } else {
                input.clone()
            };
            let shown_width = shown.width();
            line.push(shown, Style::fg(render::FG));
            self.draw_panel(
                title,
                tr!(
                    " Enter — принять · Esc — назад ",
                    " Enter accept · Esc back "
                ),
                &[line],
                0,
                Some((0, shown_width)),
            )?;
            match read_ui_event()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
                    KeyCode::Esc => return Ok(None),
                    KeyCode::Enter if !input.trim().is_empty() => return Ok(Some(input)),
                    KeyCode::Backspace => {
                        input.pop();
                    }
                    KeyCode::Char(c)
                        if !key.modifiers.contains(KeyModifiers::CONTROL)
                            && !c.is_control()
                            && input.len() < 2048 =>
                    {
                        input.push(c)
                    }
                    _ => {}
                },
                Event::Paste(text) => {
                    input.extend(
                        text.chars()
                            .filter(|c| !c.is_control())
                            .take(2048usize.saturating_sub(input.len())),
                    );
                }
                _ => {}
            }
        }
    }

    pub fn choose(&mut self, title: &str, options: &[String]) -> Result<Option<usize>> {
        if options.is_empty() {
            return Ok(None);
        }
        let mut selected = options
            .iter()
            .position(|option| option.starts_with('●'))
            .unwrap_or(0);
        loop {
            let lines: Vec<_> = options
                .iter()
                .enumerate()
                .map(|(index, option)| {
                    let mut row = Line::new();
                    if index == selected {
                        row.push(" › ", Style::brand().bold());
                    } else {
                        row.push("   ", Style::plain());
                    }
                    row.push(option, Style::fg(render::FG));
                    row
                })
                .collect();
            let visible = usize::from(terminal::size()?.1).saturating_sub(5).max(1);
            let scroll = selected.saturating_add(1).saturating_sub(visible);
            self.draw_panel(
                title,
                tr!(
                    " ↑↓ — выбор · Enter — открыть · Esc — назад ",
                    " ↑↓ select · Enter open · Esc back "
                ),
                &lines,
                scroll,
                None,
            )?;
            if let Event::Key(key) = read_ui_event()?
                && key.kind != KeyEventKind::Release
            {
                match key.code {
                    KeyCode::Up => selected = selected.saturating_sub(1),
                    KeyCode::Down => selected = (selected + 1).min(options.len() - 1),
                    KeyCode::Enter => return Ok(Some(selected)),
                    KeyCode::Esc => return Ok(None),
                    _ => {}
                }
            }
        }
    }

    pub fn tool_background(
        &mut self,
        history: &[Message],
        model: &str,
        directory: &Path,
    ) -> Result<()> {
        self.draw(history, None, model, directory)
    }

    pub fn wait_for_tool<T>(
        &mut self,
        label: &str,
        cancel: &std::sync::atomic::AtomicBool,
        worker: std::thread::ScopedJoinHandle<'_, T>,
    ) -> Result<T> {
        let message = trf!(
            "{} · F6 — разрешения · Esc — остановить",
            "{} · F6 permissions · Esc to stop",
            label
        );
        self.busy(&message)?;
        while !worker.is_finished() {
            if self.poll_tool_stop(&message)? {
                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
        worker
            .join()
            .map_err(|_| anyhow::anyhow!(tr!("Инструмент остановился", "Tool worker stopped")))
    }

    pub(super) fn wait_for_web<T: Send + 'static>(
        &mut self,
        label: &str,
        task: impl FnOnce(&std::sync::atomic::AtomicBool) -> Result<T> + Send + 'static,
    ) -> Result<Option<Result<T>>> {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };

        let message = trf!(
            "{} · F6 — разрешения · Esc — отменить",
            "{} · F6 permissions · Esc to cancel",
            label
        );
        self.busy(&message)?;
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let worker = std::thread::spawn(move || task(&worker_cancel));
        let started = Instant::now();
        while !worker.is_finished() {
            let stop = self.poll_tool_stop(&message);
            if !matches!(stop, Ok(false)) {
                cancel.store(true, Ordering::Relaxed);
                return stop.map(|_| None);
            }
            if started.elapsed() >= super::web::TIMEOUT && !worker.is_finished() {
                cancel.store(true, Ordering::Relaxed);
                return Ok(Some(Err(anyhow::anyhow!(tr!(
                    "Веб-запрос занял слишком много времени. Проверь соединение и настройку /search.",
                    "The web request timed out. Check your connection and /search settings."
                )))));
            }
        }
        Ok(Some(worker.join().unwrap_or_else(|_| {
            Err(anyhow::anyhow!(tr!(
                "Веб-запрос остановился",
                "Web worker stopped"
            )))
        })))
    }

    fn poll_tool_stop(&mut self, message: &str) -> Result<bool> {
        if !event::poll(Duration::from_millis(55))? {
            return Ok(false);
        }
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                let key = normalize_key(key);
                if key.code == KeyCode::F(6) {
                    self.permissions_menu()?;
                    self.busy(message)?;
                }
                return Ok(key.code == KeyCode::Esc
                    || (key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL));
            }
            Event::Resize(_, _) => self.busy(message)?,
            _ => {}
        }
        Ok(false)
    }

    pub fn permission_mode(&self) -> super::permissions::Mode {
        self.permission_mode
    }

    pub(super) fn dialog_mode(&self) -> DialogMode {
        self.dialog_mode
    }

    pub(super) fn set_dialog_mode(&mut self, mode: DialogMode) {
        self.dialog_mode = mode;
    }

    pub fn permissions_menu(&mut self) -> Result<()> {
        let was_approval = std::mem::replace(&mut self.approval_card, true);
        let result = self.edit_permissions();
        self.approval_card = was_approval;
        result
    }

    fn edit_permissions(&mut self) -> Result<()> {
        use super::permissions::Mode;
        let entries = [
            trf!(
                "Режим диалога · {}",
                "Conversation · {}",
                self.dialog_mode.label()
            ),
            trf!(
                "Подтверждение действий · {}",
                "Action approvals · {}",
                self.permission_mode.label()
            ),
        ];
        match self.choose(tr!("Разрешения · F6", "Permissions · F6"), &entries)? {
            Some(0) => {
                let modes = DialogMode::ALL;
                let choices = modes.map(|mode| {
                    format!(
                        "{} {}",
                        if mode == self.dialog_mode { "●" } else { " " },
                        mode.label()
                    )
                });
                if let Some(index) = self.choose(
                    tr!("Режим текущего диалога", "Current conversation mode"),
                    &choices,
                )? {
                    self.dialog_mode = modes[index];
                    if self.turn.is_some_and(|turn| turn.mode != self.dialog_mode) {
                        self.notice(&trf!(
                            "{} · со следующего сообщения.",
                            "{} · applies to the next message.",
                            self.dialog_mode.label()
                        ));
                    } else {
                        self.notice(self.dialog_mode.label());
                    }
                }
            }
            Some(1) => {
                let modes = Mode::ALL;
                let choices = modes.map(|mode| {
                    format!(
                        "{} {}",
                        if mode == self.permission_mode {
                            "●"
                        } else {
                            " "
                        },
                        mode.label()
                    )
                });
                if let Some(index) =
                    self.choose(tr!("Подтверждение действий", "Action approvals"), &choices)?
                {
                    let mode = modes[index];
                    if mode != Mode::Full
                        || self.review_card(mode.label(), mode.description(), false)?
                    {
                        self.permission_mode = mode;
                        self.notice(mode.label());
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub(super) fn request_permission(
        &mut self,
        mode: super::permissions::Mode,
        reason: &str,
    ) -> Result<bool> {
        let detail = format!(
            "{} → {}\n{}\n\n{}",
            self.permission_mode.label(),
            mode.label(),
            mode.description(),
            reason
        );
        if self.review(
            tr!("Агент запрашивает разрешение", "Agent requests permission"),
            &detail,
        )? {
            self.permission_mode = mode;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub(super) fn choose_theme(&mut self, current: theme::Theme) -> Result<Option<theme::Theme>> {
        let mut selected = theme::Theme::ALL
            .iter()
            .position(|&item| item == current)
            .unwrap_or_default();
        loop {
            let rows: Vec<_> = theme::Theme::ALL
                .iter()
                .enumerate()
                .map(|(index, &item)| {
                    let mut line = Line::of(
                        if index == selected { " › " } else { "   " },
                        Style::plain(),
                    );
                    line.push(if item == current { "● " } else { "  " }, Style::plain());
                    line.push(item.label(), item.preview(Style::brand().bold()));
                    line.push("  ", Style::plain());
                    line.push(
                        " code ",
                        item.preview(Style::fg(render::CODEFG).with_bg(render::CODE)),
                    );
                    line
                })
                .collect();
            self.draw_panel(
                tr!("Тема агента", "Agent theme"),
                tr!(
                    " ↑↓ Выбор · Enter Применить · Esc Назад ",
                    " ↑↓ Select · Enter Apply · Esc Back "
                ),
                &rows,
                selected.saturating_sub(1),
                None,
            )?;
            if let Event::Key(key) = read_ui_event()? {
                match key.code {
                    KeyCode::Up => selected = selected.saturating_sub(1),
                    KeyCode::Down => selected = (selected + 1).min(theme::Theme::ALL.len() - 1),
                    KeyCode::Enter => return Ok(Some(theme::Theme::ALL[selected])),
                    KeyCode::Esc => return Ok(None),
                    _ => {}
                }
            }
        }
    }

    pub(super) fn set_theme(&mut self, selected: theme::Theme) {
        self.theme = selected;
        theme::set(selected);
        self.cache = None;
        self.background.clear();
        self.panel_frame = None;
    }

    pub(super) fn toggle_header(&mut self) {
        self.header_hidden = !self.header_hidden;
        self.cache = None;
    }

    pub(super) fn set_planning(&mut self, planning: bool) {
        self.planning = planning;
    }

    pub(super) fn review_plan(&mut self, text: &str) -> Result<bool> {
        let title = tr!("Перейти к реализации?", "Switch to coding mode?");
        self.review_card(title, text, false)
    }

    pub fn review(&mut self, title: &str, text: &str) -> Result<bool> {
        let was_approval = std::mem::replace(&mut self.approval_card, true);
        let result = self.review_card(title, text, true);
        self.approval_card = was_approval;
        result
    }

    fn review_card(&mut self, title: &str, text: &str, permissions_shortcut: bool) -> Result<bool> {
        let (terminal_width, _) = terminal::size()?;
        let width = (usize::from(terminal_width).saturating_sub(8).clamp(36, 96)).saturating_sub(4);
        let mut lines = Vec::new();
        for paragraph in text.lines() {
            if paragraph.trim().is_empty() {
                lines.push(Line::new());
                continue;
            }
            let mut line = Line::new();
            line.push(paragraph, Style::fg(render::FG));
            lines.extend(render::wrap(&line, width));
        }
        while event::poll(Duration::ZERO)? {
            let _ = event::read()?;
        }
        let mut scroll = 0usize;
        loop {
            let panel = self.draw_panel(
                title,
                if permissions_shortcut {
                    tr!(
                        " Enter — подтвердить · Esc — отмена · F6 — разрешения · ↑↓ — прокрутка ",
                        " Enter confirm · Esc cancel · F6 permissions · ↑↓ scroll "
                    )
                } else {
                    tr!(
                        " Enter — подтвердить · Esc — назад · ↑↓ — прокрутка ",
                        " Enter confirm · Esc back · ↑↓ scroll "
                    )
                },
                &lines,
                scroll,
                None,
            )?;
            match read_ui_event()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    let key = normalize_key(key);
                    match key.code {
                        KeyCode::Enter => return Ok(true),
                        KeyCode::F(6) if permissions_shortcut => self.permissions_menu()?,
                        KeyCode::Esc | KeyCode::Char('q') => return Ok(false),
                        KeyCode::Up => scroll = scroll.saturating_sub(1),
                        KeyCode::Down => scroll = scroll.saturating_add(1),
                        KeyCode::PageUp => scroll = scroll.saturating_sub(10),
                        KeyCode::PageDown => scroll = scroll.saturating_add(10),
                        KeyCode::Home => scroll = 0,
                        KeyCode::End => scroll = lines.len().saturating_sub(panel.visible),
                        KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
                            return Ok(false);
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }

    /// The `/skills` browser: list with sources, Enter opens the instruction
    /// body, Esc goes back to the list.
    pub fn skills(&mut self, catalog: &super::skills::Catalog) -> Result<()> {
        if catalog.is_empty() {
            return self.overlay(
                tr!("Скиллы", "Skills"),
                vec![
                    Line::of(
                        tr!("Скиллов не найдено.", "No skills found."),
                        Style::fg(render::FG),
                    ),
                    Line::new(),
                    Line::of(
                        tr!(
                            "Проект: .karrycode/skills · пользователь: ~/.karrycode/skills",
                            "Project: .karrycode/skills · user: ~/.karrycode/skills"
                        ),
                        Style::dim(),
                    ),
                    Line::of(
                        tr!(
                            "Также читаются ~/.agents/skills, ~/.claude/skills и ~/.codex/skills.",
                            "Also reads ~/.agents/skills, ~/.claude/skills and ~/.codex/skills."
                        ),
                        Style::dim(),
                    ),
                ],
            );
        }
        let mut selected = 0usize;
        let mut scroll = 0usize;
        let mut preview: Option<usize> = None;
        let mut preview_scroll = 0usize;
        loop {
            if let Some(index) = preview {
                let Some(skill) = catalog.get(index) else {
                    preview = None;
                    continue;
                };
                let body = match super::skills::body(skill) {
                    Ok(body) => body,
                    Err(error) => error.to_string(),
                };
                let width = {
                    let (terminal_width, _) = terminal::size()?;
                    (usize::from(terminal_width).saturating_sub(8).clamp(36, 96)).saturating_sub(4)
                };
                let mut lines = Vec::new();
                for paragraph in body.lines() {
                    if paragraph.trim().is_empty() {
                        lines.push(Line::new());
                        continue;
                    }
                    let mut line = Line::new();
                    line.push(paragraph, Style::fg(render::FG));
                    lines.extend(render::wrap(&line, width));
                }
                let panel = self.draw_panel(
                    &trf!("Скилл: {}", "Skill: {}", skill.name),
                    tr!(
                        " ↑↓ · PgUp/PgDn — прокрутка · Esc — назад ",
                        " ↑↓ · PgUp/PgDn — scroll · Esc — back "
                    ),
                    &lines,
                    preview_scroll,
                    None,
                )?;
                match read_ui_event()? {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        let key = normalize_key(key);
                        match key.code {
                            KeyCode::Esc => preview = None,
                            KeyCode::Up => preview_scroll = preview_scroll.saturating_sub(1),
                            KeyCode::Down => preview_scroll = preview_scroll.saturating_add(1),
                            KeyCode::PageUp => preview_scroll = preview_scroll.saturating_sub(10),
                            KeyCode::PageDown => preview_scroll = preview_scroll.saturating_add(10),
                            KeyCode::Home => preview_scroll = 0,
                            KeyCode::End => {
                                preview_scroll = lines.len().saturating_sub(panel.visible)
                            }
                            KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
                                return Ok(());
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
                continue;
            }
            let mut lines = Vec::new();
            for (index, skill) in catalog.skills.iter().enumerate() {
                let is_selected = index == selected;
                let mut row = Line::new();
                row.push("  ", Style::plain());
                row.push(
                    if is_selected { "›" } else { " " },
                    if is_selected {
                        Style::brand().bold()
                    } else {
                        Style::dim()
                    },
                );
                row.push(" ", Style::plain());
                row.push(
                    skill.name.clone(),
                    if is_selected {
                        Style::brand().bold()
                    } else {
                        Style::fg(render::FG)
                    },
                );
                row.push("  ", Style::plain());
                row.push(skill.source.label(), Style::dim());
                lines.push(row);
                let description = if skill.description.is_empty() {
                    tr!("без описания", "no description").to_owned()
                } else {
                    skill.description.clone()
                };
                let mut detail = Line::new();
                detail.push("      ", Style::plain());
                detail.push(description, Style::dim());
                lines.push(detail);
                lines.push(Line::new());
            }
            let (_, terminal_height) = terminal::size()?;
            let visible = (lines.len() + 2)
                .min(usize::from(terminal_height).saturating_sub(2))
                .max(4)
                .saturating_sub(2);
            let row = selected * 3;
            if row < scroll {
                scroll = row;
            }
            if row + 1 >= scroll + visible {
                scroll = (row + 1).saturating_sub(visible.saturating_sub(1));
            }
            let panel = self.draw_panel(
                &trf!("Скиллы · {}", "Skills · {}", catalog.skills.len()),
                tr!(
                    " ↑↓ — выбор · Enter — инструкции · Esc — закрыть ",
                    " ↑↓ — select · Enter — instructions · Esc — close "
                ),
                &lines,
                scroll,
                None,
            )?;
            scroll = scroll.min(lines.len().saturating_sub(panel.visible));
            match read_ui_event()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    let key = normalize_key(key);
                    match key.code {
                        KeyCode::Up => selected = selected.saturating_sub(1),
                        KeyCode::Down => selected = (selected + 1).min(catalog.skills.len() - 1),
                        KeyCode::Home => selected = 0,
                        KeyCode::End => selected = catalog.skills.len() - 1,
                        KeyCode::PageUp => selected = selected.saturating_sub(10),
                        KeyCode::PageDown => {
                            selected = (selected + 10).min(catalog.skills.len() - 1)
                        }
                        KeyCode::Enter => {
                            preview = Some(selected);
                            preview_scroll = 0;
                        }
                        KeyCode::Esc | KeyCode::Char('q') => return Ok(()),
                        KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
                            return Ok(());
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }

    /// `/mcp`: server states, tool lists and import from other CLIs.
    pub fn mcp(
        &mut self,
        servers: &[super::mcp::ServerStatus],
        tools: &[super::mcp::ToolInfo],
        candidates: &[super::mcp::Candidate],
    ) -> Result<McpAction> {
        #[derive(Clone, Copy)]
        enum Mode {
            Servers,
            Tools(usize),
            Import,
        }
        let mut mode = Mode::Servers;
        let mut selected = 0usize;
        let mut scroll = 0usize;
        let mut checked: Vec<bool> = candidates.iter().map(|item| item.enabled).collect();
        let mut error: Option<String> = None;
        loop {
            let (title, footer, body) = match mode {
                Mode::Servers => {
                    let mut lines = Vec::new();
                    if servers.is_empty() {
                        lines.push(Line::of(
                            tr!("  MCP-серверов нет.", "  No MCP servers."),
                            Style::fg(render::FG),
                        ));
                        lines.push(Line::of(
                            tr!(
                                "  i — импортировать из Claude Code, Codex или OpenCode.",
                                "  i — import from Claude Code, Codex or OpenCode."
                            ),
                            Style::dim(),
                        ));
                    }
                    for (index, server) in servers.iter().enumerate() {
                        let is_selected = index == selected;
                        let mut row = Line::new();
                        row.push("  ", Style::plain());
                        row.push(
                            if is_selected { "›" } else { " " },
                            if is_selected {
                                Style::brand().bold()
                            } else {
                                Style::dim()
                            },
                        );
                        row.push(
                            if server.running { "●" } else { "○" },
                            if server.running {
                                Style::brand()
                            } else {
                                Style::dim()
                            },
                        );
                        row.push(" ", Style::plain());
                        row.push(
                            server.name.clone(),
                            if is_selected {
                                Style::brand().bold()
                            } else {
                                Style::fg(render::FG)
                            },
                        );
                        row.push("  ", Style::plain());
                        let detail = if server.running {
                            trf!("{} инстр.", "{} tools", server.tools)
                        } else if server.pending.is_some() {
                            tr!("требуется согласие", "approval required").to_owned()
                        } else if let Some(error) = &server.error {
                            trf!("ошибка: {}", "error: {}", error)
                        } else {
                            tr!("выключен", "disabled").to_owned()
                        };
                        row.push(
                            detail,
                            if server.error.is_some() {
                                Style::fg(render::WARN)
                            } else {
                                Style::dim()
                            },
                        );
                        row.push("  ", Style::plain());
                        row.push(server.source.clone(), Style::dim());
                        lines.push(row);
                    }
                    if let Some(error) = &error {
                        lines.push(Line::new());
                        lines.push(Line::of(format!("! {error}"), Style::fg(render::WARN)));
                    }
                    (
                        tr!("MCP-серверы", "MCP servers"),
                        tr!(
                            " Enter — открыть · r — перезапуск · i — импорт · Esc — закрыть ",
                            " Enter — open · r — restart · i — import · Esc — close "
                        ),
                        lines,
                    )
                }
                Mode::Tools(index) => {
                    let server = servers.get(index);
                    let mut lines = Vec::new();
                    if let Some(server) = server {
                        let matching: Vec<_> = tools
                            .iter()
                            .filter(|tool| server.running && tool.server == server.name)
                            .collect();
                        if matching.is_empty() {
                            lines.push(Line::of(
                                tr!(
                                    "  У сервера нет доступных инструментов.",
                                    "  The server exposes no tools."
                                ),
                                Style::dim(),
                            ));
                        }
                        for tool in matching {
                            let mut row = Line::new();
                            row.push("  ", Style::plain());
                            row.push(tool.name.clone(), Style::brand());
                            row.push("  ", Style::plain());
                            row.push(tool.description.clone(), Style::dim());
                            lines.push(row);
                            lines.push(Line::new());
                        }
                    }
                    (
                        tr!("Инструменты MCP", "MCP tools"),
                        tr!(
                            " ↑↓ · PgUp/PgDn — прокрутка · Esc — назад ",
                            " ↑↓ · PgUp/PgDn — scroll · Esc — back "
                        ),
                        lines,
                    )
                }
                Mode::Import => {
                    let mut lines = Vec::new();
                    if candidates.is_empty() {
                        lines.push(Line::of(
                            tr!(
                                "  Новых серверов в конфигах Claude Code, Codex и OpenCode нет.",
                                "  No new servers in the Claude Code, Codex or OpenCode configs."
                            ),
                            Style::dim(),
                        ));
                    }
                    for (index, candidate) in candidates.iter().enumerate() {
                        let is_selected = index == selected;
                        let mut row = Line::new();
                        row.push("  ", Style::plain());
                        row.push(
                            if is_selected { "›" } else { " " },
                            if is_selected {
                                Style::brand().bold()
                            } else {
                                Style::dim()
                            },
                        );
                        row.push(
                            if checked.get(index).copied().unwrap_or(false) {
                                "[x] "
                            } else {
                                "[ ] "
                            },
                            Style::brand(),
                        );
                        row.push(
                            candidate.name.clone(),
                            if is_selected {
                                Style::brand().bold()
                            } else {
                                Style::fg(render::FG)
                            },
                        );
                        row.push("  ", Style::plain());
                        row.push(candidate.source.clone(), Style::dim());
                        lines.push(row);
                        let mut command = Line::new();
                        command.push("        ", Style::plain());
                        command.push(candidate.command.clone(), Style::dim());
                        for argument in &candidate.args {
                            command.push(" ", Style::dim());
                            command.push(argument.clone(), Style::dim());
                        }
                        lines.push(command);
                        lines.push(Line::new());
                    }
                    if let Some(error) = &error {
                        lines.push(Line::of(format!("! {error}"), Style::fg(render::WARN)));
                    }
                    (
                        tr!("Импорт MCP", "MCP import"),
                        tr!(
                            " Space — отметить · a — все · Enter — импортировать · Esc — назад ",
                            " Space — toggle · a — all · Enter — import · Esc — back "
                        ),
                        lines,
                    )
                }
            };
            if matches!(mode, Mode::Servers) {
                selected = selected.min(servers.len().saturating_sub(1));
                let visible = usize::from(terminal::size()?.1).saturating_sub(4).max(2);
                scroll = scroll.clamp(selected.saturating_add(1).saturating_sub(visible), selected);
            }
            self.draw_panel(title, footer, &body, scroll, None)?;
            match read_ui_event()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    let key = normalize_key(key);
                    match mode {
                        Mode::Servers => match key.code {
                            KeyCode::Up => selected = selected.saturating_sub(1),
                            KeyCode::Down => {
                                selected = (selected + 1).min(servers.len().saturating_sub(1))
                            }
                            KeyCode::Home => selected = 0,
                            KeyCode::End => selected = servers.len().saturating_sub(1),
                            KeyCode::Enter => {
                                if let Some(server) = servers.get(selected) {
                                    if let Some(pending) = server.pending {
                                        return Ok(McpAction::Approve(pending));
                                    }
                                    mode = Mode::Tools(selected);
                                    scroll = 0;
                                }
                            }
                            KeyCode::Char('r') | KeyCode::Char('р') | KeyCode::Char('к') => {
                                return Ok(McpAction::Restart);
                            }
                            KeyCode::Char('i') | KeyCode::Char('и') | KeyCode::Char('ш') => {
                                error = None;
                                checked = candidates.iter().map(|item| item.enabled).collect();
                                selected = 0;
                                scroll = 0;
                                mode = Mode::Import;
                            }
                            KeyCode::Esc | KeyCode::Char('q') => return Ok(McpAction::Close),
                            KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
                                return Ok(McpAction::Close);
                            }
                            _ => {}
                        },
                        Mode::Tools(_) => match key.code {
                            KeyCode::Esc => mode = Mode::Servers,
                            KeyCode::Up => scroll = scroll.saturating_sub(1),
                            KeyCode::Down => scroll = scroll.saturating_add(1),
                            KeyCode::PageUp => scroll = scroll.saturating_sub(10),
                            KeyCode::PageDown => scroll = scroll.saturating_add(10),
                            KeyCode::Home => scroll = 0,
                            KeyCode::End => scroll = body.len(),
                            KeyCode::Char('q') => return Ok(McpAction::Close),
                            KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
                                return Ok(McpAction::Close);
                            }
                            _ => {}
                        },
                        Mode::Import => match key.code {
                            KeyCode::Esc => mode = Mode::Servers,
                            KeyCode::Up => selected = selected.saturating_sub(1),
                            KeyCode::Down => {
                                selected = (selected + 1).min(candidates.len().saturating_sub(1))
                            }
                            KeyCode::Char(' ') if !candidates.is_empty() => {
                                if let Some(value) = checked.get_mut(selected) {
                                    *value = !*value;
                                }
                            }
                            KeyCode::Char('a') | KeyCode::Char('а') | KeyCode::Char('ф') => {
                                let all = checked.iter().all(|value| *value);
                                checked.iter_mut().for_each(|value| *value = !all);
                            }
                            KeyCode::Enter => {
                                let chosen: Vec<usize> = checked
                                    .iter()
                                    .enumerate()
                                    .filter_map(|(index, value)| value.then_some(index))
                                    .collect();
                                if chosen.is_empty() {
                                    error = Some(
                                        tr!(
                                            "Отметь хотя бы один сервер",
                                            "Select at least one server"
                                        )
                                        .to_owned(),
                                    );
                                } else {
                                    return Ok(McpAction::Import(chosen));
                                }
                            }
                            KeyCode::Char('c') if key.modifiers == KeyModifiers::CONTROL => {
                                return Ok(McpAction::Close);
                            }
                            _ => {}
                        },
                    }
                }
                _ => {}
            }
        }
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let _ = self.leave();
    }
}

impl Screen {
    fn leave(&mut self) -> io::Result<()> {
        #[cfg(unix)]
        if self.enhanced_keys {
            let _ = execute!(io::stdout(), crossterm::event::PopKeyboardEnhancementFlags);
        }
        let display = execute!(
            io::stdout(),
            DisableMouseCapture,
            DisableBracketedPaste,
            SetAttribute(Attribute::Reset),
            ResetColor,
            LeaveAlternateScreen,
            Show
        );
        display.and(terminal::disable_raw_mode())
    }
}

fn enter() -> io::Result<()> {
    execute!(
        io::stdout(),
        EnterAlternateScreen,
        Hide,
        EnableBracketedPaste,
        EnableMouseCapture
    )
}

fn ignored_event(event: &Event) -> bool {
    matches!(
        event,
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            ..
        }) | Event::Key(KeyEvent {
            kind: KeyEventKind::Release,
            ..
        })
    )
}

fn read_ui_event() -> io::Result<Event> {
    loop {
        let event = event::read()?;
        if !ignored_event(&event) {
            return Ok(event);
        }
    }
}

fn put_line(out: &mut impl Write, x: usize, y: usize, line: &Line, width: usize) -> io::Result<()> {
    if width == 0 {
        return Ok(());
    }
    queue!(
        out,
        MoveTo(x as u16, y as u16),
        SetAttribute(Attribute::Reset),
        ResetColor
    )?;
    let mut current = Style::plain();
    let mut remaining = width;
    for span in line.spans() {
        if remaining == 0 {
            break;
        }
        let text = clip(&span.text, remaining);
        if text.is_empty() {
            continue;
        }
        remaining = remaining.saturating_sub(text.as_str().width());
        let style = theme::resolve(span.style);
        if style != current {
            apply_style(out, style, &mut current)?;
        }
        queue!(out, Print(text))?;
    }
    queue!(out, SetAttribute(Attribute::Reset), ResetColor)
}

fn apply_style(out: &mut impl Write, style: Style, current: &mut Style) -> io::Result<()> {
    if style.reversed != current.reversed {
        queue!(
            out,
            SetAttribute(if style.reversed {
                Attribute::Reverse
            } else {
                Attribute::NoReverse
            })
        )?;
    }
    if style.bold != current.bold {
        queue!(
            out,
            SetAttribute(if style.bold {
                Attribute::Bold
            } else {
                Attribute::NormalIntensity
            })
        )?;
    }
    if style.fg != current.fg {
        queue!(out, SetForegroundColor(style.fg.unwrap_or(Color::Reset)))?;
    }
    if style.bg != current.bg {
        queue!(out, SetBackgroundColor(style.bg.unwrap_or(Color::Reset)))?;
    }
    *current = style;
    Ok(())
}

fn clip(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for character in text.chars() {
        let cell = character.width().unwrap_or(0);
        if used + cell > width {
            break;
        }
        out.push(character);
        used += cell;
    }
    out
}

fn receipt_line(receipt: Receipt, width: usize) -> Line {
    let mut line = Line::new();
    line.push("  ", Style::plain());
    line.push("╰─ ", Style::dim());
    line.push(format!("{:.1}s", receipt.duration), Style::dim());
    let approx = if receipt.estimated { "~" } else { "" };
    let extras = [
        Some(trf!(
            "↑{}{} ↓{}{} токенов",
            "↑{}{} ↓{}{} tokens",
            approx,
            render::human(receipt.input),
            approx,
            render::human(receipt.output)
        )),
        (receipt.speed > 0.0).then(|| format!("{approx}{:.0} tok/s", receipt.speed)),
        receipt
            .ttft
            .map(|seconds| trf!("первый токен {:.1}s", "first token {:.1}s", seconds)),
    ];
    for extra in extras.into_iter().flatten() {
        let piece = Line::of(format!(" · {extra}"), Style::dim());
        if line.width().saturating_add(piece.width()) > width {
            break;
        }
        line.extend(piece);
    }
    line.truncate(width)
}

/// Joins status segments with ` · ` and drops the rightmost ones that do not fit.
fn join_segments(segments: &[Line], width: usize) -> Line {
    let mut line = Line::new();
    for (index, segment) in segments.iter().enumerate() {
        if index == 0 {
            line = segment.truncate(width);
            continue;
        }
        let mut piece = Line::of(" · ", Style::dim());
        piece.extend(segment.clone());
        if line.width().saturating_add(piece.width()) > width {
            break;
        }
        line.extend(piece);
    }
    line
}

fn reasoning_lines(reasoning: &str, width: usize, open: bool, live: bool) -> Vec<Line> {
    let text = reasoning.trim_end();
    if text.is_empty() {
        return Vec::new();
    }
    let mut body = Vec::new();
    for raw in text.lines() {
        if raw.trim().is_empty() {
            body.push(Line::of("  │", Style::dim()));
            continue;
        }
        let mut line = Line::new();
        line.push(raw, Style::dim());
        for row in render::wrap(&line, width.saturating_sub(4)) {
            let mut prefixed = Line::of("  │ ", Style::dim());
            prefixed.extend(row);
            body.push(prefixed);
        }
    }
    let limit = if live {
        2
    } else if open {
        80
    } else {
        3
    };
    let start = if live {
        body.len().saturating_sub(limit)
    } else {
        0
    };
    let mut lines = Vec::new();
    let mut head = Line::of("  │ ", Style::dim());
    head.push(tr!("reasoning", "reasoning"), Style::brand());
    lines.push(head);
    for row in body.iter().skip(start).take(limit) {
        lines.push(row.clone());
    }
    if !live && body.len() > limit {
        lines.push(Line::of(
            format!("  │ … +{} · F3", body.len() - limit),
            Style::dim(),
        ));
    }
    lines.push(Line::new());
    lines
}

fn history_lines(
    history: &[Message],
    width: usize,
    details: bool,
    reasoning_details: bool,
) -> Vec<Line> {
    let mut lines = Vec::new();
    for message in history {
        if matches!(
            message.role.as_str(),
            "assistant" | "narration" | "interrupted"
        ) {
            if message.role == "interrupted" {
                lines.push(Line::of(tr!("  Остановлено", "  Stopped"), Style::dim()));
            }
            if let Some(reasoning) = message.reasoning.as_deref() {
                lines.extend(reasoning_lines(reasoning, width, reasoning_details, false));
            }
            if let Some(action) = super::parse_action(&message.content) {
                lines.push(action_line(&action));
            } else if !message.content.is_empty() {
                lines.extend(render::assistant(&message.content, width));
            }
        } else if message.role == "event" {
            for text in message.content.lines() {
                let line = Line::of(format!("  {text}"), Style::dim());
                if line.is_blank() {
                    lines.push(line);
                } else {
                    lines.extend(render::wrap(&line, width));
                }
            }
        } else if message.role == "permission" {
            for line in message.content.lines() {
                lines.extend(render::wrap(
                    &Line::of(line, Style::fg(render::WARN).bold()),
                    width,
                ));
            }
        } else if message.role == "error" {
            for text in message.content.lines() {
                lines.extend(render::wrap(
                    &Line::of(format!("  {text}"), Style::fg(render::WARN)),
                    width,
                ));
            }
        } else if message.content.starts_with("Tool result") {
            lines.extend(tool_lines(&message.content, width, details));
        } else {
            lines.extend(render::user(
                message.display.as_deref().unwrap_or(&message.content),
                width,
            ));
        }
        if let Some(receipt) = message.receipt {
            lines.push(receipt_line(receipt, width));
        }
        lines.push(Line::new());
    }
    while lines.last().is_some_and(Line::is_blank) {
        lines.pop();
    }
    lines
}

fn action_line(action: &serde_json::Value) -> Line {
    let name = action["action"].as_str().unwrap_or("action");
    let detail = ["path", "query", "task", "text"]
        .iter()
        .find_map(|key| action[*key].as_str())
        .unwrap_or("");
    let mut line = Line::new();
    line.push("  ", Style::plain());
    line.push("· ", Style::dim());
    line.push(name, Style::fg(render::BRAND));
    if !detail.is_empty() {
        line.push(" ", Style::plain());
        line.push(
            detail
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(96)
                .collect::<String>(),
            Style::dim(),
        );
    }
    line
}

fn tool_lines(content: &str, width: usize, details: bool) -> Vec<Line> {
    if !details {
        return vec![Line::of("  · tool result · F2", Style::dim())];
    }
    let mut lines = vec![Line::of("  · tool result", Style::dim())];
    let body = content
        .strip_prefix("Tool result")
        .and_then(|rest| rest.split_once('\n').map(|(_, body)| body))
        .unwrap_or(content)
        .trim_start();
    for raw in body.lines().take(60) {
        let wrapped = render::wrap(&Line::of(raw, Style::dim()), width.saturating_sub(4));
        if wrapped.is_empty() {
            lines.push(Line::new());
            continue;
        }
        for row in wrapped {
            let mut prefixed = Line::of("    ", Style::plain());
            prefixed.extend(row);
            lines.push(prefixed);
        }
    }
    if body.lines().nth(60).is_some() {
        lines.push(Line::of("    …", Style::dim()));
    }
    lines
}

fn help_lines() -> Vec<Line> {
    let mut lines = Vec::new();
    let heading = |text: &str| Line::of(text, Style::brand().bold());
    let row = |name: &str, description: &str| {
        let mut line = Line::new();
        line.push(format!("  {name:<12}"), Style::fg(render::FG));
        line.push(description.to_owned(), Style::dim());
        line
    };
    lines.push(heading(tr!(
        "Подключение и работа",
        "Connection and workflow"
    )));
    for (name, description) in commands() {
        lines.push(row(name, description));
    }
    lines.push(Line::new());
    lines.push(heading(tr!("Файлы и клавиши", "Files and keys")));
    lines.push(row(
        "@путь",
        tr!("приложить текстовый файл", "attach a text file"),
    ));
    lines.push(row("Enter", tr!("отправить", "send")));
    lines.push(row(
        "Shift/Ctrl+Enter",
        tr!(
            "новая строка (запасной вариант: Ctrl+J)",
            "new line (fallback: Ctrl+J)"
        ),
    ));
    lines.push(row("Tab", tr!("дополнить команду", "complete a command")));
    lines.push(row(
        "↑ / ↓",
        tr!(
            "история ввода или выбор в меню",
            "input history or menu selection"
        ),
    ));
    lines.push(row(
        "Esc",
        tr!(
            "дважды за 7 с — прервать ответ; один раз — закрыть меню или очистить ввод",
            "twice within 7 s to stop a reply; once to close menus or clear input"
        ),
    ));
    lines.push(row(
        "F2",
        tr!(
            "показать или скрыть результаты инструментов",
            "show or hide tool results"
        ),
    ));
    lines.push(row(
        "F3",
        tr!(
            "раскрыть или скрыть reasoning",
            "expand or collapse reasoning"
        ),
    ));
    lines.push(row(
        "F6",
        tr!(
            "сменить разрешения во время работы",
            "change permissions during a request"
        ),
    ));
    lines.push(row(
        "PgUp/PgDn",
        tr!("прокрутить историю", "scroll history"),
    ));
    lines.push(row(
        "Shift+↑/↓",
        tr!("прокрутить историю", "scroll history"),
    ));
    lines.push(row(
        tr!("колесо", "wheel"),
        tr!("прокрутить историю", "scroll history"),
    ));
    lines.push(row(
        "Ctrl-C",
        tr!(
            "копировать выделенное мышью; иначе очистить ввод",
            "copy mouse selection; otherwise clear the input"
        ),
    ));
    lines.push(row(
        "Ctrl-D",
        tr!("выйти с пустой строкой", "exit from an empty line"),
    ));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_menu_filters_commands_by_prefix() {
        assert_eq!(menu_items("/", false, false).len(), commands().len());
        let filtered = menu_items("/st", false, false);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].0, "/status");
        assert!(
            menu_items("/co", false, false)
                .iter()
                .any(|(name, _)| *name == "/continue")
        );
        assert!(menu_items("/", true, false).is_empty());
        assert!(menu_items("/", false, true).is_empty());
        assert!(menu_items("/help me", false, false).is_empty());
        assert!(menu_items("hello", false, false).is_empty());
        assert_eq!(menu_items("draft /per", false, false)[0].0, "/permissions");
    }

    fn test_screen() -> Screen {
        Screen {
            editor: Editor::new(),
            pastes: Vec::new(),
            submitted_pastes: Vec::new(),
            notice: String::new(),
            scroll: 0,
            details: false,
            queued: String::new(),
            submit_queued: false,
            submitted: Vec::new(),
            nav: None,
            selected: 0,
            menu_closed: false,
            armed: false,
            diagnose: false,
            planning: false,
            header_hidden: false,
            escape: EscapeGate::default(),
            #[cfg(unix)]
            enhanced_keys: false,
            theme: theme::Theme::Sage,
            tick: 0,
            turn: None,
            stats: Stats::default(),
            cache: None,
            context_limit: CTX_TOKENS,
            input_limit: CTX_TOKENS.saturating_sub(4096),
            output_limit: 4096,
            input_estimate: None,
            reasoning: crate::native::Reasoning::Off,
            reasoning_details: false,
            background: Vec::new(),
            desktop_active: false,
            panel_frame: None,
            viewport: None,
            selection: None,
            permission_mode: crate::native::permissions::Mode::Ask,
            dialog_mode: crate::native::permissions::DialogMode::Agent,
            approval_card: false,
        }
    }

    #[test]
    fn switching_dialog_modes_during_a_turn_updates_only_the_next_turn() {
        let mut screen = test_screen();
        screen.begin_turn(Instant::now());
        screen.set_dialog_mode(DialogMode::Chat);
        assert_eq!(screen.turn.unwrap().mode, DialogMode::Agent);
        let _ = screen.finish_turn();
        screen.begin_turn(Instant::now());
        assert_eq!(screen.turn.unwrap().mode, DialogMode::Chat);
        screen.reset_conversation();
        assert_eq!(screen.dialog_mode(), DialogMode::Agent);
    }

    #[test]
    fn starting_a_turn_clears_the_previous_status_notice() {
        let mut screen = test_screen();
        screen.notice("Enter /continue to resume.");
        screen.begin_turn(Instant::now());
        assert!(screen.notice.is_empty());
    }

    #[test]
    fn tool_turns_count_every_streamed_reply() {
        let mut screen = test_screen();
        screen.begin_turn(Instant::now() - Duration::from_secs(2));
        screen.track("first", "");
        screen.record_reply(100, 10, false, 0.1);
        screen.track("second", "");
        screen.record_reply(200, 20, true, 0.1);
        let _ = screen.finish_turn();
        let receipt = screen.stats.last.expect("receipt");
        assert_eq!(receipt.input, 300);
        assert_eq!(receipt.output, 30);
        assert!(receipt.duration >= 2.0 && receipt.estimated);
        assert_eq!(screen.stats.turns, 1);
        screen.stage_input("old draft".to_owned());
        screen.set_input_estimate(Some(500));
        screen.reset_conversation();
        screen.paste_prompt(&"draft line\n".repeat(60));
        assert!(screen.editor.is_empty() && screen.turn.is_none());
        assert_eq!(screen.pastes.len(), 1);
        assert_eq!(screen.submit(""), "");
        assert_eq!(screen.take_pastes()[0].lines().count(), 60);
        assert!(screen.pastes.is_empty() && screen.turn.is_none());
        assert!(screen.permission_mode() == crate::native::permissions::Mode::Auto);
        assert!(screen.stats.last.is_none());
        assert_eq!(screen.stats.turns, 0);
        assert_eq!(screen.input_estimate, None);
        assert!(!screen.submit_queued);
        assert!(screen.queued.is_empty());
    }

    #[test]
    fn tool_details_follow_the_f2_toggle() {
        let message = Message {
            role: "user".into(),
            content: "Tool result (untrusted file content)\nfn agent() {}".into(),
            reasoning: None,
            ..Default::default()
        };
        let compact = history_lines(std::slice::from_ref(&message), 60, false, false);
        let full = history_lines(std::slice::from_ref(&message), 60, true, false);
        assert!(compact.iter().any(|line| line.plain_text().contains("F2")));
        assert!(
            full.iter()
                .any(|line| line.plain_text().contains("fn agent"))
        );
    }

    #[test]
    fn status_segments_drop_from_the_right() {
        let segments = vec![
            Line::of("● model", Style::plain()),
            Line::of("ctx 1/2", Style::plain()),
            Line::of("session 1s", Style::plain()),
        ];
        let line = join_segments(&segments, 14);
        assert!(line.width() <= 14);
        assert!(!line.plain_text().contains("session"));
        let full = join_segments(&segments, 80);
        assert!(full.plain_text().contains("session"));
    }

    #[test]
    fn the_editor_keeps_the_caret_visible_in_a_narrow_box() {
        let mut editor = Editor::from("abcdefghij");
        let (shown, caret) = editor.view(5);
        assert_eq!(shown, "ghij");
        assert_eq!(caret, 4);
        editor.left();
        editor.left();
        let (shown, caret) = editor.view(5);
        assert!(shown.len() <= 5);
        assert!(caret < 5);
        assert_eq!(editor.text(), "abcdefghij");
    }

    #[test]
    fn editing_keys_move_and_delete_around_the_caret() {
        let mut editor = Editor::from("one two");
        editor.kill_word();
        assert_eq!(editor.text(), "one ");
        editor.kill_to_start();
        assert_eq!(editor.text(), "");
        editor.insert("hello world".chars());
        editor.home();
        editor.delete();
        assert_eq!(editor.text(), "ello world");
        for _ in 0..4 {
            editor.right();
        }
        editor.kill_to_end();
        assert_eq!(editor.text(), "ello");
        editor.insert("!".chars());
        assert_eq!(editor.text(), "ello!");
    }

    #[test]
    fn windows_control_characters_are_normalized() {
        let key = normalize_key(KeyEvent::new(
            KeyCode::Char('\u{15}'),
            KeyModifiers::CONTROL,
        ));
        assert_eq!(key.code, KeyCode::Char('u'));
        assert!(key.modifiers.contains(KeyModifiers::CONTROL));
        let plain = normalize_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE));
        assert_eq!(plain.code, KeyCode::Char('u'));
        assert!(plain.modifiers.is_empty());
        let cyrillic = normalize_key(KeyEvent::new(KeyCode::Char('с'), KeyModifiers::CONTROL));
        assert_eq!(cyrillic.code, KeyCode::Char('c'));
        let altgr = normalize_key(KeyEvent::new(
            KeyCode::Char('с'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ));
        assert_eq!(altgr.code, KeyCode::Char('с'));
    }

    #[test]
    fn enter_modifiers_insert_lines_and_keep_the_caret_visible() {
        for mods in [KeyModifiers::SHIFT, KeyModifiers::CONTROL] {
            assert!(newline_key(KeyEvent::new(KeyCode::Enter, mods)));
        }
        assert!(!newline_key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE
        )));
        assert!(newline_key(KeyEvent::new(
            KeyCode::Char('j'),
            KeyModifiers::CONTROL
        )));
        let mut editor = Editor::from("/theme界");
        editor.left();
        editor.insert(['\n']);
        assert_eq!(editor.text(), "/theme\n界");
        assert!(!super::super::slash_command(&editor.text()));
        assert_eq!(
            editor.composer(8, 2),
            (vec!["/theme".into(), "界".into()], 1, 0)
        );
        assert_eq!(editor.composer(8, 1).1, 0);
    }

    #[test]
    fn escape_requires_two_presses_before_the_seven_second_deadline() {
        let t = Instant::now();
        let mut gate = EscapeGate::default();
        assert!(!gate.press(t, true));
        assert!(!gate.press(t + Duration::from_secs(1), true));
        gate.held = false;
        assert!(gate.press(t + Duration::from_secs(6), true));
        let mut gate = EscapeGate::default();
        assert!(!gate.press(t, false));
        assert!(!gate.active(t + Duration::from_secs(7)));
        assert!(!gate.press(t + Duration::from_secs(7), false));
        assert!(gate.press(t + Duration::from_secs(8), false));
    }

    #[test]
    fn pasted_newlines_stay_in_the_draft() {
        assert_eq!(paste_text("a\r\nb\tc\x07"), "a\nb c");
    }

    #[test]
    fn footer_reserves_space_for_context_in_idle_and_streaming_views() {
        let mut screen = test_screen();
        screen.set_limits(128_000, 100_000, 4_000, super::super::Reasoning::Off);
        screen.set_input_estimate(Some(72_000));
        screen.begin_turn(Instant::now());
        let model = "provider / a-model-with-a-very-long-name-that-can-fill-the-whole-footer";
        for streaming in [false, true] {
            for width in [40, 60, 80, 120] {
                let footer = screen.footer(model, width, streaming);
                let text = footer.plain_text();
                assert_eq!(footer.width(), width);
                assert!(text.ends_with("  ctx 72% "));
                assert!(!text.contains(['▰', '▱']));
            }
        }
        screen.stage_input("draft".repeat(500));
        assert_eq!(screen.input_percentage(), Some(72));
        screen.set_limits(128_000, 72_000, 4_000, super::super::Reasoning::Off);
        assert!(
            screen
                .footer(model, 80, false)
                .plain_text()
                .ends_with("100% ")
        );
        screen.set_input_estimate(Some(72_001));
        assert!(
            screen
                .footer(model, 80, false)
                .plain_text()
                .ends_with("100%+ ")
        );
        screen.reset_conversation();
        assert!(screen.footer(model, 80, false).plain_text().ends_with("— "));
    }
    #[test]
    fn reasoning_history_collapses_and_f3_expands() {
        let message = Message {
            role: "assistant".into(),
            content: "answer".into(),
            reasoning: Some("one\ntwo\nthree\nfour".into()),
            ..Default::default()
        };
        let compact = history_lines(std::slice::from_ref(&message), 60, false, false);
        let full = history_lines(std::slice::from_ref(&message), 60, false, true);
        let compact_text: Vec<String> = compact.iter().map(|line| line.plain_text()).collect();
        let full_text: Vec<String> = full.iter().map(|line| line.plain_text()).collect();
        assert!(compact_text.iter().any(|line| line.contains("reasoning")));
        assert!(compact_text.iter().any(|line| line.contains("+1")));
        assert!(compact_text.iter().any(|line| line.contains("F3")));
        assert!(!compact_text.iter().any(|line| line.contains("four")));
        assert!(full_text.iter().any(|line| line.contains("four")));
        assert!(!full_text.iter().any(|line| line.contains("+1")));
    }
}
