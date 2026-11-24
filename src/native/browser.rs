use super::permissions::Mode;
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    fs,
    net::{SocketAddr, TcpStream},
    path::PathBuf,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use tungstenite::{Message, WebSocket, client::client_with_config, protocol::WebSocketConfig};

pub(super) fn review_required(
    mode: Mode,
    operation: &str,
    local_preview: impl FnOnce() -> bool,
) -> bool {
    match mode {
        Mode::Full => false,
        Mode::Ask => true,
        Mode::Auto => match operation {
            "navigate" | "snapshot" | "tabs" | "console" | "select_tab" | "new_tab"
            | "close_tab" => false,
            "click" | "fill" | "type" | "scroll" => !local_preview(),
            _ => true,
        },
    }
}

fn preview_frame(tree: &Value, address: &str) -> bool {
    let url = |value: &Value| url::Url::parse(value.as_str().unwrap_or_default()).ok();
    let Some(frame) = url(&tree["frame"]["url"]) else {
        return false;
    };
    let Ok(preview) = url::Url::parse(address) else {
        return false;
    };
    frame.origin() == preview.origin() && tree["childFrames"].as_array().is_none_or(Vec::is_empty)
}

pub struct Browser {
    socket: WebSocket<TcpStream>,
    process: super::tasks::ProcessTree,
    _profile: tempfile::TempDir,
    session: String,
    target: String,
    next_id: u64,
    events: VecDeque<String>,
}

fn executable() -> Result<PathBuf> {
    #[cfg(windows)]
    for name in ["PROGRAMFILES(X86)", "PROGRAMFILES", "LOCALAPPDATA"] {
        if let Some(base) = std::env::var_os(name) {
            for suffix in [
                "Microsoft/Edge/Application/msedge.exe",
                "Google/Chrome/Application/chrome.exe",
            ] {
                let path = PathBuf::from(&base).join(suffix);
                if path.is_file() {
                    return Ok(path);
                }
            }
        }
    }
    #[cfg(not(windows))]
    for base in ["/usr/bin", "/usr/local/bin", "/opt/google/chrome"] {
        for name in [
            "chromium",
            "chromium-browser",
            "google-chrome-stable",
            "google-chrome",
            "microsoft-edge",
            "chrome",
        ] {
            let path = PathBuf::from(base).join(name);
            if path.is_file() {
                return Ok(path);
            }
        }
    }
    bail!(tr!(
        "Установи Chromium, Chrome или Edge.",
        "Install Chromium, Chrome, or Edge."
    ))
}

