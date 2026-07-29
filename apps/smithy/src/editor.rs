//! Editor view for Smithy
//!
//! This module handles the editor UI and file operations.

use std::cell::RefCell;
use std::rc::Rc;

use floem::prelude::*;
use floem::reactive::RwSignal;

use smithy_editor::{buffer::BufferId, BufferManager, BufferState, LspHandle};

/// Builds the editor pane.
pub struct EditorComponent;

impl EditorComponent {
    pub fn new() -> Self {
        Self
    }

    /// The editor pane.
    ///
    /// A fresh `code_editor` is built per file rather than swapping content into
    /// one: floem's document owns its undo history, and reusing it across files
    /// would let you undo your way into the previous file's contents.
    pub fn view(
        &self,
        active_buffer: RwSignal<Option<BufferId>>,
        buffer_manager: Rc<RefCell<BufferManager>>,
        open_editor: RwSignal<Option<smithy_editor::EditorHandle>>,
    ) -> impl IntoView {
        dyn_container(
            move || active_buffer.get(),
            move |id| match id {
                Some(id) => {
                    let loaded: Option<(std::path::PathBuf, String)> = buffer_manager
                        .try_borrow()
                        .ok()
                        .and_then(|bm| bm.get_buffer(id))
                        .and_then(|buffer| {
                            let buffer = buffer.try_borrow().ok()?;
                            Some((
                                buffer.path().cloned().unwrap_or_default(),
                                buffer.text().to_string(),
                            ))
                        });

                    match loaded {
                        Some((path, content)) => {
                            let (view, handle) = smithy_editor::code_editor(path, &content);
                            open_editor.set(Some(handle));
                            Box::new(
                                Container::new(view)
                                    .style(|s| s.width_full().height_full().min_height(0.0)),
                            ) as Box<dyn View>
                        }
                        None => {
                            Box::new(Container::new(smithy_editor::empty_editor())) as Box<dyn View>
                        }
                    }
                }
                None => {
                    open_editor.set(None);
                    Box::new(Container::new(smithy_editor::empty_editor())) as Box<dyn View>
                }
            },
        )
        .style(|s| s.width_full().height_full().min_height(0.0))
    }
}

/// Handle file opening
/// Open a file and make it the active buffer.
///
/// The previous version read the file into a *local* `Buffer`, then separately
/// called `manager.open_file()` — which creates its own buffer with its own id.
/// It then set `active_buffer` to the local id, which the manager had never
/// heard of. The tab bar (built from the manager) showed the file, the editor
/// (which looks the id up in the manager) found nothing, and the tab appeared
/// to open without becoming active. The manager's id is the only real one.
pub fn handle_file_open(
    path: std::path::PathBuf,
    buffer_manager: &Rc<RefCell<BufferManager>>,
    active_buffer: RwSignal<Option<BufferId>>,
    buffer_states: RwSignal<Vec<BufferState>>,
    editor_version: RwSignal<u64>,
    lsp_handle: &LspHandle,
) {
    let Ok(mut manager) = buffer_manager.try_borrow_mut() else {
        return;
    };

    // `open_file` is idempotent: re-opening an already-open file returns the
    // existing id rather than a duplicate buffer.
    let buffer_id = match manager.open_file(&path) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("could not open {}: {e}", path.display());
            return;
        }
    };

    manager.set_active(Some(buffer_id));
    update_buffer_states(&manager, buffer_states);

    // Read the content back out of the manager's buffer — the one the editor
    // will actually render — rather than from a second read of the file.
    let for_lsp = manager.get_buffer(buffer_id).and_then(|buffer| {
        let buffer = buffer.try_borrow().ok()?;
        Some((
            buffer.text().to_string(),
            buffer.language_id().unwrap_or("plaintext").to_string(),
        ))
    });
    drop(manager);

    active_buffer.set(Some(buffer_id));
    editor_version.update(|v| *v += 1);

    if let Some((content, language_id)) = for_lsp {
        lsp_handle.file_opened(path, language_id, content);
    }
}

/// Update buffer states signal from manager
fn update_buffer_states(manager: &BufferManager, buffer_states: RwSignal<Vec<BufferState>>) {
    let states: Vec<BufferState> = manager
        .buffer_ids()
        .filter_map(|id| {
            manager.get_buffer(id).map(|b| {
                let buffer = b.borrow();
                BufferState {
                    id,
                    name: buffer
                        .path()
                        .and_then(|p| p.file_name())
                        .and_then(|n| n.to_str())
                        .map(String::from)
                        .unwrap_or_else(|| "Untitled".to_string()),
                    is_dirty: buffer.is_dirty(),
                    path: buffer.path().map(|p| p.display().to_string()),
                }
            })
        })
        .collect();
    buffer_states.set(states);
}

/// Switch to an already-open buffer.
///
/// Setting `active_buffer` is the whole operation: the editor pane is a
/// `dyn_container` keyed on it, so changing it rebuilds the pane around the new
/// file. There is no separate editor state to keep in step.
pub fn handle_tab_click(
    id: BufferId,
    buffer_manager: &Rc<RefCell<BufferManager>>,
    active_buffer: RwSignal<Option<BufferId>>,
    editor_version: RwSignal<u64>,
) {
    let Ok(mut manager) = buffer_manager.try_borrow_mut() else {
        return;
    };
    if manager.get_buffer(id).is_some() {
        manager.set_active(Some(id));
        drop(manager);
        active_buffer.set(Some(id));
        editor_version.update(|v| *v += 1);
    }
}

/// Handle tab close - close buffer
pub fn handle_tab_close(
    id: BufferId,
    buffer_manager: &Rc<RefCell<BufferManager>>,
    active_buffer: RwSignal<Option<BufferId>>,
    buffer_states: RwSignal<Vec<BufferState>>,
) {
    let mut manager = buffer_manager.borrow_mut();
    manager.force_close_buffer(id);

    let active_id = manager.active_id();
    active_buffer.set(active_id);

    // Update tab bar
    update_buffer_states(&manager, buffer_states);
}
