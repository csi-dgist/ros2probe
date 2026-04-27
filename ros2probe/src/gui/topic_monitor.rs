use std::{
    collections::VecDeque,
    io::{BufRead, BufReader, Write as IoWrite},
    path::PathBuf,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use eframe::egui;
use serde::{Deserialize, Serialize};

use crate::{
    client::send_request,
    command::protocol::{
        CommandRequest, CommandResponse, TopicBwStartRequest, TopicBwStopRequest,
        TopicDelayStartRequest, TopicDelayStopRequest, TopicDetails, TopicEchoStartRequest,
        TopicEchoStatusRequest, TopicEchoStopRequest, TopicHzBwStatusRequest, TopicHzStartRequest,
        TopicHzStopRequest, TopicInfoRequest, TopicListRequest,
    },
};

use super::dashboard::{fetch_graph_snapshot, GraphSnapshot};
use super::ros_graph::{
    apply_filter, is_recordable_topic, render_graph_filter_bar,
    render_ros_graph_selectable, GraphFilter, LayoutWorker, LayoutedGraph,
};

const PAGE_BG: egui::Color32 = egui::Color32::from_rgb(230, 232, 236);
const HZ_COLOR: egui::Color32 = egui::Color32::from_rgb(96, 183, 255);
const BW_COLOR: egui::Color32 = egui::Color32::from_rgb(122, 208, 124);
const DELAY_COLOR: egui::Color32 = egui::Color32::from_rgb(255, 165, 60);
const ECHO_COLOR: egui::Color32 = egui::Color32::from_rgb(160, 100, 220);

const HISTORY_WINDOW_SECS: f64 = 60.0;
const HISTORY_RETENTION_SECS: f64 = 65.0;
const GRAPH_HEIGHT: f32 = 110.0;
const LEFT_PADDING: f32 = 8.0;
const RIGHT_AXIS_WIDTH: f32 = 56.0;
const TOP_PADDING: f32 = 8.0;
const BOTTOM_PADDING: f32 = 22.0;

const MAX_ECHO_DISPLAY: usize = 100;

pub struct TopicMonitorPage {
    cmd_tx: mpsc::Sender<MonitorCommand>,
    event_rx: mpsc::Receiver<MonitorEvent>,
    topics: Vec<String>,
    topic_filter: String,
    selected_topic: Option<String>,
    state: MonitorState,
    topic_info: Option<TopicDetails>,
    start_time: Instant,
    error: Option<String>,
    window_size: usize,
    raw_graph: GraphSnapshot,
    graph: GraphSnapshot,
    layout: Option<LayoutedGraph>,
    layout_worker: LayoutWorker,
    echo_enabled: bool,
    echo_messages: VecDeque<String>,
    internal_topics_open: bool,
}

enum MonitorCommand {
    SelectTopic { topic: String, window_size: usize },
    SetWindowSize(usize),
    Deselect,
    SetEchoEnabled(bool),
}

enum MonitorEvent {
    TopicsUpdated(Vec<String>),
    Snapshot(MonitorSnapshot),
    Error(String),
    GraphUpdated(GraphSnapshot),
    EchoMessages(Vec<String>),
    TopicInfoUpdated(TopicDetails),
}

#[derive(Clone, Debug, Default)]
struct MonitorState {
    hz_current: Option<f64>,
    bw_current: Option<f64>,
    delay_current: Option<f64>,
    hz_history: VecDeque<TimedSample>,
    bw_history: VecDeque<TimedSample>,
    delay_history: VecDeque<TimedSample>,
    hz_stats: Option<HzStats>,
    bw_stats: Option<BwStats>,
    delay_stats: Option<DelayStats>,
}

#[derive(Clone, Debug)]
struct MonitorSnapshot {
    time_secs: f64,
    hz: Option<f64>,
    bw_bytes_per_sec: Option<f64>,
    delay_avg_ms: Option<f64>,
    hz_stats: Option<HzStats>,
    bw_stats: Option<BwStats>,
    delay_stats: Option<DelayStats>,
}

#[derive(Clone, Debug)]
struct HzStats {
    min_period_ms: f64,
    max_period_ms: f64,
    std_dev_ms: f64,
    window: usize,
}

#[derive(Clone, Debug)]
struct BwStats {
    message_count: usize,
    mean_size_bytes: Option<f64>,
    min_size_bytes: Option<usize>,
    max_size_bytes: Option<usize>,
}

#[derive(Clone, Debug)]
struct DelayStats {
    avg_ms: f64,
    min_ms: f64,
    max_ms: f64,
    std_dev_ms: f64,
    window: usize,
}

#[derive(Clone, Copy, Debug, Default)]
struct TimedSample {
    time_secs: f64,
    value: f32,
}

impl TopicMonitorPage {
    pub fn new() -> Self {
        let start_time = Instant::now();
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();

        thread::spawn(move || monitor_worker(start_time, cmd_rx, event_tx));

        Self {
            cmd_tx,
            event_rx,
            topics: Vec::new(),
            topic_filter: String::new(),
            selected_topic: None,
            state: MonitorState::default(),
            topic_info: None,
            start_time,
            error: None,
            window_size: 100,
            raw_graph: GraphSnapshot::default(),
            graph: GraphSnapshot::default(),
            layout: None,
            layout_worker: LayoutWorker::new(),
            echo_enabled: false,
            echo_messages: VecDeque::new(),
            internal_topics_open: false,
        }
    }

    pub fn show(&mut self, ctx: &egui::Context, graph_filter: &mut GraphFilter) {
        if let Some(layout) = self.layout_worker.poll() {
            self.layout = layout;
        }
        let drained = self.drain_events(graph_filter);
        self.render(ctx, graph_filter);
        let next = if drained || self.echo_enabled || self.selected_topic.is_some() {
            Duration::from_millis(100)
        } else {
            Duration::from_secs(1)
        };
        ctx.request_repaint_after(next);
    }

    /// Called by the app when the user navigates away from this page. Stops
    /// server-side hz/bw/delay/echo sessions and pauses the worker's polling
    /// so the runtime isn't busy answering requests for a page that isn't
    /// being shown.
    pub fn pause_for_leave(&mut self) {
        if self.selected_topic.is_some() || self.echo_enabled {
            let _ = self.cmd_tx.send(MonitorCommand::SetEchoEnabled(false));
            let _ = self.cmd_tx.send(MonitorCommand::Deselect);
            self.selected_topic = None;
            self.echo_enabled = false;
            self.echo_messages.clear();
            self.state = MonitorState::default();
        }
    }

    fn drain_events(&mut self, graph_filter: &GraphFilter) -> bool {
        let mut drained = false;
        while let Ok(event) = self.event_rx.try_recv() {
            drained = true;
            match event {
                MonitorEvent::TopicsUpdated(topics) => {
                    self.topics = topics;
                }
                MonitorEvent::Snapshot(snapshot) => {
                    let state = &mut self.state;
                    state.hz_current = snapshot.hz;
                    state.bw_current = snapshot.bw_bytes_per_sec;
                    state.delay_current = snapshot.delay_avg_ms;
                    state.hz_stats = snapshot.hz_stats;
                    state.bw_stats = snapshot.bw_stats;
                    state.delay_stats = snapshot.delay_stats;
                    if let Some(hz) = snapshot.hz {
                        state.hz_history.push_back(TimedSample {
                            time_secs: snapshot.time_secs,
                            value: hz as f32,
                        });
                    }
                    if let Some(bw) = snapshot.bw_bytes_per_sec {
                        state.bw_history.push_back(TimedSample {
                            time_secs: snapshot.time_secs,
                            value: bw as f32,
                        });
                    }
                    if let Some(delay) = snapshot.delay_avg_ms {
                        state.delay_history.push_back(TimedSample {
                            time_secs: snapshot.time_secs,
                            value: delay as f32,
                        });
                    }
                    let min_time = snapshot.time_secs - HISTORY_RETENTION_SECS;
                    while state.hz_history.front().is_some_and(|s| s.time_secs < min_time) {
                        state.hz_history.pop_front();
                    }
                    while state.bw_history.front().is_some_and(|s| s.time_secs < min_time) {
                        state.bw_history.pop_front();
                    }
                    while state.delay_history.front().is_some_and(|s| s.time_secs < min_time) {
                        state.delay_history.pop_front();
                    }
                    self.error = None;
                }
                MonitorEvent::Error(err) => {
                    self.error = Some(err);
                }
                MonitorEvent::GraphUpdated(raw_graph) => {
                    let filtered = apply_filter(&raw_graph, graph_filter);
                    self.layout_worker.submit(filtered.clone());
                    self.graph = filtered;
                    self.raw_graph = raw_graph;
                }
                MonitorEvent::EchoMessages(msgs) => {
                    for msg in msgs {
                        if self.echo_messages.len() >= MAX_ECHO_DISPLAY {
                            self.echo_messages.pop_front();
                        }
                        self.echo_messages.push_back(msg);
                    }
                }
                MonitorEvent::TopicInfoUpdated(details) => {
                    self.topic_info = Some(details);
                }
            }
        }
        drained
    }

    fn render(&mut self, ctx: &egui::Context, graph_filter: &mut GraphFilter) {
        self.render_top_bar(ctx);
        self.render_topic_list(ctx);
        self.render_body(ctx, graph_filter);
    }

    fn render_top_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("monitor_header")
            .exact_height(52.0)
            .frame(
                egui::Frame::new()
                    .fill(PAGE_BG)
                    .inner_margin(egui::Margin::symmetric(12, 8)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("Topic Monitor");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if let Some(topic) = &self.selected_topic {
                            ui.label(
                                egui::RichText::new(topic)
                                    .monospace()
                                    .color(egui::Color32::from_rgb(40, 90, 140)),
                            );
                            ui.add_space(16.0);
                        }
                        let prev_window = self.window_size;
                        ui.label(egui::RichText::new("Window:").small());
                        ui.add(
                            egui::DragValue::new(&mut self.window_size)
                                .range(10..=10_000)
                                .speed(1.0),
                        );
                        if self.window_size != prev_window {
                            if self.selected_topic.is_some() {
                                let _ = self.cmd_tx.send(MonitorCommand::SetWindowSize(self.window_size));
                            }
                        }
                    });
                });
            });
    }

    fn render_topic_list(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("monitor_topic_list")
            .resizable(true)
            .default_width(220.0)
            .min_width(140.0)
            .max_width(360.0)
            .frame(
                egui::Frame::new()
                    .fill(egui::Color32::from_rgb(240, 242, 246))
                    .inner_margin(egui::Margin::same(8)),
            )
            .show(ctx, |ui| {
                ui.label(egui::RichText::new("Topics").strong());
                ui.add_space(4.0);
                egui::TextEdit::singleline(&mut self.topic_filter)
                    .hint_text("Filter...")
                    .desired_width(f32::INFINITY)
                    .show(ui);
                ui.add_space(4.0);
                ui.separator();
                ui.add_space(2.0);

                if self.topics.is_empty() {
                    ui.label(
                        egui::RichText::new("No topics found")
                            .color(egui::Color32::from_gray(140)),
                    );
                    return;
                }

                let filter = self.topic_filter.to_lowercase();
                let mut recordable: Vec<&String> = Vec::new();
                let mut internal: Vec<&String> = Vec::new();
                for topic in &self.topics {
                    if !filter.is_empty() && !topic.to_lowercase().contains(&filter) {
                        continue;
                    }
                    if is_recordable_topic(topic, &self.raw_graph) {
                        recordable.push(topic);
                    } else {
                        internal.push(topic);
                    }
                }

                let topic_btn = |ui: &mut egui::Ui,
                                 topic: &str,
                                 selected: bool,
                                 cmd_tx: &mpsc::Sender<MonitorCommand>,
                                 selected_topic: &mut Option<String>,
                                 state: &mut MonitorState,
                                 topic_info: &mut Option<_>,
                                 error: &mut Option<String>,
                                 echo_messages: &mut VecDeque<String>,
                                 window_size: usize| {
                    let label = egui::RichText::new(topic).monospace().size(11.0);
                    let btn = egui::Button::new(label)
                        .fill(if selected {
                            egui::Color32::from_rgb(40, 90, 140)
                        } else {
                            egui::Color32::TRANSPARENT
                        })
                        .stroke(egui::Stroke::NONE);
                    let response =
                        ui.add_sized([ui.available_width(), 26.0], btn).on_hover_text(topic);
                    if response.clicked() {
                        if selected {
                            let _ = cmd_tx.send(MonitorCommand::Deselect);
                            *selected_topic = None;
                            *state = MonitorState::default();
                            *topic_info = None;
                            *error = None;
                            echo_messages.clear();
                        } else {
                            let _ = cmd_tx.send(MonitorCommand::SelectTopic {
                                topic: topic.to_string(),
                                window_size,
                            });
                            *selected_topic = Some(topic.to_string());
                            *state = MonitorState::default();
                            *topic_info = None;
                            *error = None;
                            echo_messages.clear();
                        }
                    }
                };

                egui::ScrollArea::vertical().show(ui, |ui| {
                    for topic in &recordable {
                        let selected = self.selected_topic.as_deref() == Some(topic.as_str());
                        topic_btn(
                            ui,
                            topic,
                            selected,
                            &self.cmd_tx,
                            &mut self.selected_topic,
                            &mut self.state,
                            &mut self.topic_info,
                            &mut self.error,
                            &mut self.echo_messages,
                            self.window_size,
                        );
                    }

                    if !internal.is_empty() {
                        ui.add_space(4.0);
                        let header = egui::RichText::new(format!(
                            "Internal Topics ({})",
                            internal.len()
                        ))
                        .size(11.0)
                        .color(egui::Color32::from_gray(120));
                        egui::CollapsingHeader::new(header)
                            .id_salt("internal_topics_monitor")
                            .default_open(false)
                            .open(Some(self.internal_topics_open))
                            .show(ui, |ui| {
                                for topic in &internal {
                                    let selected =
                                        self.selected_topic.as_deref() == Some(topic.as_str());
                                    topic_btn(
                                        ui,
                                        topic,
                                        selected,
                                        &self.cmd_tx,
                                        &mut self.selected_topic,
                                        &mut self.state,
                                        &mut self.topic_info,
                                        &mut self.error,
                                        &mut self.echo_messages,
                                        self.window_size,
                                    );
                                }
                            })
                            .header_response
                            .clicked()
                            .then(|| {
                                self.internal_topics_open = !self.internal_topics_open;
                            });
                    }
                });
            });
    }

    fn render_body(&mut self, ctx: &egui::Context, graph_filter: &mut GraphFilter) {
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(PAGE_BG)
                    .inner_margin(egui::Margin::same(12)),
            )
            .show(ctx, |ui| {
                if self.selected_topic.is_none() {
                    // Filter bar above graph
                    if render_graph_filter_bar(ui, graph_filter) {
                        let filtered = apply_filter(&self.raw_graph, graph_filter);
                        self.layout_worker.submit(filtered.clone());
                        self.graph = filtered;
                    }
                    // Show ROS graph; click on a topic node selects it
                    if let Some(clicked) = render_ros_graph_selectable(
                        ui,
                        self.layout.as_ref(),
                        &self.graph,
                        None,
                    ) {
                        if self.topics.contains(&clicked) {
                            let _ = self.cmd_tx.send(MonitorCommand::SelectTopic {
                                topic: clicked.clone(),
                                window_size: self.window_size,
                            });
                            self.selected_topic = Some(clicked);
                            self.state = MonitorState::default();
                            self.topic_info = None;
                            self.error = None;
                            self.echo_messages.clear();
                        }
                    }
                    return;
                }

                if let Some(err) = &self.error {
                    ui.colored_label(egui::Color32::from_rgb(200, 60, 60), err);
                    return;
                }

                let now_secs = self.start_time.elapsed().as_secs_f64();

                egui::ScrollArea::vertical().show(ui, |ui| {
                    self.render_topic_info_section(ui);
                    ui.add_space(12.0);
                    self.render_hz_section(ui, now_secs);
                    ui.add_space(12.0);
                    self.render_bw_section(ui, now_secs);
                    ui.add_space(12.0);
                    self.render_delay_section(ui, now_secs);
                    ui.add_space(12.0);
                    self.render_echo_section(ui);
                });
            });
    }

    fn render_hz_section(&self, ui: &mut egui::Ui, now_secs: f64) {
        let hz_label = self
            .state
            .hz_current
            .map(|hz| format!("{hz:.2} Hz"))
            .unwrap_or_else(|| "Waiting...".to_string());

        monitor_frame(ui, "Frequency", &hz_label, HZ_COLOR, |ui| {
            if let Some(stats) = &self.state.hz_stats {
                ui.horizontal(|ui| {
                    stat_chip(ui, "min period", &format!("{:.1} ms", stats.min_period_ms));
                    ui.add_space(8.0);
                    stat_chip(ui, "max period", &format!("{:.1} ms", stats.max_period_ms));
                    ui.add_space(8.0);
                    stat_chip(ui, "std dev", &format!("{:.2} ms", stats.std_dev_ms));
                    ui.add_space(8.0);
                    stat_chip(ui, "window", &stats.window.to_string());
                });
                ui.add_space(6.0);
            }

            let hz_history: Vec<TimedSample> = self.state.hz_history.iter().copied().collect();
            let peak = hz_history.iter().fold(0.0_f32, |m, s| m.max(s.value));
            let y_max = nice_linear_max((peak * 1.1).max(1.0));

            draw_time_series(ui, now_secs, &hz_history, y_max, "Hz", HZ_COLOR);
        });
    }

    fn render_bw_section(&self, ui: &mut egui::Ui, now_secs: f64) {
        let bw_label = self
            .state
            .bw_current
            .map(|bw| format_bw(bw as f32))
            .unwrap_or_else(|| "Waiting...".to_string());

        monitor_frame(ui, "Bandwidth", &bw_label, BW_COLOR, |ui| {
            if let Some(stats) = &self.state.bw_stats {
                ui.horizontal(|ui| {
                    if let (Some(mean), Some(min), Some(max)) =
                        (stats.mean_size_bytes, stats.min_size_bytes, stats.max_size_bytes)
                    {
                        stat_chip(ui, "mean", &format_bytes(mean as f32));
                        ui.add_space(8.0);
                        stat_chip(ui, "min", &format_bytes(min as f32));
                        ui.add_space(8.0);
                        stat_chip(ui, "max", &format_bytes(max as f32));
                    }
                    ui.add_space(8.0);
                    stat_chip(ui, "window", &stats.message_count.to_string());
                });
                ui.add_space(6.0);
            }

            let bw_history: Vec<TimedSample> = self.state.bw_history.iter().copied().collect();
            let peak_bw = bw_history.iter().fold(0.0_f32, |m, s| m.max(s.value));
            let peak_mib = (peak_bw / (1024.0 * 1024.0)).max(1.0 / 1024.0);
            let y_max_mib = nice_linear_max((peak_mib * 1.1).max(1.0 / 1024.0));
            let (y_max, unit, scale) = bw_axis_unit(y_max_mib);
            let scaled: Vec<TimedSample> = bw_history
                .iter()
                .map(|s| TimedSample { time_secs: s.time_secs, value: s.value * scale })
                .collect();

            draw_time_series(ui, now_secs, &scaled, y_max, unit, BW_COLOR);
        });
    }

    fn render_delay_section(&self, ui: &mut egui::Ui, now_secs: f64) {
        let delay_label = self
            .state
            .delay_current
            .map(|d| format!("{d:.2} ms"))
            .unwrap_or_else(|| "No header.stamp".to_string());

        monitor_frame(ui, "Delay", &delay_label, DELAY_COLOR, |ui| {
            if let Some(stats) = &self.state.delay_stats {
                ui.horizontal(|ui| {
                    stat_chip(ui, "avg", &format!("{:.2} ms", stats.avg_ms));
                    ui.add_space(8.0);
                    stat_chip(ui, "min", &format!("{:.2} ms", stats.min_ms));
                    ui.add_space(8.0);
                    stat_chip(ui, "max", &format!("{:.2} ms", stats.max_ms));
                    ui.add_space(8.0);
                    stat_chip(ui, "std dev", &format!("{:.2} ms", stats.std_dev_ms));
                    ui.add_space(8.0);
                    stat_chip(ui, "window", &stats.window.to_string());
                });
                ui.add_space(6.0);
            }

            let delay_history: Vec<TimedSample> =
                self.state.delay_history.iter().copied().collect();
            let peak = delay_history.iter().fold(0.0_f32, |m, s| m.max(s.value));
            let y_max = nice_linear_max((peak * 1.1).max(1.0));

            draw_time_series(ui, now_secs, &delay_history, y_max, "ms", DELAY_COLOR);
        });
    }

    fn render_topic_info_section(&self, ui: &mut egui::Ui) {
        egui::Frame::group(ui.style())
            .fill(egui::Color32::from_rgb(252, 253, 255))
            .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(210, 217, 226)))
            .corner_radius(egui::CornerRadius::same(8))
            .inner_margin(egui::Margin::same(12))
            .show(ui, |ui| {
                egui::CollapsingHeader::new(egui::RichText::new("Topic Info").strong())
                    .default_open(false)
                    .show(ui, |ui| {
                        let Some(info) = &self.topic_info else {
                            ui.label(
                                egui::RichText::new("Loading...")
                                    .color(egui::Color32::from_gray(130)),
                            );
                            return;
                        };

                        ui.add_space(6.0);

                        // Message type(s)
                        ui.horizontal_wrapped(|ui| {
                            ui.label(
                                egui::RichText::new("Type")
                                    .size(11.0)
                                    .color(egui::Color32::from_gray(110)),
                            );
                            ui.add_space(6.0);
                            if info.type_names.is_empty() {
                                ui.label(
                                    egui::RichText::new("—").color(egui::Color32::from_gray(150)),
                                );
                            } else {
                                for type_name in &info.type_names {
                                    egui::Frame::new()
                                        .fill(egui::Color32::from_rgb(232, 241, 255))
                                        .stroke(egui::Stroke::new(
                                            1.0,
                                            egui::Color32::from_rgb(180, 210, 245),
                                        ))
                                        .corner_radius(egui::CornerRadius::same(4))
                                        .inner_margin(egui::Margin::symmetric(6, 2))
                                        .show(ui, |ui| {
                                            ui.label(
                                                egui::RichText::new(type_name)
                                                    .monospace()
                                                    .size(11.0)
                                                    .color(egui::Color32::from_rgb(30, 80, 160)),
                                            );
                                        });
                                }
                            }
                        });

                        ui.add_space(10.0);

                        // Publishers
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("Publishers").strong().size(12.0));
                            ui.label(
                                egui::RichText::new(format!("({})", info.publisher_count))
                                    .size(12.0)
                                    .color(egui::Color32::from_gray(130)),
                            );
                        });
                        ui.add_space(4.0);
                        if info.publishers.is_empty() {
                            ui.label(
                                egui::RichText::new("none").small().color(egui::Color32::from_gray(150)),
                            );
                        } else {
                            for ep in &info.publishers {
                                render_endpoint_verbose(ui, ep);
                                ui.add_space(4.0);
                            }
                        }

                        ui.add_space(8.0);

                        // Subscribers
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("Subscribers").strong().size(12.0));
                            ui.label(
                                egui::RichText::new(format!("({})", info.subscription_count))
                                    .size(12.0)
                                    .color(egui::Color32::from_gray(130)),
                            );
                        });
                        ui.add_space(4.0);
                        if info.subscriptions.is_empty() {
                            ui.label(
                                egui::RichText::new("none").small().color(egui::Color32::from_gray(150)),
                            );
                        } else {
                            for ep in &info.subscriptions {
                                render_endpoint_verbose(ui, ep);
                                ui.add_space(4.0);
                            }
                        }
                    });
            });
    }

    fn render_echo_section(&mut self, ui: &mut egui::Ui) {
        egui::Frame::group(ui.style())
            .fill(egui::Color32::from_rgb(252, 253, 255))
            .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(210, 217, 226)))
            .corner_radius(egui::CornerRadius::same(8))
            .inner_margin(egui::Margin::same(12))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Echo").strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let (btn_text, btn_fill) = if self.echo_enabled {
                            ("Disable", egui::Color32::from_rgb(200, 60, 60))
                        } else {
                            ("Enable", egui::Color32::from_rgb(56, 110, 200))
                        };
                        let btn = egui::Button::new(
                            egui::RichText::new(btn_text).color(egui::Color32::WHITE).size(13.0),
                        )
                        .fill(btn_fill)
                        .corner_radius(egui::CornerRadius::same(4));
                        if ui.add_sized([80.0, 28.0], btn).clicked() {
                            self.echo_enabled = !self.echo_enabled;
                            let _ = self
                                .cmd_tx
                                .send(MonitorCommand::SetEchoEnabled(self.echo_enabled));
                            if !self.echo_enabled {
                                self.echo_messages.clear();
                            }
                        }
                        if self.echo_enabled && !self.echo_messages.is_empty() {
                            ui.add_space(8.0);
                            ui.label(
                                egui::RichText::new(format!("{} msgs", self.echo_messages.len()))
                                    .small()
                                    .color(ECHO_COLOR),
                            );
                        }
                    });
                });

                if self.echo_enabled {
                    ui.add_space(8.0);
                    if self.echo_messages.is_empty() {
                        ui.label(
                            egui::RichText::new("Waiting for messages...")
                                .color(egui::Color32::from_gray(130)),
                        );
                    } else {
                        egui::ScrollArea::vertical()
                            .id_salt("echo_scroll")
                            .max_height(300.0)
                            .stick_to_bottom(true)
                            .show(ui, |ui| {
                                for msg in &self.echo_messages {
                                    egui::Frame::new()
                                        .fill(egui::Color32::from_rgb(240, 240, 250))
                                        .stroke(egui::Stroke::new(
                                            1.0,
                                            egui::Color32::from_rgb(210, 210, 230),
                                        ))
                                        .corner_radius(egui::CornerRadius::same(4))
                                        .inner_margin(egui::Margin::same(6))
                                        .show(ui, |ui| {
                                            ui.label(
                                                egui::RichText::new(msg).monospace().size(11.0),
                                            );
                                        });
                                    ui.add_space(4.0);
                                }
                            });
                    }
                }
            });
    }
}