impl Browser {
    pub fn start() -> Result<Self> {
        let program = executable()?;
        let profile = crate::platform::private_dir()?;
        let mut command = Command::new(program);
        let mut profile_flag = std::ffi::OsString::from("--user-data-dir=");
        profile_flag.push(profile.path());
        command
            .arg(profile_flag)
            .args([
                "--headless",
                "--remote-debugging-port=0",
                "--remote-debugging-address=127.0.0.1",
                "--window-size=1280,900",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-extensions",
                "--disable-sync",
                "--disable-background-networking",
                "--proxy-bypass-list=localhost;127.0.0.1;[::1]",
                "about:blank",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let mut process = super::tasks::ProcessTree::spawn(command)?;
        let deadline = Instant::now() + Duration::from_secs(12);
        let (port, endpoint) = loop {
            if let Ok(contents) = fs::read_to_string(profile.path().join("DevToolsActivePort"))
                && let Some((port, endpoint)) = contents.split_once('\n')
                && let Ok(port) = port.parse::<u16>()
                && endpoint.trim().starts_with("/devtools/browser/")
            {
                break (port, endpoint.trim().to_owned());
            }
            if let Some(status) = process.child_mut().try_wait()? {
                bail!("Browser exited: {status}");
            }
            ensure!(Instant::now() < deadline, "Browser setup timed out");
            thread::sleep(Duration::from_millis(50));
        };
        let address = SocketAddr::from(([127, 0, 0, 1], port));
        let stream = TcpStream::connect_timeout(&address, Duration::from_secs(3))?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        let config = WebSocketConfig::default()
            .max_message_size(Some(4 * 1024 * 1024))
            .max_frame_size(Some(4 * 1024 * 1024));
        let (socket, _) =
            client_with_config(format!("ws://{address}{endpoint}"), stream, Some(config))
                .map_err(|error| anyhow::anyhow!("Browser connection failed: {error}"))?;
        let mut browser = Self {
            socket,
            process,
            _profile: profile,
            session: String::new(),
            target: String::new(),
            next_id: 0,
            events: VecDeque::new(),
        };
        let targets = browser.rpc("Target.getTargets", json!({}), false)?;
        let first = targets["targetInfos"]
            .as_array()
            .and_then(|targets| targets.iter().find(|target| target["type"] == "page"))
            .and_then(|target| target["targetId"].as_str())
            .context(tr!("Нет вкладки браузера", "No browser tab"))?;
        browser.select(first)?;
        Ok(browser)
    }

    fn rpc(&mut self, method: &str, params: Value, page: bool) -> Result<Value> {
        self.next_id += 1;
        let mut request = json!({"id": self.next_id, "method":method, "params":params});
        if page {
            request["sessionId"] = json!(self.session);
        }
        self.socket
            .send(Message::text(request.to_string()))
            .with_context(|| format!("Browser could not send {method}"))?;
        let deadline = Instant::now() + Duration::from_secs(12);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            ensure!(
                !remaining.is_zero(),
                "Browser timed out waiting for {method}"
            );
            self.socket
                .get_mut()
                .set_read_timeout(Some(remaining.min(Duration::from_secs(5))))?;
            let event = match self.socket.read() {
                Ok(event) => event,
                Err(tungstenite::Error::Io(error))
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) =>
                {
                    continue;
                }
                Err(error) => {
                    return Err(anyhow::Error::new(error)
                        .context(format!("Browser could not receive the reply to {method}")));
                }
            };
            let Ok(text) = event.to_text() else {
                continue;
            };
            let value: Value = serde_json::from_str(text)?;
            if value["id"].as_u64() == Some(self.next_id) {
                if value.get("error").is_some() {
                    bail!(
                        "{}: {}",
                        method,
                        value["error"]["message"]
                            .as_str()
                            .unwrap_or("browser error")
                    );
                }
                return Ok(value["result"].clone());
            }
            self.record(&value);
        }
    }

