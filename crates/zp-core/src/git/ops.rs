//! Changing the repository: staging, discarding, committing, branches and remotes.

use std::path::Path;

use anyhow::bail;

use super::{git, git_ok, git_remote_ok};

/// Whether the repository has a commit yet.
fn has_head(root: &Path) -> bool {
    git(root, &["rev-parse", "--verify", "-q", "HEAD"]).is_ok_and(|o| o.status.success())
}

fn with_paths<'a>(args: &[&'a str], paths: &'a [String]) -> Vec<&'a str> {
    let mut all = args.to_vec();
    all.push("--");
    all.extend(paths.iter().map(String::as_str));
    all
}

/// Stages `paths` (relative to the repository root): changes, new files and deletions.
pub fn stage(root: &Path, paths: &[String]) -> anyhow::Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    git_ok(root, &with_paths(&["add", "-A"], paths))?;
    Ok(())
}

/// Unstages `paths`, keeping their working tree changes.
pub fn unstage(root: &Path, paths: &[String]) -> anyhow::Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    if has_head(root) {
        git_ok(root, &with_paths(&["reset", "-q"], paths))?;
    } else {
        git_ok(
            root,
            &with_paths(&["rm", "--cached", "-r", "-q", "--ignore-unmatch"], paths),
        )?;
    }
    Ok(())
}

/// Stages every change under `dir`.
pub fn stage_all(dir: &Path) -> anyhow::Result<()> {
    git_ok(dir, &["add", "-A", "--", "."])?;
    Ok(())
}

/// Stages the changes of tracked files under `dir` (not new files).
pub fn stage_tracked(dir: &Path) -> anyhow::Result<()> {
    git_ok(dir, &["add", "-u", "--", "."])?;
    Ok(())
}

/// Unstages every change under `dir`.
pub fn unstage_all(dir: &Path) -> anyhow::Result<()> {
    if has_head(dir) {
        git_ok(dir, &["reset", "-q", "--", "."])?;
    } else {
        git_ok(
            dir,
            &["rm", "--cached", "-r", "-q", "--ignore-unmatch", "--", "."],
        )?;
    }
    Ok(())
}

/// Throws away the unstaged changes of tracked `paths` (back to the index). Untracked
/// files are the caller's to delete.
pub fn discard(root: &Path, paths: &[String]) -> anyhow::Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    git_ok(root, &with_paths(&["checkout", "-q"], paths))?;
    Ok(())
}

/// How to commit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CommitOptions {
    /// Replace the last commit (keeping its message when the new one is empty).
    pub amend: bool,
    /// Add a `Signed-off-by` trailer.
    pub signoff: bool,
    /// Skip the pre-commit and commit-msg hooks.
    pub no_verify: bool,
}

/// Commits the index with `message`.
pub fn commit(root: &Path, message: &str, options: CommitOptions) -> anyhow::Result<()> {
    let message = message.trim();
    let mut args = vec!["commit", "-q"];
    if options.signoff {
        args.push("--signoff");
    }
    if options.no_verify {
        args.push("--no-verify");
    }
    if options.amend {
        args.push("--amend");
        if message.is_empty() {
            args.push("--no-edit");
        }
    } else if message.is_empty() {
        bail!("the commit message is empty");
    }
    if !message.is_empty() {
        args.extend(["-m", message]);
    }
    git_ok(root, &args)?;
    Ok(())
}

/// The full message of the last commit (`None` before the first commit).
pub fn last_commit_message(root: &Path) -> anyhow::Result<Option<String>> {
    if !has_head(root) {
        return Ok(None);
    }
    let message = git_ok(root, &["log", "-1", "--format=%B"])?;
    Ok(Some(message.trim_end().to_owned()))
}

/// Undoes the last commit, keeping its changes staged.
pub fn uncommit(root: &Path) -> anyhow::Result<()> {
    let has_parent =
        git(root, &["rev-parse", "--verify", "-q", "HEAD~1"]).is_ok_and(|o| o.status.success());
    if has_parent {
        git_ok(root, &["reset", "-q", "--soft", "HEAD~1"])?;
    } else {
        // The first commit: back to an unborn branch with everything staged.
        git_ok(root, &["update-ref", "-d", "HEAD"])?;
    }
    Ok(())
}

