---
last_mapped_commit: cd3bf434b92c678a5585cd9be2330eda49782a8a
last_mapped_at: 2026-10-01
---
# Codebase Concerns

**Analysis Date:** 2026-10-01

## Tech Debt

**`reco-gui` monolithic main file:**

- Issue: `crates/reco-gui/src/main.rs` is 4,689 lines — 74% of the entire `reco-gui` crate (6,330 lines) in one file. UI callbacks, export orchestration, playback state, calibration, and settings wiring all live here.
- Files: `crates/reco-gui/src/main.rs`
- Impact: Any UI change risks merge conflicts and requires compiling the whole crate; no unit seam for most logic (only 11 `#[test]`s in the crate, none in `main.rs`).
- Fix approach: Extract into modules under `crates/reco-gui/src/` following the existing `export.rs` / `playback.rs` / `preview.rs` / `settings.rs` split. Prioritize the export/calibration callback blocks.

**Vendored AKAZE detector carries `#![allow(dead_code)]`:**

- Issue: `crates/reco-calibrate/src/akaze/mod.rs:1` disables dead-code warnings for the whole vendored subtree (1,567 lines across 10 files). The crate-wide clippy `-D warnings` policy is locally defeated, so unused paths accumulate silently.
- Files: `crates/reco-calibrate/src/akaze/*.rs`
- Impact: Dead/unreachable branches stay compiled; future changes can't rely on the lint gate.
- Fix approach: Remove `#![allow(dead_code)]`, delete genuinely unused code, and shrink the vendored surface to the modules the fix set actually uses.

**Transitional dual Metal API surface:**

