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

## 번역 (로컬 AI 모델)

영어 글을 이 머신에서 도는 모델에 물어 한국어로 본다. 문서의 글은 설정한 서버 밖으로 나가지 않는다.

- **번역 모드**(F2, 보기 ▸ 번역 모드, 상태 표시줄의 "번역" 단추): 켜 두면 단어 위에 포인터를 0.5초 올렸을 때
  그 단어의 뜻과 그 단어가 든 문장의 번역이 팝업으로 뜨고, 글을 끌어 선택하고 놓으면 선택한 글의 번역이 뜬다.
  켠 상태는 다시 시작해도 기억한다. 켤 때 모델을 미리 올린다(상태 표시줄에 "모델 준비 중…").
- **선택한 글 번역**(Shift+F2, 편집 메뉴, 우클릭 메뉴 "번역"): 번역 모드가 꺼져 있어도 지금 선택을 한 번 번역한다.
- 팝업: 포인터가 단어와 팝업을 벗어나거나 스크롤·클릭·키를 누르면 닫힌다(팝업 위로 옮기면 남는다).
  선택 번역의 팝업은 선택이 풀리거나 Esc, "닫기" 까지 남는다. "복사" 는 번역을 클립보드에 넣는다.
  2,000자가 넘는 선택은 앞부분만 번역한다.
- 같은 글은 다시 묻지 않는다(`~/.local/state/refdesk/translations.json`, 최근 5,000개).
- 명령줄에서 설정과 모델을 점검한다: `refdesk --translate "The watchdog supervises each process."`

### 서버와 모델

설정은 `~/.config/refdesk/config.toml` 의 `[translate]` 다. 주소는 `http://` 만 받는다(로컬 서버용).
**다른 머신의 주소를 넣으면 번역할 글이 그 머신으로 간다.**

Ollama(기본):

```toml
[translate]
backend = "ollama"                  # 기본값
endpoint = "http://127.0.0.1:11434" # 기본값
model = "translategemma:4b"         # 기본값. 먼저 `ollama pull translategemma:4b`
target = "ko"
keep_alive_minutes = 30             # 마지막 요청 뒤 모델을 메모리에 두는 시간(Ollama 만)
word_with_sentence = true           # 단어 팝업에 문장 번역도 보인다
```

OpenAI 호환 서버(llama.cpp 의 `llama-server`, LM Studio, vLLM 등):

```sh
llama-server -m 모델.gguf --port 8080
```

```toml
[translate]
backend = "openai"
endpoint = "http://127.0.0.1:8080"  # openai 의 기본값
model = "모델 이름"                   # 서버의 /v1/models 에 나오는 이름
```

모델 바꾸기: **보기 ▸ 번역 모델** 에 서버가 가진 모델이 뜬다. 고르면 바로 바뀌고 다시 시작해도 기억한다
(`~/.local/state/refdesk/prefs.json`). "설정 파일의 모델로" 는 앱에서 고른 것을 지우고 `config.toml` 의 `model` 로 돌아간다.
모델이 바뀌면 예전 모델의 번역 기억은 쓰지 않는다.

서버에 닿지 않거나 모델이 없으면 상태 표시줄에 "사용 불가" 가 뜨고, 그 위에 포인터를 올리면 이유와 해결 방법
(`ollama pull 모델` 등)이 보인다.

### 지시문 직접 정하기

모델 이름이 `translategemma` 로 시작하면 그 모델의 정해진 틀을, 아니면 일반 지시문(식별자·숫자·단위는 그대로 두라는)을 쓴다.
다른 모델에 맞춰 바꾸려면:

```toml
[translate]
passage_prompt = "Translate into {target}. Keep identifiers as they are. Output only the translation.\n\n{text}"
word_prompt = "Sentence: {sentence}\n\nWhat does '{word}' mean here? Answer with one or two {target} words."
```

자리 표시: `{text}`(구절), `{word}`·`{sentence}`(단어와 그 문장), `{target}`(대상 언어 이름, 예: Korean).
구절 틀에 `{text}` 가, 단어 틀에 `{word}` 가 없으면 경고를 남기고 내장 지시문을 쓴다.

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
