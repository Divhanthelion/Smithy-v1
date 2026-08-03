//! Main Layout module - Split view with editor, terminal, and chat panels
//!
//! This module provides the main application layout including:
//! - Tab bar for open buffers with dirty indicators
//! - Editor panel for text editing
//! - Terminal panel (toggleable)
//! - Chat panel (toggleable right sidebar)
//! - Resizable split views

use floem::peniko::Color;
use floem::prelude::*;
use floem::reactive::{RwSignal, SignalGet};
use floem::views::Decorators;

use crate::buffer::BufferId;
use crate::buffer_manager::BufferState;
use crate::theme::catppuccin;

/// Theme colors for the main layout
#[derive(Clone, Copy)]
pub struct LayoutTheme {
    /// Background color for the tab bar
    pub tab_bar_background: Color,
    /// Background color for inactive tabs
    pub tab_inactive_background: Color,
    /// Background color for active tabs
    pub tab_active_background: Color,
    /// Text color for inactive tabs
    pub tab_inactive_text: Color,
    /// Text color for active tabs
    pub tab_active_text: Color,
    /// Dirty indicator color
    pub dirty_indicator: Color,
    /// Close button color
    pub close_button: Color,
    /// Close button hover color
    pub close_button_hover: Color,
    /// Panel separator color
    pub separator: Color,
    /// Panel resize handle color
    pub resize_handle: Color,
    /// Panel resize handle hover color
    pub resize_handle_hover: Color,
}

impl Default for LayoutTheme {
    fn default() -> Self {
        Self {
            tab_bar_background: catppuccin::MANTLE,
            tab_inactive_background: catppuccin::MANTLE,
            tab_active_background: catppuccin::BASE,
            tab_inactive_text: catppuccin::OVERLAY0,
            tab_active_text: catppuccin::TEXT,
            dirty_indicator: catppuccin::YELLOW,
            close_button: catppuccin::OVERLAY0,
            close_button_hover: catppuccin::RED,
            separator: catppuccin::SURFACE0,
            resize_handle: catppuccin::SURFACE0,
            resize_handle_hover: catppuccin::BLUE,
        }
    }
}

// `MainLayoutState` used to live here: a struct holding `terminal_visible`,
// `chat_visible`, `sidebar_visible`, nine panel ratios, the active buffer id and
// the buffer list, behind an `Rc<RefCell<_>>`, with setters and togglers for all of
// it.
//
// **Nothing read any of it.** Panel visibility lives in `RwSignal<bool>`s that
// `main.rs` owns and toggles; `main_layout_view` took those as separate arguments
// and never consulted the struct. Panel sizing is done by `DragState` and size
// signals, not by the ratios. The buffer list and active id come from their own
// signals.
//
// Worse, three reactive `Effect`s existed purely to copy the signal values *into*
// the struct — so every panel toggle ran three effects to maintain a shadow copy
// with no consumer. That is the same shape as the defect that once let the tab bar
// and the editor disagree about dirty state, caught before it could diverge.
//
// Its tests were the pattern HANDOFF §8 warns about: `test_main_layout_state_toggle_sidebar`
// asserted that `toggle_sidebar` flipped a boolean. It did, forever, over code
// nothing called.
//
// The layout needs one thing, and takes it directly.

/// Create a single tab view for a buffer
fn tab_view(
    buffer_state: BufferState,
    is_active: bool,
    theme: LayoutTheme,
    on_click: impl Fn(BufferId) + 'static + Clone,
    on_close: impl Fn(BufferId) + 'static + Clone,
) -> impl IntoView {
    let buffer_id = buffer_state.id;
    let name = buffer_state.name.clone();
    let is_dirty = buffer_state.is_dirty;

    let on_click_clone = on_click.clone();
    let on_close_clone = on_close.clone();

    // Tab container
    Stack::horizontal((
        // Dirty indicator (dot before name)
        Label::derived(move || if is_dirty { "● " } else { "" }.to_string()).style(move |s| {
            s.font_size(10.0)
                .color(theme.dirty_indicator)
                .margin_right(4.0)
        }),
        // Tab name
        Label::derived(move || name.clone()).style(move |s| {
            let text_color = if is_active {
                theme.tab_active_text
            } else {
                theme.tab_inactive_text
            };
            s.font_size(12.0).color(text_color)
        }),
        // Close button
        Label::derived(move || "×".to_string())
            .style(move |s| {
                s.font_size(14.0)
                    .color(theme.close_button)
                    .margin_left(8.0)
                    .padding(2.0)
                    .border_radius(4.0)
                    .hover(|s| {
                        s.color(theme.close_button_hover)
                            .background(Color::from_rgba8(255, 255, 255, 20))
                    })
            })
            .on_event_stop(floem::event::listener::Click, move |_, _| {
                on_close_clone(buffer_id);
            }),
    ))
    .style(move |s| {
        let bg_color = if is_active {
            theme.tab_active_background
        } else {
            theme.tab_inactive_background
        };
        s.padding_horiz(14.0)
            .padding_vert(6.0)
            .background(bg_color)
            .border_radius(6.0)
            .margin_top(4.0)
            .margin_horiz(2.0)
            .items_center()
            .cursor(floem::style::CursorStyle::Pointer)
            .hover(|s: floem::style::Style| {
                if !is_active {
                    s.background(catppuccin::SURFACE0)
                } else {
                    s
                }
            })
    })
    .on_event_stop(floem::event::listener::Click, move |_, _| {
        on_click_clone(buffer_id);
    })
}

