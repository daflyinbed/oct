//! models.dev `modalities_input` 的类型化解释与请求装配时的多媒体门控。
//!
//! seed 把 models.dev 的 `modalities.input` 原样透存为 JSON 字符串，这里
//! 是唯一的解释点。用户消息附件（轻引用或 data URL）与 MCP 工具结果都可
//! 携带图片；文本模型收到图片会被 provider 直接 400，所以每次模型请求装
//! 配时按模态门控：模型不接受的图片被替换为一条文本注记（opencode
//! `unsupportedParts` 的同款语义——告知而非静默丢弃，模型的其余上下文保
//! 持原样），存储与 UI 不受影响。
//!
//! NULL/损坏/解析不出任何已知模态 → [`InputModalities::Unknown`]：放行
//! 一切，与 `reasoning.rs` 的策略一致——不认识的数据不拦人（custom 模型
//! 未填 modalities 时保持既有全放行行为）。

use oct_llm_provider::core::{ContentPart, Message, ToolResult};
use serde::{Deserialize, Serialize};

/// 一种输入模态（models.dev 目录的字面量）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Modality {
    Text,
    Image,
    Audio,
    Video,
    Pdf,
}

fn modality_from_str(raw: &str) -> Option<Modality> {
    match raw {
        "text" => Some(Modality::Text),
        "image" => Some(Modality::Image),
        "audio" => Some(Modality::Audio),
        "video" => Some(Modality::Video),
        "pdf" => Some(Modality::Pdf),
        _ => None,
    }
}

/// ASCII 前缀忽略大小写比较；`get` 保证多字节字符跨越边界时返回 None
/// 而不是 panic。
fn starts_with_ignore_case(base: &str, prefix: &str) -> bool {
    base.get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

/// MIME 类型 → 模态（忽略参数段与大小写）。未识别的类型按文本处理——
/// 工具结果的文本内容本就无需门控。
pub fn mime_to_modality(mime: &str) -> Modality {
    let base = mime.split(';').next().unwrap_or("").trim();
    if base.eq_ignore_ascii_case("application/pdf") {
        Modality::Pdf
    } else if starts_with_ignore_case(base, "image/") {
        Modality::Image
    } else if starts_with_ignore_case(base, "audio/") {
        Modality::Audio
    } else if starts_with_ignore_case(base, "video/") {
        Modality::Video
    } else {
        Modality::Text
    }
}

/// `models.modalities_input` 列的解释结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputModalities {
    Known(Vec<Modality>),
    Unknown,
}

impl InputModalities {
    /// 解析 DB 里透存的 JSON 数组。任何读不出的形状（NULL/非数组/全是
    /// 未知字面量）都归为 `Unknown`——门控侧放行。
    pub fn parse(raw: Option<&str>) -> Self {
        let Some(raw) = raw else {
            return Self::Unknown;
        };
        let Ok(serde_json::Value::Array(values)) = serde_json::from_str(raw) else {
            return Self::Unknown;
        };
        let modalities: Vec<Modality> = values
            .iter()
            .filter_map(|value| value.as_str().and_then(modality_from_str))
            .collect();
        if modalities.is_empty() {
            Self::Unknown
        } else {
            Self::Known(modalities)
        }
    }

    /// 该 MIME 的内容能否发给模型。文本恒放行（一个「不支持文本」的
    /// 条目只会是坏数据，不能拿来拦截全部对话）；`Unknown` 全放行。
    pub fn allows(&self, mime: &str) -> bool {
        let modality = mime_to_modality(mime);
        if modality == Modality::Text {
            return true;
        }
        match self {
            Self::Unknown => true,
            Self::Known(list) => list.contains(&modality),
        }
    }
}

/// data URL 的 MIME 段（`data:image/png;base64,…` → `image/png`）。非 data
/// URL 返回 None；ImageUrl 的轻引用与外链仍可按其图片语义门控。
pub fn data_url_mime(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("data:")?;
    rest.split(';').next().filter(|mime| !mime.is_empty())
}

/// Shared by request projection and token accounting, including attachment
/// references before hydration. ImageUrl without a data MIME is still an image.
pub(crate) fn omitted_image_note(url: &str, modalities: &InputModalities) -> Option<&'static str> {
    (!modalities.allows(data_url_mime(url).unwrap_or("image/png")))
        .then_some("[image omitted: this model does not accept image input]")
}

