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

use std::io::{ErrorKind, Write as _};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
#[cfg(unix)]
use cap_std::fs::{MetadataExt as _, PermissionsExt as _};

/// The exact state a caller observed before proposing a write.
///
/// Missing and present-but-empty are different variants because treating both
/// as `""` lets a file created after preview be overwritten as if nothing had
/// changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileSnapshot {
    Missing,
    Present(FileBase),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileBase {
    pub content: String,
    /// Unix exposes stable object identity through metadata. Other platforms
    /// fall back to exact content/existence because cap-std has no portable file
    /// id; that weaker fallback is explicit rather than claimed as a CAS.
    pub identity: Option<FileIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    links: u64,
    #[cfg(unix)]
    owner: u32,
    #[cfg(unix)]
    group: u32,
}

#[derive(Debug)]
struct CreatedDirectory {
    path: PathBuf,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl CreatedDirectory {
    fn new(path: PathBuf, metadata: &cap_std::fs::Metadata) -> Self {
        Self {
            path,
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
        }
    }

    fn is_same(&self, metadata: &cap_std::fs::Metadata) -> bool {
        #[cfg(unix)]
        {
            metadata.dev() == self.device && metadata.ino() == self.inode
        }
        #[cfg(not(unix))]
        {
            // Portable metadata has no stable file id. `remove_dir` still
            // refuses populated directories, but replacement by another empty
            // directory in this narrow interval cannot be distinguished.
            let _ = metadata;
            true
        }
    }
}

impl FileSnapshot {
    pub fn content(&self) -> Option<&str> {
        match self {
            Self::Missing => None,
            Self::Present(base) => Some(&base.content),
        }
    }
}

/// Failure points the publication tests need to reach and no caller can.
///
/// The interesting states in `compare_and_write_inner` are the ones between
/// staging and parent fsync, and there is no way to provoke them from outside
/// without a real disk fault. Injection is the alternative, but a pair of bare
/// `bool` parameters puts two permanently false arguments in the production
/// signature and needs `#[allow(unused_variables)]` to compile. The fields here
/// are `#[cfg(test)]`, so outside tests this is a zero-sized value whose
/// accessors are `const false` and whose branches are gone from the binary.
#[derive(Clone, Copy, Default)]
struct WriteFaults {
    #[cfg(test)]
    before_rename: bool,
    #[cfg(test)]
    parent_sync: bool,
}

impl WriteFaults {
    fn before_rename(self) -> bool {
        #[cfg(test)]
        {
            self.before_rename
        }
        #[cfg(not(test))]
        {
            false
        }
    }

    fn parent_sync(self) -> bool {
        #[cfg(test)]
        {
            self.parent_sync
        }
        #[cfg(not(test))]
        {
            false
        }
    }
}

/// Which side of atomic rename an error occurred on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteFailure {
    BeforePublication(String),
    PublishedButDurabilityUncertain(String),
}

impl WriteFailure {
    pub fn published(&self) -> bool {
        matches!(self, Self::PublishedButDurabilityUncertain(_))
    }
}

impl std::fmt::Display for WriteFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BeforePublication(message) => f.write_str(message),
            Self::PublishedButDurabilityUncertain(message) => write!(
                f,
                "{message}. The rename reached disk, but parent-directory durability could not be \
                 confirmed; the new content may already be published."
            ),
        }
    }
}

/// The directory object a review was computed against.
///
/// A canonical path alone is insufficient: deleting and recreating a project
/// at the same spelling gives a different directory with the same path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceIdentity {
    canonical_root: PathBuf,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl WorkspaceIdentity {
    pub fn canonical_root(&self) -> &Path {
        &self.canonical_root
    }
}

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

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
    /// Path plus directory identity, captured when the capability was opened.
    identity: WorkspaceIdentity,
}

