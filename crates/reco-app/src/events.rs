//! Typed events emitted by the engine worker back to the UI (D-06).
//!
//! # The message-passing contract
//!
//! The engine worker owns every engine object for the app's lifetime
//! (FOUND-03). It never hands an engine value to the UI thread and the UI
//! never holds an engine lock across a tick. Instead the worker reports what
//! happened as a stream of [`WorkerEvent`] values over an
//! [`std::sync::mpsc`](std::sync::mpsc) channel; the Tauri bridge (Plan 04)
//! drains that channel on an async task and forwards each event to the webview
//! via `app.emit`.
//!
//! ```text
//!   engine worker thread                          UI thread / webview
//!   ─────────────────────                         ───────────────────
//!   engine call ──▶ WorkerEvent ──▶ Sender ──▶ mpsc ──▶ Tauri bridge ──▶ emit
//! ```
//!
//! # Shape
//!
//! [`WorkerEvent`] mirrors the transport-agnostic serde vocabulary of
//! `reco_control::ControlIntent` (`crates/reco-control/src/lib.rs:58-81`):
//! internally tagged (`kind` / `data`), snake-case, and `#[non_exhaustive]` so
//! new event categories can be added without breaking every consumer's match
//! arm (UI-SPEC Event Log Contract).
//!
//! Failures cross the channel as a typed [`WorkerError`] — never a `String`.
//! The UI renders `err.to_string()` as the human-readable message, but the
//! value stays typed so a later consumer can branch on it
//! (AGENTS.md / CONVENTIONS.md:113).

/// Severity level for an event-log line (UI-SPEC Event Log Contract).
///
/// The webview maps these one-to-one onto the three log colours: `Info` →
/// `#c8c8c8`, `Warn` → `#e0a02e` (degradation, e.g. the Wayland presenter
/// fallback or a soft-encoder fallback), `Error` → `#e5484d`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Level {
    /// Normal event line: command accepted, work started or finished.
    Info,
    /// Non-fatal degradation: a fallback path was taken.
    Warn,
    /// A command was rejected or the engine failed.
    Error,
}

/// Which camera input a selection or its metadata belongs to (IMPT-01 / D3-03).
///
/// Left/right is semantically load-bearing for stitching, so the role is
/// explicit and never auto-guessed. Serializes snake_case (`"left"` / `"right"`)
/// and mirrors into `ui/src/lib/types.ts`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputRole {
    /// Camera A — the left feed.
    Left,
    /// Camera B — the right feed.
    Right,
}

impl InputRole {
    /// The user-facing slot title (UI-SPEC Copywriting Contract).
    pub fn label(self) -> &'static str {
        match self {
            InputRole::Left => "Camera A (left)",
            InputRole::Right => "Camera B (right)",
        }
    }

    /// The snake_case name used in log lines and typed payloads.
    pub fn name(self) -> &'static str {
        match self {
            InputRole::Left => "left",
            InputRole::Right => "right",
        }
    }
}

/// Which camera's preview frame a manual-calibration payload belongs to
/// (MANU-03).
///
/// Left/right is semantically load-bearing (each side renders under its own
/// `CameraParams`), so the side is explicit and never inferred. Serializes
/// snake_case (`"left"` / `"right"`) and mirrors into `ui/src/lib/types.ts`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManualSide {
    /// The left camera's preview frame.
    Left,
    /// The right camera's preview frame.
    Right,
}

/// Whether a metadata value was read directly from the source or derived
/// (IMPT-02 / D-05 provenance-over-guessing).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    /// Read directly from the container/stream.
    Probed,
    /// Derived (e.g. duration from frame count ÷ fps); never authoritative.
    Estimated,
}

/// One metadata field with its provenance (IMPT-02).
///
/// `value` is `None` when the field is genuinely unknown; the UI renders that
/// as an em-dash, never `0` or blank.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MetadataField {
    /// The display value, or `None` when unknown.
    pub value: Option<String>,
    /// How the value was obtained.
    pub provenance: Provenance,
}

impl MetadataField {
    /// A value read directly from the source.
    pub fn probed(value: impl Into<String>) -> Self {
        Self {
            value: Some(value.into()),
            provenance: Provenance::Probed,
        }
    }

    /// A derived value; the UI tags it visibly (IMPT-02).
    pub fn estimated(value: impl Into<String>) -> Self {
        Self {
            value: Some(value.into()),
            provenance: Provenance::Estimated,
        }
    }

    /// An unknown field: no value. Provenance records how it *would* have been
    /// obtained had it been present, so the UI can still render the tag.
    pub fn missing(provenance: Provenance) -> Self {
        Self {
            value: None,
            provenance,
        }
    }
}

/// The probed metadata for one input, each field tagged with provenance
/// (IMPT-02). Mirrors into `ui/src/lib/types.ts`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct InputMetadata {
    /// Frame resolution, e.g. `1920×1080`.
    pub resolution: MetadataField,
    /// Frame rate, e.g. `30 fps`.
    pub fps: MetadataField,
    /// Duration, derived from frame count ÷ fps when the container omits it.
    pub duration: MetadataField,
    /// Codec name (populated in plan 03-02).
    pub codec: MetadataField,
}

/// Severity of one readiness finding (CALB-05).
///
/// Declared least-severe-last so a plain `sort` puts blocking-shape findings
/// first, likely-quality next, informational last. Serializes snake_case.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ReadinessSeverity {
    /// A shape mismatch — a run will almost certainly fail.
    BlockingShape,
    /// Compatible shape but likely to reduce stitch quality.
    LikelyQuality,
    /// Informational only; never a reason to avoid a run.
    Informational,
}

/// Which readiness check produced a finding (CALB-05).
///
/// A superset of the Phase-3 [`CompatibilityCode`]: the same shape checks plus
/// the sampled exposure/overlap estimates and lens-profile availability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ReadinessCode {
    /// The two clips have different frame resolutions.
    ResolutionMismatch,
    /// The two clips have different aspect ratios (tolerance ~1%).
    AspectMismatch,
    /// The two clips' frame rates differ by more than 0.5 fps.
    FpsMismatch,
    /// The two clips use different codecs.
    CodecMismatch,
    /// A lens override's resolution does not match the input's.
    LensResolutionMismatch,
    /// Both slots point at the same file.
    SameFile,
    /// The sampled exposure differs by more than the configured threshold.
    ExposureMismatch,
    /// The sampled overlap estimate is below the usable threshold.
    LowOverlap,
    /// No lens profile matched either camera.
    LensUnavailable,
}

/// One readiness finding (CALB-05).
///
/// `message` is already user-facing: the webview renders it verbatim and never
/// invents a reason (T-04-08). `estimated` marks a value derived from a sampled
/// frame pair rather than a directly measured property.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ReadinessFinding {
    /// Which check produced this finding.
    pub code: ReadinessCode,
    /// How serious the finding is (drives the banner group).
    pub severity: ReadinessSeverity,
    /// Human-readable reason.
    pub message: String,
    /// Whether the underlying value was estimated from a sample.
    pub estimated: bool,
}

/// The severity-sorted readiness report for the two selected inputs (CALB-05).
///
/// Non-blocking by design (consent, not prevention). `overlap_estimate` and
/// `exposure_delta_stops` are `None` when the cheap sampled pass could not run —
/// an honest unknown, never a fabricated `0.0`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ReadinessReport {
    /// The findings, sorted blocking-shape first, informational last.
    pub findings: Vec<ReadinessFinding>,
    /// Estimated horizontal overlap fraction (`0.0..=1.0`), or `None` if unknown.
    pub overlap_estimate: Option<f64>,
    /// Estimated exposure difference in stops, or `None` if unknown.
    pub exposure_delta_stops: Option<f64>,
}

/// One lens-profile candidate for the override dropdown (IMPT-04 / D3-07).
///
/// Mirrors `reco_calibrate::types::LensProfileSummary` so the frontend never
/// depends on an engine type directly.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LensCandidate {
    /// Camera brand/model, e.g. "GoPro HERO10".
    pub camera: String,
    /// Lens mode, e.g. "Wide".
    pub lens: String,
    /// Profile calibration width in pixels.
    pub width: u32,
    /// Profile calibration height in pixels.
    pub height: u32,
}

/// Host mirror of the engine's seven calibration steps (CALB-01 / D3-09).
///
/// The wizard's stage checklist is driven directly by these — the UI never
/// derives stage state locally (UI-SPEC Interaction rule 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CalibrationStage {
    /// Probing video metadata.
    Probing,
    /// Detecting lens profiles (telemetry parse + database lookup).
    DetectingProfiles,
    /// Detecting the temporal sync offset.
    AudioSync,
    /// Extracting video frames.
    ExtractingFrames,
    /// Correcting lens distortion.
    Undistorting,
    /// Matching features between cameras.
    FeatureMatching,
    /// Optimizing camera parameters.
    Optimizing,
}

/// Status of one row in the stage checklist (CALB-01).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageStatus {
    /// Not started yet.
    Pending,
    /// Currently running.
    Active,
    /// Finished successfully.
    Done,
    /// Failed.
    Failed,
    /// Skipped.
    Skipped,
}

