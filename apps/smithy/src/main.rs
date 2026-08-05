//! Smithy — a native Rust IDE with a local-first coding agent.
//!
//! The entry point: builds the window, wires every panel to the state in
//! [`app_state`], and owns the shortcuts and the modals that sit above them.

use floem::peniko::Color;
use floem::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use smithy_editor::{
    catppuccin, diff_modal, file_browser_view, main_layout_view, spawn_file_watcher, FileDiff,
    FileWatcherEvent, IdeFileChange,
};

mod agent;
mod app_state;
mod call_graph;
mod editor;
mod meters;
mod runtime;
mod settings;
mod terminal;
mod voice;

/// Temporary: log every key event a handler actually receives.
///
/// Off unless `SMITHY_KEY_DEBUG=1`. Here because static reading of the event
/// migration could not distinguish "the handler never fires" from "it fires but
/// the match fails" — and those have opposite fixes. Delete once the keyboard
/// regressions are closed.
pub fn key_debug(where_: &str, ev: &floem::prelude::KeyboardEvent) {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    if !*ON.get_or_init(|| std::env::var("SMITHY_KEY_DEBUG").is_ok_and(|v| v == "1")) {
        return;
    }
    eprintln!(
        "[key:{where_}] key={:?} modifiers={:?}",
        ev.key, ev.modifiers
    );
}

use app_state::{connect_agent, init_state, setup_agent_effect, submit_task};
use editor::{
    commit_close, handle_file_open, handle_tab_click, resolve_close, CloseChoice, CloseIntent,
    CloseResolution, CloseTarget, EditorComponent,
};
use terminal::MultiTerminalComponent;

/// Everything that has to be told to stop, reachable from a signal handler.
///
/// Deliberately only `Send` things. The panels, the tab manager and the buffers all
/// live in `Rc<RefCell<_>>` on floem's main thread and cannot be touched from here;
/// what needs stopping is a language server (reached by a channel) and a set of
/// shell processes (reached through `smithy_editor::kill_all_shells`).
struct ExitHooks {
    lsp: smithy_editor::LspHandle,
    ran: AtomicBool,
}

impl ExitHooks {
    /// Ask everything to stop. Idempotent, and safe on a signal path.
    fn run(&self) {
        if self.ran.swap(true, Ordering::AcqRel) {
            return;
        }
        // Asks the server to exit rather than killing it: `kill_on_drop` covers
        // the hard case, but it never gets the chance on a signal, and a server
        // told to shut down leaves no lock files or half-written caches behind.
        if !self.lsp.shutdown() {
            eprintln!("LSP shutdown worker did not acknowledge before exit");
        }
        // The one leak neither `kill_on_drop` nor the process group covers: a pty
        // child is in its own session, so `Ctrl-C` never reaches it.
        smithy_editor::kill_all_shells();
    }
}

static EXIT_HOOKS: OnceLock<Arc<ExitHooks>> = OnceLock::new();

fn run_exit_hooks() {
    if let Some(hooks) = EXIT_HOOKS.get() {
        hooks.run();
    }
}

/// Admit only events from the workspace and server lifetime the UI currently
/// represents. Status/Ready events are authoritative transitions; diagnostics
/// and request errors may describe only the already accepted process.
fn accept_lsp_stamp(
    project_root: &std::path::Path,
    accepted: &mut Option<smithy_editor::LspStamp>,
    incoming: &smithy_editor::LspStamp,
    authoritative: bool,
) -> bool {
    if incoming.root_path != project_root {
        return false;
    }
    let Some(current) = accepted.as_ref() else {
        *accepted = Some(incoming.clone());
        return true;
    };
    if current.root_path != project_root || incoming.generation > current.generation {
        *accepted = Some(incoming.clone());
        return true;
    }
    if incoming.generation < current.generation {
        return false;
    }
    if incoming.client_id == current.client_id {
        return true;
    }
    if authoritative || current.client_id.is_none() {
        *accepted = Some(incoming.clone());
        return true;
    }
    false
}

fn server_count_for_lsp_status(running: bool) -> usize {
    usize::from(running)
}

/// Install a `Ctrl-C` handler that runs the exit hooks before the process dies.
///
/// Without this, SIGINT terminates the process with no Rust cleanup at all — no
/// `Drop`, no LSP `shutdown`, and every pty shell reparented to `launchd`. Running
/// from a terminal and quitting with `Ctrl-C` is the normal way this app is used
/// during development, so it is the normal path, not the exceptional one.
fn install_signal_handler(hooks: Arc<ExitHooks>) {
    runtime::tokio_runtime().spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("shutting down…");
            hooks.run();
            std::process::exit(0);
        }
    });
}

/// Main application entry point
fn main() {
    // Log runtime configuration
    let info = runtime::runtime_info();
    eprintln!(
        "Smithy starting with {} worker threads",
        info.worker_threads
    );
    eprintln!("Set {}=<n> to configure worker threads", info.env_var);

    // Create the Floem application with the main view and set window title
    let config = floem::window::WindowConfig::default().title("Smithy");
    floem::Application::new()
        .window(app_view, Some(config))
        .run();

    // Native window close and application termination converge here. The same
    // idempotent path is used by Ctrl-C, so a platform that reports both cannot
    // race two shutdown handshakes.
    run_exit_hooks();
}

