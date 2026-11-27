# 릴리스 내기

refdesk 의 릴리스는 GitHub Actions 가 만든다. 태그 `v<버전>` 을 올리면 `.github/workflows/release.yml` 이
빌드하고, 묶음을 GitHub Releases 에 올린다. 워크플로는 아래의 스크립트를 부르기만 하므로, 로컬과 컨테이너에서
같은 스크립트를 돌려 보는 것이 곧 워크플로의 확인이다.

## 1. 받는 파일과 지원 범위

| 파일 | 내용 |
|---|---|
| `refdesk-<버전>-x86_64-linux.tar.gz` | `refdesk-<버전>/` 아래 `bin/refdesk`(strip), `share/applications/refdesk.desktop`, `share/icons/hicolor/scalable/apps/refdesk.svg`, `install.sh`, `uninstall.sh`, `LICENSE`, `README.md`, `THIRD-PARTY-NOTICES.md` |
| `refdesk_<버전>_amd64.deb` | `/usr/bin/refdesk`, `.desktop`, 아이콘, `/usr/share/doc/refdesk/`(copyright, 고지, changelog) |
| `SHA256SUMS` | 위 두 파일의 SHA-256 |

- **Linux x86_64 만.** 릴리스는 GitHub 의 `ubuntu-24.04` 러너에서 빌드하므로 glibc 2.39 이상, 즉
  **Ubuntu 24.04 이상**(그와 같거나 새 glibc 의 배포판)에서 돈다. 개발 머신(Ubuntu 26.04, glibc 2.43)에서
  빌드한 실행 파일은 24.04 에서 돌지 않는다 — 그래서 릴리스는 반드시 러너나 `ubuntu:24.04` 컨테이너에서 만든다.
- 기본 기능만 넣는다. 내장 번역 엔진(`--features embedded-vulkan`)은 릴리스 묶음에 들어 있지 않다.
- 대상 데스크톱은 GNOME Wayland 다(`docs/SPEC.md` §2). X11 에서도 뜨지만 확인하지 않는다.

### 실행 파일이 쓰는 라이브러리

실행 파일이 직접 링크하는 것은 `libc`, `libm`, `libgcc_s` 뿐이다(MuPDF 와 FreeType 은 소스째 들어 있다).
나머지는 실행 중에 `dlopen` 으로 연다. 그래서 `refdesk --version`·`--help` 는 아무것도 없는 컨테이너에서도
돌지만, 창을 띄우려면 아래가 있어야 한다. `.deb` 는 이것을 `Depends`/`Recommends`/`Suggests` 로 적는다
(`scripts/package.sh`).

| 구분 | 패키지 | 쓰는 곳 |
|---|---|---|
| Depends | `libc6 (>= 빌드한 glibc 의 기호 버전)`, `libgcc-s1` | 직접 링크 |
| Depends | `libfontconfig1` | 시스템 글꼴 찾기(mupdf 의 font-kit, eframe 의 글꼴 대체). 없으면 panic |
| Depends | `libxkbcommon0`, `libwayland-client0` | winit 의 Wayland 창 |
| Depends | `libvulkan1 \| libegl1` | wgpu 가 그리는 데(Vulkan, 없으면 GL) |
| Recommends | `libwayland-egl1`, `mesa-vulkan-drivers \| libegl-mesa0` | GL 경로, GPU 드라이버 |
| Recommends | `libdbus-1-3`, `xdg-desktop-portal`, `xdg-desktop-portal-gnome \| xdg-desktop-portal-gtk` | 파일 선택기(rfd 가 libdbus 로 포털을 부른다) |
| Recommends | `fonts-noto-cjk`, `xdg-utils` | 한글, 웹 링크 열기 |
| Suggests | `libx11-6`, `libx11-xcb1`, `libxcb1`, `libxcursor1`, `libxi6`, `libxkbcommon-x11-0` | Wayland 가 없을 때의 X11 창 |

GNOME 데스크톱이 깔린 Ubuntu 에는 모두 이미 있다.

## 2. 순서

1. **버전 올리기**: 작업 공간 `Cargo.toml` 의 `[workspace.package] version` 을 고친다(두 크레이트가 따른다).
   `cargo check` 로 `Cargo.lock` 을 갱신한다.
2. **CHANGELOG**: `CHANGELOG.md` 에 `## [<버전>] - YYYY-MM-DD` 절을 쓴다. 아직 날짜가 없는 동안은
   `Unreleased` 로 둔다 — 태그로 돌린 워크플로는 날짜가 없으면 멈춘다. 절의 본문이 릴리스 본문이 된다.
   맨 아래의 비교 링크(`[<버전>]: …/releases/tag/v<버전>`)도 더한다.
3. **고지 갱신**: 의존성이 바뀌었으면 `scripts/third-party-notices.sh` 로 `THIRD-PARTY-NOTICES.md` 를 다시 만든다.
   CI 가 `--check` 로 확인한다.
4. **게이트와 컨테이너 확인**: `scripts/check.sh`, 그리고 `scripts/verify-in-container.sh`(§4).
5. **커밋, 태그, push**:

   ```sh
   git commit -am "Release 0.1.0"
   git tag -a v0.1.0 -m "refdesk 0.1.0"
   git push origin main v0.1.0
   ```

6. **워크플로 확인**: GitHub 의 Actions 탭에서 "Release" 가 `build` → `publish` 까지 통과했는지, Releases 에
   세 파일이 붙었는지 본다. 버전에 `-` 가 들어 있으면(`0.2.0-rc.1`) 시험판(prerelease)으로 올라간다.

