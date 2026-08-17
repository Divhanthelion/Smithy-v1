//! The code editor.
//!
//! ## Why this replaces the label-based view
//!
//! The previous editor rendered each line as a row of `label` views. That looks
//! like text but is not an editor, and it could not be made into one: floem's
//! `label` intercepts `PointerDown` in `event_before_children` to run its own
//! text selection. So clicks never reached our handler and focus never landed on
//! our view — which is precisely the observed symptom, that you could *select*
//! text but never type into it. No amount of handler placement fixes that,
//! because the label consumes the event before any ancestor sees it.
//!
//! floem ships a real editor (`floem::views::text_editor`) backed by a `Rope`,
//! with a caret, selection, key handling, undo, and a gutter. Our `Buffer` is
//! already `ropey`-based, so the content moves across without conversion.
//!
//! Syntax colouring and inline diagnostics are supplied by
//! [`crate::syntax_styling::SyntaxStyling`], which implements floem's `Styling`
//! trait — the same hook serves both, so they cannot disagree about a range.
//!
//! Diagnostic squiggles cannot come through that trait and are painted as a
//! layer over the editor instead; see [`squiggle_overlay`]. That makes the view
//! a two-child stack rather than the bare `text_editor` it used to be — the
//! overlay is absolute and takes no part in the layout.

use std::path::PathBuf;
use std::rc::Rc;

use floem::context::VisualChanged;
use floem::peniko::kurbo::{Point, Rect, Stroke};
use floem::prelude::*;
use floem::reactive::{Memo, RwSignal, SignalGet, SignalTrack, SignalUpdate};
use floem::views::editor::command::Command;
use floem::views::editor::core::command::EditCommand;
use floem::views::editor::core::cursor::CursorAffinity;
use floem::views::editor::text::Document;
use floem::views::editor::Editor;
use floem::views::text_editor;
use floem::views::{canvas, Stack};

use crate::aesthetic::Aesthetic;
use crate::celestial::editor_pane_fill;
use crate::design;
use crate::squiggle::{self, VisualRow};
use crate::syntax_styling::{color_for_severity, EditSpan, InlineDiagnostic, SyntaxStyling};

/// A handle to an open editor, so the app can read the text back to save it.
#[derive(Clone)]
pub struct EditorHandle {
    /// Absolute path of the file being edited.
    pub path: PathBuf,
    /// Bumped on every edit, so a dirty indicator can react.
    pub revision: RwSignal<u64>,
    /// Bumped whenever diagnostics change. Separate from `revision` because
    /// diagnostics arrive from the language server without the text moving, and
    /// nothing else the squiggle layer reads would change to tell it so.
    diagnostics: RwSignal<u64>,
    /// The text as last written to disk. Dirty state is a comparison against
    /// this rather than a flag, so an edit-then-undo correctly reads as clean.
    saved: RwSignal<String>,
    doc: std::rc::Rc<dyn Document>,
    editor: floem::views::editor::Editor,
    styling: std::rc::Rc<SyntaxStyling>,
}

impl EditorHandle {
    /// The current text.
    pub fn text(&self) -> String {
        self.doc.text().to_string()
    }

    /// Whether the buffer differs from what is on disk.
    ///
    /// Compared against the last-saved text rather than tracked with a flag:
    /// typing a character and undoing it leaves a flag set but the file
    /// unchanged, and then you get a save prompt for a file you did not edit.
    pub fn is_dirty(&self) -> bool {
        self.text() != self.saved.get()
    }

    /// Write the buffer to its path.
    ///
    /// Uses `std::fs` directly rather than the agent's capability sandbox: this
    /// is a file the user opened themselves, and confining their own editing to
    /// the agent's workspace root would be wrong.
    pub fn save(&self) -> Result<(), String> {
        if self.path.as_os_str().is_empty() {
            return Err("this buffer has no path to save to".into());
        }
        if !self.path.is_absolute() {
            return Err("this inspection buffer is not a file".into());
        }
        let text = self.text();
        std::fs::write(&self.path, &text)
            .map_err(|e| format!("could not save {}: {e}", self.path.display()))?;
        self.saved.set(text);
        Ok(())
    }

