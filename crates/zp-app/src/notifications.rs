//! System notifications when a conversation the user is not looking at finishes or needs
//! an approval. Clicking one brings its window and tab to the front.

use std::collections::HashMap;

use gpui::{AnyWindowHandle, App, EntityId, Global, SystemNotification, Window};

use crate::project::ProjectWindow;
use crate::store::AppSettings;

/// Where each notification leads.
#[derive(Default)]
struct Targets(HashMap<String, (AnyWindowHandle, EntityId)>);

impl Global for Targets {}

/// Registers the handler for clicked notifications.
pub fn init(cx: &mut App) {
    cx.set_app_identity("dev.zp.ZP", zp_core::APP_NAME);
    cx.on_system_notification_response(|response, cx| {
        let target = cx
            .try_global::<Targets>()
            .and_then(|targets| targets.0.get(response.tag.as_ref()).copied());
        let Some((window, conversation)) = target else {
            return;
        };
        let _ = window.update(cx, |view, window, cx| {
            if let Ok(project) = view.downcast::<ProjectWindow>() {
                project.update(cx, |view, cx| {
                    view.activate_conversation(conversation, window, cx);
                });
            }
        });
    });
}

/// Whether notifications are on (the default).
pub fn enabled(cx: &App) -> bool {
    cx.try_global::<AppSettings>()
        .is_none_or(|_| AppSettings::get(cx).notifications.unwrap_or(true))
}

/// Shows a notification about `conversation` in `window`.
pub fn notify(window: &Window, conversation: EntityId, title: String, body: String, cx: &mut App) {
    if !enabled(cx) {
        return;
    }
    let tag = format!("zp-conversation-{}", conversation.as_u64());
    cx.default_global::<Targets>()
        .0
        .insert(tag.clone(), (window.window_handle(), conversation));
    cx.show_system_notification(SystemNotification {
        tag: tag.into(),
        title: title.into(),
        body: body.into(),
        actions: Vec::new(),
    });
}
