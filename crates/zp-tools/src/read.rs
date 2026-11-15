//! `read`: file contents (text with paging, or an image attachment).

use std::path::PathBuf;

use anyhow::{Context as _, bail};
use base64::Engine as _;
use futures::future::BoxFuture;
use serde_json::{Value, json};
use zp_agent::{AgentTool, AgentToolResult, ToolContext};
use zp_ai::{ImageContent, UserContent};

use crate::path::{display, resolve};
use crate::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, format_size, truncate_head};

/// Largest image sent to the model (providers reject bigger ones).
const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// Reads files relative to a working directory.
#[derive(Clone, Debug)]
pub struct ReadTool {
    cwd: PathBuf,
}

impl ReadTool {
    /// A read tool for `cwd`.
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self { cwd: cwd.into() }
    }
}

/// MIME type of a supported image, from its magic bytes.
fn image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// Heuristic used by most tools: a NUL byte in the first 8 KB.
pub(crate) fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|&b| b == 0)
}

impl AgentTool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read the contents of a file. Supports text files and images (png, jpg, gif, webp). \
         Images are sent as attachments. For text files, output is truncated to 2000 lines or \
         50KB (whichever is hit first). Use offset/limit for large files. When you need the \
         full file, continue with offset until complete."
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some("Read file contents")
    }

    fn prompt_guidelines(&self) -> &[&str] {
        &["Use read to examine files instead of cat or sed."]
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to the file to read (relative or absolute)"},
                "offset": {"type": "integer", "description": "Line number to start reading from (1-indexed)"},
                "limit": {"type": "integer", "description": "Maximum number of lines to read"}
            },
            "required": ["path"]
        })
    }

    fn execute<'a>(
        &'a self,
        args: Value,
        ctx: ToolContext,
    ) -> BoxFuture<'a, anyhow::Result<AgentToolResult>> {
        Box::pin(async move {
            let raw_path = args["path"].as_str().unwrap_or_default();
            let path = resolve(&self.cwd, raw_path);
            let shown = display(&self.cwd, &path);
            let bytes = tokio::select! {
                () = ctx.cancel.cancelled() => bail!("Operation aborted"),
                bytes = tokio::fs::read(&path) => bytes.with_context(|| format!("Could not read {shown}"))?,
            };

            if let Some(mime) = image_mime(&bytes) {
                if bytes.len() > MAX_IMAGE_BYTES {
                    return Ok(AgentToolResult::text(format!(
                        "Read image file [{mime}], but it is {} (limit {}); not attached.",
                        format_size(bytes.len()),
                        format_size(MAX_IMAGE_BYTES)
                    )));
                }
                let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
                return Ok(AgentToolResult {
                    content: vec![
                        UserContent::text(format!("Read image file [{mime}]")),
                        UserContent::Image(ImageContent {
                            data,
                            mime_type: mime.to_owned(),
                        }),
                    ],
                    ..AgentToolResult::default()
                });
            }
            if looks_binary(&bytes) {
                bail!(
                    "{shown} looks like a binary file ({}); it cannot be shown as text",
                    format_size(bytes.len())
                );
            }

            let text = String::from_utf8_lossy(&bytes);
            let lines: Vec<&str> = text.split('\n').collect();
            let total = lines.len();
            let offset = args["offset"].as_i64();
            let start = offset.map_or(0, |o| usize::try_from(o.max(1) - 1).unwrap_or(0));
            if start >= total {
                bail!(
                    "Offset {} is beyond end of file ({total} lines total)",
                    offset.unwrap_or_default()
                );
            }
            let limit = args["limit"]
                .as_i64()
                .map(|l| usize::try_from(l.max(0)).unwrap_or(0));
            let end = limit.map_or(total, |limit| (start + limit).min(total));
            let selected = lines[start..end].join("\n");
            let cut = truncate_head(&selected, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
            let first = start + 1;

            let mut details = None;
            let output = if cut.info.first_line_exceeds_limit {
                details = Some(json!({"truncation": cut.info}));
                format!(
                    "[Line {first} is {}, exceeds {} limit. Use bash: sed -n '{first}p' {raw_path} | head -c {DEFAULT_MAX_BYTES}]",
                    format_size(lines[start].len()),
                    format_size(DEFAULT_MAX_BYTES)
                )
            } else if cut.info.truncated {
                let last = first + cut.info.output_lines - 1;
                let limit_note =
                    if cut.info.truncated_by == Some(crate::truncate::TruncatedBy::Bytes) {
                        format!(" ({} limit)", format_size(DEFAULT_MAX_BYTES))
                    } else {
                        String::new()
                    };
                let text = format!(
                    "{}\n\n[Showing lines {first}-{last} of {total}{limit_note}. Use offset={} to continue.]",
                    cut.content,
                    last + 1
                );
                details = Some(json!({"truncation": cut.info}));
                text
            } else if end < total {
                format!(
                    "{}\n\n[{} more lines in file. Use offset={} to continue.]",
                    cut.content,
                    total - end,
                    end + 1
                )
            } else if cut.content.is_empty() {
                "(empty file)".to_owned()
            } else {
                cut.content
            };
            Ok(AgentToolResult {
                details,
                ..AgentToolResult::text(output)
            })
        })
    }
}
