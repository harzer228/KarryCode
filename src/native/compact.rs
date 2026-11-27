use super::{
    InputLimitExceeded, Message, OutputSpaceExceeded, Provider, RequestPlan, WireCalls,
    apply_reasoning, chat_error, constrain_request, parse_action, ui,
};
use anyhow::{Context, Result, ensure};
use serde_json::json;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc,
};

/// Automatic compaction starts once a request is estimated at this share of the
/// input limit. The estimate is bytes / 4, which is off by roughly ±15%, and the
/// next tool result and reply still need room, so waiting for the hard limit would
/// fail requests that the provider would also reject.
pub(super) const AUTO_PERCENT: u32 = 85;
/// After compaction the request should use at most about half of the input limit,
/// so a long task gets many more rounds before the next compaction.
const TARGET_PERCENT: u32 = 50;
/// Expected size of the summary in estimated tokens.
const SUMMARY_TOKENS: u32 = 4_000;
const MAX_TAIL_TOKENS: u32 = 64_000;
/// Tool output beyond this is shortened in the transcript sent for summarization.
const ENTRY_BYTES: usize = 12 * 1024;
const REQUEST_BYTES: usize = 16 * 1024;
pub(super) const HISTORY_BYTES: usize = 512 * 1024;

const INSTRUCTIONS: &str = super::prompts::COMPACTION;

const SUMMARY_HEADER: &str = "Summary of the earlier part of this conversation. KarryCode replaced the older messages with this summary to free context. Continue the work from here.";

#[derive(Debug)]
pub(super) struct HistoryTooLarge;

impl std::fmt::Display for HistoryTooLarge {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str(tr!(
            "Диалог превышает 512 КиБ. Введи /compact, начни /new или приложи меньшие фрагменты.",
            "Conversation exceeds 512 KiB. Use /compact, start /new or attach smaller excerpts."
        ))
    }
}

