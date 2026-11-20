//! The Settings window: General, Models, Providers, Permissions, Appearance and
//! Keybindings. Changes are saved to `~/.zp/settings.json` (keys to the system keychain)
//! and applied to the running app at once.

mod about;
mod keybindings;
mod mcp;
mod models;
pub(crate) mod providers;
mod subagents;

use gpui::{
    AnyElement, App, AppContext as _, ColorExt as _, Context, Entity, FocusHandle, Focusable,
    FontWeight, Global, IntoElement, Render, SharedString, Subscription, Task, Window,
    WindowBounds, WindowHandle, div, prelude::*, px, rgb_to_hsla, size,
};
use gpui_elements::editable_text::{EditableTextState, StringStorage, text_area, text_input};
use zp_ai::{CacheRetention, ThinkingLevel};
use zp_core::permissions::PermissionMode;
use zp_core::settings::{BlockDisplay, ThemeChoice, TranscriptSettings};

use crate::review_model_picker::{ReviewModelEvent, ReviewModelPicker};
use crate::store::{AppModels, AppSettings};
use crate::theme::ActiveTheme;
use crate::ui::{CUSTOM_TITLE_BAR, checkbox, title_bar};

#[cfg(test)]
pub use providers::EndpointTest;
pub use providers::ProviderDialog;

const SETTINGS_SIZE: (f32, f32) = (980.0, 700.0);
const SETTINGS_MIN_SIZE: (f32, f32) = (720.0, 480.0);
const NAV_WIDTH: f32 = 190.0;

/// A page of the settings window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    General,
    Models,
    Providers,
    Permissions,
    Subagents,
    Appearance,
    Keybindings,
    Mcp,
    About,
}

impl Page {
    pub const ALL: [Self; 9] = [
        Self::General,
        Self::Models,
        Self::Providers,
        Self::Mcp,
        Self::Permissions,
        Self::Subagents,
        Self::Appearance,
        Self::Keybindings,
        Self::About,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Models => "Models",
            Self::Providers => "Providers",
            Self::Permissions => "Permissions",
            Self::Subagents => "Subagents",
            Self::Appearance => "Appearance",
            Self::Keybindings => "Keybindings",
            Self::Mcp => "MCP Servers",
            Self::About => "About",
        }
    }
}

/// A kind of transcript block whose expansion is configurable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TranscriptBlock {
    Thinking,
    FileEdits,
    Commands,
    OtherTools,
}

impl TranscriptBlock {
    pub const ALL: [Self; 4] = [
        Self::Thinking,
        Self::FileEdits,
        Self::Commands,
        Self::OtherTools,
    ];

    fn id(self) -> &'static str {
        match self {
            Self::Thinking => "transcript-thinking",
            Self::FileEdits => "transcript-file-edits",
            Self::Commands => "transcript-commands",
            Self::OtherTools => "transcript-other-tools",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Thinking => "Thinking",
            Self::FileEdits => "File edits",
            Self::Commands => "Commands",
            Self::OtherTools => "Other tools",
        }
    }

    fn get(self, settings: &TranscriptSettings) -> BlockDisplay {
        match self {
            Self::Thinking => settings.thinking(),
            Self::FileEdits => settings.file_edits(),
            Self::Commands => settings.commands(),
            Self::OtherTools => settings.other_tools(),
        }
    }

    fn set(self, settings: &mut TranscriptSettings, display: BlockDisplay) {
        let slot = match self {
            Self::Thinking => &mut settings.thinking,
            Self::FileEdits => &mut settings.file_edits,
            Self::Commands => &mut settings.commands,
            Self::OtherTools => &mut settings.other_tools,
        };
        *slot = Some(display);
    }
}

/// The open settings window, if any.
#[derive(Default)]
struct OpenSettings(Option<WindowHandle<SettingsWindow>>);

impl Global for OpenSettings {}

