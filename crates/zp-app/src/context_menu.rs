//! A right-click menu: a list of actions shown at the pointer, dismissed by choosing an
//! item, clicking elsewhere or pressing Escape.

use std::rc::Rc;

use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, Render,
    SharedString, Window, div, prelude::*, px,
};

use crate::actions::DismissMenu;
use crate::theme::ActiveTheme;

type Handler = Rc<dyn Fn(&mut Window, &mut App)>;

enum Item {
    Action {
        label: SharedString,
        destructive: bool,
        /// A toggle: whether it is on.
        checked: Option<bool>,
        handler: Handler,
    },
    Separator,
}

/// Builder and view of a context menu.
pub struct ContextMenu {
    items: Vec<Item>,
    focus_handle: FocusHandle,
}

impl EventEmitter<DismissEvent> for ContextMenu {}

impl ContextMenu {
    /// Creates a focused menu from `build`.
    pub fn new(
        window: &mut Window,
        cx: &mut App,
        build: impl FnOnce(Self) -> Self,
    ) -> Entity<Self> {
        let menu = cx.new(|cx| {
            build(Self {
                items: Vec::new(),
                focus_handle: cx.focus_handle(),
            })
        });
        menu.read(cx).focus_handle.clone().focus(window, cx);
        menu
    }

    /// Adds an item.
    pub fn item(
        mut self,
        label: impl Into<SharedString>,
        handler: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        self.items.push(Item::Action {
            label: label.into(),
            destructive: false,
            checked: None,
            handler: Rc::new(handler),
        });
        self
    }

    /// Adds an option that is on or off, drawn with a check mark when on.
    pub fn toggle_item(
        mut self,
        label: impl Into<SharedString>,
        checked: bool,
        handler: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        self.items.push(Item::Action {
            label: label.into(),
            destructive: false,
            checked: Some(checked),
            handler: Rc::new(handler),
        });
        self
    }

    /// Adds an item drawn as destructive (e.g. Delete).
    pub fn destructive_item(
        mut self,
        label: impl Into<SharedString>,
        handler: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        self.items.push(Item::Action {
            label: label.into(),
            destructive: true,
            checked: None,
            handler: Rc::new(handler),
        });
        self
    }

    /// Adds a separator.
    pub fn separator(mut self) -> Self {
        self.items.push(Item::Separator);
        self
    }

    /// Item labels (tests).
    #[cfg(test)]
    pub fn labels(&self) -> Vec<String> {
        self.items
            .iter()
            .filter_map(|item| match item {
                Item::Action { label, .. } => Some(label.to_string()),
                Item::Separator => None,
            })
            .collect()
    }

    /// Whether the toggle labelled `label` is on (tests).
    #[cfg(test)]
    pub fn is_checked(&self, label: &str) -> Option<bool> {
        self.items.iter().find_map(|item| match item {
            Item::Action {
                label: l, checked, ..
            } if l.as_ref() == label => *checked,
            _ => None,
        })
    }

    /// The action of the item labelled `label` (tests).
    #[cfg(test)]
    pub fn handler(&self, label: &str) -> Handler {
        self.items
            .iter()
            .find_map(|item| match item {
                Item::Action {
                    label: l, handler, ..
                } if l.as_ref() == label => Some(handler.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no menu item {label}"))
    }
}

impl Focusable for ContextMenu {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ContextMenu {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        let toggles = self.items.iter().any(|item| {
            matches!(
                item,
                Item::Action {
                    checked: Some(_),
                    ..
                }
            )
        });
        let items = self
            .items
            .iter()
            .enumerate()
            .map(|(index, item)| match item {
                Item::Separator => div().h(px(1.)).my_1().bg(colors.border).into_any_element(),
                Item::Action {
                    label,
                    destructive,
                    checked,
                    handler,
                } => {
                    let handler = handler.clone();
                    div()
                        .id(("menu-item", index))
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .py_1()
                        .rounded(px(4.))
                        .cursor_pointer()
                        .when(*destructive, |this| this.text_color(colors.error))
                        .hover_style(|s| s.bg(colors.surface_hover))
                        .when(toggles, |this| {
                            this.child(
                                div()
                                    .w(px(12.))
                                    .flex_shrink_0()
                                    .text_color(colors.accent)
                                    .child(if *checked == Some(true) { "✓" } else { "" }),
                            )
                        })
                        .child(label.clone())
                        .on_click(cx.listener(move |_, _, window, cx| {
                            cx.emit(DismissEvent);
                            handler(window, cx);
                        }))
                        .into_any_element()
                }
            });
        div()
            .key_context("ContextMenu")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|_, _: &DismissMenu, _, cx| cx.emit(DismissEvent)))
            .on_mouse_down_out(cx.listener(|_, _, _, cx| cx.emit(DismissEvent)))
            .occlude()
            .min_w(px(180.))
            .p_1()
            .flex()
            .flex_col()
            .bg(colors.surface)
            .text_color(colors.text)
            .text_size(px(13.))
            .border_1()
            .border_color(colors.border)
            .rounded(px(6.))
            .shadow_lg()
            .children(items)
    }
}
