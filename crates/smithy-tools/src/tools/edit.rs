//! Targeted string-replacement edits, with the fuzzy cascade behind them.
//!
//! coda's `edit` required `old_string` to appear byte-exactly and exactly once,
//! and returned a hard error otherwise. That is correct but brittle: its own
//! post-mortem predicted the `read → edit` path would fail because `read`
//! prepends line numbers the model has to strip by hand.
//!
//! This version keeps coda's uniqueness rule for exact matches — that rule is
//! what prevents an edit from silently hitting the wrong occurrence — and falls
//! back to [`crate::fuzzy`] only when the exact match finds nothing. A fuzzy hit
//! reports back what it actually matched, so the model self-corrects instead of
//! retrying the same wrong text.

use async_trait::async_trait;
use serde_json::Value;

use crate::fuzzy::{self, MatchTier};
use crate::registry::{Tool, ToolCtx};
use crate::schema::{arg_bool, arg_str, ToolDefinition, ToolOutput, ToolParameter};

pub struct Edit;

#[async_trait]
impl Tool for Edit {
    fn name(&self) -> &'static str {
        "edit"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "edit",
            "Make a targeted edit to an existing file by replacing a string. `old_string` should \
             appear exactly once — include enough surrounding context to be unique — unless \
             replace_all is true. Line-number prefixes from `read` output are stripped \
             automatically, and near-misses in whitespace or a single line are resolved \
             automatically. Preferred over `write` for small changes to large files.",
            vec![
                ToolParameter::string("path", "File path, relative to the workspace root.", true),
                ToolParameter::string(
                    "old_string",
                    "Text to replace. Must be unique in the file unless replace_all is true.",
                    true,
                ),
                ToolParameter::string("new_string", "Replacement text.", true),
                ToolParameter::boolean(
                    "replace_all",
                    "Replace every exact occurrence (default false). Only applies to exact matches.",
                    false,
                ),
            ],
        )
    }

    async fn run(&self, args: &Value, ctx: &ToolCtx) -> ToolOutput {
        let path = match arg_str(args, "path") {
            Ok(p) => p,
            Err(e) => return ToolOutput::err(e),
        };
        let old = match arg_str(args, "old_string") {
            Ok(s) => s,
            Err(e) => return ToolOutput::err(e),
        };
        let new = match arg_str(args, "new_string") {
            Ok(s) => s,
            Err(e) => return ToolOutput::err(e),
        };
        let replace_all = arg_bool(args, "replace_all").unwrap_or(false);

        if old == new {
            return ToolOutput::err("old_string and new_string are identical — nothing to change");
        }
        if old.is_empty() {
            return ToolOutput::err("old_string is empty — use `write` to create a file");
        }

        let text = match ctx.workspace.read_to_string(path) {
            Ok(t) => t,
            Err(e) => return ToolOutput::err(e),
        };
        let shown = ctx.workspace.display_path(path);

        // Exact path first, including the uniqueness rule.
        let exact_count = fuzzy::count_exact(&text, old);
        if exact_count > 1 && !replace_all {
            return ToolOutput::err(format!(
                "`old_string` appears {exact_count} times in `{shown}`; it must be unique. \
                 Add surrounding context to disambiguate, or set replace_all=true."
            ));
        }
        if exact_count > 0 {
            let updated = if replace_all {
                text.replace(old, new)
            } else {
                text.replacen(old, new, 1)
            };
            if let Err(e) = ctx.workspace.write(path, &updated) {
                return ToolOutput::err(e);
            }
            let n = if replace_all { exact_count } else { 1 };
            return ToolOutput::ok(format!(
                "Edited `{shown}` ({n} replacement{}).",
                if n == 1 { "" } else { "s" }
            ));
        }

        // No exact hit — run the cascade.
        let Some(m) = fuzzy::find(&text, old) else {
            return ToolOutput::err(format!(
                "`old_string` was not found in `{shown}`, and no close match could be identified. \
                 Read the file and copy the exact text (including indentation) you want to replace."
            ));
        };

        if !m.auto_apply {
            return ToolOutput::err(format!(
                "`old_string` did not match exactly. The closest region ({} match, confidence \
                 {:.2}) was:\n{}\n\nThat is not close enough to edit safely. Re-issue the edit \
                 using that exact text if it is the region you meant.",
                m.tier, m.confidence, m.matched_text
            ));
        }

        if replace_all && m.tier != MatchTier::Exact {
            return ToolOutput::err(format!(
                "replace_all needs an exact match, but `old_string` only matched approximately \
                 ({}). The text that matched was:\n{}\n\nRe-issue with that exact text.",
                m.tier, m.matched_text
            ));
        }

        let mut updated = String::with_capacity(text.len() + new.len());
        updated.push_str(&text[..m.byte_offset]);
        updated.push_str(new);
        updated.push_str(&text[m.byte_offset + m.matched_text.len()..]);

        if let Err(e) = ctx.workspace.write(path, &updated) {
            return ToolOutput::err(e);
        }

        let mut msg = format!("Edited `{shown}` (1 replacement, {} match).", m.tier);
        if let Some(advisory) = m.advisory() {
            msg.push('\n');
            msg.push_str(&advisory);
        }
        ToolOutput::ok(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::Workspace;

    const SRC: &str = "fn main() {\n    let retry_limit = 5;\n    println!(\"hi\");\n}\n";

    fn ctx_with(contents: &str) -> (tempfile::TempDir, ToolCtx) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("m.rs"), contents).unwrap();
        let ws = Workspace::open(tmp.path()).unwrap();
        (tmp, ToolCtx::new(ws))
    }

    async fn edit(ctx: &ToolCtx, old: &str, new: &str) -> ToolOutput {
        Edit.run(
            &serde_json::json!({"path": "m.rs", "old_string": old, "new_string": new}),
            ctx,
        )
        .await
    }

    #[tokio::test]
    async fn exact_replacement() {
        let (_t, ctx) = ctx_with(SRC);
        let out = edit(&ctx, "retry_limit = 5", "retry_limit = 8").await;
        assert!(!out.is_error, "{}", out.content);
        assert!(ctx
            .workspace
            .read_to_string("m.rs")
            .unwrap()
            .contains("retry_limit = 8"));
    }

    #[tokio::test]
    async fn non_unique_old_string_is_refused() {
        let (_t, ctx) = ctx_with("x = 1;\nx = 1;\n");
        let out = edit(&ctx, "x = 1;", "x = 2;").await;
        assert!(out.is_error);
        assert!(out.content.contains("appears 2 times"));
        assert_eq!(
            ctx.workspace.read_to_string("m.rs").unwrap(),
            "x = 1;\nx = 1;\n"
        );
    }

    #[tokio::test]
    async fn replace_all_handles_duplicates() {
        let (_t, ctx) = ctx_with("x = 1;\nx = 1;\n");
        let out = Edit
            .run(
                &serde_json::json!({
                    "path": "m.rs", "old_string": "x = 1;", "new_string": "x = 2;",
                    "replace_all": true
                }),
                &ctx,
            )
            .await;
        assert!(!out.is_error);
        assert!(out.content.contains("2 replacements"));
        assert_eq!(
            ctx.workspace.read_to_string("m.rs").unwrap(),
            "x = 2;\nx = 2;\n"
        );
    }

    /// The failure coda predicted: the model copies `read` output into
    /// `old_string`, line numbers and all. Previously a hard error.
    #[tokio::test]
    async fn recovers_when_the_model_pastes_read_output() {
        let (_t, ctx) = ctx_with(SRC);
        let pasted = "     2\t    let retry_limit = 5;";
        let out = edit(&ctx, pasted, "    let retry_limit = 8;").await;
        assert!(!out.is_error, "{}", out.content);
        let after = ctx.workspace.read_to_string("m.rs").unwrap();
        assert!(after.contains("retry_limit = 8"));
        assert!(
            !after.contains('\t'),
            "gutter must not leak into the file: {after:?}"
        );
        assert!(out.content.contains("line-numbers-stripped"));
    }

    #[tokio::test]
    async fn recovers_from_whitespace_drift() {
        let (_t, ctx) = ctx_with(SRC);
        let out = edit(&ctx, "let  retry_limit  =  5;", "let retry_limit = 8;").await;
        assert!(!out.is_error, "{}", out.content);
        assert!(ctx
            .workspace
            .read_to_string("m.rs")
            .unwrap()
            .contains("retry_limit = 8"));
        assert!(out.content.contains("whitespace-normalized"));
    }

    #[tokio::test]
    async fn a_low_confidence_match_is_reported_not_applied() {
        let (_t, ctx) = ctx_with(SRC);
        let out = edit(
            &ctx,
            "    let retry_limit = 5;\n    do_something_else();",
            "x",
        )
        .await;
        assert!(out.is_error, "{}", out.content);
        assert_eq!(ctx.workspace.read_to_string("m.rs").unwrap(), SRC);
    }

    #[tokio::test]
    async fn unmatched_text_leaves_the_file_alone() {
        let (_t, ctx) = ctx_with(SRC);
        let out = edit(&ctx, "nothing like this exists anywhere in the file", "x").await;
        assert!(out.is_error);
        assert_eq!(ctx.workspace.read_to_string("m.rs").unwrap(), SRC);
    }

    #[tokio::test]
    async fn identical_strings_are_refused() {
        let (_t, ctx) = ctx_with(SRC);
        let out = edit(&ctx, "same", "same").await;
        assert!(out.is_error);
        assert!(out.content.contains("identical"));
    }

    #[tokio::test]
    async fn replace_all_requires_an_exact_match() {
        let (_t, ctx) = ctx_with(SRC);
        let out = Edit
            .run(
                &serde_json::json!({
                    "path": "m.rs", "old_string": "let  retry_limit  =  5;",
                    "new_string": "z", "replace_all": true
                }),
                &ctx,
            )
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("needs an exact match"));
    }
}