pub struct SettingsWindow {
    page: Page,
    /// A failed change, warning or progress note: the text and whether it is an error.
    notice: Option<(String, bool)>,
    /// The version info was just copied (shows a check on the copy button for a moment).
    version_copied: Option<Task<()>>,
    // General
    model_menu_open: bool,
    model_filter: Entity<EditableTextState>,
    // Models
    model_search: Entity<EditableTextState>,
    // Providers
    dialog: Option<ProviderDialog>,
    dialog_subscription: Option<Subscription>,
    // Permissions
    review_model_picker: Option<Entity<ReviewModelPicker>>,
    review_model_subscription: Option<Subscription>,
    allow_commands: Entity<EditableTextState>,
    directories: Entity<EditableTextState>,
    subagent_form: subagents::SubagentForm,
    // Keybindings
    recording: Option<&'static str>,
    _interceptor: Option<Subscription>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

fn input(text: impl Into<String>, cx: &mut App) -> Entity<EditableTextState> {
    let text = text.into();
    cx.new(|cx| EditableTextState::new(StringStorage::from(text), cx))
}

impl SettingsWindow {
    /// Opens the settings window, or brings it to the front.
    pub fn open(cx: &mut App) {
        Self::open_at(Page::General, cx);
    }

    /// Opens the settings window on `page`.
    pub fn open_at(page: Page, cx: &mut App) -> Option<WindowHandle<Self>> {
        if let Some(handle) = cx.try_global::<OpenSettings>().and_then(|open| open.0) {
            let shown = handle
                .update(cx, |this, window, cx| {
                    this.set_page(page, cx);
                    window.activate();
                })
                .is_ok();
            if shown {
                return Some(handle);
            }
        }
        let options = crate::workspace::window_options(
            "Settings".to_owned(),
            None,
            SETTINGS_SIZE,
            SETTINGS_MIN_SIZE,
            cx,
        )
        .window_bounds(Some(WindowBounds::centered(
            size(px(SETTINGS_SIZE.0), px(SETTINGS_SIZE.1)),
            cx,
        )));
        match cx.open_window(options, |window, cx| {
            cx.new(|cx| Self::new(page, window, cx))
        }) {
            Ok(handle) => {
                cx.set_global(OpenSettings(Some(handle)));
                Some(handle)
            }
            Err(error) => {
                tracing::error!(?error, "could not open the settings window");
                None
            }
        }
    }

    /// A settings view on `page` (screenshots).
    #[cfg(feature = "visual-tests")]
    pub fn new_on(page: Page, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::new(page, window, cx)
    }

    fn new(page: Page, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let settings = AppSettings::get(cx).clone();
        let permissions = &settings.permissions;
        let model_filter = input("", cx);
        let model_search = input("", cx);
        let allow_commands = input(permissions.allow_commands.join("\n"), cx);
        let directories = input(permissions.additional_directories.join("\n"), cx);
        let subagent_form = subagents::SubagentForm::new(
            settings.subagents.as_ref().unwrap_or(&Default::default()),
            cx,
        );
        let subscriptions = vec![
            cx.subscribe(
                &model_filter,
                |_, _, _: &gpui_elements::editable_text::TextChanged, cx| cx.notify(),
            ),
            cx.subscribe(
                &model_search,
                |_, _, _: &gpui_elements::editable_text::TextChanged, cx| cx.notify(),
            ),
            cx.observe_window_appearance(window, |_, window, cx| {
                if crate::theme::Theme::sync_with_appearance(window.appearance(), cx) {
                    cx.refresh_windows();
                }
            }),
            cx.on_release(|_, cx| cx.set_global(OpenSettings(None))),
        ];
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);
        Self {
            page,
            notice: None,
            version_copied: None,
            model_menu_open: false,
            model_filter,
            model_search,
            dialog: None,
            dialog_subscription: None,
            review_model_picker: None,
            review_model_subscription: None,
            allow_commands,
            directories,
            subagent_form,
            recording: None,
            _interceptor: None,
            focus_handle,
            _subscriptions: subscriptions,
        }
    }

    #[cfg(test)]
    pub fn page(&self) -> Page {
        self.page
    }

    pub fn set_page(&mut self, page: Page, cx: &mut Context<Self>) {
        if self.page != page {
            self.page = page;
            self.notice = None;
            self.dialog = None;
            self.dialog_subscription = None;
            self.review_model_picker = None;
            self.review_model_subscription = None;
            self.stop_recording();
        }
        cx.notify();
    }

