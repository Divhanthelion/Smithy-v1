//! Skills and Commands.
//!
//! A **Skill** is a `SKILL.md` on disk. A **Command** is the user typing `/name`
//! in the composer. Skills run only then — never from ambient text.
//!
//! Lookup: Project `.smithy/skills/<name>/SKILL.md`, then
//! `~/.smithy/skills/<name>/SKILL.md`. Editing a skill mid-Session does not
//! hot-reload; a new Command (or New session) rebuilds.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// How a Session is tooled and prompted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionKind {
    #[default]
    Coding,
    Research,
    Grill,
}

impl SessionKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Coding => "Coding",
            Self::Research => "Research",
            Self::Grill => "Grill",
        }
    }

    pub fn from_name(name: &str) -> Self {
        match name {
            "research" => Self::Research,
            "grill-me" => Self::Grill,
            _ => Self::Coding,
        }
    }
}

/// Tool set a Skill asks for. Frozen for the Session, like today's core.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToolProfile {
    #[default]
    Coding,
    Research,
    Grill,
}

impl ToolProfile {
    fn parse(s: &str) -> Self {
        match s.trim() {
            "research" => Self::Research,
            "grill" | "grill-me" => Self::Grill,
            _ => Self::Coding,
        }
    }

    pub fn kind(self) -> SessionKind {
        match self {
            Self::Coding => SessionKind::Coding,
            Self::Research => SessionKind::Research,
            Self::Grill => SessionKind::Grill,
        }
    }
}

/// Picker row: enough to show `/name` without loading the body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillMeta {
    pub name: String,
    pub description: String,
    pub argument_hint: String,
    pub profile: ToolProfile,
}

/// A loaded Skill: frontmatter plus the procedure spliced into the system prompt.
#[derive(Clone, Debug)]
pub struct Skill {
    pub meta: SkillMeta,
    pub body: String,
    pub dir: PathBuf,
}

/// `/name` plus the remainder of the line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    pub name: String,
    pub rest: String,
}

/// Parse a composer line as a Command. `/Users/...` is a path, not a Command.
pub fn parse_command(input: &str) -> Option<Command> {
    let s = input.trim_start();
    let rest = s.strip_prefix('/')?;
    let name_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let name = &rest[..name_end];
    if !is_skill_name(name) {
        return None;
    }
    Some(Command {
        name: name.to_string(),
        rest: rest[name_end..].trim().to_string(),
    })
}

fn is_skill_name(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_alphabetic() && chars.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// `@path` tokens in composer text. Quoted paths and bare paths with `/` or `.`.
pub fn mention_paths(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find('@') {
        rest = &rest[at + 1..];
        if rest.starts_with('"') {
            rest = &rest[1..];
            if let Some(end) = rest.find('"') {
                let path = &rest[..end];
                if !path.is_empty() {
                    out.push(path.to_string());
                }
                rest = &rest[end + 1..];
            }
            continue;
        }
        let end = rest
            .find(|c: char| c.is_whitespace() || c == '@')
            .unwrap_or(rest.len());
        let path = &rest[..end];
        if !path.is_empty() && (path.contains('/') || path.contains('.')) {
            out.push(path.to_string());
        }
        rest = &rest[end..];
    }
    out
}

/// Resolve mention strings against the Project root. Absolute paths stay as-is.
pub fn resolve_mentions(mentions: &[String], project_root: &Path) -> Vec<PathBuf> {
    mentions
        .iter()
        .map(|m| {
            let p = PathBuf::from(m);
            if p.is_absolute() {
                p
            } else {
                project_root.join(m)
            }
        })
        .collect()
}

pub fn user_skills_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".smithy/skills"))
}

fn project_skills_dir(project: &Path) -> PathBuf {
    project.join(".smithy/skills")
}

/// Load `{name}/SKILL.md` from the Project, then the user directory.
pub fn load_skill(project: &Path, name: &str) -> Option<Skill> {
    if !is_skill_name(name) {
        return None;
    }
    let project_file = project_skills_dir(project).join(name).join("SKILL.md");
    if project_file.is_file() {
        return Skill::from_file(&project_file).ok();
    }
    let user_file = user_skills_dir()?.join(name).join("SKILL.md");
    if user_file.is_file() {
        return Skill::from_file(&user_file).ok();
    }
    None
}

/// Skills visible in the `/` picker. Project names override user names.
pub fn list_skills(project: &Path) -> Vec<SkillMeta> {
    let mut by_name: std::collections::BTreeMap<String, SkillMeta> = Default::default();
    if let Some(user) = user_skills_dir() {
        scan_dir(&user, &mut by_name);
    }
    scan_dir(&project_skills_dir(project), &mut by_name);
    by_name.into_values().collect()
}

fn scan_dir(root: &Path, into: &mut std::collections::BTreeMap<String, SkillMeta>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path().join("SKILL.md");
        if let Ok(skill) = Skill::from_file(&path) {
            into.insert(skill.meta.name.clone(), skill.meta);
        }
    }
}