/// Create the main application view
fn app_view(window_id: floem::window::WindowId) -> impl IntoView {
    // Initialize all application state
    let (app_state, signals, agent_state) = init_state();

    // The backend settings dialog. Its signals are declared here rather than in
    // `init_state` because they are pure view state: the settings themselves
    // live on disk, and this is only what the form is currently showing.
    let settings_state = smithy_editor::SettingsState::new();
    let settings_dir = agent_state.registry.data_dir().to_path_buf();

    // The project map, shown behind the shortcuts when no file is open.
    //
    // The same text the agent gets as its project context — layout, dependencies,
    // module paths, public API — so an empty editor answers "what is in this
    // repository?" instead of only listing keyboard shortcuts. Extracted off the
    // UI thread because it shells out to `cargo metadata` and parses the tree.
    let project_map = agent_state.project_map;
    refresh_project_map(&agent_state, project_map);
    // A previously built graph for this project, if one is on disk. Never builds
    // here — that is an explicit menu action because of the cost.
    call_graph::load_for_project(&agent_state);

    // The open editor, declared before the file watcher because the watcher's
    // effect needs it: an external change is only interesting for the file
    // actually on screen.
    let open_editor = floem::reactive::RwSignal::new(None::<smithy_editor::EditorHandle>);
    let editor_sessions = smithy_editor::EditorSessions::new();
    let close_intent = floem::reactive::RwSignal::new(None::<CloseIntent>);
    // Raised only when a file changed on disk *and* has unsaved edits, which is
    // the one case where reloading and not reloading both cost something.
    let external_change = floem::reactive::RwSignal::new(None::<smithy_editor::ExternalChange>);

    // Start file watcher
    let file_browser_clone = app_state.file_browser.clone();
    let editor_version = signals.editor_version;
    let project_for_watch = agent_state.project.clone();
    let root_path = file_browser_clone.borrow().root_path.clone();

    // The handle is **kept**, not dropped. It used to be bound to `_`, which
    // dropped the command sender immediately — so `FileOpened`, `RebuildGitignore`
    // and everything else the watcher can be told were unreachable by
    // construction, and the watcher could never be re-rooted.
    if let Ok((watcher_handle, watcher_rx)) = spawn_file_watcher(root_path) {
        *agent_state.file_watcher.borrow_mut() = Some(watcher_handle);
        // A debounced batch per event, and several batches can land in one frame
        // during a `cargo build` — the previous bridge kept only the last, so an
        // externally-edited file quietly failed to reload.
        let (watcher_tick, watcher_inbox) = app_state::bridge(watcher_rx);

        floem::reactive::Effect::new(move |_| {
            watcher_tick.get();
            for event in app_state::drain(&watcher_inbox) {
                match event {
                    FileWatcherEvent::Changes(changes) => {
                        let mut needs_fb_refresh = false;
                        for change in changes {
                            match change {
                                IdeFileChange::FileModified { path, .. } => {
                                    // The `external` flag is deliberately
                                    // ignored. It was `!is_file_open`, which is
                                    // backwards — a change to a file you have
                                    // open is precisely the one worth reacting
                                    // to — and nothing ever populated the open
                                    // set anyway. Comparing content answers the
                                    // question it was trying to answer, exactly:
                                    // after Smithy's own save the disk and the
                                    // buffer agree, so there is nothing to do.
                                    let Some(handle) = open_editor.get_untracked() else {
                                        continue;
                                    };
                                    // The policy is `on_external_change`: a pure
                                    // function with the table in its own docs.
                                    // Only the three answers it needs are
                                    // computed here.
                                    match smithy_editor::on_external_change(
                                        smithy_editor::is_same_file(&handle.path, &path),
                                        handle.matches_disk(),
                                        handle.is_dirty(),
                                    ) {
                                        smithy_editor::OnExternalChange::Ignore => {}
                                        smithy_editor::OnExternalChange::Reload => {
                                            if let Err(e) = handle.reload_from_disk() {
                                                eprintln!("{e}");
                                            }
                                        }
                                        smithy_editor::OnExternalChange::Ask => {
                                            external_change.set(Some(
                                                smithy_editor::ExternalChange {
                                                    label: smithy_editor::ProblemRow::label_for(
                                                        &path,
                                                        &project_for_watch.borrow().root,
                                                    ),
                                                },
                                            ));
                                        }
                                    }
                                }
                                IdeFileChange::FileCreated { .. }
                                | IdeFileChange::FileDeleted { .. }
                                | IdeFileChange::FileRenamed { .. }
                                | IdeFileChange::DirectoryChanged { .. } => {
                                    needs_fb_refresh = true;
                                }
                                _ => {}
                            }
                        }
                        if needs_fb_refresh {
                            if let Ok(mut fb) = file_browser_clone.try_borrow_mut() {
                                fb.refresh();
                            }
                        }
                    }
                    FileWatcherEvent::Error(e) => {
                        eprintln!("File watcher error: {}", e);
                    }
                }
            }
        });
    }

    // Installed here rather than in `main`, because this is where the LSP handle
    // exists. One handler for the process; `app_view` runs once.
    let exit_hooks = Arc::new(ExitHooks {
        lsp: app_state.lsp_handle.clone(),
        ran: AtomicBool::new(false),
    });
    let _ = EXIT_HOOKS.set(exit_hooks.clone());
    install_signal_handler(exit_hooks);

    // Translate agent events into panel state, then connect in the background.
    setup_agent_effect(agent_state.clone());
    app_state::setup_shell_approval_effect(agent_state.clone());
    app_state::setup_auto_approve_effect(agent_state.clone());
    connect_agent(&agent_state);

    // The microphone. Built once and shared: the button and the hotkey must
    // press the *same* handle, or one of them silently does nothing — which is
    // what happened, and only an unused-`mut` warning gave it away.
    //
    // Always present. It touches no audio hardware here — the input device is
    // found at the press that needs one, so a headset connected after launch
    // works without a restart.
    let voice_control = voice::VoiceControl::new(agent_state.panel.voice, agent_state.panel.input);
    // Read once. A hotkey is a preference, not a live input, and re-reading a
    // file on every keystroke would be a strange thing to do.
    let voice_hotkey = smithy_editor::hotkey::Hotkey::load(agent_state.registry.data_dir());

    // The clock, off until asked for and remembered after that.
    let clock_visible = floem::reactive::RwSignal::new(smithy_editor::clock::ClockVisible::load(
        agent_state.registry.data_dir(),
    ));
    let clock_data_dir = agent_state.registry.data_dir().to_path_buf();
    floem::reactive::Effect::new(move |_| {
        let _ = smithy_editor::clock::ClockVisible::save(&clock_data_dir, clock_visible.get());
    });

    // Create editor component
    let editor_component = EditorComponent::new();

    // --- Hover and go-to-definition ---
    let hover_state = smithy_editor::HoverState::new();
    // Changes when the semantic workspace changes even if an absolute path and
    // document revision happen to be identical across the transition.
    let hover_epoch = floem::reactive::RwSignal::new(0u64);

    {
        let hover_state = hover_state.clone();
        floem::reactive::Effect::new(move |_| {
            let epoch = hover_epoch.get();
            let document = open_editor.get().map(|handle| smithy_editor::HoverDocument {
                epoch,
                path: handle.path.clone(),
                revision: handle.revision.get(),
            });
            hover_state.bind_document(document);
        });
    }

    // Fire a request at the caret. Both features are the same shape: read the
    // caret, ask the server, wait for the answer on the response channel.
    let ask_hover = {
        let lsp_handle = app_state.lsp_handle.clone();
        let hover_state = hover_state.clone();
        move || {
            let Some(handle) = open_editor.get_untracked() else {
                return;
            };
            let (line, column) = handle.caret_position();
            let document = smithy_editor::HoverDocument {
                epoch: hover_epoch.get_untracked(),
                path: handle.path.clone(),
                revision: handle.revision(),
            };
            let request_id = lsp_handle.hover(handle.path.clone(), line, column);
            hover_state.request_started(request_id, document);
        }
    };

    let ask_definition = {
        let lsp_handle = app_state.lsp_handle.clone();
        let hover_state = hover_state.clone();
        move || {
            let Some(handle) = open_editor.get_untracked() else {
                return;
            };
            // Definition supersedes the semantic popup at this caret.
            hover_state.dismiss();
            let (line, column) = handle.caret_position();
            lsp_handle.goto_definition(handle.path.clone(), line, column);
        }
    };

    // Set by anything that opens a file at a position; applied when the new
    // editor appears.
    let pending_jump = floem::reactive::RwSignal::new(None::<u32>);
    let hover_state_for_keys = hover_state.clone();
    let ask_hover_menu = ask_hover.clone();
    let ask_definition_menu = ask_definition.clone();

    // Tell the language server about every edit.
    //
    // We sent `didOpen` and then nothing, so rust-analyzer's view of the file
    // was frozen at the moment it opened: diagnostics went stale on the first
    // keystroke and reported errors against text that no longer existed.
    {
        let lsp_handle = app_state.lsp_handle.clone();
        floem::reactive::Effect::new(move |_| {
            let Some(handle) = open_editor.get() else {
                return;
            };
            let version = handle.revision.get();
            // Skip the initial state: `didOpen` already carried that content,
            // and re-sending it would be a redundant round-trip on every file
            // you merely look at.
            if version == 0 {
                return;
            }
            lsp_handle.file_changed(handle.path.clone(), version as i32, handle.text());
        });
    }

    // A prompt about a file you are no longer looking at is worse than none.
    // Depends only on which file is open, so typing does not clear it — the
    // question stays until it is answered or made moot.
    floem::reactive::Effect::new(move |_| {
        open_editor.get();
        external_change.set(None);
    });

    // Keep the tab's dirty dot in step with the editor.
    //
    // `BufferState::is_dirty` was populated from our own `Buffer`, which is no
    // longer where edits land — floem's document owns the text now, so the flag
    // never moved. The editor's revision counter is the signal to follow.
    {
        let active_buffer = signals.active_buffer;
        let buffer_states = signals.buffer_states;
        floem::reactive::Effect::new(move |_| {
            let Some(handle) = open_editor.get() else {
                return;
            };
            handle.revision.get(); // re-run on every edit
            let dirty = handle.is_dirty();
            let Some(active) = active_buffer.get_untracked() else {
                return;
            };
            buffer_states.update(|states| {
                if let Some(state) = states.iter_mut().find(|s| s.id == active) {
                    state.is_dirty = dirty;
                }
            });
        });
    }

    // The active handle is a pointer into the durable per-tab registry. Tab
    // switches change focus; they do not recreate documents or discard text.
    {
        let sessions = editor_sessions.clone();
        let active = signals.active_buffer;
        let editor_version = signals.editor_version;
        floem::reactive::Effect::new(move |_| {
            active.get();
            editor_version.get();
            open_editor.set(active.get_untracked().and_then(|id| sessions.get(id)));
        });
    }

    // Center pane: editor, or the call graph. A mode switch rather than a fourth
    // splitter — the layout is already at three panes.
    let cg_ui = agent_state.call_graph;
    let project_root_sig =
        floem::reactive::RwSignal::new(agent_state.project.borrow().root.clone());
    {
        let project = agent_state.project.clone();
        floem::reactive::Effect::new(move |_| {
            let _ = cg_ui.visible.get();
            project_root_sig.set(project.borrow().root.clone());
        });
    }
    let for_build = agent_state.clone();
    let open_from_graph = {
        let buffer_manager = app_state.buffer_manager.clone();
        let lsp_handle = app_state.lsp_handle.clone();
        let active_buffer = signals.active_buffer;
        let buffer_states = signals.buffer_states;
        let editor_version = signals.editor_version;
        move |path: std::path::PathBuf, line: usize| {
            handle_file_open(
                path,
                &buffer_manager,
                active_buffer,
                buffer_states,
                editor_version,
                &lsp_handle,
            );
            pending_jump.set(Some(line as u32));
        }
    };
    let buffer_manager_for_editor = app_state.buffer_manager.clone();
    let for_graph_build = for_build.clone();
    let graph_pane = Container::new(call_graph::call_graph_view(
        cg_ui,
        project_root_sig,
        open_from_graph,
        move || call_graph::build(&for_graph_build),
    ))
    .style(move |s| {
        let s = s.width_full().height_full();
        if cg_ui.visible.get() {
            s
        } else {
            s.display(floem::taffy::Display::None)
        }
    });
    let editor_pane = Stack::vertical((
        smithy_editor::external_change_bar(
            external_change,
            move || {
                if let Some(handle) = open_editor.get_untracked() {
                    if let Err(e) = handle.reload_from_disk() {
                        eprintln!("{e}");
                    }
                }
                external_change.set(None);
            },
            move || external_change.set(None),
        ),
        Container::new(editor_component.view(
            project_map,
            signals.active_buffer,
            buffer_manager_for_editor,
            open_editor,
            editor_sessions.clone(),
            signals.editor_version,
        ))
        .style(|s| s.flex_grow(1.0).width_full().min_height(0.0)),
    ))
    .style(move |s| {
        let s = s.width_full().height_full();
        if cg_ui.visible.get() {
            s.display(floem::taffy::Display::None)
        } else {
            s
        }
    });
    let editor_view = Stack::new((editor_pane, graph_pane))
    .style(|s| s.width_full().height_full().min_height(0.0));

    // Create multi-terminal component
    let terminal_component =
        MultiTerminalComponent::new(app_state.terminal_tabs.clone(), signals.terminal_visible);

    // Create the terminal view
    let terminal_view = terminal_component.view();

    // The agent panel.
    let agent_view = {
        let for_send = agent_state.clone();
        let on_send = std::rc::Rc::new(move |task: String| submit_task(&for_send, task));
        // Signals the running turn to stop at its next checkpoint. Returns
        // immediately — the panel leaves `busy` set until the turn reports back
        // through its stamped terminal event, so the button reflects the turn
        // actually ending rather than the click being registered.
        let lifecycle = agent_state.lifecycle.clone();
        let on_stop = std::rc::Rc::new(move || {
            lifecycle.stop_current_turn();
        });
        let voice = voice_control.clone();
        // Try the endpoint again, for the case the plan did not cover: launching
        // Smithy before the model is being served. The connection is otherwise
        // attempted once at startup and once per project switch, so a model
        // started afterwards was unreachable without restarting the editor.
        let for_reconnect = agent_state.clone();
        let on_reconnect = move || {
            connect_agent(&for_reconnect);
        };
        let settings_dir_for_open = settings_dir.clone();
        let on_settings = move || settings::open(settings_state, &settings_dir_for_open);
        let for_clear = agent_state.clone();
        let on_clear_context = move || app_state::clear_context(&for_clear);
        smithy_editor::agent_panel(
            agent_state.panel,
            on_send,
            on_stop,
            move || signals.agent_visible.set(false),
            move || voice.press(),
            on_reconnect,
            on_settings,
            on_clear_context,
            voice_hotkey.describe(),
        )
    };

    // Create file browser
    let on_file_open = {
        let buffer_manager = app_state.buffer_manager.clone();
        let lsp_handle = app_state.lsp_handle.clone();
        let active_buffer = signals.active_buffer;
        let buffer_states = signals.buffer_states;
        let editor_version = signals.editor_version;

        move |path: std::path::PathBuf| {
            handle_file_open(
                path,
                &buffer_manager,
                active_buffer,
                buffer_states,
                editor_version,
                &lsp_handle,
            );
        }
    };

    let file_browser_state = app_state.file_browser.clone();
    // The Explorer's own hide button, doing what `⌘B` does — the same signal,
    // so the two cannot disagree about whether the panel is showing.
    let sidebar_for_hide = signals.sidebar_visible;
    // Attaching from the Explorer. The panel is revealed if it was hidden —
    // otherwise the chip lands somewhere you cannot see and the click looks
    // like it did nothing, which is the failure this whole affordance exists to
    // remove.
    let on_add_to_context = {
        let panel = agent_state.panel;
        let agent_visible = signals.agent_visible;
        move |path: std::path::PathBuf| {
            panel.attach(&[path]);
            agent_visible.set(true);
        }
    };
    let file_browser = file_browser_view(
        file_browser_state,
        agent_state.file_browser_refresh,
        on_file_open,
        on_add_to_context,
        move || sidebar_for_hide.set(false),
    );

    // Create tab handlers
    let on_tab_click = {
        let buffer_manager = app_state.buffer_manager.clone();
        let active_buffer = signals.active_buffer;
        let editor_version = signals.editor_version;

        move |id| {
            handle_tab_click(id, &buffer_manager, active_buffer, editor_version);
        }
    };

    // Every close gesture first becomes an intent, including clean tabs. Only
    // this commit closure removes documents and emits didClose.
    let commit_requested_close = {
        let buffer_manager = app_state.buffer_manager.clone();
        let active_buffer = signals.active_buffer;
        let buffer_states = signals.buffer_states;
        let editor_version = signals.editor_version;
        let sessions = editor_sessions.clone();
        let lsp_handle = app_state.lsp_handle.clone();
        std::rc::Rc::new(move |intent: CloseIntent| {
            let target = intent.target;
            if let Err(refreshed) = commit_close(
                &intent,
                &buffer_manager,
                active_buffer,
                buffer_states,
                editor_version,
                &sessions,
                &lsp_handle,
            ) {
                close_intent.set(Some(refreshed));
                return;
            }
            if target == CloseTarget::Window {
                // `close_window` bypasses WindowCloseRequested, so confirmation
                // cannot recurse into another copy of this dialog.
                floem::close_window(window_id);
            }
        })
    };

    let request_close = {
        let buffer_manager = app_state.buffer_manager.clone();
        let sessions = editor_sessions.clone();
        let commit = commit_requested_close.clone();
        std::rc::Rc::new(move |target: CloseTarget| {
            let intent = CloseIntent::new(target, &sessions, &buffer_manager.borrow());
            if intent.dirty.is_empty() {
                commit(intent);
            } else {
                close_intent.set(Some(intent));
            }
        })
    };

    let on_tab_close = {
        let request_close = request_close.clone();
        move |id| {
            request_close(CloseTarget::Tab(id));
        }
    };

    // --- Problems panel: LSP diagnostics made visible ---
    let diagnostics = agent_state.diagnostics;
    let problems_visible = floem::reactive::RwSignal::new(false);

    // Report an unusable language server here rather than leaving the panel
    // mysteriously empty. This is the failure that read as "EOF while reading
    // headers" for weeks.
    {
        let availability = smithy_editor::lsp::LspRegistry::new(
            tokio::sync::mpsc::channel::<smithy_editor::ClientDiagnostics>(1).0,
        )
        .check_server("rust");
        if let Some(advice) = availability.advice() {
            diagnostics.server_status.set(Some(advice));
        }
    }

    // Clicking a problem opens the file, through the same path as the explorer.
    let open_at = {
        let buffer_manager = app_state.buffer_manager.clone();
        let lsp_handle = app_state.lsp_handle.clone();
        // Live, for the same reason as the review root above: a problem clicked
        // after a project switch would otherwise resolve against the old root.
        let project = agent_state.project.clone();
        let active_buffer = signals.active_buffer;
        let buffer_states = signals.buffer_states;
        let editor_version = signals.editor_version;

        move |file: String, line: u32, _column: u32| {
            // Diagnostics carry project-relative paths; the opener needs absolute.
            let path = project.borrow().root.join(&file);
            handle_file_open(
                path,
                &buffer_manager,
                active_buffer,
                buffer_states,
                editor_version,
                &lsp_handle,
            );
            // The jump has to be deferred. Opening a file replaces the editor
            // pane, and the new editor does not exist until the reactive
            // rebuild runs — which is after this closure returns. Record the
            // target; the effect below applies it once the handle appears.
            pending_jump.set(Some(line));
        }
    };

    // Apply a deferred caret jump once the newly-opened editor exists.
    floem::reactive::Effect::new(move |_| {
        let Some(handle) = open_editor.get() else {
            return;
        };
        let Some(line) = pending_jump.get() else {
            return;
        };
        handle.goto_line(line);
        pending_jump.set(None);
    });

    // One effect, draining the inbox and dispatching every message.
    //
    // Two effects used to match on a shared `RwSignal<Option<LspResponse>>`.
    // That lost messages: floem's channel bridge sets the signal once per
    // message inside a single effect run, so only the last of a batch was ever
    // observed — see `app_state`'s `lsp_inbox`. rust-analyzer publishes one
    // notification per file, so most of a batch went nowhere.
    {
        let hover_state = hover_state.clone();
        let open_at_definition = open_at.clone();
        let project_for_diags = agent_state.project.clone();
        let inbox = signals.lsp_inbox.clone();
        let lsp_tick = signals.lsp_tick;
        let accepted_lsp_stamp =
            std::rc::Rc::new(std::cell::RefCell::new(None::<smithy_editor::LspStamp>));

        floem::reactive::Effect::new(move |_| {
            lsp_tick.get();

            // Take everything at once and release the lock before doing any UI
            // work: the LSP reader thread is pushing into this and must not wait
            // on a repaint.
            let pending: Vec<smithy_editor::LspResponse> = match inbox.lock() {
                Ok(mut queue) => queue.drain(..).collect(),
                Err(_) => return,
            };

            for response in pending {
                if let Some(stamp) = response.stamp() {
                    let project_root = project_for_diags.borrow().root.clone();
                    let authoritative = matches!(
                        &response,
                        smithy_editor::LspResponse::Ready { .. }
                            | smithy_editor::LspResponse::ServerStatus { .. }
                            | smithy_editor::LspResponse::Error {
                                request_id: None,
                                ..
                            }
                    );
                    if !accept_lsp_stamp(
                        &project_root,
                        &mut accepted_lsp_stamp.borrow_mut(),
                        stamp,
                        authoritative,
                    ) {
                        continue;
                    }
                }
                match response {
                    smithy_editor::LspResponse::Diagnostics {
                        path,
                        diagnostics: incoming,
                        ..
                    } => {
                        let project_root = project_for_diags.borrow().root.clone();
                        // Display paths relative to the project, so rows are readable.
                        let file = smithy_editor::ProblemRow::label_for(&path, &project_root);
                        let rows = incoming
                            .iter()
                            .map(|d| smithy_editor::ProblemRow::from_diagnostic(&file, d))
                            .collect();
                        diagnostics.publish(file, rows);

                        // Same diagnostics, second destination: the open editor's
                        // inline styling. Only for the file actually on screen —
                        // pushing another file's ranges would mark arbitrary text.
                        if let Some(handle) = open_editor.get_untracked() {
                            if smithy_editor::is_same_file(&handle.path, &path) {
                                handle.set_diagnostics(
                                    incoming
                                        .iter()
                                        .map(|d| smithy_editor::InlineDiagnostic {
                                            line: d.range.start.line as usize,
                                            start_column: d.range.start.column as usize,
                                            end_column: d.range.end.column as usize,
                                            severity: d.severity,
                                        })
                                        .collect(),
                                );
                            }
                        }
                    }
                    smithy_editor::LspResponse::Hover { request_id, result } => {
                        hover_state.show(request_id, result.map(|h| h.contents));
                    }
                    smithy_editor::LspResponse::GotoDefinition {
                        location: Some((path, line, column)),
                        ..
                    } => {
                        open_at_definition(path.display().to_string(), line + 1, column + 1);
                    }
                    // A dead language server used to be invisible unless you had
                    // launched from a terminal and were reading stderr. The
                    // Problems panel already has somewhere to say so.
                    smithy_editor::LspResponse::Error {
                        request_id: Some(request_id),
                        ..
                    } => {
                        // A request-scoped error is not evidence that the
                        // language server died. In particular, one failed hover
                        // must not erase the global ready state.
                        hover_state.fail(request_id);
                    }
                    smithy_editor::LspResponse::Error {
                        request_id: None,
                        message,
                        ..
                    } => {
                        diagnostics.server_status.set(Some(message));
                        diagnostics.server_count.set(0);
                    }
                    // So an empty panel can say which kind of empty it is.
                    smithy_editor::LspResponse::Ready { servers, .. } => {
                        diagnostics.server_count.set(servers);
                        diagnostics.server_status.set(None);
                    }
                    smithy_editor::LspResponse::ServerStatus { running, .. } => {
                        diagnostics
                            .server_count
                            .set(server_count_for_lsp_status(running));
                        if running {
                            diagnostics.server_status.set(None);
                        }
                    }
                    _ => {}
                }
            }
        });
    }

    let make_problems_view = move || {
        let on_open = std::rc::Rc::new(open_at.clone());
        smithy_editor::problems_panel(diagnostics, on_open, move || problems_visible.set(false))
    };

    let aesthetic = signals.aesthetic;
    // Where the choice is remembered. Taken once here because the menu closure
    // outlives the borrow.
    let aesthetic_data_dir = agent_state.registry.data_dir().to_path_buf();

    // --- Menu bar ---
    let menu_state = smithy_editor::MenuBarState::new();
    // Whether a language server is meant to be up. Drives the memory meter's
    // wording, so "no analyzer running" reads as a choice you made rather than
    // as a measurement that failed.
    let lsp_running = floem::reactive::RwSignal::new(true);
    let menus = vec![
        smithy_editor::Menu::new("File", {
            let mut items = vec![
                smithy_editor::MenuItem::action_with(
                    "Open Project…",
                    smithy_editor::accel("O"),
                    {
                        let agent_state = agent_state.clone();
                        move || open_project_dialog(&agent_state, hover_epoch)
                    },
                ),
                smithy_editor::MenuItem::action_with(
                    "Save",
                    smithy_editor::accel("S"),
                    move || {
                        if let Some(handle) = open_editor.get_untracked() {
                            if save_editor(&handle, external_change) {
                                editor_version.update(|version| *version += 1);
                            }
                        }
                    },
                ),
            ];
            let recents = agent_state.registry.recents();
            if !recents.is_empty() {
                items.push(smithy_editor::MenuItem::Separator);
                // Skip the first: it is the project already open.
                for recent in recents.iter().skip(1).take(8) {
                    let agent_state = agent_state.clone();
                    let root = recent.root.clone();
                    items.push(smithy_editor::MenuItem::action(
                        recent.name.clone(),
                        move || switch_project(&agent_state, root.clone(), hover_epoch),
                    ));
                }
            }
            items
        }),
        smithy_editor::Menu::new("Edit", {
            // Every item dispatches the same command floem's own keymap
            // binds, so menu and keyboard cannot drift apart. Each is a
            // no-op when no file is open, rather than being hidden — a menu
            // whose contents move around is harder to learn.
            let edit_action = |f: fn(&smithy_editor::EditorHandle)| {
                move || {
                    if let Some(handle) = open_editor.get_untracked() {
                        f(&handle);
                    }
                }
            };
            vec![
                smithy_editor::MenuItem::action_with(
                    "Undo",
                    smithy_editor::accel("Z"),
                    edit_action(|h| h.undo()),
                ),
                smithy_editor::MenuItem::action_with(
                    "Redo",
                    smithy_editor::accel("⇧Z"),
                    edit_action(|h| h.redo()),
                ),
                smithy_editor::MenuItem::Separator,
                smithy_editor::MenuItem::action_with(
                    "Cut",
                    smithy_editor::accel("X"),
                    edit_action(|h| h.cut()),
                ),
                smithy_editor::MenuItem::action_with(
                    "Copy",
                    smithy_editor::accel("C"),
                    edit_action(|h| h.copy()),
                ),
                smithy_editor::MenuItem::action_with(
                    "Paste",
                    smithy_editor::accel("V"),
                    edit_action(|h| h.paste()),
                ),
                smithy_editor::MenuItem::action_with(
                    "Select All",
                    smithy_editor::accel("A"),
                    edit_action(|h| h.select_all()),
                ),
            ]
        }),
        smithy_editor::Menu::new(
            "Code",
            vec![
                smithy_editor::MenuItem::action_with("Hover", "⌃K", {
                    let ask = ask_hover_menu.clone();
                    move || ask()
                }),
                smithy_editor::MenuItem::action_with("Go to Definition", "F12", {
                    let ask = ask_definition_menu.clone();
                    move || ask()
                }),
                smithy_editor::MenuItem::Separator,
                // rust-analyzer indexes your *dependencies*, not just your code,
                // so its footprint tracks the size of the crate graph rather
                // than the size of the project: 109 crates measured at 724 MB,
                // 834 crates at 5.1 GB. That is normal and it is also a lot to
                // hold while editing a file it is not helping with. Stopping is
                // recoverable — `StopServers` leaves the worker alive, unlike
                // the `Shutdown` used at exit.
                smithy_editor::MenuItem::action("Stop Language Server (frees memory)", {
                    let lsp = app_state.lsp_handle.clone();
                    let running = lsp_running;
                    move || {
                        lsp.stop_servers();
                        running.set(false);
                    }
                }),
                smithy_editor::MenuItem::action("Start Language Server", {
                    let lsp = app_state.lsp_handle.clone();
                    let project = agent_state.project.clone();
                    let running = lsp_running;
                    move || {
                        lsp.initialize(project.borrow().root.clone());
                        running.set(true);
                    }
                }),
            ],
        ),
        // The agent's own menu. Both items also exist in the panel header, which
        // is where you reach for them; they are here because the header is
        // hidden whenever the panel is, and "how do I change the model" should
        // be answerable from the menu bar without knowing that first.
        smithy_editor::Menu::new(
            "Agent",
            vec![
                smithy_editor::MenuItem::action("Backend Settings…", {
                    let dir = settings_dir.clone();
                    move || settings::open(settings_state, &dir)
                }),
                smithy_editor::MenuItem::action("Build Call Graph", {
                    let agent_state = agent_state.clone();
                    move || call_graph::build(&agent_state)
                }),
                smithy_editor::MenuItem::action("Show Call Graph", {
                    let ui = agent_state.call_graph;
                    move || ui.visible.set(true)
                }),
                smithy_editor::MenuItem::Separator,
                smithy_editor::MenuItem::action("New Session", {
                    let agent_state = agent_state.clone();
                    move || app_state::clear_context(&agent_state)
                }),
                smithy_editor::MenuItem::action("Reconnect", {
                    let agent_state = agent_state.clone();
                    move || connect_agent(&agent_state)
                }),
            ],
        ),
        smithy_editor::Menu::new(
            "View",
            vec![
                smithy_editor::MenuItem::toggle_with(
                    "Explorer",
                    smithy_editor::accel("B"),
                    signals.sidebar_visible,
                ),
                smithy_editor::MenuItem::toggle_with(
                    "Agent",
                    smithy_editor::accel("L"),
                    signals.agent_visible,
                ),
                smithy_editor::MenuItem::toggle_with("Terminal", "⌃`", signals.terminal_visible),
                smithy_editor::MenuItem::Separator,
                smithy_editor::MenuItem::toggle("Problems", problems_visible),
                smithy_editor::MenuItem::toggle("Call Graph", agent_state.call_graph.visible),
                // No shortcut, deliberately: a clock is a preference somebody
                // sets once, not something worth a key.
                smithy_editor::MenuItem::toggle("Clock", clock_visible),
                smithy_editor::MenuItem::Separator,
                smithy_editor::MenuItem::action("Switch Look", {
                    let data_dir = aesthetic_data_dir.clone();
                    move || {
                        let next = aesthetic.get_untracked().toggled();
                        aesthetic.set(next);
                        if let Err(e) = next.save(&data_dir) {
                            eprintln!("could not remember the look: {e}");
                        }
                    }
                }),
            ],
        ),
    ];
    // The clock ticks once a second — a label cannot notice that time has
    // passed, so something has to tell it. Seconds are shown, so a minute tick
    // would leave it wrong for fifty-nine of every sixty.
    let clock_tick = smithy_editor::tick::every(std::time::Duration::from_secs(1));
    // The top-right meters. Two independent cadences: memory is a local read
    // every few seconds, the account balance is a network call every few
    // minutes. Sharing one tick would either hammer the provider or make the
    // memory figure useless.
    let status = smithy_editor::StatusReadout::new();
    {
        let meter_tick = smithy_editor::tick::every(meters::MEMORY_INTERVAL);
        let balance_cache = agent_state.balance.clone();
        let panel = agent_state.panel;
        let lifecycle = agent_state.lifecycle.clone();
        let settings_dir = settings_dir.clone();
        let usage_cache = agent_state.usage_cache.clone();

        floem::reactive::Effect::new(move |_| {
            meter_tick.get();

            let sample = meters::sample_memory();
            // When the server has been stopped deliberately, say so rather than
            // silently dropping the figure — an absent number reads as a broken
            // meter, not as reclaimed memory.
            status.memory.set(if lsp_running.get() {
                sample.render()
            } else {
                format!("{} · analyzer stopped", sample.render())
            });
            status.memory_warn.set(sample.is_heavy());

            // Cost is derived from what the *panel* already knows about the
            // model plus what the session has been billed, so it costs nothing
            // to recompute on every tick.
            let spend = meters::spend_now(
                &settings_dir,
                &panel.model_label.get_untracked(),
                lifecycle.current_session().as_ref(),
                &balance_cache,
                &usage_cache,
            );
            status.spend.set(spend.render());
            status.spend_warn.set(spend.is_low());
        });
    }
    meters::spawn_balance_poller(agent_state.clone(), settings_dir.clone());

    let menu_view =
        smithy_editor::menu_bar(menu_state, menus.clone(), clock_visible, clock_tick, status);
    let menu_overlay = smithy_editor::menu_overlay(menu_state, menus);

    // Use standardized main layout
    let agent_visible_signal = signals.agent_visible;
    let terminal_visible_signal = signals.terminal_visible;
    let sidebar_visible_signal = signals.sidebar_visible;
    // ⌘O opens a native modal, which cannot be done from inside the keyboard
    // event handler (see `file_dialog::pick_folder_async`). The dialog runs on
    // the tokio runtime and the chosen directory comes back through a channel,
    // so the project switch happens in a reactive effect rather than mid-event.
    let (project_pick_tx, project_pick_rx) = crossbeam_channel::unbounded::<std::path::PathBuf>();
    let project_pick_signal = floem::reactive::RwSignal::new(None::<std::path::PathBuf>);
    // The one bridge that is right to leave carrying a payload. Coalescing to the
    // last value is the *correct* semantics here: open the dialog three times and
    // you want the project you finally chose, not three switches in a row. Every
    // other bridge went through `app_state::bridge` for exactly the opposite
    // reason.
    floem::ext_event::update_signal_from_channel(project_pick_signal.write_only(), project_pick_rx);
    {
        let agent_for_pick = agent_state.clone();
        floem::reactive::Effect::new(move |_| {
            if let Some(dir) = project_pick_signal.get() {
                switch_project(&agent_for_pick, dir, hover_epoch);
            }
        });
    }

    // One tick a minute drives the sky. The sky moves a quarter degree in that
    // time, which is already finer than the backdrop can show.
    let sky_tick = smithy_editor::celestial::minute_tick();

    // The sky over San Francisco right now, then circuitry lit by what the
    // language server reports, then the code. All layered under the editor
    // rather than replacing it, so nothing about the editor changes when the
    // look does.
    //
    // The sky goes *under* the circuitry rather than instead of it. That was
    // the open question — replace or layer — and layering is the conservative
    // answer: it keeps both and can be undone, where replacing throws the
    // circuitry away to find out. The mosaic is sparse translucent tiles rather
    // than a solid ground, so the star field reads through it. If it turns out
    // to read as noise over the stars, dropping `draw_mosaic` is one line.
    let editor_view = Stack::new((
        smithy_editor::celestial::sky_backdrop(
            aesthetic,
            sky_tick,
            smithy_editor::celestial::DEFAULT_LOCATION,
        ),
        smithy_editor::circuit_backdrop(aesthetic, diagnostics.by_file),
        editor_view,
    ))
    .style(|s| s.width_full().height_full());

    let main_content = main_layout_view(
        app_state.layout_theme,
        signals.buffer_states,
        signals.active_buffer,
        signals.sidebar_visible,
        signals.terminal_visible,
        signals.agent_visible,
        file_browser,
        editor_view,
        terminal_view,
        agent_view,
        on_tab_click,
        on_tab_close,
    )
    // CAPTURE, not the default TARGET|BUBBLE. Application shortcuts have to be
    // seen before the focused view gets the key: the terminal panel and the
    // text editor both consume KeyDown, so on bubble this handler never ran
    // while either had focus — ⌘S did nothing precisely when you had a file
    // open and wanted to save it. Capture runs root-first, so the shortcut wins
    // and everything it does not claim still reaches the focused view.
    .on_event_with_config(
        floem::event::listener::KeyDown,
        floem::context::EventCallbackConfig {
            phases: floem::context::Phases::CAPTURE | floem::context::Phases::TARGET,
        },
        move |_, key_event| {
            {
                key_debug("root", key_event);
                // Claimed shortcuts stop here; everything else must keep going or
                // capture-phase interception would swallow all typing.
                let mut handled = false;
                // ⌘ on macOS, Ctrl elsewhere — but accept either everywhere. The
                // on-screen hints render the modifier glyph as a missing-glyph box
                // on this system, so which one is meant is genuinely unguessable;
                // refusing the one the user reached for is the worse failure.
                let cmd = key_event
                    .modifiers
                    .contains(floem::prelude::Modifiers::META)
                    || key_event
                        .modifiers
                        .contains(floem::prelude::Modifiers::CONTROL);
                // Dictation. Configurable, and read once at startup rather than
                // per keystroke — the file is a preference, not a live input.
                if voice_hotkey.matches(key_event) {
                    handled = true;
                    voice_control.press();
                }
                // Save. Cmd on macOS, Ctrl elsewhere — accept either rather than
                // making the shortcut platform-dependent in a cross-platform app.
                if key_event.key == floem::prelude::Key::Character("s".into()) && cmd {
                    handled = true;
                    if let Some(handle) = open_editor.get_untracked() {
                        if save_editor(&handle, external_change) {
                            editor_version.update(|v| *v += 1);
                        }
                    }
                }
                // Open a project. The menu has advertised this shortcut since the
                // first commit without anything ever being bound to it.
                if key_event.key == floem::prelude::Key::Character("o".into()) && cmd {
                    handled = true;
                    // Not called inline: the blocking dialog re-enters AppKit
                    // while it is still dispatching this key, and winit aborts.
                    // The async dialog yields instead, so this handler returns
                    // first and the chosen directory arrives over the channel.
                    let tx = project_pick_tx.clone();
                    crate::runtime::tokio_runtime().spawn(async move {
                        if let Some(dir) = smithy_editor::file_dialog::pick_folder_async().await {
                            let _ = tx.send(dir);
                        }
                    });
                }
                // Hover at the caret.
                if key_event.key == floem::prelude::Key::Character("k".into())
                    && key_event
                        .modifiers
                        .contains(floem::prelude::Modifiers::CONTROL)
                {
                    ask_hover();
                    handled = true;
                }
                // Go to definition.
                if key_event.key == floem::prelude::Key::Named(floem::prelude::NamedKey::F12) {
                    ask_definition();
                    handled = true;
                }
                // Escape dismisses the hover popup.
                if key_event.key == floem::prelude::Key::Named(floem::prelude::NamedKey::Escape) {
                    hover_state_for_keys.dismiss();
                    handled = true;
                }
                // Toggle the explorer. The View menu has advertised this since
                // the first commit with nothing bound to it — the same defect
                // Open Project had, found by listing every bound character and
                // diffing it against the shortcuts the menus claim.
                if key_event.key == floem::prelude::Key::Character("b".into())
                    && cmd
                    && !key_event
                        .modifiers
                        .contains(floem::prelude::Modifiers::SHIFT)
                {
                    sidebar_visible_signal.update(|v| *v = !*v);
                    handled = true;
                }
                // Check for Ctrl+L to toggle chat panel
                if key_event.key == floem::prelude::Key::Character("l".into())
                    && cmd
                    && !key_event
                        .modifiers
                        .contains(floem::prelude::Modifiers::SHIFT)
                {
                    agent_visible_signal.update(|v| *v = !*v);
                    handled = true;
                }
                // Check for Ctrl+` or Ctrl+' to toggle terminal
                if (key_event.key == floem::prelude::Key::Character("`".into())
                    || key_event.key == floem::prelude::Key::Character("'".into()))
                    && cmd
                    && !key_event
                        .modifiers
                        .contains(floem::prelude::Modifiers::SHIFT)
                {
                    terminal_visible_signal.update(|v| *v = !*v);
                    handled = true;
                }
                if handled {
                    floem::event::EventPropagation::Stop
                } else {
                    floem::event::EventPropagation::Continue
                }
            }
        },
    )
    .style(|s| s.keyboard_navigable())
    .style(|s| {
        s.width_full()
            .height_full()
            .background(catppuccin::BASE)
            .color(catppuccin::TEXT)
    });

    // Diff review modal: raised whenever the write-review hook queues a change
    // instead of letting it land on disk.
    let current_diff = agent_state.review.current;
    let pending_changes = agent_state.review.pending.clone();

    // Resolve the reviewed change and surface the next queued one, if any.
    // Resolve *this* review and surface the next queued one, if any.
    //
    // Takes the id, not the path: one turn can queue two writes to the same
    // file, and resolving by path removes whichever was queued first.
    let advance_queue = {
        let pending_changes = pending_changes.clone();
        move |id: String| current_diff.set(agent::discard_change(&pending_changes, &id))
    };

    // Both the panel and the model learn how a review resolved. The panel entry
    // is immediate feedback for the user; the queued note reaches the model at
    // the head of its next turn, because by now its tool result is frozen in
    // history. See `agent::describe_review_outcome`.
    // Deliver a review decision to whoever is waiting for it.
    //
    // The tool call is suspended inside `WriteReviewHook`, so the answer goes
    // back as *its* result — that is the whole point of the blocking gate, and
    // why this takes the lifecycle-qualified registration id. `outcomes` is now
    // only the fallback for
    // a decision nobody is waiting on (a review abandoned by a project switch,
    // or one whose turn has already ended), where the next turn's preamble is
    // still the only way to say what happened.
    let record_outcome = {
        let panel = agent_state.panel;
        let outcomes = agent_state.review.outcomes.clone();
        let review = agent_state.review.clone();
        move |id: &str,
              path: &str,
              accepted: usize,
              total: usize,
              exact_message: Option<String>| {
            let note = exact_message
                .unwrap_or_else(|| agent::describe_review_outcome(path, accepted, total));
            panel.push(smithy_editor::AgentEntry::Notice(note.clone()));

            let delivered = review.respond(
                id,
                app_state::ReviewOutcome {
                    message: note.clone(),
                    applied: accepted > 0,
                },
            );
            if !delivered {
                outcomes.borrow_mut().push(note);
            }
        }
    };

    let on_diff_accept = {
        let lifecycle = agent_state.lifecycle.clone();
        let sessions = editor_sessions.clone();
        let advance_queue = advance_queue.clone();
        let record_outcome = record_outcome.clone();
        move |_diff: FileDiff, statuses: Vec<smithy_editor::ChangeStatus>| {
            let Some(change) = current_diff.get_untracked() else {
                return;
            };
            let id = change.id.clone();
            let total = change.diff.hunks.len();
            let accepted = statuses
                .iter()
                .filter(|s| **s == smithy_editor::ChangeStatus::Accepted)
                .count();
            let content =
                smithy_editor::content_with_accepted_hunks(&change.diff, &statuses);

            let result = if accepted == 0 {
                Ok(())
            } else if lifecycle.accepts_review(&change.key) {
                let lifecycle_for_publication = lifecycle.clone();
                let key = change.key.clone();
                agent::apply_change_authorized(&change, &content, &sessions, move || {
                    if lifecycle_for_publication.accepts_review(&key) {
                        Ok(())
                    } else {
                        Err(
                            "the review expired immediately before publication because its agent \
                             generation or turn is no longer current"
                                .into(),
                        )
                    }
                })
            } else {
                Err(agent::ReviewApplyFailure::before(format!(
                    "the review of `{}` expired because its agent generation or turn is no longer \
                     current. Nothing was written; re-read the file and reissue the change for \
                     review.",
                    change.path()
                )))
            };
            match result {
                Ok(()) => {
                    let exact =
                        (accepted > 0 && accepted == total).then(|| change.success_message.clone());
                    record_outcome(&id, change.path(), accepted, total, exact)
                }
                Err(e) => {
                    eprintln!("could not apply the accepted change to {}: {e}", change.path());
                    let applied = if e.published { accepted } else { 0 };
                    record_outcome(
                        &id,
                        change.path(),
                        applied,
                        total,
                        Some(e.to_string()),
                    );
                }
            }
            advance_queue(id);
        }
    };

    let on_diff_reject = {
        let advance_queue = advance_queue.clone();
        let record_outcome = record_outcome.clone();
        move || {
            if let Some(change) = current_diff.get_untracked() {
                record_outcome(
                    &change.id,
                    change.path(),
                    0,
                    change.diff.hunks.len(),
                    None,
                );
                advance_queue(change.id);
            }
        }
    };

    // Dismissing the modal is a rejection, not a deferral: the change is dropped
    // from the queue either way, so reporting anything else would leave the
    // model believing a write might still land.
    let on_diff_close = {
        move || {
            if let Some(change) = current_diff.get_untracked() {
                record_outcome(
                    &change.id,
                    change.path(),
                    0,
                    change.diff.hunks.len(),
                    None,
                );
                advance_queue(change.id);
            }
        }
    };

    let keep_editing_after_close = {
        let buffer_manager = app_state.buffer_manager.clone();
        let sessions = editor_sessions.clone();
        move || {
            if let Some(intent) = close_intent.get_untracked() {
                let _ = resolve_close(
                    &intent,
                    CloseChoice::KeepEditing,
                    &sessions,
                    &buffer_manager.borrow(),
                );
            }
            close_intent.set(None);
        }
    };
    let save_and_close = {
        let buffer_manager = app_state.buffer_manager.clone();
        let sessions = editor_sessions.clone();
        let commit = commit_requested_close.clone();
        move || {
            let Some(intent) = close_intent.get_untracked() else {
                return;
            };
            match resolve_close(
                &intent,
                CloseChoice::SaveAndClose,
                &sessions,
                &buffer_manager.borrow(),
            ) {
                CloseResolution::Commit(current) => {
                    close_intent.set(None);
                    commit(current);
                }
                CloseResolution::KeepOpen => close_intent.set(None),
                CloseResolution::Refresh(current) => close_intent.set(Some(current)),
            }
        }
    };
    let discard_and_close = {
        let buffer_manager = app_state.buffer_manager.clone();
        let sessions = editor_sessions.clone();
        let commit = commit_requested_close.clone();
        move || {
            let Some(intent) = close_intent.get_untracked() else {
                return;
            };
            match resolve_close(
                &intent,
                CloseChoice::DiscardAndClose,
                &sessions,
                &buffer_manager.borrow(),
            ) {
                CloseResolution::Commit(current) => {
                    close_intent.set(None);
                    commit(current);
                }
                CloseResolution::Refresh(current) => close_intent.set(Some(current)),
                CloseResolution::KeepOpen => close_intent.set(None),
            }
        }
    };
    let reload_close_conflict = {
        let buffer_manager = app_state.buffer_manager.clone();
        let sessions = editor_sessions.clone();
        move || {
            let Some(mut pending) = close_intent.get_untracked() else {
                return;
            };
            let Some(id) = pending.conflict else {
                return;
            };
            let result = sessions
                .get(id)
                .ok_or_else(|| "the conflicted editor is no longer open".to_string())
                .and_then(|handle| handle.reload_from_disk());
            match result {
                Ok(()) => {
                    let refreshed =
                        CloseIntent::new(pending.target, &sessions, &buffer_manager.borrow());
                    close_intent.set((!refreshed.dirty.is_empty()).then_some(refreshed));
                }
                Err(error) => {
                    pending.error = Some(error);
                    close_intent.set(Some(pending));
                }
            }
        }
    };

    // Menu bar on top, then the main layout.
    let shell = Stack::vertical((
        menu_view,
        Container::new(main_content).style(|s| s.flex_grow(1.0).width_full().min_height(0.0)),
        dyn_container(
            move || problems_visible.get(),
            move |show| {
                if show {
                    Box::new(
                        Container::new(make_problems_view())
                            .style(|s| s.width_full().height(260.0)),
                    ) as Box<dyn View>
                } else {
                    Box::new(Empty::new().style(|s| s.display(floem::taffy::Display::None)))
                        as Box<dyn View>
                }
            },
        ),
    ))
    .style(move |s| {
        // Inset when forged so the frame has something to surround. The shell
        // itself is untouched by the switch — no panel is rebuilt, so buffers,
        // scrollback and the live agent session all survive it.
        let a = aesthetic.get();
        let inset = smithy_editor::shell_inset(a);
        s.width_full()
            .height_full()
            .padding_horiz(inset)
            .padding_bottom(inset)
            // Deeper at the top: that is where the wordmark plate sits.
            .padding_top(smithy_editor::shell_top_inset(a))
            // Transparent when forged, so the frame painted beneath shows
            // through. `padding` insets the *content* but the background still
            // fills the whole box, so an opaque shell covers exactly the ring
            // the frame occupies — which is why it was drawn and never seen.
            .background(match a {
                smithy_editor::Aesthetic::Flat => smithy_editor::design::BG_BASE,
                smithy_editor::Aesthetic::Forged => floem::peniko::Color::TRANSPARENT,
            })
    });

    Stack::new((
        // Behind everything, and paints nothing at all when flat. The sky's
        // minute clock is shared: the sun riding the top rail keeps time with
        // the sky behind the editor.
        smithy_editor::forged_frame(aesthetic, sky_tick),
        // The mascot, on the frame's bottom rail. Over the frame rather than
        // inside it, because he animates several times a second and the frame
        // repaints only when the look changes.
        smithy_editor::fisherman::fisherman_view(aesthetic, smithy_editor::tick::animation()),
        shell,
        smithy_editor::hover_popup(hover_state.clone()),
        // Above the shell so dropdowns paint over the editor, below the modals
        // so a modal still takes precedence.
        menu_overlay,
        close_confirmation_modal(
            close_intent,
            keep_editing_after_close,
            save_and_close,
            discard_and_close,
            reload_close_conflict,
        ),
        diff_modal(current_diff, on_diff_accept, on_diff_reject, on_diff_close),
        shell_approval_modal(
            agent_state.shell_approval,
            agent_state.shell_inbox.clone(),
            agent_state.lifecycle.clone(),
        ),
        smithy_editor::settings_modal(
            settings_state,
            {
                // Saving reconnects, because a backend you selected and did not
                // connect to is not a setting that has taken effect.
                //
                // The reconnect path compares the complete persistence binding.
                // A different provider/model/account/schema either resumes the
                // newest exact match (when switching back) or starts fresh with
                // a Notice naming the mismatch. Bypassing that path here made a
                // model switch look like silent data loss.
                let agent_state = agent_state.clone();
                let dir = settings_dir.clone();
                move || {
                    match settings::save(settings_state, &dir) {
                        Ok(warnings) => {
                            settings_state.close();
                            for warning in &warnings {
                                eprintln!("[settings] {warning}");
                            }
                            connect_agent(&agent_state);
                        }
                        Err(e) => settings_state.report(e, true),
                    }
                }
            },
            move |account: &str| settings::clear_key(settings_state, account),
            {
                let dir = settings_dir.clone();
                move || settings::refresh_models(settings_state, &dir)
            },
            {
                let dir = settings_dir.clone();
                move |model: &str| settings::load_model(settings_state, &dir, model)
            },
        ),
    ))
    .style(|s| s.width_full().height_full())
    .on_event_with_config(
        floem::event::listener::KeyDown,
        floem::context::EventCallbackConfig {
            phases: floem::context::Phases::CAPTURE,
        },
        move |_, event| {
            if close_intent.get_untracked().is_none() {
                return floem::event::EventPropagation::Continue;
            }
            if event.key == floem::prelude::Key::Named(floem::prelude::NamedKey::Escape) {
                close_intent.set(None);
            }
            // The close overlay is modal. This root capture also covers the one
            // frame before Floem can transfer focus from the editor to dialog.
            floem::event::EventPropagation::Stop
        },
    )
    .on_event_cont(
        floem::event::listener::WindowCloseRequested,
        move |cx, _| {
            // Always intercept the native request. Clean windows commit and
            // close immediately; dirty windows leave an explicit intent on
            // screen. The confirmed path uses unconditional `close_window`.
            cx.prevent_default();
            request_close(CloseTarget::Window);
        },
    )
}

