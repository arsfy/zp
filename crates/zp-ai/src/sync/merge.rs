//! Combining a provider's built-in catalog with models.dev metadata and its upstream list.
//!
//! Rules:
//! - Built-in models keep their protocol settings (`api`, `baseUrl`, `compat`, thinking
//!   map); models.dev refreshes limits, pricing and input modalities.
//! - models.dev models the catalog lacks are added when they are current, tool-capable chat
//!   models. Their protocol settings come from the most similar known model: same family,
//!   else longest shared id prefix, else the provider's endpoint defaults.
//! - With an upstream list, it decides availability: unlisted models are dropped (dated
//!   snapshots keep their alias, `:variant` ids their base) and listed unknown models are
//!   added, described by models.dev when possible and by the list entry otherwise. Limits,
//!   modalities and base prices the provider reports win over the other layers.
//! - Listed models neither the catalog nor the provider's models.dev entry describe (models
//!   of gateways that resell other vendors' models) borrow capabilities from a [`Reference`]:
//!   the same model in another provider's built-in catalog or models.dev entry, including
//!   its prices when the listing reports none.
//! - A listing that names the routes a model is served on (`supported_endpoints`) moves the
//!   model to a protocol it supports, e.g. Claude to `/messages` on a gateway whose default is
//!   Chat Completions.

use std::collections::{BTreeMap, HashMap, HashSet};

use super::{ModelsDevCatalog, ModelsDevModel, ModelsDevProvider, UpstreamModel, UpstreamModels};
use crate::model::{Api, InputModality, Model, ModelCompat, ModelCost, ThinkingLevel};
use crate::provider::Provider;

/// Context window assumed when nothing reports one.
const DEFAULT_CONTEXT_WINDOW: u64 = 128_000;
/// Output limit assumed when nothing reports one.
const DEFAULT_MAX_TOKENS: u64 = 16_384;
/// Shortest id prefix that makes a model a template for another.
const MIN_SHARED_PREFIX: usize = 4;

/// What is known about models outside a provider's own layers.
#[derive(Default)]
pub(crate) struct Reference<'a> {
    /// The whole models.dev catalog (all linked providers).
    pub(crate) catalog: Option<&'a ModelsDevCatalog>,
    /// Built-in catalog models of the other providers.
    pub(crate) builtin: Vec<Model>,
}

/// Lowercased id and the id without its `vendor/` prefix.
fn match_keys(id: &str) -> (String, String) {
    let lower = id.to_ascii_lowercase();
    let bare = lower.rsplit('/').next().unwrap_or(&lower).to_owned();
    (lower, bare)
}

/// Finds the first candidate whose id equals `id` (case-insensitively), else the first
/// whose id without the vendor prefix does.
fn find_same_model<'a, T>(
    id: &str,
    candidates: impl Iterator<Item = &'a T> + Clone,
    id_of: impl Fn(&T) -> &str,
) -> Option<&'a T> {
    let (lower, bare) = match_keys(id);
    candidates
        .clone()
        .find(|c| id_of(c).eq_ignore_ascii_case(&lower))
        .or_else(|| {
            candidates
                .into_iter()
                .find(|c| match_keys(id_of(c)).1 == bare)
        })
}

impl Reference<'_> {
    /// The same model in another built-in catalog, served over `api`.
    fn builtin(&self, id: &str, api: Api) -> Option<&Model> {
        find_same_model(id, self.builtin.iter().filter(|m| m.api == api), |m| &m.id)
    }

    /// The same model on models.dev under any provider.
    fn models_dev(&self, id: &str) -> Option<&ModelsDevModel> {
        let catalog = self.catalog?;
        let models = catalog
            .providers
            .values()
            .flat_map(|provider| provider.models.values())
            .filter(|info| info.is_chat() && info.tool_call);
        find_same_model(id, models, |info| &info.id)
    }
}

