//! Plan mode and the approval step between planning and implementation.

use std::cell::RefCell;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui::{AnyElement, Context, IntoElement, Window, div, prelude::*, px};
use zp_agent::AgentMessage;
use zp_ai::UserContent;
use zp_core::plan::SUBMIT_PLAN;

use super::render::{ghost_button, icon_button};
use super::{ConversationEvent, ConversationView};
use crate::store::AppPaths;
use crate::theme::ActiveTheme;

const ICON_CLOSE: &[u8] = include_bytes!("../../../../assets/icons/x.svg");

pub(super) enum PlanMode {
    Off,
    Planning,
    Closing,
    Review {
        markdown: String,
        path: Option<PathBuf>,
    },
}

impl PlanMode {
    pub(super) fn is_closing(&self) -> bool {
        matches!(self, Self::Closing)
    }

    pub(super) fn is_enabled(&self) -> bool {
        !matches!(self, Self::Off)
    }

    pub(super) fn blocks_prompt(&self, running: bool) -> bool {
        matches!(self, Self::Closing | Self::Review { .. }) || running && self.is_enabled()
    }
}

pub(super) fn command_goal(input: &str) -> Option<&str> {
    let (name, args) = input.split_once(char::is_whitespace).unwrap_or((input, ""));
    (name == "/plan").then(|| args.trim())
}

fn save_plan(data_dir: Option<&Path>, markdown: &str) -> anyhow::Result<PathBuf> {
    let mut builder = tempfile::Builder::new();
    builder.prefix("plan-").suffix(".md");
    let mut file = if let Some(data_dir) = data_dir {
        let dir = data_dir.join("plans");
        std::fs::create_dir_all(&dir)?;
        builder.tempfile_in(dir)?
    } else {
        builder.tempfile()?
    };
    writeln!(file, "{markdown}")?;
    let (_, path) = file.keep()?;
    Ok(path)
}

impl ConversationView {
    pub(crate) fn has_plan_for_review(&self) -> bool {
        matches!(self.plan, PlanMode::Review { .. })
    }

    pub(super) fn enable_plan(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(coding) = &self.coding else {
            return false;
        };
        if self.plan.blocks_prompt(self.running) {
            return false;
        }
        if self.running || !coding.agent().steering_ids().is_empty() || self.queued_edit.is_some() {
            self.info = Some(
                "Finish the current run and clear the message queue to enter Plan mode.".into(),
            );
            cx.notify();
            return false;
        }
        coding.set_plan_mode(true);
        self.plan = PlanMode::Planning;
        self.picker = None;
        cx.notify();
        true
    }

    pub(super) fn collect_plan(&mut self, messages: &[AgentMessage], cx: &mut Context<Self>) {
        if !self.running || !matches!(self.plan, PlanMode::Planning) || self.has_failed() {
            return;
        }
        let Some(result) = messages
            .iter()
            .rev()
            .filter_map(AgentMessage::as_tool_result)
            .find(|result| result.tool_name == SUBMIT_PLAN && !result.is_error)
        else {
            return;
        };
        let markdown: String = result
            .content
            .iter()
            .filter_map(UserContent::as_text)
            .collect();
        let data_dir = cx
            .try_global::<AppPaths>()
            .and_then(|paths| paths.data_dir.as_deref());
        let path = match save_plan(data_dir, &markdown) {
            Ok(path) => Some(path),
            Err(error) => {
                self.info = Some(format!("Could not save the Markdown plan: {error:#}"));
                None
            }
        };
        self.plan = PlanMode::Review { markdown, path };
    }

    pub(super) fn leave_plan(&mut self) {
        if let Some(coding) = &self.coding {
            coding.set_plan_mode(false);
        }
        self.plan = PlanMode::Off;
    }

    fn close_plan(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.running {
            self.plan = PlanMode::Closing;
            self.abort(cx);
        } else {
            self.leave_plan();
        }
        self.focus_composer(window, cx);
        cx.emit(ConversationEvent::StatusChanged);
        cx.notify();
    }

    fn approve_plan(&mut self, fresh: bool, window: &mut Window, cx: &mut Context<Self>) {
        let PlanMode::Review { markdown, .. } = &self.plan else {
            return;
        };
        if self.is_busy() {
            return;
        }
        let prompt =
            format!("Implement the approved plan below and verify the result.\n\n{markdown}");
        let Some(model) = self.model_for_send(cx) else {
            return;
        };
        let session = if fresh {
            let mut session = crate::project::new_session(&self.root, cx);
            let selection = session
                .append_model_change(&model.provider, &model.id)
                .and_then(|_| session.append_thinking_level_change(self.effective_thinking()));
            if let Err(error) = selection {
                self.info = Some(format!("Could not start a fresh conversation: {error:#}"));
                cx.notify();
                return;
            }
            Some(Rc::new(RefCell::new(Some(session))))
        } else {
            None
        };
        self.leave_plan();
        self.info = None;
        if let Some(session) = session {
            cx.emit(ConversationEvent::ExecutePlanFresh { session, prompt });
            cx.notify();
        } else {
            self.execute_approved_plan(prompt, window, cx);
        }
    }

    pub(crate) fn execute_approved_plan(
        &mut self,
        prompt: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.emit(ConversationEvent::Activity);
        self.picker = None;
        self.start_run(cx, move |coding| async move { coding.prompt(prompt).await });
        self.focus_composer(window, cx);
    }

    pub(super) fn render_plan_chip(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors.clone();
        div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap_0p5()
            .pl_2()
            .pr_0p5()
            .rounded(px(4.))
            .bg(colors.surface_hover)
            .text_size(px(12.))
            .text_color(colors.accent)
            .child("Plan")
            .child(
                icon_button("close-plan", ICON_CLOSE, "Exit Plan mode", cx)
                    .size(px(20.))
                    .debug_selector(|| "close-plan".into())
                    .on_click(cx.listener(|this, _, window, cx| this.close_plan(window, cx))),
            )
            .into_any_element()
    }

    pub(super) fn render_plan_review(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let PlanMode::Review { path, .. } = &self.plan else {
            return None;
        };
        let colors = cx.theme().colors.clone();
        Some(
            div()
                .debug_selector(|| "plan-review".into())
                .flex()
                .items_center()
                .flex_wrap()
                .gap_1()
                .px_2()
                .py_1()
                .rounded(px(6.))
                .border_1()
                .border_color(colors.border)
                .bg(colors.surface)
                .child(div().px_1().text_size(px(12.)).child("Execute this plan?"))
                .child(
                    ghost_button("execute-plan", "Execute", cx)
                        .debug_selector(|| "execute-plan".into())
                        .text_color(colors.accent)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.approve_plan(false, window, cx)),
                        ),
                )
                .child(
                    ghost_button("execute-plan-fresh", "Clear Context & Execute", cx)
                        .debug_selector(|| "execute-plan-fresh".into())
                        .on_click(
                            cx.listener(|this, _, window, cx| this.approve_plan(true, window, cx)),
                        ),
                )
                .child(
                    ghost_button("cancel-plan", "Cancel", cx)
                        .debug_selector(|| "cancel-plan".into())
                        .on_click(cx.listener(|this, _, window, cx| this.close_plan(window, cx))),
                )
                .when_some(path.clone(), |this, path| {
                    this.child(
                        ghost_button("show-plan-file", "Show Markdown File", cx)
                            .on_click(move |_, _, cx| cx.reveal_path(&path)),
                    )
                })
                .into_any_element(),
        )
    }
}
