use async_trait::async_trait;
use serde_json::Value;

use crate::registry::{Tool, ToolCtx};
use crate::schema::{arg_i64, arg_str, ToolDefinition, ToolOutput, ToolParameter};

pub const MAX_LINES: usize = 2000;
pub const MAX_LINE_CHARS: usize = 2000;

/// The line-number gutter width. Kept in sync with
/// [`crate::fuzzy::strip_line_number_gutter`], which has to recognise this
/// format to undo it when the model pastes `read` output into an `edit`.
const GUTTER_WIDTH: usize = 6;

pub struct Read;

#[async_trait]
impl Tool for Read {
    fn name(&self) -> &'static str {
        "read"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "read",
            "Read a file from the workspace. Returns up to 2000 lines, each prefixed with its \
             line number and a tab. Use offset/limit to page through larger files. Lines longer \
             than 2000 characters are truncated.",
            vec![
                ToolParameter::string("path", "File path, relative to the workspace root.", true),
                ToolParameter::integer("offset", "1-based line to start from (default 1).", false),
                ToolParameter::integer("limit", "Max lines to return (default 2000).", false),
            ],
        )
    }

    async fn run(&self, args: &Value, ctx: &ToolCtx) -> ToolOutput {
        let path = match arg_str(args, "path") {
            Ok(p) => p,
            Err(e) => return ToolOutput::err(e),
        };
        let text = match ctx.workspace.read_to_string(path) {
            Ok(t) => t,
            Err(e) => return ToolOutput::err(e),
        };

        let offset = arg_i64(args, "offset").unwrap_or(1).max(1) as usize;
        let limit = arg_i64(args, "limit")
            .map(|l| l.max(0) as usize)
            .unwrap_or(MAX_LINES)
            .min(MAX_LINES);

        let all: Vec<&str> = text.lines().collect();
        let total = all.len();

        if total == 0 {
            return ToolOutput::ok("[file is empty]");
        }
        if offset > total {
            return ToolOutput::err(format!(
                "offset {offset} is past end of file (`{path}` has {total} lines)"
            ));
        }

        let start = offset - 1;
        let end = (start + limit).min(total);
        let mut out = String::new();
        for (i, line) in all[start..end].iter().enumerate() {
            let n = start + i + 1;
            let shown: String = if line.chars().count() > MAX_LINE_CHARS {
                line.chars().take(MAX_LINE_CHARS).collect::<String>() + " …[line truncated]"
            } else {
                (*line).to_string()
            };
            out.push_str(&format!("{n:>width$}\t{shown}\n", width = GUTTER_WIDTH));
        }
        if end < total {
            out.push_str(&format!(
                "\n[showing lines {offset}–{end} of {total}; use offset={} to continue]",
                end + 1
            ));
        }
        ToolOutput::ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::Workspace;

    fn ctx_with(contents: &str) -> (tempfile::TempDir, ToolCtx) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("f.txt"), contents).unwrap();
        let ws = Workspace::open(tmp.path()).unwrap();
        (tmp, ToolCtx::new(ws))
    }

    #[tokio::test]
    async fn reads_with_line_numbers() {
        let (_t, ctx) = ctx_with("alpha\nbeta\n");
        let out = Read.run(&serde_json::json!({"path": "f.txt"}), &ctx).await;
        assert!(!out.is_error);
        assert_eq!(out.content, "     1\talpha\n     2\tbeta\n");
    }

    /// The gutter `read` emits must be exactly what the fuzzy matcher knows how
    /// to strip, or the `read → edit` round-trip breaks.
    #[tokio::test]
    async fn emitted_gutter_round_trips_through_the_fuzzy_stripper() {
        let (_t, ctx) = ctx_with("alpha\nbeta\n");
        let out = Read.run(&serde_json::json!({"path": "f.txt"}), &ctx).await;
        let stripped = crate::fuzzy::strip_line_number_gutter(out.content.trim_end()).unwrap();
        assert_eq!(stripped, "alpha\nbeta");
    }

    #[tokio::test]
    async fn paginates() {
        let body: String = (1..=10).map(|i| format!("line{i}\n")).collect();
        let (_t, ctx) = ctx_with(&body);
        let out = Read
            .run(
                &serde_json::json!({"path": "f.txt", "offset": 3, "limit": 2}),
                &ctx,
            )
            .await;
        assert!(out.content.contains("     3\tline3"));
        assert!(out.content.contains("     4\tline4"));
        assert!(!out.content.contains("line5\n"));
        assert!(out.content.contains("use offset=5 to continue"));
    }

    #[tokio::test]
    async fn reports_empty_file() {
        let (_t, ctx) = ctx_with("");
        let out = Read.run(&serde_json::json!({"path": "f.txt"}), &ctx).await;
        assert_eq!(out.content, "[file is empty]");
    }

    #[tokio::test]
    async fn offset_past_end_is_an_error() {
        let (_t, ctx) = ctx_with("one\n");
        let out = Read
            .run(&serde_json::json!({"path": "f.txt", "offset": 99}), &ctx)
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("past end of file"));
    }

    #[tokio::test]
    async fn refuses_to_escape_the_workspace() {
        let (_t, ctx) = ctx_with("x");
        let out = Read
            .run(&serde_json::json!({"path": "../../etc/passwd"}), &ctx)
            .await;
        assert!(out.is_error);
    }
}
