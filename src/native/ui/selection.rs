use super::{Line, Style};
use anyhow::{Context, Result, ensure};
use crossterm::event::MouseEvent;
use std::sync::Arc;
use unicode_width::UnicodeWidthChar;

pub(super) struct Viewport {
    pub top: usize,
    pub left: usize,
    pub width: usize,
    pub start: usize,
    pub rows: Vec<Line>,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Point {
    row: usize,
    column: usize,
}

pub(super) struct Selection {
    anchor: Point,
    end: Point,
    width: usize,
    dragging: bool,
    pressed_url: Option<Arc<url::Url>>,
}

impl Viewport {
    fn url_at(&self, mouse: MouseEvent) -> Option<&Arc<url::Url>> {
        let row = usize::from(mouse.row).checked_sub(self.top)?;
        let column = usize::from(mouse.column).checked_sub(self.left)?;
        (column < self.width)
            .then(|| self.rows.get(row)?.url_at(column))
            .flatten()
    }

    fn point(&self, mouse: MouseEvent) -> Point {
        let row = usize::from(mouse.row)
            .saturating_sub(self.top)
            .min(self.rows.len().saturating_sub(1));
        Point {
            row: self.start + row,
            column: usize::from(mouse.column)
                .saturating_sub(self.left)
                .min(self.width),
        }
    }

    pub fn begin(&self, mouse: MouseEvent) -> Option<Selection> {
        let y = usize::from(mouse.row);
        let x = usize::from(mouse.column);
        if !(self.top..self.top + self.rows.len()).contains(&y)
            || !(self.left..self.left + self.width).contains(&x)
        {
            return None;
        }
        let point = self.point(mouse);
        Some(Selection {
            anchor: point,
            end: point,
            width: self.width,
            dragging: true,
            pressed_url: self.url_at(mouse).cloned(),
        })
    }
}

impl Selection {
    pub fn release_link(&self, view: &Viewport, mouse: MouseEvent) -> Option<Arc<url::Url>> {
        if self.dragging
            && view.point(mouse) == self.anchor
            && let Some(url) = &self.pressed_url
            && view.url_at(mouse) == Some(url)
        {
            return Some(Arc::clone(url));
        }
        None
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn extend(&mut self, viewport: &Viewport, mouse: MouseEvent) {
        if self.dragging {
            self.end = viewport.point(mouse);
            if self.end != self.anchor {
                self.pressed_url = None;
            }
            self.dragging = matches!(mouse.kind, crossterm::event::MouseEventKind::Drag(_));
        }
    }

    fn columns(&self, row: usize) -> Option<std::ops::Range<usize>> {
        let first = self.anchor.min(self.end);
        let last = self.anchor.max(self.end);
        if first == last || !(first.row..=last.row).contains(&row) {
            return None;
        }
        let start = if row == first.row { first.column } else { 0 };
        let end = if row == last.row {
            last.column
        } else {
            self.width
        };
        Some(start..end)
    }

    pub fn highlight(&self, row: usize, line: &Line) -> Option<Line> {
        let columns = self.columns(row)?;
        let mut result = Line::new();
        for (text, style, selected) in split_line(line, &columns) {
            let style = if selected { style.reversed() } else { style };
            result.push(text, style);
        }
        Some(result)
    }

    pub fn text<'a>(&self, rows: impl Iterator<Item = (usize, &'a Line)>) -> Option<String> {
        let mut blocks = Vec::new();
        let mut rendered = String::new();
        let mut original = None;
        let mut complete = true;
        let mut selected = false;
        for (index, line) in rows {
            if let Some(raw) = line.original() {
                original = Some(raw);
            }
            if let Some(mut columns) = self.columns(index) {
                selected = true;
                complete &= columns.start <= line.copy_start() && columns.end >= line.copy_end();
                columns.start = columns.start.max(line.copy_start());
                columns.end = columns.end.min(line.copy_end());
                if columns.start < columns.end {
                    rendered.extend(
                        split_line(line, &columns)
                            .filter(|(_, _, selected)| *selected)
                            .map(|(text, _, _)| text),
                    );
                }
            } else {
                complete = false;
            }
            let separator = line.copy_separator();
            if separator == "\n" {
                if selected {
                    blocks.push(if complete && let Some(raw) = original {
                        raw.to_owned()
                    } else {
                        std::mem::take(&mut rendered)
                    });
                }
                rendered.clear();
                original = None;
                complete = true;
                selected = false;
            } else if selected {
                rendered.push_str(separator);
            }
        }
        if selected {
            blocks.push(rendered);
        }
        let text = blocks.join("\n");
        (!text.trim().is_empty()).then_some(text)
    }
}

