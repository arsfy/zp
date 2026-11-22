//! Claude Pro/Max: authorization code with PKCE against claude.ai.

use futures::future::BoxFuture;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::callback::CallbackServer;
use super::{
    Authorization, LoginEvent, LoginInteraction, OAuthFlow, callback_or_manual, number_field,
    parse_authorization_input, pkce, post_json, str_field, url_with_params,
};
use crate::auth::OAuthCredential;

const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
/// Preferred so the port can be forwarded; any loopback port is accepted.
const CALLBACK_PORT: u16 = 53692;
const CALLBACK_PATH: &str = "/callback";
const SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
/// Tokens are refreshed this long before they expire.
const EXPIRY_MARGIN_MS: i64 = 5 * 60 * 1000;

pub struct AnthropicOAuth;

impl OAuthFlow for AnthropicOAuth {
    fn name(&self) -> &str {
        "Claude Pro/Max"
    }

    fn login_label(&self) -> &str {
        "Sign in with Claude"
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
            let body = json!({
                "grant_type": "refresh_token",
                "client_id": CLIENT_ID,
                "refresh_token": credential.refresh,
            });
            token_request(&body, &cancel, "Anthropic token refresh").await
        })
    }
}

async fn login(interaction: LoginInteraction) -> Result<OAuthCredential, String> {
    let (verifier, challenge) = pkce();
    let server = match CallbackServer::bind(CALLBACK_PORT, CALLBACK_PATH).await {
        Ok(server) => Some(server),
        Err(_) => CallbackServer::bind(0, CALLBACK_PATH).await.ok(),
    };
    let redirect_uri = server.as_ref().map_or_else(
        || format!("http://localhost:{CALLBACK_PORT}{CALLBACK_PATH}"),
        |server| server.redirect_uri("localhost"),
    );
    let url = url_with_params(
        AUTHORIZE_URL,
        &[
            ("code", "true"),
            ("client_id", CLIENT_ID),
            ("response_type", "code"),
            ("redirect_uri", &redirect_uri),
            ("scope", SCOPES),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("state", &verifier),
        ],
    );
    interaction.notify(LoginEvent::AuthUrl {
        url,
        instructions: "Complete the sign-in in your browser. If it cannot reach ZP, paste the final redirect URL or the code shown.".into(),
    });
    let (code, state) =
        match callback_or_manual(&interaction, server, Some(&verifier), "Claude").await? {
            Authorization::Callback(params) => (params.get("code").cloned(), None),
            Authorization::Manual(text) => {
                let (code, state) = parse_authorization_input(&text);
                if state.as_deref().is_some_and(|s| s != verifier) {
                    return Err("OAuth state mismatch".into());
                }
                (code, state)
            }
        };
    let code = code.ok_or("Missing authorization code")?;
    interaction.progress("Exchanging the authorization code for tokens…");
    let body = json!({
        "grant_type": "authorization_code",
        "client_id": CLIENT_ID,
        "code": code,
        "state": state.unwrap_or_else(|| verifier.clone()),
        "redirect_uri": redirect_uri,
        "code_verifier": verifier,
    });
    token_request(&body, interaction.cancel(), "Anthropic token exchange").await
}

async fn token_request(
    body: &Value,
    cancel: &CancellationToken,
    what: &str,
) -> Result<OAuthCredential, String> {
    let reply = post_json(TOKEN_URL, body, cancel).await?;
    if !reply.ok() {
        return Err(reply.failure(what));
    }
    let json = reply.json();
    let (Some(access), Some(refresh), Some(expires_in)) = (
        str_field(&json, "access_token"),
        str_field(&json, "refresh_token"),
        // A zero lifetime would refresh before every request, like Kimi and ChatGPT reject it.
        number_field(&json, "expires_in").filter(|s| *s > 0.0),
    ) else {
        return Err(format!("{what} returned an incomplete response"));
    };
    Ok(OAuthCredential::new(
        access,
        refresh,
        expires_in,
        EXPIRY_MARGIN_MS,
    ))
}
