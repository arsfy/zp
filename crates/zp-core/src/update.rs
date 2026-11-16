//! Update checks: a small JSON feed published with each release (`latest.json`, written
//! by `scripts/release.sh`) lists the newest version and one download per platform with
//! its SHA-256.
//!
//! ```json
//! {"version": "0.2.0", "notes": "…", "platforms": {
//!   "darwin-aarch64": {"url": "https://…/ZP-0.2.0-darwin-aarch64.zip", "sha256": "…"}}}
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// Where releases are announced.
pub const DEFAULT_FEED_URL: &str =
    "https://github.com/arsfy/zp/releases/latest/download/latest.json";

/// A downloadable build.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asset {
    pub url: String,
    pub sha256: String,
}

/// The release feed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Feed {
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(default)]
    pub platforms: BTreeMap<String, Asset>,
}

/// An update for this platform.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Update {
    pub version: String,
    pub notes: Option<String>,
    pub asset: Asset,
}

/// This build's platform key, e.g. `darwin-aarch64`, `linux-x86_64`, `windows-x86_64`.
pub fn platform_key() -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    format!("{os}-{}", std::env::consts::ARCH)
}

fn parse_version(version: &str) -> (Vec<u64>, Option<&str>) {
    let version = version.trim().trim_start_matches('v');
    let (core, pre) = match version.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (version, None),
    };
    let core = core.split('+').next().unwrap_or(core);
    (
        core.split('.')
            .map(|part| part.parse().unwrap_or(0))
            .collect(),
        pre,
    )
}

/// Whether `candidate` is newer than `current` (semantic versions; a pre-release is older
/// than its release).
pub fn is_newer(candidate: &str, current: &str) -> bool {
    let (a, a_pre) = parse_version(candidate);
    let (b, b_pre) = parse_version(current);
    let len = a.len().max(b.len());
    for i in 0..len {
        let (x, y) = (
            a.get(i).copied().unwrap_or(0),
            b.get(i).copied().unwrap_or(0),
        );
        if x != y {
            return x > y;
        }
    }
    match (a_pre, b_pre) {
        (None, Some(_)) => true,
        (Some(x), Some(y)) => x > y,
        _ => false,
    }
}

/// The update `feed` offers this platform over `current`, if any.
pub fn update_from(feed: &Feed, current: &str, platform: &str) -> Option<Update> {
    if !is_newer(&feed.version, current) {
        return None;
    }
    let asset = feed.platforms.get(platform)?.clone();
    Some(Update {
        version: feed.version.clone(),
        notes: feed.notes.clone(),
        asset,
    })
}

/// Fetches the feed at `url` and returns the update for this build, if any.
pub async fn check(url: &str) -> anyhow::Result<Option<Update>> {
    let response = reqwest::get(url)
        .await
        .context("could not reach the update feed")?;
    if !response.status().is_success() {
        bail!("the update feed answered HTTP {}", response.status());
    }
    let text = response.text().await?;
    let feed: Feed = serde_json::from_str(&text).context("the update feed is not valid")?;
    Ok(update_from(&feed, crate::VERSION, &platform_key()))
}

/// The SHA-256 of `bytes`, in hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Downloads `asset` into `dir` and checks its SHA-256. Returns the file.
pub async fn download(asset: &Asset, dir: &Path) -> anyhow::Result<PathBuf> {
    let response = reqwest::get(&asset.url)
        .await
        .context("could not download the update")?;
    if !response.status().is_success() {
        bail!("the download answered HTTP {}", response.status());
    }
    let bytes = response
        .bytes()
        .await
        .context("the download was interrupted")?;
    let actual = sha256_hex(&bytes);
    if !actual.eq_ignore_ascii_case(asset.sha256.trim()) {
        bail!(
            "the download is corrupt (SHA-256 {actual}, expected {})",
            asset.sha256
        );
    }
    let name = asset
        .url
        .rsplit('/')
        .next()
        .filter(|n| !n.is_empty())
        .unwrap_or("zp-update");
    std::fs::create_dir_all(dir)?;
    let path = dir.join(name);
    std::fs::write(&path, &bytes).with_context(|| format!("could not write {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("v1.0", "0.9.9"));
        assert!(is_newer("1.0.0", "1.0.0-beta.2"));
        assert!(is_newer("1.0.0-rc.2", "1.0.0-rc.1"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0-beta", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
    }

    #[test]
    fn feed_selection() {
        let feed: Feed = serde_json::from_str(
            r#"{"version": "9.0.0", "notes": "Big", "platforms": {"darwin-aarch64": {"url": "https://x/ZP.zip", "sha256": "ab"}}}"#,
        )
        .unwrap();
        let update = update_from(&feed, "0.1.0", "darwin-aarch64").unwrap();
        assert_eq!(
            (update.version.as_str(), update.asset.url.as_str()),
            ("9.0.0", "https://x/ZP.zip")
        );
        assert_eq!(
            update_from(&feed, "0.1.0", "linux-x86_64"),
            None,
            "no build for the platform"
        );
        assert_eq!(update_from(&feed, "9.0.0", "darwin-aarch64"), None);
        assert!(platform_key().contains('-'));
    }

    #[tokio::test]
    async fn downloads_and_verifies() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut buffer = [0u8; 2048];
                let _ = socket.read(&mut buffer).await;
                let body = b"new build";
                let head = format!(
                    "HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-length: {}\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(body).await;
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let good = Asset {
            url: format!("{base}/ZP-9.zip"),
            sha256: sha256_hex(b"new build").to_uppercase(),
        };
        let path = download(&good, dir.path()).await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new build");
        assert_eq!(path.file_name().unwrap(), "ZP-9.zip");
        let bad = Asset {
            sha256: "00".into(),
            ..good
        };
        let error = download(&bad, dir.path()).await.unwrap_err();
        assert!(error.to_string().contains("corrupt"));
    }
}
