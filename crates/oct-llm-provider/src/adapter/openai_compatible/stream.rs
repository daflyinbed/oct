//! Provider stream normalization and tool-call accumulation.
use super::*;

pub fn aggregate_stream_events(events: &[StreamEvent]) -> Option<Usage> {
    events.iter().rev().find_map(|event| match event {
        StreamEvent::Usage(usage) => Some(usage.clone()),
        _ => None,
    })
}

pub fn parse_openai_sse_event(event: &str) -> Result<Vec<StreamEvent>, ModelError> {
    parse_openai_sse_event_with_state(event, &mut HashMap::new())
}

pub fn parse_openai_sse_transcript(transcript: &str) -> Result<Vec<StreamEvent>, ModelError> {
    let mut decoder = SseDecoder::new();
    let mut tool_state = HashMap::new();
    let mut events = Vec::new();

    // The transcript comes from a &str, so events are valid UTF-8 by
    // construction and decoding cannot fail.
    for event in decoder.push(transcript.as_bytes())? {
        events.extend(parse_openai_sse_event_with_state(&event, &mut tool_state)?);
    }
    for event in decoder.finish()? {
        events.extend(parse_openai_sse_event_with_state(&event, &mut tool_state)?);
    }

    Ok(events)
}

pub(super) fn parse_openai_sse_event_with_state(
    event: &str,
    tool_state: &mut HashMap<String, OpenAiToolCallAccumulator>,
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

    let data = data_lines.join("\n");
    if data == "[DONE]" {
        return Ok(Vec::new());
    }

    let chunk: ChatCompletionChunk = serde_json::from_str(&data)
        .map_err(|err| ModelError::transport(format!("invalid openai stream chunk: {err}")))?;

    normalize_chat_completion_chunk_with_state(chunk, tool_state)
}

pub fn normalize_chat_completion_chunk(
    chunk: ChatCompletionChunk,
) -> Result<Vec<StreamEvent>, ModelError> {
    normalize_chat_completion_chunk_with_state(chunk, &mut HashMap::new())
}

pub(super) fn normalize_chat_completion_chunk_with_state(
    chunk: ChatCompletionChunk,
    tool_state: &mut HashMap<String, OpenAiToolCallAccumulator>,
) -> Result<Vec<StreamEvent>, ModelError> {
    let mut events = Vec::new();

    if let Some(ref usage) = chunk.usage {
        events.push(StreamEvent::Usage(map_chat_completion_usage(usage)));
    }

    for choice in chunk.choices {
        let reasoning = choice
            .delta
            .reasoning_content
            .as_ref()
            .or(choice.delta.reasoning.as_ref())
            .filter(|s| !s.is_empty());

        if let Some(reasoning) = reasoning {
            events.push(StreamEvent::ReasoningDelta(reasoning.clone()));
        }

        if let Some(ref content) = choice.delta.content
            && !content.is_empty()
        {
            events.push(StreamEvent::TextDelta(content.clone()));
        }

        if let Some(tool_calls) = choice.delta.tool_calls {
            for tool_call in tool_calls {
                let (delta_event, final_event) = parse_tool_call_delta(tool_call, tool_state)?;
                events.push(delta_event);
                if let Some(final_event) = final_event {
                    events.push(final_event);
                }
            }
        }

        if let Some(reason) = choice.finish_reason {
            if reason == "tool_calls" || reason == "function_call" {
                for (index, accumulator) in std::mem::take(tool_state) {
                    if accumulator.id.is_some() || accumulator.name.is_some() {
                        events.push(StreamEvent::ToolCall(crate::core::ToolCall {
                            // Keep the id consistent with the delta events
                            // (which fall back to the index): an empty id here
                            // would break downstream start/result matching
                            // for providers that never send one.
                            id: accumulator.id.unwrap_or(index),
                            name: accumulator.name.unwrap_or_default(),
                            arguments: accumulator.arguments,
                        }));
                    }
                }
            }
            events.push(StreamEvent::Finish(map_openai_finish_reason(Some(&reason))));
        }
    }

    Ok(events)
}

