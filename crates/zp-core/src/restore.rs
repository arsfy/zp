//! Restoring files to recorded versions, with checks for changes made outside ZP.
//!
//! Each file has the content the session expects on disk (`expected`) and the content to
//! restore (`target`). If the disk still has the expected content, the target is written.
//! If it was modified since, the revert is applied as a three-way merge (the edits made
//! since are kept, see [`merge3`]); when that fails, or the file was deleted or created outside ZP, the
//! file is a conflict and only overwritten if the user agrees.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;

use crate::checkpoint::{BlobStore, ContentRef, RecordedChange};

/// A file to bring from `expected` to `target`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileTarget {
    pub path: PathBuf,
    /// What the session believes is on disk.
    pub expected: ContentRef,
    /// What to restore.
    pub target: ContentRef,
}

/// Why a file cannot be restored cleanly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictKind {
    /// Modified outside ZP in a way that conflicts with the restore.
    Modified,
    /// Deleted outside ZP.
    Deleted,
    /// Exists, but the session expects no file there.
    Created,
    /// Not readable as UTF-8 text.
    Unreadable,
}

impl ConflictKind {
    pub fn describe(self) -> &'static str {
        match self {
            Self::Modified => "modified outside ZP",
            Self::Deleted => "deleted outside ZP",
            Self::Created => "created outside ZP",
            Self::Unreadable => "not readable as text",
        }
    }
}

/// How a file will be restored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The file is as expected; the target replaces it.
    Clean,
    /// The file changed since; the restore was merged with those changes.
    Merged,
    /// The file already has the target content.
    AlreadyRestored,
    /// See [`ConflictKind`]; written only when overwriting conflicts.
    Conflict(ConflictKind),
}

/// What to do to the file on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Write(Arc<str>),
    Delete,
    Keep,
}

/// The plan for one file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedFile {
    pub path: PathBuf,
    pub outcome: Outcome,
    /// The action when applied (for conflicts: when overwriting).
    pub action: Action,
    /// Disk content when planned (`None`: no file).
    current: Option<Arc<str>>,
}

impl PlannedFile {
    /// Whether the disk still has what [`RestorePlan::prepare`] saw. A file that was not
    /// readable as text then counts as unchanged while it still is not.
    fn unchanged_since_prepare(&self) -> bool {
        let unreadable = self.outcome == Outcome::Conflict(ConflictKind::Unreadable);
        match read_current(&self.path) {
            Ok(current) => !unreadable && current.as_deref() == self.current.as_deref(),
            Err(()) => unreadable,
        }
    }
}

/// What [`RestorePlan::apply`] did.
#[derive(Debug, Default)]
pub struct AppliedRestore {
    /// The changes made, recorded in the store (including those made before an error).
    pub changes: Vec<RecordedChange>,
    /// Files left as they are because they changed after the plan was prepared.
    pub changed_since_prepare: Vec<PathBuf>,
    /// What stopped the restore; later files were not touched.
    pub error: Option<anyhow::Error>,
}

/// Restores checked against the disk, ready to apply.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RestorePlan {
    pub files: Vec<PlannedFile>,
}

impl RestorePlan {
    /// Reads the files and decides how to restore each.
    pub fn prepare(targets: &[FileTarget], store: &BlobStore) -> anyhow::Result<Self> {
        let mut files = Vec::new();
        for target in targets {
            let expected = target.expected.load(store)?;
            let wanted = target.target.load(store)?;
            let (current, outcome, action) = match read_current(&target.path) {
                Err(()) => (
                    None,
                    Outcome::Conflict(ConflictKind::Unreadable),
                    to_action(&wanted),
                ),
                Ok(current) => {
                    let (outcome, action) = decide(&expected, &wanted, current.as_deref());
                    (current.map(Arc::from), outcome, action)
                }
            };
            files.push(PlannedFile {
                path: target.path.clone(),
                outcome,
                action,
                current,
            });
        }
        Ok(Self { files })
    }

