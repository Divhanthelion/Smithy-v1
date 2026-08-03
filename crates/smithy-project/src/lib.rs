//! Projects — what the agent is grounded in.
//!
//! Before this, the agent was confined to whatever directory the app happened
//! to launch from, with no way to point it somewhere else. A `Project` is an
//! explicit answer to "what am I working on": a root directory, a detected
//! kind, and a deterministic description of its structure.
//!
//! ## The one hard constraint
//!
//! [`ProjectContext::rendered`] is injected into the **system prompt**, which
//! must stay byte-identical for the life of a session or the model's prefix
//! cache is thrown away and every turn pays a full cold prefill. So the context
//! is built **once**, at project open, and is deliberately not refreshed when
//! files change. Regenerating it means starting a new session.
//!
//! Noticing that it *has* gone stale is a thing this crate can support and the
//! application does not do — see [`ProjectContext::fingerprint`], which used to
//! claim otherwise.
//!
//! This is also why extraction must be **deterministic**: same tree in, same
//! bytes out. Anything that varies between runs — timestamps, hash-map
//! iteration order, absolute paths that embed a temp directory — would change
//! the prefix and defeat the point. There are tests for exactly that.

pub mod callgraph;
pub mod context;
pub mod registry;
pub mod rust;
pub mod scip;
pub mod symbols;

use std::path::{Path, PathBuf};

pub use context::{ContextBudget, ProjectContext};
pub use registry::{ProjectRegistry, RecentProject};

/// What kind of project a directory holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectKind {
    /// A Cargo project. `workspace` is true when the root manifest declares a
    /// `[workspace]` with members.
    Rust { workspace: bool },
    /// Anything else. Still usable — the agent just gets a file-tree summary
    /// instead of structured crate metadata.
    Generic,
}

impl ProjectKind {
    pub fn label(&self) -> &'static str {
        match self {
            ProjectKind::Rust { workspace: true } => "Rust workspace",
            ProjectKind::Rust { workspace: false } => "Rust crate",
            ProjectKind::Generic => "project",
        }
    }
}

/// An opened project.
#[derive(Debug, Clone)]
pub struct Project {
    pub root: PathBuf,
    pub name: String,
    pub kind: ProjectKind,
}

impl Project {
    /// Open `root` as a project, detecting its kind.
    pub fn open(root: impl AsRef<Path>) -> Result<Project, String> {
        let root = root
            .as_ref()
            .canonicalize()
            .map_err(|e| format!("cannot open {}: {e}", root.as_ref().display()))?;
        if !root.is_dir() {
            return Err(format!("{} is not a directory", root.display()));
        }

        let manifest = root.join("Cargo.toml");
        let kind = if manifest.is_file() {
            let text = std::fs::read_to_string(&manifest).unwrap_or_default();
            ProjectKind::Rust {
                workspace: declares_workspace(&text),
            }
        } else {
            ProjectKind::Generic
        };

        let name = root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| root.display().to_string());

        Ok(Project { root, name, kind })
    }

    /// Walk upward from `start` to find the nearest enclosing project root.
    ///
    /// Opening `src/` of a crate should ground the agent in the crate, not in
    /// `src/`. Stops at the filesystem root.
    pub fn discover(start: impl AsRef<Path>) -> Result<Project, String> {
        let start = start
            .as_ref()
            .canonicalize()
            .map_err(|e| format!("cannot resolve {}: {e}", start.as_ref().display()))?;

        let mut candidate: Option<PathBuf> = None;
        for dir in start.ancestors() {
            if dir.join("Cargo.toml").is_file() {
                // Keep walking: an inner crate inside a workspace should ground
                // at the workspace root, which is the outermost manifest.
                candidate = Some(dir.to_path_buf());
            }
            if dir.join(".git").exists() {
                // A repository boundary is a hard stop — never ground above it.
                return Project::open(candidate.unwrap_or_else(|| dir.to_path_buf()));
            }
        }
        match candidate {
            Some(root) => Project::open(root),
            None => Project::open(start),
        }
    }

    pub fn is_rust(&self) -> bool {
        matches!(self.kind, ProjectKind::Rust { .. })
    }

    /// Build the context block for this project.
    pub fn context(&self, budget: ContextBudget) -> ProjectContext {
        context::extract(self, budget)
    }
}

