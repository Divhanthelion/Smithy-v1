//! The editor UI.
//!
//! ## The public surface is deliberately small
//!
//! Only the modules `apps/smithy` actually names by path are `pub`; everything
//! else is `pub(crate)` and reaches the outside through the re-exports below.
//!
//! That is not tidiness, it is how dead code gets found. This crate grew out of
//! `forge`, and it inherited a large public API of which a great deal had no
//! caller at all — a whole unsaved-changes confirmation path, an incremental
//! highlighter entry point, a line-ending model, several accessors. `pub` items
//! in a library are exempt from `dead_code`, so none of it warned; it was found
//! by reading every file. Behind `pub(crate)`, the compiler finds the next one,
//! and a crate whose stated state is zero warnings then cannot accumulate more.

pub(crate) mod aesthetic;
pub mod agent_panel;
pub mod buffer;
pub(crate) mod buffer_manager;
pub mod celestial;
pub mod clock;
pub(crate) mod code_editor;
pub mod design;
pub(crate) mod diff_view;
pub(crate) mod error;
pub(crate) mod file_browser;
pub(crate) mod file_browser_view;
pub mod file_dialog;
pub(crate) mod file_watcher;
pub mod fisherman;
pub(crate) mod forged;
pub(crate) mod highlight;
pub mod hotkey;
pub(crate) mod hover_popup;
pub(crate) mod localtime;
pub mod lsp;
pub(crate) mod main_layout;
pub(crate) mod menu_bar;
pub(crate) mod problems_panel;
pub(crate) mod review;
pub mod routine;
pub(crate) mod squiggle;
pub(crate) mod syntax_styling;
pub(crate) mod terminal;
pub(crate) mod terminal_tabs;
pub mod terminal_view;
pub(crate) mod theme;
pub mod tick;

// --- The public surface ---
//
// Exactly what `apps/smithy` uses, and nothing kept "in case". Anything added
// here without a caller is invisible to `dead_code` — which is how this crate
// came to carry a whole unsaved-changes confirmation path, an incremental
// highlighter entry point and a line-ending model that nothing had ever called.

pub use aesthetic::Aesthetic;
pub use agent_panel::{agent_panel, AgentPanelState, Entry as AgentEntry, StepStatus};
pub use buffer_manager::{BufferManager, BufferState};
pub use code_editor::{
    code_editor, empty_editor, external_change_bar, on_external_change, EditorHandle,
    ExternalChange, OnExternalChange,
};
pub use diff_view::{diff_modal, FileDiff};
pub use file_browser::FileBrowserState;
pub use file_browser_view::file_browser_view;
pub use file_watcher::{spawn_file_watcher, FileWatcherEvent, FileWatcherHandle, IdeFileChange};
pub use forged::{circuit_backdrop, forged_frame, shell_inset, shell_top_inset};
pub use hover_popup::{hover_popup, HoverState};
pub use lsp::{LspDiagnostic, LspHandle, LspManager, LspResponse};
pub use main_layout::{main_layout_view, LayoutTheme};
pub use menu_bar::{accel, menu_bar, menu_overlay, Menu, MenuBarState, MenuItem};
pub use problems_panel::{is_same_file, problems_panel, DiagnosticsState, ProblemRow};
pub use review::ChangeStatus;
pub use review::{content_with_accepted_hunks, PendingChangeManager, PendingFileChange};
pub use syntax_styling::{EditSpan, InlineDiagnostic};
pub use terminal::kill_all_shells;
pub use terminal_tabs::TerminalTabManager;
pub use terminal_view::floem_key_to_terminal_key;
pub use theme::catppuccin;
