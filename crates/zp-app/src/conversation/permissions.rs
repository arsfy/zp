//! Tool call approvals in a conversation: pending requests, the approval card and the
//! permission mode picker.

use std::rc::Rc;

use futures::channel::oneshot;
use gpui::{
    AnyElement, App, AppContext as _, ColorExt as _, Context, ElementId, Entity, Focusable as _,
    FontWeight, IntoElement, Rgba, Window, div, prelude::*, px, rgb_to_hsla,
};
use gpui_elements::editable_text::{EditableTextState, StringStorage, text_input};
use zp_core::diff::{DEFAULT_CONTEXT, FileDiff};
use zp_core::permissions::{AskReason, Decision, PermissionMode, PermissionPolicy, ToolKind};

use super::files::ToolDiff;
use super::{ConversationEvent, ConversationView};
use crate::approvals::PendingApproval;
use crate::diff_view::{DiffColors, render_unified_row, unified_rows};
use crate::review_model_picker::{ReviewModelEvent, ReviewModelPicker};
use crate::store::AppStore;
use crate::theme::ActiveTheme;

/// Diff rows shown in an approval card.
const PREVIEW_ROWS: usize = 30;

pub(super) fn permission_mode_color(mode: PermissionMode, cx: &App) -> Option<Rgba> {
    let colors = &cx.theme().colors;
    match mode {
        PermissionMode::Ask => None,
        PermissionMode::AutoEdit => Some(colors.success),
        PermissionMode::AutoReview => Some(colors.accent),
        PermissionMode::FullAuto => Some(colors.warning),
    }
}

/// A tool call waiting for the user.
pub struct Approval {
    pub request: zp_core::permissions::PermissionRequest,
    reply: Option<oneshot::Sender<Decision>>,
    /// Why the user denies (sent to the model).
    reason: Entity<EditableTextState>,
    /// What a file change would do.
    preview: Option<Rc<ToolDiff>>,
}

impl ConversationView {
    /// Tool calls waiting for the user.
    #[cfg(test)]
    pub fn pending_approvals(
        &self,
    ) -> impl Iterator<Item = &zp_core::permissions::PermissionRequest> {
        self.approvals.iter().map(|approval| &approval.request)
    }

    /// Whether the conversation waits for the user.
    pub fn needs_attention(&self) -> bool {
        !self.approvals.is_empty()
    }

    /// The mode of this conversation's permission checks.
    pub fn permission_mode(&self) -> PermissionMode {
        self.coding
            .as_ref()
            .and_then(|coding| coding.permissions())
            .map(|gate| gate.policy().mode)
            .unwrap_or_default()
    }

    /// Applies new rules (the mode or settings changed); session grants are kept.
    pub fn set_permission_policy(&mut self, policy: PermissionPolicy, cx: &mut Context<Self>) {
        if let Some(gate) = self.coding.as_ref().and_then(|c| c.permissions()) {
            gate.set_policy(policy);
        }
        cx.notify();
    }

    /// Loads or drops the project's instruction files.
    pub fn set_trusted(&mut self, trusted: bool) {
        if let Some(coding) = &self.coding {
            coding.set_trusted(trusted);
        }
    }

    /// The agent's transcript (tests).
    #[cfg(test)]
    pub fn agent_messages(&self) -> Vec<zp_agent::AgentMessage> {
        self.coding
            .as_ref()
            .map(|coding| coding.agent().messages())
            .unwrap_or_default()
    }

    /// The system prompt in use (tests).
    #[cfg(test)]
    pub fn system_prompt(&self) -> String {
        self.coding
            .as_ref()
            .map(|coding| coding.system_prompt())
            .unwrap_or_default()
    }

    /// The reason input of the approval for `call_id` (tests).
    #[cfg(test)]
    pub fn approval_reason(&self, call_id: &str) -> Option<Entity<EditableTextState>> {
        self.approvals
            .iter()
            .find(|a| a.request.tool_call_id == call_id)
            .map(|a| a.reason.clone())
    }

