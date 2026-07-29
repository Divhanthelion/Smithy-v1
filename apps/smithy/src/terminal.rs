//! Multi-terminal view for Smithy
//!
//! Provides a tabbed terminal panel where each tab is an independent PTY session.

use std::cell::RefCell;
use std::rc::Rc;

use floem::peniko::Color;
use floem::prelude::*;
use floem::reactive::Effect;
use floem::reactive::{RwSignal, SignalGet};

use smithy_editor::{
    floem_key_to_terminal_key, terminal_view::terminal_grid_view, TerminalTabManager,
};

/// Shared terminal tab manager
pub type SharedTerminalTabs = Rc<RefCell<TerminalTabManager>>;

/// Multi-terminal UI component with tab bar
pub struct MultiTerminalComponent {
    tabs: SharedTerminalTabs,
    /// Whether the terminal panel is visible
    visible: RwSignal<bool>,
    /// Bumped when terminal *content* changes: a keystroke was sent, or the
    /// poll tick found new output. Drives repaint only.
    version: RwSignal<u64>,
    /// Bumped when the tab *structure* changes: switch, create, close.
    ///
    /// Separate from `version` on purpose. The panel body is a `dyn_container`,
    /// which tears down and rebuilds its child whenever a signal read in its
    /// trigger changes — there is no equality check. While one signal served
    /// both roles, every keystroke and every 60fps poll rebuilt the terminal
    /// subtree, destroying the focused view. That is why typing needed a fresh
    /// click before each character.
    tabs_version: RwSignal<u64>,
}

impl MultiTerminalComponent {
    pub fn new(tabs: SharedTerminalTabs, visible: RwSignal<bool>) -> Self {
        Self {
            tabs,
            visible,
            version: RwSignal::new(0),
            tabs_version: RwSignal::new(0),
        }
    }

    /// Build the composite terminal view: tab bar + active terminal content
    pub fn view(&self) -> impl View {
        let tabs = self.tabs.clone();
        let tabs_for_key = self.tabs.clone();
        let tabs_for_poll = self.tabs.clone();
        let tabs_for_tick = self.tabs.clone();
        let visible = self.visible;
        let version = self.version;
        let tabs_version = self.tabs_version;

        // Spawn a background thread to poll the terminal at 60fps
        let (tick_tx, tick_rx) = crossbeam_channel::unbounded::<()>();
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_millis(16));
            if tick_tx.send(()).is_err() {
                break;
            }
        });

        let tick_signal = floem::reactive::RwSignal::new(None::<()>);
        floem::ext_event::update_signal_from_channel(tick_signal.write_only(), tick_rx);

        Effect::new(move |_| {
            tick_signal.get();
            let mut updated = false;
            if let Ok(mut mgr) = tabs_for_tick.try_borrow_mut() {
                if let Some(tab) = mgr.active_tab_mut() {
                    if let Ok(mut state) = tab.state.try_borrow_mut() {
                        if state.poll_events() {
                            updated = true;
                        }
                    }
                }
            }
            if updated {
                version.update(|v| *v += 1);
            }
        });

        // Auto-create first tab when panel becomes visible
        let tabs_for_effect = self.tabs.clone();
        Effect::new(move |_| {
            if visible.get() {
                let mut needs_version_bump = false;
                if let Ok(mut mgr) = tabs_for_effect.try_borrow_mut() {
                    if mgr.is_empty() {
                        if let Err(e) = mgr.new_tab() {
                            eprintln!("Failed to spawn terminal: {}\n", e);
                            return;
                        }
                        needs_version_bump = true;
                    }
                }
                if needs_version_bump {
                    tabs_version.update(|v| *v += 1);
                }
            }
        });

        // Tab bar
        let tab_bar = terminal_tab_bar(tabs.clone(), tabs_version);

        // Terminal content area
        let content = dyn_container(
            move || {
                // tabs_version, NOT version: reading `version` here rebuilt the
                // whole subtree on every keystroke and dropped focus.
                let _ = tabs_version.get();
                if let Ok(mgr) = tabs_for_poll.try_borrow() {
                    mgr.active_tab().map(|t| t.state.clone())
                } else {
                    None
                }
            },
            move |active_state| {
                if let Some(state) = active_state {
                    // Use the optimized terminal grid view
                    Stack::vertical((terminal_grid_view(state, version),))
                        .style(|s| s.width_full().height_full().min_height(0.0))
                        .into_any()
                } else {
                    Empty::new().into_any()
                }
            },
        )
        .style(|s| {
            s.width_full()
                .flex_grow(1.0)
                .min_height(0.0)
                .background(Color::from_rgb8(30, 30, 30))
        })
        .on_event_stop(floem::event::listener::KeyDown, move |_, key_event| {
            {
                crate::key_debug("terminal", key_event);
                // Ctrl+` or Ctrl+' toggles terminal panel visibility
                if key_event
                    .modifiers
                    .contains(floem::prelude::Modifiers::CONTROL)
                    && !key_event
                        .modifiers
                        .contains(floem::prelude::Modifiers::SHIFT)
                {
                    if let floem::prelude::Key::Character(ref c) = key_event.key {
                        if c.as_str() == "`" || c.as_str() == "'" {
                            visible.update(|v| *v = !*v);
                            return;
                        }
                    }
                }
                let mut updated = false;
                if let Ok(mut mgr) = tabs_for_key.try_borrow_mut() {
                    if let Some(tab) = mgr.active_tab_mut() {
                        if let Some(terminal_key) = floem_key_to_terminal_key(&key_event.key) {
                            let ctrl = key_event
                                .modifiers
                                .contains(floem::prelude::Modifiers::CONTROL);
                            let alt = key_event.modifiers.contains(floem::prelude::Modifiers::ALT);
                            if let Ok(mut state) = tab.state.try_borrow_mut() {
                                let _ = state.send_key(terminal_key, ctrl, alt);
                                state.poll_events();
                                updated = true;
                            }
                        }
                    }
                }
                if updated {
                    version.update(|v| *v += 1);
                }
            }
        })
        .style(|s| s.keyboard_navigable())
        // Focus the terminal whenever the panel is shown, so ⌃` is enough to
        // start typing. `keyboard_navigable` only makes a view *able* to take
        // focus — something still has to hand it over, and previously nothing
        // did, so every session began with a click nobody should have to make.
        //
        // The closure is reactive: floem re-runs it on any signal it reads, so
        // reading `visible` re-requests focus each time the panel toggles.
        .request_focus(move || {
            let _ = visible.get();
            // Also after the tab exists. When the panel is first shown there is
            // no tab yet — one is created by an effect that runs afterwards —
            // so a focus request made on `visible` alone lands before there is
            // anything laid out to receive it.
            let _ = tabs_version.get();
        });

        // Composite: tab bar on top, terminal below
        Stack::vertical((tab_bar, content)).style(|s| s.width_full().height_full().min_height(0.0))
    }
}

