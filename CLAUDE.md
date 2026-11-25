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

### 앱을 확인용으로 띄울 때

앱은 세션과 문서 기록을 `~/.local/state/refdesk/` 에 저장하고, 실행 중인 인스턴스가 있으면 거기로 파일을 넘긴다.
확인하려고 띄울 때는 사용자의 실제 상태를 건드리지 않게 임시 폴더와 `--no-single-instance` 를 쓴다.

```sh
X=$(mktemp -d); XDG_STATE_HOME=$X/state XDG_CONFIG_HOME=$X/config \
  ./target/debug/refdesk --no-single-instance 파일.pdf
```

- `target/debug/refdesk` 는 `cargo build -p refdesk` 를 한 직후에만 실행한다. worktree 는 빌드 폴더를 공유하지 않는다(`docs/PLAN.md` §2). 병합 뒤 빌드가 이상하면 `cargo clean -p refdesk -p refdesk-core`.
- 이 머신(GNOME Wayland)에서 `orca computer` 는 화면 캡처, 키 입력, 스크롤을 지원하지 않는다. 되는 것은 접근성 트리 읽기(`get-app-state`)와 요소 클릭(`click --element-index`)이다. 메뉴 막대, 상태 표시줄, 탭, 탐색기의 파일 줄, 목차의 줄은 트리에 보이고 누를 수 있다. 쪽 그림과 팝업 메뉴는 보이지 않는다.
- 프로세스를 끝낼 때 `pkill -f <패턴>` 을 쓰면 그 명령을 실행하는 셸까지 죽는다. `pgrep -x refdesk` 로 찾고 PID 로 끝낸다.

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
