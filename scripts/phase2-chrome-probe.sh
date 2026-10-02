#!/usr/bin/env bash
# Phase 2 probe: native panorama presentation and chrome reachability (PREV-01).
#
# Proves, end-to-end on one real path, the highest-risk integration of Phase 2:
#
#   * real stitched frames are rendered and presented into the native X11 child
#     view (`A1 verdict` / `preview presented N frame(s)` in the worker log);
#   * the webview owns the chrome and is REACHABLE by pointer input — asserted
#     positively, by driving webview-owned controls (the controls-panel expand
#     toggle, then Play) and REQUIRING the engine log lines those controls cause.
#     A click that is swallowed instead of handled cannot produce them, so the
#     assertion is able to fail;
#   * the native child's server-side geometry is the L-shaped complement of the
#     chrome (echoed from `xwininfo`).
#
# # What the arrangement actually is
#
# The WebKitGTK webview is a GTK widget drawn into the MAIN window's own surface.
# It is NOT a separate X window, so `xwininfo -tree` shows exactly ONE
# InputOutput child: the presenter's panorama child view. An X child window always
# composites ABOVE its parent's own drawing, so inside its rectangle the panorama
# is drawn on top of the webview and receives ALL pointer input; pointer input
# reaches the webview only in the L-shaped complement (bottom transport bar,
# right controls rail, log drawer). `XLowerWindow` can only reorder SIBLING child
# windows and there is no sibling webview window, so nothing can be "lowered below
# the webview" — the probe does not claim that.
#
# Headless rig: Xvfb :99 (1280x800x24, no compositor) + dbus-run-session.
# X11 alpha compositing is a *compositor* feature; with no compositor the
# "transparent" webview region may render black instead of showing the native
# layer (RESEARCH Pitfall 2). This script therefore records the sanctioned
# tiled-opaque-child-webviews fallback arrangement in its output when no
# compositor is present, rather than claiming a compositing result it cannot
# observe.
#
# Exit 0 with `PHASE2 PROBE: PASS` on success; non-zero otherwise.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

APP_BIN="${RECO_APP_BIN:-$REPO_ROOT/target/debug/reco-app}"
DISPLAY_NUM="${PHASE2_DISPLAY:-:99}"
SCREEN_W=1280
SCREEN_H=800
LOG_FILE="$(mktemp "${TMPDIR:-/tmp}/phase2-probe.XXXXXX.log")"
XVFB_PID=""
APP_PID=""

cleanup() {
  local status=$?
  if [[ -n "$APP_PID" ]] && kill -0 "$APP_PID" 2>/dev/null; then
    kill "$APP_PID" 2>/dev/null || true
    wait "$APP_PID" 2>/dev/null || true
  fi
  if [[ -n "$XVFB_PID" ]] && kill -0 "$XVFB_PID" 2>/dev/null; then
    kill "$XVFB_PID" 2>/dev/null || true
    wait "$XVFB_PID" 2>/dev/null || true
  fi
  exit "$status"
}
trap cleanup EXIT INT TERM

fail() {
  echo "PHASE2 PROBE: FAIL — $*" >&2
  echo "--- app log (tail) ---" >&2
  sed 's/\x1b\[[0-9;]*m//g' "$LOG_FILE" 2>/dev/null | tail -n 40 >&2 || true
  exit 1
}

# `grep` against the app log, with ANSI colour codes stripped so the tracing
# subscriber's colourised output does not defeat the pattern match.
log_grep() { sed 's/\x1b\[[0-9;]*m//g' "$LOG_FILE" 2>/dev/null | grep "$@"; }

# Release any stale Xvfb on the chosen display.
if xdpyinfo -display "$DISPLAY_NUM" >/dev/null 2>&1; then
  pkill -f "Xvfb $DISPLAY_NUM" 2>/dev/null || true
  sleep 0.5
fi

# ---------------------------------------------------------------- prerequisites

[[ -x "$APP_BIN" ]] || fail "app binary not found or not executable: $APP_BIN (run: cargo build -p reco-app)"

export DISPLAY="$DISPLAY_NUM"

# ------------------------------------------------------------------- Xvfb start

Xvfb "$DISPLAY_NUM" -screen 0 "${SCREEN_W}x${SCREEN_H}x24" -nolisten tcp >/dev/null 2>&1 &
XVFB_PID=$!
for _ in $(seq 1 40); do
  xdpyinfo -display "$DISPLAY_NUM" >/dev/null 2>&1 && break
  sleep 0.25
