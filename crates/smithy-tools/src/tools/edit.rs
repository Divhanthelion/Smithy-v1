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

use crate::fuzzy::{self, MatchSearch, MatchTier};
use crate::registry::{ExecutionControl, Tool, ToolCtx};
use crate::schema::{arg_bool, arg_str, ToolDefinition, ToolOutput, ToolParameter};

pub struct Edit;

/// A validated edit computed without touching the filesystem.
///
/// Both the direct tool and the review hook use this exact planner. Keeping the
/// validation, fuzzy cascade and messages here prevents a preview from offering
/// a change that the real tool would reject.
#[derive(Debug, Clone, PartialEq)]
pub struct EditPlan {
    pub content: String,
    pub message: String,
}

impl EditPlan {
    pub fn validate(old: &str, new: &str) -> Result<(), String> {
        if old == new {
            return Err("old_string and new_string are identical — nothing to change".into());
        }
        if old.is_empty() {
            return Err("old_string is empty — use `write` to create a file".into());
        }
        Ok(())
    }

    pub fn new(
        text: &str,
        old: &str,
        new: &str,
        replace_all: bool,
        shown: &str,
    ) -> Result<Self, String> {
        Self::validate(old, new)?;

        let exact_count = fuzzy::count_exact(text, old);
        if exact_count > 1 && !replace_all {
            return Err(format!(
                "`old_string` appears {exact_count} times in `{shown}`; it must be unique. \
                 Add surrounding context to disambiguate, or set replace_all=true."
            ));
        }
        if exact_count > 0 {
            let content = if replace_all {
                text.replace(old, new)
            } else {
                text.replacen(old, new, 1)
            };
            let n = if replace_all { exact_count } else { 1 };
            return Ok(Self {
                content,
                message: format!(
                    "Edited `{shown}` ({n} replacement{}).",
                    if n == 1 { "" } else { "s" }
                ),
            });
        }

        let m = match fuzzy::find(text, old) {
            MatchSearch::Unique(found) => found,
            MatchSearch::None => {
                return Err(format!(
                    "`old_string` was not found in `{shown}`, and no close match could be identified. \
                     Read the file and copy the exact text (including indentation) you want to replace."
                ));
            }
            MatchSearch::Ambiguous(ambiguous) => {
                let candidates = ambiguous
                    .candidates
                    .iter()
                    .enumerate()
                    .map(|(index, candidate)| format!("Candidate {}:\n{}", index + 1, candidate))
                    .collect::<Vec<_>>()
                    .join("\n\n");
                return Err(format!(
                    "`old_string` matched more than one region in `{shown}` after {} matching \
                     (confidence {:.2}). Refusing to choose the first tied region. Add exact \
                     surrounding context to make the target unique.\n\n{candidates}",
                    ambiguous.tier, ambiguous.confidence
                ));
            }
        };

        if !m.auto_apply {
            return Err(format!(
                "`old_string` did not match exactly. The closest region ({} match, confidence \
                 {:.2}) was:\n{}\n\nThat is not close enough to edit safely. Re-issue the edit \
                 using that exact text if it is the region you meant.",
                m.tier, m.confidence, m.matched_text
            ));
        }

        if replace_all && m.tier != MatchTier::Exact {
            return Err(format!(
                "replace_all needs an exact match, but `old_string` only matched approximately \
                 ({}). The text that matched was:\n{}\n\nRe-issue with that exact text.",
                m.tier, m.matched_text
            ));
        }

        let mut content = String::with_capacity(text.len() + new.len());
        content.push_str(&text[..m.byte_offset]);
        content.push_str(new);
        content.push_str(&text[m.byte_offset + m.matched_text.len()..]);

        let mut message = format!("Edited `{shown}` (1 replacement, {} match).", m.tier);
        if let Some(advisory) = m.advisory() {
            message.push('\n');
            message.push_str(&advisory);
        }
        Ok(Self { content, message })
    }
}

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
        self.run_controlled(args, ctx, &ExecutionControl::default())
            .await
    }

    async fn run_controlled(
        &self,
        args: &Value,
        ctx: &ToolCtx,
        control: &ExecutionControl,
    ) -> ToolOutput {
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

        if let Err(error) = EditPlan::validate(old, new) {
            return ToolOutput::err(error);
        }
        let expected = match ctx.workspace.snapshot(path) {
            Ok(crate::sandbox::FileSnapshot::Present(base)) => {
                crate::sandbox::FileSnapshot::Present(base)
            }
            Ok(crate::sandbox::FileSnapshot::Missing) => {
                return ToolOutput::err(format!("cannot read `{path}`: file does not exist"));
            }
            Err(e) => return ToolOutput::err(e),
        };
        let text = expected.content().unwrap().to_string();
        let shown = ctx.workspace.display_path(path);
        let plan = match EditPlan::new(&text, old, new, replace_all, &shown) {
            Ok(plan) => plan,
            Err(error) => return ToolOutput::err(error),
        };
        if let Err(e) = ctx
            .workspace
            .compare_and_write_authorized(path, &expected, &plan.content, || {
                control.authorize_publication()
            })
        {
            return ToolOutput::err(e.to_string());
        }
        ToolOutput::ok(plan.message)
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

    /// A normalized tie used to select the first source-order candidate. The
    /// refusal must explain both the tier and how to make intent unique.
    #[test]
    fn ambiguous_fuzzy_ties_have_an_actionable_refusal_message() {
        let error = EditPlan::new(
            "let  value = 1;\nbetween();\nlet value  = 1;\n",
            "let   value   = 1;",
            "let value = 2;",
            false,
            "a.rs",
        )
        .unwrap_err();
        assert!(error.contains("more than one region"), "{error}");
        assert!(error.contains("whitespace-normalized"), "{error}");
        assert!(error.contains("Refusing to choose the first tied region"));
        assert!(error.contains("surrounding context"));
    }
}
