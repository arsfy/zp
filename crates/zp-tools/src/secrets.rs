//! Files that usually hold credentials, kept away from tools that hand content to a model
//! the user did not pick for the main conversation (subagents).

use std::path::{Component, Path};

/// Directories whose contents are credentials or repository internals (`.git/config` can
/// carry tokens in remote URLs).
const SECRET_DIRECTORIES: [&str; 4] = [".git", ".ssh", ".gnupg", ".aws"];

/// File names that hold credentials.
const SECRET_NAMES: [&str; 13] = [
    ".env",
    ".netrc",
    "_netrc",
    ".npmrc",
    ".pypirc",
    ".pgpass",
    ".htpasswd",
    ".git-credentials",
    "credentials.json",
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
];

/// Extensions of private keys and key stores.
const SECRET_EXTENSIONS: [&str; 7] = ["pem", "key", "p12", "pfx", "jks", "keystore", "ppk"];

/// `.env.<suffix>` files that are templates without real values.
const ENV_TEMPLATES: [&str; 5] = ["example", "sample", "template", "dist", "defaults"];

/// Whether `path` (relative to the workspace) names a likely credential file: `.env` files
/// (not their templates), private keys, credential stores, or anything inside `.git`,
/// `.ssh`, `.gnupg` or `.aws`. Case-insensitive.
pub fn is_secret_path(path: &Path) -> bool {
    let names: Vec<String> = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name.to_string_lossy().to_lowercase()),
            _ => None,
        })
        .collect();
    let Some(file) = names.last() else {
        return false;
    };
    if names
        .iter()
        .any(|name| SECRET_DIRECTORIES.contains(&name.as_str()))
    {
        return true;
    }
    if SECRET_NAMES.contains(&file.as_str()) {
        return true;
    }
    if let Some(suffix) = file.strip_prefix(".env.") {
        return !ENV_TEMPLATES.contains(&suffix);
    }
    if file.ends_with(".env") {
        return true;
    }
    Path::new(file)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| SECRET_EXTENSIONS.contains(&extension))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::is_secret_path;

    #[test]
    fn credential_files_are_secret_and_ordinary_files_are_not() {
        for path in [
            ".env",
            "config/.env.local",
            ".env.production",
            "deploy/prod.env",
            ".git/config",
            "vendor/.git/HEAD",
            "home/.ssh/known_hosts",
            ".aws/credentials",
            "id_ed25519",
            "keys/server.PEM",
            "certs/site.key",
            "android/release.keystore",
            ".npmrc",
            ".git-credentials",
            "gcp/credentials.json",
        ] {
            assert!(is_secret_path(Path::new(path)), "{path}");
        }
        for path in [
            "",
            ".",
            "src/main.rs",
            ".env.example",
            "config/.env.sample",
            ".github/workflows/ci.yml",
            ".gitignore",
            "docs/environment.md",
            "id_ed25519.pub",
            "src/keyboard.rs",
            "Cargo.lock",
        ] {
            assert!(!is_secret_path(Path::new(path)), "{path}");
        }
    }
}
