---
last_mapped_commit: cd3bf434b92c678a5585cd9be2330eda49782a8a
last_mapped_at: 2026-10-01
---
# Testing Patterns

**Analysis Date:** 2026-10-01

## Test Framework

**Runner:** Rust's built-in test harness (`cargo test`). No external runner, no async runtime in tests.

**Assertion library:** std `assert!` / `assert_eq!` / `assert_ne!` plus `approx::assert_abs_diff_eq!` for float comparisons (dev-dependency `approx = "0.5"` in `crates/reco-core/Cargo.toml` and `crates/reco-calibrate/Cargo.toml`).

**Not used:** no `tokio`/`async-std` runtime, no `mockall`, no `proptest`/`quickcheck`, no `rstest`, no `insta` snapshot tests, no `criterion` benchmarks. Do not introduce these without a project decision.

**Dev dependencies observed:**

- `approx = "0.5"` — float comparison (`crates/reco-core`, `crates/reco-calibrate`)
- `tempfile = "3"` — temp files/dirs (`crates/reco-core`); used in `crates/reco-io` via `tempfile::Builder` (`crates/reco-io/src/ffmpeg/decoder.rs:513`)
- `env_logger = "0.11"` — included as a dev-dep but logging init is now via `tracing_subscriber`
- `pollster = "0.4"` and `reco-io = { path = "../reco-io" }` (`crates/reco-calibrate`)
- `serde = { version = "1", features = ["derive"] }` (`crates/reco-io`)

**Run commands:**

```bash
cargo test --workspace                       # All tests, default features
cargo test --workspace --features profiling  # CI also runs this
cargo test --all                             # Equivalent, per AGENTS.md
cargo test -p reco-core -- --ignored         # Run GPU-gated tests explicitly
cargo test -p reco-core -- --nocapture       # Show log/print output
```

## Test File Organization

**Location:** The dominant pattern is **co-located in-module tests**: a `#[cfg(test)] mod tests { ... }` block at the bottom of the source file it tests. 63 `src` files carry one, holding 397 of the 414 total `#[test]` functions. This is required by `AGENTS.md` ("Tests in each module (`#[cfg(test)] mod tests`)").

**Per-crate `#[test]` counts:**

| Crate | Tests | In-module files | GPU/ignored |
|-------|-------|-----------------|-------------|
| `reco-core` | 158 | 26 | 9 |
| `reco-autocam` | 67 | 7 | 0 |
| `reco-calibrate` | 53 | 9 | 1 |
| `reco-io` | 52 | 7 | 5 |
| `reco-control` | 35 | 4 | 0 |
| `reco-detect` | 32 | 5 | 0 |
| `reco-gui` | 11 | 3 | 0 |
| `reco-obs` | 6 | 2 | 0 |
| `reco-cli` | 0 | 0 | 0 |

**Separate test module file:** `crates/reco-core/src/session/tests.rs` holds the `StitchSession` frame-loop tests and is pulled in with `mod tests;` (`crates/reco-core/src/session/mod.rs:41`). Note this is **not** a `#[cfg(test)]` line — the module declaration is plain; tests are compiled into the normal build path for that module. Follow the co-located convention for new work; reach for a separate `tests.rs` only when a module's test body is large.

**Integration tests:** four files under `crates/*/tests/` (17 `#[test]` fns total), used for cross-module or hardware-dependent flows:

- `crates/reco-calibrate/tests/calibrate_videos.rs`
- `crates/reco-calibrate/tests/regression.rs`
- `crates/reco-io/tests/calibration_io.rs`
- `crates/reco-io/tests/stacked_video_roundtrip.rs`

All integration files open with a `//!` header stating what they test and how to run them.

**Naming:** test files are `snake_case.rs` and describe the unit under test (`stacked_video_roundtrip.rs`, `calibration_io.rs`).

## Test Structure

