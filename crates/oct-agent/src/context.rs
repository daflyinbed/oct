//! Conversation context state and the pure rules shared by the persistence
//! layer and the agent loop.
//!
//! This module is the neutral layer between `db::messages` (which owns reads
//! and writes and provides raw snapshots) and `agent::*` (which owns trigger
//! policies and events): token estimation, compaction-marker / COW-copy
//! recognition, the usage-anchor policy, and [`ContextState`] — the loop's
//! mutable view of a conversation (system prompt + ordered entries + anchor).
//! It has no database access and no event side effects; every transformation
//! (`append`, `apply_compaction`, `apply_clears`) keeps the in-run state and
//! the next [`LlmView`] reload in lockstep, so both sides compute the same
//! effective messages, orderings and anchor.

pub mod projection;

use projection::ProjectionSnapshot;
use serde_json::Value;

use crate::modality::{self, InputModalities};
use oct_llm_provider::core::{Message, Role, ToolSpec};

// ======================================================================
// Token estimation (moved from agent::compact; the DB layer's COW anchor
// deduction needs it, and db → agent was a reverse dependency)
// ======================================================================

/// Rough token estimate for a string: ASCII chars ÷ 4, non-ASCII chars × 1.
/// Over-estimates CJK slightly (one char is often < 1 token), which errs on
/// the safe side for a trigger that wants to fire early.
pub fn estimate_str(s: &str) -> u64 {
    let mut ascii = 0u64;
    let mut non_ascii = 0u64;
    for c in s.chars() {
        if c.is_ascii() {
            ascii += 1;
        } else {
            non_ascii += 1;
        }
    }
    ascii / 4 + non_ascii
}

/// Per-message fixed overhead (role/framing tokens every provider charges).
pub const PER_MESSAGE_OVERHEAD_TOKENS: u64 = 4;
/// Per tool-call/result pairing overhead.
pub const PER_TOOL_PART_OVERHEAD_TOKENS: u64 = 8;

/// Flat per-image estimate for user attachments and tool results. Base64
/// length ÷ 4 would wildly over-count (a 1 MB image is
/// ~343k "tokens" by chars); the real cost of an image to a provider scales
/// with its pixels, so a flat value in the oh-my-pi / ZCode ballpark is the
/// honest heuristic.
pub const IMAGE_ESTIMATE_TOKENS: u64 = 1_200;
/// Compatibility name for callers estimating tool-result images.
pub const TOOL_IMAGE_ESTIMATE_TOKENS: u64 = IMAGE_ESTIMATE_TOKENS;

pub fn estimate_message(msg: &Message) -> u64 {
    estimate_message_with_modalities(msg, &InputModalities::Unknown)
}

/// Estimate the request-side projection without hydrating references or
/// cloning base64 bytes. The persisted parts remain untouched.
pub fn estimate_message_with_modalities(msg: &Message, modalities: &InputModalities) -> u64 {
    PER_MESSAGE_OVERHEAD_TOKENS
        + msg
            .parts
            .iter()
            .map(|part| estimate_part(part, modalities))
            .sum::<u64>()
}

fn estimate_part(part: &oct_llm_provider::core::ContentPart, modalities: &InputModalities) -> u64 {
    use oct_llm_provider::core::ContentPart;
    match part {
        ContentPart::Text(s) | ContentPart::Reasoning(s) => estimate_str(s),
        ContentPart::ReasoningBlock(block) => match block {
            oct_llm_provider::core::ReasoningBlock::Signed {
                text, signature, ..
            } => estimate_str(text) + estimate_str(signature),
            oct_llm_provider::core::ReasoningBlock::Redacted { data, .. } => estimate_str(data),
        },
        // Lightweight attachment refs, hosted URLs and hydrated data URLs
        // all represent images with the same budget, independent of URL length.
        ContentPart::ImageUrl { url } => modality::omitted_image_note(url, modalities)
            .map(estimate_str)
            .unwrap_or(IMAGE_ESTIMATE_TOKENS),
        ContentPart::ToolCall(tc) => {
            PER_TOOL_PART_OVERHEAD_TOKENS + estimate_str(&tc.name) + estimate_str(&tc.arguments)
        }
        ContentPart::ToolResult(tr) => {
            let content = modality::gated_tool_result_content(tr, modalities)
                .unwrap_or_else(|| tool_result_text(&tr.content));
            PER_TOOL_PART_OVERHEAD_TOKENS
                + estimate_str(&content)
                + tr.images
                    .iter()
                    .filter(|image| modalities.allows(&image.mime_type))
                    .count() as u64
                    * IMAGE_ESTIMATE_TOKENS
        }
    }
}

