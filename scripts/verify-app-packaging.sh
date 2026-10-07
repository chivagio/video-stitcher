#!/usr/bin/env bash
#
# verify-app-packaging.sh — Phase 05 (DIAG-04 / DIAG-05)
#
# Validates the Tauri 2 bundler config for `reco-app` and attempts an unsigned
# Linux `.deb` bundle. Signing/notarization is credential-gated (it runs only in
# CI when the secrets exist); this script produces NO signature and never claims
# one. See `.planning/phases/05-export-projects-diagnostics-distribution/05-02-PACKAGING-VERIFICATION.md`.
#
# DIAG-05: the `deb` must declare the FFmpeg runtime shared libraries the app
# binary links (libavcodec61/libavformat61/libavutil59/libswscale8/libswresample5
# on this FFmpeg 7.x host) so a clean install starts the app, and the bundle must
# ship RUNTIME-PREREQUISITES.md. Both are asserted below so the gap cannot
# silently reopen.
#
# Exit status: 0 when the config validates, the UI builds, and every bundle
# build that was attempted succeeded. A bundle build that runs and FAILS is a
# non-zero exit; a missing bundler toolchain (the build is skipped) is recorded,
# not fatal.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP_DIR="$REPO_ROOT/crates/reco-app"
CONF="$APP_DIR/tauri.conf.json"

echo "==> [1/4] Validating $CONF"
node - "$CONF" <<'NODE'
const path = process.argv[2];
const c = require(path);
const targets = (c.bundle.targets || []).join(',');
for (const t of ['dmg', 'nsis', 'appimage', 'deb']) {
  if (!targets.includes(t)) throw new Error(`missing bundle target: ${t}`);
}
if (!c.bundle.windows || !c.bundle.windows.webviewInstallMode) {
  throw new Error('missing bundle.windows.webviewInstallMode');
}
if (!c.bundle.macOS || !c.bundle.macOS.hardenedRuntime) {
  throw new Error('missing bundle.macOS.hardenedRuntime');
}
if (/certificate|password/i.test(JSON.stringify(c))) {
  throw new Error('secret-like key in bundler config');
}
// DIAG-05: the deb must pull in the FFmpeg runtime libraries the binary links.
const deps = (c.bundle.linux && c.bundle.linux.deb && c.bundle.linux.deb.depends) || [];
const requiredDeps = [
  'libwebkit2gtk-4.1-0', 'libgtk-3-0',
  'libavcodec61', 'libavformat61', 'libavutil59', 'libswscale8', 'libswresample5',
];
for (const d of requiredDeps) {
  if (!deps.includes(d)) throw new Error(`bundle.linux.deb.depends missing ${d}`);
}
// DIAG-05: the prerequisite doc must ship with the app.
const resources = c.bundle.resources || {};
const resourceKeys = Array.isArray(resources) ? resources : Object.keys(resources);
if (!resourceKeys.some((r) => String(r).includes('RUNTIME-PREREQUISITES'))) {
  throw new Error('bundle.resources must ship RUNTIME-PREREQUISITES.md');
}
console.log('    config OK: targets=' + targets + ' deb-depends=' + deps.length);
NODE

echo "==> [2/4] Building UI (npm --prefix crates/reco-app run build)"
npm --prefix "$APP_DIR" run build

echo "==> [3/4] Unsigned Linux deb bundle (best-effort)"
DEB_STATUS="not-produced"
DEB_PATH=""
DEB_DEPS_OK=1
DEB_RESOURCE_OK=1
if command -v dpkg-deb >/dev/null 2>&1; then
  if (cd "$APP_DIR" && npx tauri build --bundles deb); then
    DEB_PATH="$(find "$REPO_ROOT/target/release/bundle/deb" "$APP_DIR/target/release/bundle/deb" \
      -maxdepth 1 -name '*.deb' 2>/dev/null | head -n 1 || true)"
    if [ -n "$DEB_PATH" ]; then
      DEB_STATUS="produced"
      echo "    .deb produced: $DEB_PATH"
      # DIAG-05: the produced deb must actually declare the FFmpeg runtime
      # libraries the binary links, and ship the prerequisite doc. Checking the
      # artifact (not just the config) is what keeps the gap closed.
      DEB_DEPS="$(dpkg-deb -f "$DEB_PATH" Depends 2>/dev/null || true)"
      for d in libavcodec61 libavformat61 libavutil59 libswscale8 libswresample5; do
        if ! printf '%s' "$DEB_DEPS" | grep -qw "$d"; then
          echo "    ERROR: deb Depends missing $d (got: ${DEB_DEPS:-<empty>})" >&2
          DEB_DEPS_OK=0
        fi
      done
      if ! dpkg-deb -c "$DEB_PATH" 2>/dev/null | grep -q 'RUNTIME-PREREQUISITES.md'; then
        echo "    ERROR: deb does not ship RUNTIME-PREREQUISITES.md" >&2
        DEB_RESOURCE_OK=0
      fi
    else
      DEB_STATUS="build-ok-no-deb"
      echo "    WARNING: tauri build exited 0 but no .deb was found" >&2
    fi
  else
    DEB_STATUS="failed"
    echo "    ERROR: deb bundle build failed" >&2
  fi
else
  DEB_STATUS="skipped-no-dpkg-deb"
  echo "    skipped: dpkg-deb not available on this host"
fi

echo "==> [4/4] Linux AppImage bundle (best-effort; only when tooling present)"
APPIMAGE_STATUS="not-produced"
MISSING_TOOLS=""
for tool in appimagetool linuxdeploy patchelf; do
  command -v "$tool" >/dev/null 2>&1 || MISSING_TOOLS="$MISSING_TOOLS $tool"
done
if [ -z "$MISSING_TOOLS" ]; then
  if (cd "$APP_DIR" && npx tauri build --bundles appimage); then
    APPIMAGE_STATUS="produced"
  else
    APPIMAGE_STATUS="failed"
  fi
else
  APPIMAGE_STATUS="skipped-missing-tools:${MISSING_TOOLS# }"
  echo "    not produced: missing bundler tooling:$MISSING_TOOLS"
fi

echo
echo "---- packaging summary ----"
echo "deb:      $DEB_STATUS ${DEB_PATH}"
echo "appimage: $APPIMAGE_STATUS"
echo "signing:  credential-gated (not executed in this environment)"

# The success signal must reflect the bundle outcomes, not merely that the
# script reached the end: a build that ran and failed (or exited 0 without
# producing its artifact) is a packaging regression, not a pass (WR-04).
# `skipped-*` remains non-fatal — the toolchain was absent, so nothing failed.
FAILED_REASON=""
if [ "$DEB_STATUS" = "failed" ]; then
  FAILED_REASON="deb build failed"
elif [ "$DEB_STATUS" = "build-ok-no-deb" ]; then
  FAILED_REASON="deb build reported success but produced no .deb"
elif [ "$DEB_DEPS_OK" = "0" ]; then
  FAILED_REASON="deb Depends omit an FFmpeg runtime library the app links"
elif [ "$DEB_RESOURCE_OK" = "0" ]; then
  FAILED_REASON="deb does not ship RUNTIME-PREREQUISITES.md"
elif [ "$APPIMAGE_STATUS" = "failed" ]; then
  FAILED_REASON="appimage build failed"
fi

if [ -n "$FAILED_REASON" ]; then
  echo "PACKAGING VERIFY: FAIL ($FAILED_REASON)"
  exit 1
fi
echo "PACKAGING VERIFY: PASS"
