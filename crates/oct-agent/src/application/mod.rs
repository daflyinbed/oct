//! Application operations shared by HTTP and backend-driven entry points.
//! Transport adapters pass commands; preparation and execution handoff live here.

pub mod conversation;

pub(crate) mod assembly;
pub(crate) mod execution;
pub(crate) mod title;
