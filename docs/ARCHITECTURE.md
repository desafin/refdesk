# refdesk 아키텍처 v0.1

[SPEC.md](SPEC.md) 가 "무엇을"이라면 이 문서는 "어떤 구조와 계약으로"다.
[PLAN.md](PLAN.md) 의 작업 패키지(WP)는 이 문서의 절 번호를 **계약**으로 인용한다.

- 여기 적힌 타입·함수 시그니처·규칙이 모듈 사이의 약속이다. 구현 세부(비공개 함수, 자료구조)는 구현자가 정한다.
- 계약을 바꿔야 하면 코드보다 이 문서를 먼저 고치고, 그 계약에 기대는 WP 를 확인한다.
- 시그니처는 의도를 보이기 위한 것이다. 수명·제네릭 같은 표기는 컴파일되는 형태로 다듬어도 되지만 의미는 유지한다.

## 1. 불변 규칙

모든 WP 에 적용된다. 어기는 변경은 검수에서 반려한다.

| # | 규칙 |
|---|---|
| R-1 | UI 스레드는 MuPDF 를 부르지 않고 블록하지 않는다(`*_wait` 함수, 채널 `recv`, 큰 파일 IO 금지) |
| R-2 | `crates/*/src` 에서 `mupdf` 크레이트를 import 하는 파일은 `crates/core/src/engine/backend.rs` 하나뿐이다(테스트 디렉터리는 예외) |
| R-3 | `egui_dock` 을 import 하는 파일은 `crates/app/src/windows/dock.rs` 하나뿐이다 |
| R-4 | `refdesk-core` 는 egui·eframe·winit 에 의존하지 않는다 |
| R-5 | 위치는 페이지 공간(§3)의 `Location` 으로만 저장·전달한다. 화면 픽셀 좌표를 상태나 파일에 넣지 않는다 |
| R-6 | 창·탭·문서 목록 같은 앱 구조를 바꾸는 곳은 `dispatch` 뿐이다. 위젯은 `Action` 을 큐에 넣는다 |
| R-7 | 워커가 만든 결과에는 문서 세대 번호가 붙고, 현재 세대와 다르면 버린다 |
| R-8 | 사용자 데이터 파일은 임시 파일에 쓴 뒤 rename 한다. 읽기에 실패하면 기본값으로 뜨고 원본은 `.bad` 로 남긴다 |
| R-9 | 깨진 PDF, 없는 파일, 깨진 설정으로 panic 하지 않는다. `Result` 로 올려 화면에 표시한다. `unwrap`/`expect` 는 테스트 코드와, 불가능함을 주석으로 설명한 곳에만 쓴다 |
| R-10 | 판단 로직(히스토리 규칙, 목적지 해석, 배치 계산, 검색 정규화, diff)은 core 에 두고 단위 테스트한다. app 은 그리기와 입력 연결만 한다 |

## 2. 작업 공간

```
Cargo.toml                 작업 공간. 의존성 버전은 [workspace.dependencies] 한 곳에서 고정
rust-toolchain.toml        1.95
crates/
  core/                    패키지 refdesk-core (라이브러리)
  app/                     패키지 refdesk (실행 파일 refdesk)
spikes/feasibility/        컴파일 확인된 라이브러리 사용 예. 작업 공간에 속하지 않음
docs/  scripts/  assets/   문서, 스크립트, .desktop·아이콘
references/                참고용 외부 소스(gitignore)
```

### 2.1 의존성

이 머신에서 함께 빌드됨을 확인한 것:

```toml
mupdf     = { version = "0.8.0", default-features = false, features = ["system-fonts", "base14-fonts"] }
eframe    = "0.37"
egui_dock = "0.22"
# eframe 의 system_fonts(fontique)가 fontconfig 를 dlopen 모드로 켠다. font-kit 도 맞추지 않으면 컴파일이 깨진다.
font-kit  = { version = "0.14", features = ["source-fontconfig-dlopen"] }
```

쓰기로 한 것(WP-00 이 버전을 고정하며 빌드로 확인): `similar` 3.2, `serde`, `serde_json`, `toml`, `thiserror`, `log`,
`rfd`, `notify`, `directories`, `xxhash-rust`, `unicode-normalization`, `png`, 개발 의존성 `egui_kittest` 0.37.

새 의존성을 더할 때는 WP 보고에 이유를 적는다. C 라이브러리나 시스템 패키지를 요구하는 크레이트는 더하지 않는다.

### 2.2 라이브러리 API 주의

egui·eframe·egui_dock·mupdf 의 현재 API 는 모델이 기억하는 것과 다르다. 이 프로젝트에서 실제로 겪은 예:

- `eframe::App` 의 화면 함수는 `update` 가 아니라 `fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame)` 이고, UI 없는 처리는 `fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame)` 이다.
- `DockArea` 는 `.show_inside(ui, &mut viewer)` 로 그린다. `TabViewer` 는 `id()` 구현이 필수다.
- 자식 창은 `ctx.show_viewport_immediate(id, builder, |ui, class| { .. })` 이다.
- `TopBottomPanel` 은 없다. `egui::Panel::top(id)` / `Panel::bottom(id)` 를 `.show(ui, ..)` 로 그린다. `CentralPanel` 도 `Ui` 를 받는다.
- `ctx.screen_rect()` 는 없다. `ctx.viewport_rect()` / `ctx.content_rect()` 를 쓴다.
- 메뉴를 닫는 것은 `ui.close()` 다(`close_menu` 는 없다). `egui::Id::new` 는 폐기 예정이고 `Id::unique(..)` 를 쓴다.
- 메뉴 막대는 `egui::MenuBar::new().ui(ui, |ui| ui.menu_button(..))`.
- 창 없는 테스트는 `Context::run_ui(RawInput, |ui| ..)` 로 돌린다. 돌려받은 `FullOutput` 은 `.drop_without_applying_deltas()` 로 버려야 한다(안 그러면 drop 에서 panic).
- `Ui::id()` 는 폐기 예정이고 `Ui::scope_id()` 를 쓴다. `Painter::rect_stroke` 는 넷째 인자 `StrokeKind` 가 필요하다.
- Ctrl+휠은 스크롤 값이 아니라 `zoom_delta()` 로 온다. `consume_key` 도 여분의 Shift 를 무시한다(Shift+Space 를 Space 보다 먼저 검사).
- 창 없는 테스트에서 문서 notifier 의 다시 그리기 요청을 기다리려면 `Context::set_request_repaint_callback` 을 쓴다(sleep 없이 조건 대기).
  `RawInput` 에 `max_texture_side` 를 주지 않으면 2048 이다.
- 단축키 비교(`consume_shortcut`)는 여분의 Shift 를 무시한다. Ctrl+Shift+Tab 을 Ctrl+Tab 보다 먼저 검사해야 한다. Ctrl+Shift+= 는 `Key::Plus` 로 온다.
- 글꼴 추가는 `ctx.add_font(FontInsert::new(..))`. eframe 은 `NativeOptions::system_font_fallback`(기본 켜짐)으로 없는 글리프를 시스템 글꼴에서 찾는다.
- egui_dock 0.22: `TabViewer::context_menu(ui, tab, path: NodePath)`, `is_closeable(&self, &Tab)`, `allowed_in_windows(&self, &mut Tab)`.
  `find_tab_from` / `remove_tab` 은 `TabPath` 를 쓴다. 드래그 상태는 비공개이고, 끄는 탭의 `Response` 는 따로 만들어지므로
  `on_tab_button` 에서 `response.dragged()` 로는 끌기를 알 수 없다(`spikes/m0/src/main.rs` 의 감지 방법 참고).
- egui_dock 0.22(이어서): `OnCloseResponse` 는 `egui_dock::tab_viewer::OnCloseResponse` 에 있다. 내장 탭 메뉴는 자기 영어 항목(Close, Eject)을 덧붙이므로
  끄고 `on_tab_button` 의 `response.context_menu(..)` 로 직접 그린다. `Node::Horizontal` 이 좌우, `Vertical` 이 상하 분할이다.
  `Tree::split` 은 비율이 0..=1 밖이면 panic 한다. `DockState::new(vec![])` 는 빈 트리가 아니라 탭 없는 패인 하나다. 탭 내용은 `ScrollArea` 로 감싸지므로 `scroll_bars` 를 꺼야 한다.
- mupdf 0.8: `Pixmap::samples()` 는 `stride()` 에 패딩이 있을 수 있다. `Document` 는 `Send` 가 아니다.
- eframe: 창이 여러 개여도 painter 가 하나라 텍스처는 공유된다. 자식 창의 이벤트는 부모 창을 다시 그리는 것으로 처리된다.