    /// Replace the document in place. Used when re-clicking an inspection tab
    /// that is already focused — rebuilding the pane would throw away the caret.
    pub fn replace_content(&self, content: &str) {
        if content == self.text() {
            return;
        }
        let len = self.doc.text().len();
        self.doc.edit_single(
            floem::views::editor::core::selection::Selection::region(
                0,
                len,
                CursorAffinity::Backward,
            ),
            content,
            floem::views::editor::core::editor::EditType::Other,
        );
        self.saved.set(content.to_string());
    }

    /// Replace the buffer with what is on disk.
    ///
    /// **In place, through the document's own edit path**, not by rebuilding the
    /// pane. A rebuild is the obvious way to reload and it throws away the
    /// caret, the selection and the undo history — for a change the user did not
    /// make. `edit_single` over the whole range keeps all three and records the
    /// reload as one undoable step, so a reload the user did not want is one
    /// `⌘Z` away.
    ///
    /// **Identical content is not an edit.** This is what makes the whole
    /// external-change path safe against the editor's own writes: after Smithy
    /// saves, the disk and the buffer agree, so this returns having done
    /// nothing. The watcher does not have to know who wrote the file, which is a
    /// question it was previously answering with `!is_file_open` — a test that is
    /// not merely unreliable but backwards, since a change to a file you have
    /// open is exactly the one worth reacting to.
    pub fn reload_from_disk(&self) -> Result<(), String> {
        let content = std::fs::read_to_string(&self.path)
            .map_err(|e| format!("could not re-read {}: {e}", self.path.display()))?;
        if content == self.text() {
            return Ok(());
        }

        let len = self.doc.text().len();
        self.doc.edit_single(
            floem::views::editor::core::selection::Selection::region(
                0,
                len,
                CursorAffinity::Backward,
            ),
            &content,
            floem::views::editor::core::editor::EditType::Other,
        );
        // The file on disk is now what the buffer holds, so the tab is clean.
        self.saved.set(content);
        Ok(())
    }

    /// Whether the file on disk still matches what was last written from here.
    ///
    /// `Ok(false)` means somebody else changed it. An unreadable file reports
    /// `Ok(true)`: it has been deleted or is momentarily absent mid-rename, and
    /// neither is a reason to interrupt somebody with a reload prompt.
    pub fn matches_disk(&self) -> bool {
        match std::fs::read_to_string(&self.path) {
            Ok(on_disk) => on_disk == self.text(),
            Err(_) => true,
        }
    }

    /// Run an edit command — what the Edit menu dispatches.
    ///
    /// These are the same commands floem's own keymap binds, so the menu and the
    /// keyboard cannot drift apart.
    pub fn run_edit(&self, command: EditCommand) {
        self.doc.run_command(
            &self.editor,
            &Command::Edit(command),
            None,
            floem::prelude::Modifiers::empty(),
        );
    }

    pub fn undo(&self) {
        self.run_edit(EditCommand::Undo);
    }
    pub fn redo(&self) {
        self.run_edit(EditCommand::Redo);
    }
    pub fn cut(&self) {
        self.run_edit(EditCommand::ClipboardCut);
    }
    pub fn copy(&self) {
        self.run_edit(EditCommand::ClipboardCopy);
    }
    pub fn paste(&self) {
        self.run_edit(EditCommand::ClipboardPaste);
    }
    pub fn select_all(&self) {
        self.doc.run_command(
            &self.editor,
            &Command::MultiSelection(
                floem::views::editor::core::command::MultiSelectionCommand::SelectAll,
            ),
            None,
            floem::prelude::Modifiers::empty(),
        );
    }