/// Save through the editor's immutable disk base and surface stale-base
/// conflicts in the existing reload/manual-resolution bar.
fn save_editor(
    handle: &smithy_editor::EditorHandle,
    external_change: RwSignal<Option<smithy_editor::ExternalChange>>,
) -> bool {
    match handle.save() {
        Ok(()) => {
            external_change.set(None);
            true
        }
        Err(error) if error.is_conflict() => {
            external_change.set(Some(smithy_editor::ExternalChange {
                label: handle.path.display().to_string(),
            }));
            false
        }
        Err(error) => {
            eprintln!("{error}");
            false
        }
    }
}

/// Unsaved-work confirmation shared by tab and native window close.
fn close_confirmation_modal(
    intent: RwSignal<Option<CloseIntent>>,
    on_keep: impl Fn() + 'static,
    on_save: impl Fn() + 'static,
    on_discard: impl Fn() + 'static,
    on_reload: impl Fn() + 'static,
) -> impl IntoView {
    let on_keep = std::rc::Rc::new(on_keep);
    let on_save = std::rc::Rc::new(on_save);
    let on_discard = std::rc::Rc::new(on_discard);
    let on_reload = std::rc::Rc::new(on_reload);

    Container::new(dyn_container(
        move || intent.get(),
        move |pending| {
            let Some(pending) = pending else {
                return Box::new(Empty::new().style(|s| s.display(floem::taffy::Display::None)))
                    as Box<dyn View>;
            };
            let count = pending.dirty.len();
            let title = match pending.target {
                CloseTarget::Tab(_) => "Close this tab?".to_string(),
                CloseTarget::Window => {
                    format!("Close the window with {count} unsaved file{}?", if count == 1 { "" } else { "s" })
                }
            };
            let error = pending.error.clone();
            let keep = on_keep.clone();
            let keep_for_key = on_keep.clone();
            let save = on_save.clone();
            let discard = on_discard.clone();
            let reload = on_reload.clone();
            let conflict = pending.conflict;

            let dialog = Stack::vertical((
                    Label::new(title).style(|s| {
                        s.color(catppuccin::TEXT)
                            .font_size(smithy_editor::design::TEXT_LG)
                            .font_bold()
                            .margin_bottom(smithy_editor::design::SPACE_3)
                    }),
                    Label::new("Unsaved edits will be lost only if you explicitly discard them.")
                        .style(|s| {
                            s.color(catppuccin::SUBTEXT0)
                                .font_size(smithy_editor::design::TEXT_SM)
                                .margin_bottom(smithy_editor::design::SPACE_4)
                        }),
                    dyn_container(
                        move || error.clone(),
                        move |message| {
                            if let Some(message) = message {
                                Box::new(Label::new(message).style(|s| {
                                    s.color(catppuccin::RED)
                                        .font_size(smithy_editor::design::TEXT_SM)
                                        .margin_bottom(smithy_editor::design::SPACE_4)
                                })) as Box<dyn View>
                            } else {
                                Box::new(
                                    Empty::new()
                                        .style(|s| s.display(floem::taffy::Display::None)),
                                ) as Box<dyn View>
                            }
                        },
                    ),
                    dyn_container(
                        move || conflict,
                        move |conflict| {
                            if conflict.is_some() {
                                let reload = reload.clone();
                                Box::new(
                                    Button::new("Reload conflicted file from disk")
                                        .on_event_stop(
                                            floem::event::listener::Click,
                                            move |_, _| reload(),
                                        )
                                        .style(|s| {
                                            s.background(catppuccin::YELLOW)
                                                .color(catppuccin::BASE)
                                                .padding_horiz(16.0)
                                                .padding_vert(8.0)
                                                .border_radius(
                                                    smithy_editor::design::RADIUS_SM,
                                                )
                                                .margin_bottom(
                                                    smithy_editor::design::SPACE_4,
                                                )
                                        }),
                                ) as Box<dyn View>
                            } else {
                                Box::new(
                                    Empty::new()
                                        .style(|s| s.display(floem::taffy::Display::None)),
                                ) as Box<dyn View>
                            }
                        },
                    ),
                    Stack::horizontal((
                        Button::new("Keep editing")
                            .on_event_stop(floem::event::listener::Click, move |_, _| keep())
                            .style(|s| {
                                s.background(catppuccin::SURFACE0)
                                    .color(catppuccin::TEXT)
                                    .padding_horiz(16.0)
                                    .padding_vert(8.0)
                                    .border_radius(smithy_editor::design::RADIUS_SM)
                            }),
                        Container::new(Empty::new()).style(|s| s.flex_grow(1.0)),
                        Button::new("Discard and close")
                            .on_event_stop(floem::event::listener::Click, move |_, _| discard())
                            .style(|s| {
                                s.background(catppuccin::SURFACE0)
                                    .color(catppuccin::RED)
                                    .padding_horiz(16.0)
                                    .padding_vert(8.0)
                                    .border_radius(smithy_editor::design::RADIUS_SM)
                            }),
                        Button::new("Save and close")
                            .on_event_stop(floem::event::listener::Click, move |_, _| save())
                            .style(|s| {
                                s.background(catppuccin::GREEN)
                                    .color(catppuccin::BASE)
                                    .padding_horiz(16.0)
                                    .padding_vert(8.0)
                                    .border_radius(smithy_editor::design::RADIUS_SM)
                                    .margin_left(8.0)
                            }),
                    ))
                    .style(|s| s.width_full().items_center()),
                ))
                .style(|s| {
                    s.width(620.0)
                        .padding(24.0)
                        .background(catppuccin::BASE)
                        .border(1.0)
                        .border_color(catppuccin::SURFACE1)
                        .border_radius(smithy_editor::design::RADIUS_LG)
                        .keyboard_navigable()
                })
                // Focus is moved off the editor as soon as the dialog mounts.
                // Without this, typing continued to mutate a document hidden
                // behind the interaction-blocking backdrop.
                .on_event_stop(floem::event::listener::KeyDown, move |_, event| {
                    if event.key
                        == floem::prelude::Key::Named(floem::prelude::NamedKey::Escape)
                    {
                        keep_for_key();
                    }
                });
            let dialog_id = dialog.id();
            floem::action::exec_after_animation_frame(move |_| dialog_id.request_focus());

            Box::new(dialog) as Box<dyn View>
        },
    ))
    .style(move |s| {
        if intent.get().is_some() {
            s.absolute()
                .inset(0.0)
                .background(Color::from_rgba8(0, 0, 0, 204))
                .items_center()
                .justify_center()
                .z_index(500)
        } else {
            s.display(floem::taffy::Display::None)
        }
    })
}