/// The projected tool text, without cloning any base64 image bytes. Keeping
/// this shared ensures COW savings use the same omission note as the request.
pub(crate) fn gated_tool_result_content(
    result: &ToolResult,
    modalities: &InputModalities,
) -> Option<String> {
    let dropped = result
        .images
        .iter()
        .filter(|image| !modalities.allows(&image.mime_type))
        .count();
    if dropped == 0 {
        return None;
    }
    // 注记接在既有内容之后：模型的其余上下文（包括结果文本本身）不动，
    // 只让「这里本有一张图」保持可见。
    let note = format!(
        "\n[{dropped} image attachment{} omitted: this model does not accept image input]",
        if dropped == 1 { "" } else { "s" }
    );
    Some(match &result.content {
        serde_json::Value::String(text) => format!("{text}{note}"),
        other => format!("{other}{note}"),
    })
}

/// 请求装配的最后一道纯转换：把模型不接受的用户/助手消息图片（ImageUrl
/// part）与工具结果图片替换为文本注记（保留其余图片与消息结构）。无门控
/// 命中时原样返回。
pub fn gate_request_messages(messages: Vec<Message>, modalities: &InputModalities) -> Vec<Message> {
    if matches!(modalities, InputModalities::Unknown) {
        return messages;
    }
    let mut messages = messages;
    for message in &mut messages {
        for part in &mut message.parts {
            match part {
                ContentPart::ToolResult(result) => gate_tool_result(result, modalities),
                // data URL 按 MIME 门控；轻引用与外链按 ImageUrl 的图片语义。
                ContentPart::ImageUrl { url } => {
                    if let Some(note) = omitted_image_note(url, modalities) {
                        *part = ContentPart::Text(note.to_string());
                    }
                }
                _ => {}
            }
        }
    }
    messages
}

fn gate_tool_result(result: &mut ToolResult, modalities: &InputModalities) {
    let Some(content) = gated_tool_result_content(result, modalities) else {
        return;
    };
    result.content = serde_json::Value::String(content);
    result
        .images
        .retain(|image| modalities.allows(&image.mime_type));
}

#[cfg(test)]
mod tests {
    use super::*;
    use oct_llm_provider::core::{ContentPart, Role, ToolResult, ToolResultImage};

    fn image(mime: &str) -> ToolResultImage {
        ToolResultImage {
            mime_type: mime.to_string(),
            data: "Zm9v".to_string(),
        }
    }

    fn tool_message(images: Vec<ToolResultImage>) -> Message {
        Message {
            role: Role::Tool,
            parts: vec![ContentPart::ToolResult(ToolResult {
                call_id: "c1".to_string(),
                content: serde_json::Value::String("screenshot attached".to_string()),
                is_error: false,
                images,
            })],
        }
    }

    fn images_of(message: &Message) -> &[ToolResultImage] {
        let ContentPart::ToolResult(result) = &message.parts[0] else {
            panic!("not a tool result");
        };
        &result.images
    }

    fn text_of(message: &Message) -> String {
        let ContentPart::ToolResult(result) = &message.parts[0] else {
            panic!("not a tool result");
        };
        result.content.as_str().unwrap_or_default().to_string()
    }

