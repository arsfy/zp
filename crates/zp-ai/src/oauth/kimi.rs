//! Kimi Code subscription: device authorization against auth.kimi.com.

use futures::future::BoxFuture;
use tokio_util::sync::CancellationToken;

use super::device;
use super::{LoginEvent, LoginInteraction, OAuthFlow, number_field, post_form, str_field};
use crate::auth::OAuthCredential;

const CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
const DEFAULT_HOST: &str = "https://auth.kimi.com";
const DEVICE_CODE_TIMEOUT_SECONDS: f64 = 15.0 * 60.0;
const REFRESH_RETRIES: u32 = 3;

pub struct KimiCodingOAuth;

fn oauth_host() -> String {
    ["KIMI_CODE_OAUTH_HOST", "KIMI_OAUTH_HOST"]
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|v| !v.trim().is_empty()))
        .unwrap_or_else(|| DEFAULT_HOST.to_owned())
        .trim_end_matches('/')
        .to_owned()
}

impl OAuthFlow for KimiCodingOAuth {
    fn name(&self) -> &str {
        "Kimi Code subscription"
    }

    fn login_label(&self) -> &str {
        "Sign in with Kimi Code"
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
            let url = format!("{}/api/oauth/token", oauth_host());
            let params = [
                ("client_id", CLIENT_ID),
                ("grant_type", "refresh_token"),
                ("refresh_token", credential.refresh.as_str()),
            ];
            let mut attempt = 0;
            loop {
                let reply = post_form(&url, &params, &cancel).await;
                let retry = match &reply {
                    Ok(reply) => reply.status == 429 || reply.status >= 500,
                    Err(_) => !cancel.is_cancelled(),
                };
                if retry && attempt < REFRESH_RETRIES {
                    attempt += 1;
                    tokio::time::sleep(std::time::Duration::from_secs(1 << (attempt - 1))).await;
                    continue;
                }
                let reply = reply?;
                if !reply.ok() {
                    return Err(reply.failure("Kimi Code token refresh"));
                }
                return credential_from(&reply.json());
            }
        })
    }
}

fn credential_from(json: &serde_json::Value) -> Result<OAuthCredential, String> {
    let (Some(access), Some(refresh), Some(expires_in)) = (
        str_field(json, "access_token"),
        str_field(json, "refresh_token"),
        number_field(json, "expires_in").filter(|s| *s > 0.0),
    ) else {
        return Err("The Kimi Code token response is incomplete".into());
    };
    Ok(OAuthCredential::new(access, refresh, expires_in, 0))
}

async fn login(interaction: LoginInteraction) -> Result<OAuthCredential, String> {
    let host = oauth_host();
    let reply = post_form(
        &format!("{host}/api/oauth/device_authorization"),
        &[("client_id", CLIENT_ID)],
        interaction.cancel(),
    )
    .await?;
    if !reply.ok() {
        return Err(reply.failure("Kimi Code device authorization"));
    }
    let json = reply.json();
    let (Some(device_code), Some(user_code)) = (
        str_field(&json, "device_code"),
        str_field(&json, "user_code"),
    ) else {
        return Err("Kimi Code returned an invalid device authorization".into());
    };
    let verification = str_field(&json, "verification_uri_complete")
        .or_else(|| str_field(&json, "verification_uri"))
        .filter(|u| u.starts_with("https://") || u.starts_with("http://"))
        .ok_or("Kimi Code returned no verification URL")?;
    interaction.notify(LoginEvent::DeviceCode {
        user_code: user_code.to_owned(),
        verification_uri: verification.to_owned(),
    });
    let device_code = device_code.to_owned();
    let token_url = format!("{host}/api/oauth/token");
    let tokens = device::poll(
        &interaction,
        number_field(&json, "interval").filter(|s| *s > 0.0),
        Some(number_field(&json, "expires_in").unwrap_or(DEVICE_CODE_TIMEOUT_SECONDS)),
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
                Ok(device::standard_poll_result(&reply, "Kimi Code"))
            }
        },
    )
    .await?;
    credential_from(&tokens)
}
