//! The files that assemble a Session.
//!
//! Skills inject into a turn. The Harness is born with the Session: system
//! prompt first, then any `include` listed in `harness.toml`. A markdown file
//! sitting in the harness directory is not sent unless it is listed. Lookup
//! matches Skills — Project, then user, then what Smithy ships — and a
//! mid-Session edit does not hot-reload.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Shipped system prompt. `{{workspace}}` and `{{tools}}` are filled in.
pub const SYSTEM_TEMPLATE: &str = include_str!("../harness/SYSTEM.md");

/// Where a system prompt template was loaded from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HarnessSource {
    /// `{Project}/.smithy/harness/SYSTEM.md`
    Project(PathBuf),
    /// `~/.smithy/harness/SYSTEM.md`
    User(PathBuf),
    /// The template compiled into this crate.
    Shipped,
}

impl HarnessSource {
    pub fn label(&self) -> String {
        match self {
            HarnessSource::Project(path) => format!("Project {}", path.display()),
            HarnessSource::User(path) => format!("user {}", path.display()),
            HarnessSource::Shipped => "shipped".into(),
        }
    }
}

/// One file listed in `harness.toml` `include` and actually loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncludedFile {
    pub name: String,
    pub path: PathBuf,
    pub body: String,
}

/// Everything that will be frozen into the system prompt besides the Map.
#[derive(Debug, Clone)]
pub struct Harness {
    pub source: HarnessSource,
    pub template: String,
    pub includes: Vec<IncludedFile>,
    /// `harness.toml` that supplied the include list, if any.
    pub manifest: Option<PathBuf>,
    /// Include names that were listed but not found, or illegal.
    pub notices: Vec<String>,
}

impl Harness {
    /// SYSTEM.md plus includes, with `{{workspace}}` / `{{tools}}` filled in.
    /// The Map is joined afterwards by [`crate::session::with_project_context`].
    pub fn filled_base(&self, workspace: &Path, tool_names: &[&str]) -> String {
        let mut base = fill_system_template(&self.template, workspace, tool_names);
        for inc in &self.includes {
            base.push_str("\n\n");
            base.push_str(inc.body.trim_end());
        }
        base
    }
}

#[derive(Debug, Deserialize, Default)]
struct HarnessToml {
    #[serde(default)]
    include: Vec<String>,
}

pub fn user_harness_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".smithy/harness"))
}

pub fn project_harness_dir(project: &Path) -> PathBuf {
    project.join(".smithy/harness")
}

pub fn project_system_prompt_path(project: &Path) -> PathBuf {
    project_harness_dir(project).join("SYSTEM.md")
}

fn read_text(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    Some(text.replace("\r\n", "\n"))
}

/// Template and where it came from. Project file, then user file, then shipped.
pub fn resolve_system_template(project: &Path) -> (HarnessSource, String) {
    resolve_system_template_in(project, user_harness_dir())
}

fn resolve_system_template_in(
    project: &Path,
    user_dir: Option<PathBuf>,
) -> (HarnessSource, String) {
    let project_path = project_system_prompt_path(project);
    if let Some(text) = read_text(&project_path) {
        return (HarnessSource::Project(project_path), text);
    }
    if let Some(user_path) = user_dir.map(|d| d.join("SYSTEM.md")) {
        if let Some(text) = read_text(&user_path) {
            return (HarnessSource::User(user_path), text);
        }
    }
    (HarnessSource::Shipped, SYSTEM_TEMPLATE.to_string())
}

/// Load the Harness for a Project. Missing include names are notices, not errors.
pub fn load_harness(project: &Path) -> Harness {
    load_harness_in(project, user_harness_dir())
}

fn load_harness_in(project: &Path, user_dir: Option<PathBuf>) -> Harness {
    let (source, template) = resolve_system_template_in(project, user_dir.clone());
    let project_dir = project_harness_dir(project);
    let (names, manifest) = include_names(&project_dir, user_dir.as_deref());
    let mut notices = Vec::new();
    let mut includes = Vec::new();
    for name in names {
        match resolve_include(&name, &project_dir, user_dir.as_deref()) {
            Ok(path) => match read_text(&path) {
                Some(body) => includes.push(IncludedFile { name, path, body }),
                None => notices.push(format!(
                    "Harness include `{name}` unreadable at {}",
                    path.display()
                )),
            },
            Err(e) => notices.push(e),
        }
    }
    Harness {
        source,
        template,
        includes,
        manifest,
        notices,
    }
}

fn include_names(project_dir: &Path, user_dir: Option<&Path>) -> (Vec<String>, Option<PathBuf>) {
    let project_toml = project_dir.join("harness.toml");
    if project_toml.is_file() {
        return (parse_include_list(&project_toml), Some(project_toml));
    }
    if let Some(user) = user_dir {
        let user_toml = user.join("harness.toml");
        if user_toml.is_file() {
            return (parse_include_list(&user_toml), Some(user_toml));
        }
    }
    (Vec::new(), None)
}

fn parse_include_list(path: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    toml::from_str::<HarnessToml>(&text)
        .map(|t| t.include)
        .unwrap_or_default()
}

fn resolve_include(
    name: &str,
    project_dir: &Path,
    user_dir: Option<&Path>,
) -> Result<PathBuf, String> {
    let safe = include_basename(name)?;
    let project_path = project_dir.join(&safe);
    if project_path.is_file() {
        return Ok(project_path);
    }
    if let Some(user) = user_dir {
        let user_path = user.join(&safe);
        if user_path.is_file() {
            return Ok(user_path);
        }
    }
    Err(format!(
        "Harness include `{safe}` is listed but not in {} or the user harness directory",
        project_dir.display()
    ))
}

