# Runtime prerequisites

This document is the single source of truth for what the Reco desktop app
(`reco-app`) needs **at runtime**, whether each prerequisite is **bundled** with
the installer, and how to install it when it is not. It is the companion to the
build-time prerequisites in [README.md](README.md#building) and to
`.planning/phases/01-walking-skeleton-native-surface-gate/01-CREDENTIALS.md`
(signing credentials, DIAG-04).

The in-app **System Info** panel runs the same checks at startup (the typed
preflight check, DIAG-05) and shows a remediation string per failed
prerequisite — this file documents what that panel reports.

## Prerequisite matrix

| Prerequisite | Platform | Why it is needed | Bundled with the installer? | Remediation |
|---|---|---|---|---|
| **FFmpeg shared libraries** (`libavcodec`, `libavformat`, `libavutil`, `libswscale`, `libavfilter`, `libavdevice`, `libswresample`) | Linux | Decode the camera clips and encode the stitched panorama. `reco-io` links these dynamically. | **Yes** — the Linux `deb` declares `Depends: libwebkit2gtk-4.1-0, libgtk-3-0`; FFmpeg libraries come from the distribution's FFmpeg runtime package. | `sudo apt install ffmpeg` (Debian/Ubuntu) or `sudo dnf install ffmpeg` (Fedora). |
| **FFmpeg DLLs** | Windows | Same decode/encode path as above. | **Yes** — the release workflow downloads the BtbN `n7.1 win64-gpl-shared` build and the installer ships the `bin\*.dll` set alongside the app. | If a custom build omits them: download [BtbN/FFmpeg-Builds](https://github.com/BtbN/FFmpeg-Builds/releases) (`n7.1 win64-gpl-shared`), extract, and place the DLLs next to `reco-app.exe`. |
| **FFmpeg libraries** | macOS | Same decode/encode path. | **Yes** — `brew install ffmpeg` provides the dylibs the release build links; the app bundle embeds what it links. | `brew install ffmpeg`. |
| **ONNX Runtime** (`libonnxruntime`) | Linux / macOS / Windows | AI detection backends (camera control / subject tracking) when the `ort` feature is built. Loaded via `dlopen` (`load-dynamic`) so a missing runtime degrades to "detection unavailable" rather than failing to launch. | **Yes** — the release workflow bundles ONNX Runtime **1.24.4** (DirectML on Windows, CPU elsewhere) next to the binary; the app resolves it from its own directory (`$ORIGIN` / `@loader_path`). | Download the matching [ONNX Runtime release](https://github.com/microsoft/onnxruntime/releases/tag/v1.24.4) (`onnxruntime-linux-x64-1.24.4.tgz`, `onnxruntime-osx-arm64-1.24.4.tgz`, or the Windows NuGet package) and place `libonnxruntime.so` / `libonnxruntime.dylib` / `onnxruntime.dll` next to the app binary. |
| **WebView2 runtime** | Windows | The app UI is a webview; Windows ships no built-in webview. | **Bootstrapped** — `bundle.windows.webviewInstallMode = downloadBootstrapper`: the NSIS/MSI installer downloads Microsoft's WebView2 bootstrapper from the official endpoint and installs it if absent. | Already handled by the installer. Manual: [WebView2 Evergreen Bootstrapper](https://developer.microsoft.com/microsoft-edge/webview2/) (Microsoft download page). |
| **`webkit2gtk-4.1`** | Linux | The Tauri webview backend on Linux. | **Not bundled** — declared as a package dependency: `bundle.linux.deb.depends = ["libwebkit2gtk-4.1-0", "libgtk-3-0"]`, so the `deb` pulls it in. AppImage users install it manually. | `sudo apt install libwebkit2gtk-4.1-0 libgtk-3-0` (Debian/Ubuntu) or `sudo dnf install webkit2gtk4.1 gtk3` (Fedora). |
| **WKWebView** | macOS | The Tauri webview backend on macOS. | **Yes** — part of the OS (WebKit). | None required. |

## Notes

- **"Bundled" means shipped by the installer**, not statically linked. FFmpeg
  and ONNX Runtime are distributed as shared libraries next to the app so the
  same binary works across distributions without a system install.
- **Linux package dependencies** are declared in
  `crates/reco-app/tauri.conf.json` under `bundle.linux.deb.depends`. The
  AppImage target cannot express package dependencies, so AppImage users must
  install `libwebkit2gtk-4.1-0` themselves — this is the one prerequisite the
  installer does **not** cover on Linux.
- **Detection is optional.** If ONNX Runtime is absent the app still launches;
  AI-assisted camera control reports itself unavailable. This is the honest
  degradation, never a fabricated result.
- **Preflight:** the runtime check implemented in `crates/reco-app/src/preflight.rs`
  (Phase 5, DIAG-05) verifies the FFmpeg libraries load, the ONNX Runtime dylib
  resolves (when detection is built), and the webview runtime is present,
  surfacing a remediation string per failure in the System Info panel.

## Signing & distribution

Signed/notarized installers (macOS Developer ID + notarization, Windows
Authenticode) are produced by `.github/workflows/release-app.yml` **only when the
signing secrets exist**; with no secrets the workflow produces unsigned Linux
artifacts. The seven secret names and their acquisition lead time are recorded in
`01-CREDENTIALS.md` §4. No signature is faked.