    /// The caret as an LSP position: zero-based line, and column measured in
    /// **UTF-16 code units**.
    ///
    /// UTF-16 is the protocol default, and it is not the same as characters or
    /// bytes. On a line containing an emoji, a byte column, a character column
    /// and a UTF-16 column are three different numbers, and sending the wrong
    /// one makes hover and go-to-definition silently target the wrong token.
    pub fn caret_position(&self) -> (u32, u32) {
        let offset = self.editor.cursor.get_untracked().offset();
        let text = self.doc.text();
        let line = text.line_of_offset(offset);
        let line_start = text.offset_of_line(line);
        let prefix = text.slice_to_cow(line_start..offset);
        let column = utf16_len(&prefix);
        (line as u32, column as u32)
    }

    /// Move the caret to the start of a one-based line.
    ///
    /// Clamped: a diagnostic can outlive the edit that fixed it, so the line it
    /// names may no longer exist.
    pub fn goto_line(&self, line_one_based: u32) {
        let text = self.doc.text();
        let last = text.line_of_offset(text.len());
        let line = (line_one_based.saturating_sub(1) as usize).min(last);
        let offset = text.offset_of_line(line);
        self.editor.cursor.update(|cursor| {
            cursor.set_offset(offset, CursorAffinity::Backward, false, false);
        });
    }

    /// Push new diagnostics into the inline styling.
    pub fn set_diagnostics(&self, diagnostics: Vec<InlineDiagnostic>) {
        self.styling.set_diagnostics(diagnostics);
        self.diagnostics.update(|r| *r += 1);
    }

    /// Re-run the syntax parse against the current text.
    pub fn reparse(&self) {
        self.styling.reparse(&self.doc.text());
    }

    /// Filename for display, with a dot when unsaved.
    pub fn tab_label(&self) -> String {
        let name = self
            .path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "untitled".into());
        if self.is_dirty() {
            format!("● {name}")
        } else {
            name
        }
    }
}

/// Build an editor for `content`.
///
/// Returns the view and a handle for reading the text back. The caller is
/// expected to construct a fresh editor per file rather than swapping content
/// into one — floem's document owns its undo history, and reusing a document
/// across files would let you undo your way into the previous file's contents.
pub fn code_editor(
    path: PathBuf,
    content: &str,
    aesthetic: RwSignal<Aesthetic>,
) -> (impl IntoView, EditorHandle) {
    let revision = RwSignal::new(0u64);
    let diagnostics = RwSignal::new(0u64);

    // Syntax colouring and inline diagnostics share one `Styling` impl.
    let styling = std::rc::Rc::new(SyntaxStyling::new(13));
    styling.set_language_from_path(&path);
    // One conversion, at load. Every reparse after this reads floem's own rope.
    styling.reparse(&lapce_xi_rope::Rope::from(content));

    let editor = text_editor(content)
        .styling_rc(styling.clone())
        .editor_style(|s| {
            s.gutter_accent_color(design::ACCENT)
                .gutter_dim_color(design::FG_GHOST)
                .gutter_left_padding(design::SPACE_3 as f64)
                .gutter_right_padding(design::SPACE_3 as f64)
                .cursor_color(design::ACCENT)
                .selection_color(design::BG_SELECTED)
                .current_line_color(design::BG_RAISED)
                .indent_guide_color(design::BORDER_SUBTLE)
                .indent_guide(true)
                .scroll_beyond_last_line(true)
                .cursor_surrounding_lines(3)
        })
        .style(move |s| {
            s.width_full()
                .height_full()
                .font_family(design::MONO.to_string())
                .font_size(13.0)
                .color(design::FG)
                .background(editor_pane_fill(aesthetic.get()))
        });

    let doc = editor.doc();
    let editor_ref = editor.editor().clone();

    // Anything that mutates the document counts as an edit. Re-parse here so
    // colours track the text: floem only re-lays-out a line when the styling id
    // changes, and `reparse` is what changes it.
    let revision_for_edit = revision;
    let styling_for_edit = styling.clone();
    let doc_for_edit = doc.clone();
    let editor = editor.update(move |update| {
        let text = doc_for_edit.text();
        // **Incremental where it can be.** floem hands us the deltas, so
        // tree-sitter can be told what moved and reuse the rest of its tree
        // rather than re-parsing the file on every keystroke — 7 ms on a
        // 1,900-line file and 18 ms on a 13,000-line one, the latter past a
        // frame at 60 Hz.
        //
        // Only the single-delta case, which is every ordinary edit. A batch
        // would need each delta applied against the text as it stood after the
        // one before, and getting that subtly wrong desynchronises the tree
        // from the text — which shows up as colours drifting, not as an error.
        // A full re-parse is always correct, so it is what anything unusual
        // falls back to.
        let single = {
            let mut deltas = update.deltas();
            match (deltas.next(), deltas.next()) {
                (Some(delta), None) => {
                    let (interval, new_len) = delta.summary();
                    Some(EditSpan {
                        start: interval.start,
                        old_end: interval.end,
                        new_len,
                    })
                }
                _ => None,
            }
        };

        let reused = single.is_some_and(|span| styling_for_edit.reparse_incremental(&text, span));
        if !reused {
            styling_for_edit.reparse(&text);
        }
        revision_for_edit.update(|r| *r += 1);
    });

    let overlay = squiggle_overlay(editor_ref.clone(), styling.clone(), diagnostics, revision);
    // The overlay is absolute, so it takes no part in the stack's layout and
    // simply covers the editor.
    let view = Stack::new((editor, overlay)).style(|s| s.width_full().height_full());

    let handle = EditorHandle {
        path,
        revision,
        diagnostics,
        saved: RwSignal::new(content.to_string()),
        doc,
        editor: editor_ref,
        styling,
    };

    (view, handle)
}

