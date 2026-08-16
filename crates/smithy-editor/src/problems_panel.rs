//! The Problems panel — where LSP diagnostics become visible.
//!
//! Until this existed, the LSP stack was complete but invisible: the server
//! spawned, `textDocument/didOpen` fired, diagnostics arrived over the channel
//! and landed in a signal that nothing read. Every piece worked and the feature
//! did not exist.
//!
//! Diagnostics are keyed by file and re-published wholesale by the server, so a
//! file's entry is always *replaced*, never merged — a server that has fixed
//! everything sends an empty list, and merging would leave the stale errors on
//! screen forever.

use floem::peniko::Color;
use floem::prelude::*;
use floem::reactive::{RwSignal, SignalGet, SignalUpdate};
use floem::style::CustomStylable;

use crate::design;
use crate::lsp::{LspDiagnostic, Severity};

/// One diagnostic, flattened for display.
#[derive(Debug, Clone, PartialEq)]
pub struct ProblemRow {
    /// Path relative to the project root, for display.
    pub file: String,
    /// 1-based, as humans and editors count.
    pub line: u32,
    pub column: u32,
    pub severity: Severity,
    pub message: String,
    /// e.g. `E0308`, or a clippy lint name.
    pub code: Option<String>,
    pub source: Option<String>,
}

impl ProblemRow {
    /// How a diagnostic's file should be named in the panel: relative to the
    /// project when it is inside it, absolute when it is not.
    ///
    /// Its own function because getting it wrong is silent and was. The caller
    /// used to hold a project root captured once at startup, so after a project
    /// switch `strip_prefix` failed and every row showed an absolute path — the
    /// visible half of the bug. The invisible half was that the *same* stale root
    /// was used to decide which editor the inline underlines belonged to, with a
    /// `unwrap_or_default()` that turned the failure into an empty string, so the
    /// comparison could never match and no diagnostic was ever underlined.
    pub fn label_for(path: &std::path::Path, project_root: &std::path::Path) -> String {
        path.strip_prefix(project_root)
            .unwrap_or(path)
            .display()
            .to_string()
    }

    pub fn from_diagnostic(file: &str, diagnostic: &LspDiagnostic) -> Self {
        Self {
            file: file.to_string(),
            // LSP positions are 0-based; everything a user sees is 1-based.
            line: diagnostic.range.start.line + 1,
            column: diagnostic.range.start.column + 1,
            severity: diagnostic.severity,
            message: diagnostic
                .message
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_string(),
            code: diagnostic.code.clone(),
            source: diagnostic.source.clone(),
        }
    }

    pub fn location(&self) -> String {
        format!("{}:{}:{}", self.file, self.line, self.column)
    }
}

/// Whether two paths name the same file.
///
/// Plain equality first, because that is the case every time and costs nothing.
/// The `canonicalize` fallback is only reached when the strings differ, and it
/// exists because a language server is free to normalise the URI it echoes back —
/// a project reached through a symlink comes home resolved, and the diagnostic
/// then belongs to a file the editor would swear it did not have open.
///
/// This comparison has now been wrong twice, in two different ways. It is a
/// function so the next reader can see what it is defending against.
pub fn is_same_file(a: &std::path::Path, b: &std::path::Path) -> bool {
    if a == b {
        return true;
    }
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// What to say when the list is showing nothing.
///
/// The case that matters: **an empty panel used to read "No problems reported."
/// whether the language server had analysed the project and found nothing, or had
/// never started at all.** Those are opposite facts and they looked identical, so
/// a dead server presented as a clean bill of health.
///
/// A recorded failure still wins over both, because it says what to do about it.
pub fn empty_state_text(total: usize, status: Option<String>, servers: usize) -> String {
    match (total, status, servers) {
        // An unusable server is the most likely reason for silence, and the least
        // obvious.
        (_, Some(status), _) => status,
        (0, None, 0) => "No language server is running, so nothing has been checked.".to_string(),
        (0, None, _) => "No problems reported.".to_string(),
        (n, None, _) => format!("{n} below the current filter."),
    }
}

pub fn severity_rank(severity: Severity) -> u8 {
    match severity {
        Severity::Error => 3,
        Severity::Warning => 2,
        Severity::Information => 1,
        Severity::Hint => 0,
    }
}

pub fn severity_label(severity: Severity) -> &'static str {
    match severity {
        Severity::Error => "error",
        Severity::Warning => "warn",
        Severity::Information => "info",
        Severity::Hint => "hint",
    }
}