/// The remote to publish to: `origin`, else the only or first one.
fn default_remote(root: &Path) -> anyhow::Result<String> {
    let remotes = git_ok(root, &["remote"])?;
    let remotes: Vec<&str> = remotes.lines().filter(|r| !r.is_empty()).collect();
    match remotes.iter().find(|r| **r == "origin").or(remotes.first()) {
        Some(remote) => Ok((*remote).to_owned()),
        None => bail!("the repository has no remote"),
    }
}

/// Pushes the current branch; without an upstream, publishes it to the default remote.
pub fn push(root: &Path) -> anyhow::Result<()> {
    let upstream =
        git(root, &["rev-parse", "--abbrev-ref", "@{upstream}"]).is_ok_and(|o| o.status.success());
    if upstream {
        git_remote_ok(root, &["push", "-q"])?;
    } else {
        let remote = default_remote(root)?;
        git_remote_ok(root, &["push", "-q", "-u", &remote, "HEAD"])?;
    }
    Ok(())
}

/// Pulls into the current branch (merging or rebasing as configured).
pub fn pull(root: &Path) -> anyhow::Result<()> {
    git_remote_ok(root, &["pull", "-q", "--no-edit"])?;
    Ok(())
}

/// Fetches every remote.
pub fn fetch(root: &Path) -> anyhow::Result<()> {
    git_remote_ok(root, &["fetch", "-q", "--all", "--prune"])?;
    Ok(())
}

/// A branch to switch to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Branch {
    /// `main`, or `origin/feature` for a remote branch without a local one.
    pub name: String,
    pub remote: bool,
    pub current: bool,
    /// The last commit's time (seconds) and summary.
    pub time: i64,
    pub summary: String,
}

/// Local branches, then remote branches that have no local one; most recent first.
pub fn branches(root: &Path) -> anyhow::Result<Vec<Branch>> {
    let text = git_ok(
        root,
        &[
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(HEAD)%00%(refname)%00%(refname:short)%00%(committerdate:unix)%00%(contents:subject)",
            "refs/heads",
            "refs/remotes",
        ],
    )?;
    let mut local = Vec::new();
    let mut remote = Vec::new();
    for line in text.lines() {
        let parts: Vec<&str> = line.split('\0').collect();
        let [head, refname, short, time, summary] = parts[..] else {
            continue;
        };
        let branch = Branch {
            name: short.to_owned(),
            remote: refname.starts_with("refs/remotes/"),
            current: head == "*",
            time: time.parse().unwrap_or(0),
            summary: summary.to_owned(),
        };
        if branch.remote {
            // `origin/HEAD` is an alias, not a branch.
            if !refname.ends_with("/HEAD") {
                remote.push(branch);
            }
        } else {
            local.push(branch);
        }
    }
    remote.retain(|branch| {
        let short = branch.name.split_once('/').map_or("", |(_, name)| name);
        !local.iter().any(|local| local.name == short)
    });
    local.extend(remote);
    Ok(local)
}

/// Switches to `branch`; a remote branch (`origin/feature`) gets a local tracking branch.
pub fn switch_branch(root: &Path, branch: &Branch) -> anyhow::Result<()> {
    if branch.remote {
        git_ok(root, &["switch", "-q", "--track", &branch.name])?;
    } else {
        git_ok(root, &["switch", "-q", &branch.name])?;
    }
    Ok(())
}

