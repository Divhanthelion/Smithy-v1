//! Find files by name pattern, gitignore-aware.
//!
//! ## A note on the sandbox boundary
//!
//! `glob` and `grep` walk the tree with the `ignore` crate, which takes ordinary
//! paths rather than going through the [`crate::Workspace`] capability. Two
//! things keep that sound:
//!
//! - the walk is rooted at a path resolved with `Workspace::absolute_real`,
//!   which canonicalises and re-checks containment, and every result is then
//!   required to sit under the canonical root before it can be named. (This
//!   note used to claim the re-check went through `Workspace::relative`. It
//!   does not, and the textual prefix check it actually performs is what let
//!   the symlink escape through — see `Workspace::absolute_real`.);
//! - `ignore` does not follow symlinks unless asked, so a symlink out of the
//!   tree is listed as a link and never descended into.
//!
//! These are read-only discovery tools; every actual read still goes through the
//! capability. Stated plainly because it is the one place the confinement is
//! argued rather than enforced by construction.

use async_trait::async_trait;
use ignore::WalkBuilder;
use serde_json::Value;
use std::path::PathBuf;

use crate::registry::{Tool, ToolCtx};
use crate::schema::{arg_str, arg_str_opt, ToolDefinition, ToolOutput, ToolParameter};

const MAX_RESULTS: usize = 300;

pub struct Glob;

#[async_trait]
impl Tool for Glob {
    fn name(&self) -> &'static str {
        "glob"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "glob",
            "Find files whose path matches a pattern, respecting .gitignore. Supports `*` \
             (within a path segment), `**` (across segments), and `?`. Example: `src/**/*.rs`.",
            vec![
                ToolParameter::string("pattern", "Glob pattern, e.g. `**/*.rs`.", true),
                ToolParameter::string(
                    "path",
                    "Directory to search under: Project-relative, or an absolute path in the \
                     Project or scratch (default: Project root).",
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

        let matcher = match GlobMatcher::new(pattern) {
            Ok(m) => m,
            Err(e) => return ToolOutput::err(e),
        };
        let project_root = ctx.workspace.root().to_path_buf();
        let scratch_root = ctx.workspace.scratch_root().map(PathBuf::from);

        let found = tokio::task::spawn_blocking(move || {
            let mut found: Vec<String> = Vec::new();
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
                let Some(shown) = crate::sandbox::shown_if_contained(
                    entry.path(),
                    &project_root,
                    scratch_root.as_deref(),
                ) else {
                    continue;
                };
                if matcher.matches(&shown) {
                    found.push(shown);
                }
            }
            found.sort();
            found
        })
        .await;

        let mut found = match found {
            Ok(f) => f,
            Err(e) => return ToolOutput::err(format!("glob walk failed: {e}")),
        };

        if found.is_empty() {
            // Naming the ignore rules matters. A real session asked for
            // `**/PLUGIN_PLAN.md` in a repository whose `.gitignore` held
            // `*.md`; this said only "No files match", and the plan the user had
            // asked to be implemented looked absent. It was there, and `read`
            // could reach it — nothing said so.
            return ToolOutput::ok(format!(
                "No files match `{pattern}`. Note that this search skips anything the \
                 repository ignores (`.gitignore`), so an ignored file will not appear here even \
                 though it exists — if you were given an exact path, try `read` on it directly."
            ));
        }

        let total = found.len();
        found.truncate(MAX_RESULTS);
        let mut out = found.join("\n");
        if total > MAX_RESULTS {
            out.push_str(&format!("\n\n[{MAX_RESULTS} of {total} matches shown]"));
        }
        ToolOutput::ok(out)
    }
}

/// A small glob matcher supporting `*`, `**`, and `?`.
///
/// Hand-rolled rather than pulling in `globset`: the whole surface is one
/// function, and the semantics of `**` differ enough between glob libraries that
/// owning them is clearer than documenting which variant we inherited.
struct GlobMatcher {
    pattern: String,
}