/// Aggregated per-frame match metrics attached to a failure diagnosis (CALB-04).
///
/// Counters are summed across the frames that produced matches before the
/// failure; `keypoints_left` / `keypoints_right` are the minimum across those
/// frames (the weakest frame bounds the run). `frames_used` is the number of
/// frame pairs that produced matches. Everything is zero when the run failed
/// before any frame matched — an honest zero, never a fabricated number.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DiagnosisMetrics {
    /// Frame pairs that produced matches before the failure.
    pub frames_used: usize,
    /// Matched point pairs summed across those frames.
    pub total_matches: usize,
    /// Matches summed across frames that survived the ratio test.
    pub post_ratio_test: usize,
    /// Matches summed across frames that survived the spatial filter.
    pub post_spatial_filter: usize,
    /// Matches summed across frames that survived RANSAC.
    pub post_ransac: usize,
    /// Minimum keypoints detected in the left image across those frames.
    pub keypoints_left: usize,
    /// Minimum keypoints detected in the right image across those frames.
    pub keypoints_right: usize,
}

/// A plain-language explanation of a calibration failure (CALB-04).
///
/// Authored in Rust on the host (`calibration::diagnose_calibration_failure`)
/// from the typed engine error and stage metrics. The frontend renders `cause`
/// and `fix` verbatim and never invents a cause; `raw_error` and `metrics`
/// populate the collapsed Technical detail (RESEARCH Pattern 1 / D-06).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CalibrationDiagnosis {
    /// Plain-language cause (`What happened`).
    pub cause: String,
    /// Suggested next step (`What to try`).
    pub fix: String,
    /// The raw typed error's `Display` text for the technical disclosure.
    pub raw_error: String,
    /// The pipeline stage that was active when the failure occurred.
    pub stage: CalibrationStage,
    /// Aggregated per-frame match metrics (all zero when none were collected).
    pub metrics: DiagnosisMetrics,
}

/// Which path produced a calibration's temporal offset (CALB-03 / D3-14).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncMethod {
    /// IMU/gyroscope telemetry.
    Imu,
    /// Audio cross-correlation.
    Audio,
    /// Operator-set manual offset.
    Manual,
    /// No sync ran.
    None,
}

/// Confidence band word for the scorecard (CALB-03).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfidenceBand {
    /// Confidence >= 0.8.
    High,
    /// Confidence >= 0.5.
    Medium,
    /// Confidence < 0.5.
    Low,
}

/// The resolved lens profile and its source (CALB-03 / D3-13).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LensProfileView {
    /// Human-readable profile name, e.g. "GoPro HERO10 Wide".
    pub name: String,
    /// Human-readable source, e.g. "database" / "auto-detected".
    pub source: String,
}

/// The fixed offset-semantics wording (CALB-06).
///
/// Authored **once** here and carried on [`SyncView::offset_semantics`] so every
/// surface renders the identical sentence and it is never re-typed in a second
/// component (UI-SPEC Sync Diagnostics Contract). The semantics match the
/// engine's authoritative frame selection (`reco-calibrate/src/pipeline.rs`):
/// a positive offset advances the right stream, skipping right frames; a
/// negative offset advances the left stream, skipping left frames.
pub const SYNC_OFFSET_SEMANTICS: &str =
    "positive offset skips right frames, negative skips left frames";

/// The audio-sync confidence below which an estimate is treated as low
/// (MANU-02).
///
/// Mirrors the scorecard's `Low` band boundary (`calibration::confidence_band`,
/// CALB-03): a confidence below this reads the same as "unavailable" to the
/// operator — the offset defaults to 0 and must be confirmed. Used only to
/// decide the log severity here; the webview applies the same boundary to gate
/// the step.
pub const AUDIO_SYNC_CONFIDENCE_FLOOR: f64 = 0.5;

/// Which path produced a calibration's temporal offset, as a provenance mark
/// over the `IMU → Audio → Manual` chain (CALB-06).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SyncProvenance {
    /// The step in `IMU → Audio → Manual` that produced the offset.
    pub ran: SyncMethod,
    /// Whether the operator set the offset manually (no confidence computed).
    pub is_manual: bool,
}

/// The sync method, its confidence, the signed offset, and the fixed semantics
/// (CALB-03 / CALB-06).
///
/// IMU/manual paths report `confidence: None` — never a fabricated number. The
/// offset is always carried (with its sign) and the semantics string is the
/// single [`SYNC_OFFSET_SEMANTICS`] constant, so no surface re-authors it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SyncView {
    /// Which sync path ran.
    pub method: SyncMethod,
    /// Confidence of the estimate, when the path reports one.
    pub confidence: Option<f64>,
    /// Applied sync offset in frames (signed; 0 when no offset was applied).
    pub offset_frames: i64,
    /// The provenance chain mark (which path ran, and whether it was manual).
    pub provenance: SyncProvenance,
    /// The fixed offset-semantics sentence ([`SYNC_OFFSET_SEMANTICS`]).
    pub offset_semantics: String,
}

/// The CALB-03 scorecard — exactly the locked field set, no invented metrics.
///
/// Projected from a `CalibrationResult` by `calibration::project_scorecard`;
/// every field maps to a real engine value. A missing value is `None`, never a
/// fabricated zero (UI-SPEC Result Scorecard Contract).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Scorecard {
    /// Calibration confidence (0.0-1.0).
    pub confidence: f64,
    /// Confidence band word.
    pub confidence_band: ConfidenceBand,
    /// Residual seam-weighted reprojection error.
    pub residual_error: f64,
    /// Total matched point pairs across all frames.
    pub total_matches: u64,
    /// Average matches per frame used (0 when no frames were used).
    pub per_frame_matches: f64,
    /// Number of frame pairs that produced usable matches.
    pub frames_used: u64,
    /// Resolved lens profile and source, if any.
    pub lens_profile: Option<LensProfileView>,
    /// Sync method and confidence.
    pub sync: SyncView,
}

/// Advanced calibration options exposed by the wizard (D3-12).
///
/// Every field is optional: `None` means "use the engine default". Only the
/// four locked advanced fields are exposed; raw AKAZE/match/optimizer
/// thresholds stay hidden.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CalibrationOptions {
    /// Number of frame pairs to sample.
    pub num_frames: Option<usize>,
    /// Seconds to skip from the start.
    pub skip_start_secs: Option<f64>,
    /// Seconds to skip from the end.
    pub skip_end_secs: Option<f64>,
    /// Whether to seed the optimizer from IMU differential rotation.
    pub use_imu_rotation_seeds: Option<bool>,
}

/// One feature-match point for the debug inspector (CALB-08).
///
/// `x_nx` / `y_nx` are normalized to `0.0..=1.0` on the paired undistorted
/// frame (the left camera's coordinate space), so the overlay canvas scales
/// them by its own width/height and never needs the pixel geometry. `error` is
/// the per-point reprojection residual feeding the residual map's colour ramp
/// (the run's residual when no per-point estimate is available).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DebugPoint {
    /// Normalized x in the paired frame (`0.0..=1.0`).
    pub x_nx: f64,
    /// Normalized y in the paired frame (`0.0..=1.0`).
    pub y_nx: f64,
    /// Reprojection-error proxy for this point (residual map colour).
    pub error: f64,
}

/// One row of the debug inspector's per-frame match-count table (CALB-08).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FrameMatchRow {
    /// 0-based frame index within the run.
    pub frame: u64,
    /// Keypoints detected in the left image.
    pub keypoints_left: usize,
    /// Keypoints detected in the right image.
    pub keypoints_right: usize,
    /// Matches surviving the ratio test.
    pub post_ratio_test: usize,
    /// Matches surviving the spatial overlap filter.
    pub post_spatial_filter: usize,
    /// Matches surviving RANSAC.
    pub post_ransac: usize,
}

/// The bounded debug inspector payload for one sampled frame pair (CALB-08).
///
/// Carries the downscaled undistorted thumbnails (max edge
/// [`crate::calibration::DEBUG_THUMB_MAX_EDGE`]), the verified and rejected
/// match points, the run's residual, and the per-frame count rows. The point
/// lists are capped at [`crate::calibration::DEBUG_POINT_CAP`]; `points_capped`
/// records that truncation so the UI can say so rather than silently dropping
/// points (T-04-10). `left_thumb`/`right_thumb` are empty and the dimensions
/// zero when no frame pair was retained (the canvas fails closed).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DebugReport {
    /// 0-based index of the sampled frame within the run.
    pub frame_index: u64,
    /// Total frames that produced matches in the run.
    pub frames_total: u64,
    /// Downscaled left thumbnail width in pixels (0 when absent).
    pub left_width: u32,
    /// Downscaled left thumbnail height in pixels (0 when absent).
    pub left_height: u32,
    /// Downscaled right thumbnail width in pixels (0 when absent).
    pub right_width: u32,
    /// Downscaled right thumbnail height in pixels (0 when absent).
    pub right_height: u32,
    /// Downscaled left thumbnail RGBA (`left_width * left_height * 4` bytes).
    pub left_thumb: Vec<u8>,
    /// Downscaled right thumbnail RGBA (`right_width * right_height * 4` bytes).
    pub right_thumb: Vec<u8>,
    /// Match points that survived every filter (accent markers).
    pub verified: Vec<DebugPoint>,
    /// Candidate matches the filters rejected (warn markers).
    pub rejected: Vec<DebugPoint>,
    /// Residual seam-weighted reprojection error at the optimum (0 when unknown).
    pub residual_error: f64,
    /// One row per frame that produced matches.
    pub per_frame: Vec<FrameMatchRow>,
    /// Whether the point lists were truncated to the cap.
    pub points_capped: bool,
}