pub fn estimate_messages(messages: &[Message]) -> u64 {
    messages.iter().map(estimate_message).sum()
}

/// Tool schemas ride along on every request but are invisible to
/// message-level estimation — needed only on the no-anchor path (see
/// [`estimate_context`]).
pub fn estimate_tool_specs(specs: &[ToolSpec]) -> u64 {
    specs
        .iter()
        .map(|s| {
            16 + estimate_str(&s.name)
                + s.description.as_deref().map(estimate_str).unwrap_or(0)
                + estimate_str(&s.input_schema.to_string())
        })
        .sum()
}

/// Estimated context size of the NEXT request, for a messages array that
/// INCLUDES the system prompt at index 0.
///
/// * With an anchor — the provider-reported `input + output` of the request
///   that produced the anchor message — the estimate is anchor tokens plus a
///   local estimate of everything appended after that message. The system
///   prompt and tool schemas were already counted by the provider inside the
///   anchor's input tokens and must NOT be added again.
/// * Without an anchor (nothing in the view has usage yet, e.g. right after a
///   compaction) the full local estimate must explicitly include the system
///   prompt and tool schemas, or it would systematically under-count.
pub fn estimate_context(
    messages: &[Message],
    tool_specs: &[ToolSpec],
    anchor: Option<(u64, usize)>,
) -> u64 {
    match anchor {
        Some((tokens, idx)) if idx < messages.len() => {
            tokens + estimate_messages(&messages[idx + 1..])
        }
        _ => estimate_messages(messages) + estimate_tool_specs(tool_specs),
    }
}

/// The text payload of a tool result for estimation/serialization: string
/// contents pass through, anything else renders as its compact JSON.
pub fn tool_result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

// ======================================================================
// Marker / COW-copy recognition (details_json kinds)
// ======================================================================

/// details_json `kind` value marking a compaction marker row (see
/// `agent::compact`): a normal `user` message whose parts carry the summary
/// and whose details carry compaction metadata. Rows before the LAST marker
/// stay in the DB but never reach the LLM again.
pub const COMPACTION_MARKER_KIND: &str = "compaction";

/// The `kind` field of a row's details_json, when it parses. UI-only
/// metadata never enters the model-visible parts.
pub fn details_kind(details_json: Option<&str>) -> Option<String> {
    details_json
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .and_then(|v| v.get("kind").and_then(Value::as_str).map(str::to_string))
}

// ======================================================================
// The LLM view snapshot (moved from db::messages; re-exported there)
// ======================================================================

/// Provider-truth anchor for context estimates: the `input + output` tokens
/// of the request that produced the anchor message, plus that message's index
/// in the LLM view. Everything appended after it has never been counted by a
/// provider and must be estimated locally.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageAnchor {
    pub tokens: u64,
    pub message_index: usize,
    /// Identity of the prompt environment the anchor's request RAN under
    /// (stamped from the run's context at commit; persisted on the assistant
    /// row's details so later runs can compare against their own entry).
    /// `0` = unknown (rows predating the feature). An anchor from a
    /// different generation was measured under a different system prompt and
    /// must not drive the anchored estimate, which excludes the prompt.
    pub prompt_generation: u64,
    /// Snapshot of the input gate that produced these provider tokens. COW
    /// replay must not charge savings for images that request never included.
    pub input_modalities: InputModalities,
    pub projection: Option<ProjectionSnapshot>,
}

/// The model-visible view of a conversation plus the estimation anchor:
/// `[last compaction marker (if any), everything after it]` with orphan tool
/// results filtered out. System prompt is NOT included — the loop prepends it
/// fresh on every run. `orderings` is parallel to `messages` and carries each
/// message's timeline ordering (compaction consumes it to place its marker).
#[derive(Debug, Clone)]
pub struct LlmView {
    pub messages: Vec<Message>,
    pub orderings: Vec<i64>,
    pub anchor: Option<UsageAnchor>,
}

// ======================================================================
// Context state (the loop's in-run view)
// ======================================================================

/// One model-visible message at its timeline ordering — the unit the loop's
/// context state tracks, keeping the message list and the DB orderings
/// parallel by construction instead of via a second vector.
#[derive(Debug, Clone)]
pub struct ContextEntry {
    pub message: Message,
    pub ordering: i64,
}

