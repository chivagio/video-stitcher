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

/**
 * The subset of the typed `WorkerEvent` union this phase consumes.
 *
 * Serde shape is internally tagged: `{ kind, data }`. The import store reads
 * `import_metadata`; the union stays open so later calibration variants can be
 * added without a parallel vocabulary.
 */
export type WorkerEventTyped =
  | { kind: "import_metadata"; data: { role: InputRole; metadata: InputMetadata } }
  | { kind: "log"; data: LogLine }
  | { kind: "failed"; data: unknown };

/** The workflow-rail screens (D3-01). Import is the landing screen. */
export type Screen = "import" | "calibrate" | "preview";
