//! Filesystem notifications for the project and the repository that owns it.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use gpui::BackgroundExecutor;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::{Match, WalkBuilder};
use notify::event::{CreateKind, MetadataKind, ModifyKind, RemoveKind};
use notify::{Config, Event, EventKind, PathsMut, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use zp_core::git::tracked_ignored_files;
use zp_core::project::{git_dir, normalize_project_path};

const REFRESH_DELAY: Duration = Duration::from_millis(250);
#[cfg(not(test))]
const FALLBACK_INTERVAL: Duration = Duration::from_secs(5);
/// Cap on per-directory `inotify` watches registered for a single worktree on Linux.
#[cfg(target_os = "linux")]
const MAX_LINUX_WORKTREE_WATCHES: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Change {
    /// Working tree or index changed (reloads status only).
    Status,
    /// `HEAD`, refs, config or commits changed (reloads status and history).
    Repository,
    /// Repository root, worktree link or ignore rules changed (rebuilds watches).
    Reconfigure,
    Failed,
}

/// Hierarchical `.gitignore`, `.git/info/exclude` and global exclude rules for a repository.
struct IgnoreRules {
    repo_root: PathBuf,
    /// `(directory, matcher)` ordered from deepest directory to `repo_root`.
    dir_matchers: Vec<(PathBuf, Gitignore)>,
    exclude: Gitignore,
    global: Gitignore,
    /// Files force-tracked with `git add -f` despite matching ignore rules.
    tracked_ignored: Vec<PathBuf>,
}

impl IgnoreRules {
    fn build(
        repo_root: &Path,
        project: &Path,
        marker_parents: &[PathBuf],
        metadata: &[PathBuf],
        #[cfg(target_os = "linux")] worktree_dirs: &mut Vec<PathBuf>,
    ) -> Self {
        let mut dir_matchers = Vec::new();
        let mut add_gitignore = |dir: &Path| {
            let file = dir.join(".gitignore");
            if file.is_file() {
                let mut builder = GitignoreBuilder::new(dir);
                let _ = builder.add(file);
                if let Ok(matcher) = builder.build()
                    && !matcher.is_empty()
                {
                    dir_matchers.push((dir.to_path_buf(), matcher));
                }
            }
        };

        for parent in marker_parents.iter().skip(1) {
            add_gitignore(parent);
        }

        let mut walk = WalkBuilder::new(project);
        walk.hidden(false)
            .follow_links(false)
            .ignore(false)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .parents(true)
            .require_git(false)
            .filter_entry(|entry| entry.file_name() != ".git");
        for entry in walk.build().filter_map(Result::ok) {
            if entry.file_type().is_some_and(|ft| ft.is_dir()) {
                add_gitignore(entry.path());
                #[cfg(target_os = "linux")]
                worktree_dirs.push(entry.into_path());
            }
        }

        dir_matchers.sort_by_key(|a| std::cmp::Reverse(a.0.components().count()));

        let mut exclude_builder = GitignoreBuilder::new(repo_root);
        for dir in metadata {
            let exclude = dir.join("info/exclude");
            if exclude.is_file() {
                let _ = exclude_builder.add(exclude);
            }
        }
        let exclude = exclude_builder.build().unwrap_or_else(|_| Gitignore::empty());
        let (global, _) = GitignoreBuilder::new(repo_root).build_global();

        let tracked_ignored: Vec<PathBuf> = tracked_ignored_files(project).into_iter().collect();
        #[cfg(target_os = "linux")]
        if !tracked_ignored.is_empty() {
            let mut seen: HashSet<PathBuf> = worktree_dirs.iter().cloned().collect();
            for tracked in &tracked_ignored {
                for ancestor in tracked
                    .ancestors()
                    .skip(1)
                    .take_while(|dir| dir.starts_with(project))
                {
                    if seen.insert(ancestor.to_path_buf()) {
                        worktree_dirs.push(ancestor.to_path_buf());
                    }
                }
            }
        }

        Self {
            repo_root: repo_root.to_path_buf(),
            dir_matchers,
            exclude,
            global,
            tracked_ignored,
        }
    }

    fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
        if self.tracked_ignored.iter().any(|tracked| tracked == path) {
            return false;
        }
        if self.dir_matchers.is_empty() && self.exclude.is_empty() && self.global.is_empty() {
            return false;
        }
        let Ok(rel_to_root) = path.strip_prefix(&self.repo_root) else {
            return false;
        };
        let mut current = self.repo_root.clone();
        let mut components = rel_to_root.components().peekable();
        while let Some(component) = components.next() {
            current.push(component.as_os_str());
            let current_is_dir = if components.peek().is_some() {
                true
            } else {
                is_dir
            };
            if self.matches_entry(&current, current_is_dir) == Some(true) {
                return true;
            }
        }
        false
    }

    fn contains_tracked(&self, dir: &Path) -> bool {
        self.tracked_ignored
            .iter()
            .any(|tracked| tracked.starts_with(dir))
    }

    fn matches_entry(&self, entry: &Path, is_dir: bool) -> Option<bool> {
        for (dir, matcher) in &self.dir_matchers {
            if let Ok(rel) = entry.strip_prefix(dir)
                && !rel.as_os_str().is_empty()
            {
                match matcher.matched(rel, is_dir) {
                    Match::Ignore(_) => return Some(true),
                    Match::Whitelist(_) => return Some(false),
                    Match::None => {}
                }
            }
        }
        if let Ok(rel) = entry.strip_prefix(&self.repo_root)
            && !rel.as_os_str().is_empty()
        {
            for matcher in [&self.exclude, &self.global] {
                match matcher.matched(rel, is_dir) {
                    Match::Ignore(_) => return Some(true),
                    Match::Whitelist(_) => return Some(false),
                    Match::None => {}
                }
            }
        }
        None
    }
}

