//! Permissions in the app: the project's policy and trust, and approvals delivered from
//! the agent (on Tokio) to the conversation view.

use std::path::Path;

use futures::channel::{mpsc, oneshot};
use futures::future::BoxFuture;
use gpui::App;
use zp_core::permissions::{
    Approver, Decision, PermissionMode, PermissionPolicy, PermissionRequest,
};
use zp_core::settings::ProjectSettings;

use crate::store::{AppModels, AppPaths, AppStore};

/// A request waiting for the user, with the channel for the answer.
pub struct PendingApproval {
    pub request: PermissionRequest,
    pub reply: oneshot::Sender<Decision>,
}

/// What the permission gate tells the conversation view.
pub enum GateEvent {
    /// A call waits for the user (boxed: a request is much larger than the other event).
    Approval(Box<PendingApproval>),
    /// The review model started or stopped looking at a call.
    Reviewing { call_id: String, active: bool },
}

/// Sends requests to the conversation view; a dropped answer counts as a denial.
pub struct ChannelApprover {
    sender: mpsc::UnboundedSender<GateEvent>,
}

impl ChannelApprover {
    pub fn new(sender: mpsc::UnboundedSender<GateEvent>) -> Self {
        Self { sender }
    }
}

impl Approver for ChannelApprover {
    fn reviewing(&self, tool_call_id: &str, active: bool) {
        let _ = self.sender.unbounded_send(GateEvent::Reviewing {
            call_id: tool_call_id.to_owned(),
            active,
        });
    }

    fn approve(&self, request: PermissionRequest) -> BoxFuture<'static, Decision> {
        let (reply, answer) = oneshot::channel();
        let sent = self
            .sender
            .unbounded_send(GateEvent::Approval(Box::new(PendingApproval {
                request,
                reply,
            })))
            .is_ok();
        Box::pin(async move {
            if !sent {
                return Decision::Deny(None);
            }
            answer.await.unwrap_or(Decision::Deny(None))
        })
    }
}

/// Questions of an `ask` call waiting for the user, with the channel for the answers.
pub struct PendingQuestions {
    pub call_id: String,
    pub request: zp_tools::AskRequest,
    pub reply: oneshot::Sender<Option<Vec<zp_tools::Answer>>>,
}

/// Sends the agent's questions to the conversation view; a dropped request counts as
/// dismissed.
pub struct ChannelAsker {
    sender: mpsc::UnboundedSender<PendingQuestions>,
}

impl ChannelAsker {
    pub fn new(sender: mpsc::UnboundedSender<PendingQuestions>) -> Self {
        Self { sender }
    }
}

impl zp_tools::Asker for ChannelAsker {
    fn ask(
        &self,
        call_id: &str,
        request: zp_tools::AskRequest,
    ) -> BoxFuture<'static, Option<Vec<zp_tools::Answer>>> {
        let (reply, answer) = oneshot::channel();
        let sent = self
            .sender
            .unbounded_send(PendingQuestions {
                call_id: call_id.to_owned(),
                request,
                reply,
            })
            .is_ok();
        Box::pin(async move {
            if !sent {
                return None;
            }
            answer.await.ok().flatten()
        })
    }
}

/// Whether the user trusts the project at `root` (`false` until they said so).
pub fn is_trusted(root: &Path, cx: &App) -> bool {
    AppStore::state(cx).project(root).trusted == Some(true)
}

/// The delegation policy of the project at `root`: the personal settings, restricted by
/// the project's settings when it is trusted.
pub fn project_subagents(root: &Path, cx: &App) -> zp_core::subagents::SubagentSettings {
    let project = is_trusted(root, cx).then(|| {
        zp_core::settings::Settings::load_or_default(&zp_core::settings::project_settings_file(
            root,
        ))
    });
    zp_core::subagents::SubagentSettings::effective(
        crate::store::AppSettings::get(cx).subagents.as_ref(),
        project
            .as_ref()
            .and_then(|settings| settings.subagents.as_ref()),
    )
}

/// The permission policy of the project at `root`: the mode chosen for it in the app,
/// else the project's settings (if trusted), else the global settings.
pub fn project_policy(root: &Path, cx: &App) -> PermissionPolicy {
    let state = AppStore::state(cx).project(root);
    let data_dir = cx.try_global::<AppPaths>().and_then(|p| p.data_dir.clone());
    let settings = ProjectSettings::load(data_dir.as_deref(), root, state.trusted == Some(true));
    let mut policy = settings.permission_policy(root, state.permission_mode);
    if let Some(model) = state.review_model {
        policy.review_model = Some(model);
    }
    if policy.mode == PermissionMode::AutoReview
        && !cx
            .try_global::<AppModels>()
            .is_some_and(|app| policy.review_is_configured(&app.models))
    {
        policy.mode = PermissionMode::Ask;
    }
    policy
}