/// The solved plane layout as a transport-agnostic view (MANU-03).
///
/// Mirrors the seven `PlaneLayout` fields so the webview can read the solved
/// rig without importing an engine type (`reco-core`). Produced by the manual
/// solve and carried on [`WorkerEvent::ManualSolveResult`].
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PlaneLayoutView {
    /// Distance of the virtual camera from the origin along both axes.
    pub camera_axis_offset: f64,
    /// Overlap ratio between the two planes (`0.0`..=`1.0`).
    pub intersect: f64,
    /// Y-axis translation of the right plane.
    pub x_ty: f64,
    /// Z-axis rotation of the right plane (radians).
    pub x_rz: f64,
    /// X-axis rotation of the left plane (radians).
    pub z_rx: f64,
    /// X-axis rotation of the right plane (radians).
    pub x_rx: f64,
    /// Z-axis rotation of the left plane (radians).
    pub z_rz: f64,
}

impl PlaneLayoutView {
    /// Project an engine [`PlaneLayout`](reco_core::calibration::PlaneLayout)
    /// into its transport-agnostic view.
    #[must_use]
    pub fn from_layout(layout: &reco_core::calibration::PlaneLayout) -> Self {
        Self {
            camera_axis_offset: layout.camera_axis_offset,
            intersect: layout.intersect,
            x_ty: layout.x_ty,
            x_rz: layout.x_rz,
            z_rx: layout.z_rx,
            x_rx: layout.x_rx,
            z_rz: layout.z_rz,
        }
    }
}

/// One correspondence pin as it crosses to the webview (MANU-03).
///
/// `left_px`/`right_px` are natural-order pixel coordinates (the operator's
/// click points), never optimizer-plane coordinates — the swap lives only in
/// the engine helper (prohibition: never re-derive the swap in the UI).
/// `verified` is true for a pin seeded from a post-RANSAC automatic match and
/// false for one the operator placed (MANU-04).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ManualPinView {
    /// Stable per-session pin id (used by move/remove commands).
    pub id: u32,
    /// Clicked point on the left frame, `[x, y]` pixels.
    pub left_px: [f64; 2],
    /// Corresponding point on the right frame, `[x, y]` pixels.
    pub right_px: [f64; 2],
    /// Whether the pin was seeded from a verified automatic match.
    pub verified: bool,
}

/// An event emitted by the engine worker and rendered in the webview log pane.
///
/// `Clone + Send + 'static` so it can cross the worker→UI channel; the
/// compile-time assertion at the bottom of this file enforces that bound.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
#[non_exhaustive]
pub enum WorkerEvent {
    /// A human-readable line for the event log at the given [`Level`].
    ///
    /// Used for the ordinary progress narrative ("import started", "preview
    /// running") the UI-SPEC Event Log Contract specifies.
    Log {
        /// Severity of the line (drives the log colour).
        level: Level,
        /// Message text, already user-facing.
        message: String,
    },

    /// A command was rejected or the engine failed.
    ///
    /// Carries the typed [`WorkerError`] rather than a string; the webview
    /// renders `error.to_string()`, preserving the typed value for any other
    /// consumer.
    Failed(WorkerError),

    /// The worker's authoritative playback position changed (PREV-02).
    ///
    /// Emitted after each advanced frame and on every completed seek. The UI
    /// mirrors this; it never owns the position (UI-SPEC Interaction rule 1).
    Position {
        /// Current frame index (0-based).
        frame: u64,
        /// Total frames in the source, if known (drives the timeline extent).
        total: Option<u64>,
        /// Exact frame-rate rational, if the source reported one.
        fps_rational: Option<(i32, i32)>,
    },

    /// The worker's authoritative transport state changed (PREV-02).
    ///
    /// Emitted on play / pause / loop toggle / end-of-source.
    Transport {
        /// Playing / paused / ended.
        state: crate::transport::TransportState,
        /// Whether full-clip looping is enabled.
        loop_enabled: bool,
    },

    /// The worker's authoritative pose after a session tick (PREV-04).
    ///
    /// Emitted once per tick with the *current* (eased) pose the renderer used,
    /// so the UI's pan/zoom readout mirrors the render rather than the target.
    Pose {
        /// Current yaw in radians.
        yaw: f32,
        /// Current pitch in radians.
        pitch: f32,
        /// Current vertical FOV in degrees.
        fov_degrees: f32,
        /// The widest FOV the stitchable coverage actually allows, in degrees.
        ///
        /// The pose gets clamped to this every tick, so a UI that offers a
        /// wider range silently does nothing above it. On the shipped test clip
        /// this is 50.87 against a configured max of 150 — most of that slider
        /// was inert with no indication why. Reported so the control can be
        /// bounded to what is achievable rather than advertising dead range.
        fov_max: f32,
    },

    /// The active presenter changed (PREV-05).
    ///
    /// Carries only the [`PresenterKind`](crate::presenter::PresenterKind) and
    /// an owned reason string — never a window/surface handle (T-02-09). The
    /// badge renders `kind`; a `Some(reason)` is present when the activation was
    /// a fallback (the matching WARN line carries the remediation).
    Presenter {
        /// Which presenter in the chain is now active.
        kind: crate::presenter::PresenterKind,
        /// Why the active presenter was chosen, when it was a fallback.
        reason: Option<String>,
    },

    /// The preview view mode changed (PREV-03).
    ///
    /// Carries only the [`ViewMode`](crate::presenter::ViewMode) — never a
    /// surface or texture handle. Emitted only on an actual change; requesting
    /// the already-active mode is a no-op that emits no event.
    View {
        /// The new view mode.
        mode: crate::presenter::ViewMode,
    },

    /// The probed metadata for one imported input (IMPT-01 / IMPT-02).
    ///
    /// Structured — the frontend reads this typed payload and never regex-parses
    /// log strings (FRICTION A3/A12). The matching INFO line is a human-readable
    /// summary only; it is not the data channel.
    ImportMetadata {
        /// Which input this metadata describes.
        role: InputRole,
        /// The per-field metadata with provenance.
        metadata: InputMetadata,
    },

    /// The severity-sorted readiness report for the two selected inputs (CALB-05).
    ///
    /// Emitted after every input/lens change; an empty `findings` list means
    /// every check passed. Non-blocking by design (D3-05).
    Readiness {
        /// The severity-sorted report (findings + honest optional estimates).
        report: ReadinessReport,
    },

    /// Lens-profile candidates for one input's override dropdown (IMPT-04).
    LensCandidates {
        /// Which input the candidates are for.
        role: InputRole,
        /// The plausible profiles for that input's resolution.
        candidates: Vec<LensCandidate>,
    },

    /// The lens override now applied to one input (IMPT-04 / D3-08).
    ///
    /// `candidate: None` means the override was cleared back to auto-detect.
    LensOverrideApplied {
        /// Which input the override applies to.
        role: InputRole,
        /// The applied override, or `None` when cleared.
        candidate: Option<LensCandidate>,
    },

    /// One row of the stage checklist changed state (CALB-01).
    CalibrationStage {
        /// Which stage changed.
        step: CalibrationStage,
        /// Its new status.
        status: StageStatus,
        /// Human-readable detail for the row.
        detail: String,
    },

    /// Overall calibration progress, 0.0-1.0 (CALB-01).
    CalibrationProgress {
        /// Fraction complete.
        fraction: f64,
    },

    /// A heartbeat tick so a silent stage never looks stuck (CALB-01 / D3-10).
    ///
    /// Emitted on a monitor thread every ~500 ms, even while the engine is
    /// silent inside a stage.
    CalibrationHeartbeat {
        /// Milliseconds since calibration started.
        elapsed_ms: u64,
        /// The stage that was active when the tick fired.
        step: CalibrationStage,
        /// The last detail string the engine reported.
        last_detail: String,
    },

    /// The completed calibration's scorecard (CALB-03).
    CalibrationResult {
        /// The projected CALB-03 field set.
        scorecard: Scorecard,
    },

    /// A calibration run failed with a plain-language diagnosis (CALB-04).
    ///
    /// Carries the typed [`CalibrationDiagnosis`] so the Calibrate screen can
    /// render a cause/fix panel plus a collapsed technical detail — never a bare
    /// error code. Also projected to an ERROR [`LogLine`] whose message is the
    /// diagnosis cause.
    CalibrationFailed {
        /// The plain-language cause/fix plus the raw error and metrics.
        diagnosis: CalibrationDiagnosis,
    },

