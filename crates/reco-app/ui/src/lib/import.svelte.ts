/**
 * Import rune store (IMPT-01..IMPT-06).
 *
 * Wraps the typed WorkerCommand/WorkerEvent protocol for clip import, lens
 * profiles, compatibility warnings, and profile load/save. Both selection
 * paths — the native dialog and an HTML5 drop — converge on the same validated
 * `set_input` command (D3-02), so validation and metadata probing are identical.
 * The worker is authoritative: this store mirrors typed events and never
 * regex-parses log strings (E5 / FRICTION A3).
 *
 * Pure UI (D-06): this store only sends worker commands over Tauri IPC and
 * renders worker events. It never touches engine types or window lifecycle.
 */

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open, save } from "@tauri-apps/plugin-dialog";
import type {
  CompatibilityIssue,
  InputMetadata,
  InputRole,
  LensCandidate,
  WorkerEventTyped,
} from "./types";
import { WORKER_EVENT_TYPED } from "./types";

/** Per-slot state machine (UI-SPEC Import Screen Contract). */
export type InputStatus = "empty" | "loading" | "error" | "ready";

/** The lens row's provenance tag (UI-SPEC Lens Profile Contract). */
export type LensTag = "detected" | "estimated" | "overridden" | null;

/** The candidate-search state for one slot's lens dropdown. */
export type CandidateStatus = "idle" | "loading" | "ready" | "error";

/** The profile load/save state machine (UI-SPEC Profile Persistence Contract). */
export type ProfileStatus = "idle" | "loading" | "saving" | "error";

/** One camera slot's state. */
export interface InputSlot {
  role: InputRole;
  /** The operator-chosen local path, or null when empty. */
  path: string | null;
  status: InputStatus;
  /** The typed error text when `status === "error"`. */
  error: string | null;
  /** The probed metadata once the worker reports it. */
  metadata: InputMetadata | null;
  /** The resolved/overridden lens profile and its tag. */
  lens: { value: LensCandidate | null; tag: LensTag };
  /** The resolution-aware candidates for this slot's dropdown. */
  candidates: LensCandidate[];
  candidatesStatus: CandidateStatus;
  candidatesError: string | null;
}

/** Video extensions offered in the native open dialog. */
const VIDEO_EXTENSIONS = ["mp4", "mov", "mkv", "avi", "webm", "m4v"];

/** Profile extension for the load/save dialogs. */
const PROFILE_EXTENSIONS = ["json"];

function emptySlot(role: InputRole): InputSlot {
  return {
    role,
    path: null,
    status: "empty",
    error: null,
    metadata: null,
    lens: { value: null, tag: null },
    candidates: [],
    candidatesStatus: "idle",
    candidatesError: null,
  };
}

/**
 * Render a typed worker error (or a rejected command) as display text.
 *
 * Worker errors cross IPC as the externally-tagged `WorkerError` enum, e.g.
 * `{ Engine: "…" }` or `{ InvalidInput: { field, reason } }`. This unwraps the
 * typed inner text so the UI renders the engine/worker message, never a bare
 * code or `[object Object]` (CONVENTIONS.md:113).
 */
export function formatWorkerError(error: unknown): string {
  if (error == null) return "unknown error";
  if (typeof error === "string") return error;
  if (typeof error === "object") {
    const obj = error as Record<string, unknown>;
    const keys = Object.keys(obj);
    if (keys.length === 1) {
      const payload = obj[keys[0]];
      if (typeof payload === "string") return payload;
      if (payload && typeof payload === "object") {
        const p = payload as Record<string, unknown>;
        if (typeof p.field === "string" && typeof p.reason === "string") {
          return `invalid ${p.field}: ${p.reason}`;
        }
        return JSON.stringify(payload);
      }
    }
    if (typeof obj.message === "string") return obj.message;
    return JSON.stringify(error);
  }
  return String(error);
}

/**
 * The import rune store (class with `$state` fields, shared across components).
 */
class ImportStore {
  /** The two camera inputs, keyed by role. */
  inputs = $state<Record<InputRole, InputSlot>>({
    left: emptySlot("left"),
    right: emptySlot("right"),
  });

  /** The advisory compatibility findings; empty means every check passed. */
  compatibility = $state<CompatibilityIssue[]>([]);

  /** Whether the check evaluation itself failed (degraded, never blocking). */
  checksUnavailable = $state(false);
  checksError = $state<string | null>(null);

  /** Whether the current result no longer matches the inputs (D3-08). */
  resultInvalidated = $state(false);

