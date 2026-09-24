//! Explicit connection configuration for provider endpoints.
//!
//! [`ConnectionConfig`] is the ONLY shape through which runtime endpoint
//! settings (base URL, API key) enter an adapter: the caller — the
//! oct-agent model-resolution service for DB provider rows, or a provider
//! factory materializing environment defaults — decides where the values
//! come from, so no adapter reads the process environment on its own.

/// Caller-resolved connection settings for one provider endpoint, in
/// override form: a `None` field means "keep the provider family's own
/// default" (built-in base URL, or the API key materialized from the
/// environment when the provider was constructed), while `Some` wins
/// outright. Blank strings count as absent — provider rows store `''` for
/// "not configured".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConnectionConfig {
    pub base_url: Option<String>,
    pub api_key: Option<String>,
}

impl ConnectionConfig {
    pub fn new() -> Self {
        Self::default()
    }

    /// From raw provider-row fields, treating blank values as absent (an
    /// empty DB column must mean "fall back", never "override with empty").
    pub fn from_row_fields(base_url: Option<&str>, api_key: &str) -> Self {
        Self {
            base_url: non_blank(base_url.unwrap_or("")).map(str::to_string),
            api_key: non_blank(api_key).map(str::to_string),
        }
    }
}

/// Read an API-key environment variable, treating blank as absent. This is
/// the only env reader of the crate: provider FACTORIES call it once at
/// construction to materialize their defaults — adapters never do.
pub(crate) fn env_api_key(var: &str) -> Option<String> {
    non_blank(&std::env::var(var).unwrap_or_default()).map(str::to_string)
}

/// `Some(trimmed)` for a non-blank string, `None` otherwise.
fn non_blank(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_row_fields_are_absent_not_overrides() {
        let config = ConnectionConfig::from_row_fields(Some("  "), "");
        assert_eq!(config, ConnectionConfig::new());

        let config = ConnectionConfig::from_row_fields(
            Some(" http://localhost:9/v1 "),
            " sk-key ",
        );
        assert_eq!(config.base_url.as_deref(), Some("http://localhost:9/v1"));
        assert_eq!(config.api_key.as_deref(), Some("sk-key"));
    }
}
