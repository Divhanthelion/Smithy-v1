//! The agent panel — the surface you actually look at.
//!
//! Replaces forge's generic chat panel. The difference in intent: a chat panel
//! shows a conversation, whereas an agent panel has to show *work happening* —
//! which tool is running, what it touched, how much budget is left, and whether
//! the model is thinking or answering. Those are different things to render.
//!
//! Design rules this follows:
//!
//! - **Tool steps are one line each until you need more.** An agent turn can be
//!   twenty tool calls; rendering each as a paragraph buries the answer. Each
//!   step is a single row — glyph, name, one-line argument summary — and its
//!   output only appears when it failed or when you expand it. Three or more
//!   consecutive steps collapse into one expandable batch so the answer is not
//!   pushed off the screen.
//! - **Reasoning is visually subordinate to the answer.** It's dimmed and
//!   italicised, because it is context for the answer rather than the answer.
//! - **The budget is always visible.** Prefill cost grows superlinearly with
//!   context, so knowing you are at 40k rather than 4k explains why a turn got
//!   slow. A thin bar costs almost no space and answers that question.

use std::collections::HashMap;
use std::time::Duration;

use floem::peniko::kurbo::Point;
use floem::peniko::Color;
use floem::prelude::*;
use floem::reactive::{Effect, Memo, RwSignal, SignalGet, SignalUpdate};
use floem::style::CustomStylable;
use floem::text::{Attrs, AttrsList, FamilyOwned, FontStyle, FontWeight, LineHeightValue};

use crate::markdown::{self, Block, Inline};
use crate::theme::catppuccin;

/// Consecutive Steps at or above this count share one expandable row.
const BATCH_MIN: usize = 3;
const MONO_FAM: [FamilyOwned; 1] = [FamilyOwned::Monospace];
/// Far enough that [`Scroll::scroll_to`] clamps to the live edge.
const SCROLL_BOTTOM: Point = Point::new(0.0, 1_000_000.0);

/// One entry in the transcript.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    User(String),
    /// A final answer from the model.
    Answer(String),
    /// One tool call and how it resolved.
    Step {
        /// The `tool_call_id`. Rows are matched on this, never on the tool name.
        id: String,
        step: usize,
        name: String,
        summary: String,
        status: StepStatus,
        detail: String,
    },
    /// A budget warning or a retry notice.
    Notice(String),
    /// The turn ended early.
    Stopped(String),
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStatus {
    Running,
    Ok,
    Failed,
}

impl StepStatus {
    fn glyph(self) -> &'static str {
        match self {
            StepStatus::Running => "○",
            StepStatus::Ok => "●",
            StepStatus::Failed => "✕",
        }
    }

    fn color(self) -> Color {
        match self {
            StepStatus::Running => catppuccin::YELLOW,
            StepStatus::Ok => catppuccin::GREEN,
            StepStatus::Failed => catppuccin::RED,
        }
    }
}

/// One row of the context-usage breakdown, already attributed.
#[derive(Debug, Clone)]
pub struct ContextUsageRow {
    pub name: String,
    pub tokens: i64,
    pub frozen: bool,
}

/// Snapshot of context attribution, computed once per completion on the
/// agent side and only *read* while painting.
#[derive(Debug, Clone, Default)]
pub struct ContextUsageSnapshot {
    pub rows: Vec<ContextUsageRow>,
    pub prompt_tokens: i64,
    pub cached_tokens: i64,
    pub cold_tokens: i64,
    pub reasoning_tokens: i64,
    pub hit_rate: Option<f64>,
}

impl ContextUsageSnapshot {
    pub fn from_ledger(
        rows: &[ContextUsageRow],
        prompt_tokens: i64,
        cached_tokens: i64,
        cold_tokens: i64,
        reasoning_tokens: i64,
        hit_rate: Option<f64>,
    ) -> Self {
        Self {
            rows: rows.to_vec(),
            prompt_tokens,
            cached_tokens,
            cold_tokens,
            reasoning_tokens,
            hit_rate,
        }
    }
}

/// A row in the `/` picker. Kept here so the editor crate does not depend on
/// the agent crate; the app maps `SkillMeta` onto this.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillPick {
    pub name: String,
    pub description: String,
    pub argument_hint: String,
}

/// Everything the panel renders from.
#[derive(Clone, Copy)]
pub struct AgentPanelState {
    pub entries: RwSignal<Vec<Entry>>,
    pub input: RwSignal<String>,
    /// What the microphone is doing. Owned here because the button that shows
    /// it and the box the words land in are the same panel.
    pub voice: RwSignal<smithy_voice::Voice>,
    pub busy: RwSignal<bool>,
    /// Answer text accumulating live during a turn.
    pub streaming_answer: RwSignal<String>,
    /// Reasoning accumulating live. Never enters history.
    pub streaming_reasoning: RwSignal<String>,
    /// Prompt tokens reported by the last completion.
    pub context_tokens: RwSignal<i64>,
    /// Hard ceiling, derived from the loaded model's context window.
    pub context_limit: RwSignal<i64>,
    pub model_label: RwSignal<String>,
    /// Why the last turn ended early, pinned next to the budget bar so a
    /// ceiling (`time limit reached (3600s)` and friends) is not buried in a
    /// long transcript. Cleared when the next turn starts.
    pub last_stop: RwSignal<Option<String>>,
    /// Coding / Research / Grill — shown in the header.
    pub session_kind: RwSignal<String>,
    /// Segment text for inspection, stashed with the ledger (not rebuilt on paint).
    pub inspect_segments: RwSignal<Vec<(String, String)>>,
    /// Pretty-printed last provider request body.
    pub last_request: RwSignal<Option<String>>,
    /// Skills for the `/` picker.
    pub skills: RwSignal<Vec<SkillPick>>,
    /// Highlighted row in the `/` picker. Tab inserts it; arrows move it.
    pub skill_cursor: RwSignal<usize>,
    /// What the model was told about the project, e.g.
    /// "layout, dependencies, modules, public API · ~6000 tokens".
    pub context_label: RwSignal<String>,
    /// Attribution for the last prompt — stashed on completion, never rebuilt
    /// on the paint path (serializing tools inside `Label::derived` would be
    /// the CallGraph::staleness landmine at 60 Hz).
    pub context_usage: RwSignal<Option<ContextUsageSnapshot>>,
    /// Whether the endpoint preflighted successfully.
    pub connected: RwSignal<bool>,
    /// Which step indices are expanded.
    pub expanded: RwSignal<Vec<usize>>,
    /// Explicit open/closed for a Step batch, keyed by the first Step's index.
    /// Missing means the default: open while any Step is running or failed,
    /// closed once they have all succeeded.
    pub batch_open: RwSignal<HashMap<usize, bool>>,
    /// Stick the transcript to the live edge. Set when a Turn starts; cleared
    /// when the user scrolls the transcript themselves.
    pub follow_scroll: RwSignal<bool>,
    /// Files the user dropped, waiting to go out with the next message.
    ///
    /// Cleared on send rather than accumulating: an attachment is part of one
    /// message, and a list that persisted would quietly re-send every file on
    /// every turn — which at four kilobytes a token is the most expensive
    /// possible way to be surprised.
    pub attachments: RwSignal<Vec<crate::attachment::Attachment>>,
    /// Whether a drag is currently over the panel. Drives the drop outline.
    pub drop_active: RwSignal<bool>,
    /// Whether file edits skip the review modal and land directly.
    ///
    /// Mirrored by the app into an `AtomicBool` the write hook reads, because
    /// the hook runs on the tokio side where floem signals cannot be touched.
    pub auto_approve: RwSignal<bool>,
    /// The project root, for naming attachments relative to it.
    ///
    /// Lives here because the panel is the only thing that needs it and it
    /// changes when the project does — a signal rather than a constructor
    /// argument so switching project re-labels the chips without rebuilding the
    /// panel.
    pub project_root: RwSignal<std::path::PathBuf>,
}

impl AgentPanelState {
    pub fn new() -> Self {
        Self {
            entries: RwSignal::new(Vec::new()),
            input: RwSignal::new(String::new()),
            voice: RwSignal::new(smithy_voice::Voice::Cold),
            busy: RwSignal::new(false),
            streaming_answer: RwSignal::new(String::new()),
            streaming_reasoning: RwSignal::new(String::new()),
            context_tokens: RwSignal::new(0),
            context_limit: RwSignal::new(110_000),
            model_label: RwSignal::new("connecting…".to_string()),
            last_stop: RwSignal::new(None),
            session_kind: RwSignal::new("Coding".into()),
            inspect_segments: RwSignal::new(Vec::new()),
            last_request: RwSignal::new(None),
            skills: RwSignal::new(Vec::new()),
            skill_cursor: RwSignal::new(0),
            context_label: RwSignal::new(String::new()),
            context_usage: RwSignal::new(None),
            connected: RwSignal::new(false),
            expanded: RwSignal::new(Vec::new()),
            batch_open: RwSignal::new(HashMap::new()),
            follow_scroll: RwSignal::new(true),
            attachments: RwSignal::new(Vec::new()),
            drop_active: RwSignal::new(false),
            auto_approve: RwSignal::new(false),
            project_root: RwSignal::new(std::path::PathBuf::new()),
        }
    }

    /// Attach dropped paths, skipping anything already listed.
    pub fn attach(&self, paths: &[std::path::PathBuf]) {
        let root = self.project_root.get_untracked();
        let existing = self.attachments.get_untracked();
        let added = crate::attachment::collect(paths, &root, &existing);
        if added.is_empty() {
            return;
        }
        self.attachments.update(|list| list.extend(added));
    }

    pub fn remove_attachment(&self, path: &std::path::Path) {
        self.attachments
            .update(|list| list.retain(|a| a.path != path));
    }

    pub fn toggle_attachment(&self, path: &std::path::Path) {
        self.attachments.update(|list| {
            if let Some(a) = list.iter_mut().find(|a| a.path == path) {
                a.included = !a.included;
            }
        });
    }

    pub fn clear_attachments(&self) {
        self.attachments.update(|list| list.clear());
    }

    pub fn push(&self, entry: Entry) {
        self.entries.update(|e| e.push(entry));
    }

    /// Mark a step resolved, matched by its `tool_call_id`.
    ///
    /// Matching by tool *name* was wrong: two parallel calls to the same tool
    /// produce two rows with the same name, and results arrive in completion
    /// order rather than call order. Searching for "the last running `read`"
    /// therefore attached the first result to the second row, silently swapping
    /// the two outputs on screen. The id is unique per call, so it cannot.
    pub fn resolve_step(&self, id: &str, detail: String, is_error: bool) {
        self.entries.update(|entries| {
            for entry in entries.iter_mut() {
                if let Entry::Step {
                    id: entry_id,
                    status,
                    detail: d,
                    ..
                } = entry
                {
                    if entry_id == id {
                        *status = if is_error {
                            StepStatus::Failed
                        } else {
                            StepStatus::Ok
                        };
                        *d = detail;
                        return;
                    }
                }
            }
        });
    }

    pub fn clear(&self) {
        self.entries.update(|e| e.clear());
        self.streaming_answer.set(String::new());
        self.streaming_reasoning.set(String::new());
        self.context_tokens.set(0);
        self.context_usage.set(None);
        self.last_stop.set(None);
        self.inspect_segments.set(Vec::new());
        self.last_request.set(None);
        self.expanded.set(Vec::new());
        self.batch_open.set(HashMap::new());
        self.follow_scroll.set(true);
    }
}

impl Default for AgentPanelState {
    fn default() -> Self {
        Self::new()
    }
}

