#!/usr/bin/env bash
# Downloads the optional local translation model after the user accepts its terms.
set -euo pipefail

readonly filename="translategemma-4b-it.Q4_K_M.gguf"
readonly revision="d37868f759d2f817a43d28dd80cf9f982ffa62fd"
readonly sha256="81200d03e843d2ec1ece6eeafe7d13cb6e5211e1fcd336ade55790b683a08330"
readonly url="https://huggingface.co/mradermacher/translategemma-4b-it-GGUF/resolve/$revision/$filename?download=true"
data_home="${XDG_DATA_HOME:-$HOME/.local/share}"
model_dir="$data_home/refdesk/models"
model_path="$model_dir/$filename"

if [ "${1:-}" = --help ] || [ "${1:-}" = -h ]; then
    echo "Downloads $filename (about 2.5 GB) to $model_dir after Gemma Terms acceptance."
    echo "Usage: $0"
    exit 0
fi
if [ "$#" -ne 0 ]; then
    echo "install-model.sh: unknown argument: $1" >&2
    exit 2
fi
for tool in curl sha256sum; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "install-model.sh: $tool is required" >&2
        exit 1
    fi
done

if [ -f "$model_path" ]; then
    if printf '%s  %s\n' "$sha256" "$model_path" | sha256sum --check --status; then
        echo "The local translation model is already installed: $model_path"
        exit 0
    fi
    echo "install-model.sh: an unverified file already exists at $model_path; move it before retrying" >&2
    exit 1
fi

cat <<EOF
This downloads a third-party quantization of Google's TranslateGemma model:
  Model: $filename (about 2.5 GB)
  Source: https://huggingface.co/mradermacher/translategemma-4b-it-GGUF
  Terms:  https://ai.google.dev/gemma/terms

The model is not included with refdesk. Review the Gemma Terms before accepting.
EOF
printf 'Type AGREE to accept the Gemma Terms and download the model: '
IFS= read -r answer
if [ "$answer" != AGREE ]; then
    echo "Download cancelled."
    exit 0
fi

mkdir -p "$model_dir"
tmp="$(mktemp "$model_dir/.$filename.XXXXXX.part")"
trap 'rm -f -- "$tmp"' EXIT

echo "Downloading to $model_dir (this may take a while)..."
curl --fail --location --retry 3 --progress-bar --output "$tmp" "$url"
if ! printf '%s  %s\n' "$sha256" "$tmp" | sha256sum --check --status; then
    echo "install-model.sh: SHA-256 verification failed; the incomplete file will be removed" >&2
    exit 1
fi
chmod 644 "$tmp"
if [ -e "$model_path" ]; then
    echo "install-model.sh: $model_path appeared during download; keeping the existing file" >&2
    exit 1
fi
mv -- "$tmp" "$model_path"
echo "Installed and verified: $model_path"
