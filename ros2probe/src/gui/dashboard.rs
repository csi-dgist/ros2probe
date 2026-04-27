use std::{
    collections::VecDeque,
    fs,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use anyhow::Context;
use chrono::Local;
use eframe::egui;

use crate::{
    client::send_request,
    command::protocol::{
        CommandRequest, CommandResponse, NodeDetails, TopicDetails, TopicGraphRequest,
    },
};

use super::ros_graph::{
    apply_filter, render_graph_filter_bar, render_ros_graph, GraphFilter, LayoutWorker,
    LayoutedGraph,
};

const PAGE_BG: egui::Color32 = egui::Color32::from_rgb(230, 232, 236);
const HISTORY_WINDOW_SECS: f64 = 60.0;
const HISTORY_RETENTION_SECS: f64 = 65.0;
const GRAPH_HEIGHT: f32 = 120.0;
const LEFT_PADDING: f32 = 8.0;
const RIGHT_AXIS_WIDTH: f32 = 48.0;
const TOP_PADDING: f32 = 8.0;
const BOTTOM_PADDING: f32 = 22.0;

pub struct DashboardPage {
    worker_rx: Option<mpsc::Receiver<WorkerEvent>>,
    start_time: Instant,
    snapshot: DashboardSnapshot,
    connection_status: ConnectionStatus,
    discover_rx: Option<mpsc::Receiver<bool>>,
    discover_feedback: Option<(Instant, &'static str)>,
    layout_worker: LayoutWorker,
}

enum WorkerEvent {
    ResourcesUpdated(ResourceSnapshot),
    RosUpdated {
        ros: RosSnapshot,
        raw_graph: GraphSnapshot,
    },
    RosRefreshFailed(String),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ConnectionStatus {
    #[default]
    Connecting,
    Connected,
    Error,
}

#[derive(Debug, Default)]
struct DashboardSnapshot {
    resources: ResourceSnapshot,
    ros: RosSnapshot,
    raw_graph: GraphSnapshot,
    graph: GraphSnapshot,
    layout: Option<LayoutedGraph>,
    last_error: Option<String>,
    last_updated_label: Option<String>,
}

#[derive(Clone, Debug, Default)]
struct ResourceSnapshot {
    cpu_value: String,
    memory_value: String,
    network_rx: String,
    network_tx: String,
    cpu_history: Vec<TimedSample>,
    memory_history: Vec<TimedSample>,
    network_history: Vec<TimedNetworkSample>,
}

#[derive(Clone, Debug, Default)]
struct RosSnapshot {
    topic_count: usize,
    node_count: usize,
    action_count: usize,
    service_count: usize,
}

#[derive(Clone, Debug, Default)]
pub struct GraphSnapshot {
    pub topics: Vec<GraphTopic>,
    pub isolated_nodes: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct GraphTopic {
    pub name: String,
    pub publishers: Vec<String>,
    pub subscribers: Vec<String>,
    pub local_only: bool,
}

/// Rolling window used to compute a rate from raw counters. Reporting rate
/// over a ~1 s window (regardless of sample frequency) matches how
/// gnome-system-monitor displays values — small sub-second jitter averages
/// out instead of painting visible spikes on the chart.
const RATE_WINDOW: Duration = Duration::from_secs(1);

struct SystemSampler {
    start_time: Instant,
    /// Raw jiffy counters `(sampled_at, total, idle)` over the rate window.
    cpu_counter_history: VecDeque<(Instant, u64, u64)>,
    /// Raw byte counters `(sampled_at, rx_bytes, tx_bytes)` over the rate window.
    net_counter_history: VecDeque<(Instant, u64, u64)>,
    cpu_history: VecDeque<TimedSample>,
    memory_history: VecDeque<TimedSample>,
    network_history: VecDeque<TimedNetworkSample>,
}

impl DashboardPage {
    pub fn new() -> Self {
        let start_time = Instant::now();
        let (event_tx, event_rx) = mpsc::channel();

        let resource_tx = event_tx.clone();
        thread::spawn(move || resource_worker_loop(start_time, resource_tx));
        thread::spawn(move || ros_worker_loop(event_tx));

        Self {
            worker_rx: Some(event_rx),
            start_time,
            snapshot: DashboardSnapshot::default(),
            connection_status: ConnectionStatus::Connecting,
            discover_rx: None,
            discover_feedback: None,
            layout_worker: LayoutWorker::new(),
        }
    }

    pub fn show(&mut self, ctx: &egui::Context, graph_filter: &mut GraphFilter) {
        // Poll for completed graph layout first so it's available this frame.
        if let Some(layout) = self.layout_worker.poll() {
            self.snapshot.layout = layout;
        }
        self.drain_worker_events(graph_filter);
        self.drain_discover_result();
        self.render(ctx, graph_filter);
        // Resource samples arrive once per second but the chart needs to
        // scroll smoothly between them. Repaint at ~20 FPS so the time axis
        // advances continuously; y-values only change when a new 1 Hz sample
        // lands, matching the calmness of gnome-system-monitor.
        ctx.request_repaint_after(Duration::from_millis(50));
    }

    fn drain_discover_result(&mut self) {
        let Some(rx) = &self.discover_rx else { return };
        if rx.try_recv().is_ok() {
            self.discover_feedback = Some((Instant::now(), "Discovery triggered."));
            self.discover_rx = None;
        }
    }

    fn drain_worker_events(&mut self, graph_filter: &GraphFilter) {
        let Some(worker_rx) = &self.worker_rx else {
            return;
        };

        while let Ok(event) = worker_rx.try_recv() {
            match event {
                WorkerEvent::ResourcesUpdated(resources) => {
                    self.snapshot.resources = resources;
                    self.snapshot.last_updated_label = Some(format_clock_label());
                }
                WorkerEvent::RosUpdated { ros, raw_graph } => {
                    self.snapshot.ros = ros;
                    let filtered = apply_filter(&raw_graph, graph_filter);
                    self.layout_worker.submit(filtered.clone());
                    self.snapshot.graph = filtered;
                    self.snapshot.raw_graph = raw_graph;
                    self.snapshot.last_error = None;
                    self.connection_status = ConnectionStatus::Connected;
                }
                WorkerEvent::RosRefreshFailed(error) => {
                    self.snapshot.last_error = Some(error);
                    self.connection_status = ConnectionStatus::Error;
                }
            }
        }
    }

    fn render(&mut self, ctx: &egui::Context, graph_filter: &mut GraphFilter) {
        self.render_top_bar(ctx);
        self.render_body(ctx, graph_filter);
    }

    fn render_top_bar(&mut self, ctx: &egui::Context) {
        if self.discover_feedback.is_some_and(|(t, _)| t.elapsed() > Duration::from_secs(3)) {
            self.discover_feedback = None;
        }

        egui::TopBottomPanel::top("dashboard_header")
            .exact_height(52.0)
            .frame(
                egui::Frame::new()
                    .fill(PAGE_BG)
                    .inner_margin(egui::Margin::symmetric(12, 8)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("System Dashboard");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        render_status_badge(ui, self.connection_status);
                        if let Some(last_updated_label) = &self.snapshot.last_updated_label {
                            ui.label(last_updated_label);
                        }
                        ui.add_space(8.0);
                        let busy = self.discover_rx.is_some();
                        let btn = egui::Button::new(if busy { "Discovering..." } else { "Discover" })
                            .corner_radius(egui::CornerRadius::same(6));
                        if ui.add_enabled(!busy, btn).clicked() {
                            let (tx, rx) = mpsc::channel();
                            self.discover_rx = Some(rx);
                            thread::spawn(move || {
                                let _ = crate::client::send_request(
                                    crate::command::protocol::CommandRequest::Discover(
                                        crate::command::protocol::DiscoverRequest,
                                    ),
                                );
                                let _ = tx.send(true);
                            });
                        }
                        if let Some((_, msg)) = &self.discover_feedback {
                            ui.label(egui::RichText::new(*msg).color(egui::Color32::from_rgb(80, 140, 80)));
                        }
                    });
                });
            });
    }

    fn render_body(&mut self, ctx: &egui::Context, graph_filter: &mut GraphFilter) {
        egui::TopBottomPanel::bottom("dashboard_resources")
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(PAGE_BG)
                    .inner_margin(egui::Margin {
                        left: 12,
                        right: 12,
                        top: 6,
                        bottom: 12,
                    }),
            )
            .show(ctx, |ui| {
                dashboard_frame(ui, "Live System Resources", |ui| {
                    let now_secs = self.start_time.elapsed().as_secs_f64();
                    resource_section(
                        ui,
                        "CPU Usage",
                        &self.snapshot.resources.cpu_value,
                        now_secs,
                        &self.snapshot.resources.cpu_history,
                        egui::Color32::from_rgb(96, 183, 255),
                    );
                    ui.add_space(8.0);
                    resource_section(
                        ui,
                        "Memory Used",
                        &self.snapshot.resources.memory_value,
                        now_secs,
                        &self.snapshot.resources.memory_history,
                        egui::Color32::from_rgb(122, 208, 124),
                    );
                    ui.add_space(8.0);
                    network_resource_section(
                        ui,
                        "Network",
                        &self.snapshot.resources.network_rx,
                        &self.snapshot.resources.network_tx,
                        now_secs,
                        &self.snapshot.resources.network_history,
                    );
                });
            });

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(PAGE_BG)
                    .inner_margin(egui::Margin::same(12)),
            )
            .show(ctx, |ui| {
                let ros_meta = format!(
                    "{} topics  ·  {} nodes  ·  {} actions  ·  {} services",
                    self.snapshot.ros.topic_count,
                    self.snapshot.ros.node_count,
                    self.snapshot.ros.action_count,
                    self.snapshot.ros.service_count,
                );
                dashboard_frame_with_meta(ui, "ROS Graph", &ros_meta, |ui| {
                    if render_graph_filter_bar(ui, graph_filter) {
                        let filtered = apply_filter(&self.snapshot.raw_graph, graph_filter);
                        self.layout_worker.submit(filtered.clone());
                        self.snapshot.graph = filtered;
                    }
                    render_ros_graph(ui, self.snapshot.layout.as_ref(), &self.snapshot.graph);
                });
            });
    }
}