    /// The feedback line (tests).
    #[cfg(test)]
    pub fn notice(&self) -> Option<&(String, bool)> {
        self.notice.as_ref()
    }

    /// Shows a failed change above the page. A change that worked shows itself in the
    /// controls, so it only clears an earlier error.
    fn report(&mut self, result: anyhow::Result<()>, cx: &mut Context<Self>) {
        self.notice = result.err().map(|error| (format!("{error:#}"), true));
        cx.notify();
    }

    // ----- General -------------------------------------------------------------------

    pub fn set_default_model(&mut self, spec: Option<String>, cx: &mut Context<Self>) {
        self.model_menu_open = false;
        let result = AppSettings::update(cx, |settings| settings.default_model = spec);
        self.report(result, cx);
    }

    pub fn set_default_thinking(&mut self, level: ThinkingLevel, cx: &mut Context<Self>) {
        let result = AppSettings::update(cx, |s| s.default_thinking = Some(level));
        self.report(result, cx);
    }

    pub fn set_notifications(&mut self, on: bool, cx: &mut Context<Self>) {
        let result = AppSettings::update(cx, |s| s.notifications = Some(on));
        self.report(result, cx);
    }

    pub fn set_generate_titles(&mut self, on: bool, cx: &mut Context<Self>) {
        let result = AppSettings::update(cx, |s| s.generate_titles = Some(on));
        self.report(result, cx);
    }

    // ----- Permissions -----------------------------------------------------------------

    pub fn set_permission_mode(
        &mut self,
        mode: PermissionMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if mode == PermissionMode::AutoReview
            && !AppSettings::get(cx)
                .permissions
                .review_model
                .as_deref()
                .is_some_and(|spec| {
                    let models = &cx.global::<AppModels>().models;
                    models
                        .find(spec)
                        .is_some_and(|model| models.resolve_auth(&model.provider).is_some())
                })
        {
            self.open_review_model_picker(true, window, cx);
            return;
        }
        let result = AppSettings::update(cx, |s| s.permissions.mode = Some(mode));
        self.report(result, cx);
    }

    pub(crate) fn open_review_model_picker(
        &mut self,
        enable_review: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = AppSettings::get(cx).permissions.review_model.clone();
        let picker = cx.new(|cx| ReviewModelPicker::new(current, window, cx));
        self.review_model_subscription = Some(cx.subscribe_in(
            &picker,
            window,
            move |this, _, event: &ReviewModelEvent, window, cx| {
                if let ReviewModelEvent::Selected(spec) = event {
                    this.set_review_model(spec, enable_review, cx);
                }
                this.review_model_picker = None;
                this.review_model_subscription = None;
                this.focus_handle.focus(window, cx);
                cx.notify();
            },
        ));
        self.review_model_picker = Some(picker);
        cx.notify();
    }

    pub(crate) fn set_review_model(
        &mut self,
        spec: &str,
        enable_review: bool,
        cx: &mut Context<Self>,
    ) {
        let models = &cx.global::<AppModels>().models;
        let result = if !models
            .find(spec)
            .is_some_and(|model| models.resolve_auth(&model.provider).is_some())
        {
            Err(anyhow::anyhow!("unavailable review model `{spec}`"))
        } else {
            AppSettings::update(cx, |s| {
                s.permissions.review_model = Some(spec.to_owned());
                if enable_review {
                    s.permissions.mode = Some(PermissionMode::AutoReview);
                }
            })
        };
        self.report(result, cx);
    }

    pub fn set_review_thinking(&mut self, level: ThinkingLevel, cx: &mut Context<Self>) {
        let result = AppSettings::update(cx, |s| s.permissions.review_thinking = Some(level));
        self.report(result, cx);
    }

    #[cfg(any(test, feature = "visual-tests"))]
    pub(crate) fn review_model_picker(&self) -> Option<Entity<ReviewModelPicker>> {
        self.review_model_picker.clone()
    }

    /// Saves the allowed commands and directories.
    pub fn save_permission_lists(&mut self, cx: &mut Context<Self>) {
        let lines = |input: &Entity<EditableTextState>, cx: &App| -> Vec<String> {
            input
                .read(cx)
                .as_str()
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .collect()
        };
        let commands = lines(&self.allow_commands, cx);
        let directories = lines(&self.directories, cx);
        let result = AppSettings::update(cx, |s| {
            s.permissions.allow_commands = commands;
            s.permissions.additional_directories = directories;
        });
        self.report(result, cx);
    }

