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
use floem::reactive::{RwSignal, SignalGet, SignalUpdate};

use crate::theme::catppuccin;

/// Which backends the dialog offers, in the order it offers them.
///
/// Mirrors `smithy_agent::ProviderChoice` by value rather than by type, for the
/// reason in the module docs. The tags are that enum's serialized form, so the
/// app's translation is a string match and not a lookup table.
pub const PROVIDERS: [(&str, &str, &str); 2] = [
    (
        "lmstudio",
        "LM Studio",
        "A local OpenAI-compatible server. No key needed.",
    ),
    (
        "openrouter",
        "OpenRouter",
        "Hosted models, billed to your OpenRouter account.",
    ),
];

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
    /// A *newly typed* key. Empty means "leave the stored one alone" — see the
    /// module docs on why this is never seeded from storage.
    pub openrouter_key: RwSignal<String>,
    pub brave_key: RwSignal<String>,
    /// Whether a key is already in the credential store. Drives the placeholder
    /// and whether "Remove" is offered, without revealing the value.
    pub openrouter_key_stored: RwSignal<bool>,
    pub brave_key_stored: RwSignal<bool>,
    /// Whether the OS credential store answered at all. A machine where it did
    /// not must say so *before* you type a key into a field that cannot save it.
    pub keychain_available: RwSignal<bool>,
    /// The line under the buttons: what just happened, or what is wrong.
    pub status: RwSignal<String>,
    /// Whether `status` is a failure. Colour only; the text carries the meaning.
    pub status_is_error: RwSignal<bool>,
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
            openrouter_key: RwSignal::new(String::new()),
            brave_key: RwSignal::new(String::new()),
            openrouter_key_stored: RwSignal::new(false),
            brave_key_stored: RwSignal::new(false),
            keychain_available: RwSignal::new(true),
            status: RwSignal::new(String::new()),
            status_is_error: RwSignal::new(false),
        }
    }

    pub fn selected(&self) -> String {
        self.provider.get()
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

/// The dialog. Occupies no space and paints nothing while closed.
///
/// `on_save` receives nothing: the state is a set of `Copy` signals the caller
/// already holds, so it reads the fields itself. That keeps the signature stable
/// as fields are added, which for a settings form is the only certainty.
pub fn settings_modal(
    state: SettingsState,
    on_save: impl Fn() + 'static,
    on_clear_key: impl Fn(&str) + 'static,
) -> impl IntoView {
    let on_save = std::rc::Rc::new(on_save);
    let on_clear_key = std::rc::Rc::new(on_clear_key);

    Container::new(dyn_container(
        move || state.open.get(),
        move |open| {
            if !open {
                return Empty::new()
                    .style(|s| s.display(floem::taffy::Display::None))
                    .into_any();
            }
            let on_save = on_save.clone();
            let on_clear_key = on_clear_key.clone();
            panel(state, on_save, on_clear_key).into_any()
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

fn panel(
    state: SettingsState,
    on_save: std::rc::Rc<dyn Fn()>,
    on_clear_key: std::rc::Rc<dyn Fn(&str)>,
) -> impl IntoView {
    let save = on_save.clone();
    let clear_for_provider = on_clear_key.clone();

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
        // The choice itself.
        Stack::vertical((
            provider_row(state, 0),
            provider_row(state, 1),
        ))
        .style(|s| s.width_full().gap(6.0).margin_bottom(14.0)),
        // Only the selected backend's fields, because a form showing settings
        // that do nothing invites you to edit them and wonder why nothing
        // changed.
        dyn_container(
            move || state.selected(),
            move |choice| match choice.as_str() {
                "openrouter" => openrouter_fields(state, clear_for_provider.clone()).into_any(),
                _ => lmstudio_fields(state).into_any(),
            },
        ),
        // Search is provider-independent, so it sits outside the switch.
        divider(),
        brave_fields(state, on_clear_key.clone()),
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
        s.width(560.0)
            .padding(24.0)
            .background(catppuccin::BASE)
            .border(1.0)
            .border_color(catppuccin::SURFACE1)
            .border_radius(8.0)
    })
}

/// One selectable backend.
fn provider_row(state: SettingsState, index: usize) -> impl IntoView {
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
        state.provider.set(tag.to_string());
        state.status.set(String::new());
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
}
