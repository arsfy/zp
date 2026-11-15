//! Building the provider registry: built-in providers, user-defined ones from the
//! settings, and stored credentials.

use std::sync::Arc;

use anyhow::{bail, ensure};
use zp_ai::{Api, HttpProvider, InputModality, Model, ModelCompat, ModelCost, Models, Provider};

use crate::settings::CustomProvider;

/// Context window assumed for custom models.
const CUSTOM_CONTEXT_WINDOW: u64 = 128_000;
/// Output limit assumed for custom models.
const CUSTOM_MAX_TOKENS: u64 = 16_384;

/// Checks a custom provider before it is saved; `existing` are the ids already taken.
pub fn validate_custom_provider(def: &CustomProvider, existing: &[String]) -> anyhow::Result<()> {
    ensure!(!def.id.is_empty(), "the id is empty");
    ensure!(
        def.id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_')),
        "the id may only contain a-z, 0-9, - and _"
    );
    ensure!(
        !existing.iter().any(|id| id == &def.id),
        "a provider named `{}` already exists",
        def.id
    );
    let url = def.base_url.trim();
    ensure!(
        url.starts_with("http://") || url.starts_with("https://"),
        "the base URL must start with http:// or https://"
    );
    api_of(def)?;
    for name in def.headers.keys() {
        ensure!(
            !name.is_empty() && name.chars().all(|c| c.is_ascii_graphic() && c != ':'),
            "invalid header name `{name}`"
        );
    }
    Ok(())
}

fn api_of(def: &CustomProvider) -> anyhow::Result<Api> {
    match def.api.as_deref().unwrap_or("openai-completions") {
        "openai-completions" => Ok(Api::OpenAiCompletions),
        "anthropic-messages" => Ok(Api::AnthropicMessages),
        other => bail!("unknown API `{other}` (use openai-completions or anthropic-messages)"),
    }
}

/// The provider described by `def`. It needs no key (local servers); a stored key is used
/// when present.
pub fn custom_provider(def: &CustomProvider) -> anyhow::Result<Arc<dyn Provider>> {
    let api = api_of(def)?;
    let base_url = def.base_url.trim().trim_end_matches('/').to_owned();
    let headers: std::collections::HashMap<String, String> = def
        .headers
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let models: Vec<Model> = def
        .models
        .iter()
        .map(|id| id.trim())
        .filter(|id| !id.is_empty())
        .map(|id| Model {
            id: id.to_owned(),
            name: id.to_owned(),
            api,
            provider: def.id.clone(),
            base_url: base_url.clone(),
            reasoning: false,
            input: vec![InputModality::Text],
            cost: ModelCost::default(),
            context_window: CUSTOM_CONTEXT_WINDOW,
            max_tokens: CUSTOM_MAX_TOKENS,
            thinking_level_map: None,
            headers: headers.clone(),
            compat: ModelCompat::default(),
        })
        .collect();
    let name = if def.name.trim().is_empty() {
        def.id.clone()
    } else {
        def.name.trim().to_owned()
    };
    let mut provider =
        HttpProvider::new(def.id.clone(), name, Vec::<String>::new(), models.clone())
            .with_api_endpoint(api, base_url)
            .without_api_key();
    if let Some(first) = models.first() {
        provider = provider.with_default_model(first.id.clone());
    }
    Ok(Arc::new(provider))
}

/// Registers the custom providers of the settings (invalid ones are logged and skipped).
pub fn register_custom_providers(models: &Models, providers: &[CustomProvider]) {
    for def in providers {
        match custom_provider(def) {
            Ok(provider) => models.register(provider),
            Err(error) => {
                tracing::warn!(provider = %def.id, error = %format!("{error:#}"), "skipping provider")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_providers() {
        let def = CustomProvider {
            id: "local".into(),
            name: "Local LLM".into(),
            base_url: "http://localhost:11434/v1/".into(),
            api: None,
            headers: [("X-Team".to_owned(), "zp".to_owned())].into(),
            models: vec!["qwen3-coder".into(), " ".into()],
        };
        validate_custom_provider(&def, &["openai".into()]).unwrap();
        assert!(validate_custom_provider(&def, &["local".into()]).is_err());
        for bad in [
            CustomProvider {
                id: "Bad Id".into(),
                ..def.clone()
            },
            CustomProvider {
                base_url: "localhost:1234".into(),
                ..def.clone()
            },
            CustomProvider {
                api: Some("grpc".into()),
                ..def.clone()
            },
        ] {
            assert!(validate_custom_provider(&bad, &[]).is_err(), "{bad:?}");
        }

        let models = Models::new().with_env(Arc::new(|_| None));
        register_custom_providers(&models, std::slice::from_ref(&def));
        let model = models.default_model("local").unwrap();
        assert_eq!(model.id, "qwen3-coder");
        assert_eq!(model.base_url, "http://localhost:11434/v1");
        assert_eq!(model.headers["X-Team"], "zp");
        assert_eq!(models.models(Some("local")).len(), 1);
        // Keyless: available without credentials.
        assert_eq!(models.available_providers().len(), 1);
        assert_eq!(
            models.provider("local").unwrap().api_endpoint(),
            Some((Api::OpenAiCompletions, "http://localhost:11434/v1"))
        );
        assert!(models.remove_provider("local"));
        assert!(models.provider("local").is_none());
    }
}
