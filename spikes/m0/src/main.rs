//! WP-01 M0 verification app (throwaway).
//!
//! Lets the user check, by hand, the things that can only be judged with real windows on
//! GNOME/Wayland: multi-window docking, tab drag between windows, closing windows, Korean
//! text and IME, the portal file picker, image clipboard and texture sharing across viewports.
//! See README.md for the checklist.

use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui;
use egui::load::SizedTexture;
use egui::{Color32, ColorImage, Pos2, TextureHandle, TextureOptions, ViewportId};
use egui_dock::{DockArea, DockState, NodePath, Style, TabViewer, Translations};

/// Render scale (pixels per PDF point).
const RENDER_SCALE: f32 = 1.5;
/// How long after a drop the per-frame pointer samples are recorded.
const PROBE_WINDOW_MS: u128 = 500;
const MAIN_WIN: u64 = 0;

type DocId = usize;

// ---------------------------------------------------------------------------
// Messages between worker threads and the UI thread

enum Msg {
    Opened {
        doc: DocId,
        page_count: usize,
    },
    OpenFailed {
        doc: DocId,
        err: String,
    },
    Rendered {
        doc: DocId,
        page: usize,
        width: usize,
        height: usize,
        rgb: Vec<u8>,
        ms: u128,
    },
    RenderFailed {
        doc: DocId,
        page: usize,
        err: String,
    },
    FilePicked {
        win: u64,
        path: Option<PathBuf>,
        ms: u128,
    },
    Status(String),
}

/// One worker thread per document; it owns the `mupdf::Document` (which is not `Send`).
fn spawn_doc_worker(
    doc: DocId,
    path: PathBuf,
    tx: Sender<Msg>,
    ctx: egui::Context,
) -> Sender<usize> {
    let (req_tx, req_rx) = channel::<usize>();
    let spawned = thread::Builder::new()
        .name(format!("doc-{doc}"))
        .spawn(move || {
            let send = |m: Msg| {
                let _ = tx.send(m);
                ctx.request_repaint();
            };
            let document = match mupdf::Document::open(path.as_path()) {
                Ok(d) => d,
                Err(e) => {
                    return send(Msg::OpenFailed {
                        doc,
                        err: e.to_string(),
                    });
                }
            };
            match document.page_count() {
                Ok(n) => send(Msg::Opened {
                    doc,
                    page_count: n.max(0) as usize,
                }),
                Err(e) => {
                    return send(Msg::OpenFailed {
                        doc,
                        err: e.to_string(),
                    });
                }
            }
            while let Ok(page) = req_rx.recv() {
                let t = Instant::now();
                match render_page(&document, page) {
                    Ok((width, height, rgb)) => send(Msg::Rendered {
                        doc,
                        page,
                        width,
                        height,
                        rgb,
                        ms: t.elapsed().as_millis(),
                    }),
                    Err(err) => send(Msg::RenderFailed { doc, page, err }),
                }
            }
        });
    if let Err(e) = spawned {
        println!("failed to spawn worker thread: {e}");
    }
    req_tx
}

fn render_page(document: &mupdf::Document, page: usize) -> Result<(usize, usize, Vec<u8>), String> {
    let page = document.load_page(page as i32).map_err(|e| e.to_string())?;
    let pm = page
        .to_pixmap(
            &mupdf::Matrix::new_scale(RENDER_SCALE, RENDER_SCALE),
            &mupdf::Colorspace::device_rgb(),
            false,
            true,
        )
        .map_err(|e| e.to_string())?;
    let (w, h, n) = (pm.width() as usize, pm.height() as usize, pm.n() as usize);
    if n != 3 {
        return Err(format!("unexpected pixmap components n={n}"));
    }
    let stride = pm.stride() as usize;
    let samples = pm.samples();
    let mut rgb = Vec::with_capacity(w * h * 3);
    for y in 0..h {
        rgb.extend_from_slice(&samples[y * stride..y * stride + w * 3]);
    }
    Ok((w, h, rgb))
}

// ---------------------------------------------------------------------------
// State

struct DocEntry {
    name: String,
    page_count: Option<usize>,
    error: Option<String>,
    req: Sender<usize>,
    requested: HashSet<usize>,
}

struct RenderedPage {
    width: usize,
    height: usize,
    rgb: Arc<Vec<u8>>,
    /// Created once, in the main window's frame; every window draws this same handle.
    tex: TextureHandle,
}

#[derive(Clone)]
struct Tab {
    key: u64,
    doc: DocId,
    page: usize,
}

struct DragTrack {
    tab_key: u64,
    frames: u32,
    outside_frames: u32,
    first_outside: Option<Pos2>,
    last_outside: Option<Pos2>,
    pointer_lost_frames: u32,
    last_pos: Option<Pos2>,
}

