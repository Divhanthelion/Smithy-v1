//! Session-scoped task planning.
//!
//! The list lives in [`crate::ToolCtx`] and dies with the session. It exists to
//! give the model somewhere to externalise a multi-step plan so it stops
//! re-deriving one every turn, not to be a durable task database.

use async_trait::async_trait;
use serde_json::Value;

use crate::registry::{Todo, Tool, ToolCtx};
use crate::schema::{ToolDefinition, ToolOutput, ToolParameter};

const VALID_STATUSES: &[&str] = &["pending", "in_progress", "completed"];

pub struct TodoTool;

#[async_trait]
impl Tool for TodoTool {
    fn name(&self) -> &'static str {
        "todo"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "todo",
            "Record or update the plan for a multi-step task. Pass the COMPLETE list every time — \
             it replaces the previous one. Call with no arguments to read the current list. Skip \
             this for trivial one-step tasks.",
            vec![
                ToolParameter::new("todos", "array", "The complete task list, in order.", false)
                    .with_items(
                        ToolParameter::new("item", "object", "One task.", true).with_properties(
                            vec![
                                ToolParameter::string("content", "What the task is.", true),
                                ToolParameter::string(
                                    "status",
                                    "One of: pending, in_progress, completed.",
                                    true,
                                ),
                            ],
                        ),
                    ),
            ],
        )
    }

    async fn run(&self, args: &Value, ctx: &ToolCtx) -> ToolOutput {
        let Some(items) = args.get("todos") else {
            return ToolOutput::ok(render(&ctx.todos()));
        };
        let Some(items) = items.as_array() else {
            return ToolOutput::err("`todos` must be an array of {content, status} objects");
        };

        let mut parsed = Vec::with_capacity(items.len());
        for (i, item) in items.iter().enumerate() {
            let Some(content) = item.get("content").and_then(|v| v.as_str()) else {
                return ToolOutput::err(format!("todo #{} is missing `content`", i + 1));
            };
            let status = item
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("pending");
            if !VALID_STATUSES.contains(&status) {
                return ToolOutput::err(format!(
                    "todo #{} has invalid status `{status}`; must be one of: {}",
                    i + 1,
                    VALID_STATUSES.join(", ")
                ));
            }
            parsed.push(Todo {
                content: content.to_string(),
                status: status.to_string(),
            });
        }

        match ctx.todos.lock() {
            Ok(mut todos) => *todos = parsed.clone(),
            Err(_) => return ToolOutput::err("todo list is poisoned; cannot update"),
        }
        ToolOutput::ok(render(&parsed))
    }
}

fn render(todos: &[Todo]) -> String {
    if todos.is_empty() {
        return "[no tasks]".to_string();
    }
    let mut out = String::new();
    for todo in todos {
        let mark = match todo.status.as_str() {
            "completed" => "[x]",
            "in_progress" => "[~]",
            _ => "[ ]",
        };
        out.push_str(&format!("{mark} {}\n", todo.content));
    }
    let done = todos.iter().filter(|t| t.status == "completed").count();
    out.push_str(&format!("\n{done}/{} complete", todos.len()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::Workspace;

    fn ctx() -> (tempfile::TempDir, ToolCtx) {
        let tmp = tempfile::tempdir().unwrap();
        let ws = Workspace::open(tmp.path()).unwrap();
        (tmp, ToolCtx::new(ws))
    }

    #[tokio::test]
    async fn empty_list_reads_back_clearly() {
        let (_t, ctx) = ctx();
        let out = TodoTool.run(&serde_json::json!({}), &ctx).await;
        assert_eq!(out.content, "[no tasks]");
    }

    #[tokio::test]
    async fn sets_and_renders_a_list() {
        let (_t, ctx) = ctx();
        let out = TodoTool
            .run(
                &serde_json::json!({"todos": [
                    {"content": "read the file", "status": "completed"},
                    {"content": "make the edit", "status": "in_progress"},
                    {"content": "run the tests", "status": "pending"}
                ]}),
                &ctx,
            )
            .await;
        assert!(!out.is_error);
        assert!(out.content.contains("[x] read the file"));
        assert!(out.content.contains("[~] make the edit"));
        assert!(out.content.contains("[ ] run the tests"));
        assert!(out.content.contains("1/3 complete"));
    }

    #[tokio::test]
    async fn a_later_call_replaces_the_whole_list() {
        let (_t, ctx) = ctx();
        TodoTool
            .run(
                &serde_json::json!({"todos": [{"content": "old", "status": "pending"}]}),
                &ctx,
            )
            .await;
        TodoTool
            .run(
                &serde_json::json!({"todos": [{"content": "new", "status": "pending"}]}),
                &ctx,
            )
            .await;
        assert_eq!(ctx.todos().len(), 1);
        assert_eq!(ctx.todos()[0].content, "new");
    }

    #[tokio::test]
    async fn reading_back_persists_within_the_session() {
        let (_t, ctx) = ctx();
        TodoTool
            .run(
                &serde_json::json!({"todos": [{"content": "a", "status": "pending"}]}),
                &ctx,
            )
            .await;
        let out = TodoTool.run(&serde_json::json!({}), &ctx).await;
        assert!(out.content.contains("[ ] a"));
    }

    #[tokio::test]
    async fn rejects_an_unknown_status() {
        let (_t, ctx) = ctx();
        let out = TodoTool
            .run(
                &serde_json::json!({"todos": [{"content": "a", "status": "wat"}]}),
                &ctx,
            )
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("invalid status"));
    }

    #[tokio::test]
    async fn rejects_a_missing_content_field() {
        let (_t, ctx) = ctx();
        let out = TodoTool
            .run(&serde_json::json!({"todos": [{"status": "pending"}]}), &ctx)
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("missing `content`"));
    }

    #[tokio::test]
    async fn a_rejected_update_leaves_the_list_untouched() {
        let (_t, ctx) = ctx();
        TodoTool
            .run(
                &serde_json::json!({"todos": [{"content": "keep", "status": "pending"}]}),
                &ctx,
            )
            .await;
        TodoTool
            .run(&serde_json::json!({"todos": [{"status": "pending"}]}), &ctx)
            .await;
        assert_eq!(ctx.todos()[0].content, "keep");
    }
}
