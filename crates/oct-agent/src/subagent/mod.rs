//! Subagent runtime: active child state, result envelopes and completion. Workers reuse
//! the production agent loop, tool execution, compaction and persistence
//! through the shared [`crate::runtime`] execution kernel — there is no separate
//! recursive LLM caller and no REST round-trip.
//!
//! - [`crate::tools::task`]: model-facing delegation over a scoped port.
//! - [`crate::application::delegation`]: registration, launch and result envelope.
//! - [`manager`]/`registry`: active child state and completion observation.
//!
//! Crash recovery is owned by `application::conversation::recovery`.

pub mod envelope;
pub mod manager;
pub(crate) mod registry;

pub use manager::{ChildHandle, ChildRun, SubagentManager, activity_of, await_done};

pub use envelope::{TaskEnvelope, build_task_envelope};
