#!/usr/bin/env bash
#
# verify-windows-bundling.sh — quick task 261008-k9h
#
# Statically validates the Windows FFmpeg-bundling invariants for `reco-app`
# WITHOUT a Windows runner or bundler toolchain. It asserts that the release
# workflow stages the FFmpeg DLLs + GPL license/notice into both the installer
# resources overlay (`crates/reco-app/tauri.windows.conf.json`) and the portable
# ZIP, that the credential-gated Authenticode signing is preserved, and that the
# base `tauri.conf.json` (and therefore the Linux/macOS bundles) is unaffected.
#
# This is a local/executor gate; it is deliberately NOT wired into any CI
# workflow. It mirrors the style of `scripts/verify-app-packaging.sh`.
#
# Exit status: 0 on success; non-zero with a clear message on the first failed
# assertion.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORKFLOW="$REPO_ROOT/.github/workflows/release-app.yml"
CONF="$REPO_ROOT/crates/reco-app/tauri.conf.json"

fail() {
  echo "WINDOWS-BUNDLING VERIFY: FAIL ($1)" >&2
  exit 1
}

# require_token FILE TOKEN DESCRIPTION — TOKEN must appear at least once in FILE.
require_token() {
  local file="$1" token="$2" desc="$3"
  if ! grep -qF -- "$token" "$file"; then
    fail "$desc (missing '$token' in ${file#"$REPO_ROOT"/})"
  fi
}

echo "==> [1/6] Validating YAML: ${WORKFLOW#"$REPO_ROOT"/}"
if python3 -c "import yaml" >/dev/null 2>&1; then
  python3 -c "import yaml,sys; yaml.safe_load(open(sys.argv[1]))" "$WORKFLOW" \
    || fail "release-app.yml is not valid YAML"
  echo "    yaml OK"
else
  echo "    skipped: PyYAML not available"
fi

echo "==> [2/6] FFmpeg env exports (Windows step)"
require_token "$WORKFLOW" 'FFMPEG_DLL_DIR' 'Windows FFmpeg step must export FFMPEG_DLL_DIR'
require_token "$WORKFLOW" 'FFMPEG_LICENSE' 'Windows FFmpeg step must export FFMPEG_LICENSE'
require_token "$WORKFLOW" 'FFMPEG_ASSET_URL' 'Windows FFmpeg step must export FFMPEG_ASSET_URL'

echo "==> [3/6] Installer resources overlay (DLLs + GPL license/notice)"
require_token "$WORKFLOW" 'tauri.windows.conf.json' 'workflow must generate the Windows config overlay'
require_token "$WORKFLOW" 'FFMPEG_DLL_DIR' 'overlay must map the FFmpeg DLLs into bundle.resources'
require_token "$WORKFLOW" 'FFMPEG-LICENSE.txt' 'overlay must ship FFMPEG-LICENSE.txt'
require_token "$WORKFLOW" 'FFMPEG-SOURCE-OFFER' 'overlay must ship FFMPEG-SOURCE-OFFER.txt'

echo "==> [4/6] Portable ZIP step + upload path"
require_token "$WORKFLOW" 'Compress-Archive' 'portable ZIP step must use Compress-Archive'
require_token "$WORKFLOW" 'reco-app-' 'portable ZIP name must start with reco-app-'
require_token "$WORKFLOW" '-windows-x86_64.zip' 'portable ZIP name must end with -windows-x86_64.zip'
require_token "$WORKFLOW" 'reco-app-*-windows-x86_64.zip' 'upload-artifact path must include the portable ZIP'

echo "==> [5/6] Credential-gated signing preserved"
require_token "$WORKFLOW" 'WINDOWS_CERTIFICATE' 'signing step must still gate on WINDOWS_CERTIFICATE'
require_token "$WORKFLOW" 'WINDOWS_CERT_THUMBPRINT' 'signing step must still export WINDOWS_CERT_THUMBPRINT'

echo "==> [6/6] Base tauri.conf.json intact (Linux/macOS unaffected)"
if ! python3 -c "import json,sys; json.load(open(sys.argv[1]))" "$CONF" >/dev/null 2>&1; then
  fail "crates/reco-app/tauri.conf.json is not valid JSON"
fi
python3 - "$CONF" <<'PY' || fail "tauri.conf.json invariant broken"
import json
import sys

c = json.load(open(sys.argv[1]))
bundle = c.get('bundle', {})

res = bundle.get('resources', {})
keys = list(res) if isinstance(res, dict) else list(res)
if not any('RUNTIME-PREREQUISITES' in str(k) for k in keys):
    raise SystemExit('bundle.resources must still ship RUNTIME-PREREQUISITES.md')

deps = bundle.get('linux', {}).get('deb', {}).get('depends', [])
required = [
    'libavcodec61', 'libavformat61', 'libavutil59', 'libswscale8',
    'libswresample5', 'libwebkit2gtk-4.1-0', 'libgtk-3-0',
]
missing = [d for d in required if d not in deps]
if missing:
    raise SystemExit('bundle.linux.deb.depends missing: ' + ', '.join(missing))

print('    config OK: resources=' + str(keys) + ' deb-depends=' + str(len(deps)))
PY

echo "WINDOWS-BUNDLING VERIFY: PASS"
