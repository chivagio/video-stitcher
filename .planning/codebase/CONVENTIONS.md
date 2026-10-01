---
last_mapped_commit: cd3bf434b92c678a5585cd9be2330eda49782a8a
last_mapped_at: 2026-10-01
---
# Coding Conventions

**Analysis Date:** 2026-10-01

## Overview

This is a Rust 2024-edition Cargo workspace (`Cargo.toml`, `crates/*`). All conventions are enforced by CI, not by convention alone: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` (across six feature combinations), and `cargo doc` with `RUSTDOCFLAGS="-D warnings"`. The rules below are drawn from `.github/workflows/rust.yml`, `AGENTS.md`, `CONTRIBUTING.md`, and the source itself.

## Naming Patterns

**Crates / directories:** kebab-case directory under `crates/` (`reco-core`, `reco-io`, `reco-autocam`); library crate names use underscores (`reco_core`).

**Files / modules:** `snake_case.rs`. Multi-file modules use a directory with `mod.rs` (`crates/reco-core/src/core/mod.rs`, `crates/reco-core/src/detect/mod.rs`, `crates/reco-io/src/stacked_video/mod.rs`). A dedicated test file is named `tests.rs` (`crates/reco-core/src/session/tests.rs`).

**Functions / methods:** `snake_case`, verb-first, descriptive. Follow standard Rust, no `test_` prefix on test functions (see `fn validate_zero_fx_fails()` in `crates/reco-core/src/calibration.rs:738`).

**Variables:** `snake_case`. Short, local loop/geometry names are common and accepted (`w`, `h`, `fp`, `px`, `py`).

**Types / traits:** `PascalCase` (`StitchCore`, `MatchCalibration`, `UnifiedDetector`, `Panner`). Error enums are named `<Domain>Error` (`EncodeError`, `DecodeError`, `CalibrationError`, `SessionError`, `StitchCoreError`).

**Constants / statics:** `SCREAMING_SNAKE_CASE` (`DEFAULT_COAST_FRAMES`, `MAX_PATTERN_SIZE`, `DETECT_MAX_WIDTH`, `PLANE_WIDTH`). Private consts are not `pub` unless the value is part of the API contract.

**Cargo features:** kebab-case (`profiling`, `load-dynamic`, `stacked-output`, `tensorrt-native`, `no-default-features`).

## Code Style

**Formatting — `rustfmt`** (`rustfmt.toml`):

- `edition = "2024"`
- `max_width = 100` — lines wrap at 100 columns
- `use_field_init_shorthand = true` — write `Foo { x }`, not `Foo { x: x }`
- Run `cargo fmt --all`; CI runs `cargo fmt --all -- --check` (`.github/workflows/rust.yml` "Format check").

**Linting — `clippy`** (`clippy.toml`):

- `too-many-arguments-threshold = 8` — a function may take up to 8 args before `clippy::too_many_arguments`.
- Zero-warnings policy: CI runs clippy with `-D warnings` for **six** configurations: default, `--features profiling`, `--features gstreamer`, `--features load-dynamic`, `-p reco-gui --features automation`, and `--no-default-features` (`.github/workflows/rust.yml` `check` job).
- Suppressions are rare and always justified with a comment. The only repeated allow is `#[allow(clippy::too_many_arguments)]` (18 uses) for GPU/FFI call sites; one-off allows are `#[allow(clippy::large_enum_variant)]` and `#[allow(clippy::excessive_precision)]`. Always add a `// reason` comment above the allow.

**Toolchain:** pinned in `rust-toolchain.toml` to `1.92.0` with `components = ["rustfmt", "clippy"]`. `rust-version = "1.92"` in `Cargo.toml`. When bumping, update `rust-toolchain.toml`, the `msrv` job in `.github/workflows/rust.yml`, and the workspace `rust-version`.

## Unsafe Code Policy

Crates that do not need raw pointers **forbid or deny** unsafe:

```rust
// crates/reco-autocam/src/lib.rs:45
#![forbid(unsafe_code)]

// crates/reco-calibrate/src/lib.rs:12
#![deny(unsafe_code)]

// crates/reco-control/src/lib.rs:30
#![deny(unsafe_code)]
```

`reco-core` and low-level I/O crates do use `unsafe` for GPU/FFI interop (386 occurrences), but **every `unsafe` block that is not self-evidently sound carries a `// SAFETY:` comment** explaining the invariant (45 `// SAFETY:` comments across the workspace). Follow this pattern:

