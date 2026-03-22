use clap::Parser;
use futures_util::StreamExt;
use oct::core::{ContentPart, FinishReason, Message, Role, StreamEvent, ToolCall, ToolResult, ToolSpec};
use oct::model::ChatRequest;
use oct::providers::MoonshotAIProvider;
use oct::Provider;
use serde_json::json;

#[derive(Parser, Debug)]
#[command(name = "chat", about = "Chat example for Kimi API")]
struct Args {
    #[arg(short, long)]
    message: String,

    #[arg(short = 'k', long, env = "KIMI_API_KEY")]
    api_key: Option<String>,

    #[arg(long, default_value = "moonshot-v1-8k")]
    model: String,

    #[arg(short, long, default_value_t = false)]
    stream: bool,

    #[arg(long, default_value_t = false)]
    interactive: bool,
}

fn get_tools() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "get_weather".to_string(),
            description: Some("获取指定城市的天气信息".to_string()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "location": {
                        "type": "string",
                        "description": "城市名称"
                    }
                },
                "required": ["location"]
            }),
        },
        ToolSpec {
            name: "calculate".to_string(),
            description: Some("执行数学计算".to_string()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "expression": {
                        "type": "string",
                        "description": "数学表达式"
                    }
                },
                "required": ["expression"]
            }),
        },
    ]
}

fn read_tool_result(call: &ToolCall, interactive: bool) -> ToolResult {
    let result = if interactive {
        eprint!("Enter result for {} (or press Enter to skip): ", call.name);
        let mut input = String::new();
        std::io::stdin().read_line(&mut input).unwrap();
        let input = input.trim();
        if input.is_empty() {
            json!({"status": "skipped"})
        } else {
            json!({"result": input})
        }
    } else {
        json!({"status": "no handler configured", "tool": call.name, "arguments": call.arguments})
    };

    ToolResult {
        call_id: call.id.clone(),
        content: result.clone(),
        is_error: false,
    }
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    if let Some(key) = &args.api_key {
        unsafe { std::env::set_var("KIMI_API_KEY", key); }
    }

    let provider = MoonshotAIProvider::new("https://api.moonshot.cn/v1", "KIMI_API_KEY");
    let chat_model = provider.chat_model(&args.model).expect("Failed to create chat model");

    let tools = get_tools();

    let mut messages = vec![Message::text(Role::User, &args.message)];

    loop {
        let request = ChatRequest {
            messages: messages.clone(),
            tools: tools.clone(),
            options: Default::default(),
        };

        if args.stream {
            let mut stream = chat_model.stream(request).await.expect("Failed to start stream");

            let mut current_text = String::new();
            let mut current_reasoning = String::new();
            let mut tool_calls: Vec<ToolCall> = vec![];
            let mut has_finished = false;

            while let Some(event) = stream.next().await {
                match event {
                    Ok(StreamEvent::TextDelta(text)) => {
                        current_text.push_str(&text);
                        print!("{}", text);
                    }
                    Ok(StreamEvent::ReasoningDelta(text)) => {
                        current_reasoning.push_str(&text);
                        eprint!("[reasoning] {}", text);
                    }
                    Ok(StreamEvent::ToolCallDelta { call_id, name, arguments_delta }) => {
                        if let Some(name) = name {
                            if !call_id.is_empty() {
                                if !tool_calls.iter().any(|c| c.id == call_id) {
                                    tool_calls.push(ToolCall {
                                        id: call_id.clone(),
                                        name: name.clone(),
                                        arguments: json!(arguments_delta),
                                    });
                                }
                            }
                        }
                        eprintln!("\n[tool delta] {}: {:?}", call_id, arguments_delta);
                    }
                    Ok(StreamEvent::ToolCall(call)) => {
                        eprintln!("\n[tool call] {}: {:?}", call.name, call.arguments);
                        if !tool_calls.iter().any(|c| c.id == call.id) {
                            tool_calls.push(call);
                        }
                    }
                    Ok(StreamEvent::Finish(reason)) => {
                        eprintln!("\n[finish: {:?}]", reason);
                        has_finished = true;
                        if reason == FinishReason::ToolCalls || !tool_calls.is_empty() {
                            let mut tool_results = vec![];
                            for call in &tool_calls {
                                let result = read_tool_result(call, args.interactive);
                                tool_results.push(result);
                            }

                            let mut assistant_parts = vec![];
                            if !current_text.is_empty() {
                                assistant_parts.push(ContentPart::Text(current_text.clone()));
                            }
                            if !current_reasoning.is_empty() {
                                assistant_parts.push(ContentPart::Reasoning(current_reasoning.clone()));
                            }
                            for call in &tool_calls {
                                assistant_parts.push(ContentPart::ToolCall(call.clone()));
                            }
                            messages.push(Message::new(Role::Assistant, assistant_parts));

                            for result in tool_results {
                                eprintln!("[tool result] {:?}", result.content);
                                messages.push(Message::new(Role::Tool, vec![ContentPart::ToolResult(result)]));
                            }

                            break;
                        }
                    }
                    Ok(StreamEvent::Usage(usage)) => {
                        eprintln!("[usage: input={}, output={}]",
                            usage.input_tokens.unwrap_or(0),
                            usage.output_tokens.unwrap_or(0));
                    }
                    Ok(other) => eprintln!("\n{:?}", other),
                    Err(e) => {
                        eprintln!("\nError: {:?}", e);
                        has_finished = true;
                    }
                }
            }

            if has_finished && tool_calls.is_empty() {
                break;
            }

            if !has_finished {
                break;
            }
        } else {
            let response = chat_model.generate(request).await.expect("Request failed");

            let mut tool_calls: Vec<ToolCall> = vec![];
            let mut text_parts: Vec<String> = vec![];
            let mut reasoning_parts: Vec<String> = vec![];

            for part in &response.message.parts {
                match part {
                    ContentPart::Text(text) => text_parts.push(text.clone()),
                    ContentPart::Reasoning(text) => reasoning_parts.push(text.clone()),
                    ContentPart::ToolCall(call) => tool_calls.push(call.clone()),
                    _ => {}
                }
            }

            if !reasoning_parts.is_empty() {
                eprintln!("[reasoning] {}", reasoning_parts.join(""));
            }
            if !text_parts.is_empty() {
                println!("{}", text_parts.join(""));
            }

            if tool_calls.is_empty() {
                if let Some(usage) = response.usage {
                    eprintln!("\n[usage: input={}, output={}]",
                        usage.input_tokens.unwrap_or(0),
                        usage.output_tokens.unwrap_or(0));
                }
                break;
            }

            let mut tool_results = vec![];
            for call in &tool_calls {
                eprintln!("[tool call] {}: {:?}", call.name, call.arguments);

                let result = read_tool_result(call, args.interactive);
                eprintln!("[tool result] {:?}", result.content);
                tool_results.push(result);
            }

            messages.push(Message::new(Role::Assistant,
                tool_calls.iter().map(|c| ContentPart::ToolCall(c.clone())).collect()));

            for result in tool_results {
                messages.push(Message::new(Role::Tool, vec![ContentPart::ToolResult(result)]));
            }

            if response.finish_reason == FinishReason::Stop {
                if let Some(usage) = response.usage {
                    eprintln!("\n[usage: input={}, output={}]",
                        usage.input_tokens.unwrap_or(0),
                        usage.output_tokens.unwrap_or(0));
                }
                break;
            }
        }
    }
}