pub fn severity_color(severity: Severity) -> Color {
    match severity {
        Severity::Error => design::DANGER,
        Severity::Warning => design::WARN,
        Severity::Information => design::INFO,
        Severity::Hint => design::FG_FAINT,
    }
}

/// Diagnostics for the whole workspace, keyed by file.
#[derive(Clone, Copy)]
pub struct DiagnosticsState {
    /// `(file, diagnostics)` pairs. A `Vec` rather than a map so ordering is
    /// stable and the panel does not reshuffle on every publish.
    pub by_file: RwSignal<Vec<(String, Vec<ProblemRow>)>>,
    /// Whether the language server is usable, and why not if it isn't.
    pub server_status: RwSignal<Option<String>>,
    /// How many language servers are analysing this workspace.
    ///
    /// Zero and "no problems" look identical in a list, and the difference is the
    /// whole question — one means your code is clean, the other means nothing
    /// looked at it.
    pub server_count: RwSignal<usize>,
    pub min_severity: RwSignal<Severity>,
}

impl DiagnosticsState {
    pub fn new() -> Self {
        Self {
            by_file: RwSignal::new(Vec::new()),
            server_status: RwSignal::new(None),
            server_count: RwSignal::new(0),
            // Warnings and above by default: clippy's hint tier is voluminous
            // and rarely what you opened the panel for.
            min_severity: RwSignal::new(Severity::Warning),
        }
    }

    /// Replace a file's diagnostics.
    ///
    /// Replacement, not merge: the server republishes the full set for a file on
    /// every change, and an empty list is how it says "all clear".
    pub fn publish(&self, file: impl Into<String>, rows: Vec<ProblemRow>) {
        let file = file.into();
        self.by_file.update(|entries| {
            entries.retain(|(f, _)| f != &file);
            if !rows.is_empty() {
                entries.push((file, rows));
                entries.sort_by_key(|(path, _)| path.clone());
            }
        });
    }

    pub fn clear(&self) {
        self.by_file.update(|e| e.clear());
    }

    /// Every row at or above the current filter, worst first.
    pub fn visible(&self) -> Vec<ProblemRow> {
        let min = severity_rank(self.min_severity.get());
        let mut rows: Vec<ProblemRow> = self
            .by_file
            .get()
            .into_iter()
            .flat_map(|(_, rows)| rows)
            .filter(|r| severity_rank(r.severity) >= min)
            .collect();
        rows.sort_by(|a, b| {
            severity_rank(b.severity)
                .cmp(&severity_rank(a.severity))
                .then(a.file.cmp(&b.file))
                .then(a.line.cmp(&b.line))
        });
        rows
    }

    pub fn count(&self, severity: Severity) -> usize {
        self.by_file
            .get()
            .iter()
            .flat_map(|(_, rows)| rows)
            .filter(|r| r.severity == severity)
            .count()
    }

    pub fn total(&self) -> usize {
        self.by_file.get().iter().map(|(_, rows)| rows.len()).sum()
    }

    pub fn hidden_count(&self) -> usize {
        self.total() - self.visible().len()
    }
}

impl Default for DiagnosticsState {
    fn default() -> Self {
        Self::new()
    }
}

