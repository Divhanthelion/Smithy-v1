//! The application menu bar.
//!
//! An in-window menu rather than the native macOS menu bar: the panels it
//! toggles are window-local, the checkmarks have to reflect live signal state,
//! and floem's cross-platform native-menu support is thin enough that owning
//! ~200 lines here is cheaper than working around it on three platforms.
//!
//! Toggles are the point. A panel that can be checked on and off from a menu is
//! discoverable in a way a keyboard shortcut is not, and it means the top strip
//! — which was otherwise dead space with a single floating "Show Terminal" link
//! — earns its height.

use floem::peniko::Color;
use floem::prelude::*;
use floem::reactive::{RwSignal, SignalGet, SignalUpdate};

use crate::theme::catppuccin;

/// The glyph for the platform's primary shortcut modifier.
///
/// The key handler accepts META *or* CONTROL on every platform, so this changes
/// only what the menu says — but saying `⌃O` on a Mac points people at the
/// wrong key even though the right one also works, and a shortcut hint that
/// names the wrong key is worse than no hint.
pub const PRIMARY_MODIFIER: &str = if cfg!(target_os = "macos") {
    "⌘"
} else {
    "⌃"
};

/// A shortcut hint using the platform's primary modifier — `accel("O")` is
/// `⌘O` on macOS and `⌃O` elsewhere.
///
/// Use the literal glyph instead for shortcuts that are genuinely Control
/// everywhere, which is the convention for `⌃\`` and what the hover handler
/// actually matches on.
pub fn accel(key: &str) -> String {
    format!("{PRIMARY_MODIFIER}{key}")
}

/// One row inside a dropdown.
#[derive(Clone)]
pub enum MenuItem {
    /// Flips a boolean signal and shows a checkmark when it is true.
    Toggle {
        label: String,
        shortcut: Option<String>,
        signal: RwSignal<bool>,
    },
    /// Runs a callback.
    Action {
        label: String,
        shortcut: Option<String>,
        action: std::rc::Rc<dyn Fn()>,
    },
    Separator,
}

impl MenuItem {
    pub fn toggle(label: impl Into<String>, signal: RwSignal<bool>) -> Self {
        MenuItem::Toggle {
            label: label.into(),
            shortcut: None,
            signal,
        }
    }

    pub fn toggle_with(
        label: impl Into<String>,
        shortcut: impl Into<String>,
        signal: RwSignal<bool>,
    ) -> Self {
        MenuItem::Toggle {
            label: label.into(),
            shortcut: Some(shortcut.into()),
            signal,
        }
    }

    pub fn action(label: impl Into<String>, action: impl Fn() + 'static) -> Self {
        MenuItem::Action {
            label: label.into(),
            shortcut: None,
            action: std::rc::Rc::new(action),
        }
    }

    pub fn action_with(
        label: impl Into<String>,
        shortcut: impl Into<String>,
        action: impl Fn() + 'static,
    ) -> Self {
        MenuItem::Action {
            label: label.into(),
            shortcut: Some(shortcut.into()),
            action: std::rc::Rc::new(action),
        }
    }

    fn label(&self) -> &str {
        match self {
            MenuItem::Toggle { label, .. } | MenuItem::Action { label, .. } => label,
            MenuItem::Separator => "",
        }
    }

    fn shortcut(&self) -> Option<&str> {
        match self {
            MenuItem::Toggle { shortcut, .. } | MenuItem::Action { shortcut, .. } => {
                shortcut.as_deref()
            }
            MenuItem::Separator => None,
        }
    }
}

/// A top-level menu.
#[derive(Clone)]
pub struct Menu {
    pub title: String,
    pub items: Vec<MenuItem>,
}

impl Menu {
    pub fn new(title: impl Into<String>, items: Vec<MenuItem>) -> Self {
        Self {
            title: title.into(),
            items,
        }
    }

    /// Width the dropdown needs so no row wraps.
    ///
    /// Measured from character counts rather than laid-out text: floem cannot
    /// measure before layout, and a dropdown that resizes as you hover between
    /// menus looks broken. Slightly generous on purpose.
    fn dropdown_width(&self) -> f64 {
        let widest = self
            .items
            .iter()
            .map(|item| {
                let shortcut = item.shortcut().map(|s| s.chars().count() + 4).unwrap_or(0);
                item.label().chars().count() + shortcut
            })
            .max()
            .unwrap_or(10);
        (widest as f64 * 7.2 + 52.0).max(170.0)
    }
}

