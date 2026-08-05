//! Shell execution.
//!
//! Ported from coda, then changed to drain nonblocking pipes in the worker that
//! owns the child. Separate `read_to_end` threads prevented pipe deadlock but
//! leaked forever when a setsid descendant retained a write end. The worker now
//! bounds head/tail capture, kills its process group on timeout/Stop, and closes
//! capture with an explicit warning if a process deliberately escapes the group.
//!
//! The guardrail in [`crate::sandbox::check_bash`] runs first, but the real
//! control for shell commands is an approval [`crate::ToolHook`]: unlike the
//! filesystem tools, a subprocess is not confined by the workspace capability.

use std::collections::VecDeque;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::Value;

use crate::registry::{middle_truncate, ExecutionControl, Tool, ToolCtx};
use crate::sandbox::check_bash;
use crate::schema::{arg_i64, arg_str, ToolDefinition, ToolOutput, ToolParameter};

const DEFAULT_TIMEOUT_S: u64 = 30;
const MAX_TIMEOUT_S: u64 = 600;
const MAX_OUTPUT_CHARS: usize = 30_000;
/// One capture budget, shared between the streams rather than split in half in
/// advance.
///
/// A fixed half each was simple but wrong for the common case: most commands
/// write only to stdout, and an empty stderr's reservation was spent on
/// nothing, so `cargo test` retained 14k of the 28k it could have. Both buffers
/// now hold the whole budget and the final combiner allocates it against what
/// each stream actually produced. The margin below `MAX_OUTPUT_CHARS` is what
/// leaves room for the truncation notices, so the combined char cut stays a
/// backstop and cannot make a second cut through either stream's retained ends.
const MAX_CAPTURE_BYTES_TOTAL: usize = 28_000;
/// `drain_available` used to read until `WouldBlock`. A writer faster than the
/// reader could therefore keep one poll inside that loop forever, preventing
/// the timeout and Stop checks that own the process. Eight pipe-sized reads put
/// a hard scheduling boundary between continuous output and command control.
const MAX_DRAIN_READS_PER_POLL: usize = 8;
const MAX_DRAIN_BYTES_PER_POLL: usize = 64 * 1024;
/// How long a stopped command may run its own cleanup before SIGKILL.
///
/// SIGKILL alone stops the command but takes its lock files, temporary
/// directories, and partially written output with it. A handler needs only
/// milliseconds to unlink what it created; anything that wants longer is
/// exactly what the unconditional SIGKILL below is for. Short enough that Stop
/// still feels immediate.
const TERMINATE_GRACE: Duration = Duration::from_millis(200);
const TERMINATE_POLL: Duration = Duration::from_millis(10);
/// Drain passes allowed after the group is reaped.
///
/// A pipe holds at most its kernel buffer once every writer is dead, which one
/// 64 KiB pass already covers; the rest is margin. It is bounded rather than
/// "until EOF" because an escaped descendant can keep writing forever, and
/// establishing *that* is what the remaining non-EOF result reports.
const MAX_FINAL_DRAIN_POLLS: usize = 4;

pub struct Bash;

