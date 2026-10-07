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

/// Run a typed export with the operator's preset/encoder/trim/variant (EXPT-01).
///
/// Thin, same contract as [`import`]: post `Export { settings }` and return.
/// Export runs on the worker thread (the engine's one-shot file→file job),
/// never on the webview/main thread. The typed settings are parsed with a
/// default-on-unknown inside the worker (T-05-01) — the command handler never
/// names an engine type.
///
/// # Errors
///
/// Returns [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command]
pub async fn export_with(
    state: tauri::State<'_, WorkerHandle>,
    settings: crate::events::ExportSettings,
) -> Result<(), WorkerError> {
    state.send(WorkerCommand::Export { settings })
}

/// Cancel a running export (EXPT-04).
///
/// # The documented control-plane bypass
///
/// Export runs synchronously on the worker thread (like calibration), so a
/// command posted to the worker's channel would not be observed until the
/// export already returned. This handler therefore sets the shared
/// [`Arc<AtomicBool>`](std::sync::atomic::AtomicBool) that `StitchJob::run`
/// polls **directly** — it deliberately posts nothing to the blocked channel.
/// The flag is distinct from the calibration flag, so a stray export cancel can
/// never abort a calibration (prohibition).
///
/// # Errors
///
/// Never fails; returns `Ok(())` so the frontend can always clear its
/// "Cancelling…" state.
#[tauri::command]
pub async fn cancel_export(
    state: tauri::State<'_, crate::worker::ExportCancel>,
) -> Result<(), WorkerError> {
    state.0.store(true, std::sync::atomic::Ordering::SeqCst);
    Ok(())
}

/// Probe the available encoders for a codec (EXPT-02).
///
/// Thin, same contract as [`import`]: post `ProbeEncoders { codec }` and
/// return; the worker emits the typed `EncoderList` event.
///
/// # Errors
///
/// Returns [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command]
pub async fn probe_encoders(
    state: tauri::State<'_, WorkerHandle>,
    codec: String,
) -> Result<(), WorkerError> {
    state.send(WorkerCommand::ProbeEncoders { codec })
}

/// Resolve and preview the deterministic output path for `settings` (EXPT-06).
///
/// Thin, same contract as [`import`]: post `PreviewExportPath { settings }` and
/// return; the worker resolves the path (directory + input stem + variant +
/// collision suffix) and emits a typed `ExportPathPreview`. The webview shows
/// the worker's path verbatim and never constructs one itself (T-05-08).
///
/// # Errors
///
/// Returns [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command]
pub async fn preview_export_path(
    state: tauri::State<'_, WorkerHandle>,
    settings: crate::events::ExportSettings,
) -> Result<(), WorkerError> {
    state.send(WorkerCommand::PreviewExportPath { settings })
}

/// Probe the system information for the System Info panel (DIAG-01).
///
/// Thin, same contract as [`import`]: post `SystemInfo` and return; the worker
/// emits the typed `SystemInfo` event. The webview never enumerates a GPU or an
/// encoder itself.
///
/// # Errors
///
/// Returns [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command]
pub async fn system_info(state: tauri::State<'_, WorkerHandle>) -> Result<(), WorkerError> {
    state.send(WorkerCommand::SystemInfo)
}

/// Run the runtime prerequisite preflight (DIAG-05).
///
/// Thin, same contract as [`import`]: post `RunPreflight` and return; the worker
/// emits the typed `Preflight` report with a remediation per failed item.
///
/// # Errors
///
/// Returns [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command]
pub async fn run_preflight(state: tauri::State<'_, WorkerHandle>) -> Result<(), WorkerError> {
    state.send(WorkerCommand::RunPreflight)
}

/// Write the one-click, redacted, local-only diagnostics bundle (DIAG-03).
///
/// Thin: validate the path, post `ExportDiagnosticsBundle { path }`. The worker
/// gathers the retained structured logs, the probed system info, the active
/// calibration profile, and the debug inspector payload, redacts the home path
/// and account name, and writes a single local zip **atomically** (no partial
/// bundle on failure). No network is touched and the path is the operator's own
/// choice; the worker emits a typed `DiagnosticsBundleWritten` or a
/// `Failed(DiagnosticsBundle)`.
///
/// # Errors
///
/// Returns [`WorkerError::InvalidInput`] for an empty/non-local path, or
/// [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command(rename_all = "snake_case")]
pub async fn export_diagnostics_bundle(
    state: tauri::State<'_, WorkerHandle>,
    path: String,
) -> Result<(), WorkerError> {
    validate_bundle_path(&path)?;
    state.send(WorkerCommand::ExportDiagnosticsBundle { path })
}