/// Which menu, if any, is open.
#[derive(Clone, Copy)]
pub struct MenuBarState {
    pub open: RwSignal<Option<usize>>,
}

impl MenuBarState {
    pub fn new() -> Self {
        Self {
            open: RwSignal::new(None),
        }
    }

    pub fn close(&self) {
        self.open.set(None);
    }
}

impl Default for MenuBarState {
    fn default() -> Self {
        Self::new()
    }
}

/// Render the bar itself.
///
/// The open dropdown is **not** part of this view — see [`menu_overlay`]. A
/// dropdown nested here would be positioned below a parent clamped to
/// [`crate::design::MENU_BAR_HEIGHT`], so it was clipped out of existence and
/// the menus appeared not to work at all.
pub fn menu_bar(
    state: MenuBarState,
    menus: Vec<Menu>,
    clock: RwSignal<bool>,
    clock_tick: RwSignal<u64>,
) -> impl IntoView {
    let menus_for_row = menus;

    Stack::horizontal((
        // Wordmark. Cheap, and it makes the strip read as a title bar
        // rather than an empty band.
        Label::derived(|| "Smithy".to_string()).style(|s| {
            s.color(catppuccin::LAVENDER)
                .font_size(12.0)
                .font_bold()
                .padding_horiz(12.0)
        }),
        dyn_stack(
            move || menus_for_row.clone().into_iter().enumerate(),
            |(i, _)| *i,
            move |(index, menu)| menu_title(state, index, menu.title.clone()),
        )
        .style(|s| s.flex_row().items_center()),
        Container::new(Empty::new()).style(|s| s.flex_grow(1.0)),
        clock_readout(clock, clock_tick),
    ))
    .style(|s| {
        s.width_full()
            .height(crate::design::MENU_BAR_HEIGHT)
            .items_center()
            .background(catppuccin::CRUST)
            .border_bottom(1.0)
            .border_color(catppuccin::SURFACE0)
    })
}

/// The floating dropdown layer.
///
/// Mount this at the application root, as a sibling of the modals, so it can
/// paint over the whole window. It occupies no space when no menu is open.
///
/// It also renders a click-catcher behind the open menu, so clicking elsewhere
/// dismisses it.
///
/// The catcher deliberately starts **below** the bar. Covering the full window
/// meant a click on a second menu title landed on the catcher instead of the
/// title, so the menu just closed — you could never move from one menu to the
/// next, and hover-to-switch was dead for the same reason. Leaving the bar
/// uncovered is what makes ordinary menu exploration work.
pub fn menu_overlay(state: MenuBarState, menus: Vec<Menu>) -> impl IntoView {
    Stack::new((
        // Click-outside catcher, filling the overlay (which itself starts under
        // the bar).
        Empty::new()
            .on_event_stop(floem::event::listener::Click, move |_, _| state.close())
            .style(|s| s.absolute().inset(0.0)),
        dropdown_layer(state, menus),
    ))
    .style(move |s| {
        if state.open.get().is_some() {
            s.absolute()
                .inset_left(0.0)
                .inset_right(0.0)
                .inset_top(crate::design::MENU_BAR_HEIGHT)
                .inset_bottom(0.0)
                .z_index(490)
        } else {
            s.display(floem::taffy::Display::None)
        }
    })
}

fn menu_title(state: MenuBarState, index: usize, title: String) -> impl IntoView {
    let is_open = move || state.open.get() == Some(index);

    Label::derived(move || title.clone())
        .on_event_stop(floem::event::listener::Click, move |_, _| {
            state.open.update(|open| {
                *open = if *open == Some(index) {
                    None
                } else {
                    Some(index)
                };
            });
        })
        // Once a menu is open, sliding across the bar should switch menus
        // without another click — the behaviour every desktop menu has.
        .on_event_stop(floem::event::listener::PointerEnter, move |_, _| {
            if state.open.get_untracked().is_some() {
                state.open.set(Some(index));
            }
        })
        .style(move |s| {
            s.font_size(12.0)
                .padding_horiz(10.0)
                .padding_vert(5.0)
                .border_radius(4.0)
                .cursor(floem::style::CursorStyle::Pointer)
                .color(if is_open() {
                    catppuccin::TEXT
                } else {
                    catppuccin::SUBTEXT0
                })
                .background(if is_open() {
                    catppuccin::SURFACE0
                } else {
                    Color::TRANSPARENT
                })
                .hover(|s| s.background(catppuccin::SURFACE0).color(catppuccin::TEXT))
        })
}

