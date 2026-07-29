//! File Browser View module - Floem view for filesystem navigation
//!
//! This module provides the file browser sidebar UI component that displays
//! a tree view of files and folders.

use std::path::PathBuf;

use floem::prelude::*;
use floem::reactive::{RwSignal, SignalGet, SignalUpdate};
use floem::style::CustomStylable;
use floem::views::Decorators;

use crate::design;
use crate::file_browser::{FileBrowserTheme, FileEntry, SharedFileBrowserState};

/// Display information for a file entry (used for reactive updates)
#[derive(Clone, Debug)]
pub struct FileEntryDisplay {
    /// Unique key for diffing (path as string)
    pub key: String,
    /// Full path
    pub path: PathBuf,
    /// Display name
    pub name: String,
    /// Whether this is a directory
    pub is_dir: bool,
    /// Indentation level
    pub indent_level: usize,
    /// Whether expanded (for directories)
    pub is_expanded: bool,
    /// Whether selected
    pub is_selected: bool,
    /// Whether this is a parent navigation entry (..)
    pub is_parent_nav: bool,
}

impl FileEntryDisplay {
    /// Create from a FileEntry and selection state
    pub fn from_entry(entry: &FileEntry, is_selected: bool) -> Self {
        Self {
            key: entry.path.to_string_lossy().to_string(),
            path: entry.path.clone(),
            name: entry.name.clone(),
            is_dir: entry.is_dir,
            indent_level: entry.indent_level,
            is_expanded: entry.is_expanded,
            is_selected,
            is_parent_nav: entry.is_parent_nav,
        }
    }

    /// Get the display icon
    pub fn icon(&self) -> &'static str {
        if self.is_parent_nav {
            "\u{2190} " // ←
        } else if self.is_dir {
            if self.is_expanded {
                "\u{25BE} " // ▾
            } else {
                "\u{25B8} " // ▸
            }
        } else {
            self.file_icon()
        }
    }

    /// Get a file-type icon based on extension
    ///
    /// Ordered most-specific first. Exact filenames must precede the extension
    /// arms or they are unreachable: `Cargo.toml` used to be listed last and so
    /// always matched `.toml` on the way past, and `Cargo.lock` matched `.lock`.
    fn file_icon(&self) -> &'static str {
        let name = &self.name;
        if name == "Cargo.toml" || name == "Cargo.lock" {
            "\u{1F4E6} "
        }
        // 📦 Cargo
        else if name.ends_with(".rs") {
            "\u{1F9E0} "
        }
        // 🧠 Rust
        else if name.ends_with(".py") {
            "\u{1F40D} "
        }
        // 🐍 Python
        else if name.ends_with(".sh") || name.ends_with(".bash") {
            "\u{0024} "
        }
        // $ Shell
        else if name.ends_with(".wasm") || name.ends_with(".toml") {
            "\u{2699} "
        }
        // ⚙ WASM, config
        else if name.ends_with(".json") {
            "\u{007B} "
        }
        // { JSON
        else if name.ends_with(".md") {
            "\u{2756} "
        }
        // ❖ Markdown
        else if name.ends_with(".lock") {
            "\u{1F512} "
        }
        // 🔒 Lock
        else {
            "  "
        }
    }
}