#[async_trait]
impl Tool for Bash {
    fn name(&self) -> &'static str {
        "bash"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "bash",
            "Run a shell command from the workspace root and return combined stdout and stderr. \
             Default timeout 30 seconds. Output is capped at 30000 characters, truncated in the \
             middle so both the command's start and its error tail survive. Commands must be \
             non-interactive.",
            vec![
                ToolParameter::string("command", "The shell command to run.", true),
                ToolParameter::integer(
                    "timeout",
                    "Timeout in seconds (default 30, max 600).",
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
        let command = match arg_str(args, "command") {
            Ok(c) => c.to_string(),
            Err(e) => return ToolOutput::err(e),
        };
        if let Err(e) = check_bash(&command) {
            return ToolOutput::err(e);
        }
        let timeout = Duration::from_secs(
            arg_i64(args, "timeout")
                .map(|t| t.clamp(1, MAX_TIMEOUT_S as i64) as u64)
                .unwrap_or(DEFAULT_TIMEOUT_S),
        );
        let cwd = ctx.workspace.root().to_path_buf();
        let control = control.clone();

        match tokio::task::spawn_blocking(move || {
            run_with_control(&command, &cwd, timeout, &control)
        })
        .await
        {
            Ok(Ok(out)) if out.starts_with("[command stopped:") => ToolOutput::err(out),
            Ok(Ok(out)) => ToolOutput::ok(out),
            Ok(Err(e)) => ToolOutput::err(e),
            Err(e) => ToolOutput::err(format!("shell task failed: {e}")),
        }
    }

    fn owns_cancellation_cleanup(&self) -> bool {
        true
    }
}

/// Signal a whole process group.
///
/// The child is its own group leader — see `process_group(0)` below — so its pid
/// doubles as the group id.
#[cfg(unix)]
fn signal_process_group(pid: u32, signal: libc::c_int) {
    // SAFETY: `killpg` takes a group id and a signal number and touches no
    // memory we own. A group that has already exited yields `ESRCH`, which is
    // exactly the case we do not care about.
    unsafe {
        libc::killpg(pid as libc::pid_t, signal);
    }
}

/// Spawn `sh -c command`, capture combined output, kill it if it overruns.
pub fn run_with_timeout(
    command: &str,
    cwd: &std::path::Path,
    timeout: Duration,
) -> Result<String, String> {
    run_with_control(command, cwd, timeout, &ExecutionControl::default())
}

fn run_with_control(
    command: &str,
    cwd: &std::path::Path,
    timeout: Duration,
    control: &ExecutionControl,
) -> Result<String, String> {
    #[cfg(unix)]
    {
        run_with_control_unix(command, cwd, timeout, control)
    }
    #[cfg(not(unix))]
    {
        let _ = (command, cwd, timeout, control);
        Err("shell execution requires Unix process-group and nonblocking-pipe support".into())
    }
}

#[cfg(unix)]
fn run_with_control_unix(
    command: &str,
    cwd: &std::path::Path,
    timeout: Duration,
    control: &ExecutionControl,
) -> Result<String, String> {
    let mut builder = Command::new("sh");
    builder
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Put the shell in a process group of its own, so the timeout can kill
    // everything it started rather than just the shell.
    //
    // Without this, `Child::kill` reaps `sh` and leaves any backgrounded
    // grandchild running — still holding the write end of the stdout pipe it
    // inherited. The reader thread below then blocks in `read_to_end` until
    // *that* process exits, which for the runaway command this timeout exists
    // to stop is never. The observable failure is not a slow tool call; it is a
    // turn that never ends and a leaked blocking thread.
    use std::os::unix::process::CommandExt;
    builder.process_group(0);

    // spawn_blocking jobs can sit queued after Registry dispatched them. The
    // async side's check is not authority for this side effect: recheck in the
    // worker at the final boundary immediately before Command::spawn.
    control
        .check()
        .map_err(|reason| format!("[command stopped: {reason}]"))?;
    let mut child = builder
        .spawn()
        .map_err(|e| format!("failed to spawn shell: {e}"))?;
    let pid = child.id();

    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    if stdout.is_none() || stderr.is_none() {
        terminate_and_cleanup(pid, &mut child, &mut stdout, &mut stderr);
        return Err("spawned shell did not provide its requested output pipes".into());
    }
    if let Err(error) = set_nonblocking(stdout.as_ref().expect("checked above")) {
        terminate_and_cleanup(pid, &mut child, &mut stdout, &mut stderr);
        return Err(error);
    }
    if let Err(error) = set_nonblocking(stderr.as_ref().expect("checked above")) {
        terminate_and_cleanup(pid, &mut child, &mut stdout, &mut stderr);
        return Err(error);
    }
    let mut out = BoundedBytes::new(MAX_CAPTURE_BYTES_TOTAL);
    let mut err = BoundedBytes::new(MAX_CAPTURE_BYTES_TOTAL);
    let mut stdout_eof = false;
    let mut stderr_eof = false;

    let command_deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let mut stopped = None;
    let status = loop {
        match drain_available(stdout.as_mut().expect("pipe retained"), &mut out) {
            Ok(drained) => stdout_eof |= drained == Drained::Eof,
            Err(error) => {
                terminate_and_cleanup(pid, &mut child, &mut stdout, &mut stderr);
                return Err(error);
            }
        }
        match drain_available(stderr.as_mut().expect("pipe retained"), &mut err) {
            Ok(drained) => stderr_eof |= drained == Drained::Eof,
            Err(error) => {
                terminate_and_cleanup(pid, &mut child, &mut stdout, &mut stderr);
                return Err(error);
            }
        }
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                // Stop and timeout both collect through the same path: the last
                // thing a command prints before it is stopped is usually the
                // most diagnostic thing it prints, and the old ordering closed
                // the pipes as part of killing and threw it away.
                if let Err(reason) = control.check() {
                    let (out_eof, err_eof) = stop_and_collect(
                        pid, &mut child, &mut stdout, &mut stderr, &mut out, &mut err,
                    );
                    stdout_eof |= out_eof;
                    stderr_eof |= err_eof;
                    stopped = Some(reason);
                    break None;
                }
                if Instant::now() >= command_deadline {
                    let (out_eof, err_eof) = stop_and_collect(
                        pid, &mut child, &mut stdout, &mut stderr, &mut out, &mut err,
                    );
                    stdout_eof |= out_eof;
                    stderr_eof |= err_eof;
                    timed_out = true;
                    break None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                terminate_and_cleanup(pid, &mut child, &mut stdout, &mut stderr);
                return Err(format!("error waiting on command: {e}"));
            }
        }
    };

