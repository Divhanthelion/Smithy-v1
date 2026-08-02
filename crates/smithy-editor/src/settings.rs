//! The settings dialog — choosing a backend from inside the editor.
//!
//! ## Why this holds strings and not a config type
//!
//! `smithy-editor` does not depend on `smithy-agent`, and this module is not the
//! place to start. The agent panel already works this way: it renders
//! [`crate::agent_panel::Entry`] values the app translates into, so the UI layer
//! never learns what a `Session` is. The same rule here means the dialog is a
//! form over plain signals, and the app owns the round trip to `AgentConfig`.
//!
//! It also happens to be what a form *wants*. A half-edited base URL is a string
//! and not an `Endpoint`, and a dialog that could only hold valid configurations
//! would have to reject keystrokes rather than fields.
//!
//! ## The key field
//!
//! **The stored key is never read back onto the screen.** The field starts empty
//! whether or not a key exists; when one does, the placeholder says so. An empty
//! field on save therefore means *leave it alone*, and clearing a stored key is
//! its own explicit button rather than a side effect of deleting text you cannot
//! see.
//!
//! What it is not: floem's `TextInput` has no password mode at this revision, so
//! a key is legible while you type it. Reimplementing text editing to add
//! bullets would be a poor trade for that. The mitigations that matter are the
//! ones above — the value is never displayed after the fact, never written to
//! the settings file, and the field is emptied the moment it is saved.

use floem::prelude::*;
use floem::reactive::{Memo, RwSignal, SignalGet, SignalUpdate};
use floem::style::CustomStylable;

use crate::theme::catppuccin;

/// Which backends the dialog offers, in the order it offers them.
///
/// Mirrors `smithy_agent::ProviderChoice` by value rather than by type, for the
/// reason in the module docs. The tags are that enum's serialized form, so the
/// app's translation is a string match and not a lookup table.
pub const PROVIDERS: [(&str, &str, &str); 3] = [
    (
        "lmstudio",
        "LM Studio",
        "A local OpenAI-compatible server. No key needed.",
    ),
    (
        "openrouter",
        "OpenRouter",
        "Hundreds of hosted models, including a free tier.",
    ),
    (
        "deepseek",
        "DeepSeek",
        "DeepSeek's own API — 1M context, cheap, tool-capable.",
    ),
];

/// One selectable model, with everything already rendered to strings.
///
/// The app builds these from `smithy_agent::ModelEntry`. Pre-rendering `badge`
/// and `context` there rather than passing numbers here is what keeps this crate
/// from needing to know what a pricing tier is — the same reason the transcript
/// renders [`crate::agent_panel::Entry`] and not a `Message`.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelRow {
    pub id: String,
    pub label: String,
    /// e.g. `262k ctx`.
    pub context: String,
    /// e.g. `free`, `$0.09/$0.18 per M`, `29.5 GB · loaded`.
    pub badge: String,
    /// False means the agent cannot use it at all — see the filter's docs.
    pub tool_capable: bool,
    pub free: bool,
    /// A model on this machine, which can be loaded rather than merely chosen.
    pub local: bool,
    pub loaded: bool,
}

/// Everything the dialog renders from and writes back into.
#[derive(Clone, Copy)]
pub struct SettingsState {
    pub open: RwSignal<bool>,
    /// The selected backend's tag — `"lmstudio"` or `"openrouter"`.
    pub provider: RwSignal<String>,
    pub lmstudio_url: RwSignal<String>,
    pub lmstudio_model: RwSignal<String>,
    pub openrouter_url: RwSignal<String>,
    pub openrouter_model: RwSignal<String>,
    pub deepseek_url: RwSignal<String>,
    pub deepseek_model: RwSignal<String>,
    /// A *newly typed* key. Empty means "leave the stored one alone" — see the
    /// module docs on why this is never seeded from storage.
    pub openrouter_key: RwSignal<String>,
    pub deepseek_key: RwSignal<String>,
    pub brave_key: RwSignal<String>,
    /// Whether a key is already in the credential store. Drives the placeholder
    /// and whether "Remove" is offered, without revealing the value.
    pub openrouter_key_stored: RwSignal<bool>,
    pub deepseek_key_stored: RwSignal<bool>,
    pub brave_key_stored: RwSignal<bool>,
    /// Whether the OS credential store answered at all. A machine where it did
    /// not must say so *before* you type a key into a field that cannot save it.
    pub keychain_available: RwSignal<bool>,
    /// The line under the buttons: what just happened, or what is wrong.
    pub status: RwSignal<String>,
    /// Whether `status` is a failure. Colour only; the text carries the meaning.
    pub status_is_error: RwSignal<bool>,