/// The floating dropdown for whichever menu is open.
fn dropdown_layer(state: MenuBarState, menus: Vec<Menu>) -> impl IntoView {
    dyn_container(
        move || state.open.get(),
        move |open| {
            let Some(index) = open else {
                return Box::new(Empty::new().style(|s| s.display(floem::taffy::Display::None)))
                    as Box<dyn View>;
            };
            let Some(menu) = menus.get(index).cloned() else {
                return Box::new(Empty::new().style(|s| s.display(floem::taffy::Display::None)))
                    as Box<dyn View>;
            };

            // Horizontal offset: the wordmark plus every preceding title.
            let left = 12.0
                + measure_text(&"Smithy".chars().collect::<String>())
                + 12.0
                + menus[..index]
                    .iter()
                    .map(|m| measure_text(&m.title) + 20.0)
                    .sum::<f64>();
            let width = menu.dropdown_width();

            Box::new(
                Stack::vertical((dyn_stack(
                    move || menu.items.clone().into_iter().enumerate(),
                    |(i, _)| *i,
                    move |(_, item)| menu_row(state, item),
                )
                .style(|s| s.flex_col().width_full()),))
                .style(move |s| {
                    s.absolute()
                        .inset_left(left)
                        .inset_top(1.0)
                        .width(width)
                        .padding_vert(5.0)
                        .background(catppuccin::MANTLE)
                        .border(1.0)
                        .border_color(catppuccin::SURFACE1)
                        .border_radius(7.0)
                        .z_index(500)
                }),
            ) as Box<dyn View>
        },
    )
}

/// Approximate rendered width of a menu title at 12px.
fn measure_text(s: &str) -> f64 {
    s.chars().count() as f64 * 7.0
}

fn menu_row(state: MenuBarState, item: MenuItem) -> impl IntoView {
    match item {
        MenuItem::Separator => Container::new(
            Empty::new().style(|s| s.width_full().height(1.0).background(catppuccin::SURFACE0)),
        )
        .style(|s| s.width_full().padding_vert(4.0).padding_horiz(8.0))
        .into_any(),

        MenuItem::Toggle {
            label: text,
            shortcut,
            signal,
        } => {
            let checked = move || signal.get();
            Stack::horizontal((
                Label::derived(move || {
                    if checked() {
                        "✓".to_string()
                    } else {
                        String::new()
                    }
                })
                .style(move |s| {
                    s.width(18.0)
                        .font_size(11.0)
                        .font_family(crate::design::SYMBOL.to_string())
                        .color(catppuccin::GREEN)
                }),
                Label::derived(move || text.clone()).style(move |s| {
                    s.font_size(12.0).flex_grow(1.0).color(if checked() {
                        catppuccin::TEXT
                    } else {
                        catppuccin::SUBTEXT0
                    })
                }),
                shortcut_label(shortcut),
            ))
            .on_event_stop(floem::event::listener::Click, move |_, _| {
                signal.update(|v| *v = !*v);
                state.close();
            })
            .style(row_style)
            .into_any()
        }

        MenuItem::Action {
            label: text,
            shortcut,
            action,
        } => Stack::horizontal((
            Container::new(Empty::new()).style(|s| s.width(18.0)),
            Label::derived(move || text.clone())
                .style(|s| s.font_size(12.0).flex_grow(1.0).color(catppuccin::TEXT)),
            shortcut_label(shortcut),
        ))
        .on_event_stop(floem::event::listener::Click, move |_, _| {
            action();
            state.close();
        })
        .style(row_style)
        .into_any(),
    }
}

