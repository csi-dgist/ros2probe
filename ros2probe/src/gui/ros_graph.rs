use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::Write as IoWrite;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;

use eframe::egui;

use super::dashboard::{GraphSnapshot, GraphTopic};

// ── Graph filter ─────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct GraphFilter {
    pub hide_tf: bool,
    pub hide_params: bool,
    pub hide_debug: bool,
    pub hide_leaf_topics: bool,
}

impl Default for GraphFilter {
    fn default() -> Self {
        Self {
            hide_tf: true,
            hide_params: true,
            hide_debug: true,
            hide_leaf_topics: true,
        }
    }
}

/// Returns true if the topic is external: visible on a non-loopback network interface.
/// Topics communicated exclusively over shared memory or loopback are internal.
pub fn is_recordable_topic(name: &str, graph: &GraphSnapshot) -> bool {
    graph
        .topics
        .iter()
        .find(|t| t.name == name)
        .map(|t| !t.local_only)
        .unwrap_or(false)
}

/// Runs `compute_graph_layout` on a background thread so the UI thread is
/// never blocked by the `dot` process. Submit a filtered `GraphSnapshot` with
/// `submit`; call `poll` on each frame to collect the result when ready.
pub struct LayoutWorker {
    tx: mpsc::Sender<GraphSnapshot>,
    rx: mpsc::Receiver<Option<LayoutedGraph>>,
}

impl LayoutWorker {
    pub fn new() -> Self {
        let (in_tx, in_rx) = mpsc::channel::<GraphSnapshot>();
        let (out_tx, out_rx) = mpsc::channel();
        thread::spawn(move || {
            while let Ok(mut graph) = in_rx.recv() {
                // Drain queued snapshots — only process the most recent one.
                while let Ok(newer) = in_rx.try_recv() {
                    graph = newer;
                }
                if out_tx.send(compute_graph_layout(&graph)).is_err() {
                    break;
                }
            }
        });
        Self { tx: in_tx, rx: out_rx }
    }

    /// Submit a new graph for layout. Non-blocking; older pending work is
    /// discarded by the worker in favour of the latest snapshot.
    pub fn submit(&self, graph: GraphSnapshot) {
        let _ = self.tx.send(graph);
    }

    /// Returns a completed layout if one is ready, otherwise `None`.
    pub fn poll(&self) -> Option<Option<LayoutedGraph>> {
        self.rx.try_recv().ok()
    }
}

/// Applies `filter` to `snapshot` and returns a new filtered `GraphSnapshot`.
pub fn apply_filter(snapshot: &GraphSnapshot, filter: &GraphFilter) -> GraphSnapshot {
    let mut topics: Vec<GraphTopic> = snapshot.topics.clone();

    // 1. Hide /tf and /tf_static
    if filter.hide_tf {
        topics.retain(|t| t.name != "/tf" && t.name != "/tf_static");
    }

    // 2. Hide /parameter_events and /rosout
    if filter.hide_params {
        topics.retain(|t| !t.name.ends_with("/parameter_events") && t.name != "/rosout");
    }

    // 3. Hide topics where any path segment starts with '_'
    if filter.hide_debug {
        topics.retain(|t| {
            !t.name.split('/').any(|seg| !seg.is_empty() && seg.starts_with('_'))
        });
    }

    // 4. Hide leaf topics (no subscribers)
    if filter.hide_leaf_topics {
        topics.retain(|t| !t.subscribers.is_empty());
    }

    // Compute which ROS nodes are still connected
    let connected: HashSet<&str> = topics
        .iter()
        .flat_map(|t| {
            t.publishers
                .iter()
                .map(|s| s.as_str())
                .chain(t.subscribers.iter().map(|s| s.as_str()))
        })
        .collect();

    let mut isolated_nodes: Vec<String> = snapshot
        .isolated_nodes
        .iter()
        .filter(|n| !connected.contains(n.as_str()))
        .cloned()
        .collect();

    // Add nodes that were connected in the original snapshot but are disconnected now.
    let originally_connected: HashSet<&str> = snapshot
        .topics
        .iter()
        .flat_map(|t| {
            t.publishers
                .iter()
                .map(|s| s.as_str())
                .chain(t.subscribers.iter().map(|s| s.as_str()))
        })
        .collect();
    for node in originally_connected {
        if !connected.contains(node) && !isolated_nodes.iter().any(|n| n == node) {
            isolated_nodes.push(node.to_string());
        }
    }

    GraphSnapshot { topics, isolated_nodes }
}