struct Win {
    id: u64,
    dock: DockState<Tab>,
    close_requested: bool,
    drag: Option<DragTrack>,
}

/// Evidence for one "drop outside the source window" event.
struct DropProbe {
    t0: Instant,
    source: u64,
    tab_key: u64,
    threshold_ms: u64,
    decided: Option<String>,
    /// window id -> whether it ever had the pointer during the probe
    seen: HashMap<u64, u32>,
}

enum Action {
    MoveTab { key: u64, to: u64, why: String },
    DetachTab { key: u64, why: String },
    NewWindow,
    OpenFile { win: u64, path: PathBuf },
}

struct Shared {
    ctx: egui::Context,
    start: Instant,
    docs: Vec<DocEntry>,
    pages: HashMap<(DocId, usize), RenderedPage>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    log: Vec<String>,
    actions: Vec<Action>,
    threshold_ms: u64,
    ime_text: String,
    probe: Option<DropProbe>,
    pass_samples: Vec<String>,
    next_tab_key: u64,
    picker_open: bool,
    window_ids: Vec<u64>,
    font_mode: String,
}

impl Shared {
    fn log(&mut self, s: impl Into<String>) {
        let line = format!(
            "[{:>8.3}s] {}",
            self.start.elapsed().as_secs_f64(),
            s.into()
        );
        println!("{line}");
        self.log.push(line);
        if self.log.len() > 2000 {
            self.log.drain(..500);
        }
    }

    fn open_doc(&mut self, path: PathBuf) -> DocId {
        let doc = self.docs.len();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "?".into());
        let req = spawn_doc_worker(doc, path.clone(), self.tx.clone(), self.ctx.clone());
        self.docs.push(DocEntry {
            name,
            page_count: None,
            error: None,
            req,
            requested: HashSet::new(),
        });
        self.log(format!(
            "문서 #{doc} 열기 요청 (워커 스레드): {}",
            path.display()
        ));
        doc
    }

    fn new_tab(&mut self, doc: DocId, page: usize) -> Tab {
        self.next_tab_key += 1;
        Tab {
            key: self.next_tab_key,
            doc,
            page,
        }
    }

    fn request(&mut self, doc: DocId, page: usize) {
        let Some(d) = self.docs.get_mut(doc) else {
            return;
        };
        if d.requested.insert(page) {
            let _ = d.req.send(page);
        }
    }

    /// Runs inside the main window's frame (ROOT viewport), so textures are created there.
    fn drain_messages(&mut self, ctx: &egui::Context) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Opened { doc, page_count } => {
                    self.docs[doc].page_count = Some(page_count);
                    let name = self.docs[doc].name.clone();
                    self.log(format!("문서 #{doc} 열림: {name}, {page_count}쪽"));
                }
                Msg::OpenFailed { doc, err } => {
                    self.docs[doc].error = Some(err.clone());
                    self.log(format!("문서 #{doc} 열기 실패: {err}"));
                }
                Msg::Rendered {
                    doc,
                    page,
                    width,
                    height,
                    rgb,
                    ms,
                } => {
                    let image = ColorImage::from_rgb([width, height], &rgb);
                    let tex = ctx.load_texture(
                        format!("doc{doc}-p{page}"),
                        image,
                        TextureOptions::LINEAR,
                    );
                    self.log(format!(
                        "렌더링 완료 문서 #{doc} {}쪽 {width}x{height}px ({ms}ms) → 텍스처 {:?} 생성 (뷰포트 {:?})",
                        page + 1,
                        tex.id(),
                        ctx.viewport_id()
                    ));
                    self.pages.insert(
                        (doc, page),
                        RenderedPage {
                            width,
                            height,
                            rgb: Arc::new(rgb),
                            tex,
                        },
                    );
                }
                Msg::RenderFailed { doc, page, err } => {
                    self.log(format!("렌더링 실패 문서 #{doc} {}쪽: {err}", page + 1));
                }
                Msg::FilePicked { win, path, ms } => {
                    self.picker_open = false;
                    match path {
                        Some(p) => {
                            self.log(format!(
                                "파일 선택기: {} ({ms}ms 동안 열려 있었음)",
                                p.display()
                            ));
                            self.actions.push(Action::OpenFile { win, path: p });
                        }
                        None => self.log(format!("파일 선택기: 취소 ({ms}ms)")),
                    }
                }
                Msg::Status(s) => self.log(s),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tab viewer

struct Viewer<'a> {
    win_id: u64,
    sh: &'a mut Shared,
    /// Tab whose button the primary pointer button was pressed on this frame (still held).
    pressed_tab: Option<u64>,
}

