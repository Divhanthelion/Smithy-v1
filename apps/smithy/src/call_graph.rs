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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    Overview,
    Focus,
}

/// Everything the center-pane map needs, held on [`AgentState`].
#[derive(Clone, Copy)]
pub struct CallGraphUi {
    pub graph: RwSignal<Option<Arc<CallGraph>>>,
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
}

impl CallGraphUi {
    pub fn new() -> Self {
        Self {
            graph: RwSignal::new(None),
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
        }
    }
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
    if desc.is_empty() {
        format!("{n} nodes · {e} edges · {f} files")
    } else {
        format!("{n} nodes · {e} edges · {f} files · {desc}")
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
            ui.status.set(graph_summary(&graph, &stale));
            ui.stale.set(stale);
            ui.history.set(Vec::new());
            ui.query.set(String::new());
            ui.mode.set(ViewMode::Overview);
            ui.focus.set(default_focus(&graph));
            ui.graph.set(Some(Arc::new(graph)));
        }
        Err(e) if e == "none" => {
            ui.graph.set(None);
            ui.focus.set(None);
            ui.history.set(Vec::new());
            ui.query.set(String::new());
            ui.mode.set(ViewMode::Overview);
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
                let summary = graph_summary(&graph, &stale);
                ui.status.set(summary.clone());
                ui.stale.set(stale);
                ui.history.set(Vec::new());
                ui.query.set(String::new());
                ui.mode.set(ViewMode::Overview);
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
    // Prefer a readable neighborhood over the global hub. Degree ~6 is the
    // sweet spot; `execute_command`-style dispatchers score low on purpose.
    let degree = |i: u32| graph.callers(i).len() + graph.callees(i).len();
    let mut best = 0u32;
    let mut best_score = i32::MIN;
    for i in 0..graph.nodes.len() as u32 {
        let d = degree(i) as i32;
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
    ui.graph.set(None);
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

#[derive(Debug, Clone)]
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

const OV_CHIP_H: f64 = 22.0;
const OV_CHIP_PAD_X: f64 = 8.0;
const OV_COL_GAP: f64 = 6.0;
const OV_ROW_STEP: f64 = 26.0;
const OV_CLUSTER_PAD: f64 = 10.0;
const OV_TITLE_H: f64 = 18.0;
const OV_CLUSTER_GAP: f64 = 28.0;
const OV_INNER_WIDTH: f64 = 220.0;

#[derive(Debug, Clone)]
struct ClusterBox {
    title: String,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

#[derive(Debug, Clone)]
struct OverviewChip {
    index: u32,
    label: String,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    stale: bool,
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
    (label.chars().count() as f64 * 6.4 + OV_CHIP_PAD_X * 2.0).clamp(40.0, 160.0)
}

/// Benzi-style whole-map layout: one box per source file, chips inside.
fn overview_layout(
    graph: &CallGraph,
    stale: &Staleness,
    max_row_width: f64,
) -> (Vec<ClusterBox>, Vec<OverviewChip>) {
    let mut by_file: std::collections::BTreeMap<&str, Vec<u32>> = std::collections::BTreeMap::new();
    for (i, n) in graph.nodes.iter().enumerate() {
        by_file.entry(n.file.as_str()).or_default().push(i as u32);
    }
    let mut files: Vec<(&str, Vec<u32>)> = by_file.into_iter().map(|(f, v)| (f, v)).collect();
    files.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(b.0)));

    let mut clusters = Vec::new();
    let mut chips = Vec::new();
    // First pass: measure each cluster's content size.
    let mut measured: Vec<(String, String, Vec<(u32, String, f64)>, f64, f64)> = Vec::new();
    for (file, indices) in &files {
        let title = file_basename(file);
        let mut items: Vec<(u32, String, f64)> = indices
            .iter()
            .map(|&i| {
                let n = &graph.nodes[i as usize];
                let label = n.name.clone();
                let w = overview_chip_width(&label);
                (i, label, w)
            })
            .collect();
        items.sort_by(|a, b| a.1.cmp(&b.1));

        let inner_w = OV_INNER_WIDTH;
        let mut rows = 1usize;
        let mut row_w = 0.0;
        for &(_, _, w) in &items {
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
        measured.push(((*file).to_string(), title, items, cw, ch));
    }

    // Grid placement across max_row_width.
    let grid_w = max_row_width.max(OV_INNER_WIDTH + OV_CLUSTER_PAD * 2.0);
    let mut cursor_x = 0.0;
    let mut cursor_y = 0.0;
    let mut row_h = 0.0;

    for (file, title, items, cw, ch) in &measured {
        if cursor_x > 0.0 && cursor_x + cw > grid_w {
            cursor_x = 0.0;
            cursor_y += row_h + OV_CLUSTER_GAP;
            row_h = 0.0;
        }
        let cx = cursor_x;
        let cy = cursor_y;
        let cluster_idx = clusters.len();
        clusters.push(ClusterBox {
            title: title.clone(),
            x: cx,
            y: cy,
            w: *cw,
            h: *ch,
        });

        let inner_w = OV_INNER_WIDTH;
        let mut x = cx + OV_CLUSTER_PAD;
        let mut y = cy + OV_TITLE_H + OV_CLUSTER_PAD;
        let mut row_w = 0.0;
        for &(idx, ref label, w) in items {
            let next = if row_w == 0.0 {
                w
            } else {
                row_w + OV_COL_GAP + w
            };
            if row_w > 0.0 && next > inner_w {
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

        cursor_x += cw + OV_CLUSTER_GAP;
        row_h = row_h.max(*ch);
    }

    // Center the whole grid around the origin for fit_camera.
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
    // Overview is wide — allow zooming further out than Focus.
    fit_bounds(min_x, max_x, min_y, max_y, pane_w, pane_h, 0.22, 1.0)
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
            let callers = graph.callers(focus).len();
            let callees = graph.callees(focus).len();
            format!("{callers}↑ · {callees}↓")
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
            .map(|i| {
                let d = graph.callers(i).len() + graph.callees(i).len();
                (i, d)
            })
            .filter(|(_, d)| *d >= 4)
            .collect();
        scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        return scored
            .into_iter()
            .take(8)
            .map(|(i, d)| {
                let n = &graph.nodes[i as usize];
                (i, format!("{} · {d}", n.qualified()))
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
    floem::reactive::Effect::new(move |_| {
        let _mode = ui.mode.get();
        let (pw, ph) = ui.size.get();
        let Some(graph) = ui.graph.get() else {
            return;
        };
        if pw < 40.0 || ph < 40.0 {
            return;
        }
        let stale = ui.stale.get_untracked();
        let (clusters, _) = overview_layout(&graph, &stale, row_width_for_pane(pw.max(900.0)));
        let (pan, zoom) = fit_overview(&clusters, pw, ph);
        let cur = ui.pan.get_untracked();
        let cz = ui.zoom.get_untracked();
        if (cur.0 - pan.0).abs() > 0.5
            || (cur.1 - pan.1).abs() > 0.5
            || (cz - zoom).abs() > 0.01
        {
            ui.pan.set(pan);
            ui.zoom.set(zoom);
        }
    });

    let edges = canvas(move |cx, size| {
        let (w, h) = (size.width, size.height);
        let prev = ui.size.get_untracked();
        if (prev.0 - w).abs() > 0.5 || (prev.1 - h).abs() > 0.5 {
            ui.size.set((w, h));
        }
        if w < 8.0 || h < 8.0 {
            return;
        }
        cx.fill(&Rect::new(0.0, 0.0, w, h), design::BG_BASE, 0.0);

        let Some(graph) = ui.graph.get() else {
            return;
        };
        let stale = ui.stale.get();
        let (clusters, chips) =
            overview_layout(&graph, &stale, row_width_for_pane(w.max(900.0)));
        let (pan_x, pan_y) = ui.pan.get();
        let zoom = ui.zoom.get().clamp(0.15, 2.0);
        let ox = w / 2.0 + pan_x;
        let oy = h / 2.0 + pan_y;
        let to_screen = |x: f64, y: f64| Point::new(ox + x * zoom, oy + y * zoom);

        // Cluster frames.
        let frame = Stroke::new(1.0 * zoom.max(0.6));
        for c in &clusters {
            let r = Rect::new(
                ox + c.x * zoom,
                oy + c.y * zoom,
                ox + (c.x + c.w) * zoom,
                oy + (c.y + c.h) * zoom,
            );
            cx.fill(&r, design::BG_RAISED.with_alpha(0.35), 0.0);
            cx.stroke(&r, design::BORDER.with_alpha(0.55), &frame);
        }

        // Call edges between chip centers.
        let pos: std::collections::HashMap<u32, Point> = chips
            .iter()
            .map(|c| {
                (
                    c.index,
                    to_screen(c.x + c.w / 2.0, c.y + c.h / 2.0),
                )
            })
            .collect();
        for e in &graph.edges {
            let Some(&a) = pos.get(&e.from) else {
                continue;
            };
            let Some(&b) = pos.get(&e.to) else {
                continue;
            };
            let same_file = graph.nodes[e.from as usize].file == graph.nodes[e.to as usize].file;
            let color = if same_file {
                design::BORDER.with_alpha(0.18)
            } else {
                design::ACCENT.with_alpha(0.35)
            };
            let thickness = if same_file { 0.7 } else { 1.1 } * zoom.max(0.5);
            cx.stroke(&Line::new(a, b), color, &Stroke::new(thickness));
        }
    })
    .style(|s| s.absolute().inset(0.0).width_full().height_full().pointer_events_none());

    let chips_layer = dyn_container(
        move || {
            (
                ui.pan.get(),
                ui.zoom.get(),
                ui.size.get(),
                ui.graph.get().as_ref().map(|g| g.nodes.len()),
                ui.stale.get().file_count(),
            )
        },
        move |_| {
            let Some(graph) = ui.graph.get_untracked() else {
                return Empty::new().into_any();
            };
            let stale = ui.stale.get_untracked();
            let (pw, _) = ui.size.get_untracked();
            let (clusters, chips) =
                overview_layout(&graph, &stale, row_width_for_pane(pw.max(900.0)));
            let (pan_x, pan_y) = ui.pan.get_untracked();
            let zoom = ui.zoom.get_untracked().clamp(0.15, 2.0);
            let ox = pw / 2.0 + pan_x;
            let oy = ui.size.get_untracked().1 / 2.0 + pan_y;

            let mut children: Vec<_> = clusters
                .iter()
                .map(|c| {
                    let title = c.title.clone();
                    let left = ox + c.x * zoom;
                    let top = oy + c.y * zoom;
                    let width = c.w * zoom;
                    Label::derived(move || title.clone())
                        .style(move |s| {
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

            for chip in chips {
                let idx = chip.index;
                let label = chip.label.clone();
                let stale = chip.stale;
                let left = ox + chip.x * zoom;
                let top = oy + chip.y * zoom;
                let width = chip.w * zoom;
                let height = chip.h * zoom;
                children.push(
                    Label::derived(move || label.clone())
                        .on_event_stop(floem::event::listener::Click, move |_, _| {
                            focus_on(ui, idx);
                        })
                        .style(move |s| {
                            s.absolute()
                                .inset_left(left)
                                .inset_top(top)
                                .width(width)
                                .height(height)
                                .font_size((9.5 * zoom).clamp(7.5, 12.0) as f32)
                                .font_family(design::MONO.to_string())
                                .color(if stale {
                                    design::FG_GHOST
                                } else {
                                    design::FG_MUTED
                                })
                                .items_center()
                                .justify_center()
                                .background(design::BG_FLOAT.with_alpha(if stale {
                                    0.4
                                } else {
                                    0.9
                                }))
                                .border(1.0)
                                .border_color(if stale {
                                    design::FG_GHOST
                                } else {
                                    design::BORDER
                                })
                                .border_radius(3.0)
                                .cursor(floem::style::CursorStyle::Pointer)
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
                    *z = (*z * (1.0 - delta.y * 0.001)).clamp(0.15, 2.5);
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
    // Refit when the neighborhood or pane changes — not when the user pans.
    floem::reactive::Effect::new(move |_| {
        let focus = ui.focus.get();
        let hops = ui.hops.get();
        let (pw, ph) = ui.size.get();
        let Some(graph) = ui.graph.get() else {
            return;
        };
        let Some(f) = focus.or_else(|| default_focus(&graph)) else {
            return;
        };
        if pw < 40.0 || ph < 40.0 {
            return;
        }
        let stale = ui.stale.get_untracked();
        let (nodes, _) = layout(&graph, f, hops, &stale, row_width_for_pane(pw));
        let (pan, zoom) = fit_camera(&nodes, pw, ph);
        let cur = ui.pan.get_untracked();
        let cz = ui.zoom.get_untracked();
        if (cur.0 - pan.0).abs() > 0.5
            || (cur.1 - pan.1).abs() > 0.5
            || (cz - zoom).abs() > 0.01
        {
            ui.pan.set(pan);
            ui.zoom.set(zoom);
        }
    });

    let edges = canvas(move |cx, size| {
        let (w, h) = (size.width, size.height);
        let prev = ui.size.get_untracked();
        if (prev.0 - w).abs() > 0.5 || (prev.1 - h).abs() > 0.5 {
            ui.size.set((w, h));
        }
        if w < 8.0 || h < 8.0 {
            return;
        }
        cx.fill(&Rect::new(0.0, 0.0, w, h), design::BG_BASE, 0.0);

        let Some(graph) = ui.graph.get() else {
            return;
        };
        let Some(focus) = ui.focus.get().or_else(|| default_focus(&graph)) else {
            return;
        };
        let stale = ui.stale.get();
        let (nodes, _) = layout(&graph, focus, ui.hops.get(), &stale, row_width_for_pane(w));
        let (pan_x, pan_y) = ui.pan.get();
        let zoom = ui.zoom.get().clamp(0.4, 3.0);
        let ox = w / 2.0 + pan_x;
        let oy = h / 2.0 + pan_y;
        let to_screen = |x: f64, y: f64| Point::new(ox + x * zoom, oy + y * zoom);

        let Some(focus_laid) = nodes.iter().find(|n| n.layer == Layer::Focus) else {
            return;
        };
        let f_top = to_screen(focus_laid.x + focus_laid.w / 2.0, focus_laid.y);
        let f_bot = to_screen(
            focus_laid.x + focus_laid.w / 2.0,
            focus_laid.y + focus_laid.h,
        );

        let mut draw_bus = |layer: Layer, focus_pt: Point, toward_down: bool| {
            let group: Vec<&LaidOut> = nodes.iter().filter(|n| n.layer == layer).collect();
            if group.is_empty() {
                return;
            }
            let centers: Vec<Point> = group
                .iter()
                .map(|n| {
                    let cx = n.x + n.w / 2.0;
                    let ey = if toward_down { n.y } else { n.y + n.h };
                    to_screen(cx, ey)
                })
                .collect();
            let bus_y = if toward_down {
                (focus_pt.y + centers.iter().map(|p| p.y).fold(f64::INFINITY, f64::min)) / 2.0
            } else {
                (focus_pt.y + centers.iter().map(|p| p.y).fold(f64::NEG_INFINITY, f64::max)) / 2.0
            };
            let min_x = centers.iter().map(|p| p.x).fold(f64::INFINITY, f64::min);
            let max_x = centers.iter().map(|p| p.x).fold(f64::NEG_INFINITY, f64::max);

            let spine = Stroke::new(1.25 * zoom);
            let color = design::BORDER.with_alpha(0.75);
            cx.stroke(
                &Line::new(focus_pt, Point::new(focus_pt.x, bus_y)),
                color,
                &spine,
            );
            if (max_x - min_x).abs() > 1.0 {
                cx.stroke(
                    &Line::new(Point::new(min_x, bus_y), Point::new(max_x, bus_y)),
                    color,
                    &spine,
                );
            }
            for (n, p) in group.iter().zip(centers.iter()) {
                let thickness = (1.0 + (n.sites as f64).ln().max(0.0)).clamp(1.0, 3.5) * zoom;
                let c = if n.stale {
                    design::FG_GHOST.with_alpha(0.5)
                } else {
                    design::BORDER.with_alpha(0.9)
                };
                let stroke = if n.stale {
                    Stroke::new(thickness).with_dashes(0.0, [4.0, 4.0])
                } else {
                    Stroke::new(thickness)
                };
                cx.stroke(&Line::new(Point::new(p.x, bus_y), *p), c, &stroke);
            }
        };

        draw_bus(Layer::Caller, f_top, false);
        draw_bus(Layer::Callee, f_bot, true);
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
            let (pw, ph) = ui.size.get_untracked();
            let (nodes, hidden) = layout(
                &graph,
                focus,
                ui.hops.get_untracked(),
                &stale,
                row_width_for_pane(pw),
            );
            let (pan_x, pan_y) = ui.pan.get_untracked();
            let zoom = ui.zoom.get_untracked().clamp(0.4, 3.0);
            let ox = pw / 2.0 + pan_x;
            let oy = ph / 2.0 + pan_y;
            let _ = ph;

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

                    let name = Label::derived(move || label.clone()).style(move |s| {
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
                            s.absolute()
                                .inset_left(left)
                                .inset_top(top)
                                .width(width)
                                .height(height)
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
        CallGraph {
            version: 1,
            nodes,
            edges,
            stats: BuildStats::default(),
            built_at: 0,
            sources: Default::default(),
        }
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
        let g = CallGraph {
            version: 1,
            nodes,
            edges,
            stats: BuildStats::default(),
            built_at: 0,
            sources: Default::default(),
        };
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
    fn overview_summary_mentions_files() {
        let g = tiny_graph();
        let s = graph_summary(&g, &Staleness::default());
        assert!(s.contains("3 files"), "{s}");
        assert!(s.contains("3 nodes"), "{s}");
    }
}