#[cfg(windows)]
pub(super) fn open_url(url: &url::Url) -> Result<()> {
    use windows_sys::Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL};
    let target: Vec<u16> = url.as_str().encode_utf16().chain([0]).collect();
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            windows_sys::w!("open"),
            target.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    } as isize;
    ensure!(result > 32, "Windows could not open the link ({result})");
    Ok(())
}

#[cfg(unix)]
pub(super) fn open_url(url: &url::Url) -> Result<()> {
    use std::process::{Command, Stdio};
    let mut child = Command::new("xdg-open")
        .arg(url.as_str())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context(tr!(
            "Не удалось запустить xdg-open.",
            "Could not start xdg-open."
        ))?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

fn split_line<'a>(
    line: &'a Line,
    columns: &'a std::ops::Range<usize>,
) -> impl Iterator<Item = (&'a str, Style, bool)> {
    let mut column = 0usize;
    let mut selected = false;
    line.spans()
        .iter()
        .flat_map(|span| {
            span.text.char_indices().map(move |(index, character)| {
                let end = index + character.len_utf8();
                (
                    &span.text[index..end],
                    span.style,
                    character.width().unwrap_or(0),
                )
            })
        })
        .map(move |(text, style, cells)| {
            if cells != 0 {
                selected = column < columns.end && column + cells > columns.start;
                column += cells;
            }
            (text, style, selected)
        })
}

#[cfg(windows)]
pub(super) fn copy(text: &str) -> Result<()> {
    use windows_sys::Win32::{
        Foundation::GlobalFree,
        System::{
            Console::GetConsoleWindow,
            DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData},
            Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock},
            Ole::CF_UNICODETEXT,
        },
        UI::WindowsAndMessaging::GetForegroundWindow,
    };
    let data: Vec<_> = text
        .replace('\n', "\r\n")
        .encode_utf16()
        .chain([0])
        .collect();
    let bytes = std::mem::size_of_val(data.as_slice());
    unsafe {
        let mut owner = GetConsoleWindow();
        if owner.is_null() {
            owner = GetForegroundWindow();
        }
        ensure!(
            OpenClipboard(owner) != 0,
            "{}",
            tr!("Буфер обмена занят.", "The clipboard is busy.")
        );
        let result = (|| {
            let memory = GlobalAlloc(GMEM_MOVEABLE, bytes);
            ensure!(!memory.is_null(), "Clipboard allocation failed");
            let destination = GlobalLock(memory).cast::<u16>();
            if destination.is_null() {
                GlobalFree(memory);
                anyhow::bail!(tr!(
                    "Память буфера обмена недоступна",
                    "Clipboard memory is unavailable"
                ));
            }
            destination.copy_from_nonoverlapping(data.as_ptr(), data.len());
            GlobalUnlock(memory);
            if EmptyClipboard() == 0
                || SetClipboardData(u32::from(CF_UNICODETEXT), memory).is_null()
            {
                GlobalFree(memory);
                anyhow::bail!(tr!(
                    "Не удалось записать в буфер обмена",
                    "Clipboard write failed"
                ));
            }
            Ok(())
        })();
        CloseClipboard();
        result.context(tr!(
            "Не удалось скопировать текст.",
            "Could not copy the selection."
        ))
    }
}

#[cfg(not(windows))]
pub(super) fn copy(text: &str) -> Result<()> {
    use base64::Engine as _;
    use std::io::{self, Write};
    use std::process::{Command, Stdio};

    let commands: &[(&str, &[&str])] = &[
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
    ];
    for (program, args) in commands {
        if let Ok(mut child) = Command::new(program)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            let sent = child
                .stdin
                .take()
                .is_some_and(|mut input| input.write_all(text.as_bytes()).is_ok());
            if child.wait().is_ok_and(|status| status.success()) && sent {
                return Ok(());
            }
        }
    }
    ensure!(
        text.len() <= 64 * 1024,
        "Selection is too large for the terminal clipboard"
    );
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    let mut stdout = io::stdout().lock();
    write!(stdout, "\x1b]52;c;{encoded}\x07").context(tr!(
        "Терминал не записал данные в буфер обмена",
        "Terminal clipboard write failed"
    ))?;
    Ok(stdout.flush()?)
}