/// Shorten a JSON argument blob to something that fits one line.
///
/// Prefers the value of the argument that identifies the target — a `read` of
/// `src/main.rs` should read as `src/main.rs`, not `{"path":"src/main.rs"}`.
pub fn summarize_arguments(arguments: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return one_line(arguments, 60);
    };
    let Some(obj) = value.as_object() else {
        return one_line(arguments, 60);
    };
    if obj.is_empty() {
        return String::new();
    }

    for key in ["path", "command", "pattern", "url", "file_path"] {
        if let Some(v) = obj.get(key).and_then(|v| v.as_str()) {
            return one_line(v, 60);
        }
    }
    let joined = obj
        .iter()
        .map(|(k, v)| match v.as_str() {
            Some(s) => format!("{k}={}", one_line(s, 24)),
            None => format!("{k}={v}"),
        })
        .collect::<Vec<_>>()
        .join(" ");
    one_line(&joined, 60)
}

fn one_line(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        flat.chars().take(max.saturating_sub(1)).collect::<String>() + "…"
    }
}

/// Format a token count compactly: `1234` → `1.2k`.
pub fn format_tokens(n: i64) -> String {
    if n < 1000 {
        n.to_string()
    } else if n < 100_000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        format!("{}k", n / 1000)
    }
}

/// Fraction of the context budget consumed, clamped to 0.0–1.0.
pub fn context_fraction(used: i64, limit: i64) -> f64 {
    if limit <= 0 {
        return 0.0;
    }
    (used as f64 / limit as f64).clamp(0.0, 1.0)
}

/// Colour for the budget bar: green until it matters, then amber, then red.
pub fn budget_color(fraction: f64) -> Color {
    if fraction < 0.5 {
        catppuccin::GREEN
    } else if fraction < 0.8 {
        catppuccin::YELLOW
    } else {
        catppuccin::RED
    }
}

// ============================================================================
// Views
// ============================================================================

#[allow(clippy::too_many_arguments)]
pub fn agent_panel(
    state: AgentPanelState,
    on_send: std::rc::Rc<dyn Fn(String)>,
    on_stop: std::rc::Rc<dyn Fn()>,
    on_close: impl Fn() + 'static,
    on_voice: impl Fn() + 'static,
    // Try the endpoint again. Shown only while disconnected — see `header`.
    on_reconnect: impl Fn() + 'static,
    // Open the backend settings dialog.
    on_settings: impl Fn() + 'static,
    // Forget the conversation — the model's history, not just this view of it.
    on_clear_context: impl Fn() + 'static,
    // Open a ledger segment or the last request as a scratch buffer.
    on_inspect: std::rc::Rc<dyn Fn(String, String)>,
    // How the microphone's shortcut is written, for the hint beside it.
    hotkey: String,
) -> impl IntoView {
    // Re-pin when a Turn starts. The user taking over the scrollbar is the
    // only thing that unpins, and that must not survive into the next Turn.
    Effect::new(move |_| {
        if state.busy.get() {
            state.follow_scroll.set(true);
        }
    });

    // The whole panel is the drop target, not just the composer. Aiming at a
    // text field while holding a drag is fiddly, and there is nothing else you
    // could mean by dropping a file on the agent.
    let transcript_scroll = floem::views::scroll::Scroll::new(
        Stack::vertical((transcript(state), live_activity(state)))
            .style(|s| s.width_full().min_width(0.0).padding(10.0).gap(2.0)),
    )
    .scroll_to(move || {
        let following = state.follow_scroll.get();
        let _ = state.entries.get();
        let _ = state.streaming_answer.get();
        let _ = state.streaming_reasoning.get();
        if following {
            Some(SCROLL_BOTTOM)
        } else {
            None
        }
    })
    .on_event(floem::event::listener::PointerWheel, move |_, _| {
        state.follow_scroll.set(false);
        floem::event::EventPropagation::Continue
    })
    .custom_style(|s: floem::views::scroll::ScrollCustomStyle| {
        s.hide_bars(false)
            .handle_background(catppuccin::SURFACE1)
            .handle_border_radius(4.0)
    })
    .style(|s| {
        s.flex_grow(1.0)
            .flex_basis(0.0)
            .width_full()
            .min_height(0.0)
            // Without this the transcript is as wide as its widest line
            // and the panel stretches to match — see the note in
            // `main_layout`'s chat container.
            .min_width(0.0)
    });

    let panel = Stack::vertical((
        header(state, on_close, on_reconnect, on_settings, on_clear_context),
        transcript_scroll,
        budget_bar(state, on_inspect),
        attachment_row(state),
        composer(state, on_send, on_stop, on_voice, hotkey),
    ))
    .style(|s| {
        s.width_full()
            .height_full()
            .min_width(0.0)
            // **This** is what stops the transcript running off the right edge.
            // floem's `text_overflow` defaults to `NoWrap(Clip)`: text does not
            // wrap, it is cut off at the boundary. No amount of width or
            // `min_width` on the containers changes that — the earlier attempt to
            // fix this by adding `min_width(0.0)` alone did nothing, because the
            // label was never going to wrap in the first place.
            //
            // The property is `inherited`, so setting it once here covers every
            // label in the panel: answers, tool summaries, notices and the
            // reasoning trace.
            //
            // `BreakWord` rather than `Normal`: model output is full of URLs and
            // absolute paths with no spaces in them, and `Normal` leaves those
            // overflowing exactly as before. Rather than `Anywhere` because that
            // also drags the minimum content width down, which changes how the
            // panel negotiates its size with the editor beside it.
            .text_overflow(floem::style::TextOverflow::Wrap {
                overflow_wrap: floem::text::OverflowWrap::BreakWord,
                word_break: Default::default(),
            })
            .background(catppuccin::MANTLE)
            .border_left(1.0)
            .border_color(catppuccin::SURFACE0)
    });

    // Enter/Leave only toggle the outline; Drop is what reads the paths. Leave
    // has to clear the flag on *both* a real exit and a completed drop, because
    // the platform does not always send a Leave after a Drop — an outline that
    // stayed lit over a panel with nothing being dragged would be worse than no
    // outline at all.
    //
    // A macOS screenshot thumbnail often fires Enter and then nothing else —
    // no Leave, no Drop — so the outline would stick forever. A short timer
    // and a click both clear it; a real drag that lasts longer still drops,
    // because drop_active is only the outline.
    let drop_epoch = RwSignal::new(0u64);
    let arm_drop_timeout = move || {
        let epoch = drop_epoch.get_untracked() + 1;
        drop_epoch.set(epoch);
        floem::action::exec_after(Duration::from_millis(1500), move |_| {
            if drop_epoch.get_untracked() == epoch {
                state.drop_active.set(false);
            }
        });
    };
    let cancel_drop = move || {
        drop_epoch.update(|e| *e = e.wrapping_add(1));
        state.drop_active.set(false);
    };

    Stack::new((
        panel.style(|s| s.width_full().height_full().min_width(0.0)),
        drop_overlay(state),
    ))
    .on_event_stop(floem::event::listener::FileDragEnter, move |_, _| {
        state.drop_active.set(true);
        arm_drop_timeout();
    })
    .on_event_stop(floem::event::listener::FileDragLeave, move |_, _| {
        cancel_drop();
    })
    .on_event_stop(floem::event::listener::FileDragDrop, move |_, event| {
        cancel_drop();
        state.attach(&event.paths);
    })
    .on_event(floem::event::listener::PointerDown, move |_, _| {
        if state.drop_active.get_untracked() {
            cancel_drop();
        }
        floem::event::EventPropagation::Continue
    })
    .on_event(floem::event::listener::KeyDown, move |_, ev| {
        if state.drop_active.get_untracked()
            && ev.key == floem::prelude::Key::Named(floem::prelude::NamedKey::Escape)
        {
            cancel_drop();
            floem::event::EventPropagation::Stop
        } else {
            floem::event::EventPropagation::Continue
        }
    })
    .style(|s| s.width_full().height_full().min_width(0.0))
}

/// The panel's title row: what it is, whether it is connected, and to what.
///
/// **Reconnect is offered here, and only while disconnected.** Starting Smithy
/// before the model server is up used to leave the agent permanently dead: the
/// connection is attempted once at launch and once per project switch, so the
/// only way to pick up a model you started afterwards was to restart the editor
/// or open a different project and come back.
///
/// Hidden once connected rather than merely disabled, because reconnecting a
/// live session is not a no-op — it rebuilds the session from the store and
/// replays the transcript, which is the right thing after a failure and a
/// baffling thing to have happen mid-conversation.
fn header(
    state: AgentPanelState,
    on_close: impl Fn() + 'static,
    on_reconnect: impl Fn() + 'static,
    on_settings: impl Fn() + 'static,
    on_clear_context: impl Fn() + 'static,
) -> impl IntoView {
    Stack::horizontal((
        Label::derived(|| "Agent".to_string()).style(|s| {
            nowrap(s)
                .color(catppuccin::TEXT)
                .font_size(13.0)
                .font_bold()
                .margin_right(8.0)
        }),
        Label::derived(move || state.session_kind.get()).style(|s| {
            nowrap(s)
                .color(catppuccin::LAVENDER)
                .font_size(11.0)
                .margin_right(8.0)
        }),
        // Connection dot: green when the endpoint preflighted, red when not.
        Label::derived(|| "●".to_string()).style(move |s| {
            s.font_size(9.0)
                .font_family(crate::design::SYMBOL.to_string())
                .color(if state.connected.get() {
                    catppuccin::GREEN
                } else {
                    catppuccin::RED
                })
                .margin_right(5.0)
        }),
        Label::derived(move || state.model_label.get()).style(|s| {
            nowrap(s)
                .color(catppuccin::OVERLAY1)
                .font_size(11.0)
                .min_width(0.0)
        }),
        // Plain text, not a glyph: this appears exactly when something is
        // already wrong, which is the worst moment to discover a missing-glyph
        // box. See `design::glyph` on why that is a live hazard here.
        Label::derived(|| "Reconnect".to_string())
            .on_event_stop(floem::event::listener::Click, move |_, _| on_reconnect())
            .style(move |s| {
                nowrap(s)
                    .color(catppuccin::BLUE)
                    .font_size(11.0)
                    .margin_left(8.0)
                    .padding_horiz(6.0)
                    .padding_vert(1.0)
                    .border_radius(3.0)
                    .background(catppuccin::SURFACE0)
                    .cursor(floem::style::CursorStyle::Pointer)
                    .apply_if(state.connected.get(), |s| {
                        s.display(floem::taffy::Display::None)
                    })
            }),
        Container::new(Empty::new()).style(|s| s.flex_grow(1.0)),
        // What the model knows about the project before you say anything.
        // Worth showing: a silently-degraded context (say, layout only because
        // `cargo metadata` failed) explains otherwise baffling answers.
        Label::derived(move || state.context_label.get()).style(move |s| {
            nowrap(s)
                .color(catppuccin::SURFACE2)
                .font_size(10.0)
                .margin_right(8.0)
                .apply_if(state.context_label.get().is_empty(), |s| {
                    s.display(floem::taffy::Display::None)
                })
        }),
        // Words, not a glyph, and next to the context readout rather than in
        // the icon row. Two reasons. The icon row carries no tooltips — see
        // `icon_button` — so a third pictogram beside "clear the transcript"
        // would be indistinguishable from it at exactly the moment the
        // difference matters. And the difference *is* the point: one empties
        // what you can see, this one empties what the model remembers.
        //
        // Always available, never hidden behind a full context bar. A session
        // restored at launch reports zero tokens until its first turn while
        // remembering everything, which is precisely when you want this.
        // Off by default. YOLO skips Review for in-Project writes and skips the
        // shell prompt for `bash` that stays down in the Project. A command that
        // goes up or over still asks. The label says which state you are in,
        // not which state a click would produce.
        Label::derived(move || {
            if state.auto_approve.get() {
                format!("{} YOLO", crate::design::glyph::WARN)
            } else {
                format!("{} reviewed", crate::design::glyph::OK)
            }
        })
        .on_event_stop(floem::event::listener::Click, move |_, _| {
            state.auto_approve.update(|v| *v = !*v)
        })
        .style(move |s| {
            let on = state.auto_approve.get();
            s.font_family(crate::design::SYMBOL.to_string())
                .font_size(10.0)
                .margin_right(6.0)
                .padding_horiz(6.0)
                .padding_vert(2.0)
                .border_radius(3.0)
                .cursor(floem::style::CursorStyle::Pointer)
                .color(if on {
                    catppuccin::PEACH
                } else {
                    catppuccin::SURFACE2
                })
                .hover(|s| s.background(catppuccin::SURFACE0))
        }),
        Label::derived(|| "New session".to_string())
            .on_event_stop(floem::event::listener::Click, move |_, _| {
                on_clear_context()
            })
            .style(move |s| {
                nowrap(s)
                    .color(catppuccin::OVERLAY1)
                    .font_size(11.0)
                    .margin_right(4.0)
                    .padding_horiz(6.0)
                    .padding_vert(1.0)
                    .border_radius(3.0)
                    .cursor(floem::style::CursorStyle::Pointer)
                    .hover(|s| s.background(catppuccin::SURFACE0).color(catppuccin::TEXT))
            }),
        icon_button(
            crate::design::glyph::CONFIG,
            "Choose the model and endpoint",
            on_settings,
        ),
        icon_button(
            crate::design::glyph::CLEAR,
            "Clear the transcript",
            move || state.clear(),
        ),
        icon_button(crate::design::glyph::CLOSE, "Hide the panel", on_close),
    ))
    .style(|s| {
        s.width_full()
            .items_center()
            .padding_horiz(12.0)
            .padding_vert(9.0)
            .background(catppuccin::CRUST)
            .border_bottom(1.0)
            .border_color(catppuccin::SURFACE0)
    })
}

