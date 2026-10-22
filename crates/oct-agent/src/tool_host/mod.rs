//! Host adapters for progress, permissions and nested tool execution.
pub mod dispatch;

use crate::events::{AgentEvent, EventHub};
use crate::tool_contract::{
    NestedCall, NestedProgressSink, OutputStream, ToolCallIdentity, ToolProgressSink,
};

impl ToolProgressSink for EventHub {
    fn output(&self, call: &ToolCallIdentity, stream: OutputStream, delta: String) {
        self.publish(AgentEvent::ToolOutputDelta {
            call_id: call.call_id.clone(),
            stream,
            delta,
        });
    }
}

impl NestedProgressSink for EventHub {
    fn update(&self, parent_call_id: &str, call: &NestedCall) {
        self.publish(AgentEvent::NestedToolUpdate {
            parent_call_id: parent_call_id.to_owned(),
            call: call.clone(),
        });
    }
}

/// Code-mode host outputs are separate from the base tool context. Approval
/// dispatch keeps its event hub; nested execution gets only a typed observer.
#[derive(Clone)]
pub struct CodeHostEvents {
    pub approvals: std::sync::Arc<EventHub>,
    pub nested: std::sync::Arc<dyn NestedProgressSink>,
}

impl CodeHostEvents {
    pub fn new(hub: std::sync::Arc<EventHub>) -> Self {
        Self {
            approvals: hub.clone(),
            nested: hub,
        }
    }
}