/// Create the tab bar view
pub fn tab_bar_view(
    buffer_states_signal: RwSignal<Vec<BufferState>>,
    active_buffer_signal: RwSignal<Option<BufferId>>,
    theme: LayoutTheme,
    on_tab_click: impl Fn(BufferId) + 'static + Clone,
    on_tab_close: impl Fn(BufferId) + 'static + Clone,
) -> impl IntoView {
    let on_click = on_tab_click.clone();
    let on_close = on_tab_close.clone();

    Stack::horizontal((
        dyn_stack(
            move || {
                let states = buffer_states_signal.get();
                let active_id = active_buffer_signal.get();
                states
                    .into_iter()
                    .map(|state| {
                        let is_active = active_id == Some(state.id);
                        // The key must include everything the row *renders*,
                        // not just its identity. `tab_view` reads `is_dirty` and
                        // `is_active` by value at build time, so keying on the
                        // id alone means a tab that becomes dirty is never
                        // rebuilt and the indicator never appears.
                        ((state.id, state.is_dirty, is_active), state, is_active)
                    })
                    .collect::<Vec<_>>()
            },
            |(key, _, _)| *key,
            move |(_, state, is_active)| {
                tab_view(state, is_active, theme, on_click.clone(), on_close.clone())
            },
        )
        .style(|s| s.flex_row()),
        Empty::new().style(|s| s.flex_grow(1.0)),
    ))
    // Collapse entirely when nothing is open. The strip previously held a
    // "Show Terminal" link that duplicated View → Terminal, so with no files
    // open it was an empty 40px band with one floating word in it.
    .style(move |s| {
        let has_tabs = !buffer_states_signal.get().is_empty();
        s.width_full()
            .apply_if(!has_tabs, |s| s.display(floem::taffy::Display::None))
            .height(crate::design::TAB_BAR_HEIGHT)
            .background(theme.tab_bar_background)
            .border_bottom(1.0)
            .border_color(theme.separator)
            .items_center()
    })
}

/// Which panel a drag is resizing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragTarget {
    Sidebar,
    Terminal,
    Chat,
}

/// An in-progress panel resize.
///
/// `start_pos` is filled in on the first pointer move rather than on the press,
/// because a press is delivered to the *divider*, whose coordinate space moves
/// with it. Every measurement has to come from one fixed frame of reference, so
/// they all come from the layout root.
#[derive(Debug, Clone, Copy)]
pub struct ActiveDrag {
    pub target: DragTarget,
    pub start_pos: Option<f64>,
    pub start_size: f64,
}

/// Shared resize state, owned by the layout and read by the root handler.
#[derive(Clone, Copy)]
pub struct DragState {
    pub active: RwSignal<Option<ActiveDrag>>,
}

impl DragState {
    pub fn new() -> Self {
        Self {
            active: RwSignal::new(None),
        }
    }

    pub fn is_dragging(&self, target: DragTarget) -> bool {
        self.active
            .get()
            .map(|d| d.target == target)
            .unwrap_or(false)
    }

    pub fn begin(&self, target: DragTarget, start_size: f64) {
        self.active.set(Some(ActiveDrag {
            target,
            start_pos: None,
            start_size,
        }));
    }

    pub fn end(&self) {
        self.active.set(None);
    }
}

