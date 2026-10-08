# Building Reco (`reco-app`) on Windows

This guide builds the **Reco desktop app** (`reco-app`) release on Windows.
For runtime prerequisites in general (and per-platform remediation), see
[`RUNTIME-PREREQUISITES.md`](RUNTIME-PREREQUISITES.md). For build-time
prerequisites shared across platforms, see [`README.md`](README.md#building).

## What you'll produce

- `reco-app.exe` (the app)
- Installers: `Reco_<version>_x64-setup.exe` (NSIS) and
  `Reco_<version>_x64_en-US.msi` (MSI)
- Installer output: `target\release\bundle\nsis\` and `target\release\bundle\msi\`

> Building the **app** does *not* require libobs. The `reco-obs` workspace member
> is only needed for whole-workspace `cargo build` / `test` / `clippy`;
> `tauri build` compiles `reco-app` and its dependencies only.

## 1. One-time prerequisites

| Tool | Why | Notes |
|---|---|---|
| **Visual Studio 2022 Build Tools** | MSVC linker/toolchain (`x86_64-pc-windows-msvc`) | Select "Desktop development with C++" |
| **Rust (stable) 1.92+** | Workspace MSRV | `rustup default stable` |
| **Node.js 20+** (CI uses 22) | Builds the Svelte UI + runs the Tauri CLI | |
| **LLVM/Clang** | `ffmpeg-sys-next` uses bindgen (needs `libclang`) | Install to `C:\Program Files\LLVM` |
| **FFmpeg 7.x shared build** | Build-time headers/libs for `reco-io` | [BtbN/FFmpeg-Builds](https://github.com/BtbN/FFmpeg-Builds/releases) → `n7.1 win64-gpl-shared` |
| **WebView2 runtime** | The app UI is a webview | Win11 has it; the installer bootstraps it. For a dev/portable run on older Win10, install the Evergreen runtime |
| **Git** | Clone | |

**ONNX Runtime:** nothing to install. With the default `ort` feature, `ort-sys`
**statically links** its prebuilt CPU runtime into `reco-app` at build time (the
first build downloads it, so you need network access). No DLL to ship.

## 2. Point the build at FFmpeg

Extract the BtbN `win64-gpl-shared` archive (it contains `include\`, `lib\`,
`bin\`) and set these in the shell you'll build from:

```powershell
# Adjust to your extracted path
$ffmpeg = "C:\tools\ffmpeg-n7.1-latest-win64-gpl-shared-7.1"
$env:FFMPEG_DIR = $ffmpeg                          # headers + import libs (build time)
$env:LIBCLANG_PATH = "C:\Program Files\LLVM\bin"   # for bindgen
```

## 3. Build

From a **PowerShell** prompt at the repo root:

```powershell
cd crates\reco-app

# Install JS deps and build the webview bundle (frontendDist = ui/dist)
npm ci
npm run build

# Build the app + Windows installers (Tauri filters targets to the host OS → nsis + msi)
npx tauri build
```

That is the same sequence CI uses (`.github/workflows/release-app.yml`). To build
only one installer format:

```powershell
npx tauri build --bundles nsis
npx tauri build --bundles msi
```

**Portable exe only** (no installer):

```powershell
cargo build --release -p reco-app
# → target\release\reco-app.exe
```

## 4. Runtime prerequisite: FFmpeg DLLs (bundled in release artifacts)

`reco-io` links the FFmpeg shared libraries dynamically. The **release**
artifacts are self-contained: the installers (`Reco_<version>_x64-setup.exe` /
`Reco_<version>_x64_en-US.msi`) and the portable
`reco-app-<version>-windows-x86_64.zip` place the FFmpeg DLLs next to
`reco-app.exe` (via the CI-generated `bundle.resources` overlay), mirroring the
CLI `release.yml` packaging. FFmpeg's GPLv3 license text (`FFMPEG-LICENSE.txt`)
and a source-offer notice (`FFMPEG-SOURCE-OFFER.txt`) ship alongside them.

A **local/dev** portable build does *not* bundle them, so copy the DLLs from the
BtbN `bin\` folder next to `reco-app.exe`:

```
avcodec-*.dll  avformat-*.dll  avutil-*.dll  swscale-*.dll  swresample-*.dll
avfilter-*.dll avdevice-*.dll  (and any of their dependent DLLs)
```

- Installed app → already bundled; nothing to do.
- Local/dev portable build → copy them next to `target\release\reco-app.exe`.

If they are missing the app still starts; its **System Info** panel runs a
preflight check and shows the exact remediation (the same table as
`RUNTIME-PREREQUISITES.md`, which ships inside the bundle). WebView2 is handled
automatically by the installer (`webviewInstallMode = downloadBootstrapper`).

## 5. Code signing (optional)

Without a code-signing certificate the NSIS/MSI are **unsigned**, so Windows
SmartScreen warns on first run — fine for testing. To sign locally with an
Authenticode certificate:

```powershell
& "C:\Program Files (x86)\Windows Kits\10\bin\<ver>\x64\signtool.exe" sign `
  /fd SHA256 /tr http://timestamp.digicert.com /td SHA256 `
  /f C:\path\to\cert.pfx /p <password> `
  target\release\bundle\nsis\Reco_*_x64-setup.exe
```

CI signs automatically when the `WINDOWS_CERTIFICATE` /
`WINDOWS_CERTIFICATE_PASSWORD` secrets exist (gated, never faked). The seven
signing secrets are documented in
`.planning/phases/01-walking-skeleton-native-surface-gate/01-CREDENTIALS.md`.

## 6. Run / verify

```powershell
# Portable
.\target\release\reco-app.exe

# Installed
& "C:\Program Files\Reco\reco-app.exe"
```

On launch: **Import** two clips → **Calibrate** → **Preview** → **Export**. The
**System** screen shows GPU/backend/driver, available encoders (HW/SW), and the
preflight status.

## Troubleshooting

| Symptom | Fix |
|---|---|
| `ffmpeg-sys-next` can't find libs / `FFMPEG_DIR` errors | `FFMPEG_DIR` must point at the extracted FFmpeg root (with `include\` + `lib\`); re-open the shell after setting it |
| bindgen / `libclang` not found | Set `LIBCLANG_PATH` to `C:\Program Files\LLVM\bin` |
| `link.exe` not found | Install VS Build Tools "Desktop development with C++" |
| App starts but no video / decode errors | The release installer/portable ZIP bundles the FFmpeg DLLs next to `reco-app.exe`; a local/dev build must copy them there (step 4) |
| Blank window | Install/repair the WebView2 Evergreen runtime |
| First build very slow / network errors | The default `ort` feature downloads the prebuilt ONNX Runtime; ensure network access (or build `--no-default-features --features autocam,load-dynamic` to load ORT at runtime) |
| `npx tauri build` can't find the CLI | Run `npm ci` in `crates\reco-app` first (provides `@tauri-apps/cli`) |
| NSIS/MSI missing | Ensure `npx tauri build` ran from `crates\reco-app`; check `target\release\bundle\` |
