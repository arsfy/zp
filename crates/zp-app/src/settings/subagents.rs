use gpui::{
    AnyElement, Context, Entity, Focusable as _, FontWeight, Subscription, Window, div, prelude::*,
    px,
};
use gpui_elements::editable_text::{EditableTextState, TextChanged};
use zp_ai::ThinkingLevel;
use zp_core::subagents::{
    Approval, Assignment, MAX_REQUESTS_PER_TASK, MAX_TIMEOUT_SECONDS, Profile, SubagentSettings,
    configured_model, model_endpoint, valid_profile_id,
};

use super::{
    ButtonKind, SettingsWindow, button, input, page_body, radio, section, segmented, text_field,
};
use crate::review_model_picker::{ReviewModelEvent, ReviewModelPicker};
use crate::store::{AppModels, AppSettings};
use crate::theme::ActiveTheme;

/// A task limit field.
struct Limit {
    id: &'static str,
    label: &'static str,
    max: u64,
    /// What an empty field means; `None` when the limit is required.
    unset: Option<&'static str>,
}

const LIMITS: [Limit; 5] = [
    Limit {
        id: "subagent-concurrency",
        label: "Parallel tasks",
        max: 16,
        unset: None,
    },
    Limit {
        id: "subagent-per-run",
        label: "Tasks per run",
        max: 64,
        unset: None,
    },
    Limit {
        id: "subagent-requests",
        label: "Model requests per task",
        max: MAX_REQUESTS_PER_TASK as u64,
        unset: Some("No limit"),
    },
    Limit {
        id: "subagent-timeout",
        label: "Timeout (seconds)",
        max: MAX_TIMEOUT_SECONDS,
        unset: Some("No limit"),
    },
    Limit {
        id: "subagent-output-tokens",
        label: "Output tokens per request",
        max: 1_000_000,
        unset: Some("Model's limit"),
    },
];

pub(super) struct SubagentForm {
    id: Entity<EditableTextState>,
    purpose: Entity<EditableTextState>,
    /// One field per [`LIMITS`] entry.
    limits: [Entity<EditableTextState>; 5],
    _subscriptions: Vec<Subscription>,
}

impl SubagentForm {
    pub(super) fn new(settings: &SubagentSettings, cx: &mut Context<SettingsWindow>) -> Self {
        let id = input("", cx);
        let purpose = input("", cx);
        let limits = [
            Some(settings.max_concurrent() as u64),
            Some(settings.max_per_run() as u64),
            settings.max_model_requests_per_task().map(|n| n as u64),
            settings.timeout_seconds(),
            settings.max_output_tokens(),
        ]
        .map(|limit| input(limit.map(|n| n.to_string()).unwrap_or_default(), cx));
        // The buttons depend on whether the ID names an existing profile.
        let mut subscriptions = vec![cx.subscribe(&id, |_, _, _: &TextChanged, cx| cx.notify())];
        // Limits apply as they are typed.
        for (index, field) in limits.iter().enumerate() {
            subscriptions.push(cx.subscribe(field, move |this, _, _: &TextChanged, cx| {
                this.save_subagent_limit(index, cx)
            }));
        }
        Self {
            id,
            purpose,
            limits,
            _subscriptions: subscriptions,
        }
    }
}

impl SettingsWindow {
    /// The profile ID, purpose, limit and output token fields (tests).
    #[cfg(test)]
    pub(crate) fn subagent_fields(&self) -> Vec<Entity<EditableTextState>> {
        let form = &self.subagent_form;
        [&form.id, &form.purpose]
            .into_iter()
            .chain(&form.limits)
            .cloned()
            .collect()
    }

    fn update_subagents(
        &mut self,
        cx: &mut Context<Self>,
        edit: impl FnOnce(&mut SubagentSettings),
    ) {
        let result = AppSettings::update(cx, |settings| {
            edit(
                settings
                    .subagents
                    .get_or_insert_with(SubagentSettings::default),
            )
        });
        self.report(result, cx);
    }

    pub(crate) fn set_subagent_assignment(
        &mut self,
        assignment: Assignment,
        cx: &mut Context<Self>,
    ) {
        self.update_subagents(cx, |settings| settings.assignment = Some(assignment));
    }

