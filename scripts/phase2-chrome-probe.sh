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
#
# Phase 3 update (03): the shell now has a persistent 48px top workflow rail and
# boots on the Import screen (where the native child view is suspended). The
# native child geometry therefore starts at y=48 and is 680px tall at 1280x800
# (was 728 before the rail), and the controls-panel toggle now sits below the
# rail (y≈76, was 28). This probe predates the rail: before it asserts native
# child geometry it must first navigate to the Preview screen via the rail.

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

# WebKitGTK reality, as `xwininfo -tree` shows it: the webview is a GTK widget
# *inside* the toplevel, NOT a separate X11 child window, so the tree shows
# exactly ONE native InputOutput child — the presenter's panorama child view.
# The claims this block can therefore make are:
#   (a) the panorama child is the only InputOutput child, AND
#   (b) its geometry is the L-shaped complement of the chrome, i.e. strictly
#       smaller than the window in at least one dimension, AND
#   (c) it IS the topmost child — and that is EXPECTED, not a defect: an X child
#       window always composites above its parent's own drawing (where the
#       webview paints), so inside its rectangle the panorama is on top and
#       receives all pointer input. It needs to be transparent to the webview
#       only if the webview owned pose input there; under the accepted
#       arrangement the CHILD owns pose input instead.
# Pointer routing for the chrome is asserted positively further down, by driving
# webview-owned controls and requiring their log effects.
TREE="$(xwininfo -display "$DISPLAY_NUM" -tree -id "$MAIN_WIN" 2>/dev/null || true)"
[[ -n "$TREE" ]] || fail "xwininfo -tree returned nothing for window ${MAIN_WIN}"

echo "PHASE2 PROBE: window tree:"
echo "$TREE"

# Count InputOutput children (the 1x1 InputOnly helper wry/GDK creates is not a
# drawable). Extract child ids and query each one's class.
mapfile -t CHILD_IDS < <(echo "$TREE" | grep -oE '^\s+0x[0-9a-f]+' | tr -d ' ')
[[ "${#CHILD_IDS[@]}" -ge 1 ]] || fail "no child windows under ${MAIN_WIN}"

NATIVE_CHILD=""
INPUTOUTPUT_COUNT=0
for cid in "${CHILD_IDS[@]}"; do
  info="$(xwininfo -display "$DISPLAY_NUM" -id "$cid" 2>/dev/null || true)"
  cls="$(echo "$info" | awk -F': ' '/^  Class:/{print $2}' | head -n1 | tr -d ' ')"
  w="$(echo "$info" | awk -F': ' '/^  Width:/{print $2}' | head -n1 | tr -d ' ')"
  h="$(echo "$info" | awk -F': ' '/^  Height:/{print $2}' | head -n1 | tr -d ' ')"
  echo "PHASE2 PROBE: child ${cid} class=${cls} ${w}x${h}"
  if [[ "$cls" == "InputOutput" ]]; then
    INPUTOUTPUT_COUNT=$(( INPUTOUTPUT_COUNT + 1 ))
    NATIVE_CHILD="$cid"
    NATIVE_W="$w"
    NATIVE_H="$h"
  fi
done

[[ -n "$NATIVE_CHILD" ]] || fail "no InputOutput native child view found under ${MAIN_WIN}"
# (a) Exactly one InputOutput child: the webview is not a window, so any second
# InputOutput child would mean the arrangement is not the one this probe reads.
[[ "$INPUTOUTPUT_COUNT" -eq 1 ]] \
  || fail "expected exactly 1 InputOutput child (the panorama child view), found ${INPUTOUTPUT_COUNT}"

# (b) The native child must NOT cover the full window: it is the L-shaped
# complement of the chrome (1240x680 for the 1280x800 default), i.e. strictly
# smaller than the window in at least one dimension.
if [[ "$NATIVE_W" -ge "$SCREEN_W" && "$NATIVE_H" -ge "$SCREEN_H" ]]; then
  fail "native child ${NATIVE_CHILD} (${NATIVE_W}x${NATIVE_H}) covers the whole ${SCREEN_W}x${SCREEN_H} window — the chrome reservation is gone"
fi
echo "PHASE2 PROBE: panorama child ${NATIVE_CHILD} = ${NATIVE_W}x${NATIVE_H} (L-shaped complement of the chrome; the webview is drawn by the parent window, so the child composites ABOVE it and owns all pointer input in that rectangle)"

