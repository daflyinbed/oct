pub mod capabilities;
pub mod connection;
pub mod limits;
pub mod model_info;
pub mod registry;

use crate::core::ModelError;
use crate::model::{ChatModel, EmbeddingModel};

pub use capabilities::ModelCapabilities;
pub use connection::ConnectionConfig;
pub use limits::ModelLimits;
pub use model_info::ModelInfo;
pub use registry::ProviderRegistry;

pub trait Provider: Send + Sync {
    fn name(&self) -> &'static str;

    /// Build a chat model over the provider's own (default) connection.
    fn chat_model(&self, model: &str) -> Result<Box<dyn ChatModel>, ModelError>;

    /// Build a chat model over a caller-resolved connection — the
    /// DB-driven construction path: every `Some` field of `connection`
    /// overrides the family default, `None` falls back to it. Families
    /// without a runtime-configurable endpoint ignore the overrides.
    fn chat_model_with_connection(
        &self,
        model: &str,
        connection: &ConnectionConfig,
    ) -> Result<Box<dyn ChatModel>, ModelError> {
        let _ = connection;
        self.chat_model(model)
    }

    /// The family's own (default) connection: built-in base URL plus the
    /// API key materialized from the environment at construction. Lets a
    /// model-resolution service validate the effective fallback (e.g.
    /// "row has no key AND the env default is absent") before building.
    fn default_connection(&self) -> ConnectionConfig {
        ConnectionConfig::default()
    }

    fn embedding_model(&self, _model: &str) -> Result<Box<dyn EmbeddingModel>, ModelError> {
        Err(ModelError::unsupported("embedding not supported"))
    }
}
