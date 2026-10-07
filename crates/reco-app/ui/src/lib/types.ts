/**
 * Shared types for the Phase 2 preview shell frontend.
 *
 * These types mirror the Rust-side vocabulary (WorkerEvent, WorkerCommand,
 * ControlIntent, PresenterKind, ViewMode, TransportState) so the frontend
 * can parse worker events and construct typed commands without inventing
 * parallel vocabularies (02-CONTEXT).
 */

/** The three UI-SPEC log levels. */
export type Level = "info" | "warn" | "error";

/** The flat shape the Rust bridge emits (`events::LogLine`). */
export interface LogLine {
  level: Level;
  message: string;
}

/** Transport state machine states (mirror `transport::TransportState`). */
export type TransportState = "playing" | "paused" | "ended";

/** Presenter kinds (mirror `presenter::PresenterKind`). */
export type PresenterKind = "native" | "separate_window" | "readback";

/** View modes (mirror `presenter::ViewMode`). */
export type ViewMode = "source" | "panorama";

/**
 * A control intent as serialized by `reco_control::ControlIntent`.
 *
 * The serde shape is internally tagged: `{ kind: "pose", data: { action, value } }`.
 * The frontend constructs these to send pose intents via `invoke("intent", ...)`.
 */
export interface ControlIntent {
  kind: "pose";
  data: {
    action:
      | "set_yaw_rad"
      | "set_pitch_rad"
      | "set_fov_deg"
      | "delta_yaw_rad"
      | "delta_pitch_rad"
      | "delta_fov_deg"
      | "reset";
    value: number | null;
  };
}

/** The event name the Rust bridge emits under. */
export const WORKER_EVENT = "worker-event";

/**
 * The typed event name the Rust bridge forwards the full `WorkerEvent` under
 * (E5). Structured consumers read this and never regex-parse `WORKER_EVENT`
 * log lines (FRICTION A3/A12).
 */
export const WORKER_EVENT_TYPED = "worker-event-typed";

/** Which camera input a selection or its metadata belongs to (IMPT-01). */
export type InputRole = "left" | "right";

/** Which camera a manual preview frame belongs to (mirror `events::ManualSide`, MANU-03). */
export type ManualSide = "left" | "right";

/** One correspondence pin as it crosses from the worker (mirror `events::ManualPinView`, MANU-03). */
export interface ManualPinView {
  /** Stable per-session pin id (used by move/remove commands). */
  id: number;
  /** Clicked point on the left frame, `[x, y]` pixels. */
  left_px: [number, number];
  /** Corresponding point on the right frame, `[x, y]` pixels. */
  right_px: [number, number];
  /** Whether the pin was seeded from a verified automatic match (MANU-04). */
  verified: boolean;
}

/**
 * The editable subset of a camera's intrinsics (mirror `events::CameraParamsView`, MANU-05).
 *
 * Only the values an on-image handle can move: the rim drives `k1`, the center
 * crosshair drives `cx`/`cy`, and the explicit scale mode drives `fx` (with
 * `fy = fx`). `k2..k4` live behind the `Advanced lens` disclosure.
 */
export interface CameraParamsView {
  /** Focal length along the x-axis, in pixels (scale mode; `fy` mirrors it). */
  fx: number;
  /** Focal length along the y-axis, in pixels (always equal to `fx`). */
  fy: number;
  /** Principal point x-coordinate, in pixels (center handle). */
  cx: number;
  /** Principal point y-coordinate, in pixels (center handle). */
  cy: number;
  /** First-order fisheye distortion coefficient (rim handle). */
  k1: number;
}

/**
 * The typed result of an opt-in `k1` lens refinement (mirror
 * `events::IntrinsicsRefinementView`, INTR-03).
 *
 * Every field is a real engine value. The held-out residuals are `null` when
 * the conditioning gate refused before any held-out evaluation — never a
 * fabricated `0` (UI-SPEC Readout & Guard Contract). The readout renders this
 * verbatim and never re-types a value.
 */