impl Default for TopicMonitorPage {
    fn default() -> Self {
        Self::new()
    }
}

// ── Echo decoder (Python subprocess) ─────────────────────────────────────────

struct EchoDecoder {
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum HelperRequest {
    Init {
        field: Option<String>,
        truncate_length: usize,
        no_arr: bool,
        no_str: bool,
    },
    Decode {
        type_name: String,
        payload_base64: String,
    },
}

#[derive(Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum HelperResponse {
    Ok { rendered: String },
    Err { error: String },
}

fn echo_helper_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("cli")
        .join("topic")
        .join("echo_helper.py")
}

impl EchoDecoder {
    fn spawn() -> anyhow::Result<Self> {
        use anyhow::Context;
        let mut child = Command::new("python3")
            .arg(echo_helper_path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("spawn echo helper")?;
        let stdin = child.stdin.take().context("take echo helper stdin")?;
        let stdout = child.stdout.take().context("take echo helper stdout")?;
        let mut dec = Self { _child: child, stdin, stdout: BufReader::new(stdout) };
        dec.send_req(&HelperRequest::Init {
            field: None,
            truncate_length: 128,
            no_arr: false,
            no_str: false,
        })?;
        match dec.recv_resp()? {
            HelperResponse::Ok { .. } => Ok(dec),
            HelperResponse::Err { error } => anyhow::bail!(error),
        }
    }

    fn decode(&mut self, type_name: &str, payload_base64: &str) -> anyhow::Result<String> {
        self.send_req(&HelperRequest::Decode {
            type_name: type_name.to_string(),
            payload_base64: payload_base64.to_string(),
        })?;
        match self.recv_resp()? {
            HelperResponse::Ok { rendered } => Ok(rendered),
            HelperResponse::Err { error } => anyhow::bail!(error),
        }
    }

    fn send_req(&mut self, req: &HelperRequest) -> anyhow::Result<()> {
        use anyhow::Context;
        serde_json::to_writer(&mut self.stdin, req).context("serialize echo helper request")?;
        self.stdin.write_all(b"\n").context("write echo helper newline")?;
        self.stdin.flush().context("flush echo helper stdin")
    }

    fn recv_resp(&mut self) -> anyhow::Result<HelperResponse> {
        use anyhow::Context;
        let mut line = String::new();
        self.stdout.read_line(&mut line).context("read echo helper response")?;
        if line.trim().is_empty() {
            anyhow::bail!("echo helper exited unexpectedly");
        }
        serde_json::from_str(line.trim_end()).context("parse echo helper response")
    }
}

// ── Echo worker thread ────────────────────────────────────────────────────────

struct EchoWorkerHandle {
    _thread: thread::JoinHandle<()>,
}

fn start_echo_worker(
    topic: String,
    event_tx: mpsc::Sender<MonitorEvent>,
) -> Option<EchoWorkerHandle> {
    match send_request(CommandRequest::TopicEchoStart(TopicEchoStartRequest {
        topic_name: topic,
    })) {
        Ok(CommandResponse::TopicEchoStart(_)) => {}
        _ => return None,
    }

    let handle = thread::spawn(move || {
        let mut decoder = match EchoDecoder::spawn() {
            Ok(d) => d,
            Err(e) => {
                let _ = event_tx.send(MonitorEvent::Error(format!("Echo decoder: {e}")));
                return;
            }
        };

        loop {
            match send_request(CommandRequest::TopicEchoStatus(TopicEchoStatusRequest)) {
                Ok(CommandResponse::TopicEchoStatus(resp)) => {
                    if !resp.active {
                        break;
                    }
                    let decoded: Vec<String> = resp
                        .messages
                        .iter()
                        .filter_map(|msg| {
                            let type_name = msg.type_name.as_deref()?;
                            decoder.decode(type_name, &msg.payload_base64).ok()
                        })
                        .collect();
                    if !decoded.is_empty() {
                        if event_tx.send(MonitorEvent::EchoMessages(decoded)).is_err() {
                            break;
                        }
                    }
                }
                _ => break,
            }
        }
    });

    Some(EchoWorkerHandle { _thread: handle })
}

fn stop_echo_worker(handle: &mut Option<EchoWorkerHandle>) {
    let _ = send_request(CommandRequest::TopicEchoStop(TopicEchoStopRequest));
    *handle = None;
}

// ── Monitor worker ────────────────────────────────────────────────────────────

fn fetch_topic_info(topic: &str) -> Option<TopicDetails> {
    match send_request(CommandRequest::TopicInfo(TopicInfoRequest {
        topic_name: topic.to_string(),
    })) {
        Ok(CommandResponse::TopicInfo(r)) => r.topic,
        _ => None,
    }
}

fn monitor_worker(
    start_time: Instant,
    cmd_rx: mpsc::Receiver<MonitorCommand>,
    event_tx: mpsc::Sender<MonitorEvent>,
) {
    let poll_interval = Duration::from_millis(500);
    let topic_refresh_interval = Duration::from_secs(2);
    let graph_refresh_interval = Duration::from_secs(3);
    let info_refresh_interval = Duration::from_secs(3);

    let mut active_topic: Option<String> = None;
    let mut echo_enabled = false;
    let mut echo_handle: Option<EchoWorkerHandle> = None;
    let mut last_topic_refresh = Instant::now() - topic_refresh_interval;
    let mut last_graph_refresh = Instant::now() - graph_refresh_interval;
    let mut last_info_refresh = Instant::now() - info_refresh_interval;

    loop {
        match cmd_rx.recv_timeout(poll_interval) {
            Ok(cmd) => {
                apply_command(
                    cmd,
                    &mut active_topic,
                    &mut echo_enabled,
                    &mut echo_handle,
                    &event_tx,
                );
                while let Ok(cmd) = cmd_rx.try_recv() {
                    apply_command(
                        cmd,
                        &mut active_topic,
                        &mut echo_enabled,
                        &mut echo_handle,
                        &event_tx,
                    );
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }

        if last_topic_refresh.elapsed() >= topic_refresh_interval {
            if let Ok(topics) = fetch_topic_list() {
                if event_tx.send(MonitorEvent::TopicsUpdated(topics)).is_err() {
                    return;
                }
            }
            last_topic_refresh = Instant::now();
        }

        if last_graph_refresh.elapsed() >= graph_refresh_interval {
            if let Ok(raw_graph) = fetch_graph_snapshot() {
                let _ = event_tx.send(MonitorEvent::GraphUpdated(raw_graph));
            }
            last_graph_refresh = Instant::now();
        }

        if let Some(ref topic) = active_topic {
            if last_info_refresh.elapsed() >= info_refresh_interval {
                if let Some(details) = fetch_topic_info(topic) {
                    let _ = event_tx.send(MonitorEvent::TopicInfoUpdated(details));
                }
                last_info_refresh = Instant::now();
            }
        }

        if active_topic.is_some() {
            let now_secs = start_time.elapsed().as_secs_f64();

            let hzbw = send_request(CommandRequest::TopicHzBwStatus(TopicHzBwStatusRequest))
                .ok()
                .and_then(|r| match r {
                    CommandResponse::TopicHzBwStatus(s) => Some(s),
                    _ => None,
                });
            let hz_status = hzbw.as_ref().map(|s| &s.hz);
            let bw_status = hzbw.as_ref().map(|s| &s.bw);

            let fresh = hz_status
                .and_then(|s| s.last_message_secs_ago)
                .map_or(false, |secs| secs <= 2.0);

            let hz = if fresh { hz_status.and_then(|s| s.average_rate_hz) } else { None };
            let bw = if fresh { bw_status.and_then(|s| s.bytes_per_second) } else { None };

            let hz_stats = if fresh {
                hz_status.and_then(|s| {
                    Some(HzStats {
                        min_period_ms: s.min_period_seconds? * 1000.0,
                        max_period_ms: s.max_period_seconds? * 1000.0,
                        std_dev_ms: s.std_dev_seconds? * 1000.0,
                        window: s.window,
                    })
                })
            } else {
                None
            };
            let bw_stats = if fresh {
                bw_status.filter(|s| s.active).map(|s| BwStats {
                    message_count: s.message_count,
                    mean_size_bytes: s.mean_size_bytes,
                    min_size_bytes: s.min_size_bytes,
                    max_size_bytes: s.max_size_bytes,
                })
            } else {
                None
            };
            let delay_avg = if fresh {
                hzbw.as_ref().and_then(|s| s.delay.as_ref()).map(|d| d.avg_ms)
            } else {
                None
            };
            let delay_stats = if fresh {
                hzbw.as_ref().and_then(|s| s.delay.as_ref()).map(|d| DelayStats {
                    avg_ms: d.avg_ms,
                    min_ms: d.min_ms,
                    max_ms: d.max_ms,
                    std_dev_ms: d.std_dev_ms,
                    window: d.window,
                })
            } else {
                None
            };

            let snapshot = MonitorSnapshot {
                time_secs: now_secs,
                hz,
                bw_bytes_per_sec: bw,
                delay_avg_ms: delay_avg,
                hz_stats,
                bw_stats,
                delay_stats,
            };

            if event_tx.send(MonitorEvent::Snapshot(snapshot)).is_err() {
                return;
            }
        }
    }
}

fn apply_command(
    cmd: MonitorCommand,
    active_topic: &mut Option<String>,
    echo_enabled: &mut bool,
    echo_handle: &mut Option<EchoWorkerHandle>,
    event_tx: &mpsc::Sender<MonitorEvent>,
) {
    match cmd {
        MonitorCommand::SelectTopic { topic, window_size } => {
            stop_echo_worker(echo_handle);
            start_sessions(topic, window_size, active_topic, event_tx);
            if let Some(ref t) = active_topic.clone() {
                if let Some(details) = fetch_topic_info(t) {
                    let _ = event_tx.send(MonitorEvent::TopicInfoUpdated(details));
                }
            }
            if *echo_enabled {
                if let Some(ref t) = active_topic.clone() {
                    *echo_handle = start_echo_worker(t.clone(), event_tx.clone());
                }
            }
        }
        MonitorCommand::SetWindowSize(window_size) => {
            if let Some(topic) = active_topic.clone() {
                stop_echo_worker(echo_handle);
                stop_sessions();
                start_sessions(topic, window_size, active_topic, event_tx);
                if *echo_enabled {
                    if let Some(ref t) = active_topic.clone() {
                        *echo_handle = start_echo_worker(t.clone(), event_tx.clone());
                    }
                }
            }
        }
        MonitorCommand::Deselect => {
            stop_echo_worker(echo_handle);
            stop_sessions();
            *active_topic = None;
        }
        MonitorCommand::SetEchoEnabled(enabled) => {
            *echo_enabled = enabled;
            if enabled {
                if let Some(ref topic) = active_topic.clone() {
                    *echo_handle = start_echo_worker(topic.clone(), event_tx.clone());
                }
            } else {
                stop_echo_worker(echo_handle);
            }
        }
    }
}

fn stop_sessions() {
    let _ = send_request(CommandRequest::TopicHzStop(TopicHzStopRequest));
    let _ = send_request(CommandRequest::TopicBwStop(TopicBwStopRequest));
    let _ = send_request(CommandRequest::TopicDelayStop(TopicDelayStopRequest));
}

fn start_sessions(
    topic: String,
    window_size: usize,
    active_topic: &mut Option<String>,
    event_tx: &mpsc::Sender<MonitorEvent>,
) {
    stop_sessions();
    let hz_ok = send_request(CommandRequest::TopicHzStart(TopicHzStartRequest {
        topic_name: topic.clone(),
        window_size,
    }))
    .is_ok();
    let bw_ok = send_request(CommandRequest::TopicBwStart(TopicBwStartRequest {
        topic_name: topic.clone(),
        window_size,
    }))
    .is_ok();
    // Best-effort: delay can fail for topics without header.stamp — that's OK.
    let _ = send_request(CommandRequest::TopicDelayStart(TopicDelayStartRequest {
        topic_name: topic.clone(),
        window_size,
    }));

    if hz_ok && bw_ok {
        *active_topic = Some(topic);
    } else {
        let _ = event_tx.send(MonitorEvent::Error(
            "Failed to start monitoring sessions — is the runtime running?".to_string(),
        ));
        *active_topic = None;
    }
}

fn fetch_topic_list() -> anyhow::Result<Vec<String>> {
    match send_request(CommandRequest::TopicList(TopicListRequest {
        show_types: false,
        count_only: false,
        verbose: false,
    }))? {
        CommandResponse::TopicList(response) => {
            let mut names: Vec<String> = response.topics.iter().map(|t| t.name.clone()).collect();
            names.sort();
            Ok(names)
        }
        CommandResponse::Error(err) => anyhow::bail!(err.message),
        _ => anyhow::bail!("unexpected response"),
    }
}

fn monitor_frame(
    ui: &mut egui::Ui,
    title: &str,
    current_value: &str,
    color: egui::Color32,
    add_contents: impl FnOnce(&mut egui::Ui),
) {
    egui::Frame::group(ui.style())
        .fill(egui::Color32::from_rgb(252, 253, 255))
        .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(210, 217, 226)))
        .corner_radius(egui::CornerRadius::same(8))
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(title).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(current_value).color(color).strong().size(16.0),
                    );
                });
            });
            ui.add_space(8.0);
            add_contents(ui);
        });
}

