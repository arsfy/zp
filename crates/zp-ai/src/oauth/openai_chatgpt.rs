//! Sign in with ChatGPT for the OpenAI API: a dynamically registered public client whose
//! access token is sent directly to api.openai.com (Responses API only).

use futures::future::BoxFuture;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::callback::CallbackServer;
use super::{
    Authorization, LoginEvent, LoginInteraction, OAuthFlow, base64url, callback_or_manual,
    number_field, pkce, post_form, random_bytes, str_field, url_with_params,
};
use crate::auth::OAuthCredential;

/// Every login registers a new client; the issued id comes back with the callback.
const DYNAMIC_CLIENT_ID: &str = "dynamic_agent_client";
const AUTHORIZE_URL: &str = "https://auth.openai.com/api/accounts/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/api/accounts/oauth/token";
const RESOURCE: &str = "https://api.openai.com/v1";
const CALLBACK_PORT: u16 = 1455;
const CALLBACK_PATH: &str = "/auth/callback";
const REDIRECT_URI: &str = "http://127.0.0.1:1455/auth/callback";
const DIRECT_TOKEN_SCOPE: &str = "chatgpt.tokens.use.direct";
const SCOPE: &str = "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
const EXPIRY_MARGIN_MS: i64 = 3 * 60 * 1000;

pub struct OpenAiChatGptOAuth;

impl OAuthFlow for OpenAiChatGptOAuth {
    fn name(&self) -> &str {
        "ChatGPT subscription"
    }

    fn login_label(&self) -> &str {
        "Sign in with ChatGPT"
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
            let client_id = credential
                .extra_str("clientId")
                .ok_or("The stored ChatGPT sign-in has no client id; sign in again")?
                .to_owned();
            let params = [
                ("grant_type", "refresh_token"),
                ("client_id", client_id.as_str()),
                ("refresh_token", credential.refresh.as_str()),
                ("resource", RESOURCE),
            ];
            let json = token_request(&params, &cancel).await?;
            credential_from(&json, &client_id)
        })
    }
}

fn is_uuid(text: &str) -> bool {
    uuid::Uuid::parse_str(text).is_ok() && text.len() == 36
}

async fn login(interaction: LoginInteraction) -> Result<OAuthCredential, String> {
    let device_id = interaction
        .options()
        .device_id
        .clone()
        .filter(|id| is_uuid(id))
        .ok_or("Sign in with ChatGPT needs a device id (UUID) for this installation")?;
    let host_id = format!("urn:uuid:{}", device_id.to_lowercase());
    let agent_name = interaction
        .options()
        .agent_name
        .clone()
        .unwrap_or_else(|| "ZP".to_owned());
    let (verifier, challenge) = pkce();
    let state = base64url(&random_bytes(32));
    let nonce = base64url(&random_bytes(32));
    let server = CallbackServer::bind(CALLBACK_PORT, CALLBACK_PATH)
        .await
        .map_err(|_| {
            format!(
                "Port {CALLBACK_PORT} is in use, probably by another sign-in or the Codex CLI. Finish or cancel it and try again."
            )
        })?;
    let url = url_with_params(
        AUTHORIZE_URL,
        &[
            ("client_id", DYNAMIC_CLIENT_ID),
            ("agent_name_hint", &agent_name),
            ("ext_agent_host_id", &host_id),
            ("response_type", "code"),
            ("redirect_uri", REDIRECT_URI),
            ("resource", RESOURCE),
            ("scope", SCOPE),
            ("state", &state),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("nonce", &nonce),
        ],
    );
    interaction.notify(LoginEvent::AuthUrl {
        url,
        instructions: "Complete the sign-in in your browser. If it cannot reach ZP, paste the final redirect URL.".into(),
    });
    let params =
        match callback_or_manual(&interaction, Some(server), Some(&state), "ChatGPT").await? {
            Authorization::Callback(params) => params,
            Authorization::Manual(text) => {
                let url = reqwest::Url::parse(text.trim())
                    .map_err(|_| "Paste the full callback URL from the browser".to_owned())?;
                if !text.trim().starts_with(REDIRECT_URI) {
                    return Err(format!("The pasted URL must start with {REDIRECT_URI}"));
                }
                let params: std::collections::HashMap<String, String> = url
                    .query_pairs()
                    .map(|(k, v)| (k.into_owned(), v.into_owned()))
                    .collect();
                if let Some(error) = params.get("error") {
                    return Err(format!("ChatGPT authorization failed: {error}"));
                }
                if params.get("state") != Some(&state) {
                    return Err("OAuth state mismatch".into());
                }
                params
            }
        };
    let code = params.get("code").ok_or("Missing authorization code")?;
    let client_id = params
        .get("client_id")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .ok_or("The ChatGPT callback did not contain an issued client id")?
        .to_owned();
    interaction.progress("Exchanging the authorization code for tokens…");
    let exchange = [
        ("grant_type", "authorization_code"),
        ("client_id", client_id.as_str()),
        ("code", code.as_str()),
        ("code_verifier", verifier.as_str()),
        ("redirect_uri", REDIRECT_URI),
        ("resource", RESOURCE),
    ];
    let json = token_request(&exchange, interaction.cancel()).await?;
    if str_field(&json, "id_token").is_none() {
        return Err("The ChatGPT token response did not contain an ID token".into());
    }
    credential_from(&json, &client_id)
}

async fn token_request(
    params: &[(&str, &str)],
    cancel: &CancellationToken,
) -> Result<Value, String> {
    let reply = post_form(TOKEN_URL, params, cancel).await?;
    if !reply.ok() {
        return Err(reply.failure("ChatGPT token request"));
    }
    Ok(reply.json())
}

fn credential_from(json: &Value, client_id: &str) -> Result<OAuthCredential, String> {
    let (Some(access), Some(refresh), Some(scope), Some(expires_in)) = (
        str_field(json, "access_token"),
        str_field(json, "refresh_token"),
        str_field(json, "scope"),
        number_field(json, "expires_in").filter(|s| *s > 0.0),
    ) else {
        return Err("The ChatGPT token response is incomplete".into());
    };
    let scopes: Vec<&str> = scope.split_whitespace().collect();
    if !scopes.contains(&DIRECT_TOKEN_SCOPE) {
        return Err(format!(
            "The ChatGPT grant did not include {DIRECT_TOKEN_SCOPE}"
        ));
    }
    Ok(
        OAuthCredential::new(access, refresh, expires_in, EXPIRY_MARGIN_MS)
            .with_extra("clientId", client_id)
            .with_extra("scopes", scopes),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn credentials_need_the_direct_token_scope() {
        let mut json = json!({
            "access_token": "a", "refresh_token": "r", "expires_in": 3600,
            "scope": "openid chatgpt.tokens.use.direct",
        });
        let credential = credential_from(&json, "client-1").unwrap();
        assert_eq!(credential.extra_str("clientId"), Some("client-1"));
        json["scope"] = "openid".into();
        assert!(credential_from(&json, "client-1").is_err());
        assert!(is_uuid("6f1c7a4e-2d3b-4c5d-8e9f-0a1b2c3d4e5f"));
        assert!(!is_uuid("nope"));
    }
}
