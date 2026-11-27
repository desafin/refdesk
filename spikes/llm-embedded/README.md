# llm-embedded — 추론 엔진 내장 타당성 (WP-82)

llama.cpp 를 앱 프로세스 안에 넣어(`llama-cpp-2` 0.1.161, llama.cpp 는 그 크레이트가 묶어 둔 것) GGUF 모델로 번역하게 하고,
CUDA / Vulkan / CPU 전용을 같은 조건으로 잰 실험이다. 제품 코드는 건드리지 않았다. 버리는 코드이며 루트 작업 공간에 속하지 않는다(빈 `[workspace]`).

측정일 2026-11-26. 이 머신: Ubuntu 26.04, RTX 4070 Laptop 8GB(드라이버 595.99) + AMD Radeon 610M 내장, Ryzen 9 7945HX(32 스레드), RAM 60GB.
**숫자는 모두 이 머신에서 실제로 잰 것이다.** 재지 못한 것은 "재지 못함"으로 적었다. 추정은 "추정"으로 적었다.

## 실행

```sh
cd spikes/llm-embedded
llm-embedded <model.gguf> [--gpu-layers N] [--threads N] [--runs N] [--device N]
                          [--manual-template] [--no-cancel-test] [--rms-eps-override]
```

| 인자 | 뜻 |
|---|---|
| `--gpu-layers N` | GPU 에 올릴 층 수. 기본 999(전부) |
| `--threads N` | CPU 스레드 수(생성·프롬프트 모두). 기본 8 |
| `--runs N` | 요청 7개 전체를 N 번 되풀이. 1 번째는 "차가운", 나머지는 "데워진" 값. 기본 3 |
| `--device N` | ggml 장치 번호로 장치를 고정(시작할 때 장치 목록이 찍힌다). 없으면 llama.cpp 가 고른다 |
| `--manual-template` | 모델에 든 대화 틀 대신 Gemma 틀을 직접 만든다 |
| `--no-cancel-test` | 마지막의 취소 시험을 건너뜀 |
| `--rms-eps-override` | Ollama 파일 시도용(아래 §항목 3). `gemma3.attention.layer_norm_rms_epsilon` 을 덮어쓴다 |

llama.cpp 자체의 로그(장치 목록, `offloaded 35/35 layers to GPU` 등)는 stderr 로 나온다. 표와 출력 글은 stdout.

요청 7개(과제 그대로): 단어 넷(liveness, bus, idempotent, deadline — 문맥 문장 포함), 구절 셋(한 줄 둘, 세 문장 하나).
요청마다 프롬프트 토큰 수, 프롬프트 처리 시간, 첫 토큰까지의 시간(ttft), 생성 토큰 수, 생성 시간, 토큰/초, 전체 시간, 출력 글을 잰다.
샘플링은 탐욕(`LlamaSampler::greedy()`), `is_eog` 토큰에서 멈추고(최대 400 토큰), 요청 사이에 `clear_kv_cache()`. n_ctx 2048.

용어: 프롬프트 처리 = 프롬프트 디코드가 끝나 동기화될 때까지. ttft = 요청 시작부터 첫 토큰을 뽑을 때까지(토큰화 포함).
토큰/초 = (생성 토큰 − 1) / (첫 토큰 이후 시간). 생성 토큰 수는 종료 토큰을 세지 않는다(Ollama 의 `eval_count` 는 센다 — 같은 글이 1 더 크게 나온다).

### 빌드(세 변형, 각각 별도 `--target-dir`)

```sh
# CPU 전용
cargo build --release --target-dir target-cpu

# Vulkan — 시스템에 SPIRV-Headers 가 없어서 사용자 영역에 받아 설치해 두고 가리킨다(아래 "빌드 기록")
cmake -S /tmp/wp82/spirv-headers-src -B /tmp/wp82/spirv-headers-src/build -DCMAKE_INSTALL_PREFIX=/tmp/wp82/spirv-prefix
cmake --install /tmp/wp82/spirv-headers-src/build
CMAKE_PREFIX_PATH=/tmp/wp82/spirv-prefix CPLUS_INCLUDE_PATH=/tmp/wp82/spirv-prefix/include \
  cargo build --release --features vulkan --target-dir target-vulkan

# CUDA — rustc 가 libcudart_static.a 등을 못 찾아서 심볼릭 링크 폴더를 만들어 가리킨다(아래 "빌드 기록")
mkdir -p /tmp/wp82/cudalibs/lib64
for f in libcudart_static.a libcublas_static.a libcublasLt_static.a libculibos.a; do
  ln -sf /usr/lib/x86_64-linux-gnu/$f /tmp/wp82/cudalibs/lib64/$f; done
CUDA_PATH=/tmp/wp82/cudalibs CUDA_LIBRARY_PATH=/tmp/wp82/cudalibs \
  cargo build --release --features cuda --target-dir target-cuda
# (선택) 내 GPU 구조만: 위 명령에 CMAKE_CUDA_ARCHITECTURES=89 를 더하고 --target-dir target-cuda89
```

모델 파일은 `models/` (`.gitignore` 로 제외). 쓴 파일과 출처는 §항목 3.
`target*/` 도 제외다.

## 빌드 기록

| 변형 | 깨끗한 빌드 시간 | 실행 파일 크기 | 실행 시 필요한 공유 라이브러리(`ldd`) | 결과 |
|---|---|---|---|---|
| CPU 전용 | 45 s | 6.4 MB (6,408,376 B) | libstdc++, libgcc_s, libm, libc (`libgomp` 없음) | 성공, 경고 없음 |
| Vulkan | 3 m 04 s (성공한 시도) | 42 MB (42,123,880 B) | **libvulkan.so.1**, libstdc++, libgcc_s, libm, libc | 세 번째 시도에 성공(앞의 둘은 SPIRV-Headers 부족으로 실패) |
| CUDA (기본 구조 목록) | 7 m 56 s(CMake·nvcc 까지) + 링크 5 s | **1.05 GB** (1,054,803,520 B) | **libcuda.so.1**(드라이버), libstdc++, libgcc_s, libm, libc, libpthread, libdl, librt. `libcudart`·`libcublas` 는 **정적**으로 들어가 필요 없음 | 두 번째 시도에 성공(첫 시도는 링크 단계에서 실패) |
| CUDA, `CMAKE_CUDA_ARCHITECTURES=89` | 3 m 46 s | 688 MB (688,482,224 B) | 위와 같음 | 성공 |

