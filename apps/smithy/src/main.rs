//! Smithy — a native Rust IDE with a local-first coding agent.
//!
//! The entry point: builds the window, wires every panel to the state in
//! [`app_state`], and owns the shortcuts and the modals that sit above them.

use floem::peniko::Color;
use floem::prelude::*;

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
use editor::{handle_file_open, handle_tab_click, handle_tab_close, EditorComponent};
use terminal::MultiTerminalComponent;

/// Everything that has to be told to stop, reachable from a signal handler.
///
/// Deliberately only `Send` things. The panels, the tab manager and the buffers all
/// live in `Rc<RefCell<_>>` on floem's main thread and cannot be touched from here;
/// what needs stopping is a language server (reached by a channel) and a set of
/// shell processes (reached through `smithy_editor::kill_all_shells`).
struct ExitHooks {
    lsp: smithy_editor::LspHandle,
}

impl ExitHooks {
    /// Ask everything to stop. Idempotent, and safe on a signal path.
    fn run(&self) {
        // Asks the server to exit rather than killing it: `kill_on_drop` covers
        // the hard case, but it never gets the chance on a signal, and a server
        // told to shut down leaves no lock files or half-written caches behind.
        self.lsp.shutdown();
        // The one leak neither `kill_on_drop` nor the process group covers: a pty
        // child is in its own session, so `Ctrl-C` never reaches it.
        smithy_editor::kill_all_shells();
    }
}

/// Install a `Ctrl-C` handler that runs the exit hooks before the process dies.
///
/// Without this, SIGINT terminates the process with no Rust cleanup at all — no
/// `Drop`, no LSP `shutdown`, and every pty shell reparented to `launchd`. Running
/// from a terminal and quitting with `Ctrl-C` is the normal way this app is used
/// during development, so it is the normal path, not the exceptional one.
fn install_signal_handler(hooks: ExitHooks) {
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
        .window(|_| app_view(), Some(config))
        .run();

    // The window closed normally. The signal path exits the process itself, so
    // reaching here means `run()` returned and the hooks have not fired.
    smithy_editor::kill_all_shells();
}

