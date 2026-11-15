//! Well-known filesystem locations used by ZP.
//!
//! Everything lives under a single data directory (`~/.zp` by default). Set the
//! `ZP_HOME` environment variable to override it, e.g. for tests or portable setups.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

/// Environment variable that overrides the data directory.
pub const ZP_HOME_ENV: &str = "ZP_HOME";

/// Root data directory (`$ZP_HOME` or `~/.zp`).
pub fn data_dir() -> Result<PathBuf> {
    resolve_data_dir(
        std::env::var_os(ZP_HOME_ENV).map(PathBuf::from),
        dirs::home_dir(),
    )
}

fn resolve_data_dir(override_dir: Option<PathBuf>, home: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(dir) = override_dir.filter(|dir| !dir.as_os_str().is_empty()) {
        return Ok(dir);
    }
    let home = home.context("could not determine the home directory")?;
    Ok(home.join(".zp"))
}

/// Global settings file.
pub fn settings_file(data_dir: &Path) -> PathBuf {
    data_dir.join("settings.json")
}

/// Persisted UI state (recent projects, windows, open tabs).
pub fn state_file(data_dir: &Path) -> PathBuf {
    data_dir.join("state.json")
}

/// Directory containing all session files.
pub fn sessions_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("sessions")
}

/// Directory containing file snapshots used for diffs and rollbacks.
pub fn checkpoints_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("checkpoints")
}

/// Directory for log files.
pub fn logs_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("logs")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_wins_over_home() {
        let dir = resolve_data_dir(Some("/tmp/zp-test".into()), Some("/home/u".into())).unwrap();
        assert_eq!(dir, PathBuf::from("/tmp/zp-test"));
    }

    #[test]
    fn empty_override_is_ignored() {
        let dir = resolve_data_dir(Some(PathBuf::new()), Some("/home/u".into())).unwrap();
        assert_eq!(dir, Path::new("/home/u").join(".zp"));
    }

    #[test]
    fn missing_home_is_an_error() {
        assert!(resolve_data_dir(None, None).is_err());
    }

    #[test]
    fn subpaths_are_under_data_dir() {
        let root = Path::new("/data");
        assert_eq!(settings_file(root), root.join("settings.json"));
        assert_eq!(state_file(root), root.join("state.json"));
        assert_eq!(sessions_dir(root), root.join("sessions"));
        assert_eq!(checkpoints_dir(root), root.join("checkpoints"));
        assert_eq!(logs_dir(root), root.join("logs"));
    }
}