    fn record(&mut self, value: &Value) {
        let label = match value["method"].as_str() {
            Some("Runtime.exceptionThrown") => format!(
                "Page error: {}",
                value["params"]["exceptionDetails"]["text"]
            ),
            Some("Runtime.consoleAPICalled") => {
                let args = value["params"]["args"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .take(5)
                    .map(|arg| {
                        arg["value"]
                            .as_str()
                            .or(arg["description"].as_str())
                            .unwrap_or("")
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                format!("console {}: {args}", value["params"]["type"])
            }
            Some("Log.entryAdded") => {
                let entry = &value["params"]["entry"];
                format!(
                    "Browser log {}: {}",
                    entry["level"],
                    entry["text"].as_str().unwrap_or("")
                )
            }
            Some("Network.responseReceived")
                if value["params"]["response"]["status"]
                    .as_u64()
                    .is_some_and(|status| status >= 400) =>
            {
                let response = &value["params"]["response"];
                let address = response["url"]
                    .as_str()
                    .unwrap_or("")
                    .split(['?', '#'])
                    .next()
                    .unwrap_or("");
                format!("HTTP {} {address}", response["status"])
            }
            Some("Network.loadingFailed") if value["params"]["canceled"] != true => {
                format!("Request failed: {}", value["params"]["errorText"])
            }
            _ => return,
        };
        if self.events.len() >= 32 {
            self.events.pop_front();
        }
        self.events.push_back(
            label
                .chars()
                .filter(|c| !c.is_control())
                .take(300)
                .collect(),
        );
    }

    fn select(&mut self, id: &str) -> Result<()> {
        if !self.session.is_empty() {
            let _ = self.rpc(
                "Target.detachFromTarget",
                json!({"sessionId":self.session}),
                false,
            );
        }
        let target = self.rpc(
            "Target.attachToTarget",
            json!({"targetId":id,"flatten":true}),
            false,
        )?;
        self.session = target["sessionId"]
            .as_str()
            .context(tr!("Не удалось открыть вкладку", "Tab cannot be opened"))?
            .to_owned();
        self.target = id.to_owned();
        for method in [
            "Page.enable",
            "Runtime.enable",
            "Network.enable",
            "DOM.enable",
            "Log.enable",
        ] {
            self.rpc(method, json!({}), true)?;
        }
        self.events.clear();
        Ok(())
    }

    pub(super) fn is_local_preview(&mut self, address: &str) -> bool {
        self.rpc("Page.getFrameTree", json!({}), true)
            .is_ok_and(|result| preview_frame(&result["frameTree"], address))
    }

    pub fn call(&mut self, operation: &str, args: &Value) -> Result<(String, Option<String>)> {
        let text = match operation {
            "navigate" => {
                let address = args["url"]
                    .as_str()
                    .context(tr!("Не указан URL", "Missing URL"))?;
                let url = url::Url::parse(address)?;
                ensure!(
                    matches!(url.scheme(), "http" | "https")
                        && url.username().is_empty()
                        && url.password().is_none(),
                    "Use an HTTP(S) URL without credentials"
                );
                self.events.clear();
                let mut response = self.rpc("Page.navigate", json!({"url":url.as_str()}), true)?;
                if response["errorText"] == "net::ERR_SOCKET_NOT_CONNECTED" {
                    thread::sleep(Duration::from_millis(150));
                    response = self.rpc("Page.navigate", json!({"url":url.as_str()}), true)?;
                }
                if let Some(error) = response["errorText"].as_str() {
                    bail!("Navigation to {url} failed: {error}");
                }
                let loader = response["loaderId"].as_str();
                let deadline = Instant::now() + Duration::from_secs(5);
                while Instant::now() < deadline {
                    let page = self.rpc("Page.getFrameTree", json!({}), true)?;
                    let loaded = loader
                        .is_none_or(|loader| page["frameTree"]["frame"]["loaderId"] == loader);
                    if loaded
                        && let Ok(ready) = self.rpc(
                            "Runtime.evaluate",
                            json!({"expression":"document.readyState","returnByValue":true}),
                            true,
                        )
                        && ready["result"]["value"] == "complete"
                    {
                        break;
                    }
                    thread::sleep(Duration::from_millis(40));
                }
                self.snapshot()?
            }
            "tabs" => {
                let targets = self.rpc("Target.getTargets", json!({}), false)?;
                targets["targetInfos"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|tab| tab["type"] == "page")
                    .take(20)
                    .map(|tab| format!("{}  {}  {}", tab["targetId"], tab["title"], tab["url"]))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            "new_tab" => {
                let created = self.rpc(
                    "Target.createTarget",
                    json!({"url":"about:blank","background":true}),
                    false,
                )?;
                self.select(
                    created["targetId"]
                        .as_str()
                        .context(tr!("Не удалось создать вкладку", "Could not create tab"))?,
                )?;
                "Tab created".into()
            }
            "select_tab" => {
                self.select(
                    args["tab"]
                        .as_str()
                        .context(tr!("Не указан ID вкладки", "Missing tab ID"))?,
                )?;
                "Tab selected".into()
            }
            "close_tab" => {
                let id = args["tab"].as_str().unwrap_or(&self.target).to_owned();
                let result = self.rpc("Target.closeTarget", json!({"targetId":id}), false)?;
                ensure!(result["success"] != false, "Tab could not be closed");
                let deadline = Instant::now() + Duration::from_secs(3);
                let available = loop {
                    let targets = self.rpc("Target.getTargets", json!({}), false)?;
                    let still_open = targets["targetInfos"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|tab| tab["targetId"] == id);
                    if !still_open {
                        break targets;
                    }
                    ensure!(
                        Instant::now() < deadline,
                        "Browser did not finish closing the tab"
                    );
                    thread::sleep(Duration::from_millis(40));
                };
                if id == self.target {
                    self.session.clear();
                    if let Some(next) = available["targetInfos"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .find(|tab| tab["type"] == "page")
                        .and_then(|tab| tab["targetId"].as_str())
                    {
                        self.select(next)?;
                    } else {
                        let next = self.rpc(
                            "Target.createTarget",
                            json!({"url":"about:blank","background":true}),
                            false,
                        )?;
                        self.select(next["targetId"].as_str().context(tr!(
                            "Не удалось создать новую вкладку взамен старой",
                            "Could not create replacement tab"
                        ))?)?;
                    }
                }
                "Tab closed".into()
            }
            "snapshot" => self.snapshot()?,
            "console" => {
                self.rpc(
                    "Runtime.evaluate",
                    json!({"expression":"0","returnByValue":true}),
                    true,
                )?;
                if self.events.is_empty() {
                    "No console messages or failed requests observed".into()
                } else {
                    self.events.iter().cloned().collect::<Vec<_>>().join("\n")
                }
            }
            "screenshot" => {
                let result = self.rpc(
                    "Page.captureScreenshot",
                    json!({"format":"jpeg","quality":65}),
                    true,
                )?;
                let image = result["data"]
                    .as_str()
                    .context(tr!("Нет данных снимка", "Missing image data"))?;
                ensure!(
                    image.len() <= 512 * 1024,
                    "Screenshot exceeds the image limit"
                );
                return Ok(("Browser screenshot".into(), Some(image.to_owned())));
            }
            "click" | "type" | "fill" => {
                let dom = self.rpc("DOM.getDocument", json!({"depth":0}), true)?;
                let node = if let Some(backend_id) = args["node"].as_u64() {
                    let resolved = self.rpc(
                        "DOM.pushNodesByBackendIdsToFrontend",
                        json!({"backendNodeIds":[backend_id]}),
                        true,
                    )?;
                    resolved["nodeIds"][0].as_u64()
                } else {
                    let selector = args["selector"].as_str().context(tr!(
                        "Укажи ID элемента из снимка или CSS-селектор",
                        "Enter a node ID from the snapshot or a CSS selector"
                    ))?;
                    ensure!(
                        !selector.is_empty() && selector.len() <= 256,
                        "Expected a CSS selector up to 256 bytes"
                    );
                    self.rpc(
                        "DOM.querySelector",
                        json!({"nodeId":dom["root"]["nodeId"], "selector":selector}),
                        true,
                    )?["nodeId"]
                        .as_u64()
                }
                .filter(|node| *node != 0)
                .context(tr!("Элемент не найден", "No matching element"))?;
                if matches!(operation, "type" | "fill") {
                    let text = args["text"]
                        .as_str()
                        .context(tr!("Нет текста для ввода", "Missing text"))?;
                    ensure!(text.len() <= 8192, "Text exceeds the insertion limit");
                    self.rpc("DOM.focus", json!({"nodeId":node}), true)?;
                    if operation == "fill" {
                        self.rpc("Input.dispatchKeyEvent", json!({"type":"keyDown","key":"a","code":"KeyA","modifiers":2,"windowsVirtualKeyCode":65,"commands":["selectAll"]}), true)?;
                        self.rpc("Input.dispatchKeyEvent", json!({"type":"keyUp","key":"a","code":"KeyA","modifiers":2,"windowsVirtualKeyCode":65}), true)?;
                    }
                    self.rpc("Input.insertText", json!({"text":text}), true)?;
                } else {
                    self.rpc("DOM.scrollIntoViewIfNeeded", json!({"nodeId":node}), true)?;
                    let quad = self.rpc("DOM.getBoxModel", json!({"nodeId":node}), true)?;
                    let bounds = &quad["model"]["content"];
                    let x = (bounds[0].as_f64().context(tr!(
                        "Границы элемента не видны",
                        "Element has no visible box"
                    ))? + bounds[4].as_f64().context(tr!(
                        "Границы элемента не видны",
                        "Element has no visible box"
                    ))?) / 2.0;
                    let y = (bounds[1].as_f64().context(tr!(
                        "Границы элемента не видны",
                        "Element has no visible box"
                    ))? + bounds[5].as_f64().context(tr!(
                        "Границы элемента не видны",
                        "Element has no visible box"
                    ))?) / 2.0;
                    for kind in ["mousePressed", "mouseReleased"] {
                        self.rpc(
                            "Input.dispatchMouseEvent",
                            json!({"type":kind,"x":x,"y":y,"button":"left","clickCount":1}),
                            true,
                        )?;
                    }
                }
                self.snapshot()?
            }
            "scroll" => {
                let delta = args["delta"].as_i64().unwrap_or(600).clamp(-2000, 2000);
                self.rpc(
                    "Input.dispatchMouseEvent",
                    json!({"type":"mouseWheel","x":640,"y":450,"deltaY":delta,"deltaX":0}),
                    true,
                )?;
                "Scrolled".into()
            }
            _ => bail!(tr!(
                "Неизвестное действие браузера",
                "Unknown browser action"
            )),
        };
        Ok((text, None))
    }

    fn snapshot(&mut self) -> Result<String> {
        let result = self.rpc("Runtime.evaluate", json!({"expression":"({url:location.href,title:document.title,state:document.readyState})","returnByValue":true}), true)?;
        let page = &result["result"]["value"];
        let tree = self.rpc("Accessibility.getFullAXTree", json!({"depth":16}), true)?;
        let mut result = format!(
            "Page: {}\nURL: {}\nLoad: {}",
            page["title"], page["url"], page["state"]
        );
        for node in tree["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|node| node["ignored"] != true)
            .take(145)
        {
            use std::fmt::Write as _;
            let name = node["name"]["value"].as_str().unwrap_or("");
            let role = node["role"]["value"].as_str().unwrap_or("");
            if !name.is_empty() {
                let label: String = name.chars().filter(|c| !c.is_control()).take(190).collect();
                if let Some(node_id) = node["backendDOMNodeId"].as_u64() {
                    let _ = write!(result, "\n[node {node_id}] {role}: {label}");
                } else {
                    let _ = write!(result, "\n{role}: {label}");
                }
            }
        }
        Ok(result)
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.socket.send(Message::text(
            json!({"id":self.next_id + 1,"method":"Browser.close"}).to_string(),
        ));
        let _ = self.process.child_mut().try_wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_local_preview_input_is_approved_in_auto_mode() {
        let preview = "http://127.0.0.1:49152/";
        let tree = |url| json!({"frame":{"url":url}});
        assert!(preview_frame(
            &tree("http://127.0.0.1:49152/docs.html#api"),
            preview
        ));
        assert!(!preview_frame(&tree("http://127.0.0.1:49153/"), preview));
        assert!(!preview_frame(&tree("https://example.org/"), preview));
        assert!(!preview_frame(
            &json!({"frame":{"url":preview},"childFrames":[tree("https://example.org/")]}),
            preview
        ));
        assert!(!review_required(Mode::Auto, "click", || true));
        assert!(review_required(Mode::Auto, "click", || false));
        assert!(review_required(Mode::Ask, "click", || true));
        assert!(review_required(Mode::Auto, "screenshot", || true));
        assert!(!review_required(Mode::Full, "click", || false));
    }

    #[test]
    #[ignore = "requires an installed Chromium browser"]
    fn browser_checks_a_local_project_and_returns_an_image() -> Result<()> {
        let root = tempfile::tempdir()?;
        super::super::changes::Batch::prepare(root.path(), super::super::project::starter(0))?
            .apply()?;
        fs::write(
            root.path().join("form.html"),
            "<!doctype html><title>Form</title><label for=name>Name</label><input id=name value=old><div style='height:150vh'></div>",
        )?;
        let preview = super::super::preview::Preview::start(root.path())?;
        let mut browser = Browser::start()?;
        let response = ureq::get(preview.url()).call()?;
        assert!(response.into_string()?.contains("Add one"));
        browser.call("navigate", &json!({"url":preview.url()}))?;
        assert!(browser.is_local_preview(preview.url()));
        assert!(!browser.is_local_preview("http://127.0.0.1:1/"));
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let (tree, _) = browser.call("snapshot", &json!({}))?;
            if tree.contains("Add one") {
                break;
            }
            ensure!(Instant::now() < deadline, "Page did not load");
            thread::sleep(Duration::from_millis(80));
        }
        browser.call("click", &json!({"selector":"#add"}))?;
        let state = browser.rpc(
            "Runtime.evaluate",
            json!({"expression":"document.querySelector('#count').value","returnByValue":true}),
            true,
        )?;
        assert_eq!(state["result"]["value"], "1");
        let (_, image) = browser.call("screenshot", &json!({}))?;
        assert!(image.unwrap().starts_with("/9j/"));
        let (tabs, _) = browser.call("tabs", &json!({}))?;
        assert!(tabs.contains(preview.url()));
        browser.call(
            "navigate",
            &json!({"url":format!("{}form.html",preview.url())}),
        )?;
        let deadline = Instant::now() + Duration::from_secs(5);
        let node = loop {
            let (tree, _) = browser.call("snapshot", &json!({}))?;
            if let Some(id) = tree
                .lines()
                .find(|line| line.contains("textbox: Name"))
                .and_then(|line| line.strip_prefix("[node "))
                .and_then(|line| line.split(']').next())
                .and_then(|id| id.parse::<u64>().ok())
            {
                break id;
            }
            ensure!(Instant::now() < deadline, "Form did not load");
            thread::sleep(Duration::from_millis(80));
        };
        browser.call("fill", &json!({"node":node,"text":"KarryCode"}))?;
        let result = browser.rpc(
            "Runtime.evaluate",
            json!({"expression":"document.querySelector('#name').value","returnByValue":true}),
            true,
        )?;
        assert_eq!(result["result"]["value"], "KarryCode");
        browser.call("type", &json!({"selector":"#name","text":" QA"}))?;
        let result = browser.rpc(
            "Runtime.evaluate",
            json!({"expression":"document.querySelector('#name').value","returnByValue":true}),
            true,
        )?;
        assert_eq!(result["result"]["value"], "KarryCode QA");
        browser.call("scroll", &json!({"delta":400}))?;
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let position = browser.rpc(
                "Runtime.evaluate",
                json!({"expression":"window.scrollY","returnByValue":true}),
                true,
            )?;
            if position["result"]["value"]
                .as_f64()
                .is_some_and(|y| y > 0.0)
            {
                break;
            }
            ensure!(
                Instant::now() < deadline,
                "Browser scroll did not move the page"
            );
            thread::sleep(Duration::from_millis(50));
        }
        let original = browser.target.clone();
        browser.call("new_tab", &json!({}))?;
        let second = browser.target.clone();
        assert_ne!(original, second);
        browser.call("select_tab", &json!({"tab":original}))?;
        assert_eq!(browser.target, original);
        browser.call("close_tab", &json!({"tab":second}))?;
        let (tabs, _) = browser.call("tabs", &json!({}))?;
        assert!(
            tabs.contains(&original) && !tabs.contains(&second),
            "{tabs}"
        );
        browser.call("new_tab", &json!({}))?;
        browser.call("close_tab", &json!({}))?;
        assert_eq!(browser.target, original);
        Ok(())
    }
}
