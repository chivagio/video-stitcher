//! Worker command vocabulary and the UI-side [`WorkerHandle`] (D-06).
//!
//! # The message-passing contract
//!
//! Every UI→engine interaction crosses an [`std::sync::mpsc`](std::sync::mpsc)
//! channel as a typed [`WorkerCommand`]. The `#[tauri::command]` handlers (Plan
//! 04) are **thin**: they validate their arguments, build a [`WorkerCommand`],
//! and call [`WorkerHandle::send`]. They never call an engine entry point or
//! any other engine API directly — that is the whole point of D-06 and of
//! FOUND-03's "all UI↔engine communication is message passing".
//!
//! ```text
//!   #[tauri::command] import/preview/export        engine worker thread
//!   ───────────────────────────────────────        ────────────────────
//!   build WorkerCommand ──▶ WorkerHandle::send ──▶ mpsc ──▶ worker_loop
//! ```
//!
//! This module deliberately imports **no engine types** (no `reco-io` /
//! `reco-core` engine surfaces) beyond the transport-agnostic
//! [`reco_control::ControlIntent`] input vocabulary: a command handler that
//! cannot name an engine type cannot accidentally call one.
//!
//! # Why `std::sync::mpsc`
//!
//! Phase 1's command volume is three buttons. The worker loop needs only
//! "drain everything pending, then block when idle" — `try_recv` + `recv`
//! cover that. `crossbeam-channel`'s `select!`, bounded back-pressure, and
//! `recv_timeout` ergonomics buy nothing at this volume and would add a
//! dependency (RESEARCH Assumption A6). If a later phase needs bounded/select
//! semantics the channel type is already behind [`WorkerHandle`], so swapping
//! it touches this module alone.

use std::sync::mpsc::{SendError, Sender};

use crate::events::WorkerError;

/// Load the hardcoded clips and match profile into the engine worker (D-08).
///
/// # The thin-command contract (D-06)
///
/// This handler **only** posts a typed [`WorkerCommand`] to the worker's
/// channel and returns. It performs no engine work, imports no engine type
/// (see the module header), and therefore cannot hold a lock across a render
/// tick or block the webview. On Windows a synchronous command that did work
/// here would deadlock window/webview creation (wry #583 — RESEARCH Pitfall 2);
/// the handler is declared `async` and the posting is a non-blocking
/// `mpsc::send` (the channel is unbounded).
///
/// # Errors
///
/// Returns [`WorkerError::ChannelClosed`] if the worker has already exited; the
/// frontend renders that as an ERROR log line rather than hanging.
#[tauri::command]
pub async fn import(state: tauri::State<'_, WorkerHandle>) -> Result<(), WorkerError> {
    state.send(WorkerCommand::Import)
}

/// Start the live stitched-frame preview loop (D-07).
///
/// Thin, same contract as [`import`]: post `Preview` and return. The worker
/// owns the decode/render loop; the webview only sees the events it emits.
///
/// # Errors
///
/// Returns [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command]
pub async fn preview(state: tauri::State<'_, WorkerHandle>) -> Result<(), WorkerError> {
    state.send(WorkerCommand::Preview)
}

/// Run the hardcoded file→file export to the fixed output path (D-08).
///
/// Thin, same contract as [`import`]: post `Export` and return. Export runs on
/// the worker thread (the engine's one-shot file→file job), never on the
/// webview/main thread.
///
/// # Errors
///
/// Returns [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command]
pub async fn export(state: tauri::State<'_, WorkerHandle>) -> Result<(), WorkerError> {
    state.send(WorkerCommand::Export)
}

/// Begin/continue playing the preview session (PREV-02).
///
/// Thin, same contract as [`import`]: post `Play` and return. The worker owns
/// the paced session loop; the webview only sees the `Transport`/`Position`
/// events it emits.
///
/// # Errors
///
/// Returns [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command]
pub async fn play(state: tauri::State<'_, WorkerHandle>) -> Result<(), WorkerError> {
    state.send(WorkerCommand::Play)
}

/// Pause the preview session at the current position (PREV-02).
///
/// Thin, same contract as [`play`].
///
/// # Errors
///
/// Returns [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command]
pub async fn pause(state: tauri::State<'_, WorkerHandle>) -> Result<(), WorkerError> {
    state.send(WorkerCommand::Pause)
}

/// Coalesced-seek to an absolute frame index (PREV-02).
///
/// Thin, same contract as [`play`]. The frame is typed `u64` (no strings) and
/// clamped inside the worker against the loaded source (T-02-04).
///
/// # Errors
///
/// Returns [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command]
pub async fn seek(state: tauri::State<'_, WorkerHandle>, frame: u64) -> Result<(), WorkerError> {
    state.send(WorkerCommand::Seek { frame })
}

/// Validate a `StepFrame` direction at the command boundary.
///
/// Only `-1` and `+1` are valid; anything else is rejected before it can reach
/// the decoder (T-02-04). Extracted as a pure function so the boundary check is
/// unit-testable without a Tauri `State`.
fn validate_step_direction(direction: i32) -> Result<(), WorkerError> {
    if direction == 1 || direction == -1 {
        Ok(())
    } else {
        Err(WorkerError::Unsupported {
            operation: format!("step_frame direction {direction} (expected -1 or +1)"),
        })
    }
}

/// Step exactly `direction` frames (expected `-1` or `+1`; PREV-02).
///
/// Validates `direction` at the command boundary — any value other than ±1 is
/// rejected with [`WorkerError::Unsupported`] rather than reaching the decoder
/// (T-02-04) — then posts a `StepFrame`. Thin: names no engine type.
///
/// # Errors
///
/// Returns [`WorkerError::Unsupported`] for a direction other than ±1, or
/// [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command]
pub async fn step_frame(
    state: tauri::State<'_, WorkerHandle>,
    direction: i32,
) -> Result<(), WorkerError> {
    validate_step_direction(direction)?;
    state.send(WorkerCommand::StepFrame { direction })
}