/// Renders checkboxes for the graph filter and a legend for edge colors.
/// Returns `true` if any toggle changed.
pub fn render_graph_filter_bar(ui: &mut egui::Ui, filter: &mut GraphFilter) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label("Hide:");
        if ui.checkbox(&mut filter.hide_tf, "tf").changed() {
            changed = true;
        }
        if ui.checkbox(&mut filter.hide_params, "params").changed() {
            changed = true;
        }
        if ui.checkbox(&mut filter.hide_debug, "debug").changed() {
            changed = true;
        }
        if ui.checkbox(&mut filter.hide_leaf_topics, "leaf topics").changed() {
            changed = true;
        }

        ui.add_space(16.0);
        ui.separator();
        ui.add_space(8.0);
        ui.label("Edges:");
        edge_legend_swatch(ui, egui::Color32::from_rgb(190, 70, 70), "network");
        ui.add_space(6.0);
        edge_legend_swatch(ui, egui::Color32::from_rgb(60, 160, 100), "local (SHM)");
    });
    changed
}

/// Draws a small colored bar plus a label, used as a legend entry for edge colors.
fn edge_legend_swatch(ui: &mut egui::Ui, color: egui::Color32, label: &str) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(18.0, 3.0), egui::Sense::hover());
    ui.painter().line_segment(
        [rect.left_center(), rect.right_center()],
        egui::Stroke::new(2.5, color),
    );
    ui.label(label);
}

// ── Data structures ───────────────────────────────────────────────────────────

#[derive(Clone, Debug, Default)]
pub struct LayoutedGraph {
    pub width_in: f32,
    pub height_in: f32,
    pub nodes: Vec<LayoutNode>,
    pub edges: Vec<LayoutEdge>,
    pub namespace_boxes: Vec<LayoutNamespaceBox>,
}

#[derive(Clone, Debug)]
pub struct LayoutNode {
    pub name: String,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub is_topic: bool,
    pub local_only: bool,
}

#[derive(Clone, Debug)]
pub struct LayoutEdge {
    pub points: Vec<(f32, f32)>,
    pub tail: String,
    pub head: String,
    pub local_only: bool,
}

/// Bounding box for a namespace group, in graphviz coordinates (inches).
#[derive(Clone, Debug)]
pub struct LayoutNamespaceBox {
    pub label: String,
    pub cx: f32,
    pub cy: f32,
    pub w: f32,
    pub h: f32,
}

#[derive(Clone, Debug)]
struct GraphViewState {
    zoom: f32,
    pan: egui::Vec2,
}

impl Default for GraphViewState {
    fn default() -> Self {
        Self { zoom: 1.0, pan: egui::Vec2::ZERO }
    }
}

const TOPIC_PREFIX: &str = "topic::";
const DPI: f32 = 72.0;
const GRAPH_PADDING: f32 = 16.0;

/// Zoom level that fits the whole graph inside `rect` with padding.
fn compute_fit_zoom(layout: &LayoutedGraph, rect: egui::Rect) -> f32 {
    let avail_w = (rect.width() - GRAPH_PADDING * 2.0).max(1.0);
    let avail_h = (rect.height() - GRAPH_PADDING * 2.0).max(1.0);
    let w = layout.width_in * DPI;
    let h = layout.height_in * DPI;
    if w > 0.0 && h > 0.0 { (avail_w / w).min(avail_h / h) } else { 1.0 }
}

/// Padding (in graphviz inches) around topic nodes inside a namespace box.
const NS_PAD: f32 = 0.18;
/// Minimum number of topics in a namespace to draw a grouping box.
const NS_MIN_TOPICS: usize = 2;

// ── Namespace helpers ─────────────────────────────────────────────────────────

/// Returns the top-level namespace of a topic, or None for root-level topics.
/// "/cmd_vel"           → None
/// "/nav/cmd_vel"       → Some("/nav")
/// "/nav/local/cmd_vel" → Some("/nav")
fn topic_namespace(topic: &str) -> Option<String> {
    let stripped = topic.trim_start_matches('/');
    if let Some(pos) = stripped.find('/') {
        Some(format!("/{}", &stripped[..pos]))
    } else {
        None
    }
}

/// Groups topic names by their top-level namespace.
/// Only namespaces with >= NS_MIN_TOPICS topics are returned.
fn group_topics_by_namespace(
    topic_names: &BTreeSet<String>,
) -> BTreeMap<String, Vec<String>> {
    let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for t in topic_names {
        if let Some(ns) = topic_namespace(t) {
            map.entry(ns).or_default().push(t.clone());
        }
    }
    map.retain(|_, topics| topics.len() >= NS_MIN_TOPICS);
    map
}

fn cluster_dot_id(namespace: &str) -> String {
    format!("cluster_{}", namespace.replace(['/', '-', '.'], "_"))
}

// ── Layout computation ────────────────────────────────────────────────────────

