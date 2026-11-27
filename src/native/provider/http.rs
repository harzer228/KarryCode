use super::super::{Provider, Reasoning, ReasoningApi, models::Protocol, subagents};
use super::request::{RequestPlan, apply_reasoning, constrain_request, output_field};
use super::stream::{Delta, InterruptedStream, Reply, decode_stream_mode};
use anyhow::{Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    io::{BufReader, Read},
    sync::{atomic::AtomicBool, mpsc},
    time::Duration,
};

pub(in crate::native) fn http_error_text(status: u16, body: &str, key: &str) -> String {
    let mut detail = String::new();
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        for candidate in [
            value.get("error").and_then(|error| error.get("message")),
            value.get("message"),
            value.get("error"),
            value.get("detail"),
        ]
        .into_iter()
        .flatten()
        {
            if let Some(text) = candidate.as_str() {
                detail = text.to_owned();
                break;
            }
        }
    }
    if detail.is_empty() {
        detail = body.to_owned();
    }
    let mut detail: String = detail
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
        .take(1200)
        .collect();
    detail = detail.trim().to_owned();
    if !key.is_empty() {
        detail = detail.replace(key, "<key>");
    }
    if detail.is_empty() {
        format!("Provider HTTP {status}")
    } else {
        format!("Provider HTTP {status}: {detail}")
    }
}

pub(in crate::native) fn provider_transport_error(error: ureq::Error, key: &str) -> anyhow::Error {
    match error {
        ureq::Error::Status(status, response) => {
            let mut body = String::new();
            let _ = response
                .into_reader()
                .take(8 * 1024)
                .read_to_string(&mut body);
            let detail = http_error_text(status, &body, key);
            if let Some(error) = subagents::ConcurrencyError::parse(&body, detail.clone()) {
                return error.into();
            }
            anyhow::anyhow!("{detail}")
        }
        _ => anyhow::anyhow!(tr!(
            "Не удалось подключиться к провайдеру. Проверь интернет и адрес.",
            "Provider connection failed. Check network and endpoint."
        )),
    }
}

pub(super) fn provider_stream_closed(detail: &str) -> bool {
    let detail = detail.to_ascii_lowercase();
    [
        "unexpected eof",
        "eof while parsing",
        "tls close_notify",
        "connection reset",
    ]
    .iter()
    .any(|reason| detail.contains(reason))
}

pub(in crate::native) fn request(
    provider: &Provider,
    plan: &RequestPlan,
    canceled: &AtomicBool,
    sender: mpsc::Sender<Delta>,
) -> Result<Reply> {
    request_with_timeout(provider, plan, canceled, sender, 120)
}

pub(in crate::native) fn request_with_timeout(
    provider: &Provider,
    plan: &RequestPlan,
    canceled: &AtomicBool,
    sender: mpsc::Sender<Delta>,
    read_timeout_seconds: u64,
) -> Result<Reply> {
    let agent = ureq::AgentBuilder::new()
        .redirects(0)
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(read_timeout_seconds))
        .timeout_write(Duration::from_secs(15))
        .build();
    let request = provider.connection.authorize(
        agent
            .post(&format!("{}/{}", provider.connection.url, plan.suffix))
            .set("Content-Type", "application/json"),
    );
    let response = request
        .send_string(&plan.body.to_string())
        .map_err(|error| {
            let interrupted = matches!(error, ureq::Error::Status(502..=504, _));
            let error = provider_transport_error(error, &provider.connection.key);
            if interrupted && !error.is::<subagents::ConcurrencyError>() {
                InterruptedStream.into()
            } else {
                error
            }
        })?;
    decode_stream_mode(
        BufReader::new(response.into_reader().take(16 * 1024 * 1024)),
        provider.anthropic(),
        provider.responses(),
        canceled,
        sender,
    )
    .map_err(|mut error| {
        let key = &provider.connection.key;
        if error.is::<InterruptedStream>() || key.is_empty() {
            error
        } else if let Some(concurrency) = error.downcast_mut::<subagents::ConcurrencyError>() {
            concurrency.redact(key);
            error
        } else {
            anyhow::anyhow!("{}", error.to_string().replace(key, "<key>"))
        }
    })
}

