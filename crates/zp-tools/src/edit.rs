//! `edit`: exact text replacement in one file.

use std::path::PathBuf;

use anyhow::{Context as _, anyhow, bail};
use futures::future::BoxFuture;
use serde_json::{Map, Value, json};
use zp_agent::{AgentTool, AgentToolResult, ToolContext};

use crate::edit_apply::{EditSpec, apply_edits};
use crate::file_change::FileChange;
use crate::path::{display, lock_file, resolve, slash};

/// Edits files relative to a working directory.
#[derive(Clone, Debug)]
pub struct EditTool {
    cwd: PathBuf,
}

impl EditTool {
    /// An edit tool for `cwd`.
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self { cwd: cwd.into() }
    }
}

fn is_single_edit(value: &Value) -> bool {
    value.get("oldText").is_some_and(Value::is_string)
        && value.get("newText").is_some_and(Value::is_string)
}

impl AgentTool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Edit a single file using exact text replacement. Every edits[].oldText must match a \
         unique, non-overlapping region of the original file. If two changes affect the same \
         block or nearby lines, merge them into one edit instead of emitting overlapping edits. \
         Do not include large unchanged regions just to connect distant changes."
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some(
            "Make precise file edits with exact text replacement, including multiple disjoint \
             edits in one call",
        )
    }

    fn prompt_guidelines(&self) -> &[&str] {
        &[
            "Use edit for targeted changes. Batch changes to one file in a single call.",
            "Each edits[].oldText must exactly and uniquely match the original file. Keep \
             matches short and non-overlapping; merge nearby changes without including large \
             unchanged regions.",
        ]
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to the file to edit (relative or absolute)"},
                "edits": {
                    "type": "array",
                    "description": "One or more targeted replacements. Each edit is matched against the original file, not incrementally. Do not include overlapping or nested edits. If two changes touch the same block or nearby lines, merge them into one edit instead.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "oldText": {"type": "string", "description": "Exact text for one targeted replacement. It must be unique in the original file and must not overlap with any other edits[].oldText in the same call."},
                            "newText": {"type": "string", "description": "Replacement text for this targeted edit."}
                        },
                        "required": ["oldText", "newText"]
                    }
                }
            },
            "required": ["path", "edits"]
        })
    }

    /// Accepts shapes models commonly send: `edits` as a JSON string or a single object,
    /// and top-level `oldText` / `newText`.
    fn prepare_arguments(&self, arguments: Value) -> Value {
        let Value::Object(mut args) = arguments else {
            return arguments;
        };
        if let Some(Value::String(text)) = args.get("edits")
            && let Ok(parsed) = serde_json::from_str::<Value>(text)
        {
            let edits = if is_single_edit(&parsed) {
                Value::Array(vec![parsed])
            } else {
                parsed
            };
            args.insert("edits".into(), edits);
        } else if args.get("edits").is_some_and(is_single_edit) {
            let single = args.remove("edits").unwrap_or_default();
            args.insert("edits".into(), Value::Array(vec![single]));
        }
        if is_single_edit(&Value::Object(args.clone())) {
            let old = args.remove("oldText").unwrap_or_default();
            let new = args.remove("newText").unwrap_or_default();
            let mut edit = Map::new();
            edit.insert("oldText".into(), old);
            edit.insert("newText".into(), new);
            match args.get_mut("edits") {
                Some(Value::Array(edits)) => edits.push(Value::Object(edit)),
                _ => {
                    args.insert("edits".into(), Value::Array(vec![Value::Object(edit)]));
                }
            }
        }
        Value::Object(args)
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
            let edits: Vec<EditSpec> = args["edits"]
                .as_array()
                .map(|edits| {
                    edits
                        .iter()
                        .map(|edit| EditSpec {
                            old_text: edit["oldText"].as_str().unwrap_or_default().to_owned(),
                            new_text: edit["newText"].as_str().unwrap_or_default().to_owned(),
                        })
                        .collect()
                })
                .unwrap_or_default();

            let _lock = lock_file(&path).await;
            if ctx.cancel.is_cancelled() {
                bail!("Operation aborted");
            }
            let bytes = tokio::fs::read(&path)
                .await
                .with_context(|| format!("Could not edit file: {shown}"))?;
            let before = String::from_utf8(bytes)
                .map_err(|_| anyhow!("Could not edit file: {shown} is not valid UTF-8 text"))?;
            let after = apply_edits(&before, &edits, &shown).map_err(|error| anyhow!(error))?;
            if ctx.cancel.is_cancelled() {
                bail!("Operation aborted");
            }
            tokio::fs::write(&path, &after)
                .await
                .with_context(|| format!("Could not write {shown}"))?;
            let change = FileChange {
                path: slash(&path),
                before: Some(before),
                after,
            };
            Ok(AgentToolResult::text(format!(
                "Successfully replaced {} block(s) in {shown}.",
                edits.len()
            ))
            .with_details(change.to_details()))
        })
    }
}
