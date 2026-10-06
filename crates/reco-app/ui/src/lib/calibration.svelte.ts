/**
 * Calibration rune store (CALB-01/02/03).
 *
 * Wraps the typed `WorkerCommand`/`WorkerEvent` protocol for the guided
 * calibration wizard. The worker is authoritative for stage, progress, result
 * and sync info (UI-SPEC Interaction rule 1): this store mirrors the typed
 * `CalibrationStage` / `CalibrationProgress` / `CalibrationHeartbeat` /
 * `CalibrationResult` events and **never derives stage state locally**.
 *
 * Cancel (CALB-02) sets the shared `Arc<AtomicBool>` via the direct
 * `cancel_calibration` command; the worker responds with a WARN log line
 * ("calibration cancelled — partial result discarded") and no result. The store
 * treats that warn line as the signal to return to the ready state without a
 * partial scorecard.
 *
 * Pure UI (D-06): this store only sends worker commands over Tauri IPC and
 * renders worker events. It never touches engine types or window lifecycle.
 */

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type {
  CalibrationDiagnosis,
  CalibrationOptions,
  CalibrationStage,
  Scorecard,
  StageStatus,
  WorkerEventTyped,
} from "./types";
import { WORKER_EVENT_TYPED } from "./types";
import { formatWorkerError } from "./errors";

/** The wizard's screen state machine (UI-SPEC Calibration Wizard Contract). */
export type CalibrationStatus =
  | "ready"
  | "running"
  | "cancelling"
  | "done"
  | "failed";

/** The seven engine stages, in their locked order (CALB-01 / D3-09). */
export const CALIBRATION_STAGES: CalibrationStage[] = [
  "probing",
  "detecting_profiles",
  "audio_sync",
  "extracting_frames",
  "undistorting",
  "feature_matching",
  "optimizing",
];

/** The locked stage names (UI-SPEC Copywriting Contract — Calibrate screen). */
export const STAGE_NAMES: Record<CalibrationStage, string> = {
  probing: "Probing",
  detecting_profiles: "Detecting lens profiles",
  audio_sync: "Syncing audio",
  extracting_frames: "Extracting frames",
  undistorting: "Correcting distortion",
  feature_matching: "Matching features",
  optimizing: "Optimizing alignment",
};

/** Human-readable status words for the checklist (locked vocabulary). */
export const STAGE_STATUS_WORDS: Record<StageStatus, string> = {
  pending: "Waiting",
  active: "In progress",
  done: "Done",
  failed: "Failed",
  skipped: "Skipped",
};

/** All seven stages default to pending, never absent (E10 empty). */
export function emptyStages(): Record<CalibrationStage, StageStatus> {
  return {
    probing: "pending",
    detecting_profiles: "pending",
    audio_sync: "pending",
    extracting_frames: "pending",
    undistorting: "pending",
    feature_matching: "pending",
    optimizing: "pending",
  };
}

/** Empty per-stage detail strings. */
function emptyDetails(): Record<CalibrationStage, string> {
  return {
    probing: "",
    detecting_profiles: "",
    audio_sync: "",
    extracting_frames: "",
    undistorting: "",
    feature_matching: "",
    optimizing: "",
  };
}

/** Format a millisecond duration as `M:SS` (bounded; never `H:MM:SS`). */
export function formatMSS(ms: number): string {
  const totalSeconds = Math.max(0, Math.floor(ms / 1000));
  const minutes = Math.floor(totalSeconds / 60);
  const seconds = totalSeconds % 60;
  return `${minutes}:${String(seconds).padStart(2, "0")}`;
}

/** The store's default advanced options (every field "use the engine default"). */
export function defaultOptions(): CalibrationOptions {
  return {
    num_frames: null,
    skip_start_secs: null,
    skip_end_secs: null,
    use_imu_rotation_seeds: null,
  };
}

/**
 * The calibration rune store (class with `$state` fields, shared across
 * components).
 */
class CalibrationStore {
  /** The wizard's current screen state. */
  status = $state<CalibrationStatus>("ready");
  /** Per-stage checklist status; always all seven keys. */
  stages = $state<Record<CalibrationStage, StageStatus>>(emptyStages());
  /** Per-stage detail line (from typed `CalibrationStage` events). */
  detail = $state<Record<CalibrationStage, string>>(emptyDetails());
  /** Overall progress, 0.0-1.0 (from typed `CalibrationProgress`). */
  fraction = $state(0);
  /** Milliseconds since the run started (from typed `CalibrationHeartbeat`). */
  elapsedMs = $state(0);
  /** The last detail the engine reported (from the heartbeat). */
  lastDetail = $state("");
  /** The stage the run has reached (drives the progress label). */
  activeStage = $state<CalibrationStage | null>(null);
  /** The completed run's scorecard, or null. */
  result = $state<Scorecard | null>(null);
  /** The typed failure text when `status === "failed"`. */
  error = $state<string | null>(null);
  /** The plain-language failure diagnosis (CALB-04) when `status === "failed"`. */
  diagnosis = $state<CalibrationDiagnosis | null>(null);

  /** Wall-clock time of the last heartbeat tick (for the "last update" line). */
  #lastHeartbeatAt: number | null = null;
  /** A coarse clock so the heartbeat "ago" line advances without a tick. */
  #clock = $state(Date.now());
  /** The clock interval handle (only while running). */
  #clockTimer: ReturnType<typeof setInterval> | null = null;
  /** Unlisten function for the typed worker-event listener. */
  #unlisten: (() => void) | null = null;