**추측으로 쓰지 말고 고정된 버전의 소스를 읽는다.**

- 정확한 버전의 소스: `~/.cargo/registry/src/*/{egui-0.37.0, eframe-0.37.0, egui_dock-0.22.0, mupdf-0.8.0}`
- 컴파일 확인된 사용 예: `spikes/feasibility/src/bin/probe.rs`(mupdf), `uicheck.rs`(eframe + egui_dock + 자식 창),
  `spikes/m0/src/main.rs`(여러 창, 탭 이동, 끌기 감지, 파일 선택기, 클립보드 — 실제로 실행되는 앱)
- `references/` 의 클론은 더 새 버전이라 시그니처가 다를 수 있다. 설계와 예제를 참고하는 용도다.

## 3. 좌표와 단위

| 공간 | 단위·원점 | 쓰는 곳 |
|---|---|---|
| **페이지 공간** | pt(1/72 인치). 원점은 그 페이지의 왼쪽 위, y 는 아래로 증가 | core 전체, 저장 파일, core↔app 경계 |
| **내용 공간** | 논리 픽셀. 문서 전체를 세로로 이어 놓은 평면 | `PageLayout`, 스크롤 |
| **화면 공간** | egui points(논리 픽셀) | app 의 그리기·입력 |

- 엔진은 MuPDF 가 준 좌표에서 페이지 bounds 의 원점을 빼서 페이지 공간으로 맞춘 뒤에만 밖으로 내보낸다.
- `scale` = 논리 픽셀 / pt. 줌 100% 는 `scale = 96/72`.
- 렌더링 배율 = `scale × pixels_per_point`. 이 머신은 `pixels_per_point = 1.0` 이지만 항상 곱한다.
- 링크 목적지 좌표: MuPDF 0.8.0 은 목적지(`DestinationKind` 의 left/top)를 이미 위쪽 기준 좌표로 준다
  (WP-11 에서 관측: 페이지 공간 (72, 100) 을 가리키게 만든 링크가 `XYZ { left: 72, top: 100 }` 으로 나옴). 뒤집지 않는다.
  WP-12 의 링크 좌표 테스트가 이를 고정한다.

## 4. core — 모듈과 계약

```
crates/core/src/
  lib.rs
  geom.rs            Point, Size, Rect, Quad
  model.rs           Location, ZoomMode, ViewState
  error.rs
  engine/
    mod.rs           DocumentHandle 와 요청·응답 타입 (공개 계약)
    worker.rs        문서 워커 스레드, 우선순위 큐, 취소
    backend.rs       MuPDF 호출 전부 (R-2)
                     (mod.rs 에는 공개 API 껍데기가 먼저 들어가 있고, WP-12 가 구현을 채운다)
  layout.rs          PageLayout
  nav/
    dest.rs          Destination → Location
    history.rs       History
  outline.rs         Outline
  link.rs            Link, LinkTarget, 사각형 병합
  text.rs            PageText, 선택
  search/            normalize.rs, job.rs
  diff/              extract.rs, noise.rs, align.rs, refine.rs, result.rs, job.rs
  store/             paths.rs, atomic.rs, config.rs, docstate.rs, session.rs
  fingerprint.rs
  ipc.rs
  workspace.rs       작업 폴더 훑기와 감시
  testutil.rs        테스트용 PDF 생성기 (feature "testutil")
```

### 4.1 geom · model

```rust
pub struct Point { pub x: f32, pub y: f32 }
pub struct Size  { pub w: f32, pub h: f32 }
pub struct Rect  { pub x0: f32, pub y0: f32, pub x1: f32, pub y1: f32 }   // x0 <= x1, y0 <= y1
pub struct Quad  { pub ul: Point, pub ur: Point, pub ll: Point, pub lr: Point } // 기울어진 글자 대응. bounds() -> Rect

/// 문서 안의 한 점. 페이지 공간.
pub struct Location { pub page: u32, pub x: f32, pub y: f32 }

pub enum ZoomMode { FitWidth, FitPage, Custom(f32) }   // Custom(1.0) = 100%

/// 뷰 하나의 저장 가능한 상태. serde 직렬화.
pub struct ViewState {
    pub anchor: Location,    // 뷰포트 왼쪽 위가 가리키는 문서 위치
    pub zoom: ZoomMode,
    pub history: History,
}
```

### 4.2 engine — core 와 app 의 주 접점

문서마다 워커 스레드 하나가 MuPDF 문서를 소유한다. `DocumentHandle` 은 그 워커와 이야기하는 값싼 복제 가능한 손잡이다.

```rust
pub type Notifier = Arc<dyn Fn() + Send + Sync>;   // 새 결과가 생기면 호출. app 은 repaint 요청을 넘긴다

pub enum DocStatus { Loading, Ready, NeedsPassword, Failed(String) }

pub struct DocumentInfo {
    pub path: PathBuf,
    pub generation: u32,             // reload 마다 +1
    pub page_sizes: Vec<Size>,       // pt. 길이 = 쪽수
    pub page_labels: Vec<String>,    // 없으면 빈 Vec
    pub title: Option<String>,
    pub outline: Outline,            // §4.5
}

pub struct RenderKey { pub page: u32, pub scale_milli: u32, pub generation: u32 }  // scale_milli = 렌더링 배율 × 1000
pub struct RenderedPage { pub key: RenderKey, pub width: u32, pub height: u32, pub rgb: Vec<u8> } // 흰 배경, 픽셀당 3바이트
pub enum Priority { Visible, Preview, Prefetch, Background }                       // 앞일수록 먼저

impl DocumentHandle {
    pub fn open(path: &Path, notifier: Notifier) -> DocumentHandle;   // 즉시 반환. 열기는 워커에서
    pub fn path(&self) -> &Path;
    pub fn status(&self) -> DocStatus;
    pub fn info(&self) -> Option<Arc<DocumentInfo>>;                  // Ready 전에는 None
    pub fn authenticate(&self, password: &str);
    pub fn reload(&self);                                             // 파일을 다시 읽고 세대 +1

    // UI 스레드용. 절대 블록하지 않는다.
    pub fn request_render(&self, key: RenderKey, prio: Priority);
    pub fn poll_rendered(&self) -> Vec<RenderedPage>;                 // 완료된 것을 꺼내 감
    pub fn render_failed(&self, key: RenderKey) -> bool;              // 이 세대에서 그 키의 렌더가 실패했는가(다시 시도되지 않음)
    pub fn page_text(&self, page: u32) -> Option<Arc<PageText>>;      // 없으면 요청을 걸어 두고 None
    pub fn page_links(&self, page: u32) -> Option<Arc<[Link]>>;

    // 백그라운드 작업용. 블록한다 (R-1: UI 스레드에서 호출 금지).
    pub fn page_text_wait(&self, page: u32) -> Result<Arc<PageText>, EngineError>;
    pub fn render_wait(&self, key: RenderKey) -> Result<RenderedPage, EngineError>;
    pub fn render_region_wait(&self, page: u32, region: Rect, scale_milli: u32)
        -> Result<RenderedImage, EngineError>;                         // region 은 페이지 공간. 화면 캡처용
}

pub struct RenderedImage { pub width: u32, pub height: u32, pub rgb: Vec<u8> }
```

동작 규칙

- **요청은 "지금 이것이 필요하다"는 선언이다.** app 은 필요한 동안 매 프레임 `request_render` 를 다시 부른다. 같은 키의 중복 요청은 하나로 합친다.
- 워커는 우선순위가 높은 것, 같은 우선순위면 가장 최근에 요청된 것부터 처리한다(빠르게 스크롤할 때 지금 보이는 쪽이 먼저 나오게).
- 250ms 동안 다시 요청되지 않은 대기 중 렌더 요청은 버린다. 작업 단위(쪽) 사이마다 큐를 다시 확인한다.
- 텍스트와 링크는 한 번 추출하면 문서가 열려 있는 동안 보관한다. 렌더 결과는 core 가 보관하지 않는다(app 의 텍스처 캐시가 주인).
- 결과가 생길 때마다 `notifier` 를 부른다.
- `reload` 후에는 이전 세대 키의 요청·결과를 버린다(R-7).
- 열기 실패, 페이지 로드 실패는 `DocStatus::Failed` 또는 `EngineError` 로 전달한다(R-9).
- `render_region_wait` 는 쪽 전체를 그 배율로 그린 뒤 잘라 내도 된다(최적화는 나중). 쪽 밖으로 나간 영역은 쪽 안으로 줄인다.
- 워커는 `DocumentHandle` 의 마지막 복제본이 사라지면 종료한다.
- 처리 순서: 제어(reload, 암호) → `Visible`/`Preview` 렌더 → 블록하지 않는 텍스트·링크 요청 → `*_wait` 요청 → `Prefetch`/`Background` 렌더.
  즉 화면에 보이는 것이 검색·비교 같은 백그라운드 작업보다 먼저다.