/// Confirmation modal for shell commands the AI wants to run.
///
/// The tool loop is suspended awaiting the oneshot inside the request; both
/// buttons answer it and clear the signal. Like the diff modal, the dimming
/// overlay only exists while a request is showing.
fn shell_approval_modal(
    request: RwSignal<Option<app_state::ShellApprovalRequest>>,
    queued: app_state::Inbox<app_state::ShellApprovalRequest>,
    lifecycle: app_state::AgentLifecycle,
) -> impl IntoView {
    // Answering shows the next request rather than clearing the slot. A turn can
    // dispatch two `bash` calls at once, and the second used to be dropped by the
    // channel bridge — its oneshot destroyed, so the hook denied a command the
    // user was never shown.
    // `Rc`, not `Arc`: this never leaves floem's UI thread, and `dyn_container`
    // rebuilds its child per request so both buttons need a callable copy each
    // time.
    let advance = std::rc::Rc::new(move || {
        while let Some(next) = app_state::pop(&queued) {
            if lifecycle.accepts_event(&next.stamp) {
                request.set(Some(next));
                return;
            }
            next.abandon();
        }
        request.set(None);
    });

    Container::new(dyn_container(
        move || request.get(),
        move |req| {
            if let Some(req) = req {
                let command = req.command.clone();
                let req_deny = req.clone();
                let req_allow = req;
                let advance_deny = advance.clone();
                let advance_allow = advance.clone();
                Box::new(
                    Stack::vertical((
                        Label::derived(|| "Run shell command?".to_string()).style(|s| {
                            s.color(Color::WHITE)
                                .font_size(16.0)
                                .font_bold()
                                .margin_bottom(12.0)
                        }),
                        Label::derived(move || command.clone()).style(|s| {
                            s.color(catppuccin::TEXT)
                                .font_size(13.0)
                                .font_family(smithy_editor::design::MONO.to_string())
                                .padding(12.0)
                                .background(catppuccin::CRUST)
                                .border_radius(6.0)
                                .width_full()
                                .margin_bottom(16.0)
                        }),
                        Stack::horizontal((
                            Container::new(Empty::new()).style(|s| s.flex_grow(1.0)),
                            Button::new("Deny")
                                .on_event_stop(floem::event::listener::Click, move |_, _| {
                                    req_deny.respond(false);
                                    advance_deny();
                                })
                                .style(|s| {
                                    s.background(catppuccin::SURFACE0)
                                        .color(catppuccin::TEXT)
                                        .padding_horiz(20.0)
                                        .padding_vert(8.0)
                                        .border_radius(6.0)
                                }),
                            Button::new("Run command")
                                .on_event_stop(floem::event::listener::Click, move |_, _| {
                                    req_allow.respond(true);
                                    advance_allow();
                                })
                                .style(|s| {
                                    s.background(catppuccin::GREEN)
                                        .color(catppuccin::BASE)
                                        .padding_horiz(20.0)
                                        .padding_vert(8.0)
                                        .border_radius(6.0)
                                        .margin_left(8.0)
                                }),
                        ))
                        .style(|s| s.width_full()),
                    ))
                    .style(|s| {
                        s.width(560.0)
                            .padding(24.0)
                            .background(catppuccin::BASE)
                            .border(1.0)
                            .border_color(catppuccin::SURFACE1)
                            .border_radius(8.0)
                    }),
                ) as Box<dyn View>
            } else {
                Box::new(Empty::new().style(|s| s.display(floem::taffy::Display::None)))
                    as Box<dyn View>
            }
        },
    ))
    .style(move |s| {
        if request.get().is_some() {
            s.absolute()
                .inset(0.0)
                .background(Color::from_rgba8(0, 0, 0, 204))
                .items_center()
                .justify_center()
                .z_index(300)
        } else {
            s.display(floem::taffy::Display::None)
        }
    })
}

