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

## 프로젝트

PDF 를 프로젝트 단위로 모아 둔다. 프로젝트는 `refdesk-project.toml` 이 있는 폴더이고, 가져온 PDF 는 그 안에 **복사**된다(원본은 그대로).

- **만들기**: 파일 ▸ 새 프로젝트… — 이름을 치면 위치가 `~/Documents/refdesk/이름` 으로 따라온다. PDF 가 든 기존 폴더를 고르면 그 PDF 들이 프로젝트의 문서가 된다.
- **열기**: 파일 ▸ 프로젝트 열기…, 시작 화면의 "최근 프로젝트", 또는 프로젝트 폴더를 "폴더 열기"·명령줄(`refdesk ~/Documents/refdesk/이름`)로 연다. 다시 시작하면 마지막 프로젝트가 열린다.
- **가져오기**: 파일 ▸ PDF 가져오기…(Ctrl+Shift+I, 여러 개를 한 번에 고를 수 있다), 파일 ▸ 폴더에서 가져오기…(폴더 아래 PDF 를 목록에서 골라 가져온다), 탐색기 머리글의 `+`, 탐색기 폴더의 우클릭 "여기로 가져오기…", 파일 관리자에서 복사한 파일을 Ctrl+V. 같은 내용의 파일은 건너뛰고, 이름만 같으면 교체 / 둘 다 보관 / 건너뛰기를 묻는다. 가져오는 동안 상태 표시줄에 진행이 보이고 취소할 수 있다.
- **정리**: 탐색기 우클릭의 새 폴더, 이름 바꾸기, 프로젝트에서 지우기. 지운 것은 프로젝트 안 `.refdesk/trash/` 로 옮겨지고 파일 ▸ 프로젝트 휴지통 보기에서 되살린다.

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

### 서버 없이 쓰기(내장 엔진)

Ollama 같은 서버 없이 앱이 llama.cpp 로 GGUF 모델 파일을 직접 읽어 번역한다(`backend = "embedded"`).
선택 빌드 기능이라 기본 빌드(`cargo build`)에는 들어 있지 않다. 기능 없이 빌드한 실행 파일에서 `embedded` 를 고르면
"이 실행 파일에는 내장 번역 엔진이 들어 있지 않습니다" 가 뜬다.

필요한 패키지(빌드할 때): `cmake`, `libclang-dev`, C++ 컴파일러, 그리고 GPU(Vulkan)용으로 `glslc`, `libvulkan-dev`, `spirv-headers`.

```sh
sudo apt install cmake libclang-dev glslc libvulkan-dev spirv-headers
cargo build --release -p refdesk --features embedded-vulkan   # GPU(Vulkan). 실행 파일 +40MB 정도, libvulkan.so.1 필요
cargo build --release -p refdesk --features embedded          # CPU 만. 실행 파일 +6MB 정도
```

`embedded-cuda` 도 있으나 정적 cuBLAS 때문에 실행 파일이 0.7 ~ 1GB 가 된다. 속도는 Vulkan 과 같으므로(이 머신에서 잰 값) Vulkan 을 권한다.

모델 파일: Ollama 가 받아 둔 모델 파일은 llama.cpp 가 읽지 못하므로 따로 받는다. 예를 들어 Hugging Face 의
[`mradermacher/translategemma-4b-it-GGUF`](https://huggingface.co/mradermacher/translategemma-4b-it-GGUF) 의
`translategemma-4b-it.Q4_K_M.gguf`(2.5GB, 약관 Gemma Terms of Use). 받은 파일은 모델 폴더
`~/.local/share/refdesk/models/`(`$XDG_DATA_HOME/refdesk/models`)에 둔다. **보기 ▸ 번역 모델** 에 그 폴더의 `.gguf` 파일이 뜬다.

```toml
[translate]
backend = "embedded"
# models_dir = "/다른/폴더"           # 기본은 ~/.local/share/refdesk/models
# model = "translategemma-4b-it.Q4_K_M" # 폴더 안의 파일 이름(.gguf 생략 가능) 또는 절대 경로.
#                                       # 폴더에 없는 이름(기본값 translategemma:4b 등)이면 이름순 첫 .gguf 를 쓴다
gpu_layers = "all"                    # GPU 에 올릴 층 수. 0 이면 CPU 만
threads = 0                           # CPU 스레드. 0 = 논리 코어의 절반(최대 16)
context_tokens = 4096                 # 프롬프트와 답을 합친 길이(토큰)
keep_alive_minutes = 30               # 마지막 요청 뒤 이 시간이 지나면 모델을 내린다
```

이 머신(RTX 4070 Laptop)에서 모델 읽기 0.7초, 한 줄 번역 0.35 ~ 0.6초로 Ollama 와 같다. CPU 만으로는 한 줄에 2 ~ 3초.
드라이버의 셰이더 캐시가 비어 있으면(처음 실행, 드라이버 갱신 뒤) Vulkan 은 첫 번역 전에 5 ~ 10초를 더 쓴다 —
번역 모드를 켤 때 모델 준비에서 치른다. 내장 GPU 만 있으면 `"all"` 이어도 CPU 로 돈다(그쪽이 빠르다).

알려진 점:

- 모델을 올려 둔 동안 앱이 GPU 메모리를 직접 잡는다(이 모델은 약 3.3GB). 번역 모드를 끄거나 다른 모델을 고르면,
  또는 `keep_alive_minutes` 동안 요청이 없으면 내려 놓는다.
- 엔진이 같은 프로세스 안에서 돈다. llama.cpp 가 죽으면(GPU 메모리 부족 등) 앱도 같이 죽는다.
  격리가 필요하면 `ollama` 나 `openai`(`llama-server`) 백엔드를 쓴다.

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
