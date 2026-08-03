//! Call graph in the center pane — the Benzi-style map.
//!
//! The library half lives in [`smithy_project::callgraph`]. This module is what
//! puts it on screen: build/load wiring, a focus-relative layered layout, and a
//! floem canvas. Never auto-builds — indexing costs ~10 s and ~2.3 GB.

use std::path::PathBuf;
use std::sync::Arc;

use floem::kurbo::{Line, Point, Stroke};
use floem::prelude::*;
use floem::reactive::{RwSignal, SignalGet, SignalUpdate};
use floem::views::{canvas, Decorators};

use smithy_editor::design;
use smithy_project::callgraph::{CallGraph, Node, Staleness};

use crate::app_state::AgentState;
use crate::runtime;

/// Everything the center-pane map needs, held on [`AgentState`].
#[derive(Clone, Copy)]
pub struct CallGraphUi {
    pub graph: RwSignal<Option<Arc<CallGraph>>>,
    pub focus: RwSignal<Option<u32>>,
    /// 1 = direct neighbors, 2 = one more hop.
    pub hops: RwSignal<u8>,
    pub building: RwSignal<bool>,
    pub status: RwSignal<String>,
    pub visible: RwSignal<bool>,
    pub pan: RwSignal<(f64, f64)>,
    pub zoom: RwSignal<f64>,
    /// Pane size, written by the edge canvas so labels can centre themselves.
    pub size: RwSignal<(f64, f64)>,
    /// Project root used for staleness and opening files.
    pub root: RwSignal<PathBuf>,
    /// Cached at build/load — never recomputed on paint (tree walk + hash).
    pub stale: RwSignal<Staleness>,
}

impl CallGraphUi {
    pub fn new() -> Self {
        Self {
            graph: RwSignal::new(None),
            focus: RwSignal::new(None),
            hops: RwSignal::new(1),
            building: RwSignal::new(false),
            status: RwSignal::new(String::new()),
            visible: RwSignal::new(false),
            pan: RwSignal::new((0.0, 0.0)),
            zoom: RwSignal::new(1.0),
            size: RwSignal::new((0.0, 0.0)),
            root: RwSignal::new(PathBuf::new()),
            stale: RwSignal::new(Staleness::default()),
        }
    }
}

/// Load a previously saved graph for this project, if any.
pub fn load_for_project(agent: &AgentState) {
    let root = agent.project.borrow().root.clone();
    agent.call_graph.root.set(root.clone());
    let path = agent.registry.callgraph_path(&root);
    let ui = agent.call_graph;
    let (tx, rx) = crossbeam_channel::bounded::<Result<(CallGraph, Staleness), String>>(1);

    runtime::tokio_runtime().spawn(async move {
        let result = tokio::task::spawn_blocking(move || {
            if !path.exists() {
                return Err("none".into());
            }
            let graph = CallGraph::load(&path)?;
            let stale = graph.staleness(&root);
            Ok((graph, stale))
        })
        .await
        .unwrap_or_else(|e| Err(e.to_string()));
        let _ = tx.send(result);
    });

    // `poll_once`, not `Effect::new`: a menu-triggered Effect has no reactive
    // owner and is disposed before the worker finishes — which is how Build
    // looked like a no-op.
    poll_once(rx, move |result| match result {
        Ok((graph, stale)) => {
            ui.status.set(stale.describe());
            ui.stale.set(stale);
            ui.focus.set(default_focus(&graph));
            ui.graph.set(Some(Arc::new(graph)));
        }
        Err(e) if e == "none" => {
            ui.graph.set(None);
            ui.focus.set(None);
            ui.status.set(String::new());
            ui.stale.set(Staleness::default());
        }
        Err(e) => ui.status.set(e),
    });
}