impl TabViewer for Viewer<'_> {
    type Tab = Tab;

    fn id(&mut self, tab: &mut Tab) -> egui::Id {
        egui::Id::unique(("m0-tab", tab.key))
    }

    fn title(&mut self, tab: &mut Tab) -> egui::WidgetText {
        let name = self
            .sh
            .docs
            .get(tab.doc)
            .map(|d| d.name.as_str())
            .unwrap_or("?");
        format!("{name} · {}쪽 (탭 #{})", tab.page + 1, tab.key).into()
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut Tab) {
        let Some(doc) = self.sh.docs.get(tab.doc) else {
            ui.label("문서 없음");
            return;
        };
        if let Some(err) = &doc.error {
            ui.colored_label(Color32::RED, format!("열기 실패: {err}"));
            return;
        }
        let Some(page_count) = doc.page_count else {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("문서 여는 중…");
            });
            return;
        };
        ui.horizontal(|ui| {
            if ui
                .add_enabled(tab.page > 0, egui::Button::new("◀ 이전 쪽"))
                .clicked()
            {
                tab.page -= 1;
            }
            ui.label(format!("{} / {page_count} 쪽", tab.page + 1));
            if ui
                .add_enabled(tab.page + 1 < page_count, egui::Button::new("다음 쪽 ▶"))
                .clicked()
            {
                tab.page += 1;
            }
        });

        let key = (tab.doc, tab.page);
        let Some(rp) = self.sh.pages.get(&key) else {
            self.sh.request(tab.doc, tab.page);
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("렌더링 중… (워커 스레드)");
            });
            return;
        };
        let (w, h, rgb, tex_id) = (rp.width, rp.height, rp.rgb.clone(), rp.tex.id());
        ui.horizontal_wrapped(|ui| {
            ui.label(format!(
                "텍스처 {tex_id:?} · {w}×{h}px · 주 창 프레임에서 생성, 이 창은 창 {}",
                self.win_id
            ));
            if ui.button("이미지 복사 (a) ctx.copy_image").clicked() {
                ui.ctx().copy_image(ColorImage::from_rgb([w, h], &rgb));
                self.sh.log(format!(
                    "(a) ctx.copy_image 호출: {w}x{h}px. 다른 앱에 붙여 넣어 보세요"
                ));
            }
            if ui.button("이미지 복사 (b) wl-copy").clicked() {
                self.sh.log(format!(
                    "(b) PNG 인코딩 → wl-copy --type image/png 시작: {w}x{h}px"
                ));
                copy_png_with_wl_copy(
                    w as u32,
                    h as u32,
                    rgb,
                    self.sh.tx.clone(),
                    ui.ctx().clone(),
                );
            }
        });
        let avail = ui.available_width().max(100.0);
        let size = egui::vec2(avail, avail * h as f32 / w as f32);
        ui.add(egui::Image::from_texture(SizedTexture::new(tex_id, size)));
    }

    fn context_menu(&mut self, ui: &mut egui::Ui, tab: &mut Tab, _path: NodePath) {
        if ui.button("새 창으로 분리").clicked() {
            self.sh.actions.push(Action::DetachTab {
                key: tab.key,
                why: "메뉴".into(),
            });
            ui.close();
        }
        let others: Vec<u64> = self
            .sh
            .window_ids
            .iter()
            .copied()
            .filter(|&w| w != self.win_id)
            .collect();
        for w in others {
            let label = if w == MAIN_WIN {
                format!("창 {w} (주 창) 으로 이동")
            } else {
                format!("창 {w} 으로 이동")
            };
            if ui.button(label).clicked() {
                self.sh.actions.push(Action::MoveTab {
                    key: tab.key,
                    to: w,
                    why: "메뉴".into(),
                });
                ui.close();
            }
        }
    }

    fn on_tab_button(&mut self, tab: &mut Tab, response: &egui::Response) {
        let (origin, down) = response
            .ctx
            .input(|i| (i.pointer.press_origin(), i.pointer.primary_down()));
        if down && origin.is_some_and(|p| response.rect.contains(p)) {
            self.pressed_tab = Some(tab.key);
        }
    }

    fn allowed_in_windows(&self, _tab: &mut Tab) -> bool {
        // egui_dock's own floating windows are disabled; real OS windows are used instead.
        false
    }
}