/// Enable/disable full-clip looping (PREV-02).
///
/// Thin, same contract as [`play`].
///
/// # Errors
///
/// Returns [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command]
pub async fn set_loop(
    state: tauri::State<'_, WorkerHandle>,
    enabled: bool,
) -> Result<(), WorkerError> {
    state.send(WorkerCommand::SetLoop(enabled))
}

/// Report the webview chrome's collapsible state (UI-SPEC Geometry authority).
///
/// Thin: posts `SetChrome`; the worker recomputes the native viewport and the
/// native view's visibility from the active screen (Import/Calibrate suspend
/// it; Preview shows it). Names no engine type.
///
/// # Why `rename_all = "snake_case"`
///
/// Tauri resolves each command argument by a single payload key
/// (`v.get(key)` in `tauri::ipc::command`), and `#[tauri::command]` defaults that
/// key to **camelCase**. The frontend reports chrome state with snake_case keys
/// (`{ panel_expanded, drawer_expanded, active_screen }`) to match the rest of
/// the typed protocol — `WorkerCommand::SetChrome`, `ViewMode`'s `serde(rename_all =
/// "snake_case")`, `PresenterKind`, `Screen`, and every other payload in this
/// crate. Left at the default, the command deserialization fails with `missing
/// required key panelExpanded`, the worker's `set_chrome` never runs, and the
/// native child window keeps its stale geometry: the controls panel expands in
/// the webview while the panorama keeps painting over it. `void invoke(...)` in
/// the frontend swallows the rejection, so the only symptom is a silently wrong
/// viewport — see `crates/reco-app/FRICTION.md` A7.
///
/// # Errors
///
/// Returns [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command(rename_all = "snake_case")]
pub async fn set_chrome(
    state: tauri::State<'_, WorkerHandle>,
    panel_expanded: bool,
    drawer_expanded: bool,
    active_screen: crate::presenter::Screen,
) -> Result<(), WorkerError> {
    state.send(WorkerCommand::SetChrome {
        panel_expanded,
        drawer_expanded,
        active_screen,
    })
}

/// Swap the active presenter to `kind` (PREV-05).
///
/// Thin: posts `SetPresenter`; the worker re-runs the chain and reports the
/// outcome. Typed [`PresenterKind`](crate::presenter::PresenterKind) — never a
/// string (T-02-07); the override cannot inject a new surface.
///
/// # Errors
///
/// Returns [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command]
pub async fn set_presenter(
    state: tauri::State<'_, WorkerHandle>,
    kind: crate::presenter::PresenterKind,
) -> Result<(), WorkerError> {
    state.send(WorkerCommand::SetPresenter(kind))
}

/// Attach the webview readback channel to the readback presenter (PREV-05).
///
/// Thin: forwards the `Channel<Response>` to the worker through its readback
/// channel path; the worker stores it in the readback presenter. Names no engine
/// type. The channel receives raw RGBA frames as `ArrayBuffer`s.
///
/// # Errors
///
/// Returns [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command]
pub async fn preview_attach_readback(
    sender: tauri::State<'_, crate::worker::ReadbackSender>,
    on_frame: tauri::ipc::Channel<tauri::ipc::Response>,
) -> Result<(), WorkerError> {
    sender
        .0
        .send(on_frame)
        .map_err(|_| WorkerError::ChannelClosed)
}

/// Show the separate preview window (PREV-05 "Show preview window" action).
///
/// Thin: posts `ShowPreviewWindow`; the worker shows the Rust-owned preview
/// window.
///
/// # Errors
///
/// Returns [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command]
pub async fn show_preview_window(state: tauri::State<'_, WorkerHandle>) -> Result<(), WorkerError> {
    state.send(WorkerCommand::ShowPreviewWindow)
}

/// Switch the preview between source tiles and the stitched panorama (PREV-03).
///
/// Thin: posts `SetView`; the worker applies the switch at a tick boundary.
/// Typed [`ViewMode`](crate::presenter::ViewMode) — never a string.
///
/// # Errors
///
/// Returns [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command]
pub async fn set_view(
    state: tauri::State<'_, WorkerHandle>,
    mode: crate::presenter::ViewMode,
) -> Result<(), WorkerError> {
    state.send(WorkerCommand::SetView(mode))
}

/// Validate an operator-supplied input path at the command boundary.
///
/// The path is a local file string chosen by the native dialog or an HTML5
/// drop; this is a cheap sanity gate, not a path constructor. It delegates to
/// [`validate_profile_path`] so the video-input path and the profile path
/// apply the **same** untrusted-path policy — empty/whitespace and the
/// forbidden FFmpeg protocol prefixes — and the two cannot drift (T-03-01).
/// Before this, `set_input` accepted `http://…`/`pipe:…`, which the worker then
/// handed straight to FFmpeg's `format::input`; the profile path already
/// rejected them. Extracted as a pure function so the boundary check is
/// unit-testable without a Tauri `State`.
fn validate_input_path(path: &str) -> Result<(), WorkerError> {
    validate_profile_path(path)
}

/// Set the operator-chosen video path for one camera input (IMPT-01).
///
/// Thin, same contract as [`import`]: validate the path, post a typed
/// `WorkerCommand::SetInput`, and return. The worker probes the file with
/// FFmpeg and emits a typed `ImportMetadata` event. Names no engine type.
///
/// # Why `rename_all = "snake_case"`
///
/// `role` and `path` are single words today, but the command is pinned to the
/// crate's snake_case IPC protocol so a future argument rename cannot reintroduce
/// the silent-never-fires bug recorded in `crates/reco-app/FRICTION.md` A7.
///
/// # Errors
///
/// Returns [`WorkerError::InvalidInput`] for an empty path, or
/// [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command(rename_all = "snake_case")]
pub async fn set_input(
    state: tauri::State<'_, WorkerHandle>,
    role: crate::events::InputRole,
    path: String,
) -> Result<(), WorkerError> {
    validate_input_path(&path)?;
    state.send(WorkerCommand::SetInput { role, path })
}