/// A read-only duplicate of the root capability for one blocking worker.
///
/// `Workspace` itself stays non-Clone so long-lived stale handles are hard to
/// create. Grep needs one explicitly scoped duplicate because ignore traversal
/// runs on a blocking worker and every discovered path must be reopened through
/// cap-std at the moment its contents are read.
pub(crate) struct WorkspaceReader {
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
        let metadata = std::fs::metadata(&canonical)
            .map_err(|e| format!("cannot identify workspace {}: {e}", canonical.display()))?;
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        let identity = WorkspaceIdentity {
            canonical_root: canonical.clone(),
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
        };
        Ok(Workspace {
            root: canonical,
            dir,
            identity,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn identity(&self) -> &WorkspaceIdentity {
        &self.identity
    }

    /// Confirm that the path used to open this capability still names the same
    /// directory. Reviews retain this identity instead of resolving a live UI
    /// project when the user eventually accepts them.
    pub fn verify_identity(&self, expected: &WorkspaceIdentity) -> Result<(), String> {
        if &self.identity != expected {
            return if self.identity.canonical_root == expected.canonical_root {
                Err(format!(
                    "workspace root `{}` was replaced since preview",
                    expected.canonical_root.display()
                ))
            } else {
                Err(format!(
                    "workspace root changed since preview (expected `{}`)",
                    expected.canonical_root.display()
                ))
            };
        }
        let reopened = Workspace::open(&self.root)?;
        if reopened.identity != *expected {
            return Err(format!(
                "workspace root `{}` was replaced since preview",
                expected.canonical_root.display()
            ));
        }
        Ok(())
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

    pub(crate) fn try_reader(&self) -> Result<WorkspaceReader, String> {
        self.dir
            .try_clone()
            .map(|dir| WorkspaceReader { dir })
            .map_err(|error| format!("cannot duplicate workspace read capability: {error}"))
    }

    /// Read the exact state used by compare-and-write.
    pub fn snapshot(&self, path: &str) -> Result<FileSnapshot, String> {
        let rel = self.relative(path)?;
        self.reject_symlink_components(&rel)?;
        match self.dir.symlink_metadata(&rel) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(format!(
                        "refusing to write `{}` because its final path component is a symlink",
                        rel.display()
                    ));
                }
                if metadata.is_dir() {
                    return Err(format!("`{}` is a directory, not a file", rel.display()));
                }
                #[cfg(unix)]
                let identity = {
                    if metadata.nlink() != 1 {
                        return Err(format!(
                            "refusing to write `{}` because it has {} hard links; atomic \
                             replacement would update only one name",
                            rel.display(),
                            metadata.nlink()
                        ));
                    }
                    Some(FileIdentity {
                        device: metadata.dev(),
                        inode: metadata.ino(),
                        links: metadata.nlink(),
                        owner: metadata.uid(),
                        group: metadata.gid(),
                    })
                };
                #[cfg(target_os = "macos")]
                {
                    let file = self
                        .dir
                        .open(&rel)
                        .map_err(|error| format!("cannot inspect `{}` metadata: {error}", rel.display()))?;
                    let attributes = file_extended_attributes(&file)?;
                    let unsupported: Vec<_> = attributes
                        .iter()
                        .filter(|name| name.as_str() != "com.apple.provenance")
                        .collect();
                    if !unsupported.is_empty() {
                        return Err(format!(
                            "refusing to replace `{}` because it has extended attributes that \
                             atomic inode replacement cannot preserve safely: {}",
                            rel.display(),
                            unsupported
                                .iter()
                                .map(|name| name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ));
                    }
                }
                #[cfg(not(unix))]
                let identity = None;
                self.dir
                    .read_to_string(&rel)
                    .map(|content| FileSnapshot::Present(FileBase { content, identity }))
                    .map_err(|e| format!("cannot read `{}`: {e}", rel.display()))
            }
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(FileSnapshot::Missing),
            Err(error) => Err(format!(
                "cannot inspect `{}` before writing: {error}",
                rel.display()
            )),
        }
    }

    /// Publish content with a same-directory temporary file and atomic rename.
    ///
    /// This is a compare immediately followed by rename, not a cross-process
    /// CAS. Another process can still replace the destination in that narrow
    /// final interval. The temp file prevents torn/truncated content, and the
    /// second comparison fails safely for every change observed before rename.
    ///
    /// Mode and ordinary owner/group identity are preserved or refused on Unix.
    /// macOS extended attributes are detected through the already-open
    /// capability file and refused, except the OS-managed
    /// `com.apple.provenance` marker that macOS also places on replacement
    /// inodes. POSIX ACL inspection has no cap-std or portable standard-library
    /// surface, so ACLs remain the exact undetectable class; callers must not
    /// describe this as full metadata preservation.
    pub fn write(&self, path: &str, contents: &str) -> Result<(), String> {
        let expected = self.snapshot(path)?;
        self.compare_and_write(path, &expected, contents)
            .map_err(|error| error.to_string())
    }

    /// Write only if the destination still has exactly the observed state.
    pub fn compare_and_write(
        &self,
        path: &str,
        expected: &FileSnapshot,
        contents: &str,
    ) -> Result<(), WriteFailure> {
        self.compare_and_write_authorized(path, expected, contents, || Ok(()))
    }

    pub fn compare_and_write_authorized(
        &self,
        path: &str,
        expected: &FileSnapshot,
        contents: &str,
        mut authorize: impl FnMut() -> Result<(), String>,
    ) -> Result<(), WriteFailure> {
        self.compare_and_write_inner(
            path,
            expected,
            contents,
            &mut authorize,
            WriteFaults::default(),
        )
    }

    fn compare_and_write_inner(
        &self,
        path: &str,
        expected: &FileSnapshot,
        contents: &str,
        authorize: &mut impl FnMut() -> Result<(), String>,
        faults: WriteFaults,
    ) -> Result<(), WriteFailure> {
        let pre = |message: String| WriteFailure::BeforePublication(message);
        let rel = self.relative(path).map_err(pre)?;
        let parent = rel.parent().unwrap_or_else(|| Path::new(""));
        let leaf = rel
            .file_name()
            .ok_or_else(|| pre(format!("cannot write `{}`: no file name", rel.display())))?;
        let mut temp = parent.to_path_buf();
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        temp.push(format!(
            ".{}.smithy-tmp-{}-{sequence}",
            leaf.to_string_lossy(),
            std::process::id()
        ));

        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        let mut temp_created = false;
        let mut created_parents = Vec::new();
        let result = (|| -> Result<(), WriteFailure> {
            // The first base check must precede every persistent mutation. In
            // particular, a stale expected Missing must not create its parent.
            let observed = self.snapshot(path).map_err(pre)?;
            if &observed != expected {
                return Err(pre(compare_conflict(&rel, expected, &observed)));
            }

            if !parent.as_os_str().is_empty()
                && self.parent_needs_creation(parent).map_err(pre)?
            {
                // This lease is deliberately the same callback used below at
                // rename. A retired turn cannot authorize mkdir under one
                // identity and publication under another.
                authorize().map_err(pre)?;
                if let Err(error) = self.create_missing_parents(parent, &mut created_parents) {
                    return Err(pre(format!(
                        "cannot create parent of `{}`: {error}",
                        rel.display()
                    )));
                }
            }

            let mut file = self
                .dir
                .open_with(&temp, &options)
                .map_err(|e| pre(format!("cannot create temporary file for `{}`: {e}", rel.display())))?;
            temp_created = true;

            // Set destination mode before the first content byte enters the
            // temporary inode. A new file starts owner-only; an existing file
            // keeps its mode.
            if let FileSnapshot::Present(_) = expected {
                let permissions = self
                    .dir
                    .metadata(&rel)
                    .map_err(|e| pre(format!("cannot read permissions for `{}`: {e}", rel.display())))?
                    .permissions();
                self.dir
                    .set_permissions(&temp, permissions)
                    .map_err(|e| pre(format!(
                        "cannot preserve permissions for `{}`: {e}",
                        rel.display()
                    )))?;
            }
            #[cfg(unix)]
            if matches!(expected, FileSnapshot::Missing) {
                self.dir
                    .set_permissions(&temp, cap_std::fs::Permissions::from_mode(0o600))
                    .map_err(|e| pre(format!(
                        "cannot set safe permissions for `{}`: {e}",
                        rel.display()
                    )))?;
            }

            #[cfg(unix)]
            if let FileSnapshot::Present(base) = expected {
                let temp_metadata = self
                    .dir
                    .metadata(&temp)
                    .map_err(|e| pre(format!("cannot inspect temporary file for `{}`: {e}", rel.display())))?;
                let expected_identity = base.identity.as_ref().expect("Unix snapshots carry identity");
                if temp_metadata.uid() != expected_identity.owner
                    || temp_metadata.gid() != expected_identity.group
                {
                    return Err(pre(format!(
                        "refusing to replace `{}` because ownership ({}/{}) cannot be preserved by \
                         the temporary file ({}/{})",
                        rel.display(),
                        expected_identity.owner,
                        expected_identity.group,
                        temp_metadata.uid(),
                        temp_metadata.gid()
                    )));
                }
            }

            file.write_all(contents.as_bytes())
                .map_err(|e| pre(format!("cannot stage `{}`: {e}", rel.display())))?;

            file.flush()
                .map_err(|e| pre(format!("cannot flush `{}`: {e}", rel.display())))?;
            file.sync_all()
                .map_err(|e| pre(format!("cannot sync `{}`: {e}", rel.display())))?;

            // Recheck after staging narrows the race to this comparison and the
            // rename below. There is no portable true CAS for ordinary files.
            let final_observed = self.snapshot(path).map_err(pre)?;
            if &final_observed != expected {
                return Err(pre(compare_conflict(&rel, expected, &final_observed)));
            }
            authorize().map_err(pre)?;

            if faults.before_rename() {
                return Err(pre(format!(
                    "injected publication failure before replacing `{}`",
                    rel.display()
                )));
            }

            self.dir
                .rename(&temp, &self.dir, &rel)
                .map_err(|e| pre(format!("cannot publish `{}` atomically: {e}", rel.display())))?;

            // Directory sync is supported on Unix, including the only platform
            // Smithy currently builds on. Failure is reported even though the
            // rename has occurred: durability was part of the operation.
            #[cfg(unix)]
            {
                if faults.parent_sync() {
                    return Err(WriteFailure::PublishedButDurabilityUncertain(format!(
                        "injected parent sync failure for `{}`",
                        rel.display()
                    )));
                }
                let parent_dir = self
                    .dir
                    .open(if parent.as_os_str().is_empty() {
                        Path::new(".")
                    } else {
                        parent
                    })
                    .map_err(|e| WriteFailure::PublishedButDurabilityUncertain(format!(
                        "cannot open parent of `{}` for sync: {e}",
                        rel.display()
                    )))?;
                parent_dir
                    .sync_all()
                    .map_err(|e| WriteFailure::PublishedButDurabilityUncertain(format!(
                        "cannot sync parent of `{}`: {e}",
                        rel.display()
                    )))?;
            }
            Ok(())
        })();

        if result.is_err() && temp_created {
            let _ = self.dir.remove_file(&temp);
        }
        if result.as_ref().is_err_and(|error| !error.published()) {
            self.rollback_created_parents(&created_parents);
        }
        result
    }

    fn parent_needs_creation(&self, parent: &Path) -> Result<bool, String> {
        match self.dir.symlink_metadata(parent) {
            Ok(metadata) if metadata.file_type().is_symlink() => Err(format!(
                "refusing to create `{}` through symlink `{}`",
                parent.display(),
                parent.display()
            )),
            Ok(metadata) if metadata.is_dir() => Ok(false),
            Ok(_) => Err(format!("`{}` exists but is not a directory", parent.display())),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(true),
            Err(error) => Err(format!("cannot inspect `{}`: {error}", parent.display())),
        }
    }

    fn create_missing_parents(
        &self,
        parent: &Path,
        created: &mut Vec<CreatedDirectory>,
    ) -> Result<(), String> {
        let mut prefix = PathBuf::new();
        for component in parent.components() {
            prefix.push(component.as_os_str());
            match self.dir.symlink_metadata(&prefix) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(format!("`{}` is a symlink", prefix.display()));
                }
                Ok(metadata) if metadata.is_dir() => continue,
                Ok(_) => {
                    return Err(format!(
                        "`{}` exists but is not a directory",
                        prefix.display()
                    ));
                }
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!("cannot inspect `{}`: {error}", prefix.display()));
                }
            }

            match self.dir.create_dir(&prefix) {
                Ok(()) => {
                    let metadata = self
                        .dir
                        .symlink_metadata(&prefix)
                        .map_err(|error| {
                            format!(
                                "created `{}` but cannot identify it: {error}",
                                prefix.display()
                            )
                        })?;
                    created.push(CreatedDirectory::new(prefix.clone(), &metadata));
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    let metadata = self.dir.symlink_metadata(&prefix).map_err(|inspect| {
                        format!(
                            "`{}` appeared concurrently and cannot be inspected: {inspect}",
                            prefix.display()
                        )
                    })?;
                    if metadata.file_type().is_symlink() || !metadata.is_dir() {
                        return Err(format!(
                            "`{}` appeared concurrently but is not an ordinary directory",
                            prefix.display()
                        ));
                    }
                }
                Err(error) => {
                    return Err(format!("cannot create `{}`: {error}", prefix.display()));
                }
            }
        }
        Ok(())
    }

    fn rollback_created_parents(&self, created: &[CreatedDirectory]) {
        for directory in created.iter().rev() {
            let Ok(metadata) = self.dir.symlink_metadata(&directory.path) else {
                continue;
            };
            if !metadata.is_dir() || !directory.is_same(&metadata) {
                continue;
            }
            let Ok(mut entries) = self.dir.read_dir(&directory.path) else {
                continue;
            };
            if entries.next().is_none() {
                // `remove_dir` itself rechecks emptiness. A file arriving after
                // the read makes this fail safely rather than deleting it.
                let _ = self.dir.remove_dir(&directory.path);
            }
        }
    }

    fn reject_symlink_components(&self, rel: &Path) -> Result<(), String> {
        let mut prefix = PathBuf::new();
        for component in rel.components() {
            prefix.push(component.as_os_str());
            match self.dir.symlink_metadata(&prefix) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(format!(
                        "refusing to write `{}` because `{}` is a symlink",
                        rel.display(),
                        prefix.display()
                    ));
                }
                Ok(_) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => break,
                Err(error) => {
                    return Err(format!(
                        "cannot inspect path component `{}`: {error}",
                        prefix.display()
                    ));
                }
            }
        }
        Ok(())
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