**Suite organization** — the canonical in-module shape:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    struct NoopEncoder;
    impl Encoder for NoopEncoder {
        fn submit(&mut self, _f: OutputFrame<'_>) -> Result<(), EncodeError> {
            Ok(())
        }
        fn finish(&mut self) -> Result<(), EncodeError> {
            Ok(())
        }
    }

    #[test]
    fn counts_encoded_frames() {
        let mut t = AsyncEncodeThread::new(Box::new(NoopEncoder), 16, 16, 2);
        let data = vec![0u8; 16 * 16 * 3 / 2];
        for i in 0..8 {
            t.submit(&data, i).unwrap();
        }
        t.finish().unwrap();
        let (frames, avg_ms, _bp, _bp_ms) = t.stats();
        assert_eq!(frames, 8);
        assert!(avg_ms.is_finite() && avg_ms >= 0.0);
    }
}
```

(`crates/reco-core/src/async_encode.rs`)

**Naming conventions:**

- `use super::*;` at the top of the module — gives tests access to private items.
- Test function names are **descriptive snake_case without a `test_` prefix**, phrased as the assertion: `counts_encoded_frames`, `validate_zero_fx_fails`, `pack_unpack_is_byte_perfect`, `to_3d_x_plane_maps_correctly`. Prefer `<unit>_<condition>_<expected>`.
- **Helpers are grouped under a divider comment**:
  ```rust
  // ─── Helpers ───────────────────────────────────────────────────────────
  ```
  (`crates/reco-core/src/session/tests.rs:26`)
- **Test fixtures are factory functions**, not fixtures files: `fn test_calibration() -> MatchCalibration`, `fn make_detection(...) -> Detection`, `fn synthetic_tile(frame_idx, tile_idx) -> YuvFrame`, `fn make_tensor(rows: &[[f32; 6]]) -> Vec<f32>`. Examples in `crates/reco-core/src/detect/panner.rs:236`, `crates/reco-autocam/src/roi_filter.rs:184`, `crates/reco-io/tests/stacked_video_roundtrip.rs:34`, `crates/reco-detect/src/detectors/mod.rs:242`.
- For GPU tests, define small dimensions as module consts to keep allocations cheap:
  ```rust
  /// Small test dimensions to keep GPU allocations minimal.
  const W: u32 = 64;
  const H: u32 = 64;
  ```
  (`crates/reco-core/src/session/tests.rs`)

**Assertion patterns:**

- Plain `assert!`/`assert_eq!` preferred (481 and 390 uses respectively).
- **Always add a message to non-trivial assertions**, including tolerances:
  ```rust
  assert!(
      (got.camera_axis_offset - exp.camera_axis_offset).abs() < tol,
      "camera_axis_offset: got {}, expected {} (tol {tol})",
      got.camera_axis_offset,
      exp.camera_axis_offset,
  );
  ```
  (`crates/reco-calibrate/tests/calibrate_videos.rs`)
- Float comparisons use `assert_abs_diff_eq!` with an explicit `epsilon`:
  ```rust
  use approx::assert_abs_diff_eq;
  assert_abs_diff_eq!(p.x, 0.3, epsilon = 1e-10);
  ```
  (`crates/reco-calibrate/src/geometry.rs:498`)
- `#[should_panic(expected = "...")]` is used sparingly and must name the expected message: `#[should_panic(expected = "push called on full buffer")]` (`crates/reco-core/src/session/frame_buffer.rs:169`).

## Mocking

**No mocking framework.** Dependencies are mocked by **implementing the crate's own traits with hand-written test doubles**, typically a `Noop*` or `Fake*` struct local to the test module.

Example — a no-op `Encoder` and, for `StitchSession`, hand-written fake detectors/sources:

```rust
struct NoopEncoder;
impl Encoder for NoopEncoder {
    fn submit(&mut self, _f: OutputFrame<'_>) -> Result<(), EncodeError> { Ok(()) }
    fn finish(&mut self) -> Result<(), EncodeError> { Ok(()) }
}
```

**What to mock:** GPU/FFmpeg/hardware I/O boundaries, and anything with side effects. Implement the trait (`Encoder`, `FrameSource`, `UnifiedDetector`, `Panner`, `Tracker`) and capture calls with `Arc<Mutex<...>>` or atomics when the test needs to observe invocations.

**What NOT to mock:** pure computation (geometry, calibration math, packing) — test the real function. Prefer constructing real domain values via helper factories.

## Fixtures and Factories

**Test data:** built inline by helper functions. Synthetic inputs are constructed to survive the pipeline under test — e.g. `synthetic_tile` (`crates/reco-io/tests/stacked_video_roundtrip.rs:34`) writes per-frame/per-tile constant plane values so the written index is recoverable even after lossy H.264 rounding.

**Location:** factories live inside the same `#[cfg(test)] mod tests` block, directly above the tests that use them. There is no shared fixtures directory.

**Temp files:** use `tempfile`. `crates/reco-core/src/source.rs:536` uses `tempfile::tempdir()`; `crates/reco-io` uses `tempfile::Builder` when a named path is required (`crates/reco-io/src/ffmpeg/decoder.rs:513`). Never write to a fixed repo path in a test.

## GPU and Hardware-Dependent Tests