impl Default for DashboardPage {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemSampler {
    fn new(start_time: Instant) -> Self {
        Self {
            start_time,
            cpu_counter_history: VecDeque::new(),
            net_counter_history: VecDeque::new(),
            cpu_history: VecDeque::new(),
            memory_history: VecDeque::new(),
            network_history: VecDeque::new(),
        }
    }

    /// Read /proc counters and push them into the rolling windows. Call this
    /// at a high frequency (4 Hz) so the 1 s rate averages see several
    /// samples; chart history is *not* touched here.
    fn refresh_counters(&mut self) -> anyhow::Result<()> {
        self.refresh_cpu_counter().context("read CPU stats")?;
        self.refresh_net_counter().context("read network stats")?;
        Ok(())
    }

    /// Compute the current averaged CPU / network rate plus an instantaneous
    /// memory reading, append one point to each chart history, and return
    /// the snapshot to publish. Call this at the display cadence (1 Hz).
    fn snapshot(&mut self) -> anyhow::Result<ResourceSnapshot> {
        let cpu_usage = self.current_cpu_rate();
        let memory = self.sample_memory_usage().context("read memory stats")?;
        let network = self.current_network_rate();
        let now_secs = self.start_time.elapsed().as_secs_f64();

        if let Some(cpu_usage) = cpu_usage {
            self.cpu_history.push_back(TimedSample {
                time_secs: now_secs,
                value: cpu_usage,
            });
        }
        self.memory_history.push_back(TimedSample {
            time_secs: now_secs,
            value: memory.usage_percent,
        });
        if let Some(network) = network {
            self.network_history.push_back(TimedNetworkSample {
                time_secs: now_secs,
                rx_bytes_per_sec: network.rx_bytes_per_sec,
                tx_bytes_per_sec: network.tx_bytes_per_sec,
            });
        }
        self.trim_history(now_secs - HISTORY_RETENTION_SECS);

        Ok(ResourceSnapshot {
            cpu_value: format!("{:.0}%", cpu_usage.unwrap_or_default()),
            memory_value: format!(
                "{:.0}% ({:.1} / {:.1} GB)",
                memory.usage_percent, memory.used_gb, memory.total_gb
            ),
            network_rx: format_rate(network.map(|sample| sample.rx_bytes_per_sec).unwrap_or(0.0)),
            network_tx: format_rate(network.map(|sample| sample.tx_bytes_per_sec).unwrap_or(0.0)),
            cpu_history: self.cpu_history.iter().copied().collect(),
            memory_history: self.memory_history.iter().copied().collect(),
            network_history: self.network_history.iter().copied().collect(),
        })
    }

