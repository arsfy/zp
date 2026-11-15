//! `grep` and `find`: gitignore-aware search, run in-process (no ripgrep/fd binaries).

use std::path::{Path, PathBuf};

use anyhow::{Context as _, anyhow, bail};
use futures::future::BoxFuture;
use globset::GlobBuilder;
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use regex::RegexBuilder;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;
use zp_agent::{AgentTool, AgentToolResult, ToolContext};

use crate::path::{resolve, slash};
use crate::read::looks_binary;
use crate::truncate::{
    DEFAULT_MAX_BYTES, GREP_MAX_LINE_CHARS, format_size, truncate_head, truncate_line,
};

const GREP_DEFAULT_LIMIT: usize = 100;
const FIND_DEFAULT_LIMIT: usize = 1000;
/// Files larger than this are not searched by `grep`.
const GREP_MAX_FILE_BYTES: u64 = 20 * 1024 * 1024;

/// Walks `root` like ripgrep: hidden files included, `.gitignore` / `.ignore` respected
/// (also outside git repositories), `.git` skipped, entries sorted by name.
fn walker(root: &Path) -> WalkBuilder {
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false)
        .require_git(false)
        .follow_links(false)
        .filter_entry(|entry| entry.file_name() != ".git")
        .sort_by_file_name(|a, b| a.cmp(b));
    builder
}

/// Positive integer argument with a default; values below 1 mean 1.
fn limit_arg(args: &Value, name: &str, default: usize) -> usize {
    args[name]
        .as_i64()
        .map_or(default, |n| usize::try_from(n.max(1)).unwrap_or(default))
}

/// Joins result lines, applies the byte limit and appends notices.
fn finish(
    lines: &[String],
    mut notices: Vec<String>,
    mut details: Map<String, Value>,
) -> AgentToolResult {
    let cut = truncate_head(&lines.join("\n"), usize::MAX, DEFAULT_MAX_BYTES);
    let mut text = cut.content;
    if cut.info.truncated {
        notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
        details.insert("truncation".into(), json!(cut.info));
    }
    if !notices.is_empty() {
        text = format!("{text}\n\n[{}]", notices.join(". "));
    }
    let result = AgentToolResult::text(text);
    if details.is_empty() {
        result
    } else {
        result.with_details(Value::Object(details))
    }
}

async fn run_blocking<T: Send + 'static>(
    cancel: &CancellationToken,
    work: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> anyhow::Result<T> {
    tokio::select! {
        () = cancel.cancelled() => bail!("Operation aborted"),
        result = tokio::task::spawn_blocking(work) => result.map_err(|error| anyhow!("search failed: {error}"))?,
    }
}

// ----- grep ------------------------------------------------------------------------------

/// Searches file contents with a regular expression.
#[derive(Clone, Debug)]
pub struct GrepTool {
    cwd: PathBuf,
}

impl GrepTool {
    /// A grep tool for `cwd`.
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self { cwd: cwd.into() }
    }
}

struct GrepRequest {
    root: PathBuf,
    pattern: String,
    glob: Option<String>,
    ignore_case: bool,
    literal: bool,
    context: usize,
    limit: usize,
}