/// A one-line summary for a status bar: `3 errors · 12 warnings`.
pub fn summary_line(state: &DiagnosticsState) -> String {
    let errors = state.count(Severity::Error);
    let warnings = state.count(Severity::Warning);
    match (errors, warnings) {
        (0, 0) => "no problems".to_string(),
        (e, 0) => format!("{e} error{}", plural(e)),
        (0, w) => format!("{w} warning{}", plural(w)),
        (e, w) => format!("{e} error{} · {w} warning{}", plural(e), plural(w)),
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

// ============================================================================
// View
// ============================================================================

pub fn problems_panel(
    state: DiagnosticsState,
    on_open: std::rc::Rc<dyn Fn(String, u32, u32)>,
    on_close: impl Fn() + 'static,
) -> impl IntoView {
    Stack::vertical((
        header(state, on_close),
        floem::views::scroll::Scroll::new(rows(state, on_open))
            .custom_style(|s: floem::views::scroll::ScrollCustomStyle| {
                s.hide_bars(false)
                    .handle_background(design::BORDER)
                    .handle_border_radius(4.0)
            })
            .style(|s| {
                s.flex_grow(1.0)
                    .flex_basis(0.0)
                    .width_full()
                    .min_height(0.0)
            }),
    ))
    .style(|s| {
        s.width_full()
            .height_full()
            .background(design::BG_SUNKEN)
            .border_top(1.0)
            .border_color(design::BORDER_SUBTLE)
    })
}

fn header(state: DiagnosticsState, on_close: impl Fn() + 'static) -> impl IntoView {
    Stack::horizontal((
        Label::derived(|| "Problems".to_string()).style(|s| {
            s.color(design::FG)
                .font_size(design::TEXT_BASE)
                .font_bold()
                .margin_right(design::SPACE_3)
        }),
        Label::derived(move || summary_line(&state))
            .style(|s| s.font_size(design::TEXT_XS).color(design::FG_MUTED)),
        Container::new(Empty::new()).style(|s| s.flex_grow(1.0)),
        // Why the panel is empty, when it is empty for the wrong reason.
        Label::derived(move || state.server_status.get().unwrap_or_default()).style(move |s| {
            s.font_size(design::TEXT_XS)
                .color(design::WARN)
                .margin_right(design::SPACE_2)
                .apply_if(state.server_status.get().is_none(), |s| {
                    s.display(floem::taffy::Display::None)
                })
        }),
        severity_filter(state),
        Label::derived(|| "✕".to_string())
            .on_event_stop(floem::event::listener::Click, move |_, _| on_close())
            .style(|s| {
                s.color(design::FG_FAINT)
                    .font_family(design::SYMBOL.to_string())
                    .font_size(design::TEXT_BASE)
                    .padding_horiz(design::SPACE_2)
                    .border_radius(design::RADIUS_SM)
                    .hover(|s| s.background(design::BG_RAISED).color(design::FG))
            }),
    ))
    .style(|s| {
        s.width_full()
            .items_center()
            .padding_horiz(design::SPACE_3)
            .padding_vert(design::SPACE_2)
            .background(design::BG_DEEP)
            .border_bottom(1.0)
            .border_color(design::BORDER_SUBTLE)
    })
}

fn severity_filter(state: DiagnosticsState) -> impl IntoView {
    dyn_stack(
        move || [Severity::Error, Severity::Warning, Severity::Hint].into_iter(),
        |s| severity_rank(*s),
        move |severity| {
            let selected = move || state.min_severity.get() == severity;
            Label::derived(move || severity_label(severity).to_string())
                .on_event_stop(floem::event::listener::Click, move |_, _| {
                    state.min_severity.set(severity)
                })
                .style(move |s| {
                    s.font_size(design::TEXT_XS)
                        .padding_horiz(design::SPACE_2)
                        .padding_vert(2.0)
                        .margin_right(2.0)
                        .border_radius(design::RADIUS_SM)
                        .cursor(floem::style::CursorStyle::Pointer)
                        .color(if selected() {
                            design::ON_ACCENT
                        } else {
                            design::FG_FAINT
                        })
                        .background(if selected() {
                            severity_color(severity)
                        } else {
                            Color::TRANSPARENT
                        })
                        .hover(|s| s.background(design::BG_RAISED))
                })
        },
    )
    .style(|s| s.flex_row().items_center().margin_right(design::SPACE_2))
}

fn rows(state: DiagnosticsState, on_open: std::rc::Rc<dyn Fn(String, u32, u32)>) -> impl IntoView {
    Stack::vertical((
        dyn_container(
            move || {
                (
                    state.visible().len(),
                    state.total(),
                    state.server_status.get(),
                    state.server_count.get(),
                )
            },
            move |(visible, total, status, servers)| {
                if visible > 0 {
                    return Box::new(Empty::new().style(|s| s.display(floem::taffy::Display::None)))
                        as Box<dyn View>;
                }
                let text = empty_state_text(total, status, servers);
                Box::new(Label::derived(move || text.clone()).style(|s| {
                    s.color(design::FG_FAINT)
                        .font_size(design::TEXT_SM)
                        .line_height(1.5)
                        .width_full()
                        .padding(design::SPACE_4)
                })) as Box<dyn View>
            },
        ),
        dyn_stack(
            move || state.visible().into_iter().enumerate(),
            |(i, _)| *i,
            move |(_, row)| problem_row(row, on_open.clone()),
        )
        .style(|s| s.flex_col().width_full()),
    ))
    .style(|s| s.width_full().padding_vert(design::SPACE_1))
}

fn problem_row(row: ProblemRow, on_open: std::rc::Rc<dyn Fn(String, u32, u32)>) -> impl IntoView {
    let severity = row.severity;
    let message = row.message.clone();
    let location = row.location();
    let code = row.code.clone().unwrap_or_default();
    let (file, line, column) = (row.file.clone(), row.line, row.column);

    Stack::horizontal((
        Label::derived(move || severity_label(severity).to_string()).style(move |s| {
            s.font_size(9.0)
                .font_bold()
                .color(design::ON_ACCENT)
                .background(severity_color(severity))
                .padding_horiz(design::SPACE_1)
                .padding_vert(1.0)
                .border_radius(3.0)
                .width(40.0)
                .justify_center()
                .margin_right(design::SPACE_2)
        }),
        Label::derived(move || message.clone()).style(|s| {
            s.color(design::FG)
                .font_size(design::TEXT_SM)
                .flex_grow(1.0)
                .min_width(0.0)
        }),
        Label::derived(move || code.clone()).style(|s| {
            s.color(design::FG_GHOST)
                .font_size(9.5)
                .font_family(design::MONO.to_string())
                .margin_horiz(design::SPACE_2)
        }),
        Label::derived(move || location.clone()).style(|s| {
            s.color(design::FG_FAINT)
                .font_size(design::TEXT_XS)
                .font_family(design::MONO.to_string())
        }),
    ))
    .on_event_stop(floem::event::listener::Click, move |_, _| {
        on_open(file.clone(), line, column)
    })
    .style(|s| {
        s.width_full()
            .items_center()
            .padding_horiz(design::SPACE_3)
            .padding_vert(design::SPACE_2)
            .cursor(floem::style::CursorStyle::Pointer)
            .border_bottom(1.0)
            .border_color(design::BORDER_SUBTLE)
            .hover(|s| s.background(design::BG_RAISED))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// The distinction the panel exists to make. "No problems reported" over a
    /// project nothing has analysed is a clean bill of health that was never
    /// issued, and it is indistinguishable from the real thing.
    #[test]
    fn an_empty_panel_says_whether_anything_actually_checked() {
        let checked = empty_state_text(0, None, 1);
        let unchecked = empty_state_text(0, None, 0);

        assert_eq!(checked, "No problems reported.");
        assert!(
            unchecked.contains("No language server"),
            "an unchecked project must not read as a clean one: {unchecked}"
        );
        assert_ne!(checked, unchecked);
    }

    /// A recorded failure beats both, because it names the fix.
    #[test]
    fn a_server_failure_is_reported_over_either_empty_state() {
        let advice = "`rust-analyzer` is not on PATH.".to_string();
        assert_eq!(empty_state_text(0, Some(advice.clone()), 0), advice);
        assert_eq!(empty_state_text(9, Some(advice.clone()), 1), advice);
    }

    /// Rows hidden by the severity filter are neither of the above.
    #[test]
    fn rows_hidden_by_the_filter_say_so() {
        let text = empty_state_text(12, None, 1);
        assert!(text.contains("12"), "{text}");
        assert!(text.contains("filter"), "{text}");
    }

    /// Switching project has to empty the panel, and "empty" includes the server
    /// status line.
    ///
    /// Diagnostics are keyed by file and replaced per file, so nothing ever
    /// displaced the previous project's rows — a project with entirely different
    /// files simply *added* its problems to them, and the count grew on every
    /// switch. Observed: 17 warnings from one project, then 19 still naming that
    /// project's files after opening another.
    #[test]
    fn clearing_removes_the_previous_projects_rows_and_its_server_status() {
        let state = DiagnosticsState::new();
        state.publish(
            "src/game/actor.rs",
            vec![ProblemRow {
                file: "src/game/actor.rs".into(),
                line: 45,
                column: 5,
                severity: Severity::Warning,
                message: "variant `Load` is never constructed".into(),
                code: Some("dead_code".into()),
                source: Some("rust-analyzer".into()),
            }],
        );
        state
            .server_status
            .set(Some("rust-analyzer is not on PATH".into()));
        assert_eq!(state.total(), 1);

        state.clear();
        state.server_status.set(None);

        assert_eq!(state.total(), 0, "the previous project's rows survived");
        assert!(
            state.server_status.get().is_none(),
            "a stale 'server is broken' line would describe the project you left"
        );
    }

    /// Rows must read as project-relative paths. They showed absolute ones
    /// because the caller held a project root captured at startup, so after a
    /// project switch the prefix never stripped.
    #[test]
    fn a_file_inside_the_project_is_labelled_relative_to_it() {
        let label = ProblemRow::label_for(
            Path::new("/Users/rj/Desktop/terminal empire/src/main.rs"),
            Path::new("/Users/rj/Desktop/terminal empire"),
        );
        assert_eq!(label, "src/main.rs");
    }

    /// A file genuinely outside the project keeps its full path, because a
    /// misleading relative name is worse than a long one.
    #[test]
    fn a_file_outside_the_project_keeps_its_absolute_path() {
        let label = ProblemRow::label_for(
            Path::new("/elsewhere/vendor/lib.rs"),
            Path::new("/Users/rj/Desktop/terminal empire"),
        );
        assert_eq!(label, "/elsewhere/vendor/lib.rs");
    }

    /// The label is what `location()` renders, so a stale root shows up there
    /// too — this is the string the user actually reads.
    #[test]
    fn the_rendered_location_uses_the_relative_label() {
        let diagnostic = LspDiagnostic {
            range: DocumentRange {
                start: DocumentPosition {
                    line: 17,
                    column: 29,
                },
                end: DocumentPosition {
                    line: 17,
                    column: 31,
                },
            },
            severity: Severity::Error,
            message: "Syntax Error: expected COMMA".into(),
            code: Some("syntax-error".into()),
            source: Some("rust-analyzer".into()),
        };
        let label = ProblemRow::label_for(
            Path::new("/Users/rj/Desktop/terminal empire/src/main.rs"),
            Path::new("/Users/rj/Desktop/terminal empire"),
        );
        let row = ProblemRow::from_diagnostic(&label, &diagnostic);

        assert_eq!(row.location(), "src/main.rs:18:30");
    }

    /// Identical paths are the same file, and that is the fast path.
    #[test]
    fn identical_paths_are_the_same_file() {
        assert!(is_same_file(Path::new("/a/b/c.rs"), Path::new("/a/b/c.rs")));
        assert!(!is_same_file(
            Path::new("/a/b/c.rs"),
            Path::new("/a/b/d.rs")
        ));
    }

    /// A project reached through a symlink comes back from the language server
    /// resolved, and the diagnostic must still find the editor that has it open.
    #[test]
    fn a_symlinked_path_is_the_same_file_as_its_target() {
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("real.rs");
        std::fs::write(&real, "fn main() {}\n").expect("write");
        let link = dir.path().join("link.rs");

        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        #[cfg(not(unix))]
        return;

        assert_ne!(real, link, "the paths differ textually");
        assert!(
            is_same_file(&link, &real),
            "a symlink and its target are one file"
        );
    }

    /// Two paths that do not exist and do not match are not the same file —
    /// `canonicalize` failing must not be read as agreement.
    #[test]
    fn nonexistent_differing_paths_are_not_the_same_file() {
        assert!(!is_same_file(
            Path::new("/does/not/exist/a.rs"),
            Path::new("/does/not/exist/b.rs")
        ));
    }
    use crate::lsp::{DocumentPosition, DocumentRange};

    fn diagnostic(line: u32, severity: Severity, message: &str) -> LspDiagnostic {
        LspDiagnostic {
            range: DocumentRange {
                start: DocumentPosition { line, column: 4 },
                end: DocumentPosition { line, column: 9 },
            },
            severity,
            message: message.to_string(),
            code: Some("E0308".into()),
            source: Some("rustc".into()),
        }
    }

    fn populated() -> DiagnosticsState {
        let state = DiagnosticsState::new();
        state.publish(
            "src/main.rs",
            vec![
                ProblemRow::from_diagnostic(
                    "src/main.rs",
                    &diagnostic(0, Severity::Error, "mismatched types"),
                ),
                ProblemRow::from_diagnostic(
                    "src/main.rs",
                    &diagnostic(9, Severity::Hint, "unused"),
                ),
            ],
        );
        state.publish(
            "src/lib.rs",
            vec![ProblemRow::from_diagnostic(
                "src/lib.rs",
                &diagnostic(4, Severity::Warning, "unused import"),
            )],
        );
        state
    }

    /// LSP counts from zero; every editor and compiler message counts from one.
    /// Getting this wrong sends you to the wrong line, which is worse than not
    /// jumping at all.
    #[test]
    fn positions_are_converted_to_one_based() {
        let row = ProblemRow::from_diagnostic("a.rs", &diagnostic(0, Severity::Error, "boom"));
        assert_eq!(row.line, 1);
        assert_eq!(row.column, 5);
        assert_eq!(row.location(), "a.rs:1:5");
    }

    #[test]
    fn a_multiline_message_is_reduced_to_its_first_line() {
        let row = ProblemRow::from_diagnostic(
            "a.rs",
            &diagnostic(
                0,
                Severity::Error,
                "mismatched types\nexpected u64, found u32",
            ),
        );
        assert_eq!(row.message, "mismatched types");
    }

    /// The server republishes a file wholesale, so an empty list means "fixed".
    /// Merging would leave corrected errors on screen permanently.
    #[test]
    fn publishing_replaces_a_files_diagnostics() {
        let state = populated();
        assert_eq!(state.total(), 3);

        state.publish("src/main.rs", vec![]);
        assert_eq!(state.total(), 1, "an empty publish must clear that file");
        assert_eq!(
            state.count(Severity::Warning),
            1,
            "other files are untouched"
        );
    }

    #[test]
    fn publishing_does_not_duplicate_a_file() {
        let state = populated();
        let before = state.by_file.get_untracked().len();
        state.publish(
            "src/main.rs",
            vec![ProblemRow::from_diagnostic(
                "src/main.rs",
                &diagnostic(0, Severity::Error, "again"),
            )],
        );
        assert_eq!(state.by_file.get_untracked().len(), before);
    }

    #[test]
    fn rows_are_ordered_worst_first() {
        let state = populated();
        state.min_severity.set(Severity::Hint);
        let ranks: Vec<u8> = state
            .visible()
            .iter()
            .map(|r| severity_rank(r.severity))
            .collect();
        let mut sorted = ranks.clone();
        sorted.sort_by_key(|&rank| std::cmp::Reverse(rank));
        assert_eq!(ranks, sorted);
    }

    /// Clippy's hint tier is voluminous; the panel should open on what you
    /// actually came for.
    #[test]
    fn the_default_filter_hides_hints() {
        let state = populated();
        assert_eq!(state.min_severity.get_untracked(), Severity::Warning);
        assert_eq!(state.visible().len(), 2);
        assert_eq!(state.hidden_count(), 1);
    }

    #[test]
    fn lowering_the_filter_reveals_hints() {
        let state = populated();
        state.min_severity.set(Severity::Hint);
        assert_eq!(state.visible().len(), 3);
        assert_eq!(state.hidden_count(), 0);
    }

    #[test]
    fn the_summary_line_reads_naturally() {
        let state = DiagnosticsState::new();
        assert_eq!(summary_line(&state), "no problems");

        state.publish(
            "a.rs",
            vec![ProblemRow::from_diagnostic(
                "a.rs",
                &diagnostic(0, Severity::Error, "x"),
            )],
        );
        assert_eq!(summary_line(&state), "1 error");

        state.publish(
            "b.rs",
            vec![
                ProblemRow::from_diagnostic("b.rs", &diagnostic(0, Severity::Warning, "y")),
                ProblemRow::from_diagnostic("b.rs", &diagnostic(1, Severity::Warning, "z")),
            ],
        );
        assert_eq!(summary_line(&state), "1 error · 2 warnings");
    }

    #[test]
    fn clearing_empties_everything() {
        let state = populated();
        state.clear();
        assert_eq!(state.total(), 0);
        assert!(state.visible().is_empty());
    }

    #[test]
    fn a_fresh_panel_reports_nothing_rather_than_panicking() {
        let state = DiagnosticsState::new();
        assert_eq!(state.total(), 0);
        assert_eq!(state.hidden_count(), 0);
        assert!(state.visible().is_empty());
    }

    #[test]
    fn severity_ordering_is_total() {
        assert!(severity_rank(Severity::Error) > severity_rank(Severity::Warning));
        assert!(severity_rank(Severity::Warning) > severity_rank(Severity::Information));
        assert!(severity_rank(Severity::Information) > severity_rank(Severity::Hint));
    }
}