    // Drain what the kernel already has, but never wait for EOF. A descendant
    // can deliberately call setsid(2), leave our process group, and retain the
    // inherited write end indefinitely. Nonblocking reads let this worker close
    // its own descriptors and return without leaking an unjoinable reader
    // thread. The escaped process itself is outside Smithy's kill boundary.
    if let Some(stdout_pipe) = stdout.as_mut() {
        match drain_available(stdout_pipe, &mut out) {
            Ok(drained) => stdout_eof |= drained == Drained::Eof,
            Err(error) => {
                terminate_and_cleanup(pid, &mut child, &mut stdout, &mut stderr);
                return Err(error);
            }
        }
    }
    if let Some(stderr_pipe) = stderr.as_mut() {
        match drain_available(stderr_pipe, &mut err) {
            Ok(drained) => stderr_eof |= drained == Drained::Eof,
            Err(error) => {
                terminate_and_cleanup(pid, &mut child, &mut stdout, &mut stderr);
                return Err(error);
            }
        }
    }
    stdout.take();
    stderr.take();
    let escaped_pipe = !stdout_eof || !stderr_eof;

    // Allocate the shared budget only now, when both streams' totals are known.
    let (out_limit, err_limit) =
        split_capture_budget(out.captured_len(), err.captured_len(), MAX_CAPTURE_BYTES_TOTAL);
    let mut combined = String::new();
    combined.push_str(&out.render_within(out_limit));
    let err_str = err.render_within(err_limit);
    if !err_str.trim().is_empty() {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(&err_str);
    }
    if escaped_pipe {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(
            "[output capture closed: a descendant outside the command process group still held \
             a pipe; that escaped process may continue running]",
        );
    }
    let mut result = middle_truncate(combined.trim_end(), MAX_OUTPUT_CHARS);

    if let Some(reason) = stopped {
        result = format!("[command stopped: {reason}]\n{result}");
    } else if timed_out {
        result = format!(
            "[command killed after {}s timeout]\n{result}",
            timeout.as_secs()
        );
    } else if let Some(status) = status {
        if !status.success() {
            let code = status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".into());
            result = format!("[exit {code}]\n{result}");
        }
    }
    if result.trim().is_empty() {
        result = "[no output]".to_string();
    }
    Ok(result)
}

/// Tear down without collecting, for the paths that are returning an error.
#[cfg(unix)]
fn terminate_and_cleanup(
    pid: u32,
    child: &mut std::process::Child,
    stdout: &mut Option<std::process::ChildStdout>,
    stderr: &mut Option<std::process::ChildStderr>,
) {
    // Close our pipe ends as part of the same failure boundary. Keeping them
    // alive after reap is how the former reader-thread design leaked workers.
    signal_process_group(pid, libc::SIGKILL);
    let _ = child.kill();
    stdout.take();
    stderr.take();
    let _ = child.wait();
}

