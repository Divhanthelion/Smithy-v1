//! Editor view for Smithy
//!
//! This module handles the editor UI and file operations.

use std::cell::RefCell;
use std::rc::Rc;

use floem::prelude::*;
use floem::reactive::RwSignal;

use smithy_editor::{buffer::BufferId, BufferManager, BufferState, LspHandle};

/// What one user gesture is trying to close.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseTarget {
    Tab(BufferId),
    Window,
}

/// The complete close decision waiting for the user.
///
/// The displayed set is a prompt snapshot, never authority. Every button and
/// the final commit rebuild it from retained documents; revisions make a later
/// edit a different set even when it touched the same files.
#[derive(Clone, Debug)]
pub struct CloseIntent {
    pub target: CloseTarget,
    pub dirty: Vec<BufferId>,
    documents: Vec<CloseDocument>,
    pub error: Option<String>,
    pub conflict: Option<BufferId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CloseDocument {
    id: BufferId,
    revision: u64,
}

impl CloseIntent {
    pub fn new(
        target: CloseTarget,
        sessions: &smithy_editor::EditorSessions,
        manager: &BufferManager,
    ) -> Self {
        let dirty: Vec<BufferId> = match target {
            CloseTarget::Tab(id) => {
                (manager.get_buffer(id).is_some() && sessions.is_dirty(id))
                    .then_some(id)
                    .into_iter()
                    .collect()
            }
            // The retained registry is authoritative for write safety. Looking
            // only at manager rows would miss exactly the split-ownership
            // failure the registry was introduced to prevent.
            CloseTarget::Window => sessions.dirty_ids(),
        };
        let documents = dirty
            .iter()
            .filter_map(|&id| {
                sessions.get(id).map(|handle| CloseDocument {
                    id,
                    revision: handle.revision(),
                })
            })
            .collect();
        Self {
            target,
            dirty,
            documents,
            error: None,
            conflict: None,
        }
    }

    fn refreshed(
        &self,
        sessions: &smithy_editor::EditorSessions,
        manager: &BufferManager,
        error: impl Into<String>,
    ) -> Self {
        let mut current = Self::new(self.target, sessions, manager);
        current.error = Some(error.into());
        current
    }

