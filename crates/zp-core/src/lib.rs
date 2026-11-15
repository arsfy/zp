//! Shared application core for ZP: paths, logging, settings, projects, checkpoints.
//!
//! So far: paths, logging, the persisted model catalog layers, the system prompt,
//! [`CodingSession`] (agent + tools + persisted session), projects and the persisted UI
//! state. See `PLAN.md`.

pub mod app_state;
pub mod coding_session;
pub mod logging;
pub mod model_catalog;
pub mod paths;
pub mod project;
pub mod system_prompt;

pub use coding_session::{
    CodingHooks, CodingSession, CodingSessionOptions, UsageTotals, session_model,
};

/// Human-readable application name.
pub const APP_NAME: &str = "ZP";

/// Application version, shared by every binary in the workspace.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