    fn trim_history(&mut self, min_time_secs: f64) {
        trim_timed_history(&mut self.cpu_history, min_time_secs);
        trim_timed_history(&mut self.memory_history, min_time_secs);
        trim_timed_network_history(&mut self.network_history, min_time_secs);
    }

    fn refresh_cpu_counter(&mut self) -> anyhow::Result<()> {
        let stat = fs::read_to_string("/proc/stat").context("read /proc/stat")?;
        let line = stat.lines().next().context("missing aggregate CPU line")?;
        let mut parts = line.split_whitespace();
        let _ = parts.next();

        let values = parts
            .take(8)
            .map(|part| part.parse::<u64>().context("parse CPU field"))
            .collect::<anyhow::Result<Vec<_>>>()?;

        let idle = values.get(3).copied().unwrap_or(0) + values.get(4).copied().unwrap_or(0);
        let total: u64 = values.iter().sum();

        let now = Instant::now();
        self.cpu_counter_history.push_back((now, total, idle));
        prune_counter_window(&mut self.cpu_counter_history, now, RATE_WINDOW);
        Ok(())
    }

    fn current_cpu_rate(&self) -> Option<f32> {
        if self.cpu_counter_history.len() < 2 {
            return None;
        }
        // Average over the oldest-to-newest pair in the rolling window. The
        // oldest entry is up to ~RATE_WINDOW old; the newest is whatever the
        // last refresh captured.
        let (_, oldest_total, oldest_idle) = *self.cpu_counter_history.front()?;
        let (_, newest_total, newest_idle) = *self.cpu_counter_history.back()?;
        let total_delta = newest_total.saturating_sub(oldest_total);
        let idle_delta = newest_idle.saturating_sub(oldest_idle);
        if total_delta == 0 {
            return None;
        }
        let usage = ((total_delta.saturating_sub(idle_delta)) as f32 / total_delta as f32) * 100.0;
        Some(usage.clamp(0.0, 100.0))
    }

