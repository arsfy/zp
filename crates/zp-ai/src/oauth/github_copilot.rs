//! GitHub Copilot: GitHub device flow, then short-lived Copilot tokens exchanged with the
//! GitHub token (kept as the refresh token).

use futures::future::BoxFuture;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::device::{self, Poll};
use super::{
    Body, LoginEvent, LoginInteraction, OAuthFlow, Reply, number_field, post_form, send, str_field,
};
use crate::auth::{OAuthCredential, RequestAuth};
use crate::model::Model;

const CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
const DOMAIN: &str = "github.com";
const DEFAULT_BASE_URL: &str = "https://api.individual.githubcopilot.com";
const API_VERSION: &str = "2026-06-01";
/// Headers Copilot expects from its clients (also on every model of the catalog).
pub(crate) const COPILOT_HEADERS: [(&str, &str); 4] = [
    ("User-Agent", "GitHubCopilotChat/0.35.0"),
    ("Editor-Version", "vscode/1.107.0"),
    ("Editor-Plugin-Version", "copilot-chat/0.35.0"),
    ("Copilot-Integration-Id", "vscode-chat"),
];
const EXPIRY_MARGIN_MS: i64 = 5 * 60 * 1000;

pub struct GitHubCopilotOAuth;

impl OAuthFlow for GitHubCopilotOAuth {
    fn name(&self) -> &str {
        "GitHub Copilot"
    }

    fn login_label(&self) -> &str {
        "Sign in with GitHub"
    }

    fn login(
        &self,
        interaction: LoginInteraction,
    ) -> BoxFuture<'static, Result<OAuthCredential, String>> {
        Box::pin(login(interaction))
    }

    fn refresh(
        &self,
        credential: OAuthCredential,
        cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<OAuthCredential, String>> {
        Box::pin(async move {
            let mut refreshed = copilot_token(&credential.refresh, &cancel).await?;
            // Keep the previous list when the model list cannot be read.
            match available_models(&refreshed.access, &cancel).await {
                Ok(models) => {
                    refreshed = refreshed.with_extra("availableModelIds", models.available);
                }
                Err(_) => {
                    if let Some(previous) = credential.extra.get("availableModelIds") {
                        refreshed = refreshed.with_extra("availableModelIds", previous.clone());
                    }
                }
            }
            Ok(refreshed)
        })
    }

    fn request_auth(&self, credential: &OAuthCredential) -> RequestAuth {
        RequestAuth {
            api_key: Some(credential.access.clone()),
            base_url: Some(base_url(&credential.access)),
            ..RequestAuth::default()
        }
    }

    fn filter_models(&self, credential: &OAuthCredential, models: Vec<Model>) -> Vec<Model> {
        let Some(available) = credential
            .extra
            .get("availableModelIds")
            .and_then(Value::as_array)
            .filter(|ids| !ids.is_empty())
        else {
            return models;
        };
        models
            .into_iter()
            .filter(|model| available.iter().any(|id| id.as_str() == Some(&model.id)))
            .collect()
    }
}

/// The API endpoint named in a Copilot token (`…;proxy-ep=proxy.host;…` → `https://api.host`).
pub(crate) fn base_url(token: &str) -> String {
    token
        .split(';')
        .find_map(|part| part.strip_prefix("proxy-ep="))
        .map(|host| format!("https://{}", host.replacen("proxy.", "api.", 1)))
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_owned())
}

