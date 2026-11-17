//! `todo`: the agent's task list for the current work.
//!
//! Each call sends the whole list, so the list lives in the transcript: the latest `todo`
//! result's [`TodoList`] details are the current state (also after reopening a session
//! or switching branches).

use anyhow::bail;
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use zp_agent::{AgentTool, AgentToolResult, ToolContext};

/// Most items in a list.
const MAX_ITEMS: usize = 50;

/// Progress of a task.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    #[default]
    Pending,
    InProgress,
    Completed,
}

impl TodoStatus {
    /// A checkbox for text output.
    pub fn marker(self) -> &'static str {
        match self {
            Self::Pending => "[ ]",
            Self::InProgress => "[>]",
            Self::Completed => "[x]",
        }
    }
}

/// One task.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoItem {
    /// What to do, in the imperative (`Run the tests`).
    pub content: String,
    pub status: TodoStatus,
}

/// The whole list: the arguments of a `todo` call and the details of its result.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoList {
    pub todos: Vec<TodoItem>,
}

impl TodoList {
    /// The list of a `todo` result's details.
    pub fn from_details(details: &Value) -> Option<Self> {
        serde_json::from_value(details.clone()).ok()
    }

    /// Completed items.
    pub fn completed(&self) -> usize {
        self.todos
            .iter()
            .filter(|item| item.status == TodoStatus::Completed)
            .count()
    }

    /// The item being worked on, if any.
    pub fn current(&self) -> Option<&TodoItem> {
        self.todos
            .iter()
            .find(|item| item.status == TodoStatus::InProgress)
    }

    /// The list as text, one item per line.
    pub fn render(&self) -> String {
        self.todos
            .iter()
            .map(|item| format!("{} {}", item.status.marker(), item.content))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Keeps a task list; stateless (the list is in the call and its result).
#[derive(Clone, Copy, Debug, Default)]
pub struct TodoTool;

impl AgentTool for TodoTool {
    fn name(&self) -> &str {
        "todo"
    }

    fn label(&self) -> &str {
        "Todo"
    }

    fn description(&self) -> &str {
        "Create and update the task list for the current work, shown to the user as \
         progress. Send the whole list every time (it replaces the previous one). Statuses: \
         pending, in_progress, completed. Keep exactly one item in_progress while working, \
         and mark items completed as soon as they are done."
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some("Plan multi-step work and track progress")
    }

    fn prompt_guidelines(&self) -> &[&str] {
        &[
            "Use todo for tasks with three or more steps: list the steps first, then update the list as you go. Skip it for simple requests.",
        ]
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "description": "The complete task list",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {"type": "string", "description": "The task, in the imperative (e.g. Run the tests)"},
                            "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]}
                        },
                        "required": ["content", "status"]
                    }
                }
            },
            "required": ["todos"]
        })
    }

    fn execute<'a>(
        &'a self,
        args: Value,
        _ctx: ToolContext,
    ) -> BoxFuture<'a, anyhow::Result<AgentToolResult>> {
        Box::pin(async move {
            let mut list: TodoList = serde_json::from_value(args)?;
            for item in &mut list.todos {
                item.content = item.content.trim().to_owned();
            }
            if list.todos.iter().any(|item| item.content.is_empty()) {
                bail!("every item needs a description");
            }
            if list.todos.len() > MAX_ITEMS {
                bail!("at most {MAX_ITEMS} items; group smaller steps together");
            }
            let mut text = if list.todos.is_empty() {
                "Cleared the task list.".to_owned()
            } else {
                format!(
                    "Task list updated ({}/{} done):\n{}",
                    list.completed(),
                    list.todos.len(),
                    list.render()
                )
            };
            let active = list
                .todos
                .iter()
                .filter(|item| item.status == TodoStatus::InProgress)
                .count();
            if active > 1 {
                text.push_str("\nNote: keep only one item in_progress at a time.");
            }
            Ok(AgentToolResult::text(text).with_details(serde_json::to_value(&list)?))
        })
    }
}

#[cfg(test)]
mod tests {
    use tokio_util::sync::CancellationToken;

    use super::*;

    async fn run(args: Value) -> anyhow::Result<AgentToolResult> {
        TodoTool
            .execute(args, ToolContext::detached("t", CancellationToken::new()))
            .await
    }

    #[tokio::test]
    async fn keeps_the_list_in_details() {
        let result = run(json!({"todos": [
            {"content": " Write tests ", "status": "completed"},
            {"content": "Fix the bug", "status": "in_progress"},
            {"content": "Update docs", "status": "pending"}
        ]}))
        .await
        .unwrap();
        assert_eq!(
            result.content[0].as_text().unwrap(),
            "Task list updated (1/3 done):\n[x] Write tests\n[>] Fix the bug\n[ ] Update docs"
        );
        let list = TodoList::from_details(result.details.as_ref().unwrap()).unwrap();
        assert_eq!(list.current().unwrap().content, "Fix the bug");
        assert_eq!(list.todos[0].content, "Write tests");

        let two_active = run(json!({"todos": [
            {"content": "a", "status": "in_progress"},
            {"content": "b", "status": "in_progress"}
        ]}))
        .await
        .unwrap();
        assert!(
            two_active.content[0]
                .as_text()
                .unwrap()
                .contains("only one item in_progress")
        );
        let cleared = run(json!({"todos": []})).await.unwrap();
        assert_eq!(cleared.content[0].as_text(), Some("Cleared the task list."));
        assert!(
            run(json!({"todos": [{"content": " ", "status": "pending"}]}))
                .await
                .is_err()
        );
        assert!(
            run(json!({"todos": [{"content": "x", "status": "done"}]}))
                .await
                .is_err()
        );
    }
}