    fn refreshed_conflict(
        &self,
        sessions: &smithy_editor::EditorSessions,
        manager: &BufferManager,
        id: BufferId,
        error: impl Into<String>,
    ) -> Self {
        let mut current = self.refreshed(sessions, manager, error);
        current.conflict = Some(id);
        current
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseChoice {
    KeepEditing,
    SaveAndClose,
    DiscardAndClose,
}

#[derive(Clone, Debug)]
pub enum CloseResolution {
    KeepOpen,
    Refresh(CloseIntent),
    Commit(CloseIntent),
}

/// Resolve the user's explicit choice without removing a tab yet.
pub fn resolve_close(
    intent: &CloseIntent,
    choice: CloseChoice,
    sessions: &smithy_editor::EditorSessions,
    manager: &BufferManager,
) -> CloseResolution {
    match choice {
        CloseChoice::KeepEditing => CloseResolution::KeepOpen,
        CloseChoice::DiscardAndClose => {
            let current = CloseIntent::new(intent.target, sessions, manager);
            if current.documents != intent.documents {
                CloseResolution::Refresh(intent.refreshed(
                    sessions,
                    manager,
                    "Unsaved edits changed while this prompt was open. Review the current files, \
                     then choose Discard and close again.",
                ))
            } else {
                CloseResolution::Commit(current)
            }
        }
        CloseChoice::SaveAndClose => {
            let current = CloseIntent::new(intent.target, sessions, manager);
            for document in &current.documents {
                let Some(handle) = sessions.get(document.id) else {
                    return CloseResolution::Refresh(intent.refreshed(
                        sessions,
                        manager,
                        "An editor document disappeared before it could be saved.",
                    ));
                };
                if let Err(error) = handle.save_revision(document.revision) {
                    return CloseResolution::Refresh(if error.is_conflict() {
                        intent.refreshed_conflict(
                            sessions,
                            manager,
                            document.id,
                            error.to_string(),
                        )
                    } else {
                        intent.refreshed(sessions, manager, error.to_string())
                    });
                }
            }
            let after_save = CloseIntent::new(intent.target, sessions, manager);
            if after_save.dirty.is_empty() {
                CloseResolution::Commit(after_save)
            } else {
                CloseResolution::Refresh(intent.refreshed(
                    sessions,
                    manager,
                    "A document changed while files were being saved. The window remains open.",
                ))
            }
        }
    }
}

/// Builds the editor pane.
#[derive(Clone, Copy)]
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
        // The project map, shown behind the shortcuts when nothing is open.
        project_map: RwSignal<String>,
        active_buffer: RwSignal<Option<BufferId>>,
        buffer_manager: Rc<RefCell<BufferManager>>,
        open_editor: RwSignal<Option<smithy_editor::EditorHandle>>,
        sessions: smithy_editor::EditorSessions,
        editor_version: RwSignal<u64>,
    ) -> impl IntoView {
        let sessions_for_rows = sessions.clone();
        let rows = dyn_stack(
            move || {
                editor_version.get();
                buffer_manager
                    .try_borrow()
                    .map(|manager| {
                        manager
                            .buffer_ids()
                            .filter_map(|id| {
                                let buffer = manager.get_buffer(id)?;
                                let buffer = buffer.try_borrow().ok()?;
                                Some((
                                    id,
                                    buffer.path().cloned().unwrap_or_default(),
                                    buffer.text().to_string(),
                                ))
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
            },
            |(id, _, _)| *id,
            move |(id, path, content)| {
                let (view, handle) = smithy_editor::code_editor(path, &content);
                sessions_for_rows.register(id, handle.clone());
                if active_buffer.get_untracked() == Some(id) {
                    open_editor.set(Some(handle));
                }
                Container::new(view).style(move |s| {
                    let s = s.width_full().height_full().min_height(0.0);
                    if active_buffer.get() == Some(id) {
                        s
                    } else {
                        s.display(floem::taffy::Display::None)
                    }
                })
            },
        );
        let empty = Container::new(smithy_editor::empty_editor_with_map(project_map)).style(
            move |s| {
                let s = s.width_full().height_full().min_height(0.0);
                if active_buffer.get().is_none() {
                    s
                } else {
                    s.display(floem::taffy::Display::None)
                }
            },
        );
        Stack::new((rows, empty))
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
    let already_open = manager.get_buffer_by_path(&path).is_some();
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

    if !already_open {
        if let Some((content, language_id)) = for_lsp {
        lsp_handle.file_opened(path, language_id, content);
        }
    }
}

/// Update buffer states signal from manager
fn update_buffer_states(manager: &BufferManager, buffer_states: RwSignal<Vec<BufferState>>) {
    let previous = buffer_states.get_untracked();
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
                    // `Buffer` is load metadata, not the editing document. Keep
                    // the last document-derived value until the session registry
                    // reports another revision.
                    is_dirty: previous
                        .iter()
                        .find(|state| state.id == id)
                        .is_some_and(|state| state.is_dirty),
                    path: buffer.path().map(|p| p.display().to_string()),
                }
            })
        })
        .collect();
    buffer_states.set(states);
}

/// Switch to an already-open buffer.
///
/// Setting `active_buffer` changes which retained editor view is visible. The
/// per-tab document stays registered, including undo history and dirty text.
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

/// Commit a close only after clean state or an explicit dirty-buffer choice.
pub fn commit_close(
    intent: &CloseIntent,
    buffer_manager: &Rc<RefCell<BufferManager>>,
    active_buffer: RwSignal<Option<BufferId>>,
    buffer_states: RwSignal<Vec<BufferState>>,
    editor_version: RwSignal<u64>,
    sessions: &smithy_editor::EditorSessions,
    lsp_handle: &LspHandle,
) -> Result<(), CloseIntent> {
    let mut manager = buffer_manager.borrow_mut();
    let current = CloseIntent::new(intent.target, sessions, &manager);
    if current.documents != intent.documents {
        return Err(intent.refreshed(
            sessions,
            &manager,
            "Unsaved edits changed immediately before close. Review the current files and choose \
             again.",
        ));
    }
    let ids = match intent.target {
        CloseTarget::Tab(id) => vec![id],
        CloseTarget::Window => manager.buffer_ids().collect(),
    };
    for id in ids {
        let path = manager.get_buffer(id).and_then(|buffer| {
            buffer
                .try_borrow()
                .ok()
                .and_then(|buffer| buffer.path().cloned())
        });
        if manager.commit_close_buffer(id) {
            sessions.remove(id);
            if let Some(path) = path {
                lsp_handle.file_closed(path);
            }
        }
    }

    let active_id = manager.active_id();
    active_buffer.set(active_id);

    update_buffer_states(&manager, buffer_states);
    editor_version.update(|version| *version += 1);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::unbounded;
    use floem::views::editor::core::command::EditCommand;

    fn dirty_session(
        path: std::path::PathBuf,
    ) -> (
        smithy_editor::EditorSessions,
        Rc<RefCell<BufferManager>>,
        BufferId,
    ) {
        std::fs::write(&path, "saved\n").unwrap();
        let manager = Rc::new(RefCell::new(BufferManager::new()));
        let id = manager.borrow_mut().open_file(&path).unwrap();
        let sessions = smithy_editor::EditorSessions::new();
        let (_view, handle) = smithy_editor::code_editor(path, "saved\n");
        handle.select_all();
        handle.run_edit(EditCommand::DeleteSelection);
        assert!(handle.is_dirty(), "fixture must contain unsaved text");
        sessions.register(id, handle);
        (sessions, manager, id)
    }

    /// Opening another tab rebuilds the tab rows from `BufferManager`, whose
    /// load-only buffers never receive document edits. That rebuild must retain
    /// the last real document-derived dirty value instead of resetting it false.
    #[test]
    fn rebuilding_buffer_states_does_not_reset_document_dirty_state() {
        let mut manager = BufferManager::new();
        let id = manager.create_buffer();
        let states = RwSignal::new(vec![BufferState {
            id,
            name: "Untitled".into(),
            is_dirty: true,
            path: None,
        }]);

        update_buffer_states(&manager, states);

        assert!(states.get_untracked()[0].is_dirty);
    }

    /// A clean tab has no decision to ask for. It should pass through the same
    /// commit path immediately and release both the manager and LSP lifecycle.
    #[test]
    fn a_clean_tab_closes_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clean.rs");
        std::fs::write(&path, "clean\n").unwrap();
        let manager = Rc::new(RefCell::new(BufferManager::new()));
        let id = manager.borrow_mut().open_file(&path).unwrap();
        manager.borrow_mut().set_active(Some(id));
        let sessions = smithy_editor::EditorSessions::new();
        let (_view, handle) = smithy_editor::code_editor(path, "clean\n");
        sessions.register(id, handle);
        let active = RwSignal::new(Some(id));
        let states = RwSignal::new(Vec::new());
        let version = RwSignal::new(0);
        let (tx, rx) = unbounded();
        let lsp = LspHandle::new(tx);
        let intent = CloseIntent::new(CloseTarget::Tab(id), &sessions, &manager.borrow());

        assert!(intent.dirty.is_empty());
        commit_close(&intent, &manager, active, states, version, &sessions, &lsp).unwrap();

        assert_eq!(manager.borrow().buffer_count(), 0);
        assert!(sessions.get(id).is_none());
        assert!(matches!(
            rx.recv().unwrap(),
            smithy_editor::lsp::LspRequest::FileClosed { .. }
        ));
        assert!(rx.try_recv().is_err());
    }