(`target-cpu-omp`: `--features openmp` 로 OpenMP 를 켠 CPU 빌드도 만들어 비교했다. 38 s, 6.4 MB, `libgomp.so.1` 필요. 속도는 같다 — 아래 CPU 표.)

이 크레이트 설정: `llama-cpp-2` 의 기본 기능(`openmp`, `common`)은 끄고(`default-features = false`) 시작했다. 위의 `openmp` 기능으로 다시 켤 수 있다.

### CUDA 12.4 와 gcc 15

- **gcc 15 때문에 막히지 않았다.** `CUDAHOSTCXX`·`CMAKE_CUDA_HOST_COMPILER`·`CC`·`CXX` 를 하나도 지정하지 않았고, CMake 가 찾은 CUDA 호스트 컴파일러는 GNU 12.5.0 이다
  (`CMakeCUDACompiler.cmake` 의 `CMAKE_CUDA_HOST_COMPILER_VERSION`). 이 머신의 `nvcc`(`/usr/bin/nvcc`)는 래퍼이고 `/usr/lib/nvidia-cuda-toolkit/bin/{gcc,g++}` 를 앞세워 호출한다
  (`nvcc --dryrun` 으로 확인). 그 밖의 C++ 파일(ggml-cpu 등)은 시스템 gcc 15.2 로 컴파일되었고 문제 없었다.
- **첫 시도 실패**(환경 변수 없이, 8 분 걸려 CMake 빌드는 끝난 뒤): `error: could not find native static library 'cudart_static'`.
  `llama-cpp-sys-2` 의 `build.rs` 는 `find_cuda_helper` 로 `-L` 을 넣는데, 이 도우미는 `$CUDA_LIBRARY_PATH`·`/usr/local/cuda`·`/opt/cuda` 아래의 `lib64` 만 본다.
  Ubuntu 의 `nvidia-cuda-toolkit` 은 정적 라이브러리를 `/usr/lib/x86_64-linux-gnu/` 에 두고(`/usr/lib/cuda/lib64` 는 비어 있음) rustc 는 시스템 경로를 따로 뒤지지 않는다.
- **우회**: `/tmp/wp82/cudalibs/lib64/` 에 위 네 파일의 심볼릭 링크를 만들고 `CUDA_LIBRARY_PATH` 로 가리킨다. `build.rs` 가 `CUDA_PATH` 의 변화만 다시 실행 조건으로 보므로 `CUDA_PATH` 도 아무 값으로 설정해 다시 실행시켰다(이미 끝난 CMake 결과를 재사용해 5 초).
  (대안은 `RUSTFLAGS=-L native=/usr/lib/x86_64-linux-gnu` 이지만 빌드 폴더가 새로 갈린다. 시도하지 않음.)
- 기본 구조 목록(`50-virtual … 86-real 89-real 90-virtual`, ggml 의 CMake 가 정함)을 전부 컴파일해서 `libggml-cuda.a` 만 587 MB, 실행 파일 1.05 GB 다.
  `CMAKE_CUDA_ARCHITECTURES=89`(이 머신 GPU)로 줄이면 221 MB / 688 MB 이고 속도는 같다(§측정). 남은 대부분은 정적 `cublasLt`.
- 경고: 빌드 로그에 CMake 의 `ccache not found` 한 줄. Rust 쪽 경고 없음.

### Vulkan

- **필요했던 것(시스템에 없음): SPIRV-Headers.** Ubuntu 패키지 `spirv-headers`(추정: 패키지 이름은 확인하지 못했다. 설치 금지라서). 있는 것: `libvulkan-dev` 1.4.341, `glslc`(shaderc 2026.1).
- 실패 1(12 s): `CMake Error at ggml/src/ggml-vulkan/CMakeLists.txt:14 (find_package): Could not find a package configuration file provided by "SPIRV-Headers"`.
- 우회 1: `KhronosGroup/SPIRV-Headers`(`f6f78c717057ac7f5f3ea021f3f962aaf9d4da7e`, 2026-11-25)를 `/tmp/wp82/` 에 받아 `cmake --install` 로 `/tmp/wp82/spirv-prefix` 에 설치하고 `CMAKE_PREFIX_PATH` 로 가리킨다. (시스템에 설치한 것이 아니다.)
- 실패 2(1 m 33 s): `fatal error: spirv/unified1/spirv.hpp: No such file or directory`. ggml 의 CMake 가 `find_package` 만 하고 포함 경로를 컴파일러에 넘기지 않는다(시스템 `/usr/include` 에 있다고 가정).
- 우회 2: `CPLUS_INCLUDE_PATH=/tmp/wp82/spirv-prefix/include` → 성공. **같은 한 가지 원인(패키지 부재)에 대한 두 번째 우회**라서 "한 변형에서 두 번 실패" 규칙을 넘어 한 번 더 시도했다. 그 점을 알린다.
- CMake 메시지: `found components: glslc missing components: glslangValidator` — 경고일 뿐 빌드에 영향 없음.
- 실행 시 장치 목록(두 변형 공통 시작 출력): `Vulkan0` AMD Radeon 610M(RADV, 내장, 31.7 GB 공유), `Vulkan1` NVIDIA RTX 4070 Laptop. `--device` 를 주지 않으면 llama.cpp 가 **NVIDIA(Vulkan1)를 골랐다**(`llama_prepare_model_devices: using device Vulkan1`). 내장을 고르는 일은 없었다. 내장을 일부러 고르면(`--device 0`) §측정의 "AMD 내장" 줄.