/// Rebuild the project outline shown behind an empty editor.
///
/// On a worker: the extraction shells out to `cargo metadata` and parses every
/// source file with tree-sitter, which is not something to do between a click
/// and a repaint. The result crosses back through a channel because floem's
/// signals are not `Send`.
///
/// This is deliberately **not** the agent context block. That one includes the
/// public API and is sized for a model; dumping it behind the shortcuts looked
/// like a wall of `pub struct` and nothing like the Benzi-style call map.
fn refresh_project_map(agent: &app_state::AgentState, map: floem::reactive::RwSignal<String>) {
    let project = agent.project.borrow().clone();
    let (tx, rx) = crossbeam_channel::bounded::<String>(1);

    runtime::tokio_runtime().spawn(async move {
        let rendered = tokio::task::spawn_blocking(move || project.outline())
            .await
            .unwrap_or_default();
        let _ = tx.send(rendered);
    });

    // Same bridge every other payload uses.
    let (tick, inbox) = app_state::bridge(rx);
    floem::reactive::Effect::new(move |_| {
        tick.get();
        for rendered in app_state::drain(&inbox) {
            map.set(rendered);
        }
    });
}

/// Ask for a directory, then ground the agent in it.
fn open_project_dialog(agent: &app_state::AgentState, hover_epoch: RwSignal<u64>) {
    let Some(dir) = smithy_editor::file_dialog::pick_folder() else {
        return;
    };
    switch_project(agent, dir, hover_epoch);
}