export interface IntrinsicsRefinementView {
  /** The refined first radial distortion coefficient (`k1`). */
  k1: number;
  /** The profile's `k1` before the refinement (the `old` side of `old → new`). */
  baseline_k1: number;
  /** Whether the refinement passed the held-out guard and may be applied. */
  accepted: boolean;
  /** The engine-authored, user-facing reason — rendered verbatim. */
  reason: string;
  /** Held-out residual at the baseline `k1`, or `null` when not evaluated. */
  heldout_baseline: number | null;
  /** Held-out residual at the refined `k1`, or `null` when not evaluated. */
  heldout_refined: number | null;
}

/**
 * The advisory validation verdict for one manual validation frame (mirror
 * `events::ValidationVerdict`, MANU-07). Advisory only — never a hard gate.
 */
export type ValidationVerdict = "looks_good" | "check_seam";

/** The solved plane layout view (mirror `events::PlaneLayoutView`, MANU-03). */
export interface PlaneLayoutView {
  camera_axis_offset: number;
  intersect: number;
  x_ty: number;
  x_rz: number;
  z_rx: number;
  x_rx: number;
  z_rz: number;
}

/** Whether a metadata value was probed directly or derived (IMPT-02). */
export type Provenance = "probed" | "estimated";

/**
 * One metadata field with its provenance (mirror `events::MetadataField`).
 * `value: null` means unknown — the UI renders an em-dash, never `0`.
 */
export interface MetadataField {
  value: string | null;
  provenance: Provenance;
}

/** The probed metadata for one input (mirror `events::InputMetadata`). */
export interface InputMetadata {
  resolution: MetadataField;
  fps: MetadataField;
  duration: MetadataField;
  codec: MetadataField;
}

/** How serious a readiness finding is (mirror `events::ReadinessSeverity`). */
export type ReadinessSeverity =
  | "blocking_shape"
  | "likely_quality"
  | "informational";

/** Which readiness check produced a finding (mirror `events::ReadinessCode`). */
export type ReadinessCode =
  | "resolution_mismatch"
  | "aspect_mismatch"
  | "fps_mismatch"
  | "codec_mismatch"
  | "lens_resolution_mismatch"
  | "same_file"
  | "exposure_mismatch"
  | "low_overlap"
  | "lens_unavailable";

/** One readiness finding (mirror `events::ReadinessFinding`). */
export interface ReadinessFinding {
  code: ReadinessCode;
  severity: ReadinessSeverity;
  /** User-facing reason authored in Rust; rendered verbatim. */
  message: string;
  /** Whether the underlying value came from a sampled (estimated) pass. */
  estimated: boolean;
}

/**
 * The severity-sorted readiness report (mirror `events::ReadinessReport`).
 * Unknown estimates are `null`, never a fabricated `0`.
 */
export interface ReadinessReport {
  findings: ReadinessFinding[];
  overlap_estimate: number | null;
  exposure_delta_stops: number | null;
}

/** One lens-profile candidate for the override dropdown (mirror `events::LensCandidate`). */
export interface LensCandidate {
  camera: string;
  lens: string;
  width: number;
  height: number;
}

/** Host mirror of the engine's seven calibration steps (mirror `events::CalibrationStage`). */
export type CalibrationStage =
  | "probing"
  | "detecting_profiles"
  | "audio_sync"
  | "extracting_frames"
  | "undistorting"
  | "feature_matching"
  | "optimizing";

/** Status of one row in the stage checklist (mirror `events::StageStatus`). */
export type StageStatus = "pending" | "active" | "done" | "failed" | "skipped";

/** Which path produced a calibration's temporal offset (mirror `events::SyncMethod`). */
export type SyncMethod = "imu" | "audio" | "manual" | "none";

/** Confidence band word for the scorecard (mirror `events::ConfidenceBand`). */
export type ConfidenceBand = "high" | "medium" | "low";

/** The resolved lens profile and its source (mirror `events::LensProfileView`). */
export interface LensProfileView {
  name: string;
  source: string;
}

