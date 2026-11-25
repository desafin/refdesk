//! refdesk application entry point: an empty window for now.

use eframe::egui;

/// Application state. Empty until later work packages add the dock and documents.
struct App;

impl eframe::App for App {
    fn ui(&mut self, _ui: &mut egui::Ui, _frame: &mut eframe::Frame) {}
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_title("refdesk"),
        ..Default::default()
    };
    eframe::run_native("refdesk", options, Box::new(|_cc| Ok(Box::new(App))))
}
