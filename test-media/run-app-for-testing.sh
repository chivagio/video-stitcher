#!/usr/bin/env bash
# Interactive test for the Reco desktop app (Phase 2 preview shell).
#
# Phase 2 imports the hardcoded clips AT STARTUP and its chrome has no Import
# button: the transport bar is Step back / Play / Step forward / Loop. The full
# manual checklist is .planning/phases/02-.../02-UAT.md.
#
# RUN THIS FROM AN SSH SESSION WITH X11 FORWARDING:
#     ssh -X -Y <user>@<host>
#     echo $DISPLAY          # must be set (e.g. localhost:10.0)
#
# Then:
#     bash test-media/run-app-for-testing.sh
#
# It checks prerequisites, warns about VRAM, then launches the app so you can
# drive it with the mouse. Ctrl-C in this terminal stops it.

set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO"
export PATH="$HOME/.cargo/bin:$PATH"

red()  { printf '\033[31m%s\033[0m\n' "$*"; }
grn()  { printf '\033[32m%s\033[0m\n' "$*"; }
ylw()  { printf '\033[33m%s\033[0m\n' "$*"; }
hdr()  { printf '\n\033[1m== %s ==\033[0m\n' "$*"; }

hdr "1. Display (X11 forwarding)"
if [ -z "${DISPLAY:-}" ]; then
  red "DISPLAY is not set — you are not using X11 forwarding."
  echo "Reconnect with:  ssh -X -Y <user>@<host>"
  echo "(On Windows, run VcXsrv or MobaXterm first — see the notes at the end.)"
  exit 1
fi
if ! xdpyinfo >/dev/null 2>&1; then
  red "DISPLAY=$DISPLAY is set but not reachable."
  echo "Check that your X server (VcXsrv / MobaXterm) is running and that you"
  echo "connected with -X. If the X server blocks connections, set its access"
  echo "control to 'Disabled' (VcXsrv: Extra settings -> Access control)."
  exit 1
fi
grn "DISPLAY=$DISPLAY is reachable."
xdpyinfo 2>/dev/null | grep -E 'name of display|dimensions' | sed 's/^/    /'

hdr "2. GPU / VRAM"
if command -v nvidia-smi >/dev/null 2>&1; then
  nvidia-smi --query-gpu=name,memory.total,memory.used,memory.free \
             --format=csv,noheader 2>/dev/null | sed 's/^/    /'
  FREE=$(nvidia-smi --query-gpu=memory.free --format=csv,noheader,nounits 2>/dev/null | head -1)
  if [ -n "${FREE:-}" ] && [ "$FREE" -lt 1500 ]; then
    ylw "Only ${FREE} MiB VRAM free. The app may fail with 'Not enough memory left'."
    echo "Free some by stopping the LLM container first:"
    echo "    docker stop llama-server"
    echo "    nvidia-smi --query-gpu=memory.free --format=csv"
    echo "Restart it later with:  docker start llama-server"
    printf "Continue anyway? [y/N] "
    read -r ans
    case "$ans" in [yY]*) ;; *) echo "Stopped."; exit 1 ;; esac
  else
    grn "VRAM free: ${FREE:-?} MiB"
  fi
else
  ylw "nvidia-smi not found — skipping the VRAM check."
fi

hdr "3. Build"
# The Tauri bundle embeds ui/dist, and `cargo build -p reco-app` fails without
# it, so build the frontend first on a fresh clone.
if [ ! -f crates/reco-app/ui/dist/index.html ]; then
  echo "Frontend bundle missing; building it (npm ci + vite build)..."
  if ! command -v npm >/dev/null 2>&1; then
    red "npm not found but ui/dist is missing."
    echo "Install Node 20+, then re-run. Nothing else can build the webview."
    exit 1
  fi
  (cd crates/reco-app && npm ci && npm run build) \
    || { red "Frontend build failed."; exit 1; }
fi
grn "Frontend bundle present: crates/reco-app/ui/dist"

if [ ! -x target/debug/reco-app ]; then
  echo "Binary not found; building (first build is slow)..."
  cargo build -p reco-app || { red "Build failed."; exit 1; }
fi
grn "Binary present: target/debug/reco-app"

hdr "4. Launching"
cat <<'NOTES'
What you should see (Phase 2):
  * A window titled "Reco", about 1280x800.
  * A RIGHT-HAND controls panel (Pose / FOV slider / view toggle / presenter
    override) that collapses to a narrow rail.
  * A BOTTOM transport bar: timeline, then Step back / Play / Step forward /
    Loop / Log, then the presenter status badge.
  * The large top-LEFT area is the preview: a native GPU child view, not a web
    element. It is NOT painted by the webview, so it looks empty over a plain
    background until the panorama is presented.

Drive it in this order:
  1. DO NOT look for an Import button - there isn't one. The app imports the
     hardcoded clips at startup. Open the log drawer IN THE APP (the [Log]
     button on the transport bar) - the terminal does NOT show position/pose
     lines, only import/session/A1 ones. The drawer should show, in order:
         import started
         import finished
         transport: Paused, loop off
         position: frame 0/N @ 30000/1001
  2. Press Play (or Space) -> the top-left area fills with the stitched
     panorama, live at ~30fps, and the log shows
         preview session started
         A1 verdict: engine worker presented a stitched frame ...
  3. Wheel over the preview     -> FOV readout changes.
     Drag over the preview      -> yaw/pitch readout changes.
     Shift + arrow               -> the pose nudges.
     Drag the timeline           -> seeks; the accent fill follows the playhead.
  4. Toggle the view (or press V) -> the two raw sources appear tiled left|right.
  5. Export is not on the transport bar in Phase 2; skip it.

Then close the window and confirm this terminal prints:
      engine worker stopped cleanly
(That line is the FOUND-06 clean-teardown check.)

What to report back if something is wrong:
  * Which step failed, and the exact line shown in the log drawer.
  * Whether the panorama appeared in the TOP-LEFT region (vs a black box or
    nothing), and whether the status badge says "Native".
  * The last ~15 lines of this terminal's output.

The definitive checklist - including the checks that only a human with a GPU,
a real window, and a compositor can make - is:
      .planning/phases/02-zero-copy-preview-playback-pose/02-UAT.md
NOTES
echo
set -x
# A DBus session bus is required: without it GTK/WebKitGTK can hang before the
# app's setup runs. If dbus-run-session is unavailable, plain launch still works
# on a normal desktop.
if command -v dbus-run-session >/dev/null 2>&1; then
  exec dbus-run-session -- ./target/debug/reco-app
else
  exec ./target/debug/reco-app
fi