    // --- the model picker ---
    /// Everything the selected backend offers, unfiltered. Filtering happens at
    /// render time so toggling a checkbox does not require a refetch.
    pub models: RwSignal<Vec<ModelRow>>,
    pub model_query: RwSignal<String>,
    /// Hide models that cannot call tools. **On by default, and it should be**:
    /// Smithy's loop is entirely tool-driven, so a model without tool support
    /// does not produce a worse turn, it produces an empty one.
    pub tools_only: RwSignal<bool>,
    /// Hide metered models. On by default for OpenRouter, off for LM Studio
    /// where nothing has a price at all.
    pub free_only: RwSignal<bool>,
    /// Whether a fetch is in flight, so the list can say so rather than looking
    /// empty — an empty list and an unfetched list are different things.
    pub loading_models: RwSignal<bool>,
    /// Why the list is empty, when it is.
    pub models_error: RwSignal<String>,
    /// The model currently being loaded into LM Studio, if any. Loading a 30 GB
    /// model takes tens of seconds and the row has to say so.
    pub loading_into_memory: RwSignal<String>,
}

impl SettingsState {
    pub fn new() -> Self {
        Self {
            open: RwSignal::new(false),
            provider: RwSignal::new("lmstudio".to_string()),
            lmstudio_url: RwSignal::new(String::new()),
            lmstudio_model: RwSignal::new(String::new()),
            openrouter_url: RwSignal::new(String::new()),
            openrouter_model: RwSignal::new(String::new()),
            deepseek_url: RwSignal::new(String::new()),
            deepseek_model: RwSignal::new(String::new()),
            openrouter_key: RwSignal::new(String::new()),
            deepseek_key: RwSignal::new(String::new()),
            brave_key: RwSignal::new(String::new()),
            openrouter_key_stored: RwSignal::new(false),
            deepseek_key_stored: RwSignal::new(false),
            brave_key_stored: RwSignal::new(false),
            keychain_available: RwSignal::new(true),
            status: RwSignal::new(String::new()),
            status_is_error: RwSignal::new(false),
            models: RwSignal::new(Vec::new()),
            model_query: RwSignal::new(String::new()),
            tools_only: RwSignal::new(true),
            free_only: RwSignal::new(true),
            loading_models: RwSignal::new(false),
            models_error: RwSignal::new(String::new()),
            loading_into_memory: RwSignal::new(String::new()),
        }
    }

    pub fn selected(&self) -> String {
        self.provider.get()
    }

    /// The model field belonging to whichever backend is selected.
    pub fn active_model(&self) -> RwSignal<String> {
        match self.provider.get_untracked().as_str() {
            "openrouter" => self.openrouter_model,
            "deepseek" => self.deepseek_model,
            _ => self.lmstudio_model,
        }
    }

    /// The base-URL field belonging to whichever backend is selected.
    pub fn active_url(&self) -> RwSignal<String> {
        match self.provider.get_untracked().as_str() {
            "openrouter" => self.openrouter_url,
            "deepseek" => self.deepseek_url,
            _ => self.lmstudio_url,
        }
    }

    /// The rows that survive the search box and the filter toggles.
    ///
    /// Recomputed on every render rather than cached, because the inputs are
    /// three signals and a `Vec` of a few hundred short strings — cheaper to
    /// redo than to invalidate correctly.
    pub fn visible_models(&self) -> Vec<ModelRow> {
        let query = self.model_query.get();
        let tools_only = self.tools_only.get();
        let free_only = self.free_only.get();
        self.models
            .get()
            .into_iter()
            .filter(|m| !tools_only || m.tool_capable)
            .filter(|m| !free_only || m.free || m.local)
            .filter(|m| matches_query(m, &query))
            .collect()
    }

    /// Point the selected backend at `id`.
    pub fn choose_model(&self, id: &str) {
        self.active_model().set(id.to_string());
        self.status.set(String::new());
    }

    /// Clear the transient fields. Called after a save so a typed key does not
    /// sit in a signal — or on screen — for the rest of the session.
    pub fn forget_typed_secrets(&self) {
        self.openrouter_key.set(String::new());
        self.brave_key.set(String::new());
    }

    pub fn report(&self, message: impl Into<String>, is_error: bool) {
        self.status.set(message.into());
        self.status_is_error.set(is_error);
    }

    pub fn close(&self) {
        self.open.set(false);
        self.forget_typed_secrets();
        self.status.set(String::new());
    }
}