/// Clear one camera input slot (IMPT-01).
///
/// Thin: post `ClearInput`; the worker clears the slot, recomputes
/// compatibility, and invalidates any existing result.
///
/// # Errors
///
/// Returns [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command(rename_all = "snake_case")]
pub async fn clear_input(
    state: tauri::State<'_, WorkerHandle>,
    role: crate::events::InputRole,
) -> Result<(), WorkerError> {
    state.send(WorkerCommand::ClearInput { role })
}

/// Request the lens-profile candidates for one input (IMPT-04 / D3-07).
///
/// Thin: post `LensCandidates`; the worker queries the embedded `LensDatabase`
/// and emits a typed `LensCandidates` event.
///
/// # Errors
///
/// Returns [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command(rename_all = "snake_case")]
pub async fn lens_candidates(
    state: tauri::State<'_, WorkerHandle>,
    role: crate::events::InputRole,
) -> Result<(), WorkerError> {
    state.send(WorkerCommand::LensCandidates { role })
}

/// Apply a lens-profile override to one input (IMPT-04 / D3-08).
///
/// Thin: post `SetLensOverride`; the worker resolves the candidate to
/// `CameraParams`, stores it, and invalidates any existing result.
///
/// # Errors
///
/// Returns [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command(rename_all = "snake_case")]
pub async fn set_lens_override(
    state: tauri::State<'_, WorkerHandle>,
    role: crate::events::InputRole,
    candidate: crate::events::LensCandidate,
) -> Result<(), WorkerError> {
    state.send(WorkerCommand::SetLensOverride { role, candidate })
}

/// Clear a lens-profile override, returning the input to auto-detect (IMPT-04).
///
/// Thin: post `ClearLensOverride`.
///
/// # Errors
///
/// Returns [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command(rename_all = "snake_case")]
pub async fn clear_lens_override(
    state: tauri::State<'_, WorkerHandle>,
    role: crate::events::InputRole,
) -> Result<(), WorkerError> {
    state.send(WorkerCommand::ClearLensOverride { role })
}

/// Validate the advanced calibration options at the command boundary (T-03-07).
///
/// Only the four locked advanced fields are accepted; `num_frames` is capped
/// and the skip seconds must be finite and non-negative. Extracted as a pure
/// function so the boundary check is unit-testable without a Tauri `State`.
fn validate_options(options: &crate::events::CalibrationOptions) -> Result<(), WorkerError> {
    /// Upper bound on sampled frame pairs. Calibration beyond this is a
    /// pathological request, not an operator choice.
    const MAX_NUM_FRAMES: usize = 200;

    if let Some(n) = options.num_frames
        && !(1..=MAX_NUM_FRAMES).contains(&n)
    {
        return Err(WorkerError::InvalidInput {
            field: "num_frames".to_string(),
            reason: format!("must be between 1 and {MAX_NUM_FRAMES}"),
        });
    }
    for (field, value) in [
        ("skip_start_secs", options.skip_start_secs),
        ("skip_end_secs", options.skip_end_secs),
    ] {
        if let Some(v) = value
            && (!v.is_finite() || v < 0.0)
        {
            return Err(WorkerError::InvalidInput {
                field: field.to_string(),
                reason: "must be a finite value >= 0".to_string(),
            });
        }
    }
    Ok(())
}

/// Start a guided calibration run on the worker (CALB-01).
///
/// Thin: validate the advanced options, post `StartCalibration`. The worker
/// runs calibration on its own GPU device and emits stage/progress/heartbeat/
/// result events. Names no engine type.
///
/// # Errors
///
/// Returns [`WorkerError::InvalidInput`] for out-of-range options, or
/// [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command(rename_all = "snake_case")]
pub async fn start_calibration(
    state: tauri::State<'_, WorkerHandle>,
    options: crate::events::CalibrationOptions,
) -> Result<(), WorkerError> {
    validate_options(&options)?;
    state.send(WorkerCommand::StartCalibration { options })
}

/// Validate an operator-supplied profile path at the command boundary (T-03-07).
///
/// Mirrors `reco-io`'s forbidden-prefix guard: a profile is a **local file**
/// chosen by the native dialog, never an FFmpeg protocol/URL. Rejecting here
/// means the worker never opens a non-local path. Extracted as a pure function
/// so the boundary check is unit-testable without a Tauri `State`.
fn validate_profile_path(path: &str) -> Result<(), WorkerError> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err(WorkerError::InvalidInput {
            field: "path".to_string(),
            reason: "path must not be empty".to_string(),
        });
    }
    // The same prefix set `reco_io::ffmpeg::calibration_io` rejects before an
    // FFmpeg open (the path-safety precedent for any FFmpeg invocation).
    const FORBIDDEN_PREFIXES: &[&str] = &["http://", "https://", "concat:", "pipe:", "data:"];
    let lower = trimmed.to_ascii_lowercase();
    if FORBIDDEN_PREFIXES.iter().any(|p| lower.starts_with(p)) {
        return Err(WorkerError::InvalidInput {
            field: "path".to_string(),
            reason: "path must be a local file, not a URL or ffmpeg protocol".to_string(),
        });
    }
    Ok(())
}

