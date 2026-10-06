/**
 * Manual calibration rune store (MANU-01 / MANU-03).
 *
 * Wraps the typed `WorkerCommand`/`WorkerEvent` protocol for the manual
 * calibration flow. The worker is authoritative for the session, the retained
 * reference frame, and the rendered preview (UI-SPEC Interaction rule 1): this
 * store mirrors the typed `ManualSessionStarted` / `ManualPreviewFrame` /
 * `ManualSolveState` events and **never derives them locally**.
 *
 * The preview buffers are the RGBA bytes the worker produced by the GPU
 * undistort under real `CameraParams`; the frontend paints them on a canvas and
 * never touches the GPU (D-06 / T-04.1-04).
 *
 * Pure UI: this store only sends worker commands over Tauri IPC and renders
 * worker events. It never touches engine types or window lifecycle.
 */

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { WorkerEventTyped } from "./types";
import { WORKER_EVENT_TYPED } from "./types";
import { formatWorkerError } from "./errors";

/** The locked 5-step flow order (UI-SPEC Copywriting Contract). */
export type ManualStep = "time-align" | "frame" | "pin" | "bend" | "validate";

/** The step order, single source for the nav and index math. */
export const MANUAL_STEPS: ManualStep[] = [
  "time-align",
  "frame",
  "pin",
  "bend",
  "validate",
];

/** The locked step names (UI-SPEC Copywriting Contract). */
export const MANUAL_STEP_NAMES: Record<ManualStep, string> = {
  "time-align": "Time-align",
  frame: "Frame",
  pin: "Pin",
  bend: "Bend",
  validate: "Validate",
};

/** One camera's rendered preview frame (mirror `events::ManualPreviewFrame`). */
export interface ManualPreview {
  /** RGBA bytes (`width * height * 4`). */
  rgba: number[];
  width: number;
  height: number;
}

/**
 * The manual calibration rune store (class with `$state` fields, shared across
 * components).
 */
class ManualStore {
  /** Whether the manual flow surface is open. */
  open = $state(false);
  /** The active step in the guided flow. */
  step = $state<ManualStep>("time-align");
  /** The current reference frame index (worker-authoritative). */
  frame = $state(0);
  /** Total frames in the reference clip (from `manual_session_started`). */
  framesTotal = $state(0);
  /** The reference clip's frame rate (from `manual_session_started`). */
  fps = $state(0);
  /** The left camera's rendered preview, or null before the first frame. */
  previewLeft = $state<ManualPreview | null>(null);
  /** The right camera's rendered preview, or null before the first frame. */
  previewRight = $state<ManualPreview | null>(null);
  /** Whether a background solve is in flight. */
  solving = $state(false);
  /** Whether the preview shows the last solved (stale) result. */
  stale = $state(false);
  /** The last typed rejection, or null. */
  error = $state<string | null>(null);

  /** Unlisten function for the typed worker-event listener. */
  #unlisten: (() => void) | null = null;

  /** Start listening for typed worker events (idempotent). */
  async init(): Promise<void> {
    if (this.#unlisten !== null) return;
    this.#unlisten = await listen<WorkerEventTyped>(
      WORKER_EVENT_TYPED,
      (event) => {
        this.#onEvent(event.payload);
      },
    );
  }

  /** Stop listening. */
  destroy(): void {
    if (this.#unlisten !== null) {
      this.#unlisten();
      this.#unlisten = null;
    }
  }

  /** Apply a typed worker event to the store state (worker-authoritative). */
  #onEvent(event: WorkerEventTyped): void {
    switch (event.kind) {
      case "manual_session_started": {
        const { frame, fps, frames_total } = event.data;
        this.frame = frame;
        this.fps = fps;
        this.framesTotal = frames_total;
        break;
      }
      case "manual_preview_frame": {
        const { side, rgba, width, height } = event.data;
        const preview: ManualPreview = { rgba, width, height };
        if (side === "left") {
          this.previewLeft = preview;
        } else {
          this.previewRight = preview;
        }
        break;
      }
      case "manual_solve_state": {
        this.solving = event.data.busy;
        this.stale = event.data.stale;
        break;
      }
      case "failed": {
        if (this.open) {
          this.error = formatWorkerError(event.data);
        }
        break;
      }
      default:
        break;
    }
  }

  /**
   * Open a manual session at `frame` (MANU-01).
   *
   * Subscribes first, then posts `manual_begin`, so the worker's
   * `manual_session_started` / `manual_preview_frame` events are never dropped
   * by a not-yet-registered listener (the Tauri bridge is fire-and-forget).
   */
  async begin(frame = 0): Promise<void> {
    await this.init();
    this.open = true;
    this.error = null;
    this.previewLeft = null;
    this.previewRight = null;
    try {
      await invoke("manual_begin", { frame });
    } catch (e) {
      this.error = formatWorkerError(e);
    }
  }

  /** Change the reference frame (debounced by the caller if scrubbing). */
  async setFrame(frame: number): Promise<void> {
    this.frame = frame;
    try {
      await invoke("manual_set_frame", { frame });
    } catch (e) {
      this.error = formatWorkerError(e);
    }
  }

  /**
   * Close the manual session (MANU-01).
   *
   * Clears the local preview and posts `manual_exit`; the worker drops the
   * retained planes. Exiting never touches an existing calibration profile.
   */
  async exit(): Promise<void> {
    this.open = false;
    this.previewLeft = null;
    this.previewRight = null;
    this.solving = false;
    this.stale = false;
    this.error = null;
    try {
      await invoke("manual_exit");
    } catch {
      // Exiting is best-effort; a closed worker is not an error the user sees.
    }
  }

  /** Navigate to a step (free back-navigation; any step is reachable). */
  goToStep(step: ManualStep): void {
    this.step = step;
  }

  /** Move to the next step, bounded at the last. */
  next(): void {
    const idx = MANUAL_STEPS.indexOf(this.step);
    if (idx < MANUAL_STEPS.length - 1) this.step = MANUAL_STEPS[idx + 1];
  }

  /** Move to the previous step, bounded at the first. */
  back(): void {
    const idx = MANUAL_STEPS.indexOf(this.step);
    if (idx > 0) this.step = MANUAL_STEPS[idx - 1];
  }

  /** The 0-based index of the active step. */
  get stepIndex(): number {
    const idx = MANUAL_STEPS.indexOf(this.step);
    return idx < 0 ? 0 : idx;
  }
}

/** The shared manual calibration store instance. */
export const manual = new ManualStore();
