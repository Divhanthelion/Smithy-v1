//! Hover results from the language server.
//!
//! Deliberately triggered on demand (a key or a menu item) rather than on mouse
//! dwell. Dwell-triggered popups need mouse tracking over the editor's glyphs,
//! which floem's editor owns — and a popup that appears because your cursor
//! rested somewhere is as often an obstruction as a help.

use floem::prelude::*;
use floem::reactive::{RwSignal, SignalGet, SignalUpdate};
use std::path::PathBuf;

use crate::design;

/// The exact document generation a semantic answer belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HoverDocument {
    pub epoch: u64,
    pub path: PathBuf,
    pub revision: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingHover {
    pub request_id: u64,
    pub document: HoverDocument,
}

/// What the popup shows.
#[derive(Clone)]
pub struct HoverState {
    /// Markdown-ish text from the server. `None` when nothing is showing.
    pub content: RwSignal<Option<String>>,
    /// The one request allowed to change this popup.
    ///
    /// A bool cannot distinguish two in-flight requests, so an older response
    /// could replace the answer for the caret's newer position.
    pub pending: RwSignal<Option<PendingHover>>,
    document: RwSignal<Option<HoverDocument>>,
}

impl HoverState {
    pub fn new() -> Self {
        Self {
            content: RwSignal::new(None),
            pending: RwSignal::new(None),
            document: RwSignal::new(None),
        }
    }

    /// Bind the popup to what is currently active.
    ///
    /// Path alone is insufficient: an answer requested before an edit is stale
    /// for the same file, and a project transition can retain the same path.
    pub fn bind_document(&self, document: Option<HoverDocument>) {
        if self.document.get_untracked() != document {
            self.pending.set(None);
            self.content.set(None);
            self.document.set(document);
        }
    }

    pub fn request_started(&self, request_id: u64, document: HoverDocument) {
        self.bind_document(Some(document.clone()));
        self.pending.set(Some(PendingHover {
            request_id,
            document,
        }));
        self.content.set(None);
    }

    /// Show a result. An empty or whitespace-only body is *not* shown — the
    /// server says "I have nothing" by returning empty contents, and an empty
    /// box on screen reads as a bug.
    pub fn show(&self, request_id: u64, content: Option<String>) -> bool {
        let Some(pending) = self.pending.get_untracked() else {
            return false;
        };
        if pending.request_id != request_id
            || self.document.get_untracked().as_ref() != Some(&pending.document)
        {
            return false;
        }
        self.pending.set(None);
        self.content.set(match content {
            Some(text) if !text.trim().is_empty() => Some(clean_markup(&text)),
            _ => None,
        });
        true
    }

    /// Finish only the matching request without declaring the server dead.
    pub fn fail(&self, request_id: u64) -> bool {
        if self
            .pending
            .get_untracked()
            .is_none_or(|pending| pending.request_id != request_id)
        {
            return false;
        }
        self.pending.set(None);
        self.content.set(None);
        true
    }

    pub fn dismiss(&self) {
        self.pending.set(None);
        self.content.set(None);
    }

    pub fn is_visible(&self) -> bool {
        self.pending.get().is_some() || self.content.get().is_some()
    }
}

impl Default for HoverState {
    fn default() -> Self {
        Self::new()
    }
}

/// Tidy the server's markdown into something worth putting in a small box.
///
/// rust-analyzer wraps signatures in fenced code blocks and separates sections
/// with horizontal rules. The fences and rules are noise at this size; the text
/// inside them is the whole point.
pub fn clean_markup(raw: &str) -> String {
    let mut out = Vec::new();
    let mut blank_run = 0usize;

    for line in raw.lines() {
        let trimmed = line.trim_end();
        if trimmed.starts_with("```") || trimmed == "---" || trimmed == "___" {
            continue;
        }
        if trimmed.trim().is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        out.push(trimmed.to_string());
    }

    // Long docs are a scroll, not a hover. Keep the signature and the first
    // paragraph, which is what you actually looked for.
    const MAX_LINES: usize = 24;
    if out.len() > MAX_LINES {
        out.truncate(MAX_LINES);
        out.push("…".into());
    }
    out.join("\n").trim().to_string()
}

