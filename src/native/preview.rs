use anyhow::{Result, ensure};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

pub struct Preview {
    url: String,
    stopped: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Preview {
    pub fn start(root: &Path) -> Result<Self> {
        let root = root.canonicalize()?;
        ensure!(
            super::files::safe_path(&root, "index.html").is_ok_and(|path| path.is_file()),
            "No index.html in the selected folder"
        );
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        listener.set_nonblocking(true)?;
        let url = format!("http://{}/", listener.local_addr()?);
        let stopped = Arc::new(AtomicBool::new(false));
        let flag = stopped.clone();
        let thread = thread::spawn(move || {
            let mut workers: Vec<thread::JoinHandle<()>> = Vec::new();
            while !flag.load(Ordering::Relaxed) {
                let mut index = 0;
                while index < workers.len() {
                    if workers[index].is_finished() {
                        let _ = workers.swap_remove(index).join();
                    } else {
                        index += 1;
                    }
                }
                if workers.len() == 16 {
                    thread::sleep(Duration::from_millis(25));
                    continue;
                }
                match listener.accept() {
                    Ok((stream, _)) => {
                        let root = root.clone();
                        workers.push(thread::spawn(move || {
                            let _ = handle(stream, &root);
                        }));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(25))
                    }
                    Err(_) => break,
                }
            }
            for worker in workers {
                let _ = worker.join();
            }
        });
        Ok(Self {
            url,
            stopped,
            thread: Some(thread),
        })
    }
    pub fn url(&self) -> &str {
        &self.url
    }
}
impl Drop for Preview {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
fn handle(mut stream: TcpStream, root: &Path) -> Result<()> {
    use std::io::Read as _;
    // On Windows an accepted socket inherits the listener's non-blocking mode, which
    // would make reads fail with WouldBlock instead of honoring the timeouts below.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_millis(500)))?;
    stream.set_write_timeout(Some(Duration::from_millis(500)))?;
    let mut reader = BufReader::new(&stream).take(8192);
    let mut first = String::new();
    reader.read_line(&mut first)?;
    let mut pieces = first.split_whitespace();
    let method = pieces.next().unwrap_or("");
    let target = pieces.next().unwrap_or("");
    let mut host = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("host")
        {
            host = value.trim().to_owned();
        }
    }
    let expected = stream.local_addr()?.to_string();
    if host != expected || !matches!(method, "GET" | "HEAD") {
        return reply(stream, 403, "text/plain", b"Forbidden", true);
    }
    let part = target.split('?').next().unwrap_or("");
    let relative = if part == "/" {
        "index.html"
    } else {
        part.strip_prefix('/').unwrap_or("")
    };
    let (status, mime, data) =
        serve(root, relative).unwrap_or((404, "text/plain", b"Not found".to_vec()));
    if method == "HEAD" {
        return reply(stream, status, mime, &data, false);
    }
    stream.flush()?;
    reply(stream, status, mime, &data, true)
}
fn serve(root: &Path, rel: &str) -> Result<(u16, &'static str, Vec<u8>)> {
    ensure!(
        !rel.contains(['%', '\\'])
            && !rel.split('/').any(|part| part.is_empty()
                || part.starts_with('.')
                || matches!(part, "profile" | "auth.json")),
        "Invalid preview path"
    );
    let path: PathBuf = super::files::safe_path(root, rel)?;
    let kind = match path.extension().and_then(|s| s.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "png" => "image/png",
        "jpeg" | "jpg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        _ => anyhow::bail!(tr!("Тип файла не поддерживается", "Unsupported asset type")),
    };
    let metadata = fs::metadata(&path)?;
    ensure!(
        metadata.is_file() && metadata.len() <= 8 * 1024 * 1024,
        "Invalid preview asset"
    );
    Ok((200, kind, fs::read(path)?))
}
fn reply(mut stream: TcpStream, status: u16, mime: &str, data: &[u8], body: bool) -> Result<()> {
    let label = match status {
        200 => "OK",
        403 => "Forbidden",
        _ => "Not Found",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {label}\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        data.len()
    )?;
    if body {
        stream.write_all(data)?;
    }
    Ok(())
}