/** The provenance chain mark over IMU → Audio → Manual (mirror `events::SyncProvenance`). */
export interface SyncProvenance {
  /** The step in the chain that produced the offset. */
  ran: SyncMethod;
  /** Whether the operator set the offset manually. */
  is_manual: boolean;
}

/** The sync method, confidence, signed offset, and fixed semantics (mirror `events::SyncView`). */
export interface SyncView {
  method: SyncMethod;
  confidence: number | null;
  offset_frames: number;
  provenance: SyncProvenance;
  /** The fixed offset-semantics sentence (authored once in Rust). */
  offset_semantics: string;
}

/** The CALB-03 scorecard (mirror `events::Scorecard`). */
export interface Scorecard {
  confidence: number;
  confidence_band: ConfidenceBand;
  residual_error: number;
  total_matches: number;
  per_frame_matches: number;
  frames_used: number;
  lens_profile: LensProfileView | null;
  /** The profile's solved `k1`, the value the opt-in refine action will refine. */
  k1: number;
  sync: SyncView;
}

/** Aggregated per-frame match metrics on a failure diagnosis (mirror `events::DiagnosisMetrics`). */
export interface DiagnosisMetrics {
  frames_used: number;
  total_matches: number;
  post_ratio_test: number;
  post_spatial_filter: number;
  post_ransac: number;
  keypoints_left: number;
  keypoints_right: number;
}

/** A plain-language calibration failure diagnosis (mirror `events::CalibrationDiagnosis`). */
export interface CalibrationDiagnosis {
  cause: string;
  fix: string;
  raw_error: string;
  stage: CalibrationStage;
  metrics: DiagnosisMetrics;
}

/** One feature-match point for the debug inspector (mirror `events::DebugPoint`). */
export interface DebugPoint {
  /** Normalized x in the paired frame (`0..1`). */
  x_nx: number;
  /** Normalized y in the paired frame (`0..1`). */
  y_nx: number;
  /** Reprojection-error proxy for this point (residual map colour). */
  error: number;
}

/** One row of the per-frame match-count table (mirror `events::FrameMatchRow`). */
export interface FrameMatchRow {
  frame: number;
  keypoints_left: number;
  keypoints_right: number;
  post_ratio_test: number;
  post_spatial_filter: number;
  post_ransac: number;
}

/**
 * The bounded debug inspector payload (mirror `events::DebugReport`).
 * Empty thumbnails + zero dimensions mean "no frame pair was retained".
 */
export interface DebugReport {
  frame_index: number;
  frames_total: number;
  left_width: number;
  left_height: number;
  right_width: number;
  right_height: number;
  left_thumb: number[];
  right_thumb: number[];
  verified: DebugPoint[];
  rejected: DebugPoint[];
  residual_error: number;
  per_frame: FrameMatchRow[];
  points_capped: boolean;
}

/**
 * One camera's field-ROI polygon vertices, normalized `[0,1]`
 * (mirror `reco_core::calibration::FieldRoi`'s `Vec<[f64; 2]>`).
 */
export type FieldRoiCamera = [number, number][];

/**
 * The per-camera field ROI polygons for framing (mirror
 * `reco_core::calibration::FieldRoi`). The engine consumes these to filter
 * detections outside the playing field; fewer than three vertices means "no
 * filter" for that camera.
 */
export interface FieldRoi {
  left: FieldRoiCamera;
  right: FieldRoiCamera;
}

/** Advanced calibration options (mirror `events::CalibrationOptions`). */
export interface CalibrationOptions {
  num_frames: number | null;
  skip_start_secs: number | null;
  skip_end_secs: number | null;
  use_imu_rotation_seeds: boolean | null;
}

/** Built-in export presets plus Custom (mirror `events::ExportPreset`, EXPT-01). */
export type ExportPreset = "source_match" | "p1080" | "p4k" | "web" | "custom";

/** The output composition variant (mirror `events::ExportVariant`, EXPT-05). */
export type ExportVariant = "panorama" | "side_by_side" | "stacked";