- 실패한 렌더 키·쪽은 기억해 두고 다시 시도하지 않는다(매 프레임 재시도 방지). `reload` 때 비운다.
- 쪽 하나의 크기를 읽지 못하면 앞 쪽의 크기로 두고 문서는 연다. 렌더 하나의 픽셀 수는 1억 2천만으로 제한한다.
- 다른 파일을 가리키는 링크의 목적지는 쪽 번호만 살린다(`DestKind::Fit`).
- mupdf 0.8.0 은 깨진 링크 목적지에서 panic 할 수 있어 backend 가 MuPDF 호출을 `catch_unwind` 로 감싼다.
- 시험용으로 `DocumentHandle::open_with(path, notifier, EngineOptions)` 가 있다(대기 요청 만료 시간 조절).

참고 설계: `references/viewers/tdf/src/renderer.rs`(전용 스레드, 쪽 사이 취소, 현재 쪽 주변 우선).

### 4.3 layout

연속 세로 배치의 계산을 전부 맡는다. 순수 함수라 단위 테스트한다.

```rust
pub struct LayoutParams { pub viewport: Size, pub zoom: ZoomMode, pub gap: f32, pub margin: f32, // 논리 픽셀
                           pub fit_page: u32 }                    // FitPage 의 기준 쪽

impl PageLayout {
    pub fn new(page_sizes: &[Size], params: LayoutParams) -> PageLayout;
    pub fn scale(&self) -> f32;                                   // 논리 픽셀 / pt
    pub fn content_size(&self) -> Size;
    pub fn page_rect(&self, page: u32) -> Rect;                   // 내용 공간
    pub fn visible_pages(&self, scroll: Point, viewport: Size) -> Range<u32>;
    pub fn location_at(&self, content: Point) -> Location;        // 쪽 사이·바깥은 가장 가까운 쪽으로
    pub fn content_pos(&self, loc: Location) -> Point;
    pub fn max_scroll(&self, viewport: Size) -> Point;
    pub fn clamp_scroll(&self, scroll: Point, viewport: Size) -> Point;
    /// loc 이 뷰포트 안의 viewport_pos 자리에 오게 하는 스크롤. 줌할 때 커서 아래 지점을 고정하는 데 쓴다.
    pub fn scroll_to_keep(&self, loc: Location, viewport_pos: Point, viewport: Size) -> Point;
}
```

- `FitWidth` 는 문서에서 가장 넓은 쪽을 기준으로 한다(쪽마다 배율이 달라지지 않게). `FitPage` 는 `fit_page`(보통 `anchor` 가 있는 쪽) 기준.
- 줌은 10% ~ 800% 로 제한한다(`MIN_ZOOM`, `MAX_ZOOM`).
- 쪽은 가로 가운데 정렬. 내용이 뷰포트보다 넓으면 가로 스크롤.
- 뷰의 스크롤 상태는 `ViewState.anchor` 가 원본이다. 배치가 바뀌면(줌, 창 크기) `content_pos(anchor)` 로 스크롤을 다시 구한다 — 그래서 읽던 곳이 유지된다.

### 4.4 nav — 목적지와 히스토리

```rust
pub enum DestKind {
    Xyz  { left: Option<f32>, top: Option<f32> },
    Fit, FitH { top: Option<f32> }, FitV { left: Option<f32> }, FitR(Rect),
}
pub struct Destination { pub page: u32, pub kind: DestKind }      // 페이지 공간으로 정규화된 상태
pub fn resolve(dest: &Destination, page_size: Size) -> Location;
```

`resolve`: 없는 값은 0. `Fit` 은 쪽의 (0,0), `FitH` 는 (0, top), `FitV` 는 (left, 0), `FitR` 은 사각형의 왼쪽 위.
음수는 0 으로 올린다(약간 어긋난 목적지가 앞 쪽을 보여 주지 않게). 목적지가 요구하는 줌은 무시한다(LINK-1).

```rust
pub struct Place { pub doc: PathBuf, pub loc: Location }

impl History {
    pub fn new(start: Place) -> History;
    pub fn jump(&mut self, from: Place, to: Place);               // 긴 점프
    pub fn back(&mut self, current: Place) -> Option<Place>;
    pub fn forward(&mut self, current: Place) -> Option<Place>;
    pub fn touch(&mut self, current: Place);                      // 스크롤이 멈췄을 때
    pub fn can_back(&self) -> bool;  pub fn can_forward(&self) -> bool;
}
```

히스토리는 "다녀온 곳"의 목록과 "지금 있는 곳"을 가리키는 색인이다.

- `jump(from, to)`: 현재 항목을 `from` 으로 갱신 → 색인 뒤쪽 항목 삭제 → `to` 가 `from` 과 같은 자리(같은 문서·같은 쪽이고 y 차이가 작음)가 아니면 추가하고 색인 이동.
- `back(current)`: 현재 항목을 `current` 로 갱신한 뒤 색인을 하나 앞으로. 그래야 다시 `forward` 했을 때 떠났던 자리로 온다.
- `touch(current)`: 현재 항목의 위치만 갱신. 항목을 늘리지 않는다 — 스크롤이 히스토리를 채우지 않는 이유.
- 최대 200개. 넘치면 가장 오래된 것부터 버린다. serde 직렬화(저장은 최근 50개, LINK-11).

### 4.5 outline

```rust
pub struct OutlineItem {
    pub title: String,
    pub depth: u16,                 // 최상위 0
    pub parent: Option<u32>,
    pub target: Option<Location>,   // 문서 안 목적지가 없으면 None
    pub uri: Option<String>,
}
pub struct Outline { pub items: Vec<OutlineItem> }   // 선순위(문서 순서)로 평탄화

impl Outline {
    pub fn current(&self, at: Location) -> Option<u32>;           // at 이전에 시작하는 마지막 항목
    pub fn ancestors(&self, idx: u32) -> Vec<u32>;                // 가까운 조상부터
    pub fn has_children(&self, idx: u32) -> bool;
    pub fn path_titles(&self, idx: u32) -> Vec<&str>;             // 최상위 → idx (TOC-6 경로 표시)
}
```

`current` 는 (쪽, y) 순으로 비교한다. 같은 쪽에 여러 항목이 있으면 y 로 가린다. 목적지가 문서 순서와 어긋난 목차도 틀리지 않게,
`target` 기준으로 정렬한 색인을 따로 만들어 이분 탐색한다.

### 4.6 link

```rust
pub enum LinkTarget {
    Internal(Destination),
    External(String),                                     // http(s), mailto 등
    OtherDocument { path: PathBuf, dest: Option<Destination> },
}
pub struct Link { pub rects: Vec<Rect>, pub target: LinkTarget }  // rects: 페이지 공간, 줄마다 하나

pub fn hit_test(links: &[Link], p: Point) -> Option<usize>;
```

같은 목적지를 가리키며 문서 순서로 이어진 주석(여러 줄에 걸친 링크)은 하나의 `Link` 로 합친다.
참고: `references/viewers/sioyek/pdf_viewer/document.cpp:861-933`.

### 4.7 text — 텍스트 모델과 선택

```rust
pub struct TextChar  { pub ch: char, pub quad: Quad, pub size: f32 }
pub struct TextLine  { pub chars: Range<u32>, pub bounds: Rect }
pub struct TextBlock { pub lines: Range<u32>, pub bounds: Rect }
pub struct PageText  { pub chars: Vec<TextChar>, pub lines: Vec<TextLine>, pub blocks: Vec<TextBlock> }

impl PageText {
    pub fn char_at(&self, p: Point) -> Option<u32>;               // 글자 위(또는 그 줄의 가까운 여백)인가. 아니면 None
    pub fn nearest_char(&self, p: Point) -> Option<u32>;          // 글자가 있으면 항상 가장 가까운 글자
    pub fn caret_at(&self, p: Point) -> Option<u32>;              // 드래그 선택용 경계 위치(0 ..= 글자 수)
    pub fn word_at(&self, ch: u32) -> Range<u32>;                 // "7.3.2", "SWS_EM_01004" 는 한 단어
    pub fn line_of(&self, ch: u32) -> Range<u32>;
    pub fn block_of(&self, ch: u32) -> Range<u32>;                // 문단
    pub fn rects(&self, range: Range<u32>) -> Vec<Rect>;          // 줄마다 합쳐진 사각형
    pub fn text(&self, range: Range<u32>, opts: CopyOptions) -> String;
}

pub struct TextPos { pub page: u32, pub ch: u32 }
pub struct Selection { pub anchor: TextPos, pub focus: TextPos }  // 쪽을 넘어갈 수 있음
pub struct CopyOptions { pub join_lines: bool, pub dehyphenate: bool }

// Selection: ordered(), is_empty(), pages(), range_on(page, page_chars). TextPos.ch 는 경계 위치다.
pub fn selection_text(sel: &Selection, page: impl Fn(u32) -> Option<Arc<PageText>>, opts: CopyOptions) -> String;
```

