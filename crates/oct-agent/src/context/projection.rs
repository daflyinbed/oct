//! Pure media selection. The same projected messages drive request assembly,
//! token estimates and durable usage provenance; this module performs no IO.
use super::{estimate_message, tool_result_text};
use crate::attachment_types::parse_attachment_id;
use crate::modality::{InputModalities, gate_request_messages};
use oct_llm_provider::core::{ContentPart, Message, Role};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

pub const PROJECTION_VERSION: u32 = 1;
const BUDGET_NOTE: &str = "[image omitted: request media budget exceeded]";

#[derive(Debug, Clone, Copy)]
pub struct MediaPolicy {
    pub max_encoded_bytes: u64,
    pub max_images: usize,
}
impl Default for MediaPolicy {
    fn default() -> Self {
        Self {
            max_encoded_bytes: 40 * 1024 * 1024,
            max_images: 64,
        }
    }
}

/// Metadata suffices to select images before any file/base64 allocation.
#[derive(Debug, Clone)]
pub struct MediaAsset {
    pub id: String,
    pub mime_type: String,
    pub size_bytes: u64,
    pub variant: String,
}
impl MediaAsset {
    fn encoded_bytes(&self) -> u64 {
        self.size_bytes.div_ceil(3).saturating_mul(4) + self.mime_type.len() as u64 + 13
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectedEntry {
    pub ordering: i64,
    pub fingerprint: String,
    pub estimated_tokens: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionSnapshot {
    pub version: u32,
    pub system_fingerprint: String,
    pub entries: Vec<ProjectedEntry>,
}
impl ProjectionSnapshot {
    /// Appending messages may extend a request; only the anchor's measured
    /// prefix is compared. Policy changes with identical output remain valid.
    pub fn accepts_prefix(&self, current: &Self) -> bool {
        self.version == current.version
            && self.system_fingerprint == current.system_fingerprint
            && self.entries.len() <= current.entries.len()
            && self.entries.iter().zip(&current.entries).all(|(old, new)| {
                old.ordering == new.ordering && old.fingerprint == new.fingerprint
            })
    }
    pub fn entry(&self, ordering: i64) -> Option<&ProjectedEntry> {
        self.entries.iter().find(|entry| entry.ordering == ordering)
    }
    /// A declared COW clear has a known pure projection and preserves the
    /// anchor after its actual measured savings have been deducted.
    pub fn clear(&mut self, ordering: i64, message: &Message) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.ordering == ordering)
        {
            entry.fingerprint = fingerprint(message, &HashMap::new());
            entry.estimated_tokens = estimate_message(message);
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error(
    "当前消息及工具轮的图片超过请求预算（最多 {max_images} 张、{max_bytes} 编码字节）；请减少附件或更换媒体策略"
)]
pub struct MediaBudgetError {
    pub max_images: usize,
    pub max_bytes: u64,
}

pub struct ProjectionPlan {
    pub messages: Vec<Message>,
    pub snapshot: ProjectionSnapshot,
    pub encoded_bytes: u64,
    pub image_count: usize,
    orderings: Vec<i64>,
    assets: HashMap<String, MediaAsset>,
}

#[derive(Clone, Copy)]
struct Candidate {
    message: usize,
    part: usize,
    image: Option<usize>,
    bytes: u64,
}

impl ProjectionPlan {
    pub fn build(
        messages: Vec<Message>,
        orderings: &[i64],
        modalities: &InputModalities,
        assets: HashMap<String, MediaAsset>,
        missing: &HashSet<String>,
        policy: MediaPolicy,
    ) -> Result<Self, MediaBudgetError> {
        let mut messages = gate_request_messages(messages, modalities);
        omit_unavailable(&mut messages, missing);
        let mut candidates = Vec::new();
        for (mi, message) in messages.iter().enumerate() {
            for (pi, part) in message.parts.iter().enumerate() {
                match part {
                    ContentPart::ImageUrl { url } => candidates.push(Candidate {
                        message: mi,
                        part: pi,
                        image: None,
                        bytes: media_bytes(url, &assets),
                    }),
                    ContentPart::ToolResult(result) => {
                        for (ii, image) in result.images.iter().enumerate() {
                            candidates.push(Candidate {
                                message: mi,
                                part: pi,
                                image: Some(ii),
                                bytes: parse_attachment_id(&image.data)
                                    .and_then(|id| assets.get(id))
                                    .map_or(
                                        image.data.len() as u64 + image.mime_type.len() as u64 + 13,
                                        MediaAsset::encoded_bytes,
                                    ),
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
        let latest_user = messages
            .iter()
            .rposition(|message| message.role == Role::User);
        // Only the latest tool exchange is mandatory. Earlier tool rounds in
        // a long-running user turn must remain eligible for budget eviction.
        let mut latest_round = messages.len();
        while latest_round > 0 && messages[latest_round - 1].role == Role::Tool {
            latest_round -= 1;
        }
        if latest_round < messages.len()
            && latest_round > 0
            && messages[latest_round - 1].role == Role::Assistant
        {
            latest_round -= 1;
        }
        let is_protected = |image: &&Candidate| {
            Some(image.message) == latest_user || image.message >= latest_round
        };
        let protected: Vec<_> = candidates.iter().filter(is_protected).collect();
        let mut bytes = protected.iter().map(|image| image.bytes).sum::<u64>();
        let mut count = protected.len();
        if bytes > policy.max_encoded_bytes || count > policy.max_images {
            return Err(MediaBudgetError {
                max_images: policy.max_images,
                max_bytes: policy.max_encoded_bytes,
            });
        }
        let mut omitted = HashSet::new();
        for image in candidates.iter().rev().filter(|image| !is_protected(image)) {
            if count < policy.max_images
                && image.bytes <= policy.max_encoded_bytes.saturating_sub(bytes)
            {
                bytes += image.bytes;
                count += 1;
            } else {
                omitted.insert((image.message, image.part, image.image));
            }
        }
        for (mi, message) in messages.iter_mut().enumerate() {
            for (pi, part) in message.parts.iter_mut().enumerate() {
                match part {
                    ContentPart::ImageUrl { .. } if omitted.contains(&(mi, pi, None)) => {
                        *part = ContentPart::Text(BUDGET_NOTE.into())
                    }
                    ContentPart::ToolResult(result) => {
                        let mut index = 0;
                        let before = result.images.len();
                        result.images.retain(|_| {
                            let keep = !omitted.contains(&(mi, pi, Some(index)));
                            index += 1;
                            keep
                        });
                        let dropped = before - result.images.len();
                        if dropped > 0 {
                            result.content = serde_json::json!(format!(
                                "{}\n[{dropped} image attachments omitted: request media budget exceeded]",
                                tool_result_text(&result.content)
                            ));
                        }
                    }
                    _ => {}
                }
            }
        }
        let snapshot = snapshot(&messages, orderings, &assets);
        Ok(Self {
            messages,
            snapshot,
            encoded_bytes: bytes,
            image_count: count,
            orderings: orderings.into(),
            assets,
        })
    }
    /// IO failures change the final request, so they also change the usage
    /// stamp/estimate. Previously evicted images are not silently backfilled.
    pub fn omit_unavailable(&mut self, missing: &HashSet<String>) {
        omit_unavailable(&mut self.messages, missing);
        self.snapshot = snapshot(&self.messages, &self.orderings, &self.assets);
    }
}

fn media_bytes(url: &str, assets: &HashMap<String, MediaAsset>) -> u64 {
    parse_attachment_id(url)
        .and_then(|id| assets.get(id))
        .map_or_else(
            || {
                if url.starts_with("data:") {
                    url.len() as u64
                } else {
                    0
                }
            },
            MediaAsset::encoded_bytes,
        )
}

fn snapshot(
    messages: &[Message],
    orderings: &[i64],
    assets: &HashMap<String, MediaAsset>,
) -> ProjectionSnapshot {
    ProjectionSnapshot {
        version: PROJECTION_VERSION,
        system_fingerprint: messages
            .first()
            .map(|message| fingerprint(message, assets))
            .unwrap_or_default(),
        entries: messages
            .iter()
            .skip(1)
            .zip(orderings)
            .map(|(message, ordering)| ProjectedEntry {
                ordering: *ordering,
                fingerprint: fingerprint(message, assets),
                estimated_tokens: estimate_message(message),
            })
            .collect(),
    }
}

fn fingerprint(message: &Message, assets: &HashMap<String, MediaAsset>) -> String {
    let mut hash = Sha256::new();
    hash.update(serde_json::to_vec(message).expect("message serializes"));
    // Include the selected immutable derivative, not just the public asset id.
    for part in &message.parts {
        let references: Vec<&str> = match part {
            ContentPart::ImageUrl { url } => vec![url],
            ContentPart::ToolResult(result) => result
                .images
                .iter()
                .map(|image| image.data.as_str())
                .collect(),
            _ => Vec::new(),
        };
        for reference in references {
            if let Some(asset) = parse_attachment_id(reference).and_then(|id| assets.get(id)) {
                hash.update(asset.variant.as_bytes());
            }
        }
    }
    hash.finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(crate) fn omit_unavailable(messages: &mut [Message], missing: &HashSet<String>) {
    for message in messages {
        for part in &mut message.parts {
            match part {
                ContentPart::ImageUrl { url } => {
                    if let Some(id) = parse_attachment_id(url).filter(|id| missing.contains(*id)) {
                        *part = ContentPart::Text(format!(
                            "[image attachment omitted: file for {id} could not be read]"
                        ));
                    }
                }
                ContentPart::ToolResult(result) => {
                    let before = result.images.len();
                    result.images.retain(|image| {
                        !parse_attachment_id(&image.data).is_some_and(|id| missing.contains(id))
                    });
                    let dropped = before - result.images.len();
                    if dropped > 0 {
                        result.content = serde_json::json!(format!(
                            "{}\n[{dropped} image attachments omitted: files could not be read]",
                            tool_result_text(&result.content)
                        ));
                    }
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{ContextState, LlmView};
    use crate::modality::Modality;
    fn image(id: &str) -> Message {
        Message {
            role: Role::User,
            parts: vec![ContentPart::ImageUrl {
                url: format!("oct-attachment://{id}"),
            }],
        }
    }
    fn metadata() -> HashMap<String, MediaAsset> {
        ["a", "b", "c"]
            .into_iter()
            .map(|id| {
                (
                    id.into(),
                    MediaAsset {
                        id: id.into(),
                        mime_type: "image/png".into(),
                        size_bytes: 3,
                        variant: "1".into(),
                    },
                )
            })
            .collect()
    }
    fn system() -> Message {
        Message::text(Role::System, "system")
    }
    #[test]
    fn reserves_latest_turn_and_evicts_oldest_media_by_encoded_bytes() {
        let messages = vec![system(), image("a"), image("b"), image("c")];
        let per_image = metadata()["a"].encoded_bytes();
        let plan = ProjectionPlan::build(
            messages,
            &[1, 2, 3],
            &InputModalities::Unknown,
            metadata(),
            &HashSet::new(),
            MediaPolicy {
                max_encoded_bytes: per_image * 2,
                max_images: 10,
            },
        )
        .unwrap();
        assert_eq!(plan.image_count, 2);
        assert_eq!(plan.encoded_bytes, per_image * 2);
        assert!(
            matches!(&plan.messages[1].parts[0],ContentPart::Text(note) if note.contains("budget"))
        );
        assert!(matches!(
            &plan.messages[3].parts[0],
            ContentPart::ImageUrl { .. }
        ));
        assert_eq!(
            plan.snapshot.entry(1).unwrap().estimated_tokens,
            estimate_message(&plan.messages[1])
        );
    }
    #[test]
    fn rejects_a_protected_current_turn_instead_of_silently_losing_its_images() {
        let messages = vec![
            system(),
            Message {
                role: Role::User,
                parts: vec![
                    ContentPart::ImageUrl {
                        url: "data:image/png;base64,AAAA".into()
                    };
                    2
                ],
            },
        ];
        assert!(
            ProjectionPlan::build(
                messages,
                &[1],
                &InputModalities::Unknown,
                HashMap::new(),
                &HashSet::new(),
                MediaPolicy {
                    max_encoded_bytes: 1,
                    max_images: 1
                }
            )
            .is_err()
        );
    }
    #[test]
    fn earlier_tool_rounds_in_the_current_user_turn_can_be_evicted() {
        let tool = |id: &str| Message {
            role: Role::Tool,
            parts: vec![ContentPart::ToolResult(
                oct_llm_provider::core::ToolResult {
                    call_id: id.into(),
                    content: serde_json::json!("tool text"),
                    is_error: false,
                    images: vec![oct_llm_provider::core::ToolResultImage {
                        mime_type: "image/png".into(),
                        data: format!("oct-attachment://{id}"),
                    }],
                },
            )],
        };
        let plan = ProjectionPlan::build(
            vec![
                system(),
                image("a"),
                Message::text(Role::Assistant, "first call"),
                tool("b"),
                Message::text(Role::Assistant, "second call"),
                tool("c"),
            ],
            &[1, 2, 3, 4, 5],
            &InputModalities::Unknown,
            metadata(),
            &HashSet::new(),
            MediaPolicy {
                max_encoded_bytes: u64::MAX,
                max_images: 2,
            },
        )
        .unwrap();
        assert_eq!(plan.image_count, 2);
        assert!(matches!(
            &plan.messages[1].parts[0],
            ContentPart::ImageUrl { .. }
        ));
        assert!(
            matches!(&plan.messages[3].parts[0],ContentPart::ToolResult(result) if result.images.is_empty() && tool_result_text(&result.content).contains("budget"))
        );
        assert!(
            matches!(&plan.messages[5].parts[0],ContentPart::ToolResult(result) if result.images.len() == 1)
        );
    }
    #[test]
    fn text_only_projection_requires_neither_image_bytes_nor_media_budget() {
        let messages = vec![system(), image("a")];
        let plan = ProjectionPlan::build(
            messages,
            &[1],
            &InputModalities::Known(vec![Modality::Text]),
            HashMap::new(),
            &HashSet::new(),
            MediaPolicy {
                max_encoded_bytes: 0,
                max_images: 0,
            },
        )
        .unwrap();
        assert_eq!(plan.image_count, 0);
        assert!(
            matches!(&plan.messages[1].parts[0],ContentPart::Text(note) if note.contains("does not accept"))
        );
    }
    #[test]
    fn usage_prefix_survives_append_but_not_budget_eviction_or_missing_files() {
        let first = ProjectionPlan::build(
            vec![system(), image("a")],
            &[1],
            &InputModalities::Unknown,
            metadata(),
            &HashSet::new(),
            MediaPolicy::default(),
        )
        .unwrap();
        let second = ProjectionPlan::build(
            vec![system(), image("a"), image("b")],
            &[1, 2],
            &InputModalities::Unknown,
            metadata(),
            &HashSet::new(),
            MediaPolicy::default(),
        )
        .unwrap();
        assert!(first.snapshot.accepts_prefix(&second.snapshot));
        let evicted = ProjectionPlan::build(
            vec![system(), image("a"), image("b")],
            &[1, 2],
            &InputModalities::Unknown,
            metadata(),
            &HashSet::new(),
            MediaPolicy {
                max_encoded_bytes: u64::MAX,
                max_images: 1,
            },
        )
        .unwrap();
        assert!(!first.snapshot.accepts_prefix(&evicted.snapshot));
        let mut missing = second;
        missing.omit_unavailable(&HashSet::from(["a".into()]));
        assert!(!first.snapshot.accepts_prefix(&missing.snapshot));
    }
    #[test]
    fn context_estimates_the_selected_projection_and_discards_changed_anchors() {
        let mut context = ContextState::new(
            "system".into(),
            LlmView {
                messages: vec![image("a"), image("b")],
                orderings: vec![1, 2],
                anchor: None,
            },
        );
        let plan = ProjectionPlan::build(
            context.messages_for_request(),
            &[1, 2],
            &InputModalities::Unknown,
            metadata(),
            &HashSet::new(),
            MediaPolicy {
                max_encoded_bytes: u64::MAX,
                max_images: 1,
            },
        )
        .unwrap();
        let expected = plan.messages.iter().map(estimate_message).sum::<u64>();
        context.apply_projection(plan.snapshot);
        assert_eq!(context.estimate(&[]), expected);
        context.append(Message::text(Role::Assistant, "reply"), 3);
        context.set_anchor(700, 1);
        let expanded = ProjectionPlan::build(
            context.messages_for_request(),
            &[1, 2, 3],
            &InputModalities::Unknown,
            metadata(),
            &HashSet::new(),
            MediaPolicy::default(),
        )
        .unwrap();
        context.apply_projection(expanded.snapshot);
        assert!(context.anchor().is_none());
    }
    #[test]
    fn legacy_usage_without_a_projection_cannot_anchor_a_new_request() {
        let mut context = ContextState::new(
            "system".into(),
            LlmView {
                messages: vec![
                    Message::text(Role::User, "old"),
                    Message::text(Role::Assistant, "answer"),
                ],
                orderings: vec![1, 2],
                anchor: None,
            },
        );
        context.set_anchor(9999, 1);
        assert!(context.anchor().unwrap().projection.is_none());
        let plan = ProjectionPlan::build(
            context.messages_for_request(),
            &[1, 2],
            &InputModalities::Unknown,
            HashMap::new(),
            &HashSet::new(),
            MediaPolicy::default(),
        )
        .unwrap();
        context.apply_projection(plan.snapshot);
        assert!(context.anchor().is_none());
        assert!(context.estimate(&[]) < 9999);
    }
    #[test]
    fn cow_clear_only_deducts_media_that_the_anchored_request_actually_included() {
        let tool = Message {
            role: Role::Tool,
            parts: vec![ContentPart::ToolResult(
                oct_llm_provider::core::ToolResult {
                    call_id: "call".into(),
                    content: serde_json::json!("tool output ".repeat(100)),
                    is_error: false,
                    images: vec![oct_llm_provider::core::ToolResultImage {
                        mime_type: "image/png".into(),
                        data: "oct-attachment://a".into(),
                    }],
                },
            )],
        };
        let mut context = ContextState::new(
            "system".into(),
            LlmView {
                messages: vec![tool.clone(), Message::text(Role::Assistant, "answer")],
                orderings: vec![1, 2],
                anchor: None,
            },
        );
        let policy = MediaPolicy {
            max_encoded_bytes: u64::MAX,
            max_images: 0,
        };
        let plan = ProjectionPlan::build(
            context.messages_for_request(),
            &[1, 2],
            &InputModalities::Unknown,
            metadata(),
            &HashSet::new(),
            policy,
        )
        .unwrap();
        let measured = plan.snapshot.entry(1).unwrap().estimated_tokens;
        assert!(measured < estimate_message(&tool));
        context.apply_projection(plan.snapshot);
        context.set_anchor(10000, 1);
        context.apply_clears(&[(0, estimate_message(&tool))], "[cleared]");
        let after = estimate_message(&context.conversation()[0].message);
        assert_eq!(
            context.anchor().unwrap().tokens,
            10000 - measured.saturating_sub(after)
        );
        let replay = ProjectionPlan::build(
            context.messages_for_request(),
            &[1, 2],
            &InputModalities::Unknown,
            metadata(),
            &HashSet::new(),
            policy,
        )
        .unwrap();
        context.apply_projection(replay.snapshot);
        assert!(
            context.anchor().is_some(),
            "declared COW mutation preserves the adjusted prefix"
        );
    }
}