    /// The permission text inputs (tests).
    #[cfg(test)]
    pub fn permission_inputs(&self) -> [Entity<EditableTextState>; 2] {
        [self.allow_commands.clone(), self.directories.clone()]
    }

    // ----- Appearance ------------------------------------------------------------------

    pub fn set_theme(&mut self, theme: ThemeChoice, cx: &mut Context<Self>) {
        let result = AppSettings::update(cx, |s| s.theme = Some(theme));
        self.report(result, cx);
    }

    /// Sets whether `block`s start expanded and collapse when done.
    pub fn set_transcript_display(
        &mut self,
        block: TranscriptBlock,
        display: BlockDisplay,
        cx: &mut Context<Self>,
    ) {
        let result = AppSettings::update(cx, |s| block.set(&mut s.transcript, display));
        self.report(result, cx);
    }

    // ----- rendering -------------------------------------------------------------------

    fn render_nav(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors.clone();
        div()
            .w(px(NAV_WIDTH))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap_0p5()
            .p_2()
            .bg(colors.surface)
            .border_r_1()
            .border_color(colors.border)
            .children(Page::ALL.into_iter().map(|page| {
                let active = page == self.page;
                div()
                    .id(page.label())
                    .px_3()
                    .py_1p5()
                    .rounded(px(6.))
                    .cursor_pointer()
                    .when(active, |this| {
                        this.bg(colors.surface_hover)
                            .font_weight(FontWeight::SEMIBOLD)
                    })
                    .when(!active, |this| this.text_color(colors.text_muted))
                    .hover_style(|s| s.bg(colors.surface_hover).text_color(colors.text))
                    .child(page.label())
                    .on_click(cx.listener(move |this, _, _, cx| this.set_page(page, cx)))
            }))
    }