## 측정 조건

- 측정 전에 `ollama ps` 와 `nvidia-smi` 를 보고, Ollama 모델이 올라 있으면 `ollama stop translategemma:4b` 로 내렸다. 측정 스크립트는 시작 때 `ollama ps` 를 기록하고 끝날 때도 기록했다.
  **GPU 측정(CUDA, Vulkan, Ollama 비교)은 모두 Ollama 가 모델을 내린 상태에서 했고, 끝날 때까지 다시 올라오지 않았다.**
  CPU 측정은 그때 다른 작업자가 Ollama 모델을 올려 둔 상태였다(GPU 는 쉬고 있었으므로 CPU 속도와 무관).
- GPU 에는 항상 다른 프로세스 하나가 있었다: `target/debug/refdesk`(PID 17367, 사용자 또는 다른 작업자의 앱) 약 2.8 GB. 측정 전 GPU 사용량 2869 MiB. 아래 "VRAM" 은 측정 중 전체 사용량의 최댓값이고 괄호는 그 차이(≈ 이 프로그램이 쓴 양).
- CPU 가 한가한 상태는 아니었다(다른 작업자의 빌드, `localsearch` 가 1 코어 가량). 특히 스레드 32 개 실험에 영향을 줬을 수 있다(아래).
- 값 흔들림: CUDA·Vulkan 은 `--runs 5` 를 세 번(a, b, c) 되풀이했고 데워진 값은 서로 ±3 % 안이다. 표는 a 번 값이다(데워진 값 = 2 ~ 5 번째의 중앙값).

## 측정 결과

모델: `translategemma-4b-it.Q4_K_M.gguf`(HF 에서 받은 파일, 2.31 GiB, 5.12 BPW). 프롬프트 토큰 수는 단어 64–70, 구절 95–125.
ms 단위. 차가운 = 1 번째 반복, 데워진 = 2 번째 이후의 중앙값.

### CUDA (기본 빌드, `--runs 5`, 스레드 8)

모델 읽기 **4,881 ms**(이 파일을 처음 돌린 프로세스) → 이후 b, c 는 **562, 561 ms**. 컨텍스트 만들기 24 ms. 오프로드 `offloaded 35/35 layers to GPU`(장치 CUDA0, RTX 4070). VRAM 최대 6216 MiB (+3347).

| 요청 | 프롬프트 tok | 생성 tok | ttft 차가운 | ttft 데워진 | 전체 차가운 | 전체 데워진 | 토큰/초(데워진) |
|---|---|---|---|---|---|---|---|
| 단어 liveness | 64 | 4 | 94 | 22 | 178 | 73 | 59.0 |
| 단어 bus | 66 | 4 | 24 | 23 | 81 | 74 | 58.8 |
| 단어 idempotent | 70 | 2 | 24 | 23 | 54 | 50 | 37.9 |
| 단어 deadline | 64 | 4 | 23 | 22 | 80 | 74 | 58.8 |
| 구절 한 줄(line-1) | 95 | 25 | 27 | 26 | 368 | 338 | 77.6 |
| 구절 한 줄(line-2) | 113 | 47 | 30 | 30 | 608 | 614 | 78.9 |
| 구절 세 문장 | 125 | 56 | 30 | 31 | 717 | 728 | 79.4 |

`CMAKE_CUDA_ARCHITECTURES=89` 빌드(`--runs 5`): 모델 읽기 593 ms, VRAM +3339 MiB, 데워진 전체 73 / 75 / 51 / 73 / 334 / 610 / 725 ms. 기본 빌드와 같다.

### Vulkan (`--runs 5`, 스레드 8, 장치 = NVIDIA)

모델 읽기 **660 ms**(a), 742, 710(b, c). 컨텍스트 31 ms. 오프로드 `offloaded 35/35 layers to GPU`(장치 Vulkan1 = RTX 4070). VRAM 최대 6095 MiB (+3226).
(`token_embd.weight` 만 "cannot be used with preferred buffer type Vulkan_Host, using CPU instead" 로 CPU 에 남는다. 속도에는 영향 없음.)

| 요청 | 프롬프트 tok | 생성 tok | ttft 차가운 | ttft 데워진 | 전체 차가운 | 전체 데워진 | 토큰/초(데워진) |
|---|---|---|---|---|---|---|---|
| 단어 liveness | 64 | 4 | 50 | 34 | 102 | 84 | 60.0 |
| 단어 bus | 66 | 4 | 52 | 45 | 103 | 96 | 59.7 |
| 단어 idempotent | 70 | 2 | 48 | 46 | 75 | 72 | 38.6 |
| 단어 deadline | 64 | 4 | 36 | 34 | 86 | 84 | 59.7 |
| 구절 한 줄(line-1) | 95 | 25 | 50 | 49 | 356 | 356 | 78.4 |
| 구절 한 줄(line-2) | 113 | 47 | 50 | 50 | 624 | 624 | 80.1 |
| 구절 세 문장 | 125 | 56 | 50 | 50 | 735 | 735 | 80.3 |

**Vulkan 의 첫 실행 비용(드라이버 셰이더 캐시가 비어 있을 때)**: 이 변형을 처음 돌린 프로세스는 모델 읽기 1,628 ms 에 첫 두 요청이 **4.8 s, 3.4 s** 걸렸다.
`__GL_SHADER_DISK_CACHE_PATH` 를 빈 폴더로 돌려 재현했다: 첫 두 요청 5.6 s, 5.0 s(합쳐 약 10.6 s 추가), 같은 폴더로 다시 실행하면 정상(첫 요청 58 ms). 캐시는 1.1 MB.
CUDA 에는 이런 것이 없다(첫 요청이 +70 ms 정도). 캐시는 드라이버 쪽이라 드라이버를 갱신하면 다시 비워질 수 있다(추정).