/// Render the terminal tab bar
/// The tab bar reflects tab *structure* only, so it takes `tabs_version`.
fn terminal_tab_bar(tabs: SharedTerminalTabs, tabs_version: RwSignal<u64>) -> impl IntoView {
    let tabs_for_dyn = tabs.clone();
    let tabs_for_new = tabs.clone();

    // Collect tab info reactively
    let tab_infos = move || {
        let _ = tabs_version.get(); // rebuild labels on switch/create/close
        if let Ok(mgr) = tabs.try_borrow() {
            let active = mgr.active_id();
            mgr.tabs()
                .iter()
                .map(|t| (t.id, t.label.clone(), active == Some(t.id)))
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        }
    };

    Stack::horizontal((
        // Dynamic tab buttons
        dyn_stack(
            tab_infos,
            |(id, _, _)| *id,
            move |(id, tab_label, is_active)| {
                let tabs_click = tabs_for_dyn.clone();
                let tabs_close = tabs_click.clone();

                let tab_label_text = tab_label.clone();
                let active_bg = if is_active {
                    Color::from_rgb8(30, 30, 30)
                } else {
                    Color::from_rgb8(45, 45, 45)
                };
                let text_color = if is_active {
                    Color::from_rgb8(255, 255, 255)
                } else {
                    Color::from_rgb8(160, 160, 160)
                };

                Stack::horizontal((
                    // Tab label (click to switch)
                    Label::derived(move || tab_label_text.clone())
                        .style(move |s: floem::style::Style| {
                            s.color(text_color).font_size(12.0).padding_horiz(8.0)
                        })
                        .on_event_stop(floem::event::listener::Click, move |_, _| {
                            if let Ok(mut mgr) = tabs_click.try_borrow_mut() {
                                mgr.set_active(id);
                            }
                            tabs_version.update(|v| *v += 1);
                        }),
                    // Close button
                    Label::derived(|| "x".to_string())
                        .style(move |s: floem::style::Style| {
                            s.color(Color::from_rgb8(160, 160, 160))
                                .font_size(11.0)
                                .padding_horiz(4.0)
                                .hover(|s: floem::style::Style| {
                                    s.color(Color::from_rgb8(255, 100, 100))
                                })
                        })
                        .on_event_stop(floem::event::listener::Click, move |_, _| {
                            if let Ok(mut mgr) = tabs_close.try_borrow_mut() {
                                mgr.close_tab(id);
                            }
                            tabs_version.update(|v| *v += 1);
                        }),
                ))
                .style(move |s| {
                    s.items_center()
                        .background(active_bg)
                        .padding_vert(4.0)
                        .border_right(1.0)
                        .border_color(Color::from_rgb8(60, 60, 60))
                })
            },
        )
        .style(|s| s.flex_row()),
        // "+" new tab button
        {
            let tabs_new = tabs_for_new;
            Label::derived(|| "+".to_string())
                .style(|s| {
                    s.color(Color::from_rgb8(160, 160, 160))
                        .font_size(14.0)
                        .padding_horiz(10.0)
                        .padding_vert(4.0)
                        .hover(|s| s.color(Color::WHITE))
                })
                .on_event_stop(floem::event::listener::Click, move |_, _| {
                    if let Ok(mut mgr) = tabs_new.try_borrow_mut() {
                        if let Err(e) = mgr.new_tab() {
                            eprintln!("Failed to create terminal: {}\n", e);
                        }
                    }
                    tabs_version.update(|v| *v += 1);
                })
        },
    ))
    .style(|s| {
        s.width_full()
            .background(Color::from_rgb8(45, 45, 45))
            .border_bottom(1.0)
            .border_color(Color::from_rgb8(60, 60, 60))
            .items_center()
    })
}
