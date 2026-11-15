//! Project directories: identity, display name and git branch.

use std::path::{Component, Path, PathBuf};

/// The canonical form of a project directory, used as its identity (one window per
/// project). Symlinks are resolved; on Windows the `\\?\` prefix is dropped.
pub fn normalize_project_path(path: &Path) -> std::io::Result<PathBuf> {
    let canonical = std::fs::canonicalize(path)?;
    if !canonical.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotADirectory,
            format!("{} is not a directory", path.display()),
        ));
    }
    Ok(strip_verbatim(canonical))
}

/// Drops the Windows `\\?\` prefix that `canonicalize` adds, when the path is otherwise
/// ordinary.
fn strip_verbatim(path: PathBuf) -> PathBuf {
    if cfg!(windows) {
        let text = path.to_string_lossy();
        if let Some(rest) = text.strip_prefix(r"\\?\")
            && !rest.starts_with("UNC\\")
        {
            return PathBuf::from(rest);
        }
    }
    path
}

/// The name shown for a project: its directory name.
pub fn project_name(path: &Path) -> String {
    path.components()
        .rev()
        .find_map(|component| match component {
            Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
            _ => None,
        })
        .unwrap_or_else(|| path.display().to_string())
}

/// `path` with the home directory shortened to `~`.
pub fn tilde_path(path: &Path) -> String {
    if let Some(home) = dirs::home_dir()
        && let Ok(rest) = path.strip_prefix(&home)
    {
        return if rest.as_os_str().is_empty() {
            "~".into()
        } else {
            format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display())
        };
    }
    path.display().to_string()
}

/// The checked-out branch of the git repository containing `dir` (or a short commit id
/// when detached). `None` outside a repository.
pub fn git_branch(dir: &Path) -> Option<String> {
    let git_dir = find_git_dir(dir)?;
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    match head.strip_prefix("ref:") {
        Some(reference) => {
            let reference = reference.trim();
            Some(
                reference
                    .strip_prefix("refs/heads/")
                    .unwrap_or(reference)
                    .to_owned(),
            )
        }
        None if head.len() >= 7 && head.bytes().all(|b| b.is_ascii_hexdigit()) => {
            Some(head[..7].to_owned())
        }
        None => None,
    }
}

/// The git directory for `dir`: `.git` itself, or the target of a `.git` file (worktrees
/// and submodules).
fn find_git_dir(dir: &Path) -> Option<PathBuf> {
    for ancestor in dir.ancestors() {
        let dot_git = ancestor.join(".git");
        if dot_git.is_dir() {
            return Some(dot_git);
        }
        if dot_git.is_file() {
            let text = std::fs::read_to_string(&dot_git).ok()?;
            let target = text.trim().strip_prefix("gitdir:")?.trim();
            let target = Path::new(target);
            return Some(if target.is_absolute() {
                target.to_path_buf()
            } else {
                ancestor.join(target)
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_from_head_files() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        let nested = repo.join("src").join("deep");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(git_branch(&nested), None);

        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/feature/x\n").unwrap();
        assert_eq!(git_branch(&nested).as_deref(), Some("feature/x"));

        std::fs::write(
            repo.join(".git/HEAD"),
            "0123456789abcdef0123456789abcdef01234567\n",
        )
        .unwrap();
        assert_eq!(git_branch(&repo).as_deref(), Some("0123456"));

        // A linked worktree: `.git` is a file pointing at the real git dir.
        let worktree = root.path().join("wt");
        let real = root.path().join("real-git");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("HEAD"), "ref: refs/heads/wt-branch").unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", real.display()),
        )
        .unwrap();
        assert_eq!(git_branch(&worktree).as_deref(), Some("wt-branch"));
    }

    #[test]
    fn names_and_normalization() {
        assert_eq!(project_name(Path::new("/work/my-app")), "my-app");
        assert_eq!(project_name(Path::new("/work/my-app/")), "my-app");
        let dir = tempfile::tempdir().unwrap();
        let normalized = normalize_project_path(&dir.path().join(".")).unwrap();
        assert_eq!(normalized, std::fs::canonicalize(dir.path()).unwrap());
        let file = dir.path().join("f");
        std::fs::write(&file, "").unwrap();
        assert!(normalize_project_path(&file).is_err());
        assert!(normalize_project_path(&dir.path().join("missing")).is_err());
        if let Some(home) = dirs::home_dir() {
            assert_eq!(
                tilde_path(&home.join("x")),
                format!("~{}x", std::path::MAIN_SEPARATOR)
            );
        }
    }
}
