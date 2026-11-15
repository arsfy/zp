//! Flattening a session tree into displayable rows.
//!
//! Only user and assistant messages are shown; other entries (tool results, model
//! changes, labels…) are skipped while keeping their descendants. Indentation grows only
//! where the conversation branches, so a linear conversation is a flat list.

use std::collections::HashSet;

use zp_agent::AgentMessage;
use zp_ai::{Message, StopReason, UserContent};
use zp_session::{SessionEntry, SessionTreeNode};

/// Whose message a node is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeKind {
    User,
    Assistant,
}

/// One displayed node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeRow {
    pub id: String,
    pub depth: usize,
    pub kind: NodeKind,
    pub label: String,
    /// Bookmark label set on the entry.
    pub tag: Option<String>,
    /// On the branch currently shown.
    pub active: bool,
    /// The newest message of the shown branch.
    pub current: bool,
    /// The conversation forks after this node.
    pub branches: bool,
}

fn summary(entry: &SessionEntry) -> Option<(NodeKind, String)> {
    let first_line = |text: &str| zp_tools::shorten(text.trim(), 80);
    match entry.message()? {
        AgentMessage::Llm(Message::User(user)) => {
            let text: String = user
                .content
                .iter()
                .filter_map(UserContent::as_text)
                .collect();
            Some((NodeKind::User, first_line(&text)))
        }
        AgentMessage::Llm(Message::Assistant(assistant)) => {
            let text = assistant.text();
            let label = if matches!(
                assistant.stop_reason,
                StopReason::Error | StopReason::Aborted
            ) {
                "(failed)".to_owned()
            } else if !text.trim().is_empty() {
                first_line(&text)
            } else {
                let tools: Vec<&str> = assistant.tool_calls().map(|c| c.name.as_str()).collect();
                if tools.is_empty() {
                    "(empty)".to_owned()
                } else {
                    format!("tools: {}", tools.join(", "))
                }
            };
            Some((NodeKind::Assistant, label))
        }
        _ => None,
    }
}

/// The displayed nodes among `nodes`, looking through skipped entries.
fn displayed(nodes: &[SessionTreeNode]) -> Vec<&SessionTreeNode> {
    let mut out = Vec::new();
    for node in nodes {
        if summary(&node.entry).is_some() {
            out.push(node);
        } else {
            out.extend(displayed(&node.children));
        }
    }
    out
}

/// Flattens `roots`. `active_path` holds the ids on the shown branch and `leaf` the
/// shown leaf entry (which may be a skipped entry such as a tool result).
pub fn flatten(
    roots: &[SessionTreeNode],
    active_path: &HashSet<String>,
    leaf: Option<&str>,
) -> Vec<TreeRow> {
    let mut rows = Vec::new();
    let top = displayed(roots);
    let depth = usize::from(top.len() > 1);
    for node in top {
        walk(node, depth, active_path, &mut rows);
    }
    // The newest shown message on the active branch is "current".
    let current = rows.iter().rposition(|row| row.active);
    if let Some(index) = current
        && leaf.is_some()
    {
        rows[index].current = true;
    }
    rows
}

fn walk(
    node: &SessionTreeNode,
    depth: usize,
    active_path: &HashSet<String>,
    rows: &mut Vec<TreeRow>,
) {
    let Some((kind, label)) = summary(&node.entry) else {
        return;
    };
    let children = displayed(&node.children);
    rows.push(TreeRow {
        id: node.entry.id.clone(),
        depth,
        kind,
        label,
        tag: node.label.clone(),
        active: active_path.contains(&node.entry.id),
        current: false,
        branches: children.len() > 1,
    });
    let child_depth = if children.len() > 1 { depth + 1 } else { depth };
    // Show the active branch first so the shown conversation reads top to bottom.
    let mut ordered = children;
    ordered.sort_by_key(|child| !subtree_contains(child, active_path));
    for child in ordered {
        walk(child, child_depth, active_path, rows);
    }
}

fn subtree_contains(node: &SessionTreeNode, ids: &HashSet<String>) -> bool {
    ids.contains(&node.entry.id) || node.children.iter().any(|c| subtree_contains(c, ids))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use zp_ai::{AssistantContent, AssistantMessage, TextContent};
    use zp_session::Session;

    use super::*;

    fn assistant(text: &str) -> AgentMessage {
        let mut message = AssistantMessage::empty("a", "p", "m");
        message
            .content
            .push(AssistantContent::Text(TextContent { text: text.into() }));
        message.into()
    }

    #[test]
    fn indents_only_at_branches() {
        let mut session = Session::in_memory(Path::new("/p"));
        session.append_message(AgentMessage::user("u1")).unwrap();
        session.append_model_change("p", "m").unwrap();
        let a1 = session.append_message(assistant("a1")).unwrap();
        session.append_message(AgentMessage::user("u2")).unwrap();
        session.append_message(assistant("a2")).unwrap();
        session.branch(&a1).unwrap();
        let u3 = session.append_message(AgentMessage::user("u3")).unwrap();
        session.append_label(&u3, Some("retry".into())).unwrap();

        let active: HashSet<String> = session
            .branch_path(None)
            .iter()
            .map(|e| e.id.clone())
            .collect();
        let rows = flatten(&session.tree(), &active, session.leaf_id());
        let shape: Vec<(usize, &str, bool)> = rows
            .iter()
            .map(|r| (r.depth, r.label.as_str(), r.active))
            .collect();
        assert_eq!(
            shape,
            [
                (0, "u1", true),
                (0, "a1", true),
                (1, "u3", true),
                (1, "u2", false),
                (1, "a2", false),
            ]
        );
        assert!(rows[1].branches);
        assert_eq!(rows[2].tag.as_deref(), Some("retry"));
        assert!(rows[2].current && !rows[1].current);
    }
}
