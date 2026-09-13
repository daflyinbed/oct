pub mod capabilities;
pub mod limits;
pub mod model_info;
pub mod registry;

use crate::core::ModelError;
use crate::model::{ChatModel, EmbeddingModel};

pub use capabilities::ModelCapabilities;
pub use limits::ModelLimits;
pub use model_info::ModelInfo;
pub use registry::ProviderRegistry;

pub trait Provider: Send + Sync {
    fn name(&self) -> &'static str;

    fn chat_model(&self, model: &str) -> Result<Box<dyn ChatModel>, ModelError>;

    fn embedding_model(&self, _model: &str) -> Result<Box<dyn EmbeddingModel>, ModelError> {
        Err(ModelError::unsupported("embedding not supported"))
    }
}
