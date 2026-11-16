//! xAI (SuperGrok or X Premium): device authorization against auth.x.ai.

use futures::future::BoxFuture;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::device;
use super::{LoginEvent, LoginInteraction, OAuthFlow, number_field, post_form, str_field};
use crate::auth::OAuthCredential;

const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
const DEVICE_CODE_URL: &str = "https://auth.x.ai/oauth2/device/code";
const TOKEN_URL: &str = "https://auth.x.ai/oauth2/token";
const EXPIRY_MARGIN_MS: i64 = 5 * 60 * 1000;
const DEFAULT_LIFETIME_SECONDS: f64 = 3600.0;

pub struct XaiOAuth;

impl OAuthFlow for XaiOAuth {
    fn name(&self) -> &str {
        "Grok / X subscription"
    }

    fn login_label(&self) -> &str {
        "Sign in with SuperGrok or X Premium"
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
            let reply = post_form(
                TOKEN_URL,
                &[
                    ("grant_type", "refresh_token"),
                    ("client_id", CLIENT_ID),
                    ("refresh_token", credential.refresh.as_str()),
                ],
                &cancel,
            )
            .await?;
            if !reply.ok() {
                return Err(reply.failure("xAI token refresh"));
            }
            credential_from(&reply.json(), Some(&credential.refresh))
        })
    }
}

fn credential_from(
    json: &Value,
    previous_refresh: Option<&str>,
) -> Result<OAuthCredential, String> {
    let access =
        str_field(json, "access_token").ok_or("The xAI token response has no access token")?;
    // xAI may omit the refresh token when it is not rotated.
    let refresh = str_field(json, "refresh_token")
        .or(previous_refresh)
        .ok_or("The xAI token response has no refresh token")?;
    let expires_in = number_field(json, "expires_in")
        .filter(|s| *s > 0.0)
        .unwrap_or(DEFAULT_LIFETIME_SECONDS);
    Ok(OAuthCredential::new(
        access,
        refresh,
        expires_in,
        EXPIRY_MARGIN_MS,
    ))
}

async fn login(interaction: LoginInteraction) -> Result<OAuthCredential, String> {
    let reply = post_form(
        DEVICE_CODE_URL,
        &[
            ("client_id", CLIENT_ID),
            ("scope", SCOPE),
            ("referrer", "zp"),
        ],
        interaction.cancel(),
    )
    .await?;
    if !reply.ok() {
        return Err(reply.failure("xAI device authorization"));
    }
    let json = reply.json();
    let (Some(device_code), Some(user_code)) = (
        str_field(&json, "device_code"),
        str_field(&json, "user_code"),
    ) else {
        return Err("xAI returned an invalid device authorization".into());
    };
    let verification = str_field(&json, "verification_uri_complete")
        .or_else(|| str_field(&json, "verification_uri"))
        .filter(|u| u.starts_with("https://"))
        .ok_or("xAI returned an untrusted verification URL")?;
    interaction.notify(LoginEvent::DeviceCode {
        user_code: user_code.to_owned(),
        verification_uri: verification.to_owned(),
    });
    let device_code = device_code.to_owned();
    let tokens = device::poll(
        &interaction,
        number_field(&json, "interval").filter(|s| *s > 0.0),
        number_field(&json, "expires_in"),
        true,
        || {
            let device_code = device_code.clone();
            let cancel = interaction.cancel().clone();
            async move {
                let reply = post_form(
                    TOKEN_URL,
                    &[
                        ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                        ("client_id", CLIENT_ID),
                        ("device_code", &device_code),
                    ],
                    &cancel,
                )
                .await?;
                Ok(device::standard_poll_result(&reply, "xAI"))
            }
        },
    )
    .await?;
    credential_from(&tokens, None)
}