/// Stop a label breaking mid-word, and clip it if it still does not fit.
///
/// The panel root sets `TextOverflow::Wrap { BreakWord }` and — as the comment
/// there says — the property is **inherited by every label in the panel**. That
/// is exactly right for prose: model output is full of URLs and absolute paths
/// with no spaces, and without it they run off the edge.
///
/// It is exactly wrong for chrome. `BreakWord` will break *inside* a word when
/// the box is narrow, so at a 350px panel the header rendered the title as
/// "A ge nt", the button as "New sessi on", and every tool row as "bas h". Chrome
/// is fixed, known-length text that should shrink by clipping, never by being
/// torn into syllables.
fn nowrap(s: floem::style::Style) -> floem::style::Style {
    s.text_overflow(floem::style::TextOverflow::NoWrap(
        floem::style::NoWrapOverflow::Ellipsis,
    ))
}

fn icon_button(
    glyph: &'static str,
    tip: &'static str,
    on_click: impl Fn() + 'static,
) -> impl IntoView {
    Label::derived(move || glyph.to_string())
        .on_event_stop(floem::event::listener::Click, move |_, _| on_click())
        .style(|s| {
            s.color(catppuccin::OVERLAY1)
                .font_family(crate::design::SYMBOL.to_string())
                .font_size(12.0)
                .padding_horiz(6.0)
                .padding_vert(2.0)
                .border_radius(4.0)
                .hover(|s| s.background(catppuccin::SURFACE0).color(catppuccin::TEXT))
        })
        .style(|s| s.keyboard_navigable())
        .style(move |s| s.apply_if(tip.is_empty(), |s| s))
}

#[derive(Clone, Debug)]
enum TranscriptRow {
    One {
        index: usize,
        entry: Entry,
    },
    Batch {
        start: usize,
        steps: Vec<BatchedStep>,
    },
}

#[derive(Clone, Debug)]
struct BatchedStep {
    index: usize,
    step: usize,
    name: String,
    summary: String,
    status: StepStatus,
    detail: String,
}

fn batched_step(index: usize, entry: &Entry) -> Option<BatchedStep> {
    match entry {
        Entry::Step {
            step,
            name,
            summary,
            status,
            detail,
            ..
        } => Some(BatchedStep {
            index,
            step: *step,
            name: name.clone(),
            summary: summary.clone(),
            status: *status,
            detail: detail.clone(),
        }),
        _ => None,
    }
}

fn group_entries(entries: &[Entry]) -> Vec<TranscriptRow> {
    let mut rows = Vec::new();
    let mut i = 0;
    while i < entries.len() {
        if matches!(entries[i], Entry::Step { .. }) {
            let start = i;
            while i < entries.len() && matches!(entries[i], Entry::Step { .. }) {
                i += 1;
            }
            if i - start >= BATCH_MIN {
                let steps = entries[start..i]
                    .iter()
                    .enumerate()
                    .filter_map(|(offset, entry)| batched_step(start + offset, entry))
                    .collect();
                rows.push(TranscriptRow::Batch { start, steps });
            } else {
                for (index, entry) in entries.iter().enumerate().take(i).skip(start) {
                    rows.push(TranscriptRow::One {
                        index,
                        entry: entry.clone(),
                    });
                }
            }
        } else {
            rows.push(TranscriptRow::One {
                index: i,
                entry: entries[i].clone(),
            });
            i += 1;
        }
    }
    rows
}

fn row_key(row: &TranscriptRow) -> String {
    match row {
        TranscriptRow::One { index, entry } => match entry {
            Entry::Step {
                id, status, detail, ..
            } => format!("s{index}:{id}:{status:?}:{}", detail.len()),
            _ => format!("e{index}"),
        },
        TranscriptRow::Batch { start, steps } => {
            let fp: String = steps
                .iter()
                .map(|s| format!("{}{:?}{}", s.index, s.status, s.detail.len()))
                .collect();
            format!("b{start}:{}:{fp}", steps.len())
        }
    }
}

fn transcript(state: AgentPanelState) -> impl IntoView {
    dyn_stack(
        move || group_entries(&state.entries.get()).into_iter(),
        row_key,
        move |row| row_view(state, row),
    )
    .style(|s| s.flex_col().width_full().min_width(0.0))
}

fn row_view(state: AgentPanelState, row: TranscriptRow) -> impl IntoView {
    match row {
        TranscriptRow::One { index, entry } => entry_view(state, index, entry).into_any(),
        TranscriptRow::Batch { start, steps } => batch_row(state, start, steps).into_any(),
    }
}

fn entry_view(state: AgentPanelState, index: usize, entry: Entry) -> impl IntoView {
    match entry {
        Entry::User(text) => user_bubble(text).into_any(),
        Entry::Answer(text) => markdown_answer(text).into_any(),
        Entry::Step {
            step,
            name,
            summary,
            status,
            detail,
            ..
        } => step_row(state, index, step, name, summary, status, detail).into_any(),
        Entry::Notice(text) => banner(text, catppuccin::YELLOW, "⚠").into_any(),
        Entry::Stopped(text) => banner(
            format!("Turn stopped: {text}"),
            catppuccin::PEACH,
            crate::design::glyph::STOP,
        )
        .into_any(),
        Entry::Error(text) => banner(text, catppuccin::RED, crate::design::glyph::CLOSE).into_any(),
    }
}

fn batch_status(steps: &[BatchedStep]) -> StepStatus {
    if steps.iter().any(|s| s.status == StepStatus::Failed) {
        StepStatus::Failed
    } else if steps.iter().any(|s| s.status == StepStatus::Running) {
        StepStatus::Running
    } else {
        StepStatus::Ok
    }
}

fn batch_label(steps: &[BatchedStep]) -> String {
    let n = steps.len();
    let mut names = Vec::new();
    for step in steps {
        if !names.iter().any(|n| n == &step.name) {
            names.push(step.name.clone());
        }
    }
    let shown = names.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
    let extra = names.len().saturating_sub(3);
    if extra > 0 {
        format!("{n} steps · {shown} +{extra}")
    } else {
        format!("{n} steps · {shown}")
    }
}

fn batch_row(state: AgentPanelState, start: usize, steps: Vec<BatchedStep>) -> impl IntoView {
    let status = batch_status(&steps);
    let default_open = status != StepStatus::Ok;
    let label = batch_label(&steps);
    let first_step = steps.first().map(|s| s.step).unwrap_or(0);
    let last_step = steps.last().map(|s| s.step).unwrap_or(first_step);

    let is_open = move || {
        state
            .batch_open
            .get()
            .get(&start)
            .copied()
            .unwrap_or(default_open)
    };

    let toggle = move || {
        let next = !is_open();
        state.batch_open.update(|map| {
            map.insert(start, next);
        });
    };

    let steps_for_body = steps.clone();

    Stack::vertical((
        Stack::horizontal((
            Label::derived(move || status.glyph().to_string()).style(move |s| {
                s.color(status.color())
                    .font_family(crate::design::SYMBOL.to_string())
                    .font_size(9.0)
                    .width(14.0)
            }),
            Label::derived(move || {
                if first_step == last_step {
                    format!("{first_step}")
                } else {
                    format!("{first_step}–{last_step}")
                }
            })
            .style(|s| {
                nowrap(s)
                    .color(catppuccin::SURFACE2)
                    .font_size(10.0)
                    .width(36.0)
            }),
            Label::derived(move || label.clone()).style(|s| {
                nowrap(s)
                    .color(catppuccin::SAPPHIRE)
                    .font_size(12.0)
                    .font_family(crate::design::MONO.to_string())
                    .flex_grow(1.0)
                    .min_width(0.0)
            }),
            Label::derived(move || {
                if is_open() {
                    crate::design::glyph::EXPANDED.to_string()
                } else {
                    crate::design::glyph::COLLAPSED.to_string()
                }
            })
            .style(|s| {
                s.color(catppuccin::SURFACE2)
                    .font_family(crate::design::SYMBOL.to_string())
                    .font_size(9.0)
                    .width(12.0)
            }),
        ))
        .on_event_stop(floem::event::listener::Click, move |_, _| toggle())
        .style(move |s| {
            s.width_full()
                .items_center()
                .padding_horiz(4.0)
                .padding_vert(3.0)
                .border_radius(4.0)
                .hover(|s| s.background(catppuccin::SURFACE0))
                .cursor(floem::style::CursorStyle::Pointer)
        }),
        dyn_container(is_open, move |open| {
            if open {
                let steps = steps_for_body.clone();
                Box::new(
                    Stack::from_iter(steps.into_iter().map(|s| {
                        step_row(
                            state, s.index, s.step, s.name, s.summary, s.status, s.detail,
                        )
                    }))
                    .style(|s| s.flex_col().width_full().padding_left(8.0).min_width(0.0)),
                ) as Box<dyn View>
            } else {
                Box::new(Empty::new().style(|s| s.display(floem::taffy::Display::None)))
                    as Box<dyn View>
            }
        }),
    ))
    .style(|s| s.width_full().min_width(0.0))
}

fn user_bubble(text: String) -> impl IntoView {
    Container::new(Label::derived(move || text.clone()).style(|s| {
        s.color(catppuccin::TEXT)
            .font_size(13.0)
            .line_height(1.45)
            .padding_horiz(11.0)
            .padding_vert(8.0)
            .background(catppuccin::SURFACE0)
            .border_radius(8.0)
            .border_left(2.0)
            .border_color(catppuccin::LAVENDER)
    }))
    .style(|s| s.width_full().margin_vert(5.0))
}