/**
 * The typed export request (mirror `events::ExportSettings`, EXPT-01).
 *
 * `codec`/`quality` are parsed with a default-on-unknown inside the worker;
 * `start_frame`/`end_frame` are source frame indices the worker converts to
 * seconds against the source fps.
 */
export interface ExportSettings {
  preset: ExportPreset;
  width: number;
  height: number;
  codec: string;
  quality: string;
  bitrate_kbps: number | null;
  encoder_name: string | null;
  start_frame: number | null;
  end_frame: number | null;
  variant: ExportVariant;
  output_dir: string | null;
}

/** One probed encoder (mirror `events::EncoderView`, EXPT-02). */
export interface EncoderView {
  name: string;
  description: string;
  is_hardware: boolean;
}

/** One referenced camera input (mirror `project::ProjectInput`, PROJ-01). */
export interface ProjectInput {
  /** The local file path the project references (media is never copied). */
  path: string;
  /** The per-input lens override, or `null` for auto-detect. */
  lens_override: LensCandidate | null;
}

/** The restored operator pose (mirror `project::PoseView`, PROJ-01). */
export interface ProjectPoseView {
  /** Yaw in radians. */
  yaw: number;
  /** Pitch in radians. */
  pitch: number;
  /** Vertical FOV in degrees. */
  fov_degrees: number;
}

/** One missing project input (mirror `project::MissingInput`, PROJ-01). */
export interface MissingInput {
  /** Which camera the missing path belongs to. */
  role: InputRole;
  /** The missing path (shown verbatim in the relocate dialog). */
  path: string;
}

/**
 * The probed system information (mirror `system::SystemInfoView`, DIAG-01).
 *
 * Every GPU field is `null` when unknown — the panel renders `Not reported`,
 * never a fabricated `0`. `devices_note` carries an honest reason when the
 * platform does not enumerate camera devices.
 */
export interface SystemInfoView {
  gpu_name: string | null;
  backend: string | null;
  driver: string | null;
  encoders: EncoderView[];
  devices: string[];
  devices_note: string | null;
}

/** One structured engine log record (mirror `system::LogRecord`, DIAG-02). */
export interface LogRecord {
  /** Lowercase tracing level (`info` / `warn` / `error` / `debug` / `trace`). */
  level: string;
  /** The emitting module path. */
  target: string;
  /** The formatted message. */
  message: string;
  /** Unix epoch time in milliseconds. */
  timestamp_ms: number;
}

/** One checked runtime prerequisite (mirror `preflight::PreflightItem`, DIAG-05). */
export interface PreflightItem {
  id: string;
  label: string;
  ok: boolean;
  detail: string;
  /** Actionable next step; non-empty whenever `ok` is false. */
  remediation: string;
}

/** The runtime preflight verdict (mirror `preflight::PreflightReport`, DIAG-05). */
export interface PreflightReport {
  items: PreflightItem[];
  all_ok: boolean;
}

/**
 * The subset of the typed `WorkerEvent` union this phase consumes.
 *
 * Serde shape is internally tagged: `{ kind, data }`. The import store reads
 * `import_metadata`; the calibration store reads the calibration/profile
 * variants. The union stays open so later variants can be added without a
 * parallel vocabulary.
 */