#[cfg(windows)]
pub(super) fn paste() -> Result<Option<String>> {
    use windows_sys::Win32::System::{
        DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard},
        Memory::{GlobalLock, GlobalSize, GlobalUnlock},
        Ole::CF_UNICODETEXT,
    };
    unsafe {
        ensure!(
            OpenClipboard(std::ptr::null_mut()) != 0,
            tr!("Буфер обмена занят.", "The clipboard is busy.")
        );
        let result = (|| {
            let memory = GetClipboardData(u32::from(CF_UNICODETEXT));
            if memory.is_null() {
                return Ok(None);
            }
            let length = GlobalSize(memory) / size_of::<u16>();
            let source = GlobalLock(memory).cast::<u16>();
            ensure!(
                !source.is_null(),
                tr!(
                    "Память буфера обмена недоступна",
                    "Clipboard memory is unavailable"
                )
            );
            let units = std::slice::from_raw_parts(source, length.min(super::PASTE_LIMIT + 1));
            let text = units
                .iter()
                .position(|unit| *unit == 0)
                .map(|end| String::from_utf16_lossy(&units[..end]));
            GlobalUnlock(memory);
            text.map(Some).with_context(|| {
                tr!(
                    "Текст в буфере обмена слишком длинный.",
                    "Clipboard text is too long."
                )
            })
        })();
        CloseClipboard();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_selection_keeps_markdown_and_code_across_wrapped_rows() {
        let source =
            "**A long formatted paragraph**\n\n```text\n    indentation survives wrapping\n```";
        let lines = super::super::render::assistant(source, 14);
        let selection = Selection {
            anchor: Point { row: 0, column: 0 },
            end: Point {
                row: lines.len() - 1,
                column: 14,
            },
            width: 14,
            dragging: false,
            pressed_url: None,
        };
        assert_eq!(
            selection.text(lines.iter().enumerate()).as_deref(),
            Some(source)
        );
    }

    #[test]
    fn copies_reverse_selection_across_rows_and_wide_characters() {
        let view = Viewport {
            top: 0,
            left: 0,
            width: 12,
            start: 5,
            rows: vec![
                Line::of("a界e\u{301}", Style::plain()),
                Line::of("тест", Style::plain()),
            ],
        };
        let selection = Selection {
            anchor: Point { row: 6, column: 2 },
            end: Point { row: 5, column: 2 },
            width: 12,
            dragging: false,
            pressed_url: None,
        };
        assert_eq!(
            selection
                .text(
                    view.rows
                        .iter()
                        .enumerate()
                        .map(|(i, line)| (view.start + i, line))
                )
                .as_deref(),
            Some("界e\u{301}\nте")
        );
        assert!(selection.highlight(5, &view.rows[0]).is_some());
    }

    #[test]
    fn click_opens_links_but_drag_still_selects_text() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
        let mouse = |kind, column| MouseEvent {
            kind,
            row: 4,
            column,
            modifiers: KeyModifiers::NONE,
        };
        let down = MouseEventKind::Down(MouseButton::Left);
        let up = MouseEventKind::Up(MouseButton::Left);
        let drag = MouseEventKind::Drag(MouseButton::Left);
        let view = Viewport {
            top: 4,
            left: 2,
            width: 55,
            start: 12,
            rows: super::super::render::assistant(
                "Превью: [открыть](http://127.0.0.1:59114/).",
                55,
            ),
        };
        let click = mouse(up, 15);
        let mut selection = view.begin(mouse(down, 15)).unwrap();
        assert_eq!(
            selection.release_link(&view, click).unwrap().as_str(),
            "http://127.0.0.1:59114/"
        );
        selection.extend(&view, mouse(drag, 20));
        assert!(selection.release_link(&view, click).is_none());
        assert!(
            selection
                .text(view.rows.iter().enumerate().map(|(i, line)| (12 + i, line)))
                .is_some()
        );
    }
}