    fn sample_memory_usage(&self) -> anyhow::Result<MemorySample> {
        let meminfo = fs::read_to_string("/proc/meminfo").context("read /proc/meminfo")?;
        let mut total_kb = None;
        let mut available_kb = None;

        for line in meminfo.lines() {
            if line.starts_with("MemTotal:") {
                total_kb = parse_meminfo_kb(line);
            } else if line.starts_with("MemAvailable:") {
                available_kb = parse_meminfo_kb(line);
            }
        }

        let total_kb = total_kb.context("missing MemTotal")?;
        let available_kb = available_kb.context("missing MemAvailable")?;
        let used_kb = total_kb.saturating_sub(available_kb);

        Ok(MemorySample {
            used_gb: kb_to_gb(used_kb),
            total_gb: kb_to_gb(total_kb),
            usage_percent: (used_kb as f32 / total_kb as f32) * 100.0,
        })
    }

    fn refresh_net_counter(&mut self) -> anyhow::Result<()> {
        let now = Instant::now();
        let netdev = fs::read_to_string("/proc/net/dev").context("read /proc/net/dev")?;
        let mut rx_bytes = 0_u64;
        let mut tx_bytes = 0_u64;

        for line in netdev.lines().skip(2) {
            let Some((iface, stats)) = line.split_once(':') else {
                continue;
            };
            if iface.trim() == "lo" {
                continue;
            }

            let fields = stats.split_whitespace().collect::<Vec<_>>();
            if fields.len() < 16 {
                continue;
            }

            rx_bytes = rx_bytes.saturating_add(fields[0].parse::<u64>().unwrap_or(0));
            tx_bytes = tx_bytes.saturating_add(fields[8].parse::<u64>().unwrap_or(0));
        }

        self.net_counter_history.push_back((now, rx_bytes, tx_bytes));
        prune_counter_window(&mut self.net_counter_history, now, RATE_WINDOW);
        Ok(())
    }