/// Create the main application view
fn app_view() -> impl IntoView {
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
    install_signal_handler(ExitHooks {
        lsp: app_state.lsp_handle.clone(),
    });

    // Translate agent events into panel state, then connect in the background.
    setup_agent_effect(agent_state.clone());
    app_state::setup_shell_approval_effect(agent_state.clone());
    app_state::setup_auto_approve_effect(agent_state.clone());
    connect_agent(&agent_state);

    // The microphone. Built once and shared: the button and the hotkey must
    // press the *same* handle, or one of them silently does nothing — which is
    // what happened, and only an unused-`mut` warning gave it away.
    //
    // `None` when there is no capture device at all, in which case the button
    // reports that rather than pretending.
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
            hover_state.request_started();
            lsp_handle.hover(handle.path.clone(), line, column);
        }
    };

    let ask_definition = {
        let lsp_handle = app_state.lsp_handle.clone();
        move || {
            let Some(handle) = open_editor.get_untracked() else {
                return;
            };
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
    let editor_view = dyn_container(
        move || cg_ui.visible.get(),
        move |show_graph| {
            if show_graph {
                let for_build = for_build.clone();
                call_graph::call_graph_view(
                    cg_ui,
                    project_root_sig,
                    open_from_graph.clone(),
                    move || call_graph::build(&for_build),
                )
                .into_any()
            } else {
                // Rebuilt when returning from the graph — same as opening a
                // file, which already constructs a fresh editor pane.
                Stack::vertical((
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
                        buffer_manager_for_editor.clone(),
                        open_editor,
                    ))
                    .style(|s| s.flex_grow(1.0).width_full().min_height(0.0)),
                ))
                .style(|s| s.width_full().height_full())
                .into_any()
            }
        },
    )
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
        // through `AgentUiEvent::Stopped`, so the button reflects the turn
        // actually ending rather than the click being registered.
        let stopper = agent_state.stopper.clone();
        let on_stop = std::rc::Rc::new(move || {
            if let Ok(s) = stopper.lock() {
                if let Some(s) = s.as_ref() {
                    s.stop();
                }
            }
        });
        let voice = voice_control.clone();
        // Try the endpoint again, for the case the plan did not cover: launching
        // Smithy before the model is being served. The connection is otherwise
        // attempted once at startup and once per project switch, so a model
        // started afterwards was unreachable without restarting the editor.
        let for_reconnect = agent_state.clone();
        let on_reconnect = move || {
            for_reconnect.panel.connected.set(false);
            for_reconnect.panel.model_label.set("connecting…".into());
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
            move || {
                if let Some(voice) = &voice {
                    voice.press();
                }
            },
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

    let on_tab_close = {
        let buffer_manager = app_state.buffer_manager.clone();
        let active_buffer = signals.active_buffer;
        let buffer_states = signals.buffer_states;

        move |id| {
            handle_tab_close(id, &buffer_manager, active_buffer, buffer_states);
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
            tokio::sync::mpsc::channel::<(String, Vec<smithy_editor::LspDiagnostic>)>(1).0,
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
                match response {
                    smithy_editor::LspResponse::Diagnostics {
                        path,
                        diagnostics: incoming,
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
                    smithy_editor::LspResponse::Hover { result, .. } => {
                        hover_state.show(result.map(|h| h.contents));
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
                    smithy_editor::LspResponse::Error { message, .. } => {
                        diagnostics.server_status.set(Some(message));
                        diagnostics.server_count.set(0);
                    }
                    // So an empty panel can say which kind of empty it is.
                    smithy_editor::LspResponse::Ready { servers } => {
                        diagnostics.server_count.set(servers);
                        diagnostics.server_status.set(None);
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
                        move || open_project_dialog(&agent_state)
                    },
                ),
                smithy_editor::MenuItem::action_with(
                    "Save",
                    smithy_editor::accel("S"),
                    move || {
                        if let Some(handle) = open_editor.get_untracked() {
                            if let Err(e) = handle.save() {
                                eprintln!("{e}");
                            }
                            // Saving answers the question the bar was asking.
                            external_change.set(None);
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
                        move || switch_project(&agent_state, root.clone()),
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
                    move || {
                        agent_state.panel.connected.set(false);
                        agent_state.panel.model_label.set("connecting…".into());
                        connect_agent(&agent_state);
                    }
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
        let session = agent_state.session.clone();
        let settings_dir = settings_dir.clone();

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
                &session,
                &balance_cache,
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
                switch_project(&agent_for_pick, dir);
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
                    if let Some(voice) = &voice_control {
                        voice.press();
                    }
                }
                // Save. Cmd on macOS, Ctrl elsewhere — accept either rather than
                // making the shortcut platform-dependent in a cross-platform app.
                if key_event.key == floem::prelude::Key::Character("s".into()) && cmd {
                    handled = true;
                    if let Some(handle) = open_editor.get_untracked() {
                        match handle.save() {
                            Ok(()) => {
                                editor_version.update(|v| *v += 1);
                                // Saving answers the question the bar was asking.
                                external_change.set(None);
                            }
                            Err(e) => eprintln!("{e}"),
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
    // The **live** project, not a snapshot of its root.
    //
    // This was `agent_state.project.borrow().root.clone()`, captured once here at
    // view construction. Accepting a review then wrote through a `Workspace` rooted
    // at whichever project the app had *started* in — so a README accepted while
    // working in another project overwrote this repo's own README, and the damage
    // was committed by a later `git add -A`. Confirmed against the git history, not
    // inferred.
    //
    // Third instance of this exact mistake: the diagnostics root and the
    // language-server root were both startup snapshots too. Anything derived from
    // `agent_state.project` must be read at the moment it is used.
    let review_project = agent_state.project.clone();
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
    // why this takes the `tool_call_id`. `outcomes` is now only the fallback for
    // a decision nobody is waiting on (a review abandoned by a project switch,
    // or one whose turn has already ended), where the next turn's preamble is
    // still the only way to say what happened.
    let record_outcome = {
        let panel = agent_state.panel;
        let outcomes = agent_state.review.outcomes.clone();
        let review = agent_state.review.clone();
        move |id: &str, path: &str, accepted: usize, total: usize| {
            let note = agent::describe_review_outcome(path, accepted, total);
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
        let project = review_project.clone();
        let advance_queue = advance_queue.clone();
        let record_outcome = record_outcome.clone();
        move |diff: FileDiff, statuses: Vec<smithy_editor::ChangeStatus>| {
            let Some(id) = current_diff.get_untracked().map(|c| c.id) else {
                return;
            };
            let total = diff.hunks.len();
            let accepted = statuses
                .iter()
                .filter(|s| **s == smithy_editor::ChangeStatus::Accepted)
                .count();
            let content = smithy_editor::content_with_accepted_hunks(&diff, &statuses);

            // Through the workspace capability, not a joined path — an accepted
            // diff is still a model-supplied path and gets the same confinement
            // every other write does.
            let root = project.borrow().root.clone();
            match agent::apply_change(&root, &diff.path, &content) {
                Ok(()) => record_outcome(&id, &diff.path, accepted, total),
                Err(e) => {
                    eprintln!("could not apply the accepted change to {}: {e}", diff.path);
                    // The write failed, so nothing landed. Telling the model it
                    // was accepted would be a lie it then edits against.
                    record_outcome(&id, &diff.path, 0, total);
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
                record_outcome(&change.id, change.path(), 0, change.diff.hunks.len());
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
                record_outcome(&change.id, change.path(), 0, change.diff.hunks.len());
                advance_queue(change.id);
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
        diff_modal(current_diff, on_diff_accept, on_diff_reject, on_diff_close),
        shell_approval_modal(agent_state.shell_approval, agent_state.shell_inbox.clone()),
        smithy_editor::settings_modal(
            settings_state,
            {
                // Saving reconnects, because a backend you selected and did not
                // connect to is not a setting that has taken effect.
                //
                // A *different* provider, model, or URL starts a fresh session —
                // replaying the previous model's transcript into a new one left
                // the chat looking unchanged aside from a "Connected · …" notice,
                // which is how a model switch used to look like it did nothing.
                // Same endpoint → reconnect and resume, the way the header's
                // Reconnect does.
                let agent_state = agent_state.clone();
                let dir = settings_dir.clone();
                move || {
                    let previous = smithy_agent::AgentConfig::load(&dir);
                    match settings::save(settings_state, &dir) {
                        Ok(warnings) => {
                            settings_state.close();
                            for warning in &warnings {
                                eprintln!("[settings] {warning}");
                            }
                            let next = smithy_agent::AgentConfig::load(&dir);
                            let switched = previous.provider != next.provider
                                || previous.active().model != next.active().model
                                || previous.active().base_url != next.active().base_url;
                            if switched {
                                app_state::clear_context(&agent_state);
                            } else {
                                agent_state.panel.connected.set(false);
                                agent_state.panel.model_label.set("connecting…".into());
                                connect_agent(&agent_state);
                            }
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
}

/// Confirmation modal for shell commands the AI wants to run.
///
/// The tool loop is suspended awaiting the oneshot inside the request; both
/// buttons answer it and clear the signal. Like the diff modal, the dimming
/// overlay only exists while a request is showing.
fn shell_approval_modal(
    request: RwSignal<Option<app_state::ShellApprovalRequest>>,
    queued: app_state::Inbox<app_state::ShellApprovalRequest>,
) -> impl IntoView {
    // Answering shows the next request rather than clearing the slot. A turn can
    // dispatch two `bash` calls at once, and the second used to be dropped by the
    // channel bridge — its oneshot destroyed, so the hook denied a command the
    // user was never shown.
    // `Rc`, not `Arc`: this never leaves floem's UI thread, and `dyn_container`
    // rebuilds its child per request so both buttons need a callable copy each
    // time.
    let advance = std::rc::Rc::new(move || request.set(app_state::pop(&queued)));

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
fn open_project_dialog(agent: &app_state::AgentState) {
    let Some(dir) = smithy_editor::file_dialog::pick_folder() else {
        return;
    };
    switch_project(agent, dir);
}

/// Re-ground the agent in a different project.
///
/// This tears down and rebuilds the session rather than mutating it. The project
/// description lives in the system prompt, which is frozen for the life of a
/// session — rewriting it in place would invalidate the model's prefix cache and
/// silently make every subsequent turn pay a full cold prefill.
fn switch_project(agent: &app_state::AgentState, root: std::path::PathBuf) {
    let project = match smithy_project::Project::discover(&root)
        .or_else(|_| smithy_project::Project::open(&root))
    {
        Ok(p) => p,
        Err(e) => {
            agent.panel.push(smithy_editor::AgentEntry::Error(e));
            return;
        }
    };

    let _ = agent.registry.touch(&project.root, &project.name);
    *agent.sessions.borrow_mut() =
        smithy_agent::SessionStore::new(agent.registry.sessions_dir(&project.root)).ok();
    *agent.session_id.borrow_mut() = app_state::new_session_id();

    // **Before** the root moves.
    //
    // A queued review holds a workspace-relative path and is written through
    // whichever root is live when it is accepted. Leaving one queued across a
    // switch therefore aims it at the new project — `README.md` proposed in one
    // repository, written into the next — and the sandbox permits it, because
    // that path is legitimate there. This is the same shape as the accident that
    // overwrote this repository's own README, and reading the root live (which
    // is right, and stays) is what makes it reachable.
    agent.review.abandon();

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
    agent.panel.connected.set(false);
    // Attachments are named relative to the project root, and a chip left over
    // from the previous one would be labelled against a tree it does not live
    // in — the same class of mistake `review.abandon` above exists to prevent.
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
    app_state::connect_agent(agent);
}