impl Default for SettingsState {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether a row matches the search box.
///
/// Every whitespace-separated term must appear somewhere in the id or the
/// label, case-insensitively — so "gemma 26" finds
/// `google/gemma-4-26b-a4b-it:free` without anyone typing the punctuation.
fn matches_query(row: &ModelRow, query: &str) -> bool {
    if query.trim().is_empty() {
        return true;
    }
    let haystack = format!("{} {}", row.id, row.label).to_lowercase();
    query
        .split_whitespace()
        .all(|term| haystack.contains(&term.to_lowercase()))
}

/// The dialog. Occupies no space and paints nothing while closed.
///
/// `on_save` receives nothing: the state is a set of `Copy` signals the caller
/// already holds, so it reads the fields itself. That keeps the signature stable
/// as fields are added, which for a settings form is the only certainty.
pub fn settings_modal(
    state: SettingsState,
    on_save: impl Fn() + 'static,
    on_clear_key: impl Fn(&str) + 'static,
    on_refresh_models: impl Fn() + 'static,
    on_load_model: impl Fn(&str) + 'static,
) -> impl IntoView {
    let on_save = std::rc::Rc::new(on_save);
    let on_clear_key = std::rc::Rc::new(on_clear_key);
    let on_refresh_models = std::rc::Rc::new(on_refresh_models);
    let on_load_model = std::rc::Rc::new(on_load_model);

    Container::new(dyn_container(
        move || state.open.get(),
        move |open| {
            if !open {
                return Empty::new()
                    .style(|s| s.display(floem::taffy::Display::None))
                    .into_any();
            }
            panel(
                state,
                on_save.clone(),
                on_clear_key.clone(),
                on_refresh_models.clone(),
                on_load_model.clone(),
            )
            .into_any()
        },
    ))
    .style(move |s| {
        if state.open.get() {
            s.absolute()
                .inset(0.0)
                .background(Color::from_rgba8(0, 0, 0, 204))
                .items_center()
                .justify_center()
                // Above the shell and the menus, alongside the other modals.
                .z_index(300)
        } else {
            s.display(floem::taffy::Display::None)
        }
    })
}

/// The dialog's body.
///
/// Header and buttons are pinned and the middle scrolls. The model picker made
/// this necessary: a list of three hundred OpenRouter models plus two sets of
/// fields is taller than a laptop screen, and a modal that runs off the bottom
/// takes its Save button with it.
fn panel(
    state: SettingsState,
    on_save: std::rc::Rc<dyn Fn()>,
    on_clear_key: std::rc::Rc<dyn Fn(&str)>,
    on_refresh_models: std::rc::Rc<dyn Fn()>,
    on_load_model: std::rc::Rc<dyn Fn(&str)>,
) -> impl IntoView {
    let save = on_save.clone();
    let clear_for_provider = on_clear_key.clone();
    let refresh_for_switch = on_refresh_models.clone();

    let body = Stack::vertical((
        // The choice itself. Switching refetches, because the list belongs to
        // the backend and showing OpenRouter's models under LM Studio would be
        // worse than showing none.
        Stack::vertical((
            provider_row(state, 0, refresh_for_switch.clone()),
            provider_row(state, 1, refresh_for_switch.clone()),
            provider_row(state, 2, refresh_for_switch),
        ))
        .style(|s| s.width_full().gap(6.0).margin_bottom(14.0)),
        // Only the selected backend's fields, because a form showing settings
        // that do nothing invites you to edit them and wonder why nothing
        // changed.
        dyn_container(
            move || state.selected(),
            move |choice| match choice.as_str() {
                "openrouter" => openrouter_fields(state, clear_for_provider.clone()).into_any(),
                "deepseek" => deepseek_fields(state, clear_for_provider.clone()).into_any(),
                _ => lmstudio_fields(state).into_any(),
            },
        ),
        model_picker(state, on_refresh_models, on_load_model),
        // Search is provider-independent, so it sits outside the switch.
        divider(),
        brave_fields(state, on_clear_key.clone()),
    ))
    .style(|s| s.width_full());

    Stack::vertical((
        Label::derived(|| "Agent backend".to_string()).style(|s| {
            s.color(Color::WHITE)
                .font_size(16.0)
                .font_bold()
                .margin_bottom(3.0)
        }),
        Label::derived(|| {
            "Applies on save — the agent reconnects and starts a fresh session.".to_string()
        })
        .style(|s| {
            s.color(catppuccin::OVERLAY1)
                .font_size(11.0)
                .margin_bottom(14.0)
        }),
        floem::views::scroll::Scroll::new(body)
            .custom_style(|s: floem::views::scroll::ScrollCustomStyle| {
                s.hide_bars(false)
                    .handle_background(catppuccin::SURFACE1)
                    .handle_border_radius(4.0)
            })
            .style(|s| s.width_full().max_height(460.0).min_width(0.0)),
        keychain_warning(state),
        status_line(state),
        Stack::horizontal((
            Container::new(Empty::new()).style(|s| s.flex_grow(1.0)),
            Button::new("Cancel")
                .on_event_stop(floem::event::listener::Click, move |_, _| state.close())
                .style(|s| {
                    s.background(catppuccin::SURFACE0)
                        .color(catppuccin::TEXT)
                        .padding_horiz(18.0)
                        .padding_vert(8.0)
                        .border_radius(6.0)
                }),
            Button::new("Save & reconnect")
                .on_event_stop(floem::event::listener::Click, move |_, _| save())
                .style(|s| {
                    s.background(catppuccin::LAVENDER)
                        .color(catppuccin::CRUST)
                        .font_bold()
                        .padding_horiz(18.0)
                        .padding_vert(8.0)
                        .border_radius(6.0)
                        .margin_left(8.0)
                        .hover(|s| s.background(catppuccin::MAUVE))
                }),
        ))
        .style(|s| s.width_full().items_center().margin_top(16.0)),
    ))
    .style(|s| {
        s.width(600.0)
            .padding(24.0)
            .background(catppuccin::BASE)
            .border(1.0)
            .border_color(catppuccin::SURFACE1)
            .border_radius(8.0)
    })
}

/// The browsable model list.
///
/// The text field above it stays, and is the authority — this only writes into
/// it. That matters more than it sounds: a brand-new model, a self-hosted
/// endpoint, or an id the catalogue has not caught up with must still be
/// typeable, and a picker that replaced the field would make those unreachable.
fn model_picker(
    state: SettingsState,
    on_refresh: std::rc::Rc<dyn Fn()>,
    on_load: std::rc::Rc<dyn Fn(&str)>,
) -> impl IntoView {
    let refresh = on_refresh.clone();

    Stack::vertical((
        Stack::horizontal((
            Label::derived(|| "Available models".to_string())
                .style(|s| s.font_size(11.0).color(catppuccin::SUBTEXT0)),
            Container::new(Empty::new()).style(|s| s.flex_grow(1.0)),
            // Counts, so the filters are legible as filters rather than as a
            // list that mysteriously has fourteen things in it.
            Label::derived(move || {
                let total = state.models.get().len();
                if state.loading_models.get() {
                    "loading…".to_string()
                } else if total == 0 {
                    String::new()
                } else {
                    format!("{} of {total}", state.visible_models().len())
                }
            })
            .style(|s| s.font_size(10.0).color(catppuccin::SURFACE2).margin_right(8.0)),
            Label::derived(|| "Refresh".to_string())
                .on_event_stop(floem::event::listener::Click, move |_, _| refresh())
                .style(|s| {
                    s.font_size(10.0)
                        .color(catppuccin::BLUE)
                        .padding_horiz(6.0)
                        .padding_vert(1.0)
                        .border_radius(3.0)
                        .cursor(floem::style::CursorStyle::Pointer)
                        .hover(|s| s.background(catppuccin::SURFACE0))
                }),
        ))
        .style(|s| s.width_full().items_center().margin_bottom(6.0)),
        Stack::horizontal((
            TextInput::new(state.model_query)
                .placeholder("Filter by name…")
                .style(|s| {
                    text_field_style(s)
                        .flex_grow(1.0)
                        .font_size(11.0)
                        .padding_vert(5.0)
                }),
            toggle_chip(state.tools_only, "tool-capable", |s| s.margin_left(6.0)),
            // Meaningless on a local backend, where nothing has a price.
            toggle_chip(state.free_only, "free only", |s| s.margin_left(4.0)).style(move |s| {
                s.apply_if(state.selected() != "openrouter", |s| {
                    s.display(floem::taffy::Display::None)
                })
            }),
        ))
        .style(|s| s.width_full().items_center().margin_bottom(6.0)),
        // The list, or the reason there isn't one.
        dyn_container(
            move || {
                (
                    state.loading_models.get(),
                    state.models_error.get(),
                    state.models.get().len(),
                )
            },
            move |(loading, error, total)| {
                if loading {
                    return note("Fetching the model list…", catppuccin::OVERLAY1).into_any();
                }
                if !error.is_empty() {
                    return note(&error, catppuccin::PEACH).into_any();
                }
                if total == 0 {
                    return note(
                        "No models yet — press Refresh. For LM Studio the server must be running.",
                        catppuccin::SURFACE2,
                    )
                    .into_any();
                }
                model_list(state, on_load.clone()).into_any()
            },
        ),
    ))
    .style(|s| s.width_full().margin_top(14.0))
}

fn model_list(state: SettingsState, on_load: std::rc::Rc<dyn Fn(&str)>) -> impl IntoView {
    floem::views::scroll::Scroll::new(
        dyn_stack(
            move || state.visible_models(),
            |row| row.id.clone(),
            move |row| model_row(state, row, on_load.clone()),
        )
        .style(|s| s.flex_col().width_full().padding(3.0).gap(1.0)),
    )
    .custom_style(|s: floem::views::scroll::ScrollCustomStyle| {
        s.hide_bars(false)
            .handle_background(catppuccin::SURFACE1)
            .handle_border_radius(4.0)
    })
    .style(|s| {
        s.width_full()
            .height(190.0)
            .min_width(0.0)
            .background(catppuccin::CRUST)
            .border(1.0)
            .border_color(catppuccin::SURFACE0)
            .border_radius(6.0)
    })
}

fn model_row(
    state: SettingsState,
    row: ModelRow,
    on_load: std::rc::Rc<dyn Fn(&str)>,
) -> impl IntoView {
    let id = row.id.clone();
    let id_for_click = row.id.clone();
    let id_for_load = row.id.clone();
    let id_for_selected = row.id.clone();
    let id_for_loading = row.id.clone();
    let label = row.label.clone();
    let badge = row.badge.clone();
    let context = row.context.clone();
    let show_load = row.local && !row.loaded;

    // A `Memo` rather than a closure: this is read by four separate style
    // closures, and a closure capturing the id `String` is not `Copy`.
    let selected = Memo::new(move |_| state.active_model().get() == id_for_selected);

    Stack::horizontal((
        Label::derived(move || {
            if selected.get() {
                crate::design::glyph::DOT
            } else {
                crate::design::glyph::RING
            }
            .to_string()
        })
        .style(move |s| {
            s.font_family(crate::design::SYMBOL.to_string())
                .font_size(8.0)
                .margin_right(7.0)
                .color(if selected.get() {
                    catppuccin::LAVENDER
                } else {
                    catppuccin::SURFACE1
                })
        }),
        Stack::vertical((
            Label::derived(move || id.clone()).style(move |s| {
                s.font_size(11.0)
                    .font_family(crate::design::MONO.to_string())
                    .color(if selected.get() {
                        catppuccin::TEXT
                    } else {
                        catppuccin::SUBTEXT0
                    })
            }),
            Label::derived(move || label.clone())
                .style(|s| s.font_size(9.0).color(catppuccin::SURFACE2)),
        ))
        .style(|s| s.gap(1.0).flex_grow(1.0).min_width(0.0)),
        Label::derived(move || context.clone())
            .style(|s| s.font_size(9.0).color(catppuccin::SURFACE2).margin_right(8.0)),
        Label::derived(move || badge.clone()).style(move |s| {
            s.font_size(9.0).color(if row.free {
                catppuccin::GREEN
            } else if row.loaded {
                catppuccin::GREEN
            } else {
                catppuccin::SURFACE2
            })
        }),
        // Local models can be made resident from here. Optional — LM Studio's
        // JIT loader would pull it in on the first request anyway — but that
        // turns pressing Send into a minute of apparent hang, which is the
        // confusion this removes.
        Label::derived(move || {
            if state.loading_into_memory.get() == id_for_loading {
                "loading…".to_string()
            } else {
                "Load".to_string()
            }
        })
        .on_event_stop(floem::event::listener::Click, move |_, _| {
            on_load(&id_for_load)
        })
        .style(move |s| {
            s.font_size(9.0)
                .margin_left(8.0)
                .padding_horiz(6.0)
                .padding_vert(1.0)
                .border_radius(3.0)
                .background(catppuccin::SURFACE0)
                .color(catppuccin::BLUE)
                .cursor(floem::style::CursorStyle::Pointer)
                .hover(|s| s.background(catppuccin::SURFACE1))
                .apply_if(!show_load, |s| s.display(floem::taffy::Display::None))
        }),
    ))
    .on_event_stop(floem::event::listener::Click, move |_, _| {
        state.choose_model(&id_for_click)
    })
    .style(move |s| {
        s.width_full()
            .items_center()
            .padding_horiz(8.0)
            .padding_vert(5.0)
            .border_radius(4.0)
            .cursor(floem::style::CursorStyle::Pointer)
            .apply_if(selected.get(), |s| s.background(catppuccin::SURFACE0))
            .hover(|s| s.background(catppuccin::SURFACE0))
    })
}

/// A small on/off chip. floem has no checkbox in this theme, and a chip that
/// dims when off reads more clearly at this size than a tick box would.
fn toggle_chip(
    signal: RwSignal<bool>,
    label: &'static str,
    extra: impl Fn(floem::style::Style) -> floem::style::Style + 'static,
) -> impl IntoView {
    Label::derived(move || {
        let mark = if signal.get() {
            crate::design::glyph::OK
        } else {
            " "
        };
        format!("{mark} {label}")
    })
    .on_event_stop(floem::event::listener::Click, move |_, _| {
        signal.update(|v| *v = !*v)
    })
    .style(move |s| {
        let on = signal.get();
        extra(s)
            .font_size(10.0)
            .font_family(crate::design::SYMBOL.to_string())
            .padding_horiz(7.0)
            .padding_vert(5.0)
            .border_radius(5.0)
            .cursor(floem::style::CursorStyle::Pointer)
            .border(1.0)
            .border_color(if on {
                catppuccin::LAVENDER
            } else {
                catppuccin::SURFACE0
            })
            .background(if on {
                catppuccin::SURFACE0
            } else {
                catppuccin::CRUST
            })
            .color(if on {
                catppuccin::TEXT
            } else {
                catppuccin::SURFACE2
            })
    })
}

fn note(text: &str, color: Color) -> impl IntoView {
    let text = text.to_string();
    Container::new(
        Label::derived(move || text.clone())
            .style(move |s| s.font_size(10.0).color(color).line_height(1.4)),
    )
    .style(|s| {
        s.width_full()
            .padding(10.0)
            .background(catppuccin::CRUST)
            .border(1.0)
            .border_color(catppuccin::SURFACE0)
            .border_radius(6.0)
    })
}

/// One selectable backend.
fn provider_row(
    state: SettingsState,
    index: usize,
    on_switch: std::rc::Rc<dyn Fn()>,
) -> impl IntoView {
    let (tag, name, blurb) = PROVIDERS[index];
    let selected = move || state.provider.get() == tag;

    Stack::horizontal((
        // A filled dot for the selection, hollow otherwise — the panel already
        // speaks in dots, and radio buttons are not a floem primitive here.
        Label::derived(move || {
            if selected() {
                crate::design::glyph::DOT
            } else {
                crate::design::glyph::RING
            }
            .to_string()
        })
        .style(move |s| {
            s.font_family(crate::design::SYMBOL.to_string())
                .font_size(11.0)
                .margin_right(9.0)
                .color(if selected() {
                    catppuccin::LAVENDER
                } else {
                    catppuccin::SURFACE2
                })
        }),
        Stack::vertical((
            Label::derived(move || name.to_string()).style(move |s| {
                s.font_size(13.0).color(if selected() {
                    catppuccin::TEXT
                } else {
                    catppuccin::SUBTEXT0
                })
            }),
            Label::derived(move || blurb.to_string())
                .style(|s| s.font_size(10.0).color(catppuccin::SURFACE2)),
        ))
        .style(|s| s.gap(1.0)),
    ))
    .on_event_stop(floem::event::listener::Click, move |_, _| {
        if state.provider.get_untracked() == tag {
            return; // already selected; refetching would be a pointless round trip
        }
        state.provider.set(tag.to_string());
        state.status.set(String::new());
        // Free-only means nothing on a local backend, and leaving it on would
        // silently hide nothing while looking like it hides something.
        state.free_only.set(tag == "openrouter");
        state.models.set(Vec::new());
        state.models_error.set(String::new());
        on_switch();
    })
    .style(move |s| {
        s.width_full()
            .items_center()
            .padding_horiz(11.0)
            .padding_vert(9.0)
            .border_radius(6.0)
            .cursor(floem::style::CursorStyle::Pointer)
            .background(if selected() {
                catppuccin::SURFACE0
            } else {
                catppuccin::MANTLE
            })
            .border(1.0)
            .border_color(if selected() {
                catppuccin::LAVENDER
            } else {
                catppuccin::SURFACE0
            })
            .hover(|s| s.background(catppuccin::SURFACE0))
    })
}

fn lmstudio_fields(state: SettingsState) -> impl IntoView {
    Stack::vertical((
        field("Server URL", state.lmstudio_url, "http://localhost:1234/v1"),
        field("Model", state.lmstudio_model, "qwen3.6-27b"),
        hint(
            "The model must already be loaded in LM Studio. Smithy reads its real \
             context window on connect.",
        ),
    ))
    .style(|s| s.width_full().gap(10.0))
}

fn openrouter_fields(
    state: SettingsState,
    on_clear_key: std::rc::Rc<dyn Fn(&str)>,
) -> impl IntoView {
    Stack::vertical((
        field("API base URL", state.openrouter_url, "https://openrouter.ai/api/v1"),
        field("Model", state.openrouter_model, "anthropic/claude-3.5-sonnet"),
        secret_field(
            "API key",
            state.openrouter_key,
            state.openrouter_key_stored,
            "openrouter-api-key",
            on_clear_key,
        ),
    ))
    .style(|s| s.width_full().gap(10.0))
}

fn deepseek_fields(
    state: SettingsState,
    on_clear_key: std::rc::Rc<dyn Fn(&str)>,
) -> impl IntoView {
    Stack::vertical((
        field("API base URL", state.deepseek_url, "https://api.deepseek.com"),
        field("Model", state.deepseek_model, "deepseek-v4-flash"),
        secret_field(
            "API key",
            state.deepseek_key,
            state.deepseek_key_stored,
            "deepseek-api-key",
            on_clear_key,
        ),
        hint(
            "Context windows and prices below are a compiled-in snapshot — DeepSeek's API does \
             not report them, and it has announced peak-hour rates at double the list price. \
             Use them to compare models, not to estimate a bill.",
        ),
    ))
    .style(|s| s.width_full().gap(10.0))
}

fn brave_fields(state: SettingsState, on_clear_key: std::rc::Rc<dyn Fn(&str)>) -> impl IntoView {
    Stack::vertical((
        secret_field(
            "Brave Search API key",
            state.brave_key,
            state.brave_key_stored,
            "brave-api-key",
            on_clear_key,
        ),
        hint("Enables the agent's `web_search` tool. Without it, it can still fetch a URL it is given."),
    ))
    .style(|s| s.width_full().gap(8.0))
}

fn field(label: &'static str, signal: RwSignal<String>, placeholder: &'static str) -> impl IntoView {
    Stack::vertical((
        Label::derived(move || label.to_string())
            .style(|s| s.font_size(11.0).color(catppuccin::SUBTEXT0).margin_bottom(4.0)),
        TextInput::new(signal)
            .placeholder(placeholder)
            .style(text_field_style),
    ))
    .style(|s| s.width_full())
}

/// A field whose stored value is deliberately not shown.
///
/// The placeholder is the whole affordance: it is the only thing that tells you
/// a key exists, and typing over it is the only way to change one.
fn secret_field(
    label: &'static str,
    signal: RwSignal<String>,
    stored: RwSignal<bool>,
    account: &'static str,
    on_clear_key: std::rc::Rc<dyn Fn(&str)>,
) -> impl IntoView {
    Stack::vertical((
        Stack::horizontal((
            Label::derived(move || label.to_string())
                .style(|s| s.font_size(11.0).color(catppuccin::SUBTEXT0)),
            Label::derived(move || {
                if stored.get() {
                    " · saved in your keychain".to_string()
                } else {
                    String::new()
                }
            })
            .style(|s| s.font_size(10.0).color(catppuccin::GREEN)),
            Container::new(Empty::new()).style(|s| s.flex_grow(1.0)),
            Label::derived(|| "Remove".to_string())
                .on_event_stop(floem::event::listener::Click, move |_, _| {
                    on_clear_key(account)
                })
                .style(move |s| {
                    s.font_size(10.0)
                        .color(catppuccin::RED)
                        .cursor(floem::style::CursorStyle::Pointer)
                        .apply_if(!stored.get(), |s| s.display(floem::taffy::Display::None))
                }),
        ))
        .style(|s| s.width_full().items_center().margin_bottom(4.0)),
        TextInput::new(signal)
            .placeholder(if_stored_placeholder(stored))
            .style(text_field_style),
    ))
    .style(|s| s.width_full())
}

/// What an empty secret field says about itself.
///
/// Words rather than a row of bullets, and not only for taste: the glyph guard
/// in [`crate::design`] rejects U+2022 because Menlo — the family every one of
/// these fields is set in — has no glyph for it, so it would have rendered as a
/// line of missing-glyph boxes. Saying it plainly also carries more: a masked
/// string tells you a key exists, this tells you what typing will do to it.
///
/// Evaluated once when the field is built, which is correct here because the
/// dialog's children are rebuilt whenever it opens — and "a key exists" cannot
/// change while it is open except through Remove, which reports through
/// `status`.
fn if_stored_placeholder(stored: RwSignal<bool>) -> &'static str {
    if stored.get_untracked() {
        "A key is saved. Type a new one to replace it."
    } else {
        "Paste your key"
    }
}