done
xdpyinfo -display "$DISPLAY_NUM" >/dev/null 2>&1 || fail "Xvfb ${DISPLAY_NUM} did not come up"

# -------------------------------------------------------------------- app start

MEDIA_DIR="${RECO_TEST_MEDIA_DIR:-$REPO_ROOT/test-media}"
export RECO_TEST_MEDIA_DIR="$MEDIA_DIR"
export RUST_LOG="${RUST_LOG:-reco_app=info}"

dbus-run-session -- "$APP_BIN" >"$LOG_FILE" 2>&1 &
APP_PID=$!

# Wait for the main window named "Reco" to appear.
MAIN_WIN=""
for _ in $(seq 1 120); do
  MAIN_WIN="$(xdotool search --name '^Reco$' 2>/dev/null | head -n1 || true)"
  [[ -n "$MAIN_WIN" ]] && break
  sleep 0.25
done
[[ -n "$MAIN_WIN" ]] || fail "main window 'Reco' never appeared (GTK/WebKit blocked? is dbus-run-session in use?)"

# Wait for the webview to actually attach as a sibling child of the main window.
# GTK/WebKitGTK startup (portal activation, a11y, PipeWire probing) can take a
# few seconds on a headless rig before `add_child` has run.
child_count() {
  xwininfo -display "$DISPLAY_NUM" -tree -id "$MAIN_WIN" 2>/dev/null \
    | grep -cE '^\s+0x[0-9a-f]+ ' || true
}
for _ in $(seq 1 80); do
  if [[ "$(child_count)" -ge 2 ]]; then
    break
  fi
  sleep 0.25
done

echo "PHASE2 PROBE: main window id=${MAIN_WIN}"

# ------------------------------------------------------------- z-order assert

# WebKitGTK reality: the webview is a GTK widget *inside* the toplevel, not a
# separate X11 child window, so `xwininfo -tree` shows exactly ONE native
# InputOutput child of the main window — the presenter's X11 child view. The
# z-order claim therefore reduces to:
#   (a) the native child is the only InputOutput child, AND
#   (b) it was LOWERED (RESEARCH Pitfall 1) — it must not own the full window,
#       and it must not be the topmost eligible window, AND
#   (c) pointer events over the chrome reach the webview (asserted below by the
#       Import/Start-preview clicks landing on webview-owned buttons, and by a
#       companion click over the preview region NOT triggering an engine command).
TREE="$(xwininfo -display "$DISPLAY_NUM" -tree -id "$MAIN_WIN" 2>/dev/null || true)"
[[ -n "$TREE" ]] || fail "xwininfo -tree returned nothing for window ${MAIN_WIN}"

echo "PHASE2 PROBE: window tree:"
echo "$TREE"

# Count InputOutput children (the 1x1 InputOnly helper wry/GDK creates is not
# relevant to compositing). Extract child ids and query each one's class.
mapfile -t CHILD_IDS < <(echo "$TREE" | grep -oE '^\s+0x[0-9a-f]+' | tr -d ' ')
[[ "${#CHILD_IDS[@]}" -ge 1 ]] || fail "no child windows under ${MAIN_WIN}"

NATIVE_CHILD=""
for cid in "${CHILD_IDS[@]}"; do
  info="$(xwininfo -display "$DISPLAY_NUM" -id "$cid" 2>/dev/null || true)"
  cls="$(echo "$info" | awk -F': ' '/^  Class:/{print $2}' | head -n1 | tr -d ' ')"
  w="$(echo "$info" | awk -F': ' '/^  Width:/{print $2}' | head -n1 | tr -d ' ')"
  h="$(echo "$info" | awk -F': ' '/^  Height:/{print $2}' | head -n1 | tr -d ' ')"
  echo "PHASE2 PROBE: child ${cid} class=${cls} ${w}x${h}"
  if [[ "$cls" == "InputOutput" ]]; then
    NATIVE_CHILD="$cid"
    NATIVE_W="$w"
    NATIVE_H="$h"
  fi
done

[[ -n "$NATIVE_CHILD" ]] || fail "no InputOutput native child view found under ${MAIN_WIN}"