impl Skill {
    pub fn from_file(path: &Path) -> Result<Self, String> {
        let raw = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        let dir = path
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| "SKILL.md has no directory".to_string())?;
        let fallback_name = dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("skill")
            .to_string();
        let (meta, body) = parse_skill_md(&raw, &fallback_name)?;
        let mut skill = Skill { meta, body, dir };
        skill.splice_follow_ons();
        Ok(skill)
    }

    /// Grill-me is three short files; a 27B will skip them if they are only `read`.
    fn splice_follow_ons(&mut self) {
        if self.meta.profile != ToolProfile::Grill {
            return;
        }
        let extra = ["grilling.md", "rust-first.md"]
            .iter()
            .filter_map(|name| std::fs::read_to_string(self.dir.join(name)).ok())
            .collect::<Vec<_>>()
            .join("\n\n");
        if !extra.is_empty() {
            self.body = format!("{}\n\n{extra}", self.body.trim_end());
        }
    }

    pub fn system_prompt(&self, workspace: &Path) -> String {
        let ws = workspace.display();
        match self.meta.profile {
            ToolProfile::Research => format!(
                "{body}\n\n\
                 Workspace root: {ws}\n\n\
                 Write the note to `docs/research/YYYY-MM-DD-<slug>.md` through the `write` tool. \
                 That call waits for Review; do not treat the file as landed until the tool \
                 result says so. You have no `bash`, `edit`, or `explore`. Search, fetch, and \
                 read; then write one note. Sequential — one model, not a swarm.",
                body = self.body.trim()
            ),
            ToolProfile::Grill => format!(
                "{body}\n\n\
                 Workspace root: {ws}\n\n\
                 Facts via `read`, `grep`, `explore`, `ls`, `glob`, `web_fetch`, `web_search`, \
                 `symbol`. You cannot `write`, `edit`, or `bash` in this Session. When the \
                 frontier is empty, wait for the user to confirm a shared understanding. They \
                 will start a coding Session to implement.",
                body = self.body.trim()
            ),
            ToolProfile::Coding => {
                format!("{body}\n\nWorkspace root: {ws}", body = self.body.trim())
            }
        }
    }
}

fn parse_skill_md(raw: &str, fallback_name: &str) -> Result<(SkillMeta, String), String> {
    let raw = raw.trim_start_matches('\u{feff}');
    let Some(rest) = raw.strip_prefix("---") else {
        return Ok((
            SkillMeta {
                name: fallback_name.to_string(),
                description: String::new(),
                argument_hint: String::new(),
                profile: ToolProfile::from_name_hint(fallback_name),
            },
            raw.trim().to_string(),
        ));
    };
    let rest = rest.trim_start_matches(['\n', '\r']);
    let Some((front, body)) = rest.split_once("\n---") else {
        return Err("SKILL.md frontmatter is not closed".into());
    };
    let mut name = fallback_name.to_string();
    let mut description = String::new();
    let mut argument_hint = String::new();
    let mut profile = None;
    for line in front.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = unquote(value.trim());
        match key.trim() {
            "name" => name = value,
            "description" => description = value,
            "argument-hint" => argument_hint = value,
            "profile" => profile = Some(ToolProfile::parse(&value)),
            _ => {}
        }
    }
    let profile = profile.unwrap_or_else(|| ToolProfile::from_name_hint(&name));
    Ok((
        SkillMeta {
            name,
            description,
            argument_hint,
            profile,
        },
        body.trim().to_string(),
    ))
}

impl ToolProfile {
    fn from_name_hint(name: &str) -> Self {
        match name {
            "research" => Self::Research,
            "grill-me" => Self::Grill,
            _ => Self::Coding,
        }
    }
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if (s.starts_with('"') && s.ends_with('"') && s.len() >= 2)
        || (s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2)
    {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_takes_the_name_and_the_rest() {
        let c = parse_command("/research @docs/foo.md what happened to X").unwrap();
        assert_eq!(c.name, "research");
        assert_eq!(c.rest, "@docs/foo.md what happened to X");
    }

    #[test]
    fn a_unix_path_is_not_a_command() {
        assert!(parse_command("/Users/rj/Desktop/foo").is_none());
        assert!(parse_command("/etc/hosts").is_none());
    }

    #[test]
    fn a_line_without_a_slash_is_not_a_command() {
        assert!(parse_command("research this").is_none());
        assert!(parse_command(" /research").is_some());
    }

    #[test]
    fn mentions_pick_up_project_paths_and_quoted_paths() {
        let paths = mention_paths("/research @docs/foo.md and @\"bar baz.md\" skip @ok");
        assert_eq!(paths, vec!["docs/foo.md", "bar baz.md"]);
    }

    #[test]
    fn frontmatter_fills_the_meta() {
        let md = "---\nname: research\ndescription: Deep look\nargument-hint: \"The question\"\nprofile: research\n---\n\n# Hello\n";
        let (meta, body) = parse_skill_md(md, "x").unwrap();
        assert_eq!(meta.name, "research");
        assert_eq!(meta.description, "Deep look");
        assert_eq!(meta.argument_hint, "The question");
        assert_eq!(meta.profile, ToolProfile::Research);
        assert!(body.contains("# Hello"));
    }

    #[test]
    fn a_skill_file_on_disk_loads() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join(".smithy/skills/research");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: research\ndescription: d\n---\n\nDo the work.\n",
        )
        .unwrap();
        let skill = load_skill(dir.path(), "research").expect("loaded");
        assert_eq!(skill.meta.profile, ToolProfile::Research);
        assert!(skill.body.contains("Do the work"));
        assert!(skill
            .system_prompt(Path::new("/proj"))
            .contains("docs/research"));
    }

    #[test]
    fn grill_splices_the_sibling_files() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join(".smithy/skills/grill-me");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: grill-me\nprofile: grill\n---\n\nRun a grilling session.\n",
        )
        .unwrap();
        std::fs::write(skill_dir.join("grilling.md"), "Ask the whole frontier.\n").unwrap();
        std::fs::write(
            skill_dir.join("rust-first.md"),
            "everything stays focused exclusively on rust\n",
        )
        .unwrap();
        let skill = load_skill(dir.path(), "grill-me").expect("loaded");
        assert_eq!(skill.meta.profile, ToolProfile::Grill);
        assert!(skill.body.contains("Ask the whole frontier"));
        assert!(skill.body.contains("exclusively on rust"));
        assert!(skill
            .system_prompt(Path::new("/proj"))
            .contains("cannot `write`"));
    }
}