fn markdown_answer(text: String) -> impl IntoView {
    let blocks = markdown::parse(&text);
    Stack::from_iter(blocks.into_iter().map(block_view)).style(|s| {
        s.flex_col()
            .width_full()
            .min_width(0.0)
            .gap(8.0)
            .padding_horiz(2.0)
            .padding_vert(6.0)
    })
}

fn block_view(block: Block) -> impl IntoView {
    match block {
        Block::Paragraph { inlines } => {
            inline_block(inlines, 13.0, catppuccin::TEXT, false).into_any()
        }
        Block::Heading { level, inlines } => {
            let size = match level {
                1 => 18.0,
                2 => 16.0,
                _ => 14.0,
            };
            inline_block(inlines, size, catppuccin::TEXT, true).into_any()
        }
        Block::List { ordered, items } => list_view(ordered, items).into_any(),
        Block::Code { lang, code } => code_block(lang, code).into_any(),
    }
}

fn plain_text(inlines: &[Inline]) -> Option<String> {
    match inlines {
        [Inline::Text(s)] => Some(s.clone()),
        [] => Some(String::new()),
        _ => None,
    }
}

fn inline_block(inlines: Vec<Inline>, size: f32, color: Color, heading: bool) -> impl IntoView {
    if let Some(plain) = plain_text(&inlines) {
        return Label::derived(move || plain.clone())
            .style(move |s| {
                s.color(color)
                    .font_size(size)
                    .line_height(1.5)
                    .width_full()
                    .min_width(0.0)
                    .apply_if(heading, |s| s.font_weight(FontWeight::BOLD))
            })
            .into_any();
    }
    let (text, attrs) = layout_inlines(&inlines, size, color, heading);
    let text_fn = text.clone();
    let attrs_fn = attrs.clone();
    rich_text(text, attrs, move || (text_fn.clone(), attrs_fn.clone()))
        .style(|s| s.width_full().min_width(0.0))
        .into_any()
}

fn list_view(ordered: bool, items: Vec<Vec<Inline>>) -> impl IntoView {
    Stack::from_iter(items.into_iter().enumerate().map(move |(i, inlines)| {
        let marker = if ordered {
            format!("{}.", i + 1)
        } else {
            "•".to_string()
        };
        Stack::horizontal((
            Label::derived(move || marker.clone()).style(|s| {
                nowrap(s)
                    .color(catppuccin::OVERLAY1)
                    .font_size(13.0)
                    .width(22.0)
            }),
            inline_block(inlines, 13.0, catppuccin::TEXT, false),
        ))
        .style(|s| s.width_full().min_width(0.0).items_start().gap(6.0))
    }))
    .style(|s| s.flex_col().width_full().min_width(0.0).gap(4.0))
}

fn code_block(lang: String, code: String) -> impl IntoView {
    let copied = RwSignal::new(false);
    let header = if lang.is_empty() {
        "code".to_string()
    } else {
        lang
    };
    let code_for_copy = code.clone();
    Stack::vertical((
        Stack::horizontal((
            Label::derived({
                let header = header.clone();
                move || header.clone()
            })
            .style(|s| {
                nowrap(s)
                    .color(catppuccin::OVERLAY1)
                    .font_size(10.0)
                    .font_family(crate::design::MONO.to_string())
                    .flex_grow(1.0)
                    .min_width(0.0)
            }),
            Label::derived(move || {
                if copied.get() {
                    "copied".to_string()
                } else {
                    "copy".to_string()
                }
            })
            .on_event_stop(floem::event::listener::Click, move |_, _| {
                let _ = floem::Clipboard::set_contents(code_for_copy.clone());
                copied.set(true);
                floem::action::exec_after(Duration::from_secs(2), move |_| {
                    copied.set(false);
                });
            })
            .style(|s| {
                nowrap(s)
                    .color(catppuccin::LAVENDER)
                    .font_size(10.0)
                    .padding_horiz(6.0)
                    .padding_vert(2.0)
                    .border_radius(4.0)
                    .cursor(floem::style::CursorStyle::Pointer)
                    .hover(|s| s.background(catppuccin::SURFACE1))
            }),
        ))
        .style(|s| s.width_full().items_center()),
        Label::derived(move || code.clone()).style(|s| {
            s.color(catppuccin::SUBTEXT0)
                .font_size(12.0)
                .font_family(crate::design::MONO.to_string())
                .line_height(1.4)
                .width_full()
                .min_width(0.0)
        }),
    ))
    .style(|s| {
        s.width_full()
            .min_width(0.0)
            .gap(4.0)
            .padding(8.0)
            .background(catppuccin::CRUST)
            .border_radius(5.0)
    })
}

fn answer_attrs() -> Attrs<'static> {
    Attrs::new()
        .font_size(13.0)
        .color(catppuccin::TEXT)
        .line_height(LineHeightValue::Normal(1.5))
}

fn layout_inlines(
    inlines: &[Inline],
    size: f32,
    color: Color,
    heading: bool,
) -> (String, AttrsList) {
    let mut defaults = Attrs::new()
        .font_size(size)
        .color(color)
        .line_height(LineHeightValue::Normal(1.5));
    if heading {
        defaults = defaults.weight(FontWeight::BOLD);
    }
    let mut attrs = AttrsList::new(defaults);
    let mut buf = String::new();
    append_inlines(&mut buf, &mut attrs, inlines, size, color, heading);
    (buf, attrs)
}

fn append_inlines(
    buf: &mut String,
    attrs: &mut AttrsList,
    inlines: &[Inline],
    size: f32,
    color: Color,
    heading: bool,
) {
    for inline in inlines {
        let start = buf.len();
        match inline {
            Inline::Text(s) => buf.push_str(s),
            Inline::Bold(s) => {
                buf.push_str(s);
                attrs.add_span(
                    start..buf.len(),
                    Attrs::new()
                        .font_size(size)
                        .color(color)
                        .weight(FontWeight::BOLD)
                        .line_height(LineHeightValue::Normal(1.5)),
                );
            }
            Inline::Italic(s) => {
                buf.push_str(s);
                let mut span = Attrs::new()
                    .font_size(size)
                    .color(color)
                    .font_style(FontStyle::Italic)
                    .line_height(LineHeightValue::Normal(1.5));
                if heading {
                    span = span.weight(FontWeight::BOLD);
                }
                attrs.add_span(start..buf.len(), span);
            }
            Inline::Code(s) => {
                buf.push_str(s);
                attrs.add_span(
                    start..buf.len(),
                    Attrs::new()
                        .font_size(size)
                        .color(catppuccin::SAPPHIRE)
                        .family(&MONO_FAM)
                        .line_height(LineHeightValue::Normal(1.5)),
                );
            }
        }
    }
}

fn layout_flow(src: &str) -> (String, AttrsList) {
    let blocks = markdown::parse(src);
    let mut buf = String::new();
    let mut attrs = AttrsList::new(answer_attrs());
    for (i, block) in blocks.iter().enumerate() {
        if i > 0 {
            buf.push_str("\n\n");
        }
        match block {
            Block::Paragraph { inlines } => {
                append_inlines(&mut buf, &mut attrs, inlines, 13.0, catppuccin::TEXT, false);
            }
            Block::Heading { level, inlines } => {
                let size = match *level {
                    1 => 18.0,
                    2 => 16.0,
                    _ => 14.0,
                };
                append_inlines(&mut buf, &mut attrs, inlines, size, catppuccin::TEXT, true);
            }
            Block::List { ordered, items } => {
                for (n, item) in items.iter().enumerate() {
                    if n > 0 {
                        buf.push('\n');
                    }
                    if *ordered {
                        buf.push_str(&format!("{}. ", n + 1));
                    } else {
                        buf.push_str("• ");
                    }
                    append_inlines(&mut buf, &mut attrs, item, 13.0, catppuccin::TEXT, false);
                }
            }
            Block::Code { code, .. } => {
                let start = buf.len();
                buf.push_str(code);
                attrs.add_span(
                    start..buf.len(),
                    Attrs::new()
                        .font_size(12.0)
                        .color(catppuccin::SUBTEXT0)
                        .family(&MONO_FAM)
                        .line_height(LineHeightValue::Normal(1.4)),
                );
            }
        }
    }
    if buf.is_empty() && !src.is_empty() {
        buf.push_str(src);
    }
    (buf, attrs)
}

#[allow(clippy::too_many_arguments)]
fn step_row(
    state: AgentPanelState,
    index: usize,
    step: usize,
    name: String,
    summary: String,
    status: StepStatus,
    detail: String,
) -> impl IntoView {
    let has_detail = !detail.is_empty();
    let expandable = has_detail;
    // Failures open by default — an error you have to click to see is an error
    // you will miss.
    let starts_open = status == StepStatus::Failed;

    let is_open = move || {
        let manually = state.expanded.get().contains(&index);
        if starts_open {
            !manually // clicking a failed row collapses it
        } else {
            manually
        }
    };

    let toggle = move || {
        if !expandable {
            return;
        }
        state.expanded.update(|open| {
            if let Some(pos) = open.iter().position(|i| *i == index) {
                open.remove(pos);
            } else {
                open.push(index);
            }
        });
    };

    let name_for_row = name.clone();
    let summary_for_row = summary.clone();
    let detail_for_row = detail.clone();

    Stack::vertical((
        Stack::horizontal((
            Label::derived(move || status.glyph().to_string()).style(move |s| {
                s.color(status.color())
                    .font_family(crate::design::SYMBOL.to_string())
                    .font_size(9.0)
                    .width(14.0)
            }),
            Label::derived(move || format!("{step}")).style(|s| {
                nowrap(s)
                    .color(catppuccin::SURFACE2)
                    .font_size(10.0)
                    .width(18.0)
            }),
            Label::derived(move || name_for_row.clone()).style(|s| {
                nowrap(s)
                    .color(catppuccin::SAPPHIRE)
                    .font_size(12.0)
                    .font_family(crate::design::MONO.to_string())
                    .margin_right(7.0)
            }),
            Label::derived(move || summary_for_row.clone()).style(|s| {
                s.color(catppuccin::OVERLAY1)
                    .font_size(11.0)
                    .font_family(crate::design::MONO.to_string())
                    .flex_grow(1.0)
                    .min_width(0.0)
            }),
            Label::derived(move || {
                if !expandable {
                    String::new()
                } else if is_open() {
                    "▾".to_string()
                } else {
                    "▸".to_string()
                }
            })
            .style(|s| {
                s.color(catppuccin::SURFACE2)
                    .font_family(crate::design::SYMBOL.to_string())
                    .font_size(9.0)
                    .width(12.0)
            }),
        ))
        .on_event_stop(floem::event::listener::Click, move |_, _| toggle())
        .style(move |s| {
            s.width_full()
                .items_center()
                .padding_horiz(4.0)
                .padding_vert(3.0)
                .border_radius(4.0)
                .apply_if(expandable, |s| {
                    s.hover(|s| s.background(catppuccin::SURFACE0))
                })
        }),
        // Detail, only when open.
        dyn_container(
            move || is_open() && has_detail,
            move |open| {
                if open {
                    let detail = detail_for_row.clone();
                    Box::new(Label::derived(move || detail.clone()).style(move |s| {
                        s.color(if status == StepStatus::Failed {
                            catppuccin::RED
                        } else {
                            catppuccin::SUBTEXT0
                        })
                        .font_size(11.0)
                        .font_family(crate::design::MONO.to_string())
                        .line_height(1.4)
                        .width_full()
                        .padding(8.0)
                        .margin_left(32.0)
                        .margin_bottom(4.0)
                        .background(catppuccin::CRUST)
                        .border_radius(5.0)
                    })) as Box<dyn View>
                } else {
                    Box::new(Empty::new().style(|s| s.display(floem::taffy::Display::None)))
                        as Box<dyn View>
                }
            },
        ),
    ))
    .style(|s| s.width_full())
}