/// The project subtree, the directories containing a possible `.git` marker, and
/// the actual Git metadata directories (including the shared one for a linked worktree).
struct Paths {
    project: PathBuf,
    marker_parents: Vec<PathBuf>,
    metadata: Vec<PathBuf>,
    ignores: Option<IgnoreRules>,
    #[cfg(target_os = "linux")]
    worktree_dirs: Vec<PathBuf>,
}

impl Paths {
    fn new(project: &Path) -> Self {
        let project = normalize_project_path(project).unwrap_or_else(|_| project.to_path_buf());
        let repo_root = project
            .ancestors()
            .find(|dir| dir.join(".git").exists())
            .map(Path::to_path_buf);
        let mut marker_parents = vec![project.clone()];
        if let Some(root) = &repo_root {
            marker_parents.extend(
                project
                    .ancestors()
                    .skip(1)
                    .take_while(|dir| dir.starts_with(root))
                    .map(Path::to_path_buf),
            );
        }
        let mut metadata = Vec::new();
        if let Some(dir) = git_dir(&project)
            && let Ok(dir) = normalize_project_path(&dir)
        {
            let common = std::fs::read_to_string(dir.join("commondir"))
                .ok()
                .and_then(|path| {
                    normalize_project_path(&dir.join(path.trim_end_matches(['\r', '\n']))).ok()
                });
            metadata.push(dir);
            if let Some(common) = common.filter(|dir| !metadata.contains(dir)) {
                metadata.push(common);
            }
        }

        #[cfg(target_os = "linux")]
        let mut worktree_dirs = Vec::new();
        let ignores = if let Some(root) = &repo_root
            && !metadata.is_empty()
        {
            Some(IgnoreRules::build(
                root,
                &project,
                &marker_parents,
                &metadata,
                #[cfg(target_os = "linux")]
                &mut worktree_dirs,
            ))
        } else {
            None
        };

        Self {
            project,
            marker_parents,
            metadata,
            ignores,
            #[cfg(target_os = "linux")]
            worktree_dirs,
        }
    }

    fn register(&self, watcher: &mut RecommendedWatcher) -> notify::Result<()> {
        let mut paths = watcher.paths_mut();
        if self.metadata.is_empty() {
            paths.add(&self.project, RecursiveMode::NonRecursive)?;
            return paths.commit();
        }

        #[cfg(target_os = "linux")]
        {
            if self.worktree_dirs.len() > MAX_LINUX_WORKTREE_WATCHES {
                return Err(notify::Error::new(notify::ErrorKind::MaxFilesWatch));
            }
            paths.add(&self.project, RecursiveMode::NonRecursive)?;
            for dir in &self.worktree_dirs {
                if dir != &self.project {
                    add_optional(&mut *paths, dir, RecursiveMode::NonRecursive)?;
                }
            }
        }
        #[cfg(not(target_os = "linux"))]
        paths.add(&self.project, RecursiveMode::Recursive)?;

        for parent in self.marker_parents.iter().skip(1) {
            let _ = paths.add(parent, RecursiveMode::NonRecursive);
        }

        let mut registered = HashSet::new();
        for dir in &self.metadata {
            if cfg!(target_os = "linux") || !dir.starts_with(&self.project) {
                register_metadata_dir(&mut *paths, dir, &mut registered)?;
            }
        }
        paths.commit()
    }

