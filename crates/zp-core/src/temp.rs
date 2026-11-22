//! Temporary directories: scratch space the agent may use outside the project, and what
//! a run left there (to offer cleaning it up).
//!
//! What a run created is told apart from what was there before by listing the temporary
//! directories when the run starts; only new entries the agent named (in a command, a
//! tool path or a tool's output) are offered for cleanup, never other programs' files.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// A temporary directory, with the spellings a command may use for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TempRoot {
    /// Canonical path (symlinks resolved: `/private/tmp` on macOS).
    pub path: PathBuf,
    /// How it may be written in commands and output (`/tmp`, `/var/folders/…/T`).
    aliases: Vec<String>,
}

/// The temporary directories of this system that exist: the user's (`TMPDIR`, `TEMP`)
/// and, on Unix, `/tmp` and `/var/tmp`.
pub fn roots() -> Vec<TempRoot> {
    let mut candidates = vec![std::env::temp_dir()];
    if cfg!(unix) {
        candidates.extend(["/tmp", "/var/tmp"].map(PathBuf::from));
    }
    let mut roots: Vec<TempRoot> = Vec::new();
    for candidate in candidates {
        let Ok(path) = candidate.canonicalize() else {
            continue;
        };
        if !path.is_dir() || path.parent().is_none() {
            continue;
        }
        let alias = candidate
            .to_string_lossy()
            .trim_end_matches(['/', '\\'])
            .to_owned();
        match roots.iter_mut().find(|root| root.path == path) {
            Some(root) => {
                if !root.aliases.contains(&alias) {
                    root.aliases.push(alias);
                }
            }
            None => {
                let canonical = path
                    .to_string_lossy()
                    .trim_end_matches(['/', '\\'])
                    .to_owned();
                let mut aliases = vec![canonical];
                if !aliases.contains(&alias) {
                    aliases.push(alias);
                }
                roots.push(TempRoot { path, aliases });
            }
        }
    }
    // Longer spellings first, so `/private/tmp/x` is not read as `/tmp`-relative.
    for root in &mut roots {
        root.aliases
            .sort_by_key(|alias| std::cmp::Reverse(alias.len()));
    }
    roots
}

/// The temporary directories that are scratch space for a project at `project`: not one
/// that contains the project, whose other entries may be anything (another checkout).
pub fn scratch_roots(project: &Path) -> Vec<TempRoot> {
    let project = project
        .canonicalize()
        .unwrap_or_else(|_| project.to_path_buf());
    roots()
        .into_iter()
        .filter(|root| !project.starts_with(&root.path))
        .collect()
}

/// The entries of the temporary directories when a run started.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    roots: Vec<TempRoot>,
    entries: HashMap<PathBuf, HashSet<OsString>>,
}

impl Snapshot {
    /// Lists the entries directly in `roots`.
    pub fn take(roots: Vec<TempRoot>) -> Self {
        let entries = roots
            .iter()
            .map(|root| {
                let names = std::fs::read_dir(&root.path)
                    .map(|dir| {
                        dir.filter_map(Result::ok)
                            .map(|entry| entry.file_name())
                            .collect()
                    })
                    .unwrap_or_default();
                (root.path.clone(), names)
            })
            .collect();
        Self { roots, entries }
    }

    /// The entries directly in a temporary directory that `text` names by absolute path
    /// (`/tmp/build-1/out.txt` names `/tmp/build-1`), as canonical paths.
    pub fn mentioned(&self, text: &str) -> BTreeSet<PathBuf> {
        let expanded = expand_temp_variables(text);
        let mut found = BTreeSet::new();
        for root in &self.roots {
            for alias in &root.aliases {
                let prefix = format!("{alias}/");
                let mut rest: &str = &expanded;
                while let Some(at) = rest.find(&prefix) {
                    // A path starts the text or follows a separator or quote, not another path.
                    let starts_path = at == 0
                        || rest[..at].chars().next_back().is_some_and(|c| {
                            !c.is_alphanumeric() && !matches!(c, '/' | '.' | '_' | '-')
                        });
                    let after = &rest[at + prefix.len()..];
                    let name: String = after
                        .chars()
                        .take_while(|c| {
                            !c.is_whitespace()
                                && !matches!(
                                    c,
                                    '/' | '\\'
                                        | '"'
                                        | '\''
                                        | '`'
                                        | ';'
                                        | '|'
                                        | '&'
                                        | ')'
                                        | '('
                                        | '<'
                                        | '>'
                                        | ','
                                        | ':'
                                )
                        })
                        .collect();
                    let name = name.trim_end_matches(['.', ',']);
                    if starts_path && !name.is_empty() && name != "." && name != ".." {
                        found.insert(root.path.join(name));
                    }
                    rest = after;
                }
            }
        }
        found
    }

