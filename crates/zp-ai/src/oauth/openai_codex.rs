//! ChatGPT Plus/Pro through the Codex backend: authorization code with PKCE against
//! auth.openai.com.

use futures::future::BoxFuture;
use tokio_util::sync::CancellationToken;

use super::callback::CallbackServer;
use super::{
    Authorization, LoginEvent, LoginInteraction, OAuthFlow, callback_or_manual, jwt_claims,
    number_field, parse_authorization_input, pkce, post_form, random_bytes, str_field,
    url_with_params,
};
use crate::auth::{OAuthCredential, RequestAuth};

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
/// Shared with the Codex CLI; when taken, the pasted redirect URL is used.
const CALLBACK_PORT: u16 = 1455;
const CALLBACK_PATH: &str = "/auth/callback";
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const SCOPE: &str = "openid profile email offline_access";
const JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";
/// Sent as `originator` on authorization and requests.
pub(crate) const ORIGINATOR: &str = "zp";

pub struct OpenAiCodexOAuth;

impl OAuthFlow for OpenAiCodexOAuth {
    fn name(&self) -> &str {
        "ChatGPT Plus/Pro (Codex)"
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
            let params = [
                ("grant_type", "refresh_token"),
                ("refresh_token", credential.refresh.as_str()),
                ("client_id", CLIENT_ID),
            ];
            token_request(&params, &cancel, "OpenAI Codex token refresh").await
        })
    }

    fn request_auth(&self, credential: &OAuthCredential) -> RequestAuth {
        let mut auth = RequestAuth::api_key(credential.access.clone());
        if let Some(account) = credential
            .extra_str("accountId")
            .map(str::to_owned)
            .or_else(|| account_id(&credential.access))
        {
            auth.headers.insert("chatgpt-account-id".into(), account);
        }
        auth
    }
}

/// The ChatGPT account id in an access token.
pub(crate) fn account_id(token: &str) -> Option<String> {
    jwt_claims(token)?
        .get(JWT_CLAIM_PATH)?
        .get("chatgpt_account_id")?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

async fn login(interaction: LoginInteraction) -> Result<OAuthCredential, String> {
    let (verifier, challenge) = pkce();
    let state: String = random_bytes(16)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let originator = interaction
        .options()
        .agent_name
        .clone()
        .unwrap_or_else(|| ORIGINATOR.to_owned());
    let url = url_with_params(
        AUTHORIZE_URL,
        &[
            ("response_type", "code"),
            ("client_id", CLIENT_ID),
            ("redirect_uri", REDIRECT_URI),
            ("scope", SCOPE),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("state", &state),
            ("id_token_add_organizations", "true"),
            ("codex_cli_simplified_flow", "true"),
            ("originator", &originator),
        ],
    );
    let server = CallbackServer::bind(CALLBACK_PORT, CALLBACK_PATH)
        .await
        .ok();
    interaction.notify(LoginEvent::AuthUrl {
        url,
        instructions: "Complete the sign-in in your browser. If it cannot reach ZP, paste the final redirect URL.".into(),
    });
    let code = match callback_or_manual(&interaction, server, Some(&state), "OpenAI").await? {
        Authorization::Callback(params) => params.get("code").cloned(),
        Authorization::Manual(text) => {
            let (code, pasted_state) = parse_authorization_input(&text);
            if pasted_state.is_some_and(|s| s != state) {
                return Err("OAuth state mismatch".into());
            }
            code
        }
    };
    let code = code.ok_or("Missing authorization code")?;
    interaction.progress("Exchanging the authorization code for tokens…");
    let params = [
        ("grant_type", "authorization_code"),
        ("client_id", CLIENT_ID),
        ("code", code.as_str()),
        ("code_verifier", verifier.as_str()),
        ("redirect_uri", REDIRECT_URI),
    ];
    token_request(&params, interaction.cancel(), "OpenAI Codex token exchange").await
}

async fn token_request(
    params: &[(&str, &str)],
    cancel: &CancellationToken,
    what: &str,
) -> Result<OAuthCredential, String> {
    let reply = post_form(TOKEN_URL, params, cancel).await?;
    if !reply.ok() {
        return Err(reply.failure(what));
    }
    let json = reply.json();
    let (Some(access), Some(refresh), Some(expires_in)) = (
        str_field(&json, "access_token"),
        str_field(&json, "refresh_token"),
        number_field(&json, "expires_in"),
    ) else {
        return Err(format!("{what} returned an incomplete response"));
    };
    let account = account_id(access).ok_or("The token carries no ChatGPT account id")?;
    Ok(OAuthCredential::new(access, refresh, expires_in, 0).with_extra("accountId", account))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::base64url;

    #[test]
    fn the_account_id_comes_from_the_token() {
        let claims = format!(r#"{{"{JWT_CLAIM_PATH}":{{"chatgpt_account_id":"acc-1"}}}}"#);
        let token = format!("h.{}.s", base64url(claims.as_bytes()));
        assert_eq!(account_id(&token).as_deref(), Some("acc-1"));
        let credential = OAuthCredential::new(token, "r", 60.0, 0);
        let auth = OpenAiCodexOAuth.request_auth(&credential);
        assert_eq!(auth.headers["chatgpt-account-id"], "acc-1");
    }
}