/// Creates `name` at `HEAD` and switches to it.
pub fn create_branch(root: &Path, name: &str) -> anyhow::Result<()> {
    let name = name.trim();
    if name.is_empty() {
        bail!("the branch name is empty");
    }
    git_ok(root, &["switch", "-q", "-c", name])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::tests::{repo, run};
    use super::super::{GitFileStatus, status};
    use super::*;

    fn files(root: &Path) -> Vec<(String, Option<GitFileStatus>, Option<GitFileStatus>)> {
        status(root)
            .unwrap()
            .unwrap()
            .files
            .into_iter()
            .map(|f| (f.repo_path, f.staged, f.unstaged))
            .collect()
    }

    #[test]
    fn stage_commit_amend_uncommit_and_discard() {
        use GitFileStatus::*;
        let (_dir, root) = repo();
        std::fs::write(root.join("a.txt"), "a\n").unwrap();
        std::fs::write(root.join("b [x].txt"), "b\n").unwrap();

        // Before the first commit.
        stage(&root, &["a.txt".into()]).unwrap();
        assert_eq!(
            files(&root),
            [
                ("a.txt".into(), Some(Added), None),
                ("b [x].txt".into(), None, Some(Untracked))
            ]
        );
        unstage(&root, &["a.txt".into()]).unwrap();
        assert_eq!(files(&root)[0], ("a.txt".into(), None, Some(Untracked)));
        stage_all(&root).unwrap();
        assert!(commit(&root, "  ", CommitOptions::default()).is_err());
        commit(&root, "first", CommitOptions::default()).unwrap();
        assert!(files(&root).is_empty());

        // Undoing the first commit leaves everything staged.
        uncommit(&root).unwrap();
        let after = status(&root).unwrap().unwrap();
        assert_eq!((after.head.as_deref(), after.staged().count()), (None, 2));
        commit(&root, "first", CommitOptions::default()).unwrap();

        // Paths are literal, not globs.
        std::fs::write(root.join("b [x].txt"), "b2\n").unwrap();
        std::fs::write(root.join("bx.txt"), "other\n").unwrap();
        stage(&root, &["b [x].txt".into()]).unwrap();
        assert_eq!(
            files(&root),
            [
                ("b [x].txt".into(), Some(Modified), None),
                ("bx.txt".into(), None, Some(Untracked))
            ]
        );
        commit(&root, "second", CommitOptions::default()).unwrap();
        let amend = CommitOptions {
            amend: true,
            ..CommitOptions::default()
        };
        commit(&root, "second, amended\n\nWith a body.", amend).unwrap();
        commit(&root, "", amend).unwrap();
        assert_eq!(
            run(&root, &["log", "--format=%s"]),
            "second, amended\nfirst\n"
        );
        assert_eq!(
            last_commit_message(&root).unwrap().as_deref(),
            Some("second, amended\n\nWith a body.")
        );
        uncommit(&root).unwrap();
        assert_eq!(files(&root)[0], ("b [x].txt".into(), Some(Modified), None));
        unstage_all(&root).unwrap();
        assert_eq!(files(&root)[0], ("b [x].txt".into(), None, Some(Modified)));

        // Discarding goes back to the index.
        std::fs::write(root.join("a.txt"), "staged\n").unwrap();
        stage(&root, &["a.txt".into()]).unwrap();
        std::fs::write(root.join("a.txt"), "staged\nand more\n").unwrap();
        discard(&root, &["a.txt".into(), "b [x].txt".into()]).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "staged\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("b [x].txt")).unwrap(),
            "b\n"
        );
    }

    #[test]
    fn tracked_staging_signoff_and_skipped_hooks() {
        use GitFileStatus::*;
        let (_dir, root) = repo();
        assert_eq!(last_commit_message(&root).unwrap(), None);
        std::fs::write(root.join("a.txt"), "a\n").unwrap();
        stage_all(&root).unwrap();
        commit(&root, "first", CommitOptions::default()).unwrap();

        // Staging tracked changes leaves new files alone.
        std::fs::write(root.join("a.txt"), "b\n").unwrap();
        std::fs::write(root.join("new.txt"), "new\n").unwrap();
        stage_tracked(&root).unwrap();
        assert_eq!(
            files(&root),
            [
                ("a.txt".into(), Some(Modified), None),
                ("new.txt".into(), None, Some(Untracked)),
            ]
        );

        // Skipping hooks gets past a failing pre-commit hook; sign-off adds a trailer.
        let hook = root.join(".git/hooks/pre-commit");
        std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
        std::fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert!(commit(&root, "blocked", CommitOptions::default()).is_err());
        }
        let options = CommitOptions {
            signoff: true,
            no_verify: true,
            ..CommitOptions::default()
        };
        commit(&root, "second", options).unwrap();
        let message = last_commit_message(&root).unwrap().unwrap();
        assert!(message.starts_with("second\n"), "{message}");
        assert!(message.ends_with("Signed-off-by: t <t@t>"), "{message}");
    }

    /// Logs the `-c` config of every git process that updates a ref of `root` (to see
    /// what a remote command ran with); returns the log's path.
    #[cfg(unix)]
    fn log_ref_updates(root: &Path) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let log = root.join(".git/ref-updates.log");
        let hook = root.join(".git/hooks/reference-transaction");
        std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
        let script = format!(
            "#!/bin/sh\necho \"$GIT_CONFIG_PARAMETERS\" >> '{}'\n",
            log.display()
        );
        std::fs::write(&hook, script).unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        log
    }

    /// Whether the ref updates logged since the last call ran with the stall limits of
    /// remote commands; clears the log.
    #[cfg(unix)]
    fn used_remote_config(log: &Path) -> bool {
        let text = std::fs::read_to_string(log).unwrap_or_default();
        std::fs::write(log, "").unwrap();
        text.contains("'http.lowSpeedLimit'='1000'") && text.contains("'http.lowSpeedTime'='30'")
    }

    #[test]
    fn branches_and_remotes() {
        let remote_dir = tempfile::tempdir().unwrap();
        let remote = crate::project::normalize_project_path(remote_dir.path()).unwrap();
        run(&remote, &["init", "-q", "--bare"]);
        let (_dir, root) = repo();
        std::fs::write(root.join("a.txt"), "a\n").unwrap();
        stage_all(&root).unwrap();
        commit(&root, "first", CommitOptions::default()).unwrap();
        assert!(push(&root).is_err(), "no remote");
        run(
            &root,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );

        #[cfg(unix)]
        let ref_log = log_ref_updates(&root);

        // Publishing sets the upstream.
        push(&root).unwrap();
        #[cfg(unix)]
        assert!(
            used_remote_config(&ref_log),
            "push limits stalled transfers"
        );
        let published = status(&root).unwrap().unwrap();
        assert_eq!(published.upstream.as_deref(), Some("origin/main"));
        assert!(published.has_remote);
        std::fs::write(root.join("a.txt"), "b\n").unwrap();
        stage_all(&root).unwrap();
        commit(&root, "second", CommitOptions::default()).unwrap();
        assert_eq!(status(&root).unwrap().unwrap().ahead, 1);
        push(&root).unwrap();
        assert_eq!(status(&root).unwrap().unwrap().ahead, 0);

        // Another clone pushes; fetch sees it and pull brings it in.
        let other_dir = tempfile::tempdir().unwrap();
        let other = crate::project::normalize_project_path(other_dir.path()).unwrap();
        run(&other, &["clone", "-q", remote.to_str().unwrap(), "."]);
        std::fs::write(other.join("c.txt"), "c\n").unwrap();
        run(&other, &["add", "."]);
        run(&other, &["commit", "-q", "-m", "third"]);
        run(&other, &["push", "-q"]);
        run(&other, &["switch", "-q", "-c", "feature"]);
        run(&other, &["push", "-q", "-u", "origin", "feature"]);
        #[cfg(unix)]
        used_remote_config(&ref_log);
        fetch(&root).unwrap();
        #[cfg(unix)]
        assert!(
            used_remote_config(&ref_log),
            "fetch limits stalled transfers"
        );
        assert_eq!(status(&root).unwrap().unwrap().behind, 1);
        pull(&root).unwrap();
        #[cfg(unix)]
        assert!(
            used_remote_config(&ref_log),
            "pull limits stalled transfers"
        );
        assert!(root.join("c.txt").exists());

        // Branches: local first, remote ones without a local branch.
        create_branch(&root, "topic").unwrap();
        assert!(create_branch(&root, " ").is_err());
        let list = branches(&root).unwrap();
        let names: Vec<(&str, bool, bool)> = list
            .iter()
            .map(|b| (b.name.as_str(), b.remote, b.current))
            .collect();
        assert!(names.contains(&("topic", false, true)));
        assert!(names.contains(&("main", false, false)));
        assert!(names.contains(&("origin/feature", true, false)));
        assert!(
            !names
                .iter()
                .any(|(name, ..)| *name == "origin/main" || *name == "origin/HEAD")
        );
        let feature = list.iter().find(|b| b.name == "origin/feature").unwrap();
        switch_branch(&root, feature).unwrap();
        let switched = status(&root).unwrap().unwrap();
        assert_eq!(switched.branch.as_deref(), Some("feature"));
        assert_eq!(switched.upstream.as_deref(), Some("origin/feature"));
        let main = branches(&root)
            .unwrap()
            .into_iter()
            .find(|b| b.name == "main")
            .unwrap();
        switch_branch(&root, &main).unwrap();
        assert_eq!(
            status(&root).unwrap().unwrap().branch.as_deref(),
            Some("main")
        );
    }
}
