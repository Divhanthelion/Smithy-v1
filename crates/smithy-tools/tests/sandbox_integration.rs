//! The tools, the sandbox and the dispatcher, together.
//!
//! Every tool has unit tests and every sandbox check has unit tests. They all
//! pass. What nobody tested was the three of them *combined* — and this session
//! established, repeatedly and expensively, that this project's bugs live in the
//! wiring between layers rather than inside them.
//!
//! So these drive real tools through [`Registry::execute`], which is the only
//! dispatch path, against a real temporary workspace on a real filesystem.
//!
//! The escape vectors and their priority order come from
//! `docs/research/07-sandbox-adversarial.md` — vectors 1 (absolute path ingress),
//! 2 (relative `..` traversal) and 3 (external symlink escapes), which it rates
//! extreme-to-high risk for minimal effort. Vector 4 is `/proc`, which does not
//! exist on macOS, and vectors 6 and 7 (TOCTOU races, APFS case aliasing) it rates
//! high-effort and lower-risk; they are noted in HANDOFF rather than attempted
//! here.

use serde_json::{json, Value};
use smithy_tools::{Registry, ToolCall, ToolCtx, Workspace};

/// A workspace with a small tree in it, plus a secret *outside* it that every
/// escape attempt is trying to reach.
struct Sandbox {
    _root: tempfile::TempDir,
    ctx: ToolCtx,
    secret: std::path::PathBuf,
}

fn sandbox() -> Sandbox {
    let root = tempfile::tempdir().expect("tempdir");

    // The secret lives beside the workspace, not inside it.
    let outside = root.path().join("outside");
    std::fs::create_dir_all(&outside).expect("outside dir");
    let secret = outside.join("secret.txt");
    std::fs::write(&secret, "TOP SECRET\n").expect("secret");

    let work = root.path().join("workspace");
    std::fs::create_dir_all(work.join("src")).expect("src");
    std::fs::write(work.join("src/lib.rs"), "fn a() {}\nfn b() {}\n").expect("lib.rs");
    std::fs::write(work.join("Cargo.toml"), "[package]\nname = \"x\"\n").expect("manifest");

    let workspace = Workspace::open(&work).expect("workspace opens");
    Sandbox {
        _root: root,
        ctx: ToolCtx::new(workspace),
        secret,
    }
}

async fn run(
    registry: &Registry,
    ctx: &ToolCtx,
    name: &str,
    args: Value,
) -> smithy_tools::ToolResult {
    let call = ToolCall::new("call-1", name, args.to_string());
    registry.execute(&call, ctx).await
}

/// Paths that must never resolve to anything outside the workspace.
///
/// `..` traversal and absolute ingress, per research 07 vectors 1 and 2. The
/// interior form (`src/../../`) matters separately from the leading form, because
/// a check that only inspects the first component passes one and fails the other.
fn hostile_paths() -> Vec<&'static str> {
    vec![
        "../outside/secret.txt",
        "../../outside/secret.txt",
        "../../../../../../etc/passwd",
        "src/../../outside/secret.txt",
        "/etc/passwd",
        "/tmp",
        "src/../../../outside/secret.txt",
    ]
}

