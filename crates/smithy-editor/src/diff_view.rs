//! Diff view component for showing code changes
//!
//! This module provides a visual diff viewer that shows before/after code changes
//! with syntax highlighting and accept/reject controls.

use crate::review::ChangeStatus;
use floem::peniko::Color;
use floem::prelude::*;
use floem::reactive::RwSignal;
use floem::style::CustomStylable;

/// Type of diff change
///
/// There is deliberately no `Modified`. A real line diff has no such thing: a
/// changed line is a `Removed` immediately followed by an `Added`, which is
/// what lets the viewer show both texts. The previous positional differ emitted
/// `Modified` and the renderer displayed only the new side, so the old text was
/// computed and then thrown away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffChangeType {
    /// Unchanged line
    Context,
    /// Added line (green)
    Added,
    /// Removed line (red)
    Removed,
}

/// A single line in a diff
#[derive(Debug, Clone)]
pub struct DiffLine {
    /// Type of change
    pub change_type: DiffChangeType,
    /// Old line content (for removed/modified)
    pub old_content: Option<String>,
    /// New line content (for added/modified)
    pub new_content: Option<String>,
    /// Line number in old file
    pub old_line_num: Option<usize>,
    /// Line number in new file
    pub new_line_num: Option<usize>,
}

/// A diff hunk (contiguous block of changes, with its surrounding context)
///
/// The starts are **1-based**, matching the `@@ -a,b +c,d @@` header they are
/// rendered into. [`old_lines`](Self::old_lines) and
/// [`new_lines`](Self::new_lines) convert to the 0-based half-open ranges that
/// partial application needs; go through them rather than re-deriving the
/// arithmetic at each call site.
///
/// Hunks are disjoint and ordered by `old_start` — `similar`'s `grouped_ops`
/// partitions the edit script, so no two hunks can claim the same old line.
/// Partial application relies on that.
#[derive(Debug, Clone)]
pub struct DiffHunk {
    /// Starting line in old file, 1-based
    pub old_start: usize,
    /// Number of lines in old file
    pub old_count: usize,
    /// Starting line in new file, 1-based
    pub new_start: usize,
    /// Number of lines in new file
    pub new_count: usize,
    /// Lines in this hunk
    pub lines: Vec<DiffLine>,
}

impl DiffHunk {
    /// The half-open range of 0-based old-file line indices this hunk covers.
    pub fn old_lines(&self) -> std::ops::Range<usize> {
        let start = self.old_start.saturating_sub(1);
        start..start + self.old_count
    }

    /// The half-open range of 0-based new-file line indices this hunk covers.
    pub fn new_lines(&self) -> std::ops::Range<usize> {
        let start = self.new_start.saturating_sub(1);
        start..start + self.new_count
    }

    /// Whether this hunk contains any actual change, as opposed to being pure
    /// context. `grouped_ops` never yields an all-context group, but a caller
    /// building hunks by hand could.
    pub fn has_change(&self) -> bool {
        self.lines
            .iter()
            .any(|l| l.change_type != DiffChangeType::Context)
    }
}

/// Complete diff between two files
#[derive(Debug, Clone)]
pub struct FileDiff {
    /// File path
    pub path: String,
    /// Language for syntax highlighting
    pub language: String,
    /// Diff hunks
    pub hunks: Vec<DiffHunk>,
    /// Old content (full file)
    pub old_content: String,
    /// New content (full file)
    pub new_content: String,
}

impl FileDiff {
    /// Create a diff from old and new content
    pub fn from_content(path: impl Into<String>, old_content: &str, new_content: &str) -> Self {
        let path = path.into();
        let language = Self::detect_language(&path);

        let hunks = compute_diff_hunks(old_content, new_content);

        Self {
            path,
            language,
            hunks,
            old_content: old_content.to_string(),
            new_content: new_content.to_string(),
        }
    }

