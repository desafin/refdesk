# refdesk

[한국어](README.ko.md)

A Linux desktop viewer for reading and comparing large technical PDFs side by side.
Built with Rust, MuPDF and egui.

<img src="docs/images/overview.png" width="900" alt="refdesk showing two documents side by side">

## Features

- **Read across documents:** organize PDFs in tabs, splits and windows; restore your session and reading positions.
- **Follow references:** preview internal links without leaving the current page, then return to the exact spot.
- **Search everywhere:** search all open documents and step through highlighted results.
- **Compare versions:** align documents by text and review changes by section.
- **Translate as you read:** look up a word or translate a selection with a local model or a configured service.
- **Keep project notes:** manage PDFs in projects, and save highlights and bookmarks without changing the files.

The screenshots use fictional PDFs in [`assets/demo/`](assets/demo/).

## Install

Download a package from [Releases](https://github.com/desafin/refdesk/releases):

- Debian/Ubuntu: `sudo apt install ./refdesk_<version>_amd64.deb`
- Linux tarball: unpack and run `bin/refdesk`, or use the included `install.sh`.
  Release builds require glibc 2.39 or newer.

To build from source, install Rust (version pinned in `rust-toolchain.toml`), a C compiler and
libclang. On Ubuntu:

```sh
sudo apt install build-essential libclang-dev
cargo build --release -p refdesk
./target/release/refdesk file.pdf
```

Run `scripts/install.sh` to install locally; add `--set-default` to make refdesk your default PDF viewer.

## Usage

```text
refdesk [folder] [file[#page=N]]...     open a folder or files
refdesk --find text [folder] [file...]  open and search all documents
refdesk --diff old.pdf new.pdf          compare two files
refdesk --translate "text"              translate text and exit
```

If refdesk is already running, files open in its existing window.

| Shortcut | Action |
|---|---|
| `Ctrl+O` / `Ctrl+Shift+O` | Open file / folder |
| `Ctrl+P` / `Ctrl+Shift+P` | Quick open / command palette |
| `Ctrl+F` / `Ctrl+Shift+F` | Search this document / all documents |
| `Alt+←` / `Alt+→` | Back / forward |
| `Ctrl+H` / `Ctrl+D` | Highlight / bookmark |
| `F2` / `Shift+F2` | Translation mode / translate selection |
| `F7` / `Shift+F7` | Next / previous change |
| `Ctrl+\` / `Ctrl+Shift+\` | Split right / down |

Use the command palette (`Ctrl+Shift+P`) to find all commands and shortcuts.

## Translation

Translation uses Ollama by default. Install [Ollama](https://ollama.com/download), then run:

```sh
ollama pull translategemma:4b
```

Start Ollama and enable Translation Mode (`F2`). The default model and endpoint need no extra
configuration. refdesk can also use an OpenAI-compatible HTTPS service or an optional built-in
llama.cpp engine. Text is sent to the configured server; use only a service you trust.

Settings are in `~/.config/refdesk/config.toml`. Release builds include the offline engine, but not
its model weights. To install the optional TranslateGemma model, first review and accept Google's
[Gemma Terms](https://ai.google.dev/gemma/terms), then run `./install-model.sh` from the tarball or
`refdesk-install-model` after installing the `.deb`.

<details>
<summary>Advanced translation options</summary>

Set an OpenAI-compatible endpoint and model in `config.toml`:

```toml
[translate]
backend = "openai"
endpoint = "https://api.example.com"
model = "model name"
# api_key_env = "OPENAI_API_KEY" # name of an environment variable; the key is not stored
```

The built-in backend uses GGUF models in `~/.local/share/refdesk/models/`:

```toml
[translate]
backend = "embedded"
model = "translategemma-4b-it.Q4_K_M"
gpu_layers = "all" # use 0 for CPU only
```

Local builds need `--features embedded-vulkan` (GPU) or `--features embedded` (CPU).
For Vulkan builds, install `cmake libclang-dev glslc libvulkan-dev spirv-headers` first.
Set `offline_fallback = false` to disable the release build's local fallback. Translation results
are cached; project background translation stores its data in `.refdesk/translations/`.
</details>

## Settings and data

Edit `~/.config/refdesk/config.toml` for defaults such as interface language and zoom. Preferences
chosen in the menus take precedence and are saved separately. Session data, history, bookmarks and
the translation cache are stored under `~/.local/state/refdesk/`.

## Limitations

- Linux only; tested primarily on Ubuntu with GNOME on Wayland.
- File-manager drag-and-drop is unavailable on Wayland; use the Open or project import commands.
- Shortcuts cannot yet be customized.
- Highlights stay at their page positions if the PDF content changes.
- Comparison is text-based; changes inside images are not detected.
- PDF editing and printing are not supported.

## License and acknowledgements

AGPL-3.0-or-later. See [`LICENSE`](LICENSE) and
[`THIRD-PARTY-NOTICES.md`](THIRD-PARTY-NOTICES.md) for license details. The demo PDFs are fictional.

Design references and their licenses are listed in [`docs/REFERENCES.md`](docs/REFERENCES.md).
