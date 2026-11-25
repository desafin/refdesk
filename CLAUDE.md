# refdesk

소프트웨어 기술문서 PDF 를 여러 개 띄워 놓고 읽는 Linux 데스크톱 뷰어. Rust + MuPDF + egui.
대상 환경은 이 로컬 Ubuntu 머신 하나다(GNOME, Wayland). 다른 OS·배포판은 고려하지 않는다.

## 문서

| 파일 | 내용 |
|---|---|
| `docs/SPEC.md` | 무엇을 만드는가. 요구사항 ID(OPEN-1, LINK-3 …)와 우선순위 |
| `docs/ARCHITECTURE.md` | 구조, 모듈 간 계약, 불변 규칙 R-1 ~ R-10 |
| `docs/PLAN.md` | 작업 패키지(WP), 위임 규칙, 완료 조건 |
| `docs/REFERENCES.md` | `references/` 에 클론한 참고 소스에서 볼 곳 |

코드가 계약과 달라져야 하면 `docs/ARCHITECTURE.md` 를 먼저 고친다.

## 명령

- `scripts/check.sh` — 완료 게이트(fmt, clippy, test). 작업을 끝냈다고 말하기 전에 통과시킨다.
- `cargo run -p refdesk -- 파일.pdf` — 실행. **창이 사용자 화면에 뜬다.** 자동 검증에는 쓰지 않는다.
- `scripts/clone-references.sh` — `references/` 를 다시 받는다.

## 반드시 지킬 것

- `docs/ARCHITECTURE.md` §1 의 규칙. 자주 걸리는 것:
  UI 스레드에서 MuPDF 를 부르거나 블록하지 않는다 / `mupdf` 는 `engine/backend.rs` 에서만 /
  `egui_dock` 은 `windows/dock.rs` 에서만 / 위치는 페이지 공간의 `Location` 으로만 저장.
- egui·eframe·egui_dock·mupdf 의 API 를 기억으로 쓰지 않는다. `docs/ARCHITECTURE.md` §2.2 가 가리키는
  고정 버전 소스와 `spikes/feasibility/` 의 컴파일 확인된 예를 읽는다.
- WP 를 맡았으면 `docs/PLAN.md` 에 적힌 소유 파일만 고친다.
- 테스트는 창을 띄우지 않고, 네트워크와 실제 홈 디렉터리를 건드리지 않는다.
- 요청받지 않은 커밋과 push 를 하지 않는다.

## 표기

- `docs/` 와 사용자와의 대화는 한국어. 애플리케이션의 window 는 "창"이라고 쓴다("윈도우"는 Windows OS 로 읽힌다).
- 식별자, 코드 주석, 커밋 메시지는 영어. 화면에 보이는 글자는 `i18n` 을 거친다(한국어, 영어).
