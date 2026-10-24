//! Subagent runtime: active child state, result envelopes and completion. Workers reuse
//! the production agent loop, tool execution, compaction and persistence
//! through the shared [`crate::runtime`] execution kernel — there is no separate
//! recursive LLM caller and no REST round-trip.
//!
//! - [`crate::tools::task`]: model-facing delegation over a scoped port.
//! - [`crate::application::delegation`]: registration and result envelope.
//! - [`crate::application::child_startup`]: startup, recovery and settled results.
//! - [`manager`]/`registry`: active child state and completion observation.
//!
//! Parent-history recovery is coordinated by `application::conversation::recovery`.

pub mod envelope;
pub mod manager;
pub(crate) mod registry;

pub use manager::{ChildRun, SubagentManager, activity_of, await_done};

pub use envelope::{TaskEnvelope, build_task_envelope};