/// Load a calibration profile from a local `.json` file (IMPT-05 / D3-15).
///
/// Thin: validate the path, post `LoadProfile`. The worker validates by
/// deserializing into `MatchCalibration` (size cap + `validate()`) and emits a
/// typed `ProfileLoaded` or a `Failed(ProfileLoad)`.
///
/// # Errors
///
/// Returns [`WorkerError::InvalidInput`] for an empty/non-local path, or
/// [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command(rename_all = "snake_case")]
pub async fn load_profile(
    state: tauri::State<'_, WorkerHandle>,
    path: String,
) -> Result<(), WorkerError> {
    validate_profile_path(&path)?;
    state.send(WorkerCommand::LoadProfile { path })
}

/// Save the current calibration profile to a local `.json` file (IMPT-06 / D3-16).
///
/// Thin: validate the path, post `SaveProfile`. The worker writes
/// `MatchCalibration::to_json_pretty()` and emits `ProfileSaved` or a
/// `Failed(ProfileSave)`.
///
/// # Errors
///
/// Returns [`WorkerError::InvalidInput`] for an empty/non-local path, or
/// [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command(rename_all = "snake_case")]
pub async fn save_profile(
    state: tauri::State<'_, WorkerHandle>,
    path: String,
) -> Result<(), WorkerError> {
    validate_profile_path(&path)?;
    state.send(WorkerCommand::SaveProfile { path })
}

/// Upper bound on vertices per camera field-ROI polygon (CALB-09 / T-04-13).
///
/// A polygon beyond this is a pathological payload, not an operator edit; the
/// cap keeps an oversized webview payload from reaching the engine.
const MAX_FIELD_ROI_VERTICES: usize = 64;

/// Validate a field-ROI polygon pair at the command boundary (T-04-12/T-04-13).
///
/// Every coordinate must be finite and within `[0, 1]` (normalized source-frame
/// space), and each camera's vertex list is capped. A camera polygon with fewer
/// than three vertices is accepted here and normalized to "clear" by the worker
/// (the UI states this instead of saving a broken polygon). Extracted as a pure
/// function so the boundary check is unit-testable without a Tauri `State`.
fn validate_field_roi(left: &[[f64; 2]], right: &[[f64; 2]]) -> Result<(), WorkerError> {
    for (camera, verts) in [("left", left), ("right", right)] {
        if verts.len() > MAX_FIELD_ROI_VERTICES {
            return Err(WorkerError::InvalidInput {
                field: format!("field_roi.{camera}"),
                reason: format!("at most {MAX_FIELD_ROI_VERTICES} vertices are allowed"),
            });
        }
        for (i, [x, y]) in verts.iter().enumerate() {
            if !x.is_finite() || !y.is_finite() {
                return Err(WorkerError::InvalidInput {
                    field: format!("field_roi.{camera}[{i}]"),
                    reason: "coordinates must be finite".to_string(),
                });
            }
            if !(0.0..=1.0).contains(x) || !(0.0..=1.0).contains(y) {
                return Err(WorkerError::InvalidInput {
                    field: format!("field_roi.{camera}[{i}]"),
                    reason: "coordinates must be within [0, 1]".to_string(),
                });
            }
        }
    }
    Ok(())
}

/// Set the per-camera field ROI polygon for framing (CALB-09).
///
/// Thin, same contract as [`set_input`]: validate the polygon at the boundary
/// (finite, normalized `[0,1]`, bounded vertex count; T-04-12/T-04-13), post a
/// typed `WorkerCommand::SetFieldRoi`, and return. The worker writes it onto the
/// current calibration and emits a typed `FieldRoiApplied`/`FieldRoiCleared`.
/// Names no engine type.
///
/// # Why `rename_all = "snake_case"`
///
/// `left`/`right` are single words today, but the command is pinned to the
/// crate's snake_case IPC protocol so a future argument rename cannot reintroduce
/// the silent-never-fires bug recorded in `crates/reco-app/FRICTION.md` A7.
///
/// # Errors
///
/// Returns [`WorkerError::InvalidInput`] for an out-of-range, non-finite, or
/// oversized polygon, or [`WorkerError::ChannelClosed`] if the worker has
/// already exited.
#[tauri::command(rename_all = "snake_case")]
pub async fn set_field_roi(
    state: tauri::State<'_, WorkerHandle>,
    left: Vec<[f64; 2]>,
    right: Vec<[f64; 2]>,
) -> Result<(), WorkerError> {
    validate_field_roi(&left, &right)?;
    state.send(WorkerCommand::SetFieldRoi { left, right })
}

/// Cancel a running calibration (CALB-02 / D3-11).
///
/// # The documented control-plane bypass
///
/// Calibration runs synchronously on the worker thread, so a command posted to
/// the worker's channel would not be observed until calibration already
/// returned (RESEARCH Pitfall 3). This handler therefore sets the shared
/// [`Arc<AtomicBool>`](std::sync::atomic::AtomicBool) that
/// `calibrate_videos_with_gpu` polls **directly** — it deliberately posts
/// nothing to the blocked channel. This is the single sanctioned exception to
/// the "every UI→engine action is a `WorkerCommand`" rule (D-06 / T-03-10).
///
/// # Errors
///
/// Never fails; returns `Ok(())` so the frontend can always clear its
/// "Cancelling…" state.
#[tauri::command]
pub async fn cancel_calibration(
    state: tauri::State<'_, crate::worker::CalibrationCancel>,
) -> Result<(), WorkerError> {
    state.0.store(true, std::sync::atomic::Ordering::SeqCst);
    Ok(())
}

/// Forward a transport-agnostic input intent to the worker (PREV-04).
///
/// Thin, same contract as [`play`]: post `Intent` and return. The typed
/// [`reco_control::ControlIntent`] is the pose/input vocabulary — the UI
/// never invents a parallel input vocabulary (02-CONTEXT).
///
/// # Errors
///
/// Returns [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command]
pub async fn intent(
    state: tauri::State<'_, WorkerHandle>,
    intent: reco_control::ControlIntent,
) -> Result<(), WorkerError> {
    state.send(WorkerCommand::Intent(intent))
}

