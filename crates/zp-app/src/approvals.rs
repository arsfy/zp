//! Permissions in the app: the project's policy and trust, and approvals delivered from
//! the agent (on Tokio) to the conversation view.

use std::path::Path;

use futures::channel::{mpsc, oneshot};
use futures::future::BoxFuture;
use gpui::App;
use zp_core::permissions::{Approver, Decision, PermissionPolicy, PermissionRequest};
use zp_core::settings::ProjectSettings;

use crate::store::{AppPaths, AppStore};

/// A request waiting for the user, with the channel for the answer.
pub struct PendingApproval {
    pub request: PermissionRequest,
    pub reply: oneshot::Sender<Decision>,
}

/// Sends requests to the conversation view; a dropped answer counts as a denial.
pub struct ChannelApprover {
    sender: mpsc::UnboundedSender<PendingApproval>,
}

impl ChannelApprover {
    pub fn new(sender: mpsc::UnboundedSender<PendingApproval>) -> Self {
        Self { sender }
    }
}

impl Approver for ChannelApprover {
    fn approve(&self, request: PermissionRequest) -> BoxFuture<'static, Decision> {
        let (reply, answer) = oneshot::channel();
        let sent = self
            .sender
            .unbounded_send(PendingApproval { request, reply })
            .is_ok();
        Box::pin(async move {
            if !sent {
                return Decision::Deny(None);
            }
            answer.await.unwrap_or(Decision::Deny(None))
        })
    }
}

/// Whether the user trusts the project at `root` (`false` until they said so).
pub fn is_trusted(root: &Path, cx: &App) -> bool {
    AppStore::state(cx).project(root).trusted == Some(true)
}

/// The permission policy of the project at `root`: the mode chosen for it in the app,
/// else the project's settings (if trusted), else the global settings.
pub fn project_policy(root: &Path, cx: &App) -> PermissionPolicy {
    let state = AppStore::state(cx).project(root);
    let data_dir = cx.try_global::<AppPaths>().and_then(|p| p.data_dir.clone());
    let settings = ProjectSettings::load(data_dir.as_deref(), root, state.trusted == Some(true));
    settings.permission_policy(root, state.permission_mode)
}