/// The effective model list of `provider`.
pub(crate) fn resolve_models(
    provider: &dyn Provider,
    dev: Option<&ModelsDevProvider>,
    upstream: Option<&UpstreamModels>,
    reference: &Reference<'_>,
) -> Vec<Model> {
    let dev_model = |id: &str| dev.and_then(|dev| dev.models.get(id));
    let mut models: Vec<Model> = provider
        .models()
        .iter()
        .map(|model| match dev_model(&model.id) {
            Some(info) => overlay(model.clone(), info),
            None => model.clone(),
        })
        .collect();
    let templates = Templates::new(provider, models.clone(), dev);

    let mut present: HashSet<String> = models.iter().map(|m| m.id.clone()).collect();
    if let Some(dev) = dev {
        for info in dev.models.values() {
            if info.is_offered()
                && !present.contains(&info.id)
                && let Some(model) = templates.describe_models_dev(info)
            {
                present.insert(model.id.clone());
                models.push(model);
            }
        }
    }

    if let Some(upstream) = upstream.filter(|upstream| !upstream.models.is_empty()) {
        let listed: HashMap<&str, &UpstreamModel> =
            upstream.models.iter().map(|m| (m.id.as_str(), m)).collect();
        models.retain(|model| is_listed(&model.id, &listed));
        for model in &mut models {
            if let Some(entry) = listed.get(model.id.as_str()) {
                overlay_upstream(model, entry);
                if let Some(endpoints) = &entry.endpoints {
                    route(model, endpoints);
                }
            }
        }
        present = models.iter().map(|m| m.id.clone()).collect();
        for entry in &upstream.models {
            if present.contains(&entry.id) {
                continue;
            }
            let model = match dev_model(&entry.id) {
                // Listed means available, so deprecated models count here.
                Some(info) => (info.is_chat() && info.tool_call)
                    .then(|| templates.describe_models_dev(info))
                    .flatten()
                    .map(|mut model| {
                        if let Some(endpoints) = &entry.endpoints {
                            route(&mut model, endpoints);
                        }
                        model
                    }),
                None => entry
                    .looks_like_chat()
                    .then(|| templates.describe_upstream(entry, reference))
                    .flatten(),
            };
            if let Some(model) = model {
                present.insert(model.id.clone());
                models.push(model);
            }
        }
    }

    models.sort_by(|a, b| a.id.cmp(&b.id));
    models
}

/// Refreshes a known model's metadata from models.dev.
fn overlay(mut model: Model, info: &ModelsDevModel) -> Model {
    if let Some(cost) = info.model_cost() {
        model.cost = cost;
    }
    if info.limit.context > 0 {
        model.context_window = info.limit.context;
    }
    if info.limit.output > 0 {
        model.max_tokens = info.limit.output;
    }
    if !info.modalities.input.is_empty() {
        model.input = info.input_modalities();
    }
    model
}

/// Applies what the provider itself reports about a model.
fn overlay_upstream(model: &mut Model, entry: &UpstreamModel) {
    if let Some(context) = entry.context_window {
        model.context_window = context;
    }
    if let Some(max_tokens) = entry.max_tokens {
        model.max_tokens = max_tokens;
    }
    if let Some(input) = &entry.input {
        model.input.clone_from(input);
    }
    if let Some(cost) = &entry.cost {
        // Listings carry flat prices only; keep known context tiers.
        model.cost = ModelCost {
            tiers: std::mem::take(&mut model.cost.tiers),
            ..cost.clone()
        };
    }
}

/// The protocol served on a listed route (`/v1/…` prefixes ignored).
fn api_of_endpoint(endpoint: &str) -> Option<Api> {
    let path = endpoint.trim().trim_end_matches('/');
    let path = path.strip_prefix("/v1").unwrap_or(path);
    match path {
        "/chat/completions" => Some(Api::OpenAiCompletions),
        "/responses" => Some(Api::OpenAiResponses),
        "/messages" => Some(Api::AnthropicMessages),
        _ => None,
    }
}

/// The base URL for `to` on a gateway whose routes share one prefix: OpenAI-style clients
/// append `/chat/completions` to `…/v1`, the Anthropic client appends `/v1/messages` to the
/// root. `None` when the URL does not have that shape.
fn rebase(base_url: &str, from: Api, to: Api) -> Option<String> {
    let anthropic = |api| api == Api::AnthropicMessages;
    let base = base_url.trim_end_matches('/');
    match (anthropic(from), anthropic(to)) {
        (false, true) => base.strip_suffix("/v1").map(str::to_owned),
        (true, false) => Some(format!("{base}/v1")),
        _ => Some(base.to_owned()),
    }
}

