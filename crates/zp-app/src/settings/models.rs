//! The Models page: catalog sync status, a forced refresh, and every model offered by the
//! configured providers with where it comes from.

use std::collections::HashMap;

use gpui::{AnyElement, App, Context, IntoElement, div, prelude::*, px};
use zp_ai::{Model, ModelSource};
use zp_core::model_catalog::{SyncOutcome, SyncReport};

use super::{ButtonKind, SettingsWindow, ago, button, page_body, section, text_field};
use crate::store::AppModels;
use crate::theme::ActiveTheme;

/// A model as listed.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelRow {
    pub id: String,
    pub name: String,
    pub context: u64,
    pub price: Option<(f64, f64)>,
    pub source: ModelSource,
    /// The provider was added by the user.
    pub custom: bool,
}

fn outcome_text(outcome: &SyncOutcome) -> String {
    match outcome {
        SyncOutcome::Updated { models } => format!("updated ({models} models)"),
        SyncOutcome::NotModified => "unchanged".into(),
        SyncOutcome::Fresh => "up to date".into(),
        SyncOutcome::Skipped(why) => format!("skipped: {why}"),
        SyncOutcome::Failed(error) => format!("failed: {error}"),
    }
}

/// One line per layer of a sync report.
pub fn report_lines(report: &SyncReport) -> Vec<(String, bool)> {
    let failed = |o: &SyncOutcome| matches!(o, SyncOutcome::Failed(_));
    report
        .models_dev
        .iter()
        .map(|outcome| {
            (
                format!("models.dev: {}", outcome_text(outcome)),
                failed(outcome),
            )
        })
        .chain(report.upstream.iter().map(|(provider, outcome)| {
            (
                format!("{provider}: {}", outcome_text(outcome)),
                failed(outcome),
            )
        }))
        .collect()
}

fn tokens(count: u64) -> String {
    match count {
        0 => "—".into(),
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        n => format!("{}k", n / 1000),
    }
}

fn price(model: &Model) -> Option<(f64, f64)> {
    (model.cost.input > 0.0 || model.cost.output > 0.0)
        .then_some((model.cost.input, model.cost.output))
}

fn price_key(id: &str) -> String {
    id.rsplit_once('/')
        .map_or(id, |(_, name)| name)
        .to_ascii_lowercase()
}

impl SettingsWindow {
    /// Models of providers with credentials, matching the search.
    pub fn model_rows(&self, cx: &App) -> Vec<ModelRow> {
        let models = &cx.global::<AppModels>().models;
        let custom: Vec<String> = crate::store::AppSettings::get(cx)
            .providers
            .iter()
            .map(|p| p.id.clone())
            .collect();
        let query = self.model_search.read(cx).as_str().trim().to_lowercase();
        let catalog = models.models(None);
        let mut prices: HashMap<String, (f64, f64)> = catalog
            .iter()
            .filter_map(|model| price(model).map(|price| (model.id.to_ascii_lowercase(), price)))
            .collect();
        // Prefer a bare catalog id over a reseller's prefixed id for fallback prices.
        for model in &catalog {
            if let Some(price) = price(model) {
                prices.entry(price_key(&model.id)).or_insert(price);
            }
        }
        models
            .available_providers()
            .iter()
            .flat_map(|provider| models.models(Some(provider.id())))
            .filter(|model: &Model| {
                let haystack = format!("{} {}", model.qualified_id(), model.name).to_lowercase();
                query.split_whitespace().all(|word| haystack.contains(word))
            })
            .map(|model| ModelRow {
                source: models.model_source(&model),
                custom: custom.contains(&model.provider),
                id: model.qualified_id(),
                name: model.name.clone(),
                context: model.context_window,
                price: price(&model).or_else(|| prices.get(&price_key(&model.id)).copied()),
            })
            .collect()
    }

