//! Files the *user* put into the conversation.
//!
//! ## Why this is not a tool
//!
//! The agent can already read files — that is what `read` is for. This exists
//! because "read this" and "here, look at this" are different acts. A tool call
//! is the model deciding what to look at, costs a round trip, and only happens
//! once the model has guessed which file you meant. Dropping a file on the panel
//! is you answering that question before it is asked.
//!
//! ## Where the content goes, and why not the system prompt
//!
//! Attachments are appended to the **user message**, never to the system prompt.
//! `smithy-agent` is built around an append-only history with a byte-stable
//! system prompt, because the endpoint's prefix cache is a strict prefix match
//! and changing an early token costs a full cold prefill — minutes, at real
//! context sizes. Growing the user turn is free by comparison; rewriting the
//! preamble on every send would make attachments the most expensive feature in
//! the editor.
//!
//! ## The sandbox, and why it does not apply here
//!
//! `smithy-tools` confines the model to the project root through a `cap-std`
//! capability, and a file dropped from your Desktop is outside it. That
//! confinement is not weakened by reading one here, because the two cases are
//! not the same case: the sandbox exists to stop *the model* choosing to read
//! `~/.ssh/id_rsa`, and this path can only ever be walked by you choosing a file
//! with a mouse. The content also arrives as message text rather than as a tool
//! result, so nothing the model emits can cause a read to happen.
//!
//! What that does mean is that the read has to be careful in the ways an
//! ordinary file read is careful — size, encoding, and directories that turn out
//! to hold ten thousand files — which is most of what this module is.

use std::path::{Path, PathBuf};

/// Per-file ceiling. Roughly 64k tokens of text, which is already more than most
/// context windows want to spend on one attachment; past this the file is listed
/// but not inlined, so the model knows it exists and can `read` a slice of it.
pub const MAX_FILE_BYTES: u64 = 256 * 1024;

/// Ceiling across every included attachment on one send.
pub const MAX_TOTAL_BYTES: u64 = 1024 * 1024;

/// How many files a dropped directory may contribute.
///
/// Dropping a project directory is an easy thing to do by accident and an
/// expensive thing to do by mistake. The walk is gitignore-aware, so this is a
/// backstop rather than the main defence.
pub const MAX_DIR_FILES: usize = 50;

/// How much of a file is sniffed to decide whether it is text.
const SNIFF_BYTES: usize = 8192;

/// What a dropped path turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentKind {
    /// Text, small enough to inline.
    Text,
    /// Text, but over [`MAX_FILE_BYTES`]. Listed, not inlined.
    TooLarge,
    /// Contains NUL bytes. Listed, not inlined — pasting a PNG into a prompt as
    /// mojibake costs thousands of tokens and tells the model nothing.
    Binary,
    /// Could not be read at all.
    Unreadable,
}

impl AttachmentKind {
    /// Whether the file's *content* goes into the message.
    ///
    /// Everything else still appears as a named line, because a model told the
    /// file exists can `read` part of it, whereas a model told nothing concludes
    /// it does not exist.
    pub fn inlines(self) -> bool {
        matches!(self, AttachmentKind::Text)
    }

    /// The short reason shown on the chip, for the kinds that need one.
    pub fn note(self) -> Option<&'static str> {
        match self {
            AttachmentKind::Text => None,
            AttachmentKind::TooLarge => Some("too large to inline"),
            AttachmentKind::Binary => Some("binary"),
            AttachmentKind::Unreadable => Some("unreadable"),
        }
    }
}

/// One file the user attached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub path: PathBuf,
    /// How it is named on screen and to the model: relative to the project when
    /// it is inside it, absolute when it is not. The distinction is worth
    /// preserving — a path the model could pass to `read` and one it could not
    /// should not look identical.
    pub display: String,
    pub bytes: u64,
    pub kind: AttachmentKind,
    /// Unchecked chips stay visible but contribute nothing, so pruning a big
    /// context does not mean re-dragging the files you still wanted.
    pub included: bool,
}

impl Attachment {
    /// Roughly `bytes / 4`.
    ///
    /// The same characters-per-token approximation `smithy_project::ContextBudget`
    /// uses, and for the same reason: there is no local tokenizer, the endpoint
    /// reports the real number afterwards, and this only has to be good enough to
    /// warn you before you send.
    pub fn approx_tokens(&self) -> u64 {
        if self.kind.inlines() {
            self.bytes / 4
        } else {
            0
        }
    }

    /// Just the file name, for a chip that has to fit.
    pub fn short_name(&self) -> &str {
        self.path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&self.display)
    }
}