    fn classify(&self, event: &Event) -> Option<Change> {
        if event.need_rescan() {
            return Some(Change::Reconfigure);
        }
        if event.kind.is_access()
            || event.kind == EventKind::Modify(ModifyKind::Metadata(MetadataKind::AccessTime))
        {
            return None;
        }
        event
            .paths
            .iter()
            .filter_map(|path| self.classify_path(path, event.kind))
            .max()
    }

    fn classify_path(&self, path: &Path, kind: EventKind) -> Option<Change> {
        let path_replaced = matches!(
            kind,
            EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_))
        );
        if path_replaced && (path == self.project || self.metadata.iter().any(|dir| dir == path)) {
            return Some(Change::Reconfigure);
        }
        if let Some(name) = path.file_name()
            && let Some(parent) = path.parent()
            && self.marker_parents.iter().any(|dir| dir == parent)
        {
            if name == ".git" {
                return Some(if path_replaced || !path.is_dir() {
                    Change::Reconfigure
                } else {
                    Change::Repository
                });
            }
            if name == ".gitignore" || name == ".gitattributes" {
                return Some(Change::Reconfigure);
            }
        }

        if let Some(relative) = self
            .metadata
            .iter()
            .find_map(|dir| path.strip_prefix(dir).ok())
        {
            // Git can emit lock files and FSMonitor cookies during our own reads.
            if relative.extension().is_some_and(|ext| ext == "lock") {
                return None;
            }
            let mut components = relative.components();
            let Some(first) = components.next().and_then(|c| c.as_os_str().to_str()) else {
                return Some(Change::Repository);
            };
            let is_top_entry = components.next().is_none();
            return match first {
                "commondir" => Some(Change::Reconfigure),
                "refs" | "reftable" | "info" if path_replaced && is_top_entry => {
                    Some(Change::Reconfigure)
                }
                "info" if relative == Path::new("info/exclude") => Some(Change::Reconfigure),
                "index" | "info" | "MERGE_HEAD" | "CHERRY_PICK_HEAD" | "REVERT_HEAD" => {
                    Some(Change::Status)
                }
                "HEAD" | "refs" | "packed-refs" | "reftable" | "config" | "config.worktree"
                | "shallow" => Some(Change::Repository),
                "modules" => {
                    if relative
                        .components()
                        .any(|c| matches!(c.as_os_str().to_str(), Some("objects" | "logs")))
                    {
                        return None;
                    }
                    if path_replaced && (is_top_entry || path.is_dir()) {
                        return Some(Change::Reconfigure);
                    }
                    if relative.file_name().is_some_and(|name| name == "index") {
                        Some(Change::Status)
                    } else {
                        Some(Change::Repository)
                    }
                }
                _ => None,
            };
        }

        if self.metadata.is_empty() || !path.starts_with(&self.project) {
            return None;
        }
        if path
            .file_name()
            .is_some_and(|name| name == ".gitignore" || name == ".gitattributes")
        {
            let parent_ignored = path.parent().is_some_and(|parent| {
                parent.starts_with(&self.project)
                    && self
                        .ignores
                        .as_ref()
                        .is_some_and(|ignores| ignores.is_ignored(parent, true))
            });
            return (!parent_ignored).then_some(Change::Reconfigure);
        }
        let is_dir = matches!(
            kind,
            EventKind::Create(CreateKind::Folder) | EventKind::Remove(RemoveKind::Folder)
        ) || path.is_dir();
        if self
            .ignores
            .as_ref()
            .is_some_and(|ignores| ignores.is_ignored(path, is_dir))
        {
            let tracked_dir_replaced = path_replaced
                && is_dir
                && self
                    .ignores
                    .as_ref()
                    .is_some_and(|ignores| ignores.contains_tracked(path));
            return tracked_dir_replaced.then_some(Change::Status);
        }
        if cfg!(target_os = "linux")
            && is_dir
            && matches!(
                kind,
                EventKind::Create(_) | EventKind::Modify(ModifyKind::Name(_))
            )
        {
            return Some(Change::Reconfigure);
        }
        Some(Change::Status)
    }
}