pub(super) fn parse_tool_call_delta(
    tool_call: ChatCompletionChunkDeltaToolCall,
    tool_state: &mut HashMap<String, OpenAiToolCallAccumulator>,
) -> Result<(StreamEvent, Option<StreamEvent>), ModelError> {
    let index = tool_call.index.to_string();

    let call_id_str = tool_call.id.clone();
    let (name, arguments_delta) = if let Some(ref function) = tool_call.function {
        (
            function.name.clone(),
            function.arguments.clone().unwrap_or_default(),
        )
    } else {
        (None, String::new())
    };

    let accumulator = tool_state.entry(index.clone()).or_default();
    if let Some(ref id) = call_id_str {
        accumulator.id = Some(id.clone());
    }
    if let Some(n) = name.clone() {
        accumulator.name = Some(n);
    }
    accumulator.arguments.push_str(&arguments_delta);

    // Some providers (e.g. DeepSeek) include the tool call id only on the
    // first delta of a call; every later delta must reuse the id established
    // for this index, otherwise downstream delta/dispatch matching by call_id
    // breaks and the deltas split across two ids.
    let delta_call_id = accumulator.id.clone().unwrap_or_else(|| index.clone());

    Ok((
        StreamEvent::ToolCallDelta {
            call_id: delta_call_id,
            name,
            arguments_delta,
        },
        None,
    ))
}

pub type OpenAiWireRequest = ChatCompletionRequest;
pub type OpenAiGenerateResponse = ChatCompletionResponse;
pub type OpenAiChoice = ChatCompletionChoice;
pub type OpenAiRequestMessage = ChatCompletionRequestMessage;
pub type OpenAiResponseFormat = ChatCompletionResponseFormat;
pub type OpenAiToolDefinition = ChatCompletionTool;
pub type OpenAiStreamChunk = ChatCompletionChunk;
pub type OpenAiStreamChunkChoice = ChatCompletionChunkChoice;
pub type OpenAiStreamChunkChoiceDelta = ChatCompletionChunkDelta;

pub fn map_openai_generate_response(
    payload: OpenAiGenerateResponse,
) -> Result<ChatResponse, ModelError> {
    map_chat_completion_response(payload)
}

pub fn map_openai_usage(raw: &ChatCompletionUsage) -> Usage {
    map_chat_completion_usage(raw)
}

pub fn normalize_openai_stream_chunk(
    chunk: OpenAiStreamChunk,
) -> Result<Vec<StreamEvent>, ModelError> {
    normalize_chat_completion_chunk(chunk)
}

#[cfg(test)]
mod sse_tests {
    use super::*;

    /// Network chunks routinely split a multibyte UTF-8 character
    /// mid-sequence: replaying the transcript through every possible chunk
    /// split must decode without error and produce events identical to the
    /// whole-transcript parse.
    #[test]
    fn openai_sse_survives_multibyte_splits_across_chunks() {
        let transcript = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"你好\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"，世界！\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let expected = parse_openai_sse_transcript(transcript).unwrap();
        assert_eq!(
            expected,
            vec![
                StreamEvent::TextDelta("你好".to_string()),
                StreamEvent::TextDelta("，世界！".to_string()),
            ]
        );

        for size in 1..transcript.len() {
            let mut decoder = SseDecoder::new();
            let mut tool_state = HashMap::new();
            let mut got = Vec::new();
            for chunk in transcript.as_bytes().chunks(size) {
                for event in decoder
                    .push(chunk)
                    .unwrap_or_else(|err| panic!("chunk size {size}: {err}"))
                {
                    got.extend(
                        parse_openai_sse_event_with_state(&event, &mut tool_state)
                            .unwrap_or_else(|err| panic!("chunk size {size}: {err}")),
                    );
                }
            }
            for event in decoder
                .finish()
                .unwrap_or_else(|err| panic!("chunk size {size}: {err}"))
            {
                got.extend(
                    parse_openai_sse_event_with_state(&event, &mut tool_state)
                        .unwrap_or_else(|err| panic!("chunk size {size}: {err}")),
                );
            }
            assert_eq!(got, expected, "mismatch at chunk size {size}");
        }
    }
}
