/**
 * Project rune store (PROJ-01).
 *
 * Wraps the typed `WorkerCommand`/`WorkerEvent` protocol for the `.reco`
 * non-destructive project: save, open, and the relocate-on-missing-input flow.
 * The worker is authoritative — it owns the manifest, probes the referenced
 * inputs, and restores the whole state; this store mirrors the typed
 * `ProjectSaved` / `ProjectOpened` / `ProjectMissingInputs` events and never
 * reads or writes a manifest itself.
 *
 * On `project_opened` the store reconciles the import, pose, and export stores
 * from the typed payload (the worker re-emits `ImportMetadata` /
 * `LensOverrideApplied` during the restore, so the input slots fill in).
 *
 * Pure UI (D-06): this store only sends worker commands over Tauri IPC and
 * renders worker events. It never touches engine types or window lifecycle.
 */

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open, save } from "@tauri-apps/plugin-dialog";
import type {
  ExportSettings,
  InputRole,
  MissingInput,
  ProjectInput,
  ProjectPoseView,
  WorkerEventTyped,
} from "./types";
import { WORKER_EVENT_TYPED } from "./types";
import { formatWorkerError } from "./errors";
import { importStore } from "./import.svelte";
import { exportStore } from "./export.svelte";
import { pose } from "./pose.svelte";

/** The project store's state machine (UI-SPEC Project open/save + relocate). */
export type ProjectStatus =
  | "idle"
  | "saving"
  | "saved"
  | "opening"
  | "opened"
  | "missing"
  | "error";

/** Project extension for the native open/save dialogs. */
const PROJECT_EXTENSIONS = ["reco"];

/** Video extensions offered when relocating a missing input. */
const VIDEO_EXTENSIONS = ["mp4", "mov", "mkv", "avi", "webm", "m4v"];

/** The typed payload of a `project_opened` event. */
interface ProjectOpenedData {
  path: string;
  left: ProjectInput;
  right: ProjectInput;
  calibration_path: string | null;
  has_calibration: boolean;
  pose: ProjectPoseView;
  export: ExportSettings;
}

/**
 * The project rune store (class with `$state` fields, shared across components).
 */
class ProjectStore {
  /** The path of the last saved/opened project, or null. */
  path = $state<string | null>(null);
  /** The inputs still missing after an open (drives the relocate dialog). */
  missing = $state<MissingInput[]>([]);
  /** The state machine. */
  status = $state<ProjectStatus>("idle");
  /** The typed failure text when `status === "error"`. */
  error = $state<string | null>(null);
  /** The path of the last successful save (shown in the success line). */
  lastSavedPath = $state<string | null>(null);
  /**
   * A transient success confirmation for the operator, or null. Set to
   * `Input relocated — project restored.` after a relocate completes (the
   * UI-SPEC "Relocate resolved" copy); dismissed explicitly or on the next
   * open.
   */
  notice = $state<string | null>(null);

  /** Unlisten function for the typed worker-event listener. */
  #unlisten: (() => void) | null = null;
  /**
   * Whether the current open is a relocate flow. Distinguishes a restore after
   * relocating a missing input (which announces the contract confirmation) from
   * a plain open (which is silent).
   */
  #relocating = false;

