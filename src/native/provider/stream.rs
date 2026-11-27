use super::super::{field, subagents, tools};
use super::http::provider_stream_closed;
use anyhow::{Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    io::BufRead,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

pub(in crate::native) enum Delta {
    Content(String),
    Reasoning(String),
}

#[derive(Debug)]
pub(in crate::native) struct Reply {
    pub(in crate::native) content: String,
    pub(in crate::native) reasoning: Option<String>,
    pub(in crate::native) raw_reasoning: String,
    pub(in crate::native) calls: Vec<ToolCall>,
    pub(in crate::native) wire_blocks: Vec<Value>,
    pub(in crate::native) usage: Usage,
}

#[derive(Debug, Default)]
pub(in crate::native) struct Usage {
    pub(in crate::native) input: Option<u32>,
    pub(in crate::native) output: Option<u32>,
}

impl Usage {
    fn update(&mut self, value: &Value) {
        let number = |name| value[name].as_u64().and_then(|n| u32::try_from(n).ok());
        self.input = number("input_tokens")
            .or_else(|| number("prompt_tokens"))
            .map(|tokens| {
                tokens
                    .saturating_add(number("cache_creation_input_tokens").unwrap_or_default())
                    .saturating_add(number("cache_read_input_tokens").unwrap_or_default())
            })
            .or(self.input);
        self.output = number("output_tokens")
            .or_else(|| number("completion_tokens"))
            .or(self.output);
    }
}

#[derive(Debug)]
pub(in crate::native) struct InterruptedStream;

impl std::fmt::Display for InterruptedStream {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str(tr!(
            "Поток ответа провайдера оборвался.",
            "The provider's response stream disconnected."
        ))
    }
}

impl std::error::Error for InterruptedStream {}

#[derive(Debug, Default)]
pub(in crate::native) struct ToolCall {
    pub(in crate::native) id: String,
    pub(in crate::native) name: String,
    pub(in crate::native) arguments: String,
}

fn push_reasoning(reasoning: &mut String, sender: &mpsc::Sender<Delta>, text: &str) {
    if text.is_empty() || reasoning.len() >= 32 * 1024 {
        return;
    }
    let mut part = String::new();
    for character in text.chars() {
        if reasoning.len() + part.len() + character.len_utf8() > 32 * 1024 {
            break;
        }
        part.push(character);
    }
    if part.is_empty() {
        return;
    }
    reasoning.push_str(&part);
    let _ = sender.send(Delta::Reasoning(part));
}

