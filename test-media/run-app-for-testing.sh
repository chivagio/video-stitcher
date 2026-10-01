#!/usr/bin/env bash
# Interactive test for the Reco desktop app (Phase 1 walking skeleton).
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
if [ ! -x target/debug/reco-app ]; then
  echo "Binary not found; building (first build is slow)..."
  cargo build -p reco-app || { red "Build failed."; exit 1; }
fi
grn "Binary present: target/debug/reco-app"

hdr "4. Launching"
cat <<'NOTES'
What you should see:
  * A window titled "Reco", about 1280x800.
  * Bottom ~208px of UI: an event log pane above a row of three buttons
    labelled  [Import]  [Start preview]  [Export]
  * The large top area is TRANSPARENT/empty at first — that region is a
    native GPU child view, not part of the web page.

Drive it in this order:
  1. Click [Import]         -> log shows "import started" / "import finished";
                               the button greys out while working.
  2. Click [Start preview]  -> the TOP AREA fills with the stitched panorama
                               (colour-bar test footage), live at ~30fps;
                               log shows "...presented 60 frame(s)".
  3. Click [Export]         -> log shows "export started" / "export finished";
                               test-media/reco-app-export.mp4 is (re)written.

Then close the window and confirm this terminal prints:
      engine worker stopped cleanly
(That line is the FOUND-06 clean-teardown check.)

What to report back if something is wrong:
  * Which step failed, and the exact log line shown in the event pane.
  * Whether the panorama appeared in the TOP region (vs a black box or nothing).
  * The last ~15 lines of this terminal's output.
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
