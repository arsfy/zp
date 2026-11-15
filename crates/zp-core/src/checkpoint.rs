//! File snapshots for diffs and rollbacks.
//!
//! When a tool writes a file, the content before and after the write is stored once in a
//! content-addressed [`BlobStore`] (`~/.zp/checkpoints/objects/<hash>`), and the tool
//! result's `details` keep only the hashes ([`RecordedChange`]). Together with the session
//! tree this gives a checkpoint before every user message: the state of each file at the
//! start of a turn is the `before` of its first change in that turn.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use zp_tools::FileChange;

/// Content-addressed text storage (SHA-256), on disk or in memory.
#[derive(Clone)]
pub struct BlobStore {
    inner: Arc<Inner>,
}

enum Inner {
    Dir(PathBuf),
    Memory(Mutex<HashMap<String, Arc<str>>>),
}

impl std::fmt::Debug for BlobStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &*self.inner {
            Inner::Dir(dir) => f.debug_tuple("BlobStore").field(dir).finish(),
            Inner::Memory(_) => f.write_str("BlobStore(memory)"),
        }
    }
}

impl BlobStore {
    /// A store in `dir` (created on first write).
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self {
            inner: Arc::new(Inner::Dir(dir.into())),
        }
    }

    /// The store of a data directory: `<data>/checkpoints/objects`.
    pub fn in_data_dir(data_dir: &Path) -> Self {
        Self::at(crate::paths::checkpoints_dir(data_dir).join("objects"))
    }

    /// A store that lives as long as the process (for sessions without a file).
    pub fn in_memory() -> Self {
        Self {
            inner: Arc::new(Inner::Memory(Mutex::default())),
        }
    }

    /// The hash under which `text` is stored.
    pub fn hash(text: &str) -> String {
        let digest = Sha256::digest(text.as_bytes());
        digest.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    /// Stores `text` (once) and returns its hash.
    pub fn put(&self, text: &str) -> anyhow::Result<String> {
        let hash = Self::hash(text);
        match &*self.inner {
            Inner::Memory(map) => {
                map.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .entry(hash.clone())
                    .or_insert_with(|| Arc::from(text));
            }
            Inner::Dir(dir) => {
                let path = blob_path(dir, &hash);
                if !path.is_file() {
                    let parent = path.parent().unwrap_or(dir);
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("could not create {}", parent.display()))?;
                    let mut temp = tempfile_in(parent)?;
                    std::io::Write::write_all(&mut temp.1, text.as_bytes())?;
                    drop(temp.1);
                    std::fs::rename(&temp.0, &path).or_else(|error| {
                        let _ = std::fs::remove_file(&temp.0);
                        // Another writer may have stored the same blob meanwhile.
                        if path.is_file() { Ok(()) } else { Err(error) }
                    })?;
                }
            }
        }
        Ok(hash)
    }

    /// The text stored under `hash`.
    pub fn get(&self, hash: &str) -> anyhow::Result<Arc<str>> {
        match &*self.inner {
            Inner::Memory(map) => map
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(hash)
                .cloned()
                .with_context(|| format!("snapshot {hash} is missing")),
            Inner::Dir(dir) => {
                anyhow::ensure!(
                    hash.len() > 2 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
                    "invalid snapshot id {hash}"
                );
                let path = blob_path(dir, hash);
                let text = std::fs::read_to_string(&path)
                    .with_context(|| format!("snapshot {hash} is missing"))?;
                Ok(Arc::from(text))
            }
        }
    }
}

fn blob_path(dir: &Path, hash: &str) -> PathBuf {
    dir.join(&hash[..2]).join(&hash[2..])
}

/// A uniquely named file in `dir`, for atomic writes.
fn tempfile_in(dir: &Path) -> anyhow::Result<(PathBuf, std::fs::File)> {
    for attempt in 0..100u32 {
        let path = dir.join(format!(
            ".tmp-{}-{}-{attempt}",
            std::process::id(),
            zp_ai::now_ms()
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).context("could not create a snapshot file"),
        }
    }
    anyhow::bail!("could not create a snapshot file")
}

/// A version of a file in a recorded change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContentRef {
    /// The file did not exist.
    Absent,
    /// Stored in the [`BlobStore`].
    Blob(String),
    /// Kept in the session itself (changes recorded before snapshots existed).
    Inline(Arc<str>),
}

impl ContentRef {
    /// The content (`None` for [`ContentRef::Absent`]).
    pub fn load(&self, store: &BlobStore) -> anyhow::Result<Option<Arc<str>>> {
        match self {
            Self::Absent => Ok(None),
            Self::Blob(hash) => store.get(hash).map(Some),
            Self::Inline(text) => Ok(Some(text.clone())),
        }
    }

    /// Whether this is [`ContentRef::Absent`].
    pub fn is_absent(&self) -> bool {
        matches!(self, Self::Absent)
    }

    /// Whether both refer to the same content (without loading blobs).
    pub fn same_as(&self, other: &Self) -> bool {
        self.identity() == other.identity()
    }

    fn identity(&self) -> Option<String> {
        match self {
            Self::Absent => None,
            Self::Blob(hash) => Some(hash.clone()),
            Self::Inline(text) => Some(BlobStore::hash(text)),
        }
    }

    fn from_hash(hash: Option<String>) -> Self {
        hash.map_or(Self::Absent, Self::Blob)
    }

    fn hash(&self) -> Option<String> {
        self.identity()
    }
}

