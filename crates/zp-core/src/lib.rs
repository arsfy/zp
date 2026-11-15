//! Shared application core for ZP: paths, logging, settings, projects, checkpoints.
//!
//! Paths, logging, the persisted model catalog layers, the system prompt,
//! [`CodingSession`] (agent + tools + persisted session), projects, the persisted UI
//! state, and file change tracking (snapshots, diffs, restores, git). See `PLAN.md`.

pub mod app_state;
pub mod changes;
pub mod checkpoint;
pub mod coding_session;
pub mod credentials;
pub mod diff;
pub mod git;
pub mod logging;
pub mod model_catalog;
pub mod paths;
pub mod permissions;
pub mod project;
pub mod providers;
pub mod restore;
pub mod settings;
pub mod system_prompt;
pub mod title;

pub use coding_session::{
    CodingHooks, CodingSession, CodingSessionOptions, UsageTotals, session_model,
};

/// Human-readable application name.
pub const APP_NAME: &str = "ZP";

/// Application version, shared by every binary in the workspace.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
