# refdesk

소프트웨어 기술문서 PDF 를 여러 개 띄워 놓고 읽는 Linux 데스크톱 뷰어.

## 현재 상태

개발 초기다. 작업 공간 뼈대만 있고, 지금 실행하면 제목이 "refdesk" 인 빈 창이 뜬다.

## 대상 환경

Ubuntu 26.04, GNOME, Wayland 한 종류만 지원한다. 다른 OS, 배포판, X11 은 고려하지 않는다.

## 빌드와 실행

Rust 1.95 가 필요하다(`rust-toolchain.toml` 이 지정한다). 시스템 개발 패키지는 따로 필요하지 않다.

```sh
cargo build -p refdesk        # 빌드
cargo run -p refdesk          # 실행(창이 뜬다)
scripts/check.sh              # fmt, clippy, test
```

## 문서

| 파일 | 내용 |
|---|---|
| `docs/SPEC.md` | 무엇을 만드는가. 요구사항과 우선순위 |
| `docs/ARCHITECTURE.md` | 구조, 모듈 간 계약, 불변 규칙 |
| `docs/PLAN.md` | 작업 패키지와 완료 조건 |
| `docs/REFERENCES.md` | 참고한 소스에서 볼 곳 |

## 라이선스

AGPL-3.0-or-later. PDF 엔진으로 쓰는 MuPDF 가 AGPL-3.0 이라 이 프로젝트도 같은 라이선스를 따른다.
전문은 `LICENSE` 에 있다.

## 참고한 프로젝트

아래 프로젝트의 설계와 구현 방식을 참고했다. 그 소스는 이 저장소에 포함하지 않는다
(`scripts/clone-references.sh` 로 로컬에 따로 받아 읽는다). 자세한 내용은 `docs/REFERENCES.md` 에 있다.

뷰어

- sioyek — GPL-3.0
- okular — GPL-2.0+
- qpdfview — GPL-2.0+
- zathura, zathura-pdf-mupdf — zlib
- papers — GPL-2.0+
- miro — AGPL-3.0
- tdf — AGPL-3.0
- zotero-reader — AGPL-3.0

엔진과 바인딩

- mupdf, mupdf-rs — AGPL-3.0 (채택)
- pdfium, pdfium-render — BSD-3/Apache-2.0, MIT/Apache-2.0
- poppler — GPL-2/3
- hayro — MIT/Apache-2.0
- pdfjs — Apache-2.0
- lopdf — MIT

UI

- egui — MIT OR Apache-2.0 (채택)
- egui_dock — MIT (채택)
- egui_tiles — MIT OR Apache-2.0 (대안)
- iced — MIT (대안)
- gpui-component — Apache-2.0 (대안)

Diff

- pdf-diff-joshdata — CC0-1.0
- diff-pdf — GPL-2.0
- diffpdf — GPL-2.0+
- pdfcompare — Apache-2.0
- pdf-diff-serhack — MIT
- diff-pdf-visually — MIT OR Apache-2.0
- similar — Apache-2.0 (채택)
- imara-diff — Apache-2.0
- diff-match-patch — Apache-2.0
