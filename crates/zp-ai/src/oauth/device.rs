//! Polling for the result of an RFC 8628 device authorization.

use std::future::Future;
use std::time::{Duration, Instant};

use super::{LOGIN_CANCELLED, LoginInteraction};

const MINIMUM_INTERVAL: Duration = Duration::from_secs(1);
/// RFC 8628 §3.2: without an `interval`, poll every 5 seconds.
const DEFAULT_INTERVAL_SECONDS: f64 = 5.0;
/// RFC 8628 §3.5: `slow_down` adds 5 seconds.
const SLOW_DOWN_INCREMENT: Duration = Duration::from_secs(5);
/// Longest wait between polls honored; a server asking for more gets this.
const MAXIMUM_INTERVAL: Duration = Duration::from_secs(120);
/// Longest device code lifetime honored (codes usually last 15 minutes).
const MAXIMUM_LIFETIME: Duration = Duration::from_secs(60 * 60);

/// A number of seconds the server sent, kept within `min..=max`: an absurd value (say
/// `1e300`) must neither panic nor make ZP wait for ages.
fn server_duration(seconds: f64, min: Duration, max: Duration) -> Duration {
    Duration::try_from_secs_f64(seconds).map_or(max, |duration| duration.clamp(min, max))
}

/// The outcome of one poll.
pub(crate) enum Poll<T> {
    /// The user has not finished yet.
    Pending,
    /// Poll less often (optionally at the given interval, in seconds).
    SlowDown(Option<f64>),
    Complete(T),
    Failed(String),
}

/// Polls until the authorization completes, fails, expires or is cancelled.
pub(crate) async fn poll<T, F, Fut>(
    interaction: &LoginInteraction,
    interval_seconds: Option<f64>,
    expires_in_seconds: Option<f64>,
    wait_before_first_poll: bool,
    mut poll: F,
) -> Result<T, String>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Poll<T>, String>>,
{
    let deadline = expires_in_seconds
        .map(|s| Instant::now() + server_duration(s, Duration::ZERO, MAXIMUM_LIFETIME));
    let mut interval = server_duration(
        interval_seconds.unwrap_or(DEFAULT_INTERVAL_SECONDS),
        MINIMUM_INTERVAL,
        MAXIMUM_INTERVAL,
    );
    let mut slowed = false;
    let sleep = |duration: Duration| async move {
        tokio::select! {
            () = interaction.cancel().cancelled() => Err(LOGIN_CANCELLED.to_owned()),
            () = tokio::time::sleep(duration) => Ok(()),
        }
    };
    if wait_before_first_poll {
        sleep(interval).await?;
    }
    loop {
        interaction.check()?;
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(if slowed {
                "Device authorization timed out after slow_down responses; check the system clock and try again".into()
            } else {
                "Device authorization timed out".into()
            });
        }
        match poll().await? {
            Poll::Complete(value) => return Ok(value),
            Poll::Failed(message) => return Err(message),
            Poll::SlowDown(seconds) => {
                slowed = true;
                interval = match seconds.filter(|s| s.is_finite() && *s > 0.0) {
                    Some(seconds) => server_duration(seconds, MINIMUM_INTERVAL, MAXIMUM_INTERVAL),
                    None => (interval + SLOW_DOWN_INCREMENT).min(MAXIMUM_INTERVAL),
                };
            }
            Poll::Pending => {}
        }
        let wait = match deadline {
            Some(deadline) => interval.min(deadline.saturating_duration_since(Instant::now())),
            None => interval,
        };
        sleep(wait).await?;
    }
}

/// The standard token-endpoint answer while polling a device code: tokens (as the reply
/// JSON), or pending / slow down / failure.
pub(crate) fn standard_poll_result(
    reply: &super::Reply,
    provider: &str,
) -> Poll<serde_json::Value> {
    let json = reply.json();
    if reply.ok() && super::str_field(&json, "access_token").is_some() {
        return Poll::Complete(json);
    }
    match super::str_field(&json, "error") {
        Some("authorization_pending") => Poll::Pending,
        Some("slow_down") => Poll::SlowDown(super::number_field(&json, "interval")),
        Some("expired_token") => Poll::Failed(format!(
            "The {provider} device code expired; start the login again"
        )),
        Some("access_denied" | "authorization_denied") => {
            Poll::Failed(format!("{provider} login was denied"))
        }
        _ => Poll::Failed(reply.failure(&format!("{provider} device authorization"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::LoginOptions;
    use tokio_util::sync::CancellationToken;

    #[tokio::test(start_paused = true)]
    async fn polls_until_complete_and_slows_down() {
        let (interaction, _input) =
            LoginInteraction::new(CancellationToken::new(), |_| {}, LoginOptions::default());
        let mut calls = 0;
        let started = tokio::time::Instant::now();
        let value = poll(&interaction, Some(1.0), Some(600.0), false, || {
            calls += 1;
            let result = match calls {
                1 => Poll::Pending,
                2 => Poll::SlowDown(None),
                _ => Poll::Complete(calls),
            };
            async move { Ok(result) }
        })
        .await
        .unwrap();
        assert_eq!(value, 3);
        // 1 s after the first poll, then 6 s after slow_down.
        assert!(started.elapsed() >= Duration::from_secs(7));
    }

    /// A broken or hostile authorization server sends absurd numbers: polling must neither
    /// panic nor wait for ages.
    #[tokio::test(start_paused = true)]
    async fn absurd_server_numbers_are_kept_in_range() {
        let (interaction, _input) =
            LoginInteraction::new(CancellationToken::new(), |_| {}, LoginOptions::default());
        let mut calls = 0;
        let started = tokio::time::Instant::now();
        let value = poll(&interaction, Some(1e300), Some(1e300), false, || {
            calls += 1;
            let result = match calls {
                1 => Poll::SlowDown(Some(1e300)),
                2 => Poll::SlowDown(None),
                _ => Poll::Complete(calls),
            };
            async move { Ok(result) }
        })
        .await
        .unwrap();
        assert_eq!(value, 3);
        assert!(started.elapsed() <= MAXIMUM_INTERVAL * 2);

        // Lifetimes are capped the same way (the deadline uses the real clock, so this is
        // checked on the conversion).
        let lifetime = |seconds| server_duration(seconds, Duration::ZERO, MAXIMUM_LIFETIME);
        assert_eq!(lifetime(1e300), MAXIMUM_LIFETIME);
        assert_eq!(lifetime(f64::MAX), MAXIMUM_LIFETIME);
        assert_eq!(lifetime(900.0), Duration::from_secs(900));
        assert_eq!(
            server_duration(0.2, MINIMUM_INTERVAL, MAXIMUM_INTERVAL),
            MINIMUM_INTERVAL
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_stops_polling() {
        let cancel = CancellationToken::new();
        let (interaction, _input) =
            LoginInteraction::new(cancel.clone(), |_| {}, LoginOptions::default());
        cancel.cancel();
        let result: Result<(), String> = poll(&interaction, None, None, true, || async {
            Ok(Poll::Pending)
        })
        .await;
        assert_eq!(result.unwrap_err(), LOGIN_CANCELLED);
    }
}