/// Create a single file entry row view
fn file_entry_view(
    entry: FileEntryDisplay,
    theme: FileBrowserTheme,
    font_size: f32,
    on_click: impl Fn(PathBuf) + 'static + Clone,
    on_double_click: impl Fn(PathBuf) + 'static + Clone,
) -> impl IntoView {
    let path = entry.path.clone();
    let path_for_dblclick = entry.path.clone();
    let is_dir = entry.is_dir;
    let is_selected = entry.is_selected;
    let indent = entry.indent_level;
    let icon = entry.icon().to_string();
    let name = entry.name.clone();

    let on_click_clone = on_click.clone();
    let on_dblclick_clone = on_double_click.clone();

    // Create a reference to track clicks for double-click detection
    let last_click = RwSignal::new(0.0f64);

    Stack::horizontal((
        // Indentation spacer
        Empty::new().style(move |s| s.width((indent as f32) * 14.0)),
        // Icon
        // The icon is a glyph, so it needs a family chosen for coverage — the
        // folder triangles, the parent arrow and the markdown diamond are all
        // absent from the default sans and rendered as boxes.
        Label::derived(move || icon.clone()).style(move |s| {
            s.font_size(font_size - 1.0)
                .font_family(design::SYMBOL.to_string())
                .color(if is_dir {
                    theme.folder_text
                } else {
                    theme.text
                })
                .min_width(18.0)
        }),
        // File/folder name
        Label::derived(move || name.clone()).style(move |s| {
            let text_color = if is_selected {
                theme.selected_text
            } else if is_dir {
                theme.folder_text
            } else {
                theme.text
            };
            s.font_size(font_size).color(text_color)
        }),
    ))
    .style(move |s| {
        let bg = if is_selected {
            theme.selected_background
        } else {
            theme.background
        };
        s.width_full()
            .padding_vert(2.0)
            .padding_left(6.0)
            .padding_right(6.0)
            .background(bg)
            .border_radius(3.0)
            .margin_horiz(4.0)
            .margin_vert(1.0)
            .hover(|s| {
                if !is_selected {
                    s.background(theme.hover_background)
                } else {
                    s
                }
            })
            .cursor(floem::style::CursorStyle::Pointer)
    })
    .on_event_stop(floem::event::listener::Click, move |_, _| {
        // Simple double-click detection using time
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);

        let last = last_click.get();
        let elapsed = now - last;

        if elapsed < 0.4 && elapsed > 0.0 {
            // Double click - open file
            on_dblclick_clone(path_for_dblclick.clone());
        } else {
            // Single click - select
            on_click_clone(path.clone());
        }

        last_click.set(now);
    })
}

/// Create the file tree list view
fn file_list_view(
    entries_signal: RwSignal<Vec<FileEntryDisplay>>,
    theme: FileBrowserTheme,
    font_size: f32,
    on_click: impl Fn(PathBuf) + 'static + Clone,
    on_double_click: impl Fn(PathBuf) + 'static + Clone,
) -> impl IntoView {
    let on_click = on_click.clone();
    let on_dblclick = on_double_click.clone();

    dyn_stack(
        move || entries_signal.get(),
        |entry| entry.key.clone(),
        move |entry| {
            file_entry_view(
                entry,
                theme,
                font_size,
                on_click.clone(),
                on_dblclick.clone(),
            )
        },
    )
    .style(|s| s.flex_col().width_full())
}

/// Create the file browser header view
fn file_browser_header(
    root_path_signal: RwSignal<String>,
    theme: FileBrowserTheme,
    on_home: impl Fn() + 'static,
    on_root: impl Fn() + 'static,
    on_refresh: impl Fn() + 'static,
    on_collapse_all: impl Fn() + 'static,
    on_hide: impl Fn() + 'static,
) -> impl IntoView {
    Stack::vertical((
        // Title + nav buttons row
        Stack::horizontal((
            Label::derived(|| "EXPLORER".to_string())
                .style(move |s| s.font_size(10.0).color(theme.header_text)),
            // Spacer
            Empty::new().style(|s| s.flex_grow(1.0)),
            // Home button
            Label::derived(|| "\u{2302}".to_string()) // ⌂
                .style(move |s| {
                    s.width(22.0)
                        .height(22.0)
                        .items_center()
                        .justify_center()
                        .border_radius(3.0)
                        .font_size(13.0)
                        .font_family(design::SYMBOL.to_string())
                        .color(theme.header_text)
                        .hover(|s| s.background(theme.hover_background))
                        .cursor(floem::style::CursorStyle::Pointer)
                })
                .on_event_stop(floem::event::listener::Click, move |_, _| {
                    on_home();
                }),
            // Root button
            Label::derived(|| "/".to_string())
                .style(move |s| {
                    s.width(22.0)
                        .height(22.0)
                        .items_center()
                        .justify_center()
                        .border_radius(3.0)
                        .font_size(13.0)
                        .font_family(design::SYMBOL.to_string())
                        .color(theme.header_text)
                        .hover(|s| s.background(theme.hover_background))
                        .cursor(floem::style::CursorStyle::Pointer)
                })
                .on_event_stop(floem::event::listener::Click, move |_, _| {
                    on_root();
                }),
            // Refresh button
            Label::derived(|| "\u{21BB}".to_string()) // ↻
                .style(move |s| {
                    s.width(22.0)
                        .height(22.0)
                        .items_center()
                        .justify_center()
                        .border_radius(3.0)
                        .font_size(13.0)
                        .font_family(design::SYMBOL.to_string())
                        .color(theme.header_text)
                        .hover(|s| s.background(theme.hover_background))
                        .cursor(floem::style::CursorStyle::Pointer)
                })
                .on_event_stop(floem::event::listener::Click, move |_, _| {
                    on_refresh();
                }),
            // Collapse all button
            Label::derived(|| design::glyph::COLLAPSE_ALL.to_string())
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
                    on_collapse_all();
                }),
            // Hide the panel. `─` means "minimise this" everywhere else, so it
            // now does that rather than collapsing folders — which was the old
            // behaviour, was correct, and read as a dead button because there is
            // usually nothing expanded to collapse.
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
                .padding_horiz(10.0)
                .items_center()
        }),
        // Current path display
        Label::derived(move || root_path_signal.get()).style(move |s| {
            s.font_size(10.0)
                .color(theme.header_text)
                .width_full()
                .padding_horiz(10.0)
                .padding_bottom(6.0)
                .text_ellipsis()
        }),
    ))
    .style(move |s| {
        s.width_full()
            .background(theme.header_background)
            .border_bottom(1.0)
            .border_color(theme.border)
            .padding_top(6.0)
    })
}