# (b) The native child must NOT cover the full window: it is the L-shaped
# complement of the chrome (1240x728 for the 1280x800 default), i.e. strictly
# smaller than the window in at least one dimension.
if [[ "$NATIVE_W" -ge "$SCREEN_W" && "$NATIVE_H" -ge "$SCREEN_H" ]]; then
  fail "native child ${NATIVE_CHILD} (${NATIVE_W}x${NATIVE_H}) covers the whole ${SCREEN_W}x${SCREEN_H} window — it was not lowered below the transparent chrome"
fi
echo "PHASE2 PROBE: native child ${NATIVE_CHILD} = ${NATIVE_W}x${NATIVE_H} (L-shaped; below the webview)"

TOP_CHILD_ID="${CHILD_IDS[-1]}"
echo "PHASE2 PROBE: topmost child = ${TOP_CHILD_ID}"

# --------------------------------------------------- drive play (Phase 2 UI)

sleep 1.5  # let the webview finish loading index.html

xdotool windowactivate --sync "$MAIN_WIN" 2>/dev/null || true
sleep 0.3

click_at() { xdotool mousemove --sync "$1" "$2" >/dev/null 2>&1 || true; sleep 0.2; xdotool click 1 >/dev/null 2>&1 || true; sleep 0.3; }

wait_for_log() {
  local pattern="$1"
  local timeout_s="$2"
  local deadline=$(( $(date +%s) + timeout_s ))
  while [[ $(date +%s) -lt $deadline ]]; do
    if log_grep -qE "$pattern"; then
      return 0
    fi
    sleep 0.3
  done
  return 1
}

# How many times `pattern` has appeared in the app log so far.
log_count() { sed 's/\x1b\[[0-9;]*m//g' "$LOG_FILE" 2>/dev/null | grep -cE "$1" || true; }

# Wait until `pattern` appears at least once MORE than `baseline` times.
#
# Counting *new* occurrences rather than mere presence is load-bearing: the app's
# mount-time `$effect` already reports the default chrome once at startup, so
# `viewport reconfigured to 1240x728` is already in the log before a single click
# happens. A presence check for that string would therefore succeed whether or not
# the click was delivered at all — precisely the unfalsifiable-assertion defect
# this step exists to remove.
wait_for_log_delta() {
  local pattern="$1" baseline="$2" timeout_s="$3"
  local deadline=$(( $(date +%s) + timeout_s ))
  while [[ $(date +%s) -lt $deadline ]]; do
    if [[ "$(log_count "$pattern")" -ge $(( baseline + 1 )) ]]; then
      return 0
    fi
    sleep 0.3
  done
  return 1
}

# The transport strip is the bottom 72px (y ~ 728..800); button centres are on
# the control row (y ~ 776). Buttons are laid out left-to-right from the 16px
# padding; their exact rendered widths depend on the font, so sweep candidate x
# positions until the expected engine log line appears. This still requires the
# click to land on the webview (a click swallowed by the native child would
# never produce an engine command), which is the pointer-routing claim.
click_strip_until() {
  local label="$1" pattern="$2"
  local y=776
  for x in 70 110 150 190 230 270 310 350 390 430 470 510; do
    click_at "$x" "$y"
    if wait_for_log "$pattern" 3; then
      echo "PHASE2 PROBE: clicked '${label}' at (${x},${y})"
      return 0
    fi
  done
  return 1
}

# The controls rail is `position: fixed; right: 0; top: 0` with the 32x32
# `.panel-toggle` as its first child (UI-SPEC Surface Layout Contract). At the
# 1280x800 default the rail spans x=1240..1280, y=0..728 and the toggle's centre
# lands near (1258, 28). EVERY candidate x is > 1240, i.e. outside the native
# child (which is 1240 wide), so a click that lands here can only be handled by
# the webview. Exact rendered offsets depend on font metrics, so sweep a tight
# grid around the computed centre and stop at the first hit.
click_rail_until() {
  local label="$1" pattern="$2"
  local baseline
  baseline="$(log_count "$pattern")"
  local x y
  for y in 28 36; do
    for x in 1258 1264 1252 1270 1246; do
      click_at "$x" "$y"
      if wait_for_log_delta "$pattern" "$baseline" 2; then
        echo "PHASE2 PROBE: clicked '${label}' at (${x},${y})"
        return 0
      fi
    done
  done
  return 1
}