    /// The bounded debug inspector payload after a calibration run (CALB-08).
    ///
    /// Emitted once per completed run (pass or fail) when any frame produced
    /// matches, so the Calibrate screen can render the feature-match overlay,
    /// the residual map, and the per-frame count table. The payload is bounded
    /// (downscaled thumbnails, capped points) — never a CLI PNG export.
    CalibrationDebug {
        /// The sampled frame pair's points, thumbnails, and per-frame counts.
        report: DebugReport,
    },

    /// The per-camera field ROI polygon was written onto the calibration (CALB-09).
    ///
    /// Carries the `FieldRoi` actually stored (a camera polygon with fewer than
    /// three vertices is normalized to empty, and both empty clears the whole
    /// value — see [`WorkerEvent::FieldRoiCleared`]). The frontend mirrors this
    /// so the editor reflects what the worker persisted, never an optimistic
    /// local value. Projected to an INFO [`LogLine`].
    FieldRoiApplied {
        /// The polygon pair now stored on the calibration's `field_roi`.
        field_roi: reco_core::calibration::FieldRoi,
    },

    /// The per-camera field ROI polygon was cleared (CALB-09).
    ///
    /// Emitted when a command's polygons were all degenerate (<3 vertices per
    /// camera), so the calibration's `field_roi` was set back to `None`. The
    /// autocam path treats an absent ROI as "no filter".
    FieldRoiCleared,

    /// A profile was loaded from disk (IMPT-05).
    ProfileLoaded {
        /// The path that was loaded.
        path: String,
    },

    /// A profile was saved to disk (IMPT-06).
    ProfileSaved {
        /// The path that was written.
        path: String,
    },

    /// A manual calibration session was opened (MANU-01 / MANU-03).
    ///
    /// Carries the reference frame index, the source frame rate, and the total
    /// frame count so the frontend's scrubber is bounded to the real clip.
    /// Emitted once per `manual_begin`.
    ManualSessionStarted {
        /// The chosen reference frame index (0-based, clamped to the clip).
        frame: u64,
        /// The source frame rate in frames per second.
        fps: f64,
        /// Total frames in the reference clip.
        frames_total: u64,
    },

    /// One camera's reference frame rendered under its real `CameraParams`
    /// (MANU-03).
    ///
    /// `rgba` is `width * height * 4` bytes — the same RGBA-to-webview payload
    /// convention as the CALB-08 debug thumbnails. Every pixel is produced by
    /// the GPU undistort under real `CameraParams`; the frontend paints the
    /// bytes and never touches the GPU (T-04.1-04).
    ManualPreviewFrame {
        /// Which camera this frame belongs to.
        side: ManualSide,
        /// RGBA bytes (`width * height * 4`).
        rgba: Vec<u8>,
        /// Frame width in pixels.
        width: u32,
        /// Frame height in pixels.
        height: u32,
    },

    /// The manual solve's busy/stale state (MANU-03).
    ///
    /// `busy` is true while a background solve is in flight; `stale` marks that
    /// the preview currently shows the last solved result rather than a fresh
    /// one. No per-drag re-solve in v1 (UI-SPEC stale-vs-fresh contract).
    ManualSolveState {
        /// Whether a background solve is running.
        busy: bool,
        /// Whether the preview shows the last solved (stale) result.
        stale: bool,
    },

    /// The manual session's current correspondence pins (MANU-03).
    ///
    /// Emitted immediately on every pin mutation and on `manual_begin` when the
    /// session starts with seeded pins. `seeded` is true on the first emit when
    /// the editor was pre-populated from verified automatic matches (MANU-04),
    /// so the surface can show the pre-populated notice.
    ManualPins {
        /// The pins in stable creation order.
        pins: Vec<ManualPinView>,
        /// Whether these pins were seeded from verified automatic matches.
        seeded: bool,
    },

    /// A debounced background manual solve landed (MANU-03).
    ///
    /// Carries the solved [`PlaneLayoutView`] plus the pin/auto contribution
    /// counts. The preview parameters are only marked fresh after this event;
    /// between a pin drop and this landing the state is
    /// [`ManualSolveState`]` { busy: true, stale: true }`.
    ManualSolveResult {
        /// The solved plane layout.
        layout: PlaneLayoutView,
        /// Residual seam-weighted reprojection error at the optimum.
        residual: f64,
        /// Number of manual pins that contributed to the solve.
        pins_used: usize,
        /// Number of pre-populated automatic matches that contributed.
        auto_used: usize,
    },

    /// The manual flow's audio auto-sync estimate (MANU-02).
    ///
    /// Emitted on entry to the Time-align step and whenever the operator asks
    /// for a re-detect. `confidence: None` is the "unavailable" signal (the
    /// engine could not extract audio or correlate it) — never a fabricated
    /// number. `offset_semantics` carries the single [`SYNC_OFFSET_SEMANTICS`]
    /// constant so the frontend binds the wording rather than re-typing it
    /// (T-04.1-07).
    AudioSyncResult {
        /// Rounded offset in frames (0 when the estimate is unavailable).
        offset_frames: i64,
        /// Cross-correlation confidence, or `None` when unavailable.
        confidence: Option<f64>,
        /// The fixed offset-semantics sentence ([`SYNC_OFFSET_SEMANTICS`]).
        offset_semantics: String,
    },

    /// The operator set the manual sync offset (MANU-02).
    ///
    /// Records [`SyncMethod::Manual`] provenance, preserving the CALB-06 chain
    /// (IMU → audio → manual). The offset is carried on the manual session and
    /// is what the later pin/bend solves use.
    ManualSyncSet {
        /// The offset in frames the operator chose (signed).
        offset_frames: i64,
        /// The provenance mark — always [`SyncMethod::Manual`] on this path.
        method: SyncMethod,
        /// The fixed offset-semantics sentence ([`SYNC_OFFSET_SEMANTICS`]).
        offset_semantics: String,
    },

    /// The current result no longer matches the inputs or profile (D3-08).
    ///
    /// Emitted when an input or lens override changes after a run/load, so the
    /// scorecard never claims to reflect a profile it did not use.
    ResultInvalidated,
}

/// The UI-facing shape of an event-log line (UI-SPEC Event Log Contract).
///
/// The webview's `listen("worker-event", ...)` handler consumes this shape and
/// renders `[HH:MM:SS] LEVEL  message`. Keeping the projection explicit (rather
/// than letting the frontend reach into the internally-tagged `WorkerEvent`)
/// means the JS side never invents text or branches on a Rust enum: it reads
/// `level` for the colour and `message` for the body.
///
/// Produced by [`WorkerEvent::to_log_line`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LogLine {
    /// Severity, driving the level colour (`info` / `warn` / `error`).
    pub level: Level,
    /// The message body — already user-facing text.
    pub message: String,
}