/// Turn dropped paths into attachments.
///
/// Directories are walked; files are stat'd and sniffed. `root` is the project
/// root, used only to decide how each path is named.
///
/// Duplicates are dropped — dragging the same file twice is a slip, not a
/// request for two copies — and so is anything already in `existing`, so a
/// second drop onto a panel that already lists a file is a no-op rather than a
/// way to pay for it twice.
pub fn collect(paths: &[PathBuf], root: &Path, existing: &[Attachment]) -> Vec<Attachment> {
    let mut out: Vec<Attachment> = Vec::new();
    let mut seen: Vec<PathBuf> = existing.iter().map(|a| a.path.clone()).collect();

    for path in paths {
        if path.is_dir() {
            for file in walk_directory(path) {
                push_unique(&mut out, &mut seen, describe(&file, root));
            }
        } else {
            push_unique(&mut out, &mut seen, describe(path, root));
        }
    }
    out
}

fn push_unique(out: &mut Vec<Attachment>, seen: &mut Vec<PathBuf>, attachment: Attachment) {
    if seen.contains(&attachment.path) {
        return;
    }
    seen.push(attachment.path.clone());
    out.push(attachment);
}

/// Files inside a dropped directory, gitignore-aware and capped.
///
/// Uses the same walker ripgrep does — already a dependency, because `grep`
/// stopped shelling out to `rg` — so a dropped project directory skips
/// `target/` and `node_modules/` without a hand-written deny list.
///
/// Two settings differ deliberately from the `grep` and `glob` tools:
///
/// - `require_git(false)`, matching them, so a `.gitignore` is honoured in a
///   directory that is not itself a checkout. Without it the filter silently
///   does nothing outside a repository, which is the surprising half of the
///   `ignore` crate's defaults.
/// - `hidden(true)`, unlike them, so dotfiles are skipped. When the *model*
///   greps for something it should find whatever is there; when *you* drop a
///   project folder you are not asking to paste `.env` into a prompt.
fn walk_directory(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let walker = ignore::WalkBuilder::new(dir)
        .hidden(true)
        .git_ignore(true)
        .require_git(false)
        .max_depth(Some(8))
        .build();

    for entry in walker.flatten() {
        if files.len() >= MAX_DIR_FILES {
            break;
        }
        if entry.file_type().is_some_and(|t| t.is_file()) {
            files.push(entry.into_path());
        }
    }
    files.sort();
    files
}

/// Stat and sniff one file.
pub fn describe(path: &Path, root: &Path) -> Attachment {
    let display = display_path(path, root);
    let Ok(meta) = std::fs::metadata(path) else {
        return Attachment {
            path: path.to_path_buf(),
            display,
            bytes: 0,
            kind: AttachmentKind::Unreadable,
            included: true,
        };
    };

    let bytes = meta.len();
    let kind = if bytes > MAX_FILE_BYTES {
        AttachmentKind::TooLarge
    } else if looks_binary(path) {
        AttachmentKind::Binary
    } else {
        AttachmentKind::Text
    };

    Attachment {
        path: path.to_path_buf(),
        display,
        bytes,
        kind,
        included: true,
    }
}

/// Name a path relative to the project when it lives there.
pub fn display_path(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .map(|rel| rel.to_string_lossy().to_string())
        .unwrap_or_else(|_| path.to_string_lossy().to_string())
}

/// A NUL byte in the first few kilobytes, which is the test git uses.
///
/// Deliberately not an extension list: extensions lie in both directions, and a
/// `.log` full of protobuf is exactly the file you least want inlined.
fn looks_binary(path: &Path) -> bool {
    use std::io::Read as _;
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut buffer = [0u8; SNIFF_BYTES];
    match file.read(&mut buffer) {
        Ok(n) => buffer[..n].contains(&0),
        Err(_) => false,
    }
}

/// The total the chips report, counting only what will actually be sent.
pub fn total_tokens(attachments: &[Attachment]) -> u64 {
    attachments
        .iter()
        .filter(|a| a.included)
        .map(|a| a.approx_tokens())
        .sum()
}

/// Whether the included attachments exceed [`MAX_TOTAL_BYTES`].
pub fn over_budget(attachments: &[Attachment]) -> bool {
    let total: u64 = attachments
        .iter()
        .filter(|a| a.included && a.kind.inlines())
        .map(|a| a.bytes)
        .sum();
    total > MAX_TOTAL_BYTES
}