    /// Saves limit `index` of [`LIMITS`] as typed. An emptied required limit is left as
    /// it was (it is probably being retyped); an emptied optional one is removed.
    fn save_subagent_limit(&mut self, index: usize, cx: &mut Context<Self>) {
        let (Some(field), Some(limit)) = (self.subagent_form.limits.get(index), LIMITS.get(index))
        else {
            return;
        };
        let text = field.read(cx).as_str().trim().to_owned();
        let value = match text.parse::<u64>() {
            _ if text.is_empty() && limit.unset.is_some() => None,
            _ if text.is_empty() => {
                self.report(Ok(()), cx);
                return;
            }
            Ok(value) if (1..=limit.max).contains(&value) => Some(value),
            _ => {
                let empty = limit
                    .unset
                    .map(|unset| format!(", or empty for {}", unset.to_lowercase()))
                    .unwrap_or_default();
                self.report(
                    Err(anyhow::anyhow!(
                        "{} must be between 1 and {}{empty}",
                        limit.label,
                        limit.max
                    )),
                    cx,
                );
                return;
            }
        };
        let count =
            |value: Option<u64>| value.map(|value| usize::try_from(value).unwrap_or(usize::MAX));
        self.update_subagents(cx, |settings| match index {
            0 => settings.max_concurrent = count(value),
            1 => settings.max_per_run = count(value),
            2 => settings.max_model_requests_per_task = count(value),
            3 => settings.timeout_seconds = value,
            _ => settings.max_output_tokens = value,
        });
    }

    /// Checks the profile form; `model` is the model to save it with, `None` to choose
    /// one in the model picker.
    fn submit_subagent_profile(
        &mut self,
        model: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = self.subagent_form.id.read(cx).as_str().trim().to_owned();
        let purpose = self
            .subagent_form
            .purpose
            .read(cx)
            .as_str()
            .trim()
            .to_owned();
        if !valid_profile_id(&id) {
            self.report(
                Err(anyhow::anyhow!(
                    "Use a profile ID of 1–32 lowercase letters, digits, hyphens or underscores"
                )),
                cx,
            );
            return;
        }
        if purpose.len() > 300 {
            self.report(
                Err(anyhow::anyhow!("Keep the profile purpose under 300 bytes")),
                cx,
            );
            return;
        }
        match model {
            Some(model) => self.save_subagent_profile(id, purpose, model, cx),
            None => self.pick_subagent_model(id, purpose, window, cx),
        }
    }

    fn save_subagent_profile(
        &mut self,
        id: String,
        description: String,
        model: String,
        cx: &mut Context<Self>,
    ) {
        let unchanged = AppSettings::get(cx)
            .subagents
            .as_ref()
            .and_then(|settings| settings.profiles.get(&id))
            .is_some_and(|profile| profile.model == model);
        // A model kept from before may be unavailable for now; a new choice must work.
        if model != "inherit"
            && !unchanged
            && configured_model(&cx.global::<AppModels>().models, &model).is_none()
        {
            self.report(
                Err(anyhow::anyhow!("That model is no longer available")),
                cx,
            );
            return;
        }
        self.update_subagents(cx, |settings| {
            let thinking = settings
                .profiles
                .get(&id)
                .map_or(ThinkingLevel::Low, |profile| profile.thinking);
            if settings.default_profile.is_none() {
                settings.default_profile = Some(id.clone());
            }
            settings.profiles.insert(
                id,
                Profile {
                    model,
                    description,
                    thinking,
                },
            );
        });
        if self.notice.is_none() {
            self.clear_subagent_form(cx);
        }
    }

    fn clear_subagent_form(&mut self, cx: &mut Context<Self>) {
        for field in [&self.subagent_form.id, &self.subagent_form.purpose] {
            field.update(cx, |field, cx| field.emplace("", cx));
        }
        cx.notify();
    }