async fn login(interaction: LoginInteraction) -> Result<OAuthCredential, String> {
    let reply = post_form(
        &format!("https://{DOMAIN}/login/device/code"),
        &[("client_id", CLIENT_ID), ("scope", "read:user")],
        interaction.cancel(),
    )
    .await?;
    if !reply.ok() {
        return Err(reply.failure("GitHub device authorization"));
    }
    let json = reply.json();
    let (Some(device_code), Some(user_code), Some(verification_uri)) = (
        str_field(&json, "device_code"),
        str_field(&json, "user_code"),
        str_field(&json, "verification_uri").filter(|u| u.starts_with("https://")),
    ) else {
        return Err("GitHub returned an invalid device code response".into());
    };
    let device_code = device_code.to_owned();
    interaction.notify(LoginEvent::DeviceCode {
        user_code: user_code.to_owned(),
        verification_uri: verification_uri.to_owned(),
    });
    let token_url = format!("https://{DOMAIN}/login/oauth/access_token");
    let github_token = device::poll(
        &interaction,
        number_field(&json, "interval"),
        number_field(&json, "expires_in"),
        true,
        || {
            let token_url = token_url.clone();
            let device_code = device_code.clone();
            let cancel = interaction.cancel().clone();
            async move {
                let reply = post_form(
                    &token_url,
                    &[
                        ("client_id", CLIENT_ID),
                        ("device_code", &device_code),
                        ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ],
                    &cancel,
                )
                .await?;
                // GitHub answers pending states with HTTP 200 and an `error` field.
                let json = reply.json();
                Ok(match str_field(&json, "access_token") {
                    Some(token) => Poll::Complete(token.to_owned()),
                    None => match device::standard_poll_result(&reply, "GitHub") {
                        Poll::Complete(_) => Poll::Failed("Invalid GitHub token response".into()),
                        Poll::Pending => Poll::Pending,
                        Poll::SlowDown(s) => Poll::SlowDown(s),
                        Poll::Failed(message) => Poll::Failed(message),
                    },
                })
            }
        },
    )
    .await?;
    interaction.progress("Getting a Copilot token…");
    let credential = copilot_token(&github_token, interaction.cancel()).await?;
    let models = available_models(&credential.access, interaction.cancel())
        .await
        .unwrap_or_default();
    let mut available = models.available;
    if !models.needs_policy.is_empty() {
        interaction.progress("Enabling models…");
        for id in models.needs_policy {
            if enable_model(&credential.access, &id, interaction.cancel()).await {
                available.push(id);
            }
        }
    }
    available.sort();
    available.dedup();
    Ok(credential.with_extra("availableModelIds", available))
}

fn copilot_headers(token: &str) -> Vec<(&'static str, String)> {
    let mut headers: Vec<(&'static str, String)> = COPILOT_HEADERS
        .iter()
        .map(|(k, v)| (*k, (*v).to_owned()))
        .collect();
    headers.push(("Authorization", format!("Bearer {token}")));
    headers
}

async fn get_with(
    url: &str,
    headers: &[(&str, String)],
    cancel: &CancellationToken,
) -> Result<Reply, String> {
    let headers: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    send(reqwest::Method::GET, url, &headers, Body::None, cancel).await
}

/// Exchanges a GitHub token for a Copilot API token.
async fn copilot_token(
    github_token: &str,
    cancel: &CancellationToken,
) -> Result<OAuthCredential, String> {
    let reply = get_with(
        &format!("https://api.{DOMAIN}/copilot_internal/v2/token"),
        &copilot_headers(github_token),
        cancel,
    )
    .await?;
    if !reply.ok() {
        return Err(reply.failure("Copilot token request"));
    }
    let json = reply.json();
    let (Some(token), Some(expires_at)) = (
        str_field(&json, "token"),
        json.get("expires_at").and_then(Value::as_f64),
    ) else {
        return Err("Invalid Copilot token response".into());
    };
    Ok(OAuthCredential {
        access: token.to_owned(),
        refresh: github_token.to_owned(),
        expires: (expires_at * 1000.0) as i64 - EXPIRY_MARGIN_MS,
        extra: Default::default(),
    })
}

#[derive(Default)]
struct AccountModels {
    /// Usable now.
    available: Vec<String>,
    /// Listed, but their policy must be accepted first.
    needs_policy: Vec<String>,
}

async fn available_models(
    token: &str,
    cancel: &CancellationToken,
) -> Result<AccountModels, String> {
    let mut headers = copilot_headers(token);
    headers.push(("X-GitHub-Api-Version", API_VERSION.to_owned()));
    let reply = get_with(&format!("{}/models", base_url(token)), &headers, cancel).await?;
    if !reply.ok() {
        return Err(reply.failure("Copilot model list"));
    }
    Ok(parse_models(&reply.json()))
}

fn parse_models(json: &Value) -> AccountModels {
    let mut models = AccountModels::default();
    for item in json
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(id) = str_field(item, "id") else {
            continue;
        };
        let supports_tools = item
            .pointer("/capabilities/supports/tool_calls")
            .and_then(Value::as_bool)
            != Some(false);
        if !supports_tools {
            continue;
        }
        let picker = item.get("model_picker_enabled").and_then(Value::as_bool) == Some(true);
        let policy = item.pointer("/policy/state").and_then(Value::as_str);
        if picker && policy != Some("disabled") {
            models.available.push(id.to_owned());
            if policy == Some("unconfigured") {
                models.needs_policy.push(id.to_owned());
            }
        }
    }
    models
}