/// Watches a Git metadata directory without subscribing to `objects/` or `logs/`.
fn register_metadata_dir(
    paths: &mut dyn PathsMut,
    dir: &Path,
    registered: &mut HashSet<PathBuf>,
) -> notify::Result<()> {
    if registered.insert(dir.to_path_buf()) {
        paths.add(dir, RecursiveMode::NonRecursive)?;
    }
    for sub in ["refs", "reftable", "info"] {
        let target = dir.join(sub);
        if target.is_dir() && registered.insert(target.clone()) {
            add_optional(paths, &target, RecursiveMode::Recursive)?;
        }
    }
    let modules = dir.join("modules");
    if modules.is_dir() {
        let mut walk = WalkBuilder::new(&modules);
        walk.hidden(false)
            .follow_links(false)
            .ignore(false)
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false)
            .parents(false)
            .filter_entry(|entry| entry.file_name() != "objects" && entry.file_name() != "logs");
        for entry in walk.build().filter_map(Result::ok) {
            if entry.file_type().is_some_and(|ft| ft.is_dir())
                && registered.insert(entry.path().to_path_buf())
            {
                add_optional(paths, entry.path(), RecursiveMode::NonRecursive)?;
            }
        }
    }
    Ok(())
}

/// Ignores `PathNotFound` for subdirectories removed between discovery and registration.
fn add_optional(paths: &mut dyn PathsMut, path: &Path, mode: RecursiveMode) -> notify::Result<()> {
    match paths.add(path, mode) {
        Err(err) if matches!(err.kind, notify::ErrorKind::PathNotFound) => Ok(()),
        other => other,
    }
}

struct RepositoryWatch {
    _watcher: RecommendedWatcher,
    wake: mpsc::Receiver<()>,
    pending: Arc<Mutex<Option<Change>>>,
}

impl RepositoryWatch {
    fn new(project: &Path) -> notify::Result<Self> {
        let paths = Arc::new(Paths::new(project));
        let (send, wake) = mpsc::channel(1);
        let pending = Arc::new(Mutex::new(None::<Change>));
        let mut watcher = RecommendedWatcher::new(
            {
                let paths = paths.clone();
                let pending = pending.clone();
                move |event: notify::Result<Event>| {
                    let change = match event {
                        Ok(event) => paths.classify(&event),
                        Err(error) => {
                            tracing::warn!(%error, "repository file watcher stopped");
                            Some(Change::Failed)
                        }
                    };
                    if let Some(change) = change {
                        let mut pending = pending.lock().unwrap_or_else(PoisonError::into_inner);
                        *pending = (*pending).max(Some(change));
                        let _ = send.try_send(());
                    }
                }
            },
            Config::default().with_follow_symlinks(false),
        )?;
        paths.register(&mut watcher)?;
        Ok(Self {
            _watcher: watcher,
            wake,
            pending,
        })
    }

    fn take_pending(&mut self) -> Option<Change> {
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = self.wake.try_recv();
        pending.take()
    }
}

