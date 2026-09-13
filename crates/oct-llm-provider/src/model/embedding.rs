use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::core::ModelError;
use crate::provider::ModelInfo;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct EmbeddingRequest {
    pub inputs: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct EmbeddingResponse {
    pub embeddings: Vec<Vec<f32>>,
    pub provider_metadata: Map<String, Value>,
}

#[async_trait]
pub trait EmbeddingModel: Send + Sync {
    fn info(&self) -> &ModelInfo;

    async fn embed(&self, req: EmbeddingRequest) -> Result<EmbeddingResponse, ModelError>;
}