pub fn compute_graph_layout(graph: &GraphSnapshot) -> Option<LayoutedGraph> {
    if graph.topics.is_empty() && graph.isolated_nodes.is_empty() {
        return None;
    }

    let mut ros_nodes: BTreeSet<String> = BTreeSet::new();
    let mut topic_names: BTreeSet<String> = BTreeSet::new();
    let mut pub_edges: BTreeSet<(String, String)> = BTreeSet::new();
    let mut sub_edges: BTreeSet<(String, String)> = BTreeSet::new();

    for topic in &graph.topics {
        topic_names.insert(topic.name.clone());
        for pub_node in &topic.publishers {
            ros_nodes.insert(pub_node.clone());
            pub_edges.insert((pub_node.clone(), topic.name.clone()));
        }
        for sub_node in &topic.subscribers {
            ros_nodes.insert(sub_node.clone());
            sub_edges.insert((topic.name.clone(), sub_node.clone()));
        }
    }

    // Add isolated nodes to ros_nodes so they appear as isolated ellipses
    for node in &graph.isolated_nodes {
        ros_nodes.insert(node.clone());
    }

    if topic_names.is_empty() && ros_nodes.is_empty() {
        return None;
    }

    let ns_groups = group_topics_by_namespace(&topic_names);
    let dot_src = build_dot_source(&ros_nodes, &topic_names, &pub_edges, &sub_edges, &ns_groups);

    let mut child = Command::new("/usr/bin/dot")
        .arg("-Tplain")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    if let Some(stdin) = child.stdin.as_mut() {
        let _ = stdin.write_all(dot_src.as_bytes());
    }

    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }

    let plain = std::str::from_utf8(&output.stdout).ok()?;
    let mut layouted = parse_plain_output(plain)?;
    layouted.namespace_boxes = compute_namespace_boxes(&layouted.nodes, &ns_groups);

    // Enrich local_only on nodes and edges.
    // Build lookup: topic_name → GraphTopic
    let topic_map: std::collections::HashMap<&str, &GraphTopic> = graph.topics.iter()
        .map(|t| (t.name.as_str(), t))
        .collect();

    for node in &mut layouted.nodes {
        if node.is_topic {
            node.local_only = topic_map.get(node.name.as_str()).map_or(false, |t| t.local_only);
        }
    }
    for edge in &mut layouted.edges {
        // Publisher→topic edge: tail=publisher_node, head=topic_node
        if let Some(topic_name) = edge.head.strip_prefix(TOPIC_PREFIX) {
            if let Some(topic) = topic_map.get(topic_name) {
                edge.local_only = topic.local_only;
                continue;
            }
        }
        // Topic→subscriber edge: tail=topic_node, head=subscriber_node
        if let Some(topic_name) = edge.tail.strip_prefix(TOPIC_PREFIX) {
            if let Some(topic) = topic_map.get(topic_name) {
                edge.local_only = topic.local_only;
                continue;
            }
        }
    }

    Some(layouted)
}

fn compute_namespace_boxes(
    nodes: &[LayoutNode],
    ns_groups: &BTreeMap<String, Vec<String>>,
) -> Vec<LayoutNamespaceBox> {
    let mut boxes = Vec::new();

    for (ns, topics) in ns_groups {
        let ns_nodes: Vec<&LayoutNode> = nodes
            .iter()
            .filter(|n| n.is_topic && topics.contains(&n.name))
            .collect();

        if ns_nodes.is_empty() {
            continue;
        }

        let min_x = ns_nodes.iter().map(|n| n.x - n.w * 0.5).fold(f32::MAX, f32::min);
        let max_x = ns_nodes.iter().map(|n| n.x + n.w * 0.5).fold(f32::MIN, f32::max);
        let min_y = ns_nodes.iter().map(|n| n.y - n.h * 0.5).fold(f32::MAX, f32::min);
        let max_y = ns_nodes.iter().map(|n| n.y + n.h * 0.5).fold(f32::MIN, f32::max);

        boxes.push(LayoutNamespaceBox {
            label: ns.clone(),
            cx: (min_x + max_x) * 0.5,
            cy: (min_y + max_y) * 0.5,
            w: (max_x - min_x) + NS_PAD * 2.0,
            h: (max_y - min_y) + NS_PAD * 2.0,
        });
    }

    boxes
}

// ── DOT source generation ─────────────────────────────────────────────────────

fn dot_id(name: &str) -> String {
    format!("\"{}\"", name.replace('\\', "\\\\").replace('"', "\\\""))
}

fn topic_dot_id(name: &str) -> String {
    let prefixed = format!("{}{}", TOPIC_PREFIX, name);
    format!("\"{}\"", prefixed.replace('\\', "\\\\").replace('"', "\\\""))
}

