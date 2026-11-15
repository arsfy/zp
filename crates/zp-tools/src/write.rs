//! `write`: create or overwrite a file.

use std::path::PathBuf;

use anyhow::{Context as _, bail};
use futures::future::BoxFuture;
use serde_json::{Value, json};
use zp_agent::{AgentTool, AgentToolResult, ToolContext};

use crate::file_change::FileChange;
use crate::path::{display, lock_file, resolve, slash};

/// Writes files relative to a working directory.
#[derive(Clone, Debug)]
pub struct WriteTool {
    cwd: PathBuf,
}

impl WriteTool {
    /// A write tool for `cwd`.
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self { cwd: cwd.into() }
    }
}

/// The current text of `path`, `None` if it does not exist.
pub(crate) async fn read_existing(path: &std::path::Path) -> anyhow::Result<Option<String>> {
    match tokio::fs::read(path).await {
        Ok(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

impl AgentTool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. \
         Automatically creates parent directories."
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some("Create or overwrite files")
    }

    fn prompt_guidelines(&self) -> &[&str] {
        &["Use write only for new files or complete rewrites."]
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to the file to write (relative or absolute)"},
                "content": {"type": "string", "description": "Content to write to the file"}
            },
            "required": ["path", "content"]
        })
    }

    fn execute<'a>(
        &'a self,
        args: Value,
        ctx: ToolContext,
    ) -> BoxFuture<'a, anyhow::Result<AgentToolResult>> {
        Box::pin(async move {
            let path = resolve(&self.cwd, args["path"].as_str().unwrap_or_default());
            let shown = display(&self.cwd, &path);
            let content = args["content"].as_str().unwrap_or_default().to_owned();
            let _lock = lock_file(&path).await;
            if ctx.cancel.is_cancelled() {
                bail!("Operation aborted");
            }
            let before = read_existing(&path)
                .await
                .with_context(|| format!("Could not read {shown}"))?;
            if let Some(dir) = path.parent() {
                tokio::fs::create_dir_all(dir)
                    .await
                    .with_context(|| format!("Could not create {}", display(&self.cwd, dir)))?;
            }
            tokio::fs::write(&path, &content)
                .await
                .with_context(|| format!("Could not write {shown}"))?;
            let change = FileChange {
                path: slash(&path),
                before,
                after: content,
            };
            let verb = if change.is_creation() {
                "Created"
            } else {
                "Wrote"
            };
            Ok(
                AgentToolResult::text(format!("{verb} {shown} ({} bytes)", change.after.len()))
                    .with_details(change.to_details()),
            )
        })
    }
}