export type WorkerEventTyped =
  | { kind: "import_metadata"; data: { role: InputRole; metadata: InputMetadata } }
  | { kind: "readiness"; data: { report: ReadinessReport } }
  | { kind: "lens_candidates"; data: { role: InputRole; candidates: LensCandidate[] } }
  | {
      kind: "lens_override_applied";
      data: { role: InputRole; candidate: LensCandidate | null };
    }
  | {
      kind: "calibration_stage";
      data: { step: CalibrationStage; status: StageStatus; detail: string };
    }
  | { kind: "calibration_progress"; data: { fraction: number } }
  | {
      kind: "calibration_heartbeat";
      data: { elapsed_ms: number; step: CalibrationStage; last_detail: string };
    }
  | { kind: "calibration_result"; data: { scorecard: Scorecard } }
  | { kind: "calibration_failed"; data: { diagnosis: CalibrationDiagnosis } }
  | { kind: "calibration_debug"; data: { report: DebugReport } }
  | { kind: "field_roi_applied"; data: { field_roi: FieldRoi } }
  | { kind: "field_roi_cleared"; data: null }
  | { kind: "profile_loaded"; data: { path: string } }
  | { kind: "profile_saved"; data: { path: string } }
  | {
      kind: "manual_session_started";
      data: { frame: number; fps: number; frames_total: number };
    }
  | { kind: "manual_solve_state"; data: { busy: boolean; stale: boolean; degenerate: boolean } }
  | { kind: "manual_pins"; data: { pins: ManualPinView[]; seeded: boolean } }
  | {
      kind: "manual_solve_result";
      data: {
        layout: PlaneLayoutView;
        residual: number;
        pins_used: number;
        auto_used: number;
      };
    }
  | {
      kind: "manual_params";
      data: {
        left: CameraParamsView;
        right: CameraParamsView;
        layout: PlaneLayoutView;
      };
    }
  | {
      kind: "manual_layout_delta";
      data: {
        cam_d: number;
        intersect: number;
        x_ty: number;
        x_rz: number;
      };
    }
  | {
      kind: "audio_sync_result";
      data: {
        offset_frames: number;
        confidence: number | null;
        /** The fixed offset-semantics sentence (authored once in Rust). */
        offset_semantics: string;
      };
    }
  | {
      kind: "manual_sync_set";
      data: {
        offset_frames: number;
        method: SyncMethod;
        /** The fixed offset-semantics sentence (authored once in Rust). */
        offset_semantics: string;
      };
    }
  | {
      kind: "manual_validation_frame";
      data: {
        frame: number;
        width: number;
        height: number;
        /** Per-frame residual (px). */
        residual: number;
        verdict: ValidationVerdict;
        reference_width: number;
        reference_height: number;
      };
    }
  | { kind: "manual_saved"; data: { path: string } }
  | { kind: "intrinsics_refined"; data: { refinement: IntrinsicsRefinementView } }
  | { kind: "result_invalidated"; data: null }
  | {
      kind: "export_progress";
      data: {
        frames_completed: number;
        total: number | null;
        elapsed_ms: number;
        eta_ms: number | null;
        percent: number;
      };
    }
  | {
      kind: "export_finished";
      data: {
        path: string;
        encoder: string;
        hardware: boolean;
        variant: ExportVariant;
      };
    }
  | { kind: "export_cancelled"; data: null }
  | { kind: "export_failed"; data: { message: string } }
  | { kind: "export_path_preview"; data: { path: string } }
  | {
      kind: "encoder_list";
      data: {
        encoders: EncoderView[];
        auto: EncoderView;
        auto_hardware: boolean;
      };
    }
  | {
      kind: "export_fallback";
      data: { requested: string; used: string; used_hardware: boolean };
    }
  | { kind: "system_info"; data: { info: SystemInfoView } }
  | { kind: "preflight"; data: { report: PreflightReport } }
  | { kind: "log_record"; data: { record: LogRecord } }
  | { kind: "project_saved"; data: { path: string } }
  | {
      kind: "project_opened";
      data: {
        path: string;
        left: ProjectInput;
        right: ProjectInput;
        calibration_path: string | null;
        has_calibration: boolean;
        pose: ProjectPoseView;
        export: ExportSettings;
      };
    }
  | { kind: "project_missing_inputs"; data: { missing: MissingInput[] } }
  | {
      kind: "diagnostics_bundle_written";
      data: { path: string; files: number; redacted: boolean };
    }
  | { kind: "log"; data: LogLine }
  | { kind: "failed"; data: unknown };

/** The workflow-rail screens (D3-01). Import is the landing screen. */
export type Screen = "import" | "calibrate" | "preview" | "export" | "system";
