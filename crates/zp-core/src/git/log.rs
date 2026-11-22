//! History: commits, the files a commit changed, and file contents at a revision.

use std::path::Path;

use super::{GitFileStatus, git, git_ok};
use crate::diff::MAX_DIFF_BYTES;

/// A commit of the history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    pub sha: String,
    pub short_sha: String,
    pub author: String,
    /// Author time in seconds.
    pub time: i64,
    pub summary: String,
    /// Branch and tag names pointing at it (`HEAD -> main, origin/main, tag: v1`).
    pub refs: String,
}

/// A file changed by a commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitFile {
    pub repo_path: String,
    pub renamed_from: Option<String>,
    pub status: GitFileStatus,
}

/// The last `limit` commits of `HEAD` (none before the first commit).
pub fn history(root: &Path, limit: usize) -> anyhow::Result<Vec<Commit>> {
    let has_head =
        git(root, &["rev-parse", "--verify", "-q", "HEAD"]).is_ok_and(|o| o.status.success());
    if !has_head {
        return Ok(Vec::new());
    }
    let limit = format!("-n{limit}");
    let text = git_ok(
        root,
        &[
            "log",
            &limit,
            "--format=%H%x1f%h%x1f%an%x1f%at%x1f%s%x1f%D%x1e",
        ],
    )?;
    Ok(text
        .split('\x1e')
        .filter_map(|record| {
            let fields: Vec<&str> = record.trim_start_matches('\n').split('\x1f').collect();
            let [sha, short_sha, author, time, summary, refs] = fields[..] else {
                return None;
            };
            Some(Commit {
                sha: sha.to_owned(),
                short_sha: short_sha.to_owned(),
                author: author.to_owned(),
                time: time.parse().unwrap_or(0),
                summary: summary.to_owned(),
                refs: refs.to_owned(),
            })
        })
        .collect())
}

/// The files `sha` changed, against its first parent (everything for a root commit).
pub fn commit_files(root: &Path, sha: &str) -> anyhow::Result<Vec<CommitFile>> {
    let text = git_ok(
        root,
        &[
            "diff-tree",
            "--no-commit-id",
            "-r",
            "-z",
            "-M",
            "--root",
            "--first-parent",
            "-m",
            "--name-status",
            sha,
        ],
    )?;
    let mut fields = text.split('\0').filter(|field| !field.is_empty());
    let mut files = Vec::new();
    while let Some(code) = fields.next() {
        let letter = code.as_bytes().first().copied().unwrap_or(b'M');
        let renamed = matches!(letter, b'R' | b'C');
        let first = fields.next().unwrap_or_default().to_owned();
        let (repo_path, renamed_from) = if renamed {
            (fields.next().unwrap_or_default().to_owned(), Some(first))
        } else {
            (first, None)
        };
        files.push(CommitFile {
            repo_path,
            renamed_from: renamed_from.filter(|_| letter == b'R'),
            status: GitFileStatus::from_code(letter).unwrap_or(GitFileStatus::Modified),
        });
    }
    Ok(files)
}

/// One side of a file diff, read only when it is small enough to compare.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileText {
    /// The file does not exist there, or is not text.
    Missing,
    Text(String),
    /// Larger than [`MAX_DIFF_BYTES`], so not read.
    TooLarge,
}

impl FileText {
    /// The text, if it was read.
    pub fn into_text(self) -> Option<String> {
        match self {
            Self::Text(text) => Some(text),
            Self::Missing | Self::TooLarge => None,
        }
    }
}

/// The text of `repo_path` at `revision` (`HEAD`, a sha, `sha^`), or in the index when
/// `revision` is empty.
pub fn file_at(root: &Path, revision: &str, repo_path: &str) -> anyhow::Result<FileText> {
    let object = format!("{revision}:{repo_path}");
    let size = git(root, &["cat-file", "-s", &object])?;
    if !size.status.success() {
        return Ok(FileText::Missing);
    }
    let size: u64 = String::from_utf8_lossy(&size.stdout)
        .trim()
        .parse()
        .unwrap_or(u64::MAX);
    if size > MAX_DIFF_BYTES as u64 {
        return Ok(FileText::TooLarge);
    }
    let output = git(root, &["show", &object])?;
    if !output.status.success() {
        return Ok(FileText::Missing);
    }
    Ok(String::from_utf8(output.stdout).map_or(FileText::Missing, FileText::Text))
}

