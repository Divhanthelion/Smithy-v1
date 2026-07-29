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

use std::path::{Component, Path, PathBuf};

use cap_std::ambient_authority;
use cap_std::fs::Dir;

/// A workspace root, held as a capability.
///
/// Deliberately not `Clone`: a second handle to the same root is never needed,
/// and a type that can be copied around invites one being kept past a project
/// switch — which is the mistake this file's whole `absolute_real` note is
/// about. (The doc here used to describe cloning behaviour for an impl that
/// does not exist.)
pub struct Workspace {
    /// Canonicalized root, kept for display and for the lexical pre-check.
    root: PathBuf,
    /// The capability. Every filesystem operation goes through this.
    dir: Dir,
}

impl std::fmt::Debug for Workspace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Workspace")
            .field("root", &self.root)
            .finish()
    }
}

impl Workspace {
    /// Open `root` as a confined workspace.
    pub fn open(root: impl AsRef<Path>) -> Result<Workspace, String> {
        let root = root.as_ref();
        let canonical = root.canonicalize().map_err(|e| {
            format!(
                "workspace {} does not exist or is unreadable: {e}",
                root.display()
            )
        })?;
        if !canonical.is_dir() {
            return Err(format!(
                "workspace {} is not a directory",
                canonical.display()
            ));
        }
        let dir = Dir::open_ambient_dir(&canonical, ambient_authority())
            .map_err(|e| format!("cannot open workspace {}: {e}", canonical.display()))?;
        Ok(Workspace {
            root: canonical,
            dir,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Normalize a model-supplied path into a workspace-relative one.
    ///
    /// Accepts either a relative path or an absolute path that already lies
    /// inside the root (models frequently echo back absolute paths they saw in
    /// tool output). Rejects anything that escapes, with a message the model can
    /// act on. `cap-std` will reject an escape too — this just gets there first
    /// with a better explanation.
    pub fn relative(&self, path: &str) -> Result<PathBuf, String> {
        let raw = Path::new(path);

        let stripped: PathBuf = if raw.is_absolute() {
            // Canonicalize lexically before comparing, so `/root/./a` matches.
            let normalized = lexical_normalize(raw)?;
            normalized
                .strip_prefix(&self.root)
                .map_err(|_| {
                    format!(
                        "path `{path}` is outside the workspace root {}",
                        self.root.display()
                    )
                })?
                .to_path_buf()
        } else {
            let joined = self.root.join(raw);
            let normalized = lexical_normalize(&joined)?;
            normalized
                .strip_prefix(&self.root)
                .map_err(|_| format!("path `{path}` escapes the workspace root"))?
                .to_path_buf()
        };

        if stripped.as_os_str().is_empty() {
            return Ok(PathBuf::from("."));
        }
        Ok(stripped)
    }

    /// Display form for messages back to the model: always workspace-relative.
    pub fn display_path(&self, path: &str) -> String {
        self.relative(path)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| path.to_string())
    }

    pub fn read_to_string(&self, path: &str) -> Result<String, String> {
        let rel = self.relative(path)?;
        self.dir
            .read_to_string(&rel)
            .map_err(|e| format!("cannot read `{}`: {e}", rel.display()))
    }

    pub fn write(&self, path: &str, contents: &str) -> Result<(), String> {
        let rel = self.relative(path)?;
        if let Some(parent) = rel.parent() {
            if !parent.as_os_str().is_empty() {
                self.dir
                    .create_dir_all(parent)
                    .map_err(|e| format!("cannot create parent of `{}`: {e}", rel.display()))?;
            }
        }
        self.dir
            .write(&rel, contents.as_bytes())
            .map_err(|e| format!("cannot write `{}`: {e}", rel.display()))
    }

    pub fn exists(&self, path: &str) -> bool {
        match self.relative(path) {
            Ok(rel) => self.dir.exists(&rel),
            Err(_) => false,
        }
    }

    pub fn is_dir(&self, path: &str) -> bool {
        match self.relative(path) {
            Ok(rel) => self.dir.metadata(&rel).map(|m| m.is_dir()).unwrap_or(false),
            Err(_) => false,
        }
    }

    /// List a directory. Returns `(name, is_dir)` pairs, sorted directories-first
    /// then alphabetically, so output is deterministic across runs.
    pub fn read_dir(&self, path: &str) -> Result<Vec<(String, bool)>, String> {
        let rel = self.relative(path)?;
        let entries = self
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
        Ok(self.root.join(self.relative(path)?))
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
        let candidate = self.absolute(path)?;
        let real = candidate
            .canonicalize()
            .map_err(|e| format!("cannot resolve `{path}`: {e}"))?;
        let real_root = self
            .root
            .canonicalize()
            .map_err(|e| format!("cannot resolve the workspace root: {e}"))?;
        if !real.starts_with(&real_root) {
            return Err(format!("`{path}` resolves outside the workspace"));
        }
        Ok(real)
    }
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
}