    /// The preview of the approval for `call_id` (tests).
    #[cfg(test)]
    pub fn approval_preview(&self, call_id: &str) -> Option<(usize, usize)> {
        self.approvals
            .iter()
            .find(|a| a.request.tool_call_id == call_id)
            .and_then(|a| a.preview.as_ref())
            .map(|p| (p.diff.added, p.diff.removed))
    }

    pub(super) fn add_approval(&mut self, pending: PendingApproval, cx: &mut Context<Self>) {
        let request = pending.request;
        let reason = cx.new(|cx| EditableTextState::new(StringStorage::default(), cx));
        let preview = self.change_preview(&request).map(Rc::new);
        let call_id = request.tool_call_id.clone();
        cx.emit(ConversationEvent::ApprovalRequested(
            request.summary.clone(),
        ));
        self.approvals.push(Approval {
            request,
            reply: Some(pending.reply),
            reason,
            preview,
        });
        self.refresh_tool_row(&call_id);
        cx.emit(ConversationEvent::StatusChanged);
        cx.notify();
    }

    /// What a pending `write` / `edit` would change, from the arguments of its call.
    fn change_preview(
        &self,
        request: &zp_core::permissions::PermissionRequest,
    ) -> Option<ToolDiff> {
        if request.kind != ToolKind::Edit {
            return None;
        }
        let args = &request.args;
        let path = zp_tools::resolve_path(&self.root, args["path"].as_str()?);
        let before = std::fs::read_to_string(&path).ok();
        let after = match request.tool_name.as_str() {
            "write" => args["content"].as_str()?.to_owned(),
            "edit" => {
                let edits: Vec<zp_tools::EditSpec> = args["edits"]
                    .as_array()?
                    .iter()
                    .map(|edit| zp_tools::EditSpec {
                        old_text: edit["oldText"].as_str().unwrap_or_default().to_owned(),
                        new_text: edit["newText"].as_str().unwrap_or_default().to_owned(),
                    })
                    .collect();
                zp_tools::apply_edits(before.as_deref()?, &edits, "file").ok()?
            }
            _ => return None,
        };
        let diff = FileDiff::compute(before.as_deref(), Some(&after), DEFAULT_CONTEXT);
        let rows = unified_rows(&diff);
        Some(ToolDiff { diff, rows })
    }

    /// Answers the approval of tool call `call_id`.
    pub(crate) fn answer_approval(
        &mut self,
        call_id: &str,
        decision: Decision,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self
            .approvals
            .iter()
            .position(|a| a.request.tool_call_id == call_id)
        else {
            return;
        };
        let mut approval = self.approvals.remove(index);
        if let Some(reply) = approval.reply.take() {
            let _ = reply.send(decision);
        }
        self.refresh_tool_row(call_id);
        cx.emit(ConversationEvent::StatusChanged);
        cx.notify();
    }

    /// Denies `call_id` with the reason typed in its card.
    pub(crate) fn deny_approval(&mut self, call_id: &str, cx: &mut Context<Self>) {
        let reason = self
            .approvals
            .iter()
            .find(|a| a.request.tool_call_id == call_id)
            .map(|a| a.reason.read(cx).as_str().trim().to_owned())
            .filter(|reason| !reason.is_empty());
        self.answer_approval(call_id, Decision::Deny(reason), cx);
    }

    /// Enter in a reason field: denies that call with the reason.
    pub(super) fn deny_focused_approval(
        &mut self,
        _: &crate::actions::DenyToolCall,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let focused = self
            .approvals
            .iter()
            .find(|a| a.reason.read(cx).focus_handle(cx).is_focused(window))
            .map(|a| a.request.tool_call_id.clone());
        if let Some(call_id) = focused {
            self.deny_approval(&call_id, cx);
            self.focus_composer(window, cx);
        }
    }

    /// Denies the first pending call with `reason` (screenshots).
    #[cfg(feature = "visual-tests")]
    pub fn deny_first_approval(&mut self, reason: &str, cx: &mut Context<Self>) {
        if let Some(approval) = self.approvals.first() {
            let id = approval.request.tool_call_id.clone();
            approval
                .reason
                .update(cx, |input, cx| input.emplace(reason, cx));
            self.deny_approval(&id, cx);
        }
    }