/// Diagnostics as wavy underlines, painted over the text.
///
/// It has to be a painted layer: `Styling` hands floem a cosmic-text `Attrs`,
/// which has no underline and no background, so the colouring layer can only
/// recolour and embolden the characters. The geometry — which is the part that
/// can be got wrong — lives in [`crate::squiggle`] and is tested there. This
/// function does only the two things that cannot be tested: ask floem for the x
/// of a byte offset, and stroke.
///
/// **Three coordinate spaces meet here**, and the transform between them is the
/// classic place this goes wrong; a phantom gutter offset has already cost this
/// project one bug:
///
/// - **document space** is what floem's line geometry speaks. `vline_y` already
///   has the scroll folded in, and x comes from the line's own text layout.
/// - **window space** is `ed.window_origin + (doc - viewport.origin)` — which is
///   exactly the transform floem itself uses to place the IME preedit.
/// - **this canvas's space** is that, less the canvas's own window origin.
///
/// The gutter appears nowhere in that chain, and adding it would be the bug.
/// `ed.window_origin` is the origin of the *content* view — the scroll
/// container to the right of the gutter — so its width is already accounted
/// for. That was the one thing about this job nobody had established; it is
/// settled by reading `editor_content`, which sets `window_origin` from the
/// scroll container's own `VisualChanged`.
fn squiggle_overlay(
    ed: Editor,
    styling: Rc<SyntaxStyling>,
    diagnostics: RwSignal<u64>,
    revision: RwSignal<u64>,
) -> impl IntoView {
    let here = RwSignal::new(Point::ZERO);
    let last_report: RwSignal<Option<String>> = RwSignal::new(None);

    // Resolving a diagnostic to a byte range slices its line out of the rope,
    // and this layer repaints on every scrolled frame. Recompute it only when
    // the diagnostics or the text actually change.
    let ranges = {
        let styling = styling.clone();
        Memo::new(move |_| {
            diagnostics.track();
            revision.track();
            styling.diagnostic_ranges()
        })
    };

    canvas(move |cx, _size| {
        // Every `get` below is a subscription, and that is the whole repaint
        // mechanism: this layer redraws on scroll, resize, edit and on new
        // diagnostics because it reads the signals that carry each of those.
        let viewport = ed.viewport.get();
        let content = ed.window_origin.get();
        let screen_lines = ed.screen_lines.get();
        let origin = here.get();
        let ranges = ranges.get();
        if ranges.is_empty() {
            return;
        }

        let offset = (
            content.x - origin.x - viewport.x0,
            content.y - origin.y - viewport.y0,
        );

        // Clipped to the content area, or a long line scrolled sideways paints
        // its squiggle across the gutter.
        cx.clip(&Rect::from_origin_size(
            (content.x - origin.x, content.y - origin.y),
            viewport.size(),
        ));

        let rows: Vec<VisualRow> = screen_lines
            .iter_line_info()
            .map(|info| VisualRow {
                offsets: info.vline_info.interval.start..info.vline_info.interval.end,
                top: info.vline_y,
                height: f64::from(ed.line_height(info.vline_info.rvline.line)),
            })
            .collect();

        // Nothing here can be seen from the outside, and the failures all look
        // identical: no squiggles. This says which of the four links in the
        // chain is broken — diagnostics resolving to no ranges, no visible
        // rows, no overlap between them, or a transform that put the marks
        // somewhere off-screen. It prints only when the summary changes, so
        // scrolling does not flood the terminal.
        if squiggle_debug() {
            let summary = format!(
                "squiggle: {} ranges, {} rows, {} runs | viewport {:?} content {:?} \
                 overlay {:?} offset ({:.1}, {:.1})",
                ranges.len(),
                rows.len(),
                squiggle::runs(&rows, &ranges).len(),
                viewport.origin(),
                content,
                origin,
                offset.0,
                offset.1
            );
            last_report.with(|previous| {
                if previous.as_deref() != Some(summary.as_str()) {
                    eprintln!("{summary}");
                }
            });
            last_report.set(Some(summary));
        }

        for run in squiggle::runs(&rows, &ranges) {
            // Backward affinity at the end of a run, forward at the start. At a
            // soft-wrap boundary one byte offset is both the end of one visual
            // row and the start of the next, and forward affinity there
            // resolves to column zero of the *next* row — a squiggle that
            // shoots back to the left margin.
            let x0 = ed
                .line_point_of_offset(run.offsets.start, CursorAffinity::Forward)
                .x;
            let x1 = ed
                .line_point_of_offset(run.offsets.end, CursorAffinity::Backward)
                .x;

            let path = squiggle::wave(
                x0 + offset.0,
                x1 + offset.0,
                run.centre + offset.1,
                run.amplitude,
                run.wavelength,
            );
            cx.stroke(
                &path,
                color_for_severity(run.severity),
                &Stroke::new(SQUIGGLE_WEIGHT),
            );
        }
    })
    .on_event_stop(VisualChanged::listener(), move |_, change| {
        here.set(change.visual_window_origin());
    })
    // Without this the overlay sits over the text for hit-testing and swallows
    // every click into the editor.
    .style(|s| s.absolute().inset(0.0).pointer_events_none())
}

