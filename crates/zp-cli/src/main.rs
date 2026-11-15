//! Headless entry point for ZP, used to exercise the core crates without the GUI.

mod run;

use std::io::IsTerminal as _;

use anyhow::{Context as _, Result, bail};
use clap::{Args, Parser, Subcommand};
use zp_ai::{MODELS_DEV_URL, Model, Models, ThinkingLevel};
use zp_core::model_catalog::{DEFAULT_MAX_AGE, ModelCatalogCache, SyncOptions, SyncOutcome};

#[derive(Parser)]
#[command(name = "zp-cli", version, about = "ZP command-line interface")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Send a prompt and stream the response.
    Run(run::RunArgs),
    /// List models.
    Models {
        /// Only this provider.
        #[arg(short, long)]
        provider: Option<String>,
        /// Only providers with credentials.
        #[arg(short, long)]
        available: bool,
    },
    /// List providers and whether credentials are configured.
    Providers,
    /// Refresh model catalogs from models.dev and the configured providers' model lists.
    Sync(SyncArgs),
    /// Print data locations.
    Paths,
}

#[derive(Args)]
struct SyncArgs {
    /// Refresh even if the cached copies are recent.
    #[arg(short, long)]
    force: bool,
    /// Only refresh the model list of this provider (repeatable).
    #[arg(short, long = "provider")]
    providers: Vec<String>,
    /// Skip models.dev.
    #[arg(long)]
    no_models_dev: bool,
    /// Skip provider model lists.
    #[arg(long)]
    no_upstream: bool,
    /// models.dev catalog URL.
    #[arg(long, default_value = MODELS_DEV_URL)]
    models_dev_url: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    zp_core::logging::init();
    let cli = Cli::parse();
    let models = Models::builtin();
    let cache = zp_core::paths::data_dir()
        .map(|dir| ModelCatalogCache::new(&dir))
        .inspect_err(|error| eprintln!("warning: model cache unavailable: {error:#}"))
        .ok();
    if let Some(cache) = &cache {
        cache.load(&models);
    }
    match cli.command {
        Command::Run(args) => run::run(&models, args).await,
        Command::Models {
            provider,
            available,
        } => {
            list_models(&models, provider.as_deref(), available);
            Ok(())
        }
        Command::Providers => {
            list_providers(&models);
            Ok(())
        }
        Command::Sync(args) => {
            let cache = cache.context("no data directory for the model cache")?;
            sync(&models, &cache, args).await
        }
        Command::Paths => {
            let data_dir = zp_core::paths::data_dir()?;
            println!("data:     {}", data_dir.display());
            println!(
                "settings: {}",
                zp_core::paths::settings_file(&data_dir).display()
            );
            println!(
                "state:    {}",
                zp_core::paths::state_file(&data_dir).display()
            );
            println!(
                "sessions: {}",
                zp_core::paths::sessions_dir(&data_dir).display()
            );
            Ok(())
        }
    }
}

async fn sync(models: &Models, cache: &ModelCatalogCache, args: SyncArgs) -> Result<()> {
    let options = SyncOptions {
        models_dev: !args.no_models_dev,
        models_dev_url: args.models_dev_url,
        upstream: !args.no_upstream,
        providers: args.providers,
        max_age: if args.force {
            std::time::Duration::ZERO
        } else {
            DEFAULT_MAX_AGE
        },
        ..SyncOptions::default()
    };
    let cancel = options.cancel.clone();
    let report = tokio::select! {
        report = cache.sync(models, &options) => report,
        _ = tokio::signal::ctrl_c() => {
            cancel.cancel();
            bail!("cancelled");
        }
    };

    let describe = |outcome: &SyncOutcome| match outcome {
        SyncOutcome::Updated { models } => ("updated", format!("{models} models fetched")),
        SyncOutcome::NotModified => ("current", "unchanged upstream".to_owned()),
        SyncOutcome::Fresh => ("current", "cached copy is recent (use --force)".to_owned()),
        SyncOutcome::Skipped(reason) => ("skipped", reason.clone()),
        SyncOutcome::Failed(error) => ("failed", error.clone()),
    };
    let mut rows = vec![[
        "SOURCE".to_owned(),
        "STATUS".to_owned(),
        "MODELS".to_owned(),
        "DETAIL".to_owned(),
    ]];
    let mut failed = false;
    if let Some(outcome) = &report.models_dev {
        let (status, detail) = describe(outcome);
        failed |= matches!(outcome, SyncOutcome::Failed(_));
        rows.push(["models.dev".into(), status.into(), "-".into(), detail]);
    }
    for (provider, outcome) in &report.upstream {
        let (status, detail) = describe(outcome);
        failed |= matches!(outcome, SyncOutcome::Failed(_));
        rows.push([
            provider.clone(),
            status.into(),
            models.models(Some(provider)).len().to_string(),
            detail,
        ]);
    }
    if report.upstream.is_empty() && !args.no_upstream {
        rows.push([
            "providers".into(),
            "skipped".into(),
            "-".into(),
            "no provider has credentials".into(),
        ]);
    }
    print_table(&rows);
    println!("cache: {}", cache.dir().display());
    if failed {
        bail!("some sources failed to refresh");
    }
    Ok(())
}