    /// Files that were changed outside ZP in a conflicting way.
    pub fn conflicts(&self) -> impl Iterator<Item = &PlannedFile> {
        self.files
            .iter()
            .filter(|file| matches!(file.outcome, Outcome::Conflict(_)))
    }

    /// Whether nothing would change on disk.
    pub fn is_noop(&self) -> bool {
        self.files.iter().all(|file| file.action == Action::Keep)
    }

    /// Writes the plan; conflicts are overwritten only if `overwrite_conflicts`. A file
    /// that changed since [`Self::prepare`] (e.g. while the user was asked about
    /// conflicts) is left alone and reported. Stops at the first error, still returning
    /// the changes made until then.
    pub fn apply(&self, overwrite_conflicts: bool, store: &BlobStore) -> AppliedRestore {
        let mut applied = AppliedRestore::default();
        for file in &self.files {
            let skipped_conflict =
                matches!(file.outcome, Outcome::Conflict(_)) && !overwrite_conflicts;
            if skipped_conflict || file.action == Action::Keep {
                continue;
            }
            if !file.unchanged_since_prepare() {
                applied.changed_since_prepare.push(file.path.clone());
                continue;
            }
            match apply_file(file, store) {
                Ok(Some(change)) => applied.changes.push(change),
                Ok(None) => {}
                Err(error) => {
                    applied.error = Some(error);
                    break;
                }
            }
        }
        applied
    }
}

/// Applies the action of `file`, returning the change made (recorded in `store`).
fn apply_file(file: &PlannedFile, store: &BlobStore) -> anyhow::Result<Option<RecordedChange>> {
    let after = match &file.action {
        Action::Keep => return Ok(None),
        Action::Write(text) => {
            if let Some(dir) = file.path.parent() {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("could not create {}", dir.display()))?;
            }
            std::fs::write(&file.path, text.as_bytes())
                .with_context(|| format!("could not write {}", file.path.display()))?;
            Some(text.as_ref())
        }
        Action::Delete => {
            match std::fs::remove_file(&file.path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("could not delete {}", file.path.display()));
                }
            }
            None
        }
    };
    if file.current.as_deref() == after {
        return Ok(None);
    }
    RecordedChange::store(store, &file.path, file.current.as_deref(), after).map(Some)
}

/// The file's text; `Ok(None)` when it does not exist, `Err` when it is not text.
fn read_current(path: &Path) -> Result<Option<String>, ()> {
    match std::fs::read(path) {
        Ok(bytes) => String::from_utf8(bytes).map(Some).map_err(|_| ()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(()),
    }
}

fn to_action(target: &Option<Arc<str>>) -> Action {
    match target {
        Some(text) => Action::Write(text.clone()),
        None => Action::Delete,
    }
}

fn decide(
    expected: &Option<Arc<str>>,
    target: &Option<Arc<str>>,
    current: Option<&str>,
) -> (Outcome, Action) {
    if current == target.as_deref() {
        return (Outcome::AlreadyRestored, Action::Keep);
    }
    if current == expected.as_deref() {
        return (Outcome::Clean, to_action(target));
    }
    match (expected, target, current) {
        (Some(expected), Some(target), Some(current)) => {
            match merge3(expected, current, target) {
                // The restore is already contained in the current version.
                Some(merged) if merged == current => (Outcome::AlreadyRestored, Action::Keep),
                Some(merged) => (Outcome::Merged, Action::Write(Arc::from(merged))),
                None => (
                    Outcome::Conflict(ConflictKind::Modified),
                    Action::Write(target.clone()),
                ),
            }
        }
        (Some(_), _, None) => (Outcome::Conflict(ConflictKind::Deleted), to_action(target)),
        (None, _, Some(_)) => (Outcome::Conflict(ConflictKind::Created), to_action(target)),
        (Some(_), None, Some(_)) => (Outcome::Conflict(ConflictKind::Modified), Action::Delete),
        // `current == expected == None` was handled above.
        (None, _, None) => (Outcome::Clean, to_action(target)),
    }
}

/// A replacement of base lines `start..end` by `lines`.
struct Hunk<'a> {
    start: usize,
    end: usize,
    lines: &'a [&'a str],
    ours: bool,
}

