//! The searchable review-model dialog shared by conversations and settings.

use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, Focusable as _, IntoElement, MouseButton,
    Render, ScrollHandle, Subscription, Window, div, prelude::*, px, rgb_to_hsla,
};
use gpui_elements::editable_text::{EditableTextState, StringStorage, TextChanged, text_input};
use zp_ai::Model;

use crate::actions::{AcceptCompletion, CloseCompletion, CompletionNext, CompletionPrevious};
use crate::store::AppModels;
use crate::theme::ActiveTheme;

pub enum ReviewModelEvent {
    Selected(String),
    Cancelled,
}

pub struct ReviewModelPicker {
    filter: Entity<EditableTextState>,
    current: Option<String>,
    selected: usize,
    scroll: ScrollHandle,
    _changed: Subscription,
}

impl EventEmitter<ReviewModelEvent> for ReviewModelPicker {}

impl ReviewModelPicker {
    pub fn new(current: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| EditableTextState::new(StringStorage::default(), cx));
        let scroll = ScrollHandle::new();
        let changed = cx.subscribe(&filter, |this, _, _: &TextChanged, cx| {
            this.selected = 0;
            this.scroll.scroll_to_item(0);
            cx.notify();
        });
        filter.read(cx).focus_handle(cx).focus(window, cx);
        let mut this = Self {
            filter,
            current,
            selected: 0,
            scroll,
            _changed: changed,
        };
        this.selected = this
            .choices(cx)
            .iter()
            .position(|(model, _)| this.current.as_deref() == Some(model.qualified_id().as_str()))
            .unwrap_or(0);
        this.scroll.scroll_to_item(this.selected);
        this
    }

    fn choices(&self, cx: &App) -> Vec<(Model, String)> {
        let query = self.filter.read(cx).as_str().trim().to_lowercase();
        let models = &cx.global::<AppModels>().models;
        models
            .available_providers()
            .iter()
            .flat_map(|provider| {
                models
                    .models(Some(provider.id()))
                    .into_iter()
                    .map(|model| (model, provider.name().to_owned()))
            })
            .filter(|(model, provider)| {
                let haystack =
                    format!("{} {} {provider}", model.qualified_id(), model.name).to_lowercase();
                query.split_whitespace().all(|word| haystack.contains(word))
            })
            .take(200)
            .collect()
    }

    pub fn choose(&mut self, spec: &str, cx: &mut Context<Self>) {
        let models = &cx.global::<AppModels>().models;
        if let Some(model) = models.find(spec)
            && models.resolve_auth(&model.provider).is_some()
        {
            cx.emit(ReviewModelEvent::Selected(model.qualified_id()));
        }
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let count = self.choices(cx).len();
        if count > 0 {
            self.selected = (self.selected as isize + delta).rem_euclid(count as isize) as usize;
            self.scroll.scroll_to_item(self.selected);
            cx.notify();
        }
    }
}

impl Render for ReviewModelPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let colors = theme.colors.clone();
        let font_size = theme.metrics.font_size;
        let choices = self.choices(cx);
        let empty = choices.is_empty();
        let no_providers = cx
            .global::<AppModels>()
            .models
            .available_providers()
            .is_empty();
        div()
            .id("review-model-backdrop")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .occlude()
            .flex()
            .justify_center()
            .items_start()
            .px_4()
            .pt_8()
            .pb_4()
            .bg(gpui::rgba(0x0000_0088))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _, _, cx| cx.emit(ReviewModelEvent::Cancelled)),
            )
            .child(
                div()
                    .id("review-model-dialog")
                    .key_context("ReviewModelPicker")
                    .on_action(
                        cx.listener(|this, _: &CompletionNext, _, cx| this.move_selection(1, cx)),
                    )
                    .on_action(cx.listener(|this, _: &CompletionPrevious, _, cx| {
                        this.move_selection(-1, cx)
                    }))
                    .on_action(cx.listener(|this, _: &AcceptCompletion, _, cx| {
                        if let Some((model, _)) = this.choices(cx).get(this.selected) {
                            this.choose(&model.qualified_id(), cx);
                        }
                    }))
                    .on_action(cx.listener(|_, _: &CloseCompletion, _, cx| {
                        cx.emit(ReviewModelEvent::Cancelled)
                    }))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .w_full()
                    .max_w(px(560.))
                    .max_h_full()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3()
                    .rounded(px(8.))
                    .border_1()
                    .border_color(colors.border)
                    .bg(colors.surface)
                    .shadow_lg()
                    .text_size(font_size)
                    .child(div().flex_shrink_0().child("Review model"))
                    .child(
                        text_input("review-model-filter")
                            .state(self.filter.downgrade())
                            .placeholder("Filter models...")
                            .placeholder_color(rgb_to_hsla(colors.text_muted))
                            .caret_color(rgb_to_hsla(colors.text))
                            .px_2()
                            .py_1()
                            .w_full()
                            .flex_shrink_0()
                            .rounded(px(4.))
                            .border_1()
                            .border_color(colors.border)
                            .whitespace_nowrap(),
                    )
                    .child(
                        div()
                            .id("review-model-list")
                            .min_h_0()
                            .max_h(px(320.))
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll)
                            .children(choices.into_iter().enumerate().map(
                                |(index, (model, provider))| {
                                    let spec = model.qualified_id();
                                    let current = self.current.as_deref() == Some(spec.as_str());
                                    let label = crate::ui::model_display_name(&spec).to_owned();
                                    div()
                                        .id(("review-model", index))
                                        .debug_selector(move || format!("review-model-{index}"))
                                        .h(px(30.))
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .px_2()
                                        .rounded(px(4.))
                                        .cursor_pointer()
                                        .when(index == self.selected, |this| {
                                            this.bg(colors.surface_hover)
                                        })
                                        .hover_style(|s| s.bg(colors.surface_hover))
                                        .child(div().w(px(12.)).flex_shrink_0().child(if current {
                                            "\u{2713}"
                                        } else {
                                            ""
                                        }))
                                        .child(div().flex_1().min_w_0().truncate().child(label))
                                        .child(
                                            div()
                                                .flex_shrink_0()
                                                .max_w(px(180.))
                                                .truncate()
                                                .text_size(px(11.))
                                                .text_color(colors.text_muted)
                                                .child(format!(
                                                    "{provider} {}k",
                                                    model.context_window / 1000
                                                )),
                                        )
                                        .tooltip(crate::ui::tooltip(spec.clone()))
                                        .on_click(
                                            cx.listener(move |this, _, _, cx| {
                                                this.choose(&spec, cx)
                                            }),
                                        )
                                },
                            ))
                            .when(empty, |this| {
                                this.child(div().px_2().py_3().text_color(colors.text_muted).child(
                                    if no_providers {
                                        "No available models"
                                    } else {
                                        "No matching models"
                                    },
                                ))
                            }),
                    )
                    .child(
                        div().flex().justify_end().flex_shrink_0().child(
                            div()
                                .id("cancel-review-model")
                                .px_3()
                                .py_1()
                                .rounded(px(4.))
                                .cursor_pointer()
                                .hover_style(|s| s.bg(colors.surface_hover))
                                .child("Cancel")
                                .on_click(
                                    cx.listener(|_, _, _, cx| cx.emit(ReviewModelEvent::Cancelled)),
                                ),
                        ),
                    ),
            )
    }
}
