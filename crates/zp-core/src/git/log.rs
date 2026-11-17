//! History: commits, the files a commit changed, and file contents at a revision.

use std::path::Path;

use super::{GitFileStatus, git, git_ok};

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

/// The text of `repo_path` at `revision` (`HEAD`, a sha, `sha^`), or in the index when
/// `revision` is empty; `None` if it does not exist there or is not text.
pub fn file_at(root: &Path, revision: &str, repo_path: &str) -> anyhow::Result<Option<String>> {
    let output = git(root, &["show", &format!("{revision}:{repo_path}")])?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(String::from_utf8(output.stdout).ok())
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
        assert_eq!(
            file_at(&root, sha, "a.txt").unwrap().as_deref(),
            Some("two\n")
        );
        assert_eq!(
            file_at(&root, &format!("{sha}^"), "a.txt")
                .unwrap()
                .as_deref(),
            Some("one\n")
        );
        assert_eq!(file_at(&root, "HEAD", "missing.txt").unwrap(), None);
        std::fs::write(root.join("a.txt"), "staged\n").unwrap();
        run(&root, &["add", "a.txt"]);
        assert_eq!(
            file_at(&root, "", "a.txt").unwrap().as_deref(),
            Some("staged\n")
        );
    }
}
