use async_trait::async_trait;
use serde_json::Value;

use crate::registry::{Tool, ToolCtx};
use crate::schema::{arg_str_opt, ToolDefinition, ToolOutput, ToolParameter};

const MAX_ENTRIES: usize = 500;

pub struct Ls;

#[async_trait]
impl Tool for Ls {
    fn name(&self) -> &'static str {
        "ls"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "ls",
            "List the contents of a directory. Directories are listed first and marked with a \
             trailing slash. Use `glob` to find files by name across the tree.",
            vec![ToolParameter::string(
                "path",
                "Directory path relative to the workspace root (default: the root itself).",
                false,
            )],
        )
    }

    async fn run(&self, args: &Value, ctx: &ToolCtx) -> ToolOutput {
        let path = arg_str_opt(args, "path").unwrap_or(".");

        if ctx.workspace.exists(path) && !ctx.workspace.is_dir(path) {
            return ToolOutput::err(format!("`{path}` is a file, not a directory — use `read`"));
        }

        let entries = match ctx.workspace.read_dir(path) {
            Ok(e) => e,
            Err(e) => return ToolOutput::err(e),
        };

        if entries.is_empty() {
            return ToolOutput::ok(format!("`{}` is empty", ctx.workspace.display_path(path)));
        }

        let total = entries.len();
        let mut out = String::new();
        for (name, is_dir) in entries.iter().take(MAX_ENTRIES) {
            if *is_dir {
                out.push_str(&format!("{name}/\n"));
            } else {
                out.push_str(&format!("{name}\n"));
            }
        }
        if total > MAX_ENTRIES {
            out.push_str(&format!("\n[{} of {total} entries shown]", MAX_ENTRIES));
        }
        ToolOutput::ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::Workspace;

    fn ctx() -> (tempfile::TempDir, ToolCtx) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "").unwrap();
        let ws = Workspace::open(tmp.path()).unwrap();
        (tmp, ToolCtx::new(ws))
    }

    #[tokio::test]
    async fn lists_directories_first_with_a_slash() {
        let (_t, ctx) = ctx();
        let out = Ls.run(&serde_json::json!({}), &ctx).await;
        assert_eq!(out.content, "src/\nCargo.toml\n");
    }

    #[tokio::test]
    async fn listing_a_file_is_an_error() {
        let (_t, ctx) = ctx();
        let out = Ls
            .run(&serde_json::json!({"path": "Cargo.toml"}), &ctx)
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("use `read`"));
    }

    #[tokio::test]
    async fn reports_an_empty_directory() {
        let (_t, ctx) = ctx();
        let out = Ls.run(&serde_json::json!({"path": "src"}), &ctx).await;
        assert!(out.content.contains("is empty"));
    }

    #[tokio::test]
    async fn refuses_to_escape_the_workspace() {
        let (_t, ctx) = ctx();
        let out = Ls.run(&serde_json::json!({"path": "../.."}), &ctx).await;
        assert!(out.is_error);
    }
}