fn build_dot_source(
    ros_nodes: &BTreeSet<String>,
    topic_names: &BTreeSet<String>,
    pub_edges: &BTreeSet<(String, String)>,
    sub_edges: &BTreeSet<(String, String)>,
    ns_groups: &BTreeMap<String, Vec<String>>,
) -> String {
    let mut s = String::from("digraph ros {\n  rankdir=LR;\n  edge [arrowsize=0.7];\n");

    // ROS nodes (ellipses)
    s.push_str("  node [shape=ellipse, fontname=\"sans-serif\", fontsize=11];\n");
    for n in ros_nodes {
        s.push_str(&format!("  {};\n", dot_id(n)));
    }

    // Collect which topics are in a namespace cluster
    let clustered: BTreeSet<String> =
        ns_groups.values().flatten().cloned().collect();

    // Namespace clusters
    for (ns, topics) in ns_groups {
        s.push_str(&format!(
            "  subgraph {} {{\n    label={};\n    style=filled;\n    fillcolor=\"#eaf4ea\";\n    color=\"#6aaa6a\";\n",
            cluster_dot_id(ns),
            dot_id(ns),
        ));
        s.push_str("    node [shape=box, fontname=\"sans-serif\", fontsize=10];\n");
        for t in topics {
            s.push_str(&format!("    {};\n", topic_dot_id(t)));
        }
        s.push_str("  }\n");
    }

    // Root-level (unclustered) topic nodes
    let has_root = topic_names.iter().any(|t| !clustered.contains(t));
    if has_root {
        s.push_str("  node [shape=box, fontname=\"sans-serif\", fontsize=10];\n");
        for t in topic_names {
            if !clustered.contains(t) {
                s.push_str(&format!("  {};\n", topic_dot_id(t)));
            }
        }
    }

    // Edges
    for (ros_node, topic) in pub_edges {
        s.push_str(&format!("  {} -> {};\n", dot_id(ros_node), topic_dot_id(topic)));
    }
    for (topic, ros_node) in sub_edges {
        s.push_str(&format!("  {} -> {};\n", topic_dot_id(topic), dot_id(ros_node)));
    }

    s.push_str("}\n");
    s
}

// ── Plain output parser ───────────────────────────────────────────────────────

fn tokenize(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut chars = line.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_ascii_whitespace() {
            chars.next();
            continue;
        }
        if c == '"' {
            chars.next();
            let mut tok = String::new();
            let mut esc = false;
            loop {
                match chars.next() {
                    None => break,
                    Some('\\') if !esc => { esc = true; }
                    Some('"') if !esc => break,
                    Some(ch) => { esc = false; tok.push(ch); }
                }
            }
            tokens.push(tok);
        } else {
            let mut tok = String::new();
            while let Some(&ch) = chars.peek() {
                if ch.is_ascii_whitespace() { break; }
                tok.push(ch);
                chars.next();
            }
            tokens.push(tok);
        }
    }
    tokens
}

fn parse_plain_output(plain: &str) -> Option<LayoutedGraph> {
    let mut layouted = LayoutedGraph::default();

    for line in plain.lines() {
        let tok = tokenize(line);
        if tok.is_empty() { continue; }
        match tok[0].as_str() {
            "graph" if tok.len() >= 4 => {
                layouted.width_in = tok[2].parse().unwrap_or(1.0);
                layouted.height_in = tok[3].parse().unwrap_or(1.0);
            }
            "node" if tok.len() >= 6 => {
                let raw = tok[1].clone();
                let (name, is_topic) =
                    if let Some(stripped) = raw.strip_prefix(TOPIC_PREFIX) {
                        (stripped.to_string(), true)
                    } else {
                        (raw, false)
                    };
                layouted.nodes.push(LayoutNode {
                    name,
                    x: tok[2].parse().unwrap_or(0.0),
                    y: tok[3].parse().unwrap_or(0.0),
                    w: tok[4].parse().unwrap_or(1.0),
                    h: tok[5].parse().unwrap_or(0.5),
                    is_topic,
                    local_only: false,
                });
            }
            "edge" if tok.len() >= 4 => {
                let tail = tok[1].clone();
                let head = tok[2].clone();
                let n: usize = tok[3].parse().unwrap_or(0);
                if tok.len() < 4 + n * 2 { continue; }
                let mut points = Vec::with_capacity(n);
                for i in 0..n {
                    points.push((
                        tok[4 + i * 2].parse().unwrap_or(0.0),
                        tok[5 + i * 2].parse().unwrap_or(0.0),
                    ));
                }
                layouted.edges.push(LayoutEdge { points, tail, head, local_only: false });
            }
            _ => {}
        }
    }

    Some(layouted)
}

// ── Public render entry point ─────────────────────────────────────────────────