/// Ask the worker to re-assert its whole projection (position, transport,
/// pose, view mode, presenter) to the event channel.
///
/// This exists because the Tauri bridge is fire-and-forget: `emit` only
/// reaches the listeners that are registered *at that moment*. The worker is
/// spawned in `setup()` and imports immediately, so its opening position,
/// transport, and view projections are emitted roughly a second into startup —
/// before the webview has finished loading and run its `listen`. Those lines
/// are dropped, and the frontend sits in its initial `empty` state: transport
/// controls stay disabled and the clip length reads as unknown.
///
/// The frontend therefore subscribes first and *then* calls this, which
/// reconciles the missed opening state. It is also what restores the UI after
/// a webview reload, where every projection since boot has been missed.
///
/// Thin, same contract as [`play`]: post the command and return; the worker
/// emits on its own thread.
///
/// # Errors
///
/// Returns [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command]
pub async fn republish_projection(
    state: tauri::State<'_, WorkerHandle>,
) -> Result<(), WorkerError> {
    state.send(WorkerCommand::RepublishProjection)
}

/// A command from the UI to the engine worker.
///
/// `Clone + Send + 'static` — the compile-time assertion in `events.rs`
/// enforces it, because a command is moved through the worker channel.
// The Tauri command handlers construct these variants; the worker loop consumes
// them. A subset is still dead-code-flagged in test builds that do not exercise
// the binary path, so the suppression stays narrowly scoped to this enum.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum WorkerCommand {
    /// Load the hardcoded clips and match profile into the session.
    ///
    /// Paths are resolved inside the worker from `hardcoded.rs` (D-08); the
    /// command deliberately carries no paths so file selection stays out of
    /// the UI for Phase 1.
    Import,

    /// Start the live stitched-frame preview loop (D-07).
    Preview,

    /// Run the hardcoded file→file export to the fixed output path (D-08).
    Export,

    /// Forward a transport-agnostic input intent (pan / zoom / quality) to the
    /// worker's pose state on the same message-passing path as the other
    /// commands.
    ///
    /// Reusing `reco_control::ControlIntent` (PATTERNS.md "Don't Hand-Roll")
    /// means the UI never invents a parallel input vocabulary and there is no
    /// direct pose-control call from a command handler.
    Intent(reco_control::ControlIntent),

    /// Begin/continue playing the preview session (PREV-02).
    Play,

    /// Pause the preview session at the current position (PREV-02).
    Pause,

    /// Coalesced seek to `frame` (PREV-02). The worker stores this as the single
    /// pending seek and executes at most one `source.seek` per session tick.
    Seek {
        /// Target frame index (0-based). Clamped to the loaded source's total.
        frame: u64,
    },

    /// Step exactly `direction` frames (expected `-1` or `+1`; PREV-02).
    StepFrame {
        /// Frame delta; only `-1` and `+1` are accepted (T-02-04).
        direction: i32,
    },

    /// Enable/disable full-clip looping (PREV-02).
    SetLoop(bool),

    /// Report the webview chrome's collapsible state so the worker recomputes
    /// the native viewport and the native view's visibility (UI-SPEC Geometry
    /// authority / Screen Router).
    SetChrome {
        /// Whether the right controls panel is expanded.
        panel_expanded: bool,
        /// Whether the event-log drawer is expanded.
        drawer_expanded: bool,
        /// Which top-level screen is active. Import/Calibrate suspend the native
        /// child view; Preview shows it. Typed [`Screen`](crate::presenter::Screen)
        /// — never a string; an unknown value is rejected at the IPC boundary
        /// (T-03-11).
        active_screen: crate::presenter::Screen,
    },

    /// Reconfigure the native viewport for a new window size (PREV-01/04).
    ///
    /// Sent by `main.rs` on `WindowEvent::Resized`; carries no engine type and
    /// preserves transport position and pose (Pitfall 8).
    ResizeViewport {
        /// New window width in physical pixels.
        width: u32,
        /// New window height in physical pixels.
        height: u32,
    },

    /// Swap the active presenter to `kind` (PREV-05).
    ///
    /// Typed [`PresenterKind`](crate::presenter::PresenterKind) — never a string
    /// (T-02-07). The worker re-runs the chain from `kind` and reports the
    /// outcome; the override cannot inject a new surface.
    SetPresenter(crate::presenter::PresenterKind),

    /// Show the separate preview window (PREV-05).
    ShowPreviewWindow,

    /// Re-assert the whole worker-owned projection to the event channel.
    ///
    /// Sent by the frontend once every store has subscribed, to recover the
    /// opening position/transport/pose/view that the fire-and-forget bridge
    /// dropped before the webview was listening (see
    /// [`republish_projection`]).
    RepublishProjection,

    /// Switch the preview between source tiles and the stitched panorama
    /// (PREV-03).
    ///
    /// Typed [`ViewMode`](crate::presenter::ViewMode) — never a string. The
    /// worker applies the switch at a tick boundary; the playhead and pose are
    /// preserved.
    SetView(crate::presenter::ViewMode),

    /// Set the operator-chosen video path for one camera input (IMPT-01).
    ///
    /// Carries only a typed [`InputRole`](crate::events::InputRole) and a local
    /// file path string chosen by the native dialog or an HTML5 drop. The worker
    /// probes the file with FFmpeg and emits a typed `ImportMetadata` event. A
    /// job: it blocks on the probe, so the loop re-drains after it.
    SetInput {
        /// Which camera input the path belongs to.
        role: crate::events::InputRole,
        /// The local file path to probe.
        path: String,
    },

    /// Clear one camera input slot (IMPT-01).
    ///
    /// The worker clears the slot, recomputes compatibility from the remaining
    /// inputs, and invalidates any existing result (D3-08).
    ClearInput {
        /// Which camera input to clear.
        role: crate::events::InputRole,
    },

    /// Request the lens-profile candidates for one input (IMPT-04 / D3-07).
    ///
    /// The worker queries the embedded `LensDatabase` for the input's
    /// resolution and emits a typed `LensCandidates` event.
    LensCandidates {
        /// Which camera input to list candidates for.
        role: crate::events::InputRole,
    },

    /// Apply a lens-profile override to one input (IMPT-04 / D3-08).
    ///
    /// The worker resolves the candidate to `CameraParams` via
    /// `LensDatabase::load_by_summary`, stores it, and invalidates any existing
    /// result.
    SetLensOverride {
        /// Which camera input the override applies to.
        role: crate::events::InputRole,
        /// The chosen profile.
        candidate: crate::events::LensCandidate,
    },

    /// Clear a lens-profile override, returning the input to auto-detect
    /// (IMPT-04).
    ClearLensOverride {
        /// Which camera input to reset.
        role: crate::events::InputRole,
    },

    /// Start a guided calibration run (CALB-01 / D3-09).
    ///
    /// A job: calibration blocks the worker thread until it finishes or is
    /// cancelled via the shared flag.
    StartCalibration {
        /// The advanced options (all optional).
        options: crate::events::CalibrationOptions,
    },

    /// Load a calibration profile from a local `.json` file (IMPT-05).
    ///
    /// A job: the worker reads and validates the file.
    LoadProfile {
        /// The local file path to load.
        path: String,
    },

    /// Save the current calibration profile to a local `.json` file (IMPT-06).
    ///
    /// A job: the worker writes the pretty-printed JSON.
    SaveProfile {
        /// The local file path to write.
        path: String,
    },

    /// Set the per-camera field ROI polygon for framing (CALB-09).
    ///
    /// The worker normalizes each camera's vertex list (fewer than three
    /// vertices clears that camera's polygon) and writes the result onto the
    /// current calibration's `field_roi`, then emits a typed
    /// `FieldRoiApplied`/`FieldRoiCleared`. Coordinates are normalized `[0,1]`
    /// and validated at the command boundary (T-04-12/T-04-13). The ROI does not
    /// change the stitch geometry, so the result is not invalidated.
    SetFieldRoi {
        /// Left-camera polygon vertices, normalized `[0,1]`.
        left: Vec<[f64; 2]>,
        /// Right-camera polygon vertices, normalized `[0,1]`.
        right: Vec<[f64; 2]>,
    },

    /// Stop the worker loop and return.
    Shutdown,

    /// Simulate a device-loss event (reserved for Plan 05's FOUND-05 work).
    ///
    /// Rejected with [`WorkerError::Unsupported`] in this plan; the variant
    /// exists now so Plan 05 extends the protocol rather than reshaping it.
    SimulateDeviceLoss,
}