fn stat_chip(ui: &mut egui::Ui, label: &str, value: &str) {
    egui::Frame::new()
        .fill(egui::Color32::from_rgb(240, 244, 250))
        .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(210, 217, 226)))
        .corner_radius(egui::CornerRadius::same(4))
        .inner_margin(egui::Margin::symmetric(6, 3))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(label)
                        .small()
                        .color(egui::Color32::from_gray(120)),
                );
                ui.label(egui::RichText::new(value).small().strong());
            });
        });
}

fn draw_time_series(
    ui: &mut egui::Ui,
    now_secs: f64,
    samples: &[TimedSample],
    y_max: f32,
    unit: &str,
    color: egui::Color32,
) {
    let desired_size = egui::vec2(ui.available_width(), GRAPH_HEIGHT);
    let (rect, _) = ui.allocate_exact_size(desired_size, egui::Sense::hover());
    let painter = ui.painter_at(rect);

    draw_chart_frame(&painter, rect);
    draw_grid_lines(&painter, rect, 5);
    draw_time_guides(ui, &painter, rect);
    draw_axis_labels(ui, &painter, rect, y_max, unit);

    let gr = graph_inner_rect(rect);
    let points = build_series_points(rect, now_secs, samples, y_max);
    draw_series(&painter.with_clip_rect(gr), points, color);
}

