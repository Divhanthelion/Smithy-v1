//! Conversation history in the left rail.
//!
//! Shares the pane with the file explorer. Rows are Sessions this Project has
//! stored; click resumes, the log glyph opens the JSON on disk.

use floem::peniko::Color;
use floem::prelude::*;
use floem::reactive::{RwSignal, SignalGet, SignalUpdate};
use floem::style::CustomStylable;
use floem::views::Decorators;

use crate::design;
use crate::file_browser::FileBrowserTheme;

/// One stored Session in the history list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionListRow {
    pub id: String,
    pub title: String,
    pub when: String,
    pub model: String,
    pub skill: Option<String>,
    pub active: bool,
}

/// Files vs conversation history in the left rail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SidebarTab {
    Files,
    History,
}

pub fn session_history_view(
    rows: RwSignal<Vec<SessionListRow>>,
    on_open: impl Fn(String) + 'static + Clone,
    on_open_log: impl Fn(String) + 'static + Clone,
    on_delete: impl Fn(String) + 'static + Clone,
) -> impl IntoView {
    let theme = FileBrowserTheme::default();
    let list = rows;
    Stack::vertical((
        Label::derived(move || {
            if list.get().is_empty() {
                "No conversations yet. They appear here after a turn.".to_string()
            } else {
                String::new()
            }
        })
        .style(move |s| {
            s.font_size(11.0)
                .color(theme.header_text)
                .padding_horiz(10.0)
                .padding_vert(8.0)
                .apply_if(!list.get().is_empty(), |s| {
                    s.display(floem::style::Display::None)
                })
        }),
        floem::views::scroll::Scroll::new(dyn_stack(
            move || list.get().into_iter(),
            |row| row.id.clone(),
            move |row| {
                history_row(
                    row,
                    theme,
                    on_open.clone(),
                    on_open_log.clone(),
                    on_delete.clone(),
                )
            },
        ))
        .custom_style(|s: floem::views::scroll::ScrollCustomStyle| {
            s.hide_bars(false)
                .handle_background(Color::from_rgba8(150, 150, 150, 150))
                .handle_border_radius(4.0)
        })
        .style(move |s| {
            s.flex_grow(1.0)
                .width_full()
                .min_height(0.0)
                .background(theme.background)
        }),
    ))
    .style(move |s| {
        s.width_full()
            .height_full()
            .flex_col()
            .background(theme.background)
    })
}

fn history_row(
    row: SessionListRow,
    theme: FileBrowserTheme,
    on_open: impl Fn(String) + 'static + Clone,
    on_open_log: impl Fn(String) + 'static + Clone,
    on_delete: impl Fn(String) + 'static + Clone,
) -> impl IntoView {
    let id = row.id.clone();
    let id_log = row.id.clone();
    let id_del = row.id.clone();
    let active = row.active;
    let meta = match &row.skill {
        Some(skill) if !skill.is_empty() => format!("{} · {}", row.when, skill),
        _ => {
            if row.model.is_empty() {
                row.when.clone()
            } else {
                format!("{} · {}", row.when, row.model)
            }
        }
    };

    Stack::horizontal((
        Stack::vertical((
            Label::new(row.title.clone()).style(move |s| {
                s.font_size(12.0)
                    .color(if active {
                        theme.selected_text
                    } else {
                        theme.text
                    })
                    .text_ellipsis()
                    .width_full()
            }),
            Label::new(meta).style(move |s| {
                s.font_size(10.0)
                    .color(theme.header_text)
                    .text_ellipsis()
                    .width_full()
            }),
        ))
        .style(|s| s.flex_grow(1.0).flex_col().min_width(0.0).gap(2.0)),
        Label::new("☰".to_string())
            .style(move |s| {
                s.font_size(11.0)
                    .color(theme.header_text)
                    .padding_horiz(6.0)
                    .padding_vert(4.0)
                    .hover(|s| s.color(theme.text).background(theme.hover_background))
                    .cursor(floem::style::CursorStyle::Pointer)
            })
            .on_event_stop(floem::event::listener::Click, move |_, _| {
                on_open_log(id_log.clone());
            }),
        Label::new("×".to_string())
            .style(move |s| {
                s.font_size(14.0)
                    .color(theme.header_text)
                    .padding_horiz(6.0)
                    .padding_vert(4.0)
                    .hover(|s| {
                        s.color(crate::theme::catppuccin::RED)
                            .background(theme.hover_background)
                    })
                    .cursor(floem::style::CursorStyle::Pointer)
            })
            .on_event_stop(floem::event::listener::Click, move |_, _| {
                on_delete(id_del.clone());
            }),
    ))
    .style(move |s| {
        let s = s
            .width_full()
            .padding_horiz(10.0)
            .padding_vert(8.0)
            .items_center()
            .gap(4.0)
            .cursor(floem::style::CursorStyle::Pointer)
            .hover(|s| s.background(theme.hover_background));
        if active {
            s.background(theme.selected_background)
        } else {
            s
        }
    })
    .on_event_stop(floem::event::listener::Click, move |_, _| {
        on_open(id.clone());
    })
}

pub fn sidebar_mode_bar(
    tab: RwSignal<SidebarTab>,
    on_hide: impl Fn() + 'static + Clone,
) -> impl IntoView {
    let theme = FileBrowserTheme::default();
    Stack::horizontal((
        mode_chip(tab, SidebarTab::Files, "Files", theme),
        mode_chip(tab, SidebarTab::History, "History", theme),
        Empty::new().style(|s| s.flex_grow(1.0)),
        Label::derived(|| design::glyph::HIDE.to_string())
            .style(move |s| {
                s.width(22.0)
                    .height(22.0)
                    .items_center()
                    .justify_center()
                    .border_radius(3.0)
                    .font_size(11.0)
                    .font_family(design::SYMBOL.to_string())
                    .color(theme.header_text)
                    .hover(|s| s.background(theme.hover_background))
                    .cursor(floem::style::CursorStyle::Pointer)
            })
            .on_event_stop(floem::event::listener::Click, move |_, _| {
                on_hide();
            }),
    ))
    .style(move |s| {
        s.width_full()
            .height(28.0)
            .padding_horiz(8.0)
            .items_center()
            .gap(6.0)
            .background(theme.header_background)
            .border_bottom(1.0)
            .border_color(theme.border)
    })
}

fn mode_chip(
    tab: RwSignal<SidebarTab>,
    which: SidebarTab,
    label: &'static str,
    theme: FileBrowserTheme,
) -> impl IntoView {
    Label::derived(move || label.to_string())
        .on_event_stop(floem::event::listener::Click, move |_, _| {
            tab.set(which);
        })
        .style(move |s| {
            let on = tab.get() == which;
            s.font_size(10.0)
                .padding_horiz(8.0)
                .padding_vert(4.0)
                .border_radius(4.0)
                .cursor(floem::style::CursorStyle::Pointer)
                .color(if on {
                    theme.selected_text
                } else {
                    theme.header_text
                })
                .background(if on {
                    theme.selected_background
                } else {
                    Color::from_rgba8(0, 0, 0, 0)
                })
                .hover(|s| s.background(theme.hover_background))
        })
}