글자 순서는 MuPDF 가 준 읽기 순서를 그대로 쓴다. 선택은 글자 색인 범위다(화면 사각형으로 고르지 않는다 — 2단 문서에서 한 단만 선택되게).
이 모델을 검색과 diff 가 그대로 쓴다.

### 4.8 search

```rust
pub struct SearchQuery { pub text: String, pub case_sensitive: bool, pub whole_word: bool, pub regex: bool }
pub struct MatchPart { pub page: u32, pub chars: Range<u32>, pub rects: Vec<Rect> }
pub struct SearchMatch { pub page: u32, pub chars: Range<u32>, pub rects: Vec<Rect>,   // 시작한 쪽의 부분
                         pub cont: Option<MatchPart>,                                    // 다음 쪽으로 넘어간 부분
                         pub before: String, pub matched: String, pub after: String }
pub struct SearchSnapshot { pub matches: Vec<SearchMatch>, pub pages_done: u32, pub pages_total: u32,
                            pub finished: bool, pub truncated: bool }                    // 일치 10만 개에서 멈춤

impl SearchJob {
    pub fn start(doc: DocumentHandle, query: SearchQuery, start_page: u32, notifier: Notifier)
        -> Result<SearchJob, SearchError>;         // 빈 검색어, 잘못된 정규식은 Err
    pub fn snapshot(&self) -> Arc<SearchSnapshot>; // 값싸다. matches 는 항상 문서 순서. 최대 50ms 늦을 수 있다
}   // drop 하면 취소 (기다리지 않는다)
```

- 자체 스레드에서 `page_text_wait` 로 `start_page` 부터 끝까지, 이어서 처음부터 돈다. 쪽마다 취소 여부를 확인한다.
- 정규화(`normalize.rs`): 대소문자(옵션), NFKC(합자), 줄바꿈 → 공백, 글자 사이에 낀 줄 끝 하이픈 제거.
  정규화된 문자열의 각 위치가 원래 글자 색인으로 돌아가는 표를 함께 만든다.
- 쪽 경계에 걸친 문구를 위해 다음 쪽 앞부분을 이어 붙여 검색하고, 일치는 시작한 쪽에 속한다.
- 대소문자를 무시할 때 ß 는 ss 로 본다. 정규식 방식에서는 검색어를 정규화하지 않는다.
- 문서가 아직 열리는 중이면 검색 스레드가 기다린다. 열 수 없는 문서는 결과 없이 끝난다.
- 참고: `references/engines/pdfjs/web/pdf_find_controller.js:95-366`.

### 4.9 diff

파이프라인은 SPEC §8.5. 공개 계약:

```rust
pub struct DiffOptions { pub skip_repeated_lines: bool, pub ignore_case: bool }   // 단어 단위 비교라 공백은 원래 무시된다
pub struct DiffLimits { pub paragraph_timeout: Duration, pub word_timeout: Duration, pub total_word_timeout: Duration }
pub struct DiffInput { pub tokens: DocTokens, pub page_sizes: Vec<Size> }
pub fn diff_tokens(left: &DiffInput, right: &DiffInput, limits: &DiffLimits) -> DiffResult;   // 순수 로직

pub enum ChangeKind { Added, Removed, Modified }
pub struct Span { pub start: TextPos, pub end: TextPos }
pub struct Change { pub kind: ChangeKind, pub left: Option<Span>, pub right: Option<Span>,
                    pub left_text: String, pub right_text: String,      // 목록 표시용 일부(200자 이내)
                    pub anchor: (Location, Location) }                  // 양쪽에서 이 변경이 놓인 자리
pub enum MarkKind { Removed, Added, ModifiedBlock, ModifiedWord }
pub struct Mark { pub rect: Rect, pub kind: MarkKind, pub change: u32 } // change: changes 의 색인
pub enum Side { Left, Right }

pub struct DiffResult {
    pub changes: Vec<Change>,                  // 문서 순서
    pub left_marks: Vec<Vec<Mark>>,            // 쪽별. ModifiedBlock 이 ModifiedWord 보다 앞(순서대로 그리면 단어가 위)
    pub right_marks: Vec<Vec<Mark>>,
    pub sync: Vec<(Location, Location)>,       // 양쪽에서 대응하는 점. 양쪽 모두 엄격히 증가
    pub added: u32, pub removed: u32, pub modified: u32,
    pub left_page_sizes: Vec<Size>, pub right_page_sizes: Vec<Size>,
    pub approximate: bool,                     // 시간 제한에 걸린 구간이 있었다(그 구간은 통째로 삭제 + 추가)
    pub generations: (u32, u32),               // 계산에 쓴 양쪽 문서 세대 (R-7)
}
impl DiffResult {
    pub fn marks(&self, side: Side, page: u32) -> &[Mark];
    pub fn map_location(&self, from: Side, loc: Location) -> Location;   // 동기 스크롤: 한쪽 위치 → 다른 쪽 위치
}

impl DiffJob {
    pub fn start(left: DocumentHandle, right: DocumentHandle, opts: DiffOptions, notifier: Notifier) -> DiffJob;
    pub fn progress(&self) -> f32;                       // 0.0 ..= 1.0, 줄지 않는다
    pub fn result(&self) -> Option<Result<Arc<DiffResult>, String>>;
}   // drop 하면 취소 (기다리지 않는다)
```

- `sync` 는 동기 스크롤의 근거다. `map_location` 이 한쪽 위치를 `sync` 의 이웃한 두 점 사이에서 보간해 다른 쪽 위치를 준다.
- `Modified` 로 묶는 기준: 두 문단이 공유하는 단어가 짧은 쪽 단어 수의 50% 이상. 그보다 적으면 삭제 + 추가다.
- 단어 비교에서 하이픈은 빼고 본다(줄 끝에서 끊긴 복합어가 변경으로 나오지 않게). 하이픈만 달라진 변경은 보이지 않는다.
- 알려진 한계: 옮겨진 문단은 삭제 + 추가로 나온다. 표·다단에서 읽기 순서가 버전마다 다르면 큰 변경으로 나온다.
  `Modified` 의 자리는 묶음의 첫 문단이라, 긴 문단의 뒷부분이 바뀌면 목록에서 눌렀을 때 문단 처음으로 간다.
- 입력은 `diff::extract` 가 만든 `DocTokens`(문단과 단어의 흐름, 반복되는 머리글·바닥글 제거됨)다. 정의는 `crates/core/src/diff/extract.rs`.
- 반복 줄 판정(`diff/noise.rs`): 쪽 위·아래 15% 띠의 줄 가운데, 숫자를 가린 모양과 글자 크기가 같은 것이 전체 쪽의 절반 이상,
  또는 짝수 쪽(홀수 쪽)의 절반 이상, 또는 3쪽 이상 이어져 나타나면 잡음으로 본다.
  알려진 한계: 쪽마다 글이 다른 머리글(현재 절 제목)은 못 거른다. 띠 안에서 여러 쪽 이어지는 표의 열 제목은 지워진다.
- 1차 정렬은 `similar` 의 임의 토큰 비교(문단 해시). 2차는 어긋난 구간 전체를 단어 단위로 다시 맞춘다 —
  같은 글이 버전마다 다른 문단 경계로 쪼개질 수 있어서, 단어가 같으면 문단 경계가 달라도 변경이 아니어야 한다. 모두 시간 제한을 건다.
- 참고: `references/diff/pdf-diff-joshdata/pdf_diff/command_line.py`(전체를 한 흐름으로 비교, 결과를 상자로 되돌리기).

### 4.10 store — 저장