/// Run `rust-analyzer scip`, assemble, save, and show.
pub fn build(agent: &AgentState) {
    let ui = agent.call_graph;
    if ui.building.get_untracked() {
        return;
    }
    let root = agent.project.borrow().root.clone();
    ui.root.set(root.clone());
    ui.building.set(true);
    ui.status.set("building — ~10 s, ~2 GB…".into());
    ui.visible.set(true);
    agent.panel.push(smithy_editor::AgentEntry::Notice(
        "Building call graph — ~10 s, uses ~2 GB…".into(),
    ));

    let path = agent.registry.callgraph_path(&root);
    let panel = agent.panel;
    let (tx, rx) = crossbeam_channel::bounded::<Result<(CallGraph, Staleness), String>>(1);

    runtime::tokio_runtime().spawn(async move {
        let result = tokio::task::spawn_blocking(move || {
            eprintln!("[callgraph] building for {}…", root.display());
            let graph = CallGraph::build(&root)?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            graph.save(&path)?;
            let stale = graph.staleness(&root);
            eprintln!(
                "[callgraph] done: {} nodes, {} edges → {}",
                graph.nodes.len(),
                graph.edges.len(),
                path.display()
            );
            Ok((graph, stale))
        })
        .await
        .unwrap_or_else(|e| Err(e.to_string()));
        let _ = tx.send(result);
    });

    poll_once(rx, move |result| {
        ui.building.set(false);
        match result {
            Ok((graph, stale)) => {
                let n = graph.nodes.len();
                let e = graph.edges.len();
                let desc = stale.describe();
                let summary = if desc.is_empty() {
                    format!("{n} nodes · {e} edges")
                } else {
                    format!("{n} nodes · {e} edges · {desc}")
                };
                ui.status.set(summary.clone());
                ui.stale.set(stale);
                ui.focus.set(default_focus(&graph));
                ui.graph.set(Some(Arc::new(graph)));
                ui.pan.set((0.0, 0.0));
                ui.zoom.set(1.0);
                ui.visible.set(true);
                panel.push(smithy_editor::AgentEntry::Notice(format!(
                    "Call graph ready — {summary}"
                )));
            }
            Err(e) => {
                ui.status.set(format!("build failed: {e}"));
                panel.push(smithy_editor::AgentEntry::Error(format!(
                    "Call graph build failed: {e}"
                )));
            }
        }
    });
}

/// Deliver one channel message onto the UI thread without a long-lived Effect.
///
/// Copied from settings: menu actions have no reactive owner, so `Effect::new`
/// created inside them is disposed before the worker replies.
fn poll_once<T: 'static>(rx: crossbeam_channel::Receiver<T>, deliver: impl Fn(T) + 'static) {
    fn tick<T: 'static>(rx: crossbeam_channel::Receiver<T>, deliver: std::rc::Rc<dyn Fn(T)>) {
        floem::action::exec_after(std::time::Duration::from_millis(60), move |_| {
            match rx.try_recv() {
                Ok(value) => deliver(value),
                Err(crossbeam_channel::TryRecvError::Disconnected) => {}
                Err(crossbeam_channel::TryRecvError::Empty) => tick(rx, deliver),
            }
        });
    }
    tick(rx, std::rc::Rc::new(deliver));
}

fn default_focus(graph: &CallGraph) -> Option<u32> {
    if graph.nodes.is_empty() {
        return None;
    }
    let mut best = 0u32;
    let mut best_score = 0usize;
    for i in 0..graph.nodes.len() as u32 {
        let score = graph.callers(i).len() + graph.callees(i).len();
        if score > best_score {
            best_score = score;
            best = i;
        }
    }
    Some(best)
}

/// Clear on project switch so the previous tree's map cannot linger.
pub fn clear(ui: CallGraphUi) {
    ui.graph.set(None);
    ui.focus.set(None);
    ui.status.set(String::new());
    ui.building.set(false);
    ui.pan.set((0.0, 0.0));
    ui.zoom.set(1.0);
    ui.root.set(PathBuf::new());
    ui.stale.set(Staleness::default());
}

// --- layout -----------------------------------------------------------------

const MAX_VISIBLE: usize = 60;
const NODE_H: f64 = 28.0;
const NODE_PAD_X: f64 = 12.0;
const ROW_GAP: f64 = 56.0;
const COL_GAP: f64 = 16.0;