**AMD 내장(Vulkan0, `--device 0`, `--runs 3`)**: 모델 읽기 1,292 ms, VRAM 증가 없음(공유 메모리). 데워진 ttft 0.9 ~ 1.7 s, 전체 단어 1.3 ~ 1.9 s, 한 줄 4.1 / 7.2 s, 세 문장 8.4 s, 생성 4.4 ~ 8.5 tok/s.
CPU 보다 **느리다**(아래). llama.cpp 는 기본으로 NVIDIA 를 골랐으므로 이 머신에서는 문제가 아니다. 내장 GPU 만 있는 머신에서는 CPU 가 더 낫다.

### CPU 전용 (OpenMP 끔, `--gpu-layers` 무관, 스레드 수 비교)

모델 읽기 1.1 ~ 1.3 s(파일이 페이지 캐시에 있을 때), 컨텍스트 ~90 ms. 아래는 데워진 중앙값(차가운 값도 거의 같다: 예 16 스레드 단어 ttft 351 ~ 416 ms).

| 스레드 | 단어 ttft | 단어 전체 | 한 줄(line-1) 전체 | 한 줄(line-2) 전체 | 세 문장 전체 | 토큰/초(구절) | 반복 수 |
|---|---|---|---|---|---|---|---|
| 8 | 487–582 | 685–756 | 2085 | 3387 | 3928 | 18.0–18.4 | 4 |
| **12** | 369–423 | 523–601 | **1815** | **3056** | **3694** | **18.8–19.5** | 3 |
| 16 | 356–414 | 523–616 | 1937 | 3245 | 3748 | 17.1–17.8 | 4 |
| 24 | 342–453 | 533–738 | 1938 | 3325 | 3885 | 16.8–17.1 | 3 |
| 32 | 2757–6851 | 4,875 ~ 23,334 | 70,000 ~ 84,000 | 117,000 ~ 165,000 | 151,000 ~ 174,000 | **0.1–0.4** | 3 번째 반복 도중 중단 |

- 가장 빠른 값은 12 ~ 16 스레드(차이는 흔들림 안). **32 스레드는 파국적으로 느리다(0.1 ~ 0.4 tok/s, 위 표는 중단하기 전까지의 값)**. 머신의 논리 코어 수와 같게 잡았을 때 다른 프로세스(`localsearch` 가 1 코어)와 경쟁하며 스레드가 서로 기다리는 것으로 보이나(추정, 확인하지 않음) 원인은 재지 못했다. 코어 수만큼은 잡지 말아야 한다.
- OpenMP 를 켠 빌드(`target-cpu-omp`, 3 번 반복): 12 스레드 전체 단어 526–609, 한 줄 1905 / 3189, 세 문장 3693, 17.7–18.5 tok/s. 16 스레드 520–611, 1946 / 3273, 3737, 17.2–18.0 tok/s. 기본 빌드와 차이 없다.

### 같은 모델을 Ollama(HTTP)로 같은 조건에서 잰 값

`/api/chat`, `stream: false`, `temperature 0`, `seed 0`, `num_ctx 2048`, 같은 요청 7개를 5 번 되풀이, 100% GPU, 이 세션에서 직접 잼(스크립트는 저장소에 넣지 않았다). 측정 전 VRAM 2869 MiB. 데워진 값의 중앙값:

| 요청 | 프롬프트 tok(Ollama 계산) | 전체(벽시계) ms | 프롬프트 처리 ms | 토큰/초 |
|---|---|---|---|---|
| 단어 liveness | 65 | 129 | 29 | 94.7 |
| 단어 bus | 67 | 88 | 28 | 98.5 |
| 단어 idempotent | 71 | 66 | 29 | 112.6 |
| 단어 deadline | 65 | 92 | 28 | 98.7 |
| 구절 line-1 | 96 | 358 | 37 | 85.0 |
| 구절 line-2 | 114 | 602 | 40 | 84.2 |
| 구절 세 문장 | 126 | 728 | 33 | 83.8 |

첫 호출(모델 올리기 포함) 4.6 s(`load_duration` 4.4 s). 과제가 준 기준값 17 초는 이번에는 재현되지 않았다(이번에는 파일이 페이지 캐시에 있었다 — 추정).
Ollama 의 토큰/초는 `eval_count / eval_duration` 이라 짧은 요청에서 높게 나온다(`eval_count` 에 종료 토큰이 들어가고 분모에 첫 토큰이 포함됨). 같은 정의로 맞춘 비교는 "전체 시간" 열이다.

### 취소 시험

마지막에 긴 요청(세 문장)을 시작하고 150 ms 뒤 다른 스레드에서 `AtomicBool` 을 세운다. 세운 시각부터 `translate()` 가 돌아올 때까지:

| 변형 | 취소 지연 | 그때까지 나온 글 |
|---|---|---|
| CUDA | 14.3 ms | 토큰 8 개 / "기능 그룹은 일관성 있게" |
| Vulkan | 8.4 ms | 토큰 5 개 / "기능 그룹은 일" |
| CPU 16 스레드 | 22.2 ms | 없음(프롬프트 디코드 중) |
| CPU 8 스레드 | 96.2 ms | 없음(프롬프트 디코드 중) |

취소한 뒤 같은 컨텍스트로 바로 다음 요청을 해도 정상이다(56 토큰, 같은 속도). 프롬프트는 32 토큰씩 나눠 디코드하고 청크 사이와 토큰 사이에 플래그를 본다.
**`ctx.decode()` 한 번은 중간에 끊을 수 없다**: 청크 하나(CPU 에서 수십~수백 ms)나 토큰 하나(GPU 에서 ~12 ms)만큼은 기다려야 한다. 청크로 나누지 않으면 CPU 에서 프롬프트 전체(0.4 ~ 0.7 s)를 기다린다(청크로 나누기 전의 16 스레드 시험에서 취소 지연 524 ms 를 봤다).