    fn current_network_rate(&self) -> Option<NetworkSample> {
        if self.net_counter_history.len() < 2 {
            return None;
        }
        let (oldest_time, oldest_rx, oldest_tx) = *self.net_counter_history.front()?;
        let (newest_time, newest_rx, newest_tx) = *self.net_counter_history.back()?;
        let elapsed_secs = newest_time
            .saturating_duration_since(oldest_time)
            .as_secs_f32();
        if elapsed_secs <= 0.0 {
            return None;
        }

        let rx_bytes_per_sec =
            (newest_rx.saturating_sub(oldest_rx) as f32 / elapsed_secs).max(0.0);
        let tx_bytes_per_sec =
            (newest_tx.saturating_sub(oldest_tx) as f32 / elapsed_secs).max(0.0);

        Some(NetworkSample {
            rx_bytes_per_sec,
            tx_bytes_per_sec,
        })
    }
}

/// Remove counter samples older than `now - window`, but always keep at least
/// one entry so the next `.front()` call can compute a rate. The front entry
/// is therefore at most `window + one_sample_interval` old.
fn prune_counter_window<T>(history: &mut VecDeque<(Instant, T, T)>, now: Instant, window: Duration) {
    while history.len() > 1 {
        let front_time = history.front().unwrap().0;
        if now.saturating_duration_since(front_time) > window {
            history.pop_front();
        } else {
            break;
        }
    }
}

fn resource_worker_loop(start_time: Instant, tx: mpsc::Sender<WorkerEvent>) {
    let mut sampler = SystemSampler::new(start_time);
    // Read /proc counters at 4 Hz so the rolling 1 s rate is an average over
    // several samples (not a fragile single delta). Publish one snapshot
    // per second to the UI so the chart gets one data point per second and
    // the numeric readout stays stable — gnome-system-monitor style.
    let read_interval = Duration::from_millis(100);
    let emit_interval = Duration::from_secs(1);
    let mut last_emit = Instant::now();
    loop {
        let deadline = Instant::now() + read_interval;
        let _ = sampler.refresh_counters();
        if last_emit.elapsed() >= emit_interval {
            if let Ok(snapshot) = sampler.snapshot() {
                if tx.send(WorkerEvent::ResourcesUpdated(snapshot)).is_err() {
                    return;
                }
            }
            last_emit = Instant::now();
        }
        let now = Instant::now();
        if deadline > now {
            thread::sleep(deadline - now);
        }
    }
}

fn ros_worker_loop(tx: mpsc::Sender<WorkerEvent>) {
    let interval = Duration::from_secs(1);
    loop {
        let started = Instant::now();
        let event = match collect_ros_snapshot() {
            Ok((ros, raw_graph)) => WorkerEvent::RosUpdated { ros, raw_graph },
            Err(error) => WorkerEvent::RosRefreshFailed(error.to_string()),
        };
        if tx.send(event).is_err() {
            return;
        }
        let elapsed = started.elapsed();
        if elapsed < interval {
            thread::sleep(interval - elapsed);
        }
    }
}

fn collect_ros_snapshot() -> anyhow::Result<(RosSnapshot, GraphSnapshot)> {
    let (topics, nodes, totals) = fetch_topic_graph().context("fetch topic graph")?;
    let graph = build_graph_snapshot(&topics, &nodes);
    Ok((
        RosSnapshot {
            topic_count: totals.topics,
            node_count: totals.nodes,
            action_count: totals.actions,
            service_count: totals.services,
        },
        graph,
    ))
}

/// Counts pulled from the server for the dashboard headline. Unfiltered.
struct GraphTotals {
    topics: usize,
    nodes: usize,
    actions: usize,
    services: usize,
}

/// Single-shot RPC to pull the full ROS graph. Replaces the previous
/// N+2 sequential calls (TopicList + NodeList + per-topic TopicInfo).
fn fetch_topic_graph() -> anyhow::Result<(Vec<TopicDetails>, Vec<NodeDetails>, GraphTotals)> {
    match send_request(CommandRequest::TopicGraph(TopicGraphRequest {
        // GUI applies its own filters (tf/params/debug/leaf) — let the server
        // return every topic so the user can opt out of all filtering.
        include_hidden: true,
    }))? {
        CommandResponse::TopicGraph(response) => {
            let totals = GraphTotals {
                topics: response.total_topics_count,
                nodes: response.total_nodes_count,
                actions: response.total_actions_count,
                services: response.total_services_count,
            };
            Ok((response.topics, response.nodes, totals))
        }
        CommandResponse::Error(error) => anyhow::bail!(error.message),
        _ => anyhow::bail!("unexpected response for topic graph"),
    }
}

pub fn fetch_graph_snapshot() -> anyhow::Result<GraphSnapshot> {
    let (topics, nodes, _) = fetch_topic_graph()?;
    Ok(build_graph_snapshot(&topics, &nodes))
}

fn build_graph_snapshot(topics: &[TopicDetails], nodes: &[NodeDetails]) -> GraphSnapshot {
    let mut graph_topics: Vec<GraphTopic> = Vec::new();
    for details in topics
        .iter()
        .filter(|t| t.publisher_count > 0 || t.subscription_count > 0)
    {
        let publishers: Vec<String> = {
            let mut v: Vec<String> = details
                .publishers
                .iter()
                .filter_map(|p| {
                    p.node_name
                        .as_ref()
                        .map(|name| full_node_name(p.node_namespace.as_deref().unwrap_or("/"), name))
                })
                .collect();
            v.sort();
            v.dedup();
            v
        };
        let subscribers: Vec<String> = {
            let mut v: Vec<String> = details
                .subscriptions
                .iter()
                .filter_map(|s| {
                    s.node_name
                        .as_ref()
                        .map(|name| full_node_name(s.node_namespace.as_deref().unwrap_or("/"), name))
                })
                .collect();
            v.sort();
            v.dedup();
            v
        };
        graph_topics.push(GraphTopic {
            name: details.name.clone(),
            publishers,
            subscribers,
            local_only: details.local_only,
        });
    }

    // Compute isolated nodes: node names not appearing in any topic's pub/sub lists
    let connected_nodes: std::collections::HashSet<&str> = graph_topics
        .iter()
        .flat_map(|t| {
            t.publishers
                .iter()
                .map(|s| s.as_str())
                .chain(t.subscribers.iter().map(|s| s.as_str()))
        })
        .collect();

    let isolated_nodes: Vec<String> = nodes
        .iter()
        .map(|n| full_node_name(&n.namespace, &n.name))
        .filter(|full_name| !connected_nodes.contains(full_name.as_str()))
        .collect();

    GraphSnapshot { topics: graph_topics, isolated_nodes }
}


fn render_status_badge(ui: &mut egui::Ui, connection_status: ConnectionStatus) {
    let (label, color) = match connection_status {
        ConnectionStatus::Connecting => ("Connecting", egui::Color32::from_rgb(197, 150, 58)),
        ConnectionStatus::Connected => ("Connected", egui::Color32::from_rgb(58, 173, 110)),
        ConnectionStatus::Error => ("Error", egui::Color32::from_rgb(197, 84, 84)),
    };

    egui::Frame::new()
        .fill(color.gamma_multiply(0.15))
        .stroke(egui::Stroke::new(1.0, color))
        .corner_radius(egui::CornerRadius::same(6))
        .inner_margin(egui::Margin::symmetric(10, 6))
        .show(ui, |ui| {
            ui.colored_label(color, label);
        });
}

fn dashboard_frame(ui: &mut egui::Ui, title: &str, add_contents: impl FnOnce(&mut egui::Ui)) {
    dashboard_frame_with_meta(ui, title, "", add_contents);
}

fn dashboard_frame_with_meta(
    ui: &mut egui::Ui,
    title: &str,
    meta: &str,
    add_contents: impl FnOnce(&mut egui::Ui),
) {
    egui::Frame::group(ui.style())
        .fill(egui::Color32::from_rgb(252, 253, 255))
        .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(210, 217, 226)))
        .corner_radius(egui::CornerRadius::same(8))
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading(title);
                if !meta.is_empty() {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(meta);
                    });
                }
            });
            ui.add_space(8.0);
            add_contents(ui);
        });
}