fn banner(text: String, color: Color, glyph: &'static str) -> impl IntoView {
    Stack::horizontal((
        Label::derived(move || glyph.to_string()).style(move |s| {
            s.color(color)
                .font_family(crate::design::SYMBOL.to_string())
                .font_size(11.0)
                .margin_right(7.0)
        }),
        Label::derived(move || text.clone()).style(move |s| {
            s.color(color)
                .font_size(11.5)
                .line_height(1.4)
                .flex_grow(1.0)
        }),
    ))
    .style(move |s| {
        s.width_full()
            .items_start()
            .padding_horiz(9.0)
            .padding_vert(6.0)
            .margin_vert(4.0)
            .background(catppuccin::SURFACE0)
            .border_left(2.0)
            .border_color(color)
            .border_radius(4.0)
    })
}

/// Live reasoning and answer text while a turn runs.
///
/// **The triggers below are `Memo`s and not plain closures, and that is what
/// keeps the panel on screen.**
///
/// `dyn_container` tears down and rebuilds its entire child whenever a signal
/// read in its trigger changes — there is no equality check. These triggers read
/// the streaming text, which changes on *every token the model emits*. So a long
/// answer rebuilt this whole region dozens of times a second for as long as it
/// took to arrive, and the visible result was the right-hand panel going blank
/// until the turn finished and the streaming signals were cleared.
///
/// A `Memo` propagates only when its value actually changes, so the rebuild
/// happens on the two transitions that matter — activity starting, activity
/// stopping — instead of on every delta. The text still updates live without
/// any of this: `Label::derived` and `rich_text`'s `text_fn` re-read their own
/// signals, and changing a string is not a view being rebuilt.
///
/// This is the third time this exact trap has been paid for here. It cost the
/// terminal its keyboard focus once — every keystroke rebuilt the subtree and
/// destroyed the focused view — which is why `apps/smithy/src/terminal.rs`
/// carries two version signals rather than one. Anything handed to
/// `dyn_container` wants asking: *does this change more often than the shape of
/// what it builds?*
fn live_activity(state: AgentPanelState) -> impl IntoView {
    let showing_reasoning = Memo::new(move |_| !state.streaming_reasoning.get().is_empty());
    let showing_answer = Memo::new(move |_| !state.streaming_answer.get().is_empty());
    let has_activity = Memo::new(move |_| showing_reasoning.get() || showing_answer.get());

    dyn_container(
        move || has_activity.get(),
        move |active| {
            if !active {
                return Box::new(Empty::new().style(|s| s.display(floem::taffy::Display::None)))
                    as Box<dyn View>;
            }
            Box::new(
                Stack::vertical((
                    // Reasoning: dimmed and italic, and only the tail — the whole
                    // chain can run to thousands of tokens and the recent part is
                    // the part that tells you what it is doing now.
                    dyn_container(
                        move || showing_reasoning.get(),
                        move |show| {
                            if !show {
                                return Box::new(
                                    Empty::new().style(|s| s.display(floem::taffy::Display::None)),
                                ) as Box<dyn View>;
                            }
                            // Scrollable, and showing the *whole* trace rather
                            // than a 240-character tail.
                            //
                            // The tail was the reason the thinking could not be
                            // followed: by the time you looked, the sentence you
                            // wanted had already scrolled out of a window you
                            // could not scroll. A bounded height keeps it from
                            // pushing the composer off screen; the scroll gives
                            // the rest back. It stays dimmed and italic, because
                            // it is still subordinate to the answer.
                            //
                            // Pinned to the bottom as it streams, so the live
                            // edge is what you see without touching anything —
                            // and scrolling up still works, which is the whole
                            // point.
                            Box::new(
                                floem::views::scroll::Scroll::new(
                                    Label::derived(move || state.streaming_reasoning.get()).style(
                                        |s| {
                                            s.color(catppuccin::OVERLAY0)
                                                .font_size(11.0)
                                                .font_style(floem::text::FontStyle::Italic)
                                                .line_height(1.45)
                                                .width_full()
                                                .padding_right(6.0)
                                        },
                                    ),
                                )
                                .scroll_to_percent(move || {
                                    // Read the signal so this re-runs on every
                                    // fragment; the value itself is always "the
                                    // end".
                                    let _ = state.streaming_reasoning.get();
                                    100.0
                                })
                                .custom_style(|s: floem::views::scroll::ScrollCustomStyle| {
                                    s.hide_bars(false)
                                        .handle_background(catppuccin::SURFACE1)
                                        .handle_border_radius(4.0)
                                })
                                .style(|s| {
                                    s.width_full()
                                        .min_width(0.0)
                                        .max_height(150.0)
                                        .border_left(2.0)
                                        .border_color(catppuccin::SURFACE1)
                                        .padding_left(8.0)
                                }),
                            ) as Box<dyn View>
                        },
                    ),
                    dyn_container(
                        move || showing_answer.get(),
                        move |show| {
                            if !show {
                                return Box::new(
                                    Empty::new().style(|s| s.display(floem::taffy::Display::None)),
                                ) as Box<dyn View>;
                            }
                            // `rich_text` updates in place via `text_fn`. Do not
                            // parse this into a Stack of blocks on every token —
                            // that is the dyn_container trap above.
                            Box::new(
                                rich_text(
                                    String::new(),
                                    AttrsList::new(answer_attrs()),
                                    move || layout_flow(&state.streaming_answer.get()),
                                )
                                .style(|s| s.width_full().min_width(0.0).margin_top(4.0)),
                            ) as Box<dyn View>
                        },
                    ),
                ))
                .style(|s| {
                    s.width_full()
                        // For the same reason the transcript's scroll container has
                        // it: `min-width: auto` is the flexbox default and refuses
                        // to shrink below the content's intrinsic width, so one long
                        // unbroken token in a streaming answer would otherwise widen
                        // this region and the panel with it.
                        .min_width(0.0)
                        .padding_vert(4.0)
                }),
            ) as Box<dyn View>
        },
    )
}

/// The attached-files row, and the whole of the context-management surface.
///
/// Each chip is one file: a toggle, its name, its cost, and a way to drop it.
/// The row totals what you are about to spend and turns red past the ceiling,
/// which is the point — the failure mode this replaces is discovering that a
/// message was enormous only after the endpoint has spent a minute prefilling it.
///
/// Toggling rather than only removing is deliberate. Trimming a context usually
/// means "not this one, this time", and a chip you unchecked is one you can
/// check again without going back to the file browser.
fn attachment_row(state: AgentPanelState) -> impl IntoView {
    let over = move || crate::attachment::over_budget(&state.attachments.get());

    Stack::vertical((
        Stack::horizontal((
            Label::derived(move || {
                let list = state.attachments.get();
                let count = list.iter().filter(|a| a.included).count();
                let tokens = crate::attachment::total_tokens(&list);
                match count {
                    0 => "no files included".to_string(),
                    1 => format!("1 file · ~{} tokens", format_tokens(tokens as i64)),
                    n => format!("{n} files · ~{} tokens", format_tokens(tokens as i64)),
                }
            })
            .style(move |s| {
                s.font_size(10.0).color(if over() {
                    catppuccin::RED
                } else {
                    catppuccin::SURFACE2
                })
            }),
            Label::derived(|| " — over the size limit; uncheck some".to_string()).style(move |s| {
                s.font_size(10.0)
                    .color(catppuccin::RED)
                    .apply_if(!over(), |s| s.display(floem::taffy::Display::None))
            }),
            Container::new(Empty::new()).style(|s| s.flex_grow(1.0)),
            Label::derived(|| "Remove all".to_string())
                .on_event_stop(floem::event::listener::Click, move |_, _| {
                    state.clear_attachments()
                })
                .style(|s| {
                    s.font_size(10.0)
                        .color(catppuccin::OVERLAY1)
                        .padding_horiz(5.0)
                        .border_radius(3.0)
                        .cursor(floem::style::CursorStyle::Pointer)
                        .hover(|s| s.color(catppuccin::RED))
                }),
        ))
        .style(|s| s.width_full().items_center().margin_bottom(5.0)),
        dyn_stack(
            move || state.attachments.get(),
            |a| a.path.clone(),
            move |a| attachment_chip(state, a),
        )
        .style(|s| {
            s.width_full()
                .flex_row()
                .flex_wrap(floem::taffy::FlexWrap::Wrap)
                .gap(5.0)
        }),
    ))
    .style(move |s| {
        s.width_full()
            .padding_horiz(11.0)
            .padding_top(9.0)
            .background(catppuccin::MANTLE)
            .apply_if(state.attachments.get().is_empty(), |s| {
                s.display(floem::taffy::Display::None)
            })
    })
}

fn attachment_chip(
    state: AgentPanelState,
    attachment: crate::attachment::Attachment,
) -> impl IntoView {
    let included = attachment.included;
    let name = attachment.short_name().to_string();
    let full = attachment.display.clone();
    let note = attachment.kind.note();
    let size = crate::attachment::human_size(attachment.bytes);
    let toggle_path = attachment.path.clone();
    let remove_path = attachment.path.clone();

    Stack::horizontal((
        Label::derived(move || {
            if included {
                crate::design::glyph::DOT
            } else {
                crate::design::glyph::RING
            }
            .to_string()
        })
        .style(move |s| {
            s.font_family(crate::design::SYMBOL.to_string())
                .font_size(8.0)
                .margin_right(5.0)
                .color(if included {
                    catppuccin::GREEN
                } else {
                    catppuccin::SURFACE2
                })
        }),
        // The file name, with the rest of its path as the quiet half — a chip
        // showing only `mod.rs` is not a chip that tells you which one.
        Label::derived(move || name.clone()).style(move |s| {
            s.font_size(11.0)
                .font_family(crate::design::MONO.to_string())
                .color(if included {
                    catppuccin::TEXT
                } else {
                    catppuccin::SURFACE2
                })
        }),
        Label::derived(move || match note {
            Some(note) => format!(" {note}"),
            None => format!(" {size}"),
        })
        .style(move |s| {
            s.font_size(9.0).color(if note.is_some() {
                catppuccin::PEACH
            } else {
                catppuccin::SURFACE2
            })
        }),
        Label::derived(|| crate::design::glyph::CLOSE.to_string())
            .on_event_stop(floem::event::listener::Click, move |_, _| {
                state.remove_attachment(&remove_path)
            })
            .style(|s| {
                s.font_family(crate::design::SYMBOL.to_string())
                    .font_size(9.0)
                    .margin_left(6.0)
                    .color(catppuccin::SURFACE2)
                    .cursor(floem::style::CursorStyle::Pointer)
                    .hover(|s| s.color(catppuccin::RED))
            }),
    ))
    .on_event_stop(floem::event::listener::Click, move |_, _| {
        state.toggle_attachment(&toggle_path)
    })
    // The full path, for the chip that says `mod.rs`.
    .style(move |s| {
        let _ = &full;
        s.items_center()
            .padding_horiz(7.0)
            .padding_vert(3.0)
            .border_radius(5.0)
            .background(catppuccin::SURFACE0)
            .border(1.0)
            .border_color(if included {
                catppuccin::SURFACE1
            } else {
                catppuccin::SURFACE0
            })
            .cursor(floem::style::CursorStyle::Pointer)
            .hover(|s| s.background(catppuccin::SURFACE1))
    })
}

