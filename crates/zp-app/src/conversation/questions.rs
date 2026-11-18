//! The agent's questions (`ask`): the card under the call where the user picks options
//! or types answers, and the answered questions and task lists of finished calls.

use futures::channel::oneshot;
use gpui::{
    AnyElement, AppContext as _, ColorExt as _, Context, ElementId, Entity, Focusable as _,
    FontWeight, IntoElement, Window, div, prelude::*, px, rgb_to_hsla,
};
use gpui_elements::editable_text::{EditableTextState, StringStorage, text_input};
use serde_json::Value;
use zp_tools::{Answer, AskOutcome, Question, TodoList, TodoStatus};

use super::{COMPOSER_CARD_RADIUS, ConversationEvent, ConversationView};
use crate::approvals::PendingQuestions;
use crate::theme::ActiveTheme;
use crate::ui::{disclosure_icon, ui_font};

/// Questions of an `ask` call waiting for the user.
pub struct QuestionCard {
    pub call_id: String,
    pub questions: Vec<Question>,
    /// Picked options, per question.
    selected: Vec<Vec<bool>>,
    /// Typed answers, per question.
    texts: Vec<Entity<EditableTextState>>,
    reply: Option<oneshot::Sender<Option<Vec<Answer>>>>,
}

impl ConversationView {
    /// Questions waiting for the user (tests).
    #[cfg(test)]
    pub fn pending_questions(&self) -> Vec<(String, Vec<Question>)> {
        self.questions
            .iter()
            .map(|card| (card.call_id.clone(), card.questions.clone()))
            .collect()
    }

    /// The answer input of question `index` of `call_id` (tests).
    #[cfg(test)]
    pub fn question_input(&self, call_id: &str, index: usize) -> Option<Entity<EditableTextState>> {
        self.questions
            .iter()
            .find(|card| card.call_id == call_id)
            .and_then(|card| card.texts.get(index).cloned())
    }

    pub(super) fn add_questions(&mut self, pending: PendingQuestions, cx: &mut Context<Self>) {
        let questions = pending.request.questions;
        let texts = questions
            .iter()
            .map(|_| cx.new(|cx| EditableTextState::new(StringStorage::default(), cx)))
            .collect();
        let selected = questions
            .iter()
            .map(|question| vec![false; question.options.len()])
            .collect();
        cx.emit(ConversationEvent::QuestionAsked(
            questions
                .first()
                .map(|question| question.question.clone())
                .unwrap_or_default(),
        ));
        let call_id = pending.call_id;
        self.questions.push(QuestionCard {
            call_id: call_id.clone(),
            questions,
            selected,
            texts,
            reply: Some(pending.reply),
        });
        self.progress
            .update(cx, |progress, cx| progress.set_paused(true, cx));
        self.refresh_tool_row(&call_id);
        cx.emit(ConversationEvent::StatusChanged);
        cx.notify();
    }

    /// Picks (or for multiple choice, toggles) option `option` of question `question`.
    pub(crate) fn toggle_question_option(
        &mut self,
        call_id: &str,
        question: usize,
        option: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(card) = self.questions.iter_mut().find(|c| c.call_id == call_id) else {
            return;
        };
        let multi = card
            .questions
            .get(question)
            .is_some_and(|question| question.multi_select);
        let Some(picked) = card.selected.get_mut(question) else {
            return;
        };
        let Some(was) = picked.get(option).copied() else {
            return;
        };
        if !multi {
            picked.iter_mut().for_each(|picked| *picked = false);
        }
        picked[option] = !was || !multi;
        // A single question with a single choice is answered by the click.
        let done = card.questions.len() == 1 && !multi;
        if done {
            self.answer_questions(call_id, cx);
            return;
        }
        self.refresh_tool_row(call_id);
        cx.notify();
    }

    /// Sends the picked options and typed text of `call_id`.
    pub(crate) fn answer_questions(&mut self, call_id: &str, cx: &mut Context<Self>) {
        let Some(card) = self.questions.iter().find(|c| c.call_id == call_id) else {
            return;
        };
        let answers = card
            .questions
            .iter()
            .zip(&card.selected)
            .zip(&card.texts)
            .map(|((question, picked), text)| Answer {
                selected: question
                    .options
                    .iter()
                    .zip(picked)
                    .filter(|(_, picked)| **picked)
                    .map(|(option, _)| option.label.clone())
                    .collect(),
                text: text.read(cx).as_str().trim().to_owned(),
            })
            .collect();
        self.close_questions(call_id, Some(answers), cx);
    }

    /// Dismisses the questions of `call_id` without answering.
    pub(crate) fn dismiss_questions(&mut self, call_id: &str, cx: &mut Context<Self>) {
        self.close_questions(call_id, None, cx);
    }