fn shortcut_label(shortcut: Option<String>) -> impl IntoView {
    // `SYMBOL`, because the content is ⌘/⌃/⇧ and the default sans has none of
    // them — the accelerators rendered as missing-glyph boxes, which is why the
    // app ended up accepting either modifier: nobody could read which was meant.
    Label::derived(move || shortcut.clone().unwrap_or_default()).style(|s| {
        s.font_size(10.5)
            .font_family(crate::design::SYMBOL.to_string())
            .color(catppuccin::SURFACE2)
            .margin_left(16.0)
    })
}

fn row_style(s: floem::style::Style) -> floem::style::Style {
    s.width_full()
        .items_center()
        .padding_horiz(9.0)
        .padding_vert(5.0)
        .cursor(floem::style::CursorStyle::Pointer)
        .hover(|s| s.background(catppuccin::SURFACE0))
}

/// The clock, at the right-hand end of the menu bar.
///
/// Shows the time and today's sunrise and sunset, because those are the
/// fisherman's anchors and the sky's — seeing them is how you check that the
/// backdrop is telling the truth.
///
/// It reads a one-second tick rather than the clock directly: a label cannot
/// notice that time has passed, so something has to tell it.
fn clock_readout(visible: RwSignal<bool>, tick: RwSignal<u64>) -> impl IntoView {
    Label::derived(move || {
        tick.get();
        if !visible.get() {
            return String::new();
        }
        let seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let (sunrise, sunset) = crate::celestial::todays_sun(seconds);
        crate::clock::format_with_sun(crate::localtime::local_hours(seconds), sunrise, sunset)
    })
    .style(move |s| {
        s.font_family(crate::design::SYMBOL.to_string())
            .font_size(11.0)
            .color(catppuccin::OVERLAY1)
            .padding_horiz(12.0)
            .apply_if(!visible.get(), |s| s.display(floem::taffy::Display::None))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_toggle_item_reports_its_label_and_shortcut() {
        let sig = RwSignal::new(false);
        let item = MenuItem::toggle_with("Terminal", "⌃`", sig);
        assert_eq!(item.label(), "Terminal");
        assert_eq!(item.shortcut(), Some("⌃`"));
    }

    #[test]
    fn a_separator_has_no_label() {
        assert_eq!(MenuItem::Separator.label(), "");
        assert_eq!(MenuItem::Separator.shortcut(), None);
    }

    /// The dropdown must be wide enough for its widest row, or labels wrap and
    /// the menu looks broken.
    #[test]
    fn dropdown_width_grows_with_the_widest_row() {
        let narrow = Menu::new("V", vec![MenuItem::action("Go", || {})]);
        let wide = Menu::new(
            "V",
            vec![MenuItem::action("A considerably longer menu entry", || {})],
        );
        assert!(wide.dropdown_width() > narrow.dropdown_width());
    }

    #[test]
    fn dropdown_width_has_a_floor() {
        let tiny = Menu::new("V", vec![MenuItem::action("x", || {})]);
        assert!(tiny.dropdown_width() >= 170.0);
    }

    #[test]
    fn dropdown_width_accounts_for_the_shortcut_column() {
        let bare = Menu::new("V", vec![MenuItem::action("Toggle Terminal", || {})]);
        let with_shortcut = Menu::new(
            "V",
            vec![MenuItem::action_with("Toggle Terminal", "⌃⇧`", || {})],
        );
        assert!(with_shortcut.dropdown_width() > bare.dropdown_width());
    }

    #[test]
    fn an_empty_menu_still_has_a_sane_width() {
        assert!(Menu::new("V", vec![]).dropdown_width() >= 170.0);
    }

    #[test]
    fn the_menu_bar_starts_closed() {
        let state = MenuBarState::new();
        assert_eq!(state.open.get_untracked(), None);
    }

    #[test]
    fn closing_clears_the_open_menu() {
        let state = MenuBarState::new();
        state.open.set(Some(2));
        state.close();
        assert_eq!(state.open.get_untracked(), None);
    }

    /// Menu offsets accumulate left to right; each dropdown must sit under its
    /// own title rather than all stacking at the origin.
    #[test]
    fn title_offsets_accumulate() {
        let a = measure_text("File");
        let b = measure_text("View");
        assert!(a > 0.0 && b > 0.0);
        assert!(measure_text("Tools") > measure_text("File"));
    }
}