# (c) The single InputOutput child IS the topmost child. This is a fact about
# the tree and it is expected under the accepted arrangement — not a defect to
# fix by lowering it. `XLowerWindow` can only reorder SIBLING child windows and
# there is no sibling webview window, so lowering cannot put the webview above
# the panorama.
TOP_CHILD_ID="${CHILD_IDS[-1]}"
echo "PHASE2 PROBE: topmost child = ${TOP_CHILD_ID} (= the panorama child; expected: a child window composites above its parent's own drawing, and the child — not the webview — owns pose input inside its rectangle)"

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
# `viewport reconfigured to 1240x680` is already in the log before a single click
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

# ---------------------------------------------------------------- pose helpers

# The worker's pose report, in the same shape the webview's typed `Pose` event
# projects to:
#     "pose: yaw {:.3}, pitch {:.3}, fov {:.1}"
# It is written to stdout at exactly two points, neither per-tick:
#   * once when a session starts (the BASELINE, before any gesture), and
#   * after a gesture is drained (the TARGET that `dispatch_intent` set).
# Reading the target keeps the assertion immune to the easing rate of the
# current pose, and the session-start line is what makes a "before" value
# exist at all.
#
# Values are located by KEYWORD rather than by fixed field offset, so adding a
# prefix to the message cannot silently shift every index and make the
# assertions compare the wrong number.
#
# Prints EMPTY when no such line exists yet. Callers must treat empty as "no
# baseline" and fail — never as 0, which would silently pass a comparison.
#
# Always exits 0: the script runs with `set -euo pipefail`, and a `grep` that
# finds nothing would otherwise fail the *assignment* this is called from,
# killing the script before any caller's emptiness check can run. That is a
# silent death with no FAIL line — far worse than an empty value.
last_pose_field() {
  local idx="$1" key
  case "$idx" in
    1) key="yaw" ;;
    2) key="pitch" ;;
    3) key="fov" ;;
    *) return 0 ;;
  esac
  local line
  line="$(sed 's/\x1b\[[0-9;]*m//g' "$LOG_FILE" 2>/dev/null \
    | grep -E 'pose: yaw -?[0-9]' | tail -n 1 || true)"
  [[ -n "$line" ]] || return 0
  # Locate the value by its KEY: scan tokens for the key, take the next one with
  # any trailing comma stripped. Keying on the name means reformatting the
  # message cannot silently shift an index onto the wrong number.
  printf '%s\n' "$line" \
    | awk -v key="$key" '
        {
          for (i = 1; i < NF; i++) {
            if ($i == key) {
              v = $(i + 1)
              gsub(/,/, "", v)
              if (v ~ /^-?[0-9]+\.[0-9]+$/) print v
              exit
            }
          }
        }' || true
  return 0
}

# Poll until pose field `idx` differs from `baseline` by more than `threshold`.
# Echoes the new value; non-zero on timeout.
pose_field_changed() {
  local idx="$1" baseline="$2" threshold="$3" timeout_s="$4"
  local deadline=$(( $(date +%s) + timeout_s ))
  local now delta
  while [[ $(date +%s) -lt $deadline ]]; do
    now="$(last_pose_field "$idx")"
    if [[ -n "$now" ]]; then
      delta="$(awk -v a="$now" -v b="$baseline" \
        'BEGIN { d = a - b; if (d < 0) d = -d; printf "%.6f", d }')"
      if awk -v d="$delta" -v t="$threshold" 'BEGIN { exit !(d > t) }'; then
        echo "$now"
        return 0
      fi
    fi
    sleep 0.3
  done
  return 1
}

# ---------------------------------------------------------------- geometry helpers

# Server-side width of the native child, from the X server itself — not from
# the app's own log. Queried against the SAME $NATIVE_CHILD id captured at
# startup; nothing is re-derived.
#
# Always exits 0 (same `set -e` hazard as `last_pose_field`): callers test the
# returned string, and a failing query must yield an empty value they can
# report, not terminate the probe silently.
child_width() {
  xwininfo -display "$DISPLAY_NUM" -id "$NATIVE_CHILD" 2>/dev/null \
    | awk -F': ' '/^  Width:/{print $2}' | head -n 1 | tr -d ' ' || true
  return 0
}