  /** The path of a loaded profile, or null (IMPT-05). */
  profilePath = $state<string | null>(null);

  /** The load/save state machine. */
  profileStatus = $state<ProfileStatus>("idle");
  profileError = $state<string | null>(null);
  /** Which profile operation last ran (drives the typed error copy). */
  profileOp = $state<"load" | "save" | null>(null);
  lastSavedPath = $state<string | null>(null);

  /** Unlisten function for the typed worker-event listener. */
  #unlisten: (() => void) | null = null;

  /** Roles whose `set_input` command has not yet produced a terminal event. */
  #importQueue: InputRole[] = [];
  /** The role whose lens-candidate request is in flight. */
  #pendingLens: InputRole | null = null;

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
      case "import_metadata": {
        const { role, metadata } = event.data;
        const slot = this.inputs[role];
        slot.metadata = metadata;
        slot.status = "ready";
        slot.error = null;
        this.#removeFromQueue(role);
        break;
      }
      case "compatibility": {
        this.compatibility = event.data.issues;
        this.checksUnavailable = false;
        this.checksError = null;
        break;
      }
      case "lens_candidates": {
        const { role, candidates } = event.data;
        const slot = this.inputs[role];
        slot.candidates = candidates;
        slot.candidatesStatus = "ready";
        slot.candidatesError = null;
        if (this.#pendingLens === role) this.#pendingLens = null;
        break;
      }
      case "lens_override_applied": {
        const { role, candidate } = event.data;
        this.inputs[role].lens = {
          value: candidate,
          tag: candidate === null ? null : "overridden",
        };
        break;
      }
      case "profile_loaded": {
        this.profilePath = event.data.path;
        this.profileStatus = "idle";
        this.profileError = null;
        this.resultInvalidated = false;
        break;
      }
      case "profile_saved": {
        this.lastSavedPath = event.data.path;
        this.profileStatus = "idle";
        this.profileError = null;
        break;
      }
      case "result_invalidated": {
        this.resultInvalidated = true;
        break;
      }
      case "failed": {
        this.#handleFailure(event.data);
        break;
      }
      default:
        // Other typed events (calibration) are consumed by the calibration
        // store (plan 03-06); the import store ignores them.
        break;
    }
  }

  /**
   * Route a typed failure to the operation that is actually in flight.
   *
   * The worker emits a generic `Failed(WorkerError)` with no role, so the store
   * maps it to the earliest unresolved import command, then a pending lens
   * request, then an in-flight profile operation, and finally treats an
   * unattributed failure with both clips ready as a checks-unavailable notice
   * (degraded, never blocking).
   */
  #handleFailure(error: unknown): void {
    const message = formatWorkerError(error);
    const importRole = this.#importQueue.shift();
    if (importRole !== undefined) {
      const slot = this.inputs[importRole];
      slot.status = "error";
      slot.error = message;
      return;
    }
    if (this.#pendingLens !== null) {
      const slot = this.inputs[this.#pendingLens];
      slot.candidatesStatus = "error";
      slot.candidatesError = message;
      this.#pendingLens = null;
      return;
    }
    if (this.profileStatus === "loading" || this.profileStatus === "saving") {
      this.profileStatus = "error";
      this.profileError = message;
      return;
    }
    if (this.bothReady) {
      this.checksUnavailable = true;
      this.checksError = message;
      return;
    }
  }

  #removeFromQueue(role: InputRole): void {
    const idx = this.#importQueue.indexOf(role);
    if (idx >= 0) this.#importQueue.splice(idx, 1);
  }

  /**
   * Open the native file dialog for `role`, then post the chosen path.
   *
   * The dialog plugin runs in the webview; only the resulting path string
   * crosses IPC (D3-02).
   */
  async chooseFile(role: InputRole): Promise<void> {
    try {
      const selected = await open({
        multiple: false,
        directory: false,
        filters: [{ name: "Video", extensions: VIDEO_EXTENSIONS }],
      });
      // `null` means the operator cancelled the dialog — a no-op.
      if (typeof selected !== "string") return;
      await this.setPathFromDrop(role, selected);
    } catch (e) {
      const slot = this.inputs[role];
      slot.status = "error";
      slot.error = formatWorkerError(e);
    }
  }

  /**
   * Post an operator-chosen path for `role` (the shared drop/dialog path).
   *
   * Marks the slot loading and invokes `set_input`; the worker emits the typed
   * `import_metadata` event that transitions it to ready, or a `failed` event
   * surfaces as an error.
   */
  async setPathFromDrop(role: InputRole, path: string): Promise<void> {
    const slot = this.inputs[role];
    slot.path = path;
    slot.status = "loading";
    slot.error = null;
    slot.metadata = null;
    slot.candidates = [];
    slot.candidatesStatus = "idle";
    this.#importQueue.push(role);
    try {
      await invoke("set_input", { role, path });
    } catch (e) {
      this.#removeFromQueue(role);
      slot.status = "error";
      slot.error = formatWorkerError(e);
    }
  }

  /**
   * Handle a drop of one or more files (D3-03).
   *
   * Dropping two files at once fills Camera A then Camera B; a single file
   * fills the slot it was dropped on.
   */
  async dropFiles(role: InputRole, paths: string[]): Promise<void> {
    if (paths.length >= 2) {
      await this.setPathFromDrop("left", paths[0]);
      await this.setPathFromDrop("right", paths[1]);
    } else if (paths.length === 1) {
      await this.setPathFromDrop(role, paths[0]);
    }
  }

  /** Clear one camera slot back to empty. */
  async clearInput(role: InputRole): Promise<void> {
    try {
      await invoke("clear_input", { role });
    } catch {
      // The worker will emit a corrective compatibility/invalidation event.
    }
    this.inputs[role] = emptySlot(role);
  }

  /** Request the resolution-aware lens candidates for one slot. */
  async requestLensCandidates(role: InputRole): Promise<void> {
    const slot = this.inputs[role];
    slot.candidatesStatus = "loading";
    slot.candidatesError = null;
    this.#pendingLens = role;
    try {
      await invoke("lens_candidates", { role });
    } catch (e) {
      if (this.#pendingLens === role) this.#pendingLens = null;
      slot.candidatesStatus = "error";
      slot.candidatesError = formatWorkerError(e);
    }
  }

  /** Apply a lens-profile override to one slot. */
  async setLensOverride(role: InputRole, candidate: LensCandidate): Promise<void> {
    try {
      await invoke("set_lens_override", { role, candidate });
    } catch (e) {
      const slot = this.inputs[role];
      slot.candidatesStatus = "error";
      slot.candidatesError = formatWorkerError(e);
    }
  }

  /** Clear a lens override, returning the slot to auto-detect. */
  async clearLensOverride(role: InputRole): Promise<void> {
    try {
      await invoke("clear_lens_override", { role });
    } catch {
      // The worker emits LensOverrideApplied(None) on success.
    }
    this.inputs[role].lens = { value: null, tag: null };
  }

  /** Load a calibration profile from a `.json` file (IMPT-05). */
  async loadProfile(): Promise<void> {
    try {
      const selected = await open({
        multiple: false,
        directory: false,
        filters: [{ name: "Calibration profile", extensions: PROFILE_EXTENSIONS }],
      });
      if (typeof selected !== "string") return;
      this.profileStatus = "loading";
      this.profileOp = "load";
      this.profileError = null;
      await invoke("load_profile", { path: selected });
    } catch (e) {
      this.profileStatus = "error";
      this.profileError = formatWorkerError(e);
    }
  }

  /** Save the current calibration profile (IMPT-06). */
  async saveProfile(): Promise<void> {
    try {
      const selected = await save({
        defaultPath: this.#deriveFilename(),
        filters: [{ name: "Calibration profile", extensions: PROFILE_EXTENSIONS }],
      });
      if (typeof selected !== "string") return;
      this.profileStatus = "saving";
      this.profileOp = "save";
      this.profileError = null;
      await invoke("save_profile", { path: selected });
    } catch (e) {
      this.profileStatus = "error";
      this.profileError = formatWorkerError(e);
    }
  }

  /** Derive `<cameraA-stem>_<cameraB-stem>.json` from the input names (D3-16). */
  #deriveFilename(): string {
    const stem = (role: InputRole): string => {
      const p = this.inputs[role].path;
      if (p === null) return "clip";
      const base = p.split(/[\\/]/).pop() ?? p;
      return base.replace(/\.[^.]+$/, "") || "clip";
    };
    return `${stem("left")}_${stem("right")}.json`;
  }

  /** Whether both camera slots hold valid, probed clips. */
  get bothReady(): boolean {
    return (
      this.inputs.left.status === "ready" && this.inputs.right.status === "ready"
    );
  }

  /**
   * Whether a valid result exists (fresh calibration or loaded profile) — the
   * Preview step's enablement. A fresh run's result is wired in plan 03-06;
   * a loaded profile populates it here.
   */
  get hasResult(): boolean {
    return this.profilePath !== null && !this.resultInvalidated;
  }
}

/** The shared import store instance. */
export const importStore = new ImportStore();