pub fn render_ros_graph(
    ui: &mut egui::Ui,
    layout: Option<&LayoutedGraph>,
    graph: &GraphSnapshot,
) {
    let desired_size = egui::vec2(ui.available_width(), ui.available_height().max(200.0));
    let (rect, response) =
        ui.allocate_exact_size(desired_size, egui::Sense::click_and_drag());

    let painter = ui.painter_at(rect);
    draw_panel_background(&painter, rect);

    if graph.topics.is_empty() && graph.isolated_nodes.is_empty() {
        painter.text(
            rect.center(), egui::Align2::CENTER_CENTER,
            "No graph edges available yet.",
            egui::TextStyle::Body.resolve(ui.style()),
            egui::Color32::from_gray(110),
        );
        return;
    }

    let Some(layout) = layout else {
        painter.text(
            rect.center(), egui::Align2::CENTER_CENTER,
            "Computing layout...",
            egui::TextStyle::Body.resolve(ui.style()),
            egui::Color32::from_gray(110),
        );
        return;
    };

    let view_id = egui::Id::new("ros_graph_view");
    let fit_zoom = compute_fit_zoom(layout, rect);
    let mut view: GraphViewState = ui.data(|d| d.get_temp(view_id))
        .unwrap_or_else(|| GraphViewState { zoom: fit_zoom, pan: egui::Vec2::ZERO });

    // Fullscreen button
    let fs_btn_rect = egui::Rect::from_min_size(
        egui::pos2(rect.right() - 28.0, rect.top() + 6.0),
        egui::vec2(22.0, 22.0),
    );
    let fs_btn = ui.interact(
        fs_btn_rect, egui::Id::new("ros_graph_fs_btn"), egui::Sense::click(),
    );
    let btn_bg = if fs_btn.hovered() {
        egui::Color32::from_rgba_premultiplied(80, 120, 180, 40)
    } else {
        egui::Color32::TRANSPARENT
    };
    painter.rect_filled(fs_btn_rect, 4.0, btn_bg);
    draw_fullscreen_icon(&painter, fs_btn_rect, egui::Color32::from_rgb(90, 120, 160));

    if fs_btn.clicked() {
        let fs_rect = ui.ctx().screen_rect();
        let fs_fit = compute_fit_zoom(layout, fs_rect);
        let fs_id = egui::Id::new("ros_graph_view_fs");
        ui.data_mut(|d| d.insert_temp(fs_id, GraphViewState { zoom: fs_fit, pan: egui::Vec2::ZERO }));
        ui.data_mut(|d| d.insert_temp(egui::Id::new("ros_graph_fullscreen"), true));
    }

    if !fs_btn.hovered() {
        handle_view_input(ui, &response, rect, &mut view, fit_zoom);
    }

    ui.data_mut(|d| d.insert_temp(view_id, view.clone()));
    draw_graph_content(&painter, rect, layout, &view);

    let is_fullscreen: bool =
        ui.data(|d| d.get_temp(egui::Id::new("ros_graph_fullscreen")).unwrap_or(false));
    if is_fullscreen {
        render_fullscreen(ui, layout);
    }
}

// ── Fullscreen overlay ────────────────────────────────────────────────────────

fn render_fullscreen(ui: &mut egui::Ui, layout: &LayoutedGraph) {
    let ctx = ui.ctx().clone();
    let screen_rect = ctx.screen_rect();
    let fs_view_id = egui::Id::new("ros_graph_view_fs");
    let fs_flag_id = egui::Id::new("ros_graph_fullscreen");

    egui::Area::new(egui::Id::new("ros_graph_fullscreen_area"))
        .order(egui::Order::Foreground)
        .fixed_pos(screen_rect.min)
        .show(&ctx, |ui| {
            let (rect, response) =
                ui.allocate_exact_size(screen_rect.size(), egui::Sense::click_and_drag());
            let painter = ui.painter_at(rect);
            painter.rect_filled(rect, 0.0, egui::Color32::from_rgb(245, 248, 252));

            let fit_zoom = compute_fit_zoom(layout, rect);
            let mut view: GraphViewState = ui.data(|d| d.get_temp(fs_view_id))
                .unwrap_or_else(|| GraphViewState { zoom: fit_zoom, pan: egui::Vec2::ZERO });

            // Close button
            let close_rect = egui::Rect::from_min_size(
                egui::pos2(rect.right() - 36.0, rect.top() + 8.0),
                egui::vec2(28.0, 28.0),
            );
            let close_btn = ui.interact(
                close_rect, egui::Id::new("ros_graph_close_fs"), egui::Sense::click(),
            );
            let close_bg = if close_btn.hovered() {
                egui::Color32::from_rgba_premultiplied(200, 60, 60, 50)
            } else {
                egui::Color32::from_rgba_premultiplied(100, 100, 100, 20)
            };
            painter.rect_filled(close_rect, 6.0, close_bg);
            painter.text(
                close_rect.center(), egui::Align2::CENTER_CENTER, "✕",
                egui::FontId::proportional(14.0), egui::Color32::from_rgb(80, 80, 80),
            );

            if close_btn.clicked() || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                ui.data_mut(|d| d.insert_temp(fs_flag_id, false));
            }

            if !close_btn.hovered() {
                handle_view_input(ui, &response, rect, &mut view, fit_zoom);
            }

            ui.data_mut(|d| d.insert_temp(fs_view_id, view.clone()));
            draw_graph_content(&painter, rect, layout, &view);

            let hint = format!(
                "{:.0}%  (scroll · drag · double-click to fit · Esc to exit)",
                view.zoom * 100.0
            );
            painter.text(
                rect.left_bottom() + egui::vec2(12.0, -10.0),
                egui::Align2::LEFT_BOTTOM, hint,
                egui::FontId::proportional(10.0), egui::Color32::from_gray(150),
            );
        });
}