/// The agent loop's mutable view of a conversation: the system prompt,
/// ordered conversation entries (system prompt NOT among them), and the
/// usage anchor (index into the entries).
///
/// Every transformation mirrors what the DB layer does in the same step, so
/// the in-run state and a fresh `load_llm_view` agree on the effective
/// messages, orderings and anchor:
/// * [`append`](Self::append) — one persisted row at its returned ordering;
/// * [`apply_compaction`](Self::apply_compaction) — the marker takes over the
///   cut slot, the tail shifts +1 (mirroring `insert_message_at_ordering`),
///   the anchor resets to `None`;
/// * [`apply_clears`](Self::apply_clears) — COW placeholder rewrite plus the
///   pre-anchor savings deduction, the same rule `load_llm_view` recomputes
///   from `swapped_at_revision` on every load.
#[derive(Debug, Clone)]
pub struct ContextState {
    system_prompt: String,
    entries: Vec<ContextEntry>,
    anchor: Option<UsageAnchor>,
    input_modalities: InputModalities,
    projection: Option<ProjectionSnapshot>,
}

impl ContextState {
    pub fn new(system_prompt: String, view: LlmView) -> Self {
        let input_modalities = view
            .anchor
            .as_ref()
            .map(|anchor| anchor.input_modalities.clone())
            .unwrap_or(InputModalities::Unknown);
        Self {
            system_prompt,
            entries: view
                .messages
                .into_iter()
                .zip(view.orderings)
                .map(|(message, ordering)| ContextEntry { message, ordering })
                .collect(),
            anchor: view.anchor,
            input_modalities,
            projection: None,
        }
    }

    /// A changed input gate changes the request's prefix just as a changed
    /// prompt does; its old usage cannot account for the new projection.
    pub fn with_input_modalities(mut self, modalities: InputModalities) -> Self {
        if self
            .anchor
            .as_ref()
            .is_some_and(|anchor| anchor.input_modalities != modalities)
        {
            self.anchor = None;
        }
        self.input_modalities = modalities;
        self
    }

    /// The final request projection is also the estimator's source of truth.
    /// An unmeasured/changed prefix cannot borrow an older provider usage.
    pub fn apply_projection(&mut self, projection: ProjectionSnapshot) {
        if self.anchor.as_ref().is_some_and(|anchor| {
            anchor
                .projection
                .as_ref()
                .is_none_or(|old| !old.accepts_prefix(&projection))
        }) {
            self.anchor = None;
        }
        self.projection = Some(projection);
    }

    pub fn projection(&self) -> Option<&ProjectionSnapshot> {
        self.projection.as_ref()
    }

    pub fn input_modalities(&self) -> &InputModalities {
        &self.input_modalities
    }

    pub fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    /// The conversation entries (no system prompt); indices here are the
    /// indices selection functions return.
    pub fn conversation(&self) -> &[ContextEntry] {
        &self.entries
    }

    pub fn anchor(&self) -> Option<&UsageAnchor> {
        self.anchor.as_ref()
    }

    /// The messages of the next LLM request: system prompt first, then the
    /// conversation entries in order.
    pub fn messages_for_request(&self) -> Vec<Message> {
        std::iter::once(Message::text(Role::System, &self.system_prompt))
            .chain(self.entries.iter().map(|e| e.message.clone()))
            .collect()
    }

    /// Estimated context of the next request (see [`estimate_view`]).
    pub fn estimate(&self, tool_specs: &[ToolSpec]) -> u64 {
        if let Some(projection) = &self.projection {
            let cost = |entry: &ContextEntry| {
                projection
                    .entry(entry.ordering)
                    .map(|entry| entry.estimated_tokens)
                    .unwrap_or_else(|| {
                        estimate_message_with_modalities(&entry.message, &self.input_modalities)
                    })
            };
            return match &self.anchor {
                Some(anchor) if anchor.message_index < self.entries.len() => {
                    anchor.tokens
                        + self.entries[anchor.message_index + 1..]
                            .iter()
                            .map(cost)
                            .sum::<u64>()
                }
                _ => {
                    PER_MESSAGE_OVERHEAD_TOKENS
                        + estimate_str(&self.system_prompt)
                        + estimate_tool_specs(tool_specs)
                        + self.entries.iter().map(cost).sum::<u64>()
                }
            };
        }
        estimate_view_with_modalities(
            &self.entries,
            Some(&self.system_prompt),
            tool_specs,
            self.anchor.as_ref(),
            &self.input_modalities,
        )
    }