    /// Of `mentioned` entries, those that exist now but did not when the run started.
    pub fn created<'a>(&self, mentioned: impl IntoIterator<Item = &'a PathBuf>) -> Vec<PathBuf> {
        mentioned
            .into_iter()
            .filter(|path| {
                let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
                    return false;
                };
                self.entries
                    .get(parent)
                    .is_some_and(|before| !before.contains(name))
                    && path.symlink_metadata().is_ok()
            })
            .cloned()
            .collect()
    }

    /// Whether `path` is an entry directly in one of the snapshot's directories.
    fn contains_entry(&self, path: &Path) -> bool {
        path.parent()
            .is_some_and(|parent| self.roots.iter().any(|root| root.path == parent))
            && path.file_name().is_some()
    }
}

/// `$TMPDIR`, `${TMPDIR}` and `%TEMP%` written out, so paths built from them are found.
fn expand_temp_variables(text: &str) -> std::borrow::Cow<'_, str> {
    // Most text (file contents, output) names no variable: no copies then.
    if !text.contains(['$', '%']) {
        return std::borrow::Cow::Borrowed(text);
    }
    let temp = std::env::temp_dir();
    let temp = temp.to_string_lossy();
    let temp = temp.trim_end_matches(['/', '\\']);
    std::borrow::Cow::Owned(
        text.replace("${TMPDIR}", temp)
            .replace("$TMPDIR", temp)
            .replace("%TEMP%", temp)
            .replace("%TMP%", temp),
    )
}

/// Bytes `path` takes: a file's size, or a directory's files (symlinks not followed).
pub fn size(path: &Path) -> u64 {
    let Ok(metadata) = path.symlink_metadata() else {
        return 0;
    };
    if !metadata.is_dir() {
        return metadata.len();
    }
    std::fs::read_dir(path)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| size(&entry.path()))
                .sum()
        })
        .unwrap_or(0)
}

/// Deletes `paths`, entries directly in a temporary directory of `snapshot` (anything
/// else is refused). A symlink is removed, not what it points to. Returns the paths that
/// could not be deleted, with why.
pub fn remove(snapshot: &Snapshot, paths: &[PathBuf]) -> Vec<(PathBuf, String)> {
    let mut failed = Vec::new();
    for path in paths {
        if !snapshot.contains_entry(path) {
            failed.push((path.clone(), "not in a temporary directory".to_owned()));
            continue;
        }
        let result = match path.symlink_metadata() {
            Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(path),
            Ok(_) => std::fs::remove_file(path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        };
        if let Err(error) = result {
            failed.push((path.clone(), error.to_string()));
        }
    }
    failed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(dir: &Path, alias: &str) -> TempRoot {
        let path = dir.canonicalize().unwrap();
        TempRoot {
            aliases: vec![path.to_string_lossy().into_owned(), alias.to_owned()],
            path,
        }
    }

    #[test]
    fn paths_built_from_temp_variables_are_found() {
        let temp = std::env::temp_dir();
        let alias = temp
            .to_string_lossy()
            .trim_end_matches(['/', '\\'])
            .to_owned();
        let snapshot = Snapshot::take(vec![root(&temp, &alias)]);
        let canonical = temp.canonicalize().unwrap();
        assert_eq!(
            snapshot.mentioned("cat $TMPDIR/zp-a/x \"${TMPDIR}/zp-b\"; ls %TEMP%/zp-c"),
            ["zp-a", "zp-b", "zp-c"]
                .map(|name| canonical.join(name))
                .into()
        );
        assert!(snapshot.mentioned("costs 5% of $5").is_empty());
    }

    #[test]
    fn finds_what_a_run_created_where_it_said_so() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("before.txt"), "old").unwrap();
        let snapshot = Snapshot::take(vec![root(dir.path(), "/scratch")]);
        std::fs::create_dir(dir.path().join("build-1")).unwrap();
        std::fs::write(dir.path().join("build-1/out.txt"), "12345").unwrap();
        std::fs::write(dir.path().join("unnamed.txt"), "x").unwrap();
        let mentioned = snapshot.mentioned(
            "cd /scratch/build-1/ && cat '/scratch/before.txt' > out; ls /scratch; x/scratch/no",
        );
        let canonical = dir.path().canonicalize().unwrap();
        assert_eq!(
            mentioned,
            [canonical.join("before.txt"), canonical.join("build-1")].into()
        );
        // Only new entries: not what was there, nor what the agent never named.
        let created = snapshot.created(&mentioned);
        assert_eq!(created, [canonical.join("build-1")]);
        assert_eq!(size(&created[0]), 5);

        let failed = remove(&snapshot, &[created[0].clone(), canonical.clone()]);
        assert!(!canonical.join("build-1").exists());
        assert_eq!(failed.len(), 1, "never the directory itself");
        assert!(canonical.join("before.txt").exists());
    }

    #[test]
    fn a_temporary_directory_that_holds_the_project_is_not_scratch_space() {
        let temp = std::env::temp_dir().canonicalize().unwrap();
        let project = tempfile::tempdir().unwrap();
        assert!(
            !scratch_roots(project.path())
                .iter()
                .any(|root| root.path == temp)
        );
        assert!(roots().iter().any(|root| root.path == temp));
    }
}