```rust
// crates/reco-core/src/interop/d3d11.rs:399
// SAFETY: FFmpeg guarantees data[0] is a valid ID3D11Texture2D*
// for the lifetime of the decoded frame. We AddRef via clone
// to get our own reference, then release at scope end.
```

Also comment `unsafe impl` blocks the same way (`crates/reco-core/src/interop/cuda.rs:843`).

## Doc Comments

**Every module opens with a `//!` header** explaining its purpose; 100% of `src` files in `reco-core`, `reco-autocam`, `reco-cli`, `reco-control`, `reco-detect`, `reco-gui`, `reco-io`, and `reco-obs` carry one (58/58 in `reco-core`, 22/22 in `reco-io`). Headers frequently include an ASCII pipeline diagram and usage, e.g. `crates/reco-core/src/lib.rs` documents the frame pipeline and geometric model.

**Every public item gets a `///` doc comment**, including enum variants and struct fields:

```rust
// crates/reco-core/src/encoder.rs
/// Errors that can occur during encoding. `Clone + Send + Sync` so a
/// background encoder thread can send the result through an mpsc
/// channel without forcing the consumer to stringify.
#[derive(Debug, Clone, Error)]
pub enum EncodeError {
    /// The encoder failed to initialize.
    #[error("encoder initialization failed: {reason}")]
    Init {
        /// Human-readable explanation of the failure.
        reason: String,
    },
```

**Intra-doc links are required and checked** — `cargo doc --workspace --no-deps` runs with `RUSTDOCFLAGS="-D warnings"` in CI, so broken links like `[`source::FrameSource`]` fail the build. Use them liberally (see the module map in `crates/reco-core/src/lib.rs`).

**Architecture and "why" comments:** non-obvious design decisions carry a `//` comment stating the reason and often a work-item reference. Preserve these when editing.

## Import Organization

No `rustfmt` import-granularity config is set, so imports are manually grouped and ordered. Observed pattern in `crates/reco-io/src/ffmpeg/decoder.rs`:

1. Module doc (`//!`) and any `extern crate`
2. External crate imports (`use ffmpeg::...`, `use thiserror::Error;`)
3. Workspace crate imports (`use reco_core::profile_scope;`)
4. `std` imports (`use std::path::Path;`)
5. `super::` / `crate::` local imports

Type aliases disambiguate name collisions: `use ffmpeg::software::scaling::{context::Context as ScalingContext, flag::Flags as ScalingFlags};`.

Do not add a new import-ordering tool; match the surrounding file.

## Error Handling

**Library crates use `thiserror` with typed, domain-specific error enums.** Do not use `anyhow` in library crates. Each fallible subsystem defines its own enum (`crates/reco-core/src/encoder.rs`, `crates/reco-core/src/source.rs`, `crates/reco-io/src/ffmpeg/decoder.rs`, `crates/reco-calibrate/src/error.rs`).

Patterns:

- Derive `#[derive(Debug, thiserror::Error)]` plus `Clone + Send + Sync` where the error crosses a thread/channel boundary (documented in the type doc, e.g. `EncodeError`).
- Use `#[error("...")]` messages that name the operation, not the type.
- Nest via `#[from]` for source errors:

```rust
// crates/reco-core/src/core/types.rs
pub enum StitchCoreError {
    #[error("pipeline: {0}")]
    Pipeline(#[from] PipelineError),
    #[error("readback: {0}")]
    Readback(#[from] RgbaReadbackError),
    #[error("config: {0}")]
    Config(String),
    #[error("stacked packer: {0}")]
    StackedPacker(#[from] PackerError),
}
```

- When a foreign error is not `Clone`, stringify it at the `From` boundary and say so in the doc comment (see `DecodeError` in `crates/reco-io/src/ffmpeg/decoder.rs`, where `ffmpeg::Error` is stringified).

**The CLI crate (`reco-cli`) uses `anyhow`** for top-level orchestration: `-> anyhow::Result<()>` and `anyhow::ensure!(...)` / `anyhow::anyhow!(...)` (`crates/reco-cli/src/calibrate.rs`). This boundary between typed library errors and `anyhow` at the binary edge is intentional.

**`unwrap`/`expect`:** allowed in tests and in startup/constant paths; in library code prefer propagating a typed error. There are ~273 `unwrap`/`expect` occurrences across `src`, concentrated in GPU setup and tests — do not add new panicking calls to library hot paths.

## Logging

**Framework:** the `log` facade is the dominant call-site API (658 `log::info!`/`warn!`/`error!` calls), bridged to `tracing` via `tracing-log` (see `Cargo.toml` workspace deps). Write `log::info!`, `log::warn!`, `log::error!`, `log::debug!` in crates.

