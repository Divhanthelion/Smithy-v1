//! Skills and Commands.
//!
//! A **Skill** is a `SKILL.md` on disk: a context-injection macro. A **Command**
//! is the user typing `/name` in the composer. Skills run only then — never
//! from ambient text. There is no Session kind per skill. A Command injects the
//! skill body into the current user turn; it does not rebuild the Session and
//! does not compress history. Tools stay whatever this Session was born with.
//!
//! Lookup: Project `.smithy/skills/<name>/SKILL.md`, then
//! `~/.smithy/skills/<name>/SKILL.md`, then the `research` and `grill-me`
//! procedures Smithy ships. Compact and Handoff are harness Commands, not
//! Skills; they are listed in the `/` picker anyway.

use std::path::{Path, PathBuf};

/// Picker row: enough to show `/name` without loading the body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillMeta {
    pub name: String,
    pub description: String,
    pub argument_hint: String,
    /// Allowlist of tool names. `None` means the coding defaults. Applied only
    /// if a Session is rebuilt with this Skill; a Command does not rebuild.
    pub tools: Option<Vec<String>>,
    /// Sibling files concatenated into the body, relative to the skill dir.
    pub include: Vec<String>,
    /// Wall-clock override, in seconds. Same as `tools`: rebuild only.
    pub max_seconds: Option<u64>,
}

/// A loaded Skill: frontmatter plus the procedure injected into a user turn.
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

/// Load `{name}/SKILL.md` from the Project, then the user directory, then
/// procedures Smithy ships (`research`, `grill-me`).
pub fn load_skill(project: &Path, name: &str) -> Option<Skill> {
    if !is_skill_name(name) {
        return None;
    }
    let project_file = project_skills_dir(project).join(name).join("SKILL.md");
    if project_file.is_file() {
        return Skill::from_file(&project_file).ok();
    }
    if let Some(user_file) = user_skills_dir().map(|d| d.join(name).join("SKILL.md")) {
        if user_file.is_file() {
            return Skill::from_file(&user_file).ok();
        }
    }
    bundled_skill(name)
}

/// Harness Commands that are not Skills. Shown in the `/` picker.
pub fn harness_commands() -> Vec<SkillMeta> {
    vec![
        SkillMeta {
            name: "compact".into(),
            description: "Replace this Session's History with a lossy summary. Same conversation."
                .into(),
            argument_hint: "Optional focus for the summary".into(),
            tools: None,
            include: Vec::new(),
            max_seconds: None,
        },
        SkillMeta {
            name: "handoff".into(),
            description: "Write a Project-owned note for a later Session. History stays.".into(),
            argument_hint: "What the next Session is for".into(),
            tools: None,
            include: Vec::new(),
            max_seconds: None,
        },
    ]
}

pub fn is_harness_command(name: &str) -> bool {
    matches!(name, "compact" | "handoff")
}

/// Body injected for `/handoff`. A Project `handoff` Skill replaces this.
pub fn handoff_injection(rest: &str) -> String {
    let purpose = if rest.trim().is_empty() {
        "Continue the work in this Project.".to_string()
    } else {
        rest.trim().to_string()
    };
    format!(
        "# Handoff\n\n\
Write a handoff the next Session can run from. This Session's History stays as it is — \
do not summarize in-context to free tokens; that is `/compact`.\n\n\
If they passed arguments, that is what the next Session is for: {purpose}\n\n\
Update the Project's existing `HANDOFF.md` or `docs/HANDOFF.md` if either exists. \
Otherwise `write` `HANDOFF.md` at the Project root. Review-gated — do not treat it as \
landed until the tool result says so. Never write this to a temp directory. The Project owns memory.\n\n\
```markdown\n\
# Handoff\n\
\n\
**Next session:** {purpose}\n\
\n\
## Already right\n\
Decisions that look like bugs. Do not \"fix\" these.\n\
\n\
## State\n\
What is true now. Pointers to files, branches, commits — not pasted diffs.\n\
\n\
## Open\n\
Unresolved decisions. Questions, not tasks, if they are still decisions.\n\
\n\
## Next\n\
Ordered next moves for the stated session purpose.\n\
```\n\n\
Do not duplicate specs, plans, or research notes — reference them by path. Redact secrets. \
Do not start the next Session's work here unless they ask."
    )
}

/// Skills visible in the `/` picker. Project names override user names;
/// both override the procedures Smithy ships.
pub fn list_skills(project: &Path) -> Vec<SkillMeta> {
    let mut by_name: std::collections::BTreeMap<String, SkillMeta> = Default::default();
    for skill in bundled_skills() {
        by_name.insert(skill.meta.name.clone(), skill.meta);
    }
    if let Some(user) = user_skills_dir() {
        scan_dir(&user, &mut by_name);
    }
    scan_dir(&project_skills_dir(project), &mut by_name);
    by_name.into_values().collect()
}

