//! Command line. Kept tiny so the rest of the crate is the Session, not a parser.

use std::path::PathBuf;

pub struct Args {
    pub project: PathBuf,
    pub yolo: bool,
    pub message: Option<String>,
    pub init_harness: bool,
    pub which_harness: bool,
}

pub fn usage() -> &'static str {
    "\
smithy-agent — a Session in the terminal. Same loop as the editor.

  smithy-agent [PROJECT]         REPL in this Project (cwd if omitted)
  smithy-agent -m TEXT [PROJECT] one Turn, then exit
  smithy-agent --yolo …          skip Review for in-Project writes; bash that
                                 stays down in the tree skips the prompt
  smithy-agent --init-harness    copy the shipped system prompt into
                                 .smithy/harness/SYSTEM.md so you can edit it
  smithy-agent --which-harness   print which SYSTEM.md this Project would load

In the REPL: /help  /inspect  /prompt  /request  /skills
             /new  /yolo  /reviewed  /compact  /handoff  /quit
A Skill is /name. /inspect shows what this Session will send (chars, not a
guess). A file in the harness directory is not sent unless harness.toml lists
it. Editing the Harness mid-Session does not hot-reload — /new to pick it up.
"
}

pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut project = None;
    let mut yolo = false;
    let mut message = None;
    let mut init_harness = false;
    let mut which_harness = false;
    let mut rest = args.into_iter().peekable();

    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "-h" | "--help" => return Err(usage().trim_end().to_string()),
            "--yolo" => yolo = true,
            "--init-harness" => init_harness = true,
            "--which-harness" => which_harness = true,
            "-m" | "--message" => {
                message = Some(
                    rest.next()
                        .ok_or_else(|| " -m needs a message".to_string())?,
                );
            }
            flag if flag.starts_with('-') => {
                return Err(format!("unknown flag {flag}\n\n{}", usage().trim_end()));
            }
            path => {
                if project.is_some() {
                    return Err(format!(
                        "unexpected extra argument {path}\n\n{}",
                        usage().trim_end()
                    ));
                }
                project = Some(PathBuf::from(path));
            }
        }
    }

    Ok(Args {
        project: project.unwrap_or_else(|| PathBuf::from(".")),
        yolo,
        message,
        init_harness,
        which_harness,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn cwd_is_the_default_project() {
        let a = parse(args("")).unwrap();
        assert_eq!(a.project, PathBuf::from("."));
        assert!(!a.yolo);
        assert!(a.message.is_none());
    }

    #[test]
    fn yolo_and_message_and_path() {
        let a = parse(args("--yolo -m fix the-crate")).unwrap();
        assert!(a.yolo);
        assert_eq!(a.message.as_deref(), Some("fix"));
        assert_eq!(a.project, PathBuf::from("the-crate"));
    }

    #[test]
    fn unknown_flag_is_an_error() {
        assert!(parse(args("--nope")).is_err());
    }
}
