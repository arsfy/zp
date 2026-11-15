//! Tree-structured JSONL session storage for ZP.
//!
//! - [`Session`]: one session file — an append-only tree of [`SessionEntry`]s (messages,
//!   model / thinking-level changes, names, labels, compactions). Branching moves the leaf;
//!   [`Session::context`] rebuilds the agent transcript of the active branch.
//! - [`SessionManager`]: sessions grouped per project under `~/.zp/sessions/--<cwd>--/`,
//!   with listing, lookup by id prefix and forking a branch into a new file.

mod entry;
mod manager;
mod session;
pub mod time;

pub use entry::{EntryData, HeaderType, SESSION_VERSION, SessionEntry, SessionHeader};
pub use manager::{SessionInfo, SessionManager, encode_cwd};
pub use session::{COMPACTION_SUMMARY_TYPE, ModelRef, Session, SessionContext, SessionTreeNode};