// ── Shared input handler ──────────────────────────────────────────────────────

fn handle_view_input(
    ui: &egui::Ui,
    response: &egui::Response,
    rect: egui::Rect,
    view: &mut GraphViewState,
    fit_zoom: f32,
) {
    if response.dragged() {
        view.pan += response.drag_delta();
    }
    if response.double_clicked() {
        *view = GraphViewState { zoom: fit_zoom, pan: egui::Vec2::ZERO };
        return;
    }
    let scroll_delta = ui.ctx().input(|i| i.smooth_scroll_delta.y);
    if scroll_delta != 0.0 {
        let cursor = ui.ctx().input(|i| i.pointer.hover_pos().unwrap_or(rect.center()));
        if rect.contains(cursor) {
            let factor = (scroll_delta * 0.002).exp();
            let center = rect.center();
            view.pan = (cursor - center) * (1.0 - factor) + view.pan * factor;
            view.zoom = (view.zoom * factor).clamp(0.1, 10.0);
        }
    }
}

// ── Core drawing ──────────────────────────────────────────────────────────────

const BASE_FONT_SIZE: f32 = 10.0;

/// Renders the graph by applying zoom/pan manually to every coordinate.
/// Avoids layer-ordering issues (no separate layer needed) while keeping
/// text sharp at every zoom level.
fn draw_graph_content(
    painter: &egui::Painter,
    rect: egui::Rect,
    layout: &LayoutedGraph,
    view: &GraphViewState,
) {
    // Natural size: zoom=1 means 1:1 DPI, graph centered in rect.
    let base_left = rect.center().x - layout.width_in * DPI * 0.5;
    let base_bottom = rect.center().y + layout.height_in * DPI * 0.5;

    // Convert graphviz inches → base screen pos (no user zoom)
    let to_base = |x: f32, y: f32| -> egui::Pos2 {
        egui::pos2(base_left + x * DPI, base_bottom - y * DPI)
    };
    let to_px = |len: f32| len * DPI;

    // Apply user zoom + pan: screen = center + (base - center) * zoom + pan
    let vc = rect.center();
    let apply = |p: egui::Pos2| -> egui::Pos2 {
        vc + (p - vc) * view.zoom + view.pan
    };

    let font_size = (BASE_FONT_SIZE * view.zoom).max(1.0);
    let painter = painter.with_clip_rect(rect);

    // ── 1. Namespace boxes ───────────────────────────────────────────────────
    let ns_fill         = egui::Color32::from_rgb(234, 244, 234);
    let ns_stroke_color = egui::Color32::from_rgb(130, 180, 130);
    let ns_text_color   = egui::Color32::from_rgb(60, 110, 60);

    for ns_box in &layout.namespace_boxes {
        let c = apply(to_base(ns_box.cx, ns_box.cy));
        let w = to_px(ns_box.w) * view.zoom;
        let h = to_px(ns_box.h) * view.zoom;
        let r = egui::Rect::from_center_size(c, egui::vec2(w, h));
        painter.rect(r, 6.0, ns_fill,
            egui::Stroke::new(1.5, ns_stroke_color),
            egui::StrokeKind::Outside,
        );
    }

    // ── 2. Edges ─────────────────────────────────────────────────────────────
    let edge_color_net   = egui::Color32::from_rgb(190, 70, 70);
    let edge_color_local = egui::Color32::from_rgb(60, 160, 100);
    for edge in &layout.edges {
        let sp: Vec<egui::Pos2> =
            edge.points.iter().map(|&(x, y)| apply(to_base(x, y))).collect();
        if !sp.is_empty() {
            let color = if edge.local_only { edge_color_local } else { edge_color_net };
            draw_bezier_edge(&painter, &sp, color, 1.5);
        }
    }

    // ── 3. Nodes ─────────────────────────────────────────────────────────────
    let ros_fill   = egui::Color32::from_rgb(220, 235, 248);
    let ros_stroke = egui::Color32::from_rgb(90, 130, 170);
    let topic_fill   = egui::Color32::from_rgb(255, 255, 255);
    let topic_stroke = egui::Color32::from_rgb(90, 150, 90);
    let text_color   = egui::Color32::from_rgb(25, 45, 65);

    for node in &layout.nodes {
        let c  = apply(to_base(node.x, node.y));
        let rx = to_px(node.w) * view.zoom * 0.5;
        let ry = to_px(node.h) * view.zoom * 0.5;

        if node.is_topic {
            let node_rect = egui::Rect::from_center_size(c, egui::vec2(rx * 2.0, ry * 2.0));
            painter.rect(node_rect, 0.0, topic_fill,
                egui::Stroke::new(1.0, topic_stroke),
                egui::StrokeKind::Outside,
            );
        } else {
            painter.add(egui::Shape::Ellipse(egui::epaint::EllipseShape {
                center: c,
                radius: egui::vec2(rx, ry),
                fill: ros_fill,
                stroke: egui::Stroke::new(1.0, ros_stroke),
            }));
        }

        let label = clip_text(&painter, &node.name, (rx * 2.0 - 6.0).max(0.0), font_size);
        painter.text(c, egui::Align2::CENTER_CENTER, label,
            egui::FontId::proportional(font_size), text_color,
        );
    }

    // ── 4. Namespace box labels (drawn after nodes so they're always on top) ──
    for ns_box in &layout.namespace_boxes {
        let c = apply(to_base(ns_box.cx, ns_box.cy));
        let w = to_px(ns_box.w) * view.zoom;
        let h = to_px(ns_box.h) * view.zoom;
        let r = egui::Rect::from_center_size(c, egui::vec2(w, h));
        let label_pos = r.left_top() + egui::vec2(6.0, 4.0);
        // Background pill behind the label so it's readable over topic boxes.
        let galley = painter.layout_no_wrap(
            ns_box.label.clone(),
            egui::FontId::proportional(font_size),
            ns_text_color,
        );
        let text_size = galley.size();
        let bg_rect = egui::Rect::from_min_size(
            label_pos - egui::vec2(3.0, 2.0),
            text_size + egui::vec2(6.0, 4.0),
        );
        painter.rect(bg_rect, 4.0, ns_fill,
            egui::Stroke::new(1.0, ns_stroke_color),
            egui::StrokeKind::Outside,
        );
        painter.text(
            label_pos,
            egui::Align2::LEFT_TOP,
            &ns_box.label,
            egui::FontId::proportional(font_size),
            ns_text_color,
        );
    }

    // ── Zoom hint (not clipped, drawn on the rect's bottom-left) ─────────────
    let hint = format!("{:.0}%  (scroll · drag · double-click to fit)", view.zoom * 100.0);
    painter.text(
        rect.left_bottom() + egui::vec2(10.0, -8.0),
        egui::Align2::LEFT_BOTTOM, hint,
        egui::FontId::proportional(9.0), egui::Color32::from_gray(140),
    );
}

