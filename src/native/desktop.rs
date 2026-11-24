pub(super) const STOP_COMMAND: &str = "/__desktop_stop";

#[cfg(windows)]
mod windows {
    use anyhow::{Context, Result, bail, ensure};
    use serde_json::Value;
    use std::{
        io::Read,
        mem::size_of,
        path::PathBuf,
        process::{Command, Stdio},
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicU64, Ordering},
            mpsc,
        },
        thread,
        time::{Duration, Instant},
    };
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HWND, LPARAM, POINT, RECT},
        Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation},
        System::{
            Console::GetConsoleWindow,
            StationsAndDesktops::{
                CloseDesktop, DESKTOP_READOBJECTS, GetUserObjectInformationW, OpenInputDesktop,
                UOI_NAME,
            },
            SystemServices::{SS_CENTER, SS_CENTERIMAGE},
            Threading::{
                OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
                QueryFullProcessImageNameW,
            },
        },
        UI::{
            HiDpi::{
                DPI_AWARENESS_CONTEXT, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
                SetThreadDpiAwarenessContext,
            },
            Input::KeyboardAndMouse::*,
            WindowsAndMessaging::*,
        },
    };

    #[derive(Default)]
    pub struct Access {
        approved: bool,
        denied: bool,
        stopped: bool,
        indicator: Option<Indicator>,
    }

    impl Access {
        fn needs_consent(&self, mode: super::super::permissions::Mode) -> bool {
            !self.approved && mode != super::super::permissions::Mode::Full
        }

        pub fn paused(&self) -> bool {
            self.stopped
                || self.denied
                || self
                    .indicator
                    .as_ref()
                    .is_some_and(|indicator| indicator.stop.load(Ordering::Acquire))
        }

        pub fn end_turn(&mut self) {
            if let Some(indicator) = self.indicator.take() {
                self.stopped |= indicator.stop.load(Ordering::Acquire);
            }
        }

        pub fn stop(&mut self) {
            self.stopped = true;
            self.approved = false;
            self.end_turn();
        }

        pub fn reset_chat(&mut self) {
            self.end_turn();
            self.approved = false;
            self.denied = false;
        }

        pub fn resume(&mut self) {
            *self = Self::default();
        }

        pub fn authorize(
            &mut self,
            screen: &mut super::super::ui::Screen,
            mode: super::super::permissions::Mode,
            enabled: bool,
        ) -> Result<Arc<AtomicBool>> {
            ensure!(
                enabled,
                tr!(
                    "Управление выключено в /setting.",
                    "Desktop control is off in /setting."
                )
            );
            ensure!(
                !self.paused(),
                tr!(
                    "Управление остановлено. Возобновить его можно в /setting.",
                    "Desktop control is paused. Resume it in /setting."
                )
            );
            unlocked_desktop()?;
            if self.needs_consent(mode) {
                self.approved = screen.review(
                    tr!("Разрешить доступ к рабочему столу?", "Allow desktop control?"),
                    tr!(
                        "На текущий чат. Модель может получить названия окон, элементы интерфейса и снимки, затем выполнить ввод. Остановить можно клавишами Ctrl+Alt+Shift+F12.",
                        "For this conversation. The model can receive window titles, controls and screenshots, then send input. Press Ctrl+Alt+Shift+F12 to stop."
                    ),
                )?;
                self.denied = !self.approved;
                ensure!(
                    self.approved,
                    tr!(
                        "Пользователь отклонил доступ к рабочему столу.",
                        "The user declined desktop access."
                    )
                );
            }
            if self.indicator.is_none() {
                self.indicator = Some(Indicator::start()?);
                screen.desktop_active(true);
            }
            Ok(Arc::clone(&self.indicator.as_ref().unwrap().stop))
        }
    }

    struct Indicator {
        stop: Arc<AtomicBool>,
        worker: Option<thread::JoinHandle<()>>,
    }

    impl Indicator {
        fn start() -> Result<Self> {
            let stop = Arc::new(AtomicBool::new(false));
            let stop_requested = Arc::clone(&stop);
            let (send, receive) = mpsc::sync_channel(1);
            let worker = thread::spawn(move || {
                let class: Vec<_> = "STATIC".encode_utf16().chain([0]).collect();
                let title: Vec<_> = tr!(
                    "KarryCode управляет ПК   •   Ctrl+Alt+Shift+F12 — стоп",
                    "KarryCode desktop control   •   Ctrl+Alt+Shift+F12 to stop"
                )
                .encode_utf16()
                .chain([0])
                .collect();
                let hwnd = unsafe {
                    CreateWindowExW(
                        WS_EX_TOPMOST
                            | WS_EX_TOOLWINDOW
                            | WS_EX_NOACTIVATE
                            | WS_EX_TRANSPARENT
                            | WS_EX_LAYERED,
                        class.as_ptr(),
                        title.as_ptr(),
                        WS_POPUP | WS_BORDER | SS_CENTER | SS_CENTERIMAGE,
                        12,
                        12,
                        490,
                        30,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null(),
                    )
                };
                if hwnd.is_null() {
                    let _ = send.send(false);
                    return;
                }
                let registered = unsafe {
                    SetLayeredWindowAttributes(hwnd, 0, 240, LWA_ALPHA) != 0
                        && RegisterHotKey(
                            hwnd,
                            1,
                            MOD_CONTROL | MOD_ALT | MOD_SHIFT | MOD_NOREPEAT,
                            u32::from(VK_F12),
                        ) != 0
                };
                if registered {
                    unsafe {
                        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                    }
                    let _ = send.send(true);
                    let mut message = MSG::default();
                    while !stop_requested.load(Ordering::Acquire) {
                        while unsafe {
                            PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE)
                        } != 0
                        {
                            if matches!(message.message, WM_HOTKEY | WM_QUIT) {
                                stop_requested.store(true, Ordering::Release);
                                break;
                            }
                            unsafe {
                                TranslateMessage(&message);
                                DispatchMessageW(&message);
                            }
                        }
                        thread::sleep(Duration::from_millis(15));
                    }
                    unsafe {
                        UnregisterHotKey(hwnd, 1);
                    }
                } else {
                    let _ = send.send(false);
                }
                unsafe {
                    DestroyWindow(hwnd);
                }
            });
            let indicator = Self {
                stop,
                worker: Some(worker),
            };
            ensure!(
                receive.recv_timeout(Duration::from_secs(2)) == Ok(true),
                tr!(
                    "Не удалось показать индикатор или зарегистрировать клавишу остановки.",
                    "Could not show the indicator or register the emergency stop shortcut."
                )
            );
            Ok(indicator)
        }
    }

    impl Drop for Indicator {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    fn unlocked_desktop() -> Result<()> {
        let desktop = unsafe { OpenInputDesktop(0, 0, DESKTOP_READOBJECTS) };
        let mut name = [0u16; 64];
        let mut needed = 0;
        let unlocked = if desktop.is_null() {
            false
        } else {
            let status = unsafe {
                GetUserObjectInformationW(
                    desktop,
                    UOI_NAME,
                    name.as_mut_ptr().cast(),
                    size_of::<[u16; 64]>() as u32,
                    &mut needed,
                )
            };
            unsafe {
                CloseDesktop(desktop);
            }
            status != 0
                && name
                    .iter()
                    .copied()
                    .take_while(|&unit| unit != 0)
                    .eq("Default".encode_utf16())
        };
        ensure!(
            unlocked,
            tr!(
                "Разблокируй рабочий стол Windows.",
                "Unlock the Windows desktop."
            )
        );
        Ok(())
    }

    struct DpiContext(DPI_AWARENESS_CONTEXT);

    impl DpiContext {
        fn physical_pixels() -> Self {
            Self(unsafe {
                SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)
            })
        }
    }

    impl Drop for DpiContext {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    SetThreadDpiAwarenessContext(self.0);
                }
            }
        }
    }

    fn running(stop: &AtomicBool) -> Result<()> {
        ensure!(
            !stop.load(Ordering::Acquire),
            tr!(
                "Управление остановлено пользователем.",
                "Desktop control was stopped by the user."
            )
        );
        Ok(())
    }

    #[derive(Clone)]
    pub struct Window {
        handle: usize,
        pid: u32,
        pub title: String,
        pub process: String,
    }

    impl Window {
        pub fn id(&self) -> String {
            format!("{:x}-{}", self.handle, self.pid)
        }

        pub fn describe(&self) -> String {
            let title: String = self.title.chars().filter(|ch| !ch.is_control()).collect();
            format!(
                "{} | {} (PID {}) | {title}",
                self.id(),
                self.process,
                self.pid
            )
        }
    }

    pub fn find(id: &str) -> Option<Window> {
        let foreground = unsafe { GetForegroundWindow() } as usize;
        let windows = list();
        if id.is_empty() {
            return windows
                .into_iter()
                .find(|window| window.handle == foreground);
        }
        let mut matches = windows.into_iter().filter(|window| {
            window.id() == id
                || window.process.eq_ignore_ascii_case(id)
                || window.pid.to_string() == id
        });
        let window = matches.next()?;
        matches.next().is_none().then_some(window)
    }

    fn regular_process(pid: u32) -> bool {
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if process.is_null() {
            return false;
        }
        let mut token = std::ptr::null_mut();
        let opened = unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) };
        unsafe { CloseHandle(process) };
        if opened == 0 {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut returned = 0;
        let status = unsafe {
            GetTokenInformation(
                token,
                TokenElevation,
                std::ptr::from_mut(&mut elevation).cast(),
                size_of::<TOKEN_ELEVATION>() as u32,
                &mut returned,
            )
        };
        unsafe { CloseHandle(token) };
        status != 0 && elevation.TokenIsElevated == 0
    }

    fn process_name(pid: u32) -> String {
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if process.is_null() {
            return String::new();
        }
        let mut path = [0u16; 1024];
        let mut length = path.len() as u32;
        let found =
            unsafe { QueryFullProcessImageNameW(process, 0, path.as_mut_ptr(), &mut length) } != 0;
        unsafe {
            CloseHandle(process);
        }
        if found {
            let path = String::from_utf16_lossy(&path[..length as usize]);
            path.rsplit('\\').next().unwrap_or("").to_owned()
        } else {
            String::new()
        }
    }

    unsafe extern "system" fn collect(hwnd: HWND, param: LPARAM) -> i32 {
        let entries = unsafe { &mut *(param as *mut Vec<Window>) };
        if entries.len() >= 80 {
            return 0;
        }
        if unsafe { IsWindowVisible(hwnd) } == 0 {
            return 1;
        }
        let mut name = [0u16; 256];
        let count = unsafe { GetWindowTextW(hwnd, name.as_mut_ptr(), name.len() as i32) };
        if count > 0 {
            let mut pid = 0;
            unsafe {
                GetWindowThreadProcessId(hwnd, &mut pid);
            }
            if pid != std::process::id()
                && hwnd != unsafe { GetConsoleWindow() }
                && regular_process(pid)
            {
                entries.push(Window {
                    handle: hwnd as usize,
                    pid,
                    title: String::from_utf16_lossy(&name[..count as usize]),
                    process: process_name(pid),
                });
            }
        }
        1
    }
    pub fn list() -> Vec<Window> {
        let mut windows = Vec::new();
        unsafe {
            EnumWindows(Some(collect), &mut windows as *mut Vec<Window> as LPARAM);
        }
        windows
    }

    pub struct Session {
        pub window: Window,
        observed: Option<Observation>,
    }
    struct Observation {
        id: u64,
        captured: Instant,
        rect: RECT,
        foreground: usize,
        tree: Vec<Value>,
    }

    static NEXT_OBSERVATION: AtomicU64 = AtomicU64::new(1);

    impl Session {
        pub fn new(window: Window) -> Self {
            Self {
                window,
                observed: None,
            }
        }
        fn hwnd(&self) -> Result<(HWND, RECT)> {
            let hwnd = self.window.handle as HWND;
            let mut current_pid = 0;
            let mut rect = RECT::default();
            unsafe {
                GetWindowThreadProcessId(hwnd, &mut current_pid);
            }
            ensure!(
                current_pid == self.window.pid
                    && regular_process(current_pid)
                    && unsafe { IsWindowVisible(hwnd) } != 0
                    && unsafe { IsIconic(hwnd) } == 0
                    && unsafe { GetWindowRect(hwnd, &mut rect) } != 0,
                tr!(
                    "Выбранное окно больше не видно",
                    "Selected window is no longer visible"
                )
            );
            Ok((hwnd, rect))
        }
        pub fn observe(
            &mut self,
            screenshot: bool,
            stop: &AtomicBool,
        ) -> Result<(String, Option<String>)> {
            running(stop)?;
            let _dpi = DpiContext::physical_pixels();
            self.observed = None;
            let (_, rect) = self.hwnd()?;
            let foreground = unsafe { GetForegroundWindow() } as usize;
            let expression = if screenshot { CAPTURE } else { UIA };
            let script = format!(
                "$h=[IntPtr]::new([int64]{}); $ErrorActionPreference='Stop'; [Console]::OutputEncoding=[Text.Encoding]::UTF8; {expression}",
                self.window.handle
            );
            let executable = std::env::var_os("SystemRoot")
                .map(PathBuf::from)
                .context(tr!(
                    "Папка Windows недоступна",
                    "Windows directory is unavailable"
                ))?
                .join("System32/WindowsPowerShell/v1.0/powershell.exe");
            use std::os::windows::process::CommandExt;
            let mut command = Command::new(executable);
            command
                .creation_flags(0x08000000)
                .args(["-NoProfile", "-NonInteractive", "-Command", &script])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            let mut process = crate::native::tasks::ProcessTree::spawn(command)?;
            let output = process
                .child_mut()
                .stdout
                .take()
                .context(tr!("Нет вывода наблюдения", "Missing observation output"))?;
            let reader = thread::spawn(move || -> std::io::Result<Vec<u8>> {
                let mut bytes = Vec::new();
                output.take(512 * 1024 + 1).read_to_end(&mut bytes)?;
                Ok(bytes)
            });
            let deadline = Instant::now() + Duration::from_secs(10);
            let status = loop {
                running(stop)?;
                if let Some(status) = process.child_mut().try_wait()? {
                    break status;
                }
                ensure!(
                    Instant::now() < deadline,
                    tr!(
                        "Время ожидания наблюдения истекло",
                        "Window observation timed out"
                    )
                );
                thread::sleep(Duration::from_millis(35));
            };
            ensure!(
                status.success(),
                tr!(
                    "Не удалось прочитать выбранное окно",
                    "Could not read the selected window"
                )
            );
            let bytes = reader.join().map_err(|_| {
                anyhow::anyhow!(tr!(
                    "Наблюдение за окном остановлено",
                    "Window observation stopped"
                ))
            })??;
            let text = String::from_utf8_lossy(&bytes);
            ensure!(
                text.len() <= 512 * 1024 && !text.trim().is_empty(),
                tr!(
                    "Наблюдение пустое или слишком большое",
                    "Window observation is empty or too large"
                )
            );
            let (_, current_rect) = self.hwnd()?;
            ensure!(
                same_rect(rect, current_rect)
                    && foreground == unsafe { GetForegroundWindow() } as usize,
                tr!(
                    "Окно или фокус изменились. Повтори наблюдение.",
                    "The window or focus changed. Observe again."
                )
            );
            running(stop)?;
            let tree = if screenshot {
                Vec::new()
            } else {
                serde_json::from_str::<Vec<Value>>(&text)?
            };
            let id = NEXT_OBSERVATION.fetch_add(1, Ordering::Relaxed);
            self.observed = Some(Observation {
                id,
                captured: Instant::now(),
                rect,
                foreground,
                tree,
            });
            let caption = trf!(
                "Окно: {} · наблюдение {id}",
                "Window: {} · observation {id}",
                self.window.describe()
            );
            if screenshot {
                Ok((
                    format!(
                        "{caption} · {}×{}",
                        rect.right - rect.left,
                        rect.bottom - rect.top
                    ),
                    Some(text.trim().to_owned()),
                ))
            } else {
                Ok((
                    format!(
                        "{caption}\n{}",
                        text.chars().take(12000).collect::<String>()
                    ),
                    None,
                ))
            }
        }
        pub fn input(&mut self, args: &Value, stop: &AtomicBool) -> Result<String> {
            running(stop)?;
            let _dpi = DpiContext::physical_pixels();
            let (hwnd, rect) = self.hwnd()?;
            let observation = self.observed.take().context(tr!(
                "Сначала прочитай окно.",
                "Observe the window before requesting input."
            ))?;
            let focused = unsafe { GetForegroundWindow() } as usize;
            ensure!(
                args["observation"].as_u64() == Some(observation.id)
                    && args["window"]
                        .as_str()
                        .is_none_or(|id| id == self.window.id())
                    && observation.captured.elapsed() <= Duration::from_secs(90)
                    && same_rect(observation.rect, rect)
                    && focused == observation.foreground,
                tr!(
                    "Наблюдение устарело. Прочитай окно заново.",
                    "The observation is stale. Observe the window again."
                )
            );
            if focused != hwnd as usize {
                unsafe {
                    SetForegroundWindow(hwnd);
                }
            }
            ensure!(
                unsafe { GetForegroundWindow() } == hwnd,
                tr!(
                    "Windows не разрешила переключить фокус. Открой окно и повтори наблюдение.",
                    "Windows did not allow focusing the target. Bring the window forward and observe again."
                )
            );
            let action = args["operation"].as_str().unwrap_or("");
            let events = match action {
                "click" | "scroll" | "drag" => {
                    let coords = if let Some(element) = args["element"].as_u64() {
                        let node = observation
                            .tree
                            .iter()
                            .find(|node| node["id"].as_u64() == Some(element))
                            .context(tr!(
                                "Элемент отсутствует в наблюдении.",
                                "Element is absent from the observation."
                            ))?;
                        ensure!(
                            node["enabled"].as_bool() == Some(true),
                            tr!(
                                "Элемент недоступен для ввода.",
                                "The element is unavailable for input."
                            )
                        );
                        (
                            node["x"].as_i64().unwrap_or_default()
                                + node["width"].as_i64().unwrap_or_default() / 2
                                - i64::from(rect.left),
                            node["y"].as_i64().unwrap_or_default()
                                + node["height"].as_i64().unwrap_or_default() / 2
                                - i64::from(rect.top),
                        )
                    } else {
                        coordinates(args, "x", "y")?
                    };
                    let start = checked_point(hwnd, rect, coords)?;
                    let mut events = vec![absolute_mouse(start)];
                    match action {
                        "click" => events
                            .extend([mouse(MOUSEEVENTF_LEFTDOWN, 0), mouse(MOUSEEVENTF_LEFTUP, 0)]),
                        "drag" => {
                            let end =
                                checked_point(hwnd, rect, coordinates(args, "end_x", "end_y")?)?;
                            events.extend([
                                mouse(MOUSEEVENTF_LEFTDOWN, 0),
                                absolute_mouse(end),
                                mouse(MOUSEEVENTF_LEFTUP, 0),
                            ]);
                        }
                        _ => events.push(mouse(
                            MOUSEEVENTF_WHEEL,
                            args["delta"].as_i64().unwrap_or(-120).clamp(-960, 960) as u32,
                        )),
                    }
                    events
                }
                "type" => {
                    let text = args["text"]
                        .as_str()
                        .context(tr!("Нет текста для ввода", "Missing text"))?;
                    ensure!(
                        !text.is_empty()
                            && text.chars().count() <= 500
                            && !text.chars().any(char::is_control),
                        tr!(
                            "Введи до 500 печатных символов",
                            "Enter up to 500 printable characters"
                        )
                    );
                    text.encode_utf16()
                        .flat_map(|code| {
                            [
                                keyboard(0, code, KEYEVENTF_UNICODE),
                                keyboard(0, code, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP),
                            ]
                        })
                        .collect()
                }
                "key" => {
                    let spec = args["key"]
                        .as_str()
                        .context(tr!("Не указана клавиша", "Missing key"))?;
                    let parts: Vec<_> = spec.split('+').collect();
                    ensure!(
                        !parts.is_empty() && parts.len() <= 3,
                        tr!("Сочетание клавиш не поддерживается", "Unsupported shortcut")
                    );
                    let mut keys = Vec::new();
                    for part in parts {
                        keys.push(match part {
                            "Ctrl" => VK_CONTROL,
                            "Shift" => VK_SHIFT,
                            "Enter" => VK_RETURN,
                            "Escape" => VK_ESCAPE,
                            "Tab" => VK_TAB,
                            "Up" => VK_UP,
                            "Down" => VK_DOWN,
                            "Left" => VK_LEFT,
                            "Right" => VK_RIGHT,
                            "Space" => VK_SPACE,
                            one if one.len() == 1 && one.as_bytes()[0].is_ascii_alphanumeric() => {
                                u16::from(one.as_bytes()[0].to_ascii_uppercase())
                            }
                            _ => bail!(tr!(
                                "Сочетание клавиш не поддерживается",
                                "Unsupported shortcut"
                            )),
                        });
                    }
                    keys.iter()
                        .map(|&key| keyboard(key, 0, 0))
                        .chain(
                            keys.iter()
                                .rev()
                                .map(|&key| keyboard(key, 0, KEYEVENTF_KEYUP)),
                        )
                        .collect()
                }
                _ => bail!(tr!(
                    "Неизвестное действие на рабочем столе",
                    "Unknown desktop input"
                )),
            };
            ensure!(
                unsafe { GetForegroundWindow() } == hwnd,
                "Selected window is no longer active"
            );
            running(stop)?;
            unlocked_desktop()?;
            let sent = unsafe {
                SendInput(
                    events.len() as u32,
                    events.as_ptr(),
                    size_of::<INPUT>() as i32,
                )
            };
            if sent as usize != events.len() {
                let releases: Vec<_> = events[sent as usize..]
                    .iter()
                    .copied()
                    .filter(|event| unsafe {
                        match event.r#type {
                            INPUT_KEYBOARD => event.Anonymous.ki.dwFlags & KEYEVENTF_KEYUP != 0,
                            INPUT_MOUSE => event.Anonymous.mi.dwFlags & MOUSEEVENTF_LEFTUP != 0,
                            _ => false,
                        }
                    })
                    .collect();
                if !releases.is_empty() {
                    unsafe {
                        SendInput(
                            releases.len() as u32,
                            releases.as_ptr(),
                            size_of::<INPUT>() as i32,
                        );
                    }
                }
                bail!(tr!("Windows отклонила ввод.", "Windows rejected input."));
            }
            Ok(tr!(
                "Ввод отправлен. Проверь результат новым наблюдением.",
                "Input sent. Observe again to verify the result."
            )
            .into())
        }
    }
    fn same_rect(a: RECT, b: RECT) -> bool {
        [a.left, a.right, a.top, a.bottom] == [b.left, b.right, b.top, b.bottom]
    }

    fn coordinates(args: &Value, x: &str, y: &str) -> Result<(i64, i64)> {
        let pair = args[x].as_i64().zip(args[y].as_i64());
        pair.context(tr!("Укажи координаты точки.", "Provide point coordinates."))
    }

    fn checked_point(hwnd: HWND, rect: RECT, (x, y): (i64, i64)) -> Result<POINT> {
        ensure!(
            x >= 0
                && y >= 0
                && x < i64::from(rect.right - rect.left)
                && y < i64::from(rect.bottom - rect.top),
            tr!(
                "Точка за пределами окна.",
                "Point is outside the selected window."
            )
        );
        let point = POINT {
            x: rect.left + x as i32,
            y: rect.top + y as i32,
        };
        ensure!(
            unsafe { GetAncestor(WindowFromPoint(point), GA_ROOT) } == hwnd,
            tr!(
                "Точку перекрывает другое окно.",
                "Point is covered by another window."
            )
        );
        Ok(point)
    }

    fn absolute_mouse(point: POINT) -> INPUT {
        let mut event = mouse(
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
            0,
        );
        unsafe {
            let x = GetSystemMetrics(SM_XVIRTUALSCREEN);
            let y = GetSystemMetrics(SM_YVIRTUALSCREEN);
            let width = GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1);
            let height = GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1);
            event.Anonymous.mi.dx = (((i64::from(point.x) - i64::from(x)) * 65536
                + i64::from(width) / 2)
                / i64::from(width))
            .clamp(0, 65535) as i32;
            event.Anonymous.mi.dy = (((i64::from(point.y) - i64::from(y)) * 65536
                + i64::from(height) / 2)
                / i64::from(height))
            .clamp(0, 65535) as i32;
        }
        event
    }

    fn keyboard(vk: u16, scan: u16, flags: u32) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: scan,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }
    fn mouse(flags: u32, data: u32) -> INPUT {
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: data,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }
    const UIA: &str = r#"Add-Type -AssemblyName UIAutomationClient,UIAutomationTypes;