```rust
pub struct Paths { pub state_dir: PathBuf, pub config_dir: PathBuf, pub socket_path: PathBuf, pub capture_dir: PathBuf }
impl Paths {
    pub fn from_env() -> Paths;         // $XDG_STATE_HOME/refdesk, $XDG_CONFIG_HOME/refdesk, $XDG_RUNTIME_DIR/refdesk.sock, ~/Pictures/refdesk
    pub fn under(root: &Path) -> Paths; // 테스트용: 모든 경로를 root 아래로
}
pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()>;   // 임시 파일 → fsync → rename (R-8)
```

모든 파일의 최상위에 `"version"` 을 둔다. 모르는 필드는 무시하고, 없는 필드는 기본값을 쓴다.

**session.json** — 마지막에 열려 있던 것.

```rust
pub struct Session { pub version: u32, pub windows: Vec<SessionWindow>, pub tabs: BTreeMap<String, SessionTab> }
pub struct SessionWindow { pub main: bool, pub size: (f32, f32), pub maximized: bool,
                           pub layout: LayoutNode, pub toc_open: bool, pub toc_width: f32 }
pub enum LayoutNode {
    Tabs  { tabs: Vec<String>, active: usize },                       // 탭 키 목록
    Split { vertical: bool, fraction: f32, first: Box<LayoutNode>, second: Box<LayoutNode> },
}
pub enum SessionTab { Pdf { path: PathBuf, view: ViewState }, Diff { left: PathBuf, right: PathBuf } }
```

배치는 `egui_dock` 의 직렬화 형식이 아니라 우리 `LayoutNode` 로 저장한다. 도킹 라이브러리 버전이 바뀌어도 세션 파일이 유효하도록.
`LayoutNode` ↔ 도킹 상태 변환은 app 의 `windows/dock.rs` 가 한다.

**documents.json** — 문서별 기억.

```rust
pub struct DocRecord { pub path: PathBuf, pub fingerprint: Option<String>, pub size: u64, pub mtime: i64,
                       pub last_opened: i64, pub view: ViewState, pub toc_collapsed: Vec<Vec<String>> }
impl DocStateDb {
    pub fn load(path: &Path) -> DocStateDb;          // 실패해도 빈 DB 로 뜬다 (R-8)
    pub fn lookup(&self, path: &Path, fingerprint: Option<&str>) -> Option<&DocRecord>; // 경로 또는 해시, 최근 것 우선 (SES-4)
    pub fn record(&mut self, rec: DocRecord);
    pub fn recent(&self, n: usize) -> Vec<&DocRecord>;        // SES-7
    pub fn save(&self, path: &Path) -> io::Result<()>;
}
```

저장 함수들은 경로를 인자로 받는다(전역 경로를 몰래 쓰지 않는다 — 테스트가 홈 디렉터리를 건드리지 않게). 시각도 호출자가 넘긴다.
최근 문서 목록은 따로 저장하지 않고 `DocStateDb::recent` 로 얻는다.

`fingerprint` 는 파일 전체의 XXH3-128. 백그라운드에서 계산하고 (size, mtime) 이 같으면 다시 계산하지 않는다.
경로가 같으면 지문이 달라도 같은 문서로 본다(같은 자리에 새 버전이 온 경우).

**prefs.json** — 앱 안에서 바꾼 환경 설정(스크롤 속도 등). 앱이 쓴다. 값이 있으면 `config.toml` 의 같은 항목보다 우선한다.
`config.toml` 은 사람이 고치는 파일이라 앱이 덮어쓰지 않으므로, 메뉴에서 바꾼 값은 여기에 둔다.

**config.toml** — 사람이 고치는 설정: 테마, 기본 줌, 세션 복원 여부, 캐시 예산, 언어, 단축키 재정의.

### 4.11 ipc — 단일 인스턴스

> `IpcRequest::Open` 에는 `find: Option<String>` 이 있다(FIND-12, 없으면 직렬화하지 않는다). 받은 쪽은 파일을 연 뒤 그 글로 모든 문서 찾기를 시작한다.

`socket_path()` 의 유닉스 소켓에 한 줄짜리 JSON 을 보낸다.

```rust
pub enum IpcRequest { Open { targets: Vec<OpenTarget> }, Diff { left: PathBuf, right: PathBuf } }
pub struct OpenTarget { pub path: PathBuf, pub page: Option<u32> }       // path 는 절대 경로
pub fn send(socket: &Path, req: &IpcRequest) -> io::Result<bool>;        // 받은 인스턴스가 있으면 true
pub fn listen(socket: &Path, on_request: impl Fn(IpcRequest) + Send + 'static) -> io::Result<IpcListener>;
// IpcRequest::with_absolute_paths(&cwd): 보내는 쪽이 자기 현재 디렉터리 기준으로 절대 경로로 바꾼다
```

시작 시 `send` 가 `true` 면 그대로 종료. 연결이 안 되는 낡은 소켓 파일은 지우고 새로 연다.
소켓 경로는 인자로 받는다(`Paths::from_env().socket_path`). `OpenTarget` 은 여기(`core::ipc`)에 있고 app 이 가져다 쓴다.
명령줄 해석은 app 의 `cli.rs` 에 있다: `refdesk [파일[#page=N]]...`, `--diff A B`, `--no-single-instance`, `--help`, `--version`.

### 4.13 workspace — 작업 폴더

폴더를 훑어 PDF 파일의 색인을 만든다. 디스크를 읽으므로 백그라운드에서 돈다(R-1).

```rust
pub enum EntryKind { Dir, Pdf }
pub struct WorkspaceEntry { pub name: String, pub rel_path: PathBuf, pub kind: EntryKind,
                            pub parent: Option<u32>, pub depth: u16 }
pub struct WorkspaceIndex { pub root: PathBuf, pub entries: Vec<WorkspaceEntry>, pub truncated: bool }
// entries: 선순위(트리를 위에서 아래로 그린 순서). 폴더 먼저, 이름은 자연 순서. PDF 가 없는 폴더는 들어 있지 않다.

pub struct ScanLimits { pub max_entries: usize, pub max_depth: u16 }       // 기본 100_000, 16
pub fn scan(root: &Path, limits: &ScanLimits, cancel: &AtomicBool) -> io::Result<WorkspaceIndex>;

impl WorkspaceScan {                                                        // 백그라운드 실행
    pub fn start(root: PathBuf, limits: ScanLimits, notifier: Notifier) -> WorkspaceScan;
    pub fn index(&self) -> Option<Arc<WorkspaceIndex>>;   // 끝난 가장 최근 결과
    pub fn scanning(&self) -> bool;
    pub fn rescan(&self);
}   // drop 하면 취소

impl WorkspaceWatcher {                                                     // 폴더 변화 감시
    pub fn start(root: &Path, on_change: impl Fn() + Send + 'static) -> io::Result<WorkspaceWatcher>;
}
```

- PDF 판정은 확장자 `.pdf`(대소문자 무시)만 본다. 이름이 `.` 으로 시작하는 항목은 건너뛴다.
- 폴더 심볼릭 링크는 이미 지나온 폴더로 돌아가지 않을 때만 따라간다(순환 방지).
- 상한을 넘으면 거기서 멈추고 `truncated` 를 세운다. 읽을 수 없는 폴더는 건너뛴다.
- 감시는 폴더와 PDF 파일의 변화만 알리고, 0.5초 동안의 변화를 한 번으로 묶는다. 감시를 걸 수 없으면(감시 수 한도 등) 오류를 돌려주고, 호출자는 수동 새로 고침으로 지낸다.
- 세션에는 작업 폴더와 펼친 폴더를(`Session.workspace`), 문서 기록 파일에는 최근 폴더를 둔다.
- 구현 메모(WP-70): `WorkspaceIndex` 는 `scan` 이나 `WorkspaceIndex::new` 로만 만든다(안에 조회용 색인이 있다). `find` 는 디스크를 읽지 않는다.
  5만 개 파일의 훑기가 86ms, `find` 가 1µs 미만이다. 훑기와 감시의 알림은 백그라운드 스레드에서 불린다.

### 4.12 testutil — 테스트용 PDF

저장소에 외부 PDF 를 넣지 않는다. 내용과 좌표를 아는 PDF 를 테스트 코드에서 만든다.

```rust
let pdf: Vec<u8> = PdfBuilder::new()
    .page(612.0, 792.0, |p| { p.text(72.0, 100.0, 12.0, "1 Introduction"); p.link_to(rect, 2, Some((72.0, 100.0))); })
    .page(612.0, 792.0, |p| { /* .. */ })
    .outline("1 Introduction", 0, 100.0, |o| { o.child("1.1 Scope", 1, 300.0); })
    .build();
```