// ── Drawing primitives ────────────────────────────────────────────────────────

fn draw_panel_background(painter: &egui::Painter, rect: egui::Rect) {
    painter.rect_filled(rect, 8.0, egui::Color32::from_rgb(249, 251, 254));
    painter.rect_stroke(
        rect, 8.0,
        egui::Stroke::new(1.0, egui::Color32::from_rgb(214, 220, 228)),
        egui::StrokeKind::Outside,
    );
}

fn draw_fullscreen_icon(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let m = 5.0_f32;
    let l = 5.0_f32;
    let stroke = egui::Stroke::new(1.5, color);
    let corners = [
        (rect.left_top(),     egui::vec2(1.0, 0.0),  egui::vec2(0.0, 1.0)),
        (rect.right_top(),    egui::vec2(-1.0, 0.0), egui::vec2(0.0, 1.0)),
        (rect.left_bottom(),  egui::vec2(1.0, 0.0),  egui::vec2(0.0, -1.0)),
        (rect.right_bottom(), egui::vec2(-1.0, 0.0), egui::vec2(0.0, -1.0)),
    ];
    for (corner, dx, dy) in corners {
        let o = corner + (dx + dy) * m;
        painter.line_segment([o, o + dx * l], stroke);
        painter.line_segment([o, o + dy * l], stroke);
    }
}

fn draw_bezier_edge(
    painter: &egui::Painter,
    sp: &[egui::Pos2],
    color: egui::Color32,
    width: f32,
) {
    if sp.len() >= 4 {
        let mut i = 0;
        while i + 3 < sp.len() {
            painter.add(egui::Shape::CubicBezier(egui::epaint::CubicBezierShape {
                points: [sp[i], sp[i + 1], sp[i + 2], sp[i + 3]],
                closed: false,
                fill: egui::Color32::TRANSPARENT,
                stroke: egui::Stroke::new(width, color).into(),
            }));
            i += 3;
        }
        if i + 1 < sp.len() {
            painter.line_segment([sp[i], sp[sp.len() - 1]], egui::Stroke::new(width, color));
        }
    } else if sp.len() >= 2 {
        painter.line_segment([sp[0], sp[sp.len() - 1]], egui::Stroke::new(width, color));
    }

    let n = sp.len();
    if n >= 2 {
        let tip = sp[n - 1];
        let dir = (tip - sp[n - 2]).normalized();
        if dir.length_sq() > 0.0 {
            let perp = egui::Vec2::new(-dir.y, dir.x);
            let back = tip - dir * 9.0;
            painter.add(egui::Shape::convex_polygon(
                vec![tip, back + perp * 4.5, back - perp * 4.5],
                color, egui::Stroke::NONE,
            ));
        }
    }
}