    /// Drops pending approvals (the run ended; the gate stopped waiting).
    pub(super) fn clear_approvals(&mut self) {
        let ids: Vec<String> = self
            .approvals
            .drain(..)
            .map(|a| a.request.tool_call_id)
            .collect();
        for id in ids {
            self.refresh_tool_row(&id);
        }
    }

    fn refresh_tool_row(&mut self, call_id: &str) {
        if let Some(row) = self.transcript.row_of_tool(call_id) {
            self.list_state.splice(row..row + 1, 1);
        }
    }

    pub(crate) fn select_permission_mode(
        &mut self,
        mode: PermissionMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if mode == PermissionMode::AutoReview {
            let current = self
                .coding
                .as_ref()
                .and_then(|coding| coding.permissions())
                .and_then(|gate| gate.policy().review_model);
            let picker = cx.new(|cx| ReviewModelPicker::new(current, window, cx));
            self.review_model_subscription = Some(cx.subscribe_in(
                &picker,
                window,
                |this, _, event: &ReviewModelEvent, window, cx| {
                    this.review_model_picker = None;
                    this.review_model_subscription = None;
                    if let ReviewModelEvent::Selected(spec) = event {
                        AppStore::update(cx, |state| {
                            let project = state.project_mut(&this.root);
                            project.review_model = Some(spec.clone());
                            project.permission_mode = Some(PermissionMode::AutoReview);
                        });
                        let policy = crate::approvals::project_policy(&this.root, cx);
                        this.set_permission_policy(policy, cx);
                        this.apply_permission_mode(PermissionMode::AutoReview, window, cx);
                    } else {
                        this.focus_composer(window, cx);
                        cx.notify();
                    }
                },
            ));
            self.review_model_picker = Some(picker);
            self.picker = None;
            cx.notify();
            return;
        }
        self.review_model_picker = None;
        self.review_model_subscription = None;
        self.apply_permission_mode(mode, window, cx);
    }

    fn apply_permission_mode(
        &mut self,
        mode: PermissionMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(gate) = self.coding.as_ref().and_then(|c| c.permissions()) {
            gate.set_mode(mode);
        }
        self.picker = None;
        self.focus_composer(window, cx);
        cx.emit(ConversationEvent::PermissionModeChanged(mode));
        cx.notify();
    }

