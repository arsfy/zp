//! Settings: global (`~/.zp/settings.json`) and per project (`<project>/.zp/settings.json`,
//! only read for trusted projects).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::permissions::{PermissionMode, PermissionPolicy};

/// Permission settings.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PermissionSettings {
    /// The default mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<PermissionMode>,
    /// Model for auto-review, as `provider/model`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_model: Option<String>,
    /// Command prefixes that run without asking, e.g. `cargo test`, `git status`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub allow_commands: Vec<String>,
    /// Directories outside the project the tools may use without asking.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub additional_directories: Vec<String>,
}

/// Light or dark UI.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ThemeChoice {
    /// Follow the system appearance.
    #[default]
    System,
    Dark,
    Light,
}

/// An OpenAI-compatible (or Anthropic-compatible) endpoint added by the user.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CustomProvider {
    /// Provider id (`[a-z0-9_-]`), also the keychain entry of its key.
    pub id: String,
    pub name: String,
    /// API base URL, e.g. `http://localhost:11434/v1`.
    pub base_url: String,
    /// Wire protocol: `openai-completions` (default) or `anthropic-messages`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    /// Extra request headers.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// Model ids; empty means the models the endpoint lists.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
}

/// The contents of a `settings.json`. Unknown keys are kept when saving.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// Model of new conversations, as `provider/model`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// Reasoning level of new conversations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_thinking: Option<zp_ai::ThinkingLevel>,
    /// Name conversations with a short model-generated title (default on).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generate_titles: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<ThemeChoice>,
    pub permissions: PermissionSettings,
    /// User-defined providers.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<CustomProvider>,
    /// Shortcut overrides: action name (`zp::NewChat`) → keystrokes (`cmd-k`); an empty
    /// string removes the shortcut.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub keybindings: BTreeMap<String, String>,
    #[serde(flatten)]
    pub other: Map<String, Value>,
}

/// `<project>/.zp/settings.json`.
pub fn project_settings_file(root: &Path) -> PathBuf {
    root.join(".zp").join("settings.json")
}

impl Settings {
    /// Reads `path`; a missing file gives the defaults.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("{} is not valid settings", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).with_context(|| format!("could not read {}", path.display())),
        }
    }

    /// Reads `path`, logging problems and falling back to the defaults.
    pub fn load_or_default(path: &Path) -> Self {
        Self::load(path).unwrap_or_else(|error| {
            tracing::warn!(error = %format!("{error:#}"), "ignoring settings");
            Self::default()
        })
    }

    /// Writes the settings to `path`.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut text = serde_json::to_string_pretty(self)?;
        text.push('\n');
        std::fs::write(path, text).with_context(|| format!("could not write {}", path.display()))
    }
}

/// The settings that apply to a project: global ones, overridden by the project's when it
/// is trusted.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProjectSettings {
    pub global: Settings,
    /// `None` when the project has none or is not trusted.
    pub project: Option<Settings>,
}

impl ProjectSettings {
    /// Loads the global settings from `data_dir` and, if `trusted`, the project's.
    pub fn load(data_dir: Option<&Path>, root: &Path, trusted: bool) -> Self {
        let global = data_dir
            .map(|dir| Settings::load_or_default(&crate::paths::settings_file(dir)))
            .unwrap_or_default();
        let file = project_settings_file(root);
        let project = (trusted && file.is_file()).then(|| Settings::load_or_default(&file));
        Self { global, project }
    }

