//! Saving files without leaving them half-written, and keeping unreadable ones.

use std::fmt::Display;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::Context as _;

/// Symlinks followed before giving up, like the OS's own loop limit.
const MAX_SYMLINKS: usize = 40;

/// Replaces `path` with `bytes` atomically: writes a temporary file next to it and renames
/// it over `path`, removing the temporary file on failure. A symlinked `path` (settings
/// kept in dotfiles) stays a link: the file it points to is replaced. Creates the parent
/// directory.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let target = resolve_symlinks(path)?;
    let dir = match target.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    std::fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;
    let name = target
        .file_name()
        .with_context(|| format!("{} is not a file path", path.display()))?;
    let mut temp = tempfile::Builder::new()
        .prefix(&format!(".{}.", name.to_string_lossy()))
        .suffix(".tmp")
        .tempfile_in(dir)
        .with_context(|| format!("could not write {}", path.display()))?;
    // The temporary file is private; keep the permissions of the file it replaces.
    if let Ok(metadata) = std::fs::metadata(&target) {
        temp.as_file()
            .set_permissions(metadata.permissions())
            .with_context(|| format!("could not write {}", path.display()))?;
    }
    temp.write_all(bytes)
        .and_then(|()| temp.as_file().sync_all())
        .with_context(|| format!("could not write {}", path.display()))?;
    temp.persist(&target)
        .map_err(|error| error.error)
        .with_context(|| format!("could not write {}", path.display()))?;
    Ok(())
}

/// The file `path` names after following symlinks, even a dangling one.
fn resolve_symlinks(path: &Path) -> anyhow::Result<PathBuf> {
    let mut resolved = path.to_path_buf();
    for _ in 0..MAX_SYMLINKS {
        match std::fs::read_link(&resolved) {
            // A relative link is relative to its directory; an absolute one replaces it.
            Ok(link) => resolved = resolved.parent().unwrap_or(Path::new("")).join(link),
            Err(_) => return Ok(resolved),
        }
    }
    anyhow::bail!("too many symlinks at {}", path.display())
}

/// Moves the unreadable file `path` aside to `<name>.corrupt-<unix ms>`, logging `error`,
/// so the next save does not destroy what it held. Returns the new path.
pub fn move_aside_corrupt(path: &Path, error: &dyn Display) -> Option<PathBuf> {
    let mut name = path.file_name()?.to_os_string();
    name.push(format!(".corrupt-{}", zp_ai::now_ms()));
    let backup = path.with_file_name(name);
    match std::fs::rename(path, &backup) {
        Ok(()) => {
            tracing::warn!(
                path = %path.display(),
                backup = %backup.display(),
                %error,
                "moved an invalid file aside"
            );
            Some(backup)
        }
        Err(rename_error) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                %rename_error,
                "ignoring an invalid file that could not be moved aside"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn write_atomic_creates_and_replaces_without_leftovers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/settings.json");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "two");
        assert_eq!(entries(&dir.path().join("nested")), ["settings.json"]);
    }

    #[test]
    fn write_atomic_failure_keeps_the_target_and_removes_the_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        // A directory cannot be replaced by a file.
        let path = dir.path().join("settings.json");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("inside"), "kept").unwrap();
        assert!(write_atomic(&path, b"new").is_err());
        assert_eq!(entries(dir.path()), ["settings.json"]);
        assert_eq!(
            std::fs::read_to_string(path.join("inside")).unwrap(),
            "kept"
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_keeps_symlinks_and_permissions() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let dotfiles = dir.path().join("dotfiles");
        std::fs::create_dir(&dotfiles).unwrap();
        let real = dotfiles.join("zp-settings.json");
        std::fs::write(&real, "old").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o640)).unwrap();
        let link = dir.path().join("settings.json");
        std::os::unix::fs::symlink("dotfiles/zp-settings.json", &link).unwrap();

        write_atomic(&link, b"new").unwrap();
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "new");
        let mode = std::fs::metadata(&real).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o640);
        assert_eq!(entries(&dotfiles), ["zp-settings.json"]);
    }

    #[test]
    fn corrupt_files_are_moved_aside() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, "not json").unwrap();
        let backup = move_aside_corrupt(&path, &"invalid").unwrap();
        assert!(!path.exists());
        assert_eq!(backup.parent(), Some(dir.path()));
        let name = backup.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("state.json.corrupt-"), "{name}");
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), "not json");
    }
}