$element=[Windows.Automation.AutomationElement]::FromHandle($h);
$count=0; $walker=[Windows.Automation.TreeWalker]::ControlViewWalker;
$queue=New-Object 'System.Collections.Generic.Queue[Windows.Automation.AutomationElement]';
$items=New-Object Collections.Generic.List[object];
$queue.Enqueue($element); while($queue.Count -gt 0 -and $count -lt 80) {
  $node=$queue.Dequeue(); $item=$node.Current;
  if (-not $item.IsPassword -and -not $item.IsOffscreen) {
    $bounds=$item.BoundingRectangle;
    if (-not $bounds.IsEmpty) {
      $name = [string]$item.Name;
      if ($name.Length -gt 180) { $name=$name.Substring(0,180); }
      $items.Add(@{id=$count;role=$item.ControlType.ProgrammaticName;label=$name;
        x=[int]$bounds.X;y=[int]$bounds.Y;width=[int]$bounds.Width;height=[int]$bounds.Height;
        focused=$item.HasKeyboardFocus;enabled=$item.IsEnabled});
    }
  }
  $count++; $child=$walker.GetFirstChild($node);
  while($child -and $queue.Count -lt 120) { $queue.Enqueue($child); $child=$walker.GetNextSibling($child) }
}
ConvertTo-Json -Compress -InputObject @($items.ToArray())"#;
    const CAPTURE: &str = r#"Add-Type -AssemblyName System.Drawing;
