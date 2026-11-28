# refdesk

[English](README.md)

사양서, 레퍼런스 매뉴얼, 표준 문서처럼 큰 기술 PDF 여러 개를 나란히 읽고 비교하는 Linux 데스크톱 뷰어.
Rust, MuPDF, egui로 만들었다.

<img src="docs/images/overview.png" width="900" alt="문서 두 개를 나란히 띄운 refdesk">

## 주요 기능

- **여러 문서 읽기:** 탭·분할·여러 창으로 정리하고 읽던 상태를 복원한다.
- **참조 따라가기:** 페이지를 떠나지 않고 내부 링크를 미리 보고, 원래 위치로 돌아온다.
- **통합 검색:** 열린 모든 문서를 검색하고 결과를 차례로 확인한다.
- **버전 비교:** 글을 기준으로 문서를 정렬해 절별 변경 사항을 보여 준다.
- **읽으며 번역:** 로컬 모델이나 설정한 서비스로 단어와 선택 영역을 번역한다.
- **프로젝트와 표시:** PDF를 프로젝트별로 관리하고, 파일을 수정하지 않고 하이라이트와 책갈피를 저장한다.

스크린샷에는 [`assets/demo/`](assets/demo/)의 가상 PDF를 사용했다.

## 설치

[Releases](https://github.com/desafin/refdesk/releases)에서 패키지를 받는다.

- Debian/Ubuntu: `sudo apt install ./refdesk_<version>_amd64.deb`
- Linux tarball: 압축을 풀고 `bin/refdesk`를 실행하거나 포함된 `install.sh`를 실행한다.
  릴리스 빌드는 glibc 2.39 이상이 필요하다.

소스에서 빌드하려면 `rust-toolchain.toml`에 고정된 Rust 버전, C 컴파일러와 libclang이 필요하다.
Ubuntu에서는 다음을 설치한다.

```sh
sudo apt install build-essential libclang-dev
cargo build --release -p refdesk
./target/release/refdesk file.pdf
```

`scripts/install.sh`로 설치할 수 있으며, `--set-default` 옵션은 기본 PDF 뷰어로 설정한다.

## 사용법

```text
refdesk [folder] [file[#page=N]]...     폴더 또는 파일 열기
refdesk --find text [folder] [file...]  모든 문서를 열고 검색
refdesk --diff old.pdf new.pdf          두 파일 비교
refdesk --translate "text"              번역하고 종료
```

refdesk가 이미 실행 중이면 파일은 기존 창에서 열린다.

| 단축키 | 기능 |
|---|---|
| `Ctrl+O` / `Ctrl+Shift+O` | 파일 / 폴더 열기 |
| `Ctrl+P` / `Ctrl+Shift+P` | 빠른 열기 / 명령 팔레트 |
| `Ctrl+F` / `Ctrl+Shift+F` | 현재 문서 / 모든 문서 검색 |
| `Alt+←` / `Alt+→` | 뒤로 / 앞으로 |
| `Ctrl+H` / `Ctrl+D` | 하이라이트 / 책갈피 |
| `F2` / `Shift+F2` | 번역 모드 / 선택 영역 번역 |
| `F7` / `Shift+F7` | 다음 / 이전 변경 |
| `Ctrl+\` / `Ctrl+Shift+\` | 오른쪽 / 아래로 분할 |

모든 명령과 단축키는 명령 팔레트(`Ctrl+Shift+P`)에서 찾을 수 있다.

## 번역

기본 번역 엔진은 Ollama다. [Ollama](https://ollama.com/download)를 설치한 뒤 모델을 받는다.

```sh
ollama pull translategemma:4b
```

Ollama를 실행하고 번역 모드(`F2`)를 켠다. 기본 모델과 주소는 이미 설정되어 있다. OpenAI 호환 HTTPS
서비스와 선택형 llama.cpp 내장 엔진도 지원한다. 번역할 글은 설정한 서버로 전송되므로 신뢰하는 서비스만 사용한다.

설정 파일은 `~/.config/refdesk/config.toml`이다. 릴리스에는 오프라인 엔진이 포함되지만 모델 가중치는 없다.
선택형 TranslateGemma 모델을 설치하려면 먼저 Google의
[Gemma 약관](https://ai.google.dev/gemma/terms)을 확인하고 동의한 뒤, tarball에서는 `./install-model.sh`를,
`.deb` 설치 후에는 `refdesk-install-model`을 실행한다.

<details>
<summary>번역 고급 설정</summary>

OpenAI 호환 서버의 주소와 모델을 `config.toml`에 지정한다.

```toml
[translate]
backend = "openai"
endpoint = "https://api.example.com"
model = "model name"
# api_key_env = "OPENAI_API_KEY" # 키를 저장하지 않고 환경 변수에서 읽는다
```

내장 엔진은 `~/.local/share/refdesk/models/`의 GGUF 모델을 사용한다.

```toml
[translate]
backend = "embedded"
model = "translategemma-4b-it.Q4_K_M"
gpu_layers = "all" # CPU만 사용하려면 0
```

직접 빌드할 때 GPU는 `--features embedded-vulkan`, CPU는 `--features embedded`를 사용한다.
Vulkan 빌드에는 `cmake libclang-dev glslc libvulkan-dev spirv-headers` 패키지가 필요하다.
릴리스의 로컬 대체 모델을 끄려면 `offline_fallback = false`로 설정한다. 번역 결과는 캐시되며,
프로젝트 백그라운드 번역 데이터는 `.refdesk/translations/`에 저장된다.
</details>

## 설정과 데이터

`~/.config/refdesk/config.toml`에서 화면 언어와 기본 확대율 등을 설정한다. 메뉴에서 고른 값은 별도로 저장되며
설정 파일보다 우선한다. 세션, 기록, 책갈피와 번역 캐시는 `~/.local/state/refdesk/`에 저장된다.

## 알려진 한계

- Linux 전용이며 Ubuntu의 GNOME Wayland에서 주로 확인했다.
- Wayland에서는 파일 관리자에서 끌어다 놓을 수 없다. 열기 메뉴나 프로젝트 가져오기를 사용한다.
- 단축키 변경은 지원하지 않는다.
- PDF 내용이 바뀌어도 하이라이트는 기존 쪽 위치에 남는다.
- 비교는 텍스트 기반이라 그림 안의 변경은 찾지 못한다.
- PDF 편집과 인쇄는 지원하지 않는다.

## 라이선스와 참고 자료

AGPL-3.0-or-later. 자세한 내용은 [`LICENSE`](LICENSE)와
[`THIRD-PARTY-NOTICES.md`](THIRD-PARTY-NOTICES.md)를 참고한다. 시연 PDF는 가상 문서다.

설계 참고 프로젝트와 라이선스는 [`docs/REFERENCES.md`](docs/REFERENCES.md)에 정리했다.