fn copy_png_with_wl_copy(w: u32, h: u32, rgb: Arc<Vec<u8>>, tx: Sender<Msg>, ctx: egui::Context) {
    thread::spawn(move || {
        let t = Instant::now();
        let result = (|| -> Result<String, String> {
            let mut buf = Vec::new();
            {
                let mut enc = png::Encoder::new(&mut buf, w, h);
                enc.set_color(png::ColorType::Rgb);
                enc.set_depth(png::BitDepth::Eight);
                let mut wr = enc.write_header().map_err(|e| format!("png header: {e}"))?;
                wr.write_image_data(&rgb)
                    .map_err(|e| format!("png data: {e}"))?;
                wr.finish().map_err(|e| format!("png finish: {e}"))?;
            }
            let mut child = Command::new("wl-copy")
                .args(["--type", "image/png"])
                .stdin(Stdio::piped())
                .spawn()
                .map_err(|e| format!("wl-copy 실행 실패: {e}"))?;
            if let Some(mut stdin) = child.stdin.take() {
                stdin
                    .write_all(&buf)
                    .map_err(|e| format!("wl-copy stdin: {e}"))?;
            }
            let status = child.wait().map_err(|e| format!("wl-copy wait: {e}"))?;
            Ok(format!(
                "PNG {} 바이트, wl-copy 종료 상태 {status}",
                buf.len()
            ))
        })();
        let ms = t.elapsed().as_millis();
        let line = match result {
            Ok(s) => format!("(b) 완료 ({ms}ms): {s}. 다른 앱에 붙여 넣어 보세요"),
            Err(e) => format!("(b) 실패 ({ms}ms): {e}"),
        };
        let _ = tx.send(Msg::Status(line));
        ctx.request_repaint();
    });
}

// ---------------------------------------------------------------------------
// Per-window frame

fn window_title(id: u64) -> String {
    if id == MAIN_WIN {
        "refdesk M0 — 창 0 (주 창)".into()
    } else {
        format!("refdesk M0 — 창 {id}")
    }
}

fn window_ui(ui: &mut egui::Ui, win: &mut Win, sh: &mut Shared) {
    let ctx = ui.ctx().clone();
    let wid = win.id;

    egui::Panel::top(("m0-top", wid)).show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.strong(if wid == MAIN_WIN {
                "창 0 (주 창)".to_string()
            } else {
                format!("창 {wid}")
            });
            ui.separator();
            if ui.button("새 창").clicked() {
                sh.actions.push(Action::NewWindow);
            }
            if ui
                .add_enabled(
                    !sh.picker_open,
                    egui::Button::new("PDF 열기… (파일 선택기)"),
                )
                .clicked()
            {
                sh.picker_open = true;
                sh.log(format!(
                    "창 {wid}: 파일 선택기 여는 중 (rfd/xdg-portal, 별도 스레드)"
                ));
                let tx = sh.tx.clone();
                let c = ctx.clone();
                thread::spawn(move || {
                    let t = Instant::now();
                    let path = rfd::FileDialog::new()
                        .add_filter("PDF", &["pdf"])
                        .set_title("PDF 열기")
                        .pick_file();
                    let _ = tx.send(Msg::FilePicked {
                        win: wid,
                        path,
                        ms: t.elapsed().as_millis(),
                    });
                    c.request_repaint();
                });
            }
            ui.separator();
            ui.label("결합 판정 시간:");
            ui.add(egui::Slider::new(&mut sh.threshold_ms, 50..=500).suffix(" ms"));
            ui.separator();
            ui.label(format!("글꼴: {}", sh.font_mode));
        });
        ui.horizontal(|ui| {
            ui.label("한글 입력 시험:");
            ui.add(
                egui::TextEdit::singleline(&mut sh.ime_text)
                    .hint_text("여기에 한글을 입력해 보세요")
                    .desired_width(320.0),
            );
            ui.label(format!("({}자)", sh.ime_text.chars().count()));
        });
    });

    egui::Panel::bottom(("m0-log", wid))
        .resizable(true)
        .default_size(170.0)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.strong("기록");
                if ui.small_button("지우기").clicked() {
                    sh.log.clear();
                }
            });
            egui::ScrollArea::vertical()
                .id_salt(("m0-log-scroll", wid))
                .stick_to_bottom(true)
                .auto_shrink(false)
                .show_rows(
                    ui,
                    ui.text_style_height(&egui::TextStyle::Monospace),
                    sh.log.len(),
                    |ui, rows| {
                        for line in &sh.log[rows] {
                            ui.monospace(line);
                        }
                    },
                );
        });

    let mut pressed_tab = None;
    egui::CentralPanel::default().show(ui, |ui| {
        if win.dock.iter_all_tabs().next().is_none() {
            ui.label("탭 없음. 다른 창의 탭을 이 창으로 옮기거나 \"PDF 열기…\"를 누르세요.");
        }
        let mut viewer = Viewer {
            win_id: wid,
            sh,
            pressed_tab: None,
        };
        DockArea::new(&mut win.dock)
            .id(egui::Id::unique(("m0-dock", wid)))
            .style(Style::from_egui(ui.style().as_ref()))
            .show_leaf_close_all_buttons(false)
            .show_leaf_collapse_buttons(false)
            .show_inside(ui, &mut viewer);
        pressed_tab = viewer.pressed_tab;
    });

    track_drag(&ctx, win, sh, pressed_tab);
    sample_probe(&ctx, wid, sh);

    if wid != MAIN_WIN && ctx.input(|i| i.viewport().close_requested()) && !win.close_requested {
        win.close_requested = true;
        let n = win.dock.iter_all_tabs().count();
        sh.log(format!(
            "창 {wid}: 닫기 요청 → 이 창만 닫음 (탭 {n}개 버림)"
        ));
    }
}

