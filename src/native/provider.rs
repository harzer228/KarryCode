mod http;
mod request;
mod stream;

pub(super) use http::{probe_provider, provider_transport_error, request, request_with_timeout};
pub(super) use request::{
    InputLimitExceeded, OutputSpaceExceeded, RequestMode, RequestPlan, Visual, WireCalls,
    apply_reasoning, attach_image, available_output, constrain_request, input_estimate,
    prepare_work_request,
};
pub(super) use stream::{Delta, InterruptedStream};
#[cfg(test)]
pub(super) use {
    request::prepare_request,
    stream::{Reply, ToolCall, Usage, decode_stream_mode},
};

#[cfg(test)]
pub(super) mod tests {
    use super::super::*;
    use super::{http::*, request::*, stream::*};
    use std::io::BufReader;
    pub(in crate::native) fn provider(name: &str, key: &str, context: u32) -> Provider {
        Provider {
            connection: Connection {
                name: name.into(),
                url: "https://example.org/v1".into(),
                key: key.into(),
                ..Connection::default()
            },
            chat_only: false,
            vision: false,
            model: "test".into(),
            context,
            input: None,
            output: None,
            reasoning: Reasoning::Off,
            reasoning_api: ReasoningApi::Auto,
            reasoning_levels: None,
        }
    }

    fn decode(stream: &str, anthropic: bool) -> Result<Reply> {
        let (sender, _) = mpsc::channel::<Delta>();
        decode_stream(
            std::io::Cursor::new(stream),
            anthropic,
            &AtomicBool::new(false),
            sender,
        )
    }
    #[test]
    fn openai_stream_handles_text_and_rejects_disconnect() {
        let stream =
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\ndata: [DONE]\n\n";
        assert_eq!(decode(stream, false).unwrap().content, "Hello");
        assert!(decode(stream.split("data: [DONE]").next().unwrap(), false).is_err());
    }