/// Moves `model` to a protocol it is served on, when the listing names its routes and the
/// current protocol is not among them. Protocol settings of the old protocol are dropped.
fn route(model: &mut Model, endpoints: &[String]) {
    let served: Vec<Api> = endpoints
        .iter()
        .filter_map(|e| api_of_endpoint(e))
        .collect();
    if served.is_empty() || served.contains(&model.api) {
        return;
    }
    let target = [
        Api::OpenAiCompletions,
        Api::OpenAiResponses,
        Api::AnthropicMessages,
    ]
    .into_iter()
    .filter(|api| served.contains(api))
    .find_map(|api| Some((api, rebase(&model.base_url, model.api, api)?)));
    if let Some((api, base_url)) = target {
        model.api = api;
        model.base_url = base_url;
        model.compat = ModelCompat::default();
        model.thinking_level_map = None;
    }
}

/// A thinking map borrowed from another provider: which levels exist carries over, but the
/// value that turns reasoning off is provider-specific (`none`, `off`, a toggle), so `off`
/// falls back to sending nothing.
fn borrowed_thinking_map(
    map: Option<BTreeMap<ThinkingLevel, Option<String>>>,
) -> Option<BTreeMap<ThinkingLevel, Option<String>>> {
    map.map(|mut map| {
        if matches!(map.get(&ThinkingLevel::Off), Some(Some(_))) {
            map.remove(&ThinkingLevel::Off);
        }
        map
    })
}

/// Whether a known model is covered by the upstream list.
fn is_listed(id: &str, listed: &HashMap<&str, &UpstreamModel>) -> bool {
    if listed.contains_key(id) {
        return true;
    }
    // An alias of a dated snapshot, e.g. `claude-x` for `claude-x-20260101`.
    let is_snapshot = |candidate: &&str| {
        candidate
            .strip_prefix(id)
            .and_then(|rest| rest.strip_prefix('-'))
            .is_some_and(|date| date.len() >= 6 && date.bytes().all(|b| b.is_ascii_digit()))
    };
    if listed.keys().any(is_snapshot) {
        return true;
    }
    // A routing variant of a listed model, e.g. `vendor/model:free`.
    id.rsplit_once(':')
        .is_some_and(|(base, _)| listed.contains_key(base))
}

fn limits(context: Option<u64>, output: Option<u64>) -> (u64, u64) {
    let context = context.filter(|&n| n > 0).unwrap_or(DEFAULT_CONTEXT_WINDOW);
    let output = output
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_MAX_TOKENS.min(context));
    (context, output)
}

fn shared_prefix(a: &str, b: &str) -> usize {
    a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count()
}

/// Picks protocol settings for models the catalog does not describe.
struct Templates {
    provider: String,
    known: Vec<Model>,
    families: HashMap<String, String>,
    fallback: Option<Model>,
}

impl Templates {
    fn new(provider: &dyn Provider, known: Vec<Model>, dev: Option<&ModelsDevProvider>) -> Self {
        let families = dev
            .map(|dev| {
                dev.models
                    .values()
                    .filter_map(|m| Some((m.id.clone(), m.family.clone()?)))
                    .collect()
            })
            .unwrap_or_default();
        let is_default = |model: &&Model| Some(model.id.as_str()) == provider.default_model_id();
        let fallback = match provider.api_endpoint() {
            Some((api, base_url)) => {
                let matching: Vec<&Model> = known
                    .iter()
                    .filter(|m| m.api == api && m.base_url == base_url)
                    .collect();
                matching
                    .iter()
                    .copied()
                    .find(is_default)
                    .or(matching.first().copied())
                    .cloned()
                    .or_else(|| {
                        Some(Model {
                            id: String::new(),
                            name: String::new(),
                            api,
                            provider: provider.id().to_owned(),
                            base_url: base_url.to_owned(),
                            reasoning: false,
                            input: vec![InputModality::Text],
                            cost: ModelCost::default(),
                            context_window: 0,
                            max_tokens: 0,
                            thinking_level_map: None,
                            headers: HashMap::new(),
                            compat: ModelCompat::default(),
                        })
                    })
            }
            None => known.iter().find(is_default).or(known.first()).cloned(),
        };
        Self {
            provider: provider.id().to_owned(),
            known,
            families,
            fallback,
        }
    }

