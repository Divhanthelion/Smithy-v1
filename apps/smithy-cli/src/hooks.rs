//! Review and shell approval that wait on the terminal, not a modal.
//!
//! Same seam as the editor: both are [`ToolHook`]s. The loop never learns that
//! the human is on stdin.

use std::io::{self, BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use similar::TextDiff;
use smithy_tools::{yolo_skips_bash, yolo_skips_write, HookDecision, ToolCall, ToolCtx, ToolHook};

pub struct ShellApprovalHook {
    pub auto_approve: Arc<AtomicBool>,
}

#[async_trait]
impl ToolHook for ShellApprovalHook {
    fn name(&self) -> &'static str {
        "shell-approval"
    }

    async fn before(&self, call: &ToolCall, args: &Value, ctx: &ToolCtx) -> HookDecision {
        if call.name != "bash" {
            return HookDecision::Allow;
        }
        let command = args
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();

        if self.auto_approve.load(Ordering::Relaxed)
            && yolo_skips_bash(&command, ctx.workspace.root())
        {
            return HookDecision::Allow;
        }

        let _hold = ctx.gate.hold();
        let prompt = format!("run this command?\n  {command}\n[y]es  [n]o  ");
        match ask(&prompt) {
            Answer::Yes => HookDecision::Allow,
            Answer::No | Answer::Eof => HookDecision::Deny(
                "the user declined to run this command. Try a different approach, or explain why \
                 it is necessary."
                    .into(),
            ),
        }
    }
}

pub struct WriteReviewHook {
    pub auto_approve: Arc<AtomicBool>,
}

#[async_trait]
impl ToolHook for WriteReviewHook {
    fn name(&self) -> &'static str {
        "write-review"
    }

    async fn before(&self, call: &ToolCall, args: &Value, ctx: &ToolCtx) -> HookDecision {
        let Some(path) = args.get("path").and_then(|v| v.as_str()) else {
            return HookDecision::Allow;
        };

        if self.auto_approve.load(Ordering::Relaxed) && yolo_skips_write(&ctx.workspace, path) {
            return HookDecision::Allow;
        }

        let new_content = match proposed_content(call, args, ctx, path) {
            Ok(Some(c)) => c,
            Ok(None) => return HookDecision::Allow,
            Err(e) => return HookDecision::Deny(e),
        };

        let old_content = ctx.workspace.read_to_string(path).unwrap_or_default();
        if old_content == new_content {
            return HookDecision::Deny(format!("`{path}` already has exactly that content"));
        }

        let display = ctx.workspace.display_path(path);
        let diff = unified(&display, &old_content, &new_content);
        let _hold = ctx.gate.hold();
        let prompt = format!("{diff}\napply `{display}`?\n[y]es  [n]o  ");
        match ask(&prompt) {
            Answer::Yes => match ctx.workspace.write(path, &new_content) {
                Ok(()) => HookDecision::Fulfilled(format!(
                    "Your proposed change to `{display}` was accepted in full and is now on disk."
                )),
                Err(e) => HookDecision::Deny(format!("could not write `{display}`: {e}")),
            },
            Answer::No | Answer::Eof => HookDecision::Deny(format!(
                "Your proposed change to `{display}` was rejected. The file is unchanged."
            )),
        }
    }
}

fn proposed_content(
    call: &ToolCall,
    args: &Value,
    ctx: &ToolCtx,
    path: &str,
) -> Result<Option<String>, String> {
    match call.name.as_str() {
        "write" => Ok(args
            .get("content")
            .and_then(|v| v.as_str())
            .map(str::to_string)),
        "edit" => {
            let (Some(old), Some(new)) = (
                args.get("old_string").and_then(|v| v.as_str()),
                args.get("new_string").and_then(|v| v.as_str()),
            ) else {
                return Ok(None);
            };
            let Ok(current) = ctx.workspace.read_to_string(path) else {
                return Ok(None);
            };
            match smithy_tools::fuzzy::find(&current, old) {
                Some(m) if m.auto_apply => {
                    let mut updated = String::with_capacity(current.len() + new.len());
                    updated.push_str(&current[..m.byte_offset]);
                    updated.push_str(new);
                    updated.push_str(&current[m.byte_offset + m.matched_text.len()..]);
                    Ok(Some(updated))
                }
                _ => Ok(None),
            }
        }
        _ => Ok(None),
    }
}

fn unified(path: &str, old: &str, new: &str) -> String {
    format!(
        "{}",
        TextDiff::from_lines(old, new)
            .unified_diff()
            .context_radius(3)
            .header(&format!("a/{path}"), &format!("b/{path}"))
    )
}

enum Answer {
    Yes,
    No,
    Eof,
}

fn ask(prompt: &str) -> Answer {
    let mut stderr = io::stderr().lock();
    let _ = write!(stderr, "{prompt}");
    let _ = stderr.flush();
    let mut line = String::new();
    match io::stdin().lock().read_line(&mut line) {
        Ok(0) => Answer::Eof,
        Ok(_) => match line.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" | "a" | "apply" => Answer::Yes,
            _ => Answer::No,
        },
        Err(_) => Answer::Eof,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unified_mentions_both_sides() {
        let d = unified("n.rs", "a\n", "b\n");
        assert!(d.contains("a/n.rs"));
        assert!(d.contains("-a"));
        assert!(d.contains("+b"));
    }
}
