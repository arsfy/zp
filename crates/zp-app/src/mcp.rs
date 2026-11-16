//! MCP servers and other per-project resources (prompt templates, skills) in the app.
//!
//! Each open project has one [`McpHub`]: its servers start when the project window opens
//! and stop when it closes. Conversations get the hub's tools next to the built-in ones.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{App, Global};
use zp_agent::AgentTool;
use zp_core::resources::Resources;
use zp_mcp::McpHub;

use crate::store::AppPaths;

/// The MCP hubs of open projects.
#[derive(Default)]
pub struct McpHubs(HashMap<PathBuf, Arc<McpHub>>);

impl Global for McpHubs {}

fn data_dir(cx: &App) -> Option<PathBuf> {
    cx.try_global::<AppPaths>().and_then(|p| p.data_dir.clone())
}

/// The prompt templates and skills of a project.
pub fn project_resources(root: &Path, cx: &App) -> Arc<Resources> {
    let trusted = crate::approvals::is_trusted(root, cx);
    Arc::new(Resources::load(data_dir(cx).as_deref(), root, trusted))
}

/// Built-in tools plus the tools of the project's connected MCP servers.
pub fn project_tools(root: &Path, cx: &App) -> Vec<Arc<dyn AgentTool>> {
    let mut tools = zp_tools::coding_tools(root);
    if let Some(hub) = hub(root, cx) {
        tools.extend(hub.tools());
    }
    tools
}

/// The hub of an open project.
pub fn hub(root: &Path, cx: &App) -> Option<Arc<McpHub>> {
    cx.try_global::<McpHubs>()?.0.get(root).cloned()
}

/// Every open project's hub.
pub fn hubs(cx: &App) -> Vec<(PathBuf, Arc<McpHub>)> {
    cx.try_global::<McpHubs>()
        .map(|hubs| hubs.0.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default()
}

/// (Re)starts the MCP servers of `root` from its configuration. Returns the hub.
pub fn start(root: &Path, cx: &mut App) -> Arc<McpHub> {
    let trusted = crate::approvals::is_trusted(root, cx);
    let env = |name: &str| std::env::var(name).ok();
    let (servers, errors) = zp_mcp::config::load(data_dir(cx).as_deref(), root, trusted, &env);
    for error in &errors {
        tracing::warn!(project = %root.display(), %error, "MCP configuration");
    }
    let connect = servers.iter().any(|s| s.enabled);
    let hub = McpHub::new(servers, errors);
    if connect {
        // Connecting spawns Tokio tasks.
        let started = hub.clone();
        gpui_tokio::Tokio::spawn(cx, async move { started.start() }).detach();
    }
    let old = cx
        .default_global::<McpHubs>()
        .0
        .insert(root.to_path_buf(), hub.clone());
    if let Some(old) = old {
        old.shutdown();
    }
    hub
}

/// Stops the servers of a closed project.
pub fn stop(root: &Path, cx: &mut App) {
    if let Some(hub) = cx.default_global::<McpHubs>().0.remove(root) {
        hub.shutdown();
    }
}

/// Reconnects server `name` of `root`.
pub fn reconnect(root: &Path, name: &str, cx: &mut App) {
    if let Some(hub) = hub(root, cx) {
        let name = name.to_owned();
        gpui_tokio::Tokio::spawn(cx, async move { hub.connect(&name).await }).detach();
    }
}
