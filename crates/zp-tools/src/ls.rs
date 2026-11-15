//! `ls`: directory listing.

use std::path::PathBuf;

use anyhow::{Context as _, bail};
use futures::future::BoxFuture;
use serde_json::{Value, json};
use zp_agent::{AgentTool, AgentToolResult, ToolContext};

use crate::path::{display, resolve};
use crate::truncate::{DEFAULT_MAX_BYTES, format_size, truncate_head};

const DEFAULT_LIMIT: usize = 500;

/// Lists directories relative to a working directory.
#[derive(Clone, Debug)]
pub struct LsTool {
    cwd: PathBuf,
}

impl LsTool {
    /// An ls tool for `cwd`.
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self { cwd: cwd.into() }
    }
}

impl AgentTool for LsTool {
    fn name(&self) -> &str {
        "ls"
    }

    fn description(&self) -> &str {
        "List directory contents. Returns entries sorted alphabetically, with '/' suffix for \
         directories. Includes dotfiles. Output is truncated to 500 entries or 50KB (whichever \
         is hit first)."
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some("List directory contents")
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Directory to list (default: current directory)"},
                "limit": {"type": "integer", "description": "Maximum number of entries to return (default: 500)"}
            }
        })
    }

    fn execute<'a>(
        &'a self,
        args: Value,
        ctx: ToolContext,
    ) -> BoxFuture<'a, anyhow::Result<AgentToolResult>> {
        Box::pin(async move {
            let dir = resolve(&self.cwd, args["path"].as_str().unwrap_or("."));
            let shown = display(&self.cwd, &dir);
            let limit = args["limit"].as_i64().map_or(DEFAULT_LIMIT, |n| {
                usize::try_from(n.max(1)).unwrap_or(DEFAULT_LIMIT)
            });
            let metadata = tokio::fs::metadata(&dir)
                .await
                .with_context(|| format!("Path not found: {shown}"))?;
            if !metadata.is_dir() {
                bail!("Not a directory: {shown}");
            }
            let mut reader = tokio::fs::read_dir(&dir)
                .await
                .with_context(|| format!("Cannot read directory {shown}"))?;
            let mut entries = Vec::new();
            while let Some(entry) = reader.next_entry().await? {
                if ctx.cancel.is_cancelled() {
                    bail!("Operation aborted");
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                // Follow symlinks so a link to a directory is shown as one.
                let is_dir = tokio::fs::metadata(entry.path())
                    .await
                    .is_ok_and(|m| m.is_dir());
                entries.push((name, is_dir));
            }
            if entries.is_empty() {
                return Ok(AgentToolResult::text("(empty directory)"));
            }
            entries.sort_by(|a, b| {
                a.0.to_lowercase()
                    .cmp(&b.0.to_lowercase())
                    .then(a.0.cmp(&b.0))
            });
            let limit_reached = entries.len() > limit;
            let listing: Vec<String> = entries
                .into_iter()
                .take(limit)
                .map(|(name, is_dir)| if is_dir { format!("{name}/") } else { name })
                .collect();
            let cut = truncate_head(&listing.join("\n"), usize::MAX, DEFAULT_MAX_BYTES);
            let mut notices = Vec::new();
            let mut details = serde_json::Map::new();
            if limit_reached {
                notices.push(format!(
                    "{limit} entries limit reached. Use limit={} for more",
                    limit * 2
                ));
                details.insert("entryLimitReached".into(), json!(limit));
            }
            if cut.info.truncated {
                notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
                details.insert("truncation".into(), json!(cut.info));
            }
            let mut text = cut.content;
            if !notices.is_empty() {
                text = format!("{text}\n\n[{}]", notices.join(". "));
            }
            let result = AgentToolResult::text(text);
            Ok(if details.is_empty() {
                result
            } else {
                result.with_details(Value::Object(details))
            })
        })
    }
}
