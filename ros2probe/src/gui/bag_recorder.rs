use std::{
    collections::HashSet,
    path::PathBuf,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use eframe::egui;

use crate::{
    client::send_request,
    command::protocol::{
        BagRecordRequest, BagSessionInfo, BagSetPausedRequest, BagStatusRequest, BagStopRequest,
        CommandRequest, CommandResponse, CompressionFormat,
    },
};

use super::dashboard::fetch_graph_snapshot;
use super::ros_graph::is_recordable_topic;

const PAGE_BG: egui::Color32 = egui::Color32::from_rgb(230, 232, 236);
const REC_RED: egui::Color32 = egui::Color32::from_rgb(210, 45, 45);
const PAUSE_AMBER: egui::Color32 = egui::Color32::from_rgb(190, 130, 20);

pub struct BagRecorderPage {
    cmd_tx: mpsc::Sender<RecorderCommand>,
    event_rx: mpsc::Receiver<RecorderEvent>,

    // Config (editable when idle)
    output_path: String,
    file_dialog_rx: Option<mpsc::Receiver<Option<PathBuf>>>,
    all_topics: bool,
    topic_filter: String,
    available_topics: Vec<String>,
    internal_topics: Vec<String>,
    internal_topics_open: bool,
    selected_topics: HashSet<String>,
    compression: CompressionFormat,
    no_discovery: bool,
    start_paused_opt: bool,

    // Live state
    session: Option<BagSessionInfo>,
    recording_since: Option<Instant>,
    error: Option<String>,
}

enum RecorderCommand {
    Start(BagRecordRequest),
    Stop,
    SetPaused(bool),
}

enum RecorderEvent {
    Status(Option<BagSessionInfo>),
    TopicsUpdated {
        recordable: Vec<String>,
        internal: Vec<String>,
    },
    Error(String),
}

impl BagRecorderPage {
    pub fn new() -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        thread::spawn(move || recorder_worker(cmd_rx, event_tx));

        Self {
            cmd_tx,
            event_rx,
            output_path: String::new(),
            file_dialog_rx: None,
            all_topics: false,
            topic_filter: String::new(),
            available_topics: Vec::new(),
            internal_topics: Vec::new(),
            internal_topics_open: false,
            selected_topics: HashSet::new(),
            compression: CompressionFormat::None,
            no_discovery: false,
            start_paused_opt: false,
            session: None,
            recording_since: None,
            error: None,
        }
    }

    pub fn show(&mut self, ctx: &egui::Context) {
        self.drain_events();
        self.render_top_bar(ctx);
        if self.session.is_none() {
            self.render_topic_panel(ctx);
            self.render_config_panel(ctx);
        } else {
            self.render_recording_view(ctx);
        }
        ctx.request_repaint_after(Duration::from_millis(500));
    }

    fn drain_events(&mut self) {
        // Poll file dialog result
        if let Some(rx) = &self.file_dialog_rx {
            if let Ok(result) = rx.try_recv() {
                if let Some(path) = result {
                    self.output_path = path.to_string_lossy().into_owned();
                }
                self.file_dialog_rx = None;
            }
        }

        while let Ok(event) = self.event_rx.try_recv() {
            match event {
                RecorderEvent::Status(session) => {
                    if session.is_some() && self.session.is_none() {
                        self.recording_since = Some(Instant::now());
                    } else if session.is_none() {
                        self.recording_since = None;
                    }
                    self.session = session;
                    self.error = None;
                }
                RecorderEvent::TopicsUpdated { recordable, internal } => {
                    self.available_topics = recordable;
                    self.internal_topics = internal;
                }
                RecorderEvent::Error(e) => {
                    self.error = Some(e);
                }
            }
        }
    }

    fn render_top_bar(&self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("bag_header")
            .exact_height(52.0)
            .frame(
                egui::Frame::new()
                    .fill(PAGE_BG)
                    .inner_margin(egui::Margin::symmetric(12, 8)),
            )
            .show(ctx, |ui| {
                ui.heading("Bag Recorder");
            });
    }

    fn render_topic_panel(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("bag_topic_panel")
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

                let filter = self.topic_filter.to_lowercase();
                let filtered_recordable: Vec<&String> = self
                    .available_topics
                    .iter()
                    .filter(|t| filter.is_empty() || t.to_lowercase().contains(&filter))
                    .collect();
                let filtered_internal: Vec<&String> = self
                    .internal_topics
                    .iter()
                    .filter(|t| filter.is_empty() || t.to_lowercase().contains(&filter))
                    .collect();

                if self.available_topics.is_empty() && self.internal_topics.is_empty() {
                    ui.label(
                        egui::RichText::new("No topics found")
                            .color(egui::Color32::from_gray(140)),
                    );
                    return;
                }

                // "All topics" checkbox — checks/unchecks recordable topics
                let all_filtered_selected = !filtered_recordable.is_empty()
                    && (self.all_topics
                        || filtered_recordable.iter().all(|t| self.selected_topics.contains(*t)));
                let mut all_checked = all_filtered_selected;
                let all_resp = ui.checkbox(
                    &mut all_checked,
                    egui::RichText::new("All topics").strong().size(12.0),
                );
                if all_resp.changed() {
                    self.all_topics = all_checked;
                    if all_checked {
                        for t in &self.available_topics {
                            self.selected_topics.insert(t.clone());
                        }
                    } else {
                        self.selected_topics.clear();
                    }
                }

                ui.add_space(2.0);
                ui.separator();
                ui.add_space(2.0);

                egui::ScrollArea::vertical().show(ui, |ui| {
                    for topic in &filtered_recordable {
                        let mut checked =
                            self.all_topics || self.selected_topics.contains(*topic);
                        let resp = ui.checkbox(
                            &mut checked,
                            egui::RichText::new(*topic).monospace().size(11.0),
                        );
                        if resp.changed() {
                            if checked {
                                self.selected_topics.insert((*topic).clone());
                            } else {
                                self.selected_topics.remove(*topic);
                                self.all_topics = false;
                            }
                        }
                    }

                    if !filtered_internal.is_empty() {
                        ui.add_space(4.0);
                        let header = egui::RichText::new(format!(
                            "Internal Topics ({})",
                            filtered_internal.len()
                        ))
                        .size(11.0)
                        .color(egui::Color32::from_gray(120));
                        egui::CollapsingHeader::new(header)
                            .id_salt("internal_topics_recorder")
                            .default_open(false)
                            .open(Some(self.internal_topics_open))
                            .show(ui, |ui| {
                                for topic in &filtered_internal {
                                    let mut checked = self.selected_topics.contains(*topic);
                                    let resp = ui.checkbox(
                                        &mut checked,
                                        egui::RichText::new(*topic)
                                            .monospace()
                                            .size(11.0)
                                            .color(egui::Color32::from_gray(140)),
                                    );
                                    if resp.changed() {
                                        if checked {
                                            self.selected_topics.insert((*topic).clone());
                                            self.all_topics = false;
                                        } else {
                                            self.selected_topics.remove(*topic);
                                        }
                                    }
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

    fn render_config_panel(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(PAGE_BG).inner_margin(egui::Margin::same(16)))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    // Output path
                    config_card(ui, "Output Path", |ui| {
                        ui.horizontal(|ui| {
                            egui::TextEdit::singleline(&mut self.output_path)
                                .hint_text("Leave blank for auto-generated name")
                                .desired_width(ui.available_width() - 36.0)
                                .show(ui);
                            let folder_btn = egui::Button::new("📂")
                                .corner_radius(egui::CornerRadius::same(4));
                            if ui.add_sized([28.0, 22.0], folder_btn).on_hover_text("Browse…").clicked() {
                                self.open_file_dialog();
                            }
                        });
                        ui.add_space(2.0);
                        ui.label(
                            egui::RichText::new("MCAP file path. Leave blank to auto-generate.")
                                .small()
                                .color(egui::Color32::from_gray(120)),
                        );
                    });

                    ui.add_space(12.0);

                    // Compression
                    config_card(ui, "Compression", |ui| {
                        ui.horizontal(|ui| {
                            for (label, variant) in [
                                ("None", CompressionFormat::None),
                                ("Zstd", CompressionFormat::Zstd),
                                ("Lz4", CompressionFormat::Lz4),
                            ] {
                                let selected = self.compression == variant;
                                let btn = egui::Button::new(
                                    egui::RichText::new(label).size(12.0).color(if selected {
                                        egui::Color32::WHITE
                                    } else {
                                        egui::Color32::from_gray(60)
                                    }),
                                )
                                .fill(if selected {
                                    egui::Color32::from_rgb(56, 110, 200)
                                } else {
                                    egui::Color32::from_gray(218)
                                })
                                .corner_radius(egui::CornerRadius::same(4));
                                if ui.add_sized([64.0, 26.0], btn).clicked() {
                                    self.compression = variant;
                                }
                                ui.add_space(2.0);
                            }
                        });
                    });

                    ui.add_space(12.0);

                    // Options
                    config_card(ui, "Options", |ui| {
                        ui.checkbox(&mut self.no_discovery, "No auto-discovery")
                            .on_hover_text("Only record topics present at recording start");
                        ui.add_space(4.0);
                        ui.checkbox(&mut self.start_paused_opt, "Start paused");
                    });

                    ui.add_space(20.0);

                    if let Some(err) = &self.error.clone() {
                        ui.colored_label(egui::Color32::from_rgb(200, 50, 50), err);
                        ui.add_space(8.0);
                    }

                    let can_start = self.all_topics || !self.selected_topics.is_empty();
                    let btn = egui::Button::new(
                        egui::RichText::new("Start Recording")
                            .color(egui::Color32::WHITE)
                            .size(14.0),
                    )
                    .fill(if can_start { REC_RED } else { egui::Color32::from_gray(180) })
                    .corner_radius(egui::CornerRadius::same(6));

                    if ui.add_sized([180.0, 38.0], btn).clicked() && can_start {
                        let topics: Vec<String> = if self.all_topics {
                            Vec::new()
                        } else {
                            let mut v: Vec<String> =
                                self.selected_topics.iter().cloned().collect();
                            v.sort();
                            v
                        };
                        let req = BagRecordRequest {
                            all: self.all_topics,
                            topics,
                            output: if self.output_path.trim().is_empty() {
                                None
                            } else {
                                Some(self.output_path.trim().to_string())
                            },
                            compression_format: Some(self.compression),
                            no_discovery: self.no_discovery,
                            start_paused: self.start_paused_opt,
                        };
                        let _ = self.cmd_tx.send(RecorderCommand::Start(req));
                    }
                });
            });
    }

    fn open_file_dialog(&mut self) {
        let (tx, rx) = mpsc::channel();
        self.file_dialog_rx = Some(rx);
        thread::spawn(move || {
            let path = rfd::FileDialog::new()
                .set_title("Save bag file")
                .add_filter("MCAP bag", &["mcap"])
                .save_file();
            let _ = tx.send(path);
        });
    }

    fn render_recording_view(&mut self, ctx: &egui::Context) {
        let Some(session) = self.session.clone() else { return };

        let elapsed = self.recording_since.map(|t| t.elapsed()).unwrap_or_default();
        let elapsed_str = format!(
            "{:02}:{:02}:{:02}",
            elapsed.as_secs() / 3600,
            (elapsed.as_secs() % 3600) / 60,
            elapsed.as_secs() % 60,
        );

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(PAGE_BG).inner_margin(egui::Margin::same(16)))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    // Status banner
                    egui::Frame::new()
                        .fill(egui::Color32::from_rgb(252, 253, 255))
                        .stroke(egui::Stroke::new(
                            1.5,
                            if session.paused { PAUSE_AMBER } else { REC_RED },
                        ))
                        .corner_radius(egui::CornerRadius::same(8))
                        .inner_margin(egui::Margin::same(14))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                let (dot_color, status_text) = if session.paused {
                                    (PAUSE_AMBER, "PAUSED")
                                } else {
                                    (REC_RED, "RECORDING")
                                };
                                egui::Frame::new()
                                    .fill(dot_color)
                                    .corner_radius(egui::CornerRadius::same(99))
                                    .inner_margin(egui::Margin::symmetric(10, 4))
                                    .show(ui, |ui| {
                                        ui.label(
                                            egui::RichText::new(status_text)
                                                .strong()
                                                .size(12.0)
                                                .color(egui::Color32::WHITE),
                                        );
                                    });
                                ui.add_space(10.0);
                                ui.label(
                                    egui::RichText::new(&elapsed_str)
                                        .monospace()
                                        .size(20.0)
                                        .color(egui::Color32::from_gray(40)),
                                );

                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        let stop_btn = egui::Button::new(
                                            egui::RichText::new("Stop")
                                                .color(egui::Color32::WHITE)
                                                .size(13.0),
                                        )
                                        .fill(egui::Color32::from_rgb(200, 45, 45))
                                        .corner_radius(egui::CornerRadius::same(5));
                                        if ui.add_sized([72.0, 30.0], stop_btn).clicked() {
                                            let _ = self.cmd_tx.send(RecorderCommand::Stop);
                                        }

                                        ui.add_space(6.0);

                                        let (pause_label, pause_color) = if session.paused {
                                            ("Resume", egui::Color32::from_rgb(30, 130, 60))
                                        } else {
                                            ("Pause", egui::Color32::from_rgb(56, 110, 200))
                                        };
                                        let pause_btn = egui::Button::new(
                                            egui::RichText::new(pause_label)
                                                .color(egui::Color32::WHITE)
                                                .size(13.0),
                                        )
                                        .fill(pause_color)
                                        .corner_radius(egui::CornerRadius::same(5));
                                        if ui.add_sized([80.0, 30.0], pause_btn).clicked() {
                                            let _ = self.cmd_tx.send(RecorderCommand::SetPaused(
                                                !session.paused,
                                            ));
                                        }
                                    },
                                );
                            });

                            ui.add_space(8.0);
                            ui.label(
                                egui::RichText::new(&session.output)
                                    .monospace()
                                    .size(11.5)
                                    .color(egui::Color32::from_gray(65)),
                            );
                        });

                    ui.add_space(12.0);

                    // Session details
                    egui::Frame::new()
                        .fill(egui::Color32::from_rgb(252, 253, 255))
                        .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(210, 217, 226)))
                        .corner_radius(egui::CornerRadius::same(8))
                        .inner_margin(egui::Margin::same(14))
                        .show(ui, |ui| {
                            egui::Grid::new("session_details")
                                .num_columns(2)
                                .spacing([20.0, 4.0])
                                .show(ui, |ui| {
                                    let topics_str = if session.all_topics {
                                        "All topics".to_string()
                                    } else {
                                        format!("{} selected", session.topics.len())
                                    };
                                    detail_row(ui, "Topics", &topics_str);
                                    detail_row(
                                        ui,
                                        "Compression",
                                        compression_label(session.compression_format),
                                    );
                                    detail_row(
                                        ui,
                                        "Auto-discovery",
                                        if session.no_discovery { "Off" } else { "On" },
                                    );
                                });

                            if !session.all_topics && !session.topics.is_empty() {
                                ui.add_space(10.0);
                                ui.label(
                                    egui::RichText::new("Recorded topics").strong().size(12.0),
                                );
                                ui.add_space(4.0);
                                ui.horizontal_wrapped(|ui| {
                                    for topic in &session.topics {
                                        egui::Frame::new()
                                            .fill(egui::Color32::from_rgb(235, 240, 250))
                                            .stroke(egui::Stroke::new(
                                                1.0,
                                                egui::Color32::from_rgb(200, 215, 240),
                                            ))
                                            .corner_radius(egui::CornerRadius::same(4))
                                            .inner_margin(egui::Margin::symmetric(6, 2))
                                            .show(ui, |ui| {
                                                ui.label(
                                                    egui::RichText::new(topic)
                                                        .monospace()
                                                        .size(11.0),
                                                );
                                            });
                                        ui.add_space(2.0);
                                    }
                                });
                            }
                        });
                });
            });
    }
}

