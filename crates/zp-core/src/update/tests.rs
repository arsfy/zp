use std::sync::{Arc, Mutex};

use ring::signature::{Ed25519KeyPair, KeyPair as _};
use serde_json::json;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::*;

fn release(tag: &str, draft: bool, prerelease: bool) -> Release {
    Release {
        tag_name: tag.into(),
        draft,
        prerelease,
        assets: vec![],
    }
}

#[test]
fn channels_and_semver() {
    for (candidate_version, current) in [
        ("v0.3.0-rc.10", "0.3.0-rc.2"),
        ("0.3.0-rc.1", "0.3.0-preview.10"),
        ("0.3.0-preview.10", "0.3.0-preview.2"),
        ("0.3.0", "0.3.0-rc.10"),
        ("0.4.0-preview.1", "0.3.0"),
    ] {
        assert!(is_newer(candidate_version, current));
        assert!(!is_newer(current, candidate_version));
    }
    assert!(!is_newer("0.3.0+new", "0.3.0+old"));
    assert!(!is_newer("invalid", "0.1.0"));
    assert!(!is_newer("0.3", "0.1.0"));
    for tag in ["v0.3.0-preview.1", "v0.3.0-rc.1"] {
        let release = release(tag, false, true);
        assert!(!candidate(&release, UpdateChannel::Stable, "0.1.0"));
        assert!(candidate(&release, UpdateChannel::Preview, "0.1.0"));
    }
    for release in [
        release("v0.3.0", true, false),
        release("v0.3.0-rc.1", false, false),
        release("v0.3.0", false, true),
        release("v0.3.0-beta.1", false, true),
        release("v0.3.0-rc.1.extra", false, true),
        release("v0.3.0", false, false),
    ] {
        assert!(!candidate(&release, UpdateChannel::Preview, "0.4.0-rc.1"));
    }
    assert!(candidate(
        &release("v0.3.0", false, false),
        UpdateChannel::Preview,
        "0.3.0-rc.1"
    ));
}

fn key() -> Ed25519KeyPair {
    Ed25519KeyPair::from_seed_unchecked(&[7; 32]).unwrap()
}

fn public_key() -> String {
    base64::engine::general_purpose::STANDARD.encode(key().public_key().as_ref())
}

fn manifest(version: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "schema_version": 1, "version": version,
        "channel": version_channel(version).unwrap(),
        "published_at": "2026-11-18T00:00:00Z", "notes": "Changes", "platforms": {}
    }))
    .unwrap()
}

#[test]
fn manifest_requires_authentic_signature_and_consistent_channel() {
    let bytes = manifest("0.3.0-rc.1");
    let signature = key().sign(&bytes);
    assert_eq!(
        verify_manifest(&bytes, signature.as_ref(), &public_key())
            .unwrap()
            .channel,
        UpdateChannel::Preview
    );
    assert!(verify_manifest(&bytes, signature.as_ref(), "").is_err());
    let mut tampered = bytes.clone();
    tampered.push(b' ');
    assert!(verify_manifest(&tampered, signature.as_ref(), &public_key()).is_err());
    let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    value["channel"] = json!("stable");
    let inconsistent = serde_json::to_vec(&value).unwrap();
    assert!(
        verify_manifest(
            &inconsistent,
            key().sign(&inconsistent).as_ref(),
            &public_key()
        )
        .is_err()
    );
}

type Routes = Arc<Mutex<BTreeMap<String, Vec<u8>>>>;

async fn server() -> (
    String,
    Routes,
    Arc<Mutex<Vec<String>>>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let routes: Routes = Arc::default();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let served = routes.clone();
    let recorded = requests.clone();
    let task = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            let n = socket.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..n]);
            let path = request.split_whitespace().nth(1).unwrap().to_owned();
            recorded.lock().unwrap().push(path.clone());
            let body = served.lock().unwrap().get(&path).cloned();
            let status = if body.is_some() {
                "200 OK"
            } else {
                "404 Not Found"
            };
            let body = body.unwrap_or_default();
            let header = format!(
                "HTTP/1.1 {status}\r\nconnection: close\r\ncontent-length: {}\r\n\r\n",
                body.len()
            );
            socket.write_all(header.as_bytes()).await.unwrap();
            let _ = socket.write_all(&body).await;
        }
    });
    (url, routes, requests, task)
}