/// Stroke width of a squiggle. Thin enough not to compete with the glyphs it
/// runs under, thick enough to survive the wave's steepest slope.
const SQUIGGLE_WEIGHT: f64 = 1.1;

/// `SMITHY_SQUIGGLE_DEBUG=1` reports what the squiggle layer computed.
///
/// In the spirit of `SMITHY_KEY_DEBUG`: nothing about a painted layer can be
/// verified from inside the process, and every way it can fail looks the same
/// from outside. One run with this set beats four rounds of reasoning, which is
/// the trade this project has already lost twice.
fn squiggle_debug() -> bool {
    std::env::var("SMITHY_SQUIGGLE_DEBUG").is_ok_and(|v| v != "0")
}

/// Length of `s` in UTF-16 code units — what LSP means by "character".
pub fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// The file open in the editor changed on disk while it had unsaved edits.
///
/// Only ever raised for the dirty case. A clean buffer is reloaded silently —
/// there is nothing to lose and nothing to ask about, and a prompt for every
/// `git checkout` would train people to dismiss it without reading.
#[derive(Debug, Clone, PartialEq)]
pub struct ExternalChange {
    /// What to call the file. Project-relative, because an absolute path in a
    /// one-line bar is mostly directory.
    pub label: String,
}

/// What an external change to a file should cause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnExternalChange {
    /// Do nothing. Not our file, or the change was our own save.
    Ignore,
    /// Silently take what is on disk.
    Reload,
    /// Both outcomes lose something. Raise the bar.
    Ask,
}