/// Accepts the policy of model `id` (needed once for some models). Best effort.
async fn enable_model(token: &str, id: &str, cancel: &CancellationToken) -> bool {
    let mut headers = copilot_headers(token);
    headers.push(("openai-intent", "chat-policy".to_owned()));
    headers.push(("x-interaction-type", "chat-policy".to_owned()));
    let headers: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let body = json!({"state": "enabled"});
    send(
        reqwest::Method::POST,
        &format!("{}/models/{id}/policy", base_url(token)),
        &headers,
        Body::Json(&body),
        cancel,
    )
    .await
    .is_ok_and(|reply| reply.ok())
}

/// Request headers Copilot wants per request: who initiated it and whether it has images.
pub(crate) fn dynamic_headers(
    context: &crate::types::Context,
) -> Vec<(&'static str, &'static str)> {
    use crate::types::{Message, UserContent};
    let initiator = match context.messages.last() {
        Some(Message::User(_)) | None => "user",
        Some(_) => "agent",
    };
    let has_images = context.messages.iter().any(|message| match message {
        Message::User(user) => user
            .content
            .iter()
            .any(|c| matches!(c, UserContent::Image(_))),
        Message::ToolResult(result) => result
            .content
            .iter()
            .any(|c| matches!(c, UserContent::Image(_))),
        Message::Assistant(_) => false,
    });
    let mut headers = vec![
        ("X-Initiator", initiator),
        ("Openai-Intent", "conversation-edits"),
    ];
    if has_images {
        headers.push(("Copilot-Vision-Request", "true"));
    }
    headers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_endpoint_comes_from_the_token() {
        assert_eq!(
            base_url("tid=1;exp=2;proxy-ep=proxy.business.githubcopilot.com;st=x"),
            "https://api.business.githubcopilot.com"
        );
        assert_eq!(base_url("opaque"), DEFAULT_BASE_URL);
    }

    #[test]
    fn model_list_and_filter() {
        let json = json!({"data": [
            {"id": "gpt-5", "model_picker_enabled": true, "policy": {"state": "enabled"}},
            {"id": "claude", "model_picker_enabled": true, "policy": {"state": "unconfigured"}},
            {"id": "hidden", "model_picker_enabled": false},
            {"id": "no-tools", "model_picker_enabled": true,
             "capabilities": {"supports": {"tool_calls": false}}},
        ]});
        let models = parse_models(&json);
        assert_eq!(models.available, ["gpt-5", "claude"]);
        assert_eq!(models.needs_policy, ["claude"]);

        let credential =
            OAuthCredential::new("t", "g", 60.0, 0).with_extra("availableModelIds", vec!["gpt-5"]);
        let model = |id: &str| {
            let mut model: Model = serde_json::from_value(json!({
                "id": id, "name": id, "api": "openai-completions", "provider": "github-copilot",
                "baseUrl": DEFAULT_BASE_URL,
            }))
            .unwrap();
            model.id = id.to_owned();
            model
        };
        let kept = GitHubCopilotOAuth.filter_models(&credential, vec![model("gpt-5"), model("x")]);
        assert_eq!(kept.len(), 1);
    }
}
