//! Git through the `git` command line: working tree and index status, staging,
//! commits, branches, remotes and history.

mod log;
mod ops;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, bail};

pub use log::{Commit, CommitFile, commit_files, file_at, history};
pub use ops::{
    Branch, CommitOptions, branches, commit, create_branch, discard, fetch, last_commit_message,
    pull, push, stage, stage_all, stage_tracked, switch_branch, uncommit, unstage, unstage_all,
};

/// How a file differs on one side (index vs `HEAD`, or working tree vs index).
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

    /// From a porcelain status letter (`.` is unchanged).
    fn from_code(code: u8) -> Option<Self> {
        match code {
            b'M' | b'T' => Some(Self::Modified),
            b'A' | b'C' => Some(Self::Added),
            b'D' => Some(Self::Deleted),
            b'R' => Some(Self::Renamed),
            b'U' => Some(Self::Conflicted),
            _ => None,
        }
    }
}

/// A changed file, with what is staged and what is not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitFile {
    /// Absolute path.
    pub path: PathBuf,
    /// Path relative to the repository root (as git names it).
    pub repo_path: String,
    /// The previous path of a staged rename (relative to the repository root).
    pub renamed_from: Option<String>,
    /// Index vs `HEAD`; `None` when nothing is staged.
    pub staged: Option<GitFileStatus>,
    /// Working tree vs index; `None` when everything is staged.
    pub unstaged: Option<GitFileStatus>,
    /// Lines added and removed in the index.
    pub staged_stats: (usize, usize),
    /// Lines added and removed in the working tree (all lines of an untracked file).
    pub unstaged_stats: (usize, usize),
}

impl GitFile {
    /// The status shown for the file as a whole.
    pub fn status(&self) -> GitFileStatus {
        match (self.staged, self.unstaged) {
            (_, Some(GitFileStatus::Conflicted)) => GitFileStatus::Conflicted,
            (Some(staged), _) => staged,
            (None, Some(unstaged)) => unstaged,
            (None, None) => GitFileStatus::Modified,
        }
    }

    pub fn is_untracked(&self) -> bool {
        self.unstaged == Some(GitFileStatus::Untracked)
    }
}

/// The repository state under a project directory.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitStatus {
    /// Repository root.
    pub root: PathBuf,
    /// The checked-out branch; `None` when `HEAD` is detached.
    pub branch: Option<String>,
    /// The `HEAD` commit; `None` before the first commit.
    pub head: Option<String>,
    /// The branch's upstream (`origin/main`), if it has one.
    pub upstream: Option<String>,
    /// Commits on the branch but not its upstream, and the other way round.
    pub ahead: usize,
    pub behind: usize,
    /// Whether the repository has a remote (to publish a branch to).
    pub has_remote: bool,
    /// Changed files under the project directory, by path.
    pub files: Vec<GitFile>,
    /// The `HEAD` commit's summary line.
    pub last_commit: Option<String>,
}

impl GitStatus {
    /// Files with staged changes.
    pub fn staged(&self) -> impl Iterator<Item = &GitFile> {
        self.files.iter().filter(|file| file.staged.is_some())
    }

    /// Files with changes that are not staged (including untracked files).
    pub fn unstaged(&self) -> impl Iterator<Item = &GitFile> {
        self.files.iter().filter(|file| file.unstaged.is_some())
    }
}

/// Runs git in `dir`, never waiting for a terminal prompt or an editor.
pub(crate) fn git(dir: &Path, args: &[&str]) -> anyhow::Result<std::process::Output> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_LITERAL_PATHSPECS", "1")
        .env("GIT_EDITOR", "true")
        .env("LC_ALL", "C");
    if std::env::var_os("GIT_SSH_COMMAND").is_none() {
        command.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command.output().context("could not run git")
}

