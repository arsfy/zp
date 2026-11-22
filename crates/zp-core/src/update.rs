//! GitHub release discovery and authenticated, user-initiated update downloads.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, bail, ensure};
use base64::Engine as _;
use reqwest::Client;
use ring::signature::{ED25519, UnparsedPublicKey};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio_util::sync::CancellationToken;

pub const REPOSITORY_URL: &str = "https://github.com/arsfy/zp";
pub const DEFAULT_RELEASES_URL: &str = "https://api.github.com/repos/arsfy/zp/releases";
pub const PUBLIC_KEY: &str = match option_env!("ZP_UPDATE_PUBLIC_KEY") {
    Some(key) => key,
    None => "",
};
const MAX_METADATA_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UpdateChannel {
    Stable,
    Preview,
}

impl Default for UpdateChannel {
    fn default() -> Self {
        version_channel(crate::VERSION).unwrap_or(Self::Stable)
    }
}

impl UpdateChannel {
    pub const ALL: [Self; 2] = [Self::Stable, Self::Preview];

    pub fn label(self) -> &'static str {
        match self {
            Self::Stable => "Stable",
            Self::Preview => "Preview",
        }
    }

    fn accepts(self, version: &str) -> bool {
        match version_channel(version) {
            Some(Self::Stable) => true,
            Some(Self::Preview) => self == Self::Preview,
            None => false,
        }
    }
}

fn parse_version(version: &str) -> Option<Version> {
    Version::parse(version.strip_prefix('v').unwrap_or(version)).ok()
}

/// Only stable, preview.N and rc.N versions are update candidates.
pub fn version_channel(version: &str) -> Option<UpdateChannel> {
    let version = parse_version(version)?;
    if version.pre.is_empty() {
        return Some(UpdateChannel::Stable);
    }
    let (name, number) = version.pre.as_str().split_once('.')?;
    (matches!(name, "preview" | "rc") && number.parse::<u64>().is_ok())
        .then_some(UpdateChannel::Preview)
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(candidate), Some(current)) => candidate.cmp_precedence(&current).is_gt(),
        _ => false,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PackageKind {
    MacApp,
    WindowsInstaller,
    WindowsPortable,
    AppImage,
    Deb,
    Archive,
}