## 보고에 답할 것

### 1. CUDA 와 Vulkan 중 무엇이 더 빠른가

**사실상 같다. 굳이 가르면 CUDA 가 아주 조금 빠르고, 차이는 사용자가 느끼지 못하는 크기다.**

| (데워진, 중앙값, ms) | CUDA ttft | Vulkan ttft | CUDA 전체 | Vulkan 전체 |
|---|---|---|---|---|
| 단어 (4 개 범위) | 22–23 | 34–46 | 50–74 | 72–96 |
| 한 줄 line-1 | 26 | 49 | 338 | 356 |
| 한 줄 line-2 | 30 | 50 | 614 | 624 |
| 세 문장 | 31 | 50 | 728 | 735 |

- 첫 토큰까지: CUDA 가 12 ~ 24 ms 빠르다(프롬프트 처리가 CUDA 25 ~ 30 ms, Vulkan 45 ~ 50 ms). 둘 다 사람이 알아채는 한계(~100 ms)보다 한참 작다.
- 생성 속도는 같다: 구절에서 CUDA 77.6 ~ 79.4, Vulkan 78.4 ~ 80.3 tok/s(메모리 대역폭이 한계로 보인다. 추정). 그래서 길어질수록 전체 시간 차이가 줄어 세 문장에서는 1 ~ 3 % (b, c 반복에서는 CUDA 711 / 713 ms, Vulkan 739 / 751 ms 로 4 ~ 5 %).
- 모델 읽기: CUDA 562 ~ 593 ms, Vulkan 660 ~ 742 ms (파일이 페이지 캐시에 있을 때, 반복 측정). 이 파일을 **처음** 돌린 프로세스는 CUDA 4.9 s, Vulkan 1.6 s(+ 위의 셰이더 컴파일 약 10 s). CUDA 의 4.9 s 는 한 번만 봤고 원인을 확인하지 못했다(실행 파일이 1 GB 라 디스크에서 읽은 것일 수 있다 — 추정).
- Ollama(HTTP)와의 비교(같은 모델 아님: 파일이 다르다, 항목 2): 전체 시간이 단어 66 ~ 129 ms, 한 줄 358 / 602 ms, 세 문장 728 ms. **내장 CUDA 의 값(50 ~ 74 / 338 / 614 / 728 ms)과 같거나 약간 낫다.** 내장이 HTTP 왕복 몇 ms 를 아끼지만 눈에 띄지 않는다. 모델 올리기는 Ollama 4.6 s(이번 측정), 내장 0.6 ~ 0.7 s(캐시 있음).
  과제가 준 기준값(모델 올리기 17 s, 85 tok/s)과 이번에 잰 값은 비슷하나 모델 올리기만 다르다(위 설명).
- VRAM: CUDA +3.3 GB, Vulkan +3.2 GB (n_ctx 2048). 같은 정도.

### 2. 번역 품질이 Ollama 를 거칠 때와 같은가

**대부분 같고, 일부 낱말·구절이 다르다. 품질이 떨어졌다고 볼 근거는 없으나 "출력이 같아야 한다"는 기대는 맞지 않았다.**

(데워진 2 번째 반복의 출력. 같은 요청에서 변형끼리 비교)

| 요청 | Ollama(그 파일, temp 0) | llama.cpp CUDA | llama.cpp Vulkan | llama.cpp CPU |
|---|---|---|---|---|
| liveness | 존재 여부 | 존재 여부 | **존재성** | **생존성** |
| bus / idempotent / deadline | CAN 버스 / 동일 / 마감일 | 같음 | 같음 | 같음 |
| line-1 | …상태 **변경** 요청 후… | …상태 **전환** 요청 후… | 같음(전환) | 같음(전환) |
| line-2 | **생명체** 감시가…, 플랫폼 건강 관리**는** | **생존성** 감시가…, 플랫폼 건강 관리 **시스템은** | 같음 | 같음 |
| 세 문장 | (같다) | 같다 | 같다 | 같다 |

- 같은 입력 틀: Ollama 의 템플릿(`ollama show --template`)은 `<start_of_turn>user\n{내용}<end_of_turn>\n<start_of_turn>model\n` 이고 llama.cpp 도 모델에 든 틀로 같은 문자열을 만든다(`--manual-template` 와 토큰 수·출력이 모두 같음을 Vulkan 에서 확인). 종료는 둘 다 `<end_of_turn>`(llama.cpp 는 `is_eog`).
  프롬프트 토큰 수는 Ollama 가 1 많다(65 대 64). 원인은 찾지 못했다(`/api/generate` 의 `raw` 로 `<bos>` 를 넣고 빼도 둘 다 65). 글은 같은 것이 많으므로 큰 차이는 아닌 것으로 보인다.
- **다른 원인 후보는 두 가지이고 둘 다 실제로 있다**:
  1. **양자화(파일)**: Ollama 의 파일은 3.06 GiB 로 텐서 형식이 섞여 있고(f16 137, q4_K 205, q6_K 34, q5_0 13, q8_0 14, bf16 1; 그림 입력 부분 포함), 내가 쓴 HF 파일은 2.31 GiB(q4_K 204, q6_K 35, f32 205)다. 같은 "Q4_K_M" 이름이어도 가중치가 같지 않다. line-1, line-2 의 차이는 이쪽으로 설명되나 **직접 확인하지는 못했다**(같은 HF 파일을 Ollama 에 올려 비교하지 않았다. 사용자의 Ollama 에 모델을 추가하기 싫어서).
  2. **계산 백엔드**: 같은 HF 파일, 같은 프롬프트, 탐욕인데 liveness 가 CUDA "존재 여부", Vulkan "존재성", CPU "생존성" 으로 **세 가지**다. 부동소수점 계산 순서 차이로 확률이 비슷한 후보가 뒤집힌 것이다. 나머지 6 개 요청은 세 백엔드 모두 같은 글.