태그와 `Cargo.toml` 의 버전이 다르거나, CHANGELOG 에 그 버전의 절이 없거나 날짜가 없으면 `build` 가 처음에 멈춘다.
잘못 올린 태그는 지우고(`git push origin :refs/tags/v0.1.0`, 릴리스가 생겼으면 그것도) 고친 커밋에 다시 붙인다.

### 손으로 돌리기

Actions 탭의 "Release" → "Run workflow"(`workflow_dispatch`)는 같은 빌드를 하되 릴리스를 만들지 않고, 묶음을
워크플로 산출물(`refdesk-<버전>`, `dist/` 와 `notes/`)로만 남긴다. 날짜 없는 CHANGELOG 도 받는다.

## 3. 로컬에서 묶음 만들기

```sh
scripts/package.sh                       # 기본 기능
scripts/package.sh --features embedded   # 기능을 넣어 보기(릴리스에는 쓰지 않는다)
```

`dist/` 에 세 파일이 생긴다(`dist/` 는 매번 지우고 새로 만든다, git 은 무시한다). 버전은 `packaging/version.sh` 가
`Cargo.toml` 에서 읽는다. 파일 시각은 `SOURCE_DATE_EPOCH`(기본: 마지막 커밋 시각)로 맞추고 묶음 안의 순서를
정렬하므로, 같은 실행 파일이면 같은 묶음이 나온다. 이 머신에서 만든 묶음은 glibc 2.43 을 요구하므로 남에게 줄
수 없다 — 배포용은 컨테이너나 러너에서 만든다.

받은 묶음 설치:

```sh
tar -xzf refdesk-0.1.0-x86_64-linux.tar.gz
refdesk-0.1.0/install.sh                     # ~/.local 에
refdesk-0.1.0/install.sh --prefix /opt/refdesk   # 다른 곳에(쓰기 권한이 있어야 한다)
refdesk-0.1.0/uninstall.sh [--prefix …]
sudo apt install ./refdesk_0.1.0_amd64.deb   # 또는 .deb
```

`install.sh` 는 소스 트리(`scripts/install.sh`, 빌드부터 한다)와 묶음(들어 있는 실행 파일을 쓴다) 양쪽에서 동작한다.
설치한 `bin` 이 `PATH` 에 없으면 `.desktop` 의 `Exec` 를 그 실행 파일의 절대 경로로 바꿔 쓴다.

## 4. 깨끗한 컨테이너에서 확인

```sh
scripts/verify-in-container.sh                 # 빌드부터(오래 걸린다)
scripts/verify-in-container.sh --install-only  # target/container/dist/ 의 묶음으로 설치 확인만
```

docker 가 있어야 한다. 하는 일:

1. `ubuntu:24.04` 컨테이너에 저장소를 복사한다(git 이 아는 파일만 — `target/`, `dist/`, `references/` 는 넣지 않는다).
   `packaging/setup-build-env.sh` 로 `packaging/build-deps.txt` 의 apt 패키지와 `rust-toolchain.toml` 의 툴체인을
   깔고, `scripts/check.sh`, `scripts/package.sh` 를 돈다. 묶음은 `target/container/dist/` 로 꺼낸다.
2. 또 다른 깨끗한 컨테이너에서 `apt-get install ./refdesk_….deb` 후 `refdesk --version`, `--help`, 그리고 제거.
3. 또 다른 깨끗한 컨테이너(패키지 추가 없음)에서 tar.gz 를 풀어 `install.sh --prefix /opt/refdesk` 와 기본
   `~/.local` 로 깔고 같은 확인, `uninstall.sh`.

CI 와 릴리스 워크플로도 1번과 같은 `packaging/setup-build-env.sh` → `scripts/check.sh` → `scripts/package.sh` 를
부른다. 빌드에 필요한 apt 패키지를 바꿀 때는 `packaging/build-deps.txt` 만 고치고 이 스크립트로 확인한다.
테스트는 창을 띄우지 않으므로 디스플레이 없는 컨테이너와 러너에서 돈다.

컨테이너는 끝나면 지운다. 받은 이미지는 남는다(`docker image rm ubuntu:24.04` 로 지운다).

## 5. 워크플로

| 파일 | 언제 | 하는 일 |
|---|---|---|
| `.github/workflows/ci.yml` | `main` 에 push, PR | `setup-build-env.sh` → `check.sh` → `third-party-notices.sh --check`. cargo 캐시는 `Swatinem/rust-cache` |
| `.github/workflows/release.yml` | 태그 `v*`, 손으로 | 버전·태그·CHANGELOG 확인 → `check.sh` → 고지 확인 → `package.sh` → 산출물 → (태그일 때만) `gh release create` |

권한은 최소다: 워크플로 기본은 `contents: read`, 릴리스를 만드는 `publish` 작업만 `contents: write`(`GITHUB_TOKEN`).
릴리스 빌드는 캐시를 쓰지 않고 처음부터 빌드한다. 액션은 주 버전 태그로 고정한다(`actions/checkout@v7`,
`actions/upload-artifact@v7`, `actions/download-artifact@v8`, `Swatinem/rust-cache@v2`).

GitHub 러너는 깨끗한 이미지가 아니어서 `pkg-config` 와 FreeType 개발 패키지가 있으면 `freetype-sys` 가 시스템
FreeType 에 링크한다. `scripts/package.sh` 는 `FREETYPE2_NO_PKG_CONFIG=1` 로 이를 막아, 러너에서 만든 실행 파일도 컨테이너에서
만든 것처럼 libc 와 libgcc 에만 링크되게 한다. 링크된 라이브러리는 `package.sh` 가 출력하고 `Depends` 에 반영한다.
