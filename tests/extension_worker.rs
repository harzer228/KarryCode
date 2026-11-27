use serde_json::{Value, json};
use std::{
    io::Write as _,
    process::{Command, Stdio},
};

#[test]
fn worker_accepts_a_bounded_json_payload_and_returns_a_proposal() -> anyhow::Result<()> {
    let mut worker = Command::new(env!("CARGO_BIN_EXE_karrycode"))
        .arg("--karrycode-extension-worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let payload = json!({
        "script": r#"#{ text: files["hello.txt"], changes: [#{ path: "greeting.txt", content: arguments.value }] }"#,
        "files": { "hello.txt": "Привет" },
        "arguments": { "value": "Hello" }
    });
    worker
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(&payload)?)?;
    let result = worker.wait_with_output()?;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let result: Value = serde_json::from_slice(&result.stdout)?;
    assert_eq!(result["text"], "Привет");
    assert_eq!(
        result["changes"][0],
        json!({"path":"greeting.txt","content":"Hello"})
    );
    Ok(())
}