/// Watches tab drags in this window and starts a `DropProbe` when a tab is released outside it.
fn track_drag(ctx: &egui::Context, win: &mut Win, sh: &mut Shared, pressed_tab: Option<u64>) {
    let (dragging, released, down, latest, interact) = ctx.input(|i| {
        (
            i.pointer.is_decidedly_dragging(),
            i.pointer.primary_released(),
            i.pointer.primary_down(),
            i.pointer.latest_pos(),
            i.pointer.interact_pos(),
        )
    });
    let area = ctx.viewport_rect();
    let wid = win.id;

    if let Some(key) = pressed_tab
        && dragging
        && ctx.dragged_id().is_some()
        && win.drag.is_none()
    {
        sh.log(format!("창 {wid}: 탭 #{key} 끌기 시작 (창 영역 {area:?})"));
        win.drag = Some(DragTrack {
            tab_key: key,
            frames: 0,
            outside_frames: 0,
            first_outside: None,
            last_outside: None,
            pointer_lost_frames: 0,
            last_pos: None,
        });
    }

    if let Some(tr) = &mut win.drag
        && down
    {
        tr.frames += 1;
        match latest {
            Some(p) => {
                tr.last_pos = Some(p);
                if !area.contains(p) {
                    tr.outside_frames += 1;
                    tr.last_outside = Some(p);
                    if tr.first_outside.is_none() {
                        tr.first_outside = Some(p);
                        let line = format!(
                            "창 {wid}: 끄는 중 창 밖 좌표가 들어옴 ({:.0},{:.0})",
                            p.x, p.y
                        );
                        sh.log(line);
                    }
                }
            }
            None => {
                tr.pointer_lost_frames += 1;
                if tr.pointer_lost_frames == 1 {
                    sh.log(format!(
                        "창 {wid}: 끄는 중 포인터를 잃음 (PointerGone, 버튼은 눌린 상태)"
                    ));
                }
            }
        }
    }

    let ended_without_release = !released && !down && win.drag.is_some();
    if released || ended_without_release {
        let Some(tr) = win.drag.take() else { return };
        let pos = interact.or(latest).or(tr.last_pos);
        let outside = pos.is_none_or(|p| !area.contains(p));
        let pos_s = pos.map_or("없음".to_string(), |p| format!("({:.0},{:.0})", p.x, p.y));
        let how = if released {
            "primary_released"
        } else {
            "놓음 이벤트 없이 버튼이 올라가 있음"
        };
        sh.log(format!(
            "창 {wid}: 탭 #{} 놓음 [{how}] 위치 {pos_s} {} | 끄는 동안 프레임 {} 중 창 밖 좌표 {}프레임 (처음 {:?}, 마지막 {:?}), 포인터 잃음 {}프레임",
            tr.tab_key,
            if outside { "→ 창 밖" } else { "→ 창 안 (egui_dock 이 처리)" },
            tr.frames,
            tr.outside_frames,
            tr.first_outside,
            tr.last_outside,
            tr.pointer_lost_frames,
        ));
        if outside {
            if let Some(old) = sh.probe.take() {
                sh.log(format!(
                    "이전 판정(탭 #{}) 기록을 중단하고 새로 시작",
                    old.tab_key
                ));
            }
            let threshold_ms = sh.threshold_ms;
            sh.log(format!(
                "판정 시작: {threshold_ms}ms 안에 포인터가 들어온 다른 창에 결합, 없으면 새 창으로 분리. {PROBE_WINDOW_MS}ms 동안 프레임별 기록"
            ));
            sh.probe = Some(DropProbe {
                t0: Instant::now(),
                source: wid,
                tab_key: tr.tab_key,
                threshold_ms,
                decided: None,
                seen: HashMap::new(),
            });
        }
    }
}

/// Records, for this window's frame, whether it has the pointer while a drop probe is running.
fn sample_probe(ctx: &egui::Context, wid: u64, sh: &mut Shared) {
    let Some(probe) = &mut sh.probe else { return };
    let ms = probe.t0.elapsed().as_millis();
    if ms > PROBE_WINDOW_MS {
        return;
    }
    let (has, pos, moved, gone) = ctx.input(|i| {
        let moved = i
            .raw
            .events
            .iter()
            .any(|e| matches!(e, egui::Event::PointerMoved(_)));
        let gone = i
            .raw
            .events
            .iter()
            .any(|e| matches!(e, egui::Event::PointerGone));
        (i.pointer.has_pointer(), i.pointer.latest_pos(), moved, gone)
    });
    let mut s = format!("창{wid}@{ms}ms:");
    match pos {
        Some(p) if has => s.push_str(&format!("포인터({:.0},{:.0})", p.x, p.y)),
        _ => s.push('-'),
    }
    if moved {
        s.push_str("[moved]");
    }
    if gone {
        s.push_str("[gone]");
    }
    sh.pass_samples.push(s);
    if has {
        *probe.seen.entry(wid).or_default() += 1;
    }
    if probe.decided.is_none() && wid != probe.source && has && ms <= probe.threshold_ms as u128 {
        let why = format!("놓은 뒤 {ms}ms 에 창 {wid} 이 포인터를 받음");
        probe.decided = Some(format!("창 {wid} 에 결합 ({why})"));
        let key = probe.tab_key;
        sh.actions.push(Action::MoveTab { key, to: wid, why });
    }
}

