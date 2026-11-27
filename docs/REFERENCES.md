# 참고 소스 모음

`scripts/clone-references.sh` 를 실행하면 아래 저장소가 `references/` 에 얕게(depth 1) 클론된다.
`references/` 는 `.gitignore` 대상이라 이 저장소에는 올라가지 않는다 (2026-11-25 기준 약 1GB).

코드를 **읽고 설계를 참고**하는 용도다. GPL/AGPL 코드를 그대로 옮겨 적지 않는다.
(우리 프로젝트는 MuPDF 때문에 AGPL-3.0 이 되므로 라이선스 충돌은 없지만, 출처 관리는 깔끔하게 한다.)

"볼 곳"의 `파일:줄` 은 조사 과정에서 읽은 위치다. ✔ 표시는 직접 다시 확인한 것이고,
나머지는 조사 보고에 근거한 것이므로 구현 전에 해당 코드를 직접 열어 확인한다.

## 뷰어 (`references/viewers/`)

| 저장소 | 라이선스 | 스택 | 가져올 아이디어 → 볼 곳 |
|---|---|---|---|
| `sioyek` | GPL-3.0 | C++/Qt/MuPDF | 문서 공유 + 뷰별 상태(`pdf_viewer/document.cpp:1224`), 히스토리 push/prev 규칙(`main_widget.cpp:2782-2865`), 링크 미리보기 overview(`main_widget.cpp:3841-3895`), 스마트 점프 색인(`document.cpp:1380-1460`, `utils.cpp:1421`), 여러 줄 링크 사각형 병합(`document.cpp:861-933`), 줌 중 가까운 배율 재사용(`pdf_renderer.cpp:140-234`), 목차 없는 문서의 제목 합성(`utils.cpp:2283`) |
| `okular` | GPL-2.0+ | C++/KDE/Poppler | 같은 페이지 이동은 히스토리 덮어쓰기(`core/document.cpp:3659-3700`), 링크 미리보기 위젯(`part/pageview.cpp:147-300`), 목차 현재 절의 조상 체인 강조(`gui/tocmodel.cpp:145-170`), 쿼드트리 타일(`core/tilesmanager.cpp`), 문서별 히스토리 10개 저장(`core/document.cpp:1303-1335`) |
| `qpdfview` | GPL-2.0+ | C++/Qt | 탭 세션 스키마 `tabs_v5`(`sources/database.cpp:397-420`), 목차 펼침 상태 저장(`database.cpp:704`), 가운데 클릭 = 새 탭에서 링크 열기(`pageitem.cpp:526`), 검색 결과 모델 + 문맥(`searchmodel.h:104-126`), 줌 중 이전 픽스맵 유지(`tileitem.cpp:97-160`) |
| `zathura`, `zathura-pdf-mupdf` | zlib | C/GTK/MuPDF | ✔ 문서 식별 "경로 OR 내용 해시, 최근 것 우선"(`zathura/database-sqlite.c:758`), 점프리스트 저장(`jumplist.c`), 키보드 링크 힌트(`shortcuts.c:284`) |
| `papers` | GPL-2.0+ | Rust 셸 + C 라이브러리 | 백링크(현재 위치를 XYZ 목적지로) 먼저 push(`shell/src/document_view/signals.rs:419-467`), 목차 현재 항목 `BTreeMap` 조회(`shell/src/sidebar_links.rs:568-611`), 목차 펼침 상태 저장(`sidebar_links.rs:15-160`), 마우스 8/9 버튼 = 뒤로/앞으로(`signals.rs:80-97`) |
| `miro` | AGPL-3.0 | Rust/iced/MuPDF | **반면교사**: UI 스레드에서 MuPDF 호출(`src/pdf/widget.rs:1695-1728`), 화면 좌표 기반 히스토리의 한계(`src/jumplist.rs:9-17`), 링크 목적지의 페이지만 사용(`widget.rs:2222-2226`). 참고할 것: 검색 취소/디바운스(`widget.rs:1945-1972`) |
| `tdf` | AGPL-3.0 | Rust/MuPDF (터미널) | 문서 전용 렌더 스레드 + 채널(`src/renderer.rs:78-86`), 페이지마다 채널 확인해 취소(`renderer.rs:369-374`), 현재 페이지 양옆으로 번갈아 선렌더(`renderer.rs:279-300`), ✔ 부모 디렉터리 감시로 자동 리로드(`src/main.rs:220-233`) |
| `zotero-reader` | AGPL-3.0 | TypeScript/pdf.js | 링크 미리보기 팝업 지연 100ms/닫힘 500ms(`popup-delayer.js:30-60`), 같은 문서 분할 뷰(`reader.js:768-800`), 2초 내 히스토리 병합(`common/lib/history.ts:46-73`) |