fn resource_section(
    ui: &mut egui::Ui,
    title: &str,
    current_value: &str,
    now_secs: f64,
    samples: &[TimedSample],
    color: egui::Color32,
) {
    ui.horizontal(|ui| {
        ui.label(title);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(egui::RichText::new(current_value).color(color).strong());
        });
    });
    ui.add_space(6.0);

    let desired_size = egui::vec2(ui.available_width(), GRAPH_HEIGHT);
    let (rect, _) = ui.allocate_exact_size(desired_size, egui::Sense::hover());
    let painter = ui.painter_at(rect);

    let y_max = 100.0_f32;
    let unit = "%";

    draw_chart_frame(&painter, rect);
    draw_grid_lines(&painter, rect, 5);
    draw_time_guides(ui, &painter, rect);
    draw_axis_labels(ui, &painter, rect, y_max, unit);

    let gr = graph_inner_rect(rect);
    let series = build_single_series_points(rect, now_secs, samples, y_max);
    draw_series(&painter.with_clip_rect(gr), series, color);
}

fn network_resource_section(
    ui: &mut egui::Ui,
    title: &str,
    rx_rate: &str,
    tx_rate: &str,
    now_secs: f64,
    samples: &[TimedNetworkSample],
) {
    let rx_color = egui::Color32::from_rgb(255, 176, 90);
    let tx_color = egui::Color32::from_rgb(114, 168, 255);
    ui.horizontal(|ui| {
        ui.label(title);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(egui::RichText::new(tx_rate).color(tx_color).strong());
            ui.colored_label(tx_color, "Sending");
            ui.add_space(12.0);
            ui.label(egui::RichText::new(rx_rate).color(rx_color).strong());
            ui.colored_label(rx_color, "Receiving");
        });
    });
    ui.add_space(6.0);

    let peak = samples.iter().fold(0.0_f32, |peak, sample| {
        peak.max(sample.rx_bytes_per_sec).max(sample.tx_bytes_per_sec)
    });
    let peak_mib = (peak / (1024.0 * 1024.0)).max(1.0 / 1024.0);
    let y_max_mib = nice_network_max_mib(peak_mib);
    let (y_max, unit_label, scale) = network_axis_unit(y_max_mib);

    let desired_size = egui::vec2(ui.available_width(), GRAPH_HEIGHT);
    let (rect, _) = ui.allocate_exact_size(desired_size, egui::Sense::hover());
    let painter = ui.painter_at(rect);

    draw_chart_frame(&painter, rect);
    draw_grid_lines(&painter, rect, 5);
    draw_time_guides(ui, &painter, rect);
    draw_axis_labels(ui, &painter, rect, y_max, unit_label);

    let gr = graph_inner_rect(rect);
    let graph_painter = painter.with_clip_rect(gr);
    draw_series(
        &graph_painter,
        build_network_series_points(rect, now_secs, samples, y_max, scale, true),
        rx_color,
    );
    draw_series(
        &graph_painter,
        build_network_series_points(rect, now_secs, samples, y_max, scale, false),
        tx_color,
    );
}