fn text_field_style(s: floem::style::Style) -> floem::style::Style {
    s.width_full()
        .padding_horiz(10.0)
        .padding_vert(7.0)
        .font_size(12.0)
        .font_family(crate::design::MONO.to_string())
        .color(catppuccin::TEXT)
        .background(catppuccin::CRUST)
        .border(1.0)
        .border_color(catppuccin::SURFACE0)
        .border_radius(6.0)
        .focus(|s| s.border_color(catppuccin::LAVENDER))
}

fn hint(text: &'static str) -> impl IntoView {
    Label::derived(move || text.to_string()).style(|s| {
        s.font_size(10.0)
            .color(catppuccin::SURFACE2)
            .line_height(1.4)
            .width_full()
    })
}

fn divider() -> impl IntoView {
    Container::new(Empty::new()).style(|s| {
        s.width_full()
            .height(1.0)
            .margin_vert(16.0)
            .background(catppuccin::SURFACE0)
    })
}

/// Shown only when the credential store did not answer.
///
/// Placed above the buttons rather than in `status`, because it is a condition
/// that exists before you press anything — telling you *after* a failed save
/// that keys cannot be saved here is telling you too late.
fn keychain_warning(state: SettingsState) -> impl IntoView {
    Label::derived(|| {
        "Your OS credential store is unavailable, so keys cannot be saved. Smithy will still \
         read one from OPENROUTER_API_KEY."
            .to_string()
    })
    .style(move |s| {
        s.font_size(10.0)
            .color(catppuccin::PEACH)
            .line_height(1.4)
            .width_full()
            .margin_top(12.0)
            .apply_if(state.keychain_available.get(), |s| {
                s.display(floem::taffy::Display::None)
            })
    })
}

