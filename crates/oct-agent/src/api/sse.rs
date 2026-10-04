use crate::agent::AgentEvent;
use axum::response::sse::Event;

/// Serialize an AgentEvent as an SSE `data:` payload.
pub(crate) fn sse_event(e: &AgentEvent) -> Event {
    Event::default()
        .json_data(e)
        .unwrap_or_else(|_| Event::default())
}