fn draw_chart_frame(painter: &egui::Painter, rect: egui::Rect) {
    let graph_rect = graph_inner_rect(rect);
    painter.rect_filled(
        graph_rect,
        4.0,
        egui::Color32::from_rgb(247, 249, 252),
    );
    painter.rect_stroke(
        graph_rect,
        4.0,
        egui::Stroke::new(1.0, egui::Color32::from_rgb(214, 220, 228)),
        egui::StrokeKind::Inside,
    );
}

fn draw_grid_lines(painter: &egui::Painter, rect: egui::Rect, segments: usize) {
    let graph_rect = graph_inner_rect(rect);
    for step in 1..segments {
        let t = step as f32 / segments as f32;
        let y = graph_rect.bottom() - graph_rect.height() * t;
        painter.line_segment(
            [egui::pos2(graph_rect.left(), y), egui::pos2(graph_rect.right(), y)],
            egui::Stroke::new(1.0, egui::Color32::from_rgb(231, 235, 241)),
        );
    }
}

fn draw_time_guides(ui: &egui::Ui, painter: &egui::Painter, rect: egui::Rect) {
    let graph_rect = graph_inner_rect(rect);
    for age_secs in (0..=60).step_by(10) {
        let x = graph_rect.right() - graph_rect.width() * (age_secs as f32 / 60.0);
        painter.line_segment(
            [egui::pos2(x, graph_rect.top()), egui::pos2(x, graph_rect.bottom())],
            egui::Stroke::new(1.0, egui::Color32::from_rgb(231, 235, 241)),
        );
        painter.text(
            egui::pos2(x, graph_rect.bottom() + 4.0),
            egui::Align2::CENTER_TOP,
            format!("{age_secs}s"),
            egui::TextStyle::Small.resolve(ui.style()),
            egui::Color32::from_gray(120),
        );
    }
}

fn draw_axis_labels(
    ui: &egui::Ui,
    painter: &egui::Painter,
    rect: egui::Rect,
    y_max: f32,
    unit: &str,
) {
    let graph_rect = graph_inner_rect(rect);
    let label_x = graph_rect.right() + 6.0;
    for step in 0..=5 {
        let value = y_max * (5 - step) as f32 / 5.0;
        let y = graph_rect.top() + graph_rect.height() * (step as f32 / 5.0);
        let label = match unit {
            "%" => format!("{value:.0}%"),
            _ => format!("{value:.1} {unit}"),
        };
        painter.text(
            egui::pos2(label_x, y),
            egui::Align2::LEFT_CENTER,
            label,
            egui::TextStyle::Small.resolve(ui.style()),
            egui::Color32::from_gray(110),
        );
    }
}

fn draw_series(painter: &egui::Painter, series: Vec<egui::Pos2>, color: egui::Color32) {
    if series.is_empty() {
        return;
    }
    if series.len() == 1 {
        painter.circle_filled(series[0], 2.5, color);
        return;
    }

    painter.add(egui::Shape::line(series, egui::Stroke::new(1.5, color)));
}

fn build_single_series_points(
    rect: egui::Rect,
    now_secs: f64,
    samples: &[TimedSample],
    y_max: f32,
) -> Vec<egui::Pos2> {
    let values = samples
        .iter()
        .map(|sample| (sample.time_secs, sample.value))
        .collect::<Vec<_>>();
    build_visible_series_points(rect, now_secs, &values, y_max)
}

fn build_network_series_points(
    rect: egui::Rect,
    now_secs: f64,
    samples: &[TimedNetworkSample],
    y_max: f32,
    scale: f32,
    receive: bool,
) -> Vec<egui::Pos2> {
    let values = samples
        .iter()
        .map(|sample| {
            let value = if receive {
                sample.rx_bytes_per_sec
            } else {
                sample.tx_bytes_per_sec
            };
            (sample.time_secs, value * scale)
        })
        .collect::<Vec<_>>();
    build_visible_series_points(rect, now_secs, &values, y_max)
}

