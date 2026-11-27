#!/usr/bin/env bash
# Takes the README screenshots (docs/images/*.png) from the real app on a virtual X display.
#
#   scripts/screenshots.sh [scene ...]     # all scenes, or only the named ones
#
# Nothing appears on your screen and your own refdesk state is not touched:
# - the app runs on its own Xvfb display, forced to X11 (WAYLAND_DISPLAY is removed);
# - it runs inside `dbus-run-session`, so file dialogs, notifications and the like cannot reach
#   your desktop session;
# - HOME and every XDG folder point into /tmp/refdesk-demo, which is wiped first. The demo
#   documents are copied from assets/demo/ into /tmp/refdesk-demo/specs, so only neutral paths
#   show up in the pictures.
#
# Needs: Xvfb, xdotool, xwd, dbus-run-session, python3 with PIL, a Vulkan driver that can render
# without a GPU (Mesa lavapipe). The translation scenes need Ollama on 127.0.0.1:11434 with the
# model translategemma:4b; they are skipped otherwise.
#
# The app is driven with clicks at fixed positions in a fixed-size window (1440 x 900 logical
# pixels at scale 2). If the layout of the app changes, the positions below need updating;
# look at the pictures after every run.

set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
HELPERS=$ROOT/scripts/screenshots
OUT=$ROOT/docs/images
DEMO=/tmp/refdesk-demo
SCALE=2
WIN_W=1440
WIN_H=900
COLORS=256
BIN=$ROOT/target/release/refdesk

for tool in Xvfb xdotool xwd dbus-run-session python3; do
  command -v "$tool" >/dev/null || { echo "screenshots.sh: $tool not found" >&2; exit 1; }
done
python3 -c 'import PIL' 2>/dev/null || { echo "screenshots.sh: python3 PIL not found" >&2; exit 1; }
LVP=$(ls /usr/share/vulkan/icd.d/lvp_icd*.json 2>/dev/null | head -1 || true)

(cd "$ROOT" && cargo build --release -p refdesk)

# --- virtual display ------------------------------------------------------------------------

DISPLAY_NUM=
for n in $(seq 90 120); do
  if [ ! -e /tmp/.X11-unix/X$n ] && [ ! -e /tmp/.X$n-lock ]; then DISPLAY_NUM=$n; break; fi
done
[ -n "$DISPLAY_NUM" ] || { echo "screenshots.sh: no free display number" >&2; exit 1; }

XVFB_PID=
APP_PID=
cleanup() {
  stop_app
  if [ -n "$XVFB_PID" ]; then kill "$XVFB_PID" 2>/dev/null || true; wait "$XVFB_PID" 2>/dev/null || true; fi
}
trap cleanup EXIT

Xvfb ":$DISPLAY_NUM" -screen 0 "$((WIN_W * SCALE))x$((WIN_H * SCALE))x24" -nolisten tcp \
  >/dev/null 2>&1 &
XVFB_PID=$!
export DISPLAY=":$DISPLAY_NUM"
unset WAYLAND_DISPLAY
for _ in $(seq 50); do xdotool getdisplaygeometry >/dev/null 2>&1 && break; sleep 0.1; done
xdotool getdisplaygeometry >/dev/null

# --- demo folder ----------------------------------------------------------------------------