impl WorkspaceReader {
    /// Revalidate and read a walker-discovered relative file.
    ///
    /// `None` is an ordinary oversize file. Every path or capability failure is
    /// an error so callers cannot accidentally turn a symlink swap into an
    /// ambient fallback.
    pub(crate) fn read_limited(
        &self,
        relative: &Path,
        max_bytes: u64,
    ) -> Result<Option<Vec<u8>>, String> {
        reject_symlink_components_in(&self.dir, relative)?;
        let metadata = self
            .dir
            .symlink_metadata(relative)
            .map_err(|error| format!("cannot inspect `{}`: {error}", relative.display()))?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "refusing to read `{}` because it is a symlink",
                relative.display()
            ));
        }
        if !metadata.is_file() {
            return Err(format!("`{}` is not a regular file", relative.display()));
        }
        if metadata.len() > max_bytes {
            return Ok(None);
        }
        let bytes = self
            .dir
            .read(relative)
            .map_err(|error| format!("cannot read `{}`: {error}", relative.display()))?;
        if bytes.len() as u64 > max_bytes {
            return Ok(None);
        }
        Ok(Some(bytes))
    }
}

fn reject_symlink_components_in(dir: &Dir, relative: &Path) -> Result<(), String> {
    let mut prefix = PathBuf::new();
    for component in relative.components() {
        prefix.push(component.as_os_str());
        match dir.symlink_metadata(&prefix) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(format!(
                    "refusing to read `{}` because `{}` is a symlink",
                    relative.display(),
                    prefix.display()
                ))
            }
            Ok(_) => {}
            Err(error) => {
                return Err(format!(
                    "cannot inspect `{}` while reading `{}`: {error}",
                    prefix.display(),
                    relative.display()
                ))
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn file_extended_attributes(file: &cap_std::fs::File) -> Result<Vec<String>, String> {
    use std::os::fd::AsRawFd;

    // `flistxattr` asks about the already capability-opened inode, so there is
    // no ambient path resolution and no second symlink race. A null buffer with
    // size zero is the documented count-only form.
    let count = unsafe {
        libc::flistxattr(
            file.as_raw_fd(),
            std::ptr::null_mut(),
            0,
            0,
        )
    };
    if count < 0 {
        return Err(format!(
            "cannot inspect extended attributes: {}",
            std::io::Error::last_os_error()
        ));
    }
    if count == 0 {
        return Ok(Vec::new());
    }
    let mut bytes = vec![0u8; count as usize];
    let written = unsafe {
        libc::flistxattr(
            file.as_raw_fd(),
            bytes.as_mut_ptr().cast(),
            bytes.len(),
            0,
        )
    };
    if written < 0 {
        return Err(format!(
            "cannot read extended attributes: {}",
            std::io::Error::last_os_error()
        ));
    }
    bytes.truncate(written as usize);
    Ok(bytes
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
        .map(|name| String::from_utf8_lossy(name).into_owned())
        .collect())
}

fn compare_conflict(rel: &Path, expected: &FileSnapshot, observed: &FileSnapshot) -> String {
    let expected = match expected {
        FileSnapshot::Missing => "missing",
        FileSnapshot::Present(_) => "the previewed file identity and content",
    };
    let observed = match observed {
        FileSnapshot::Missing => "missing",
        FileSnapshot::Present(_) => "different identity or content",
    };
    format!(
        "`{}` changed since preview (expected {expected}, found {observed}). Nothing was written; \
         re-read the file and reissue the change.",
        rel.display()
    )
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

    /// A retired generation used to leave directories behind because mkdir ran
    /// before the first lifecycle check.
    #[test]
    fn stale_authorization_creates_no_parent_directories() {
        let (tmp, ws) = workspace();
        let error = ws
            .compare_and_write_authorized(
                "stale/nested/file.txt",
                &FileSnapshot::Missing,
                "content",
                || Err("generation retired".into()),
            )
            .unwrap_err();
        assert!(error.to_string().contains("generation retired"));
        assert!(!tmp.path().join("stale").exists());
    }

    /// Expected-base validation is earlier than authorization and mkdir. A stale
    /// request cannot mutate the tree merely by naming a missing nested path.
    #[test]
    fn a_base_conflict_creates_no_parent_directories() {
        let (tmp, ws) = workspace();
        let expected = FileSnapshot::Present(FileBase {
            content: "expected".into(),
            identity: None,
        });
        let error = ws
            .compare_and_write_authorized(
                "conflict/nested/file.txt",
                &expected,
                "content",
                || Ok(()),
            )
            .unwrap_err();
        assert!(error.to_string().contains("changed since preview"));
        assert!(!tmp.path().join("conflict").exists());
    }

    /// Cancellation after mkdir but before rename must unwind only the empty
    /// directories this operation introduced.
    #[test]
    fn cancellation_after_mkdir_rolls_back_empty_parents() {
        let (tmp, ws) = workspace();
        let mut checks = 0;
        let error = ws
            .compare_and_write_authorized(
                "cancel/nested/file.txt",
                &FileSnapshot::Missing,
                "content",
                || {
                    checks += 1;
                    if checks == 1 {
                        Ok(())
                    } else {
                        Err("stopped by user".into())
                    }
                },
            )
            .unwrap_err();
        assert!(error.to_string().contains("stopped by user"));
        assert_eq!(checks, 2);
        assert!(!tmp.path().join("cancel").exists());
    }

    /// Rollback uses directory identity and emptiness. If another actor puts a
    /// file into a newly-created parent, neither it nor its ancestors are removed.
    #[test]
    fn a_concurrent_file_prevents_unsafe_parent_removal() {
        let (tmp, ws) = workspace();
        let root = tmp.path().to_path_buf();
        let mut checks = 0;
        let error = ws
            .compare_and_write_authorized(
                "shared/nested/file.txt",
                &FileSnapshot::Missing,
                "content",
                || {
                    checks += 1;
                    if checks == 1 {
                        Ok(())
                    } else {
                        fs::write(root.join("shared/nested/concurrent.txt"), "theirs").unwrap();
                        Err("stopped by user".into())
                    }
                },
            )
            .unwrap_err();
        assert!(error.to_string().contains("stopped by user"));
        assert_eq!(
            fs::read_to_string(tmp.path().join("shared/nested/concurrent.txt")).unwrap(),
            "theirs"
        );
        assert!(tmp.path().join("shared/nested").is_dir());
    }

    /// Successful publication owns its parent creation permanently; cleanup is
    /// only a pre-publication rollback path.
    #[test]
    fn a_successful_nested_write_retains_created_parents() {
        let (tmp, ws) = workspace();
        ws.compare_and_write(
            "kept/nested/file.txt",
            &FileSnapshot::Missing,
            "content",
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(tmp.path().join("kept/nested/file.txt")).unwrap(),
            "content"
        );
        assert!(tmp.path().join("kept/nested").is_dir());
    }

    /// The same stateful authorization lease is checked once before mkdir and
    /// again after final base validation immediately before rename.
    #[test]
    fn authorization_is_rechecked_before_rename_after_creating_parents() {
        let (tmp, ws) = workspace();
        let mut checks = 0;
        ws.compare_and_write_authorized(
            "authorized/nested/file.txt",
            &FileSnapshot::Missing,
            "content",
            || {
                checks += 1;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(checks, 2);
        assert_eq!(
            fs::read_to_string(tmp.path().join("authorized/nested/file.txt")).unwrap(),
            "content"
        );
    }

    /// Missing and empty were previously both represented by `unwrap_or_default`,
    /// so a file created after preview could be overwritten without a conflict.
    #[test]
    fn missing_and_present_but_empty_are_different_expected_bases() {
        let (_tmp, ws) = workspace();
        let missing = ws.snapshot("new.txt").unwrap();
        assert_eq!(missing, FileSnapshot::Missing);

        ws.write("new.txt", "").unwrap();
        assert_eq!(
            ws.snapshot("new.txt").unwrap().content(),
            Some("")
        );
        let error = ws
            .compare_and_write("new.txt", &missing, "reviewed")
            .unwrap_err();
        assert!(error.to_string().contains("changed since preview"), "{error}");
        assert_eq!(ws.read_to_string("new.txt").unwrap(), "");
    }

    /// A final-component symlink must never inherit write semantics: replacing
    /// its target would escape the reviewed pathname, even inside the workspace.
    #[cfg(unix)]
    #[test]
    fn a_final_component_symlink_is_refused() {
        let (tmp, ws) = workspace();
        fs::write(tmp.path().join("target.txt"), "target").unwrap();
        std::os::unix::fs::symlink("target.txt", tmp.path().join("link.txt")).unwrap();

        let error = ws.write("link.txt", "replacement").unwrap_err();
        assert!(error.contains("`link.txt` is a symlink"), "{error}");
        assert_eq!(fs::read_to_string(tmp.path().join("target.txt")).unwrap(), "target");
    }

    /// Truncate-in-place destroyed the original before an I/O failure was known.
    /// A fault after staging must leave the original byte-for-byte intact.
    #[test]
    fn a_fault_before_publication_leaves_the_original_intact() {
        let (tmp, ws) = workspace();
        let expected = ws.snapshot("src/main.rs").unwrap();
        let mut authorize = || Ok(());
        let error = ws
            .compare_and_write_inner(
                "src/main.rs",
                &expected,
                "replacement\n",
                &mut authorize,
                WriteFaults {
                    before_rename: true,
                    parent_sync: false,
                },
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("injected publication failure"),
            "{error}"
        );
        assert_eq!(
            fs::read_to_string(tmp.path().join("src/main.rs")).unwrap(),
            "fn main() {}\n"
        );
    }

    /// Failed staging used to leave dotfiles that later appeared in searches
    /// and diffs. Cleanup is part of every pre-rename error path.
    #[test]
    fn a_faulted_publication_leaves_no_temporary_debris() {
        let (tmp, ws) = workspace();
        let expected = ws.snapshot("src/main.rs").unwrap();
        let mut authorize = || Ok(());
        let _ = ws.compare_and_write_inner(
            "src/main.rs",
            &expected,
            "replacement\n",
            &mut authorize,
            WriteFaults {
                before_rename: true,
                parent_sync: false,
            },
        );
        let names: Vec<_> = fs::read_dir(tmp.path().join("src"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("main.rs")]);
    }

    /// Atomic rename installs the temporary inode, so its default mode would
    /// silently remove executable or owner-only permissions without this copy.
    #[cfg(unix)]
    #[test]
    fn replacing_a_file_preserves_its_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let (tmp, ws) = workspace();
        let path = tmp.path().join("src/main.rs");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o751)).unwrap();
        ws.write("src/main.rs", "replacement\n").unwrap();
        assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o751);
    }

    /// A symlink in a parent component can redirect the temporary file and
    /// rename even when the final component is ordinary. Reviewed paths reject
    /// every symlink component rather than relying on where it happens to point.
    #[cfg(unix)]
    #[test]
    fn an_intermediate_symlink_is_refused_even_when_it_points_inward() {
        let (tmp, ws) = workspace();
        fs::create_dir(tmp.path().join("real")).unwrap();
        std::os::unix::fs::symlink("real", tmp.path().join("alias")).unwrap();
        let error = ws.write("alias/new.txt", "content").unwrap_err();
        assert!(error.contains("`alias` is a symlink"), "{error}");
        assert!(!tmp.path().join("real/new.txt").exists());
    }

    /// Atomic replacement of one hard-link name silently splits it from its
    /// aliases. That semantic change is refused rather than called preservation.
    #[cfg(unix)]
    #[test]
    fn a_hard_linked_destination_is_refused() {
        let (tmp, ws) = workspace();
        fs::hard_link(
            tmp.path().join("src/main.rs"),
            tmp.path().join("src/alias.rs"),
        )
        .unwrap();
        let error = ws.write("src/main.rs", "replacement\n").unwrap_err();
        assert!(error.contains("hard links"), "{error}");
        assert_eq!(
            fs::read_to_string(tmp.path().join("src/alias.rs")).unwrap(),
            "fn main() {}\n"
        );
    }

    /// Content equality alone cannot detect delete-and-recreate. Device/inode
    /// identity makes an identical-byte substitution stale on Unix.
    #[cfg(unix)]
    #[test]
    fn same_content_target_substitution_is_detected() {
        let (tmp, ws) = workspace();
        let expected = ws.snapshot("src/main.rs").unwrap();
        let path = tmp.path().join("src/main.rs");
        fs::remove_file(&path).unwrap();
        fs::write(&path, "fn main() {}\n").unwrap();

        let error = ws
            .compare_and_write("src/main.rs", &expected, "replacement\n")
            .unwrap_err();
        assert!(error.to_string().contains("identity or content"), "{error}");
        assert_eq!(fs::read_to_string(path).unwrap(), "fn main() {}\n");
    }

    /// Lifecycle approval is checked after staging and the final base compare,
    /// immediately before rename. Retirement there leaves the original intact.
    #[test]
    fn publication_authorization_is_rechecked_at_the_rename_boundary() {
        let (tmp, ws) = workspace();
        let expected = ws.snapshot("src/main.rs").unwrap();
        let error = ws
            .compare_and_write_authorized(
                "src/main.rs",
                &expected,
                "replacement\n",
                || Err("turn retired".into()),
            )
            .unwrap_err();
        assert!(error.to_string().contains("turn retired"), "{error}");
        assert_eq!(
            fs::read_to_string(tmp.path().join("src/main.rs")).unwrap(),
            "fn main() {}\n"
        );
    }

    /// Parent fsync happens after rename. Reporting that as "nothing written"
    /// makes the caller retry against bytes that may already be live.
    #[cfg(unix)]
    #[test]
    fn parent_sync_failure_reports_publication_as_uncertain() {
        let (tmp, ws) = workspace();
        let expected = ws.snapshot("src/main.rs").unwrap();
        let mut authorize = || Ok(());
        let error = ws
            .compare_and_write_inner(
                "src/main.rs",
                &expected,
                "replacement\n",
                &mut authorize,
                WriteFaults {
                    before_rename: false,
                    parent_sync: true,
                },
            )
            .unwrap_err();
        assert!(error.published());
        assert!(error.to_string().contains("may already be published"));
        assert_eq!(
            fs::read_to_string(tmp.path().join("src/main.rs")).unwrap(),
            "replacement\n"
        );
    }

    /// A new file gets owner-only mode before bytes are staged; process umask is
    /// not relied on to make model-generated content private.
    #[cfg(unix)]
    #[test]
    fn a_new_destination_starts_with_restrictive_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let (tmp, ws) = workspace();
        ws.write("secret.txt", "content").unwrap();
        assert_eq!(
            fs::metadata(tmp.path().join("secret.txt"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    /// macOS xattrs would be dropped with the old inode. They are detectable on
    /// the capability-opened fd, so replacement fails safely instead.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_destination_with_extended_attributes_is_refused() {
        use std::os::fd::AsRawFd;

        let (tmp, ws) = workspace();
        let file = fs::OpenOptions::new()
            .write(true)
            .open(tmp.path().join("src/main.rs"))
            .unwrap();
        let name = std::ffi::CString::new("com.smithy.test").unwrap();
        let value = b"metadata";
        let result = unsafe {
            libc::fsetxattr(
                file.as_raw_fd(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
                0,
            )
        };
        assert_eq!(result, 0, "fixture could not set an xattr");

        let error = ws.write("src/main.rs", "replacement\n").unwrap_err();
        assert!(error.contains("extended attributes"), "{error}");
        assert_eq!(
            fs::read_to_string(tmp.path().join("src/main.rs")).unwrap(),
            "fn main() {}\n"
        );
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