fn build_visible_series_points(
    rect: egui::Rect,
    now_secs: f64,
    samples: &[(f64, f32)],
    y_max: f32,
) -> Vec<egui::Pos2> {
    let gr = graph_inner_rect(rect);
    let window = HISTORY_WINDOW_SECS as f32;
    let one_step = gr.width() / window;

    let age_to_x = |raw_age: f32| gr.right() - gr.width() * (raw_age / window);
    let val_to_y = |v: f32| {
        let norm = if y_max > 0.0 { v.clamp(0.0, y_max) / y_max } else { 0.0 };
        gr.bottom() - gr.height() * norm
    };

    if samples.is_empty() {
        return vec![];
    }

    let off_left = samples.iter().rposition(|&(time_secs, _)| {
        age_to_x((now_secs - time_secs).max(0.0) as f32) < gr.left()
    });
    let start = off_left.unwrap_or(0);

    let mut points = Vec::new();

    for &(time_secs, value) in &samples[start..] {
        let raw_age = (now_secs - time_secs).max(0.0) as f32;
        points.push(egui::pos2(age_to_x(raw_age), val_to_y(value)));
    }

    // Extend last value 1 s past the right edge; clip rect hides it, giving
    // smooth gnome-system-monitor-style continuity at the 0 s mark.
    let last_y = val_to_y(samples.last().unwrap().1);
    points.push(egui::pos2(gr.right() + one_step, last_y));

    points
}

fn graph_inner_rect(rect: egui::Rect) -> egui::Rect {
    egui::Rect::from_min_max(
        egui::pos2(rect.left() + LEFT_PADDING, rect.top() + TOP_PADDING),
        egui::pos2(rect.right() - RIGHT_AXIS_WIDTH, rect.bottom() - BOTTOM_PADDING),
    )
}

fn trim_timed_history(history: &mut VecDeque<TimedSample>, min_time_secs: f64) {
    while history
        .front()
        .is_some_and(|sample| sample.time_secs < min_time_secs)
    {
        history.pop_front();
    }
}

fn trim_timed_network_history(history: &mut VecDeque<TimedNetworkSample>, min_time_secs: f64) {
    while history
        .front()
        .is_some_and(|sample| sample.time_secs < min_time_secs)
    {
        history.pop_front();
    }
}

fn nice_network_max_mib(peak_mib: f32) -> f32 {
    nice_linear_max((peak_mib * 1.1).max(1.0 / 1024.0))
}

fn nice_linear_max(value: f32) -> f32 {
    if value <= 0.0 {
        return 1.0;
    }
    let magnitude = 10_f32.powf(value.log10().floor());
    let normalized = value / magnitude;
    let nice = if normalized <= 1.0 {
        1.0
    } else if normalized <= 2.0 {
        2.0
    } else if normalized <= 5.0 {
        5.0
    } else {
        10.0
    };
    nice * magnitude
}

fn network_axis_unit(y_max_mib: f32) -> (f32, &'static str, f32) {
    if y_max_mib < 1.0 {
        (y_max_mib * 1024.0, "KiB/s", 1.0 / 1024.0)
    } else {
        (y_max_mib, "MiB/s", 1.0 / (1024.0 * 1024.0))
    }
}

fn format_rate(value_bytes_per_sec: f32) -> String {
    if value_bytes_per_sec < 1024.0 * 1024.0 {
        format!("{:.1} KiB/s", value_bytes_per_sec / 1024.0)
    } else {
        format!("{:.1} MiB/s", value_bytes_per_sec / (1024.0 * 1024.0))
    }
}

fn full_node_name(namespace: &str, name: &str) -> String {
    if namespace.is_empty() || namespace == "/" {
        format!("/{name}")
    } else {
        format!("{}/{}", namespace.trim_end_matches('/'), name)
    }
}

fn format_clock_label() -> String {
    Local::now().format("%H:%M:%S").to_string()
}

struct MemorySample {
    used_gb: f32,
    total_gb: f32,
    usage_percent: f32,
}

#[derive(Clone, Copy, Default)]
struct NetworkSample {
    rx_bytes_per_sec: f32,
    tx_bytes_per_sec: f32,
}

#[derive(Clone, Copy, Debug, Default)]
struct TimedSample {
    time_secs: f64,
    value: f32,
}

#[derive(Clone, Copy, Debug, Default)]
struct TimedNetworkSample {
    time_secs: f64,
    rx_bytes_per_sec: f32,
    tx_bytes_per_sec: f32,
}

fn parse_meminfo_kb(line: &str) -> Option<u64> {
    line.split_whitespace().nth(1)?.parse::<u64>().ok()
}

fn kb_to_gb(kb: u64) -> f32 {
    kb as f32 / 1024.0 / 1024.0
}
