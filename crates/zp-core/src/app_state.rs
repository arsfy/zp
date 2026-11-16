//! Persisted UI state (`~/.zp/state.json`): recent projects, which project windows were
//! open, and each project's window geometry and panel layout.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use zp_ai::now_ms;

/// Most recent projects remembered.
pub const MAX_RECENT_PROJECTS: usize = 20;

/// A project opened before.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecentProject {
    /// Project directory.
    pub path: PathBuf,
    /// Last time it was opened (Unix milliseconds).
    pub last_opened: i64,
}

/// How a window was shown.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WindowMode {
    /// A normal window.
    #[default]
    Windowed,
    /// Maximized; the bounds are the restore size.
    Maximized,
    /// Full screen; the bounds are the restore size.
    Fullscreen,
}

/// Window placement, relative to its display.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowGeometry {
    /// Stable id of the display the window was on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
    /// Left edge.
    pub x: f32,
    /// Top edge.
    pub y: f32,
    /// Width.
    pub width: f32,
    /// Height.
    pub height: f32,
    /// Window state.
    #[serde(default)]
    pub mode: WindowMode,
}

/// Visibility and size of the side panels.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PanelLayout {
    /// Whether the left sidebar is shown.
    pub sidebar_visible: bool,
    /// Sidebar width in pixels.
    pub sidebar_width: f32,
    /// Whether the right (changes) panel is shown.
    pub right_panel_visible: bool,
    /// Right panel width in pixels.
    pub right_panel_width: f32,
    /// Whether the terminal panel is shown.
    pub terminal_visible: bool,
    /// Terminal panel height in pixels.
    pub terminal_height: f32,
    /// Whether the terminal panel takes the whole height.
    pub terminal_maximized: bool,
    /// Width of the terminal list in pixels.
    pub terminal_list_width: f32,
}

impl Default for PanelLayout {
    fn default() -> Self {
        Self {
            sidebar_visible: true,
            sidebar_width: 240.0,
            right_panel_visible: false,
            right_panel_width: 320.0,
            terminal_visible: false,
            terminal_height: 260.0,
            terminal_maximized: false,
            terminal_list_width: 180.0,
        }
    }
}

/// Per-project UI state.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ProjectUiState {
    /// Last window geometry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<WindowGeometry>,
    /// Panel layout.
    pub layout: PanelLayout,
    /// Open conversation tabs, in order.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tabs: Vec<TabState>,
    /// Index of the active tab.
    pub active_tab: usize,
    /// Whether the user trusts the project; `None` until asked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trusted: Option<bool>,
    /// The permission mode chosen for this project (overrides the settings).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<crate::permissions::PermissionMode>,
}

/// A conversation tab to restore.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TabState {
    /// Session file; `None` for a chat that had no messages yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<PathBuf>,
    /// The branch shown (session leaf entry), if not the latest one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub leaf: Option<String>,
    /// Pinned tabs stay at the front.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub pinned: bool,
}

/// Everything in `state.json`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AppState {
    /// Most recently opened first.
    pub recent_projects: Vec<RecentProject>,
    /// Project windows to reopen at startup, in opening order.
    pub open_projects: Vec<PathBuf>,
    /// Per-project state, keyed by project directory.
    pub projects: BTreeMap<PathBuf, ProjectUiState>,
    /// Geometry of the last welcome window.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub welcome_window: Option<WindowGeometry>,
}

impl AppState {
    /// Reads `path`; a missing or unreadable file yields the default state.
    pub fn load(path: &Path) -> Self {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
                tracing::warn!(path = %path.display(), %error, "ignoring invalid state file");
                Self::default()
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "could not read state file");
                Self::default()
            }
        }
    }

    /// Writes the state atomically.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let temp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        std::fs::write(&temp, serde_json::to_vec_pretty(self)?)
            .with_context(|| format!("writing {}", temp.display()))?;
        std::fs::rename(&temp, path).with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }

    /// Moves `path` to the front of the recent projects.
    pub fn touch_recent(&mut self, path: &Path) {
        self.recent_projects.retain(|recent| recent.path != path);
        self.recent_projects.insert(
            0,
            RecentProject {
                path: path.to_path_buf(),
                last_opened: now_ms(),
            },
        );
        self.recent_projects.truncate(MAX_RECENT_PROJECTS);
    }

    /// Forgets a recent project.
    pub fn remove_recent(&mut self, path: &Path) {
        self.recent_projects.retain(|recent| recent.path != path);
    }

    /// Records that a project window is open.
    pub fn mark_open(&mut self, path: &Path) {
        if !self.open_projects.iter().any(|open| open == path) {
            self.open_projects.push(path.to_path_buf());
        }
    }

    /// Records that a project window was closed by the user.
    pub fn mark_closed(&mut self, path: &Path) {
        self.open_projects.retain(|open| open != path);
    }

    /// The UI state of `path` (defaults if unknown).
    pub fn project(&self, path: &Path) -> ProjectUiState {
        self.projects.get(path).cloned().unwrap_or_default()
    }

    /// Mutable UI state of `path`, created on demand.
    pub fn project_mut(&mut self, path: &Path) -> &mut ProjectUiState {
        self.projects.entry(path.to_path_buf()).or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_and_open_projects() {
        let mut state = AppState::default();
        for i in 0..25 {
            state.touch_recent(Path::new(&format!("/p{i}")));
        }
        state.touch_recent(Path::new("/p3"));
        assert_eq!(state.recent_projects.len(), MAX_RECENT_PROJECTS);
        assert_eq!(state.recent_projects[0].path, Path::new("/p3"));
        assert_eq!(state.recent_projects[1].path, Path::new("/p24"));
        state.remove_recent(Path::new("/p3"));
        assert_eq!(state.recent_projects[0].path, Path::new("/p24"));

        state.mark_open(Path::new("/a"));
        state.mark_open(Path::new("/b"));
        state.mark_open(Path::new("/a"));
        assert_eq!(state.open_projects, [Path::new("/a"), Path::new("/b")]);
        state.mark_closed(Path::new("/a"));
        assert_eq!(state.open_projects, [Path::new("/b")]);
    }

    #[test]
    fn round_trips_and_tolerates_bad_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        assert_eq!(AppState::load(&path), AppState::default());

        let mut state = AppState::default();
        state.touch_recent(Path::new("/work/app"));
        state.mark_open(Path::new("/work/app"));
        let project = state.project_mut(Path::new("/work/app"));
        project.layout.sidebar_width = 300.0;
        project.window = Some(WindowGeometry {
            display: Some("display-1".into()),
            x: 10.0,
            y: 20.0,
            width: 1200.0,
            height: 800.0,
            mode: WindowMode::Maximized,
        });
        state.save(&path).unwrap();
        assert_eq!(AppState::load(&path), state);
        assert_eq!(
            state.project(Path::new("/unknown")),
            ProjectUiState::default()
        );

        // Unknown fields and missing sections are fine; garbage falls back to defaults.
        std::fs::write(&path, r#"{"openProjects": ["/x"], "future": 1}"#).unwrap();
        assert_eq!(AppState::load(&path).open_projects, [Path::new("/x")]);
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(AppState::load(&path), AppState::default());
    }
}