    fn close_questions(
        &mut self,
        call_id: &str,
        answers: Option<Vec<Answer>>,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.questions.iter().position(|c| c.call_id == call_id) else {
            return;
        };
        let mut card = self.questions.remove(index);
        if !self.needs_attention() && self.is_busy() {
            self.progress
                .update(cx, |progress, cx| progress.set_paused(false, cx));
        }
        if let Some(reply) = card.reply.take() {
            let _ = reply.send(answers);
        }
        self.refresh_tool_row(call_id);
        cx.emit(ConversationEvent::StatusChanged);
        cx.notify();
    }

    /// Enter in an answer field: sends that card's answers.
    pub(super) fn submit_focused_answers(
        &mut self,
        _: &crate::actions::SubmitAnswers,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let focused = self
            .questions
            .iter()
            .find(|card| {
                card.texts
                    .iter()
                    .any(|text| text.read(cx).focus_handle(cx).is_focused(window))
            })
            .map(|card| card.call_id.clone());
        if let Some(call_id) = focused {
            self.answer_questions(&call_id, cx);
            self.focus_composer(window, cx);
        }
    }

    /// Drops pending questions (the run ended; nobody waits for the answers).
    pub(super) fn clear_questions(&mut self) {
        let ids: Vec<String> = self.questions.drain(..).map(|c| c.call_id).collect();
        for id in ids {
            self.refresh_tool_row(&id);
        }
    }

    /// The question card under an `ask` call waiting for the user.
    pub(super) fn render_questions(
        &self,
        call_id: &str,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let card = self.questions.iter().find(|c| c.call_id == call_id)?;
        let colors = cx.theme().colors.clone();
        let accent = colors.accent;
        let button = |id: &str, label: &'static str| {
            div()
                .id(ElementId::Name(format!("{id}-{call_id}").into()))
                .debug_selector(|| format!("question-{id}"))
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
        let single = card.questions.len() == 1;
        let questions = card.questions.iter().enumerate().map(|(q, question)| {
            let options =
                question.options.iter().enumerate().map(|(o, option)| {
                    let picked = card.selected[q][o];
                    let id = call_id.to_owned();
                    div()
                        .id(ElementId::Name(
                            format!("question-option-{call_id}-{q}-{o}").into(),
                        ))
                        .debug_selector(move || format!("question-option-{q}-{o}"))
                        .flex()
                        .items_start()
                        .gap_2()
                        .px_2()
                        .py_1()
                        .rounded(px(4.))
                        .border_1()
                        .border_color(if picked { accent } else { colors.border })
                        .when(picked, |this| this.bg(accent.opacity(0.1)))
                        .cursor_pointer()
                        .hover_style(|s| s.bg(colors.surface_hover))
                        .child(choice_mark(question.multi_select, picked, cx))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .min_w_0()
                                .child(option.label.clone())
                                .when(!option.description.is_empty(), |this| {
                                    this.child(
                                        div()
                                            .text_size(px(12.))
                                            .text_color(colors.text_muted)
                                            .child(option.description.clone()),
                                    )
                                }),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.toggle_question_option(&id, q, o, cx)
                        }))
                });
            let placeholder = if question.options.is_empty() {
                "Your answer"
            } else {
                "Or type your own answer"
            };
            div()
                .flex()
                .flex_col()
                .gap_1p5()
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(question.question.clone()),
                )
                .children(options)
                .child(
                    div().key_context("QuestionAnswer").child(
                        text_input(ElementId::Name(
                            format!("question-text-{call_id}-{q}").into(),
                        ))
                        .state(card.texts[q].downgrade())
                        .placeholder(placeholder)
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
        });
        let id = call_id.to_owned();
        let submit = button("submit", "Answer")
            .bg(accent)
            .text_color(colors.accent_text)
            .border_color(accent)
            .on_click(cx.listener(move |this, _, _, cx| this.answer_questions(&id, cx)));
        let id = call_id.to_owned();
        let dismiss = button("dismiss", "Skip")
            .on_click(cx.listener(move |this, _, _, cx| this.dismiss_questions(&id, cx)));
        Some(
            div()
                .flex()
                .flex_col()
                .gap_3()
                .px_3()
                .py_2p5()
                .border_t_1()
                .border_color(accent)
                .bg(accent.opacity(0.06))
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(colors.text_muted)
                        .child(if single {
                            "The agent has a question"
                        } else {
                            "The agent has some questions"
                        }),
                )
                .children(questions)
                .child(div().flex().gap_2().child(submit).child(dismiss))
                .into_any_element(),
        )
    }

    /// A finished `ask` or `todo` call shown from its result details: the questions
    /// with their answers, or the task list.
    pub(super) fn render_tool_details(
        &self,
        tool: &str,
        details: &Value,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let colors = cx.theme().colors.clone();
        let body = div()
            .flex()
            .flex_col()
            .gap_1()
            .px_3()
            .py_2()
            .border_t_1()
            .border_color(colors.border)
            .text_size(px(13.));
        match tool {
            "todo" => {
                let list = TodoList::from_details(details)?;
                Some(
                    body.when(list.todos.is_empty(), |this| {
                        this.child(
                            div()
                                .text_color(colors.text_muted)
                                .child("(cleared the task list)"),
                        )
                    })
                    .children(list.todos.iter().map(|item| todo_row(item, cx)))
                    .into_any_element(),
                )
            }
            "ask" => {
                let outcome = AskOutcome::from_details(details)?;
                let answers = outcome.answers.as_ref();
                Some(
                    body.gap_2()
                        .children(outcome.questions.iter().enumerate().map(|(q, question)| {
                            let answer = answers.map(|answers| {
                                answers.get(q).map(Answer::summary).unwrap_or_default()
                            });
                            div()
                                .flex()
                                .flex_col()
                                .child(
                                    div()
                                        .text_color(colors.text_muted)
                                        .child(question.question.clone()),
                                )
                                .child(match answer {
                                    Some(answer) => div().child(format!("→ {answer}")),
                                    None => div()
                                        .italic()
                                        .text_color(colors.text_muted)
                                        .child("Skipped"),
                                })
                        }))
                        .into_any_element(),
                )
            }
            _ => None,
        }
    }

    /// The latest task list of the transcript, if it still has open items or a run is
    /// going.
    pub(crate) fn current_todos(&self) -> Option<TodoList> {
        let list = self
            .transcript
            .latest_tool_details("todo")
            .and_then(|details| {
                TodoList::from_details(details).filter(|list| !list.todos.is_empty())
            })?;
        (self.is_busy() || list.completed() < list.todos.len()).then_some(list)
    }

    /// The current task list above the composer: one line, or every item when opened.
    pub(super) fn render_todo_panel(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let list = self.current_todos()?;
        let colors = cx.theme().colors.clone();
        let open = self.todos_open;
        let current = list
            .current()
            .map(|item| item.content.clone())
            .unwrap_or_default();
        Some(
            div()
                .flex()
                .flex_col()
                .gap_0p5()
                .px_3()
                .py_1p5()
                .rounded(px(COMPOSER_CARD_RADIUS))
                .border_1()
                .border_color(colors.border)
                .bg(colors.surface)
                .text_size(px(12.))
                .child(
                    div()
                        .id("todo-panel")
                        .debug_selector(|| "todo-panel".into())
                        .flex()
                        .items_center()
                        .gap_2()
                        .cursor_pointer()
                        .child(disclosure_icon(
                            open,
                            colors.text_muted,
                            (&ui_font(), px(12.)),
                            cx,
                        ))
                        .child(div().font_weight(FontWeight::SEMIBOLD).child(format!(
                            "Tasks {}/{}",
                            list.completed(),
                            list.todos.len()
                        )))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(colors.text_muted)
                                .when(!open, |this| this.child(current)),
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.todos_open = !this.todos_open;
                            cx.notify();
                        })),
                )
                .when(open, |this| {
                    this.children(list.todos.iter().map(|item| todo_row(item, cx)))
                })
                .into_any_element(),
        )
    }
}