/// A file change as recorded in a session: `write`/`edit` tool results and file
/// restores.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedChange {
    /// Absolute path.
    pub path: PathBuf,
    pub before: ContentRef,
    pub after: ContentRef,
    /// Lines added.
    pub added: usize,
    /// Lines removed.
    pub removed: usize,
}

/// The stored form of a [`RecordedChange`] in result `details`.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredDetails {
    path: String,
    before_hash: Option<String>,
    after_hash: Option<String>,
    #[serde(default)]
    added: usize,
    #[serde(default)]
    removed: usize,
}

impl RecordedChange {
    /// Stores both versions in `store` and records the change between them.
    pub fn store(
        store: &BlobStore,
        path: impl Into<PathBuf>,
        before: Option<&str>,
        after: Option<&str>,
    ) -> anyhow::Result<Self> {
        let (added, removed) = crate::diff::line_stats(before, after);
        let put = |text: Option<&str>| -> anyhow::Result<ContentRef> {
            Ok(match text {
                Some(text) => ContentRef::Blob(store.put(text)?),
                None => ContentRef::Absent,
            })
        };
        Ok(Self {
            path: path.into(),
            before: put(before)?,
            after: put(after)?,
            added,
            removed,
        })
    }

    /// Reads a change from tool result `details`: the stored form, or the inline
    /// [`FileChange`] of the tools.
    pub fn from_details(details: &Value) -> Option<Self> {
        if details.get("beforeHash").is_some() || details.get("afterHash").is_some() {
            let stored: StoredDetails = serde_json::from_value(details.clone()).ok()?;
            return Some(Self {
                path: PathBuf::from(stored.path),
                before: ContentRef::from_hash(stored.before_hash),
                after: ContentRef::from_hash(stored.after_hash),
                added: stored.added,
                removed: stored.removed,
            });
        }
        let change = FileChange::from_details(details)?;
        let (added, removed) = change.line_stats();
        Some(Self {
            path: PathBuf::from(&change.path),
            before: change.before.map_or(ContentRef::Absent, |text| {
                ContentRef::Inline(Arc::from(text))
            }),
            after: ContentRef::Inline(Arc::from(change.after)),
            added,
            removed,
        })
    }

    /// The stored form, as result `details`. Inline contents are referenced by hash, so
    /// only use this for changes created with [`RecordedChange::store`].
    pub fn to_details(&self) -> Value {
        serde_json::to_value(StoredDetails {
            path: self.path.to_string_lossy().replace('\\', "/"),
            before_hash: self.before.hash(),
            after_hash: self.after.hash(),
            added: self.added,
            removed: self.removed,
        })
        .unwrap_or(Value::Null)
    }

    /// Whether the change created the file.
    pub fn is_creation(&self) -> bool {
        self.before.is_absent()
    }

    /// Whether the change deleted the file.
    pub fn is_deletion(&self) -> bool {
        self.after.is_absent()
    }
}

/// Moves the contents of a tool's inline [`FileChange`] details into `store`; `None`
/// when `details` are not an inline change.
pub fn store_tool_change(store: &BlobStore, details: &Value) -> Option<anyhow::Result<Value>> {
    if details.get("beforeHash").is_some() {
        return None;
    }
    let change = FileChange::from_details(details)?;
    Some(
        RecordedChange::store(
            store,
            &change.path,
            change.before.as_deref(),
            Some(&change.after),
        )
        .map(|recorded| recorded.to_details()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blobs_on_disk_and_in_memory() {
        let dir = tempfile::tempdir().unwrap();
        for store in [BlobStore::at(dir.path()), BlobStore::in_memory()] {
            let hash = store.put("hello\n").unwrap();
            assert_eq!(hash.len(), 64);
            assert_eq!(store.put("hello\n").unwrap(), hash, "stored once");
            assert_eq!(&*store.get(&hash).unwrap(), "hello\n");
            assert!(store.get(&BlobStore::hash("other")).is_err());
        }
        let hash = BlobStore::hash("hello\n");
        assert!(dir.path().join(&hash[..2]).join(&hash[2..]).is_file());
        assert!(BlobStore::at(dir.path()).get("../../etc").is_err());
    }

    #[test]
    fn changes_round_trip_through_details() {
        let store = BlobStore::in_memory();
        let inline = FileChange {
            path: "/p/a.txt".into(),
            before: Some("a\nb\n".into()),
            after: "a\nc\n".into(),
        }
        .to_details();
        let stored = store_tool_change(&store, &inline).unwrap().unwrap();
        assert!(stored.get("after").is_none(), "contents leave the session");
        assert!(store_tool_change(&store, &stored).is_none());

        let from_inline = RecordedChange::from_details(&inline).unwrap();
        let from_stored = RecordedChange::from_details(&stored).unwrap();
        assert_eq!((from_stored.added, from_stored.removed), (1, 1));
        assert!(from_inline.after.same_as(&from_stored.after));
        assert!(from_inline.before.same_as(&from_stored.before));
        assert_eq!(&*from_stored.after.load(&store).unwrap().unwrap(), "a\nc\n");

        let created = RecordedChange::store(&store, "/p/new", None, Some("x\n")).unwrap();
        let back = RecordedChange::from_details(&created.to_details()).unwrap();
        assert_eq!(back, created);
        assert!(back.is_creation() && !back.is_deletion());
        assert_eq!(back.before.load(&store).unwrap(), None);
    }
}