    fn pick_subagent_model(
        &mut self,
        id: String,
        purpose: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = AppSettings::get(cx)
            .subagents
            .as_ref()
            .and_then(|settings| settings.profiles.get(&id))
            .map(|profile| profile.model.clone());
        let picker =
            cx.new(|cx| ReviewModelPicker::with_title("Subagent model", current, window, cx));
        self.review_model_subscription = Some(cx.subscribe_in(
            &picker,
            window,
            move |this, _, event: &ReviewModelEvent, window, cx| {
                if let ReviewModelEvent::Selected(spec) = event {
                    this.save_subagent_profile(id.clone(), purpose.clone(), spec.clone(), cx);
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

    pub(super) fn render_subagents(&self, cx: &mut Context<Self>) -> AnyElement {
        let settings = AppSettings::get(cx).subagents.clone().unwrap_or_default();
        let enabled = settings.is_enabled();
        let colors = cx.theme().colors.clone();
        let page = page_body()
            .child(
                div().flex().child(
                    crate::ui::checkbox("enable-subagents", "Enable subagents", enabled, cx)
                        .debug_selector(|| "enable-subagents".into())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.update_subagents(cx, |settings| settings.enabled = Some(!enabled))
                        })),
                ),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(colors.text_muted)
                    .child(
                        "The agent can hand read-only research to subagents that work in \
                         parallel. A subagent can read any file in the project and sends what \
                         it reads to its model. Changes apply to the next run; a trusted \
                         project can restrict them in .zp/settings.json.",
                    ),
            );
        if !enabled {
            return page.into_any_element();
        }
        page.child(section(
            "Model assignment",
            "Which model a subagent works with.",
            cx,
        ))
        .children(
            [
                (
                    Assignment::FollowMainModel,
                    "follow-main-model",
                    "Follow main model",
                    "Use the model and thinking level of the current main run.",
                ),
                (
                    Assignment::TaskProfiles,
                    "task-profiles",
                    "Task profiles",
                    "Let the main agent choose among the profiles you approve.",
                ),
            ]
            .into_iter()
            .map(|(assignment, id, label, description)| {
                radio(
                    id,
                    label,
                    description,
                    settings.assignment() == assignment,
                    cx,
                )
                .debug_selector(move || id.into())
                .on_click(
                    cx.listener(move |this, _, _, cx| this.set_subagent_assignment(assignment, cx)),
                )
            }),
        )
        .when(settings.assignment() == Assignment::TaskProfiles, |page| {
            page.child(self.render_subagent_profiles(&settings, cx))
        })
        .child(section(
            "Approval",
            "Whether you confirm each task before it is sent to a model.",
            cx,
        ))
        .children(
            [
                (
                    Approval::Allowlisted,
                    "allowlisted-subagents",
                    "Start automatically",
                    "Subagents only read the project, with the main model or a profile's \
                     model, within the limits below.",
                ),
                (
                    Approval::Ask,
                    "ask-subagents",
                    "Ask each time",
                    "Confirm each task, its model and what it may read, also in Full auto.",
                ),
            ]
            .into_iter()
            .map(|(approval, id, label, description)| {
                radio(id, label, description, settings.approval() == approval, cx)
                    .debug_selector(move || id.into())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.update_subagents(cx, |settings| settings.approval = Some(approval))
                    }))
            }),
        )
        .child(section(
            "Limits",
            "Parallel tasks and tasks per run are required; leave the others empty for no \
             limit (output tokens then use the model's own limit).",
            cx,
        ))
        .child(
            div().flex().flex_wrap().gap_3().children(
                LIMITS
                    .iter()
                    .zip(&self.subagent_form.limits)
                    .map(|(limit, field)| {
                        let id = limit.id;
                        div()
                            .debug_selector(move || id.into())
                            .flex()
                            .flex_col()
                            .gap_1()
                            .w(px(160.))
                            .text_size(px(12.))
                            .child(limit.label)
                            .child(text_field(field, limit.unset.unwrap_or(""), cx))
                    }),
            ),
        )
        .into_any_element()
    }

    /// The task profiles and the form that adds or edits one.
    fn render_subagent_profiles(
        &self,
        settings: &SubagentSettings,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let typed_id = self.subagent_form.id.read(cx).as_str().trim().to_owned();
        let editing = settings.profiles.get(&typed_id).cloned();
        let actions = match &editing {
            Some(profile) => {
                let model = profile.model.clone();
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(
                        button("save-subagent-profile", "Save", ButtonKind::Primary, cx)
                            .debug_selector(|| "save-subagent-profile".into())
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.submit_subagent_profile(Some(model.clone()), window, cx)
                            })),
                    )
                    .child(
                        button(
                            "change-subagent-model",
                            "Change model…",
                            ButtonKind::Normal,
                            cx,
                        )
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.submit_subagent_profile(None, window, cx)
                        })),
                    )
                    .child(
                        button(
                            "inherit-subagent-model",
                            "Use main model",
                            ButtonKind::Normal,
                            cx,
                        )
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.submit_subagent_profile(Some("inherit".into()), window, cx)
                        })),
                    )
                    .child(
                        button("cancel-subagent-edit", "Cancel", ButtonKind::Normal, cx)
                            .on_click(cx.listener(|this, _, _, cx| this.clear_subagent_form(cx))),
                    )
            }
            None => div()
                .flex()
                .flex_wrap()
                .gap_2()
                .child(
                    button(
                        "select-subagent-model",
                        "Choose model…",
                        ButtonKind::Primary,
                        cx,
                    )
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.submit_subagent_profile(None, window, cx)
                    })),
                )
                .child(
                    button(
                        "inherit-subagent-model",
                        "Use main model",
                        ButtonKind::Normal,
                        cx,
                    )
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.submit_subagent_profile(Some("inherit".into()), window, cx)
                    })),
                ),
        };
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(section(
                "Profiles",
                "The models that may receive delegated tasks and the files subagents read. \
                 The main agent picks a profile by its purpose.",
                cx,
            ))
            .when(settings.profiles.is_empty(), |list| {
                list.child(
                    div()
                        .text_size(px(12.))
                        .text_color(colors.text_muted)
                        .child("No profiles yet: subagents are off until you add one."),
                )
            })
            .children(settings.profiles.iter().map(|(id, profile)| {
                self.render_subagent_profile(
                    id,
                    profile,
                    settings.default_profile.as_deref() == Some(id),
                    cx,
                )
            }))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3()
                    .rounded(px(6.))
                    .border_1()
                    .border_color(colors.border)
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(match &editing {
                                Some(_) => format!("Edit {typed_id}"),
                                None => "Add a profile".to_owned(),
                            }),
                    )
                    .child(text_field(
                        &self.subagent_form.id,
                        "Profile ID, e.g. explore",
                        cx,
                    ))
                    .child(text_field(
                        &self.subagent_form.purpose,
                        "Purpose, e.g. locate code and tests",
                        cx,
                    ))
                    .child(actions),
            )
            .into_any_element()
    }

    fn render_subagent_profile(
        &self,
        id: &str,
        profile: &Profile,
        default: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let resolved = (profile.model != "inherit")
            .then(|| configured_model(&cx.global::<AppModels>().models, &profile.model));
        let model = match &resolved {
            None => "Main model".to_owned(),
            Some(Some(model)) => match model_endpoint(model) {
                Some(endpoint) => format!("{} · {endpoint}", profile.model),
                None => profile.model.clone(),
            },
            Some(None) => format!("{} · unavailable", profile.model),
        };
        // The levels the model offers (all of them for the main model, unknown here).
        let (levels, thinking) = match resolved.flatten() {
            Some(model) => (
                model.supported_thinking_levels(),
                model.clamp_thinking_level(profile.thinking),
            ),
            None => (ThinkingLevel::ALL.to_vec(), profile.thinking),
        };
        let (edit_id, purpose) = (id.to_owned(), profile.description.clone());
        let default_id = id.to_owned();
        let remove_id = id.to_owned();
        let thinking_id = id.to_owned();
        let level_options = levels.clone();
        div()
            .id(gpui::SharedString::from(format!("subagent-profile-{id}")))
            .flex()
            .flex_col()
            .gap_1p5()
            .p_3()
            .rounded(px(6.))
            .border_1()
            .border_color(colors.border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(id.to_owned()))
                    .when(default, |title| {
                        title.child(
                            div()
                                .text_size(px(11.))
                                .text_color(colors.text_muted)
                                .child("default"),
                        )
                    }),
            )
            .when(!profile.description.is_empty(), |card| {
                card.child(profile.description.clone())
            })
            .child(
                div()
                    .text_color(colors.text_muted)
                    .text_size(px(12.))
                    .child(model),
            )
            .when(levels.len() > 1, |card| {
                card.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(colors.text_muted)
                                .child("Thinking"),
                        )
                        .child(segmented(
                            "subagent-thinking",
                            levels
                                .iter()
                                .map(|level| (level.as_str().to_owned(), *level == thinking)),
                            cx,
                            move |this, index, cx| {
                                let level = level_options[index];
                                let id = thinking_id.clone();
                                this.update_subagents(cx, |settings| {
                                    if let Some(profile) = settings.profiles.get_mut(&id) {
                                        profile.thinking = level;
                                    }
                                })
                            },
                        )),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(
                        button("edit-profile", "Edit", ButtonKind::Normal, cx).on_click(
                            cx.listener(move |this, _, window, cx| {
                                this.subagent_form
                                    .id
                                    .update(cx, |field, cx| field.emplace(&edit_id, cx));
                                this.subagent_form
                                    .purpose
                                    .update(cx, |field, cx| field.emplace(&purpose, cx));
                                this.subagent_form
                                    .purpose
                                    .read(cx)
                                    .focus_handle(cx)
                                    .focus(window, cx);
                            }),
                        ),
                    )
                    .when(!default, |actions| {
                        actions.child(
                            button("default-profile", "Make default", ButtonKind::Normal, cx)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.update_subagents(cx, |settings| {
                                        settings.default_profile = Some(default_id.clone());
                                    })
                                })),
                        )
                    })
                    .child(
                        button("remove-profile", "Remove", ButtonKind::Danger, cx).on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.update_subagents(cx, |settings| {
                                    settings.profiles.remove(&remove_id);
                                    if settings.default_profile.as_deref() == Some(&remove_id) {
                                        settings.default_profile =
                                            settings.profiles.keys().next().cloned();
                                    }
                                })
                            }),
                        ),
                    ),
            )
            .into_any_element()
    }
}