PDF 를 바이트로 직접 써 내는 작은 생성기다(기본 14 글꼴만 사용, 압축 없음). 좌표 인자는 페이지 공간(왼쪽 위 원점)으로 받고 내부에서 PDF 좌표로 바꾼다.
`feature = "testutil"` 로 app 의 테스트에서도 쓴다.

실제 API 는 `crates/core/src/testutil.rs` 의 문서 주석이 원본이다. 생성기의 쪽 번호는 1부터 센다(코어의 `Location.page` 는 0부터).
여러 쪽에 걸친 문단 흐름은 `flow_document` 로 만든다.

## 5. app — 구조

```
crates/app/src/
  main.rs            명령줄, 단일 인스턴스, eframe 시작
  app.rs             RefdeskApp: eframe::App 구현 (logic, ui)
  state.rs           AppState 와 하위 저장소
  action.rs          Action, dispatch
  commands.rs        명령 표(이름, 기본 단축키, 가능 여부), 단축키 처리
  windows/
    mod.rs           WindowState, 모든 창 그리기(루트 + 자식 뷰포트)
    dock.rs          egui_dock 접점 전부 (R-3)
    transfer.rs      탭의 창 간 분리·결합
  views/
    pdf_view.rs      PDF 뷰 위젯
    textures.rs      텍스처 캐시
    diff_view.rs     비교 뷰
    selection.rs     텍스트 선택의 판단 로직
    links.rs         링크의 판단 로직
    preload.rs       전체 쪽 미리 그리기
    start_page.rs
  panels/            toc.rs, search.rs(검색 막대와 결과 목록), changes.rs(비교 변경 목록), status_bar.rs, breadcrumb.rs
  overlays/          palette.rs, link_preview.rs, goto_page.rs, password.rs, capture.rs
  session.rs         AppState ↔ Session, 지연 저장, 문서별 기억
  cli.rs             명령줄 해석
  i18n.rs  theme.rs
```

### 5.1 상태

```rust
pub struct AppState {
    pub docs: DocStore,            // DocId → DocumentHandle (+ 경로로 찾기). 같은 파일은 하나만
    pub tabs: TabStore,            // TabId → Tab
    pub windows: Vec<WindowState>, // [0] 이 주 창
    pub focus: Option<TabId>,      // 명령이 적용될 뷰 (WIN-6)
    pub textures: TextureCache,
    pub config: Config,
    pub doc_db: DocStateDb,
    pub actions: Vec<Action>,      // 이번 프레임에 쌓인 것
    pub file_dialog_requested: bool, // dispatch 가 세우고 logic() 이 파일 선택기 스레드를 띄운다
}
pub enum Tab { Pdf(PdfTab), Diff(DiffTab), Start }
pub struct PdfTab { pub doc: DocId, pub view: ViewState, pub ui: PdfViewUiState } // ui: 선택, 검색, 호버 등 저장하지 않는 상태
pub struct WindowState { pub id: WindowId, pub dock: DockLayout, pub toc_open: bool, pub toc_width: f32, .. }
```

- 문서는 공유, 뷰는 독립. 탭을 옮기는 것은 `TabId` 를 한 창의 배치에서 다른 창의 배치로 옮기는 것뿐이다.
- `DockLayout` 은 `windows/dock.rs` 가 정의하는, 도킹 상태를 감싼 타입이다. 다른 모듈은 그 메서드만 쓴다.

### 5.2 Action 과 프레임 흐름 (R-6)

```rust
pub enum Action {
    OpenFiles(Vec<OpenTarget>), CloseTab(TabId), DuplicateTab(TabId), FocusTab(TabId),
    DetachTab(TabId), MoveTab { tab: TabId, to: WindowId }, MergeAllWindows, NewWindow, CloseWindow(WindowId),
    Navigate { tab: TabId, to: Location, how: OpenHow }, // 히스토리에 기록되는 긴 점프. 문서는 그 탭의 문서(WP-31)
    ToggleToc,                                           // 포커스된 창의 목차 패널을 켜고 끈다
    Back(TabId), Forward(TabId),
    SetZoom { tab: TabId, zoom: ZoomMode }, StartDiff { left: PathBuf, right: PathBuf },
    // ...
}
pub enum OpenHow { Here, NewTab, SplitSide }
pub fn dispatch(state: &mut AppState, action: Action, ctx: &egui::Context);
```

한 프레임:

1. `logic()`: 외부 이벤트(IPC, 파일 변경)를 `Action` 으로 바꿔 큐에 넣는다. 모든 문서의 `poll_rendered()` 를 꺼내 텍스처로 올린다. 세션 지연 저장 타이머를 돌린다.
2. `ui()`: 주 창을 그리고, 나머지 창을 각각 `show_viewport_immediate` 로 그린다. 위젯은 상태를 읽고 `Action` 을 큐에 넣는다.
3. `ui()` 의 끝: 큐의 `Action` 을 순서대로 `dispatch` 한다. 구조 변경은 여기서만 일어난다.

위젯이 자기 뷰의 `ViewState`(스크롤 위치, 줌)와 `PdfViewUiState` 를 직접 고치는 것은 허용한다. 그것은 구조가 아니라 그 뷰의 내용이다.
`dispatch` 는 창을 띄우지 않고 테스트한다.

### 5.3 명령과 단축키

모든 사용자 동작은 명령 표의 한 줄이다: 식별자, 이름(한/영), 기본 단축키, 지금 가능한지 판단하는 함수, 만들어 낼 `Action`.
메뉴·우클릭 메뉴·명령 팔레트·단축키는 모두 이 표를 읽는다(SPEC 원칙 3). 기능을 추가하는 WP 는 반드시 명령을 등록한다.
단축키는 포커스된 창에서 프레임당 한 번 처리하고, 글자를 입력 중인 텍스트 칸이 있으면 그쪽이 우선이다.

### 5.4 창과 도킹

- 창마다 `DockLayout` 하나. 탭의 내용은 `TabId` 로 `TabStore` 에서 찾아 그린다.
- `dock.rs` 가 제공하는 것: 탭 추가·제거·활성화, 분할, 패인 사이 이동, 탭 우클릭 메뉴 연결, `LayoutNode` 와의 상호 변환, 각 패인의 화면 사각형.
  정확한 API 는 `crates/app/src/windows/dock.rs` 의 `DockLayout`, `DockHost`.
- 탭을 한 창에서 다른 창으로 옮기기: `windows[from].dock.remove_tab(tab)` → `windows[to].dock.add_tab(tab, None)` → `state.focus = Some(tab)`. 탭과 `ViewState` 는 그대로다.
- 탭 우클릭 메뉴는 `commands::TAB_MENU` 표에서 만든다. 창 목록처럼 동적인 항목은 `windows/mod.rs` 의 `tab_menu()` 에서 덧붙인다.
- 창 하나의 내용은 `windows::show_window(ui, state, window_index, queue)` 가 그린다.
- `transfer.rs`: 분리(`DetachTab` → 새 창을 만들고 탭을 옮김), 결합(`MoveTab`), 모두 합치기.
  끌어 놓기에 의한 분리·결합은 M0 결과에 따라 여기에 구현한다(§6).
- 빈 창이 되면(마지막 탭이 떠나면) 그 창은 닫는다. 주 창은 시작 화면 탭을 보여 준다.

### 5.5 PDF 뷰 위젯

매 프레임의 순서:

1. `doc.info()` 가 없으면 상태(`Loading`/`Failed`/`NeedsPassword`)를 그리고 끝.
2. `PageLayout::new` 로 배치 계산. 스크롤 위치는 `content_pos(view.anchor)`.
3. 입력 처리: 스크롤, Ctrl+휠 줌(커서 아래 지점 고정), 드래그 선택, 링크 호버·클릭. 결과로 `view.anchor` 를 갱신하거나 `Action` 을 낸다.
4. 보이는 쪽(+ 주변 쪽은 `Prefetch`: 멈춰 있으면 앞뒤 6쪽, 스크롤 중이면 진행 방향 10쪽·반대쪽 3쪽, 가까운 쪽부터)마다:
   정확한 배율의 텍스처가 있으면 그린다. 없으면 `request_render` 하고,
   같은 쪽의 다른 배율 텍스처(최소한 미리보기, §5.6)가 있으면 늘려서, 없으면 흰 사각형을 그린다(VIEW-3).
5. 겹쳐 그리기: 검색 강조, 선택, 링크 호버, 도착 지점 강조, (비교 뷰에서) 변경 표시.

