//! Subagent runtime (ADR 0002): the `task` tool, the
//! [`SubagentManager`] and the crash-recovery orchestrator. Workers reuse
//! the production agent loop, tool execution, compaction and persistence
//! through the shared [`crate::runtime::RunService`] — there is no separate
//! recursive LLM caller and no REST round-trip.
//!
//! - [`task_tool`]: the model-facing delegation interface (two required
//!   arguments, `description` + `prompt`; the role is fixed to `worker`).
//! - [`manager`]: registration identity, launch/re-launch, settlement and
//!   the result envelope shared by the live path, backfill and recovery.
//! - [`recover`]: the resume-driven orchestration that rebuilds a crashed
//!   task tree before continuing the parent loop.

pub mod manager;
pub mod recover;
pub mod task_tool;

pub use manager::{
    ChildHandle, ChildRun, ChildRunRequest, SubagentManager, TaskEnvelope, TaskError,
    activity_of, await_done, build_task_envelope,
};
pub use recover::{RecoveryJob, orchestrate};
pub use task_tool::{TaskSpawnConfig, TaskTool, ToolsetSlot};
