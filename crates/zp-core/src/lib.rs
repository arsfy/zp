//! Shared application core for ZP: paths, logging, settings, projects, checkpoints.
//!
//! Only the foundation (paths and logging) exists so far; the rest arrives in later
//! milestones. See `PLAN.md`.

pub mod logging;
pub mod paths;

/// Human-readable application name.
pub const APP_NAME: &str = "ZP";

/// Application version, shared by every binary in the workspace.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