/// Write shipped Skills into `~/.smithy/skills/` when `SKILL.md` is missing.
///
/// Empty stub directories (a name with no file) used to hide `/research` and
/// `/grill-me` in every Project that did not ship its own copy. Does not
/// overwrite a file the user already has.
pub fn install_bundled_user_skills() {
    let Some(root) = user_skills_dir() else {
        return;
    };
    for (name, files) in BUNDLED {
        let dir = root.join(name);
        if dir.join("SKILL.md").is_file() {
            continue;
        }
        let _ = std::fs::create_dir_all(&dir);
        for (filename, contents) in *files {
            let path = dir.join(filename);
            if !path.is_file() {
                let _ = std::fs::write(path, contents);
            }
        }
    }
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

/// Procedures Smithy ships. Paths are the repo copies; `include_str!` embeds
/// them so `/research` and `/grill-me` work in a Project that has neither.
const BUNDLED: &[(&str, &[(&str, &str)])] = &[
    (
        "research",
        &[
            (
                "SKILL.md",
                include_str!("../../../.smithy/skills/research/SKILL.md"),
            ),
            (
                "snowball.md",
                include_str!("../../../.smithy/skills/research/snowball.md"),
            ),
            (
                "sift.md",
                include_str!("../../../.smithy/skills/research/sift.md"),
            ),
            (
                "ach.md",
                include_str!("../../../.smithy/skills/research/ach.md"),
            ),
        ],
    ),
    (
        "grill-me",
        &[
            (
                "SKILL.md",
                include_str!("../../../.smithy/skills/grill-me/SKILL.md"),
            ),
            (
                "grilling.md",
                include_str!("../../../.smithy/skills/grill-me/grilling.md"),
            ),
            (
                "rust-first.md",
                include_str!("../../../.smithy/skills/grill-me/rust-first.md"),
            ),
        ],
    ),
];

fn bundled_skills() -> Vec<Skill> {
    BUNDLED
        .iter()
        .filter_map(|(name, _)| bundled_skill(name))
        .collect()
}

fn bundled_skill(name: &str) -> Option<Skill> {
    let files = BUNDLED.iter().find(|(n, _)| *n == name)?.1;
    let raw = files.iter().find(|(file, _)| *file == "SKILL.md")?.1;
    let (meta, mut body) = parse_skill_md(raw, name).ok()?;
    let extra = meta
        .include
        .iter()
        .filter_map(|inc| {
            let text = files.iter().find(|(file, _)| *file == inc)?.1;
            Some(format!("# {inc}\n\n{}", text.trim()))
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    if !extra.is_empty() {
        body = format!("{}\n\n{extra}", body.trim_end());
    }
    Some(Skill {
        meta,
        body,
        dir: PathBuf::from(format!("bundled:{name}")),
    })
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
        skill.splice_includes();
        Ok(skill)
    }

    fn splice_includes(&mut self) {
        if self.meta.include.is_empty() {
            return;
        }
        let extra = self
            .meta
            .include
            .iter()
            .filter_map(|name| {
                let text = std::fs::read_to_string(self.dir.join(name)).ok()?;
                Some(format!("# {name}\n\n{}", text.trim()))
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        if !extra.is_empty() {
            self.body = format!("{}\n\n{extra}", self.body.trim_end());
        }
    }

    /// Body prefixed onto the current user message when `/name` is typed.
    pub fn injection(&self) -> String {
        format!("# Skill `{}`\n\n{}", self.meta.name, self.body.trim())
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
                tools: None,
                include: Vec::new(),
                max_seconds: None,
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
    let mut tools = None;
    let mut include = Vec::new();
    let mut max_seconds = None;
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
            "tools" => tools = parse_tools_field(&value),
            "include" => include = parse_list(&value),
            "max-seconds" => max_seconds = value.parse().ok(),
            _ => {}
        }
    }
    Ok((
        SkillMeta {
            name,
            description,
            argument_hint,
            tools,
            include,
            max_seconds,
        },
        body.trim().to_string(),
    ))
}

/// `None` if the key is present but empty (treat as omitted). `Some([])` for `[]`.
fn parse_tools_field(value: &str) -> Option<Vec<String>> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(parse_list(trimmed))
}

fn parse_list(value: &str) -> Vec<String> {
    let v = value
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim();
    if v.is_empty() {
        return Vec::new();
    }
    v.split(',')
        .map(|s| unquote(s.trim()))
        .filter(|s| !s.is_empty())
        .collect()
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
        let md = "---\nname: research\ndescription: Deep look\nargument-hint: \"The question\"\ntools: [read, write, web_search]\nmax-seconds: 7200\n---\n\n# Hello\n";
        let (meta, body) = parse_skill_md(md, "x").unwrap();
        assert_eq!(meta.name, "research");
        assert_eq!(meta.description, "Deep look");
        assert_eq!(meta.argument_hint, "The question");
        assert_eq!(
            meta.tools,
            Some(vec!["read".into(), "write".into(), "web_search".into()])
        );
        assert_eq!(meta.max_seconds, Some(7200));
        assert!(body.contains("# Hello"));
    }

    #[test]
    fn omitted_tools_means_coding_defaults() {
        let md = "---\nname: notes\n---\n\nTake notes.\n";
        let (meta, _) = parse_skill_md(md, "notes").unwrap();
        assert_eq!(meta.tools, None);
        assert!(meta.include.is_empty());
        assert_eq!(meta.max_seconds, None);
    }

    #[test]
    fn empty_tools_list_is_none_of_the_core_set() {
        let md = "---\nname: silent\ntools: []\n---\n\nJust talk.\n";
        let (meta, _) = parse_skill_md(md, "silent").unwrap();
        assert_eq!(meta.tools, Some(Vec::new()));
    }

    #[test]
    fn a_skill_file_on_disk_loads() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join(".smithy/skills/research");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: research\ndescription: d\ntools: read, write\n---\n\nDo the work.\n",
        )
        .unwrap();
        let skill = load_skill(dir.path(), "research").expect("loaded");
        assert_eq!(skill.meta.tools, Some(vec!["read".into(), "write".into()]));
        assert!(skill.body.contains("Do the work"));
        assert!(skill.injection().starts_with("# Skill `research`"));
    }

    #[test]
    fn include_splices_sibling_files() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join(".smithy/skills/grill-me");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: grill-me\ninclude: grilling.md, rust-first.md\ntools: read, explore\n---\n\nRun a grilling session.\n",
        )
        .unwrap();
        std::fs::write(skill_dir.join("grilling.md"), "Ask the whole frontier.\n").unwrap();
        std::fs::write(
            skill_dir.join("rust-first.md"),
            "everything stays focused exclusively on rust\n",
        )
        .unwrap();
        let skill = load_skill(dir.path(), "grill-me").expect("loaded");
        assert!(skill.body.contains("Ask the whole frontier"));
        assert!(skill.body.contains("exclusively on rust"));
        assert!(skill.injection().contains("# Skill `grill-me`"));
    }

    #[test]
    fn a_third_skill_is_just_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join(".smithy/skills/domain-modeling");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: domain-modeling\ndescription: Sharpen the glossary.\n---\n\nUpdate CONTEXT.md.\n",
        )
        .unwrap();
        let skill = load_skill(dir.path(), "domain-modeling").expect("loaded");
        assert_eq!(skill.meta.tools, None);
        assert!(skill.injection().contains("Update CONTEXT.md"));
    }

    #[test]
    fn this_project_ships_research_and_grill_me() {
        let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let root = manifest.parent().unwrap().parent().unwrap();
        let names: Vec<String> = list_skills(root).into_iter().map(|m| m.name).collect();
        assert!(names.iter().any(|n| n == "research"), "{names:?}");
        assert!(names.iter().any(|n| n == "grill-me"), "{names:?}");
    }

    #[test]
    fn research_and_grill_me_load_in_a_project_with_no_skills() {
        let dir = tempfile::tempdir().unwrap();
        let research = load_skill(dir.path(), "research").expect("bundled research");
        assert_eq!(research.meta.name, "research");
        assert!(
            research.body.contains("Pinned question"),
            "{}",
            research.body
        );
        let grill = load_skill(dir.path(), "grill-me").expect("bundled grill-me");
        assert_eq!(grill.meta.name, "grill-me");
        assert!(grill.body.contains("Ask the whole frontier") || grill.body.contains("frontier"));
        let names: Vec<String> = list_skills(dir.path())
            .into_iter()
            .map(|m| m.name)
            .collect();
        assert!(names.iter().any(|n| n == "research"), "{names:?}");
        assert!(names.iter().any(|n| n == "grill-me"), "{names:?}");
    }

    #[test]
    fn a_project_skill_still_overrides_the_shipped_one() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join(".smithy/skills/research");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: research\n---\n\nProject copy.\n",
        )
        .unwrap();
        let skill = load_skill(dir.path(), "research").expect("project wins");
        assert!(skill.body.contains("Project copy"));
        assert!(!skill.body.contains("Pinned question"));
    }
}
