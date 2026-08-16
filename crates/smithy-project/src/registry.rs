//! Recent projects, and where each one's conversations live.
//!
//! This is what makes "project groupings" real: sessions are stored *under* a
//! project rather than in one global pile, so opening a project shows you that
//! project's conversation history and nothing else.
//!
//! A project's storage directory is derived from a hash of its canonical root
//! path. That means moving a project loses its history — an acceptable trade
//! for never having to maintain a mutable path index that can disagree with the
//! filesystem.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A project the user has opened before.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecentProject {
    pub root: PathBuf,
    pub name: String,
    /// Unix seconds.
    pub last_opened: u64,
}

/// The list of known projects, and the layout of per-project storage.
pub struct ProjectRegistry {
    data_dir: PathBuf,
}

impl ProjectRegistry {
    pub fn new(data_dir: impl Into<PathBuf>) -> Result<Self, String> {
        let data_dir = data_dir.into();
        std::fs::create_dir_all(&data_dir)
            .map_err(|e| format!("cannot create {}: {e}", data_dir.display()))?;
        Ok(Self { data_dir })
    }

    /// `~/.local/share/smithy`.
    pub fn default_location() -> Result<Self, String> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or("HOME is not set")?;
        ProjectRegistry::new(home.join(".local/share/smithy"))
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    fn recents_path(&self) -> PathBuf {
        self.data_dir.join("recent-projects.json")
    }

    /// Where a project's sessions live.
    pub fn sessions_dir(&self, root: &Path) -> PathBuf {
        self.data_dir
            .join("projects")
            .join(project_key(root))
            .join("sessions")
    }

    /// Where a project's persisted call graph lives.
    ///
    /// Beside `sessions/`, under the same key, so everything Smithy knows about
    /// one project sits in one browsable directory.
    pub fn callgraph_path(&self, root: &Path) -> PathBuf {
        self.data_dir
            .join("projects")
            .join(project_key(root))
            .join("callgraph.json")
    }

    pub fn recents(&self) -> Vec<RecentProject> {
        let Ok(text) = std::fs::read_to_string(self.recents_path()) else {
            return Vec::new();
        };
        let mut list: Vec<RecentProject> = serde_json::from_str(&text).unwrap_or_default();
        // A project deleted on disk should not keep appearing in the menu.
        list.retain(|p| p.root.is_dir());
        list.sort_by_key(|p| std::cmp::Reverse(p.last_opened));
        list
    }

    /// Record a project as opened, moving it to the front.
    pub fn touch(&self, root: &Path, name: &str) -> Result<(), String> {
        let mut list = self.recents();
        list.retain(|p| p.root != root);
        list.insert(
            0,
            RecentProject {
                root: root.to_path_buf(),
                name: name.to_string(),
                last_opened: unix_seconds(),
            },
        );
        list.truncate(20);

        let json = serde_json::to_string_pretty(&list)
            .map_err(|e| format!("cannot serialize recent projects: {e}"))?;
        // Write-then-rename so an interrupted write cannot leave a file that
        // fails to parse on next launch.
        let final_path = self.recents_path();
        let tmp = final_path.with_extension("json.tmp");
        std::fs::write(&tmp, json).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &final_path)
            .map_err(|e| format!("cannot finalize {}: {e}", final_path.display()))?;
        Ok(())
    }

    pub fn forget(&self, root: &Path) -> Result<(), String> {
        let mut list = self.recents();
        list.retain(|p| p.root != root);
        let json = serde_json::to_string_pretty(&list).map_err(|e| e.to_string())?;
        std::fs::write(self.recents_path(), json).map_err(|e| e.to_string())
    }
}

/// A filesystem-safe, stable key for a project root.
///
/// FNV-1a of the canonical path, hex encoded. Stable across processes, unlike
/// anything built on `RandomState`.
pub fn project_key(root: &Path) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in root.to_string_lossy().as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    // Prefix with the directory name so the storage tree is browsable by a
    // human rather than being an opaque field of hashes.
    let name: String = root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "project".into())
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(32)
        .collect();
    format!("{name}-{hash:016x}")
}

fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> (tempfile::TempDir, ProjectRegistry) {
        let tmp = tempfile::tempdir().unwrap();
        let reg = ProjectRegistry::new(tmp.path().join("data")).unwrap();
        (tmp, reg)
    }

    #[test]
    fn a_fresh_registry_has_no_recents() {
        let (_t, reg) = registry();
        assert!(reg.recents().is_empty());
    }

    #[test]
    fn touching_records_a_project() {
        let (tmp, reg) = registry();
        let project = tmp.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();

        reg.touch(&project, "proj").unwrap();
        let recents = reg.recents();
        assert_eq!(recents.len(), 1);
        assert_eq!(recents[0].name, "proj");
    }

    #[test]
    fn touching_again_moves_a_project_to_the_front_without_duplicating() {
        let (tmp, reg) = registry();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();

        reg.touch(&a, "a").unwrap();
        reg.touch(&b, "b").unwrap();
        reg.touch(&a, "a").unwrap();

        let recents = reg.recents();
        assert_eq!(recents.len(), 2, "should not duplicate");
        assert_eq!(recents[0].name, "a", "most recent first");
    }

    /// A deleted project should stop appearing rather than offering a dead link.
    #[test]
    fn recents_drops_projects_that_no_longer_exist() {
        let (tmp, reg) = registry();
        let gone = tmp.path().join("gone");
        std::fs::create_dir_all(&gone).unwrap();
        reg.touch(&gone, "gone").unwrap();
        assert_eq!(reg.recents().len(), 1);

        std::fs::remove_dir_all(&gone).unwrap();
        assert!(reg.recents().is_empty());
    }

    #[test]
    fn forgetting_removes_a_project() {
        let (tmp, reg) = registry();
        let p = tmp.path().join("p");
        std::fs::create_dir_all(&p).unwrap();
        reg.touch(&p, "p").unwrap();
        reg.forget(&p).unwrap();
        assert!(reg.recents().is_empty());
    }

    #[test]
    fn the_recents_list_is_capped() {
        let (tmp, reg) = registry();
        for i in 0..30 {
            let p = tmp.path().join(format!("p{i}"));
            std::fs::create_dir_all(&p).unwrap();
            reg.touch(&p, &format!("p{i}")).unwrap();
        }
        assert_eq!(reg.recents().len(), 20);
    }

    /// Different projects must not share a session directory.
    #[test]
    fn each_project_gets_its_own_sessions_directory() {
        let (_t, reg) = registry();
        let a = reg.sessions_dir(Path::new("/home/u/alpha"));
        let b = reg.sessions_dir(Path::new("/home/u/beta"));
        assert_ne!(a, b);
        assert!(a.ends_with("sessions"));
    }

    #[test]
    fn the_same_project_always_maps_to_the_same_directory() {
        let (_t, reg) = registry();
        let path = Path::new("/home/u/alpha");
        assert_eq!(reg.sessions_dir(path), reg.sessions_dir(path));
    }

    /// The storage tree should be browsable, not an opaque field of hashes.
    #[test]
    fn the_project_key_carries_a_readable_name() {
        let key = project_key(Path::new("/home/u/my-project"));
        assert!(key.starts_with("my-project-"), "got {key}");
        assert_eq!(key.len(), "my-project-".len() + 16);
    }

    #[test]
    fn awkward_directory_names_produce_safe_keys() {
        let key = project_key(Path::new("/tmp/we!rd na@me #1"));
        assert!(
            key.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "key must be filesystem safe, got {key}"
        );
    }

    #[test]
    fn a_corrupt_recents_file_does_not_take_out_the_list() {
        let (_t, reg) = registry();
        std::fs::write(reg.recents_path(), "{ not json").unwrap();
        assert!(
            reg.recents().is_empty(),
            "should degrade to empty, not panic"
        );
    }

    #[test]
    fn saving_leaves_no_temp_file_behind() {
        let (tmp, reg) = registry();
        let p = tmp.path().join("p");
        std::fs::create_dir_all(&p).unwrap();
        reg.touch(&p, "p").unwrap();
        reg.touch(&p, "p").unwrap();

        let strays: Vec<String> = std::fs::read_dir(reg.data_dir())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(strays.is_empty(), "left temp files: {strays:?}");
    }
}
