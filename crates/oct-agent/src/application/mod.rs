//! Application operations shared by HTTP and backend-driven entry points.
//! Transport adapters pass commands; preparation and execution handoff live here.

pub mod conversation;

pub(crate) mod assembly;
pub mod execution;
pub(crate) mod title;

pub mod delegation;
pub mod inheritance;
pub mod notifications;
pub mod tool_ports;

pub mod history;

pub mod settings;
