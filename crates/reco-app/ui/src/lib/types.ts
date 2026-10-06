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

/** Advanced calibration options (mirror `events::CalibrationOptions`). */
export interface CalibrationOptions {
  num_frames: number | null;
  skip_start_secs: number | null;
  skip_end_secs: number | null;
  use_imu_rotation_seeds: boolean | null;
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
  | { kind: "profile_loaded"; data: { path: string } }
  | { kind: "profile_saved"; data: { path: string } }
  | { kind: "result_invalidated"; data: null }
  | { kind: "log"; data: LogLine }
  | { kind: "failed"; data: unknown };

/** The workflow-rail screens (D3-01). Import is the landing screen. */
export type Screen = "import" | "calibrate" | "preview";