줌 동작이 이어지는 동안은 새 배율로 렌더를 요청하지 않고 기존 텍스처를 늘려 그리다가, 80ms 멈추면 요청한다.

구현 메모(WP-15)
- 스크롤은 egui 의 `ScrollArea` 를 쓰지 않고 직접 관리한다. 현재 스크롤 값(내용 공간)은 `PdfViewUiState` 에 있고,
  `anchor` 에서 다시 구하는 것은 배치가 바뀌었을 때와 밖에서 `anchor` 를 바꿨을 때뿐이다. 스크롤 막대도 직접 그린다.
- `anchor` 는 쪽 안으로 줄이지 않은 위치로 둔다(쪽 위 여백에서는 y 가 음수일 수 있다). 스크롤 → anchor → 스크롤이 정확히 왕복하게 하려는 것이다.
- 쪽 맞춤의 기준 쪽은 그 방식을 고른 순간의 현재 쪽으로 고정한다.
- 어느 쪽을 어떤 우선순위로 요청할지, 어느 텍스처를 쓸지, 줌 뒤의 스크롤 값은 egui 없이 테스트되는 순수 함수다.

### 5.6 텍스처 캐시

두 층으로 둔다(VIEW-10).

| 층 | 내용 | 수명 |
|---|---|---|
| 미리보기 | 문서의 **모든 쪽**을 낮은 배율로 그린 것. 문서를 열면 백그라운드에서 채운다 | 문서가 열려 있는 동안 버리지 않는다 |
| 선명한 그림 | 지금 배율로 그린 것. 보이는 쪽과 그 주변(스크롤 방향으로 더 멀리) | 바이트 예산 LRU |

- 어느 쪽이든 화면에 들어오면 미리보기가 이미 있으므로 빈 쪽이 보이지 않는다. 선명한 그림이 도착하면 바꿔 그린다.
- 미리보기 배율은 문서마다 정한다: 미리보기 예산을 쪽수로 나눠 쪽 폭을 240 ~ 900 픽셀 사이에서 고른다(쪽이 적은 문서는 거의 읽을 수 있는 해상도, 수천 쪽 문서는 윤곽이 보이는 해상도).
- 미리보기 요청은 엔진에 `Priority::Background` 로, 지금 보는 쪽에서 가까운 쪽부터 묶음으로 낸다(엔진은 다시 요청되지 않은 대기 요청을 버리므로 매 프레임 다음 묶음을 갱신한다).

구현 메모(WP-17)
- 미리보기 배율: 문서당 예산 `min(1GB, 3GB / 열린 문서 수)`, 가장 넓은 쪽의 폭 `W = clamp(sqrt(예산 / (쪽수 × 4 × 평균 세로/가로)), 240, 900)`,
  배율 = `W / 가장 넓은 쪽의 폭(pt)`, 어느 쪽의 긴 변도 `max_texture_side` 를 넘지 않게 낮춘다. `views/preload.rs` 의 순수 함수.
- 캐시는 그 문서의 미리보기 배율과 같은 키를 미리보기 층에 넣는다. 배율이 바뀌면(열린 문서 수 변화) 옛 미리보기는 같은 쪽의 새 것이 올 때까지 남는다.
- 렌더에 실패한 미리보기 쪽은 `render_failed` 로 알아내 "끝난 것"으로 친다. 실패는 notifier 를 부르지 않으므로,
  미리 그리기가 남아 있는 동안은 100ms 마다 프레임을 돌린다.
- 같은 우선순위 안에서는 가장 최근 요청이 먼저 처리되므로, 요청은 중요한 것이 마지막에 오게 거꾸로 낸다.
- 텍스처 올리기는 프레임당 16MB 까지(선명한 그림 먼저). 남은 것은 다음 프레임으로 넘기고 다시 그리기를 요청한다.


키 `(DocId, RenderKey)`, 값은 텍스처 핸들과 바이트 수. 예산(기본 2GB)을 넘으면 가장 오래 안 쓴 것부터 버린다.
"같은 쪽의 가장 가까운 배율" 조회(`nearest(doc, page, generation, scale_milli)`: 배율의 비가 가장 가까운 것, 같으면 더 선명한 쪽)를 제공한다.
세대가 지난 항목은 문서 reload 시 한꺼번에 버린다. 구현은 `views/textures.rs`.

### 5.7 화면 캡처

`overlays/capture.rs`. 캡처 모드에서는 PDF 뷰의 드래그가 텍스트 선택 대신 사각형을 그린다.
놓으면 사각형을 페이지 공간의 `Rect` 로 바꿔(R-5) 백그라운드 스레드에서 `render_region_wait` 를 부르고(R-1),
결과가 오면 클립보드에 넣고 알림을 띄운다. 알림에는 "저장" 버튼이 있다. PNG 인코딩과 파일 쓰기도 백그라운드에서 한다.
사각형이 두 쪽에 걸치면 시작한 쪽 안으로 줄인다.

클립보드: `wl-copy` 가 `PATH` 에 있고 Wayland 세션이면 PNG 를 `wl-copy --type image/png` 의 표준 입력으로 넘기고(백그라운드 스레드, 2초 안에 끝나야 한다),
없거나 실패하면 egui 의 `ctx.copy_image` 를 쓴다. GNOME Wayland 에서 `ctx.copy_image` 는 X11 클립보드로 들어가 다른 앱에 붙지 않을 수 있어서 순서를 이렇게 정했다(§6).
클립보드는 `Clipboard` trait 뒤에 있고(`AppState.capture_clipboard` 로 바꿔 끼운다), 방법 고르기(`plan_methods`)는 순수 함수다.

구현 메모(WP-54)
- 동작은 `Action::StartCapture / CapturePage / CaptureRegion { tab, page, rect, scale_milli } / SaveCapture` 다. 뷰는 놓는 순간 `CaptureRegion` 을 큐에 넣고, 스레드는 `dispatch` 가 띄운다.
- 캡처 모드와 알림은 뷰마다 `PdfViewUiState.capture`(저장하지 않음, 복제하면 비어서 시작). 스레드는 `Inbox` 로 결과를 남기고 뷰가 매 프레임 꺼내 간다. 알림은 6초, 저장하면 6초 더.
- 사각형은 시작점을 **페이지 공간**으로 들고 있다가(스크롤해도 같은 자리) 시작한 쪽의 좌표계로 끝점을 환산해 쪽 안으로 줄인다. 어느 변이든 4 논리 픽셀 미만이면 취소.
- 배율 = 화면 배율(레이아웃 배율 × `pixels_per_point`) × `config.capture.scale`, 올림. 긴 변 8000 픽셀과 엔진의 쪽 전체 1억 2천만 픽셀 제한을 넘지 않게 낮춘다(`capture_scale_milli`).
- 파일 이름은 `문서이름-p쪽-YYYYMMDD-HHMMSS.png`(현지 시각, 쪽은 1부터). 같은 이름이 있으면 `-2`, `-3`. `AppState.paths` 가 없으면 저장은 꺼진다.

### 5.9 사이드바와 파일 탐색기

- 창 왼쪽의 사이드바는 두 구역이다: 위는 파일 탐색기(`panels/explorer.rs`), 아래는 목차(`panels/toc.rs`). 구역마다 접을 수 있고 경계를 끌 수 있다. 배치는 `panels/sidebar.rs` 가 맡는다.
- 작업 폴더는 앱 전체에 하나다(`AppState.workspace`). 모든 창의 탐색기가 같은 색인을 본다. 펼침·거르기·선택 상태는 창마다 따로다.
- 탐색기는 `WorkspaceIndex` 를 읽기만 한다. 보이는 줄 계산, 이름 거르기, 지금 문서까지 펼치기는 egui 없이 테스트되는 순수 함수다(R-10). 줄이 수만 개여도 보이는 줄만 그린다.
- **미리보기 탭**: `PdfTab.preview` 가 참인 탭. 탐색기에서 한 번 눌러 열면 포커스된 패인의 미리보기 탭을 대체한다(없으면 새로 만든다). 두 번 누르기, 탭 제목 두 번 누르기, 복제·분할·창 분리·비교에 쓰이면 고정 탭이 된다. 미리보기 탭도 세션에 저장된다.
- 빠른 열기는 열린 탭, 작업 폴더의 파일, 최근 문서 순으로 보여 준다.

### 5.10 모든 문서에서 찾기 (WP-55, FIND-7 ~ 12)