/// Runs on the background executor so starting and stopping native watchers stays off the UI.
pub(super) async fn monitor(
    project: PathBuf,
    executor: BackgroundExecutor,
    changes: mpsc::Sender<Change>,
) {
    'watch: loop {
        let mut watch = match RepositoryWatch::new(&project) {
            Ok(watch) => watch,
            Err(error) => {
                tracing::warn!(%error, path = %project.display(), "could not watch repository files");
                break;
            }
        };
        // Initial load (and resync after watcher registration or reconfiguration).
        if changes.send(Change::Repository).await.is_err() {
            return;
        }
        while watch.wake.recv().await.is_some() {
            executor.timer(REFRESH_DELAY).await;
            match watch.take_pending() {
                Some(Change::Reconfigure) => continue 'watch,
                Some(Change::Failed) => break 'watch,
                Some(change) if changes.send(change).await.is_err() => return,
                _ => {}
            }
        }
        break;
    }

    let _ = changes.send(Change::Repository).await;
    #[cfg(not(test))]
    while {
        executor.timer(FALLBACK_INTERVAL).await;
        changes.send(Change::Repository).await.is_ok()
    } {}
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use notify::event::{DataChange, ModifyKind};

    use super::*;

    #[tokio::test]
    async fn linked_worktree_watches_its_files_index_and_shared_refs() {
        let fixture = tempfile::tempdir().unwrap();
        let root = normalize_project_path(fixture.path()).unwrap();
        let project = root.join("linked");
        let common = root.join("repo/.git");
        let metadata = common.join("worktrees/linked");
        std::fs::create_dir(&project).unwrap();
        std::fs::create_dir_all(&metadata).unwrap();
        std::fs::create_dir_all(common.join("refs/heads")).unwrap();
        std::fs::create_dir_all(common.join("objects/ab")).unwrap();
        std::fs::write(metadata.join("commondir"), "../..\n").unwrap();
        std::fs::write(
            project.join(".git"),
            format!("gitdir: {}\n", metadata.display()),
        )
        .unwrap();
        let paths = Paths::new(&project);
        assert_eq!(
            paths.classify(
                &Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Any)))
                    .add_path(common.join("objects/ab/1234"))
            ),
            None,
            "loose object writes do not wake the panel"
        );

        let (send, mut events) = mpsc::unbounded_channel();
        let mut watcher = notify::recommended_watcher(move |event| {
            let _ = send.send(event);
        })
        .unwrap();
        paths.register(&mut watcher).unwrap();

        for (path, expected) in [
            (project.join("new.txt"), Change::Status),
            (metadata.join("index"), Change::Status),
            (common.join("refs/heads/branch"), Change::Repository),
        ] {
            std::fs::write(&path, "changed").unwrap();
            let mut observed = Vec::new();
            tokio::time::timeout(Duration::from_secs(5), async {
                while let Some(event) = events.recv().await {
                    let event = event.expect("native file watcher error");
                    let change = paths.classify(&event);
                    if change == Some(expected) && event.paths.contains(&path) {
                        return;
                    }
                    observed.push((change, event.paths));
                }
                panic!("file watcher stopped");
            })
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "no {expected:?} notification for {} (received {observed:?})",
                    path.display()
                )
            });
        }
    }

    #[test]
    fn gitignore_and_submodule_metadata_classification() {
        let fixture = tempfile::tempdir().unwrap();
        let root = normalize_project_path(fixture.path()).unwrap();
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(["init", "-q"])
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(root.join(".gitignore"), "/target/\n*.log\n").unwrap();
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        std::fs::create_dir_all(root.join("keep")).unwrap();
        std::fs::write(root.join("keep/.gitignore"), "!important.log\n").unwrap();
        std::fs::write(root.join("target/forced.txt"), "tracked\n").unwrap();
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(["add", "-f", "target/forced.txt"])
                .status()
                .unwrap()
                .success()
        );
        std::fs::create_dir_all(root.join(".git/modules/sub/objects/aa")).unwrap();
        std::fs::create_dir_all(root.join(".git/modules/sub/refs/heads")).unwrap();

        let paths = Paths::new(&root);
        let modify = |path: PathBuf| {
            paths.classify(
                &Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Any))).add_path(path),
            )
        };
        let create = |path: PathBuf| {
            paths.classify(&Event::new(EventKind::Create(CreateKind::File)).add_path(path))
        };

        assert_eq!(modify(root.join("target/debug/deps.d")), None);
        assert_eq!(create(root.join("target/.gitignore")), None);
        assert_eq!(modify(root.join("app.log")), None);
        assert_eq!(
            modify(root.join("keep/important.log")),
            Some(Change::Status)
        );
        assert_eq!(modify(root.join("target/forced.txt")), Some(Change::Status));
        assert_eq!(
            paths.classify(
                &Event::new(EventKind::Remove(RemoveKind::Folder)).add_path(root.join("target"))
            ),
            Some(Change::Status)
        );
        assert_eq!(
            paths.classify(
                &Event::new(EventKind::Remove(RemoveKind::Folder))
                    .add_path(root.join("target/debug"))
            ),
            None
        );
        assert_eq!(
            modify(root.join("keep/.gitignore")),
            Some(Change::Reconfigure)
        );
        assert_eq!(
            modify(root.join(".git/info/exclude")),
            Some(Change::Reconfigure)
        );
        assert_eq!(
            modify(root.join(".git/modules/sub/HEAD")),
            Some(Change::Repository)
        );
        assert_eq!(
            modify(root.join(".git/modules/sub/index")),
            Some(Change::Status)
        );
        assert_eq!(modify(root.join(".git/modules/sub/objects/aa/bb")), None);
    }
}