#[cfg(test)]
pub(in crate::native) fn decode_stream(
    reader: impl BufRead,
    anthropic: bool,
    canceled: &AtomicBool,
    sender: mpsc::Sender<Delta>,
) -> Result<Reply> {
    decode_stream_mode(reader, anthropic, false, canceled, sender)
}
pub(in crate::native) fn decode_stream_mode(
    mut reader: impl BufRead,
    anthropic: bool,
    responses: bool,
    canceled: &AtomicBool,
    sender: mpsc::Sender<Delta>,
) -> Result<Reply> {
    let mut output = String::new();
    let mut reasoning = String::new();
    let mut line = Vec::new();
    let started = std::time::Instant::now();
    let mut complete = false;
    let mut raw_reasoning = String::new();
    let mut tools = std::collections::BTreeMap::<u64, ToolCall>::new();
    let mut wire_blocks = std::collections::BTreeMap::<u64, Value>::new();
    let mut usage = Usage::default();
    loop {
        if canceled.load(Ordering::Relaxed) {
            bail!("Canceled");
        }
        ensure!(
            started.elapsed() < Duration::from_secs(300),
            "Provider response exceeded five minutes"
        );
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::UnexpectedEof
                        | std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::BrokenPipe
                ) =>
            {
                break;
            }
            Err(error) => return Err(error.into()),
        }
        let line = match std::str::from_utf8(&line) {
            Ok(line) => line,
            Err(error) if error.error_len().is_none() => break,
            Err(error) => {
                return Err(anyhow::Error::new(error).context(tr!(
                    "Некорректная кодировка UTF-8 в потоке провайдера",
                    "Invalid UTF-8 in provider stream"
                )));
            }
        };
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        if data.trim() == "[DONE]" {
            complete = true;
            break;
        }
        let event: Value = match serde_json::from_str(data.trim()) {
            Ok(event) => event,
            Err(error) if error.is_eof() || !line.ends_with('\n') => break,
            Err(error) => {
                return Err(anyhow::Error::new(error).context(tr!(
                    "Некорректное событие в потоке провайдера",
                    "Invalid provider stream event"
                )));
            }
        };
        usage.update(&event["usage"]);
        usage.update(&event["message"]["usage"]);
        usage.update(&event["response"]["usage"]);
        if let Some(error) = event.get("error").filter(|error| !error.is_null()) {
            let detail = error["message"]
                .as_str()
                .or(error.as_str())
                .unwrap_or("Provider returned a stream error");
            if provider_stream_closed(detail) {
                return Err(InterruptedStream.into());
            }
            let detail = detail.chars().take(1200).collect::<String>();
            if let Some(error) = subagents::ConcurrencyError::parse(data, detail.clone()) {
                return Err(error.into());
            }
            bail!("{detail}");
        }
        ensure!(event["type"] != "error", "Provider returned a stream error");
        ensure!(
            event["delta"]["stop_reason"] != "max_tokens"
                && event["choices"][0]["finish_reason"] != "length",
            tr!(
                "Достигнут лимит выхода модели. Увеличь «Выход» в /model или сократи задачу.",
                "Model output limit reached. Increase Output in /model or shorten the task."
            )
        );
        if event["type"] == "message_stop" {
            complete = true;
        }
        if responses {
            match event["type"].as_str() {
                Some("response.output_text.delta") => {
                    if let Some(part) = event["delta"].as_str() {
                        output.push_str(part);
                        let _ = sender.send(Delta::Content(part.to_owned()));
                    }
                }
                Some("response.reasoning_summary_text.delta") => {
                    if let Some(part) = event["delta"].as_str() {
                        push_reasoning(&mut reasoning, &sender, part);
                    }
                }
                Some("response.output_item.added" | "response.output_item.done")
                    if event["item"]["type"] == "function_call" =>
                {
                    let index = event["output_index"].as_u64().unwrap_or(0);
                    let call = tools.entry(index).or_default();
                    let item = &event["item"];
                    if let Some(id) = item["call_id"].as_str() {
                        call.id = id.to_owned();
                    }
                    if let Some(name) = item["name"].as_str() {
                        call.name = name.to_owned();
                    }
                    if let Some(arguments) =
                        item["arguments"].as_str().filter(|arg| !arg.is_empty())
                    {
                        call.arguments = arguments.to_owned();
                    }
                }
                Some(
                    "response.function_call_arguments.delta"
                    | "response.function_call_arguments.done",
                ) => {
                    let call = tools
                        .entry(event["output_index"].as_u64().unwrap_or(0))
                        .or_default();
                    if let Some(part) = event["delta"].as_str() {
                        call.arguments.push_str(part);
                    } else if let Some(arguments) = event["arguments"].as_str() {
                        call.arguments = arguments.to_owned();
                    }
                }
                Some("response.output_item.done") if event["item"]["type"] == "reasoning" => {
                    wire_blocks.insert(
                        event["output_index"].as_u64().unwrap_or(0),
                        event["item"].clone(),
                    );
                }
                Some("response.completed") => {
                    complete = true;
                    break;
                }
                Some("response.incomplete")
                    if event["response"]["incomplete_details"]["reason"] == "max_output_tokens" =>
                {
                    bail!(
                        "{}",
                        tr!(
                            "Достигнут лимит выхода модели. Увеличь «Выход» в /model или сократи задачу.",
                            "Model output limit reached. Increase Output in /model or shorten the task."
                        )
                    );
                }
                Some("response.failed" | "response.incomplete") => {
                    bail!(
                        "{}",
                        event["response"]["error"]["message"]
                            .as_str()
                            .unwrap_or("Provider did not complete the response")
                            .chars()
                            .take(1200)
                            .collect::<String>()
                    );
                }
                _ => {}
            }
        } else if anthropic {
            let index = event["index"].as_u64().unwrap_or(0);
            if event["type"] == "content_block_start" {
                let block = &event["content_block"];
                match block["type"].as_str() {
                    Some("tool_use") => {
                        let call = tools.entry(index).or_default();
                        call.id = field(block, "id").to_owned();
                        call.name = field(block, "name").to_owned();
                        if let Some(input) = block.get("input").filter(|input| {
                            input.as_object().is_some_and(|object| !object.is_empty())
                        }) {
                            call.arguments = input.to_string();
                        }
                    }
                    Some("thinking" | "redacted_thinking") => {
                        wire_blocks.insert(index, block.clone());
                    }
                    _ => {}
                }
            }
            let delta = &event["delta"];
            if let Some(part) = delta["partial_json"].as_str() {
                tools.entry(index).or_default().arguments.push_str(part);
            }
            match delta["type"].as_str() {
                Some("thinking_delta") => {
                    if let Some(part) = delta["thinking"].as_str() {
                        push_reasoning(&mut reasoning, &sender, part);
                        if let Some(block) = wire_blocks.get_mut(&index) {
                            block["thinking"] =
                                json!(format!("{}{part}", field(block, "thinking")));
                        }
                    }
                }
                Some("signature_delta") => {
                    if let Some(block) = wire_blocks.get_mut(&index) {
                        let previous = field(block, "signature");
                        block["signature"] =
                            json!(format!("{previous}{}", field(delta, "signature")));
                    }
                }
                Some("text_delta") => {
                    if let Some(part) = delta["text"].as_str() {
                        output.push_str(part);
                        let _ = sender.send(Delta::Content(part.to_owned()));
                    }
                }

                _ => {}
            }
        } else {
            if matches!(
                event["choices"][0]["finish_reason"].as_str(),
                Some("stop" | "tool_calls" | "function_call")
            ) {
                complete = true;
            }
            if let Some(calls) = event["choices"][0]["delta"]["tool_calls"].as_array() {
                for delta in calls {
                    let index = delta["index"].as_u64().unwrap_or(0);
                    let call = tools.entry(index).or_default();
                    if let Some(id) = delta["id"].as_str() {
                        call.id = id.to_owned();
                    }
                    if let Some(name) = delta["function"]["name"].as_str() {
                        call.name.push_str(name);
                    }
                    if let Some(part) = delta["function"]["arguments"].as_str() {
                        call.arguments.push_str(part);
                    }
                }
            }
            let delta = &event["choices"][0]["delta"];
            if let Some(part) = ["reasoning_content", "reasoning", "thinking"]
                .into_iter()
                .find_map(|field| delta[field].as_str())
            {
                raw_reasoning.push_str(part);
                push_reasoning(&mut reasoning, &sender, part);
            }
            if let Some(text) = delta["content"].as_str() {
                output.push_str(text);
                let _ = sender.send(Delta::Content(text.to_owned()));
            }
        }
    }
    if !complete || (output.is_empty() && reasoning.is_empty() && tools.is_empty()) {
        return Err(InterruptedStream.into());
    }
    ensure!(
        tools.len() <= 32,
        "Provider returned more than 32 tool calls"
    );
    let mut ids = std::collections::BTreeSet::new();
    for call in tools.values_mut() {
        ensure!(
            !call.id.is_empty() && ids.insert(call.id.clone()),
            "Provider returned invalid tool call identifiers"
        );
        let Some(action) = tools::decode(&call.name, &call.arguments) else {
            if serde_json::from_str::<Value>(&call.arguments).is_err_and(|error| error.is_eof()) {
                return Err(InterruptedStream.into());
            }
            bail!(tr!(
                "Провайдер вернул некорректные аргументы инструмента",
                "Provider returned invalid tool arguments"
            ));
        };
        call.arguments = action.to_string();
    }
    Ok(Reply {
        content: output,
        reasoning: (!reasoning.is_empty()).then_some(reasoning),
        raw_reasoning,
        calls: tools.into_values().collect(),
        wire_blocks: wire_blocks.into_values().collect(),
        usage,
    })
}
