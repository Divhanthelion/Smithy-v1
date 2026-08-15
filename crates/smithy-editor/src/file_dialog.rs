//! Native directory pickers, for opening a project.
//!
//! Deliberately only directories. This module used to carry a full
//! file-open/save-as apparatus — `show_open_dialog`, `show_save_dialog`, their
//! async twins, `FileFilters`, `OpenDialogResult`, `SaveDialogResult` — about
//! 200 lines of it, none of it ever called. Smithy opens *projects*, individual
//! files come from the Explorer, and saving goes to the path the buffer was
//! loaded from. There was no place left for a file dialog to be reached from.
//!
//! Its tests went with it, and they are worth recording as a warning:
//! `test_open_dialog_result_variants` constructed both variants of a two-variant
//! enum and asserted it had got them. It could not fail, and it passed for the
//! entire life of the dead code it was guarding.

use rfd::AsyncFileDialog;

/// Ask the user for a directory, without blocking the caller.
///
/// `rfd`'s blocking `FileDialog::pick_folder` runs a nested native event loop
/// (`NSOpenPanel.runModal()` on macOS). Calling it from a winit event handler
/// — click or key — re-enters AppKit while winit still holds the current
/// event, and winit aborts with "tried to handle event while another event is
/// currently being handled".
///
/// `AsyncFileDialog` presents a sheet (`beginSheetModalForWindow`) instead.
/// Construct it off the UI thread (a tokio spawn is enough) so the event
/// handler can return before the panel is attached.
pub async fn pick_folder_async() -> Option<std::path::PathBuf> {
    AsyncFileDialog::new()
        .set_title("Open Project")
        .pick_folder()
        .await
        .map(|handle| handle.path().to_path_buf())
}