fn to_screen(x: f32, y: f32, rect: egui::Rect, layout: &LayoutedGraph, view: &GraphViewState) -> egui::Pos2 {
    let base_left = rect.center().x - layout.width_in * DPI * 0.5;
    let base_bottom = rect.center().y + layout.height_in * DPI * 0.5;
    let base = egui::pos2(base_left + x * DPI, base_bottom - y * DPI);
    let vc = rect.center();
    vc + (base - vc) * view.zoom + view.pan
}

fn find_clicked_topic(
    layout: &LayoutedGraph,
    rect: egui::Rect,
    view: &GraphViewState,
    click_pos: egui::Pos2,
) -> Option<String> {
    for node in &layout.nodes {
        if !node.is_topic {
            continue;
        }
        let center = to_screen(node.x, node.y, rect, layout, view);
        let hw = node.w * DPI * view.zoom * 0.5;
        let hh = node.h * DPI * view.zoom * 0.5;
        let node_rect = egui::Rect::from_center_size(center, egui::vec2(hw * 2.0, hh * 2.0));
        if node_rect.contains(click_pos) {
            return Some(node.name.clone());
        }
    }
    None
}

/// Like `render_ros_graph` but topic nodes are clickable.
/// Returns the name of the topic node that was clicked, if any.
/// `selected_topic` is highlighted with a blue outline.
pub fn render_ros_graph_selectable(
    ui: &mut egui::Ui,
    layout: Option<&LayoutedGraph>,
    graph: &GraphSnapshot,
    selected_topic: Option<&str>,
) -> Option<String> {
    let desired_size = egui::vec2(ui.available_width(), ui.available_height().max(200.0));
    let (rect, response) =
        ui.allocate_exact_size(desired_size, egui::Sense::click_and_drag());

    let painter = ui.painter_at(rect);
    draw_panel_background(&painter, rect);

    if graph.topics.is_empty() && graph.isolated_nodes.is_empty() {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "No graph edges available yet.",
            egui::TextStyle::Body.resolve(ui.style()),
            egui::Color32::from_gray(110),
        );
        return None;
    }

    let Some(layout) = layout else {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "Computing layout...",
            egui::TextStyle::Body.resolve(ui.style()),
            egui::Color32::from_gray(110),
        );
        return None;
    };

    let view_id = egui::Id::new("topic_monitor_graph_view");
    let fit_zoom = compute_fit_zoom(layout, rect);
    let mut view: GraphViewState = ui
        .data(|d| d.get_temp(view_id))
        .unwrap_or_else(|| GraphViewState { zoom: fit_zoom, pan: egui::Vec2::ZERO });

    handle_view_input(ui, &response, rect, &mut view, fit_zoom);
    draw_graph_content(&painter, rect, layout, &view);

    // Highlight selected topic
    if let Some(sel) = selected_topic {
        for node in layout.nodes.iter().filter(|n| n.is_topic && n.name == sel) {
            let c = to_screen(node.x, node.y, rect, layout, &view);
            let hw = node.w * DPI * view.zoom * 0.5 + 3.0;
            let hh = node.h * DPI * view.zoom * 0.5 + 3.0;
            let hl_rect = egui::Rect::from_center_size(c, egui::vec2(hw * 2.0, hh * 2.0));
            let clipped_painter = painter.with_clip_rect(rect);
            clipped_painter.rect_stroke(
                hl_rect,
                0.0,
                egui::Stroke::new(2.5, egui::Color32::from_rgb(56, 110, 200)),
                egui::StrokeKind::Outside,
            );
        }
    }

    // Hover cursor feedback on topic nodes
    if let Some(hover_pos) = ui.ctx().input(|i| i.pointer.hover_pos()) {
        if rect.contains(hover_pos) {
            if find_clicked_topic(layout, rect, &view, hover_pos).is_some() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
        }
    }

    ui.data_mut(|d| d.insert_temp(view_id, view.clone()));

    // Return clicked topic name
    if response.clicked() {
        if let Some(click_pos) = response.interact_pointer_pos() {
            return find_clicked_topic(layout, rect, &view, click_pos);
        }
    }
    None
}

fn clip_text(painter: &egui::Painter, text: &str, max_width: f32, font_size: f32) -> String {
    let font = egui::FontId::proportional(font_size);
    let measure = |s: &str| -> f32 {
        painter.ctx().fonts(|f| {
            f.layout_no_wrap(s.to_string(), font.clone(), egui::Color32::BLACK).size().x
        })
    };

    if measure(text) <= max_width {
        return text.to_string();
    }

    let mut cur = text;
    loop {
        let rest = cur.trim_start_matches('/');
        if let Some(pos) = rest.find('/') {
            cur = &rest[pos..];
            if measure(cur) <= max_width {
                return cur.to_string();
            }
        } else {
            if measure(rest) <= max_width { return rest.to_string(); }
            break;
        }
    }

    let budget = (max_width - measure("...")).max(0.0);
    for &end in text.char_indices().map(|(i, _)| i).collect::<Vec<_>>().iter().rev() {
        if measure(&text[..end]) <= budget {
            return format!("{}...", &text[..end]);
        }
    }
    String::new()
}
