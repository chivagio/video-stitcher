/**
 * Import rune store (IMPT-01 / IMPT-02).
 *
 * Wraps the typed WorkerCommand/WorkerEvent protocol for clip import. Both
 * selection paths — the native dialog and an HTML5 drop — converge on the same
 * validated `set_input` command (D3-02), so validation and metadata probing are
 * identical. The worker is authoritative: this store mirrors the typed
 * `import_metadata` event and never regex-parses log strings (E5 / FRICTION A3).
 *
 * Pure UI (D-06): this store only sends worker commands over Tauri IPC and
 * renders worker events. It never touches engine types or window lifecycle.
 */

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import type { InputMetadata, InputRole, WorkerEventTyped } from "./types";
import { WORKER_EVENT_TYPED } from "./types";

/** Per-slot state machine (UI-SPEC Import Screen Contract). */
export type InputStatus = "empty" | "loading" | "error" | "ready";

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
}

/** Video extensions offered in the native open dialog. */
const VIDEO_EXTENSIONS = ["mp4", "mov", "mkv", "avi", "webm", "m4v"];

function emptySlot(role: InputRole): InputSlot {
  return { role, path: null, status: "empty", error: null, metadata: null };
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

  /** The path of a loaded profile, or null (IMPT-05). */
  profilePath = $state<string | null>(null);

  /** Whether the current result no longer matches the inputs (D3-08). */
  resultInvalidated = $state(false);

  /**
   * Whether a valid result exists (fresh calibration or loaded profile) — the
   * Preview step's enablement. A fresh run's result is wired in plan 03-06;
   * a loaded profile populates it here.
   */
  get hasResult(): boolean {
    return this.profilePath !== null && !this.resultInvalidated;
  }

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

  /** Stop listening. */
  destroy(): void {
    if (this.#unlisten !== null) {
      this.#unlisten();
      this.#unlisten = null;
    }
  }

  /** Apply a typed worker event to the slot state. */
  #onEvent(event: WorkerEventTyped): void {
    if (event.kind === "import_metadata") {
      const { role, metadata } = event.data;
      const slot = this.inputs[role];
      slot.metadata = metadata;
      slot.status = "ready";
      slot.error = null;
    }
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
      slot.error = String(e);
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
    try {
      await invoke("set_input", { role, path });
    } catch (e) {
      slot.status = "error";
      slot.error = String(e);
    }
  }
}

/** The shared import store instance. */
export const importStore = new ImportStore();