- 상태는 앱 전체에 하나다: `AppState.global_search: GlobalSearch`(`panels/global_search.rs`). 검색어와 옵션, 문서마다의 `SearchJob`·스냅숏·`PageIndex`, 지금 결과 `(DocId, MatchKey)`. 저장하지 않는다.
- 대상은 "PDF 탭에 열린 문서"다(같은 문서는 한 번). 비교 탭의 문서는 대상이 아니다. 문서의 순서는 파일 이름의 자연 순서(`natural_cmp`), 같으면 경로.
- 프레임마다 `logic()` 에서 `global_search::tick` 을 한 번 부른다: 대상 목록을 다시 구해 새 문서의 작업을 시작하고 닫힌 문서의 작업을 버리며, 세대가 바뀐 문서는 다시 찾고(R-7), 입력 지연이 끝난 검색어를 시작하고, 작업의 스냅숏을 받아 온다. core 의 `SearchJob` 을 문서마다 하나씩 그대로 쓴다(core 는 바꾸지 않는다). UI 스레드는 기다리지 않는다(R-1).
- 사이드바에는 보기가 둘이다: `WindowState.sidebar_view` = `Files`(탐색기 + 목차, 기본) | `Search`. 보기는 창마다 따로지만 검색 상태는 하나라, 어느 창의 검색 보기도 같은 결과를 보인다. 탐색기·목차를 보이는 명령은 보기를 `Files` 로 돌린다.
- 결과 목록은 줄 높이가 일정하고 보이는 줄만 그린다(문서당 일치 10만 개까지 올 수 있다). 줄에는 접근성 이름을 단다.
- 강조: PDF 뷰는 자기 문서의 전체 검색 일치를 문서 내 검색 강조 **아래**에 다른 색으로 그린다. 보이는 쪽의 사각형만 계산한다(`search::match_rects` 재사용).
- 결과로 가기(`GlobalSearchGoTo`, `GlobalSearchStep`)는 dispatch 가 한다(R-6): 탭을 고르고(포커스된 탭이 그 문서면 그것, 아니면 동작이 일어난 창의 탭, 아니면 다른 창의 탭) → `FocusTab` 과 같은 방식으로 포커스 → 뷰를 옮기고 히스토리에 긴 점프로 남긴다.
- 무엇을 대상으로 삼는지, 묶음과 줄, 다음·이전(문서 넘기와 돌기), 탭 고르기, 요약 글은 egui 없이 테스트되는 순수 함수다(R-10).

### 5.11 검색 기록 (WP-56, FIND-13 ~ 15)

- core: `store/search_history.rs` 의 `SearchHistory` — 항목은 `SearchQuery` 의 네 값과 마지막으로 쓴 시각. `record`(같은 것은 맨 앞으로, 100개 상한), `remove`, `clear`, `entries`(최근 것 먼저). 파일은 `state_dir/search_history.json`(`Paths::search_history_file()`), 형식 버전·원자적 쓰기·깨지면 `.bad`(R-8).
- app: `AppState.search_history`. 바꾸는 것은 dispatch 뿐이다(R-6): `RecordSearch(SearchQuery)`, `RemoveSearchHistory(usize)`, `ClearSearchHistory`. 바뀌면 바로 저장한다(`paths` 가 없으면 저장하지 않는다).
- `panels/search_history.rs` 가 입력란 하나에 붙는 부품을 준다: ↑/↓ 불러오기의 상태(`Recall`: 기록의 몇 번째를 보고 있는지와 불러오기 전에 치던 글), 기록 단추와 목록, "2초 동안 그대로"와 앞부분 대체를 판단하는 `Settle`. 문서 내 검색 막대와 모든 문서 찾기의 입력란이 같은 부품을 쓴다. 판단은 순수 함수다(R-10).

### 5.8 세션

`session.rs` 가 `AppState` → `Session` 변환과 그 반대를 맡는다. 구조나 위치가 바뀌면 2초 뒤 저장(연속 변경은 한 번으로), 종료 시 즉시 저장.
복원 시 파일이 없는 탭은 지우지 않고 오류 상태의 탭으로 만든다(SES-6).

### 5.9 목차 패널 (WP-31)

`panels/toc.rs`. 창마다 왼쪽 옆 패널(`WindowState.toc_open`, `toc_width`), 내용은 그 창에서 포커스된 탭의 `Outline`.

- 탭마다 `PdfTab.toc: TocState`(필터 글, 접힌 항목, 저장하지 않음). 접힌 항목은 색인 집합이고, 세션 저장용으로는
  제목 경로 `Vec<Vec<String>>` 로 내보내고 되읽는다(`TocState::collapsed_paths` / `set_collapsed_paths`, `DocRecord.toc_collapsed` 의 형태).
  같은 부모 아래 같은 제목의 형제는 경로 마디 끝에 NUL 을 앞선 형제 수만큼 붙여 구별한다. 문서에 없는 경로는 되읽을 때 무시한다.
  되읽은 경로는 목차가 처음 그려질 때 적용된다. 문서 세대가 바뀌면 접힘 상태는 처음(최상위만)으로 돌아간다.
- 보이는 줄 계산(`visible_rows`), 필터(`filtered_rows`, 일치 구간 포함), 현재 절(`TocState::prepare`), 스크롤 판단(`scroll_target`)은 egui 없는 순수 함수다.
- 현재 절은 `anchor` 가 아니라 뷰포트 높이의 1/4 아래 지점(`reading_location`)으로 `Outline::current` 를 구한다.
  현재 절이 **바뀐 프레임에만** 그 경로를 펼치고(필터 중에는 펼치지 않음), 포인터가 패널 밖일 때만 그 줄로 스크롤한다.
- 목차 항목을 눌러 가는 곳의 x 는 0 으로 둔다(제목 목적지의 x 는 쪽 여백이라, 확대한 뷰가 옆으로 밀리지 않게).
- `Navigate` 는 `Here`(히스토리에 `jump`), `NewTab`(= `DuplicateTab` 후 이동), `SplitSide`(= 오른쪽 `SplitTab` 후 이동)다. 쪽 번호는 문서 쪽수로 줄인다.

## 6. M0 에서 확정될 것

아래는 창을 띄워 봐야 정해진다. 그 전까지 관련 코드는 `windows/` 안에 가둔다.

| 항목 | 지금의 가정 | 확정 후 영향 범위 |
|---|---|---|
| 끌어서 다른 창에 결합(WIN-3) | 놓은 직후 포인터가 들어온 창을 대상으로 판정 | `windows/transfer.rs` |
| 주 창을 닫을 때(WIN-5) | 주 창을 닫으면 저장 후 전체 종료 | `windows/mod.rs`, `app.rs` |
| 텍스처가 모든 창에서 공유되는가 | 공유된다(egui 컨텍스트가 하나) | 아니라면 `views/textures.rs` 를 창별로 |
| `logic()` 과 자식 창의 호출 순서 | 프레임마다 `logic` → `ui`, 자식 창은 `ui` 안에서 | §5.2 의 흐름 |
| 한글 입력 | 텍스트 칸에서 fcitx5 조합이 된다 | 안 되면 검색 입력 방식 재검토 |
| 이미지 클립보드 | `ctx.copy_image` 로 복사한 이미지가 다른 앱에 붙는다 | 안 되면 `overlays/capture.rs` 가 `wl-copy` 를 쓴다(WP-54 는 `wl-copy` 를 먼저 시도하는 순서로 구현) |

## 7. 오류와 로그

- 모듈마다 `thiserror` 오류 타입. core 의 공개 함수는 `Result` 를 돌려주거나, 실패할 수 없게 설계한다(`DocStateDb::load` 처럼).
- 사용자에게 보여야 하는 오류는 그 탭 안에(문서 오류) 또는 창 아래 알림으로(저장 실패 등) 표시한다.
- `log` 로 기록한다. 열기·렌더·검색·비교의 소요 시간은 `debug` 로 남긴다(SPEC 원칙 2: 최적화는 나중이지만 퇴행은 알아챌 수 있게).

## 8. 테스트 규약

- core: 모듈 옆 단위 테스트 + `crates/core/tests/` 통합 테스트. PDF 가 필요하면 `testutil::PdfBuilder`.
- app: `dispatch` 와 상태 변환은 창 없이 테스트. 위젯은 `egui_kittest` 로 상호작용과 화면을 검증.
- 테스트는 창을 띄우지 않고, 네트워크와 사용자 홈 디렉터리를 건드리지 않는다(경로는 임시 디렉터리로 주입).
- 버그를 고칠 때는 그 버그를 재현하는 테스트를 먼저 넣는다.