    /// Append one persisted message at its timeline ordering.
    pub fn append(&mut self, message: Message, ordering: i64) {
        self.entries.push(ContextEntry { message, ordering });
    }

    /// The ordering `insert_message` would assign next — the degraded-append
    /// fallback when a persist failed (keeps the entries parallel with the
    /// DB instead of a blind last+1, which could desync a later compaction
    /// marker's slot).
    pub fn next_ordering_fallback(&self) -> i64 {
        self.entries.last().map(|e| e.ordering).unwrap_or_default() + 1
    }

    /// Commit a compaction: the rebuilt entries (marker at the cut slot, tail
    /// shifted +1) replace the conversation, and the anchor resets — no
    /// provider has ever seen the rebuilt history, so estimates fall back to
    /// the (schema-aware) local path until the next real usage arrives.
    pub fn apply_compaction(&mut self, rebuilt: Vec<ContextEntry>) {
        self.entries = rebuilt;
        self.anchor = None;
        self.projection = None;
    }

    /// Commit this request's reported usage as the anchor on the LAST entry
    /// (the assistant message the loop just appended). Entry-relative index,
    /// matching `load_llm_view`'s anchor coordinates. `prompt_generation`
    /// comes from the run's context: this request ran under THAT
    /// environment's system prompt, whatever the cache does later.
    pub fn set_anchor(&mut self, tokens: u64, prompt_generation: u64) {
        if let Some(index) = self.entries.len().checked_sub(1) {
            self.anchor = Some(UsageAnchor {
                tokens,
                message_index: index,
                prompt_generation,
                input_modalities: self.input_modalities.clone(),
                projection: self.projection.clone(),
            });
        }
    }

    /// Rewrite the tool-result parts of the given entries to the placeholder
    /// content (images cleared) and deduct its request-side saving from the anchor when
    /// the entry sat before it: a cleared result counted inside the anchor's
    /// provider tokens must be subtracted, while results after the anchor
    /// only shrink the local increment (automatic once rewritten).
    pub fn apply_clears(&mut self, cleared: &[(usize, u64)], placeholder: &str) {
        for (index, _) in cleared {
            let before = self
                .anchor
                .as_ref()
                .filter(|anchor| *index < anchor.message_index)
                .map(|anchor| {
                    anchor
                        .projection
                        .as_ref()
                        .and_then(|p| p.entry(self.entries[*index].ordering))
                        .map(|entry| entry.estimated_tokens)
                        .unwrap_or_else(|| {
                            estimate_message_with_modalities(
                                &self.entries[*index].message,
                                &anchor.input_modalities,
                            )
                        })
                });
            for part in &mut self.entries[*index].message.parts {
                if let oct_llm_provider::core::ContentPart::ToolResult(tr) = part {
                    tr.content = Value::String(placeholder.to_string());
                    tr.images.clear();
                }
            }
            if let (Some(before), Some(anchor)) = (before, self.anchor.as_mut()) {
                let after = estimate_message_with_modalities(
                    &self.entries[*index].message,
                    &anchor.input_modalities,
                );
                anchor.tokens = anchor.tokens.saturating_sub(before.saturating_sub(after));
                if let Some(projection) = &mut anchor.projection {
                    projection.clear(self.entries[*index].ordering, &self.entries[*index].message);
                }
            }
            if let Some(projection) = &mut self.projection {
                projection.clear(self.entries[*index].ordering, &self.entries[*index].message);
            }
        }
    }
}

/// Estimated context of the next request built from conversation entries.
/// Entry-relative anchor coordinates (as `load_llm_view` produces and
/// [`ContextState`] keeps); `system` participates only on the no-anchor
/// path — with an anchor the system prompt was already counted inside the
/// anchor's input tokens. This is [`estimate_context`] in entry coordinates;
/// keep the two rules in sync.
pub fn estimate_view(
    entries: &[ContextEntry],
    system: Option<&str>,
    tool_specs: &[ToolSpec],
    anchor: Option<&UsageAnchor>,
) -> u64 {
    estimate_view_with_modalities(
        entries,
        system,
        tool_specs,
        anchor,
        anchor
            .map(|a| &a.input_modalities)
            .unwrap_or(&InputModalities::Unknown),
    )
}