fn grep(request: GrepRequest, cancel: CancellationToken) -> anyhow::Result<AgentToolResult> {
    let source = if request.literal {
        regex::escape(&request.pattern)
    } else {
        request.pattern.clone()
    };
    let regex = RegexBuilder::new(&source)
        .case_insensitive(request.ignore_case)
        .build()
        .map_err(|error| anyhow!("Invalid regex: {error}"))?;
    let metadata = std::fs::metadata(&request.root)
        .map_err(|_| anyhow!("Path not found: {}", request.root.display()))?;
    let is_dir = metadata.is_dir();

    let mut builder = walker(&request.root);
    if is_dir && let Some(glob) = &request.glob {
        // A leading "./" never matches; "!" negates.
        let (negate, rest) = match glob.strip_prefix('!') {
            Some(rest) => ("!", rest),
            None => ("", glob.as_str()),
        };
        let rest = rest.trim_start_matches("./");
        let overrides = OverrideBuilder::new(&request.root)
            .add(&format!("{negate}{rest}"))
            .and_then(|builder| builder.build())
            .map_err(|error| anyhow!("Invalid glob: {error}"))?;
        builder.overrides(overrides);
    }

    let mut lines = Vec::new();
    let mut matches = 0;
    let mut limit_reached = false;
    let mut lines_truncated = false;
    for entry in builder.build() {
        if cancel.is_cancelled() {
            bail!("Operation aborted");
        }
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        if entry
            .metadata()
            .is_ok_and(|m| m.len() > GREP_MAX_FILE_BYTES)
        {
            continue;
        }
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        if looks_binary(&bytes) {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        let file_lines: Vec<&str> = text.lines().collect();
        let hits: Vec<usize> = file_lines
            .iter()
            .enumerate()
            .filter(|(_, line)| regex.is_match(line))
            .map(|(i, _)| i)
            .collect();
        if hits.is_empty() {
            continue;
        }
        if matches >= request.limit {
            limit_reached = true;
            break;
        }
        let take = hits.len().min(request.limit - matches);
        if take < hits.len() {
            limit_reached = true;
        }
        let shown_path = if is_dir {
            slash(path.strip_prefix(&request.root).unwrap_or(path))
        } else {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default()
        };
        let hit_set: std::collections::HashSet<usize> = hits[..take].iter().copied().collect();
        let mut next_line = 0;
        for &hit in &hits[..take] {
            let from = hit.saturating_sub(request.context).max(next_line);
            let to = (hit + request.context).min(file_lines.len() - 1);
            for (i, line) in file_lines.iter().enumerate().take(to + 1).skip(from) {
                let (text, cut) = truncate_line(line, GREP_MAX_LINE_CHARS);
                lines_truncated |= cut;
                if hit_set.contains(&i) {
                    lines.push(format!("{shown_path}:{}: {text}", i + 1));
                } else {
                    lines.push(format!("{shown_path}-{}- {text}", i + 1));
                }
            }
            next_line = to + 1;
        }
        matches += take;
        if limit_reached {
            break;
        }
    }

    if matches == 0 {
        return Ok(AgentToolResult::text("No matches found"));
    }
    let mut notices = Vec::new();
    let mut details = Map::new();
    if limit_reached {
        notices.push(format!(
            "{} matches limit reached. Use limit={} for more, or refine pattern",
            request.limit,
            request.limit * 2
        ));
        details.insert("matchLimitReached".into(), json!(request.limit));
    }
    if lines_truncated {
        notices.push(format!(
            "Some lines truncated to {GREP_MAX_LINE_CHARS} chars. Use read tool to see full lines"
        ));
        details.insert("linesTruncated".into(), json!(true));
    }
    Ok(finish(&lines, notices, details))
}

impl AgentTool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search file contents for a pattern. Returns matching lines with file paths and line \
         numbers. Respects .gitignore. Output is truncated to 100 matches or 50KB (whichever is \
         hit first). Long lines are truncated to 500 chars."
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some("Search file contents for patterns (respects .gitignore)")
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Search pattern (regex or literal string)"},
                "path": {"type": "string", "description": "Directory or file to search (default: current directory)"},
                "glob": {"type": "string", "description": "Filter files by glob pattern, relative to path, e.g. '*.ts', 'src/**/*.ts' or '**/*.spec.ts'. A pattern without '/' matches file names at any depth"},
                "ignoreCase": {"type": "boolean", "description": "Case-insensitive search (default: false)"},
                "literal": {"type": "boolean", "description": "Treat pattern as literal string instead of regex (default: false)"},
                "context": {"type": "integer", "description": "Number of lines to show before and after each match (default: 0)"},
                "limit": {"type": "integer", "description": "Maximum number of matches to return (default: 100)"}
            },
            "required": ["pattern"]
        })
    }

    fn execute<'a>(
        &'a self,
        args: Value,
        ctx: ToolContext,
    ) -> BoxFuture<'a, anyhow::Result<AgentToolResult>> {
        Box::pin(async move {
            let request = GrepRequest {
                root: resolve(&self.cwd, args["path"].as_str().unwrap_or(".")),
                pattern: args["pattern"].as_str().unwrap_or_default().to_owned(),
                glob: args["glob"]
                    .as_str()
                    .filter(|g| !g.is_empty())
                    .map(str::to_owned),
                ignore_case: args["ignoreCase"].as_bool().unwrap_or(false),
                literal: args["literal"].as_bool().unwrap_or(false),
                context: args["context"]
                    .as_i64()
                    .map_or(0, |c| usize::try_from(c.max(0)).unwrap_or(0)),
                limit: limit_arg(&args, "limit", GREP_DEFAULT_LIMIT),
            };
            let cancel = ctx.cancel.clone();
            run_blocking(&ctx.cancel, move || grep(request, cancel)).await
        })
    }
}