/// Save the current working state as a `.reco` project (PROJ-01).
///
/// Thin: validate the path, post `SaveProject { path, settings }`. The worker
/// assembles the manifest from its retained state plus the webview-owned export
/// settings, writes it atomically, and emits a typed `ProjectSaved` or a
/// `Failed(ProjectSave)`. The webview never writes a manifest itself.
///
/// # Errors
///
/// Returns [`WorkerError::InvalidInput`] for an empty/non-local path, or
/// [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command(rename_all = "snake_case")]
pub async fn save_project(
    state: tauri::State<'_, WorkerHandle>,
    path: String,
    settings: crate::events::ExportSettings,
) -> Result<(), WorkerError> {
    validate_project_path(&path)?;
    state.send(WorkerCommand::SaveProject { path, settings })
}

/// Open a `.reco` project and restore the whole working state (PROJ-01).
///
/// Thin: validate the path, post `OpenProject { path }`. The worker parses and
/// validates the manifest, probes each referenced input, and emits either a
/// typed `ProjectOpened` (full restore) or `ProjectMissingInputs` (a relocate
/// list) — a missing input never fails the open.
///
/// # Errors
///
/// Returns [`WorkerError::InvalidInput`] for an empty/non-local path, or
/// [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command(rename_all = "snake_case")]
pub async fn open_project(
    state: tauri::State<'_, WorkerHandle>,
    path: String,
) -> Result<(), WorkerError> {
    validate_project_path(&path)?;
    state.send(WorkerCommand::OpenProject { path })
}