Zotero 본체는 문헌 관리 도구라 PDF 뷰어 부분인 `zotero/reader` 만 클론했다.

## 엔진과 Rust 바인딩 (`references/engines/`, `references/rust-bindings/`)

| 저장소 | 라이선스 | 판단 |
|---|---|---|
| `mupdf` + `mupdf-rs` | AGPL-3.0 | **채택.** ✔ 렌더링/링크/목차/문자 좌표/검색 API 확인(`mupdf-rs/src/{document,page,text_page,link,outline,display_list}.rs`). ✔ `DisplayList` 는 `Send + Sync`(`display_list.rs:197-200`), `Document`/`Page` 는 스레드 로컬 컨텍스트에 묶임(`context.rs`) |
| `pdfium` + `pdfium-render` | BSD-3/Apache-2.0, MIT/Apache-2.0 | 대안. 기능은 충분하나 `libpdfium.so` 를 따로 구해 배치해야 하고 스레드 안전성이 전역 뮤텍스 방식 |
| `poppler` | GPL-2/3 | 대안. glib/cairo 개발 패키지가 필요(현재 미설치) |
| `hayro` | MIT/Apache-2.0 | 순수 Rust 렌더러. ✔ README 가 텍스트 선택·검색을 범위 밖으로 명시 → 지금은 부적합, 나중에 재검토 |
| `pdfjs` | Apache-2.0 | 엔진이 아니라 `web/` 뷰어 계층의 설계 참고: 목적지 종류별 스크롤(`web/pdf_viewer.js:1821-1964`), 히스토리 규칙(`web/pdf_history.js:404-455, 565-586`), 검색 정규화와 원문 위치 역매핑(`web/pdf_find_controller.js:95-366`), 현재 목차 항목(`web/pdf_outline_viewer.js:293-390`) |
| `lopdf` | MIT | PDF 객체 수준 접근이 필요해질 때만 |

## UI (`references/ui/`)

| 저장소 | 판단 |
|---|---|
| `egui` 0.37, `egui_dock` 0.22 | **채택.** ✔ 예제: `egui/examples/multiple_viewports`, `egui_dock/examples/{simple,hello}.rs`. ✔ `egui_dock` 의 탭 떼어내기는 앱 내부 플로팅 창까지만(`src/dock_state/surface.rs`) → OS 창 분리는 직접 구현. 테스트는 `egui/crates/egui_kittest` |
| `egui_tiles` | 대안 도킹 라이브러리. 3분할 이상 격자가 필요해지면 재검토 |
| `iced` | 대안. `pane_grid` 는 있으나 탭/도킹 위젯이 없음 |
| `gpui-component` | 대안. 도킹은 있으나 OS 창 분리 없음(`crates/base/src/dock/`), API 변동이 큼 |

## Diff (`references/diff/`)

| 저장소 | 방식 | 가져올 아이디어 → 볼 곳 |
|---|---|---|
| `pdf-diff-joshdata` | 텍스트(단어 박스) | **핵심 참고.** 문서 전체를 하나의 문자열로 이어 비교해 페이지 밀림에 강함(`pdf_diff/command_line.py:22-54`), diff 결과를 박스로 역매핑(`:127-184`), 줄 끝 하이픈 처리와 그 한계(`:98-118`), 양쪽 정렬용 여백 삽입(`:371-417`) |
| `diff-pdf` | 픽셀 | 겹쳐 보기 색 합성(`diff-pdf.cpp:226-310`), 변경 페이지 썸네일 거터(`gutter.cpp`), 오프셋 미세 조정(`diff-pdf.cpp:915-949`) |
| `diffpdf` | 단어/문자/픽셀 | 근접 하이라이트 병합(`renderer.cpp:286-300`), 유니코드 매핑 없는 글리프는 픽셀로 대조(`pagecompare.cpp:162-215`), 여백 제외 옵션 |
| `pdfcompare` | 픽셀 | 제외 영역 정의(`Exclusions.java:29-44`), 변경 없는 부분을 흐리게(`DiffImage.java:132-146`) |
| `pdf-diff-serhack`, `diff-pdf-visually` | 픽셀 | 참고 가치 낮음 (CI용 단순 판정기) |
| `similar` 3.2.1 | Rust 라이브러리 | **채택.** ✔ Histogram/Patience, 임의 토큰 슬라이스, 데드라인(`src/common.rs:70`), 인라인(단어 수준) 정제. ✔ 앵커 diff 와 이동 감지(`EditScript`)는 아직 미출시(`CHANGELOG.md` Unreleased) |
| `imara-diff` | Rust 라이브러리 | 더 빠른 대안. 데드라인·앵커 없음 |
| `diff-match-patch` | 알고리즘 참고 | `cleanupSemanticLossless`(편집 경계를 단어 경계로 정리) |