/// Whether a file changing on disk should reload, prompt, or be ignored.
///
/// The whole policy, as one function, so the table below is checked rather than
/// reasoned about — the same shape as `smithy_voice::press`. Every input is a
/// plain `bool` because every one of them is a question the caller has already
/// had to answer:
///
/// | on screen | buffer matches disk | unsaved edits | |
/// |---|---|---|---|
/// | no | — | — | ignore |
/// | yes | yes | — | ignore — this is our own save coming back |
/// | yes | no | no | reload |
/// | yes | no | yes | ask |
///
/// **Matching content is checked before dirtiness, and that ordering is the
/// point.** Smithy writes the file itself, on `⌘S` and on every accepted agent
/// review, and the watcher reports those exactly as it reports anybody else's.
/// Comparing content is what tells them apart, and it does so without the
/// watcher having to track who wrote what — which is what the old `external`
/// flag was attempting, using `!is_file_open`: a test that was backwards, and
/// that nothing populated in any case.
pub fn on_external_change(
    is_the_open_file: bool,
    matches_disk: bool,
    has_unsaved_edits: bool,
) -> OnExternalChange {
    if !is_the_open_file || matches_disk {
        return OnExternalChange::Ignore;
    }
    if has_unsaved_edits {
        OnExternalChange::Ask
    } else {
        OnExternalChange::Reload
    }
}

/// The bar offering to reload a file that changed underneath unsaved edits.
///
/// Both outcomes lose something, so both are named for what they cost rather
/// than for what they do: "Discard my edits" and "Keep my edits" say which text
/// survives. A pair of buttons reading "Reload" and "Cancel" does not, and this
/// is the one prompt in the application where guessing wrong destroys work.
pub fn external_change_bar(
    change: RwSignal<Option<ExternalChange>>,
    on_reload: impl Fn() + 'static,
    on_keep: impl Fn() + 'static,
) -> impl IntoView {
    let on_reload = Rc::new(on_reload);
    let on_keep = Rc::new(on_keep);

    dyn_container(
        move || change.get(),
        move |current| {
            let Some(current) = current else {
                return Box::new(Empty::new().style(|s| s.display(floem::taffy::Display::None)))
                    as Box<dyn View>;
            };
            let reload = on_reload.clone();
            let keep = on_keep.clone();
            let label = current.label.clone();

            let button = |text: &'static str, accent: bool| {
                Button::new(text).style(move |s| {
                    s.font_size(design::TEXT_XS)
                        .padding_horiz(design::SPACE_3)
                        .padding_vert(2.0)
                        .border_radius(design::RADIUS_SM)
                        .margin_left(design::SPACE_2)
                        .background(if accent {
                            design::WARN
                        } else {
                            design::BG_RAISED
                        })
                        .color(if accent { design::BG_BASE } else { design::FG })
                })
            };

            Box::new(
                Stack::horizontal((
                    Label::derived(move || {
                        format!("{label} changed on disk, and you have unsaved edits.")
                    })
                    .style(|s| s.font_size(design::TEXT_XS).color(design::FG)),
                    Container::new(Empty::new()).style(|s| s.flex_grow(1.0)),
                    button("Discard my edits", true)
                        .on_event_stop(floem::event::listener::Click, move |_, _| reload()),
                    button("Keep my edits", false)
                        .on_event_stop(floem::event::listener::Click, move |_, _| keep()),
                ))
                .style(|s| {
                    s.width_full()
                        .items_center()
                        .padding_horiz(design::SPACE_3)
                        .padding_vert(design::SPACE_2)
                        .background(design::BG_RAISED)
                        .border_bottom(1.0)
                        .border_color(design::WARN)
                }),
            ) as Box<dyn View>
        },
    )
}