- `tracing::error!` is used directly only in the CLI panic hook (`crates/reco-cli/src/main.rs`) and OBS integration.
- Each binary installs a `tracing_subscriber` subscriber once at startup (CLI `crates/reco-cli/src/main.rs:723`, GUI `crates/reco-gui/src/main.rs:1217`, OBS `crates/reco-obs/src/lib.rs`). Libraries must not install subscribers.
- Structured fields over string concatenation where useful; prefer inline format args (`log::info!("AE: mean_g={mean_g:.0} ratio={clamped:.2}")`, `crates/reco-core/src/bayer.rs:279`).
- **Profiling instrumentation is opt-in and zero-cost when off.** Wrap hot paths in the `profile_scope!` macro exported by `reco-core` (82 call sites). It expands to a no-op unless the `profiling` feature is enabled (`crates/reco-core/src/lib.rs`).
- `println!`/`eprintln!` belong only in `reco-cli`/`reco-gui`/`reco-obs` user-facing output and in tests; the 34 non-CLI occurrences are progress/UI reporting.

## Comments

**When to comment:** explain *why*, not *what*. Reference work items by ID where relevant (`M6.5 item 3`, `deep-review-2026-04-18 T-1`, `#446`) — this codebase links decisions to issues/milestones. Preserve existing explanatory comments when modifying code.

**Section dividers:** test and long modules use box-drawing dividers to group helpers from tests:

```rust
// ─── Helpers ───────────────────────────────────────────────────────────
```

(`crates/reco-core/src/session/tests.rs`)

**JSDoc/TSDoc equivalent:** Rustdoc `///` / `//!` as covered above.

## Function Design

- Functions stay small; long orchestration lives in named private helpers (`encode_loop`, `make_frame`, `synthetic_tile`).
- Up to 8 arguments without lint; beyond that either box parameters into a config struct (the norm — `CalibrateVideosOptions`, `StackedEncoderConfig`, `ViewportConfig`) or add a justified `#[allow(clippy::too_many_arguments)]`.
- Return `Result<T, DomainError>` for fallible operations; return `Option` only for genuinely absent values. Avoid `Result` for "not found" when an `Option` is semantically correct.
- Builder/config-struct pattern with `::default()` is preferred over many positional booleans (`StackedEncoderConfig::default()`).

## Module Design

- `lib.rs` exposes `pub mod` / `pub use`; keep `reco-core` a **pure library with no I/O dependencies** (`AGENTS.md`, `CONTRIBUTING.md`). Detection, encoding, and camera backends live in their own crates.
- Use `pub(crate)` (123 occurrences) to keep the public surface minimal; do not make an item `pub` until a consumer needs it.
- Traits define pluggable seams (`FrameSource`, `Encoder`, `UnifiedDetector`, `Tracker`, `Panner`) — new backends implement a trait in their own crate rather than adding `match` arms to core.
- **Document friction, don't work around it** (`AGENTS.md`): if a `reco-core` API gap forces a consumer hack, add a `FRICTION.md` entry in that crate (e.g. `crates/reco-gui/FRICTION.md`, `crates/reco-obs/FRICTION.md`) instead of a workaround.
- Trait objects + `Box<dyn ...>` are used at backend boundaries; generic parameters are used for hot-path monomorphization.

## Attributes

- `#[non_exhaustive]` on public enums in `reco-control` (`ControlIntent`, `PoseControl`) and `reco-autocam` (`TrackingMode`) so new variants are non-breaking.
- `#[must_use]` on pure constructors/getters (23 occurrences).
- `#[derive(Debug, Clone, Copy, PartialEq, Eq)]` on value types; `#[derive(Debug, Clone, Error)]` on error types.
- `#[cfg(feature = "...")]` gates optional backend code and tests; the feature must appear in the crate's `Cargo.toml`.

## Git / PR Conventions

From `CONTRIBUTING.md` and `.github/PULL_REQUEST_TEMPLATE.md`:

- Branch names: `feat/...`, `fix/...`, `refactor/...`.
- Commit prefixes: `feat:`, `fix:`, `refactor:`, `chore:`, `perf:`. Keep messages concise and technical — what changed + why, no filler/marketing.
- One PR per feature/fix; squash-merge; reference `Closes #123`.
- PRs must pass fmt, clippy (all feature sets), test (default + profiling), doc, cargo-deny, RUSTSEC audit, gitleaks, and CLA.

---

*Convention analysis: 2026-10-01*