/// Relocate one missing project input and re-run the restore (PROJ-01).
///
/// Thin: validate the path, post `RelocateProjectInput { role, path }`. The
/// worker replaces the named role's referenced path on the retained pending
/// project and re-probes; a complete set restores, a still-missing set re-emits
/// `ProjectMissingInputs`.
///
/// # Errors
///
/// Returns [`WorkerError::InvalidInput`] for an empty/non-local path, or
/// [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command(rename_all = "snake_case")]
pub async fn relocate_project_input(
    state: tauri::State<'_, WorkerHandle>,
    role: crate::events::InputRole,
    path: String,
) -> Result<(), WorkerError> {
    validate_project_path(&path)?;
    state.send(WorkerCommand::RelocateProjectInput { role, path })
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

/// Attach the webview binary channel for manual preview/validation frames
/// (MANU-03).
///
/// The manual flow streams its preview and validation RGBA over a binary
/// `Channel<Response>` rather than the JSON `worker-event-typed` bridge: a
/// bounded frame is ~2 MB, which serializes to ~8 MB of JSON numbers and made
/// the webview parse millions of numbers per frame (Phase 04.1 OOM). The
/// channel receives `[kind: u8][width: u32 LE][height: u32 LE][RGBA]` frames,
/// the readback convention plus a kind tag. Stores the channel in the shared
/// slot the worker's event sink already holds; a later attach replaces it.
///
/// # Errors
///
/// Currently infallible, but returns `Result` for a uniform command surface
/// with [`preview_attach_readback`].
#[tauri::command]
pub async fn manual_attach_preview(
    sender: tauri::State<'_, crate::worker::ManualFrameSender>,
    on_frame: tauri::ipc::Channel<tauri::ipc::Response>,
) -> Result<(), WorkerError> {
    sender.attach(on_frame);
    Ok(())
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

/// Validate an operator-supplied `.reco` path at the command boundary (PROJ-01).
///
/// A project is a local file chosen by the native dialog; the same
/// empty/forbidden-prefix guard as a profile path applies, so the worker never
/// opens a non-local path. Reuses [`validate_profile_path`] rather than
/// duplicating the prefix set.
fn validate_project_path(path: &str) -> Result<(), WorkerError> {
    validate_profile_path(path)
}

/// Validate an operator-supplied diagnostics-bundle path at the command
/// boundary (DIAG-03).
///
/// The bundle is a local zip chosen by the native save dialog; the same
/// empty/forbidden-prefix guard as a profile path applies, so the worker never
/// writes to a non-local path. Reuses [`validate_profile_path`] rather than
/// duplicating the prefix set.
fn validate_bundle_path(path: &str) -> Result<(), WorkerError> {
    validate_profile_path(path)
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

/// Refine the current profile's lens `k1` against the retained matches (INTR-03).
///
/// Thin, same contract as [`set_field_roi`]: validate the held-out fraction at
/// the boundary, post a typed `WorkerCommand::RefineLens`, and return. The
/// worker derives raw-pixel observations from the retained verified matches,
/// runs the reduced `k1` refinement behind the held-out guard, emits a typed
/// `IntrinsicsRefined`, and writes the profile's `k1` **only** when the result
/// is accepted. The refinement is opt-in — this command is the only trigger and
/// it is never called from the calibration wizard. Names no engine type.
///
/// # Why `rename_all = "snake_case"`
///
/// `heldout_fraction` is pinned to the crate's snake_case IPC protocol so a
/// future argument rename cannot reintroduce the silent-never-fires bug
/// recorded in `crates/reco-app/FRICTION.md` A7.
///
/// # Errors
///
/// Returns [`WorkerError::InvalidInput`] for a non-finite or out-of-range
/// fraction, or [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command(rename_all = "snake_case")]
pub async fn refine_lens(
    state: tauri::State<'_, WorkerHandle>,
    heldout_fraction: f64,
) -> Result<(), WorkerError> {
    validate_heldout_fraction(heldout_fraction)?;
    state.send(WorkerCommand::RefineLens { heldout_fraction })
}

/// Validate the held-out fraction at the command boundary (INTR-03).
///
/// The fraction must be finite and strictly inside `(0, 1)`: `0` would leave no
/// guard set and `1` no fit set, so either would defeat the held-out guard.
/// Extracted as a pure function so the boundary check is unit-testable without a
/// Tauri `State`.
fn validate_heldout_fraction(fraction: f64) -> Result<(), WorkerError> {
    if !fraction.is_finite() || fraction <= 0.0 || fraction >= 1.0 {
        return Err(WorkerError::InvalidInput {
            field: "heldout_fraction".to_string(),
            reason: "must be a finite fraction strictly between 0 and 1".to_string(),
        });
    }
    Ok(())
}

/// Open a manual calibration session at `frame` (MANU-01 / MANU-03).
///
/// Thin, same contract as [`set_field_roi`]: post a typed
/// `WorkerCommand::ManualBegin` and return. The worker extracts the reference
/// frame pair, retains its YUV planes, emits a typed `ManualSessionStarted`, and
/// streams one binary preview per camera rendered under real `CameraParams` over
/// the channel attached by [`manual_attach_preview`]. Names no engine type.
///
/// # Why `rename_all = "snake_case"`
///
/// `frame` is a single word today, but the command is pinned to the crate's
/// snake_case IPC protocol so a future argument rename cannot reintroduce the
/// silent-never-fires bug recorded in `crates/reco-app/FRICTION.md` A7.
///
/// # Errors
///
/// Returns [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command(rename_all = "snake_case")]
pub async fn manual_begin(
    state: tauri::State<'_, WorkerHandle>,
    frame: u64,
) -> Result<(), WorkerError> {
    state.send(WorkerCommand::ManualBegin { frame })
}

/// Change the manual session's reference frame (MANU-03).
///
/// Thin, same contract as [`manual_begin`]: post `ManualSetFrame`; the worker
/// clamps the index against the probed frame count (T-04.1-01), re-extracts
/// only when the frame changed, and re-renders the preview.
///
/// # Errors
///
/// Returns [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command(rename_all = "snake_case")]
pub async fn manual_set_frame(
    state: tauri::State<'_, WorkerHandle>,
    frame: u64,
) -> Result<(), WorkerError> {
    state.send(WorkerCommand::ManualSetFrame { frame })
}

/// Close the manual calibration session (MANU-01).
///
/// Thin, same contract as [`manual_begin`]: post `ManualExit`; the worker drops
/// the retained reference-frame planes (T-04.1-02) and emits the session-ended
/// state. Exiting never touches an existing calibration profile.
///
/// # Errors
///
/// Returns [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command]
pub async fn manual_exit(state: tauri::State<'_, WorkerHandle>) -> Result<(), WorkerError> {
    state.send(WorkerCommand::ManualExit)
}

/// Run the manual flow's audio auto-sync on the worker (MANU-02).
///
/// Thin, same contract as [`manual_begin`]: post `ManualDetectSync`; the worker
/// extracts each clip's PCM, cross-correlates via the engine, and emits a typed
/// `AudioSyncResult` (or an "unavailable" result with `confidence: None`). The
/// command returns before the estimate lands — the webview mirrors the event,
/// never the invocation.
///
/// # Errors
///
/// Returns [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command]
pub async fn manual_detect_sync(state: tauri::State<'_, WorkerHandle>) -> Result<(), WorkerError> {
    state.send(WorkerCommand::ManualDetectSync)
}

/// Set the manual sync offset in frames (MANU-02).
///
/// Thin, same contract as [`manual_begin`]: post `ManualSetSync`; the worker
/// records the offset on the manual session with `SyncMethod::Manual`
/// provenance and emits a typed `ManualSyncSet`. The offset feeds the later
/// pin/bend solves.
///
/// # Why `rename_all = "snake_case"`
///
/// `offset_frames` is multi-word; the IPC key must stay snake_case to match the
/// crate's protocol, or the handler silently never fires (`FRICTION.md` A7).
///
/// # Errors
///
/// Returns [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command(rename_all = "snake_case")]
pub async fn manual_set_sync(
    state: tauri::State<'_, WorkerHandle>,
    offset_frames: i64,
) -> Result<(), WorkerError> {
    state.send(WorkerCommand::ManualSetSync { offset_frames })
}

/// Reject a non-finite pin coordinate at the command boundary (T-04.1-10).
///
/// The worker clamps coordinates to the frame bounds, but a `NaN`/`inf` survives
/// `f64::clamp` and would poison the solve. This is the boundary check; the
/// worker owns the bounds clamp.
fn validate_pin_point(px: [f64; 2], field: &str) -> Result<(), WorkerError> {
    if px.iter().any(|v| !v.is_finite()) {
        return Err(WorkerError::InvalidInput {
            field: field.to_string(),
            reason: "pin coordinates must be finite".to_string(),
        });
    }
    Ok(())
}

/// Add one correspondence pin to the manual session (MANU-03).
///
/// Thin, same contract as [`manual_begin`]: validate the points are finite, then
/// post `ManualAddPin`. The worker clamps them to the frame bounds, emits the
/// updated pin list plus an instant preview, and arms the debounced background
/// solve.
///
/// # Errors
///
/// * [`WorkerError::InvalidInput`] if a coordinate is not finite.
/// * [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command(rename_all = "snake_case")]
pub async fn manual_add_pin(
    state: tauri::State<'_, WorkerHandle>,
    left_px: [f64; 2],
    right_px: [f64; 2],
) -> Result<(), WorkerError> {
    validate_pin_point(left_px, "left_px")?;
    validate_pin_point(right_px, "right_px")?;
    state.send(WorkerCommand::ManualAddPin { left_px, right_px })
}

/// Move one side of an existing pin (MANU-03).
///
/// Thin, same contract as [`manual_add_pin`]; re-arms the debounced solve.
///
/// # Errors
///
/// * [`WorkerError::InvalidInput`] if the coordinate is not finite.
/// * [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command(rename_all = "snake_case")]
pub async fn manual_move_pin(
    state: tauri::State<'_, WorkerHandle>,
    id: u32,
    side: crate::events::ManualSide,
    px: [f64; 2],
) -> Result<(), WorkerError> {
    validate_pin_point(px, "px")?;
    state.send(WorkerCommand::ManualMovePin { id, side, px })
}

/// Remove one pin (MANU-03).
///
/// Thin, same contract as [`manual_begin`].
///
/// # Errors
///
/// Returns [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command(rename_all = "snake_case")]
pub async fn manual_remove_pin(
    state: tauri::State<'_, WorkerHandle>,
    id: u32,
) -> Result<(), WorkerError> {
    state.send(WorkerCommand::ManualRemovePin { id })
}

/// Remove every pin from the manual session (MANU-03).
///
/// Thin, same contract as [`manual_begin`].
///
/// # Errors
///
/// Returns [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command]
pub async fn manual_clear_pins(state: tauri::State<'_, WorkerHandle>) -> Result<(), WorkerError> {
    state.send(WorkerCommand::ManualClearPins)
}

/// Reject a non-finite handle value at the command boundary (T-04.1-13).
///
/// The worker clamps each handle to its travel range, but a `NaN`/`inf` survives
/// `f64::clamp` and would poison the stored `CameraParams`/layout. This is the
/// boundary check; the worker owns the travel clamp.
fn validate_manual_scalar(value: f64, field: &str) -> Result<(), WorkerError> {
    if !value.is_finite() {
        return Err(WorkerError::InvalidInput {
            field: field.to_string(),
            reason: "handle value must be finite".to_string(),
        });
    }
    Ok(())
}

/// Apply an on-image lens-handle edit to one camera (MANU-05).
///
/// Thin, same contract as [`manual_begin`]: validate the values are finite, then
/// post `ManualSetLens`. The worker clamps each value to its travel range,
/// enforces `fy = fx`, persists the edited `CameraParams` into the calibration,
/// re-renders the instant preview without re-solving, and arms the debounced
/// background re-solve.
///
/// # Errors
///
/// * [`WorkerError::InvalidInput`] if a value is not finite.
/// * [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command(rename_all = "snake_case")]
pub async fn manual_set_lens(
    state: tauri::State<'_, WorkerHandle>,
    side: crate::events::ManualSide,
    fx: f64,
    cx: f64,
    cy: f64,
    k1: f64,
) -> Result<(), WorkerError> {
    validate_manual_scalar(fx, "fx")?;
    validate_manual_scalar(cx, "cx")?;
    validate_manual_scalar(cy, "cy")?;
    validate_manual_scalar(k1, "k1")?;
    state.send(WorkerCommand::ManualSetLens {
        side,
        fx,
        cx,
        cy,
        k1,
    })
}

/// Apply a constrained layout-handle edit (MANU-06).
///
/// Thin, same contract as [`manual_begin`]: validate the values are finite, then
/// post `ManualSetLayout`. The worker clamps each value to its travel range and
/// confirms the edit with a debounced background solve.
///
/// # Errors
///
/// * [`WorkerError::InvalidInput`] if a value is not finite.
/// * [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command(rename_all = "snake_case")]
pub async fn manual_set_layout(
    state: tauri::State<'_, WorkerHandle>,
    cam_d: f64,
    intersect: f64,
    x_ty: f64,
    x_rz: f64,
) -> Result<(), WorkerError> {
    validate_manual_scalar(cam_d, "cam_d")?;
    validate_manual_scalar(intersect, "intersect")?;
    validate_manual_scalar(x_ty, "x_ty")?;
    validate_manual_scalar(x_rz, "x_rz")?;
    state.send(WorkerCommand::ManualSetLayout {
        cam_d,
        intersect,
        x_ty,
        x_rz,
    })
}

/// Restore the lens intrinsics captured at `manual_begin` (MANU-05).
///
/// Thin, same contract as [`manual_begin`].
///
/// # Errors
///
/// Returns [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command]
pub async fn manual_reset_lens(state: tauri::State<'_, WorkerHandle>) -> Result<(), WorkerError> {
    state.send(WorkerCommand::ManualResetLens)
}

/// Restore the rig layout captured at `manual_begin` (MANU-06).
///
/// Thin, same contract as [`manual_begin`].
///
/// # Errors
///
/// Returns [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command]
pub async fn manual_reset_rig(state: tauri::State<'_, WorkerHandle>) -> Result<(), WorkerError> {
    state.send(WorkerCommand::ManualResetRig)
}

/// Validate the manual result on one additional frame (MANU-07).
///
/// Thin, same contract as [`manual_begin`]: post `ManualValidate`. The worker
/// extracts the validation frame pair (applying the session's sync offset to
/// the right index), renders the stitched comparison under the current manual
/// parameters/layout, runs the engine on the frame to obtain a per-frame
/// residual, and emits a typed `ManualValidationFrame` with an advisory
/// verdict. The index is typed `u32` (no negative/string) and is clamped inside
/// the worker against the probed frame count, mirroring `seek` (T-04.1-16).
///
/// # Why `rename_all = "snake_case"`
///
/// Pins the command to the crate's snake_case IPC protocol so a future argument
/// rename cannot reintroduce the silent-never-fires bug (`FRICTION.md` A7).
///
/// # Errors
///
/// Returns [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command(rename_all = "snake_case")]
pub async fn manual_validate(
    state: tauri::State<'_, WorkerHandle>,
    frame: u32,
) -> Result<(), WorkerError> {
    state.send(WorkerCommand::ManualValidate { frame })
}

/// Save the manual result as a normal calibration profile (MANU-07).
///
/// Thin, same contract as [`save_profile`]: validate the path, post
/// `ManualSave`. The worker assembles a `MatchCalibration` from the session's
/// parameters, layout, sync offset, and the carried profile fields, gates the
/// write on `MatchCalibration::validate`, writes it through the existing
/// `.json` path, and emits a typed `ManualSaved`. An invalid profile is never
/// written (T-04.1-16); a repeated save overwrites the target.
///
/// # Errors
///
/// Returns [`WorkerError::InvalidInput`] for an empty/non-local path, or
/// [`WorkerError::ChannelClosed`] if the worker has already exited.
#[tauri::command(rename_all = "snake_case")]
pub async fn manual_save(
    state: tauri::State<'_, WorkerHandle>,
    path: String,
) -> Result<(), WorkerError> {
    validate_profile_path(&path)?;
    state.send(WorkerCommand::ManualSave { path })
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

    /// Run a typed export described by `settings` (EXPT-01).
    ///
    /// Replaces Phase 1's fixed-path `Export`: the settings carry the preset
    /// parameters, encoder override, trim window, variant, and output directory.
    /// The worker builds a `reco_io::StitchJob` from them and emits typed
    /// progress/finished/cancelled/failed events. A job: it blocks the worker
    /// thread until the run finishes or the shared export-cancel flag is set.
    Export {
        /// The typed export request.
        settings: crate::events::ExportSettings,
    },

    /// Cancel a running export (EXPT-04).
    ///
    /// A typed mirror of the `cancel_export` command; because the export blocks
    /// the worker loop, the real cancel path writes the shared
    /// [`ExportCancel`](crate::worker::ExportCancel) flag directly. This command
    /// exists for protocol completeness and the same direct-write handler.
    CancelExport,

    /// Probe the available encoders for `codec` and emit an `EncoderList`
    /// (EXPT-02).
    ///
    /// A cheap job: the worker enumerates encoders via FFmpeg and reports them
    /// in preference order (hardware first).
    ProbeEncoders {
        /// The codec name to enumerate encoders for (`"h264"` / `"hevc"` / `"av1"`).
        codec: String,
    },

    /// Resolve and emit the deterministic output path preview (EXPT-06).
    ///
    /// A cheap job: the worker resolves the collision-free path for the current
    /// settings (directory + input stem + variant) and emits an
    /// `ExportPathPreview`. No encoder, no calibration, no file is written.
    PreviewExportPath {
        /// The typed export request the path is resolved for.
        settings: crate::events::ExportSettings,
    },

    /// Probe the system for the System Info panel (DIAG-01).
    ///
    /// A cheap job: the worker reads its own GPU context, enumerates encoders,
    /// and scans for camera devices, then emits a typed `SystemInfo`. The
    /// webview never enumerates a GPU or an encoder itself.
    SystemInfo,

    /// Run the runtime prerequisite preflight (DIAG-05).
    ///
    /// A cheap job: the worker probes FFmpeg, ONNX Runtime (when detection is
    /// built), and the webview runtime, then emits a typed `Preflight` report.
    RunPreflight,

    /// Write the one-click, redacted, local-only diagnostics bundle (DIAG-03).
    ///
    /// A job: the worker gathers the retained structured logs, the probed
    /// system info, the active calibration profile, and the debug inspector
    /// payload, redacts the home path/account name, and writes a single local
    /// zip at `path` atomically (no partial bundle on failure), then emits
    /// `DiagnosticsBundleWritten`. Nothing here touches the network.
    ExportDiagnosticsBundle {
        /// The local `.zip` path the bundle is written to.
        path: String,
    },

    /// Save the current working state as a `.reco` project (PROJ-01).
    ///
    /// A job: the worker assembles a `RecoProject` from its retained state
    /// (inputs, lens overrides, calibration path + inline snapshot, pose) plus
    /// the `settings` the webview owns (the export settings live in the webview,
    /// so it passes them with the command — FIFO ordering means the value is
    /// current), writes it atomically, and emits `ProjectSaved`. No media is
    /// copied.
    SaveProject {
        /// The local `.reco` path to write.
        path: String,
        /// The webview's current export settings to persist.
        settings: crate::events::ExportSettings,
    },

    /// Open a `.reco` project and restore the whole working state (PROJ-01).
    ///
    /// A job: the worker parses and validates the manifest, probes each
    /// referenced input, and either restores everything and emits
    /// `ProjectOpened`, or emits `ProjectMissingInputs` (retaining the parsed
    /// project) — never a partial restore, never a failure on a missing input.
    OpenProject {
        /// The local `.reco` path to read.
        path: String,
    },

    /// Relocate one missing project input and re-run the restore (PROJ-01).
    ///
    /// A job: the worker replaces the named role's referenced path on the
    /// retained pending project and re-probes; a complete set restores and emits
    /// `ProjectOpened`, a still-missing set re-emits `ProjectMissingInputs`.
    RelocateProjectInput {
        /// Which input to relocate.
        role: crate::events::InputRole,
        /// The replacement local file path.
        path: String,
    },

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

    /// Refine the current profile's lens `k1` against the retained matches
    /// (INTR-03).
    ///
    /// An opt-in diagnostic: the worker derives raw-pixel observations from the
    /// retained verified matches, runs the reduced `k1` refinement behind the
    /// held-out guard, emits a typed `IntrinsicsRefined`, and writes the
    /// profile's `k1` onto both cameras **only** when the result is accepted
    /// (T-04.2-10). It is never invoked by the calibration wizard (T-04.2-11).
    RefineLens {
        /// Fraction of observations reserved as the held-out guard set.
        ///
        /// Validated to be finite and strictly inside `(0, 1)` at the command
        /// boundary; the engine clamps the split to at least one observation on
        /// each side.
        heldout_fraction: f64,
    },

    /// Open a manual calibration session at `frame` (MANU-01 / MANU-03).
    ///
    /// The worker extracts the reference frame for both clips, retains the YUV
    /// planes for the session's duration, seeds the per-camera `CameraParams`
    /// from the loaded profile (or a neutral default), emits a typed
    /// `ManualSessionStarted`, and streams one binary preview per camera over the
    /// `manual_attach_preview` channel. The frame index is clamped against the
    /// probed frame count (T-04.1-01). No calibration `.json` is required to
    /// begin.
    ManualBegin {
        /// The reference frame index (0-based; clamped by the worker).
        frame: u64,
    },

    /// Change the manual session's reference frame (MANU-03).
    ///
    /// The worker clamps the index, re-extracts the frame pair only when the
    /// index changed, and re-renders the preview under the current parameters.
    ManualSetFrame {
        /// The new reference frame index (0-based; clamped by the worker).
        frame: u64,
    },

    /// Close the manual calibration session (MANU-01).
    ///
    /// The worker drops the retained reference-frame planes (T-04.1-02) and
    /// emits the session-ended state. It never touches an existing calibration
    /// profile, so exiting the flow is never a dead end.
    ManualExit,

    /// Run the manual flow's audio auto-sync (MANU-02).
    ///
    /// The worker extracts each clip's PCM and cross-correlates via the engine,
    /// then emits a typed `AudioSyncResult`. On failure it emits an
    /// "unavailable" result (`confidence: None`, offset 0) rather than a
    /// fabricated zero, so the UI can require an explicit manual offset.
    ManualDetectSync,

    /// Set the manual sync offset in frames (MANU-02).
    ///
    /// The worker records the offset on the open manual session with
    /// `SyncMethod::Manual` provenance and emits a typed `ManualSyncSet`. That
    /// offset is what the later pin/bend solves use.
    ManualSetSync {
        /// The operator-chosen offset in frames (signed).
        offset_frames: i64,
    },

    /// Add one correspondence pin to the manual session (MANU-03).
    ///
    /// The worker clamps both points to the session's frame bounds (T-04.1-10),
    /// emits the updated `ManualPins` plus an instant preview, marks the solve
    /// busy, and arms a debounced background solve. The solve never runs from
    /// this command directly.
    ManualAddPin {
        /// Clicked point on the left frame, `[x, y]` pixels.
        left_px: [f64; 2],
        /// Corresponding point on the right frame, `[x, y]` pixels.
        right_px: [f64; 2],
    },

    /// Move one side of an existing pin (MANU-03).
    ///
    /// Re-arms the debounced background solve exactly like [`Self::ManualAddPin`].
    ManualMovePin {
        /// The pin's stable id.
        id: u32,
        /// Which side of the pin moved.
        side: crate::events::ManualSide,
        /// The new point on that side, `[x, y]` pixels.
        px: [f64; 2],
    },

    /// Remove one pin (MANU-03).
    ///
    /// Removing the last pin leaves an empty set, which never triggers a solve.
    ManualRemovePin {
        /// The pin's stable id.
        id: u32,
    },

    /// Remove every pin from the manual session (MANU-03).
    ///
    /// An empty set never triggers a solve (the UI shows the empty prompt).
    ManualClearPins,

    /// Apply an on-image lens-handle edit to one camera (MANU-05).
    ///
    /// The worker clamps each value against the baseline captured at
    /// `manual_begin` (k1 ±0.3, cx/cy ±10% of the frame, fx ±15% floored at
    /// 5 px), enforces `fy = fx`, persists the edited `CameraParams` into
    /// `current_calibration.left`/`.right`, re-renders the instant preview under
    /// the edited intrinsics **without re-solving**, and arms the debounced
    /// background re-solve. The solve never runs from this command.
    ManualSetLens {
        /// Which camera's intrinsics the edit applies to.
        side: crate::events::ManualSide,
        /// Requested focal length x in pixels (scale mode; `fy` mirrors it).
        fx: f64,
        /// Requested principal point x in pixels.
        cx: f64,
        /// Requested principal point y in pixels.
        cy: f64,
        /// Requested first-order distortion coefficient.
        k1: f64,
    },

    /// Apply a constrained layout-handle edit (MANU-06).
    ///
    /// Each value is clamped to its travel range (x_ty ±0.1, x_rz ±0.3 rad,
    /// intersect 0–1, cam_d 0.1–0.30), written into `current_calibration.layout`,
    /// re-rendered instantly, and confirmed by a debounced background solve.
    ManualSetLayout {
        /// Requested `camera_axis_offset` (cam_d).
        cam_d: f64,
        /// Requested overlap ratio `intersect`.
        intersect: f64,
        /// Requested right-plane Y translation `x_ty`.
        x_ty: f64,
        /// Requested right-plane Z rotation `x_rz` (radians).
        x_rz: f64,
    },

    /// Restore the lens intrinsics captured at `manual_begin` (MANU-05).
    ManualResetLens,

    /// Restore the rig layout captured at `manual_begin` (MANU-06).
    ManualResetRig,

    /// Validate the manual result on one additional frame (MANU-07).
    ///
    /// The worker extracts the validation frame pair (the right index carries
    /// the session's sync offset), renders the stitched comparison under the
    /// current manual parameters/layout, runs the engine on the frame for a
    /// per-frame residual, computes the advisory verdict, and emits a typed
    /// `ManualValidationFrame`. The index is clamped against the probed frame
    /// count. Validation is advisory, never a gate.
    ManualValidate {
        /// The validation frame index (0-based; clamped by the worker).
        frame: u32,
    },

    /// Save the manual result as a normal calibration profile (MANU-07).
    ///
    /// The worker assembles a `MatchCalibration` from the session's parameters,
    /// layout, sync offset, and the carried profile fields (`field_roi`,
    /// `lens_correction_amount`, `blend_width`), validates it with
    /// `MatchCalibration::validate`, and writes it through the existing `.json`
    /// path. Manual and auto profiles are identical in shape; a repeated save
    /// overwrites the target, never appends.
    ManualSave {
        /// The local file path to write.
        path: String,
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

    #[test]
    fn refine_lens_command_round_trips_through_the_channel() {
        // INTR-03: the opt-in refinement crosses as a typed command with its
        // held-out fraction, never a string.
        let (tx, rx) = mpsc::channel();
        let handle = WorkerHandle::new(tx);
        handle
            .send(WorkerCommand::RefineLens {
                heldout_fraction: 0.2,
            })
            .unwrap();
        assert_eq!(
            rx.recv().unwrap(),
            WorkerCommand::RefineLens {
                heldout_fraction: 0.2
            }
        );
    }

    #[test]
    fn validate_heldout_fraction_rejects_out_of_range_and_non_finite() {
        // INTR-03: a fraction outside (0, 1) or non-finite would defeat the
        // held-out guard, so the boundary rejects it before it reaches the worker.
        assert!(validate_heldout_fraction(0.2).is_ok());
        for bad in [0.0, 1.0, -0.1, 1.5, f64::NAN, f64::INFINITY] {
            assert!(
                matches!(
                    validate_heldout_fraction(bad),
                    Err(WorkerError::InvalidInput { .. })
                ),
                "expected {bad} to be rejected"
            );
        }
    }

    #[test]
    fn manual_commands_round_trip_through_the_channel() {
        // MANU-01 / MANU-03: the manual session protocol crosses the channel as
        // typed commands, never strings.
        let (tx, rx) = mpsc::channel();
        let handle = WorkerHandle::new(tx);
        handle
            .send(WorkerCommand::ManualBegin { frame: 12 })
            .unwrap();
        handle
            .send(WorkerCommand::ManualSetFrame { frame: 34 })
            .unwrap();
        handle.send(WorkerCommand::ManualExit).unwrap();
        handle.send(WorkerCommand::ManualDetectSync).unwrap();
        handle
            .send(WorkerCommand::ManualSetSync { offset_frames: -7 })
            .unwrap();
        handle
            .send(WorkerCommand::ManualAddPin {
                left_px: [1.0, 2.0],
                right_px: [3.0, 4.0],
            })
            .unwrap();
        handle
            .send(WorkerCommand::ManualMovePin {
                id: 2,
                side: crate::events::ManualSide::Right,
                px: [5.0, 6.0],
            })
            .unwrap();
        handle
            .send(WorkerCommand::ManualRemovePin { id: 2 })
            .unwrap();
        handle.send(WorkerCommand::ManualClearPins).unwrap();
        assert_eq!(rx.recv().unwrap(), WorkerCommand::ManualBegin { frame: 12 });
        assert_eq!(
            rx.recv().unwrap(),
            WorkerCommand::ManualSetFrame { frame: 34 }
        );
        assert_eq!(rx.recv().unwrap(), WorkerCommand::ManualExit);
        assert_eq!(rx.recv().unwrap(), WorkerCommand::ManualDetectSync);
        assert_eq!(
            rx.recv().unwrap(),
            WorkerCommand::ManualSetSync { offset_frames: -7 }
        );
        assert_eq!(
            rx.recv().unwrap(),
            WorkerCommand::ManualAddPin {
                left_px: [1.0, 2.0],
                right_px: [3.0, 4.0]
            }
        );
        assert_eq!(
            rx.recv().unwrap(),
            WorkerCommand::ManualMovePin {
                id: 2,
                side: crate::events::ManualSide::Right,
                px: [5.0, 6.0]
            }
        );
        assert_eq!(rx.recv().unwrap(), WorkerCommand::ManualRemovePin { id: 2 });
        assert_eq!(rx.recv().unwrap(), WorkerCommand::ManualClearPins);
    }

    #[test]
    fn manual_validate_and_save_round_trip_through_the_channel() {
        // MANU-07: the validation frame index and the save path cross as typed
        // values.
        let (tx, rx) = mpsc::channel();
        let handle = WorkerHandle::new(tx);
        handle
            .send(WorkerCommand::ManualValidate { frame: 42 })
            .unwrap();
        handle
            .send(WorkerCommand::ManualSave {
                path: "/media/manual.json".to_string(),
            })
            .unwrap();
        assert_eq!(
            rx.recv().unwrap(),
            WorkerCommand::ManualValidate { frame: 42 }
        );
        assert_eq!(
            rx.recv().unwrap(),
            WorkerCommand::ManualSave {
                path: "/media/manual.json".to_string()
            }
        );
    }

    #[test]
    fn validate_pin_point_rejects_non_finite_coordinates() {
        // T-04.1-10: a NaN/inf pin would survive the worker's bounds clamp and
        // poison the solve, so the boundary rejects it.
        assert!(validate_pin_point([1.0, 2.0], "px").is_ok());
        assert!(validate_pin_point([f64::NAN, 2.0], "px").is_err());
        assert!(validate_pin_point([1.0, f64::INFINITY], "px").is_err());
    }
}