/// The popup, anchored to the top-right of the editor pane.
///
/// Not anchored at the caret: getting a caret's screen position out of floem's
/// editor means reaching into its layout, and a fixed corner is honest and
/// never covers the line you are reading.
pub fn hover_popup(state: HoverState) -> impl IntoView {
    let for_dismiss = state.clone();
    let for_body = state.clone();
    let for_style = state.clone();

    Container::new(
        Stack::vertical((
            Stack::horizontal((
                Label::derived(|| "Hover".to_string()).style(|s| {
                    s.font_size(design::TEXT_XS)
                        .font_bold()
                        .color(design::FG_FAINT)
                }),
                Container::new(Empty::new()).style(|s| s.flex_grow(1.0)),
                Label::derived(|| "✕".to_string())
                    .on_event_stop(floem::event::listener::Click, move |_, _| {
                        for_dismiss.dismiss()
                    })
                    .style(|s| {
                        s.font_size(design::TEXT_XS)
                            .color(design::FG_FAINT)
                            .padding_horiz(design::SPACE_1)
                            .border_radius(design::RADIUS_SM)
                            .hover(|s| s.background(design::BG_RAISED).color(design::FG))
                    }),
            ))
            .style(|s| s.width_full().items_center().margin_bottom(design::SPACE_2)),
            Label::derived(move || {
                if for_body.pending.get().is_some() {
                    "…".to_string()
                } else {
                    for_body.content.get().unwrap_or_default()
                }
            })
            .style(|s| {
                s.font_size(design::TEXT_SM)
                    .font_family(design::MONO.to_string())
                    .color(design::FG)
                    .line_height(1.5)
                    .width_full()
            }),
        ))
        .style(|s| {
            s.max_width(520.0)
                .padding(design::SPACE_3)
                .background(design::BG_FLOAT)
                .border(1.0)
                .border_color(design::BORDER)
                .border_radius(design::RADIUS_LG)
        }),
    )
    .style(move |s| {
        if for_style.is_visible() {
            s.absolute()
                .inset_top(design::SPACE_4)
                .inset_right(design::SPACE_4)
                .z_index(400)
        } else {
            s.display(floem::taffy::Display::None)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(path: &str, revision: u64) -> HoverDocument {
        HoverDocument {
            epoch: 1,
            path: PathBuf::from(path),
            revision,
        }
    }

    #[test]
    fn code_fences_and_rules_are_stripped() {
        let raw = "```rust\npub fn parse(s: &str) -> Result<Ast>\n```\n\n---\n\nParses input.";
        let cleaned = clean_markup(raw);
        assert!(!cleaned.contains("```"));
        assert!(!cleaned.contains("---"));
        assert!(cleaned.contains("pub fn parse"));
        assert!(cleaned.contains("Parses input."));
    }

    #[test]
    fn runs_of_blank_lines_collapse() {
        assert_eq!(clean_markup("a\n\n\n\n\nb"), "a\n\nb");
    }

    #[test]
    fn long_documentation_is_truncated_with_a_marker() {
        let raw = (0..100)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let cleaned = clean_markup(&raw);
        assert!(cleaned.lines().count() <= 25);
        assert!(cleaned.ends_with('…'));
    }

    #[test]
    fn short_documentation_is_untouched() {
        assert_eq!(clean_markup("just this"), "just this");
    }

    /// The server signals "nothing here" with empty contents. Rendering an
    /// empty box for that reads as a broken popup.
    #[test]
    fn an_empty_result_shows_nothing() {
        let state = HoverState::new();
        state.request_started(1, document("a.rs", 0));
        state.show(1, Some("   \n\n  ".into()));
        assert!(!state.is_visible());
        assert_eq!(state.content.get_untracked(), None);
    }

    #[test]
    fn a_missing_result_shows_nothing() {
        let state = HoverState::new();
        state.request_started(1, document("a.rs", 0));
        state.show(1, None);
        assert!(!state.is_visible());
    }

    /// Pending must be visible, so an in-flight request looks different from
    /// "the server had nothing to say".
    #[test]
    fn a_pending_request_is_visible() {
        let state = HoverState::new();
        assert!(!state.is_visible());
        state.request_started(1, document("a.rs", 0));
        assert!(state.is_visible());
    }

    #[test]
    fn showing_a_result_clears_pending() {
        let state = HoverState::new();
        state.request_started(1, document("a.rs", 0));
        state.show(1, Some("fn main()".into()));
        assert!(state.pending.get_untracked().is_none());
        assert!(state.is_visible());
    }

    #[test]
    fn dismissing_clears_everything() {
        let state = HoverState::new();
        state.request_started(1, document("a.rs", 0));
        state.show(1, Some("something".into()));
        state.dismiss();
        assert!(!state.is_visible());
    }

    /// A second request must not leave the previous answer on screen, which
    /// would look like an instant — and wrong — response.
    #[test]
    fn a_new_request_clears_the_previous_answer() {
        let state = HoverState::new();
        state.request_started(1, document("a.rs", 0));
        state.show(1, Some("first".into()));
        state.request_started(2, document("a.rs", 0));
        assert_eq!(state.content.get_untracked(), None);
        assert_eq!(
            state
                .pending
                .get_untracked()
                .map(|pending| pending.request_id),
            Some(2)
        );
    }

    /// Network replies can arrive in the opposite order from requests. The
    /// older answer must not overwrite the caret's newer hover.
    #[test]
    fn a_late_hover_response_cannot_replace_the_current_request() {
        let state = HoverState::new();
        state.request_started(10, document("a.rs", 0));
        state.request_started(11, document("a.rs", 0));

        assert!(!state.show(10, Some("old".into())));
        assert_eq!(
            state
                .pending
                .get_untracked()
                .map(|pending| pending.request_id),
            Some(11)
        );
        assert!(state.show(11, Some("new".into())));
        assert_eq!(state.content.get_untracked().as_deref(), Some("new"));
    }

    /// Dismissal is a decision that the in-flight answer is no longer wanted.
    /// Clearing only the pixels let the late response reopen the popup.
    #[test]
    fn dismissing_a_pending_hover_invalidates_its_late_response() {
        let state = HoverState::new();
        state.request_started(20, document("a.rs", 0));
        state.dismiss();

        assert!(!state.show(20, Some("too late".into())));
        assert!(!state.is_visible());
    }

    /// One hover can fail while rust-analyzer remains healthy for diagnostics,
    /// definition and later hovers. The failure belongs only to its request.
    #[test]
    fn a_hover_error_clears_only_the_matching_request() {
        let state = HoverState::new();
        state.request_started(30, document("a.rs", 0));
        assert!(state.fail(30));
        assert!(!state.is_visible());

        state.request_started(31, document("a.rs", 0));
        assert!(!state.fail(30));
        assert_eq!(
            state
                .pending
                .get_untracked()
                .map(|pending| pending.request_id),
            Some(31)
        );
    }

    /// Request ids are process-wide but an answer also belongs to the document
    /// generation at the caret. Switching files must invalidate the old file's
    /// answer before it arrives.
    #[test]
    fn switching_documents_rejects_a_late_old_file_hover() {
        let state = HoverState::new();
        state.request_started(40, document("old.rs", 0));
        state.bind_document(Some(document("new.rs", 0)));

        assert!(!state.show(40, Some("old file".into())));
        assert!(!state.is_visible());
    }

    /// A hover computed before an edit describes the old syntax even when the
    /// path did not move. Revision changes therefore invalidate it too.
    #[test]
    fn editing_the_active_document_rejects_its_pre_edit_hover() {
        let state = HoverState::new();
        state.request_started(50, document("same.rs", 7));
        state.bind_document(Some(document("same.rs", 8)));

        assert!(!state.show(50, Some("revision seven".into())));
        assert!(!state.is_visible());
    }

    /// Project switches can retain an identical absolute path in synthetic or
    /// replaced workspaces. The project epoch prevents that answer crossing the
    /// semantic boundary.
    #[test]
    fn changing_project_epoch_rejects_a_late_hover_for_the_same_path() {
        let state = HoverState::new();
        state.request_started(60, document("same.rs", 0));
        let mut next_project = document("same.rs", 0);
        next_project.epoch = 2;
        state.bind_document(Some(next_project));

        assert!(!state.show(60, Some("previous project".into())));
        assert!(!state.is_visible());
    }
}