/// The UI-side handle to the engine worker's command channel.
///
/// Wraps the sending half of the channel. Only [`WorkerHandle::send`] is
/// exposed — there is no way to reach the worker's engine objects from here,
/// which is what makes the single-owner boundary (FOUND-03) structurally
/// difficult to violate.
// See the `WorkerCommand` note above: the handle's constructor (`EngineWorker`)
// and the Tauri handlers using it land in Task 2 / Plan 04.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone)]
pub struct WorkerHandle {
    tx: Sender<WorkerCommand>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl WorkerHandle {
    /// Wrap a command sender, e.g. the one retained by [`crate::worker::EngineWorker`].
    pub fn new(tx: Sender<WorkerCommand>) -> Self {
        Self { tx }
    }

    /// Post a command to the worker.
    ///
    /// Blocks only if the channel is bounded (it is not: `std::sync::mpsc` is
    /// unbounded), so in practice this is a non-blocking handoff.
    ///
    /// # Errors
    ///
    /// Returns [`WorkerError::ChannelClosed`] when the worker thread has
    /// exited and the receiver has been dropped — the UI turns this into an
    /// ERROR log line rather than panicking.
    pub fn send(&self, cmd: WorkerCommand) -> Result<(), WorkerError> {
        self.tx
            .send(cmd)
            .map_err(|SendError(_)| WorkerError::ChannelClosed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn send_delivers_in_arrival_order() {
        let (tx, rx) = mpsc::channel();
        let handle = WorkerHandle::new(tx);
        handle.send(WorkerCommand::Import).unwrap();
        handle.send(WorkerCommand::Preview).unwrap();
        handle.send(WorkerCommand::Shutdown).unwrap();
        assert_eq!(rx.recv().unwrap(), WorkerCommand::Import);
        assert_eq!(rx.recv().unwrap(), WorkerCommand::Preview);
        assert_eq!(rx.recv().unwrap(), WorkerCommand::Shutdown);
    }

    #[test]
    fn send_after_receiver_drop_reports_channel_closed() {
        let (tx, rx) = mpsc::channel();
        let handle = WorkerHandle::new(tx);
        drop(rx);
        assert_eq!(
            handle.send(WorkerCommand::Import),
            Err(WorkerError::ChannelClosed)
        );
    }

    #[test]
    fn handle_is_clone_and_send() {
        // The Tauri command handlers each hold their own clone of the handle;
        // `State<WorkerHandle>` requires it be `Send + Sync`.
        fn assert_send_sync<T: Send + Sync + Clone>() {}
        assert_send_sync::<WorkerHandle>();
    }

    #[test]
    fn new_transport_commands_round_trip_through_the_channel() {
        let (tx, rx) = mpsc::channel();
        let handle = WorkerHandle::new(tx);
        handle.send(WorkerCommand::Play).unwrap();
        handle.send(WorkerCommand::Pause).unwrap();
        handle.send(WorkerCommand::Seek { frame: 42 }).unwrap();
        handle
            .send(WorkerCommand::StepFrame { direction: 1 })
            .unwrap();
        handle.send(WorkerCommand::SetLoop(true)).unwrap();
        handle
            .send(WorkerCommand::SetChrome {
                panel_expanded: true,
                drawer_expanded: false,
                active_screen: crate::presenter::Screen::Calibrate,
            })
            .unwrap();
        handle
            .send(WorkerCommand::ResizeViewport {
                width: 1280,
                height: 800,
            })
            .unwrap();
        handle
            .send(WorkerCommand::SetPresenter(
                crate::presenter::PresenterKind::SeparateWindow,
            ))
            .unwrap();
        handle.send(WorkerCommand::ShowPreviewWindow).unwrap();
        assert_eq!(rx.recv().unwrap(), WorkerCommand::Play);
        assert_eq!(rx.recv().unwrap(), WorkerCommand::Pause);
        assert_eq!(rx.recv().unwrap(), WorkerCommand::Seek { frame: 42 });
        assert_eq!(
            rx.recv().unwrap(),
            WorkerCommand::StepFrame { direction: 1 }
        );
        assert_eq!(rx.recv().unwrap(), WorkerCommand::SetLoop(true));
        assert_eq!(
            rx.recv().unwrap(),
            WorkerCommand::SetChrome {
                panel_expanded: true,
                drawer_expanded: false,
                active_screen: crate::presenter::Screen::Calibrate
            }
        );
        assert_eq!(
            rx.recv().unwrap(),
            WorkerCommand::ResizeViewport {
                width: 1280,
                height: 800
            }
        );
        assert_eq!(
            rx.recv().unwrap(),
            WorkerCommand::SetPresenter(crate::presenter::PresenterKind::SeparateWindow)
        );
        assert_eq!(rx.recv().unwrap(), WorkerCommand::ShowPreviewWindow);
    }

    #[test]
    fn set_view_command_round_trips_through_the_channel() {
        let (tx, rx) = mpsc::channel();
        let handle = WorkerHandle::new(tx);
        handle
            .send(WorkerCommand::SetView(crate::presenter::ViewMode::Source))
            .unwrap();
        handle
            .send(WorkerCommand::SetView(crate::presenter::ViewMode::Panorama))
            .unwrap();
        assert_eq!(
            rx.recv().unwrap(),
            WorkerCommand::SetView(crate::presenter::ViewMode::Source)
        );
        assert_eq!(
            rx.recv().unwrap(),
            WorkerCommand::SetView(crate::presenter::ViewMode::Panorama)
        );
    }

    #[test]
    fn step_direction_validation_accepts_only_plus_minus_one() {
        assert!(validate_step_direction(1).is_ok());
        assert!(validate_step_direction(-1).is_ok());
        for bad in [0, 2, -2, 30, i32::MAX, i32::MIN] {
            match validate_step_direction(bad) {
                Err(WorkerError::Unsupported { operation }) => {
                    assert!(operation.contains("expected -1 or +1"), "op: {operation}");
                }
                other => panic!("expected Unsupported for {bad}, got {other:?}"),
            }
        }
    }

    #[test]
    fn validate_input_path_rejects_empty_and_whitespace() {
        assert!(validate_input_path("/media/clip.mp4").is_ok());
        for bad in ["", "   ", "\t\n"] {
            match validate_input_path(bad) {
                Err(WorkerError::InvalidInput { field, reason }) => {
                    assert_eq!(field, "path");
                    assert!(reason.contains("must not be empty"), "reason: {reason}");
                }
                other => panic!("expected InvalidInput for {bad:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn validate_input_path_rejects_ffmpeg_protocol_prefixes() {
        // CR-01: the video-input path applies the same forbidden-prefix guard
        // as the profile path, so a crafted `set_input` cannot reach FFmpeg's
        // protocol handlers (`http://`, `concat:`, `pipe:`, `data:`).
        for bad in [
            "https://example.com/clip.mp4",
            "http://example.com/clip.mp4",
            "concat:seg1.mp4|seg2.mp4",
            "pipe:0",
            "data:video/mp4;base64,AAAA",
        ] {
            assert!(
                matches!(
                    validate_input_path(bad),
                    Err(WorkerError::InvalidInput { .. })
                ),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn set_input_command_round_trips_through_the_channel() {
        let (tx, rx) = mpsc::channel();
        let handle = WorkerHandle::new(tx);
        handle
            .send(WorkerCommand::SetInput {
                role: crate::events::InputRole::Left,
                path: "/media/a.mp4".to_string(),
            })
            .unwrap();
        assert_eq!(
            rx.recv().unwrap(),
            WorkerCommand::SetInput {
                role: crate::events::InputRole::Left,
                path: "/media/a.mp4".to_string()
            }
        );
    }

    #[test]
    fn validate_options_rejects_zero_frames_and_negative_skip_secs() {
        use crate::events::CalibrationOptions;

        // None for every field is valid (engine defaults).
        assert!(validate_options(&CalibrationOptions::default()).is_ok());
        assert!(
            validate_options(&CalibrationOptions {
                num_frames: Some(50),
                skip_start_secs: Some(1.5),
                skip_end_secs: Some(0.0),
                use_imu_rotation_seeds: Some(true),
            })
            .is_ok()
        );

        // Zero frames is rejected.
        match validate_options(&CalibrationOptions {
            num_frames: Some(0),
            ..Default::default()
        }) {
            Err(WorkerError::InvalidInput { field, reason }) => {
                assert_eq!(field, "num_frames");
                assert!(reason.contains("between 1 and 200"), "reason: {reason}");
            }
            other => panic!("expected InvalidInput for 0 frames, got {other:?}"),
        }

        // A negative skip is rejected.
        match validate_options(&CalibrationOptions {
            skip_start_secs: Some(-1.0),
            ..Default::default()
        }) {
            Err(WorkerError::InvalidInput { field, .. }) => assert_eq!(field, "skip_start_secs"),
            other => panic!("expected InvalidInput for negative skip, got {other:?}"),
        }

        // A non-finite skip is rejected.
        assert!(matches!(
            validate_options(&CalibrationOptions {
                skip_end_secs: Some(f64::NAN),
                ..Default::default()
            }),
            Err(WorkerError::InvalidInput { .. })
        ));
    }

    #[test]
    fn validate_profile_path_rejects_empty_and_non_local() {
        assert!(validate_profile_path("/home/op/match.json").is_ok());
        for bad in [
            "",
            "   ",
            "https://example.com/match.json",
            "pipe:0",
            "data:abc",
        ] {
            assert!(
                matches!(
                    validate_profile_path(bad),
                    Err(WorkerError::InvalidInput { .. })
                ),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn calibration_commands_round_trip_through_the_channel() {
        let (tx, rx) = mpsc::channel();
        let handle = WorkerHandle::new(tx);
        handle
            .send(WorkerCommand::ClearInput {
                role: crate::events::InputRole::Left,
            })
            .unwrap();
        handle
            .send(WorkerCommand::LensCandidates {
                role: crate::events::InputRole::Right,
            })
            .unwrap();
        handle
            .send(WorkerCommand::SetLensOverride {
                role: crate::events::InputRole::Left,
                candidate: crate::events::LensCandidate {
                    camera: "GoPro HERO10".to_string(),
                    lens: "Wide".to_string(),
                    width: 3840,
                    height: 2160,
                },
            })
            .unwrap();
        handle
            .send(WorkerCommand::ClearLensOverride {
                role: crate::events::InputRole::Left,
            })
            .unwrap();
        handle
            .send(WorkerCommand::StartCalibration {
                options: crate::events::CalibrationOptions::default(),
            })
            .unwrap();
        handle
            .send(WorkerCommand::LoadProfile {
                path: "/media/match.json".to_string(),
            })
            .unwrap();
        handle
            .send(WorkerCommand::SaveProfile {
                path: "/media/out.json".to_string(),
            })
            .unwrap();

        assert_eq!(
            rx.recv().unwrap(),
            WorkerCommand::ClearInput {
                role: crate::events::InputRole::Left
            }
        );
        assert_eq!(
            rx.recv().unwrap(),
            WorkerCommand::LensCandidates {
                role: crate::events::InputRole::Right
            }
        );
        assert!(matches!(
            rx.recv().unwrap(),
            WorkerCommand::SetLensOverride { .. }
        ));
        assert_eq!(
            rx.recv().unwrap(),
            WorkerCommand::ClearLensOverride {
                role: crate::events::InputRole::Left
            }
        );
        assert!(matches!(
            rx.recv().unwrap(),
            WorkerCommand::StartCalibration { .. }
        ));
        assert_eq!(
            rx.recv().unwrap(),
            WorkerCommand::LoadProfile {
                path: "/media/match.json".to_string()
            }
        );
        assert_eq!(
            rx.recv().unwrap(),
            WorkerCommand::SaveProfile {
                path: "/media/out.json".to_string()
            }
        );
    }

    #[test]
    fn validate_field_roi_accepts_a_normalized_polygon() {
        // In-range vertices (including the exact bounds) and a degenerate short
        // list are all accepted at the boundary; the worker decides what a short
        // list means (clear).
        assert!(
            validate_field_roi(
                &[[0.0, 1.0], [0.5, 0.5], [1.0, 0.0], [0.25, 0.75]],
                &[[0.1, 0.2]]
            )
            .is_ok()
        );
        assert!(validate_field_roi(&[], &[]).is_ok());
    }

    #[test]
    fn validate_field_roi_rejects_out_of_range_and_non_finite() {
        // T-04-12: an out-of-range or non-finite coordinate is rejected before
        // it can reach the engine.
        for (left, right) in [
            (vec![[1.2, 0.5]], vec![]),
            (vec![[0.5, -0.1]], vec![]),
            (vec![], vec![[0.0, f64::NAN]]),
            (vec![], vec![[f64::INFINITY, 0.5]]),
            (vec![], vec![[0.5, f64::NEG_INFINITY]]),
        ] {
            assert!(
                matches!(
                    validate_field_roi(&left, &right),
                    Err(WorkerError::InvalidInput { .. })
                ),
                "expected {left:?}/{right:?} to be rejected"
            );
        }
    }

    #[test]
    fn validate_field_roi_caps_the_vertex_count() {
        // T-04-13: an oversized vertex list from the webview is rejected.
        let big: Vec<[f64; 2]> = (0..=MAX_FIELD_ROI_VERTICES)
            .map(|i| [i as f64 / 100.0, 0.5])
            .collect();
        match validate_field_roi(&big, &[]) {
            Err(WorkerError::InvalidInput { field, reason }) => {
                assert_eq!(field, "field_roi.left");
                assert!(reason.contains("at most 64"), "reason: {reason}");
            }
            other => panic!("expected InvalidInput for an oversized list, got {other:?}"),
        }
        // Exactly the cap is fine.
        let at_cap: Vec<[f64; 2]> = (0..MAX_FIELD_ROI_VERTICES)
            .map(|i| [i as f64 / 100.0, 0.5])
            .collect();
        assert!(validate_field_roi(&at_cap, &[]).is_ok());
    }

    #[test]
    fn set_field_roi_command_round_trips_through_the_channel() {
        let (tx, rx) = mpsc::channel();
        let handle = WorkerHandle::new(tx);
        handle
            .send(WorkerCommand::SetFieldRoi {
                left: vec![[0.1, 0.2], [0.3, 0.4], [0.5, 0.6]],
                right: vec![],
            })
            .unwrap();
        assert_eq!(
            rx.recv().unwrap(),
            WorkerCommand::SetFieldRoi {
                left: vec![[0.1, 0.2], [0.3, 0.4], [0.5, 0.6]],
                right: vec![]
            }
        );
    }
}