Hardware tests are marked `#[ignore]` with an explicit reason and are skipped in CI:

```rust
#[ignore = "requires a GPU (wgpu adapter init)"]
```

(`crates/reco-core/src/gpu/yuv_stack_packer.rs:899`; also `crates/reco-core/src/session/tests.rs` uses `#[ignore] // requires GPU`)

There are 15 `#[ignore]`d tests total (9 in `reco-core`, 5 in `reco-io`, 1 in `reco-calibrate`). Run them deliberately:

```bash
cargo test -p reco-core -- --ignored
```

**Hardware tests must degrade gracefully when fixtures are absent** rather than fail — check for the file and return early with a message:

```rust
fn have_test_footage() -> bool { Path::new(LEFT_4K).exists() && Path::new(KNOWN_GOOD).exists() }

#[test]
#[ignore]
fn calibrate_videos_matches_known_good() {
    if !have_test_footage() {
        eprintln!("Skipping: test footage not available");
        return;
    }
    ...
}
```

(`crates/reco-calibrate/tests/calibrate_videos.rs`)

## Feature-Gated Tests

Integration tests and feature-specific code are gated at the file or item level with `#![cfg(feature = "...")]` / `#[cfg(feature = "...")]`, and the file header states the run command:

```rust
//! Integration tests for the stacked-video encoder / source.
//! ...
//! cargo test -p reco-io --features stacked-output --test stacked_video_roundtrip
#![cfg(feature = "stacked-output")]
```

(`crates/reco-io/tests/stacked_video_roundtrip.rs`)

When adding a test for feature-gated code, gate it identically and document the exact `cargo test` invocation in the module doc.

## Fuzzing

A separate nightly-only `fuzz` crate lives outside the workspace (`fuzz/`, excluded via `exclude = ["fuzz"]` in `Cargo.toml`) with three `libfuzzer` targets:

- `fuzz/fuzz_targets/calibration_json.rs` — JSON parse + `validate()` attack surface
- `fuzz/fuzz_targets/input_path.rs`
- `fuzz/fuzz_targets/onnx_names.rs`

Run with `cargo +nightly fuzz run <target>` from `fuzz/`. Fuzz targets assert the invariant "always return `Err`, never panic, never hang" and carry a `//!` explanation.

## Coverage

**Requirements:** none enforced — there is no coverage gate in CI and no `llvm-cov`/`tarpaulin` config in the repo.

**View coverage (ad hoc):**

```bash
cargo llvm-cov --workspace            # if cargo-llvm-cov is installed
```

## Test Types

**Unit tests:** the default — co-located `#[cfg(test)] mod tests`, exercising pure functions, validation, state machines, and trait implementations. This is where ~96% of tests live.

**Integration tests:** `crates/*/tests/*.rs`, used only when a flow crosses module or process boundaries (encoder↔source round trip, one-call calibration API, calibration I/O). Often `#[ignore]`d because they need real video/GPU.

**Property/fuzz tests:** `fuzz/` targets only.

**E2E tests:** not used; the CLI is exercised manually (`cargo run -p reco-cli -- stitch ...`).

## CI Test Matrix

From `.github/workflows/rust.yml`:

- **`test` job** runs `cargo test --workspace` and `cargo test --workspace --features profiling`.
- **`check` job** runs clippy across six feature sets (default, `profiling`, `gstreamer`, `load-dynamic`, `reco-gui automation`, `--no-default-features`).
- **`doc` job** builds docs with `RUSTDOCFLAGS="-D warnings"`.
- **`bench-build`** runs `cargo bench --workspace --no-run` (compile-only; no benchmarks directory exists yet).
- **`msrv`** runs `cargo check --workspace` on Rust 1.92.0.
- Cross-compile checks (`check-windows-arm`, `check-linux-arm`, `check-android`) run `cargo check -p reco-core` with/without `profiling` only — because `reco-io` needs target FFmpeg libs.

Note: CI does **not** install a GPU, so every GPU/`#[ignore]`d test is skipped there by design. A change touching GPU code needs the ignored tests run locally (`cargo test -p reco-core -- --ignored`).

## Test Coverage Gaps

- **`reco-cli` has zero tests** (0 `#[test]`). CLI argument parsing and command wiring are untested in-tree; exercise with `cargo run -p reco-cli -- <cmd>`.
- No async/concurrency stress tests despite several channels and worker threads (`AsyncEncodeThread`, session loop).
- No property tests for the pure geometry/packing math (only example-based cases).

---

*Testing analysis: 2026-10-01*
