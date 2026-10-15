//! Provider stream normalization and tool-call accumulation.
use super::*;

#[derive(Default)]
pub(super) struct AnthropicStreamState {
    tool_state: HashMap<usize, AnthropicToolAccumulator>,
    reasoning_state: std::collections::BTreeMap<usize, AnthropicContentBlock>,
    stop_reason: Option<String>,
}

pub fn aggregate_anthropic_stream(events: &[StreamEvent]) -> Option<Usage> {
    events.iter().rev().find_map(|event| match event {
        StreamEvent::Usage(usage) => Some(usage.clone()),
        _ => None,
    })
}

pub fn parse_anthropic_sse_event(event: &str) -> Result<Vec<StreamEvent>, ModelError> {
    parse_anthropic_sse_event_with_state(event, &mut AnthropicStreamState::default())
}

pub fn parse_anthropic_sse_transcript(transcript: &str) -> Result<Vec<StreamEvent>, ModelError> {
    let mut decoder = SseDecoder::new();
    let mut state = AnthropicStreamState::default();
    let mut events = Vec::new();

    // The transcript comes from a &str, so events are valid UTF-8 by
    // construction and decoding cannot fail.
    for event in decoder.push(transcript.as_bytes())? {
        events.extend(parse_anthropic_sse_event_with_state(&event, &mut state)?);
    }
    for event in decoder.finish()? {
        events.extend(parse_anthropic_sse_event_with_state(&event, &mut state)?);
    }

    Ok(events)
}

pub(super) fn parse_anthropic_sse_event_with_state(
    event: &str,
    state: &mut AnthropicStreamState,
) -> Result<Vec<StreamEvent>, ModelError> {
    let mut data_lines = Vec::new();
    for line in event.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.trim());
        }
    }

    if data_lines.is_empty() {
        return Ok(Vec::new());
    }

    let payload = data_lines.join("\n");
    let stream_event: AnthropicStreamEvent = serde_json::from_str(&payload)
        .map_err(|err| ModelError::transport(format!("invalid anthropic stream chunk: {err}")))?;

    normalize_anthropic_stream_event_with_state(stream_event, state)
}

pub fn normalize_anthropic_stream_event(
    event: AnthropicStreamEvent,
) -> Result<Vec<StreamEvent>, ModelError> {
    normalize_anthropic_stream_event_with_state(event, &mut AnthropicStreamState::default())
}

