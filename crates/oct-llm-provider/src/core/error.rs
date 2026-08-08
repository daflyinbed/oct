#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("unsupported: {0}")]
    Unsupported(String),

    #[error("unknown provider: {0}")]
    UnknownProvider(String),

    #[error("invalid model spec: {0}")]
    InvalidModelSpec(String),

    #[error("authentication failed")]
    Authentication,

    #[error("rate limited")]
    RateLimited,

    #[error("provider error: {message}")]
    Provider { message: String },

    #[error("transport error: {message}")]
    Transport { message: String },
}

impl ModelError {
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::Unsupported(message.into())
    }

    pub fn unknown_provider(name: impl Into<String>) -> Self {
        Self::UnknownProvider(name.into())
    }

    pub fn invalid_model_spec(spec: impl Into<String>) -> Self {
        Self::InvalidModelSpec(spec.into())
    }

    pub fn provider(message: impl Into<String>) -> Self {
        Self::Provider {
            message: message.into(),
        }
    }

    pub fn transport(message: impl Into<String>) -> Self {
        Self::Transport {
            message: message.into(),
        }
    }
}