// ---------------------------------------------------------------------------
// App

struct App {
    sh: Shared,
    windows: Vec<Win>,
    next_win_id: u64,
    smoke: bool,
    smoke_ok: Arc<AtomicBool>,
    closing: bool,
    passes: u64,
}

impl App {
    fn take_tab(&mut self, key: u64) -> Option<(u64, Tab)> {
        for w in &mut self.windows {
            if let Some(path) = w.dock.find_tab_from(|t| t.key == key) {
                return w.dock.remove_tab(path).map(|t| (w.id, t));
            }
        }
        None
    }

    fn add_window(&mut self, tabs: Vec<Tab>) -> u64 {
        let id = self.next_win_id;
        self.next_win_id += 1;
        self.windows.push(Win {
            id,
            dock: new_dock(tabs),
            close_requested: false,
            drag: None,
        });
        id
    }

    fn apply_actions(&mut self) {
        let actions = std::mem::take(&mut self.sh.actions);
        for a in actions {
            match a {
                Action::MoveTab { key, to, why } => {
                    let Some((from, tab)) = self.take_tab(key) else {
                        continue;
                    };
                    let target = self
                        .windows
                        .iter_mut()
                        .find(|w| w.id == to && !w.close_requested);
                    match target {
                        Some(w) => {
                            w.dock.push_to_focused_leaf(tab);
                            self.sh
                                .log(format!("탭 #{key}: 창 {from} → 창 {to} 이동 ({why})"));
                        }
                        None => {
                            self.windows[0].dock.push_to_focused_leaf(tab);
                            self.sh
                                .log(format!("탭 #{key}: 대상 창 {to} 이 없어 주 창으로 ({why})"));
                        }
                    }
                }
                Action::DetachTab { key, why } => {
                    let Some((from, tab)) = self.take_tab(key) else {
                        continue;
                    };
                    let id = self.add_window(vec![tab]);
                    self.sh.log(format!(
                        "탭 #{key}: 창 {from} 에서 분리 → 새 창 {id} ({why})"
                    ));
                }
                Action::NewWindow => {
                    if self.sh.docs.is_empty() {
                        self.sh
                            .log("새 창: 열린 문서가 없어 만들지 않음 (빈 창은 바로 닫히므로)");
                        continue;
                    }
                    let tab = self.sh.new_tab(0, 0);
                    let id = self.add_window(vec![tab]);
                    self.sh.log(format!("새 창 {id} (문서 #0 1쪽 탭 하나)"));
                }
                Action::OpenFile { win, path } => {
                    let doc = self.sh.open_doc(path);
                    let tab = self.sh.new_tab(doc, 0);
                    let w = self
                        .windows
                        .iter_mut()
                        .find(|w| w.id == win && !w.close_requested);
                    match w {
                        Some(w) => w.dock.push_to_focused_leaf(tab),
                        None => self.windows[0].dock.push_to_focused_leaf(tab),
                    }
                }
            }
        }
        // Close windows that asked to close or became empty (never the main window).
        let mut closed = Vec::new();
        self.windows.retain(|w| {
            let keep =
                w.id == MAIN_WIN || (!w.close_requested && w.dock.iter_all_tabs().next().is_some());
            if !keep {
                closed.push((w.id, w.close_requested));
            }
            keep
        });
        for (id, requested) in closed {
            let why = if requested {
                "닫기 요청"
            } else {
                "탭이 모두 떠남"
            };
            self.sh.log(format!("창 {id} 닫힘 ({why})"));
        }
        self.sh.window_ids = self.windows.iter().map(|w| w.id).collect();
    }

