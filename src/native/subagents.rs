use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fmt,
    sync::{
        Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

pub(super) const MAX_SUBAGENTS: u8 = 30;

#[derive(Clone, Copy, Serialize, Deserialize)]
pub(super) struct Limit {
    pub count: u8,
    pub estimated: bool,
}

#[derive(Debug)]
pub(super) struct ConcurrencyError {
    maximum: Option<u8>,
    detail: String,
}

impl fmt::Display for ConcurrencyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for ConcurrencyError {}

impl ConcurrencyError {
    pub fn redact(&mut self, key: &str) {
        self.detail = self.detail.replace(key, "<key>");
    }

    pub fn parse(body: &str, detail: String) -> Option<Self> {
        let data: Value = serde_json::from_str(body).unwrap_or(Value::Null);
        let error = data.get("error").unwrap_or(&data);
        let code = error
            .get("code")
            .or_else(|| error.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or(body)
            .to_ascii_lowercase();
        let explicit = matches!(
            code,
            "concurrency_limit_exceeded"
                | "concurrent_requests_limit_exceeded"
                | "too_many_concurrent_requests"
        ) || [
            "too many concurrent requests",
            "maximum concurrent requests",
            "concurrent request limit",
            "concurrent requests limit",
            "concurrency limit",
            "too many simultaneous requests",
        ]
        .iter()
        .any(|phrase| message.contains(phrase));
        let maximum = ["concurrency_limit", "max_concurrent_requests"]
            .iter()
            .find_map(|key| {
                error
                    .get(key)
                    .or_else(|| data.get(key))
                    .and_then(Value::as_u64)
            })
            .or_else(|| explicit.then(|| error["limit"].as_u64()).flatten())
            .or_else(|| {
                [
                    "maximum concurrent requests:",
                    "concurrency limit:",
                    "concurrent request limit:",
                    "maximum of ",
                ]
                .iter()
                .find_map(|prefix| {
                    let (_, rest) = message.split_once(prefix)?;
                    let rest = rest.trim_start();
                    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
                    if *prefix == "maximum of "
                        && !rest[digits..]
                            .trim_start()
                            .starts_with("concurrent requests")
                    {
                        return None;
                    }
                    rest[..digits].parse::<u64>().ok()
                })
            });
        (explicit || maximum.is_some()).then(|| Self {
            maximum: maximum
                .filter(|&value| value > 0)
                .map(|value| value.min(u64::from(MAX_SUBAGENTS)) as u8),
            detail,
        })
    }
}

pub(super) struct Batch {
    initial: u8,
    state: Mutex<Active>,
    available: Condvar,
}

struct Active {
    running: u8,
    ceiling: u8,
    learned: Option<Limit>,
}

impl Batch {
    pub fn new(count: u8) -> Self {
        Self {
            initial: count,
            state: Mutex::new(Active {
                running: 0,
                ceiling: count,
                learned: None,
            }),
            available: Condvar::new(),
        }
    }

    pub fn learned(&self) -> Option<Limit> {
        self.state.lock().unwrap().learned
    }

    pub fn request<T>(&self, canceled: &AtomicBool, call: impl FnOnce() -> Result<T>) -> Result<T> {
        let mut state = self.state.lock().unwrap();
        while state.running >= state.ceiling && !canceled.load(Ordering::Relaxed) {
            state = self
                .available
                .wait_timeout(state, Duration::from_millis(50))
                .unwrap()
                .0;
        }
        ensure!(
            !canceled.load(Ordering::Relaxed),
            tr!("Саб-агент остановлен", "Subagent canceled")
        );
        state.running += 1;
        drop(state);
        let _slot = Slot(self);
        let result = call();
        if let Err(error) = &result
            && let Some(error) = error.downcast_ref::<ConcurrencyError>()
        {
            let limit = Limit {
                count: error.maximum.unwrap_or((self.initial / 2).max(1)),
                estimated: error.maximum.is_none(),
            };
            let mut state = self.state.lock().unwrap();
            if state.learned.is_none_or(|current| {
                (current.estimated && !limit.estimated)
                    || (current.estimated == limit.estimated && limit.count < current.count)
            }) {
                state.learned = Some(limit);
                state.ceiling = state.ceiling.min(limit.count);
            }
        }
        result
    }
}

struct Slot<'a>(&'a Batch);

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        self.0.state.lock().unwrap().running -= 1;
        self.0.available.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc, thread, time::Instant};

    #[test]
    fn batch_starts_thirty_calls_without_serializing_them() {
        let batch = Batch::new(MAX_SUBAGENTS);
        let canceled = AtomicBool::new(false);
        let release = AtomicBool::new(false);
        let (send, receive) = mpsc::channel();
        thread::scope(|scope| {
            for _ in 0..MAX_SUBAGENTS {
                scope.spawn(|| {
                    batch
                        .request(&canceled, || {
                            send.send(())?;
                            let deadline = Instant::now() + Duration::from_secs(6);
                            while !release.load(Ordering::Acquire) && Instant::now() < deadline {
                                thread::sleep(Duration::from_millis(5));
                            }
                            Ok(())
                        })
                        .unwrap()
                });
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            let started = (0..MAX_SUBAGENTS)
                .take_while(|_| {
                    receive
                        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                        .is_ok()
                })
                .count();
            release.store(true, Ordering::Release);
            assert_eq!(started, usize::from(MAX_SUBAGENTS));
        });
    }

    #[test]
    fn only_explicit_concurrency_errors_reduce_the_batch_limit() {
        let cancel = AtomicBool::new(false);
        let batch = Batch::new(30);
        let normal = r#"{"error":{"code":"rate_limit_exceeded","limit":3,"message":"Too many tokens per minute"}}"#;
        assert!(ConcurrencyError::parse(normal, "HTTP 429".into()).is_none());
        let _ = batch.request::<()>(&cancel, || anyhow::bail!("HTTP 429"));
        assert!(batch.learned().is_none());

        let estimate =
            ConcurrencyError::parse("Too many concurrent requests", "HTTP 429".into()).unwrap();
        let _ = batch.request::<()>(&cancel, || Err(estimate.into()));
        assert_eq!(batch.learned().unwrap().count, 15);
        assert!(batch.learned().unwrap().estimated);

        let exact = ConcurrencyError::parse(
            r#"{"error":{"code":"concurrency_limit_exceeded","limit":8}}"#,
            "HTTP 429".into(),
        )
        .unwrap();
        let _ = batch.request::<()>(&cancel, || Err(exact.into()));
        assert_eq!(batch.learned().unwrap().count, 8);
        assert!(!batch.learned().unwrap().estimated);
        assert_eq!(batch.state.lock().unwrap().ceiling, 8);

        cancel.store(true, Ordering::Relaxed);
        assert!(
            batch
                .request(&cancel, || -> Result<()> {
                    panic!("unexpected canceled request")
                })
                .is_err()
        );
    }
}