impl WorkerEvent {
    /// Project this event into the [`LogLine`] the webview renders.
    ///
    /// * [`WorkerEvent::Log`] carries its own level and message.
    /// * [`WorkerEvent::Failed`] is an ERROR line whose message is the typed
    ///   error's `Display` text — the frontend never invents error copy
    ///   (UI-SPEC Error state); it wraps the worker's typed message.
    pub fn to_log_line(&self) -> LogLine {
        match self {
            WorkerEvent::Log { level, message } => LogLine {
                level: *level,
                message: message.clone(),
            },
            WorkerEvent::Failed(error) => LogLine {
                level: Level::Error,
                message: error.to_string(),
            },
            // The state-carrying variants are not log lines; project them to a
            // concise INFO summary so a log-only consumer (or a headless gate
            // report) still sees them without inventing text on the JS side.
            WorkerEvent::Position {
                frame,
                total,
                fps_rational,
            } => LogLine {
                level: Level::Info,
                // The rational rides along so the frontend can compute an exact
                // timecode. Without it the UI falls back to a nominal 30 fps and
                // drifts on non-30 material (29.97 is ~3.6 s per hour).
                message: match (total, fps_rational) {
                    (Some(total), Some((num, den))) => {
                        format!("position: frame {frame}/{total} @ {num}/{den}")
                    }
                    (Some(total), None) => format!("position: frame {frame}/{total}"),
                    (None, Some((num, den))) => {
                        format!("position: frame {frame} @ {num}/{den}")
                    }
                    (None, None) => format!("position: frame {frame}"),
                },
            },
            WorkerEvent::Transport {
                state,
                loop_enabled,
            } => LogLine {
                level: Level::Info,
                message: format!(
                    "transport: {state:?}, loop {}",
                    if *loop_enabled { "on" } else { "off" }
                ),
            },
            WorkerEvent::Pose {
                yaw,
                pitch,
                fov_degrees,
                fov_max,
            } => LogLine {
                level: Level::Info,
                // `max` is APPENDED, not interleaved: `parsePose` in the webview
                // matches this string, so keeping the original prefix intact and
                // making the new group optional means an older consumer still
                // parses the three values it knows rather than failing outright.
                message: format!(
                    "pose: yaw {:.3}, pitch {:.3}, fov {:.1}, max {:.1}",
                    yaw, pitch, fov_degrees, fov_max
                ),
            },
            WorkerEvent::Presenter { kind, reason } => LogLine {
                level: if reason.is_some() {
                    Level::Warn
                } else {
                    Level::Info
                },
                message: match reason {
                    Some(reason) => crate::presenter::fallback_warn_line(*kind, reason),
                    None => format!("presenter: {}", kind.name()),
                },
            },
            WorkerEvent::View { mode } => LogLine {
                level: Level::Info,
                message: format!(
                    "view switched to {}",
                    match mode {
                        crate::presenter::ViewMode::Source => "source",
                        crate::presenter::ViewMode::Panorama => "panorama",
                    }
                ),
            },
            WorkerEvent::ImportMetadata { role, metadata } => LogLine {
                level: Level::Info,
                // Human-readable summary only; the structured payload travels
                // under `worker-event-typed` (see main.rs).
                message: format!(
                    "metadata: {} {}",
                    role.name(),
                    metadata.resolution.value.as_deref().unwrap_or("—"),
                ),
            },
            // Readiness is a WARN narrative when a run is likely doomed (a
            // blocking-shape finding) and INFO otherwise (UI-SPEC Event Log
            // Contract); the structured report rides the typed channel.
            WorkerEvent::Readiness { report } => LogLine {
                level: if report
                    .findings
                    .iter()
                    .any(|f| f.severity == ReadinessSeverity::BlockingShape)
                {
                    Level::Warn
                } else {
                    Level::Info
                },
                message: match report.findings.first() {
                    Some(first) => format!(
                        "readiness: {} issue(s) found — {}",
                        report.findings.len(),
                        first.message
                    ),
                    None => "readiness: all checks passed".to_string(),
                },
            },
            WorkerEvent::LensCandidates { role, candidates } => LogLine {
                level: Level::Info,
                message: format!("lens candidates: {} for {}", candidates.len(), role.name()),
            },
            WorkerEvent::LensOverrideApplied { role, candidate } => LogLine {
                level: Level::Info,
                message: match candidate {
                    Some(c) => {
                        format!("lens override: {} -> {} {}", role.name(), c.camera, c.lens)
                    }
                    None => format!("lens override cleared: {}", role.name()),
                },
            },
            WorkerEvent::CalibrationStage {
                step,
                status,
                detail,
            } => LogLine {
                level: Level::Info,
                message: format!("calibration stage: {step:?} {status:?} — {detail}"),
            },
            WorkerEvent::CalibrationProgress { fraction } => LogLine {
                level: Level::Info,
                message: format!("calibration progress: {:.0}%", fraction * 100.0),
            },
            WorkerEvent::CalibrationHeartbeat {
                elapsed_ms,
                step,
                last_detail,
            } => LogLine {
                level: Level::Info,
                message: format!(
                    "calibration heartbeat: {elapsed_ms} ms in {step:?} — {last_detail}"
                ),
            },
            WorkerEvent::CalibrationResult { scorecard } => LogLine {
                level: Level::Info,
                message: format!(
                    "calibration result: confidence {:.0}%",
                    scorecard.confidence * 100.0
                ),
            },
            WorkerEvent::CalibrationFailed { diagnosis } => LogLine {
                level: Level::Error,
                // The cause is already user-facing (authored in Rust on the
                // diagnosis) — the frontend renders it verbatim (CALB-04).
                message: diagnosis.cause.clone(),
            },
            // Debug data published is an INFO line (UI-SPEC Event Log Contract);
            // the structured report rides the typed channel.
            WorkerEvent::CalibrationDebug { report } => LogLine {
                level: Level::Info,
                message: format!(
                    "debug data published: frame {} of {}, {} verified / {} rejected matches",
                    report.frame_index + 1,
                    report.frames_total,
                    report.verified.len(),
                    report.rejected.len(),
                ),
            },
            // The field ROI is applied/cleared at INFO (UI-SPEC Event Log
            // Contract); the polygon rides the typed channel.
            WorkerEvent::FieldRoiApplied { field_roi } => LogLine {
                level: Level::Info,
                message: format!(
                    "field ROI applied: {} left / {} right vertices",
                    field_roi.left.len(),
                    field_roi.right.len()
                ),
            },
            WorkerEvent::FieldRoiCleared => LogLine {
                level: Level::Info,
                message: "field ROI cleared".to_string(),
            },
            WorkerEvent::ProfileLoaded { path } => LogLine {
                level: Level::Info,
                message: format!("profile loaded: {path}"),
            },
            WorkerEvent::ProfileSaved { path } => LogLine {
                level: Level::Info,
                message: format!("profile saved: {path}"),
            },
            WorkerEvent::ManualSessionStarted {
                frame,
                fps,
                frames_total,
            } => LogLine {
                level: Level::Info,
                message: format!(
                    "manual session started: frame {frame}/{frames_total} @ {fps:.3} fps"
                ),
            },
            // The RGBA payload is deliberately NOT logged: a log line carries a
            // human-readable summary, never a megabyte of frame bytes.
            WorkerEvent::ManualPreviewFrame {
                side,
                width,
                height,
                ..
            } => LogLine {
                level: Level::Info,
                message: format!("manual preview frame: {side:?} {width}×{height}"),
            },
            WorkerEvent::ManualSolveState { busy, stale } => LogLine {
                level: Level::Info,
                message: format!(
                    "manual solve: {}",
                    if *busy {
                        "solving…"
                    } else if *stale {
                        "preview shows last solved result"
                    } else {
                        "solved"
                    }
                ),
            },
            // The pin list crosses as a structured payload; the log line is a
            // bounded summary (never the pixel coordinates of every pin).
            WorkerEvent::ManualPins { pins, seeded } => LogLine {
                level: Level::Info,
                message: if *seeded {
                    format!(
                        "manual pins: {} seeded from verified automatic matches",
                        pins.len()
                    )
                } else {
                    format!("manual pins: {}", pins.len())
                },
            },
            // A landed solve is INFO. The degenerate/insufficient case never
            // produces this event — the worker emits a WARN log line and a stale
            // solve state instead, so no garbage rig is ever presented.
            WorkerEvent::ManualSolveResult {
                residual,
                pins_used,
                auto_used,
                ..
            } => LogLine {
                level: Level::Info,
                message: format!(
                    "manual solve finished: residual {residual:.6} ({pins_used} pins, {auto_used} auto)"
                ),
            },
            // Audio sync is INFO when a confident estimate exists and WARN when
            // it is low or absent (UI-SPEC Event Log Contract). The offset is
            // rendered WITH its fixed semantics sentence so a log reader cannot
            // mistake the sign's meaning (T-04.1-07).
            WorkerEvent::AudioSyncResult {
                offset_frames,
                confidence,
                offset_semantics,
            } => LogLine {
                level: if confidence.is_some_and(|c| c >= AUDIO_SYNC_CONFIDENCE_FLOOR) {
                    Level::Info
                } else {
                    Level::Warn
                },
                message: match confidence {
                    Some(confidence) => format!(
                        "audio sync: offset {offset_frames:+} frames (confidence {:.0}%) — {offset_semantics}",
                        confidence * 100.0,
                    ),
                    None => format!(
                        "audio sync unavailable or low confidence — set the offset manually ({offset_semantics})"
                    ),
                },
            },
            WorkerEvent::ManualSyncSet {
                offset_frames,
                method,
                offset_semantics,
            } => LogLine {
                level: Level::Info,
                message: format!(
                    "manual sync: offset {offset_frames:+} frames ({method:?}) — {offset_semantics}"
                ),
            },
            WorkerEvent::ResultInvalidated => LogLine {
                level: Level::Warn,
                message: "result invalidated — inputs changed; re-run calibration".to_string(),
            },
        }
    }
}

/// A typed worker failure that crosses the command/event channel.
///
/// `Clone + Send + Sync` (enforced by the assertion below) because it is moved
/// through [`std::sync::mpsc`](std::sync::mpsc) and may be read by more than
/// one consumer. It never carries a raw OS or GPU handle — only owned data.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, thiserror::Error)]
pub enum WorkerError {
    /// A command required an imported session but none exists yet.
    ///
    /// Phase 2 imports at startup and its chrome has no Import button, so the
    /// message must not tell the user to press one: the actionable cause is a
    /// failed startup import (see the worker log for the engine error).
    #[error("no clips are loaded yet — the startup import did not complete (see the log)")]
    NotImported,

    /// The command is not implemented in this phase.
    ///
    /// Plan 05 implements `Shutdown`-adjacent device-loss simulation; until
    /// then the worker rejects it with this variant.
    #[error("operation not supported yet: {operation}")]
    Unsupported {
        /// Which operation was rejected.
        operation: String,
    },

    /// A command argument failed boundary validation (e.g. an empty input path).
    ///
    /// Rejected before any engine work (T-03-01); the UI renders the typed
    /// `field` / `reason` as text, never a bare code.
    #[error("invalid {field}: {reason}")]
    InvalidInput {
        /// Which argument was rejected.
        field: String,
        /// Why it was rejected.
        reason: String,
    },

    /// The worker was already shutting down and refuses new work.
    #[error("the engine worker is shutting down")]
    ShuttingDown,

    /// Loading a calibration profile failed (IMPT-05).
    ///
    /// The inner message is the typed load error's `Display` text (size cap,
    /// parse, or validation failure) — never a fabricated cause.
    #[error("cannot load profile: {0}")]
    ProfileLoad(String),