impl PackageKind {
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacApp
        } else if cfg!(windows) {
            let installed = std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|p| p.join("unins000.exe")))
                .is_some_and(|p| p.exists());
            if installed {
                Self::WindowsInstaller
            } else {
                Self::WindowsPortable
            }
        } else if std::env::var_os("APPIMAGE").is_some() {
            Self::AppImage
        } else if std::env::current_exe().is_ok_and(|p| p == Path::new("/usr/bin/zp")) {
            Self::Deb
        } else {
            Self::Archive
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asset {
    pub name: String,
    pub kind: PackageKind,
    pub url: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Feed {
    pub schema_version: u32,
    pub version: String,
    pub channel: UpdateChannel,
    pub published_at: String,
    #[serde(default)]
    pub notes: String,
    pub platforms: BTreeMap<String, Vec<Asset>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Update {
    pub version: String,
    pub channel: UpdateChannel,
    pub notes: String,
    pub release_url: String,
    pub asset: Asset,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckResult {
    Available(Update),
    UpToDate,
    Unavailable(String),
}

#[derive(Debug, Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
    size: u64,
}

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<ReleaseAsset>,
}

pub fn platform_key() -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    format!("{os}-{}", std::env::consts::ARCH)
}

fn client(timeout: Duration) -> anyhow::Result<Client> {
    Ok(Client::builder()
        .user_agent(concat!("ZP/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .timeout(timeout)
        .build()?)
}

async fn metadata(response: reqwest::Response) -> anyhow::Result<Vec<u8>> {
    let mut response = response.error_for_status()?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            bytes.len() + chunk.len() <= MAX_METADATA_BYTES,
            "release metadata is too large"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn verify_manifest(bytes: &[u8], signature: &[u8], public_key: &str) -> anyhow::Result<Feed> {
    ensure!(
        !public_key.is_empty(),
        "this build has no update verification key; download from {REPOSITORY_URL}/releases"
    );
    let key = base64::engine::general_purpose::STANDARD
        .decode(public_key.trim())
        .context("invalid update verification key")?;
    UnparsedPublicKey::new(&ED25519, key)
        .verify(bytes, signature)
        .map_err(|_| anyhow::anyhow!("release manifest signature is invalid"))?;
    let feed: Feed = serde_json::from_slice(bytes).context("invalid release manifest")?;
    ensure!(
        feed.schema_version == 1,
        "unsupported release manifest version"
    );
    ensure!(
        version_channel(&feed.version) == Some(feed.channel),
        "release channel does not match its version"
    );
    Ok(feed)
}

fn candidate(release: &Release, channel: UpdateChannel, current: &str) -> bool {
    !release.draft
        && channel.accepts(&release.tag_name)
        && version_channel(&release.tag_name)
            .is_some_and(|c| release.prerelease == (c == UpdateChannel::Preview))
        && is_newer(&release.tag_name, current)
}

pub async fn check(
    url: &str,
    channel: UpdateChannel,
    current: &str,
    platform: &str,
    kind: PackageKind,
    public_key: &str,
) -> anyhow::Result<CheckResult> {
    ensure!(
        parse_version(current).is_some(),
        "invalid installed version"
    );
    let client = client(Duration::from_secs(60))?;
    let mut releases = Vec::new();
    for page in 1..=100 {
        let mut page_url = reqwest::Url::parse(url)?;
        page_url
            .query_pairs_mut()
            .append_pair("per_page", "100")
            .append_pair("page", &page.to_string());
        let bytes = metadata(
            client
                .get(page_url)
                .header("Accept", "application/vnd.github+json")
                .send()
                .await
                .context("could not reach GitHub releases")?,
        )
        .await?;
        let batch: Vec<Release> =
            serde_json::from_slice(&bytes).context("invalid GitHub releases response")?;
        let done = batch.len() < 100;
        releases.extend(batch.into_iter().filter(|r| candidate(r, channel, current)));
        if done {
            break;
        }
        ensure!(page < 100, "too many release pages; try again later");
    }
    releases.sort_by(|a, b| {
        parse_version(&b.tag_name)
            .unwrap()
            .cmp_precedence(&parse_version(&a.tag_name).unwrap())
    });
    let Some(newest) = releases.first() else {
        return Ok(CheckResult::UpToDate);
    };
    // A release published without its manifest is skipped; anything wrong with a manifest
    // that is there still fails the check.
    let Some((release, manifest, signature)) = releases.iter().find_map(|release| {
        match manifest_assets(release) {
            Ok((manifest, signature)) => Some((release, manifest, signature)),
            Err(error) => {
                tracing::warn!(release = %release.tag_name, %error, "skipping an update release");
                None
            }
        }
    }) else {
        return Err(manifest_assets(newest).expect_err("no release has a manifest"));
    };
    let bytes = metadata(client.get(&manifest.browser_download_url).send().await?).await?;
    let signature = metadata(client.get(&signature.browser_download_url).send().await?).await?;
    let feed = verify_manifest(&bytes, &signature, public_key)?;
    ensure!(
        parse_version(&feed.version) == parse_version(&release.tag_name),
        "manifest version does not match release tag"
    );
    let Some(asset) = feed
        .platforms
        .get(platform)
        .and_then(|assets| assets.iter().find(|a| a.kind == kind))
    else {
        return Ok(CheckResult::Unavailable(feed.version));
    };
    validate_asset(asset)?;
    ensure!(
        release.assets.iter().any(|a| a.name == asset.name
            && a.browser_download_url == asset.url
            && a.size == asset.size),
        "the update package does not match the release assets"
    );
    Ok(CheckResult::Available(Update {
        release_url: format!("{REPOSITORY_URL}/releases/tag/{}", release.tag_name),
        version: feed.version,
        channel: feed.channel,
        notes: feed.notes,
        asset: asset.clone(),
    }))
}

/// The signed manifest of `release` and its signature.
fn manifest_assets(release: &Release) -> anyhow::Result<(&ReleaseAsset, &ReleaseAsset)> {
    let manifest = release
        .assets
        .iter()
        .find(|a| a.name == "release.json")
        .context("this release is missing release.json")?;
    let signature = release
        .assets
        .iter()
        .find(|a| a.name == "release.json.sig")
        .context("this release is missing its signature")?;
    Ok((manifest, signature))
}

fn validate_asset(asset: &Asset) -> anyhow::Result<()> {
    ensure!(
        !asset.name.is_empty()
            && asset.name != "."
            && asset.name != ".."
            && !asset.name.contains(['/', '\\', ':'])
            && !asset.name.chars().any(char::is_control),
        "invalid update filename"
    );
    ensure!(
        asset.size > 0
            && asset.sha256.len() == 64
            && asset.sha256.bytes().all(|c| c.is_ascii_hexdigit()),
        "invalid update checksum or size"
    );
    let url = reqwest::Url::parse(&asset.url)?;
    ensure!(
        url.scheme() == "https"
            || (cfg!(any(test, feature = "test-support"))
                && url.scheme() == "http"
                && url.host_str() == Some("127.0.0.1")),
        "update packages require HTTPS"
    );
    Ok(())
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex_digest(&Sha256::digest(bytes))
}

fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn verify_download(path: &Path, asset: &Asset) -> anyhow::Result<()> {
    let mut file = std::fs::File::open(path)?;
    ensure!(
        file.metadata()?.len() == asset.size,
        "the downloaded update size has changed"
    );
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        digest.update(&buffer[..n]);
    }
    ensure!(
        hex_digest(&digest.finalize()).eq_ignore_ascii_case(&asset.sha256),
        "the downloaded update has changed; download it again"
    );
    Ok(())
}

/// Partial files are removed on cancellation or failure; only verified packages persist.
pub async fn download(
    asset: &Asset,
    dir: &Path,
    cancel: &CancellationToken,
    progress: impl Fn(u64, u64),
) -> anyhow::Result<PathBuf> {
    validate_asset(asset)?;
    tokio::select! {
        _ = cancel.cancelled() => bail!("update download cancelled"),
        result = async {
            std::fs::create_dir_all(dir)?;
            let mut file = tempfile::NamedTempFile::new_in(dir)?;
            let mut response = client(Duration::from_secs(30 * 60))?.get(&asset.url).send().await?.error_for_status()?;
            let mut digest = Sha256::new();
            let mut received = 0;
            while let Some(chunk) = response.chunk().await.context("the download was interrupted")? {
                received += chunk.len() as u64;
                ensure!(received <= asset.size, "download exceeds its declared size");
                file.write_all(&chunk)?;
                digest.update(&chunk);
                progress(received, asset.size);
            }
            ensure!(received == asset.size, "the download is incomplete");
            ensure!(hex_digest(&digest.finalize()).eq_ignore_ascii_case(&asset.sha256), "the download is corrupt (SHA-256 mismatch)");
            file.as_file().sync_all()?;
            let path = dir.join(&asset.name);
            file.persist(&path).map_err(|e| e.error)?;
            Ok(path)
        } => result,
    }
}

#[cfg(test)]
mod tests;