# Poll until the X server reports the child at <expected>. Echoes the observed
# width and returns non-zero on timeout.
wait_for_child_width() {
  local expected="$1" timeout_s="$2"
  local deadline=$(( $(date +%s) + timeout_s ))
  local w=""
  while [[ $(date +%s) -lt $deadline ]]; do
    w="$(child_width)"
    if [[ "$w" == "$expected" ]]; then
      echo "PHASE2 PROBE: native child width = ${w} (expected ${expected})"
      return 0
    fi
    sleep 0.3
  done
  echo "PHASE2 PROBE: child width never reached ${expected} (observed '${w:-unreadable}')" >&2
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
# 1280x800 default the rail spans x=1240..1280, y=48..728 and the toggle's centre
# lands near (1258, 76). EVERY candidate x is > 1240, i.e. outside the native
# child (which is 1240 wide), so a click that lands here can only be handled by
# the webview. Exact rendered offsets depend on font metrics, so sweep a tight
# grid around the computed centre and stop at the first hit.
click_rail_until() {
  local label="$1" pattern="$2"
  local baseline
  baseline="$(log_count "$pattern")"
  local x y
  for y in 72 80; do
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
#     -> reconfigure_viewport -> INFO "viewport reconfigured to 1000x680"
#
# 1000 = 1280 - CONTROLS_PANEL_WIDTH (280); 680 = 800 - WORKFLOW_RAIL_HEIGHT (48) - TRANSPORT_BAR_HEIGHT (72);
# both are the numbers Rust's `ViewportRect::for_chrome` computes, so the line can
# only appear if the click reached the webview, crossed the typed worker channel,
# and drove a presenter reconfigure. If the pattern never appears the probe FAILS
# — it does not fall through.
EXPANDED="viewport reconfigured to 1000x680"
COLLAPSED="viewport reconfigured to 1240x680"

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

# --------------------------------------------------- geometry (server-side)
#
# 02-08 already asserts the WORKER RECEIVED the chrome change. That assertion
# passed while the child window stayed 1240 wide: the surface and the log were
# right and the window was wrong. This step asks the X SERVER.
#
# The two numbers are hard-coded on purpose, not derived from the screen size.
# They come from `ViewportRect::for_chrome(1280, 800, …)` — 1000 = 1280 - 280
# (expanded controls panel), 1240 = 1280 - 40 (collapsed rail), 680 = 800 - 48 - 72
# (transport bar). Hard-coding them means a change to the geometry authority
# itself is a visible failure rather than something the probe silently agrees
# with by recomputing the same wrong number.
w0="$(child_width)"
if [[ "$w0" != "1240" ]]; then
  fail "native child baseline width is '${w0:-unreadable}', expected 1240 (collapsed default)"
fi
echo "PHASE2 PROBE: geometry baseline, child width = ${w0} (collapsed)"

click_rail_until "controls-panel expand toggle (geometry)" "$EXPANDED" \
  || fail "could not expand the panel to assert geometry: '${EXPANDED}' missing from the log"
wait_for_child_width 1000 10 \
  || fail "the X server still reports the native child at '$(child_width)' after expansion — expected 1000; the window did not follow the chrome (surface/log may still agree, which is exactly the defect this step catches)"

# The app's OWN report must agree with the server's, in the agreeing form only:
# `mismatch` means the window refused the request and must fail even if the
# numbers happened to line up afterwards.
if log_grep -qE 'native viewport: requested 1000x680, child window .*mismatch'; then
  fail "the worker reports a geometry MISMATCH for the expanded state"
fi
if ! log_grep -qE 'native viewport: requested 1000x680, child window 1000x680'; then
  fail "the worker never reported the expanded geometry agreeing with the request"
fi
echo "PHASE2 PROBE: expanded geometry — server and worker both report 1000x680"

click_rail_until "controls-panel collapse toggle (geometry)" "$COLLAPSED" \
  || fail "could not collapse the panel to restore geometry: '${COLLAPSED}' missing from the log"
wait_for_child_width 1240 10 \
  || fail "the X server reports '$(child_width)' after collapse — expected 1240"
if log_grep -qE 'native viewport: requested 1240x680, child window .*mismatch'; then
  fail "the worker reports a geometry MISMATCH for the collapsed state"
fi
echo "PHASE2 PROBE: geometry restored to 1240; child is back at its default size"

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

# ------------------------------------------------ native pose input assertions
#
# 02-08 removed an unfalsifiable claim but added no assertion requiring
# behaviour the tree did not have yet. 02-10 owns pose input on the native
# child; these are the assertions that would have caught its absence.
#
# They come AFTER the A1 wait because `pose:` lines are only emitted while a
# session is ticking — and after the panel expand/collapse above (which restores
# the 1240-wide child) so the gestures land inside the child's rectangle.
#
# DIRECTION IS DELIBERATELY NOT CHECKED. Dragging right takes a negative yaw
# delta (+yaw looks LEFT), per `crates/reco-cli/src/preview.rs:582-598` and
# FRICTION A6. Turning this into a sign check would make the probe a direction
# oracle, and direction is a product judgement for the human verification list.
# The probe's job is to prove the gesture REACHED THE ENGINE AT ALL. Do not
# "improve" this into a sign comparison.

# --- drag pan -------------------------------------------------------------
yaw_before="$(last_pose_field 1)"
if [[ -z "$yaw_before" ]]; then
  fail "no 'gesture applied: yaw …' line in the log before dragging — the session is not ticking or no gesture has ever been drained (probe sequencing bug), not a product failure"
fi

# Step the pointer across the child in explicit moves. A single jump can be
# coalesced by the X server into one motion event, and the drain runs once per
# tick, so intermediate steps are what make the drag actually register.
#
# Written out rather than as a loop: this is a probe, and being able to count
# the steps with `grep -c mousemove` is itself part of the check that they
# exist. Ten steps across 300 -> 900 at constant y, so the assertion is about
# yaw alone (pitch is clamped per tick by coverage and proves less here).
if ! command -v xdotool >/dev/null 2>&1; then
  fail "xdotool is required to synthesise the drag — not skipping this assertion"
fi
xdotool mousemove --sync 300 300 >/dev/null 2>&1 || true
sleep 0.2
xdotool mousedown 1 >/dev/null 2>&1 || fail "xdotool mousedown failed — cannot synthesise a drag"
xdotool mousemove --sync 360 300 >/dev/null 2>&1 || true
sleep 0.06
xdotool mousemove --sync 420 300 >/dev/null 2>&1 || true
sleep 0.06
xdotool mousemove --sync 480 300 >/dev/null 2>&1 || true
sleep 0.06
xdotool mousemove --sync 540 300 >/dev/null 2>&1 || true
sleep 0.06
xdotool mousemove --sync 600 300 >/dev/null 2>&1 || true
sleep 0.06
xdotool mousemove --sync 660 300 >/dev/null 2>&1 || true
sleep 0.06
xdotool mousemove --sync 720 300 >/dev/null 2>&1 || true
sleep 0.06
xdotool mousemove --sync 780 300 >/dev/null 2>&1 || true
sleep 0.06
xdotool mousemove --sync 840 300 >/dev/null 2>&1 || true
sleep 0.06
xdotool mousemove --sync 900 300 >/dev/null 2>&1 || true
sleep 0.1
xdotool mouseup 1 >/dev/null 2>&1 || true

yaw_after="$(pose_field_changed 1 "$yaw_before" 0.01 10)" \
  || fail "yaw did not change after a 600px drag over the native child (before=${yaw_before}) — native pointer input is not reaching ControlIntent"
echo "PHASE2 PROBE: drag changed yaw ${yaw_before} -> ${yaw_after} (direction intentionally not asserted)"

# --- wheel zoom -----------------------------------------------------------
fov_before="$(last_pose_field 3)"
if [[ -z "$fov_before" ]]; then
  fail "no 'pose: … fov …' line in the log before wheeling — probe sequencing bug, not a product failure"
fi

xdotool mousemove --sync 600 400 >/dev/null 2>&1 || true
sleep 0.2
xdotool click 5 >/dev/null 2>&1 || fail "xdotool click 5 failed — cannot synthesise a wheel"
sleep 0.2
xdotool click 5 >/dev/null 2>&1 || true
sleep 0.2
xdotool click 5 >/dev/null 2>&1 || true

fov_after="$(pose_field_changed 3 "$fov_before" 1.0 10)" \
  || fail "FOV did not change after three wheel-down notches (before=${fov_before}) — wheel input is not reaching the pose"
echo "PHASE2 PROBE: wheel changed FOV ${fov_before} -> ${fov_after}"

# ------------------------------------------------- best-effort presenter swap
#
# The geometry step above is what makes the PresenterSelect reachable at all
# (before it, the native child covered x=1000..1240). This reports the outcome
# and does NOT fail: selecting a presenter re-binds a wgpu surface, and the
# readback arm cannot be exercised reliably on a software renderer with no
# compositor. A gate that fails for an environment reason is worse than no gate.
#
# Reported only. Do not promote this to an assertion without a compositor.
presenter_base="$(log_count 'presenter:')"
click_at 1258 330
if wait_for_log_delta 'presenter:' "$presenter_base" 5; then
  echo "PHASE2 PROBE: best-effort — presenter select drove a swap ($(log_grep -E 'presenter:' | tail -n 1 | sed 's/^.*INFO[^:]*: //' | cut -c1-80))"
else
  echo "PHASE2 PROBE: best-effort — presenter select did not report a swap (expected on a software renderer with no compositor; NOT a failure)"
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