    /// Refreshes models.dev and every provider list, ignoring cache ages.
    pub fn refresh_all(&mut self, cx: &mut Context<Self>) {
        AppModels::sync(cx, Vec::new(), true);
        self.notice = Some(("Refreshing the model catalog…".into(), false));
        cx.notify();
    }

    pub(super) fn render_models(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors.clone();
        let app = cx.global::<AppModels>();
        let running = app.sync.running;
        let last = app.sync.last.clone();
        let status = AppModels::catalog_cache(cx).map(|cache| cache.status(&app.models));
        let models_dev = match status.as_ref().and_then(|s| s.models_dev) {
            Some((at, count)) => format!("models.dev: {count} models, checked {}", ago(at)),
            None => "models.dev: not synced yet".to_owned(),
        };
        let rows = self.model_rows(cx);
        let count = rows.len();
        let list = div()
            .w_full()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .children(rows.into_iter().map(|row| {
                let badge = match row.source {
                    ModelSource::BuiltIn => colors.text_muted,
                    ModelSource::ModelsDev => colors.accent,
                    ModelSource::Upstream => colors.warning,
                };
                div()
                    .h(px(28.))
                    .flex_shrink_0()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_2()
                    .border_b_1()
                    .border_color(colors.border)
                    .text_size(px(12.))
                    .child(div().flex_1().min_w_0().truncate().child(row.id.clone()))
                    .child(
                        div()
                            .w(px(180.))
                            .truncate()
                            .text_color(colors.text_muted)
                            .child(row.name.clone()),
                    )
                    .child(
                        div()
                            .w(px(50.))
                            .text_color(colors.text_muted)
                            .child(tokens(row.context)),
                    )
                    .child(
                        div()
                            .w(px(120.))
                            .flex_shrink_0()
                            .truncate()
                            .text_color(colors.text_muted)
                            .child(
                                row.price
                                    .map(|(i, o)| format!("${i:.2} / ${o:.2}"))
                                    .unwrap_or_else(|| "—".into()),
                            ),
                    )
                    .child(
                        div().w(px(84.)).flex().justify_end().child(
                            div()
                                .px_1()
                                .rounded(px(3.))
                                .text_size(px(10.))
                                .border_1()
                                .border_color(badge)
                                .text_color(badge)
                                .child(if row.custom && row.source == ModelSource::BuiltIn {
                                    "custom"
                                } else {
                                    row.source.label()
                                }),
                        ),
                    )
            }));

        page_body()
            .child(section(
                "Model catalog",
                "Built-in models are refreshed from models.dev (limits, prices, new models) and filtered by what each provider lists for your key. ZP refreshes in the background once a day.",
                cx,
            ))
            .child(div().text_size(px(12.)).text_color(colors.text_muted).child(models_dev))
            .when_some(last, |this, (at, report)| {
                this.child(
                    div()
                        .flex()
                        .flex_col()
                        .text_size(px(12.))
                        .child(
                            div()
                                .text_color(colors.text_muted)
                                .child(format!("Last refresh {}:", ago(at))),
                        )
                        .children(report_lines(&report).into_iter().map(|(line, failed)| {
                            div()
                                .pl_3()
                                .text_color(if failed { colors.error } else { colors.text_muted })
                                .child(line)
                        })),
                )
            })
            .child(
                div().flex().child(
                    button(
                        "refresh-all",
                        if running { "Refreshing…" } else { "Refresh All" },
                        ButtonKind::Normal,
                        cx,
                    )
                    .when(!running, |this| {
                        this.on_click(cx.listener(|this, _, _, cx| this.refresh_all(cx)))
                    }),
                ),
            )
            .child(section(
                "Available models",
                "Models of the providers that have a key. Sources: built-in catalog, models.dev, or only the provider's own list.",
                cx,
            ))
            .child(text_field(&self.model_search, "Search models…", cx))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(colors.text_muted)
                    .child(format!("{count} models")),
            )
            .child(list)
            .into_any_element()
    }
}