impl GlobMatcher {
    fn new(pattern: &str) -> Result<Self, String> {
        if pattern.trim().is_empty() {
            return Err("glob pattern is empty".into());
        }
        Ok(GlobMatcher {
            pattern: pattern.trim_start_matches("./").to_string(),
        })
    }

    fn matches(&self, path: &str) -> bool {
        // A bare `*.rs` should match at any depth, which is what a user means
        // even though it is not what the pattern literally says.
        if !self.pattern.contains('/') {
            let base = path.rsplit('/').next().unwrap_or(path);
            return glob_match(&self.pattern, base);
        }
        glob_match(&self.pattern, path)
    }
}

/// Match a workspace-relative path against an `include`-style glob.
///
/// Shared with [`super::grep`] so both tools interpret `include: "*.rs"` the
/// same way — a bare pattern with no `/` matches the basename at any depth.
pub fn matches_include(pattern: &str, path: &str) -> bool {
    match GlobMatcher::new(pattern) {
        Ok(m) => m.matches(path),
        Err(_) => false,
    }
}

/// Backtracking glob match over path segments.
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<&str> = pattern.split('/').collect();
    let t: Vec<&str> = text.split('/').collect();
    segments_match(&p, &t)
}

fn segments_match(p: &[&str], t: &[&str]) -> bool {
    if p.is_empty() {
        return t.is_empty();
    }
    if p[0] == "**" {
        // `**` matches zero or more segments.
        for skip in 0..=t.len() {
            if segments_match(&p[1..], &t[skip..]) {
                return true;
            }
        }
        return false;
    }
    if t.is_empty() {
        return false;
    }
    if !segment_match(p[0], t[0]) {
        return false;
    }
    segments_match(&p[1..], &t[1..])
}

/// Match a single path segment, where `*` matches any run of non-`/` characters
/// and `?` matches exactly one.
fn segment_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut backtrack) = (usize::MAX, 0usize);

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = pi;
            backtrack = ti;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            backtrack += 1;
            ti = backtrack;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::Workspace;

    fn ctx() -> (tempfile::TempDir, ToolCtx) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("src/inner")).unwrap();
        std::fs::write(tmp.path().join("src/main.rs"), "").unwrap();
        std::fs::write(tmp.path().join("src/inner/deep.rs"), "").unwrap();
        std::fs::write(tmp.path().join("README.md"), "").unwrap();
        let ws = Workspace::open(tmp.path()).unwrap();
        (tmp, ToolCtx::new(ws))
    }

    #[test]
    fn segment_matching() {
        assert!(segment_match("*.rs", "main.rs"));
        assert!(!segment_match("*.rs", "main.md"));
        assert!(segment_match("m?in.rs", "main.rs"));
        assert!(segment_match("*", "anything"));
        assert!(segment_match("a*c", "abbbc"));
        assert!(!segment_match("a*c", "abbbd"));
    }

    #[test]
    fn double_star_crosses_segments() {
        assert!(glob_match("src/**/*.rs", "src/inner/deep.rs"));
        assert!(glob_match("src/**/*.rs", "src/main.rs"));
        assert!(!glob_match("src/**/*.rs", "tests/main.rs"));
        assert!(glob_match("**/*.rs", "a/b/c/d.rs"));
    }

    #[test]
    fn single_star_does_not_cross_segments() {
        assert!(!glob_match("src/*.rs", "src/inner/deep.rs"));
        assert!(glob_match("src/*.rs", "src/main.rs"));
    }

    #[tokio::test]
    async fn finds_by_extension_at_any_depth() {
        let (_t, ctx) = ctx();
        let out = Glob
            .run(&serde_json::json!({"pattern": "*.rs"}), &ctx)
            .await;
        assert!(out.content.contains("src/main.rs"));
        assert!(out.content.contains("src/inner/deep.rs"));
        assert!(!out.content.contains("README.md"));
    }

    #[tokio::test]
    async fn results_are_sorted() {
        let (_t, ctx) = ctx();
        let out = Glob
            .run(&serde_json::json!({"pattern": "**/*.rs"}), &ctx)
            .await;
        let lines: Vec<&str> = out.content.lines().collect();
        let mut sorted = lines.clone();
        sorted.sort();
        assert_eq!(lines, sorted);
    }

    #[tokio::test]
    async fn reports_no_matches_clearly() {
        let (_t, ctx) = ctx();
        let out = Glob
            .run(&serde_json::json!({"pattern": "*.zzz"}), &ctx)
            .await;
        assert!(!out.is_error);
        assert!(out.content.contains("No files match"));
    }

    #[tokio::test]
    async fn respects_gitignore() {
        let (_t, ctx) = ctx();
        std::fs::write(ctx.workspace.root().join(".gitignore"), "ignored.rs\n").unwrap();
        std::fs::write(ctx.workspace.root().join("ignored.rs"), "").unwrap();
        let out = Glob
            .run(&serde_json::json!({"pattern": "*.rs"}), &ctx)
            .await;
        assert!(!out.content.contains("ignored.rs"), "got: {}", out.content);
    }
}