/// Runs git and returns its output, or an error with what git said.
pub(crate) fn git_ok(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let output = git(dir, args)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let message = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        bail!(
            "git {} failed: {}",
            args.first().copied().unwrap_or_default(),
            message
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
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

/// Lines added and removed per path, from `git diff --numstat -z`.
fn numstat(dir: &Path, cached: bool) -> anyhow::Result<HashMap<String, (usize, usize)>> {
    let mut args = vec!["diff", "--numstat", "-z", "-M"];
    if cached {
        args.push("--cached");
    }
    args.extend(["--", "."]);
    let text = git_ok(dir, &args)?;
    let mut stats = HashMap::new();
    let mut fields = text.split('\0');
    while let Some(field) = fields.next() {
        let mut parts = field.splitn(3, '\t');
        let (Some(added), Some(removed), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        // A rename: the path field is empty and the old and new paths follow.
        let path = if path.is_empty() {
            let _old = fields.next();
            fields.next().unwrap_or_default()
        } else {
            path
        };
        // Binary files show `-`.
        let count = |n: &str| n.parse().unwrap_or(0);
        stats.insert(path.to_owned(), (count(added), count(removed)));
    }
    Ok(stats)
}

/// Lines of a new file (0 when it is binary or large).
fn line_count(path: &Path) -> usize {
    const LIMIT: u64 = 4 * 1024 * 1024;
    if std::fs::metadata(path).map_or(true, |m| !m.is_file() || m.len() > LIMIT) {
        return 0;
    }
    match std::fs::read(path) {
        Ok(bytes) if !bytes.contains(&0) => {
            let lines = bytecount_newlines(&bytes);
            lines + usize::from(!bytes.is_empty() && !bytes.ends_with(b"\n"))
        }
        _ => 0,
    }
}

fn bytecount_newlines(bytes: &[u8]) -> usize {
    bytes.iter().filter(|&&b| b == b'\n').count()
}

/// The repository state under `dir`: branch, upstream and changed files (including
/// untracked ones), with line counts; `None` outside a repository.
pub fn status(dir: &Path) -> anyhow::Result<Option<GitStatus>> {
    let Some(root) = repo_root(dir) else {
        return Ok(None);
    };
    let text = git_ok(
        dir,
        &[
            "status",
            "--porcelain=v2",
            "--branch",
            "-z",
            "--untracked-files=all",
            "--",
            ".",
        ],
    )?;
    let mut status = parse_status(&root, &text);
    let staged = numstat(dir, true)?;
    let unstaged = numstat(dir, false)?;
    for file in &mut status.files {
        if file.staged.is_some() {
            file.staged_stats = staged.get(&file.repo_path).copied().unwrap_or_default();
        }
        file.unstaged_stats = match file.unstaged {
            Some(GitFileStatus::Untracked) => (line_count(&file.path), 0),
            Some(_) => unstaged.get(&file.repo_path).copied().unwrap_or_default(),
            None => (0, 0),
        };
    }
    status.has_remote = !git_ok(&root, &["remote"])?.trim().is_empty();
    if status.head.is_some() {
        status.last_commit = git_ok(&root, &["log", "-1", "--format=%s"])
            .ok()
            .map(|s| s.trim_end().to_owned());
    }
    Ok(Some(status))
}

/// Parses `git status --porcelain=v2 --branch -z`.
fn parse_status(root: &Path, text: &str) -> GitStatus {
    let mut status = GitStatus {
        root: root.to_path_buf(),
        ..GitStatus::default()
    };
    let mut fields = text.split('\0').filter(|field| !field.is_empty());
    while let Some(field) = fields.next() {
        if let Some(header) = field.strip_prefix("# ") {
            let (key, value) = header.split_once(' ').unwrap_or((header, ""));
            match key {
                "branch.oid" if value != "(initial)" => status.head = Some(value.to_owned()),
                "branch.head" if value != "(detached)" => status.branch = Some(value.to_owned()),
                "branch.upstream" => status.upstream = Some(value.to_owned()),
                "branch.ab" => {
                    let mut counts = value
                        .split(' ')
                        .map(|n| n.trim_start_matches(['+', '-']).parse().unwrap_or(0));
                    status.ahead = counts.next().unwrap_or(0);
                    status.behind = counts.next().unwrap_or(0);
                }
                _ => {}
            }
            continue;
        }
        let (kind, rest) = field.split_at(1);
        let rest = rest.trim_start();
        let (staged, unstaged, path, renamed_from) = match kind {
            // `1 XY sub mH mI mW hH hI path`
            "1" => {
                let parts: Vec<&str> = rest.splitn(8, ' ').collect();
                let Some(path) = parts.get(7) else { continue };
                let xy = parts[0].as_bytes();
                (xy[0], xy[1], *path, None)
            }
            // `2 XY sub mH mI mW hH hI Xscore path`, then the original path.
            "2" => {
                let parts: Vec<&str> = rest.splitn(9, ' ').collect();
                let Some(path) = parts.get(8) else { continue };
                let xy = parts[0].as_bytes();
                (xy[0], xy[1], *path, fields.next().map(str::to_owned))
            }
            // `u XY sub m1 m2 m3 mW h1 h2 h3 path`: a merge conflict.
            "u" => {
                let parts: Vec<&str> = rest.splitn(10, ' ').collect();
                let Some(path) = parts.get(9) else { continue };
                (b'.', b'U', *path, None)
            }
            "?" => (b'.', b'?', rest, None),
            _ => continue,
        };
        status.files.push(GitFile {
            path: root.join(path),
            repo_path: path.to_owned(),
            renamed_from,
            staged: GitFileStatus::from_code(staged),
            unstaged: if unstaged == b'?' {
                Some(GitFileStatus::Untracked)
            } else {
                GitFileStatus::from_code(unstaged)
            },
            staged_stats: (0, 0),
            unstaged_stats: (0, 0),
        });
    }
    status.files.sort_by(|a, b| a.repo_path.cmp(&b.repo_path));
    status
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Runs git with a test identity; panics on failure.
    pub(crate) fn run(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// A new repository with a test identity in its config.
    pub(crate) fn repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = crate::project::normalize_project_path(dir.path()).unwrap();
        run(&root, &["init", "-q"]);
        run(&root, &["config", "user.name", "t"]);
        run(&root, &["config", "user.email", "t@t"]);
        run(&root, &["config", "commit.gpgsign", "false"]);
        run(&root, &["config", "core.autocrlf", "false"]);
        (dir, root)
    }

    fn shape(status: &GitStatus) -> Vec<(&str, Option<GitFileStatus>, Option<GitFileStatus>)> {
        status
            .files
            .iter()
            .map(|f| (f.repo_path.as_str(), f.staged, f.unstaged))
            .collect()
    }

    #[test]
    fn status_sides_stats_and_branch() {
        use GitFileStatus::*;
        let dir = tempfile::tempdir().unwrap();
        let plain = crate::project::normalize_project_path(dir.path()).unwrap();
        assert_eq!(status(&plain).unwrap(), None);

        let (_dir, root) = repo();
        // Before the first commit everything is new.
        std::fs::write(root.join("kept.txt"), "one\n").unwrap();
        run(&root, &["add", "kept.txt"]);
        let initial = status(&root).unwrap().unwrap();
        assert_eq!(
            (initial.head.as_deref(), initial.last_commit.as_deref()),
            (None, None)
        );
        assert_eq!(shape(&initial), [("kept.txt", Some(Added), None)]);
        assert_eq!(initial.files[0].staged_stats, (1, 0));

        std::fs::write(root.join("gone.txt"), "bye\n").unwrap();
        std::fs::write(root.join("old name.txt"), "moving\n").unwrap();
        run(&root, &["add", "."]);
        run(&root, &["commit", "-q", "-m", "init"]);

        // Staged and then changed again; deleted; renamed; untracked in a folder.
        std::fs::write(root.join("kept.txt"), "two\n").unwrap();
        run(&root, &["add", "kept.txt"]);
        std::fs::write(root.join("kept.txt"), "two\nthree\nfour\n").unwrap();
        std::fs::remove_file(root.join("gone.txt")).unwrap();
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/new.txt"), "new\nfile").unwrap();
        run(&root, &["mv", "old name.txt", "new name.txt"]);

        let status = status(&root).unwrap().unwrap();
        assert_eq!(status.root, root);
        assert_eq!(status.branch.as_deref(), Some("main"));
        assert!(status.head.is_some());
        assert_eq!(status.last_commit.as_deref(), Some("init"));
        assert_eq!(
            (status.upstream.as_deref(), status.has_remote),
            (None, false)
        );
        assert_eq!(
            shape(&status),
            [
                ("gone.txt", None, Some(Deleted)),
                ("kept.txt", Some(Modified), Some(Modified)),
                ("new name.txt", Some(Renamed), None),
                ("sub/new.txt", None, Some(Untracked)),
            ]
        );
        let kept = &status.files[1];
        assert_eq!((kept.staged_stats, kept.unstaged_stats), ((1, 1), (2, 0)));
        assert_eq!(status.files[0].unstaged_stats, (0, 1));
        assert_eq!(
            status.files[2].renamed_from.as_deref(),
            Some("old name.txt")
        );
        assert_eq!(status.files[3].unstaged_stats, (2, 0));
        assert_eq!(status.files[1].path, root.join("kept.txt"));
        assert_eq!(status.staged().count(), 2);
        assert_eq!(status.unstaged().count(), 3);

        // Only files under the given directory.
        let sub = status_of(&root.join("sub"));
        assert_eq!(sub, ["sub/new.txt"]);
    }

    #[test]
    fn conflicts_and_detached_head() {
        let (_dir, root) = repo();
        std::fs::write(root.join("a.txt"), "base\n").unwrap();
        run(&root, &["add", "."]);
        run(&root, &["commit", "-q", "-m", "base"]);
        run(&root, &["switch", "-q", "-c", "other"]);
        std::fs::write(root.join("a.txt"), "other\n").unwrap();
        run(&root, &["commit", "-q", "-am", "other"]);
        run(&root, &["switch", "-q", "main"]);
        std::fs::write(root.join("a.txt"), "mine\n").unwrap();
        run(&root, &["commit", "-q", "-am", "mine"]);
        let merge = Command::new("git")
            .args(["-C"])
            .arg(&root)
            .args(["merge", "-q", "other"])
            .output()
            .unwrap();
        assert!(!merge.status.success());
        let status = status(&root).unwrap().unwrap();
        assert_eq!(status.files[0].status(), GitFileStatus::Conflicted);
        assert_eq!(status.files[0].staged, None);
        run(&root, &["merge", "--abort"]);

        run(&root, &["switch", "-q", "--detach", "HEAD~1"]);
        let detached = super::status(&root).unwrap().unwrap();
        assert_eq!(detached.branch, None);
        assert!(detached.head.is_some());
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
