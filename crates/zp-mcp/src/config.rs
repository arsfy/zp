//! MCP server configuration: `~/.zp/mcp.json` and `<project>/.zp/mcp.json` (trusted
//! projects only), in the `mcpServers` format other MCP clients use.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};

/// One server entry.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ServerConfig {
    /// `stdio`, `http` or `streamable-http`; inferred from `command` / `url` when absent.
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Executable of a stdio server (not a shell command line).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Endpoint of a streamable HTTP server.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// `false` keeps the entry without connecting.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Per-request timeout in seconds (default 60).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
    /// What the server offers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// How to reach a server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Transport {
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        cwd: Option<PathBuf>,
    },
    Http {
        url: String,
        headers: BTreeMap<String, String>,
    },
}

/// A validated server, with variables expanded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Server {
    pub name: String,
    pub transport: Transport,
    pub enabled: bool,
    pub timeout_secs: u64,
    pub description: Option<String>,
    /// Which file defines it.
    pub source: PathBuf,
}

#[derive(Default, Deserialize)]
struct File {
    #[serde(default, rename = "mcpServers")]
    servers: BTreeMap<String, ServerConfig>,
}

/// Expands `${VAR}` from `env` (unknown variables become empty) and a leading `~/`.
pub fn expand(value: &str, env: &dyn Fn(&str) -> Option<String>) -> String {
    let mut out = String::new();
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        match rest[start + 2..].find('}') {
            Some(end) => {
                let name = &rest[start + 2..start + 2 + end];
                out.push_str(&env(name).unwrap_or_default());
                rest = &rest[start + 3 + end..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    if let Some(stripped) = out.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(stripped).to_string_lossy().into_owned();
    }
    out
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn resolve(
    name: &str,
    config: &ServerConfig,
    cwd: &Path,
    source: &Path,
    env: &dyn Fn(&str) -> Option<String>,
) -> anyhow::Result<Server> {
    if !valid_name(name) {
        bail!("invalid server name `{name}` (letters, digits, _ and - only)");
    }
    let kind = config.kind.as_deref();
    let transport = match (kind, &config.command, &config.url) {
        (Some("sse"), _, _) => {
            bail!("SSE servers are not supported; use the streamable HTTP endpoint (often /mcp)")
        }
        (Some("stdio") | None, Some(command), None) => Transport::Stdio {
            command: expand(command, env),
            args: config.args.iter().map(|a| expand(a, env)).collect(),
            env: config
                .env
                .iter()
                .map(|(k, v)| (k.clone(), expand(v, env)))
                .collect(),
            cwd: config.cwd.as_deref().map(|dir| {
                let dir = PathBuf::from(expand(dir, env));
                if dir.is_absolute() {
                    dir
                } else {
                    cwd.join(dir)
                }
            }),
        },
        (Some("http" | "streamable-http") | None, None, Some(url)) => Transport::Http {
            url: expand(url, env),
            headers: config
                .headers
                .iter()
                .map(|(k, v)| (k.clone(), expand(v, env)))
                .collect(),
        },
        (Some(other), _, _) if !matches!(other, "stdio" | "http" | "streamable-http") => {
            bail!("unknown server type `{other}`")
        }
        (_, Some(_), Some(_)) => bail!("set either `command` or `url`, not both"),
        _ => bail!("a server needs a `command` or a `url`"),
    };
    Ok(Server {
        name: name.to_owned(),
        transport,
        enabled: config.enabled.unwrap_or(true),
        timeout_secs: config.timeout.unwrap_or(60).max(1),
        description: config.description.clone(),
        source: source.to_path_buf(),
    })
}

/// `<project>/.zp/mcp.json`.
pub fn project_file(root: &Path) -> PathBuf {
    root.join(".zp").join("mcp.json")
}

/// `<data>/mcp.json`.
pub fn user_file(data_dir: &Path) -> PathBuf {
    data_dir.join("mcp.json")
}

fn read(path: &Path) -> anyhow::Result<BTreeMap<String, ServerConfig>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(serde_json::from_str::<File>(&text)
            .with_context(|| format!("{} is not a valid MCP configuration", path.display()))?
            .servers),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(error) => Err(error).with_context(|| format!("could not read {}", path.display())),
    }
}

/// The servers of a project: the user's, overridden by the project's when trusted. A
/// project entry with neither `command` nor `url` only changes `enabled`. Returns the
/// servers and the configuration errors.
pub fn load(
    data_dir: Option<&Path>,
    root: &Path,
    trusted: bool,
    env: &dyn Fn(&str) -> Option<String>,
) -> (Vec<Server>, Vec<String>) {
    let mut errors = Vec::new();
    let mut merged: BTreeMap<String, (ServerConfig, PathBuf)> = BTreeMap::new();
    let mut files: Vec<PathBuf> = data_dir.map(user_file).into_iter().collect();
    if trusted {
        files.push(project_file(root));
    }
    for file in files {
        match read(&file) {
            Ok(servers) => {
                for (name, config) in servers {
                    let partial =
                        config.command.is_none() && config.url.is_none() && config.kind.is_none();
                    match merged.get_mut(&name) {
                        Some((existing, _)) if partial => {
                            if config.enabled.is_some() {
                                existing.enabled = config.enabled;
                            }
                        }
                        _ => {
                            merged.insert(name, (config, file.clone()));
                        }
                    }
                }
            }
            Err(error) => errors.push(format!("{error:#}")),
        }
    }
    let mut servers = Vec::new();
    for (name, (config, source)) in merged {
        match resolve(&name, &config, root, &source, env) {
            Ok(server) => servers.push(server),
            Err(error) => errors.push(format!("{name}: {error:#}")),
        }
    }
    (servers, errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_and_validates() {
        let data = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            user_file(data.path()),
            r#"{"mcpServers": {
                "files": {"command": "npx", "args": ["-y", "server", "${HOME_DIR}"], "env": {"TOKEN": "${TOKEN}"}},
                "docs": {"url": "https://example.com/mcp", "headers": {"Authorization": "Bearer ${TOKEN}"}},
                "old": {"type": "sse", "url": "https://x/sse"},
                "bad name": {"command": "x"}
            }}"#,
        )
        .unwrap();
        std::fs::create_dir_all(project.path().join(".zp")).unwrap();
        std::fs::write(
            project_file(project.path()),
            r#"{"mcpServers": {"docs": {"enabled": false}, "local": {"command": "./tool", "cwd": "tools"}}}"#,
        )
        .unwrap();
        let env = |name: &str| match name {
            "TOKEN" => Some("t0k".to_owned()),
            _ => None,
        };

        let (servers, errors) = load(Some(data.path()), project.path(), false, &env);
        let names: Vec<&str> = servers.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["docs", "files"]);
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(servers[0].enabled);
        assert_eq!(
            servers[1].transport,
            Transport::Stdio {
                command: "npx".into(),
                args: vec!["-y".into(), "server".into(), String::new()],
                env: [("TOKEN".to_owned(), "t0k".to_owned())].into(),
                cwd: None,
            }
        );
        assert!(
            matches!(&servers[0].transport, Transport::Http { headers, .. } if headers["Authorization"] == "Bearer t0k")
        );

        let (servers, _) = load(Some(data.path()), project.path(), true, &env);
        let docs = servers.iter().find(|s| s.name == "docs").unwrap();
        assert!(!docs.enabled, "the project turned it off");
        let local = servers.iter().find(|s| s.name == "local").unwrap();
        assert!(
            matches!(&local.transport, Transport::Stdio { cwd: Some(dir), .. } if *dir == project.path().join("tools"))
        );
    }
}