/// What the walker can and cannot see.
///
/// Written after a real session lost a step to it: the user asked the agent to
/// implement `PLUGIN_PLAN.md`, `glob **/PLUGIN_PLAN.md` answered "No files
/// match", and the plan was reachable only because the model guessed the exact
/// path and used `read`. The repository's `.gitignore` contained `*.md`.
///
/// That is the walker behaving as designed — but the consequence is worth
/// pinning down, because "the design document is invisible to search" is a
/// surprising thing for a coding agent to be true of.
#[cfg(test)]
mod visibility {
    use super::*;
    use crate::sandbox::Workspace;

    fn workspace_with(files: &[(&str, &str)]) -> (tempfile::TempDir, ToolCtx) {
        let tmp = tempfile::tempdir().unwrap();
        for (path, contents) in files {
            let full = tmp.path().join(path);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(full, contents).unwrap();
        }
        let ws = Workspace::open(tmp.path()).unwrap();
        (tmp, ToolCtx::new(ws))
    }

    /// `**/` must match a file sitting at the workspace root, not only nested
    /// ones. This was the first hypothesis for the miss above, and it is wrong —
    /// which is worth keeping a test for so it stays wrong.
    #[tokio::test]
    async fn double_star_matches_a_file_at_the_workspace_root() {
        let (_t, ctx) = workspace_with(&[("PLUGIN_PLAN.md", "# plan"), ("src/deep.md", "x")]);
        let out = Glob
            .run(&serde_json::json!({"pattern": "**/PLUGIN_PLAN.md"}), &ctx)
            .await;
        assert!(
            out.content.contains("PLUGIN_PLAN.md"),
            "`**/` must match at the root; got: {}",
            out.content
        );
    }

    /// The actual cause: an ignored file is invisible to `glob`, whether or not
    /// the directory is a git checkout. `read` still reaches it by exact path,
    /// which is the only reason that session recovered.
    #[tokio::test]
    async fn a_gitignored_file_is_invisible_to_glob() {
        let (_t, ctx) = workspace_with(&[
            (".gitignore", "*.md\n!README.md\n"),
            ("PLUGIN_PLAN.md", "# plan"),
            ("README.md", "# readme"),
        ]);
        let out = Glob
            .run(&serde_json::json!({"pattern": "**/*.md"}), &ctx)
            .await;
        assert!(
            !out.content.contains("PLUGIN_PLAN.md"),
            "ignored files are excluded by design; got: {}",
            out.content
        );
        assert!(
            out.content.contains("README.md"),
            "a negated rule must still be visible; got: {}",
            out.content
        );
    }
}
