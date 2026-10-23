//! Changes to the live permission gates of a conversation tree.
use crate::permissions::{evaluator::PermissionMode, gate::PermissionGate};
use dashmap::DashMap;
use std::sync::{Arc, Weak};
#[derive(Clone)]
pub struct PermissionsApplication {
    gates: Arc<DashMap<String, Vec<Weak<PermissionGate>>>>,
}
impl PermissionsApplication {
    pub(crate) fn new(gates: Arc<DashMap<String, Vec<Weak<PermissionGate>>>>) -> Self {
        Self { gates }
    }
    /// The conversation's currently live gates (root run plus any live
    /// subagent runs of its tree).
    pub fn active_gates(&self, conversation_id: &str) -> Vec<Arc<PermissionGate>> {
        match self.gates.get(conversation_id) {
            Some(gates) => gates.iter().filter_map(Weak::upgrade).collect(),
            None => Vec::new(),
        }
    }

    /// Live mode switch for every active gate of a conversation (the PATCH
    /// endpoint; takes effect on the very next tool call). Subagent gates are
    /// registered under the root id too, so the whole tree follows.
    pub fn set_conversation_permission_mode(&self, conversation_id: &str, mode: PermissionMode) {
        for gate in self.active_gates(conversation_id) {
            gate.set_mode(mode);
        }
    }
}
