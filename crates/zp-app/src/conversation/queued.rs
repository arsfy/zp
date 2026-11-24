//! Waiting prompts, their original input, and edits made before delivery.

use gpui::{AppContext as _, Context, Entity, Focusable as _, Window};
use gpui_elements::editable_text::{EditableTextState, StringStorage};
use zp_agent::SteeringId;
use zp_core::attachments::{ImageAttachment, build_message};

use super::ConversationView;
use crate::store::AppModels;

pub(super) struct QueuedPrompt {
    pub id: SteeringId,
    pub text: String,
    pub images: Vec<ImageAttachment>,
}

pub(super) struct QueuedEdit {
    pub id: SteeringId,
    pub input: Entity<EditableTextState>,
}

impl ConversationView {
    pub(super) fn prune_queued_prompts(&mut self) {
        if self.queued_prompts.is_empty() {
            return;
        }
        if let Some(coding) = &self.coding {
            let ids = coding.agent().steering_ids();
            self.queued_prompts
                .retain(|prompt| ids.contains(&prompt.id));
            if self
                .queued_edit
                .as_ref()
                .is_some_and(|edit| !ids.contains(&edit.id))
            {
                self.queued_edit = None;
            }
        }
    }

    pub(super) fn edit_queued(
        &mut self,
        id: SteeringId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.queued_edit.is_some() || self.send_next == Some(id) {
            return;
        }
        let (Some(prompt), Some(coding)) = (
            self.queued_prompts.iter().find(|prompt| prompt.id == id),
            &self.coding,
        ) else {
            return;
        };
        if !coding.agent().hold_steering(id, true) {
            self.prune_queued_prompts();
            cx.notify();
            return;
        }
        let len = prompt.text.len();
        let input =
            cx.new(|cx| EditableTextState::new(StringStorage::from(prompt.text.clone()), cx));
        input.update(cx, |input, cx| input.move_to(len, cx));
        input.read(cx).focus_handle(cx).focus(window, cx);
        self.completion = None;
        self.picker = None;
        self.queued_edit = Some(QueuedEdit { id, input });
        cx.notify();
    }

    pub(super) fn save_queued_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(edit), Some(coding)) = (&self.queued_edit, &self.coding) else {
            return;
        };
        let Some(prompt) = self
            .queued_prompts
            .iter_mut()
            .find(|prompt| prompt.id == edit.id)
        else {
            return;
        };
        let text = edit.input.read(cx).as_str().trim().to_owned();
        if text.is_empty() && prompt.images.is_empty() {
            return;
        }
        let expanded = coding.expand_input(&text);
        let (message, notes) = build_message(&expanded, &self.root, &prompt.images);
        if coding.agent().replace_steering(prompt.id, message) {
            prompt.text = text;
            if !notes.is_empty() {
                self.info = Some(notes.join("\n"));
            }
        }
        self.queued_edit = None;
        self.prune_queued_prompts();
        self.focus_composer(window, cx);
        self.resume_queued(cx);
        cx.notify();
    }

    pub(super) fn cancel_queued_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(edit) = self.queued_edit.take() {
            if let Some(coding) = &self.coding {
                coding.agent().hold_steering(edit.id, false);
            }
            self.focus_composer(window, cx);
            self.resume_queued(cx);
            cx.notify();
        }
    }

    pub(super) fn withdraw_queued(
        &mut self,
        id: SteeringId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (Some(index), Some(coding)) = (
            self.queued_prompts
                .iter()
                .position(|prompt| prompt.id == id),
            &self.coding,
        ) else {
            return;
        };
        if !coding.agent().remove_steering(id) {
            self.prune_queued_prompts();
            cx.notify();
            return;
        }
        if self.send_next == Some(id) {
            self.send_next = None;
        }
        let prompt = self.queued_prompts.remove(index);
        let text = match self.queued_edit.take_if(|edit| edit.id == id) {
            Some(edit) => edit.input.read(cx).as_str().to_owned(),
            None => prompt.text,
        };
        self.attachments.extend(prompt.images);
        if !text.is_empty() {
            let mut draft = self.draft(cx);
            if !draft.is_empty() {
                draft.push_str("\n\n");
            }
            draft.push_str(&text);
            self.set_draft(&draft, cx);
        }
        self.focus_composer(window, cx);
        self.resume_queued(cx);
        cx.notify();
    }

    pub(super) fn send_queued_now(&mut self, id: SteeringId, cx: &mut Context<Self>) {
        if self.compacting
            || self.send_next.is_some()
            || self.queued_edit.as_ref().is_some_and(|edit| edit.id == id)
        {
            return;
        }
        let Some(coding) = &self.coding else {
            return;
        };
        if !coding.agent().prioritize_steering(id) {
            self.prune_queued_prompts();
            cx.notify();
            return;
        }
        if self.running {
            self.send_next = Some(id);
            self.continue_queued = false;
            coding.agent().abort();
            cx.notify();
        } else {
            self.start_queued(id, cx);
        }
    }

    pub(super) fn start_queued(&mut self, id: SteeringId, cx: &mut Context<Self>) {
        let Some(coding) = &self.coding else {
            return;
        };
        let model = self.model();
        let missing_key = model
            .as_ref()
            .filter(|model| !AppModels::has_credentials(&model.provider, cx));
        let unsupported_images = model.as_ref().is_some_and(|model| !model.supports_images())
            && self
                .queued_prompts
                .iter()
                .any(|prompt| prompt.id == id && !prompt.images.is_empty());
        if model.is_none() || missing_key.is_some() || unsupported_images {
            self.needs_model = model.is_none();
            self.needs_key = missing_key.map(|model| model.provider.clone());
            if unsupported_images {
                self.run_error =
                    Some("The selected model does not accept the queued images".into());
            }
            self.continue_queued = false;
            coding.agent().hold_steering(id, false);
            cx.notify();
            return;
        }
        self.needs_model = false;
        self.needs_key = None;
        self.start_run(cx, move |coding| async move {
            coding.agent().wait_for_idle().await;
            coding.agent().prompt_steering(id).await
        });
    }

    pub(super) fn resume_queued(&mut self, cx: &mut Context<Self>) {
        if self.running || self.compacting || self.queued_edit.is_some() || !self.continue_queued {
            return;
        }
        if let Some(coding) = &self.coding
            && let Some(id) = coding.agent().steering_ids().first().copied()
        {
            self.start_queued(id, cx);
        }
    }
}
