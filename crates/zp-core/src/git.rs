//! Working tree status and `HEAD` contents, from the `git` command line.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, bail};

/// The state of a changed file in the working tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitFileStatus {
    Modified,
    Added,
    Deleted,
    Renamed,
    Untracked,
    Conflicted,
}

impl GitFileStatus {
    /// One-letter code, as in `git status --short`.
    pub fn letter(self) -> &'static str {
        match self {
            Self::Modified => "M",
            Self::Added => "A",
            Self::Deleted => "D",
            Self::Renamed => "R",
            Self::Untracked => "U",
            Self::Conflicted => "!",
        }
    }
}

/// A changed file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitFile {
    /// Absolute path.
    pub path: PathBuf,
    /// Path relative to the repository root (as git names it).
    pub repo_path: String,
    /// The previous path of a rename (relative to the repository root).
    pub renamed_from: Option<String>,
    pub status: GitFileStatus,
}

/// Changed files under a project directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitStatus {
    /// Repository root.
    pub root: PathBuf,
    pub files: Vec<GitFile>,
}

fn git(dir: &Path, args: &[&str]) -> anyhow::Result<std::process::Output> {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .context("could not run git")
}

/// The repository root containing `dir`, if any.
pub fn repo_root(dir: &Path) -> Option<PathBuf> {
    let output = git(dir, &["rev-parse", "--show-toplevel"]).ok()?;
    if !output.status.success() {
        return None;
    }
    let root = String::from_utf8(output.stdout).ok()?;
    let root = PathBuf::from(root.trim_end_matches(['\n', '\r']));
    Some(crate::project::normalize_project_path(&root).unwrap_or(root))
}

/// The changed files under `dir` (including untracked ones); `None` outside a repository.
pub fn status(dir: &Path) -> anyhow::Result<Option<GitStatus>> {
    let Some(root) = repo_root(dir) else {
        return Ok(None);
    };
    let output = git(
        dir,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--",
            ".",
        ],
    )?;
    if !output.status.success() {
        bail!(
            "git status failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut fields = text.split('\0').filter(|field| !field.is_empty());
    let mut files = Vec::new();
    while let Some(field) = fields.next() {
        if field.len() < 4 {
            continue;
        }
        let (code, path) = field.split_at(3);
        let code = code.as_bytes();
        let (x, y) = (code[0], code[1]);
        let status = match (x, y) {
            (b'?', b'?') => GitFileStatus::Untracked,
            (b'U', _) | (_, b'U') | (b'A', b'A') | (b'D', b'D') => GitFileStatus::Conflicted,
            (b'R', _) | (_, b'R') => GitFileStatus::Renamed,
            (b'A', _) => GitFileStatus::Added,
            (b'D', _) | (_, b'D') => GitFileStatus::Deleted,
            _ => GitFileStatus::Modified,
        };
        let renamed_from = if x == b'R' || x == b'C' {
            fields.next().map(str::to_owned)
        } else {
            None
        };
        files.push(GitFile {
            path: root.join(path),
            repo_path: path.to_owned(),
            renamed_from,
            status,
        });
    }
    files.sort_by(|a, b| a.repo_path.cmp(&b.repo_path));
    Ok(Some(GitStatus { root, files }))
}

/// The content of `repo_path` at `HEAD`; `None` if it is not in `HEAD` (or not text).
pub fn head_content(root: &Path, repo_path: &str) -> anyhow::Result<Option<String>> {
    let output = git(root, &["show", &format!("HEAD:{repo_path}")])?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(String::from_utf8(output.stdout).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            status.status.success(),
            "{}",
            String::from_utf8_lossy(&status.stderr)
        );
    }

    #[test]
    fn status_and_head() {
        let dir = tempfile::tempdir().unwrap();
        let root = crate::project::normalize_project_path(dir.path()).unwrap();
        assert_eq!(status(&root).unwrap(), None);
        run(&root, &["init", "-q"]);
        std::fs::write(root.join("kept.txt"), "one\n").unwrap();
        std::fs::write(root.join("gone.txt"), "bye\n").unwrap();
        std::fs::write(root.join("old name.txt"), "moving\n").unwrap();
        run(&root, &["add", "."]);
        run(&root, &["commit", "-q", "-m", "init"]);

        std::fs::write(root.join("kept.txt"), "two\n").unwrap();
        std::fs::remove_file(root.join("gone.txt")).unwrap();
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/new.txt"), "new\n").unwrap();
        run(&root, &["mv", "old name.txt", "new name.txt"]);

        let status = status(&root).unwrap().unwrap();
        assert_eq!(status.root, root);
        let shape: Vec<(&str, GitFileStatus)> = status
            .files
            .iter()
            .map(|f| (f.repo_path.as_str(), f.status))
            .collect();
        assert_eq!(
            shape,
            [
                ("gone.txt", GitFileStatus::Deleted),
                ("kept.txt", GitFileStatus::Modified),
                ("new name.txt", GitFileStatus::Renamed),
                ("sub/new.txt", GitFileStatus::Untracked),
            ]
        );
        assert_eq!(
            status.files[2].renamed_from.as_deref(),
            Some("old name.txt")
        );
        assert_eq!(status.files[1].path, root.join("kept.txt"));
        assert_eq!(
            head_content(&root, "kept.txt").unwrap().as_deref(),
            Some("one\n")
        );
        assert_eq!(head_content(&root, "sub/new.txt").unwrap(), None);

        // Only files under the given directory.
        let sub = status_of(&root.join("sub"));
        assert_eq!(sub, ["sub/new.txt"]);
    }

    fn status_of(dir: &Path) -> Vec<String> {
        status(dir)
            .unwrap()
            .unwrap()
            .files
            .into_iter()
            .map(|f| f.repo_path)
            .collect()
    }
}