/// Placeholder shown when no file is open.
/// The empty-editor view, with the project outline behind the shortcuts.
///
/// Crates and modules — not the agent context dump, and not the call graph.
/// Showing the public API here made the pane look like a paste of `cargo doc`
/// output; the navigable map (Benzi-style) is a separate piece of work.
pub fn empty_editor_with_map(
    map: RwSignal<String>,
    aesthetic: RwSignal<Aesthetic>,
) -> impl IntoView {
    // The outer container, not the stack, owns size and the opaque fill.
    //
    // Both children are `absolute`, so they contribute nothing to intrinsic
    // size. A stack that is *only* absolute children collapses to empty under
    // percentage sizing — its `background` paints a zero rect, the circuit
    // backdrop shows through, and the map text (when it arrives) sits on that
    // busy field in ghost colour and reads as missing. Stretching a real
    // container first, then layering inside it, is what makes the pane a pane.
    Container::new(
        Stack::new((
            floem::views::scroll::Scroll::new(Label::derived(move || map.get()).style(|s| {
                s.font_family(design::MONO.to_string())
                    .font_size(design::TEXT_XS)
                    .line_height(1.5)
                    // Faint, not ghost: ghost on the forged backdrop was
                    // indistinguishable from "not there" even after the
                    // opaque fill landed.
                    .color(design::FG_FAINT)
                    .padding(design::SPACE_5)
                    .width_full()
            }))
            .style(move |s| {
                s.absolute()
                    .inset(0.0)
                    .width_full()
                    .height_full()
                    .min_width(0.0)
                    .apply_if(map.get().trim().is_empty(), |s| {
                        s.display(floem::taffy::Display::None)
                    })
            }),
            // Shortcuts float over the map. `shortcut_list` rather than
            // `empty_editor`, because that one paints an opaque background and
            // would hide the very thing this function exists to show.
            Container::new(shortcut_list())
                .style(|s| s.absolute().inset(0.0).items_center().justify_center()),
        ))
        .style(|s| s.width_full().height_full()),
    )
    .style(move |s| {
        s.width_full()
            .height_full()
            .min_height(0.0)
            .background(editor_pane_fill(aesthetic.get()))
    })
}

/// Just the shortcut list, with no background of its own.
fn shortcut_list() -> impl IntoView {
    // `design::SYMBOL`, not the bare generic. These keycaps did render as
    // missing-glyph boxes, but dropping the family was the wrong fix and did not
    // help: the generic `monospace` resolved to Courier, which has no ⌘ (U+2318)
    // or ⌃ (U+2303) — and neither does the default sans you get with no family
    // set at all. Menlo has both. See `design::SYMBOL`.
    let shortcut = |keys: String, what: &'static str| {
        Stack::horizontal((
            Label::derived(move || keys.clone()).style(|s| {
                s.font_size(design::TEXT_XS)
                    .font_family(design::SYMBOL.to_string())
                    .color(design::FG_MUTED)
                    .background(design::BG_RAISED)
                    .padding_horiz(design::SPACE_2)
                    .padding_vert(2.0)
                    .border_radius(design::RADIUS_SM)
                    .width(74.0)
                    .justify_center()
            }),
            Label::derived(move || what.to_string()).style(|s| {
                s.font_size(design::TEXT_SM)
                    .color(design::FG_FAINT)
                    .margin_left(design::SPACE_3)
            }),
        ))
        .style(|s| s.items_center().margin_bottom(design::SPACE_2))
    };

    Stack::vertical((
        Label::derived(|| "No file open".to_string()).style(|s| {
            s.font_size(design::TEXT_LG)
                .color(design::FG_MUTED)
                .margin_bottom(design::SPACE_5)
        }),
        shortcut("dbl-click".to_string(), "open a file from the Explorer"),
        shortcut(crate::menu_bar::accel("O"), "open a project"),
        shortcut(crate::menu_bar::accel("B"), "toggle the Explorer"),
        shortcut(crate::menu_bar::accel("L"), "toggle the agent"),
        // Control on every platform, which is the convention for this one and
        // what the handler matches.
        shortcut("⌃`".to_string(), "toggle the terminal"),
    ))
    .style(|s| s.items_center().justify_center())
}

