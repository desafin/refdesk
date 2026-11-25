//! 컴파일 검증 전용: eframe 멀티 뷰포트 + egui_dock(serde) 조합이 이 머신에서 빌드되는지 확인. 실행하지 않는다.
use eframe::egui;
use egui_dock::{DockArea, DockState, Style, TabViewer};

struct Viewer;
impl TabViewer for Viewer {
    type Tab = String;
    fn id(&mut self, tab: &mut String) -> egui::Id {
        egui::Id::unique(tab)
    }
    fn title(&mut self, tab: &mut String) -> egui::WidgetText {
        (&*tab).into()
    }
    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut String) {
        ui.label(tab.as_str());
    }
}

struct App {
    main: DockState<String>,
    detached: Vec<DockState<String>>,
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        DockArea::new(&mut self.main).style(Style::from_egui(ui.style().as_ref())).show_inside(ui, &mut Viewer);
        for (i, dock) in self.detached.iter_mut().enumerate() {
            let id = egui::ViewportId::from_hash_of(("detached", i));
            let builder = egui::ViewportBuilder::default().with_title(format!("창 {i}"));
            ui.ctx().show_viewport_immediate(id, builder, |ui, _class| {
                DockArea::new(dock).show_inside(ui, &mut Viewer);
            });
        }
    }
}

fn main() -> eframe::Result {
    let app = App { main: DockState::new(vec!["a.pdf".into(), "b.pdf".into()]), detached: vec![DockState::new(vec!["c.pdf".into()])] };
    // 세션 저장용: DockState 가 serde 로 직렬화되는지 타입 수준에서 확인
    fn assert_serde<T: serde::Serialize + serde::de::DeserializeOwned>(_: &T) {}
    assert_serde(&app.main);
    eframe::run_native("uicheck", eframe::NativeOptions::default(), Box::new(|_cc| Ok(Box::new(app))))
}
