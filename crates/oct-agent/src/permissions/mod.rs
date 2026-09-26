//! Tool permission layer: the gate every model-driven tool call
//! passes through at the agent loop's dispatch point.
//!
//! * [`rules`] — the in-memory rule model and its matching semantics
//!   (exact tool name or `mcp__{server}__*`; NULL pattern, command prefix,
//!   absolute-path glob).
//! * [`evaluator`] — the decision table: `decide(action, mode, rules)`
//!   with fixed precedence deny > ask > allow > mode default, plus the
//!   built-in deny over the background-task output directory.
//! * [`descriptor`] — `(tool_name, args)` → the neutral action the table
//!   consumes; unresolvable calls stay Trusted so the tool reports its own
//!   error.
//! * [`gate`] — the checkpoint itself: Allow proceeds, Deny produces an
//!   error ToolOutput on the normal result path, Ask parks on the registry
//!   + oneshot under `select!` with cancel and the fail-closed timeout.
//! * [`registry`] — the Ask arm's parking lot: in-memory pending approvals
//!   answered through a oneshot (waiting/timeout stays in the gate).
pub mod descriptor;
pub mod evaluator;
pub mod gate;
pub mod registry;
pub mod rules;