/// The outline shown while files are being dragged over the panel.
///
/// An overlay rather than a border on the panel itself: floem lays a border out
/// as part of the box, so switching one on mid-drag would reflow everything
/// underneath it — the content would visibly jump as the cursor crossed the edge.
fn drop_overlay(state: AgentPanelState) -> impl IntoView {
    Container::new(
        Label::derived(|| "Drop files to add them to the next message".to_string()).style(|s| {
            s.color(catppuccin::LAVENDER)
                .font_size(12.0)
                .padding_horiz(14.0)
                .padding_vert(9.0)
                .background(catppuccin::BASE)
                .border_radius(7.0)
        }),
    )
    .style(move |s| {
        if state.drop_active.get() {
            s.absolute()
                .inset(0.0)
                .items_center()
                .justify_center()
                .background(Color::from_rgba8(30, 30, 46, 216))
                .border(2.0)
                .border_color(catppuccin::LAVENDER)
                .border_radius(4.0)
                .z_index(50)
                // File-drag events are target-phase only — they do not bubble.
                // An overlay that accepts hits steals Drop and Leave from the
                // stack that owns them, so the outline stays up forever. A
                // screenshot dragged from the macOS thumbnail is the usual way
                // this shows up: Enter lights the overlay, then nothing else
                // reaches the handler. `pointer_events_none` stops the steal;
                // the panel also times the outline out and clears it on click.
                .pointer_events_none()
        } else {
            s.display(floem::taffy::Display::None)
        }
    })
}

#[derive(Clone)]
struct LedgerLine {
    key: String,
    label: String,
    inspectable: bool,
}

fn ledger_lines(state: AgentPanelState) -> Vec<LedgerLine> {
    let Some(usage) = state.context_usage.get() else {
        return Vec::new();
    };
    let mut lines = Vec::new();
    for row in &usage.rows {
        if row.tokens <= 0 && row.name != "Conversation" {
            continue;
        }
        let tag = if row.frozen { "fixed" } else { "live" };
        lines.push(LedgerLine {
            key: row.name.clone(),
            label: format!(
                "{} {} · {}",
                format_tokens(row.tokens),
                row.name.to_ascii_lowercase(),
                tag
            ),
            inspectable: true,
        });
    }
    let pending = crate::attachment::total_tokens(&state.attachments.get()) as i64;
    if pending > 0 {
        lines.push(LedgerLine {
            key: "attachments".into(),
            label: format!("{} attachments · pending", format_tokens(pending)),
            inspectable: false,
        });
    }
    if usage.reasoning_tokens > 0 {
        lines.push(LedgerLine {
            key: "reasoning".into(),
            label: format!(
                "{} reasoning · generated, not sent",
                format_tokens(usage.reasoning_tokens)
            ),
            inspectable: false,
        });
    }
    if state.last_request.get().is_some() {
        lines.push(LedgerLine {
            key: "Last request".into(),
            label: "last request · posted".into(),
            inspectable: true,
        });
    }
    lines
}

fn inspect_filename(segment: &str) -> String {
    match segment {
        "System prompt" => "system-prompt.md".into(),
        "Project context" => "project-context.md".into(),
        "Tool schemas" => "tool-schemas.json".into(),
        "Conversation" => "conversation.md".into(),
        "Last request" => "last-request.json".into(),
        other => format!("{other}.md"),
    }
}

fn ledger_row(
    state: AgentPanelState,
    line: LedgerLine,
    on_inspect: std::rc::Rc<dyn Fn(String, String)>,
) -> impl IntoView {
    let inspectable = line.inspectable;
    let key = line.key.clone();
    Label::derived(move || line.label.clone())
        .on_event_stop(floem::event::listener::Click, move |_, _| {
            if !inspectable {
                return;
            }
            let body = if key == "Last request" {
                state.last_request.get_untracked().unwrap_or_default()
            } else {
                state
                    .inspect_segments
                    .get_untracked()
                    .into_iter()
                    .find(|(n, _)| n == &key)
                    .map(|(_, t)| t)
                    .unwrap_or_else(|| "(empty)".into())
            };
            on_inspect(inspect_filename(&key), body);
        })
        .style(move |s| {
            s.color(catppuccin::SURFACE2)
                .font_size(10.0)
                .line_height(1.35)
                .apply_if(inspectable, |s| {
                    s.cursor(floem::style::CursorStyle::Pointer)
                        .hover(|s| s.color(catppuccin::TEXT))
                })
        })
}

/// Context usage: bar + per-segment attribution.
///
/// Prefill cost grows superlinearly with context, so this is the readout that
/// explains a slow turn. Rows come from a stashed snapshot — never rebuilt
/// here from Session state.
fn budget_bar(
    state: AgentPanelState,
    on_inspect: std::rc::Rc<dyn Fn(String, String)>,
) -> impl IntoView {
    let fraction = move || context_fraction(state.context_tokens.get(), state.context_limit.get());

    Stack::vertical((
        Label::derived(move || {
            state
                .last_stop
                .get()
                .map(|r| format!("Turn stopped: {r}"))
                .unwrap_or_default()
        })
        .style(move |s| {
            s.color(catppuccin::PEACH)
                .font_size(11.0)
                .padding_horiz(12.0)
                .padding_top(6.0)
                .apply_if(state.last_stop.get().is_none(), |s| {
                    s.display(floem::taffy::Display::None)
                })
        }),
        Stack::horizontal((
            Label::derived(move || {
                let used = state.context_tokens.get();
                if used == 0 {
                    String::new()
                } else {
                    format!(
                        "{} / {} context",
                        format_tokens(used),
                        format_tokens(state.context_limit.get())
                    )
                }
            })
            .style(|s| s.color(catppuccin::SURFACE2).font_size(10.0)),
            Container::new(Empty::new()).style(|s| s.flex_grow(1.0)),
            Label::derived(move || {
                state
                    .context_usage
                    .get()
                    .and_then(|u| u.hit_rate)
                    .map(|r| format!("cache {:.0}%", r * 100.0))
                    .unwrap_or_default()
            })
            .style(|s| s.color(catppuccin::SURFACE2).font_size(10.0)),
        ))
        .style(|s| s.width_full().padding_horiz(12.0).padding_bottom(3.0)),
        // The bar itself: a filled track whose width tracks the fraction.
        // Cached portion is drawn first (cooler), cold on top of the remainder.
        Container::new(
            Stack::horizontal((
                Empty::new().style(move |s| {
                    let usage = state.context_usage.get();
                    let cached_pct = usage
                        .as_ref()
                        .filter(|u| u.prompt_tokens > 0)
                        .map(|u| {
                            (u.cached_tokens as f64 / u.prompt_tokens as f64) * fraction() * 100.0
                        })
                        .unwrap_or(0.0);
                    s.height_full()
                        .width_pct(cached_pct)
                        .background(catppuccin::TEAL)
                        .border_radius(1.0)
                }),
                Empty::new().style(move |s| {
                    let usage = state.context_usage.get();
                    let cold_pct = usage
                        .as_ref()
                        .filter(|u| u.prompt_tokens > 0)
                        .map(|u| {
                            (u.cold_tokens as f64 / u.prompt_tokens as f64) * fraction() * 100.0
                        })
                        .unwrap_or_else(|| fraction() * 100.0);
                    s.height_full()
                        .width_pct(cold_pct)
                        .background(budget_color(fraction()))
                        .border_radius(1.0)
                }),
            ))
            .style(|s| s.width_full().height_full()),
        )
        .style(|s| {
            s.width_full()
                .height(2.0)
                .margin_horiz(12.0)
                .background(catppuccin::SURFACE0)
        }),
        // Segment rows. Click a row to open that segment's text. Frozen
        // (system / project / tools) vs live (conversation). Pending
        // attachments come from the panel signal — they are not in the
        // session ledger until the next send (audit §4.3).
        dyn_stack(
            move || ledger_lines(state),
            |line| line.key.clone(),
            move |line| ledger_row(state, line, on_inspect.clone()),
        )
        .style(move |s| {
            s.flex_col()
                .width_full()
                .padding_horiz(12.0)
                .padding_top(4.0)
                .apply_if(state.context_usage.get().is_none(), |s| {
                    s.display(floem::taffy::Display::None)
                })
        }),
    ))
    .style(move |s| {
        s.width_full()
            .padding_top(5.0)
            .padding_bottom(4.0)
            .apply_if(state.context_tokens.get() == 0, |s| {
                s.display(floem::taffy::Display::None)
            })
    })
}

/// The microphone.
///
/// One button with five meanings, which is why what it *does* lives in
/// `smithy_voice::press` as a pure function and not in this closure. Here it
/// only has to look like whatever it currently is.
///
/// A filled red dot for a live microphone rather than a mic pictogram: it is
/// the one state with a privacy cost, "recording" is universally a red circle,
/// and the panel header already speaks in dots. Yellow means busy — loading the
/// model the first time takes tens of seconds, and a button that looked idle
/// through that would be pressed again and again.
///
/// **The failure detail lives under a hover, not beside the dot.** It used to
/// print the raw error inline — `microphone unavailable: Failed to get default
/// input config` — parked in red in the composer. That inverted the panel's own
/// severity language: an unreachable agent, which stops the entire point of the
/// app, is one small dot, while an unconfigured microphone, which costs nothing,
/// was a sentence in red. A stranger reads the loudest thing on screen as the
/// most broken thing. The dot already carried the state; the string only
/// carried alarm.
fn microphone(
    state: AgentPanelState,
    hotkey: String,
    on_voice: impl Fn() + 'static,
) -> impl IntoView {
    use smithy_voice::Voice;

    // The hover text needs the shortcut too, and both closures outlive us.
    let hover_hotkey = hotkey.clone();
    let hovered = RwSignal::new(false);

    Stack::horizontal((
        // The detail, on hover, **above** the row.
        //
        // Not `floem::views::tooltip`: it places the overlay at the hover point
        // plus a hardcoded `(0, 10)` and `TooltipStyle` exposes only a delay, so
        // it always opens down and to the right. This button sits on the bottom
        // edge of the window, where down is off-screen — which is exactly where
        // the first version of this put it. `hover_popup` already establishes
        // the pattern that works here: an absolutely-positioned container
        // toggled by a signal, so the app decides where it goes.
        //
        // Out of flow, so it cannot move the row it is anchored to.
        Container::new(
            Label::derived(move || {
                let hotkey = hover_hotkey.clone();
                match state.voice.get() {
                    Voice::Cold => format!("Dictation. {hotkey} loads the model on first use."),
                    Voice::Loading => {
                        "Loading the speech model. The first time takes tens of seconds."
                            .to_string()
                    }
                    Voice::Ready => format!("Dictation. Hold {hotkey} to talk."),
                    Voice::Listening => "Listening. Release to transcribe.".to_string(),
                    Voice::Transcribing => "Transcribing what you said.".to_string(),
                    // The whole reason this hover exists.
                    Voice::Failed(why) => why,
                }
            })
            .style(|s| {
                s.font_family(crate::design::SYMBOL.to_string())
                    .font_size(crate::design::TEXT_SM)
                    .color(crate::design::FG)
                    .line_height(1.4)
            }),
        )
        .style(move |s| {
            if hovered.get() {
                s.absolute()
                    // Clear of a row about 24px tall, anchored to its bottom.
                    .inset_bottom(28.0)
                    .inset_left(0.0)
                    .max_width(340.0)
                    .padding(crate::design::SPACE_2)
                    .background(crate::design::BG_FLOAT)
                    .border(1.0)
                    .border_color(crate::design::BORDER)
                    .border_radius(crate::design::RADIUS_LG)
                    .z_index(400)
            } else {
                s.display(floem::taffy::Display::None)
            }
        }),
        Label::derived(move || {
            match state.voice.get() {
                // Hollow until there is a model behind it.
                Voice::Cold | Voice::Failed(_) => crate::design::glyph::RING,
                _ => crate::design::glyph::DOT,
            }
            .to_string()
        })
        .on_event_stop(floem::event::listener::Click, move |_, _| on_voice())
        .style(move |s| {
            let voice = state.voice.get();
            s.font_family(crate::design::SYMBOL.to_string())
                .font_size(14.0)
                .padding_horiz(9.0)
                .padding_vert(4.0)
                .margin_right(6.0)
                .border_radius(6.0)
                .color(match &voice {
                    Voice::Listening => catppuccin::RED,
                    Voice::Loading | Voice::Transcribing => catppuccin::YELLOW,
                    Voice::Failed(_) => catppuccin::MAROON,
                    _ => catppuccin::SURFACE2,
                })
                .apply_if(voice.is_recording(), |s| s.background(catppuccin::SURFACE0))
                .apply_if(voice.accepts_press(), |s| {
                    s.cursor(floem::style::CursorStyle::Pointer)
                        .hover(|s| s.background(catppuccin::SURFACE0))
                })
        }),
        // The shortcut, and — while anything is happening — what is happening.
        // Loading takes tens of seconds the first time and a silent button would
        // be pressed again and again.
        Label::derived(move || match state.voice.get() {
            Voice::Cold => hotkey.clone(),
            Voice::Loading => "loading model…".to_string(),
            Voice::Ready => hotkey.clone(),
            Voice::Listening => "listening…".to_string(),
            Voice::Transcribing => "transcribing…".to_string(),
            // One word, not the error. Short enough to read as a state rather
            // than a fault, and long enough to invite the hover that explains
            // it. Deliberately not the shortcut — offering a key that will not
            // work is worse than saying nothing.
            Voice::Failed(_) => "unavailable".to_string(),
        })
        .style(move |s| {
            // **The family matters even on a label that is mostly words.** This
            // hint carries `⌘` and `⇧`, and floem's default family is sans,
            // which resolves to Helvetica here — neither glyph exists in it, so
            // the shortcut rendered as two boxes and a V. The glyph guard
            // cannot catch this: the characters were right, the font was never
            // asked for.
            s.font_family(crate::design::SYMBOL.to_string())
                .font_size(9.0)
                .margin_right(8.0)
                .color(match state.voice.get() {
                    Voice::Listening => catppuccin::RED,
                    Voice::Loading | Voice::Transcribing => catppuccin::YELLOW,
                    Voice::Failed(_) => catppuccin::MAROON,
                    _ => catppuccin::SURFACE2,
                })
        }),
    ))
    .on_event_stop(floem::event::listener::PointerEnter, move |_, _| {
        hovered.set(true)
    })
    .on_event_stop(floem::event::listener::PointerLeave, move |_, _| {
        hovered.set(false)
    })
    // Relative, so the absolute child above anchors to this row rather than
    // escaping to the panel.
    .style(|s| s.items_center().position(floem::taffy::Position::Relative))
}