Add-Type -TypeDefinition 'using System; using System.Runtime.InteropServices;
public static class Snap {
[StructLayout(LayoutKind.Sequential)] public struct R { public int Left, Top, Right, Bottom; }
[DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out R r);
[DllImport("user32.dll")] public static extern IntPtr SetThreadDpiAwarenessContext(IntPtr context);
[DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr dc, uint flags);
}';
$null=[Snap]::SetThreadDpiAwarenessContext([IntPtr]::new(-4));
$r=New-Object 'Snap+R'; if (-not [Snap]::GetWindowRect($h,[ref]$r)) { throw 'Window closed' };
$w=$r.Right-$r.Left; $height=$r.Bottom-$r.Top;
if ($w -lt 1 -or $height -lt 1 -or $w -gt 4000 -or $height -gt 2400) { throw 'Window too large' };
$bitmap=New-Object Drawing.Bitmap($w,$height); $gfx=[Drawing.Graphics]::FromImage($bitmap);
$dc=$gfx.GetHdc(); try { if (-not [Snap]::PrintWindow($h,$dc,2)) { throw 'Capture blocked' } } finally { $gfx.ReleaseHdc($dc); $gfx.Dispose() }
$stream=New-Object IO.MemoryStream; $bitmap.Save($stream,[Drawing.Imaging.ImageFormat]::Jpeg);
[Convert]::ToBase64String($stream.ToArray()); $stream.Dispose(); $bitmap.Dispose()"#;
    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn desktop_consent_applies_to_one_chat_and_stop_needs_user_resume() {
            use crate::native::permissions::Mode;
            let mut access = Access::default();
            assert!(access.needs_consent(Mode::Ask));
            assert!(access.needs_consent(Mode::Auto));
            assert!(!access.needs_consent(Mode::Full));
            access.approved = true;
            assert!(!access.needs_consent(Mode::Auto));
            access.reset_chat();
            assert!(access.needs_consent(Mode::Auto));
            access.stop();
            access.reset_chat();
            assert!(access.paused());
            access.resume();
            assert!(!access.paused());
        }

        #[test]
        #[ignore = "requires an interactive Windows desktop"]
        fn observes_a_synthetic_window() -> Result<()> {
            let (send, receive) = std::sync::mpsc::channel();
            let fixture = thread::spawn(move || {
                let title: Vec<_> = "KarryCode window smoke test\0".encode_utf16().collect();
                let class: Vec<_> = "STATIC\0".encode_utf16().collect();
                let hwnd = unsafe {
                    CreateWindowExW(
                        0,
                        class.as_ptr(),
                        title.as_ptr(),
                        WS_OVERLAPPEDWINDOW,
                        80,
                        80,
                        400,
                        180,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    )
                };
                if hwnd.is_null() {
                    let _ = send.send(0);
                    return;
                }
                unsafe {
                    ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                }
                let _ = send.send(hwnd as usize);
                let mut message = MSG::default();
                while unsafe { GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) } > 0 {
                    unsafe {
                        TranslateMessage(&message);
                        DispatchMessageW(&message);
                    }
                    if unsafe { IsWindow(hwnd) } == 0 {
                        break;
                    }
                }
            });
            let handle = receive.recv_timeout(Duration::from_secs(2))?;
            ensure!(handle != 0, "Could not create the test window");
            let mut session = Session::new(Window {
                handle,
                pid: std::process::id(),
                title: "KarryCode window smoke test".into(),
                process: "karrycode.exe".into(),
            });
            let result = (|| -> Result<()> {
                let (text, _) = session.observe(false, &AtomicBool::new(false))?;
                assert!(text.contains("KarryCode window smoke test"));
                let stale = serde_json::json!({"window": session.window.id(), "observation": 0, "operation":"key", "key":"Enter"});
                assert!(session.input(&stale, &AtomicBool::new(false)).is_err());
                let (_, image) = session.observe(true, &AtomicBool::new(false))?;
                ensure!(
                    image.is_some_and(|image| image.starts_with("/9j/")),
                    "No captured JPEG"
                );
                Ok(())
            })();
            unsafe {
                PostMessageW(handle as HWND, WM_CLOSE, 0, 0);
            }
            fixture
                .join()
                .map_err(|_| anyhow::anyhow!("Test window stopped"))?;
            result
        }
    }
}
#[cfg(windows)]
pub use windows::*;