fn draw_chart_frame(painter: &egui::Painter, rect: egui::Rect) {
    let gr = graph_inner_rect(rect);
    painter.rect_filled(gr, 4.0, egui::Color32::from_rgb(247, 249, 252));
    painter.rect_stroke(
        gr,
        4.0,
        egui::Stroke::new(1.0, egui::Color32::from_rgb(214, 220, 228)),
        egui::StrokeKind::Inside,
    );
}

fn draw_grid_lines(painter: &egui::Painter, rect: egui::Rect, segments: usize) {
    let gr = graph_inner_rect(rect);
    for step in 1..segments {
        let t = step as f32 / segments as f32;
        let y = gr.bottom() - gr.height() * t;
        painter.line_segment(
            [egui::pos2(gr.left(), y), egui::pos2(gr.right(), y)],
            egui::Stroke::new(1.0, egui::Color32::from_rgb(231, 235, 241)),
        );
    }
}

fn draw_time_guides(ui: &egui::Ui, painter: &egui::Painter, rect: egui::Rect) {
    let gr = graph_inner_rect(rect);
    for age_secs in (0..=60).step_by(10) {
        let x = gr.right() - gr.width() * (age_secs as f32 / 60.0);
        painter.line_segment(
            [egui::pos2(x, gr.top()), egui::pos2(x, gr.bottom())],
            egui::Stroke::new(1.0, egui::Color32::from_rgb(231, 235, 241)),
        );
        painter.text(
            egui::pos2(x, gr.bottom() + 4.0),
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
    let gr = graph_inner_rect(rect);
    let label_x = gr.right() + 6.0;
    for step in 0..=5 {
        let value = y_max * (5 - step) as f32 / 5.0;
        let y = gr.top() + gr.height() * (step as f32 / 5.0);
        let label = match unit {
            "Hz" => format!("{value:.1}"),
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

fn build_series_points(
    rect: egui::Rect,
    now_secs: f64,
    samples: &[TimedSample],
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

    // Find the rightmost sample off the left edge; include it so the painter
    // interpolates the crossing at gr.left() instead of dropping to zero.
    let off_left = samples
        .iter()
        .rposition(|s| age_to_x((now_secs - s.time_secs).max(0.0) as f32) < gr.left());

    let start = off_left.unwrap_or(0);
    let mut points = Vec::new();

    for s in &samples[start..] {
        let raw_age = (now_secs - s.time_secs).max(0.0) as f32;
        points.push(egui::pos2(age_to_x(raw_age), val_to_y(s.value)));
    }

    // Extend last value 1 s past the right edge; clip rect hides it, giving
    // smooth gnome-system-monitor-style continuity at the 0 s mark.
    let last_y = val_to_y(samples.last().unwrap().value);
    points.push(egui::pos2(gr.right() + one_step, last_y));

    points
}

fn graph_inner_rect(rect: egui::Rect) -> egui::Rect {
    egui::Rect::from_min_max(
        egui::pos2(rect.left() + LEFT_PADDING, rect.top() + TOP_PADDING),
        egui::pos2(rect.right() - RIGHT_AXIS_WIDTH, rect.bottom() - BOTTOM_PADDING),
    )
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

fn bw_axis_unit(y_max_mib: f32) -> (f32, &'static str, f32) {
    if y_max_mib < 1.0 {
        (y_max_mib * 1024.0, "KiB/s", 1.0 / 1024.0)
    } else {
        (y_max_mib, "MiB/s", 1.0 / (1024.0 * 1024.0))
    }
}

fn format_bw(bytes_per_sec: f32) -> String {
    if bytes_per_sec < 1024.0 {
        format!("{bytes_per_sec:.0} B/s")
    } else if bytes_per_sec < 1024.0 * 1024.0 {
        format!("{:.1} KiB/s", bytes_per_sec / 1024.0)
    } else {
        format!("{:.2} MiB/s", bytes_per_sec / (1024.0 * 1024.0))
    }
}

fn endpoint_node_label(ep: &crate::command::protocol::TopicEndpointInfo) -> String {
    match (ep.node_namespace.as_deref(), ep.node_name.as_deref()) {
        (Some(ns), Some(name)) => {
            if ns.is_empty() || ns == "/" { format!("/{name}") }
            else { format!("{}/{name}", ns.trim_end_matches('/')) }
        }
        (None, Some(name)) => format!("/{name}"),
        _ => ep.gid.clone(),
    }
}

fn render_endpoint_verbose(ui: &mut egui::Ui, ep: &crate::command::protocol::TopicEndpointInfo) {
    egui::Frame::new()
        .fill(egui::Color32::from_rgb(244, 246, 250))
        .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(218, 224, 232)))
        .corner_radius(egui::CornerRadius::same(5))
        .inner_margin(egui::Margin::symmetric(8, 6))
        .show(ui, |ui| {
            ui.label(
                egui::RichText::new(endpoint_node_label(ep))
                    .monospace()
                    .size(11.5)
                    .color(egui::Color32::from_rgb(25, 45, 90)),
            );
            ui.add_space(4.0);

            let label_color = egui::Color32::from_gray(115);
            let grid_id = format!("ep_qos_{}", ep.gid);
            egui::Grid::new(grid_id)
                .num_columns(2)
                .spacing([16.0, 2.0])
                .show(ui, |ui| {
                    qos_row(ui, "GID", &ep.gid, egui::Color32::from_gray(80), true);

                    if let Some(v) = &ep.reliability {
                        let color = match v.as_str() {
                            "RELIABLE" => egui::Color32::from_rgb(20, 80, 170),
                            "BEST_EFFORT" => egui::Color32::from_rgb(170, 80, 10),
                            _ => label_color,
                        };
                        qos_row(ui, "Reliability", v, color, false);
                    }

                    if let Some(v) = &ep.durability {
                        let color = match v.as_str() {
                            "TRANSIENT_LOCAL" | "TRANSIENT" | "PERSISTENT" =>
                                egui::Color32::from_rgb(20, 120, 50),
                            "VOLATILE" => egui::Color32::from_gray(100),
                            _ => label_color,
                        };
                        qos_row(ui, "Durability", v, color, false);
                    }

                    if let Some(v) = &ep.lifespan {
                        qos_row(ui, "Lifespan", v, egui::Color32::from_gray(80), false);
                    }

                    if let Some(v) = &ep.deadline {
                        qos_row(ui, "Deadline", v, egui::Color32::from_gray(80), false);
                    }

                    if let Some(v) = &ep.liveliness {
                        let color = match v.as_str() {
                            "AUTOMATIC" => egui::Color32::from_gray(100),
                            _ => egui::Color32::from_rgb(90, 50, 160),
                        };
                        qos_row(ui, "Liveliness", v, color, false);
                    }

                    if let Some(v) = &ep.liveliness_lease_duration {
                        qos_row(ui, "Lease duration", v, egui::Color32::from_gray(80), false);
                    }
                });
        });
}

fn qos_row(ui: &mut egui::Ui, label: &str, value: &str, value_color: egui::Color32, monospace: bool) {
    ui.label(
        egui::RichText::new(label).size(11.0).color(egui::Color32::from_gray(115)),
    );
    let text = egui::RichText::new(value).size(11.0).color(value_color);
    let text = if monospace { text.monospace() } else { text };
    ui.label(text);
    ui.end_row();
}

fn format_bytes(bytes: f32) -> String {
    if bytes < 1024.0 {
        format!("{bytes:.0} B")
    } else if bytes < 1024.0 * 1024.0 {
        format!("{:.1} KiB", bytes / 1024.0)
    } else {
        format!("{:.2} MiB", bytes / (1024.0 * 1024.0))
    }
}