    /// Cancel is the safety choice: reaching it must not save or authorize the
    /// later commit stage.
    #[test]
    fn keep_editing_leaves_a_dirty_close_uncommitted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keep.rs");
        let (sessions, manager, id) = dirty_session(path.clone());
        let intent = CloseIntent::new(CloseTarget::Tab(id), &sessions, &manager.borrow());

        assert!(matches!(
            resolve_close(
                &intent,
                CloseChoice::KeepEditing,
                &sessions,
                &manager.borrow()
            ),
            CloseResolution::KeepOpen
        ));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "saved\n");
        assert!(sessions.is_dirty(id));
    }

    /// Save and close must write the retained document, not `BufferManager`'s
    /// stale load-time text, before authorizing removal.
    #[test]
    fn save_and_close_persists_the_retained_document_before_commit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("save.rs");
        let (sessions, manager, id) = dirty_session(path.clone());
        let intent = CloseIntent::new(CloseTarget::Tab(id), &sessions, &manager.borrow());

        assert!(matches!(
            resolve_close(
                &intent,
                CloseChoice::SaveAndClose,
                &sessions,
                &manager.borrow()
            ),
            CloseResolution::Commit(_)
        ));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "");
        assert!(!sessions.is_dirty(id));
    }

    /// Discard is explicit permission to remove the tab without touching disk;
    /// an undecided dialog can never arrive here.
    #[test]
    fn discard_and_close_authorizes_commit_without_saving() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("discard.rs");
        let (sessions, manager, id) = dirty_session(path.clone());
        let intent = CloseIntent::new(CloseTarget::Tab(id), &sessions, &manager.borrow());

        assert!(matches!(
            resolve_close(
                &intent,
                CloseChoice::DiscardAndClose,
                &sessions,
                &manager.borrow()
            ),
            CloseResolution::Commit(_)
        ));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "saved\n");
    }

    /// The editor remains live while the native close prompt is visible. A
    /// keyboard edit made after it opened must not be covered by the earlier
    /// discard click; the refreshed prompt requires explicit consent again.
    #[test]
    fn an_edit_after_the_prompt_opens_requires_a_fresh_discard_choice() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("later-edit.rs");
        let (sessions, manager, id) = dirty_session(path);
        let original = CloseIntent::new(CloseTarget::Window, &sessions, &manager.borrow());
        let handle = sessions.get(id).unwrap();
        handle.undo();
        handle.redo();

        let CloseResolution::Refresh(refreshed) = resolve_close(
            &original,
            CloseChoice::DiscardAndClose,
            &sessions,
            &manager.borrow(),
        ) else {
            panic!("the stale discard decision must refresh the prompt");
        };
        assert!(refreshed.error.is_some());
        assert!(matches!(
            resolve_close(
                &refreshed,
                CloseChoice::DiscardAndClose,
                &sessions,
                &manager.borrow()
            ),
            CloseResolution::Commit(_)
        ));
    }

    /// Save is evaluated at the click, not from text captured when the prompt
    /// appeared. Otherwise a later edit can be overwritten by the stale save
    /// and disappear with the closing window.
    #[test]
    fn save_and_close_writes_an_edit_made_after_the_prompt_opened() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("save-later.rs");
        let (sessions, manager, id) = dirty_session(path.clone());
        let original = CloseIntent::new(CloseTarget::Window, &sessions, &manager.borrow());
        sessions
            .get(id)
            .unwrap()
            .run_edit(EditCommand::InsertNewLine);

        assert!(matches!(
            resolve_close(
                &original,
                CloseChoice::SaveAndClose,
                &sessions,
                &manager.borrow()
            ),
            CloseResolution::Commit(_)
        ));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "\n");
    }

    /// Even an explicit discard token is checked once more at commit. This
    /// closes the last event-ordering gap between button resolution and tab
    /// removal.
    #[test]
    fn an_edit_after_discard_resolution_still_blocks_commit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("commit-race.rs");
        let (sessions, manager, id) = dirty_session(path);
        let prompt = CloseIntent::new(CloseTarget::Window, &sessions, &manager.borrow());
        let CloseResolution::Commit(authorized) = resolve_close(
            &prompt,
            CloseChoice::DiscardAndClose,
            &sessions,
            &manager.borrow(),
        ) else {
            panic!("unchanged prompt should authorize discard");
        };
        sessions
            .get(id)
            .unwrap()
            .run_edit(EditCommand::InsertNewLine);
        let active = RwSignal::new(Some(id));
        let states = RwSignal::new(Vec::new());
        let version = RwSignal::new(0);
        let (tx, rx) = unbounded();
        let lsp = LspHandle::new(tx);

        let refreshed = commit_close(
            &authorized,
            &manager,
            active,
            states,
            version,
            &sessions,
            &lsp,
        )
        .unwrap_err();

        assert!(refreshed.error.is_some());
        assert_eq!(manager.borrow().buffer_count(), 1);
        assert!(rx.try_recv().is_err(), "a blocked commit must send no didClose");
    }

    /// A failed save used to be indistinguishable from a successful click and
    /// the force-close path still removed the only copy of the edits.
    #[test]
    fn a_failed_save_refuses_close_and_keeps_the_document_dirty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gone").join("failed.rs");
        let manager = Rc::new(RefCell::new(BufferManager::new()));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "saved\n").unwrap();
        let id = manager.borrow_mut().open_file(&path).unwrap();
        let sessions = smithy_editor::EditorSessions::new();
        let (_view, handle) = smithy_editor::code_editor(path.clone(), "saved\n");
        handle.select_all();
        handle.run_edit(EditCommand::DeleteSelection);
        sessions.register(id, handle);
        let intent = CloseIntent::new(CloseTarget::Tab(id), &sessions, &manager.borrow());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();

        let CloseResolution::Refresh(refreshed) = resolve_close(
            &intent,
            CloseChoice::SaveAndClose,
            &sessions,
            &manager.borrow(),
        ) else {
            panic!("failed save must refresh the prompt");
        };
        assert!(refreshed.error.is_some());
        assert_eq!(refreshed.conflict, Some(id));
        assert!(sessions.is_dirty(id));
    }

    /// Save-and-close shares the same exact disk base as ordinary Save. A stale
    /// old-root file must refresh the prompt and retain the tab rather than
    /// turning close confirmation into an overwrite bypass.
    #[test]
    fn save_and_close_conflict_keeps_the_tab_open_for_reload_or_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("close-conflict.rs");
        let (sessions, manager, id) = dirty_session(path.clone());
        let intent = CloseIntent::new(CloseTarget::Tab(id), &sessions, &manager.borrow());
        std::fs::write(&path, "external\n").unwrap();

        let CloseResolution::Refresh(refreshed) = resolve_close(
            &intent,
            CloseChoice::SaveAndClose,
            &sessions,
            &manager.borrow(),
        ) else {
            panic!("stale save-and-close must refresh the prompt");
        };

        assert_eq!(refreshed.conflict, Some(id));
        assert_eq!(manager.borrow().buffer_count(), 1);
        assert!(sessions.is_dirty(id));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "external\n");
    }

    /// Window close must inspect every retained document, not only the active
    /// editor signal; otherwise an inactive dirty tab disappears without a
    /// prompt when the title-bar close button is used.
    #[test]
    fn window_close_collects_multiple_dirty_retained_documents() {
        let dir = tempfile::tempdir().unwrap();
        let (sessions, manager, first) = dirty_session(dir.path().join("first.rs"));
        let second_path = dir.path().join("second.rs");
        std::fs::write(&second_path, "saved\n").unwrap();
        let second = manager.borrow_mut().open_file(&second_path).unwrap();
        let (_view, second_handle) = smithy_editor::code_editor(second_path, "saved\n");
        second_handle.select_all();
        second_handle.run_edit(EditCommand::DeleteSelection);
        sessions.register(second, second_handle);
        let intent = CloseIntent::new(CloseTarget::Window, &sessions, &manager.borrow());
        assert_eq!(intent.dirty.len(), 2);
        assert!(intent.dirty.contains(&first));
        assert!(intent.dirty.contains(&second));
    }

    /// Selecting an already-open tab is not a second protocol open. After a
    /// committed close, reopening is a genuinely new lifecycle and emits one
    /// new didOpen, with one didClose between them.
    #[test]
    fn did_open_and_did_close_are_emitted_once_per_open_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lifecycle.rs");
        std::fs::write(&path, "fn main() {}\n").unwrap();
        let manager = Rc::new(RefCell::new(BufferManager::new()));
        let active = RwSignal::new(None);
        let states = RwSignal::new(Vec::new());
        let version = RwSignal::new(0);
        let sessions = smithy_editor::EditorSessions::new();
        let (tx, rx) = unbounded();
        let lsp = LspHandle::new(tx);
        lsp.initialize(dir.path().to_path_buf());

        handle_file_open(path.clone(), &manager, active, states, version, &lsp);
        handle_file_open(path.clone(), &manager, active, states, version, &lsp);
        let id = active.get_untracked().unwrap();
        let intent = CloseIntent::new(CloseTarget::Tab(id), &sessions, &manager.borrow());
        commit_close(&intent, &manager, active, states, version, &sessions, &lsp).unwrap();
        handle_file_open(path, &manager, active, states, version, &lsp);

        let requests: Vec<_> = rx.try_iter().collect();
        assert_eq!(
            requests
                .iter()
                .filter(|request| matches!(request, smithy_editor::lsp::LspRequest::FileOpened { .. }))
                .count(),
            2
        );
        assert_eq!(
            requests
                .iter()
                .filter(|request| matches!(request, smithy_editor::lsp::LspRequest::FileClosed { .. }))
                .count(),
            1
        );
    }
}