    #[test]
    fn a_cut_event_or_reset_is_retryable_but_never_returns_partial_tool_calls() {
        struct Reset;
        impl Read for Reset {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof))
            }
        }
        let tool = json!({"choices":[{"delta":{"tool_calls":[{
            "index":0,"id":"one","function":{"name":"project","arguments":"{\"action\":\"create_file\""}
        }]}}]});
        let part = format!("data: {tool}\n\n");
        for tail in [
            "data: {\"choices\":[{\"delta\":{\"content\":\"cut",
            "data: [DONE]\n",
        ] {
            assert!(
                decode(&format!("{part}{tail}"), false)
                    .unwrap_err()
                    .is::<InterruptedStream>()
            );
        }
        let reader = BufReader::new(std::io::Cursor::new(part).chain(Reset));
        let (sender, _) = mpsc::channel();
        assert!(
            decode_stream(reader, false, &AtomicBool::new(false), sender)
                .unwrap_err()
                .is::<InterruptedStream>()
        );
    }

    #[test]
    fn explicit_stream_finish_preserves_usage_without_a_done_marker() -> Result<()> {
        let stream = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Finished\"},\"finish_reason\":\"stop\"}]}\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":120,\"completion_tokens\":13}}\n"
        );
        let reply = decode(stream, false)?;
        assert_eq!(reply.content, "Finished");
        assert_eq!(reply.usage.input, Some(120));
        assert_eq!(reply.usage.output, Some(13));
        Ok(())
    }

    #[test]
    fn tool_batches_keep_results_and_reasoning_with_the_correct_response() -> Result<()> {
        let messages = [
            json!({"choices":[{"delta":{"reasoning_content":"reason","tool_calls":[
                {"index":1,"id":"second","function":{"name":"project","arguments":"{\"action\":\"stat\""}},
                {"index":0,"id":"first","function":{"name":"read","arguments":"{\"offset\":0"}}
            ]}}]}),
            json!({"choices":[{"delta":{"reasoning_content":"ing","tool_calls":[
                {"index":0,"function":{"arguments":",\"path\":\"README.md\"}"}},
                {"index":1,"function":{"arguments":",\"path\":\"Cargo.toml\"}"}}
            ]}}]}),
        ];
        let stream = messages
            .iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect::<String>()
            + "data: [DONE]\n\n";
        let reply = decode(&stream, false)?;
        assert_eq!(reply.calls[0].id, "first");
        assert_eq!(reply.calls[1].id, "second");
        let mut history = Vec::new();
        for call in &reply.calls {
            history.push(Message {
                role: "assistant".into(),
                content: call.arguments.clone(),
                reasoning: None,
                ..Default::default()
            });
            history.push(Message {
                role: "user".into(),
                content: format!("Result for {}", call.id),
                reasoning: None,
                ..Default::default()
            });
        }
        chat_error(&mut history, "Local error", "not sent");
        let wire = WireCalls::from([(0, reply)]);
        let mut provider = provider("Demo", "", default_context());
        provider.reasoning_api = ReasoningApi::DeepSeek;
        let plan = prepare_request(&provider, &history, false, "", &wire)?;
        let messages = plan.body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[1]["reasoning_content"], "reasoning");
        assert_eq!(messages[1]["tool_calls"][1]["id"], "second");
        assert_eq!(messages[1]["tool_calls"][0]["function"]["name"], "read");
        assert_eq!(messages[1]["tool_calls"][1]["function"]["name"], "stat");
        assert_eq!(
            messages[1]["tool_calls"][0]["function"]["arguments"],
            r#"{"offset":0,"path":"README.md"}"#
        );
        assert_eq!(messages[2]["tool_call_id"], "first");
        assert_eq!(messages[3]["content"], "Result for second");

        provider.chat_only = true;
        let plan = prepare_request(&provider, &history, false, "", &wire)?;
        assert!(plan.body.get("tools").is_none());
        let messages = plan.body["messages"].as_array().unwrap();
        assert!(messages.iter().all(|message| {
            message.get("tool_calls").is_none()
                && message.get("tool_call_id").is_none()
                && message["role"] != "tool"
        }));
        Ok(())
    }

    #[test]
    fn anthropic_stream_assembles_fragmented_tools() {
        let mut stream = String::new();
        for event in [
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","name":"read","id":"use_read","input":{}}}),
            json!({"type":"content_block_delta","index":0,"delta":{"partial_json":"{\"path\":"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"partial_json":"\"README.md\"}"}}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","name":"git_status","id":"use_status","input":{}}}),
            json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","name":"project","id":"use_old"}}),
            json!({"type":"content_block_delta","index":2,"delta":{"partial_json":"{\"action\":\"stat\","}}),
            json!({"type":"content_block_delta","index":2,"delta":{"partial_json":"\"path\":\"src\"}"}}),
            json!({"type":"message_stop"}),
        ] {
            stream.push_str(&format!("data: {event}\n\n"));
        }
        let calls = decode(&stream, true).unwrap().calls;
        let actions: Vec<_> = calls
            .iter()
            .map(|call| parse_action(&call.arguments).unwrap())
            .collect();
        assert_eq!(actions[0], json!({"action":"read","path":"README.md"}));
        assert_eq!(actions[1], json!({"action":"git_status"}));
        assert_eq!(actions[2], json!({"action":"stat","path":"src"}));
    }

    fn native_provider(protocol: Protocol) -> Provider {
        let mut provider = provider("m", "", default_context());
        provider.connection.protocol = protocol;
        provider
    }

    const PROTOCOLS: [Protocol; 3] = [
        Protocol::ChatCompletions,
        Protocol::Responses,
        Protocol::AnthropicMessages,
    ];

    fn tool_names(body: &Value) -> Vec<String> {
        body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| {
                tool.get("function")
                    .unwrap_or(tool)
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap()
                    .to_owned()
            })
            .collect()
    }

    #[test]
    fn every_protocol_exposes_one_tool_per_action_for_each_mode() -> Result<()> {
        let base = [
            "read",
            "list",
            "search",
            "glob",
            "stat",
            "preview",
            "inspect_project",
            "git_status",
            "git_diff",
            "plan",
            "skill",
            "ask_user",
            "web_search",
            "web_fetch",
            "browser",
        ];
        let writes = [
            "edit",
            "create_file",
            "apply_changes",
            "request_permissions",
            "run_task",
            "shell",
            "mcp",
        ];
        let modes = [
            (
                RequestMode {
                    desktop: false,
                    ..Default::default()
                },
                [&base[..], &writes, &["delegate"]].concat(),
            ),
            (
                RequestMode {
                    desktop: true,
                    ..Default::default()
                },
                [
                    &base[..],
                    &["computer_observe", "computer_list", "computer_input"],
                    &writes,
                    &["delegate"],
                ]
                .concat(),
            ),
            (
                RequestMode {
                    desktop: false,
                    delegate_limit: 0,
                    ..Default::default()
                },
                [&base[..], &writes].concat(),
            ),
            (
                RequestMode {
                    diagnose: true,
                    desktop: false,
                    ..Default::default()
                },
                base.to_vec(),
            ),
            (
                RequestMode {
                    diagnose: true,
                    desktop: true,
                    ..Default::default()
                },
                [&base[..], &["computer_observe", "computer_list"]].concat(),
            ),
            (
                RequestMode {
                    planning: true,
                    ..Default::default()
                },
                permissions::PLAN_ACTIONS.to_vec(),
            ),
            (
                RequestMode {
                    worker: true,
                    ..Default::default()
                },
                permissions::WORKER_ACTIONS.to_vec(),
            ),
        ];
        for protocol in PROTOCOLS {
            let provider = native_provider(protocol);
            for (mode, expected) in &modes {
                let plan = prepare_work_request(&provider, &[], *mode, "", &WireCalls::new())?;
                assert_eq!(tool_names(&plan.body), *expected, "{protocol:?}");
                for tool in plan.body["tools"].as_array().unwrap() {
                    let schema = match protocol {
                        Protocol::AnthropicMessages => &tool["input_schema"],
                        Protocol::Responses => &tool["parameters"],
                        _ => &tool["function"]["parameters"],
                    };
                    assert_eq!(schema["type"], "object");
                    assert!(schema["properties"].get("action").is_none());
                }
            }
        }
        Ok(())
    }

    #[test]
    fn per_action_calls_decode_to_the_same_actions_as_project_calls() {
        for old in [
            r#"{"action":"read","path":"README.md","offset":10,"limit":20}"#,
            r#"{"action":"search","query":"fn main","regex":true,"root":"/srv/x"}"#,
            r#"{"action":"git_status"}"#,
            r#"{"action":"browser","operation":"click","node":7}"#,
            r#"{"action":"edit","edit":{"path":"a.rs","reason":"r","before":"x","after":"y"}}"#,
            r#"{"action":"apply_changes","changes":[{"kind":"delete","path":"tmp"}]}"#,
            r#"{"action":"run_task","task":"cargo-test"}"#,
            r#"{"action":"run","task":"cargo-test"}"#,
            r#"{"action":"mcp","server":"s","tool":"t","arguments":{"a":1}}"#,
            r#"{"action":"delegate","tasks":[{"role":"researcher","task":"Find"}]}"#,
            r#"{"action":"computer_input","observation":3,"operation":"key","key":"Enter"}"#,
        ] {
            let expected = parse_action(old).unwrap();
            let (name, input) = tools::replay(tools::LEGACY_TOOL, old);
            let name = if expected["action"] == "run" {
                "run"
            } else {
                name.as_str()
            };
            assert_eq!(
                tools::decode(name, &input.to_string()).unwrap(),
                expected,
                "{old}"
            );
            assert_eq!(tools::decode(tools::LEGACY_TOOL, old).unwrap(), expected);
        }
    }

    #[test]
    fn legacy_project_calls_replay_as_valid_per_action_calls() -> Result<()> {
        let old = [
            r#"{"action":"read","path":"README.md"}"#,
            r#"{"action":"edit","edit":{"path":"a.rs","reason":"r","before":"x","after":"y"}}"#,
            r#"{"action":"run","task":"cargo-test"}"#,
        ];
        let calls: Vec<_> = old
            .iter()
            .enumerate()
            .map(|(index, arguments)| ToolCall {
                id: format!("call_{index}"),
                name: "project".into(),
                arguments: (*arguments).into(),
            })
            .collect();
        let mut history = vec![message("Fix it")];
        for call in &calls {
            history.push(Message {
                role: "assistant".into(),
                content: call.arguments.clone(),
                ..Default::default()
            });
            history.push(message(&format!("Tool result\n{}", call.id)));
        }
        let wire = WireCalls::from([(
            1,
            Reply {
                content: String::new(),
                reasoning: None,
                raw_reasoning: String::new(),
                calls,
                wire_blocks: Vec::new(),
                usage: Usage::default(),
            },
        )]);
        let expected = [
            ("read", json!({"path":"README.md"})),
            (
                "edit",
                json!({"path":"a.rs","reason":"r","before":"x","after":"y"}),
            ),
            ("run_task", json!({"task":"cargo-test"})),
        ];
        for protocol in PROTOCOLS {
            let plan = prepare_request(&native_provider(protocol), &history, false, "", &wire)?;
            let names = tool_names(&plan.body);
            let replayed: Vec<(String, String, Value)> = match protocol {
                Protocol::Responses => plan.body["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|item| item["type"] == "function_call")
                    .map(|item| {
                        (
                            field(item, "call_id").to_owned(),
                            field(item, "name").to_owned(),
                            serde_json::from_str(field(item, "arguments")).unwrap(),
                        )
                    })
                    .collect(),
                Protocol::AnthropicMessages => plan.body["messages"][1]["content"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|block| {
                        assert_eq!(block["type"], "tool_use");
                        (
                            field(block, "id").to_owned(),
                            field(block, "name").to_owned(),
                            block["input"].clone(),
                        )
                    })
                    .collect(),
                _ => plan.body["messages"][2]["tool_calls"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|call| {
                        (
                            field(call, "id").to_owned(),
                            field(&call["function"], "name").to_owned(),
                            serde_json::from_str(field(&call["function"], "arguments")).unwrap(),
                        )
                    })
                    .collect(),
            };
            assert_eq!(replayed.len(), 3, "{protocol:?}");
            for (index, ((id, name, input), (expected_name, expected_input))) in
                replayed.iter().zip(&expected).enumerate()
            {
                assert_eq!(id, &format!("call_{index}"));
                assert_eq!(name, expected_name, "{protocol:?}");
                assert!(names.contains(name), "{protocol:?} {name}");
                assert_eq!(input, expected_input, "{protocol:?}");
            }
            let results = plan.body.to_string();
            for index in 0..3 {
                assert!(results.contains(&format!("Tool result\\ncall_{index}")));
            }
        }
        Ok(())
    }

    #[test]
    fn unknown_tool_names_reach_dispatch_as_tool_errors() -> Result<()> {
        let event = json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"id":"x","function":{"name":"teleport","arguments":"{\"to\":\"moon\"}"}}
        ]},"finish_reason":"tool_calls"}]});
        let reply = decode(&format!("data: {event}\n\n"), false)?;
        let action = parse_action(&reply.calls[0].arguments).unwrap();
        assert_eq!(action, json!({"action":"teleport","to":"moon"}));
        let error = tools::unknown(&action).unwrap().to_string();
        assert!(error.contains("teleport"), "{error}");
        Ok(())
    }

    pub(in crate::native) fn message(text: &str) -> Message {
        Message {
            role: "user".into(),
            content: text.into(),
            reasoning: None,
            ..Default::default()
        }
    }

    #[test]
    fn openai_reasoning_is_streamed_separately() -> Result<()> {
        let stream = "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"plan \"}}]}\n\n\
                      data: {\"choices\":[{\"delta\":{\"reasoning\":\"more\"}}]}\n\n\
                      data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\n\
                      data: [DONE]\n\n";
        let reply = decode(stream, false)?;
        assert_eq!(reply.content, "Hello");
        assert_eq!(reply.reasoning.as_deref(), Some("plan more"));
        Ok(())
    }

    #[test]
    fn anthropic_thinking_is_streamed_separately() -> Result<()> {
        let stream = "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"consider\"}}\n\n\
                      data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"Answer\"}}\n\n\
                      data: {\"type\":\"message_stop\"}\n\n";
        let reply = decode(stream, true)?;
        assert_eq!(reply.content, "Answer");
        assert_eq!(reply.reasoning.as_deref(), Some("consider"));
        Ok(())
    }

    #[test]
    fn reasoning_only_reply_is_complete() -> Result<()> {
        let stream = "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"thinking\"}}]}\n\ndata: [DONE]\n\n";
        let reply = decode(stream, false)?;
        assert!(reply.content.is_empty());
        assert_eq!(reply.reasoning.as_deref(), Some("thinking"));
        Ok(())
    }

    #[test]
    fn output_limit_is_actionable() {
        let stream =
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\ndata: [DONE]\n\n";
        let error = decode(stream, false).unwrap_err().to_string();
        assert!(error.contains("Выход") || error.contains("output"));
    }

    #[test]
    fn openai_reasoning_payload_uses_effort_and_output_limit() -> Result<()> {
        let mut provider = provider("m", "k", 128_000);
        provider.reasoning = Reasoning::High;
        provider.output = Some(8192);
        let plan = build_request(&provider, &[message("hi")], false, "")?;
        assert_eq!(plan.body["reasoning_effort"], "high");
        assert_eq!(plan.body["max_tokens"], 8192);
        assert!(plan.input_tokens > 0);
        Ok(())
    }

    #[test]
    fn openrouter_reasoning_payload_uses_object() -> Result<()> {
        let mut provider = provider("m", "k", 128_000);
        provider.connection.url = "https://openrouter.ai/api/v1".into();
        provider.reasoning = Reasoning::Medium;
        let plan = build_request(&provider, &[message("hi")], false, "")?;
        assert_eq!(plan.body["reasoning"]["effort"], "medium");
        assert!(plan.body.get("reasoning_effort").is_none());
        Ok(())
    }

    #[test]
    fn anthropic_reasoning_payload_uses_budget() -> Result<()> {
        let mut provider = provider("m", "k", 128_000);
        provider.connection.protocol = Protocol::AnthropicMessages;
        provider.reasoning = Reasoning::Medium;
        provider.output = Some(8192);
        let plan = build_request(&provider, &[message("hi")], false, "")?;
        assert_eq!(plan.body["thinking"]["type"], "enabled");
        assert_eq!(plan.body["thinking"]["budget_tokens"], 2048);
        assert_eq!(plan.body["max_tokens"], 8192);
        Ok(())
    }

    #[test]
    fn anthropic_reasoning_rejects_small_output() {
        let mut provider = provider("m", "k", 128_000);
        provider.connection.protocol = Protocol::AnthropicMessages;
        provider.reasoning = Reasoning::High;
        provider.output = Some(4096);
        assert!(build_request(&provider, &[message("hi")], false, "").is_err());
    }

    #[test]
    fn equal_maxima_allow_a_reply_capped_by_the_current_request() -> Result<()> {
        for (protocol, expected_key, host) in [
            (
                Protocol::ChatCompletions,
                "max_tokens",
                "https://example.org/v1",
            ),
            (
                Protocol::ChatCompletions,
                "max_completion_tokens",
                "https://api.openai.com/v1",
            ),
            (
                Protocol::Responses,
                "max_output_tokens",
                "https://example.org/v1",
            ),
            (
                Protocol::AnthropicMessages,
                "max_tokens",
                "https://example.org/v1",
            ),
        ] {
            let mut provider = provider("m", "", 1_048_576);
            provider.connection.protocol = protocol;
            provider.connection.url = host.into();
            provider.input = Some(1_048_576);
            provider.output = Some(1_048_576);
            let history = [message(&"x".repeat(1200)), message("continue")];
            let baseline = build_request(&provider, &[], false, "")?;
            let plan = build_request(&provider, &history, false, &"instructions ".repeat(120))?;
            assert!(plan.input_tokens > baseline.input_tokens + 600);
            assert_eq!(
                plan.body[expected_key],
                provider.context_limit() - plan.input_tokens
            );

            let next_limit = plan.input_tokens + 1;
            provider.context = next_limit;
            provider.input = Some(next_limit);
            provider.output = Some(next_limit);
            let plan = build_request(&provider, &history, false, &"instructions ".repeat(120))?;
            assert_eq!(plan.body[expected_key], 1);
            provider.context -= 1;
            let error = build_request(&provider, &history, false, &"instructions ".repeat(120))
                .unwrap_err();
            assert!(error.downcast_ref::<OutputSpaceExceeded>().is_some());
        }
        Ok(())
    }

    #[test]
    fn context_shortage_blocks_anthropic_reasoning_before_network() -> Result<()> {
        let mut provider = provider("m", "", 8_000);
        provider.connection.protocol = Protocol::AnthropicMessages;
        provider.reasoning = Reasoning::Medium;
        provider.output = Some(4_096);
        let history = [message(&"x".repeat(2_000))];
        let estimate = build_request(&provider, &history, false, "")?.input_tokens;
        provider.context = estimate + 2_048;
        let result = build_request(&provider, &history, false, "");
        assert_eq!(input_estimate(&result), Some(estimate));
        let error = result.unwrap_err();
        let error = error.downcast_ref::<OutputSpaceExceeded>().unwrap();
        assert_eq!(error.available, 2_048);
        assert_eq!(error.reasoning_budget, 2_048);
        Ok(())
    }

    #[test]
    fn deepseek_off_and_manual_reasoning_use_each_protocols_fields() -> Result<()> {
        for (protocol, enable, effort, disable) in [
            (
                Protocol::ChatCompletions,
                Some("/thinking/type"),
                "/reasoning_effort",
                "/thinking/type",
            ),
            (
                Protocol::Responses,
                None,
                "/reasoning/effort",
                "/reasoning/effort",
            ),
            (
                Protocol::AnthropicMessages,
                Some("/thinking/type"),
                "/output_config/effort",
                "/thinking/type",
            ),
        ] {
            let mut provider = provider("m", "", 64_000);
            provider.connection.protocol = protocol;
            provider.connection.url = "https://api.deepseek.com/v1".into();
            provider.reasoning_levels = Some(vec![ReasoningLevel {
                level: Reasoning::Xhigh,
                wire: Some("high".into()),
            }]);
            provider.reasoning = Reasoning::Xhigh;
            let plan = build_request(&provider, &[], false, "")?;
            assert_eq!(plan.body.pointer(effort), Some(&json!("high")));
            if let Some(field) = enable {
                assert_eq!(plan.body.pointer(field), Some(&json!("enabled")));
            }
            provider.reasoning = Reasoning::Off;
            let plan = build_request(&provider, &[], false, "")?;
            let disabled = if protocol == Protocol::Responses {
                "none"
            } else {
                "disabled"
            };
            assert_eq!(plan.body.pointer(disable), Some(&json!(disabled)));

            provider.connection.url = "https://gateway.example/v1".into();
            provider.reasoning_levels = Some(vec![ReasoningLevel {
                level: Reasoning::Ultra,
                wire: Some("deep".into()),
            }]);
            provider.reasoning = Reasoning::Ultra;
            if protocol == Protocol::AnthropicMessages {
                provider.reasoning_api = ReasoningApi::Adaptive;
            }
            let plan = build_request(&provider, &[], false, "")?;
            assert_eq!(plan.body.pointer(effort), Some(&json!("deep")));
        }
        Ok(())
    }

    #[test]
    fn reasoning_off_and_none_send_nothing() -> Result<()> {
        let mut provider = provider("m", "k", 128_000);
        let plan = build_request(&provider, &[message("hi")], false, "")?;
        assert!(plan.body.get("reasoning_effort").is_none());
        provider.reasoning = Reasoning::High;
        provider.reasoning_api = ReasoningApi::None;
        let plan = build_request(&provider, &[message("hi")], false, "")?;
        assert!(plan.body.get("reasoning_effort").is_none());
        assert!(plan.body.get("reasoning").is_none());
        Ok(())
    }

    #[test]
    fn input_limit_is_enforced_before_network() {
        let mut provider = provider("m", "k", 128_000);
        let initial = prepare_request(&provider, &[], false, "", &WireCalls::new());
        assert!(input_estimate(&initial).is_some_and(|tokens| tokens > 0));

        provider.input = Some(1_000);
        let too_large = build_request(&provider, &[message(&"x".repeat(20_000))], false, "");
        assert!(input_estimate(&too_large).is_some_and(|tokens| tokens > 5_000));
        let error = too_large.unwrap_err().to_string();
        assert!(error.contains("1000") || error.contains("1 000"));
        assert!(error.contains("лимит") || error.contains("limit"));

        let invalid = build_request(&provider, &[message(&"x".repeat(600_000))], false, "");
        assert!(invalid.is_err());
        assert_eq!(input_estimate(&invalid), None);
    }

    #[test]
    fn provider_error_exposes_body_and_redacts_key() {
        let text = http_error_text(
            401,
            "{\"error\":{\"message\":\"bad key sk-secret-123\"}}",
            "sk-secret-123",
        );
        assert!(text.contains("401"));
        assert!(text.contains("bad key"));
        assert!(!text.contains("sk-secret-123"));
        assert!(text.contains("<key>"));
    }

    #[test]
    fn probe_payload_is_small_and_has_no_tools() {
        let provider = provider("m", "k", 128_000);
        let body = probe_payload(&provider);
        assert_eq!(body["max_tokens"], 16);
        assert!(body.get("tools").is_none());
        assert_eq!(body["messages"][0]["content"], "ping");
    }

    #[test]
    fn screenshot_is_attached_to_the_next_request_only() {
        for (protocol, path) in [
            (Protocol::ChatCompletions, "messages"),
            (Protocol::AnthropicMessages, "messages"),
            (Protocol::Responses, "input"),
        ] {
            let mut provider = provider("test", "", default_context());
            provider.connection.protocol = protocol;
            provider.vision = true;
            let mut body = json!({path:[]});
            attach_image(
                &provider,
                &mut body,
                &Visual {
                    mime: "image/jpeg".into(),
                    data: "/9j/".into(),
                },
            );
            let content = body[path].as_array().unwrap();
            assert_eq!(content.len(), 1);
            assert!(content[0]["content"].to_string().contains("/9j/"));
            provider.vision = false;
            body[path] = json!([]);
            attach_image(
                &provider,
                &mut body,
                &Visual {
                    mime: "image/jpeg".into(),
                    data: "/9j/".into(),
                },
            );
            assert!(body[path].as_array().unwrap().is_empty());
        }
    }

    #[test]
    fn responses_continues_native_tools_with_the_exact_identifier() -> Result<()> {
        let payload = [
            json!({"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_test","name":"read"}}),
            json!({"type":"response.function_call_arguments.delta","delta":"{\"path\":"}),
            json!({"type":"response.function_call_arguments.delta","delta":"\"README.md\"}"}),
            json!({"type":"response.completed"}),
        ];
        let stream = payload
            .iter()
            .map(|value| format!("data: {value}\n\n"))
            .collect::<String>();
        let (tx, _rx) = mpsc::channel();
        let result = decode_stream_mode(
            std::io::Cursor::new(stream),
            false,
            true,
            &AtomicBool::new(false),
            tx,
        )?;
        assert_eq!(result.calls[0].id, "call_test");
        let history = vec![
            Message {
                role: "assistant".into(),
                content: result.calls[0].arguments.clone(),
                reasoning: None,
                ..Default::default()
            },
            Message {
                role: "user".into(),
                content: "File contents".into(),
                reasoning: None,
                ..Default::default()
            },
        ];
        let wire = WireCalls::from([(0, result)]);
        let mut provider = provider("test", "", default_context());
        provider.connection.protocol = Protocol::Responses;
        let plan = prepare_request(&provider, &history, false, "", &wire)?;
        assert_eq!(plan.body["input"][0]["call_id"], "call_test");
        assert_eq!(plan.body["input"][0]["name"], "read");
        assert_eq!(
            plan.body["input"][0]["arguments"],
            r#"{"path":"README.md"}"#
        );
        assert_eq!(plan.body["input"][1]["call_id"], "call_test");
        assert_eq!(plan.body["input"][1]["output"], "File contents");
        Ok(())
    }
}
