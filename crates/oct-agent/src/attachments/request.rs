//! IO orchestration around the pure context projection. Provider adapters see
//! only ordinary messages; neither resource references nor storage handles cross.
use super::{AttachmentService, ServiceError};
use crate::attachment_types::parse_attachment_id;
use crate::context::{
    ContextState,
    projection::{MediaAsset, MediaPolicy, ProjectionPlan},
};
use crate::modality::gate_request_messages;
use oct_llm_provider::core::{ContentPart, Message};
use std::collections::{HashMap, HashSet};

pub struct RequestAssembler<'a> {
    service: Option<&'a AttachmentService>,
    policy: MediaPolicy,
}
impl<'a> RequestAssembler<'a> {
    pub fn new(service: Option<&'a AttachmentService>, policy: MediaPolicy) -> Self {
        Self { service, policy }
    }
    pub async fn plan(&self, context: &ContextState) -> Result<ProjectionPlan, ServiceError> {
        // Unsupported references disappear before metadata reads or base64 IO.
        let messages =
            gate_request_messages(context.messages_for_request(), context.input_modalities());
        let mut ids = HashSet::new();
        for message in &messages {
            for part in &message.parts {
                match part {
                    ContentPart::ImageUrl { url } => {
                        if let Some(id) = parse_attachment_id(url) {
                            ids.insert(id.to_owned());
                        }
                    }
                    ContentPart::ToolResult(result) => {
                        for image in &result.images {
                            if let Some(id) = parse_attachment_id(&image.data) {
                                ids.insert(id.to_owned());
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        let mut assets = HashMap::new();
        let mut missing = HashSet::new();
        if let Some(service) = self.service {
            let rows = service
                .assets(&ids.iter().cloned().collect::<Vec<_>>())
                .await?;
            for row in rows {
                assets.insert(
                    row.id.clone(),
                    MediaAsset {
                        id: row.id,
                        mime_type: row.model_mime_type,
                        size_bytes: row.model_size_bytes.max(0) as u64,
                        variant: format!("{}:{}", row.model_storage_key, row.normalizer_version),
                    },
                );
            }
            missing.extend(ids.into_iter().filter(|id| !assets.contains_key(id)));
        }
        let orderings = context
            .conversation()
            .iter()
            .map(|entry| entry.ordering)
            .collect::<Vec<_>>();
        ProjectionPlan::build(
            messages,
            &orderings,
            context.input_modalities(),
            assets,
            &missing,
            self.policy,
        )
        .map_err(|error| ServiceError::Invalid(error.to_string()))
    }
    pub async fn materialize(&self, plan: &mut ProjectionPlan) -> Vec<Message> {
        match self.service {
            Some(service) => {
                let (messages, missing) =
                    service.hydrate_with_failures(plan.messages.clone()).await;
                plan.omit_unavailable(&missing);
                messages
            }
            None => plan.messages.clone(),
        }
    }
}