    fn render_general(&self, cx: &mut Context<Self>) -> AnyElement {
        let settings = AppSettings::get(cx).clone();
        let colors = cx.theme().colors.clone();
        let current = settings.default_model.clone();
        let model_button = button(
            "default-model",
            format!(
                "{} ▾",
                current
                    .clone()
                    .unwrap_or_else(|| "Last used model".to_owned())
            ),
            ButtonKind::Normal,
            cx,
        )
        .on_click(cx.listener(|this, _, window, cx| {
            this.model_menu_open = !this.model_menu_open;
            if this.model_menu_open {
                this.model_filter
                    .update(cx, |filter, cx| filter.emplace("", cx));
                this.model_filter
                    .read(cx)
                    .focus_handle(cx)
                    .focus(window, cx);
            }
            cx.notify();
        }));
        let menu = self.model_menu_open.then(|| {
            let filter = self.model_filter.read(cx).as_str().trim().to_lowercase();
            let models = &cx.global::<AppModels>().models;
            let choices: Vec<String> = models
                .available_providers()
                .iter()
                .flat_map(|p| models.models(Some(p.id())))
                .map(|m| m.qualified_id())
                .filter(|id| {
                    filter
                        .split_whitespace()
                        .all(|word| id.to_lowercase().contains(word))
                })
                .take(200)
                .collect();
            div()
                .mt_1()
                .w(px(420.))
                .flex()
                .flex_col()
                .p_1()
                .rounded(px(6.))
                .border_1()
                .border_color(colors.border)
                .bg(colors.surface)
                .child(text_field(&self.model_filter, "Filter models…", cx))
                .child(
                    div()
                        .id("default-model-list")
                        .max_h(px(260.))
                        .overflow_y_scroll()
                        .child(
                            menu_item("model-auto", "Last used model", current.is_none(), cx)
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.set_default_model(None, cx)),
                                ),
                        )
                        .children(choices.into_iter().enumerate().map(|(index, id)| {
                            let selected = current.as_deref() == Some(id.as_str());
                            let spec = id.clone();
                            menu_item(("model", index), id, selected, cx).on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.set_default_model(Some(spec.clone()), cx)
                                },
                            ))
                        })),
                )
        });
        let thinking = settings.default_thinking.unwrap_or(ThinkingLevel::High);
        let levels = [
            ThinkingLevel::Off,
            ThinkingLevel::Minimal,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
            ThinkingLevel::XHigh,
        ];
        let cache_retention = settings.cache_retention.unwrap_or_default();
        let cache_choices = [
            ("Default", CacheRetention::Short),
            ("Extended", CacheRetention::Long),
            ("Off", CacheRetention::None),
        ];
        let titles = settings.generate_titles.unwrap_or(true);
        let notifications = settings.notifications.unwrap_or(true);
        page_body()
            .child(section(
                "Default model",
                "The model new conversations start with. Each conversation can switch models.",
                cx,
            ))
            .child(div().flex().flex_col().items_start().child(model_button).children(menu))
            .child(section(
                "Default thinking level",
                "How much new conversations reason; models without that level use the closest one.",
                cx,
            ))
            .child(div().flex().child(segmented(
                "thinking",
                levels.iter().map(|level| (level.as_str().to_owned(), *level == thinking)),
                cx,
                move |this, index, cx| this.set_default_thinking(levels[index], cx),
            )))
            .child(section(
                "Prompt cache retention",
                "How long providers keep the reused start of a conversation. Extended asks for 1 hour on Anthropic (2× the input price per cache write), 24 hours on earlier OpenAI models and 12 hours on GPT-5.6 and later (2.5× per write); other endpoints keep their default unless set up for it. Off stops cache writes, so every request pays the full input price: usually more expensive.",
                cx,
            ))
            .child(div().flex().child(segmented(
                "cache-retention",
                cache_choices.iter().map(|(label, value)| (label.to_string(), *value == cache_retention)),
                cx,
                move |this, index, cx| {
                    let result = AppSettings::update(cx, |settings| {
                        settings.cache_retention = Some(cache_choices[index].1);
                    });
                    this.report(result, cx);
                },
            )))
            .child(section(
                "Conversation titles",
                "Ask a small, cheap model of the same provider for a short title after the first answer.",
                cx,
            ))
            .child(div().flex().child(
                checkbox("generate-titles", "Name conversations with a model", titles, cx)
                    .on_click(cx.listener(move |this, _, _, cx| this.set_generate_titles(!titles, cx))),
            ))
            .child(section(
                "Notifications",
                "Tell me when a conversation I'm not looking at finishes or needs an approval.",
                cx,
            ))
            .child(div().flex().child(
                checkbox("notifications", "Show system notifications", notifications, cx)
                    .on_click(cx.listener(move |this, _, _, cx| this.set_notifications(!notifications, cx))),
            ))
            .into_any_element()
    }

    fn render_permissions(&self, cx: &mut Context<Self>) -> AnyElement {
        let current = AppSettings::get(cx).permissions.mode.unwrap_or_default();
        let review_thinking = AppSettings::get(cx)
            .permissions
            .review_thinking
            .unwrap_or_default();
        let levels = [
            ThinkingLevel::Off,
            ThinkingLevel::Minimal,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
        ];
        page_body()
            .child(section(
                "Default permission mode",
                "Used by projects that do not choose their own mode (in the conversation's mode menu or `.zp/settings.json`).",
                cx,
            ))
            .children(PermissionMode::ALL.into_iter().enumerate().map(|(index, mode)| {
                radio(("permission-mode", index), mode.label(), mode.description(), mode == current, cx)
                    .on_click(cx.listener(move |this, _, window, cx| this.set_permission_mode(mode, window, cx)))
            }))
            .child(section(
                "Review model",
                "Model used to review tool calls.",
                cx,
            ))
            .child(div().flex().child(
                button(
                    "review-model-button",
                    format!("{} \u{25be}", AppSettings::get(cx).permissions.review_model.as_deref().map(crate::ui::model_display_name).unwrap_or("Choose a model")),
                    ButtonKind::Normal,
                    cx,
                )
                .max_w_full()
                .truncate()
                .on_click(cx.listener(|this, _, window, cx| this.open_review_model_picker(false, window, cx))),
            ))
            .child(section(
                "Review thinking level",
                "How much the review model reasons before deciding. Higher is more careful but slower and costlier; models without that level use the closest one.",
                cx,
            ))
            .child(div().flex().child(segmented(
                "review-thinking",
                levels.iter().map(|level| (level.as_str().to_owned(), *level == review_thinking)),
                cx,
                move |this, index, cx| this.set_review_thinking(levels[index], cx),
            )))
            .child(section(
                "Allowed commands",
                "Command prefixes that run without asking, one per line (e.g. `cargo test`, `git status`). Destructive commands always ask.",
                cx,
            ))
            .child(text_block(&self.allow_commands, "One prefix per line, e.g. cargo test", cx))
            .child(section(
                "Additional directories",
                "Directories outside a project that tools may read and write without asking, one per line.",
                cx,
            ))
            .child(text_block(&self.directories, "One directory per line, e.g. ~/notes", cx))
            .child(
                div().pt_2().flex().child(
                    button("save-permissions", "Save", ButtonKind::Primary, cx).on_click(
                        cx.listener(|this, _, _, cx| this.save_permission_lists(cx)),
                    ),
                ),
            )
            .into_any_element()
    }

    fn render_appearance(&self, cx: &mut Context<Self>) -> AnyElement {
        let current = AppSettings::get(cx).theme.unwrap_or_default();
        let transcript = AppSettings::transcript(cx);
        let themes = [
            (
                ThemeChoice::System,
                "System",
                "Follow the system's light or dark appearance",
            ),
            (ThemeChoice::Dark, "Dark", "Always dark"),
            (ThemeChoice::Light, "Light", "Always light"),
        ];
        page_body()
            .child(section("Theme", "", cx))
            .children(
                themes
                    .into_iter()
                    .enumerate()
                    .map(|(index, (theme, label, description))| {
                        radio(("theme", index), label, description, theme == current, cx)
                            .on_click(cx.listener(move |this, _, _, cx| this.set_theme(theme, cx)))
                    }),
            )
            .child(section(
                "Conversation details",
                "Default visibility of thinking and tool details. Failed tools start expanded.",
                cx,
            ))
            .children(TranscriptBlock::ALL.into_iter().map(|block| {
                let current = block.get(&transcript);
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(div().w(px(110.)).flex_shrink_0().child(block.label()))
                    .child(segmented(
                        block.id(),
                        BlockDisplay::ALL
                            .iter()
                            .map(|display| (display.label().to_owned(), *display == current)),
                        cx,
                        move |this, index, cx| {
                            this.set_transcript_display(block, BlockDisplay::ALL[index], cx)
                        },
                    ))
            }))
            .into_any_element()
    }
}