    #[test]
    fn parses_known_arrays_and_degrades_gracefully() {
        assert_eq!(
            InputModalities::parse(Some(r#"["text","image","pdf"]"#)),
            InputModalities::Known(vec![Modality::Text, Modality::Image, Modality::Pdf])
        );
        // NULL / 损坏 / 非数组 / 全是未知字面量 / 空数组 → Unknown（放行）
        assert_eq!(InputModalities::parse(None), InputModalities::Unknown);
        assert_eq!(
            InputModalities::parse(Some("oops")),
            InputModalities::Unknown
        );
        assert_eq!(
            InputModalities::parse(Some(r#"{"input":["text"]}"#)),
            InputModalities::Unknown
        );
        assert_eq!(
            InputModalities::parse(Some(r#"["future"]"#)),
            InputModalities::Unknown
        );
        assert_eq!(InputModalities::parse(Some("[]")), InputModalities::Unknown);
        // 未知字面量混在已知之中：只保留已知的
        assert_eq!(
            InputModalities::parse(Some(r#"["text","pdf","future"]"#)),
            InputModalities::Known(vec![Modality::Text, Modality::Pdf])
        );
    }

    #[test]
    fn maps_mimes_to_modalities() {
        assert_eq!(mime_to_modality("image/png"), Modality::Image);
        assert_eq!(mime_to_modality("IMAGE/PNG"), Modality::Image);
        assert_eq!(mime_to_modality("video/mp4; codecs=avc1"), Modality::Video);
        assert_eq!(mime_to_modality("application/pdf"), Modality::Pdf);
        assert_eq!(mime_to_modality("audio/ogg"), Modality::Audio);
        assert_eq!(mime_to_modality("text/plain"), Modality::Text);
        assert_eq!(mime_to_modality("application/octet-stream"), Modality::Text);
    }

    #[test]
    fn text_modality_always_allowed_even_when_absent() {
        let text_only = InputModalities::Known(vec![Modality::Text]);
        assert!(text_only.allows("text/plain"));
        assert!(!text_only.allows("image/png"));
        let unknown = InputModalities::Unknown;
        assert!(unknown.allows("image/png"));
        assert!(unknown.allows("audio/ogg"));
    }

    #[test]
    fn gates_replace_denied_images_with_a_note() {
        let modalities = InputModalities::Known(vec![Modality::Text]);
        let messages = gate_request_messages(
            vec![
                Message::text(Role::User, "go"),
                tool_message(vec![image("image/png"), image("image/jpeg")]),
            ],
            &modalities,
        );
        assert!(images_of(&messages[1]).is_empty());
        assert_eq!(
            text_of(&messages[1]),
            "screenshot attached\n[2 image attachments omitted: this model does not accept image input]"
        );
    }

    #[test]
    fn gates_keep_allowed_images_and_touch_nothing_when_unknown() {
        let multimodal = InputModalities::Known(vec![Modality::Text, Modality::Image]);
        let messages =
            gate_request_messages(vec![tool_message(vec![image("image/png")])], &multimodal);
        assert_eq!(images_of(&messages[0]).len(), 1);
        assert_eq!(text_of(&messages[0]), "screenshot attached");

        let unknown = InputModalities::Unknown;
        let messages = gate_request_messages(
            vec![tool_message(vec![image("image/png"), image("video/mp4")])],
            &unknown,
        );
        assert_eq!(images_of(&messages[0]).len(), 2);
        assert_eq!(text_of(&messages[0]), "screenshot attached");
    }

    #[test]
    fn gates_drop_only_the_denied_ones() {
        // image 允许、video 不允许：视频图片目前不存在于 ToolResult，但
        // 门控按模态逐张判定，未来扩展无需改结构
        let no_video = InputModalities::Known(vec![Modality::Text, Modality::Image]);
        let messages = gate_request_messages(
            vec![tool_message(vec![image("image/png"), image("video/mp4")])],
            &no_video,
        );
        assert_eq!(images_of(&messages[0]).len(), 1);
        assert_eq!(images_of(&messages[0])[0].mime_type, "image/png");
        assert_eq!(
            text_of(&messages[0]),
            "screenshot attached\n[1 image attachment omitted: this model does not accept image input]"
        );
    }

    fn user_with_images(urls: Vec<&str>) -> Message {
        Message {
            role: Role::User,
            parts: urls
                .into_iter()
                .map(|url| ContentPart::ImageUrl {
                    url: url.to_string(),
                })
                .chain(std::iter::once(ContentPart::Text("look".to_string())))
                .collect(),
        }
    }

    #[test]
    fn gates_user_message_images_by_data_url_mime() {
        let text_only = InputModalities::Known(vec![Modality::Text]);
        let messages = gate_request_messages(
            vec![user_with_images(vec![
                "data:image/png;base64,AA",
                "data:image/jpeg;base64,BB",
            ])],
            &text_only,
        );
        let parts = &messages[0].parts;
        assert_eq!(parts.len(), 3);
        assert!(matches!(&parts[0], ContentPart::Text(t) if t.contains("[image omitted")));
        assert!(matches!(&parts[1], ContentPart::Text(t) if t.contains("[image omitted")));
        assert!(matches!(&parts[2], ContentPart::Text(t) if t == "look"));
    }

    #[test]
    fn gates_image_references_and_hosted_urls_before_hydration() {
        let image_ok = InputModalities::Known(vec![Modality::Text, Modality::Image]);
        let messages = gate_request_messages(
            vec![user_with_images(vec!["data:image/png;base64,AA"])],
            &image_ok,
        );
        assert!(matches!(
            &messages[0].parts[0],
            ContentPart::ImageUrl { .. }
        ));

        // 未水合的存储引用与外链也都是 ImageUrl，可在读盘前按图片门控。
        let text_only = InputModalities::Known(vec![Modality::Text]);
        let messages = gate_request_messages(
            vec![user_with_images(vec![
                "oct-attachment://abc",
                "https://example.com/a.png",
            ])],
            &text_only,
        );
        assert!(matches!(
            &messages[0].parts[0],
            ContentPart::Text(note) if note.contains("[image omitted")
        ));
        assert!(matches!(
            &messages[0].parts[1],
            ContentPart::Text(note) if note.contains("[image omitted")
        ));
    }
}