/// Arguments that satisfy each path-taking tool's schema, with `path` filled in.
///
/// Returns `None` for tools that take no `path`, which are exercised separately.
fn args_for(tool: &str, path: &str) -> Option<Value> {
    Some(match tool {
        "read" => json!({ "path": path }),
        "write" => json!({ "path": path, "content": "pwned\n" }),
        "edit" => json!({ "path": path, "old_string": "TOP", "new_string": "pwned" }),
        "ls" => json!({ "path": path }),
        "grep" => json!({ "pattern": "SECRET", "path": path }),
        "glob" => json!({ "pattern": "**/*", "path": path }),
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Escape attempts, through the dispatcher
// ---------------------------------------------------------------------------

/// The structural test: **every** tool in the core registry that accepts a
/// `path` must refuse to leave the workspace.
///
/// Driven from `definitions()` rather than a hand-written list, so a tool added
/// later is covered the moment it declares a `path` parameter. A hardcoded list
/// would pass forever while quietly not testing the new thing.
#[tokio::test]
async fn no_path_taking_tool_can_reach_outside_the_workspace() {
    let s = sandbox();
    let registry = Registry::core();

    let path_taking: Vec<String> = registry
        .definitions()
        .into_iter()
        .filter(|d| d.parameters.iter().any(|p| p.name == "path"))
        .map(|d| d.name)
        .collect();

    assert!(
        path_taking.len() >= 4,
        "expected several path-taking tools, found {path_taking:?}"
    );

    for tool in &path_taking {
        for path in hostile_paths() {
            let Some(args) = args_for(tool, path) else {
                panic!("`{tool}` declares a `path` parameter but this test has no arguments for it — add them, do not skip it");
            };
            let result = run(&registry, &s.ctx, tool, args).await;

            assert!(
                result.is_error,
                "`{tool}` accepted `{path}`, which resolves outside the workspace"
            );
            assert!(
                !result.content.contains("TOP SECRET"),
                "`{tool}` leaked the contents of a file outside the workspace via `{path}`"
            );
        }
    }

    assert_eq!(
        std::fs::read_to_string(&s.secret).expect("secret still readable"),
        "TOP SECRET\n",
        "a write escaped the workspace and modified a file outside it"
    );
}

/// Research 07 vector 3, end to end: a symlink *inside* the workspace pointing
/// outside it. The path never leaves the root textually, so a purely lexical
/// check passes it — this is why the sandbox is a capability and not a string
/// comparison.
#[cfg(unix)]
#[tokio::test]
async fn a_symlink_pointing_out_of_the_workspace_is_refused() {
    let s = sandbox();
    let registry = Registry::core();
    let root = s.ctx.workspace.root().to_path_buf();

    std::os::unix::fs::symlink(&s.secret, root.join("escape.txt")).expect("symlink");
    std::os::unix::fs::symlink(s.secret.parent().unwrap(), root.join("escape_dir"))
        .expect("dir symlink");

    for path in ["escape.txt", "escape_dir/secret.txt", "escape_dir"] {
        for tool in ["read", "ls", "grep"] {
            let result = run(&registry, &s.ctx, tool, args_for(tool, path).unwrap()).await;
            assert!(
                !result.content.contains("TOP SECRET"),
                "`{tool}` followed a symlink out of the workspace via `{path}`"
            );
        }
    }
}

/// A symlink is not suspicious in itself — only one that leaves. Refusing all of
/// them would break every `node_modules` and `target` layout in existence, so the
/// negative control matters as much as the positive.
#[cfg(unix)]
#[tokio::test]
async fn a_symlink_staying_inside_the_workspace_still_works() {
    let s = sandbox();
    let registry = Registry::core();
    let root = s.ctx.workspace.root().to_path_buf();

    // Relative target. An *absolute* target is refused even when it points
    // inside — see `an_absolute_symlink_target_is_refused_even_pointing_inward`.
    std::os::unix::fs::symlink("src/lib.rs", root.join("linked.rs")).expect("symlink");

    let result = run(&registry, &s.ctx, "read", json!({ "path": "linked.rs" })).await;
    assert!(
        !result.is_error,
        "a relative symlink that stays inside the workspace is legitimate: {}",
        result.content
    );
    assert!(result.content.contains("fn a()"), "{}", result.content);
}

/// cap-std refuses an absolute symlink target even when it points back inside the
/// workspace, because resolving it would mean leaving the `Dir` capability and
/// re-entering from the filesystem root — which is exactly the ambient authority
/// the capability exists to remove.
///
/// Recorded as a test rather than left to be rediscovered: it is correct
/// behaviour, it is surprising, and a project that uses absolute symlinks will
/// have unreadable files with no obvious reason why.
#[cfg(unix)]
#[tokio::test]
async fn an_absolute_symlink_target_is_refused_even_pointing_inward() {
    let s = sandbox();
    let registry = Registry::core();
    let root = s.ctx.workspace.root().to_path_buf();

    std::os::unix::fs::symlink(root.join("src/lib.rs"), root.join("abs_link.rs")).expect("symlink");

    let result = run(&registry, &s.ctx, "read", json!({ "path": "abs_link.rs" })).await;
    assert!(
        result.is_error,
        "an absolute symlink target leaves the capability, even pointing inward"
    );
}

/// Interior `..` that never leaves is legitimate and must keep working, or the
/// confinement check is just a ban on a character sequence.
#[tokio::test]
async fn interior_dot_dot_that_stays_inside_is_allowed() {
    let s = sandbox();
    let registry = Registry::core();

    let result = run(
        &registry,
        &s.ctx,
        "read",
        json!({ "path": "src/../Cargo.toml" }),
    )
    .await;

    assert!(!result.is_error, "{}", result.content);
    assert!(result.content.contains("[package]"));
}

// ---------------------------------------------------------------------------
// The round trip the agent actually performs
// ---------------------------------------------------------------------------

/// The sequence every edit the agent makes depends on: read a file, then edit it
/// using text taken from what was read. If `read` reformats, trims, or adds line
/// numbers to its output, the `old_string` the model sends back will not match,
/// and the failure looks like the model hallucinating rather than like a tool bug.
#[tokio::test]
async fn text_taken_from_read_can_be_used_as_an_edit_target() {
    let s = sandbox();
    let registry = Registry::core();

    let read = run(&registry, &s.ctx, "read", json!({ "path": "src/lib.rs" })).await;
    assert!(!read.is_error, "{}", read.content);

    // Take a line straight out of the tool's own output, exactly as a model would.
    let line = read
        .content
        .lines()
        .find(|l| l.contains("fn b()"))
        .expect("read output contains the line")
        .trim()
        .to_string();

    let edited = run(
        &registry,
        &s.ctx,
        "edit",
        json!({ "path": "src/lib.rs", "old_string": line, "new_string": "fn b() { todo!() }" }),
    )
    .await;

    assert!(
        !edited.is_error,
        "an edit using text read back from `read` must apply: {}",
        edited.content
    );
    let on_disk = s
        .ctx
        .workspace
        .read_to_string("src/lib.rs")
        .expect("read back");
    assert!(on_disk.contains("fn b() { todo!() }"), "{on_disk}");
    assert!(
        on_disk.contains("fn a() {}"),
        "the rest of the file survived"
    );
}

/// An edit whose target is not present must fail, and say so, rather than
/// writing something wrong or reporting success.
#[tokio::test]
async fn an_edit_that_matches_nothing_fails_and_leaves_the_file_alone() {
    let s = sandbox();
    let registry = Registry::core();
    let before = s.ctx.workspace.read_to_string("src/lib.rs").unwrap();

    let result = run(
        &registry,
        &s.ctx,
        "edit",
        json!({ "path": "src/lib.rs", "old_string": "fn nowhere() {}", "new_string": "x" }),
    )
    .await;

    assert!(result.is_error, "{}", result.content);
    assert_eq!(
        s.ctx.workspace.read_to_string("src/lib.rs").unwrap(),
        before,
        "a failed edit must not modify the file"
    );
}

// ---------------------------------------------------------------------------
// Dispatcher behaviour
// ---------------------------------------------------------------------------

/// Every result carries the id of the call that produced it. Parallel calls to
/// one tool are otherwise indistinguishable, in the loop and in the UI.
#[tokio::test]
async fn a_result_carries_the_id_of_its_call() {
    let s = sandbox();
    let registry = Registry::core();

    let call = ToolCall::new(
        "call-42",
        "read",
        json!({ "path": "src/lib.rs" }).to_string(),
    );
    let result = registry.execute(&call, &s.ctx).await;

    assert_eq!(result.tool_call_id, "call-42");
    assert_eq!(result.name, "read");
}

/// Malformed input from a model is routine, not exceptional. It has to come back
/// as an error result the loop can feed onward, never as a panic.
#[tokio::test]
async fn malformed_calls_are_reported_rather_than_panicking() {
    let s = sandbox();
    let registry = Registry::core();

    let cases = [
        ("read", "{not json at all"),
        ("read", "{}"),
        ("read", "null"),
        ("read", r#"{"path": 42}"#),
        ("no_such_tool", "{}"),
    ];

    for (name, raw) in cases {
        let call = ToolCall::new("c", name, raw);
        let result = registry.execute(&call, &s.ctx).await;
        assert!(
            result.is_error,
            "`{name}` with `{raw}` should be an error result"
        );
        assert!(
            !result.content.is_empty(),
            "an error result must say something the model can act on"
        );
    }
}