/// Resolves `--model`, or the default model of the first provider with credentials.
pub(crate) fn select_model(models: &Models, spec: Option<&str>) -> Result<Model> {
    if let Some(spec) = spec {
        return models
            .find(spec)
            .with_context(|| format!("unknown model `{spec}` (see `zp-cli models`)"));
    }
    for provider in models.available_providers() {
        if let Some(model) = models.default_model(provider.id()) {
            return Ok(model);
        }
    }
    let vars: Vec<String> = models
        .providers()
        .iter()
        .flat_map(|p| p.api_key_env_vars().to_vec())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    bail!(
        "no provider has credentials; set one of: {}",
        vars.join(", ")
    )
}

fn format_tokens(tokens: u64) -> String {
    match tokens {
        t if t >= 1_000_000 => format!("{}M", t as f64 / 1_000_000.0),
        t if t >= 1_000 => format!("{}k", t / 1_000),
        t => t.to_string(),
    }
}

fn thinking_summary(model: &Model) -> String {
    let levels = model.supported_thinking_levels();
    if levels == [ThinkingLevel::Off] {
        return "-".into();
    }
    levels
        .iter()
        .map(|level| level.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

fn list_models(models: &Models, provider: Option<&str>, available: bool) {
    let mut rows = vec![[
        "MODEL".to_owned(),
        "CONTEXT".to_owned(),
        "OUTPUT".to_owned(),
        "$IN/$OUT per 1M".to_owned(),
        "THINKING".to_owned(),
    ]];
    for model in models.models(provider) {
        if available && models.resolve_auth(&model.provider).is_none() {
            continue;
        }
        rows.push([
            model.qualified_id(),
            format_tokens(model.context_window),
            format_tokens(model.max_tokens),
            format!("{}/{}", model.cost.input, model.cost.output),
            thinking_summary(&model),
        ]);
    }
    print_table(&rows);
}

fn list_providers(models: &Models) {
    let mut rows = vec![[
        "PROVIDER".to_owned(),
        "NAME".to_owned(),
        "MODELS".to_owned(),
        "DEFAULT".to_owned(),
        "AUTH".to_owned(),
    ]];
    for provider in models.providers() {
        let auth = match models.resolve_auth(provider.id()) {
            Some(auth) => format!("ok ({})", auth.source),
            None => format!("missing ({})", provider.api_key_env_vars().join(" / ")),
        };
        rows.push([
            provider.id().to_owned(),
            provider.name().to_owned(),
            models.models(Some(provider.id())).len().to_string(),
            provider.default_model_id().unwrap_or("-").to_owned(),
            auth,
        ]);
    }
    print_table(&rows);
}

fn print_table<const N: usize>(rows: &[[String; N]]) {
    let mut widths = [0usize; N];
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let bold = std::io::stdout().is_terminal();
    for (index, row) in rows.iter().enumerate() {
        let line: Vec<String> = row
            .iter()
            .zip(widths)
            .map(|(cell, width)| format!("{cell:<width$}"))
            .collect();
        let line = line.join("  ");
        if index == 0 && bold {
            println!("\x1b[1m{}\x1b[0m", line.trim_end());
        } else {
            println!("{}", line.trim_end());
        }
    }
}