pub(in crate::native) fn probe_payload(provider: &Provider) -> Value {
    let field = output_field(provider);
    let mut body = if provider.responses() {
        json!({"model":provider.model,"input":"Reply OK.","store":false})
    } else {
        json!({"model":provider.model,"messages":[{"role":"user","content":"ping"}]})
    };
    let limit = if provider.responses() { 64 } else { 16 };
    body[field] = json!(limit.min(provider.output_limit()));
    body
}

pub(in crate::native) struct Probe {
    pub(in crate::native) seconds: f32,
    pub(in crate::native) reasoning: bool,
}

pub(in crate::native) fn probe_provider(provider: &Provider) -> Result<Probe> {
    let started = std::time::Instant::now();
    let agent = ureq::AgentBuilder::new()
        .redirects(0)
        .timeout_connect(Duration::from_secs(8))
        .timeout_read(Duration::from_secs(20))
        .timeout_write(Duration::from_secs(8))
        .build();
    let suffix = if provider.responses() {
        "responses"
    } else if provider.anthropic() {
        "messages"
    } else {
        "chat/completions"
    };
    let request = provider.connection.authorize(
        agent
            .post(&format!("{}/{suffix}", provider.connection.url))
            .set("Content-Type", "application/json"),
    );
    let mut payload = probe_payload(provider);
    if !matches!(
        provider.reasoning,
        Reasoning::ProviderDefault | Reasoning::Off
    ) && provider.reasoning_api != ReasoningApi::None
    {
        let limit = if provider.anthropic()
            && !matches!(
                provider.reasoning_api,
                ReasoningApi::Adaptive | ReasoningApi::DeepSeek
            )
            && !provider.uses_deepseek_reasoning()
        {
            provider.reasoning.budget().unwrap_or(255) + 64
        } else {
            256
        };
        let limit_field = output_field(provider);
        let limit = limit.min(provider.output_limit());
        payload[limit_field] = json!(limit);
        if provider.responses() {
            payload["input"] = json!("Compute 21 times 17 and reply with just the number.");
        } else {
            payload["messages"][0]["content"] =
                json!("Compute 21 times 17 and reply with just the number.");
        }
    }
    apply_reasoning(provider, &mut payload)?;
    constrain_request(provider, &mut payload)?;
    let response = request
        .send_string(&payload.to_string())
        .map_err(|error| provider_transport_error(error, &provider.connection.key))?;
    let mut text = String::new();
    response
        .into_reader()
        .take(256 * 1024)
        .read_to_string(&mut text)?;
    let value: Value = serde_json::from_str(&text)?;
    if value["status"] == "failed" || !value["error"].is_null() {
        bail!("{}", http_error_text(200, &text, &provider.connection.key));
    }
    let result = match provider.connection.protocol {
        Protocol::ChatCompletions => "choices",
        Protocol::Responses => "output",
        Protocol::AnthropicMessages => "content",
    };
    ensure!(
        value[result].is_array(),
        tr!(
            "Неизвестный формат ответа на тест модели.",
            "Unrecognized model test response."
        )
    );
    let reasoning = [
        "/usage/completion_tokens_details/reasoning_tokens",
        "/usage/output_tokens_details/reasoning_tokens",
    ]
    .into_iter()
    .any(|path| {
        value
            .pointer(path)
            .and_then(Value::as_u64)
            .is_some_and(|n| n > 0)
    }) || value["output"]
        .as_array()
        .is_some_and(|items| items.iter().any(|item| item["type"] == "reasoning"))
        || value["content"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item["type"] == "thinking"))
        || ["reasoning_content", "reasoning", "thinking"]
            .into_iter()
            .any(|field| {
                value["choices"][0]["message"][field]
                    .as_str()
                    .is_some_and(|text| !text.is_empty())
            });
    Ok(Probe {
        seconds: started.elapsed().as_secs_f32(),
        reasoning,
    })
}