/// The text of the working tree file at `path`.
pub fn file_in_worktree(path: &Path) -> FileText {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_file() && metadata.len() > MAX_DIFF_BYTES as u64 => {
            FileText::TooLarge
        }
        Ok(_) => std::fs::read_to_string(path).map_or(FileText::Missing, FileText::Text),
        Err(_) => FileText::Missing,
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{repo, run};
    use super::*;

    #[test]
    fn history_files_and_contents() {
        let (_dir, root) = repo();
        assert!(history(&root, 10).unwrap().is_empty());
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        std::fs::write(root.join("old.txt"), "moving\n").unwrap();
        run(&root, &["add", "."]);
        run(&root, &["commit", "-q", "-m", "first"]);
        std::fs::write(root.join("a.txt"), "two\n").unwrap();
        std::fs::write(root.join("b.txt"), "new\n").unwrap();
        run(&root, &["mv", "old.txt", "new.txt"]);
        run(&root, &["add", "."]);
        run(&root, &["commit", "-q", "-m", "second: with | odd ; text"]);
        run(&root, &["tag", "v1"]);

        let commits = history(&root, 10).unwrap();
        let summaries: Vec<&str> = commits.iter().map(|c| c.summary.as_str()).collect();
        assert_eq!(summaries, ["second: with | odd ; text", "first"]);
        assert_eq!(commits[0].author, "t");
        assert!(commits[0].refs.contains("HEAD -> main") && commits[0].refs.contains("tag: v1"));
        assert!(commits[0].sha.starts_with(&commits[0].short_sha));
        assert_eq!(history(&root, 1).unwrap().len(), 1);

        let files = commit_files(&root, &commits[0].sha).unwrap();
        let shape: Vec<(&str, GitFileStatus, Option<&str>)> = files
            .iter()
            .map(|f| (f.repo_path.as_str(), f.status, f.renamed_from.as_deref()))
            .collect();
        assert_eq!(
            shape,
            [
                ("a.txt", GitFileStatus::Modified, None),
                ("b.txt", GitFileStatus::Added, None),
                ("new.txt", GitFileStatus::Renamed, Some("old.txt")),
            ]
        );
        let root_files = commit_files(&root, &commits[1].sha).unwrap();
        assert_eq!(root_files.len(), 2);
        assert!(root_files.iter().all(|f| f.status == GitFileStatus::Added));

        let sha = &commits[0].sha;
        let text = |s: &str| FileText::Text(s.to_owned());
        assert_eq!(file_at(&root, sha, "a.txt").unwrap(), text("two\n"));
        assert_eq!(
            file_at(&root, &format!("{sha}^"), "a.txt").unwrap(),
            text("one\n")
        );
        assert_eq!(
            file_at(&root, "HEAD", "missing.txt").unwrap(),
            FileText::Missing
        );
        std::fs::write(root.join("a.txt"), "staged\n").unwrap();
        run(&root, &["add", "a.txt"]);
        assert_eq!(file_at(&root, "", "a.txt").unwrap(), text("staged\n"));
        assert_eq!(file_in_worktree(&root.join("a.txt")), text("staged\n"));
        assert_eq!(
            file_in_worktree(&root.join("missing.txt")),
            FileText::Missing
        );
    }

    #[test]
    fn files_too_large_to_compare_are_not_read() {
        let (_dir, root) = repo();
        let limit = MAX_DIFF_BYTES;
        std::fs::write(root.join("big.txt"), "x".repeat(limit + 1)).unwrap();
        std::fs::write(root.join("edge.txt"), "x".repeat(limit)).unwrap();
        run(&root, &["add", "."]);
        // Compared without printing megabytes when a file is read after all.
        let too_large = |file: FileText| file == FileText::TooLarge;
        assert!(too_large(file_at(&root, "", "big.txt").unwrap()));
        run(&root, &["commit", "-q", "-m", "big"]);
        assert!(too_large(file_at(&root, "HEAD", "big.txt").unwrap()));
        assert!(too_large(file_in_worktree(&root.join("big.txt"))));
        assert_eq!(FileText::TooLarge.into_text(), None);

        // At the limit the file is still read.
        let edge = file_at(&root, "HEAD", "edge.txt").unwrap().into_text();
        assert_eq!(edge.map(|text| text.len()), Some(limit));
        let edge = file_in_worktree(&root.join("edge.txt")).into_text();
        assert_eq!(edge.map(|text| text.len()), Some(limit));
    }
}