fn hunks<'a>(base: &[&str], side: &'a [&'a str], ours: bool) -> Vec<Hunk<'a>> {
    similar::capture_diff_slices(similar::Algorithm::Myers, base, side)
        .into_iter()
        .filter(|op| op.tag() != similar::DiffTag::Equal)
        .map(|op| Hunk {
            start: op.old_range().start,
            end: op.old_range().end,
            lines: &side[op.new_range()],
            ours,
        })
        .collect()
}

/// Applies `hunks` (of one side, sorted) to `base[start..end]`.
fn apply_region<'a>(
    base: &[&'a str],
    start: usize,
    end: usize,
    hunks: &[&Hunk<'a>],
) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut pos = start;
    for hunk in hunks {
        out.extend_from_slice(&base[pos..hunk.start]);
        out.extend_from_slice(hunk.lines);
        pos = hunk.end;
    }
    out.extend_from_slice(&base[pos..end]);
    out
}

/// Three-way merge of the changes `base → ours` and `base → theirs`, by line. Changes
/// conflict only where they overlap (not when merely adjacent); overlapping regions of
/// equal length are merged line by line. `None` on a conflict.
pub fn merge3(base: &str, ours: &str, theirs: &str) -> Option<String> {
    let base: Vec<&str> = base.split_inclusive('\n').collect();
    let ours: Vec<&str> = ours.split_inclusive('\n').collect();
    let theirs: Vec<&str> = theirs.split_inclusive('\n').collect();
    let mut all: Vec<Hunk> = hunks(&base, &ours, true);
    all.extend(hunks(&base, &theirs, false));
    all.sort_by_key(|hunk| (hunk.start, hunk.end));

    let mut out: Vec<&str> = Vec::new();
    let mut pos = 0;
    let mut index = 0;
    while index < all.len() {
        // A cluster of hunks overlapping each other (an insertion at the edge of a
        // change belongs to it, since the order would be ambiguous).
        let (start, mut end) = (all[index].start, all[index].end);
        let mut last = index + 1;
        while let Some(next) = all.get(last) {
            let insertion = next.start == next.end;
            if next.start < end || (next.start == end && (insertion || start == end)) {
                end = end.max(next.end);
                last += 1;
            } else {
                break;
            }
        }
        let cluster = &all[index..last];
        index = last;

        out.extend_from_slice(&base[pos..start]);
        pos = end;
        let ours_hunks: Vec<&Hunk> = cluster.iter().filter(|h| h.ours).collect();
        let theirs_hunks: Vec<&Hunk> = cluster.iter().filter(|h| !h.ours).collect();
        let ours_region = apply_region(&base, start, end, &ours_hunks);
        let theirs_region = apply_region(&base, start, end, &theirs_hunks);
        if ours_hunks.is_empty() || ours_region == theirs_region {
            out.extend(theirs_region);
        } else if theirs_hunks.is_empty() {
            out.extend(ours_region);
        } else if ours_region.len() == end - start && theirs_region.len() == end - start {
            for (line, (ours, theirs)) in base[start..end]
                .iter()
                .zip(ours_region.iter().zip(&theirs_region))
            {
                if ours == line || ours == theirs {
                    out.push(theirs);
                } else if theirs == line {
                    out.push(ours);
                } else {
                    return None;
                }
            }
        } else {
            return None;
        }
    }
    out.extend_from_slice(&base[pos..]);
    Some(out.concat())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(
        store: &BlobStore,
        path: &Path,
        expected: Option<&str>,
        target: Option<&str>,
    ) -> FileTarget {
        let put = |text: Option<&str>| {
            text.map_or(ContentRef::Absent, |t| {
                ContentRef::Blob(store.put(t).unwrap())
            })
        };
        FileTarget {
            path: path.to_path_buf(),
            expected: put(expected),
            target: put(target),
        }
    }

    fn applied_cleanly(applied: AppliedRestore) -> Vec<RecordedChange> {
        assert!(applied.error.is_none(), "{:?}", applied.error);
        assert!(applied.changed_since_prepare.is_empty());
        applied.changes
    }

    #[test]
    fn clean_merged_and_conflicting_restores() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::in_memory();
        let path = |name: &str| dir.path().join(name);
        std::fs::write(path("clean"), "new\n").unwrap();
        std::fs::write(
            path("merged"),
            "one\nTWO\nthree\nfour\nfive\nsix (edited by user)\n",
        )
        .unwrap();
        std::fs::write(path("conflict"), "user rewrote everything\n").unwrap();
        std::fs::write(path("created"), "made by the agent\n").unwrap();
        std::fs::write(path("done"), "old\n").unwrap();

        let targets = [
            target(&store, &path("clean"), Some("new\n"), Some("old\n")),
            target(
                &store,
                &path("merged"),
                Some("one\nTWO\nthree\nfour\nfive\nsix\n"),
                Some("one\ntwo\nthree\nfour\nfive\nsix\n"),
            ),
            target(
                &store,
                &path("conflict"),
                Some("agent version\n"),
                Some("original\n"),
            ),
            target(&store, &path("created"), Some("made by the agent\n"), None),
            target(
                &store,
                &path("deleted"),
                Some("was here\n"),
                Some("original\n"),
            ),
            target(&store, &path("done"), Some("new\n"), Some("old\n")),
        ];
        let plan = RestorePlan::prepare(&targets, &store).unwrap();
        let outcomes: Vec<&Outcome> = plan.files.iter().map(|f| &f.outcome).collect();
        assert_eq!(
            outcomes,
            [
                &Outcome::Clean,
                &Outcome::Merged,
                &Outcome::Conflict(ConflictKind::Modified),
                &Outcome::Clean,
                &Outcome::Conflict(ConflictKind::Deleted),
                &Outcome::AlreadyRestored,
            ]
        );
        assert_eq!(plan.conflicts().count(), 2);

        let changes = applied_cleanly(plan.apply(false, &store));
        let read = |name: &str| std::fs::read_to_string(path(name)).ok();
        assert_eq!(read("clean").as_deref(), Some("old\n"));
        assert_eq!(
            read("merged").as_deref(),
            Some("one\ntwo\nthree\nfour\nfive\nsix (edited by user)\n"),
            "the user's later edit survives"
        );
        assert_eq!(
            read("conflict").as_deref(),
            Some("user rewrote everything\n")
        );
        assert_eq!(read("created"), None);
        assert_eq!(read("deleted"), None);
        assert_eq!(changes.len(), 3);
        assert!(changes[2].is_deletion());
        assert_eq!(&*changes[0].before.load(&store).unwrap().unwrap(), "new\n");

        // Overwriting applies the conflicts too.
        let plan = RestorePlan::prepare(&targets, &store).unwrap();
        assert_eq!(plan.conflicts().count(), 2);
        applied_cleanly(plan.apply(true, &store));
        assert_eq!(read("conflict").as_deref(), Some("original\n"));
        assert_eq!(read("deleted").as_deref(), Some("original\n"));
        assert!(RestorePlan::prepare(&targets, &store).unwrap().is_noop());
    }

    #[test]
    fn three_way_merges() {
        // Adjacent changes do not conflict.
        assert_eq!(
            merge3(
                "one\nTWO\nthree\n",
                "one\nTWO\nTHREE\n",
                "one\ntwo\nthree\n"
            )
            .as_deref(),
            Some("one\ntwo\nTHREE\n")
        );
        // Overlapping changes of the same lines merge line by line.
        assert_eq!(
            merge3("ONE\nTWO\n", "ONE\ntwo\n", "one\ntwo\n").as_deref(),
            Some("one\ntwo\n")
        );
        // Both sides changing a line differently conflict.
        assert_eq!(merge3("a\nb\nc\n", "a\nX\nc\n", "a\nY\nc\n"), None);
        // Insertions at the same place conflict unless identical.
        assert_eq!(merge3("a\nc\n", "a\nb\nc\n", "a\nB\nc\n"), None);
        assert_eq!(
            merge3("a\nc\n", "a\nb\nc\n", "a\nb\nc\n").as_deref(),
            Some("a\nb\nc\n")
        );
        // Far apart changes, deletions, and files without a final newline.
        assert_eq!(
            merge3("1\n2\n3\n4\n5", "1\n3\n4\n5", "1\n2\n3\n4\nfive").as_deref(),
            Some("1\n3\n4\nfive")
        );
        assert_eq!(merge3("", "a\n", "").as_deref(), Some("a\n"));
    }

    #[test]
    fn unreadable_and_unexpected_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::in_memory();
        let binary = dir.path().join("binary");
        std::fs::write(&binary, [0xff, 0xfe, 0x00]).unwrap();
        let surprise = dir.path().join("surprise");
        std::fs::write(&surprise, "someone made this\n").unwrap();
        let plan = RestorePlan::prepare(
            &[
                target(&store, &binary, Some("text\n"), Some("old\n")),
                target(&store, &surprise, None, Some("ours\n")),
            ],
            &store,
        )
        .unwrap();
        assert_eq!(
            plan.files[0].outcome,
            Outcome::Conflict(ConflictKind::Unreadable)
        );
        assert_eq!(
            plan.files[1].outcome,
            Outcome::Conflict(ConflictKind::Created)
        );
    }

    #[test]
    fn files_changed_after_prepare_are_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::in_memory();
        let edited = dir.path().join("edited");
        let untouched = dir.path().join("untouched");
        std::fs::write(&edited, "new\n").unwrap();
        std::fs::write(&untouched, "new\n").unwrap();
        let plan = RestorePlan::prepare(
            &[
                target(&store, &edited, Some("new\n"), Some("old\n")),
                target(&store, &untouched, Some("new\n"), Some("old\n")),
            ],
            &store,
        )
        .unwrap();
        // E.g. while the user was asked about conflicts.
        std::fs::write(&edited, "edited meanwhile\n").unwrap();

        let applied = plan.apply(true, &store);
        assert!(applied.error.is_none());
        assert_eq!(applied.changed_since_prepare, std::slice::from_ref(&edited));
        assert_eq!(
            std::fs::read_to_string(&edited).unwrap(),
            "edited meanwhile\n"
        );
        assert_eq!(std::fs::read_to_string(&untouched).unwrap(), "old\n");
        assert_eq!(applied.changes.len(), 1);
        assert_eq!(applied.changes[0].path, untouched);
    }

    #[test]
    fn an_error_keeps_the_changes_made_before_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::in_memory();
        let first = dir.path().join("first");
        let last = dir.path().join("last");
        std::fs::write(&first, "new\n").unwrap();
        std::fs::write(&last, "new\n").unwrap();
        std::fs::write(dir.path().join("blocker"), "a file, not a directory\n").unwrap();
        let blocked = dir.path().join("blocker/inside");
        let plan = RestorePlan::prepare(
            &[
                target(&store, &first, Some("new\n"), Some("old\n")),
                target(&store, &blocked, None, Some("old\n")),
                target(&store, &last, Some("new\n"), Some("old\n")),
            ],
            &store,
        )
        .unwrap();

        let applied = plan.apply(true, &store);
        assert!(applied.error.is_some());
        assert_eq!(
            applied.changes.len(),
            1,
            "the restored file is still reported"
        );
        assert_eq!(applied.changes[0].path, first);
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "old\n");
        assert_eq!(
            std::fs::read_to_string(&last).unwrap(),
            "new\n",
            "files after the error are not touched"
        );
    }
}
