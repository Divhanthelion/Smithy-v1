//! Call graph in the center pane — the Benzi-style map.
//!
//! The library half lives in [`smithy_project::callgraph`]. This module is what
//! puts it on screen: build/load wiring, a focus-relative layered layout, and a
//! floem canvas. Never auto-builds — indexing costs ~10 s and ~2.3 GB.

use std::path::PathBuf;
use std::sync::Arc;

use floem::kurbo::{Line, Point, Rect, Stroke};
use floem::prelude::*;
use floem::reactive::{RwSignal, SignalGet, SignalUpdate};
use floem::views::{canvas, Decorators};

use smithy_editor::design;
use smithy_project::callgraph::{CallGraph, Node, Staleness};

use crate::app_state::AgentState;
use crate::runtime;

/// Overview = Benzi-style whole map; Focus = neighborhood of one symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ViewMode {
    Overview,
    Focus,
}

/// Everything the center-pane map needs, held on [`AgentState`].
#[derive(Clone, Copy)]
pub struct CallGraphUi {
    pub graph: RwSignal<Option<Arc<CallGraph>>>,
    /// Epoch for asynchronous load/build ownership. Every new task and clear
    /// advances it, so an older callback cannot mutate a newer project's UI.
    pub task_generation: RwSignal<u64>,
    /// Changes only when a different prepared graph is installed.
    pub graph_generation: RwSignal<u64>,
    pub focus: RwSignal<Option<u32>>,
    /// Prior foci — Back pops. Not pushed on the initial default focus.
    pub history: RwSignal<Vec<u32>>,
    /// Jump-to-symbol filter. Non-empty opens the results strip.
    pub query: RwSignal<String>,
    pub mode: RwSignal<ViewMode>,
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
    /// Staleness content is part of world geometry (chip styling), while its
    /// potentially large vectors do not belong in a layout key.
    pub staleness_generation: RwSignal<u64>,
    /// One immutable world-space result shared by paint, labels and hit targets.
    pub snapshot: RwSignal<Option<Arc<CallGraphSnapshot>>>,
    pub snapshot_generation: RwSignal<u64>,
}