    fn finish_pass(&mut self) {
        let samples = std::mem::take(&mut self.sh.pass_samples);
        let Some(probe) = &mut self.sh.probe else {
            return;
        };
        let ms = probe.t0.elapsed().as_millis();
        let mut lines = Vec::new();
        if !samples.is_empty() {
            lines.push(format!("  +{ms:>3}ms  {}", samples.join("  ")));
        }
        let mut action = None;
        if probe.decided.is_none() && ms > probe.threshold_ms as u128 {
            let why = format!("{}ms 안에 다른 창이 포인터를 받지 못함", probe.threshold_ms);
            probe.decided = Some(format!("새 창으로 분리 ({why})"));
            action = Some(Action::DetachTab {
                key: probe.tab_key,
                why,
            });
        }
        let done = ms > PROBE_WINDOW_MS;
        if done {
            let mut seen: Vec<_> = probe
                .seen
                .iter()
                .map(|(w, n)| format!("창 {w}: {n}프레임"))
                .collect();
            seen.sort();
            lines.push(format!(
                "판정 끝 (탭 #{}, 출발 창 {}, 판정 시간 {}ms): {} | {PROBE_WINDOW_MS}ms 동안 포인터를 가진 창: {}",
                probe.tab_key,
                probe.source,
                probe.threshold_ms,
                probe.decided.as_deref().unwrap_or("?"),
                if seen.is_empty() { "없음".to_string() } else { seen.join(", ") }
            ));
        }
        if let Some(a) = action {
            self.sh.actions.push(a);
        }
        if done {
            self.sh.probe = None;
        } else {
            self.sh.ctx.request_repaint();
        }
        for l in lines {
            self.sh.log(l);
        }
    }
}

fn new_dock(tabs: Vec<Tab>) -> DockState<Tab> {
    let mut tr = Translations::english();
    tr.tab_context_menu.close_button = "탭 닫기".into();
    DockState::new(tabs).with_translations(tr)
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.passes += 1;
        let ctx = ui.ctx().clone();
        self.sh.drain_messages(&ctx);

        // Main window = ROOT viewport.
        {
            let App { sh, windows, .. } = self;
            window_ui(ui, &mut windows[0], sh);
        }

        // Other windows: immediate viewports, each with its own dock state.
        {
            let App { sh, windows, .. } = self;
            for win in windows.iter_mut().skip(1) {
                if win.close_requested {
                    continue;
                }
                let vid = ViewportId::from_hash_of(("m0-win", win.id));
                let builder = egui::ViewportBuilder::default()
                    .with_title(window_title(win.id))
                    .with_inner_size([820.0, 900.0]);
                ctx.show_viewport_immediate(vid, builder, |ui, _class| {
                    window_ui(ui, win, sh);
                });
            }
        }

        self.finish_pass();
        self.apply_actions();

        if self.smoke {
            let elapsed = self.sh.start.elapsed();
            if elapsed >= Duration::from_secs(2) && !self.closing {
                self.closing = true;
                let opened = self
                    .sh
                    .docs
                    .iter()
                    .filter(|d| d.page_count.is_some())
                    .count();
                let failed = self.sh.docs.iter().filter(|d| d.error.is_some()).count();
                let ok = failed == 0 && (self.sh.docs.is_empty() || !self.sh.pages.is_empty());
                self.smoke_ok.store(ok, Ordering::SeqCst);
                println!(
                    "smoke-test: {:.2}s elapsed, passes={}, windows={}, docs opened={opened} failed={failed}, textures={}, ok={ok}",
                    elapsed.as_secs_f64(),
                    self.passes,
                    self.windows.len(),
                    self.sh.pages.len()
                );
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            } else {
                ctx.request_repaint_after(Duration::from_millis(100));
            }
        }
    }

    fn on_exit(&mut self) {
        println!(
            "on_exit: 주 창이 닫혀 앱 종료 (창 {}개 열려 있었음)",
            self.windows.len()
        );
    }
}

// ---------------------------------------------------------------------------
// Logging (so egui_winit / arboard clipboard errors become visible) and fonts

struct StdoutLogger;

impl log::Log for StdoutLogger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        let t = m.target();
        if t.starts_with("egui_winit::clipboard") || t.starts_with("arboard") {
            return true;
        }
        if t.starts_with("eframe") || t.starts_with("egui") {
            return m.level() <= log::Level::Info;
        }
        m.level() <= log::Level::Warn
    }
    fn log(&self, r: &log::Record) {
        if self.enabled(r.metadata()) {
            println!("[log {} {}] {}", r.level(), r.target(), r.args());
        }
    }
    fn flush(&self) {}
}

static LOGGER: StdoutLogger = StdoutLogger;

fn load_noto_cjk() -> Result<(PathBuf, egui::FontData), String> {
    use font_kit::family_name::FamilyName;
    use font_kit::handle::Handle;
    use font_kit::properties::Properties;
    use font_kit::source::SystemSource;
    let source = SystemSource::new();
    let handle = source
        .select_best_match(
            &[FamilyName::Title("Noto Sans CJK KR".into())],
            &Properties::new(),
        )
        .map_err(|e| format!("font-kit: {e:?}"))?;
    match handle {
        Handle::Path { path, font_index } => {
            let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            Ok((path, egui::FontData::from_blob(Arc::new(bytes), font_index)))
        }
        Handle::Memory { bytes, font_index } => Ok((
            PathBuf::from("<memory>"),
            egui::FontData::from_blob(Arc::new(bytes.to_vec()), font_index),
        )),
    }
}