/// Stop the command's process group and collect what it produced on the way out.
///
/// SIGTERM first, so a command that installs a handler can remove its lock file
/// or temporary directory; SIGKILL unconditionally afterwards, so a command
/// that ignores or blocks SIGTERM is still stopped and the grace period cannot
/// become a second, unbounded timeout.
///
/// The grace loop keeps draining rather than merely sleeping. That is not only
/// to capture the cleanup output: a child blocked writing into a pipe we have
/// stopped reading never reaches its own SIGTERM handler, so declining to drain
/// here would guarantee the SIGKILL this exists to avoid.
///
/// Returns whether each stream reached EOF. After the group is reaped every
/// writer is gone unless one deliberately left the group, so a stream that
/// still will not reach EOF is evidence of exactly that, and the caller reports
/// it rather than waiting on it.
#[cfg(unix)]
fn stop_and_collect(
    pid: u32,
    child: &mut std::process::Child,
    stdout: &mut Option<std::process::ChildStdout>,
    stderr: &mut Option<std::process::ChildStderr>,
    out: &mut BoundedBytes,
    err: &mut BoundedBytes,
) -> (bool, bool) {
    let mut stdout_eof = false;
    let mut stderr_eof = false;
    signal_process_group(pid, libc::SIGTERM);
    let grace_deadline = Instant::now() + TERMINATE_GRACE;
    loop {
        stdout_eof |= drain_pipe(stdout, out) == Drained::Eof;
        stderr_eof |= drain_pipe(stderr, err) == Drained::Eof;
        match child.try_wait() {
            Ok(None) if Instant::now() < grace_deadline => std::thread::sleep(TERMINATE_POLL),
            // Reaping the shell does not mean the group is empty, which is why
            // the SIGKILL below is not conditional on having waited it out.
            _ => break,
        }
    }
    signal_process_group(pid, libc::SIGKILL);
    let _ = child.kill();
    let _ = child.wait();

    for _ in 0..MAX_FINAL_DRAIN_POLLS {
        if stdout_eof && stderr_eof {
            break;
        }
        stdout_eof |= drain_pipe(stdout, out) == Drained::Eof;
        stderr_eof |= drain_pipe(stderr, err) == Drained::Eof;
    }
    stdout.take();
    stderr.take();
    (stdout_eof, stderr_eof)
}

/// Drain one optional pipe, treating a read error as "nothing more to read".
///
/// A pipe we have already closed reports `Eof`: it cannot be the escaped writer
/// the caller is looking for. A read error reports `WouldBlock`, which is the
/// conservative direction — it may produce the escaped-descendant notice on an
/// I/O error that was something else, and never suppresses a real one.
#[cfg(unix)]
fn drain_pipe(pipe: &mut Option<impl std::io::Read>, capture: &mut BoundedBytes) -> Drained {
    match pipe.as_mut() {
        Some(reader) => drain_available(reader, capture).unwrap_or(Drained::WouldBlock),
        None => Drained::Eof,
    }
}

/// Why one nonblocking drain pass stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Drained {
    /// Every write end is closed: the stream is complete.
    Eof,
    /// Nothing more is available right now.
    WouldBlock,
    /// The pass hit its fairness budget with data still pending.
    Budget,
}