/// Build the block that goes in front of the user's message.
///
/// `read` is injected so the shape of the output can be tested without a
/// filesystem, and so the caller decides how a mid-flight read failure is
/// reported rather than this deciding for it.
///
/// Returns `task` unchanged when nothing is attached — an empty preamble would
/// still be a change to the message, and a user who attached nothing should send
/// exactly what they typed.
pub fn materialize(
    attachments: &[Attachment],
    task: &str,
    read: impl Fn(&Path) -> Result<String, String>,
) -> String {
    let included: Vec<&Attachment> = attachments.iter().filter(|a| a.included).collect();
    if included.is_empty() {
        return task.to_string();
    }

    let mut out = String::new();
    // Named as coming from the user, for the same reason `prepend_review_outcomes`
    // brackets its additions: the model has to be able to tell what you asked for
    // apart from what the IDE is telling it.
    out.push_str("[Files attached by the user]\n\n");

    for attachment in &included {
        if !attachment.kind.inlines() {
            let note = attachment.kind.note().unwrap_or("not shown");
            out.push_str(&format!(
                "- `{}` ({note}, {}). Use `read` if you need its contents.\n",
                attachment.display,
                human_size(attachment.bytes)
            ));
            continue;
        }
        match read(&attachment.path) {
            Ok(content) => {
                out.push_str(&format!(
                    "--- {} ---\n{}\n{}\n{}\n\n",
                    attachment.display,
                    fence(&attachment.display),
                    content.trim_end(),
                    "```"
                ));
            }
            Err(e) => {
                out.push_str(&format!("- `{}` could not be read: {e}\n", attachment.display));
            }
        }
    }

    out.push_str("[End of attached files]\n\n");
    out.push_str(task);
    out
}

/// The opening fence, tagged by extension so the model gets the language for
/// free rather than inferring it from the contents.
fn fence(display: &str) -> String {
    let tag = Path::new(display)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    format!("```{tag}")
}