    /// The template for `id` and whether it is from the same family.
    fn pick(&self, id: &str, family: Option<&str>) -> Option<(&Model, bool)> {
        fn closest<'a>(id: &str, candidates: Vec<&'a Model>) -> Option<&'a Model> {
            candidates
                .into_iter()
                .max_by_key(|model| shared_prefix(id, &model.id))
        }
        if let Some(family) = family {
            let same_family = self
                .known
                .iter()
                .filter(|m| self.families.get(&m.id).map(String::as_str) == Some(family))
                .collect();
            if let Some(model) = closest(id, same_family) {
                return Some((model, true));
            }
        }
        // Routed ids (`vendor/model`) only borrow from the same vendor.
        let vendor = id.split_once('/').map(|(vendor, _)| vendor.len() + 1);
        let similar = self
            .known
            .iter()
            .filter(|m| {
                let shared = shared_prefix(id, &m.id);
                shared >= MIN_SHARED_PREFIX.max(vendor.unwrap_or(0))
            })
            .collect();
        if let Some(model) = closest(id, similar) {
            return Some((model, false));
        }
        self.fallback.as_ref().map(|model| (model, false))
    }

    fn instantiate(&self, template: &Model, id: &str, name: Option<&str>) -> Model {
        let mut model = template.clone();
        model.id = id.to_owned();
        model.name = name
            .filter(|name| !name.trim().is_empty())
            .unwrap_or(id)
            .to_owned();
        model.provider.clone_from(&self.provider);
        model
    }

    fn describe_models_dev(&self, info: &ModelsDevModel) -> Option<Model> {
        let (template, same_family) = self.pick(&info.id, info.family.as_deref())?;
        let mut model = self.instantiate(template, &info.id, Some(&info.name));
        model.input = info.input_modalities();
        model.cost = info.model_cost().unwrap_or_default();
        (model.context_window, model.max_tokens) =
            limits(Some(info.limit.context), Some(info.limit.output));
        model.thinking_level_map = if !info.reasoning {
            None
        } else if same_family && template.reasoning {
            template.thinking_level_map.clone()
        } else {
            info.thinking_level_map()
        };
        model.reasoning = info.reasoning;
        Some(model)
    }

    fn describe_upstream(&self, entry: &UpstreamModel, reference: &Reference<'_>) -> Option<Model> {
        let (template, _) = self.pick(&entry.id, None)?;
        let mut model = self.instantiate(template, &entry.id, entry.name.as_deref());
        model.cost = entry.cost.clone().unwrap_or_default();
        model.thinking_level_map = None;
        if let Some(endpoints) = &entry.endpoints {
            route(&mut model, endpoints);
        }

        // What the listing leaves out comes from the same model elsewhere, if known: the
        // capabilities and thinking levels from a built-in catalog (which knows the
        // protocol), limits and modalities preferably from models.dev (kept current).
        let peer = reference.builtin(&entry.id, model.api);
        let info = reference.models_dev(&entry.id);
        if let Some(peer) = peer
            && model.api == Api::AnthropicMessages
        {
            // Same protocol: its settings (e.g. adaptive thinking) apply as they are.
            model.compat = peer.compat.clone();
        }
        let (reasoning, map) = match (peer, info) {
            (Some(peer), _) => (
                peer.reasoning,
                borrowed_thinking_map(peer.thinking_level_map.clone()),
            ),
            (None, Some(info)) => (
                info.reasoning,
                borrowed_thinking_map(info.thinking_level_map()),
            ),
            (None, None) => (false, None),
        };
        let (input, context, output) = match (info, peer) {
            (Some(info), _) => (
                (!info.modalities.input.is_empty()).then(|| info.input_modalities()),
                Some(info.limit.context),
                Some(info.limit.output),
            ),
            (None, Some(peer)) => (
                Some(peer.input.clone()),
                Some(peer.context_window),
                Some(peer.max_tokens),
            ),
            (None, None) => (None, None, None),
        };
        // Prices the listing leaves out: the same model's list price, so usage is not shown
        // as free (models.dev first, kept current; then the built-in catalog).
        if entry.cost.is_none() {
            model.cost = info
                .and_then(ModelsDevModel::model_cost)
                .or_else(|| peer.map(|peer| peer.cost.clone()))
                .unwrap_or_default();
        }
        model.reasoning = entry.reasoning.unwrap_or(reasoning);
        model.thinking_level_map = map.filter(|_| model.reasoning);
        model.input = entry
            .input
            .clone()
            .or(input)
            .unwrap_or_else(|| vec![InputModality::Text]);
        let context = entry.context_window.or(context.filter(|&n| n > 0));
        let output = entry.max_tokens.or_else(|| {
            output
                .filter(|&n| n > 0)
                .map(|n| context.map_or(n, |c| n.min(c)))
        });
        (model.context_window, model.max_tokens) = limits(context, output);
        Some(model)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::*;
    use crate::model::{Api, ThinkingFormat, ThinkingLevel};
    use crate::provider::HttpProvider;
    use crate::sync::ModelsDevCatalog;

    fn catalog_model(id: &str, api: Api, base_url: &str) -> Model {
        let mut model = crate::model::test_model(api);
        model.id = id.into();
        model.name = id.into();
        model.base_url = base_url.into();
        model
    }

    /// A routed provider with two protocols, like OpenRouter.
    fn provider() -> HttpProvider {
        let mut claude = catalog_model(
            "anthropic/claude-a",
            Api::AnthropicMessages,
            "https://r/api",
        );
        claude.reasoning = true;
        claude.thinking_level_map = Some(BTreeMap::from([(ThinkingLevel::Off, None)]));
        claude.compat.force_adaptive_thinking = Some(true);
        let routed = |id: &str| {
            let mut model = catalog_model(id, Api::OpenAiCompletions, "https://r/api/v1");
            model.compat.thinking_format = Some(ThinkingFormat::Openrouter);
            model
        };
        let models = vec![
            claude,
            routed("acme/one"),
            routed("acme/old"),
            routed("acme/one:free"),
            routed("acme/dated"),
        ];
        HttpProvider::new("router", "Router", ["ROUTER_KEY"], models)
            .with_default_model("acme/one")
            .with_api_endpoint(Api::OpenAiCompletions, "https://r/api/v1")
            .with_models_dev_id("router-dev")
    }

    fn dev() -> ModelsDevCatalog {
        let json = json!({
            "router-dev": {"id": "router-dev", "models": {
                "anthropic/claude-a": {
                    "id": "anthropic/claude-a", "family": "claude", "reasoning": true, "tool_call": true,
                    "modalities": {"input": ["text"], "output": ["text"]},
                    "limit": {"context": 1000000, "output": 128000},
                    "cost": {"input": 9, "output": 45}
                },
                "anthropic/claude-b": {
                    "id": "anthropic/claude-b", "name": "Claude B", "family": "claude", "reasoning": true,
                    "tool_call": true, "modalities": {"input": ["text", "image"], "output": ["text"]},
                    "reasoning_options": [{"type": "effort", "values": ["low"]}],
                    "limit": {"context": 500000, "output": 64000}, "cost": {"input": 1, "output": 5}
                },
                "zeta/new": {
                    "id": "zeta/new", "name": "Zeta", "reasoning": true, "tool_call": true,
                    "reasoning_options": [{"type": "effort", "values": ["low", "high"]}],
                    "limit": {"context": 64000, "output": 0}
                },
                "zeta/old": {"id": "zeta/old", "tool_call": true, "status": "deprecated", "limit": {"context": 1}},
                "zeta/notools": {"id": "zeta/notools", "tool_call": false, "limit": {"context": 1}},
                "zeta/speech": {"id": "zeta/speech", "type": "speech", "tool_call": true, "limit": {"context": 1}}
            }}
        });
        ModelsDevCatalog::parse(&serde_json::to_vec(&json).unwrap(), |_| true).unwrap()
    }

    fn ids(models: &[Model]) -> Vec<&str> {
        models.iter().map(|m| m.id.as_str()).collect()
    }

    #[test]
    fn catalog_only_is_unchanged() {
        let provider = provider();
        let models = resolve_models(&provider, None, None, &Reference::default());
        let mut expected: Vec<Model> = provider.models().to_vec();
        expected.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(models, expected);
    }

    #[test]
    fn models_dev_refreshes_and_adds() {
        let provider = provider();
        let dev = dev();
        let models = resolve_models(
            &provider,
            dev.provider("router-dev"),
            None,
            &Reference::default(),
        );
        assert_eq!(
            ids(&models),
            [
                "acme/dated",
                "acme/old",
                "acme/one",
                "acme/one:free",
                "anthropic/claude-a",
                "anthropic/claude-b",
                "zeta/new"
            ]
        );
        let find = |id: &str| models.iter().find(|m| m.id == id).unwrap();

        // Known: metadata refreshed, protocol settings kept.
        let a = find("anthropic/claude-a");
        assert_eq!((a.context_window, a.cost.input), (1_000_000, 9.0));
        assert_eq!(a.input, [InputModality::Text]);
        assert_eq!(a.compat.force_adaptive_thinking, Some(true));

        // Same family: protocol and thinking map from the sibling.
        let b = find("anthropic/claude-b");
        assert_eq!(
            (b.api, b.base_url.as_str()),
            (Api::AnthropicMessages, "https://r/api")
        );
        assert_eq!(b.thinking_level_map, a.thinking_level_map);
        assert_eq!(b.name, "Claude B");
        assert!(b.supports_images());
        assert_eq!(b.provider, "router");

        // Unrelated: the provider endpoint, thinking map derived from effort values.
        let zeta = find("zeta/new");
        assert_eq!(
            (zeta.api, zeta.base_url.as_str()),
            (Api::OpenAiCompletions, "https://r/api/v1")
        );
        assert_eq!(
            zeta.compat.thinking_format,
            Some(ThinkingFormat::Openrouter)
        );
        assert_eq!((zeta.context_window, zeta.max_tokens), (64_000, 16_384));
        assert_eq!(
            zeta.supported_thinking_levels(),
            [ThinkingLevel::Low, ThinkingLevel::High]
        );
    }

    #[test]
    fn upstream_decides_availability() {
        let provider = provider();
        let dev = dev();
        let mut groq_like = UpstreamModel::new("acme/two");
        groq_like.context_window = Some(32_000);
        groq_like.reasoning = Some(true);
        let mut one = UpstreamModel::new("acme/one");
        one.context_window = Some(42_000);
        one.cost = Some(ModelCost {
            input: 0.5,
            ..ModelCost::default()
        });
        let upstream = UpstreamModels::new(vec![
            one,
            UpstreamModel::new("acme/dated-20260102"),
            UpstreamModel::new("anthropic/claude-b"),
            UpstreamModel::new("zeta/old"),
            UpstreamModel::new("zeta/notools"),
            UpstreamModel::new("zeta/speech"),
            UpstreamModel::new("text-embedding-x"),
            groq_like,
        ]);
        let models = resolve_models(
            &provider,
            dev.provider("router-dev"),
            Some(&upstream),
            &Reference::default(),
        );
        assert_eq!(
            ids(&models),
            [
                "acme/dated",
                "acme/dated-20260102",
                "acme/one",
                "acme/one:free",
                "acme/two",
                "anthropic/claude-b",
                "zeta/old"
            ]
        );
        let one = models.iter().find(|m| m.id == "acme/one").unwrap();
        assert_eq!((one.context_window, one.max_tokens), (42_000, 64_000));
        assert_eq!(one.cost.input, 0.5);
        let two = models.iter().find(|m| m.id == "acme/two").unwrap();
        // Borrowed from the closest known id; limits from the listing.
        assert_eq!(two.compat.thinking_format, Some(ThinkingFormat::Openrouter));
        assert_eq!((two.context_window, two.max_tokens), (32_000, 16_384));
        assert!(two.reasoning && two.thinking_level_map.is_none());

        // An empty list is treated as a failed listing, not "nothing available".
        let empty = UpstreamModels::new(Vec::new());
        assert_eq!(
            resolve_models(&provider, None, Some(&empty), &Reference::default()).len(),
            provider.models().len()
        );
    }

    #[test]
    fn gateway_models_borrow_capabilities_and_routes() {
        let provider = HttpProvider::new("gw", "Gateway", Vec::<String>::new(), Vec::new())
            .with_api_endpoint(Api::OpenAiCompletions, "https://gw/provider/v1");
        let mut claude = catalog_model("claude-x", Api::AnthropicMessages, "https://api");
        claude.reasoning = true;
        claude.compat.force_adaptive_thinking = Some(true);
        claude.max_tokens = 128_000;
        claude.cost.input = 5.0;
        let catalog = ModelsDevCatalog::parse(
            &serde_json::to_vec(&json!({"other": {"id": "other", "models": {
                "reasoner": {
                    "id": "reasoner", "reasoning": true, "tool_call": true,
                    "modalities": {"input": ["text", "image"], "output": ["text"]},
                    "reasoning_options": [{"type": "effort", "values": ["none", "low", "high"]}],
                    "limit": {"context": 200000, "output": 64000},
                    "cost": {"input": 2, "output": 8}
                }
            }}}))
            .unwrap(),
            |_| true,
        )
        .unwrap();
        let reference = Reference {
            catalog: Some(&catalog),
            builtin: vec![claude],
        };
        let listed = |id: &str, endpoints: &[&str]| {
            let mut entry = UpstreamModel::new(id);
            entry.context_window = Some(1_000_000);
            entry.endpoints = Some(endpoints.iter().map(|e| (*e).to_owned()).collect());
            entry
        };
        let upstream = UpstreamModels::new(vec![
            listed("claude-x", &["/messages"]),
            listed("vendor/Reasoner", &["/chat/completions", "/responses"]),
            listed("vendor/plain", &["/chat/completions"]),
        ]);
        let models = resolve_models(&provider, None, Some(&upstream), &reference);
        let find = |id: &str| models.iter().find(|m| m.id == id).unwrap();

        // Served on /messages only: Anthropic protocol at the gateway root, with the
        // built-in model's protocol settings.
        let claude = find("claude-x");
        assert_eq!(claude.api, Api::AnthropicMessages);
        assert_eq!(claude.base_url, "https://gw/provider");
        assert_eq!(claude.compat.force_adaptive_thinking, Some(true));
        assert!(claude.reasoning);
        assert_eq!(
            (
                claude.context_window,
                claude.max_tokens,
                claude.provider.as_str()
            ),
            (1_000_000, 128_000, "gw")
        );

        // Described by models.dev under another provider (vendor prefix and case ignored).
        let reasoner = find("vendor/Reasoner");
        assert_eq!(reasoner.api, Api::OpenAiCompletions);
        assert_eq!(reasoner.base_url, "https://gw/provider/v1");
        assert!(reasoner.reasoning && reasoner.supports_images());
        assert_eq!(
            reasoner.supported_thinking_levels(),
            [ThinkingLevel::Off, ThinkingLevel::Low, ThinkingLevel::High]
        );
        // `off` sends nothing rather than another provider's value.
        assert_eq!(reasoner.thinking_level_value(ThinkingLevel::Off), None);
        assert_eq!(reasoner.max_tokens, 64_000);
        // Unpriced listings borrow the same model's list price.
        assert_eq!((reasoner.cost.input, reasoner.cost.output), (2.0, 8.0));
        assert_eq!(claude.cost.input, 5.0);

        let plain = find("vendor/plain");
        assert!(!plain.reasoning);
        assert_eq!(plain.cost, ModelCost::default());
        assert_eq!(
            (plain.context_window, plain.max_tokens),
            (1_000_000, 16_384)
        );
    }

    #[test]
    fn dynamic_provider_without_catalog() {
        let provider = HttpProvider::new("local", "Local", Vec::<String>::new(), Vec::new())
            .with_api_endpoint(Api::OpenAiCompletions, "http://localhost:11434/v1")
            .without_api_key();
        let upstream = UpstreamModels::new(vec![UpstreamModel::new("qwen3:8b")]);
        let models = resolve_models(&provider, None, Some(&upstream), &Reference::default());
        assert_eq!(ids(&models), ["qwen3:8b"]);
        assert_eq!(models[0].base_url, "http://localhost:11434/v1");
        assert_eq!(models[0].provider, "local");
    }
}