#[derive(Debug, Clone)]
struct LaidOut {
    index: u32,
    label: String,
    location: String,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    stale: bool,
    sites: u32,
    layer: Layer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layer {
    Caller,
    Focus,
    Callee,
}

/// Deterministic focus-relative layout. Callers above, focus centre, callees
/// below. Caps at [`MAX_VISIBLE`].
///
/// `stale` must be precomputed — [`CallGraph::staleness`] walks and hashes the
/// whole tree, and this runs on every paint.
fn layout(
    graph: &CallGraph,
    focus: u32,
    hops: u8,
    stale: &Staleness,
) -> (Vec<LaidOut>, usize) {
    let Some(focus_node) = graph.nodes.get(focus as usize) else {
        return (Vec::new(), 0);
    };

    let mut callers: Vec<(u32, u32)> = graph
        .edges
        .iter()
        .filter(|e| e.to == focus)
        .map(|e| (e.from, e.sites))
        .collect();
    let mut callees: Vec<(u32, u32)> = graph
        .edges
        .iter()
        .filter(|e| e.from == focus)
        .map(|e| (e.to, e.sites))
        .collect();

    callers.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    callees.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

    let mut hop2_callers = Vec::new();
    let mut hop2_callees = Vec::new();
    if hops >= 2 {
        let hop1: std::collections::HashSet<u32> = callers
            .iter()
            .chain(callees.iter())
            .map(|(i, _)| *i)
            .chain(std::iter::once(focus))
            .collect();
        for &(c, _) in &callers {
            for e in graph.edges.iter().filter(|e| e.to == c) {
                if !hop1.contains(&e.from) {
                    hop2_callers.push((e.from, e.sites));
                }
            }
        }
        for &(c, _) in &callees {
            for e in graph.edges.iter().filter(|e| e.from == c) {
                if !hop1.contains(&e.to) {
                    hop2_callees.push((e.to, e.sites));
                }
            }
        }
        hop2_callers.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        hop2_callers.dedup_by_key(|(i, _)| *i);
        hop2_callees.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        hop2_callees.dedup_by_key(|(i, _)| *i);
    }

    let budget_each = (MAX_VISIBLE.saturating_sub(1)) / 2;
    let mut hidden = 0usize;

    let take = |src: &[(u32, u32)], limit: usize, hidden: &mut usize| -> Vec<(u32, u32)> {
        if src.len() > limit {
            *hidden += src.len() - limit;
            src[..limit].to_vec()
        } else {
            src.to_vec()
        }
    };

    let callers_v = take(&callers, budget_each, &mut hidden);
    let callees_v = take(&callees, budget_each, &mut hidden);
    let used = 1 + callers_v.len() + callees_v.len();
    let leftover = MAX_VISIBLE.saturating_sub(used);
    let half = leftover / 2;
    let hop2_c = if hops >= 2 {
        take(&hop2_callers, half, &mut hidden)
    } else {
        Vec::new()
    };
    let hop2_e = if hops >= 2 {
        take(&hop2_callees, leftover.saturating_sub(hop2_c.len()), &mut hidden)
    } else {
        Vec::new()
    };

    let label_w = |n: &Node| -> f64 {
        let label = n.qualified();
        (label.chars().count() as f64 * 7.2 + NODE_PAD_X * 2.0).clamp(72.0, 280.0)
    };

    let place_row = |items: &[(u32, u32)], y: f64, layer: Layer, out: &mut Vec<LaidOut>| {
        if items.is_empty() {
            return;
        }
        let widths: Vec<f64> = items
            .iter()
            .map(|(i, _)| label_w(&graph.nodes[*i as usize]))
            .collect();
        let total: f64 = widths.iter().sum::<f64>() + COL_GAP * (items.len().saturating_sub(1)) as f64;
        let mut x = -total / 2.0;
        for (k, &(idx, sites)) in items.iter().enumerate() {
            let n = &graph.nodes[idx as usize];
            let w = widths[k];
            out.push(LaidOut {
                index: idx,
                label: n.qualified(),
                location: n.location(),
                x,
                y,
                w,
                h: NODE_H,
                stale: graph.node_is_stale(n, stale),
                sites,
                layer,
            });
            x += w + COL_GAP;
        }
    };

    let mut out = Vec::new();
    place_row(&hop2_c, -ROW_GAP * 2.0, Layer::Caller, &mut out);
    place_row(&callers_v, -ROW_GAP, Layer::Caller, &mut out);

    let fw = label_w(focus_node);
    out.push(LaidOut {
        index: focus,
        label: focus_node.qualified(),
        location: focus_node.location(),
        x: -fw / 2.0,
        y: 0.0,
        w: fw,
        h: NODE_H,
        stale: graph.node_is_stale(focus_node, stale),
        sites: 0,
        layer: Layer::Focus,
    });

    place_row(&callees_v, ROW_GAP, Layer::Callee, &mut out);
    place_row(&hop2_e, ROW_GAP * 2.0, Layer::Callee, &mut out);

    (out, hidden)
}

// --- view -------------------------------------------------------------------

/// The center-pane call graph.
pub fn call_graph_view(
    ui: CallGraphUi,
    project_root: RwSignal<PathBuf>,
    on_open: impl Fn(PathBuf, usize) + 'static + Clone,
    on_build: impl Fn() + 'static + Clone,
) -> impl IntoView {
    let on_build_empty = on_build.clone();

    Stack::vertical((
        toolbar(ui, on_build),
        dyn_container(
            move || (ui.graph.get().is_some(), ui.building.get()),
            move |(has, building)| {
                if building && !has {
                    message_pane(
                        "Building call graph…",
                        "rust-analyzer scip · expect ~10 s and ~2 GB RAM",
                    )
                    .into_any()
                } else if !has {
                    empty_pane(on_build_empty.clone()).into_any()
                } else {
                    graph_pane(ui, project_root, on_open.clone()).into_any()
                }
            },
        )
        .style(|s| s.flex_grow(1.0).width_full().min_height(0.0)),
    ))
    .style(|s| {
        s.width_full()
            .height_full()
            .background(design::BG_BASE)
            .min_height(0.0)
    })
}

fn toolbar(ui: CallGraphUi, on_build: impl Fn() + 'static) -> impl IntoView {
    Stack::horizontal((
        Label::derived(|| "Call graph".to_string()).style(|s| {
            s.font_size(design::TEXT_SM)
                .font_bold()
                .color(design::FG)
                .margin_right(design::SPACE_3)
        }),
        Label::derived(move || ui.status.get()).style(move |s| {
            s.font_size(design::TEXT_XS)
                .color(design::FG_FAINT)
                .margin_right(design::SPACE_3)
                .apply_if(ui.status.get().is_empty(), |s| {
                    s.display(floem::taffy::Display::None)
                })
        }),
        Container::new(Empty::new()).style(|s| s.flex_grow(1.0)),
        Label::derived(move || {
            if ui.hops.get() >= 2 {
                "2 hops".to_string()
            } else {
                "1 hop".to_string()
            }
        })
        .on_event_stop(floem::event::listener::Click, move |_, _| {
            ui.hops.update(|h| *h = if *h >= 2 { 1 } else { 2 });
        })
        .style(|s| {
            s.font_size(design::TEXT_XS)
                .color(design::FG_MUTED)
                .margin_right(design::SPACE_3)
                .padding_horiz(design::SPACE_2)
                .padding_vert(2.0)
                .cursor(floem::style::CursorStyle::Pointer)
        }),
        Label::derived(move || {
            if ui.building.get() {
                "Building…".to_string()
            } else {
                "Rebuild".to_string()
            }
        })
        .on_event_stop(floem::event::listener::Click, move |_, _| {
            if !ui.building.get_untracked() {
                on_build();
            }
        })
        .style(move |s| {
            s.font_size(design::TEXT_XS)
                .color(if ui.building.get() {
                    design::FG_MUTED
                } else {
                    design::ACCENT
                })
                .padding_horiz(design::SPACE_2)
                .padding_vert(2.0)
                .cursor(floem::style::CursorStyle::Pointer)
        }),
        Label::derived(|| "Editor".to_string())
            .on_event_stop(floem::event::listener::Click, move |_, _| {
                ui.visible.set(false);
            })
            .style(|s| {
                s.font_size(design::TEXT_XS)
                    .color(design::FG_MUTED)
                    .margin_left(design::SPACE_3)
                    .padding_horiz(design::SPACE_2)
                    .padding_vert(2.0)
                    .cursor(floem::style::CursorStyle::Pointer)
            }),
    ))
    .style(|s| {
        s.width_full()
            .items_center()
            .padding_horiz(design::SPACE_3)
            .padding_vert(design::SPACE_2)
            .border_bottom(1.0)
            .border_color(design::BORDER)
    })
}

fn empty_pane(on_build: impl Fn() + 'static) -> impl IntoView {
    Stack::vertical((
        Label::derived(|| "No call graph yet".to_string()).style(|s| {
            s.font_size(design::TEXT_LG)
                .color(design::FG_MUTED)
                .margin_bottom(design::SPACE_3)
        }),
        Label::derived(|| {
            "A compiler-resolved map of who calls whom.\nCosts ~10 seconds and ~2 GB while rust-analyzer indexes."
                .to_string()
        })
        .style(|s| {
            s.font_size(design::TEXT_SM)
                .color(design::FG_FAINT)
                .margin_bottom(design::SPACE_5)
        }),
        Label::derived(|| "Build Call Graph".to_string())
            .on_event_stop(floem::event::listener::Click, move |_, _| on_build())
            .style(|s| {
                s.font_size(design::TEXT_SM)
                    .color(design::ON_ACCENT)
                    .background(design::ACCENT)
                    .padding_horiz(design::SPACE_4)
                    .padding_vert(design::SPACE_2)
                    .border_radius(design::RADIUS_SM)
                    .cursor(floem::style::CursorStyle::Pointer)
            }),
    ))
    .style(|s| s.width_full().height_full().items_center().justify_center())
}

fn message_pane(title: &'static str, detail: &'static str) -> impl IntoView {
    Stack::vertical((
        Label::derived(move || title.to_string()).style(|s| {
            s.font_size(design::TEXT_LG)
                .color(design::FG_MUTED)
                .margin_bottom(design::SPACE_2)
        }),
        Label::derived(move || detail.to_string())
            .style(|s| s.font_size(design::TEXT_SM).color(design::FG_FAINT)),
    ))
    .style(|s| s.width_full().height_full().items_center().justify_center())
}

fn graph_pane(
    ui: CallGraphUi,
    project_root: RwSignal<PathBuf>,
    on_open: impl Fn(PathBuf, usize) + 'static + Clone,
) -> impl IntoView {
    let edges = canvas(move |cx, size| {
        // Only notify when the pane actually resized. Writing size on every
        // paint fed `dyn_container` below and froze the UI (Not Responding).
        let (w, h) = (size.width, size.height);
        let prev = ui.size.get_untracked();
        if (prev.0 - w).abs() > 0.5 || (prev.1 - h).abs() > 0.5 {
            ui.size.set((w, h));
        }
        if w < 8.0 || h < 8.0 {
            return;
        }
        let Some(graph) = ui.graph.get() else {
            return;
        };
        let Some(focus) = ui.focus.get().or_else(|| default_focus(&graph)) else {
            return;
        };
        let stale = ui.stale.get();
        let (nodes, _) = layout(&graph, focus, ui.hops.get(), &stale);
        let (pan_x, pan_y) = ui.pan.get();
        let zoom = ui.zoom.get().clamp(0.4, 3.0);
        let ox = w / 2.0 + pan_x;
        let oy = h / 2.0 + pan_y;
        let to_screen = |x: f64, y: f64| Point::new(ox + x * zoom, oy + y * zoom);

        let Some(focus_laid) = nodes.iter().find(|n| n.layer == Layer::Focus) else {
            return;
        };
        let f = to_screen(
            focus_laid.x + focus_laid.w / 2.0,
            focus_laid.y + focus_laid.h / 2.0,
        );
        for n in &nodes {
            if n.layer == Layer::Focus {
                continue;
            }
            let p = to_screen(n.x + n.w / 2.0, n.y + n.h / 2.0);
            let thickness = (1.0 + (n.sites as f64).ln().max(0.0)).clamp(1.0, 4.0) * zoom;
            let color = if n.stale {
                design::FG_GHOST.with_alpha(0.5)
            } else {
                design::BORDER.with_alpha(0.85)
            };
            let stroke = if n.stale {
                Stroke::new(thickness).with_dashes(0.0, [4.0, 4.0])
            } else {
                Stroke::new(thickness)
            };
            cx.stroke(&Line::new(f, p), color, &stroke);
        }
    })
    .style(|s| s.absolute().inset(0.0).width_full().height_full().pointer_events_none());

    let nodes_layer = dyn_container(
        move || {
            (
                ui.focus.get(),
                ui.hops.get(),
                ui.pan.get(),
                ui.zoom.get(),
                ui.size.get(),
                project_root.get(),
                ui.graph.get().as_ref().map(|g| g.nodes.len()),
            )
        },
        move |_| {
            let Some(graph) = ui.graph.get_untracked() else {
                return Empty::new().into_any();
            };
            let focus = ui
                .focus
                .get_untracked()
                .or_else(|| default_focus(&graph))
                .unwrap_or(0);
            let stale = ui.stale.get_untracked();
            let (nodes, hidden) = layout(&graph, focus, ui.hops.get_untracked(), &stale);
            let (pan_x, pan_y) = ui.pan.get_untracked();
            let zoom = ui.zoom.get_untracked().clamp(0.4, 3.0);
            let (pw, ph) = ui.size.get_untracked();
            let ox = pw / 2.0 + pan_x;
            let oy = ph / 2.0 + pan_y;

            let mut children: Vec<_> = nodes
                .into_iter()
                .map(|n| {
                    let idx = n.index;
                    let label = n.label.clone();
                    let location = n.location.clone();
                    let is_focus = n.layer == Layer::Focus;
                    let stale = n.stale;
                    let left = ox + n.x * zoom;
                    let top = oy + n.y * zoom;
                    let width = n.w * zoom;
                    let height = n.h * zoom;
                    let on_open = on_open.clone();
                    let last = RwSignal::new(
                        std::time::Instant::now() - std::time::Duration::from_secs(1),
                    );

                    Label::derived(move || label.clone())
                        .on_event_stop(floem::event::listener::Click, move |_, _| {
                            let now = std::time::Instant::now();
                            let double =
                                now.duration_since(last.get_untracked()).as_millis() < 400;
                            last.set(now);
                            if double {
                                let root = project_root.get_untracked();
                                if let Some(graph) = ui.graph.get_untracked() {
                                    if let Some(node) = graph.nodes.get(idx as usize) {
                                        on_open(root.join(&node.file), node.line);
                                        ui.visible.set(false);
                                    }
                                }
                            } else {
                                ui.focus.set(Some(idx));
                                ui.pan.set((0.0, 0.0));
                            }
                        })
                        .style(move |s| {
                            s.absolute()
                                .inset_left(left)
                                .inset_top(top)
                                .width(width)
                                .height(height)
                                .font_size((design::TEXT_XS as f64 * zoom).clamp(9.0, 15.0) as f32)
                                .font_family(design::MONO.to_string())
                                .color(if stale {
                                    design::FG_GHOST
                                } else if is_focus {
                                    design::FG
                                } else {
                                    design::FG_MUTED
                                })
                                .items_center()
                                .justify_center()
                                .background(if is_focus {
                                    design::BG_FLOAT
                                } else {
                                    design::BG_RAISED.with_alpha(if stale { 0.5 } else { 1.0 })
                                })
                                .border(if is_focus { 1.5 } else { 1.0 })
                                .border_color(if is_focus {
                                    design::ACCENT
                                } else if stale {
                                    design::FG_GHOST
                                } else {
                                    design::BORDER
                                })
                                .border_radius(4.0)
                                .cursor(floem::style::CursorStyle::Pointer)
                        })
                        .tooltip(move || {
                            let loc = location.clone();
                            Label::derived(move || loc.clone()).style(|s| {
                                s.font_size(design::TEXT_XS)
                                    .font_family(design::MONO.to_string())
                                    .color(design::FG)
                                    .padding(design::SPACE_2)
                            })
                        })
                        .into_any()
                })
                .collect();

            if hidden > 0 {
                children.push(
                    Label::derived(move || format!("+{hidden} more"))
                        .style(|s| {
                            s.absolute()
                                .inset_bottom(design::SPACE_3)
                                .inset_right(design::SPACE_3)
                                .font_size(design::TEXT_XS)
                                .color(design::FG_FAINT)
                        })
                        .into_any(),
                );
            }

            Stack::new(children)
                .style(|s| s.absolute().inset(0.0).width_full().height_full())
                .into_any()
        },
    )
    .style(|s| s.absolute().inset(0.0).width_full().height_full());

    Stack::new((edges, nodes_layer))
        .style(|s| s.width_full().height_full().min_height(0.0))
        .on_event_stop(floem::event::listener::PointerWheel, move |_, update| {
            use floem::event::PointerScrollEventExt;
            let delta = update.resolve_to_points(None, None);
            let mods = update.state.modifiers;
            if mods.contains(floem::prelude::Modifiers::META)
                || mods.contains(floem::prelude::Modifiers::CONTROL)
            {
                ui.zoom.update(|z| {
                    *z = (*z * (1.0 - delta.y * 0.001)).clamp(0.4, 3.0);
                });
            } else {
                ui.pan.update(|(x, y)| {
                    *x += delta.x;
                    *y += delta.y;
                });
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithy_project::callgraph::{BuildStats, Edge};

    fn tiny_graph() -> CallGraph {
        CallGraph {
            version: 1,
            nodes: vec![
                Node {
                    name: "a".into(),
                    container: None,
                    file: "a.rs".into(),
                    line: 1,
                    end_line: 3,
                },
                Node {
                    name: "b".into(),
                    container: Some("T".into()),
                    file: "b.rs".into(),
                    line: 10,
                    end_line: 20,
                },
                Node {
                    name: "c".into(),
                    container: None,
                    file: "c.rs".into(),
                    line: 5,
                    end_line: 8,
                },
            ],
            edges: vec![
                Edge {
                    from: 0,
                    to: 1,
                    sites: 2,
                },
                Edge {
                    from: 1,
                    to: 2,
                    sites: 1,
                },
            ],
            stats: BuildStats::default(),
            built_at: 0,
            sources: Default::default(),
        }
    }

    #[test]
    fn layout_puts_callers_above_and_callees_below() {
        let g = tiny_graph();
        let (nodes, hidden) = layout(&g, 1, 1, &Staleness::default());
        assert_eq!(hidden, 0);
        let focus = nodes.iter().find(|n| n.layer == Layer::Focus).unwrap();
        assert_eq!(focus.label, "T::b");
        let callers: Vec<_> = nodes.iter().filter(|n| n.layer == Layer::Caller).collect();
        let callees: Vec<_> = nodes.iter().filter(|n| n.layer == Layer::Callee).collect();
        assert_eq!(callers.len(), 1);
        assert_eq!(callees.len(), 1);
        assert!(callers[0].y < focus.y);
        assert!(callees[0].y > focus.y);
    }

    #[test]
    fn default_focus_picks_the_busiest_node() {
        assert_eq!(default_focus(&tiny_graph()), Some(1));
    }
}