fn main() {
    let _ = log::set_logger(&LOGGER).map(|()| log::set_max_level(log::LevelFilter::Debug));

    let mut smoke = false;
    let mut font_mode = "system".to_string();
    let mut files = Vec::new();
    for a in std::env::args().skip(1) {
        if a == "--smoke-test" {
            smoke = true;
        } else if let Some(m) = a.strip_prefix("--font=") {
            font_mode = m.to_string();
        } else if a == "-h" || a == "--help" {
            println!("usage: refdesk-m0 [--smoke-test] [--font=system|noto|none] [FILE.pdf ...]");
            return;
        } else {
            files.push(PathBuf::from(a));
        }
    }
    if !matches!(font_mode.as_str(), "system" | "noto" | "none") {
        println!("unknown --font={font_mode} (system|noto|none)");
        std::process::exit(2);
    }

    let mut options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(window_title(MAIN_WIN))
            .with_inner_size([1000.0, 900.0]),
        ..Default::default()
    };
    // "system": eframe 0.37's own system font fallback (fontique). "noto": fallback off, Noto Sans
    // CJK loaded explicitly. "none": fallback off, nothing added (control case; Hangul should be tofu).
    options.system_font_fallback = font_mode == "system";

    let smoke_ok = Arc::new(AtomicBool::new(false));
    let smoke_ok_app = smoke_ok.clone();
    let start = Instant::now();

    let result = eframe::run_native(
        "refdesk-m0",
        options,
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
            let (tx, rx) = channel();
            let mut font_desc = match font_mode.as_str() {
                "system" => "system (eframe system_font_fallback)".to_string(),
                "none" => "none (대조군: 한글이 네모여야 정상)".to_string(),
                _ => String::new(),
            };
            if font_mode == "noto" {
                match load_noto_cjk() {
                    Ok((path, data)) => {
                        ctx.add_font(egui::FontInsert::new(
                            "NotoSansCJK",
                            data,
                            vec![
                                egui::epaint::text::InsertFontFamily {
                                    family: egui::FontFamily::Proportional,
                                    priority: egui::FontPriority::Lowest,
                                },
                                egui::epaint::text::InsertFontFamily {
                                    family: egui::FontFamily::Monospace,
                                    priority: egui::FontPriority::Lowest,
                                },
                            ],
                        ));
                        font_desc = format!("noto ({})", path.display());
                    }
                    Err(e) => font_desc = format!("noto 불러오기 실패: {e}"),
                }
            }
            let mut sh = Shared {
                ctx: ctx.clone(),
                start,
                docs: Vec::new(),
                pages: HashMap::new(),
                tx,
                rx,
                log: Vec::new(),
                actions: Vec::new(),
                threshold_ms: 150,
                ime_text: String::new(),
                probe: None,
                pass_samples: Vec::new(),
                next_tab_key: 0,
                picker_open: false,
                window_ids: Vec::new(),
                font_mode: font_desc.clone(),
            };
            sh.log(format!(
                "시작. 글꼴 모드: {font_desc}. 렌더러: wgpu (eframe 기본). WAYLAND_DISPLAY={:?}",
                std::env::var("WAYLAND_DISPLAY").ok()
            ));
            let mut main_tabs = Vec::new();
            let mut second_tabs = Vec::new();
            for f in files {
                let doc = sh.open_doc(f);
                main_tabs.push(sh.new_tab(doc, 0));
                main_tabs.push(sh.new_tab(doc, 1));
                // Same document, same page in window 1: shares the texture created in the main window.
                second_tabs.push(sh.new_tab(doc, 0));
            }
            if main_tabs.is_empty() {
                sh.log("명령줄로 받은 PDF 가 없습니다. \"PDF 열기…\"로 여세요.");
            }
            let mut app = App {
                sh,
                windows: Vec::new(),
                next_win_id: 0,
                smoke,
                smoke_ok: smoke_ok_app,
                closing: false,
                passes: 0,
            };
            app.add_window(main_tabs);
            if !second_tabs.is_empty() {
                app.add_window(second_tabs);
            }
            app.sh.window_ids = app.windows.iter().map(|w| w.id).collect();
            Ok(Box::new(app))
        }),
    );

    match result {
        Ok(()) => {
            if smoke && !smoke_ok.load(Ordering::SeqCst) {
                println!("smoke-test: FAILED");
                std::process::exit(1);
            }
            if smoke {
                println!("smoke-test: OK");
            }
        }
        Err(e) => {
            println!("eframe error: {e}");
            std::process::exit(1);
        }
    }
}