- Issue: `crates/reco-core/Cargo.toml` pulls both `metal = "0.33"` (wgpu-hal 28's dependency) and `objc2-metal = "0.3"` (CoreML path); the comment states the objc2-metal path reverts once wgpu 29 moves back. Two parallel Metal bindings are in the tree at once.
- Files: `crates/reco-core/Cargo.toml`, `crates/reco-core/src/interop/metal.rs`, `crates/reco-detect/src/metal_compute.rs`, `crates/reco-detect/src/coreml_inference.rs`
- Impact: Duplicated unsafe FFI + duplicated review burden; conflicts likely on the wgpu 29 bump.
- Fix approach: Track the wgpu 29 migration; collapse to a single Metal binding and delete the alternate path.

**`wgpu` is not a workspace dependency — three independent `"28"` pins:**

- Issue: `crates/reco-core/Cargo.toml:16`, `crates/reco-autocam/Cargo.toml:19`, and `crates/reco-detect/Cargo.toml:29` each declare `wgpu = "28"` (only `reco-core` adds features). A future bump can update one crate and not others.
- Files: `crates/reco-core/Cargo.toml`, `crates/reco-autocam/Cargo.toml`, `crates/reco-detect/Cargo.toml`
- Impact: Silent version drift; Cargo.lock currently resolves a single wgpu 28.0.0 but the declaration doesn't enforce it.
- Fix approach: Add `wgpu` to `[workspace.dependencies]` in `/Cargo.toml` and switch all three to `wgpu = { workspace = true, features = [...] }`.

**`libloading` resolves as a 3-way duplicate (0.7.4 / 0.8.9 / 0.9.0):**

- Issue: `Cargo.lock:3518,3528,3538` carry three major versions. `deny.toml:129-132` documents the intent to track this, but `skip-tree = []` is empty and the comment names an exception that is not actually recorded.
- Files: `Cargo.lock`, `deny.toml:129-132`, `crates/reco-core/Cargo.toml:31`, `crates/reco-detect/Cargo.toml:28,39`
- Impact: Bloat and a stale comment that misleads reviewers into thinking an exception exists.
- Fix approach: Either add the documented `skip-tree` entry or remove the comment; run `cargo tree -i libloading` to confirm the transitive pullers (wgpu, nvml-wrapper).

**Placeholder / stub modules shipped behind features:**

- Issue: `reco-control`'s `gopro` module is documented as a placeholder (`crates/reco-control/src/lib.rs:28`) and the README states the transports are `todo!()` (`crates/reco-control/README.md:17`). `crates/reco-core/src/source.rs:464,501` describes "placeholder impls" for `CameraInput`. `crates/reco-detect/src/detectors/mod.rs:113` is a "TEMPORARY adapter".
- Files: `crates/reco-control/src/lib.rs`, `crates/reco-control/README.md`, `crates/reco-core/src/source.rs`, `crates/reco-detect/src/detectors/mod.rs`
- Impact: The public API surface advertises capability that may not exist; consumers can select a feature that returns a stub.
- Fix approach: Gate stubs behind `unimplemented` docs/cfg or remove features from the published surface until implemented.

**CPU-only stacked-video pack + no hardware encode path:**

- Issue: `crates/reco-io/src/stacked_video/mod.rs:45-50` documents a CPU per-row `memcpy` pack (~8 ms for 4K vstack) as "good enough"; `crates/reco-io/src/stacked_video/encoder.rs:11` says the NV12 hardware pack variant is "not implemented in this cut".
- Files: `crates/reco-io/src/stacked_video/mod.rs`, `crates/reco-io/src/stacked_video/encoder.rs`
- Impact: Replay recording spends CPU on a side-path while the GPU sits idle; forces a software encoder, contending with the main stitch encode.
- Fix approach: Implement the wgpu render-target atlas path described in the module docs and feed the GPU-resident encoder.

**`smart_source` placeholder bind-group setup:**

- Issue: `crates/reco-io/src/smart_source.rs:405-412` contains a multi-line "Build bind groups placeholder … For now, let's store the textures and bufs directly" comment; bind groups are deferred to `configure_gpu_source()`.
- Files: `crates/reco-io/src/smart_source.rs`
- Impact: The zero-copy smart path has an implicit two-phase contract (textures stored, bind groups later); violating call order fails at run time.
- Fix approach: Make `SharedTextureSet` construction return a type that can only be completed into bind groups, or build them eagerly.

**Stale doc comments that contradict the tree:**

- Issue: `rust-toolchain.toml` warns to update "three rogue per-crate pins - M8 polish", but every crate now uses `rust-version.workspace = true` — the pins are gone. `deny.toml:129-132` describes a libloading exception that `skip-tree = []` does not encode.
- Files: `rust-toolchain.toml`, `deny.toml:129-132`
- Impact: Reviewers chase resolved/nonexistent debt; trust in the comments erodes.
- Fix approach: Delete the resolved note; make the deny.toml comment match the actual config.

**`cuda_kernels` uses nearest-neighbour sampling where bilinear is intended:**

- Issue: `crates/reco-detect/src/cuda_kernels.rs:365` — `// map to source coords (nearest for now, bilinear TODO)`.
- Files: `crates/reco-detect/src/cuda_kernels.rs`
- Impact: Detection preprocessing quality differs from the CPU/ORT paths, causing backend-dependent accuracy.
- Fix approach: Implement bilinear sampling or document the accuracy delta as accepted.

**`reco-core` "for now" behavior:**

- Issue: `crates/reco-core/src/core/render.rs:210` — BGRA submits tick the director with the last (stale) frame position "For now".
- Files: `crates/reco-core/src/core/render.rs`
- Impact: On BGRA paths the director receives stale tracking, which can lag raw panning.
- Fix approach: Pass the current BGRA frame's timestamp/pose through the director tick.

## Known Bugs

**Constrained-look regression from manual `rig_tilt` threading (`N15`):**

- Symptoms: Constrained-look view can ignore clamps; consumers must pass `rig_tilt` to both `clamp_via_coverage` and `render_pose` every tick.
- Files: `crates/reco-gui/FRICTION.md:33-37`, `crates/reco-control/src/pose_control.rs`, `crates/reco-core/src/render/renderer.rs`
- Trigger: Any tick where `rig_tilt` is not threaded to both calls.
- Workaround: Thread the value both places; best documented in the FRICTION entry. Real fix: renderer owns the pose state machine.

**Recording lags preview and drops frames during panning (`N16`):**

- Symptoms: NV12 readback runs on the UI thread and stalls the Slint compositor, so recording falls behind preview while panning.
- Files: `crates/reco-gui/FRICTION.md:39-43`, `crates/reco-gui/src/main.rs`, `crates/reco-gui/src/export.rs`
- Trigger: Recording enabled while panning.
- Workaround: None; the fix is to move recording to a background `StitchSession`.

**FOV applied via cached push, not a render parameter (`N19`):**

- Symptoms: A pose change outside the smoothing tick (e.g. re-enabling constrained look clamps `current_fov` directly) updates yaw/pitch live but leaves the cached FOV stale — slider shows the clamp, view ignores it.
- Files: `crates/reco-gui/FRICTION.md:56-65`
- Trigger: Out-of-tick clamp / constrained-look toggle.
- Workaround: Push FOV after any out-of-tick clamp. Fix: make FOV a `render_yuv` parameter.

**fps-probe fallback indistinguishable from a real 30fps source (`N20`):**

- Symptoms: `VideoDecoder::frame_rate()` logs an error then returns `Rational(30, 1)` — identical to a genuine 30fps file, so export timing (speed, trim) can be silently wrong.
- Files: `crates/reco-gui/FRICTION.md:67-74`, `crates/reco-io/src/ffmpeg/decoder.rs:1323`
- Fix approach: Add an `fps_is_estimated` provenance flag to `SourceInfo`.

**OBS input dimension mismatch silently freezes output (`A7` / `A22`):**

- Symptoms: Declared (not detected) input W/H; a mismatch between properties and actual frames silently freezes the output instead of erroring.
- Files: `crates/reco-obs/FRICTION.md:26-29,91-95`, `crates/reco-obs/src/source.rs`
- Workaround: Manually keep properties in sync; consumer-side rebuild is not automatic. Fix: auto-detect from first frame and rebuild pipeline.

**Visible upstream OBS sources steal async frames (`A11`):**

- Symptoms: When an upstream source is visible in the scene, its renderer consumes the async frame before the plugin's poller can read it; output freezes until the source is hidden.
- Files: `crates/reco-obs/FRICTION.md:42-46`, `crates/reco-obs/src/source.rs`
- Workaround: Hide the source via the eye icon. Fix: auto-hide on pick.

## Security Considerations

**Large unsafe FFI surface across GPU backends:**

- Risk: 408 `unsafe` occurrences across 31 files, concentrated in `crates/reco-core/src/interop/{cuda,d3d11,metal,dmabuf,vulkan}.rs`, `crates/reco-core/src/nvbuf_transform.rs`, `crates/reco-io/src/ffmpeg/{decoder,encoder}.rs`, `crates/reco-io/src/gstreamer/*`, `crates/reco-io/src/v4l2.rs`, and all of `crates/reco-obs/src/source.rs`. Any raw-pointer misuse is memory-unsafe.
- Files: as listed above
- Current mitigation: `unsafe` is scoped to `unsafe {}` blocks and FFI boundaries; decoder avoids `transmute` via a `union` (`crates/reco-io/src/ffmpeg/decoder.rs:1269-1281`); CI builds the CUDA/TensorRT/NCNN/CoreML backends compile-only (`rust.yml` `gpu-backends`).
- Recommendations: Add per-BLOCK `// SAFETY:` justification comments (not just module notes) and run Miri/ASan on the pure-Rust paths; wire the existing `fuzz/` targets into CI.

**`unsafe impl Send`/`Sync` on raw FD/pointer holders:**

- Risk: ~30 impls assert thread-safety for types wrapping FFI handles — e.g. `crates/reco-io/src/ffmpeg/decoder.rs:232` (`VideoDecoder`), `154` (`D3d11Frame`), `1114` (`SharedHwDevice`); `crates/reco-io/src/ffmpeg/encoder.rs:567-570`; `crates/reco-detect/src/detectors/trt/engine.rs:109,254`; `crates/reco-core/src/interop/cuda.rs:279-280,845-846`. A wrong assertion is UB that tests typically won't catch.
- Files: see list; full inventory via `grep -rn "unsafe impl" crates`
- Current mitigation: These types stay within single-threaded pipeline ownership in most cases.
- Recommendations: Document the invariant that makes each `Send`/`Sync` sound and confirm whether `Sync` (not just `Send`) is needed, especially `CudaFunctions`/`CudaKernel`/`NppFunctions`.

**OBS raw-pointer source callbacks:**

- Risk: `crates/reco-obs/src/source.rs` implements `unsafe extern "C"` callbacks (`source_create`, `source_destroy`, `source_video_tick`, `source_mouse_*`) that dereference `*mut c_void` data and `*mut obs_source_t`; `crates/reco-obs/src/source.rs:1345,1380-1392` build slices from raw `frame.data` pointers. A null/stale pointer from OBS is UB.
- Files: `crates/reco-obs/src/source.rs`, `crates/reco-obs/src/ffi.rs`, `crates/reco-obs/src/lib.rs`
- Current mitigation: `set_source_slot` null-checks and releases frames (`source.rs:278-279,611-624,780-783`); panic-catching callbacks exist (`source.rs:320-348`).
- Recommendations: Audit every `from_raw_parts` for length/stride and null assumptions; covered by only 6 `#[test]`s in the crate.

**Calibration / model / path parsers are attacker-facing:**

- Risk: Calibration JSON, ONNX `names` metadata, and input paths are user-controlled. Prior defects (documented in fuzz headers) include an OOM `Vec::with_capacity` from `{999999999: 'ball'}` and unbounded parse.
- Files: `fuzz/fuzz_targets/calibration_json.rs`, `fuzz/fuzz_targets/onnx_names.rs`, `fuzz/fuzz_targets/input_path.rs`, `crates/reco-core/src/calibration.rs`, `crates/reco-detect/src/`
- Current mitigation: `MAX_CALIBRATION_FILE_SIZE` cap (`fuzz_targets/calibration_json.rs`), N-C1 names cap, `validate_input_path` structured rejection.
- Recommendations: Run these targets in CI on a schedule; add an OsStr-based path target (noted as a gap in `fuzz_targets/input_path.rs`) and an FFmpeg/frame-decode target.

## Performance Bottlenecks

**UI-thread NV12 readback stalls the compositor:**

- Problem: Per-frame readback on the Slint UI thread (`N16`).
- Files: `crates/reco-gui/src/main.rs`, `crates/reco-gui/FRICTION.md:39-43`
- Cause: Preview consumes GPU readback synchronously on the UI thread.
- Improvement path: Move to a background `StitchSession` with a bounded frame channel.

**CPU stacked-video pack (~8 ms/frame at 4K):**

- Problem: `memcpy` pack on the CPU for replay recording.
- Files: `crates/reco-io/src/stacked_video/mod.rs:45-50`
- Cause: No wgpu atlas path implemented.
- Improvement path: Compute/render-target atlas keeping frames GPU-resident.

**OBS CPU roundtrip (~8 MB/frame at 1080p):**

- Problem: OBS (OpenGL/D3D11) ⇄ wgpu interop goes through CPU because there is no shared-texture path.
- Files: `crates/reco-obs/FRICTION.md:7-12`
- Cause: Platform interop (DMA-BUF / shared D3D11) not implemented.
- Improvement path: Add Linux DMA-BUF and Windows shared-D3D11 interop; this is a fundamental architectural cost until then.

**Redundant container open on export:**

- Problem: `FfmpegFileSource` opens the full container just to read `total_frames`, so export performs two muxer opens (probe + read).
- Files: `crates/reco-gui/FRICTION.md:14-17`, `crates/reco-io/src/ffmpeg/decoder.rs`
- Cause: No lightweight `reco_io::probe_duration(path)`.
- Improvement path: Add a metadata-only probe.

**Long silent calibration steps (20-60s):**

- Problem: AKAZE on dense scenes, optimizer on 100+ pairs, and audio PCM extraction can run with no heartbeat, so consumers can't distinguish progress from a hang.
- Files: `crates/reco-gui/FRICTION.md:8-12`, `crates/reco-calibrate/src/`, `crates/reco-io/src/`
- Improvement path: Emit intra-step progress/heartbeat events.

## Fragile Areas

**`frame_processing` relies on unwrap/expect-encoded invariants in the hot path:**

- Files: `crates/reco-core/src/session/frame_processing.rs:195,409,455,568,583,601,612,800,823`
- Why fragile: `.as_ref().unwrap()` on `d3d11_staging_pool`, `metal_texture_cache`, and `vram_pool`, plus `.expect("vram_pool must exist when current_vram_slot is set")`, are guarded only by `is_some()`/slot-state invariants established earlier in the same function. Any reordering or early-return breaks them as panics in the render loop.
- Safe modification: Convert to `ok_or_else(|| SessionError::...)` or carry the pool handle in the matched variant so the compiler enforces presence.
- Test coverage: GPU paths are `#[ignore]` in CI (`crates/reco-core/src/session/tests.rs:279,307,373,417`), so these guards are not exercised by default CI.

**`run_loop` queue pop relies on length arithmetic:**

- Files: `crates/reco-core/src/session/run_loop.rs:497` (`.unwrap()` after `pose_queue.len() > post_smooth_half`)
- Why fragile: Correctness depends on the `len()` guard and `post_smooth_half` never being adjusted between the check and the pop. A refactor that changes smoothing math can turn this into a panic.
- Safe modification: Use `if let Some(...) = pose_queue.pop_front()`.

**Platform interop is cfg-gated and unevenly tested:**

- Files: `crates/reco-core/src/interop/{cuda,d3d11,metal,dmabuf,vulkan}.rs`, `crates/reco-io/src/gstreamer/*`, `crates/reco-io/src/v4l2.rs`
- Why fragile: `target_os` gating is heavily skewed (`linux` 75, `windows` 63, `macos` 14 occurrences across the tree). Windows `reco-gui` is deliberately excluded from CI ("Deferred to M8", `rust.yml` `check-windows`), `reco-obs` builds only on Linux in CI, and Intel macOS is suspended. Platform code can rot undetected.
- Safe modification: Prefer shared abstractions over new `cfg` branches; add a compile-check job before touching platform code.
- Test coverage: The macOS path has no runtime CI beyond `cargo check -p reco-core` / `-p reco-gui`.

**Mutex-poisoning panics:**

- Files: `crates/reco-gui/src/main.rs:798,830,1465,3497`, `crates/reco-gui/src/export.rs:214`, `crates/reco-core/src/detect/pipeline_event.rs:303`, `crates/reco-control/src/intent_translator.rs:238,243`
- Why fragile: `lock().unwrap()` propagates a panic if any thread panicked while holding the lock, taking down the app/thread instead of recovering.
- Safe modification: Use `unwrap_or_else(|e| e.into_inner())` where stale state is acceptable, or handle poisoning explicitly.

**`yuv_stack_packer` can panic "never produced a frame after 8 submits":**

- Files: `crates/reco-core/src/gpu/yuv_stack_packer.rs:892` (panic in test), tests ignored at `:899,951,1031`
- Why fragile: The packer uses a fixed 8-submit spin; on a slow/software adapter this can fail, and the only coverage is `#[ignore]`d.
- Safe modification: Validate with a real GPU before touching submit sequencing.

## Scaling Limits

**OBS replay rolling buffer memory:**

- Current capacity: Estimated ~7 GB at 1080p/30fps/30s (`crates/reco-obs/FRICTION.md:73-74`).
- Limit: RAM-bound; a 4K/60fps or longer window scales linearly and can OOM the OBS host.
- Scaling path: Encoded (not raw) rolling buffer or configurable window with hard cap.

**VRAM pool fixed at two decode slots per eye:**

- Files: `crates/reco-core/src/session/vram_pool.rs` (module carries `#![allow(dead_code)]` at line 9), `crates/reco-core/src/session/frame_processing.rs:187-188` (`frame_count % 2`, `left_slot + 2`)
- Limit: Only 2 in-flight frames per source; higher input resolutions/FPS or deeper pipelines can starve.
- Scaling path: Make slot count configurable and validate against adapter memory.

## Dependencies at Risk

**`wgpu 28` pinned with an unresolved interop SPIKE:**

- Risk: `Cargo.toml` root comment records "SPIKE: wgpu 28 interop with Slint … if wgpu 28 doesn't have the shaderDrawParameters regression, no patch is needed." The Slint backend also can't expose `AdapterInfo` (`crates/reco-gui/FRICTION.md:19-21`).
- Impact: Blocked preview/diagnostic features and a fork risk on upgrade.
- Migration plan: Resolve before wgpu 29; centralize the pin as a workspace dependency.

**`ort 2.0.0-rc.12` is a pre-release:**

- Risk: `crates/reco-detect/Cargo.toml:26` — release-candidate API can break on the 2.0 final.
- Impact: ONNX Runtime backends (CPU/GPU/CoreML) need re-validation on every rc bump.
- Migration plan: Track ORT 2.0 stable and pin explicitly.

**`ffmpeg-sys-next 8.1.0` links the system FFmpeg:**

- Risk: `Cargo.lock:1801`; API/ABI follows the installed FFmpeg 7.1 dev libs. Windows requires vcpkg-provisioned FFmpeg, which is why Windows `reco-gui` is out of CI (`rust.yml` `check-windows`).
- Impact: Environment-sensitive builds; the FFmpeg version deployed must match the linked one.
- Migration plan: Document the supported FFmpeg range and pin/mirror prebuilts per platform.

**Git-pinned calibration dependency:**

- Risk: `deny.toml:142-148` allows `telemetry-parser` and two transitive git repos (`mp4parse-rust`, `fc-blackbox`) pinned by rev.
- Impact: Supply-chain surface outside crates.io; rev pins must be manually refreshed.
- Migration plan: Keep `deny.toml` sources allowlist authoritative and review on upstream changes.

**`libloading` 3-way duplicate (see Tech Debt):**

- Risk: Version skew between `reco-core` (0.9) and transitive pulls (0.7/0.8).
- Impact: Binary bloat; potential divergent symbol loading.
- Migration plan: Converge transitive deps or record the accepted `skip-tree`.

## Missing Critical Features

**OBS integration gaps:**

- Problem: No wgpu/OBS GPU interop (`A3`), no detector/director hooks on the push API so AI tracking can't run in OBS (`A10`/`A17`), no live calibration for non-file sources (`A14`), no auto-resize on dimension change (`A7`/`A22`), no constrained-look toggle (`A13`), no keyboard/controller pan-zoom (`A12`), no built-in replay mode (`A16`).
- Files: `crates/reco-obs/FRICTION.md`, `crates/reco-obs/src/source.rs`
- Blocks: Real-time AI framing and reliable live operation in OBS.

**GUI ROI editing:**

- Problem: Calibration produces `field_roi` but the GUI never displays or lets users adjust it; users must read raw JSON to verify.
- Files: `crates/reco-gui/FRICTION.md:45-49`, `crates/reco-gui/src/main.rs`, `crates/reco-gui/src/settings.rs`
- Blocks: Self-service ROI correction and trust in calibration output.

**Hardware-encoded stacked replay output:**

- Problem: `StackedEncoder` forces a software encoder; the NV12 hardware pack variant is unimplemented.
- Files: `crates/reco-io/src/stacked_video/encoder.rs:11`
- Blocks: GPU-efficient, higher-resolution replay recording.

**Calibration progress feedback:**

- Problem: No intra-step heartbeat during 20-60s steps (`A5-residual`).
- Files: `crates/reco-gui/FRICTION.md:8-12`
- Blocks: Users can't tell a slow calibration from a hung process.

## Test Coverage Gaps

**`reco-cli` has zero `#[test]`:**

- What's not tested: The `reco stitch` / `info` / `calibrate` / `preview` command paths (1,154 lines in `main.rs`, plus `camera.rs` 916 and `preview.rs` 943).
- Files: `crates/reco-cli/src/`
- Risk: CLI regressions (arg parsing, output naming, exit codes) only surface manually.
- Priority: High.

**GPU-dependent tests are `#[ignore]`d and skip in CI:**

- What's not tested: Render/packer/session GPU paths and the KB4 wgsl-vs-Rust parity test.
- Files: `crates/reco-core/src/gpu/yuv_stack_packer.rs:899,951,1031`, `crates/reco-core/src/session/tests.rs:279,307,373,417`, `crates/reco-core/src/lens/mod.rs:243`, `crates/reco-io/tests/stacked_video_roundtrip.rs:92`, `crates/reco-io/tests/calibration_io.rs:18,40,63`, `crates/reco-calibrate/tests/calibrate_videos.rs:23`
- Risk: The core render path has no default-CI execution evidence.
- Priority: High — add a GPU runner or a software-adapter (llvmpipe/lavapipe) job.

**Thin coverage in consumer crates:**

- What's not tested: `reco-gui` (11 `#[test]`s across 6,330 lines), `reco-obs` (6 across 2,000+), and no tests on the Slint callback wiring or OBS FFI callbacks.
- Files: `crates/reco-gui/src/`, `crates/reco-obs/src/`
- Risk: UI/OBS integration bugs reach release; `reco-obs` FFI is also the most unsafe code.
- Priority: High.

**No benchmark targets, despite a CI bench job:**

- What's not tested: `cargo bench --workspace --no-run` (`rust.yml` `bench-build`) compiles test targets; there are no `benches/` directories, so no performance regression signal exists for the real-time frame budget.
- Files: workspace-wide
- Risk: Performance regressions in the render/stitch loop go unmeasured.
- Priority: Medium.

**Platform-gated code not exercised:**

- What's not tested: Windows `reco-gui` (excluded from CI), `reco-obs` on Windows/macOS, Intel macOS suspended, and the ARM64/Android jobs are compile-only.
- Files: `.github/workflows/rust.yml`
- Risk: Platform breakage found only at release time.
- Priority: Medium.

**Fuzz targets are not wired into CI:**

- What's not tested: The three `fuzz/fuzz_targets/*.rs` parsers run only manually (`fuzz/` is outside the workspace).
- Files: `fuzz/`, `.github/workflows/rust.yml`
- Risk: Regressions in the hardened parsers (calibration JSON, ONNX names, input paths) go unnoticed.
- Priority: Medium.

---

*Concerns audit: 2026-10-01*
