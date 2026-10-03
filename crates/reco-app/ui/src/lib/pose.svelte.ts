/**
 * Pose rune store (PREV-04).
 *
 * Wraps the typed WorkerCommand::Intent protocol for pose input: drag pan,
 * wheel zoom, FOV slider, arrow nudge, and Reset view. Mirrors the worker's
 * authoritative pose state.
 *
 * The frontend keeps no pose state of its own beyond mirroring the worker's
 * Pose events (UI-SPEC Interaction rule 1: the worker's value wins). Pose
 * input is translated into `reco_control::ControlIntent` and sent as
 * `WorkerCommand::Intent` — no parallel input vocabulary (02-CONTEXT).
 *
 * Pure UI (CONTEXT D-02/D-06): this store only sends worker commands over
 * Tauri IPC and renders worker events.
 */

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { ControlIntent, LogLine } from "./types";
import { WORKER_EVENT } from "./types";

/** The worker's authoritative pose state. */
export interface PoseState {
  /** Current yaw in radians. */
  yaw: number;
  /** Current pitch in radians. */
  pitch: number;
  /** Current vertical FOV in degrees. */
  fov: number;
  /**
   * The widest FOV the stitchable coverage allows, in degrees.
   *
   * The pose is clamped to this every tick, so a control offering a wider range
   * does nothing above it. Optional so an older or partial payload still parses
   * — consumers fall back to the configured maximum.
   */
  fovMax?: number;
}

/**
 * Parse a "pose: yaw X, pitch Y, fov Z" log message.
 * Returns the pose state or null when the message is not a pose line.
 */
function parsePose(message: string): PoseState | null {
  // The trailing `max` is OPTIONAL: it was appended after the original three
  // fields so that a payload without it still parses rather than failing the
  // whole match. Anchoring only the known prefix is what keeps that true.
  const m = message.match(
    /^pose: yaw ([-\d.]+), pitch ([-\d.]+), fov ([-\d.]+)(?:, max ([-\d.]+))?$/,
  );
  if (!m) return null;
  return {
    yaw: Number(m[1]),
    pitch: Number(m[2]),
    fov: Number(m[3]),
    ...(m[4] !== undefined ? { fovMax: Number(m[4]) } : {}),
  };
}

/** The fixed arrow-nudge step in radians. */
const NUDGE_STEP_RAD = 0.05;

/** The pose rune store. */
class PoseStore {
  /** The worker's authoritative pose (null until the first Pose event). */
  pose = $state<PoseState | null>(null);
  /** Whether pose controls are enabled (true after the first pose event). */
  enabled = $state(false);

  /** Unlisten function for the worker-event listener. */
  #unlisten: (() => void) | null = null;

  /** Start listening to worker events. */
  async init(): Promise<void> {
    if (this.#unlisten !== null) return;
    this.#unlisten = await listen<LogLine>(WORKER_EVENT, (event) => {
      this.#onEvent(event.payload);
    });
  }

  /** Stop listening to worker events. */
  destroy(): void {
    if (this.#unlisten !== null) {
      this.#unlisten();
      this.#unlisten = null;
    }
  }

  /** Handle a worker event (projected to a LogLine). */
  #onEvent(line: LogLine): void {
    const pose = parsePose(line.message);
    if (pose !== null) {
      this.pose = pose;
      this.enabled = true;
    }
  }

  /** Send a pose intent to the worker. */
  async #sendIntent(intent: ControlIntent): Promise<void> {
    try {
      await invoke("intent", { intent });
    } catch {
      // The worker will emit a Pose event to correct the state.
    }
  }

  /** Nudge yaw by `delta` radians (drag or arrow key). */
  async nudgeYaw(delta: number): Promise<void> {
    await this.#sendIntent({
      kind: "pose",
      data: { action: "delta_yaw_rad", value: delta },
    });
  }

  /** Nudge pitch by `delta` radians (drag or arrow key). */
  async nudgePitch(delta: number): Promise<void> {
    await this.#sendIntent({
      kind: "pose",
      data: { action: "delta_pitch_rad", value: delta },
    });
  }

  /** Nudge FOV by `delta` degrees (mouse wheel). */
  async nudgeFov(delta: number): Promise<void> {
    await this.#sendIntent({
      kind: "pose",
      data: { action: "delta_fov_deg", value: delta },
    });
  }

  /** Set FOV to an absolute value (slider). */
  async setFov(degrees: number): Promise<void> {
    const clamped = Math.max(40, Math.min(150, degrees));
    await this.#sendIntent({
      kind: "pose",
      data: { action: "set_fov_deg", value: clamped },
    });
  }

  /** Reset the pose to the configured rest position. */
  async reset(): Promise<void> {
    await this.#sendIntent({
      kind: "pose",
      data: { action: "reset", value: null },
    });
  }

  /**
   * Arrow-key nudge: yaw by one step.
   *
   * `dir` is the sign of the yaw DELTA, not the screen direction. `PoseControl`
   * treats +yaw as looking LEFT (verified against `view_matrix`, and matching
   * the CLI's arrow mapping), so Left passes +1 and Right passes -1. The
   * default keeps the previous always-positive behaviour for callers that do
   * not care.
   */
  async nudgeYawStep(dir: 1 | -1 = 1): Promise<void> {
    await this.nudgeYaw(dir * NUDGE_STEP_RAD);
  }

  /**
   * Arrow-key nudge: pitch by one step.
   *
   * `dir` is the sign of the pitch DELTA. +pitch looks UP, so Up passes +1 and
   * Down passes -1.
   */
  async nudgePitchStep(dir: 1 | -1 = 1): Promise<void> {
    await this.nudgePitch(dir * NUDGE_STEP_RAD);
  }

  /** The current FOV value for the slider, or 75 (default) when unknown. */
  get fovValue(): number {
    return this.pose?.fov ?? 75;
  }

  /**
   * The widest FOV this clip's coverage allows, for bounding the slider.
   *
   * Falls back to 150 — the configured maximum — until the worker reports the
   * coverage ceiling, so the control is never *below* the real range.
   */
  get fovMax(): number {
    return this.pose?.fovMax ?? 150;
  }
}

/** The shared pose store instance. */
export const pose = new PoseStore();