fn command_matches(input: &str, skills: &[SkillPick]) -> Vec<SkillPick> {
    let Some(rest) = input.strip_prefix('/') else {
        return Vec::new();
    };
    if rest.contains(char::is_whitespace) {
        return Vec::new();
    }
    skills
        .iter()
        .filter(|s| s.name.starts_with(rest))
        .cloned()
        .collect()
}

fn matching_skills(state: AgentPanelState) -> Vec<SkillPick> {
    command_matches(&state.input.get(), &state.skills.get())
}

/// Tab against the current `/` token. Unique prefix fills that name; a second
/// Tab (or Shift+Tab) cycles when several still match.
fn complete_slash(
    input: &str,
    names: &[String],
    cursor: usize,
    reverse: bool,
) -> Option<(String, usize)> {
    let rest = input.strip_prefix('/')?;
    if rest.contains(char::is_whitespace) {
        return None;
    }
    let matches: Vec<(usize, &String)> = names
        .iter()
        .enumerate()
        .filter(|(_, n)| n.starts_with(rest))
        .collect();
    if matches.is_empty() {
        return None;
    }
    let exact = names.iter().position(|n| n == rest);
    let pick = if let Some(at) = exact {
        let n = names.len();
        if reverse {
            (at + n - 1) % n
        } else {
            (at + 1) % n
        }
    } else if reverse {
        matches.last().map(|(i, _)| *i).unwrap_or(0)
    } else {
        matches
            .get(cursor.min(matches.len() - 1))
            .map(|(i, _)| *i)
            .unwrap_or(0)
    };
    Some((format!("/{} ", names[pick]), pick))
}

fn skill_picker(state: AgentPanelState) -> impl IntoView {
    floem::views::scroll::Scroll::new(dyn_stack(
        move || matching_skills(state),
        |s| s.name.clone(),
        move |skill| {
            let name = skill.name.clone();
            let desc = skill.description.clone();
            let hint = skill.argument_hint.clone();
            let name_for_sel = name.clone();
            Stack::vertical((
                Label::derived({
                    let name = name.clone();
                    move || format!("/{name}")
                })
                .style(move |s| {
                    let on = matching_skills(state)
                        .get(state.skill_cursor.get())
                        .is_some_and(|row| row.name == name_for_sel);
                    s.color(if on {
                        catppuccin::LAVENDER
                    } else {
                        catppuccin::TEXT
                    })
                    .font_size(12.0)
                }),
                Label::derived(move || {
                    if hint.is_empty() {
                        desc.clone()
                    } else {
                        format!("{desc} — {hint}")
                    }
                })
                .style(|s| {
                    s.color(catppuccin::OVERLAY0)
                        .font_size(11.0)
                        .text_ellipsis()
                        .width_full()
                }),
            ))
            .style(move |s| {
                let on = matching_skills(state)
                    .get(state.skill_cursor.get())
                    .is_some_and(|row| row.name == skill.name);
                s.width_full()
                    .padding_horiz(10.0)
                    .padding_vert(5.0)
                    .cursor(floem::style::CursorStyle::Pointer)
                    .background(if on {
                        catppuccin::SURFACE0
                    } else {
                        Color::from_rgba8(0, 0, 0, 0)
                    })
                    .hover(|s| s.background(catppuccin::SURFACE0))
            })
            .on_event_stop(floem::event::listener::Click, move |_, _| {
                state.input.set(format!("/{name} "));
            })
        },
    ))
    .custom_style(|s: floem::views::scroll::ScrollCustomStyle| {
        s.hide_bars(false)
            .handle_background(catppuccin::SURFACE1)
            .handle_border_radius(4.0)
    })
    .style(move |s| {
        s.flex_col()
            .width_full()
            .max_height(180.0)
            .margin_bottom(6.0)
            .border(1.0)
            .border_color(catppuccin::SURFACE0)
            .border_radius(6.0)
            .background(catppuccin::BASE)
            .apply_if(matching_skills(state).is_empty(), |s| {
                s.display(floem::taffy::Display::None)
            })
    })
}

