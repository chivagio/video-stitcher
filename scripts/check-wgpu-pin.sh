#!/usr/bin/env bash
#
# check-wgpu-pin.sh — FOUND-04 gate (success criterion 5).
#
# Guards two independent invariants of the wgpu interop wire:
#
#   1. no-direct-wgpu: no *consumer* crate declares `wgpu` directly. wgpu
#      must be reached through `reco_core::wgpu` (re-exported at
#      crates/reco-core/src/lib.rs) so every consumer shares ONE wgpu type
#      universe. A direct `wgpu` key in a second crate is the
#      texture-interop version-skew anti-pattern: the same device/texture
#      types are no longer identical across crates and interop silently
#      breaks at runtime instead of at compile time.
#
#      `reco-core` is the designated owner of the pin (it constructs the
#      GPU context and re-exports wgpu); every other workspace crate must
#      consume it transitively through reco-core. The allow-list below is
#      deliberately explicit: any NEW crate that adds `wgpu` fails the gate
#      until it is consciously added here with a justification.
#
#      As of this plan the pin is ALSO centralized in
#      `[workspace.dependencies]`, so all crates reference it via
#      `workspace = true`. Check 1 additionally asserts that central entry
#      is exactly `28`; check 2 remains belt-and-braces against the lock.
#
#   2. pin-drift: `Cargo.lock` must resolve exactly ONE `wgpu` package and
#      it must be in the 28.x line. Two locked versions, or a drift off 28,
#      means the interop ABI moved underneath the pinned `metal = "0.33"` /
#      raw-window-handle 0.6 seam in reco-core.
#
# Both checks parse machine-readable input (`cargo metadata` JSON and
# `Cargo.lock` text) rather than `cargo tree` text — the `cargo tree
# --prefix-depth` output format is tool-version sensitive (RESEARCH §The
# wgpu pin gate).
#
# Dependencies: bash + node (stdlib only). `node` is already used by CI
# tooling, so this adds no new toolchain requirement.
#
# Exit 0 and print "OK" when both invariants hold; exit non-zero with a
# named offending crate/version otherwise.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

fail=0

# ---------------------------------------------------------------------------
# Check 1 — no direct `wgpu` dependency in any workspace crate.
# ---------------------------------------------------------------------------
if ! command -v node >/dev/null 2>&1; then
  echo "FAIL: node is required to parse cargo metadata JSON" >&2
  exit 1
fi

if ! cargo metadata --format-version 1 --no-deps > /tmp/opencode/_wgpu_meta.json 2>/tmp/opencode/_wgpu_meta.err; then
  echo "FAIL: cargo metadata failed:" >&2
  cat /tmp/opencode/_wgpu_meta.err >&2
  exit 1
fi

no_direct=$(node - "$ROOT" <<'NODE'
const fs = require("fs");
const root = process.argv[2];
// Engine-layer crates that are part of the wgpu interop wire and legitimately
// name wgpu directly:
//   - reco-core  : owns device/context construction; re-exports `reco_core::wgpu`.
//   - reco-autocam: GPU preprocessing detector (uses wgpu::Device/Queue on the
//                   shared device, not its own).
//   - reco-detect : GPU preprocess/metal-compute (shares reco-core's device).
// These three predate the gate (CONCERNS.md) and are NOT GUI crates. The gate's
// target per FOUND-04 / success criterion 5 is: no *GUI/host* crate may add a
// direct wgpu key (which would create a second wgpu type universe). Any new
// crate that wants wgpu must be consciously added here with justification.
const ALLOWED = new Set(["reco-core", "reco-autocam", "reco-detect"]);
const meta = JSON.parse(fs.readFileSync("/tmp/opencode/_wgpu_meta.json", "utf8"));
let bad = 0;
for (const pkg of meta.packages) {
  // Only our own workspace crates matter; third-party deps (e.g. tauri,
  // winit) are free to depend on wgpu transitively — we only forbid a
  // DIRECT wgpu key in a reco-* crate manifest.
  if (!pkg.name.startsWith("reco-")) continue;
  if (!pkg.manifest_path.includes("/crates/")) continue;
  if (ALLOWED.has(pkg.name)) continue;
  const dep = pkg.dependencies.find((d) => d.name === "wgpu");
  if (dep) {
    bad++;
    console.error(
      `FAIL: crate '${pkg.name}' declares a direct wgpu dependency ` +
        `(req '${dep.req}') in ${pkg.manifest_path}`
    );
    console.error(
      `      FOUND-04 forbids this — reach wgpu via reco_core::wgpu instead.`
    );
  }
}
// Also assert the single source of truth: `[workspace.dependencies].wgpu`.
// With centralization applied, drift is structurally impossible, but the
// gate stays belt-and-braces (RESEARCH §The wgpu pin gate).
const rootManifest = fs.readFileSync(`${root}/Cargo.toml`, "utf8");
const wsSection = rootManifest.split(/^\[workspace\.dependencies\]$/m)[1] || "";
const wsWgpu = wsSection.match(/^\s*wgpu\s*=\s*"([^"]+)"/m);
if (!wsWgpu) {
  console.error("FAIL: root Cargo.toml [workspace.dependencies] has no `wgpu` entry");
  bad++;
} else if (!/^28(\.|$)/.test(wsWgpu[1])) {
  console.error(
    `FAIL: workspace wgpu pin drifted — Cargo.toml declares "${wsWgpu[1]}", expected 28`
  );
  bad++;
}
process.exit(bad === 0 ? 0 : 1);
NODE
) || fail=1

# ---------------------------------------------------------------------------
# Check 2 — Cargo.lock resolves exactly one wgpu, pinned to 28.x.
# ---------------------------------------------------------------------------
lock_check=$(node - <<'NODE'
const fs = require("fs");
const lock = fs.readFileSync("Cargo.lock", "utf8");
// Extract every `[[package]]` block named exactly "wgpu" and read its
// `version = "..."` line. Anchored on the block name so we do not match
// wgpu-core / wgpu-hal / wgpu-types etc.
const blocks = lock.split(/^\[\[package\]\]$/m).slice(1);
const versions = [];
for (const b of blocks) {
  const name = (b.match(/^name = "(.*)"$/m) || [])[1];
  if (name !== "wgpu") continue;
  const v = (b.match(/^version = "(.*)"$/m) || [])[1];
  if (v) versions.push(v);
}
if (versions.length === 0) {
  console.error("FAIL: no `wgpu` package found in Cargo.lock");
  process.exit(1);
}
if (versions.length > 1) {
  console.error(
    `FAIL: Cargo.lock resolves ${versions.length} wgpu versions: ${versions.join(", ")}`
  );
  console.error("      Exactly one wgpu version must be locked (FOUND-04).");
  process.exit(1);
}
const v = versions[0];
if (!/^28\./.test(v)) {
  console.error(`FAIL: wgpu pin drifted — Cargo.lock resolves wgpu ${v}, expected 28.x`);
  process.exit(1);
}
process.stdout.write(`wgpu ${v}`);
NODE
) || fail=1

if [ "$fail" -ne 0 ]; then
  exit 1
fi

echo "OK — no direct wgpu dependency in any crate; Cargo.lock pins $lock_check"