  /** Start listening for typed worker events. */
  async init(): Promise<void> {
    if (this.#unlisten !== null) return;
    this.#unlisten = await listen<WorkerEventTyped>(
      WORKER_EVENT_TYPED,
      (event) => {
        this.#onEvent(event.payload);
      },
    );
  }

  /** Stop listening and clear the clock. */
  destroy(): void {
    if (this.#unlisten !== null) {
      this.#unlisten();
      this.#unlisten = null;
    }
    this.#stopClock();
  }

  /** Apply a typed worker event to the store state (worker-authoritative). */
  #onEvent(event: WorkerEventTyped): void {
    switch (event.kind) {
      case "calibration_stage": {
        const { step, status, detail } = event.data;
        this.stages[step] = status;
        if (detail !== "") this.detail[step] = detail;
        // Any stage transition marks the run's current stage; a `done` on the
        // last stage keeps it as the progress label's stage.
        this.activeStage = step;
        break;
      }
      case "calibration_progress": {
        this.fraction = event.data.fraction;
        break;
      }
      case "calibration_heartbeat": {
        this.elapsedMs = event.data.elapsed_ms;
        this.lastDetail = event.data.last_detail;
        this.#lastHeartbeatAt = Date.now();
        break;
      }
      case "calibration_result": {
        this.result = event.data.scorecard;
        this.fraction = 1;
        this.status = "done";
        this.error = null;
        this.diagnosis = null;
        this.#stopClock();
        break;
      }
      case "calibration_failed": {
        // CALB-04: the worker authors the plain-language cause/fix in Rust; the
        // store only stores the typed diagnosis and marks the run failed.
        this.diagnosis = event.data.diagnosis;
        this.error = event.data.diagnosis.cause;
        this.status = "failed";
        this.#stopClock();
        break;
      }
      case "log": {
        // A cancelled run is signalled by a WARN log line; there is no result.
        // Return to ready and discard any partial result (CALB-02).
        const { level, message } = event.data;
        if (
          this.status === "cancelling" &&
          level === "warn" &&
          message.includes("calibration cancelled")
        ) {
          this.status = "ready";
          this.result = null;
          this.error = null;
          this.#stopClock();
        }
        break;
      }
      case "failed": {
        // Only a failure while a run is in flight is a calibration failure;
        // import/profile failures are owned by the import store.
        if (this.status === "running" || this.status === "cancelling") {
          this.error = formatWorkerError(event.data);
          this.status = "failed";
          this.#stopClock();
        }
        break;
      }
      default:
        break;
    }
  }

  /** Start a fresh calibration run with the given advanced options. */
  async start(options: CalibrationOptions): Promise<void> {
    this.#clearRun();
    this.status = "running";
    this.#startClock();
    try {
      await invoke("start_calibration", { options });
    } catch (e) {
      // The command boundary rejected the options (T-03-16); surface typed text.
      this.error = formatWorkerError(e);
      this.status = "failed";
      this.#stopClock();
    }
  }

  /**
   * Request cancellation of the running calibration (CALB-02).
   *
   * Flips the shared cancel flag; the worker returns a WARN log line when it
   * observes it, which the store reduces to the ready state. `cancel_calibration`
   * never fails, so a rejected invoke is swallowed and the run stays cancelling
   * until the worker's warn line arrives.
   */
  async cancel(): Promise<void> {
    if (this.status !== "running") return;
    this.status = "cancelling";
    try {
      await invoke("cancel_calibration");
    } catch {
      // Never fails; the warn log line remains the terminal signal.
    }
  }

  /** Discard the run state and return to the ready screen. */
  reset(): void {
    this.#clearRun();
    this.status = "ready";
    this.#stopClock();
  }

  /** Clear every per-run field back to its initial value. */
  #clearRun(): void {
    this.stages = emptyStages();
    this.detail = emptyDetails();
    this.fraction = 0;
    this.elapsedMs = 0;
    this.lastDetail = "";
    this.activeStage = null;
    this.result = null;
    this.error = null;
    this.diagnosis = null;
    this.#lastHeartbeatAt = null;
  }

  #startClock(): void {
    this.#stopClock();
    this.#clock = Date.now();
    this.#clockTimer = setInterval(() => {
      this.#clock = Date.now();
    }, 1000);
  }

  #stopClock(): void {
    if (this.#clockTimer !== null) {
      clearInterval(this.#clockTimer);
      this.#clockTimer = null;
    }
  }

  /** Whether a run is in flight (drives the import store's failure routing). */
  get isActive(): boolean {
    return this.status === "running" || this.status === "cancelling";
  }

  /** Milliseconds since the last heartbeat tick (for the "last update" line). */
  get heartbeatAgoMs(): number {
    if (this.#lastHeartbeatAt === null) return 0;
    return Math.max(0, this.#clock - this.#lastHeartbeatAt);
  }

  /** The stage driving the progress label (Probing before any transition). */
  get currentStage(): CalibrationStage {
    return this.activeStage ?? "probing";
  }

  /** The 1-based ordinal of the current stage (1..7) for "Step n of 7". */
  get currentIndex(): number {
    const idx = CALIBRATION_STAGES.indexOf(this.currentStage);
    return idx < 0 ? 1 : idx + 1;
  }
}

/** The shared calibration store instance. */
export const calibration = new CalibrationStore();