fn composer(
    state: AgentPanelState,
    on_send: std::rc::Rc<dyn Fn(String)>,
    on_stop: std::rc::Rc<dyn Fn()>,
    on_voice: impl Fn() + 'static,
    hotkey: String,
) -> impl IntoView {
    let send = {
        let on_send = on_send.clone();
        move || {
            if state.busy.get_untracked() {
                return;
            }
            let text = state.input.get_untracked().trim().to_string();
            if text.is_empty() {
                return;
            }
            state.input.set(String::new());
            on_send(text);
        }
    };
    let send_on_enter = send.clone();

    Stack::vertical((
        skill_picker(state),
        TextInput::new(state.input)
            .placeholder("Ask the agent, or / for a command…")
            .on_event(floem::event::listener::KeyDown, move |_, ev| {
                use floem::event::EventPropagation;
                use floem::prelude::{Key, Modifiers, NamedKey};
                if ev.key == Key::Named(NamedKey::Enter) && !ev.modifiers.contains(Modifiers::SHIFT)
                {
                    send_on_enter();
                    return EventPropagation::Stop;
                }
                let matches =
                    command_matches(&state.input.get_untracked(), &state.skills.get_untracked());
                if matches.is_empty() {
                    return EventPropagation::Continue;
                }
                if ev.key == Key::Named(NamedKey::Tab) {
                    let names: Vec<String> = state
                        .skills
                        .get_untracked()
                        .iter()
                        .map(|s| s.name.clone())
                        .collect();
                    let reverse = ev.modifiers.contains(Modifiers::SHIFT);
                    if let Some((next, cursor)) = complete_slash(
                        &state.input.get_untracked(),
                        &names,
                        state.skill_cursor.get_untracked(),
                        reverse,
                    ) {
                        state.skill_cursor.set(cursor);
                        state.input.set(next);
                    }
                    return EventPropagation::Stop;
                }
                let n = matches.len();
                if ev.key == Key::Named(NamedKey::ArrowDown) {
                    state
                        .skill_cursor
                        .set((state.skill_cursor.get_untracked() + 1) % n);
                    return EventPropagation::Stop;
                }
                if ev.key == Key::Named(NamedKey::ArrowUp) {
                    state
                        .skill_cursor
                        .set((state.skill_cursor.get_untracked() + n - 1) % n);
                    return EventPropagation::Stop;
                }
                EventPropagation::Continue
            })
            .style(|s| {
                s.width_full()
                    .padding_horiz(10.0)
                    .padding_vert(8.0)
                    .font_size(13.0)
                    .color(catppuccin::TEXT)
                    .background(catppuccin::BASE)
                    .border(1.0)
                    .border_color(catppuccin::SURFACE0)
                    .border_radius(7.0)
                    .focus(|s| s.border_color(catppuccin::LAVENDER))
            }),
        Stack::horizontal((
            Label::derived(move || {
                if state.busy.get() {
                    "working…".to_string()
                } else {
                    "Enter to send · Tab completes /commands · Shift+Enter for a newline"
                        .to_string()
                }
            })
            .style(move |s| {
                s.font_size(10.0).color(if state.busy.get() {
                    catppuccin::YELLOW
                } else {
                    catppuccin::SURFACE2
                })
            }),
            Container::new(Empty::new()).style(|s| s.flex_grow(1.0)),
            microphone(state, hotkey, on_voice),
            // Send and Stop occupy the same slot — only one is ever meaningful.
            dyn_container(
                move || state.busy.get(),
                move |busy| {
                    if busy {
                        let on_stop = on_stop.clone();
                        Box::new(
                            Button::new("Stop")
                                .on_event_stop(floem::event::listener::Click, move |_, _| on_stop())
                                .style(|s| {
                                    s.background(catppuccin::SURFACE0)
                                        .color(catppuccin::RED)
                                        .font_size(12.0)
                                        .padding_horiz(14.0)
                                        .padding_vert(5.0)
                                        .border_radius(6.0)
                                        .hover(|s| s.background(catppuccin::SURFACE1))
                                }),
                        ) as Box<dyn View>
                    } else {
                        let send = send.clone();
                        Box::new(
                            Button::new("Send")
                                .on_event_stop(floem::event::listener::Click, move |_, _| send())
                                .style(|s| {
                                    s.background(catppuccin::LAVENDER)
                                        .color(catppuccin::CRUST)
                                        .font_size(12.0)
                                        .font_bold()
                                        .padding_horiz(16.0)
                                        .padding_vert(5.0)
                                        .border_radius(6.0)
                                        .hover(|s| s.background(catppuccin::MAUVE))
                                }),
                        ) as Box<dyn View>
                    }
                },
            ),
        ))
        .style(|s| s.width_full().items_center().margin_top(7.0)),
    ))
    .style(|s| {
        s.width_full()
            .padding(11.0)
            .background(catppuccin::MANTLE)
            .border_top(1.0)
            .border_color(catppuccin::SURFACE0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarizes_a_path_argument_as_just_the_path() {
        assert_eq!(
            summarize_arguments(r#"{"path":"src/main.rs"}"#),
            "src/main.rs"
        );
    }

    #[test]
    fn prefers_the_identifying_argument_over_the_others() {
        let s = summarize_arguments(r#"{"offset":1,"path":"src/lib.rs","limit":50}"#);
        assert_eq!(s, "src/lib.rs");
    }

    #[test]
    fn summarizes_a_shell_command() {
        assert_eq!(
            summarize_arguments(r#"{"command":"cargo test"}"#),
            "cargo test"
        );
    }

    #[test]
    fn empty_arguments_summarize_to_nothing() {
        assert_eq!(summarize_arguments("{}"), "");
    }

    #[test]
    fn falls_back_to_key_value_pairs() {
        let s = summarize_arguments(r#"{"a":"one","b":2}"#);
        assert!(s.contains("a=one"));
        assert!(s.contains("b=2"));
    }

    #[test]
    fn malformed_arguments_still_render_something() {
        assert_eq!(summarize_arguments("{broken"), "{broken");
    }

    #[test]
    fn long_summaries_are_truncated() {
        let long = format!(r#"{{"command":"{}"}}"#, "x".repeat(200));
        let s = summarize_arguments(&long);
        assert!(s.chars().count() <= 60, "got {} chars", s.chars().count());
        assert!(s.ends_with('…'));
    }

    #[test]
    fn multiline_arguments_collapse_to_one_line() {
        let s = summarize_arguments(r#"{"command":"line one\n   line two"}"#);
        assert!(!s.contains('\n'));
        assert_eq!(s, "line one line two");
    }

    #[test]
    fn formats_token_counts_compactly() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1234), "1.2k");
        assert_eq!(format_tokens(32_768), "32.8k");
        assert_eq!(format_tokens(131_072), "131k");
    }

    #[test]
    fn context_fraction_is_clamped() {
        assert_eq!(context_fraction(0, 100), 0.0);
        assert_eq!(context_fraction(50, 100), 0.5);
        assert_eq!(
            context_fraction(500, 100),
            1.0,
            "overflow must not exceed the track"
        );
        assert_eq!(
            context_fraction(10, 0),
            0.0,
            "a zero limit must not divide by zero"
        );
    }

    #[test]
    fn the_budget_bar_escalates_in_colour() {
        assert_eq!(budget_color(0.1), catppuccin::GREEN);
        assert_eq!(budget_color(0.6), catppuccin::YELLOW);
        assert_eq!(budget_color(0.95), catppuccin::RED);
    }

    #[test]
    fn resolving_a_step_marks_the_last_running_one() {
        let state = AgentPanelState::new();
        state.push(Entry::Step {
            id: "c1".into(),
            step: 1,
            name: "read".into(),
            summary: "a.rs".into(),
            status: StepStatus::Running,
            detail: String::new(),
        });
        state.resolve_step("c1", "contents".into(), false);

        let entries = state.entries.get_untracked();
        match &entries[0] {
            Entry::Step { status, detail, .. } => {
                assert_eq!(*status, StepStatus::Ok);
                assert_eq!(detail, "contents");
            }
            other => panic!("expected a step, got {other:?}"),
        }
    }

    #[test]
    fn resolving_marks_failures_distinctly() {
        let state = AgentPanelState::new();
        state.push(Entry::Step {
            id: "c1".into(),
            step: 1,
            name: "bash".into(),
            summary: "false".into(),
            status: StepStatus::Running,
            detail: String::new(),
        });
        state.resolve_step("c1", "exit 1".into(), true);
        match &state.entries.get_untracked()[0] {
            Entry::Step { status, .. } => assert_eq!(*status, StepStatus::Failed),
            other => panic!("expected a step, got {other:?}"),
        }
    }

    /// The regression this fixes. Two parallel `read` calls produce two rows
    /// with the same tool name; results arrive in completion order, not call
    /// order. Matching by name attached the first result to the second row and
    /// silently swapped the two outputs.
    #[test]
    fn parallel_calls_to_the_same_tool_keep_their_own_output() {
        let state = AgentPanelState::new();
        for (id, summary) in [("call_a", "a.rs"), ("call_b", "b.rs")] {
            state.push(Entry::Step {
                id: id.into(),
                step: 1,
                name: "read".into(),
                summary: summary.into(),
                status: StepStatus::Running,
                detail: String::new(),
            });
        }
        // Second call finishes first — the ordering that broke the old code.
        state.resolve_step("call_b", "contents of b".into(), false);
        state.resolve_step("call_a", "contents of a".into(), false);

        let entries = state.entries.get_untracked();
        for entry in &entries {
            if let Entry::Step {
                id,
                summary,
                detail,
                status,
                ..
            } = entry
            {
                assert_eq!(*status, StepStatus::Ok, "no row may be left running");
                match id.as_str() {
                    "call_a" => {
                        assert_eq!(summary, "a.rs");
                        assert_eq!(detail, "contents of a", "output landed on the wrong row");
                    }
                    "call_b" => {
                        assert_eq!(summary, "b.rs");
                        assert_eq!(detail, "contents of b", "output landed on the wrong row");
                    }
                    other => panic!("unexpected id {other}"),
                }
            }
        }
    }

    #[test]
    fn resolving_an_unknown_id_is_a_no_op() {
        let state = AgentPanelState::new();
        state.push(Entry::User("hi".into()));
        state.resolve_step("nonexistent", "x".into(), false);
        assert_eq!(state.entries.get_untracked().len(), 1);
    }

    #[test]
    fn clearing_resets_the_live_state_too() {
        let state = AgentPanelState::new();
        state.push(Entry::User("hi".into()));
        state.streaming_answer.set("partial".into());
        state.context_tokens.set(5000);
        state.context_usage.set(Some(ContextUsageSnapshot {
            prompt_tokens: 5000,
            ..Default::default()
        }));
        state.clear();
        assert!(state.entries.get_untracked().is_empty());
        assert!(state.streaming_answer.get_untracked().is_empty());
        assert_eq!(state.context_tokens.get_untracked(), 0);
        assert!(state.context_usage.get_untracked().is_none());
    }

    /// Chrome must never break mid-word.
    ///
    /// The panel root sets `Wrap { BreakWord }` and it is inherited, which at a
    /// narrow panel rendered the header title as "A ge nt", the button as
    /// "New sessi on" and every tool row as "bas h". Prose still wraps; fixed
    /// labels clip instead.
    #[test]
    fn chrome_labels_clip_rather_than_breaking_words() {
        let style = nowrap(floem::style::Style::new());
        let rendered = format!("{style:?}");
        assert!(
            rendered.contains("NoWrap"),
            "chrome must opt out of the inherited BreakWord: {rendered}"
        );
    }

    #[test]
    fn tab_completes_a_unique_prefix() {
        let names = vec![
            "compact".into(),
            "handoff".into(),
            "grill-me".into(),
            "research".into(),
        ];
        let (out, _) = complete_slash("/c", &names, 0, false).unwrap();
        assert_eq!(out, "/compact ");
        let (out, _) = complete_slash("/r", &names, 0, false).unwrap();
        assert_eq!(out, "/research ");
        let (out, _) = complete_slash("/g", &names, 0, false).unwrap();
        assert_eq!(out, "/grill-me ");
    }

    #[test]
    fn tab_on_a_bare_slash_fills_the_highlighted_command() {
        let names = vec!["compact".into(), "handoff".into(), "research".into()];
        let (out, cursor) = complete_slash("/", &names, 0, false).unwrap();
        assert_eq!(out, "/compact ");
        assert_eq!(cursor, 0);
        let (out, cursor) = complete_slash("/", &names, 0, true).unwrap();
        assert_eq!(out, "/research ");
        assert_eq!(cursor, 2);
    }

    #[test]
    fn a_second_tab_cycles_when_the_name_is_already_filled() {
        let names = vec!["compact".into(), "handoff".into()];
        let (out, cursor) = complete_slash("/compact", &names, 0, false).unwrap();
        assert_eq!(out, "/handoff ");
        assert_eq!(cursor, 1);
        let (out, cursor) = complete_slash("/handoff", &names, 1, false).unwrap();
        assert_eq!(out, "/compact ");
        assert_eq!(cursor, 0);
    }

    #[test]
    fn tab_does_nothing_once_arguments_have_started() {
        let names = vec!["compact".into()];
        assert!(complete_slash("/compact focus", &names, 0, false).is_none());
        assert!(complete_slash("no slash", &names, 0, false).is_none());
    }

    fn ok_step(id: &str, n: usize, name: &str) -> Entry {
        Entry::Step {
            id: id.into(),
            step: n,
            name: name.into(),
            summary: "x".into(),
            status: StepStatus::Ok,
            detail: String::new(),
        }
    }

    #[test]
    fn two_steps_stay_individual_rows() {
        let rows = group_entries(&[ok_step("a", 1, "read"), ok_step("b", 2, "grep")]);
        assert_eq!(rows.len(), 2);
        assert!(matches!(rows[0], TranscriptRow::One { index: 0, .. }));
        assert!(matches!(rows[1], TranscriptRow::One { index: 1, .. }));
    }

    #[test]
    fn three_consecutive_steps_collapse() {
        let rows = group_entries(&[
            ok_step("a", 1, "read"),
            ok_step("b", 2, "grep"),
            ok_step("c", 3, "bash"),
        ]);
        assert_eq!(rows.len(), 1);
        match &rows[0] {
            TranscriptRow::Batch { start, steps } => {
                assert_eq!(*start, 0);
                assert_eq!(steps.len(), 3);
            }
            other => panic!("expected a batch, got a single row: {other:?}"),
        }
    }

    #[test]
    fn an_answer_splits_step_batches() {
        let rows = group_entries(&[
            Entry::User("go".into()),
            ok_step("a", 1, "read"),
            ok_step("b", 2, "grep"),
            ok_step("c", 3, "bash"),
            Entry::Answer("done".into()),
            ok_step("d", 4, "read"),
            ok_step("e", 5, "read"),
        ]);
        assert_eq!(rows.len(), 5);
        assert!(matches!(rows[0], TranscriptRow::One { .. }));
        assert!(matches!(rows[1], TranscriptRow::Batch { start: 1, .. }));
        assert!(matches!(
            rows[2],
            TranscriptRow::One {
                entry: Entry::Answer(_),
                ..
            }
        ));
        assert!(matches!(rows[3], TranscriptRow::One { index: 5, .. }));
        assert!(matches!(rows[4], TranscriptRow::One { index: 6, .. }));
    }

    #[test]
    fn batch_keys_change_when_a_step_resolves() {
        let running = Entry::Step {
            id: "a".into(),
            step: 1,
            name: "read".into(),
            summary: "x".into(),
            status: StepStatus::Running,
            detail: String::new(),
        };
        let done = Entry::Step {
            id: "a".into(),
            step: 1,
            name: "read".into(),
            summary: "x".into(),
            status: StepStatus::Ok,
            detail: "hi".into(),
        };
        let before = row_key(&TranscriptRow::One {
            index: 0,
            entry: running,
        });
        let after = row_key(&TranscriptRow::One {
            index: 0,
            entry: done,
        });
        assert_ne!(
            before, after,
            "dyn_stack only rebuilds on key change; status must be in the key"
        );
    }
}
