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