impl Focusable for SettingsWindow {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SettingsWindow {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let colors = theme.colors.clone();
        let font_size = theme.metrics.font_size;
        let body = match self.page {
            Page::General => self.render_general(cx),
            Page::Models => self.render_models(cx),
            Page::Providers => self.render_providers(cx),
            Page::Permissions => self.render_permissions(cx),
            Page::Subagents => self.render_subagents(cx),
            Page::Appearance => self.render_appearance(cx),
            Page::Keybindings => self.render_keybindings(cx),
            Page::Mcp => self.render_mcp(cx),
            Page::About => self.render_about(cx),
        };
        let notice = self.notice.clone().filter(|(text, _)| !text.is_empty());
        div()
            .id("settings")
            .key_context(if self.recording.is_some() {
                "Keybindings"
            } else {
                "Settings"
            })
            .track_focus(&self.focus_handle)
            .on_action(
                cx.listener(|this, _: &crate::actions::CancelRecording, _, cx| {
                    this.stop_recording();
                    cx.notify();
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::CloseCompletion, _, cx| {
                    this.close_dialog(cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::actions::AcceptCompletion, _, cx| this.save_key(cx)),
            )
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(colors.background)
            .text_color(colors.text)
            .text_size(font_size)
            .when(CUSTOM_TITLE_BAR, |this| {
                this.child(
                    title_bar("settings-title")
                        .bg(colors.surface)
                        .border_b_1()
                        .border_color(colors.border)
                        .child(
                            div()
                                .flex_1()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child("Settings"),
                        )
                        .children(crate::updates::title_bar_button(false, cx)),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(self.render_nav(cx))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .px_6()
                                    .pt_4()
                                    .pb_2()
                                    .text_size(px(18.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(self.page.label()),
                            )
                            .when_some(notice, |this, (text, error)| {
                                this.child(
                                    div()
                                        .mx_6()
                                        .mb_2()
                                        .px_3()
                                        .py_1p5()
                                        .rounded(px(6.))
                                        .text_size(px(12.))
                                        .bg(colors.surface)
                                        .text_color(if error {
                                            colors.error
                                        } else {
                                            colors.text_muted
                                        })
                                        .child(text),
                                )
                            })
                            .child(
                                div()
                                    .id("settings-page")
                                    .flex_1()
                                    .min_h_0()
                                    .overflow_y_scroll()
                                    .child(body),
                            ),
                    ),
            )
            .children(self.render_provider_dialog(cx))
            .children(self.review_model_picker.clone())
    }
}

// ----- shared controls -------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ButtonKind {
    Normal,
    Primary,
    Danger,
}

fn page_body() -> gpui::Div {
    div().w_full().flex().flex_col().gap_2().px_6().pb_6()
}

pub(super) fn section(title: &str, description: &str, cx: &App) -> impl IntoElement {
    let colors = &cx.theme().colors;
    div()
        .pt_3()
        .flex()
        .flex_col()
        .gap_0p5()
        .child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .child(title.to_owned()),
        )
        .when(!description.is_empty(), |this| {
            this.child(
                div()
                    .text_size(px(12.))
                    .text_color(colors.text_muted)
                    .child(description.to_owned()),
            )
        })
}

pub(super) fn button(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
    kind: ButtonKind,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    let colors = cx.theme().colors.clone();
    let base = div()
        .id(id)
        .flex_shrink_0()
        .px_3()
        .py_1()
        .rounded(px(5.))
        .cursor_pointer()
        .text_size(px(12.))
        .border_1()
        .child(label.into());
    match kind {
        ButtonKind::Primary => base
            .bg(colors.accent)
            .border_color(colors.accent)
            .text_color(colors.accent_text),
        ButtonKind::Danger => base
            .border_color(colors.border)
            .text_color(colors.error)
            .hover_style(move |s| s.bg(colors.surface_hover)),
        ButtonKind::Normal => base
            .border_color(colors.border)
            .bg(colors.surface)
            .hover_style(move |s| s.bg(colors.surface_hover)),
    }
}

fn menu_item(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
    selected: bool,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    let colors = cx.theme().colors.clone();
    div()
        .id(id)
        .flex()
        .gap_2()
        .px_2()
        .py_1()
        .rounded(px(4.))
        .cursor_pointer()
        .text_size(px(12.))
        .hover_style(move |s| s.bg(colors.surface_hover))
        .child(
            div()
                .w(px(12.))
                .flex_shrink_0()
                .child(if selected { "✓" } else { "" }),
        )
        .child(label.into())
}

fn radio(
    id: impl Into<gpui::ElementId>,
    label: &str,
    description: &str,
    selected: bool,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    let colors = cx.theme().colors.clone();
    div()
        .id(id)
        .flex()
        .items_start()
        .gap_2()
        .px_3()
        .py_2()
        .rounded(px(6.))
        .border_1()
        .border_color(if selected {
            colors.accent
        } else {
            colors.border
        })
        .cursor_pointer()
        .hover_style(move |s| s.bg(colors.surface_hover))
        .child(
            div()
                .mt(px(3.))
                .size(px(12.))
                .flex_shrink_0()
                .rounded_full()
                .border_1()
                .border_color(if selected {
                    colors.accent
                } else {
                    colors.text_muted
                })
                .when(selected, |this| this.bg(colors.accent)),
        )
        .child(
            div().flex().flex_col().child(label.to_owned()).child(
                div()
                    .text_size(px(12.))
                    .text_color(colors.text_muted)
                    .child(description.to_owned()),
            ),
        )
}

fn segmented(
    id: &'static str,
    options: impl Iterator<Item = (String, bool)>,
    cx: &mut Context<SettingsWindow>,
    on_select: impl Fn(&mut SettingsWindow, usize, &mut Context<SettingsWindow>) + Clone + 'static,
) -> impl IntoElement {
    let colors = cx.theme().colors.clone();
    div()
        .flex()
        .p_0p5()
        .gap_0p5()
        .rounded(px(6.))
        .bg(colors.surface)
        .border_1()
        .border_color(colors.border)
        .w_auto()
        .children(options.enumerate().map(|(index, (label, selected))| {
            let on_select = on_select.clone();
            div()
                .id((id, index))
                .px_3()
                .py_1()
                .rounded(px(4.))
                .cursor_pointer()
                .text_size(px(12.))
                .when(selected, |this| {
                    this.bg(colors.accent).text_color(colors.accent_text)
                })
                .when(!selected, |this| {
                    this.hover_style(|s| s.bg(colors.surface_hover))
                })
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| on_select(this, index, cx)))
        }))
}

pub(super) fn text_field(
    state: &Entity<EditableTextState>,
    placeholder: &'static str,
    cx: &App,
) -> impl IntoElement {
    let colors = &cx.theme().colors;
    text_input(("field", state.entity_id()))
        .state(state.downgrade())
        .placeholder(placeholder)
        .placeholder_color(rgb_to_hsla(colors.text_muted))
        .caret_color(rgb_to_hsla(colors.text))
        .selection_color(rgb_to_hsla(colors.accent).opacity(0.35))
        .w_full()
        .px_2()
        .py_1()
        .rounded(px(5.))
        .border_1()
        .border_color(colors.border)
        .bg(colors.surface)
        .text_size(px(13.))
        .whitespace_nowrap()
}

/// A single-line field showing dots instead of its text (API keys). The text is drawn
/// invisibly in a monospace font with the dots over it, so the caret still lines up.
pub(super) fn secret_field(
    state: &Entity<EditableTextState>,
    placeholder: &'static str,
    cx: &App,
) -> impl IntoElement {
    let colors = &cx.theme().colors;
    let mono = cx.theme().metrics.mono_font.clone();
    let length = state.read(cx).as_str().chars().count();
    div()
        .relative()
        .w_full()
        .child(
            text_input(("secret", state.entity_id()))
                .state(state.downgrade())
                .placeholder(placeholder)
                .placeholder_color(rgb_to_hsla(colors.text_muted))
                .caret_color(rgb_to_hsla(colors.text))
                .selection_color(rgb_to_hsla(colors.accent).opacity(0.35))
                .text_color(gpui::transparent_black())
                .font_family(mono.clone())
                .w_full()
                .px_2()
                .py_1()
                .rounded(px(5.))
                .border_1()
                .border_color(colors.border)
                .bg(colors.surface)
                .text_size(px(13.))
                .whitespace_nowrap()
                .overflow_hidden(),
        )
        .child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .bottom_0()
                .px_2()
                .py_1()
                .border_1()
                .border_color(gpui::transparent_black())
                .overflow_hidden()
                .whitespace_nowrap()
                .font_family(mono)
                .text_size(px(13.))
                .text_color(colors.text)
                .child("•".repeat(length)),
        )
}

fn text_block(
    state: &Entity<EditableTextState>,
    placeholder: &'static str,
    cx: &App,
) -> impl IntoElement {
    let colors = &cx.theme().colors;
    text_area(("block", state.entity_id()))
        .state(state.downgrade())
        .placeholder(placeholder)
        .placeholder_color(rgb_to_hsla(colors.text_muted))
        .caret_color(rgb_to_hsla(colors.text))
        .selection_color(rgb_to_hsla(colors.accent).opacity(0.35))
        .w_full()
        .min_h(px(72.))
        .max_h(px(180.))
        .overflow_y_scroll()
        .px_2()
        .py_1()
        .rounded(px(5.))
        .border_1()
        .border_color(colors.border)
        .bg(colors.surface)
        .font_family(cx.theme().metrics.mono_font.clone())
        .text_size(px(12.))
}

/// `ms` as "just now", "5 min ago", "3 h ago" or a date.
pub(super) fn ago(ms: i64) -> String {
    crate::ui::relative_time(ms, zp_ai::now_ms())
}