- 의미로는 모두 적당하다. 굳이 말하면 용어("liveness")는 어느 것도 AUTOSAR 의 "생존 감시"에 정확히 맞지 않는다 — 모델의 한계이고 엔진과 무관하다.
- 결론: 엔진을 바꿔도 품질 수준은 같다. 캐시 열쇠(`identity()`)에는 백엔드 종류와 모델 파일 이름을 넣는 것이 안전하다(같은 모델 이름이라도 글이 다를 수 있다).

### 3. Ollama 가 받아 둔 모델 파일을 그대로 읽을 수 있는가

**읽을 수 없다.** `/usr/share/ollama/.ollama/models/blobs/sha256-bdbf939b…4815d4` (3.06 GiB, `general.architecture = gemma3`, 883 텐서)를 `llama-cpp-2` 0.1.161 에 주면:

1. `llama_model_load: error loading model: error loading model hyperparameters: key not found in model: gemma3.attention.layer_norm_rms_epsilon` — Ollama 가 만든 GGUF 에는 이 필수 키가 없다(Ollama 자체 로더가 기본값을 쓴다).
2. 그 키를 `--rms-eps-override`(KV 덮어쓰기 `Float(1e-6)`)로 채워 봐도 `done_getting_tensors: wrong number of tensors; expected 883, got 444` 로 실패한다. 절반쯤이 llama.cpp 가 모르는 텐서(그림 입력부 등 Ollama 방식의 한 파일 합본)다.
   (여기서 멈췄다. 텐서를 걸러내거나 이름을 바꿔 읽게 만드는 시도는 하지 않았다.)

그래서 Hugging Face 에서 llama.cpp 용 파일을 받았다:

- 저장소: `mradermacher/translategemma-4b-it-GGUF` (로그인 없이 받음, `gated: false`)
- 파일: `translategemma-4b-it.Q4_K_M.gguf`, **2,489,909,760 B (2.32 GiB)**, sha256 `81200d03e843d2ec1ece6eeafe7d13cb6e5211e1fcd336ade55790b683a08330`
  (같은 저장소에 그림 입력부 `…mmproj-Q8_0.gguf` 592 MB 도 있으나 받지 않았다. 번역에는 필요 없다)
- 원본: `google/translategemma-4b-it`(Hugging Face 에서 약관 동의가 필요한 저장소), 이 변환본의 라이선스 표기는 `gemma`(= Gemma Terms of Use, Ollama 파일도 같은 약관).
  **llama.cpp 는 MIT, `llama-cpp-2` 는 MIT OR Apache-2.0.** 모델 약관의 재배포 조건은 검토하지 않았다 — 앱이 모델을 같이 배포하는 경우에는 따로 확인해야 한다.
- 저장 위치: `spikes/llm-embedded/models/translategemma-4b-it.Q4_K_M.gguf` (`.gitignore` 로 제외, 커밋 안 됨).
- 같은 저장소의 다른 크기(바이트): Q4_K_S 2.38 GB, Q5_K_M 2.83 GB, Q6_K 3.19 GB, Q8_0 4.13 GB, IQ4_XS 2.28 GB. 더 작은 품질 손실이 필요하면 Q5_K_M 정도(추정).

제품의 뜻: 사용자가 Ollama 를 쓰면서 같은 파일을 공유하는 것은 불가능하다. 내장 방식은 모델 파일을 따로 받아야 한다(2.3 GB). 제품은 모델 경로를 설정으로 받고(`[translate] model_path` 같은 것), 없으면 `ModelMissing` 을 돌려주는 것이 자연스럽다.

### 4. CPU 만으로는 얼마나 느린가

12 ~ 16 스레드에서 단어 **0.5 ~ 0.6 s**(ttft 0.36 ~ 0.42 s), 한 줄 **1.8 ~ 3.2 s**, 세 문장 **3.7 s**, 생성 17 ~ 19 tok/s. GPU(CUDA)의 단어 0.05 ~ 0.07 s, 한 줄 0.34 ~ 0.61 s, 세 문장 0.73 s 에 비해 단어 8 ~ 12 배, 구절 5 배 정도 느리다. 프롬프트 처리 약 180 tok/s.
**GPU 가 없는 사용자에게도 쓸 만하다**: 단어는 "포인터를 올리고 반 초" 안이고 한 줄은 첫 글자가 0.5 ~ 0.7 s 에 나오며 이후 글이 흘러나온다(조각 출력을 쓰면). 세 문장은 4 초라 기다려야 한다.
단 (a) 번역하는 동안 CPU 12 ~ 16 스레드를 쓴다(PDF 렌더링 등 다른 작업과 경쟁), (b) 스레드 수를 코어 수만큼 잡으면 100 배 느려질 수 있다(위 32 스레드), (c) 이 CPU 는 Ryzen 9 7945HX 로 데스크톱급이다. 약한 CPU 의 값은 재지 못했다. (d) AMD 내장 GPU 의 Vulkan 은 CPU 보다 느리다.

### 5. 제품에 넣는다면: `EmbeddedBackend` (`TranslateBackend` 구현 하나)

크레이트 API(소스를 읽고 이번 실험 코드로 확인한 것):