  /** Start listening for typed worker events. */
  async init(): Promise<void> {
    if (this.#unlisten !== null) return;
    this.#unlisten = await listen<WorkerEventTyped>(WORKER_EVENT_TYPED, (event) => {
      this.#onEvent(event.payload);
    });
  }

  /** Stop listening. */
  destroy(): void {
    if (this.#unlisten !== null) {
      this.#unlisten();
      this.#unlisten = null;
    }
  }

  /** Apply a typed worker event to the store state. */
  #onEvent(event: WorkerEventTyped): void {
    switch (event.kind) {
      case "project_saved": {
        this.lastSavedPath = event.data.path;
        this.path = event.data.path;
        this.status = "saved";
        this.error = null;
        break;
      }
      case "project_opened": {
        this.#applyOpened(event.data);
        break;
      }
      case "project_missing_inputs": {
        this.missing = event.data.missing;
        this.status = "missing";
        this.error = null;
        break;
      }
      case "failed": {
        // The generic failure carries no operation tag; only claim it when a
        // project operation is actually in flight (mirrors the import store's
        // in-flight routing).
        if (this.status === "saving" || this.status === "opening") {
          this.error = formatWorkerError(event.data);
          this.status = "error";
        }
        break;
      }
      default:
        break;
    }
  }

  /** Reconcile the import/pose/export stores from a completed open (PROJ-01). */
  #applyOpened(data: ProjectOpenedData): void {
    this.path = data.path;
    this.missing = [];
    this.status = "opened";
    this.error = null;

    // A restore that followed a relocate announces the declared confirmation;
    // a plain open is silent (UI-SPEC "Relocate resolved").
    if (this.#relocating) {
      this.notice = "Input relocated — project restored.";
      this.#relocating = false;
    }

    // Inputs: set the referenced paths; the worker's ImportMetadata /
    // LensOverrideApplied events (emitted during the restore) fill in the
    // probed metadata and the lens tags.
    importStore.inputs.left.path = data.left.path;
    importStore.inputs.right.path = data.right.path;
    // A restored calibration means the result is valid again.
    importStore.resultInvalidated = false;
    if (data.has_calibration) {
      importStore.profilePath = data.calibration_path ?? data.path;
    }

    // Pose: seed the pose store from the typed payload (the worker sets its own
    // target; it emits no Pose line on a restore).
    pose.pose = {
      yaw: data.pose.yaw,
      pitch: data.pose.pitch,
      fov: data.pose.fov_degrees,
      fovMax: pose.fovMax,
    };
    pose.enabled = true;

    // Export settings: the webview owns these, so apply the restored values and
    // re-resolve the worker-side output path preview.
    exportStore.settings = data.export;
    exportStore.preset = data.export.preset;
    void exportStore.previewPath();
  }

  /**
   * Save the current state as a `.reco` project (PROJ-01).
   *
   * Opens the native save dialog, then posts `save_project` with the webview's
   * current export settings (the worker assembles the manifest). No media is
   * copied.
   */
  async save(): Promise<void> {
    try {
      const selected = await save({
        defaultPath: this.#deriveFilename(),
        filters: [{ name: "Reco project", extensions: PROJECT_EXTENSIONS }],
      });
      if (typeof selected !== "string") return;
      this.status = "saving";
      this.error = null;
      await invoke("save_project", {
        path: selected,
        settings: exportStore.settings,
      });
    } catch (e) {
      this.status = "error";
      this.error = formatWorkerError(e);
    }
  }

  /**
   * Open a `.reco` project (PROJ-01).
   *
   * Opens the native open dialog, then posts `open_project`. The worker restores
   * everything and emits `project_opened`, or emits `project_missing_inputs`
   * (which surfaces the relocate dialog) — a missing input never fails the open.
   */
  async open(): Promise<void> {
    try {
      const selected = await open({
        multiple: false,
        directory: false,
        filters: [{ name: "Reco project", extensions: PROJECT_EXTENSIONS }],
      });
      if (typeof selected !== "string") return;
      this.status = "opening";
      this.error = null;
      this.missing = [];
      // A plain open is silent; clear any pending relocate confirmation.
      this.#relocating = false;
      this.notice = null;
      await invoke("open_project", { path: selected });
    } catch (e) {
      this.status = "error";
      this.error = formatWorkerError(e);
    }
  }

  /**
   * Relocate one missing input to `path` and re-run the restore (PROJ-01).
   *
   * The worker replaces the role's referenced path and re-probes; a complete set
   * restores (`project_opened`), a still-missing set re-emits
   * `project_missing_inputs`.
   */
  async relocate(role: InputRole, path: string): Promise<void> {
    try {
      this.status = "opening";
      this.error = null;
      // Mark this restore as a relocate so its completion announces the
      // contract confirmation.
      this.#relocating = true;
      await invoke("relocate_project_input", { role, path });
    } catch (e) {
      this.status = "error";
      this.error = formatWorkerError(e);
    }
  }

  /**
   * Open a file picker for one missing input and relocate it (PROJ-01).
   *
   * The dialog runs in the webview; only the chosen path crosses IPC.
   */
  async chooseRelocation(role: InputRole): Promise<void> {
    try {
      const selected = await open({
        multiple: false,
        directory: false,
        filters: [{ name: "Video", extensions: VIDEO_EXTENSIONS }],
      });
      if (typeof selected !== "string") return;
      await this.relocate(role, selected);
    } catch (e) {
      this.status = "error";
      this.error = formatWorkerError(e);
    }
  }

  /** Close the relocate dialog without relocating (the app is unchanged). */
  dismissMissing(): void {
    this.missing = [];
    this.status = "idle";
    this.#relocating = false;
  }

  /** Dismiss the transient relocate-success confirmation. */
  dismissNotice(): void {
    this.notice = null;
  }

  /** Derive `<left-stem>_<right-stem>.reco` from the input names (PROJ-01). */
  #deriveFilename(): string {
    const stem = (role: InputRole): string => {
      const p = importStore.inputs[role].path;
      if (p === null) return "clip";
      const base = p.split(/[\\/]/).pop() ?? p;
      return base.replace(/\.[^.]+$/, "") || "clip";
    };
    return `${stem("left")}_${stem("right")}.reco`;
  }

  /** Whether a relocate flow is currently offered. */
  get hasMissing(): boolean {
    return this.missing.length > 0;
  }
}

/** The shared project store instance. */
export const projectStore = new ProjectStore();
