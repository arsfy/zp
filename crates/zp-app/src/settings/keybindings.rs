//! The Keybindings page: every customizable shortcut, recorded by pressing the keys.

use gpui::{AnyElement, Context, IntoElement, Keystroke, Window, div, prelude::*, px};

use super::{ButtonKind, SettingsWindow, button, page_body, section};
use crate::actions::{shortcut_keys, shortcuts};
use crate::store::AppSettings;
use crate::theme::ActiveTheme;
use crate::ui::shortcut_label;

/// Keys that only modify others.
const MODIFIER_KEYS: [&str; 5] = ["shift", "control", "alt", "platform", "function"];

impl SettingsWindow {
    /// Waits for the next keystroke and makes it the shortcut of `action`.
    pub fn start_recording(
        &mut self,
        action: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.recording = Some(action);
        self.focus_handle.focus(window, cx);
        let this = cx.entity().downgrade();
        // Interceptors run before key bindings, so the shortcut being recorded does not
        // trigger its current action.
        self._interceptor = Some(cx.intercept_keystrokes(move |event, _, cx| {
            let keystroke = event.keystroke.clone();
            let handled = this
                .update(cx, |this, cx| this.record(&keystroke, cx))
                .unwrap_or(false);
            if handled {
                cx.stop_propagation();
            }
        }));
        cx.notify();
    }

    /// Takes a recorded keystroke; returns whether it was consumed.
    pub fn record(&mut self, keystroke: &Keystroke, cx: &mut Context<Self>) -> bool {
        let Some(action) = self.recording else {
            return false;
        };
        if MODIFIER_KEYS.contains(&keystroke.key.as_str()) {
            return true;
        }
        if keystroke.key == "escape" && !keystroke.modifiers.modified() {
            self.stop_recording();
            cx.notify();
            return true;
        }
        let keys = keystroke.unparse();
        self.stop_recording();
        self.set_shortcut(action, Some(keys), cx);
        true
    }

    pub(super) fn stop_recording(&mut self) {
        self.recording = None;
        self._interceptor = None;
    }

    #[cfg(test)]
    pub fn is_recording(&self) -> bool {
        self.recording.is_some()
    }

    /// Sets (`Some`), clears (`Some("")`) or resets (`None`) the shortcut of `action`.
    pub fn set_shortcut(&mut self, action: &str, keys: Option<String>, cx: &mut Context<Self>) {
        let label = shortcuts()
            .into_iter()
            .find(|s| s.action == action)
            .map_or(action, |s| s.label);
        let taken = keys.as_deref().filter(|k| !k.is_empty()).and_then(|keys| {
            shortcuts()
                .into_iter()
                .find(|other| {
                    other.action != action
                        && shortcut_keys(other.action, cx).as_deref() == Some(keys)
                })
                .map(|other| other.label)
        });
        let message = match (&keys, taken) {
            (Some(keys), _) if keys.is_empty() => format!("Removed the shortcut of {label}"),
            (Some(keys), Some(other)) => {
                format!("{label}: {} (also used by {other})", shortcut_label(keys))
            }
            (Some(keys), None) => format!("{label}: {}", shortcut_label(keys)),
            (None, _) => format!("Reset the shortcut of {label}"),
        };
        let action = action.to_owned();
        let result = AppSettings::update(cx, |settings| match keys {
            Some(keys) => {
                settings.keybindings.insert(action, keys);
            }
            None => {
                settings.keybindings.remove(&action);
            }
        })
        .map(|()| message);
        self.report(result, cx);
    }

    pub(super) fn render_keybindings(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let overrides = AppSettings::get(cx).keybindings.clone();
        let mut body = page_body().child(section(
            "Shortcuts",
            "Click Change and press the new keys. Esc cancels.",
            cx,
        ));
        for (index, shortcut) in shortcuts().into_iter().enumerate() {
            let action = shortcut.action;
            let recording = self.recording == Some(action);
            let keys = shortcut_keys(action, cx);
            let changed = overrides.contains_key(action);
            body = body.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .py_1()
                    .border_b_1()
                    .border_color(colors.border)
                    .child(div().flex_1().child(shortcut.label))
                    .child(
                        div()
                            .min_w(px(120.))
                            .px_2()
                            .py_0p5()
                            .rounded(px(4.))
                            .text_size(px(12.))
                            .when(recording, |this| {
                                this.border_1()
                                    .border_color(colors.accent)
                                    .text_color(colors.accent)
                                    .child("Press keys…")
                            })
                            .when(!recording, |this| {
                                this.bg(colors.surface).child(match &keys {
                                    Some(keys) => shortcut_label(keys),
                                    None => "—".to_owned(),
                                })
                            }),
                    )
                    .child(
                        button(("change-shortcut", index), "Change", ButtonKind::Normal, cx)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.start_recording(action, window, cx)
                            })),
                    )
                    .child(
                        button(("remove-shortcut", index), "Remove", ButtonKind::Normal, cx)
                            .when(keys.is_none(), |this| this.invisible())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.set_shortcut(action, Some(String::new()), cx)
                            })),
                    )
                    .child(
                        button(("reset-shortcut", index), "Reset", ButtonKind::Normal, cx)
                            .when(!changed, |this| this.invisible())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.set_shortcut(action, None, cx)
                            })),
                    ),
            );
        }
        body.into_any_element()
    }
}