/// Whether a manifest declares a workspace with members.
///
/// Deliberately a textual check rather than a TOML parse: a virtual manifest
/// (a `[workspace]` with no `[package]`) is exactly the case that matters, and
/// it is unambiguous in the text.
fn declares_workspace(manifest: &str) -> bool {
    manifest
        .lines()
        .map(str::trim)
        .any(|l| l == "[workspace]" || l.starts_with("[workspace]"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rust_crate() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        tmp
    }

    #[test]
    fn detects_a_rust_crate() {
        let tmp = rust_crate();
        let project = Project::open(tmp.path()).unwrap();
        assert_eq!(project.kind, ProjectKind::Rust { workspace: false });
        assert!(project.is_rust());
    }

    #[test]
    fn detects_a_rust_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"a\"]\n",
        )
        .unwrap();
        let project = Project::open(tmp.path()).unwrap();
        assert_eq!(project.kind, ProjectKind::Rust { workspace: true });
    }

    #[test]
    fn a_directory_without_a_manifest_is_generic() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("notes.txt"), "hi").unwrap();
        assert_eq!(
            Project::open(tmp.path()).unwrap().kind,
            ProjectKind::Generic
        );
    }

    #[test]
    fn the_name_comes_from_the_directory() {
        let tmp = rust_crate();
        let project = Project::open(tmp.path()).unwrap();
        assert_eq!(
            project.name,
            tmp.path().file_name().unwrap().to_string_lossy()
        );
    }

    #[test]
    fn opening_a_file_path_is_refused() {
        let tmp = rust_crate();
        assert!(Project::open(tmp.path().join("Cargo.toml")).is_err());
    }

    /// Opening `src/` should ground the agent in the crate, not in `src/`.
    #[test]
    fn discovery_walks_up_to_the_manifest() {
        let tmp = rust_crate();
        let project = Project::discover(tmp.path().join("src")).unwrap();
        assert_eq!(project.root, tmp.path().canonicalize().unwrap());
    }

    /// An inner crate inside a workspace should ground at the workspace root,
    /// because that is where `cargo` commands and cross-crate edits operate.
    #[test]
    fn discovery_prefers_the_outermost_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[workspace]\nmembers=[\"inner\"]\n",
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("inner/src")).unwrap();
        std::fs::write(
            tmp.path().join("inner/Cargo.toml"),
            "[package]\nname=\"inner\"\nversion=\"0.1.0\"\n",
        )
        .unwrap();

        let project = Project::discover(tmp.path().join("inner/src")).unwrap();
        assert_eq!(project.root, tmp.path().canonicalize().unwrap());
    }

    /// A repository boundary is a hard stop: never ground above a `.git`.
    #[test]
    fn discovery_stops_at_a_repository_boundary() {
        let outer = tempfile::tempdir().unwrap();
        std::fs::write(outer.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        let repo = outer.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(
            repo.join("Cargo.toml"),
            "[package]\nname=\"r\"\nversion=\"0.1.0\"\n",
        )
        .unwrap();

        let project = Project::discover(repo.join("src")).unwrap();
        assert_eq!(project.root, repo.canonicalize().unwrap());
    }

    #[test]
    fn discovery_of_a_plain_directory_returns_that_directory() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("deep/nested")).unwrap();
        let project = Project::discover(tmp.path().join("deep/nested")).unwrap();
        assert_eq!(project.kind, ProjectKind::Generic);
    }

    #[test]
    fn workspace_detection_ignores_a_mention_in_a_comment() {
        assert!(!declares_workspace(
            "# see [workspace] docs\n[package]\nname=\"x\"\n"
        ));
        assert!(declares_workspace("[workspace]\nmembers = []\n"));
        assert!(declares_workspace("[package]\nname=\"x\"\n\n[workspace]\n"));
    }
}