impl Default for DragState {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolve a pointer position into a new panel size.
///
/// Pure so it can be tested without a window — the geometry is the part that
/// was wrong, and it is the part a view test could never reach.
pub fn resize_to(drag: &ActiveDrag, pos: f64, min: f64, max: f64, invert: bool) -> f64 {
    let Some(start) = drag.start_pos else {
        return drag.start_size;
    };
    let delta = pos - start;
    let raw = if invert {
        drag.start_size - delta
    } else {
        drag.start_size + delta
    };
    raw.clamp(min, max)
}

/// A vertical grab handle between two side-by-side panels.
///
/// The handle only *starts* the drag. Tracking happens on the layout root, via
/// [`layout_drag_handlers`] — binding `PointerMove` to the handle itself meant
/// the drag stopped the moment the cursor outran a four-pixel strip, which is
/// immediately.
pub fn h_divider(drag: DragState, target: DragTarget, size_signal: RwSignal<f64>) -> impl IntoView {
    Empty::new()
        .style(move |s| {
            s.width(5.0)
                .height_full()
                .cursor(floem::style::CursorStyle::ColResize)
                .background(if drag.is_dragging(target) {
                    crate::design::ACCENT
                } else {
                    crate::design::BORDER_SUBTLE
                })
                .hover(|s| s.background(crate::design::ACCENT))
        })
        .on_event_stop(floem::event::listener::PointerDown, move |_, _| {
            drag.begin(target, size_signal.get_untracked());
        })
}

/// A horizontal grab handle between stacked panels.
pub fn v_divider(drag: DragState, target: DragTarget, size_signal: RwSignal<f64>) -> impl IntoView {
    Empty::new()
        .style(move |s| {
            s.height(5.0)
                .width_full()
                .cursor(floem::style::CursorStyle::RowResize)
                .background(if drag.is_dragging(target) {
                    crate::design::ACCENT
                } else {
                    crate::design::BORDER_SUBTLE
                })
                .hover(|s| s.background(crate::design::ACCENT))
        })
        .on_event_stop(floem::event::listener::PointerDown, move |_, _| {
            drag.begin(target, size_signal.get_untracked());
        })
}

/// Create the main layout with editor, terminal, and chat panels
///
/// This creates a layout with:
/// - Tab bar at the top
/// - Main content area with horizontal split:
///   - Left: Editor + Terminal (vertical split)
///   - Right: Chat panel (optional sidebar)
///
/// The argument count is the panel set plus its visibility signals. A params
/// struct would relocate the list rather than shorten it.
#[allow(clippy::too_many_arguments)]
pub fn main_layout_view<SV, EV, TV, CV>(
    theme: LayoutTheme,
    buffer_states_signal: RwSignal<Vec<BufferState>>,
    active_buffer_signal: RwSignal<Option<BufferId>>,
    sidebar_visible_signal: RwSignal<bool>,
    terminal_visible_signal: RwSignal<bool>,
    chat_visible_signal: RwSignal<bool>,
    sidebar_view: SV,
    editor_view: EV,
    terminal_view: TV,
    chat_view: CV,
    on_tab_click: impl Fn(BufferId) + 'static + Clone,
    on_tab_close: impl Fn(BufferId) + 'static + Clone,
) -> impl IntoView
where
    SV: IntoView + 'static,
    EV: IntoView + 'static,
    TV: IntoView + 'static,
    CV: IntoView + 'static,
{
    // Width/Height signals for resizable panels
    let sidebar_width = RwSignal::new(220.0f64);
    let terminal_height = RwSignal::new(200.0f64);
    // 420 rather than 350. At 350 the agent panel is the most cramped thing on
    // screen and it is where the work actually happens: the header has eight
    // controls in a row, tool rows carry a name plus an argument summary, and
    // reasoning is prose. It is still draggable — this is only where it starts.
    let chat_width = RwSignal::new(420.0f64);

    // Dragging state signals
    // One shared drag state: only one divider can be dragged at a time, and the
    // root handler needs to know which.
    let drag = DragState::new();

    Stack::vertical((
        // Tab bar
        tab_bar_view(
            buffer_states_signal,
            active_buffer_signal,
            theme,
            on_tab_click,
            on_tab_close,
        ),
        // Main content area
        Stack::horizontal((
            // Left sidebar
            Container::new(sidebar_view).style(move |s| {
                if sidebar_visible_signal.get() {
                    let width = sidebar_width.get();
                    s.height_full().width(width as f32)
                } else {
                    s.width(0.0)
                        .height(0.0)
                        .display(floem::style::Display::None)
                }
            }),
            // Sidebar resize divider
            Container::new(h_divider(drag, DragTarget::Sidebar, sidebar_width)).style(move |s| {
                if sidebar_visible_signal.get() {
                    s.height_full()
                } else {
                    s.display(floem::style::Display::None)
                }
            }),
            // Center content: Editor + Terminal
            Stack::vertical((
                // Editor panel
                Container::new(editor_view).style(move |s| {
                    s.width_full()
                        .flex_grow(1.0)
                        .flex_basis(0.0)
                        .min_height(0.0)
                }),
                // Terminal resize divider
                Container::new(v_divider(drag, DragTarget::Terminal, terminal_height)).style(
                    move |s| {
                        if terminal_visible_signal.get() {
                            s.width_full()
                        } else {
                            s.display(floem::style::Display::None)
                        }
                    },
                ),
                // Terminal panel (conditionally visible)
                Container::new(terminal_view).style(move |s| {
                    if terminal_visible_signal.get() {
                        let height = terminal_height.get();
                        s.width_full()
                            .height(height as f32)
                            .min_height(100.0)
                            .border_top(1.0)
                            .border_color(theme.separator)
                    } else {
                        s.width(0.0)
                            .height(0.0)
                            .display(floem::style::Display::None)
                    }
                }),
            ))
            .style(move |s| {
                s.height_full()
                    .flex_grow(1.0)
                    .flex_basis(0.0)
                    .flex_col()
                    .min_width(200.0)
            }),
            // Chat resize divider
            Container::new(h_divider(drag, DragTarget::Chat, chat_width)).style(move |s| {
                if chat_visible_signal.get() {
                    s.height_full()
                } else {
                    s.display(floem::style::Display::None)
                }
            }),
            // Right side: Chat panel (conditionally visible)
            Container::new(chat_view).style(move |s| {
                if chat_visible_signal.get() {
                    let width = chat_width.get();
                    s.height_full()
                        .min_height(0.0)
                        // `min_width: auto` is the flexbox default, and it refuses
                        // to shrink an item below its content's intrinsic width —
                        // so a long unwrapped line in the agent panel pushed the
                        // panel wider than the width set right here, and the text
                        // ran off the right edge of the window. The same trick as
                        // `min_height(0.0)` on the line above, which was already
                        // needed for the vertical case.
                        .min_width(0.0)
                        .width(width as f32)
                        .border_left(1.0)
                        .border_color(theme.separator)
                } else {
                    s.width(0.0)
                        .height(0.0)
                        .display(floem::style::Display::None)
                }
            }),
        ))
        .style(|s| {
            s.width_full()
                .flex_grow(1.0)
                .flex_basis(0.0)
                .flex_row()
                .min_height(0.0)
        }),
    ))
    // Tracking lives here rather than on the handle: the root spans the window,
    // so it keeps receiving moves however fast the cursor travels, and its
    // coordinate space does not shift as the panel resizes.
    .on_event_stop(floem::event::listener::PointerMove, move |_, update| {
        let Some(mut active) = drag.active.get_untracked() else {
            return;
        };
        // `logical_point` divides out the display scale factor; the raw
        // `position` is physical, which would move the divider at double speed
        // on a retina display.
        let ptr = update.current.logical_point();

        let (pos, size_signal, min, max, invert) = match active.target {
            DragTarget::Sidebar => (ptr.x, sidebar_width, 150.0, 600.0, false),
            // The 1100 ceiling is deliberate: on a wide display the agent panel
            // is often the thing you want most of the window given to, and 800
            // stopped short of that.
            DragTarget::Chat => (ptr.x, chat_width, 280.0, 1100.0, true),
            DragTarget::Terminal => (ptr.y, terminal_height, 80.0, 800.0, true),
        };

        // The press lands on the divider, whose coordinates are its own; anchor
        // on the first move instead, in the root's frame.
        if active.start_pos.is_none() {
            active.start_pos = Some(pos);
            drag.active.set(Some(active));
            return;
        }
        size_signal.set(resize_to(&active, pos, min, max, invert));
    })
    .on_event_stop(floem::event::listener::PointerUp, move |_, _| drag.end())
    .style(|s| s.width_full().height_full().flex_col().min_height(0.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_layout_theme_is_fully_populated() {
        let theme = LayoutTheme::default();
        assert_eq!(theme.tab_bar_background, catppuccin::MANTLE);
        assert_eq!(theme.tab_active_background, catppuccin::BASE);
    }

    // Panel context tests

    // Chat panel tests

    // Panel command tests

    // Keybinding tests
    // --- panel resize geometry ---
    //
    // These cover the part that was broken: the arithmetic. The handle bug was
    // that `PointerMove` was bound to a four-pixel strip and measured against a
    // coordinate space that moved with the divider, so drags died instantly and
    // the delta was computed against a moving origin. The pure function below is
    // now the whole calculation.

    fn drag_at(start_pos: f64, start_size: f64, target: DragTarget) -> ActiveDrag {
        ActiveDrag {
            target,
            start_pos: Some(start_pos),
            start_size,
        }
    }

    #[test]
    fn dragging_right_widens_a_left_anchored_panel() {
        let drag = drag_at(200.0, 220.0, DragTarget::Sidebar);
        assert_eq!(resize_to(&drag, 260.0, 150.0, 600.0, false), 280.0);
    }

    #[test]
    fn dragging_left_narrows_a_left_anchored_panel() {
        let drag = drag_at(200.0, 220.0, DragTarget::Sidebar);
        assert_eq!(resize_to(&drag, 150.0, 150.0, 600.0, false), 170.0);
    }

    /// A right-anchored panel grows when the cursor moves *left*.
    #[test]
    fn a_right_anchored_panel_inverts() {
        let drag = drag_at(900.0, 350.0, DragTarget::Chat);
        assert_eq!(resize_to(&drag, 800.0, 250.0, 800.0, true), 450.0);
        assert_eq!(resize_to(&drag, 1000.0, 250.0, 800.0, true), 250.0);
    }

    /// Likewise a bottom-anchored panel grows when the cursor moves *up*.
    #[test]
    fn a_bottom_anchored_panel_inverts() {
        let drag = drag_at(500.0, 200.0, DragTarget::Terminal);
        assert_eq!(resize_to(&drag, 400.0, 80.0, 800.0, true), 300.0);
    }

    #[test]
    fn sizes_are_clamped_at_both_ends() {
        let drag = drag_at(200.0, 220.0, DragTarget::Sidebar);
        assert_eq!(resize_to(&drag, 5000.0, 150.0, 600.0, false), 600.0);
        assert_eq!(resize_to(&drag, -5000.0, 150.0, 600.0, false), 150.0);
    }

    /// A panel can never be dragged shut, which would make it unrecoverable
    /// without the menu.
    #[test]
    fn a_panel_cannot_be_dragged_to_nothing() {
        let drag = drag_at(200.0, 220.0, DragTarget::Sidebar);
        assert!(resize_to(&drag, -1.0e9, 150.0, 600.0, false) >= 150.0);
    }

    /// Before the first move there is no anchor, so the size must not jump.
    #[test]
    fn an_unanchored_drag_does_not_move_the_panel() {
        let drag = ActiveDrag {
            target: DragTarget::Sidebar,
            start_pos: None,
            start_size: 220.0,
        };
        assert_eq!(resize_to(&drag, 999.0, 150.0, 600.0, false), 220.0);
    }

    #[test]
    fn a_drag_is_reversible_to_its_starting_size() {
        let drag = drag_at(200.0, 220.0, DragTarget::Sidebar);
        resize_to(&drag, 400.0, 150.0, 600.0, false);
        assert_eq!(resize_to(&drag, 200.0, 150.0, 600.0, false), 220.0);
    }

    #[test]
    fn drag_state_tracks_one_target_at_a_time() {
        let state = DragState::new();
        assert!(!state.is_dragging(DragTarget::Sidebar));

        state.begin(DragTarget::Sidebar, 220.0);
        assert!(state.is_dragging(DragTarget::Sidebar));
        assert!(!state.is_dragging(DragTarget::Chat));

        state.begin(DragTarget::Chat, 350.0);
        assert!(
            !state.is_dragging(DragTarget::Sidebar),
            "starting a new drag must end the old one"
        );

        state.end();
        assert!(!state.is_dragging(DragTarget::Chat));
    }

    #[test]
    fn a_new_drag_starts_unanchored() {
        let state = DragState::new();
        state.begin(DragTarget::Terminal, 200.0);
        let active = state.active.get_untracked().unwrap();
        assert_eq!(active.start_pos, None);
        assert_eq!(active.start_size, 200.0);
    }
}