/// A size a person can read at a glance.
pub fn human_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.0} kB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn write(dir: &Path, name: &str, contents: &[u8]) -> PathBuf {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(contents).unwrap();
        path
    }

    #[test]
    fn a_file_inside_the_project_is_named_relative_to_it() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write(tmp.path(), "src/main.rs", b"fn main() {}");
        let a = describe(&path, tmp.path());
        assert_eq!(a.display, "src/main.rs");
        assert_eq!(a.kind, AttachmentKind::Text);
    }

    /// A file dragged from outside the project keeps its absolute path, because
    /// a relative-looking name the model cannot pass to `read` is a trap.
    #[test]
    fn a_file_outside_the_project_keeps_its_absolute_path() {
        let project = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let path = write(elsewhere.path(), "notes.md", b"hello");
        let a = describe(&path, project.path());
        assert!(a.display.starts_with('/'), "{}", a.display);
    }

    #[test]
    fn a_file_with_nul_bytes_is_binary_whatever_its_extension() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write(tmp.path(), "data.txt", b"before\0after");
        assert_eq!(describe(&path, tmp.path()).kind, AttachmentKind::Binary);
    }

    #[test]
    fn a_file_over_the_ceiling_is_listed_rather_than_inlined() {
        let tmp = tempfile::tempdir().unwrap();
        let big = vec![b'x'; (MAX_FILE_BYTES + 1) as usize];
        let path = write(tmp.path(), "huge.txt", &big);
        let a = describe(&path, tmp.path());
        assert_eq!(a.kind, AttachmentKind::TooLarge);
        assert!(!a.kind.inlines());
        assert_eq!(a.approx_tokens(), 0, "a file we do not send costs nothing");
    }

    #[test]
    fn a_missing_file_is_unreadable_rather_than_a_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let a = describe(&tmp.path().join("nope.rs"), tmp.path());
        assert_eq!(a.kind, AttachmentKind::Unreadable);
    }

    #[test]
    fn a_dropped_directory_expands_to_its_files() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "a.rs", b"fn a() {}");
        write(tmp.path(), "nested/b.rs", b"fn b() {}");
        let found = collect(&[tmp.path().to_path_buf()], tmp.path(), &[]);
        let names: Vec<&str> = found.iter().map(|a| a.short_name()).collect();
        assert!(names.contains(&"a.rs"), "{names:?}");
        assert!(names.contains(&"b.rs"), "{names:?}");
    }

    /// The walk is gitignore-aware, which is the difference between dropping a
    /// Rust project and dropping its `target/` directory too.
    #[test]
    fn a_dropped_directory_respects_gitignore() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), ".gitignore", b"ignored/\n");
        write(tmp.path(), "kept.rs", b"fn kept() {}");
        write(tmp.path(), "ignored/skipped.rs", b"fn skipped() {}");
        let found = collect(&[tmp.path().to_path_buf()], tmp.path(), &[]);
        let names: Vec<&str> = found.iter().map(|a| a.short_name()).collect();
        assert!(names.contains(&"kept.rs"), "{names:?}");
        assert!(!names.contains(&"skipped.rs"), "{names:?}");
    }

    /// Dropping a project folder must not sweep `.env` into the prompt. A file
    /// can still be attached deliberately by dropping it on its own — this only
    /// governs what a *directory* contributes.
    #[test]
    fn a_dropped_directory_skips_dotfiles() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "main.rs", b"fn main() {}");
        write(tmp.path(), ".env", b"OPENROUTER_API_KEY=sk-secret");
        let found = collect(&[tmp.path().to_path_buf()], tmp.path(), &[]);
        let names: Vec<&str> = found.iter().map(|a| a.short_name()).collect();
        assert!(names.contains(&"main.rs"), "{names:?}");
        assert!(!names.contains(&".env"), "{names:?}");
    }

    /// ...and dropping it directly still works, because that is you asking.
    #[test]
    fn a_dotfile_dropped_on_its_own_is_attached() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write(tmp.path(), ".env", b"KEY=value");
        let found = collect(&[path], tmp.path(), &[]);
        assert_eq!(found.len(), 1, "{found:?}");
    }

    #[test]
    fn dropping_the_same_file_twice_attaches_it_once() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write(tmp.path(), "a.rs", b"fn a() {}");
        let found = collect(&[path.clone(), path.clone()], tmp.path(), &[]);
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn a_file_already_attached_is_not_attached_again() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write(tmp.path(), "a.rs", b"fn a() {}");
        let first = collect(std::slice::from_ref(&path), tmp.path(), &[]);
        let second = collect(&[path], tmp.path(), &first);
        assert!(second.is_empty(), "{second:?}");
    }

    #[test]
    fn nothing_attached_leaves_the_message_byte_identical() {
        let out = materialize(&[], "explain this", |_| Ok(String::new()));
        assert_eq!(out, "explain this");
    }

    /// An attachment unchecked in the UI must not reach the model, or the
    /// checkbox is decorative.
    #[test]
    fn an_excluded_attachment_contributes_nothing() {
        let a = Attachment {
            path: "/tmp/a.rs".into(),
            display: "a.rs".into(),
            bytes: 100,
            kind: AttachmentKind::Text,
            included: false,
        };
        let out = materialize(&[a], "hi", |_| Ok("SECRET".into()));
        assert_eq!(out, "hi");
    }

    #[test]
    fn an_inlined_file_carries_its_path_and_a_language_fence() {
        let a = Attachment {
            path: "/p/src/main.rs".into(),
            display: "src/main.rs".into(),
            bytes: 12,
            kind: AttachmentKind::Text,
            included: true,
        };
        let out = materialize(&[a], "explain", |_| Ok("fn main() {}".into()));
        assert!(out.contains("--- src/main.rs ---"), "{out}");
        assert!(out.contains("```rs"), "{out}");
        assert!(out.contains("fn main() {}"), "{out}");
        assert!(out.ends_with("explain"), "the task stays last: {out}");
    }

    /// A binary is named but not inlined, and the model is told what to do
    /// about it — silence would teach it the file does not exist.
    #[test]
    fn a_binary_is_named_but_not_inlined() {
        let a = Attachment {
            path: "/p/logo.png".into(),
            display: "logo.png".into(),
            bytes: 2048,
            kind: AttachmentKind::Binary,
            included: true,
        };
        let out = materialize(&[a], "what is this", |_| {
            panic!("a binary must never be read")
        });
        assert!(out.contains("logo.png"), "{out}");
        assert!(out.contains("binary"), "{out}");
        assert!(out.contains("read"), "{out}");
    }

    #[test]
    fn a_read_failure_is_reported_rather_than_swallowed() {
        let a = Attachment {
            path: "/p/gone.rs".into(),
            display: "gone.rs".into(),
            bytes: 10,
            kind: AttachmentKind::Text,
            included: true,
        };
        let out = materialize(&[a], "go", |_| Err("permission denied".into()));
        assert!(out.contains("permission denied"), "{out}");
        assert!(out.ends_with("go"), "{out}");
    }

    #[test]
    fn the_token_estimate_counts_only_included_text() {
        let text = |included| Attachment {
            path: "/p/a.rs".into(),
            display: "a.rs".into(),
            bytes: 4000,
            kind: AttachmentKind::Text,
            included,
        };
        assert_eq!(total_tokens(&[text(true)]), 1000);
        assert_eq!(total_tokens(&[text(false)]), 0);
    }

    #[test]
    fn the_total_ceiling_notices_a_pile_of_medium_files() {
        let one = Attachment {
            path: "/p/a.rs".into(),
            display: "a.rs".into(),
            bytes: MAX_FILE_BYTES,
            kind: AttachmentKind::Text,
            included: true,
        };
        let five = vec![one.clone(), one.clone(), one.clone(), one.clone(), one];
        assert!(over_budget(&five));
        assert!(!over_budget(&five[..2]));
    }

    #[test]
    fn sizes_read_the_way_a_person_would_say_them() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2 kB");
        assert_eq!(human_size(3 * 1024 * 1024), "3.0 MB");
    }
}