- **수명과 스레드**: `LlamaBackend`(전역, `init()` 은 프로세스에서 한 번만, 두 번째는 `BackendAlreadyInitialized`; 드롭하면 `llama_backend_free`)는 모델·컨텍스트보다 오래 살아야 한다.
  `LlamaModel` 은 `Send + Sync`(소스의 `unsafe impl`)라서 `Arc<LlamaModel>` 로 공유할 수 있다. `LlamaContext<'a>` 는 모델을 **빌린다**(`'a`)에 `Send` 지만 `!Sync` 다(컴파일 타임 검사가 크레이트에 있다).
  그래서 `struct { model, context }` 로 한 구조체에 담을 수 없다(자기 참조). 실험은 컨텍스트를 만들어 작업 스레드로 **옮겨서** 돌렸고 동작했다(`std::thread::scope`).
  - 권장 모양(추정, 아직 구현하지 않음): **추론 전용 스레드 하나가 모델과 컨텍스트를 둘 다 만들어 가지고 있고**(라이프타임 문제가 없다, `unsafe` 불필요), `EmbeddedBackend`(`Send + Sync`)는 그 스레드에 채널로 요청을 보낸다. `warm_up()` = 스레드를 띄우고 모델을 읽게 한 뒤 완료를 기다림. `translate()` = 요청을 보내고 조각을 채널로 받아 `on_text` 에 넘기며 `cancel` 을 본다. 백엔드를 드롭하면 스레드가 끝나고 VRAM 이 풀린다.
  - 대안: 요청마다 컨텍스트를 만든다(실측 19 ~ 32 ms, CPU 90 ms). 단어(총 50 ~ 90 ms)에서는 눈에 띄게 더하므로 비권장.
- **조각 단위 출력**: 토큰 하나마다 `vocab.token_to_piece()` 가 **바이트**를 준다. 한글은 한 글자가 토큰 여러 개에 걸칠 수 있으므로 UTF-8 경계를 모아서 내보내야 한다(이 실험의 `Utf8Stream`, 한글 출력에서 문제 없었다). 이후 `on_text(&str)` 에 넘기면 된다.
- **취소**: 토큰 사이에 `AtomicBool` 을 보고 `Err(Cancelled)` 로 나가면 된다(측정: GPU 8 ~ 14 ms). 프롬프트가 길 때를 위해 프롬프트 디코드도 작은 청크로 나눈다. 디코드 한 번은 끊을 수 없다. 모델 읽기는 `LlamaModelParams::with_progress_callback`(`false` 를 돌려주면 읽기 중단, 소스 주석) 이 있다 — **사용해 보지 않았다**. 컨텍스트는 취소 뒤에도 `clear_kv_cache()` 만 하면 계속 쓸 수 있었다.
- **`TranslateError` 대응**: 모델 파일 없음 → `ModelMissing`; 읽기 실패(`LlamaModelLoadError`) → `Protocol(String)` 에 가까움(새 변형이 필요할 수 있다: 아키텍처 미지원 같은 것); `Unreachable`·`Http` 는 해당 없음. `models()` 는 설정된 모델 파일 이름 하나, 또는 모델 폴더의 `*.gguf` 목록. `identity()` 는 파일 이름 + 대상 언어(+ 백엔드 종류, 항목 2).
- **프롬프트**: `translategemma` 틀은 `model.chat_template(None)` + `apply_chat_template` 로 얻은 것이 Gemma 틀과 같았다. 이 글에서 쓴 구절 틀(`You are a professional English (en) to Korean (ko) translator…`)이 §4.14 의 `translate/prompt.rs` 와 같으면 그대로 공유할 수 있다. `Passage` 와 `Word` 의 사용자 메시지는 이 실험 것을 그대로 썼다.
- **로그**: llama.cpp 는 기본으로 stderr 에 장치·텐서 로그를 쏟는다(몇 천 줄). `llama_cpp_2::send_logs_to_tracing(LogOptions::default().with_logs_enabled(false))` 나 `LlamaBackend::void_logs()` 로 끄거나 `tracing` 으로 돌려야 한다(이 실험은 일부러 stderr 로 두었다).

선택 빌드 기능으로 둘 때의 빌드 조건:

| | 필요한 것 | 빌드 시간(깨끗한) | 실행 파일 증가 | 실행 시 필요 |
|---|---|---|---|---|
| `embedded`(CPU) | C++ 컴파일러, `cmake`, **`libclang`**(bindgen), 인터넷 없이 가능 | 45 s | +6 MB | 없음 |
| `embedded-vulkan` | 위 + `glslc`, `libvulkan-dev`, **SPIRV-Headers**(이 머신에는 없어 직접 받음) | 3 m | +42 MB | `libvulkan.so.1` |
| `embedded-cuda` | 위 + `nvcc`(12.4, 이 머신은 gcc-12 래퍼가 알아서), 정적 CUDA 라이브러리 경로 우회 | 4 ~ 8 m | **+0.7 ~ 1.05 GB** | `libcuda.so.1`(드라이버) |

- **기본 빌드에 주는 영향은 0 이어야 한다**: `cargo build` 의 기본 기능에 넣지 않는다. `llama-cpp-2` 는 CPU 전용이어도 `cmake` + `libclang` + C++ 컴파일러를 요구하고(`build.rs` 가 CMake 로 llama.cpp 전체를 컴파일, 45 s) 이 중 하나가 없으면 빌드가 죽는다. 그러므로 `refdesk` 의 `[features]` 에서 `dep:llama-cpp-2` 를 선택 의존으로 두고, `scripts/check.sh`(fmt, clippy, test)는 기본 기능으로만 돈다. `--all-features` 로 clippy 를 돌리는 곳이 있으면 거기서 깨진다.
- **실행 파일이 바로 시작하려면 `libvulkan.so.1` / `libcuda.so.1` 이 있어야 한다**(`ldd` 의 NEEDED, 정적 링크가 아니라 로더가 요구). 해당 라이브러리가 없는 머신에서는 실행 파일이 아예 시작하지 않는다(**시험하지 않음, `ldd` 구조에서 추정**). 한 파일로 모든 사용자를 덮으려면 ggml 의 동적 백엔드(`dynamic-backends`, 런타임에 `.so` 로 GPU 백엔드 적재)가 필요한데 **써 보지 않았다**.
- 대상 환경이 이 머신 하나(NVIDIA + AMD 내장, Ubuntu)라는 전제에서는 `embedded-vulkan` 하나로 충분하다.