reset_demo() {
  rm -rf "$DEMO"
  mkdir -p "$DEMO/specs" "$DEMO/.config/refdesk" "$DEMO/.local/state" "$DEMO/.local/share" \
    "$DEMO/.cache" "$DEMO/.run"
  chmod 700 "$DEMO/.run"
  cp "$ROOT"/assets/demo/*.pdf "$DEMO/specs/"
  printf 'language = "en"\n' >"$DEMO/.config/refdesk/config.toml"
}

# --- the app --------------------------------------------------------------------------------

WIN=
start_app() {
  stop_app
  (
    cd "$DEMO"
    exec env -i PATH="$PATH" LANG=en_US.UTF-8 DISPLAY="$DISPLAY" HOME="$DEMO" USER=demo \
      XDG_CONFIG_HOME="$DEMO/.config" XDG_STATE_HOME="$DEMO/.local/state" \
      XDG_DATA_HOME="$DEMO/.local/share" XDG_CACHE_HOME="$DEMO/.cache" \
      XDG_RUNTIME_DIR="$DEMO/.run" WINIT_UNIX_BACKEND=x11 WINIT_X11_SCALE_FACTOR=$SCALE \
      ${LVP:+VK_ICD_FILENAMES=$LVP} \
      dbus-run-session -- "$BIN" --no-single-instance --no-restore "$@"
  ) >"$DEMO/.run/app.log" 2>&1 &
  WIN=$(xdotool search --sync --onlyvisible --name 'refdesk' | head -1)
  APP_PID=$(xdotool getwindowpid "$WIN")
  xdotool windowmove "$WIN" 0 0 windowsize "$WIN" $((WIN_W * SCALE)) $((WIN_H * SCALE))
  xdotool windowfocus "$WIN"
  stable 30 2
}

stop_app() {
  if [ -n "$APP_PID" ]; then
    kill "$APP_PID" 2>/dev/null || true
    for _ in $(seq 50); do kill -0 "$APP_PID" 2>/dev/null || break; sleep 0.1; done
    APP_PID=
  fi
}

# --- input, in logical pixels ---------------------------------------------------------------

focus() { xdotool windowfocus "$WIN"; }
move() { xdotool mousemove $(($1 * SCALE)) $(($2 * SCALE)); }
click() { move "$1" "$2"; sleep 0.15; xdotool click "${3:-1}"; sleep 0.3; }
dclick() { move "$1" "$2"; sleep 0.15; xdotool click --repeat 2 --delay 80 1; sleep 0.3; }
key() { focus; xdotool key --clearmodifiers "$@"; sleep 0.3; }
typetext() { focus; xdotool type --delay 30 "$1"; sleep 0.3; }
# drag X0 Y0 X1 Y1: press, move in steps, release.
drag() {
  move "$1" "$2"
  sleep 0.2
  xdotool mousedown 1
  sleep 0.1
  local i
  for i in 1 2 3 4 5 6 7 8; do
    move $(($1 + ($3 - $1) * i / 8)) $(($2 + ($4 - $2) * i / 8))
    sleep 0.04
  done
  sleep 0.1
  xdotool mouseup 1
  sleep 0.3
}
park() { move 1439 899; }  # keep the pointer from hovering anything
# stable TIMEOUT [MIN]: wait until the screen stops changing.
stable() { python3 "$HELPERS/xshot.py" stable --timeout "$1" --min "${2:-0}" || true; }
shot() {
  stable 20 0.5
  python3 "$HELPERS/xshot.py" save "$OUT/$1.png" --colors $COLORS ${2:+--crop "$2"}
  echo "  $1.png  $(($(stat -c %s "$OUT/$1.png") / 1024)) KiB"
}

PM10=$DEMO/specs/process-manager-v1.0.pdf
PM11=$DEMO/specs/process-manager-v1.1.pdf
WDG=$DEMO/specs/watchdog-interface-v1.0.pdf

# --- scenes ---------------------------------------------------------------------------------

# Explorer, two documents side by side, outline and section path.
scene_overview() {
  reset_demo
  start_app "$DEMO/specs"
  dclick 104 131                    # explorer: process-manager-v1.1.pdf, opened as a kept tab
  stable 20 1
  click 110 153 3                   # watchdog-interface-v1.0.pdf ▸ context menu
  click 176 216                     # "Open in Split Beside"
  stable 20 1
  click 18 545                      # outline of the watchdog document: unfold "2 Interface"
  click 90 567                      # "2.1 WatchdogClient"
  click 550 600                     # focus the left view
  click 18 589                      # outline: unfold "4 Process lifecycle"
  click 32 611                      # unfold "4.1 Startup"
  click 100 655                     # "4.1.2 Startup timeout"
  park
  shot overview
}

# Link preview: one pinned, one under the pointer.
scene_links() {
  reset_demo
  start_app "$DEMO/specs" "$PM11#page=10"
  move 855 761; move 860 761        # [SWS_PM_00008]
  stable 10 1.5
  click 1190 348                    # pin the preview
  drag 1195 741 1035 581            # make it smaller
  drag 700 348 1156 559             # and move it to the lower right
  move 709 552; move 713 552        # "5.2"
  stable 10 1.5
  shot link-preview
}

# Find in all open documents.
scene_find() {
  reset_demo
  start_app --find deadline "$DEMO/specs" "$PM11" "$WDG"
  drag 260 450 380 450              # widen the sidebar
  stable 20 1
  click 150 225                     # a result in the body of the specification
  park
  shot find-all
}

# Two versions compared.
scene_compare() {
  reset_demo
  start_app --diff "$PM10" "$PM11"
  stable 30 2
  click 1265 519                    # change list: the reworded startup timeout requirement
  park
  shot compare
}

# Translation popups, highlights and bookmarks, thumbnails with dark pages.
scene_reading() {
  reset_demo
  start_app "$DEMO/specs" "$PM11#page=8"
  click 1380 400                    # focus the view (page margin)
  if curl -s -m 3 http://127.0.0.1:11434/api/tags | grep -q translategemma; then
    key F2                          # translation mode on; the model loads
    stable 60 8
    move 800 609; move 805 609      # the word "supervision"
    stable 60 4
    shot translate-word
    move 1380 400                   # away, so that the popup closes
    stable 10 1.5
    drag 393 636 1201 663           # select a sentence
    stable 60 4
    shot translate-selection
    key Escape
    key F2                          # translation mode off
  else
    echo "  translation scenes skipped: Ollama with translategemma:4b is not reachable"
  fi
  drag 393 636 1201 663             # select the second sentence below the figure
  key ctrl+h                        # highlight in the default colour
  drag 393 609 1243 609             # select the first sentence
  click 1000 609 3                  # context menu
  move 1035 704; sleep 0.5          # "Highlight" ▸
  click 1220 727                    # green
  key ctrl+d                        # bookmark; the sidebar shows "Marks" with the name field
  key Return
  park
  shot marks

  click 158 38                      # sidebar: "Pages"
  key ctrl+i                        # dark page colours
  stable 60 3
  park
  shot pages-dark
  key ctrl+i
}

# Projects: the start screen with recent projects, the explorer of a project.
scene_projects() {
  reset_demo
  start_app
  click 784 154                     # start screen: "New project"
  click 748 432                     # name field
  typetext "Watchdog review"
  key Return
  stable 10 1
  click 914 154                     # "New project" again (the sidebar now takes room)
  click 748 432
  typetext "Example Platform"
  key Return
  stable 10 1
  local project="$DEMO/Documents/refdesk/Example Platform"
  mkdir -p "$project/drafts"
  cp "$PM11" "$WDG" "$project/"
  cp "$PM10" "$project/drafts/"
  stable 20 2
  click 213 61                      # explorer: refresh
  click 18 109                      # unfold "drafts"
  park
  shot projects

  dclick 104 153                    # open process-manager-v1.1.pdf
  stable 20 1
  key ctrl+shift+p                  # command palette
  typetext "find"
  shot command-palette
  key Escape
}

ALL=(overview links find compare reading projects)
SCENES=("$@")
[ ${#SCENES[@]} -gt 0 ] || SCENES=("${ALL[@]}")
mkdir -p "$OUT"
for s in "${SCENES[@]}"; do
  echo "scene $s"
  "scene_$s"
done
stop_app
rm -rf "$DEMO"
echo "done: $OUT"
