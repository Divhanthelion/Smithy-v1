use async_trait::async_trait;
use serde_json::Value;

use crate::registry::{ExecutionControl, Tool, ToolCtx};
use crate::schema::{arg_str, ToolDefinition, ToolOutput, ToolParameter};

pub struct Write;

impl Write {
    async fn run_with_control(
        &self,
        args: &Value,
        ctx: &ToolCtx,
        control: &ExecutionControl,
    ) -> ToolOutput {
        let path = match arg_str(args, "path") {
            Ok(p) => p,
            Err(e) => return ToolOutput::err(e),
        };
        let content = match arg_str(args, "content") {
            Ok(c) => c,
            Err(e) => return ToolOutput::err(e),
        };

        let expected = match ctx.workspace.snapshot(path) {
            Ok(snapshot) => snapshot,
            Err(error) => return ToolOutput::err(error),
        };
        let existed = matches!(expected, crate::sandbox::FileSnapshot::Present(_));
        if let Err(e) = ctx.workspace.compare_and_write_authorized(
            path,
            &expected,
            content,
            || control.authorize_publication(),
        ) {
            return ToolOutput::err(e.to_string());
        }

        let lines = content.lines().count();
        let verb = if existed { "Overwrote" } else { "Created" };
        ToolOutput::ok(format!(
            "{verb} `{}` ({lines} line{}).",
            ctx.workspace.display_path(path),
            if lines == 1 { "" } else { "s" }
        ))
    }
}

#[async_trait]
impl Tool for Write {
    fn name(&self) -> &'static str {
        "write"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "write",
            "Create a new file, or completely replace an existing one. Always emit the COMPLETE \
             file contents — never a diff or an excerpt. For a small change to an existing file, \
             prefer `edit`. Missing parent directories are created.",
            vec![
                ToolParameter::string("path", "File path, relative to the workspace root.", true),
                ToolParameter::string("content", "The complete file contents.", true),
            ],
        )
    }

    async fn run(&self, args: &Value, ctx: &ToolCtx) -> ToolOutput {
        self.run_with_control(args, ctx, &ExecutionControl::default())
            .await
    }

    async fn run_controlled(
        &self,
        args: &Value,
        ctx: &ToolCtx,
        control: &ExecutionControl,
    ) -> ToolOutput {
        self.run_with_control(args, ctx, control).await
    }
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
    async fn creates_a_new_file() {
        let (_t, ctx) = ctx();
        let out = Write
            .run(
                &serde_json::json!({"path": "a.txt", "content": "hi\n"}),
                &ctx,
            )
            .await;
        assert!(!out.is_error);
        assert!(out.content.starts_with("Created"));
        assert_eq!(ctx.workspace.read_to_string("a.txt").unwrap(), "hi\n");
    }

    #[tokio::test]
    async fn reports_overwrite_distinctly() {
        let (_t, ctx) = ctx();
        ctx.workspace.write("a.txt", "old").unwrap();
        let out = Write
            .run(
                &serde_json::json!({"path": "a.txt", "content": "new"}),
                &ctx,
            )
            .await;
        assert!(out.content.starts_with("Overwrote"));
        assert_eq!(ctx.workspace.read_to_string("a.txt").unwrap(), "new");
    }

    #[tokio::test]
    async fn creates_parent_directories() {
        let (_t, ctx) = ctx();
        let out = Write
            .run(
                &serde_json::json!({"path": "deep/nest/a.txt", "content": "x"}),
                &ctx,
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            ctx.workspace.read_to_string("deep/nest/a.txt").unwrap(),
            "x"
        );
    }

    #[tokio::test]
    async fn refuses_to_write_outside_the_workspace() {
        let (_t, ctx) = ctx();
        let out = Write
            .run(
                &serde_json::json!({"path": "../evil.txt", "content": "x"}),
                &ctx,
            )
            .await;
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn refuses_to_overwrite_a_directory() {
        let (_t, ctx) = ctx();
        std::fs::create_dir(ctx.workspace.root().join("sub")).unwrap();
        let out = Write
            .run(&serde_json::json!({"path": "sub", "content": "x"}), &ctx)
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("directory"));
    }
}