#[tokio::test]
async fn discovery_paginates_sorts_and_never_downloads_packages() {
    let (url, routes, requests, task) = server().await;
    let package = Asset {
        name: "ZP.zip".into(),
        kind: PackageKind::MacApp,
        url: format!("{url}/ZP.zip"),
        size: 3,
        sha256: sha256_hex(b"new"),
    };
    let mut feed: serde_json::Value = serde_json::from_slice(&manifest("0.3.0-rc.10")).unwrap();
    feed["platforms"] = json!({"darwin-aarch64": [package]});
    let bytes = serde_json::to_vec(&feed).unwrap();
    let assets = json!([
        {"name":"release.json", "browser_download_url":format!("{url}/release.json"), "size":bytes.len()},
        {"name":"release.json.sig", "browser_download_url":format!("{url}/release.json.sig"), "size":64},
        {"name":"ZP.zip", "browser_download_url":package.url, "size":3}
    ]);
    let mut page =
        vec![json!({"tag_name":"v9.0.0", "draft":true, "prerelease":false, "assets":[]}); 99];
    page.push(json!({"tag_name":"v0.3.0-rc.2", "draft":false, "prerelease":true, "assets":[]}));
    {
        let mut routes = routes.lock().unwrap();
        routes.insert(
            "/releases?per_page=100&page=1".into(),
            serde_json::to_vec(&page).unwrap(),
        );
        routes.insert(
            "/releases?per_page=100&page=2".into(),
            serde_json::to_vec(&json!([
                {"tag_name":"v0.3.0-rc.10", "draft":false, "prerelease":true, "assets":assets}
            ]))
            .unwrap(),
        );
        routes.insert("/release.json".into(), bytes.clone());
        routes.insert(
            "/release.json.sig".into(),
            key().sign(&bytes).as_ref().to_vec(),
        );
    }
    let check_version = |channel, platform: &'static str| {
        let url = format!("{url}/releases");
        async move {
            check(
                &url,
                channel,
                "0.1.0",
                platform,
                PackageKind::MacApp,
                &public_key(),
            )
            .await
            .unwrap()
        }
    };
    assert!(
        matches!(check_version(UpdateChannel::Preview, "darwin-aarch64").await,
        CheckResult::Available(u) if u.version == "0.3.0-rc.10")
    );
    assert_eq!(
        check_version(UpdateChannel::Stable, "darwin-aarch64").await,
        CheckResult::UpToDate
    );
    assert_eq!(
        check_version(UpdateChannel::Preview, "linux-x86_64").await,
        CheckResult::Unavailable("0.3.0-rc.10".into())
    );
    assert!(!requests.lock().unwrap().iter().any(|r| r == "/ZP.zip"));
    task.abort();
}

#[tokio::test]
async fn downloads_verify_and_remove_incomplete_files() {
    let (url, routes, _, task) = server().await;
    routes
        .lock()
        .unwrap()
        .insert("/ZP.zip".into(), b"new".to_vec());
    let asset = Asset {
        name: "ZP.zip".into(),
        kind: PackageKind::MacApp,
        url: format!("{url}/ZP.zip"),
        size: 3,
        sha256: sha256_hex(b"new"),
    };
    let dir = tempfile::tempdir().unwrap();
    let progress = std::cell::Cell::new(0);
    let path = download(
        &asset,
        dir.path(),
        &CancellationToken::new(),
        |received, _| progress.set(received),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"new");
    assert_eq!(progress.get(), 3);
    std::fs::remove_file(&path).unwrap();
    for bad in [
        Asset {
            sha256: "00".repeat(32),
            ..asset.clone()
        },
        Asset {
            size: 4,
            ..asset.clone()
        },
        Asset {
            name: "../zp".into(),
            ..asset.clone()
        },
    ] {
        assert!(
            download(&bad, dir.path(), &CancellationToken::new(), |_, _| {})
                .await
                .is_err()
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(
        download(&asset, dir.path(), &cancel, |_, _| {})
            .await
            .is_err()
    );
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    task.abort();
}