pub(super) fn normalize_anthropic_stream_event_with_state(
    event: AnthropicStreamEvent,
    state: &mut AnthropicStreamState,
) -> Result<Vec<StreamEvent>, ModelError> {
    let AnthropicStreamState {
        tool_state,
        reasoning_state,
        stop_reason,
    } = state;
    let mut events = Vec::new();

    match event {
        AnthropicStreamEvent::MessageStart { message } => {
            events.push(StreamEvent::Usage(map_anthropic_usage(&message.usage)));
        }
        AnthropicStreamEvent::MessageDelta { delta, usage } => {
            *stop_reason = delta.stop_reason.clone();
            let mut delta_usage = Usage::new(
                usage.input_tokens.map(|v| v as u32),
                Some(usage.output_tokens as u32),
                None,
            );
            delta_usage.cached_input_tokens = usage.cache_read_input_tokens.map(|v| v as u32);
            delta_usage.provider_details.insert(
                "cache_creation_input_tokens".to_string(),
                Value::Number(usage.cache_creation_input_tokens.unwrap_or(0).into()),
            );
            if let Some(server_tool_use) = usage.server_tool_use {
                delta_usage.provider_details.insert(
                    "server_tool_use".to_string(),
                    serde_json::to_value(server_tool_use).unwrap_or(Value::Null),
                );
            }
            events.push(StreamEvent::Usage(delta_usage));
        }
        AnthropicStreamEvent::MessageStop => {
            for block in std::mem::take(reasoning_state).into_values() {
                events.push(completed_reasoning(block)?);
            }
            for (index, acc) in std::mem::take(tool_state) {
                if !acc.emitted && (acc.name.is_some() || !acc.arguments.is_empty()) {
                    events.push(StreamEvent::ToolCall(crate::core::ToolCall {
                        id: acc.id.unwrap_or_else(|| format!("tool_{}", index)),
                        name: acc.name.unwrap_or_default(),
                        arguments: acc.arguments,
                    }));
                }
            }
            let finish_reason = match stop_reason.as_deref() {
                Some(reason) => anthropic_finish_reason(Some(reason)),
                None => FinishReason::Stop,
            };
            events.push(StreamEvent::Finish(finish_reason));
        }
        AnthropicStreamEvent::ContentBlockStart {
            index,
            content_block,
        } => match content_block {
            AnthropicContentBlock::ToolUse(tool_block) => {
                tool_state.insert(
                    index,
                    AnthropicToolAccumulator {
                        id: Some(tool_block.id.clone()),
                        name: Some(tool_block.name.clone()),
                        arguments: String::new(),
                        emitted: false,
                    },
                );
            }
            AnthropicContentBlock::Thinking(thinking_block) => {
                if !thinking_block.thinking.is_empty() {
                    events.push(StreamEvent::ReasoningDelta(thinking_block.thinking.clone()));
                }
                reasoning_state.insert(index, AnthropicContentBlock::Thinking(thinking_block));
            }
            block @ AnthropicContentBlock::RedactedThinking(_) => {
                reasoning_state.insert(index, block);
            }
            AnthropicContentBlock::Text(text_block) if !text_block.text.is_empty() => {
                events.push(StreamEvent::TextDelta(text_block.text));
            }
            _ => {}
        },
        AnthropicStreamEvent::ContentBlockDelta { index, delta } => {
            match delta {
                AnthropicContentBlockDelta::TextDelta { text } => {
                    events.push(StreamEvent::TextDelta(text));
                }
                AnthropicContentBlockDelta::InputJsonDelta { partial_json } => {
                    let accumulator = tool_state.entry(index).or_default();
                    accumulator.arguments.push_str(&partial_json);
                    let call_id = accumulator
                        .id
                        .clone()
                        .unwrap_or_else(|| format!("tool_{}", index));
                    events.push(StreamEvent::ToolCallDelta {
                        call_id,
                        name: None,
                        arguments_delta: partial_json,
                    });
                }
                AnthropicContentBlockDelta::ThinkingDelta { thinking } => {
                    let block = reasoning_state.entry(index).or_insert_with(|| {
                        AnthropicContentBlock::Thinking(AnthropicThinkingBlock {
                            thinking: String::new(),
                            signature: None,
                            prefix_fingerprint: None,
                        })
                    });
                    if let AnthropicContentBlock::Thinking(block) = block {
                        block.thinking.push_str(&thinking);
                    }
                    events.push(StreamEvent::ReasoningDelta(thinking));
                }
                AnthropicContentBlockDelta::SignatureDelta { signature } => {
                    // Preserve the opaque signature for tool-loop continuity;
                    // it travels in the completed block, never as display text.
                    let block = reasoning_state.entry(index).or_insert_with(|| {
                        AnthropicContentBlock::Thinking(AnthropicThinkingBlock {
                            thinking: String::new(),
                            signature: None,
                            prefix_fingerprint: None,
                        })
                    });
                    if let AnthropicContentBlock::Thinking(block) = block {
                        block
                            .signature
                            .get_or_insert_with(String::new)
                            .push_str(&signature);
                    }
                }
                AnthropicContentBlockDelta::CitationsDelta { citation: _ } => {
                    // Citations are not exposed in our current API
                }
            }
        }
        AnthropicStreamEvent::ContentBlockStop { index } => {
            if let Some(block) = reasoning_state.remove(&index) {
                events.push(completed_reasoning(block)?);
            }
            if let Some(acc) = tool_state.remove(&index)
                && !acc.emitted
                && (acc.name.is_some() || !acc.arguments.is_empty())
            {
                events.push(StreamEvent::ToolCall(crate::core::ToolCall {
                    id: acc.id.unwrap_or_else(|| format!("tool_{}", index)),
                    name: acc.name.unwrap_or_default(),
                    arguments: acc.arguments,
                }));
            }
        }
        AnthropicStreamEvent::Ping => {
            // Ping events are keep-alive signals, no action needed
        }
    }

    Ok(events)
}

fn completed_reasoning(block: AnthropicContentBlock) -> Result<StreamEvent, ModelError> {
    let ContentPart::ReasoningBlock(block) = parse_anthropic_content_block(block)? else {
        unreachable!("the reasoning accumulator contains only thinking/redacted blocks")
    };
    Ok(StreamEvent::ReasoningBlock(block))
}
#[cfg(test)]
mod sse_tests {
    use super::*;

    /// Network chunks routinely split a multibyte UTF-8 character
    /// mid-sequence: replaying the transcript through every possible chunk
    /// split must decode without error and produce events identical to the
    /// whole-transcript parse.
    #[test]
    fn anthropic_sse_survives_multibyte_splits_across_chunks() {
        let transcript = concat!(
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"你好\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"，世界！\"}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let expected = parse_anthropic_sse_transcript(transcript).unwrap();
        assert_eq!(
            expected,
            vec![
                StreamEvent::TextDelta("你好".to_string()),
                StreamEvent::TextDelta("，世界！".to_string()),
                StreamEvent::Finish(FinishReason::Stop),
            ]
        );

        for size in 1..transcript.len() {
            let mut decoder = SseDecoder::new();
            let mut state = AnthropicStreamState::default();
            let mut got = Vec::new();
            for chunk in transcript.as_bytes().chunks(size) {
                for event in decoder
                    .push(chunk)
                    .unwrap_or_else(|err| panic!("chunk size {size}: {err}"))
                {
                    got.extend(
                        parse_anthropic_sse_event_with_state(&event, &mut state)
                            .unwrap_or_else(|err| panic!("chunk size {size}: {err}")),
                    );
                }
            }
            for event in decoder
                .finish()
                .unwrap_or_else(|err| panic!("chunk size {size}: {err}"))
            {
                got.extend(
                    parse_anthropic_sse_event_with_state(&event, &mut state)
                        .unwrap_or_else(|err| panic!("chunk size {size}: {err}")),
                );
            }
            assert_eq!(got, expected, "mismatch at chunk size {size}");
        }
    }
}