impl CallGraphUi {
    pub fn new() -> Self {
        Self {
            graph: RwSignal::new(None),
            task_generation: RwSignal::new(0),
            graph_generation: RwSignal::new(0),
            focus: RwSignal::new(None),
            history: RwSignal::new(Vec::new()),
            query: RwSignal::new(String::new()),
            mode: RwSignal::new(ViewMode::Overview),
            hops: RwSignal::new(1),
            building: RwSignal::new(false),
            status: RwSignal::new(String::new()),
            visible: RwSignal::new(false),
            pan: RwSignal::new((0.0, 0.0)),
            zoom: RwSignal::new(1.0),
            size: RwSignal::new((0.0, 0.0)),
            root: RwSignal::new(PathBuf::new()),
            stale: RwSignal::new(Staleness::default()),
            staleness_generation: RwSignal::new(0),
            snapshot: RwSignal::new(None),
            snapshot_generation: RwSignal::new(0),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CallGraphTaskStamp {
    root: PathBuf,
    generation: u64,
}

#[derive(Debug)]
struct Stamped<T> {
    stamp: CallGraphTaskStamp,
    result: T,
}

fn canonical_root(root: &std::path::Path) -> PathBuf {
    root.canonicalize().unwrap_or_else(|_| root.to_path_buf())
}

fn advance_task_generation(ui: CallGraphUi) -> u64 {
    let generation = ui
        .task_generation
        .get_untracked()
        .checked_add(1)
        .expect("call-graph task generation exhausted");
    ui.task_generation.set(generation);
    generation
}

fn begin_task(ui: CallGraphUi, root: &std::path::Path) -> CallGraphTaskStamp {
    let root = canonical_root(root);
    let generation = advance_task_generation(ui);
    ui.root.set(root.clone());
    CallGraphTaskStamp { root, generation }
}

fn task_is_current(
    ui: CallGraphUi,
    current_project_root: &std::path::Path,
    stamp: &CallGraphTaskStamp,
) -> bool {
    ui.task_generation.get_untracked() == stamp.generation
        && ui.root.get_untracked() == stamp.root
        && canonical_root(current_project_root) == stamp.root
}

/// Refocus, remembering where we came from so Back works. Always enters Focus.
fn focus_on(ui: CallGraphUi, next: u32) {
    let cur = ui.focus.get_untracked();
    if cur == Some(next) {
        ui.mode.set(ViewMode::Focus);
        return;
    }
    if let Some(cur) = cur {
        ui.history.update(|h| {
            if h.last() != Some(&cur) {
                h.push(cur);
            }
            if h.len() > 64 {
                h.remove(0);
            }
        });
    }
    ui.focus.set(Some(next));
    ui.query.set(String::new());
    ui.mode.set(ViewMode::Focus);
}

fn go_back(ui: CallGraphUi) {
    let prev = ui.history.try_update(|h| h.pop()).flatten();
    if let Some(prev) = prev {
        ui.focus.set(Some(prev));
        ui.mode.set(ViewMode::Focus);
    }
}

fn graph_summary(graph: &CallGraph, stale: &Staleness) -> String {
    let n = graph.nodes.len();
    let e = graph.edges.len();
    let mut files = std::collections::HashSet::new();
    for node in &graph.nodes {
        files.insert(node.file.as_str());
    }
    let f = files.len();
    let desc = stale.describe();
    // ASCII separators only — the mono UI font often lacks middots/arrows.
    let invalid = graph.invalid_edge_count();
    let invalid = (invalid > 0).then(|| {
        format!(
            "{invalid} invalid edge{} skipped",
            if invalid == 1 { "" } else { "s" }
        )
    });
    if desc.is_empty() && invalid.is_none() {
        format!("{n} nodes, {e} edges, {f} files")
    } else {
        let detail = [(!desc.is_empty()).then_some(desc), invalid]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(", ");
        format!("{n} nodes, {e} edges, {f} files, {detail}")
    }
}

type GraphTaskResult = Result<(CallGraph, Staleness), String>;

fn apply_load_result(
    ui: CallGraphUi,
    current_project_root: &std::path::Path,
    stamped: Stamped<GraphTaskResult>,
) -> bool {
    if !task_is_current(ui, current_project_root, &stamped.stamp) {
        return false;
    }
    match stamped.result {
        Ok((graph, stale)) => {
            ui.status.set(graph_summary(&graph, &stale));
            ui.stale.set(stale);
            ui.staleness_generation.update(|generation| *generation += 1);
            ui.history.set(Vec::new());
            ui.query.set(String::new());
            ui.mode.set(ViewMode::Overview);
            ui.focus.set(default_focus(&graph));
            ui.graph.set(Some(Arc::new(graph)));
            ui.graph_generation.update(|generation| *generation += 1);
        }
        Err(error) if error == "none" => {
            ui.graph.set(None);
            ui.graph_generation.update(|generation| *generation += 1);
            ui.focus.set(None);
            ui.history.set(Vec::new());
            ui.query.set(String::new());
            ui.mode.set(ViewMode::Overview);
            ui.status.set(String::new());
            ui.stale.set(Staleness::default());
            ui.staleness_generation.update(|generation| *generation += 1);
        }
        Err(error) => ui.status.set(error),
    }
    true
}

/// Load a previously saved graph for this project, if any.
pub fn load_for_project(agent: &AgentState) {
    let ui = agent.call_graph;
    let stamp = begin_task(ui, &agent.project.borrow().root);
    let path = agent.registry.callgraph_path(&stamp.root);
    let task_root = stamp.root.clone();
    let task_stamp = stamp.clone();
    let project = agent.project.clone();
    let (tx, rx) = crossbeam_channel::bounded::<Stamped<GraphTaskResult>>(1);

    runtime::tokio_runtime().spawn(async move {
        let result = tokio::task::spawn_blocking(move || {
            if !path.exists() {
                return Err("none".into());
            }
            let graph = CallGraph::load(&path)?;
            let stale = graph.staleness(&task_root);
            Ok((graph, stale))
        })
        .await
        .unwrap_or_else(|e| Err(e.to_string()));
        let _ = tx.send(Stamped {
            stamp: task_stamp,
            result,
        });
    });

    // `poll_once`, not `Effect::new`: a menu-triggered Effect has no reactive
    // owner and is disposed before the worker finishes — which is how Build
    // looked like a no-op.
    poll_once(rx, move |stamped| {
        let current_root = project.borrow().root.clone();
        apply_load_result(ui, &current_root, stamped);
    });
}

fn apply_build_result(
    ui: CallGraphUi,
    current_project_root: &std::path::Path,
    stamped: Stamped<GraphTaskResult>,
) -> Option<Result<String, String>> {
    if !task_is_current(ui, current_project_root, &stamped.stamp) {
        return None;
    }
    ui.building.set(false);
    match stamped.result {
        Ok((graph, stale)) => {
            let summary = graph_summary(&graph, &stale);
            ui.status.set(summary.clone());
            ui.stale.set(stale);
            ui.staleness_generation.update(|generation| *generation += 1);
            ui.history.set(Vec::new());
            ui.query.set(String::new());
            ui.mode.set(ViewMode::Overview);
            ui.focus.set(default_focus(&graph));
            ui.graph.set(Some(Arc::new(graph)));
            ui.graph_generation.update(|generation| *generation += 1);
            ui.pan.set((0.0, 0.0));
            ui.zoom.set(1.0);
            ui.visible.set(true);
            Some(Ok(summary))
        }
        Err(error) => {
            ui.status.set(format!("build failed: {error}"));
            Some(Err(error))
        }
    }
}

/// Run `rust-analyzer scip`, assemble, save, and show.
pub fn build(agent: &AgentState) {
    let ui = agent.call_graph;
    if ui.building.get_untracked() {
        return;
    }
    let stamp = begin_task(ui, &agent.project.borrow().root);
    if !task_is_current(ui, &agent.project.borrow().root, &stamp) {
        return;
    }
    ui.building.set(true);
    ui.status.set("building — ~10 s, ~2 GB…".into());
    ui.visible.set(true);
    agent.panel.push(smithy_editor::AgentEntry::Notice(
        "Building call graph — ~10 s, uses ~2 GB…".into(),
    ));

    let path = agent.registry.callgraph_path(&stamp.root);
    let panel = agent.panel;
    let project = agent.project.clone();
    let task_root = stamp.root.clone();
    let task_stamp = stamp.clone();
    let (tx, rx) = crossbeam_channel::bounded::<Stamped<GraphTaskResult>>(1);

    runtime::tokio_runtime().spawn(async move {
        let result = tokio::task::spawn_blocking(move || {
            eprintln!("[callgraph] building for {}…", task_root.display());
            let graph = CallGraph::build(&task_root)?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            graph.save(&path)?;
            let stale = graph.staleness(&task_root);
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
        let _ = tx.send(Stamped {
            stamp: task_stamp,
            result,
        });
    });

    poll_once(rx, move |stamped| {
        let current_root = project.borrow().root.clone();
        match apply_build_result(ui, &current_root, stamped) {
            Some(Ok(summary)) => {
                panel.push(smithy_editor::AgentEntry::Notice(format!(
                    "Call graph ready — {summary}"
                )));
            }
            Some(Err(error)) => {
                panel.push(smithy_editor::AgentEntry::Error(format!(
                    "Call graph build failed: {error}"
                )));
            }
            None => {}
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
    // Prefer a readable neighborhood over the global hub. Degree ~6 is the
    // sweet spot; `execute_command`-style dispatchers score low on purpose.
    let mut best = 0u32;
    let mut best_score = i32::MIN;
    for i in 0..graph.nodes.len() as u32 {
        let d = graph.degree(i) as i32;
        if d == 0 {
            continue;
        }
        let score = if (3..=14).contains(&d) {
            100 - (d - 6).abs()
        } else if d < 3 {
            d
        } else {
            20 - (d - 14).min(20)
        };
        if score > best_score {
            best_score = score;
            best = i;
        }
    }
    if best_score == i32::MIN {
        Some(0)
    } else {
        Some(best)
    }
}

/// Clear on project switch so the previous tree's map cannot linger.
pub fn clear(ui: CallGraphUi) {
    advance_task_generation(ui);
    ui.graph.set(None);
    ui.graph_generation.update(|generation| *generation += 1);
    ui.focus.set(None);
    ui.history.set(Vec::new());
    ui.query.set(String::new());
    ui.mode.set(ViewMode::Overview);
    ui.status.set(String::new());
    ui.building.set(false);
    ui.pan.set((0.0, 0.0));
    ui.zoom.set(1.0);
    ui.root.set(PathBuf::new());
    ui.stale.set(Staleness::default());
    ui.staleness_generation.update(|generation| *generation += 1);
    ui.snapshot.set(None);
    ui.snapshot_generation.update(|generation| *generation += 1);
}

// --- layout -----------------------------------------------------------------

const MAX_PER_LAYER: usize = 18;
const MAX_VISIBLE: usize = 40;
const NODE_H: f64 = 44.0;
const NODE_PAD_X: f64 = 14.0;
/// Vertical gap between hop bands (caller ↔ focus ↔ callee).
const BAND_GAP: f64 = 64.0;
/// Vertical gap between wrapped rows inside one band.
const ROW_STEP: f64 = 52.0;
const COL_GAP: f64 = 14.0;
const FIT_MARGIN: f64 = 36.0;
/// World-space row width when the pane size is not yet known.
const DEFAULT_ROW_WIDTH: f64 = 640.0;

#[derive(Debug, Clone, PartialEq)]
struct LaidOut {
    index: u32,
    label: String,
    /// Second line on the chip — basename:line (always visible; tooltips pile up).
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

fn display_label(node: &Node, focus: &Node) -> String {
    // Same `impl` as the focus → drop the repeated container so a dispatcher
    // of `cmd_*` reads as names, not a wall of `Terminal::`.
    match (&node.container, &focus.container) {
        (Some(c), Some(fc)) if c == fc => node.name.clone(),
        _ => node.qualified(),
    }
}

fn short_location(node: &Node) -> String {
    let base = std::path::Path::new(&node.file)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(&node.file);
    format!("{base}:{}", node.line)
}

fn label_width(label: &str, location: &str) -> f64 {
    let top = label.chars().count() as f64 * 7.4;
    let bot = location.chars().count() as f64 * 6.2;
    (top.max(bot) + NODE_PAD_X * 2.0).clamp(88.0, 260.0)
}

/// Deterministic focus-relative layout. Callers above, focus centre, callees
/// below. High fan-out **wraps** within `max_row_width` instead of one endless
/// strip. `stale` must be precomputed — never call [`CallGraph::staleness`] here.
fn layout(
    graph: &CallGraph,
    focus: u32,
    hops: u8,
    stale: &Staleness,
    max_row_width: f64,
) -> (Vec<LaidOut>, usize) {
    let Some(focus_node) = graph.nodes.get(focus as usize) else {
        return (Vec::new(), 0);
    };

    let mut callers: Vec<(u32, u32)> = graph
        .incoming(focus)
        .iter()
        .map(|edge| (edge.node, edge.sites))
        .collect();
    let mut callees: Vec<(u32, u32)> = graph
        .outgoing(focus)
        .iter()
        .map(|edge| (edge.node, edge.sites))
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
            for edge in graph.incoming(c) {
                if !hop1.contains(&edge.node) {
                    hop2_callers.push((edge.node, edge.sites));
                }
            }
        }
        for &(c, _) in &callees {
            for edge in graph.outgoing(c) {
                if !hop1.contains(&edge.node) {
                    hop2_callees.push((edge.node, edge.sites));
                }
            }
        }
        hop2_callers.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        hop2_callers.dedup_by_key(|(i, _)| *i);
        hop2_callees.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        hop2_callees.dedup_by_key(|(i, _)| *i);
    }

    let mut hidden = 0usize;
    let take = |src: &[(u32, u32)], limit: usize, hidden: &mut usize| -> Vec<(u32, u32)> {
        if src.len() > limit {
            *hidden += src.len() - limit;
            src[..limit].to_vec()
        } else {
            src.to_vec()
        }
    };

    let callers_v = take(&callers, MAX_PER_LAYER, &mut hidden);
    let callees_v = take(&callees, MAX_PER_LAYER, &mut hidden);
    let used = 1 + callers_v.len() + callees_v.len();
    let leftover = MAX_VISIBLE.saturating_sub(used);
    let half = leftover / 2;
    let hop2_c = if hops >= 2 {
        take(&hop2_callers, half.min(MAX_PER_LAYER / 2), &mut hidden)
    } else {
        Vec::new()
    };
    let hop2_e = if hops >= 2 {
        take(
            &hop2_callees,
            leftover.saturating_sub(hop2_c.len()).min(MAX_PER_LAYER / 2),
            &mut hidden,
        )
    } else {
        Vec::new()
    };

    let max_w = max_row_width.max(200.0);
    let mut out = Vec::new();

    // Place from focus outward so band gaps stay consistent after wrapping.
    let fw = label_width(&focus_node.qualified(), &short_location(focus_node));
    out.push(LaidOut {
        index: focus,
        label: focus_node.qualified(),
        location: short_location(focus_node),
        x: -fw / 2.0,
        y: 0.0,
        w: fw,
        h: NODE_H,
        stale: graph.node_is_stale(focus_node, stale),
        sites: 0,
        layer: Layer::Focus,
    });

    let place_wrapped = |items: &[(u32, u32)],
                         y0: f64,
                         downward: bool,
                         layer: Layer,
                         out: &mut Vec<LaidOut>| {
        if items.is_empty() {
            return;
        }
        let step = if downward { ROW_STEP } else { -ROW_STEP };
        let mut row: Vec<(u32, u32, f64)> = Vec::new();
        let mut row_w = 0.0;
        let mut y = y0;

        let flush = |row: &mut Vec<(u32, u32, f64)>, y: f64, out: &mut Vec<LaidOut>| {
            if row.is_empty() {
                return;
            }
            let total: f64 = row.iter().map(|(_, _, w)| *w).sum::<f64>()
                + COL_GAP * (row.len().saturating_sub(1)) as f64;
            let mut x = -total / 2.0;
            for &(idx, sites, w) in row.iter() {
                let n = &graph.nodes[idx as usize];
                let label = display_label(n, focus_node);
                out.push(LaidOut {
                    index: idx,
                    label,
                    location: short_location(n),
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
            row.clear();
        };

        for &(idx, sites) in items {
            let n = &graph.nodes[idx as usize];
            let label = display_label(n, focus_node);
            let w = label_width(&label, &short_location(n));
            let next = if row.is_empty() {
                w
            } else {
                row_w + COL_GAP + w
            };
            if !row.is_empty() && next > max_w {
                flush(&mut row, y, out);
                row_w = 0.0;
                y += step;
            }
            row.push((idx, sites, w));
            row_w = if row.len() == 1 {
                w
            } else {
                row_w + COL_GAP + w
            };
        }
        flush(&mut row, y, out);
    };

    // How tall did a wrapped band grow? Used to offset hop-2 away from hop-1.
    let band_depth = |items: &[(u32, u32)]| -> f64 {
        if items.is_empty() {
            return 0.0;
        }
        let mut rows = 1usize;
        let mut row_w = 0.0;
        for &(idx, _) in items {
            let n = &graph.nodes[idx as usize];
            let label = display_label(n, focus_node);
            let w = label_width(&label, &short_location(n));
            let next = if row_w == 0.0 { w } else { row_w + COL_GAP + w };
            if row_w > 0.0 && next > max_w {
                rows += 1;
                row_w = w;
            } else {
                row_w = next;
            }
        }
        (rows.saturating_sub(1)) as f64 * ROW_STEP
    };

    let callee_depth = band_depth(&callees_v);
    let caller_depth = band_depth(&callers_v);

    place_wrapped(
        &callers_v,
        -BAND_GAP,
        false,
        Layer::Caller,
        &mut out,
    );
    place_wrapped(
        &hop2_c,
        -(BAND_GAP * 2.0 + caller_depth),
        false,
        Layer::Caller,
        &mut out,
    );
    place_wrapped(&callees_v, BAND_GAP, true, Layer::Callee, &mut out);
    place_wrapped(
        &hop2_e,
        BAND_GAP * 2.0 + callee_depth,
        true,
        Layer::Callee,
        &mut out,
    );

    (out, hidden)
}

/// Pan/zoom so the laid-out neighborhood fits in the pane with margin.
fn fit_camera(nodes: &[LaidOut], pane_w: f64, pane_h: f64) -> ((f64, f64), f64) {
    if nodes.is_empty() {
        return ((0.0, 0.0), 1.0);
    }
    let mut min_x = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    for n in nodes {
        min_x = min_x.min(n.x);
        max_x = max_x.max(n.x + n.w);
        min_y = min_y.min(n.y);
        max_y = max_y.max(n.y + n.h);
    }
    fit_bounds(min_x, max_x, min_y, max_y, pane_w, pane_h, 0.55, 1.35)
}

// Bounds, viewport, and zoom limits are kept explicit because tests vary each
// independently; grouping them would make those geometry cases less legible.
#[allow(clippy::too_many_arguments)]
fn fit_bounds(
    min_x: f64,
    max_x: f64,
    min_y: f64,
    max_y: f64,
    pane_w: f64,
    pane_h: f64,
    zoom_min: f64,
    zoom_max: f64,
) -> ((f64, f64), f64) {
    if pane_w < 40.0 || pane_h < 40.0 {
        return ((0.0, 0.0), 1.0);
    }
    let bw = (max_x - min_x).max(1.0);
    let bh = (max_y - min_y).max(1.0);
    let zoom = ((pane_w - 2.0 * FIT_MARGIN) / bw)
        .min((pane_h - 2.0 * FIT_MARGIN) / bh)
        .clamp(zoom_min, zoom_max);
    let cx = (min_x + max_x) / 2.0;
    let cy = (min_y + max_y) / 2.0;
    ((-cx * zoom, -cy * zoom), zoom)
}

fn row_width_for_pane(pane_w: f64) -> f64 {
    if pane_w < 40.0 {
        DEFAULT_ROW_WIDTH
    } else {
        (pane_w - 2.0 * FIT_MARGIN).clamp(280.0, 720.0)
    }
}

// --- overview layout (file clusters) ----------------------------------------

const OV_CHIP_H: f64 = 18.0;
const OV_CHIP_PAD_X: f64 = 6.0;
const OV_COL_GAP: f64 = 4.0;
const OV_ROW_STEP: f64 = 21.0;
const OV_CLUSTER_PAD: f64 = 6.0;
const OV_TITLE_H: f64 = 16.0;
const OV_CLUSTER_GAP: f64 = 14.0;
/// Narrowest cluster column — packs more columns across the pane.
const OV_MIN_COL: f64 = 135.0;
/// Below this zoom, chips render as dots (labels only on titles).
const OV_LABEL_ZOOM: f64 = 0.55;

#[derive(Debug, Clone, PartialEq)]
struct ClusterBox {
    title: String,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

#[derive(Debug, Clone, PartialEq)]
struct OverviewChip {
    index: u32,
    label: String,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    stale: bool,
    /// Index into the clusters vec — used by tests / future hull highlighting.
    #[allow(dead_code)]
    cluster: usize,
}

fn file_basename(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(path)
        .to_string()
}

fn overview_chip_width(label: &str) -> f64 {
    (label.chars().count() as f64 * 5.8 + OV_CHIP_PAD_X * 2.0).clamp(32.0, 120.0)
}

/// Pane width available for the overview grid (uses the real center pane).
fn overview_grid_width(pane_w: f64) -> f64 {
    if pane_w < 40.0 {
        900.0
    } else {
        // Leave a little margin but use nearly the full center pane.
        (pane_w - 2.0 * FIT_MARGIN).clamp(480.0, 1800.0)
    }
}

fn measure_cluster_body(items: &[(u32, String, f64)], inner_w: f64) -> (f64, f64) {
    if items.is_empty() {
        let cw = inner_w + OV_CLUSTER_PAD * 2.0;
        let ch = OV_TITLE_H + OV_CLUSTER_PAD * 2.0;
        return (cw, ch);
    }
    let mut rows = 1usize;
    let mut row_w = 0.0;
    for &(_, _, w) in items {
        let next = if row_w == 0.0 {
            w
        } else {
            row_w + OV_COL_GAP + w
        };
        if row_w > 0.0 && next > inner_w {
            rows += 1;
            row_w = w;
        } else {
            row_w = next;
        }
    }
    let content_h = rows as f64 * OV_ROW_STEP;
    let cw = inner_w + OV_CLUSTER_PAD * 2.0;
    let ch = OV_TITLE_H + OV_CLUSTER_PAD + content_h + OV_CLUSTER_PAD;
    (cw, ch)
}

/// Fill the pane: as many columns as `OV_MIN_COL` allows, up to file count.
fn pick_overview_columns(n_files: usize, grid_w: f64) -> usize {
    if n_files == 0 {
        return 1;
    }
    let max_by_width =
        ((grid_w + OV_CLUSTER_GAP) / (OV_MIN_COL + OV_CLUSTER_GAP)).floor() as usize;
    max_by_width.clamp(1, n_files).min(12)
}

/// Benzi-style whole-map layout: one box per source file, every symbol as a chip.
///
/// Packs into as many columns as the pane can hold (fills width), masonry-
/// balances column heights, and degree-sorts chips so hubs lead each cluster.
fn overview_layout(
    graph: &CallGraph,
    stale: &Staleness,
    grid_w: f64,
) -> (Vec<ClusterBox>, Vec<OverviewChip>) {
    let mut by_file: std::collections::BTreeMap<&str, Vec<u32>> =
        std::collections::BTreeMap::new();
    for (i, n) in graph.nodes.iter().enumerate() {
        by_file.entry(n.file.as_str()).or_default().push(i as u32);
    }
    let mut files: Vec<(&str, Vec<u32>)> = by_file.into_iter().collect();
    files.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(b.0)));

    let n_files = files.len();
    let cols = pick_overview_columns(n_files, grid_w).max(1);
    // Stretch columns so the packed grid spans the full grid width.
    let col_w = if cols == 1 {
        grid_w.max(OV_MIN_COL)
    } else {
        ((grid_w - (cols as f64 - 1.0) * OV_CLUSTER_GAP) / cols as f64).max(OV_MIN_COL)
    };
    let inner_w = (col_w - OV_CLUSTER_PAD * 2.0).max(80.0);

    struct Measured {
        title: String,
        items: Vec<(u32, String, f64)>,
        ch: f64,
    }
    let mut measured: Vec<Measured> = Vec::with_capacity(n_files);
    for (file, indices) in &files {
        let mut ranked: Vec<(u32, usize)> = indices
            .iter()
            .map(|&i| {
                let d = graph.degree(i);
                (i, d)
            })
            .collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let items: Vec<(u32, String, f64)> = ranked
            .iter()
            .map(|&(i, _)| {
                let n = &graph.nodes[i as usize];
                let label = n.name.clone();
                let w = overview_chip_width(&label);
                (i, label, w)
            })
            .collect();
        let (_cw, ch) = measure_cluster_body(&items, inner_w);
        let title = format!("{} ({})", file_basename(file), indices.len());
        measured.push(Measured { title, items, ch });
    }

    // Column pack: place heaviest clusters into the shortest column.
    let mut col_heights = vec![0.0_f64; cols];
    let mut col_stacks: Vec<Vec<usize>> = vec![Vec::new(); cols];
    let mut order: Vec<usize> = (0..measured.len()).collect();
    order.sort_by(|&a, &b| {
        measured[b]
            .ch
            .partial_cmp(&measured[a].ch)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    for idx in order {
        let j = (0..cols)
            .min_by(|a, b| {
                col_heights[*a]
                    .partial_cmp(&col_heights[*b])
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .unwrap_or(0);
        col_heights[j] += measured[idx].ch + OV_CLUSTER_GAP;
        col_stacks[j].push(idx);
    }

    let mut clusters = Vec::new();
    let mut chips = Vec::new();
    let mut placed_at = vec![(0.0_f64, 0.0_f64); measured.len()];

    for (col, stack) in col_stacks.iter().enumerate() {
        let mut y = 0.0;
        let x = col as f64 * (col_w + OV_CLUSTER_GAP);
        for &mi in stack {
            placed_at[mi] = (x, y);
            y += measured[mi].ch + OV_CLUSTER_GAP;
        }
    }

    for (mi, m) in measured.iter().enumerate() {
        let (cx, cy) = placed_at[mi];
        let cluster_idx = clusters.len();
        clusters.push(ClusterBox {
            title: m.title.clone(),
            x: cx,
            y: cy,
            w: col_w,
            h: m.ch,
        });

        let mut x = cx + OV_CLUSTER_PAD;
        let mut y = cy + OV_TITLE_H + OV_CLUSTER_PAD;
        let mut row_w = 0.0;
        let body_w = col_w - OV_CLUSTER_PAD * 2.0;
        for &(idx, ref label, w) in &m.items {
            let next = if row_w == 0.0 {
                w
            } else {
                row_w + OV_COL_GAP + w
            };
            if row_w > 0.0 && next > body_w {
                x = cx + OV_CLUSTER_PAD;
                y += OV_ROW_STEP;
                row_w = 0.0;
            }
            let n = &graph.nodes[idx as usize];
            chips.push(OverviewChip {
                index: idx,
                label: label.clone(),
                x,
                y,
                w,
                h: OV_CHIP_H,
                stale: graph.node_is_stale(n, stale),
                cluster: cluster_idx,
            });
            x += w + OV_COL_GAP;
            row_w = if row_w == 0.0 {
                w
            } else {
                row_w + OV_COL_GAP + w
            };
        }
    }

    // Center around origin for the camera.
    if !clusters.is_empty() {
        let min_x = clusters.iter().map(|c| c.x).fold(f64::INFINITY, f64::min);
        let max_x = clusters
            .iter()
            .map(|c| c.x + c.w)
            .fold(f64::NEG_INFINITY, f64::max);
        let min_y = clusters.iter().map(|c| c.y).fold(f64::INFINITY, f64::min);
        let max_y = clusters
            .iter()
            .map(|c| c.y + c.h)
            .fold(f64::NEG_INFINITY, f64::max);
        let ox = (min_x + max_x) / 2.0;
        let oy = (min_y + max_y) / 2.0;
        for c in &mut clusters {
            c.x -= ox;
            c.y -= oy;
        }
        for chip in &mut chips {
            chip.x -= ox;
            chip.y -= oy;
        }
    }

    (clusters, chips)
}

fn fit_overview(
    clusters: &[ClusterBox],
    pane_w: f64,
    pane_h: f64,
) -> ((f64, f64), f64) {
    if clusters.is_empty() {
        return ((0.0, 0.0), 1.0);
    }
    let min_x = clusters.iter().map(|c| c.x).fold(f64::INFINITY, f64::min);
    let max_x = clusters
        .iter()
        .map(|c| c.x + c.w)
        .fold(f64::NEG_INFINITY, f64::max);
    let min_y = clusters.iter().map(|c| c.y).fold(f64::INFINITY, f64::min);
    let max_y = clusters
        .iter()
        .map(|c| c.y + c.h)
        .fold(f64::NEG_INFINITY, f64::max);
    // Prefer fitting the whole map readable; allow a bit lower so wide grids fill.
    fit_bounds(min_x, max_x, min_y, max_y, pane_w, pane_h, 0.28, 1.2)
}

/// Inputs that can change world-space call-graph geometry.
///
/// Camera state is deliberately absent: panning and zooming only transform an
/// existing snapshot. Pane dimensions are bucketed to ignore subpixel layout
/// noise that otherwise rebuilt thousands of labels for identical geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallGraphLayoutKey {
    graph_generation: u64,
    mode: ViewMode,
    focus: Option<u32>,
    hops: u8,
    staleness_generation: u64,
    pane_width: u32,
    pane_height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct WorldEdge {
    from: Point,
    to: Point,
    sites: u32,
    stale: bool,
    same_file: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct HitTarget {
    index: u32,
    rect: Rect,
}

#[derive(Debug, Clone, PartialEq)]
struct OverviewSnapshot {
    clusters: Vec<ClusterBox>,
    chips: Vec<OverviewChip>,
    edges: Vec<WorldEdge>,
    hits: Vec<HitTarget>,
}

#[derive(Debug, Clone, PartialEq)]
struct FocusSnapshot {
    nodes: Vec<LaidOut>,
    edges: Vec<WorldEdge>,
    hits: Vec<HitTarget>,
    hidden: usize,
}

#[derive(Debug, Clone, PartialEq)]
enum SnapshotContent {
    Overview(OverviewSnapshot),
    Focus(FocusSnapshot),
}

/// Immutable world-space geometry consumed by every visual and hit layer.
#[derive(Debug, Clone, PartialEq)]
pub struct CallGraphSnapshot {
    key: CallGraphLayoutKey,
    content: SnapshotContent,
    fit_pan: (f64, f64),
    fit_zoom: f64,
}

fn pane_bucket(value: f64) -> u32 {
    ((value.max(0.0) / 2.0).round() * 2.0) as u32
}

fn layout_key(ui: CallGraphUi) -> CallGraphLayoutKey {
    let (width, height) = ui.size.get();
    CallGraphLayoutKey {
        graph_generation: ui.graph_generation.get(),
        mode: ui.mode.get(),
        focus: ui.focus.get(),
        hops: ui.hops.get(),
        staleness_generation: ui.staleness_generation.get(),
        pane_width: pane_bucket(width),
        pane_height: pane_bucket(height),
    }
}

fn build_snapshot(
    graph: &CallGraph,
    stale: &Staleness,
    key: CallGraphLayoutKey,
) -> Option<CallGraphSnapshot> {
    let pane_w = key.pane_width as f64;
    let pane_h = key.pane_height as f64;
    if pane_w < 40.0 || pane_h < 40.0 {
        return None;
    }
    match key.mode {
        ViewMode::Overview => {
            let (clusters, chips) =
                overview_layout(graph, stale, overview_grid_width(pane_w));
            let mut centers = vec![None; graph.nodes.len()];
            let hits = chips
                .iter()
                .map(|chip| {
                    centers[chip.index as usize] =
                        Some(Point::new(chip.x + chip.w / 2.0, chip.y + chip.h / 2.0));
                    HitTarget {
                        index: chip.index,
                        rect: Rect::new(
                            chip.x,
                            chip.y,
                            chip.x + chip.w,
                            chip.y + chip.h,
                        ),
                    }
                })
                .collect();
            let edges = graph
                .edges
                .iter()
                .filter_map(|edge| {
                    let from = *centers.get(edge.from as usize)?.as_ref()?;
                    let to = *centers.get(edge.to as usize)?.as_ref()?;
                    let from_node = graph.nodes.get(edge.from as usize)?;
                    let to_node = graph.nodes.get(edge.to as usize)?;
                    Some(WorldEdge {
                        from,
                        to,
                        sites: edge.sites,
                        stale: false,
                        same_file: from_node.file == to_node.file,
                    })
                })
                .collect();
            let (fit_pan, fit_zoom) = fit_overview(&clusters, pane_w, pane_h);
            Some(CallGraphSnapshot {
                key,
                content: SnapshotContent::Overview(OverviewSnapshot {
                    clusters,
                    chips,
                    edges,
                    hits,
                }),
                fit_pan,
                fit_zoom,
            })
        }
        ViewMode::Focus => {
            let focus = key.focus.or_else(|| default_focus(graph))?;
            let (nodes, hidden) = layout(
                graph,
                focus,
                key.hops,
                stale,
                row_width_for_pane(pane_w),
            );
            let focus_node = nodes.iter().find(|node| node.layer == Layer::Focus)?;
            let focus_top = Point::new(focus_node.x + focus_node.w / 2.0, focus_node.y);
            let focus_bottom = Point::new(
                focus_node.x + focus_node.w / 2.0,
                focus_node.y + focus_node.h,
            );
            let mut edges = Vec::new();
            for (layer, focus_point, toward_down) in [
                (Layer::Caller, focus_top, false),
                (Layer::Callee, focus_bottom, true),
            ] {
                let group: Vec<&LaidOut> =
                    nodes.iter().filter(|node| node.layer == layer).collect();
                if group.is_empty() {
                    continue;
                }
                let endpoints: Vec<Point> = group
                    .iter()
                    .map(|node| {
                        Point::new(
                            node.x + node.w / 2.0,
                            if toward_down { node.y } else { node.y + node.h },
                        )
                    })
                    .collect();
                let bus_y = if toward_down {
                    (focus_point.y
                        + endpoints.iter().map(|point| point.y).fold(f64::INFINITY, f64::min))
                        / 2.0
                } else {
                    (focus_point.y
                        + endpoints
                            .iter()
                            .map(|point| point.y)
                            .fold(f64::NEG_INFINITY, f64::max))
                        / 2.0
                };
                let min_x = endpoints.iter().map(|point| point.x).fold(f64::INFINITY, f64::min);
                let max_x = endpoints
                    .iter()
                    .map(|point| point.x)
                    .fold(f64::NEG_INFINITY, f64::max);
                edges.push(WorldEdge {
                    from: focus_point,
                    to: Point::new(focus_point.x, bus_y),
                    sites: 0,
                    stale: false,
                    same_file: false,
                });
                if (max_x - min_x).abs() > 1.0 {
                    edges.push(WorldEdge {
                        from: Point::new(min_x, bus_y),
                        to: Point::new(max_x, bus_y),
                        sites: 0,
                        stale: false,
                        same_file: false,
                    });
                }
                for (node, endpoint) in group.into_iter().zip(endpoints) {
                    edges.push(WorldEdge {
                        from: Point::new(endpoint.x, bus_y),
                        to: endpoint,
                        sites: node.sites,
                        stale: node.stale,
                        same_file: false,
                    });
                }
            }
            let hits = nodes
                .iter()
                .map(|node| HitTarget {
                    index: node.index,
                    rect: Rect::new(node.x, node.y, node.x + node.w, node.y + node.h),
                })
                .collect();
            let (fit_pan, fit_zoom) = fit_camera(&nodes, pane_w, pane_h);
            Some(CallGraphSnapshot {
                key,
                content: SnapshotContent::Focus(FocusSnapshot {
                    nodes,
                    edges,
                    hits,
                    hidden,
                }),
                fit_pan,
                fit_zoom,
            })
        }
    }
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

    // The sole world-layout owner. It lives above mode-specific subtrees, so
    // switching panes cannot dispose it. Pan/zoom are untracked and therefore
    // only transform the immutable result.
    floem::reactive::Effect::new(move |_| {
        let key = layout_key(ui);
        if key.pane_width < 40 || key.pane_height < 40 {
            return;
        }
        if ui
            .snapshot
            .get_untracked()
            .as_ref()
            .is_some_and(|snapshot| snapshot.key == key)
        {
            return;
        }
        let Some(graph) = ui.graph.get_untracked() else {
            ui.snapshot.set(None);
            return;
        };
        let stale = ui.stale.get_untracked();
        let Some(snapshot) = build_snapshot(&graph, &stale, key) else {
            return;
        };
        let fit_pan = snapshot.fit_pan;
        let fit_zoom = snapshot.fit_zoom;
        ui.snapshot.set(Some(Arc::new(snapshot)));
        ui.snapshot_generation.update(|generation| *generation += 1);

        let current_pan = ui.pan.get_untracked();
        let current_zoom = ui.zoom.get_untracked();
        if (current_pan.0 - fit_pan.0).abs() > 0.5
            || (current_pan.1 - fit_pan.1).abs() > 0.5
            || (current_zoom - fit_zoom).abs() > 0.01
        {
            ui.pan.set(fit_pan);
            ui.zoom.set(fit_zoom);
        }
    });

    Stack::vertical((
        toolbar(ui, on_build),
        nav_bar(ui),
        dyn_container(
            move || {
                (
                    ui.graph.get().is_some(),
                    ui.building.get(),
                    ui.mode.get(),
                )
            },
            move |(has, building, mode)| {
                if building && !has {
                    message_pane(
                        "Building call graph…",
                        "rust-analyzer scip · expect ~10 s and ~2 GB RAM",
                    )
                    .into_any()
                } else if !has {
                    empty_pane(on_build_empty.clone()).into_any()
                } else if mode == ViewMode::Overview {
                    overview_pane(ui).into_any()
                } else {
                    focus_pane(ui, project_root, on_open.clone()).into_any()
                }
            },
        )
        .on_event_cont(floem::context::LayoutChangedListener, move |_, layout| {
            let size = layout.new_box.size();
            if size.width <= 0.0 || size.height <= 0.0 {
                return;
            }
            let previous = ui.size.get_untracked();
            if pane_bucket(previous.0) != pane_bucket(size.width)
                || pane_bucket(previous.1) != pane_bucket(size.height)
            {
                ui.size.set((size.width, size.height));
            }
        })
        .style(|s| {
            s.flex_grow(1.0)
                .width_full()
                .min_height(0.0)
                .background(design::BG_BASE)
        }),
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
        Label::derived(move || {
            if ui.mode.get() != ViewMode::Focus {
                return String::new();
            }
            let Some(graph) = ui.graph.get() else {
                return String::new();
            };
            let Some(focus) = ui.focus.get().or_else(|| default_focus(&graph)) else {
                return String::new();
            };
            let callers = graph.incoming(focus).len();
            let callees = graph.outgoing(focus).len();
            format!("{callers} callers / {callees} callees")
        })
        .style(move |s| {
            s.font_size(design::TEXT_XS)
                .color(design::FG_MUTED)
                .margin_right(design::SPACE_3)
                .apply_if(
                    ui.graph.get().is_none() || ui.mode.get() != ViewMode::Focus,
                    |s| s.display(floem::taffy::Display::None),
                )
        }),
        Container::new(Empty::new()).style(|s| s.flex_grow(1.0)),
        Label::derived(move || "Overview".to_string())
            .on_event_stop(floem::event::listener::Click, move |_, _| {
                ui.mode.set(ViewMode::Overview);
            })
            .style(move |s| {
                let active = ui.mode.get() == ViewMode::Overview;
                s.font_size(design::TEXT_XS)
                    .color(if active {
                        design::ACCENT
                    } else {
                        design::FG_MUTED
                    })
                    .margin_right(design::SPACE_2)
                    .padding_horiz(design::SPACE_2)
                    .padding_vert(2.0)
                    .cursor(floem::style::CursorStyle::Pointer)
            }),
        Label::derived(move || "Focus".to_string())
            .on_event_stop(floem::event::listener::Click, move |_, _| {
                ui.mode.set(ViewMode::Focus);
            })
            .style(move |s| {
                let active = ui.mode.get() == ViewMode::Focus;
                s.font_size(design::TEXT_XS)
                    .color(if active {
                        design::ACCENT
                    } else {
                        design::FG_MUTED
                    })
                    .margin_right(design::SPACE_3)
                    .padding_horiz(design::SPACE_2)
                    .padding_vert(2.0)
                    .cursor(floem::style::CursorStyle::Pointer)
            }),
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
        .style(move |s| {
            s.font_size(design::TEXT_XS)
                .color(design::FG_MUTED)
                .margin_right(design::SPACE_3)
                .padding_horiz(design::SPACE_2)
                .padding_vert(2.0)
                .cursor(floem::style::CursorStyle::Pointer)
                .apply_if(ui.mode.get() != ViewMode::Focus, |s| {
                    s.display(floem::taffy::Display::None)
                })
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
            .background(design::BG_BASE)
    })
}

/// Back + jump search + quick hubs. This is how you leave a local pocket.
fn nav_bar(ui: CallGraphUi) -> impl IntoView {
    let jump = TextInput::new(ui.query)
        .placeholder("Jump to symbol or file…")
        .style(|s| {
            s.flex_grow(1.0)
                .min_width(0.0)
                .height(28.0)
                .font_size(design::TEXT_SM)
                .font_family(design::MONO.to_string())
                .color(design::FG)
                .background(design::BG_RAISED)
                .border(1.0)
                .border_color(design::BORDER)
                .border_radius(4.0)
                .padding_horiz(design::SPACE_2)
        });

    let back = Label::derived(move || {
        if ui.history.get().is_empty() {
            "Back".to_string()
        } else {
            format!("Back ({})", ui.history.get().len())
        }
    })
    .on_event_stop(floem::event::listener::Click, move |_, _| go_back(ui))
    .style(move |s| {
        let empty = ui.history.get().is_empty();
        s.font_size(design::TEXT_XS)
            .color(if empty {
                design::FG_GHOST
            } else {
                design::ACCENT
            })
            .padding_horiz(design::SPACE_2)
            .padding_vert(2.0)
            .margin_right(design::SPACE_2)
            .cursor(if empty {
                floem::style::CursorStyle::Default
            } else {
                floem::style::CursorStyle::Pointer
            })
    });

    let row = Stack::horizontal((back, jump)).style(|s| {
        s.width_full()
            .items_center()
            .padding_horiz(design::SPACE_3)
            .padding_vert(design::SPACE_2)
            .background(design::BG_BASE)
    });

    let results = dyn_container(
        move || {
            (
                ui.query.get(),
                ui.graph.get().as_ref().map(|g| g.nodes.len()),
            )
        },
        move |(query, _)| {
            let Some(graph) = ui.graph.get_untracked() else {
                return Empty::new().into_any();
            };
            let hits = jump_hits(&graph, &query);
            if hits.is_empty() {
                return Empty::new().into_any();
            }
            let chips: Vec<_> = hits
                .into_iter()
                .map(|(idx, label)| {
                    Label::derived(move || label.clone())
                        .on_event_stop(floem::event::listener::Click, move |_, _| {
                            focus_on(ui, idx);
                        })
                        .style(|s| {
                            s.font_size(design::TEXT_XS)
                                .font_family(design::MONO.to_string())
                                .color(design::FG_MUTED)
                                .background(design::BG_RAISED)
                                .border(1.0)
                                .border_color(design::BORDER)
                                .border_radius(4.0)
                                .padding_horiz(design::SPACE_2)
                                .padding_vert(2.0)
                                .margin_right(design::SPACE_2)
                                .margin_bottom(design::SPACE_1)
                                .cursor(floem::style::CursorStyle::Pointer)
                        })
                        .into_any()
                })
                .collect();
            Stack::new(chips)
                .style(|s| {
                    s.width_full()
                        .flex_row()
                        .flex_wrap(floem::taffy::FlexWrap::Wrap)
                        .padding_horiz(design::SPACE_3)
                        .padding_bottom(design::SPACE_2)
                        .background(design::BG_BASE)
                        .border_bottom(1.0)
                        .border_color(design::BORDER)
                })
                .into_any()
        },
    );

    Stack::vertical((row, results)).style(move |s| {
        s.width_full().apply_if(ui.graph.get().is_none(), |s| {
            s.display(floem::taffy::Display::None)
        })
    })
}

/// Matches for the jump box. Empty query → a handful of hubs so you can leave
/// a local pocket without knowing a name.
fn jump_hits(graph: &CallGraph, query: &str) -> Vec<(u32, String)> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        let mut scored: Vec<(u32, usize)> = (0..graph.nodes.len() as u32)
            .map(|i| (i, graph.degree(i)))
            .filter(|(_, d)| *d >= 4)
            .collect();
        scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        return scored
            .into_iter()
            .take(8)
            .map(|(i, d)| {
                let n = &graph.nodes[i as usize];
                (i, format!("{} ({d})", n.qualified()))
            })
            .collect();
    }
    if q.chars().count() < 2 {
        return Vec::new();
    }
    let mut hits = Vec::new();
    for (i, n) in graph.nodes.iter().enumerate() {
        let qual = n.qualified();
        let file = n.file.to_lowercase();
        if qual.to_lowercase().contains(&q)
            || n.name.to_lowercase().contains(&q)
            || file.contains(&q)
        {
            hits.push((i as u32, format!("{}  {}", qual, short_location(n))));
        }
    }
    hits.sort_by(|a, b| a.1.cmp(&b.1));
    hits.truncate(12);
    hits
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
    .style(|s| {
        s.width_full()
            .height_full()
            .items_center()
            .justify_center()
            .background(design::BG_BASE)
    })
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
    .style(|s| {
        s.width_full()
            .height_full()
            .items_center()
            .justify_center()
            .background(design::BG_BASE)
    })
}

fn overview_pane(ui: CallGraphUi) -> impl IntoView {
    let edges = canvas(move |cx, size| {
        let (w, h) = (size.width, size.height);
        if w < 8.0 || h < 8.0 {
            return;
        }
        cx.fill(&Rect::new(0.0, 0.0, w, h), design::BG_BASE, 0.0);

        let Some(snapshot) = ui.snapshot.get() else {
            return;
        };
        let SnapshotContent::Overview(overview) = &snapshot.content else {
            return;
        };
        let (pan_x, pan_y) = ui.pan.get();
        let zoom = ui.zoom.get().clamp(0.2, 2.5);
        let ox = w / 2.0 + pan_x;
        let oy = h / 2.0 + pan_y;
        let to_screen = |x: f64, y: f64| Point::new(ox + x * zoom, oy + y * zoom);
        let show_labels = zoom >= OV_LABEL_ZOOM;

        // Cluster frames.
        let frame = Stroke::new((1.0 * zoom).clamp(0.7, 1.5));
        for c in &overview.clusters {
            let r = Rect::new(
                ox + c.x * zoom,
                oy + c.y * zoom,
                ox + (c.x + c.w) * zoom,
                oy + (c.y + c.h) * zoom,
            );
            cx.fill(&r, design::BG_RAISED.with_alpha(0.4), 0.0);
            cx.stroke(&r, design::BORDER.with_alpha(0.6), &frame);
        }

        // When zoomed out, draw chips as dots in the canvas (labels would smear).
        if !show_labels {
            for chip in &overview.chips {
                let p = to_screen(chip.x + chip.w / 2.0, chip.y + chip.h / 2.0);
                let r = 2.5 * zoom.max(0.5);
                let dot = floem::kurbo::Circle::new(p, r);
                let color = if chip.stale {
                    design::FG_GHOST.with_alpha(0.6)
                } else {
                    design::FG_MUTED.with_alpha(0.85)
                };
                cx.fill(&dot, color, 0.0);
            }
        }

        for edge in &overview.edges {
            let color = if edge.same_file {
                design::BORDER.with_alpha(if show_labels { 0.15 } else { 0.08 })
            } else {
                design::ACCENT.with_alpha(if show_labels { 0.4 } else { 0.25 })
            };
            let thickness = if edge.same_file { 0.6 } else { 1.0 } * zoom.max(0.45);
            cx.stroke(
                &Line::new(
                    to_screen(edge.from.x, edge.from.y),
                    to_screen(edge.to.x, edge.to.y),
                ),
                color,
                &Stroke::new(thickness),
            );
        }
    })
    .style(|s| s.absolute().inset(0.0).width_full().height_full().pointer_events_none());

    let chips_layer = dyn_container(
        move || ui.snapshot_generation.get(),
        move |_| {
            let Some(snapshot) = ui.snapshot.get_untracked() else {
                return Empty::new().into_any();
            };
            let SnapshotContent::Overview(overview) = &snapshot.content else {
                return Empty::new().into_any();
            };

            let mut children: Vec<_> = overview
                .clusters
                .iter()
                .map(|c| {
                    let title = c.title.clone();
                    let (x, y, w) = (c.x, c.y, c.w);
                    Label::derived(move || title.clone())
                        .style(move |s| {
                            let (pane_w, pane_h) = ui.size.get();
                            let (pan_x, pan_y) = ui.pan.get();
                            let zoom = ui.zoom.get().clamp(0.2, 2.5);
                            let left = pane_w / 2.0 + pan_x + x * zoom;
                            let top = pane_h / 2.0 + pan_y + y * zoom;
                            let width = w * zoom;
                            s.absolute()
                                .inset_left(left + 6.0 * zoom)
                                .inset_top(top + 2.0 * zoom)
                                .width((width - 12.0 * zoom).max(20.0))
                                .font_size((10.0 * zoom).clamp(8.0, 12.0) as f32)
                                .font_family(design::MONO.to_string())
                                .color(design::FG_FAINT)
                        })
                        .into_any()
                })
                .collect();

            for (chip, hit) in overview.chips.iter().zip(&overview.hits) {
                let idx = chip.index;
                let label = chip.label.clone();
                let stale_chip = chip.stale;
                debug_assert_eq!(idx, hit.index);
                let (x, y, w, h) = (
                    hit.rect.x0,
                    hit.rect.y0,
                    hit.rect.width(),
                    hit.rect.height(),
                );
                children.push(
                    Label::derived(move || label.clone())
                        .on_event_stop(floem::event::listener::Click, move |_, _| {
                            focus_on(ui, idx);
                        })
                        .style(move |s| {
                            let (pane_w, pane_h) = ui.size.get();
                            let (pan_x, pan_y) = ui.pan.get();
                            let zoom = ui.zoom.get().clamp(0.2, 2.5);
                            let left = pane_w / 2.0 + pan_x + x * zoom;
                            let top = pane_h / 2.0 + pan_y + y * zoom;
                            s.absolute()
                                .inset_left(left)
                                .inset_top(top)
                                .width(w * zoom)
                                .height(h * zoom)
                                .font_size((9.0 * zoom).clamp(7.5, 12.0) as f32)
                                .font_family(design::MONO.to_string())
                                .color(if stale_chip {
                                    design::FG_GHOST
                                } else {
                                    design::FG_MUTED
                                })
                                .items_center()
                                .justify_center()
                                .background(design::BG_FLOAT.with_alpha(if stale_chip {
                                    0.4
                                } else {
                                    0.9
                                }))
                                .border(1.0)
                                .border_color(if stale_chip {
                                    design::FG_GHOST
                                } else {
                                    design::BORDER
                                })
                                .border_radius(3.0)
                                .cursor(floem::style::CursorStyle::Pointer)
                                .apply_if(zoom < OV_LABEL_ZOOM, |s| {
                                    s.display(floem::taffy::Display::None)
                                })
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

    Stack::new((edges, chips_layer))
        .style(|s| {
            s.width_full()
                .height_full()
                .min_height(0.0)
                .background(design::BG_BASE)
        })
        .on_event_stop(floem::event::listener::PointerWheel, move |_, update| {
            use floem::event::PointerScrollEventExt;
            let delta = update.resolve_to_points(None, None);
            let mods = update.state.modifiers;
            if mods.contains(floem::prelude::Modifiers::META)
                || mods.contains(floem::prelude::Modifiers::CONTROL)
            {
                ui.zoom.update(|z| {
                    *z = (*z * (1.0 - delta.y * 0.001)).clamp(0.2, 2.5);
                });
            } else {
                ui.pan.update(|(x, y)| {
                    *x += delta.x;
                    *y += delta.y;
                });
            }
        })
}

fn focus_pane(
    ui: CallGraphUi,
    project_root: RwSignal<PathBuf>,
    on_open: impl Fn(PathBuf, usize) + 'static + Clone,
) -> impl IntoView {
    let edges = canvas(move |cx, size| {
        let (w, h) = (size.width, size.height);
        if w < 8.0 || h < 8.0 {
            return;
        }
        cx.fill(&Rect::new(0.0, 0.0, w, h), design::BG_BASE, 0.0);

        let Some(snapshot) = ui.snapshot.get() else {
            return;
        };
        let SnapshotContent::Focus(focus) = &snapshot.content else {
            return;
        };
        let (pan_x, pan_y) = ui.pan.get();
        let zoom = ui.zoom.get().clamp(0.4, 3.0);
        let ox = w / 2.0 + pan_x;
        let oy = h / 2.0 + pan_y;
        let to_screen = |x: f64, y: f64| Point::new(ox + x * zoom, oy + y * zoom);

        for edge in &focus.edges {
            let thickness = if edge.sites == 0 {
                1.25 * zoom
            } else {
                (1.0 + (edge.sites as f64).ln().max(0.0)).clamp(1.0, 3.5) * zoom
            };
            let color = if edge.stale {
                design::FG_GHOST.with_alpha(0.5)
            } else if edge.sites == 0 {
                design::BORDER.with_alpha(0.75)
            } else {
                design::BORDER.with_alpha(0.9)
            };
            let stroke = if edge.stale {
                Stroke::new(thickness).with_dashes(0.0, [4.0, 4.0])
            } else {
                Stroke::new(thickness)
            };
            cx.stroke(
                &Line::new(
                    to_screen(edge.from.x, edge.from.y),
                    to_screen(edge.to.x, edge.to.y),
                ),
                color,
                &stroke,
            );
        }
    })
    .style(|s| s.absolute().inset(0.0).width_full().height_full().pointer_events_none());

    let nodes_layer = dyn_container(
        move || ui.snapshot_generation.get(),
        move |_| {
            let Some(snapshot) = ui.snapshot.get_untracked() else {
                return Empty::new().into_any();
            };
            let SnapshotContent::Focus(focus) = &snapshot.content else {
                return Empty::new().into_any();
            };

            let mut children: Vec<_> = focus
                .nodes
                .iter()
                .zip(&focus.hits)
                .map(|(n, hit)| {
                    let idx = n.index;
                    let label = n.label.clone();
                    let location = n.location.clone();
                    let is_focus = n.layer == Layer::Focus;
                    let stale = n.stale;
                    debug_assert_eq!(idx, hit.index);
                    let (x, y, w, h) = (
                        hit.rect.x0,
                        hit.rect.y0,
                        hit.rect.width(),
                        hit.rect.height(),
                    );
                    let on_open = on_open.clone();
                    let last = RwSignal::new(
                        std::time::Instant::now() - std::time::Duration::from_secs(1),
                    );

                    let name = Label::derived(move || label.clone()).style(move |s| {
                        let zoom = ui.zoom.get().clamp(0.4, 3.0);
                        s.font_size((design::TEXT_XS as f64 * zoom).clamp(10.0, 14.0) as f32)
                            .font_family(design::MONO.to_string())
                            .color(if stale {
                                design::FG_GHOST
                            } else if is_focus {
                                design::FG
                            } else {
                                design::FG_MUTED
                            })
                    });
                    let loc = Label::derived(move || location.clone()).style(move |s| {
                        let zoom = ui.zoom.get().clamp(0.4, 3.0);
                        s.font_size((9.0 * zoom).clamp(8.0, 11.0) as f32)
                            .font_family(design::MONO.to_string())
                            .color(design::FG_FAINT)
                    });

                    Stack::vertical((name, loc))
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
                                focus_on(ui, idx);
                            }
                        })
                        .style(move |s| {
                            let (pane_w, pane_h) = ui.size.get();
                            let (pan_x, pan_y) = ui.pan.get();
                            let zoom = ui.zoom.get().clamp(0.4, 3.0);
                            let left = pane_w / 2.0 + pan_x + x * zoom;
                            let top = pane_h / 2.0 + pan_y + y * zoom;
                            s.absolute()
                                .inset_left(left)
                                .inset_top(top)
                                .width(w * zoom)
                                .height(h * zoom)
                                .items_center()
                                .justify_center()
                                .padding_horiz(4.0)
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
                        .into_any()
                })
                .collect();

            if focus.hidden > 0 {
                let hidden = focus.hidden;
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
        .style(|s| {
            s.width_full()
                .height_full()
                .min_height(0.0)
                .background(design::BG_BASE)
        })
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
    use smithy_project::callgraph::Edge;

    #[derive(Default)]
    struct SnapshotMemo {
        current: Option<Arc<CallGraphSnapshot>>,
        builds: usize,
    }

    impl SnapshotMemo {
        fn update(
            &mut self,
            graph: &CallGraph,
            stale: &Staleness,
            key: CallGraphLayoutKey,
        ) -> Arc<CallGraphSnapshot> {
            if let Some(current) = &self.current {
                if current.key == key {
                    return current.clone();
                }
            }
            let snapshot = Arc::new(build_snapshot(graph, stale, key).unwrap());
            self.current = Some(snapshot.clone());
            self.builds += 1;
            snapshot
        }
    }

    fn key(mode: ViewMode) -> CallGraphLayoutKey {
        CallGraphLayoutKey {
            graph_generation: 1,
            mode,
            focus: Some(1),
            hops: 1,
            staleness_generation: 1,
            pane_width: 800,
            pane_height: 600,
        }
    }

    fn one_node_graph(name: &str, file: &str) -> CallGraph {
        CallGraph::from_parts(
            vec![Node {
                name: name.into(),
                container: None,
                file: file.into(),
                line: 1,
                end_line: 2,
            }],
            Vec::new(),
        )
    }

    fn tiny_graph() -> CallGraph {
        CallGraph::from_parts(
            vec![
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
            vec![
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
        )
    }

    fn hub_graph(callees: usize) -> CallGraph {
        let mut nodes = vec![Node {
            name: "dispatch".into(),
            container: Some("Terminal".into()),
            file: "t.rs".into(),
            line: 1,
            end_line: 10,
        }];
        let mut edges = Vec::new();
        for i in 0..callees {
            nodes.push(Node {
                name: format!("cmd_{i}"),
                container: Some("Terminal".into()),
                file: "t.rs".into(),
                line: 20 + i,
                end_line: 22 + i,
            });
            edges.push(Edge {
                from: 0,
                to: (i + 1) as u32,
                sites: 1,
            });
        }
        CallGraph::from_parts(nodes, edges)
    }

    #[test]
    fn layout_puts_callers_above_and_callees_below() {
        let g = tiny_graph();
        let (nodes, hidden) = layout(&g, 1, 1, &Staleness::default(), 640.0);
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
    fn high_fanout_wraps_instead_of_one_strip() {
        let g = hub_graph(12);
        let (nodes, _) = layout(&g, 0, 1, &Staleness::default(), 320.0);
        let callees: Vec<_> = nodes.iter().filter(|n| n.layer == Layer::Callee).collect();
        assert_eq!(callees.len(), 12);
        let ys: std::collections::HashSet<i64> = callees
            .iter()
            .map(|n| (n.y * 10.0).round() as i64)
            .collect();
        assert!(
            ys.len() >= 2,
            "expected wrapped rows, got single y band: {ys:?}"
        );
        // Shared container → short labels
        assert!(callees.iter().all(|n| n.label.starts_with("cmd_")));
        assert!(!callees.iter().any(|n| n.label.contains("::")));
    }

    #[test]
    fn fit_camera_keeps_content_inside_the_pane() {
        let g = hub_graph(10);
        let (nodes, _) = layout(&g, 0, 1, &Staleness::default(), 400.0);
        let ((pan_x, pan_y), zoom) = fit_camera(&nodes, 800.0, 600.0);
        let ox = 400.0 + pan_x;
        let oy = 300.0 + pan_y;
        for n in &nodes {
            let left = ox + n.x * zoom;
            let right = ox + (n.x + n.w) * zoom;
            let top = oy + n.y * zoom;
            let bottom = oy + (n.y + n.h) * zoom;
            assert!(left >= -1.0, "left {left}");
            assert!(right <= 801.0, "right {right}");
            assert!(top >= -1.0, "top {top}");
            assert!(bottom <= 601.0, "bottom {bottom}");
        }
    }

    #[test]
    fn default_focus_prefers_a_moderate_neighborhood() {
        // Hub (degree 20) vs a middling node (degree 4) — pick middling.
        let mut nodes = vec![
            Node {
                name: "hub".into(),
                container: None,
                file: "a.rs".into(),
                line: 1,
                end_line: 2,
            },
            Node {
                name: "mid".into(),
                container: None,
                file: "b.rs".into(),
                line: 1,
                end_line: 2,
            },
        ];
        let mut edges = Vec::new();
        for i in 0..20 {
            nodes.push(Node {
                name: format!("leaf{i}"),
                container: None,
                file: "c.rs".into(),
                line: i,
                end_line: i,
            });
            edges.push(Edge {
                from: 0,
                to: (i + 2) as u32,
                sites: 1,
            });
        }
        // mid ↔ four of the leaves
        for i in 0..4 {
            edges.push(Edge {
                from: 1,
                to: (i + 2) as u32,
                sites: 1,
            });
        }
        let g = CallGraph::from_parts(nodes, edges);
        assert_eq!(default_focus(&g), Some(1));
    }

    #[test]
    fn overview_clusters_by_file_and_keeps_every_node() {
        let g = tiny_graph(); // a.rs, b.rs, c.rs
        let (clusters, chips) = overview_layout(&g, &Staleness::default(), 800.0);
        assert!(
            clusters.len() >= 2,
            "expected multiple file clusters, got {}",
            clusters.len()
        );
        assert_eq!(chips.len(), g.nodes.len());
        let mut seen = std::collections::HashSet::new();
        for c in &chips {
            assert!(seen.insert(c.index), "duplicate chip {}", c.index);
        }
        // Cross-file edge 0→1: endpoints in different clusters
        let c0 = chips.iter().find(|c| c.index == 0).unwrap().cluster;
        let c1 = chips.iter().find(|c| c.index == 1).unwrap().cluster;
        assert_ne!(c0, c1);
    }

    #[test]
    fn overview_includes_every_node_and_packs_wide() {
        // 24 files × 20 symbols — must show every chip and fill horizontal space.
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        for f in 0..24u32 {
            let file = format!("mod{f}.rs");
            for s in 0..20u32 {
                let i = nodes.len() as u32;
                nodes.push(Node {
                    name: format!("fn_{f}_{s}"),
                    container: None,
                    file: file.clone(),
                    line: (s + 1) as usize,
                    end_line: (s + 2) as usize,
                });
                if s > 0 {
                    edges.push(Edge {
                        from: i - 1,
                        to: i,
                        sites: 1,
                    });
                }
            }
        }
        let g = CallGraph::from_parts(nodes, edges);
        let grid_w = overview_grid_width(1100.0);
        let (clusters, chips) = overview_layout(&g, &Staleness::default(), grid_w);
        assert_eq!(clusters.len(), 24);
        assert_eq!(chips.len(), g.nodes.len(), "every symbol should be a chip");

        let min_x = clusters.iter().map(|c| c.x).fold(f64::INFINITY, f64::min);
        let max_x = clusters
            .iter()
            .map(|c| c.x + c.w)
            .fold(f64::NEG_INFINITY, f64::max);
        let min_y = clusters.iter().map(|c| c.y).fold(f64::INFINITY, f64::min);
        let max_y = clusters
            .iter()
            .map(|c| c.y + c.h)
            .fold(f64::NEG_INFINITY, f64::max);
        let bw = max_x - min_x;
        let bh = max_y - min_y;
        // Columns stretch to fill grid_w.
        assert!(
            (bw - grid_w).abs() < 2.0,
            "grid should span pane width: bw={bw:.0} grid_w={grid_w:.0}"
        );
        let cols = pick_overview_columns(24, grid_w);
        assert!(
            cols >= 5,
            "expected ≥5 columns for ~1100px pane, got {cols}"
        );
        let aspect = bh / bw.max(1.0);
        assert!(
            aspect < 2.5,
            "overview still too tall: aspect={aspect:.2} ({bw:.0}x{bh:.0})"
        );
    }

    #[test]
    fn overview_summary_mentions_files() {
        let g = tiny_graph();
        let s = graph_summary(&g, &Staleness::default());
        assert!(s.contains("3 files"), "{s}");
        assert!(s.contains("3 nodes"), "{s}");
    }

    /// Corrupt persisted endpoints are skipped by the prepared index, but the
    /// status line must make that loss visible instead of presenting a clean map.
    #[test]
    fn overview_summary_reports_invalid_edges() {
        let graph = CallGraph::from_parts(
            vec![Node {
                name: "a".into(),
                container: None,
                file: "a.rs".into(),
                line: 1,
                end_line: 2,
            }],
            vec![Edge {
                from: 0,
                to: 8,
                sites: 1,
            }],
        );
        let summary = graph_summary(&graph, &Staleness::default());
        assert!(summary.contains("1 invalid edge skipped"), "{summary}");
    }

    /// Snapshotting is a lifecycle change, not a visual one: the cached
    /// overview and focus geometry must be byte-for-byte the old pure layouts.
    #[test]
    fn snapshots_preserve_the_existing_world_geometry() {
        let graph = tiny_graph();
        let stale = Staleness::default();

        let overview = build_snapshot(&graph, &stale, key(ViewMode::Overview)).unwrap();
        let SnapshotContent::Overview(cached_overview) = overview.content else {
            panic!("expected overview");
        };
        let (clusters, chips) = overview_layout(&graph, &stale, overview_grid_width(800.0));
        assert_eq!(cached_overview.clusters, clusters);
        assert_eq!(cached_overview.chips, chips);

        let focus = build_snapshot(&graph, &stale, key(ViewMode::Focus)).unwrap();
        let SnapshotContent::Focus(cached_focus) = focus.content else {
            panic!("expected focus");
        };
        let (nodes, hidden) = layout(&graph, 1, 1, &stale, row_width_for_pane(800.0));
        assert_eq!(cached_focus.nodes, nodes);
        assert_eq!(cached_focus.hidden, hidden);
    }

    /// Camera changes are intentionally absent from `CallGraphLayoutKey`; a
    /// wheel gesture must transform cached geometry, not rebuild the world.
    #[test]
    fn pan_and_zoom_do_not_rebuild_world_geometry() {
        let graph = tiny_graph();
        let mut memo = SnapshotMemo::default();
        let layout_key = key(ViewMode::Overview);
        let first = memo.update(&graph, &Staleness::default(), layout_key);
        let _camera_after_pan_and_zoom = ((83.0, -41.0), 1.7);
        let second = memo.update(&graph, &Staleness::default(), layout_key);
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(memo.builds, 1);
    }

    /// Focus, hop count, and pane geometry all alter world placement and must
    /// invalidate exactly once each.
    #[test]
    fn focus_hops_and_size_rebuild_world_geometry() {
        let graph = tiny_graph();
        let mut memo = SnapshotMemo::default();
        let mut layout_key = key(ViewMode::Focus);
        memo.update(&graph, &Staleness::default(), layout_key);
        layout_key.focus = Some(0);
        memo.update(&graph, &Staleness::default(), layout_key);
        layout_key.hops = 2;
        memo.update(&graph, &Staleness::default(), layout_key);
        layout_key.pane_width = 900;
        memo.update(&graph, &Staleness::default(), layout_key);
        assert_eq!(memo.builds, 4);
    }

    /// Floem can report fractional size jitter for unchanged geometry. Bucketing
    /// prevents that from recreating every chip and writing the same fit camera.
    #[test]
    fn equivalent_pane_geometry_does_not_rebuild() {
        let graph = tiny_graph();
        let mut memo = SnapshotMemo::default();
        let mut first_key = key(ViewMode::Overview);
        first_key.pane_width = pane_bucket(800.1);
        first_key.pane_height = pane_bucket(599.9);
        let first = memo.update(&graph, &Staleness::default(), first_key);
        let mut second_key = first_key;
        second_key.pane_width = pane_bucket(800.8);
        second_key.pane_height = pane_bucket(600.7);
        let second = memo.update(&graph, &Staleness::default(), second_key);
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(memo.builds, 1);
    }

    /// The clickable rectangles and edge endpoints are derived in the same
    /// immutable snapshot consumed by paint; parallel recomputation used to let
    /// labels and lines disagree during resize.
    #[test]
    fn paint_edges_and_hit_targets_share_one_snapshot() {
        let graph = tiny_graph();
        let snapshot =
            build_snapshot(&graph, &Staleness::default(), key(ViewMode::Overview)).unwrap();
        let SnapshotContent::Overview(overview) = snapshot.content else {
            panic!("expected overview");
        };
        for (chip, hit) in overview.chips.iter().zip(&overview.hits) {
            assert_eq!(chip.index, hit.index);
            assert_eq!(
                hit.rect,
                Rect::new(chip.x, chip.y, chip.x + chip.w, chip.y + chip.h)
            );
        }
        let edge = overview.edges.first().unwrap();
        let from = overview
            .chips
            .iter()
            .find(|chip| chip.index == 0)
            .unwrap();
        let to = overview
            .chips
            .iter()
            .find(|chip| chip.index == 1)
            .unwrap();
        assert_eq!(
            edge.from,
            Point::new(from.x + from.w / 2.0, from.y + from.h / 2.0)
        );
        assert_eq!(
            edge.to,
            Point::new(to.x + to.w / 2.0, to.y + to.h / 2.0)
        );
    }

    /// Project B can load faster than project A even though A started first.
    /// A's late graph must not replace B's graph, because its relative node
    /// paths would then be joined to B's root when the user double-clicked.
    #[test]
    fn out_of_order_cross_project_load_drops_the_old_graph() {
        let old = tempfile::tempdir().unwrap();
        let new = tempfile::tempdir().unwrap();
        let old_root = old.path().canonicalize().unwrap();
        let new_root = new.path().canonicalize().unwrap();
        let ui = CallGraphUi::new();

        let old_stamp = begin_task(ui, &old_root);
        let new_stamp = begin_task(ui, &new_root);
        assert!(apply_load_result(
            ui,
            &new_root,
            Stamped {
                stamp: new_stamp,
                result: Ok((
                    one_node_graph("new", "src/new.rs"),
                    Staleness::default()
                )),
            },
        ));
        assert!(!apply_load_result(
            ui,
            &new_root,
            Stamped {
                stamp: old_stamp,
                result: Ok((
                    one_node_graph("old", "src/old.rs"),
                    Staleness::default()
                )),
            },
        ));

        let graph = ui.graph.get_untracked().unwrap();
        assert_eq!(graph.nodes[0].name, "new");
        assert_eq!(ui.root.get_untracked(), new_root);
        assert_eq!(
            ui.root.get_untracked().join(&graph.nodes[0].file),
            new_root.join("src/new.rs")
        );
    }

    /// A build error from the retired project must not stop the current build,
    /// replace its status, hide its graph, or reset its camera.
    #[test]
    fn out_of_order_cross_project_build_drops_the_old_callback() {
        let old = tempfile::tempdir().unwrap();
        let new = tempfile::tempdir().unwrap();
        let old_root = old.path().canonicalize().unwrap();
        let new_root = new.path().canonicalize().unwrap();
        let ui = CallGraphUi::new();

        let old_stamp = begin_task(ui, &old_root);
        ui.building.set(true);
        clear(ui);
        let new_stamp = begin_task(ui, &new_root);
        ui.building.set(true);
        assert!(apply_build_result(
            ui,
            &new_root,
            Stamped {
                stamp: new_stamp,
                result: Ok((
                    one_node_graph("new", "src/new.rs"),
                    Staleness::default()
                )),
            },
        )
        .is_some());
        ui.pan.set((17.0, -9.0));
        ui.zoom.set(1.8);
        let status = ui.status.get_untracked();

        assert!(apply_build_result(
            ui,
            &new_root,
            Stamped {
                stamp: old_stamp,
                result: Err("old project failed".into()),
            },
        )
        .is_none());
        assert_eq!(ui.status.get_untracked(), status);
        assert_eq!(ui.pan.get_untracked(), (17.0, -9.0));
        assert_eq!(ui.zoom.get_untracked(), 1.8);
        assert!(ui.visible.get_untracked());
        assert!(!ui.building.get_untracked());
        assert_eq!(
            ui.graph.get_untracked().unwrap().nodes[0].name,
            "new"
        );
    }
}