    /// The permission policy for `root`; `mode` (the user's choice for this project in the
    /// UI or on the command line) wins over the settings files.
    pub fn permission_policy(&self, root: &Path, mode: Option<PermissionMode>) -> PermissionPolicy {
        let global = &self.global.permissions;
        let project = self.project.as_ref().map(|s| &s.permissions);
        let mut directories = Vec::new();
        let mut commands = Vec::new();
        for settings in std::iter::once(global).chain(project) {
            for dir in &settings.additional_directories {
                let path = zp_tools::resolve_path(root, dir);
                directories.push(path.canonicalize().unwrap_or(path));
            }
            commands.extend(settings.allow_commands.iter().cloned());
        }
        PermissionPolicy {
            mode: mode
                .or(project.and_then(|p| p.mode))
                .or(global.mode)
                .unwrap_or_default(),
            root: root.to_path_buf(),
            additional_directories: directories,
            allow_commands: commands,
            review_model: project
                .and_then(|p| p.review_model.clone())
                .or_else(|| global.review_model.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layers_and_round_trip() {
        let data = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let root = project.path().canonicalize().unwrap();
        std::fs::write(
            crate::paths::settings_file(data.path()),
            r#"{"theme": "dark", "permissions": {"mode": "autoEdit", "allowCommands": ["cargo test"], "reviewModel": "openai/gpt-5-mini"}}"#,
        )
        .unwrap();
        std::fs::create_dir_all(root.join(".zp")).unwrap();
        std::fs::create_dir_all(root.join("vendor")).unwrap();
        std::fs::write(
            project_settings_file(&root),
            r#"{"permissions": {"mode": "fullAuto", "allowCommands": ["make"], "additionalDirectories": ["vendor"]}}"#,
        )
        .unwrap();

        let untrusted = ProjectSettings::load(Some(data.path()), &root, false);
        assert_eq!(untrusted.project, None);
        let policy = untrusted.permission_policy(&root, None);
        assert_eq!(policy.mode, PermissionMode::AutoEdit);
        assert_eq!(policy.allow_commands, ["cargo test"]);

        let trusted = ProjectSettings::load(Some(data.path()), &root, true);
        let policy = trusted.permission_policy(&root, None);
        assert_eq!(policy.mode, PermissionMode::FullAuto);
        assert_eq!(policy.allow_commands, ["cargo test", "make"]);
        assert_eq!(policy.additional_directories, [root.join("vendor")]);
        assert_eq!(policy.review_model.as_deref(), Some("openai/gpt-5-mini"));
        assert_eq!(
            trusted
                .permission_policy(&root, Some(PermissionMode::Ask))
                .mode,
            PermissionMode::Ask
        );

        // Unknown keys survive a save.
        let file = crate::paths::settings_file(data.path());
        let mut settings = Settings::load(&file).unwrap();
        settings.permissions.mode = Some(PermissionMode::Ask);
        settings.save(&file).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("\"theme\": \"dark\""));
        assert!(text.contains("\"mode\": \"ask\""));
        // Invalid files fall back to the defaults.
        std::fs::write(&file, "{ nope").unwrap();
        assert!(Settings::load(&file).is_err());
        assert_eq!(Settings::load_or_default(&file), Settings::default());
    }

    #[test]
    fn general_appearance_providers_and_keys() {
        let text = r#"{
            "defaultModel": "deepseek/deepseek-v4-pro",
            "defaultThinking": "high",
            "generateTitles": false,
            "theme": "light",
            "providers": [{"id": "local", "name": "Local", "baseUrl": "http://localhost:1234/v1", "models": ["m1"]}],
            "keybindings": {"zp::NewChat": "cmd-k"}
        }"#;
        let settings: Settings = serde_json::from_str(text).unwrap();
        assert_eq!(settings.default_thinking, Some(zp_ai::ThinkingLevel::High));
        assert_eq!(settings.theme, Some(ThemeChoice::Light));
        assert_eq!(settings.providers[0].models, ["m1"]);
        assert_eq!(settings.keybindings["zp::NewChat"], "cmd-k");
        let back: Settings =
            serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
        assert_eq!(back, settings);
        // Defaults stay out of the file.
        assert_eq!(
            serde_json::to_string(&Settings::default()).unwrap(),
            r#"{"permissions":{}}"#
        );
    }
}