/// Re-ground the agent in a different project.
///
/// This tears down and rebuilds the session rather than mutating it. The project
/// description lives in the system prompt, which is frozen for the life of a
/// session — rewriting it in place would invalidate the model's prefix cache and
/// silently make every subsequent turn pay a full cold prefill.
fn switch_project(
    agent: &app_state::AgentState,
    root: std::path::PathBuf,
    hover_epoch: RwSignal<u64>,
) {
    let project = match smithy_project::Project::discover(&root)
        .or_else(|_| smithy_project::Project::open(&root))
    {
        Ok(p) => p,
        Err(e) => {
            agent.panel.push(smithy_editor::AgentEntry::Error(e));
            return;
        }
    };

    // Retire the old generation before any project-relative state moves. A
    // turn can still be unwinding on the runtime, but from this point its
    // events, reviews and approvals no longer belong to the visible project.
    let session_transition =
        app_state::begin_project_transition(agent, project.root.clone());
    hover_epoch.update(|epoch| *epoch = epoch.wrapping_add(1));

    let _ = agent.registry.touch(&project.root, &project.name);
    *agent.sessions.borrow_mut() =
        smithy_agent::SessionStore::new(agent.registry.sessions_dir(&project.root)).ok();
    *agent.session_id.borrow_mut() = app_state::new_session_id();

    *agent.project.borrow_mut() = project.clone();

    // Re-root the language server. It is keyed by (language, root), but nothing
    // used to tell it the root had moved — so diagnostics, hover and
    // go-to-definition were answered by a server analysing the previous project,
    // which also stayed resident with that project's whole crate graph loaded.
    agent.lsp_handle.initialize(project.root.clone());

    // And the file watcher, which had the same defect and kept watching the tree
    // you had left — so nothing in the new project ever reported a change.
    if let Some(watcher) = agent.file_watcher.borrow().as_ref() {
        watcher.rebase(project.root.clone());
    }

    // And the terminal's working directory, set once at startup and never again,
    // so every shell opened after a switch started in the previous project.
    // Existing tabs keep their own cwd — a running shell is the user's, and
    // moving it under them would be worse than leaving it where they put it.
    if let Ok(mut tabs) = agent.terminal_tabs.try_borrow_mut() {
        tabs.set_cwd(project.root.clone());
    }

    // Point the explorer at the new project too — an explorer still showing the
    // previous tree while the agent works somewhere else is actively misleading.
    if let Ok(mut browser) = agent.file_browser.try_borrow_mut() {
        browser.set_root(project.root.clone());
    }
    agent.file_browser_refresh.update(|v| *v += 1);

    // Empty the Problems panel. Diagnostics are keyed by file and replaced per
    // file, so nothing ever displaced the previous project's rows — the new
    // project's were simply added to them, and the count grew on every switch.
    agent.diagnostics.clear();
    agent.diagnostics.server_status.set(None);
    // Zero until the new project's server reports in, so the panel says "nothing
    // has been checked" rather than "no problems" during the gap.
    agent.diagnostics.server_count.set(0);

    agent.panel.clear();
    // Attachments are named relative to the project root, and a chip left over
    // from the previous one would be labelled against a tree it does not live
    // in — the same class of mistake the transition's review abandonment exists
    // to prevent.
    agent.panel.clear_attachments();
    agent.panel.project_root.set(project.root.clone());
    // The map behind an empty editor belongs to the project, so it moves too.
    agent.project_map.set(String::new());
    refresh_project_map(agent, agent.project_map);
    // Same for the call graph — a previous project's map would be a lie.
    call_graph::clear(agent.call_graph);
    agent.call_graph.visible.set(false);
    call_graph::load_for_project(agent);
    agent
        .panel
        .model_label
        .set(format!("opening {}…", project.name));
    app_state::finish_project_transition(agent, session_transition);
}