fn estimate_view_with_modalities(
    entries: &[ContextEntry],
    system: Option<&str>,
    tool_specs: &[ToolSpec],
    anchor: Option<&UsageAnchor>,
    modalities: &InputModalities,
) -> u64 {
    match anchor {
        Some(a) if a.message_index < entries.len() => {
            let after = entries[a.message_index + 1..]
                .iter()
                .map(|e| estimate_message_with_modalities(&e.message, modalities))
                .sum::<u64>();
            a.tokens + after
        }
        _ => {
            let system_tokens = system
                .map(|s| PER_MESSAGE_OVERHEAD_TOKENS + estimate_str(s))
                .unwrap_or(0);
            let messages: u64 = entries
                .iter()
                .map(|e| estimate_message_with_modalities(&e.message, modalities))
                .sum();
            system_tokens + messages + estimate_tool_specs(tool_specs)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modality::Modality;
    use oct_llm_provider::core::{ContentPart, ToolResult, ToolResultImage};

    #[test]
    fn image_cost_is_independent_of_storage_and_base64_length() {
        for url in [
            "oct-attachment://00000000-0000-0000-0000-000000000000".to_string(),
            "https://example.com/image.png".to_string(),
            format!("data:image/png;base64,{}", "A".repeat(1024 * 1024)),
        ] {
            let message = Message {
                role: Role::User,
                parts: vec![ContentPart::ImageUrl { url }],
            };
            assert_eq!(
                estimate_message(&message),
                PER_MESSAGE_OVERHEAD_TOKENS + IMAGE_ESTIMATE_TOKENS
            );
            let text_only = InputModalities::Known(vec![Modality::Text]);
            let gated = modality::gate_request_messages(vec![message.clone()], &text_only);
            assert_eq!(
                estimate_message_with_modalities(&message, &text_only),
                estimate_message(&gated[0])
            );
        }
    }

    #[test]
    fn tool_image_estimate_matches_the_request_projection() {
        for content in [
            serde_json::json!("screenshot"),
            serde_json::json!({"ok": true}),
        ] {
            let message = Message {
                role: Role::Tool,
                parts: vec![ContentPart::ToolResult(ToolResult {
                    call_id: "c1".into(),
                    content,
                    is_error: false,
                    images: vec![
                        ToolResultImage {
                            mime_type: "image/png".into(),
                            data: "AAAA".into(),
                        },
                        ToolResultImage {
                            mime_type: "video/mp4".into(),
                            data: "BBBB".into(),
                        },
                    ],
                })],
            };
            for modalities in [
                InputModalities::Unknown,
                InputModalities::Known(vec![Modality::Text]),
                InputModalities::Known(vec![Modality::Image]),
            ] {
                let gated = modality::gate_request_messages(vec![message.clone()], &modalities);
                assert_eq!(
                    estimate_message_with_modalities(&message, &modalities),
                    estimate_message(&gated[0])
                );
            }
        }
    }

    #[test]
    fn changing_input_modalities_invalidates_the_old_usage_anchor() {
        let mut state = ContextState::new(
            String::new(),
            LlmView {
                messages: vec![Message {
                    role: Role::User,
                    parts: vec![ContentPart::ImageUrl {
                        url: "oct-attachment://image".into(),
                    }],
                }],
                orderings: vec![1],
                anchor: None,
            },
        )
        .with_input_modalities(InputModalities::Known(vec![Modality::Text]));
        state.set_anchor(100, 7);
        assert_eq!(
            state.anchor().unwrap().input_modalities,
            InputModalities::Known(vec![Modality::Text])
        );
        let state = state.with_input_modalities(InputModalities::Known(vec![
            Modality::Text,
            Modality::Image,
        ]));
        assert!(state.anchor().is_none());
        assert_eq!(
            state.estimate(&[]),
            2 * PER_MESSAGE_OVERHEAD_TOKENS + IMAGE_ESTIMATE_TOKENS
        );
    }

    fn user(text: &str) -> Message {
        Message::text(Role::User, text)
    }

    fn tool_result(call_id: &str, text: &str) -> Message {
        Message {
            role: Role::Tool,
            parts: vec![ContentPart::ToolResult(ToolResult {
                call_id: call_id.to_string(),
                content: Value::String(text.to_string()),
                is_error: false,
                images: Vec::new(),
            })],
        }
    }

    fn entries_of(messages: Vec<Message>) -> Vec<ContextEntry> {
        messages
            .into_iter()
            .enumerate()
            .map(|(i, message)| ContextEntry {
                message,
                ordering: i as i64 + 1,
            })
            .collect()
    }

    /// The acceptance property of the whole module: after every in-run
    /// transformation, the state's effective messages / orderings / anchor
    /// match a fresh reload of the same DB-level history (here expressed as
    /// the equivalent LlmView).
    #[test]
    fn transformations_agree_with_view_reload_semantics() {
        let history = vec![
            user("go"),
            tool_result("c1", &"x".repeat(2_000)),
            user("again"),
            tool_result("c2", &"y".repeat(2_000)),
            user("more"),
        ];
        let view = LlmView {
            messages: history.clone(),
            orderings: (1..=5).collect(),
            anchor: None,
        };
        let mut state = ContextState::new("sys prompt".to_string(), view.clone());

        // Append two entries (assistant + tool result), anchoring on the
        // assistant.
        state.append(user("late"), 6);
        state.set_anchor(4_200, 7);
        state.append(tool_result("c3", &"z".repeat(2_000)), 7);

        // (1) plain append + anchor: reload-equivalent view would be the same
        // rows with the anchor on the assistant row.
        assert_eq!(state.conversation().len(), 7);
        assert_eq!(state.anchor().unwrap().message_index, 5);
        // estimate == anchor + local increment of what follows.
        let specs = [ToolSpec {
            name: "read".to_string(),
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
        }];
        let expected = 4_200 + estimate_message(&state.conversation()[6].message);
        assert_eq!(state.estimate(&specs), expected);

        // (2) COW clears: pre-anchor clear deducts from the anchor,
        // post-anchor clear only shrinks the increment.
        let pre_savings = estimate_message(&state.conversation()[1].message)
            - estimate_message(&tool_result("c1", "[cleared]"));
        state.apply_clears(&[(1, pre_savings), (6, 999)], "[cleared]");
        assert_eq!(state.anchor().unwrap().tokens, 4_200 - pre_savings);
        let ContentPart::ToolResult(tr) = &state.conversation()[1].message.parts[0] else {
            unreachable!()
        };
        assert_eq!(tr.content, Value::String("[cleared]".to_string()));

        // (3) compaction at cut 4: marker takes slot ordering 5, tail shifts
        // +1, anchor resets.
        let rebuilt = vec![
            ContextEntry {
                message: user("[summary]"),
                ordering: 5,
            },
            ContextEntry {
                message: state.conversation()[4].message.clone(),
                ordering: 6,
            },
            ContextEntry {
                message: state.conversation()[5].message.clone(),
                ordering: 7,
            },
            ContextEntry {
                message: state.conversation()[6].message.clone(),
                ordering: 8,
            },
        ];
        state.apply_compaction(rebuilt);
        assert!(state.anchor().is_none());
        assert_eq!(
            state
                .conversation()
                .iter()
                .map(|e| e.ordering)
                .collect::<Vec<_>>(),
            vec![5, 6, 7, 8]
        );
        // No anchor → local estimate includes system + specs.
        assert_eq!(
            state.estimate(&specs),
            estimate_view(state.conversation(), Some("sys prompt"), &specs, None)
        );
    }

    #[test]
    fn system_offset_disappears_between_coordinate_systems() {
        // estimate_view over entries + system equals estimate_context over
        // the system-prepended array, on both anchor paths.
        let entries = entries_of(vec![user("a"), user("b"), user("c")]);
        let specs = [ToolSpec {
            name: "t".to_string(),
            description: None,
            input_schema: serde_json::json!({}),
        }];
        let system = "sys";
        let mut with_system = vec![Message::text(Role::System, system)];
        with_system.extend(entries.iter().map(|e| e.message.clone()));

        assert_eq!(
            estimate_view(&entries, Some(system), &specs, None),
            estimate_context(&with_system, &specs, None)
        );

        // Anchor on entries[1] == anchor on with_system[2].
        let anchor = UsageAnchor {
            tokens: 500,
            message_index: 1,
            prompt_generation: 7,
            input_modalities: InputModalities::Unknown,
            projection: None,
        };
        assert_eq!(
            estimate_view(&entries, Some(system), &specs, Some(&anchor)),
            estimate_context(&with_system, &specs, Some((500, 2)))
        );
    }
}