/// A drawn radio button (single choice) or checkbox (multiple choice); symbol glyphs
/// render inconsistently across fonts.
fn choice_mark(multi: bool, picked: bool, cx: &Context<ConversationView>) -> impl IntoElement {
    let colors = &cx.theme().colors;
    let border = if picked {
        colors.accent
    } else {
        colors.text_muted
    };
    let mark = div()
        .mt(px(3.))
        .size(px(14.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .border_1()
        .border_color(border);
    if multi {
        mark.rounded(px(3.)).when(picked, |this| {
            this.bg(colors.accent)
                .text_color(colors.accent_text)
                .text_size(px(10.))
                .child("✓")
        })
    } else {
        mark.rounded_full().when(picked, |this| {
            this.child(div().size(px(6.)).rounded_full().bg(colors.accent))
        })
    }
}

fn todo_row(item: &zp_tools::TodoItem, cx: &Context<ConversationView>) -> impl IntoElement {
    let colors = &cx.theme().colors;
    let (mark, color) = match item.status {
        TodoStatus::Completed => ("✓", colors.success),
        TodoStatus::InProgress => ("▸", colors.accent),
        TodoStatus::Pending => ("○", colors.text_muted),
    };
    div()
        .flex()
        .gap_2()
        .child(
            div()
                .w(px(12.))
                .flex_shrink_0()
                .text_color(color)
                .child(mark),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .when(item.status == TodoStatus::Completed, |this| {
                    this.text_color(colors.text_muted).line_through()
                })
                .when(item.status == TodoStatus::InProgress, |this| {
                    this.font_weight(FontWeight::SEMIBOLD)
                })
                .child(item.content.clone()),
        )
}