    /// Represent creation of a zero-byte file as an explicit review decision.
    ///
    /// An ordinary text diff from `""` to `""` has no hunks, which previously
    /// collapsed "missing" and "present but empty" and gave the modal nothing to
    /// accept. This sentinel hunk changes no bytes; it records file existence.
    pub fn empty_creation(path: impl Into<String>) -> Self {
        let path = path.into();
        Self {
            language: Self::detect_language(&path),
            hunks: vec![DiffHunk {
                old_start: 1,
                old_count: 0,
                new_start: 1,
                new_count: 0,
                lines: vec![DiffLine {
                    change_type: DiffChangeType::Added,
                    old_content: None,
                    new_content: Some("[create empty file]".into()),
                    old_line_num: None,
                    new_line_num: None,
                }],
            }],
            path,
            old_content: String::new(),
            new_content: String::new(),
        }
    }

    /// Detect language from file path
    fn detect_language(path: &str) -> String {
        std::path::Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .map(|ext| match ext {
                "rs" => "rust",
                "js" => "javascript",
                "ts" => "typescript",
                "py" => "python",
                "go" => "go",
                "c" | "h" => "c",
                "cpp" | "hpp" | "cc" => "cpp",
                "java" => "java",
                "md" => "markdown",
                "json" => "json",
                "toml" => "toml",
                "yaml" | "yml" => "yaml",
                _ => "plaintext",
            })
            .unwrap_or("plaintext")
            .to_string()
    }

    /// Check if there are any changes
    pub fn has_changes(&self) -> bool {
        self.hunks.iter().any(DiffHunk::has_change)
    }

    /// Get statistics about the diff
    pub fn stats(&self) -> DiffStats {
        let mut added = 0;
        let mut removed = 0;

        for hunk in &self.hunks {
            for line in &hunk.lines {
                match line.change_type {
                    DiffChangeType::Added => added += 1,
                    DiffChangeType::Removed => removed += 1,
                    DiffChangeType::Context => {}
                }
            }
        }

        DiffStats {
            files_changed: 1,
            insertions: added,
            deletions: removed,
        }
    }
}

/// Diff statistics
#[derive(Debug, Clone, Copy, Default)]
pub struct DiffStats {
    pub files_changed: usize,
    pub insertions: usize,
    pub deletions: usize,
}

/// Colors for diff visualization
#[derive(Clone, Copy)]
pub struct DiffColors {
    pub added_bg: Color,
    pub added_fg: Color,
    pub removed_bg: Color,
    pub removed_fg: Color,
    pub context_bg: Color,
    pub context_fg: Color,
    pub border: Color,
    pub gutter_bg: Color,
    pub gutter_fg: Color,
}

impl Default for DiffColors {
    fn default() -> Self {
        Self {
            added_bg: Color::from_rgb8(46, 160, 67), // Git green
            added_fg: Color::from_rgb8(255, 255, 255),
            removed_bg: Color::from_rgb8(248, 81, 73), // Git red
            removed_fg: Color::from_rgb8(255, 255, 255),
            context_bg: Color::from_rgb8(30, 30, 30),
            context_fg: Color::from_rgb8(212, 212, 212),
            border: Color::from_rgb8(60, 60, 60),
            gutter_bg: Color::from_rgb8(40, 40, 40),
            gutter_fg: Color::from_rgb8(150, 150, 150),
        }
    }
}

/// How many unchanged lines to show either side of a change.
pub(crate) const CONTEXT_LINES: usize = 3;