/// The empty-editor pane with no project map behind it.
pub fn empty_editor() -> impl IntoView {
    Container::new(shortcut_list()).style(|s| {
        s.width_full()
            .height_full()
            .items_center()
            .justify_center()
            .background(design::BG_BASE)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// LSP columns are UTF-16 code units, which is neither bytes nor chars.
    /// Getting this wrong points hover at the wrong token on any line
    /// containing non-BMP text.
    #[test]
    fn utf16_length_is_not_bytes_or_chars() {
        assert_eq!(utf16_len("abc"), 3);
        // é is 2 bytes, 1 char, 1 UTF-16 unit.
        assert_eq!("é".len(), 2);
        assert_eq!(utf16_len("é"), 1);
        // 🦀 is 4 bytes, 1 char, 2 UTF-16 units.
        assert_eq!("🦀".len(), 4);
        assert_eq!("🦀".chars().count(), 1);
        assert_eq!(utf16_len("🦀"), 2);
    }

    #[test]
    fn utf16_length_of_empty_is_zero() {
        assert_eq!(utf16_len(""), 0);
    }

    #[test]
    fn utf16_length_accumulates_across_a_mixed_line() {
        assert_eq!(utf16_len("let x = \"🦀\";"), "let x = \"".len() + 2 + 2);
    }

    /// The policy table in `on_external_change`'s own documentation.
    #[test]
    fn an_external_change_reloads_prompts_or_is_ignored_per_the_table() {
        use OnExternalChange::*;

        // Not the file on screen: never anything to do, whatever its state.
        assert_eq!(on_external_change(false, false, false), Ignore);
        assert_eq!(on_external_change(false, false, true), Ignore);

        // On screen and already agrees with disk.
        assert_eq!(on_external_change(true, true, false), Ignore);

        // Genuinely changed underneath us.
        assert_eq!(on_external_change(true, false, false), Reload);
        assert_eq!(on_external_change(true, false, true), Ask);
    }

    /// **Smithy's own saves must not come back as external changes.** The
    /// watcher reports the editor's `⌘S` and every accepted agent review exactly
    /// as it reports anybody else's write, so without this the act of saving
    /// would prompt you about the file you just saved.
    ///
    /// Content is what distinguishes them, and it is checked before dirtiness —
    /// so this holds even for a buffer that is dirty again by the time the event
    /// arrives.
    #[test]
    fn the_editors_own_save_is_not_reported_back_as_an_external_change() {
        assert_eq!(
            on_external_change(true, true, false),
            OnExternalChange::Ignore
        );
        assert_eq!(
            on_external_change(true, true, true),
            OnExternalChange::Ignore,
            "matching content settles it; dirtiness is only consulted when they differ"
        );
    }

    /// Unsaved work is never discarded without being asked about. This is the
    /// one prompt in the application where guessing wrong destroys work.
    #[test]
    fn a_file_with_unsaved_edits_is_never_reloaded_silently() {
        assert_ne!(
            on_external_change(true, false, true),
            OnExternalChange::Reload,
            "reloading over unsaved edits without asking loses them with no undo"
        );
    }
}