/// Create the full file browser panel view
///
/// The argument count is the callback set the panel needs; bundling them into a
/// struct would move the same list one level down without simplifying a caller.
#[allow(clippy::too_many_arguments)]
pub fn file_browser_panel_view(
    entries_signal: RwSignal<Vec<FileEntryDisplay>>,
    root_path_signal: RwSignal<String>,
    theme: FileBrowserTheme,
    font_size: f32,
    on_click: impl Fn(PathBuf) + 'static + Clone,
    on_double_click: impl Fn(PathBuf) + 'static + Clone,
    on_home: impl Fn() + 'static + Clone,
    on_root: impl Fn() + 'static + Clone,
    on_refresh: impl Fn() + 'static + Clone,
    on_collapse_all: impl Fn() + 'static + Clone,
    on_hide: impl Fn() + 'static + Clone,
) -> impl IntoView {
    let on_home_clone = on_home.clone();
    let on_root_clone = on_root.clone();
    let on_refresh_clone = on_refresh.clone();
    let on_collapse_clone = on_collapse_all.clone();
    let on_hide_clone = on_hide.clone();

    Stack::vertical((
        // Header
        file_browser_header(
            root_path_signal,
            theme,
            on_home_clone,
            on_root_clone,
            on_refresh_clone,
            on_collapse_clone,
            on_hide_clone,
        ),
        // Scrollable file list
        floem::views::scroll::Scroll::new(file_list_view(
            entries_signal,
            theme,
            font_size,
            on_click,
            on_double_click,
        ))
        .custom_style(|s: floem::views::scroll::ScrollCustomStyle| {
            s.hide_bars(false)
                .handle_background(Color::from_rgba8(150, 150, 150, 150))
                .handle_border_radius(4.0)
        })
        .style(|s| s.flex_grow(1.0).width_full().min_height(0.0)),
    ))
    .style(move |s| {
        s.width_full()
            .height_full()
            .flex_col()
            .background(theme.background)
            .border_right(1.0)
            .border_color(theme.border)
    })
}

