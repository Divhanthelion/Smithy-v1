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
use regex::{Regex, RegexBuilder};
use serde_json::Value;
use std::path::Path;

use crate::registry::{ExecutionControl, Tool, ToolCtx};
use crate::sandbox::WorkspaceReader;
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
        self.run_controlled(args, ctx, &ExecutionControl::default())
            .await
    }

    async fn run_controlled(
        &self,
        args: &Value,
        ctx: &ToolCtx,
        control: &ExecutionControl,
    ) -> ToolOutput {
        if let Err(reason) = control.check() {
            return ToolOutput::err(reason);
        }
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
        if let Err(reason) = control.check() {
            return ToolOutput::err(reason);
        }
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
        if let Err(reason) = control.check() {
            return ToolOutput::err(reason);
        }
        let reader = match ctx.workspace.try_reader() {
            Ok(reader) => reader,
            Err(error) => return ToolOutput::err(error),
        };
        let pattern_owned = pattern.to_string();
        let worker_control = control.clone();
        let worker_cancel = tokio_util::sync::CancellationToken::new();
        let worker_cancelled = worker_cancel.clone();
        let _worker_guard = WorkerCancellation(worker_cancel);

        let search = tokio::task::spawn_blocking(move || {
            search_with_hook(
                SearchInput {
                    root: &root,
                    workspace_root: &ws_root,
                    reader: &reader,
                    re: &re,
                    include: include.as_deref(),
                    control: &worker_control,
                    worker_cancelled: &worker_cancelled,
                },
                &mut |_| {},
                &mut |_| {},
            )
        })
        .await;

        let (matches, total) = match search {
            Ok(Ok(result)) => result,
            Ok(Err(reason)) => return ToolOutput::err(reason),
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

    fn owns_cancellation_cleanup(&self) -> bool {
        true
    }
}

/// Cancels the worker if the async join future is ever dropped.
///
/// Registry ordinarily observes controlled cleanup, but this guard closes the
/// stronger ownership hole: task teardown must not detach an ignore walk merely
/// because its JoinHandle future disappeared.
struct WorkerCancellation(tokio_util::sync::CancellationToken);

impl Drop for WorkerCancellation {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

struct SearchInput<'a> {
    root: &'a Path,
    workspace_root: &'a Path,
    reader: &'a WorkspaceReader,
    re: &'a Regex,
    include: Option<&'a str>,
    control: &'a ExecutionControl,
    worker_cancelled: &'a tokio_util::sync::CancellationToken,
}

fn search_with_hook(
    input: SearchInput<'_>,
    before_read: &mut dyn FnMut(&Path),
    after_read: &mut dyn FnMut(&Path),
) -> Result<(Vec<String>, usize), String> {
    let mut matches: Vec<String> = Vec::new();
    let mut total = 0usize;

    check_search(input.control, input.worker_cancelled)?;
    for entry in WalkBuilder::new(input.root)
        .hidden(false)
        .follow_links(false)
        .require_git(false)
        .build()
        .flatten()
    {
        check_search(input.control, input.worker_cancelled)?;
        if !entry.file_type().map(|kind| kind.is_file()).unwrap_or(false) {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(input.workspace_root) else {
            continue;
        };
        let relative = relative.to_path_buf();
        let relative_display = relative.to_string_lossy().replace('\\', "/");
        if input.include.is_some_and(|pattern| {
            !super::glob::matches_include(pattern, &relative_display)
        }) {
            continue;
        }

        check_search(input.control, input.worker_cancelled)?;
        before_read(&relative);
        check_search(input.control, input.worker_cancelled)?;
        // The ignore walker is only a candidate enumerator. Reopen the relative
        // name through the duplicated root capability after the hook/race
        // window; a deletion or symlink swap is skipped, never ambient-read.
        let Ok(Some(contents)) = input.reader.read_limited(&relative, MAX_FILE_BYTES) else {
            continue;
        };
        after_read(&relative);
        check_search(input.control, input.worker_cancelled)?;
        let Ok(text) = String::from_utf8(contents) else {
            continue;
        };
        for (line_number, line) in text.lines().enumerate() {
            check_search(input.control, input.worker_cancelled)?;
            if !input.re.is_match(line) {
                continue;
            }
            check_search(input.control, input.worker_cancelled)?;
            total += 1;
            if matches.len() < MAX_MATCHES {
                let shown = if line.chars().count() > MAX_LINE_CHARS {
                    line.chars().take(MAX_LINE_CHARS).collect::<String>() + "…"
                } else {
                    line.to_string()
                };
                matches.push(format!(
                    "{relative_display}:{}: {}",
                    line_number + 1,
                    shown.trim_end()
                ));
            }
        }
    }
    check_search(input.control, input.worker_cancelled)?;
    Ok((matches, total))
}

fn check_search(
    control: &ExecutionControl,
    worker_cancelled: &tokio_util::sync::CancellationToken,
) -> Result<(), String> {
    if worker_cancelled.is_cancelled() {
        return Err("stopped because the grep task was dropped".into());
    }
    control.check()
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

    /// The ignore walker can observe a regular file and then lose a race before
    /// its contents are opened. Replacing that name with an external symlink
    /// must be skipped by the capability read rather than leaking the sentinel.
    #[cfg(unix)]
    #[test]
    fn a_walked_file_swapped_to_an_external_symlink_is_not_read() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("victim.txt"), "safe text").unwrap();
        std::fs::write(
            outside.path().join("secret.txt"),
            "EXTERNAL_GREP_SENTINEL",
        )
        .unwrap();
        let ws = Workspace::open(workspace.path()).unwrap();
        let reader = ws.try_reader().unwrap();
        let root = ws.absolute_real(".").unwrap();
        let workspace_root = ws.root().to_path_buf();
        let re = RegexBuilder::new("EXTERNAL_GREP_SENTINEL").build().unwrap();
        let control = ExecutionControl::default();
        let worker_cancelled = tokio_util::sync::CancellationToken::new();
        let mut swapped = false;
        let (matches, total) = search_with_hook(
            SearchInput {
                root: &root,
                workspace_root: &workspace_root,
                reader: &reader,
                re: &re,
                include: None,
                control: &control,
                worker_cancelled: &worker_cancelled,
            },
            &mut |relative| {
                if relative == Path::new("victim.txt") {
                    std::fs::remove_file(workspace.path().join(relative)).unwrap();
                    symlink(
                        outside.path().join("secret.txt"),
                        workspace.path().join(relative),
                    )
                    .unwrap();
                    swapped = true;
                }
            },
            &mut |_| {},
        )
        .unwrap();
        assert!(swapped, "the deterministic race seam did not run");
        assert_eq!(total, 0);
        assert!(matches.is_empty(), "{matches:?}");
    }

    /// Cancellation used to win Registry's select by dropping the JoinHandle
    /// while the blocking ignore walk kept opening every remaining file. Stop
    /// at the pre-open seam so not even the file that observed Stop is reopened.
    #[test]
    fn a_started_tree_walk_opens_no_files_after_cancellation() {
        let (tmp, ctx) = ctx();
        for number in 0..32 {
            std::fs::write(tmp.path().join(format!("file-{number:02}.txt")), "needle\n").unwrap();
        }
        let reader = ctx.workspace.try_reader().unwrap();
        let root = ctx.workspace.absolute_real(".").unwrap();
        let workspace_root = ctx.workspace.root().to_path_buf();
        let re = Regex::new("needle").unwrap();
        let (control, stopper) = ExecutionControl::for_turn(
            crate::registry::ExecutionToken::new(1, 1),
            std::time::Duration::from_secs(60),
        );
        let worker_cancelled = tokio_util::sync::CancellationToken::new();
        let mut reached_open = 0usize;
        let mut completed_open = 0usize;

        let result = search_with_hook(
            SearchInput {
                root: &root,
                workspace_root: &workspace_root,
                reader: &reader,
                re: &re,
                include: None,
                control: &control,
                worker_cancelled: &worker_cancelled,
            },
            &mut |_| {
                reached_open += 1;
                stopper.stop();
            },
            &mut |_| completed_open += 1,
        );

        assert_eq!(result.unwrap_err(), "stopped by user");
        assert_eq!(reached_open, 1, "the walk advanced after observing Stop");
        assert_eq!(
            completed_open, 0,
            "a capability read started after cancellation"
        );
    }

    /// A dropped join once detached one blocking walk per cancelled grep. Run
    /// the cooperative boundary repeatedly and require every worker to leave
    /// before starting the next cancellation.
    #[tokio::test]
    async fn repeated_cancellations_do_not_accumulate_tree_walk_workers() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        struct ActiveWorker(Arc<AtomicUsize>);
        impl Drop for ActiveWorker {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }

        let (tmp, ctx) = ctx();
        for number in 0..32 {
            std::fs::write(tmp.path().join(format!("file-{number:02}.txt")), "needle\n").unwrap();
        }
        let root = ctx.workspace.absolute_real(".").unwrap();
        let workspace_root = ctx.workspace.root().to_path_buf();
        let active = Arc::new(AtomicUsize::new(0));
        let re = Regex::new("needle").unwrap();

        for turn in 0..8 {
            let reader = ctx.workspace.try_reader().unwrap();
            let root = root.clone();
            let workspace_root = workspace_root.clone();
            let active_worker = active.clone();
            let re = re.clone();
            let (control, stopper) = ExecutionControl::for_turn(
                crate::registry::ExecutionToken::new(1, turn),
                std::time::Duration::from_secs(60),
            );
            let worker_cancelled = tokio_util::sync::CancellationToken::new();

            let result = tokio::task::spawn_blocking(move || {
                active_worker.fetch_add(1, Ordering::SeqCst);
                let _active = ActiveWorker(active_worker);
                search_with_hook(
                    SearchInput {
                        root: &root,
                        workspace_root: &workspace_root,
                        reader: &reader,
                        re: &re,
                        include: None,
                        control: &control,
                        worker_cancelled: &worker_cancelled,
                    },
                    &mut |_| stopper.stop(),
                    &mut |_| {},
                )
            })
            .await
            .unwrap();

            assert_eq!(result.unwrap_err(), "stopped by user");
            assert_eq!(
                active.load(Ordering::SeqCst),
                0,
                "cancelled grep worker {turn} was still alive"
            );
        }
    }

    /// A deadline reached before dispatch must remain a stopped result; reporting
    /// "No matches" hid that the search never ran.
    #[tokio::test]
    async fn an_observed_deadline_returns_a_stopped_tool_output() {
        let (_tmp, ctx) = ctx();
        let (control, _stopper) = ExecutionControl::with_deadline(
            crate::registry::ExecutionToken::new(1, 1),
            tokio::time::Instant::now(),
        );

        let output = Grep
            .run_controlled(
                &serde_json::json!({"pattern": "secret"}),
                &ctx,
                &control,
            )
            .await;

        assert!(output.is_error);
        assert_eq!(output.content, "turn deadline reached");
    }
}