/// Split text into lines *keeping* their terminators.
///
/// This must agree line-for-line with how `similar` splits, because hunk line
/// indices are used to slice these very vectors when applying a partial
/// acceptance. Keeping the terminators is what makes reassembly byte-exact —
/// `lines()` would silently normalise a missing final newline into a present
/// one, rewriting a file the user never asked to change.
pub(crate) fn split_keeping_ends(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

/// Compute diff hunks between two texts.
///
/// Backed by `similar`'s Myers diff. The previous implementation compared
/// `old[i]` to `new[i]` positionally, which is not a diff: inserting one line
/// at the top of a file reported every following line as changed, emitted the
/// same context line several times, and attributed the insertion to the last
/// line rather than the first. It also could not support partial application,
/// because positional "hunks" do not correspond to edit regions.
fn compute_diff_hunks(old: &str, new: &str) -> Vec<DiffHunk> {
    use similar::{ChangeTag, TextDiff};

    let diff = TextDiff::from_lines(old, new);

    diff.grouped_ops(CONTEXT_LINES)
        .into_iter()
        .filter_map(|group| {
            let first = group.first()?;
            let last = group.last()?;
            let old_range = first.old_range().start..last.old_range().end;
            let new_range = first.new_range().start..last.new_range().end;

            let mut lines = Vec::new();
            for op in &group {
                for change in diff.iter_changes(op) {
                    // The terminator is part of the value; strip it for display
                    // only. Reassembly reads the original text, not this.
                    let text = change.value().trim_end_matches(['\n', '\r']).to_string();
                    let (change_type, old_content, new_content) = match change.tag() {
                        ChangeTag::Equal => {
                            (DiffChangeType::Context, Some(text.clone()), Some(text))
                        }
                        ChangeTag::Delete => (DiffChangeType::Removed, Some(text), None),
                        ChangeTag::Insert => (DiffChangeType::Added, None, Some(text)),
                    };
                    lines.push(DiffLine {
                        change_type,
                        old_content,
                        new_content,
                        old_line_num: change.old_index().map(|i| i + 1),
                        new_line_num: change.new_index().map(|i| i + 1),
                    });
                }
            }

            Some(DiffHunk {
                old_start: old_range.start + 1,
                old_count: old_range.len(),
                new_start: new_range.start + 1,
                new_count: new_range.len(),
                lines,
            })
        })
        .collect()
}

/// Diff header with stats and action buttons
fn diff_header(
    diff: RwSignal<FileDiff>,
    statuses: RwSignal<Vec<ChangeStatus>>,
    colors: DiffColors,
    on_accept: impl Fn() + 'static,
    on_reject: impl Fn() + 'static,
) -> impl IntoView {
    let stats = move || {
        let s = diff.get().stats();
        format!("+{} -{}", s.insertions, s.deletions)
    };

    // The Accept button applies whatever the per-hunk decisions currently say,
    // so it has to name that rather than "Accept" — otherwise the one control
    // that writes to disk gives no clue what it is about to write.
    let kept_of_total = move || {
        let decided = statuses.get();
        let total = decided.len();
        let kept = decided
            .iter()
            .filter(|s| **s == ChangeStatus::Accepted)
            .count();
        (kept, total)
    };

    let accept_label = move || match kept_of_total() {
        (_, 0) => "Apply".to_string(),
        (k, t) if k == t => "Apply all".to_string(),
        // Not "Apply nothing": that read as a *third* action beside Discard,
        // which it is not — it is the same outcome under a different name. The
        // button goes inert instead and the hint line says to use Discard.
        (0, _) => "Apply".to_string(),
        (k, t) => format!("Apply {k} of {t}"),
    };

    // What the buttons are about to do, in a sentence. Without it, a reviewer who
    // clicks Skip is left with a hunk labelled "will skip" and no indication of
    // what commits that decision — the per-hunk buttons look like they should
    // have done something already.
    let hint = move || match kept_of_total() {
        (_, 0) => String::new(),
        (0, 1) => {
            "This change will not be applied — that is the same as discarding it.".to_string()
        }
        (0, t) => format!("None of the {t} hunks will be applied — use Discard instead."),
        (k, t) if k == t && t == 1 => "Apply writes this change to the file.".to_string(),
        (k, t) if k == t => {
            format!("All {t} hunks will be applied. Skip any you do not want, then Apply.")
        }
        (k, t) => format!(
            "{k} of {t} hunks will be applied, {} skipped. Apply writes only the accepted ones.",
            t - k
        ),
    };

    Stack::vertical((
        Stack::horizontal((
            Label::derived(move || format!("File: {}", diff.get().path))
                .style(move |s| s.color(colors.context_fg).font_size(14.0)),
            Label::derived(stats)
                .style(move |s| s.color(colors.added_bg).font_size(12.0).margin_left(16.0)),
            Container::new(Stack::horizontal((
                Button::new(Label::derived(accept_label))
                    .on_event_stop(floem::event::listener::Click, move |_, _| {
                        // Inert when it would write nothing. Resolving the review
                        // here would be indistinguishable from Discard, and two
                        // buttons doing one thing is what made this confusing.
                        let (kept, _) = kept_of_total();
                        if kept > 0 {
                            on_accept();
                        }
                    })
                    .style(move |s| {
                        let (kept, _) = kept_of_total();
                        s.background(if kept == 0 {
                            colors.border
                        } else {
                            colors.added_bg
                        })
                        .color(if kept == 0 {
                            colors.gutter_fg
                        } else {
                            Color::WHITE
                        })
                        .padding_horiz(12.0)
                        .padding_vert(4.0)
                        .border_radius(4.0)
                    }),
                Button::new("Discard")
                    .on_event_stop(floem::event::listener::Click, move |_, _| on_reject())
                    .style(move |s| {
                        s.background(colors.removed_bg)
                            .color(Color::WHITE)
                            .padding_horiz(12.0)
                            .padding_vert(4.0)
                            .border_radius(4.0)
                            .margin_left(8.0)
                    }),
            )))
            .style(|s| s.flex_grow(1.0).justify_end()),
        ))
        .style(|s| s.width_full().items_center()),
        Label::derived(hint).style(move |s| {
            s.color(colors.gutter_fg)
                .font_size(11.0)
                .margin_top(6.0)
                .apply_if(hint().is_empty(), |s| {
                    s.display(floem::taffy::Display::None)
                })
        }),
    ))
    .style(move |s| {
        s.width_full()
            .padding(12.0)
            .background(colors.gutter_bg)
            .border_bottom(1.0)
            .border_color(colors.border)
    })
}

/// Create a diff view with per-hunk accept/reject controls.
///
/// `statuses` is indexed in step with `diff.hunks`; it is read to show each
/// hunk's current decision and written by the callbacks. Keeping it a signal
/// rather than deriving it inside is what lets the header say how many hunks
/// the Apply button is about to write.
pub fn diff_view_reviewable(
    diff: RwSignal<FileDiff>,
    statuses: RwSignal<Vec<ChangeStatus>>,
    colors: DiffColors,
    on_accept_hunk: impl Fn(usize) + 'static + Clone,
    on_reject_hunk: impl Fn(usize) + 'static + Clone,
    on_accept_all: impl Fn() + 'static,
    on_reject_all: impl Fn() + 'static,
) -> impl IntoView {
    let header = diff_header(diff, statuses, colors, on_accept_all, on_reject_all);

    // A one-hunk diff gets no per-hunk controls. Apply/Skip on the single hunk
    // decides exactly what the header's Apply/Discard already decides, and having
    // both is what made the modal read as broken: clicking Skip set the hunk to
    // "will skip" and appeared to do nothing, because the action that commits it
    // is the other pair of buttons. Read untracked because the hunk count of a
    // given review cannot change while its modal is open.
    let per_hunk_controls = diff.get_untracked().hunks.len() > 1;

    let content = floem::views::scroll::Scroll::new(
        dyn_stack(
            move || {
                let d = diff.get();
                d.hunks.into_iter().enumerate().collect::<Vec<_>>()
            },
            |(idx, _)| *idx,
            move |(idx, hunk)| {
                let on_accept = on_accept_hunk.clone();
                let on_reject = on_reject_hunk.clone();
                diff_view_hunk_reviewable(
                    idx,
                    hunk,
                    statuses,
                    colors,
                    per_hunk_controls,
                    on_accept,
                    on_reject,
                )
            },
        )
        .style(|s| s.flex_col().width_full()),
    )
    .custom_style(|s: floem::views::scroll::ScrollCustomStyle| {
        s.hide_bars(false)
            .handle_background(Color::from_rgba8(150, 150, 150, 150))
            .handle_border_radius(4.0)
    })
    .style(|s| s.width_full().flex_grow(1.0));

    let bg = colors.context_bg;
    Stack::vertical((header, content)).style(move |s| s.width_full().height_full().background(bg))
}

/// What a hunk's current decision is, as read from the shared status vector.
///
/// Out-of-range reads answer `Pending` rather than panicking: the statuses
/// signal and the diff signal are updated separately, so for one frame after a
/// new review is raised the vector can still be the previous change's.
fn hunk_status(statuses: RwSignal<Vec<ChangeStatus>>, idx: usize) -> ChangeStatus {
    statuses
        .get()
        .get(idx)
        .copied()
        .unwrap_or(ChangeStatus::Pending)
}

/// Single hunk view with accept/reject buttons
fn diff_view_hunk_reviewable(
    hunk_idx: usize,
    hunk: DiffHunk,
    statuses: RwSignal<Vec<ChangeStatus>>,
    colors: DiffColors,
    // Whether this hunk shows its own Apply/Skip pair. False for a single-hunk
    // review, where the header's buttons already say everything.
    show_controls: bool,
    on_accept: impl Fn(usize) + 'static,
    on_reject: impl Fn(usize) + 'static,
) -> impl IntoView {
    let old_start = hunk.old_start;
    let old_count = hunk.old_count;
    let new_start = hunk.new_start;
    let new_count = hunk.new_count;
    let lines = hunk.lines;

    let header = Stack::horizontal((
        Label::derived(move || {
            format!(
                "@@ -{},{} +{},{} @@",
                old_start, old_count, new_start, new_count
            )
        })
        .style(move |s| {
            s.color(colors.gutter_fg)
                .font_size(12.0)
                .font_family(crate::design::SYMBOL.to_string())
                .apply_if(!show_controls, |s| s.flex_grow(1.0))
        }),
        // The decision in words. Colour alone would carry it for most people
        // and for nobody with a red-green deficiency, which is roughly one man
        // in twelve — on a control that decides what gets written to disk.
        Label::derived(move || match hunk_status(statuses, hunk_idx) {
            ChangeStatus::Accepted => "will apply".to_string(),
            ChangeStatus::Rejected => "will skip".to_string(),
            ChangeStatus::Pending => "undecided".to_string(),
        })
        .style(move |s| {
            s.font_size(11.0)
                .margin_left(12.0)
                .flex_grow(1.0)
                .color(match hunk_status(statuses, hunk_idx) {
                    ChangeStatus::Accepted => colors.added_bg,
                    ChangeStatus::Rejected => colors.removed_bg,
                    ChangeStatus::Pending => colors.gutter_fg,
                })
                .apply_if(!show_controls, |s| s.display(floem::taffy::Display::None))
        }),
        Button::new("Apply")
            .on_event_stop(floem::event::listener::Click, move |_, _| {
                on_accept(hunk_idx)
            })
            .style(move |s| {
                // The unchosen button recedes rather than disappearing, so the
                // decision stays reversible and visibly so.
                let chosen = hunk_status(statuses, hunk_idx) == ChangeStatus::Accepted;
                s.background(if chosen {
                    colors.added_bg
                } else {
                    colors.border
                })
                .color(if chosen {
                    Color::WHITE
                } else {
                    colors.gutter_fg
                })
                .padding_horiz(8.0)
                .padding_vert(2.0)
                .border_radius(3.0)
                .font_size(11.0)
                .apply_if(!show_controls, |s| s.display(floem::taffy::Display::None))
            }),
        Button::new("Skip")
            .on_event_stop(floem::event::listener::Click, move |_, _| {
                on_reject(hunk_idx)
            })
            .style(move |s| {
                let chosen = hunk_status(statuses, hunk_idx) == ChangeStatus::Rejected;
                s.background(if chosen {
                    colors.removed_bg
                } else {
                    colors.border
                })
                .color(if chosen {
                    Color::WHITE
                } else {
                    colors.gutter_fg
                })
                .padding_horiz(8.0)
                .padding_vert(2.0)
                .border_radius(3.0)
                .font_size(11.0)
                .margin_left(4.0)
                .apply_if(!show_controls, |s| s.display(floem::taffy::Display::None))
            }),
    ))
    .style(move |s| {
        s.background(colors.gutter_bg)
            .padding(4.0)
            .width_full()
            .items_center()
    });

    let lines_view = dyn_stack(
        move || lines.clone().into_iter().enumerate().collect::<Vec<_>>(),
        |(idx, _)| *idx,
        move |(_, line)| diff_view_line(line, colors),
    )
    .style(move |s| {
        // A skipped hunk fades. Without it, scrolling a long review gives no
        // peripheral sense of what is still going to land.
        s.flex_col().width_full().apply_if(
            hunk_status(statuses, hunk_idx) == ChangeStatus::Rejected,
            |s| s.opacity(0.45),
        )
    });

    Stack::vertical((header, lines_view)).style(|s| s.width_full().margin_bottom(16.0))
}

/// Single diff line view
fn diff_view_line(line: DiffLine, colors: DiffColors) -> impl IntoView {
    // The foreground moves with the background. Leaving every line on
    // `context_fg` put mid-grey text on saturated green and red — about 2.3:1,
    // below any legibility threshold — and left `added_fg`/`removed_fg`
    // defined but never read.
    let (bg_color, fg_color, prefix) = match line.change_type {
        DiffChangeType::Context => (colors.context_bg, colors.context_fg, " "),
        DiffChangeType::Added => (colors.added_bg, colors.added_fg, "+"),
        DiffChangeType::Removed => (colors.removed_bg, colors.removed_fg, "-"),
    };

    let line_num = format!(
        "{:4} {:4}",
        line.old_line_num.map(|n| n.to_string()).unwrap_or_default(),
        line.new_line_num.map(|n| n.to_string()).unwrap_or_default()
    );

    let content = line.new_content.or(line.old_content).unwrap_or_default();

    Stack::horizontal((
        Label::derived(move || line_num.clone()).style(move |s| {
            s.color(colors.gutter_fg)
                .font_size(12.0)
                .font_family(crate::design::MONO.to_string())
                .background(colors.gutter_bg)
                .width(80.0)
                .padding_horiz(8.0)
        }),
        Label::derived(move || format!("{} {}", prefix, content)).style(move |s| {
            s.color(fg_color)
                .font_size(12.0)
                .font_family(crate::design::MONO.to_string())
                .background(bg_color)
                .flex_grow(1.0)
                .padding_horiz(8.0)
        }),
    ))
    .style(|s| s.width_full())
}

// ============================================================================
// Review modal
// ============================================================================
//
// Recovered from forge's `side_chat` module, which mixed the chat transcript,
// the diff modal, and the theme into one file. The modal is about diffs, so it
// lives with them.

use std::sync::Arc;

/// Diff modal overlay.
///
/// `on_accept` receives the diff together with the per-hunk decisions, and is
/// responsible for turning those into file content — the modal deliberately
/// does not write anything itself.
///
/// The decisions start as `Accepted`. A reviewer who opens the modal and clicks
/// Apply means "yes, all of it"; skipping a hunk is the deliberate act. Note
/// that `PendingFileChange` defaults the other way, to `Pending`, which
/// `build_accepted_content` treats as *do not write*. That asymmetry is
/// intended: the model is conservative so an unreviewed change can never leak
/// onto disk, and the intent to apply is supplied here, by a person looking at
/// the diff.
pub fn diff_modal(
    review: RwSignal<Option<crate::review::PendingFileChange>>,
    on_accept: impl Fn(FileDiff, Vec<ChangeStatus>) + 'static,
    on_reject: impl Fn() + 'static,
    on_close: impl Fn() + 'static,
) -> impl IntoView {
    let colors = DiffColors::default();
    // Plain Arc (no Option/take): the callbacks are Fn and must stay callable across
    // repeated modal openings — a taken Option would leave dead buttons on the second review.
    let on_accept = Arc::new(on_accept);
    let on_reject = Arc::new(on_reject);
    let on_close = Arc::new(on_close);

    Container::new(dyn_container(
        // Keyed on the queued change, not on a bare diff. Reviews are matched
        // on their opaque registration — one turn can queue two writes to the
        // same path — so the id survives as far as whatever resolves them.
        move || review.get(),
        move |d| {
            if let Some(file_diff) = d.map(|c| c.diff) {
                let on_accept_all = on_accept.clone();
                let on_accept_hunk_diff = file_diff.clone();
                let on_reject = on_reject.clone();
                let on_close = on_close.clone();

                // Created here, inside the branch that rebuilds per change, so
                // the vector is always the right length for the diff beside it.
                // Hoisting it out would leave the previous change's decisions in
                // place when the next review is raised.
                let statuses = RwSignal::new(vec![ChangeStatus::Accepted; file_diff.hunks.len()]);
                let diff_signal = RwSignal::new(file_diff);

                let set_hunk = move |idx: usize, status: ChangeStatus| {
                    statuses.update(|v| {
                        if let Some(slot) = v.get_mut(idx) {
                            *slot = status;
                        }
                    });
                };

                Box::new(
                    Stack::vertical((
                        // Header
                        Stack::horizontal((
                            Label::derived(move || "Proposed Changes".to_string())
                                .style(|s| s.color(Color::WHITE).font_size(18.0).font_bold()),
                            Container::new(
                                Button::new("×")
                                    .on_event_stop(floem::event::listener::Click, move |_, _| {
                                        on_close()
                                    })
                                    .style(|s| {
                                        s.background(Color::TRANSPARENT)
                                            .color(Color::WHITE)
                                            .font_size(24.0)
                                    }),
                            )
                            .style(|s| s.flex_grow(1.0).justify_end()),
                        ))
                        .style(|s| s.width_full().padding(16.0)),
                        // Diff view
                        diff_view_reviewable(
                            diff_signal,
                            statuses,
                            colors,
                            move |idx| set_hunk(idx, ChangeStatus::Accepted),
                            move |idx| set_hunk(idx, ChangeStatus::Rejected),
                            move || {
                                on_accept_all(on_accept_hunk_diff.clone(), statuses.get_untracked())
                            },
                            move || on_reject(),
                        ),
                    ))
                    .style(|s| {
                        s.width(800.0)
                            .height(600.0)
                            .background(Color::from_rgb8(30, 30, 30))
                            .border_radius(8.0)
                    }),
                ) as Box<dyn View>
            } else {
                Box::new(Empty::new().style(|s| s.display(floem::taffy::Display::None)))
                    as Box<dyn View>
            }
        },
    ))
    .style(move |s| {
        // The dimming overlay must only exist while a diff is showing — this modal is
        // stacked permanently over the main window, so an unconditional overlay would
        // dim and click-block the entire app.
        if review.get().is_some() {
            s.absolute()
                .inset(0.0)
                .background(Color::from_rgba8(0, 0, 0, 204))
                .items_center()
                .justify_center()
                .z_index(100)
        } else {
            s.display(floem::taffy::Display::None)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn changed_lines(diff: &FileDiff) -> Vec<(DiffChangeType, String)> {
        diff.hunks
            .iter()
            .flat_map(|h| &h.lines)
            .filter(|l| l.change_type != DiffChangeType::Context)
            .map(|l| {
                (
                    l.change_type,
                    l.new_content
                        .clone()
                        .or_else(|| l.old_content.clone())
                        .unwrap_or_default(),
                )
            })
            .collect()
    }

    /// The defect that motivated replacing the differ: the old one compared
    /// `old[i]` to `new[i]`, so a single insertion at the top reported every
    /// following line as changed and blamed the insertion on the last line.
    #[test]
    fn inserting_one_line_reports_exactly_one_insertion() {
        let old = "alpha\nbravo\ncharlie\ndelta\necho\nfoxtrot\n";
        let new = "INSERTED\nalpha\nbravo\ncharlie\ndelta\necho\nfoxtrot\n";

        let diff = FileDiff::from_content("t.rs", old, new);
        let stats = diff.stats();

        assert_eq!(stats.insertions, 1, "one line was added");
        assert_eq!(stats.deletions, 0, "no line was removed");
        assert_eq!(
            changed_lines(&diff),
            vec![(DiffChangeType::Added, "INSERTED".to_string())],
            "the added line must be the one actually inserted"
        );
    }

    /// Same failure in the other direction.
    #[test]
    fn deleting_one_line_reports_exactly_one_deletion() {
        let old = "alpha\nbravo\ncharlie\ndelta\necho\nfoxtrot\n";
        let new = "alpha\ncharlie\ndelta\necho\nfoxtrot\n";

        let diff = FileDiff::from_content("t.rs", old, new);
        let stats = diff.stats();

        assert_eq!(stats.deletions, 1);
        assert_eq!(stats.insertions, 0);
        assert_eq!(
            changed_lines(&diff),
            vec![(DiffChangeType::Removed, "bravo".to_string())]
        );
    }

    /// A changed line must surface *both* texts. The old differ collapsed it to
    /// one `Modified` line and the renderer showed only the new side, so the
    /// user could not see what was being replaced.
    #[test]
    fn a_changed_line_shows_both_the_old_and_the_new_text() {
        let old = "keep\nbefore\nkeep2\n";
        let new = "keep\nafter\nkeep2\n";

        let diff = FileDiff::from_content("t.rs", old, new);

        assert_eq!(
            changed_lines(&diff),
            vec![
                (DiffChangeType::Removed, "before".to_string()),
                (DiffChangeType::Added, "after".to_string()),
            ]
        );
    }

    /// Context lines are for orientation; repeating them makes the hunk
    /// unreadable. The old differ re-emitted up to three of them per change.
    #[test]
    fn context_lines_are_not_repeated_within_a_hunk() {
        let old = "a\nb\nc\nd\ne\nf\ng\n";
        let new = "a\nb\nC\nd\nE\nf\ng\n";

        let diff = FileDiff::from_content("t.rs", old, new);

        for hunk in &diff.hunks {
            let mut seen = std::collections::HashSet::new();
            for line in &hunk.lines {
                if line.change_type == DiffChangeType::Context {
                    let n = line.old_line_num.expect("context line has an old number");
                    assert!(seen.insert(n), "old line {n} appeared twice in one hunk");
                }
            }
        }
    }

    /// Two changes far apart are two hunks; two changes close together share
    /// one. Partial acceptance is only meaningful if the grouping is sane.
    #[test]
    fn distant_changes_become_separate_hunks() {
        let mut old: Vec<String> = (0..40).map(|i| format!("line{i}")).collect();
        let new_lines = {
            let mut v = old.clone();
            v[2] = "CHANGED-EARLY".into();
            v[35] = "CHANGED-LATE".into();
            v
        };
        old.push(String::new());

        let diff = FileDiff::from_content(
            "t.rs",
            &format!("{}\n", old[..40].join("\n")),
            &format!("{}\n", new_lines.join("\n")),
        );

        assert_eq!(diff.hunks.len(), 2, "40 lines apart is not one hunk");
        assert!(diff.hunks[0].old_start < diff.hunks[1].old_start);
    }

    /// Hunk ranges are what partial application slices with. If they overlap or
    /// run backwards, accepting one hunk corrupts its neighbour.
    #[test]
    fn hunk_ranges_are_disjoint_and_ascending() {
        let old: String = (0..60)
            .map(|i| format!("line{i}\n"))
            .collect::<Vec<_>>()
            .join("");
        let new: String = (0..60)
            .map(|i| {
                if i % 17 == 0 {
                    format!("CHANGED{i}\n")
                } else {
                    format!("line{i}\n")
                }
            })
            .collect::<Vec<_>>()
            .join("");

        let diff = FileDiff::from_content("t.rs", &old, &new);
        assert!(diff.hunks.len() > 1);

        let mut prev_end = 0;
        for hunk in &diff.hunks {
            let r = hunk.old_lines();
            assert!(
                r.start >= prev_end,
                "hunk starting at {} overlaps the previous, which ended at {prev_end}",
                r.start
            );
            prev_end = r.end;
        }
    }

    /// Identical content is not a change, however the differ is arranged.
    #[test]
    fn identical_content_produces_no_changes() {
        let text = "one\ntwo\nthree\n";
        let diff = FileDiff::from_content("t.rs", text, text);
        assert!(!diff.has_changes());
        assert_eq!(diff.stats().insertions, 0);
        assert_eq!(diff.stats().deletions, 0);
    }

    #[test]
    fn a_path_extension_selects_the_diff_language() {
        assert_eq!(FileDiff::detect_language("test.rs"), "rust");
        assert_eq!(FileDiff::detect_language("test.js"), "javascript");
        assert_eq!(FileDiff::detect_language("test.py"), "python");
        assert_eq!(FileDiff::detect_language("test.unknown"), "plaintext");
    }
}