    pub(super) fn render_permission_picker(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let current = self.permission_mode();
        let items = PermissionMode::ALL
            .into_iter()
            .enumerate()
            .map(|(index, mode)| {
                div()
                    .id(("permission-mode", index))
                    .flex()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded(px(4.))
                    .cursor_pointer()
                    .hover_style(|s| s.bg(colors.surface_hover))
                    .when_some(permission_mode_color(mode, cx), |this, color| {
                        this.text_color(color)
                    })
                    .child(div().w(px(12.)).flex_shrink_0().child(if mode == current {
                        "✓"
                    } else {
                        ""
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(mode.label())
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(colors.text_muted)
                                    .child(mode.description()),
                            ),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.select_permission_mode(mode, window, cx)
                    }))
            });
        self.popover(div().w(px(320.)).p_1().children(items), cx)
    }

    /// The approval card under a tool call waiting for the user.
    pub(super) fn render_approval(
        &self,
        call_id: &str,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let approval = self
            .approvals
            .iter()
            .find(|a| a.request.tool_call_id == call_id)?;
        let request = &approval.request;
        let colors = cx.theme().colors.clone();
        let mono = cx.theme().metrics.mono_font.clone();
        let dangerous = request
            .reasons
            .iter()
            .any(|r| matches!(r, AskReason::Dangerous(_)));
        let outside = request
            .reasons
            .iter()
            .any(|r| matches!(r, AskReason::OutsideProject(_)));
        let title = match request.kind {
            ToolKind::Execute => "Run this command?",
            ToolKind::Edit if outside => "Change a file outside the project?",
            ToolKind::Edit => "Apply this change?",
            ToolKind::Read => "Read outside the project?",
            ToolKind::Other => "Allow this tool call?",
        };
        let accent = if dangerous {
            colors.error
        } else {
            colors.warning
        };
        let button = |id: &str, label: String| {
            div()
                .id(ElementId::Name(format!("{id}-{call_id}").into()))
                .debug_selector(|| format!("approval-{id}"))
                .px_2p5()
                .py_1()
                .rounded(px(4.))
                .cursor_pointer()
                .text_size(px(12.))
                .border_1()
                .border_color(colors.border)
                .hover_style(|s| s.bg(colors.surface_hover))
                .child(label)
        };
        let id = call_id.to_owned();
        let approve = button("approve", "Approve".into())
            .bg(colors.accent)
            .text_color(colors.accent_text)
            .border_color(colors.accent)
            .on_click(
                cx.listener(move |this, _, _, cx| this.answer_approval(&id, Decision::Allow, cx)),
            );
        let id = call_id.to_owned();
        let for_session = (!request.grants.is_empty()).then(|| {
            let grants: Vec<String> = request.grants.iter().map(|g| g.describe()).collect();
            button(
                "approve-session",
                format!("Approve {} for this session", grants.join(", ")),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.answer_approval(&id, Decision::AllowForSession, cx)
            }))
        });
        let id = call_id.to_owned();
        let deny = button("deny", "Deny".into())
            .text_color(colors.error)
            .on_click(cx.listener(move |this, _, _, cx| this.deny_approval(&id, cx)));
        let diff_colors = DiffColors::of(cx);

        Some(
            div()
                .flex()
                .flex_col()
                .gap_2()
                .px_3()
                .py_2p5()
                .border_t_1()
                .border_color(accent)
                .bg(accent.opacity(0.06))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(div().text_color(accent).child("⚠"))
                        .child(div().font_weight(FontWeight::SEMIBOLD).child(title)),
                )
                .children(request.reasons.iter().map(|reason| {
                    div()
                        .text_size(px(12.))
                        .text_color(if matches!(reason, AskReason::Dangerous(_)) {
                            colors.error
                        } else {
                            colors.text_muted
                        })
                        .child(reason.describe())
                }))
                .when_some(request.command.clone(), |this, command| {
                    this.child(
                        div()
                            .px_2()
                            .py_1p5()
                            .rounded(px(4.))
                            .bg(colors.code_background)
                            .font_family(mono.clone())
                            .text_size(px(12.))
                            .child(format!("$ {command}")),
                    )
                })
                .when_some(approval.preview.clone(), |this, preview| {
                    let hidden = preview.rows.len().saturating_sub(PREVIEW_ROWS);
                    this.child(
                        div()
                            .rounded(px(4.))
                            .overflow_hidden()
                            .bg(colors.code_background)
                            .children(
                                preview
                                    .rows
                                    .iter()
                                    .take(PREVIEW_ROWS)
                                    .map(|row| render_unified_row(row, &diff_colors)),
                            )
                            .when(hidden > 0, |this| {
                                this.child(
                                    div()
                                        .px_3()
                                        .py_1()
                                        .text_size(px(12.))
                                        .text_color(colors.text_muted)
                                        .child(format!("… {hidden} more lines")),
                                )
                            }),
                    )
                })
                .child(
                    div().key_context("ApprovalReason").child(
                        text_input(ElementId::Name(format!("deny-reason-{call_id}").into()))
                            .state(approval.reason.downgrade())
                            .placeholder("Reason for denying (optional, sent to the agent)")
                            .placeholder_color(rgb_to_hsla(colors.text_muted))
                            .caret_color(rgb_to_hsla(colors.text))
                            .w_full()
                            .px_2()
                            .py_1()
                            .rounded(px(4.))
                            .border_1()
                            .border_color(colors.border)
                            .bg(colors.background)
                            .text_size(px(12.))
                            .whitespace_nowrap(),
                    ),
                )
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .child(approve)
                        .children(for_session)
                        .child(deny),
                )
                .into_any_element(),
        )
    }
}