    /// Saving a calibration profile failed (IMPT-06).
    #[error("cannot save profile: {0}")]
    ProfileSave(String),

    /// The worker thread's command channel is closed (the worker has exited).
    #[error("the engine worker is no longer running")]
    ChannelClosed,

    /// The worker thread did not stop within the shutdown timeout.
    #[error("the engine worker did not stop within {timeout_ms} ms")]
    ShutdownTimeout {
        /// The timeout that elapsed, in milliseconds.
        timeout_ms: u64,
    },

    /// An engine operation failed; the inner message is the rendered cause.
    ///
    /// Engine errors are flattened to a string **at this boundary** because
    /// several of them (e.g. `wgpu` request errors) are not `Clone`. The typed
    /// outer variant is what crosses the channel (CONVENTIONS.md:135).
    #[error("{0}")]
    Engine(String),
}

// Compile-time bound check: both halves of the protocol are `Clone + Send +
// 'static` so a worker thread can move them through an mpsc channel (the same
// idiom as `crates/reco-control/src/lib.rs:211-218`). The `WorkerCommand` half
// is declared in `commands.rs`; it is asserted here alongside `WorkerEvent` so
// the whole protocol's thread-safety is checked in one place.
const _: fn() = || {
    fn assert_clone_send<T: Clone + Send + 'static>() {}
    assert_clone_send::<WorkerEvent>();
    assert_clone_send::<WorkerError>();
    assert_clone_send::<crate::commands::WorkerCommand>();
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_event_roundtrips_through_serde() {
        let event = WorkerEvent::Log {
            level: Level::Info,
            message: "import started".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"kind\":\"log\""), "unexpected json: {json}");
        assert!(
            json.contains("\"level\":\"info\""),
            "unexpected json: {json}"
        );
        let back: WorkerEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, back);
    }

    #[test]
    fn warn_level_serializes_snake_case() {
        let event = WorkerEvent::Log {
            level: Level::Warn,
            message: "software encoder fallback".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(
            json.contains("\"level\":\"warn\""),
            "unexpected json: {json}"
        );
    }

    #[test]
    fn failed_event_roundtrips_typed_error() {
        let event = WorkerEvent::Failed(WorkerError::NotImported);
        let json = serde_json::to_string(&event).unwrap();
        assert!(
            json.contains("\"kind\":\"failed\""),
            "unexpected json: {json}"
        );
        let back: WorkerEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, back);
    }

    #[test]
    fn failed_event_carries_a_typed_error_not_a_string() {
        // The UI renders `to_string()`, but the value stays typed so a
        // consumer can branch on the variant.
        let err = WorkerError::Unsupported {
            operation: "simulate_device_loss".to_string(),
        };
        assert!(err.to_string().contains("simulate_device_loss"));
        let event = WorkerEvent::Failed(err.clone());
        match event {
            WorkerEvent::Failed(error) => {
                assert_eq!(
                    error,
                    WorkerError::Unsupported {
                        operation: "simulate_device_loss".to_string()
                    }
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn log_event_projects_to_a_log_line_verbatim() {
        let event = WorkerEvent::Log {
            level: Level::Warn,
            message: "software encoder fallback".to_string(),
        };
        assert_eq!(
            event.to_log_line(),
            LogLine {
                level: Level::Warn,
                message: "software encoder fallback".to_string(),
            }
        );
    }

    #[test]
    fn failed_event_projects_to_an_error_line_with_the_typed_message() {
        // The message the webview renders comes from the typed error, never
        // from JS-side string invention (UI-SPEC Error state).
        let event = WorkerEvent::Failed(WorkerError::NotImported);
        let line = event.to_log_line();
        assert_eq!(line.level, Level::Error);
        assert_eq!(
            line.message,
            "no clips are loaded yet — the startup import did not complete (see the log)"
        );
    }

    #[test]
    fn log_line_serializes_with_snake_case_level() {
        let line = LogLine {
            level: Level::Info,
            message: "import started".to_string(),
        };
        let json = serde_json::to_string(&line).unwrap();
        assert!(
            json.contains("\"level\":\"info\""),
            "unexpected json: {json}"
        );
        assert!(
            json.contains("\"message\":\"import started\""),
            "unexpected json: {json}"
        );
        let back: LogLine = serde_json::from_str(&json).unwrap();
        assert_eq!(line, back);
    }

    #[test]
    fn position_event_roundtrips_through_serde() {
        let event = WorkerEvent::Position {
            frame: 42,
            total: Some(300),
            fps_rational: Some((30000, 1001)),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(
            json.contains("\"kind\":\"position\""),
            "unexpected json: {json}"
        );
        let back: WorkerEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, back);
    }

    #[test]
    fn transport_event_roundtrips_with_snake_case_state() {
        let event = WorkerEvent::Transport {
            state: crate::transport::TransportState::Playing,
            loop_enabled: true,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(
            json.contains("\"kind\":\"transport\""),
            "unexpected json: {json}"
        );
        assert!(
            json.contains("\"state\":\"playing\""),
            "unexpected json: {json}"
        );
        let back: WorkerEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, back);
    }

    #[test]
    fn presenter_event_roundtrips_and_projects_to_a_warn_line_on_fallback() {
        let event = WorkerEvent::Presenter {
            kind: crate::presenter::PresenterKind::Readback,
            reason: Some("native unavailable".to_string()),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(
            json.contains("\"kind\":\"presenter\""),
            "unexpected json: {json}"
        );
        assert!(
            json.contains("\"kind\":\"readback\""),
            "unexpected json: {json}"
        );
        let back: WorkerEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, back);

        // A fallback projects to exactly one WARN line carrying reason +
        // remediation.
        let line = event.to_log_line();
        assert_eq!(line.level, Level::Warn);
        assert!(line.message.starts_with("Presenter fallback to Readback:"));
        assert!(line.message.contains("throttled"));

        // A successful selection projects to an INFO line.
        let active = WorkerEvent::Presenter {
            kind: crate::presenter::PresenterKind::Native,
            reason: None,
        };
        assert_eq!(active.to_log_line().level, Level::Info);
        assert_eq!(active.to_log_line().message, "presenter: Native");
    }

    #[test]
    fn view_event_roundtrips_through_serde() {
        let event = WorkerEvent::View {
            mode: crate::presenter::ViewMode::Source,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(
            json.contains("\"kind\":\"view\""),
            "unexpected json: {json}"
        );
        assert!(
            json.contains("\"mode\":\"source\""),
            "unexpected json: {json}"
        );
        let back: WorkerEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, back);
    }

    #[test]
    fn view_event_projects_to_an_info_log_line() {
        let source = WorkerEvent::View {
            mode: crate::presenter::ViewMode::Source,
        };
        assert_eq!(source.to_log_line().level, Level::Info);
        assert_eq!(source.to_log_line().message, "view switched to source");

        let panorama = WorkerEvent::View {
            mode: crate::presenter::ViewMode::Panorama,
        };
        assert_eq!(panorama.to_log_line().message, "view switched to panorama");
    }

    #[test]
    fn pose_event_roundtrips_through_serde() {
        let event = WorkerEvent::Pose {
            yaw: 0.25,
            pitch: -0.1,
            fov_degrees: 75.0,
            fov_max: 50.9,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(
            json.contains("\"kind\":\"pose\""),
            "unexpected json: {json}"
        );
        let back: WorkerEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, back);
    }

    #[test]
    fn state_events_project_to_neutral_log_lines() {
        let pos = WorkerEvent::Position {
            frame: 5,
            total: Some(100),
            fps_rational: None,
        };
        assert_eq!(pos.to_log_line().level, Level::Info);
        assert_eq!(pos.to_log_line().message, "position: frame 5/100");

        // With a rational: the frontend needs it to compute an exact timecode
        // instead of assuming a nominal 30 fps.
        let pos_rational = WorkerEvent::Position {
            frame: 5,
            total: Some(100),
            fps_rational: Some((30000, 1001)),
        };
        assert_eq!(
            pos_rational.to_log_line().message,
            "position: frame 5/100 @ 30000/1001"
        );

        // Rational without a total must still carry it.
        let pos_rational_no_total = WorkerEvent::Position {
            frame: 5,
            total: None,
            fps_rational: Some((25, 1)),
        };
        assert_eq!(
            pos_rational_no_total.to_log_line().message,
            "position: frame 5 @ 25/1"
        );

        let tr = WorkerEvent::Transport {
            state: crate::transport::TransportState::Paused,
            loop_enabled: false,
        };
        assert_eq!(tr.to_log_line().level, Level::Info);
        assert!(tr.to_log_line().message.contains("transport"));

        let pose = WorkerEvent::Pose {
            yaw: 0.0,
            pitch: 0.0,
            fov_degrees: 75.0,
            fov_max: 50.9,
        };
        assert_eq!(pose.to_log_line().level, Level::Info);
        let line = pose.to_log_line().message;
        assert!(
            line.contains("fov 75.0"),
            "the current FOV must still be reported: {line}"
        );
        // The ceiling is APPENDED so an older parser that anchors after `fov`
        // keeps working; this asserts the suffix is present and parseable, since
        // the whole point of the field is that the UI can bound the control to
        // what the clip allows.
        assert!(
            line.contains("max 50.9"),
            "the coverage ceiling must be appended to the pose line: {line}"
        );
    }

    #[test]
    fn import_metadata_roundtrips_with_snake_case_provenance() {
        let event = WorkerEvent::ImportMetadata {
            role: InputRole::Left,
            metadata: InputMetadata {
                resolution: MetadataField::probed("1920×1080"),
                fps: MetadataField::probed("30 fps"),
                duration: MetadataField::estimated("0:02"),
                codec: MetadataField::missing(Provenance::Probed),
            },
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(
            json.contains("\"kind\":\"import_metadata\""),
            "unexpected json: {json}"
        );
        assert!(
            json.contains("\"role\":\"left\""),
            "unexpected json: {json}"
        );
        assert!(
            json.contains("\"provenance\":\"estimated\""),
            "unexpected json: {json}"
        );
        assert!(
            json.contains("\"codec\":{\"value\":null"),
            "a missing field must serialize as value:null, not a fabricated zero: {json}"
        );
        let back: WorkerEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, back);
    }

    #[test]
    fn import_metadata_projects_to_an_info_metadata_line() {
        let event = WorkerEvent::ImportMetadata {
            role: InputRole::Right,
            metadata: InputMetadata {
                resolution: MetadataField::probed("1920×1080"),
                fps: MetadataField::probed("30 fps"),
                duration: MetadataField::estimated("0:02"),
                codec: MetadataField::missing(Provenance::Probed),
            },
        };
        let line = event.to_log_line();
        assert_eq!(line.level, Level::Info);
        assert_eq!(line.message, "metadata: right 1920×1080");
    }

    #[test]
    fn missing_metadata_field_renders_an_em_dash_never_zero() {
        let event = WorkerEvent::ImportMetadata {
            role: InputRole::Left,
            metadata: InputMetadata {
                resolution: MetadataField::missing(Provenance::Probed),
                fps: MetadataField::missing(Provenance::Probed),
                duration: MetadataField::missing(Provenance::Estimated),
                codec: MetadataField::missing(Provenance::Probed),
            },
        };
        assert!(
            event.to_log_line().message.contains('—'),
            "an unknown value must render as an em-dash, never 0"
        );
        assert!(!event.to_log_line().message.contains('0'));
    }

    #[test]
    fn scorecard_roundtrips_through_serde() {
        let scorecard = Scorecard {
            confidence: 0.87,
            confidence_band: ConfidenceBand::High,
            residual_error: 0.42,
            total_matches: 1234,
            per_frame_matches: 411.33,
            frames_used: 3,
            lens_profile: Some(LensProfileView {
                name: "GoPro HERO10 Wide".to_string(),
                source: "database".to_string(),
            }),
            sync: SyncView {
                method: SyncMethod::Imu,
                confidence: None,
                offset_frames: 12,
                provenance: SyncProvenance {
                    ran: SyncMethod::Imu,
                    is_manual: false,
                },
                offset_semantics: SYNC_OFFSET_SEMANTICS.to_string(),
            },
        };
        let json = serde_json::to_string(&scorecard).unwrap();
        assert!(
            json.contains("\"confidence_band\":\"high\""),
            "unexpected json: {json}"
        );
        assert!(
            json.contains("\"method\":\"imu\""),
            "unexpected json: {json}"
        );
        assert!(
            json.contains("\"offset_frames\":12"),
            "the signed offset must ride the payload: {json}"
        );
        assert!(
            json.contains("\"offset_semantics\":\"positive offset skips right frames"),
            "the fixed semantics must ride the payload: {json}"
        );
        assert!(
            json.contains("\"confidence\":null"),
            "an absent sync confidence must serialize as null, not a fabricated number: {json}"
        );
        let back: Scorecard = serde_json::from_str(&json).unwrap();
        assert_eq!(scorecard, back);
    }

    #[test]
    fn readiness_report_roundtrips_and_keeps_unknown_estimates_null() {
        // CALB-05: the typed report carries severity-classified findings plus
        // honest optional estimates; an unknown estimate is null, never 0.0.
        let report = ReadinessReport {
            findings: vec![ReadinessFinding {
                code: ReadinessCode::ResolutionMismatch,
                severity: ReadinessSeverity::BlockingShape,
                message: "Camera A is 1920×1080 but Camera B is 3840×2160".to_string(),
                estimated: false,
            }],
            overlap_estimate: None,
            exposure_delta_stops: None,
        };
        let json = serde_json::to_string(&report).unwrap();
        assert!(
            json.contains("\"code\":\"resolution_mismatch\""),
            "unexpected json: {json}"
        );
        assert!(
            json.contains("\"severity\":\"blocking_shape\""),
            "unexpected json: {json}"
        );
        assert!(
            json.contains("\"overlap_estimate\":null"),
            "an unknown overlap must serialize as null: {json}"
        );
        let back: ReadinessReport = serde_json::from_str(&json).unwrap();
        assert_eq!(report, back);
    }

    #[test]
    fn calibration_event_variants_roundtrip_and_project() {
        let stage = WorkerEvent::CalibrationStage {
            step: CalibrationStage::DetectingProfiles,
            status: StageStatus::Active,
            detail: "Detecting lens profiles".to_string(),
        };
        let json = serde_json::to_string(&stage).unwrap();
        assert!(
            json.contains("\"kind\":\"calibration_stage\""),
            "unexpected json: {json}"
        );
        assert!(
            json.contains("\"step\":\"detecting_profiles\""),
            "unexpected json: {json}"
        );
        let back: WorkerEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(stage, back);
        assert_eq!(stage.to_log_line().level, Level::Info);

        // A blocking-shape readiness finding is a WARN narrative; an
        // informational-only report stays INFO.
        let doomed = WorkerEvent::Readiness {
            report: ReadinessReport {
                findings: vec![ReadinessFinding {
                    code: ReadinessCode::ResolutionMismatch,
                    severity: ReadinessSeverity::BlockingShape,
                    message: "resolutions differ".to_string(),
                    estimated: false,
                }],
                overlap_estimate: None,
                exposure_delta_stops: None,
            },
        };
        assert_eq!(doomed.to_log_line().level, Level::Warn);
        assert!(
            doomed.to_log_line().message.starts_with("readiness:"),
            "unexpected line: {}",
            doomed.to_log_line().message
        );

        let informational = WorkerEvent::Readiness {
            report: ReadinessReport {
                findings: vec![ReadinessFinding {
                    code: ReadinessCode::LensUnavailable,
                    severity: ReadinessSeverity::Informational,
                    message: "no profile".to_string(),
                    estimated: false,
                }],
                overlap_estimate: None,
                exposure_delta_stops: None,
            },
        };
        assert_eq!(informational.to_log_line().level, Level::Info);

        // Result invalidation is a WARN.
        assert_eq!(
            WorkerEvent::ResultInvalidated.to_log_line().level,
            Level::Warn
        );
    }

    #[test]
    fn calibration_failed_roundtrips_and_projects_to_an_error_line() {
        // CALB-04: a failed calibration crosses as a typed diagnosis whose cause
        // and fix are already user-facing, plus the raw error and metrics for the
        // technical disclosure. The frontend renders it; it never invents a cause.
        let diagnosis = CalibrationDiagnosis {
            cause: "No frame pair produced usable matches.".to_string(),
            fix: "Increase overlap between the cameras and set the lens profile.".to_string(),
            raw_error: "no usable frame pairs (all frames failed matching)".to_string(),
            stage: CalibrationStage::Undistorting,
            metrics: DiagnosisMetrics {
                frames_used: 0,
                total_matches: 0,
                post_ratio_test: 0,
                post_spatial_filter: 0,
                post_ransac: 0,
                keypoints_left: 0,
                keypoints_right: 0,
            },
        };
        let event = WorkerEvent::CalibrationFailed {
            diagnosis: diagnosis.clone(),
        };

        let json = serde_json::to_string(&event).unwrap();
        assert!(
            json.contains("\"kind\":\"calibration_failed\""),
            "unexpected json: {json}"
        );
        assert!(
            json.contains("\"diagnosis\""),
            "the diagnosis must ride under `data.diagnosis`: {json}"
        );
        let back: WorkerEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, back);
        assert_eq!(back, WorkerEvent::CalibrationFailed { diagnosis });

        // The log projection is an ERROR line whose message is the cause text.
        let line = event.to_log_line();
        assert_eq!(line.level, Level::Error);
        assert_eq!(line.message, "No frame pair produced usable matches.");
    }

    #[test]
    fn calibration_debug_roundtrips_and_projects_to_an_info_line() {
        // CALB-08: the bounded debug payload crosses as a typed report and
        // projects to an INFO line (UI-SPEC Event Log Contract).
        let report = DebugReport {
            frame_index: 1,
            frames_total: 3,
            left_width: 2,
            left_height: 1,
            right_width: 2,
            right_height: 1,
            left_thumb: vec![0, 0, 0, 255, 255, 255, 255, 255],
            right_thumb: vec![0, 0, 0, 255, 255, 255, 255, 255],
            verified: vec![DebugPoint {
                x_nx: 0.25,
                y_nx: 0.5,
                error: 0.01,
            }],
            rejected: vec![DebugPoint {
                x_nx: 0.75,
                y_nx: 0.5,
                error: 0.2,
            }],
            residual_error: 0.01,
            per_frame: vec![FrameMatchRow {
                frame: 0,
                keypoints_left: 100,
                keypoints_right: 90,
                post_ratio_test: 40,
                post_spatial_filter: 30,
                post_ransac: 25,
            }],
            points_capped: false,
        };
        let event = WorkerEvent::CalibrationDebug {
            report: report.clone(),
        };

        let json = serde_json::to_string(&event).unwrap();
        assert!(
            json.contains("\"kind\":\"calibration_debug\""),
            "unexpected json: {json}"
        );
        assert!(
            json.contains("\"report\""),
            "the report must ride under `data.report`: {json}"
        );
        let back: WorkerEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, back);
        assert_eq!(back, WorkerEvent::CalibrationDebug { report });

        let line = event.to_log_line();
        assert_eq!(line.level, Level::Info);
        assert_eq!(
            line.message,
            "debug data published: frame 2 of 3, 1 verified / 1 rejected matches"
        );
    }

    #[test]
    fn field_roi_events_roundtrip_and_project_to_info_lines() {
        // CALB-09: the applied polygon crosses as the engine's own `FieldRoi`
        // shape (normalized `[0,1]`) and both events project to INFO lines.
        let roi = reco_core::calibration::FieldRoi {
            left: vec![[0.1, 0.9], [0.3, 0.7], [0.5, 0.6]],
            right: vec![[0.6, 0.9], [0.8, 0.7], [0.7, 0.6]],
        };
        let event = WorkerEvent::FieldRoiApplied {
            field_roi: roi.clone(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(
            json.contains("\"kind\":\"field_roi_applied\""),
            "unexpected json: {json}"
        );
        let back: WorkerEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, back);
        assert_eq!(back, WorkerEvent::FieldRoiApplied { field_roi: roi });

        let line = event.to_log_line();
        assert_eq!(line.level, Level::Info);
        assert_eq!(line.message, "field ROI applied: 3 left / 3 right vertices");

        let cleared = WorkerEvent::FieldRoiCleared;
        let json = serde_json::to_string(&cleared).unwrap();
        assert!(
            json.contains("\"kind\":\"field_roi_cleared\""),
            "unexpected json: {json}"
        );
        assert_eq!(
            serde_json::from_str::<WorkerEvent>(&json).unwrap(),
            WorkerEvent::FieldRoiCleared
        );
        assert_eq!(cleared.to_log_line().level, Level::Info);
        assert_eq!(cleared.to_log_line().message, "field ROI cleared");
    }

    #[test]
    fn manual_preview_frame_roundtrips_and_projects_to_an_info_line() {
        // MANU-03: the rendered RGBA crosses as a typed event whose length is
        // exactly `width * height * 4`; the log projection carries the geometry,
        // never the bytes.
        let event = WorkerEvent::ManualPreviewFrame {
            side: ManualSide::Left,
            rgba: vec![0, 0, 0, 255, 255, 255, 255, 255],
            width: 2,
            height: 1,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(
            json.contains("\"kind\":\"manual_preview_frame\""),
            "unexpected json: {json}"
        );
        assert!(
            json.contains("\"side\":\"left\""),
            "unexpected json: {json}"
        );
        let back: WorkerEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, back);

        let line = event.to_log_line();
        assert_eq!(line.level, Level::Info);
        assert_eq!(line.message, "manual preview frame: Left 2×1");
        assert!(
            !line.message.contains("255"),
            "the log line must not carry frame bytes: {line:?}"
        );
    }

    #[test]
    fn manual_session_and_solve_state_events_roundtrip_and_project() {
        let started = WorkerEvent::ManualSessionStarted {
            frame: 3,
            fps: 29.97,
            frames_total: 300,
        };
        let json = serde_json::to_string(&started).unwrap();
        assert!(
            json.contains("\"kind\":\"manual_session_started\""),
            "unexpected json: {json}"
        );
        assert_eq!(serde_json::from_str::<WorkerEvent>(&json).unwrap(), started);
        assert_eq!(started.to_log_line().level, Level::Info);

        let busy = WorkerEvent::ManualSolveState {
            busy: true,
            stale: false,
        };
        assert_eq!(busy.to_log_line().message, "manual solve: solving…");
        let stale = WorkerEvent::ManualSolveState {
            busy: false,
            stale: true,
        };
        assert_eq!(
            stale.to_log_line().message,
            "manual solve: preview shows last solved result"
        );
        let fresh = WorkerEvent::ManualSolveState {
            busy: false,
            stale: false,
        };
        assert_eq!(fresh.to_log_line().message, "manual solve: solved");
    }

    #[test]
    fn audio_sync_result_roundtrips_and_warns_when_absent_or_low() {
        // MANU-02: a confident estimate is INFO and carries the fixed semantics;
        // a low or absent confidence is WARN so the operator is told to set the
        // offset manually. The semantics sentence is the single constant.
        let confident = WorkerEvent::AudioSyncResult {
            offset_frames: 5,
            confidence: Some(0.92),
            offset_semantics: SYNC_OFFSET_SEMANTICS.to_string(),
        };
        let json = serde_json::to_string(&confident).unwrap();
        assert!(
            json.contains("\"kind\":\"audio_sync_result\""),
            "unexpected json: {json}"
        );
        assert_eq!(
            serde_json::from_str::<WorkerEvent>(&json).unwrap(),
            confident
        );
        let line = confident.to_log_line();
        assert_eq!(line.level, Level::Info);
        assert!(line.message.contains(SYNC_OFFSET_SEMANTICS), "{line:?}");

        let low = WorkerEvent::AudioSyncResult {
            offset_frames: 0,
            confidence: Some(AUDIO_SYNC_CONFIDENCE_FLOOR - 0.01),
            offset_semantics: SYNC_OFFSET_SEMANTICS.to_string(),
        };
        assert_eq!(low.to_log_line().level, Level::Warn);

        let unavailable = WorkerEvent::AudioSyncResult {
            offset_frames: 0,
            confidence: None,
            offset_semantics: SYNC_OFFSET_SEMANTICS.to_string(),
        };
        assert_eq!(unavailable.to_log_line().level, Level::Warn);
        assert!(
            unavailable
                .to_log_line()
                .message
                .contains("set the offset manually"),
            "the unavailable line must direct a manual offset"
        );
    }

    #[test]
    fn manual_sync_set_roundtrips_and_projects_to_info() {
        // MANU-02: a manual nudge carries Manual provenance (T-04.1-08) and the
        // fixed semantics sentence.
        let event = WorkerEvent::ManualSyncSet {
            offset_frames: -7,
            method: SyncMethod::Manual,
            offset_semantics: SYNC_OFFSET_SEMANTICS.to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(
            json.contains("\"kind\":\"manual_sync_set\""),
            "unexpected json: {json}"
        );
        assert!(
            json.contains("\"method\":\"manual\""),
            "the manual provenance must ride the payload: {json}"
        );
        assert_eq!(serde_json::from_str::<WorkerEvent>(&json).unwrap(), event);
        let line = event.to_log_line();
        assert_eq!(line.level, Level::Info);
        assert!(line.message.contains(SYNC_OFFSET_SEMANTICS), "{line:?}");
    }

    #[test]
    fn manual_pins_and_solve_result_roundtrip_and_project() {
        // MANU-03 / MANU-04: the pin list carries natural-order pixel coords and
        // a per-pin verified mark; a landed solve carries the typed layout view.
        let pins = WorkerEvent::ManualPins {
            pins: vec![ManualPinView {
                id: 0,
                left_px: [10.0, 20.0],
                right_px: [30.0, 40.0],
                verified: true,
            }],
            seeded: true,
        };
        let json = serde_json::to_string(&pins).unwrap();
        assert!(
            json.contains("\"kind\":\"manual_pins\""),
            "unexpected json: {json}"
        );
        assert!(
            json.contains("\"verified\":true"),
            "verified must ride: {json}"
        );
        assert_eq!(serde_json::from_str::<WorkerEvent>(&json).unwrap(), pins);
        let seeded_line = pins.to_log_line();
        assert_eq!(seeded_line.level, Level::Info);
        assert!(seeded_line.message.contains("seeded"), "{seeded_line:?}");

        let result = WorkerEvent::ManualSolveResult {
            layout: PlaneLayoutView {
                camera_axis_offset: 0.24,
                intersect: 0.55,
                x_ty: 0.01,
                x_rz: 0.0,
                z_rx: 0.0,
                x_rx: 0.0,
                z_rz: 0.0,
            },
            residual: 0.000004,
            pins_used: 10,
            auto_used: 32,
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(
            json.contains("\"kind\":\"manual_solve_result\""),
            "unexpected json: {json}"
        );
        assert_eq!(serde_json::from_str::<WorkerEvent>(&json).unwrap(), result);
        let line = result.to_log_line();
        assert_eq!(line.level, Level::Info);
        assert!(line.message.contains("10 pins"), "{line:?}");
    }
}