/// Create a file browser view from shared state
///
/// This is the main entry point for creating a file browser sidebar.
/// The file explorer.
///
/// `refresh` is an externally-owned counter: bumping it rebuilds the tree from
/// `state`. The entry and path signals are created inside this function, so
/// without a trigger nothing outside could ever make the explorer re-read its
/// root — which is why it kept showing the old project after switching.
pub fn file_browser_view(
    state: SharedFileBrowserState,
    refresh: RwSignal<u64>,
    on_file_open: impl Fn(PathBuf) + 'static + Clone,
    on_hide: impl Fn() + 'static + Clone,
) -> impl IntoView {
    let state_ref = state.borrow();
    let theme = state_ref.theme;
    let font_size = state_ref.font_size;

    // Build initial entries
    let initial_entries: Vec<FileEntryDisplay> = state_ref
        .build_entry_list()
        .iter()
        .map(|e| FileEntryDisplay::from_entry(e, state_ref.is_selected(&e.path)))
        .collect();
    let root_path = state_ref.root_path_display();

    drop(state_ref);

    // Create signals
    let entries_signal = RwSignal::new(initial_entries);
    let root_path_signal = RwSignal::new(root_path);

    // Rebuild whenever an external caller bumps the trigger — e.g. after the
    // project changes underneath us.
    {
        let state = state.clone();
        floem::reactive::Effect::new(move |_| {
            refresh.get();
            let Ok(browser) = state.try_borrow() else {
                return;
            };
            let entries: Vec<FileEntryDisplay> = browser
                .build_entry_list()
                .iter()
                .map(|e| FileEntryDisplay::from_entry(e, browser.is_selected(&e.path)))
                .collect();
            let path = browser.root_path_display();
            drop(browser);
            entries_signal.set(entries);
            root_path_signal.set(path);
        });
    }

    // Clone state for callbacks
    let state_for_click = state.clone();
    let state_for_dblclick = state.clone();
    let state_for_home = state.clone();
    let state_for_root = state.clone();
    let state_for_refresh = state.clone();
    let state_for_collapse = state.clone();

    let entries_for_click = entries_signal;
    let entries_for_dblclick = entries_signal;
    let entries_for_home = entries_signal;
    let entries_for_root = entries_signal;
    let entries_for_refresh = entries_signal;
    let entries_for_collapse = entries_signal;

    let root_path_for_click = root_path_signal;
    let root_path_for_home = root_path_signal;
    let root_path_for_root = root_path_signal;
    let root_path_for_refresh = root_path_signal;

    let on_file_open_clone = on_file_open.clone();

    // Click handler - select item, toggle folder, or navigate up for ".."
    let on_click = move |path: PathBuf| {
        let mut state = state_for_click.borrow_mut();

        // Check if this is a parent navigation - if we're clicking on a path
        // that is the parent of our current root, we should navigate up
        if Some(path.as_path()) == state.root_path.parent() {
            // Navigate to parent directory
            state.set_root(path);
            root_path_for_click.set(state.root_path_display());
        } else if path.is_dir() {
            // Regular directory - toggle expansion
            state.toggle_folder(&path);
            state.select(Some(path));
        } else {
            // File - just select
            state.select(Some(path));
        }

        // Update entries
        let new_entries: Vec<FileEntryDisplay> = state
            .build_entry_list()
            .iter()
            .map(|e| FileEntryDisplay::from_entry(e, state.is_selected(&e.path)))
            .collect();
        entries_for_click.set(new_entries);
    };

    // Double-click handler - open file or navigate into directory
    let on_double_click = move |path: PathBuf| {
        if path.is_dir() {
            // Double-click on directory - navigate into it (set as new root)
            let mut state = state_for_dblclick.borrow_mut();
            state.set_root(path);

            let new_entries: Vec<FileEntryDisplay> = state
                .build_entry_list()
                .iter()
                .map(|e| FileEntryDisplay::from_entry(e, state.is_selected(&e.path)))
                .collect();
            entries_for_dblclick.set(new_entries);
        } else {
            // File - select and open
            {
                let mut state = state_for_dblclick.borrow_mut();
                state.select(Some(path.clone()));

                let new_entries: Vec<FileEntryDisplay> = state
                    .build_entry_list()
                    .iter()
                    .map(|e| FileEntryDisplay::from_entry(e, state.is_selected(&e.path)))
                    .collect();
                entries_for_dblclick.set(new_entries);
            }

            // Call the file open callback
            on_file_open_clone(path);
        }
    };

    // Home navigation handler
    let on_home = move || {
        let mut state = state_for_home.borrow_mut();
        state.navigate_home();

        let new_entries: Vec<FileEntryDisplay> = state
            .build_entry_list()
            .iter()
            .map(|e| FileEntryDisplay::from_entry(e, state.is_selected(&e.path)))
            .collect();
        entries_for_home.set(new_entries);
        root_path_for_home.set(state.root_path_display());
    };

    // Root navigation handler
    let on_root = move || {
        let mut state = state_for_root.borrow_mut();
        state.navigate_root();

        let new_entries: Vec<FileEntryDisplay> = state
            .build_entry_list()
            .iter()
            .map(|e| FileEntryDisplay::from_entry(e, state.is_selected(&e.path)))
            .collect();
        entries_for_root.set(new_entries);
        root_path_for_root.set(state.root_path_display());
    };

    // Refresh handler
    let on_refresh = move || {
        let state = state_for_refresh.borrow();
        let new_entries: Vec<FileEntryDisplay> = state
            .build_entry_list()
            .iter()
            .map(|e| FileEntryDisplay::from_entry(e, state.is_selected(&e.path)))
            .collect();
        entries_for_refresh.set(new_entries);
        root_path_for_refresh.set(state.root_path_display());
    };

    // Collapse all handler
    let on_collapse_all = move || {
        let mut state = state_for_collapse.borrow_mut();
        state.collapse_all();

        let new_entries: Vec<FileEntryDisplay> = state
            .build_entry_list()
            .iter()
            .map(|e| FileEntryDisplay::from_entry(e, state.is_selected(&e.path)))
            .collect();
        entries_for_collapse.set(new_entries);
    };

    file_browser_panel_view(
        entries_signal,
        root_path_signal,
        theme,
        font_size,
        on_click,
        on_double_click,
        on_home,
        on_root,
        on_refresh,
        on_collapse_all,
        on_hide,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_browser::FileEntry;
    use std::path::PathBuf;

    #[test]
    fn a_file_entry_renders_its_name_and_kind() {
        let entry = FileEntry::new(PathBuf::from("/test/file.rs"), 1, false);
        let display = FileEntryDisplay::from_entry(&entry, false);

        assert_eq!(display.name, "file.rs");
        assert_eq!(display.indent_level, 1);
        assert!(!display.is_selected);
    }

    #[test]
    fn a_file_entry_shows_an_icon_for_its_kind() {
        let file_display = FileEntryDisplay {
            key: "file".to_string(),
            path: PathBuf::from("/test/file.rs"),
            name: "file.rs".to_string(),
            is_dir: false,
            indent_level: 0,
            is_expanded: false,
            is_selected: false,
            is_parent_nav: false,
        };
        assert_eq!(file_display.icon(), "\u{1F9E0} "); // 🧠 Rust

        // Test non-special file gets plain icon
        let txt_display = FileEntryDisplay {
            name: "notes.txt".to_string(),
            ..file_display.clone()
        };
        assert_eq!(txt_display.icon(), "  ");

        let collapsed_dir = FileEntryDisplay {
            key: "dir".to_string(),
            path: PathBuf::from("/test/src"),
            name: "src".to_string(),
            is_dir: true,
            indent_level: 0,
            is_expanded: false,
            is_selected: false,
            is_parent_nav: false,
        };
        assert_eq!(collapsed_dir.icon(), "\u{25B8} "); // ▸

        let expanded_dir = FileEntryDisplay {
            is_expanded: true,
            ..collapsed_dir
        };
        assert_eq!(expanded_dir.icon(), "\u{25BE} "); // ▾

        let parent_nav = FileEntryDisplay {
            key: "parent".to_string(),
            path: PathBuf::from("/test"),
            name: "..".to_string(),
            is_dir: true,
            indent_level: 0,
            is_expanded: false,
            is_selected: false,
            is_parent_nav: true,
        };
        assert_eq!(parent_nav.icon(), "\u{2190} "); // ←
    }

    /// The icon chain is ordered, so a general extension arm placed above a
    /// specific filename silently swallows it. `Cargo.toml` shipped showing the
    /// generic config gear and `Cargo.lock` the generic padlock, because both
    /// extension arms sat above the exact-name arm. Asserting the requirement
    /// rather than the chain: a Cargo manifest is identifiable as one.
    #[test]
    fn cargo_manifests_are_distinguishable_from_ordinary_toml_and_lock_files() {
        let entry = |name: &str| FileEntryDisplay {
            key: name.to_string(),
            path: PathBuf::from("/p").join(name),
            name: name.to_string(),
            is_dir: false,
            indent_level: 0,
            is_expanded: false,
            is_selected: false,
            is_parent_nav: false,
        };

        let cargo_toml = entry("Cargo.toml").icon();
        let cargo_lock = entry("Cargo.lock").icon();

        assert_eq!(cargo_toml, cargo_lock, "both Cargo files read as one thing");
        assert_ne!(
            cargo_toml,
            entry("rustfmt.toml").icon(),
            "Cargo.toml must not look like any other .toml"
        );
        assert_ne!(
            cargo_lock,
            entry("flake.lock").icon(),
            "Cargo.lock must not look like any other .lock"
        );
    }
}
