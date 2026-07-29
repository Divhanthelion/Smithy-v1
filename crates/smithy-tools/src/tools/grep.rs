//! Content search.
//!
//! coda shelled out to `ripgrep`, which made it the agent's one runtime
//! dependency — its README told users to `brew install ripgrep` and it
//! preflighted for the binary on startup. Since ripgrep's walker (`ignore`) and
//! matcher (`regex`) are both already in this dependency tree, doing the search
//! in-process removes that dependency entirely and gives the same
//! gitignore-aware behaviour.
//!
//! See [`super::glob`] for how the walk relates to the sandbox boundary.

use async_trait::async_trait;
use ignore::WalkBuilder;
use regex::RegexBuilder;
use serde_json::Value;

use crate::registry::{Tool, ToolCtx};
use crate::schema::{arg_bool, arg_str, arg_str_opt, ToolDefinition, ToolOutput, ToolParameter};

const MAX_MATCHES: usize = 200;
const MAX_LINE_CHARS: usize = 400;
/// Files above this size are skipped — they are almost always build artefacts
/// or vendored blobs, and reading them stalls the walk.
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

pub struct Grep;

#[async_trait]
impl Tool for Grep {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "grep",
            "Search file contents with a regular expression, respecting .gitignore. Returns \
             `path:line: text` for each match. Use `glob` to find files by name instead.",
            vec![
                ToolParameter::string("pattern", "Regular expression to search for.", true),
                ToolParameter::string(
                    "path",
                    "Directory to search under, relative to the workspace root (default: root).",
                    false,
                ),
                ToolParameter::string(
                    "include",
                    "Only search files whose path matches this glob, e.g. `*.rs`.",
                    false,
                ),
                ToolParameter::boolean(
                    "case_sensitive",
                    "Match case-sensitively (default false).",
                    false,
                ),
            ],
        )
    }

    async fn run(&self, args: &Value, ctx: &ToolCtx) -> ToolOutput {
        let pattern = match arg_str(args, "pattern") {
            Ok(p) => p,
            Err(e) => return ToolOutput::err(e),
        };
        let sub = arg_str_opt(args, "path").unwrap_or(".");
        let include = arg_str_opt(args, "include").map(|s| s.to_string());
        let case_sensitive = arg_bool(args, "case_sensitive").unwrap_or(false);

        // `absolute_real`, not `absolute`: this path is handed to a directory
        // walker that opens it with ordinary `std` calls, outside the cap-std
        // capability. A lexical check cannot see that `sub` is a symlink leading
        // out of the workspace, and that was a real escape.
        let root = match ctx.workspace.absolute_real(sub) {
            Ok(p) => p,
            Err(e) => return ToolOutput::err(e),
        };
        if !root.is_dir() {
            return ToolOutput::err(format!("`{sub}` is not a directory"));
        }

        let re = match RegexBuilder::new(pattern)
            .case_insensitive(!case_sensitive)
            .build()
        {
            Ok(r) => r,
            Err(e) => {
                return ToolOutput::err(format!(
                    "`{pattern}` is not a valid regular expression: {e}"
                ))
            }
        };

        let ws_root = ctx.workspace.root().to_path_buf();
        let pattern_owned = pattern.to_string();

        let search = tokio::task::spawn_blocking(move || {
            let mut matches: Vec<String> = Vec::new();
            let mut total = 0usize;

            for entry in WalkBuilder::new(&root)
                .hidden(false)
                .follow_links(false)
                .require_git(false)
                .build()
                .flatten()
            {
                if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                    continue;
                }
                if entry
                    .metadata()
                    .map(|m| m.len() > MAX_FILE_BYTES)
                    .unwrap_or(true)
                {
                    continue;
                }
                let Ok(rel) = entry.path().strip_prefix(&ws_root) else {
                    continue;
                };
                let rel_str = rel.to_string_lossy().replace('\\', "/");

                if let Some(inc) = &include {
                    if !super::glob::matches_include(inc, &rel_str) {
                        continue;
                    }
                }

                // Binary files read as lossy UTF-8 produce noise; skip anything
                // that is not valid UTF-8 rather than emitting replacement chars.
                let Ok(contents) = std::fs::read(entry.path()) else {
                    continue;
                };
                let Ok(text) = String::from_utf8(contents) else {
                    continue;
                };

                for (i, line) in text.lines().enumerate() {
                    if !re.is_match(line) {
                        continue;
                    }
                    total += 1;
                    if matches.len() < MAX_MATCHES {
                        let shown = if line.chars().count() > MAX_LINE_CHARS {
                            line.chars().take(MAX_LINE_CHARS).collect::<String>() + "…"
                        } else {
                            line.to_string()
                        };
                        matches.push(format!("{rel_str}:{}: {}", i + 1, shown.trim_end()));
                    }
                }
            }
            (matches, total)
        })
        .await;

        let (matches, total) = match search {
            Ok(r) => r,
            Err(e) => return ToolOutput::err(format!("search failed: {e}")),
        };

        if matches.is_empty() {
            return ToolOutput::ok(format!("No matches for `{pattern_owned}`."));
        }

        let mut out = matches.join("\n");
        if total > matches.len() {
            out.push_str(&format!(
                "\n\n[{} of {total} matches shown; narrow the pattern or set `include`]",
                matches.len()
            ));
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
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(
            tmp.path().join("src/main.rs"),
            "fn main() {\n    let secret = 1;\n}\n",
        )
        .unwrap();
        std::fs::write(tmp.path().join("notes.md"), "the secret is here\n").unwrap();
        let ws = Workspace::open(tmp.path()).unwrap();
        (tmp, ToolCtx::new(ws))
    }

    #[tokio::test]
    async fn finds_matches_with_path_and_line() {
        let (_t, ctx) = ctx();
        let out = Grep
            .run(&serde_json::json!({"pattern": "secret"}), &ctx)
            .await;
        assert!(out.content.contains("src/main.rs:2:"));
        assert!(out.content.contains("notes.md:1:"));
    }

    #[tokio::test]
    async fn include_filter_narrows_by_glob() {
        let (_t, ctx) = ctx();
        let out = Grep
            .run(
                &serde_json::json!({"pattern": "secret", "include": "*.rs"}),
                &ctx,
            )
            .await;
        assert!(out.content.contains("src/main.rs"));
        assert!(!out.content.contains("notes.md"));
    }

    #[tokio::test]
    async fn case_insensitive_by_default() {
        let (_t, ctx) = ctx();
        let out = Grep
            .run(&serde_json::json!({"pattern": "SECRET"}), &ctx)
            .await;
        assert!(out.content.contains("src/main.rs"));

        let out = Grep
            .run(
                &serde_json::json!({"pattern": "SECRET", "case_sensitive": true}),
                &ctx,
            )
            .await;
        assert!(out.content.contains("No matches"));
    }

    #[tokio::test]
    async fn regex_metacharacters_work() {
        let (_t, ctx) = ctx();
        let out = Grep
            .run(&serde_json::json!({"pattern": r"let \w+ = \d+;"}), &ctx)
            .await;
        assert!(out.content.contains("src/main.rs:2:"));
    }

    #[tokio::test]
    async fn an_invalid_regex_is_reported_not_panicked() {
        let (_t, ctx) = ctx();
        let out = Grep.run(&serde_json::json!({"pattern": "["}), &ctx).await;
        assert!(out.is_error);
        assert!(out.content.contains("not a valid regular expression"));
    }

    #[tokio::test]
    async fn reports_no_matches_clearly() {
        let (_t, ctx) = ctx();
        let out = Grep
            .run(&serde_json::json!({"pattern": "zzzz"}), &ctx)
            .await;
        assert!(!out.is_error);
        assert!(out.content.contains("No matches"));
    }
}