impl std::error::Error for HistoryTooLarge {}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Trigger {
    Manual,
    Auto,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Outcome {
    Compacted,
    Skipped,
    Canceled,
}

pub(super) fn over_threshold(tokens: u32, limit: u32) -> bool {
    u64::from(tokens) * 100 >= u64::from(limit) * u64::from(AUTO_PERCENT)
}

/// Whether a prepared request is close enough to the limit, or already rejected
/// for its size, that the history should be compacted before sending.
pub(super) fn needed(provider: &Provider, request: &Result<RequestPlan>) -> bool {
    match request {
        Ok(plan) => over_threshold(plan.input_tokens, provider.input_limit()),
        Err(error) => {
            error.is::<InputLimitExceeded>()
                || error.is::<OutputSpaceExceeded>()
                || error.is::<HistoryTooLarge>()
        }
    }
}

fn tool_result(message: &Message) -> bool {
    message.role == "user" && !message.summary && message.content.starts_with("Tool result")
}

fn prompt(message: &Message) -> bool {
    message.role == "user" && !message.summary && !tool_result(message)
}

fn tokens(bytes: usize) -> u32 {
    u32::try_from(bytes.div_ceil(4)).unwrap_or(u32::MAX)
}

/// A cut keeps `history[cut..]` verbatim. It is valid only between complete tool
/// rounds: a native tool turn starting at `start` owns `2 * calls` messages
/// (arguments and result for each call), and a narration belongs to the calls
/// that follow it.
fn safe_cut(history: &[Message], wire: &WireCalls, cut: usize) -> bool {
    if cut == 0 || cut > history.len() {
        return false;
    }
    if history.get(cut).is_some_and(tool_result) || history[cut - 1].role == "narration" {
        return false;
    }
    !wire
        .range(..cut)
        .any(|(&start, turn)| start + turn.calls.len() * 2 > cut)
}

fn weights(history: &[Message], wire: &WireCalls) -> Vec<u32> {
    let mut weights: Vec<u32> = history
        .iter()
        .map(|message| {
            if message.for_provider() {
                tokens(message.content.len())
            } else {
                0
            }
        })
        .collect();
    for (&index, turn) in wire {
        if let Some(weight) = weights.get_mut(index) {
            let blocks: usize = turn
                .wire_blocks
                .iter()
                .map(|block| block.to_string().len())
                .sum();
            *weight = weight.saturating_add(tokens(
                turn.content.len() + turn.raw_reasoning.len() + blocks,
            ));
        }
    }
    weights
}

/// How many estimated tokens of recent history to keep verbatim.
pub(super) fn tail_budget(input_limit: u32, overhead: u32, history_tokens: u32) -> u32 {
    let target = (u64::from(input_limit) * u64::from(TARGET_PERCENT) / 100) as u32;
    target
        .saturating_sub(overhead.saturating_add(SUMMARY_TOKENS))
        .min(history_tokens / 4)
        .min(MAX_TAIL_TOKENS)
}

/// Picks the earliest valid cut whose verbatim tail fits `tail_budget`, or the
/// latest valid cut when no tail fits. The summarized prefix must contain at
/// least `min_prefix` estimated tokens and something besides an earlier summary.
pub(super) fn choose_cut(
    history: &[Message],
    wire: &WireCalls,
    tail_budget: u32,
    min_prefix: u32,
) -> Option<usize> {
    let weights = weights(history, wire);
    let total = weights
        .iter()
        .fold(0u32, |sum, weight| sum.saturating_add(*weight));
    let mut prefix = 0u32;
    let mut fresh = false;
    let mut best = None;
    for cut in 1..=history.len() {
        let message = &history[cut - 1];
        prefix = prefix.saturating_add(weights[cut - 1]);
        fresh |= message.for_provider() && !message.summary;
        if !fresh || prefix < min_prefix.max(1) || !safe_cut(history, wire, cut) {
            continue;
        }
        best = Some(cut);
        if total - prefix <= tail_budget {
            break;
        }
    }
    best
}

fn floor_boundary(text: &str, mut index: usize) -> usize {
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn clip(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let head = floor_boundary(text, limit * 3 / 4);
    let mut tail = text.len() - limit / 4;
    while !text.is_char_boundary(tail) {
        tail += 1;
    }
    format!(
        "{}\n[… {} bytes omitted …]\n{}",
        &text[..head],
        tail - head,
        &text[tail..]
    )
}

fn transcript(history: &[Message]) -> Vec<String> {
    history
        .iter()
        .filter(|message| !message.content.trim().is_empty())
        .filter_map(|message| {
            if message.summary {
                return Some(format!("[Summary of an earlier part]\n{}", message.content));
            }
            let label = match message.role.as_str() {
                "user" if tool_result(message) => "Tool",
                "user" => "User",
                "assistant" if parse_action(&message.content).is_some() => "Assistant tool call",
                "assistant" | "narration" => "Assistant",
                "interrupted" => "Assistant (interrupted)",
                "error" => "Error",
                _ => return None,
            };
            Some(format!(
                "[{label}]\n{}",
                clip(&message.content, ENTRY_BYTES)
            ))
        })
        .collect()
}

fn parts(entries: Vec<String>, budget: usize) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    for entry in entries {
        let entry = clip(&entry, budget);
        if !current.is_empty() && current.len() + entry.len() + 2 > budget {
            parts.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push_str("\n\n");
        }
        current.push_str(&entry);
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

/// Bytes of transcript that fit in one summarization request next to the
/// instructions, the running summary and the reply.
fn part_bytes(provider: &Provider) -> Result<usize> {
    let reply = provider.output_limit().min(8_192);
    let cap = provider
        .input_limit()
        .min(provider.context_limit().saturating_sub(reply));
    let reserved = tokens(INSTRUCTIONS.len()) + (2 * SUMMARY_TOKENS).min(cap / 4) + 512;
    // A quarter of the room absorbs JSON escaping and the estimate's error.
    let room = (u64::from(cap) * 3 / 4) as u32;
    let room = room.saturating_sub(reserved);
    ensure!(
        room >= 1_024,
        tr!(
            "Окно контекста модели слишком мало для сжатия. Увеличь лимиты в /model или начни /new.",
            "The model's context window is too small to compact. Raise the limits in /model or start /new."
        )
    );
    Ok(room as usize * 4)
}

pub(super) fn summary_request(
    provider: &Provider,
    previous: Option<&str>,
    part: &str,
) -> Result<RequestPlan> {
    let mut text = String::new();
    if let Some(previous) = previous {
        text.push_str(&format!(
            "Summary so far:\n<summary>\n{previous}\n</summary>\n\nUpdate this summary with the next part of the conversation and return the full updated summary.\n\n"
        ));
    }
    text.push_str(&format!("<conversation>\n{part}\n</conversation>"));
    let user = json!({"role":"user","content":text});
    let (suffix, mut body) = if provider.responses() {
        (
            "responses",
            json!({"model":provider.model,"instructions":INSTRUCTIONS,"input":[user],"stream":true,"store":false}),
        )
    } else if provider.anthropic() {
        (
            "messages",
            json!({"model":provider.model,"system":INSTRUCTIONS,"messages":[user],"stream":true}),
        )
    } else {
        (
            "chat/completions",
            json!({"model":provider.model,"messages":[{"role":"system","content":INSTRUCTIONS},user],"stream":true}),
        )
    };
    apply_reasoning(provider, &mut body)?;
    let input_tokens = constrain_request(provider, &mut body)?;
    Ok(RequestPlan {
        suffix,
        body,
        input_tokens,
    })
}

/// Summarizes the transcript part by part, folding each part into the running
/// summary, so a prefix larger than one request still fits.
pub(super) fn summarize(
    provider: &Provider,
    entries: Vec<String>,
    mut send: impl FnMut(&RequestPlan) -> Result<String>,
) -> Result<String> {
    let budget = part_bytes(provider)?;
    let mut summary: Option<String> = None;
    for part in parts(entries, budget) {
        let plan = summary_request(provider, summary.as_deref(), &part)?;
        let text = send(&plan)?;
        let text = text.trim();
        ensure!(
            !text.is_empty(),
            tr!(
                "Модель вернула пустую сводку.",
                "The model returned an empty summary."
            )
        );
        summary = Some(text.to_owned());
    }
    summary.context(tr!("Нечего сжимать.", "Nothing to compact."))
}

pub(super) struct Plan {
    pub(super) cut: usize,
    entries: Vec<String>,
    latest_request: Option<String>,
}

pub(super) fn plan(
    provider: &Provider,
    history: &[Message],
    wire: &WireCalls,
    overhead: u32,
    trigger: Trigger,
) -> Option<Plan> {
    let total = weights(history, wire)
        .iter()
        .fold(0u32, |sum, weight| sum.saturating_add(*weight));
    let budget = tail_budget(provider.input_limit(), overhead, total);
    let min_prefix = match trigger {
        Trigger::Manual => 1,
        Trigger::Auto => SUMMARY_TOKENS,
    };
    let cut = choose_cut(history, wire, budget, min_prefix)?;
    let entries = transcript(&history[..cut]);
    if entries.is_empty() {
        return None;
    }
    // When the cut lands inside the current turn, the request that started it
    // would only survive as a paraphrase; keep its wording.
    let latest_request = if history[cut..].iter().any(prompt) {
        None
    } else {
        history[..cut]
            .iter()
            .rfind(|message| prompt(message))
            .map(|message| clip(&message.content, REQUEST_BYTES))
    };
    Some(Plan {
        cut,
        entries,
        latest_request,
    })
}

pub(super) fn summary_message(summary: &str, latest_request: Option<&str>) -> Message {
    let mut content = format!("{SUMMARY_HEADER}\n\n{summary}");
    if let Some(request) = latest_request {
        content.push_str("\n\nThe user's latest request, verbatim:\n");
        content.push_str(request);
    }
    Message {
        role: "user".into(),
        content,
        summary: true,
        ..Default::default()
    }
}

/// Replaces `history[..cut]` with `summary` and shifts native tool turns so they
/// still point at their messages.
pub(super) fn apply(
    history: &mut Vec<Message>,
    wire: &mut WireCalls,
    cut: usize,
    summary: Message,
) {
    let tail = history.split_off(cut);
    *history = vec![summary];
    history.extend(tail);
    *wire = std::mem::take(wire)
        .into_iter()
        .filter(|(index, _)| *index >= cut)
        .map(|(index, turn)| (index - cut + 1, turn))
        .collect();
}

pub(super) fn run(
    screen: &mut ui::Screen,
    provider: &Provider,
    history: &mut Vec<Message>,
    wire: &mut WireCalls,
    trigger: Trigger,
    estimate: impl Fn(&[Message], &WireCalls) -> Option<u32>,
) -> Result<Outcome> {
    let overhead = estimate(&[], &WireCalls::new()).unwrap_or_default();
    let Some(plan) = plan(provider, history, wire, overhead, trigger) else {
        if trigger == Trigger::Manual {
            screen.notice(tr!("Пока нечего сжимать.", "Nothing to compact yet."));
        }
        return Ok(Outcome::Skipped);
    };
    let before = estimate(history, wire);
    let cancel = AtomicBool::new(false);
    let label = match trigger {
        Trigger::Manual => tr!("Сжимаю контекст…", "Compacting context…"),
        Trigger::Auto => tr!(
            "Контекст почти заполнен, сжимаю…",
            "Context is nearly full, compacting…"
        ),
    };
    let summary = std::thread::scope(|scope| {
        let (cancel, entries) = (&cancel, plan.entries);
        let worker = scope.spawn(move || {
            summarize(provider, entries, |plan| {
                let (sender, _) = mpsc::channel();
                super::request(provider, plan, cancel, sender).map(|reply| reply.content)
            })
        });
        screen.wait_for_tool(label, cancel, worker)
    })?;
    if cancel.load(Ordering::Relaxed) {
        screen.notice(tr!("Сжатие отменено.", "Compaction canceled."));
        return Ok(Outcome::Canceled);
    }
    let summary = match summary {
        Ok(summary) => summary,
        Err(error) => {
            chat_error(
                history,
                tr!("Сжатие контекста", "Context compaction"),
                error,
            );
            screen.notice("");
            return Ok(Outcome::Skipped);
        }
    };
    let kept = history.len() - plan.cut;
    apply(
        history,
        wire,
        plan.cut,
        summary_message(&summary, plan.latest_request.as_deref()),
    );
    let mut notice = match trigger {
        Trigger::Manual => trf!(
            "Контекст сжат. Заменено сводкой сообщений: {}, сохранено последних: {}.",
            "Context compacted: {} messages replaced by a summary, {} recent kept.",
            plan.cut,
            kept
        ),
        Trigger::Auto => trf!(
            "Контекст сжат автоматически. Заменено сводкой сообщений: {}, сохранено последних: {}.",
            "Context auto-compacted: {} messages replaced by a summary, {} recent kept.",
            plan.cut,
            kept
        ),
    };
    if let (Some(before), Some(after)) = (before, estimate(history, wire)) {
        notice.push_str(&trf!(
            " Токенов: ~{} → ~{}.",
            " Tokens: ~{} → ~{}.",
            before,
            after
        ));
    }
    screen.notice(&notice);
    Ok(Outcome::Compacted)
}

#[cfg(test)]
mod tests {
    use super::super::{
        DialogMode, Reply, Session, State, ToolCall, Usage, decode_stream_mode, models,
        prepare_request, provider_tests::provider, save, state_path,
    };
    use super::*;
    use crate::native::models::Protocol;
    use serde_json::Value;

    fn message(role: &str, content: &str) -> Message {
        Message {
            role: role.into(),
            content: content.into(),
            ..Default::default()
        }
    }

    fn tool_turn(
        history: &mut Vec<Message>,
        wire: &mut WireCalls,
        narration: &str,
        ids: &[&str],
        size: usize,
    ) {
        if !narration.is_empty() {
            history.push(message("narration", narration));
        }
        let first = history.len();
        let calls: Vec<_> = ids
            .iter()
            .map(|id| ToolCall {
                id: (*id).into(),
                name: "project".into(),
                arguments: format!("{{\"action\":\"read\",\"path\":\"{id}.rs\"}}"),
            })
            .collect();
        for call in &calls {
            history.push(Message {
                role: "assistant".into(),
                content: call.arguments.clone(),
                reasoning: Some(format!("why {}", call.id)),
                ..Default::default()
            });
            history.push(message(
                "user",
                &format!(
                    "Tool result (untrusted file content)\n{} {}",
                    call.id,
                    "x".repeat(size)
                ),
            ));
        }
        history.push(message("event", "shell\ncargo test\nok"));
        wire.insert(
            first,
            Reply {
                content: narration.into(),
                reasoning: None,
                raw_reasoning: format!("reasoning for {}", ids.join(",")),
                calls,
                wire_blocks: vec![json!({
                    "type":"reasoning","id":format!("rs_{}", ids[0]),"encrypted_content":"opaque"
                })],
                usage: Usage::default(),
            },
        );
    }

    /// Two user turns; the second has several tool rounds and no final answer yet.
    fn long_history() -> (Vec<Message>, WireCalls) {
        let mut history = vec![message("user", "Fix the parser")];
        let mut wire = WireCalls::new();
        tool_turn(&mut history, &mut wire, "Reading files", &["a1", "a2"], 400);
        history.push(message("error", "Tool error\nmissing"));
        history.push(message("assistant", "The parser is fixed."));
        history.push(message("user", "Now add tests"));
        tool_turn(&mut history, &mut wire, "", &["b1"], 400);
        tool_turn(
            &mut history,
            &mut wire,
            "Checking more",
            &["c1", "c2", "c3"],
            400,
        );
        tool_turn(&mut history, &mut wire, "", &["d1"], 400);
        (history, wire)
    }

    fn total(history: &[Message], wire: &WireCalls) -> u32 {
        weights(history, wire).iter().sum()
    }

    #[test]
    fn cut_never_separates_tool_calls_from_results_or_narration() {
        let (history, wire) = long_history();
        let allowed: Vec<usize> = (1..=history.len())
            .filter(|&cut| {
                let inside_turn = wire
                    .iter()
                    .any(|(&start, turn)| start < cut && cut < start + turn.calls.len() * 2);
                !inside_turn
                    && !history.get(cut).is_some_and(tool_result)
                    && history[cut - 1].role != "narration"
            })
            .collect();
        assert!(allowed.len() > 3);
        for cut in 0..=history.len() + 1 {
            assert_eq!(safe_cut(&history, &wire, cut), allowed.contains(&cut));
        }
        let mut seen = Vec::new();
        for budget in 0..=total(&history, &wire) + 1 {
            let cut = choose_cut(&history, &wire, budget, 1).unwrap();
            assert!(allowed.contains(&cut), "budget {budget} chose {cut}");
            if !seen.contains(&cut) {
                seen.push(cut);
            }
        }
        assert!(seen.len() > 2);
        let cut = choose_cut(&history, &wire, 0, 1).unwrap();
        assert!(
            weights(&history, &wire)[cut..]
                .iter()
                .all(|weight| *weight == 0)
        );

        // Saved sessions have no wire turns: the tool result prefix still holds pairs.
        let empty = WireCalls::new();
        for budget in 0..=total(&history, &empty) {
            let cut = choose_cut(&history, &empty, budget, 1).unwrap();
            assert!(!history.get(cut).is_some_and(tool_result));
        }
    }

    #[test]
    fn earlier_summary_alone_is_not_compacted_again() {
        let history = vec![
            summary_message("old summary", None),
            message("assistant", "Done"),
        ];
        assert_eq!(choose_cut(&history, &WireCalls::new(), 0, 1), Some(2));
        let history = vec![summary_message("old summary", None)];
        assert_eq!(choose_cut(&history, &WireCalls::new(), 0, 1), None);
        assert_eq!(choose_cut(&[], &WireCalls::new(), 0, 1), None);
    }

    fn assert_valid(protocol: Protocol, body: &Value) {
        match protocol {
            Protocol::ChatCompletions => {
                let messages = body["messages"].as_array().unwrap();
                assert_eq!(messages[0]["role"], "system");
                let mut pending: Vec<String> = Vec::new();
                for message in &messages[1..] {
                    if message["role"] == "tool" {
                        let id = message["tool_call_id"].as_str().unwrap();
                        assert_eq!(pending.first().map(String::as_str), Some(id));
                        pending.remove(0);
                        continue;
                    }
                    assert!(pending.is_empty(), "tool results missing: {pending:?}");
                    if let Some(calls) = message["tool_calls"].as_array() {
                        assert_eq!(message["role"], "assistant");
                        pending = calls
                            .iter()
                            .map(|call| call["id"].as_str().unwrap().to_owned())
                            .collect();
                    }
                }
                assert!(pending.is_empty());
            }
            Protocol::Responses => {
                let input = body["input"].as_array().unwrap();
                let mut open: Vec<String> = Vec::new();
                for item in input {
                    match item["type"].as_str() {
                        Some("function_call") => {
                            open.push(item["call_id"].as_str().unwrap().to_owned())
                        }
                        Some("function_call_output") => {
                            let id = item["call_id"].as_str().unwrap();
                            let position = open.iter().position(|open| open == id);
                            open.remove(position.expect("output without a call"));
                        }
                        _ => assert!(open.is_empty(), "call without output: {open:?}"),
                    }
                }
                assert!(open.is_empty());
            }
            Protocol::AnthropicMessages => {
                let messages = body["messages"].as_array().unwrap();
                assert_eq!(messages[0]["role"], "user");
                let ids = |message: &Value, kind: &str, key: &str| -> Vec<String> {
                    message["content"]
                        .as_array()
                        .map(|blocks| {
                            blocks
                                .iter()
                                .filter(|block| block["type"] == kind)
                                .map(|block| block[key].as_str().unwrap().to_owned())
                                .collect()
                        })
                        .unwrap_or_default()
                };
                for (index, message) in messages.iter().enumerate() {
                    let results = ids(message, "tool_result", "tool_use_id");
                    if !results.is_empty() {
                        assert_eq!(message["role"], "user");
                        assert_eq!(ids(&messages[index - 1], "tool_use", "id"), results);
                    }
                    let uses = ids(message, "tool_use", "id");
                    if !uses.is_empty() {
                        assert_eq!(message["role"], "assistant");
                        assert_eq!(
                            ids(&messages[index + 1], "tool_result", "tool_use_id"),
                            uses
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn compacted_history_builds_valid_requests_for_every_protocol() -> Result<()> {
        let (history, wire) = long_history();
        let cuts: Vec<usize> = (1..=history.len())
            .filter(|&cut| safe_cut(&history, &wire, cut))
            .collect();
        for protocol in [
            Protocol::ChatCompletions,
            Protocol::Responses,
            Protocol::AnthropicMessages,
        ] {
            for &cut in &cuts {
                let mut provider = provider("test", "", 128_000);
                provider.connection.protocol = protocol;
                let (mut compacted, mut kept) = (history.clone(), wire.clone_turns());
                apply(
                    &mut compacted,
                    &mut kept,
                    cut,
                    summary_message("Goal: fix the parser.", Some("Now add tests")),
                );
                assert_eq!(compacted.len(), history.len() - cut + 1);
                for (index, turn) in &kept {
                    let original = &wire[&(index + cut - 1)];
                    assert_eq!(turn.calls[0].id, original.calls[0].id);
                    assert_eq!(compacted[*index].content, turn.calls[0].arguments);
                }
                let plan = prepare_request(&provider, &compacted, false, "", &kept)?;
                assert_valid(protocol, &plan.body);
                let text = plan.body.to_string();
                assert!(text.contains("Goal: fix the parser."));
                for (start, turn) in &wire {
                    for call in &turn.calls {
                        let quoted = format!("\"{}\"", call.id);
                        assert_eq!(text.contains(&quoted), *start >= cut, "{protocol:?} {cut}");
                    }
                }
                if protocol == Protocol::Responses {
                    let input = plan.body["input"].as_array().unwrap();
                    for (start, turn) in &wire {
                        if *start < cut {
                            continue;
                        }
                        let reasoning = input
                            .iter()
                            .position(|item| item["id"] == format!("rs_{}", turn.calls[0].id));
                        let call = input
                            .iter()
                            .position(|item| item["call_id"] == turn.calls[0].id.as_str());
                        assert!(reasoning.unwrap() < call.unwrap());
                    }
                }
            }
        }

        let mut provider = provider("Demo", "", 128_000);
        provider.reasoning_api = crate::native::ReasoningApi::DeepSeek;
        let (mut compacted, mut kept) = (history.clone(), wire.clone_turns());
        apply(
            &mut compacted,
            &mut kept,
            cuts[1],
            summary_message("S", None),
        );
        let plan = prepare_request(&provider, &compacted, false, "", &kept)?;
        assert_valid(Protocol::ChatCompletions, &plan.body);
        let messages = plan.body["messages"].as_array().unwrap();
        assert!(messages.iter().any(|message| {
            message["tool_calls"].is_array()
                && message["reasoning_content"]
                    .as_str()
                    .is_some_and(|text| text.starts_with("reasoning for"))
        }));
        Ok(())
    }

    trait CloneTurns {
        fn clone_turns(&self) -> WireCalls;
    }

    impl CloneTurns for WireCalls {
        fn clone_turns(&self) -> WireCalls {
            self.iter()
                .map(|(index, turn)| {
                    let calls = turn
                        .calls
                        .iter()
                        .map(|call| ToolCall {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            arguments: call.arguments.clone(),
                        })
                        .collect();
                    let reply = Reply {
                        content: turn.content.clone(),
                        reasoning: turn.reasoning.clone(),
                        raw_reasoning: turn.raw_reasoning.clone(),
                        calls,
                        wire_blocks: turn.wire_blocks.clone(),
                        usage: Usage::default(),
                    };
                    (*index, reply)
                })
                .collect()
        }
    }

    #[test]
    fn auto_compaction_threshold() -> Result<()> {
        assert!(!over_threshold(84, 100));
        assert!(over_threshold(85, 100));
        assert!(over_threshold(u32::MAX, u32::MAX));
        assert!(!over_threshold(0, 1));

        let mut provider = provider("test", "", 20_000);
        let small = prepare_request(
            &provider,
            &[message("user", "hi")],
            false,
            "",
            &WireCalls::new(),
        );
        assert!(!needed(&provider, &small));
        let near = prepare_request(
            &provider,
            &[message("user", &"x".repeat(64_000))],
            false,
            "",
            &WireCalls::new(),
        );
        let tokens = near.as_ref().unwrap().input_tokens;
        assert!(tokens > 17_000 && tokens <= 20_000, "{tokens}");
        assert!(needed(&provider, &near));
        provider.input = Some(10_000);
        let over = prepare_request(
            &provider,
            &[message("user", &"x".repeat(64_000))],
            false,
            "",
            &WireCalls::new(),
        );
        assert!(over.as_ref().unwrap_err().is::<InputLimitExceeded>());
        assert!(needed(&provider, &over));
        provider.context = 1_000_000;
        provider.input = None;
        let huge = prepare_request(
            &provider,
            &[message("user", &"x".repeat(HISTORY_BYTES + 1))],
            false,
            "",
            &WireCalls::new(),
        );
        assert!(huge.as_ref().unwrap_err().is::<HistoryTooLarge>());
        assert!(needed(&provider, &huge));
        assert!(!needed(&provider, &Err(anyhow::anyhow!("unrelated"))));

        assert_eq!(tail_budget(100_000, 10_000, 1_000_000), 36_000);
        assert_eq!(tail_budget(100_000, 10_000, 40_000), 10_000);
        assert_eq!(tail_budget(8_000, 5_000, 1_000_000), 0);
        assert_eq!(tail_budget(1_000_000, 0, u32::MAX), MAX_TAIL_TOKENS);

        // A tiny prefix cannot free room, so automatic compaction skips it.
        let short = vec![message("user", "hi"), message("assistant", "hello")];
        let provider = provider_for(Protocol::ChatCompletions, 128_000);
        assert!(plan(&provider, &short, &WireCalls::new(), 3_000, Trigger::Auto).is_none());
        assert!(plan(&provider, &short, &WireCalls::new(), 3_000, Trigger::Manual).is_some());
        Ok(())
    }

    fn provider_for(protocol: Protocol, context: u32) -> Provider {
        let mut provider = provider("test", "", context);
        provider.connection.protocol = protocol;
        provider
    }

    fn fake_stream(protocol: Protocol, text: &str) -> String {
        let events = match protocol {
            Protocol::ChatCompletions => vec![
                json!({"choices":[{"delta":{"content":text}}]}).to_string(),
                "[DONE]".into(),
            ],
            Protocol::Responses => vec![
                json!({"type":"response.output_text.delta","delta":text}).to_string(),
                json!({"type":"response.completed"}).to_string(),
            ],
            Protocol::AnthropicMessages => vec![
                json!({"type":"content_block_delta","delta":{"type":"text_delta","text":text}})
                    .to_string(),
                json!({"type":"message_stop"}).to_string(),
            ],
        };
        events
            .iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect()
    }

    #[test]
    fn summary_requests_use_each_protocol_and_fold_parts() -> Result<()> {
        let mut history = vec![message("user", "Fix the parser")];
        let mut wire = WireCalls::new();
        for id in ["a", "b", "c", "d", "e", "f"] {
            tool_turn(&mut history, &mut wire, "", &[id], 20_000);
        }
        for protocol in [
            Protocol::ChatCompletions,
            Protocol::Responses,
            Protocol::AnthropicMessages,
        ] {
            // A small window forces several summarization requests.
            let provider = provider_for(protocol, 32_000);
            let plan = plan(&provider, &history, &wire, 1_000, Trigger::Manual).unwrap();
            let mut bodies = Vec::new();
            let summary = summarize(&provider, plan.entries, |request| {
                assert!(request.body.get("tools").is_none());
                let (system, conversation) = match protocol {
                    Protocol::ChatCompletions => {
                        assert_eq!(request.suffix, "chat/completions");
                        let messages = &request.body["messages"];
                        (
                            messages[0]["content"].clone(),
                            messages[1]["content"].clone(),
                        )
                    }
                    Protocol::Responses => {
                        assert_eq!(request.suffix, "responses");
                        (
                            request.body["instructions"].clone(),
                            request.body["input"][0]["content"].clone(),
                        )
                    }
                    Protocol::AnthropicMessages => {
                        assert_eq!(request.suffix, "messages");
                        (
                            request.body["system"].clone(),
                            request.body["messages"][0]["content"].clone(),
                        )
                    }
                };
                assert!(system.as_str().unwrap().contains("unresolved failures"));
                let conversation = conversation.as_str().unwrap().to_owned();
                assert!(conversation.contains("<conversation>"));
                assert!(!conversation.contains("why a"));
                assert!(conversation.contains("bytes omitted"));
                if let Some(previous) = bodies.last() {
                    assert!(conversation.contains(&format!("<summary>\nsummary {previous}\n")));
                }
                bodies.push(bodies.len());
                let (sender, _) = mpsc::channel();
                decode_stream_mode(
                    std::io::Cursor::new(fake_stream(
                        protocol,
                        &format!("summary {}", bodies.len() - 1),
                    )),
                    protocol == Protocol::AnthropicMessages,
                    protocol == Protocol::Responses,
                    &AtomicBool::new(false),
                    sender,
                )
                .map(|reply| reply.content)
            })?;
            assert!(bodies.len() > 1, "{protocol:?}");
            assert_eq!(summary, format!("summary {}", bodies.len() - 1));
        }

        let provider = provider_for(Protocol::ChatCompletions, 128_000);
        let entries = vec!["[User]\nhello".to_owned()];
        let error = summarize(&provider, entries.clone(), |_| Ok("  ".into())).unwrap_err();
        assert!(error.to_string().contains("summary") || error.to_string().contains("сводк"));
        let tiny = provider_for(Protocol::ChatCompletions, 2_000);
        assert!(summarize(&tiny, entries, |_| Ok("x".into())).is_err());
        Ok(())
    }

    #[test]
    fn mid_turn_cut_keeps_the_request_wording() {
        let (history, wire) = long_history();
        let provider = provider_for(Protocol::ChatCompletions, 128_000);
        let cut = (1..=history.len())
            .rev()
            .find(|&cut| {
                safe_cut(&history, &wire, cut)
                    && history
                        .get(cut)
                        .is_some_and(|next| next.role == "narration")
            })
            .unwrap();
        assert!(cut > 8);
        let request = history[..cut]
            .iter()
            .rfind(|message| prompt(message))
            .unwrap();
        assert_eq!(request.content, "Now add tests");
        let message = summary_message("S", Some(&request.content));
        assert!(message.content.ends_with("verbatim:\nNow add tests"));
        assert!(message.summary && message.for_provider());
        let plan = plan(&provider, &history, &wire, 0, Trigger::Manual).unwrap();
        let tail_has_prompt = history[plan.cut..].iter().any(prompt);
        assert_eq!(plan.latest_request.is_some(), !tail_has_prompt);
        let clipped = clip(&"ж".repeat(10_000), 1_001);
        assert!(clipped.len() < 1_100 && clipped.contains("bytes omitted"));
    }

    #[test]
    fn compacted_session_roundtrips_through_saved_state() -> Result<()> {
        let (mut history, mut wire) = long_history();
        let cut = (1..history.len())
            .find(|&cut| safe_cut(&history, &wire, cut) && prompt(&history[cut]))
            .unwrap();
        apply(
            &mut history,
            &mut wire,
            cut,
            summary_message("Goal: parser.", None),
        );
        let work = tempfile::tempdir()?;
        let mut state = models::load_state(
            br#"{"provider":{"anthropic":false,"url":"https://example.org/v1","model":"test","key":"k"},"sessions":[]}"#.as_slice(),
        )?;
        state.sessions.push(Session {
            project: "demo".into(),
            portable: false,
            messages: history.clone(),
            mode: DialogMode::Agent,
            ..Default::default()
        });
        save(work.path(), &state)?;
        let saved = std::fs::read_to_string(state_path(work.path()))?;
        assert_eq!(saved.matches("\"summary\":true").count(), 1);
        assert!(!saved.contains("\"summary\":false"));
        let restored: State = models::load_state(saved.as_bytes())?;
        let messages = &restored.sessions[0].messages;
        assert_eq!(messages.len(), history.len());
        assert!(messages[0].summary && !messages[1].summary);
        assert_eq!(messages[0].content, history[0].content);
        let provider = restored.active_provider(DialogMode::Agent).unwrap();
        let plan = prepare_request(&provider, messages, false, "", &WireCalls::new())?;
        let sent = plan.body["messages"].as_array().unwrap();
        assert_eq!(sent[1]["role"], "user");
        assert!(
            sent[1]["content"]
                .as_str()
                .unwrap()
                .contains("Goal: parser.")
        );
        Ok(())
    }
}
