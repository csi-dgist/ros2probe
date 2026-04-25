use eframe::egui;

use super::bag_recorder::BagRecorderPage;
use super::dashboard::DashboardPage;
use super::ros_graph::GraphFilter;
use super::topic_monitor::TopicMonitorPage;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum Page {
    #[default]
    Dashboard,
    TopicMonitor,
    BagRecorder,
}

pub struct Ros2ProbeGuiApp {
    current_page: Page,
    dashboard: DashboardPage,
    topic_monitor: TopicMonitorPage,
    bag_recorder: BagRecorderPage,
    graph_filter: GraphFilter,
}

impl Ros2ProbeGuiApp {
    pub fn new() -> Self {
        Self {
            current_page: Page::Dashboard,
            dashboard: DashboardPage::new(),
            topic_monitor: TopicMonitorPage::new(),
            bag_recorder: BagRecorderPage::new(),
            graph_filter: GraphFilter::default(),
        }
    }

    fn render_sidebar(&mut self, ctx: &egui::Context) {
        const SIDEBAR_BG: egui::Color32 = egui::Color32::from_rgb(22, 28, 38);
        const SELECTED_FILL: egui::Color32 = egui::Color32::from_rgb(56, 110, 200);
        const SELECTED_TEXT: egui::Color32 = egui::Color32::WHITE;
        const IDLE_TEXT: egui::Color32 = egui::Color32::from_rgb(170, 182, 200);

        egui::SidePanel::left("app_sidebar")
            .resizable(false)
            .exact_width(168.0)
            .frame(egui::Frame::new().fill(SIDEBAR_BG).inner_margin(egui::Margin::same(10)))
            .show(ctx, |ui| {
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new("ros2probe")
                        .strong()
                        .color(egui::Color32::WHITE)
                        .size(16.0),
                );
                ui.label(
                    egui::RichText::new("DDS · Monitoring")
                        .small()
                        .color(egui::Color32::from_rgb(120, 135, 155)),
                );
                ui.add_space(16.0);

                for (page, label) in [
                    (Page::Dashboard, "Dashboard"),
                    (Page::TopicMonitor, "Topic Monitor"),
                    (Page::BagRecorder, "Bag Recorder"),
                ] {
                    let selected = self.current_page == page;
                    let text = egui::RichText::new(label)
                        .color(if selected { SELECTED_TEXT } else { IDLE_TEXT });
                    let btn = egui::Button::new(text)
                        .fill(if selected { SELECTED_FILL } else { egui::Color32::TRANSPARENT })
                        .stroke(egui::Stroke::NONE)
                        .corner_radius(egui::CornerRadius::same(6));
                    if ui.add_sized([ui.available_width(), 34.0], btn).clicked() {
                        // Pause Topic Monitor's background polling + server-side
                        // sessions when the user navigates away, so the runtime
                        // isn't busy serving 500ms hz/bw status polls for a page
                        // that isn't being shown.
                        if self.current_page == Page::TopicMonitor && page != Page::TopicMonitor {
                            self.topic_monitor.pause_for_leave();
                        }
                        self.current_page = page;
                    }
                    ui.add_space(2.0);
                }
            });
    }

    fn render_current_page(&mut self, ctx: &egui::Context) {
        match self.current_page {
            Page::Dashboard => self.dashboard.show(ctx, &mut self.graph_filter),
            Page::TopicMonitor => self.topic_monitor.show(ctx, &mut self.graph_filter),
            Page::BagRecorder => self.bag_recorder.show(ctx),
        }
    }
}

impl Default for Ros2ProbeGuiApp {
    fn default() -> Self {
        Self::new()
    }
}

impl eframe::App for Ros2ProbeGuiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.set_visuals(egui::Visuals::light());
        self.render_sidebar(ctx);
        self.render_current_page(ctx);
        // Child pages request their own repaint cadence based on pending data.
    }
}
