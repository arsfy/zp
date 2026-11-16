//! The MCP Servers page: the servers of each open project with their state and tools.

use std::path::PathBuf;

use gpui::{AnyElement, Context, FontWeight, IntoElement, div, prelude::*, px};
use zp_mcp::ServerState;

use super::{ButtonKind, SettingsWindow, button, page_body, section};
use crate::store::AppPaths;
use crate::theme::ActiveTheme;

const TEMPLATE: &str = r#"{
  "mcpServers": {
  }
}
"#;

impl SettingsWindow {
    /// Opens the user's `mcp.json` (created if missing) in the default editor.
    pub fn edit_mcp_config(&mut self, cx: &mut Context<Self>) {
        let Some(dir) = cx.try_global::<AppPaths>().and_then(|p| p.data_dir.clone()) else {
            return;
        };
        let path: PathBuf = zp_mcp::config::user_file(&dir);
        if !path.exists() {
            let _ = std::fs::create_dir_all(&dir);
            if let Err(error) = std::fs::write(&path, TEMPLATE) {
                self.report(Err(error.into()), cx);
                return;
            }
        }
        cx.open_with_system(&path);
    }

    pub(super) fn render_mcp(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors.clone();
        // Projects with servers (or configuration problems) only.
        let hubs: Vec<_> = crate::mcp::hubs(cx)
            .into_iter()
            .filter(|(_, hub)| !hub.statuses().is_empty() || !hub.errors().is_empty())
            .collect();
        let mut body = page_body()
            .child(section(
                "MCP servers",
                "Servers from ~/.zp/mcp.json (and a trusted project's .zp/mcp.json), in the mcpServers format other MCP clients use. Their tools are offered to the agent as mcp__server__tool; permissions apply as for other tools. Use /reload in a conversation after editing.",
                cx,
            ))
            .child(
                div().flex().gap_1p5().child(
                    button("edit-mcp", "Edit mcp.json", ButtonKind::Normal, cx)
                        .on_click(cx.listener(|this, _, _, cx| this.edit_mcp_config(cx))),
                ),
            );
        if hubs.is_empty() {
            body = body.child(
                div()
                    .text_size(px(12.))
                    .text_color(colors.text_muted)
                    .child(
                        "No servers in the open projects. Add them to mcp.json, then use /reload.",
                    ),
            );
        }
        for (project_index, (root, hub)) in hubs.into_iter().enumerate() {
            body = body.child(section(&zp_core::project::tilde_path(&root), "", cx));
            for error in hub.errors() {
                body = body.child(
                    div()
                        .text_size(px(12.))
                        .text_color(colors.error)
                        .child(error.clone()),
                );
            }
            let statuses = hub.statuses();
            for (index, status) in statuses.into_iter().enumerate() {
                let (state, color) = match &status.state {
                    ServerState::Connected => (
                        format!("Connected · {} tools", status.tools.len()),
                        colors.success,
                    ),
                    ServerState::Connecting => ("Connecting…".to_owned(), colors.text_muted),
                    ServerState::Disabled => ("Disabled".to_owned(), colors.text_muted),
                    ServerState::Failed(_) => ("Failed".to_owned(), colors.error),
                };
                let error = match &status.state {
                    ServerState::Failed(error) => Some(error.clone()),
                    _ => None,
                };
                let (reconnect_root, name) = (root.clone(), status.name.clone());
                body = body.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .p_3()
                        .rounded(px(8.))
                        .border_1()
                        .border_color(colors.border)
                        .child(
                            div()
                                .flex()
                                .items_baseline()
                                .gap_2()
                                .child(
                                    div()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child(status.name.clone()),
                                )
                                .child(
                                    div()
                                        .text_size(px(11.))
                                        .text_color(colors.text_muted)
                                        .child(zp_core::project::tilde_path(&status.source)),
                                )
                                .child(div().flex_1())
                                .child(div().text_size(px(12.)).text_color(color).child(state)),
                        )
                        .when_some(status.description.clone(), |this, description| {
                            this.child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(colors.text_muted)
                                    .child(description),
                            )
                        })
                        .when(!status.tools.is_empty(), |this| {
                            this.child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(colors.text_muted)
                                    .child(status.tools.join(", ")),
                            )
                        })
                        .when_some(error, |this, error| {
                            this.child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(colors.error)
                                    .whitespace_normal()
                                    .child(error),
                            )
                        })
                        .when(status.state != ServerState::Disabled, |this| {
                            this.child(
                                div().flex().child(
                                    button(
                                        ("reconnect", project_index * 1000 + index),
                                        "Reconnect",
                                        ButtonKind::Normal,
                                        cx,
                                    )
                                    .on_click(cx.listener(
                                        move |_, _, _, cx| {
                                            crate::mcp::reconnect(&reconnect_root, &name, cx);
                                        },
                                    )),
                                ),
                            )
                        }),
                );
            }
        }
        body.into_any_element()
    }
}
