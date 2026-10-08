# Runtime prerequisites

This document is the single source of truth for what the Reco desktop app
(`reco-app`) needs **at runtime**, whether each prerequisite is **bundled** with
the installer, and how to install it when it is not. It is the companion to the
build-time prerequisites in [README.md](README.md#building) and to
`.planning/phases/01-walking-skeleton-native-surface-gate/01-CREDENTIALS.md`
(signing credentials, DIAG-04).

This file is also included in the app bundle (`bundle.resources` →
`RUNTIME-PREREQUISITES.md`), so the install instructions ship with the app on
every platform.

The in-app **System Info** panel runs the same checks at startup (the typed
preflight check, DIAG-05) and shows a remediation string per failed
prerequisite — this file documents what that panel reports.

## Prerequisite matrix

| Prerequisite | Platform | Why it is needed | Bundled with the installer? | Remediation |
|---|---|---|---|---|
| **FFmpeg shared libraries** (`libavcodec`, `libavformat`, `libavutil`, `libswscale`, `libavfilter`, `libavdevice`, `libswresample`) | Linux | Decode the camera clips and encode the stitched panorama. `reco-io` links these dynamically. | **Declared as package dependencies** — `bundle.linux.deb.depends` lists the FFmpeg runtime libraries the binary links (`libavcodec61`, `libavformat61`, `libavutil59`, `libswscale8`, `libswresample5`), so installing the `deb` pulls them in from the distribution. The AppImage target cannot express package dependencies, so AppImage users install them manually. | `deb` installs them automatically. Manual / AppImage: `sudo apt install libavcodec61 libavformat61 libavutil59 libswscale8 libswresample5` (Debian/Ubuntu) or `sudo dnf install ffmpeg-libs` (Fedora). |
| **FFmpeg DLLs** | Windows | Same decode/encode path as above. | **Yes** — the Windows NSIS/MSI installer and the `reco-app-<version>-windows-x86_64.zip` portable artifact both place the FFmpeg DLLs next to `reco-app.exe` (via `bundle.resources`), and ship FFmpeg's GPLv3 license text (`FFMPEG-LICENSE.txt`) plus a written source-offer notice (`FFMPEG-SOURCE-OFFER.txt`). | None for a released installer / portable ZIP. For a hand-built/dev copy, download [BtbN/FFmpeg-Builds](https://github.com/BtbN/FFmpeg-Builds/releases) (`n7.1 win64-gpl-shared`), extract, and place the DLLs next to `reco-app.exe`. |
| **FFmpeg libraries** | macOS | Same decode/encode path. | **No** — the app bundle does not embed FFmpeg dylibs. | `brew install ffmpeg`. |
| **ONNX Runtime** (`libonnxruntime`) | Linux / macOS / Windows | AI detection backends (camera control / subject tracking) when the `ort` feature is built. | **Statically linked — shipped inside the `reco-app` binary** for the default feature set (`ort`'s prebuilt CPU runtime is linked in; verified: no `libonnxruntime` `DT_NEEDED`, ONNX Runtime symbols embedded). No install and no package dependency. A `--features load-dynamic` build instead loads the library at runtime. | Default build: none required. `load-dynamic` build: download the matching [ONNX Runtime release](https://github.com/microsoft/onnxruntime/releases/tag/v1.24.4) and place `libonnxruntime.so` / `libonnxruntime.dylib` / `onnxruntime.dll` next to the app binary. |
| **WebView2 runtime** | Windows | The app UI is a webview; Windows ships no built-in webview. | **Bootstrapped** — `bundle.windows.webviewInstallMode = downloadBootstrapper`: the NSIS/MSI installer downloads Microsoft's WebView2 bootstrapper from the official endpoint and installs it if absent. | Already handled by the installer. Manual: [WebView2 Evergreen Bootstrapper](https://developer.microsoft.com/microsoft-edge/webview2/) (Microsoft download page). |
| **`webkit2gtk-4.1`** | Linux | The Tauri webview backend on Linux. | **Declared as a package dependency** — `bundle.linux.deb.depends = ["libwebkit2gtk-4.1-0", "libgtk-3-0", …]`, so the `deb` pulls it in. AppImage users install it manually. | `sudo apt install libwebkit2gtk-4.1-0 libgtk-3-0` (Debian/Ubuntu) or `sudo dnf install webkit2gtk4.1 gtk3` (Fedora). |
| **WKWebView** | macOS | The Tauri webview backend on macOS. | **Yes** — part of the OS (WebKit). | None required. |

## Notes

- **FFmpeg on Linux ships through package metadata, not as copied files.** The
  `deb` declares the exact runtime shared libraries the binary links in
  `bundle.linux.deb.depends`, so `dpkg`/`apt` installs them. This is the "shipped"
  mechanism for Linux (DIAG-05 / ROADMAP SC5); the libraries are never bundled as
  loose files. macOS installers do **not** bundle FFmpeg, while the Windows
  installers and the portable ZIP **do** (via `bundle.resources`) — see the table
  above.
- **ONNX Runtime ships inside the binary, not as a package dependency.** With
  the default feature set (`ort`, `download-binaries`) `ort-sys` statically
  links its prebuilt CPU runtime into `reco-app`, so there is nothing to install
  and no `deb` dependency to declare. AI detection is still "optional" in the
  sense that a build made with `--features load-dynamic` (or another backend)
  loads the library at runtime and degrades to "detection unavailable" when it
  is absent; the preflight remediation covers that case. (`release.yml`'s *CLI*
  artifacts copy a runtime next to the CLI; `release-app.yml` does not, and does
  not need to for the app.)
- **Linux package dependencies** are declared in
  `crates/reco-app/tauri.conf.json` under `bundle.linux.deb.depends`. The
  AppImage target cannot express package dependencies, so AppImage users must
  install `libwebkit2gtk-4.1-0` and the FFmpeg libraries themselves — this is
  the one distribution channel where the installer does **not** cover Linux
  prerequisites.
- **Soname → package mapping (ABI caveat).** The dependency names above are the
  Debian/Ubuntu runtime packages for the FFmpeg 7.x ABI this build links
  (`libavcodec.so.61` → `libavcodec61`, `libavformat.so.61` → `libavformat61`,
  `libavutil.so.59` → `libavutil59`, `libswscale.so.8` → `libswscale8`,
  `libswresample.so.5` → `libswresample5`; verified with `dpkg -S`). Because
  Debian/Ubuntu encode the ABI major version in the package name, a `deb` built
  against a different FFmpeg major version (for example the FFmpeg 6.x ABI on an
  older Ubuntu LTS) needs the corresponding `libavcodec60`/`libavformat60`/…
  names. Keep this list in sync with the FFmpeg ABI of the CI build runner;
  `scripts/verify-app-packaging.sh` asserts the declared names.
- **Preflight:** the runtime check implemented in `crates/reco-app/src/preflight.rs`
  (Phase 5, DIAG-05) verifies the FFmpeg libraries load, the ONNX Runtime dylib
  resolves (when detection is built), and the webview runtime is present,
  surfacing a **platform-specific** remediation string per failure in the System
  Info panel (`apt` on Linux, `brew` on macOS, the BtbN DLL step on Windows).

## Signing & distribution

Signed/notarized installers (macOS Developer ID + notarization, Windows
Authenticode) are produced by `.github/workflows/release-app.yml` **only when the
signing secrets exist**. With no secrets, the workflow produces Linux artifacts
with no signature and a macOS `.app`/`.dmg` that is **ad-hoc signed**
(`bundle.macOS.signingIdentity: "-"`) but **not** notarized — ad-hoc signing is
still a signature and is incompatible with notarization, so "no signature is
faked" means no *Developer ID* signature is fabricated, not that the macOS
artifact is entirely unsigned. The seven secret names and their acquisition lead
time are recorded in `01-CREDENTIALS.md` §4.