#[cfg(unix)]
fn set_nonblocking(io: &impl std::os::fd::AsRawFd) -> Result<(), String> {
    let fd = io.as_raw_fd();
    // SAFETY: fcntl reads/updates flags on this owned pipe descriptor and does
    // not retain the pointer or touch Rust-managed memory.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(format!(
            "failed to make shell output nonblocking: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn drain_available(
    reader: &mut impl std::io::Read,
    capture: &mut BoundedBytes,
) -> Result<Drained, String> {
    let mut chunk = [0u8; 8192];
    let mut bytes_read = 0;
    for _ in 0..MAX_DRAIN_READS_PER_POLL {
        let remaining = MAX_DRAIN_BYTES_PER_POLL - bytes_read;
        if remaining == 0 {
            return Ok(Drained::Budget);
        }
        let read_limit = chunk.len().min(remaining);
        match reader.read(&mut chunk[..read_limit]) {
            Ok(0) => return Ok(Drained::Eof),
            Ok(n) => {
                capture.push(&chunk[..n]);
                bytes_read += n;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                return Ok(Drained::WouldBlock)
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(format!("failed to read shell output: {error}")),
        }
    }
    Ok(Drained::Budget)
}

struct BoundedBytes {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    head_cap: usize,
    tail_cap: usize,
    omitted: usize,
}

impl BoundedBytes {
    fn new(cap: usize) -> Self {
        Self {
            head: Vec::with_capacity(cap / 2),
            tail: VecDeque::with_capacity(cap - cap / 2),
            head_cap: cap / 2,
            tail_cap: cap - cap / 2,
            omitted: 0,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        for byte in bytes {
            if self.head.len() < self.head_cap {
                self.head.push(*byte);
            } else if self.tail.len() < self.tail_cap {
                self.tail.push_back(*byte);
            } else {
                self.tail.pop_front();
                self.tail.push_back(*byte);
                self.omitted = self.omitted.saturating_add(1);
            }
        }
    }

    /// Bytes actually retained, which is what the budget is allocated against.
    fn captured_len(&self) -> usize {
        self.head.len() + self.tail.len()
    }

    /// Render, cutting the middle again if this stream's share is smaller than
    /// what it retained.
    ///
    /// The second cut merges with the first rather than adding a marker. It can
    /// only ever remove a span that already contains the original head/tail
    /// boundary — `limit` never exceeds the buffer's own capacity, so the new
    /// ends land inside the old ones — which means one count and one notice
    /// still describe the whole omission truthfully.
    fn render_within(&self, limit: usize) -> String {
        let mut retained = self.head.clone();
        retained.extend(self.tail.iter());
        let (head, tail, omitted) = if retained.len() > limit {
            let head_len = limit / 2;
            let tail_start = retained.len() - (limit - head_len);
            (
                retained[..head_len].to_vec(),
                retained[tail_start..].to_vec(),
                self.omitted + (retained.len() - limit),
            )
        } else {
            let split = self.head.len();
            (
                retained[..split].to_vec(),
                retained[split..].to_vec(),
                self.omitted,
            )
        };
        let mut bytes = head;
        if omitted > 0 {
            bytes.extend_from_slice(format!("\n\n… [{omitted} bytes truncated] …\n\n").as_bytes());
        }
        bytes.extend_from_slice(&tail);
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[cfg(test)]
    fn render(&self) -> String {
        self.render_within(usize::MAX)
    }
}

/// Divide one capture budget between the streams.
///
/// A stream that stayed under half keeps everything it has and the other takes
/// the remainder, so the usual command — all stdout, empty stderr — spends the
/// whole budget on the stream that used it. Only when both overrun does this
/// fall back to an even split, which is the case the fixed halves were designed
/// for and the only one where they were right.
fn split_capture_budget(out_len: usize, err_len: usize, total: usize) -> (usize, usize) {
    if out_len + err_len <= total {
        return (out_len, err_len);
    }
    let half = total / 2;
    if out_len <= half {
        (out_len, total - out_len)
    } else if err_len <= half {
        (total - err_len, err_len)
    } else {
        (half, total - half)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::Workspace;
    use crate::{ExecutionToken, Tool};

    fn ctx() -> (tempfile::TempDir, ToolCtx) {
        let tmp = tempfile::tempdir().unwrap();
        let ws = Workspace::open(tmp.path()).unwrap();
        (tmp, ToolCtx::new(ws))
    }

    #[tokio::test]
    async fn runs_a_command_and_returns_stdout() {
        let (_t, ctx) = ctx();
        let out = Bash
            .run(&serde_json::json!({"command": "echo hello"}), &ctx)
            .await;
        assert!(!out.is_error);
        assert_eq!(out.content, "hello");
    }

    #[tokio::test]
    async fn runs_in_the_workspace_root() {
        let (_t, ctx) = ctx();
        ctx.workspace.write("marker.txt", "").unwrap();
        let out = Bash.run(&serde_json::json!({"command": "ls"}), &ctx).await;
        assert!(out.content.contains("marker.txt"));
    }

    #[tokio::test]
    async fn reports_a_nonzero_exit_code() {
        let (_t, ctx) = ctx();
        let out = Bash
            .run(&serde_json::json!({"command": "exit 3"}), &ctx)
            .await;
        assert!(out.content.starts_with("[exit 3]"));
    }

    #[tokio::test]
    async fn captures_stderr_too() {
        let (_t, ctx) = ctx();
        let out = Bash
            .run(&serde_json::json!({"command": "echo oops 1>&2"}), &ctx)
            .await;
        assert!(out.content.contains("oops"));
    }

    /// coda shipped the kill path untested. This verifies a runaway command is
    /// actually terminated at the deadline rather than hanging the agent.
    #[tokio::test]
    async fn kills_a_runaway_command_at_the_timeout() {
        let (_t, ctx) = ctx();
        let started = Instant::now();
        let out = Bash
            .run(
                &serde_json::json!({"command": "sleep 30", "timeout": 1}),
                &ctx,
            )
            .await;
        let elapsed = started.elapsed();
        assert!(
            out.content.contains("killed after 1s timeout"),
            "got: {}",
            out.content
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "took {elapsed:?}, should have been ~1s"
        );
    }

    /// **A command that backgrounds something must still be killed whole.**
    ///
    /// `Child::kill` signals only the shell. A grandchild it started remains a
    /// live side effect even though nonblocking capture can now close safely.
    /// The group boundary is what makes ordinary descendants die together.
    #[tokio::test]
    async fn a_backgrounded_grandchild_is_killed_with_the_shell_that_started_it() {
        let (_t, ctx) = ctx();
        let started = Instant::now();
        let out = Bash
            .run(
                &serde_json::json!({"command": "sleep 30 & wait", "timeout": 1}),
                &ctx,
            )
            .await;
        let elapsed = started.elapsed();

        assert!(
            out.content.contains("killed after 1s timeout"),
            "got: {}",
            out.content
        );
        assert!(
            elapsed < Duration::from_millis(2_500),
            "took {elapsed:?}: the shell was killed but its child kept the pipe open, so this \
             returned via the reader grace period rather than because the group died"
        );
    }

    /// Dropping the async wrapper used to leave spawn_blocking and its entire
    /// process tree alive. Stop may return only after Bash kills and reaps the
    /// group it owns.
    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_kills_and_reaps_the_whole_bash_process_group() {
        let (_t, ctx) = ctx();
        let ctx = std::sync::Arc::new(ctx);
        let (control, stopper) = ExecutionControl::for_turn(
            ExecutionToken::new(1, 1),
            Duration::from_secs(60),
        );
        let task_ctx = ctx.clone();
        let task = tokio::spawn(async move {
            Bash.run_controlled(
                &serde_json::json!({
                    "command": "sleep 30 & echo $! > child.pid; wait",
                    "timeout": 60
                }),
                &task_ctx,
                &control,
            )
            .await
        });

        let child_file = ctx.workspace.root().join("child.pid");
        let mut announced = None;
        for _ in 0..10_000 {
            if let Ok(text) = std::fs::read_to_string(&child_file) {
                if let Ok(pid) = text.trim().parse() {
                    announced = Some(pid);
                    break;
                }
            }
            tokio::task::yield_now().await;
        }
        let pid: libc::pid_t = announced.expect("the command announced its child");
        stopper.stop();
        let output = task.await.unwrap();
        assert!(output.is_error, "stopped Bash must not look successful");
        assert!(output.content.contains("stopped by user"), "{}", output.content);

        for _ in 0..10_000 {
            // SAFETY: signal 0 performs existence/permission checking only.
            if unsafe { libc::kill(pid, 0) } == -1 {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("background child {pid} survived cancellation");
    }

    /// Tokio cannot remove a spawn_blocking closure once its worker begins.
    /// A Stop while that closure was queued used to launch the command anyway
    /// because only the async caller had checked the control.
    #[cfg(unix)]
    #[test]
    fn a_worker_that_starts_after_stop_rechecks_before_spawning() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("must-not-exist");
        let (control, stopper) = ExecutionControl::for_turn(
            ExecutionToken::new(1, 1),
            Duration::from_secs(60),
        );
        stopper.stop();
        let result = run_with_control(
            "touch must-not-exist",
            tmp.path(),
            Duration::from_secs(10),
            &control,
        );
        assert!(result.unwrap_err().contains("stopped by user"));
        assert!(!marker.exists(), "the queued worker launched after Stop");
    }

    #[cfg(unix)]
    #[test]
    fn escaped_pipe_helper() {
        let Ok(path) = std::env::var("SMITHY_ESCAPED_PID") else {
            return;
        };
        // SAFETY: this process exists only as the test's deliberate escaped
        // descendant; becoming a new session leader is the behavior under test.
        assert_ne!(unsafe { libc::setsid() }, -1);
        std::fs::write(path, std::process::id().to_string()).unwrap();
        std::thread::sleep(Duration::from_secs(30));
    }

    /// A process that calls setsid deliberately leaves the group Smithy owns.
    /// It may survive, but retaining its inherited pipe must neither strand a
    /// reader thread nor delay the tool result.
    #[cfg(unix)]
    #[test]
    fn an_escaped_descendant_pipe_is_closed_and_reported_without_waiting() {
        let tmp = tempfile::tempdir().unwrap();
        let pid_file = tmp.path().join("escaped.pid");
        let exe = std::env::current_exe().unwrap();
        let quote = |text: &str| format!("'{}'", text.replace('\'', "'\\''"));
        let command = format!(
            "SMITHY_ESCAPED_PID={} {} --exact \
             tools::bash::tests::escaped_pipe_helper --nocapture & \
             while [ ! -s {} ]; do :; done",
            quote(&pid_file.display().to_string()),
            quote(&exe.display().to_string()),
            quote(&pid_file.display().to_string()),
        );
        let started = Instant::now();
        let result = run_with_timeout(&command, tmp.path(), Duration::from_secs(5)).unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "capture waited for the escaped writer: {result}"
        );
        assert!(result.contains("outside the command process group"), "{result}");
        assert!(result.contains("may continue running"), "{result}");

        let pid: libc::pid_t = std::fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // SAFETY: the pid was written by the helper created solely for this
        // test; SIGKILL prevents the test from leaving the documented escape.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }

    /// Also untested in coda: that large output truncates instead of blowing the
    /// context budget, and that the tail survives.
    #[tokio::test]
    async fn truncates_large_output_keeping_both_ends() {
        let (_t, ctx) = ctx();
        let out = Bash
            .run(
                &serde_json::json!({
                    "command": "for i in $(seq 1 20000); do echo LINE$i; done",
                    "timeout": 60
                }),
                &ctx,
            )
            .await;
        assert!(
            out.content.contains("truncated"),
            "output should be truncated"
        );
        assert!(out.content.contains("LINE1\n"), "head should survive");
        assert!(out.content.contains("LINE20000"), "tail should survive");
        assert!(out.content.chars().count() < MAX_OUTPUT_CHARS + 200);
    }

    /// A bounded read cycle still accumulates without bound if every cycle
    /// appends to a Vec. The retained storage itself must stop at the configured
    /// capacity while the logical byte count continues.
    #[test]
    fn logically_huge_output_keeps_fixed_storage_and_an_exact_omission_count() {
        let cap = 64;
        let bytes = vec![b'x'; 1_000_000];
        let mut capture = BoundedBytes::new(cap);
        capture.push(&bytes);

        assert_eq!(capture.head.len() + capture.tail.len(), cap);
        assert_eq!(capture.omitted, bytes.len() - cap);
        assert!(capture.head.capacity() <= capture.head_cap);
        assert!(capture.tail.capacity() <= capture.tail_cap);
        assert!(capture.render().contains("999936 bytes truncated"));
    }

    struct ContinuousReader {
        reads: usize,
    }

    impl std::io::Read for ContinuousReader {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            self.reads += 1;
            buffer.fill(b'x');
            Ok(buffer.len())
        }
    }

    /// A continuously readable pipe used to keep `drain_available` alive
    /// forever, so neither timeout nor Stop could be observed by its caller.
    #[test]
    fn one_output_poll_has_hard_read_and_byte_boundaries() {
        let mut reader = ContinuousReader { reads: 0 };
        let mut capture = BoundedBytes::new(32);
        assert_eq!(
            drain_available(&mut reader, &mut capture).unwrap(),
            Drained::Budget
        );
        assert_eq!(reader.reads, MAX_DRAIN_READS_PER_POLL);
        assert_eq!(
            capture.head.len() + capture.tail.len() + capture.omitted,
            MAX_DRAIN_BYTES_PER_POLL
        );
    }

    /// The unit boundary above matters only if the worker returns to its
    /// deadline check while a real child keeps both pipe and CPU busy.
    #[tokio::test]
    async fn a_continuous_writer_cannot_starve_the_timeout() {
        let (_t, ctx) = ctx();
        let started = Instant::now();
        let out = Bash
            .run(
                &serde_json::json!({
                    "command": "while :; do printf x; done",
                    "timeout": 1
                }),
                &ctx,
            )
            .await;
        assert!(out.content.contains("killed after 1s timeout"), "{}", out.content);
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    /// The old final combined truncation could preserve stdout's beginning and
    /// stderr's end while silently deleting the other two diagnostic edges.
    #[tokio::test]
    async fn both_streams_preserve_their_own_head_and_tail_tags() {
        let (_t, ctx) = ctx();
        let out = Bash
            .run(
                &serde_json::json!({
                    "command": "\
                        printf OUT_HEAD; \
                        i=0; while [ $i -lt 30000 ]; do printf o; i=$((i+1)); done; \
                        printf OUT_TAIL; \
                        printf ERR_HEAD >&2; \
                        i=0; while [ $i -lt 30000 ]; do printf e >&2; i=$((i+1)); done; \
                        printf ERR_TAIL >&2",
                    "timeout": 60
                }),
                &ctx,
            )
            .await;

        for tag in ["OUT_HEAD", "OUT_TAIL", "ERR_HEAD", "ERR_TAIL"] {
            assert!(out.content.contains(tag), "missing {tag}: {}", out.content);
        }
        assert_eq!(out.content.matches("bytes truncated").count(), 2);
    }

    /// SIGKILL alone stops the command and takes its cleanup with it: the lock
    /// file, the temporary directory, and the last thing it was trying to say.
    /// The stop path signals TERM first and keeps draining, so a command that
    /// handles it both runs its handler and is heard afterwards.
    #[cfg(unix)]
    #[test]
    fn a_stopped_command_may_clean_up_and_is_still_heard() {
        let tmp = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let result = run_with_timeout(
            "trap 'printf CLEANED_UP; exit 0' TERM; sleep 30 & wait",
            tmp.path(),
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(result.contains("CLEANED_UP"), "{result}");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "the grace period became a second timeout"
        );
    }

    /// A command that ignores SIGTERM must not turn the grace period into an
    /// unbounded second timeout.
    #[cfg(unix)]
    #[test]
    fn a_command_that_ignores_the_signal_is_still_killed_promptly() {
        let tmp = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let result = run_with_timeout(
            "trap '' TERM; printf IGNORING; sleep 30 & wait",
            tmp.path(),
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(result.contains("killed after 1s timeout"), "{result}");
        assert!(result.contains("IGNORING"), "{result}");
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    /// Fixed halves spent stderr's reservation on nothing for the ordinary
    /// command, which writes to stdout alone. It should retain more than the
    /// half it used to be allowed.
    #[tokio::test]
    async fn a_single_stream_command_gets_the_whole_capture_budget() {
        let (_t, ctx) = ctx();
        let out = Bash
            .run(
                &serde_json::json!({
                    "command": "for i in $(seq 1 20000); do echo LINE$i; done",
                    "timeout": 60
                }),
                &ctx,
            )
            .await;
        assert!(
            out.content.len() > MAX_CAPTURE_BYTES_TOTAL / 2 + 5_000,
            "stdout kept only {} bytes of the {MAX_CAPTURE_BYTES_TOTAL} budget",
            out.content.len()
        );
        assert!(out.content.contains("LINE1\n"), "head should survive");
        assert!(out.content.contains("LINE20000"), "tail should survive");
    }

    #[test]
    fn an_unused_share_goes_to_the_stream_that_needs_it() {
        // Both fit: neither is cut.
        assert_eq!(split_capture_budget(10, 20, 100), (10, 20));
        // Silent stderr hands its whole share over.
        assert_eq!(split_capture_budget(500, 0, 100), (100, 0));
        assert_eq!(split_capture_budget(500, 30, 100), (70, 30));
        // Both overrun: the even split is right only here.
        assert_eq!(split_capture_budget(500, 500, 100), (50, 50));
    }

    /// Two cuts through the same buffer describe one omission, not two, and the
    /// count has to cover both or the notice understates what was lost.
    #[test]
    fn a_second_cut_merges_with_the_first_and_keeps_the_count_exact() {
        let mut capture = BoundedBytes::new(64);
        capture.push(&vec![b'x'; 1_000]);
        let rendered = capture.render_within(16);
        assert_eq!(rendered.matches("bytes truncated").count(), 1);
        assert!(rendered.contains("[984 bytes truncated]"), "{rendered}");
        let kept: usize = rendered.matches('x').count();
        assert_eq!(kept, 16);
    }

    /// Arbitrary subprocess bytes are not necessarily UTF-8. Rendering must
    /// replace malformed sequences without panicking or losing the valid ends.
    #[test]
    fn invalid_utf8_is_lossy_but_keeps_the_valid_edges() {
        let mut capture = BoundedBytes::new(16);
        capture.push(b"HEAD\xff\xfeTAIL");
        let rendered = capture.render();
        assert!(rendered.starts_with("HEAD"), "{rendered:?}");
        assert!(rendered.ends_with("TAIL"), "{rendered:?}");
        assert!(rendered.contains('\u{fffd}'), "{rendered:?}");
    }

    #[tokio::test]
    async fn a_command_producing_nothing_says_so() {
        let (_t, ctx) = ctx();
        let out = Bash
            .run(&serde_json::json!({"command": "true"}), &ctx)
            .await;
        assert_eq!(out.content, "[no output]");
    }

    #[tokio::test]
    async fn the_guardrail_blocks_before_spawning() {
        let (_t, ctx) = ctx();
        let out = Bash
            .run(&serde_json::json!({"command": "sudo rm -rf /"}), &ctx)
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("guardrail"));
    }

    #[tokio::test]
    async fn stdin_is_closed_so_interactive_commands_cannot_hang() {
        let (_t, ctx) = ctx();
        let out = Bash
            .run(&serde_json::json!({"command": "cat", "timeout": 5}), &ctx)
            .await;
        // With stdin at /dev/null, `cat` reads EOF immediately instead of hanging.
        assert!(
            !out.content.contains("killed after"),
            "got: {}",
            out.content
        );
    }
}