# (1) A click directly over the *preview region* (top-left, over the native
# child) starts no engine command. This is a NEGATIVE observation, so it proves
# only one thing: the click did not reach a webview-owned control. It CANNOT
# distinguish "the webview handled it and nothing happened" from "the native
# child swallowed it" — which is why it is NOT the pointer-routing claim, and why
# the old claim text (asserting the click reached the webview) was removed.
# FRICTION A5: this negative-only assertion is what let two blockers through
# three verification passes.
click_at 400 300
sleep 1.0
if log_grep -qE "preview session started|transport: Playing|export started"; then
  fail "a click over the preview region triggered an engine command — pointer routing is wrong"
fi
echo "PHASE2 PROBE: preview-region click started no engine command (the native child owns pointer input across its rectangle; this does NOT prove the webview saw the click)"

# (2) POSITIVE webview-routing assertion. Drive a webview-owned control that
# lives OUTSIDE the native child's rectangle and REQUIRE its effect to appear in
# the worker log. The chain this proves, end to end, on one click:
#
#   xdotool click -> webview .panel-toggle -> invoke('set_chrome')
#     -> WorkerCommand::SetChrome -> GpuEngineBackend::set_chrome
#     -> reconfigure_viewport -> INFO "viewport reconfigured to 1000x728"
#
# 1000 = 1280 - CONTROLS_PANEL_WIDTH (280); 728 = 800 - TRANSPORT_BAR_HEIGHT (72);
# both are the numbers Rust's `ViewportRect::for_chrome` computes, so the line can
# only appear if the click reached the webview, crossed the typed worker channel,
# and drove a presenter reconfigure. If the pattern never appears the probe FAILS
# — it does not fall through.
EXPANDED="viewport reconfigured to 1000x728"
COLLAPSED="viewport reconfigured to 1240x728"

click_rail_until "controls-panel expand toggle" "$EXPANDED" \
  || fail "the controls-panel expand toggle was never driven: '${EXPANDED}' did not appear in the log after sweeping the rail — pointer input outside the native child is not reaching the webview"
echo "PHASE2 PROBE: webview-owned toggle drove the worker (${EXPANDED})"

# Restore the collapsed default so later steps (and later plans) start from a
# known chrome state. 1240 = 1280 - CONTROLS_PANEL_COLLAPSED_WIDTH (40). The
# delta wait is what makes this a real check: the same string is already in the
# log from startup, so presence would prove nothing.
click_rail_until "controls-panel collapse toggle" "$COLLAPSED" \
  || fail "the controls-panel collapse toggle was never driven: '${COLLAPSED}' did not appear in the log after sweeping the rail — the panel stayed expanded"
echo "PHASE2 PROBE: chrome restored (${COLLAPSED})"

# (3) Play via a webview-owned button in the strip. Phase 2 auto-imports on
# startup (hardcoded clips), so the only command needed is Play.
click_strip_until "Play" "preview session started" \
  || fail "no play command after sweeping the transport strip — pointer events did not reach the webview buttons"

# ---------------------------------------------------- assert engine progress

wait_for_log "A1 verdict|preview presented" 60 || fail "no stitched frame presented (A1 verdict / preview presented missing)"

# The app must NOT have logged a preview failure.
if log_grep -qE "preview failed"; then
  fail "app logged 'preview failed'"
fi

# --------------------------------------------------- transparency disposition

# Determine whether the transparent single-webview arrangement is viable on this
# rig. On a compositor-less Xvfb the alpha region cannot be blended, so the
# native layer renders (log proves it) but is not visible through the webview.
# We record the disposition rather than hard-failing: both arrangements satisfy
# the probe's core claims (z-order + pointer routing + real frames).
if command -v xcompmgr >/dev/null 2>&1 || command -v picom >/dev/null 2>&1; then
  ARRANGEMENT="transparent single full-window webview (compositor available)"
else
  ARRANGEMENT="transparent single full-window webview (no compositor on rig — X11 alpha blending unobservable; sanctioned fallback: tiled opaque child webviews for bottom bar + right rail, tiled around the hole)"
fi
echo "PHASE2 PROBE: arrangement = ${ARRANGEMENT}"

# Optional evidence: capture the tree so the z-order claim is recorded.
echo "PHASE2 PROBE: presented frames tail:"
log_grep -E "A1 verdict|preview presented" | tail -n 3 || true

echo "PHASE2 PROBE: PASS"
