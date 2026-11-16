//! OpenRouter: PKCE sign-in that issues a regular, user-controlled API key.

use futures::future::BoxFuture;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::callback::CallbackServer;
use super::{
    Authorization, LoginEvent, LoginInteraction, OAuthFlow, base64url, callback_or_manual,
    parse_authorization_input, pkce, post_json, random_bytes, str_field, url_with_params,
};
use crate::auth::OAuthCredential;

const AUTHORIZE_URL: &str = "https://openrouter.ai/auth";
const TOKEN_URL: &str = "https://openrouter.ai/api/v1/auth/keys";

pub struct OpenRouterOAuth;

impl OAuthFlow for OpenRouterOAuth {
    fn name(&self) -> &str {
        "OpenRouter account"
    }

    fn login_label(&self) -> &str {
        "Sign in with OpenRouter"
    }

    fn login(
        &self,
        interaction: LoginInteraction,
    ) -> BoxFuture<'static, Result<OAuthCredential, String>> {
        Box::pin(login(interaction))
    }

    /// The issued key does not expire.
    fn refresh(
        &self,
        credential: OAuthCredential,
        _cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<OAuthCredential, String>> {
        Box::pin(async move { Ok(credential) })
    }
}

async fn login(interaction: LoginInteraction) -> Result<OAuthCredential, String> {
    let (verifier, challenge) = pkce();
    // OpenRouter sends no `state`; a random path keeps stray requests out.
    let path = format!("/oauth/callback/{}", base64url(&random_bytes(12)));
    let server = CallbackServer::bind(0, &path)
        .await
        .map_err(|error| format!("Could not listen for the OpenRouter callback: {error}"))?;
    let redirect_uri = server.redirect_uri("localhost");
    let url = url_with_params(
        AUTHORIZE_URL,
        &[
            ("callback_url", &redirect_uri),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
        ],
    );
    interaction.notify(LoginEvent::AuthUrl {
        url,
        instructions: "Complete the sign-in in your browser. If it cannot reach ZP, paste the final redirect URL.".into(),
    });
    let code = match callback_or_manual(&interaction, Some(server), None, "OpenRouter").await? {
        Authorization::Callback(params) => params.get("code").cloned(),
        Authorization::Manual(text) => parse_authorization_input(&text).0,
    };
    let code = code.ok_or("Missing authorization code")?;
    interaction.progress("Exchanging the authorization code for an API key…");
    let body = json!({
        "code": code,
        "code_verifier": verifier,
        "code_challenge_method": "S256",
    });
    let reply = post_json(TOKEN_URL, &body, interaction.cancel()).await?;
    if !reply.ok() {
        return Err(reply.failure("OpenRouter key exchange"));
    }
    let key = str_field(&reply.json(), "key")
        .ok_or("The OpenRouter response carries no key")?
        .to_owned();
    Ok(OAuthCredential {
        access: key,
        refresh: String::new(),
        expires: i64::MAX,
        extra: Default::default(),
    })
}
