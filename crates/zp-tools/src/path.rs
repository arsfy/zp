//! Resolving tool paths against the working directory.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, Weak};

use tokio::sync::OwnedMutexGuard;

/// Resolves a path given by the model: trims it, turns exotic Unicode spaces into plain
/// spaces, drops a leading `@` (file mentions), expands `~`, and makes it absolute against
/// `cwd`. `.` and `..` are resolved lexically.
pub fn resolve(cwd: &Path, input: &str) -> PathBuf {
    let cleaned: String = input
        .trim()
        .chars()
        .map(|c| match c {
            '\u{00A0}' | '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
            c => c,
        })
        .collect();
    let cleaned = cleaned.strip_prefix('@').unwrap_or(&cleaned);
    let path = if cleaned == "~" {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from(cleaned))
    } else if let Some(rest) = cleaned
        .strip_prefix("~/")
        .or_else(|| cleaned.strip_prefix("~\\"))
        && let Some(home) = dirs::home_dir()
    {
        home.join(rest)
    } else {
        PathBuf::from(cleaned)
    };
    normalize(&if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    })
}

/// Removes `.` components and folds `..` without touching the filesystem.
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(component);
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// `path` relative to `cwd` when it lies inside it, else absolute; always with `/`.
pub fn display(cwd: &Path, path: &Path) -> String {
    let shown = path.strip_prefix(cwd).map_or(path, |relative| {
        if relative.as_os_str().is_empty() {
            Path::new(".")
        } else {
            relative
        }
    });
    slash(shown)
}

/// A path with `/` separators.
pub fn slash(path: &Path) -> String {
    let text = path.to_string_lossy();
    if cfg!(windows) {
        text.replace('\\', "/")
    } else {
        text.into_owned()
    }
}

static FILE_LOCKS: LazyLock<Mutex<HashMap<PathBuf, Weak<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(Mutex::default);

/// Serializes mutations of one file (parallel tool calls may target the same file).
/// Different files do not block each other.
pub async fn lock_file(path: &Path) -> OwnedMutexGuard<()> {
    let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mutex = {
        let mut locks = FILE_LOCKS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        locks.retain(|_, lock| lock.strong_count() > 0);
        match locks.get(&key).and_then(Weak::upgrade) {
            Some(mutex) => mutex,
            None => {
                let mutex = Arc::new(tokio::sync::Mutex::new(()));
                locks.insert(key, Arc::downgrade(&mutex));
                mutex
            }
        }
    };
    mutex.lock_owned().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn resolves_relative_and_special_paths() {
        let cwd = Path::new("/work/project");
        assert_eq!(
            resolve(cwd, "src/main.rs"),
            Path::new("/work/project/src/main.rs")
        );
        assert_eq!(
            resolve(cwd, " ./a/../b.txt "),
            Path::new("/work/project/b.txt")
        );
        assert_eq!(
            resolve(cwd, "@src/lib.rs"),
            Path::new("/work/project/src/lib.rs")
        );
        assert_eq!(
            resolve(cwd, "my\u{00A0}file"),
            Path::new("/work/project/my file")
        );
        assert_eq!(resolve(cwd, "/etc/hosts"), Path::new("/etc/hosts"));
        if let Some(home) = dirs::home_dir() {
            assert_eq!(resolve(cwd, "~/x"), home.join("x"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn displays_relative_inside_cwd() {
        let cwd = Path::new("/work/project");
        assert_eq!(
            display(cwd, Path::new("/work/project/src/a.rs")),
            "src/a.rs"
        );
        assert_eq!(display(cwd, Path::new("/work/project")), ".");
        assert_eq!(display(cwd, Path::new("/elsewhere/b")), "/elsewhere/b");
    }

    #[tokio::test]
    async fn file_locks_serialize_per_path() {
        let a = lock_file(Path::new("/tmp/zp-lock-test-a")).await;
        // A different file is not blocked.
        let _b = lock_file(Path::new("/tmp/zp-lock-test-b")).await;
        let pending = tokio::spawn(async { lock_file(Path::new("/tmp/zp-lock-test-a")).await });
        tokio::task::yield_now().await;
        assert!(!pending.is_finished());
        drop(a);
        pending.await.unwrap();
    }
}
