//! Subagent runtime: the `task` tool, the
//! [`SubagentManager`] and the crash-recovery orchestrator. Workers reuse
//! the production agent loop, tool execution, compaction and persistence
//! through the shared [`crate::runtime`] execution kernel — there is no separate
//! recursive LLM caller and no REST round-trip.
//!
//! - [`crate::tools::task`]: model-facing delegation over a scoped port.
//! - [`crate::application::delegation`]: registration, launch and result envelope.
//! - [`manager`]/`registry`: active child state and completion observation.
//! - [`recover`]: compatibility export of application-owned crash recovery.

mod envelope;
pub mod manager;
pub mod recover;
pub(crate) mod registry;

pub use crate::application::delegation::TaskSpawnConfig;
pub use crate::tools::task::TaskTool;
pub use manager::{
    ChildHandle, ChildRun, SubagentManager, TaskEnvelope, TaskError, activity_of, await_done,
    build_task_envelope,
};
pub use recover::{RecoveryJob, orchestrate};

pub use crate::application::execution::ChildRunRequest;