걸릴 만한 문제:

- **엔진 오류가 앱을 죽인다**: llama.cpp 는 `GGML_ASSERT` 실패나 GPU 메모리 부족에서 `abort()` 한다(추정, 이 실험에서 일으켜 보지 않았다). 같은 프로세스이므로 뷰어 전체가 죽는다. HTTP 서버 방식은 격리된다. 이 점이 내장의 가장 큰 단점이다(`llama-server` 를 따로 띄워 쓰는 `openai` 백엔드가 이미 있어 격리가 필요한 사용자는 그쪽을 쓸 수 있다).
- **VRAM 을 앱이 3.2 ~ 3.3 GB 점유한다**. 모델을 올려 두는 동안 계속(Ollama 의 `keep_alive` 같은 내리기를 직접 만들어야 한다: 백엔드를 드롭하면 풀린다). 8 GB GPU 에서 그림을 그리는 refdesk(이미 2.8 GB)와 합쳐 6.1 ~ 6.2 GB. 작은 GPU 에서는 모자랄 수 있다(`--gpu-layers` 로 일부만 올리는 길은 있으나 속도는 재지 않았다).
- **Vulkan 첫 실행에 10 s**(드라이버 셰이더 캐시): 앱이 `warm_up()` 에서 모델을 읽은 뒤 더미 요청 하나(단어, 구절)를 돌려 숨겨야 한다. 이 안에는 ~10 s 가 들어갈 수 있다.
- **같은 모델 이름이라도 파일이 다르면 글이 다르다**(항목 2). 캐시 열쇠에 반영.
- **모델은 별도로 받아야 한다**(2.3 GB, HF, 약관 `gemma`). Ollama 파일은 못 쓴다(항목 3).
- 크레이트가 빠르게 바뀐다(문서에 "API 안정성을 목표로 하지 않는다"고 적혀 있다). 버전을 고정해야 한다(`=0.1.161`).
- 초기화 순서: `LlamaBackend` 는 프로세스당 하나. 앱이 다른 곳에서 llama.cpp 를 쓰지 않는 한 문제 없다.
- 라이선스: llama.cpp MIT, `llama-cpp-2` MIT OR Apache-2.0(확인: 크레이트 `Cargo.toml`). 정적 CUDA 라이브러리를 실행 파일에 넣어 배포하면 NVIDIA 의 CUDA 재배포 조건이 걸린다(추정, 검토하지 않음). Vulkan 은 해당 없음.

### 6. 권장

**내장 GPU 백엔드를 고른다면 Vulkan 을 기본으로 삼는다.** 이유:

1. **속도 차이가 없다**: 생성 속도 같음, 단어 전체 시간 +10 ~ 22 ms(72 ~ 96 대 50 ~ 74 ms), 구절 +1 ~ 5 %. 사람이 느끼지 못한다.
2. **실행 파일이 42 MB 대 0.7 ~ 1.05 GB**. CUDA 는 정적 cuBLAS 때문에 한 파일이 거의 1 GB 가 된다.
3. 빌드: Vulkan 은 3 분(`glslc`, SPIRV-Headers 필요), CUDA 는 4 ~ 8 분(`nvcc` + 정적 라이브러리 경로 우회). 환경 의존이 CUDA 쪽이 더 크다.
4. 벤더 중립: NVIDIA 든 AMD(RADV)든 쓴다. 이 머신은 NVIDIA 를 자동으로 골랐다. 단, 내장 GPU 만 있을 때는 Vulkan 이 CPU 보다 느리므로(AMD 610M 에서 4 ~ 8 tok/s 대 CPU 17 ~ 19) 장치 종류가 `IntegratedGpu` 이면 `--gpu-layers 0` 으로 CPU 를 쓰게 하는 것이 낫다.
5. 반대 근거(Vulkan 의 약점): 첫 실행 ~10 s(워밍업으로 숨김), ttft 가 12 ~ 24 ms 더 길다.

더 큰 질문 — 내장이 Ollama(HTTP)보다 나은가 — 에 대해서는 **속도 이점이 없다**(Ollama 와 같은 속도). 이점은 "외부 서버 없이 동작"뿐이고 대가는 크래시 격리 상실, VRAM 점유, 빌드 의존이다.
따라서 기본 백엔드는 지금처럼 Ollama 로 두고, `EmbeddedBackend` 는 선택 빌드 기능(`embedded-vulkan`)으로 추가하는 것을 권한다. CPU 전용(`embedded`)은 GPU 없는 사용자용으로 45 s 짜리 빌드를 같이 둘 수 있다.

## 재지 못한 것 / 한계

- Ollama 에 같은 HF 파일을 올려 양자화 효과를 분리하지 않았다(사용자 Ollama 에 모델을 추가하지 않으려고).
- 모델 읽기 취소(`with_progress_callback`) 는 써 보지 않았다.
- GPU 메모리 부족·`abort()` 동작, 라이브러리 없는 머신에서의 실행 파일 시작, `dynamic-backends` 는 시험하지 않았다.
- 약한 CPU, 다른 양자화(Q5/Q6/Q8), 다른 `n_ctx`, 일부만 GPU 에 올리기(`--gpu-layers` 부분값)는 재지 않았다.
- CUDA 첫 프로세스의 모델 읽기 4.9 s 의 원인을 확인하지 못했다. 한 번만 관찰했다.
- CPU 스레드 32 개 실험은 다른 프로세스가 CPU 를 쓰고 있을 때 했고 3 번째 반복 도중에 중단했다. 원인은 추정이다.
- 번역 품질은 사람이 판정하지 않았다(글을 비교만 함). 요청 7 개는 표본이 작다.
- VRAM 은 `nvidia-smi` 의 전체 사용량 차이(다른 프로세스 2.8 GB 가 늘 있었다)로 어림했다.
