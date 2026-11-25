#!/usr/bin/env bash
# 참고용 오픈소스 프로젝트를 references/ 아래에 얕게(shallow) 클론한다.
# references/ 는 .gitignore 대상이며 이 저장소에 커밋되지 않는다.
# 사용법: scripts/clone-references.sh [병렬 개수=6]
set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEST="$ROOT/references"
JOBS="${1:-6}"
export DEST
export GIT_TERMINAL_PROMPT=0   # 없는 저장소에서 인증 프롬프트로 멈추지 않게

# <대상 디렉터리> <URL>
read -r -d '' REPOS <<'EOF'
viewers/sioyek            https://github.com/ahrm/sioyek
viewers/okular            https://invent.kde.org/graphics/okular.git
viewers/zathura           https://github.com/pwmt/zathura
viewers/zathura-pdf-mupdf https://github.com/pwmt/zathura-pdf-mupdf
viewers/qpdfview          https://git.launchpad.net/qpdfview
viewers/papers            https://gitlab.gnome.org/GNOME/papers.git
viewers/zotero-reader     https://github.com/zotero/reader
viewers/miro              https://github.com/vincent-uden/miro
viewers/tdf               https://github.com/itsjunetime/tdf
engines/mupdf             https://github.com/ArtifexSoftware/mupdf
engines/poppler           https://gitlab.freedesktop.org/poppler/poppler.git
engines/pdfjs             https://github.com/mozilla/pdf.js
engines/pdfium            https://pdfium.googlesource.com/pdfium
engines/hayro             https://github.com/LaurenzV/hayro
rust-bindings/mupdf-rs      https://github.com/messense/mupdf-rs
rust-bindings/pdfium-render https://github.com/ajrcarey/pdfium-render
rust-bindings/lopdf         https://github.com/J-F-Liu/lopdf
ui/egui                   https://github.com/emilk/egui
ui/egui_dock              https://github.com/Adanos020/egui_dock
ui/egui_tiles             https://github.com/rerun-io/egui_tiles
ui/iced                   https://github.com/iced-rs/iced
ui/gpui-component         https://github.com/longbridge/gpui-component
diff/diff-pdf             https://github.com/vslavik/diff-pdf
diff/pdf-diff-joshdata    https://github.com/JoshData/pdf-diff
diff/pdf-diff-serhack     https://github.com/serhack/pdf-diff
diff/diffpdf              https://salsa.debian.org/debian/diffpdf.git
diff/pdfcompare           https://github.com/red6/pdfcompare
diff/diff-pdf-visually    https://github.com/bgeron/diff-pdf-visually
diff/similar              https://github.com/mitsuhiko/similar
diff/imara-diff           https://github.com/pascalkuthe/imara-diff
diff/diff-match-patch     https://github.com/google/diff-match-patch
EOF

clone_one() {
  local dir="$1" url="$2" target="$DEST/$1"
  if [ -d "$target/.git" ]; then
    echo "SKIP  $dir (이미 있음)"
    return 0
  fi
  mkdir -p "$(dirname "$target")"
  if git clone --quiet --depth 1 --single-branch --no-tags "$url" "$target" 2>"$target.err"; then
    rm -f "$target.err"
    echo "OK    $dir ($(du -sh "$target" | cut -f1))"
  else
    echo "FAIL  $dir <- $url :: $(tail -1 "$target.err" 2>/dev/null)"
    rm -rf "$target" "$target.err"
    return 0
  fi
}
export -f clone_one

mkdir -p "$DEST"
printf '%s\n' "$REPOS" | grep -v '^\s*$' \
  | xargs -P "$JOBS" -L 1 bash -c 'clone_one "$0" "$1"'
echo "--- 완료: $(du -sh "$DEST" | cut -f1) ---"