fn include_basename(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("empty Harness include name".into());
    }
    let path = Path::new(name);
    if path.components().count() != 1
        || name.contains("..")
        || Path::new(name)
            .file_name()
            .is_none_or(|f| f != path.as_os_str())
    {
        return Err(format!(
            "Harness include `{name}` must be a file name in the harness directory, not a path"
        ));
    }
    if path.extension().and_then(|e| e.to_str()) != Some("md") {
        return Err(format!("Harness include `{name}` must be a `.md` file"));
    }
    Ok(name.to_string())
}

/// Markdown files in the harness directories that `harness.toml` did not list.
pub fn unused_harness_files(project: &Path, harness: &Harness) -> Vec<PathBuf> {
    let mut listed: std::collections::HashSet<&str> =
        harness.includes.iter().map(|i| i.name.as_str()).collect();
    listed.insert("SYSTEM.md");
    let mut out = Vec::new();
    for dir in [project_harness_dir(project)]
        .into_iter()
        .chain(user_harness_dir())
    {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            if listed.contains(name) {
                continue;
            }
            out.push(path);
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Fill `{{workspace}}` / `{{tools}}`. Trailing whitespace is stripped so a
/// POSIX final newline in the file does not become a blank line in the prompt.
/// The Map is joined afterwards by [`crate::session::with_project_context`].
pub fn fill_system_template(template: &str, workspace: &Path, tool_names: &[&str]) -> String {
    template
        .replace("{{workspace}}", &workspace.display().to_string())
        .replace("{{tools}}", &tool_names.join(", "))
        .trim_end()
        .to_string()
}

/// Write the shipped template into the Project so it can be edited.
///
/// Refuses to overwrite an existing file — that file *is* the experiment.
pub fn init_project_harness(project: &Path) -> Result<PathBuf, String> {
    let dir = project_harness_dir(project);
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let path = dir.join("SYSTEM.md");
    if path.is_file() {
        return Err(format!(
            "{} already exists — edit that file, or delete it to restore the shipped prompt",
            path.display()
        ));
    }
    std::fs::write(&path, SYSTEM_TEMPLATE)
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_fill_substitutes_workspace_and_tools() {
        let path = Path::new("/tmp/ws");
        let tools = ["read", "write"];
        let rendered = fill_system_template(SYSTEM_TEMPLATE, path, &tools);
        assert!(rendered.contains("Workspace root: /tmp/ws"));
        assert!(rendered.contains("You have these tools: read, write."));
        assert!(rendered.contains("Be concise."));
        assert!(!rendered.contains("{{workspace}}"));
        assert!(!rendered.contains("{{tools}}"));
    }

    #[test]
    fn a_project_file_beats_the_shipped_template() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path();
        std::fs::create_dir_all(project_harness_dir(project)).unwrap();
        std::fs::write(
            project_system_prompt_path(project),
            "custom {{workspace}} :: {{tools}}\n",
        )
        .unwrap();

        let (source, template) =
            resolve_system_template_in(project, Some(tmp.path().join("user-harness")));
        assert!(matches!(source, HarnessSource::Project(_)));
        let prompt = fill_system_template(&template, project, &["read"]);
        assert!(prompt.starts_with("custom "));
        assert!(prompt.contains(":: read"));
    }

    #[test]
    fn a_user_file_beats_shipped_when_the_project_has_none() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("proj");
        let user = tmp.path().join("user-harness");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&user).unwrap();
        std::fs::write(user.join("SYSTEM.md"), "from-user {{tools}}\n").unwrap();

        let (source, template) = resolve_system_template_in(&project, Some(user));
        assert!(matches!(source, HarnessSource::User(_)));
        assert!(template.contains("from-user"));
    }

    #[test]
    fn init_refuses_to_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let path = init_project_harness(tmp.path()).unwrap();
        assert!(path.is_file());
        assert!(init_project_harness(tmp.path()).is_err());
    }

    #[test]
    fn an_unlisted_markdown_file_is_not_sent() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path();
        let dir = project_harness_dir(project);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("voice.md"), "ALWAYS SHOUT\n").unwrap();
        std::fs::write(dir.join("harness.toml"), "include = []\n").unwrap();

        let h = load_harness_in(project, None);
        assert!(h.includes.is_empty());
        let unused = unused_harness_files(project, &h);
        assert!(unused.iter().any(|p| p.ends_with("voice.md")));
        assert!(!h.filled_base(project, &["read"]).contains("ALWAYS SHOUT"));
    }

    #[test]
    fn a_listed_include_is_joined_onto_the_base() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path();
        let dir = project_harness_dir(project);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("voice.md"), "speak plainly\n").unwrap();
        std::fs::write(dir.join("harness.toml"), "include = [\"voice.md\"]\n").unwrap();

        let h = load_harness_in(project, None);
        assert_eq!(h.includes.len(), 1);
        let base = h.filled_base(project, &["read"]);
        assert!(base.contains("speak plainly"));
    }

    #[test]
    fn a_path_include_is_rejected() {
        assert!(include_basename("../secret.md").is_err());
        assert!(include_basename("nested/voice.md").is_err());
        assert!(include_basename("voice.txt").is_err());
        assert_eq!(include_basename("voice.md").unwrap(), "voice.md");
    }
}