// ----- find ------------------------------------------------------------------------------

/// Finds files by glob pattern.
#[derive(Clone, Debug)]
pub struct FindTool {
    cwd: PathBuf,
}

impl FindTool {
    /// A find tool for `cwd`.
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self { cwd: cwd.into() }
    }
}

fn find(
    root: PathBuf,
    pattern: &str,
    limit: usize,
    cancel: CancellationToken,
) -> anyhow::Result<AgentToolResult> {
    if !root.is_dir() {
        bail!("Path not found: {}", root.display());
    }
    let pattern = if cfg!(windows) {
        pattern.replace('\\', "/")
    } else {
        pattern.to_owned()
    };
    let path_pattern = pattern.contains('/');
    let effective = if !path_pattern || pattern.starts_with("**/") || pattern == "**" {
        pattern.clone()
    } else if let Some(anchored) = pattern.strip_prefix('/') {
        anchored.to_owned()
    } else {
        format!("**/{pattern}")
    };
    let matcher = GlobBuilder::new(&effective)
        .literal_separator(path_pattern)
        // Smart case, like fd: all-lowercase patterns match case-insensitively.
        .case_insensitive(!pattern.chars().any(char::is_uppercase))
        .build()
        .map_err(|error| anyhow!("Invalid glob pattern: {error}"))?
        .compile_matcher();

    let mut results = Vec::new();
    let mut limit_reached = false;
    for entry in walker(&root).build() {
        if cancel.is_cancelled() {
            bail!("Operation aborted");
        }
        let Ok(entry) = entry else { continue };
        if entry.depth() == 0 {
            continue;
        }
        let relative = entry.path().strip_prefix(&root).unwrap_or(entry.path());
        let candidate = if path_pattern {
            slash(relative)
        } else {
            entry.file_name().to_string_lossy().into_owned()
        };
        if !matcher.is_match(&candidate) {
            continue;
        }
        if results.len() == limit {
            limit_reached = true;
            break;
        }
        let mut shown = slash(relative);
        if entry.file_type().is_some_and(|t| t.is_dir()) {
            shown.push('/');
        }
        results.push(shown);
    }

    if results.is_empty() {
        return Ok(AgentToolResult::text("No files found matching pattern"));
    }
    let mut notices = Vec::new();
    let mut details = Map::new();
    if limit_reached {
        notices.push(format!(
            "{limit} results limit reached. Use limit={} for more, or refine pattern",
            limit * 2
        ));
        details.insert("resultLimitReached".into(), json!(limit));
    }
    Ok(finish(&results, notices, details))
}

impl AgentTool for FindTool {
    fn name(&self) -> &str {
        "find"
    }

    fn description(&self) -> &str {
        "Search for files by glob pattern. Returns matching file paths relative to the search \
         directory. Respects .gitignore. Output is truncated to 1000 results or 50KB (whichever \
         is hit first)."
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some("Find files by glob pattern (respects .gitignore)")
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Glob pattern to match files, e.g. '*.ts', '**/*.json', or 'src/**/*.spec.ts'"},
                "path": {"type": "string", "description": "Directory to search in (default: current directory)"},
                "limit": {"type": "integer", "description": "Maximum number of results (default: 1000)"}
            },
            "required": ["pattern"]
        })
    }

    fn execute<'a>(
        &'a self,
        args: Value,
        ctx: ToolContext,
    ) -> BoxFuture<'a, anyhow::Result<AgentToolResult>> {
        Box::pin(async move {
            let root = resolve(&self.cwd, args["path"].as_str().unwrap_or("."));
            let pattern = args["pattern"].as_str().unwrap_or_default().to_owned();
            let limit = limit_arg(&args, "limit", FIND_DEFAULT_LIMIT);
            let cancel = ctx.cancel.clone();
            run_blocking(&ctx.cancel, move || find(root, &pattern, limit, cancel))
                .await
                .context("find failed")
        })
    }
}
