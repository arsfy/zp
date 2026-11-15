//! The record of a file mutation, attached to `write` / `edit` results as `details`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A file's content before and after a tool call.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileChange {
    /// Absolute path.
    pub path: String,
    /// Previous content; `None` when the file was created.
    pub before: Option<String>,
    /// New content.
    pub after: String,
}

impl FileChange {
    /// As tool result `details`.
    pub fn to_details(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    /// Reads a change back from tool result `details`.
    pub fn from_details(details: &Value) -> Option<Self> {
        serde_json::from_value(details.clone()).ok()
    }

    /// Whether the call created the file.
    pub fn is_creation(&self) -> bool {
        self.before.is_none()
    }

    /// `(added, removed)` line counts, from a line-level comparison of the two versions
    /// (lines are matched by their longest common subsequence prefix/suffix only, which
    /// is exact for the typical single-region change).
    pub fn line_stats(&self) -> (usize, usize) {
        let before: Vec<&str> = self.before.as_deref().unwrap_or_default().lines().collect();
        let after: Vec<&str> = self.after.lines().collect();
        let prefix = before
            .iter()
            .zip(&after)
            .take_while(|(a, b)| a == b)
            .count();
        let suffix = before[prefix..]
            .iter()
            .rev()
            .zip(after[prefix..].iter().rev())
            .take_while(|(a, b)| a == b)
            .count();
        (
            after.len() - prefix - suffix,
            before.len() - prefix - suffix,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_counts_lines() {
        let change = FileChange {
            path: "/p/a.txt".into(),
            before: Some("a\nb\nc\n".into()),
            after: "a\nB\nB2\nc\n".into(),
        };
        assert_eq!(
            FileChange::from_details(&change.to_details()),
            Some(change.clone())
        );
        assert_eq!(change.line_stats(), (2, 1));
        let created = FileChange {
            path: "/p/new".into(),
            before: None,
            after: "x\ny".into(),
        };
        assert!(created.is_creation());
        assert_eq!(created.line_stats(), (2, 0));
    }
}