impl Default for BagRecorderPage {
    fn default() -> Self {
        Self::new()
    }
}

fn compression_label(fmt: CompressionFormat) -> &'static str {
    match fmt {
        CompressionFormat::None => "None",
        CompressionFormat::Zstd => "Zstd",
        CompressionFormat::Lz4 => "Lz4",
    }
}

fn config_card(ui: &mut egui::Ui, title: &str, add_contents: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .fill(egui::Color32::from_rgb(252, 253, 255))
        .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(210, 217, 226)))
        .corner_radius(egui::CornerRadius::same(8))
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(title).strong());
            ui.add_space(6.0);
            add_contents(ui);
        });
}

fn detail_row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.label(egui::RichText::new(label).size(12.0).color(egui::Color32::from_gray(115)));
    ui.label(egui::RichText::new(value).size(12.0));
    ui.end_row();
}

fn recorder_worker(
    cmd_rx: mpsc::Receiver<RecorderCommand>,
    event_tx: mpsc::Sender<RecorderEvent>,
) {
    let poll_interval = Duration::from_millis(1000);
    let topic_refresh_interval = Duration::from_secs(3);
    let mut last_topic_refresh = Instant::now() - topic_refresh_interval;

    loop {
        match cmd_rx.recv_timeout(poll_interval) {
            Ok(cmd) => match cmd {
                RecorderCommand::Start(req) => {
                    match send_request(CommandRequest::BagRecord(req)) {
                        Ok(CommandResponse::BagRecord(resp)) => {
                            let session = BagSessionInfo {
                                output: resp.output,
                                paused: resp.paused,
                                topics: resp.topics,
                                all_topics: resp.all_topics,
                                no_discovery: resp.no_discovery,
                                compression_format: resp.compression_format,
                            };
                            let _ = event_tx.send(RecorderEvent::Status(Some(session)));
                        }
                        Ok(CommandResponse::Error(e)) => {
                            let _ = event_tx.send(RecorderEvent::Error(e.message));
                        }
                        Err(e) => {
                            let _ = event_tx.send(RecorderEvent::Error(e.to_string()));
                        }
                        _ => {}
                    }
                }
                RecorderCommand::Stop => {
                    match send_request(CommandRequest::BagStop(BagStopRequest)) {
                        Ok(CommandResponse::BagStop(_)) => {
                            let _ = event_tx.send(RecorderEvent::Status(None));
                        }
                        Err(e) => {
                            let _ = event_tx.send(RecorderEvent::Error(e.to_string()));
                        }
                        _ => {}
                    }
                }
                RecorderCommand::SetPaused(paused) => {
                    let _ = send_request(CommandRequest::BagSetPaused(BagSetPausedRequest {
                        paused,
                    }));
                }
            },
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }

        // Poll status every cycle
        if let Ok(CommandResponse::BagStatus(resp)) =
            send_request(CommandRequest::BagStatus(BagStatusRequest))
        {
            if event_tx.send(RecorderEvent::Status(resp.session)).is_err() {
                return;
            }
        }

        // Refresh topic list
        if last_topic_refresh.elapsed() >= topic_refresh_interval {
            if let Ok(graph) = fetch_graph_snapshot() {
                let all_names: Vec<String> = {
                    let mut names: Vec<String> = graph
                        .topics
                        .iter()
                        .map(|t| t.name.clone())
                        .collect();
                    names.sort();
                    names
                };
                let mut recordable: Vec<String> = Vec::new();
                let mut internal: Vec<String> = Vec::new();
                for name in &all_names {
                    if is_recordable_topic(name, &graph) {
                        recordable.push(name.clone());
                    } else {
                        internal.push(name.clone());
                    }
                }
                if event_tx
                    .send(RecorderEvent::TopicsUpdated { recordable, internal })
                    .is_err()
                {
                    return;
                }
            }
            last_topic_refresh = Instant::now();
        }
    }
}