#[cfg(test)]
mod lsp_stamp_tests {
    use super::*;

    fn stamp(root: &str, generation: u64, client: u64) -> smithy_editor::LspStamp {
        smithy_editor::LspStamp {
            root_path: std::path::PathBuf::from(root),
            generation,
            client_id: Some(client),
        }
    }

    /// Project switch clears the Problems panel, but an old-root diagnostic
    /// could previously arrive afterward and repopulate it.
    #[test]
    fn an_old_root_diagnostic_is_discarded_after_project_switch() {
        let mut accepted = Some(stamp("/old", 3, 8));
        assert!(!accept_lsp_stamp(
            std::path::Path::new("/new"),
            &mut accepted,
            &stamp("/old", 3, 8),
            false,
        ));
    }

    /// A newer initialize intent wins even at the same root; delayed Ready and
    /// diagnostics from its predecessor cannot roll the panel backward.
    #[test]
    fn an_old_generation_cannot_replace_the_current_lsp_status() {
        let mut accepted = Some(stamp("/project", 9, 22));
        assert!(!accept_lsp_stamp(
            std::path::Path::new("/project"),
            &mut accepted,
            &stamp("/project", 8, 21),
            true,
        ));
    }

    /// Replacement status changes the accepted client identity. Diagnostics
    /// already queued by the crashed client are rejected afterward.
    #[test]
    fn stale_client_diagnostics_cannot_repopulate_the_cleared_panel() {
        let mut accepted = Some(stamp("/project", 4, 11));
        assert!(accept_lsp_stamp(
            std::path::Path::new("/project"),
            &mut accepted,
            &stamp("/project", 4, 12),
            true,
        ));
        assert!(!accept_lsp_stamp(
            std::path::Path::new("/project"),
            &mut accepted,
            &stamp("/project", 4, 11),
            false,
        ));
    }

    /// Exhausting restart attempts is a disconnected status, not a log-only
    /// event that leaves the Problems panel claiming one healthy server.
    #[test]
    fn exhausted_retries_zero_the_healthy_server_count() {
        assert_eq!(server_count_for_lsp_status(false), 0);
        assert_eq!(server_count_for_lsp_status(true), 1);
    }
}