fn status_line(state: SettingsState) -> impl IntoView {
    Label::derived(move || state.status.get()).style(move |s| {
        s.font_size(11.0)
            .line_height(1.4)
            .width_full()
            .margin_top(12.0)
            .color(if state.status_is_error.get() {
                catppuccin::RED
            } else {
                catppuccin::GREEN
            })
            .apply_if(state.status.get().is_empty(), |s| {
                s.display(floem::taffy::Display::None)
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tags in [`PROVIDERS`] are the app's translation key into
    /// `smithy_agent::ProviderChoice`. A typo here is a settings dialog that
    /// silently selects the wrong backend, so it is worth pinning.
    #[test]
    fn provider_tags_are_the_serialized_names() {
        assert_eq!(PROVIDERS[0].0, "lmstudio");
        assert_eq!(PROVIDERS[1].0, "openrouter");
    }

    #[test]
    fn every_provider_has_a_name_and_a_blurb() {
        for (tag, name, blurb) in PROVIDERS {
            assert!(!tag.is_empty());
            assert!(!name.is_empty());
            assert!(!blurb.is_empty(), "{tag} needs a description");
        }
    }

    fn row(id: &str, tool_capable: bool, free: bool, local: bool) -> ModelRow {
        ModelRow {
            id: id.to_string(),
            label: format!("Label for {id}"),
            context: "128k ctx".into(),
            badge: if free { "free".into() } else { "$1.00".into() },
            tool_capable,
            free,
            local,
            loaded: false,
        }
    }

    #[test]
    fn an_empty_query_matches_everything() {
        let r = row("anything", true, true, false);
        assert!(matches_query(&r, ""));
        assert!(matches_query(&r, "   "));
    }

    /// Searching the way people actually type: words they remember, not the
    /// hyphens and colons the id happens to contain.
    #[test]
    fn every_term_must_match_somewhere_in_id_or_label() {
        let r = row("google/gemma-4-26b-a4b-it:free", true, true, false);
        assert!(matches_query(&r, "gemma 26"));
        assert!(matches_query(&r, "GOOGLE"));
        assert!(matches_query(&r, "label"), "the label counts too");
        assert!(!matches_query(&r, "gemma llama"), "all terms must match");
    }

    /// The filter that stops you selecting a model the agent cannot drive.
    /// Defaults on, so this is the behaviour most people will actually see.
    #[test]
    fn the_tool_filter_hides_models_that_cannot_call_tools() {
        let state = SettingsState::new();
        state.models.set(vec![
            row("chat", true, true, false),
            row("tts", false, true, false),
        ]);
        state.free_only.set(false);

        state.tools_only.set(true);
        let ids: Vec<String> = state.visible_models().iter().map(|m| m.id.clone()).collect();
        assert_eq!(ids, vec!["chat"]);

        state.tools_only.set(false);
        assert_eq!(state.visible_models().len(), 2);
    }

    #[test]
    fn the_free_filter_hides_metered_models() {
        let state = SettingsState::new();
        state.models.set(vec![
            row("free-one", true, true, false),
            row("paid-one", true, false, false),
        ]);
        state.free_only.set(true);
        let ids: Vec<String> = state.visible_models().iter().map(|m| m.id.clone()).collect();
        assert_eq!(ids, vec!["free-one"]);
    }

    /// A local model has no price, so "free only" must not hide the entire
    /// LM Studio library — which is what a naive `free` check would do.
    #[test]
    fn the_free_filter_never_hides_local_models() {
        let state = SettingsState::new();
        state.models.set(vec![row("local-model", true, false, true)]);
        state.free_only.set(true);
        assert_eq!(state.visible_models().len(), 1, "local models are not metered");
    }

    #[test]
    fn filters_and_search_compose() {
        let state = SettingsState::new();
        state.models.set(vec![
            row("nvidia/nemotron-free", true, true, false),
            row("nvidia/nemotron-paid", true, false, false),
            row("nvidia/nemotron-tts", false, true, false),
            row("google/gemma-free", true, true, false),
        ]);
        state.tools_only.set(true);
        state.free_only.set(true);
        state.model_query.set("nemotron".into());
        let ids: Vec<String> = state.visible_models().iter().map(|m| m.id.clone()).collect();
        assert_eq!(ids, vec!["nvidia/nemotron-free"]);
    }

    /// Choosing writes into whichever backend's field is live, or picking a
    /// model under one provider would silently reconfigure the other.
    #[test]
    fn choosing_a_model_writes_to_the_selected_backends_field() {
        let state = SettingsState::new();

        state.provider.set("openrouter".into());
        state.choose_model("google/gemma-4-31b-it:free");
        assert_eq!(state.openrouter_model.get_untracked(), "google/gemma-4-31b-it:free");
        assert_eq!(state.lmstudio_model.get_untracked(), "", "untouched");

        state.provider.set("lmstudio".into());
        state.choose_model("qwen3.6-27b");
        assert_eq!(state.lmstudio_model.get_untracked(), "qwen3.6-27b");
        assert_eq!(
            state.openrouter_model.get_untracked(),
            "google/gemma-4-31b-it:free",
            "the other backend keeps its model"
        );
    }
}
