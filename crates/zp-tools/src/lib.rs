//! Built-in coding tools for ZP: `read`, `write`, `edit`, `bash`, `grep`, `find`, `ls`,
//! `todo`, and `ask` (when the front end can put questions to the user).
//!
//! Each tool is bound to a working directory; relative paths resolve against it.
//! `write` and `edit` return a [`FileChange`] (before/after content) as result `details`.
//! Output is truncated to [`DEFAULT_MAX_LINES`] lines or [`DEFAULT_MAX_BYTES`] bytes with
//! a notice telling the model how to get the rest.

mod ask;
mod bash;
mod describe;
mod edit;
mod edit_apply;
mod file_change;
mod ls;
mod output;
mod path;
mod read;
mod search;
mod secrets;
mod shell;
mod todo;
mod truncate;
mod write;

use std::path::Path;
use std::sync::Arc;

use zp_agent::AgentTool;

pub use ask::{
    Answer, AskOutcome, AskRequest, AskTool, Asker, MAX_OPTIONS, MAX_QUESTIONS, Question,
    QuestionOption,
};
pub use bash::BashTool;
pub use describe::{describe_call, shorten};
pub use edit::EditTool;
pub use edit_apply::{EditSpec, apply_edits};
pub use file_change::FileChange;
pub use ls::LsTool;
pub use path::{display as display_path, resolve as resolve_path};
pub use read::ReadTool;
pub use search::{FindTool, GrepTool};
pub use secrets::is_secret_path;
pub use shell::{Shell, kill_tree};
pub use todo::{TodoItem, TodoList, TodoStatus, TodoTool};
pub use truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, Truncated, TruncatedBy, TruncationInfo, format_size,
    truncate_head, truncate_tail,
};
pub use write::WriteTool;

/// Names of all built-in tools created by [`create_tool`] (`ask` needs an [`Asker`]).
pub const ALL_TOOLS: [&str; 8] = [
    "read", "bash", "edit", "write", "grep", "find", "ls", "todo",
];

/// Names of the tools that cannot modify anything.
pub const READ_ONLY_TOOLS: [&str; 4] = ["read", "grep", "find", "ls"];

/// Creates the built-in tool `name` for `cwd`.
pub fn create_tool(name: &str, cwd: &Path) -> Option<Arc<dyn AgentTool>> {
    let tool: Arc<dyn AgentTool> = match name {
        "read" => Arc::new(ReadTool::new(cwd)),
        "bash" => Arc::new(BashTool::new(cwd)),
        "edit" => Arc::new(EditTool::new(cwd)),
        "write" => Arc::new(WriteTool::new(cwd)),
        "grep" => Arc::new(GrepTool::new(cwd)),
        "find" => Arc::new(FindTool::new(cwd)),
        "ls" => Arc::new(LsTool::new(cwd)),
        "todo" => Arc::new(TodoTool),
        _ => return None,
    };
    Some(tool)
}

/// All built-in tools for `cwd`.
pub fn coding_tools(cwd: &Path) -> Vec<Arc<dyn AgentTool>> {
    ALL_TOOLS
        .iter()
        .filter_map(|name| create_tool(name, cwd))
        .collect()
}

/// The read-only tools for `cwd`.
pub fn read_only_tools(cwd: &Path) -> Vec<Arc<dyn AgentTool>> {
    READ_ONLY_TOOLS
        .iter()
        .filter_map(|name| create_tool(name, cwd))
        .collect()
}
